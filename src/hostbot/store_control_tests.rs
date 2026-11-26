#[test]
fn lane_control_legacy_roundtrip_and_invalid_state_blocks_execution_and_rewrite() {
    let (_home_lock, _home) = with_temp_home();
    let path = session_path("fixture", "policy", "1");
    let legacy =
        serde_json::json!({"platform": "fixture", "route": "policy", "chat": "1", "messages": []});
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(
        load_session_with_state("fixture", "policy", "1")
            .unwrap()
            .unwrap()
            .2,
        SessionControl::default()
    );
    let control = SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 4,
        ..SessionControl::default()
    };
    let task = crate::agent::todo::TaskState::default();
    let history = vec![Message::user("restricted lane")];
    save_session_with_control("fixture", "policy", "1", &history, &task, &control).unwrap();
    assert_eq!(
        load_session_with_state("fixture", "policy", "1")
            .unwrap()
            .unwrap()
            .2,
        control
    );
    save_session("fixture", "policy", "1", &history).unwrap();
    assert_eq!(
        load_session_with_state("fixture", "policy", "1")
            .unwrap()
            .unwrap()
            .2,
        control
    );
    let baseline: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (version, value) in [
        (1, None),
        (1, Some(serde_json::Value::Null)),
        (0, Some(serde_json::Value::Null)),
        (
            1,
            Some(
                serde_json::json!({"version": 99, "mode": "normal", "generation": 0, "plan_target": null}),
            ),
        ),
        (2, Some(serde_json::to_value(&control).unwrap())),
    ] {
        let mut invalid = baseline.clone();
        invalid["version"] = version.into();
        match value {
            Some(value) => invalid["session_control"] = value,
            None => {
                invalid.as_object_mut().unwrap().remove("session_control");
            }
        }
        let bytes = serde_json::to_vec(&invalid).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_sessions_with_state("fixture").is_empty());
        assert!(
            load_session_with_state("fixture", "policy", "1").is_err(),
            "missing scan seed cannot default to normal"
        );
        assert!(save_session("fixture", "policy", "1", &history).is_err());
        assert!(save_session_with_control(
            "fixture",
            "policy",
            "1",
            &history,
            &task,
            &SessionControl::default()
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn lane_active_normal_cannot_overwrite_saved_restriction() {
    let (_home_lock, _home) = with_temp_home();
    let history = vec![Message::user("restricted lane")];
    let control = SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 1,
        ..SessionControl::default()
    };
    save_session_with_control(
        "fixture",
        "conflict",
        "1",
        &history,
        &crate::agent::todo::TaskState::default(),
        &control,
    )
    .unwrap();
    let path = session_path("fixture", "conflict", "1");
    let bytes = std::fs::read(&path).unwrap();
    let owner = crate::agent::todo::TaskContext::private();
    let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner);
    crate::core::exec_ctx::with_current(exec, || {
        assert!(save_session("fixture", "conflict", "1", &history).is_err());
    });
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn lane_gc_preserves_restricted_and_corrupt_policy_records() {
    let (_home_lock, _home) = with_temp_home();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_SESSION_TTL_DAYS", "1")]);
    let control = SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 1,
        ..SessionControl::default()
    };
    let history = vec![Message::user("keep restriction")];
    save_session_with_control(
        "fixture",
        "gc-policy",
        "restricted",
        &history,
        &crate::agent::todo::TaskState::default(),
        &control,
    )
    .unwrap();
    let corrupt = session_path("fixture", "gc-policy", "corrupt");
    std::fs::write(&corrupt, b"{broken policy").unwrap();
    let restricted = session_path("fixture", "gc-policy", "restricted");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60);
    for path in [&restricted, &corrupt] {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    assert_eq!(gc_sessions(), 0);
    assert!(corrupt.exists());
    assert_eq!(
        load_session_with_state("fixture", "gc-policy", "restricted")
            .unwrap()
            .unwrap()
            .2,
        control
    );
}

#[test]
fn route_reset_keeps_restrictions_and_corruption_but_deletes_normal_transcripts() {
    let (_home_lock, _home) = with_temp_home();
    let history = vec![Message::user("route history")];
    let task = crate::agent::todo::TaskState::default();
    let control = SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 8,
        ..SessionControl::default()
    };
    save_session_with_control(
        "fixture",
        "route-reset",
        "planning",
        &history,
        &task,
        &control,
    )
    .unwrap();
    save_session_with_control(
        "fixture",
        "route-reset",
        "normal",
        &history,
        &task,
        &SessionControl::default(),
    )
    .unwrap();
    save_session_with_control(
        "fixture",
        "other-route",
        "sibling",
        &history,
        &task,
        &control,
    )
    .unwrap();
    let corrupt = session_path("fixture", "route-reset", "broken");
    std::fs::write(&corrupt, b"{unreadable policy").unwrap();
    assert!(drop_session("fixture", "route-reset", "planning").is_err());
    let before = std::fs::read(session_path("fixture", "route-reset", "planning")).unwrap();
    let mut stale = control.clone();
    stale.generation -= 1;
    assert!(reset_session_with_control("fixture", "route-reset", "planning", &stale).is_err());
    assert!(reset_session_with_control(
        "fixture",
        "route-reset",
        "planning",
        &SessionControl::default()
    )
    .is_err());
    assert_eq!(
        std::fs::read(session_path("fixture", "route-reset", "planning")).unwrap(),
        before
    );
    assert!(drop_session("fixture", "route-reset", "broken").is_err());
    drop_route_sessions("fixture", "route-reset");
    let (messages, saved_task, saved_control) =
        load_session_with_state("fixture", "route-reset", "planning")
            .unwrap()
            .unwrap();
    assert!(messages.is_empty());
    assert_eq!(saved_task, task);
    assert_eq!(saved_control, control);
    assert!(load_session_with_state("fixture", "route-reset", "normal")
        .unwrap()
        .is_none());
    assert_eq!(std::fs::read(&corrupt).unwrap(), b"{unreadable policy");
    assert_eq!(
        load_session_with_state("fixture", "other-route", "sibling")
            .unwrap()
            .unwrap()
            .0
            .len(),
        1
    );
}
