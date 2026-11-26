//! Admission-state regression tests using real slots and disposable Git-owned lifecycles.
//! Historical overlay execution is retired; serialized records never construct runtime authority.

use super::*;
use crate::agent::task_tool::SubagentGate;
use crate::agent::tools::{self, workers, PreparedToolCall, ToolEffectState, ToolOutcome};
use crate::core::cancel::TurnCancel;
use crate::core::exec_ctx::{self, ExecutionContext};
use crate::core::file_placement::{tests::TestDir, PlacementContext};
use crate::core::git_ops::{GitNativeAttempt, OwnedTask, PreparedOwnedTask};
use crate::core::native_scope::NativeWriteScope;
use crate::core::worker_evidence::{DriverResult, WorkerResult};
use crate::core::workspace_txn::{require_workspace_quiescent, TestLockNamespace};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

const PARENT: &str = "controller/workflow-admission";
const LIVE_BYTES: &[u8] = b"untouched native live source\n";

/// Shared only with the graph-owner regression: allocation always uses the real Git controller.
pub(super) struct Fixture {
    pub(super) work: PathBuf,
    home: PathBuf,
    pub(super) parent: ExecutionContext,
    runtime: tokio::runtime::Runtime,
    active: &'static AtomicUsize,
    gate: SubagentGate,
}

struct NativeFixture {
    owned: Arc<OwnedTask>,
    attempt: GitNativeAttempt,
    child: ExecutionContext,
    baseline: Vec<(String, crate::core::persist::FileFingerprint)>,
}

impl Fixture {
    fn slot(&self) -> crate::agent::task_tool::SubagentSlot {
        self.gate.try_acquire().unwrap()
    }

    pub(super) fn allocate(&self) -> Arc<OwnedTask> {
        Arc::new(
            PreparedOwnedTask::prepare(&self.work, PARENT)
                .unwrap()
                .execute()
                .unwrap(),
        )
    }

    fn native(&self) -> NativeFixture {
        let owned = self.allocate();
        let scope = NativeWriteScope::resolve(owned.root(), &["owned.txt".to_owned()]).unwrap();
        let baseline = owned.capture_baseline(&scope).unwrap();
        let attempt = owned.begin_native_attempt(&scope).unwrap();
        let child = self
            .parent
            .for_owned_git_task(owned.clone(), scope)
            .unwrap();
        NativeFixture {
            owned,
            attempt,
            child,
            baseline,
        }
    }

    fn run<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(30),
                exec_ctx::with_current_future(self.parent.clone(), future),
            )
            .await
            .expect("bounded workflow admission fixture")
        })
    }

    fn assert_released_and_untouched(&self, owned: Option<&OwnedTask>) {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(&self.work).unwrap();
        assert_eq!(
            std::fs::read(self.work.join("owned.txt")).unwrap(),
            LIVE_BYTES
        );
        if let Some(owned) = owned {
            require_workspace_quiescent(owned.root()).unwrap();
            assert_eq!(
                std::fs::read(owned.root().join("owned.txt")).unwrap(),
                LIVE_BYTES
            );
        }
        drop(self.slot());
    }

    fn assert_quarantined(&self, native: &NativeFixture) {
        assert_eq!(self.active.load(Ordering::SeqCst), 1);
        assert!(self.gate.try_acquire().is_err());
        assert!(require_workspace_quiescent(native.owned.root()).is_err());
        assert_eq!(
            std::fs::read(self.work.join("owned.txt")).unwrap(),
            LIVE_BYTES
        );
        assert_eq!(
            std::fs::read(native.owned.root().join("owned.txt")).unwrap(),
            LIVE_BYTES
        );
    }

    fn prepared_read(
        &self,
        native: &NativeFixture,
        ctx: &ExecutionContext,
    ) -> (Arc<dyn tools::Tool>, PreparedToolCall) {
        let tool = crate::agent::builtin::file_registry_for_test(native.owned.root())
            .get_arc("file_read")
            .unwrap();
        let prepared = exec_ctx::with_current(ctx.clone(), || {
            tool.prepare_checked(&serde_json::json!({"path": "owned.txt"}), ctx)
        })
        .unwrap();
        (tool, prepared)
    }
}

/// A global HOME guard protects the existing config API, not policy or effect admission.
/// No sandbox setting is changed. All Git commands below target this disposable repository.
pub(super) fn with_fixture(test: impl FnOnce(&Fixture)) {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let temp = TestDir::new();
    let work = temp.path().join("work");
    let home = temp.path().join("home");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&home).unwrap();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let git = crate::core::gitx::git_exe().expect("Git required for owned admission fixture");
    let git = |args: &[&str]| {
        let mut child = std::process::Command::new(&git)
            .current_dir(&work)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stop = std::time::Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= stop {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded disposable Git fixture command timed out: {args:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "--local", "user.name", "Fixture"]);
    git(&["config", "--local", "user.email", "fixture@example.invalid"]);
    git(&["config", "--local", "commit.gpgsign", "false"]);
    git(&["config", "--local", "core.autocrlf", "false"]);
    let hooks = home.join("empty-hooks");
    std::fs::create_dir(&hooks).unwrap();
    git(&[
        "config",
        "--local",
        "core.hooksPath",
        &hooks.to_string_lossy(),
    ]);
    std::fs::write(work.join("owned.txt"), LIVE_BYTES).unwrap();
    git(&["add", "--", "owned.txt"]);
    git(&["commit", "-qm", "fixture base"]);
    let work = std::fs::canonicalize(work).unwrap();
    let active = Box::leak(Box::new(AtomicUsize::new(0)));
    let parent = ExecutionContext::default()
        .with_resource_scope(PARENT)
        .with_placement(PlacementContext::new(work.clone(), home.clone()).unwrap())
        .with_test_lock_namespace(TestLockNamespace::fixture(&home));
    let fixture = Fixture {
        work,
        home: home.clone(),
        parent: parent.clone(),
        runtime: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
        active,
        gate: SubagentGate::isolated(active, 1, home.join("slots")),
    };
    crate::core::workspace_txn::with_test_lock_home(&home, || {
        exec_ctx::with_current(parent, || test(&fixture))
    });
}

fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(10)
}

fn task(role: &str) -> WorkflowTask {
    WorkflowTask {
        id: "known-not-started".to_owned(),
        role: role.to_owned(),
        execution: ExecutionMode::Direct,
        prompt: "Inspect the exact owned file and return a concise report; do not run commands."
            .to_owned(),
        write_paths: Some(vec!["owned.txt".to_owned()]),
        max_steps: Some(1),
        ..WorkflowTask::default()
    }
}

async fn admitted_no_network(
    fixture: &Fixture,
    task: &WorkflowTask,
    cancel: TurnCancel,
    owned: Option<&OwnedWorkflowAttempt>,
) -> TaskOutcome {
    // Malformed URL and prestart branches ensure the model client never makes an external request.
    let root = task
        .owned
        .as_ref()
        .map_or(fixture.work.as_path(), |owned| owned.root());
    run_one_task_admitted_owned(
        &reqwest::Client::new(),
        "://unused-invalid-endpoint",
        "unused-test-key",
        "unused-test-model",
        crate::core::approval::ApprovalMode::Yolo,
        root,
        "2026-10-16",
        task,
        None,
        cancel,
        0,
        &fixture.parent,
        fixture.slot(),
        owned,
    )
    .await
}

fn assert_known_no_start(outcome: &TaskOutcome, status: &str) {
    assert_eq!(outcome.status, status, "{}", outcome.summary);
    assert_eq!(outcome.iters, 0);
    assert_eq!((outcome.tokens_in, outcome.tokens_out), (0, 0));
    assert!(outcome.git_handoff.is_none());
    assert!(!outcome
        .summary
        .contains("physical attempt closure was not certified"));
}

#[test]
fn admitted_wrapper_precancel_preserves_cancelled_without_unknown_worker() {
    with_fixture(|fixture| {
        for git_owned in [false, true] {
            let native = git_owned.then(|| fixture.native());
            let live = native.as_ref().map(|native| OwnedWorkflowAttempt {
                attempt: native.attempt.clone(),
                released: std::sync::Mutex::new(None),
            });
            let mut task = task("daedalus");
            if let Some(native) = &native {
                task.owned = Some(native.owned.clone());
                task.isolation = WorkflowIsolation::Worktree;
            }
            let cancel = TurnCancel::new();
            cancel.cancel();
            let outcome = fixture.run(admitted_no_network(fixture, &task, cancel, live.as_ref()));
            assert_known_no_start(&outcome, "cancelled");
            if let Some(live) = &live {
                assert!(live.released.lock().unwrap().is_none());
            }
            fixture.assert_released_and_untouched(native.as_ref().map(|n| n.owned.as_ref()));
        }
    });
}

#[test]
fn admitted_wrapper_incomplete_owned_lifecycle_is_known_no_start_without_direct_fallback() {
    with_fixture(|fixture| {
        let native = fixture.native();
        let live = OwnedWorkflowAttempt {
            attempt: native.attempt.clone(),
            released: std::sync::Mutex::new(None),
        };
        // Both missing halves are refused, before cancellation, role resolution, or any model IO.
        for task_has_owner in [false, true] {
            let mut task = task("daedalus");
            if task_has_owner {
                task.owned = Some(native.owned.clone());
                task.isolation = WorkflowIsolation::Worktree;
            }
            let outcome = fixture.run(admitted_no_network(
                fixture,
                &task,
                TurnCancel::new(),
                (!task_has_owner).then_some(&live),
            ));
            assert_known_no_start(&outcome, "admission-error");
            assert!(
                outcome.summary.contains("Git-native lifecycle"),
                "{}",
                outcome.summary
            );
            assert!(
                outcome.summary.contains("no direct fallback"),
                "{}",
                outcome.summary
            );
            assert!(live.released.lock().unwrap().is_none());
            fixture.assert_released_and_untouched(Some(&native.owned));
        }
        // Rejection did not consume the real owner ticket.
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        let executable = admission.bind_context(&native.child).unwrap();
        drop(executable);
        fixture
            .run(admission.finish_native(deadline(), DriverResult::Cancelled))
            .unwrap();
        assert!(admission.take_git_release().is_some());
        fixture.assert_released_and_untouched(Some(&native.owned));
    });
}

#[test]
fn admitted_wrapper_bad_role_preserves_error_without_unknown_worker() {
    with_fixture(|fixture| {
        for git_owned in [false, true] {
            let native = git_owned.then(|| fixture.native());
            let live = native.as_ref().map(|native| OwnedWorkflowAttempt {
                attempt: native.attempt.clone(),
                released: std::sync::Mutex::new(None),
            });
            let mut task = task("not-a-real-workflow-role");
            if let Some(native) = &native {
                task.owned = Some(native.owned.clone());
                task.isolation = WorkflowIsolation::Worktree;
            }
            let outcome = fixture.run(admitted_no_network(
                fixture,
                &task,
                TurnCancel::new(),
                live.as_ref(),
            ));
            assert_known_no_start(&outcome, "error");
            assert!(outcome.summary.contains("role"), "{}", outcome.summary);
            fixture.assert_released_and_untouched(native.as_ref().map(|n| n.owned.as_ref()));
        }
    });
}

#[test]
fn direct_admission_keeps_existing_arc_retention_behavior() {
    with_fixture(|fixture| {
        let mut admission = WorkflowAttemptAdmission::new_direct(fixture.slot());
        assert!(matches!(admission, WorkflowAttemptAdmission::Direct(_)));
        assert!(admission.physically_finished());
        assert!(admission.take_git_release().is_none());
        let context = admission.bind_context(&fixture.parent).unwrap();
        assert!(
            !admission.physically_finished(),
            "direct context retains its Arc slot"
        );
        assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
        drop(context);
        assert!(admission.physically_finished());
        drop(admission);
        fixture.assert_released_and_untouched(None);
    });
}

#[test]
fn git_admission_ready_running_released_and_taken_token_stay_distinct() {
    with_fixture(|fixture| {
        let background = crate::core::workspace_txn::SidecarEffectLease::acquire(
            crate::core::workspace_txn::SidecarKind::BackgroundProcess,
            Duration::ZERO,
            None,
        )
        .unwrap();
        assert!(crate::core::workspace_txn::BackgroundProcessBarrier::acquire(None).is_err());
        drop(background);
        let native = fixture.native();
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        assert!(matches!(
            admission,
            WorkflowAttemptAdmission::GitReady { slot: Some(_), .. }
        ));
        assert!(
            admission.physically_finished(),
            "ready is known not started"
        );
        assert!(admission.take_git_release().is_none());
        let executable = admission.bind_context(&native.child).unwrap();
        assert!(matches!(admission, WorkflowAttemptAdmission::GitRunning(_)));
        assert!(!admission.physically_finished());
        assert!(require_workspace_quiescent(native.owned.root()).is_err());
        assert!(
            native.attempt.claim_owner().is_err(),
            "owner claim is single-use"
        );
        drop(executable);
        fixture
            .run(admission.finish_native(deadline(), DriverResult::Completed))
            .unwrap();
        assert!(matches!(
            admission,
            WorkflowAttemptAdmission::GitReleased(Some(_))
        ));
        assert!(admission.physically_finished());
        let token = admission
            .take_git_release()
            .expect("released attempt has exactly one token");
        assert!(matches!(
            admission,
            WorkflowAttemptAdmission::GitReleased(None)
        ));
        assert!(
            admission.physically_finished(),
            "moving proof cannot undo physical closure"
        );
        assert!(admission.take_git_release().is_none());
        assert!(
            native.attempt.reserve_launch().is_err(),
            "raw attempt cannot reopen admission"
        );
        assert!(native.attempt.claim_owner().is_err());
        let handoff = native
            .owned
            .clone()
            .finish_native(native.attempt.scope(), &native.baseline, token)
            .unwrap();
        assert!(handoff.paths.is_empty());
        assert!(!handoff.verified && !handoff.integrated);
        fixture.assert_released_and_untouched(Some(&native.owned));
    });
}

#[test]
fn cancelled_git_finish_stays_unknown_and_retains_slot_and_activity() {
    with_fixture(|fixture| {
        let native = fixture.native();
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        let executable = admission.bind_context(&native.child).unwrap();
        let producer = executable.tool_workers().unwrap().clone();
        let (_, prepared) = fixture.prepared_read(&native, &executable);
        fixture.run(async {
            let mut finish = Box::pin(admission.finish_native(deadline(), DriverResult::Completed));
            std::future::poll_fn(|cx| {
                assert!(matches!(finish.as_mut().poll(cx), Poll::Pending));
                Poll::Ready(())
            })
            .await;
            drop(finish);
        });
        assert!(matches!(admission, WorkflowAttemptAdmission::GitUnknown));
        assert!(!admission.physically_finished());
        assert!(admission.take_git_release().is_none());
        fixture.assert_quarantined(&native);
        let called = Arc::new(AtomicUsize::new(0));
        let body_called = called.clone();
        let rejected = fixture.run(async {
            workers::launch_prepared(
                Some(&producer),
                &prepared,
                move || {
                    body_called.fetch_add(1, Ordering::SeqCst);
                    ToolOutcome::failed("must never execute")
                },
                ToolEffectState::UnknownWorker,
            )
        });
        assert!(rejected.is_err());
        assert_eq!(called.load(Ordering::SeqCst), 0);
        drop((prepared, producer, executable));
        drop(admission);
        // Retiring the final executable carrier is not a quarantine reaper or a release proof.
        fixture.assert_quarantined(&native);
        assert!(native.attempt.reserve_launch().is_err());
    });
}

#[test]
fn git_constructor_wrong_root_is_known_no_start_after_slot_consumption() {
    with_fixture(|fixture| {
        let native = fixture.native();
        let wrong_root = fixture.work.parent().unwrap().join("wrong-workspace");
        std::fs::create_dir(&wrong_root).unwrap();
        let wrong = native.child.with_placement(
            PlacementContext::new(wrong_root.clone(), fixture.home.clone()).unwrap(),
        );
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        assert!(admission.bind_context(&wrong).is_err());
        assert!(matches!(
            admission,
            WorkflowAttemptAdmission::GitReady { slot: None, .. }
        ));
        assert!(admission.physically_finished());
        assert!(admission.take_git_release().is_none());
        assert!(
            admission.bind_context(&native.child).is_err(),
            "consumed slot cannot be retried"
        );
        require_workspace_quiescent(&wrong_root).unwrap();
        fixture.assert_released_and_untouched(Some(&native.owned));
        // A failed prestart binding does not spend the actual lifecycle owner claim.
        let mut valid = WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        drop(valid.bind_context(&native.child).unwrap());
        fixture
            .run(valid.finish_native(deadline(), DriverResult::Cancelled))
            .unwrap();
        assert!(valid.take_git_release().is_some());
        fixture.assert_released_and_untouched(Some(&native.owned));
    });
}

#[test]
fn git_admission_real_worker_join_is_required_after_observer_is_dropped() {
    with_fixture(|fixture| {
        let native = fixture.native();
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        let executable = admission.bind_context(&native.child).unwrap();
        let (tool, prepared) = fixture.prepared_read(&native, &executable);
        let body_ctx = executable.clone();
        let body_prepared = prepared.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        fixture.run(async {
            let observer = workers::launch_prepared(
                executable.tool_workers(),
                &prepared,
                move || {
                    exec_ctx::with_current(body_ctx, || {
                        let _ = started_tx.send(());
                        release_rx
                            .recv_timeout(Duration::from_secs(5))
                            .expect("bounded fixture release");
                        crate::agent::execute_owned_native_fixture_body(tool, &body_prepared)
                    })
                },
                ToolEffectState::UnknownWorker,
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), started_rx)
                .await
                .unwrap()
                .unwrap();
            drop(observer);
            drop((prepared, executable));
            assert!(!admission.physically_finished());
            assert!(admission.take_git_release().is_none());
            let mut finish = Box::pin(admission.finish_native(deadline(), DriverResult::Completed));
            std::future::poll_fn(|cx| {
                assert!(matches!(finish.as_mut().poll(cx), Poll::Pending));
                Poll::Ready(())
            })
            .await;
            assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
            assert!(require_workspace_quiescent(native.owned.root()).is_err());
            release_tx.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(10), finish)
                .await
                .unwrap()
                .unwrap();
        });
        let release = admission.take_git_release().unwrap();
        let (facts, driver) = release.into_joined_for(&native.attempt).unwrap();
        assert_eq!(driver, DriverResult::Completed);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].result, WorkerResult::Returned);
        fixture.assert_released_and_untouched(Some(&native.owned));
    });
}

#[test]
fn git_admission_failed_settled_body_is_released_but_not_a_successful_candidate() {
    with_fixture(|fixture| {
        let native = fixture.native();
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        let executable = admission.bind_context(&native.child).unwrap();
        let (_, prepared) = fixture.prepared_read(&native, &executable);
        let observation = fixture.run(async {
            workers::launch_prepared(
                executable.tool_workers(),
                &prepared,
                || ToolOutcome::failed("typed settled fixture failure"),
                ToolEffectState::UnknownWorker,
            )
            .unwrap()
            .await
            .unwrap()
        });
        assert_eq!(observation.outcome.effect_state, ToolEffectState::Settled);
        assert!(observation.outcome.failure.is_some());
        drop((prepared, executable));
        fixture
            .run(admission.finish_native(deadline(), DriverResult::Completed))
            .unwrap();
        assert!(admission.physically_finished());
        let release = admission.take_git_release().unwrap();
        let error = native
            .owned
            .clone()
            .finish_native(native.attempt.scope(), &native.baseline, release)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("without an eligible candidate"),
            "{error:#}"
        );
        assert_ne!(
            crate::agent::tool_error_effect_state(&error),
            ToolEffectState::UnknownWorker
        );
        assert!(admission.take_git_release().is_none());
        assert!(native.attempt.reserve_launch().is_err());
        fixture.assert_released_and_untouched(Some(&native.owned));
    });
}

#[test]
fn git_admission_unknown_body_remains_unknown_after_actual_observation_and_carrier_drop() {
    with_fixture(|fixture| {
        let native = fixture.native();
        let mut admission =
            WorkflowAttemptAdmission::new_git(fixture.slot(), native.attempt.clone());
        let executable = admission.bind_context(&native.child).unwrap();
        let (_, prepared) = fixture.prepared_read(&native, &executable);
        let observation = fixture.run(async {
            workers::launch_prepared(
                executable.tool_workers(),
                &prepared,
                || {
                    let mut outcome = ToolOutcome::failed("actual body cannot certify its effects");
                    outcome.effect_state = ToolEffectState::UnknownWorker;
                    outcome
                },
                ToolEffectState::UnknownWorker,
            )
            .unwrap()
            .await
            .unwrap()
        });
        assert_eq!(
            observation.outcome.effect_state,
            ToolEffectState::UnknownWorker
        );
        drop((observation, prepared, executable));
        let error = fixture
            .run(admission.finish_native(deadline(), DriverResult::Failed))
            .unwrap_err();
        assert_eq!(
            crate::agent::tool_error_effect_state(&error),
            ToolEffectState::UnknownWorker
        );
        assert!(matches!(admission, WorkflowAttemptAdmission::GitUnknown));
        assert!(!admission.physically_finished());
        assert!(admission.take_git_release().is_none());
        fixture.assert_quarantined(&native);
        assert!(fixture
            .run(admission.finish_native(deadline(), DriverResult::Completed))
            .is_err());
        assert!(!admission.physically_finished());
        assert!(admission.take_git_release().is_none());
        drop(admission);
        fixture.assert_quarantined(&native);
        assert!(native.attempt.reserve_launch().is_err());
    });
}
