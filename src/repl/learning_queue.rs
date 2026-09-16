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

impl LearningQueue {
    pub const fn new() -> Self {
        LearningQueue {
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Run `work` in the background and count it until it finishes. The task is detached: the REPL
    /// does not hold it, a panic inside it is contained by the runtime, and its lines still reach
    /// the transcript through `tui::emit_line`.
    pub fn spawn<F>(&'static self, work: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            work.await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        });
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
    async fn an_empty_queue_drains_at_once() {
        let q = fresh();
        let started = std::time::Instant::now();
        assert!(q.drain(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
