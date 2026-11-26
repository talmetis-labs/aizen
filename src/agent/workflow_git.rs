//! Native-only workflow tasks in explicit Git-owned worktrees under retained parent ownership.
//! Handoffs park for explicit parent integration/checks; no branch mutation is authorized by prose.
use super::*;
use crate::core::{exec_ctx, git_ops};
use std::sync::Arc;

pub(super) fn validate(task: &WorkflowTask, parent: &exec_ctx::ExecutionContext) -> Result<()> {
    if task.isolation != WorkflowIsolation::Worktree {
        anyhow::ensure!(
            task.owned.is_none(),
            "shared workflow cannot carry owned Git authority"
        );
        return Ok(());
    }
    let mut args = serde_json::json!({ "role": task.role, "execution": "direct" });
    if let Some(paths) = &task.write_paths {
        args["write_paths"] = serde_json::json!(paths);
    }
    if let Some(paths) = &task.read_paths {
        args["read_paths"] = serde_json::json!(paths);
    }
    anyhow::ensure!(
        task.execution == ExecutionMode::Direct
            && task.agent.is_none()
            && task.reference_ids.is_empty()
            && task.return_evidence.is_empty()
            && task.retry_on_fail.is_none(),
        "owned workflow requires direct native role, no media, specialist or automatic fix loop"
    );
    crate::agent::task_tool::validate_owned_workflow(&args, parent)
}

pub(super) fn freeze_base(root: &Path, task: &mut WorkflowTask) -> Result<()> {
    if task.isolation == WorkflowIsolation::Worktree && task.git_base.is_none() {
        let (tip, _) = git_ops::workflow_destination(root)?;
        // Dependent dispatch requires a verification record for the actual current source tree.
        if !task.after.is_empty() {
            git_ops::require_verified_destination(root, &tip)?;
        }
        git_ops::require_workflow_ancestors(root, &tip, &task.git_prerequisites)?;
        task.git_base = Some(tip);
    }
    Ok(())
}

pub(super) fn rebind_dispatch(
    task: &WorkflowTask,
    owned: &Arc<git_ops::OwnedTask>,
    date: &str,
    parent: &exec_ctx::ExecutionContext,
) -> Result<(WorkflowTask, crate::core::native_scope::NativeWriteScope)> {
    let frozen = task
        .prepared
        .as_ref()
        .context("owned workflow dispatch not prepared")?;
    let scope = crate::core::native_scope::NativeWriteScope::resolve_access(
        owned.root(),
        task.read_paths.as_deref().unwrap_or(&[]),
        task.write_paths
            .as_deref()
            .context("owned workflow needs write_paths")?,
    )?;
    let context = parent.for_owned_git_task(owned.clone(), scope.clone())?;
    let mut dispatch = frozen.as_ref().clone();
    exec_ctx::with_current(context.clone(), || -> Result<()> {
        dispatch.registry = role_registry(&task.role, owned.root());
        dispatch.system = build_subagent_prompt(
            &task.role,
            &dispatch.registry.advertised_names(),
            owned.root(),
            &dispatch.ep.model,
            date,
            Some(&dispatch.contract),
            Some(&task.prompt),
        );
        crate::agent::task_tool::seat_native_registry(
            &mut dispatch.registry,
            &mut dispatch.system,
            &scope,
        )?;
        if let Some(schema) = &dispatch.expects {
            crate::agent::task_tool::append_output_contract(&mut dispatch.system, schema);
        }
        dispatch.native_scope = Some(scope.clone());
        dispatch.system.push_str("\n<git_owned_task>Only this fresh checkout is editable. Parent changes are excluded. Return a native handoff; do not claim builds, integration or verification. The controller commits exact attributed paths after physical settlement.</git_owned_task>\n");
        Ok(())
    })?;
    let mut task = task.clone();
    task.prepared = Some(Arc::new(dispatch));
    task.owned = Some(owned.clone());
    Ok((task, scope))
}

/// Allocation/commit are controller mutations in explicitly owned workspaces only. Any failure
/// parks the branch; there is deliberately no retry, adoption, reset or cleanup here.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    root: &Path,
    date: &str,
    task: &WorkflowTask,
    parent_track: Option<u64>,
    allocator: &git_ops::OwnedTaskAllocator,
    context_window: usize,
    parent: &exec_ctx::ExecutionContext,
    slot: crate::agent::task_tool::SubagentSlot,
) -> TaskOutcome {
    let cancel = allocator.cancel();
    let failed = |error: anyhow::Error| {
        allocator.stop();
        TaskOutcome {
        id: task.id.clone(), attempt: task.attempt.clone(), selected_for: None,
        role: task.role.clone(), model: model.into(), status: workflow_error_status(&error).into(),
        summary: format!("Owned Git workflow stopped; inspect retained worktree/registration; no automatic retry: {error:#}"),
        iters: 0, tokens_in: 0, tokens_out: 0, evidence_images: Vec::new(), git_handoff: None,
    }
    };
    let setup = crate::core::cancel::with_current(cancel.clone(), || {
        let context = allocator.context(parent)?;
        exec_ctx::with_current(context, || -> Result<_> {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "owned task cohort cancelled before allocation"
            );
            validate(task, parent)?;
            let base = task
                .git_base
                .as_ref()
                .context("owned workflow lost frozen dispatch base")?;
            let (current, _) = git_ops::workflow_destination(root)?;
            anyhow::ensure!(
                &current == base,
                "workflow destination moved after dispatch intent"
            );
            if !task.after.is_empty() {
                git_ops::require_verified_destination(root, base)?;
            }
            git_ops::require_workflow_ancestors(root, base, &task.git_prerequisites)?;
            let owned = Arc::new(allocator.execute(git_ops::PreparedOwnedTask::prepare_from(
                root,
                &parent.resource_scope(),
                base,
            )?)?);
            let (task, scope) = rebind_dispatch(task, &owned, date, parent)?;
            // Git inspection/commit belong to the coordinating parent, never the native-only child.
            // Retain parent policy/physical owners and use the owned namespace for admission; the
            // child context is seated separately below and cannot acquire opaque execution authority.
            let controller = parent.with_resource_scope(owned.scope());
            let baseline =
                exec_ctx::with_current(controller.clone(), || owned.capture_baseline(&scope))?;
            let attempt =
                exec_ctx::with_current(controller.clone(), || owned.begin_native_attempt(&scope))?;
            Ok((owned, task, controller, scope, baseline, attempt))
        })
    });
    let (owned, task, controller, scope, baseline, attempt) = match setup {
        Ok(v) => v,
        Err(error) => return failed(error),
    };
    let native = OwnedWorkflowAttempt {
        attempt,
        released: std::sync::Mutex::new(None),
    };
    let mut outcome = run_one_task_admitted_owned(
        http,
        base_url,
        api_key,
        model,
        approval_mode,
        owned.root(),
        date,
        &task,
        parent_track,
        cancel.clone(),
        context_window,
        parent,
        slot,
        Some(&native),
    )
    .await;
    if !integration_candidate_status(&outcome.status)
        || verdict_failed(&outcome)
        || cancel.is_cancelled()
    {
        allocator.stop();
        outcome.status = if matches!(
            outcome.status.as_str(),
            "unknown-worker" | "possibly-applied"
        ) {
            outcome.status
        } else {
            "git-task-incomplete".into()
        };
        outcome.summary.push_str(&format!(
            "\n[No commit/handoff. Owned worktree retained at {:?}; no dependency release.]",
            owned.root()
        ));
        return outcome;
    }
    let finished = crate::core::cancel::with_current(cancel.clone(), || {
        let released = native
            .released
            .lock()
            .map_err(|_| anyhow::anyhow!("Git-native workflow release state poisoned"))?
            .take()
            .context("Git-native workflow returned no physical release proof; no commit")?;
        exec_ctx::with_current(controller, || {
            owned.finish_native(&scope, &baseline, released)
        })
    });
    match finished {
        Ok(handoff) => {
            outcome.status = "git-handoff-pending".into();
            outcome.summary.push_str(&format!(
                "\n[UNVERIFIED, NOT INTEGRATED Git handoff]\n{}",
                serde_json::to_string_pretty(&handoff).unwrap_or_default()
            ));
            outcome.git_handoff = Some(handoff);
        }
        Err(error) => {
            allocator.stop();
            outcome.status = if matches!(
                workflow_error_status(&error),
                "unknown-worker" | "possibly-applied"
            ) {
                workflow_error_status(&error).into()
            } else {
                "git-task-incomplete".into()
            };
            outcome.summary.push_str(&format!(
                "\n[Commit/handoff refused; no retry or release: {error:#}]"
            ));
        }
    }
    outcome
}

pub(super) fn validate_handoff(handoff: &git_ops::TaskHandoff) -> Result<()> {
    git_ops::validate_task_handoff(handoff)
}

pub(super) fn accept(
    root: &Path,
    handoff: &git_ops::TaskHandoff,
) -> Result<git_ops::VerifiedTaskIntegration> {
    git_ops::accepted_task_integration(root, handoff)?.with_context(|| format!(
        "workflow parked: approve `aizen time integrate-task {} --yes`, then `aizen time verify-task <integration-operation-id> --yes` at the destination; resume the SAME workflow afterwards. Handoff alone never releases dependencies",
        handoff.registration.registration_id,
    ))
}

#[cfg(test)]
#[path = "workflow_git_dispatch_tests.rs"]
mod dispatch_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_native_admission_stays_closed_to_other_dispatch_modes() {
        let owned = WorkflowDispatchMode::OwnedNative;
        assert!(owned.compatible(&WorkflowDispatchMode::OwnedNative));
        assert!(!owned.compatible(&WorkflowDispatchMode::Opaque));
        assert!(!owned.compatible(&WorkflowDispatchMode::ReadOnly));
        let task = WorkflowTask {
            isolation: WorkflowIsolation::Worktree,
            ..Default::default()
        };
        assert!(matches!(
            workflow_dispatch_mode(&task),
            WorkflowDispatchMode::Opaque
        ));
    }

    #[test]
    fn owned_validation_keeps_the_closed_native_contract() -> Result<()> {
        let parent = exec_ctx::ExecutionContext::default();
        let mut task = WorkflowTask {
            id: "native".into(),
            role: "daedalus".into(),
            isolation: WorkflowIsolation::Worktree,
            write_paths: Some(vec!["src/lib.rs".into()]),
            ..Default::default()
        };
        validate(&task, &parent)?;
        for path in ["../escape", "C:/escape", "src/./lib.rs", ".git/config"] {
            task.write_paths = Some(vec![path.into()]);
            assert!(validate(&task, &parent).is_err());
        }
        task.write_paths = Some(vec!["src/lib.rs".into()]);
        task.execution = ExecutionMode::Speculative;
        assert!(validate(&task, &parent).is_err());
        task.execution = ExecutionMode::Direct;
        task.retry_on_fail = Some("native".into());
        assert!(validate(&task, &parent).is_err());
        Ok(())
    }
}
