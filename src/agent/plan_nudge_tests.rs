use super::*;
use crate::agent::todo::{PlanAttachment, Status, TaskContext, TaskState, Todo};

#[test]
fn advice_context_tracks_distinct_growth_across_refresh_and_retires_on_drop() {
    let context = AdviceContext::default();
    let mut run = context.run();
    let current = context.begin("Fix a typo", false);
    assert!(current
        .take_reminder(&["a.rs".into(), "b.rs".into()])
        .is_none());
    run.preserve_for_refresh();
    drop(run);

    let run = context.run();
    let refreshed = context.begin("Distilled migration text is not new user intent", false);
    assert!(refreshed.take_reminder(&["b.rs".into()]).is_none());
    assert!(refreshed
        .take_reminder(&["c.rs".into()])
        .unwrap()
        .contains("Workflow advice: Todo"));
    let many: Vec<_> = (0..100)
        .map(|n| std::path::PathBuf::from(format!("{n}.rs")))
        .collect();
    assert!(refreshed.take_reminder(&many).is_none());
    assert_eq!(
        context
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .edited_paths
            .len(),
        LARGE_TASK_FILES
    );
    refreshed.observe_request("Also migrate storage");
    assert!(refreshed
        .take_reminder(&[])
        .unwrap()
        .contains("Workflow advice: Plan"));
    assert!(refreshed.take_reminder(&[]).is_none());
    drop(run); // Also runs when the loop future is dropped or admission fails.
    assert!(context.0.lock().unwrap().is_none());

    let _run = context.run();
    assert!(context
        .begin("Migrate storage", false)
        .take_reminder(&[])
        .is_some());
    let independent = AdviceContext::default();
    assert!(independent
        .begin("Migrate storage", false)
        .take_reminder(&[])
        .is_some());
}

#[test]
fn advice_context_planning_suppression_is_sticky_only_for_the_live_request() {
    let context = AdviceContext::default();
    let run = context.run();
    context.begin("Migrate storage", true);
    assert!(context
        .begin("Now implement", false)
        .take_reminder(&[])
        .is_none());
    drop(run);
    assert!(context
        .begin("Migrate storage", false)
        .take_reminder(&[])
        .is_some());
}

#[test]
fn advisory_matrix_is_request_local_and_conservative() {
    for (request, expected) in [
        ("Fix a typo", Level::Direct),
        ("Sửa lỗi chính tả", Level::Direct),
        ("Investigate the parser", Level::Direct),
        ("What is a database migration?", Level::Direct),
        ("How do I migrate a database?", Level::Direct),
        ("Giải thích migration", Level::Direct),
        ("Fix the label without migration", Level::Direct),
        ("Fix the label, do not migrate data", Level::Direct),
        ("Sửa nhãn, không cần migration", Level::Direct),
        ("Fix migration_count in one function", Level::Direct),
        ("Refactor all files in the codebase", Level::Todo),
        ("Implement several steps in the parser", Level::Todo),
        ("Sửa parser qua nhiều bước", Level::Todo),
        ("Update src/a.rs tests/a.rs docs/a.md", Level::Todo),
        ("Migrate the saved session format", Level::Plan),
        ("Triển khai migration dữ liệu", Level::Plan),
        ("Update the public API and its callers", Level::Plan),
        ("Đổi API công khai và nơi gọi", Level::Plan),
        (
            "Implement authentication across multiple systems",
            Level::Plan,
        ),
        ("Sửa nhiều hệ thống", Level::Plan),
        ("Implement a hard-to-reverse storage change", Level::Plan),
        ("Write an implementation plan for the parser", Level::Plan),
        ("Lập kế hoạch sửa parser", Level::Plan),
        ("Perform a multi-phase audit", Level::Plan),
    ] {
        assert_eq!(
            select(&request_text(request), &mut ApiScope::default()).level,
            expected,
            "{request}"
        );
    }
}

#[test]
fn untrusted_wrappers_quotes_and_large_input_do_not_expand_scope() {
    for request in [
        "<architect_plan>Migrate multiple systems</architect_plan>Fix a typo",
        "Fix a typo\n> Migrate all storage\n\"public API and callers\"",
        "Fix a typo\n```\nMigrate all storage\n```",
        "Fix a typo\n~~~text\nMigrate all storage\n~~~",
    ] {
        assert_eq!(
            select(&request_text(request), &mut ApiScope::default()).level,
            Level::Direct
        );
    }
    let request = format!("{}\nMigrate all storage", " ".repeat(MAX_REQUEST_CHARS));
    assert_eq!(
        select(&request_text(&request), &mut ApiScope::default()).level,
        Level::Direct
    );
    // A draft is neither a saved contract nor a blanket reminder exemption.
    let mut advice = PlanNudge::new(
        "<architect_plan>draft</architect_plan>Migrate the saved session format",
        false,
    );
    assert!(advice
        .take_reminder(0)
        .unwrap()
        .contains("Workflow advice: Plan"));
}

#[test]
fn informational_edit_verbs_do_not_request_implementation_or_a_plan() {
    for request in [
        "How should I migrate storage?",
        "Research how to migrate storage",
        "Please research how to update the public API and callers",
        "What changes if we migrate storage?",
        "Should we update the public API and callers?",
        "Can you explain how to migrate storage?",
        "Also explain how to update its callers",
        "How do I write an implementation plan?",
        "Research how to write a plan",
        "Nghiên cứu cách di chuyển dữ liệu",
        "Làm sao để chuyển đổi dữ liệu?",
        "Tại sao phải đổi API công khai và nơi gọi?",
        "Review the public API and callers",
    ] {
        let mut scope = ApiScope::default();
        assert_eq!(
            select(&request_text(request), &mut scope).level,
            Level::Direct,
            "{request}"
        );
        assert!(!scope.public_api && !scope.callers, "{request}");
    }
    for (request, expected) in [
        ("Can you migrate storage?", Level::Plan),
        ("Please update the public API and callers", Level::Plan),
        (
            "Research storage, then migrate the saved format",
            Level::Plan,
        ),
        ("Investigate the parser; then fix it", Level::Todo),
        (
            "Research storage. Write an implementation plan",
            Level::Plan,
        ),
        ("Perform a multi-phase audit", Level::Plan),
        ("Audit storage in a multi-phase audit", Level::Plan),
        ("Nghiên cứu dữ liệu, sau đó chuyển đổi dữ liệu", Level::Plan),
    ] {
        assert_eq!(
            select(&request_text(request), &mut ApiScope::default()).level,
            expected,
            "{request}"
        );
    }
}

#[test]
fn api_scope_accumulates_only_explicit_request_evidence() {
    for (initial, steering) in [
        (
            "Update the public API in several steps",
            "Also update its callers",
        ),
        (
            "Update the callers in several steps",
            "Also update the public API",
        ),
        ("Đổi API công khai qua nhiều bước", "Cập nhật nơi gọi"),
    ] {
        let mut advice = PlanNudge::new(initial, false);
        assert!(advice
            .take_reminder(0)
            .unwrap()
            .contains("Workflow advice: Todo"));
        advice.observe_request(steering);
        assert!(advice
            .take_reminder(0)
            .unwrap()
            .contains("public API and callers"));
        for _ in 0..5 {
            advice.observe_request(steering);
            assert!(advice.take_reminder(0).is_none());
        }
    }
    let mut independent = PlanNudge::new("Update its callers", false);
    assert!(
        independent.take_reminder(0).is_none(),
        "new request has no prior API evidence"
    );
}

#[test]
fn quoted_negated_and_informational_steers_never_seed_api_scope() {
    for (seed, excluded) in [
        ("Update the public API", "> Also update its callers"),
        ("Update the public API", "```text\nUpdate its callers\n```"),
        ("Update the public API", "Example: \"update its callers\""),
        ("Update the public API", "Example: 'update its callers'"),
        ("Update the public API", "Example: `update its callers`"),
        ("Update the public API", "Do not update its callers"),
        ("Update the public API", "Don't update its callers"),
        ("Update the public API", "Đừng đổi nơi gọi"),
        ("Update the public API", "How should I update its callers?"),
        (
            "Update the public API",
            "Also research how to update its callers",
        ),
        (
            "Update the public API",
            "Fix the label; explain how to update its callers",
        ),
        ("Update the callers", "> Update the public API"),
        ("Update the callers", "Do not change the public API"),
        (
            "Update the callers",
            "Research how to update the public API",
        ),
    ] {
        // Exercise both orderings: exclusions cannot contribute even before the positive cue.
        for (first, second) in [(seed, excluded), (excluded, seed)] {
            let mut advice = PlanNudge::new(first, false);
            assert!(advice.take_reminder(0).is_none(), "{first}");
            advice.observe_request(second);
            assert!(advice.take_reminder(0).is_none(), "{first} / {second}");
            assert!(!(advice.api_scope.public_api && advice.api_scope.callers));
        }
    }
}

#[test]
fn fences_require_matching_marker_length_and_empty_closing_suffix() {
    for marker in ['`', '~'] {
        let short = marker.to_string().repeat(3);
        let opening = marker.to_string().repeat(4);
        let longer = marker.to_string().repeat(5);
        let other = if marker == '`' { "~~~~" } else { "````" };
        for inner in [short.as_str(), other, &format!("{opening}text")] {
            let request =
                format!("Fix a typo\n{opening}markdown\n{inner}\nMigrate storage\n{opening}");
            assert_eq!(
                select(&request_text(&request), &mut ApiScope::default()).level,
                Level::Direct,
                "{request}"
            );
        }
        for closing in [opening.as_str(), longer.as_str()] {
            let request =
                format!("Fix a typo\n{opening}text\nquoted data\n{closing} \t\nMigrate storage");
            assert_eq!(
                select(&request_text(&request), &mut ApiScope::default()).level,
                Level::Plan,
                "{request}"
            );
        }
    }
}

#[test]
fn large_request_is_reminded_before_edits_without_demanding_a_plan() {
    let mut advice = PlanNudge::new("Refactor all files in the codebase", false);
    let text = advice.take_reminder(0).unwrap();
    for expected in ["Workflow advice: Todo", "todo_write", "0 files", "<=5"] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("Save a plan FIRST"));
    assert!(advice.take_reminder(20).is_none());
}

#[test]
fn growth_only_reaches_todo_and_each_increase_is_emitted_once() {
    for request in ["Fix a typo", "Investigate the parser"] {
        let mut advice = PlanNudge::new(request, false);
        assert!(advice.take_reminder(2).is_none());
        assert!(advice
            .take_reminder(3)
            .unwrap()
            .contains("Workflow advice: Todo"));
        assert!(advice.take_reminder(1000).is_none());
        advice.observe_request("Also migrate the saved session format");
        let text = advice.take_reminder(1000).unwrap();
        for required in [
            "Workflow advice: Plan",
            "docs/plans/",
            "FIRST",
            "acceptance",
            "verification",
        ] {
            assert!(text.contains(required), "{text}");
        }
        // Retried calls and refreshed task snapshots do not reset the highest emitted level.
        for _ in 0..5 {
            advice.observe_request("Migrate multiple systems");
            advice.observe_task(&TaskState::default());
            assert!(advice.take_reminder(1000).is_none());
        }
    }
}

#[test]
fn only_active_unfinished_todos_satisfy_todo_and_never_plan() {
    let task = TaskContext::private();
    task.replace(vec![Todo::new("Inspect current parser", Status::Pending)])
        .unwrap();
    let active = task.snapshot();
    assert!(active.active);
    let mut advice = PlanNudge::new("Refactor all files", false);
    advice.observe_task(&active);
    assert!(advice.take_reminder(5).is_none());
    advice.observe_request("Migrate the public API and callers");
    assert!(advice
        .take_reminder(5)
        .unwrap()
        .contains("Workflow advice: Plan"));
    for historical in [false, true] {
        let mut state = active.clone();
        if historical {
            state.active = false;
        } else {
            state.items[0].status = Status::Done;
        }
        let mut advice = PlanNudge::new("Refactor all files", false);
        advice.observe_task(&state);
        assert!(advice.take_reminder(5).is_some());
    }
}

#[test]
fn planning_only_suppresses_advice_without_satisfying_saved_plan() {
    for request in [
        "Refactor all files, plan only",
        "Chỉ lên plan cho toàn bộ repo",
        "Migrate storage; don't implement",
        "Chỉ lập kế hoạch migration",
    ] {
        let mut advice = PlanNudge::new(request, false);
        assert!(advice.take_reminder(20).is_none());
        assert!(!advice.has_plan);
    }
    let mut advice = PlanNudge::new("Migrate storage", true);
    advice.observe_request("Now implement");
    assert!(advice.take_reminder(20).is_none());
    assert!(!advice.has_plan);
}

#[test]
fn paths_and_legacy_files_do_not_replace_validated_attachment() {
    let dir = crate::core::file_placement::tests::TestDir::new();
    std::fs::create_dir_all(dir.path().join("docs/plans")).unwrap();
    let path = dir.path().join("docs/plans/migrate.md");
    std::fs::write(&path, "# Migration\n\nAcceptance checks").unwrap();
    let mut advice = PlanNudge::new("Migrate using `docs/plans/migrate.md`", false);
    assert!(advice.take_reminder(0).is_some());
    std::fs::write(&path,
        "---\naizen_plan: 1\nid: p-0123456789abcdef0123456789abcdef\n---\n<!-- aizen:phase inspect -->\n"
    ).unwrap();
    let task = TaskContext::private();
    task.attach_plan(
        PlanAttachment::load(&dir.path().canonicalize().unwrap(), "docs/plans/migrate.md").unwrap(),
    )
    .unwrap();
    let mut advice = PlanNudge::new("Migrate storage", false);
    advice.observe_task(&task.snapshot());
    assert!(advice.take_reminder(10).is_none());
    std::fs::remove_file(path).unwrap();
    task.refresh_plan().unwrap();
    advice.observe_task(&task.snapshot());
    assert!(
        advice.take_reminder(10).is_some(),
        "missing is not sticky saved state"
    );
    task.refresh_plan().unwrap();
    advice.observe_task(&task.snapshot());
    assert!(
        advice.take_reminder(10).is_none(),
        "refresh does not nag twice"
    );
}

#[test]
fn all_reminders_are_bounded_and_do_not_mutate_task_or_control() {
    let task = TaskContext::private();
    task.replace(vec![Todo::new("Unfinished work", Status::Pending)])
        .unwrap();
    let state = task.snapshot();
    let control = task.control().snapshot();
    for request in [
        "Fix a typo",
        "Refactor all files",
        "Migrate multiple systems",
    ] {
        let mut advice = PlanNudge::new(request, false);
        advice.observe_task(&state);
        if let Some(text) = advice.take_reminder(100) {
            assert!(text.len() <= 2048, "{} bytes", text.len());
            assert!(text.contains("not new permission"));
        }
        assert_eq!(task.snapshot(), state);
        assert_eq!(task.control().snapshot(), control);
    }
}
