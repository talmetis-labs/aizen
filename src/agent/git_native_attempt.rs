//! Physical ownership for an explicitly allocated Git-native task.
//!
//! Durable metadata never constructs this owner. A live allocation retains its registration and
//! writer leases; this owner additionally retains the slot, activity and every actual tool join.

use crate::agent::task_tool::SubagentSlot;
use crate::agent::tools::workers::ToolWorkerOwner;
use crate::core::exec_ctx::{AttemptCarrierRegistry, ExecutionContext, WorkspaceBinding};
use crate::core::git_ops::GitNativeAttempt;
use crate::core::worker_evidence::{DriverResult, WorkerJoinFact};
use crate::core::workspace_txn::{BackgroundProcessBarrier, WorkspaceActivity};
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

struct State {
    background: BackgroundProcessBarrier,
    slot: SubagentSlot,
    activity: Arc<WorkspaceActivity>,
    attempt: GitNativeAttempt,
    binding: WorkspaceBinding,
    workers: ToolWorkerOwner,
    carriers: Arc<AttemptCarrierRegistry>,
}

// No automatic reaper: a timeout, cancellation or panic is not permission to release ownership.
static QUARANTINE: std::sync::LazyLock<Mutex<Vec<State>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

#[must_use]
pub(crate) struct OwnedNativeAttemptOwner {
    state: Option<State>,
}

impl OwnedNativeAttemptOwner {
    /// The coordinating parent must already have durably admitted this exact allocation.
    pub(crate) fn admit(
        slot: SubagentSlot,
        attempt: GitNativeAttempt,
        child: &ExecutionContext,
    ) -> Result<Self> {
        attempt.check_owned_context(child)?;
        anyhow::ensure!(
            child.tool_workers().is_none(),
            "Git-native owner cannot replace an existing worker producer"
        );
        let binding = child
            .workspace_binding()
            .context("Git-native child has no immutable workspace binding")?
            .clone();
        let background =
            BackgroundProcessBarrier::acquire(crate::core::cancel::current().as_ref())?;
        let activity = WorkspaceActivity::begin(attempt.root(), attempt.owner_scope())?;
        attempt.claim_owner()?;
        let workers = ToolWorkerOwner::new_with_git_lifecycle(attempt.clone(), binding.clone());
        Ok(Self {
            state: Some(State {
                background,
                slot,
                activity,
                attempt,
                binding,
                workers,
                carriers: AttemptCarrierRegistry::new(),
            }),
        })
    }

    pub(crate) fn bind_context(&self, child: &ExecutionContext) -> Result<ExecutionContext> {
        let state = self
            .state
            .as_ref()
            .context("Git-native owner already released")?;
        state.attempt.check_owned_context(child)?;
        child.require_current_binding(Some(&state.binding))?;
        anyhow::ensure!(
            child.tool_workers().is_none(),
            "Git-native owner cannot replace an existing worker producer"
        );
        Ok(child
            .with_attempt_carrier_registry(&state.carriers)
            .with_tool_workers(state.workers.producer()))
    }

    /// Every failure leaves state in self so Drop quarantines all release-relevant resources.
    pub(crate) async fn finish_with_driver(
        mut self,
        deadline: Instant,
        driver: DriverResult,
    ) -> Result<ReleasedGitAttempt> {
        let state = self
            .state
            .as_mut()
            .context("Git-native owner already released")?;
        let joined = state.workers.close_and_drain(deadline).await?;
        state.carriers.wait_empty(deadline).await.map_err(|error| {
            error.context(crate::agent::tools::ToolEffectsUnsettled(
                crate::agent::tools::ToolEffectState::UnknownWorker,
            ))
        })?;
        state.attempt.close_admission().map_err(|error| {
            error.context(crate::agent::tools::ToolEffectsUnsettled(
                crate::agent::tools::ToolEffectState::UnknownWorker,
            ))
        })?;
        let State {
            background,
            slot,
            activity,
            attempt,
            binding: _,
            workers,
            carriers,
        } = self
            .state
            .take()
            .expect("state retained until physical closure");
        // No released token exists until actual bodies and all executable contexts are gone.
        drop(workers);
        drop(carriers);
        drop(activity);
        drop(slot);
        drop(background);
        Ok(ReleasedGitAttempt {
            attempt,
            facts: joined.facts,
            driver,
        })
    }
}

impl Drop for OwnedNativeAttemptOwner {
    fn drop(&mut self) {
        if let Some(mut state) = self.state.take() {
            state.workers.close_admission();
            // Also seal the raw backend capability. Failure cannot skip quarantine;
            // a poisoned or closed backend already refuses new launch tickets.
            let _ = state.attempt.close_admission();
            QUARANTINE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(state);
        }
    }
}

/// Volatile, single-use proof of actual body/context closure. Not commit or verification success.
#[derive(Debug)]
#[must_use]
pub(crate) struct ReleasedGitAttempt {
    attempt: GitNativeAttempt,
    facts: Vec<WorkerJoinFact>,
    driver: DriverResult,
}

impl ReleasedGitAttempt {
    pub(crate) fn attempt(&self) -> &GitNativeAttempt {
        &self.attempt
    }

    pub(crate) fn into_joined_for(
        self,
        expected: &GitNativeAttempt,
    ) -> Result<(Vec<WorkerJoinFact>, DriverResult)> {
        anyhow::ensure!(
            self.attempt.same_attempt(expected)
                && self.attempt.scope().same_authority(expected.scope()),
            "released Git-native attempt belongs to another live owner"
        );
        Ok((self.facts, self.driver))
    }
}

#[cfg(test)]
#[path = "git_native_attempt_tests.rs"]
mod tests;
