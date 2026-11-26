//! Restart/native evidence regressions, independent of the retired checkpoint product.

use super::{inspect_speculation_recovery, record_speculation_recovery_observation};
use crate::core::file_placement::tests::TestDir;
use crate::core::persist::FileFingerprint;
use crate::core::speculation::evidence::inventory_revision as recovery_inventory_digest;
use crate::core::speculation::inspection::fixtures::*;
use crate::core::speculation::inspection::{RecoveryImageClass, RecoveryReportStatus};
use crate::core::speculation::legacy::{
    self, require_settled_speculation, store_has_speculation_evidence, LegacyNamespace,
    PlannedLegacyNamespace,
};
use crate::core::speculation::namespace::{self, PlannedNamespace};
use crate::core::workspace_txn::{PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An unborn disposable Git repository: no checkpoint writer or developer-repository mutation.
fn init_git_fixture(root: &Path) {
    fs::create_dir_all(root.join(".git/objects")).unwrap();
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    fs::write(
        root.join(".git/config"),
        b"[core]\nrepositoryformatversion = 0\nbare = false\n",
    )
    .unwrap();
}

fn speculation_git_fixture(tag: &str) -> Option<(TestDir, WorkspaceIdentity, PathBuf)> {
    if crate::core::gitx::git_exe().is_none() {
        eprintln!("SKIP restart evidence fixture: Git unavailable");
        return None;
    }
    let fixture = TestDir::new();
    let root = fixture.path().join(tag);
    init_git_fixture(&root);
    fs::write(root.join("live.txt"), b"unchanged live bytes").unwrap();
    let identity = WorkspaceIdentity::discover(&root).unwrap();
    assert!(identity.canonical_git_dir.is_some());
    let home = fixture.path().join("private-home");
    Some((fixture, identity, home))
}

/// Fixture producer only: each test retains the real namespace admission and lock domain.
/// Legacy `admit` is a cfg(test) metadata producer, not the removed checkpoint backend.
fn admitted_recovery_namespace(identity: &WorkspaceIdentity, home: &Path, legacy: bool) -> PathBuf {
    let writer = WorkspaceWriterLease::acquire_identity_scoped(
        identity.clone(),
        Duration::ZERO,
        None,
        "namespace fixture",
        "parent",
    )
    .unwrap();
    let physical =
        PhysicalEffectLease::exclusive(Duration::ZERO, None, "namespace fixture", "parent")
            .unwrap();
    if legacy {
        PlannedLegacyNamespace::plan(identity.clone(), home)
            .unwrap()
            .admit(&writer, &physical)
            .unwrap()
            .namespace_dir()
            .to_path_buf()
    } else {
        let directory = home.join("native-workspaces").join(&identity.worktree_id);
        namespace::create_metadata_directory(&directory).unwrap();
        let lock = directory.join("transaction.lock");
        if !lock.exists() {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock)
                .unwrap();
        }
        let opened = PlannedNamespace::plan(identity.clone(), home)
            .unwrap()
            .open_existing()
            .unwrap()
            .unwrap();
        crate::core::speculation::inspection::RecoveryNamespace::verify_admission(
            &opened, &writer, &physical,
        )
        .unwrap();
        opened.namespace_dir().to_path_buf()
    }
}

fn with_legacy_fixture<T>(
    identity: &WorkspaceIdentity,
    home: &Path,
    owner: &str,
    body: impl FnOnce(&LegacyNamespace, &WorkspaceWriterLease, &PhysicalEffectLease) -> T,
) -> T {
    let plan = PlannedLegacyNamespace::plan(identity.clone(), home).unwrap();
    let writer = WorkspaceWriterLease::acquire_identity_scoped(
        identity.clone(),
        Duration::ZERO,
        None,
        "restart evidence fixture",
        owner,
    )
    .unwrap();
    let physical =
        PhysicalEffectLease::exclusive(Duration::ZERO, None, "restart evidence fixture", owner)
            .unwrap();
    let binding = plan.admit(&writer, &physical).unwrap();
    body(&binding, &writer, &physical)
}

fn assert_no_checkpoint_storage(namespace: &Path) {
    assert!(!namespace.join("ledger.json").exists());
    assert!(!namespace.join("journal.json").exists());
    assert!(!namespace
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("store.git")
        .exists());
}

fn bound_recovery_cohort(namespace: &Path, identity: &WorkspaceIdentity) -> PathBuf {
    use crate::core::speculation::evidence::fixtures::{write_base, HistoricalFile};
    let file = HistoricalFile::explicit(
        identity.canonical_root.join("historical-empty-source.txt"),
        None,
        true,
    );
    write_base(namespace, identity, &[file])
}

/// Raw historical layout for corruption/bounds tests; no live authority is manufactured.
fn metadata_namespace(fixture: &TestDir) -> PathBuf {
    let namespace = fixture
        .path()
        .join("repo-0123456789abcdef/worktrees/wt-0123456789abcdef");
    fs::create_dir_all(&namespace).unwrap();
    namespace
}

#[test]
fn cohort_capture_uses_native_storage_for_git_and_non_git_and_retains_prior_vetoes() {
    use crate::core::{
        exec_ctx::{self, ExecutionContext},
        file_placement::PlacementContext,
        workspace_txn::with_test_lock_home,
    };
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (git, prior) in [
        (false, None),
        (true, None),
        (true, Some(true)),
        (true, Some(false)),
    ] {
        let fixture = TestDir::new();
        let root = fixture.path().join("workspace");
        let home = fixture.path().join("home");
        fs::create_dir(&root).unwrap();
        if git {
            assert!(
                crate::core::gitx::git_exe().is_some(),
                "Git required for historical namespace fixture"
            );
            init_git_fixture(&root);
        }
        let identity = WorkspaceIdentity::discover(&root).unwrap();
        assert_eq!(identity.canonical_git_dir.is_some(), git);
        fs::write(identity.canonical_root.join("owned.rs"), b"fn owned() {}\n").unwrap();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
        let parent = ExecutionContext::default()
            .with_resource_scope("parent")
            .with_placement(
                PlacementContext::with_paths(
                    identity.canonical_root.clone(),
                    home.clone(),
                    fixture.path().join("process"),
                    "capture-test",
                )
                .unwrap(),
            );
        with_test_lock_home(fixture.path(), || {
            let evidence = prior.map(|legacy| {
                let namespace = admitted_recovery_namespace(&identity, &home, legacy);
                let directory = bound_recovery_cohort(&namespace, &identity);
                durable_record(&directory, BASE, "integration-prepared", empty_prepared());
                let inventory = recovery_inventory_digest(&directory).unwrap();
                (directory, inventory)
            });
            exec_ctx::with_current(parent.clone(), || {
                if let Some((directory, inventory)) = &evidence {
                    let writer = WorkspaceWriterLease::acquire_identity_scoped(
                        identity.clone(),
                        Duration::ZERO,
                        None,
                        "historical veto fixture",
                        "parent",
                    )
                    .unwrap();
                    let physical = PhysicalEffectLease::exclusive(
                        Duration::ZERO,
                        None,
                        "historical veto fixture",
                        "parent",
                    )
                    .unwrap();
                    let inspection =
                        legacy::require_settled(&identity.canonical_root, &writer, &physical)
                            .and_then(|_| {
                                namespace::inspect_existing(identity.clone(), &home).map(|_| ())
                            });
                    assert!(
                        inspection.is_err(),
                        "retained pending evidence must veto a new cohort"
                    );
                    assert_eq!(&recovery_inventory_digest(directory).unwrap(), inventory);
                    if prior == Some(true) {
                        assert!(!home.join("native-workspaces").exists());
                    }
                } else {
                    // Materialize a historical completed cohort directly, not through retired capture.
                    let namespace = admitted_recovery_namespace(&identity, &home, false);
                    let directory = bound_recovery_cohort(&namespace, &identity);
                    write_verified_chain(&directory);
                    assert!(namespace.join("speculation-v1").join(BASE).is_dir());
                    assert!(!home.join("timemachine").exists());
                    assert!(namespace.join("transaction.lock").is_file());
                    let summary = namespace::inspect_existing(identity.clone(), &home).unwrap();
                    assert_eq!(summary.cohorts, 1);
                }
            });
            assert_eq!(
                fs::read(identity.canonical_root.join("owned.rs")).unwrap(),
                b"fn owned() {}\n"
            );
        });
    }
}

#[test]
fn native_restart_report_and_observation_keep_source_journal_and_legacy_storage_untouched() {
    use crate::core::{
        exec_ctx, native_journal,
        workspace_txn::{with_test_lock_home, NativeResource},
    };
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("native-restart") else {
        return;
    };
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let source = identity.canonical_root.join("source.txt");
    fs::write(&source, b"before").unwrap();
    with_test_lock_home(fixture.path(), || {
        native_journal::with_test_storage(&home, || {
            let id = apply_recovery_fixture_write(&identity.canonical_root, &source, "parent");
            let namespace = admitted_recovery_namespace(&identity, &home, false);
            let directory = bound_recovery_cohort(&namespace, &identity);
            let resource = NativeResource::resolve(&source).unwrap();
            let prepared = durable_recovery_receipt_for_resource(&directory, &resource, id, false);
            let inventory = recovery_inventory_digest(&directory).unwrap();
            let journal_path = home.join("native-journal-v1/records.bin");
            let journal = fs::read(&journal_path).unwrap();
            let prepared_bytes = fs::read(directory.join(&prepared)).unwrap();
            let parent =
                exec_ctx::ExecutionContext::default().with_resource_scope("fresh-inspector");
            exec_ctx::with_current(parent.clone(), || {
                let report = inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &["source.txt".into()],
                )
                .unwrap();
                assert_eq!(report.status, RecoveryReportStatus::InspectionOnly);
                assert_eq!(report.files[0].class, RecoveryImageClass::Post);
                assert_eq!(report.native_records[0].id, id);
                assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
                assert!(inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &[]
                )
                .is_err());
                assert!(inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &["live.txt".into()],
                )
                .is_err());
                let observed = record_speculation_recovery_observation(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &["source.txt".into()],
                )
                .unwrap();
                assert_eq!(observed.status, RecoveryReportStatus::ObservedNonterminal);
                assert!(observed.observation.is_some());
            });
            exec_ctx::with_current(parent.without_workspace_restore(), || {
                assert!(inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &["source.txt".into()],
                )
                .is_err());
            });
            assert!(namespace::inspect_existing(identity.clone(), &home).is_err());
            assert_eq!(fs::read(&source).unwrap(), b"after");
            assert_eq!(fs::read(&journal_path).unwrap(), journal);
            assert_eq!(fs::read(directory.join(&prepared)).unwrap(), prepared_bytes);
            assert!(
                !home.join("timemachine").exists(),
                "native restart must not initialize a legacy registry/store"
            );
        })
    });
}

#[test]
fn duplicate_cohort_refuses_both_routes_even_when_the_other_copy_is_partial() {
    use crate::core::{exec_ctx, workspace_txn::with_test_lock_home};
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for valid_is_legacy in [false, true] {
        let Some((fixture, identity, home)) = speculation_git_fixture("duplicate-cohort") else {
            return;
        };
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
        with_test_lock_home(fixture.path(), || {
            let valid = admitted_recovery_namespace(&identity, &home, valid_is_legacy);
            let other = admitted_recovery_namespace(&identity, &home, !valid_is_legacy);
            let directory = bound_recovery_cohort(&valid, &identity);
            let prepared =
                durable_record(&directory, BASE, "integration-prepared", empty_prepared());
            let duplicate = other.join("speculation-v1").join(BASE);
            fs::create_dir_all(&duplicate).unwrap();
            let inventory = recovery_inventory_digest(&directory).unwrap();
            let registry = fs::read(home.join("timemachine/registry.json")).unwrap();
            let parent = exec_ctx::ExecutionContext::default().with_resource_scope("parent");
            exec_ctx::with_current(parent, || {
                for partial in [false, true] {
                    if partial {
                        fs::write(duplicate.join(&prepared), b"corrupt partial evidence").unwrap();
                    }
                    for observation in [false, true] {
                        let inspect = if observation {
                            record_speculation_recovery_observation
                        } else {
                            inspect_speculation_recovery
                        };
                        let error =
                            inspect(&identity.canonical_root, BASE, &prepared, &[]).unwrap_err();
                        assert!(
                            format!("{error:#}").contains("exactly one matching"),
                            "{error:#}"
                        );
                    }
                }
            });
            assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
            assert_eq!(
                fs::read(duplicate.join(&prepared)).unwrap(),
                b"corrupt partial evidence"
            );
            // Unknown namespace versions are not absence; neither route may choose the other copy.
            let future = other.join("speculation-v999");
            fs::rename(other.join("speculation-v1"), &future).unwrap();
            exec_ctx::with_current(
                exec_ctx::ExecutionContext::default().with_resource_scope("parent"),
                || {
                    for observation in [false, true] {
                        let inspect = if observation {
                            record_speculation_recovery_observation
                        } else {
                            inspect_speculation_recovery
                        };
                        let error =
                            inspect(&identity.canonical_root, BASE, &prepared, &[]).unwrap_err();
                        assert!(
                            format!("{error:#}")
                                .contains("unsupported future speculation namespace"),
                            "{error:#}"
                        );
                    }
                },
            );
            assert_eq!(
                fs::read(future.join(BASE).join(&prepared)).unwrap(),
                b"corrupt partial evidence"
            );
            assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
            assert_eq!(
                fs::read(home.join("timemachine/registry.json")).unwrap(),
                registry
            );
            assert_eq!(
                fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                b"unchanged live bytes"
            );
        });
    }
}

#[test]
fn native_report_does_not_fall_back_past_corrupt_legacy_registry() {
    use crate::core::{exec_ctx, workspace_txn::with_test_lock_home};
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("native-corrupt-registry") else {
        return;
    };
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    with_test_lock_home(fixture.path(), || {
        let namespace = admitted_recovery_namespace(&identity, &home, false);
        let directory = bound_recovery_cohort(&namespace, &identity);
        let prepared = durable_record(&directory, BASE, "integration-prepared", empty_prepared());
        fs::create_dir(home.join("timemachine")).unwrap();
        let registry = home.join("timemachine/registry.json");
        fs::write(&registry, b"unreadable registry").unwrap();
        let inventory = recovery_inventory_digest(&directory).unwrap();
        exec_ctx::with_current(
            exec_ctx::ExecutionContext::default().with_resource_scope("parent"),
            || {
                let error =
                    inspect_speculation_recovery(&identity.canonical_root, BASE, &prepared, &[])
                        .unwrap_err();
                assert!(
                    format!("{error:#}").contains("registry is corrupt"),
                    "{error:#}"
                );
            },
        );
        assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
        assert_eq!(fs::read(&registry).unwrap(), b"unreadable registry");
    });
}

#[test]
fn native_pending_evidence_remains_a_veto_without_a_legacy_restore_journal() {
    use crate::core::workspace_txn::with_test_lock_home;
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("dual-guard") else {
        return;
    };
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    with_test_lock_home(fixture.path(), || {
        let legacy = admitted_recovery_namespace(&identity, &home, true);
        let native = admitted_recovery_namespace(&identity, &home, false);
        let directory = bound_recovery_cohort(&native, &identity);
        durable_record(&directory, BASE, "integration-prepared", empty_prepared());
        let inventory = recovery_inventory_digest(&directory).unwrap();
        assert_no_checkpoint_storage(&legacy);
        // The independent guard refuses even when historical checkpoint storage is empty.
        assert!(namespace::inspect_existing(identity.clone(), &home).is_err());
        assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
        assert_no_checkpoint_storage(&legacy);
        assert_eq!(
            fs::read(identity.canonical_root.join("live.txt")).unwrap(),
            b"unchanged live bytes"
        );
    });
}

#[test]
fn workspace_inspection_retains_completed_evidence_from_both_namespaces() {
    use crate::core::workspace_txn::with_test_lock_home;
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("dual-summary") else {
        return;
    };
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    with_test_lock_home(fixture.path(), || {
        let mut evidence = Vec::new();
        for legacy in [false, true] {
            let namespace = admitted_recovery_namespace(&identity, &home, legacy);
            let directory = bound_recovery_cohort(&namespace, &identity);
            write_verified_chain(&directory);
            let inventory = recovery_inventory_digest(&directory).unwrap();
            evidence.push((namespace, directory, inventory));
        }
        let native = namespace::inspect_existing(identity.clone(), &home).unwrap();
        let plan = PlannedLegacyNamespace::plan(identity.clone(), &home).unwrap();
        let binding = plan.open_existing().unwrap().unwrap();
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::ZERO,
            None,
            "dual summary fixture",
            "parent",
        )
        .unwrap();
        let physical =
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "dual summary fixture", "parent")
                .unwrap();
        let legacy = binding
            .with_transaction_lock(&writer, &physical, || {
                require_settled_speculation(binding.namespace_dir())
            })
            .unwrap();
        assert_eq!(
            (native.cohorts, native.prepared, native.verified),
            (1, 1, 1)
        );
        assert_eq!(
            (legacy.cohorts, legacy.prepared, legacy.verified),
            (1, 1, 1)
        );
        assert_eq!(
            (
                native.cohorts + legacy.cohorts,
                native.prepared + legacy.prepared,
                native.verified + legacy.verified
            ),
            (2, 2, 2)
        );
        for (_, directory, inventory) in evidence {
            assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
        }
        assert_no_checkpoint_storage(binding.namespace_dir());
    });
}

#[test]
fn direct_mode_and_completed_records_survive_read_only_evidence_inspection() {
    let fixture = TestDir::new();
    let namespace = metadata_namespace(&fixture);
    let empty = require_settled_speculation(&namespace).unwrap();
    assert_eq!((empty.cohorts, empty.prepared, empty.verified), (0, 0, 0));
    let directory = cohort(&namespace);
    write_verified_chain(&directory);
    let inventory = recovery_inventory_digest(&directory).unwrap();
    let inspection = require_settled_speculation(&namespace).unwrap();
    assert_eq!(
        (inspection.cohorts, inspection.prepared, inspection.verified),
        (1, 1, 1)
    );
    assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
    assert!(store_has_speculation_evidence(namespace.parent().unwrap().parent().unwrap()).unwrap());
    assert_no_checkpoint_storage(&namespace);
}

#[test]
fn owner_probe_never_waits_and_future_namespace_or_manifest_retains_evidence() {
    let fixture = TestDir::new();
    let namespace = metadata_namespace(&fixture);
    let directory = cohort(&namespace);
    write_verified_chain(&directory);
    let owner = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &directory.join("owner.lock"),
        Duration::ZERO,
    )
    .unwrap();
    let start = std::time::Instant::now();
    assert!(require_settled_speculation(&namespace).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(owner);
    assert!(require_settled_speculation(&namespace).is_ok());
    let future = serde_json::to_vec(
        &serde_json::json!({"format":"aizen-speculation-v2","kind":"base","base":BASE}),
    )
    .unwrap();
    let path = durable_manifest(&directory, &future);
    let inventory = recovery_inventory_digest(&directory).unwrap();
    assert!(require_settled_speculation(&namespace).is_err());
    assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
    fs::remove_file(directory.join(path)).unwrap();
    fs::create_dir(namespace.join("speculation-v2")).unwrap();
    assert!(require_settled_speculation(&namespace).is_err());
    assert!(store_has_speculation_evidence(namespace.parent().unwrap().parent().unwrap()).unwrap());
}

#[test]
fn hardlinked_metadata_and_oversized_cohort_traversal_are_refused() {
    let fixture = TestDir::new();
    let namespace = metadata_namespace(&fixture);
    let directory = cohort(&namespace);
    let marker = fixture.path().join("outside-metadata");
    fs::write(&marker, b"outside untouched").unwrap();
    fs::hard_link(&marker, directory.join("b-alias")).unwrap();
    assert!(require_settled_speculation(&namespace).is_err());
    assert_eq!(fs::read(&marker).unwrap(), b"outside untouched");
    fs::remove_file(directory.join("b-alias")).unwrap();
    let version = directory.parent().unwrap();
    for index in 0..32 {
        fs::create_dir(version.join(format!("{index:032x}"))).unwrap();
    }
    assert!(require_settled_speculation(&namespace).is_err());
}

#[cfg(unix)]
#[test]
fn linked_speculation_namespace_is_refused_not_followed() {
    let fixture = TestDir::new();
    let namespace = metadata_namespace(&fixture);
    let outside = fixture.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, namespace.join("speculation-v1")).unwrap();
    assert!(require_settled_speculation(&namespace).is_err());
    assert!(store_has_speculation_evidence(namespace.parent().unwrap().parent().unwrap()).unwrap());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn bounded_store_scan_preserves_completed_or_unknown_evidence() {
    let fixture = TestDir::new();
    let namespace = metadata_namespace(&fixture);
    let directory = cohort(&namespace);
    write_verified_chain(&directory);
    let inventory = recovery_inventory_digest(&directory).unwrap();
    let root = namespace.parent().unwrap().parent().unwrap();
    require_settled_speculation(&namespace).unwrap();
    assert!(store_has_speculation_evidence(root).unwrap());
    let other = root.join("worktrees/wt-fedcba9876543210");
    fs::create_dir_all(other.join("speculation-v9")).unwrap();
    assert!(require_settled_speculation(&other).is_err());
    assert!(store_has_speculation_evidence(root).unwrap());
    // A preserved historical store remains evidence wherever it was moved; no trash/GC API runs.
    let preserved = fixture.path().join("trash-stamp/repo-fedcba9876543210");
    fs::create_dir_all(preserved.join("worktrees/wt-fedcba9876543210/speculation-v9")).unwrap();
    assert!(store_has_speculation_evidence(&preserved).unwrap());
    assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
    for index in 0..legacy::MAX_WORKTREES {
        fs::create_dir(root.join("worktrees").join(format!("wt-{index:016x}"))).unwrap();
    }
    assert!(store_has_speculation_evidence(root).is_err());
    assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
}

#[test]
fn corrupt_or_missing_registry_cannot_hide_speculation_behind_legacy_fallback() {
    let fixture = TestDir::new();
    let root = fixture.path().join("home/timemachine");
    let evidence = root.join("repo-0123456789abcdef/worktrees/wt-0123456789abcdef/speculation-v1");
    fs::create_dir_all(&evidence).unwrap();
    for bytes in [
        None,
        Some(b"corrupt registry".as_slice()),
        Some(br#"{"schema_version":2,"repos":[]}"#.as_slice()),
    ] {
        if let Some(bytes) = bytes {
            fs::write(root.join("registry.json"), bytes).unwrap();
        }
        assert!(legacy::read_registry(&root).is_err());
        if let Some(bytes) = bytes {
            assert_eq!(fs::read(root.join("registry.json")).unwrap(), bytes);
        } else {
            assert!(!root.join("registry.json").exists());
        }
        assert!(evidence.exists());
    }
}

#[test]
fn recovery_report_requires_parent_and_keeps_pending_guard_and_metadata_unchanged() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("recovery-report") else {
        return;
    };
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        let parent = crate::core::exec_ctx::ExecutionContext::default()
            .with_resource_scope("recovery-parent");
        crate::core::exec_ctx::with_current(parent.clone(), || {
            with_legacy_fixture(
                &identity,
                &home,
                "recovery-parent",
                |binding, writer, physical| {
                    let directory = bound_recovery_cohort(binding.namespace_dir(), &identity);
                    let prepared =
                        durable_record(&directory, BASE, "integration-prepared", empty_prepared());
                    let inventory = recovery_inventory_digest(&directory).unwrap();
                    let selection = binding
                        .select_speculation_recovery(&parent, writer, physical, BASE, &prepared)
                        .unwrap();
                    let report = binding
                        .report_speculation_recovery(&parent, writer, physical, &selection, &[])
                        .unwrap();
                    assert_eq!(report.status, RecoveryReportStatus::InspectionOnly);
                    assert_eq!(report.cohort, BASE);
                    assert_eq!(report.prepared, prepared);
                    assert_eq!(report.inventory_sha256, inventory);
                    assert!(report.files.is_empty());
                    assert!(
                        report.native_records.is_empty(),
                        "no receipt must not invent native ownership"
                    );
                    assert!(
                        require_settled_speculation(binding.namespace_dir()).is_err(),
                        "a report cannot settle prepared evidence"
                    );
                    let delegated = parent.without_workspace_restore();
                    assert!(binding
                        .select_speculation_recovery(&delegated, writer, physical, BASE, &prepared)
                        .is_err());
                    let stale = stale_selection(selection.clone());
                    assert!(binding
                        .report_speculation_recovery(&parent, writer, physical, &stale, &[])
                        .is_err());
                    assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
                    let orphan = b"new immutable evidence after selection";
                    let hash = FileFingerprint::for_bytes(orphan)
                        .sha256
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    let orphan_path = directory.join(format!("b-{hash}"));
                    fs::write(&orphan_path, orphan).unwrap();
                    let changed_inventory = recovery_inventory_digest(&directory).unwrap();
                    assert_ne!(changed_inventory, inventory);
                    let error = binding
                        .report_speculation_recovery(&parent, writer, physical, &selection, &[])
                        .unwrap_err();
                    assert!(error.to_string().contains("selection is stale"));
                    assert_eq!(
                        recovery_inventory_digest(&directory).unwrap(),
                        changed_inventory
                    );
                    assert_eq!(fs::read(&orphan_path).unwrap(), orphan);
                    assert!(require_settled_speculation(binding.namespace_dir()).is_err());
                    assert_eq!(
                        fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                        b"unchanged live bytes"
                    );
                    assert_no_checkpoint_storage(binding.namespace_dir());
                },
            );
        });
    });
}

#[test]
fn recovery_report_surface_requires_explicit_resources_and_plain_parent_without_creating_storage() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = TestDir::new();
    let home = fixture.path().join("absent-home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let root = fixture.path().join("source");
    fs::create_dir(&root).unwrap();
    let prepared = format!("m-{}.json", "a".repeat(64));
    for observation in [false, true] {
        let inspect = if observation {
            record_speculation_recovery_observation
        } else {
            inspect_speculation_recovery
        };
        assert!(
            inspect(&root, BASE, &prepared, &[]).is_err(),
            "restart route cannot fabricate a default parent"
        );
        let parent = ExecutionContext::default().with_resource_scope("report-parent");
        exec_ctx::with_current(parent.without_workspace_restore(), || {
            let error = inspect(&root, BASE, &prepared, &[]).unwrap_err();
            assert!(error.to_string().contains("coordinating-parent"));
        });
        exec_ctx::with_current(parent, || {
            // Typed routes require resource_paths explicitly; malformed JSON/retired tool actions
            // no longer form part of this independent restart contract.
            for (cohort, record, paths) in [
                (BASE, prepared.as_str(), vec!["a.txt".into(); 257]),
                ("../outside", prepared.as_str(), Vec::new()),
                (BASE, "../outside", Vec::new()),
                (BASE, prepared.as_str(), vec!["../outside".into()]),
            ] {
                assert!(inspect(&root, cohort, record, &paths).is_err());
            }
            assert!(inspect(&root, BASE, &prepared, &[])
                .unwrap_err()
                .to_string()
                .contains("no admitted speculative namespace"));
        });
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[test]
fn recovery_report_uses_real_exact_receipt_under_parent_admission_without_settlement() {
    use crate::core::{exec_ctx, native_journal, workspace_txn::NativeResource};
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("recovery-native-report") else {
        return;
    };
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    let source = identity.canonical_root.join("source.txt");
    let unrelated = identity.canonical_root.join("unrelated.txt");
    fs::write(&source, b"before").unwrap();
    fs::write(&unrelated, b"before").unwrap();
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        native_journal::with_test_storage(&home, || {
            apply_recovery_fixture_write(&identity.canonical_root, &unrelated, "other-owner");
            let id = apply_recovery_fixture_write(&identity.canonical_root, &source, "parent");
            assert_eq!(id, 2);
            let resources = [NativeResource::resolve(&source).unwrap()];
            let parent = exec_ctx::ExecutionContext::default().with_resource_scope("parent");
            let (prepared, inventory, directory, journal) =
                exec_ctx::with_current(parent.clone(), || {
                    with_legacy_fixture(&identity, &home, "parent", |binding, writer, physical| {
                        let directory = bound_recovery_cohort(binding.namespace_dir(), &identity);
                        let prepared = durable_recovery_receipt_for_resource(
                            &directory,
                            &resources[0],
                            id,
                            false,
                        );
                        let inventory = recovery_inventory_digest(&directory).unwrap();
                        let journal_path = home.join("native-journal-v1/records.bin");
                        let journal = fs::read(&journal_path).unwrap();
                        let selection = binding
                            .select_speculation_recovery(&parent, writer, physical, BASE, &prepared)
                            .unwrap();
                        let report = binding
                            .report_speculation_recovery(
                                &parent, writer, physical, &selection, &resources,
                            )
                            .unwrap();
                        assert_eq!(report.status, RecoveryReportStatus::InspectionOnly);
                        assert_eq!(
                            report.native_records,
                            vec![native_journal::RecordProof {
                                id,
                                status: native_journal::RecordProofStatus::SettledApplied
                            }]
                        );
                        assert_eq!(report.files.len(), 1);
                        assert_eq!(report.files[0].class, RecoveryImageClass::Post);
                        assert_eq!(
                            report.files[0].current,
                            FileFingerprint::for_bytes(b"after")
                        );
                        assert!(require_settled_speculation(binding.namespace_dir()).is_err());
                        assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
                        assert_eq!(fs::read(&journal_path).unwrap(), journal);
                        assert_eq!(fs::read(&source).unwrap(), b"after");
                        assert_eq!(fs::read(&unrelated).unwrap(), b"after");
                        assert_eq!(
                            fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                            b"unchanged live bytes"
                        );
                        assert_no_checkpoint_storage(binding.namespace_dir());
                        (prepared, inventory, directory, journal)
                    })
                });
            exec_ctx::with_current(parent.clone(), || {
                let report = inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &["source.txt".into()],
                )
                .unwrap();
                let value = report.to_json();
                assert_eq!(value["status"], "inspection_only");
                assert_eq!(value["inspection_only"], true);
                assert_eq!(value["pending_guard_unchanged"], true);
                assert_eq!(value["native_records"][0]["id"], id);
                assert_eq!(value["files"][0]["class"], "post");
                let text = report.to_text();
                assert!(text.contains("pending guard unchanged"));
                assert!(text.contains("historical native record 2: settled_applied"));
                assert!(inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &[]
                )
                .is_err());
            });
            exec_ctx::with_current(parent.without_workspace_restore(), || {
                assert!(inspect_speculation_recovery(
                    &identity.canonical_root,
                    BASE,
                    &prepared,
                    &["source.txt".into()]
                )
                .is_err());
            });
            assert_eq!(fs::read(&source).unwrap(), b"after");
            assert_eq!(
                fs::read(home.join("native-journal-v1/records.bin")).unwrap(),
                journal
            );
            assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
            assert!(
                require_settled_speculation(directory.parent().unwrap().parent().unwrap()).is_err()
            );
        });
    });
}

#[test]
fn recovery_observation_missing_receipt_remains_pending_and_busy_owner_prevents_append() {
    use crate::core::{exec_ctx, repo_lock::RepoTxnLock, workspace_txn::NativeResource};
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("observation-no-receipt") else {
        return;
    };
    let source = identity.canonical_root.join("source.txt");
    fs::write(&source, b"before").unwrap();
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        let parent = exec_ctx::ExecutionContext::default().with_resource_scope("fresh-inspector");
        exec_ctx::with_current(parent.clone(), || {
            with_legacy_fixture(
                &identity,
                &home,
                "fresh-inspector",
                |binding, writer, physical| {
                    let directory = bound_recovery_cohort(binding.namespace_dir(), &identity);
                    let resource = NativeResource::resolve(&source).unwrap();
                    let mut evidence = prepared();
                    change_metadata(&mut evidence, |value| {
                        value["observations"][0]["key"] = serde_json::json!(resource.key());
                        value["observations"][0]["path"] = serde_json::json!(source);
                        value["files"][0]["key"] = serde_json::json!(resource.key());
                        value["files"][0]["path"] = serde_json::json!(source);
                    });
                    let prepared = durable_record_images(
                        &directory,
                        BASE,
                        "integration-prepared",
                        serde_json::from_slice(&evidence.metadata).unwrap(),
                        &evidence.images,
                    );
                    let selection = binding
                        .select_speculation_recovery(&parent, writer, physical, BASE, &prepared)
                        .unwrap();
                    assert_no_native_receipt(&selection);
                    let before = selection_inventory(&selection);
                    let owner = RepoTxnLock::acquire_existing_exclusive(
                        &directory.join("owner.lock"),
                        Duration::ZERO,
                    )
                    .unwrap();
                    let start = std::time::Instant::now();
                    assert!(binding
                        .inspect_selected_recovery(
                            &parent,
                            writer,
                            physical,
                            &selection,
                            std::slice::from_ref(&resource),
                            true
                        )
                        .is_err());
                    assert!(start.elapsed() < Duration::from_secs(2));
                    assert_eq!(recovery_inventory_digest(&directory).unwrap(), before);
                    drop(owner);
                    let report = binding
                        .inspect_selected_recovery(
                            &parent,
                            writer,
                            physical,
                            &selection,
                            &[resource],
                            true,
                        )
                        .unwrap();
                    assert_eq!(report.files[0].class, RecoveryImageClass::Pre);
                    assert!(report.native_records.is_empty());
                    assert_eq!(report.status, RecoveryReportStatus::ObservedNonterminal);
                    // An observation is historical/nonterminal. Read diagnostic evidence without
                    // upgrading its missing worker roster or immutable lineage into settlement.
                    let diagnostic =
                        crate::core::speculation::evidence::ExistingEvidenceReader::open(
                            &directory,
                        )
                        .unwrap()
                        .inspect()
                        .unwrap();
                    assert!(diagnostic.limitations.contains(
                        &crate::core::speculation::evidence::HistoricalLimitation::MissingWorkerRoster,
                    ));
                    assert!(diagnostic.limitations.iter().any(|limitation| matches!(limitation,
                        crate::core::speculation::evidence::HistoricalLimitation::MissingSealedLineage { record }
                            if record == &prepared
                    )));
                    assert_eq!(&report.limitations, &diagnostic.limitations);
                    assert!(
                        crate::core::speculation::evidence::require_supported_history(
                            BASE,
                            &diagnostic.records,
                        )
                        .is_err()
                    );
                    let records = diagnostic.records;
                    let record = records
                        .iter()
                        .find(|record| record.id == report.observation.as_ref().unwrap().record)
                        .unwrap();
                    let observation: crate::core::speculation::recovery::InspectionObservation =
                        serde_json::from_slice(&record.metadata).unwrap();
                    assert!(observation.receipt.is_none() && observation.native_records.is_empty());
                    assert_eq!(observation.owner, "parent");
                    assert_eq!(observation.observer, "fresh-inspector");
                    assert_valid_selection(&directory, BASE, &prepared);
                    assert!(require_settled_speculation(binding.namespace_dir()).is_err());
                    assert_eq!(fs::read(&source).unwrap(), b"before");
                    assert!(!home.join("native-journal-v1").exists());
                    assert_no_checkpoint_storage(binding.namespace_dir());
                },
            );
        });
    });
}

#[test]
fn recovery_observation_appends_under_fresh_parent_without_settling_native_effects() {
    use crate::core::{
        exec_ctx, native_journal,
        repo_lock::LockMode,
        workspace_txn::{NativeResource, NativeResourceLease},
    };
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for possible in [false, true] {
        let Some((fixture, identity, home)) = speculation_git_fixture("recovery-observation")
        else {
            return;
        };
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
        let source = identity.canonical_root.join("source.txt");
        fs::write(&source, b"before").unwrap();
        crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
            native_journal::with_test_storage(&home, || {
                let id = if possible {
                    let resource = NativeResource::resolve(&source).unwrap();
                    let historical =
                        exec_ctx::ExecutionContext::default().with_resource_scope("parent");
                    let error = exec_ctx::with_current(historical, || {
                        let _lease = NativeResourceLease::acquire(
                            &identity.canonical_root,
                            &[(resource.clone(), LockMode::Exclusive)],
                            Duration::ZERO,
                            None,
                            "parent",
                        )
                        .unwrap();
                        native_journal::with_test_after_write_failure(|| {
                            native_journal::apply(
                                &resource,
                                &FileFingerprint::for_bytes(b"before"),
                                b"after",
                                "parent",
                            )
                        })
                        .unwrap_err()
                    });
                    error
                        .downcast_ref::<native_journal::PossiblyApplied>()
                        .unwrap()
                        .record_id
                } else {
                    apply_recovery_fixture_write(&identity.canonical_root, &source, "parent")
                };
                let resources = [NativeResource::resolve(&source).unwrap()];
                let observer = exec_ctx::ExecutionContext::default()
                    .with_resource_scope("new-inspection-parent");
                let (prepared, directory, initial, after_first, journal) = exec_ctx::with_current(
                    observer.clone(),
                    || {
                        with_legacy_fixture(
                            &identity,
                            &home,
                            "new-inspection-parent",
                            |binding, writer, physical| {
                                let directory =
                                    bound_recovery_cohort(binding.namespace_dir(), &identity);
                                let prepared = durable_recovery_receipt_for_resource(
                                    &directory,
                                    &resources[0],
                                    id,
                                    possible,
                                );
                                let selection = binding
                                    .select_speculation_recovery(
                                        &observer, writer, physical, BASE, &prepared,
                                    )
                                    .unwrap();
                                let journal =
                                    fs::read(home.join("native-journal-v1/records.bin")).unwrap();
                                let initial = selection_inventory(&selection);
                                let report = binding
                                    .inspect_selected_recovery(
                                        &observer, writer, physical, &selection, &resources, true,
                                    )
                                    .unwrap();
                                assert_eq!(
                                    report.status,
                                    RecoveryReportStatus::ObservedNonterminal
                                );
                                let publication = report.observation.as_ref().unwrap();
                                assert_eq!(publication.before, initial);
                                assert_ne!(publication.after, initial);
                                assert_eq!(
                                    publication.after,
                                    recovery_inventory_digest(&directory).unwrap()
                                );
                                assert_eq!(report.to_json()["inspection_only"], false);
                                assert_eq!(report.to_json()["pending_guard_unchanged"], true);
                                // An observation is historical/nonterminal. Read diagnostic evidence without
                                // upgrading its missing worker roster or immutable lineage into settlement.
                                let diagnostic = crate::core::speculation::evidence::ExistingEvidenceReader::open(&directory)
                                    .unwrap().inspect().unwrap();
                                assert!(diagnostic.limitations.contains(
                                    &crate::core::speculation::evidence::HistoricalLimitation::MissingWorkerRoster,
                                ));
                                assert!(diagnostic.limitations.iter().any(|limitation| matches!(limitation,
                                    crate::core::speculation::evidence::HistoricalLimitation::MissingSealedLineage { record }
                                        if record == &prepared
                                )));
                                assert_eq!(&report.limitations, &diagnostic.limitations);
                                assert!(
                                    crate::core::speculation::evidence::require_supported_history(
                                        BASE,
                                        &diagnostic.records,
                                    )
                                    .is_err()
                                );
                                let records = diagnostic.records;
                                let observed = records
                                    .iter()
                                    .find(|record| record.id == publication.record)
                                    .unwrap();
                                let data: crate::core::speculation::recovery::InspectionObservation = serde_json::from_slice(&observed.metadata).unwrap();
                                assert_eq!(observed.owner, "new-inspection-parent");
                                assert_eq!(data.owner, "parent");
                                assert_eq!(data.observer, "new-inspection-parent");
                                assert!(data.receipt.is_some());
                                assert_eq!(data.native_records[0].id, id);
                                let status = if possible {
                                    crate::core::speculation::recovery::ObservedNativeStatus::Prepared
                                } else {
                                    crate::core::speculation::recovery::ObservedNativeStatus::SettledApplied
                                };
                                assert_eq!(data.native_records[0].status, status);
                                assert!(binding
                                    .inspect_selected_recovery(
                                        &observer, writer, physical, &selection, &resources, true
                                    )
                                    .is_err());
                                assert!(
                                    require_settled_speculation(binding.namespace_dir()).is_err()
                                );
                                assert_no_checkpoint_storage(binding.namespace_dir());
                                (prepared, directory, initial, publication.after, journal)
                            },
                        )
                    },
                );
                exec_ctx::with_current(observer.clone(), || {
                    let report = record_speculation_recovery_observation(
                        &identity.canonical_root,
                        BASE,
                        &prepared,
                        &["source.txt".into()],
                    )
                    .unwrap();
                    let publication = report.observation.as_ref().unwrap();
                    assert_eq!(publication.before, after_first);
                    assert_ne!(publication.after, initial);
                    assert_ne!(publication.after, after_first);
                    let before_report = publication.after;
                    let inspection = inspect_speculation_recovery(
                        &identity.canonical_root,
                        BASE,
                        &prepared,
                        &["source.txt".into()],
                    )
                    .unwrap();
                    assert!(inspection
                        .to_text()
                        .contains("Inspection only; pending guard unchanged"));
                    assert_eq!(
                        recovery_inventory_digest(&directory).unwrap(),
                        before_report
                    );
                    // An observation is historical/nonterminal. Read diagnostic evidence without
                    // upgrading its missing worker roster or immutable lineage into settlement.
                    let diagnostic =
                        crate::core::speculation::evidence::ExistingEvidenceReader::open(
                            &directory,
                        )
                        .unwrap()
                        .inspect()
                        .unwrap();
                    assert!(diagnostic.limitations.contains(
                        &crate::core::speculation::evidence::HistoricalLimitation::MissingWorkerRoster,
                    ));
                    assert!(diagnostic.limitations.iter().any(|limitation| matches!(limitation,
                        crate::core::speculation::evidence::HistoricalLimitation::MissingSealedLineage { record }
                            if record == &prepared
                    )));
                    assert_eq!(&inspection.limitations, &diagnostic.limitations);
                    assert!(
                        crate::core::speculation::evidence::require_supported_history(
                            BASE,
                            &diagnostic.records,
                        )
                        .is_err()
                    );
                    let records = diagnostic.records;
                    assert_eq!(
                        records
                            .iter()
                            .filter(|record| record.label
                                == crate::core::speculation::recovery::OBSERVATION_LABEL)
                            .count(),
                        2
                    );
                    assert_valid_selection(&directory, BASE, &prepared);
                });
                let final_inventory = recovery_inventory_digest(&directory).unwrap();
                exec_ctx::with_current(observer.without_workspace_restore(), || {
                    assert!(record_speculation_recovery_observation(
                        &identity.canonical_root,
                        BASE,
                        &prepared,
                        &["source.txt".into()]
                    )
                    .is_err());
                });
                assert_eq!(
                    recovery_inventory_digest(&directory).unwrap(),
                    final_inventory
                );
                assert_eq!(fs::read(&source).unwrap(), b"after");
                assert_eq!(
                    fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                    b"unchanged live bytes"
                );
                assert_eq!(
                    fs::read(home.join("native-journal-v1/records.bin")).unwrap(),
                    journal
                );
                assert!(
                    require_settled_speculation(directory.parent().unwrap().parent().unwrap())
                        .is_err()
                );
            });
        });
    }
}

#[test]
fn future_legacy_journal_is_retained_and_never_automatically_recovered() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((fixture, identity, home)) = speculation_git_fixture("future-legacy-journal") else {
        return;
    };
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        with_legacy_fixture(&identity, &home, "parent", |binding, writer, physical| {
            let path = binding.namespace_dir().join("journal.json");
            // Opaque historical bytes, not a production checkpoint Journal or recovery call.
            let bytes = br#"{"schema_version":2,"kind":"clear","generation":0}"#;
            fs::write(&path, bytes).unwrap();
            let registry_path = home.join("timemachine/registry.json");
            let registry = fs::read(&registry_path).unwrap();
            let error =
                legacy::require_settled(&identity.canonical_root, writer, physical).unwrap_err();
            assert!(format!("{error:#}")
                .contains("legacy checkpoint transaction journal is unresolved"));
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read(&registry_path).unwrap(), registry);
            assert!(!binding.namespace_dir().join("ledger.json").exists());
            assert!(!binding
                .namespace_dir()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("store.git")
                .exists());
            assert_eq!(
                fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                b"unchanged live bytes"
            );
        });
    });
}
