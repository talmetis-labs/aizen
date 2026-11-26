use super::*;
use crate::core::file_placement::tests::TestDir;

const PATH: &str = "docs/plans/work.md";
const ID: &str = "p-0123456789abcdef0123456789abcdef";

fn document(id: &str, phases: &[&str]) -> String {
    format!(
        "---\naizen_plan: 1\nid: {id}\n---\n{}",
        phases
            .iter()
            .map(|phase| format!("<!-- aizen:phase {phase} -->\n## {phase}\n"))
            .collect::<String>()
    )
}
fn fixture() -> (TestDir, std::path::PathBuf) {
    let temp = TestDir::new();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("docs/plans")).unwrap();
    std::fs::write(root.join(PATH), document(ID, &["inspect", "implement"])).unwrap();
    (temp, root)
}
fn linked(root: &Path) -> TaskContext {
    let task = TaskContext::private();
    task.replace_with_plan(
        vec![Todo::new("summarize", Status::Done)],
        vec![("@index:0".into(), Requirement::Report)],
        Some(PlanAttachment::load(root, PATH).unwrap()),
        vec![("@index:0".into(), "inspect".into())],
    )
    .unwrap();
    task
}

#[test]
fn completed_planning_followup_keeps_the_saved_contract() {
    use crate::core::session_control::{ExecutionMode, PlanTarget};
    let (_temp, root) = fixture();
    let task = linked(&root);
    task.record_report().unwrap();
    task.control()
        .transition(
            ExecutionMode::Planning,
            Some(PlanTarget {
                root: root.to_string_lossy().into_owned(),
                path: PATH.into(),
                id: ID.into(),
            }),
            |_| Ok(()),
        )
        .unwrap();
    task.finish_turn(TurnEnd::Completed);
    let saved = task.snapshot();
    task.mark_saved(&saved);
    assert!(
        !task.begin_user_turn(),
        "plan delivery must not discard the selected contract"
    );
    assert_eq!(task.snapshot(), saved);
    assert!(task.snapshot().plan.unwrap().usable());
}

#[test]
fn plan_metadata_never_completes_without_evidence() {
    let (_temp, root) = fixture();
    let task = linked(&root);
    assert!(!task.is_complete());
    task.record_report().unwrap();
    assert!(task.is_complete());
    assert!(task.snapshot().checks.is_empty());
    assert!(task.snapshot().obligations.is_empty());
}

#[test]
fn external_edits_stale_evidence_once_and_missing_phases_stay_unresolved() {
    let (_temp, root) = fixture();
    let task = linked(&root);
    task.record_report().unwrap();
    let before = task.snapshot();
    task.refresh_plan().unwrap();
    assert_eq!(task.snapshot(), before);
    std::fs::write(root.join(PATH), document(ID, &["implement"])).unwrap();
    task.refresh_plan().unwrap();
    assert_eq!(task.effect_epoch(), before.effect_epoch + 1);
    assert!(!task.is_complete());
    task.record_report().unwrap();
    assert!(
        !task.is_complete(),
        "report cannot repair a broken phase link"
    );
    assert_eq!(task.snapshot().item_phases, before.item_phases);
    let refreshed = task.snapshot();
    task.refresh_plan().unwrap();
    assert_eq!(task.snapshot(), refreshed);
}

#[test]
fn changed_identity_needs_explicit_reattachment_even_if_changed_back() {
    let (_temp, root) = fixture();
    let task = linked(&root);
    std::fs::write(
        root.join(PATH),
        document("p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &["inspect"]),
    )
    .unwrap();
    task.refresh_plan().unwrap();
    assert_eq!(
        task.snapshot().plan.unwrap().freshness,
        PlanFreshness::IdentityChanged
    );
    std::fs::write(root.join(PATH), document(ID, &["inspect"])).unwrap();
    task.refresh_plan().unwrap();
    assert_eq!(
        task.snapshot().plan.unwrap().freshness,
        PlanFreshness::IdentityChanged
    );
    let rows = task.snapshot().items;
    task.replace_with_plan(
        rows,
        vec![],
        Some(PlanAttachment::load(&root, PATH).unwrap()),
        vec![],
    )
    .unwrap();
    assert!(task.snapshot().plan.unwrap().usable());
    assert!(!task.is_complete());
}

#[test]
fn malformed_content_and_missing_file_are_idempotent_invalidations() {
    let (_temp, root) = fixture();
    let task = linked(&root);
    for text in ["malformed one", "malformed two"] {
        let old_epoch = task.effect_epoch();
        std::fs::write(root.join(PATH), text).unwrap();
        task.refresh_plan().unwrap();
        assert_eq!(task.effect_epoch(), old_epoch + 1);
        let unchanged = task.snapshot();
        task.refresh_plan().unwrap();
        assert_eq!(task.snapshot(), unchanged);
        assert_eq!(unchanged.plan.unwrap().freshness, PlanFreshness::Invalid);
    }
    std::fs::remove_file(root.join(PATH)).unwrap();
    task.refresh_plan().unwrap();
    let missing = task.snapshot();
    assert_eq!(
        missing.plan.as_ref().unwrap().freshness,
        PlanFreshness::Missing
    );
    task.refresh_plan().unwrap();
    assert_eq!(task.snapshot(), missing);
}

#[test]
fn bad_phase_proposal_is_atomic_and_omission_retains_history_link() {
    let (_temp, root) = fixture();
    let task = linked(&root);
    let before = task.snapshot();
    let mut rows = before.items.clone();
    rows[0].content = "renamed but invalid proposal".into();
    assert!(task
        .replace_with_plan(
            rows,
            vec![],
            None,
            vec![(before.items[0].id.clone(), "missing".into())]
        )
        .is_err());
    assert_eq!(task.snapshot(), before);
    task.replace(Vec::new()).unwrap();
    let omitted = task.snapshot();
    assert_eq!(omitted.item_phases, before.item_phases);
    assert_eq!(omitted.history.len(), 1);
    assert!(!task.is_complete());
}

#[test]
fn disk_attachment_is_historical_and_v2_hint_never_becomes_identity() {
    let (_temp, root) = fixture();
    let task = linked(&root);
    task.record_report().unwrap();
    let saved = serde_json::to_value(task.snapshot()).unwrap();
    let restored: TaskState = serde_json::from_value(saved.clone()).unwrap();
    assert!(!restored.plan.as_ref().unwrap().usable());
    let owner = TaskContext::private();
    owner.restore(restored).unwrap();
    owner.refresh_plan().unwrap();
    assert!(owner.snapshot().plan.unwrap().usable());
    assert!(
        !owner.is_complete(),
        "disk restore cannot regain report authority"
    );
    let mut legacy = saved;
    legacy["version"] = 2.into();
    legacy.as_object_mut().unwrap().remove("plan");
    legacy.as_object_mut().unwrap().remove("item_phases");
    legacy["plan_path"] = PATH.into();
    legacy["phase_id"] = "inspect".into();
    let migrated: TaskState = serde_json::from_value(legacy).unwrap();
    assert_eq!(migrated.version, 3);
    assert!(migrated.plan.is_none());
    assert!(migrated.item_phases.is_empty());
    assert_eq!(migrated.plan_path.as_deref(), Some(PATH));
}
