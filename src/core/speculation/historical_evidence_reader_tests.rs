//! Historical reader regressions rehomed from integration_tests.rs; no integration engine.
use super::fixtures::*;
use super::*;
use crate::core::file_placement::tests::TestDir;
use crate::core::workspace_txn::WorkspaceIdentity;
use serde_json::{json, Value};

struct Fixture {
    _temp: TestDir,
    work: PathBuf,
    namespace: PathBuf,
    identity: WorkspaceIdentity,
}
impl Fixture {
    fn new() -> Self {
        let temp = TestDir::new();
        let work = temp.path().join("workspace");
        let namespace = temp.path().join("history");
        fs::create_dir(&work).unwrap();
        fs::create_dir(&namespace).unwrap();
        let identity = WorkspaceIdentity::discover(&work).unwrap();
        Self {
            _temp: temp,
            work: identity.canonical_root.clone(),
            namespace,
            identity,
        }
    }
    fn file(&self, name: &str, before: Option<&[u8]>, writable: bool) -> HistoricalFile {
        HistoricalFile::explicit(self.work.join(name), before, writable)
    }
    fn directory(&self, files: &[HistoricalFile]) -> PathBuf {
        write_base(&self.namespace, &self.identity, files)
    }
}
fn changed(file: &HistoricalFile, after: &[u8], provenance: WriteProvenance) -> HistoricalEdit {
    HistoricalEdit {
        key: file.key.clone(),
        after: after.to_vec(),
        provenance,
    }
}
fn publish_prepared(
    directory: &Path,
    authors: &[HistoricalAuthor],
    candidates: BTreeMap<String, Vec<u8>>,
) -> (Value, Vec<Vec<u8>>) {
    let (value, images) = prepared_document(authors, &candidates);
    write_record(directory, "parent", "integration-prepared", &value, &images);
    (value, images)
}
fn inspect_supported(directory: &Path) -> Result<Vec<InspectedRecord>> {
    ExistingEvidenceReader::open(directory)?
        .inspect()?
        .require_supported()
}
const RUST_BASE: &[u8] = b"fn alpha() -> i32 {\n    101\n}\n\nfn beta() -> i32 {\n    202\n}\n";
const RUST_A: &[u8] = b"fn alpha() -> i32 {\n    111\n}\n\nfn beta() -> i32 {\n    202\n}\n";
const RUST_B: &[u8] = b"fn alpha() -> i32 {\n    101\n}\n\nfn beta() -> i32 {\n    222\n}\n";
const HISTORICAL_MERGED: &[u8] =
    b"fn alpha() -> i32 {\n    111\n}\n\nfn beta() -> i32 {\n    222\n}\n";

#[test]
fn sealed_lineage_allows_distinct_overlays_owned_by_the_same_task() {
    let f = Fixture::new();
    let files = [
        f.file("a.txt", Some(b"base"), true),
        f.file("b.txt", Some(b"base"), true),
    ];
    // This is a historical already-applied image, not execution through a retained old writer.
    fs::write(f.work.join("a.txt"), b"first").unwrap();
    fs::write(f.work.join("b.txt"), b"second").unwrap();
    let directory = f.directory(&files);
    let authors = [
        write_sealed_history(
            &directory,
            1,
            "parent/task",
            &files,
            &[changed(&files[0], b"first", WriteProvenance::Rewrite)],
        ),
        write_sealed_history(
            &directory,
            2,
            "parent/task",
            &files,
            &[changed(&files[1], b"second", WriteProvenance::Rewrite)],
        ),
    ];
    let (meta, _) = publish_prepared(
        &directory,
        &authors,
        BTreeMap::from([
            (files[0].key.clone(), b"first".to_vec()),
            (files[1].key.clone(), b"second".to_vec()),
        ]),
    );
    let identities = sealed_lineage(&meta).unwrap().unwrap();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0].owner, identities[1].owner);
    assert_ne!(identities[0].overlay, identities[1].overlay);
    let inspected = ExistingEvidenceReader::open(&directory)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(inspected
        .records
        .iter()
        .any(|record| record.label == "integration-prepared"));
    // A pre-roster same-owner cohort cannot borrow a new roster: duplicate worker owners are
    // forbidden. Its immutable lineage remains readable, but it is not upgraded to settlement.
    assert!(inspected
        .limitations
        .contains(&HistoricalLimitation::MissingWorkerRoster));
    assert!(inspected.require_supported().is_err());
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"first");
    assert_eq!(fs::read(f.work.join("b.txt")).unwrap(), b"second");
}

#[test]
fn prepared_restart_recomputes_single_and_merged_candidates_from_sealed_images() {
    for multiple in [false, true] {
        for mismatch in [
            "none",
            "postimage",
            "contributors",
            "omitted",
            "extra",
            "postimage-index",
            "preimage-index",
            "missing-index",
            "postimage-missing",
            "preimage-missing",
            "preimage-descriptor",
            "empty-images",
            "extra-images",
        ] {
            let f = Fixture::new();
            fs::write(f.work.join("lib.rs"), RUST_BASE).unwrap();
            let file = f.file("lib.rs", Some(RUST_BASE), true);
            let directory = f.directory(std::slice::from_ref(&file));
            let start = RUST_BASE
                .windows(3)
                .position(|part| part == b"101")
                .unwrap();
            let mut authors = vec![write_sealed_history(
                &directory,
                1,
                "parent/a",
                std::slice::from_ref(&file),
                &[changed(
                    &file,
                    RUST_A,
                    WriteProvenance::Localized {
                        start,
                        end: start + 3,
                    },
                )],
            )];
            if multiple {
                let start = RUST_BASE
                    .windows(3)
                    .position(|part| part == b"202")
                    .unwrap();
                authors.push(write_sealed_history(
                    &directory,
                    2,
                    "parent/b",
                    std::slice::from_ref(&file),
                    &[changed(
                        &file,
                        RUST_B,
                        WriteProvenance::Localized {
                            start,
                            end: start + 3,
                        },
                    )],
                ));
            }
            write_worker_history(&directory, &authors);
            let candidates = BTreeMap::from([(
                file.key.clone(),
                if multiple { HISTORICAL_MERGED } else { RUST_A }.to_vec(),
            )]);
            let (mut value, mut images) = prepared_document(&authors, &candidates);
            match mismatch {
                "postimage" => {
                    let file = &mut value["files"][0];
                    let index = file["postimage"]["index"].as_u64().unwrap() as usize;
                    images[index] = b"invented candidate, not the sealed result".to_vec();
                    let image = serde_json::to_value(Image::bytes(&images[index])).unwrap();
                    file["after"] = image.clone();
                    file["postimage"] = image;
                    file["postimage"]["missing"] = false.into();
                    file["postimage"]["index"] = index.into();
                }
                "contributors" => value["files"][0]["contributors"] = json!([]),
                "postimage-index" => value["files"][0]["postimage"]["index"] = 0.into(),
                "preimage-index" => value["files"][0]["preimage"]["index"] = 1.into(),
                "missing-index" => {
                    value["files"][0]["postimage"]
                        .as_object_mut()
                        .unwrap()
                        .remove("index");
                }
                "postimage-missing" => value["files"][0]["postimage"]["missing"] = true.into(),
                "preimage-missing" => value["files"][0]["preimage"]["missing"] = true.into(),
                "preimage-descriptor" => {
                    value["files"][0]["preimage"]["sha256"] = json!(vec![0u8; 32])
                }
                "empty-images" => images.clear(),
                "extra-images" => images.push(b"unreferenced".to_vec()),
                "omitted" => value["files"] = json!([]),
                "extra" => {
                    let duplicate = value["files"][0].clone();
                    value["files"].as_array_mut().unwrap().push(duplicate);
                }
                _ => {}
            }
            write_record(
                &directory,
                "parent",
                "integration-prepared",
                &value,
                &images,
            );
            let inspected =
                ExistingEvidenceReader::open(&directory).and_then(|reader| reader.inspect());
            if multiple && matches!(mismatch, "none" | "postimage") {
                // A self-consistent forged multi-author candidate is unproven, not silently
                // accepted. Without the removed merger it has the same diagnostic limitation.
                let report = inspected.unwrap();
                assert!(report
                    .limitations
                    .iter()
                    .any(|limitation| matches!(limitation,
                    HistoricalLimitation::MultipleAuthors { key, .. } if key == &file.key)));
                assert!(report.require_supported().is_err());
            } else if mismatch != "none" {
                // Unsupported multi-author input must not hide actual malformed mappings,
                // contributors or image references behind an unconditional settlement refusal.
                assert!(inspected.is_err(), "corruption was only masked by unsupported history: multiple={multiple}, mismatch={mismatch}");
            } else {
                inspected.unwrap().require_supported().unwrap();
            }
            // All original corruptions still refuse. Valid nonidentical multi-author bytes are
            // now deliberately unsupported, rather than reconstructed by a hidden test merger.
            assert_eq!(
                inspect_supported(&directory).is_ok(),
                mismatch == "none" && !multiple,
                "multiple={multiple}, mismatch={mismatch}"
            );
            assert_eq!(fs::read(f.work.join("lib.rs")).unwrap(), RUST_BASE);
        }
    }
}

#[test]
fn prepared_restart_absent_preimage_mapping_is_exact() {
    for mismatch in ["none", "index", "missing", "descriptor"] {
        let f = Fixture::new();
        let file = f.file("new.txt", None, true);
        let directory = f.directory(std::slice::from_ref(&file));
        let authors = [write_sealed_history(
            &directory,
            1,
            "parent/task",
            std::slice::from_ref(&file),
            &[changed(&file, b"created", WriteProvenance::Rewrite)],
        )];
        write_worker_history(&directory, &authors);
        let (mut value, images) = prepared_document(
            &authors,
            &BTreeMap::from([(file.key.clone(), b"created".to_vec())]),
        );
        match mismatch {
            "index" => value["files"][0]["preimage"]["index"] = 0.into(),
            "missing" => value["files"][0]["preimage"]["missing"] = false.into(),
            "descriptor" => value["files"][0]["preimage"]["byte_len"] = 1.into(),
            _ => {}
        }
        write_record(
            &directory,
            "parent",
            "integration-prepared",
            &value,
            &images,
        );
        let (result, _) = inspect_with_work_budget(&directory, u64::MAX, u64::MAX);
        assert_eq!(
            result
                .and_then(EvidenceInspection::require_supported)
                .is_ok(),
            mismatch == "none",
            "{mismatch}"
        );
        assert!(!f.work.join("new.txt").exists());
    }
}

#[test]
fn inspection_history_cache_and_cumulative_work_refuse_before_next_operation() {
    for unique in [false, true] {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt"] {
            fs::write(f.work.join(name), b"abcdef").unwrap();
        }
        let files = [
            f.file("a.txt", Some(b"abcdef"), true),
            f.file("b.txt", Some(b"abcdef"), true),
        ];
        let directory = f.directory(&files);
        let mut authors = vec![write_sealed_history(
            &directory,
            1,
            "parent/a",
            &files,
            &[
                changed(
                    &files[0],
                    b"aXcdef",
                    WriteProvenance::Localized { start: 1, end: 2 },
                ),
                changed(
                    &files[0],
                    b"aYcdef",
                    WriteProvenance::Localized { start: 1, end: 2 },
                ),
            ],
        )];
        let mut candidates = BTreeMap::from([(files[0].key.clone(), b"aYcdef".to_vec())]);
        if unique {
            authors.push(write_sealed_history(
                &directory,
                2,
                "parent/b",
                &files,
                &[changed(
                    &files[1],
                    b"abZdef",
                    WriteProvenance::Localized { start: 2, end: 3 },
                )],
            ));
            candidates.insert(files[1].key.clone(), b"abZdef".to_vec());
        }
        write_worker_history(&directory, &authors);
        let (mut metadata, images) = prepared_document(&authors, &candidates);
        for nonce in 0..3 {
            metadata["inspection_test_nonce"] = nonce.into();
            write_record(
                &directory,
                "parent",
                "integration-prepared",
                &metadata,
                &images,
            );
        }
        let (result, stats) = inspect_with_work_budget(&directory, u64::MAX, u64::MAX);
        result.unwrap().require_supported().unwrap();
        assert_eq!(
            stats.histories,
            if unique { 2 } else { 1 },
            "history validation must be content-addressed, not per reference"
        );
        assert!(stats.read_bytes > 0 && stats.comparison_bytes > 0);
        // The removed engine's merge-work counter is intentionally not recreated. These are all
        // remaining real reader budgets, charged before object reads and byte comparisons.
        let (result, exact) =
            inspect_with_work_budget(&directory, stats.read_bytes, stats.comparison_bytes);
        result.unwrap().require_supported().unwrap();
        assert_eq!(exact.object_reads, stats.object_reads);
        for (read, compare) in [
            (0, u64::MAX),
            (stats.read_bytes - 1, u64::MAX),
            (u64::MAX, stats.comparison_bytes - 1),
        ] {
            let (result, refused) = inspect_with_work_budget(&directory, read, compare);
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("work budget exceeded"));
            assert!(refused.read_bytes <= read && refused.comparison_bytes <= compare);
            if read == 0 {
                assert_eq!(
                    refused.object_reads, 0,
                    "charge before even the first object read"
                );
            }
        }
        let mismatch = if unique { "observations" } else { "identity" };
        let mut invalid = metadata.clone();
        invalid["inspection_test_nonce"] = mismatch.into();
        if mismatch == "identity" {
            invalid["overlays"][0]["generation"] = 0.into();
        } else {
            invalid["observations"] = json!([]);
        }
        write_record(
            &directory,
            "parent",
            "integration-prepared",
            &invalid,
            &images,
        );
        assert!(inspect_with_work_budget(&directory, u64::MAX, u64::MAX)
            .0
            .is_err());
        for name in ["a.txt", "b.txt"] {
            assert_eq!(fs::read(f.work.join(name)).unwrap(), b"abcdef");
        }
    }
}

#[test]
fn worker_restart_crosslinks_exact_sealed_scope_generation_and_complete_selection() {
    use crate::core::speculation::verification::DurableEffectState;
    use crate::core::speculation::workers::{
        self, AttemptAdmissionClosed, AttemptClosed, AttemptMember, DriverResult, NativeWorkerKind,
        WorkerClosed, WorkerIntent, WorkerReceipt, WorkerResult, WorkerRoster,
    };

    for mismatch in ["none", "scope", "generation", "manifest", "subset"] {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt"] {
            fs::write(f.work.join(name), b"base").unwrap();
        }
        let files = [
            f.file("a.txt", Some(b"base"), true),
            f.file("b.txt", Some(b"base"), true),
        ];
        let directory = f.directory(&files);
        let mut sealed = Vec::new();
        let mut members = Vec::new();
        for index in 0..2 {
            let owner = format!("parent/task-{}/attempt/1", index + 1);
            let scope_files = if index == 0 {
                vec![files[0].clone()]
            } else {
                let mut reader = files[0].clone();
                reader.writable = false;
                vec![reader, files[1].clone()]
            };
            let closed = write_sealed_history(
                &directory,
                index + 1,
                &owner,
                &scope_files,
                &[changed(&files[index], b"private", WriteProvenance::Rewrite)],
            );
            let scope_sha256 = workers::scope_sha256(
                closed
                    .bindings
                    .iter()
                    .map(|(key, path, writable)| (key.as_str(), path.as_path(), *writable)),
            )
            .unwrap();
            members.push(AttemptMember {
                attempt: index as u32 + 1,
                owner,
                overlay: closed.identity.overlay.clone(),
                scope_sha256,
            });
            sealed.push(closed);
        }
        if mismatch == "scope" {
            members[0].scope_sha256[0] ^= 1;
        }
        let selected = if mismatch == "subset" {
            &sealed[..1]
        } else {
            &sealed[..]
        };
        let candidates = selected
            .iter()
            .enumerate()
            .map(|(index, _)| (files[index].key.clone(), b"private".to_vec()))
            .collect();
        let (prepared, images) = prepared_document(selected, &candidates);
        // Raw immutable test data may deliberately contain a corrupt prefix. No publication
        // reservation or live controller is retained merely to create historical reader fixtures.
        let roster = WorkerRoster {
            schema: 1,
            lifecycle_version: 1,
            base: FIXTURE_BASE.to_owned(),
            owner: "parent".into(),
            incarnation: "fedcba9876543210fedcba9876543210".into(),
            members,
        };
        let roster_id = write_record(&directory, "parent", workers::ROSTER_LABEL, &roster, &[]);
        for (index, member) in roster.members.iter().enumerate() {
            let intent = WorkerIntent {
                schema: 1,
                lifecycle_version: 1,
                base: FIXTURE_BASE.to_owned(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                attempt: member.attempt,
                worker: 1,
                parent_worker: None,
                kind: NativeWorkerKind::Write,
                prepared_sha256: [index as u8 + 1; 32],
            };
            let intent_id = write_record(&directory, "parent", workers::INTENT_LABEL, &intent, &[]);
            let closed = WorkerClosed {
                schema: 1,
                lifecycle_version: 1,
                base: FIXTURE_BASE.to_owned(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                intent: intent_id.clone(),
                result: WorkerResult::Returned,
                effect_state: DurableEffectState::Settled,
                ownership_released: true,
                children: Vec::new(),
            };
            let closed_id = write_record(&directory, "parent", workers::CLOSED_LABEL, &closed, &[]);
            let admission = AttemptAdmissionClosed {
                schema: 1,
                lifecycle_version: 1,
                base: FIXTURE_BASE.to_owned(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                attempt: member.attempt,
                driver_result: DriverResult::Completed,
                intents: vec![intent_id.clone()],
            };
            let admission_id = write_record(
                &directory,
                "parent",
                workers::ADMISSION_LABEL,
                &admission,
                &[],
            );
            let mut identity = sealed[index].identity.clone();
            if index == 0 {
                match mismatch {
                    "generation" => identity.generation += 1,
                    "manifest" => identity.manifest = sealed[1].identity.manifest.clone(),
                    _ => {}
                }
            }
            let attempt = AttemptClosed {
                schema: 1,
                lifecycle_version: 1,
                base: FIXTURE_BASE.to_owned(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                admission: admission_id,
                ownership_released: true,
                workers: vec![WorkerReceipt {
                    intent: intent_id,
                    closed: closed_id,
                }],
                sealed: identity,
            };
            write_record(&directory, "parent", workers::ATTEMPT_LABEL, &attempt, &[]);
        }
        write_record(
            &directory,
            "parent",
            "integration-prepared",
            &prepared,
            &images,
        );
        // Historical inspection must use immutable objects, not reinterpret a changed live tree.
        fs::write(f.work.join("a.txt"), b"foreign live bytes").unwrap();
        let inspected = inspect_supported(&directory);
        assert_eq!(
            inspected.is_ok(),
            mismatch == "none",
            "{mismatch}: {inspected:?}"
        );
        if let Err(error) = &inspected {
            let expected = match mismatch {
                "scope" => "inspected worker scope differs",
                "generation" | "manifest" => "inspected sealed lineage differs from manifest",
                "subset" => "integration omits admitted physical attempts",
                _ => unreachable!("positive case must inspect successfully"),
            };
            let message = format!("{error:#}");
            assert!(
                message.contains(expected)
                    && !message.contains("worker evidence encoding is not canonical"),
                "semantic mismatch must reach its exact guard: {mismatch}: {message}"
            );
        }
        assert_eq!(
            fs::read(f.work.join("a.txt")).unwrap(),
            b"foreign live bytes"
        );
        assert_eq!(fs::read(f.work.join("b.txt")).unwrap(), b"base");
    }
}
