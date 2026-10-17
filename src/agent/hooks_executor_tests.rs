use super::*;

include!("native_hooks_tests.rs");

fn write_hooks(home: &std::path::Path, deny: bool) {
    use crate::core::cli_config::{CliConfig, Hook, HooksConfig};
    let pre = if cfg!(windows) {
        format!(
            "echo %AIZEN_HOOK_TOOL%>> hook-pre.txt{}",
            if deny { " & exit 2" } else { "" }
        )
    } else {
        format!(
            "printf '%s\\n' \"$AIZEN_HOOK_TOOL\" >> hook-pre.txt{}",
            if deny { "; exit 2" } else { "" }
        )
    };
    let post = if cfg!(windows) {
        "echo %AIZEN_HOOK_TOOL%>> hook-post.txt"
    } else {
        "printf '%s\\n' \"$AIZEN_HOOK_TOOL\" >> hook-post.txt"
    };
    let hook = |run: String| Hook {
        run,
        matches: Some("file_read|file_write".into()),
        timeout_secs: Some(5),
    };
    // Fixture-owned configuration only; never touch the user's actual home/configuration.
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("cli-config.json"),
        serde_json::to_vec(&CliConfig {
            hooks: Some(HooksConfig {
                pre_tool: vec![hook(pre)],
                post_tool: vec![hook(post.into())],
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::await_holding_lock,
    reason = "Fixture HOME and hook toggles stay pinned through awaited workers and EnvGuard restoration"
)]
async fn executor_hooks_run_once_for_mixed_native_and_eager_calls_in_pinned_workspace() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, config) = placement_executor_fixture();
    let root = config.effective_root();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let _hooks_enabled = crate::core::config::EnvGuard::unset(["AIZEN_NO_HOOKS"]);
    write_hooks(&home, false);
    assert!(
        hooks::configured().is_some(),
        "fixture hooks must be enabled"
    );
    std::fs::write(root.join("read.txt"), "owned read").unwrap();
    let registry = builtin::file_registry_for_test(&root);
    let calls = [
        call("read", "file_read", r#"{"path":"read.txt"}"#),
        call(
            "write",
            "file_write",
            r#"{"path":"write.txt","content":"owned write","purpose":"project"}"#,
        ),
    ];
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        exec(&registry, &calls, &config),
    )
    .await
    .unwrap();
    assert!(
        results.iter().all(|(_, text)| !text.starts_with("error:")),
        "{results:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("write.txt")).unwrap(),
        "owned write"
    );
    for name in ["hook-pre.txt", "hook-post.txt"] {
        let text = std::fs::read_to_string(root.join(name)).unwrap();
        let mut lines: Vec<_> = text.lines().map(str::trim).collect();
        lines.sort_unstable();
        assert_eq!(lines, ["file_read", "file_write"], "{name}: {text}");
    }

    let eager_call = call("eager", "file_read", r#"{"path":"read.txt"}"#);
    let started = eager_starter(&registry, &config)(0, &eager_call).expect("owned safe eager read");
    let mut sink = vec![Message::tool_result(
        eager_call.id.clone(),
        INTERRUPTED_TOOL_PLACEHOLDER,
    )];
    let mut checkpointed = false;
    let mut writer_lease = None;
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        execute_calls(
            &registry,
            &[eager_call],
            &config,
            &mut sink,
            vec![(0, started)],
            &mut checkpointed,
            &mut writer_lease,
        ),
    )
    .await
    .unwrap();
    assert!(!results[0].1.starts_with("error:"), "{results:?}");
    for name in ["hook-pre.txt", "hook-post.txt"] {
        let text = std::fs::read_to_string(root.join(name)).unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| line.trim() == "file_read")
                .count(),
            2,
            "eager adoption must not run hooks twice: {text}"
        );
        assert_eq!(
            text.lines()
                .filter(|line| line.trim() == "file_write")
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::await_holding_lock,
    reason = "Fixture HOME and hook toggles stay pinned through awaited workers and EnvGuard restoration"
)]
async fn executor_hooks_deny_reader_and_writer_before_native_effects() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, config) = placement_executor_fixture();
    let root = config.effective_root();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let _hooks_enabled = crate::core::config::EnvGuard::unset(["AIZEN_NO_HOOKS"]);
    write_hooks(&home, true);
    assert!(
        hooks::configured().is_some(),
        "fixture hooks must be enabled"
    );
    std::fs::write(root.join("read.txt"), "private fixture content").unwrap();
    let registry = builtin::file_registry_for_test(&root);
    let calls = [
        call("read", "file_read", r#"{"path":"read.txt"}"#),
        call(
            "write",
            "file_write",
            r#"{"path":"write.txt","content":"must not land","purpose":"project"}"#,
        ),
    ];
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        exec(&registry, &calls, &config),
    )
    .await
    .unwrap();
    assert!(
        results
            .iter()
            .all(|(_, text)| text.starts_with("error: blocked by hook")),
        "{results:?}"
    );
    assert!(results
        .iter()
        .all(|(_, text)| !text.contains("private fixture content")));
    assert!(!root.join("write.txt").exists());
    assert!(!root.join("hook-post.txt").exists());
    let text = std::fs::read_to_string(root.join("hook-pre.txt")).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
}
