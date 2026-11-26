use super::*;
use crate::core::session_control::{ExecutionMode, PlanTarget};

fn planning() -> SessionControl {
    SessionControl {
        mode: ExecutionMode::Planning,
        generation: 3,
        plan_target: Some(PlanTarget {
            root: std::env::temp_dir().to_string_lossy().into_owned(),
            path: "docs/plans/persistence.md".into(),
            id: "p-0123456789abcdef0123456789abcdef".into(),
        }),
        ..SessionControl::default()
    }
}

fn with_pool(f: impl FnOnce()) {
    let _lock = config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let _env = config::EnvGuard::set([("AIZEN_HOME", dir.path().as_os_str())]);
    std::fs::create_dir_all(sessions_dir()).unwrap();
    f();
}

#[test]
fn legacy_defaults_and_current_required_control_fail_closed() {
    for value in [
        serde_json::json!([]),
        serde_json::json!({"messages": []}),
        serde_json::json!({"version": 1, "messages": []}),
        serde_json::json!({"version": 2, "messages": []}),
    ] {
        assert_eq!(
            parse_session_full(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .4,
            SessionControl::default()
        );
    }
    let control = planning();
    let valid = serde_json::json!({"version": 3, "messages": [], "session_control": control});
    assert_eq!(
        parse_session_full(&serde_json::to_vec(&valid).unwrap())
            .unwrap()
            .4,
        control
    );
    for version in [0, 1, 2, 3] {
        for bad in [
            Value::Null,
            serde_json::json!({}),
            serde_json::json!({"version": 99, "mode": "normal", "generation": 0, "plan_target": null}),
        ] {
            let mut value = valid.clone();
            value["version"] = version.into();
            value["session_control"] = bad;
            assert!(parse_session_full(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }
    for version in [3, 4, 99] {
        let value = serde_json::json!({"version": version, "messages": []});
        assert!(parse_session_full(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut future = valid;
    future["version"] = 4.into();
    assert!(parse_session_full(&serde_json::to_vec(&future).unwrap()).is_err());
}

#[test]
fn malformed_control_cannot_be_rewritten_even_by_explicit_transition_save() {
    with_pool(|| {
        let history = vec![Message::user("private history")];
        let task = TaskState::default();
        let control = planning();
        save_session_with_control(&history, "bad-control", None, None, &task, &control).unwrap();
        let path = sessions_dir().join("bad-control.json");
        let baseline: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut invalid_target = serde_json::to_value(&control).unwrap();
        invalid_target["plan_target"]["path"] = "../code.rs".into();
        let mut future = serde_json::to_value(&control).unwrap();
        future["version"] = 999.into();
        let mut unknown = serde_json::to_value(&control).unwrap();
        unknown["allow_shell"] = true.into();
        for bad in [
            None,
            Some(Value::Null),
            Some(future),
            Some(unknown),
            Some(invalid_target),
        ] {
            let mut envelope = baseline.clone();
            match bad {
                Some(value) => envelope["session_control"] = value,
                None => {
                    envelope.as_object_mut().unwrap().remove("session_control");
                }
            }
            let bytes = serde_json::to_vec(&envelope).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(read_session_for_edit_with_state("bad-control").is_err());
            assert!(
                autosave_session_delta_with_state(&history, "bad-control", None, None, &task)
                    .is_err()
            );
            assert!(save_session_with_control(
                &history,
                "bad-control",
                None,
                None,
                &task,
                &SessionControl::default()
            )
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    });
}

#[test]
fn offline_compaction_retag_and_ownerless_save_keep_disk_restriction() {
    with_pool(|| {
        let history = vec![Message::user("saved plan")];
        let control = planning();
        let task = TaskState::default();
        write_session_with_control(
            &history,
            "restricted",
            SessionMeta {
                project_slug: Some("old-control-zone".into()),
                ..SessionMeta::default()
            },
            None,
            &task,
            &control,
        )
        .unwrap();
        let ((_, meta, design, _, loaded), revision) =
            read_session_for_edit_with_state("restricted").unwrap();
        assert_eq!(loaded, control);
        let other = todo::TaskContext::private();
        let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(other.clone());
        crate::core::exec_ctx::with_current(exec, || {
            write_session_if_unchanged(
                &history,
                "restricted",
                meta.unwrap(),
                design.as_ref(),
                &revision,
            )
            .unwrap();
            assert_eq!(
                other.control().snapshot(),
                SessionControl::default(),
                "inspection must not restore"
            );
            assert!(
                save_session_with_state(&history, "restricted", None, None, &other.snapshot())
                    .is_err()
            );
        });
        let mut errors = Vec::new();
        assert_eq!(
            retag_sessions_of_slug("old-control-zone", &mut |e| errors.push(e)),
            1
        );
        assert!(errors.is_empty(), "{errors:?}");
        save_session_with_state(&history, "restricted", None, None, &task).unwrap();
        assert_eq!(
            read_session_path_with_state(&sessions_dir().join("restricted.json"))
                .unwrap()
                .4,
            control
        );
    });
}

#[test]
fn delta_policy_change_forces_full_generation_and_cas_detects_control_only_edit() {
    with_pool(|| {
        let task = TaskState::default();
        let mut history = vec![Message::user("first")];
        save_session_with_control(
            &history,
            "delta-policy",
            None,
            None,
            &task,
            &SessionControl::default(),
        )
        .unwrap();
        let path = sessions_dir().join("delta-policy.json");
        history.push(Message::assistant("second"));
        autosave_session_delta_with_state(&history, "delta-policy", None, None, &task).unwrap();
        let old_delta = std::fs::read(delta_path(&path)).unwrap();
        let ((_, meta, design), revision) = read_session_for_edit("delta-policy").unwrap();
        let mut disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let generation = disk["generation"].clone();
        let control = planning();
        disk["session_control"] = serde_json::to_value(&control).unwrap();
        std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        assert!(write_session_if_unchanged(
            &history,
            "delta-policy",
            meta.unwrap(),
            design.as_ref(),
            &revision
        )
        .is_err());
        history.push(Message::assistant("third"));
        autosave_session_delta_with_state(&history, "delta-policy", None, None, &task).unwrap();
        assert!(
            !delta_path(&path).exists(),
            "changed policy must never use the old append fingerprint"
        );
        let envelope: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_ne!(envelope["generation"], generation);
        std::fs::write(delta_path(&path), old_delta).unwrap();
        let loaded = read_session_path_with_state(&path).unwrap();
        assert_eq!(loaded.0.len(), history.len());
        assert_eq!(loaded.4, control);
    });
}

#[test]
fn explicit_transition_save_never_reenters_control_and_stale_snapshots_refuse() {
    with_pool(|| {
        let owner = todo::TaskContext::private();
        let history = vec![Message::user("transition")];
        let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
        crate::core::exec_ctx::with_current(exec, || {
            let prepared = prepare_control_transition(
                &history,
                "transition",
                SessionMeta::default(),
                None,
                &owner.snapshot(),
                &owner.control().snapshot(),
                None,
            )
            .unwrap();
            owner
                .control()
                .transition(ExecutionMode::Planning, planning().plan_target, |control| {
                    prepared.save(control).map(|_| ())
                })
                .unwrap();
            let task = owner.snapshot();
            let snapshot = PersistenceSnapshot::capture(&task).unwrap();
            assert!(owner
                .control()
                .transition(ExecutionMode::Normal, None, |_| panic!("admitted save"))
                .is_err());
            owner
                .replace(vec![todo::Todo::new("new work", todo::Status::Pending)])
                .unwrap();
            assert!(snapshot.check(&task).is_err());
        });
    });
}

#[test]
fn prepared_transition_rejects_stale_policy_and_changed_transcript() {
    with_pool(|| {
        let task = TaskState::default();
        let history = vec![Message::user("initial")];
        let control = planning();
        write_session_with_control(
            &history,
            "checked-transition",
            SessionMeta::default(),
            None,
            &task,
            &control,
        )
        .unwrap();
        let (snapshot, revision) = read_session_for_edit_with_state("checked-transition").unwrap();
        let mut changed = control.clone();
        changed.generation += 1;
        write_session_with_control(
            &history,
            "checked-transition",
            SessionMeta::default(),
            None,
            &task,
            &changed,
        )
        .unwrap();
        assert!(prepare_control_transition(
            &history,
            "checked-transition",
            SessionMeta::default(),
            None,
            &task,
            &control,
            None,
        )
        .is_err());
        assert!(
            prepare_control_transition(
                &snapshot.0,
                "checked-transition",
                SessionMeta::default(),
                None,
                &task,
                &changed,
                Some(&revision),
            )
            .is_err(),
            "a current policy does not authorize overwriting stale history"
        );

        let prepared = prepare_control_transition(
            &history,
            "checked-transition",
            SessionMeta::default(),
            None,
            &task,
            &changed,
            None,
        )
        .unwrap();
        let path = sessions_dir().join("checked-transition.json");
        // A delta edit after preparation is part of the CAS, even without a policy change.
        std::fs::write(delta_path(&path), b"new concurrent delta\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        let owner = todo::TaskContext::private();
        owner.control().restore(changed).unwrap();
        assert!(owner
            .control()
            .transition(ExecutionMode::Normal, None, |next| {
                prepared.save(next).map(|_| ())
            })
            .is_err());
        assert!(owner.control().snapshot().is_planning());
        assert!(owner.control().policy().validate_current().is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    });
}

#[test]
fn prepared_new_transition_refuses_appeared_or_missing_restricted_session() {
    with_pool(|| {
        let owner = todo::TaskContext::private();
        let task = owner.snapshot();
        let prepared = prepare_control_transition(
            &[],
            "appeared",
            SessionMeta::default(),
            None,
            &task,
            &SessionControl::default(),
            None,
        )
        .unwrap();
        write_session_with_control(
            &[],
            "appeared",
            SessionMeta::default(),
            None,
            &task,
            &SessionControl::default(),
        )
        .unwrap();
        let path = sessions_dir().join("appeared.json");
        let before = std::fs::read(&path).unwrap();
        assert!(owner
            .control()
            .transition(ExecutionMode::Planning, planning().plan_target, |next| {
                prepared.save(next).map(|_| ())
            })
            .is_err());
        assert!(owner.control().policy().validate_current().is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(prepare_control_transition(
            &[],
            "missing-restricted",
            SessionMeta::default(),
            None,
            &task,
            &planning(),
            None,
        )
        .is_err());
    });
}

#[test]
fn refused_execution_restore_does_not_replace_history_or_slug() {
    with_pool(|| {
        let owner = todo::TaskContext::private();
        let mut other = planning();
        other.plan_target.as_mut().unwrap().path = "docs/plans/other.md".into();
        owner.control().restore(planning()).unwrap();
        save_session_with_control(
            &[Message::user("other")],
            "other-policy",
            None,
            None,
            &TaskState::default(),
            &other,
        )
        .unwrap();
        let exec = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
        let previous = current_session_slug();
        let mut history = vec![Message::user("keep")];
        crate::core::exec_ctx::with_current(exec, || {
            assert!(load_session_with_state(&mut history, "other-policy", "fixture").is_err());
        });
        assert_eq!(history[0].content.as_deref(), Some("keep"));
        assert_eq!(current_session_slug(), previous);
        assert_eq!(owner.control().snapshot(), planning());
    });
}
