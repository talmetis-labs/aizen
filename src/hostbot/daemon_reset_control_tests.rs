fn reset_control_fixture() -> crate::core::session_control::SessionControl {
    crate::core::session_control::SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 4,
        plan_target: Some(crate::core::session_control::PlanTarget {
            root: std::env::temp_dir().to_string_lossy().into_owned(),
            path: "docs/plans/hostbot-reset.md".into(),
            id: "p-0123456789abcdef0123456789abcdef".into(),
        }),
        ..crate::core::session_control::SessionControl::default()
    }
}

#[test]
fn planning_reset_commands_keep_policy_after_reload_and_only_clear_current_lane() {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (command, argument) in [("new", ""), ("reset", ""), ("model", "fixture-model")] {
        let fixture = MemoryCommandFixture::new();
        let owner = crate::agent::todo::TaskContext::private();
        owner
            .replace(vec![crate::agent::todo::Todo::new(
                "unfinished implementation",
                crate::agent::todo::Status::InProgress,
            )])
            .unwrap();
        let mut history = vec![Message::user("saved planning conversation")];
        let control = reset_control_fixture();
        // A normal freshly allocated lane must restore saved planning before deciding how to reset.
        store::save_session_with_control(
            OfflinePlatform.name(),
            "fixture-route",
            "1",
            &history,
            &owner.snapshot(),
            &control,
        )
        .unwrap();
        store::save_session_with_control(
            OfflinePlatform.name(),
            "fixture-route",
            "2",
            &history,
            &owner.snapshot(),
            &control,
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let reply = runtime
            .block_on(crate::agent::todo::with_context(
                owner.clone(),
                handle_command(
                    &OfflinePlatform,
                    command,
                    argument,
                    &mut history,
                    "fixture-route",
                    1,
                    "fixture-default",
                    &Weak::new(),
                    &tx,
                ),
            ))
            .unwrap();
        assert!(!reply.contains("refused"), "{reply}");
        assert!(history.is_empty());
        assert!(owner.snapshot().items.is_empty());
        assert_eq!(owner.control().snapshot(), control);
        let (saved_history, saved_task, saved_control) =
            store::load_session_with_state(OfflinePlatform.name(), "fixture-route", "1")
                .unwrap()
                .unwrap();
        assert!(saved_history.is_empty());
        assert_eq!(saved_task, crate::agent::todo::TaskState::default());
        assert_eq!(saved_control, control);
        let resumed = crate::agent::todo::TaskContext::private();
        resumed.control().restore(saved_control).unwrap();
        resumed.restore(saved_task).unwrap();
        assert!(resumed.control().snapshot().is_planning());
        assert!(
            !store::load_session_with_state(OfflinePlatform.name(), "fixture-route", "2")
                .unwrap()
                .unwrap()
                .0
                .is_empty()
        );
        assert!(crate::plugin::snapshots::current(&fixture.key)
            .context
            .is_none());
        assert!(!crate::plugin::snapshots::current(&fixture.sibling)
            .turn_prefix(true)
            .is_empty());
    }
}

#[test]
fn malformed_reset_refuses_before_history_task_and_memory_are_cleared() {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for command in ["new", "reset", "model"] {
        let fixture = MemoryCommandFixture::new();
        let owner = crate::agent::todo::TaskContext::private();
        owner
            .replace(vec![crate::agent::todo::Todo::new(
                "retain unfinished task",
                crate::agent::todo::Status::InProgress,
            )])
            .unwrap();
        let before_task = owner.snapshot();
        let mut history = vec![Message::user("keep context")];
        let before_history = serde_json::to_value(&history).unwrap();
        store::save_session_with_control(
            OfflinePlatform.name(),
            "fixture-route",
            "1",
            &history,
            &before_task,
            &reset_control_fixture(),
        )
        .unwrap();
        let path = fixture
            .home
            .path()
            .join("hostbot/sessions/memory-command-fixture.fixture-route.1.json");
        let bytes = br#"{"version":1,"platform":"memory-command-fixture","route":"fixture-route","chat":"1","messages":[],"session_control":null}"#;
        std::fs::write(&path, bytes).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let argument = if command == "model" {
            "fixture-model"
        } else {
            ""
        };
        let reply = runtime
            .block_on(crate::agent::todo::with_context(
                owner.clone(),
                handle_command(
                    &OfflinePlatform,
                    command,
                    argument,
                    &mut history,
                    "fixture-route",
                    1,
                    "fixture-default",
                    &Weak::new(),
                    &tx,
                ),
            ))
            .unwrap();
        assert!(reply.contains("refused"), "{reply}");
        assert_eq!(owner.snapshot(), before_task);
        assert_eq!(serde_json::to_value(&history).unwrap(), before_history);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!crate::plugin::snapshots::current(&fixture.key)
            .turn_prefix(true)
            .is_empty());
    }
}

#[test]
fn reset_with_admitted_worker_refuses_without_mutating_saved_or_live_state() {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = MemoryCommandFixture::new();
    let owner = crate::agent::todo::TaskContext::private();
    owner.control().restore(reset_control_fixture()).unwrap();
    owner
        .replace(vec![crate::agent::todo::Todo::new(
            "worker owns task",
            crate::agent::todo::Status::InProgress,
        )])
        .unwrap();
    let task = owner.snapshot();
    let mut history = vec![Message::user("worker still live")];
    store::save_session_with_control(
        OfflinePlatform.name(),
        "fixture-route",
        "1",
        &history,
        &task,
        &reset_control_fixture(),
    )
    .unwrap();
    let lease = owner.control().policy().admit().unwrap();
    let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
    crate::core::exec_ctx::with_current(exec, || {
        assert!(
            reset_lane_context(OfflinePlatform.name(), "fixture-route", "1", &mut history).is_err()
        );
    });
    assert_eq!(history.len(), 1);
    assert_eq!(owner.snapshot(), task);
    assert_eq!(
        store::load_session_with_state(OfflinePlatform.name(), "fixture-route", "1")
            .unwrap()
            .unwrap()
            .0
            .len(),
        1
    );
    assert!(!crate::plugin::snapshots::current(&fixture.key)
        .turn_prefix(true)
        .is_empty());
    drop(lease);
    // A never-saved lane needs the same settled-worker precondition.
    let lease = owner.control().policy().admit().unwrap();
    let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
    crate::core::exec_ctx::with_current(exec, || {
        assert!(reset_lane_context(
            OfflinePlatform.name(),
            "fixture-route",
            "unsaved",
            &mut history
        )
        .is_err());
    });
    assert_eq!(history.len(), 1);
    assert_eq!(owner.snapshot(), task);
    drop(lease);
}

#[test]
fn normal_reset_still_removes_saved_transcript_and_clears_memory() {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = MemoryCommandFixture::new();
    let owner = crate::agent::todo::TaskContext::private();
    owner
        .replace(vec![crate::agent::todo::Todo::new(
            "normal task",
            crate::agent::todo::Status::Pending,
        )])
        .unwrap();
    let mut history = vec![Message::user("normal lane")];
    store::save_session_with_control(
        OfflinePlatform.name(),
        "fixture-route",
        "1",
        &history,
        &owner.snapshot(),
        &owner.control().snapshot(),
    )
    .unwrap();
    let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
    crate::core::exec_ctx::with_current(exec, || {
        reset_lane_context(OfflinePlatform.name(), "fixture-route", "1", &mut history)
    })
    .unwrap();
    assert!(history.is_empty());
    assert!(owner.snapshot().items.is_empty());
    assert!(!owner.control().snapshot().is_planning());
    assert!(
        store::load_session_with_state(OfflinePlatform.name(), "fixture-route", "1")
            .unwrap()
            .is_none()
    );
    assert!(crate::plugin::snapshots::current(&fixture.key)
        .context
        .is_none());
}
