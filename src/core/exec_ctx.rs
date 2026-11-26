//! Turn-scoped execution context — the explicit carrier of per-turn/per-conversation identity that
//! a tool body needs, threaded across the `spawn_blocking` boundary the way [`crate::core::cancel`]
//! already threads the cancellation token.
//!
//! # Why this exists
//!
//! Tool bodies run on a `spawn_blocking` worker thread (`src/agent/mod.rs`), so any per-turn fact a
//! tool needs must either be captured into the `move` closure or seeded into a thread-local INSIDE
//! that closure — a plain process-global read gives whatever the driver thread last set, which bleeds
//! across turns the moment two turns interleave. Today exactly one turn runs at a time per process
//! (the REPL is serial; the `serve` daemon handles messages serially), so the bleed is latent rather
//! than live — but the browser tools already key their per-conversation session registry off
//! [`crate::core::convo::active()`], so the conversation id is the one piece of context a tool body
//! genuinely reads across the hop. This module makes that read authoritative: the loop pins an
//! `ExecutionContext` for the turn, seeds it into the thread-local inside the same closure that seeds
//! the cancel token, and [`crate::core::convo::active()`] prefers it over the process-global slot.
//!
//! # What lives here, and what deliberately does not
//!
//! The context carries per-turn facts whose reader would otherwise have to consult a process-global:
//! the conversation id (read by the browser session registry inside a tool body) and the approval
//! route (read by the approval gate on the driver). Both are read far from where they are set.
//!
//! Two other per-turn facts are deliberately NOT here, because they already have a direct owner and a
//! second copy would just be a second source of truth to keep in sync:
//!   * the workspace root → `AgentConfig::workspace_root`, read via `AgentConfig::effective_root`;
//!     the placement snapshot is derived from it at execution, not a second mutable authority;
//!   * the reasoning-effort tier → passed explicitly to `client::chat_with_tools_effort`.
//!
//! How to READ each field, which differs by where the code runs:
//!   * inside a tool body (`spawn_blocking` worker) → [`current`], seeded by the executor;
//!   * on the driver (the approval gate) → through `AgentConfig::exec_ctx`, because the driver never
//!     pushes itself onto the thread-local stack.
//!
//! Every field is `Option`: `None` means "no per-turn override — fall back to the process-global /
//! config value", which is exactly what the REPL and one-shot CLI paths want.

use crate::core::convo::ConversationId;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Volatile ownership accounting for one speculative attempt.
///
/// This is deliberately separate from `Arc::strong_count`: each executable context fork owns a
/// carrier, and the coordinating owner waits for all carriers to retire before releasing attempt
/// resources. It is a closure condition, not a durable lifecycle receipt.
#[derive(Debug)]
pub(crate) struct AttemptCarrierRegistry {
    live: AtomicUsize,
    notify: tokio::sync::Notify,
}

impl AttemptCarrierRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    pub(crate) fn acquire(self: &Arc<Self>) -> AttemptCarrier {
        self.live.fetch_add(1, Ordering::AcqRel);
        AttemptCarrier {
            registry: self.clone(),
        }
    }

    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_empty(&self, deadline: tokio::time::Instant) -> anyhow::Result<()> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.live() == 0 {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .map_err(|_| {
                    anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(
                        crate::agent::tools::ToolEffectState::UnknownWorker,
                    ))
                    .context(format!(
                        "speculative attempt carriers remain live ({}); release not certified",
                        self.live()
                    ))
                })?;
        }
    }
}

/// One executable-context ownership carrier. It is neither serializable nor model-controlled.
#[derive(Debug)]
pub(crate) struct AttemptCarrier {
    registry: Arc<AttemptCarrierRegistry>,
}

impl Clone for AttemptCarrier {
    fn clone(&self) -> Self {
        self.registry.acquire()
    }
}

impl Drop for AttemptCarrier {
    fn drop(&mut self) {
        let previous = self.registry.live.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "attempt carrier accounting underflow");
        self.registry.notify.notify_one();
    }
}

/// Actual delegated workspace changes, independent of a child's final report or success status.
/// Inherited collectors see descendants' writes; siblings keep separate per-dispatch collectors.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceChanges {
    paths: std::sync::Mutex<Vec<std::path::PathBuf>>,
    changed: std::sync::atomic::AtomicBool,
}

impl WorkspaceChanges {
    pub(crate) fn record(&self, paths: &[std::path::PathBuf]) {
        let mut recorded = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        for path in paths {
            if !recorded.contains(path) {
                recorded.push(path.clone());
            }
        }
        self.changed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn changed(&self) -> bool {
        self.changed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn paths(&self) -> Vec<std::path::PathBuf> {
        self.paths.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Where a destructive-op approval prompt for THIS turn should be delivered: the hosted-bot route
/// (sub-bot name) and the platform chat id, rendered as a string so this stays platform-agnostic
/// (Telegram `i64`, Discord `u64`). The Telegram platform maps `route` back to that bot's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRoute {
    pub route: String,
    pub chat: String,
    pub sender_id: Option<String>,
}

/// Host-only provenance for prepared calls. It identifies one immutable placement, not live Git
/// ancestry or a mutable session focus. Moving to another placement mints a new token; existing
/// children retain their original one. Full focus must additionally settle/rebind all host scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceBinding {
    generation: u64,
    workspace_root: std::path::PathBuf,
    project_root: std::path::PathBuf,
    home_root: std::path::PathBuf,
}

impl WorkspaceBinding {
    fn new(placement: &crate::core::file_placement::PlacementContext) -> Self {
        Self {
            generation: next_workspace_generation(),
            workspace_root: placement.workspace_root().to_path_buf(),
            project_root: placement.project_root().to_path_buf(),
            home_root: placement.home_root().to_path_buf(),
        }
    }

    fn matches_placement(&self, placement: &crate::core::file_placement::PlacementContext) -> bool {
        self.workspace_root == placement.workspace_root()
            && self.project_root == placement.project_root()
            && self.home_root == placement.home_root()
    }
}

/// A host-only placement snapshot that retains prepared-call provenance when a fresh turn is built.
/// It carries no task, policy, admission, resource scope, or authority from the originating turn.
#[derive(Debug, Clone)]
pub(crate) struct WorkspacePlacement {
    placement: crate::core::file_placement::PlacementContext,
    binding: WorkspaceBinding,
}

static NEXT_WORKSPACE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_workspace_generation() -> u64 {
    let mut current = NEXT_WORKSPACE_GENERATION.load(Ordering::Relaxed);
    loop {
        let next = current
            .checked_add(1)
            .expect("workspace binding generation exhausted");
        match NEXT_WORKSPACE_GENERATION.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return current,
            Err(actual) => current = actual,
        }
    }
}

/// Immutable per-turn identity. Cheap to clone (one `Arc` bump); shared by a top-level turn and every
/// delegated sub-agent that inherits it.
#[derive(Debug)]
struct Inner {
    /// The conversation this turn serves — a REPL session slug, or a `serve`
    /// `platform:route:chat` triple. Scopes per-conversation resources (the browser session).
    conversation: Option<ConversationId>,
    /// One concrete agent/tool-execution scope within a conversation. Top-level turns inherit the
    /// conversation id; each delegated child derives its own id so stateful tools such as `process`
    /// cannot list/read/kill a sibling's handles.
    resource_scope: Option<String>,
    /// Whether tool bodies may emit progress lines into the parent transcript. Delegated children run
    /// quiet and publish progress through the orchestration board instead.
    trace_visible: bool,
    /// Persona this turn speaks as, layered over the global `config.persona`. `None` ⇒ the global one.
    persona: Option<String>,
    /// Where an approval prompt goes this turn. `None` ⇒ the platform's process-global fallback.
    approval_route: Option<ApprovalRoute>,
    /// The delegated child this context runs — role and brief subject (`daedalus · fix parser`)
    /// — so an approval it raises says who is asking. `None` for a top-level turn.
    dispatch_label: Option<String>,
    /// Conversation-owned design state; cloned handles share a task, delegated children fork it.
    design: Option<crate::agent::design::DesignContext>,
    /// Owned checklist authority; never resolved through a process-global task list.
    task: Option<crate::agent::todo::TaskContext>,
    /// Frozen conversation permission, independent of private/reset task state.
    policy: Option<crate::core::session_control::ExecutionPolicy>,
    /// Kept by every executable fork until the last physical worker settles.
    policy_lease: Option<Arc<crate::core::session_control::PolicyLease>>,
    /// Single native document operation, armed only inside its guarded write closure.
    plan_write_proof: Option<Arc<crate::agent::planning_write::PlanWriteProof>>,
    /// Live-tree ancestors receive effect invalidation, never observation/check authority.
    task_effect_sinks: Vec<crate::agent::todo::TaskContext>,
    /// Immutable destination roots/run scope pinned from AgentConfig::effective_root by executor.
    placement: Option<crate::core::file_placement::PlacementContext>,
    /// Immutable workspace placement and controller generation. A child inherits this binding unless
    /// an explicit owned-worktree fork supplies a new placement/generation.
    binding: Option<WorkspaceBinding>,
    /// Clone within one logical run; explicitly fork for a delegated child or fresh user work.
    recovery: crate::core::run_recovery::RunRecovery,
    /// Only the coordinating parent may restore workspace state; delegation revokes this permanently.
    restore_authorized: bool,
    /// Exact file authority, inherited monotonically across retries and nested contexts.
    native_scope: Option<crate::core::native_scope::NativeWriteScope>,
    workspace_activities: Vec<Arc<crate::core::workspace_txn::WorkspaceActivity>>,
    workspace_changes: Vec<Arc<WorkspaceChanges>>,
    /// Retain the separately registered Git task owner through every physical child worker.
    owned_git_task: Option<Arc<crate::core::git_ops::OwnedTask>>,
    /// Lifetime-only controller retention for Git preparation/inspection workers, not native authority.
    owned_task_allocation_owner: Option<Arc<crate::core::git_ops::OwnedTaskAllocationOwner>>,
    #[cfg(test)]
    native_body_probe: Option<crate::agent::builtin::NativeBodyTestProbe>,
    /// Physical execution keeps admission charged while any blocking tool worker still owns it.
    attempt_slot: Option<Arc<crate::agent::task_tool::SubagentSlot>>,
    /// Weak launch admission only; the controller owns physical body joins separately.
    tool_workers: Option<crate::agent::tools::workers::ToolWorkerProducer>,
    writer_leases: Vec<crate::core::workspace_txn::WorkspaceWriterLease>,
    /// Retention of an already-admitted synchronous body, never admission for a second body.
    physical_effect_leases: Vec<crate::core::workspace_txn::PhysicalEffectLease>,
    /// Absolute parent run deadline inherited across delegation and retries, never refreshed.
    deadline: Option<std::time::Instant>,
    /// Volatile selected-reference consent; never recovered from serialized attachment metadata.
    references: Option<crate::core::reference_media::ReferenceAccess>,
    /// Current completed tool image groups for this one resource scope; never a session/global index.
    tool_images: Arc<std::sync::Mutex<Vec<crate::core::tool_media::ToolImage>>>,
    visual: Arc<std::sync::Mutex<crate::core::visual_context::VisualContext>>,
    media: Arc<crate::core::media_budget::MediaBudget>,
    /// Keep the owned fixture directory alive until this context's lock leases have dropped.
    #[cfg(test)]
    test_lock_namespace: Option<crate::core::workspace_txn::TestLockNamespace>,
    /// Last field: retire the carrier only after this context's resource fields have dropped.
    /// This ordering still does not prove release of raw handles cloned outside the context.
    attempt_carrier: Option<AttemptCarrier>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            conversation: None,
            resource_scope: None,
            trace_visible: true,
            persona: None,
            approval_route: None,
            dispatch_label: None,
            design: None,
            task: None,
            policy: None,
            policy_lease: None,
            plan_write_proof: None,
            task_effect_sinks: Vec::new(),
            placement: None,
            binding: None,
            recovery: crate::core::run_recovery::RunRecovery::default(),
            restore_authorized: true,
            native_scope: None,
            workspace_activities: Vec::new(),
            workspace_changes: Vec::new(),
            owned_git_task: None,
            owned_task_allocation_owner: None,
            #[cfg(test)]
            native_body_probe: None,
            attempt_carrier: None,
            attempt_slot: None,
            tool_workers: None,
            writer_leases: Vec::new(),
            physical_effect_leases: Vec::new(),
            deadline: None,
            references: None,
            tool_images: Arc::new(std::sync::Mutex::new(Vec::new())),
            visual: Arc::new(std::sync::Mutex::new(Default::default())),
            media: Arc::new(Default::default()),
            #[cfg(test)]
            test_lock_namespace: crate::core::workspace_txn::explicit_test_lock_namespace(),
        }
    }
}

/// A native read's pre-operation owner/requirement/epoch, never constructible from tool output.
pub(crate) struct TaskObservation {
    task: crate::agent::todo::TaskContext,
    token: crate::agent::todo::EvidenceToken,
    target: String,
    path: std::path::PathBuf,
}

impl TaskObservation {
    pub(crate) fn record(self, fingerprint: &crate::core::persist::FileFingerprint) {
        // Evidence admission is best-effort: a stale token/capacity refusal must not turn a
        // successful file read into a misleading tool failure, nor fabricate completion.
        let _ = self
            .task
            .record_file_observation(self.token, self.target, &self.path, fingerprint);
    }
}

/// Cloneable handle to one turn's execution context. Threaded through [`crate::agent::AgentConfig`]
/// and seeded into a thread-local inside the `spawn_blocking` closure so tool bodies read it.
#[derive(Debug, Clone)]
pub struct ExecutionContext(Arc<Inner>);

impl ExecutionContext {
    /// Build a context for a turn serving `conversation`.
    pub fn new(conversation: ConversationId) -> Self {
        Self(Arc::new(Inner {
            resource_scope: Some(conversation.to_string()),
            conversation: Some(conversation),
            trace_visible: true,
            ..Inner::default()
        }))
    }

    /// The conversation this turn serves. Read by `convo::active()` inside tool bodies.
    #[cfg_attr(not(any(feature = "browser", test)), allow(dead_code))]
    pub fn conversation(&self) -> ConversationId {
        self.0
            .conversation
            .clone()
            .unwrap_or_else(|| ConversationId::new("default"))
    }

    // ── per-turn overrides ────────────────────────────────────────────────────────────
    //
    // Builders take `self` and return a new `Arc` rather than mutating: the context is shared with
    // already-spawned tool bodies, so an in-place edit would retroactively change a running turn's
    // view. Chain them at the turn boundary, before the context is handed to `AgentConfig`.

    fn with(&self, f: impl FnOnce(&mut Inner)) -> Self {
        let mut next = Inner {
            conversation: self.0.conversation.clone(),
            resource_scope: self.0.resource_scope.clone(),
            trace_visible: self.0.trace_visible,
            persona: self.0.persona.clone(),
            approval_route: self.0.approval_route.clone(),
            dispatch_label: self.0.dispatch_label.clone(),
            design: self.0.design.clone(),
            task: self.0.task.clone(),
            policy: self.0.policy.clone(),
            policy_lease: self.0.policy_lease.clone(),
            plan_write_proof: self.0.plan_write_proof.clone(),
            task_effect_sinks: self.0.task_effect_sinks.clone(),
            placement: self.0.placement.clone(),
            binding: self.0.binding.clone(),
            recovery: self.0.recovery.clone(),
            restore_authorized: self.0.restore_authorized,
            native_scope: self.0.native_scope.clone(),
            workspace_activities: self.0.workspace_activities.clone(),
            workspace_changes: self.0.workspace_changes.clone(),
            owned_git_task: self.0.owned_git_task.clone(),
            owned_task_allocation_owner: self.0.owned_task_allocation_owner.clone(),
            #[cfg(test)]
            native_body_probe: self.0.native_body_probe.clone(),
            attempt_carrier: self.0.attempt_carrier.clone(),
            attempt_slot: self.0.attempt_slot.clone(),
            tool_workers: self.0.tool_workers.clone(),
            writer_leases: self.0.writer_leases.clone(),
            physical_effect_leases: self.0.physical_effect_leases.clone(),
            deadline: self.0.deadline,
            references: self.0.references.clone(),
            tool_images: self.0.tool_images.clone(),
            visual: self.0.visual.clone(),
            media: self.0.media.clone(),
            #[cfg(test)]
            test_lock_namespace: self.0.test_lock_namespace.clone(),
        };
        f(&mut next);
        Self(Arc::new(next))
    }

    #[cfg(test)]
    pub(crate) fn with_test_lock_namespace(
        &self,
        namespace: crate::core::workspace_txn::TestLockNamespace,
    ) -> Self {
        self.with(|i| i.test_lock_namespace = Some(namespace))
    }

    #[cfg(test)]
    pub(crate) fn test_lock_namespace(
        &self,
    ) -> Option<&crate::core::workspace_txn::TestLockNamespace> {
        self.0.test_lock_namespace.as_ref()
    }

    pub fn with_deadline(&self, deadline: Option<std::time::Instant>) -> Self {
        self.with(|i| {
            i.deadline = match (i.deadline, deadline) {
                (Some(parent), Some(child)) => Some(parent.min(child)),
                (parent, child) => parent.or(child),
            }
        })
    }

    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.0.deadline
    }

    pub(crate) fn with_attempt_carrier_registry(
        &self,
        registry: &Arc<AttemptCarrierRegistry>,
    ) -> Self {
        self.with(|i| i.attempt_carrier = Some(registry.acquire()))
    }

    /// Retain admission across the blocking worker boundary, not just the observing future.
    pub(crate) fn with_attempt_slot(
        &self,
        slot: Arc<crate::agent::task_tool::SubagentSlot>,
    ) -> Self {
        self.with(|i| i.attempt_slot = Some(slot))
    }

    pub(crate) fn with_tool_workers(
        &self,
        producer: crate::agent::tools::workers::ToolWorkerProducer,
    ) -> Self {
        self.with(|i| i.tool_workers = Some(producer))
    }

    pub(crate) fn tool_workers(&self) -> Option<&crate::agent::tools::workers::ToolWorkerProducer> {
        self.0.tool_workers.as_ref()
    }

    pub(crate) fn with_writer_lease(
        &self,
        lease: crate::core::workspace_txn::WorkspaceWriterLease,
    ) -> Self {
        self.with(|i| i.writer_leases.push(lease))
    }

    /// Give a delegated child an isolated resource namespace without changing lifetime ownership.
    pub fn with_resource_scope(&self, scope: impl Into<String>) -> Self {
        let scope = scope.into();
        self.with(|i| {
            if i.resource_scope.as_deref() != Some(scope.as_str()) {
                i.tool_images = Arc::new(std::sync::Mutex::new(Vec::new()));
                i.visual = Arc::new(std::sync::Mutex::new(Default::default()));
                // Child scratch work must never acquire the parent's checklist authority.
                // Keep only an invalidation sink: a failed child can still have changed live files.
                if let Some(parent) = i.task.take() {
                    if i.policy.is_none() {
                        i.policy = Some(parent.control().policy());
                    }
                    if !i
                        .task_effect_sinks
                        .iter()
                        .any(|sink| sink.same_owner(&parent))
                    {
                        i.task_effect_sinks.push(parent);
                    }
                }
                let task = crate::agent::todo::TaskContext::private();
                i.task = Some(match &i.policy {
                    Some(policy) => task.with_control(policy.context()),
                    None => task,
                });
            }
            i.resource_scope = Some(scope);
        })
    }
    /// Resource namespace visible to stateful tool bodies.
    pub fn resource_scope(&self) -> String {
        self.0
            .resource_scope
            .clone()
            .or_else(|| self.0.conversation.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| "default".to_string())
    }

    /// Control whether a tool body may append progress to the parent transcript.
    pub fn with_trace_visible(&self, visible: bool) -> Self {
        self.with(|i| i.trace_visible = visible)
    }
    pub fn trace_visible(&self) -> bool {
        self.0.trace_visible
    }

    /// Pin the persona this turn speaks as (`None` ⇒ fall back to the global `config.persona`).
    pub fn with_persona(&self, persona: Option<String>) -> Self {
        self.with(|i| i.persona = persona)
    }
    /// The turn's persona override, if any.
    pub fn persona(&self) -> Option<String> {
        self.0.persona.clone()
    }

    /// Pin where an approval prompt for this turn is delivered.
    pub fn with_approval_route(&self, route: Option<ApprovalRoute>) -> Self {
        self.with(|i| i.approval_route = route)
    }

    /// Name the delegated child this context runs (role · brief subject), so an approval it
    /// raises and the `/workflows` row say who is asking.
    pub fn with_dispatch_label(&self, label: Option<String>) -> Self {
        self.with(|i| i.dispatch_label = label)
    }
    /// The delegated child's label, if this context runs one.
    pub fn dispatch_label(&self) -> Option<String> {
        self.0.dispatch_label.clone()
    }
    pub fn with_task(&self, task: crate::agent::todo::TaskContext) -> Self {
        self.with(|i| {
            let task = if let Some(policy) = &i.policy {
                task.with_control(policy.context())
            } else {
                i.policy = Some(task.control().policy());
                task
            };
            i.task = Some(task);
        })
    }

    pub(crate) fn policy(&self) -> Option<&crate::core::session_control::ExecutionPolicy> {
        self.0.policy.as_ref()
    }

    pub(crate) fn is_planning(&self) -> bool {
        self.policy().is_some_and(|policy| policy.is_planning())
    }

    /// Freeze admission into the context so detached worker clones retain it after observer exit.
    pub(crate) fn admit_policy(&self) -> anyhow::Result<Self> {
        let policy = self
            .policy()
            .ok_or_else(|| anyhow::anyhow!("execution policy is missing"))?;
        policy.validate_current()?;
        if let Some(parent) = crate::agent::planning::current_policy() {
            parent.check_inheritance(policy)?;
        }
        if self.0.policy_lease.is_some() {
            return Ok(self.clone());
        }
        let lease = Arc::new(policy.admit()?);
        Ok(self.with(|i| i.policy_lease = Some(lease)))
    }

    /// Host automation is not a model tool; it must consult the same immutable restriction.
    pub(crate) fn permits_automation(&self) -> bool {
        let owned = self
            .policy()
            .cloned()
            .or_else(|| self.task().map(|task| task.control().policy()));
        let inherited = crate::agent::planning::current_policy();
        if let (Some(parent), Some(owned)) = (&inherited, &owned) {
            if parent.check_inheritance(owned).is_err() {
                return false;
            }
        }
        owned
            .or(inherited)
            .is_none_or(|policy| policy.validate_current().is_ok() && !policy.is_planning())
    }

    pub fn task(&self) -> Option<&crate::agent::todo::TaskContext> {
        self.0.task.as_ref()
    }

    /// Capture only an explicitly requested exact native file, before reading its bytes.
    /// Relative requirements are resolved against the tool's pinned root, never process cwd.
    pub(crate) fn capture_task_observation(
        &self,
        path: &std::path::Path,
        root: &std::path::Path,
    ) -> Option<TaskObservation> {
        let task = self.task()?;
        let absolute = path.to_str()?;
        let relative = path.strip_prefix(root).ok().and_then(|path| path.to_str());
        for target in std::iter::once(absolute).chain(relative) {
            if let Some(token) = task.observation_capture(target) {
                return Some(TaskObservation {
                    task: task.clone(),
                    token,
                    target: target.to_owned(),
                    path: path.to_path_buf(),
                });
            }
            let portable = target.replace('\\', "/");
            if portable != target {
                if let Some(token) = task.observation_capture(&portable) {
                    return Some(TaskObservation {
                        task: task.clone(),
                        token,
                        target: portable,
                        path: path.to_path_buf(),
                    });
                }
            }
        }
        None
    }

    pub fn with_design(&self, design: crate::agent::design::DesignContext) -> Self {
        self.with(|i| i.design = Some(design))
    }

    pub fn design(&self) -> Option<crate::agent::design::DesignContext> {
        self.0.design.clone()
    }

    /// Only the controller seats a grant. A changed resource scope cannot read inherited references.
    pub fn with_references(&self, access: crate::core::reference_media::ReferenceAccess) -> Self {
        self.with(|i| i.references = Some(access))
    }

    pub fn without_references(&self) -> Self {
        self.with(|i| i.references = None)
    }

    pub fn references(&self) -> Option<crate::core::reference_media::ReferenceAccess> {
        self.references_checked().ok().flatten()
    }

    /// Unlike optional lookup, preserves revoked/expired selection as an actionable failure.
    pub fn references_checked(
        &self,
    ) -> anyhow::Result<Option<crate::core::reference_media::ReferenceAccess>> {
        if let Some(access) = &self.0.references {
            access.validate_scope(&self.conversation().to_string(), &self.resource_scope())?;
            Ok(Some(access.clone()))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn media(&self) -> Arc<crate::core::media_budget::MediaBudget> {
        self.0.media.clone()
    }

    pub(crate) fn visual(
        &self,
    ) -> std::sync::MutexGuard<'_, crate::core::visual_context::VisualContext> {
        self.0.visual.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn visual_requirements(&self) -> crate::core::visual_context::VisualRequirements {
        let mut required = crate::core::visual_context::VisualRequirements::default();
        if let Some(design) = self.design() {
            let state = design.snapshot();
            if state.active && !state.archived {
                if state.comparison_request.is_some() {
                    required.references = state
                        .references
                        .iter()
                        .filter(|reference| reference.original_sha256.is_some())
                        .map(|reference| reference.reference_id.clone())
                        .collect();
                }
                let scope = design.evidence_scope();
                required.captures = state
                    .evidence
                    .iter()
                    .filter(|evidence| {
                        evidence.owner_id == state.owner_id
                            && evidence.task_id == state.task_id
                            && evidence.render_revision == state.render_revision
                            && evidence.source_hash.is_some()
                            && evidence.source_hash == state.source_hash
                            && state.target.as_deref() == Some(evidence.target.as_str())
                            && evidence.ready
                            && !matches!(
                                evidence.status,
                                crate::agent::design::EvidenceStatus::Stale
                                    | crate::agent::design::EvidenceStatus::Unavailable
                                    | crate::agent::design::EvidenceStatus::SkippedByUser
                            )
                    })
                    .filter_map(|evidence| {
                        evidence.capture_identity.as_ref().map(|identity| {
                            (
                                evidence.id.clone(),
                                identity.output_sha256.clone(),
                                scope.clone(),
                            )
                        })
                    })
                    .collect();
            }
        }
        required
    }

    pub(crate) fn visual_pins(
        &self,
        messages: &[crate::core::types::Message],
    ) -> std::collections::BTreeSet<(usize, bool, usize)> {
        let required = self.visual_requirements();
        self.visual().pinned_occurrences(messages, &required)
    }

    pub(crate) fn refresh_visual(
        &self,
        messages: &mut [crate::core::types::Message],
    ) -> anyhow::Result<()> {
        let refs = self.references();
        let required = self.visual_requirements();
        let retained = self
            .0
            .tool_images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut visual = self.visual();
        visual.refresh_required(messages, refs.as_ref(), &retained, &required)?;
        visual.sync(messages);
        Ok(())
    }

    /// Replace the local snapshot from completed groups, retaining compacted originals only when
    /// the same live scope already owned their reread handles. Saved boundaries grant no access.
    pub(crate) fn refresh_tool_images(&self, messages: &[crate::core::types::Message]) {
        let mut images = Vec::new();
        let mut bytes = 0usize;
        if let Some(design) = self.design() {
            let scope = design.evidence_scope();
            let pins = self.visual_pins(messages);
            let live = self
                .0
                .tool_images
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut occurrences: Vec<_> =
                crate::llm::media::completed_tool_image_occurrences(messages)
                    .into_iter()
                    .map(|image| (image.message_index, image.image_index))
                    .collect();
            for (mi, message) in messages.iter().enumerate() {
                if message.role != "system"
                    || !message.content.as_deref().is_some_and(|content| {
                        content.starts_with(crate::agent::compact::COMPACT_MARKER_PREFIX)
                    })
                {
                    continue;
                }
                for (ii, image) in message.tool_images.iter().enumerate() {
                    if message
                        .visual_context
                        .iter()
                        .any(|state| state.matches_tool(ii, image) && state.releases_pixels())
                        && live
                            .iter()
                            .any(|candidate| candidate.metadata() == image.metadata())
                    {
                        occurrences.push((mi, ii));
                    }
                }
            }
            occurrences.sort_unstable();
            occurrences.reverse();
            occurrences.sort_by_key(|&(mi, ii)| !pins.contains(&(mi, true, ii)));
            for (mi, ii) in occurrences {
                let Some(image) = messages
                    .get(mi)
                    .and_then(|message| message.tool_images.get(ii))
                else {
                    continue;
                };
                let metadata = image.metadata();
                if !image.is_available()
                    || metadata.provenance.design_scope.as_deref() != Some(scope.as_str())
                    || metadata.provenance.source == "reference_read"
                    || metadata.provenance.source == crate::core::reference_media::REFERENCE_SOURCE
                {
                    continue;
                }
                if images.len() >= crate::agent::observe::MAX_TOOL_IMAGES {
                    break;
                }
                let Some(next) = bytes
                    .checked_add(metadata.byte_len)
                    .filter(|n| *n <= crate::agent::observe::MAX_TOOL_IMAGE_BYTES)
                else {
                    continue;
                };
                bytes = next;
                images.push(image.clone());
            }
        }
        *self.0.tool_images.lock().unwrap_or_else(|e| e.into_inner()) = images;
    }

    pub(crate) fn scoped_tool_image(&self, id: &str) -> Option<crate::core::tool_media::ToolImage> {
        let scope = self.design()?.evidence_scope();
        self.0
            .tool_images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|image| {
                let provenance = &image.metadata().provenance;
                provenance.evidence_id.as_deref() == Some(id)
                    && provenance.design_scope.as_deref() == Some(scope.as_str())
                    && provenance.source != "reference_read"
                    && provenance.source != crate::core::reference_media::REFERENCE_SOURCE
                    && image.is_available()
            })
            .cloned()
    }

    /// Pin native file destination roots at the execution boundary. Children explicitly fork this
    /// snapshot with their actual workspace and resource scope; no workspace global is introduced.
    pub fn with_placement(&self, placement: crate::core::file_placement::PlacementContext) -> Self {
        self.with(|i| {
            if !i
                .binding
                .as_ref()
                .is_some_and(|binding| binding.matches_placement(&placement))
            {
                i.binding = Some(WorkspaceBinding::new(&placement));
            }
            i.placement = Some(placement);
        })
    }

    /// Capture workspace identity without retaining a previous turn's authority or admission.
    pub(crate) fn workspace_snapshot(&self) -> Option<WorkspacePlacement> {
        Some(WorkspacePlacement {
            placement: self.placement()?.clone(),
            binding: self.workspace_binding()?.clone(),
        })
    }

    /// A new turn in the same host session inherits the exact token, not a new same-path identity.
    pub(crate) fn with_workspace(&self, workspace: &WorkspacePlacement) -> Self {
        self.with(|i| {
            i.placement = Some(workspace.placement.clone());
            i.binding = Some(workspace.binding.clone());
        })
    }

    pub(crate) fn workspace_binding(&self) -> Option<&WorkspaceBinding> {
        self.0.binding.as_ref()
    }

    pub(crate) fn require_current_binding(
        &self,
        prepared: Option<&WorkspaceBinding>,
    ) -> anyhow::Result<()> {
        // Legacy unbound standalone calls remain supported. Losing an existing bound provenance
        // or moving an unbound prepared call into a bound execution context is not compatible.
        anyhow::ensure!(
            self.workspace_binding() == prepared,
            "prepared workspace binding is stale or missing; prepare again in this context"
        );
        Ok(())
    }

    pub fn placement(&self) -> Option<&crate::core::file_placement::PlacementContext> {
        self.0.placement.as_ref()
    }

    /// Retain an already-admitted controller owner with inspection/preparation workers. This changes
    /// no placement, binding, policy, native scope, writer admission, or restore authority.
    pub(crate) fn with_owned_task_allocation_owner(
        &self,
        owner: Arc<crate::core::git_ops::OwnedTaskAllocationOwner>,
    ) -> Self {
        self.with(|i| i.owned_task_allocation_owner = Some(owner))
    }

    /// A fresh Git checkout does not mutate the parent's tree. Only the controller's native-only
    /// lane may detach these effect collectors; inherited physical/policy guards remain retained.
    pub(crate) fn for_owned_git_task(
        &self,
        task: Arc<crate::core::git_ops::OwnedTask>,
        scope: crate::core::native_scope::NativeWriteScope,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.native_scope().is_none(),
            "owned task cannot widen an existing lane"
        );
        for resource in scope.resources() {
            anyhow::ensure!(
                resource.path().starts_with(task.root()),
                "owned native authority escapes worktree"
            );
        }
        let placement = match self.placement() {
            Some(p) => p.fork_for(task.root().to_path_buf(), task.scope())?,
            None => crate::core::file_placement::PlacementContext::new(
                task.root().to_path_buf(),
                crate::core::config::aizen_home(),
            )?
            .fork_for(task.root().to_path_buf(), task.scope())?,
        };
        Ok(self
            .with_resource_scope(task.scope())
            .without_workspace_restore()
            .with(|i| {
                i.task_effect_sinks.clear();
                i.workspace_activities.clear();
                i.workspace_changes.clear();
                i.plan_write_proof = None;
                i.binding = Some(WorkspaceBinding::new(&placement));
                i.placement = Some(placement);
                i.native_scope = Some(scope);
                i.owned_git_task = Some(task);
            }))
    }

    /// Check the actual live allocation, never a reconstructed path, branch, or owner string.
    pub(crate) fn require_owned_git_task(
        &self,
        task: &Arc<crate::core::git_ops::OwnedTask>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.0
                .owned_git_task
                .as_ref()
                .is_some_and(|bound| Arc::ptr_eq(bound, task)),
            "execution context belongs to another live Git task"
        );
        anyhow::ensure!(
            !self.can_restore_workspace()
                && self.resource_scope() == task.scope()
                && self
                    .placement()
                    .is_some_and(|p| p.workspace_root() == task.root()),
            "Git task context lost its exact native ownership binding"
        );
        Ok(())
    }

    pub(crate) fn has_owned_git_task(&self) -> bool {
        self.0.owned_git_task.is_some()
    }

    pub(crate) fn with_workspace_changes(&self, changes: Arc<WorkspaceChanges>) -> Self {
        self.with(|i| {
            if !i
                .workspace_changes
                .iter()
                .any(|existing| Arc::ptr_eq(existing, &changes))
            {
                i.workspace_changes.push(changes);
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn with_native_body_probe(
        &self,
        probe: crate::agent::builtin::NativeBodyTestProbe,
    ) -> Self {
        self.with(|i| i.native_body_probe = Some(probe))
    }

    #[cfg(test)]
    pub(crate) fn native_body_probe(&self) -> Option<&crate::agent::builtin::NativeBodyTestProbe> {
        self.0.native_body_probe.as_ref()
    }

    /// Invalidate completion proof for each admitted opaque effect or actual native mutation.
    /// This is ordered (every call advances the epoch), unlike the sticky path collector below.
    pub(crate) fn note_task_effects(&self) {
        let root = self
            .placement()
            .map(|placement| placement.workspace_root().to_path_buf())
            .or_else(|| std::env::current_dir().ok());
        self.note_task_effects_at(root.as_deref());
    }

    pub(crate) fn note_task_effects_at(&self, root: Option<&std::path::Path>) {
        if let Some(task) = self.task() {
            task.invalidate_effects_at(root);
        }
        for sink in &self.0.task_effect_sinks {
            if self.task().is_none_or(|task| !task.same_owner(sink)) {
                sink.invalidate_effects_at(root);
            }
        }
    }

    pub(crate) fn with_plan_write_proof(
        &self,
        proof: Arc<crate::agent::planning_write::PlanWriteProof>,
    ) -> Self {
        self.with(|i| i.plan_write_proof = Some(proof))
    }

    pub(crate) fn note_native_mutation(&self, paths: &[std::path::PathBuf]) {
        if self
            .0
            .plan_write_proof
            .as_ref()
            .is_some_and(|proof| proof.record_mutation(self, paths))
        {
            if let Some(task) = self.task() {
                task.invalidate_plan_write_effects();
            }
            for sink in &self.0.task_effect_sinks {
                if self.task().is_none_or(|task| !task.same_owner(sink)) {
                    sink.invalidate_plan_write_effects();
                }
            }
            self.note_workspace_changes(paths);
            return;
        }
        // Writes can land outside placement, or in several projects at once (for example a move).
        // Preserve every actual verification root instead of recording an unresolvable unknown or
        // letting the last root stand in for all earlier effects. Deleted targets still have a parent.
        let mut roots = std::collections::BTreeSet::new();
        for path in paths {
            let root = std::path::absolute(path).ok().and_then(|path| {
                let parent = path.parent()?;
                let root = crate::agent::verify_gate::verify_root(parent)
                    .unwrap_or_else(|| parent.to_path_buf());
                Some(root.canonicalize().unwrap_or(root))
            });
            roots.insert(root);
        }
        if roots.is_empty() {
            roots.insert(None);
        }
        for root in roots {
            self.note_task_effects_at(root.as_deref());
        }
        self.note_workspace_changes(paths);
    }

    pub(crate) fn note_workspace_changes(&self, paths: &[std::path::PathBuf]) {
        for changes in &self.0.workspace_changes {
            changes.record(paths);
        }
    }

    pub(crate) fn with_workspace_activity(
        &self,
        activity: Arc<crate::core::workspace_txn::WorkspaceActivity>,
    ) -> Self {
        self.with(|i| i.workspace_activities.push(activity))
    }

    pub(crate) fn workspace_activities(
        &self,
    ) -> Vec<Arc<crate::core::workspace_txn::WorkspaceActivity>> {
        self.0.workspace_activities.clone()
    }

    pub(crate) fn with_physical_effect_lease(
        &self,
        lease: crate::core::workspace_txn::PhysicalEffectLease,
    ) -> Self {
        self.with(|i| i.physical_effect_leases.push(lease))
    }

    pub(crate) fn writer_leases(&self) -> Vec<crate::core::workspace_txn::WorkspaceWriterLease> {
        self.0.writer_leases.clone()
    }

    pub(crate) fn with_native_scope(
        &self,
        scope: crate::core::native_scope::NativeWriteScope,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = &self.0.native_scope {
            parent.check_narrowing(&scope)?;
        }
        Ok(self.with(|i| i.native_scope = Some(scope)))
    }

    pub(crate) fn native_scope(&self) -> Option<&crate::core::native_scope::NativeWriteScope> {
        self.0.native_scope.as_ref()
    }

    /// Delegation revokes workspace restore authority; builders and retries cannot regrant it.
    pub fn without_workspace_restore(&self) -> Self {
        self.with(|i| {
            i.restore_authorized = false;
            i.recovery = i.recovery.without_workspace_restore();
        })
    }

    pub fn can_restore_workspace(&self) -> bool {
        self.0.restore_authorized
    }

    pub fn recovery(&self) -> &crate::core::run_recovery::RunRecovery {
        &self.0.recovery
    }

    pub fn with_recovery(&self, recovery: crate::core::run_recovery::RunRecovery) -> Self {
        self.with(|i| {
            i.restore_authorized &= recovery.can_restore_workspace();
            i.recovery = if i.restore_authorized {
                recovery
            } else {
                recovery.without_workspace_restore()
            };
        })
    }

    pub fn fork_recovery(&self) -> Self {
        self.with_recovery(crate::core::run_recovery::RunRecovery::default())
    }

    /// The turn's approval route, if any.
    pub fn approval_route(&self) -> Option<ApprovalRoute> {
        self.0.approval_route.clone()
    }
}

impl Default for ExecutionContext {
    /// The shared `"default"` identity — right for a one-shot CLI run or any turn with no persistent
    /// conversation thread. Matches the fallback [`crate::core::convo::active`] uses.
    fn default() -> Self {
        Self::new(ConversationId::new("default"))
    }
}

thread_local! {
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "initializer is already const; Clippy 1.99 reports this macro invocation"
    )]
    /// Stack of contexts active on THIS thread. A stack (not a single slot) so a nested
    /// `with_current` — e.g. a tool that re-enters the agent loop — restores the outer context on
    /// exit, exactly like [`crate::core::cancel`]'s token stack.
    static CURRENT: RefCell<Vec<ExecutionContext>> = const { RefCell::new(Vec::new()) };
}

/// Run synchronous tool code with `ctx` as the active per-turn context, restored on return (even on
/// panic, via the `Drop` guard). Must wrap the tool body INSIDE the `spawn_blocking` closure — a
/// context pushed on the driver thread is invisible on the worker thread the body actually runs on.
pub fn with_current<T>(ctx: ExecutionContext, f: impl FnOnce() -> T) -> T {
    // A nested shared-body caller cannot erase the originating restriction by constructing a
    // default/private context. Preserve its frozen view (including staleness), never reread a
    // newer owner state that could widen permission.
    let parent = current();
    let inherited = crate::agent::planning::current_policy()
        .filter(|policy| policy.is_planning() || policy.validate_current().is_err());
    let ctx = if let Some(policy) = inherited {
        ctx.with(|inner| {
            let own_admission = inner
                .policy
                .as_ref()
                .filter(|own| policy.check_inheritance(own).is_ok())
                .and_then(|_| inner.policy_lease.clone());
            inner.policy = Some(policy.clone());
            inner.policy_lease = parent
                .as_ref()
                .and_then(|parent| parent.0.policy_lease.clone())
                .or(own_admission);
            if let Some(task) = inner.task.take() {
                inner.task = Some(task.with_control(policy.context()));
            }
        })
    } else {
        ctx
    };
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            CURRENT.with(|slot| {
                slot.borrow_mut().pop();
            });
        }
    }

    CURRENT.with(|slot| slot.borrow_mut().push(ctx));
    let _pop = Pop;
    f()
}

/// Bind a captured context on each poll, never across an await on a shared runtime thread.
/// The future owns the context (and any admission) even if its original observer returns early.
pub(crate) fn with_current_future<F: std::future::Future>(
    ctx: ExecutionContext,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    // Box before returning the wrapper, not on its first poll: an async fn would retain the
    // entire unpolled child inline through nested agent/tool/runtime frames. Construction still
    // neither polls the child nor binds CURRENT. The small async shell keeps the original
    // completion behavior: Ready drops the inner future and context even if the shell is retained.
    let mut future = Box::pin(future);
    async move {
        std::future::poll_fn(move |cx| with_current(ctx.clone(), || future.as_mut().poll(cx))).await
    }
}

/// Controller-only synchronous completion of focus inside the currently executing REPL poll.
/// The enclosing `with_current` still restores its caller on return. A queued ready submission may
/// run before the next poll, so updating only the session handle would otherwise leave it on A.
pub(crate) fn rebind_current_workspace(workspace: &WorkspacePlacement) {
    CURRENT.with(|slot| {
        if let Some(context) = slot.borrow_mut().last_mut() {
            *context = context.with_workspace(workspace);
        }
    });
}

/// The context of the currently executing synchronous tool body, if any. `None` on a thread with no
/// pinned turn (e.g. a caller reading before any `with_current` wrap) — callers fall back to their
/// process-global slot.
pub fn current() -> Option<ExecutionContext> {
    CURRENT.with(|slot| slot.borrow().last().cloned())
}

/// Actual synchronous native mutation, called before fallible post-write bookkeeping.
pub(crate) fn note_native_mutation(path: &std::path::Path) {
    if let Some(ctx) = current() {
        ctx.note_native_mutation(&[path.to_path_buf()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_future_large_unpolled_body_stays_out_of_wrapper_state() {
        struct LargeFuture {
            _padding: [u8; 256 * 1024],
        }
        impl std::future::Future for LargeFuture {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                std::task::Poll::Ready(())
            }
        }
        fn future_type_bytes<F: std::future::Future>(
            _factory: fn(ExecutionContext, LargeFuture) -> F,
        ) -> usize {
            std::mem::size_of::<F>()
        }
        // Measure the type only. Constructing the deliberately large input on the test stack
        // would conflate the regression with the stack space needed by the fixture itself.
        let bytes = future_type_bytes(with_current_future::<LargeFuture>);
        assert!(bytes <= 1024, "unpolled context wrapper is {bytes} bytes");
    }

    struct ContextFutureProbe {
        polls: Arc<std::sync::atomic::AtomicUsize>,
        drops: Arc<std::sync::atomic::AtomicUsize>,
        dropped_under: Arc<std::sync::Mutex<Option<String>>>,
        ready: bool,
    }

    impl std::future::Future for ContextFutureProbe {
        type Output = ();
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            assert_eq!(current().unwrap().resource_scope(), "eager-child");
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.ready {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        }
    }

    impl Drop for ContextFutureProbe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.dropped_under.lock().unwrap() = current().map(|ctx| ctx.resource_scope());
        }
    }

    #[test]
    fn current_future_unpolled_drop_keeps_carrier_without_binding_or_polling() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = AttemptCarrierRegistry::new();
        let child = ExecutionContext::default()
            .with_resource_scope("eager-child")
            .with_attempt_carrier_registry(&registry);
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let dropped_under = Arc::new(std::sync::Mutex::new(None));
        with_current(
            ExecutionContext::default().with_resource_scope("outer"),
            || {
                let run = with_current_future(
                    child,
                    ContextFutureProbe {
                        polls: polls.clone(),
                        drops: drops.clone(),
                        dropped_under: dropped_under.clone(),
                        ready: false,
                    },
                );
                assert_eq!(polls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert_eq!(
                    registry.live(),
                    1,
                    "unpolled wrapper retains executable authority"
                );
                assert_eq!(current().unwrap().resource_scope(), "outer");
                drop(run);
                assert_eq!(polls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(
                    registry.live(),
                    0,
                    "dropping the unpolled wrapper retires its carrier"
                );
                assert_eq!(current().unwrap().resource_scope(), "outer");
            },
        );
        assert_eq!(dropped_under.lock().unwrap().as_deref(), Some("outer"));
    }

    #[test]
    fn current_future_pending_polls_rebind_and_restore_without_releasing_carrier() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = AttemptCarrierRegistry::new();
        let child = ExecutionContext::default()
            .with_resource_scope("eager-child")
            .with_attempt_carrier_registry(&registry);
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let dropped_under = Arc::new(std::sync::Mutex::new(None));
        let mut run = Box::pin(with_current_future(
            child,
            ContextFutureProbe {
                polls: polls.clone(),
                drops: drops.clone(),
                dropped_under: dropped_under.clone(),
                ready: false,
            },
        ));
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        for scope in ["first-observer", "second-observer"] {
            with_current(
                ExecutionContext::default().with_resource_scope(scope),
                || {
                    assert!(std::future::Future::poll(run.as_mut(), &mut cx).is_pending());
                    assert_eq!(current().unwrap().resource_scope(), scope);
                    assert_eq!(registry.live(), 1);
                    assert_eq!(drops.load(Ordering::SeqCst), 0);
                },
            );
        }
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        with_current(
            ExecutionContext::default().with_resource_scope("drop-observer"),
            || {
                drop(run);
                assert_eq!(current().unwrap().resource_scope(), "drop-observer");
            },
        );
        assert_eq!(registry.live(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            dropped_under.lock().unwrap().as_deref(),
            Some("drop-observer")
        );
    }

    #[test]
    fn current_future_ready_releases_inner_capture_even_if_completed_shell_is_retained() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = AttemptCarrierRegistry::new();
        let child = ExecutionContext::default()
            .with_resource_scope("eager-child")
            .with_attempt_carrier_registry(&registry);
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let dropped_under = Arc::new(std::sync::Mutex::new(None));
        let mut run = Box::pin(with_current_future(
            child,
            ContextFutureProbe {
                polls: polls.clone(),
                drops: drops.clone(),
                dropped_under: dropped_under.clone(),
                ready: true,
            },
        ));
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        with_current(
            ExecutionContext::default().with_resource_scope("outer"),
            || {
                assert!(std::future::Future::poll(run.as_mut(), &mut cx).is_ready());
                assert_eq!(polls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    1,
                    "Ready retires the inner future before shell drop"
                );
                assert_eq!(
                    registry.live(),
                    0,
                    "Ready retires the owned context before shell drop"
                );
                assert_eq!(current().unwrap().resource_scope(), "outer");
                assert_eq!(dropped_under.lock().unwrap().as_deref(), Some("outer"));
                drop(run);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            },
        );
    }

    #[test]
    fn current_future_panicking_poll_matches_original_capture_retirement() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Exact pre-eager async wrapper body. A caught poll panic unwinds its inner poll_fn,
        // even while the caller retains the outer future. Compare that contract, not the
        // different lifetime of a bare PollFn. This grants no worker join or release proof.
        async fn reference_pre_eager_current_future<F: std::future::Future>(
            ctx: ExecutionContext,
            future: F,
        ) -> F::Output {
            let mut future = Box::pin(future);
            std::future::poll_fn(move |cx| with_current(ctx.clone(), || future.as_mut().poll(cx)))
                .await
        }

        struct PanicProbe {
            polls: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
            dropped_under: Arc<std::sync::Mutex<Option<String>>>,
        }
        impl std::future::Future for PanicProbe {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                assert_eq!(current().unwrap().resource_scope(), "eager-child");
                self.polls.fetch_add(1, Ordering::SeqCst);
                panic!("fixture poll panic");
            }
        }
        impl Drop for PanicProbe {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                *self.dropped_under.lock().unwrap() = current().map(|ctx| ctx.resource_scope());
            }
        }
        fn observe<F: std::future::Future<Output = ()>>(
            factory: impl FnOnce(ExecutionContext, PanicProbe) -> F,
        ) -> (usize, usize, usize, Option<String>, usize) {
            let registry = AttemptCarrierRegistry::new();
            let child = ExecutionContext::default()
                .with_resource_scope("eager-child")
                .with_attempt_carrier_registry(&registry);
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let dropped_under = Arc::new(std::sync::Mutex::new(None));
            let mut run = Box::pin(factory(
                child,
                PanicProbe {
                    polls: polls.clone(),
                    drops: drops.clone(),
                    dropped_under: dropped_under.clone(),
                },
            ));
            assert_eq!(
                registry.live(),
                1,
                "the unpolled wrapper owns its executable capture"
            );
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            let previous = current().map(|ctx| ctx.resource_scope());
            let observed = with_current(
                ExecutionContext::default().with_resource_scope("outer"),
                || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        std::future::Future::poll(run.as_mut(), &mut cx)
                    }));
                    assert!(result.is_err());
                    assert_eq!(current().unwrap().resource_scope(), "outer");
                    let before_shell_drop = (
                        registry.live(),
                        polls.load(Ordering::SeqCst),
                        drops.load(Ordering::SeqCst),
                        dropped_under.lock().unwrap().clone(),
                    );
                    drop(run);
                    assert_eq!(registry.live(), 0);
                    assert_eq!(current().unwrap().resource_scope(), "outer");
                    (
                        before_shell_drop.0,
                        before_shell_drop.1,
                        before_shell_drop.2,
                        before_shell_drop.3,
                        drops.load(Ordering::SeqCst),
                    )
                },
            );
            assert_eq!(current().map(|ctx| ctx.resource_scope()), previous);
            observed
        }
        let original = observe(reference_pre_eager_current_future::<PanicProbe>);
        let eager = observe(with_current_future::<PanicProbe>);
        assert_eq!(
            eager, original,
            "eager allocation must preserve async unwind ownership"
        );
        assert_eq!(
            original,
            (0, 1, 1, Some("outer".into()), 1),
            "the inner body and executable capture retire exactly once under the restored observer"
        );
    }

    #[test]
    fn test_lock_namespace_survives_child_worker_until_physical_completion() {
        use crate::core::workspace_txn::{
            TestLockNamespace, WorkspaceIdentity, WorkspaceWriterLease,
        };
        use std::time::Duration;

        let namespace = TestLockNamespace::owned();
        let root = namespace.root().to_path_buf();
        let mut run = Box::pin(crate::core::workspace_txn::with_test_lock_namespace_future(
            Some(namespace),
            async { ExecutionContext::default() },
        ));
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let std::task::Poll::Ready(parent) = std::future::Future::poll(run.as_mut(), &mut cx)
        else {
            panic!("context creation must complete in one poll");
        };
        drop(run);
        assert!(
            root.is_dir(),
            "the context created during polling must retain the namespace owner"
        );
        let child = parent
            .with_resource_scope("parent/task")
            .with_trace_visible(false)
            .fork_recovery();
        let worker_root = root.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            with_current(child, || {
                assert_eq!(
                    current().unwrap().test_lock_namespace().unwrap().root(),
                    worker_root.as_path()
                );
                let identity = WorkspaceIdentity {
                    repo_id: "namespace-worker-repo".into(),
                    worktree_id: "namespace-worker-tree".into(),
                    canonical_root: worker_root.clone(),
                    canonical_git_dir: None,
                    canonical_common_git_dir: None,
                };
                assert!(identity.lock_root().starts_with(&worker_root));
                let lease = WorkspaceWriterLease::acquire_identity(
                    identity,
                    Duration::ZERO,
                    None,
                    "namespace worker regression",
                )
                .unwrap();
                ready.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(worker_root.is_dir());
                drop(lease);
            });
        });
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(parent);
        assert!(root.is_dir(), "the running worker still owns the fixture");
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            !root.exists(),
            "the final context releases its owned fixture"
        );
    }

    #[test]
    fn attempt_carriers_track_context_forks_until_all_handles_retire() {
        let registry = AttemptCarrierRegistry::new();
        let mut ctx = ExecutionContext::default().with_attempt_carrier_registry(&registry);
        assert_eq!(registry.live(), 1);
        let retained = ctx.clone();
        assert_eq!(registry.live(), 1, "an Arc clone retains the same carrier");
        ctx = ctx.with_trace_visible(false);
        assert_eq!(registry.live(), 2, "a context fork owns a distinct carrier");
        ctx = ctx.with_dispatch_label(Some("owned-attempt".into()));
        assert_eq!(
            registry.live(),
            2,
            "assignment retires the replaced context fork"
        );
        drop(ctx);
        assert_eq!(
            registry.live(),
            1,
            "the earlier executable handle still owns authority"
        );
        drop(retained);
        assert_eq!(registry.live(), 0);
    }

    #[tokio::test]
    async fn attempt_carrier_timeout_is_unknown_worker_until_last_capture_retires() {
        let registry = AttemptCarrierRegistry::new();
        let ctx = ExecutionContext::default().with_attempt_carrier_registry(&registry);
        let prepared_capture = ctx.with_trace_visible(false);
        drop(ctx);
        let error = registry
            .wait_empty(tokio::time::Instant::now())
            .await
            .unwrap_err();
        assert_eq!(
            crate::agent::tool_error_effect_state(&error),
            crate::agent::tools::ToolEffectState::UnknownWorker
        );
        assert!(error.to_string().contains("remain live (1)"));
        drop(prepared_capture);
        registry
            .wait_empty(tokio::time::Instant::now())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attempt_carriers_wait_for_actual_capture_drop_without_losing_wakeup() {
        for _ in 0..64 {
            let registry = AttemptCarrierRegistry::new();
            let ctx = ExecutionContext::default().with_attempt_carrier_registry(&registry);
            let (release, released) = tokio::sync::oneshot::channel();
            let retirement = tokio::spawn(async move {
                released.await.unwrap();
                drop(ctx);
            });
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let waiting = registry.wait_empty(deadline);
            tokio::pin!(waiting);
            // Poll first with the carrier retained, then race retirement with the next poll.
            assert!(futures_util::poll!(&mut waiting).is_pending());
            assert_eq!(registry.live(), 1);
            release.send(()).unwrap();
            waiting.await.unwrap();
            tokio::time::timeout_at(deadline, retirement)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(registry.live(), 0);
        }
    }

    #[test]
    fn delegated_changes_reach_parent_without_leaking_to_siblings() {
        let parent_changes = Arc::new(WorkspaceChanges::default());
        let a_changes = Arc::new(WorkspaceChanges::default());
        let b_changes = Arc::new(WorkspaceChanges::default());
        let parent = ExecutionContext::default().with_workspace_changes(parent_changes.clone());
        let a = parent
            .with_resource_scope("a")
            .with_workspace_changes(a_changes.clone());
        let b = parent
            .with_resource_scope("b")
            .with_workspace_changes(b_changes.clone());
        let a_file = std::path::PathBuf::from("a.txt");
        let b_file = std::path::PathBuf::from("b.txt");
        assert!(!a_changes.changed());
        a.note_workspace_changes(std::slice::from_ref(&a_file));
        a.note_workspace_changes(std::slice::from_ref(&a_file));
        assert_eq!(a_changes.paths(), std::slice::from_ref(&a_file));
        assert!(a_changes.changed());
        assert!(!b_changes.changed());
        b.note_workspace_changes(std::slice::from_ref(&b_file));
        assert_eq!(b_changes.paths(), std::slice::from_ref(&b_file));
        assert_eq!(parent_changes.paths(), [a_file, b_file]);
        assert!(parent_changes.changed());
    }

    #[test]
    fn task_effect_epochs_reach_ancestors_without_sharing_checklist_owners() {
        let task = crate::agent::todo::TaskContext::private();
        let parent = ExecutionContext::default().with_task(task.clone());
        let child = parent.with_resource_scope("child");
        let sibling = parent.with_resource_scope("sibling");
        let grandchild = child.with_resource_scope("grandchild");
        assert!(!task.same_owner(child.task().unwrap()));
        assert!(!child.task().unwrap().same_owner(sibling.task().unwrap()));
        let base = task.effect_epoch();
        let child_base = child.task().unwrap().effect_epoch();
        let sibling_base = sibling.task().unwrap().effect_epoch();
        let path = std::path::PathBuf::from("same.txt");
        grandchild.note_native_mutation(std::slice::from_ref(&path));
        grandchild.note_native_mutation(std::slice::from_ref(&path));
        assert_eq!(task.effect_epoch(), base + 2);
        assert_eq!(child.task().unwrap().effect_epoch(), child_base + 2);
        assert_eq!(sibling.task().unwrap().effect_epoch(), sibling_base);
        // Attribution-only calls do not invent another mutation or turn a no-op into a write.
        child.note_workspace_changes(&[path]);
        assert_eq!(task.effect_epoch(), base + 2);
        // An explicitly reattached same owner is deduplicated from the inherited sink list.
        child.with_task(task.clone()).note_task_effects();
        assert_eq!(task.effect_epoch(), base + 3);
    }

    #[test]
    fn native_mutation_preserves_actual_roots_with_or_without_placement() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let base = fixture.path().canonicalize().unwrap();
        let a = base.join("project-a");
        let b = base.join("project-b");
        let plain = base.join("plain");
        for root in [&a, &b] {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        }
        std::fs::create_dir_all(plain.join(".aizen")).unwrap();
        // Bound discovery to this unsupported fixture rather than any manifest in the tester's
        // home directory above the system temporary directory. No runnable command is provided.
        std::fs::write(plain.join(".aizen/verify.json"), "{}").unwrap();
        let expected: std::collections::BTreeSet<_> = [&a, &b, &plain]
            .into_iter()
            .map(|root| root.canonicalize().unwrap())
            .collect();
        for placement in [false, true] {
            let task = crate::agent::todo::TaskContext::private();
            let mut ctx = ExecutionContext::default().with_task(task.clone());
            if placement {
                ctx = ctx.with_placement(
                    crate::core::file_placement::PlacementContext::with_paths(
                        a.clone(),
                        base.join("home"),
                        base.join("scratch"),
                        "mutation-roots",
                    )
                    .unwrap(),
                );
            }
            let epoch = task.effect_epoch();
            // Targets may already have been deleted; parent roots must remain verifiable. Two paths
            // in one project share a root, but an outside-project effect cannot collapse into it.
            ctx.note_native_mutation(&[
                a.join("src/a.rs"),
                a.join("src/deleted.rs"),
                b.join("src/b.rs"),
                plain.join("deleted.txt"),
            ]);
            assert_eq!(task.effect_epoch(), epoch + 3);
            let roots: std::collections::BTreeSet<_> = task
                .snapshot()
                .obligations
                .iter()
                .filter(|obligation| obligation.item_id.is_empty())
                .map(|obligation| {
                    std::path::PathBuf::from(obligation.root.as_ref().expect("known native root"))
                        .canonicalize()
                        .unwrap()
                })
                .collect();
            assert_eq!(roots, expected);
        }
    }

    #[test]
    fn opaque_effect_has_runtime_root_but_explicit_unknown_stays_unknown() {
        let task = crate::agent::todo::TaskContext::private();
        ExecutionContext::default()
            .with_task(task.clone())
            .note_task_effects();
        let state = task.snapshot();
        assert_eq!(state.obligations.len(), 1);
        let root = state.obligations[0].root.as_ref().unwrap();
        assert_eq!(
            std::path::Path::new(root).canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        let unknown = crate::agent::todo::TaskContext::private();
        ExecutionContext::default()
            .with_task(unknown.clone())
            .note_task_effects_at(None);
        let state = unknown.snapshot();
        assert_eq!(state.obligations.len(), 1);
        assert!(state.obligations[0].root.is_none());
    }

    #[test]
    fn native_observation_capture_is_owner_and_epoch_scoped() {
        use crate::agent::todo::{Requirement, Status, TaskContext, Todo};
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let root = fixture.path().canonicalize().unwrap();
        let path = root.join("read.txt");
        std::fs::write(&path, b"native bytes").unwrap();
        let task = TaskContext::private();
        let rows = task
            .replace(vec![Todo::new("observe file", Status::Done)])
            .unwrap();
        let id = rows[0].id.clone();
        task.replace_with_requirements(
            rows,
            vec![(
                id,
                Requirement::Observe {
                    target: "read.txt".into(),
                },
            )],
        )
        .unwrap();
        let parent = ExecutionContext::default().with_task(task.clone());
        assert!(parent
            .with_resource_scope("child")
            .capture_task_observation(&path, &root)
            .is_none());
        assert!(parent
            .capture_task_observation(&root.join("other.txt"), &root)
            .is_none());
        let old = parent.capture_task_observation(&path, &root).unwrap();
        parent.note_task_effects();
        old.record(&crate::core::persist::FileFingerprint::for_bytes(
            b"native bytes",
        ));
        assert!(
            task.snapshot().observations.is_empty(),
            "old epoch cannot mint a receipt"
        );
        parent
            .capture_task_observation(&path, &root)
            .unwrap()
            .record(&crate::core::persist::FileFingerprint::for_bytes(
                b"native bytes",
            ));
        assert_eq!(task.snapshot().observations.len(), 1);
        assert!(
            task.snapshot().checks.is_empty(),
            "observations never become checks"
        );
    }

    #[test]
    fn delegated_restore_authority_cannot_be_regranted_by_context_builders() {
        let parent = ExecutionContext::new(ConversationId::new("restore-authority"));
        assert!(parent.can_restore_workspace());
        let child = parent
            .without_workspace_restore()
            .with_resource_scope("restore-authority/task/a")
            .fork_recovery();
        assert!(!child.can_restore_workspace());
        assert!(!child.recovery().can_restore_workspace());
        let reset = child.with_recovery(crate::core::run_recovery::RunRecovery::default());
        assert!(!reset.can_restore_workspace());
        assert!(!reset.recovery().can_restore_workspace());
        assert!(!reset.fork_recovery().recovery().can_restore_workspace());
        assert!(!child
            .with_dispatch_label(None)
            .with_trace_visible(true)
            .with_resource_scope("restore-authority/task/a/task/b")
            .fork_recovery()
            .can_restore_workspace());
        assert!(parent.can_restore_workspace());
        with_current(parent.clone(), || {
            assert!(current().unwrap().can_restore_workspace());
            with_current(child, || {
                assert!(!current().unwrap().can_restore_workspace());
            });
            assert!(current().unwrap().can_restore_workspace());
        });
    }

    #[test]
    fn deadline_is_inherited_and_can_only_be_tightened() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let parent = ExecutionContext::default().with_deadline(Some(deadline));
        let child = parent
            .with_resource_scope("child")
            .fork_recovery()
            .with_deadline(Some(deadline + std::time::Duration::from_secs(30)));
        assert_eq!(child.deadline(), Some(deadline));
        assert_eq!(child.with_deadline(None).deadline(), Some(deadline));
        let earlier = deadline - std::time::Duration::from_secs(1);
        assert_eq!(child.with_deadline(Some(earlier)).deadline(), Some(earlier));
        assert_eq!(parent.deadline(), Some(deadline));
        assert_eq!(ExecutionContext::default().deadline(), None);
    }

    #[test]
    fn default_is_the_shared_identity() {
        assert_eq!(
            ExecutionContext::default().conversation().as_str(),
            "default"
        );
    }

    #[test]
    fn current_is_none_without_a_pinned_turn() {
        assert!(current().is_none(), "no context pinned on a bare thread");
    }

    #[test]
    fn with_current_pins_and_restores() {
        let outer = ExecutionContext::new(ConversationId::new("repl:main"));
        let inner = ExecutionContext::new(ConversationId::new("telegram:main:42"));
        with_current(outer.clone(), || {
            assert_eq!(current().unwrap().conversation().as_str(), "repl:main");
            with_current(inner.clone(), || {
                assert_eq!(
                    current().unwrap().conversation().as_str(),
                    "telegram:main:42"
                );
            });
            assert_eq!(
                current().unwrap().conversation().as_str(),
                "repl:main",
                "inner context popped on exit, outer restored"
            );
        });
        assert!(
            current().is_none(),
            "outer context popped after the outermost scope"
        );
    }

    #[test]
    fn a_fresh_context_carries_no_overrides() {
        // Every per-turn field defaults to "no override" so the REPL / one-shot CLI keeps reading
        // its process-global exactly as before. This is what makes P0 a no-behavior-change change.
        let ctx = ExecutionContext::new(ConversationId::new("repl:main"));
        assert_eq!(ctx.persona(), None);
        assert_eq!(ctx.approval_route(), None);
        assert_eq!(ctx.resource_scope(), "repl:main");
        assert!(ctx.trace_visible());
    }

    #[test]
    fn delegated_scope_and_trace_visibility_are_isolated() {
        let parent = ExecutionContext::new(ConversationId::new("repl:main"));
        let child = parent
            .with_resource_scope("repl:main/task/7")
            .with_trace_visible(false);
        assert_eq!(parent.resource_scope(), "repl:main");
        assert!(parent.trace_visible());
        assert_eq!(child.resource_scope(), "repl:main/task/7");
        assert!(!child.trace_visible());
        assert_eq!(child.conversation().as_str(), "repl:main");
    }

    #[test]
    fn local_completed_output_lookup_never_leaks_parent_or_sibling_images() {
        use crate::core::types::{FunctionCall, Message, ToolCall};
        let design = crate::agent::design::DesignContext::new(
            "owner",
            "fixture",
            crate::agent::design::DesignMode::Auto,
        );
        design.begin_turn("parent-task", "fixture");
        let scope = design.evidence_scope();
        let ctx = ExecutionContext::new(ConversationId::new("owner")).with_design(design.clone());
        let mut provenance = crate::core::tool_media::ImageProvenance::new("browser_capture", None);
        provenance.design_scope = Some(scope);
        provenance.evidence_id = Some("output".into());
        let bytes = crate::core::tool_media::encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap();
        let image = crate::core::tool_media::ToolImage::from_bytes(&bytes, "image/png", provenance)
            .unwrap();
        let mut assistant = Message::user("unused");
        assistant.role = "assistant".into();
        assistant.tool_calls = vec![ToolCall {
            id: "capture-call".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "browser_capture".into(),
                arguments: "{}".into(),
            },
        }];
        let mut output = Message::tool_result("capture-call", "captured");
        output.tool_images = vec![image];
        ctx.refresh_tool_images(&[assistant.clone(), output.clone()]);
        assert!(ctx.scoped_tool_image("output").is_some());
        assert!(ctx
            .with_resource_scope("child")
            .scoped_tool_image("output")
            .is_none());
        let child_design = design.fork_child("child", "fixture");
        let child = ctx.with_resource_scope("child").with_design(child_design);
        child.refresh_tool_images(&[assistant.clone(), output.clone()]);
        assert!(child.scoped_tool_image("output").is_none());
        output.tool_call_id = Some("unmatched".into());
        ctx.refresh_tool_images(&[assistant, output]);
        assert!(ctx.scoped_tool_image("output").is_none());
    }

    #[test]
    fn live_compacted_capture_reread_survives_without_restoring_saved_authority() {
        use crate::core::tool_media::{encode_rgba_png, ImageProvenance, ToolImage};
        use crate::core::types::Message;
        let design = crate::agent::design::DesignContext::new(
            "owner",
            "fixture",
            crate::agent::design::DesignMode::Auto,
        );
        design.begin_turn("task", "fixture");
        let mut provenance = ImageProvenance::new("image_read", None);
        provenance.design_scope = Some(design.evidence_scope());
        provenance.evidence_id = Some("compact-output".into());
        provenance.approved_destination = Some("a".repeat(64));
        let image = ToolImage::from_bytes(
            &encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
            "image/png",
            provenance,
        )
        .unwrap();
        let ctx = ExecutionContext::new(ConversationId::new("owner")).with_design(design.clone());
        let mut messages = vec![Message::system("SYS")];
        messages.extend(crate::llm::media::tests::observed_group(image));
        ctx.refresh_tool_images(&messages);
        ctx.refresh_visual(&mut messages).unwrap();
        let id = messages[2].visual_context[0].id.clone();
        let (_, receipt) = crate::llm::media::project_messages_for_send(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&"a".repeat(64)),
            crate::llm::image_send::ImageSendPolicy::Original,
        );
        ctx.visual().record_delivery(&receipt);
        ctx.visual()
            .mutate(&id, "release", Some("Original retained"), None, true)
            .unwrap();
        ctx.refresh_visual(&mut messages).unwrap();
        let cut = messages.len();
        crate::agent::compact::splice_compacted(&mut messages, cut, "summary").unwrap();
        assert!(crate::llm::media::completed_tool_image_occurrences(&messages).is_empty());
        for _ in 0..2 {
            ctx.refresh_tool_images(&messages);
            ctx.refresh_visual(&mut messages).unwrap();
            assert!(ctx.scoped_tool_image("compact-output").is_some());
            assert_eq!(ctx.visual().list()[0]["evidence_id"], "compact-output");
        }
        let restored_messages: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let fresh = ExecutionContext::new(ConversationId::new("owner")).with_design(design.clone());
        fresh.refresh_tool_images(&restored_messages);
        assert!(fresh.scoped_tool_image("compact-output").is_none());
        let sibling = ctx
            .with_resource_scope("sibling")
            .with_design(design.fork_child("sibling", "fixture"));
        sibling.refresh_tool_images(&messages);
        assert!(sibling.scoped_tool_image("compact-output").is_none());
        design.begin_turn("new-task", "fixture");
        ctx.refresh_tool_images(&messages);
        assert!(ctx.scoped_tool_image("compact-output").is_none());
    }

    #[test]
    fn newer_completed_captures_win_quota_over_unpinned_compacted_originals() {
        use crate::core::tool_media::{encode_rgba_png, ImageProvenance, ToolImage};
        use crate::core::types::Message;
        let design = crate::agent::design::DesignContext::new(
            "owner",
            "fixture",
            crate::agent::design::DesignMode::Auto,
        );
        design.begin_turn("task", "fixture");
        let mut provenance = ImageProvenance::new("image_read", None);
        provenance.design_scope = Some(design.evidence_scope());
        provenance.evidence_id = Some("older-compacted".into());
        provenance.approved_destination = Some("a".repeat(64));
        let original = ToolImage::from_bytes(
            &encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
            "image/png",
            provenance,
        )
        .unwrap();
        let ctx = ExecutionContext::new(ConversationId::new("owner")).with_design(design);
        let mut messages = vec![Message::system("SYS")];
        messages.extend(crate::llm::media::tests::observed_group(original.clone()));
        ctx.refresh_tool_images(&messages);
        ctx.refresh_visual(&mut messages).unwrap();
        let id = messages[2].visual_context[0].id.clone();
        let (_, receipt) = crate::llm::media::project_messages_for_send(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&"a".repeat(64)),
            crate::llm::image_send::ImageSendPolicy::Original,
        );
        ctx.visual().record_delivery(&receipt);
        ctx.visual()
            .mutate(&id, "release", Some("Original retained"), None, true)
            .unwrap();
        ctx.refresh_visual(&mut messages).unwrap();
        let cut = messages.len();
        crate::agent::compact::splice_compacted(&mut messages, cut, "summary").unwrap();
        for index in 0..crate::agent::observe::MAX_TOOL_IMAGES {
            let mut provenance = original.metadata().provenance.clone();
            provenance.evidence_id = Some(format!("newer-{index}"));
            let capture =
                ToolImage::from_bytes(&original.bytes().unwrap(), "image/png", provenance).unwrap();
            messages.extend(crate::llm::media::tests::observed_group(capture));
        }
        ctx.refresh_tool_images(&messages);
        assert!(ctx.scoped_tool_image("older-compacted").is_none());
        for index in 0..crate::agent::observe::MAX_TOOL_IMAGES {
            assert!(ctx.scoped_tool_image(&format!("newer-{index}")).is_some());
        }
        assert!(messages.iter().any(|message| message
            .tool_images
            .iter()
            .any(|image| image.metadata() == original.metadata() && image.is_available())));
    }

    #[test]
    fn current_capture_pin_survives_retention_and_expires_with_render_revision() {
        use crate::agent::design::{
            CaptureIdentity, DesignBrief, DesignContext, DesignIntent, DesignMode, Viewport,
        };
        let design = DesignContext::new("owner", "fixture", DesignMode::Auto);
        design.begin_turn("task", "fixture");
        design
            .apply_brief(
                DesignBrief {
                    intent: DesignIntent::Create,
                    brief: "Inspect the current generated fixture".into(),
                    acceptance: "Capture current pixels before review".into(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        design.set_target("owned-fixture").unwrap();
        design.set_source_revision("fixture-source").unwrap();
        let mut provenance =
            crate::core::tool_media::ImageProvenance::new("browser_screenshot", None);
        provenance.evidence_id = Some("required-capture".into());
        provenance.design_scope = Some(design.evidence_scope());
        let image = crate::core::tool_media::ToolImage::from_bytes(
            &crate::core::tool_media::encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
            "image/png",
            provenance,
        )
        .unwrap();
        design
            .record_capture(
                "required-capture",
                Some("fixture-source"),
                "owned-fixture",
                Viewport {
                    width: 1,
                    height: 1,
                },
                design.current_revision(),
                true,
                "Owned fixture",
            )
            .unwrap();
        design
            .record_capture_identity(
                "required-capture",
                CaptureIdentity {
                    output_sha256: image.metadata().sha256.clone(),
                    dpr_milli: 1000,
                    font_identity: "fixture-font".into(),
                    frame_state: "fixture-frame".into(),
                },
            )
            .unwrap();
        let ctx = ExecutionContext::new(ConversationId::new("owner")).with_design(design.clone());
        let mut messages = crate::llm::media::tests::observed_group(image.clone());
        for i in 0..crate::agent::observe::MAX_TOOL_IMAGES + 2 {
            let mut provenance = image.metadata().provenance.clone();
            provenance.evidence_id = Some(format!("optional-{i}"));
            let optional = crate::core::tool_media::ToolImage::from_bytes(
                &image.bytes().unwrap(),
                "image/png",
                provenance,
            )
            .unwrap();
            messages.extend(crate::llm::media::tests::observed_group(optional));
        }
        let pins = ctx.visual_pins(&messages);
        assert_eq!(pins, std::collections::BTreeSet::from([(1, true, 0)]));
        crate::agent::observe::retain_tool_images_with_pins(&mut messages, &pins).unwrap();
        ctx.refresh_tool_images(&messages);
        ctx.refresh_visual(&mut messages).unwrap();
        assert!(ctx.scoped_tool_image("required-capture").is_some());
        assert!(messages[1].visual_context[0].runtime_pinned);
        design.mark_render_changed();
        assert!(
            ctx.visual_pins(&messages).is_empty(),
            "stale revision cannot keep a controller pin"
        );
        ctx.refresh_visual(&mut messages).unwrap();
        assert!(!messages[1].visual_context[0].runtime_pinned);
    }

    #[test]
    fn selected_reference_access_is_never_implicit_in_a_new_task_scope() {
        use crate::core::reference_media::{
            ReferenceDerivatives, ReferenceGrantScope, ReferenceRegistry, ReferenceRoute,
        };
        let bytes = crate::core::tool_media::encode_rgba_png(&[7, 5, 3, 255], 1, 1).unwrap();
        let image = crate::core::tool_media::ToolImage::from_bytes(
            &bytes,
            "image/png",
            crate::core::tool_media::ImageProvenance::new("fixture", None),
        )
        .unwrap();
        let mut messages = vec![crate::core::types::Message::user_with_images(
            "ref",
            vec![image.data_url()],
        )];
        let registry = ReferenceRegistry::new("owner");
        let id = registry.register_loaded(&mut messages).unwrap()[0]
            .reference_id
            .clone();
        let access = registry
            .grant(
                &[id],
                ReferenceGrantScope {
                    owner: "owner".into(),
                    purpose: "match reference".into(),
                    task_scope: "owner".into(),
                    route: ReferenceRoute {
                        destination: "route".into(),
                        account: "account".into(),
                    },
                },
                ReferenceDerivatives::ALL,
            )
            .unwrap();
        let parent =
            ExecutionContext::new(ConversationId::new("owner")).with_references(access.clone());
        assert!(parent.references().is_some());
        assert!(parent
            .with_resource_scope("child")
            .references_checked()
            .is_err());
        assert!(parent
            .without_references()
            .references_checked()
            .unwrap()
            .is_none());
        access.revoke();
        assert!(parent.references_checked().is_err());
        assert!(parent.references().is_none());
    }

    #[test]
    fn each_override_round_trips_and_leaves_the_others_alone() {
        let base = ExecutionContext::new(ConversationId::new("telegram:work:7"));
        let ctx = base
            .with_persona(Some("Aria".into()))
            .with_approval_route(Some(ApprovalRoute {
                route: "work".into(),
                chat: "7".into(),
                sender_id: None,
            }));

        assert_eq!(ctx.persona().as_deref(), Some("Aria"));
        assert_eq!(ctx.approval_route().unwrap().route, "work");
        assert_eq!(ctx.approval_route().unwrap().chat, "7");
        // Chaining must not drop the identity the whole context is keyed on.
        assert_eq!(ctx.conversation().as_str(), "telegram:work:7");
    }

    #[test]
    fn a_builder_does_not_mutate_the_context_already_handed_out() {
        // The context is shared with tool bodies that may already be running. A builder returns a
        // NEW Arc, so a later override can't retroactively change a turn in flight.
        let original = ExecutionContext::new(ConversationId::new("repl:main"));
        let derived = original.with_persona(Some("Aria".into()));
        assert_eq!(original.persona(), None, "original is untouched");
        assert_eq!(derived.persona().as_deref(), Some("Aria"));
    }

    #[test]
    fn two_lane_contexts_stay_independent_on_one_thread() {
        // The concurrency case this whole module exists for: two hosted-bot lanes must never read
        // each other's approval route, or one bot's ✓/✗ prompt lands in the other bot's chat.
        let a = ExecutionContext::new(ConversationId::new("telegram:default:1"))
            .with_approval_route(Some(ApprovalRoute {
                route: "default".into(),
                chat: "1".into(),
                sender_id: None,
            }));
        let b = ExecutionContext::new(ConversationId::new("telegram:work:2")).with_approval_route(
            Some(ApprovalRoute {
                route: "work".into(),
                chat: "2".into(),
                sender_id: None,
            }),
        );
        with_current(a.clone(), || {
            assert_eq!(
                current().unwrap().approval_route().unwrap().route,
                "default"
            );
            with_current(b.clone(), || {
                assert_eq!(current().unwrap().approval_route().unwrap().route, "work");
            });
            assert_eq!(
                current().unwrap().approval_route().unwrap().route,
                "default",
                "lane A's route is restored when lane B's scope ends"
            );
        });
    }
}
