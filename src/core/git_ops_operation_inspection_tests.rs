//! Exact selected-receipt reports never settle, rewrite, or replay Git operations.
use super::super::operation_inspection::{inspect_operation, FrozenOperationObservation};
use super::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn settled(f: &Fixture) -> Result<String> {
    let commit = f.commit_file("a.txt", b"selected\n")?;
    Ok(f.revert(&commit)?.id)
}
fn failed(f: &Fixture) -> Result<String> {
    let target = f.commit_file("a.txt", b"selected\n")?;
    f.commit_file("a.txt", b"overlapping\n")?;
    assert!(f.revert(&target).is_err());
    operation_receipts(&f.root)?
        .into_iter()
        .find(|r| r.state == "failed_or_conflicted")
        .map(|r| r.id)
        .context("missing actual failed receipt")
}
fn path(f: &Fixture, id: &str) -> Result<PathBuf> {
    Ok(receipt_directory(&inspect_identity(&f.root, None)?).join(format!("{id}.json")))
}
fn report(f: &Fixture, id: &str) -> Result<Value> {
    Ok(serde_json::to_value(inspect_operation(&f.root, id)?)?)
}
fn image_tree(root: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    fn visit(
        root: &Path,
        path: &Path,
        image: &mut BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<()> {
        let md = fs::symlink_metadata(path)?;
        ensure!(
            !crate::core::file_placement::is_link_or_reparse(&md),
            "fixture image contains a link"
        );
        let key = path.strip_prefix(root)?.to_path_buf();
        if md.is_dir() {
            image.insert(key, None);
            for entry in fs::read_dir(path)? {
                visit(root, &entry?.path(), image)?;
            }
        } else {
            image.insert(key, Some(fs::read(path)?));
        }
        Ok(())
    }
    let mut image = BTreeMap::new();
    if root.exists() {
        visit(root, root, &mut image)?;
    }
    Ok(image)
}
fn preserved(f: &Fixture) -> Result<Vec<BTreeMap<PathBuf, Option<Vec<u8>>>>> {
    let home = crate::core::config::aizen_home();
    [
        &f.root,
        &home.join("git-operations-v1"),
        &home.join("native-journal-v1"),
    ]
    .into_iter()
    .map(|root| image_tree(root))
    .collect()
}

#[test]
fn operation_inspection_reports_exact_fingerprint_identity_and_current_git_without_effects(
) -> Result<()> {
    fixture(|f| {
        let id = settled(f)?;
        let receipt_path = path(f, &id)?;
        let before = preserved(f)?;
        let value = report(f, &id)?;
        assert_eq!(value["status"], "observed");
        assert_eq!(value["selected_receipt"], id);
        assert_eq!(value["receipt"]["record"]["id"], id);
        let fingerprint =
            crate::core::persist::FileFingerprint::for_bytes(&fs::read(receipt_path)?);
        let digest: String = fingerprint
            .sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(value["receipt"]["fingerprint"]["sha256"], digest);
        assert_eq!(
            value["receipt"]["fingerprint"]["byte_len"],
            fingerprint.byte_len
        );
        let identity = inspect_identity(&f.root, None)?;
        assert_eq!(
            value["identity"]["root"],
            serde_json::to_value(&identity.canonical_root)?
        );
        assert_eq!(value["current"]["head"], f.git(&["rev-parse", "HEAD"])?);
        assert_eq!(value["current"]["porcelain_clean"], true);
        assert!(value["current"]["sequencer"].is_null());
        assert_eq!(value["reconciliation_preview_available"], false);
        assert_eq!(preserved(f)?, before);
        Ok(())
    })
}

#[test]
fn operation_inspection_observes_owned_conflict_without_granting_sequencer_or_acknowledgment(
) -> Result<()> {
    fixture(|f| {
        let id = failed(f)?;
        let before = preserved(f)?;
        let value = report(f, &id)?;
        assert_eq!(value["status"], "observed");
        assert_eq!(value["receipt"]["disposition"], "failed_or_conflicted");
        assert_eq!(value["current"]["sequencer"]["action"], "revert");
        assert_eq!(value["current"]["porcelain_clean"], false);
        assert_eq!(value["reconciliation_preview_available"], false);
        assert!(require_no_pending(&inspect_identity(&f.root, None)?).is_err());
        assert_eq!(preserved(f)?, before);
        Ok(())
    })
}

#[test]
fn operation_inspection_only_offers_existing_reconcile_for_manually_resolved_terminal_failure(
) -> Result<()> {
    fixture(|f| {
        let id = failed(f)?;
        f.git(&["revert", "--abort"])?;
        let before = preserved(f)?;
        let value = report(f, &id)?;
        assert_eq!(value["status"], "observed");
        assert_eq!(value["reconciliation_preview_available"], true);
        assert_eq!(value["execution_barriers_fully_checked"], false);
        assert!(value["notice"]
            .as_str()
            .unwrap()
            .contains("not fully checked"));
        assert_eq!(value["receipt"]["record"]["state"], "failed_or_conflicted");
        assert!(value["receipt"]["record"].get("reconciliation").is_none());
        assert!(require_no_pending(&inspect_identity(&f.root, None)?).is_err());
        assert_eq!(preserved(f)?, before);
        let journal = crate::core::config::aizen_home().join("native-journal-v1");
        fs::create_dir(&journal)?;
        fs::write(journal.join("storage.lock"), b"")?;
        fs::write(
            journal.join("records.bin"),
            b"malformed native journal fixture",
        )?;
        let retained = preserved(f)?;
        let blocked_preview = report(f, &id)?;
        assert_eq!(blocked_preview["status"], "observed");
        assert_eq!(blocked_preview["reconciliation_preview_available"], false);
        assert_eq!(blocked_preview["execution_barriers_fully_checked"], false);
        assert!(blocked_preview["reason"]
            .as_str()
            .unwrap()
            .contains("native journal preview evidence refused"));
        assert_eq!(
            preserved(f)?,
            retained,
            "malformed journal must remain byte-identical"
        );
        Ok(())
    })
}

#[test]
fn operation_inspection_clean_git_never_upgrades_prepared_running_or_unknown_records() -> Result<()>
{
    fixture(|f| {
        let id = settled(f)?;
        let receipt_path = path(f, &id)?;
        let original: Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
        for state in ["prepared", "running", "unsettled", "unknown"] {
            let mut value = original.clone();
            value["state"] = state.into();
            fs::write(&receipt_path, serde_json::to_vec(&value)?)?;
            let before = preserved(f)?;
            let inspected = report(f, &id)?;
            assert_eq!(inspected["status"], "observed");
            assert_eq!(inspected["current"]["porcelain_clean"], true);
            assert!(inspected["current"]["sequencer"].is_null());
            assert_eq!(inspected["receipt"]["disposition"], "pending_unknown");
            assert_eq!(inspected["reconciliation_preview_available"], false);
            assert!(require_no_pending(&inspect_identity(&f.root, None)?).is_err());
            assert_eq!(preserved(f)?, before);
        }
        Ok(())
    })
}

#[test]
fn operation_inspection_missing_foreign_corrupt_and_oversized_receipts_fail_closed() -> Result<()> {
    fixture(|f| {
        let id = settled(f)?;
        let receipt_path = path(f, &id)?;
        let bytes = fs::read(&receipt_path)?;
        for bad in ["../escape", "42", "", "missing-operation"] {
            let before = preserved(f)?;
            assert!(inspect_operation(&f.root, bad).is_err());
            assert_eq!(preserved(f)?, before);
        }
        let mut foreign: Value = serde_json::from_slice(&bytes)?;
        foreign["root"] = json!(f.root.join("foreign"));
        for bad in [
            serde_json::to_vec(&foreign)?,
            b"{corrupt".to_vec(),
            vec![b' '; 128 * 1024 + 1],
        ] {
            fs::write(&receipt_path, bad)?;
            let before = preserved(f)?;
            assert!(inspect_operation(&f.root, &id).is_err());
            assert_eq!(preserved(f)?, before);
        }
        Ok(())
    })
}

#[test]
fn operation_inspection_frozen_receipt_refuses_even_whitespace_drift() -> Result<()> {
    fixture(|f| {
        let id = settled(f)?;
        let selected = FrozenOperationObservation::select(inspect_identity(&f.root, None)?, &id)?;
        let receipt_path = path(f, &id)?;
        let mut changed = fs::read(&receipt_path)?;
        changed.push(b'\n');
        fs::write(&receipt_path, changed)?;
        let before = preserved(f)?;
        let error = selected.inspect(&f.root).unwrap_err();
        assert!(format!("{error:#}").contains("receipt changed"));
        assert_eq!(preserved(f)?, before);
        Ok(())
    })
}

#[test]
fn operation_inspection_unknown_marker_is_blocked_not_reported_idle() -> Result<()> {
    fixture(|f| {
        let id = settled(f)?;
        fs::create_dir(f.root.join(".git/rebase-merge"))?;
        let before = preserved(f)?;
        let value = report(f, &id)?;
        assert_eq!(value["status"], "observation_blocked");
        assert!(value["current"].is_null());
        assert_eq!(value["reconciliation_preview_available"], false);
        assert!(value["reason"]
            .as_str()
            .unwrap()
            .contains("unsupported Git operation state"));
        assert_eq!(preserved(f)?, before);
        Ok(())
    })
}

#[test]
fn operation_inspection_actual_runner_unknown_latch_refuses_queries_without_fallback() -> Result<()>
{
    fixture(|f| {
        let id = settled(f)?;
        let selected = FrozenOperationObservation::select(inspect_identity(&f.root, None)?, &id)?;
        let before = preserved(f)?;
        // Exercise the production sticky unknown gate in this disposable lock namespace. No fake
        // release, reset or test-only admission bypass is introduced by the observer.
        crate::core::proctree::latch_bounded_unknown(true);
        let value = report(f, &id)?;
        assert_eq!(value["status"], "observation_blocked");
        assert!(value["identity"].is_null());
        assert!(value["receipt"].is_null());
        assert!(value["current"].is_null());
        assert_eq!(value["reconciliation_preview_available"], false);
        assert!(crate::core::proctree::check_bounded_effects().is_err());
        let selected_report = serde_json::to_value(selected.inspect(&f.root)?)?;
        assert_eq!(selected_report["status"], "observation_blocked");
        assert_eq!(selected_report["receipt"]["record"]["id"], id);
        assert!(selected_report["current"].is_null());
        assert_eq!(selected_report["reconciliation_preview_available"], false);
        assert!(crate::core::proctree::check_bounded_effects().is_err());
        assert_eq!(preserved(f)?, before);
        Ok(())
    })
}

#[test]
fn operation_inspection_dirty_manual_resolution_never_offers_acknowledgment() -> Result<()> {
    fixture(|f| {
        let id = failed(f)?;
        f.git(&["revert", "--abort"])?;
        fs::write(f.root.join("a.txt"), b"manual unresolved edits\n")?;
        f.git(&["add", "--", "a.txt"])?;
        let before = preserved(f)?;
        let value = report(f, &id)?;
        assert_eq!(value["status"], "observed");
        assert_eq!(value["current"]["porcelain_clean"], false);
        assert!(value["current"]["index"]["sha256"].as_str().unwrap().len() == 64);
        assert_eq!(value["reconciliation_preview_available"], false);
        assert!(require_no_pending(&inspect_identity(&f.root, None)?).is_err());
        assert_eq!(preserved(f)?, before);
        Ok(())
    })
}
