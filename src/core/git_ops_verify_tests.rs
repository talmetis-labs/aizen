//! Destination verification linked to one real settled task integration in disposable repos.
use super::*;
use crate::core::git_ops::{
    operation_receipts, verification_records, Change as GitChange, ChangeReceipt, PreparedChange,
    PreparedVerification, VerificationRecord,
};

/// An integrated handoff in a disposable repository whose destination is a Cargo-shaped project.
/// Each call writes distinct bytes so a repeated integration still produces one real commit.
fn integrated(f: &Fixture) -> Result<ChangeReceipt> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let payload = format!(
        "integrated result {}\n",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let task = f.task()?;
    let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
    let attempt = f.begin(&task, &scope)?;
    f.write(&attempt, "tracked.txt", payload.as_bytes())?;
    let handoff = f.finish(task, &scope, attempt)?;
    let receipt = PreparedChange::prepare(
        &f.root,
        GitChange::IntegrateTask {
            receipt: handoff.registration.registration_id.clone(),
        },
    )?
    .execute()?;
    assert_eq!(receipt.state, "settled_unverified");
    Ok(receipt)
}

/// A committed project shape at the destination so a real verification command can run.
/// Trust is recorded for the fixture root explicitly; the ambient checkout's trust store is not used.
fn add_manifest(f: &Fixture, command: &str) -> Result<()> {
    std::fs::write(f.root.join("Cargo.toml"), "[package]\n")?;
    std::fs::create_dir_all(f.root.join(".aizen"))?;
    std::fs::write(
        f.root.join(".aizen").join("verify.json"),
        serde_json::json!({"commands": [command], "timeout_secs": 60}).to_string(),
    )?;
    f.git(&["add", "--", "Cargo.toml", ".aizen/verify.json"])?;
    f.git(&["commit", "-qm", "manifest"])?;
    let root = f.root.canonicalize()?;
    let placement =
        crate::core::file_placement::PlacementContext::new(root.clone(), f.home.clone())?;
    crate::core::exec_ctx::with_current(
        crate::core::exec_ctx::ExecutionContext::default().with_placement(placement),
        crate::agent::mcp::trust_project,
    )?;
    ensure!(
        crate::agent::mcp::project_trusted_at(&root),
        "fixture could not trust {}",
        root.display()
    );
    Ok(())
}

fn record_path(f: &Fixture, record: &VerificationRecord) -> Result<PathBuf> {
    let identity = inspect_identity(&f.root, None)?;
    Ok(f.home
        .join("verification-v1")
        .join(identity.repo_id)
        .join(identity.worktree_id)
        .join(format!("{}.json", record.id)))
}

#[cfg(windows)]
#[test]
fn task_integration_and_verification_accept_ordinary_windows_home_paths() -> Result<()> {
    fixture(|f| {
        // TestDir is canonical/verbatim. Real CLI env overrides usually use ordinary C:\ paths.
        let home = f.home.to_string_lossy();
        let ordinary = PathBuf::from(
            home.strip_prefix(r"\\?\")
                .context("verbatim fixture home")?,
        );
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &ordinary)]);
        assert_ne!(ordinary, ordinary.canonicalize()?);
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        assert_eq!(operation_receipts(&f.root)?.len(), 1);
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        assert!(record.verified);
        assert_eq!(verification_records(&f.root)?.len(), 1);
        assert!(operation_receipts(&f.root)?[0].verified);
        Ok(())
    })
}

#[test]
fn verify_task_runs_frozen_commands_and_marks_only_the_integration_verified() -> Result<()> {
    fixture(|f| {
        let integration = integrated(f)?;
        add_manifest(f, "exit 0")?;
        // The manifest commit moved HEAD, so the frozen integration no longer describes the tip.
        let error = PreparedVerification::prepare(&f.root, &integration.id).unwrap_err();
        assert!(
            error.to_string().contains("HEAD moved"),
            "a moved destination must refuse: {error:#}"
        );
        // Re-integrate from the new base so the receipt matches the current destination exactly.
        let integration = integrated(f)?;
        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        let preview: serde_json::Value = serde_json::from_str(&prepared.preview()?)?;
        assert_eq!(preview["integration_receipt"], integration.id);
        assert_eq!(preview["commands"], serde_json::json!(["exit 0"]));
        assert_eq!(
            preview["destination"].as_str(),
            integration.after.as_ref().map(|oid| oid.as_str())
        );
        assert!(verification_records(&f.root)?.is_empty());
        let record = prepared.execute()?;
        assert!(record.verified, "a settled pass marks the integration");
        assert_eq!(record.state, "settled");
        assert_eq!(record.integration, integration.id);
        assert_eq!(record.commands, vec!["exit 0".to_string()]);
        assert_eq!(record.passed_commands, vec!["exit 0".to_string()]);
        assert!(record.source_identity.is_some());
        let stored = verification_records(&f.root)?;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, record.id);
        let on_disk = record_path(f, &record)?;
        assert!(
            on_disk.is_file(),
            "record persisted at {}",
            on_disk.display()
        );
        let persisted: VerificationRecord = serde_json::from_slice(&std::fs::read(&on_disk)?)?;
        assert!(persisted.verified && persisted.finished_at.is_some());
        let receipts = operation_receipts(&f.root)?;
        let verified = receipts
            .iter()
            .find(|receipt| receipt.id == integration.id)
            .unwrap();
        assert!(verified.verified);
        assert_eq!(verified.verification.as_deref(), Some(record.id.as_str()));
        // A second verification of an already proven destination is refused, not re-run.
        let error = PreparedVerification::prepare(&f.root, &integration.id).unwrap_err();
        assert!(error.to_string().contains("already verified"), "{error:#}");
        assert_eq!(head(&f.root, None)?, integration.after.unwrap());
        Ok(())
    })
}

#[test]
fn verify_task_relinks_a_durable_pass_without_running_another_command() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let identity = inspect_identity(&f.root, None)?;
        let integration_path = f
            .home
            .join("git-operations-v1")
            .join(&identity.repo_id)
            .join(&identity.worktree_id)
            .join(format!("{}.json", integration.id));
        let unlinked = std::fs::read(&integration_path)?;
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        assert!(record.verified);
        let path = record_path(f, &record)?;
        let record_bytes = std::fs::read(&path)?;
        // Reproduce a crash between durable pass publication and integration-link publication.
        // Only this disposable fixture's receipt is restored, not source or Git state.
        crate::core::persist::atomic_write_owner_only_checked(&integration_path, &unlinked)?;
        let index = std::fs::read(f.root.join(".git/index"))?;
        let before = head(&f.root, None)?;
        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        let preview: serde_json::Value = serde_json::from_str(&prepared.preview()?)?;
        assert_eq!(preview["mode"], "relink_durable_pass");
        assert_eq!(preview["verification_record"], record.id);
        assert!(
            !operation_receipts(&f.root)?[0].verified,
            "preview published a link"
        );
        // A current-thread runtime cannot execute the synchronous command bridge. Relinking must
        // work here because it reuses settled evidence instead of creating a second attempt.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let recovered = runtime.block_on(async { prepared.execute() })?;
        assert_eq!(recovered.id, record.id);
        assert!(recovered.verified);
        assert_eq!(std::fs::read(&path)?, record_bytes);
        assert_eq!(verification_records(&f.root)?.len(), 1);
        assert_eq!(head(&f.root, None)?, before);
        assert_eq!(std::fs::read(f.root.join(".git/index"))?, index);
        let stored = operation_receipts(&f.root)?;
        assert!(stored[0].verified);
        assert_eq!(stored[0].verification.as_deref(), Some(record.id.as_str()));
        let handoff: crate::core::git_ops::TaskHandoff =
            serde_json::from_value(serde_json::to_value(&integration)?["handoff"].clone())?;
        assert_eq!(
            crate::core::git_ops::accepted_task_integration(&f.root, &handoff)?
                .context("relinked result was not accepted")?
                .verification,
            record.id
        );
        crate::core::git_ops::require_verified_destination(&f.root, &before)?;
        Ok(())
    })
}

#[test]
fn verification_relink_refuses_drift_tampering_duplicates_and_unknown_attempts() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let identity = inspect_identity(&f.root, None)?;
        let integration_path = f
            .home
            .join("git-operations-v1")
            .join(&identity.repo_id)
            .join(&identity.worktree_id)
            .join(format!("{}.json", integration.id));
        let unlinked = std::fs::read(&integration_path)?;
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        let path = record_path(f, &record)?;
        let original = std::fs::read(&path)?;
        crate::core::persist::atomic_write_owner_only_checked(&integration_path, &unlinked)?;

        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        let mut tampered: serde_json::Value = serde_json::from_slice(&original)?;
        tampered["output"] = serde_json::json!("changed after approval");
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&tampered)?,
        )?;
        assert!(format!("{:#}", prepared.execute().unwrap_err()).contains("pass changed"));
        crate::core::persist::atomic_write_owner_only_checked(&path, &original)?;

        // Before approval, a pass on different inputs cannot be borrowed even if it is internally valid.
        let mut changed: serde_json::Value = serde_json::from_slice(&original)?;
        changed["source_identity"] = serde_json::json!("f".repeat(64));
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&changed)?,
        )?;
        assert!(format!(
            "{:#}",
            PreparedVerification::prepare(&f.root, &integration.id).unwrap_err()
        )
        .contains("no longer match"));
        changed["source_identity"] =
            serde_json::from_slice::<serde_json::Value>(&original)?["source_identity"].clone();
        changed["commands"] = serde_json::json!(["exit 1"]);
        changed["passed_commands"] = serde_json::json!(["exit 1"]);
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&changed)?,
        )?;
        assert!(PreparedVerification::prepare(&f.root, &integration.id).is_err());
        crate::core::persist::atomic_write_owner_only_checked(&path, &original)?;

        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        std::fs::write(f.root.join("untracked-source.txt"), b"new local input\n")?;
        assert!(
            prepared.execute().is_err(),
            "untracked source drift reused the old pass"
        );
        std::fs::remove_file(f.root.join("untracked-source.txt"))?;
        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        let manifest = f.root.join(".aizen/verify.json");
        let manifest_bytes = std::fs::read(&manifest)?;
        std::fs::write(&manifest, r#"{"commands":["exit 1"]}"#)?;
        assert!(
            prepared.execute().is_err(),
            "changed commands reused the old pass"
        );
        std::fs::write(&manifest, manifest_bytes)?;

        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        let placement = crate::core::file_placement::PlacementContext::new(
            f.root.canonicalize()?,
            f.home.clone(),
        )?;
        crate::core::exec_ctx::with_current(
            crate::core::exec_ctx::current()
                .unwrap()
                .with_placement(placement.clone()),
            crate::agent::mcp::untrust_project,
        )?;
        assert!(
            prepared.execute().is_err(),
            "revoked trust reused the old pass"
        );
        crate::core::exec_ctx::with_current(
            crate::core::exec_ctx::current()
                .unwrap()
                .with_placement(placement),
            crate::agent::mcp::trust_project,
        )?;

        let extra = path.parent().unwrap().join("verify-extra.json");
        let mut duplicate: serde_json::Value = serde_json::from_slice(&original)?;
        duplicate["id"] = serde_json::json!("verify-extra");
        crate::core::persist::atomic_write_owner_only_checked(
            &extra,
            &serde_json::to_vec(&duplicate)?,
        )?;
        assert!(format!(
            "{:#}",
            PreparedVerification::prepare(&f.root, &integration.id).unwrap_err()
        )
        .contains("multiple durable passes"));
        duplicate["state"] = serde_json::json!("running");
        duplicate["termination"] = serde_json::json!("running");
        duplicate["verified"] = serde_json::json!(false);
        duplicate["passed_commands"] = serde_json::json!([]);
        duplicate["source_identity"] = serde_json::Value::Null;
        duplicate["finished_at"] = serde_json::Value::Null;
        duplicate["duration_ms"] = serde_json::json!(0);
        crate::core::persist::atomic_write_owner_only_checked(
            &extra,
            &serde_json::to_vec(&duplicate)?,
        )?;
        assert!(format!(
            "{:#}",
            PreparedVerification::prepare(&f.root, &integration.id).unwrap_err()
        )
        .contains("unfinished or unknown"));
        duplicate["state"] = serde_json::json!("unsettled");
        duplicate["termination"] = serde_json::json!("unsettled");
        duplicate["finished_at"] = serde_json::json!(record.finished_at);
        crate::core::persist::atomic_write_owner_only_checked(
            &extra,
            &serde_json::to_vec(&duplicate)?,
        )?;
        assert!(PreparedVerification::prepare(&f.root, &integration.id).is_err());
        std::fs::remove_file(extra)?;

        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        f.git(&["commit", "--allow-empty", "-qm", "moved destination"])?;
        assert!(prepared.execute().is_err());
        assert!(!operation_receipts(&f.root)?[0].verified);
        assert_eq!(verification_records(&f.root)?.len(), 1);
        assert_eq!(std::fs::read(&path)?, original);
        Ok(())
    })
}

#[test]
fn verify_task_records_a_failing_check_without_claiming_success() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 7")?;
        let integration = integrated(f)?;
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        assert!(!record.verified, "a failing command never verifies");
        assert_eq!(record.state, "settled");
        assert!(record.passed_commands.is_empty());
        let receipts = operation_receipts(&f.root)?;
        let stored = receipts
            .iter()
            .find(|receipt| receipt.id == integration.id)
            .unwrap();
        assert!(
            !stored.verified && stored.verification.is_none(),
            "the integration stays unverified after a failed check"
        );
        // A settled failure is repeatable: the operator may fix the project and try again.
        assert!(PreparedVerification::prepare(&f.root, &integration.id).is_ok());
        Ok(())
    })
}

#[test]
fn verify_task_refuses_a_moved_destination_and_unsettled_operations() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        f.git(&["commit", "--allow-empty", "-qm", "destination drift"])?;
        let error = prepared.execute().unwrap_err();
        assert!(
            format!("{error:#}").contains("HEAD moved"),
            "approval must not survive destination drift: {error:#}"
        );
        assert!(verification_records(&f.root)?.is_empty());
        Ok(())
    })
}

#[test]
fn verify_task_refuses_pending_tracked_edits_at_the_destination() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        std::fs::write(f.root.join("tracked.txt"), b"pending user edit\n")?;
        let error = PreparedVerification::prepare(&f.root, &integration.id).unwrap_err();
        assert!(
            error.to_string().contains("pending tracked edits"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(f.root.join("tracked.txt"))?,
            b"pending user edit\n"
        );
        assert!(verification_records(&f.root)?.is_empty());
        Ok(())
    })
}

#[test]
fn verification_without_a_supported_command_refuses_instead_of_reporting_success() -> Result<()> {
    fixture(|f| {
        // No manifest of any kind: detection yields nothing to run.
        let integration = integrated(f)?;
        let error = PreparedVerification::prepare(&f.root, &integration.id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("1-8 supported verification commands"),
            "{error:#}"
        );
        assert!(verification_records(&f.root)?.is_empty());
        Ok(())
    })
}

#[test]
fn verification_selection_requires_a_settled_integration_receipt() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let identity = inspect_identity(&f.root, None)?;
        let path = f
            .home
            .join("git-operations-v1")
            .join(&identity.repo_id)
            .join(&identity.worktree_id)
            .join(format!("{}.json", integration.id));
        let original = std::fs::read(&path)?;
        // A record that claims verification without the linked record is not a valid receipt.
        let mut claimed: serde_json::Value = serde_json::from_slice(&original)?;
        claimed["verified"] = serde_json::json!(true);
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&claimed)?,
        )?;
        assert!(PreparedVerification::prepare(&f.root, &integration.id).is_err());
        // Nor can a non-integration action authorize destination verification.
        let mut other: serde_json::Value = serde_json::from_slice(&original)?;
        other["change"] = serde_json::json!({"action": "integrate", "commit": "HEAD"});
        other["handoff"] = serde_json::Value::Null;
        crate::core::persist::atomic_write_owner_only_checked(&path, &serde_json::to_vec(&other)?)?;
        assert!(PreparedVerification::prepare(&f.root, &integration.id).is_err());
        // The exact, unchanged integration receipt is selectable again.
        crate::core::persist::atomic_write_owner_only_checked(&path, &original)?;
        PreparedVerification::prepare(&f.root, &integration.id)?;
        Ok(())
    })
}

#[test]
fn workflow_acceptance_requires_exact_verified_handoff_and_current_source() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let handoff: crate::core::git_ops::TaskHandoff =
            serde_json::from_value(serde_json::to_value(&integration)?["handoff"].clone())?;
        assert!(crate::core::git_ops::accepted_task_integration(&f.root, &handoff)?.is_none());
        assert!(crate::core::git_ops::require_verified_destination(&f.root, &handoff.tip).is_err());
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        let accepted = crate::core::git_ops::accepted_task_integration(&f.root, &handoff)?.unwrap();
        assert_eq!(accepted.integration, integration.id);
        assert_eq!(accepted.verification, record.id);
        crate::core::git_ops::require_verified_destination(&f.root, &accepted.destination)?;
        let mut foreign = handoff.clone();
        foreign.paths = vec!["other.txt".into()];
        assert!(crate::core::git_ops::accepted_task_integration(&f.root, &foreign).is_err());
        std::fs::write(f.root.join("tracked.txt"), b"unverified source\n")?;
        assert!(
            crate::core::git_ops::require_verified_destination(&f.root, &accepted.destination)
                .is_err()
        );
        f.git(&["add", "--", "tracked.txt"])?;
        f.git(&["commit", "-qm", "later destination"])?;
        // Historical acceptance survives only retained history; a fresh launch cannot use it
        // as verification of a later, unchecked destination.
        assert_eq!(
            crate::core::git_ops::accepted_task_integration(&f.root, &handoff)?,
            Some(accepted.clone())
        );
        assert!(
            crate::core::git_ops::require_verified_destination(&f.root, &accepted.destination)
                .is_err()
        );
        let later = head(&f.root, None)?;
        assert!(crate::core::git_ops::require_verified_destination(&f.root, &later).is_err());
        Ok(())
    })
}

/// Unfinished verification is a durable workspace barrier, including after process restart.
#[test]
fn unfinished_verification_blocks_mutations_focus_and_owned_creation() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 7")?;
        let integration = integrated(f)?;
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        let path = record_path(f, &record)?;
        let original = std::fs::read(&path)?;
        let mut value: serde_json::Value = serde_json::from_slice(&original)?;
        value["state"] = serde_json::json!("running");
        value["termination"] = serde_json::json!("running");
        value["finished_at"] = serde_json::Value::Null;
        value["source_identity"] = serde_json::Value::Null;
        value["duration_ms"] = serde_json::json!(0);
        value["passed_commands"] = serde_json::json!([]);
        crate::core::persist::atomic_write_owner_only_checked(&path, &serde_json::to_vec(&value)?)?;
        let identity = inspect_identity(&f.root, None)?;
        assert!(crate::core::git_ops::require_settled_operations(&identity).is_err());
        assert!(crate::core::git_ops::require_focus_settled(&identity).is_err());
        assert!(PreparedVerification::prepare(&f.root, &integration.id).is_err());
        crate::core::persist::atomic_write_owner_only_checked(&path, &original)?;
        crate::core::git_ops::require_settled_operations(&identity)?;
        Ok(())
    })
}

#[test]
fn current_thread_runtime_refusal_does_not_publish_running_attempt() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let prepared = PreparedVerification::prepare(&f.root, &integration.id)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let error = runtime.block_on(async { prepared.execute() }).unwrap_err();
        assert!(format!("{error:#}").contains("current-thread runtime"));
        assert!(verification_records(&f.root)?.is_empty());
        Ok(())
    })
}

/// A record that claims success without a settled pass is rejected by the reader itself, so a
/// tampered file cannot become evidence even before an integration receipt is consulted.
#[test]
fn verification_record_reader_rejects_impossible_outcomes() -> Result<()> {
    fixture(|f| {
        add_manifest(f, "exit 0")?;
        let integration = integrated(f)?;
        let record = PreparedVerification::prepare(&f.root, &integration.id)?.execute()?;
        assert!(record.verified);
        let path = record_path(f, &record)?;
        let original = std::fs::read(&path)?;
        let identity = inspect_identity(&f.root, None)?;
        let directory = path.parent().unwrap().to_path_buf();
        let witness = crate::core::speculation::namespace::create_metadata_directory(&directory)?;
        for (pointer, value) in [
            // A verified record that is not settled is internally impossible.
            ("/state", serde_json::json!("running")),
            ("/schema", serde_json::json!(99)),
            ("/commands", serde_json::json!([])),
            ("/id", serde_json::json!("../escape")),
            ("/termination", serde_json::json!("failed")),
            ("/passed_commands", serde_json::json!([])),
            ("/source_identity", serde_json::Value::Null),
            ("/finished_at", serde_json::Value::Null),
            ("/started_at", serde_json::json!(0)),
        ] {
            let mut changed: serde_json::Value = serde_json::from_slice(&original)?;
            *changed.pointer_mut(pointer).unwrap() = value;
            crate::core::persist::atomic_write_owner_only_checked(
                &path,
                &serde_json::to_vec(&changed)?,
            )?;
            assert!(
                verification_records(&f.root).is_err(),
                "reader accepted tampered {pointer}"
            );
        }
        crate::core::persist::atomic_write_owner_only_checked(&path, &original)?;
        assert_eq!(verification_records(&f.root)?.len(), 1);
        drop(witness);
        assert!(!identity.repo_id.is_empty());
        Ok(())
    })
}
