//! Pure, bounded textual merging of changes made against the exact same base.
//!
//! No file access, parser dependency, normalization, or semantic-safety claim is made here.
//! A Rust merge is deliberately much narrower than a general three-way merge: only
//! distinct, confidently scanned function bodies can be combined. Callers must still
//! verify the candidate before publishing it.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    Rust,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Localized,
    Rewrite,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthoredChange<'a> {
    pub author: &'a str,
    /// `None` is an absent file, not a present, empty file.
    pub base: Option<&'a [u8]>,
    pub result: &'a [u8],
    pub operation: Operation,
}

/// Limits can lower, but never raise, the engine's hard ceilings. Diff cells, work,
/// hunks, and scanned tokens are charged across the entire merge, not per author.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_changes: usize,
    pub max_bytes: usize,
    pub max_total_bytes: usize,
    pub max_lines: usize,
    pub max_diff_cells: usize,
    pub max_work: usize,
    pub max_hunks: usize,
    pub max_tokens: usize,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_changes: 32,
            max_bytes: 256 * 1024,
            max_total_bytes: 2 * 1024 * 1024,
            max_lines: 8192,
            max_diff_cells: 1_000_000,
            max_work: 16_000_000,
            max_hunks: 128,
            max_tokens: 32_768,
            max_depth: 64,
        }
    }
}

impl Limits {
    fn bounded(self) -> Self {
        Self {
            max_changes: self.max_changes.min(64),
            max_bytes: self.max_bytes.min(1024 * 1024),
            max_total_bytes: self.max_total_bytes.min(16 * 1024 * 1024),
            max_lines: self.max_lines.min(16_384),
            max_diff_cells: self.max_diff_cells.min(4_000_000),
            max_work: self.max_work.min(64_000_000),
            max_hunks: self.max_hunks.min(512),
            max_tokens: self.max_tokens.min(65_536),
            max_depth: self.max_depth.min(128),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub bytes: Vec<u8>,
    /// Sorted, unique IDs of effective contributors. No-op changes are excluded.
    pub contributors: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LimitKind {
    Changes,
    SourceBytes,
    TotalBytes,
    Lines,
    DiffCells,
    Work,
    Hunks,
    Tokens,
    Nesting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictKind {
    EmptyInput,
    InvalidAuthor,
    InvalidUtf8,
    BaseMismatch,
    AuthorCollision,
    Creation,
    WholeRewrite,
    UnsupportedLanguage,
    AmbiguousDiff,
    Overlap,
    CompetingInsertion,
    SameFunction,
    OutsideFunction,
    MacroOrAttribute,
    AmbiguousRust,
    LimitExceeded(LimitKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Conflict {
    pub kind: ConflictKind,
    /// Bounded to 64 IDs of at most 128 bytes each; empty for invalid input IDs/counts.
    pub authors: Vec<String>,
    /// Half-open original-base byte range, when the conflict has a useful location.
    pub base_range: Option<Range<usize>>,
}

type Checked<T> = Result<T, ConflictKind>;

struct Budget {
    limits: Limits,
    work: usize,
    cells: usize,
    hunks: usize,
    tokens: usize,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self { limits, work: 0, cells: 0, hunks: 0, tokens: 0 }
    }

    fn work(&mut self, amount: usize) -> Checked<()> {
        self.work = self.work.checked_add(amount).ok_or(
            ConflictKind::LimitExceeded(LimitKind::Work),
        )?;
        if self.work > self.limits.max_work {
            return Err(ConflictKind::LimitExceeded(LimitKind::Work));
        }
        Ok(())
    }

    fn cells(&mut self, amount: usize) -> Checked<()> {
        self.cells = self.cells.checked_add(amount).ok_or(
            ConflictKind::LimitExceeded(LimitKind::DiffCells),
        )?;
        if self.cells > self.limits.max_diff_cells {
            return Err(ConflictKind::LimitExceeded(LimitKind::DiffCells));
        }
        Ok(())
    }

    fn hunk(&mut self) -> Checked<()> {
        self.hunks += 1;
        if self.hunks > self.limits.max_hunks {
            return Err(ConflictKind::LimitExceeded(LimitKind::Hunks));
        }
        Ok(())
    }

    fn token(&mut self) -> Checked<()> {
        self.tokens += 1;
        if self.tokens > self.limits.max_tokens {
            return Err(ConflictKind::LimitExceeded(LimitKind::Tokens));
        }
        Ok(())
    }
}

struct Variant<'a> {
    result: &'a [u8],
    rewrite: bool,
}

#[derive(Clone, Debug)]
struct Hunk {
    base: Range<usize>,
    result: Range<usize>,
}

struct Edit<'a> {
    base: Range<usize>,
    replacement: &'a [u8],
    owners: u64,
    function: usize,
}

fn conflict(kind: ConflictKind, authors: &[&str], base_range: Option<Range<usize>>) -> Conflict {
    Conflict {
        kind,
        authors: authors.iter().map(|author| (*author).to_owned()).collect(),
        base_range,
    }
}

/// Merge all inputs at once; input permutation never selects a winner. Identical
/// results/edits coalesce, but competing changes fail closed. Even a single
/// localized change is diffed (and subject to ambiguity/work/hunk limits); a lone
/// explicit rewrite needs no diff. Rust scanning is needed only when combining
/// nonidentical results. This is a textual candidate, never proof of semantics.
pub(crate) fn merge(
    language: Language,
    changes: &[AuthoredChange<'_>],
    limits: &Limits,
) -> Result<Candidate, Conflict> {
    let limits = limits.bounded();
    if changes.is_empty() {
        return Err(conflict(ConflictKind::EmptyInput, &[], None));
    }
    if changes.len() > limits.max_changes {
        return Err(conflict(ConflictKind::LimitExceeded(LimitKind::Changes), &[], None));
    }
    if changes.iter().any(|c| {
        c.author.is_empty() || c.author.len() > 128 || c.author.chars().any(char::is_control)
    }) {
        return Err(conflict(ConflictKind::InvalidAuthor, &[], None));
    }
    let mut ordered: Vec<_> = changes.iter().collect();
    ordered.sort_by(|a, b| a.author.cmp(b.author));
    let mut authors: Vec<_> = ordered.iter().map(|c| c.author).collect();
    authors.dedup();
    let fail = |kind| conflict(kind, &authors, None);
    let mut total = 0usize;
    for change in &ordered {
        if change.result.len() > limits.max_bytes
            || change.base.is_some_and(|base| base.len() > limits.max_bytes)
        {
            return Err(fail(ConflictKind::LimitExceeded(LimitKind::SourceBytes)));
        }
        total = total.saturating_add(change.result.len());
        total = total.saturating_add(change.base.map_or(0, <[u8]>::len));
    }
    if total > limits.max_total_bytes {
        return Err(fail(ConflictKind::LimitExceeded(LimitKind::TotalBytes)));
    }
    if ordered.iter().any(|change| {
        std::str::from_utf8(change.result).is_err()
            || change.base.is_some_and(|base| std::str::from_utf8(base).is_err())
    }) {
        return Err(fail(ConflictKind::InvalidUtf8));
    }
    let base = ordered[0].base;
    if ordered.iter().any(|change| change.base != base) {
        return Err(fail(ConflictKind::BaseMismatch));
    }
    for pair in ordered.windows(2) {
        if pair[0].author == pair[1].author && pair[0].result != pair[1].result {
            return Err(fail(ConflictKind::AuthorCollision));
        }
    }
    let mut budget = Budget::new(limits);
    let mut variants: Vec<Variant<'_>> = Vec::new();
    let mut contributors: Vec<&str> = Vec::new();
    for change in ordered {
        if base == Some(change.result) {
            continue;
        }
        contributors.push(change.author);
        let mut found = false;
        for variant in &mut variants {
            budget.work(change.result.len().saturating_add(1)).map_err(&fail)?;
            if variant.result == change.result {
                variant.rewrite |= change.operation == Operation::Rewrite;
                found = true;
                break;
            }
        }
        if !found {
            variants.push(Variant {
                result: change.result,
                rewrite: change.operation == Operation::Rewrite,
            });
        }
    }
    contributors.dedup();
    let candidate = |bytes: Vec<u8>| Candidate {
        bytes,
        contributors: contributors.iter().map(|id| (*id).to_owned()).collect(),
    };
    let Some(base) = base else {
        return if variants.len() == 1 {
            Ok(candidate(variants[0].result.to_vec()))
        } else {
            Err(fail(ConflictKind::Creation))
        };
    };
    if variants.is_empty() {
        return Ok(candidate(base.to_vec()));
    }
    if variants.iter().any(|variant| variant.rewrite) {
        return if variants.len() == 1 {
            Ok(candidate(variants[0].result.to_vec()))
        } else {
            Err(fail(ConflictKind::WholeRewrite))
        };
    }
    let mut diffs = Vec::new();
    for variant in &variants {
        let hunks = match diff(base, variant.result, &mut budget) {
            // Repeated characters can make a minimal textual alignment ambiguous even when
            // unchanged Rust item shells prove the exact body boundaries. In that case use
            // whole bodies as atomic hunks, never choose one of the ambiguous alignments.
            Err(ConflictKind::AmbiguousDiff) if language == Language::Rust => {
                match structural_diff(base, variant.result, &mut budget) {
                    // Without a confidently scanned item shell, preserve the original refusal.
                    Err(ConflictKind::AmbiguousRust) => Err(ConflictKind::AmbiguousDiff),
                    result => result,
                }
            }
            result => result,
        };
        diffs.push(hunks.map_err(&fail)?);
    }
    if variants.len() == 1 {
        return Ok(candidate(variants[0].result.to_vec()));
    }
    if language != Language::Rust {
        return Err(fail(ConflictKind::UnsupportedLanguage));
    }
    if diffs.iter().flatten().any(|hunk| !base.is_empty() && hunk.base == (0..base.len())) {
        return Err(fail(ConflictKind::WholeRewrite));
    }
    let original = scan_rust(base, &mut budget).map_err(&fail)?;
    let mut edits: Vec<Edit<'_>> = Vec::new();
    for (owner, (variant, hunks)) in variants.iter().zip(&diffs).enumerate() {
        let parsed = scan_rust(variant.result, &mut budget).map_err(&fail)?;
        if !same_shell(base, &original, variant.result, &parsed, &mut budget).map_err(&fail)? {
            return Err(fail(ConflictKind::OutsideFunction));
        }
        for hunk in hunks {
            let function = locate(&original, &hunk.base).map_err(|kind| {
                conflict(kind, &authors, Some(hunk.base.clone()))
            })?;
            let target = locate(&parsed, &hunk.result).map_err(|kind| {
                conflict(kind, &authors, Some(hunk.base.clone()))
            })?;
            if function != target {
                return Err(conflict(ConflictKind::OutsideFunction, &authors, Some(hunk.base.clone())));
            }
            edits.push(Edit {
                base: hunk.base.clone(),
                replacement: &variant.result[hunk.result.clone()],
                owners: 1u64 << owner,
                function,
            });
        }
    }
    // Stable, explicit byte ordering; author-sorted variant IDs are only owner bits.
    edits.sort_by(|a, b| {
        a.base.start.cmp(&b.base.start)
            .then(a.base.end.cmp(&b.base.end))
            .then(a.replacement.cmp(b.replacement))
    });
    let mut unique: Vec<Edit<'_>> = Vec::new();
    for edit in edits {
        if let Some(last) = unique.last_mut() {
            if last.base == edit.base && last.replacement == edit.replacement {
                last.owners |= edit.owners;
                continue;
            }
            if touches(&last.base, &edit.base) {
                let kind = if last.base.is_empty() && edit.base.is_empty() {
                    ConflictKind::CompetingInsertion
                } else {
                    ConflictKind::Overlap
                };
                return Err(conflict(kind, &authors, Some(edit.base.clone())));
            }
        }
        unique.push(edit);
    }
    let mut function_owners = vec![0u64; original.functions.len()];
    for edit in &unique {
        function_owners[edit.function] |= edit.owners;
    }
    for edit in &unique {
        // Multiple authors may share an identical complete edit set in this item,
        // but cannot each make a different edit anywhere in the same function.
        if edit.owners != function_owners[edit.function] {
            return Err(conflict(ConflictKind::SameFunction, &authors, Some(edit.base.clone())));
        }
    }
    let removed: usize = unique.iter().map(|edit| edit.base.len()).sum();
    let added: usize = unique.iter().map(|edit| edit.replacement.len()).sum();
    let size = base.len().saturating_sub(removed).saturating_add(added);
    if size > limits.max_bytes {
        return Err(fail(ConflictKind::LimitExceeded(LimitKind::SourceBytes)));
    }
    let mut bytes = Vec::with_capacity(size);
    let mut cursor = 0;
    for edit in unique {
        bytes.extend_from_slice(&base[cursor..edit.base.start]);
        bytes.extend_from_slice(edit.replacement);
        cursor = edit.base.end;
    }
    bytes.extend_from_slice(&base[cursor..]);
    // Independently rescan the composition: separate valid edits can change lexing
    // across a boundary. Its immutable shell must still be exactly the same.
    let parsed = scan_rust(&bytes, &mut budget).map_err(&fail)?;
    if !same_shell(base, &original, &bytes, &parsed, &mut budget).map_err(&fail)? {
        return Err(fail(ConflictKind::AmbiguousRust));
    }
    Ok(candidate(bytes))
}

/// Inserts on replacement boundaries are deliberately treated as touching.
fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() {
        b.start <= a.start && a.start <= b.end
    } else if b.is_empty() {
        a.start <= b.start && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

fn lines(bytes: &[u8], budget: &mut Budget) -> Checked<Vec<Range<usize>>> {
    budget.work(bytes.len())?;
    let mut result = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if result.len() >= budget.limits.max_lines {
                return Err(ConflictKind::LimitExceeded(LimitKind::Lines));
            }
            result.push(start..index + 1);
            start = index + 1;
        }
    }
    if start < bytes.len() {
        if result.len() >= budget.limits.max_lines {
            return Err(ConflictKind::LimitExceeded(LimitKind::Lines));
        }
        result.push(start..bytes.len());
    }
    Ok(result)
}

fn units(bytes: &[u8], range: Range<usize>, budget: &mut Budget) -> Checked<Vec<Range<usize>>> {
    budget.work(range.len())?;
    // The public entry point checked UTF-8; line boundaries preserve it.
    let text = std::str::from_utf8(&bytes[range.clone()]).map_err(|_| ConflictKind::InvalidUtf8)?;
    Ok(text.char_indices().map(|(i, c)| {
        range.start + i..range.start + i + c.len_utf8()
    }).collect())
}

fn equal(
    left: &[u8], a: &Range<usize>, right: &[u8], b: &Range<usize>, budget: &mut Budget,
) -> Checked<bool> {
    budget.work(1 + if a.len() == b.len() { a.len() } else { 0 })?;
    Ok(left[a.clone()] == right[b.clone()])
}

/// A bounded LCS, with ambiguity measured in matched *positions*, not the
/// arbitrary order of deleting/inserting an unmatched gap. Every pair that can
/// occur in any optimal alignment is counted; more than LCS-length pairs means
/// different equally good anchors exist, so no alignment is silently chosen.
fn anchors(
    left: &[u8], a: &[Range<usize>], right: &[u8], b: &[Range<usize>], budget: &mut Budget,
) -> Checked<Vec<(usize, usize)>> {
    let width = b.len() + 1;
    let count = (a.len() + 1).checked_mul(width)
        .ok_or(ConflictKind::LimitExceeded(LimitKind::DiffCells))?;
    budget.cells(count)?;
    let mut suffix = vec![0u32; count];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let same = equal(left, &a[i], right, &b[j], budget)?;
            suffix[i * width + j] = if same {
                1 + suffix[(i + 1) * width + j + 1]
            } else {
                suffix[(i + 1) * width + j].max(suffix[i * width + j + 1])
            };
        }
    }
    let length = suffix[0] as usize;
    let mut result = Vec::with_capacity(length);
    let mut previous = vec![0u32; width];
    let mut row = vec![0u32; width];
    for i in 0..a.len() {
        row[0] = 0;
        for j in 0..b.len() {
            let same = equal(left, &a[i], right, &b[j], budget)?;
            if same && previous[j] + 1 + suffix[(i + 1) * width + j + 1] == length as u32 {
                if result.len() == length {
                    return Err(ConflictKind::AmbiguousDiff);
                }
                result.push((i, j));
            }
            row[j + 1] = if same { previous[j] + 1 } else { previous[j + 1].max(row[j]) };
        }
        std::mem::swap(&mut previous, &mut row);
    }
    Ok(result)
}

fn diff(base: &[u8], result: &[u8], budget: &mut Budget) -> Checked<Vec<Hunk>> {
    let old_lines = lines(base, budget)?;
    let new_lines = lines(result, budget)?;
    let matches = anchors(base, &old_lines, result, &new_lines, budget)?;
    let mut hunks = Vec::new();
    let (mut old, mut new) = (0, 0);
    for (i, j) in matches {
        refine(base, old..old_lines[i].start, result, new..new_lines[j].start, &mut hunks, budget)?;
        old = old_lines[i].end;
        new = new_lines[j].end;
    }
    refine(base, old..base.len(), result, new..result.len(), &mut hunks, budget)?;
    Ok(hunks)
}

/// An exact unchanged item shell can disambiguate function boundaries, not edits inside a
/// function. Treat each changed body as one indivisible replacement so same-item changes still
/// conflict. Unsupported/opaque structure and all existing global work limits remain fail-closed.
fn structural_diff(base: &[u8], result: &[u8], budget: &mut Budget) -> Checked<Vec<Hunk>> {
    let original = scan_rust(base, budget)?;
    let parsed = scan_rust(result, budget)?;
    if !same_shell(base, &original, result, &parsed, budget)? {
        return Err(ConflictKind::OutsideFunction);
    }
    let mut hunks = Vec::new();
    for (old, new) in original.functions.iter().zip(&parsed.functions) {
        if !equal(base, &old.body, result, &new.body, budget)? {
            if old.opaque || new.opaque {
                return Err(ConflictKind::MacroOrAttribute);
            }
            budget.hunk()?;
            hunks.push(Hunk { base: old.body.clone(), result: new.body.clone() });
        }
    }
    Ok(hunks)
}

fn refine(
    base: &[u8], old: Range<usize>, result: &[u8], new: Range<usize>,
    hunks: &mut Vec<Hunk>, budget: &mut Budget,
) -> Checked<()> {
    if old.is_empty() && new.is_empty() {
        return Ok(());
    }
    let a = units(base, old.clone(), budget)?;
    let b = units(result, new.clone(), budget)?;
    let matches = anchors(base, &a, result, &b, budget)?;
    let (mut i, mut j) = (old.start, new.start);
    for (x, y) in matches {
        if i != a[x].start || j != b[y].start {
            budget.hunk()?;
            hunks.push(Hunk { base: i..a[x].start, result: j..b[y].start });
        }
        i = a[x].end;
        j = b[y].end;
    }
    if i != old.end || j != new.end {
        budget.hunk()?;
        hunks.push(Hunk { base: i..old.end, result: j..new.end });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ident,
    Literal,
    Lifetime,
    Punct(u8),
}

#[derive(Clone, Debug)]
struct Token {
    kind: Kind,
    span: Range<usize>,
    mate: usize,
}

fn ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn ident_continue(byte: u8) -> bool {
    ident_start(byte) || byte.is_ascii_digit()
}

fn punct(token: &Token, byte: u8) -> bool {
    token.kind == Kind::Punct(byte)
}

fn word(bytes: &[u8], token: &Token, expected: &[u8]) -> bool {
    token.kind == Kind::Ident && &bytes[token.span.clone()] == expected
}

fn escape(bytes: &[u8], i: &mut usize, byte_literal: bool, allow_continuation: bool) -> Checked<()> {
    *i += 1;
    let next = *bytes.get(*i).ok_or(ConflictKind::AmbiguousRust)?;
    *i += 1;
    match next {
        b'n' | b'r' | b't' | b'0' | b'\\' | b'\'' | b'"' => Ok(()),
        b'x' => {
            let digits = bytes.get(*i..i.saturating_add(2)).ok_or(ConflictKind::AmbiguousRust)?;
            if !digits.iter().all(u8::is_ascii_hexdigit)
                || (!byte_literal && digits[0] > b'7')
            {
                return Err(ConflictKind::AmbiguousRust);
            }
            *i += 2;
            Ok(())
        }
        b'u' if !byte_literal => {
            if bytes.get(*i) != Some(&b'{') {
                return Err(ConflictKind::AmbiguousRust);
            }
            *i += 1;
            if !bytes.get(*i).is_some_and(u8::is_ascii_hexdigit) {
                return Err(ConflictKind::AmbiguousRust);
            }
            let (mut value, mut digits) = (0u32, 0usize);
            while let Some(&byte) = bytes.get(*i) {
                if byte == b'}' {
                    break;
                }
                if byte != b'_' {
                    let digit = (byte as char).to_digit(16).ok_or(ConflictKind::AmbiguousRust)?;
                    digits += 1;
                    if digits > 6 {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    value = value * 16 + digit;
                }
                *i += 1;
            }
            if digits == 0 || bytes.get(*i) != Some(&b'}') || char::from_u32(value).is_none() {
                return Err(ConflictKind::AmbiguousRust);
            }
            *i += 1;
            Ok(())
        }
        b'\n' | b'\r' if allow_continuation => {
            if next == b'\r' {
                if bytes.get(*i) != Some(&b'\n') {
                    return Err(ConflictKind::AmbiguousRust);
                }
                *i += 1;
            }
            while bytes.get(*i).is_some_and(u8::is_ascii_whitespace) {
                *i += 1;
            }
            Ok(())
        }
        _ => Err(ConflictKind::AmbiguousRust),
    }
}

fn cooked(bytes: &[u8], quote: usize, byte_literal: bool) -> Checked<usize> {
    let mut i = quote + 1;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'"' => return Ok(i + 1),
            b'\\' => escape(bytes, &mut i, byte_literal, true)?,
            b'\r' if bytes.get(i + 1) != Some(&b'\n') => return Err(ConflictKind::AmbiguousRust),
            byte if byte_literal && !byte.is_ascii() => return Err(ConflictKind::AmbiguousRust),
            _ => i += 1,
        }
    }
    Err(ConflictKind::AmbiguousRust)
}

/// Returns `None` for a raw identifier such as `r#name`.
fn raw(bytes: &[u8], r: usize, byte_literal: bool, budget: &mut Budget) -> Checked<Option<usize>> {
    let mut quote = r + 1;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
        if quote - r - 1 > 255 {
            return Err(ConflictKind::AmbiguousRust);
        }
    }
    if bytes.get(quote) != Some(&b'"') {
        return Ok(None);
    }
    let hashes = quote - r - 1;
    let mut i = quote + 1;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'"' {
            budget.work(hashes + 1)?;
            if bytes.get(i + 1..i + 1 + hashes)
                .is_some_and(|tail| tail.iter().all(|b| *b == b'#'))
            {
                return Ok(Some(i + 1 + hashes));
            }
        }
        if (byte_literal && !byte.is_ascii())
            || (byte == b'\r' && bytes.get(i + 1) != Some(&b'\n'))
        {
            return Err(ConflictKind::AmbiguousRust);
        }
        i += 1;
    }
    Err(ConflictKind::AmbiguousRust)
}

fn character(bytes: &[u8], quote: usize, byte_literal: bool) -> Checked<Option<usize>> {
    let mut i = quote + 1;
    let byte = *bytes.get(i).ok_or(ConflictKind::AmbiguousRust)?;
    if byte == b'\\' {
        escape(bytes, &mut i, byte_literal, false)?;
    } else {
        if matches!(byte, b'\'' | b'\n' | b'\r' | b'\t') || (byte_literal && !byte.is_ascii()) {
            return Err(ConflictKind::AmbiguousRust);
        }
        // Decode only this scalar, not the entire remaining file (which would
        // make a file containing many character literals quadratic to scan).
        let width = if byte < 0x80 { 1 } else if byte < 0xe0 { 2 } else if byte < 0xf0 { 3 } else { 4 };
        let scalar = bytes.get(i..i + width).ok_or(ConflictKind::AmbiguousRust)?;
        let text = std::str::from_utf8(scalar).map_err(|_| ConflictKind::AmbiguousRust)?;
        i += text.chars().next().ok_or(ConflictKind::AmbiguousRust)?.len_utf8();
    }
    if bytes.get(i) == Some(&b'\'') {
        Ok(Some(i + 1))
    } else if !byte_literal && ident_start(byte) {
        Ok(None) // A lifetime/label is resolved conservatively by the lexer.
    } else {
        Err(ConflictKind::AmbiguousRust)
    }
}

fn lex(bytes: &[u8], budget: &mut Budget) -> Checked<Vec<Token>> {
    budget.work(bytes.len())?;
    let mut tokens: Vec<Token> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes.get(i..i + 2) == Some(b"//") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes.get(i..i + 2) == Some(b"/*") {
            i += 2;
            let mut depth = 1;
            if depth > budget.limits.max_depth {
                return Err(ConflictKind::LimitExceeded(LimitKind::Nesting));
            }
            while i < bytes.len() && depth > 0 {
                if bytes.get(i..i + 2) == Some(b"/*") {
                    depth += 1;
                    if depth > budget.limits.max_depth {
                        return Err(ConflictKind::LimitExceeded(LimitKind::Nesting));
                    }
                    i += 2;
                } else if bytes.get(i..i + 2) == Some(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth != 0 {
                return Err(ConflictKind::AmbiguousRust);
            }
            continue;
        }
        let start = i;
        let mut kind = Kind::Literal;
        let mut literal_end = None;
        if byte == b'r' {
            literal_end = raw(bytes, i, false, budget)?;
        } else if matches!(byte, b'b' | b'c') && bytes.get(i + 1) == Some(&b'r') {
            literal_end = raw(bytes, i + 1, byte == b'b', budget)?;
        }
        if literal_end.is_none() {
            if byte == b'"' {
                literal_end = Some(cooked(bytes, i, false)?);
            } else if matches!(byte, b'b' | b'c') && bytes.get(i + 1) == Some(&b'"') {
                literal_end = Some(cooked(bytes, i + 1, byte == b'b')?);
            } else if byte == b'b' && bytes.get(i + 1) == Some(&b'\'') {
                literal_end = character(bytes, i + 1, true)?;
            } else if byte == b'\'' {
                literal_end = character(bytes, i, false)?;
                if literal_end.is_none() {
                    i += 1;
                    while bytes.get(i).is_some_and(|byte| ident_continue(*byte)) {
                        i += 1;
                    }
                    if bytes.get(i) == Some(&b'\'') {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    kind = Kind::Lifetime;
                }
            }
        }
        if let Some(end) = literal_end {
            i = end;
        } else if i != start {
            // Lifetime consumed above.
        } else if ident_start(byte) {
            kind = Kind::Ident;
            i += 1;
            if byte == b'r' && bytes.get(i) == Some(&b'#') {
                i += 1;
                if !bytes.get(i).is_some_and(|byte| ident_start(*byte)) {
                    return Err(ConflictKind::AmbiguousRust);
                }
            }
            while bytes.get(i).is_some_and(|byte| ident_continue(*byte)) {
                i += 1;
            }
        } else if byte.is_ascii_digit() {
            i += 1;
            while bytes.get(i).is_some_and(|byte| ident_continue(*byte) || *byte == b'.') {
                i += 1;
            }
        } else if b"{}()[];:,.<>+-=*/%!&|^?@#$".contains(&byte) {
            kind = Kind::Punct(byte);
            i += 1;
        } else {
            // Non-ASCII identifiers and unsupported token forms are not guessed.
            return Err(ConflictKind::AmbiguousRust);
        }
        budget.token()?;
        let index = tokens.len();
        let mut token = Token { kind, span: start..i, mate: usize::MAX };
        if let Kind::Punct(byte) = kind {
            if b"{([".contains(&byte) {
                if stack.len() >= budget.limits.max_depth {
                    return Err(ConflictKind::LimitExceeded(LimitKind::Nesting));
                }
                stack.push(index);
            } else if b"})]".contains(&byte) {
                let open = stack.pop().ok_or(ConflictKind::AmbiguousRust)?;
                let expected = match byte { b'}' => b'{', b')' => b'(', _ => b'[' };
                if !punct(&tokens[open], expected) {
                    return Err(ConflictKind::AmbiguousRust);
                }
                tokens[open].mate = index;
                token.mate = open;
            }
        }
        tokens.push(token);
    }
    if !stack.is_empty() {
        return Err(ConflictKind::AmbiguousRust);
    }
    Ok(tokens)
}

struct Function {
    body: Range<usize>,
    /// Attributes and macro invocations make the entire function opaque: macro
    /// paths or neighboring token changes must not evade a span-only check.
    opaque: bool,
}

struct Parsed {
    functions: Vec<Function>,
}

struct Scanner<'a, 'b> {
    bytes: &'a [u8],
    tokens: Vec<Token>,
    functions: Vec<Function>,
    budget: &'b mut Budget,
}

impl Scanner<'_, '_> {
    fn is_word(&self, index: usize, expected: &[u8]) -> bool {
        self.tokens.get(index).is_some_and(|token| word(self.bytes, token, expected))
    }

    fn is_punct(&self, index: usize, byte: u8) -> bool {
        self.tokens.get(index).is_some_and(|token| punct(token, byte))
    }

    fn group_end(&self, index: usize, end: usize) -> Checked<usize> {
        let close = self.tokens[index].mate;
        if close == usize::MAX || close >= end {
            return Err(ConflictKind::AmbiguousRust);
        }
        Ok(close)
    }

    fn angle(&mut self, i: &mut usize, depth: &mut usize) -> Checked<()> {
        if self.is_punct(*i, b'<') {
            *depth += 1;
            if *depth > self.budget.limits.max_depth {
                return Err(ConflictKind::LimitExceeded(LimitKind::Nesting));
            }
        } else if self.is_punct(*i, b'>') && !self.is_punct(i.saturating_sub(1), b'-') {
            if *depth == 0 {
                return Err(ConflictKind::AmbiguousRust);
            }
            *depth -= 1;
        }
        Ok(())
    }

    fn body_or_semicolon(&mut self, mut i: usize, end: usize) -> Checked<usize> {
        let mut angles = 0;
        while i < end {
            self.budget.work(1)?;
            if self.is_punct(i, b'!') {
                return Err(ConflictKind::AmbiguousRust);
            }
            if self.is_punct(i, b'{') || self.is_punct(i, b';') {
                if angles != 0 {
                    return Err(ConflictKind::AmbiguousRust);
                }
                return Ok(i);
            }
            if self.is_punct(i, b'(') || self.is_punct(i, b'[') {
                i = self.group_end(i, end)? + 1;
            } else {
                self.angle(&mut i, &mut angles)?;
                i += 1;
            }
        }
        Err(ConflictKind::AmbiguousRust)
    }

    fn terminated(&mut self, mut i: usize, end: usize) -> Checked<usize> {
        while i < end {
            self.budget.work(1)?;
            if self.is_punct(i, b';') {
                return Ok(i + 1);
            }
            if matches!(self.tokens[i].kind, Kind::Punct(b'{' | b'(' | b'[')) {
                i = self.group_end(i, end)? + 1;
            } else {
                i += 1;
            }
        }
        Err(ConflictKind::AmbiguousRust)
    }

    fn function(&mut self, mut i: usize, end: usize, attributed: bool) -> Checked<usize> {
        // i points just after `fn`. Only ordinary named functions are recognized.
        if self.tokens.get(i).is_none_or(|token| token.kind != Kind::Ident) {
            return Err(ConflictKind::AmbiguousRust);
        }
        i += 1;
        if self.is_punct(i, b'<') {
            let mut depth = 0;
            loop {
                if i >= end || self.is_punct(i, b'{') || self.is_punct(i, b'!') {
                    return Err(ConflictKind::AmbiguousRust);
                }
                self.budget.work(1)?;
                if self.is_punct(i, b'(') || self.is_punct(i, b'[') {
                    i = self.group_end(i, end)? + 1;
                } else {
                    self.angle(&mut i, &mut depth)?;
                    i += 1;
                }
                if depth == 0 {
                    break;
                }
            }
        }
        if !self.is_punct(i, b'(') {
            return Err(ConflictKind::AmbiguousRust);
        }
        let args_end = self.group_end(i, end)?;
        if self.tokens[i..=args_end].iter().any(|token| punct(token, b'!') || punct(token, b'{')) {
            return Err(ConflictKind::AmbiguousRust);
        }
        let tail = args_end + 1;
        // After the argument group a function can only end, introduce a return
        // type, or introduce a where-clause. Unknown trailing tokens are not a
        // license to scan ahead until some unrelated opening brace.
        if !(self.is_punct(tail, b'{') || self.is_punct(tail, b';')
            || self.is_word(tail, b"where")
            || self.is_punct(tail, b'-') && self.is_punct(tail + 1, b'>'))
        {
            return Err(ConflictKind::AmbiguousRust);
        }
        let open = self.body_or_semicolon(tail, end)?;
        if self.is_punct(open, b';') {
            return Ok(open + 1); // Trait/foreign declaration, not an editable body.
        }
        let close = self.group_end(open, end)?;
        let mut opaque = attributed;
        let mut j = open + 1;
        while j < close {
            self.budget.work(1)?;
            // A nested item is not a body-local expression edit: it can contain
            // signatures/imports/attributes of its own. Do not try to infer its
            // visibility or treat nested functions as independent merge slots.
            if [b"fn".as_slice(), b"use", b"mod", b"struct", b"enum", b"union", b"impl",
                b"trait", b"type", b"const", b"static", b"extern", b"macro"]
                .iter().any(|keyword| self.is_word(j, keyword))
                || self.is_punct(j, b'#') || self.is_punct(j, b'$')
            {
                return Err(ConflictKind::AmbiguousRust);
            }
            let invocation = self.is_punct(j, b'!') && j > open + 1
                && self.tokens[j - 1].kind == Kind::Ident;
            if invocation {
                let mut group = j + 1;
                if self.is_word(j - 1, b"macro_rules") {
                    if self.tokens.get(group).is_none_or(|token| token.kind != Kind::Ident) {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    group += 1;
                }
                if !matches!(self.tokens.get(group).map(|token| token.kind),
                    Some(Kind::Punct(b'{' | b'(' | b'[')))
                {
                    return Err(ConflictKind::AmbiguousRust);
                }
                let last = self.group_end(group, close)?;
                opaque = true;
                j = last + 1;
            } else {
                j += 1;
            }
        }
        self.functions.push(Function {
            body: self.tokens[open].span.end..self.tokens[close].span.start,
            opaque,
        });
        Ok(close + 1)
    }

    fn items(&mut self, mut i: usize, end: usize, depth: usize, inherited: bool) -> Checked<()> {
        if depth > self.budget.limits.max_depth {
            return Err(ConflictKind::LimitExceeded(LimitKind::Nesting));
        }
        while i < end {
            self.budget.work(1)?;
            if self.is_punct(i, b';') {
                i += 1;
                continue;
            }
            let mut attributed = inherited;
            while self.is_punct(i, b'#') {
                i += 1;
                if self.is_punct(i, b'!') {
                    // An inner attribute can affect every item in its container.
                    return Err(ConflictKind::AmbiguousRust);
                }
                if !self.is_punct(i, b'[') {
                    return Err(ConflictKind::AmbiguousRust);
                }
                i = self.group_end(i, end)? + 1;
                attributed = true;
            }
            if self.is_word(i, b"pub") {
                i += 1;
                if self.is_punct(i, b'(') {
                    i = self.group_end(i, end)? + 1;
                }
            }
            let head = i;
            let mut qualified = i;
            while self.is_word(qualified, b"async") || self.is_word(qualified, b"unsafe")
                || self.is_word(qualified, b"const") || self.is_word(qualified, b"extern")
            {
                let foreign = self.is_word(qualified, b"extern");
                qualified += 1;
                if foreign && self.tokens.get(qualified).is_some_and(|t| t.kind == Kind::Literal) {
                    qualified += 1;
                }
            }
            if self.is_word(qualified, b"fn") {
                i = self.function(qualified + 1, end, attributed)?;
                continue;
            }
            if self.is_word(i, b"unsafe") {
                i += 1;
            }
            if self.is_word(i, b"impl") || self.is_word(i, b"trait") || self.is_word(i, b"mod") {
                let module = self.is_word(i, b"mod");
                let body = if module {
                    if self.tokens.get(i + 1).is_none_or(|token| token.kind != Kind::Ident) {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    i + 2
                } else {
                    self.body_or_semicolon(i + 1, end)?
                };
                if self.is_punct(body, b';') {
                    if !module {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    i = body + 1;
                } else {
                    if !self.is_punct(body, b'{') {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    let close = self.group_end(body, end)?;
                    self.items(body + 1, close, depth + 1, attributed)?;
                    i = close + 1;
                }
            } else if self.is_word(i, b"use") || self.is_word(i, b"type")
                || self.is_word(i, b"const") || self.is_word(i, b"static")
            {
                i = self.terminated(i + 1, end)?;
            } else if self.is_word(i, b"struct") || self.is_word(i, b"enum") || self.is_word(i, b"union") {
                let body = self.body_or_semicolon(i + 1, end)?;
                i = if self.is_punct(body, b';') { body + 1 } else { self.group_end(body, end)? + 1 };
            } else if self.is_word(i, b"extern") {
                if self.is_word(i + 1, b"crate") {
                    i = self.terminated(i + 2, end)?;
                } else {
                    let body = self.body_or_semicolon(i + 1, end)?;
                    if !self.is_punct(body, b'{') {
                        return Err(ConflictKind::AmbiguousRust);
                    }
                    i = self.group_end(body, end)? + 1;
                }
            } else if self.is_word(i, b"macro_rules") && self.is_punct(i + 1, b'!') {
                if self.tokens.get(i + 2).is_none_or(|t| t.kind != Kind::Ident)
                    || !matches!(self.tokens.get(i + 3).map(|t| t.kind), Some(Kind::Punct(b'{' | b'(' | b'[')))
                {
                    return Err(ConflictKind::AmbiguousRust);
                }
                i = self.group_end(i + 3, end)? + 1;
            } else {
                // A named item macro is opaque. Never interpret `fn` inside it.
                let mut bang = head;
                while bang < end && (self.tokens[bang].kind == Kind::Ident || self.is_punct(bang, b':')) {
                    bang += 1;
                }
                if bang == head || !self.is_punct(bang, b'!')
                    || !matches!(self.tokens.get(bang + 1).map(|t| t.kind), Some(Kind::Punct(b'{' | b'(' | b'[')))
                {
                    return Err(ConflictKind::AmbiguousRust);
                }
                i = self.group_end(bang + 1, end)? + 1;
            }
        }
        Ok(())
    }
}

fn scan_rust(bytes: &[u8], budget: &mut Budget) -> Checked<Parsed> {
    let tokens = lex(bytes, budget)?;
    let end = tokens.len();
    let mut scanner = Scanner { bytes, tokens, functions: Vec::new(), budget };
    scanner.items(0, end, 0, false)?;
    Ok(Parsed { functions: scanner.functions })
}

fn same_shell(
    base: &[u8], original: &Parsed, result: &[u8], parsed: &Parsed, budget: &mut Budget,
) -> Checked<bool> {
    if original.functions.len() != parsed.functions.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (0, 0);
    for (old, new) in original.functions.iter().zip(&parsed.functions) {
        budget.work((old.body.start - a).saturating_add(new.body.start - b))?;
        if base[a..old.body.start] != result[b..new.body.start] {
            return Ok(false);
        }
        a = old.body.end;
        b = new.body.end;
    }
    budget.work((base.len() - a).saturating_add(result.len() - b))?;
    Ok(base[a..] == result[b..])
}

fn locate(parsed: &Parsed, range: &Range<usize>) -> Checked<usize> {
    for (index, function) in parsed.functions.iter().enumerate() {
        if function.body.start <= range.start && range.end <= function.body.end {
            if function.opaque {
                return Err(ConflictKind::MacroOrAttribute);
            }
            return Ok(index);
        }
    }
    Err(ConflictKind::OutsideFunction)
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
