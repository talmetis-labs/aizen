//! Bounded Markdown plan contracts. File contents are data, never execution permission.

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

use crate::core::file_placement::{read_existing_regular, validate_managed_name};
use crate::core::workspace_txn::NativeResource;

pub(crate) const MAX_DOCUMENT_BYTES: usize = 65_536;
const MAX_HEADER_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 512;
const MAX_EXCERPT_BYTES: usize = 6_144;
const MAX_PHASES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPlan {
    pub id: String,
    pub phases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadPlan {
    pub parsed: ParsedPlan,
    pub fingerprint: String,
    /// Raw, untrusted document data. The caller must encode it for its prompt/UI surface.
    pub excerpt: String,
}

pub(crate) fn valid_plan_id(id: &str) -> bool {
    id.len() == 34
        && id.starts_with("p-")
        && id.as_bytes()[2..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub(crate) fn valid_phase_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Never fall back to a path, clock, prose, or predictable counter as plan identity.
pub(crate) fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| {
        anyhow::anyhow!("OS randomness is required to allocate a plan ID: {error}")
    })?;
    let mut id = String::from("p-");
    for byte in bytes {
        use std::fmt::Write;
        write!(id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(id)
}

fn document_text(bytes: &[u8]) -> Result<&str> {
    ensure!(
        bytes.len() <= MAX_DOCUMENT_BYTES,
        "plan document exceeds {MAX_DOCUMENT_BYTES} bytes"
    );
    let text = std::str::from_utf8(bytes).context("plan document must be UTF-8")?;
    ensure!(
        bytes
            .iter()
            .enumerate()
            .all(|(i, b)| *b != b'\r' || bytes.get(i + 1) == Some(&b'\n')),
        "plan document must use LF or CRLF line endings"
    );
    Ok(text)
}

fn line_text(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

pub(crate) fn parse(bytes: &[u8]) -> Result<ParsedPlan> {
    let text = document_text(bytes)?;
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().context("plan document is empty")?;
    ensure!(line_text(first) == "---", "plan header must begin with ---");
    let mut header_bytes = first.len();
    let mut version_seen = false;
    let mut id = None;
    loop {
        let raw = lines.next().context("plan header is not closed")?;
        header_bytes += raw.len();
        ensure!(
            header_bytes <= MAX_HEADER_BYTES,
            "plan header exceeds {MAX_HEADER_BYTES} bytes"
        );
        let line = line_text(raw);
        if line == "---" {
            break;
        }
        // A tiny scalar grammar, not YAML: unindented keys, ASCII spaces, no comments,
        // quoting, aliases, tags, collections, blank lines, or multiline values.
        let (key, value) = line
            .split_once(": ")
            .context("plan header needs an unquoted key: value scalar")?;
        let value = value.trim_matches(' ');
        match key {
            "aizen_plan" => {
                ensure!(!version_seen, "duplicate aizen_plan header key");
                ensure!(value == "1", "unsupported or malformed aizen_plan version");
                version_seen = true;
            }
            "id" => {
                ensure!(id.is_none(), "duplicate plan id header key");
                ensure!(valid_plan_id(value), "invalid plan id");
                id = Some(value.to_owned());
            }
            _ => bail!("unknown or malformed plan header key"),
        }
    }
    ensure!(version_seen, "plan header is missing aizen_plan");
    let id = id.context("plan header is missing id")?;
    let mut phases = Vec::new();
    let mut seen = HashSet::new();
    let mut fence: Option<(u8, usize)> = None;
    for raw in lines {
        let line = line_text(raw);
        let run = fence_run(line);
        if let Some((delimiter, length)) = fence {
            if run.is_some_and(|(ch, n, rest)| {
                ch == delimiter && n >= length && rest.bytes().all(|b| b == b' ' || b == b'\t')
            }) {
                fence = None;
            }
            continue;
        }
        if let Some((delimiter, length, rest)) = run {
            if delimiter != b'`' || !rest.contains('`') {
                fence = Some((delimiter, length));
                continue;
            }
        }
        // Contract-looking comments must be exact whole lines, not partial matches.
        // Literal examples belong in fenced code blocks.
        if line.contains("<!--") && line.contains("aizen:") {
            let phase = line
                .strip_prefix("<!-- aizen:phase ")
                .and_then(|s| s.strip_suffix(" -->"))
                .context("phase marker must be exactly <!-- aizen:phase id -->")?;
            ensure!(valid_phase_id(phase), "invalid phase id");
            ensure!(seen.insert(phase), "duplicate phase id {phase}");
            ensure!(
                phases.len() < MAX_PHASES,
                "plan has more than {MAX_PHASES} phases"
            );
            phases.push(phase.to_owned());
        }
    }
    ensure!(!phases.is_empty(), "plan needs at least one phase marker");
    Ok(ParsedPlan { id, phases })
}

/// CommonMark fences: up to three leading spaces and at least three matching delimiters.
/// The caller distinguishes an opening info string from a closing whitespace-only suffix.
fn fence_run(line: &str) -> Option<(u8, usize, &str)> {
    let indent = line.bytes().take_while(|b| *b == b' ').count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let delimiter = *line.as_bytes().first()?;
    if delimiter != b'`' && delimiter != b'~' {
        return None;
    }
    let length = line.bytes().take_while(|b| *b == delimiter).count();
    (length >= 3).then(|| (delimiter, length, &line[length..]))
}

pub(crate) fn validate_relative_path(path: &str) -> Result<()> {
    ensure!(
        path.len() <= MAX_PATH_BYTES
            && path.starts_with("docs/plans/")
            && path.ends_with(".md")
            && !path.contains('\\'),
        "plan path must be a workspace-relative docs/plans/**/*.md path of at most {MAX_PATH_BYTES} bytes"
    );
    validate_managed_name(path)?;
    NativeResource::validate_spelling(Path::new(path))?;
    Ok(())
}

/// Binds each observation to checked non-link ancestors and a checked regular descriptor.
/// No inode is retained between calls: an editor's same-ID atomic save is a new observation.
/// These shared guards detect ordinary drift, not hostile kernel-atomic path substitution.
/// The bytes may be malformed: callers can fingerprint an invalid observation without adopting it.
pub(crate) fn read_bytes(root: &Path, relative: &str) -> Result<Vec<u8>> {
    validate_relative_path(relative)?;
    ensure!(root.is_absolute(), "plan workspace root must be absolute");
    NativeResource::validate_spelling(root)?;
    // Inspect the supplied ancestry BEFORE canonicalization could hide a link/reparse point.
    let resource = NativeResource::resolve(&root.join(relative))?;
    let workspace = std::fs::canonicalize(root).context("canonicalizing plan workspace")?;
    ensure!(
        resource.path() == workspace.join(relative),
        "plan path must use its canonical spelling beneath workspace docs/plans"
    );
    let bytes = read_existing_regular(resource.path(), MAX_DOCUMENT_BYTES as u64)?;
    resource.verify()?;
    Ok(bytes)
}

pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Raw untrusted text, capped at a UTF-8 boundary. Invalid UTF-8 has no display excerpt.
pub(crate) fn excerpt(bytes: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return String::new();
    };
    let mut end = text.len().min(MAX_EXCERPT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

pub(crate) fn read(root: &Path, relative: &str) -> Result<ReadPlan> {
    let bytes = read_bytes(root, relative)?;
    Ok(ReadPlan {
        parsed: parse(&bytes)?,
        fingerprint: fingerprint(&bytes),
        excerpt: excerpt(&bytes),
    })
}

/// Explicit migration only. The caller handles preview, approval, CAS and persistence.
/// Even fenced reserved tokens are refused rather than guessing whether they carry identity.
pub(crate) fn migrate_legacy(bytes: &[u8], id: &str) -> Result<String> {
    ensure!(valid_plan_id(id), "invalid migration plan id");
    let text = document_text(bytes)?;
    ensure!(
        !text.contains("aizen_plan") && !text.contains("aizen:"),
        "document already contains a plan contract or a malformed contract marker"
    );
    // An id-only frontmatter block can be a damaged modern header. Ordinary legacy
    // frontmatter without identity remains prose and is preserved, including its delimiters.
    let mut lines = text.lines();
    if lines
        .next()
        .is_some_and(|line| line.trim_start_matches('\u{feff}').trim() == "---")
    {
        for line in lines {
            if matches!(line.trim(), "---" | "...") {
                break;
            }
            if let Some((key, _)) = line.split_once(':') {
                ensure!(
                    key.trim().trim_matches(['\'', '"']) != "id",
                    "legacy header contains an ambiguous plan identity"
                );
            }
        }
    }
    let migrated =
        format!("---\naizen_plan: 1\nid: {id}\n---\n\n<!-- aizen:phase legacy -->\n\n{text}");
    // Includes the prepended bytes in the limit and validates the complete result.
    parse(migrated.as_bytes())?;
    Ok(migrated)
}

#[cfg(test)]
#[path = "plan_document_tests.rs"]
mod tests;
