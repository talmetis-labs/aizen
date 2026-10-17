#[test]
fn native_scope_hooks_cannot_spawn_or_widen_authority() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (fixture, mut config) = placement_executor_fixture();
    let root = config.effective_root();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let _hooks_enabled = crate::core::config::EnvGuard::unset(["AIZEN_NO_HOOKS"]);
    write_hooks(&home, true);
    assert!(hooks::configured().is_some());
    std::fs::write(root.join("allowed.txt"), "owned read").unwrap();
    std::fs::write(root.join("outside.txt"), "private outside").unwrap();
    config.exec_ctx = config
        .exec_ctx
        .with_native_scope(
            crate::core::native_scope::NativeWriteScope::resolve_access(
                &root,
                &["allowed.txt".into()],
                &[],
            )
            .unwrap(),
        )
        .unwrap()
        .with_dispatch_label(Some("argus · owned scoped fixture".into()));
    let registry = builtin::file_registry_for_test(&root);
    crate::core::exec_ctx::with_current(config.exec_ctx.clone(), || {
        let context = hooks::Context::here(true);
        assert_eq!(context.cwd, root);
        assert_eq!(context.dispatch, config.exec_ctx.dispatch_label());
        assert_eq!(context.cwd, hooks::Context::from_cfg(&config).cwd);
        assert_eq!(
            hooks::pre_tool(
                "file_read",
                &serde_json::json!({"path":"allowed.txt"}),
                &context
            ),
            hooks::PreToolVerdict::Pass,
            "an unavailable opaque hook reports an error, never manufactures an allow or deny",
        );
    });
    assert_eq!(
        execute_one_for_test(
            &registry,
            &call("allowed", "file_read", r#"{"path":"allowed.txt"}"#),
            &config,
        ),
        "owned read",
    );
    let denied = execute_one_for_test(
        &registry,
        &call("outside", "file_read", r#"{"path":"outside.txt"}"#),
        &config,
    );
    assert!(denied.starts_with("error:"), "{denied}");
    assert!(!denied.contains("private outside"));
    assert!(!root.join("hook-pre.txt").exists());
    assert!(!root.join("hook-post.txt").exists());
}
