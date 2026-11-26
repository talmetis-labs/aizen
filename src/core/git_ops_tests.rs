use super::*;

fn record(path: &str, state: &str) -> String {
    format!("worktree {path}\0HEAD {}\0{state}\0\0", "a".repeat(40))
}

#[test]
fn worktree_records_preserve_spaces_newlines_and_external_paths() -> Result<()> {
    let data = format!(
        "{}{}worktree /bare\0bare\0\0",
        record("/outside/project with spaces", "branch refs/heads/main"),
        record(
            "/external/with\nnewline",
            "detached\0locked reason with spaces\0prunable missing path"
        )
    );
    let entries = parse_worktrees(data.as_bytes())?;
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path, "/outside/project with spaces");
    assert_eq!(entries[0].branch.as_deref(), Some("refs/heads/main"));
    assert_eq!(entries[1].path, "/external/with\nnewline");
    assert_eq!(entries[1].locked.as_deref(), Some("reason with spaces"));
    assert!(entries[1].detached && entries[2].bare);
    Ok(())
}

#[test]
fn index_visibility_refuses_flags_that_hide_dirty_files() {
    assert!(validate_visible_index(b"").is_ok());
    assert!(validate_visible_index(b"H ordinary.txt\0H path with\nnewline\0").is_ok());
    for bytes in [
        b"h assumed.txt\0".as_slice(),
        b"S sparse.txt\0",
        b"s hidden.txt\0",
        b"M conflict.txt\0",
        b"H unterminated",
        b"H \0",
        b"\0",
    ] {
        assert!(validate_visible_index(bytes).is_err(), "accepted {bytes:?}");
    }
}

#[test]
fn worktree_records_support_unborn_and_both_object_formats() -> Result<()> {
    for size in [40, 64] {
        let data = format!(
            "worktree /unborn\0HEAD {}\0branch refs/heads/main\0\0",
            "0".repeat(size)
        );
        assert!(parse_worktrees(data.as_bytes())?[0].head.is_none());
        let data = data.replace(&"0".repeat(size), &"b".repeat(size));
        assert_eq!(
            parse_worktrees(data.as_bytes())?[0]
                .head
                .as_ref()
                .unwrap()
                .as_str()
                .len(),
            size
        );
    }
    Ok(())
}

#[test]
fn worktree_records_refuse_partial_duplicate_and_unsupported_state() {
    let good = record("/repo", "branch refs/heads/main");
    for bad in [
        String::new(),
        good.trim_end_matches('\0').into(),
        good[..good.len() - 1].into(),
        format!("{good}{good}"),
        good.replace("HEAD ", "future "),
        good.replace("branch refs/heads/main", "branch refs/heads/main\0detached"),
        good.replace("branch refs/heads/main", "bare"),
        good.replace("branch refs/heads/main", "branch refs/tags/tag"),
        good.replace(&"a".repeat(40), "1234"),
        good.replace(
            "branch refs/heads/main",
            "branch refs/heads/main\0locked\0locked",
        ),
    ] {
        assert!(parse_worktrees(bad.as_bytes()).is_err(), "accepted {bad:?}");
    }
    assert!(parse_worktrees(b"worktree /invalid\xff\0bare\0\0").is_err());
}

#[test]
fn inspection_environment_removes_redirection_without_erasing_host_config() {
    let mut command = std::process::Command::new("git");
    let forbidden = [
        "GIT_DIR",
        "git_work_tree",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_OBJECT_DIRECTORY",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_12",
        "GIT_CONFIG_VALUE_12",
        "GIT_CONFIG_PARAMETERS",
        "GIT_TRACE2_EVENT",
        "GIT_EXTERNAL_DIFF",
        "GIT_GLOB_PATHSPECS",
    ];
    for key in forbidden {
        command.env(key, "fixture");
    }
    command
        .env("HOME", "fixture-home")
        .env("GIT_CONFIG_GLOBAL", "fixture-global")
        .env("GIT_OPTIONAL_LOCKS", "1")
        .env("GIT_TERMINAL_PROMPT", "1");
    configure_inspection_environment(&mut command, forbidden.map(std::ffi::OsString::from));
    let values: std::collections::HashMap<_, _> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().to_ascii_uppercase(),
                value.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for key in forbidden {
        assert_eq!(values.get(&key.to_ascii_uppercase()), Some(&None), "{key}");
    }
    assert_eq!(values["HOME"].as_deref(), Some("fixture-home"));
    assert_eq!(
        values["GIT_CONFIG_GLOBAL"].as_deref(),
        Some("fixture-global")
    );
    assert_eq!(values["GIT_OPTIONAL_LOCKS"].as_deref(), Some("0"));
    assert_eq!(values["GIT_TERMINAL_PROMPT"].as_deref(), Some("0"));
    assert_eq!(values["GIT_NO_LAZY_FETCH"].as_deref(), Some("1"));
}

#[test]
fn incomplete_git_transport_never_becomes_an_empty_inventory() {
    use crate::core::proctree::{BoundedBytes, BoundedExecutionError, BoundedSettlement};
    let output = || BoundedBytes {
        stdout: record("/repo", "branch refs/heads/main").into_bytes(),
        stderr: Vec::new(),
        code: Some(0),
        timed_out: false,
        output_truncated: false,
        settlement: BoundedSettlement::Proven,
    };
    for kind in ["timeout", "truncated", "nonzero", "unknown"] {
        let mut failed = output();
        match kind {
            "timeout" => failed.timed_out = true,
            "truncated" => failed.output_truncated = true,
            "nonzero" => failed.code = Some(128),
            "unknown" => failed.settlement = BoundedSettlement::Unknown,
            _ => unreachable!(),
        }
        let error = checked_output(Ok(failed)).err().expect("must refuse");
        if kind == "unknown" {
            assert!(error
                .downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
                .is_some());
        }
    }
    let error = checked_output(Err(BoundedExecutionError {
        cause: std::io::Error::other("worker still running"),
        settlement: BoundedSettlement::Unknown,
    }))
    .err()
    .expect("must retain unknown settlement");
    assert!(error
        .downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
        .is_some());
    assert!(checked_output(Ok(output())).is_ok());
}

#[test]
fn git_inventory_reads_actual_external_worktrees_without_touching_parent() -> Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let root = fixture.path().join("repo");
    let home = fixture.path().join("home");
    let external = fixture.path().join("external worktree");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let Some(git) = crate::core::gitx::git_exe() else {
        anyhow::ensure!(std::env::var_os("CI").is_none(), "Git required in CI");
        eprintln!("SKIP: Git unavailable for worktree integration fixture");
        return Ok(());
    };
    let run = |args: &[&str]| -> Result<()> {
        // Fixed disposable fixture setup, never model/repository-controlled process input.
        let output = std::process::Command::new(&git)
            .current_dir(&root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "fixture Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    run(&["init", "-q"])?;
    std::fs::write(root.join("tracked.txt"), b"committed\n")?;
    run(&["add", "tracked.txt"])?;
    run(&["commit", "-qm", "base"])?;
    let destination = external.to_str().context("fixture path is not Unicode")?;
    let destination = destination.strip_prefix(r"\\?\").unwrap_or(destination);
    run(&["worktree", "add", "--detach", destination, "HEAD"])?;
    run(&[
        "worktree",
        "lock",
        "--reason",
        "external owner",
        destination,
    ])?;
    std::fs::write(root.join("tracked.txt"), b"user pending edit\n")?;
    std::fs::write(root.join("untracked.txt"), b"user addition\n")?;
    let head = std::fs::read(root.join(".git/HEAD"))?;
    let index = std::fs::read(root.join(".git/index"))?;
    crate::core::workspace_txn::with_test_lock_home(&home, || -> Result<()> {
        let entries = worktree_inventory(&root)?;
        assert_eq!(entries.len(), 2);
        let found = entries
            .iter()
            .find(|e| e.path.ends_with("external worktree"))
            .unwrap();
        assert!(found.detached);
        assert_eq!(found.locked.as_deref(), Some("external owner"));
        assert_eq!(std::fs::read(root.join(".git/HEAD"))?, head);
        assert_eq!(std::fs::read(root.join(".git/index"))?, index);
        assert_eq!(
            std::fs::read(root.join("tracked.txt"))?,
            b"user pending edit\n"
        );
        assert_eq!(
            std::fs::read(root.join("untracked.txt"))?,
            b"user addition\n"
        );
        Ok(())
    })
}
