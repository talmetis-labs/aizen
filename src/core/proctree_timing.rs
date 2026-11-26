//! Relative legacy waits and absolute admitted-operation deadlines are deliberately distinct.
//! An absolute deadline is never converted into a new relative budget after OS spawn/IO startup.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct BoundedDeadlines {
    pub(crate) run_until: Instant,
    pub(crate) settle_until: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BoundedTiming {
    Relative(Duration),
    // No production already-admitted caller until the source-Git safety gates pass.
    #[cfg(test)]
    Absolute(BoundedDeadlines),
}

impl BoundedTiming {
    pub(crate) fn spawn_deadlines(self) -> Option<BoundedDeadlines> {
        match self {
            Self::Relative(_) => None,
            #[cfg(test)]
            Self::Absolute(deadlines) => Some(deadlines),
        }
    }

    pub(crate) fn run_deadline(self, now: Instant) -> Instant {
        match self {
            Self::Relative(timeout) => now.checked_add(timeout).unwrap_or(now),
            #[cfg(test)]
            Self::Absolute(deadlines) => deadlines.run_until,
        }
    }

    pub(crate) fn cleanup_deadline(self, now: Instant, grace: Duration) -> Instant {
        let deadline = now.checked_add(grace).unwrap_or(now);
        self.spawn_deadlines()
            .map_or(deadline, |absolute| deadline.min(absolute.settle_until))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_deadlines_do_not_restart_after_spawn_or_worker_startup() {
        let now = Instant::now();
        let run_until = now + Duration::from_millis(100);
        let settle_until = now + Duration::from_millis(120);
        let timing = BoundedTiming::Absolute(BoundedDeadlines {
            run_until,
            settle_until,
        });
        for elapsed in [0, 50, 100, 150] {
            let after_startup = now + Duration::from_millis(elapsed);
            assert_eq!(timing.run_deadline(after_startup), run_until);
            assert!(timing.cleanup_deadline(after_startup, Duration::from_secs(2)) <= settle_until);
        }
        assert_eq!(
            timing
                .run_deadline(settle_until)
                .saturating_duration_since(settle_until),
            Duration::ZERO
        );
    }

    #[test]
    fn legacy_relative_wait_and_grace_stay_relative() {
        let now = Instant::now();
        let timing = BoundedTiming::Relative(Duration::from_secs(3));
        assert!(timing.spawn_deadlines().is_none());
        assert_eq!(timing.run_deadline(now), now + Duration::from_secs(3));
        assert_eq!(
            timing.cleanup_deadline(now, Duration::from_secs(2)),
            now + Duration::from_secs(2)
        );
    }
}
