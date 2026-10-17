use super::*;

#[test]
fn scoped_native_reads_ignore_cache_candidates_and_reject_outside_entries() {
    use crate::core::native_scope::NativeWriteScope;

    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (_fixture, mut config) = placement_executor_fixture();
    let root = config.effective_root();
    let home = config
        .exec_ctx
        .placement()
        .unwrap()
        .home_root()
        .to_path_buf();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()),
        ("AIZEN_NO_HOOKS", std::ffi::OsStr::new("1")),
    ]);
    std::fs::write(root.join("allowed.txt"), "owned content").unwrap();
    std::fs::write(root.join("outside.txt"), "private outside").unwrap();
    let registry = builtin::file_registry_for_test(&root);
    let tool = registry.get_arc("file_read").unwrap();
    let broad = config.exec_ctx.clone();
    config.exec_ctx = config
        .exec_ctx
        .with_native_scope(
            NativeWriteScope::resolve_access(&root, &["allowed.txt".into()], &[]).unwrap(),
        )
        .unwrap();
    let args = serde_json::json!({"path": "allowed.txt"});
    let prepared = tool.prepare_call(&args, &config.exec_ctx).unwrap();
    let candidate = ReadCacheEntry {
        files: vec![(
            root.join("allowed.txt"),
            crate::core::persist::FileFingerprint::for_bytes(b"owned content"),
        )],
        windows: vec![None],
        msg_index: 0,
        call_id: "old-owned-read".into(),
        result_chars: 13,
        result_prefix: "owned content".into(),
        seq: 0,
    };
    assert!(
        unchanged_read_result(&candidate, &root, &args).is_some(),
        "prove this would otherwise be a cache hit"
    );
    let outcome = crate::core::exec_ctx::with_current(config.exec_ctx.clone(), || {
        run_prepared_tool_body_cached(
            tool.clone(),
            &prepared,
            true,
            config.result_budgets(),
            None,
            Some((candidate.clone(), root.clone())),
        )
    });
    assert!(outcome.failure.is_none(), "{outcome:?}");
    assert_eq!(
        outcome.observation.text, "owned content",
        "native-scoped reads must not borrow earlier-history cache evidence"
    );

    // A cached/prepared call from broader authority cannot carry private bytes into this scope.
    let outside_args = serde_json::json!({"path": "outside.txt"});
    let outside = tool.prepare_call(&outside_args, &broad).unwrap();
    let outside_candidate = ReadCacheEntry {
        files: vec![(
            root.join("outside.txt"),
            crate::core::persist::FileFingerprint::for_bytes(b"private outside"),
        )],
        ..candidate
    };
    let outcome = crate::core::exec_ctx::with_current(config.exec_ctx.clone(), || {
        run_prepared_tool_body_cached(
            tool,
            &outside,
            true,
            config.result_budgets(),
            None,
            Some((outside_candidate, root)),
        )
    });
    assert!(outcome.failure.is_some());
    assert!(!outcome.observation.text.contains("private outside"));
    assert_eq!(outcome.effect_state, tools::ToolEffectState::Settled);
}

#[test]
fn scoped_native_registry_intersects_role_grants_without_upgrading_readers() {
    use crate::core::native_scope::NativeWriteScope;

    let (_fixture, config) = placement_executor_fixture();
    let root = config.effective_root();
    let read = NativeWriteScope::resolve_access(&root, &["owned.txt".into()], &[]).unwrap();
    let write = NativeWriteScope::resolve(&root, &["owned.txt".into()]).unwrap();
    for role in ["argus", "daedalus"] {
        let mut registry = builtin::role_registry(role, &root);
        task_tool::seat_native_registry(&mut registry, &mut String::new(), &read).unwrap();
        assert_eq!(registry.names(), ["file_read"]);
    }
    let mut reader = builtin::role_registry("argus", &root);
    let before = reader.names();
    assert!(task_tool::seat_native_registry(&mut reader, &mut String::new(), &write).is_err());
    assert_eq!(
        reader.names(),
        before,
        "a rejected declaration cannot widen the role"
    );
    let mut editor = builtin::role_registry("daedalus", &root);
    task_tool::seat_native_registry(&mut editor, &mut String::new(), &write).unwrap();
    let mut names = editor.names();
    names.sort();
    assert_eq!(names, ["file_edit", "file_read", "file_write"]);
}
