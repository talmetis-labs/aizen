//! The post-turn learning passes, off the critical path (E4.4).
//!
//! Quality plan M6: the end-of-turn secretary and the persona reflection ran INLINE after a turn,
//! on the main coding model, each under a 300 s ceiling — the prompt came back only when they
//! did, on 39 % of turns. They read the turn and write the store; nothing in the next turn waits
//! on them except recall, which would like to see what the last turn taught. So the passes are
//! spawned as a background task when the turn ends, and the NEXT turn joins them for at most
//! [`DRAIN_JOIN`] before it builds its prompt: a fast pass lands in time, a slow one keeps going
//! and lands later, and the prompt is never held for a model call that is not the user's.
//!
//! Auto-compaction is NOT queued here — it rewrites the history the next prompt is built from.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How long the next turn waits for the previous turn's learning before going ahead without it.
pub const DRAIN_JOIN: Duration = Duration::from_secs(5);

/// The REPL's queue. One per process: the passes write one store.
pub static LEARNING: LearningQueue = LearningQueue::new();

/// Counts detached learning tasks so a later turn can wait for them without holding a handle.
pub struct LearningQueue {
    in_flight: AtomicUsize,
}

/// Owned by the spawned future before its first poll, so panic, cancellation and runtime shutdown
/// all release the count just like normal completion.
struct InFlightGuard(&'static LearningQueue);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl LearningQueue {
    pub const fn new() -> Self {
        LearningQueue {
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Run `work` in the background and count it until it finishes. The task is detached: the REPL
    /// does not hold it, a panic inside it is contained by the runtime, and its lines still reach
    /// the transcript through `tui::emit_line`.
    #[cfg(test)]
    pub fn spawn<F>(&'static self, work: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let ctx = crate::agent::hooks::automation_context(None);
        self.spawn_in_context(&ctx, work);
    }

    /// Driver callers pass the originating context explicitly. Discard restricted/stale work
    /// before counting or scheduling it; never keep a backlog to replay after leaving planning.
    pub(crate) fn spawn_in_context<F>(
        &'static self,
        ctx: &crate::core::exec_ctx::ExecutionContext,
        work: F,
    ) where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let ctx = crate::agent::hooks::automation_context(Some(ctx));
        let Ok(ctx) = crate::agent::hooks::admit_automation(&ctx) else {
            return;
        };
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let guard = InFlightGuard(self);
        tokio::spawn(async move {
            let _guard = guard;
            // Bind the complete originating context on each poll. Checklist-only task-local
            // binding does not pin project config/retrieval for detached work after navigation.
            let task = ctx.task().cloned();
            let work = crate::core::exec_ctx::with_current_future(ctx, work);
            if let Some(task) = task {
                crate::agent::todo::with_context(task, work).await;
            } else {
                work.await;
            }
        });
    }

    /// Independent owners must not queue their network work behind each other's model calls.
    /// Each job keeps its own count/guard, including when its sibling panics.
    #[cfg(test)]
    pub fn spawn_independent<I, P>(&'static self, identity: I, provider: P)
    where
        I: std::future::Future<Output = ()> + Send + 'static,
        P: std::future::Future<Output = ()> + Send + 'static,
    {
        let ctx = crate::agent::hooks::automation_context(None);
        self.spawn_independent_in_context(&ctx, identity, provider);
    }

    pub(crate) fn spawn_independent_in_context<I, P>(
        &'static self,
        ctx: &crate::core::exec_ctx::ExecutionContext,
        identity: I,
        provider: P,
    ) where
        I: std::future::Future<Output = ()> + Send + 'static,
        P: std::future::Future<Output = ()> + Send + 'static,
    {
        let ctx = crate::agent::hooks::automation_context(Some(ctx));
        let Ok(ctx) = crate::agent::hooks::admit_automation(&ctx) else {
            return;
        };
        self.spawn_in_context(&ctx, provider);
        self.spawn_in_context(&ctx, identity);
    }

    /// How many learning tasks are still running.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Wait until every queued task has finished, or `limit` has passed. `false` means a slow pass
    /// is still running and the caller went ahead without it. Polls rather than parks: the wait is
    /// short and rare, and a poll cannot miss a wake-up.
    pub async fn drain(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while self.in_flight() > 0 {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        true
    }
}

impl Default for LearningQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> &'static LearningQueue {
        Box::leak(Box::new(LearningQueue::new()))
    }

    #[tokio::test]
    async fn planning_jobs_are_not_scheduled_or_replayed_after_exit() {
        use crate::core::session_control::ExecutionMode;
        let q = fresh();
        let task = crate::agent::todo::TaskContext::private();
        let control = task.control();
        control
            .transition(ExecutionMode::Planning, None, |_| Ok(()))
            .unwrap();
        crate::agent::todo::with_context(task, async {
            q.spawn_independent(
                async { panic!("planning identity learning must not be polled") },
                async { panic!("planning provider learning must not be polled") },
            );
            assert_eq!(q.in_flight(), 0, "deny before scheduling either future");
        })
        .await;
        control
            .transition(ExecutionMode::Normal, None, |_| Ok(()))
            .unwrap();
        assert!(q.drain(Duration::ZERO).await);
        assert_eq!(q.in_flight(), 0, "no backlog survives explicit exit");
    }

    #[tokio::test]
    async fn stale_and_blocked_jobs_are_dropped_before_scheduling() {
        use crate::core::{exec_ctx::ExecutionContext, session_control::ExecutionMode};
        let q = fresh();
        let task = crate::agent::todo::TaskContext::private();
        let control = task.control();
        let stale = ExecutionContext::default().with_task(task.clone());
        control
            .transition(ExecutionMode::Normal, None, |_| Ok(()))
            .unwrap();
        q.spawn_in_context(&stale, async { panic!("stale work must not run") });
        let blocked = ExecutionContext::default().with_task(task);
        assert!(control
            .transition(ExecutionMode::Planning, None, |_| {
                anyhow::bail!("fixture disk failure")
            })
            .is_err());
        q.spawn_in_context(&blocked, async { panic!("blocked work must not run") });
        assert_eq!(q.in_flight(), 0);
    }

    #[tokio::test]
    async fn background_learning_retains_owner_until_settled() {
        use crate::core::{exec_ctx::ExecutionContext, session_control::ExecutionMode};
        let q = fresh();
        let task = crate::agent::todo::TaskContext::private();
        let control = task.control();
        let ctx = ExecutionContext::default().with_task(task);
        let (finish, wait) = tokio::sync::oneshot::channel();
        q.spawn_in_context(&ctx, async move {
            assert!(crate::agent::todo::bound_context().is_some());
            wait.await.unwrap();
        });
        assert!(control
            .transition(ExecutionMode::Planning, None, |_| {
                panic!("must refuse before persisting while background worker is admitted")
            })
            .is_err());
        finish.send(()).unwrap();
        assert!(q.drain(Duration::from_secs(2)).await);
        control
            .transition(ExecutionMode::Planning, None, |_| Ok(()))
            .unwrap();
    }

    #[tokio::test]
    async fn detached_jobs_pin_originating_placement_on_every_poll() {
        use crate::core::{config, exec_ctx, file_placement::PlacementContext};
        let q = fresh();
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let old = fixture.path().join("old");
        let new = fixture.path().join("new");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        let placement = PlacementContext::new(old.clone(), fixture.path().join("home")).unwrap();
        let origin = exec_ctx::ExecutionContext::default()
            .with_placement(placement.clone())
            .with_task(crate::agent::todo::TaskContext::private());
        let child = origin.with_placement(placement.fork_for(new.clone(), "new").unwrap());
        let token = origin.workspace_binding().cloned();
        let cwd = std::env::current_dir().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        q.spawn_in_context(&origin, async move {
            assert_eq!(config::project_root(), old);
            assert_eq!(
                exec_ctx::current().unwrap().workspace_binding(),
                token.as_ref()
            );
            wait.await.unwrap();
            for _ in 0..4 {
                tokio::task::yield_now().await;
                assert_eq!(config::project_root(), old);
                assert_eq!(
                    exec_ctx::current().unwrap().workspace_binding(),
                    token.as_ref()
                );
            }
        });
        exec_ctx::with_current_future(child, async {
            assert_eq!(config::project_root(), new);
            release.send(()).unwrap();
            assert!(q.drain(Duration::from_secs(2)).await);
            assert_eq!(config::project_root(), new);
        })
        .await;
        assert!(exec_ctx::current().is_none());
        assert_eq!(std::env::current_dir().unwrap(), cwd);
    }

    #[tokio::test]
    async fn a_fast_pass_lands_before_the_next_turn() {
        let q = fresh();
        q.spawn(async { tokio::time::sleep(Duration::from_millis(30)).await });
        assert_eq!(q.in_flight(), 1);
        assert!(
            q.drain(Duration::from_secs(2)).await,
            "a 30 ms pass lands inside the join"
        );
        assert_eq!(q.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_slow_pass_does_not_hold_the_next_turn_and_lands_later() {
        let q = fresh();
        q.spawn(async { tokio::time::sleep(Duration::from_millis(300)).await });
        let started = std::time::Instant::now();
        assert!(
            !q.drain(Duration::from_millis(60)).await,
            "the join gives up, the task keeps running"
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "went ahead promptly"
        );
        assert_eq!(q.in_flight(), 1);
        assert!(
            q.drain(Duration::from_secs(2)).await,
            "and a later drain sees it land"
        );
    }

    #[tokio::test]
    async fn provider_learning_starts_while_identity_learning_is_still_waiting() {
        let q = fresh();
        let (release, blocked) = tokio::sync::oneshot::channel::<()>();
        let (started, provider) = tokio::sync::oneshot::channel();
        q.spawn_independent(
            async move {
                let _ = blocked.await;
            },
            async move {
                started.send(()).unwrap();
            },
        );
        tokio::time::timeout(Duration::from_secs(2), provider)
            .await
            .expect("provider must not wait for secretary or reflection")
            .unwrap();
        assert_eq!(q.in_flight(), 1, "identity learning is still pending");
        release.send(()).unwrap();
        assert!(q.drain(Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn panicked_learning_releases_the_count() {
        let q = fresh();
        q.spawn(async { panic!("fixture learning panic") });
        assert!(q.drain(Duration::from_secs(2)).await);
        assert_eq!(q.in_flight(), 0);
    }

    #[test]
    fn runtime_shutdown_releases_unpolled_and_pending_learning() {
        let unpolled = fresh();
        let pending = fresh();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started, waiting) = tokio::sync::oneshot::channel();
            pending.spawn(async move {
                started.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            waiting.await.unwrap();
        });
        {
            let _entered = runtime.enter();
            unpolled.spawn(std::future::pending());
        }
        assert_eq!(pending.in_flight(), 1);
        assert_eq!(unpolled.in_flight(), 1);
        drop(runtime);
        assert_eq!(pending.in_flight(), 0);
        assert_eq!(unpolled.in_flight(), 0);
    }

    #[tokio::test]
    async fn an_empty_queue_drains_at_once() {
        let q = fresh();
        let started = std::time::Instant::now();
        assert!(q.drain(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
