//! The sibling blackboard: one directory per conversation under the run's scratch dir where the
//! harness files every delegated child's report as `<child>.md`, and which every child's
//! `<environment>` names. A child dispatched later — the next wave of a workflow, the next `task`
//! in the same turn — can `file_read` a sibling's full report instead of relying on the parent's
//! truncated relay of it (a 9-finding review reached the parent as findings 1–3 and 7–9). No new
//! tool: the notes are files, and reading files is what every role can already do.
//!
//! Append-only: a child's report is appended under a timestamped heading, never replaced, so a
//! retried task (`implement#2`) and a re-dispatched role both leave their history. The directory
//! lives under `scratch::dir()`, so the sweeper reclaims it with the run.

use std::path::{Path, PathBuf};

/// Most sibling notes named in a child's `<environment>` line.
const LISTED_MAX: usize = 12;
/// A report longer than this is clipped in the note — the note is a reference, not an archive.
const NOTE_MAX_CHARS: usize = 24_000;

/// The board for one conversation: `scratch/blackboard/<scope>` with the scope made path-safe.
pub fn dir_for(scope: &str) -> PathBuf {
    crate::core::scratch::dir()
        .join("blackboard")
        .join(safe_segment(scope))
}

/// A scope or child id as one path segment: `[A-Za-z0-9._-]`, everything else `_`, capped.
pub fn safe_segment(s: &str) -> String {
    let mut out: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("default");
    }
    out.truncate(80);
    out
}

/// File a child's report on the board. Best-effort: a board that cannot be written costs the
/// note, never the dispatch. Returns the note's path when written.
pub fn note(scope: &str, child_id: &str, report: &str) -> Option<PathBuf> {
    let text = report.trim();
    if text.is_empty() {
        return None;
    }
    let dir = dir_for(scope);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.md", safe_segment(child_id)));
    let mut body: String = text.chars().take(NOTE_MAX_CHARS).collect();
    if body.len() < text.len() {
        body.push_str("\n…[clipped]");
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let entry = format!("## {child_id} — {stamp}\n\n{body}\n\n");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    f.write_all(entry.as_bytes()).ok()?;
    Some(path)
}

/// The notes already on the board, newest first, as file names.
pub fn list(scope: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir_for(scope)) else {
        return Vec::new();
    };
    let mut rows: Vec<(std::time::SystemTime, String)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                return None;
            }
            let t = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((t, name))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, n)| n).collect()
}

/// The `<environment>` line for a child spawned under `scope`: where the board is, and which
/// sibling reports are already on it. Nothing is created here — the directory appears with the
/// first note.
pub fn env_line(scope: &str) -> String {
    let dir = dir_for(scope);
    let notes = list(scope);
    let mut line = format!("blackboard: {}", dir.display());
    if notes.is_empty() {
        line.push_str(" (sibling reports land here as <id>.md; none yet)");
    } else {
        let shown: Vec<&str> = notes.iter().take(LISTED_MAX).map(String::as_str).collect();
        let more = notes.len().saturating_sub(shown.len());
        line.push_str(&format!(
            " — sibling reports: {}{}; file_read one before repeating its work",
            shown.join(", "),
            if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            }
        ));
    }
    line
}

/// The note id for a `task` dispatch: the role label plus the dispatch counter from its scope
/// (`…/task/7` → `argus-7`), so two argus dispatches in one turn keep separate notes.
pub fn task_note_id(label: &str, child_scope: &str) -> String {
    let n = Path::new(child_scope)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if n.is_empty() {
        label.to_string()
    } else {
        format!("{label}-{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_path_safe_and_bounded() {
        assert_eq!(safe_segment("conv/abc:1 x"), "conv_abc_1_x");
        assert_eq!(safe_segment("implement#2"), "implement_2");
        assert_eq!(safe_segment("   "), "default");
        assert_eq!(safe_segment(&"a".repeat(200)).len(), 80);
    }

    #[test]
    fn notes_append_and_the_env_line_lists_them_newest_first() {
        let scope = format!("bb-test-{}-{}", std::process::id(), line!());
        assert!(env_line(&scope).contains("none yet"));
        assert!(
            note(&scope, "argus-1", "   ").is_none(),
            "an empty report is not filed"
        );
        let p = note(&scope, "argus-1", "found parse_kv at src/lib.rs:12").unwrap();
        assert!(p.ends_with("argus-1.md"), "{}", p.display());
        note(&scope, "argus-1", "second pass").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("## argus-1 — "));
        assert!(
            text.contains("found parse_kv") && text.contains("second pass"),
            "append-only"
        );
        note(
            &scope,
            "implement#2",
            "x".repeat(NOTE_MAX_CHARS + 10).as_str(),
        )
        .unwrap();
        let listed = list(&scope);
        assert_eq!(listed.len(), 2);
        assert!(listed.contains(&"implement_2.md".to_string()));
        let line = env_line(&scope);
        assert!(
            line.contains("sibling reports:") && line.contains("argus-1.md"),
            "{line}"
        );
        let clipped = std::fs::read_to_string(dir_for(&scope).join("implement_2.md")).unwrap();
        assert!(clipped.contains("…[clipped]"));
        let _ = std::fs::remove_dir_all(dir_for(&scope));
    }

    #[test]
    fn task_note_ids_carry_the_dispatch_counter() {
        assert_eq!(task_note_id("argus", "conv-1/task/7"), "argus-7");
        assert_eq!(task_note_id("nemesis", ""), "nemesis");
    }
}
