//! Historical-reader regressions; no live source store is constructed by these fixtures.
use super::*;
use crate::core::workspace_txn::WorkspaceIdentity;

struct Fixture {
    _temp: crate::core::file_placement::tests::TestDir,
    work: PathBuf,
    store: PathBuf,
    identity: WorkspaceIdentity,
}
impl Fixture {
    fn new() -> Self {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let work = temp.path().join("work");
        let store = temp.path().join("evidence");
        fs::create_dir(&work).unwrap();
        fs::create_dir(&store).unwrap();
        let identity = WorkspaceIdentity::discover(&work).unwrap();
        Self {
            _temp: temp,
            work,
            store,
            identity,
        }
    }
}
fn abandoned_directory(fixture: &Fixture) -> PathBuf {
    let file = fixtures::HistoricalFile::explicit(fixture.work.join("a.txt"), None, true);
    fixtures::write_base(&fixture.store, &fixture.identity, &[file])
}
fn inspected_records(directory: &Path) -> Vec<InspectedRecord> {
    let inspection = ExistingEvidenceReader::open(directory)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(inspection
        .limitations
        .contains(&HistoricalLimitation::MissingWorkerRoster));
    inspection.records
}
fn objects(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_str().unwrap().to_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect()
}
fn empty_history(fixture: &Fixture) -> (PathBuf, fixtures::HistoricalAuthor, Vec<u8>) {
    let file = fixtures::HistoricalFile::explicit(fixture.work.join("a.txt"), None, true);
    let directory = fixtures::write_base(
        &fixture.store,
        &fixture.identity,
        std::slice::from_ref(&file),
    );
    let author = fixtures::write_sealed_history(&directory, 1, "parent/task", &[file], &[]);
    let bytes = fs::read(directory.join(&author.identity.manifest)).unwrap();
    (directory, author, bytes)
}
#[test]
fn existing_inventory_never_recreates_missing_namespace_or_owner() {
    let fixture = Fixture::new();
    let directory = fixture
        .store
        .join(NAMESPACE)
        .join("0123456789abcdef0123456789abcdef");
    assert!(inventory_revision(&directory).is_err());
    assert!(inspect_controller_records(&directory).is_err());
    assert!(!fixture.store.join(NAMESPACE).exists());
    let directory = abandoned_directory(&fixture);
    fs::remove_file(directory.join("owner.lock")).unwrap();
    assert!(inventory_revision(&directory).is_err());
    assert!(inspect_controller_records(&directory).is_err());
    assert!(!directory.join("owner.lock").exists());
    fs::remove_dir_all(&directory).unwrap();
    assert!(inventory_revision(&directory).is_err());
    assert!(inspect_controller_records(&directory).is_err());
    assert!(!directory.exists());
}

#[test]
fn existing_inventory_revision_includes_orphans_and_matches_recovery_encoding() {
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    let before = inventory_revision(&directory).unwrap();
    let bytes = b"unreferenced durable evidence";
    let name = Image::bytes(bytes).blob_name().unwrap();
    fs::write(directory.join(&name), bytes).unwrap();
    let after = inventory_revision(&directory).unwrap();
    assert_ne!(before, after);
    let mut paths = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    let mut expected = ring::digest::Context::new(&ring::digest::SHA256);
    expected.update(b"aizen-tm-recovery-inventory-v1\0");
    for path in paths {
        let name = path.file_name().unwrap().to_str().unwrap();
        let image = Image::bytes(&fs::read(&path).unwrap());
        expected.update(&(name.len() as u64).to_le_bytes());
        expected.update(name.as_bytes());
        expected.update(&image.byte_len.to_le_bytes());
        expected.update(&image.sha256);
    }
    assert_eq!(after.as_slice(), expected.finish().as_ref());
    let owner = RepoTxnLock::acquire_existing(
        &directory.join("owner.lock"),
        crate::core::repo_lock::LockMode::Exclusive,
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(
        owner.read_bounded_exclusive(1024).unwrap(),
        b"aizen-speculation-owner-v1\n"
    );
    assert_eq!(
        inventory_revision(&directory).unwrap(),
        after,
        "revision cannot read through a second owner descriptor"
    );
    let start = std::time::Instant::now();
    assert!(inspect_controller_records(&directory).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(owner);
    assert!(inspected_records(&directory).is_empty());
    fs::remove_file(directory.join(name)).unwrap();
    assert_eq!(inventory_revision(&directory).unwrap(), before);
}

#[test]
fn existing_inventory_marker_is_canonical_for_revision_but_inspection_checks_actual_bytes() {
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    let before = inventory_revision(&directory).unwrap();
    let mut marker = b"aizen-speculation-owner-v1\n".to_vec();
    marker[0] = b'x';
    fs::write(directory.join("owner.lock"), &marker).unwrap();
    assert_eq!(inventory_revision(&directory).unwrap(), before);
    assert!(inspect_controller_records(&directory).is_err());
    assert_eq!(fs::read(directory.join("owner.lock")).unwrap(), marker);
    fs::write(directory.join("owner.lock"), b"short").unwrap();
    assert!(inventory_revision(&directory).is_err());
    assert!(inspect_controller_records(&directory).is_err());
}

#[test]
fn existing_inventory_refuses_unknown_corrupt_special_and_linked_objects() {
    for name in [
        ".orphan.aizen-tmp-1-2",
        "b-not-a-content-address",
        "m-invalid.json",
    ] {
        let fixture = Fixture::new();
        let directory = abandoned_directory(&fixture);
        fs::write(directory.join(name), b"retained invalid evidence").unwrap();
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        assert_eq!(
            fs::read(directory.join(name)).unwrap(),
            b"retained invalid evidence"
        );
    }
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    let name = Image::bytes(b"special").blob_name().unwrap();
    fs::create_dir(directory.join(&name)).unwrap();
    assert!(inventory_revision(&directory).is_err());
    assert!(inspect_controller_records(&directory).is_err());
    fs::remove_dir(directory.join(&name)).unwrap();
    let outside = fixture.store.join("retained-foreign-object");
    fs::write(&outside, b"special").unwrap();
    fs::hard_link(&outside, directory.join(name)).unwrap();
    assert!(inventory_revision(&directory).is_err());
    assert!(inspect_controller_records(&directory).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"special");
}

#[test]
fn existing_inventory_enforces_cumulative_object_and_manifest_limits() {
    for manifests in [false, true] {
        let fixture = Fixture::new();
        let directory = abandoned_directory(&fixture);
        let count = if manifests { MAX_RECORDS } else { MAX_OBJECTS };
        for index in 0..count {
            let bytes = format!("{{\"inventory_fixture\":{index}}}").into_bytes();
            let name = if manifests {
                manifest_name(&bytes)
            } else {
                Image::bytes(&bytes).blob_name().unwrap()
            };
            fs::write(directory.join(name), bytes).unwrap();
        }
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            count + 2,
            "quota refusal retains every object"
        );
    }
}

#[test]
fn existing_inventory_aggregate_byte_limit_includes_owner_marker() {
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    for entry in fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() != "owner.lock" {
            fs::remove_file(path).unwrap();
        }
    }
    let mut bytes = vec![0u8; MAX_FILE_BYTES];
    let mut last = PathBuf::new();
    for index in 0..(MAX_STORE_BYTES / MAX_FILE_BYTES) {
        bytes[0] = index as u8;
        last = directory.join(Image::bytes(&bytes).blob_name().unwrap());
        fs::write(&last, &bytes).unwrap();
    }
    assert!(inventory_revision(&directory)
        .unwrap_err()
        .to_string()
        .contains("including owner marker"));
    assert!(inspect_controller_records(&directory).is_err());
    fs::remove_file(last).unwrap();
    assert!(inventory_revision(&directory).is_ok());
    assert!(inspected_records(&directory).is_empty());
}
#[test]
fn sealed_manifest_decoder_rejects_nested_unknown_missing_duplicate_and_oversized_data() {
    let fixture = Fixture::new();
    let (_, _, bytes) = empty_history(&fixture);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for pointer in ["", "/bindings/0", "/bindings/0/base"] {
        let mut mutated = value.clone();
        mutated
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<SealedOverlayManifest>(mutated).is_err(),
            "{pointer}"
        );
    }
    for field in [
        "generation",
        "sealed",
        "bindings",
        "files",
        "observations",
        "edits",
    ] {
        let mut mutated = value.clone();
        mutated.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<SealedOverlayManifest>(mutated).is_err(),
            "{field}"
        );
    }
    let key = &value["bindings"][0]["key"];
    let version = &value["files"][key.as_str().unwrap()];
    let duplicate_files = format!("{{{key}:{version},{key}:{version}}}");
    let text = String::from_utf8(bytes.clone()).unwrap();
    let manifest: SealedOverlayManifest = serde_json::from_slice(&bytes).unwrap();
    let original_files = serde_json::to_string(&manifest.files).unwrap();
    assert!(text.contains(&original_files));
    assert!(
        decode_sealed_manifest(text.replace(&original_files, &duplicate_files).as_bytes()).is_err()
    );
    let duplicate_field = text.replacen("\"sealed\":true", "\"sealed\":false,\"sealed\":true", 1);
    assert!(decode_sealed_manifest(duplicate_field.as_bytes()).is_err());
    assert!(decode_sealed_manifest(&vec![b' '; MAX_META_BYTES + 1]).is_err());
    let mut manifest: SealedOverlayManifest = serde_json::from_slice(&bytes).unwrap();
    manifest.owner = "x".repeat(MAX_OWNER_BYTES + 1);
    assert!(decode_sealed_manifest(&encode_meta(&manifest).unwrap()).is_err());
    manifest.owner = "parent/task".into();
    manifest.observations = vec![
        Observation {
            key: "unknown".into(),
            generation: 0,
            image: Image::missing()
        };
        MAX_OBSERVATIONS + 1
    ];
    assert!(decode_sealed_manifest(&encode_meta(&manifest).unwrap()).is_err());
}

#[test]
fn missing_worker_roster_is_inspectable_but_never_supported() {
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    let before = objects(&directory);
    let inspection = ExistingEvidenceReader::open(&directory)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(inspection.records.is_empty());
    assert_eq!(
        inspection.limitations,
        vec![HistoricalLimitation::MissingWorkerRoster]
    );
    assert!(inspection.require_supported().is_err());
    assert!(inspect_controller_records(&directory).is_err());
    assert_eq!(objects(&directory), before);
}

#[test]
fn existing_reader_detects_inventory_drift_after_open_and_keeps_every_byte() {
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    let reader = ExistingEvidenceReader::open(&directory).unwrap();
    fixtures::write_blob(&directory, b"retained orphan");
    let before = objects(&directory);
    assert!(reader.inspect().is_err());
    assert_eq!(objects(&directory), before);
}

#[test]
fn sealed_manifest_identity_detects_same_length_corruption_and_content_address_substitution() {
    for replacement in [false, true] {
        let fixture = Fixture::new();
        let (directory, author, mut bytes) = empty_history(&fixture);
        if replacement {
            fs::write(directory.join(format!("m-{}.json", "f".repeat(64))), &bytes).unwrap();
        } else {
            bytes[0] ^= 1;
            fs::write(directory.join(author.identity.manifest), &bytes).unwrap();
        }
        let before = objects(&directory);
        assert!(ExistingEvidenceReader::open(&directory).is_err());
        assert_eq!(objects(&directory), before);
        assert!(!fixture.work.join("a.txt").exists());
    }
}

#[test]
fn sealed_lineage_restart_requires_exact_immutable_manifest_and_base() {
    for label in ["integration-prepared", "integration-conflict"] {
        for mismatch in [
            "none",
            "missing",
            "base",
            "owner",
            "overlay",
            "generation",
            "kind",
            "binding",
            "observation",
        ] {
            let fixture = Fixture::new();
            fs::write(fixture.work.join("a.txt"), b"base").unwrap();
            let file =
                fixtures::HistoricalFile::explicit(fixture.work.join("a.txt"), Some(b"base"), true);
            let directory = fixtures::write_base(
                &fixture.store,
                &fixture.identity,
                std::slice::from_ref(&file),
            );
            let author = fixtures::write_sealed_history(
                &directory,
                1,
                "parent/task",
                std::slice::from_ref(&file),
                &[],
            );
            fixtures::write_worker_history(&directory, std::slice::from_ref(&author));
            let identity = &author.identity;
            let mut selected = identity.manifest.clone();
            if mismatch == "missing" {
                selected = format!("m-{}.json", "f".repeat(64));
            } else if !matches!(mismatch, "none" | "observation") {
                let mut manifest: SealedOverlayManifest =
                    serde_json::from_slice(&fs::read(directory.join(&selected)).unwrap()).unwrap();
                match mismatch {
                    "base" => manifest.base = "f".repeat(32),
                    "owner" => manifest.owner = "parent/other".into(),
                    "overlay" => manifest.overlay = "overlay-2".into(),
                    "generation" => manifest.generation = 1,
                    "kind" => manifest.kind = "base".into(),
                    "binding" => manifest.bindings[0].base = Image::bytes(b"foreign"),
                    _ => unreachable!(),
                }
                selected = fixtures::write_manifest(&directory, &encode_meta(&manifest).unwrap());
            }
            let observations = if mismatch == "observation" {
                serde_json::json!([{
                    "author":"parent/task/overlay-1","key":file.key,"path":file.path,"generation":0,"fingerprint":Image::bytes(b"base")
                }])
            } else {
                serde_json::json!([])
            };
            let value = serde_json::json!({"overlay_lineage_version":1,"base":fixtures::FIXTURE_BASE,"owner":"parent",
                "observations":observations,"files":[],"overlays":[{"base":identity.base,"id":identity.overlay,"owner":identity.owner,
                    "author":"parent/task/overlay-1","generation":0,"manifest":selected}]});
            fixtures::write_record(&directory, "parent", label, &value, &[]);
            let before = objects(&directory);
            let inspected = inspect_controller_records(&directory);
            assert_eq!(inspected.is_ok(), mismatch == "none", "{label}: {mismatch}");
            assert_eq!(objects(&directory), before);
            assert_eq!(fs::read(fixture.work.join("a.txt")).unwrap(), b"base");
        }
    }
}

#[test]
fn sealed_history_restart_rejects_unobserved_cas_and_false_localized_provenance() {
    for mismatch in ["none", "missing-read", "provenance"] {
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"abcdef").unwrap();
        let file =
            fixtures::HistoricalFile::explicit(fixture.work.join("a.txt"), Some(b"abcdef"), true);
        let directory = fixtures::write_base(
            &fixture.store,
            &fixture.identity,
            std::slice::from_ref(&file),
        );
        let edits = [
            fixtures::HistoricalEdit {
                key: file.key.clone(),
                after: b"aXcdef".to_vec(),
                provenance: WriteProvenance::Localized { start: 1, end: 2 },
            },
            fixtures::HistoricalEdit {
                key: file.key.clone(),
                after: b"aXYdef".to_vec(),
                provenance: WriteProvenance::Localized { start: 2, end: 3 },
            },
        ];
        let mut author = fixtures::write_sealed_history(
            &directory,
            1,
            "parent/task",
            std::slice::from_ref(&file),
            &edits,
        );
        let mut manifest: SealedOverlayManifest =
            serde_json::from_slice(&fs::read(directory.join(&author.identity.manifest)).unwrap())
                .unwrap();
        match mismatch {
            "missing-read" => manifest.observations.retain(|read| read.generation != 1),
            "provenance" => {
                manifest.edits[0].provenance = WriteProvenance::Localized { start: 2, end: 3 }
            }
            _ => {}
        }
        let encoded = encode_meta(&manifest).unwrap();
        assert_eq!(
            decode_sealed_manifest(&encoded).is_ok(),
            mismatch != "missing-read"
        );
        author.identity.manifest = fixtures::write_manifest(&directory, &encoded);
        author.observations = manifest
            .observations
            .iter()
            .map(|read| {
                serde_json::json!({"author":"parent/task/overlay-1",
            "key":read.key,"path":file.path,"generation":read.generation,"fingerprint":read.image})
            })
            .collect();
        fixtures::write_worker_history(&directory, std::slice::from_ref(&author));
        let value = serde_json::json!({"overlay_lineage_version":1,"base":fixtures::FIXTURE_BASE,"owner":"parent",
            "observations":author.observations,"overlays":[{"base":author.identity.base,"id":author.identity.overlay,
            "owner":author.identity.owner,"author":"parent/task/overlay-1","generation":author.identity.generation,"manifest":author.identity.manifest}]});
        fixtures::write_record(&directory, "parent", "integration-conflict", &value, &[]);
        let before = objects(&directory);
        assert_eq!(
            inspect_controller_records(&directory).is_ok(),
            mismatch == "none",
            "{mismatch}"
        );
        assert_eq!(objects(&directory), before);
        assert_eq!(fs::read(fixture.work.join("a.txt")).unwrap(), b"abcdef");
    }
}

#[test]
fn inspected_expansion_counts_shared_metadata_and_each_image_reference() {
    let fixture = Fixture::new();
    let directory = abandoned_directory(&fixture);
    let metadata = serde_json::json!({"shared":true});
    let image = b"shared image".to_vec();
    fixtures::write_record(
        &directory,
        "owner-a",
        "first",
        &metadata,
        &[image.clone(), image.clone(), image.clone()],
    );
    fixtures::write_record(
        &directory,
        "owner-b",
        "second",
        &metadata,
        &[image.clone(), image.clone()],
    );
    let reader = ExistingEvidenceReader::open(&directory).unwrap();
    let (records, expanded) =
        inspect_records_with_expansion(&reader.directory, &reader.inventory, &reader.directory.id)
            .unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        expanded,
        2 * serde_json::to_vec(&metadata).unwrap().len() + 5 * image.len()
    );
    assert_eq!(
        reader
            .inventory
            .objects
            .keys()
            .filter(|name| name.starts_with("b-"))
            .count(),
        2
    );
    assert_eq!(
        checked_inspection_expansion(MAX_STORE_BYTES - 1, 1, MAX_STORE_BYTES).unwrap(),
        MAX_STORE_BYTES
    );
    assert!(checked_inspection_expansion(MAX_STORE_BYTES, 1, MAX_STORE_BYTES).is_err());
    assert!(checked_inspection_expansion(usize::MAX, 1, MAX_STORE_BYTES).is_err());
    assert!(checked_inspection_expansion(1, 0, 0).is_err());
}
