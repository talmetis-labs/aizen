use super::*;

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    git: PathBuf,
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        // Fixed disposable fixture setup; never model/repository-controlled execution.
        let output = std::process::Command::new(&self.git)
            .current_dir(&self.root)
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
        Ok(output.stdout)
    }

    fn target(&self, name: &str) -> PathBuf {
        self.home.join("worktrees").join("fixture").join(name)
    }

    fn plan(&self, name: &str) -> Result<PreparedWorktreeCreation> {
        PreparedWorktreeCreation::prepare(&self.root, &self.target(name), &format!("aizen/{name}"))
    }

    fn hook(&self, exit: u8) -> Result<()> {
        let hook = self.root.join(".git/hooks/post-checkout");
        fs::write(
            &hook,
            format!("#!/bin/sh\nprintf 'hook work\\n' > hook-result.txt\nexit {exit}\n"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(hook, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn fixture(test: impl FnOnce(&Fixture) -> Result<()>) -> Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // These exercise orchestration, not platform-specific kernel grants. Keep mode changes local
    // and restored even after an assertion panic; strict refusal has its own runner tests.
    struct RestoreMode(crate::sandbox::SandboxMode);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            crate::sandbox::set_mode(self.0);
        }
    }
    let _restore = RestoreMode(crate::sandbox::mode());
    crate::sandbox::set_mode(crate::sandbox::SandboxMode::Guarded);
    let temp = crate::core::file_placement::tests::TestDir::new();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir(&root)?;
    fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let Some(git) = crate::core::gitx::git_exe() else {
        ensure!(std::env::var_os("CI").is_none(), "Git required in CI");
        eprintln!("SKIP: Git unavailable for worktree creation fixture");
        return Ok(());
    };
    let f = Fixture { root, home, git };
    f.run(&["init", "-q"])?;
    // Production preserves configured hooks. Isolate fixtures from the developer's global hooks.
    f.run(&["config", "core.hooksPath", ".git/hooks"])?;
    fs::write(f.root.join("tracked.txt"), b"base\n")?;
    fs::write(f.root.join(".gitignore"), b"ignored.tmp\n")?;
    f.run(&["add", "tracked.txt", ".gitignore"])?;
    f.run(&["commit", "-qm", "base"])?;
    crate::core::workspace_txn::with_test_lock_home(&f.home, || test(&f))
}

#[test]
fn create_from_frozen_commit_preserves_dirty_parent_and_does_not_copy_secrets() -> Result<()> {
    fixture(|f| {
        fs::write(f.root.join("tracked.txt"), b"staged version\n")?;
        f.run(&["add", "tracked.txt"])?;
        fs::write(f.root.join("tracked.txt"), b"unstaged version\n")?;
        fs::write(f.root.join("untracked.txt"), b"private pending addition\n")?;
        fs::write(f.root.join("ignored.tmp"), b"private ignored fixture\n")?;
        let parent_head = fs::read(f.root.join(".git/HEAD"))?;
        let parent_index = fs::read(f.root.join(".git/index"))?;
        let plan = f.plan("dirty-parent")?;
        let base = plan.base.clone();
        assert!(
            !f.target("dirty-parent").exists(),
            "prepare must not create a directory"
        );
        let created = plan.execute()?;
        assert_eq!(created.base, base);
        assert!(!created.dirty);
        assert_eq!(head(&created.path, None)?, base);
        let bytes = fs::read_to_string(created.path.join("tracked.txt"))?;
        assert_eq!(bytes.replace("\r\n", "\n"), "base\n");
        assert!(!created.path.join("ignored.tmp").exists());
        assert!(!created.path.join("untracked.txt").exists());
        assert_eq!(fs::read(f.root.join(".git/HEAD"))?, parent_head);
        assert_eq!(fs::read(f.root.join(".git/index"))?, parent_index);
        assert_eq!(fs::read(f.root.join("tracked.txt"))?, b"unstaged version\n");
        assert_eq!(
            fs::read(f.root.join("untracked.txt"))?,
            b"private pending addition\n"
        );
        assert_eq!(
            fs::read(f.root.join("ignored.tmp"))?,
            b"private ignored fixture\n"
        );
        Ok(())
    })
}

#[test]
fn explicit_base_preview_is_read_only_and_ref_movement_cannot_retarget_creation() -> Result<()> {
    fixture(|f| {
        let base = head(&f.root, None)?;
        f.run(&["branch", "selected-base"])?;
        fs::write(f.root.join("tracked.txt"), b"later committed version\n")?;
        f.run(&["add", "tracked.txt"])?;
        f.run(&["commit", "-qm", "later"])?;
        let parent = head(&f.root, None)?;
        fs::write(f.root.join("tracked.txt"), b"staged parent\n")?;
        f.run(&["add", "tracked.txt"])?;
        fs::write(f.root.join("tracked.txt"), b"pending parent\n")?;
        fs::write(f.root.join("ignored.tmp"), b"private parent\n")?;
        let index = fs::read(f.root.join(".git/index"))?;
        let target = f.target("older-base");
        let plan = PreparedWorktreeCreation::prepare_from(
            &f.root,
            &target,
            "aizen/older-base",
            "selected-base",
        )?;
        let preview: serde_json::Value = serde_json::from_str(&plan.preview()?)?;
        assert_eq!(preview["base"], serde_json::to_value(&base)?);
        assert_eq!(preview["source_head"], serde_json::to_value(&parent)?);
        assert!(!target.exists());
        assert!(f
            .run(&[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/aizen/older-base"
            ])?
            .is_empty());
        f.run(&["branch", "--force", "selected-base", parent.as_str()])?;
        let created = plan.execute()?;
        assert_eq!(created.base, base);
        assert_eq!(head(&created.path, None)?, base);
        assert_eq!(
            fs::read_to_string(created.path.join("tracked.txt"))?.replace("\r\n", "\n"),
            "base\n"
        );
        assert!(!created.path.join("ignored.tmp").exists());
        assert_eq!(head(&f.root, None)?, parent);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(fs::read(f.root.join("tracked.txt"))?, b"pending parent\n");
        assert_eq!(fs::read(f.root.join("ignored.tmp"))?, b"private parent\n");
        Ok(())
    })
}

#[test]
fn explicit_base_rejects_noncommits_legacy_ids_and_source_drift() -> Result<()> {
    fixture(|f| {
        let base = head(&f.root, None)?;
        let blob = String::from_utf8(f.run(&["rev-parse", "HEAD:tracked.txt"])?)?;
        for revision in ["42", "#42", "--all", "missing-ref", blob.trim()] {
            assert!(
                PreparedWorktreeCreation::prepare_from(
                    &f.root,
                    &f.target("invalid-base"),
                    "aizen/invalid-base",
                    revision,
                )
                .is_err(),
                "accepted {revision:?}"
            );
            assert!(!f.target("invalid-base").exists());
        }
        let plan = PreparedWorktreeCreation::prepare_from(
            &f.root,
            &f.target("stale-source"),
            "aizen/stale-source",
            base.as_str(),
        )?;
        f.run(&["commit", "--allow-empty", "-qm", "source moved"])?;
        assert!(plan
            .execute()
            .unwrap_err()
            .to_string()
            .contains("source HEAD moved"));
        assert!(!f.target("stale-source").exists());
        fs::write(
            f.root.join(".git/info/grafts"),
            format!("{}\n", base.as_str()),
        )?;
        assert!(PreparedWorktreeCreation::prepare_from(
            &f.root,
            &f.target("grafted"),
            "aizen/grafted",
            base.as_str(),
        )
        .unwrap_err()
        .to_string()
        .contains("unmodified commit ancestry"));
        assert!(!f.target("grafted").exists());
        Ok(())
    })
}

#[test]
fn creation_refuses_moved_head_and_existing_branch_or_destination() -> Result<()> {
    fixture(|f| {
        let plan = f.plan("moved-head")?;
        f.run(&["commit", "--allow-empty", "-qm", "new base"])?;
        let error = plan.execute().unwrap_err();
        assert!(format!("{error:#}").contains("HEAD moved"));
        assert!(!f.target("moved-head").exists());
        assert!(f
            .run(&[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/aizen/moved-head"
            ])?
            .is_empty());
        f.run(&["branch", "aizen/existing"])?;
        assert!(f.plan("existing").is_err());
        assert!(!f.target("existing").exists());
        let plan = f.plan("existing-dir")?;
        fs::create_dir_all(f.target("existing-dir"))?;
        fs::write(f.target("existing-dir").join("user.txt"), b"preserve")?;
        assert!(plan.execute().is_err());
        assert_eq!(
            fs::read(f.target("existing-dir").join("user.txt"))?,
            b"preserve"
        );
        for invalid in ["bad..name", "trailing.", "trailing.lock"] {
            assert!(f.plan(invalid).is_err(), "accepted {invalid}");
        }
        assert!(
            PreparedWorktreeCreation::prepare(&f.root, &f.target("option"), "--option").is_err()
        );
        let plan = f.plan("branch-moved")?;
        f.run(&["checkout", "-qb", "different-branch-same-commit"])?;
        let error = plan.execute().unwrap_err();
        assert!(format!("{error:#}").contains("source branch changed"));
        assert!(!f.target("branch-moved").exists());
        Ok(())
    })
}

#[test]
fn creation_honors_hooks_and_reports_dirty_or_failed_partial_results() -> Result<()> {
    fixture(|f| {
        // Absolute configured hooks work across linked worktrees; no mutation suppresses them.
        let hooks = git_path(&f.root.join(".git/hooks"))?;
        f.run(&["config", "core.hooksPath", &hooks])?;
        f.hook(0)?;
        let created = f.plan("hook-dirty")?.execute()?;
        assert!(created.dirty);
        assert_eq!(
            fs::read(created.path.join("hook-result.txt"))?,
            b"hook work\n"
        );
        f.hook(7)?;
        let error = f.plan("hook-failed")?.execute().unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("no automatic cleanup"), "{message}");
        assert!(message.contains("Git command failed"), "{message}");
        assert_eq!(
            fs::read(f.target("hook-failed").join("hook-result.txt"))?,
            b"hook work\n"
        );
        assert!(!f
            .run(&[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/aizen/hook-failed"
            ])?
            .is_empty());
        assert!(
            f.plan("hook-failed").is_err(),
            "partial work must not be silently reused"
        );
        fs::write(f.root.join(".git/hooks/post-checkout"),
            "#!/bin/sh\nprintf 'unfinished fixture\\n' > \"$(git rev-parse --git-path REVERT_HEAD)\"\n")?;
        let error = f.plan("hook-operation")?.execute().unwrap_err();
        assert!(format!("{error:#}").contains("active Git operation state"));
        assert!(f.target("hook-operation").join("tracked.txt").exists());
        Ok(())
    })
}

#[test]
fn creation_supports_detached_source_and_refuses_non_git_and_unborn() -> Result<()> {
    fixture(|f| {
        f.run(&["checkout", "--detach", "-q"])?;
        let created = f.plan("from-detached")?.execute()?;
        assert_eq!(head(&created.path, None)?, head(&f.root, None)?);
        let empty = f.home.join("empty");
        fs::create_dir(&empty)?;
        let destination = f.target("unborn");
        assert!(PreparedWorktreeCreation::prepare(&empty, &destination, "aizen/unborn").is_err());
        let output = std::process::Command::new(&f.git)
            .current_dir(&empty)
            .args(["init", "-q"])
            .output()?;
        assert!(output.status.success());
        assert!(PreparedWorktreeCreation::prepare(&empty, &destination, "aizen/unborn").is_err());
        assert!(!destination.exists());
        Ok(())
    })
}

#[test]
fn creation_from_linked_source_preserves_source_index_and_uses_shared_history() -> Result<()> {
    fixture(|f| {
        let first = f.plan("first")?.execute()?;
        fs::write(first.path.join("tracked.txt"), b"pending linked edit\n")?;
        let identity = workspace_identity(&first.path)?;
        let git_dir = identity.canonical_git_dir.as_ref().unwrap();
        let index = fs::read(git_dir.join("index"))?;
        let head_bytes = fs::read(git_dir.join("HEAD"))?;
        let second =
            PreparedWorktreeCreation::prepare(&first.path, &f.target("second"), "aizen/second")?
                .execute()?;
        assert_eq!(second.base, first.base);
        assert_eq!(fs::read(git_dir.join("index"))?, index);
        assert_eq!(fs::read(git_dir.join("HEAD"))?, head_bytes);
        assert_eq!(
            fs::read(first.path.join("tracked.txt"))?,
            b"pending linked edit\n"
        );
        assert_eq!(
            workspace_identity(&second.path)?.canonical_common_git_dir,
            identity.canonical_common_git_dir
        );
        Ok(())
    })
}

#[test]
fn creation_refuses_missing_registered_destination_without_pruning_or_adopting_it() -> Result<()> {
    fixture(|f| {
        let first = f.plan("stale")?.execute()?;
        let saved = first.path.with_file_name("retained-checkout");
        fs::rename(&first.path, &saved)?;
        let error = PreparedWorktreeCreation::prepare(&f.root, &first.path, "aizen/other-branch")
            .unwrap_err();
        assert!(format!("{error:#}").contains("registered worktree"));
        assert!(saved.join("tracked.txt").exists());
        assert!(!first.path.exists());
        let entries = parse_worktrees(&query(
            &f.root,
            &["worktree", "list", "--porcelain", "-z"],
            None,
        )?)?;
        assert_eq!(entries.len(), 2, "stale registration must not be pruned");
        Ok(())
    })
}

#[test]
fn creation_refuses_replaced_destination_ancestor_without_touching_replacement() -> Result<()> {
    fixture(|f| {
        let parent = f.target("replaced").parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent)?;
        let plan = f.plan("replaced")?;
        let saved = parent.with_file_name("saved-parent");
        fs::rename(&parent, &saved)?;
        fs::create_dir(&parent)?;
        fs::write(parent.join("user.txt"), b"preserve replacement")?;
        assert!(plan.execute().is_err());
        assert_eq!(fs::read(parent.join("user.txt"))?, b"preserve replacement");
        assert!(!f.target("replaced").exists());
        Ok(())
    })
}

#[test]
fn owned_creation_refusal_reports_retained_identity_without_retry_or_adoption() -> Result<()> {
    fixture(|f| {
        let hooks = git_path(&f.root.join(".git/hooks"))?;
        f.run(&["config", "core.hooksPath", &hooks])?;
        f.hook(0)?;
        let before_head = head(&f.root, None)?;
        let before_index = fs::read(f.root.join(".git/index"))?;
        let before_bytes = fs::read(f.root.join("tracked.txt"))?;
        let target = f.target("owned-hook-dirty");
        let error = f
            .plan("owned-hook-dirty")?
            .execute_for_task("parent/owned-hook-dirty")
            .err()
            .context("dirty checkout must not return task authority")?;
        let message = format!("{error:#}");
        assert!(
            message.contains(&fs::canonicalize(&target)?.display().to_string()),
            "missing retained root: {message}"
        );
        assert!(
            message.contains("aizen/owned-hook-dirty"),
            "missing retained branch: {message}"
        );
        assert!(
            message.contains(before_head.as_str()),
            "missing frozen base: {message}"
        );
        assert!(
            message.contains("no task authority was returned"),
            "{message}"
        );
        assert!(
            message.contains("no automatic retry, adoption or cleanup"),
            "{message}"
        );
        assert!(
            message.contains("checkout hook left changes"),
            "underlying refusal was lost: {message}"
        );
        assert_eq!(fs::read(target.join("hook-result.txt"))?, b"hook work\n");
        assert_eq!(head(&target, None)?, before_head);
        assert_eq!(head(&f.root, None)?, before_head);
        assert_eq!(fs::read(f.root.join(".git/index"))?, before_index);
        assert_eq!(fs::read(f.root.join("tracked.txt"))?, before_bytes);
        assert!(
            f.plan("owned-hook-dirty").is_err(),
            "retained worktree must not be silently adopted"
        );
        Ok(())
    })
}
