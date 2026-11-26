use super::*;
use crate::core::file_placement::tests::TestDir;
use crate::core::workspace_txn::with_test_lock_home;

struct Fixture {
    temp: TestDir,
    home: PathBuf,
    identity: WorkspaceIdentity,
}

impl Fixture {
    fn new() -> Option<Self> {
        if crate::core::gitx::git_exe().is_none() {
            eprintln!("SKIP legacy evidence fixture: Git unavailable");
            return None;
        }
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        // A minimal unborn repository; no subprocess or developer-repository mutation needed.
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(
            root.join(".git/config"),
            b"[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .unwrap();
        fs::write(root.join("source.txt"), b"retain exact source").unwrap();
        let identity = WorkspaceIdentity::discover(&root).unwrap();
        assert!(identity.canonical_git_dir.is_some());
        let home = temp.path().join("home");
        Some(Self {
            temp,
            home,
            identity,
        })
    }

    fn plan(&self) -> PlannedLegacyNamespace {
        PlannedLegacyNamespace::plan(self.identity.clone(), &self.home).unwrap()
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
                "legacy fixture",
                "legacy-evidence-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "legacy fixture",
                "legacy-evidence-parent",
            )
            .unwrap();
            body(&writer, &physical)
        })
    }

    fn namespace(&self) -> LegacyNamespace {
        self.admitted(|writer, physical| self.plan().admit(writer, physical).unwrap())
    }
}

#[test]
fn absent_legacy_evidence_never_creates_registry_namespace_or_store() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let plan = fixture.plan();
    assert!(plan.open_existing().unwrap().is_none());
    assert!(!fixture.home.exists());
    assert_eq!(
        fs::read(fixture.identity.canonical_root.join("source.txt")).unwrap(),
        b"retain exact source"
    );
}

#[test]
fn reopened_bridge_owns_original_locks_without_opening_history_or_recovering_journal() {
    use crate::core::repo_lock::LockBusy;
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let namespace = fixture.namespace();
    let directory = namespace.namespace_dir();
    let repo = directory.parent().unwrap().parent().unwrap();
    // Garbage history must not be read; a pending journal must not be removed or recovered.
    fs::write(
        directory.join("ledger.json"),
        b"history deliberately unreadable",
    )
    .unwrap();
    let journal = b"future or partial checkpoint transaction";
    fs::write(directory.join("journal.json"), journal).unwrap();
    let registry = fixture.home.join("timemachine/registry.json");
    let registry_before = fs::read(&registry).unwrap();
    let reopened = fixture.plan().open_existing().unwrap().unwrap();
    fixture.admitted(|writer, physical| {
        reopened
            .with_transaction_lock(writer, physical, || {
                for path in [repo.join("store.lock"), directory.join("transaction.lock")] {
                    assert!(RepoTxnLock::acquire_exclusive(&path, Duration::ZERO)
                        .unwrap_err()
                        .downcast_ref::<LockBusy>()
                        .is_some());
                }
                assert!(require_no_checkpoint_journal(directory).is_err());
                Ok(())
            })
            .unwrap();
    });
    assert_eq!(fs::read(&registry).unwrap(), registry_before);
    assert_eq!(fs::read(directory.join("journal.json")).unwrap(), journal);
    assert_eq!(
        fs::read(directory.join("ledger.json")).unwrap(),
        b"history deliberately unreadable"
    );
    assert!(!repo.join("store.git").exists());
    assert!(!directory.join("empty-hooks").exists());
    RepoTxnLock::acquire_exclusive(&directory.join("transaction.lock"), Duration::ZERO).unwrap();
}

#[test]
fn missing_registry_cannot_hide_checkpoint_or_future_speculation_evidence() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let namespace = fixture.namespace();
    let registry = fixture.home.join("timemachine/registry.json");
    // Fixture-only removal models the registry already being missing at restart.
    fs::remove_file(&registry).unwrap();
    let journal = namespace.namespace_dir().join("journal.json");
    fs::write(&journal, b"unresolved journal").unwrap();
    assert!(PlannedLegacyNamespace::plan(fixture.identity.clone(), &fixture.home).is_err());
    assert_eq!(fs::read(&journal).unwrap(), b"unresolved journal");
    fs::remove_file(&journal).unwrap();
    fs::create_dir(namespace.namespace_dir().join("speculation-v99")).unwrap();
    assert!(PlannedLegacyNamespace::plan(fixture.identity.clone(), &fixture.home).is_err());
    assert!(!registry.exists());
}

#[test]
fn registry_retarget_refuses_before_callback_and_preserves_both_domains() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let namespace = fixture.namespace();
    let plan = fixture.plan();
    let root = fixture.home.join("timemachine");
    let mut registry = read_registry(&root).unwrap();
    registry.repos[0].worktrees[0].worktree_id = "wt-ffffffffffffffff".into();
    let changed = serde_json::to_vec(&registry).unwrap();
    fs::write(root.join("registry.json"), &changed).unwrap();
    assert!(plan.open_existing().is_err());
    fixture.admitted(|writer, physical| {
        assert!(namespace
            .with_transaction_lock(writer, physical, || -> Result<()> {
                panic!("stale binding must not reach callback")
            })
            .is_err());
    });
    assert_eq!(fs::read(root.join("registry.json")).unwrap(), changed);
    assert!(namespace.namespace_dir().exists());
    assert!(!root
        .join(&registry.repos[0].repo_id)
        .join("worktrees/wt-ffffffffffffffff")
        .exists());
}

#[test]
fn corrupt_future_oversized_or_nonregular_registry_is_retained() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let root = fixture.home.join("timemachine");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("registry.json");
    for bytes in [
        b"partial json".to_vec(),
        br#"{"schema_version":99,"repos":[]}"#.to_vec(),
        vec![b' '; MAX_REGISTRY_BYTES as usize + 1],
    ] {
        fs::write(&path, &bytes).unwrap();
        assert!(read_registry(&root).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(read_registry(&root).is_err());
    assert!(path.is_dir());
}

#[test]
fn current_speculation_admission_refuses_any_legacy_checkpoint_journal() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let namespace = fixture.namespace();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
    fixture.admitted(|writer, physical| {
        require_settled(&fixture.identity.canonical_root, writer, physical).unwrap();
        for bytes in [
            b"not-json".as_slice(),
            br#"{"schema_version":99}"#.as_slice(),
            br#"{"schema_version":1,"phase":"ledger_committed"}"#.as_slice(),
        ] {
            fs::write(namespace.namespace_dir().join("journal.json"), bytes).unwrap();
            let error =
                require_settled(&fixture.identity.canonical_root, writer, physical).unwrap_err();
            assert!(
                error.to_string().contains("no automatic recovery"),
                "{error:#}"
            );
            assert_eq!(
                fs::read(namespace.namespace_dir().join("journal.json")).unwrap(),
                bytes
            );
        }
    });
    assert_eq!(
        fs::read(fixture.identity.canonical_root.join("source.txt")).unwrap(),
        b"retain exact source"
    );
}
