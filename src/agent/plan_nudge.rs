//! One advisory planning reminder before the next model call, never an edit gate.
//! The model judges task size; file count is only a fallback when work grows mid-run.

use crate::core::turn_shape::{self, TurnShape};
use std::path::{Component, Path};

pub(crate) const LARGE_TASK_FILES: usize = 3;

pub(crate) struct PlanNudge {
    large_request: bool,
    plan_only: bool,
    reminded: bool,
    has_plan: bool,
}

impl PlanNudge {
    pub(crate) fn new(request: &str, root: &Path) -> Self {
        // Architect prose is not the user's intent, and already carries its own save-first advice.
        let (brief, has_draft) = request
            .strip_prefix("<architect_plan>")
            .and_then(|s| s.split_once("</architect_plan>"))
            .map_or((request, false), |(_, brief)| (brief, true));
        let lower = brief.to_lowercase();
        let plan_only = [
            "plan only",
            "plan-only",
            "don't implement",
            "do not implement",
            "no implementation",
            "chỉ lập kế hoạch",
            "chỉ lên plan",
            "chỉ viết plan",
            "chỉ viết spec",
            "chưa triển khai",
            "không triển khai",
            "đừng triển khai",
            "chưa code",
            "đừng code",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase));
        let has_plan = brief.split_whitespace().any(|token| {
            let token = token.trim_matches(['`', '"', '\'', '(', ')', '[', ']', ',', ';']);
            is_plan_path(root, Path::new(token))
        });
        Self {
            large_request: turn_shape::classify(brief) == TurnShape::MultiFile,
            plan_only,
            reminded: has_draft,
            has_plan,
        }
    }

    pub(crate) fn observe_path(&mut self, root: &Path, path: &Path) {
        self.has_plan |= is_plan_path(root, path);
    }

    pub(crate) fn take_reminder(
        &mut self,
        files_touched: usize,
        has_todos: bool,
    ) -> Option<String> {
        if self.reminded
            || self.plan_only
            || self.has_plan
            || has_todos
            || (!self.large_request && files_touched < LARGE_TASK_FILES)
        {
            return None;
        }
        self.reminded = true;
        Some(format!(
            "{} Judge the scope before further implementation ({} files changed so far). \
             For a small, reversible task use a short todo_write checklist (<=5 items). For a \
             multi-system change, migration, public API plus callers, or hard-to-reverse work, \
             write docs/plans/<topic>.md FIRST using ordinary file tools: requirements, scope and \
             non-goals, design decisions, phases with files, acceptance criteria and verification. \
             Then track execution with todos. Read an existing relevant plan rather than replacing \
             it; keep its path in handoff summaries. If the task is genuinely small, continue \
             without a plan file. Plan-only requests do not authorize implementation. This is an \
             advisory reminder, not new permission to edit or bypass a denied write.",
            super::PLAN_NUDGE_PREFIX,
            files_touched,
        ))
    }
}

/// Only a named, nonempty Markdown file under the chosen directory counts. This is reminder
/// bookkeeping, not filesystem authorization; actual reads/writes still use the normal tools.
fn is_plan_path(root: &Path, path: &Path) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let Ok(relative) = path.strip_prefix(root.join(super::architect::PLAN_DIR)) else {
        return false;
    };
    relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        && relative.extension().is_some_and(|e| e == "md")
        && path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_request_is_reminded_before_edits_and_only_once() {
        let mut state = PlanNudge::new("Refactor all files in the codebase", Path::new("."));
        let text = state.take_reminder(0, false).unwrap();
        for required in [
            "todo_write",
            "docs/plans/",
            "FIRST",
            "acceptance",
            "0 files",
        ] {
            assert!(text.contains(required), "{text}");
        }
        assert!(state.take_reminder(20, false).is_none());
    }

    #[test]
    fn growing_work_is_reminded_before_the_next_step() {
        let mut state = PlanNudge::new("Fix a typo", Path::new("."));
        assert!(state.take_reminder(2, false).is_none());
        assert!(state.take_reminder(3, false).is_some());
        assert!(state.take_reminder(4, false).is_none());
    }

    #[test]
    fn model_tracking_and_plan_only_requests_do_not_get_a_second_demand() {
        for request in [
            "Refactor all files, plan only",
            "Chỉ lên plan cho toàn bộ repo",
            "<architect_plan>spec</architect_plan> Refactor all files",
        ] {
            assert!(PlanNudge::new(request, Path::new("."))
                .take_reminder(20, false)
                .is_none());
        }
        assert!(PlanNudge::new("Refactor all files", Path::new("."))
            .take_reminder(10, true)
            .is_none());
    }

    #[test]
    fn an_unrelated_research_turn_does_not_hide_growing_edits() {
        let mut state = PlanNudge::new("Investigate the parser", Path::new("."));
        assert!(state.take_reminder(0, false).is_none());
        assert!(state.take_reminder(3, false).is_some());
    }

    #[test]
    fn only_a_real_named_markdown_plan_suppresses_the_reminder() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        std::fs::create_dir_all(dir.path().join("docs/plans")).unwrap();
        let path = dir.path().join("docs/plans/migrate.md");
        std::fs::write(&path, "# Migration\n\nAcceptance checks").unwrap();
        let mut state = PlanNudge::new(
            "Implement all files using `docs/plans/migrate.md`",
            dir.path(),
        );
        assert!(state.take_reminder(0, false).is_none());
        let mut state = PlanNudge::new("Implement all files", dir.path());
        state.observe_path(dir.path(), &path);
        assert!(state.take_reminder(10, false).is_none());
        for name in [
            "docs/plans/missing.md",
            "docs/plans/../other.md",
            "docs/plans/data.json",
        ] {
            assert!(!is_plan_path(dir.path(), Path::new(name)));
        }
        std::fs::write(&path, "").unwrap();
        assert!(!is_plan_path(dir.path(), &path));
    }
}
