use super::*;
use crate::core::file_placement::tests::TestDir;
use crate::core::repo_lock::LockMode;
use crate::core::speculation::inspection::fixtures;
use crate::core::workspace_txn::{with_test_lock_home, NativeResourceLease};
use std::collections::BTreeMap;

struct Fixture {
    temp: TestDir,
    home: PathBuf,
    identity: WorkspaceIdentity,
}

impl Fixture {
    fn new() -> Self {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("source.txt"), b"source must survive").unwrap();
        let identity = WorkspaceIdentity::discover(&root).unwrap();
        assert!(identity.canonical_git_dir.is_none());
        assert!(identity.canonical_common_git_dir.is_none());
        let home = temp.path().join("home");
        Self {
            temp,
            home,
            identity,
        }
    }

    fn plan(&self) -> PlannedNamespace {
        PlannedNamespace::plan(self.identity.clone(), &self.home).unwrap()
    }

    fn admitted<T>(
        &self,
        body: impl FnOnce(&WorkspaceWriterLease, &PhysicalEffectLease) -> T,
    ) -> T {
        with_test_lock_home(self.temp.path(), || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                self.identity.clone(),
                Duration::ZERO,
                None,
                "namespace fixture",
                "namespace-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "namespace fixture",
                "namespace-parent",
            )
            .unwrap();
            body(&writer, &physical)
        })
    }

    fn namespace(&self) -> NativeNamespace {
        let plan = self.plan();
        self.admitted(|writer, physical| materialize_namespace(&plan, writer, physical).unwrap())
    }
}

fn materialize_namespace(
    plan: &PlannedNamespace,
    writer: &WorkspaceWriterLease,
    physical: &PhysicalEffectLease,
) -> Result<NativeNamespace> {
    plan.verify_admission(writer, physical)?;
    create_metadata_directory(&plan.path)?;
    plan.verify()?;
    let lock = plan.path.join("transaction.lock");
    if !lock.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)?;
    }
    plan.open_existing()?
        .context("disposable historical namespace missing")
}

fn evidence_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
fn planning_and_absent_inspection_create_no_metadata() {
    let fixture = Fixture::new();
    let before: Vec<_> = fs::read_dir(fixture.temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let plan = fixture.plan();
    assert!(plan.open_existing().unwrap().is_none());
    let summary = inspect_existing(fixture.identity.clone(), &fixture.home).unwrap();
    assert_eq!(summary.cohorts, 0);
    assert_eq!(summary.prepared, 0);
    assert!(!fixture.home.exists());
    let after: Vec<_> = fs::read_dir(fixture.temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn admitted_non_git_namespace_owns_only_native_storage_and_reopens() {
    let fixture = Fixture::new();
    let namespace = fixture.namespace();
    assert_eq!(
        namespace.namespace_dir(),
        fixture
            .home
            .join("native-workspaces")
            .join(&fixture.identity.worktree_id)
    );
    assert_eq!(RecoveryNamespace::identity(&namespace), &fixture.identity);
    fixture.admitted(|writer, physical| {
        namespace
            .with_transaction_lock(writer, physical, || {
                assert!(namespace.namespace_dir().join("transaction.lock").is_file());
                Ok(())
            })
            .unwrap();
    });
    let home_entries: Vec<_> = fs::read_dir(&fixture.home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        home_entries,
        vec![std::ffi::OsString::from("native-workspaces")]
    );
    assert!(!fixture.home.join("timemachine").exists());
    assert!(!namespace.namespace_dir().join("ledger.json").exists());
    assert!(!namespace.namespace_dir().join("store.git").exists());
    let reopened = fixture.plan().open_existing().unwrap().unwrap();
    assert_eq!(reopened.namespace_dir(), namespace.namespace_dir());
    assert_eq!(reopened.require_settled().unwrap().cohorts, 0);
    assert_eq!(
        fs::read(fixture.identity.canonical_root.join("source.txt")).unwrap(),
        b"source must survive"
    );
}

#[test]
fn foreign_writer_cannot_create_or_use_a_native_namespace() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let foreign_root = fixture.temp.path().join("foreign");
    fs::create_dir(&foreign_root).unwrap();
    let foreign_identity = WorkspaceIdentity::discover(&foreign_root).unwrap();
    let attempt = |namespace: Option<&NativeNamespace>| {
        with_test_lock_home(fixture.temp.path(), || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                foreign_identity.clone(),
                Duration::ZERO,
                None,
                "foreign fixture",
                "foreign-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "foreign fixture",
                "foreign-parent",
            )
            .unwrap();
            assert!(materialize_namespace(&plan, &writer, &physical).is_err());
            if let Some(namespace) = namespace {
                let mut called = false;
                assert!(namespace
                    .with_transaction_lock(&writer, &physical, || {
                        called = true;
                        Ok(())
                    })
                    .is_err());
                assert!(!called);
            }
        })
    };
    attempt(None);
    assert!(!fixture.home.exists());
    let namespace = fixture.namespace();
    attempt(Some(&namespace));
}

#[test]
fn shared_native_body_cannot_supply_required_exclusive_physical_admission() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    with_test_lock_home(fixture.temp.path(), || {
        let _writer = WorkspaceWriterLease::acquire_identity_scoped(
            fixture.identity.clone(),
            Duration::ZERO,
            None,
            "shared fixture",
            "namespace-parent",
        )
        .unwrap();
        // The shared PhysicalEffectLease is intentionally encapsulated by this real body. There is
        // no constructor or extractor that could smuggle it into namespace admission as exclusive.
        let resource =
            NativeResource::resolve(&fixture.identity.canonical_root.join("source.txt")).unwrap();
        let body = NativeResourceLease::acquire(
            &fixture.identity.canonical_root,
            &[(resource, LockMode::Shared)],
            Duration::ZERO,
            None,
            "namespace-parent/reader",
        )
        .unwrap();
        assert!(PhysicalEffectLease::exclusive(
            Duration::ZERO,
            None,
            "namespace fixture",
            "namespace-parent",
        )
        .is_err());
        assert!(!fixture.home.exists());
        drop(body);
        let physical = PhysicalEffectLease::exclusive(
            Duration::ZERO,
            None,
            "namespace fixture",
            "namespace-parent",
        )
        .unwrap();
        materialize_namespace(&plan, &_writer, &physical).unwrap();
    });
}

#[test]
fn namespace_transaction_contends_on_its_actual_lock_without_running_body() {
    let fixture = Fixture::new();
    let namespace = fixture.namespace();
    let path = namespace.namespace_dir().join("transaction.lock");
    let held = RepoTxnLock::acquire_exclusive(&path, Duration::ZERO).unwrap();
    let mut called = false;
    let error = namespace
        .transaction(Duration::ZERO, || {
            called = true;
            Ok(())
        })
        .unwrap_err();
    assert!(error
        .downcast_ref::<crate::core::repo_lock::LockBusy>()
        .is_some());
    assert!(!called);
    assert!(namespace.require_settled().is_err());
    drop(held);
    namespace
        .transaction(Duration::ZERO, || {
            called = true;
            assert!(RepoTxnLock::acquire_exclusive(&path, Duration::ZERO).is_err());
            Ok(())
        })
        .unwrap();
    assert!(called);
}

#[test]
fn replacing_planned_ancestor_refuses_admission_before_creating_suffix() {
    let fixture = Fixture::new();
    fs::create_dir(&fixture.home).unwrap();
    let plan = fixture.plan();
    let original = fixture.temp.path().join("original-home");
    fs::rename(&fixture.home, &original).unwrap();
    fs::create_dir(&fixture.home).unwrap();
    assert!(plan.open_existing().is_err());
    fixture.admitted(|writer, physical| {
        assert!(materialize_namespace(&plan, writer, physical).is_err())
    });
    assert!(fs::read_dir(&fixture.home).unwrap().next().is_none());
    assert!(fs::read_dir(&original).unwrap().next().is_none());
}

#[test]
fn replacing_admitted_directory_invalidates_its_live_transaction_handle() {
    let fixture = Fixture::new();
    let namespace = fixture.namespace();
    fs::write(
        namespace.namespace_dir().join("retained-evidence"),
        b"original evidence",
    )
    .unwrap();
    let original = fixture.home.join("retained-native");
    fs::rename(namespace.namespace_dir(), &original).unwrap();
    fs::create_dir(namespace.namespace_dir()).unwrap();
    let mut called = false;
    assert!(namespace
        .transaction(Duration::ZERO, || {
            called = true;
            Ok(())
        })
        .is_err());
    assert!(!called);
    assert!(!namespace.namespace_dir().join("transaction.lock").exists());
    assert_eq!(
        fs::read(original.join("retained-evidence")).unwrap(),
        b"original evidence"
    );
}

#[test]
fn workspace_replacement_and_forged_identity_are_refused() {
    let fixture = Fixture::new();
    let mut forged = fixture.identity.clone();
    forged.repo_id.push_str("-foreign");
    let forged_plan = PlannedNamespace::plan(forged, &fixture.home).unwrap();
    assert!(forged_plan.open_existing().is_err());
    let plan = fixture.plan();
    let old_root = fixture.temp.path().join("old-workspace");
    fs::rename(&fixture.identity.canonical_root, &old_root).unwrap();
    fs::create_dir(&fixture.identity.canonical_root).unwrap();
    assert!(plan.open_existing().is_err());
    fixture.admitted(|writer, physical| {
        assert!(materialize_namespace(&plan, writer, physical).is_err())
    });
    assert!(!fixture.home.exists());
    assert_eq!(
        fs::read(old_root.join("source.txt")).unwrap(),
        b"source must survive"
    );
}

#[test]
fn planning_refuses_workspace_overlap_and_incomplete_git_identity() {
    let fixture = Fixture::new();
    assert!(
        PlannedNamespace::plan(fixture.identity.clone(), &fixture.identity.canonical_root).is_err()
    );
    let mut incomplete = fixture.identity.clone();
    incomplete.canonical_git_dir = Some(fixture.temp.path().to_path_buf());
    assert!(PlannedNamespace::plan(incomplete, &fixture.home).is_err());
    let mut invalid = fixture.identity.clone();
    invalid.worktree_id = "../foreign".into();
    assert!(PlannedNamespace::plan(invalid, &fixture.home).is_err());
    assert!(!fixture.home.exists());
}

#[cfg(unix)]
#[test]
fn linked_metadata_ancestor_or_namespace_is_never_followed() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let destination = fixture.temp.path().join("link-target");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("sentinel"), b"untouched").unwrap();
    symlink(&destination, &fixture.home).unwrap();
    assert!(PlannedNamespace::plan(fixture.identity.clone(), &fixture.home).is_err());
    fs::remove_file(&fixture.home).unwrap();
    let plan = fixture.plan();
    fs::create_dir_all(plan.path.parent().unwrap()).unwrap();
    symlink(&destination, &plan.path).unwrap();
    assert!(plan.open_existing().is_err());
    fixture.admitted(|writer, physical| {
        assert!(materialize_namespace(&plan, writer, physical).is_err())
    });
    assert_eq!(
        fs::read(destination.join("sentinel")).unwrap(),
        b"untouched"
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
}

#[test]
fn unknown_native_evidence_blocks_inspection_and_keeps_original_bytes() {
    let fixture = Fixture::new();
    let namespace = fixture.namespace();
    let version = namespace.namespace_dir().join("speculation-v999");
    fs::create_dir(&version).unwrap();
    fs::write(
        version.join("unknown-record"),
        b"future schema must survive",
    )
    .unwrap();
    let before = evidence_bytes(&version);
    let error = inspect_existing(fixture.identity.clone(), &fixture.home).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported future speculation namespace"));
    assert_eq!(evidence_bytes(&version), before);
    assert_eq!(
        fs::read(fixture.identity.canonical_root.join("source.txt")).unwrap(),
        b"source must survive"
    );
}

#[test]
fn pending_native_evidence_blocks_inspection_and_keeps_original_bytes() {
    let fixture = Fixture::new();
    let namespace = fixture.namespace();
    let cohort = fixtures::cohort(namespace.namespace_dir());
    fixtures::write_pending_prepared(&cohort);
    let before = evidence_bytes(&cohort);
    let error = inspect_existing(fixture.identity.clone(), &fixture.home).unwrap_err();
    assert!(format!("{error:#}").contains("has no durable receipt"));
    assert_eq!(evidence_bytes(&cohort), before);
    assert_eq!(
        fs::read(fixture.identity.canonical_root.join("source.txt")).unwrap(),
        b"source must survive"
    );
}
