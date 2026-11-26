//! Progress-based soft time grants inside an immutable dispatch hard ceiling.
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROGRESS_FRESHNESS: Duration = Duration::from_secs(300);

/// Only completed work/data observations can earn time. Planning, transport heartbeats and
/// background-process status polling cannot manufacture progress merely by changing a timestamp.
pub(crate) fn progress_tool(name: &str) -> bool {
    matches!(
        name,
        "file_read"
            | "file_edit"
            | "file_write"
            | "search_files"
            | "file_glob"
            | "read_symbol"
            | "memory_search"
            | "session_recall"
            | "codebase_search"
            | "git_inspect"
            | "shell_run"
            | "web_fetch"
            | "web_crawl"
            | "web_search"
    )
}

#[derive(Clone, Debug)]
pub(crate) struct AdaptiveTimeBudget(Arc<Mutex<State>>);

#[derive(Debug)]
struct State {
    soft_deadline: Instant,
    hard_deadline: Instant,
    last_progress: Option<Instant>,
    progress: u64,
    seen: HashSet<u64>,
    granted_progress: u64,
    step_millis: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct TimeGrant {
    pub(crate) previous: Instant,
    pub(crate) deadline: Instant,
}

impl AdaptiveTimeBudget {
    pub(crate) fn new(start: Instant, initial: Duration, hard_deadline: Instant) -> Self {
        let soft_deadline = start
            .checked_add(initial)
            .unwrap_or(start)
            .min(hard_deadline);
        Self(Arc::new(Mutex::new(State {
            soft_deadline,
            hard_deadline,
            last_progress: None,
            progress: 0,
            seen: HashSet::new(),
            granted_progress: 0,
            // A slow delegated model call can take five minutes. The estimate is updated only
            // by completed productive steps, never transport activity or a provider's claim.
            step_millis: 150_000,
        })))
    }

    /// The caller must supply a novel successful tool result from this exact run. Failure,
    /// repeated output, status polling and the model's own text are not progress certificates.
    pub(crate) fn note_progress(&self, now: Instant, elapsed: Duration, tool: &str, body: &str) {
        if !progress_tool(tool) || body.trim().is_empty() {
            return;
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        tool.hash(&mut hash);
        body.hash(&mut hash);

        let Ok(mut state) = self.0.lock() else { return };
        if now >= state.soft_deadline
            || now >= state.hard_deadline
            || state.last_progress.is_some_and(|last| now < last)
        {
            return;
        }
        // The evidence set lives with the shared time budget, so retrying the agent loop or
        // cloning a config cannot reuse observations even if that loop's stall ledger resets.
        // A hash collision only withholds a grant; it can never manufacture one.
        if !state.seen.insert(hash.finish()) {
            return;
        }
        let sample = elapsed.as_millis().clamp(1, 600_000) as u64;
        state.step_millis = (state.step_millis * 3 + sample) / 4;
        state.progress = state.progress.saturating_add(1);
        state.last_progress = Some(now);
    }

    /// A grant is made before expiry and spends fresh progress once. Neither repeated checks nor
    /// cloned configs mint more credit. This never changes an execution context's hard deadline.
    pub(crate) fn deadline(
        &self,
        now: Instant,
        grant_permitted: bool,
    ) -> (Instant, Option<TimeGrant>) {
        let Ok(mut state) = self.0.lock() else {
            // Policy state lost to a panic cannot authorize further work.
            return (now, None);
        };
        let lead = Duration::from_millis((state.step_millis * 2).clamp(60_000, 300_000));
        let fresh = state.last_progress.is_some_and(|last| {
            now >= last && now.saturating_duration_since(last) <= PROGRESS_FRESHNESS
        });
        if grant_permitted
            && now < state.soft_deadline
            && now < state.hard_deadline
            && state.soft_deadline.saturating_duration_since(now) <= lead
            && state.progress > state.granted_progress
            && fresh
        {
            let extra = Duration::from_millis((state.step_millis * 4).clamp(120_000, 600_000));
            let previous = state.soft_deadline;
            let deadline = previous
                .checked_add(extra)
                .unwrap_or(state.hard_deadline)
                .min(state.hard_deadline);
            state.granted_progress = state.progress;
            state.soft_deadline = deadline;
            if deadline > previous {
                return (deadline, Some(TimeGrant { previous, deadline }));
            }
        }
        (state.soft_deadline.min(state.hard_deadline), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> (Instant, AdaptiveTimeBudget) {
        let start = Instant::now();
        (
            start,
            AdaptiveTimeBudget::new(
                start,
                Duration::from_secs(1200),
                start + Duration::from_secs(2700),
            ),
        )
    }

    #[test]
    fn control_and_polling_results_never_earn_time() {
        for name in [
            "process",
            "todo_write",
            "goal_complete",
            "clarify",
            "task",
            "workflow",
            "heartbeat",
        ] {
            assert!(!progress_tool(name));
        }
        for name in ["file_read", "file_edit", "shell_run", "search_files"] {
            assert!(progress_tool(name));
        }
    }

    #[test]
    fn a_productive_long_step_can_earn_time_before_soft_expiry() {
        let (start, budget) = budget();
        let now = start + Duration::from_secs(1000);
        budget.note_progress(
            now,
            Duration::from_secs(240),
            "file_read",
            "fresh file contents",
        );
        let (until, grant) = budget.deadline(now, true);
        let grant = grant.unwrap();
        assert_eq!(grant.previous, start + Duration::from_secs(1200));
        assert_eq!(grant.deadline, until);
        assert!(until > grant.previous && until <= start + Duration::from_secs(2700));
    }

    #[test]
    fn idle_checks_cannot_earn_more_time() {
        let (start, budget) = budget();
        for seconds in [900, 1000, 1199, 1200, 1800] {
            assert_eq!(
                budget.deadline(start + Duration::from_secs(seconds), true),
                (start + Duration::from_secs(1200), None)
            );
        }
    }

    #[test]
    fn config_clones_spend_the_same_progress_credit() {
        let (start, budget) = budget();
        let clone = budget.clone();
        let now = start + Duration::from_secs(1150);
        for i in 0..30 {
            budget.note_progress(
                now,
                Duration::from_millis(1),
                "file_read",
                &format!("file {i}"),
            );
        }
        let (until, grant) = clone.deadline(now, true);
        assert!(grant.is_some());
        assert!(budget
            .deadline(until - Duration::from_secs(1), true)
            .1
            .is_none());
        assert_eq!(budget.deadline(now, true).0, until);
    }

    #[test]
    fn duplicate_observations_cannot_earn_again_through_a_cloned_config() {
        let (start, budget) = budget();
        let now = start + Duration::from_secs(1150);
        for i in 0..30 {
            budget.note_progress(
                now,
                Duration::from_millis(1),
                "file_read",
                &format!("file {i}"),
            );
        }
        let until = budget.deadline(now, true).0;
        let retry = budget.clone();
        let near = until - Duration::from_secs(1);
        retry.note_progress(near, Duration::from_secs(1), "file_read", "file 0");
        retry.note_progress(
            near,
            Duration::from_secs(1),
            "process",
            "still running at a new time",
        );
        retry.note_progress(near, Duration::from_secs(1), "file_read", " ");
        assert_eq!(retry.deadline(near, true), (until, None));
        retry.note_progress(
            near,
            Duration::from_secs(1),
            "file_read",
            "actually changed contents",
        );
        assert!(retry.deadline(near, true).1.is_some());
    }

    #[test]
    fn stale_progress_does_not_keep_an_idle_run_alive() {
        let (start, budget) = budget();
        budget.note_progress(
            start + Duration::from_secs(1),
            Duration::from_secs(1),
            "file_read",
            "fresh file contents",
        );
        assert!(budget
            .deadline(start + Duration::from_secs(1199), true)
            .1
            .is_none());
    }

    #[test]
    fn late_progress_never_reopens_an_expired_budget() {
        let (start, budget) = budget();
        for seconds in [1200, 1201] {
            let now = start + Duration::from_secs(seconds);
            budget.note_progress(
                now,
                Duration::from_secs(1),
                "file_read",
                "fresh file contents",
            );
            assert!(budget.deadline(now, true).1.is_none());
            assert_eq!(
                budget.deadline(now, true).0,
                start + Duration::from_secs(1200)
            );
        }
    }

    #[test]
    fn parent_ceiling_wins_over_progress_grants() {
        let start = Instant::now();
        let hard = start + Duration::from_secs(1250);
        let budget = AdaptiveTimeBudget::new(start, Duration::from_secs(1200), hard);
        let now = start + Duration::from_secs(1100);
        budget.note_progress(
            now,
            Duration::from_secs(240),
            "file_read",
            "fresh file contents",
        );
        assert_eq!(budget.deadline(now, true).0, hard);
        budget.note_progress(
            hard - Duration::from_secs(1),
            Duration::from_secs(1),
            "file_read",
            "fresh file contents",
        );
        assert_eq!(budget.deadline(hard - Duration::from_secs(1), true).0, hard);
        assert!(budget.deadline(hard, true).1.is_none());
    }

    #[test]
    fn cancellation_disallows_an_otherwise_earned_grant() {
        let (start, budget) = budget();
        let now = start + Duration::from_secs(1100);
        budget.note_progress(
            now,
            Duration::from_secs(240),
            "file_read",
            "fresh file contents",
        );
        assert!(budget.deadline(now, false).1.is_none());
        assert_eq!(
            budget.deadline(now, false).0,
            start + Duration::from_secs(1200)
        );
    }

    #[test]
    fn an_already_expired_parent_never_receives_an_initial_window() {
        let start = Instant::now();
        let budget = AdaptiveTimeBudget::new(start, Duration::from_secs(1200), start);
        budget.note_progress(
            start,
            Duration::from_secs(1),
            "file_read",
            "fresh file contents",
        );
        assert_eq!(budget.deadline(start, true).0, start);
        assert!(budget.deadline(start, true).1.is_none());
    }
}
