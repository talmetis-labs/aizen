//! Native task handoff classification, independent of source storage and integration.

use crate::core::worker_evidence::DriverResult;
use anyhow::Result;

/// Runtime-only admission to parent integration/check, never child verified success.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UnverifiedHandoff(bool);

impl UnverifiedHandoff {
    pub(crate) fn eligible(self) -> bool {
        self.0
    }
}

/// Capture the loop's typed terminal result before settlement; a later cancel flag is not
/// evidence that a completed driver was cancelled. Completed here means the private driver
/// returned an admissible candidate, not that its work passed parent verification.
pub(crate) fn driver_result(
    result: Result<&crate::agent::AgentOutcome, &anyhow::Error>,
    handoff: UnverifiedHandoff,
) -> DriverResult {
    match result {
        Ok(outcome) => match outcome.stop {
            crate::agent::StopReason::Done => DriverResult::Completed,
            crate::agent::StopReason::Incomplete if handoff.eligible() => DriverResult::Completed,
            crate::agent::StopReason::Cancelled => DriverResult::Cancelled,
            _ => DriverResult::Failed,
        },
        Err(_) => DriverResult::Failed,
    }
}

pub(crate) fn unverified_handoff(
    outcome: &crate::agent::AgentOutcome,
    execution: &crate::core::exec_ctx::ExecutionContext,
    cancel: &crate::core::cancel::TurnCancel,
) -> UnverifiedHandoff {
    classify_handoff(
        outcome,
        execution.native_scope().is_some(),
        execution.task().is_some_and(|task| task.handoff_eligible()),
        cancel.is_cancelled(),
        execution
            .deadline()
            .is_some_and(|deadline| std::time::Instant::now() >= deadline),
    )
}

fn classify_handoff(
    outcome: &crate::agent::AgentOutcome,
    native_scoped: bool,
    awaiting_parent_check_only: bool,
    cancelled: bool,
    deadline_elapsed: bool,
) -> UnverifiedHandoff {
    UnverifiedHandoff(
        matches!(outcome.stop, crate::agent::StopReason::Incomplete)
            && outcome
                .final_text
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
            && native_scoped
            && awaiting_parent_check_only
            && !cancelled
            && !deadline_elapsed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_handoff_requires_runtime_scope_and_only_parent_check() {
        let mut outcome = crate::agent::AgentOutcome {
            speech_final_verified: false,
            final_text: Some("done; incomplete-unverified-handoff; tests passed".into()),
            iters: 2,
            stop: crate::agent::StopReason::Incomplete,
            verification: None,
        };
        let eligible = classify_handoff(&outcome, true, true, false, false);
        assert!(eligible.eligible());
        assert!(matches!(
            driver_result(Ok(&outcome), eligible),
            DriverResult::Completed
        ));
        assert_eq!(outcome.stop, crate::agent::StopReason::Incomplete);
        for facts in [
            (false, true, false, false),
            (true, false, false, false), // Open checklist/failed proof is not a handoff.
            (true, true, true, false),
            (true, true, false, true),
        ] {
            let rejected = classify_handoff(&outcome, facts.0, facts.1, facts.2, facts.3);
            assert!(!rejected.eligible());
            assert!(matches!(
                driver_result(Ok(&outcome), rejected),
                DriverResult::Failed
            ));
        }
        assert!(matches!(
            driver_result(Ok(&outcome), UnverifiedHandoff::default()),
            DriverResult::Failed
        ));
        assert!(matches!(
            driver_result(Err(&anyhow::anyhow!("driver failed")), eligible),
            DriverResult::Failed
        ));
        outcome.final_text = None;
        assert!(!classify_handoff(&outcome, true, true, false, false).eligible());
        outcome.final_text = Some("complete".into());
        for stop in [
            crate::agent::StopReason::Done,
            crate::agent::StopReason::Cancelled,
            crate::agent::StopReason::Deadline,
            crate::agent::StopReason::VerificationFailed,
        ] {
            outcome.stop = stop;
            assert!(!classify_handoff(&outcome, true, true, false, false).eligible());
        }
    }

    #[test]
    fn driver_result_preserves_unverified_handoff_semantics() {
        for (stop, expected) in [
            (crate::agent::StopReason::Done, DriverResult::Completed),
            (
                crate::agent::StopReason::Incomplete,
                DriverResult::Completed,
            ),
            (crate::agent::StopReason::Cancelled, DriverResult::Cancelled),
            (crate::agent::StopReason::Deadline, DriverResult::Failed),
            (
                crate::agent::StopReason::VerificationFailed,
                DriverResult::Failed,
            ),
        ] {
            let outcome = crate::agent::AgentOutcome {
                speech_final_verified: false,
                final_text: Some("pending parent verification".into()),
                iters: 1,
                stop,
                verification: None,
            };
            let handoff = classify_handoff(&outcome, true, true, false, false);
            assert_eq!(driver_result(Ok(&outcome), handoff), expected);
            if outcome.stop == crate::agent::StopReason::Incomplete {
                assert_eq!(
                    driver_result(Ok(&outcome), UnverifiedHandoff::default()),
                    DriverResult::Failed
                );
                assert_eq!(outcome.stop, crate::agent::StopReason::Incomplete);
            }
        }
        assert_eq!(
            driver_result(
                Err(&anyhow::anyhow!("driver failed")),
                UnverifiedHandoff::default()
            ),
            DriverResult::Failed
        );
    }
}
