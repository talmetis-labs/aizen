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
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// Immutable destination roots/run scope pinned from AgentConfig::effective_root by executor.
    placement: Option<crate::core::file_placement::PlacementContext>,
    /// Clone within one logical run; explicitly fork for a delegated child or fresh user work.
    recovery: crate::features::timemachine::RunRecovery,
    /// Only the coordinating parent may restore workspace state; delegation revokes this permanently.
    restore_authorized: bool,
    /// Exact file authority, inherited monotonically across retries and nested contexts.
    native_scope: Option<crate::core::native_scope::NativeWriteScope>,
    /// Controller-owned immutable lane binding; changing other context fields cannot remove it.
    overlay: Option<crate::core::speculation::store::OverlayHandle>,
    workspace_activities: Vec<Arc<crate::core::workspace_txn::WorkspaceActivity>>,
    workspace_changes: Vec<Arc<WorkspaceChanges>>,
    #[cfg(test)]
    native_body_probe: Option<crate::agent::builtin::NativeBodyTestProbe>,
    #[cfg(test)]
    speculative_driver_join_probe: Option<crate::agent::tools::workers::WorkerJoinTestProbe>,
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
            placement: None,
            recovery: crate::features::timemachine::RunRecovery::default(),
            restore_authorized: true,
            native_scope: None,
            overlay: None,
            workspace_activities: Vec::new(),
            workspace_changes: Vec::new(),
            #[cfg(test)]
            native_body_probe: None,
            #[cfg(test)]
            speculative_driver_join_probe: None,
            attempt_carrier: None,
            attempt_slot: None,
            tool_workers: None,
            writer_leases: Vec::new(),
            physical_effect_leases: Vec::new(),
            deadline: None,
            references: None,
            tool_images: Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_lock_namespace: crate::core::workspace_txn::explicit_test_lock_namespace(),
        }
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
            placement: self.0.placement.clone(),
            recovery: self.0.recovery.clone(),
            restore_authorized: self.0.restore_authorized,
            native_scope: self.0.native_scope.clone(),
            overlay: self.0.overlay.clone(),
            workspace_activities: self.0.workspace_activities.clone(),
            workspace_changes: self.0.workspace_changes.clone(),
            #[cfg(test)]
            native_body_probe: self.0.native_body_probe.clone(),
            #[cfg(test)]
            speculative_driver_join_probe: self.0.speculative_driver_join_probe.clone(),
            attempt_carrier: self.0.attempt_carrier.as_ref().map(Clone::clone),
            attempt_slot: self.0.attempt_slot.clone(),
            tool_workers: self.0.tool_workers.clone(),
            writer_leases: self.0.writer_leases.clone(),
            physical_effect_leases: self.0.physical_effect_leases.clone(),
            deadline: self.0.deadline,
            references: self.0.references.clone(),
            tool_images: self.0.tool_images.clone(),
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

    /// Replace the local image snapshot from exact completed groups in this run's current messages.
    /// Payload retention applies first; stale/partial groups and references never become outputs.
    pub(crate) fn refresh_tool_images(&self, messages: &[crate::core::types::Message]) {
        let mut images = Vec::new();
        let mut bytes = 0usize;
        if let Some(design) = self.design() {
            let scope = design.evidence_scope();
            for occurrence in crate::llm::media::completed_tool_image_occurrences(messages)
                .into_iter()
                .rev()
            {
                let Some(image) = messages
                    .get(occurrence.message_index)
                    .and_then(|message| message.tool_images.get(occurrence.image_index))
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
        self.with(|i| i.placement = Some(placement))
    }

    pub fn placement(&self) -> Option<&crate::core::file_placement::PlacementContext> {
        self.0.placement.as_ref()
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

    #[cfg(test)]
    pub(crate) fn with_speculative_driver_join_probe(
        &self,
        probe: crate::agent::tools::workers::WorkerJoinTestProbe,
    ) -> Self {
        self.with(|i| i.speculative_driver_join_probe = Some(probe))
    }

    #[cfg(test)]
    pub(crate) fn speculative_driver_join_probe(
        &self,
    ) -> Option<&crate::agent::tools::workers::WorkerJoinTestProbe> {
        self.0.speculative_driver_join_probe.as_ref()
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
            if self.0.overlay.is_some() {
                parent.check_narrowing_frozen(&scope)?;
            } else {
                parent.check_narrowing(&scope)?;
            }
        }
        Ok(self.with(|i| i.native_scope = Some(scope)))
    }

    pub(crate) fn native_scope(&self) -> Option<&crate::core::native_scope::NativeWriteScope> {
        self.0.native_scope.as_ref()
    }

    /// Only a controller-held handle can enter a private lane. Existing lanes cannot be replaced.
    pub(crate) fn with_overlay(
        &self,
        overlay: crate::core::speculation::store::OverlayHandle,
    ) -> anyhow::Result<Self> {
        let scope = self.native_scope().ok_or_else(|| {
            anyhow::anyhow!("a speculative overlay requires an exact native scope")
        })?;
        overlay.check_owner_scope(&self.resource_scope(), scope)?;
        if let Some(current) = &self.0.overlay {
            anyhow::ensure!(
                current.same_handle(&overlay),
                "cannot replace an active overlay"
            );
        }
        Ok(self.with(|i| {
            i.overlay = Some(overlay);
            i.restore_authorized = false;
            i.recovery = i.recovery.without_workspace_restore();
        }))
    }

    pub(crate) fn overlay(&self) -> Option<&crate::core::speculation::store::OverlayHandle> {
        self.0.overlay.as_ref()
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

    pub fn recovery(&self) -> &crate::features::timemachine::RunRecovery {
        &self.0.recovery
    }

    pub fn with_recovery(&self, recovery: crate::features::timemachine::RunRecovery) -> Self {
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
        self.with_recovery(crate::features::timemachine::RunRecovery::default())
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
    /// Stack of contexts active on THIS thread. A stack (not a single slot) so a nested
    /// `with_current` — e.g. a tool that re-enters the agent loop — restores the outer context on
    /// exit, exactly like [`crate::core::cancel`]'s token stack.
    static CURRENT: RefCell<Vec<ExecutionContext>> = const { RefCell::new(Vec::new()) };
}

/// Run synchronous tool code with `ctx` as the active per-turn context, restored on return (even on
/// panic, via the `Drop` guard). Must wrap the tool body INSIDE the `spawn_blocking` closure — a
/// context pushed on the driver thread is invisible on the worker thread the body actually runs on.
pub fn with_current<T>(ctx: ExecutionContext, f: impl FnOnce() -> T) -> T {
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

/// The context of the currently executing synchronous tool body, if any. `None` on a thread with no
/// pinned turn (e.g. a caller reading before any `with_current` wrap) — callers fall back to their
/// process-global slot.
pub fn current() -> Option<ExecutionContext> {
    CURRENT.with(|slot| slot.borrow().last().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn delegated_restore_authority_cannot_be_regranted_by_context_builders() {
        let parent = ExecutionContext::new(ConversationId::new("restore-authority"));
        assert!(parent.can_restore_workspace());
        let child = parent
            .without_workspace_restore()
            .with_resource_scope("restore-authority/task/a")
            .fork_recovery();
        assert!(!child.can_restore_workspace());
        assert!(!child.recovery().can_restore_workspace());
        let reset = child.with_recovery(crate::features::timemachine::RunRecovery::default());
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
            }));
        let b = ExecutionContext::new(ConversationId::new("telegram:work:2")).with_approval_route(
            Some(ApprovalRoute {
                route: "work".into(),
                chat: "2".into(),
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
