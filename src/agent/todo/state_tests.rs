use super::*;
use crate::core::file_placement::tests::TestDir;

fn requested(requirement: Requirement) -> TaskContext {
    let task = TaskContext::private();
    let rows = vec![Todo::new("work", Status::Done)];
    task.replace_with_requirements(rows, vec![("@index:0".into(), requirement)])
        .unwrap();
    task
}
fn check() -> Requirement {
    Requirement::Check {
        command: "cargo check".into(),
        root: "workspace".into(),
        source_identity: None,
    }
}
fn record(task: &TaskContext, token: EvidenceToken, result: CheckResult) -> Result<()> {
    task.record_check(token, "cargo check", "workspace", "source-v1", result)
}

#[test]
fn completion_request_waits_for_matching_evidence_without_repeat_update() {
    let task = requested(check());
    assert_eq!(task.snapshot().items[0].status, Status::Pending);
    let token = task.evidence_capture().unwrap();
    record(&task, token, CheckResult::Pass).unwrap();
    assert!(task.is_complete());
    assert_eq!(task.snapshot().items[0].status, Status::Done);
    assert!(task.evidence_captures().is_empty());
}

#[test]
fn focus_stales_evidence_without_weakening_or_inventing_obligations() {
    let fresh = TaskContext::private();
    fresh.invalidate_workspace_evidence();
    fresh.snapshot().validate().unwrap();
    let task = requested(check());
    let prior = task.evidence_capture().unwrap();
    record(&task, prior.clone(), CheckResult::Pass).unwrap();
    assert!(task.is_complete());
    let before = task.snapshot();
    task.invalidate_workspace_evidence();
    assert!(!task.is_complete());
    assert_eq!(task.snapshot().checks.len(), before.checks.len());
    assert_eq!(task.snapshot().obligations.len(), before.obligations.len());
    assert!(record(&task, prior, CheckResult::Pass).is_err());
    record(&task, task.evidence_capture().unwrap(), CheckResult::Pass).unwrap();
    assert!(task.is_complete());
}

#[test]
fn receipt_rejects_foreign_owner_and_later_effect() {
    let first = requested(check());
    let second = requested(check());
    let token = first.evidence_capture().unwrap();
    assert!(record(&second, token.clone(), CheckResult::Pass).is_err());
    first.invalidate_effects_at(Some(Path::new("workspace")));
    assert!(record(&first, token, CheckResult::Pass).is_err());
}

#[test]
fn later_failed_check_supersedes_pass_without_erasing_history() {
    let task = requested(check());
    let token = task.evidence_capture().unwrap();
    record(&task, token.clone(), CheckResult::Pass).unwrap();
    record(&task, token, CheckResult::Fail).unwrap();
    assert!(!task.is_complete());
    assert_eq!(task.snapshot().checks.len(), 2);
    assert_eq!(task.snapshot().items[0].status, Status::Pending);
}

#[test]
fn status_save_and_turn_seal_do_not_stale_receipt() {
    let task = requested(check());
    record(&task, task.evidence_capture().unwrap(), CheckResult::Pass).unwrap();
    let epoch = task.effect_epoch();
    task.replace(task.snapshot().items).unwrap();
    task.mark_saved(&task.snapshot());
    assert!(!task.is_dirty());
    assert_eq!(
        task.finish_turn(TurnEnd::Completed),
        FinishDisposition::Completed
    );
    assert_eq!(task.effect_epoch(), epoch);
    assert!(task.is_complete());
    assert!(task.is_dirty());
}

#[test]
fn omitted_pending_or_weakened_check_never_disappears() {
    let task = TaskContext::private();
    task.replace(vec![Todo::new("not done", Status::Pending)])
        .unwrap();
    task.replace(Vec::new()).unwrap();
    assert!(task.completion_pending());
    assert!(task.snapshot().obligations.iter().any(|o| o.abandoned));
    task.finish_turn(TurnEnd::Incomplete);
    assert!(task.snapshot().carry_over);
    let task = requested(check());
    let rows = vec![Todo {
        status: Status::Done,
        ..task.snapshot().items[0].clone()
    }];
    let id = rows[0].id.clone();
    task.replace_with_requirements(rows, vec![(id, Requirement::Report)])
        .unwrap();
    task.record_report().unwrap();
    assert!(!task.is_complete());
}

#[test]
fn root_scoped_mutations_survive_empty_checklist_and_require_each_root() {
    let task = TaskContext::private();
    task.invalidate_effects_at(Some(Path::new("root-a")));
    task.invalidate_effects_at(Some(Path::new("root-b")));
    assert_eq!(task.verification_roots().len(), 2);
    let tokens = task.evidence_captures();
    for token in tokens {
        if token.matches_check("check", "root-a") {
            task.record_check(token, "check", "root-a", "identity-a", CheckResult::Pass)
                .unwrap();
        }
    }
    assert!(!task.is_complete());
    task.finish_turn(TurnEnd::Incomplete);
    assert!(task.snapshot().carry_over);
    assert_eq!(task.verification_roots(), vec!["root-b"]);
    let token = task.evidence_capture().unwrap();
    task.record_check(token, "check", "root-b", "identity-b", CheckResult::Pass)
        .unwrap();
    assert!(task.is_complete());
    task.invalidate_effects_at(None);
    assert!(!task.is_complete());
    assert!(task
        .evidence_captures()
        .iter()
        .any(|t| t.scope_root().is_none()));
    assert!(
        !task.handoff_eligible(),
        "unknown effects cannot authorize child handoff"
    );
}

#[test]
fn repeated_path_effects_and_restore_invalidate_tokens() {
    let task = requested(check());
    task.invalidate_effects_at(Some(Path::new("workspace")));
    let epoch = task.effect_epoch();
    task.invalidate_effects_at(Some(Path::new("workspace")));
    assert_eq!(task.effect_epoch(), epoch + 1);
    let token = task.evidence_capture().unwrap();
    let saved = task.snapshot();
    task.restore(saved).unwrap();
    assert!(record(&task, token, CheckResult::Pass).is_err());
}

#[test]
fn report_does_not_satisfy_check_and_question_needs_no_receipt() {
    let empty = TaskContext::private();
    empty.record_report().unwrap();
    assert!(empty.is_complete());
    assert!(empty.snapshot().reports.is_empty());
    let task = requested(check());
    task.record_report().unwrap();
    assert!(!task.is_complete());
    assert!(task.snapshot().reports.is_empty());
    let report = requested(Requirement::Report);
    assert!(report.only_report_pending());
    report.record_report().unwrap();
    assert!(report.is_complete());
}

#[test]
fn native_observation_is_scoped_and_external_drift_stales_it() {
    let dir = TestDir::new();
    let path = dir.path().join("source.rs");
    std::fs::write(&path, b"old").unwrap();
    let task = requested(Requirement::Observe {
        target: "source.rs".into(),
    });
    let token = task.observation_capture("source.rs").unwrap();
    task.record_file_observation(
        token,
        "source.rs",
        &path,
        &crate::core::persist::FileFingerprint::for_bytes(b"old"),
    )
    .unwrap();
    assert!(task.is_complete());
    assert_eq!(
        task.completion_basis(&task.snapshot().items[0].id),
        CompletionBasis::Observed
    );
    std::fs::write(&path, b"new").unwrap();
    assert!(task.revalidate_sources().is_err());
    assert!(!task.is_complete());
    assert!(
        !task.needs_check(),
        "read-only observation drift does not require a build"
    );
}

#[test]
fn observation_cannot_satisfy_check_or_an_unrelated_target() {
    let observed = requested(Requirement::Observe {
        target: "a.rs".into(),
    });
    let checked = requested(check());
    let token = observed.observation_capture("a.rs").unwrap();
    assert!(observed.observation_capture("b.rs").is_none());
    assert!(checked
        .record_observation_with_token(token.clone(), "a.rs")
        .is_err());
    assert!(observed
        .record_observation_with_token(token.clone(), "b.rs")
        .is_err());
    observed
        .record_observation_with_token(token, "a.rs")
        .unwrap();
    assert!(observed.is_complete());
    assert!(!checked.is_complete());
}

#[test]
fn disk_restore_keeps_history_but_not_live_authority() {
    let task = requested(check());
    record(&task, task.evidence_capture().unwrap(), CheckResult::Pass).unwrap();
    let saved = task.snapshot();
    let copy: TaskState = serde_json::from_value(serde_json::to_value(&saved).unwrap()).unwrap();
    assert_eq!(copy, saved);
    let restored = TaskContext::private();
    restored.restore(copy).unwrap();
    assert!(!restored.is_complete());
    assert!(restored.is_dirty());
    assert_eq!(restored.snapshot().checks.len(), 1);
}

#[test]
fn legacy_claims_migrate_without_proof_and_inactive_stays_inactive() {
    for active in [false, true] {
        let old = serde_json::json!({"version":1,"id":"task-old","revision":3,"active":active,"carry_over":active,"abandoned":false,"next_item":2,"items":[{"id":"item-1","content":"legacy","status":"done"}]});
        let state: TaskState = serde_json::from_value(old).unwrap();
        assert_eq!(state.version, 3);
        assert_eq!(state.active, active);
        assert_eq!(state.legacy_claims[0].status, Status::Done);
        assert!(state.checks.is_empty());
        assert!(state.pending());
        if active {
            assert_eq!(state.items[0].status, Status::Pending);
        }
    }
}

#[test]
fn malformed_or_future_state_is_refused_atomically() {
    let task = requested(check());
    let old = task.snapshot();
    let mut future = serde_json::to_value(&old).unwrap();
    future["version"] = 99.into();
    assert!(serde_json::from_value::<TaskState>(future).is_err());
    let mut invalid = old.clone();
    invalid.next_item = 0;
    assert!(task.restore(invalid).is_err());
    assert_eq!(task.snapshot(), old);
}

#[test]
fn exact_save_ack_does_not_hide_a_concurrent_mutation() {
    let task = requested(Requirement::Report);
    let before = task.snapshot();
    task.record_report().unwrap();
    task.mark_saved(&before);
    assert!(task.is_dirty());
    task.mark_saved(&task.snapshot());
    assert!(!task.is_dirty());
}

#[test]
fn human_reset_does_not_retain_model_abandonment() {
    let task = requested(check());
    task.replace(Vec::new()).unwrap();
    assert!(!task.is_complete());
    task.clear();
    assert_eq!(task.snapshot(), TaskState::default());
    assert!(task.is_complete());
}
