use super::*;

fn requested_check(task: &todo::TaskContext, root: &Path, command: &str) {
    let mut rows = task
        .replace(vec![todo::Todo::new("check", todo::Status::Pending)])
        .unwrap();
    let requirements = vec![(
        rows[0].id.clone(),
        todo::Requirement::Check {
            command: command.into(),
            root: root.to_string_lossy().into_owned(),
            source_identity: None,
        },
    )];
    rows[0].status = todo::Status::Done;
    task.replace_with_requirements(rows, requirements).unwrap();
}

fn pass(root: &Path, commands: &[&str]) -> verify_gate::VerifyGateResult {
    verify_gate::VerifyGateResult {
        termination: verify_gate::VerifyTermination::Passed,
        source_identity: verify_gate::source_fingerprint(root),
        passed_commands: commands.iter().map(|c| (*c).into()).collect(),
        background_deferred: false,
        passed: true,
        command: commands.last().unwrap().to_string(),
        output: String::new(),
        duration_ms: 1,
        stable: true,
        effect_state: crate::agent::tools::ToolEffectState::Settled,
    }
}

#[test]
fn receipt_only_covers_actual_passing_command_and_resolves_request() {
    let directory = crate::core::file_placement::tests::TestDir::new();
    let task = todo::TaskContext::private();
    requested_check(&task, directory.path(), "cargo test -q");
    let tokens = task.evidence_captures();
    record_gate(
        &task,
        tokens.clone(),
        directory.path(),
        &pass(directory.path(), &["cargo check"]),
    )
    .unwrap();
    assert!(task.completion_pending());
    assert_ne!(task.snapshot().items[0].status, todo::Status::Done);
    record_gate(
        &task,
        tokens,
        directory.path(),
        &pass(directory.path(), &["cargo check", "cargo test -q"]),
    )
    .unwrap();
    assert!(task.is_complete());
    assert_eq!(task.snapshot().items[0].status, todo::Status::Done);
}

#[test]
fn source_drift_downgrades_a_fresh_check() {
    let directory = crate::core::file_placement::tests::TestDir::new();
    std::fs::write(directory.path().join("source.rs"), "fn old() {}\n").unwrap();
    let task = todo::TaskContext::private();
    requested_check(&task, directory.path(), "cargo check");
    record_gate(
        &task,
        task.evidence_captures(),
        directory.path(),
        &pass(directory.path(), &["cargo check"]),
    )
    .unwrap();
    assert!(task.is_complete());
    std::fs::write(directory.path().join("source.rs"), "fn changed() {}\n").unwrap();
    revalidate(&task).unwrap();
    assert!(!task.is_complete());
    assert!(task.completion_pending());
}

#[test]
fn fresh_question_is_done_but_cancelled_answer_is_not() {
    let ctx = ExecutionContext::default().with_task(todo::TaskContext::private());
    let cancel = TurnCancel::new();
    let mut answer = AgentOutcome {
        speech_final_verified: false,
        final_text: Some("answer".into()),
        iters: 1,
        verification: None,
        stop: StopReason::Done,
    };
    answer.reconcile_task_completion(&ctx, &cancel);
    assert_eq!(answer.stop, StopReason::Done);
    cancel.cancel();
    answer.reconcile_task_completion(&ctx, &cancel);
    assert_eq!(answer.stop, StopReason::Cancelled);
}

#[test]
fn late_hook_effect_cannot_keep_a_successful_completion() {
    let directory = crate::core::file_placement::tests::TestDir::new();
    let task = todo::TaskContext::private();
    requested_check(&task, directory.path(), "cargo check");
    record_gate(
        &task,
        task.evidence_captures(),
        directory.path(),
        &pass(directory.path(), &["cargo check"]),
    )
    .unwrap();
    let ctx = ExecutionContext::default().with_task(task.clone());
    let cancel = TurnCancel::new();
    let mut answer = AgentOutcome {
        speech_final_verified: false,
        final_text: Some("complete".into()),
        iters: 1,
        verification: None,
        stop: StopReason::Done,
    };
    answer.reconcile_task_completion(&ctx, &cancel);
    assert_eq!(answer.stop, StopReason::Done);
    ctx.note_task_effects_at(Some(directory.path()));
    answer.reconcile_task_completion(&ctx, &cancel);
    assert_eq!(answer.stop, StopReason::Incomplete);
    assert_eq!(
        answer.verification.unwrap()["completion"]["completed"],
        false
    );
}

#[tokio::test]
async fn planning_checks_leave_existing_completion_authority_untouched() {
    use crate::core::session_control::ExecutionMode;
    let directory = crate::core::file_placement::tests::TestDir::new();
    let task = todo::TaskContext::private();
    requested_check(&task, directory.path(), "cargo check");
    task.control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .unwrap();
    let ctx = ExecutionContext::default().with_task(task.clone());
    let before = task.snapshot();
    assert!(run_checks(&ctx, directory.path(), 1, &[], true)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        task.snapshot(),
        before,
        "no fabricated evidence or new obligation"
    );
    assert!(task.completion_pending());
}

#[test]
fn stale_policy_downgrades_done_even_without_checklist_work() {
    use crate::core::session_control::ExecutionMode;
    let task = todo::TaskContext::private();
    let ctx = ExecutionContext::default().with_task(task.clone());
    task.control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .unwrap();
    let mut answer = AgentOutcome {
        speech_final_verified: false,
        final_text: Some("answer from old context".into()),
        iters: 1,
        verification: None,
        stop: StopReason::Done,
    };
    answer.reconcile_task_completion(&ctx, &TurnCancel::new());
    assert_eq!(answer.stop, StopReason::Incomplete);
}

#[test]
fn completion_refreshes_removed_plan_phase_before_reporting_done() {
    use crate::agent::todo::PlanAttachment;
    let directory = crate::core::file_placement::tests::TestDir::new();
    let root = directory.path().canonicalize().unwrap();
    let path = "docs/plans/completion.md";
    let header = "---\naizen_plan: 1\nid: p-0123456789abcdef0123456789abcdef\n---\n";
    std::fs::create_dir_all(root.join("docs/plans")).unwrap();
    std::fs::write(
        root.join(path),
        format!("{header}<!-- aizen:phase inspect -->\n## Inspect\n"),
    )
    .unwrap();
    let task = todo::TaskContext::private();
    task.replace_with_plan(
        vec![todo::Todo::new("summarize", todo::Status::Done)],
        vec![("@index:0".into(), todo::Requirement::Report)],
        Some(PlanAttachment::load(&root, path).unwrap()),
        vec![("@index:0".into(), "inspect".into())],
    )
    .unwrap();
    task.record_report().unwrap();
    assert!(task.is_complete());
    let ctx = ExecutionContext::default().with_task(task.clone());
    std::fs::write(
        root.join(path),
        format!("{header}<!-- aizen:phase implement -->\n## Implement\n"),
    )
    .unwrap();
    let mut answer = AgentOutcome {
        speech_final_verified: false,
        final_text: Some("complete".into()),
        iters: 1,
        verification: None,
        stop: StopReason::Done,
    };
    answer.reconcile_task_completion(&ctx, &TurnCancel::new());
    assert_eq!(answer.stop, StopReason::Incomplete);
    let refreshed = task.snapshot();
    revalidate(&task).unwrap();
    assert_eq!(
        task.snapshot(),
        refreshed,
        "unchanged refresh must not churn evidence"
    );
    task.record_report().unwrap();
    assert!(
        !task.is_complete(),
        "report cannot complete an unresolved plan phase"
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Keep isolated HOME and root trust pinned through the actual verification process"
)]
async fn shared_check_command_runs_once_for_all_items() {
    use crate::core::config::{EnvGuard, TEST_HOME_LOCK};
    use crate::core::file_placement::{tests::TestDir, PlacementContext};

    let _home = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = TestDir::new();
    let root = fixture.path().join("workspace");
    let home = fixture.path().join("home");
    std::fs::create_dir_all(root.join(".aizen")).unwrap();
    let root = root.canonicalize().unwrap();
    let _env = EnvGuard::set([("AIZEN_HOME", &home)]);
    let command = "echo owned check";
    std::fs::write(
        root.join(".aizen/verify.json"),
        serde_json::json!({"commands": [command]}).to_string(),
    )
    .unwrap();
    let task = todo::TaskContext::private();
    let requirements = (0..9)
        .map(|i| {
            (
                format!("@index:{i}"),
                todo::Requirement::Check {
                    command: command.into(),
                    root: root.to_string_lossy().into_owned(),
                    source_identity: None,
                },
            )
        })
        .collect();
    task.replace_with_requirements(
        (0..9)
            .map(|i| todo::Todo::new(format!("check {i}"), todo::Status::Done))
            .collect(),
        requirements,
    )
    .unwrap();
    let ctx = ExecutionContext::default()
        .with_placement(PlacementContext::new(root.clone(), home).unwrap())
        .with_task(task.clone());
    crate::core::exec_ctx::with_current(ctx.clone(), crate::agent::mcp::trust_project).unwrap();
    let gate = crate::core::exec_ctx::with_current_future(
        ctx.clone(),
        run_checks(&ctx, &root, 5, &[], true),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(gate.accepted(), "{}", gate.output);
    assert_eq!(gate.passed_commands, vec![command]);
    assert_eq!(task.snapshot().checks.len(), 9);
    assert!(task.is_complete());
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Keep isolated HOME and exact root trust pinned across bounded verification attempts"
)]
async fn bounded_multi_root_checks_leave_unvisited_roots_pending() {
    use crate::core::config::{EnvGuard, TEST_HOME_LOCK};
    use crate::core::file_placement::{tests::TestDir, PlacementContext};

    let _home = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = TestDir::new();
    let home = fixture.path().join("home");
    let _env = EnvGuard::set([("AIZEN_HOME", &home)]);
    let task = todo::TaskContext::private();
    let mut roots = Vec::new();
    for i in 0..5 {
        let root = fixture.path().join(format!("project-{i}"));
        std::fs::create_dir_all(root.join(".aizen")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("input.txt"), "original").unwrap();
        std::fs::write(
            root.join(".aizen/verify.json"),
            serde_json::json!({"commands": ["echo owned root check"]}).to_string(),
        )
        .unwrap();
        let ctx = ExecutionContext::default()
            .with_placement(PlacementContext::new(root.clone(), home.clone()).unwrap());
        crate::core::exec_ctx::with_current(ctx, crate::agent::mcp::trust_project).unwrap();
        task.invalidate_effects_at(Some(&root));
        roots.push(root);
    }
    let ctx = ExecutionContext::default()
        .with_placement(PlacementContext::new(roots[0].clone(), home).unwrap())
        .with_task(task.clone());
    let first = crate::core::exec_ctx::with_current_future(
        ctx.clone(),
        run_checks(&ctx, &roots[0], 5, &[], true),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(first.accepted(), "{}", first.output);
    assert_eq!(task.snapshot().checks.len(), 4);
    assert_eq!(
        task.verification_roots(),
        vec![roots[4].to_string_lossy().into_owned()]
    );
    let mut outcome = AgentOutcome {
        speech_final_verified: false,
        final_text: Some("four roots verified".into()),
        iters: 1,
        verification: None,
        stop: StopReason::Done,
    };
    outcome.reconcile_task_completion(&ctx, &TurnCancel::new());
    assert_eq!(outcome.stop, StopReason::Incomplete);
    let last = crate::core::exec_ctx::with_current_future(
        ctx.clone(),
        run_checks(&ctx, &roots[0], 5, &[], true),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(last.accepted(), "{}", last.output);
    assert!(task.is_complete());
    assert_eq!(task.snapshot().checks.len(), 5);
    std::fs::write(roots[0].join("input.txt"), "changed after verification").unwrap();
    revalidate(&task).unwrap();
    assert!(
        !task.is_complete(),
        "the last root cannot certify earlier roots after drift"
    );
}

#[tokio::test]
async fn unsupported_check_is_not_executed_as_shell() {
    let directory = crate::core::file_placement::tests::TestDir::new();
    let ctx = ExecutionContext::default();
    let result = verify_gate::run_completion_ladder_in_context(
        directory.path(),
        1,
        &[],
        true,
        &ctx,
        &["not-a-runtime-known-check".into()],
    )
    .await
    .unwrap();
    assert_eq!(
        result.termination,
        verify_gate::VerifyTermination::Unavailable
    );
    assert!(!result.accepted());
}
