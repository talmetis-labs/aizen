//! The child context pack: what the parent already knows, handed to a delegated child so it
//! starts from the parent's position instead of from zero. Three parts, each optional:
//!
//! - the locations the parent has read in this conversation, harvested from the parent's
//!   read-cache scope (the store the identical-re-read short-circuit already keeps, so nothing is
//!   recorded twice);
//! - the findings the parent states as established — the `context` arg on `task`, or a workflow
//!   task's `context` field;
//! - the todo item the parent is working on.
//!
//! Hard-capped so the pack can never crowd out the brief, and rendered INSIDE the child's one user
//! message ahead of the brief: a second user message before the brief would break the strict role
//! alternation some providers enforce, and the brief is what the child must read last.

use serde_json::Value;
use std::path::Path;

/// Most read locations listed.
pub const MAX_READS: usize = 15;
/// Most findings listed.
pub const MAX_FINDINGS: usize = 10;
/// A finding longer than this is clipped — a finding is a line, not a report.
pub const MAX_FINDING_CHARS: usize = 300;
/// The whole pack, tag included.
pub const MAX_CHARS: usize = 2_500;
/// The block's tag in the child's user message.
pub const TAG: &str = "parent_context";

/// One location the parent read: a path (relative to the project root when it lies under it) and
/// the requested line window when the read was ranged — `(start, Some(end))`, or `(start, None)`
/// for a read that ran to the end of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadLoc {
    pub path: String,
    pub range: Option<(usize, Option<usize>)>,
}

impl ReadLoc {
    fn render(&self) -> String {
        match self.range {
            Some((a, Some(b))) => format!("{}:{a}-{b}", self.path),
            Some((a, None)) => format!("{}:{a}-", self.path),
            None => self.path.clone(),
        }
    }
}

/// The `context` arg: an array of strings, or one string split on newlines. Bullet markers and
/// whitespace are trimmed, empties dropped, at most [`MAX_FINDINGS`] kept, each clipped to
/// [`MAX_FINDING_CHARS`].
pub fn findings_from_args(args: &Value) -> Vec<String> {
    let raw: Vec<String> = match args.get("context") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s.lines().map(str::to_string).collect(),
        _ => Vec::new(),
    };
    raw.iter()
        .map(|s| s.trim().trim_start_matches(['-', '*', '•']).trim())
        .filter(|s| !s.is_empty())
        .map(|s| clip(s, MAX_FINDING_CHARS))
        .take(MAX_FINDINGS)
        .collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Render the pack, or `None` when there is nothing to hand over. Under the cap the parent's own
/// words outrank the reading list: the todo line is kept, then findings, then reads are dropped
/// from the oldest end first, and the count of what was omitted is said.
pub fn render(reads: &[ReadLoc], findings: &[String], todo: Option<&str>) -> Option<String> {
    let findings: Vec<&String> = findings.iter().take(MAX_FINDINGS).collect();
    let reads: Vec<&ReadLoc> = reads.iter().take(MAX_READS).collect();
    let todo = todo.map(str::trim).filter(|s| !s.is_empty());
    if findings.is_empty() && reads.is_empty() && todo.is_none() {
        return None;
    }
    let mut n_reads = reads.len();
    let mut n_findings = findings.len();
    loop {
        let text = assemble(
            &reads[..n_reads],
            &findings[..n_findings],
            todo,
            reads.len() - n_reads,
            findings.len() - n_findings,
        );
        if text.chars().count() <= MAX_CHARS {
            return Some(text);
        }
        if n_reads > 0 {
            n_reads -= 1;
        } else if n_findings > 0 {
            n_findings -= 1;
        } else {
            return Some(clip(&text, MAX_CHARS));
        }
    }
}

fn assemble(
    reads: &[&ReadLoc],
    findings: &[&String],
    todo: Option<&str>,
    reads_dropped: usize,
    findings_dropped: usize,
) -> String {
    let mut out = format!("<{TAG}>\n");
    if let Some(t) = todo {
        out.push_str(&format!("Parent's current step: {t}\n"));
    }
    if !findings.is_empty() || findings_dropped > 0 {
        out.push_str("Established by the parent (do not re-derive):\n");
        for f in findings {
            out.push_str(&format!("- {f}\n"));
        }
        if findings_dropped > 0 {
            out.push_str(&format!("- … {findings_dropped} more omitted\n"));
        }
    }
    if !reads.is_empty() || reads_dropped > 0 {
        out.push_str("Locations the parent already read (start there instead of searching):\n");
        for r in reads {
            out.push_str(&format!("- {}\n", r.render()));
        }
        if reads_dropped > 0 {
            out.push_str(&format!("- … {reads_dropped} more omitted\n"));
        }
    }
    out.push_str(&format!("</{TAG}>"));
    out
}

/// Build the pack for one dispatch from the parent's state: its read-cache scope, the findings it
/// passed, and the process-global todo list's in-progress item.
pub fn gather(root: &Path, parent_scope: &str, findings: &[String]) -> Option<String> {
    let reads: Vec<ReadLoc> = crate::agent::read_cache_recent(parent_scope, MAX_READS)
        .into_iter()
        .map(|(path, range)| ReadLoc {
            path: display_path(root, &path),
            range,
        })
        .collect();
    let todo = crate::agent::todo::active_item();
    render(&reads, findings, todo.as_deref())
}

/// A path as the model wrote it: relative to the root when under it, forward slashes. The read
/// cache stores canonical paths (a `\\?\` prefix on Windows), so the root is tried both as given
/// and canonicalized.
pub fn display_path(root: &Path, path: &Path) -> String {
    let rel = path
        .strip_prefix(root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let canon = root.canonicalize().ok()?;
            path.strip_prefix(&canon).ok().map(Path::to_path_buf)
        })
        .unwrap_or_else(|| path.to_path_buf());
    rel.to_string_lossy().replace('\\', "/")
}

/// The child's one user message: the pack (when any) ahead of the brief.
pub fn prepend(pack: Option<&str>, prompt: &str) -> String {
    match pack {
        Some(p) => format!("{p}\n\n{prompt}"),
        None => prompt.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_come_from_an_array_or_a_string_trimmed_and_capped() {
        let v = serde_json::json!({"context": ["- a", "  ", "* b ", "• c"]});
        assert_eq!(findings_from_args(&v), vec!["a", "b", "c"]);
        let v = serde_json::json!({"context": "one\n\n- two\n"});
        assert_eq!(findings_from_args(&v), vec!["one", "two"]);
        assert!(findings_from_args(&serde_json::json!({})).is_empty());
        assert!(findings_from_args(&serde_json::json!({"context": 7})).is_empty());
        let many: Vec<String> = (0..14).map(|i| format!("f{i}")).collect();
        assert_eq!(
            findings_from_args(&serde_json::json!({"context": many})).len(),
            MAX_FINDINGS
        );
        let long = "x".repeat(MAX_FINDING_CHARS + 50);
        let got = findings_from_args(&serde_json::json!({"context": [long]}));
        assert_eq!(got[0].chars().count(), MAX_FINDING_CHARS);
        assert!(got[0].ends_with('…'));
    }

    #[test]
    fn render_orders_todo_findings_reads_and_says_nothing_when_empty() {
        assert!(render(&[], &[], None).is_none());
        assert!(render(&[], &[], Some("  ")).is_none());
        let reads = vec![
            ReadLoc {
                path: "src/a.rs".into(),
                range: Some((10, Some(20))),
            },
            ReadLoc {
                path: "src/b.rs".into(),
                range: Some((5, None)),
            },
            ReadLoc {
                path: "src/c.rs".into(),
                range: None,
            },
        ];
        let findings = vec!["parser lives in a.rs".to_string()];
        let text = render(&reads, &findings, Some("fix the parser")).unwrap();
        assert!(text.starts_with("<parent_context>\nParent's current step: fix the parser\n"));
        let f = text.find("Established by the parent").unwrap();
        let r = text.find("Locations the parent already read").unwrap();
        assert!(f < r, "findings before reads:\n{text}");
        assert!(text.contains("- src/a.rs:10-20\n"));
        assert!(text.contains("- src/b.rs:5-\n"));
        assert!(text.contains("- src/c.rs\n"));
        assert!(text.ends_with("</parent_context>"));
        // No todo, no findings: only the reading list, no empty headers.
        let only_reads = render(&reads, &[], None).unwrap();
        assert!(!only_reads.contains("Established"));
        assert!(!only_reads.contains("current step"));
    }

    #[test]
    fn render_drops_reads_first_under_the_cap_and_names_the_omission() {
        let reads: Vec<ReadLoc> = (0..MAX_READS)
            .map(|i| ReadLoc {
                path: format!("{}/{i}.rs", "d".repeat(120)),
                range: Some((1, Some(999))),
            })
            .collect();
        // Ten 150-char findings fit under the cap on their own; fifteen long paths push the
        // whole pack over it, so the reads are what must give.
        let findings: Vec<String> = (0..MAX_FINDINGS)
            .map(|i| format!("{i}:{}", "f".repeat(150)))
            .collect();
        let text = render(&reads, &findings, Some("step")).unwrap();
        assert!(
            text.chars().count() <= MAX_CHARS,
            "{}",
            text.chars().count()
        );
        assert!(text.contains("more omitted"), "{text}");
        // Every finding survived; the reads absorbed the cut.
        assert!(text.contains("- 9:fff"), "{text}");
        assert!(text.contains("Parent's current step: step"));
    }

    #[test]
    fn prepend_leaves_the_brief_alone_without_a_pack() {
        assert_eq!(prepend(None, "do x"), "do x");
        assert_eq!(prepend(Some("<p>"), "do x"), "<p>\n\ndo x");
    }

    #[test]
    fn display_path_is_root_relative_with_forward_slashes() {
        let root = std::env::temp_dir().join(format!("aizen-ctx-pack-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join("src"));
        std::fs::write(root.join("src").join("a.rs"), "x").unwrap();
        // The read cache stores what `canonicalize` returns; the root is handed in as configured.
        let canon = root.join("src").join("a.rs").canonicalize().unwrap();
        assert_eq!(display_path(&root, &canon), "src/a.rs");
        assert_eq!(
            display_path(&root, &root.join("src").join("b.rs")),
            "src/b.rs"
        );
        let elsewhere = Path::new("/other/x.rs");
        assert_eq!(display_path(&root, elsewhere), "/other/x.rs");
        let _ = std::fs::remove_dir_all(&root);
    }
}
