//! Age-based observation collapsing and file-system spill for tool results.
//!
//! Two SWE-agent findings, applied to the transcript rather than to the tool budgets: the model
//! works best with a short window of RECENT observations in full, and it must be able to get an old
//! one back without re-running a command. Before this, an old `cargo test` log or a 12 KB
//! `search_files` sweep stayed verbatim in history until the context-percentage clearing pass
//! evicted it to a generic placeholder — full price on every request in between, gone for good
//! afterwards.
//!
//! Collapsing: once more than `keep_recent` newer tool results exist, an older result longer than
//! `min_chars` becomes one line — the tool, its target, its size, and the scratch file holding the
//! full text. It fires in BATCHES of `batch` candidates: every mid-history rewrite invalidates the
//! provider's prompt cache from that byte onward, so one rewrite per batch of observations is the
//! cadence, not one per step. Independent of context %, so the working set has the same shape on a
//! 32k local model and a 200k hosted one. The read-cache short-circuit revalidates a hit against
//! the result's length and prefix, so a collapsed result can never be answered with "see the
//! earlier result" — the next identical read pays for a real read, which is what the model wants.
//!
//! Spill: a raw result longer than `spill_over` is written to the scratch dir in full BEFORE the
//! budget cut, and the cut result ends with the path. Nothing the tool produced is lost; the model
//! reads the part it needs with `file_read` instead of re-running the command. Skipped for
//! `file_read` itself — that file is already on disk.

use crate::core::types::Message;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Every collapsed digest starts with this — the idempotence check and what a reader greps for.
pub const COLLAPSED_PREFIX: &str = "[collapsed]";
const SPILL_NOTE_OPEN: &str = "[full output saved to ";
const SPILL_NOTE_SEP: &str = " — ";

#[derive(Clone, Copy, Debug)]
pub struct CollapsePolicy {
    /// The newest N tool results are never collapsed.
    pub keep_recent: usize,
    /// Collapse only when at least this many older results qualify, then all of them at once.
    pub batch: usize,
    /// A result at or under this many chars is cheaper than its digest — left alone.
    pub min_chars: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollapseStats {
    pub collapsed: usize,
    pub chars_reclaimed: usize,
}

/// Replace aged, bulky tool results with one-line digests. `spill(tool, body)` persists a body and
/// returns where; a body that already carries a spill note reuses that path. Messages other than
/// tool results, the newest `keep_recent` results, short results, and digests are never touched;
/// `tool_call_id` always survives, so the assistant/tool pairing strict gateways check stays valid.
pub fn collapse_aged(
    messages: &mut [Message],
    policy: CollapsePolicy,
    spill: &mut dyn FnMut(&str, &str) -> Option<PathBuf>,
) -> CollapseStats {
    let mut stats = CollapseStats::default();
    if policy.keep_recent == 0 {
        return stats;
    }
    let tool_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .map(|(i, _)| i)
        .collect();
    if tool_idxs.len() <= policy.keep_recent {
        return stats;
    }
    let aged = &tool_idxs[..tool_idxs.len() - policy.keep_recent];
    let candidates: Vec<usize> = aged
        .iter()
        .copied()
        .filter(|&i| is_candidate(&messages[i], policy.min_chars))
        .collect();
    if candidates.len() < policy.batch.max(1) {
        return stats;
    }
    // Pair each result with the call that produced it — the digest names the tool and its target.
    let calls: HashMap<String, (String, String)> = messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .map(|tc| {
            (
                tc.id.clone(),
                (tc.function.name.clone(), tc.function.arguments.clone()),
            )
        })
        .collect();
    for i in candidates {
        let body = messages[i].content.take().unwrap_or_default();
        let (tool, args) = messages[i]
            .tool_call_id
            .as_deref()
            .and_then(|id| calls.get(id))
            .map(|(n, a)| (n.as_str(), a.as_str()))
            .unwrap_or(("tool", "{}"));
        let path = match spilled_path_in(&body) {
            Some(p) => Some(PathBuf::from(p)),
            None => spill(tool, &body),
        };
        let line = digest(tool, args, &body, path.as_deref());
        stats.chars_reclaimed += body.chars().count().saturating_sub(line.chars().count());
        stats.collapsed += 1;
        messages[i].content = Some(line);
    }
    stats
}

fn is_candidate(m: &Message, min_chars: usize) -> bool {
    match m.content.as_deref() {
        Some(b) => !b.starts_with(COLLAPSED_PREFIX) && b.chars().count() > min_chars,
        None => false,
    }
}

/// The one line an aged result becomes. A failure keeps its first line — the error is the signal
/// the model may still be acting on.
fn digest(tool: &str, args: &str, body: &str, path: Option<&Path>) -> String {
    let mut out = format!("{COLLAPSED_PREFIX} {tool}");
    let target = arg_summary(args);
    if !target.is_empty() {
        out.push(' ');
        out.push_str(&target);
    }
    out.push_str(&format!(
        " · {} lines · {}",
        body.lines().count(),
        human_size(body.len())
    ));
    if super::is_failure_result(body) {
        let first: String = body
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        out.push_str(" · ");
        out.push_str(first.trim());
    }
    match path {
        Some(p) => out.push_str(&format!(
            " · full text at {} (file_read it if needed)",
            p.display()
        )),
        None => out.push_str(" · full text dropped (re-run the tool if needed)"),
    }
    out
}

/// The argument that names what a call was about, one line, at most 80 chars.
fn arg_summary(args: &str) -> String {
    const KEYS: [&str; 12] = [
        "path", "file", "files", "cmd", "command", "query", "pattern", "url", "prompt", "name",
        "symbol", "id",
    ];
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return String::new();
    };
    let Some(obj) = v.as_object() else {
        return String::new();
    };
    for key in KEYS {
        let text = match obj.get(key) {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(serde_json::Value::Array(items)) if !items.is_empty() => {
                let shown: Vec<&str> = items.iter().filter_map(|i| i.as_str()).take(3).collect();
                if shown.is_empty() {
                    continue;
                }
                let mut s = shown.join(", ");
                if items.len() > shown.len() {
                    s.push_str(" …");
                }
                s
            }
            _ => continue,
        };
        let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        return if one_line.chars().count() > 80 {
            let cut: String = one_line.chars().take(79).collect();
            format!("{cut}…")
        } else {
            one_line
        };
    }
    String::new()
}

pub(crate) fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

static SPILL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Write `body` under this run's scratch dir; `None` when the disk says no (the caller then keeps
/// whatever it was going to keep and says the text was dropped).
pub fn spill_to_scratch(tool: &str, body: &str) -> Option<PathBuf> {
    let dir = crate::core::scratch::dir().join("tool-output");
    std::fs::create_dir_all(&dir).ok()?;
    let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = dir.join(format!("{seq:04}-{safe}.txt"));
    std::fs::write(&path, body).ok()?;
    Some(path)
}

/// The trailer a cut result carries when its full text was spilled.
pub fn spill_note(path: &Path, body: &str) -> String {
    format!(
        "{SPILL_NOTE_OPEN}{}{SPILL_NOTE_SEP}{}, {} lines; file_read it for the parts cut above]",
        path.display(),
        human_size(body.len()),
        body.lines().count()
    )
}

/// The path a spill note names, when a result carries one.
pub fn spilled_path_in(body: &str) -> Option<&str> {
    let start = body.rfind(SPILL_NOTE_OPEN)? + SPILL_NOTE_OPEN.len();
    let rest = &body[start..];
    let end = rest.find(SPILL_NOTE_SEP)?;
    Some(&rest[..end])
}

/// Append the note only when the cut actually dropped something; an uncut result needs no pointer.
pub fn attach_spill_note(cut: String, raw_chars: usize, note: Option<String>) -> String {
    match note {
        Some(n) if cut.chars().count() < raw_chars => format!("{cut}\n{n}"),
        _ => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionCall, ToolCall};

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    /// `n` call/result pairs after a system + user prelude; every body is `size` chars.
    fn history(n: usize, size: usize) -> Vec<Message> {
        let mut msgs = vec![Message::system("sys"), Message::user("task")];
        for k in 0..n {
            let id = format!("c{k}");
            msgs.push(Message::assistant_tool_calls(vec![call(
                &id,
                "shell_run",
                &format!("{{\"cmd\": \"cargo test --lib step{k}\"}}"),
            )]));
            msgs.push(Message::tool_result(id, "x".repeat(size)));
        }
        msgs
    }

    fn policy(keep: usize, batch: usize) -> CollapsePolicy {
        CollapsePolicy {
            keep_recent: keep,
            batch,
            min_chars: 100,
        }
    }

    fn fake_spill(tool: &str, _body: &str) -> Option<PathBuf> {
        Some(PathBuf::from(format!("/scratch/{tool}.txt")))
    }

    fn tool_bodies(msgs: &[Message]) -> Vec<String> {
        msgs.iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn nothing_happens_inside_the_window_or_below_the_batch() {
        let mut msgs = history(8, 500);
        let stats = collapse_aged(&mut msgs, policy(8, 4), &mut fake_spill);
        assert_eq!(stats, CollapseStats::default());
        // Nine results: one aged, batch of four not reached → untouched.
        let mut msgs = history(9, 500);
        let stats = collapse_aged(&mut msgs, policy(8, 4), &mut fake_spill);
        assert_eq!(stats.collapsed, 0);
        assert!(tool_bodies(&msgs).iter().all(|b| b.len() == 500));
    }

    #[test]
    fn a_full_batch_collapses_every_aged_result_at_once_and_keeps_the_window() {
        let mut msgs = history(12, 500);
        let stats = collapse_aged(&mut msgs, policy(8, 4), &mut fake_spill);
        assert_eq!(stats.collapsed, 4);
        assert!(stats.chars_reclaimed > 4 * 300, "{stats:?}");
        let bodies = tool_bodies(&msgs);
        for (k, b) in bodies.iter().enumerate() {
            if k < 4 {
                assert!(b.starts_with(COLLAPSED_PREFIX), "aged #{k}: {b}");
                assert!(b.contains("shell_run cargo test --lib step"), "{b}");
                assert!(b.contains("1 lines · 500 B"), "{b}");
                assert!(b.contains("/scratch/shell_run.txt"), "{b}");
            } else {
                assert_eq!(b.len(), 500, "recent #{k} kept verbatim");
            }
        }
        // Pairing intact.
        for m in msgs.iter().filter(|m| m.role == "tool") {
            assert!(m.tool_call_id.is_some());
        }
        // Idempotent: the digests are short and already prefixed.
        let again = collapse_aged(&mut msgs, policy(8, 1), &mut fake_spill);
        assert_eq!(again.collapsed, 0);
    }

    #[test]
    fn short_results_and_non_tool_messages_are_never_touched() {
        let mut msgs = history(12, 50);
        let stats = collapse_aged(&mut msgs, policy(4, 1), &mut fake_spill);
        assert_eq!(stats.collapsed, 0);
        assert_eq!(msgs[0].content.as_deref(), Some("sys"));
        assert_eq!(msgs[1].content.as_deref(), Some("task"));
        assert!(msgs
            .iter()
            .all(|m| m.role != "tool" || m.content.as_deref() == Some(&"x".repeat(50))));
    }

    #[test]
    fn a_failure_keeps_its_first_line_and_a_spilled_body_reuses_its_path() {
        let mut msgs = history(6, 500);
        let big = format!(
            "exit 101\nerror[E0308]: mismatched types\n{}\n{}",
            "y".repeat(400),
            spill_note(Path::new("/scratch/earlier.txt"), "zzz")
        );
        msgs[3].content = Some(big);
        let mut spills = 0usize;
        let stats = collapse_aged(&mut msgs, policy(2, 1), &mut |t, b| {
            spills += 1;
            fake_spill(t, b)
        });
        assert_eq!(stats.collapsed, 4);
        let first = tool_bodies(&msgs).remove(0);
        assert!(
            first.starts_with("[collapsed] shell_run cargo test --lib step0"),
            "{first}"
        );
        assert!(
            first.contains("· exit 101 ·"),
            "failure first line kept: {first}"
        );
        assert!(
            first.contains("/scratch/earlier.txt"),
            "reused the spill path: {first}"
        );
        assert_eq!(
            spills, 3,
            "only the three bodies without a note were spilled"
        );
    }

    #[test]
    fn a_missing_spill_says_so_instead_of_pretending() {
        let mut msgs = history(3, 500);
        let stats = collapse_aged(&mut msgs, policy(1, 1), &mut |_, _| None);
        assert_eq!(stats.collapsed, 2);
        assert!(tool_bodies(&msgs)[0].contains("full text dropped"));
    }

    #[test]
    fn arg_summary_picks_the_target_and_stays_one_short_line() {
        assert_eq!(
            arg_summary(r#"{"path": "src/main.rs", "offset": 3}"#),
            "src/main.rs"
        );
        assert_eq!(
            arg_summary(r#"{"files": ["a.rs", "b.rs", "c.rs", "d.rs"]}"#),
            "a.rs, b.rs, c.rs …"
        );
        assert_eq!(arg_summary("{\"cmd\": \"cargo\\n  test\"}"), "cargo test");
        assert_eq!(arg_summary(r#"{"n": 3}"#), "");
        assert_eq!(arg_summary("not json"), "");
        let long = format!("{{\"query\": \"{}\"}}", "q".repeat(200));
        let s = arg_summary(&long);
        assert_eq!(s.chars().count(), 80);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn spill_note_round_trips_its_path_and_attaches_only_after_a_cut() {
        let note = spill_note(
            Path::new("C:\\tmp\\aizen-scratch\\0001-shell_run.txt"),
            "a\nb\nc",
        );
        assert_eq!(
            spilled_path_in(&format!("head…\n{note}")),
            Some("C:\\tmp\\aizen-scratch\\0001-shell_run.txt")
        );
        assert!(note.contains("3 lines"));
        assert_eq!(spilled_path_in("no note here"), None);
        let cut = "short".to_string();
        assert_eq!(
            attach_spill_note(cut.clone(), 5, Some(note.clone())),
            "short",
            "nothing was cut → no pointer"
        );
        assert!(attach_spill_note(cut, 5000, Some(note)).ends_with(']'));
    }

    #[test]
    fn spill_to_scratch_writes_the_body_and_names_the_tool() {
        let p = spill_to_scratch("shell/run", "hello\nworld").expect("scratch is writable");
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-shell-run.txt"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\nworld");
        let _ = std::fs::remove_file(&p);
    }
}
