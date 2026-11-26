use super::*;
use crate::core::file_placement::tests::TestDir;
use crate::core::session_control::{ControlContext, ExecutionMode};

const ID: &str = "p-0123456789abcdef0123456789abcdef";
const OTHER: &str = "p-fedcba9876543210fedcba9876543210";
const PATH: &str = "docs/plans/work.md";

fn document(id: &str, body: &str) -> String {
    format!("---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase investigate -->\n## Investigate\n{body}\n")
}

struct Fixture {
    _dir: TestDir,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = TestDir::new();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let target = self.root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, bytes).unwrap();
    }
    fn target(&self) -> PlanTarget {
        PlanTarget {
            root: self.root.to_string_lossy().into_owned(),
            path: PATH.into(),
            id: ID.into(),
        }
    }
}

#[test]
fn allocation_is_random_absent_and_never_a_saved_document() {
    let f = Fixture::new();
    let first = select_target(&f.root, None, None).unwrap();
    let second = select_target(&f.root, None, None).unwrap();
    assert_ne!(first.target.id, second.target.id);
    assert_ne!(first.target.path, second.target.path);
    first.target.validate().unwrap();
    assert!(first.observation.is_none());
    assert!(second.observation.is_none());
    assert!(
        !f.root.join("docs").exists(),
        "target allocation must not create even its parents"
    );
}

#[test]
fn explicit_absent_selection_keeps_exact_path_without_writing() {
    let f = Fixture::new();
    let selection = select_with_id(&f.root, Some(PATH), None, || Ok(ID.into())).unwrap();
    assert_eq!(selection.target, f.target());
    assert!(selection.observation.is_none());
    assert!(!f.root.join("docs").exists());
}

#[test]
fn explicit_existing_identity_comes_only_from_checked_disk_bytes() {
    let f = Fixture::new();
    let bytes = document(OTHER, "existing document");
    f.write(PATH, &bytes);
    let old = f.target();
    let selection = select_with_id(&f.root, Some(PATH), Some(&old), || {
        panic!("existing selection needs no RNG")
    })
    .unwrap();
    assert_eq!(selection.target.id, OTHER);
    let observation = selection.observation.unwrap();
    assert_eq!(observation.parsed.id, OTHER);
    assert_eq!(
        observation.fingerprint,
        plan_document::fingerprint(bytes.as_bytes())
    );
    assert_eq!(std::fs::read_to_string(f.root.join(PATH)).unwrap(), bytes);
}

#[test]
fn omitted_path_reuses_same_id_after_editor_atomic_save() {
    let f = Fixture::new();
    f.write(PATH, document(ID, "before"));
    let old = f.target();
    let first = select_target(&f.root, None, Some(&old)).unwrap();
    crate::core::persist::atomic_write(&f.root.join(PATH), document(ID, "editor save").as_bytes())
        .unwrap();
    let refreshed = select_target(&f.root, None, Some(&old)).unwrap();
    assert_eq!(first.target, refreshed.target);
    assert_ne!(
        first.observation.unwrap().fingerprint,
        refreshed.observation.unwrap().fingerprint
    );
}

#[test]
fn omitted_path_cannot_adopt_changed_identity() {
    let f = Fixture::new();
    f.write(PATH, document(OTHER, "another plan"));
    let old = f.target();
    let error = select_target(&f.root, None, Some(&old)).unwrap_err();
    assert!(error.to_string().contains("identity changed"));
    assert_eq!(old.id, ID);
}

#[test]
fn missing_old_selection_allocates_a_new_target_not_fabricated_observation() {
    let f = Fixture::new();
    let old = f.target();
    let next = select_with_id(&f.root, None, Some(&old), || Ok(OTHER.into())).unwrap();
    assert_eq!(next.target.id, OTHER);
    assert_ne!(next.target.path, old.path);
    assert!(next.observation.is_none());
    assert!(!f.root.join("docs").exists());
}

#[test]
fn legacy_malformed_oversized_and_non_utf8_existing_files_are_errors() {
    let f = Fixture::new();
    let old = f.target();
    for bytes in [
        b"# Legacy notes\nNo migration has been requested.\n".to_vec(),
        document(ID, "body")
            .replace("aizen_plan: 1", "aizen_plan: 2")
            .into_bytes(),
        document(ID, "body")
            .replace("id: ", "id: invalid\nid: ")
            .into_bytes(),
        document(ID, &"x".repeat(65_536)).into_bytes(),
        vec![0xff, 0xfe],
    ] {
        f.write(PATH, &bytes);
        assert!(select_target(&f.root, Some(PATH), None).is_err());
        assert!(select_target(&f.root, None, Some(&old)).is_err());
        assert_eq!(std::fs::read(f.root.join(PATH)).unwrap(), bytes);
    }
}

#[test]
fn rejects_paths_outside_canonical_plan_namespace_and_missing_nested_parents() {
    let f = Fixture::new();
    for path in [
        "../work.md",
        "docs/plans/../work.md",
        "docs/plans//work.md",
        "docs/plans/./work.md",
        "docs\\plans\\work.md",
        "docs/plans/work.txt",
        "docs/plans/nested/work.md",
    ] {
        assert!(
            select_target(&f.root, Some(path), None).is_err(),
            "accepted {path}"
        );
    }
    assert!(select_target(&f.root, f.root.join(PATH).to_str(), None).is_err());
    assert!(!f.root.join("docs").exists());
    std::fs::create_dir_all(f.root.join("docs/plans/nested")).unwrap();
    let selection = select_target(&f.root, Some("docs/plans/nested/work.md"), None).unwrap();
    assert!(selection.observation.is_none());
    assert!(!f.root.join("docs/plans/nested/work.md").exists());
}

#[test]
fn omitted_selection_cannot_cross_workspaces() {
    let f = Fixture::new();
    let other = Fixture::new();
    other.write(PATH, document(ID, "other workspace"));
    assert!(select_target(&f.root, None, Some(&other.target())).is_err());
    assert!(!f.root.join("docs").exists());
    // A human explicit path intentionally selects inside this workspace, not the old one.
    let explicit = select_target(&f.root, Some(PATH), Some(&other.target())).unwrap();
    assert_eq!(explicit.target.root, f.root.to_string_lossy());
}

#[test]
fn random_collision_is_not_overwritten_and_retry_count_is_bounded() {
    let f = Fixture::new();
    let collision = format!("docs/plans/{ID}.md");
    f.write(&collision, "unrelated legacy content");
    let mut calls = 0;
    let selected = select_with_id(&f.root, None, None, || {
        calls += 1;
        Ok(if calls == 1 { ID } else { OTHER }.into())
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(selected.target.id, OTHER);
    assert!(selected.observation.is_none());
    assert_eq!(
        std::fs::read_to_string(f.root.join(&collision)).unwrap(),
        "unrelated legacy content"
    );
    let mut calls = 0;
    assert!(select_with_id(&f.root, None, None, || {
        calls += 1;
        Ok(ID.into())
    })
    .is_err());
    assert_eq!(calls, ALLOCATION_ATTEMPTS);
}

#[test]
fn rng_failure_returns_error_without_disk_mutation_or_predictable_fallback() {
    let f = Fixture::new();
    assert!(select_with_id(&f.root, None, None, || anyhow::bail!(
        "injected RNG failure"
    ))
    .is_err());
    assert!(select_with_id(&f.root, Some(PATH), None, || anyhow::bail!(
        "injected RNG failure"
    ))
    .is_err());
    assert!(select_with_id(&f.root, None, None, || Ok("bad-id".into())).is_err());
    assert!(!f.root.join("docs").exists());
}

#[test]
fn selecting_neither_changes_control_nor_bypasses_failed_transition_persistence() {
    let f = Fixture::new();
    let control = ControlContext::default();
    let before = control.snapshot();
    let selection = select_target(&f.root, None, None).unwrap();
    assert_eq!(control.snapshot(), before);
    let old_policy = control.policy();
    assert!(control
        .transition(
            ExecutionMode::Planning,
            Some(selection.target),
            |_| anyhow::bail!("disk full")
        )
        .is_err());
    assert!(old_policy.admit().is_err());
    assert_eq!(control.snapshot(), before);
    assert!(!f.root.join("docs").exists());
}

#[test]
fn hardlinked_plan_is_not_a_valid_selection() {
    let f = Fixture::new();
    f.write(PATH, document(ID, "hardlink"));
    match std::fs::hard_link(f.root.join(PATH), f.root.join("alias.md")) {
        Ok(()) => assert!(select_target(&f.root, Some(PATH), None).is_err()),
        Err(error) => eprintln!("SKIP hardlink fixture: {error}"),
    }
}

#[cfg(unix)]
#[test]
fn refuses_linked_workspace_parent_leaf_and_dangling_leaf() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let external = Fixture::new();
    external.write(PATH, document(ID, "external"));
    symlink(&external.root, f.root.join("linked-root")).unwrap();
    assert!(select_target(&f.root.join("linked-root"), Some(PATH), None).is_err());
    symlink(external.root.join("docs"), f.root.join("docs")).unwrap();
    assert!(select_target(&f.root, Some(PATH), None).is_err());
    std::fs::remove_file(f.root.join("docs")).unwrap();
    std::fs::create_dir_all(f.root.join("docs/plans")).unwrap();
    symlink(external.root.join(PATH), f.root.join(PATH)).unwrap();
    assert!(select_target(&f.root, Some(PATH), None).is_err());
    std::fs::remove_file(f.root.join(PATH)).unwrap();
    symlink(external.root.join("missing.md"), f.root.join(PATH)).unwrap();
    assert!(select_target(&f.root, Some(PATH), None).is_err());
}

#[cfg(windows)]
#[test]
fn refuses_windows_reparse_parent_when_symlink_privilege_is_available() {
    let f = Fixture::new();
    let external = Fixture::new();
    external.write(PATH, document(ID, "external"));
    match std::os::windows::fs::symlink_dir(external.root.join("docs"), f.root.join("docs")) {
        Ok(()) => assert!(select_target(&f.root, Some(PATH), None).is_err()),
        Err(error) => eprintln!("SKIP Windows directory-symlink fixture: {error}"),
    }
}
