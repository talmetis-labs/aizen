//! Hyperlink detection and OSC 8 injection for the retained TUI.
//!
//! After each `terminal.draw()` call, `inject_hyperlinks` walks the visible transcript rows,
//! detects URLs (`https?://…`) and file paths (`src/foo.rs`, `C:\…`, `/abs/path`), then writes
//! OSC 8 escape sequences directly via `backend_mut()` — bypassing ratatui's cell model entirely,
//! exactly like the screensaver sixel blitter. ratatui's cell diff never sees the OSC 8 bytes so
//! they are never overwritten mid-session; a `terminal.clear()` (Ctrl-L) wipes them, but the next
//! draw + inject restores them.
//!
//! ## Why post-draw injection?
//! ratatui 0.30 `Cell` has no hyperlink field and no OSC 8 feature flag. Injecting raw bytes into
//! a cell's symbol would confuse the width-accounting code (OSC 8 sequences contain visible-width
//! chars like `]`, `;`). Post-draw is the only race-free approach that works with this version.
//!
//! ## OSC 8 format (widely supported: WezTerm, Windows Terminal 1.19+, iTerm2, foot, …)
//! ```text
//! \x1b]8;;URL\x1b\\ <visible text> \x1b]8;;\x1b\\
//! ```
//! The empty second parameter is "no params"; the terminator is ST (`\x1b\\`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

/// One shared physical budget for transcript painting and post-draw overprinting. The last
/// column belongs to the scrollbar, even in a pane too small to hold any text.
pub(crate) fn transcript_content_width(width: u16) -> u16 {
    width.saturating_sub(1)
}

/// Measure whole extended graphemes, just like the wrapper and ratatui's text cells.
pub(crate) fn display_width(text: &str) -> usize {
    text.graphemes(true).map(console::measure_text_width).sum()
}

/// Keep only the numeric SGR dialect understood by the retained painter. Consume entire control
/// strings (OSC/DCS/APC/PM/SOS), including their payload, rather than leaking it as visible text.
pub(crate) fn sanitize_sgr(input: &str) -> String {
    fn control_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, osc: bool) {
        while let Some(c) = chars.next() {
            if c == '\u{009c}' || (osc && c == '\x07') {
                break;
            }
            if c == '\x1b' && chars.peek() == Some(&'\\') {
                chars.next();
                break;
            }
        }
    }
    fn csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String) {
        let mut params = String::new();
        for c in chars.by_ref() {
            if ('\u{0040}'..='\u{007e}').contains(&c) {
                if c == 'm' && params.chars().all(|p| p.is_ascii_digit() || p == ';') {
                    out.push_str("\x1b[");
                    out.push_str(&params);
                    out.push('m');
                }
                break;
            }
            params.push(c);
        }
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => csi(&mut chars, &mut out),
                Some(']') => control_string(&mut chars, true),
                Some('P' | 'X' | '^' | '_') => control_string(&mut chars, false),
                Some(c) if (' '..='/').contains(&c) => {
                    while chars.peek().is_some_and(|c| (' '..='/').contains(c)) {
                        chars.next();
                    }
                    chars.next();
                }
                _ => {}
            },
            '\u{009b}' => csi(&mut chars, &mut out),
            '\u{009d}' => control_string(&mut chars, true),
            '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => control_string(&mut chars, false),
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}

/// A complete printable grapheme, including SGR changes embedded between its codepoints. `prefix`
/// replays the style in force before it, so a slice or a wrapped continuation can stand alone.
pub(crate) struct SgrGlyph {
    pub text: String,
    pub raw: String,
    pub prefix: String,
    pub width: usize,
}

pub(crate) fn sgr_glyphs(input: &str) -> (Vec<SgrGlyph>, String) {
    let safe = sanitize_sgr(input);
    let mut plain = String::with_capacity(safe.len());
    let mut events = Vec::new();
    let mut rest = safe.as_str();
    while let Some(i) = rest.find("\x1b[") {
        plain.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find('m').expect("sanitized SGR terminator") + 1;
        events.push((plain.len(), rest[..end].to_string()));
        rest = &rest[end..];
    }
    plain.push_str(rest);
    let mut events = events.into_iter().peekable();
    let mut active = String::new();
    let mut glyphs = Vec::new();
    for (start, text) in plain.grapheme_indices(true) {
        let end = start + text.len();
        let prefix = active.clone();
        let mut raw = String::new();
        let mut at = start;
        while events.peek().is_some_and(|(pos, _)| *pos < end) {
            let (pos, seq) = events.next().unwrap();
            raw.push_str(&plain[at..pos]);
            raw.push_str(&seq);
            // A leading full reset makes earlier history irrelevant. Keep compound sequences
            // intact: zeroes inside an RGB colour are NOT reset parameters.
            let params = &seq[2..seq.len() - 1];
            if params.is_empty() || params.split(';').next() == Some("0") {
                active.clear();
            }
            active.push_str(&seq);
            at = pos;
        }
        raw.push_str(&plain[at..end]);
        glyphs.push(SgrGlyph {
            text: text.to_string(),
            raw,
            prefix,
            width: console::measure_text_width(text),
        });
    }
    let trailing = events.map(|(_, seq)| seq).collect::<String>();
    (glyphs, trailing)
}

/// A detected hyperlink span within a plain-text row.
#[derive(Debug, Clone)]
pub struct LinkSpan {
    /// Display-cell column where the link text starts (0-indexed).
    pub col: usize,
    /// Display-cell width of the link text (number of cells).
    pub width: usize,
    /// The URL to open (already formatted as `https://…` or `file:///…`).
    pub url: String,
    /// The visible text (subset of the row).
    ///
    /// The injector re-slices the row from `col`/`width` to keep the SGR colours, so it never reads
    /// this field; the tests assert on it, because "which text got linked" is the thing a reader
    /// needs to see to trust the scan.
    #[allow(dead_code)]
    pub text: String,
}

/// A placement-bound alias in a tool target. Offsets are UTF-8 boundaries in `text`.
#[derive(Clone, Debug, Default)]
pub(crate) struct DisplayTarget {
    pub text: String,
    pub paths: Vec<(
        std::ops::Range<usize>,
        crate::core::file_placement::DisplayPath,
    )>,
}

/// One painted fragment of an alias, aligned with the exact wrapped row and frame.
#[derive(Clone, Debug)]
pub(crate) struct PathFragment {
    pub id: (u64, usize),
    pub span: LinkSpan,
    pub actual: PathBuf,
    pub first: bool,
    pub last: bool,
}

pub(crate) fn wrap_path_fragments(
    rows: &[String],
    start: usize,
    display: &DisplayTarget,
    seq: u64,
) -> Vec<Vec<PathFragment>> {
    let mut result = vec![Vec::new(); rows.len()];
    let Some(target_end) = start.checked_add(display.text.len()) else {
        return result;
    };
    // Sanitization or a narrow-pane glyph substitution must not move an alias onto other text.
    let mut header = String::new();
    for row in rows {
        header.push_str(row);
        if header.len() >= target_end {
            break;
        }
    }
    if header.get(start..target_end) != Some(display.text.as_str()) {
        return result;
    }
    let mut offset = 0;
    for (row, fragments) in rows.iter().zip(&mut result) {
        let end = offset + row.len();
        for (index, (range, path)) in display.paths.iter().enumerate() {
            if display.text.get(range.clone()) != Some(path.label()) {
                continue;
            }
            let a = start + range.start;
            let b = start + range.end;
            let lo = a.max(offset);
            let hi = b.min(end);
            if lo >= hi {
                continue;
            }
            let Some(text) = row.get(lo - offset..hi - offset) else {
                continue;
            };
            let Some(url) = file_url(path.actual()).filter(|url| {
                !url.chars().any(char::is_control)
                    && url::Url::parse(url)
                        .ok()
                        .is_some_and(|url| url.host_str().is_none())
            }) else {
                continue;
            };
            fragments.push(PathFragment {
                id: (seq, index),
                span: LinkSpan {
                    col: display_width(&row[..lo - offset]),
                    width: display_width(text),
                    text: text.to_owned(),
                    url,
                },
                actual: path.actual().to_path_buf(),
                first: lo == a,
                last: hi == b,
            });
        }
        offset = end;
        if offset >= start + display.text.len() {
            break;
        }
    }
    result
}

/// What a shape-scan matched, before anything has been checked against the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// `https://…` / `http://…` — unambiguous, needs no validation.
    Url,
    /// Something path-SHAPED. Only a filesystem probe can say whether it really is one.
    Path,
}

/// A link candidate located purely by SHAPE. Char indices into the row, half-open `[start, end)`.
///
/// Kept separate from [`LinkSpan`] because shape and truth are different questions: `và/hoặc` and
/// `/usr/bin` have the SAME shape, so no amount of shape rules can separate them. Producing
/// candidates is pure (offline-testable); deciding which survive is [`resolve_path`]'s job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub start: usize,
    pub end: usize,
    pub kind: CandidateKind,
}

/// Locate link candidates in a plain-text row by shape alone. **Pure** — never touches the disk.
pub fn scan_row_shapes(row: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let chars: Vec<char> = row.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        // ── URL: https?:// ──────────────────────────────────────────────────────────────
        if starts_scheme(&chars, i) {
            let start = i;
            while i < len && !is_url_break(chars[i]) {
                i += 1;
            }
            let raw: String = chars[start..i].iter().collect();
            let trimmed = trim_link_tail(&raw);
            // `text.len() > 10` (bytes) is the long-standing noise floor — `http://x` is far more
            // likely to be prose about URLs than a link worth underlining. Kept deliberately.
            if trimmed.len() > 10 {
                out.push(Candidate {
                    start,
                    end: start + trimmed.chars().count(),
                    kind: CandidateKind::Url,
                });
            }
            continue;
        }

        // ── file path ───────────────────────────────────────────────────────────────────
        if let Some(end) = try_path_shape(&chars, i) {
            out.push(Candidate {
                start: i,
                end,
                kind: CandidateKind::Path,
            });
            i = end;
            continue;
        }

        i += 1;
    }
    out
}

/// Scan a row and return the spans that are really links, using `resolve` to decide whether a
/// path-shaped candidate names a file that actually exists.
///
/// Thin wrapper over [`scan_window_with`] with an unbounded content width, so no row can look
/// "full" and nothing is ever joined — one row, judged on its own.
#[cfg(test)]
fn scan_row_with(row: &str, resolve: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<LinkSpan> {
    let rows = [row.to_string()];
    scan_window_with(&rows, 0, 1, usize::MAX, resolve)
        .into_iter()
        .map(|l| l.span)
        .collect()
}

// ── helpers ─────────────────────────────────────────────────────────────────────────────────────

fn starts_scheme(chars: &[char], i: usize) -> bool {
    let rest_is = |lit: &str| {
        let n = lit.chars().count();
        i + n <= chars.len() && chars[i..i + n].iter().copied().eq(lit.chars())
    };
    rest_is("https://") || rest_is("http://")
}

fn is_url_break(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '"' | '\'' | '>' | '<' | ')' | ']' | '}' | '`'
    )
}

/// Trim trailing punctuation that is usually sentence punctuation, not part of the link.
///
/// Applies to paths as well as URLs: `/help...` is prose, and leaving the dots on made the whole
/// token fail every subsequent check for the wrong reason.
fn trim_link_tail(s: &str) -> &str {
    let mut end = s.len();
    while end > 0 {
        match s.as_bytes()[end - 1] {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b')' | b']' | b'}' => end -= 1,
            _ => break,
        }
    }
    &s[..end]
}

/// A path may only START at a word boundary: beginning of row, or after a character that could not
/// itself be part of a path.
///
/// This is the single rule that separates `và/hoặc`, `and/or`, `parser/lexer`, `15.000/kg` and
/// `input/output` from a real `/usr/bin`. Before it, ANY `/` began a path scan, so every Vietnamese
/// `và/hoặc` in the transcript grew a `file:///hoặc` link.
///
/// Rejecting a preceding SEPARATOR (not just an alphanumeric) is what stops a scan from starting in
/// the middle of a longer path-shaped token. Without it `\\host\share\x.rs` yielded a candidate
/// beginning at `host` — the UNC marker sliced off, so the UNC guard downstream never recognised it.
fn at_word_boundary(chars: &[char], i: usize) -> bool {
    i == 0 || !is_path_char(chars[i - 1])
}

/// Match a path by SHAPE starting at `chars[i]`, returning the exclusive end index.
/// Says nothing about whether the path exists — that is [`resolve_path`]'s question.
fn try_path_shape(chars: &[char], i: usize) -> Option<usize> {
    let len = chars.len();
    if !at_word_boundary(chars, i) {
        return None;
    }

    // Windows absolute: letter + `:\` or `:/`
    let is_win_abs = i + 2 < len
        && chars[i].is_ascii_alphabetic()
        && chars[i + 1] == ':'
        && (chars[i + 2] == '\\' || chars[i + 2] == '/');
    let is_unix_abs = chars[i] == '/';
    let is_rel = !is_win_abs
        && !is_unix_abs
        && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '.'));

    if !is_win_abs && !is_unix_abs && !is_rel {
        return None;
    }

    let mut j = i;
    while j < len && is_path_char(chars[j]) {
        j += 1;
    }
    if j == i {
        return None;
    }

    // Drop sentence punctuation before judging the shape, so `src/foo.rs.` is still `src/foo.rs`.
    let raw: String = chars[i..j].iter().collect();
    let trimmed = trim_link_tail(&raw);
    if trimmed.is_empty() || is_unc(trimmed) {
        // UNC is refused HERE, at the shape layer, so no resolver ever sees it — see `is_unc`.
        return None;
    }
    let end = i + trimmed.chars().count();

    // Relative paths need a separator AND a known extension: a bare word is far more likely to be
    // prose. Absolute paths are NOT held to the extension rule — `/usr/bin/python` is a real path —
    // because the existence probe is what filters them. (The old `len >= 3` floor let `/or` through.)
    if is_rel && (!trimmed.contains('/') && !trimmed.contains('\\') || !has_source_ext(trimmed)) {
        return None;
    }

    Some(end)
}

/// Reject UNC paths (`\\server\share`, `//server/share`). Probing one opens an SMB connection to a
/// host named by model output, which can leak credentials on Windows. A hyperlink is never worth
/// that, so the refusal lives in the SHAPE layer — no resolver is ever handed a UNC candidate.
fn is_unc(p: &str) -> bool {
    p.starts_with("\\\\") || p.starts_with("//")
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '-' | '.' | '/' | '\\' | ':')
        || (!c.is_control() && console::measure_text_width(&c.to_string()) == 0)
        || ('\u{1f000}'..='\u{1faff}').contains(&c)
}

/// Known source/config file extensions that should be linkified.
fn has_source_ext(path: &str) -> bool {
    const EXTS: &[&str] = &[
        ".rs", ".toml", ".json", ".md", ".txt", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".c",
        ".cpp", ".h", ".hpp", ".java", ".kt", ".swift", ".rb", ".sh", ".yaml", ".yml", ".env",
        ".lock", ".html", ".css", ".scss",
    ];
    let lower = strip_line_suffix(path).to_lowercase();
    EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// Drop a trailing `:line` / `:line:col`, leaving the filename. A Windows drive colon is never
/// touched because the suffix must be all digits and the drive letter is not.
fn strip_line_suffix(path: &str) -> &str {
    let mut s = path;
    for _ in 0..2 {
        match s.rsplit_once(':') {
            Some((head, tail))
                if !tail.is_empty()
                    && tail.bytes().all(|b| b.is_ascii_digit())
                    && !head.is_empty() =>
            {
                s = head;
            }
            _ => break,
        }
    }
    s
}

// ── path resolution (the only part that touches the disk) ───────────────────────────────────────

/// Project root, resolved ONCE. `config::project_root()` shells out to `git rev-parse` on its first
/// branch — utterly unacceptable at the ~9fps this is called from, so it is cached for the process.
fn project_root_cached() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(crate::core::config::project_root)
}

/// How long a resolution stays cached. Long enough that a steady screen costs zero syscalls, short
/// enough that a file the agent just created becomes clickable while the reader is still looking.
const RESOLVE_TTL: Duration = Duration::from_secs(3);
/// Cache ceiling; on overflow the whole map is dropped (cheaper than LRU bookkeeping, and the next
/// frame refills only what is actually on screen).
const RESOLVE_CACHE_CAP: usize = 1024;

#[allow(clippy::type_complexity)]
fn resolve_cache() -> &'static Mutex<HashMap<String, (Option<PathBuf>, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Option<PathBuf>, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A path-shaped candidate → the absolute path it names, but ONLY if that file really exists.
///
/// Existence is the one signal that separates a path from prose, because their shapes are identical.
fn resolve_path(cand: &str) -> Option<PathBuf> {
    if cand.is_empty() || is_unc(cand) {
        return None;
    }
    let now = Instant::now();
    if let Ok(cache) = resolve_cache().lock() {
        if let Some((hit, at)) = cache.get(cand) {
            if now.duration_since(*at) < RESOLVE_TTL {
                return hit.clone();
            }
        }
    }
    let found = probe_path(cand);
    if let Ok(mut cache) = resolve_cache().lock() {
        if cache.len() >= RESOLVE_CACHE_CAP {
            cache.clear();
        }
        cache.insert(cand.to_string(), (found.clone(), now));
    }
    found
}

/// Reject UNC paths (`\\server\share`, `//server/share`) BEFORE any syscall — see [`is_unc`].
fn probe_path(cand: &str) -> Option<PathBuf> {
    let p = Path::new(cand);
    if p.is_absolute() {
        return std::fs::metadata(p).is_ok().then(|| p.to_path_buf());
    }
    let mut roots: Vec<PathBuf> = vec![project_root_cached().to_path_buf()];
    if let Ok(cwd) = std::env::current_dir() {
        if !roots.contains(&cwd) {
            roots.push(cwd);
        }
    }
    roots.into_iter().find_map(|root| {
        let joined = root.join(p);
        std::fs::metadata(&joined).is_ok().then_some(joined)
    })
}

/// Absolute path → a `file:` URL. `Url::from_file_path` handles the Windows drive letter, `\`→`/`,
/// and percent-encoding of spaces and non-ASCII.
///
/// The old `format!("file://{path}")` produced an INVALID URL for relative paths: under RFC 8089 the
/// two slashes open an authority, so `file://src/ui/tui.rs` names a HOST called `src`. Those links
/// could never open anything.
fn file_url(abs: &Path) -> Option<String> {
    url::Url::from_file_path(abs).ok().map(|u| u.to_string())
}

/// Compute the display-cell column of `chars[idx]` (sum of char widths before it).
fn display_col(chars: &[char], idx: usize) -> usize {
    display_width(&chars[..idx.min(chars.len())].iter().collect::<String>())
}

// ── wrapped-link rejoin ─────────────────────────────────────────────────────────────────────────

/// How many rows above and below the viewport are scanned so a link split across the viewport edge
/// still resolves. Bounded (not "the whole transcript") to keep the per-frame cost O(visible) — the
/// transcript can be thousands of rows and this runs at ~9fps.
const REJOIN_WINDOW: usize = 4;

/// One link occupying one or more consecutive rows.
#[derive(Debug, Clone)]
pub struct RowLink {
    /// Absolute row index into `plain_rows`.
    pub row: usize,
    pub span: LinkSpan,
}

/// Strip a wrapped-row continuation prefix: an optional `▌` gutter bar (every assistant row carries
/// one — see `markdown::gutter`) plus the padding after it.
///
/// A LEADING SPACE with no gutter is deliberately not stripped. `char_chunks` hard-cuts a long token
/// with no padding, so a genuine continuation starts at column 0; a row that opens with a space is
/// ordinary prose. Allowing it made `see https://example.com/abcdefghij` + ` and then…` fuse into
/// the URL `…abcdefghijand`, which was never on screen.
fn strip_continuation_prefix(row: &str) -> (usize, &str) {
    let Some(after_bar) = row.strip_prefix('▌') else {
        return (0, row);
    };
    let rest = after_bar.trim_start_matches(' ');
    let skipped = row.len() - rest.len();
    (skipped, rest)
}

fn row_continues(row: &str, next: &str, content_width: usize) -> bool {
    if content_width == 0
        || content_width == usize::MAX
        || row.chars().last().is_none_or(is_url_break)
    {
        return false;
    }
    let (_, rest) = strip_continuation_prefix(next);
    let Some(first) = rest.graphemes(true).next() else {
        return false;
    };
    if first.chars().any(is_url_break)
        || first
            .chars()
            .any(|c| matches!(c, '└' | '│' | '◆' | '▸' | '→' | '✓' | '✗'))
        || rest.starts_with("https://")
        || rest.starts_with("http://")
    {
        return false;
    }
    // A new absolute path is a separate token, not an arbitrary URL suffix. A wrapped token
    // beginning with '/' still joins unless it carries a complete independent scheme/drive.
    let chars: Vec<char> = rest.chars().take(3).collect();
    if chars.len() == 3 && chars[0].is_ascii_alphabetic() && chars[1] == ':' {
        return false;
    }
    let used = display_width(row);
    used <= content_width && content_width - used < console::measure_text_width(first)
}

/// Find links in a rendered window. Scan joined hard-wrap runs BEFORE shape detection: a path's
/// extension (or even a URL's scheme in a tiny pane) may be several rows beyond its first piece.
/// All fragments share the complete URI, and are emitted only for rows in the requested window.
pub fn scan_window_with(
    plain_rows: &[String],
    lo: usize,
    hi: usize,
    content_width: usize,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Vec<RowLink> {
    let hi = hi.min(plain_rows.len());
    let lo = lo.min(hi);
    if lo == hi {
        return Vec::new();
    }
    let mut row = lo;
    while row > 0 && row_continues(&plain_rows[row - 1], &plain_rows[row], content_width) {
        row -= 1;
    }
    let mut out = Vec::new();
    while row < hi {
        let mut joined = plain_rows[row].clone();
        // (row, start char in joined run, chars skipped at that row's gutter, length)
        let mut pieces = vec![(row, 0usize, 0usize, joined.chars().count())];
        let mut next = row + 1;
        while next < plain_rows.len()
            && row_continues(&plain_rows[next - 1], &plain_rows[next], content_width)
        {
            let (skip, rest) = strip_continuation_prefix(&plain_rows[next]);
            let start = joined.chars().count();
            let skipped = plain_rows[next][..skip].chars().count();
            let len = rest.chars().count();
            joined.push_str(rest);
            pieces.push((next, start, skipped, len));
            next += 1;
        }
        let chars: Vec<char> = joined.chars().collect();
        for cand in scan_row_shapes(&joined) {
            let full: String = chars[cand.start..cand.end].iter().collect();
            let url = match cand.kind {
                CandidateKind::Url => full,
                CandidateKind::Path => match resolve(strip_line_suffix(&full))
                    .as_deref()
                    .and_then(file_url)
                {
                    Some(url) => url,
                    None => continue,
                },
            };
            for &(prow, begin, skipped, len) in &pieces {
                if prow < lo || prow >= hi {
                    continue;
                }
                let from = cand.start.max(begin);
                let to = cand.end.min(begin + len);
                if from >= to {
                    continue;
                }
                let row_chars: Vec<char> = plain_rows[prow].chars().collect();
                let s = skipped + from - begin;
                let e = skipped + to - begin;
                let col = display_col(&row_chars, s);
                out.push(RowLink {
                    row: prow,
                    span: LinkSpan {
                        col,
                        width: display_col(&row_chars, e).saturating_sub(col),
                        url: url.clone(),
                        text: row_chars[s..e].iter().collect(),
                    },
                });
            }
        }
        row = next;
    }
    out
}

// ── OSC 8 injection ─────────────────────────────────────────────────────────────────────────────

/// Build an OSC 8 hyperlink sequence wrapping `text` with `url`.
/// Format: `\x1b]8;;URL\x1b\\ TEXT \x1b]8;;\x1b\\`
pub fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Does the cell range `[x, x+w)` on screen row `y` fall inside any occluding rect?
fn occluded(rects: &[ratatui::layout::Rect], x: u16, y: u16, w: u16) -> bool {
    rects.iter().any(|r| {
        let (x, y, w) = (usize::from(x), usize::from(y), usize::from(w));
        y >= usize::from(r.y)
            && y < usize::from(r.y) + usize::from(r.height)
            && x < usize::from(r.x) + usize::from(r.width)
            && x + w > usize::from(r.x)
    })
}

/// Where the transcript sits on screen, and what is currently painted over it.
///
/// Bundled rather than passed loose because the injector writes at ABSOLUTE coordinates after
/// ratatui has composited the frame: geometry, occlusion and the caret are one consistent snapshot
/// of a single draw, and splitting them invites passing halves of two different frames.
#[derive(Debug, Clone, Default)]
pub struct InjectCtx {
    /// First transcript row visible at `area.y`.
    pub start: usize,
    /// Viewport height in rows.
    pub visible: usize,
    pub area: ratatui::layout::Rect,
    /// Rects painted OVER the transcript (overlay box, Copy menu). Spans intersecting one are
    /// skipped — otherwise link text is scribbled across a floating panel.
    pub occluders: Vec<ratatui::layout::Rect>,
    /// Where `frame.set_cursor_position` left the input caret. ratatui shows and positions it as the
    /// LAST step of `draw`, so moving the cursor here without putting it back strands the caret in
    /// the transcript.
    pub caret: Option<(u16, u16)>,
    /// Absolute transcript row of `plain_rows[0]` / `sgr_rows[0]`: the painter hands over only
    /// the rendered window, not the whole session.
    pub rows_offset: usize,
}

/// After `terminal.draw()`, walk the visible transcript rows and overprint each detected link span
/// with an OSC 8 sequence at the exact screen coordinates.
///
/// `sgr_rows` are the raw rendered strings (with SGR colour codes) for the full transcript;
/// `plain_rows` is the same content with SGR stripped. Both are indexed absolutely.
///
/// We use `MoveTo` + `Print` (raw crossterm) via `backend_mut()` — exactly like the screensaver
/// blitter — so ratatui's cell model is never disturbed.
#[cfg(test)]
pub fn inject_hyperlinks<W: std::io::Write>(
    out: &mut W,
    sgr_rows: &[String],
    plain_rows: &[String],
    ctx: &InjectCtx,
) {
    inject_hyperlinks_with_paths(out, sgr_rows, plain_rows, ctx, &[]);
}

pub(crate) fn inject_hyperlinks_with_paths<W: std::io::Write>(
    out: &mut W,
    sgr_rows: &[String],
    plain_rows: &[String],
    ctx: &InjectCtx,
    paths: &[Vec<PathFragment>],
) {
    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;

    let InjectCtx {
        start,
        visible,
        area,
        occluders,
        caret,
        rows_offset,
    } = ctx;
    let (start, visible, area, base) = (*start, *visible, *area, *rows_offset);
    let visible = visible.min(usize::from(area.height));
    let content_width = usize::from(transcript_content_width(area.width));
    if visible == 0 || content_width == 0 {
        return;
    }

    // Scan a window that reaches past both edges of the viewport so a URL wrapped across the top or
    // bottom boundary is still joined into one link (`scan_window_with` needs the off-screen half).
    // Window-relative: the rows handed over start at absolute row `base`.
    let lo = start.saturating_sub(REJOIN_WINDOW).saturating_sub(base);
    let hi = start
        .saturating_add(visible)
        .saturating_add(REJOIN_WINDOW)
        .saturating_sub(base)
        .min(plain_rows.len());
    // Mirror `draw_transcript`'s content budget exactly: the scrollbar overlays the rightmost
    // column, so content gets `width - 1`. A wider assumption here would join/split wrapped URLs
    // one cell off from how the painter wrapped them.
    let mut links = scan_window_with(plain_rows, lo, hi, content_width, &resolve_path);
    links.retain(|link| {
        !paths.get(link.row).is_some_and(|fragments| {
            fragments.iter().any(|p| {
                link.span.col < p.span.col + p.span.width
                    && p.span.col < link.span.col + link.span.width
            })
        })
    });
    for (row, fragments) in paths.iter().enumerate().take(hi).skip(lo) {
        links.extend(fragments.iter().map(|path| RowLink {
            row,
            span: path.span.clone(),
        }));
    }

    let mut wrote = false;
    for link in &links {
        // Rows outside the viewport were only scanned for context — they have no screen position.
        let Some(screen_row) = link
            .row
            .checked_add(base)
            .and_then(|row| row.checked_sub(start))
        else {
            continue;
        };
        if screen_row >= visible {
            continue;
        }
        let Some(sgr) = sgr_rows.get(link.row) else {
            continue;
        };
        let span = &link.span;
        if span.col >= content_width || span.url.chars().any(char::is_control) {
            continue;
        }
        let Some(y) = usize::from(area.y)
            .checked_add(screen_row)
            .and_then(|y| u16::try_from(y).ok())
        else {
            continue;
        };
        let Some(x) = usize::from(area.x)
            .checked_add(span.col)
            .and_then(|x| u16::try_from(x).ok())
        else {
            continue;
        };
        // The scrollbar and sidebar are outside this budget. Keep the full URI in OSC8 metadata,
        // but NEVER use the original (possibly overlong) span width for the raw printable slice.
        let w = span.width.min(content_width - span.col);
        let coloured_text = extract_sgr_slice(sgr, span.col, w);
        let printed_w = display_width(console::strip_ansi_codes(&coloured_text).as_ref());
        if printed_w == 0
            || usize::from(x)
                .checked_add(printed_w)
                .is_none_or(|end| end > usize::from(u16::MAX))
            || occluded(occluders, x, y, printed_w as u16)
        {
            continue;
        }
        let linked = osc8(&span.url, &coloured_text);
        let _ = queue!(out, MoveTo(x, y), Print(&linked));
        wrote = true;
    }
    // Put the caret back where the footer asked for it. Only when we actually moved it.
    if wrote {
        if let Some((cx, cy)) = *caret {
            let _ = queue!(out, MoveTo(cx, cy));
        }
    }
    let _ = out.flush();
}

/// Slice only complete graphemes in `[col, col+width)`, replaying the style active at the first
/// glyph. The final reset prevents raw post-draw colour from leaking into the next terminal write.
fn extract_sgr_slice(sgr: &str, col: usize, width: usize) -> String {
    let Some(end) = col.checked_add(width) else {
        return String::new();
    };
    if width == 0 {
        return String::new();
    }
    let (glyphs, _) = sgr_glyphs(sgr);
    let mut out = String::new();
    let mut pos = 0usize;
    let mut started = false;
    for glyph in glyphs {
        if glyph.text == "\n" {
            break;
        }
        let next = pos.saturating_add(glyph.width);
        if pos < col {
            if next > col {
                // Starting halfway through a wide glyph would shift later glyphs to the wrong cell.
                return String::new();
            }
        } else if next > end {
            break;
        } else if glyph.width > 0 || started {
            if !started {
                if pos != col {
                    return String::new();
                }
                out.push_str(&glyph.prefix);
                started = true;
            }
            out.push_str(&glyph.raw);
        }
        pos = next;
    }
    if started {
        out.push_str("\x1b[0m");
    }
    out
}

/// Decode our captured post-draw stream for geometry assertions. OSC metadata has zero width;
/// tests must inspect the printable payload rather than mistaking a safe ratatui buffer for proof.
#[cfg(test)]
pub(crate) fn captured_links(bytes: &[u8]) -> Vec<(usize, usize, String, String)> {
    let raw = std::str::from_utf8(bytes).expect("UTF-8 terminal writes");
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("\x1b]8;;") {
        let before = &rest[..start];
        let cursor = before.rsplit("\x1b[").next().unwrap();
        let (y, x) = cursor.trim_end_matches('H').split_once(';').unwrap();
        let (y, x) = (
            y.parse::<usize>().unwrap() - 1,
            x.parse::<usize>().unwrap() - 1,
        );
        rest = &rest[start + 5..];
        let (uri, tail) = rest.split_once("\x1b\\").unwrap();
        let (payload, tail) = tail.split_once("\x1b]8;;\x1b\\").unwrap();
        out.push((x, y, uri.to_string(), payload.to_string()));
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolver that says every candidate exists, mapping it under a fixed absolute root. Lets the
    /// SHAPE rules be tested without depending on what happens to be on this machine's disk.
    fn always(cand: &str) -> Option<PathBuf> {
        let p = Path::new(cand);
        Some(if p.is_absolute() {
            p.to_path_buf()
        } else if cfg!(windows) {
            Path::new(r"C:\repo").join(p)
        } else {
            Path::new("/repo").join(p)
        })
    }

    /// Resolver that says nothing exists.
    fn never(_: &str) -> Option<PathBuf> {
        None
    }

    fn urls(row: &str, resolve: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<String> {
        scan_row_with(row, resolve)
            .into_iter()
            .map(|s| s.url)
            .collect()
    }

    #[test]
    fn detects_https_url() {
        let spans = scan_row_with("see https://example.com/foo for details", &never);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/foo");
        assert_eq!(spans[0].text, "https://example.com/foo");
    }

    #[test]
    fn trims_trailing_punctuation_from_url() {
        let spans = scan_row_with("visit https://example.com/foo. done", &never);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/foo");
    }

    #[test]
    fn detects_relative_file_path_with_known_ext() {
        let spans = scan_row_with("edited src/ui/tui/retained.rs line 42", &always);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].url.starts_with("file:///"));
        assert!(spans[0].text.contains("retained.rs"));
    }

    #[test]
    fn ignores_bare_word_without_slash() {
        // "retained.rs" alone (no slash) must NOT be linkified — too many false positives
        let spans = scan_row_with("see retained.rs for details", &always);
        assert!(
            spans.is_empty(),
            "bare filename without slash must not link: {spans:?}"
        );
    }

    #[test]
    fn osc8_format_correct() {
        let s = osc8("https://x.io", "click");
        assert!(s.starts_with("\x1b]8;;https://x.io\x1b\\"));
        assert!(s.contains("click"));
        assert!(s.ends_with("\x1b]8;;\x1b\\"));
    }

    // ── the reported bug: prose with slashes was linkified ──────────────────────────────────────

    #[test]
    fn slash_inside_a_word_is_never_a_path() {
        // Every one of these produced a bogus `file:///…` link before the word-boundary gate. The
        // resolver here says YES to everything, so only the shape rule can save these rows.
        for row in [
            "và/hoặc là cách nói thường gặp",
            "the ratio was 100% and/or more",
            "input/output đều ổn, n/a cho phần còn lại",
            "TODO(user): fix the parser/lexer split",
            "giá 15.000/kg tại chợ",
            "tỉ lệ 3/4 và 1/2/2026 trong báo cáo",
            "so sánh A/B testing",
            "chạy 24/7 không nghỉ",
        ] {
            let got = urls(row, &always);
            assert!(got.is_empty(), "prose must not linkify: {row:?} -> {got:?}");
        }
    }

    #[test]
    fn path_candidate_drops_trailing_punctuation() {
        // `/help...` is prose. The dots must not ride along into the candidate.
        let cands = scan_row_shapes("/help... abcd");
        assert_eq!(cands.len(), 1, "{cands:?}");
        let c = &cands[0];
        let text: String = "/help... abcd".chars().collect::<Vec<_>>()[c.start..c.end]
            .iter()
            .collect();
        assert_eq!(text, "/help");
    }

    // ── existence is what separates a path from prose ───────────────────────────────────────────

    #[test]
    fn nonexistent_path_is_not_linked() {
        let got = urls("edited src/nope/imaginary.rs today", &never);
        assert!(
            got.is_empty(),
            "must not link a file that isn't there: {got:?}"
        );
    }

    #[test]
    fn existing_path_is_linked() {
        let got = urls("edited src/ui/links.rs today", &always);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].starts_with("file:///"), "{got:?}");
    }

    #[test]
    fn unc_path_is_refused_without_touching_the_network() {
        // Probing a UNC path opens an SMB connection to a host named by model output. The refusal
        // must happen BEFORE the resolver runs, so this one panics if it is ever consulted.
        let tripwire =
            |_: &str| -> Option<PathBuf> { panic!("resolver must not be called for UNC") };
        assert!(resolve_path(r"\\evil.example.com\share\x.rs").is_none());
        assert!(resolve_path("//evil.example.com/share/x.rs").is_none());
        // And the same through the scan path, where a UNC row must simply produce nothing.
        let got = urls(r"see \\evil.example.com\share\x.rs now", &tripwire);
        assert!(got.is_empty(), "{got:?}");
    }

    // ── URL correctness ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn file_url_is_absolute_and_percent_encoded() {
        // `file://src/x.rs` is INVALID: RFC 8089 reads `src` as a hostname. Must be `file:///…`,
        // and spaces / non-ASCII must be percent-encoded.
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\my repo\tài liệu")
        } else {
            PathBuf::from("/my repo/tài liệu")
        };
        let u = file_url(&root.join("a.rs")).expect("file url");
        assert!(u.starts_with("file:///"), "needs three slashes: {u}");
        assert!(!u.contains(' '), "space must be encoded: {u}");
        assert!(u.contains("%20"), "space must be encoded: {u}");
        assert!(!u.contains("tài"), "non-ascii must be percent-encoded: {u}");
    }

    #[test]
    fn line_suffix_stays_in_text_but_leaves_the_url() {
        let spans = scan_row_with("edited src/foo.rs:42 ok", &always);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(spans[0].text, "src/foo.rs:42", "reader still sees the line");
        assert!(
            !spans[0].url.contains(":42"),
            "file:// has no line parameter: {}",
            spans[0].url
        );
        assert!(spans[0].url.ends_with("foo.rs"), "{}", spans[0].url);
    }

    #[test]
    fn strip_line_suffix_leaves_a_windows_drive_alone() {
        assert_eq!(strip_line_suffix(r"C:\a\b.rs"), r"C:\a\b.rs");
        assert_eq!(strip_line_suffix(r"C:\a\b.rs:12"), r"C:\a\b.rs");
        assert_eq!(strip_line_suffix("src/a.rs:12:5"), "src/a.rs");
    }

    // ── wrapped links ───────────────────────────────────────────────────────────────────────────

    /// Build rows the way `char_chunks` does: hard-cut at exactly `w` cells, no hyphen.
    fn hard_wrap(s: &str, w: usize) -> Vec<String> {
        s.chars()
            .collect::<Vec<_>>()
            .chunks(w)
            .map(|c| c.iter().collect())
            .collect()
    }

    #[test]
    fn wrapped_url_yields_one_url_for_both_halves() {
        let url = "https://example.com/a/very/long/path/that/keeps/going/final.html";
        let rows = hard_wrap(url, 30);
        assert!(rows.len() >= 2, "test needs a real wrap: {rows:?}");
        let links = scan_window_with(&rows, 0, rows.len(), 30, &never);
        assert_eq!(links.len(), rows.len(), "every piece links: {links:?}");
        for l in &links {
            assert_eq!(l.span.url, url, "each piece must point at the WHOLE url");
        }
    }

    #[test]
    fn two_full_rows_that_are_not_a_wrap_do_not_join() {
        // Row 0 ends with a URL but is nowhere near the column budget, so the renderer did NOT cut
        // it — row 1 is a new line of text. Joining them would fabricate a URL never on screen.
        let rows = vec![
            "see https://example.com/abcdefghij".to_string(),
            "morewords.html and so on".to_string(),
        ];
        let links = scan_window_with(&rows, 0, rows.len(), 60, &never);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(
            links[0].span.url, "https://example.com/abcdefghij",
            "a short row was not wrapped, so nothing may be appended"
        );
    }

    #[test]
    fn wrap_split_across_the_viewport_edge_still_resolves() {
        // The first half has scrolled off the top: the viewport starts at row 1. This must go
        // through `inject_hyperlinks`, because the look-behind window is what that function adds —
        // calling `scan_window_with` directly would silently pass with no window at all.
        use ratatui::layout::Rect;
        let url = "https://example.com/a/very/long/path/that/keeps/going/final.html";
        let rows = hard_wrap(url, 30);
        assert!(rows.len() >= 2, "test needs a real wrap: {rows:?}");
        let area = Rect::new(0, 0, 31, 8);
        let mut out: Vec<u8> = Vec::new();
        inject_hyperlinks(
            &mut out,
            &rows,
            &rows,
            &InjectCtx {
                start: 1,
                visible: 8,
                area,
                ..Default::default()
            },
        );
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains(url),
            "the off-screen head must still be joined in: {s:?}"
        );
    }

    #[test]
    fn gutter_prefix_is_stripped_when_joining() {
        // Assistant rows carry a `▌ ` gutter; a continuation must skip it before matching.
        let (skipped, rest) = strip_continuation_prefix("▌ g/path/x.html");
        assert!(skipped > 0);
        assert_eq!(rest, "g/path/x.html");
    }

    // ── occlusion + caret ───────────────────────────────────────────────────────────────────────

    #[test]
    fn occluded_span_emits_nothing() {
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 80, 10);
        let rows = vec!["see https://example.com/foo now".to_string()];
        let mut out: Vec<u8> = Vec::new();
        // Sanity: with no occluder it DOES write.
        inject_hyperlinks(
            &mut out,
            &rows,
            &rows,
            &InjectCtx {
                start: 0,
                visible: 10,
                area,
                ..Default::default()
            },
        );
        assert!(!out.is_empty(), "baseline must emit a link");

        // Now cover the whole transcript with an overlay.
        let mut covered: Vec<u8> = Vec::new();
        inject_hyperlinks(
            &mut covered,
            &rows,
            &rows,
            &InjectCtx {
                start: 0,
                visible: 10,
                area,
                occluders: vec![Rect::new(0, 0, 80, 10)],
                caret: None,
                rows_offset: 0,
            },
        );
        assert!(
            covered.is_empty(),
            "must not print over an overlay: {:?}",
            String::from_utf8_lossy(&covered)
        );
    }

    #[test]
    fn caret_is_restored_after_injecting() {
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 80, 10);
        let rows = vec!["see https://example.com/foo now".to_string()];
        let mut out: Vec<u8> = Vec::new();
        inject_hyperlinks(
            &mut out,
            &rows,
            &rows,
            &InjectCtx {
                start: 0,
                visible: 10,
                area,
                occluders: Vec::new(),
                caret: Some((7, 21)),
                rows_offset: 0,
            },
        );
        let s = String::from_utf8_lossy(&out);
        // crossterm MoveTo is 1-based and row-first: `ESC[{y+1};{x+1}H`
        assert!(
            s.ends_with("\x1b[22;8H"),
            "must end by parking the caret back: {s:?}"
        );
    }

    #[test]
    fn raw_injection_clips_before_scrollbar_and_keeps_full_uri_and_style() {
        let uri = format!("https://example.com/{}", "long/".repeat(35));
        let row = format!("界e\u{301} {uri}");
        let sgr = format!("\x1b[38;2;12;0;90m{row}\x1b[0m");
        let mut out = Vec::new();
        inject_hyperlinks(
            &mut out,
            &[sgr],
            &[row],
            &InjectCtx {
                area: ratatui::layout::Rect::new(7, 5, 42, 2),
                visible: usize::MAX,
                caret: Some((3, 20)),
                ..Default::default()
            },
        );
        let links = captured_links(&out);
        assert_eq!(links.len(), 1);
        let (x, y, url, text) = &links[0];
        assert_eq!((*x, *y), (11, 5), "combining mark is not an extra column");
        assert_eq!(url, &uri, "metadata must not be clipped");
        assert!(
            text.starts_with("\x1b[38;2;12;0;90m"),
            "active style replay: {text:?}"
        );
        assert!(text.ends_with("\x1b[0m"), "raw style must end here");
        assert!(*x + display_width(console::strip_ansi_codes(text).as_ref()) <= 7 + 41);
        assert!(std::str::from_utf8(&out).unwrap().ends_with("\x1b[21;4H"));
    }

    #[test]
    fn raw_slice_preserves_complete_graphemes_and_rejects_controls() {
        let text = "\x1b[31mae\x1b[1m\u{301}界👩‍💻Z\x1b[0m";
        let slice = extract_sgr_slice(text, 1, 1);
        assert_eq!(console::strip_ansi_codes(&slice), "e\u{301}");
        assert!(slice.starts_with("\x1b[31m"));
        assert!(slice.ends_with("\x1b[0m"));
        let glyphs = "e\u{301}界👩‍💻Z";
        for width in 0..=display_width(glyphs) {
            let slice = extract_sgr_slice(glyphs, 0, width);
            let plain = console::strip_ansi_codes(&slice);
            assert!(display_width(&plain) <= width, "{width}: {slice:?}");
            assert!(!plain.ends_with('\u{200d}'));
        }
        assert!(extract_sgr_slice("界x", 1, 2).is_empty());
        assert!(extract_sgr_slice("abc", usize::MAX, 1).is_empty());
        let dirty = "\x1b]8;;https://evil.invalid\x07ok\x1b]8;;\x1b\\\x1b[2J\x1bPsecret\x1b\\!";
        assert_eq!(
            console::strip_ansi_codes(&extract_sgr_slice(dirty, 0, 4)),
            "ok!"
        );
        assert_eq!(sanitize_sgr("ok\x1b]unfinished payload"), "ok");
    }

    #[test]
    fn raw_wide_glyph_never_crosses_the_clip_boundary() {
        let row = "https://example.com/界suffix".to_string();
        let prefix_w = display_width("https://example.com/");
        let mut out = Vec::new();
        inject_hyperlinks(
            &mut out,
            std::slice::from_ref(&row),
            std::slice::from_ref(&row),
            &InjectCtx {
                area: ratatui::layout::Rect::new(4, 2, (prefix_w + 2) as u16, 1),
                visible: 50,
                ..Default::default()
            },
        );
        let links = captured_links(&out);
        assert_eq!(links.len(), 1);
        assert_eq!(
            console::strip_ansi_codes(&links[0].3),
            "https://example.com/"
        );
        assert!(links[0].2.ends_with("界suffix"));
    }

    #[test]
    fn raw_injection_limits_tiny_height_origins_and_large_column_casts() {
        let rows = vec!["https://example.com/long/path".to_string(); 3];
        for width in 0..=3 {
            let mut out = Vec::new();
            inject_hyperlinks(
                &mut out,
                &rows,
                &rows,
                &InjectCtx {
                    area: ratatui::layout::Rect::new(9, 4, width, 1),
                    visible: usize::MAX,
                    ..Default::default()
                },
            );
            for (x, y, _, text) in captured_links(&out) {
                assert_eq!(y, 4);
                assert!(
                    x + display_width(console::strip_ansi_codes(&text).as_ref())
                        <= 9 + usize::from(width.saturating_sub(1))
                );
            }
            if width <= 1 {
                assert!(out.is_empty());
            }
        }
        let mut out = Vec::new();
        inject_hyperlinks(
            &mut out,
            &rows,
            &rows,
            &InjectCtx {
                area: ratatui::layout::Rect::new(2, 3, 80, 1),
                visible: usize::MAX,
                ..Default::default()
            },
        );
        assert!(captured_links(&out).iter().all(|(_, y, _, _)| *y == 3));
        // The link is beyond the physical pane; narrowing a usize to u16 must not bring it back.
        let row = format!("{}https://example.com/path", " ".repeat(65536));
        let mut out = Vec::new();
        inject_hyperlinks(
            &mut out,
            std::slice::from_ref(&row),
            std::slice::from_ref(&row),
            &InjectCtx {
                area: ratatui::layout::Rect::new(2, 3, 80, 1),
                visible: 1,
                ..Default::default()
            },
        );
        assert!(out.is_empty());
    }

    #[test]
    fn wrapped_paths_join_before_existence_and_extension_checks() {
        let source = format!("src/{}/source.rs", "directory/".repeat(12));
        let dest = format!("src/{}/destination.rs", "another/".repeat(14));
        let rows = hard_wrap(&format!("{source} → {dest}"), 21);
        let seen = std::cell::RefCell::new(Vec::new());
        let links = scan_window_with(&rows, 0, rows.len(), 21, &|path| {
            seen.borrow_mut().push(path.to_string());
            if path == dest {
                always(path)
            } else {
                None
            }
        });
        assert!(
            seen.borrow().contains(&source),
            "a moved-away source is still resolved as a FULL path"
        );
        assert!(seen.borrow().contains(&dest));
        assert!(!links.is_empty());
        assert!(links
            .iter()
            .all(|link| link.span.url.ends_with("destination.rs")));
        assert_eq!(
            links
                .iter()
                .map(|link| link.span.text.as_str())
                .collect::<String>(),
            dest
        );
    }

    #[test]
    fn wrapped_urls_join_unicode_short_edges_and_deep_viewport() {
        let viewport_row = REJOIN_WINDOW + 2;
        let part = "界e\u{301}👩‍💻/";
        let repeats = (19 * (viewport_row + 2)).div_ceil(display_width(part));
        let uri = format!("https://example.com/{}", part.repeat(repeats));
        // Use the same grapheme wrapper as production without importing the private TUI module.
        let mut rows = Vec::new();
        let mut row = String::new();
        for glyph in uri.graphemes(true) {
            if display_width(&row) + display_width(glyph) > 19 {
                rows.push(std::mem::take(&mut row));
            }
            row.push_str(glyph);
        }
        rows.push(row);
        assert!(
            rows.len() > viewport_row,
            "fixture needs a visible row beyond the lookbehind: {rows:?}"
        );
        let links = scan_window_with(&rows, viewport_row, viewport_row + 1, 19, &never);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].span.url, uri);
        let mut out = Vec::new();
        inject_hyperlinks(
            &mut out,
            &rows,
            &rows,
            &InjectCtx {
                start: viewport_row,
                visible: 1,
                area: ratatui::layout::Rect::new(3, 2, 20, 1),
                ..Default::default()
            },
        );
        assert!(captured_links(&out)
            .iter()
            .all(|(_, _, full, _)| full == &uri));
        assert!(!out.is_empty());
    }

    #[test]
    fn full_urls_do_not_absorb_indented_prose_or_another_scheme() {
        let url = "https://example.com/abc";
        for next in [
            " and then continue",
            "https://other.example/new",
            "└ result",
            "C:/new/file.rs",
        ] {
            let rows = vec![url.to_string(), next.to_string()];
            let links = scan_window_with(&rows, 0, 2, display_width(url), &never);
            assert_eq!(links[0].span.url, url, "{next:?}: {links:?}");
        }
    }

    #[test]
    fn no_links_means_the_caret_is_left_alone() {
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 80, 10);
        let rows = vec!["nothing to see here".to_string()];
        let mut out: Vec<u8> = Vec::new();
        inject_hyperlinks(
            &mut out,
            &rows,
            &rows,
            &InjectCtx {
                start: 0,
                visible: 10,
                area,
                occluders: Vec::new(),
                caret: Some((7, 21)),
                rows_offset: 0,
            },
        );
        assert!(out.is_empty(), "must not touch the cursor for nothing");
    }
}
