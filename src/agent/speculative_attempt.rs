//! Native-only speculative attempt ownership.
//!
//! This module owns the resources that must remain live until a speculative body has
//! physically stopped and every executable context carrier has retired.  It deliberately
//! does not use `Arc::strong_count` as a lifecycle proof.

use crate::agent::task_tool::SubagentSlot;
use crate::agent::tools::workers::ToolWorkerOwner;
use crate::core::exec_ctx::{AttemptCarrierRegistry, ExecutionContext};
use crate::core::native_scope::NativeWriteScope;
use crate::core::speculation::lifecycle::{AttemptLifecycle, WorkerJoinFact};
use crate::core::speculation::store::OverlayHandle;
use crate::core::speculation::workers::DriverResult;
use crate::core::workspace_txn::WorkspaceActivity;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

/// All resources retained by one native speculative attempt.
///
/// The whole value is moved into `QUARANTINE` when the owner is abandoned before a
/// certified release.  In particular, the raw slot and activity must not be dropped while
/// a detached worker or an executable context can still exist.
struct State {
    _background: crate::core::workspace_txn::BackgroundProcessBarrier,
    slot: SubagentSlot,
    activity: Arc<WorkspaceActivity>,
    overlay: OverlayHandle,
    scope: NativeWriteScope,
    owner_scope: String,
    workers: ToolWorkerOwner,
    carriers: Arc<AttemptCarrierRegistry>,
    lifecycle: Option<AttemptLifecycle>,
}

/// Process-local quarantine for attempts whose physical closure was not certified.
///
/// There is intentionally no reaper.  Retention until process exit is safer than claiming
/// cleanup after cancellation, timeout, panic, or an unknown worker state.
static QUARANTINE: std::sync::LazyLock<Mutex<Vec<State>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Controller-owned native speculative attempt.
///
/// Direct execution does not use this type.  The owner never exposes its raw resources or
/// carrier registry to an `ExecutionContext`; it exposes only the weak worker producer and
/// newly acquired context carriers through `bind_context`.
#[must_use]
pub(crate) struct AttemptOwner {
    state: Option<State>,
}

impl AttemptOwner {
    /// Admit one native speculative attempt and retain every release-relevant resource.
    pub(crate) fn new(
        slot: SubagentSlot,
        root: &Path,
        owner_scope: &str,
        overlay: OverlayHandle,
        scope: &NativeWriteScope,
    ) -> Result<Self> {
        // This checks both the sealed overlay owner and that the supplied child authority does
        // not widen it.  The overlay handle is also retained so later token checks use identity,
        // not a caller-controlled id or path reconstruction.
        overlay.check_owner_scope(owner_scope, scope)?;
        overlay.check_scope(scope)?;
        // Resolve sealed resource spellings lexically, without reading mutable source bytes.
        for resource in scope.resources() {
            overlay.resolve(root, resource.path())?;
        }
        let background = crate::core::workspace_txn::BackgroundProcessBarrier::acquire(
            crate::core::cancel::current().as_ref(),
        )?;
        let activity = WorkspaceActivity::begin(root, owner_scope)
            .context("native speculative workspace activity unavailable")?;
        let lifecycle = overlay.lifecycle().cloned();
        // Admit exactly one owner for this overlay before exposing any executable context.
        overlay.admit_attempt(owner_scope, scope)?;
        let workers = match lifecycle.clone() {
            Some(lifecycle) => ToolWorkerOwner::new_with_lifecycle(lifecycle),
            None => ToolWorkerOwner::new(),
        };
        let carriers = AttemptCarrierRegistry::new();
        Ok(Self {
            state: Some(State {
                _background: background,
                slot,
                activity,
                overlay,
                scope: scope.clone(),
                owner_scope: owner_scope.to_owned(),
                workers,
                carriers,
                lifecycle,
            }),
        })
    }

    pub(crate) fn new_required(
        slot: SubagentSlot,
        root: &Path,
        owner_scope: &str,
        overlay: OverlayHandle,
        scope: &NativeWriteScope,
    ) -> Result<Self> {
        anyhow::ensure!(
            overlay.lifecycle().is_some(),
            "speculative worker roster was not frozen before native admission"
        );
        Self::new(slot, root, owner_scope, overlay, scope)
    }

    /// Bind the executable parts of this attempt into a child context.
    /// The context must already carry the exact private overlay, native authority, and
    /// resource scope selected by the controller.  Only the carrier and weak producer are
    /// seated; raw admission/resources never cross this API.
    pub(crate) fn bind_context(&self, ctx: &ExecutionContext) -> Result<ExecutionContext> {
        let state = self
            .state
            .as_ref()
            .context("native speculative attempt owner is already released")?;
        let context_overlay = ctx
            .overlay()
            .context("native speculative context is missing its private overlay")?;
        anyhow::ensure!(
            state.overlay.same_handle(context_overlay),
            "native speculative context carries a different overlay"
        );
        let context_scope = ctx
            .native_scope()
            .context("native speculative context is missing its native scope")?;
        anyhow::ensure!(
            context_scope.same_authority(&state.scope),
            "native speculative context carries a different native authority"
        );
        anyhow::ensure!(
            ctx.resource_scope() == state.owner_scope,
            "native speculative context carries a different owner scope"
        );
        state
            .overlay
            .check_owner_scope(&state.owner_scope, context_scope)?;
        state.overlay.check_scope(context_scope)?;
        Ok(ctx
            .with_attempt_carrier_registry(&state.carriers)
            .with_tool_workers(state.workers.producer()))
    }

    /// Close admission, join every physical body, retire every context carrier, and release
    /// the attempt's resources.  A cancelled/failed future drops this owner while `state` is
    /// still present, so `Drop` quarantines the complete state instead of releasing it.
    #[cfg(test)]
    pub(crate) async fn finish(self, deadline: Instant) -> Result<ReleasedAttempt> {
        self.finish_with_driver(deadline, DriverResult::Completed)
            .await
    }

    pub(crate) async fn finish_with_driver(
        mut self,
        deadline: Instant,
        driver: DriverResult,
    ) -> Result<ReleasedAttempt> {
        let state = self
            .state
            .as_mut()
            .context("native speculative attempt owner is already released")?;
        let joined = state.workers.close_and_drain(deadline).await?;
        state.carriers.wait_empty(deadline).await.map_err(|error| {
            error.context(crate::agent::tools::ToolEffectsUnsettled(
                crate::agent::tools::ToolEffectState::UnknownWorker,
            ))
        })?;
        // Close overlay body admission and snapshot generation only after all executable
        // holders have retired. Failure keeps the whole state available to Drop, just like
        // a body/carrier timeout. A retained controller handle cannot admit new bodies later.
        let generation = state
            .overlay
            .finish_attempt(&state.owner_scope)
            .map_err(|error| {
                error.context(crate::agent::tools::ToolEffectsUnsettled(
                    crate::agent::tools::ToolEffectState::UnknownWorker,
                ))
            })?;

        let state = self
            .state
            .take()
            .expect("attempt state remains present until release proof completes");
        let State {
            _background,
            slot,
            activity,
            overlay,
            scope,
            owner_scope,
            workers,
            carriers,
            lifecycle,
        } = state;
        // Keep the order explicit: all physical/context proof has completed before any
        // release-relevant field is dropped.  `workers` is already drained and admission is
        // closed; dropping it cannot detach an unjoined body on this success path.
        drop(workers);
        drop(carriers);
        drop(activity);
        drop(slot);
        Ok(ReleasedAttempt {
            overlay,
            scope,
            owner_scope,
            generation,
            lifecycle,
            facts: joined.facts,
            driver,
        })
    }
}

/// Capture the loop's typed terminal result before settlement; a later cancel flag is not
/// evidence that a completed driver was cancelled.
pub(crate) fn driver_result(
    result: Result<&crate::agent::AgentOutcome, &anyhow::Error>,
) -> DriverResult {
    match result {
        Ok(outcome) => match outcome.stop {
            crate::agent::StopReason::Done => DriverResult::Completed,
            crate::agent::StopReason::Cancelled => DriverResult::Cancelled,
            _ => DriverResult::Failed,
        },
        Err(_) => DriverResult::Failed,
    }
}

impl Drop for AttemptOwner {
    fn drop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        // This is only an admission close.  It is not a body join or a release receipt.
        state.workers.close_admission();
        QUARANTINE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(state);
    }
}

/// Volatile proof that one exact native speculative attempt released its resources.
///
/// This token is private-field and non-Clone.  It is consumed by `consume_for`, including on
/// mismatch, so a failed identity check cannot be retried with a different overlay.
#[derive(Debug)]
#[must_use]
pub(crate) struct ReleasedAttempt {
    overlay: OverlayHandle,
    scope: NativeWriteScope,
    owner_scope: String,
    generation: u64,
    lifecycle: Option<AttemptLifecycle>,
    facts: Vec<WorkerJoinFact>,
    driver: DriverResult,
}

impl ReleasedAttempt {
    /// Consume this proof for exactly the overlay that produced it.
    #[cfg(test)]
    pub(crate) fn consume_for(self, overlay: &OverlayHandle) -> Result<()> {
        self.check_overlay(overlay)
    }

    pub(crate) fn into_joined_for(
        self,
        lifecycle: &AttemptLifecycle,
        overlay: &OverlayHandle,
    ) -> Result<(Vec<WorkerJoinFact>, DriverResult)> {
        self.check_overlay(overlay)?;
        anyhow::ensure!(
            self.lifecycle
                .as_ref()
                .is_some_and(|bound| bound.same_attempt(lifecycle)),
            "released attempt belongs to another live lifecycle"
        );
        Ok((self.facts, self.driver))
    }

    fn check_overlay(&self, overlay: &OverlayHandle) -> Result<()> {
        anyhow::ensure!(
            self.overlay.same_handle(overlay),
            "released speculative attempt token belongs to a different overlay"
        );
        anyhow::ensure!(
            overlay.owner() == self.owner_scope,
            "released speculative attempt token owner differs from overlay"
        );
        overlay.check_owner_scope(&self.owner_scope, &self.scope)?;
        overlay.check_scope(&self.scope)?;
        anyhow::ensure!(
            overlay.generation()? == self.generation,
            "released speculative attempt overlay changed after release"
        );
        Ok(())
    }
}

/// Release a real idle native attempt for parent integration fixtures.
///
/// This does not construct a token directly: it takes a real isolated admission slot,
/// registers activity, and runs the production close/drain/carrier/release path.  The
/// fixture must already have a Tokio runtime and pass its real captured overlay/scope.
#[cfg(test)]
pub(super) fn release_idle_for_test(
    root: &Path,
    overlay: &OverlayHandle,
    scope: &NativeWriteScope,
) -> Result<ReleasedAttempt> {
    use futures_util::FutureExt;
    use std::sync::atomic::AtomicUsize;

    let temp = crate::core::file_placement::tests::TestDir::new();
    let active: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    let gate =
        crate::agent::task_tool::SubagentGate::isolated(active, 1, temp.path().join("slots"));
    let slot = gate
        .try_acquire()
        .map_err(|error| anyhow::anyhow!("idle native fixture admission failed: {error:?}"))?;
    let owner = AttemptOwner::new(slot, root, overlay.owner(), overlay.clone(), scope)?;
    owner
        .finish(Instant::now() + std::time::Duration::from_secs(10))
        .now_or_never()
        .context("idle native fixture unexpectedly required asynchronous settlement")?
}

#[cfg(test)]
#[path = "speculative_attempt_tests.rs"]
mod tests;
