use super::*;
use crate::agent::todo::{PlanAttachment, Requirement, Status, TaskContext, Todo};
use crate::core::file_placement::tests::TestDir;

fn fixture() -> (TestDir, std::path::PathBuf, TaskContext) {
    let temp = TestDir::new();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("docs/plans")).unwrap();
    std::fs::write(root.join("docs/plans/ui.md"), "---\naizen_plan: 1\nid: p-0123456789abcdef0123456789abcdef\n---\n<!-- aizen:phase inspect -->\n").unwrap();
    let task = TaskContext::private();
    task.replace_with_plan(
        vec![Todo::new("report", Status::Done)],
        vec![("@index:0".into(), Requirement::Report)],
        Some(PlanAttachment::load(&root, "docs/plans/ui.md").unwrap()),
        vec![("@index:0".into(), "inspect".into())],
    )
    .unwrap();
    (temp, root, task)
}

#[test]
fn plan_projection_uses_only_the_supplied_snapshot() {
    let (_temp, root, task) = fixture();
    task.record_report().unwrap();
    let snapshot = task.snapshot();
    let items = vec![(2, "report".to_owned())];
    std::fs::remove_file(root.join("docs/plans/ui.md")).unwrap();
    task.refresh_plan().unwrap();
    let event = plan_projection(&items, Some(&snapshot));
    assert_eq!(event["task_revision"], snapshot.revision);
    assert_eq!(event["items"][0]["completion_basis"], "reported");
    assert_eq!(event["items"][0]["phase"]["resolved"], true);
    assert_eq!(event["document"]["validation"], "validated at last refresh");
    assert_eq!(event["document"]["usable"], true);
    let current = task.snapshot();
    let missing = plan_projection(&[(0, "report".to_owned())], Some(&current));
    assert_eq!(missing["items"][0]["phase"]["resolved"], false);
    assert_eq!(missing["document"]["freshness"], "missing");
    assert_eq!(missing["document"]["usable"], false);
    assert!(plan_document_summary(&snapshot)
        .unwrap()
        .contains("validated at last refresh"));
    assert!(plan_document_summary(&current)
        .unwrap()
        .contains("not usable"));
}

#[test]
fn delayed_rows_never_borrow_another_snapshots_metadata() {
    let (_temp, _root, task) = fixture();
    let event = plan_projection(&[(1, "another task".to_owned())], Some(&task.snapshot()));
    assert_eq!(event["items"][0]["status"], "in_progress");
    assert_eq!(event["items"][0]["text"], "another task");
    assert!(event.get("document").is_none());
    assert!(event["items"][0].get("id").is_none());
    assert!(event["items"][0].get("phase").is_none());
}

#[test]
fn unsaved_draft_has_no_document_projection_or_check_evidence() {
    let mut draft = crate::agent::architect::parse_plan("Implement the parser", "request");
    crate::agent::architect::prepare_document("request", &mut draft, "planner", None).unwrap();
    let task = TaskContext::private();
    let event = plan_projection(&[], Some(&task.snapshot()));
    assert!(event["document"].is_null());
    assert!(plan_document_summary(&task.snapshot()).is_none());
    assert!(task.snapshot().checks.is_empty());
}
