#[test]
fn pre_tool_early_deny_cannot_be_lost_to_incomplete_stdin() {
    let _mode = crate::sandbox::runner::tests::lock();
    with_fixture(|ctx| {
        let args = json!({ "command": "x".repeat(8 * 1024 * 1024) });
        let cfg = HooksConfig {
            pre_tool: vec![hook("exit 2", None)],
            ..Default::default()
        };
        assert!(
            matches!(
                pre_tool_with(&cfg, "shell_run", &args, ctx),
                PreToolVerdict::Deny { .. }
            ),
            "a deny hook that closes stdin must not become Pass"
        );
    });
}

#[test]
fn incomplete_hook_input_cannot_grant_allow_or_post_context() {
    let _mode = crate::sandbox::runner::tests::lock();
    let line = if cfg!(windows) {
        "echo {\"decision\":\"allow\",\"context\":\"untrusted\"}"
    } else {
        "printf '%s' '{\"decision\":\"allow\",\"context\":\"untrusted\"}'"
    };
    with_fixture(|ctx| {
        let args = json!({ "command": "x".repeat(8 * 1024 * 1024) });
        let cfg = HooksConfig {
            pre_tool: vec![hook(line, None)],
            post_tool: vec![hook(line, None)],
            ..Default::default()
        };
        assert!(
            matches!(
                pre_tool_with(&cfg, "shell_run", &args, ctx),
                PreToolVerdict::Deny { .. }
            ),
            "incomplete hook input cannot authorize the call"
        );
        assert!(post_tool_with(&cfg, "shell_run", &args, "done", true, ctx).is_none());
    });
}
