//! Synthetic native headers are inspected, never launched. All homes/data below are disposable.
use super::*;
use crate::plugin::{manifest, settings::ExecutionGrant};
use std::io::{Cursor, Write};

fn archive(root: &Path, version: &str) -> (PathBuf, releases::Candidate) {
    let target = manager::host_target().unwrap();
    let mut native = vec![0u8; 256];
    let name = if target.ends_with("windows-msvc") {
        native[..2].copy_from_slice(b"MZ");
        native[60..64].copy_from_slice(&64u32.to_le_bytes());
        native[64..68].copy_from_slice(b"PE\0\0");
        native[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        native[70..72].copy_from_slice(&1u16.to_le_bytes());
        native[84..86].copy_from_slice(&112u16.to_le_bytes());
        native[86..88].copy_from_slice(&2u16.to_le_bytes());
        native[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        native[104..108].copy_from_slice(&1u32.to_le_bytes());
        native[156..158].copy_from_slice(&3u16.to_le_bytes());
        "provider.exe"
    } else if target.contains("linux") {
        native[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        native[16..18].copy_from_slice(&2u16.to_le_bytes());
        native[18..20].copy_from_slice(&62u16.to_le_bytes());
        native[24..32].copy_from_slice(&1u64.to_le_bytes());
        "provider"
    } else {
        native[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
        native[4..8].copy_from_slice(&0x0100000cu32.to_le_bytes());
        native[12..16].copy_from_slice(&2u32.to_le_bytes());
        "provider"
    };
    let json = serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"id":"mnemos","name":"Synthetic nonexecuting update","version":version,"kind":"memory",
        "protocol":{"major":1,"min_minor":0,"max_minor":0},"target":target,"entrypoint":name,
        "capabilities":[],"permissions":{"network":false,"host_llm":false},
        "files":[{"path":name,"bytes":native.len(),"sha256":manifest::sha256(&native)}]
    })).unwrap();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes, mode) in [
        ("plugin.json", json.as_slice(), 0o600),
        (name, native.as_slice(), 0o700),
    ] {
        zip.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(mode),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    let path = root.join(format!("fixture-{version}.zip"));
    fs::write(&path, &bytes).unwrap();
    (
        path,
        releases::Candidate {
            release_id: 1,
            version: version.into(),
            target: target.into(),
            asset_id: 2,
            filename: format!("mnemos-{version}-{target}.zip"),
            bytes: bytes.len() as u64,
            sha256: manifest::sha256(&bytes),
            checksums_asset_id: 3,
            checksums_sha256: "a".repeat(64),
        },
    )
}
fn setup(root: &Path) -> MemoryPlugins {
    let (source, candidate) = archive(root, "1.0.0");
    manager::install_release(&source, &candidate, 0).unwrap();
    crate::core::cli_config::edit_memory_plugins(|state| {
        let grant = ExecutionGrant {
            manifest_sha256: state.packages["mnemos"].pins.manifest.sha256.clone(),
            guarded_execution: true,
            network: false,
            learning: true,
            turn_model_learning: true,
        };
        state.activate("mnemos", grant)?;
        state.set_storage("mnemos", Some(root.join("data")))?;
        Ok(())
    })
    .unwrap();
    fs::create_dir(root.join("data")).unwrap();
    fs::write(root.join("data/keep"), b"synthetic data sentinel").unwrap();
    manager::read_state().unwrap()
}
fn set_fault(point: Option<&'static str>) {
    FAULT.with(|f| *f.borrow_mut() = point);
}

#[test]
fn each_update_boundary_blocks_then_explicitly_recovers_without_execution_or_data_changes() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for boundary in [
        "prepared",
        "old_renamed",
        "before_backed_up",
        "backed_up",
        "new_renamed",
        "before_published",
        "published",
        "before_config_write",
        "config_written",
        "before_committed",
        "committed",
        "before_retire",
    ] {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let home = dir.path().join("home");
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", home.as_path()),
            ("HOME", dir.path()),
            ("USERPROFILE", dir.path()),
        ]);
        let old = setup(dir.path());
        let (source, candidate) = archive(dir.path(), "1.1.0");
        set_fault(Some(boundary));
        assert!(
            update_release(
                &source,
                &candidate,
                old.generation,
                &old.packages["mnemos"].pins
            )
            .is_err(),
            "{boundary}"
        );
        set_fault(None);
        assert!(recovery_required().unwrap());
        assert!(manager::Admission::acquire().is_err());
        let inspection = inspect_recovery().unwrap();
        assert_ne!(inspection.state, "blocked", "{boundary}: {inspection:?}");
        let id = inspection.operation_id.unwrap();
        let before = fs::read(marker_path()).unwrap();
        assert!(recover("00000000000000000000000000000000").is_err());
        assert_eq!(fs::read(marker_path()).unwrap(), before);
        recover(&id).unwrap();
        assert!(!recovery_required().unwrap());
        let after = manager::read_state().unwrap();
        assert_eq!(after.selected, old.selected);
        assert_eq!(
            after.packages["mnemos"].data_root,
            old.packages["mnemos"].data_root
        );
        assert_eq!(
            after.packages["mnemos"].settings,
            old.packages["mnemos"].settings
        );
        assert_eq!(
            after.packages["mnemos"].budgets,
            old.packages["mnemos"].budgets
        );
        if boundary == "prepared" {
            assert_eq!(after, old);
        } else {
            assert_eq!(after.generation, old.generation + 1);
            assert!(!after.enabled && !after.packages["mnemos"].enabled);
            assert!(after.packages["mnemos"].grant.is_none());
            assert!(manager::selected_at(old.generation).is_err());
        }
        assert_eq!(
            fs::read(dir.path().join("data/keep")).unwrap(),
            b"synthetic data sentinel"
        );
        assert!(manager::info("mnemos").is_ok());
    }
}

#[test]
fn recovery_itself_can_crash_after_old_restore_without_restoring_execution_authority() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for boundary in ["old_renamed", "new_renamed"] {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let home = dir.path().join("home");
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_path())]);
        let old = setup(dir.path());
        let (source, candidate) = archive(dir.path(), "1.1.0");
        set_fault(Some(boundary));
        assert!(update_release(
            &source,
            &candidate,
            old.generation,
            &old.packages["mnemos"].pins
        )
        .is_err());
        set_fault(None);
        let id = inspect_recovery().unwrap().operation_id.unwrap();
        set_fault(Some("old_restored"));
        assert!(recover(&id).is_err());
        set_fault(None);
        assert_ne!(inspect_recovery().unwrap().state, "blocked");
        recover(&id).unwrap();
        let after = manager::read_state().unwrap();
        assert_eq!(after.generation, old.generation + 1);
        assert!(after.packages["mnemos"].grant.is_none() && !after.enabled);
    }
}

#[test]
fn stale_review_digest_target_not_newer_busy_and_tampering_refuse_without_replacement() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let home = dir.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let old = setup(dir.path());
    let pins = &old.packages["mnemos"].pins;
    let (source, candidate) = archive(dir.path(), "1.1.0");
    assert!(update_release(&source, &candidate, old.generation + 1, pins).is_err());
    let mut bad = pins.clone();
    bad.manifest.sha256 = "f".repeat(64);
    assert!(update_release(&source, &candidate, old.generation, &bad).is_err());
    let mut bad = candidate.clone();
    bad.sha256 = "f".repeat(64);
    assert!(update_release(&source, &bad, old.generation, pins).is_err());
    let mut bad = candidate.clone();
    bad.target = "aarch64-apple-darwin".into();
    if bad.target == candidate.target {
        bad.target = "x86_64-pc-windows-msvc".into();
    }
    assert!(update_release(&source, &bad, old.generation, pins).is_err());
    let (same, same_candidate) = archive(dir.path(), "1.0.0");
    assert!(update_release(&same, &same_candidate, old.generation, pins).is_err());
    let (older, older_candidate) = archive(dir.path(), "0.9.0");
    assert!(update_release(&older, &older_candidate, old.generation, pins).is_err());
    let busy = manager::Admission::acquire().unwrap();
    assert!(update_release(&source, &candidate, old.generation, pins).is_err());
    drop(busy);
    assert_eq!(manager::read_state().unwrap(), old);
    assert!(!recovery_required().unwrap());
    let live = manager::info("mnemos").unwrap();
    fs::write(live.entrypoint, b"tampered synthetic bytes").unwrap();
    assert!(update_release(&source, &candidate, old.generation, pins).is_err());
    assert_eq!(manager::read_state().unwrap(), old);
    assert!(!recovery_required().unwrap());
}

#[test]
fn successful_update_keeps_old_payload_receipt_and_requires_fresh_activation() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let home = dir.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let old = setup(dir.path());
    let (source, candidate) = archive(dir.path(), "1.1.0");
    let updated = update_release(
        &source,
        &candidate,
        old.generation,
        &old.packages["mnemos"].pins,
    )
    .unwrap();
    assert_eq!(updated.manifest.version, "1.1.0");
    let now = manager::read_state().unwrap();
    assert_eq!(now.generation, old.generation + 1);
    assert!(now.packages["mnemos"].grant.is_none() && !now.enabled);
    assert_eq!(
        fs::read_dir(manager::packages_root())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".mnemos-old-"))
            .count(),
        1
    );
    assert_eq!(
        fs::read_dir(marker_path().parent().unwrap())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("mnemos-update-"))
            .count(),
        1
    );
    assert!(!recovery_required().unwrap());
}

#[test]
fn physical_marker_invalid_journal_and_unknown_evidence_remain_blocked_and_unchanged() {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", dir.path())]);
    install::ensure_private_root(marker_path().parent().unwrap()).unwrap();
    fs::write(marker_path(), b"not a journal").unwrap();
    assert_eq!(inspect_recovery().unwrap().state, "blocked");
    assert!(recover("anything").is_err());
    assert_eq!(fs::read(marker_path()).unwrap(), b"not a journal");
    fs::write(manager::transaction_marker(), b"physical unsettled").unwrap();
    assert!(manager::Admission::acquire_update_recovery().is_err());
    assert_eq!(
        fs::read(manager::transaction_marker()).unwrap(),
        b"physical unsettled"
    );
}

#[test]
fn incompatible_retained_setting_and_changed_recovery_evidence_are_never_discarded() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let home = dir.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    setup(dir.path());
    crate::core::cli_config::edit_memory_plugins(|state| {
        state.configure(
            "mnemos",
            std::collections::BTreeMap::from([("removed_setting".into(), serde_json::json!(true))]),
            Some(Default::default()),
        )
    })
    .unwrap();
    let old = manager::read_state().unwrap();
    let (source, candidate) = archive(dir.path(), "1.1.0");
    assert!(update_release(
        &source,
        &candidate,
        old.generation,
        &old.packages["mnemos"].pins
    )
    .is_err());
    assert_eq!(manager::read_state().unwrap(), old);
    assert!(!recovery_required().unwrap());
    assert_eq!(
        fs::read_dir(manager::packages_root()).unwrap().count(),
        1,
        "unprepared owned stage is cleaned"
    );
    crate::core::cli_config::edit_memory_plugins(|state| {
        state.configure("mnemos", Default::default(), Some(Default::default()))
    })
    .unwrap();
    let old = manager::read_state().unwrap();
    set_fault(Some("new_renamed"));
    assert!(update_release(
        &source,
        &candidate,
        old.generation,
        &old.packages["mnemos"].pins
    )
    .is_err());
    set_fault(None);
    let j = load().unwrap().unwrap();
    let unknown = manager::packages_root().join(&j.backup).join("unknown");
    fs::write(&unknown, b"foreign evidence").unwrap();
    let marker = fs::read(marker_path()).unwrap();
    assert_eq!(inspect_recovery().unwrap().state, "blocked");
    assert!(recover(&j.operation_id).is_err());
    assert_eq!(fs::read(marker_path()).unwrap(), marker);
    assert_eq!(fs::read(unknown).unwrap(), b"foreign evidence");
    assert_eq!(manager::read_state().unwrap(), old);
}

#[test]
fn cancellation_before_prepared_cleans_owned_stage_but_handoff_retains_it() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let home = dir.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    setup(dir.path());
    let (source, candidate) = archive(dir.path(), "1.1.0");
    let expectation = install::ReleaseExpectation {
        id: "mnemos",
        version: &candidate.version,
        archive_sha256: &candidate.sha256,
    };
    let stage = install::stage_release(
        &manager::packages_root(),
        &source,
        &candidate.target,
        &expectation,
        candidate.bytes,
    )
    .unwrap();
    let path = stage.package.directory.clone();
    drop(stage);
    assert!(!path.exists());
    let mut stage = install::stage_release(
        &manager::packages_root(),
        &source,
        &candidate.target,
        &expectation,
        candidate.bytes,
    )
    .unwrap();
    let path = stage.package.directory.clone();
    stage.retain_for_recovery();
    drop(stage);
    assert!(path.exists());
}

#[test]
fn pure_state_update_preserves_other_provider_and_rejects_full_old_state_drift() {
    let pins = install::InstallPins {
        schema_version: 1,
        id: "mnemos".into(),
        target: "x86_64-pc-windows-msvc".into(),
        manifest: install::FilePin {
            path: "plugin.json".into(),
            bytes: 1,
            sha256: "a".repeat(64),
            unix_mode: None,
        },
        files: vec![],
    };
    let mut state = MemoryPlugins::default();
    state.record_install(pins.clone()).unwrap();
    let mut other = pins.clone();
    other.id = "other".into();
    state.record_install(other).unwrap();
    let grant = ExecutionGrant {
        manifest_sha256: "a".repeat(64),
        guarded_execution: true,
        network: false,
        learning: false,
        turn_model_learning: false,
    };
    state.activate("other", grant).unwrap();
    let before = state.clone();
    let saved = state.packages["mnemos"].clone();
    let mut next = pins;
    next.manifest.sha256 = "b".repeat(64);
    let mut wrong = saved.clone();
    wrong.budgets = Some(Default::default());
    assert!(state
        .replace_package(before.generation, &wrong, next.clone())
        .is_err());
    assert_eq!(state, before);
    state
        .replace_package(before.generation, &saved, next)
        .unwrap();
    assert!(state.enabled);
    assert_eq!(state.selected, before.selected);
    assert_eq!(state.packages["other"], before.packages["other"]);
    assert_eq!(state.generation, before.generation + 1);
}
