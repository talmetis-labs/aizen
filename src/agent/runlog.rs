//! Run log — the durable, append-only event record for one agent run (workflow or single task).
//!
//! Design: event sourcing (Temporal-style). The orchestrator's decisions are a pure function of the
//! log: `plan(&[Event]) -> Vec<Action>`. Every side-effect (LLM call, tool call, spawn, note) is
//! recorded exactly once, with enough payload to replay it. A crash mid-run loses only the
//! in-flight event; `resume` re-reads the log, skips events that already have a recorded outcome,
//! and continues from the first gap.
//!
//! File layout: one JSONL file per run, `<scratch>/runs/<run-id>.jsonl`. Scratch, not `.aizen`,
//! because a run log is transient by definition — the run that owns it can sweep it on success, and
//! the per-run scratch dir is already the boundary for "state that must not outlive the task". The
//! `.aizen` tree is for durable user data (memory, sessions); a run log is neither.
//!
//! Every event has a monotonically increasing `seq` within its run, assigned by the writer, so a
//! reader can detect a torn write (the last line is not valid JSON) and treat it as "crashed here",
//! not "corrupt file".

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// One line in the run log. The variants are the side-effect kinds the orchestrator performs;
/// anything not in this enum is not recorded, which is the point — if it isn't here, `resume`
/// won't know it happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// An LLM call. `request_hash` is a stable hash of the messages+tools sent, so `resume` can
    /// detect "the inputs changed since the crash" (the caller mutated state) and refuse to replay
    /// rather than silently continuing with different context. The response is the raw text the
    /// model returned; tool calls live in their own events.
    Llm {
        seq: u64,
        request_hash: u64,
        response: String,
        /// Tokens billed, when the provider reports them. Needed for the budget gate (P5) to
        /// account for the call on resume without re-billing it.
        usage: Option<TokenUsage>,
    },
    /// A tool call and its outcome. `result` is the tool's return body (already truncated to the
    /// budget the run enforced), `error` is set instead when the call failed. On replay, a tool
    /// event with `result` present is skipped; one with `error` is re-run — a failed call is not a
    /// recorded side-effect, it's a recorded attempt.
    Tool {
        seq: u64,
        name: String,
        args_hash: u64,
        result: Option<String>,
        error: Option<String>,
    },
    /// A sub-agent spawn. `child_run_id` is the run-log id of the child, so the parent's `resume`
    /// can find and (if needed) resume the child rather than re-spawning it from scratch.
    Spawn {
        seq: u64,
        child_run_id: String,
        role: String,
        prompt_hash: u64,
    },
    /// A blackboard note or other free-form record the orchestrator chose to persist (phase
    /// boundary, stall-guard fire, verify-gate outcome). The `label` is the queryable handle;
    /// `body` is opaque to the log itself.
    Note {
        seq: u64,
        label: String,
        body: String,
    },
    /// A workflow-level task outcome (one sub-agent child run to its end). Recorded by
    /// `workflow.rs` after each `run_one_task` so a resume can skip tasks that already
    /// reached a terminal state instead of re-spending their tokens. `task_id` matches the
    /// workflow spec's task id; a fix-loop retry carries the `#n` suffix and supersedes the
    /// base id (see `plan`).
    Task {
        seq: u64,
        task_id: String,
        status: String,
        summary: String,
        iters: usize,
        tokens_in: u64,
        tokens_out: u64,
    },
    /// A signal delivered to a running child (Phase 2's nudge channel). Recorded so a resumed run
    /// re-delivers signals that arrived after the crash point, and does NOT re-deliver ones the
    /// child already consumed.
    Signal {
        seq: u64,
        target_run_id: String,
        body: String,
    },
}

impl Event {
    pub fn seq(&self) -> u64 {
        match self {
            Event::Llm { seq, .. }
            | Event::Tool { seq, .. }
            | Event::Spawn { seq, .. }
            | Event::Note { seq, .. }
            | Event::Task { seq, .. }
            | Event::Signal { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
}

/// The append-only writer for one run. Construct once at run start; every side-effect site calls
/// `record` with the event and gets back the assigned `seq`. The writer owns the file handle and
/// the next sequence number, so two parts of the orchestrator can't both write seq=7.
pub struct RunLogWriter {
    file: fs::File,
    next_seq: u64,
    path: PathBuf,
}

/// Where run logs live: `.aizen/runs/` (per the Phase-1 design doc). `AIZEN_RUNS_DIR` overrides
/// it — integration tests point it at a temp dir so they don't have to guess the scratch path.
pub fn runs_dir() -> PathBuf {
    std::env::var_os("AIZEN_RUNS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::scratch::dir().join("runs"))
}

impl RunLogWriter {
    /// Open (creating if needed) the log for `run_id` under `runs_dir`. If the file exists and is
    /// non-empty this is a RESUME: the writer picks up at the next unwritten seq, so the caller can
    /// re-run the orchestrator and have the log continue rather than restart at 0.
    pub fn open(runs_dir: &Path, run_id: &str) -> Result<Self> {
        fs::create_dir_all(runs_dir)
            .with_context(|| format!("creating run-log dir {}", runs_dir.display()))?;
        let path = runs_dir.join(format!("{run_id}.jsonl"));
        let next_seq = if path.exists() {
            // A torn last line (crash mid-write) reads as one fewer valid event; that's the correct
            // resume point — the partially-written event never had an outcome, so it SHOULD be re-run.
            read_events(&path)?.last().map(|e| e.seq() + 1).unwrap_or(0)
        } else {
            0
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening run log {}", path.display()))?;
        Ok(Self {
            file,
            next_seq,
            path,
        })
    }

    /// Append one event, assigning its sequence number. The caller passes the event with a
    /// placeholder seq (0 is fine); the writer overwrites it with the real one before serializing.
    /// Returns the assigned seq so the caller can reference it ("the tool call at seq=12").
    pub fn record(&mut self, mut event: Event) -> Result<u64> {
        let seq = self.next_seq;
        // Stamping seq into the event is the only mutation the writer performs — every other field
        // is owned by the caller. Doing it here (not at the call site) is what guarantees the
        // sequence is dense and monotonic even when two sites record concurrently.
        match &mut event {
            Event::Llm { seq: s, .. }
            | Event::Tool { seq: s, .. }
            | Event::Spawn { seq: s, .. }
            | Event::Note { seq: s, .. }
            | Event::Task { seq: s, .. }
            | Event::Signal { seq: s, .. } => *s = seq,
        }
        let line = serde_json::to_string(&event).context("serializing run-log event")?;
        self.file
            .write_all(line.as_bytes())
            .and_then(|_| self.file.write_all(b"\n"))
            .and_then(|_| self.file.flush())
            .with_context(|| format!("appending to {}", self.path.display()))?;
        self.next_seq += 1;
        Ok(seq)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read every valid event from a log file. A trailing partial line (crash mid-write) is dropped,
/// not an error — that event never completed, so the run SHOULD resume from before it. A malformed
/// line anywhere else is an error: it means the file was edited outside the writer, and replaying
/// from a corrupted log is worse than refusing.
pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut lines = reader.lines().enumerate().peekable();
    while let Some((idx, line)) = lines.next() {
        let line = line.with_context(|| format!("reading {}:{}", path.display(), idx + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(trimmed) {
            Ok(ev) => events.push(ev),
            Err(e) => {
                // The LAST line being invalid is the crash-mid-write case; tolerate it. Anything
                // earlier being invalid is corruption.
                if lines.peek().is_none() {
                    break;
                }
                return Err(anyhow!(
                    "malformed run-log event at {}:{}: {e}",
                    path.display(),
                    idx + 1
                ));
            }
        }
    }
    Ok(events)
}

/// The pure planning function the orchestrator's resume path uses. Given every event recorded so
/// far, return the sequence numbers that still need to run — the events that either failed (a Tool
/// with `error`) or whose outcome is missing. The orchestrator walks its own task list and, for
/// each step, asks "is there a recorded event for this step's id?" — this function is what answers
/// the generic half of that question ("which seqs are unresolved").
///
/// Deliberately minimal: the orchestrator owns the mapping from "my task step" to "the seq that
/// step recorded"; the log only knows which seqs are done. Keeping the two apart is what lets the
/// same log serve a workflow run, a single task, and (Phase 2) a resumed child.
pub fn unresolved(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| matches!(e, Event::Tool { error: Some(_), .. }))
        .map(|e| e.seq())
        .collect()
}

/// What a resume should do with one workflow task, derived purely from the recorded events.
/// `plan` is the seam the design doc calls for: a pure function over the log, so the skip
/// policy is unit-testable without a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// No terminal Task event for this id (or only failures): run it for real.
    Run,
    /// A skippable outcome is on the log; inject the recorded result and skip the LLM calls.
    Skip {
        status: String,
        summary: String,
        iters: usize,
        tokens_in: u64,
        tokens_out: u64,
    },
}

/// A task outcome counts as skippable when it reached a completed state. `error` and
/// `cancelled` do NOT: re-running them is the point of a resume. `json:invalid` stays
/// skippable — the child finished and produced a report; a resume that re-ran it would burn
/// tokens chasing a schema the first run already failed to satisfy.
fn task_outcome_skippable(status: &str) -> bool {
    !matches!(status, "error" | "cancelled")
}

/// `<id>#2` → `<id>`: a fix-loop retry supersedes its base attempt (mirrors `workflow::base_id`,
/// duplicated here so `plan` does not depend on the orchestrator module).
fn plan_base_id(id: &str) -> &str {
    match id.rsplit_once('#') {
        Some((base, n)) if !base.is_empty() && n.parse::<usize>().is_ok() => base,
        _ => id,
    }
}

/// Plan a resume: for each task id in spec order, decide Skip (a skippable outcome is on the
/// log) or Run. The LAST Task event per base id wins, so a fix-loop retry (`#2`) replaces the
/// failed first attempt. Ids in the log that the spec no longer names are ignored (the caller
/// reports them as orphans).
pub fn plan(events: &[Event], task_ids: &[&str]) -> Vec<Action> {
    let mut latest: std::collections::HashMap<&str, &Event> = std::collections::HashMap::new();
    for e in events {
        if let Event::Task { task_id, .. } = e {
            latest.insert(plan_base_id(task_id), e);
        }
    }
    task_ids
        .iter()
        .map(|id| match latest.get(id) {
            Some(Event::Task {
                status,
                summary,
                iters,
                tokens_in,
                tokens_out,
                ..
            }) if task_outcome_skippable(status) => Action::Skip {
                status: status.clone(),
                summary: summary.clone(),
                iters: *iters,
                tokens_in: *tokens_in,
                tokens_out: *tokens_out,
            },
            _ => Action::Run,
        })
        .collect()
}

/// One line per event, for `run-status` / `run-resume --dry-run` output.
pub fn describe(e: &Event) -> String {
    match e {
        Event::Llm {
            seq, request_hash, ..
        } => format!("#{seq:>4} llm hash={request_hash:#x}"),
        Event::Tool {
            seq, name, error, ..
        } => match error {
            Some(err) => format!("#{seq:>4} tool {name} → ERROR {err}"),
            None => format!("#{seq:>4} tool {name} → ok"),
        },
        Event::Spawn {
            seq,
            child_run_id,
            role,
            ..
        } => {
            format!("#{seq:>4} spawn {role} → {child_run_id}")
        }
        Event::Note { seq, label, .. } => format!("#{seq:>4} note {label}"),
        Event::Task {
            seq,
            task_id,
            status,
            iters,
            tokens_in,
            tokens_out,
            ..
        } => format!(
            "#{seq:>4} task {task_id} → {status} [{iters} step(s), {tokens_in}+{tokens_out} tok]"
        ),
        Event::Signal {
            seq, target_run_id, ..
        } => {
            format!("#{seq:>4} signal → {target_run_id}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aizen-runlog-test-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_reads_back_dense_sequence() {
        let dir = temp_dir("dense");
        let mut w = RunLogWriter::open(&dir, "run-1").unwrap();
        let s0 = w
            .record(Event::Note {
                seq: 0,
                label: "start".into(),
                body: "begin".into(),
            })
            .unwrap();
        let s1 = w
            .record(Event::Tool {
                seq: 0,
                name: "file_read".into(),
                args_hash: 42,
                result: Some("ok".into()),
                error: None,
            })
            .unwrap();
        assert_eq!((s0, s1), (0, 1));
        let events = read_events(w.path()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq(), 0);
        assert_eq!(events[1].seq(), 1);
    }

    #[test]
    fn reopen_continues_sequence_instead_of_restarting() {
        let dir = temp_dir("resume");
        {
            let mut w = RunLogWriter::open(&dir, "run-2").unwrap();
            w.record(Event::Note {
                seq: 0,
                label: "a".into(),
                body: String::new(),
            })
            .unwrap();
            w.record(Event::Note {
                seq: 0,
                label: "b".into(),
                body: String::new(),
            })
            .unwrap();
        }
        // Simulate a crash and reopen: the writer must pick up at seq 2, not 0.
        let mut w = RunLogWriter::open(&dir, "run-2").unwrap();
        let s = w
            .record(Event::Note {
                seq: 0,
                label: "c".into(),
                body: String::new(),
            })
            .unwrap();
        assert_eq!(s, 2);
        assert_eq!(read_events(w.path()).unwrap().len(), 3);
    }

    #[test]
    fn torn_last_line_is_tolerated_and_resumed_from() {
        let dir = temp_dir("torn");
        let path = {
            let mut w = RunLogWriter::open(&dir, "run-3").unwrap();
            w.record(Event::Note {
                seq: 0,
                label: "a".into(),
                body: String::new(),
            })
            .unwrap();
            w.record(Event::Note {
                seq: 0,
                label: "b".into(),
                body: String::new(),
            })
            .unwrap();
            w.path().to_path_buf()
        };
        // Simulate a crash mid-write: append a partial JSON line.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"kind\":\"note\",\"seq\":2,\"label\":\"cr")
            .unwrap();
        drop(f);
        // The reader sees only the two complete events...
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        // ...and a resumed writer re-uses seq 2 for the event that was torn.
        let mut w = RunLogWriter::open(&dir, "run-3").unwrap();
        assert_eq!(
            w.record(Event::Note {
                seq: 0,
                label: "c".into(),
                body: String::new()
            })
            .unwrap(),
            2
        );
    }

    #[test]
    fn unresolved_reports_failed_tool_calls_only() {
        let events = vec![
            Event::Tool {
                seq: 0,
                name: "a".into(),
                args_hash: 0,
                result: Some("ok".into()),
                error: None,
            },
            Event::Tool {
                seq: 1,
                name: "b".into(),
                args_hash: 0,
                result: None,
                error: Some("boom".into()),
            },
            Event::Note {
                seq: 2,
                label: "x".into(),
                body: String::new(),
            },
            Event::Tool {
                seq: 3,
                name: "c".into(),
                args_hash: 0,
                result: None,
                error: Some("bang".into()),
            },
        ];
        assert_eq!(unresolved(&events), vec![1, 3]);
    }

    #[test]
    fn malformed_middle_line_is_an_error() {
        let dir = temp_dir("corrupt");
        let path = dir.join("bad.jsonl");
        fs::write(
            &path,
            "{\"kind\":\"note\",\"seq\":0,\"label\":\"a\",\"body\":\"\"}\nnot json\n{\"kind\":\"note\",\"seq\":2,\"label\":\"c\",\"body\":\"\"}\n",
        )
        .unwrap();
        let err = read_events(&path).unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    #[test]
    fn plan_skips_completed_tasks_and_reruns_failures() {
        let events = vec![
            Event::Task {
                seq: 1,
                task_id: "scout".into(),
                status: "ok".into(),
                summary: "found it".into(),
                iters: 3,
                tokens_in: 100,
                tokens_out: 50,
            },
            Event::Task {
                seq: 2,
                task_id: "impl".into(),
                status: "error".into(),
                summary: "boom".into(),
                iters: 0,
                tokens_in: 0,
                tokens_out: 0,
            },
            // A fix-loop retry supersedes the errored base attempt.
            Event::Task {
                seq: 3,
                task_id: "impl#2".into(),
                status: "ok".into(),
                summary: "fixed".into(),
                iters: 5,
                tokens_in: 200,
                tokens_out: 80,
            },
            Event::Note {
                seq: 4,
                label: "workflow-spec".into(),
                body: String::new(),
            },
        ];
        let actions = plan(&events, &["scout", "impl", "verify"]);
        assert_eq!(actions.len(), 3);
        assert!(matches!(&actions[0], Action::Skip { summary, .. } if summary == "found it"));
        assert!(matches!(&actions[1], Action::Skip { summary, .. } if summary == "fixed"));
        assert_eq!(actions[2], Action::Run);
    }

    #[test]
    fn plan_ignores_task_events_for_ids_not_in_the_spec() {
        let events = vec![Event::Task {
            seq: 1,
            task_id: "dropped-task".into(),
            status: "ok".into(),
            summary: "s".into(),
            iters: 1,
            tokens_in: 1,
            tokens_out: 1,
        }];
        assert_eq!(plan(&events, &["still-there"]), vec![Action::Run]);
    }

    #[test]
    fn task_event_roundtrips_through_jsonl() {
        let dir = temp_dir("task-roundtrip");
        let mut w = RunLogWriter::open(&dir, "run-t").unwrap();
        w.record(Event::Task {
            seq: 0,
            task_id: "verify".into(),
            status: "json:ok".into(),
            summary: "{}".into(),
            iters: 2,
            tokens_in: 10,
            tokens_out: 5,
        })
        .unwrap();
        let events = read_events(w.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(describe(&events[0]).contains("task verify"));
        assert!(matches!(
            plan(&events, &["verify"]).as_slice(),
            [Action::Skip { .. }]
        ));
    }
}
