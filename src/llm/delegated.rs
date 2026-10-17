//! One deadline and send allowance for a delegated model step, including every retry layer.

use anyhow::Result;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Deadline,
    Exhausted,
    Protocol,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Deadline => "delegated model step reached its absolute deadline",
            Self::Exhausted => "delegated model step exhausted its HTTP send budget (3 sends)",
            Self::Protocol => "invalid Chat Completions stream (expected choices/delta, not another API dialect or malformed answer frames)",
        })
    }
}
impl std::error::Error for Failure {}

#[derive(Debug)]
pub(crate) struct Control {
    started: Instant,
    deadline: Instant,
    sends: AtomicUsize,
    row: Option<u64>,
    /// Cheap throttle state, checked before the publishing path: one streaming fragment per token
    /// must not take a lock or repaint the panel.
    last_stage: AtomicUsize,
    last_note_ms: AtomicU64,
}

tokio::task_local! {
    static CURRENT: Arc<Control>;
}

impl Control {
    pub(crate) fn new(cap: Duration, parent: Option<Instant>, row: Option<u64>) -> Arc<Self> {
        let started = Instant::now();
        let deadline = started + cap;
        Arc::new(Self {
            started,
            deadline: parent.map_or(deadline, |p| p.min(deadline)),
            sends: AtomicUsize::new(0),
            row,
            last_stage: AtomicUsize::new(0),
            last_note_ms: AtomicU64::new(0),
        })
    }

    fn check(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(Failure::Deadline.into());
        }
        Ok(())
    }

    fn send(&self) -> Result<()> {
        self.check()?;
        self.sends.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 3).then_some(n + 1)
        }).map_err(|_| Failure::Exhausted)?;
        self.note("sending / waiting for first output");
        Ok(())
    }

    /// Sends spent so far, for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn sends(&self) -> usize {
        self.sends.load(Ordering::Acquire)
    }

    fn note(&self, stage: &'static str) {
        let Some(row) = self.row else { return };
        let stage_id = stage.as_ptr() as usize;
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let previous = self.last_note_ms.load(Ordering::Acquire);
        if self.last_stage.load(Ordering::Acquire) == stage_id
            && elapsed_ms.saturating_sub(previous) < 1_000
        {
            return;
        }
        self.last_stage.store(stage_id, Ordering::Release);
        self.last_note_ms.store(elapsed_ms, Ordering::Release);
        crate::agent::orchestration::note_transport(
            row, self.sends.load(Ordering::Acquire), stage,
        );
    }
}

/// Scope only the HTTP future, never a mutation/settlement future. Dropping a stream still runs
/// its usage guard; blocking tool ownership continues to be managed by the worker registry.
pub(crate) async fn run<T>(control: Option<&Arc<Control>>, future: impl Future<Output = Result<T>>) -> Result<T> {
    let Some(control) = control else { return future.await; };
    control.check()?;
    if control.sends.load(Ordering::Acquire) >= 3 {
        return Err(Failure::Exhausted.into());
    }
    // The inner future is a whole streaming model call. Boxing it keeps this wrapper a pointer
    // wide: nested delegated children run inside `block_in_place` on a worker-thread stack, and
    // holding that state machine inline here made deep nesting overflow it.
    let future = Box::pin(future);
    CURRENT.scope(control.clone(), async move {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(control.deadline.into()) => Err(Failure::Deadline.into()),
            result = future => result,
        }
    }).await
}

pub(crate) fn before_send() -> Result<()> {
    CURRENT.try_with(|c| c.send()).unwrap_or(Ok(()))
}

pub(crate) fn progress() {
    let _ = CURRENT.try_with(|c| c.note("streaming"));
}

pub(crate) async fn sleep(delay: Duration) -> Result<()> {
    if let Ok(control) = CURRENT.try_with(Arc::clone) {
        control.check()?;
        if control.sends.load(Ordering::Acquire) >= 3 {
            return Err(Failure::Exhausted.into());
        }
        control.note("retry backoff");
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(control.deadline.into()) => Err(Failure::Deadline.into()),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    } else {
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

/// Outer retry sleeps share the step deadline too. On expiry the next call returns the typed
/// terminal failure; cancellation keeps its existing distinct outcome in the agent loop.
pub(crate) fn remaining(control: Option<&Arc<Control>>, delay: u64) -> u64 {
    control.map_or(delay, |c| {
        c.note("retry backoff");
        if c.sends.load(Ordering::Acquire) >= 3 { return 0; }
        delay.min(c.deadline.saturating_duration_since(Instant::now()).as_millis() as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn all_retry_layers_share_three_sends() {
        let control = Control::new(Duration::from_secs(1), None, None);
        for _ in 0..3 {
            run(Some(&control), async { before_send() }).await.unwrap();
        }
        let error = run(Some(&control), async { before_send() }).await.unwrap_err();
        assert_eq!(error.downcast_ref::<Failure>(), Some(&Failure::Exhausted));
        assert_eq!(control.sends.load(Ordering::Acquire), 3);
    }

    #[tokio::test]
    async fn pending_call_and_backoff_use_original_parent_deadline() {
        let control = Control::new(Duration::from_secs(300), Some(Instant::now() + Duration::from_millis(25)), None);
        let error = run(Some(&control), async {
            before_send()?;
            sleep(Duration::from_secs(300)).await
        }).await.unwrap_err();
        assert_eq!(error.downcast_ref::<Failure>(), Some(&Failure::Deadline));
        let error = run(Some(&control), async { before_send() }).await.unwrap_err();
        assert_eq!(error.downcast_ref::<Failure>(), Some(&Failure::Deadline));
        assert_eq!(control.sends.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn controls_are_child_local_and_uncontrolled_chat_is_unchanged() {
        let a = Control::new(Duration::from_secs(1), None, None);
        let b = Control::new(Duration::from_secs(1), None, None);
        for _ in 0..3 { run(Some(&a), async { before_send() }).await.unwrap(); }
        run(Some(&b), async { before_send() }).await.unwrap();
        for _ in 0..5 { before_send().unwrap(); }
        assert_eq!(b.sends.load(Ordering::Acquire), 1);
    }
}
