//! Lazy serial provider actor. This module owns physical work, not a resident provider process.

use super::{install, manager, memory_api as api, transport};
use crate::agent::process::{SettlementLifetime, UnsettledCharge, UnsettledOwnership};
use crate::agent::tools::{ToolEffectState, ToolEffectsUnsettled};
use anyhow::{ensure, Context, Result};
use api::{ErrorCode, MemoryError, Method, Validate};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, BufReader};

#[path = "adoption/process.rs"]
mod adoption_process;
pub(crate) use adoption_process::adoption_execute;

const QUEUE_CAPACITY: usize = 8;
const MAX_TRANSACTION: Duration = Duration::from_secs(85);

static UNSETTLED: OnceLock<Arc<UnsettledOwnership>> = OnceLock::new();
fn unsettled() -> &'static Arc<UnsettledOwnership> {
    UNSETTLED.get_or_init(|| Arc::new(UnsettledOwnership::new()))
}

fn unknown_worker(message: &str) -> anyhow::Error {
    anyhow::Error::new(ToolEffectsUnsettled(ToolEffectState::UnknownWorker))
        .context(message.to_owned())
}

fn unresolved_outcome(message: &str) -> anyhow::Error {
    anyhow::Error::new(ToolEffectsUnsettled(ToolEffectState::PossiblyApplied(None)))
        .context(message.to_owned())
}

/// Queue/observer ownership starts before admission or marker creation. Losing an observer is not
/// proof that its queued work stopped; only the actor's pre-spawn exit or physical release clears it.
struct Observer {
    settlement: Arc<SettlementLifetime<()>>,
    _charge: UnsettledCharge,
}
impl Observer {
    fn new(tracker: &Arc<UnsettledOwnership>) -> Self {
        let settlement = Arc::new(SettlementLifetime::new((), |_| {}));
        let charge = tracker.track(&settlement);
        Self {
            settlement,
            _charge: charge,
        }
    }
}

struct BeforeSpawnProof {
    settlement: Option<Arc<SettlementLifetime<()>>>,
    marker: Option<(PathBuf, crate::core::persist::FileFingerprint)>,
}
impl BeforeSpawnProof {
    fn new(settlement: Arc<SettlementLifetime<()>>) -> Self {
        Self {
            settlement: Some(settlement),
            marker: None,
        }
    }

    fn spawn_if_live<T>(
        &mut self,
        cancel: &crate::core::cancel::TurnCancel,
        deadline: Instant,
        spawn: impl FnOnce() -> T,
    ) -> Result<T> {
        check_before_spawn(cancel, deadline)?;
        // After this point, only the contained spawn's cleanup proof or physical release may settle.
        self.settlement.take();
        self.marker.take();
        Ok(spawn())
    }
}
impl Drop for BeforeSpawnProof {
    fn drop(&mut self) {
        if let Some(settlement) = self.settlement.take() {
            if let Some((marker, fingerprint)) = &self.marker {
                let _ = crate::core::persist::remove_if_unchanged(marker, fingerprint);
            }
            settlement.prove_tree_settled();
        }
    }
}

fn check_before_spawn(cancel: &crate::core::cancel::TurnCancel, deadline: Instant) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "Memory request cancelled before spawn"
    );
    ensure!(
        deadline > Instant::now(),
        "Memory execution deadline elapsed before spawn"
    );
    Ok(())
}

/// Called at model/turn boundaries, including optional-context failures. Marker absence alone is
/// never settlement proof: an observer can disappear before the actor reaches marker creation.
pub(crate) fn check_effects() -> Result<()> {
    if !unsettled().wait_for_recent_settlement(crate::agent::mcp::STARTUP_SETTLEMENT_GRACE) {
        return Err(unknown_worker(
            "Memory actor ownership has not been proven settled; new work withheld",
        ));
    }
    if manager::transaction_marker().try_exists().map_err(|_| {
        unresolved_outcome("Memory recovery receipt could not be checked; new work withheld")
    })? {
        return Err(unresolved_outcome(
            "Memory transaction requires explicit recovery; new work withheld",
        ));
    }
    Ok(())
}

pub(crate) fn optional_failure(error: anyhow::Error) -> Result<()> {
    if !matches!(
        crate::agent::tool_error_effect_state(&error),
        ToolEffectState::Settled
    ) {
        return Err(error);
    }
    check_effects()
}

fn clean_terminal_reply(result: &Result<api::Result<Value>>, stderr_ok: bool) -> bool {
    stderr_ok
        && match result {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => !matches!(
                error.code,
                ErrorCode::AmbiguousResult | ErrorCode::RecoveryRequired
            ),
            Err(_) => false,
        }
}

/// Constructed by a host dispatcher only AFTER its ordinary approval decision. Never deserialized
/// from provider/model JSON. The saved package execution grant is checked again at physical launch.
pub(crate) struct ApprovedInvocation {
    background_learning: bool,
    human_management: bool,
}
impl ApprovedInvocation {
    pub(crate) fn explicit_cli() -> Self {
        Self {
            background_learning: false,
            human_management: true,
        }
    }

    /// A scoped browser is an explicit human read, not a cross-workspace/device management
    /// request. Attenuate the sealed approval before the runtime constructs wire capabilities.
    pub(crate) fn without_management(mut self) -> Self {
        self.human_management = false;
        self
    }

    fn model_tool() -> Self {
        Self::saved_context()
    }

    pub(crate) fn saved_context() -> Self {
        Self {
            background_learning: false,
            human_management: false,
        }
    }

    pub(crate) fn saved_learning() -> Self {
        Self {
            background_learning: true,
            human_management: false,
        }
    }
}

fn check_invocation_origin(
    approval: &ApprovedInvocation,
    context: &api::ProviderContext,
    execution: Option<&crate::core::exec_ctx::ExecutionContext>,
) -> Result<()> {
    // Callers cannot supply the wire grant. Only run(), after pinned-manifest validation, adds it.
    ensure!(
        !context
            .granted_capabilities
            .contains(&api::Capability::Management),
        "Memory management authority cannot be supplied in a caller context"
    );
    if approval.human_management {
        ensure!(
            execution.is_none_or(|ctx| ctx.can_restore_workspace()
                && ctx.dispatch_label().is_none()
                && ctx.native_scope().is_none()
                && ctx.overlay().is_none()),
            "delegated or speculative execution cannot request human Memory management"
        );
    }
    Ok(())
}

fn grant_management(
    approval: &ApprovedInvocation,
    context: &mut api::ProviderContext,
    declared: &[api::Capability],
) {
    // Optional 1.0 extension: old third-party providers receive the unchanged base context.
    if approval.human_management && declared.contains(&api::Capability::Management) {
        context
            .granted_capabilities
            .push(api::Capability::Management);
    }
}

struct Work {
    approval: ApprovedInvocation,
    context: api::ProviderContext,
    method: Method,
    params: Value,
    execution: Option<crate::core::exec_ctx::ExecutionContext>,
    cancel: crate::core::cancel::TurnCancel,
    settlement: Arc<SettlementLifetime<()>>,
    reply: tokio::sync::oneshot::Sender<Result<Value>>,
}

static ACTOR: OnceLock<tokio::sync::mpsc::Sender<Work>> = OnceLock::new();
static ACTOR_START: Mutex<()> = Mutex::new(());

fn actor() -> Result<tokio::sync::mpsc::Sender<Work>> {
    if let Some(actor) = ACTOR.get() {
        return Ok(actor.clone());
    }
    let _start = ACTOR_START
        .lock()
        .map_err(|_| anyhow::anyhow!("Memory actor startup poisoned"))?;
    if let Some(actor) = ACTOR.get() {
        return Ok(actor.clone());
    }
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Work>(QUEUE_CAPACITY);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("aizen-memory".into())
        .spawn(move || {
            runtime.block_on(async move {
                while let Some(work) = receiver.recv().await {
                    // Receiver cancellation cancels this transaction but does not abandon its settlement.
                    let result = run(
                        work.approval,
                        work.context,
                        work.method,
                        work.params,
                        work.execution,
                        work.cancel,
                        work.settlement,
                    )
                    .await;
                    let _ = work.reply.send(result);
                }
            });
        })
        .context("starting Memory actor")?;
    let _ = ACTOR.set(sender.clone());
    Ok(sender)
}

fn operation_params(
    context: &api::ProviderContext,
    method: Method,
    mut params: Value,
) -> Result<Value> {
    if matches!(
        method,
        Method::BuildContext | Method::Recall | Method::Search
    ) && params.get("operation_id").is_none()
    {
        // Context/search may persist exposure or caches. The host supplies an opaque identity even
        // when a minimal provider treats them as reads; no retry is inferred from this identifier.
        let binding = serde_json::to_vec(&json!({
            "context": context, "method": method.name(), "params": params,
        }))?;
        let object = params
            .as_object_mut()
            .context("Memory params must be an object")?;
        object.insert(
            "operation_id".into(),
            json!(format!("host-{}", super::manifest::sha256(&binding))),
        );
    }
    Ok(params)
}

fn check_before_enqueue(
    execution: Option<&crate::core::exec_ctx::ExecutionContext>,
    cancel: &crate::core::cancel::TurnCancel,
) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "Memory request cancelled before enqueue"
    );
    ensure!(
        execution
            .and_then(|ctx| ctx.deadline())
            .is_none_or(|deadline| deadline > Instant::now()),
        "Memory execution deadline elapsed before enqueue"
    );
    Ok(())
}

fn observer_deadline(execution: Option<&crate::core::exec_ctx::ExecutionContext>) -> Instant {
    let ceiling = Instant::now() + MAX_TRANSACTION + Duration::from_secs(10);
    execution
        .and_then(|ctx| ctx.deadline())
        .map_or(ceiling, |deadline| deadline.min(ceiling))
}

async fn observe_response(
    response: tokio::sync::oneshot::Receiver<Result<Value>>,
    cancel: &crate::core::cancel::TurnCancel,
    child_cancel: crate::core::cancel::TurnCancel,
    deadline: Instant,
    _observer: Observer,
) -> Result<Value> {
    struct CancelOnDrop(crate::core::cancel::TurnCancel);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel_on_drop = CancelOnDrop(child_cancel);
    // Only logical observation expires here. The actor still owns admission and physical cleanup.
    tokio::select! {
        biased;
        result = response => result.map_err(|_| unknown_worker("Memory actor stopped without a settlement result"))?,
        _ = cancel.cancelled() => Err(unknown_worker(
            "Memory request cancelled; actor retains physical ownership until settlement")),
        _ = tokio::time::sleep_until(deadline.into()) => Err(unknown_worker(
            "Memory observer deadline elapsed; actor retains physical ownership until settlement")),
    }
}

/// Async orchestration boundary. Prompt builders must only consume an already-adopted snapshot.
pub(crate) async fn request(
    approval: ApprovedInvocation,
    context: api::ProviderContext,
    method: Method,
    params: Value,
    execution: Option<crate::core::exec_ctx::ExecutionContext>,
    cancel: crate::core::cancel::TurnCancel,
) -> Result<Value> {
    check_before_enqueue(execution.as_ref(), &cancel)?;
    let deadline = observer_deadline(execution.as_ref());
    context.validate()?;
    check_invocation_origin(&approval, &context, execution.as_ref())?;
    ensure!(
        execution
            .as_ref()
            .is_none_or(|ctx| ctx.native_scope().is_none()),
        "Memory provider is busy: native-scoped calls must dispatch before tool-body admission"
    );
    manager::selected_at(context.generation)?;
    if let Some(capability) = method.capability() {
        ensure!(
            context.granted_capabilities.contains(&capability),
            "Memory capability was not granted by host"
        );
    }
    if context
        .granted_capabilities
        .contains(&api::Capability::HostLlm)
    {
        ensure!(
            approval.background_learning
                && matches!(method, Method::PrepareJob | Method::CompleteJob),
            "same-turn model jobs require the sealed completed-turn invocation"
        );
    }
    // Validate before enqueueing; no arbitrary large/unbounded JSON is retained by the actor queue.
    let params = operation_params(&context, method, params)?;
    api::strict_json(&serde_json::to_vec(&params)?)?;
    check_effects()?;
    check_before_enqueue(execution.as_ref(), &cancel)?;
    let (reply, response) = tokio::sync::oneshot::channel();
    let sender = actor()?;
    let child_cancel = cancel.child();
    let observer = Observer::new(unsettled());
    if sender
        .try_send(Work {
            approval,
            context,
            method,
            params,
            execution,
            cancel: child_cancel.clone(),
            settlement: observer.settlement.clone(),
            reply,
        })
        .is_err()
    {
        observer.settlement.prove_tree_settled();
        return Err(
            MemoryError::new(ErrorCode::Busy, "Memory actor queue is full or unavailable").into(),
        );
    }
    observe_response(response, &cancel, child_cancel, deadline, observer).await
}

pub(crate) fn request_blocking(
    context: api::ProviderContext,
    method: Method,
    params: Value,
) -> Result<Value> {
    // Called only by the synchronous Tool worker dispatcher, never prompt/render futures. The
    // actor has its own runtime; this bridge does not enter or nest the caller's Tokio runtime.
    let execution = crate::core::exec_ctx::current();
    let parent_cancel = crate::core::cancel::current().unwrap_or_default();
    check_before_enqueue(execution.as_ref(), &parent_cancel)?;
    let deadline = observer_deadline(execution.as_ref());
    ensure!(
        execution
            .as_ref()
            .is_none_or(|ctx| ctx.native_scope().is_none()),
        "Memory cannot reenter native-scoped execution"
    );
    context.validate()?;
    check_invocation_origin(
        &ApprovedInvocation::model_tool(),
        &context,
        execution.as_ref(),
    )?;
    manager::selected_at(context.generation)?;
    let params = operation_params(&context, method, params)?;
    api::strict_json(&serde_json::to_vec(&params)?)?;
    let cancel = parent_cancel.child();
    check_effects()?;
    check_before_enqueue(execution.as_ref(), &parent_cancel)?;
    let sender = actor()?;
    let observer = Observer::new(unsettled());
    let (reply, mut response) = tokio::sync::oneshot::channel();
    if sender
        .try_send(Work {
            approval: ApprovedInvocation::model_tool(),
            context,
            method,
            params,
            execution: execution.clone(),
            cancel: cancel.clone(),
            settlement: observer.settlement.clone(),
            reply,
        })
        .is_err()
    {
        observer.settlement.prove_tree_settled();
        return Err(MemoryError::new(ErrorCode::Busy, "Memory actor queue is full").into());
    }
    struct CancelOnDrop(crate::core::cancel::TurnCancel);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    loop {
        match response.try_recv() {
            Ok(result) => return result,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                return Err(unknown_worker(
                    "Memory actor stopped before confirming settlement",
                ))
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        }
        if parent_cancel.is_cancelled() || Instant::now() >= deadline {
            cancel.cancel();
            return Err(crate::agent::tools::ToolEffectsUnsettled(
                crate::agent::tools::ToolEffectState::UnknownWorker,
            )
            .into());
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

struct Ownership {
    // Dispatch carriers and all admission are held until tree exit AND both pipe drains.
    _execution: Option<crate::core::exec_ctx::ExecutionContext>,
    _admission: Arc<manager::Admission>,
    _lease: crate::core::workspace_txn::MemoryProviderLease,
    _sandbox: crate::sandbox::runner::SpawnGuard,
    containment: Option<Arc<crate::core::proctree::Containment>>,
    marker: PathBuf,
    marker_fingerprint: crate::core::persist::FileFingerprint,
    terminal: Arc<AtomicBool>,
    private_temp: PathBuf,
}
impl Drop for Ownership {
    fn drop(&mut self) {
        if self.terminal.load(Ordering::Acquire) {
            let _ =
                crate::core::persist::remove_if_unchanged(&self.marker, &self.marker_fingerprint);
        }
        // A plugin may put unknown paths/links here. Never recursively delete provider-controlled
        // contents; empty-dir removal is the only automatic cleanup allowed after settlement.
        let _ = std::fs::remove_dir(self.private_temp.join("home"));
        let _ = std::fs::remove_dir(&self.private_temp);
    }
}
static QUARANTINE: Mutex<Vec<Ownership>> = Mutex::new(Vec::new());
struct FailedSpawn {
    _error: crate::core::proctree::ContainedSpawnError<tokio::process::Child>,
    _ownership: Ownership,
}
static FAILED_SPAWNS: Mutex<Vec<FailedSpawn>> = Mutex::new(Vec::new());
fn quarantine(ownership: Ownership) {
    if let Some(containment) = &ownership.containment {
        crate::core::proctree::terminate_tree(containment);
    }
    QUARANTINE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(ownership);
}

fn check_learning_policy(
    approval: &ApprovedInvocation,
    method: Method,
    granted: bool,
) -> Result<()> {
    if approval.background_learning {
        ensure!(
            granted
                && matches!(
                    method,
                    Method::ProcessTurn | Method::PrepareJob | Method::CompleteJob
                ),
            "completed-turn Memory sharing was not explicitly authorized"
        );
        ensure!(
            crate::repl::postturn::memory_auto_learn_enabled(),
            "Memory auto-learning was disabled before launch"
        );
    }
    Ok(())
}

async fn run(
    approval: ApprovedInvocation,
    mut context: api::ProviderContext,
    method: Method,
    params: Value,
    execution: Option<crate::core::exec_ctx::ExecutionContext>,
    cancel: crate::core::cancel::TurnCancel,
    settlement: Arc<SettlementLifetime<()>>,
) -> Result<Value> {
    let mut before_spawn = BeforeSpawnProof::new(settlement.clone());
    check_invocation_origin(&approval, &context, execution.as_ref())?;
    ensure!(
        !cancel.is_cancelled(),
        "Memory request cancelled before admission"
    );
    let root = PathBuf::from(&context.workspace_root);
    let scope = execution
        .as_ref()
        .map(|ctx| ctx.resource_scope())
        .unwrap_or_else(|| format!("memory-cli/{}", context.session_id));
    let deadline = execution
        .as_ref()
        .and_then(|ctx| ctx.deadline())
        .map_or_else(
            || Instant::now() + MAX_TRANSACTION,
            |deadline| deadline.min(Instant::now() + MAX_TRANSACTION),
        );
    ensure!(
        deadline > Instant::now(),
        "Memory execution deadline elapsed before admission"
    );
    let admit = || -> Result<_> {
        let lease =
            crate::core::workspace_txn::MemoryProviderLease::acquire(&root, &scope, Some(&cancel))?;
        let admission = manager::Admission::acquire()?;
        Ok((lease, admission))
    };
    let (lease, admission) = match execution.clone() {
        Some(ctx) => crate::core::exec_ctx::with_current(ctx, admit),
        None => admit(),
    }?;
    let (id, state) = manager::selected_at(context.generation)?;
    let package = install::verify_installed(
        &manager::packages_root(),
        &state.pins,
        manager::host_target()?,
    )?;
    let configured =
        super::configuration::effective_settings(&package.manifest.settings, &state.settings)?;
    let supplied: std::collections::BTreeMap<_, _> = context
        .settings
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "prepared_job" | "graph_recording" | "graph_expansion"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    ensure!(
        configured == supplied,
        "provider settings differ from the reviewed active configuration"
    );
    let budget_caps = state.budgets.clone().unwrap_or_default();
    budget_caps.validate()?;
    ensure!(
        context.budgets.context_tokens <= budget_caps.context_tokens
            && context.budgets.recall_tokens <= budget_caps.recall_tokens
            && context.budgets.max_records <= budget_caps.max_records,
        "provider budgets exceed configured host limits"
    );
    let grant = state
        .grant
        .as_ref()
        .context("Memory execution grant missing")?;
    check_learning_policy(&approval, method, grant.learning)?;
    if context
        .granted_capabilities
        .contains(&api::Capability::HostLlm)
    {
        ensure!(
            grant.turn_model_learning
                && grant.learning
                && package.manifest.permissions.host_llm
                && approval.background_learning
                && matches!(method, Method::PrepareJob | Method::CompleteJob),
            "same-turn model learning was not explicitly authorized"
        );
    }
    let declared: Vec<api::Capability> = package
        .manifest
        .capabilities
        .iter()
        .filter(|cap| !matches!(cap.as_str(), "legacy_adoption_v1" | "legacy_adoption_v2"))
        .map(|cap| serde_json::from_value(json!(cap)))
        .collect::<std::result::Result<_, _>>()?;
    grant_management(&approval, &mut context, &declared);
    ensure!(
        context
            .granted_capabilities
            .iter()
            .all(|cap| declared.contains(cap)),
        "host context capabilities exceed the selected package declarations"
    );
    if let Some(capability) = method.capability() {
        ensure!(
            declared.contains(&capability),
            "unsupported_capability: package does not declare the requested operation"
        );
    }
    #[cfg(windows)]
    ensure!(
        grant.guarded_execution,
        "Windows Memory execution needs informed guarded consent"
    );
    // Data initialization happens only for an actual explicitly authorized active operation.
    let configured_data = manager::data_root_for(&id, &state)?;
    let data =
        super::storage::validate(&configured_data, &id, &manager::read_state()?, Some(&root))?;
    install::ensure_private_root(&data)?;
    context.data_root = std::fs::canonicalize(&data)?.to_string_lossy().into_owned();
    context.validate()?;
    let temp_parent = crate::core::config::aizen_home().join("plugin-temp");
    install::ensure_private_root(&temp_parent)?;
    let private_temp = temp_parent.join(format!(
        "transaction-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    ));
    ensure!(!private_temp.try_exists()?, "Memory private temp collision");
    install::ensure_private_root(&private_temp.join("home"))?;
    let policy = crate::sandbox::request::NativePluginPolicy::new(
        crate::sandbox::request::NativePluginPaths {
            package_root: package.directory.clone(),
            data_root: data,
            model_roots: vec![],
            synthetic_home: private_temp.join("home"),
            private_temp: private_temp.clone(),
        },
        crate::sandbox::request::NativePluginPlatformEnv::for_host()?,
        if grant.network && package.manifest.permissions.network {
            crate::sandbox::request::NativePluginNetworkGrant::Granted
        } else {
            crate::sandbox::request::NativePluginNetworkGrant::Denied
        },
    )?;
    let request = crate::sandbox::request::SandboxRequest::native_plugin(
        crate::sandbox::CommandOrigin::MemoryProvider,
        package.entrypoint.clone(),
        vec![],
        policy,
    )?
    .wall_timeout(MAX_TRANSACTION);
    let prepare = || crate::sandbox::runner::prepare_tokio(request);
    let mut prepared = match execution.clone() {
        Some(ctx) => crate::core::exec_ctx::with_current(ctx, prepare),
        None => prepare(),
    }?;
    ensure!(prepared.degraded.is_empty() || grant.guarded_execution,
        "restricted Memory sandbox degraded; explicit informed guarded-execution consent is required");
    prepared
        .command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let marker = manager::transaction_marker();
    install::ensure_private_root(marker.parent().context("Memory marker has no parent")?)?;
    let bytes = serde_json::to_vec(
        &json!({"schema_version":1,"provider":id,"generation":context.generation,
        "session_id":context.session_id,"turn_id":context.turn_id,"method":method.name(),
        "operation_id":params.get("operation_id").and_then(Value::as_str),
        "params_sha256":super::manifest::sha256(&serde_json::to_vec(&params)?),
        "pid":std::process::id()}),
    )?;
    ensure!(
        !marker.try_exists()?,
        "Memory recovery marker appeared before spawn"
    );
    check_learning_policy(&approval, method, grant.learning)?;
    check_before_spawn(&cancel, deadline)?;
    // Arm cleanup before writing: owner-only hardening can fail after an atomic rename.
    before_spawn.marker = Some((
        marker.clone(),
        crate::core::persist::FileFingerprint::for_bytes(&bytes),
    ));
    crate::core::persist::atomic_write_owner_only(&marker, &bytes)?;
    let terminal = Arc::new(AtomicBool::new(false));
    let spawned =
        before_spawn.spawn_if_live(&cancel, deadline, || prepared.spawn_contained_tokio())?;
    let mut ownership = Ownership {
        _execution: execution,
        _admission: Arc::new(admission),
        _lease: lease,
        _sandbox: prepared.into_guard(),
        containment: None,
        marker,
        marker_fingerprint: crate::core::persist::FileFingerprint::for_bytes(&bytes),
        terminal: terminal.clone(),
        private_temp,
    };
    let (mut child, containment) = match spawned {
        Ok(child) => child,
        Err(error) if error.cleanup_verified() => {
            terminal.store(true, Ordering::Release);
            drop(ownership);
            settlement.prove_tree_settled();
            return Err(error.into());
        }
        Err(error) => {
            unsettled().charge_permanent();
            FAILED_SPAWNS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(FailedSpawn {
                    _error: error,
                    _ownership: ownership,
                });
            return Err(unknown_worker(
                "Memory spawn cleanup unproven; admission quarantined",
            ));
        }
    };
    let containment = Arc::new(containment);
    ownership.containment = Some(containment.clone());
    let lifetime = Arc::new(crate::agent::process::SettlementLifetime::new(
        ownership, quarantine,
    ));
    let stdout_drain = lifetime.drain();
    let stderr_drain = lifetime.drain();
    // Do not publish release until protocol/IO outcome is classified, even if the process exits
    // and both pipes finish first. This is ownership of THIS admitted transaction, not new work.
    let classification_drain = lifetime.drain();
    // Extract all pipes before publishing the monitor's tree proof. Missing pipe paths are retained
    // by the same lifetime and are never interpreted as successful cleanup.
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (released, release) = tokio::sync::oneshot::channel();
    lifetime.on_release(move || {
        settlement.prove_tree_settled();
        let _ = released.send(());
    });
    let stop = crate::agent::mcp::monitor_contained_settlement(
        child,
        containment.clone(),
        lifetime.clone(),
    );
    let budget = transport::IoBudget::default();
    let stderr_budget = budget.clone();
    let (stderr_failure, mut stderr_failed) = tokio::sync::oneshot::channel();
    let stderr_worker = tokio::spawn(async move {
        let _drain = stderr_drain;
        let mut stderr = stderr.context("Memory stderr pipe missing")?;
        let mut buffer = [0u8; 8192];
        loop {
            let count = match stderr.read(&mut buffer).await {
                Ok(count) => count,
                Err(_) => {
                    let _ = stderr_failure.send(());
                    anyhow::bail!("Memory stderr drain failed");
                }
            };
            if count == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            if stderr_budget.charge(count).is_err() {
                let _ = stderr_failure.send(());
                anyhow::bail!("Memory combined transaction IO limit exceeded");
            }
            // Intentionally discard lower-trust stderr instead of leaking arbitrary secret text.
        }
    });
    let operation = async {
        let _drain = stdout_drain;
        let mut connection = transport::Connection::with_budget(
            BufReader::new(stdout.context("Memory stdout pipe missing")?),
            stdin.context("Memory stdin pipe missing")?,
            budget,
        );
        let initialized: api::Initialized = connection
            .request(
                "initialize",
                &api::Initialize {
                    protocol: api::ProtocolVersion {
                        major: api::API_MAJOR,
                        minor: api::API_MINOR,
                    },
                    context: &context,
                },
                Duration::from_secs(5),
            )
            .await?;
        initialized.validate()?;
        ensure!(
            initialized.provider.id == id
                && initialized.provider.version == package.manifest.version,
            "Memory executable identity does not match its pinned manifest"
        );
        let declared: Vec<api::Capability> = package
            .manifest
            .capabilities
            .iter()
            .filter(|cap| !matches!(cap.as_str(), "legacy_adoption_v1" | "legacy_adoption_v2"))
            .map(|cap| serde_json::from_value(json!(cap)))
            .collect::<std::result::Result<_, _>>()?;
        ensure!(
            initialized
                .capabilities
                .iter()
                .all(|cap| declared.contains(cap)),
            "Memory executable advertised undeclared capabilities"
        );
        if let Some(capability) = method.capability() {
            ensure!(
                initialized.capabilities.contains(&capability),
                "Memory provider declined the requested capability"
            );
        }
        let result: api::Result<Value> = connection
            .request(
                method.name(),
                &params,
                Duration::from_secs(if method.long_deadline() { 60 } else { 10 }),
            )
            .await;
        ensure!(
            !connection.is_poisoned(),
            "Memory operation outcome is ambiguous; no retry is permitted"
        );
        if let Ok(value) = &result {
            if matches!(method, Method::Command) {
                super::client::validate_rich_result(&params, value, &context.cwd)?;
            }
            validate_result(method, &params, value)?;
            validate_response_budget(&context, method, &params, value)?;
        }
        let shutdown: Value = connection
            .request("shutdown", &json!({}), Duration::from_secs(2))
            .await?;
        ensure!(
            shutdown == json!({"stopping":true}),
            "invalid Memory shutdown acknowledgement"
        );
        connection.finish_stdout(Duration::from_secs(3)).await?;
        Ok::<_, anyhow::Error>(result)
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = tokio::select! {
        result = tokio::time::timeout(remaining, operation) => result.context("Memory transaction deadline exceeded").and_then(|result| result),
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Memory transaction cancelled; outcome may be ambiguous")),
        _ = async {
            if (&mut stderr_failed).await.is_err() {
                std::future::pending::<()>().await;
            }
        } => Err(anyhow::anyhow!("Memory stderr worker failed before transaction completion")),
    };
    if result.is_err() {
        crate::core::proctree::terminate_tree(&containment);
    }
    drop(stop);
    let stderr_ok = matches!(
        tokio::time::timeout(Duration::from_secs(6), stderr_worker).await,
        Ok(Ok(Ok(())))
    );
    if clean_terminal_reply(&result, stderr_ok) {
        terminal.store(true, Ordering::Release);
    }
    drop(classification_drain);
    let settled = tokio::time::timeout(Duration::from_secs(11), release).await;
    if !matches!(settled, Ok(Ok(()))) {
        return Err(unknown_worker(
            "Memory physical settlement unproven; admission quarantined and recovery required",
        ));
    }
    if !stderr_ok || !matches!(manager::transaction_marker().try_exists(), Ok(false)) {
        return Err(unresolved_outcome(
            "Memory outcome lacks a clean durable receipt; explicit recovery required; no replay permitted",
        ));
    }
    result?.map_err(Into::into)
}

fn validate_response_budget(
    context: &api::ProviderContext,
    method: Method,
    params: &Value,
    value: &Value,
) -> Result<()> {
    if !matches!(
        method,
        Method::Recall | Method::BuildContext | Method::Search | Method::List
    ) {
        return Ok(());
    }
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .context("expected response items")?;
    let requested = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .min(100) as usize;
    ensure!(
        items.len() <= context.budgets.max_records.min(requested),
        "Memory response exceeds configured record cap"
    );
    let tokens = match method {
        Method::Recall => Some(context.budgets.recall_tokens),
        Method::BuildContext => Some(context.budgets.context_tokens),
        _ => None,
    };
    if let Some(budget) = tokens {
        let used = items
            .iter()
            .try_fold(0usize, |sum, item| -> Result<usize> {
                Ok(sum.saturating_add(crate::core::tokens::estimate_str_ceil(
                    &serde_json::to_string(item)?,
                )))
            })?;
        ensure!(
            used <= budget,
            "Memory response exceeds configured token cap"
        );
    }
    Ok(())
}

fn validate_result(method: Method, params: &Value, value: &Value) -> Result<()> {
    match method {
        Method::Recall | Method::Search | Method::List => {
            serde_json::from_value::<api::Hits>(value.clone())?.validate()?
        }
        Method::Get | Method::Store | Method::Update => {
            serde_json::from_value::<api::RecordResult>(value.clone())?.validate()?
        }
        Method::BuildContext => {
            serde_json::from_value::<api::ContextItems>(value.clone())?.validate()?
        }
        Method::Affinity => {
            let result: api::AffinityResult = serde_json::from_value(value.clone())?;
            result.validate()?;
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .min(100);
            ensure!(
                result.items.len() as u64 <= limit,
                "Memory affinity exceeds requested limit"
            );
        }
        Method::PrepareJob => {
            let request: api::PrepareJob = serde_json::from_value(params.clone())?;
            request.validate()?;
            let result: api::PrepareJobResult = serde_json::from_value(value.clone())?;
            result.validate()?;
            ensure!(
                result.operation_id == request.operation_id
                    && result.turn_operation_id == request.turn.operation_id
                    && Some(&result.payload_sha256) == request.turn.payload_sha256.as_ref(),
                "Memory prepared job does not match its request"
            );
        }
        Method::ProcessTurn | Method::CompleteJob => {
            let result: api::ProcessTurnResult = serde_json::from_value(value.clone())?;
            result.validate()?;
            ensure!(
                matches!(
                    (method, &result.learning_mode),
                    (Method::ProcessTurn, api::LearningMode::Offline)
                        | (Method::CompleteJob, api::LearningMode::SameTurnModel)
                ),
                "Memory learning receipt mode does not match its operation"
            );
            ensure!(
                params.get("operation_id").and_then(Value::as_str)
                    == Some(result.operation_id.as_str())
                    && params.get("payload_sha256").and_then(Value::as_str)
                        == result.payload_sha256.as_deref(),
                "Memory completed-turn receipt does not match its request"
            );
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
