//! First-party content search (`search_files`). Before this, the tool surface told the model to
//! "use shell_run + ripgrep" for content search — a silent footgun: on a machine without `rg` on
//! PATH it just fails. This drives ripgrep's OWN walker (`ignore`, which respects `.gitignore` and
//! skips hidden/binary) + the in-tree `regex` crate for line matching. Pure-Rust, static-safe,
//! read-only — and it's the canonical content-search tool (`file_glob` matches NAMES, this matches
//! CONTENT).

use crate::agent::builtin::confine;
use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use memchr::memchr;
use regex::RegexBuilder;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cap on emitted matches (the loop also bounds history growth at the agent layer).
const DEFAULT_MAX_RESULTS: usize = 200;
/// Skip files larger than this (generated bundles / data dumps aren't useful search targets).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes sniffed for a NUL to classify a file as binary (then skipped).
const BINARY_SNIFF: usize = 8192;
/// Clip a very long matched line so one minified line can't blow the result.
const MAX_LINE_CHARS: usize = 240;
/// Hard wall-clock cap so a search over a giant tree can't freeze the agent loop (the parallel walk
/// checks this per file and quits every thread once it trips).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on `context` — ±10 lines already shows a whole function around most matches; more is a
/// file read wearing a search costume.
const MAX_CONTEXT: usize = 10;

/// Decode a file's bytes to searchable text, detecting UTF-16 (P2.2). A UTF-16-encoded source file
/// (common on Windows — PowerShell `>` redirection, some editors, .NET logs) is `byte,0,byte,0…`,
/// so the plain NUL sniff would wrongly write it off as binary and the search would silently miss
/// it. We detect UTF-16 by BOM first (unambiguous), then by a strong alternating-NUL heuristic on
/// the head. Returns `None` for genuine binary (so the caller skips it).
pub(crate) fn decode_text(bytes: &[u8]) -> Option<String> {
    // BOM-based detection (unambiguous).
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let u16s: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    let head = &bytes[..bytes.len().min(BINARY_SNIFF)];
    if !head.contains(&0) {
        return Some(String::from_utf8_lossy(bytes).into_owned()); // ordinary UTF-8 / ASCII
    }
    // NULs present without a BOM: could be BOM-less UTF-16, or genuine binary. UTF-16LE ASCII text
    // has NULs on the ODD byte offsets (high byte of each code unit); UTF-16BE on the EVEN offsets.
    // If ~80% of one parity is NUL, treat it as UTF-16 text of that endianness.
    let half = head.len() / 2;
    if half == 0 {
        return None;
    }
    let odd_nul = head.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    let even_nul = head.iter().step_by(2).filter(|&&b| b == 0).count();
    if odd_nul * 10 >= half * 8 {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    if even_nul * 10 >= half * 8 {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    None // genuine binary
}

// ---------------------------------------------------------------------------
// Literal prefilter (ripgrep's matcher-side speed secret). A regex like `fn\s+foo.*Bar` can only
// match lines that contain BOTH `foo` and `Bar`, so before the regex engine runs on a file we do
// one SIMD pass (`memchr`) per required literal over the decoded text — a file missing any of them
// is skipped without a single regex call. The gate is CONSERVATIVE: it may only skip files the
// regex itself would produce no matches in, never drop a real match (pinned down by the
// `prefilter_never_changes_results` property test below).
// ---------------------------------------------------------------------------

/// A literal every match of a pattern must contain, plus the pattern offset where the proof was
/// found (used only to order same-length candidates deterministically).
#[derive(Debug)]
struct Requirement {
    bytes: Vec<u8>,
    pos: usize,
}

/// A conservative, hand-rolled required-literal extractor + SIMD gate. It ONLY trusts literals
/// from non-alternating concat paths (`foo.*bar` requires both `foo` and `bar`), and bails to
/// "no prefilter" the moment it meets anything it cannot prove:
///
///   * ANY top-level `|` poisons the whole pattern — `foo$|bar$` matches "foo" OR "bar", so
///     neither literal is required (alternation inside a `(…)` group is safe: the group is
///     skipped wholesale and only literals OUTSIDE it are trusted).
///   * Nullable constructs (`*` `?` `{0,…}`) un-require the single preceding atom.
///   * `.`, `^`, `$`, `[…]`, `(…)`, class escapes (`\d` `\w` `\b` …) and backreferences (`\1`
///     `\k<..>`) are not literal bytes — they close the current run.
///   * Inline flag groups (`(?i)` `(?-u:…)` …) anywhere in the pattern → no prefilter at all
///     (they can change case/unicode semantics mid-pattern, invalidating the extraction).
///
/// Under `ignore_case` the regex crate applies Unicode-aware folding, so only PURE-ASCII literals
/// without k/K/s/S are kept: every other ASCII letter folds exactly to its ASCII counterpart
/// (byte-fold is equivalent), while k folds to KELVIN SIGN (U+212A) and s to LATIN SMALL LONG S
/// (U+017F) — different BYTES — and a non-ASCII literal like `café` can fold to `CAFÉ`. Keeping
/// those would risk false negatives, i.e. dropped real matches.
struct LiteralPrefilter {
    /// Fold both needle and haystack to ASCII-lowercase before scanning (safe per the rules above).
    fold: bool,
    needles: Vec<Vec<u8>>,
}

impl LiteralPrefilter {
    fn build(pattern: &str, ignore_case: bool) -> Option<Self> {
        if has_case_or_unicode_flags(pattern) {
            return None;
        }
        let reqs = pick_literals(extract_requirements(pattern));
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for r in reqs {
            if r.bytes.is_empty() {
                continue;
            }
            if ignore_case {
                if !r.bytes.is_ascii()
                    || r.bytes
                        .iter()
                        .any(|b| matches!(b, b'k' | b'K' | b's' | b'S'))
                {
                    // Unsafe under Unicode folding — drop this literal. Proven-safe siblings from
                    // the same pattern still gate on their own.
                    continue;
                }
                needles.push(r.bytes.to_ascii_lowercase());
            } else {
                needles.push(r.bytes);
            }
        }
        if needles.is_empty() {
            return None;
        }
        Some(Self {
            fold: ignore_case,
            needles,
        })
    }

    /// True iff every required literal occurs in `text` (folding per `fold`). False means the
    /// regex CANNOT match any line of this text and the file may be skipped outright.
    fn matches_text(&self, text: &str) -> bool {
        let folded;
        let hay: &[u8] = if self.fold {
            folded = text.as_bytes().to_ascii_lowercase();
            &folded
        } else {
            text.as_bytes()
        };
        self.needles.iter().all(|n| contains_bytes(hay, n))
    }

    #[cfg(test)]
    fn needles(&self) -> &[Vec<u8>] {
        &self.needles
    }
}

/// `(?X` where X is a flag letter (or `-`, introducing a negation) anywhere in the pattern. `(?:`
/// and `(?P<…>` are NOT flags and don't affect semantics. Over-approximates on escaped/inside-class
/// occurrences — that only costs the prefilter, never correctness.
fn has_case_or_unicode_flags(pattern: &str) -> bool {
    let b = pattern.as_bytes();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'(' && b[i + 1] == b'?' {
            let n = b[i + 2];
            if matches!(
                n,
                b'i' | b'I' | b'm' | b'M' | b's' | b'S' | b'x' | b'X' | b'u' | b'U' | b'-'
            ) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `hay` contains `needle` as a contiguous substring (SIMD `memchr` on the first byte, then
/// verify — the fast path ripgrep itself uses before promoting to a full inner-literal matcher).
fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    let first = needle[0];
    let mut from = 0usize;
    while let Some(i) = memchr(first, &hay[from..]) {
        let start = from + i;
        if hay.len() - start < needle.len() {
            return false;
        }
        if &hay[start..start + needle.len()] == needle {
            return true;
        }
        from = start + 1;
        if from >= hay.len() {
            return false;
        }
    }
    false
}

/// Walk a pattern char by char, accumulating REQUIRED literals — byte runs sitting on a
/// non-alternating concat path that every match must traverse. Everything not provably required
/// closes the current run; a top-level `|` poisons the result entirely.
fn extract_requirements(pattern: &str) -> Vec<Requirement> {
    let chars: Vec<(usize, char)> = pattern.char_indices().collect();
    let mut out: Vec<Requirement> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_pos = 0usize;
    let mut poisoned = false;
    let mut i = 0usize;

    fn flush(out: &mut Vec<Requirement>, cur: &mut Vec<u8>, pos: usize) {
        if !cur.is_empty() {
            out.push(Requirement {
                bytes: std::mem::take(cur),
                pos,
            });
        }
    }

    while i < chars.len() {
        let (byte_pos, c) = chars[i];
        match c {
            // Alternation at THIS level: no literal anywhere is required (a match may come from a
            // branch that doesn't contain it). Keep parsing (to stay structurally aligned) but
            // discard everything at the end.
            '|' => {
                flush(&mut out, &mut cur, cur_pos);
                poisoned = true;
                i += 1;
            }
            // Escape: a literal escaped char extends the run ONLY if it quotes a non-alphanumeric
            // char (regex `\X` == literal `X`). Class escapes (`\d` `\w` `\b` …), anchors (`\A`
            // `\z`), backreferences (`\1`–`\9`, `\k`, `\g`) and hex/unicode forms (`\x` `\u`)
            // match UNKNOWN bytes — they close the run.
            '\\' => {
                let Some(&(_, e)) = chars.get(i + 1) else {
                    break; // trailing backslash — invalid regex; stop, keep what we have
                };
                match e {
                    'd'
                    | 'D'
                    | 's'
                    | 'S'
                    | 'w'
                    | 'W'
                    | 'b'
                    | 'B'
                    | 'A'
                    | 'z'
                    | 'k'
                    | 'g'
                    | 'p'
                    | 'P'
                    | 'x'
                    | 'u'
                    | 'U'
                    | 'n'
                    | 'r'
                    | 't'
                    | 'f'
                    | 'v'
                    | 'a'
                    | 'e'
                    | '0'..='9' => {
                        flush(&mut out, &mut cur, cur_pos);
                        i += 2;
                    }
                    _ => {
                        if cur.is_empty() {
                            cur_pos = byte_pos;
                        }
                        let mut buf = [0u8; 4];
                        cur.extend_from_slice(e.encode_utf8(&mut buf).as_bytes());
                        i += 2;
                    }
                }
            }
            // Structure that closes the current run: `.` (any char), `^`/`$` (zero-width), `[…]`
            // (one char of several), `(…)` (a group — may hide alternation, backrefs, flags, so
            // its content is untrusted; skipped whole and parsing resumes after it).
            '.' | '^' | '$' | '[' | '(' => {
                flush(&mut out, &mut cur, cur_pos);
                match c {
                    '[' => {
                        i += 1;
                        if i < chars.len() && chars[i].1 == '^' {
                            i += 1;
                        }
                        if i < chars.len() && chars[i].1 == ']' {
                            i += 1;
                        }
                        while i < chars.len() {
                            if chars[i].1 == '\\' {
                                i += 2;
                                continue;
                            }
                            if chars[i].1 == ']' {
                                i += 1;
                                break;
                            }
                            i += 1;
                        }
                    }
                    '(' => {
                        let mut depth = 1usize;
                        i += 1;
                        while i < chars.len() && depth > 0 {
                            match chars[i].1 {
                                '\\' => i += 2,
                                '(' => {
                                    depth += 1;
                                    i += 1;
                                }
                                ')' => {
                                    depth -= 1;
                                    i += 1;
                                }
                                _ => i += 1,
                            }
                        }
                    }
                    _ => i += 1,
                }
            }
            // Quantifier: applies to the PREVIOUS atom. `*` `?` `{0,…}` make that atom NULLABLE —
            // if it was the last literal char, that char is no longer required. `+`/`{n,}` (n≥1)
            // keep it required but close the run. A quantifier after a group/class/empty run is a
            // no-op for the accumulator.
            '*' | '?' => {
                drop_last_required_char(&mut cur);
                flush(&mut out, &mut cur, cur_pos);
                i += 1;
            }
            '+' => {
                flush(&mut out, &mut cur, cur_pos);
                i += 1;
            }
            '{' => {
                let mut j = i + 1;
                let mut min_digits = String::new();
                while j < chars.len() && chars[j].1.is_ascii_digit() {
                    min_digits.push(chars[j].1);
                    j += 1;
                }
                let mut k = j;
                if k < chars.len() && chars[k].1 == ',' {
                    k += 1;
                    while k < chars.len() && chars[k].1.is_ascii_digit() {
                        k += 1;
                    }
                }
                if !min_digits.is_empty() && k < chars.len() && chars[k].1 == '}' {
                    let min: u32 = min_digits.parse().unwrap_or(0);
                    if min == 0 {
                        drop_last_required_char(&mut cur);
                    }
                    flush(&mut out, &mut cur, cur_pos);
                    i = k + 1;
                } else {
                    // Not a quantifier — regex treats a bare `{` as a literal.
                    if cur.is_empty() {
                        cur_pos = byte_pos;
                    }
                    cur.push(b'{');
                    i += 1;
                }
            }
            // Ordinary literal char: extends the current required run.
            _ => {
                if cur.is_empty() {
                    cur_pos = byte_pos;
                }
                let mut buf = [0u8; 4];
                cur.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                i += 1;
            }
        }
    }
    flush(&mut out, &mut cur, cur_pos);
    if poisoned {
        Vec::new()
    } else {
        out
    }
}

/// The previous atom became nullable (`*`/`?`/`{0,…}`): remove ONE trailing char from the current
/// run, UTF-8 aware (the run minus its last char is still required — `foos?` requires `foo`).
fn drop_last_required_char(cur: &mut Vec<u8>) {
    while let Some(&b) = cur.last() {
        cur.pop();
        if b & 0b1100_0000 != 0b1000_0000 {
            break; // popped a lead/ASCII byte — exactly one full char removed
        }
    }
}

/// Best literals first: longest (most selective — a rare first byte lets `memchr` reject a file in
/// one pass), ties by pattern position. Capped: each needle costs a SIMD pass over the file and
/// past a couple the marginal selectivity is noise.
fn pick_literals(mut reqs: Vec<Requirement>) -> Vec<Requirement> {
    reqs.sort_by(|a, b| b.bytes.len().cmp(&a.bytes.len()).then(a.pos.cmp(&b.pos)));
    reqs.truncate(4);
    reqs
}

pub struct SearchFiles {
    root: PathBuf,
}
impl SearchFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for SearchFiles {
    fn name(&self) -> &str {
        "search_files"
    }
    fn description(&self) -> &str {
        concat!(
            "Search file CONTENT by regular expression. Skips hidden + binary + .gitignored unless \
         hidden:true. Rows are `path:line: text`; context:N adds ±N lines. Do NOT shell out \
         to grep/ripgrep. Read-only.",
            crate::search_routing!()
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "regular expression matched per line"},
                "path": {"type": "string", "description": "directory (subdir, ../ or absolute)"},
                "glob": {"type": "string", "description": "file glob, e.g. *.rs or src/**/*.ts"},
                "ignore_case": {"type": "boolean", "description": "case-insensitive match (default false)"},
                "hidden": {"type": "boolean", "description": "also hidden and .gitignored paths (default false)"},
                "max_results": {"type": "integer", "description": "cap (default 200)"},
                "context": {"type": "integer", "description": "±N lines of context (default 0, max 10)"},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches"], "description": "files_with_matches lists matching paths only"},
                "format": crate::agent::result_format::schema_property()
            },
            "required": ["pattern"]
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("missing `pattern`")?;
        let ignore_case = args
            .get("ignore_case")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let files_only = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .map(|m| m == "files_with_matches")
            .unwrap_or(false);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_RESULTS as u64) as usize;
        let ctx = args
            .get("context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_CONTEXT as u64) as usize;
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .with_context(|| format!("invalid regex `{pattern}`"))?;

        // Literal prefilter: built once per search from the (valid) pattern. Only literals the
        // regex provably cannot match without are kept — see `LiteralPrefilter` for the safety
        // argument. `None` = pattern yields no safe literal (alternation, all-class, inline flags,
        // …) and every file goes straight to the regex, exactly as before.
        let prefilter = LiteralPrefilter::build(pattern, ignore_case);
        let prefilter = prefilter.as_ref();

        let search_root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => confine(&self.root, p, true)?,
            None => self.root.clone(),
        };

        let show_hidden = args
            .get("hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut wb = WalkBuilder::new(&search_root);
        wb.git_global(false); // honor the repo's .gitignore, not the dev's global one (reproducible)
        if show_hidden {
            // Opt-in "see everything": stop honoring .gitignore / hidden-file rules so the walk
            // reaches dotfiles and ignored paths too (the user asked for nothing to be skipped).
            wb.hidden(false)
                .git_ignore(false)
                .git_exclude(false)
                .ignore(false)
                .parents(false);
        }
        if let Some(glob) = args.get("glob").and_then(|v| v.as_str()) {
            let mut ob = OverrideBuilder::new(&search_root);
            ob.add(glob)
                .with_context(|| format!("invalid glob `{glob}`"))?;
            wb.overrides(ob.build().context("building glob filter")?);
        }

        // A PARALLEL walk (ripgrep's own engine) with a SHARED wall-clock budget: every worker
        // checks the deadline before opening a file and returns `Quit` once it trips, so a search
        // over a huge tree can't freeze the agent loop. Results, the match counter, and the
        // skipped-large-file counter are shared behind a `Mutex`/atomics. Per-file work (read →
        // binary/UTF-16 classify → literal prefilter → line-scan) happens on the worker thread.
        // Per file: (path, rows), each row = (1-based line, clipped text, is_match). With
        // context=0 the rows are exactly the match lines (the classic flat shape); with context>0
        // they are the MERGED ±ctx windows around the matches, so an overlapping pair of matches
        // never prints a line twice.
        type FileRows = (PathBuf, Vec<(usize, String, bool)>);
        let results: Mutex<Vec<FileRows>> = Mutex::new(Vec::new());
        let files_hit = AtomicUsize::new(0);
        let match_count = AtomicUsize::new(0);
        let skipped_large = AtomicUsize::new(0);
        let truncated = AtomicBool::new(false);
        let deadline = Instant::now() + SEARCH_TIMEOUT;
        let budget_hit = AtomicBool::new(false);
        let re = &re;
        wb.build_parallel().run(|| {
            Box::new(|dent| {
                // Stop the whole walk once the match cap or the wall-clock budget is reached.
                if match_count.load(Ordering::Relaxed) >= max_results {
                    truncated.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                if Instant::now() >= deadline {
                    budget_hit.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                let dent = match dent {
                    Ok(d) => d,
                    Err(_) => return WalkState::Continue,
                };
                if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    return WalkState::Continue;
                }
                if dent.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                    skipped_large.fetch_add(1, Ordering::Relaxed); // P2.3: count, then report
                    return WalkState::Continue;
                }
                let bytes = match std::fs::read(dent.path()) {
                    Ok(b) => b,
                    Err(_) => return WalkState::Continue,
                };
                let text = match decode_text(&bytes) {
                    Some(t) => t,
                    None => return WalkState::Continue, // binary
                };
                // LITERAL PREFILTER GATE: one SIMD pass per required literal over the DECODED text.
                // A miss here means the regex cannot match any line of this file (the literal sits
                // on a non-alternating concat path of the pattern) — skip without a regex call.
                // Decoded text, not raw bytes: a UTF-16 file's bytes interleave NULs and would not
                // contain the literal as plain bytes, so it would be wrongly skipped.
                if let Some(pf) = prefilter {
                    if !pf.matches_text(&text) {
                        return WalkState::Continue;
                    }
                }
                let rel = dent
                    .path()
                    .strip_prefix(&self.root)
                    .unwrap_or(dent.path())
                    .to_path_buf();
                // files_with_matches: existence is the whole answer. Probed PER LINE, exactly like
                // content mode, so `^`/`$` anchors behave identically in both modes (`$` is
                // end-of-text without multiline — a whole-text probe would disagree with the
                // per-line content mode for anchored patterns). No rows are materialized.
                if files_only {
                    if text.lines().any(|line| re.is_match(line)) {
                        let n = match_count.fetch_add(1, Ordering::Relaxed);
                        if n >= max_results {
                            truncated.store(true, Ordering::Relaxed);
                            return WalkState::Quit;
                        }
                        files_hit.fetch_add(1, Ordering::Relaxed);
                        results.lock().unwrap().push((rel, Vec::new()));
                    }
                    return WalkState::Continue;
                }
                // Pass 1: collect this file's match line numbers (1-based) under the global cap.
                let mut hit_lines: Vec<usize> = Vec::new();
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let n = match_count.fetch_add(1, Ordering::Relaxed);
                        if n >= max_results {
                            truncated.store(true, Ordering::Relaxed);
                            break;
                        }
                        hit_lines.push(i + 1);
                    }
                }
                if hit_lines.is_empty() {
                    return WalkState::Continue;
                }
                files_hit.fetch_add(1, Ordering::Relaxed);
                // Pass 2: materialize rows — the match lines alone, or their merged ±ctx windows.
                let lines: Vec<&str> = text.lines().collect();
                let clip = |line: &str| -> String {
                    let trimmed = line.trim_end();
                    if trimmed.chars().count() > MAX_LINE_CHARS {
                        trimmed.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
                    } else {
                        trimmed.to_string()
                    }
                };
                let rows: Vec<(usize, String, bool)> = if ctx == 0 {
                    hit_lines
                        .iter()
                        .map(|&n| (n, clip(lines[n - 1]), true))
                        .collect()
                } else {
                    let mut windows: Vec<(usize, usize)> = Vec::new();
                    for &n in &hit_lines {
                        let s = n.saturating_sub(ctx).max(1);
                        let e = (n + ctx).min(lines.len());
                        match windows.last_mut() {
                            // Merge overlapping/adjacent windows so no line prints twice.
                            Some((_, prev_end)) if s <= *prev_end + 1 => {
                                *prev_end = (*prev_end).max(e)
                            }
                            _ => windows.push((s, e)),
                        }
                    }
                    let hits: std::collections::HashSet<usize> =
                        hit_lines.iter().copied().collect();
                    windows
                        .iter()
                        .flat_map(|&(s, e)| {
                            (s..=e).map(|n| (n, clip(lines[n - 1]), hits.contains(&n)))
                        })
                        .collect()
                };
                results.lock().unwrap().push((rel, rows));
                WalkState::Continue
            })
        });

        let mut results = results.into_inner().unwrap();
        // The parallel walk collects in nondeterministic order; sort so output is stable/readable.
        // (Rows within a file are already in line order — built by one worker in one pass.)
        results.sort_by(|a, b| a.0.cmp(&b.0));
        let files_hit = files_hit.load(Ordering::Relaxed);
        let skipped_large = skipped_large.load(Ordering::Relaxed);

        // files_with_matches: paths only, one per line (the grep -l shape), sorted. Same trailers
        // as content mode so caps / budget / skipped-large are never silent.
        if files_only {
            if results.is_empty() {
                let mut msg = format!("no matches for /{pattern}/");
                if skipped_large > 0 {
                    msg.push_str(&format!(
                        "\n(skipped {skipped_large} file(s) larger than {} MB — search a specific path if the match is in one)",
                        MAX_FILE_BYTES / (1024 * 1024)
                    ));
                }
                if budget_hit.load(Ordering::Relaxed) {
                    msg.push_str("\n(search budget reached before the whole tree was scanned — narrow with path/glob)");
                }
                return Ok(msg);
            }
            let mut out = format!("{files_hit} file(s) with matches:");
            for (rel, _) in &results {
                out.push_str(&format!("\n{}", rel.display()));
            }
            if truncated.load(Ordering::Relaxed) {
                out.push_str(&format!(
                    "\n…[capped at {max_results} files — narrow the pattern or set path/glob]"
                ));
            }
            if budget_hit.load(Ordering::Relaxed) {
                out.push_str("\n…[search budget reached before the whole tree was scanned — narrow with path/glob]");
            }
            if skipped_large > 0 {
                out.push_str(&format!(
                    "\n…[skipped {skipped_large} file(s) larger than {} MB]",
                    MAX_FILE_BYTES / (1024 * 1024)
                ));
            }
            return Ok(out);
        }

        if results.is_empty() {
            let mut msg = format!("no matches for /{pattern}/");
            if skipped_large > 0 {
                msg.push_str(&format!(
                    "\n(skipped {skipped_large} file(s) larger than {} MB — search a specific path if the match is in one)",
                    MAX_FILE_BYTES / (1024 * 1024)
                ));
            }
            if budget_hit.load(Ordering::Relaxed) {
                msg.push_str("\n(search budget reached before the whole tree was scanned — narrow with path/glob)");
            }
            return Ok(msg);
        }
        let shown_matches: usize = results
            .iter()
            .map(|(_, rows)| rows.iter().filter(|r| r.2).count())
            .sum();
        // grep's own convention: `path:line:` marks a MATCH row, `path-line-` a context row, `--`
        // separates non-contiguous groups — a shape every model already knows how to read.
        let mut lines: Vec<String> = Vec::new();
        // Which file each row belongs to and whether it is a match row (not context, not `--`), so
        // the concise view can count what it hides per file.
        let mut owner: Vec<usize> = Vec::new();
        let mut is_match_row: Vec<bool> = Vec::new();
        for (f, (rel, rows)) in results.iter().enumerate() {
            let mut prev: Option<usize> = None;
            for (n, shown, is_match) in rows {
                if !lines.is_empty() && ctx > 0 && prev.is_none_or(|p| *n != p + 1) {
                    lines.push("--".to_string());
                    owner.push(f);
                    is_match_row.push(false);
                }
                if *is_match {
                    lines.push(format!("{}:{}: {}", rel.display(), n, shown));
                } else {
                    lines.push(format!("{}-{}- {}", rel.display(), n, shown));
                }
                owner.push(f);
                is_match_row.push(*is_match);
                prev = Some(*n);
            }
        }
        let header = format!("{shown_matches} match(es) in {files_hit} file(s):");
        let mut trailer = String::new();
        if truncated.load(Ordering::Relaxed) {
            trailer.push_str(&format!(
                "\n…[capped at {max_results} matches — narrow the pattern or set path/glob]"
            ));
        }
        if budget_hit.load(Ordering::Relaxed) {
            trailer.push_str("\n…[search budget reached before the whole tree was scanned — narrow with path/glob]");
        }
        if skipped_large > 0 {
            trailer.push_str(&format!(
                "\n…[skipped {skipped_large} file(s) larger than {} MB]",
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        let full = format!("{header}\n{}{trailer}", lines.join("\n"));
        let keep = crate::agent::result_format::CONCISE_SEARCH_ROWS;
        if crate::agent::result_format::from_args(args)
            == crate::agent::result_format::ResultFormat::Detailed
            || lines.len() <= keep
        {
            return Ok(full);
        }
        // CONCISE (default): the first rows verbatim, the hidden rows as per-file counts, and the
        // whole list on disk — the model narrows or reads the file instead of re-searching.
        let path = crate::agent::observe::spill_to_scratch("search_files", &full);
        let mut per_file: Vec<(usize, usize)> = Vec::new();
        for i in keep..lines.len() {
            if !is_match_row[i] {
                continue;
            }
            match per_file.last_mut() {
                Some((f, c)) if *f == owner[i] => *c += 1,
                _ => per_file.push((owner[i], 1)),
            }
        }
        let hidden: usize = per_file.iter().map(|(_, c)| c).sum();
        let mut listing: Vec<String> = per_file
            .iter()
            .take(12)
            .map(|(f, c)| format!("{} ({c})", results[*f].0.display()))
            .collect();
        if per_file.len() > 12 {
            listing.push("…".to_string());
        }
        let mut out = format!("{header}\n{}", lines[..keep].join("\n"));
        out.push_str(&format!(
            "\n…[{hidden} more match(es) in {} file(s): {}",
            per_file.len(),
            listing.join(", ")
        ));
        if let Some(p) = path {
            out.push_str(&format!(" · full list at {} (file_read it)", p.display()));
        }
        out.push_str(" · format:\"detailed\" for every row, or narrow with path/glob]");
        out.push_str(&trailer);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::current_dir().unwrap().canonicalize().unwrap()
    }

    #[test]
    fn finds_a_known_string_in_the_repo() {
        // Search the crate for a string we KNOW exists (this tool's own name in its description).
        let t = SearchFiles::new(root());
        let out = t
            .execute(&serde_json::json!({"pattern": "canonical content search", "glob": "*.rs"}))
            .unwrap();
        assert!(
            out.contains("search.rs:"),
            "should locate this file; got:\n{out}"
        );
        assert!(out.contains("match(es)"));
    }

    #[test]
    fn no_match_reports_cleanly() {
        let t = SearchFiles::new(root());
        // Build the needle at runtime so its literal can't appear contiguously in this source file
        // (a fixed literal would match its own test line — the walker really does find everything).
        let needle = format!("{}{}{}", "zNoMatch", "Tok", "42xQ");
        let out = t
            .execute(&serde_json::json!({ "pattern": needle }))
            .unwrap();
        assert!(out.starts_with("no matches"), "got: {out}");
    }

    #[test]
    fn bad_regex_errors() {
        let t = SearchFiles::new(root());
        assert!(t.execute(&serde_json::json!({"pattern": "("})).is_err());
        assert!(t.execute(&serde_json::json!({})).is_err()); // missing pattern
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aizen-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn decode_text_handles_utf16_bom_le_and_be() {
        // P2.2: a UTF-16 file (BOM'd) must decode to searchable text, not be dropped as binary.
        let le = {
            let mut b = vec![0xFF, 0xFE];
            for u in "hello".encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b
        };
        assert_eq!(decode_text(&le).as_deref(), Some("hello"));
        let be = {
            let mut b = vec![0xFE, 0xFF];
            for u in "hello".encode_utf16() {
                b.extend_from_slice(&u.to_be_bytes());
            }
            b
        };
        assert_eq!(decode_text(&be).as_deref(), Some("hello"));
    }

    #[test]
    fn decode_text_handles_bomless_utf16le() {
        // BOM-less UTF-16LE ASCII: NUL on every odd byte. The alternating-NUL heuristic must catch it
        // instead of the plain NUL sniff writing it off as binary.
        let mut b = Vec::new();
        for u in "function main".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        let decoded = decode_text(&b).expect("bom-less utf16le should decode as text");
        assert!(decoded.contains("function main"), "got: {decoded:?}");
    }

    #[test]
    fn decode_text_rejects_real_binary() {
        // Genuine binary: NULs present but scattered across BOTH parities (neither the odd nor the
        // even offsets are dominated by NUL), so neither the BOM check nor the alternating-NUL
        // UTF-16 heuristic fires — it stays classified as binary and is skipped.
        let b = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0xFF, 0x89, 0x13, 0x37, 0x00];
        assert_eq!(decode_text(&b), None);
    }

    #[test]
    fn utf16_file_is_searched_end_to_end() {
        // A UTF-16LE-BOM file on disk must be found by a content search (regression for the old NUL
        // sniff that skipped it).
        let root = tmp("utf16");
        let mut bytes = vec![0xFF, 0xFE];
        for u in "let secret = 42;".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(root.join("cfg.ts"), &bytes).unwrap();
        let t = SearchFiles::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern": "secret"}))
            .unwrap();
        assert!(out.contains("cfg.ts"), "utf-16 file searched: {out}");
    }

    #[test]
    fn context_lines_wrap_matches_and_merge_overlapping_windows() {
        let root = tmp("context");
        // Lines 1..=12; matches at 4 and 6 (windows ±2 overlap → ONE merged group 2..=8) and at 12.
        let body = (1..=12)
            .map(|i| {
                if i == 4 || i == 6 || i == 12 {
                    format!("needle line {i}")
                } else {
                    format!("plain line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("ctx.txt"), body).unwrap();
        let t = SearchFiles::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern": "needle", "context": 2}))
            .unwrap();
        assert!(
            out.starts_with("3 match(es) in 1 file(s):"),
            "header counts MATCHES, not rows: {out}"
        );
        // Match rows use `path:line:`, context rows `path-line-` (grep -C convention).
        assert!(out.contains("ctx.txt:4: needle line 4"), "{out}");
        assert!(out.contains("ctx.txt-3- plain line 3"), "{out}");
        // Overlapping windows merged: line 5 (between the 4 and 6 matches) appears exactly once.
        assert_eq!(out.matches("plain line 5").count(), 1, "{out}");
        // Non-contiguous groups separated by `--`: the 2..=8 block vs the 10..=12 block.
        assert!(out.contains("--"), "{out}");
        assert!(
            !out.contains("plain line 9"),
            "gap line outside both windows: {out}"
        );
        // context:0 (default) keeps the classic flat shape.
        let flat = t2_flat(&t);
        assert!(flat.contains("ctx.txt:4: needle line 4"), "{flat}");
        assert!(!flat.contains("-3-"), "no context rows by default: {flat}");
    }

    fn t2_flat(t: &SearchFiles) -> String {
        t.execute(&serde_json::json!({"pattern": "needle"}))
            .unwrap()
    }

    #[test]
    fn reports_skipped_oversized_files() {
        // P2.3: a file over the size cap is skipped but REPORTED so the model knows to target it.
        let root = tmp("skip-large");
        let big = "x".repeat((MAX_FILE_BYTES + 1024) as usize);
        std::fs::write(root.join("huge.log"), &big).unwrap();
        std::fs::write(root.join("small.txt"), "nothing here").unwrap();
        let t = SearchFiles::new(root);
        let out = t.execute(&serde_json::json!({"pattern": "xxxxx"})).unwrap();
        assert!(out.contains("skipped 1 file"), "skip reported: {out}");
    }

    #[test]
    fn concise_shows_the_first_rows_and_counts_the_rest_per_file() {
        let root = tmp("concise-rows");
        let many: String = (1..=60).map(|i| format!("needle {i}\n")).collect();
        std::fs::write(root.join("a.txt"), &many).unwrap();
        let few: String = (1..=5).map(|i| format!("needle b{i}\n")).collect();
        std::fs::write(root.join("b.txt"), &few).unwrap();
        let t = SearchFiles::new(root);
        let keep = crate::agent::result_format::CONCISE_SEARCH_ROWS;
        let rows = |out: &str| {
            out.lines()
                .filter(|l| l.starts_with("a.txt:") || l.starts_with("b.txt:"))
                .count()
        };

        let out = t
            .execute(&serde_json::json!({"pattern": "needle"}))
            .unwrap();
        assert!(out.starts_with("65 match(es) in 2 file(s):"), "{out}");
        assert_eq!(rows(&out), keep, "concise shows the first rows: {out}");
        assert!(
            out.contains("…[25 more match(es) in 2 file(s): a.txt (20), b.txt (5)"),
            "{out}"
        );
        assert!(out.contains("full list at "), "{out}");
        assert!(out.contains("format:\"detailed\""), "{out}");

        let all = t
            .execute(&serde_json::json!({"pattern": "needle", "format": "detailed"}))
            .unwrap();
        assert_eq!(rows(&all), 65, "detailed shows every row: {all}");
        assert!(!all.contains("more match(es)"), "{all}");
    }

    // ---------------------------------------------------------------------
    // Literal prefilter
    // ---------------------------------------------------------------------

    fn req_strings(pattern: &str) -> Vec<String> {
        pick_literals(extract_requirements(pattern))
            .into_iter()
            .map(|r| String::from_utf8(r.bytes).unwrap())
            .collect()
    }

    #[test]
    fn extractor_pulls_required_literals_from_concat_paths() {
        assert_eq!(req_strings("foo.*bar"), vec!["foo", "bar"]);
        assert_eq!(req_strings("fn\\s+main"), vec!["main", "fn"]);
        assert_eq!(req_strings("needle"), vec!["needle"]);
        // Anchors are zero-width — the run on either side is still required.
        assert_eq!(req_strings("^foo$"), vec!["foo"]);
        assert_eq!(req_strings("foo.*bar.*baz"), vec!["foo", "bar", "baz"]);
        assert_eq!(req_strings("https?://"), vec!["http", "://"]);
    }

    #[test]
    fn extractor_handles_alternation_and_nullable_constructs() {
        // Top-level alternation poisons EVERYTHING (a match may come from either branch).
        assert!(req_strings("foo|bar").is_empty());
        assert!(req_strings("foo$|bar$").is_empty());
        // Alternation inside a group is safe: the group is skipped, outer literals still required.
        assert_eq!(req_strings("pre(foo|bar)post"), vec!["post", "pre"]);
        // `*`/`?` un-require only the single preceding char (`foos?` still requires `foo`).
        assert_eq!(req_strings("foos?bar"), vec!["foo", "bar"]);
        assert_eq!(req_strings("colou*r"), vec!["colo", "r"]);
        assert_eq!(req_strings("foo?"), vec!["fo"]);
        assert_eq!(req_strings("fo*"), vec!["f"]);
        // `+` keeps the atom required but closes the run.
        assert_eq!(req_strings("ab+c"), vec!["ab", "c"]);
        // Character classes are "one of several" — not literal; text after them still is.
        assert_eq!(req_strings("[Ff]oo"), vec!["oo"]);
        assert_eq!(req_strings("x[0-9]+y"), vec!["x", "y"]);
        // `{n,}` n≥1 keeps the atom; `{0,…}` un-requires it; a malformed brace is a literal `{`.
        assert_eq!(req_strings("fo{2,}bar"), vec!["bar", "fo"]);
        assert_eq!(req_strings("fo{0,2}bar"), vec!["bar", "f"]);
    }

    #[test]
    fn extractor_handles_escapes_groups_and_backrefs() {
        // Escaped metachars are literal.
        assert_eq!(req_strings("foo\\.rs"), vec!["foo.rs"]);
        assert_eq!(req_strings("a\\*b"), vec!["a*b"]);
        // Class escapes close the run.
        assert_eq!(req_strings("v\\d+"), vec!["v"]);
        assert_eq!(req_strings("\\w+handle"), vec!["handle"]);
        // Backreferences are NOT the literal digit — `(a|b)\1` has no required "1".
        assert_eq!(req_strings("x\\1y"), vec!["x", "y"]);
        // Groups are skipped whole; text after them is still required.
        assert_eq!(req_strings("x(?:abc)y"), vec!["x", "y"]);
        // Inline flags anywhere → the CALLER refuses to build a prefilter at all.
        assert!(LiteralPrefilter::build("(?i)foo", false).is_none());
        assert!(LiteralPrefilter::build("foo(?-u:bar)", false).is_none());
        // …but a non-capturing group marker is not a flag.
        assert!(LiteralPrefilter::build("x(?:abc)y", false).is_some());
    }

    #[test]
    fn prefilter_case_rules() {
        // Case-sensitive: any literal usable as-is, non-ASCII included.
        let pf = LiteralPrefilter::build("Café.*au", false).unwrap();
        assert!(pf.matches_text("un Café au lait"));
        assert!(!pf.matches_text("un cafe au lait"));
        // ignore_case: safe ASCII literals fold on BOTH sides — different-cased text still passes.
        let pf = LiteralPrefilter::build("Needle", true).unwrap();
        assert_eq!(pf.needles(), &[b"needle".to_vec()]);
        assert!(pf.matches_text("a NEEDLE in the hay"));
        assert!(!pf.matches_text("nothing here"));
        // ignore_case + non-ASCII literal: dropped (Unicode folding could change its bytes), but
        // a proven-safe ASCII sibling still gates.
        let pf = LiteralPrefilter::build("café.*menu", true).unwrap();
        assert_eq!(pf.needles(), &[b"menu".to_vec()]);
        // …and if the ONLY literal is non-ASCII under ignore_case, no prefilter is built.
        assert!(LiteralPrefilter::build("café", true).is_none());
        // k/K/s/S fold outside ASCII (KELVIN SIGN, LONG S) under Unicode folding — dropped too.
        assert!(LiteralPrefilter::build("sky", true).is_none());
        assert!(LiteralPrefilter::build("K", true).is_none());
        // …but safe under case-SENSITIVE search.
        assert!(LiteralPrefilter::build("sky", false).is_some());
    }

    /// The prefilter is only legal if it can NEVER skip a file the regex would match in. Verify
    /// that property directly against `regex` over a corpus of patterns × texts, for both
    /// case-sensitive and case-insensitive searches.
    #[test]
    fn prefilter_never_changes_results() {
        let patterns = [
            "foo",
            "foo.*bar",
            "^foo$",
            "fn\\s+main\\(",
            "foo|bar",
            "foos?",
            "colou*r",
            "[Ff]oo",
            "x[0-9]+y",
            "https?://",
            "foo\\.rs",
            "v\\d+",
            "\\w+handle",
            "needle",
            "NEEDLE",
            "café",
            "café.*menu",
            "a.c",
            "ab*c",
            "ab+c",
            "z{2,3}",
            "x(?:abc)y",
            "(foo|bar)baz",
            "pre(foo|bar)post",
            ".*",
            "^",
            "$",
            "\\d+",
            "foo$|bar$",
            "qux{0,2}quux",
            "foo(bar)?baz",
            "partial(?i)CASE",
            "sky",
            "K",
            "K.*sky",
            "(a|b)\\1",
            "x\\1y",
            "fo{2,}bar",
            "cafÉ.*MENU",
        ];
        let texts = [
            "foo",
            "FOO",
            "Foo bar\nbaz",
            "fn main() {\n}\n",
            "fn  main(",
            "foos and bars",
            "colour color colouur",
            "Foo\nfoo\nFOO",
            "x42y x7y",
            "https://a http://b",
            "foo.rs foo_rs",
            "v1 v42 w9",
            "my_handle handler",
            "a needle\nNEEDLE here",
            "un café au lait\nCAFÉ menu",
            "café MENU",
            "cafÉ and a MENU",
            "abc axc a.c",
            "abbc ac abc",
            "zzz zz zzzzz",
            "xabcy xy",
            "foobaz barbaz",
            "prebarpost prepost",
            "anything at all",
            "",
            "  \n",
            "123 456",
            "foo\nbar",
            "bar",
            "quxquux quux",
            "foobarbaz foobaz",
            "partialCase PARTIALcase",
            "bar\nfoo bar\n",
            "the sky is blue",
            "SKY",
            "K is for kelvin",
            "k K k",
            "aa bb ab",
            "xaay xby",
            "foobar",
            "FOOBAR",
        ];
        for &p in &patterns {
            for &ic in &[false, true] {
                let re = match RegexBuilder::new(p).case_insensitive(ic).build() {
                    Ok(r) => r,
                    Err(_) => continue, // invalid pattern never reaches the prefilter anyway
                };
                let pf = LiteralPrefilter::build(p, ic);
                for t in &texts {
                    let regex_hit = re.is_match(t);
                    if let Some(pf) = &pf {
                        let pf_pass = pf.matches_text(t);
                        assert!(
                            pf_pass || !regex_hit,
                            "prefilter REJECTED a text the regex matches: pattern={p:?} ic={ic} text={t:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn prefilter_gate_skips_non_candidate_files_but_finds_the_match() {
        // End to end: with a literal-bearing pattern the hit is still found (the gate is a pure
        // optimization — output is identical to a regex-only walk).
        let root = tmp("prefilter-e2e");
        std::fs::write(root.join("hit.rs"), "fn compute_total() {}\n").unwrap();
        std::fs::write(root.join("miss.rs"), "fn unrelated() {}\n").unwrap();
        let t = SearchFiles::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern": "compute\\w*total"}))
            .unwrap();
        assert!(
            out.contains("hit.rs:1:"),
            "match found through the gate: {out}"
        );
        assert!(
            !out.contains("miss.rs"),
            "non-candidate never surfaces: {out}"
        );
    }

    #[test]
    fn files_with_matches_returns_only_paths() {
        let root = tmp("fwm");
        std::fs::write(root.join("a.rs"), "needle here\nand needle again\n").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.rs"), "needle\n").unwrap();
        std::fs::write(root.join("c.rs"), "nothing\n").unwrap();
        let t = SearchFiles::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches"}))
            .unwrap();
        assert!(out.starts_with("2 file(s) with matches:"), "{out}");
        assert!(out.contains("a.rs"), "{out}");
        assert!(out.contains("b.rs"), "{out}");
        assert!(!out.contains("c.rs"), "{out}");
        // Paths only — no row text, no match line content, no per-line colon rows.
        assert!(!out.contains("needle here"), "no row text leaks: {out}");
        assert!(!out.contains("a.rs:1:"), "no line numbers: {out}");

        // Anchored patterns agree with content mode (per-line probe, not whole-text).
        let anchored = t
            .execute(
                &serde_json::json!({"pattern": "^needle$", "output_mode": "files_with_matches"}),
            )
            .unwrap();
        assert!(
            anchored.starts_with("1 file(s) with matches:"),
            "{anchored}"
        );
        assert!(anchored.contains("b.rs"), "{anchored}");
        assert!(!anchored.contains("a.rs"), "{anchored}");

        // No-match and cap behavior mirror content mode.
        let none = t
            .execute(&serde_json::json!({"pattern": "absent", "output_mode": "files_with_matches"}))
            .unwrap();
        assert!(none.starts_with("no matches"), "{none}");
        let capped = t
            .execute(&serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches", "max_results": 1}))
            .unwrap();
        assert!(capped.contains("capped at 1 files"), "{capped}");
    }
}
