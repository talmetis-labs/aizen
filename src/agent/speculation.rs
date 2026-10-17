//! Parent-owned coordination for native speculative cohorts.
//!
//! A JSON execution mode only requests this lane. Immutable exact scopes and in-process handles
//! still grant authority; durable records never recreate a running controller after restart.

use crate::core::cancel::TurnCancel;
use crate::core::exec_ctx::ExecutionContext;
use crate::core::native_scope::NativeWriteScope;
use crate::core::speculation::integration::{
    IntegrationPlan, IntegrationReceipt as IntegrationOutcome, PlanningOutcome,
};
use crate::core::speculation::lifecycle::CohortLifecycle;
use crate::core::speculation::merge;
use crate::core::speculation::store::{
    BaseSnapshot, ControllerReservation, OverlayHandle, SealedOverlay,
};
use crate::core::speculation::verification::{
    DurableEffectState, VerificationFinished, VerificationStarted, VerificationSummary,
    FINISH_LABEL, START_LABEL,
};
use crate::core::workspace_txn::{
    NativeResource, PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    #[default]
    Direct,
    Speculative,
}

impl ExecutionMode {
    pub(crate) fn from_args(args: &serde_json::Value) -> Result<Self> {
        match args.get("execution") {
            None => Ok(Self::Direct),
            Some(value) => serde_json::from_value(value.clone())
                .context("execution must be direct or speculative"),
        }
    }
}

/// Parent-sealed private bytes backed by a consumed volatile native-attempt release token.
/// Raw `SealedOverlay` values alone do not authorize production cohort integration.
pub(crate) struct ReleasedOverlay {
    sealed: SealedOverlay,
}

pub(crate) struct Cohort {
    parent: String,
    base: BaseSnapshot,
    binding: Option<crate::features::timemachine::TimeMachineBinding>,
    members: Mutex<Vec<OverlayHandle>>,
    lifecycle: Mutex<Option<Arc<CohortLifecycle>>>,
    integration_started: Mutex<bool>,
    applied_identity: Mutex<Option<[u8; 32]>>,
    verification_reservation: Mutex<Option<ControllerReservation>>,
    #[cfg(test)]
    verification_test: VerificationTest,
}

impl std::fmt::Debug for Cohort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cohort")
            .field("base", &self.base.id())
            .finish_non_exhaustive()
    }
}

impl Cohort {
    pub(crate) fn capture(
        parent: &ExecutionContext,
        scopes: &[NativeWriteScope],
        cancel: Option<&TurnCancel>,
    ) -> Result<Arc<Self>> {
        check_parent(parent)?;
        super::hooks::check_speculative_policy()?;
        ensure!(
            !scopes.is_empty() && scopes.len() <= 32,
            "speculative cohort requires 1..=32 exact scopes"
        );
        let root = parent
            .placement()
            .context("speculation requires a bound workspace")?
            .workspace_root();
        let identity = WorkspaceIdentity::discover(root)?;
        ensure!(
            identity.canonical_root == root,
            "speculation requires the canonical workspace root"
        );
        for scope in scopes {
            scope.check_narrowing(scope)?;
            for resource in scope.resources() {
                if scope.write_keys().iter().any(|key| key == resource.key()) {
                    check_write_target(resource.path())?;
                }
            }
        }
        let planned = crate::features::timemachine::plan_speculation_binding(root)?;
        let scope = parent.resource_scope();
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::from_secs(15),
            cancel,
            "capture speculative base",
            &scope,
        )?;
        let physical = PhysicalEffectLease::exclusive(
            Duration::from_secs(15),
            cancel,
            "capture speculative base",
            &scope,
        )?;
        check_cancel(cancel)?;
        super::hooks::check_speculative_policy()?;
        ensure!(
            WorkspaceIdentity::discover(root)? == identity,
            "workspace identity changed before capture"
        );
        let binding = planned
            .map(|plan| plan.admit(&writer, &physical))
            .transpose()?;
        let store_root = match &binding {
            Some(binding) => binding.namespace_dir().to_path_buf(),
            None => non_git_store(&identity)?,
        };
        let base = match &binding {
            Some(binding) => binding.with_transaction_lock(&writer, &physical, || {
                BaseSnapshot::capture(&identity, scopes, &store_root, &writer, &physical)
            })?,
            None => BaseSnapshot::capture(&identity, scopes, &store_root, &writer, &physical)?,
        };
        // Fail before any child starts on unsupported text, not after a model has edited it.
        for resource in base.resources() {
            if let Some(bytes) = base.image(&resource)? {
                ensure!(
                    !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok(),
                    "speculative native files must be UTF-8 text without NUL bytes"
                );
            }
        }
        Ok(Arc::new(Self {
            parent: scope,
            base,
            binding,
            members: Mutex::new(Vec::new()),
            lifecycle: Mutex::new(None),
            integration_started: Mutex::new(false),
            applied_identity: Mutex::new(None),
            verification_reservation: Mutex::new(None),
            #[cfg(test)]
            verification_test: VerificationTest::default(),
        }))
    }

    pub(crate) fn base(&self) -> &BaseSnapshot {
        &self.base
    }

    pub(crate) fn overlay(&self, owner: &str, scope: &NativeWriteScope) -> Result<OverlayHandle> {
        ensure!(
            owner.starts_with(&format!("{}/", self.parent)),
            "overlay owner is outside coordinating parent"
        );
        let started = self
            .integration_started
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort state poisoned"))?;
        ensure!(
            !*started,
            "cohort integration already started; admission is closed"
        );
        let mut members = self
            .members
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort members poisoned"))?;
        ensure!(
            self.lifecycle
                .lock()
                .map_err(|_| anyhow::anyhow!("cohort lifecycle poisoned"))?
                .is_none(),
            "speculative cohort membership is frozen; no new overlay can be admitted"
        );
        ensure!(
            !members.iter().any(|member| member.owner() == owner),
            "duplicate speculative attempt owner"
        );
        let overlay = self.base.create_overlay(owner, scope)?;
        members.push(overlay.clone());
        Ok(overlay)
    }

    /// Freeze stable member order and publish the live worker roster before any child can admit a
    /// native attempt. The returned controller is retained by the cohort; durable JSON cannot
    /// recreate it after restart.
    pub(crate) fn freeze_lifecycle(&self) -> Result<Arc<CohortLifecycle>> {
        let members = self
            .members
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort members poisoned"))?;
        let mut lifecycle_slot = self
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort lifecycle poisoned"))?;
        if let Some(lifecycle) = lifecycle_slot.as_ref() {
            return Ok(lifecycle.clone());
        }
        ensure!(
            !members.is_empty() && members.len() <= 32,
            "speculative cohort has no bounded members"
        );
        let roster = members
            .iter()
            .enumerate()
            .map(|(index, overlay)| {
                let scope = overlay.scope().clone();
                let scope_sha256 = crate::core::speculation::workers::scope_sha256(
                    scope.resources().iter().map(|resource| {
                        (
                            resource.key(),
                            resource.path(),
                            scope.write_keys().iter().any(|key| key == resource.key()),
                        )
                    }),
                )?;
                Ok((
                    crate::core::speculation::workers::AttemptMember {
                        attempt: u32::try_from(index + 1)?,
                        owner: overlay.owner().to_owned(),
                        overlay: overlay.id().to_owned(),
                        scope_sha256,
                    },
                    scope,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let lifecycle = CohortLifecycle::freeze(&self.base, &self.parent, roster)?;
        for (index, overlay) in members.iter().enumerate() {
            let scope = overlay.scope().clone();
            overlay.bind_lifecycle(lifecycle.attempt(index, &scope)?)?;
        }
        *lifecycle_slot = Some(lifecycle.clone());
        Ok(lifecycle)
    }

    fn lifecycle(&self) -> Result<Option<Arc<CohortLifecycle>>> {
        Ok(self
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort lifecycle poisoned"))?
            .clone())
    }

    /// Consume volatile native-attempt release proof before granting parent-side sealing.
    pub(crate) fn seal_released(
        &self,
        parent: &ExecutionContext,
        overlay: &OverlayHandle,
        released: super::speculative_attempt::ReleasedAttempt,
    ) -> Result<ReleasedOverlay> {
        check_parent(parent)?;
        ensure!(
            parent.resource_scope() == self.parent,
            "only the coordinating parent may seal this cohort"
        );
        let lifecycle = match self.lifecycle()? {
            Some(lifecycle) => lifecycle,
            None => {
                #[cfg(test)]
                if self.verification_test.lock_home.is_some() {
                    released.consume_for(overlay)?;
                    return Ok(ReleasedOverlay {
                        sealed: self.seal_after_driver(parent, overlay)?,
                    });
                }
                bail!("speculative cohort has no frozen worker roster");
            }
        };
        lifecycle.check_member(overlay)?;
        let attempt = overlay
            .lifecycle()
            .context("private overlay lost its live lifecycle")?;
        let publication = attempt.publish_joined(released, overlay)?;
        let sealed = self.seal_after_driver(parent, overlay)?;
        ensure!(
            attempt.publish_sealed(publication, sealed.manifest_identity()?)?,
            "speculative driver or native worker failed; sealed private evidence retained"
        );
        Ok(ReleasedOverlay { sealed })
    }

    /// Checks private-body quiescence only; production callers must first consume release proof.
    fn seal_after_driver(
        &self,
        parent: &ExecutionContext,
        overlay: &OverlayHandle,
    ) -> Result<SealedOverlay> {
        check_parent(parent)?;
        ensure!(
            parent.resource_scope() == self.parent,
            "only the coordinating parent may seal this cohort"
        );
        let members = self
            .members
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort members poisoned"))?;
        ensure!(
            members.iter().any(|member| member.same_handle(overlay)),
            "private overlay was not admitted by this coordinator"
        );
        let sealed = overlay.seal(overlay.owner())?;
        ensure!(
            sealed.belongs_to(&self.base),
            "private overlay belongs to a foreign base"
        );
        Ok(sealed)
    }

    pub(crate) fn integrate(
        &self,
        parent: &ExecutionContext,
        released: &[ReleasedOverlay],
        cancel: Option<&TurnCancel>,
    ) -> Result<IntegrationOutcome> {
        let overlays = released
            .iter()
            .map(|candidate| candidate.sealed.clone())
            .collect::<Vec<_>>();
        check_parent(parent)?;
        ensure!(
            parent.resource_scope() == self.parent,
            "only the coordinating parent may integrate this cohort"
        );
        super::hooks::check_speculative_policy()?;
        {
            let mut started = self
                .integration_started
                .lock()
                .map_err(|_| anyhow::anyhow!("cohort state poisoned"))?;
            ensure!(!*started, "cohort integration is single-use; inspect the retained transaction before retrying");
            let members = self
                .members
                .lock()
                .map_err(|_| anyhow::anyhow!("cohort members poisoned"))?;
            ensure!(
                !members.is_empty() && members.len() == overlays.len(),
                "integration requires the complete admitted cohort"
            );
            let mut selected = std::collections::BTreeSet::new();
            for overlay in &overlays {
                ensure!(
                    overlay.belongs_to(&self.base) && selected.insert(overlay.id()),
                    "duplicate or foreign sealed overlay"
                );
                ensure!(
                    members
                        .iter()
                        .any(|member| member.id() == overlay.id()
                            && member.owner() == overlay.owner()),
                    "sealed overlay was not admitted by this coordinator"
                );
            }
            if let Some(lifecycle) = self.lifecycle()? {
                lifecycle.check_complete(
                    &overlays
                        .iter()
                        .map(SealedOverlay::manifest_identity)
                        .collect::<Result<Vec<_>>>()?,
                )?;
            } else {
                #[cfg(not(test))]
                bail!("speculative integration requires a frozen physical-worker roster");
                #[cfg(test)]
                ensure!(
                    self.verification_test.lock_home.is_some(),
                    "speculative integration requires a frozen physical-worker roster"
                );
            }
            *started = true;
        }
        check_cancel(cancel)?;
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            self.base.identity().clone(),
            Duration::from_secs(15),
            cancel,
            "integrate speculative cohort",
            &self.parent,
        )?;
        let physical = PhysicalEffectLease::exclusive(
            Duration::from_secs(15),
            cancel,
            "integrate speculative cohort",
            &self.parent,
        )?;
        check_cancel(cancel)?;
        super::hooks::check_speculative_policy()?;
        ensure!(
            WorkspaceIdentity::discover(&self.base.identity().canonical_root)?
                == *self.base.identity(),
            "workspace identity changed since speculative capture"
        );
        // Planning may publish a durable conflict. Keep it in the same actual transaction domain
        // as preparation/application so Time Machine cannot sweep or recover between these stages.
        let apply = || {
            let plan = match IntegrationPlan::build(parent, &self.base, &overlays, &merge::Limits::default())? {
                PlanningOutcome::Ready(plan) => plan,
                PlanningOutcome::Conflicted(conflict) => bail!("speculative integration conflict: {conflict:?}; overlays retained, no live edits applied"),
            };
            *self
                .verification_reservation
                .lock()
                .map_err(|_| anyhow::anyhow!("cohort verification state poisoned"))? = Some(
                self.base
                    .reserve_controller_metadata(&self.parent, 2, 256 * 1024)?,
            );
            plan.apply(parent, &writer, &physical, cancel)
        };
        let result = crate::core::exec_ctx::with_current(parent.clone(), || match &self.binding {
            Some(binding) => binding.with_transaction_lock(&writer, &physical, apply),
            None => apply(),
        })?;
        *self
            .applied_identity
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort verification state poisoned"))? =
            super::verify_gate::source_fingerprint(&self.base.identity().canonical_root);
        Ok(result)
    }

    pub(crate) async fn verify_applied(
        self: &Arc<Self>,
        parent: &ExecutionContext,
        outcome: &IntegrationOutcome,
        cancel: Option<&TurnCancel>,
        timeout_secs: u64,
    ) -> Result<String> {
        self.verify_applied_with_gate(parent, outcome, cancel, || {
            super::verify_gate::run_verify_gate_in_context(
                &self.base.identity().canonical_root,
                timeout_secs,
                parent,
            )
        })
        .await
    }

    async fn verify_applied_with_gate<F, Fut>(
        self: &Arc<Self>,
        parent: &ExecutionContext,
        outcome: &IntegrationOutcome,
        cancel: Option<&TurnCancel>,
        gate: F,
    ) -> Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<super::verify_gate::VerifyGateResult>>,
    {
        check_parent(parent)?;
        ensure!(
            parent.resource_scope() == self.parent,
            "verification requires the coordinating parent"
        );
        ensure!(
            outcome.state == crate::core::speculation::integration::ReceiptState::AppliedUnverified
                && outcome.postimage_verified,
            "partial or unchecked integration cannot be verified"
        );
        let expected = *self
            .applied_identity
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort verification state poisoned"))?;
        // Claim once BEFORE either publication or process execution. An abandoned/erroring attempt
        // consumes the capability; neither a second caller nor restart evidence may rerun the gate.
        let mut reservation = self
            .verification_reservation
            .lock()
            .map_err(|_| anyhow::anyhow!("cohort verification state poisoned"))?
            .take()
            .context("verification is single-use or was not reserved")?;
        let started = VerificationStarted {
            schema: 1,
            lifecycle_version: 1,
            state: "started".into(),
            base: self.base.id().to_owned(),
            owner: self.parent.clone(),
            prepared: outcome.prepared.as_str().to_owned(),
            integration: outcome.receipt.as_str().to_owned(),
            source_identity: expected,
        };
        started.validate()?;
        let cohort = self.clone();
        let parent_ctx = parent.clone();
        let (mut reservation, start_id) = tokio::task::spawn_blocking(move || {
            let start_id = cohort.publish_verification(&parent_ctx, || {
                #[cfg(test)]
                cohort
                    .verification_test
                    .fail(VerificationFault::BeforeStart)?;
                let id = cohort.base.controller_record_reserved(
                    &mut reservation,
                    START_LABEL,
                    &serde_json::to_vec(&started)?,
                    &[],
                )?;
                crate::core::speculation::verification::validate_verification_records(
                    cohort.base.id(),
                    &cohort.base.inspect_controller_records()?,
                )?;
                #[cfg(test)]
                cohort
                    .verification_test
                    .fail(VerificationFault::AfterStart)?;
                Ok(id)
            })?;
            Ok::<_, anyhow::Error>((reservation, start_id))
        })
        .await
        .context("verification start publication worker failed")??;
        // The durable start means attempt intent, NOT proof a process spawned. All start guards
        // are gone before the gate acquires its own fresh process-body admission.
        let root = &self.base.identity().canonical_root;
        let eligible = expected.is_some()
            && super::verify_gate::source_fingerprint(root) == expected
            && !cancel.is_some_and(TurnCancel::is_cancelled)
            && super::hooks::check_speculative_policy().is_ok();
        let result = if eligible { gate().await } else { None };
        let requested_state = match &result {
            Some(result) if result.accepted() => "verified",
            Some(result) if !result.stable => "stale",
            Some(_) => "verification_failed",
            None => "unverified",
        };
        let command = result.as_ref().map(|result| result.command.clone());
        let accepted = result.as_ref().is_some_and(|result| result.accepted());
        let verifier_effect = result
            .as_ref()
            .map_or(super::tools::ToolEffectState::Settled, |result| {
                result.effect_state
            });
        let effect_state = durable_verifier_effect(verifier_effect)?;
        let read_dependencies = outcome
            .read_dependencies
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let applied_paths = outcome.applied_paths.clone();
        let read_dependency_status = if read_dependencies.is_empty() {
            "none"
        } else {
            "requires_semantic_review"
        };
        let cohort = self.clone();
        let parent_ctx = parent.clone();
        let prepared = outcome.prepared.clone();
        let integration = outcome.receipt.clone();
        let verification_started = start_id.as_str().to_owned();
        // Fresh publication admission is independent of the gate's process-body ownership. In
        // particular, an unknown worker may block this; the retained start then stays unresolved.
        let publication = tokio::task::spawn_blocking(move || {
            cohort.publish_verification(&parent_ctx, || {
                #[cfg(test)]
                cohort.verification_test.fail(VerificationFault::BeforeFinish)?;
                let stable = expected.is_some()
                    && super::verify_gate::source_fingerprint(&cohort.base.identity().canonical_root) == expected
                    && super::hooks::check_speculative_policy().is_ok();
                let state = if stable { requested_state } else { "stale" };
                let finished = VerificationFinished {
                    schema: 1, lifecycle_version: 1, state: state.into(),
                    base: cohort.base.id().to_owned(), owner: cohort.parent.clone(),
                    prepared: prepared.as_str().to_owned(), integration: integration.as_str().to_owned(),
                    verification_started, source_identity: expected, command,
                    accepted: accepted && stable, effect_state,
                    applied_paths, read_dependencies,
                    read_dependency_status: read_dependency_status.into(),
                    scope: "local source drift check; not semantic serializability or complete build-input provenance".into(),
                };
                finished.validate()?;
                let receipt = cohort.base.controller_record_reserved(
                    &mut reservation, FINISH_LABEL, &serde_json::to_vec(&finished)?, &[],
                )?;
                Ok((receipt, stable, state))
            })
        }).await.context("verification publication worker failed").and_then(|result| result);
        let (receipt, stable, state) =
            settle_verification_publication(verifier_effect, publication)?;
        match result {
            Some(result) if result.accepted() && stable => {
                let summary = VerificationSummary {
                    summary_version: 1, lifecycle_version: 1, base: self.base.id().to_owned(),
                    prepared: outcome.prepared.as_str().to_owned(), integration: outcome.receipt.as_str().to_owned(),
                    verification_started: start_id.as_str().to_owned(), verification: receipt.as_str().to_owned(),
                    source_identity: expected.context("accepted verification lacks source identity")?,
                    state: "verified".into(), command: result.command, effect_state,
                    applied_paths: outcome.applied_paths.clone(),
                    read_dependencies: outcome.read_dependencies.iter().map(serde_json::to_value)
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                    read_dependency_status: read_dependency_status.into(),
                    scope: "command passed on stable local source; peer findings remain base-scoped, not semantic serializability".into(),
                };
                summary.validate(self.base.id())?;
                let raw = serde_json::to_string(&summary)?;
                crate::core::speculation::verification::parse_success_summary(&raw, self.base.id())?;
                Ok(raw)
            },
            Some(result) => bail!("integration {} applied but {state}; verification record {}: {}",
                outcome.receipt.as_str(), receipt.as_str(), result.output),
            None => bail!("integration {} applied but {state}: no accepted verification result on a fresh admitted tree; record {}",
                outcome.receipt.as_str(), receipt.as_str()),
        }
    }

    fn publish_verification<T>(
        &self,
        parent: &ExecutionContext,
        publication: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let publish = || {
            check_parent(parent)?;
            ensure!(
                !self.parent.is_empty() && parent.resource_scope() == self.parent,
                "verification publication requires the exact coordinating parent"
            );
            crate::core::exec_ctx::with_current(parent.clone(), || {
                let current = crate::core::exec_ctx::current()
                    .context("verification parent context missing")?;
                check_parent(&current)?;
                ensure!(
                    current.resource_scope() == self.parent,
                    "verification publication current parent changed"
                );
                let writer = WorkspaceWriterLease::acquire_identity_scoped(
                    self.base.identity().clone(),
                    Duration::from_secs(15),
                    None,
                    "publish speculative verification",
                    &self.parent,
                )?;
                let physical = PhysicalEffectLease::exclusive(
                    Duration::from_secs(15),
                    None,
                    "publish speculative verification",
                    &self.parent,
                )?;
                ensure!(
                    WorkspaceIdentity::discover(&self.base.identity().canonical_root)?
                        == *self.base.identity(),
                    "workspace identity changed before verification publication"
                );
                match &self.binding {
                    Some(binding) => binding.with_transaction_lock(&writer, &physical, publication),
                    None => publication(),
                }
            })
        };
        #[cfg(test)]
        if let Some(home) = &self.verification_test.lock_home {
            return crate::core::workspace_txn::with_test_lock_home(home, publish);
        }
        publish()
    }
}

type CohortCall = (
    usize,
    Arc<dyn super::tools::Tool>,
    super::tools::PreparedToolCall,
);

/// Approval has already completed for every member. Results retain call order, never finish order.
pub(crate) async fn run_task_cohort(
    calls: Vec<CohortCall>,
    cfg: &super::AgentConfig,
) -> Vec<(
    usize,
    super::tools::PreparedToolCall,
    super::tools::ToolOutcome,
)> {
    let scopes: Vec<_> = calls
        .iter()
        .filter_map(|(_, _, call)| super::task_tool::speculative_scope(call))
        .collect();
    let parent = cfg.exec_ctx.clone();
    let cancel = cfg.cancel.clone();
    let captured = tokio::task::spawn_blocking(move || {
        crate::core::cancel::with_current(cancel.clone(), || {
            crate::core::exec_ctx::with_current(parent.clone(), || {
                Cohort::capture(&parent, &scopes, Some(&cancel))
            })
        })
    })
    .await;
    let cohort = match captured {
        Ok(Ok(cohort)) => cohort,
        other => {
            let message = match other {
                Ok(Err(error)) => {
                    format!("error: speculative capture failed; no children started: {error:#}")
                }
                Err(error) => format!(
                    "error: speculative capture worker failed; no children started: {error}"
                ),
                Ok(Ok(_)) => unreachable!(),
            };
            return calls
                .into_iter()
                .map(|(index, _, call)| (index, call, super::tools::ToolOutcome::failed(&message)))
                .collect();
        }
    };
    let mut seated = Vec::new();
    for (index, tool, call) in &calls {
        match super::task_tool::seat_speculative(call, cohort.clone(), &cfg.exec_ctx) {
            Ok(call) => seated.push((*index, tool.clone(), call)),
            Err(error) => {
                return calls
                    .into_iter()
                    .map(|(index, _, call)| {
                        (
                            index,
                            call,
                            super::tools::ToolOutcome::failed(format!(
                            "error: speculative admission failed; no children started: {error:#}"
                        )),
                        )
                    })
                    .collect()
            }
        }
    }
    if let Err(error) = cohort.freeze_lifecycle() {
        let message = format!(
            "error: speculative worker roster could not be frozen; no children started: {error:#}"
        );
        return calls
            .into_iter()
            .map(|(index, _, call)| (index, call, super::tools::ToolOutcome::failed(&message)))
            .collect();
    }
    let mut driver_owner = super::tools::workers::ToolWorkerOwner::new();
    #[cfg(test)]
    if let Some(probe) = cfg.exec_ctx.speculative_driver_join_probe() {
        driver_owner.observe_joins(probe.clone());
    }
    let driver_producer = driver_owner.producer();
    let mut workers = Vec::new();
    for (index, tool, call) in seated {
        let cancel = cfg.cancel.clone();
        let ctx = cfg.exec_ctx.clone();
        let body = call.clone();
        let quiet = cfg.quiet && !crate::ui::events::on();
        let budgets = cfg.result_budgets();
        workers.push((
            index,
            super::tools::StartedSafeCall::start_settling_mutation(
                format!("speculation-{}-{index}", cohort.base().id()),
                call,
                cancel.clone(),
                super::safe_tool_deadline(cfg),
                Some(&driver_producer),
                move || {
                    crate::core::cancel::with_current(cancel, || {
                        crate::core::exec_ctx::with_current(ctx, || {
                            super::run_prepared_tool_body(tool, &body, quiet, budgets, None)
                        })
                    })
                },
            ),
        ));
    }
    let mut results = Vec::new();
    for (index, worker) in workers {
        let frozen = worker.prepared().clone();
        let outcome = match worker.await {
            Ok(record) => record.outcome,
            Err(error) => super::tools::ToolOutcome::failed_with_effect_state(
                format!("error: speculative worker failed: {error}; overlays retained"),
                super::tools::ToolEffectState::UnknownWorker,
            ),
        };
        results.push((index, frozen, outcome));
    }
    let driver_join = driver_owner
        .close_and_drain(tokio::time::Instant::now() + Duration::from_secs(15))
        .await;
    let sealed = driver_join.and_then(|_joined| {
        results
            .iter()
            .map(|(_, call, outcome)| {
                ensure!(
                    outcome.failure.is_none()
                        && outcome.effect_state == super::tools::ToolEffectState::Settled,
                    "a speculative child failed or is physically unsettled"
                );
                super::task_tool::seal_speculative_after_driver(call, &cfg.exec_ctx)
            })
            .collect::<Result<Vec<_>>>()
    });
    let live_changes = Arc::new(crate::core::exec_ctx::WorkspaceChanges::default());
    let integration = match sealed {
        Ok(sealed) if !cfg.cancel.is_cancelled() => {
            let parent = cfg.exec_ctx.with_workspace_changes(live_changes.clone());
            let cancel = cfg.cancel.clone();
            let integrating = cohort.clone();
            match tokio::task::spawn_blocking(move || {
                crate::core::cancel::with_current(cancel.clone(), || {
                    integrating.integrate(&parent, &sealed, Some(&cancel))
                })
            })
            .await
            {
                Ok(Ok(receipt)) => {
                    cohort
                        .verify_applied(
                            &cfg.exec_ctx,
                            &receipt,
                            Some(&cfg.cancel),
                            cfg.verify_gate_timeout_secs,
                        )
                        .await
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(anyhow::Error::new(super::tools::ToolEffectsUnsettled(
                    super::tools::ToolEffectState::UnknownWorker,
                )))
                .context(format!("speculative integration worker failed: {error}")),
            }
        }
        Ok(_) => Err(anyhow::anyhow!(
            "cancelled after private editing; no integration started"
        )),
        Err(error) => {
            Err(error.context("complete cohort was not selected; no live integration started"))
        }
    };
    // Track physical effects even when verification or receipt publication fails. Never infer
    // live changes from private authorship or from a child's textual success report.
    let applied_paths = live_changes.paths();
    for (_, call, _) in &results {
        if let (Some(changes), Some(scope)) = (
            &call.workspace_changes,
            super::task_tool::speculative_scope(call),
        ) {
            let paths: Vec<_> = scope
                .resources()
                .iter()
                .filter(|resource| {
                    scope.write_keys().iter().any(|key| key == resource.key())
                        && applied_paths.iter().any(|path| path == resource.path())
                })
                .map(|resource| resource.path().to_path_buf())
                .collect();
            if !paths.is_empty() {
                changes.record(&paths);
            }
        }
    }
    match integration {
        Ok(receipt) => {
            for (_, call, outcome) in &mut results {
                super::task_tool::finish_speculative_status(call, true, &receipt);
                outcome.observation.text = format!(
                    "[speculative integration verified: {receipt}]\n{}",
                    outcome.observation.text
                );
            }
        }
        Err(error) => {
            let effect = super::tool_error_effect_state(&error);
            for (_, call, outcome) in &mut results {
                super::task_tool::finish_speculative_status(call, false, &format!("{error:#}"));
                let message = format!("error: speculative cohort not verified: {error:#}; private records retained\n{}", outcome.observation.text);
                outcome.observation.text = message.clone();
                outcome.failure = Some(message);
                if effect != super::tools::ToolEffectState::Settled {
                    outcome.effect_state = effect;
                }
            }
        }
    }
    // Results keep immutable intent/collectors, not executable overlay authority. Releasing all
    // settled handles lets later Time Machine operations inspect the durable evidence normally.
    for (index, call, _) in &mut results {
        if let Some((_, _, original)) = calls.iter().find(|(position, _, _)| position == index) {
            call.delegation = original.delegation.clone();
        }
    }
    results
}

/// Physical uncertainty takes precedence over an evidence worker's join/publication error.
fn settle_verification_publication<T: std::fmt::Debug>(
    effect: super::tools::ToolEffectState,
    publication: Result<T>,
) -> Result<T> {
    if effect != super::tools::ToolEffectState::Settled {
        return Err(anyhow::Error::new(super::tools::ToolEffectsUnsettled(
            effect,
        )))
        .context(format!(
            "verification unsettled; evidence publication: {publication:?}"
        ));
    }
    publication
}

fn durable_verifier_effect(effect: super::tools::ToolEffectState) -> Result<DurableEffectState> {
    use super::tools::ToolEffectState;
    Ok(match effect {
        ToolEffectState::Settled => DurableEffectState::Settled,
        ToolEffectState::PossiblyApplied(record) => DurableEffectState::PossiblyApplied {
            record: match record {
                Some(id) => Some(
                    std::num::NonZeroU64::new(id)
                        .context("invalid verifier record identity")
                        .map_err(|_| {
                            anyhow::Error::new(super::tools::ToolEffectsUnsettled(effect))
                        })?,
                ),
                None => None,
            },
        },
        ToolEffectState::UnknownWorker => DurableEffectState::UnknownWorker,
    })
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerificationFault {
    BeforeStart,
    AfterStart,
    StartPanic,
    BeforeFinish,
    FinishPanic,
}

#[cfg(test)]
#[derive(Default)]
struct VerificationTest {
    lock_home: Option<PathBuf>,
    fault: Mutex<Option<VerificationFault>>,
}

#[cfg(test)]
impl VerificationTest {
    fn fail(&self, point: VerificationFault) -> Result<()> {
        let fault = {
            let mut slot = self.fault.lock().unwrap();
            if *slot == Some(point)
                || (point == VerificationFault::BeforeStart
                    && *slot == Some(VerificationFault::StartPanic))
                || (point == VerificationFault::BeforeFinish
                    && *slot == Some(VerificationFault::FinishPanic))
            {
                slot.take()
            } else {
                None
            }
        };
        match fault {
            Some(VerificationFault::StartPanic | VerificationFault::FinishPanic) => {
                panic!("owned verification publication fault")
            }
            Some(_) => bail!("owned verification publication fault"),
            None => Ok(()),
        }
    }
}

fn check_parent(parent: &ExecutionContext) -> Result<()> {
    ensure!(parent.overlay().is_none() && parent.native_scope().is_none() && parent.can_restore_workspace(),
        "speculative coordination requires unrestricted parent authority; children cannot integrate");
    Ok(())
}

fn check_cancel(cancel: Option<&TurnCancel>) -> Result<()> {
    ensure!(
        !cancel.is_some_and(TurnCancel::is_cancelled),
        "speculative operation cancelled before live effects"
    );
    Ok(())
}

fn check_write_target(path: &Path) -> Result<()> {
    for component in path.components() {
        let name = component
            .as_os_str()
            .to_str()
            .context("speculative path must be Unicode")?;
        let lower = name.to_ascii_lowercase();
        ensure!(!matches!(lower.as_str(), ".git" | ".aizen" | "target" | "node_modules" | "dist" | "build" | "generated" | "vendor")
            && !lower.ends_with(".lock") && !lower.ends_with("-lock.json")
            && !matches!(lower.as_str(), "yarn.lock" | "pnpm-lock.yaml" | "npm-shrinkwrap.json"),
            "speculative writes to managed/generated output or lockfiles are unsupported; use direct mode");
    }
    Ok(())
}

fn non_git_store(identity: &WorkspaceIdentity) -> Result<PathBuf> {
    let root = crate::core::config::aizen_home()
        .join("native-workspaces")
        .join(&identity.worktree_id);
    ensure!(
        !root.starts_with(&identity.canonical_root) && !identity.canonical_root.starts_with(&root),
        "native speculative metadata must be outside the live workspace"
    );
    create_private_directory(&root)?;
    Ok(std::fs::canonicalize(root)?)
}

fn create_private_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !crate::core::file_placement::is_link_or_reparse(&metadata),
                "speculation metadata requires non-link directories"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("speculation metadata has no parent")?;
            create_private_directory(parent)?;
            let guard = NativeResource::resolve(&parent.join(".speculation-directory-boundary"))?;
            std::fs::create_dir(path)?;
            guard.verify()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            }
            #[cfg(windows)]
            crate::core::persist::harden_owner_only_checked(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    // NativeResource captures and verifies the complete ancestor chain, including existing roots.
    NativeResource::resolve(&path.join(".speculation-directory-boundary"))?.verify()
}

#[cfg(test)]
#[path = "speculation_verification_tests.rs"]
mod verification_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::ToolEffectState;

    #[tokio::test]
    async fn speculation_verifier_uncertainty_survives_publication_worker_panic() {
        let joined = tokio::task::spawn_blocking(|| -> Result<()> {
            panic!("owned verification publication fault")
        })
        .await
        .context("verification publication worker failed")
        .and_then(|result| result);
        let error =
            settle_verification_publication(ToolEffectState::UnknownWorker, joined).unwrap_err();
        assert_eq!(
            super::super::tool_error_effect_state(&error),
            ToolEffectState::UnknownWorker
        );
        assert!(error.to_string().contains("verification unsettled"));
    }

    #[test]
    fn speculation_verifier_possibly_applied_precedes_secondary_publication_error() {
        let error = settle_verification_publication::<()>(
            ToolEffectState::PossiblyApplied(Some(17)),
            Err(anyhow::anyhow!("owned publication failure")),
        )
        .unwrap_err();
        assert_eq!(
            super::super::tool_error_effect_state(&error),
            ToolEffectState::PossiblyApplied(Some(17))
        );
        assert!(settle_verification_publication(ToolEffectState::Settled, Ok(7)).is_ok());
        let error = settle_verification_publication::<()>(
            ToolEffectState::Settled,
            Err(anyhow::anyhow!("owned publication failure")),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "owned publication failure");
    }
}
