//! Conversation-owned execution policy, independent of the checklist and its lifecycle.
//! Only trusted ingress may commit transitions. Document text and model DTOs carry no authority.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    #[default]
    Normal,
    Planning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanTarget {
    pub root: String,
    pub path: String,
    pub id: String,
}
impl PlanTarget {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            Path::new(&self.root).is_absolute()
                && self.root.len() <= 4096
                && !self.root.chars().any(char::is_control),
            "invalid plan workspace"
        );
        crate::agent::plan_document::validate_relative_path(&self.path)?;
        ensure!(
            crate::agent::plan_document::valid_plan_id(&self.id),
            "invalid plan target identity"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionControl {
    pub version: u32,
    pub mode: ExecutionMode,
    pub generation: u64,
    pub plan_target: Option<PlanTarget>,
}
impl Default for SessionControl {
    fn default() -> Self {
        Self {
            version: VERSION,
            mode: ExecutionMode::Normal,
            generation: 0,
            plan_target: None,
        }
    }
}
impl SessionControl {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported session control version"
        );
        ensure!(
            self.generation < u64::MAX,
            "session control generation exhausted"
        );
        if let Some(target) = &self.plan_target {
            target.validate()?;
        }
        Ok(())
    }
    pub(crate) fn is_planning(&self) -> bool {
        self.mode == ExecutionMode::Planning
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ControlContext(Arc<Mutex<ControlInner>>);
#[derive(Debug, Default)]
struct ControlInner {
    state: SessionControl,
    /// Local generation also changes on restore; disk counters never resurrect a live handle.
    revision: u64,
    admitted: usize,
    /// A synchronous human workspace transition closes admission without blocking its inspection.
    workspace_transition: bool,
    blocked: bool,
}

impl ControlContext {
    /// Foreground navigation is synchronous and holds this guard until every host scope is rebound.
    /// No policy revision changes here: old children remain pinned; new admission is excluded.
    pub(crate) fn hold_workspace_transition(&self) -> Result<WorkspaceTransition> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !inner.blocked && !inner.state.is_planning(),
            "workspace focus is unavailable while execution is restricted"
        );
        ensure!(
            inner.admitted == 0 && !inner.workspace_transition,
            "workspace focus requires settled workers"
        );
        inner.workspace_transition = true;
        Ok(WorkspaceTransition(self.clone()))
    }

    pub(crate) fn snapshot(&self) -> SessionControl {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .clone()
    }
    pub(crate) fn policy(&self) -> ExecutionPolicy {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ExecutionPolicy {
            owner: self.clone(),
            revision: inner.revision,
            state: inner.state.clone(),
        }
    }
    /// Loading old/normal state cannot silently remove an existing planning restriction.
    /// The caller loads this before registry construction or any effectful startup operation.
    pub(crate) fn restore(&self, incoming: SessionControl) -> Result<()> {
        incoming.validate()?;
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            inner.admitted == 0 && !inner.workspace_transition,
            "cannot restore execution policy while workers are admitted"
        );
        let incoming = if inner.state.is_planning() {
            ensure!(
                !incoming.is_planning() || incoming.plan_target == inner.state.plan_target,
                "incoming plan target differs; explicitly select it before resuming"
            );
            inner.state.clone()
        } else {
            incoming
        };
        let revision = inner
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("execution policy generation exhausted"))?;
        inner.state = incoming;
        inner.revision = revision;
        // A failed transition must not be unblocked merely by loading a transcript.
        Ok(())
    }
    /// Trusted human transition. Persist the complete conversation generation in `save`.
    /// Holding this owner lock closes the check/admit race; save must not reenter this context.
    pub(crate) fn transition(
        &self,
        mode: ExecutionMode,
        target: Option<PlanTarget>,
        save: impl FnOnce(&SessionControl) -> Result<()>,
    ) -> Result<()> {
        if let Some(target) = &target {
            target.validate()?;
        }
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            inner.admitted == 0 && !inner.workspace_transition,
            "execution policy transition requires settled workers"
        );
        let generation = inner
            .state
            .generation
            .checked_add(1)
            .filter(|n| *n < u64::MAX)
            .ok_or_else(|| anyhow::anyhow!("session control generation exhausted"))?;
        let revision = inner
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("execution policy generation exhausted"))?;
        let next = SessionControl {
            version: VERSION,
            mode,
            generation,
            plan_target: target,
        };
        // Failed entry must not leave a normal-mode fallback usable. Failed exit stays restricted.
        inner.blocked = true;
        save(&next)?;
        inner.state = next;
        inner.revision = revision;
        inner.blocked = false;
        Ok(())
    }
}

/// Immutable view propagated through builders/children; a later human exit never widens this view.
#[derive(Debug, Clone)]
pub(crate) struct ExecutionPolicy {
    owner: ControlContext,
    revision: u64,
    state: SessionControl,
}
impl ExecutionPolicy {
    pub(crate) fn state(&self) -> &SessionControl {
        &self.state
    }
    pub(crate) fn is_planning(&self) -> bool {
        self.state.is_planning()
    }
    pub(crate) fn validate_current(&self) -> Result<()> {
        let inner = self.owner.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !inner.blocked && inner.revision == self.revision,
            "execution policy changed or is not durably saved"
        );
        Ok(())
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner.0, &other.owner.0)
    }
    pub(crate) fn context(&self) -> ControlContext {
        self.owner.clone()
    }
    pub(crate) fn check_inheritance(&self, next: &Self) -> Result<()> {
        ensure!(
            self.same_owner(next) && self.revision == next.revision && self.state == next.state,
            "execution policy cannot change inside a running context"
        );
        self.validate_current()
    }
    /// A native allowlist, not a name-based claim that arbitrary tools are safe. The registry must
    /// register only these audited built-ins; execution still checks prepared resource provenance.
    pub(crate) fn permits_tool(&self, name: &str) -> bool {
        !self.is_planning()
            || matches!(
                name,
                "file_read"
                    | "file_glob"
                    | "search_files"
                    | "todo_write"
                    | "tool_search"
                    | "file_write"
            )
    }
    pub(crate) fn admit(&self) -> Result<PolicyLease> {
        let mut inner = self.owner.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !inner.blocked && !inner.workspace_transition && inner.revision == self.revision,
            "stale execution policy or workspace transition is in progress"
        );
        inner.admitted = inner
            .admitted
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("execution policy admission limit reached"))?;
        Ok(PolicyLease(self.owner.clone()))
    }
}

/// Session-owned admission hold; dropping after a refused focus reopens the unchanged workspace.
pub(crate) struct WorkspaceTransition(ControlContext);
impl Drop for WorkspaceTransition {
    fn drop(&mut self) {
        self.0
             .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .workspace_transition = false;
    }
}

/// Must live through worker settlement, not just the observer's timeout/cancellation.
#[derive(Debug)]
pub(crate) struct PolicyLease(ControlContext);
impl Drop for PolicyLease {
    fn drop(&mut self) {
        let mut inner = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        inner.admitted = inner.admitted.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> PlanTarget {
        PlanTarget {
            root: std::env::temp_dir().to_string_lossy().into_owned(),
            path: "docs/plans/a.md".into(),
            id: "p-0123456789abcdef0123456789abcdef".into(),
        }
    }
    #[test]
    fn workspace_transition_closes_admission_and_never_lifts_planning() {
        let owner = ControlContext::default();
        let policy = owner.policy();
        let live = policy.admit().unwrap();
        assert!(owner.hold_workspace_transition().is_err());
        drop(live);
        let guard = owner.hold_workspace_transition().unwrap();
        assert!(policy.admit().is_err());
        assert!(owner.restore(SessionControl::default()).is_err());
        assert!(owner
            .transition(ExecutionMode::Planning, None, |_| Ok(()))
            .is_err());
        drop(guard);
        assert!(policy.admit().is_ok());
        owner
            .transition(ExecutionMode::Planning, None, |_| Ok(()))
            .unwrap();
        assert!(owner.hold_workspace_transition().is_err());
        assert!(owner.snapshot().is_planning());
    }

    #[test]
    fn transition_needs_persistence_and_never_widens_old_policy() {
        let owner = ControlContext::default();
        let old = owner.policy();
        owner
            .transition(ExecutionMode::Planning, Some(target()), |_| Ok(()))
            .unwrap();
        assert!(old.validate_current().is_err());
        let restricted = owner.policy();
        assert!(restricted.permits_tool("file_read"));
        for tool in [
            "shell_run",
            "process",
            "task",
            "workflow",
            "mcp_read",
            "unknown",
        ] {
            assert!(!restricted.permits_tool(tool));
        }
        owner
            .transition(ExecutionMode::Normal, None, |_| Ok(()))
            .unwrap();
        assert!(restricted.is_planning());
        assert!(restricted.validate_current().is_err());
        assert!(!owner.policy().is_planning());
    }
    #[test]
    fn failed_transition_blocks_all_stale_execution_until_explicit_retry() {
        let owner = ControlContext::default();
        let before = owner.policy();
        assert!(owner
            .transition(ExecutionMode::Planning, Some(target()), |_| anyhow::bail!(
                "disk full"
            ))
            .is_err());
        assert!(before.admit().is_err());
        owner.restore(SessionControl::default()).unwrap();
        assert!(owner.policy().admit().is_err());
        owner
            .transition(ExecutionMode::Planning, Some(target()), |_| Ok(()))
            .unwrap();
        assert!(owner.policy().admit().is_ok());
    }
    #[test]
    fn admitted_worker_blocks_transition_until_settled() {
        let owner = ControlContext::default();
        let policy = owner.policy();
        let lease = policy.admit().unwrap();
        assert!(owner
            .transition(ExecutionMode::Planning, Some(target()), |_| panic!(
                "must not save"
            ))
            .is_err());
        drop(lease);
        owner
            .transition(ExecutionMode::Planning, Some(target()), |_| Ok(()))
            .unwrap();
    }
    #[test]
    fn legacy_or_normal_restore_cannot_lift_active_planning() {
        let owner = ControlContext::default();
        owner
            .transition(ExecutionMode::Planning, Some(target()), |_| Ok(()))
            .unwrap();
        owner.restore(SessionControl::default()).unwrap();
        assert!(owner.snapshot().is_planning());
        assert_eq!(owner.snapshot().plan_target, Some(target()));
    }
    #[test]
    fn record_validation_rejects_unknown_fields_versions_and_bad_targets() {
        let good = SessionControl::default();
        let value = serde_json::to_value(&good).unwrap();
        let mut unknown = value.clone();
        unknown["waiver"] = true.into();
        assert!(serde_json::from_value::<SessionControl>(unknown).is_err());
        let mut future = good.clone();
        future.version = 9;
        assert!(future.validate().is_err());
        let mut invalid = target();
        invalid.path = "docs/plans/../code.rs".into();
        assert!(invalid.validate().is_err());
        assert_eq!(
            serde_json::from_value::<SessionControl>(value).unwrap(),
            good
        );
    }
}
