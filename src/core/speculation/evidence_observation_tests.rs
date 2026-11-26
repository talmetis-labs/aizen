//! Rehomed historical observation regressions. Only disposable metadata fixtures are written.
use super::*;
use crate::core::native_scope::NativeWriteScope;
use crate::core::speculation::evidence::fixtures;
use crate::core::workspace_txn::{with_test_lock_home, WorkspaceIdentity};

struct Fixture {
    _temp: crate::core::file_placement::tests::TestDir,
    work: PathBuf,
    home: PathBuf,
    store: PathBuf,
    identity: WorkspaceIdentity,
    _gate: crate::core::speculation::test_support::GateTestLock,
}
impl Fixture {
    fn new() -> Self {
        let gate = crate::core::speculation::test_support::GateTestLock::acquire();
        let temp = crate::core::file_placement::tests::TestDir::new();
        let work = temp.path().join("work");
        let home = temp.path().join("locks");
        let store = temp.path().join("evidence");
        for path in [&work, &home, &store] {
            fs::create_dir(path).unwrap();
        }
        let identity = WorkspaceIdentity::discover(&work).unwrap();
        Self {
            _temp: temp,
            work,
            home,
            store,
            identity,
            _gate: gate,
        }
    }
    fn scope(&self, reads: &[&str], writes: &[&str]) -> NativeWriteScope {
        NativeWriteScope::resolve_access(
            &self.work,
            &reads
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            &writes
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }
    fn guards(&self) -> (WorkspaceWriterLease, PhysicalEffectLease) {
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            self.identity.clone(),
            Duration::ZERO,
            None,
            "historical observation fixture",
            "controller",
        )
        .unwrap();
        let physical = PhysicalEffectLease::exclusive(
            Duration::ZERO,
            None,
            "historical observation fixture",
            "controller",
        )
        .unwrap();
        (writer, physical)
    }
}

fn observation_fixture(
    fixture: &Fixture,
) -> (
    PathBuf,
    crate::core::speculation::recovery::InspectionObservation,
) {
    use crate::core::speculation::recovery::{InspectionObservation, ObservationState};
    fs::write(fixture.work.join("a.txt"), b"unchanged source").unwrap();
    let file = fixtures::HistoricalFile::explicit(
        fixture.work.join("a.txt"),
        Some(b"unchanged source"),
        true,
    );
    let directory = fixtures::write_base(&fixture.store, &fixture.identity, &[file]);
    // Deliberately old incomplete metadata remains diagnostic; this does not fabricate settlement.
    let prepared = fixtures::write_record(
        &directory,
        "integration-owner",
        "integration-prepared",
        &serde_json::json!({}),
        &[],
    );
    let revision = inventory_revision(&directory).unwrap();
    (
        directory,
        InspectionObservation {
            schema: 1,
            base: fixtures::FIXTURE_BASE.into(),
            prepared,
            receipt: None,
            owner: "integration-owner".into(),
            observer: "controller".into(),
            state: ObservationState::ObservedNonterminal,
            inventory_sha256: revision,
            files: Vec::new(),
            native_records: Vec::new(),
        },
    )
}

fn observation_objects(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            (name != "owner.lock").then(|| (name, fs::read(path).unwrap()))
        })
        .collect()
}
fn observation_pair(
    observation: &crate::core::speculation::recovery::InspectionObservation,
) -> (Vec<u8>, String, Vec<u8>, String) {
    let metadata = serde_json::to_vec(observation).unwrap();
    let blob = Image::bytes(&metadata).blob_name().unwrap();
    let envelope = observation_envelope(observation, &metadata).unwrap();
    let manifest = manifest_name(&envelope);
    (metadata, blob, envelope, manifest)
}
fn inspected_records(directory: &Path) -> Vec<InspectedRecord> {
    let inspected = ExistingEvidenceReader::open(directory)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(inspected
        .limitations
        .contains(&HistoricalLimitation::MissingWorkerRoster));
    inspected.records
}
#[test]
fn existing_observation_owner_acquire_is_existing_only_busy_stale_and_marker_checked() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    let fixture = Fixture::new();
    let (directory, observation) = observation_fixture(&fixture);
    with_test_lock_home(&fixture.home, || {
        let (writer, physical) = fixture.guards();
        exec_ctx::with_current(
            ExecutionContext::default().with_resource_scope("controller"),
            || {
                let missing = fixture.store.join(NAMESPACE).join("0".repeat(32));
                assert!(
                    ExistingObservationAppend::acquire(&missing, [0; 32], &writer, &physical)
                        .is_err()
                );
                assert!(!missing.exists());
                let before = observation_objects(&directory);
                let owner = ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical,
                )
                .unwrap();
                assert!(ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical
                )
                .is_err());
                drop(owner);
                let mut stale = observation.inventory_sha256;
                stale[0] ^= 1;
                assert!(
                    ExistingObservationAppend::acquire(&directory, stale, &writer, &physical)
                        .is_err()
                );
                assert_eq!(observation_objects(&directory), before);
                let lock = directory.join("owner.lock");
                let mut marker = OWNER_MARKER.to_vec();
                marker[0] ^= 1;
                fs::write(&lock, &marker).unwrap();
                // Canonical inventory bytes alone do not bless a same-length corrupt marker.
                assert_eq!(
                    inventory_revision(&directory).unwrap(),
                    observation.inventory_sha256
                );
                assert!(ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical
                )
                .is_err());
                assert_eq!(fs::read(&lock).unwrap(), marker);
                fs::remove_file(&lock).unwrap();
                assert!(ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical
                )
                .is_err());
                assert!(!lock.exists());
                assert_eq!(observation_objects(&directory), before);
            },
        );
    });
    assert_eq!(
        fs::read(fixture.work.join("a.txt")).unwrap(),
        b"unchanged source"
    );
}

#[test]
fn existing_observation_owner_refuses_absent_delegated_and_native_contexts() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    let fixture = Fixture::new();
    let (directory, observation) = observation_fixture(&fixture);
    let before = observation_objects(&directory);
    with_test_lock_home(&fixture.home, || {
        let (writer, physical) = fixture.guards();
        assert!(exec_ctx::current().is_none());
        assert!(ExistingObservationAppend::acquire(
            &directory,
            observation.inventory_sha256,
            &writer,
            &physical
        )
        .is_err());
        let parent = ExecutionContext::default().with_resource_scope("controller");
        let scoped = parent
            .with_native_scope(fixture.scope(&[], &["a.txt"]))
            .unwrap();
        for context in [parent.without_workspace_restore(), scoped] {
            exec_ctx::with_current(context, || {
                assert!(ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical
                )
                .is_err());
            });
        }
        exec_ctx::with_current(parent, || {
            let owner = ExistingObservationAppend::acquire(
                &directory,
                observation.inventory_sha256,
                &writer,
                &physical,
            )
            .unwrap();
            assert!(!owner.consumed && !owner.poisoned);
        });
    });
    assert_eq!(observation_objects(&directory), before);
}

#[test]
fn existing_observation_owner_success_is_one_shot_exact_pair_and_no_source_effect() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    let fixture = Fixture::new();
    let (directory, observation) = observation_fixture(&fixture);
    let before = observation_objects(&directory);
    let (metadata, blob, envelope, manifest) = observation_pair(&observation);
    with_test_lock_home(&fixture.home, || {
        let (writer, physical) = fixture.guards();
        exec_ctx::with_current(
            ExecutionContext::default().with_resource_scope("controller"),
            || {
                let mut owner = ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical,
                )
                .unwrap();
                let (bytes, records, objects) = (
                    owner.disk.bytes,
                    owner.disk.records,
                    owner.disk.objects.len(),
                );
                let publication = owner.append(&observation).unwrap();
                assert_eq!(publication.record, manifest);
                assert_eq!(publication.before, observation.inventory_sha256);
                assert_eq!(publication.after, inventory_revision(&directory).unwrap());
                assert_ne!(publication.before, publication.after);
                assert_eq!(owner.disk.bytes, bytes + metadata.len() + envelope.len());
                assert_eq!(owner.disk.records, records + 1);
                assert_eq!(owner.disk.objects.len(), objects + 2);
                assert!(owner.consumed && !owner.poisoned);
                assert!(owner.append(&observation).is_err());
                let mut expected = before.clone();
                expected.insert(blob.clone(), metadata.clone());
                expected.insert(manifest.clone(), envelope.clone());
                assert_eq!(observation_objects(&directory), expected);
            },
        );
    });
    let records = inspected_records(&directory);
    let record = records.iter().find(|record| record.id == manifest).unwrap();
    assert_eq!(
        record.label,
        crate::core::speculation::recovery::OBSERVATION_LABEL
    );
    assert_eq!(record.owner, "controller");
    assert_eq!(record.metadata, metadata);
    assert!(record.images.is_empty());
    assert_eq!(
        fs::read(fixture.work.join("a.txt")).unwrap(),
        b"unchanged source"
    );
}

#[test]
fn existing_observation_owner_publication_fault_retains_evidence_and_consumes_holder() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    for point in [Fault::BeforePublish, Fault::AfterPublish] {
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        let (metadata, blob, _, manifest) = observation_pair(&observation);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            exec_ctx::with_current(
                ExecutionContext::default().with_resource_scope("controller"),
                || {
                    let mut owner = ExistingObservationAppend::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    FAULT.with(|slot| slot.set(Some(point)));
                    let result = owner.append(&observation);
                    let remaining_fault = FAULT.with(|slot| slot.replace(None));
                    assert!(remaining_fault.is_none());
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains("persisted evidence may remain"));
                    assert!(owner.consumed && owner.poisoned);
                    assert!(owner.append(&observation).is_err());
                    let mut expected = before.clone();
                    if point == Fault::AfterPublish {
                        expected.insert(blob.clone(), metadata.clone());
                    }
                    assert_eq!(observation_objects(&directory), expected);
                    assert!(!directory.join(&manifest).exists());
                },
            );
        });
        assert_eq!(
            fs::read(fixture.work.join("a.txt")).unwrap(),
            b"unchanged source"
        );
        // Current fault hooks reach the first (metadata blob) publication only, not the manifest.
    }
}

#[test]
fn existing_observation_owner_preflights_expanded_records_before_either_object() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    let fixture = Fixture::new();
    let (directory, observation) = observation_fixture(&fixture);
    let before = observation_objects(&directory);
    let (metadata, blob, _, manifest) = observation_pair(&observation);
    with_test_lock_home(&fixture.home, || {
        let (writer, physical) = fixture.guards();
        exec_ctx::with_current(
            ExecutionContext::default().with_resource_scope("controller"),
            || {
                let mut owner = ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical,
                )
                .unwrap();
                let (_, expanded) =
                    inspect_records_with_expansion(&owner.storage, &owner.disk, &observation.base)
                        .unwrap();
                let exact = expanded + metadata.len();
                // Exercise the real one-shot publication with a smaller private test-only budget.
                let error = owner.append_bounded(&observation, exact - 1).unwrap_err();
                assert!(format!("{error:#}").contains("expansion exceeds bound"));
                assert!(owner.consumed && owner.poisoned);
                assert_eq!(observation_objects(&directory), before);
                assert!(!directory.join(&blob).exists() && !directory.join(&manifest).exists());
                drop(owner);
                let mut owner = ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical,
                )
                .unwrap();
                owner.append_bounded(&observation, exact).unwrap();
                let (records, after) =
                    inspect_records_with_expansion(&owner.storage, &owner.disk, &observation.base)
                        .unwrap();
                assert_eq!(after, exact);
                assert!(records.iter().any(|record| record.id == manifest));
            },
        );
    });
    assert_eq!(inspected_records(&directory).len(), 2);
    assert_eq!(
        fs::read(fixture.work.join("a.txt")).unwrap(),
        b"unchanged source"
    );
}

#[test]
fn existing_observation_owner_manifest_faults_retain_objects_without_false_success() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    for point in [
        Fault::BeforeObservationManifest,
        Fault::AfterObservationManifest,
    ] {
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        let (metadata, blob, envelope, manifest) = observation_pair(&observation);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            exec_ctx::with_current(
                ExecutionContext::default().with_resource_scope("controller"),
                || {
                    let mut owner = ExistingObservationAppend::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    FAULT.with(|slot| slot.set(Some(point)));
                    let result = owner.append(&observation);
                    assert!(FAULT.with(|slot| slot.replace(None)).is_none());
                    assert!(result
                        .unwrap_err()
                        .to_string()
                        .contains("persisted evidence may remain"));
                    assert!(owner.consumed && owner.poisoned);
                    assert!(owner.append(&observation).is_err());
                    let mut expected = before.clone();
                    expected.insert(blob.clone(), metadata.clone());
                    if point == Fault::AfterObservationManifest {
                        expected.insert(manifest.clone(), envelope.clone());
                    }
                    assert_eq!(observation_objects(&directory), expected);
                },
            );
        });
        let records = inspected_records(&directory);
        assert_eq!(
            records
                .iter()
                .filter(
                    |record| record.label == crate::core::speculation::recovery::OBSERVATION_LABEL
                )
                .count(),
            usize::from(point == Fault::AfterObservationManifest)
        );
        assert_ne!(
            inventory_revision(&directory).unwrap(),
            observation.inventory_sha256
        );
        assert_eq!(
            fs::read(fixture.work.join("a.txt")).unwrap(),
            b"unchanged source"
        );
    }
}

#[test]
fn existing_observation_owner_changed_context_or_missing_prepared_consumes_without_publication() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    for failure in ["scope", "delegated", "prepared"] {
        let fixture = Fixture::new();
        let (directory, mut observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            let parent = ExecutionContext::default().with_resource_scope("controller");
            exec_ctx::with_current(parent.clone(), || {
                let mut owner = ExistingObservationAppend::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical,
                )
                .unwrap();
                if failure == "prepared" {
                    observation.prepared = format!("m-{}.json", "f".repeat(64));
                }
                let context = match failure {
                    "scope" => parent.with_resource_scope("other-observer"),
                    "delegated" => parent.without_workspace_restore(),
                    _ => parent.clone(),
                };
                exec_ctx::with_current(context, || {
                    assert!(owner.append(&observation).is_err());
                });
                assert!(owner.consumed && owner.poisoned);
                assert!(owner.append(&observation).is_err());
                assert_eq!(observation_objects(&directory), before);
            });
        });
    }
}

#[test]
fn existing_observation_append_preflights_complete_pair_before_metadata_blob() {
    use crate::core::exec_ctx::{self, ExecutionContext};
    for quota in ["bytes", "records", "objects"] {
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        let (metadata, blob, _, manifest) = observation_pair(&observation);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            exec_ctx::with_current(
                ExecutionContext::default().with_resource_scope("controller"),
                || {
                    let mut owner = ExistingObservationAppend::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    // Lower private budgets exercise the real append/preflight without fake disk accounting.
                    let mut limits = ObservationLimits::default();
                    match quota {
                        "bytes" => {
                            limits.bytes = OWNER_MARKER.len() + owner.disk.bytes + metadata.len()
                        }
                        "records" => limits.records = owner.disk.records,
                        "objects" => limits.objects = owner.disk.objects.len() + 1,
                        _ => unreachable!(),
                    }
                    assert!(owner
                        .reserve_pair(&[(&blob, metadata.len())], 0, limits)
                        .is_ok());
                    assert!(
                        owner.append_with_limits(&observation, limits).is_err(),
                        "{quota}"
                    );
                    assert!(owner.consumed && owner.poisoned);
                    assert!(owner.append(&observation).is_err());
                    assert!(!directory.join(&blob).exists() && !directory.join(&manifest).exists());
                    assert_eq!(observation_objects(&directory), before);
                },
            );
        });
    }
}
