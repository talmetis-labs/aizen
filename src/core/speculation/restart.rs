//! Restart inspection routes to exactly one existing evidence namespace, never a checkpoint store.
//! The selected records are data; fresh parent policy, workspace and resource admission still apply.

use super::inspection::{self, SpeculationRecoveryReport};
use crate::core::{
    exec_ctx,
    workspace_txn::{PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease},
};
use anyhow::{bail, ensure, Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// Inspect caller-selected exact resources. No source bytes or recovery evidence are changed.
pub(crate) fn inspect_speculation_recovery(
    root: &Path,
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
) -> Result<SpeculationRecoveryReport> {
    inspect_or_record(root, cohort, prepared, resource_paths, false)
}

/// Append only a nonterminal historical observation after fresh admission. It cannot clear a pending
/// transaction or reconstruct execution/completion authority from stored paths or policy metadata.
pub(crate) fn record_speculation_recovery_observation(
    root: &Path,
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
) -> Result<SpeculationRecoveryReport> {
    inspect_or_record(root, cohort, prepared, resource_paths, true)
}

fn inspect_or_record(
    root: &Path,
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
    record_observation: bool,
) -> Result<SpeculationRecoveryReport> {
    const TIMEOUT: Duration = Duration::from_secs(15);
    let parent = exec_ctx::current().context("restart report requires the current plain parent")?;
    inspection::require_recovery_report_parent(&parent)?;
    inspection::validate_recovery_report_request(cohort, prepared, resource_paths)?;
    let identity = WorkspaceIdentity::discover(root)?;
    let approved =
        inspection::resolve_recovery_report_resources(&identity.canonical_root, resource_paths)?;
    let native_plan = super::namespace::PlannedNamespace::plan(
        identity.clone(),
        &crate::core::config::aizen_home(),
    )?;
    let native_existing = native_plan.open_existing()?;
    let legacy_plan = super::legacy::plan_existing(&identity.canonical_root)?;
    if native_existing.is_none() && legacy_plan.is_none() {
        bail!("restart report found no admitted speculative namespace for this workspace");
    }
    let cancel = crate::core::cancel::current();
    let start = Instant::now();
    let scope = parent.resource_scope();
    let writer = WorkspaceWriterLease::acquire_identity_scoped(
        identity,
        TIMEOUT,
        cancel.as_ref(),
        "restart inspection",
        &scope,
    )?;
    let physical = PhysicalEffectLease::exclusive_without_background(
        TIMEOUT.saturating_sub(start.elapsed()),
        cancel.as_ref(),
        "restart inspection",
        &scope,
    )?;
    let body_ctx = parent
        .with_writer_lease(writer.clone())
        .with_physical_effect_lease(physical.clone());
    exec_ctx::with_current(body_ctx, || {
        let legacy = legacy_plan
            .as_ref()
            .map(|plan| plan.open_existing())
            .transpose()?
            .flatten();
        let native = native_plan.open_existing()?;
        let native_candidate = native
            .as_ref()
            .map(|namespace| {
                inspection::contains_recovery_cohort(namespace, &writer, &physical, cohort)
            })
            .transpose()?
            .unwrap_or(false);
        let legacy_candidate = legacy
            .as_ref()
            .map(|namespace| {
                inspection::contains_recovery_cohort(namespace, &writer, &physical, cohort)
            })
            .transpose()?
            .unwrap_or(false);
        ensure!(native_candidate ^ legacy_candidate,
            "restart report requires exactly one matching speculative namespace; missing or ambiguous evidence was retained");
        if native_candidate {
            let namespace = native.context("native namespace disappeared during inspection")?;
            let selection = inspection::select_speculation_recovery(
                &namespace, &parent, &writer, &physical, cohort, prepared,
            )?;
            inspection::inspect_selected_recovery(
                &namespace,
                &parent,
                &writer,
                &physical,
                &selection,
                &approved,
                record_observation,
            )
        } else {
            let binding = legacy.context("legacy namespace disappeared during inspection")?;
            let selection = binding
                .select_speculation_recovery(&parent, &writer, &physical, cohort, prepared)?;
            binding.inspect_selected_recovery(
                &parent,
                &writer,
                &physical,
                &selection,
                &approved,
                record_observation,
            )
        }
    })
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod tests;
