use super::*;
use crate::agent::todo::{self, TaskContext};
use crate::core::session_control::{ExecutionMode, SessionControl};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[tokio::test]
#[expect(
    clippy::async_yields_async,
    reason = "Return the admitted worker handle to test transition refusal before settlement"
)]
async fn normal_startup_worker_retains_owner_and_blocks_transition_until_settled() {
    let owner = TaskContext::private();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let expected = owner.clone();
    let worker = todo::with_context(owner.clone(), async {
        spawn_startup_automation(async move {
            let bound = todo::bound_context().expect("captured caller owner");
            assert!(bound.same_owner(&expected));
            assert!(crate::core::exec_ctx::current()
                .unwrap()
                .permits_automation());
            ready_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            assert!(todo::bound_context().unwrap().same_owner(&expected));
        })
        .unwrap()
    })
    .await;
    // Admission is captured before scheduling, not lazily at the worker's first poll.
    assert!(owner
        .control()
        .transition(ExecutionMode::Planning, None, |_| {
            panic!("worker admission must block transition")
        })
        .is_err());
    ready_rx.await.unwrap();
    assert!(owner
        .control()
        .transition(ExecutionMode::Planning, None, |_| {
            panic!("worker admission must survive await")
        })
        .is_err());
    finish_tx.send(()).unwrap();
    worker.await.unwrap();
    owner
        .control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .unwrap();
}

#[tokio::test]
async fn restricted_blocked_and_stale_startup_work_is_discarded_without_replay() {
    let launches = Arc::new(AtomicUsize::new(0));
    for blocked in [false, true] {
        let owner = TaskContext::private();
        if blocked {
            assert!(owner
                .control()
                .transition(ExecutionMode::Planning, None, |_| {
                    anyhow::bail!("fixture persistence failure")
                })
                .is_err());
        } else {
            owner
                .control()
                .restore(SessionControl {
                    mode: ExecutionMode::Planning,
                    ..Default::default()
                })
                .unwrap();
        }
        let count = launches.clone();
        todo::with_context(owner.clone(), async {
            assert!(spawn_startup_automation(async move {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .is_none());
        })
        .await;
        let old = crate::core::exec_ctx::ExecutionContext::default().with_task(owner.clone());
        owner
            .control()
            .transition(ExecutionMode::Normal, None, |_| Ok(()))
            .unwrap();
        let count = launches.clone();
        crate::core::exec_ctx::with_current(old, || {
            assert!(spawn_startup_automation(async move {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .is_none());
        });
    }
    tokio::task::yield_now().await;
    assert_eq!(launches.load(Ordering::SeqCst), 0);
}

#[test]
fn restricted_startup_entrypoints_do_not_persist_or_prompt() {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let home = dir.path().join("not-created");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let owner = TaskContext::private();
    owner
        .control()
        .restore(SessionControl {
            mode: ExecutionMode::Planning,
            ..Default::default()
        })
        .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(todo::with_context(owner, async {
            prompt_mcp_trust(1);
            first_run_onboarding().await;
            run_id_migration_once();
            startup_update_probe();
        }));
    assert!(!home.exists());
}
