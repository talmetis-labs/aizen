use super::*;
use crate::core::exec_ctx::ExecutionContext;
use crate::core::session_control::SessionControl;

struct SlugGuard(Option<String>);
impl Drop for SlugGuard {
    fn drop(&mut self) {
        session_store::set_session_slug(self.0.take());
    }
}

fn fixture(f: impl FnOnce(&std::path::Path, todo::TaskContext)) {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let _slug = SlugGuard(session_store::current_session_slug());
    session_store::set_session_slug(None);
    let owner = todo::TaskContext::private();
    let copy = owner.clone();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(todo::with_context(owner, async { f(&root, copy) }));
}

fn pending_work(owner: &todo::TaskContext) -> todo::TaskState {
    owner
        .replace(vec![todo::Todo::new(
            "implementation remains",
            todo::Status::Pending,
        )])
        .unwrap();
    owner.invalidate_effects();
    let state = owner.snapshot();
    assert!(!state.obligations.is_empty());
    state
}

#[test]
fn planning_workspace_uses_explicit_binding_and_never_falls_back_when_missing() {
    use crate::core::file_placement::{tests::TestDir, PlacementContext};

    let fixture = TestDir::new();
    let root = fixture.path().join("bound");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let ctx = ExecutionContext::default().with_placement(
        PlacementContext::with_paths(
            root.clone(),
            fixture.path().join("home"),
            fixture.path().join("process"),
            "planning-root",
        )
        .unwrap(),
    );
    crate::core::exec_ctx::with_current(ctx.clone(), || {
        assert_eq!(workspace_root().unwrap(), root);
    });
    std::fs::remove_dir(&root).unwrap();
    crate::core::exec_ctx::with_current(ctx, || {
        assert!(workspace_root()
            .unwrap_err()
            .to_string()
            .contains("unavailable"));
    });
}

#[test]
fn repl_entry_exit_persist_without_completing_or_replacing_implementation_work() {
    fixture(|root, owner| {
        let before = pending_work(&owner);
        let mut history = vec![Message::user("investigate")];
        let old_normal = ExecutionContext::default().with_task(owner.clone());
        transition_in(true, None, &mut history, "fixture", None, root, None, || {}).unwrap();
        let slug = session_store::current_session_slug().unwrap();
        let ((_, _, _, disk_task, disk_control), _) =
            session_store::read_session_for_edit_with_state(&slug).unwrap();
        assert_eq!(disk_task, before);
        assert_eq!(owner.snapshot(), before);
        assert!(disk_control.is_planning());
        assert!(old_normal.policy().unwrap().validate_current().is_err());
        let old_planning = ExecutionContext::default().with_task(owner.clone());
        transition_in(
            false,
            None,
            &mut history,
            "fixture",
            None,
            root,
            Some(slug.clone()),
            || {},
        )
        .unwrap();
        let ((_, _, _, disk_task, exited), _) =
            session_store::read_session_for_edit_with_state(&slug).unwrap();
        assert_eq!(disk_task, before);
        assert_eq!(owner.snapshot(), before);
        assert!(!exited.is_planning());
        assert_eq!(exited.plan_target, disk_control.plan_target);
        assert!(old_planning.is_planning());
        assert!(!old_planning.permits_automation());
        assert!(ExecutionContext::default()
            .with_task(owner)
            .permits_automation());
    });
}

#[test]
fn failed_entry_blocks_execution_but_status_remains_available_without_refresh() {
    fixture(|root, owner| {
        let before = pending_work(&owner);
        let mut history = vec![Message::user("keep history")];
        let before_history = serde_json::to_value(&history).unwrap();
        std::fs::create_dir_all(crate::core::config::aizen_home()).unwrap();
        std::fs::write(session_store::sessions_dir(), b"not a directory").unwrap();
        assert!(
            transition_in(true, None, &mut history, "fixture", None, root, None, || {}).is_err()
        );
        assert!(owner.control().policy().validate_current().is_err());
        assert_eq!(owner.snapshot(), before);
        assert!(session_store::current_session_slug().is_none());
        let text = status_text(&mut history).unwrap();
        assert!(text.contains("execution blocked"));
        assert!(text.contains("refresh suppressed while blocked"));
        assert_eq!(serde_json::to_value(&history).unwrap(), before_history);
        assert_eq!(owner.snapshot(), before);
    });
}

#[test]
fn failed_exit_preserves_restriction_task_and_corrupt_saved_bytes() {
    fixture(|root, owner| {
        let before = pending_work(&owner);
        let mut history = vec![Message::user("inspect")];
        transition_in(true, None, &mut history, "fixture", None, root, None, || {}).unwrap();
        let slug = session_store::current_session_slug().unwrap();
        let control = owner.control().snapshot();
        let path = session_store::sessions_dir().join(format!("{slug}.json"));
        let bytes = br#"{"version":3,"messages":[],"session_control":null}"#;
        std::fs::write(&path, bytes).unwrap();
        assert!(transition_in(
            false,
            None,
            &mut history,
            "fixture",
            None,
            root,
            Some(slug),
            || {}
        )
        .is_err());
        assert_eq!(owner.control().snapshot(), control);
        assert_eq!(owner.snapshot(), before);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        let text = status_text(&mut history).unwrap();
        assert!(text.contains("Planning: on (execution blocked"));
    });
}

#[test]
fn workers_refuse_transition_and_task_reset_or_legacy_restore_cannot_widen() {
    fixture(|root, owner| {
        let mut history = vec![Message::user("inspect")];
        let lease = owner.control().policy().admit().unwrap();
        assert!(
            transition_in(true, None, &mut history, "fixture", None, root, None, || {}).is_err()
        );
        assert_eq!(owner.control().snapshot(), SessionControl::default());
        assert!(session_store::current_session_slug().is_none());
        drop(lease);
        transition_in(true, None, &mut history, "fixture", None, root, None, || {}).unwrap();
        let restriction = owner.control().snapshot();
        owner.clear();
        owner
            .replace(vec![todo::Todo::new(
                "replacement task",
                todo::Status::Pending,
            )])
            .unwrap();
        owner.finish_turn(todo::TurnEnd::Interrupted);
        owner.control().restore(SessionControl::default()).unwrap();
        assert_eq!(owner.control().snapshot(), restriction);
        assert!(!ExecutionContext::default()
            .with_task(owner)
            .permits_automation());
    });
}
