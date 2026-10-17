//! Run log — the durable, append-only event record for one agent run (workflow or single task).
//!
//! Workflow decisions are reconstructed from versioned dispatch, completion and settlement events.
//! A completion is flushed before releasing dependencies. An admitted attempt without completion has
//! an unknown side-effect outcome and requires reconciliation, never automatic writer replay.
//! This is process-crash continuation, not exactly-once effects or power-loss durability: flush is
//! not fsync. Legacy tool failures are diagnostic observations, not proof that nothing changed.
//!
//! File layout: one JSONL file per run, `<scratch>/runs/<run-id>.jsonl`. Scratch, not `.aizen`,
//! because a run log is transient by definition — the run that owns it can sweep it on success, and
//! the per-run scratch dir is already the boundary for "state that must not outlive the task". The
//! `.aizen` tree is for durable user data (memory, sessions); a run log is neither.
//!
//! Every event has a monotonically increasing `seq` within its run, assigned by the writer, so a
//! reader can detect a torn write (the last line is not valid JSON) and treat it as "crashed here",
//! not "corrupt file".

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// One line in the run log. The variants are the side-effect kinds the orchestrator performs;
/// anything not in this enum is not recorded, which is the point — if it isn't here, `resume`
/// won't know it happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "Durable event schema keeps dispatch intents inline; warning cleanup must not restructure journal records"
)]
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
    /// A tool outcome. Failure may follow partial effects; this event diagnoses it but never grants
    /// authority to replay a mutation.
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
    /// A legacy workflow task outcome. Exact spec IDs take precedence; old display suffixes are
    /// interpreted only when their ownership is unambiguous. New runs use typed attempts below.
    Task {
        seq: u64,
        task_id: String,
        status: String,
        summary: String,
        iters: usize,
        tokens_in: u64,
        tokens_out: u64,
    },
    WorkflowStarted {
        seq: u64,
        version: u32,
        fingerprint: u64,
        /// Rebuild immutable capabilities with the original prompt date, not the resume day's date.
        #[serde(default)]
        dispatch_date: Option<String>,
        task_ids: Vec<crate::agent::workflow::TaskId>,
        metis: bool,
    },
    DispatchIntent {
        seq: u64,
        intent: crate::agent::workflow::DispatchIntent,
    },
    AttemptCompleted {
        seq: u64,
        key: crate::agent::workflow::AttemptKey,
        role: String,
        model: String,
        status: String,
        summary: String,
        iters: usize,
        tokens_in: u64,
        tokens_out: u64,
    },
    SpeculativeCohortCaptured {
        seq: u64,
        cohort_id: String,
        members: Vec<crate::agent::workflow::AttemptKey>,
    },
    /// Emitted only after the complete selected set applied and parent verification accepted
    /// the integrated tree. It is evidence, never a capability to reopen or reapply overlays.
    SpeculativeCohortIntegrated {
        seq: u64,
        cohort_id: String,
        members: Vec<crate::agent::workflow::AttemptKey>,
        receipt: String,
    },
    TaskSettled {
        seq: u64,
        task_id: crate::agent::workflow::TaskId,
        selected: crate::agent::workflow::AttemptKey,
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
            | Event::Signal { seq, .. }
            | Event::WorkflowStarted { seq, .. }
            | Event::DispatchIntent { seq, .. }
            | Event::AttemptCompleted { seq, .. }
            | Event::SpeculativeCohortCaptured { seq, .. }
            | Event::SpeculativeCohortIntegrated { seq, .. }
            | Event::TaskSettled { seq, .. } => *seq,
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
    _lease: crate::core::repo_lock::RepoTxnLock,
    file: fs::File,
    next_seq: u64,
    path: PathBuf,
    poisoned: bool,
}

/// Where run logs live: `<scratch>/runs/`. `AIZEN_RUNS_DIR` overrides
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
        let _lease = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
            &runs_dir.join(format!("{run_id}.writer.lock")),
            std::time::Duration::ZERO,
        )
        .context("run log already has a writer or its ownership lock is unavailable")?;
        let path = runs_dir.join(format!("{run_id}.jsonl"));
        let (events, valid_end, separator) = scan_events(&path)?;
        let next_seq = events.last().map(|e| e.seq() + 1).unwrap_or(0);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening run log {}", path.display()))?;
        // Repair only the inspected torn tail. Appending after it would turn a tolerated final
        // fragment into a corrupt middle event on the next resume.
        if file.metadata()?.len() != valid_end {
            file.set_len(valid_end)
                .context("truncating inspected torn run-log tail")?;
        }
        // FILE_APPEND_DATA alone cannot truncate on Windows. The exclusive writer lease makes
        // a read/write handle positioned at EOF safe for both inspected repair and appends.
        file.seek(SeekFrom::End(0))?;
        if separator {
            file.write_all(b"\n")?;
        }
        Ok(Self {
            _lease,
            file,
            next_seq,
            path,
            poisoned: false,
        })
    }

    /// Append one event, assigning its sequence number. The caller passes the event with a
    /// placeholder seq (0 is fine); the writer overwrites it with the real one before serializing.
    /// Returns the assigned seq so the caller can reference it ("the tool call at seq=12").
    pub fn record(&mut self, mut event: Event) -> Result<u64> {
        if self.poisoned {
            bail!("run-log writer is poisoned after a failed append; reopen and reconcile the inspected tail");
        }
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
            | Event::Signal { seq: s, .. }
            | Event::WorkflowStarted { seq: s, .. }
            | Event::DispatchIntent { seq: s, .. }
            | Event::AttemptCompleted { seq: s, .. }
            | Event::SpeculativeCohortCaptured { seq: s, .. }
            | Event::SpeculativeCohortIntegrated { seq: s, .. }
            | Event::TaskSettled { seq: s, .. } => *s = seq,
        }
        let line = serde_json::to_string(&event).context("serializing run-log event")?;
        self.poisoned = true;
        self.file
            .write_all(line.as_bytes())
            .and_then(|_| self.file.write_all(b"\n"))
            .and_then(|_| self.file.flush())
            .with_context(|| format!("appending to {}", self.path.display()))?;
        self.next_seq += 1;
        self.poisoned = false;
        Ok(seq)
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read valid events, dropping only a trailing partial JSON line. An incomplete intent/completion
/// is not proof of no effects: replay must still reconcile unknown workers. A malformed middle
/// line is an error; continuing a corrupted journal is worse than refusing.
pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    scan_events(path).map(|(events, _, _)| events)
}

fn scan_events(path: &Path) -> Result<(Vec<Event>, u64, bool)> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0, false)),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut offset = 0u64;
    let mut line_no = 0;
    let mut separator = false;
    loop {
        let mut line = Vec::new();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        line_no += 1;
        let end = offset + count as u64;
        let parsed = std::str::from_utf8(&line).ok().map(str::trim);
        if parsed.is_some_and(str::is_empty) {
            offset = end;
            separator = !line.ends_with(b"\n");
            continue;
        }
        match parsed.and_then(|s| serde_json::from_str::<Event>(s).ok()) {
            Some(event) => {
                if event.seq() != events.len() as u64 {
                    bail!("non-dense run-log sequence at {}:{line_no}", path.display());
                }
                events.push(event);
                offset = end;
                separator = !line.ends_with(b"\n");
            }
            None if end == len => return Ok((events, offset, separator)),
            None => bail!("malformed run-log event at {}:{line_no}", path.display()),
        }
    }
    Ok((events, offset, separator))
}

/// Sequence numbers of observed tool failures, for diagnostics only. Missing completions and
/// authority to continue are handled by the workflow plan, never inferred from this list.
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
    /// Preserve an admitted but uncompleted attempt, or a legacy identity ambiguity.
    Reconcile { reason: String },
    /// A completed physical attempt still has a queued logical continuation.
    Continue,
    Skip {
        status: String,
        summary: String,
        iters: usize,
        tokens_in: u64,
        tokens_out: u64,
    },
}

/// Only a completed, contract-valid outcome is skippable. Refresh and all other incomplete stops
/// require reconciliation or explicitly authorized continuation, never a successful skip.
fn task_outcome_skippable(status: &str) -> bool {
    matches!(
        status.split('·').next().map(str::trim),
        Some("done" | "ok" | "json:ok")
    ) && !status.contains("json:invalid")
}

/// `<id>#2` → `<id>`: a fix-loop retry supersedes its base attempt (mirrors `workflow::base_id`,
/// duplicated here so `plan` does not depend on the orchestrator module).
fn plan_base_id(id: &str) -> &str {
    match id.rsplit_once('#') {
        Some((base, n)) if !base.is_empty() && (n.parse::<usize>().is_ok() || n == "restart") => {
            base
        }
        _ => id,
    }
}

pub(crate) const WORKFLOW_VERSION: u32 = 2;

/// Validate the journal envelope before deriving routing or deciding that a run is already done.
pub(crate) fn workflow_header<'a>(
    events: &'a [Event],
    task_ids: &[&str],
) -> Result<Option<&'a str>> {
    let headers: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::WorkflowStarted { .. }))
        .collect();
    if headers.is_empty() {
        if events.iter().any(|e| {
            matches!(
                e,
                Event::DispatchIntent { .. }
                    | Event::AttemptCompleted { .. }
                    | Event::SpeculativeCohortCaptured { .. }
                    | Event::SpeculativeCohortIntegrated { .. }
                    | Event::TaskSettled { .. }
            )
        }) {
            bail!("versioned workflow events have no header; reconciliation required");
        }
        return Ok(None);
    }
    if headers.len() != 1 || headers[0].0 != 0 {
        bail!("workflow journal must have exactly one leading header");
    }
    let Event::WorkflowStarted {
        version,
        task_ids: prior,
        dispatch_date,
        metis,
        ..
    } = headers[0].1
    else {
        unreachable!()
    };
    if *version != WORKFLOW_VERSION || !metis {
        bail!("unsupported workflow journal version/policy; reconciliation required");
    }
    if prior.iter().map(|id| id.0.as_str()).collect::<Vec<_>>() != task_ids {
        bail!("workflow journal literal task IDs/order differ from the spec");
    }
    let mut seen = std::collections::HashSet::new();
    if prior.is_empty()
        || prior
            .iter()
            .any(|id| id.0.trim().is_empty() || !seen.insert(id.0.as_str()))
    {
        bail!("workflow journal has empty or duplicate literal task IDs");
    }
    let date = dispatch_date
        .as_deref()
        .context("workflow journal has no original dispatch date")?;
    let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .context("invalid original dispatch date")?;
    if parsed.format("%Y-%m-%d").to_string() != date {
        bail!("non-canonical original dispatch date");
    }
    Ok(Some(date))
}

/// Cohort records are a fail-closed replay barrier. A captured base cannot be reopened from
/// the log, and even completed private attempts are not live effects until parent verification.
pub(crate) fn validate_speculative_cohorts(
    events: &[Event],
) -> Result<std::collections::HashSet<crate::agent::workflow::AttemptKey>> {
    use crate::agent::speculation::ExecutionMode;
    use crate::agent::workflow::{AttemptKey, AttemptKind};
    use std::collections::{HashMap, HashSet};
    let mut captured = HashSet::new();
    let mut membership = HashMap::<AttemptKey, String>::new();
    let mut integrated = HashSet::new();
    let mut dispatched = HashSet::new();
    let mut completed = HashSet::new();
    let mut open: Option<(String, Vec<AttemptKey>)> = None;
    for event in events {
        match event {
            Event::SpeculativeCohortCaptured { cohort_id, members, .. } => {
                anyhow::ensure!(open.is_none() && !cohort_id.is_empty() && cohort_id.len() <= 128
                    && captured.insert(cohort_id.clone()) && !members.is_empty() && members.len() <= 32,
                    "invalid or overlapping speculative cohort capture; reconciliation required");
                for key in members {
                    anyhow::ensure!(key.kind == AttemptKind::Initial && key.ordinal == 1
                        && membership.insert(key.clone(), cohort_id.clone()).is_none(),
                        "speculative membership is duplicated or an automatic retry");
                }
                open = Some((cohort_id.clone(), members.clone()));
            }
            Event::DispatchIntent { intent, .. } => {
                let speculative = intent.task.execution == ExecutionMode::Speculative;
                anyhow::ensure!(speculative == membership.contains_key(&intent.key),
                    "speculative dispatch has no captured cohort, or direct work names private membership");
                if let Some((_, members)) = &open {
                    anyhow::ensure!(members.contains(&intent.key), "direct or foreign dispatch entered an active speculative cohort");
                }
                if speculative {
                    anyhow::ensure!(!integrated.contains(&intent.key) && dispatched.insert(intent.key.clone()),
                        "speculative attempt dispatched twice or after integration");
                }
            }
            Event::AttemptCompleted { key, status, summary, .. } if membership.contains_key(key) => {
                anyhow::ensure!(dispatched.contains(key) && !integrated.contains(key)
                    && completed.insert(key.clone()) && crate::agent::workflow::completed_status(status),
                    "speculative attempt failed, incomplete or not uniquely dispatched; reconciliation required");
                let report = crate::agent::workflow::TaskOutcome {
                    id: key.display(), attempt: Some(key.clone()), selected_for: None,
                    role: String::new(), model: String::new(), status: status.clone(), summary: summary.clone(),
                    iters: 0, tokens_in: 0, tokens_out: 0, evidence_images: Vec::new(),
                };
                anyhow::ensure!(!crate::agent::workflow::verdict_failed(&report),
                    "speculative attempt reported FAIL; integration cannot authorize replay");
            }
            Event::SpeculativeCohortIntegrated { cohort_id, members, receipt, .. } => {
                let (expected_id, expected_members) = open.as_ref().context("integration without captured cohort")?;
                anyhow::ensure!(cohort_id == expected_id && members == expected_members
                    && members.iter().all(|key| completed.contains(key)),
                    "integration receipt does not cover the complete successful frozen cohort");
                // This proves a supported lifecycle summary, not current source truth or renewed
                // execution authority. Legacy/arbitrary strings cannot release dependencies.
                crate::core::speculation::verification::parse_success_summary(receipt, cohort_id)
                    .context("speculative integration lifecycle evidence is unsupported; reconciliation required")?;
                integrated.extend(members.iter().cloned());
                open = None;
            }
            Event::TaskSettled { selected, .. } if membership.contains_key(selected) => {
                anyhow::ensure!(integrated.contains(selected), "speculative settlement precedes accepted integration");
            }
            _ => {}
        }
    }
    anyhow::ensure!(open.is_none(), "captured speculative cohort has no accepted integration: inspect retained private overlays/integration receipts; never automatically reapply or redispatch");
    Ok(integrated)
}

/// Plan a resume using literal IDs and selected logical outcomes. Legacy display aliases are
/// interpreted only where ownership is unambiguous; failures never authorize repeating a write.
pub fn plan(events: &[Event], task_ids: &[&str]) -> Vec<Action> {
    if let Err(error) = workflow_header(events, task_ids)
        .and_then(|_| validate_speculative_cohorts(events).map(|_| ())) {
        return task_ids
            .iter()
            .map(|_| Action::Reconcile {
                reason: error.to_string(),
            })
            .collect();
    }
    use crate::agent::workflow::{AttemptKey, TaskId};
    let mut latest: std::collections::HashMap<&str, &Event> = std::collections::HashMap::new();
    let mut ambiguous = std::collections::HashSet::new();
    let mut intents = std::collections::HashSet::<AttemptKey>::new();
    let mut completed = std::collections::HashMap::<AttemptKey, &Event>::new();
    let mut settled = std::collections::HashMap::<TaskId, AttemptKey>::new();
    let versioned = events
        .iter()
        .any(|e| matches!(e, Event::WorkflowStarted { .. }));
    for e in events {
        match e {
            Event::Task { task_id, .. } => {
                let base = plan_base_id(task_id);
                if !versioned
                    && base != task_id
                    && task_ids.contains(&base)
                    && task_ids.contains(&task_id.as_str())
                {
                    // Old display labels cannot establish ownership when both names are valid IDs.
                    ambiguous.insert(base);
                    ambiguous.insert(task_id.as_str());
                }
                if task_ids.contains(&task_id.as_str()) {
                    latest.insert(task_id, e);
                } else {
                    let base = plan_base_id(task_id);
                    if base != task_id && task_ids.contains(&base) {
                        // Legacy suffixes are accepted only when the exact base was recorded and
                        // the spec does not contain this literal ID. Otherwise ownership is unknown.
                        if latest.contains_key(base) {
                            latest.insert(base, e);
                        } else {
                            ambiguous.insert(base);
                        }
                    }
                }
            }
            Event::DispatchIntent { intent, .. } => {
                intents.insert(intent.key.clone());
            }
            Event::AttemptCompleted { key, .. } => {
                completed.insert(key.clone(), e);
            }
            Event::TaskSettled {
                task_id, selected, ..
            } => {
                settled.insert(task_id.clone(), selected.clone());
            }
            _ => {}
        }
    }
    task_ids.iter().map(|id| {
        if let Some(key) = intents.iter().find(|key| key.task_id.0 == *id && !completed.contains_key(*key)) {
            return Action::Reconcile { reason: format!("unknown outcome: {} was admitted without a completion; inspect crash recovery/checkpoints before repeating", key.display()) };
        }
        if completed.iter().any(|(key, event)| key.task_id.0 == *id
            && matches!(event, Event::AttemptCompleted { status, .. } if status == "unknown-worker")) {
            return Action::Reconcile { reason: "attempt still had a live blocking worker; inspect eventual effects before new work".into() };
        }
        if ambiguous.contains(id) {
            return Action::Reconcile { reason: "legacy suffix alias is ambiguous; exact IDs are not proof of retry ownership".into() };
        }
        let event = if versioned {
            let Some(key) = settled.get(&TaskId((*id).to_string())) else {
                return if intents.iter().any(|key| key.task_id.0 == *id) { Action::Continue } else { Action::Run };
            };
            match completed.get(key) {
                Some(event) => Some(*event),
                None => return Action::Reconcile { reason: "settlement has no recorded completion".into() },
            }
        } else { latest.get(id).copied() };
        match event {
            Some(Event::Task { status, summary, iters, tokens_in, tokens_out, .. }
                | Event::AttemptCompleted { status, summary, iters, tokens_in, tokens_out, .. })
                if task_outcome_skippable(status) => Action::Skip { status: status.clone(),
                    summary: summary.clone(), iters: *iters, tokens_in: *tokens_in, tokens_out: *tokens_out },
            Some(_) if versioned => Action::Reconcile { reason: "logical task settled incomplete/failed; resume cannot label it done or repeat its side effects".into() },
            _ => Action::Run,
        }
    }).collect()
}

/// One line per event, for run-log diagnostic assertions.
#[cfg(test)]
pub fn describe(e: &Event) -> String {
    match e {
        Event::WorkflowStarted {
            seq,
            version,
            task_ids,
            ..
        } => format!(
            "#{seq:>4} workflow v{version} · {} literal tasks",
            task_ids.len()
        ),
        Event::DispatchIntent { seq, intent } => {
            format!("#{seq:>4} dispatch {}", intent.key.display())
        }
        Event::AttemptCompleted {
            seq,
            key,
            status,
            tokens_in,
            tokens_out,
            ..
        } => format!(
            "#{seq:>4} completed {} → {status} [{tokens_in}+{tokens_out} tok]",
            key.display()
        ),
        Event::SpeculativeCohortCaptured { seq, cohort_id, members } => {
            format!("#{seq:>4} speculative {cohort_id} · {} frozen attempts", members.len())
        }
        Event::SpeculativeCohortIntegrated { seq, cohort_id, .. } => {
            format!("#{seq:>4} speculative {cohort_id} · integrated and verified")
        }
        Event::TaskSettled {
            seq,
            task_id,
            selected,
        } => format!("#{seq:>4} settled {} → {}", task_id.0, selected.display()),
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

    use crate::core::file_placement::tests::TestDir;

    fn header(ids: &[&str]) -> Event {
        Event::WorkflowStarted {
            seq: 0,
            version: WORKFLOW_VERSION,
            fingerprint: 42,
            dispatch_date: Some("2026-10-11".into()),
            task_ids: ids
                .iter()
                .map(|id| crate::agent::workflow::TaskId((*id).into()))
                .collect(),
            metis: true,
        }
    }

    #[test]
    fn workflow_header_checks_version_order_unique_ids_and_original_date() {
        let ids = ["a", "a#2", "a#restart"];
        let valid = vec![header(&ids)];
        assert_eq!(workflow_header(&valid, &ids).unwrap(), Some("2026-10-11"));
        assert!(workflow_header(&valid, &["a#2", "a", "a#restart"]).is_err());
        for date in [
            None,
            Some("2026-02-30"),
            Some("2026-1-1"),
            Some("not a date"),
        ] {
            let mut invalid = valid.clone();
            if let Event::WorkflowStarted { dispatch_date, .. } = &mut invalid[0] {
                *dispatch_date = date.map(str::to_string);
            }
            assert!(workflow_header(&invalid, &ids).is_err(), "{date:?}");
        }
        for version in [1, WORKFLOW_VERSION + 1] {
            let mut invalid = valid.clone();
            if let Event::WorkflowStarted { version: v, .. } = &mut invalid[0] {
                *v = version;
            }
            assert!(plan(&invalid, &ids)
                .iter()
                .all(|a| matches!(a, Action::Reconcile { .. })));
        }
        let mut invalid = valid.clone();
        if let Event::WorkflowStarted { metis, .. } = &mut invalid[0] {
            *metis = false;
        }
        assert!(workflow_header(&invalid, &ids).is_err());
        for bad in [vec![], vec![" "], vec!["a", "a"]] {
            assert!(workflow_header(&[header(&bad)], &bad).is_err());
        }
        let note = Event::Note {
            seq: 0,
            label: "before".into(),
            body: String::new(),
        };
        assert!(workflow_header(&[note, header(&ids)], &ids).is_err());
        assert!(workflow_header(&[header(&ids), header(&ids)], &ids).is_err());
        assert_eq!(workflow_header(&[], &ids).unwrap(), None);
    }

    #[test]
    fn live_worker_completion_and_missing_header_require_reconciliation() {
        use crate::agent::workflow::{AttemptKey, AttemptKind, TaskId};
        let completion = Event::AttemptCompleted {
            seq: 1,
            key: AttemptKey {
                task_id: TaskId("a".into()),
                ordinal: 1,
                kind: AttemptKind::Initial,
            },
            role: "argus".into(),
            model: "m".into(),
            status: "unknown-worker".into(),
            summary: "observer returned while physical worker still owns attempt".into(),
            iters: 1,
            tokens_in: 11,
            tokens_out: 3,
        };
        assert!(workflow_header(std::slice::from_ref(&completion), &["a"]).is_err());
        assert!(
            matches!(plan(&[header(&["a"]), completion], &["a"]).as_slice(),
            [Action::Reconcile { reason }] if reason.contains("live blocking worker"))
        );
    }

    #[test]
    fn failed_append_poison_prevents_blind_second_write() {
        let dir = TestDir::new();
        let mut writer = RunLogWriter::open(dir.path(), "poison").unwrap();
        // Inject a deterministic write failure without modifying filesystem permissions.
        writer.file = fs::File::open(writer.path()).unwrap();
        let note = Event::Note {
            seq: 0,
            label: "a".into(),
            body: String::new(),
        };
        assert!(writer.record(note.clone()).is_err());
        assert_eq!(writer.next_seq, 0);
        assert!(writer
            .record(note)
            .unwrap_err()
            .to_string()
            .contains("poisoned"));
        assert!(read_events(writer.path()).unwrap().is_empty());
    }

    #[test]
    fn writes_and_reads_back_dense_sequence() {
        let dir = TestDir::new();
        let mut w = RunLogWriter::open(dir.path(), "run-1").unwrap();
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
        let dir = TestDir::new();
        {
            let mut w = RunLogWriter::open(dir.path(), "run-2").unwrap();
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
        let mut w = RunLogWriter::open(dir.path(), "run-2").unwrap();
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
        let dir = TestDir::new();
        let path = {
            let mut w = RunLogWriter::open(dir.path(), "run-3").unwrap();
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
        let mut w = RunLogWriter::open(dir.path(), "run-3").unwrap();
        assert_eq!(
            w.record(Event::Note {
                seq: 0,
                label: "c".into(),
                body: String::new()
            })
            .unwrap(),
            2
        );
        assert_eq!(read_events(&path).unwrap().len(), 3);
        drop(w);
        let mut w = RunLogWriter::open(dir.path(), "run-3").unwrap();
        assert_eq!(
            w.record(Event::Note {
                seq: 0,
                label: "d".into(),
                body: String::new()
            })
            .unwrap(),
            3
        );
        assert_eq!(read_events(&path).unwrap().len(), 4);
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
        let dir = TestDir::new();
        let path = dir.path().join("bad.jsonl");
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
    fn legacy_literal_suffix_collisions_require_reconciliation() {
        for suffix in ["#2", "#restart"] {
            let events = vec![
                Event::Task {
                    seq: 0,
                    task_id: "a".into(),
                    status: "done".into(),
                    summary: "base".into(),
                    iters: 1,
                    tokens_in: 11,
                    tokens_out: 3,
                },
                Event::Task {
                    seq: 1,
                    task_id: format!("a{suffix}"),
                    status: "done".into(),
                    summary: "unknown ownership".into(),
                    iters: 1,
                    tokens_in: 11,
                    tokens_out: 3,
                },
            ];
            let literal = format!("a{suffix}");
            assert!(plan(&events, &["a", &literal])
                .iter()
                .all(|action| matches!(action, Action::Reconcile { .. })));
            assert!(
                matches!(plan(&events, &["a"]).as_slice(), [Action::Skip { summary, .. }] if summary == "unknown ownership")
            );
        }
    }

    #[test]
    fn writer_ownership_and_complete_no_newline_tail_are_preserved() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let mut writer = RunLogWriter::open(dir.path(), "owned").unwrap();
        writer
            .record(Event::Note {
                seq: 0,
                label: "a".into(),
                body: String::new(),
            })
            .unwrap();
        assert!(RunLogWriter::open(dir.path(), "owned").is_err());
        let path = writer.path().to_path_buf();
        drop(writer);
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&path, bytes).unwrap();
        let mut writer = RunLogWriter::open(dir.path(), "owned").unwrap();
        assert_eq!(
            writer
                .record(Event::Note {
                    seq: 0,
                    label: "b".into(),
                    body: String::new()
                })
                .unwrap(),
            1
        );
        assert_eq!(read_events(&path).unwrap().len(), 2);
    }

    #[test]
    fn task_event_roundtrips_through_jsonl() {
        let dir = TestDir::new();
        let mut w = RunLogWriter::open(dir.path(), "run-t").unwrap();
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
