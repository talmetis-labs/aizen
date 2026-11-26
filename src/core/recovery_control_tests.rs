#[test]
fn control_sidecar_roundtrip_legacy_and_required_fields() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = ActiveFixture::new();
    let history = vec![Message::user("restricted recovery")];
    let task = TaskState::default();
    let control = SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 2,
        ..SessionControl::default()
    };
    checkpoint_history_with_control(&history, None, RecoveryPhase::Idle, None, &task, &control)
        .unwrap();
    let manifest = fixture.stored();
    assert_eq!(manifest.schema, SCHEMA);
    let path = history_path(&fixture.dir, manifest.sidecar_generation);
    let original = fs::read(&path).unwrap();
    let baseline: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(baseline["version"], 1);
    assert_eq!(
        read_authoritative_snapshot(&fixture.dir, &manifest)
            .unwrap()
            .3,
        control
    );
    // Ownerless checkpoints retain the existing restriction instead of manufacturing normal mode.
    checkpoint_history_with_state(&history, None, RecoveryPhase::Idle, None, &task).unwrap();
    let manifest = fixture.stored();
    let path = history_path(&fixture.dir, manifest.sidecar_generation);
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("version");
    legacy.as_object_mut().unwrap().remove("session_control");
    fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(
        read_authoritative_snapshot(&fixture.dir, &manifest)
            .unwrap()
            .3,
        SessionControl::default()
    );
    let manifest_bytes = fs::read(manifest_path(&fixture.dir)).unwrap();
    for (version, control_value) in [
        (1, None),
        (1, Some(serde_json::Value::Null)),
        (0, Some(serde_json::Value::Null)),
        (
            1,
            Some(
                serde_json::json!({"version": 99, "mode": "normal", "generation": 0, "plan_target": null}),
            ),
        ),
        (9, Some(serde_json::to_value(&control).unwrap())),
    ] {
        let mut invalid = legacy.clone();
        invalid["version"] = version.into();
        if let Some(value) = control_value {
            invalid["session_control"] = value;
        }
        let bytes = serde_json::to_vec(&invalid).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(read_authoritative_snapshot(&fixture.dir, &manifest).is_err());
        assert!(checkpoint_history_with_control(
            &history,
            None,
            RecoveryPhase::Idle,
            None,
            &task,
            &control
        )
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            fs::read(manifest_path(&fixture.dir)).unwrap(),
            manifest_bytes
        );
    }
}

#[test]
fn control_restore_refusal_keeps_recovery_offer_unconsumed() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = ActiveFixture::new();
    let control = SessionControl {
        mode: crate::core::session_control::ExecutionMode::Planning,
        generation: 1,
        ..SessionControl::default()
    };
    checkpoint_history_with_control(
        &[Message::user("recover")],
        None,
        RecoveryPhase::Idle,
        None,
        &TaskState::default(),
        &control,
    )
    .unwrap();
    fixture.exit_process();
    let offers = scan_stale("owned-scope");
    assert_eq!(offers.len(), 1);
    let owner = crate::agent::todo::TaskContext::private();
    let lease = owner.control().policy().admit().unwrap();
    let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
    crate::core::exec_ctx::with_current(exec, || {
        assert!(accept_with_state(&offers[0]).is_err());
    });
    assert!(manifest_path(&fixture.dir).is_file());
    assert_eq!(owner.control().snapshot(), SessionControl::default());
    drop(lease);
    let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
    crate::core::exec_ctx::with_current(exec, || {
        assert_eq!(accept_with_state(&offers[0]).unwrap().4, control);
    });
    assert_eq!(owner.control().snapshot(), control);
}
