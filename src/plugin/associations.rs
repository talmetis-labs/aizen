//! Optional cross-kind associations. Every queued event retains its original live turn owner;
//! a conversation name alone never authorizes feedback from a later turn or a delegated child.

use super::{memory_api as api, snapshots};
use crate::core::{cancel::TurnCancel, exec_ctx::ExecutionContext};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

pub(super) struct TurnOwner {
    pub execution: ExecutionContext,
    pub cancel: TurnCancel,
    pub feedback_cancel: TurnCancel,
}
impl TurnOwner {
    fn live(&self) -> bool {
        !self.cancel.is_cancelled()
            && !self.feedback_cancel.is_cancelled()
            && self.execution.can_restore_workspace()
            && self
                .execution
                .deadline()
                .is_none_or(|deadline| deadline > std::time::Instant::now())
    }
}

struct Pending {
    context: api::ProviderContext,
    ids: Vec<String>,
    owner: Arc<TurnOwner>,
}
struct Binding {
    snapshot: snapshots::Snapshot,
    execution: ExecutionContext,
}
tokio::task_local! { static BINDING: Binding; }
thread_local! {
    static AFFINITY: RefCell<Vec<snapshots::Snapshot>> = const { RefCell::new(Vec::new()) };
}
static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

/// Synchronous prompt construction uses the snapshot actually being rendered, not a last-session
/// lookup. The stack is scoped, restores on unwind and never opens a provider or persistent store.
pub(crate) fn with_affinity<T>(snapshot: &snapshots::Snapshot, f: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            AFFINITY.with(|slot| {
                slot.borrow_mut().pop();
            });
        }
    }
    AFFINITY.with(|slot| slot.borrow_mut().push(snapshot.clone()));
    let _pop = Pop;
    f()
}

/// Background identity learning carries its original snapshot even if another user turn is seated.
pub(crate) async fn scoped<F: std::future::Future>(
    snapshot: snapshots::Snapshot,
    execution: ExecutionContext,
    future: F,
) -> F::Output {
    BINDING
        .scope(
            Binding {
                snapshot,
                execution,
            },
            future,
        )
        .await
}

fn binding() -> Option<(snapshots::Snapshot, ExecutionContext)> {
    binding_with(snapshots::for_session)
}

fn binding_with(
    snapshot_for_session: impl FnOnce(&str) -> Option<snapshots::Snapshot>,
) -> Option<(snapshots::Snapshot, ExecutionContext)> {
    if let Some(execution) = crate::core::exec_ctx::current() {
        // A denied TLS carrier is authoritative: no parent task-local fallback for children.
        if !execution.can_restore_workspace() {
            return None;
        }
        let cancel = crate::core::cancel::current()?;
        let snapshot = snapshot_for_session(&execution.conversation().to_string())?;
        let owner = snapshot.turn_owner.as_ref()?;
        if !owner.cancel.same_turn(&cancel) || !owner.live() {
            return None;
        }
        return Some((snapshot, execution));
    }
    BINDING
        .try_with(|bound| {
            (bound.execution.can_restore_workspace()
                && bound
                    .snapshot
                    .turn_owner
                    .as_ref()
                    .is_some_and(|owner| owner.live()))
            .then(|| (bound.snapshot.clone(), bound.execution.clone()))
        })
        .ok()
        .flatten()
}

pub(crate) fn note(node: String) {
    let Some(item) = prepare_note(node, binding()) else {
        return;
    };
    let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
    pending.retain(|queued| queued.owner.live());
    if pending.len() < 64
        && !pending
            .iter()
            .any(|queued| Arc::ptr_eq(&queued.owner, &item.owner) && queued.ids == item.ids)
    {
        pending.push(item);
    }
}

fn prepare_note(
    node: String,
    bound: Option<(snapshots::Snapshot, ExecutionContext)>,
) -> Option<Pending> {
    let (snapshot, execution) = bound?;
    let context = snapshot.context?;
    let owner = snapshot.turn_owner?;
    if !owner.live()
        || !execution.can_restore_workspace()
        || api::opaque_id(&node).is_err()
        || !(node.starts_with("skill:") || node.starts_with("persona:"))
        || context.session_id != execution.conversation().to_string()
        || context.session_id != owner.execution.conversation().to_string()
        || snapshot.ids.is_empty()
        || !context
            .granted_capabilities
            .contains(&api::Capability::Associations)
    {
        return None;
    }
    let mut ids = vec![node];
    for id in snapshot.ids.into_iter().take(31) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Some(Pending {
        context,
        ids,
        owner,
    })
}

pub(crate) fn skill_affinity() -> std::collections::HashMap<String, f64> {
    if crate::core::exec_ctx::current().is_some_and(|execution| !execution.can_restore_workspace())
    {
        return Default::default();
    }
    AFFINITY
        .with(|slot| {
            slot.borrow()
                .last()
                .map(|snapshot| snapshot.affinity.clone())
        })
        .or_else(|| {
            BINDING
                .try_with(|bound| {
                    (bound.execution.can_restore_workspace()
                        && bound
                            .snapshot
                            .turn_owner
                            .as_ref()
                            .is_some_and(|owner| owner.live()))
                    .then(|| bound.snapshot.affinity.clone())
                })
                .ok()
                .flatten()
        })
        .unwrap_or_default()
}

fn take_owned(snapshot: &snapshots::Snapshot, pending: &mut Vec<Pending>) -> Vec<Pending> {
    let Some(owner) = &snapshot.turn_owner else {
        return Vec::new();
    };
    let (work, other): (Vec<_>, Vec<_>) = std::mem::take(pending)
        .into_iter()
        .partition(|item| Arc::ptr_eq(&item.owner, owner));
    *pending = other;
    work
}

/// Reset/cancel/error deliberately drops unattempted work. It is never adopted by a later turn.
pub(crate) fn discard(snapshot: &snapshots::Snapshot) {
    if let Some(owner) = &snapshot.turn_owner {
        owner.feedback_cancel.cancel();
    }
    take_owned(
        snapshot,
        &mut PENDING.lock().unwrap_or_else(|error| error.into_inner()),
    );
}

/// Drain only this exact live snapshot. Runtime admission rechecks selection and generation;
/// original cancellation/deadline travel through dispatch, including physical actor settlement.
pub(crate) async fn flush(snapshot: &snapshots::Snapshot) -> anyhow::Result<()> {
    let work = take_owned(
        snapshot,
        &mut PENDING.lock().unwrap_or_else(|error| error.into_inner()),
    );
    for item in work {
        if !item.owner.live() || !snapshots::is_live_turn(snapshot) {
            continue;
        }
        let operation_id = format!(
            "associate-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        );
        if let Err(error) = super::runtime::request(
            super::runtime::ApprovedInvocation::saved_context(),
            item.context,
            api::Method::Associate,
            serde_json::json!({"operation_id":operation_id,"ids":item.ids}),
            Some(item.owner.execution.clone()),
            item.owner.feedback_cancel.clone(),
        )
        .await
        {
            // No replay after an ambiguous attempt; remaining local work is deliberately dropped.
            super::runtime::optional_failure(error)?;
            crate::ui::tui::note_line(
                "Memory association failed; no automatic retry was attempted.",
            );
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{convo::ConversationId, exec_ctx};

    fn executions() -> (ExecutionContext, ExecutionContext) {
        let parent = ExecutionContext::new(ConversationId::new("association-fixture"))
            .with_dispatch_label(Some("diagnostic label alone is not delegation".into()));
        let child = crate::agent::task_tool::child_execution_context(
            &parent,
            "association-fixture/task/child",
            std::path::Path::new("association-fixture"),
            "child",
        )
        .with_dispatch_label(None)
        .with_trace_visible(true)
        .fork_recovery();
        assert_eq!(parent.conversation(), child.conversation());
        assert!(parent.can_restore_workspace());
        assert!(!child.can_restore_workspace());
        (parent, child)
    }

    fn snapshot(execution: &ExecutionContext) -> snapshots::Snapshot {
        // Inert protocol bytes: no store, config, provider or filesystem is opened.
        let root = std::env::temp_dir()
            .join("aizen-association-no-io-fixture")
            .display()
            .to_string();
        snapshots::Snapshot {
            context: Some(api::ProviderContext {
                generation: 1,
                user_id: "fixture-user".into(),
                workspace_id: "fixture-workspace".into(),
                workspace_root: root.clone(),
                cwd: root.clone(),
                device_id: "fixture-device".into(),
                device_aliases: Vec::new(),
                session_id: execution.conversation().to_string(),
                turn_id: "fixture-turn".into(),
                boundary_id: "fixture-boundary".into(),
                data_root: root,
                settings: Default::default(),
                granted_capabilities: vec![api::Capability::Associations],
                budgets: api::Budgets {
                    context_tokens: 300,
                    recall_tokens: 300,
                    max_records: 12,
                },
            }),
            ids: vec!["parent-memory".into()],
            affinity: std::collections::HashMap::from([("parent-skill".into(), 0.75)]),
            turn_owner: Some({
                let cancel = TurnCancel::new();
                Arc::new(TurnOwner {
                    execution: execution.clone(),
                    feedback_cancel: cancel.child(),
                    cancel,
                })
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tls_child_is_denied_before_lookup_without_parent_task_local_fallback() {
        let (parent, child) = executions();
        let snapshot = snapshot(&parent);
        let token = snapshot.turn_owner.as_ref().unwrap().cancel.clone();
        scoped(snapshot.clone(), parent.clone(), async {
            exec_ctx::with_current(parent.clone(), || {
                crate::core::cancel::with_current(token, || {
                    let bound = binding_with(|session| {
                        assert_eq!(session, parent.conversation().as_str());
                        Some(snapshot.clone())
                    });
                    let item = prepare_note("skill:parent-skill".into(), bound).unwrap();
                    assert_eq!(item.ids, ["skill:parent-skill", "parent-memory"]);
                    assert_eq!(item.context.session_id, parent.conversation().as_str());
                    assert!(item.owner.execution.can_restore_workspace());
                })
            });
            exec_ctx::with_current(child, || {
                assert!(binding_with(|_| panic!("child denied before lookup")).is_none());
                assert!(skill_affinity().is_empty());
            });
            assert_eq!(skill_affinity(), snapshot.affinity);
        })
        .await;
    }

    #[tokio::test]
    async fn task_local_parent_can_associate_but_child_has_no_notes_or_skill_affinity() {
        let (parent, child) = executions();
        let snapshot = snapshot(&parent);
        assert!(exec_ctx::current().is_none());
        scoped(snapshot.clone(), parent.clone(), async {
            let bound = binding_with(|_| panic!("task-local requires no lookup"));
            let item = prepare_note("persona:fixture/insight".into(), bound).unwrap();
            assert_eq!(item.ids, ["persona:fixture/insight", "parent-memory"]);
            assert_eq!(skill_affinity(), snapshot.affinity);
            scoped(snapshot.clone(), child, async {
                assert!(binding().is_none());
                assert!(skill_affinity().is_empty());
            })
            .await;
        })
        .await;
        assert!(binding().is_none());
    }

    #[test]
    fn prepared_notes_keep_session_recall_and_capability_checks() {
        let (parent, child) = executions();
        let snapshot = snapshot(&parent);
        let mut wrong_session = snapshot.clone();
        wrong_session.context.as_mut().unwrap().session_id = "other-session".into();
        let mut no_recall = snapshot.clone();
        no_recall.ids.clear();
        let mut no_grant = snapshot.clone();
        no_grant
            .context
            .as_mut()
            .unwrap()
            .granted_capabilities
            .clear();
        for invalid in [
            wrong_session,
            no_recall,
            no_grant,
            snapshots::Snapshot::default(),
        ] {
            assert!(
                prepare_note("skill:fixture".into(), Some((invalid, parent.clone()))).is_none()
            );
        }
        assert!(prepare_note("skill:fixture".into(), Some((snapshot, child))).is_none());
    }

    #[test]
    fn later_turn_same_session_cannot_adopt_earlier_work_or_token() {
        let (parent, _) = executions();
        let first = snapshot(&parent);
        let later = snapshot(&parent);
        let item = prepare_note(
            "skill:fixture".into(),
            Some((first.clone(), parent.clone())),
        )
        .unwrap();
        let mut pending = vec![item];
        assert!(take_owned(&later, &mut pending).is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(take_owned(&first, &mut pending).len(), 1);
        let old_token = first.turn_owner.as_ref().unwrap().cancel.clone();
        exec_ctx::with_current(parent, || {
            crate::core::cancel::with_current(old_token, || {
                assert!(binding_with(|_| Some(later)).is_none());
            })
        });
    }

    #[test]
    fn cancelled_and_expired_owners_cannot_prepare_feedback() {
        let (parent, _) = executions();
        let cancelled = snapshot(&parent);
        cancelled.turn_owner.as_ref().unwrap().cancel.cancel();
        assert!(prepare_note("skill:fixture".into(), Some((cancelled, parent.clone()))).is_none());
        let expired = parent.with_deadline(Some(std::time::Instant::now()));
        assert!(
            prepare_note("skill:fixture".into(), Some((snapshot(&expired), expired))).is_none()
        );
    }

    #[test]
    fn reset_revokes_feedback_without_cancelling_the_completed_coding_turn() {
        let (parent, _) = executions();
        let snapshot = snapshot(&parent);
        let owner = snapshot.turn_owner.as_ref().unwrap();
        discard(&snapshot);
        assert!(!owner.cancel.is_cancelled());
        assert!(owner.feedback_cancel.is_cancelled());
        assert!(prepare_note("skill:fixture".into(), Some((snapshot.clone(), parent))).is_none());
    }

    #[test]
    fn explicit_affinity_binding_restores_parent_and_is_empty_without_a_binding() {
        let (parent, _) = executions();
        let snapshot = snapshot(&parent);
        assert!(skill_affinity().is_empty());
        with_affinity(&snapshot, || {
            assert_eq!(skill_affinity(), snapshot.affinity);
            with_affinity(&snapshots::Snapshot::default(), || {
                assert!(skill_affinity().is_empty())
            });
            assert_eq!(skill_affinity(), snapshot.affinity);
        });
        assert!(skill_affinity().is_empty());
    }
}
