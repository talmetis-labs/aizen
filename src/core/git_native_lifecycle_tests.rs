use super::*;
use crate::features::coop::{GitTaskRegistration, GitTaskState, SessionManifest, SessionState};
use serde_json::{json, Value};

struct Fixture {
    _directory: crate::core::file_placement::tests::TestDir,
    identity: WorkspaceIdentity,
    scope: NativeWriteScope,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let directory = crate::core::file_placement::tests::TestDir::new();
        let root = directory.path().join("work");
        let git = directory.path().join("git");
        let home = directory.path().join("home");
        for path in [&root, &git, &home] {
            std::fs::create_dir(path)?;
        }
        std::fs::write(root.join("file.txt"), b"retained source\n")?;
        let root = std::fs::canonicalize(root)?;
        let git = std::fs::canonicalize(git)?;
        let identity = WorkspaceIdentity::from_bound_paths(root, Some(git.clone()), Some(git));
        let scope = NativeWriteScope::resolve(&identity.canonical_root, &["file.txt".into()])?;
        Ok(Self {
            _directory: directory,
            identity,
            scope,
            home,
        })
    }
    fn binding(&self) -> Result<NativeBinding> {
        NativeBinding::new(
            &self.identity,
            "git-task-v2-fixture",
            "refs/heads/task",
            "parent/task",
            &oid('a'),
            &self.scope,
        )
    }
    fn manifest(&self, native: NativeLifecycle) -> Result<SessionManifest> {
        Ok(SessionManifest {
            schema: 1,
            session_id: "git-task-v2-fixture".into(),
            pid: 1,
            binary_version: "fixture".into(),
            repo_id: self.identity.repo_id.clone(),
            worktree_id: self.identity.worktree_id.clone(),
            root: self.identity.canonical_root.display().to_string(),
            branch: Some("refs/heads/task".into()),
            session_name: None,
            task: "fixture".into(),
            task_pinned: true,
            state: SessionState::Working,
            turns: 0,
            base_checkpoint: None,
            files: Vec::new(),
            truncated_files: 0,
            degraded: None,
            coverage_gap: false,
            started_unix: 1,
            updated_unix: 1,
            git_task: Some(GitTaskState {
                native_lifecycle: Some(native),
                owner: "parent/task".into(),
                base: oid('a'),
                tip: oid('a'),
                phase: "running".into(),
                inverse: None,
                paths: Vec::new(),
                verified: false,
            }),
        })
    }
    fn open(&self) -> Result<NativeLifecycle> {
        let mut evidence = NativeLifecycle::required();
        evidence.begin(self.binding()?)?;
        Ok(evidence)
    }
    fn manifest_path(&self) -> Result<PathBuf> {
        std::fs::read_dir(
            self.home
                .join("coop")
                .join(&self.identity.repo_id)
                .join("sessions"),
        )?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .context("fixture manifest absent")
    }
}
fn oid(ch: char) -> GitOid {
    GitOid::parse(&ch.to_string().repeat(40)).unwrap()
}

#[test]
fn metadata_schema_is_exact_bounded_and_required_before_native_admission() -> Result<()> {
    let f = Fixture::new()?;
    let mut evidence = f.open()?;
    evidence.append_intent(&f.binding()?, NativeWorkerKind::Edit, [9; 32])?;
    let encoded = serde_json::to_value(&evidence)?;
    for corrupt in ["version", "max_workers", "max_manifest_bytes"] {
        let mut value = encoded.clone();
        value[corrupt] = json!(0);
        let decoded: NativeLifecycle = serde_json::from_value(value)?;
        assert!(decoded.validate().is_err(), "accepted {corrupt}");
    }
    let mut unknown = encoded.clone();
    unknown["extra"] = json!(true);
    assert!(serde_json::from_value::<NativeLifecycle>(unknown).is_err());
    let mut missing = encoded.clone();
    missing["evidence"]["workers"][0]
        .as_object_mut()
        .unwrap()
        .remove("closed");
    assert!(serde_json::from_value::<NativeLifecycle>(missing).is_err());
    let mut too_many = encoded.clone();
    too_many["evidence"]["workers"] = json!(vec![
        encoded["evidence"]["workers"][0].clone();
        MAX_WORKERS + 1
    ]);
    assert!(serde_json::from_value::<NativeLifecycle>(too_many).is_err());
    assert!(NativeLifecycle::required().require_candidate().is_err());
    assert!(evidence.require_candidate().is_err());
    assert!(evidence.begin(f.binding()?).is_err());
    Ok(())
}

#[test]
fn metadata_binding_covers_registration_workspace_base_owner_and_scope() -> Result<()> {
    let f = Fixture::new()?;
    let evidence = f.open()?;
    let manifest = f.manifest(evidence.clone())?;
    evidence.validate_manifest(&manifest)?;
    for key in [
        "registration",
        "repo_id",
        "worktree_id",
        "owner",
        "branch",
        "base",
    ] {
        let mut value = serde_json::to_value(&evidence)?;
        value["evidence"]["binding"][key] = match key {
            "registration" => json!("git-task-v2-other"),
            "base" => json!(oid('b')),
            "branch" => json!("refs/heads/other"),
            _ => json!("other"),
        };
        let changed: NativeLifecycle = serde_json::from_value(value)?;
        assert!(
            changed.validate_manifest(&manifest).is_err(),
            "accepted changed {key}"
        );
    }
    let foreign = Fixture::new()?;
    assert!(f.binding()?.require_identity(&foreign.identity).is_err());
    assert_ne!(scope_digest(&f.scope)?, scope_digest(&foreign.scope)?);
    Ok(())
}

#[test]
fn metadata_intent_inventory_is_contiguous_bounded_and_not_terminal() -> Result<()> {
    let f = Fixture::new()?;
    let binding = f.binding()?;
    let mut evidence = f.open()?;
    for ordinal in 1..=MAX_WORKERS {
        let fact = evidence.append_intent(&binding, NativeWorkerKind::Read, [ordinal as u8; 32])?;
        assert_eq!(fact.worker as usize, ordinal);
        assert_eq!(fact.intent, binding.intent(ordinal as u32));
    }
    let before = serde_json::to_vec(&evidence)?;
    assert!(evidence
        .append_intent(&binding, NativeWorkerKind::Read, [0; 32])
        .is_err());
    assert_eq!(serde_json::to_vec(&evidence)?, before);
    assert!(evidence.require_candidate().is_err());
    let mut gap: Value = serde_json::from_slice(&before)?;
    gap["evidence"]["workers"][1]["ordinal"] = json!(3);
    assert!(serde_json::from_value::<NativeLifecycle>(gap)?
        .validate()
        .is_err());
    Ok(())
}

// These are untrusted historical-data fixtures, not live runtime proofs or publication helpers.
fn historical_release(
    f: &Fixture,
    result: WorkerResult,
    effect: DurableEffectState,
    driver: DriverResult,
) -> Result<NativeLifecycle> {
    let mut evidence = f.open()?;
    evidence.append_intent(&f.binding()?, NativeWorkerKind::Write, [7; 32])?;
    let mut value = serde_json::to_value(evidence)?;
    value["evidence"]["state"] = json!("released");
    value["evidence"]["driver"] = json!(driver);
    value["evidence"]["ownership_released"] = json!(true);
    value["evidence"]["workers"][0]["closed"] = json!({"result": result, "effect": effect});
    Ok(serde_json::from_value(value)?)
}

#[test]
fn historical_physical_closure_keeps_failure_and_uncertainty_distinct_from_candidate() -> Result<()>
{
    let f = Fixture::new()?;
    for (result, effect, driver, candidate) in [
        (
            WorkerResult::Returned,
            DurableEffectState::Settled,
            DriverResult::Completed,
            true,
        ),
        (
            WorkerResult::Failed,
            DurableEffectState::Settled,
            DriverResult::Completed,
            false,
        ),
        (
            WorkerResult::Panicked,
            DurableEffectState::Settled,
            DriverResult::Completed,
            false,
        ),
        (
            WorkerResult::Returned,
            DurableEffectState::UnknownWorker,
            DriverResult::Completed,
            false,
        ),
        (
            WorkerResult::Returned,
            DurableEffectState::PossiblyApplied { record: None },
            DriverResult::Completed,
            false,
        ),
        (
            WorkerResult::Returned,
            DurableEffectState::Settled,
            DriverResult::Cancelled,
            false,
        ),
        (
            WorkerResult::Returned,
            DurableEffectState::Settled,
            DriverResult::Failed,
            false,
        ),
    ] {
        let evidence = historical_release(&f, result, effect, driver)?;
        evidence.validate()?;
        assert_eq!(evidence.require_candidate().is_ok(), candidate);
        let json = serde_json::to_value(&evidence)?;
        assert_eq!(
            json["evidence"]["workers"][0]["closed"]["effect"],
            serde_json::to_value(effect)?
        );
        let mut manifest = f.manifest(evidence.clone())?;
        evidence.validate_manifest(&manifest)?;
        manifest.state = SessionState::Finished;
        manifest.git_task.as_mut().unwrap().phase = "handed_off".into();
        assert_eq!(evidence.validate_manifest(&manifest).is_ok(), candidate);
    }
    Ok(())
}

#[test]
fn historical_release_requires_exact_complete_inventory_and_explicit_resource_release() -> Result<()>
{
    let f = Fixture::new()?;
    let original = serde_json::to_value(historical_release(
        &f,
        WorkerResult::Returned,
        DurableEffectState::Settled,
        DriverResult::Completed,
    )?)?;
    for (field, value) in [
        ("ownership_released", json!(false)),
        (
            "workers",
            json!([{
                "ordinal": 1, "kind": "write", "prepared_sha256": vec![7u8; 32], "closed": null
            }]),
        ),
    ] {
        let mut bad = original.clone();
        bad["evidence"][field] = value;
        assert!(serde_json::from_value::<NativeLifecycle>(bad)?
            .validate()
            .is_err());
    }
    let mut missing = original.clone();
    missing["evidence"]
        .as_object_mut()
        .unwrap()
        .remove("ownership_released");
    assert!(serde_json::from_value::<NativeLifecycle>(missing).is_err());
    let mut unknown = original;
    unknown["evidence"]["workers"][0]["closed"]["effect"]["unexpected"] = json!(true);
    assert!(serde_json::from_value::<NativeLifecycle>(unknown).is_err());
    Ok(())
}

#[test]
fn inverse_exemption_is_explicit_exact_and_cannot_replace_an_admitted_native_attempt() -> Result<()>
{
    let f = Fixture::new()?;
    let inverse = crate::core::git_ops::PathInverse {
        commit: oid('b'),
        parent: oid('a'),
        mainline: None,
        paths: vec!["file.txt".into()],
    };
    let mut evidence = NativeLifecycle::required();
    assert!(evidence.require_transition(true).is_err());
    evidence.start_inverse(&inverse)?;
    assert!(evidence.require_candidate().is_err());
    evidence.require_transition(true)?;
    let mut manifest = f.manifest(evidence.clone())?;
    manifest.git_task.as_mut().unwrap().inverse = Some(inverse.clone());
    manifest.git_task.as_mut().unwrap().phase = "applying_inverse".into();
    evidence.validate_manifest(&manifest)?;
    manifest
        .git_task
        .as_mut()
        .unwrap()
        .inverse
        .as_mut()
        .unwrap()
        .commit = oid('c');
    assert!(evidence.validate_manifest(&manifest).is_err());
    assert!(evidence.begin(f.binding()?).is_err());
    assert!(f.open()?.start_inverse(&inverse).is_err());
    Ok(())
}

fn registry_fixture(test: impl FnOnce(&Fixture, GitTaskRegistration) -> Result<()>) -> Result<()> {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let f = Fixture::new()?;
    let _environment = crate::core::config::EnvGuard::set([("AIZEN_HOME", &f.home)]);
    crate::core::workspace_txn::with_test_lock_home(&f.home, || {
        let barrier = crate::features::coop::acquire_workspace_registration_barrier(&f.identity)?;
        let registration = GitTaskRegistration::create(
            &f.identity,
            "refs/heads/task",
            "parent/task",
            &oid('a'),
            &barrier,
        )?;
        test(&f, registration)
    })
}

#[test]
fn registration_admission_fault_before_or_after_replace_is_sticky_and_blocks_restart() -> Result<()>
{
    for after_replace in [false, true] {
        registry_fixture(|f, registration| {
            let path = f.manifest_path()?;
            let before = std::fs::read(&path)?;
            registration.fail_next_native_publication_for_test(after_replace);
            assert!(registration.begin_native(&f.identity, &f.scope).is_err());
            assert!(registration.begin_native(&f.identity, &f.scope).is_err());
            assert!(registration.verify(&f.identity).is_err());
            let after = std::fs::read(&path)?;
            assert_eq!(before == after, !after_replace);
            drop(registration);
            assert!(crate::features::coop::require_no_other_workspace_owner(&f.identity).is_err());
            assert_eq!(std::fs::read(&path)?, after);
            assert_eq!(
                std::fs::read(f.identity.canonical_root.join("file.txt"))?,
                b"retained source\n"
            );
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn registration_intent_fault_never_republishes_or_treats_missing_fact_as_settlement() -> Result<()>
{
    for after_replace in [false, true] {
        registry_fixture(|f, registration| {
            let binding = registration.begin_native(&f.identity, &f.scope)?;
            let path = f.manifest_path()?;
            let before = std::fs::read(&path)?;
            registration.fail_next_native_publication_for_test(after_replace);
            assert!(registration
                .publish_native_intent(&binding, NativeWorkerKind::Edit, [5; 32])
                .is_err());
            assert!(registration
                .publish_native_intent(&binding, NativeWorkerKind::Edit, [5; 32])
                .is_err());
            assert!(registration
                .record("handed_off", &oid('a'), Vec::new())
                .is_err());
            let after = std::fs::read(&path)?;
            assert_eq!(before == after, !after_replace);
            let manifest: SessionManifest = serde_json::from_slice(&after)?;
            let native = manifest.git_task.unwrap().native_lifecycle.unwrap();
            let EvidenceState::Open { workers, .. } = native.evidence else {
                panic!("intent fault fabricated terminal evidence")
            };
            assert_eq!(workers.len(), usize::from(after_replace));
            drop(registration);
            assert!(crate::features::coop::require_no_other_workspace_owner(&f.identity).is_err());
            assert_eq!(std::fs::read(&path)?, after);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn registry_rejects_v2_missing_or_inconsistent_terminal_evidence_even_after_owner_dies(
) -> Result<()> {
    for missing in [true, false] {
        registry_fixture(|f, registration| {
            let path = f.manifest_path()?;
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            value["state"] = json!("finished");
            value["git_task"]["phase"] = json!("handed_off");
            if missing {
                value["git_task"]
                    .as_object_mut()
                    .unwrap()
                    .remove("native_lifecycle");
            }
            let bytes = serde_json::to_vec(&value)?;
            std::fs::write(&path, &bytes)?;
            drop(registration);
            assert!(crate::features::coop::require_no_other_workspace_owner(&f.identity).is_err());
            assert!(crate::features::coop::require_no_other_repository_owner(&f.identity).is_err());
            assert!(crate::features::coop::FrozenGitTask::select(
                &f.identity,
                value["session_id"].as_str().unwrap()
            )
            .is_err());
            assert_eq!(std::fs::read(path)?, bytes);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn terminal_capacity_is_checked_before_admission_and_before_any_worker_intent() -> Result<()> {
    registry_fixture(|f, registration| {
        // Leave room for the running registration, but not for its full worker envelope.
        // Long filesystem paths would hit the host's path limit before the manifest limit.
        registration.set_task_description_for_test("x".repeat(MAX_MANIFEST_BYTES - 8 * 1024))?;
        let path = f.manifest_path()?;
        let before = std::fs::read(&path)?;
        let error = registration
            .begin_native(&f.identity, &f.scope)
            .unwrap_err();
        assert!(error.to_string().contains("128 KiB"), "{error:#}");
        assert_eq!(std::fs::read(path)?, before);
        assert_eq!(
            std::fs::read(f.identity.canonical_root.join("file.txt"))?,
            b"retained source\n"
        );
        Ok(())
    })
}

#[test]
fn registration_can_account_for_128_intents_but_never_publish_a_handoff_without_release(
) -> Result<()> {
    registry_fixture(|f, registration| {
        let binding = registration.begin_native(&f.identity, &f.scope)?;
        for ordinal in 1..=MAX_WORKERS {
            assert_eq!(
                registration
                    .publish_native_intent(&binding, NativeWorkerKind::Write, [255; 32])?
                    .worker as usize,
                ordinal
            );
        }
        let path = f.manifest_path()?;
        let before = std::fs::read(&path)?;
        assert!(before.len() <= MAX_MANIFEST_BYTES);
        assert!(registration
            .publish_native_intent(&binding, NativeWorkerKind::Write, [255; 32])
            .is_err());
        assert!(registration
            .record("committing", &oid('a'), vec!["file.txt".into()])
            .is_err());
        assert!(registration
            .record("handed_off", &oid('a'), Vec::new())
            .is_err());
        assert!(registration.handoff_receipt().is_err());
        assert_eq!(std::fs::read(path)?, before);
        Ok(())
    })
}

#[test]
fn native_terminal_path_reservation_matches_canonical_resource_case_semantics() -> Result<()> {
    registry_fixture(|f, registration| {
        registration.begin_native(&f.identity, &f.scope)?;
        registration.check_reserved_native_paths_for_test(&[])?;
        registration.check_reserved_native_paths_for_test(&["file.txt".into()])?;
        assert!(registration
            .check_reserved_native_paths_for_test(&["other.txt".into()])
            .is_err());
        assert!(registration
            .check_reserved_native_paths_for_test(&["file.txt".into(), "file.txt".into()])
            .is_err());
        let alias = registration.check_reserved_native_paths_for_test(&["FILE.TXT".into()]);
        #[cfg(any(windows, target_os = "macos"))]
        assert!(
            alias.is_ok(),
            "ASCII case must retain the existing NativeResource identity"
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(
            alias.is_err(),
            "case-sensitive native identities cannot widen at publication"
        );
        assert!(registration
            .check_reserved_native_paths_for_test(&["../file.txt".into()])
            .is_err());
        assert!(registration
            .check_reserved_native_paths_for_test(&["file.txt/".into()])
            .is_err());
        Ok(())
    })
}
