//! Admission-state regression tests using real slots, native scopes, and private overlays.

use super::*;
use crate::agent::task_tool::SubagentGate;
use crate::agent::tools::workers;
use crate::agent::tools::ToolEffectState;
use crate::core::cancel::TurnCancel;
use crate::core::exec_ctx::ExecutionContext;
use crate::core::file_placement::tests::TestDir;
use crate::core::native_scope::NativeWriteScope;
use crate::core::speculation::store::{BaseSnapshot, OverlayHandle};
use crate::core::workspace_txn::{
    require_workspace_quiescent, with_test_lock_home, PhysicalEffectLease, WorkspaceIdentity,
    WorkspaceWriterLease,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

const OWNER: &str = "controller/task/admission/attempt/1";
const LIVE_BYTES: &[u8] = b"untouched native live source\n";

struct Fixture {
    _temp: TestDir,
    work: PathBuf,
    locks: PathBuf,
    store: PathBuf,
    active: &'static AtomicUsize,
    gate: SubagentGate,
}

impl Fixture {
    fn new() -> Self {
        let temp = TestDir::new();
        let work = temp.path().join("work");
        let locks = temp.path().join("locks");
        let store = temp.path().join("store");
        for directory in [&work, &locks, &store] {
            std::fs::create_dir(directory).unwrap();
        }
        std::fs::write(work.join("owned.txt"), LIVE_BYTES).unwrap();
        let work = std::fs::canonicalize(work).unwrap();
        let active: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
        let gate = SubagentGate::isolated(active, 1, locks.join("slots"));
        Self {
            _temp: temp,
            work,
            locks,
            store,
            active,
            gate,
        }
    }

    fn slot(&self) -> crate::agent::task_tool::SubagentSlot {
        self.gate.try_acquire().unwrap()
    }

    fn assert_released_and_untouched(&self) {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        require_workspace_quiescent(&self.work).unwrap();
        assert_eq!(
            std::fs::read(self.work.join("owned.txt")).unwrap(),
            LIVE_BYTES
        );
        drop(self.slot());
    }

    fn native(&self) -> (NativeWriteScope, OverlayHandle, ExecutionContext) {
        let scope =
            NativeWriteScope::resolve_access(&self.work, &[], &["owned.txt".to_owned()]).unwrap();
        let identity = WorkspaceIdentity::discover(&self.work).unwrap();
        let base = with_test_lock_home(&self.locks, || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "workflow admission fixture",
                "controller",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "workflow admission capture",
                "controller",
            )
            .unwrap();
            BaseSnapshot::capture(
                &identity,
                std::slice::from_ref(&scope),
                &self.store,
                &writer,
                &physical,
            )
            .unwrap()
        });
        let overlay = base.create_overlay(OWNER, &scope).unwrap();
        let lifecycle = crate::core::speculation::lifecycle::CohortLifecycle::freeze(
            &base,
            "controller",
            vec![(
                crate::core::speculation::workers::AttemptMember {
                    attempt: 1,
                    owner: OWNER.to_owned(),
                    overlay: overlay.id().to_owned(),
                    scope_sha256: crate::core::speculation::workers::scope_sha256(
                        scope.resources().iter().map(|resource| {
                            (
                                resource.key(),
                                resource.path(),
                                scope.write_keys().iter().any(|key| key == resource.key()),
                            )
                        }),
                    )
                    .unwrap(),
                },
                scope.clone(),
            )],
        )
        .unwrap();
        overlay
            .bind_lifecycle(lifecycle.attempt(0, &scope).unwrap())
            .unwrap();
        let context = ExecutionContext::default()
            .with_resource_scope(OWNER)
            .with_native_scope(scope.clone())
            .unwrap()
            .with_overlay(overlay.clone())
            .unwrap();
        (scope, overlay, context)
    }
}

fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(10)
}

fn task(mode: ExecutionMode, role: &str) -> WorkflowTask {
    WorkflowTask {
        id: "known-not-started".to_owned(),
        role: role.to_owned(),
        execution: mode,
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
) -> TaskOutcome {
    // A malformed URL cannot make an external request. All cases must return from known
    // prestart branches, before the model client is ever constructed/polled for a turn.
    run_one_task_admitted(
        &reqwest::Client::new(),
        "://unused-invalid-endpoint",
        "unused-test-key",
        "unused-test-model",
        crate::core::approval::ApprovalMode::Yolo,
        &fixture.work,
        "2026-10-16",
        task,
        None,
        cancel,
        0,
        &ExecutionContext::default(),
        fixture.slot(),
    )
    .await
}

#[tokio::test]
async fn admitted_wrapper_precancel_preserves_cancelled_without_unknown_worker() {
    for mode in [ExecutionMode::Direct, ExecutionMode::Speculative] {
        let fixture = Fixture::new();
        let cancel = TurnCancel::new();
        cancel.cancel();
        let outcome = admitted_no_network(&fixture, &task(mode, "daedalus"), cancel).await;
        assert_eq!(
            outcome.status, "cancelled",
            "mode {mode:?}: {}",
            outcome.summary
        );
        assert_eq!(outcome.iters, 0);
        assert_eq!((outcome.tokens_in, outcome.tokens_out), (0, 0));
        assert!(!outcome
            .summary
            .contains("physical attempt closure was not certified"));
        fixture.assert_released_and_untouched();
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Isolate configured endpoints/hooks in the owned fixture HOME.
async fn admitted_wrapper_missing_overlay_is_known_admission_error() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let _env = crate::core::config::EnvGuard::set([(
        "AIZEN_HOME",
        fixture.work.parent().unwrap().join("home"),
    )]);
    let outcome = admitted_no_network(
        &fixture,
        &task(ExecutionMode::Speculative, "daedalus"),
        TurnCancel::new(),
    )
    .await;
    assert_eq!(outcome.status, "admission-error", "{}", outcome.summary);
    assert!(outcome.summary.contains("overlay"), "{}", outcome.summary);
    assert_eq!(outcome.iters, 0);
    assert_eq!((outcome.tokens_in, outcome.tokens_out), (0, 0));
    assert!(!outcome
        .summary
        .contains("physical attempt closure was not certified"));
    fixture.assert_released_and_untouched();
}

#[tokio::test]
async fn admitted_wrapper_bad_role_preserves_error_without_unknown_worker() {
    for mode in [ExecutionMode::Direct, ExecutionMode::Speculative] {
        let fixture = Fixture::new();
        let outcome = admitted_no_network(
            &fixture,
            &task(mode, "not-a-real-workflow-role"),
            TurnCancel::new(),
        )
        .await;
        assert_eq!(
            outcome.status, "error",
            "mode {mode:?}: {}",
            outcome.summary
        );
        assert!(outcome.summary.contains("role"), "{}", outcome.summary);
        assert_eq!(outcome.iters, 0);
        assert_eq!((outcome.tokens_in, outcome.tokens_out), (0, 0));
        assert!(!outcome
            .summary
            .contains("physical attempt closure was not certified"));
        fixture.assert_released_and_untouched();
    }
}

#[test]
fn direct_admission_keeps_legacy_arc_retention_behavior() {
    let fixture = Fixture::new();
    let mut admission = WorkflowAttemptAdmission::new(ExecutionMode::Direct, fixture.slot());
    assert!(matches!(admission, WorkflowAttemptAdmission::Direct(_)));
    assert!(admission.physically_finished());
    assert!(admission.take_release().is_none());
    let context = admission
        .bind_context(&fixture.work, &ExecutionContext::default())
        .unwrap();
    assert!(
        !admission.physically_finished(),
        "legacy context retains direct Arc slot"
    );
    assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
    drop(context);
    assert!(admission.physically_finished());
    drop(admission);
    fixture.assert_released_and_untouched();
}

#[tokio::test]
async fn native_admission_ready_running_released_and_taken_token_stay_distinct() {
    let fixture = Fixture::new();
    let (_scope, overlay, context) = fixture.native();
    let mut admission = WorkflowAttemptAdmission::new(ExecutionMode::Speculative, fixture.slot());
    assert!(matches!(
        admission,
        WorkflowAttemptAdmission::NativeReady(Some(_))
    ));
    assert!(
        admission.physically_finished(),
        "ready is known not started"
    );
    assert!(admission.take_release().is_none());
    let executable = admission.bind_context(&fixture.work, &context).unwrap();
    assert!(matches!(
        admission,
        WorkflowAttemptAdmission::NativeRunning(_)
    ));
    assert!(!admission.physically_finished());
    assert!(require_workspace_quiescent(&fixture.work).is_err());
    drop(executable);
    admission
        .finish_native(
            deadline(),
            crate::core::speculation::workers::DriverResult::Completed,
        )
        .await
        .unwrap();
    assert!(matches!(
        admission,
        WorkflowAttemptAdmission::NativeReleased(Some(_))
    ));
    assert!(admission.physically_finished());
    let token = admission
        .take_release()
        .expect("released attempt has one token");
    assert!(matches!(
        admission,
        WorkflowAttemptAdmission::NativeReleased(None)
    ));
    assert!(
        admission.physically_finished(),
        "moving proof must not undo physical closure"
    );
    assert!(admission.take_release().is_none());
    assert!(
        overlay.body_lease(OWNER).is_err(),
        "raw handle cannot reopen released body admission"
    );
    token.consume_for(&overlay).unwrap();
    fixture.assert_released_and_untouched();
}

#[tokio::test]
async fn cancelled_native_finish_stays_unknown_and_retains_slot_and_activity() {
    let fixture = Fixture::new();
    let (_scope, overlay, context) = fixture.native();
    let mut admission = WorkflowAttemptAdmission::new(ExecutionMode::Speculative, fixture.slot());
    let executable = admission.bind_context(&fixture.work, &context).unwrap();
    let producer = executable.tool_workers().unwrap().clone();
    let mut finish = Box::pin(admission.finish_native(
        deadline(),
        crate::core::speculation::workers::DriverResult::Completed,
    ));
    std::future::poll_fn(|cx| {
        assert!(matches!(finish.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    drop(finish);
    assert!(matches!(admission, WorkflowAttemptAdmission::NativeUnknown));
    assert!(!admission.physically_finished());
    assert!(admission.take_release().is_none());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
    assert!(require_workspace_quiescent(&fixture.work).is_err());
    assert!(workers::launch(
        Some(&producer),
        || panic!("unknown attempt must not admit another body"),
        ToolEffectState::UnknownWorker,
    )
    .is_err());
    drop(executable);
    drop(admission);
    assert_eq!(
        fixture.active.load(Ordering::SeqCst),
        1,
        "retiring last carrier is not a quarantine reaper"
    );
    assert!(fixture.gate.try_acquire().is_err());
    assert!(require_workspace_quiescent(&fixture.work).is_err());
    assert_eq!(
        std::fs::read(fixture.work.join("owned.txt")).unwrap(),
        LIVE_BYTES
    );
    // The raw overlay is retained, but no token can be taken from the Unknown enum state.
    assert_eq!(overlay.owner(), OWNER);
}

#[test]
fn native_constructor_wrong_root_is_known_no_start_after_slot_consumption() {
    let fixture = Fixture::new();
    let (_scope, _overlay, context) = fixture.native();
    let wrong_root = fixture.work.parent().unwrap().join("wrong-workspace");
    std::fs::create_dir(&wrong_root).unwrap();
    let mut admission = WorkflowAttemptAdmission::new(ExecutionMode::Speculative, fixture.slot());
    assert!(admission.bind_context(&wrong_root, &context).is_err());
    assert!(matches!(
        admission,
        WorkflowAttemptAdmission::NativeReady(None)
    ));
    assert!(admission.physically_finished());
    assert!(admission.take_release().is_none());
    require_workspace_quiescent(&wrong_root).unwrap();
    fixture.assert_released_and_untouched();
}
