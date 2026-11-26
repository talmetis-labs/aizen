//! Run-local inverse-operation authority and budget. No source capture, store, or restore handles.
//! Durable evidence lives in the native journal and Git operation receipts, not in this coordinator.

use anyhow::{ensure, Result};
use std::sync::{Arc, Mutex};

const MAX_INVERSES: u8 = 2;
const CHILD_DENIED: &str = "delegated children cannot undo workspace state; retain partial changes and report them to the coordinating parent";

#[derive(Debug, Default)]
struct State {
    attempts: u8,
    in_flight: bool,
    background_coverage_gap: bool,
}

/// Clones share one logical-run budget; denied views never revoke the parent's authority.
#[derive(Debug, Clone)]
pub(crate) struct RunRecovery(Arc<Mutex<State>>, bool);

impl Default for RunRecovery {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State::default())), true)
    }
}

impl RunRecovery {
    pub(crate) fn without_workspace_restore(&self) -> Self {
        Self(Arc::clone(&self.0), false)
    }

    pub(crate) fn can_restore_workspace(&self) -> bool {
        self.1
    }

    /// Sticky warning, not blanket invalidation of exact native postimage checks.
    pub(crate) fn defer_background_coverage(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .background_coverage_gap = true;
    }

    pub(crate) fn hint(&self, _root: &std::path::Path) -> Option<String> {
        if !self.1 {
            return None;
        }
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.attempts >= MAX_INVERSES {
            return Some("Automatic inverse-operation budget exhausted. Preserve the current state and report the failure; do not reset or restore a whole tree.".into());
        }
        let background = if state.background_coverage_gap {
            " Background effects invalidate broad recovery coverage."
        } else {
            ""
        };
        Some(format!("No source checkpoint or universal rollback exists.{background} For exact journaled native edits, the parent may request native_owner_undo on selected owners/files; newer postimages block undo. Committed work requires an explicitly approved Git inverse. Unattributed shell/user edits must be inspected and retained, not silently restored."))
    }

    /// Reserve before the first inverse write. Errors/partial applications still consume an attempt.
    /// The caller must independently hold the real workspace/resource and physical-effect leases.
    pub(crate) fn begin_inverse(&self) -> Result<InverseReservation> {
        ensure!(self.1, "{CHILD_DENIED}");
        ensure!(
            crate::core::exec_ctx::current().is_none_or(|ctx| ctx.can_restore_workspace()),
            "{CHILD_DENIED}"
        );
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?;
        ensure!(!state.in_flight, "another inverse operation is in flight");
        ensure!(
            state.attempts < MAX_INVERSES,
            "run inverse-operation budget exhausted"
        );
        state.attempts += 1;
        state.in_flight = true;
        Ok(InverseReservation(self.clone()))
    }
}

pub(crate) struct InverseReservation(RunRecovery);

impl Drop for InverseReservation {
    fn drop(&mut self) {
        self.0
             .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_budget_is_shared_and_failures_do_not_refund_it() {
        let run = RunRecovery::default();
        let clone = run.clone();
        let first = run.begin_inverse().unwrap();
        assert!(clone.begin_inverse().is_err());
        drop(first);
        drop(clone.begin_inverse().unwrap());
        assert!(run.begin_inverse().is_err());
        assert!(!run.0.lock().unwrap().in_flight);
    }

    #[test]
    fn denied_view_cannot_borrow_parent_or_ambient_authority() {
        let run = RunRecovery::default();
        assert!(run.without_workspace_restore().begin_inverse().is_err());
        assert!(run.can_restore_workspace());
        let child = crate::core::exec_ctx::ExecutionContext::default().without_workspace_restore();
        crate::core::exec_ctx::with_current(child, || {
            assert!(run.begin_inverse().is_err());
        });
        assert_eq!(run.0.lock().unwrap().attempts, 0);
    }

    #[test]
    fn background_gap_is_sticky_and_does_not_invent_restore_authority() {
        let run = RunRecovery::default();
        run.defer_background_coverage();
        assert!(run
            .hint(std::path::Path::new("."))
            .unwrap()
            .contains("Background effects"));
        assert!(run
            .without_workspace_restore()
            .hint(std::path::Path::new("."))
            .is_none());
    }
}
