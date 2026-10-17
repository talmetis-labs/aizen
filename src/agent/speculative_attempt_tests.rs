use super::*;
use crate::agent::task_tool::SubagentGate;
use crate::agent::tools::workers;
use crate::agent::tools::{ToolEffectState, ToolEffectsUnsettled, ToolObservation, ToolOutcome};
use crate::core::file_placement::tests::TestDir;
use crate::core::speculation::store::BaseSnapshot;
use crate::core::workspace_txn::{
    require_workspace_quiescent, with_test_lock_home, PhysicalEffectLease, WorkspaceIdentity,
    WorkspaceWriterLease,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

const OWNER: &str = "controller/task/native/attempt/1";

/// Real native resources with a value-scoped admission gate, not a fabricated slot.
struct Fixture {
    _temp: TestDir,
    work: PathBuf,
    locks: PathBuf,
    scope: NativeWriteScope,
    base: BaseSnapshot,
    overlay: OverlayHandle,
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
        std::fs::write(work.join("read.txt"), b"read-only base").unwrap();
        std::fs::write(work.join("write.txt"), b"writable base").unwrap();
        let work = std::fs::canonicalize(work).unwrap();
        let scope = NativeWriteScope::resolve_access(
            &work,
            &["read.txt".to_owned()],
            &["write.txt".to_owned()],
        )
        .unwrap();
        let identity = WorkspaceIdentity::discover(&work).unwrap();
        let base = with_test_lock_home(&locks, || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "native attempt fixture",
                "controller",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "native attempt capture",
                "controller",
            )
            .unwrap();
            BaseSnapshot::capture(
                &identity,
                std::slice::from_ref(&scope),
                &store,
                &writer,
                &physical,
            )
            .unwrap()
        });
        let overlay = base.create_overlay(OWNER, &scope).unwrap();
        let active: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
        let gate = SubagentGate::isolated(active, 1, locks.join("slots"));
        Self {
            _temp: temp,
            work,
            locks,
            scope,
            base,
            overlay,
            active,
            gate,
        }
    }

    fn owner(&self) -> AttemptOwner {
        with_test_lock_home(&self.locks, || {
            AttemptOwner::new(
                self.gate.try_acquire().unwrap(),
                &self.work,
                OWNER,
                self.overlay.clone(),
                &self.scope,
            )
            .unwrap()
        })
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::default()
            .with_resource_scope(OWNER)
            .with_native_scope(self.scope.clone())
            .unwrap()
            .with_overlay(self.overlay.clone())
            .unwrap()
    }

    fn assert_retained(&self) {
        assert_eq!(self.active.load(Ordering::SeqCst), 1);
        assert!(
            self.gate.try_acquire().is_err(),
            "unresolved admission remains charged"
        );
        assert!(require_workspace_quiescent(&self.work).is_err());
        let quarantine = QUARANTINE.lock().unwrap_or_else(|error| error.into_inner());
        assert!(quarantine
            .iter()
            .any(|state| state.overlay.same_handle(&self.overlay)));
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn settled() -> ToolOutcome {
    ToolOutcome {
        observation: ToolObservation::text("native body completed"),
        failure: None,
        effect_state: ToolEffectState::Settled,
    }
}

#[tokio::test]
async fn full_native_release_retires_context_and_releases_owned_slot_and_activity() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let state = owner.state.as_ref().unwrap();
    let activity = Arc::downgrade(&state.activity);
    let carriers = state.carriers.clone();
    let context = owner.bind_context(&fixture.context()).unwrap();
    assert_eq!(carriers.live(), 1);
    assert!(activity.upgrade().is_some());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
    let producer = context.tool_workers().unwrap().clone();
    let actual = workers::launch(
        Some(&producer),
        move || crate::core::exec_ctx::with_current(context, settled),
        ToolEffectState::UnknownWorker,
    )
    .unwrap();
    actual.await.unwrap();
    let released = owner.finish(deadline()).await.unwrap();
    assert_eq!(carriers.live(), 0);
    assert!(activity.upgrade().is_none());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    require_workspace_quiescent(&fixture.work).unwrap();
    let next_slot = fixture.gate.try_acquire().unwrap();
    drop(next_slot);
    released.consume_for(&fixture.overlay).unwrap();
}

#[tokio::test]
async fn pending_carrier_timeout_is_unknown_and_quarantines_all_resources() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let context = owner.bind_context(&fixture.context()).unwrap();
    let producer = context.tool_workers().unwrap().clone();
    let error = match owner.finish(Instant::now()).await {
        Ok(_) => panic!("live carrier must not release native resources"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<ToolEffectsUnsettled>().is_some());
    assert!(format!("{error:#}").contains("carriers remain live"));
    fixture.assert_retained();
    assert!(workers::launch(
        Some(&producer),
        || panic!("closed admission ran"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
    drop(context);
    // Carrier retirement after abandonment does not run a reaper or mint a release token.
    fixture.assert_retained();
}

#[tokio::test]
async fn cancelling_consuming_finish_retains_slot_activity_and_closes_admission() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let context = owner.bind_context(&fixture.context()).unwrap();
    let producer = context.tool_workers().unwrap().clone();
    let mut finish = Box::pin(owner.finish(deadline()));
    std::future::poll_fn(|cx| {
        assert!(matches!(finish.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    drop(finish);
    fixture.assert_retained();
    assert!(workers::launch(
        Some(&producer),
        || panic!("cancelled owner admitted a body"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
    drop(context);
    fixture.assert_retained();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_native_body_timeout_never_releases_admission_or_activity() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let context = owner.bind_context(&fixture.context()).unwrap();
    let producer = context.tool_workers().unwrap().clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let result = workers::launch(
        Some(&producer),
        move || {
            crate::core::exec_ctx::with_current(context, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                settled()
            })
        },
        ToolEffectState::UnknownWorker,
    )
    .unwrap();
    tokio::time::timeout_at(deadline(), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let error = match owner.finish(Instant::now()).await {
        Ok(_) => panic!("running physical body must not release resources"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<ToolEffectsUnsettled>().is_some());
    fixture.assert_retained();
    assert!(workers::launch(
        Some(&producer),
        || panic!("timed-out owner admitted body"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
    release_tx.send(()).unwrap();
    tokio::time::timeout_at(deadline(), result)
        .await
        .unwrap()
        .unwrap();
    fixture.assert_retained();
}

#[tokio::test]
async fn released_token_rejects_sibling_overlay_even_with_identical_full_scope() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let sibling = fixture.base.create_overlay(OWNER, &fixture.scope).unwrap();
    let token = owner.finish(deadline()).await.unwrap();
    assert!(token.consume_for(&sibling).is_err());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn released_overlay_refuses_new_body_and_exact_token_remains_valid() {
    let fixture = Fixture::new();
    let token = fixture.owner().finish(deadline()).await.unwrap();
    assert!(fixture.overlay.body_lease(OWNER).is_err());
    token.consume_for(&fixture.overlay).unwrap();
}

#[tokio::test]
async fn duplicate_owner_cannot_release_an_overlay_while_first_attempt_is_live() {
    let fixture = Fixture::new();
    let first = fixture.owner();
    let first_context = first.bind_context(&fixture.context()).unwrap();
    let active: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    let duplicate_gate = SubagentGate::isolated(active, 1, fixture.locks.join("duplicate-slots"));
    assert!(AttemptOwner::new(
        duplicate_gate.try_acquire().unwrap(),
        &fixture.work,
        OWNER,
        fixture.overlay.clone(),
        &fixture.scope,
    )
    .is_err());
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "refused duplicate releases only its own unadmitted slot"
    );
    assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
    assert!(require_workspace_quiescent(&fixture.work).is_err());
    drop(first_context);
    let token = first.finish(deadline()).await.unwrap();
    token.consume_for(&fixture.overlay).unwrap();
    assert!(
        AttemptOwner::new(
            duplicate_gate.try_acquire().unwrap(),
            &fixture.work,
            OWNER,
            fixture.overlay.clone(),
            &fixture.scope,
        )
        .is_err(),
        "released overlay cannot be re-admitted for a fresh idle token"
    );
}

#[test]
fn binding_rejects_scope_reduction_owner_and_overlay_substitution() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let read_only = NativeWriteScope::resolve_access(
        &fixture.work,
        &["read.txt".to_owned(), "write.txt".to_owned()],
        &[],
    )
    .unwrap();
    let reduced = ExecutionContext::default()
        .with_resource_scope(OWNER)
        .with_native_scope(read_only.clone())
        .unwrap()
        .with_overlay(fixture.overlay.clone())
        .unwrap();
    assert!(owner.bind_context(&reduced).is_err());
    let other_owner = fixture
        .context()
        .with_resource_scope("controller/task/other");
    assert!(owner.bind_context(&other_owner).is_err());
    let sibling = fixture.base.create_overlay(OWNER, &fixture.scope).unwrap();
    let other_overlay = ExecutionContext::default()
        .with_resource_scope(OWNER)
        .with_native_scope(fixture.scope.clone())
        .unwrap()
        .with_overlay(sibling)
        .unwrap();
    assert!(owner.bind_context(&other_overlay).is_err());
    drop(owner);
    fixture.assert_retained();
}

#[test]
fn constructor_rejects_narrowed_scope_and_wrong_workspace_root_before_activity() {
    let fixture = Fixture::new();
    let read_only = NativeWriteScope::resolve_access(
        &fixture.work,
        &["read.txt".to_owned(), "write.txt".to_owned()],
        &[],
    )
    .unwrap();
    // A narrowing overlay check alone would allow this constructor.  Full exact scope does not.
    assert!(AttemptOwner::new(
        fixture.gate.try_acquire().unwrap(),
        &fixture.work,
        OWNER,
        fixture.overlay.clone(),
        &read_only,
    )
    .is_err());
    let wrong_root = fixture.work.parent().unwrap().join("different-workspace");
    std::fs::create_dir(&wrong_root).unwrap();
    assert!(AttemptOwner::new(
        fixture.gate.try_acquire().unwrap(),
        &wrong_root,
        OWNER,
        fixture.overlay.clone(),
        &fixture.scope,
    )
    .is_err());
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    require_workspace_quiescent(&fixture.work).unwrap();
}
