//! The tool surface: a `Tool` trait + a registry the agent loop advertises to the model.
//!
//! Discipline (the repo's hard-won lesson — semantic OVERLAP between tools is the killer,
//! not the count): names are `category_action`, one canonical tool per capability, and each
//! description says when to use it AND when NOT to (point at the sibling tool instead).
//! Execution is synchronous — tools are local (memory / fs / subprocess).

use crate::core::types::ToolDef;
use anyhow::Result;
use serde_json::Value;

#[path = "tool_workers.rs"]
pub(crate) mod workers;

/// What a tool call may mutate in the local workspace. This is deliberately separate from
/// `is_destructive`: an outbound notification needs approval but no code checkpoint, while a file
/// edit needs both. Args-aware classification keeps read-only process actions from creating Git
/// snapshots merely because their tool shares one broad capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceEffect {
    None,
    Paths,
    OpaqueWorkspace,
    /// Private speculative bytes, not a materialized workspace edit or checkpoint.
    PrivateOverlay,
    RepoMetadata,
    External,
}

impl WorkspaceEffect {
    pub fn needs_checkpoint(self) -> bool {
        matches!(self, Self::Paths | Self::OpaqueWorkspace)
    }
}

/// Structured observation: only `text` belongs in hooks, logs, terminal output and progress
/// heuristics. Images stay typed and are projected into the request at the provider boundary.
#[derive(Debug, Clone, Default)]
pub struct ToolObservation {
    pub text: String,
    pub images: Vec<crate::core::tool_media::ToolImage>,
}

impl ToolObservation {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

impl From<String> for ToolObservation {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// Physical uncertainty is typed before hooks/trimming; error prose never grants retry authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolEffectState {
    #[default]
    Settled,
    PossiblyApplied(Option<u64>),
    UnknownWorker,
}

/// Propagate uncertainty across a delegated loop without converting it into a retryable stop.
#[derive(Debug)]
pub struct ToolEffectsUnsettled(pub ToolEffectState);
impl std::fmt::Display for ToolEffectsUnsettled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tool effects are not settled ({:?}); no retry, undo or new mutation is authorized",
            self.0
        )
    }
}
impl std::error::Error for ToolEffectsUnsettled {}

/// The result classification is captured before hooks or result trimming can change the text.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub observation: ToolObservation,
    pub failure: Option<String>,
    pub effect_state: ToolEffectState,
}

impl ToolOutcome {
    pub fn failed(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            observation: ToolObservation::text(message.clone()),
            failure: Some(message),
            effect_state: ToolEffectState::Settled,
        }
    }

    pub fn failed_with_effect_state(
        message: impl Into<String>,
        effect_state: ToolEffectState,
    ) -> Self {
        if effect_state != ToolEffectState::Settled {
            crate::core::recovery::mark_effects_unsettled();
        }
        Self {
            effect_state,
            ..Self::failed(message)
        }
    }
}

impl From<String> for ToolOutcome {
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

impl std::ops::Deref for ToolOutcome {
    type Target = ToolObservation;

    fn deref(&self) -> &ToolObservation {
        &self.observation
    }
}

#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub call_id: String,
    pub prepared: Option<PreparedToolCall>,
    pub outcome: ToolOutcome,
}

impl std::ops::Deref for ToolExecution {
    type Target = ToolOutcome;

    fn deref(&self) -> &ToolOutcome {
        &self.outcome
    }
}

/// A safe call's watcher starts with the body, not with sequential result collection.
/// Dropping the watcher does not prove that its blocking worker has stopped.
pub struct StartedSafeCall {
    handle: tokio::task::JoinHandle<ToolExecution>,
    prepared: PreparedToolCall,
    settling: bool,
    /// Only a delegation-local token may be cancelled when its observer goes away.
    delegation_cancel: Option<crate::core::cancel::TurnCancel>,
}

impl StartedSafeCall {
    pub fn start(
        call_id: String,
        prepared: PreparedToolCall,
        cancel: crate::core::cancel::TurnCancel,
        deadline: tokio::time::Instant,
        producer: Option<&workers::ToolWorkerProducer>,
        body: impl FnOnce() -> ToolOutcome + Send + 'static,
    ) -> Self {
        Self::start_observed(call_id, prepared, cancel, deadline, producer, body, false)
    }

    /// `cancel` must be derived for this invocation, never the parent/sibling token.
    pub(crate) fn start_delegated(
        call_id: String,
        prepared: PreparedToolCall,
        cancel: crate::core::cancel::TurnCancel,
        deadline: tokio::time::Instant,
        producer: Option<&workers::ToolWorkerProducer>,
        body: impl FnOnce() -> ToolOutcome + Send + 'static,
    ) -> Self {
        Self::start_observed(call_id, prepared, cancel, deadline, producer, body, true)
    }

    fn start_observed(
        call_id: String,
        prepared: PreparedToolCall,
        cancel: crate::core::cancel::TurnCancel,
        deadline: tokio::time::Instant,
        producer: Option<&workers::ToolWorkerProducer>,
        body: impl FnOnce() -> ToolOutcome + Send + 'static,
        delegated: bool,
    ) -> Self {
        let actual_prepared = prepared.clone();
        let delegation_cancel = delegated.then(|| cancel.clone());
        let worker_cancel = cancel.clone();
        // Admission precedes spawn and watcher creation, including eager calls never adopted.
        let worker = if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            drop(body);
            Err(anyhow::anyhow!("tool launch was cancelled or its deadline elapsed; nothing started"))
        } else {
            workers::launch_prepared(producer, &prepared, move || {
                if worker_cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                    ToolOutcome::failed("error: tool launch was cancelled or its deadline elapsed")
                } else {
                    body()
                }
            }, ToolEffectState::UnknownWorker)
        };
        let handle = tokio::spawn(async move {
            let outcome = match worker {
                Err(error) => ToolOutcome::failed(format!("error: tool launch refused: {error:#}")),
                Ok(mut worker) => {
                    let interrupted = tokio::select! {
                        biased;
                        result = &mut worker => {
                            return ToolExecution { call_id, prepared: Some(prepared), outcome: match result {
                                Ok(joined) if joined.completed <= deadline => joined.outcome,
                                Ok(joined) => ToolOutcome::failed_with_effect_state(
                                    format!("error: tool call exceeded its launch deadline; partial report:\n{}", joined.outcome.text),
                                    joined.outcome.effect_state,
                                ),
                                Err(_) => ToolOutcome::failed_with_effect_state(
                                    "error: tool-body supervisor stopped; actual effects are unknown",
                                    ToolEffectState::UnknownWorker,
                                ),
                            }};
                        },
                        _ = cancel.cancelled() => "cancelled",
                        _ = tokio::time::sleep_until(deadline) => "deadline exceeded",
                    };
                    if delegated {
                        cancel.cancel();
                        // Cooperative shutdown is not physical proof. Keep ownership charged until
                        // the independent worker supervisor delivers its actual outcome.
                        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut worker).await {
                            Ok(Ok(joined)) => ToolOutcome::failed_with_effect_state(
                                format!("error: delegated call {interrupted}; partial report:\n{}", joined.outcome.text),
                                joined.outcome.effect_state,
                            ),
                            _ => ToolOutcome::failed_with_effect_state(
                                format!("error: delegated call {interrupted}; settlement unconfirmed (worker may still be running)"),
                                ToolEffectState::UnknownWorker,
                            ),
                        }
                    } else {
                        ToolOutcome::failed_with_effect_state(
                            format!("error: tool call {interrupted}; observation abandoned (worker may still be running)"),
                            ToolEffectState::UnknownWorker,
                        )
                    }
                },
            };
            ToolExecution { call_id, prepared: Some(prepared), outcome }
        });
        Self { handle, prepared: actual_prepared, settling: false, delegation_cancel }
    }

    pub fn start_settling_mutation(
        call_id: String,
        prepared: PreparedToolCall,
        cancel: crate::core::cancel::TurnCancel,
        deadline: tokio::time::Instant,
        producer: Option<&workers::ToolWorkerProducer>,
        body: impl FnOnce() -> ToolOutcome + Send + 'static,
    ) -> Self {
        let actual_prepared = prepared.clone();
        let worker = if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            drop(body);
            Err(anyhow::anyhow!("mutation launch was cancelled or its deadline elapsed; nothing started"))
        } else {
            workers::launch_prepared(producer, &prepared, move || {
                if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                    ToolOutcome::failed("error: mutation launch was cancelled or its deadline elapsed; nothing started")
                } else {
                    body()
                }
            }, ToolEffectState::PossiblyApplied(None))
        };
        let handle = tokio::spawn(async move {
            // Only result observation lives here. Actual join ownership is independent of it.
            let outcome = match worker {
                Ok(worker) => worker.await.map(|joined| joined.outcome).unwrap_or_else(|_| {
                    ToolOutcome::failed_with_effect_state(
                        "error: mutation supervisor stopped; effects may be applied; inspect before retrying",
                        ToolEffectState::UnknownWorker,
                    )
                }),
                Err(error) => ToolOutcome::failed(format!("error: mutation launch refused: {error:#}")),
            };
            ToolExecution { call_id, prepared: Some(prepared), outcome }
        });
        Self { handle, prepared: actual_prepared, settling: true, delegation_cancel: None }
    }

    pub fn prepared(&self) -> &PreparedToolCall {
        &self.prepared
    }

    pub fn abort(&self) {
        // A mutation watcher is settlement ownership, not disposable observation.
        if let Some(cancel) = &self.delegation_cancel {
            cancel.cancel();
        } else if !self.settling {
            self.handle.abort();
        }
    }
}

impl Drop for StartedSafeCall {
    fn drop(&mut self) {
        if let Some(cancel) = &self.delegation_cancel {
            cancel.cancel();
        }
    }
}

impl std::future::Future for StartedSafeCall {
    type Output = Result<ToolExecution, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.handle).poll(cx)
    }
}

impl ToolExecution {
    pub fn changed_workspace(&self, tool: &dyn Tool) -> bool {
        if let Some(changes) = self
            .prepared
            .as_ref()
            .and_then(|p| p.workspace_changes.as_ref())
        {
            return changes.changed();
        }
        self.outcome.failure.is_none()
            && self
                .prepared
                .as_ref()
                .is_some_and(|p| tool.workspace_effect_prepared(p).needs_checkpoint())
            && !self
                .outcome
                .text
                .starts_with(crate::agent::builtin::NOOP_WRITE_PREFIX)
            && !self
                .outcome
                .text
                .starts_with(crate::agent::builtin::DRY_RUN_PREFIX)
    }

    pub fn log_result(&self) -> (Option<String>, Option<String>) {
        match &self.outcome.failure {
            Some(error) => (None, Some(error.chars().take(200).collect())),
            None => (Some(self.outcome.text.chars().take(200).collect()), None),
        }
    }
}

/// Actual frozen local resources, not model-provided claims about the tool's effects.
#[derive(Debug, Clone)]
pub(crate) enum PreparedFootprint {
    Native(
        Vec<(
            crate::core::workspace_txn::NativeResource,
            crate::core::repo_lock::LockMode,
        )>,
    ),
    /// Controller-seated private resources; never acquire live path locks for these bodies.
    Overlay {
        handle: crate::core::speculation::store::OverlayHandle,
        files: Vec<(
            crate::core::speculation::store::OverlayFile,
            crate::core::repo_lock::LockMode,
        )>,
    },
    Global,
}

/// Local execution arguments and guards. The model's original call stays in wire history;
/// approval, mutation tracking and execution consume this prepared target instead.
#[derive(Clone)]
pub struct PreparedToolCall {
    pub args: Value,
    pub preview_lines: Vec<String>,
    pub preview: Option<ApprovalPreview>,
    pub file: Option<FilePreparation>,
    pub(crate) footprint: Option<PreparedFootprint>,
    /// Placement-bound UI aliases; execution, approval and wire results retain absolute paths.
    pub(crate) display_paths: Vec<crate::core::file_placement::DisplayPath>,
    /// Frozen delegation resolution, internal only; never read from model arguments.
    pub delegation: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    /// Shared with the actual delegated workers, never inferred from their textual reports.
    pub(crate) workspace_changes: Option<std::sync::Arc<crate::core::exec_ctx::WorkspaceChanges>>,
    /// Single-use stream correlation, shared only by clones of this prepared invocation.
    pub(crate) approval_identity: crate::ui::events::ApprovalIdentity,
}

impl std::fmt::Debug for PreparedToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedToolCall")
            .field("args", &self.args)
            .field("file", &self.file)
            .field("delegation", &self.delegation.is_some())
            .finish_non_exhaustive()
    }
}

impl PreparedToolCall {
    pub fn new(args: Value) -> Self {
        Self {
            args,
            preview_lines: Vec::new(),
            preview: None,
            file: None,
            footprint: None,
            display_paths: Vec::new(),
            delegation: None,
            workspace_changes: None,
            approval_identity: crate::ui::events::ApprovalIdentity::default(),
        }
    }
}

impl std::ops::Deref for PreparedToolCall {
    type Target = Value;

    fn deref(&self) -> &Value {
        &self.args
    }
}

#[derive(Debug, Clone)]
pub struct FilePreparation {
    pub destination: crate::core::file_placement::ResolvedDestination,
    pub source: Option<crate::core::file_placement::ResolvedDestination>,
    /// Recognized managed origin: move source, or an existing write target addressed by its
    /// returned absolute path. This is internal provenance, never supplied by model arguments.
    pub source_managed: bool,
    pub expected: Option<crate::core::persist::FileFingerprint>,
    pub source_expected: Option<crate::core::persist::FileFingerprint>,
    pub source_tree: Option<Vec<(std::path::PathBuf, crate::core::persist::FileFingerprint)>>,
    pub destination_tree: Option<Vec<(std::path::PathBuf, crate::core::persist::FileFingerprint)>>,
}

/// A capability the model may invoke.
///
/// `Send + Sync` is REQUIRED: the agent loop runs each call of a concurrency-safe batch on a
/// `tokio::task::spawn_blocking` thread (see `agent::execute_calls`), which needs the shared
/// `Arc<dyn Tool>` to cross threads. All built-in tools hold only `PathBuf`/unit, so they satisfy
/// this trivially. A future tool holding a non-`Sync` field (`Rc`, `RefCell`, …) will fail to
/// compile here — which is the correct guardrail, not a false alarm: such a tool could not be run
/// concurrently anyway.
pub trait Tool: Send + Sync {
    /// `category_action`, unique in the registry (e.g. `memory_search`, `file_read`).
    fn name(&self) -> &str;
    /// What it does · when to use · "Not for X → use Y" · caveat (see system-prompt.md).
    fn description(&self) -> &str;
    /// JSON-Schema for the arguments object (use `additionalProperties:false` for strict gateways).
    fn parameters(&self) -> Value;
    /// Destructive / outward-facing → the loop gates it behind approval before executing.
    fn is_destructive(&self) -> bool {
        false
    }
    /// Frozen delegation may require controller approval despite a mixed static tool surface.
    fn is_destructive_prepared(&self, call: &PreparedToolCall) -> bool {
        let _ = call;
        self.is_destructive()
    }
    /// Exact keys of an internally frozen native-only child; never inferred from JSON arguments.
    /// This is a scheduling hint, not filesystem authority; body resource locks remain mandatory.
    fn parallel_write_keys_prepared(&self, _call: &PreparedToolCall) -> Option<Vec<String>> {
        None
    }
    /// Frozen exact read/write authority shared with workflow admission. Body locks still apply.
    fn parallel_native_scope_prepared(
        &self,
        _call: &PreparedToolCall,
    ) -> Option<crate::core::native_scope::NativeWriteScope> {
        None
    }
    /// Read-only & side-effect-free → eligible for concurrent execution in a parallel batch.
    /// Destructive tools also set this `false`.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    /// Args-aware refinement of [`Tool::is_concurrency_safe`]: a tool whose safety depends on WHAT
    /// it was asked to do overrides this (e.g. a `task` dispatch is safe iff the resolved sub-agent
    /// registry is read-only). Default: the static flag.
    fn is_concurrency_safe_for(&self, _args: &Value) -> bool {
        self.is_concurrency_safe()
    }
    fn is_concurrency_safe_prepared(&self, call: &PreparedToolCall) -> bool {
        self.is_concurrency_safe_for(&call.args)
    }
    /// Workspace mutation classification used by automatic checkpoints and verification. Override
    /// this for local writers; approval-only/network/process-control tools keep the default `None`.
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::None
    }
    /// WHERE this call will write — an absolute directory to look for a repository in. The
    /// checkpoint gate uses it to discover the work tree that actually owns the change, instead of
    /// assuming the process's cwd does. Those differ constantly in practice: a session launched from
    /// the home directory editing `Desktop/proj/src/x.js` found no repo at cwd and reported "not a
    /// git repository (run `git init`)" — advice that, followed literally, would have turned the
    /// user's ENTIRE HOME DIRECTORY into a repository, while the real project sat one level down
    /// with a perfectly good `.git`. Override in every tool that names a path; the default `None`
    /// means "no particular path" and leaves the gate on cwd.
    fn workspace_target(&self, _args: &Value) -> Option<std::path::PathBuf> {
        None
    }
    /// Prepared mutation classification may use internal placement provenance, never model JSON.
    fn workspace_effect_prepared(&self, call: &PreparedToolCall) -> WorkspaceEffect {
        self.workspace_effect(&call.args)
    }
    fn workspace_target_prepared(&self, call: &PreparedToolCall) -> Option<std::path::PathBuf> {
        self.workspace_target(&call.args)
    }
    /// Whether beginning this call may create a durable workspace, process, network, external-service,
    /// or delegated-agent effect whose completion would be ambiguous after a crash. Defaults to the
    /// existing safety classifications; pure reads override naturally by being non-destructive with
    /// no workspace effect.
    fn recovery_effect(&self, args: &Value) -> bool {
        self.is_destructive() || self.workspace_effect(args) != WorkspaceEffect::None
    }
    /// The shell command line this call would EXECUTE, when this tool runs commands at all. The
    /// loop's hard `cmd_guard` floor and the network-escalation notice key on this — it used to be
    /// a hand-written name match in the executor, which meant a future command-running tool
    /// silently received no floor. Owning it here makes the guard travel with the capability:
    /// override in every tool that spawns a model-supplied command; the default `None` means
    /// "executes nothing".
    fn command_to_guard(&self, _args: &Value) -> Option<String> {
        None
    }
    /// Pure dispatch planning: delegation freezes capabilities here; native file tools defer
    /// filesystem snapshots to `prepare_call` at their own execution boundary.
    fn plan_call(
        &self,
        args: &Value,
        _ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        Ok(PreparedToolCall::new(args.clone()))
    }
    /// A delegation's effective read-only scope. Unknown/unprepared dispatches fail closed.
    fn delegation_read_only_prepared(&self, _call: &PreparedToolCall) -> Option<bool> {
        None
    }
    /// Resolve local execution arguments before approval or effects. Must not create files,
    /// directories, processes or network requests. Default preserves legacy tools unchanged.
    fn prepare_call(
        &self,
        args: &Value,
        _ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        Ok(PreparedToolCall::new(args.clone()))
    }
    /// Run the tool. `args` is the parsed (object) arguments. Return the result text; return
    /// an `Err` for a real failure — the loop feeds the error back to the model to recover.
    fn execute(&self, args: &Value) -> Result<String>;
    /// Backwards-compatible structured execution. Media-aware tools override this; legacy tools
    /// need no changes and return a text-only observation.
    fn execute_observation(&self, args: &Value) -> Result<ToolObservation> {
        self.execute(args).map(ToolObservation::from)
    }
    /// Execute the same target approved by the driver. Native file tools verify their typed
    /// guards here; direct `execute` remains responsible for enforcing the creation contract.
    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        self.execute_observation(&prepared.args)
    }
    /// Explicit image tools require an outbound image approval before execution.
    fn transmits_images(&self, _args: &Value) -> bool {
        false
    }
    /// A tool may return images even when that was not its declared operation (MCP). The executor
    /// can gate/stamp that observation without blocking its ordinary text execution.
    fn may_return_images(&self, args: &Value) -> bool {
        self.transmits_images(args)
    }
    /// Does THIS tool consider `result` a failure? A tool that returns `Ok(...)` even on a logical
    /// failure (so the model sees the detail) — e.g. `shell_run`'s `exit N`, or an MCP tool that
    /// encodes `{"isError":true}` in its Ok body — overrides this so the loop's progress/thrash
    /// guard doesn't count that "success" as real progress (W12). `None` (the default) ⇒ defer to
    /// the loop's generic heuristic (`error:` prefix / non-zero `exit N`); `Some(b)` is definitive.
    fn result_is_error(&self, _result: &str) -> Option<bool> {
        None
    }
    /// What this call WILL do, computed before it does anything — shown above the approval
    /// question so the user approves a patch, a full command line and its directory, not a
    /// basename. Edit tools return the patch (their dry-run); command tools their argv and cwd.
    /// The default `None` keeps the bare question for tools with nothing to show. Must not
    /// write, spawn or touch the network: it runs before the user has said yes.
    fn preview(&self, _args: &Value) -> Option<ApprovalPreview> {
        None
    }
}

/// The pre-flight payload of a destructive call (see [`Tool::preview`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPreview {
    /// The box title: the repo-relative target of an edit, the tool name for a command.
    pub title: String,
    /// The unified diff of the change, when the tool can compute it without writing.
    pub diff: Option<String>,
    /// Plain rows to show — the cwd and full command, the destination, byte counts. Wrapped by
    /// the surface, never clipped here.
    pub lines: Vec<String>,
}

/// Drive an async future from the sync `Tool::execute` path, RACED against user cancellation
/// (Esc while working) so a long network call aborts promptly instead of blocking the turn.
///
/// Validity invariant (the async execution core rests on this): the body is safe BOTH on a tokio
/// multi-thread WORKER (where `block_in_place` parks the worker's queue — the classic bridge) AND
/// on a `spawn_blocking` thread — verified against the vendored tokio source: `block_in_place`
/// with a runtime context but off a worker is a no-op pass-through, and `Handle::block_on` off a
/// worker is the documented sync bridge. Pinned by `bridge_works_inside_spawn_blocking` below so
/// a future tokio upgrade that changes this fails loudly.
pub(crate) fn block_for_tool<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let cancel = crate::core::cancel::current();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            match cancel {
                Some(token) => tokio::select! {
                    r = f => r,
                    _ = token.cancelled() => Err(anyhow::anyhow!("cancelled by user")),
                },
                None => f.await,
            }
        })
    })
}

/// The deferred half of a registry's surface: tool names (registration order) and per-origin
/// counts (first-appearance order). Published as a pair so the prompt note and the skills gate
/// read one consistent snapshot.
pub type DeferredSurface = (Vec<String>, Vec<(String, usize)>);

/// One registry slot: the tool plus how it is surfaced to the model.
///
/// `deferred` tools are REGISTERED but not ADVERTISED: they execute normally when called by name,
/// yet their schema never rides on the request. The model reaches them through `tool_search`,
/// whose results carry the full schema in-band — so a large MCP surface costs tokens only when it
/// is actually used, and the request's `tools` array stays byte-stable for the prefix cache.
#[derive(Clone)]
struct Entry {
    tool: std::sync::Arc<dyn Tool>,
    deferred: bool,
    /// Where a deferred tool came from (the MCP server key) — feeds the per-server counts in the
    /// prompt's deferred-surface note. `None` for every advertised tool.
    origin: Option<String>,
}

/// The set of tools advertised to the model. Insertion order is the advertised order.
/// Stored as `Arc<dyn Tool>` so a call can be moved onto a `spawn_blocking` thread (`'static`
/// requirement) without cloning the tool itself; `register` keeps the `Box` signature so the ~40
/// existing call sites compile unchanged.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Entry>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(Entry {
            tool: std::sync::Arc::from(tool),
            deferred: false,
            origin: None,
        });
    }

    /// Register a tool WITHOUT advertising its schema on the request. It stays fully dispatchable
    /// (`get`/`get_arc` find it), so a model that learned its name — from a `tool_search` result —
    /// calls it like any other tool. `origin` labels the surface it was deferred from (server key).
    /// Takes an `Arc` rather than a `Box` because the caller typically also hands the same handle
    /// to the `tool_search` index.
    pub fn register_deferred(&mut self, tool: std::sync::Arc<dyn Tool>, origin: String) {
        self.tools.push(Entry {
            tool,
            deferred: true,
            origin: Some(origin),
        });
    }

    /// Look up a tool by exact name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|e| e.tool.name() == name)
            .map(|e| e.tool.as_ref())
    }

    /// Look up a tool by exact name as an owned handle — for moving into a `spawn_blocking`
    /// closure (which needs `'static`).
    pub fn get_arc(&self, name: &str) -> Option<std::sync::Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|e| e.tool.name() == name)
            .map(|e| e.tool.clone())
    }

    /// The `tools` array for the chat request — the ADVERTISED surface only. Deferred tools are
    /// deliberately absent: their schemas travel in `tool_search` results instead, which keeps this
    /// array (and the prefix cache built over it) byte-stable however many integrations connect.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .filter(|e| !e.deferred)
            .map(|e| ToolDef::function(e.tool.name(), e.tool.description(), e.tool.parameters()))
            .collect()
    }

    /// EVERY registered tool name (advertised + deferred), in registration order — the dispatchable
    /// surface. The skills `requires:` gate uses this: a skill whose tool is deferred still works,
    /// because a deferred tool is callable once discovered.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|e| e.tool.name().to_string())
            .collect()
    }

    /// The ADVERTISED tool names (advertised order) — exactly the names in `defs()`, so the prompt's
    /// routing map and the request's `tools` array can never disagree.
    pub fn advertised_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|e| !e.deferred)
            .map(|e| e.tool.name().to_string())
            .collect()
    }

    /// The deferred surface: every deferred tool name (registration order) plus per-origin counts
    /// (first-appearance order). `None` when nothing is deferred — the common case, costing nothing.
    pub fn deferred_summary(&self) -> Option<DeferredSurface> {
        let mut names = Vec::new();
        let mut by_origin: Vec<(String, usize)> = Vec::new();
        for e in self.tools.iter().filter(|e| e.deferred) {
            names.push(e.tool.name().to_string());
            let origin = e.origin.clone().unwrap_or_default();
            match by_origin.iter_mut().find(|(o, _)| *o == origin) {
                Some((_, n)) => *n += 1,
                None => by_origin.push((origin, 1)),
            }
        }
        if names.is_empty() {
            None
        } else {
            Some((names, by_origin))
        }
    }

    /// Every registered tool. Lets a test sweep the WHOLE surface (rather than a hand-listed
    /// sample that silently stops covering tools added later).
    #[cfg(test)]
    pub fn tools(&self) -> impl Iterator<Item = &std::sync::Arc<dyn Tool>> {
        self.tools.iter().map(|e| &e.tool)
    }

    /// Keep only tools for which `keep(name)` is true (Hermes `disabled_toolsets` filter).
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&str) -> bool,
    {
        self.tools.retain(|e| keep(e.tool.name()));
    }

    /// Flip an already-registered tool to DEFERRED (dispatchable by name, absent from `defs()`),
    /// returning its handle for the `tool_search` index. `None` when no tool has that name.
    pub fn defer(&mut self, name: &str, origin: &str) -> Option<std::sync::Arc<dyn Tool>> {
        let e = self.tools.iter_mut().find(|e| e.tool.name() == name)?;
        e.deferred = true;
        e.origin = Some(origin.to_string());
        Some(e.tool.clone())
    }

    /// Every deferred tool with its origin label, in registration order.
    pub fn deferred_entries(&self) -> Vec<(std::sync::Arc<dyn Tool>, String)> {
        self.tools
            .iter()
            .filter(|e| e.deferred)
            .map(|e| (e.tool.clone(), e.origin.clone().unwrap_or_default()))
            .collect()
    }
}

// ── schema-driven argument repair ───────────────────────────────────────────────
//
// A model that names an argument `q` instead of `query`, or wraps the whole object in `{"input": …}`,
// costs a full round trip: the call fails with `missing required string arg`, the model reads the
// error, and calls again. That is one wasted tool call plus one wasted assistant turn, per slip.
//
// These helpers close that loop by reading each tool's OWN `parameters()` schema, so every tool —
// builtin, LSP, skills, MCP — is covered by one implementation, and a tool added later is covered
// without touching this file. The alternative (per-tool alias lists, as `web_search::extract_queries`
// does) cannot be replicated across ~40 tools and drifts from the schema the moment either side
// changes.
//
// The rules only ever RESHAPE what the model sent; they never invent a value. Anything ambiguous is
// left alone so the model gets a real error instead of a silently wrong call.

/// A schema's `required` keys that are declared `type: "string"`.
fn required_string_keys(schema: &Value) -> Vec<String> {
    let props = schema.get("properties").and_then(Value::as_object);
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|req| {
            req.iter()
                .filter_map(Value::as_str)
                .filter(|k| {
                    // Absent `properties` entry ⇒ assume string: the key is required either way, and
                    // a repair that hands it a string is still better than a missing-arg failure.
                    props
                        .and_then(|p| p.get(*k))
                        .and_then(|s| s.get("type"))
                        .and_then(Value::as_str)
                        .map(|t| t == "string")
                        .unwrap_or(true)
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Is this arg present AND usable as a non-empty string?
fn has_usable_string(args: &Value, key: &str) -> bool {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// The `required` string keys `schema` demands that `args` does not usably supply.
///
/// Reads the schema directly, so it applies to every tool in the registry (and to MCP tools, whose
/// server-provided schema passes through verbatim). A schema with no `required` array — an open MCP
/// schema, or `web_search`, which enforces "query or queries" at runtime instead — yields an empty
/// list, making every caller a no-op.
pub fn missing_required_strings(schema: &Value, args: &Value) -> Vec<String> {
    required_string_keys(schema)
        .into_iter()
        .filter(|k| !has_usable_string(args, k))
        .collect()
}

/// Keys the schema declares in `properties`.
fn declared_keys(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

/// The single-key wrapper names models most often wrap a whole argument object in.
const WRAPPER_KEYS: &[&str] = &[
    "input",
    "args",
    "arguments",
    "parameters",
    "params",
    "kwargs",
];

/// Repair argument-shape slips that can be inferred WITH CERTAINTY. Returns the corrected arguments
/// plus a short human description of what changed (for the trace line), or `None` when there is
/// nothing to fix or the fix would be a guess.
///
/// Three rules, applied in order:
///
/// 1. **UNWRAP** — the whole object is nested under one wrapper key (`{"input": {"path": …}}`).
///    Accepted only when the inner object satisfies the schema's required keys and the outer object
///    has nothing else, so it cannot swallow a real single-argument call.
/// 2. **ALIAS** — exactly ONE required string key is missing and the arguments carry exactly ONE
///    undeclared string key. The one-to-one condition is what makes this safe: with two candidates on
///    either side the mapping is a coin flip, so it is refused.
/// 3. **COERCE** — a required string key is present but as a number/bool (stringified) or as a
///    single-element string array (unwrapped). No information is created.
///
/// Deliberately NOT done: filling a missing key from a default, dropping undeclared keys (a strict
/// gateway already rejected those upstream, and dropping could discard something meaningful), or
/// renaming when the schema declares no `required`.
pub fn repair_args(schema: &Value, args: &Value) -> Option<(Value, String)> {
    let obj = args.as_object()?;
    let required = required_string_keys(schema);
    if required.is_empty() {
        return None; // nothing to satisfy ⇒ nothing to repair
    }
    if required.iter().all(|k| has_usable_string(args, k)) {
        return None; // already valid: the overwhelmingly common path, allocation-free
    }

    // ── rule 1: UNWRAP ────────────────────────────────────────────────────────
    if obj.len() == 1 {
        let (k, v) = obj.iter().next()?;
        if WRAPPER_KEYS.contains(&k.as_str()) && v.is_object() {
            let inner_ok = required.iter().all(|r| has_usable_string(v, r));
            if inner_ok {
                return Some((v.clone(), format!("unwrapped args from '{k}'")));
            }
        }
    }

    let declared = declared_keys(schema);
    let mut fixed = obj.clone();
    let mut notes: Vec<String> = Vec::new();

    // ── rule 2: ALIAS ─────────────────────────────────────────────────────────
    let missing: Vec<String> = required
        .iter()
        .filter(|k| !has_usable_string(args, k))
        .cloned()
        .collect();
    if missing.len() == 1 {
        let strays: Vec<String> = obj
            .iter()
            .filter(|(k, v)| {
                !declared.contains(*k) && v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if strays.len() == 1 {
            let from = &strays[0];
            let to = &missing[0];
            if let Some(v) = fixed.remove(from) {
                fixed.insert(to.clone(), v);
                notes.push(format!("arg '{from}' → '{to}'"));
            }
        }
    }

    // ── rule 3: COERCE ────────────────────────────────────────────────────────
    for key in &required {
        if has_usable_string(&Value::Object(fixed.clone()), key) {
            continue;
        }
        let coerced = match fixed.get(key) {
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::Bool(b)) => Some(b.to_string()),
            Some(Value::Array(a)) if a.len() == 1 => a[0]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            _ => None,
        };
        if let Some(s) = coerced {
            fixed.insert(key.clone(), Value::String(s));
            notes.push(format!("arg '{key}' coerced to string"));
        }
    }

    if notes.is_empty() {
        return None;
    }
    Some((Value::Object(fixed), notes.join(", ")))
}

/// The model-facing error for a call whose missing arguments could not be repaired. Carries the tool
/// name, what the schema actually requires, and what the model sent — so the retry is informed
/// rather than a second guess. Shared by every tool via the executor, which is why the per-tool
/// messages (`missing required string arg 'x'`) rarely surface any more.
pub fn missing_args_error(tool: &str, schema: &Value, args: &Value, missing: &[String]) -> String {
    let props = schema.get("properties").and_then(Value::as_object);
    let spec = required_string_keys(schema)
        .iter()
        .map(|k| {
            let ty = props
                .and_then(|p| p.get(k))
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("string");
            format!("{k} ({ty})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sent = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    let sent = if sent.chars().count() > 200 {
        let mut t: String = sent.chars().take(197).collect();
        t.push_str("...");
        t
    } else {
        sent
    };
    format!(
        "error: missing required arg{} {} for {tool} — required: {spec}. You sent: {sent}. \
         Call again using those exact key names.",
        if missing.len() == 1 { "" } else { "s" },
        missing
            .iter()
            .map(|m| format!("'{m}'"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn safe_fixture(id: &str, deadline: tokio::time::Instant, delay_ms: u64) -> StartedSafeCall {
        StartedSafeCall::start(
            id.to_string(),
            PreparedToolCall::new(json!({"id": id})),
            crate::core::cancel::TurnCancel::new(),
            deadline,
            None,
            move || {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                ToolOutcome {
                    observation: ToolObservation::text("done"),
                    failure: None,
                    effect_state: ToolEffectState::Settled,
                }
            },
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn safe_watchers_share_launch_window_not_collection_windows() {
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(60);
        let calls: Vec<_> = (0..5)
            .map(|i| safe_fixture(&i.to_string(), deadline, 300))
            .collect();
        for (i, call) in calls.into_iter().enumerate() {
            let execution = call.await.unwrap();
            assert_eq!(execution.call_id, i.to_string());
            assert!(execution.failure.as_deref().unwrap().contains("deadline"));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "sequential collection must not add five timeout windows"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn safe_on_time_completion_survives_late_collection() {
        let call = safe_fixture(
            "early",
            tokio::time::Instant::now() + std::time::Duration::from_millis(100),
            0,
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let result = call.await.unwrap();
        assert!(
            result.failure.is_none(),
            "collection time is not completion time"
        );
        assert_eq!(result.text, "done");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn safe_cancel_abandons_observation_without_claiming_worker_stopped() {
        let cancel = crate::core::cancel::TurnCancel::new();
        let call = StartedSafeCall::start(
            "cancel".into(),
            PreparedToolCall::new(json!({})),
            cancel.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            None,
            || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                ToolOutcome {
                    observation: ToolObservation::text("late"),
                    failure: None,
                    effect_state: ToolEffectState::Settled,
                }
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), call)
            .await
            .unwrap()
            .unwrap();
        assert!(result
            .failure
            .as_deref()
            .unwrap()
            .contains("worker may still be running"));
    }

    #[tokio::test]
    async fn safe_worker_panic_has_typed_unknown_effects() {
        let result = StartedSafeCall::start(
            "panic".into(),
            PreparedToolCall::new(json!({})),
            crate::core::cancel::TurnCancel::new(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            None,
            || panic!("owned safe worker fault"),
        )
        .await
        .unwrap();
        assert_eq!(result.effect_state, ToolEffectState::UnknownWorker);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settling_mutation_keeps_actual_result_after_cancel_deadline_and_abort() {
        let cancel = crate::core::cancel::TurnCancel::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let call = StartedSafeCall::start_settling_mutation(
            "owned-mutation".into(),
            PreparedToolCall::new(json!({})),
            cancel.clone(),
            deadline,
            None,
            move || {
                let _ = started_tx.send(());
                let deadline = finish_rx.recv().unwrap();
                assert!(tokio::time::Instant::now() >= deadline);
                ToolOutcome {
                    observation: ToolObservation::text("actual late mutation"),
                    failure: None,
                    effect_state: ToolEffectState::Settled,
                }
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        tokio::time::sleep_until(deadline).await;
        assert!(
            !call.handle.is_finished(),
            "deadline/cancel cannot finish a live mutation observer"
        );
        call.abort(); // Mutation observation cannot abort settlement ownership.
        finish_tx.send(deadline).unwrap();
        let result = call.await.unwrap();
        assert_eq!(result.call_id, "owned-mutation");
        assert_eq!(result.text, "actual late mutation");
        assert_eq!(result.effect_state, ToolEffectState::Settled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settling_mutation_panic_has_typed_uncertainty() {
        let result = StartedSafeCall::start_settling_mutation(
            "panic".into(),
            PreparedToolCall::new(json!({})),
            crate::core::cancel::TurnCancel::new(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            None,
            || panic!("owned mutation fault"),
        )
        .await
        .unwrap();
        assert_eq!(result.effect_state, ToolEffectState::PossiblyApplied(None));
        assert!(result.failure.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settling_mutation_cancelled_before_start_never_calls_body() {
        let cancel = crate::core::cancel::TurnCancel::new();
        cancel.cancel();
        let result = StartedSafeCall::start_settling_mutation(
            "cancelled".into(),
            PreparedToolCall::new(json!({})),
            cancel,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            None,
            || panic!("cancelled body must not run"),
        )
        .await
        .unwrap();
        assert_eq!(result.effect_state, ToolEffectState::Settled);
        assert!(result
            .failure
            .as_deref()
            .unwrap()
            .contains("nothing started"));
    }

    #[test]
    fn delegated_partial_changes_survive_failed_summary_but_empty_success_is_not_an_edit() {
        struct Delegate;
        impl Tool for Delegate {
            fn name(&self) -> &str {
                "test_delegate"
            }
            fn description(&self) -> &str {
                "Owned attribution test"
            }
            fn parameters(&self) -> Value {
                json!({"type":"object"})
            }
            fn execute(&self, _: &Value) -> Result<String> {
                unreachable!()
            }
            fn workspace_effect(&self, _: &Value) -> WorkspaceEffect {
                WorkspaceEffect::Paths
            }
        }
        let changes = std::sync::Arc::new(crate::core::exec_ctx::WorkspaceChanges::default());
        let mut prepared = PreparedToolCall::new(json!({}));
        prepared.workspace_changes = Some(changes.clone());
        let mut result = ToolExecution {
            call_id: "delegate".into(),
            prepared: Some(prepared),
            outcome: ToolOutcome {
                observation: ToolObservation::text("done"),
                failure: None,
                effect_state: ToolEffectState::Settled,
            },
        };
        assert!(
            !result.changed_workspace(&Delegate),
            "a successful report alone is not a mutation"
        );
        changes.record(&[std::path::PathBuf::from("owned.txt")]);
        result.outcome = ToolOutcome::failed("error: next step failed after the settled edit");
        assert!(result.changed_workspace(&Delegate));
        assert_eq!(changes.paths(), [std::path::PathBuf::from("owned.txt")]);
    }

    #[test]
    fn execution_log_distinguishes_identity_empty_success_and_failure() {
        let success = ToolExecution {
            call_id: "not-the-body".into(),
            prepared: None,
            outcome: ToolOutcome {
                observation: ToolObservation::text(""),
                failure: None,
                effect_state: ToolEffectState::Settled,
            },
        };
        assert_eq!(success.log_result(), (Some(String::new()), None));
        let failure = ToolExecution {
            outcome: ToolOutcome::failed("error: refused"),
            ..success
        };
        assert_eq!(failure.log_result(), (None, Some("error: refused".into())));
        assert_eq!(failure.call_id, "not-the-body");
    }

    /// `file_read`-shaped schema: one required string plus optional extras.
    fn path_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start": {"type": "integer"},
                "number": {"type": "boolean"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn repair_unwraps_a_wrapped_args_object() {
        for wrapper in [
            "input",
            "args",
            "arguments",
            "parameters",
            "params",
            "kwargs",
        ] {
            let args = json!({wrapper: {"path": "a.rs"}});
            let (fixed, what) = repair_args(&path_schema(), &args)
                .unwrap_or_else(|| panic!("wrapper '{wrapper}' must unwrap"));
            assert_eq!(fixed, json!({"path": "a.rs"}));
            assert!(what.contains(wrapper), "note names the wrapper: {what}");
        }
    }

    #[test]
    fn repair_does_not_unwrap_when_the_inner_object_still_lacks_the_key() {
        // `{"input": {...}}` that doesn't satisfy the schema is not a wrapper slip — leave it alone
        // so the model gets a real error instead of a differently-broken call.
        let args = json!({"input": {"nope": "a.rs"}});
        assert!(repair_args(&path_schema(), &args).is_none());
    }

    #[test]
    fn repair_maps_a_single_unknown_alias() {
        let (fixed, what) = repair_args(&path_schema(), &json!({"file": "a.rs"})).unwrap();
        assert_eq!(fixed, json!({"path": "a.rs"}));
        assert!(what.contains("'file'") && what.contains("'path'"), "{what}");

        // A different tool shape, same rule — this is why it isn't a per-tool alias table.
        let query_schema = json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        });
        let (fixed, _) = repair_args(&query_schema, &json!({"q": "hermes"})).unwrap();
        assert_eq!(fixed, json!({"query": "hermes"}));

        // Declared optional args ride along untouched.
        let (fixed, _) = repair_args(&path_schema(), &json!({"file": "a.rs", "start": 3})).unwrap();
        assert_eq!(fixed, json!({"path": "a.rs", "start": 3}));
    }

    #[test]
    fn repair_refuses_when_ambiguous() {
        // TWO stray keys → which one is the path? Refuse rather than guess.
        assert!(
            repair_args(&path_schema(), &json!({"file": "a.rs", "dest": "b.rs"})).is_none(),
            "two candidate strays must not be guessed"
        );
        // TWO missing required keys → a single stray cannot fill both.
        let two = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "body": {"type": "string"}},
            "required": ["name", "body"]
        });
        assert!(
            repair_args(&two, &json!({"title": "x"})).is_none(),
            "two missing keys must not be guessed"
        );
    }

    #[test]
    fn repair_coerces_scalar_and_single_element_array() {
        let (fixed, what) = repair_args(&path_schema(), &json!({"path": 123})).unwrap();
        assert_eq!(fixed, json!({"path": "123"}));
        assert!(what.contains("coerced"), "{what}");

        let (fixed, _) = repair_args(&path_schema(), &json!({"path": ["a.rs"]})).unwrap();
        assert_eq!(fixed, json!({"path": "a.rs"}));

        // A multi-element array is a real disagreement about arity — not ours to resolve.
        assert!(repair_args(&path_schema(), &json!({"path": ["a.rs", "b.rs"]})).is_none());
    }

    #[test]
    fn repair_is_a_noop_for_valid_args() {
        assert!(repair_args(&path_schema(), &json!({"path": "a.rs"})).is_none());
        // No `required` (an open MCP schema, or web_search's runtime-enforced pair) ⇒ never repaired.
        let open = json!({"type": "object", "additionalProperties": true});
        assert!(repair_args(&open, &json!({"anything": 1})).is_none());
        // Non-object arguments are not repairable.
        assert!(repair_args(&path_schema(), &json!("just a string")).is_none());
    }

    #[test]
    fn missing_required_strings_reports_only_unusable_keys() {
        assert!(missing_required_strings(&path_schema(), &json!({"path": "a.rs"})).is_empty());
        assert_eq!(
            missing_required_strings(&path_schema(), &json!({})),
            vec!["path".to_string()]
        );
        // Present but blank / wrong type still counts as missing.
        assert_eq!(
            missing_required_strings(&path_schema(), &json!({"path": "   "})),
            vec!["path".to_string()]
        );
        assert_eq!(
            missing_required_strings(&path_schema(), &json!({"path": 7})),
            vec!["path".to_string()]
        );
    }

    #[test]
    fn missing_args_error_carries_the_schema_and_what_was_sent() {
        let msg = missing_args_error(
            "file_glob",
            &json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"]
            }),
            &json!({"glb": "**/*.rs"}),
            &["pattern".to_string()],
        );
        assert!(msg.starts_with("error: "), "{msg}");
        assert!(msg.contains("file_glob"), "names the tool: {msg}");
        assert!(msg.contains("pattern (string)"), "states the schema: {msg}");
        assert!(
            msg.contains(r#"{"glb":"**/*.rs"}"#),
            "echoes the args: {msg}"
        );
    }

    /// A named no-op tool, for exercising the registry's advertised/deferred split.
    struct Dummy(&'static str);
    impl Tool for Dummy {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "a dummy tool for registry tests, long enough to be a usable description"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn execute(&self, _args: &Value) -> Result<String> {
            Ok(format!("ran {}", self.0))
        }
    }

    #[test]
    fn legacy_tool_execution_gets_a_text_only_observation() {
        let tool = Dummy("legacy");
        let observation = tool.execute_observation(&json!({})).unwrap();
        assert_eq!(observation.text, "ran legacy");
        assert!(observation.images.is_empty());
        assert!(!tool.transmits_images(&json!({})));
        assert!(!tool.may_return_images(&json!({})));
    }

    #[test]
    fn deferred_tools_are_dispatchable_but_never_advertised() {
        let mut r = ToolRegistry::new();
        r.register(Box::new(Dummy("visible_one")));
        r.register_deferred(std::sync::Arc::new(Dummy("hidden_one")), "srv_a".into());
        r.register_deferred(std::sync::Arc::new(Dummy("hidden_two")), "srv_a".into());
        r.register_deferred(std::sync::Arc::new(Dummy("hidden_three")), "srv_b".into());
        r.register(Box::new(Dummy("visible_two")));

        // The request advertises only the non-deferred tools, in registration order.
        let advertised: Vec<String> = r.defs().into_iter().map(|d| d.function.name).collect();
        assert_eq!(advertised, vec!["visible_one", "visible_two"]);
        assert_eq!(r.advertised_names(), advertised);

        // But EVERY tool is dispatchable — a deferred tool called by name still runs.
        assert_eq!(
            r.names(),
            vec![
                "visible_one",
                "hidden_one",
                "hidden_two",
                "hidden_three",
                "visible_two"
            ]
        );
        let hidden = r.get_arc("hidden_two").expect("deferred is dispatchable");
        assert_eq!(hidden.execute(&json!({})).unwrap(), "ran hidden_two");

        // The deferred summary carries names + per-origin counts, first-appearance order.
        let (names, by_origin) = r.deferred_summary().expect("has deferred tools");
        assert_eq!(names, vec!["hidden_one", "hidden_two", "hidden_three"]);
        assert_eq!(
            by_origin,
            vec![("srv_a".to_string(), 2), ("srv_b".to_string(), 1)]
        );
    }

    #[test]
    fn deferred_summary_is_none_without_deferred_tools() {
        let mut r = ToolRegistry::new();
        r.register(Box::new(Dummy("only_visible")));
        assert!(r.deferred_summary().is_none());
        // And `retain` drops deferred tools like any other (the toolset filter path).
        r.register_deferred(std::sync::Arc::new(Dummy("mcp_x_y")), "x".into());
        r.retain(|n| !n.starts_with("mcp_"));
        assert!(r.deferred_summary().is_none());
        assert!(r.get("mcp_x_y").is_none());
    }

    /// PINS the tokio behavior `block_for_tool` depends on: `block_in_place` + `Handle::block_on`
    /// must work unchanged INSIDE a `spawn_blocking` thread (context propagates; block_in_place is
    /// a pass-through off a worker). If a tokio upgrade breaks this, the async execution core's
    /// parallel path breaks with it — this test makes that loud and local.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_works_inside_spawn_blocking() {
        // On a spawn_blocking thread…
        let via_blocking = tokio::task::spawn_blocking(|| {
            block_for_tool(async { Ok::<_, anyhow::Error>(41 + 1) })
        })
        .await
        .expect("spawn_blocking join");
        assert_eq!(via_blocking.unwrap(), 42);
        // …and on a worker thread (the classic serial path).
        let on_worker = block_for_tool(async { Ok::<_, anyhow::Error>("ok") });
        assert_eq!(on_worker.unwrap(), "ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_observes_the_scoped_turn_token() {
        let token = crate::core::cancel::TurnCancel::new();
        token.cancel();
        let err = tokio::task::spawn_blocking(move || {
            crate::core::cancel::with_current(token, || {
                block_for_tool(async {
                    std::future::pending::<()>().await;
                    Ok::<_, anyhow::Error>(())
                })
            })
        })
        .await
        .expect("spawn_blocking join")
        .unwrap_err();
        assert!(err.to_string().contains("cancelled by user"), "{err}");
    }
}
