use super::*;
use crate::agent::tools::{PreparedToolCall, StartedSafeCall, ToolObservation};
use crate::core::cancel::TurnCancel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn success() -> ToolOutcome {
    ToolOutcome {
        observation: ToolObservation::text("actual result"),
        failure: None,
        effect_state: ToolEffectState::Settled,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

struct Released(Arc<AtomicBool>);
impl Drop for Released {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_observer_expiry_cancels_only_child_and_drains_actual_body() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let parent = TurnCancel::new();
    let child = parent.child();
    let sibling = parent.child();
    let body_cancel = child.clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let released = Arc::new(AtomicBool::new(false));
    let witness = Released(released.clone());
    let call = StartedSafeCall::start_delegated(
        "expired-child".into(),
        PreparedToolCall::new(serde_json::json!({})),
        child.clone(),
        Instant::now() + Duration::from_secs(1),
        Some(&producer),
        move || {
            let _witness = witness;
            entered_tx.send(()).unwrap();
            tokio::runtime::Handle::current().block_on(body_cancel.cancelled());
            success()
        },
    );
    tokio::time::timeout_at(deadline(), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let observed = tokio::time::timeout_at(deadline(), call)
        .await
        .unwrap()
        .unwrap();
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
    assert!(!sibling.is_cancelled());
    assert_eq!(observed.effect_state, ToolEffectState::Settled);
    assert!(observed.text.contains("partial report:\nactual result"));
    // A subsequent body still has admission; observer completion never closes the owner.
    let next = StartedSafeCall::start_delegated(
        "healthy-sibling".into(),
        PreparedToolCall::new(serde_json::json!({})),
        sibling,
        deadline(),
        Some(&producer),
        success,
    );
    assert!(next.await.unwrap().failure.is_none());
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 2);
    assert!(released.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_observer_drop_signals_cancel_but_cannot_release_blocked_body() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let parent = TurnCancel::new();
    let child = parent.child();
    let sibling = parent.child();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let witness = Released(released.clone());
    let call = StartedSafeCall::start_delegated(
        "blocked-child".into(),
        PreparedToolCall::new(serde_json::json!({})),
        child.clone(),
        deadline(),
        Some(&producer),
        move || {
            let _witness = witness;
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            success()
        },
    );
    tokio::time::timeout_at(deadline(), entered_rx)
        .await
        .unwrap()
        .unwrap();
    drop(call);
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
    assert!(!sibling.is_cancelled());
    assert!(owner.close_and_drain(Instant::now()).await.is_err());
    assert_eq!(owner.pending.len(), 1);
    assert!(!released.load(Ordering::Acquire));
    release_tx.send(()).unwrap();
    assert_eq!(owner.close_and_drain(deadline()).await.unwrap()._workers, 1);
    assert!(released.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_drains_aborted_observer_and_retains_joins_after_timeout() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let witness = Released(released.clone());
    let call = StartedSafeCall::start(
        "unadopted-eager".into(),
        PreparedToolCall::new(serde_json::json!({})),
        TurnCancel::new(),
        deadline(),
        Some(&producer),
        move || {
            let _witness = witness;
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            success()
        },
    );
    tokio::time::timeout_at(deadline(), entered_rx)
        .await
        .unwrap()
        .unwrap();
    call.abort();
    assert!(call.await.is_err());
    let error = owner.close_and_drain(Instant::now()).await.unwrap_err();
    assert!(error.downcast_ref::<ToolEffectsUnsettled>().is_some());
    assert_eq!(
        owner.pending.len(),
        1,
        "timeout must not detach the owned join"
    );
    assert!(!released.load(Ordering::Acquire));
    release_tx.send(()).unwrap();
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 1);
    assert!(
        released.load(Ordering::Acquire),
        "body captures must be released before join proof"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_retains_actual_body_after_cancelled_observation() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let cancel = TurnCancel::new();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let call = StartedSafeCall::start(
        "cancel".into(),
        PreparedToolCall::new(serde_json::json!({})),
        cancel.clone(),
        deadline(),
        Some(&producer),
        move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            success()
        },
    );
    tokio::time::timeout_at(deadline(), entered_rx)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    let observation = tokio::time::timeout_at(deadline(), call)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation.effect_state, ToolEffectState::UnknownWorker);
    assert!(owner.close_and_drain(Instant::now()).await.is_err());
    release_tx.send(()).unwrap();
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(
        joined._workers, 1,
        "late actual outcome is distinct from observer uncertainty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_observer_drop_never_aborts_mutation_or_loses_capture_release() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let witness = Released(released.clone());
    let call = StartedSafeCall::start_settling_mutation(
        "mutation".into(),
        PreparedToolCall::new(serde_json::json!({})),
        TurnCancel::new(),
        deadline(),
        Some(&producer),
        move || {
            let _witness = witness;
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            success()
        },
    );
    tokio::time::timeout_at(deadline(), entered_rx)
        .await
        .unwrap()
        .unwrap();
    // Even direct cancellation of the disposable watcher cannot reach the join supervisor.
    call.handle.abort();
    drop(call);
    assert!(owner.close_and_drain(Instant::now()).await.is_err());
    assert!(!released.load(Ordering::Acquire));
    release_tx.send(()).unwrap();
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 1);
    assert!(released.load(Ordering::Acquire));
}

#[tokio::test]
async fn body_owner_closed_or_dropped_admission_refuses_before_spawn() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 0);
    assert!(launch(
        Some(&producer),
        || panic!("closed producer ran"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
    drop(owner);
    assert!(launch(
        Some(&producer),
        || panic!("expired producer ran"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_drains_all_bodies_but_never_certifies_panic_or_uncertain_effects() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let panic = launch(
        Some(&producer),
        || panic!("owned body fault"),
        ToolEffectState::PossiblyApplied(None),
    )
    .unwrap();
    let uncertain = launch(
        Some(&producer),
        || {
            ToolOutcome::failed_with_effect_state(
                "owned unresolved effect",
                ToolEffectState::UnknownWorker,
            )
        },
        ToolEffectState::UnknownWorker,
    )
    .unwrap();
    assert_eq!(
        panic.await.unwrap().outcome.effect_state,
        ToolEffectState::PossiblyApplied(None)
    );
    assert_eq!(
        uncertain.await.unwrap().outcome.effect_state,
        ToolEffectState::UnknownWorker
    );
    assert!(owner.close_and_drain(deadline()).await.is_err());
    assert_eq!(owner.joined, 2);
    assert!(owner.pending.is_empty());
    assert!(
        owner.close_and_drain(deadline()).await.is_err(),
        "uncertainty must remain sticky"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_registration_race_is_either_joined_or_refused_before_execution() {
    for _ in 0..24 {
        let mut owner = ToolWorkerOwner::new();
        let producer = owner.producer();
        let ran = Arc::new(AtomicBool::new(false));
        let worker_ran = ran.clone();
        let (go_tx, go_rx) = oneshot::channel();
        let contender = tokio::spawn(async move {
            go_rx.await.unwrap();
            launch(
                Some(&producer),
                move || {
                    worker_ran.store(true, Ordering::Release);
                    success()
                },
                ToolEffectState::UnknownWorker,
            )
        });
        go_tx.send(()).unwrap();
        let joined = owner.close_and_drain(deadline()).await.unwrap();
        match tokio::time::timeout_at(deadline(), contender)
            .await
            .unwrap()
            .unwrap()
        {
            Ok(observation) => {
                observation.await.unwrap();
                assert_eq!(
                    joined._workers, 1,
                    "admitted body cannot be omitted from closed inventory"
                );
                assert!(ran.load(Ordering::Acquire));
            }
            Err(_) => {
                assert_eq!(joined._workers, 0);
                assert!(
                    !ran.load(Ordering::Acquire),
                    "refused registration cannot run"
                );
            }
        }
    }
}

#[tokio::test]
async fn body_owner_actual_completion_does_not_wait_for_async_supervisor_forwarding() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let mut observation = launch(Some(&producer), success, ToolEffectState::UnknownWorker).unwrap();
    // Run the supervisor once so it can start its blocking body; then deliberately stop driving
    // this current-thread runtime. Delivery must not need the async supervisor to be polled again.
    tokio::task::yield_now().await;
    let bound = std::time::Instant::now() + Duration::from_secs(5);
    let joined = loop {
        match observation.try_recv() {
            Ok(joined) => break joined,
            Err(oneshot::error::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < bound,
                    "actual completion waited on async forwarding"
                );
                std::thread::yield_now();
            }
            Err(error) => panic!("actual completion lost: {error}"),
        }
    };
    assert_eq!(joined.outcome.text, "actual result");
    // The blocking body delivered its observation, but its supervisor may already have returned
    // before this assertion. Only close_and_drain is the stable join boundary.
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_drains_descendant_even_after_parent_body_returned() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let child_producer = producer.clone();
    let (admitted_tx, admitted_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let witness = Released(released.clone());
    let parent = launch(
        Some(&producer),
        move || {
            let descendant = launch(
                Some(&child_producer),
                move || {
                    let _witness = witness;
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    success()
                },
                ToolEffectState::UnknownWorker,
            )
            .unwrap();
            drop(descendant);
            admitted_tx.send(()).unwrap();
            success()
        },
        ToolEffectState::UnknownWorker,
    )
    .unwrap();
    tokio::time::timeout_at(deadline(), admitted_rx)
        .await
        .unwrap()
        .unwrap();
    parent.await.unwrap();
    assert!(owner.close_and_drain(Instant::now()).await.is_err());
    assert!(!released.load(Ordering::Acquire));
    release_tx.send(()).unwrap();
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 2);
    assert!(released.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_inventory_bound_refuses_next_body_without_starting_it() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let mut observations = Vec::new();
    for _ in 0..MAX_BODIES {
        observations
            .push(launch(Some(&producer), success, ToolEffectState::UnknownWorker).unwrap());
    }
    assert!(launch(
        Some(&producer),
        || panic!("over-quota body ran"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
    for observation in observations {
        observation.await.unwrap();
    }
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, MAX_BODIES);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_owner_context_is_a_weak_producer_not_a_self_ownership_cycle() {
    let mut owner = ToolWorkerOwner::new();
    let producer = owner.producer();
    let ctx =
        crate::core::exec_ctx::ExecutionContext::default().with_tool_workers(producer.clone());
    let body = launch(
        ctx.tool_workers(),
        {
            let ctx = ctx.clone();
            move || {
                crate::core::exec_ctx::with_current(ctx, || {
                    assert!(crate::core::exec_ctx::current()
                        .unwrap()
                        .tool_workers()
                        .is_some());
                    success()
                })
            }
        },
        ToolEffectState::UnknownWorker,
    )
    .unwrap();
    drop(body);
    let joined = owner.close_and_drain(deadline()).await.unwrap();
    assert_eq!(joined._workers, 1);
    drop(owner);
    assert!(
        producer.admission.upgrade().is_none(),
        "retained contexts cannot retain their supervisor"
    );
    assert!(launch(
        ctx.tool_workers(),
        || panic!("orphan context ran"),
        ToolEffectState::UnknownWorker
    )
    .is_err());
}
