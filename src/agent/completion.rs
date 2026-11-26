//! Runtime-only completion reconciliation. Model prose never creates check receipts.
use super::{todo, verify_gate, AgentOutcome, StopReason};
use crate::core::{cancel::TurnCancel, exec_ctx::ExecutionContext};
use std::path::Path;

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;

impl StopReason {
    pub fn task_turn_end(&self) -> todo::TurnEnd {
        match self {
            Self::Done => todo::TurnEnd::Completed,
            Self::Incomplete => todo::TurnEnd::Incomplete,
            Self::Divergence | Self::Refresh => todo::TurnEnd::Interrupted,
            Self::MaxIters => todo::TurnEnd::MaxIterations,
            Self::VerificationFailed => todo::TurnEnd::Failed,
            Self::AwaitingInput(_) => todo::TurnEnd::AwaitingInput,
            Self::Cancelled => todo::TurnEnd::Cancelled,
            Self::Deadline => todo::TurnEnd::Deadline,
        }
    }
}

pub(super) fn identity_string(identity: [u8; 32]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(64);
    for byte in identity {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Revalidate once at a completion boundary, never on each token/model iteration. Restored
/// evidence is already historical; these checks protect live receipts from external edits.
pub(super) fn revalidate(task: &todo::TaskContext) -> anyhow::Result<()> {
    task.refresh_plan()?;
    revalidate_after_plan_refresh(task)
}

/// The loop's bounded plan-context refresh already checked the contract at this boundary.
/// Reuse that observation rather than reopening the document for each terminal gate.
pub(super) fn revalidate_after_plan_refresh(task: &todo::TaskContext) -> anyhow::Result<()> {
    task.revalidate_sources()?;
    let sources = task.fresh_sources();
    if !sources.is_empty()
        && (super::managed_background_active()
            || super::check_background_effects().is_err()
            || super::process::check_background_effects().is_err())
    {
        task.invalidate_effects();
        return Ok(());
    }
    for (root, expected) in sources {
        let current = verify_gate::source_fingerprint(Path::new(&root)).map(identity_string);
        if current.as_deref() != Some(expected.as_str()) {
            task.invalidate_effects_at(Some(Path::new(&root)));
            break;
        }
    }
    Ok(())
}

pub(super) fn record_gate(
    task: &todo::TaskContext,
    tokens: Vec<todo::EvidenceToken>,
    root: &Path,
    result: &verify_gate::VerifyGateResult,
) -> anyhow::Result<()> {
    if result.termination == verify_gate::VerifyTermination::Passed
        && crate::core::cancel::current().is_some_and(|cancel| cancel.is_cancelled())
    {
        return Ok(());
    }
    use todo::CheckResult;
    use verify_gate::VerifyTermination;
    let kind = match result.termination {
        VerifyTermination::Passed if result.accepted() && result.source_identity.is_some() => {
            CheckResult::Pass
        }
        VerifyTermination::Passed | VerifyTermination::Unstable => CheckResult::Unstable,
        VerifyTermination::Failed => CheckResult::Fail,
        VerifyTermination::Unavailable => CheckResult::Unavailable,
        VerifyTermination::Cancelled => CheckResult::Cancelled,
        VerifyTermination::TimedOut => CheckResult::TimedOut,
        VerifyTermination::Deferred => CheckResult::Deferred,
        VerifyTermination::Unsettled => CheckResult::Unsettled,
    };
    let root = root.to_string_lossy();
    let identity = result
        .source_identity
        .map(identity_string)
        .unwrap_or_else(|| "unavailable".into());
    let commands = if kind == CheckResult::Pass {
        result.passed_commands.clone()
    } else {
        vec![result.command.clone()]
    };
    for token in tokens {
        for command in &commands {
            if token.matches_check(command, &root) {
                task.record_check(
                    token.clone(),
                    command.clone(),
                    root.to_string(),
                    identity.clone(),
                    kind,
                )?;
                break;
            }
        }
    }
    Ok(())
}

/// Run only bounded runtime-known check scopes. Independent roots are never discharged by the
/// last edited tree, and an unsupported explicit command is not executed as arbitrary shell text.
pub(super) async fn run_checks(
    ctx: &ExecutionContext,
    fallback: &Path,
    timeout_secs: u64,
    edited: &[std::path::PathBuf],
    quiet: bool,
) -> anyhow::Result<Option<verify_gate::VerifyGateResult>> {
    let ctx = super::hooks::automation_context(Some(ctx));
    let Ok(ctx) = super::hooks::admit_automation(&ctx) else {
        // Suppression is not successful verification and must not resolve existing obligations.
        return Ok(None);
    };
    let ctx = &ctx;
    let tokens = ctx
        .task()
        .map(|task| task.evidence_captures())
        .unwrap_or_default();
    let mut roots = ctx
        .task()
        .map(|task| task.verification_roots())
        .unwrap_or_default();
    if roots.is_empty() {
        roots.push(fallback.to_string_lossy().into_owned());
    }
    // Leaving additional roots outstanding is deliberate: this is not an unbounded job runner.
    let mut result = None;
    for root in roots.into_iter().take(4) {
        let requested: Vec<String> = tokens
            .iter()
            .filter_map(|token| match token.requirement() {
                todo::Requirement::Check {
                    root: expected,
                    command,
                    ..
                } if expected == &root => Some(command.clone()),
                _ => None,
            })
            .collect();
        let gate = verify_gate::run_completion_ladder_in_context(
            Path::new(&root),
            timeout_secs,
            edited,
            quiet,
            ctx,
            &requested,
        )
        .await;
        if let Some(gate) = gate.as_ref() {
            if gate.effect_state != super::tools::ToolEffectState::Settled {
                ctx.note_task_effects();
                return Err(anyhow::Error::new(super::tools::ToolEffectsUnsettled(
                    gate.effect_state,
                ))
                .context(gate.output.clone()));
            }
            if let Some(task) = ctx.task() {
                record_gate(task, tokens.clone(), Path::new(&root), gate)?;
            }
        }
        let accepted = gate.as_ref().is_some_and(|gate| gate.accepted());
        result = gate;
        if !accepted {
            break;
        }
    }
    Ok(result)
}

impl AgentOutcome {
    /// Called again after stop hooks and before persistence/learning/result emission. A late
    /// mutation may downgrade a completed answer, but never upgrades any non-success stop.
    pub fn reconcile_task_completion(&mut self, ctx: &ExecutionContext, cancel: &TurnCancel) {
        let ctx = super::hooks::automation_context(Some(ctx));
        if self.stop == StopReason::Done {
            if cancel.is_cancelled() {
                self.stop = StopReason::Cancelled;
            } else if ctx
                .deadline()
                .is_some_and(|d| std::time::Instant::now() >= d)
            {
                self.stop = StopReason::Deadline;
            } else if ctx
                .policy()
                .is_some_and(|policy| policy.validate_current().is_err())
            {
                self.stop = StopReason::Incomplete;
            } else if let Some(task) = ctx.task() {
                if revalidate(task).is_err() || task.completion_pending() || !task.is_complete() {
                    self.stop = StopReason::Incomplete;
                }
            }
        }
        let basis = ctx
            .task()
            .map(|task| {
                let state = task.snapshot();
                let items: Vec<_> = state
                    .items
                    .iter()
                    .map(|row| {
                        serde_json::json!({
                            "id": row.id,
                            "basis": match task.completion_basis(&row.id) {
                                todo::CompletionBasis::Observed => "observed",
                                todo::CompletionBasis::Reported => "reported",
                                todo::CompletionBasis::Verified => "verified",
                                todo::CompletionBasis::NotRequired => "not_required",
                                todo::CompletionBasis::Unverified => "unverified",
                            },
                        })
                    })
                    .collect();
                items
            })
            .unwrap_or_default();
        let payload = self
            .verification
            .get_or_insert_with(|| serde_json::json!({}));
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "completion".into(),
                serde_json::json!({
                    "status": self.stop.label(),
                    "completed": self.stop == StopReason::Done,
                    "evidence_items": basis,
                }),
            );
        }
    }
}
