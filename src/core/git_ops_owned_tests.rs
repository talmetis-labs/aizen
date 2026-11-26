use super::*;
use crate::core::{
    convo::ConversationId,
    exec_ctx::{self, ExecutionContext},
    git_ops::{Change, PreparedChange},
    persist::FileFingerprint,
};

/// Own the real native lifecycle and its executable context, never a fabricated release token.
struct FixtureAttempt {
    baseline: Vec<(String, FileFingerprint)>,
    ctx: ExecutionContext,
    owner: crate::agent::git_native_attempt::OwnedNativeAttemptOwner,
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    git: PathBuf,
    runtime: tokio::runtime::Runtime,
    gate: crate::agent::task_tool::SubagentGate,
}
impl Fixture {
    fn git_at(&self, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
        // Fixed test-only commands, always in a disposable repository.
        let output = std::process::Command::new(&self.git)
            .current_dir(root)
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "fixture Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output.stdout)
    }
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.git_at(&self.root, args)
    }
    fn task(&self) -> Result<Arc<OwnedTask>> {
        Ok(Arc::new(
            PreparedOwnedTask::prepare(&self.root, "fixture-parent")?.execute()?,
        ))
    }
    fn begin(&self, task: &Arc<OwnedTask>, scope: &NativeWriteScope) -> Result<FixtureAttempt> {
        // Capture clean bytes before admission. No physical lease crosses runner admission.
        let baseline = task.capture_baseline(scope)?;
        let attempt = task.begin_native_attempt(scope)?;
        let child = exec_ctx::current()
            .context("fixture parent context missing")?
            .for_owned_git_task(task.clone(), scope.clone())?;
        let slot = self
            .gate
            .try_acquire()
            .map_err(|error| anyhow::anyhow!("fixture native slot: {error:?}"))?;
        let owner = crate::agent::git_native_attempt::OwnedNativeAttemptOwner::admit(
            slot, attempt, &child,
        )?;
        let ctx = owner.bind_context(&child)?;
        drop(child);
        Ok(FixtureAttempt {
            baseline,
            ctx,
            owner,
        })
    }

    fn invoke(&self, attempt: &FixtureAttempt, name: &str, args: serde_json::Value) -> Result<()> {
        let root = attempt
            .ctx
            .placement()
            .context("fixture native placement missing")?
            .workspace_root();
        let tool = crate::agent::builtin::file_registry_for_test(root)
            .get_arc(name)
            .context("fixture native tool missing")?;
        let prepared = exec_ctx::with_current(attempt.ctx.clone(), || {
            tool.prepare_checked(&args, &attempt.ctx)
        })?;
        let body_prepared = prepared.clone();
        let body_ctx = attempt.ctx.clone();
        // The real worker owner publishes the frozen intent before starting this closure.
        let observation = self.runtime.block_on(async {
            crate::agent::tools::workers::launch_prepared(
                attempt.ctx.tool_workers(),
                &prepared,
                move || {
                    exec_ctx::with_current(body_ctx, || {
                        crate::agent::execute_owned_native_fixture_body(tool, &body_prepared)
                    })
                },
                crate::agent::tools::ToolEffectState::PossiblyApplied(None),
            )?
            .await
            .context("fixture lost actual native body observation")
        })?;
        ensure!(
            observation.outcome.effect_state == crate::agent::tools::ToolEffectState::Settled,
            "fixture native body effects are not settled: {:?}",
            observation.outcome.effect_state
        );
        ensure!(
            observation.outcome.failure.is_none(),
            "fixture native body failed: {}",
            observation.outcome.text
        );
        Ok(())
    }

    fn write(&self, attempt: &FixtureAttempt, name: &str, bytes: &[u8]) -> Result<()> {
        let content = std::str::from_utf8(bytes)?;
        let root = attempt.ctx.placement().unwrap().workspace_root();
        if root.join(name).exists() {
            // Existing overwrites must have a real current read, not fabricated ledger evidence.
            self.invoke(attempt, "file_read", serde_json::json!({"path": name}))?;
        }
        self.invoke(
            attempt,
            "file_write",
            serde_json::json!({"path": name, "content": content, "purpose": "project"}),
        )
    }

    fn finish(
        &self,
        task: Arc<OwnedTask>,
        scope: &NativeWriteScope,
        attempt: FixtureAttempt,
    ) -> Result<TaskHandoff> {
        let FixtureAttempt {
            baseline,
            ctx,
            owner,
        } = attempt;
        // Tool observations alone are not settlement. Retire every executable carrier, then
        // join actual bodies and consume the private release returned by the production owner.
        drop(ctx);
        let released = self.runtime.block_on(owner.finish_with_driver(
            tokio::time::Instant::now() + Duration::from_secs(30),
            crate::core::speculation::workers::DriverResult::Completed,
        ))?;
        task.finish_native(scope, &baseline, released)
    }

    fn hook(&self, task: &OwnedTask, name: &str, body: &str) -> Result<()> {
        let hook_dir = PathBuf::from(String::from_utf8(
            self.git_at(task.root(), &["rev-parse", "--git-path", "hooks"])?
                .trim_ascii()
                .to_vec(),
        )?);
        let hook_dir = if hook_dir.is_absolute() {
            hook_dir
        } else {
            task.root().join(hook_dir)
        };
        std::fs::create_dir_all(&hook_dir)?;
        let hook = hook_dir.join(name);
        std::fs::write(&hook, format!("#!/bin/sh\n{body}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))?;
        }
        let hooks = hook_config_path(&hook_dir);
        self.git_at(task.root(), &["config", "core.hooksPath", &hooks])?;
        Ok(())
    }
}
fn hook_config_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        // Git for Windows cannot execute hooks through Rust's verbatim prefix.
        if let Some(unc) = path.strip_prefix("//?/UNC/") {
            return format!("//{unc}");
        }
        if let Some(disk) = path.strip_prefix("//?/") {
            return disk.to_owned();
        }
    }
    path
}

fn fixture(test: impl FnOnce(&Fixture) -> Result<()>) -> Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct Restore(crate::sandbox::SandboxMode);
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::sandbox::set_mode(self.0);
        }
    }
    let _restore = Restore(crate::sandbox::mode());
    crate::sandbox::set_mode(crate::sandbox::SandboxMode::Guarded);
    let dir = crate::core::file_placement::tests::TestDir::new();
    let root = dir.path().join("repo");
    let home = dir.path().join("home");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let git = crate::core::gitx::git_exe().context("Git required for owned task fixtures")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let active = Box::leak(Box::new(std::sync::atomic::AtomicUsize::new(0)));
    let gate =
        crate::agent::task_tool::SubagentGate::isolated(active, 2, home.join("native-slots"));
    let f = Fixture {
        root,
        home,
        git,
        runtime,
        gate,
    };
    f.git(&["init", "-q"])?;
    f.git(&["config", "user.name", "Fixture"])?;
    f.git(&["config", "user.email", "fixture@example.invalid"])?;
    f.git(&["config", "commit.gpgsign", "false"])?;
    f.git(&["config", "core.autocrlf", "false"])?;
    // Shared hooks must resolve independently of a linked worktree's .git file.
    let hooks = f.root.join(".git").join("hooks");
    let hooks = hook_config_path(&hooks);
    f.git(&["config", "core.hooksPath", &hooks])?;
    std::fs::write(f.root.join("tracked.txt"), b"base\n")?;
    std::fs::write(f.root.join(".gitignore"), b"ignored.tmp\n")?;
    f.git(&["add", "tracked.txt", ".gitignore"])?;
    f.git(&["commit", "-qm", "base"])?;
    crate::core::workspace_txn::with_test_lock_home(&f.home, || {
        exec_ctx::with_current(
            ExecutionContext::new(ConversationId::new("fixture-parent")).with_test_lock_namespace(
                crate::core::workspace_txn::TestLockNamespace::fixture(&f.home),
            ),
            || test(&f),
        )
    })
}

#[test]
fn owned_task_explicit_base_preserves_dirty_parent_and_binds_registration() -> Result<()> {
    fixture(|f| {
        let selected_base = head(&f.root, None)?;
        std::fs::write(f.root.join("tracked.txt"), b"later committed parent\n")?;
        f.git(&["add", "--", "tracked.txt"])?;
        f.git(&["commit", "-qm", "later parent"])?;
        let source_head = head(&f.root, None)?;
        assert_ne!(source_head, selected_base);
        std::fs::write(f.root.join("tracked.txt"), b"staged parent\n")?;
        f.git(&["add", "--", "tracked.txt"])?;
        std::fs::write(f.root.join("tracked.txt"), b"unstaged parent\n")?;
        std::fs::write(f.root.join("ignored.tmp"), b"private\n")?;
        let index = std::fs::read(f.root.join(".git/index"))?;
        let inventory = f.git(&["worktree", "list", "--porcelain", "-z"])?;
        let prepared = PreparedOwnedTask::prepare_from(&f.root, "fixture-parent", &selected_base)?;
        let preview = prepared.preview()?;
        assert!(preview.contains(selected_base.as_str()));
        assert!(preview.contains(source_head.as_str()));
        assert_eq!(
            f.git(&["worktree", "list", "--porcelain", "-z"])?,
            inventory
        );
        let task = Arc::new(prepared.execute()?);
        assert_eq!(task.base, selected_base);
        assert_eq!(std::fs::read(task.root().join("tracked.txt"))?, b"base\n");
        assert!(!task.root().join("ignored.tmp").exists());
        assert!(task.registration.handoff_receipt().is_err());
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        let handoff = f.finish(task.clone(), &scope, attempt)?;
        assert_eq!(handoff.base, selected_base);
        assert_eq!(handoff.tip, selected_base);
        assert_eq!(handoff.repo_id, task.identity.repo_id);
        assert_eq!(handoff.worktree_id, task.identity.worktree_id);
        assert_eq!(handoff.owner, task.scope());
        assert_eq!(handoff.registration.owner, handoff.owner);
        assert_eq!(handoff.registration, task.registration.handoff_receipt()?);
        assert!(handoff.commits.is_empty() && handoff.paths.is_empty());
        let manifest_path = f
            .home
            .join("coop")
            .join(&handoff.repo_id)
            .join("sessions")
            .join(format!("{}.json", handoff.registration.registration_id));
        let bytes = std::fs::read(&manifest_path)?;
        assert_eq!(
            handoff.registration.manifest_sha256,
            FileFingerprint::for_bytes(&bytes).sha256
        );
        let manifest: crate::features::coop::SessionManifest = serde_json::from_slice(&bytes)?;
        assert_eq!(manifest.repo_id, handoff.repo_id);
        assert_eq!(manifest.worktree_id, handoff.worktree_id);
        assert_eq!(manifest.branch.as_deref(), Some(handoff.branch.as_str()));
        let registered = manifest.git_task.context("missing registered task")?;
        assert_eq!(registered.owner, handoff.owner);
        assert_eq!(registered.base, selected_base);
        assert_eq!(registered.tip, selected_base);
        assert_eq!(registered.phase, "handed_off");
        assert!(!registered.verified);
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes)?;
        changed["git_task"]["owner"] = serde_json::json!("other-owner");
        crate::core::persist::atomic_write_owner_only_checked(
            &manifest_path,
            &serde_json::to_vec_pretty(&changed)?,
        )?;
        assert!(task.registration.handoff_receipt().is_err());
        assert_eq!(head(&f.root, None)?, source_head);
        assert_eq!(std::fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(
            std::fs::read(f.root.join("tracked.txt"))?,
            b"unstaged parent\n"
        );
        assert_eq!(std::fs::read(f.root.join("ignored.tmp"))?, b"private\n");
        Ok(())
    })
}

#[test]
fn owned_task_explicit_base_refuses_source_drift_and_invalid_owner_before_creation() -> Result<()> {
    fixture(|f| {
        let base = head(&f.root, None)?;
        for owner in ["", " ", "parent\nchild"] {
            assert!(PreparedOwnedTask::prepare_from(&f.root, owner, &base).is_err());
        }
        let absent = GitOid::parse(&"1".repeat(base.as_str().len()))?;
        assert!(PreparedOwnedTask::prepare_from(&f.root, "fixture-parent", &absent).is_err());
        let prepared = PreparedOwnedTask::prepare_from(&f.root, "fixture-parent", &base)?;
        f.git(&["commit", "--allow-empty", "-qm", "moved source"])?;
        let moved = head(&f.root, None)?;
        let index = std::fs::read(f.root.join(".git/index"))?;
        let inventory = f.git(&["worktree", "list", "--porcelain", "-z"])?;
        let refs = f.git(&["show-ref"])?;
        let error = prepared.execute().unwrap_err();
        assert!(error.to_string().contains("source HEAD moved"), "{error:#}");
        assert_eq!(head(&f.root, None)?, moved);
        assert_eq!(
            f.git(&["worktree", "list", "--porcelain", "-z"])?,
            inventory
        );
        assert_eq!(f.git(&["show-ref"])?, refs);
        assert_eq!(std::fs::read(f.root.join(".git/index"))?, index);
        assert!(!f.home.join("coop").exists());
        Ok(())
    })
}

#[test]
fn owned_task_commits_only_proven_child_paths_and_preserves_dirty_parent() -> Result<()> {
    fixture(|f| {
        std::fs::write(f.root.join("tracked.txt"), b"staged parent\n")?;
        f.git(&["add", "tracked.txt"])?;
        std::fs::write(f.root.join("tracked.txt"), b"unstaged parent\n")?;
        std::fs::write(f.root.join("ignored.tmp"), b"private\n")?;
        let index = std::fs::read(f.root.join(".git/index"))?;
        let head_before = f.git(&["rev-parse", "HEAD"])?;
        let task = f.task()?;
        let scope =
            NativeWriteScope::resolve(task.root(), &["tracked.txt".into(), "new file.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child change\n")?;
        f.write(&attempt, "new file.txt", b"new child\n")?;
        let handoff = f.finish(task.clone(), &scope, attempt)?;
        assert_eq!(handoff.commits.len(), 1);
        assert_eq!(handoff.paths, vec!["new file.txt", "tracked.txt"]);
        assert!(!handoff.verified && !handoff.integrated && !handoff.dirty);
        assert_eq!(std::fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, head_before);
        assert_eq!(
            std::fs::read(f.root.join("tracked.txt"))?,
            b"unstaged parent\n"
        );
        assert!(!f.root.join("new file.txt").exists());
        assert!(!handoff.root.join("ignored.tmp").exists());
        drop(task);
        let manifests = crate::features::coop::list();
        // Current project-root may differ in this fixture; durable data is separately checked by admission below.
        drop(manifests);
        crate::features::coop::require_no_other_workspace_owner(&inspect_identity(
            &handoff.root,
            None,
        )?)?;
        Ok(())
    })
}

#[test]
fn owned_task_refuses_unattributed_and_carried_in_changes() -> Result<()> {
    // Each finalization consumes its release proof, even when Git refuses the candidate.
    // Exercise both provenance failures independently, never retry an already-finished attempt.
    for append_child in [false, true] {
        fixture(|f| {
            let task = f.task()?;
            let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
            let attempt = f.begin(&task, &scope)?;
            std::fs::write(task.root().join("tracked.txt"), b"external\n")?;
            if append_child {
                f.write(&attempt, "tracked.txt", b"external plus child\n")?;
                assert!(f
                    .finish(task.clone(), &scope, attempt)
                    .unwrap_err()
                    .to_string()
                    .contains("clean baseline"));
            } else {
                assert!(f.finish(task.clone(), &scope, attempt).is_err());
            }
            assert_eq!(head(task.root(), Some(&task.owner))?, task.base);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn owned_task_refuses_preexisting_staging_and_retains_abandoned_owner() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let root = task.root().to_path_buf();
        let scope = NativeWriteScope::resolve(&root, &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child\n")?;
        f.git_at(&root, &["add", "tracked.txt"])?;
        assert!(f.finish(task, &scope, attempt).is_err());
        let identity = inspect_identity(&root, None)?;
        let error = crate::features::coop::require_no_other_workspace_owner(&identity).unwrap_err();
        assert!(error.to_string().contains("unfinished owned Git task"));
        assert!(!f
            .git_at(&root, &["diff", "--cached", "--name-only"])?
            .is_empty());
        Ok(())
    })
}

#[test]
fn owned_task_honors_failed_commit_hook() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let root = task.root().to_path_buf();
        let scope = NativeWriteScope::resolve(&root, &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child\n")?;
        f.hook(
            &task,
            "prepare-commit-msg",
            "printf 'hook surprise\\n' > hook.txt\nexit 9",
        )?;
        assert!(f.finish(task.clone(), &scope, attempt).is_err());
        assert_eq!(head(&root, Some(&task.owner))?, task.base);
        assert!(root.join("hook.txt").exists());
        assert!(!f
            .git_at(&root, &["diff", "--cached", "--name-only"])?
            .is_empty());
        drop(task);
        assert!(
            crate::features::coop::require_no_other_workspace_owner(&inspect_identity(
                &root, None
            )?)
            .is_err()
        );
        Ok(())
    })
}

#[test]
fn owned_task_refuses_tree_changing_commit_hook() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let root = task.root().to_path_buf();
        let base = task.base.clone();
        let scope = NativeWriteScope::resolve(&root, &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child\n")?;
        // pre-commit runs before Git freezes the commit tree.
        f.hook(
            &task,
            "pre-commit",
            "printf 'hook surprise\\n' > hook.txt\ngit add -- hook.txt",
        )?;
        let error = f.finish(task, &scope, attempt).unwrap_err();
        assert!(
            error.to_string().contains("hook changed tree/parents"),
            "{error:#}"
        );
        assert_ne!(head(&root, None)?, base);
        assert_eq!(
            f.git_at(&root, &["show", "HEAD:hook.txt"])?,
            b"hook surprise\n"
        );
        assert!(
            crate::features::coop::require_no_other_workspace_owner(&inspect_identity(
                &root, None
            )?)
            .is_err()
        );
        Ok(())
    })
}

#[test]
fn owned_task_refuses_commit_hook_that_dirties_worktree() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let root = task.root().to_path_buf();
        let base = task.base.clone();
        let scope = NativeWriteScope::resolve(&root, &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child\n")?;
        // prepare-commit-msg runs after Git freezes the tree; staging here dirties the index.
        f.hook(
            &task,
            "prepare-commit-msg",
            "printf 'hook surprise\\n' > hook.txt\ngit add -- hook.txt",
        )?;
        let error = f.finish(task, &scope, attempt).unwrap_err();
        assert!(
            error.to_string().contains("worktree/index must be clean"),
            "{error:#}"
        );
        assert_ne!(head(&root, None)?, base);
        assert!(root.join("hook.txt").exists());
        assert_eq!(
            f.git_at(&root, &["ls-tree", "--name-only", "HEAD", "--", "hook.txt"])?,
            b""
        );
        assert_eq!(
            f.git_at(&root, &["diff", "--cached", "--name-only"])?,
            b"hook.txt\n"
        );
        assert!(
            crate::features::coop::require_no_other_workspace_owner(&inspect_identity(
                &root, None
            )?)
            .is_err()
        );
        Ok(())
    })
}

#[test]
fn owned_allocator_retains_shared_owner_for_two_exact_roots_and_pending_parent() -> Result<()> {
    fixture(|f| {
        std::fs::write(f.root.join("tracked.txt"), b"staged parent\n")?;
        f.git(&["add", "--", "tracked.txt"])?;
        std::fs::write(f.root.join("tracked.txt"), b"unstaged parent\n")?;
        std::fs::write(f.root.join("ignored.tmp"), b"private\n")?;
        let index = std::fs::read(f.root.join(".git/index"))?;
        let base = head(&f.root, None)?;
        let parent_cancel = crate::core::cancel::TurnCancel::new();
        let allocator = OwnedTaskAllocator::new(&parent_cancel);
        let first =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let second =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        assert!(Arc::ptr_eq(&first.owner, &second.owner));
        assert_ne!(first.root(), second.root());
        assert_ne!(first.scope(), second.scope());
        let first_scope = NativeWriteScope::resolve(first.root(), &["tracked.txt".into()])?;
        let second_scope = NativeWriteScope::resolve(second.root(), &["tracked.txt".into()])?;
        assert_ne!(first_scope.write_keys(), second_scope.write_keys());
        let first_attempt = f.begin(&first, &first_scope)?;
        let second_attempt = f.begin(&second, &second_scope)?;
        assert_eq!(std::fs::read(first.root().join("tracked.txt"))?, b"base\n");
        assert_eq!(std::fs::read(second.root().join("tracked.txt"))?, b"base\n");
        f.write(&first_attempt, "tracked.txt", b"first\n")?;
        f.write(&second_attempt, "tracked.txt", b"second\n")?;
        let a = f.finish(first.clone(), &first_scope, first_attempt)?;
        let b = f.finish(second.clone(), &second_scope, second_attempt)?;
        assert_eq!(a.base, base);
        assert_eq!(b.base, base);
        assert_ne!(a.tip, b.tip);
        assert!(!a.verified && !a.integrated && !b.verified && !b.integrated);
        assert_eq!(std::fs::read(first.root().join("tracked.txt"))?, b"first\n");
        assert_eq!(
            std::fs::read(second.root().join("tracked.txt"))?,
            b"second\n"
        );
        assert!(!first.root().join("ignored.tmp").exists());
        assert!(!second.root().join("ignored.tmp").exists());
        assert_eq!(
            std::fs::read(f.root.join("tracked.txt"))?,
            b"unstaged parent\n"
        );
        assert_eq!(std::fs::read(f.root.join("ignored.tmp"))?, b"private\n");
        assert_eq!(std::fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(head(&f.root, None)?, base);
        assert!(!parent_cancel.is_cancelled());
        Ok(())
    })
}

#[test]
fn owned_allocator_serializes_parent_integration_until_settled_cohort_release() -> Result<()> {
    fixture(|f| {
        let parent_cancel = crate::core::cancel::TurnCancel::new();
        let allocator = OwnedTaskAllocator::new(&parent_cancel);
        let first =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let second =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let first_scope = NativeWriteScope::resolve(first.root(), &["tracked.txt".into()])?;
        let second_scope = NativeWriteScope::resolve(second.root(), &["tracked.txt".into()])?;
        let first_attempt = f.begin(&first, &first_scope)?;
        let second_attempt = f.begin(&second, &second_scope)?;
        f.write(&first_attempt, "tracked.txt", b"first result\n")?;
        f.write(&second_attempt, "tracked.txt", b"second result\n")?;
        // Both records have real settled commit evidence, not merely dead-owner/liveness evidence.
        let a = f.finish(first, &first_scope, first_attempt)?;
        let b = f.finish(second, &second_scope, second_attempt)?;
        let parent_head = head(&f.root, None)?;
        let parent_index = std::fs::read(f.root.join(".git/index"))?;
        let action = || Change::IntegrateTask {
            receipt: a.registration.registration_id.clone(),
        };
        let prepared = PreparedChange::prepare(&f.root, action())?;
        let error = prepared.execute().unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::core::repo_lock::LockBusy>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(head(&f.root, None)?, parent_head);
        assert_eq!(std::fs::read(f.root.join(".git/index"))?, parent_index);
        assert_eq!(std::fs::read(f.root.join("tracked.txt"))?, b"base\n");
        assert!(crate::core::git_ops::operation_receipts(&f.root)?.is_empty());
        drop(allocator);
        assert!(!parent_cancel.is_cancelled());
        // A fresh explicit integration can proceed only after all settled carriers have retired.
        let receipt = PreparedChange::prepare(&f.root, action())?.execute()?;
        assert_eq!(receipt.after.as_ref(), Some(&a.tip));
        assert_eq!(receipt.state, "settled_unverified");
        assert!(!receipt.verified);
        assert_eq!(head(&a.root, None)?, a.tip);
        assert_eq!(head(&b.root, None)?, b.tip);
        assert_eq!(
            std::fs::read(b.root.join("tracked.txt"))?,
            b"second result\n"
        );
        assert!(crate::core::git_ops::accepted_task_integration(&f.root, &a)?.is_none());
        Ok(())
    })
}

#[test]
fn owned_allocator_scope_drift_stops_cohort_without_new_worktree_or_commit() -> Result<()> {
    fixture(|f| {
        let parent_cancel = crate::core::cancel::TurnCancel::new();
        let allocator = OwnedTaskAllocator::new(&parent_cancel);
        let first =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let scope = NativeWriteScope::resolve(first.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&first, &scope)?;
        f.write(&attempt, "tracked.txt", b"retained child\n")?;
        let inventory = f.git(&["worktree", "list", "--porcelain", "-z"])?;
        let prepared = PreparedOwnedTask::prepare(&f.root, "another-parent")?;
        let error = allocator.execute(prepared).unwrap_err();
        assert!(
            error.to_string().contains("parent scope changed"),
            "{error:#}"
        );
        assert!(allocator.cancel().is_cancelled());
        assert!(!parent_cancel.is_cancelled());
        assert_eq!(
            f.git(&["worktree", "list", "--porcelain", "-z"])?,
            inventory
        );
        let error = f.finish(first.clone(), &scope, attempt).unwrap_err();
        assert!(error.to_string().contains("cohort stopped"), "{error:#}");
        assert_eq!(head(first.root(), None)?, first.base);
        assert_eq!(
            std::fs::read(first.root().join("tracked.txt"))?,
            b"retained child\n"
        );
        Ok(())
    })
}

#[test]
fn owned_allocator_rejects_same_root_new_generation_before_allocation() -> Result<()> {
    fixture(|f| {
        let placement =
            crate::core::file_placement::PlacementContext::new(f.root.clone(), f.home.clone())?;
        let parent = exec_ctx::current()
            .unwrap()
            .with_placement(placement.clone());
        let allocator = OwnedTaskAllocator::new(&crate::core::cancel::TurnCancel::new());
        let first = exec_ctx::with_current(parent.clone(), || {
            allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)
        })?;
        let prepared = exec_ctx::with_current(parent.clone(), || {
            PreparedOwnedTask::prepare(&f.root, "fixture-parent")
        })?;
        let before = f.git(&["worktree", "list", "--porcelain", "-z"])?;
        // Re-seating the identical placement deliberately preserves a turn's binding. Exercise
        // a real A -> B -> A generation change instead of treating that no-op as a focus change.
        assert_eq!(
            parent.with_placement(placement.clone()).workspace_binding(),
            parent.workspace_binding()
        );
        let away_root = f.home.join("other-bound-workspace");
        std::fs::create_dir(&away_root)?;
        let rebound = parent
            .with_placement(placement.fork_for(away_root, &parent.resource_scope())?)
            .with_placement(placement);
        assert_ne!(rebound.workspace_binding(), parent.workspace_binding());
        assert_eq!(
            rebound.placement().unwrap().workspace_root(),
            parent.placement().unwrap().workspace_root()
        );
        let error = exec_ctx::with_current(rebound, || allocator.execute(prepared)).unwrap_err();
        assert!(
            error.to_string().contains("workspace binding is stale"),
            "{error:#}"
        );
        assert_eq!(f.git(&["worktree", "list", "--porcelain", "-z"])?, before);
        assert_eq!(head(first.root(), None)?, first.base);
        Ok(())
    })
}

#[test]
fn owned_allocator_worker_carrier_retains_barrier_and_dead_owner_is_not_settlement() -> Result<()> {
    fixture(|f| {
        let parent_cancel = crate::core::cancel::TurnCancel::new();
        let allocator = OwnedTaskAllocator::new(&parent_cancel);
        let task =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let identity = inspect_identity(&f.root, None)?;
        let barrier = f
            .home
            .join("coop-registration")
            .join(format!("{}.lock", identity.repo_id));
        let carrier = task.clone();
        allocator.stop();
        drop(allocator);
        drop(task);
        assert!(
            crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(
                &barrier,
                Duration::ZERO
            )
            .is_err()
        );
        assert!(!parent_cancel.is_cancelled());
        drop(carrier);
        let settled_lock = crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(
            &barrier,
            Duration::ZERO,
        )?;
        drop(settled_lock);
        let error =
            crate::features::coop::require_no_other_repository_owner(&identity).unwrap_err();
        assert!(
            error.to_string().contains("unfinished owned Git task"),
            "{error:#}"
        );
        Ok(())
    })
}

#[test]
fn owned_allocator_preparation_context_retains_owner_without_granting_authority() -> Result<()> {
    fixture(|f| {
        let parent = exec_ctx::current().unwrap();
        let allocator = OwnedTaskAllocator::new(&crate::core::cancel::TurnCancel::new());
        let task = allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?;
        let identity = inspect_identity(&f.root, None)?;
        let barrier = f
            .home
            .join("coop-registration")
            .join(format!("{}.lock", identity.repo_id));
        let carrier = allocator.context(&parent)?;
        assert_eq!(carrier.workspace_binding(), parent.workspace_binding());
        assert_eq!(carrier.resource_scope(), parent.resource_scope());
        assert_eq!(
            carrier.can_restore_workspace(),
            parent.can_restore_workspace()
        );
        assert!(carrier.native_scope().is_none());
        drop(allocator);
        drop(task);
        assert!(
            crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(
                &barrier,
                Duration::ZERO
            )
            .is_err()
        );
        drop(carrier);
        let lock = crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(
            &barrier,
            Duration::ZERO,
        )?;
        drop(lock);
        assert!(crate::features::coop::require_no_other_repository_owner(&identity).is_err());
        Ok(())
    })
}

#[test]
fn owned_allocator_hook_changes_to_sibling_have_no_native_attribution() -> Result<()> {
    fixture(|f| {
        let allocator = OwnedTaskAllocator::new(&crate::core::cancel::TurnCancel::new());
        let first =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let second =
            Arc::new(allocator.execute(PreparedOwnedTask::prepare(&f.root, "fixture-parent")?)?);
        let first_scope = NativeWriteScope::resolve(first.root(), &["tracked.txt".into()])?;
        let second_scope = NativeWriteScope::resolve(second.root(), &["tracked.txt".into()])?;
        let first_attempt = f.begin(&first, &first_scope)?;
        let second_attempt = f.begin(&second, &second_scope)?;
        f.write(&first_attempt, "tracked.txt", b"first\n")?;
        f.write(&second_attempt, "tracked.txt", b"second\n")?;
        let target = hook_config_path(&second.root().join("tracked.txt"));
        assert!(!target.contains('\''));
        f.hook(
            &first,
            "pre-commit",
            &format!("printf 'foreign hook\\n' > '{target}'"),
        )?;
        let first_handoff = f.finish(first.clone(), &first_scope, first_attempt)?;
        assert_eq!(first_handoff.paths, ["tracked.txt"]);
        assert_eq!(
            std::fs::read(second.root().join("tracked.txt"))?,
            b"foreign hook\n"
        );
        assert!(f
            .finish(second.clone(), &second_scope, second_attempt)
            .is_err());
        assert!(allocator.cancel().is_cancelled());
        assert_eq!(head(second.root(), None)?, second.base);
        assert_eq!(std::fs::read(f.root.join("tracked.txt"))?, b"base\n");
        Ok(())
    })
}

#[test]
fn owned_tasks_with_same_relative_file_do_not_share_native_identity() -> Result<()> {
    fixture(|f| {
        let first = f.task()?;
        let first_root = first.root().to_path_buf();
        let first_scope = NativeWriteScope::resolve(first.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&first, &first_scope)?;
        f.write(&attempt, "tracked.txt", b"first\n")?;
        let first_result = f.finish(first, &first_scope, attempt)?;
        let second = f.task()?;
        let scope = NativeWriteScope::resolve(second.root(), &["tracked.txt".into()])?;
        assert_ne!(scope.write_keys(), first_scope.write_keys());
        let attempt = f.begin(&second, &scope)?;
        assert_eq!(std::fs::read(second.root().join("tracked.txt"))?, b"base\n");
        f.write(&attempt, "tracked.txt", b"second\n")?;
        let second_result = f.finish(second, &scope, attempt)?;
        assert_ne!(first_result.tip, second_result.tip);
        assert_eq!(std::fs::read(first_root.join("tracked.txt"))?, b"first\n");
        assert_eq!(std::fs::read(f.root.join("tracked.txt"))?, b"base\n");
        Ok(())
    })
}

#[test]
fn owned_task_without_changes_hands_off_base_without_commit() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let base = task.base.clone();
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        let handoff = f.finish(task.clone(), &scope, attempt)?;
        assert_eq!(handoff.base, base);
        assert_eq!(handoff.tip, base);
        assert!(handoff.commits.is_empty() && handoff.paths.is_empty());
        assert!(!handoff.dirty && !handoff.verified && !handoff.integrated);
        assert_eq!(handoff.tests, "not_run_native_only");
        Ok(())
    })
}

#[test]
fn owned_task_refuses_out_of_scope_and_ignored_files() -> Result<()> {
    // Separate attempts retain each rejected file and its single-use lifecycle evidence.
    for (name, bytes) in [
        ("outside.txt", b"external\n".as_slice()),
        ("ignored.tmp", b"private\n".as_slice()),
    ] {
        fixture(|f| {
            let task = f.task()?;
            let root = task.root().to_path_buf();
            let scope = NativeWriteScope::resolve(&root, &["tracked.txt".into()])?;
            let attempt = f.begin(&task, &scope)?;
            f.write(&attempt, "tracked.txt", b"child\n")?;
            std::fs::write(root.join(name), bytes)?;
            assert!(f.finish(task.clone(), &scope, attempt).is_err());
            assert_eq!(head(&root, Some(&task.owner))?, task.base);
            assert!(f
                .git_at(&root, &["diff", "--cached", "--name-only"])?
                .is_empty());
            assert_eq!(std::fs::read(root.join(name))?, bytes);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn owned_task_honors_required_clean_filter_failure() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let root = task.root().to_path_buf();
        let scope = NativeWriteScope::resolve(&root, &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child\n")?;
        // Shared attributes are outside the checkout; production must not bypass them.
        std::fs::write(
            f.root.join(".git/info/attributes"),
            b"tracked.txt filter=fixture-fail\n",
        )?;
        f.git_at(&root, &["config", "filter.fixture-fail.clean", "exit 19"])?;
        f.git_at(&root, &["config", "filter.fixture-fail.required", "true"])?;
        assert!(f.finish(task.clone(), &scope, attempt).is_err());
        assert_eq!(head(&root, Some(&task.owner))?, task.base);
        assert_eq!(std::fs::read(root.join("tracked.txt"))?, b"child\n");
        assert_eq!(
            f.git_at(&root, &["config", "--get", "filter.fixture-fail.required"])?,
            b"true\n"
        );
        drop(task);
        assert!(
            crate::features::coop::require_no_other_workspace_owner(&inspect_identity(
                &root, None
            )?)
            .is_err()
        );
        Ok(())
    })
}

#[test]
fn handed_off_task_can_be_explicitly_fast_forwarded_into_clean_parent() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"integrated child\n")?;
        let handoff = f.finish(task.clone(), &scope, attempt)?;
        assert_eq!(handoff.commits.len(), 1);
        assert!(!handoff.verified && !handoff.integrated);
        let child_root = task.root().to_path_buf();
        drop(task);

        // Integration is a separate explicit parent operation. It must consume the exact
        // handoff branch and leave the child worktree/receipt evidence intact.
        let receipt = PreparedChange::prepare(
            &f.root,
            Change::Integrate {
                commit: handoff.branch.clone(),
            },
        )?
        .execute()?;
        assert_eq!(receipt.state, "settled_unverified");
        assert!(receipt.resulting_commits.is_empty());
        assert_eq!(
            String::from_utf8(f.git(&["rev-parse", "HEAD"])?)?,
            format!("{}\n", handoff.tip.as_str())
        );
        assert_eq!(
            std::fs::read(f.root.join("tracked.txt"))?,
            b"integrated child\n"
        );
        assert!(f.git(&["status", "--porcelain"])?.is_empty());
        assert_eq!(head(&child_root, None)?, handoff.tip);
        // Allocation can use the integrated tip explicitly. Scheduler verification remains a
        // separate prerequisite; this fixture proves checkout/base routing, not task success.
        let integrated_tip = receipt.after.context("missing integrated tip")?;
        assert_eq!(integrated_tip, handoff.tip);
        let dependent = Arc::new(
            PreparedOwnedTask::prepare_from(&f.root, "fixture-parent/dependent", &integrated_tip)?
                .execute()?,
        );
        assert_eq!(dependent.base, integrated_tip);
        assert_eq!(
            std::fs::read(dependent.root().join("tracked.txt"))?,
            b"integrated child\n"
        );
        let scope = NativeWriteScope::resolve(dependent.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&dependent, &scope)?;
        let next = f.finish(dependent, &scope, attempt)?;
        assert_eq!(next.base, integrated_tip);
        assert_ne!(next.worktree_id, handoff.worktree_id);
        assert_ne!(
            next.registration.registration_id,
            handoff.registration.registration_id
        );
        assert!(!next.verified && !next.integrated);
        Ok(())
    })
}

#[test]
fn handed_off_task_integration_refuses_dirty_parent_without_touching_child() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child retained\n")?;
        let handoff = f.finish(task.clone(), &scope, attempt)?;
        let child_root = task.root().to_path_buf();
        drop(task);
        let parent_head = f.git(&["rev-parse", "HEAD"])?;
        std::fs::write(f.root.join("tracked.txt"), b"parent pending\n")?;
        let error = PreparedChange::prepare(
            &f.root,
            Change::Integrate {
                commit: handoff.branch.clone(),
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("clean") || error.to_string().contains("unmodified"),
            "{error:#}"
        );
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, parent_head);
        assert_eq!(
            std::fs::read(f.root.join("tracked.txt"))?,
            b"parent pending\n"
        );
        assert_eq!(
            std::fs::read(child_root.join("tracked.txt"))?,
            b"child retained\n"
        );
        Ok(())
    })
}

#[test]
fn handed_off_task_integration_refuses_destination_drift() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"child branch\n")?;
        let handoff = f.finish(task.clone(), &scope, attempt)?;
        let child_root = task.root().to_path_buf();
        drop(task);
        f.git(&["switch", "-qc", "parent-drift"])?;
        f.git(&["switch", "-q", "master"])?;
        // The source branch is still frozen; move the destination with an independent commit.
        std::fs::write(f.root.join("other.txt"), b"destination drift\n")?;
        f.git(&["add", "--", "other.txt"])?;
        f.git(&["commit", "-qm", "destination drift"])?;
        let before = f.git(&["rev-parse", "HEAD"])?;
        assert!(PreparedChange::prepare(
            &f.root,
            Change::Integrate {
                commit: handoff.branch.clone(),
            },
        )
        .is_err());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        assert_eq!(
            std::fs::read(child_root.join("tracked.txt"))?,
            b"child branch\n"
        );
        Ok(())
    })
}

#[path = "git_ops_handoff_tests.rs"]
mod handoff_tests;

#[path = "git_ops_verify_tests.rs"]
mod verify_tests;

#[test]
fn owned_task_status_parser_refuses_unsafe_or_nonliteral_paths() {
    assert!(changed_paths(b" M ordinary.txt\0?? :/glob/literal.txt\0").is_err());
    assert_eq!(
        changed_paths(b" M ordinary.txt\0?? normal.txt\0").unwrap(),
        vec!["normal.txt", "ordinary.txt"]
    );
    for status in [
        b"M  tracked.txt\0".as_slice(),
        b" D tracked.txt\0",
        b"!! ignored.tmp\0",
        b" R a\0b\0",
        b"?? ../escape\0",
        b" M partial",
    ] {
        assert!(changed_paths(status).is_err(), "{status:?}");
    }
}

#[test]
fn owned_native_terminal_publication_fault_retains_real_release_evidence_and_blocks_restart(
) -> Result<()> {
    for after_replace in [false, true] {
        fixture(|f| {
            let task = f.task()?;
            let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
            let attempt = f.begin(&task, &scope)?;
            f.write(&attempt, "tracked.txt", b"retained native child\n")?;
            let child_root = task.root().to_path_buf();
            let child_identity = task.identity.clone();
            let base = task.base.clone();
            let worktrees = f.git(&["worktree", "list", "--porcelain", "-z"])?;
            let sessions = f
                .home
                .join("coop")
                .join(&child_identity.repo_id)
                .join("sessions");
            let manifests: Vec<_> = std::fs::read_dir(&sessions)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                .collect();
            assert_eq!(manifests.len(), 1);
            let manifest_path = &manifests[0];
            let before = std::fs::read(manifest_path)?;
            let admitted: serde_json::Value = serde_json::from_slice(&before)?;
            let registration = admitted["session_id"].as_str().unwrap().to_owned();
            assert!(registration.starts_with("git-task-v2-"));
            let open = &admitted["git_task"]["native_lifecycle"]["evidence"];
            assert_eq!(open["state"], "open");
            let intents = open["workers"].as_array().unwrap();
            assert_eq!(intents.len(), 2);
            assert_eq!(intents[0]["kind"], "read");
            assert_eq!(intents[1]["kind"], "write");
            assert!(intents.iter().all(|intent| intent["closed"].is_null()));

            let FixtureAttempt {
                baseline,
                ctx,
                owner,
            } = attempt;
            drop(ctx);
            let released = f.runtime.block_on(owner.finish_with_driver(
                tokio::time::Instant::now() + Duration::from_secs(30),
                crate::core::speculation::workers::DriverResult::Completed,
            ))?;
            let live_attempt = released.attempt().clone();
            // Real joins/carrier retirement do not themselves publish terminal registry facts.
            assert_eq!(std::fs::read(manifest_path)?, before);
            let journal_path = f.home.join("native-journal-v1").join("records.bin");
            let journal = std::fs::read(&journal_path)?;
            assert!(!journal.is_empty());
            task.registration
                .fail_next_native_publication_for_test(after_replace);
            let error = task
                .clone()
                .finish_native(&scope, &baseline, released)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected native metadata publication fault"),
                "{error:#}"
            );
            assert!(live_attempt.reserve_launch().is_err());
            assert!(live_attempt.close_admission().is_err());
            assert!(task.registration.handoff_receipt().is_err());
            let retry = task
                .registration
                .begin_native(&child_identity, &scope)
                .unwrap_err();
            assert!(
                retry.to_string().contains("publication uncertain"),
                "{retry:#}"
            );

            let after = std::fs::read(manifest_path)?;
            assert_eq!(before == after, !after_replace);
            let visible: serde_json::Value = serde_json::from_slice(&after)?;
            assert_eq!(visible["state"], "working");
            assert_eq!(visible["git_task"]["phase"], "running");
            assert_eq!(visible["git_task"]["base"], admitted["git_task"]["base"]);
            assert_eq!(visible["git_task"]["tip"], admitted["git_task"]["tip"]);
            assert_eq!(visible["git_task"]["paths"], serde_json::json!([]));
            let evidence = &visible["git_task"]["native_lifecycle"]["evidence"];
            assert_eq!(evidence["binding"], open["binding"]);
            assert_eq!(
                evidence["state"],
                if after_replace { "released" } else { "open" }
            );
            let workers = evidence["workers"].as_array().unwrap();
            assert_eq!(workers.len(), intents.len());
            for (worker, intent) in workers.iter().zip(intents) {
                for key in ["ordinal", "kind", "prepared_sha256"] {
                    assert_eq!(worker[key], intent[key]);
                }
                if after_replace {
                    assert_eq!(worker["closed"]["result"], "returned");
                    assert_eq!(worker["closed"]["effect"]["state"], "settled");
                } else {
                    assert!(worker["closed"].is_null());
                }
            }
            if after_replace {
                assert_eq!(evidence["driver"], "completed");
                assert_eq!(evidence["ownership_released"], true);
            }
            assert_eq!(head(&child_root, None)?, base);
            assert!(f
                .git_at(&child_root, &["diff", "--cached", "--name-only"])?
                .is_empty());
            drop(live_attempt);
            drop(task);

            // Even proven lock release is not handoff or permission to infer missing durable
            // facts. Both pre-replace Open and post-replace Released retain a running v2 owner.
            let liveness = crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(
                &manifest_path.with_extension("lock"),
                Duration::ZERO,
            )?;
            drop(liveness);
            let mutation = crate::features::coop::require_no_other_workspace_owner(&child_identity)
                .unwrap_err();
            assert!(
                mutation.to_string().contains("unfinished owned Git task"),
                "{mutation:#}"
            );
            let repository =
                crate::features::coop::require_no_other_repository_owner(&child_identity)
                    .unwrap_err();
            assert!(
                repository.to_string().contains("unfinished owned Git task"),
                "{repository:#}"
            );
            assert!(
                crate::features::coop::FrozenGitTask::select(&child_identity, &registration)
                    .is_err()
            );
            assert!(PreparedChange::prepare(
                &f.root,
                Change::IntegrateTask {
                    receipt: registration
                }
            )
            .is_err());
            let parent = exec_ctx::current().unwrap().with_placement(
                crate::core::file_placement::PlacementContext::new(f.root.clone(), f.home.clone())?,
            );
            let name = child_root.file_name().unwrap().to_str().unwrap();
            let removal =
                exec_ctx::with_current(parent, || crate::features::coop::work_remove(name, false))
                    .unwrap_err();
            assert!(
                removal.to_string().contains("unfinished owned Git task"),
                "{removal:#}"
            );
            assert_eq!(std::fs::read(manifest_path)?, after);
            assert_eq!(std::fs::read(journal_path)?, journal);
            assert_eq!(
                std::fs::read(child_root.join("tracked.txt"))?,
                b"retained native child\n"
            );
            assert_eq!(std::fs::read(f.root.join("tracked.txt"))?, b"base\n");
            assert_eq!(head(&child_root, None)?, base);
            assert_eq!(head(&f.root, None)?, base);
            assert_eq!(
                f.git(&["worktree", "list", "--porcelain", "-z"])?,
                worktrees
            );
            Ok(())
        })?;
    }
    Ok(())
}
