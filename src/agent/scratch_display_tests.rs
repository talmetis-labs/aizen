use super::*;

#[test]
fn prepared_scratch_display_binds_relative_reads_without_rewriting_raw_data() {
    use crate::core::file_placement::PlacementContext;

    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (fixture, mut config) = placement_executor_fixture();
    let root = config.effective_root();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()),
        ("AIZEN_NO_HOOKS", std::ffi::OsStr::new("1")),
    ]);
    config.exec_ctx = config.exec_ctx.with_placement(
        PlacementContext::with_paths(root.clone(), home, root.join("process"), "display-test")
            .unwrap(),
    );
    let registry = builtin::file_registry_for_test(&root);
    let label = "báo cáo/結果 e\u{301} 👩‍💻.txt";
    let write_args =
        serde_json::json!({"path": label, "content": "owned content", "purpose": "scratch"});
    let write = registry
        .get_arc("file_write")
        .unwrap()
        .prepare_call(&write_args, &config.exec_ctx)
        .unwrap();
    let actual = std::path::PathBuf::from(write.args["path"].as_str().unwrap());
    assert!(
        !actual.exists(),
        "preparation must not create scratch files"
    );
    let output = execute_one_for_test(
        &registry,
        &call("write", "file_write", &write_args.to_string()),
        &config,
    );
    assert!(output.contains(&actual.display().to_string()), "{output}");
    assert_eq!(
        prepared_result_summary("file_write", &write, &output),
        (true, "created".into())
    );
    assert_eq!(
        write_args["path"], label,
        "the original request is unchanged"
    );

    let relative = actual.strip_prefix(&root).unwrap().to_str().unwrap();
    let read_args = serde_json::json!({"path": relative});
    let read = registry
        .get_arc("file_read")
        .unwrap()
        .prepare_call(&read_args, &config.exec_ctx)
        .unwrap();
    assert_eq!(
        read.args, read_args,
        "read arguments retain their original spelling"
    );
    let display = prepared_display_target("file_read", &read);
    assert_eq!(display.text, format!("scratch/{label}"));
    assert_eq!(display.paths.len(), 1);
    let (range, path) = &display.paths[0];
    assert_eq!(&display.text[range.clone()], path.label());
    assert_eq!(path.actual(), actual);
    assert!(
        !path.matches_argument(std::path::Path::new(path.label())),
        "display labels are not execution aliases"
    );
    assert_eq!(
        execute_one_for_test(
            &registry,
            &call("read", "file_read", &read_args.to_string()),
            &config
        ),
        "owned content",
    );

    std::fs::write(root.join("ordinary.txt"), "ordinary").unwrap();
    let batch_args = serde_json::json!({"files": [
        {"path": "ordinary.txt"}, {"path": relative}, {"path": actual},
    ]});
    let batch = registry
        .get_arc("file_read")
        .unwrap()
        .prepare_call(&batch_args, &config.exec_ctx)
        .unwrap();
    let display = prepared_display_target("file_read", &batch);
    assert_eq!(batch.args, batch_args);
    assert_eq!(display.paths.len(), 2);
    for (range, path) in &display.paths {
        assert_eq!(&display.text[range.clone()], format!("scratch/{label}"));
        assert_eq!(path.actual(), actual);
    }
    assert!(display.text.starts_with("ordinary.txt, "));
    let output = execute_one_for_test(
        &registry,
        &call("batch", "file_read", &batch_args.to_string()),
        &config,
    );
    assert!(
        output.contains(relative) && output.contains(&actual.display().to_string()),
        "raw batch headers must retain real arguments: {output}"
    );
}

#[test]
fn prepared_scratch_summaries_keep_errors_and_ordinary_results_intact() {
    let (_fixture, config) = placement_executor_fixture();
    let placement = config.exec_ctx.placement().unwrap();
    let path = placement.scratch_root().join("report with spaces.txt");
    let mut prepared = tools::PreparedToolCall::new(serde_json::json!({"path": path}));
    prepared
        .display_paths
        .push(placement.scratch_display(&path).unwrap());
    for (tool, prefix, expected) in [
        ("file_write", "created", "created"),
        ("file_write", "overwrote", "overwrote"),
        ("file_edit", "created", "created"),
        ("file_edit", "inserted", "inserted"),
        ("file_edit", "replaced", "replaced"),
        ("file_move", "moved", "moved"),
    ] {
        let output = format!("{prefix} {} (1 line(s))\nraw diagnostic", path.display());
        let original = output.clone();
        assert_eq!(
            prepared_result_summary(tool, &prepared, &output),
            (true, expected.into())
        );
        assert_eq!(output, original, "UI summaries must not mutate raw results");
    }
    assert_eq!(
        prepared_result_summary("file_write", &prepared, builtin::NOOP_WRITE_PREFIX),
        (true, "unchanged".into()),
    );
    for output in [
        "error: missing old_string",
        "error: preparation changed; re-prepare",
    ] {
        assert_eq!(
            prepared_result_summary("file_edit", &prepared, output),
            summarize_result("file_edit", output)
        );
    }
    let ordinary = tools::PreparedToolCall::new(serde_json::json!({"path": "src/ordinary.rs"}));
    for tool in ["file_write", "file_edit", "file_move"] {
        let output = "created src/ordinary.rs";
        assert_eq!(
            prepared_result_summary(tool, &ordinary, output),
            summarize_result(tool, output)
        );
    }
}
