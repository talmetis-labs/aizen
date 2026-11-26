use super::*;
use crate::agent::task_tool::SubagentGate;
use crate::agent::tools::{self, PreparedToolCall, ToolEffectState, ToolOutcome};
use crate::core::exec_ctx;
use crate::core::git_ops::{OwnedTask, PreparedOwnedTask};
use crate::core::native_scope::NativeWriteScope;
use crate::core::worker_evidence::WorkerResult;
use crate::core::workspace_txn::{require_workspace_quiescent, TestLockNamespace};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct Fixture {
    parent: ExecutionContext,
    owned: Arc<OwnedTask>,
    attempt: GitNativeAttempt,
    child: ExecutionContext,
    baseline: Vec<(String, crate::core::persist::FileFingerprint)>,
    runtime: tokio::runtime::Runtime,
    gate: SubagentGate,
    active: &'static AtomicUsize,
    registration: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn owner(&self) -> Result<OwnedNativeAttemptOwner> {
        OwnedNativeAttemptOwner::admit(
            self.gate
                .try_acquire()
                .map_err(|e| anyhow::anyhow!("fixture slot: {e:?}"))?,
            self.attempt.clone(),
            &self.child,
        )
    }

    fn prepared(
        &self,
        ctx: &ExecutionContext,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(Arc<dyn tools::Tool>, PreparedToolCall)> {
        let tool = crate::agent::builtin::file_registry_for_test(self.owned.root())
            .get_arc(name)
            .context("fixture file tool missing")?;
        let prepared = exec_ctx::with_current(ctx.clone(), || tool.prepare_checked(&args, ctx))?;
        Ok((tool, prepared))
    }

    fn invoke(&self, ctx: &ExecutionContext, name: &str, args: serde_json::Value) -> Result<()> {
        let (tool, prepared) = self.prepared(ctx, name, args)?;
        let body_ctx = ctx.clone();
        let body_prepared = prepared.clone();
        let observed = self.runtime.block_on(async {
            tools::workers::launch_prepared(
                ctx.tool_workers(),
                &prepared,
                move || {
                    exec_ctx::with_current(body_ctx, || {
                        crate::agent::execute_owned_native_fixture_body(tool, &body_prepared)
                    })
                },
                ToolEffectState::PossiblyApplied(None),
            )?
            .await
            .context("actual fixture worker result missing")
        })?;
        anyhow::ensure!(
            observed.outcome.failure.is_none(),
            "{}",
            observed.outcome.text
        );
        assert_eq!(observed.outcome.effect_state, ToolEffectState::Settled);
        Ok(())
    }

    fn execute_prepared(
        &self,
        ctx: &ExecutionContext,
        tool: Arc<dyn tools::Tool>,
        prepared: &PreparedToolCall,
    ) -> Result<ToolOutcome> {
        let body_ctx = ctx.clone();
        let body_prepared = prepared.clone();
        self.runtime.block_on(async {
            Ok(tools::workers::launch_prepared(
                ctx.tool_workers(),
                prepared,
                move || {
                    exec_ctx::with_current(body_ctx, || {
                        crate::agent::execute_owned_native_fixture_body(tool, &body_prepared)
                    })
                },
                ToolEffectState::PossiblyApplied(None),
            )?
            .await
            .context("actual parity worker result missing")?
            .outcome)
        })
    }

    fn journal_image(&self) -> Result<(bool, Option<Vec<u8>>)> {
        let dir = self.home.join("native-journal-v1");
        let records = dir.join("records.bin");
        Ok((
            dir.exists(),
            if records.exists() {
                Some(std::fs::read(records)?)
            } else {
                None
            },
        ))
    }

    /// A second real registration in a separate disposable repository, never a forged attempt.
    fn peer(&self) -> Result<(Arc<OwnedTask>, GitNativeAttempt, ExecutionContext)> {
        let root = self.home.parent().unwrap().join("peer-repo");
        std::fs::create_dir(&root)?;
        git(&root, &["init", "-q"])?;
        git(&root, &["config", "user.name", "Fixture"])?;
        git(&root, &["config", "user.email", "fixture@example.invalid"])?;
        git(&root, &["config", "commit.gpgsign", "false"])?;
        git(&root, &["config", "core.autocrlf", "false"])?;
        std::fs::write(root.join("tracked.txt"), b"peer\n")?;
        git(&root, &["add", "tracked.txt"])?;
        git(&root, &["commit", "-qm", "base"])?;
        let owned = Arc::new(PreparedOwnedTask::prepare(&root, "fixture-parent")?.execute()?);
        let scope = NativeWriteScope::resolve(owned.root(), &["tracked.txt".into()])?;
        let _baseline = owned.capture_baseline(&scope)?;
        let attempt = owned.begin_native_attempt(&scope)?;
        let child = self.parent.for_owned_git_task(owned.clone(), scope)?;
        Ok((owned, attempt, child))
    }

    fn assert_quarantined(&self) {
        assert!(
            self.attempt.reserve_launch().is_err(),
            "quarantined owner must also close raw native launch admission"
        );
        assert_eq!(self.active.load(Ordering::SeqCst), 1);
        assert!(require_workspace_quiescent(self.owned.root()).is_err());
        let quarantine = QUARANTINE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(quarantine
            .iter()
            .any(|state| state.attempt.same_attempt(&self.attempt)));
    }
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    // Fixed test-only commands in an exclusively owned disposable repository.
    let output =
        std::process::Command::new(crate::core::gitx::git_exe().context("Git unavailable")?)
            .current_dir(root)
            .args(args)
            .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
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
    let temp = crate::core::file_placement::tests::TestDir::new();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    git(&root, &["init", "-q"])?;
    git(&root, &["config", "user.name", "Fixture"])?;
    git(&root, &["config", "user.email", "fixture@example.invalid"])?;
    git(&root, &["config", "commit.gpgsign", "false"])?;
    git(&root, &["config", "core.autocrlf", "false"])?;
    std::fs::write(root.join("tracked.txt"), b"base\n")?;
    std::fs::write(root.join("other.txt"), b"other\n")?;
    git(&root, &["add", "tracked.txt", "other.txt"])?;
    git(&root, &["commit", "-qm", "base"])?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let active = Box::leak(Box::new(AtomicUsize::new(0)));
    let gate = SubagentGate::isolated(active, 2, home.join("native-runtime-slots"));
    let parent = ExecutionContext::default()
        .with_resource_scope("fixture-parent")
        .with_test_lock_namespace(TestLockNamespace::fixture(&home));
    crate::core::workspace_txn::with_test_lock_home(&home, || {
        exec_ctx::with_current(parent.clone(), || {
            let owned = Arc::new(PreparedOwnedTask::prepare(&root, "fixture-parent")?.execute()?);
            let scope = NativeWriteScope::resolve(
                owned.root(),
                &["tracked.txt".into(), "other.txt".into()],
            )?;
            let baseline = owned.capture_baseline(&scope)?;
            let attempt = owned.begin_native_attempt(&scope)?;
            let child = parent.for_owned_git_task(owned.clone(), scope)?;
            let identity = crate::core::workspace_txn::WorkspaceIdentity::discover(owned.root())?;
            let registrations =
                std::fs::read_dir(home.join("coop").join(identity.repo_id).join("sessions"))?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "json")
                    })
                    .collect::<Vec<_>>();
            anyhow::ensure!(
                registrations.len() == 1,
                "fixture needs one exact registration"
            );
            test(&Fixture {
                parent,
                owned,
                attempt,
                child,
                baseline,
                runtime,
                gate,
                active,
                registration: registrations[0].clone(),
                home: home.clone(),
            })
        })
    })
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

#[test]
fn owned_native_release_requires_real_workers_then_commits_unverified() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        f.invoke(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        f.invoke(
            &ctx,
            "file_write",
            json!({"path":"tracked.txt","content":"owned\n"}),
        )?;
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(f.owned.root())?;
        assert_eq!(released.facts.len(), 2);
        assert!(released
            .facts
            .iter()
            .all(|fact| fact.result == WorkerResult::Returned));
        let handoff = f
            .owned
            .clone()
            .finish_native(f.attempt.scope(), &f.baseline, released)?;
        assert!(!handoff.verified && !handoff.integrated);
        assert_eq!(handoff.paths, vec!["tracked.txt"]);
        Ok(())
    })
}

#[test]
fn owned_native_drop_closes_raw_launch_admission_without_claiming_release() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let raw_attempt = f.attempt.clone();
        let registration = std::fs::read(&f.registration)?;
        let journal = f.journal_image()?;
        let source = std::fs::read(f.owned.root().join("tracked.txt"))?;
        drop(owner);
        assert!(raw_attempt.reserve_launch().is_err());
        f.assert_quarantined();
        assert_eq!(std::fs::read(&f.registration)?, registration);
        assert_eq!(f.journal_image()?, journal);
        assert_eq!(std::fs::read(f.owned.root().join("tracked.txt"))?, source);
        // Retiring the last executable context does not turn abandonment into release.
        drop(ctx);
        assert!(raw_attempt.reserve_launch().is_err());
        f.assert_quarantined();
        assert_eq!(std::fs::read(&f.registration)?, registration);
        assert_eq!(f.journal_image()?, journal);
        assert_eq!(std::fs::read(f.owned.root().join("tracked.txt"))?, source);
        Ok(())
    })
}

#[test]
fn owned_native_live_carrier_timeout_quarantines_all_resources() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let error = f
            .runtime
            .block_on(owner.finish_with_driver(Instant::now(), DriverResult::Completed))
            .unwrap_err();
        assert_eq!(
            crate::agent::tool_error_effect_state(&error),
            ToolEffectState::UnknownWorker
        );
        assert_eq!(
            error
                .downcast_ref::<tools::ToolEffectsUnsettled>()
                .unwrap()
                .0,
            ToolEffectState::UnknownWorker
        );
        assert!(format!("{error:#}").contains("carriers remain live"));
        f.assert_quarantined();
        drop(ctx);
        f.assert_quarantined();
        Ok(())
    })
}

#[test]
fn owned_native_cancelled_finish_cannot_release_slot_or_activity() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        f.runtime.block_on(async {
            assert!(tokio::time::timeout(
                Duration::from_millis(25),
                owner.finish_with_driver(deadline(), DriverResult::Cancelled)
            )
            .await
            .is_err());
        });
        f.assert_quarantined();
        let producer = ctx.tool_workers().unwrap().clone();
        let (_, prepared) = f.prepared(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        let called = Arc::new(AtomicUsize::new(0));
        let body_called = called.clone();
        let result = f.runtime.block_on(async {
            tools::workers::launch_prepared(
                Some(&producer),
                &prepared,
                move || {
                    body_called.fetch_add(1, Ordering::SeqCst);
                    ToolOutcome::failed("not executed")
                },
                ToolEffectState::UnknownWorker,
            )
        });
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("quarantined native admission must not launch a typed worker"),
        };
        assert!(format!("{error:#}").contains("tool-body admission is closed"));
        assert_eq!(called.load(Ordering::SeqCst), 0);
        drop(prepared);
        drop(ctx);
        Ok(())
    })
}

#[test]
fn owned_native_duplicate_owner_and_context_substitution_refuse() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        assert!(f.owner().is_err());
        assert_eq!(f.active.load(Ordering::SeqCst), 1);
        assert!(owner.bind_context(&ExecutionContext::default()).is_err());
        let narrower = f.child.with_native_scope(NativeWriteScope::resolve(
            f.owned.root(),
            &["tracked.txt".into()],
        )?)?;
        assert!(owner.bind_context(&narrower).is_err());
        let generation = f
            .parent
            .for_owned_git_task(f.owned.clone(), f.attempt.scope().clone())?;
        assert!(owner.bind_context(&generation).is_err());
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Failed))?;
        assert_eq!(released.driver, DriverResult::Failed);
        assert!(f.attempt.reserve_launch().is_err());
        Ok(())
    })
}

#[test]
fn owned_native_same_paths_read_only_scope_refuses_before_claim_and_during_bind() -> Result<()> {
    fixture(|f| {
        let registration = std::fs::read(&f.registration)?;
        let tracked = std::fs::read(f.owned.root().join("tracked.txt"))?;
        let other = std::fs::read(f.owned.root().join("other.txt"))?;
        let journal = f.journal_image()?;
        let unchanged = || -> Result<()> {
            assert_eq!(std::fs::read(&f.registration)?, registration);
            assert_eq!(std::fs::read(f.owned.root().join("tracked.txt"))?, tracked);
            assert_eq!(std::fs::read(f.owned.root().join("other.txt"))?, other);
            assert_eq!(f.journal_image()?, journal);
            Ok(())
        };
        let read_only = NativeWriteScope::resolve_access(
            f.owned.root(),
            &["tracked.txt".into(), "other.txt".into()],
            &[],
        )?;
        assert!(f.attempt.scope().has_writes());
        assert!(!read_only.has_writes());
        assert!(f
            .attempt
            .scope()
            .resources()
            .iter()
            .map(|resource| resource.key())
            .eq(read_only.resources().iter().map(|resource| resource.key())));
        // This is valid narrowing over exactly the same paths, not exact attempt authority.
        f.attempt.scope().check_narrowing(&read_only)?;
        assert!(!f.attempt.scope().same_authority(&read_only));
        let narrowed = f.child.with_native_scope(read_only)?;
        let error = OwnedNativeAttemptOwner::admit(
            f.gate.try_acquire().unwrap(),
            f.attempt.clone(),
            &narrowed,
        )
        .err()
        .context("read-only substitution unexpectedly admitted an owner")?;
        assert!(
            format!("{error:#}").contains("native lifecycle context scope differs"),
            "{error:#}"
        );
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(f.owned.root())?;
        unchanged()?;

        // Successful normal admission proves the refusal did not consume the one-shot claim.
        let owner = f.owner()?;
        assert_eq!(f.active.load(Ordering::SeqCst), 1);
        assert!(require_workspace_quiescent(f.owned.root()).is_err());
        let error = owner
            .bind_context(&narrowed)
            .err()
            .context("read-only substitution unexpectedly bound an existing owner")?;
        assert!(
            format!("{error:#}").contains("native lifecycle context scope differs"),
            "{error:#}"
        );
        assert_eq!(f.active.load(Ordering::SeqCst), 1);
        assert!(require_workspace_quiescent(f.owned.root()).is_err());
        unchanged()?;

        // Only the exact context can launch a real prepared worker; rejection is not release.
        let ctx = owner.bind_context(&f.child)?;
        f.invoke(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        drop((ctx, narrowed));
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert_eq!(released.facts.len(), 1);
        assert_eq!(released.facts[0].result, WorkerResult::Returned);
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(f.owned.root())?;
        assert!(f.attempt.reserve_launch().is_err());
        let handoff = f
            .owned
            .clone()
            .finish_native(f.attempt.scope(), &f.baseline, released)?;
        assert!(handoff.paths.is_empty());
        assert!(!handoff.verified && !handoff.integrated);
        assert_eq!(std::fs::read(f.owned.root().join("tracked.txt"))?, tracked);
        assert_eq!(std::fs::read(f.owned.root().join("other.txt"))?, other);
        assert_eq!(f.journal_image()?, journal);
        Ok(())
    })
}

#[test]
fn owned_native_unbound_prepared_call_cannot_fall_back_to_direct() -> Result<()> {
    fixture(|f| {
        let (_, prepared) = f.prepared(&f.child, "file_read", json!({"path":"tracked.txt"}))?;
        assert!(crate::agent::builtin::prepared_git_native_worker(&prepared).is_err());
        let called = Arc::new(AtomicUsize::new(0));
        let body_called = called.clone();
        assert!(f
            .runtime
            .block_on(async {
                tools::workers::launch_prepared(
                    None,
                    &prepared,
                    move || {
                        body_called.fetch_add(1, Ordering::SeqCst);
                        ToolOutcome::failed("not executed")
                    },
                    ToolEffectState::UnknownWorker,
                )
            })
            .is_err());
        assert_eq!(called.load(Ordering::SeqCst), 0);
        Ok(())
    })
}

#[test]
fn owned_native_descriptor_uses_frozen_operation_and_exact_generation() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let (_, mut prepared) = f.prepared(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        let descriptor = crate::agent::builtin::prepared_git_native_worker(&prepared)?.unwrap();
        let digest = descriptor.digest();
        prepared.args = json!({"path":"other.txt","number":true});
        assert_eq!(
            crate::agent::builtin::prepared_git_native_worker(&prepared)?
                .unwrap()
                .digest(),
            digest
        );
        let generation = f
            .parent
            .for_owned_git_task(f.owned.clone(), f.attempt.scope().clone())?;
        assert!(descriptor
            .check_lifecycle(&f.attempt, generation.workspace_binding().unwrap())
            .is_err());
        prepared.footprint = Some(tools::PreparedFootprint::Native(vec![]));
        assert!(crate::agent::builtin::prepared_git_native_worker(&prepared).is_err());
        drop(prepared);
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert!(released.facts.is_empty());
        Ok(())
    })
}

#[test]
fn owned_native_failed_settled_is_closed_but_not_a_successful_candidate() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let (_, prepared) = f.prepared(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        let observation = f.runtime.block_on(async {
            tools::workers::launch_prepared(
                ctx.tool_workers(),
                &prepared,
                || ToolOutcome::failed("registered fixture failure"),
                ToolEffectState::UnknownWorker,
            )?
            .await
            .context("fixture observation lost")
        })?;
        assert_eq!(observation.outcome.effect_state, ToolEffectState::Settled);
        drop(prepared);
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert_eq!(released.facts[0].result, WorkerResult::Failed);
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        let error = f
            .owned
            .clone()
            .finish_native(f.attempt.scope(), &f.baseline, released)
            .unwrap_err();
        assert_ne!(
            crate::agent::tool_error_effect_state(&error),
            ToolEffectState::UnknownWorker
        );
        Ok(())
    })
}

#[test]
fn owned_native_batch_read_preserves_order_duplicates_ranges_and_omissions() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let args = json!({"files":[
            {"path":"other.txt","start":1,"end":1},
            {"path":"tracked.txt"},
            {"path":"other.txt","start":1}
        ]});
        let (_, prepared) = f.prepared(&ctx, "file_read", args.clone())?;
        let Some(tools::PreparedFootprint::Native(resources)) = &prepared.footprint else {
            panic!("native batch footprint missing");
        };
        assert_eq!(resources.len(), 3);
        assert_eq!(resources[0].0.key(), resources[2].0.key());
        let digest = crate::agent::builtin::prepared_git_native_worker(&prepared)?
            .unwrap()
            .digest();
        let (_, reversed) = f.prepared(
            &ctx,
            "file_read",
            json!({"files":[
                {"path":"other.txt","start":1},
                {"path":"tracked.txt"},
                {"path":"other.txt","start":1,"end":1}
            ]}),
        )?;
        assert_ne!(
            crate::agent::builtin::prepared_git_native_worker(&reversed)?
                .unwrap()
                .digest(),
            digest
        );
        let entries = vec![json!({"path":"tracked.txt"}); 9];
        let (_, omitted) = f.prepared(&ctx, "file_read", json!({"files":entries}))?;
        let (_, eight) = f.prepared(&ctx, "file_read", json!({"files":&entries[..8]}))?;
        assert_ne!(
            crate::agent::builtin::prepared_git_native_worker(&omitted)?
                .unwrap()
                .digest(),
            crate::agent::builtin::prepared_git_native_worker(&eight)?
                .unwrap()
                .digest()
        );
        f.invoke(&ctx, "file_read", args)?;
        drop((prepared, reversed, omitted, eight));
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert_eq!(released.facts.len(), 1);
        Ok(())
    })
}

#[test]
fn owned_native_panicked_body_never_mints_release() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let (_, prepared) = f.prepared(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        let observation = f.runtime.block_on(async {
            tools::workers::launch_prepared(
                ctx.tool_workers(),
                &prepared,
                || panic!("native fixture panic"),
                ToolEffectState::UnknownWorker,
            )?
            .await
            .context("fixture observation lost")
        })?;
        assert_eq!(
            observation.outcome.effect_state,
            ToolEffectState::UnknownWorker
        );
        drop(prepared);
        drop(ctx);
        assert!(f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Failed))
            .is_err());
        f.assert_quarantined();
        Ok(())
    })
}

#[test]
fn owned_native_observer_drop_cannot_settle_a_blocked_actual_body() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let (tool, prepared) = f.prepared(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        let (started_send, started_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::channel();
        let (done_send, done_receive) = std::sync::mpsc::channel();
        let body_prepared = prepared.clone();
        let body_ctx = ctx.clone();
        let observer = f.runtime.block_on(async {
            tools::workers::launch_prepared(
                ctx.tool_workers(),
                &prepared,
                move || {
                    started_send.send(()).unwrap();
                    release_receive
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                    let result = exec_ctx::with_current(body_ctx, || {
                        crate::agent::execute_owned_native_fixture_body(tool, &body_prepared)
                    });
                    done_send.send(()).unwrap();
                    result
                },
                ToolEffectState::UnknownWorker,
            )
        })?;
        started_receive.recv_timeout(Duration::from_secs(10))?;
        drop(observer);
        drop(prepared);
        drop(ctx);
        assert!(f
            .runtime
            .block_on(owner.finish_with_driver(Instant::now(), DriverResult::Completed))
            .is_err());
        f.assert_quarantined();
        release_send.send(())?;
        done_receive.recv_timeout(Duration::from_secs(10))?;
        f.assert_quarantined();
        Ok(())
    })
}

#[test]
fn owned_native_intent_publication_failure_never_starts_body_or_releases_owner() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let (_, prepared) = f.prepared(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        let mut changed = std::fs::read(&f.registration)?;
        changed.push(b'\n');
        std::fs::write(&f.registration, &changed)?;
        let calls = Arc::new(AtomicUsize::new(0));
        let body_calls = calls.clone();
        let observation = f.runtime.block_on(async {
            tools::workers::launch_prepared(
                ctx.tool_workers(),
                &prepared,
                move || {
                    body_calls.fetch_add(1, Ordering::SeqCst);
                    ToolOutcome::failed("body must not start")
                },
                ToolEffectState::UnknownWorker,
            )?
            .await
            .context("fixture observation lost")
        })?;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            observation.outcome.effect_state,
            ToolEffectState::UnknownWorker
        );
        assert!(observation
            .outcome
            .text
            .contains("intent publication uncertain"));
        assert_eq!(std::fs::read(&f.registration)?, changed);
        assert_eq!(
            std::fs::read(f.owned.root().join("tracked.txt"))?,
            b"base\n"
        );
        drop(prepared);
        drop(ctx);
        assert!(f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))
            .is_err());
        f.assert_quarantined();
        assert!(f.attempt.reserve_launch().is_err());
        Ok(())
    })
}

#[test]
fn owned_native_terminal_publication_failure_never_commits_or_reopens() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        f.invoke(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        let mut changed = std::fs::read(&f.registration)?;
        changed.push(b'\n');
        std::fs::write(&f.registration, &changed)?;
        assert!(f
            .owned
            .clone()
            .finish_native(f.attempt.scope(), &f.baseline, released)
            .is_err());
        assert!(f.attempt.reserve_launch().is_err());
        assert!(f.attempt.claim_owner().is_err());
        assert_eq!(std::fs::read(&f.registration)?, changed);
        let manifest: serde_json::Value = serde_json::from_slice(&changed)?;
        assert_eq!(manifest["git_task"]["phase"], "running");
        Ok(())
    })
}

#[test]
fn owned_native_footprint_missing_substitution_and_access_changes_never_launch() -> Result<()> {
    use crate::core::repo_lock::LockMode;
    use crate::core::workspace_txn::NativeResource;
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let registration = std::fs::read(&f.registration)?;
        let journal = f.journal_image()?;
        for (name, args, expected_mode) in [
            ("file_read", json!({"path":"tracked.txt"}), LockMode::Shared),
            (
                "file_edit",
                json!({"path":"tracked.txt","old_string":"base","new_string":"edited"}),
                LockMode::Exclusive,
            ),
            (
                "file_write",
                json!({"path":"tracked.txt","content":"written\n"}),
                LockMode::Exclusive,
            ),
        ] {
            let (_, prepared) = f.prepared(&ctx, name, args)?;
            let original = NativeResource::resolve(&f.owned.root().join("tracked.txt"))?;
            let replacement = NativeResource::resolve(&f.owned.root().join("other.txt"))?;
            let flipped = if expected_mode == LockMode::Shared {
                LockMode::Exclusive
            } else {
                LockMode::Shared
            };
            for footprint in [
                None,
                Some(tools::PreparedFootprint::Global),
                Some(tools::PreparedFootprint::Native(vec![])),
                Some(tools::PreparedFootprint::Native(vec![(
                    replacement,
                    expected_mode,
                )])),
                Some(tools::PreparedFootprint::Native(vec![(original, flipped)])),
            ] {
                let mut altered = prepared.clone();
                altered.footprint = footprint;
                assert!(
                    crate::agent::builtin::prepared_git_native_worker(&altered).is_err(),
                    "{name}"
                );
                let calls = Arc::new(AtomicUsize::new(0));
                let body_calls = calls.clone();
                let result = f.runtime.block_on(async {
                    tools::workers::launch_prepared(
                        ctx.tool_workers(),
                        &altered,
                        move || {
                            body_calls.fetch_add(1, Ordering::SeqCst);
                            ToolOutcome::failed("must not execute")
                        },
                        ToolEffectState::UnknownWorker,
                    )
                });
                assert!(result.is_err(), "{name}");
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            }
        }
        assert_eq!(std::fs::read(&f.registration)?, registration);
        assert_eq!(f.journal_image()?, journal);
        assert_eq!(
            std::fs::read(f.owned.root().join("tracked.txt"))?,
            b"base\n"
        );
        assert_eq!(std::fs::read(f.owned.root().join("other.txt"))?, b"other\n");
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert!(released.facts.is_empty());
        Ok(())
    })
}

#[test]
fn owned_native_all_file_tools_refuse_owner_attempt_unbound_and_generation_transplants(
) -> Result<()> {
    fixture(|f| {
        let (peer, peer_attempt, peer_child) = f.peer()?;
        let owner = f.owner()?;
        let peer_owner = OwnedNativeAttemptOwner::admit(
            f.gate.try_acquire().unwrap(),
            peer_attempt.clone(),
            &peer_child,
        )?;
        let ctx = owner.bind_context(&f.child)?;
        let peer_ctx = peer_owner.bind_context(&peer_child)?;
        let wrong_owner = ctx.with_resource_scope("fixture-parent/impostor");
        let generation = f
            .parent
            .for_owned_git_task(f.owned.clone(), f.attempt.scope().clone())?;
        let direct = f.parent.with_native_scope(f.attempt.scope().clone())?;
        let journal = f.journal_image()?;
        for (name, args) in [
            ("file_read", json!({"path":"tracked.txt"})),
            (
                "file_edit",
                json!({"path":"tracked.txt","old_string":"base","new_string":"edited"}),
            ),
            (
                "file_write",
                json!({"path":"tracked.txt","content":"written\n"}),
            ),
        ] {
            let (tool, prepared) = f.prepared(&ctx, name, args.clone())?;
            let descriptor = crate::agent::builtin::prepared_git_native_worker(&prepared)?.unwrap();
            assert!(descriptor
                .check_lifecycle(&peer_attempt, peer_ctx.workspace_binding().unwrap())
                .is_err());
            for ambient in [
                &wrong_owner,
                &peer_ctx,
                &f.child,
                &generation,
                &direct,
                &f.parent,
            ] {
                // Exercise the actual frozen tool guard, as well as launch/controller admission.
                assert!(
                    exec_ctx::with_current(ambient.clone(), || tool.execute_prepared(&prepared))
                        .is_err(),
                    "{name}"
                );
                match f.execute_prepared(ambient, tool.clone(), &prepared) {
                    Err(_) => {}
                    Ok(outcome) => {
                        assert!(outcome.failure.is_some(), "{name}");
                        assert_eq!(outcome.effect_state, ToolEffectState::Settled);
                    }
                }
            }
            // The reverse direction cannot smuggle ordinary direct preparation into the owner.
            let (direct_tool, direct_call) = f.prepared(&direct, name, args)?;
            assert!(f.execute_prepared(&ctx, direct_tool, &direct_call).is_err());
        }
        assert_eq!(f.journal_image()?, journal);
        assert_eq!(
            std::fs::read(f.owned.root().join("tracked.txt"))?,
            b"base\n"
        );
        assert_eq!(std::fs::read(peer.root().join("tracked.txt"))?, b"peer\n");
        drop((ctx, peer_ctx, wrong_owner, generation, direct));
        let release = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Failed))?;
        assert!(release
            .facts
            .iter()
            .all(|fact| fact.result == WorkerResult::Failed));
        let peer_release = f
            .runtime
            .block_on(peer_owner.finish_with_driver(deadline(), DriverResult::Failed))?;
        assert!(peer_release.facts.is_empty());
        Ok(())
    })
}

#[test]
fn owned_native_hidden_ninth_read_is_validated_before_any_observation_or_publication() -> Result<()>
{
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let registration = std::fs::read(&f.registration)?;
        let journal = f.journal_image()?;
        let path = f.owned.root().join("tracked.txt");
        let sentinel = crate::core::persist::FileFingerprint::for_bytes(b"not the actual source");
        crate::core::read_ledger::note(&path, &sentinel);
        for (hidden, expected) in [
            (json!({"path":"outside.txt"}), "outside controller-approved"),
            (json!({"start":1}), "requires an exact path"),
        ] {
            let mut files = vec![json!({"path":"tracked.txt"}); 8];
            files.push(hidden);
            let error = match f.prepared(&ctx, "file_read", json!({"files":files})) {
                Err(error) => error,
                Ok(_) => panic!("omitted ninth entry must be validated"),
            };
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert_eq!(
                crate::core::read_ledger::observed(&path),
                Some(sentinel.clone())
            );
        }
        assert_eq!(std::fs::read(&f.registration)?, registration);
        assert_eq!(f.journal_image()?, journal);
        assert!(!f.owned.root().join("outside.txt").exists());
        assert_eq!(std::fs::read(&path)?, b"base\n");
        crate::core::read_ledger::forget(&path);
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert!(released.facts.is_empty());
        Ok(())
    })
}

#[test]
fn owned_native_edit_and_write_execute_frozen_bytes_not_retargeted_wire_arguments() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        f.invoke(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        for (name, args, expected) in [
            (
                "file_edit",
                json!({"path":"tracked.txt","old_string":"base","new_string":"edited"}),
                "edited\n",
            ),
            (
                "file_write",
                json!({"path":"tracked.txt","content":"written\n"}),
                "written\n",
            ),
        ] {
            let (tool, mut prepared) = f.prepared(&ctx, name, args)?;
            let digest = crate::agent::builtin::prepared_git_native_worker(&prepared)?
                .unwrap()
                .digest();
            prepared.args = json!({"path":"other.txt","old_string":"other","new_string":"forged","content":"forged\n","purpose":"scratch"});
            assert_eq!(
                crate::agent::builtin::prepared_git_native_worker(&prepared)?
                    .unwrap()
                    .digest(),
                digest
            );
            let outcome = f.execute_prepared(&ctx, tool, &prepared)?;
            assert!(outcome.failure.is_none(), "{}", outcome.text);
            assert_eq!(outcome.effect_state, ToolEffectState::Settled);
            assert_eq!(
                std::fs::read_to_string(f.owned.root().join("tracked.txt"))?,
                expected
            );
            assert_eq!(std::fs::read(f.owned.root().join("other.txt"))?, b"other\n");
            f.invoke(&ctx, "file_read", json!({"path":"tracked.txt"}))?;
        }
        drop(ctx);
        let released = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert_eq!(released.facts.len(), 5);
        let handoff = f
            .owned
            .clone()
            .finish_native(f.attempt.scope(), &f.baseline, released)?;
        assert_eq!(handoff.paths, vec!["tracked.txt"]);
        assert!(!handoff.verified && !handoff.integrated);
        Ok(())
    })
}

#[test]
fn owned_native_nonallowlisted_file_move_is_refused_before_effects() -> Result<()> {
    fixture(|f| {
        let owner = f.owner()?;
        let ctx = owner.bind_context(&f.child)?;
        let tool = crate::agent::builtin::file_registry_for_test(f.owned.root())
            .get_arc("file_move")
            .unwrap();
        let prepared = exec_ctx::with_current(ctx.clone(), || {
            PreparedToolCall::new(
                json!({"from":"tracked.txt","to":"other.txt","purpose":"project"}),
            )
        });
        let registration = std::fs::read(&f.registration)?;
        let journal = f.journal_image()?;
        assert!(f.execute_prepared(&ctx, tool.clone(), &prepared).is_err());
        // The body boundary independently refuses even if an internal caller reaches it directly.
        let outcome = exec_ctx::with_current(ctx.clone(), || {
            crate::agent::execute_owned_native_fixture_body(tool, &prepared)
        });
        assert!(outcome
            .failure
            .as_deref()
            .unwrap()
            .contains("outside its approved file-only allowlist"));
        assert_eq!(outcome.effect_state, ToolEffectState::Settled);
        assert_eq!(std::fs::read(&f.registration)?, registration);
        assert_eq!(f.journal_image()?, journal);
        assert_eq!(
            std::fs::read(f.owned.root().join("tracked.txt"))?,
            b"base\n"
        );
        assert_eq!(std::fs::read(f.owned.root().join("other.txt"))?, b"other\n");
        drop(prepared);
        drop(ctx);
        let release = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert!(release.facts.is_empty());
        Ok(())
    })
}

#[test]
fn owned_native_wrong_root_admission_cannot_claim_owner_and_release_rejects_peer() -> Result<()> {
    fixture(|f| {
        let (peer, peer_attempt, _) = f.peer()?;
        let wrong = f
            .child
            .with_placement(crate::core::file_placement::PlacementContext::new(
                peer.root().to_path_buf(),
                f.home.clone(),
            )?);
        assert!(OwnedNativeAttemptOwner::admit(
            f.gate.try_acquire().unwrap(),
            f.attempt.clone(),
            &wrong
        )
        .is_err());
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(f.owned.root())?;
        let owner = f.owner()?; // Rejection did not consume the one-time owner claim.
        let release = f
            .runtime
            .block_on(owner.finish_with_driver(deadline(), DriverResult::Completed))?;
        assert!(f.attempt.reserve_launch().is_err());
        assert!(release.into_joined_for(&peer_attempt).is_err());
        assert_eq!(f.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(f.owned.root())?;
        Ok(())
    })
}

#[test]
fn native_fixture_lock_namespace_survives_threads_without_hiding_child_contention() -> Result<()> {
    use crate::core::workspace_txn::{
        with_test_lock_home, PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
    };
    let temp = crate::core::file_placement::tests::TestDir::new();
    let root = temp.path().join("workspace");
    let ambient = temp.path().join("ambient");
    let isolated = temp.path().join("isolated");
    for path in [&root, &ambient, &isolated] {
        std::fs::create_dir(path)?;
    }
    let identity = WorkspaceIdentity::from_bound_paths(std::fs::canonicalize(root)?, None, None);
    let ambient_owners = with_test_lock_home(&ambient, || -> Result<_> {
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::ZERO,
            None,
            "ambient writer",
            "ambient",
        )?;
        let physical =
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "ambient physical", "ambient")?;
        assert!(WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::ZERO,
            None,
            "unrelated ambient",
            "parent"
        )
        .is_err());
        assert!(PhysicalEffectLease::exclusive(
            Duration::ZERO,
            None,
            "ambient contention",
            "parent"
        )
        .is_err());
        Ok((writer, physical))
    })?;
    let parent = ExecutionContext::default()
        .with_resource_scope("parent")
        .with_test_lock_namespace(TestLockNamespace::fixture(&isolated));
    let child = parent.with_resource_scope("parent/child");
    exec_ctx::with_current(parent, || -> Result<()> {
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::ZERO,
            None,
            "fixture writer",
            "parent",
        )?;
        let physical =
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "fixture physical", "parent")?;
        let gate = crate::core::workspace_txn::resource_lock("native-effects", "global");
        std::thread::spawn(move || {
            exec_ctx::with_current(child.clone(), || -> Result<()> {
                assert_eq!(
                    crate::core::workspace_txn::resource_lock("native-effects", "global"),
                    gate
                );
                assert!(WorkspaceWriterLease::acquire_identity_scoped(
                    identity.clone(),
                    Duration::ZERO,
                    None,
                    "unrelated fixture",
                    "unrelated"
                )
                .is_err());
                let _inherited = WorkspaceWriterLease::acquire_identity_scoped(
                    identity,
                    Duration::ZERO,
                    None,
                    "fixture child",
                    &child.resource_scope(),
                )?;
                assert!(PhysicalEffectLease::exclusive(
                    Duration::ZERO,
                    None,
                    "fixture child physical",
                    &child.resource_scope()
                )
                .is_err());
                Ok(())
            })
        })
        .join()
        .expect("fixture worker panicked")?;
        drop((writer, physical));
        let _settled =
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "after real release", "parent")?;
        Ok(())
    })?;
    drop(ambient_owners);
    Ok(())
}
