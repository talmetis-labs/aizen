//! Literal task identities and one queued admission lane for every physical attempt.

#[cfg(test)]
#[path = "workflow_scheduler_tests.rs"]
mod tests;

use super::{
    completed_status, verdict_failed, with_fix_request, with_upstream, TaskOutcome, WorkflowTask,
};
use crate::agent::runlog::Event;
use crate::agent::task_tool::SlotDenied;
use crate::agent::speculation::ExecutionMode;
use crate::core::cancel::TurnCancel;
use anyhow::{bail, Context, Result};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TaskId(pub String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttemptKind {
    Initial,
    Restart,
    Fix,
    Recheck,
    Metis,
    Replacement,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) struct AttemptKey {
    pub task_id: TaskId,
    pub ordinal: u32,
    pub kind: AttemptKind,
}

impl AttemptKey {
    pub fn display(&self) -> String {
        if self.kind == AttemptKind::Initial {
            return self.task_id.0.clone();
        }
        format!("{} [{:?} #{}]", self.task_id.0, self.kind, self.ordinal)
    }
}

/// Contains no endpoint secret or model-supplied authorization. Capabilities are reattached only
/// from the fingerprint-checked prepared spec; prompts and exact upstream keys are replay payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DispatchIntent {
    pub key: AttemptKey,
    pub goal: TaskId,
    pub task: WorkflowTask,
    pub inputs: Vec<AttemptKey>,
    pub stage: AttemptKind,
    pub trigger: Option<AttemptKey>,
    /// Controller-stamped wall deadline, persisted so recovery cannot buy another execution budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    /// Monotonic while this controller is alive; rebuilt once from the journal on resume.
    #[serde(skip)]
    pub live_deadline: Option<std::time::Instant>,
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

impl DispatchIntent {
    pub(crate) fn deadline(&self) -> Option<std::time::Instant> {
        self.live_deadline.or_else(|| self.deadline_unix_ms.map(|d| {
            let now = std::time::Instant::now();
            // Invalid journal timestamps fail closed instead of overflowing Instant.
            now.checked_add(Duration::from_millis(d.saturating_sub(unix_ms())))
                .unwrap_or(now)
        }))
    }
}

struct Pending {
    intent: DispatchIntent,
    due: Instant,
    admission_deadline: Option<Instant>,
}

#[derive(Clone, Copy)]
pub(crate) struct Policy {
    pub metis: bool,
    pub parent_deadline: Option<std::time::Instant>,
    pub admission_timeout: Duration,
    pub restart_backoff: Duration,
    pub probe_interval: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            metis: false,
            parent_deadline: None,
            admission_timeout: Duration::from_secs(60),
            restart_backoff: Duration::from_millis(500),
            probe_interval: Duration::from_millis(100),
        }
    }
}

struct State<'a> {
    tasks: &'a [WorkflowTask],
    policy: Policy,
    queue: VecDeque<Pending>,
    intents: HashMap<AttemptKey, DispatchIntent>,
    completed: HashSet<AttemptKey>,
    settled: HashMap<TaskId, AttemptKey>,
    settlements: VecDeque<(TaskId, AttemptKey)>,
    ordinal: HashMap<TaskId, u32>,
    fixed: HashSet<TaskId>,
    replanned: HashSet<TaskId>,
    results: Vec<TaskOutcome>,
    deadlines: HashMap<TaskId, Option<u64>>,
    live_deadlines: HashMap<TaskId, Option<std::time::Instant>>,
}

impl<'a> State<'a> {
    fn new(tasks: &'a [WorkflowTask], policy: Policy) -> Self {
        let mut state = Self {
            tasks,
            policy,
            queue: VecDeque::new(),
            intents: HashMap::new(),
            completed: HashSet::new(),
            settled: HashMap::new(),
            settlements: VecDeque::new(),
            ordinal: HashMap::new(),
            fixed: HashSet::new(),
            replanned: HashSet::new(),
            results: Vec::new(),
            deadlines: HashMap::new(),
            live_deadlines: HashMap::new(),
        };
        for task in tasks {
            state.enqueue(
                task.clone(),
                TaskId(task.id.clone()),
                AttemptKind::Initial,
                AttemptKind::Initial,
                None,
                Vec::new(),
                Duration::ZERO,
            );
        }
        state
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Each argument is an explicit attempt transition field; keep scheduler admission structure unchanged"
    )]
    fn enqueue(
        &mut self,
        task: WorkflowTask,
        goal: TaskId,
        kind: AttemptKind,
        stage: AttemptKind,
        trigger: Option<AttemptKey>,
        inputs: Vec<AttemptKey>,
        delay: Duration,
    ) {
        let task_id = TaskId(task.id.clone());
        let ordinal = self.ordinal.entry(task_id.clone()).or_default();
        *ordinal += 1;
        let key = AttemptKey {
            task_id,
            ordinal: *ordinal,
            kind,
        };
        let deadline_unix_ms = self.deadlines.get(&goal).copied().flatten();
        let live_deadline = self.live_deadlines.get(&goal).copied().flatten();
        let intent = DispatchIntent {
            key,
            goal,
            task,
            inputs,
            stage,
            trigger,
            deadline_unix_ms,
            live_deadline,
        };
        let due = Instant::now() + delay;
        self.queue.push_back(Pending {
            intent,
            due,
            admission_deadline: None,
        });
    }

    fn freeze_deadline(&mut self, intent: &mut DispatchIntent) {
        let live = *self.live_deadlines.entry(intent.goal.clone()).or_insert_with(|| {
            let local = crate::agent::task_tool::subagent_wall_deadline();
            match (local, self.policy.parent_deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        });
        intent.deadline_unix_ms = *self.deadlines.entry(intent.goal.clone()).or_insert_with(|| {
            live.map(|d| unix_ms().saturating_add(
                d.saturating_duration_since(std::time::Instant::now()).as_millis()
                    .min(u64::MAX as u128) as u64,
            ))
        });
        intent.live_deadline = live;
    }

    fn original(&self, id: &TaskId) -> Result<&WorkflowTask> {
        self.tasks
            .iter()
            .find(|task| task.id == id.0)
            .context("attempt names an unknown literal task")
    }

    fn published(&self) -> HashMap<String, usize> {
        self.settled
            .iter()
            .filter_map(|(id, key)| {
                self.results
                    .iter()
                    .position(|r| r.attempt.as_ref() == Some(key))
                    .map(|index| (id.0.clone(), index))
            })
            .collect()
    }

    fn inputs(&self, task: &WorkflowTask) -> Vec<AttemptKey> {
        task.after
            .iter()
            .filter_map(|id| self.settled.get(&TaskId(id.trim().to_string())))
            .cloned()
            .collect()
    }

    fn materialize(&self, intent: &mut DispatchIntent) -> Result<()> {
        if intent.key.kind == AttemptKind::Initial {
            let original = self.original(&intent.key.task_id)?;
            intent.inputs = self.inputs(original);
            intent.task = with_upstream(original, &self.results, &self.published(), None);
        }
        Ok(())
    }

    fn eligible(&self, pending: &Pending) -> bool {
        pending.intent.key.kind != AttemptKind::Initial
            || pending
                .intent
                .task
                .after
                .iter()
                .all(|dep| self.settled.contains_key(&TaskId(dep.trim().to_string())))
    }

    fn complete(&mut self, intent: DispatchIntent, mut outcome: TaskOutcome) -> Result<()> {
        if !self.completed.insert(intent.key.clone()) {
            bail!("duplicate attempt completion");
        }
        outcome.id = intent.key.display();
        outcome.selected_for = None;
        outcome.attempt = Some(intent.key.clone());
        let mut recorded = intent.clone();
        // Completed graph metadata must not retain executable overlay/base ownership. The
        // active cohort alone keeps its selected handles until parent verification settles.
        recorded.task.overlay = None;
        self.intents.insert(intent.key.clone(), recorded);
        self.results.push(outcome);
        let idx = self.results.len() - 1;
        let outcome = &self.results[idx];
        if intent.task.execution == ExecutionMode::Speculative {
            // Private attempts have exactly one dispatch. A failed/partial overlay must never
            // become a fresh live retry, fix or planner replacement behind the parent's back.
            self.settlements.push_back((intent.goal, intent.key));
            return Ok(());
        }
        let primary = outcome.status.split('·').next().map(str::trim);
        if matches!(primary, Some("deadline" | "retry-exhausted" | "protocol-error"
            | "authentication-error" | "permanent-error" | "cancelled"
            | "unknown-worker" | "possibly-applied")) {
            self.settlements.push_back((intent.goal, intent.key));
            return Ok(());
        }
        let transient = primary == Some("error");
        if transient
            && intent.key.kind != AttemptKind::Restart
            && intent.stage != AttemptKind::Replacement
        {
            let mut inputs = intent.inputs.clone();
            inputs.push(intent.key.clone());
            self.enqueue(
                intent.task.clone(),
                intent.goal.clone(),
                AttemptKind::Restart,
                intent.stage,
                intent.trigger.clone(),
                inputs,
                self.policy.restart_backoff,
            );
            return Ok(());
        }
        if intent.stage == AttemptKind::Metis {
            if completed_status(&outcome.status) && !outcome.summary.trim().is_empty() {
                let mut replacement = self.original(&intent.goal)?.clone();
                replacement.prompt = outcome.summary.clone();
                self.enqueue(
                    replacement,
                    intent.goal.clone(),
                    AttemptKind::Replacement,
                    AttemptKind::Replacement,
                    Some(intent.key.clone()),
                    vec![intent.key],
                    Duration::ZERO,
                );
            } else {
                // Planner output is not the selected result of the failed original goal.
                let failed = intent.trigger.context("planner missing failed attempt")?;
                self.settlements.push_back((intent.goal, failed));
            }
            return Ok(());
        }
        if intent.stage == AttemptKind::Fix {
            self.settlements
                .push_back((intent.key.task_id.clone(), intent.key.clone()));
            let verifier_key = intent.trigger.context("fix missing verifier attempt")?;
            let mut verifier = self.original(&intent.goal)?.clone();
            let mut published = self.published();
            published.insert(intent.key.task_id.0.clone(), idx);
            let inputs = verifier
                .after
                .iter()
                .filter_map(|id| published.get(id.trim()))
                .filter_map(|&index| self.results[index].attempt.clone())
                .collect();
            verifier = with_upstream(&verifier, &self.results, &published, None);
            self.enqueue(
                verifier,
                intent.goal,
                AttemptKind::Recheck,
                AttemptKind::Recheck,
                Some(verifier_key),
                inputs,
                Duration::ZERO,
            );
            return Ok(());
        }
        if transient
            && self.policy.metis
            && intent.stage != AttemptKind::Replacement
            && self.replanned.insert(intent.goal.clone())
        {
            let original = self.original(&intent.goal)?;
            let prompt = format!(
                "A workflow task failed after its one restart. Write a REPLACEMENT \
                task prompt achieving the SAME goal by a different approach. Keep all boundaries; \
                do not expand authorization. Reply ONLY with the replacement prompt.\n\n\
                Original prompt: {}\nBoundaries: {}\nFailure report:\n{}",
                original.prompt,
                original.boundaries.as_deref().unwrap_or("(unchanged)"),
                super::clip_chars(&outcome.summary, super::FIX_REQUEST_CHARS)
            );
            let planner = WorkflowTask {
                id: original.id.clone(),
                role: "metis".into(),
                prompt,
                prepared: original.prepared_metis.clone(),
                ..Default::default()
            };
            self.enqueue(
                planner,
                intent.goal,
                AttemptKind::Metis,
                AttemptKind::Metis,
                Some(intent.key.clone()),
                vec![intent.key],
                Duration::ZERO,
            );
            return Ok(());
        }
        if completed_status(&outcome.status)
            && verdict_failed(outcome)
            && !self.fixed.contains(&intent.goal)
        {
            if let Some(target) = self.original(&intent.goal)?.retry_on_fail.as_deref() {
                let target = self.original(&TaskId(target.trim().to_string()))?;
                if target.execution == ExecutionMode::Speculative {
                    // A verifier can report failure, but only a new explicit parent dispatch
                    // may choose another private attempt; the historical fix loop is direct-only.
                    self.settlements.push_back((intent.goal, intent.key));
                    return Ok(());
                }
                let inputs = self.inputs(target);
                let fix = with_fix_request(
                    target,
                    &outcome.summary,
                    2,
                    &self.results,
                    &self.published(),
                );
                self.fixed.insert(intent.goal.clone());
                self.enqueue(
                    fix,
                    intent.goal,
                    AttemptKind::Fix,
                    AttemptKind::Fix,
                    Some(intent.key),
                    inputs,
                    Duration::ZERO,
                );
                return Ok(());
            }
        }
        self.settlements.push_back((intent.goal, intent.key));
        Ok(())
    }

    fn settle(&mut self, goal: TaskId, selected: AttemptKey) -> Result<()> {
        if !self.completed.contains(&selected) {
            bail!("settlement without attempt completion");
        }
        for outcome in &mut self.results {
            if outcome.selected_for.as_ref() == Some(&goal) {
                outcome.selected_for = None;
            }
            if outcome.attempt.as_ref() == Some(&selected) {
                outcome.selected_for = Some(goal.clone());
            }
        }
        self.settlements
            .retain(|(g, k)| g != &goal || k != &selected);
        self.settled.insert(goal, selected);
        Ok(())
    }

    fn replay(&mut self, events: &[Event]) -> Result<()> {
        crate::agent::runlog::validate_speculative_cohorts(events)?;
        for event in events {
            match event {
                Event::DispatchIntent { intent, .. } => {
                    if self.intents.contains_key(&intent.key) {
                        bail!(
                            "duplicate durable dispatch intent: {}",
                            intent.key.display()
                        );
                    }
                    let pending = self
                        .queue
                        .iter()
                        .find(|p| p.intent.key == intent.key)
                        .context(
                            "durable attempt is not a deterministic continuation of this spec",
                        )?;
                    if !self.eligible(pending) {
                        bail!("durable dispatch precedes dependency settlement");
                    }
                    let mut expected = pending.intent.clone();
                    self.materialize(&mut expected)?;
                    if let Some(previous) = self.deadlines.get(&intent.goal) {
                        if *previous != intent.deadline_unix_ms {
                            bail!("durable attempt changed its logical execution deadline");
                        }
                    } else {
                        self.deadlines.insert(intent.goal.clone(), intent.deadline_unix_ms);
                        let mut restored = intent.clone();
                        restored.live_deadline = None;
                        let live = match (restored.deadline(), self.policy.parent_deadline) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                        self.live_deadlines.insert(intent.goal.clone(), live);
                    }
                    expected.deadline_unix_ms = intent.deadline_unix_ms;
                    expected.live_deadline = self.live_deadlines.get(&intent.goal).copied().flatten();
                    if serde_json::to_value(&expected)? != serde_json::to_value(intent)? {
                        bail!("durable attempt payload/transition mismatch");
                    }
                    self.intents.insert(intent.key.clone(), expected);
                }
                Event::AttemptCompleted {
                    key,
                    role,
                    model,
                    status,
                    summary,
                    iters,
                    tokens_in,
                    tokens_out,
                    ..
                } => {
                    if status == "unknown-worker" {
                        bail!("durable attempt still had a live worker; reconciliation required, never replay its effects");
                    }
                    if status == "possibly-applied" {
                        bail!("durable attempt had possibly-applied effects; reconciliation required, never replay its effects");
                    }
                    let intent = self
                        .intents
                        .get(key)
                        .cloned()
                        .context("completion without intent")?;
                    let pos = self
                        .queue
                        .iter()
                        .position(|p| p.intent.key == *key)
                        .context(
                            "durable attempt is not a deterministic continuation of this spec",
                        )?;
                    let pending = self.queue.remove(pos).unwrap();
                    if pending.intent.goal != intent.goal || pending.intent.stage != intent.stage {
                        bail!("durable attempt transition mismatch");
                    }
                    self.complete(
                        intent,
                        TaskOutcome {
                            id: key.display(),
                            selected_for: None,
                            attempt: Some(key.clone()),
                            role: role.clone(),
                            model: model.clone(),
                            status: status.clone(),
                            summary: summary.clone(),
                            iters: *iters,
                            tokens_in: *tokens_in,
                            tokens_out: *tokens_out,
                            evidence_images: Vec::new(),
                        },
                    )?;
                }
                Event::TaskSettled {
                    task_id, selected, ..
                } => {
                    if !self
                        .settlements
                        .iter()
                        .any(|(g, k)| g == task_id && k == selected)
                    {
                        bail!("durable settlement is not the derived next transition");
                    }
                    self.settle(task_id.clone(), selected.clone())?;
                }
                _ => {}
            }
        }
        for key in self.intents.keys() {
            if !self.completed.contains(key) {
                bail!("unknown outcome for admitted attempt {}: dispatch intent has no completion. \
                    Do not repeat a possible write. Inspect `aizen time doctor` and the crash-recovery \
                    journal/checkpoints, reconcile the workspace, then start explicitly authorized new work",
                    key.display());
            }
        }
        // A legacy/unbounded journal proves what completed, not how much execution time remains.
        // Read settled history, but never mint fresh retry/planner authority from an ambiguous error.
        if self.queue.iter().any(|p| p.intent.key.kind != AttemptKind::Initial
            && self.deadlines.get(&p.intent.goal).is_some_and(Option::is_none)) {
            bail!("queued recovery has no durable execution deadline; completed reports are retained, but a new explicit dispatch is required");
        }
        // Backoff/admission windows are new finite waits after resume, not refreshed execution budgets.
        for pending in &mut self.queue {
            pending.due = Instant::now();
            pending.admission_deadline = None;
        }
        Ok(())
    }
}

pub(crate) fn completion_event(intent: &DispatchIntent, outcome: &TaskOutcome) -> Event {
    Event::AttemptCompleted {
        seq: 0,
        key: intent.key.clone(),
        role: outcome.role.clone(),
        model: outcome.model.clone(),
        status: outcome.status.clone(),
        summary: outcome.summary.clone(),
        iters: outcome.iters,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
    }
}

/// A lifecycle adapter for the same admission driver, not a second scheduler. Capabilities
/// inserted by capture stay process-local; only bounded membership/evidence reaches the journal.
pub(crate) trait CohortLifecycle {
    fn capture(&self, intents: &mut [DispatchIntent]) -> Result<String>;
    fn finish<'a>(
        &'a self,
        id: &'a str,
        intents: &'a [DispatchIntent],
    ) -> futures_util::future::LocalBoxFuture<'a, Result<String>>;
    /// Publish logical success only after verified evidence is durably accepted.
    fn accepted(&self, _intents: &[DispatchIntent]) {}
    /// Drop controller authority after admitted futures drain, including on failure.
    fn release(&self) {}
}

#[cfg(test)]
struct NoCohorts;
#[cfg(test)]
impl CohortLifecycle for NoCohorts {
    fn capture(&self, _intents: &mut [DispatchIntent]) -> Result<String> {
        bail!("speculative workflow requires its controller-owned cohort lifecycle")
    }
    fn finish<'a>(&'a self, _id: &'a str, _intents: &'a [DispatchIntent])
        -> futures_util::future::LocalBoxFuture<'a, Result<String>> {
        Box::pin(async { bail!("no speculative cohort was captured") })
    }
}

struct ActiveCohort {
    id: String,
    intents: Vec<DispatchIntent>,
}

/// Only the driver awaits completion/timers. An admission refusal never occupies width or a writer
/// lane. Already-admitted futures are drained on cancel/error; their permits live inside execution.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn schedule<F, Fut, P, Admit, Prepare, Persist>(
    tasks: &[WorkflowTask],
    width: usize,
    model: &str,
    cancel: &TurnCancel,
    policy: Policy,
    replay: &[Event],
    prepare: Prepare,
    admit: Admit,
    persist: Persist,
    run: F,
) -> Result<Vec<TaskOutcome>>
where
    F: Fn(DispatchIntent, P) -> Fut,
    Fut: std::future::Future<Output = TaskOutcome>,
    Prepare: FnMut(&mut DispatchIntent) -> Result<()>,
    Admit: FnMut(&DispatchIntent) -> std::result::Result<P, SlotDenied>,
    Persist: FnMut(Event) -> Result<()>,
{
    schedule_with_admission_policy(
        tasks,
        width,
        model,
        cancel,
        policy,
        replay,
        prepare,
        admit,
        persist,
        run,
        super::SCOPED_NATIVE_PARALLEL_ENABLED,
    )
    .await
}

/// Internal explicit override exists for deterministic tests, not model-callable policy.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn schedule_with_admission_policy<F, Fut, P, Admit, Prepare, Persist>(
    tasks: &[WorkflowTask],
    width: usize,
    model: &str,
    cancel: &TurnCancel,
    policy: Policy,
    replay: &[Event],
    prepare: Prepare,
    admit: Admit,
    persist: Persist,
    run: F,
    scoped_admission: bool,
) -> Result<Vec<TaskOutcome>>
where
    F: Fn(DispatchIntent, P) -> Fut,
    Fut: std::future::Future<Output = TaskOutcome>,
    Prepare: FnMut(&mut DispatchIntent) -> Result<()>,
    Admit: FnMut(&DispatchIntent) -> std::result::Result<P, SlotDenied>,
    Persist: FnMut(Event) -> Result<()>,
{
    schedule_driver(
        tasks, width, model, cancel, policy, replay, prepare, admit, persist, run,
        scoped_admission, &NoCohorts,
    ).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn schedule_cohorts<F, Fut, P, Admit, Prepare, Persist, C>(
    tasks: &[WorkflowTask], width: usize, model: &str, cancel: &TurnCancel,
    policy: Policy, replay: &[Event], prepare: Prepare, admit: Admit, persist: Persist,
    run: F, cohorts: &C,
) -> Result<Vec<TaskOutcome>>
where
    F: Fn(DispatchIntent, P) -> Fut,
    Fut: std::future::Future<Output = TaskOutcome>,
    Prepare: FnMut(&mut DispatchIntent) -> Result<()>,
    Admit: FnMut(&DispatchIntent) -> std::result::Result<P, SlotDenied>,
    Persist: FnMut(Event) -> Result<()>,
    C: CohortLifecycle,
{
    schedule_driver(tasks, width, model, cancel, policy, replay, prepare, admit, persist,
        run, super::SCOPED_NATIVE_PARALLEL_ENABLED, cohorts).await
}

#[allow(clippy::too_many_arguments)]
async fn schedule_driver<F, Fut, P, Admit, Prepare, Persist, C>(
    tasks: &[WorkflowTask], width: usize, model: &str, cancel: &TurnCancel,
    policy: Policy, replay: &[Event], mut prepare: Prepare, mut admit: Admit,
    mut persist: Persist, run: F, scoped_admission: bool, cohorts: &C,
) -> Result<Vec<TaskOutcome>>
where
    F: Fn(DispatchIntent, P) -> Fut,
    Fut: std::future::Future<Output = TaskOutcome>,
    Prepare: FnMut(&mut DispatchIntent) -> Result<()>,
    Admit: FnMut(&DispatchIntent) -> std::result::Result<P, SlotDenied>,
    Persist: FnMut(Event) -> Result<()>,
    C: CohortLifecycle,
{
    super::waves(tasks)?;
    let mut state = State::new(tasks, policy);
    state.replay(replay)?;
    let mut cohort: Option<ActiveCohort> = None;
    let mut in_flight = FuturesUnordered::new();
    let mut active = HashMap::<AttemptKey, (super::WorkflowDispatchMode, bool)>::new();
    let mut failure: Option<anyhow::Error> = None;
    let mut unsettled_effect = None;
    // A driver failure stops admission, but a healthy journal must still retain drained outcomes.
    // After a write failure the sink may contain a torn line; do not blindly append again.
    let mut journal_failed = false;
    let mut observed_in: u64 = state.results.iter().map(|r| r.tokens_in).sum();
    let mut observed_out: u64 = state.results.iter().map(|r| r.tokens_out).sum();
    let mut observed_count = state.results.len();
    let observed_report = |outcome: &TaskOutcome| {
        format!(
            "\n── {} ({}/{}) — {} ──\n{}\n",
            outcome.id, outcome.role, outcome.model, outcome.status, outcome.summary
        )
    };
    let mut observed_reports: Vec<_> = state.results.iter().map(observed_report).collect();
    // Labelled so the expiry path can leave the admission scan and let the top of the loop drain
    // the settlement it just recorded, instead of breaking out with a settlement still pending.
    'driver: loop {
        if cancel.is_cancelled() && cohort.is_some() && failure.is_none() {
            failure = Some(anyhow::anyhow!("speculative cohort cancelled; private reports retained, no integration or automatic replay"));
        }
        let stopping = cancel.is_cancelled() || failure.is_some();
        if !stopping {
            if let Some(selected) = cohort.as_ref().filter(|selected|
                selected.intents.iter().all(|intent| state.completed.contains(&intent.key))) {
                let members = selected.intents.iter().map(|intent| intent.key.clone()).collect();
                let integrated = cohorts.finish(&selected.id, &selected.intents).await.and_then(|receipt| {
                    crate::core::speculation::verification::parse_success_summary(&receipt, &selected.id)
                        .context("speculative integration lifecycle summary invalid; reconcile retained evidence")?;
                    Ok(receipt)
                });
                match integrated {
                    Ok(receipt) => {
                        if let Err(error) = persist(Event::SpeculativeCohortIntegrated {
                            seq: 0, cohort_id: selected.id.clone(), members, receipt,
                        }) {
                            journal_failed = true;
                            failure = Some(error.context("persisting integrated cohort; dependencies not released"));
                        } else {
                            let mut accepted = selected.intents.clone();
                            for intent in &mut accepted {
                                intent.task.overlay = None;
                            }
                            // Release every controller/graph handle before publishing success or
                            // admitting direct work that may checkpoint the now-verified live tree.
                            cohort = None;
                            cohorts.accepted(&accepted);
                        }
                    }
                    Err(error) => failure = Some(error.context("speculative cohort did not integrate and verify; no settlement or automatic retry")),
                }
            }
        }
        if !stopping && failure.is_none() && cohort.is_none() {
            while let Some((goal, selected)) = state.settlements.front().cloned() {
                if let Err(error) = persist(Event::TaskSettled {
                    seq: 0,
                    task_id: goal.clone(),
                    selected: selected.clone(),
                }) {
                    journal_failed = true;
                    failure =
                        Some(error.context("persisting settlement; dependencies not released"));
                    break;
                }
                if let Err(error) = state.settle(goal, selected) {
                    failure = Some(error.context("applying durable settlement"));
                    break;
                }
            }
        }
        if !stopping && failure.is_none() {
            if cohort.is_none() && in_flight.is_empty() {
                let first = state.queue.iter().find(|pending|
                    state.eligible(pending) && pending.due <= Instant::now());
                if first.is_some_and(|pending| pending.intent.task.execution == ExecutionMode::Speculative) {
                    let keys: Vec<_> = state.queue.iter().filter(|pending|
                        pending.intent.task.execution == ExecutionMode::Speculative
                            && state.eligible(pending) && pending.due <= Instant::now())
                        .take(width.clamp(1, 32)).map(|pending| pending.intent.key.clone()).collect();
                    let mut selected = Vec::with_capacity(keys.len());
                    for key in &keys {
                        let position = state.queue.iter().position(|pending| &pending.intent.key == key).unwrap();
                        let mut pending = state.queue.remove(position).unwrap();
                        // Freeze the logical budget at capture too: a private attempt is a physical
                        // attempt like any other and must not outlive the parent's deadline.
                        state.freeze_deadline(&mut pending.intent);
                        if let Err(error) = state.materialize(&mut pending.intent)
                            .and_then(|()| prepare(&mut pending.intent)) {
                            failure = Some(error.context("preparing frozen speculative ready set"));
                            break;
                        }
                        selected.push(pending.intent);
                    }
                    if failure.is_none() {
                        match cohorts.capture(&mut selected) {
                            Ok(id) if !id.is_empty() && id.len() <= 128 => {
                                let members = selected.iter().map(|intent| intent.key.clone()).collect();
                                if let Err(error) = persist(Event::SpeculativeCohortCaptured {
                                    seq: 0, cohort_id: id.clone(), members,
                                }) {
                                    journal_failed = true;
                                    failure = Some(error.context("persisting speculative membership; dispatch not started"));
                                } else {
                                    for intent in selected.iter().rev() {
                                        state.queue.push_front(Pending {
                                            intent: intent.clone(), due: Instant::now(), admission_deadline: None,
                                        });
                                    }
                                    cohort = Some(ActiveCohort { id, intents: selected });
                                }
                            }
                            Ok(_) => failure = Some(anyhow::anyhow!("invalid controller cohort identity")),
                            Err(error) => failure = Some(error.context("capturing speculative cohort; no dispatch started")),
                        }
                    }
                }
            }
            let count = if failure.is_none() { state.queue.len() } else { 0 };
            for _ in 0..count {
                if in_flight.len() >= width.max(1) {
                    break;
                }
                let Some(mut pending) = state.queue.pop_front() else {
                    break;
                };
                if !state.eligible(&pending) || pending.due > Instant::now() {
                    state.queue.push_back(pending);
                    continue;
                }
                if let Some(selected) = &cohort {
                    if !selected.intents.iter().any(|intent| intent.key == pending.intent.key) {
                        state.queue.push_back(pending);
                        continue;
                    }
                } else if pending.intent.task.execution == ExecutionMode::Speculative {
                    // A cohort starts only at the idle boundary; direct work cannot enter it.
                    state.queue.push_front(pending);
                    break;
                }
                if cohort.is_none() {
                    if let Err(error) = state.materialize(&mut pending.intent) {
                        failure = Some(error.context("materializing queued dispatch"));
                        break;
                    }
                }
                state.freeze_deadline(&mut pending.intent);
                if pending.intent.deadline().is_some_and(|d| d <= std::time::Instant::now()) {
                    let intent = pending.intent;
                    let outcome = TaskOutcome {
                        id: intent.key.display(), attempt: Some(intent.key.clone()), selected_for: None,
                        role: intent.task.role.clone(), model: model.to_string(), status: "deadline".into(),
                        summary: "logical task deadline elapsed; no new attempt dispatched".into(),
                        iters: 0, tokens_in: 0, tokens_out: 0, evidence_images: Vec::new(),
                    };
                    if let Err(error) = persist(Event::DispatchIntent { seq: 0, intent: intent.clone() })
                        .and_then(|()| persist(completion_event(&intent, &outcome))) {
                        journal_failed = true;
                        failure = Some(error.context("recording expired logical task"));
                        break;
                    }
                    if let Err(error) = state.complete(intent, outcome) {
                        failure = Some(error.context("settling expired logical task"));
                        break;
                    }
                    // Back to the top so the recorded settlement is drained and published as a
                    // terminal logical outcome; the remaining queue keeps its own deadlines.
                    continue 'driver;
                }
                if let Err(error) = prepare(&mut pending.intent) {
                    failure = Some(error.context("preparing queued dispatch"));
                    break;
                }
                let writer = super::workflow_task_is_writer(&pending.intent.task);
                let mode = super::workflow_dispatch_mode(&pending.intent.task);
                let conflict = cohort.is_none() && active.values().any(|(other, other_writer)| {
                    if scoped_admission {
                        !mode.compatible(other)
                    } else {
                        writer && *other_writer
                    }
                });
                if conflict {
                    state.queue.push_back(pending);
                    continue;
                }
                let admission_deadline = *pending
                    .admission_deadline
                    .get_or_insert_with(|| Instant::now() + policy.admission_timeout);
                let permit = match admit(&pending.intent) {
                    Ok(permit) => permit,
                    Err(SlotDenied::Full) if Instant::now() < admission_deadline => {
                        state.queue.push_back(pending);
                        continue;
                    }
                    Err(SlotDenied::Full) => {
                        failure = Some(anyhow::anyhow!(
                            "bounded admission deadline expired for {}",
                            pending.intent.key.display()
                        ));
                        break;
                    }
                    Err(SlotDenied::Io(error)) => {
                        failure = Some(anyhow::anyhow!(
                            "sub-agent admission infrastructure failed: {error}"
                        ));
                        break;
                    }
                };
                if let Err(error) = persist(Event::DispatchIntent {
                    seq: 0,
                    intent: pending.intent.clone(),
                }) {
                    drop(permit);
                    journal_failed = true;
                    failure = Some(error.context("persisting intent; dispatch not started"));
                    break;
                }
                let intent = pending.intent;
                active.insert(intent.key.clone(), (mode, writer));
                // Boxed: one attempt's future carries the whole child agent loop (streaming model
                // call, tool execution, settlement), and holding several of them inline inside the
                // driver's own state machine overflows a worker-thread stack at real fan-out widths.
                let future = Box::pin(run(intent.clone(), permit));
                in_flight.push(async move { (intent, future.await) });
            }
        }
        if in_flight.is_empty() {
            if failure.is_some() || cancel.is_cancelled() || state.queue.is_empty() {
                break;
            }
            if !state.queue.iter().any(|p| state.eligible(p)) {
                if !state.settlements.is_empty() {
                    continue;
                }
                bail!("workflow queue has no eligible continuation");
            }
        }
        let wake = state
            .queue
            .iter()
            .filter(|p| state.eligible(p) && p.due > Instant::now())
            .map(|p| p.due)
            .min()
            .unwrap_or(Instant::now() + policy.probe_interval)
            .min(Instant::now() + policy.probe_interval);
        let finished = if failure.is_some() || cancel.is_cancelled() {
            in_flight.next().await
        } else {
            tokio::select! {
                result = in_flight.next(), if !in_flight.is_empty() => result,
                _ = cancel.cancelled() => { continue; }
                _ = tokio::time::sleep_until(wake) => { continue; }
            }
        };
        let Some((intent, mut outcome)) = finished else {
            break;
        };
        outcome.id = intent.key.display();
        outcome.attempt = Some(intent.key.clone());
        observed_in = observed_in.saturating_add(outcome.tokens_in);
        observed_out = observed_out.saturating_add(outcome.tokens_out);
        observed_count += 1;
        observed_reports.push(observed_report(&outcome));
        let worker_running = outcome.status == "unknown-worker";
        let effect = match outcome.status.as_str() {
            "unknown-worker" => Some(crate::agent::tools::ToolEffectState::UnknownWorker),
            "possibly-applied" => Some(crate::agent::tools::ToolEffectState::PossiblyApplied(None)),
            _ => None,
        };
        if let Some(effect) = effect {
            if unsettled_effect.is_none()
                || effect == crate::agent::tools::ToolEffectState::UnknownWorker
            {
                unsettled_effect = Some(effect);
            }
            failure.get_or_insert_with(|| {
                anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(effect)).context(
                    format!(
                        "attempt {} has unsettled effects; no retry or dependency release",
                        intent.key.display()
                    ),
                )
            });
        }
        if !worker_running {
            active.remove(&intent.key);
        }
        // Observer completion never clears an unknown physical worker's admission footprint.
        if !journal_failed {
            if let Err(error) = persist(completion_event(&intent, &outcome)) {
                journal_failed = true;
                failure = Some(
                    error.context("persisting completion; no new admission or dependency release"),
                );
            } else if worker_running {
                failure = Some(anyhow::Error::new(crate::agent::task_tool::TaskWorkerStillRunning)
                    .context(format!("attempt {} still has a live blocking worker; no retry or dependency release", intent.key.display())));
            } else if failure.is_none() {
                let speculative_failed = intent.task.execution == ExecutionMode::Speculative
                    && (!completed_status(&outcome.status) || verdict_failed(&outcome));
                if let Err(error) = state.complete(intent, outcome) {
                    failure = Some(error.context("applying durable completion"));
                } else if speculative_failed {
                    failure = Some(anyhow::anyhow!("speculative cohort has a failed, incomplete or FAIL-verdict attempt; private edits retained, no integration, retry or dependency release"));
                }
            }
        }
    }
    // All admitted futures were drained. A live blocking body retains its own execution
    // context/handle; scheduler graph metadata must never keep a physically-settled owner alive.
    drop(cohort);
    for pending in &mut state.queue {
        pending.intent.task.overlay = None;
    }
    for intent in state.intents.values_mut() {
        intent.task.overlay = None;
    }
    cohorts.release();
    if let Some(error) = failure {
        let mut report = format!(
            "workflow infrastructure failure: {error:#}\nobserved spend: {observed_in}→{observed_out} tok across {observed_count} observed physical completions (live workers may have additional unreported spend)"
        );
        if !observed_reports.is_empty() {
            report.push_str("\n[Workflow incomplete: infrastructure stopped new admission; observed reports are retained below, not proof of completed work.]\n");
            for outcome in &observed_reports {
                report.push_str(outcome);
            }
        }
        let error = match unsettled_effect {
            Some(crate::agent::tools::ToolEffectState::UnknownWorker) => {
                anyhow::Error::new(crate::agent::task_tool::TaskWorkerStillRunning)
                    .context("workflow has unsettled physical effects; live blocking worker requires reconciliation")
                    .context(report)
            }
            Some(effect) => anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(effect))
                .context("workflow has unsettled possibly-applied effects; reconcile before new work")
                .context(report),
            None => error.context(report),
        };
        return Err(error);
    }
    for task in tasks {
        if !state.settled.contains_key(&TaskId(task.id.clone())) {
            state.results.push(TaskOutcome {
                id: task.id.clone(),
                selected_for: Some(TaskId(task.id.clone())),
                attempt: None,
                role: task.role.clone(),
                model: model.to_string(),
                status: "cancelled".into(),
                summary: "cancelled before logical task settled; admitted executions were drained"
                    .into(),
                iters: 0,
                tokens_in: 0,
                tokens_out: 0,
                evidence_images: Vec::new(),
            });
        }
    }
    Ok(state.results)
}

#[cfg(test)]
mod scoped_admission_tests {
    use super::*;
    use crate::agent::workflow::{ResolvedWorkflowDispatch, WorkflowDispatchMode};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct Capability(&'static str);
    impl crate::agent::tools::Tool for Capability {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "deterministic scheduler capability"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_destructive(&self) -> bool {
            self.0 != "file_read"
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            bail!("fixture never executes tools")
        }
    }

    pub(super) fn frozen_task(
        root: &std::path::Path,
        id: &str,
        kind: &str,
        paths: &[&str],
    ) -> WorkflowTask {
        let mut registry = crate::agent::tools::ToolRegistry::new();
        registry.register(Box::new(Capability("file_read")));
        if kind != "read" {
            registry.register(Box::new(Capability(if kind == "opaque" {
                "shell_run"
            } else {
                "file_edit"
            })));
        }
        let native_scope = if kind == "scoped" {
            Some(
                crate::core::native_scope::NativeWriteScope::resolve(
                    root,
                    &paths
                        .iter()
                        .map(|path| path.to_string())
                        .collect::<Vec<_>>(),
                )
                .unwrap(),
            )
        } else {
            None
        };
        let declared_write_paths = native_scope
            .as_ref()
            .map(|_| paths.iter().map(|path| path.to_string()).collect());
        let dispatch = ResolvedWorkflowDispatch {
            label: id.into(),
            ep: crate::core::cli_config::ResolvedEndpoint {
                base_url: "https://unused.example".into(),
                api_key: "unused".into(),
                model: "m".into(),
            },
            registry,
            native_scope,
            declared_write_paths: declared_write_paths.clone(),
            declared_read_paths: None,
            execution: ExecutionMode::Direct,
            system: "fixed".into(),
            contract: crate::agent::task_tool::TaskContract {
                boundaries: None,
                expected_output: None,
                max_steps: 3,
            },
            expects: None,
            reasoning_effort: None,
            image_config: crate::core::cli_config::CliConfig::default(),
            image_policy: crate::llm::media::VisionPolicy::Unknown,
            image_destination: "unused".into(),
            relay_allowed: false,
        };
        WorkflowTask {
            id: id.into(),
            role: if kind == "read" { "argus" } else { "daedalus" }.into(),
            prompt: format!("complete {id}"),
            write_paths: declared_write_paths,
            prepared: Some(std::sync::Arc::new(dispatch)),
            ..Default::default()
        }
    }

    fn done(intent: &DispatchIntent, status: &str) -> TaskOutcome {
        TaskOutcome {
            id: intent.key.display(),
            role: intent.task.role.clone(),
            model: "m".into(),
            status: status.into(),
            summary: "fixture complete".into(),
            iters: 1,
            tokens_in: 1,
            tokens_out: 1,
            selected_for: None,
            attempt: None,
            evidence_images: Vec::new(),
        }
    }

    #[test]
    fn frozen_modes_reject_json_only_scope_and_keep_writers_destructive() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let writer = frozen_task(fixture.path(), "writer", "scoped", &["a.rs"]);
        assert!(super::super::workflow_task_is_writer(&writer));
        assert!(matches!(
            super::super::workflow_dispatch_mode(&writer),
            WorkflowDispatchMode::ScopedNative(_)
        ));
        let mut forged = writer.clone();
        forged.prepared = None;
        assert!(matches!(
            super::super::workflow_dispatch_mode(&forged),
            WorkflowDispatchMode::Opaque
        ));
        let other = frozen_task(fixture.path(), "other", "scoped", &["b.rs"]);
        let spec = super::super::WorkflowSpec {
            name: "disjoint".into(),
            tasks: vec![writer.clone(), other],
            synthesis: None,
            prepared_synthesis: None,
        };
        assert!(super::super::enforce_workflow_writer_policy(&spec, false).is_err());
        assert!(super::super::enforce_workflow_writer_policy(&spec, true).is_ok());
        let mut conflict = spec;
        conflict.tasks[1] = frozen_task(fixture.path(), "other", "scoped", &["a.rs"]);
        assert!(super::super::enforce_workflow_writer_policy(&conflict, true).is_ok());
        assert!(!super::super::workflow_dispatch_mode(&conflict.tasks[0])
            .compatible(&super::super::workflow_dispatch_mode(&conflict.tasks[1])));
        conflict.tasks[1].prepared = None;
        assert!(super::super::enforce_workflow_writer_policy(&conflict, true).is_err());
    }

    #[tokio::test]
    async fn disjoint_scoped_bodies_overlap_with_dependencies_and_durable_order() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let mut tasks = vec![
            frozen_task(fixture.path(), "a", "scoped", &["a.rs"]),
            frozen_task(fixture.path(), "b", "scoped", &["b.rs"]),
            frozen_task(fixture.path(), "dependent", "read", &[]),
        ];
        tasks[2].after = vec!["a".into(), "b".into()];
        let barrier = Rc::new(tokio::sync::Barrier::new(2));
        let active = Rc::new(Cell::new(0));
        let peak = Rc::new(Cell::new(0));
        let events = RefCell::new(Vec::new());
        let ran = RefCell::new(Vec::new());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            schedule_with_admission_policy(
                &tasks,
                3,
                "m",
                &TurnCancel::new(),
                Policy::default(),
                &[],
                |_| Ok(()),
                |_| Ok(()),
                |event| {
                    events.borrow_mut().push(event);
                    Ok(())
                },
                |intent, ()| {
                    let barrier = barrier.clone();
                    let active = active.clone();
                    let peak = peak.clone();
                    let ran = &ran;
                    async move {
                        ran.borrow_mut().push(intent.key.task_id.0.clone());
                        if intent.task.id == "dependent" {
                            assert_eq!(active.get(), 0);
                            assert_eq!(intent.inputs.len(), 2);
                        } else {
                            active.set(active.get() + 1);
                            peak.set(peak.get().max(active.get()));
                            barrier.wait().await;
                            active.set(active.get() - 1);
                        }
                        done(&intent, "done")
                    }
                },
                true,
            ),
        )
        .await
        .expect("deterministic disjoint barrier must make progress")
        .unwrap();
        assert_eq!(peak.get(), 2);
        assert_eq!(*ran.borrow(), ["a", "b", "dependent"]);
        let events = events.borrow();
        let dependent = events.iter().position(|event| matches!(event, Event::DispatchIntent { intent, .. } if intent.task.id == "dependent")).unwrap();
        assert_eq!(
            events[..dependent]
                .iter()
                .filter(|event| matches!(event, Event::TaskSettled { .. }))
                .count(),
            2
        );
        State::new(&tasks, Policy::default())
            .replay(&events)
            .unwrap();
        assert_eq!(super::super::logical_outcomes(&result).len(), 3);
    }

    #[tokio::test]
    async fn conflicting_unknown_reads_opaque_and_gate_off_are_serial() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        for (first, second, same, enabled) in [
            ("scoped", "scoped", true, true),
            ("read", "scoped", false, true),
            ("scoped", "read", false, true),
            ("opaque", "scoped", false, true),
            ("read", "opaque", false, true),
            ("scoped", "scoped", false, false),
        ] {
            let tasks = vec![
                frozen_task(fixture.path(), "a", first, &["a.rs"]),
                frozen_task(
                    fixture.path(),
                    "b",
                    second,
                    &[if same { "a.rs" } else { "b.rs" }],
                ),
            ];
            let active = Cell::new(0);
            let peak = Cell::new(0);
            let outcomes = schedule_with_admission_policy(
                &tasks,
                2,
                "m",
                &TurnCancel::new(),
                Policy::default(),
                &[],
                |_| Ok(()),
                |_| Ok(()),
                |_| Ok(()),
                |intent, ()| {
                    // run is invoked at admission, before futures are polled: charging here catches
                    // an early conflicting admission even if the first future returns immediately.
                    active.set(active.get() + 1);
                    peak.set(peak.get().max(active.get()));
                    let active = &active;
                    async move {
                        tokio::task::yield_now().await;
                        active.set(active.get() - 1);
                        done(&intent, "done")
                    }
                },
                enabled,
            )
            .await
            .unwrap();
            assert_eq!(
                peak.get(),
                1,
                "{first}/{second}, same={same}, enabled={enabled}"
            );
            assert_eq!(outcomes.len(), 2);
        }
    }

    #[tokio::test]
    async fn frozen_readers_overlap_and_cancel_drains_scoped_permits() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        for kind in ["read", "scoped"] {
            let tasks = vec![
                frozen_task(fixture.path(), "a", kind, &["a.rs"]),
                frozen_task(fixture.path(), "b", kind, &["b.rs"]),
                frozen_task(fixture.path(), "pending", kind, &["c.rs"]),
            ];
            let cancel = TurnCancel::new();
            let barrier = Rc::new(tokio::sync::Barrier::new(2));
            let permits = Rc::new(Cell::new(0));
            struct Permit(Rc<Cell<usize>>);
            impl Drop for Permit {
                fn drop(&mut self) {
                    self.0.set(self.0.get() - 1);
                }
            }
            let ran = RefCell::new(Vec::new());
            let completed = RefCell::new(Vec::new());
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                schedule_with_admission_policy(
                    &tasks,
                    2,
                    "m",
                    &cancel,
                    Policy::default(),
                    &[],
                    |_| Ok(()),
                    |_| {
                        if permits.get() == 2 {
                            return Err(SlotDenied::Full);
                        }
                        permits.set(permits.get() + 1);
                        Ok(Permit(permits.clone()))
                    },
                    |event| {
                        if let Event::AttemptCompleted { key, .. } = event {
                            completed.borrow_mut().push(key.task_id.0);
                        }
                        Ok(())
                    },
                    |intent, permit| {
                        ran.borrow_mut().push(intent.task.id.clone());
                        let barrier = barrier.clone();
                        let cancel = cancel.clone();
                        async move {
                            barrier.wait().await;
                            cancel.cancel();
                            tokio::task::yield_now().await;
                            drop(permit);
                            done(&intent, "done")
                        }
                    },
                    true,
                ),
            )
            .await
            .expect("reader/scoped barrier must make progress")
            .unwrap();
            assert_eq!(*ran.borrow(), ["a", "b"]);
            assert_eq!(permits.get(), 0);
            assert_eq!(completed.borrow().len(), 2);
            assert!(result
                .iter()
                .any(|outcome| outcome.id == "pending" && outcome.status == "cancelled"));
        }
    }

    #[tokio::test]
    async fn scoped_unknown_worker_stops_retry_dependencies_and_drains_admitted_sibling() {
        for effect_status in ["unknown-worker", "possibly-applied"] {
            for failed_completion in [None, Some("a"), Some("b")] {
                let fixture = crate::core::file_placement::tests::TestDir::new();
                let mut tasks = vec![
                    frozen_task(fixture.path(), "a", "scoped", &["a.rs"]),
                    frozen_task(fixture.path(), "b", "scoped", &["b.rs"]),
                    frozen_task(fixture.path(), "dependent", "read", &[]),
                ];
                tasks[2].after = vec!["a".into()];
                let ran = RefCell::new(Vec::new());
                let events = RefCell::new(Vec::new());
                let error = schedule_with_admission_policy(
                    &tasks,
                    3,
                    "m",
                    &TurnCancel::new(),
                    Policy::default(),
                    &[],
                    |_| Ok(()),
                    |_| Ok(()),
                    |event| {
                        let failed = matches!(&event, Event::AttemptCompleted { key, .. }
                    if Some(key.task_id.0.as_str()) == failed_completion);
                        events.borrow_mut().push(event);
                        if failed {
                            bail!("owned completion persistence fault");
                        }
                        Ok(())
                    },
                    |intent, ()| {
                        ran.borrow_mut().push(intent.task.id.clone());
                        async move {
                            done(
                                &intent,
                                if intent.task.id == "a" {
                                    effect_status
                                } else {
                                    "done"
                                },
                            )
                        }
                    },
                    true,
                )
                .await
                .unwrap_err();
                if effect_status == "unknown-worker" {
                    assert!(error.is::<crate::agent::task_tool::TaskWorkerStillRunning>());
                } else {
                    assert_eq!(
                        crate::agent::tool_error_effect_state(&error),
                        crate::agent::tools::ToolEffectState::PossiblyApplied(None)
                    );
                }
                assert!(
                    format!("{error:#}").contains("unsettled")
                        || format!("{error:#}").contains("live blocking worker")
                );
                assert!(
                    format!("{error:#}").contains("fixture complete"),
                    "completed sibling summaries survive typed propagation"
                );
                assert_eq!(*ran.borrow(), ["a", "b"]);
                assert_eq!(
                    events
                        .borrow()
                        .iter()
                        .filter(|event| matches!(event, Event::AttemptCompleted { .. }))
                        .count(),
                    if failed_completion == Some("a") { 1 } else { 2 }
                );
                assert!(State::new(&tasks, Policy::default())
                    .replay(&events.borrow())
                    .is_err());
            }
        }
    }
}
