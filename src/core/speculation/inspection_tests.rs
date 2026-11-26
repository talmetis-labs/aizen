use super::fixtures::*;
use super::*;
use crate::core::file_placement::tests::TestDir;
use crate::core::persist::FileFingerprint;
use crate::core::speculation::evidence::InspectedRecord;

fn classify_fixture(base: &str, records: &[InspectedRecord]) -> Result<(usize, usize, usize)> {
    classify_fixture_with_peer(base, records, false)
}
fn classify_fixture_with_peer(
    base: &str,
    records: &[InspectedRecord],
    peer: bool,
) -> Result<(usize, usize, usize)> {
    let mut complete = historical_worker_records(peer);
    complete.extend(records.iter().map(copy_record));
    classify_integration_records(base, &complete)
}

fn classify_peer_fixture(base: &str, records: &[InspectedRecord]) -> Result<(usize, usize, usize)> {
    classify_fixture_with_peer(base, records, true)
}

/// Paths only: inspection fixtures do not initialize a checkpoint store or depend on its types.
fn inspection_fixture(fixture: &TestDir) -> (PathBuf, PathBuf) {
    let root = fixture.path().join("source");
    let namespace = fixture
        .path()
        .join("repo-0123456789abcdef/worktrees/wt-0123456789abcdef");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&namespace).unwrap();
    (root, namespace)
}

#[test]
fn only_complete_cross_linked_accepted_verification_settles_integration() {
    assert_eq!(classify_fixture(BASE, &[]).unwrap(), (0, 0, 0));
    assert!(
        classify_integration_records(BASE, &[]).is_err(),
        "an existing cohort without worker evidence cannot prove zero launches"
    );
    assert!(classify_fixture(BASE, &[sealed_prepared()]).is_err());
    assert!(classify_fixture(BASE, &[sealed_prepared(), receipt("partial", 0, false)]).is_err());
    assert!(classify_fixture(
        BASE,
        &[sealed_prepared(), receipt("applied-unverified", 1, true)]
    )
    .is_err());
    for state in ["unverified", "stale", "verification_failed"] {
        assert!(classify_fixture(
            BASE,
            &[
                sealed_prepared(),
                receipt("applied-unverified", 1, true),
                verification_started(),
                verification(state, false),
            ]
        )
        .is_err());
    }
    assert_eq!(
        classify_fixture(
            BASE,
            &[
                sealed_prepared(),
                receipt("applied-unverified", 1, true),
                verification_started(),
                verification("verified", true),
            ]
        )
        .unwrap(),
        (1, 1, 0)
    );
}

#[test]
fn corrupt_future_orphan_duplicate_and_mismatched_records_never_settle() {
    let mut future = sealed_prepared();
    let mut value: serde_json::Value = serde_json::from_slice(&future.metadata).unwrap();
    value["schema"] = serde_json::json!(2);
    future.metadata = serde_json::to_vec(&value).unwrap();
    assert!(classify_fixture(BASE, &[future]).is_err());
    let mut corrupt = sealed_prepared();
    corrupt.metadata = b"not-json".to_vec();
    assert!(classify_fixture(BASE, &[corrupt]).is_err());
    assert!(classify_fixture(BASE, &[receipt("applied-unverified", 1, true)]).is_err());
    let mut wrong = verification("verified", true);
    let mut value: serde_json::Value = serde_json::from_slice(&wrong.metadata).unwrap();
    value["integration"] = serde_json::json!("other-receipt");
    wrong.metadata = serde_json::to_vec(&value).unwrap();
    assert!(classify_fixture(
        BASE,
        &[
            sealed_prepared(),
            receipt("applied-unverified", 1, true),
            verification_started(),
            wrong
        ]
    )
    .is_err());
    assert!(classify_fixture(
        BASE,
        &[
            sealed_prepared(),
            receipt("applied-unverified", 1, true),
            verification_started(),
            verification("verified", true),
            verification("verified", true),
        ]
    )
    .is_err());
    let unknown = record(
        "unknown",
        "integration-future",
        serde_json::json!({"schema":1,"base":BASE,"owner":"parent"}),
    );
    assert!(classify_fixture(BASE, &[unknown]).is_err());
    assert!(classify_fixture(
        BASE,
        &[
            sealed_prepared(),
            receipt("applied-unverified", 1, false),
            verification_started(),
            verification("verified", true),
        ]
    )
    .is_err());
}

#[test]
fn prepared_images_require_exact_bytes_descriptors_and_deterministic_mapping() {
    let reject = |prepared| {
        assert!(classify_fixture(
            BASE,
            &[
                prepared,
                receipt("applied-unverified", 1, true),
                verification_started(),
                verification("verified", true),
            ]
        )
        .is_err())
    };
    for mutate in [
        (|value: &mut serde_json::Value| {
            value["files"][0]["postimage"]["index"] = serde_json::json!(0)
        }) as fn(&mut serde_json::Value),
        |value| value["files"][0]["preimage"]["sha256"] = serde_json::json!(vec![0u8; 32]),
        |value| value["files"][0]["postimage"]["missing"] = serde_json::json!(true),
    ] {
        let mut invalid = sealed_prepared();
        change_metadata(&mut invalid, mutate);
        reject(invalid);
    }
    let mut wrong_bytes = sealed_prepared();
    wrong_bytes.images[1] = b"other".to_vec();
    reject(wrong_bytes);
    let mut missing = sealed_prepared();
    missing.images.pop();
    reject(missing);
    let mut unused = sealed_prepared();
    unused.images.push(b"unused".to_vec());
    reject(unused);

    let mut created = sealed_prepared();
    change_metadata(&mut created, |value| {
        let absent = serde_json::json!({"exists":false,"byte_len":0,"sha256":vec![0u8;32]});
        value["files"][0]["expected"] = absent.clone();
        value["files"][0]["preimage"] = absent.clone();
        value["files"][0]["preimage"]["missing"] = serde_json::json!(true);
        value["files"][0]["preimage"]["index"] = serde_json::Value::Null;
        value["files"][0]["postimage"]["index"] = serde_json::json!(0);
        value["observations"][0]["fingerprint"] = absent;
    });
    created.images.remove(0);
    assert_eq!(
        classify_fixture(
            BASE,
            &[
                copy_record(&created),
                receipt("applied-unverified", 1, true),
                verification_started(),
                verification("verified", true),
            ]
        )
        .unwrap(),
        (1, 1, 0)
    );
    change_metadata(&mut created, |value| {
        value["files"][0]["preimage"]["index"] = serde_json::json!(0)
    });
    reject(created);
}

#[test]
fn prepared_lineage_base_reads_and_receipt_file_evidence_are_cross_checked() {
    for mutate in [
        (|value: &mut serde_json::Value| {
            value["files"][0]["lineage"][0]["author"] = serde_json::json!("unknown")
        }) as fn(&mut serde_json::Value),
        |value| value["files"][0]["operation"] = serde_json::json!("rewrite"),
        |value| value["observations"] = serde_json::json!([]),
        |value| value["observations"][0]["fingerprint"] = fingerprint(b"other"),
    ] {
        let mut invalid = sealed_prepared();
        change_metadata(&mut invalid, mutate);
        assert!(classify_fixture(
            BASE,
            &[
                invalid,
                receipt("applied-unverified", 1, true),
                verification_started(),
                verification("verified", true),
            ]
        )
        .is_err());
    }
    let mut invalid_receipt = receipt("applied-unverified", 1, true);
    change_metadata(&mut invalid_receipt, |value| {
        value["files"][0]["key"] = serde_json::json!("another-file")
    });
    assert!(classify_fixture(
        BASE,
        &[
            sealed_prepared(),
            invalid_receipt,
            verification_started(),
            verification("verified", true),
        ]
    )
    .is_err());
}

#[test]
fn sealed_prepared_lineage_requires_explicit_version_and_bounded_exact_identities() {
    let validate = |record: &InspectedRecord| {
        let value = serde_json::from_slice(&record.metadata).unwrap();
        validate_prepared_evidence(BASE, "parent", &value, &record.images)
    };
    validate(&prepared()).unwrap();
    validate(&sealed_prepared()).unwrap();
    for mutate in [
        (|v: &mut serde_json::Value| {
            v.as_object_mut().unwrap().remove("overlay_lineage_version");
        }) as fn(&mut serde_json::Value),
        |v| v["overlay_lineage_version"] = serde_json::Value::Null,
        |v| v["overlay_lineage_version"] = serde_json::json!(0),
        |v| v["overlay_lineage_version"] = serde_json::json!(2),
        |v| v["overlay_lineage_version"] = serde_json::json!("1"),
        |v| {
            v["overlays"][0]
                .as_object_mut()
                .unwrap()
                .remove("generation");
        },
        |v| {
            v["overlays"][0].as_object_mut().unwrap().remove("manifest");
        },
        |v| v["overlays"][0]["generation"] = serde_json::json!(-1),
        |v| v["overlays"][0]["generation"] = serde_json::json!(1.5),
        |v| v["overlays"][0]["generation"] = serde_json::json!(513),
        |v| v["overlays"][0]["manifest"] = serde_json::json!("../m-bad.json"),
        |v| v["overlays"][0]["manifest"] = serde_json::json!(format!("m-{}.json", "A".repeat(64))),
        |v| v["overlays"][0]["manifest"] = serde_json::json!(format!("m-{}.json", "a".repeat(63))),
        |v| v["overlays"][0]["owner"] = serde_json::json!("parentx/child"),
        |v| v["overlays"][0]["unknown"] = serde_json::json!(true),
        |v| v["files"][0]["lineage"][0]["generation"] = serde_json::json!(2),
        |v| v["observations"][0]["generation"] = serde_json::json!(2),
        |v| {
            v["overlays"]
                .as_array_mut()
                .unwrap()
                .push(overlay("parent/peer", "overlay-2"))
        },
    ] {
        let mut invalid = sealed_prepared();
        change_metadata(&mut invalid, mutate);
        assert!(validate(&invalid).is_err());
    }
    for field in ["manifest", "generation"] {
        let mut unversioned = prepared();
        change_metadata(&mut unversioned, |value| {
            value["overlays"][0][field] = serde_json::json!(null)
        });
        assert!(validate(&unversioned).is_err(), "{field}");
    }
    let empty = serde_json::json!({
        "overlay_lineage_version": 1, "base": BASE, "owner": "parent", "files": [],
        "overlays": [{"base": BASE, "owner": "parent/reader", "id": "overlay-1",
            "author": "parent/reader/overlay-1", "generation": 0, "manifest": PREPARED}],
        "observations": [], "read_dependencies": [],
    });
    validate_prepared_evidence(BASE, "parent", &empty, &[]).unwrap();
}

#[test]
fn peer_read_concerns_settle_bytes_only_with_exact_concern_copy_and_review_status() {
    let mut prepared = sealed_prepared();
    let dependencies = serde_json::json!([{"observer":"parent/peer/overlay-2","key":"exact-file","peer_authors":[AUTHOR]}]);
    change_metadata(&mut prepared, |value| {
        value["overlays"]
            .as_array_mut()
            .unwrap()
            .push(sealed_peer());
        value["observations"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "author":"parent/peer/overlay-2","key":"exact-file","path":SOURCE_PATH,
                "generation":0,"fingerprint":fingerprint(b"before")
            }));
        value["read_dependencies"] = dependencies.clone();
    });
    let mut accepted = verification("verified", true);
    change_metadata(&mut accepted, |value| {
        value["read_dependencies"] = dependencies.clone();
        value["read_dependency_status"] = serde_json::json!("requires_semantic_review");
    });
    assert_eq!(
        classify_peer_fixture(
            BASE,
            &[
                copy_record(&prepared),
                receipt("applied-unverified", 1, true),
                verification_started(),
                copy_record(&accepted),
            ]
        )
        .unwrap(),
        (1, 1, 1)
    );
    for mutate in [
        (|value: &mut serde_json::Value| value["read_dependencies"] = serde_json::json!([]))
            as fn(&mut serde_json::Value),
        |value| value["read_dependency_status"] = serde_json::json!("none"),
        |value| value["read_dependency_status"] = serde_json::json!("acknowledged"),
        |value| value["applied_paths"] = serde_json::json!(["exact-file"]),
    ] {
        let mut invalid = copy_record(&accepted);
        change_metadata(&mut invalid, mutate);
        assert!(classify_peer_fixture(
            BASE,
            &[
                copy_record(&prepared),
                receipt("applied-unverified", 1, true),
                verification_started(),
                invalid,
            ]
        )
        .is_err());
    }
    change_metadata(&mut prepared, |value| {
        value["read_dependencies"] = serde_json::json!([])
    });
    assert!(classify_peer_fixture(
        BASE,
        &[
            prepared,
            receipt("applied-unverified", 1, true),
            verification_started(),
            accepted,
        ]
    )
    .is_err());
}

#[test]
fn receipt_native_record_linkage_refuses_missing_future_malformed_and_unowned_ids() {
    let prepared = sealed_prepared();
    let metadata: serde_json::Value = serde_json::from_slice(&prepared.metadata).unwrap();
    let evidence = validate_prepared_evidence(BASE, "parent", &metadata, &prepared.images).unwrap();
    let good = receipt("applied-unverified", 1, true);
    let metadata: serde_json::Value = serde_json::from_slice(&good.metadata).unwrap();
    validate_receipt_record_ids(&metadata, &evidence.files).unwrap();
    for mutate in [
        (|value: &mut serde_json::Value| {
            value
                .as_object_mut()
                .unwrap()
                .remove("native_journal_linkage");
        }) as fn(&mut serde_json::Value),
        |value| value["native_journal_linkage"] = serde_json::json!(2),
        |value| {
            value["files"][0]
                .as_object_mut()
                .unwrap()
                .remove("native_record_id");
        },
        |value| value["files"][0]["native_record_id"] = serde_json::Value::Null,
        |value| value["files"][0]["native_record_id"] = serde_json::json!(0),
        |value| value["files"][0]["native_record_id"] = serde_json::json!(-1),
        |value| value["files"][0]["native_record_id"] = serde_json::json!("1"),
        |value| value["possibly_applied_record"] = serde_json::json!(1),
    ] {
        let mut invalid = copy_record(&good);
        change_metadata(&mut invalid, mutate);
        assert!(classify_fixture(
            BASE,
            &[
                copy_record(&prepared),
                invalid,
                verification_started(),
                verification("verified", true),
            ]
        )
        .is_err());
    }

    let mut metadata = metadata;
    metadata["applied"] = serde_json::json!(0);
    metadata["files"][0]["state"] = serde_json::json!("not-applied");
    assert!(validate_receipt_record_ids(&metadata, &evidence.files).is_err());
    metadata["files"][0]["native_record_id"] = serde_json::Value::Null;
    validate_receipt_record_ids(&metadata, &evidence.files).unwrap();
    metadata["possibly_applied_record"] = serde_json::json!(7);
    metadata["files"][0]["state"] = serde_json::json!("possibly-applied");
    metadata["files"][0]["native_record_id"] = serde_json::json!(8);
    assert!(validate_receipt_record_ids(&metadata, &evidence.files).is_err());
    metadata["files"][0]["native_record_id"] = serde_json::json!(7);
    validate_receipt_record_ids(&metadata, &evidence.files).unwrap();
}

#[test]
fn receipt_native_record_linkage_preserves_order_and_never_attributes_coincident_images() {
    let prepared = prepared();
    let metadata: serde_json::Value = serde_json::from_slice(&prepared.metadata).unwrap();
    let evidence = validate_prepared_evidence(BASE, "parent", &metadata, &prepared.images).unwrap();
    let mut files = vec![evidence.files[0].clone(), evidence.files[0].clone()];
    files[1].key = "second-file".into();
    let mut receipt = serde_json::json!({
        "native_journal_linkage":1,"applied":2,"total":2,"possibly_applied_record":null,
        "files":[{"key":"exact-file","state":"applied","native_record_id":11},
            {"key":"second-file","state":"applied","native_record_id":12}]
    });
    validate_receipt_record_ids(&receipt, &files).unwrap();
    for id in [11, 10] {
        receipt["files"][1]["native_record_id"] = serde_json::json!(id);
        assert!(validate_receipt_record_ids(&receipt, &files).is_err());
    }
    files[1].postimage = files[1].preimage.clone();
    receipt["files"][1]["native_record_id"] = serde_json::json!(12);
    assert!(
        validate_receipt_record_ids(&receipt, &files).is_err(),
        "matching images never grant journal ownership"
    );
    receipt["files"][1]["native_record_id"] = serde_json::Value::Null;
    validate_receipt_record_ids(&receipt, &files).unwrap();
}

#[test]
fn covered_receipts_require_exact_prepared_identity_and_never_upgrade_legacy_ids() {
    use crate::core::native_journal::BatchIdentity;
    let coverage = BatchIdentity {
        epoch: [1; 32],
        batch: [2; 32],
    };
    let mut prepared = sealed_prepared();
    change_metadata(&mut prepared, |value| {
        value["native_coverage"] = serde_json::to_value(&coverage).unwrap()
    });
    let metadata = serde_json::from_slice(&prepared.metadata).unwrap();
    let evidence = validate_prepared_evidence(BASE, "parent", &metadata, &prepared.images).unwrap();
    assert_eq!(evidence.native_coverage, Some(coverage.clone()));
    let mut receipt = receipt("applied-unverified", 1, true);
    change_metadata(&mut receipt, |value| {
        value["native_journal_linkage"] = serde_json::json!(2);
        value["native_coverage"] = serde_json::to_value(&coverage).unwrap();
    });
    let metadata: serde_json::Value = serde_json::from_slice(&receipt.metadata).unwrap();
    validate_receipt_evidence(&metadata, &evidence).unwrap();
    // Structural validation is historical linkage only, not a read of this batch's journal.
    assert_eq!(
        classify_fixture(
            BASE,
            &[
                copy_record(&prepared),
                copy_record(&receipt),
                verification_started(),
                verification("verified", true),
            ]
        )
        .unwrap(),
        (1, 1, 0)
    );
    for mutate in [
        (|value: &mut serde_json::Value| {
            value.as_object_mut().unwrap().remove("native_coverage");
        }) as fn(&mut serde_json::Value),
        |value| value["native_coverage"] = serde_json::Value::Null,
        |value| value["native_coverage"]["epoch"] = serde_json::json!(vec![3u8; 32]),
        |value| value["native_coverage"]["batch"] = serde_json::json!(vec![3u8; 32]),
        |value| value["native_coverage"]["batch"] = serde_json::json!(vec![0u8; 32]),
        |value| value["native_coverage"]["unexpected"] = serde_json::json!(true),
        |value| value["native_journal_linkage"] = serde_json::json!(1),
        |value| value["native_journal_linkage"] = serde_json::json!(3),
    ] {
        let mut invalid = metadata.clone();
        mutate(&mut invalid);
        assert!(
            validate_receipt_evidence(&invalid, &evidence).is_err(),
            "accepted {invalid}"
        );
    }
    let legacy_prepared = empty_prepared();
    let legacy = validate_prepared_evidence(BASE, "parent", &legacy_prepared, &[]).unwrap();
    assert!(legacy.native_coverage.is_none());
    assert!(validate_receipt_coverage(&metadata, legacy.native_coverage.as_ref()).is_err());
    let mut downgraded = metadata.clone();
    downgraded["native_journal_linkage"] = serde_json::json!(1);
    downgraded
        .as_object_mut()
        .unwrap()
        .remove("native_coverage");
    assert!(validate_receipt_evidence(&downgraded, &evidence).is_err());
    assert!(validate_receipt_coverage(&downgraded, None)
        .unwrap()
        .is_none());
    for malformed in [
        serde_json::Value::Null,
        serde_json::json!({"epoch":vec![1u8;32]}),
        serde_json::json!({"epoch":vec![0u8;32],"batch":vec![2u8;32]}),
        serde_json::json!({"epoch":vec![1u8;32],"batch":vec![2u8;31]}),
        serde_json::json!({"epoch":vec![1u8;32],"batch":vec![2u8;32],"extra":true}),
    ] {
        let mut invalid = legacy_prepared.clone();
        invalid["native_coverage"] = malformed;
        assert!(validate_prepared_evidence(BASE, "parent", &invalid, &[]).is_err());
    }
}

#[test]
fn verification_lifecycle_is_order_independent_exact_and_nonterminal_until_settled_success() {
    use crate::core::speculation::verification::validate_verification_records;
    let chain = || {
        vec![
            verification("verified", true),
            receipt("applied-unverified", 1, true),
            verification_started(),
            sealed_prepared(),
        ]
    };
    validate_verification_records(BASE, &chain()).unwrap();
    assert_eq!(classify_fixture(BASE, &chain()).unwrap(), (1, 1, 0));
    let start_only = vec![
        sealed_prepared(),
        receipt("applied-unverified", 1, true),
        verification_started(),
    ];
    validate_verification_records(BASE, &start_only).unwrap();
    assert!(classify_fixture(BASE, &start_only).is_err());
    for effect in [
        serde_json::json!({"state":"unknown_worker"}),
        serde_json::json!({"state":"possibly_applied","record":null}),
        serde_json::json!({"state":"possibly_applied","record":7}),
    ] {
        let mut records = chain();
        change_metadata(&mut records[0], |value| {
            value["state"] = serde_json::json!("unverified");
            value["accepted"] = serde_json::json!(false);
            value["effect_state"] = effect;
        });
        validate_verification_records(BASE, &records).unwrap();
        assert!(classify_fixture(BASE, &records).is_err());
    }
    for mutate in [
        (|value: &mut serde_json::Value| {
            value["source_identity"] = serde_json::json!(vec![2u8; 32])
        }) as fn(&mut serde_json::Value),
        |value| value["verification_started"] = serde_json::json!(PREPARED),
        |value| value["lifecycle_version"] = serde_json::json!(2),
        |value| value["effect_state"] = serde_json::json!({"state":"settled","extra":true}),
        |value| {
            value.as_object_mut().unwrap().remove("effect_state");
        },
        |value| value["extra"] = serde_json::json!(true),
    ] {
        let mut records = chain();
        change_metadata(&mut records[0], mutate);
        assert!(validate_verification_records(BASE, &records).is_err());
        assert!(classify_fixture(BASE, &records).is_err());
    }
    let mut records = chain();
    records[2].owner = "foreign".into();
    assert!(validate_verification_records(BASE, &records).is_err());
    let mut records = chain();
    records[2].images.push(b"not permitted".to_vec());
    assert!(validate_verification_records(BASE, &records).is_err());
    let mut records = chain();
    records[0].images.push(Vec::new());
    assert!(validate_verification_records(BASE, &records).is_err());
    let mut records = chain();
    records.remove(2);
    assert!(validate_verification_records(BASE, &records).is_err());
    let mut records = chain();
    records.remove(1);
    assert!(validate_verification_records(BASE, &records).is_err());
    let mut records = chain();
    let mut duplicate = verification_started();
    duplicate.id = format!("m-{}.json", "5".repeat(64));
    records.push(duplicate);
    assert!(validate_verification_records(BASE, &records).is_err());
    let mut records = chain();
    let mut duplicate = verification("verified", true);
    duplicate.id = format!("m-{}.json", "5".repeat(64));
    records.push(duplicate);
    assert!(validate_verification_records(BASE, &records).is_err());
}

#[test]
fn worker_launch_intent_without_closure_blocks_even_a_verified_matching_tree() {
    use crate::core::speculation::workers::{
        AttemptMember, NativeWorkerKind, WorkerIntent, WorkerRoster, INTENT_LABEL, ROSTER_LABEL,
    };
    let roster = WorkerRoster {
        schema: 1,
        lifecycle_version: 1,
        base: BASE.into(),
        owner: "parent".into(),
        incarnation: "fedcba9876543210fedcba9876543210".into(),
        members: vec![AttemptMember {
            attempt: 1,
            owner: "parent/child".into(),
            overlay: "overlay-1".into(),
            scope_sha256: [1; 32],
        }],
    };
    let roster_id = format!("m-{}.json", "6".repeat(64));
    let roster_record = || InspectedRecord {
        id: roster_id.clone(),
        owner: "parent".into(),
        label: ROSTER_LABEL.into(),
        metadata: serde_json::to_vec(&roster).unwrap(),
        images: Vec::new(),
    };
    let intent = WorkerIntent {
        schema: 1,
        lifecycle_version: 1,
        base: BASE.into(),
        owner: "parent".into(),
        roster: roster_id.clone(),
        attempt: 1,
        worker: 1,
        parent_worker: None,
        kind: NativeWorkerKind::Edit,
        prepared_sha256: [2; 32],
    };
    let intent_record = || InspectedRecord {
        id: format!("m-{}.json", "7".repeat(64)),
        owner: "parent".into(),
        label: INTENT_LABEL.into(),
        metadata: serde_json::to_vec(&intent).unwrap(),
        images: Vec::new(),
    };
    assert!(classify_integration_records(BASE, &[roster_record()]).is_err());
    assert!(classify_integration_records(BASE, &[roster_record(), intent_record()]).is_err());
    let mut records = vec![
        sealed_prepared(),
        receipt("applied-unverified", 1, true),
        verification_started(),
        verification("verified", true),
    ];
    assert_eq!(classify_fixture(BASE, &records).unwrap(), (1, 1, 0));
    records.push(roster_record());
    records.push(intent_record());
    assert!(
        classify_integration_records(BASE, &records).is_err(),
        "logical success, accepted verifier and matching bytes cannot replace physical closure"
    );
}

#[test]
fn legacy_verification_is_inspectable_but_never_accepted_or_upgraded_by_start() {
    use crate::core::speculation::verification::validate_verification_records;
    let mut legacy = verification("verified", true);
    change_metadata(&mut legacy, |value| {
        for field in ["lifecycle_version", "verification_started", "effect_state"] {
            value.as_object_mut().unwrap().remove(field);
        }
    });
    let mut records = vec![
        sealed_prepared(),
        receipt("applied-unverified", 1, true),
        legacy,
    ];
    validate_verification_records(BASE, &records).unwrap();
    assert!(classify_fixture(BASE, &records).is_err());
    records.push(verification_started());
    assert!(
        validate_verification_records(BASE, &records).is_err(),
        "legacy cannot borrow a later lifecycle start"
    );
}

#[test]
fn recovery_selection_inspects_start_only_and_unknown_verifier_without_settling_or_writing() {
    let fixture = TestDir::new();
    let (_root, namespace) = inspection_fixture(&fixture);
    let directory = cohort(&namespace);
    let prepared_id = durable_record(&directory, BASE, "integration-prepared", empty_prepared());
    let receipt_id = durable_record(
        &directory,
        BASE,
        "integration-receipt",
        serde_json::json!({
            "schema":1,"native_journal_linkage":1,"base":BASE,"owner":"parent","state":"applied-unverified",
            "prepared":prepared_id,"applied":0,"total":0,"postimage_verified":true,"files":[],
            "possibly_applied_record":null,"stop":null
        }),
    );
    let started = durable_record(
        &directory,
        BASE,
        "integration-verification-started",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"state":"started","base":BASE,"owner":"parent",
            "prepared":prepared_id,"integration":receipt_id,"source_identity":null
        }),
    );
    let before = recovery_inventory_digest(&directory).unwrap();
    read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert!(inspect_speculation_namespace(&namespace).is_err());
    assert_eq!(before, recovery_inventory_digest(&directory).unwrap());
    durable_record(
        &directory,
        BASE,
        "integration-verification",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"state":"unverified","base":BASE,"owner":"parent",
            "prepared":prepared_id,"integration":receipt_id,"verification_started":started,
            "source_identity":null,"command":null,"accepted":false,"effect_state":{"state":"unknown_worker"},
            "read_dependencies":[],"read_dependency_status":"none","applied_paths":[],"scope":"fixture evidence only"
        }),
    );
    let after = recovery_inventory_digest(&directory).unwrap();
    read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert!(inspect_speculation_namespace(&namespace).is_err());
    assert_eq!(after, recovery_inventory_digest(&directory).unwrap());
    assert!(
        !namespace.join("ledger.json").exists()
            && !namespace.join("journal.json").exists()
            && !fixture
                .path()
                .join("repo-0123456789abcdef/store.git")
                .exists()
    );
}

#[test]
fn recovery_image_classification_distinguishes_missing_empty_and_coincident_images() {
    let missing = FileFingerprint::missing();
    let empty = FileFingerprint::for_bytes(b"");
    let after = FileFingerprint::for_bytes(b"after");
    assert_eq!(
        recovery_image_class(&missing, &missing, &empty),
        RecoveryImageClass::Pre
    );
    assert_eq!(
        recovery_image_class(&empty, &missing, &empty),
        RecoveryImageClass::Post
    );
    assert_eq!(
        recovery_image_class(&after, &missing, &empty),
        RecoveryImageClass::Foreign
    );
    assert_eq!(
        recovery_image_class(&empty, &empty, &empty),
        RecoveryImageClass::SamePreAndPost
    );
}

#[test]
fn recovery_owner_claim_is_nonblocking_and_never_enters_body_while_owned() {
    let fixture = TestDir::new();
    let (_root, namespace) = inspection_fixture(&fixture);
    let directory = cohort(&namespace);
    let before = recovery_inventory_digest(&directory).unwrap();
    let owner = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &directory.join("owner.lock"),
        Duration::ZERO,
    )
    .unwrap();
    let entered = std::cell::Cell::new(false);
    let start = std::time::Instant::now();
    assert!(with_recovery_owner_claim(&directory, || {
        entered.set(true);
        Ok(())
    })
    .is_err());
    assert!(!entered.get());
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(owner);
    with_recovery_owner_claim(&directory, || {
        entered.set(true);
        Ok(())
    })
    .unwrap();
    assert!(entered.get());
    assert_eq!(before, recovery_inventory_digest(&directory).unwrap());
}

#[test]
fn recovery_owner_claim_rejects_same_length_marker_change_after_selection() {
    let fixture = TestDir::new();
    let (_root, namespace) = inspection_fixture(&fixture);
    let directory = cohort(&namespace);
    let prepared = durable_record(&directory, BASE, "integration-prepared", empty_prepared());
    let (inventory, _, _, _) = read_recovery_selection(&directory, BASE, &prepared).unwrap();
    let mut changed = b"aizen-speculation-owner-v1\n".to_vec();
    changed[0] = b'x';
    fs::write(directory.join("owner.lock"), &changed).unwrap();
    // The Windows-safe inventory deliberately cannot read this marker through a second
    // descriptor. The exclusive owner claim must reject its actual bytes before entering.
    assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
    let entered = std::cell::Cell::new(false);
    let error = with_recovery_owner_claim(&directory, || {
        entered.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported restart owner marker"));
    assert!(!entered.get());
    assert_eq!(fs::read(directory.join("owner.lock")).unwrap(), changed);
    assert!(!namespace.join("ledger.json").exists());
    assert!(!namespace.join("journal.json").exists());
    assert!(!fixture
        .path()
        .join("repo-0123456789abcdef/store.git")
        .exists());
}

#[test]
fn recovery_selection_rejects_stale_unknown_and_arbitrary_paths_without_writes() {
    let fixture = TestDir::new();
    let (_root, namespace) = inspection_fixture(&fixture);
    let directory = cohort(&namespace);
    let record = prepared();
    let prepared = durable_record_images(
        &directory,
        BASE,
        "integration-prepared",
        serde_json::from_slice(&record.metadata).unwrap(),
        &record.images,
    );
    let (before, files, native_receipt, limitations) =
        read_recovery_selection(&directory, BASE, &prepared).unwrap();
    assert_eq!(files.len(), 1);
    assert!(limitations
        .contains(&crate::core::speculation::evidence::HistoricalLimitation::MissingWorkerRoster));
    assert!(
        native_receipt.is_none(),
        "a missing receipt is retained as missing proof"
    );
    assert!(recovery_cohort_directory(&namespace, "../outside").is_err());
    let orphan = fingerprint(b"retained orphan blob");
    let hash = orphan["sha256"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| format!("{:02x}", byte.as_u64().unwrap()))
        .collect::<String>();
    fs::write(directory.join(format!("b-{hash}")), b"retained orphan blob").unwrap();
    assert_ne!(before, recovery_inventory_digest(&directory).unwrap());
    let snapshot = recovery_inventory_digest(&directory).unwrap();
    durable_record(
        &directory,
        BASE,
        "integration-future",
        serde_json::json!({"schema":1,"base":BASE,"owner":"parent"}),
    );
    assert!(read_recovery_selection(&directory, BASE, &prepared).is_err());
    assert_ne!(snapshot, recovery_inventory_digest(&directory).unwrap());
    assert!(!namespace.join("ledger.json").exists());
    assert!(!namespace.join("journal.json").exists());
    assert!(!fixture
        .path()
        .join("repo-0123456789abcdef/store.git")
        .exists());
}

fn recovery_selection_for_resources(
    root: &Path,
    resources: &[crate::core::workspace_txn::NativeResource],
) -> SpeculationRecoverySelection {
    SpeculationRecoverySelection {
        identity: crate::core::workspace_txn::WorkspaceIdentity {
            repo_id: "fixture-repo".into(),
            worktree_id: "fixture-worktree".into(),
            canonical_root: root.to_path_buf(),
            canonical_git_dir: None,
            canonical_common_git_dir: None,
        },
        namespace: root.parent().unwrap().join("metadata"),
        cohort: BASE.into(),
        prepared: PREPARED.into(),
        inventory_sha256: [1u8; 32],
        native_receipt: None,
        limitations: vec![
            crate::core::speculation::evidence::HistoricalLimitation::MissingWorkerRoster,
        ],
        owner: "parent".into(),
        files: resources
            .iter()
            .map(|resource| PreparedFileEvidence {
                key: resource.key().to_owned(),
                path: resource.path().to_str().unwrap().to_owned(),
                preimage: FileFingerprint::for_bytes(b"before"),
                postimage: FileFingerprint::for_bytes(b"after"),
            })
            .collect(),
    }
}

#[test]
fn recovery_resource_report_refuses_foreign_mixed_and_unapproved_evidence_before_effect() {
    let fixture = TestDir::new();
    let root = fixture.path().join("source");
    fs::create_dir(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let a = root.join("a.txt");
    let b = root.join("b.txt");
    fs::write(&a, b"before").unwrap();
    fs::write(&b, b"before").unwrap();
    let resources = [
        crate::core::workspace_txn::NativeResource::resolve(&a).unwrap(),
        crate::core::workspace_txn::NativeResource::resolve(&b).unwrap(),
    ];
    let selection = recovery_selection_for_resources(&root, &resources);
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        let locked = acquire_recovery_resources(&root, &selection, &resources).unwrap();
        let path = crate::core::workspace_txn::resource_lock("native-file", resources[0].key());
        assert!(
            crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&path, Duration::ZERO).is_err()
        );
        assert!(classify_locked_recovery_resources(&selection, &locked)
            .unwrap()
            .iter()
            .all(|file| file.class == RecoveryImageClass::Pre));
        drop(locked);
        let before = classify_recovery_resources(&root, &selection, &resources).unwrap();
        assert!(before
            .iter()
            .all(|file| file.class == RecoveryImageClass::Pre));
        fs::write(&a, b"foreign").unwrap();
        assert!(classify_recovery_resources(&root, &selection, &resources).is_err());
        assert_eq!(fs::read(&a).unwrap(), b"foreign");
        fs::write(&a, b"after").unwrap();
        assert!(classify_recovery_resources(&root, &selection, &resources).is_err());
        assert_eq!(fs::read(&a).unwrap(), b"after");
        assert_eq!(fs::read(&b).unwrap(), b"before");
        fs::write(&b, b"after").unwrap();
        assert!(classify_recovery_resources(&root, &selection, &resources)
            .unwrap()
            .iter()
            .all(|file| file.class == RecoveryImageClass::Post));
        assert!(classify_recovery_resources(&root, &selection, &resources[..1]).is_err());
        let mut untrusted = selection.clone();
        untrusted.files[0].path = fixture
            .path()
            .join("never-opened.txt")
            .to_string_lossy()
            .into_owned();
        assert!(classify_recovery_resources(&root, &untrusted, &resources).is_err());
        assert!(!fixture.path().join("never-opened.txt").exists());
    });
}

#[test]
fn recovery_native_receipt_joins_exact_journal_record_and_refuses_mismatches_without_writes() {
    use crate::core::{native_journal, workspace_txn::NativeResource};
    let fixture = TestDir::new();
    let (root, namespace) = inspection_fixture(&fixture);
    let root = fs::canonicalize(&root).unwrap();
    let source = root.join("source.txt");
    let unrelated = root.join("unrelated.txt");
    fs::write(&source, b"before").unwrap();
    fs::write(&unrelated, b"before").unwrap();
    let home = fixture.path().join("private-home");
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        native_journal::with_test_storage(&home, || {
            assert_eq!(
                apply_recovery_fixture_write(&root, &unrelated, "other-owner"),
                1
            );
            let id = apply_recovery_fixture_write(&root, &source, "parent");
            assert_eq!(id, 2, "receipt IDs must not be inferred from file order");
            let resources = [NativeResource::resolve(&source).unwrap()];
            let directory = cohort(&namespace);
            let prepared =
                durable_recovery_receipt_for_resource(&directory, &resources[0], id, false);
            let (inventory, files, native_receipt, limitations) =
                read_recovery_selection(&directory, BASE, &prepared).unwrap();
            let mut selection = recovery_selection_for_resources(&root, &resources);
            selection.files = files;
            selection.native_receipt = native_receipt;
            selection.limitations = limitations;
            let journal_path = home.join("native-journal-v1/records.bin");
            let journal = fs::read(&journal_path).unwrap();
            let locked = acquire_recovery_resources(&root, &selection, &resources).unwrap();
            let proof = inspect_recovery_native_records(&selection, &locked).unwrap();
            assert_eq!(
                proof,
                vec![native_journal::RecordProof {
                    id,
                    status: native_journal::RecordProofStatus::SettledApplied
                }]
            );
            for mutate in [
                (|value: &mut SpeculationRecoverySelection| {
                    value.native_receipt.as_mut().unwrap().records[0].id = 1
                }) as fn(&mut SpeculationRecoverySelection),
                |value| value.native_receipt.as_mut().unwrap().records[0].id = 999,
                |value| value.native_receipt.as_mut().unwrap().owner = "other-owner".into(),
                |value| value.files[0].preimage = FileFingerprint::missing(),
                |value| value.files[0].postimage = FileFingerprint::for_bytes(b"wrong"),
                |value| value.native_receipt.as_mut().unwrap().records[0].file = 1,
            ] {
                let mut invalid = selection.clone();
                mutate(&mut invalid);
                assert!(inspect_recovery_native_records(&invalid, &locked).is_err());
            }
            selection.native_receipt = None;
            assert!(
                inspect_recovery_native_records(&selection, &locked)
                    .unwrap()
                    .is_empty(),
                "missing receipt remains lack of proof"
            );
            assert_eq!(fs::read(&source).unwrap(), b"after");
            assert_eq!(fs::read(&unrelated).unwrap(), b"after");
            assert_eq!(fs::read(&journal_path).unwrap(), journal);
            assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
            assert!(inspect_speculation_namespace(&namespace).is_err());
        });
    });
}

#[test]
fn recovery_covered_receipt_checks_complete_fence_not_just_selected_numeric_ids() {
    use crate::core::{
        exec_ctx, native_journal,
        repo_lock::LockMode,
        workspace_txn::{NativeResource, NativeResourceLease},
    };
    for omitted_intent in [false, true] {
        let fixture = TestDir::new();
        let (root, namespace) = inspection_fixture(&fixture);
        let root = fs::canonicalize(&root).unwrap();
        for name in ["source.txt", "second.txt"] {
            fs::write(root.join(name), b"before").unwrap();
        }
        let resources = [
            NativeResource::resolve(&root.join("source.txt")).unwrap(),
            NativeResource::resolve(&root.join("second.txt")).unwrap(),
        ];
        let home = fixture.path().join("private-home");
        crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
            native_journal::with_test_storage(&home, || {
                let (coverage, id) = exec_ctx::with_current(
                    exec_ctx::ExecutionContext::default().with_resource_scope("parent"),
                    || {
                        let selected: Vec<_> = resources
                            .iter()
                            .map(|resource| (resource.clone(), LockMode::Exclusive))
                            .collect();
                        let _lease = NativeResourceLease::acquire(
                            &root,
                            &selected,
                            Duration::ZERO,
                            None,
                            "parent",
                        )
                        .unwrap();
                        let before = FileFingerprint::for_bytes(b"before");
                        let count = if omitted_intent { 2 } else { 1 };
                        let writes: Vec<_> = resources[..count]
                            .iter()
                            .map(|resource| (resource, &before, b"after".as_slice()))
                            .collect();
                        let mut batch =
                            native_journal::begin_covered_batch("parent", &writes).unwrap();
                        let coverage = batch.coverage_identity().unwrap().clone();
                        let id = batch
                            .apply_with_record(&resources[0], &before, b"after")
                            .unwrap()
                            .native_record_id
                            .unwrap();
                        (coverage, id)
                    },
                );
                let directory = cohort(&namespace);
                let prepared = durable_recovery_evidence(
                    &directory,
                    &resources[0],
                    id,
                    false,
                    Some(&coverage),
                );
                let (inventory, files, native_receipt, limitations) =
                    read_recovery_selection(&directory, BASE, &prepared).unwrap();
                let mut selection = recovery_selection_for_resources(&root, &resources[..1]);
                selection.files = files;
                selection.native_receipt = native_receipt;
                selection.limitations = limitations;
                assert_eq!(
                    selection.native_receipt.as_ref().unwrap().coverage.as_ref(),
                    Some(&coverage)
                );
                let locked =
                    acquire_recovery_resources(&root, &selection, &resources[..1]).unwrap();
                let journal_path = home.join("native-journal-v1/records.bin");
                let journal = fs::read(&journal_path).unwrap();
                if omitted_intent {
                    assert!(
                        inspect_recovery_native_records(&selection, &locked).is_err(),
                        "an exact applied first ID must not cover an omitted second intent"
                    );
                } else {
                    assert_eq!(
                        inspect_recovery_native_records(&selection, &locked).unwrap(),
                        vec![native_journal::RecordProof {
                            id,
                            status: native_journal::RecordProofStatus::SettledApplied
                        }]
                    );
                }
                for mutate in [
                    (|receipt: &mut RecoveryNativeReceipt| receipt.records.clear())
                        as fn(&mut RecoveryNativeReceipt),
                    |receipt| receipt.coverage.as_mut().unwrap().epoch[0] ^= 1,
                    |receipt| receipt.coverage.as_mut().unwrap().batch[0] ^= 1,
                ] {
                    let mut invalid = selection.clone();
                    mutate(invalid.native_receipt.as_mut().unwrap());
                    assert!(inspect_recovery_native_records(&invalid, &locked).is_err());
                }
                assert_eq!(fs::read(&journal_path).unwrap(), journal);
                assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
                assert_eq!(fs::read(root.join("source.txt")).unwrap(), b"after");
                assert_eq!(fs::read(root.join("second.txt")).unwrap(), b"before");
                assert!(inspect_speculation_namespace(&namespace).is_err());
            });
        });
    }
}

#[test]
fn recovery_empty_covered_receipt_still_checks_the_existing_fence() {
    use crate::core::{exec_ctx, native_journal};
    let fixture = TestDir::new();
    let (root, namespace) = inspection_fixture(&fixture);
    let root = fs::canonicalize(&root).unwrap();
    let home = fixture.path().join("private-home");
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        native_journal::with_test_storage(&home, || {
            let coverage = exec_ctx::with_current(
                exec_ctx::ExecutionContext::default().with_resource_scope("parent"),
                || {
                    let batch = native_journal::begin_covered_batch("parent", &[]).unwrap();
                    batch.coverage_identity().unwrap().clone()
                },
            );
            let directory = cohort(&namespace);
            let mut prepared_value = empty_prepared();
            prepared_value["native_coverage"] = serde_json::to_value(&coverage).unwrap();
            let prepared = durable_record(&directory, BASE, "integration-prepared", prepared_value);
            durable_record(
                &directory,
                BASE,
                "integration-receipt",
                serde_json::json!({
                    "schema":1,"native_journal_linkage":2,"native_coverage":coverage,"base":BASE,"owner":"parent",
                    "state":"applied-unverified","prepared":prepared,"applied":0,"total":0,
                    "postimage_verified":true,"files":[],"possibly_applied_record":null,"stop":null
                }),
            );
            let (_, files, native_receipt, limitations) =
                read_recovery_selection(&directory, BASE, &prepared).unwrap();
            let mut selection = recovery_selection_for_resources(&root, &[]);
            selection.files = files;
            selection.native_receipt = native_receipt;
            selection.limitations = limitations;
            let locked = acquire_recovery_resources(&root, &selection, &[]).unwrap();
            assert!(inspect_recovery_native_records(&selection, &locked)
                .unwrap()
                .is_empty());
            selection
                .native_receipt
                .as_mut()
                .unwrap()
                .coverage
                .as_mut()
                .unwrap()
                .batch[0] ^= 1;
            assert!(
                inspect_recovery_native_records(&selection, &locked).is_err(),
                "empty claims cannot bypass fence lookup"
            );
            assert!(inspect_speculation_namespace(&namespace).is_err());
        });
    });
}

#[test]
fn recovery_possible_native_record_stays_unresolved_and_cannot_certify_applied_receipt() {
    use crate::core::{
        exec_ctx, native_journal,
        repo_lock::LockMode,
        workspace_txn::{NativeResource, NativeResourceLease},
    };
    for mutation_started in [false, true] {
        let fixture = TestDir::new();
        let (root, namespace) = inspection_fixture(&fixture);
        let root = fs::canonicalize(&root).unwrap();
        let source = root.join("source.txt");
        fs::write(&source, b"before").unwrap();
        let home = fixture.path().join("private-home");
        crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
            native_journal::with_test_storage(&home, || {
                let resource = NativeResource::resolve(&source).unwrap();
                let error = exec_ctx::with_current(
                    exec_ctx::ExecutionContext::default().with_resource_scope("parent"),
                    || {
                        let _lease = NativeResourceLease::acquire(
                            &root,
                            &[(resource.clone(), LockMode::Exclusive)],
                            Duration::ZERO,
                            None,
                            "parent",
                        )
                        .unwrap();
                        let apply = || {
                            native_journal::apply(
                                &resource,
                                &FileFingerprint::for_bytes(b"before"),
                                b"after",
                                "parent",
                            )
                        };
                        if mutation_started {
                            native_journal::with_test_after_write_failure(apply)
                        } else {
                            native_journal::with_test_prepared_failure(apply)
                        }
                        .unwrap_err()
                    },
                );
                let id = error
                    .downcast_ref::<native_journal::PossiblyApplied>()
                    .unwrap()
                    .record_id;
                let resources = [NativeResource::resolve(&source).unwrap()];
                let directory = cohort(&namespace);
                let prepared =
                    durable_recovery_receipt_for_resource(&directory, &resources[0], id, true);
                let (inventory, files, native_receipt, limitations) =
                    read_recovery_selection(&directory, BASE, &prepared).unwrap();
                let mut selection = recovery_selection_for_resources(&root, &resources);
                selection.files = files;
                selection.native_receipt = native_receipt;
                selection.limitations = limitations;
                let journal_path = home.join("native-journal-v1/records.bin");
                let journal = fs::read(&journal_path).unwrap();
                let locked = acquire_recovery_resources(&root, &selection, &resources).unwrap();
                assert_eq!(
                    inspect_recovery_native_records(&selection, &locked).unwrap(),
                    vec![native_journal::RecordProof {
                        id,
                        status: native_journal::RecordProofStatus::Prepared
                    }]
                );
                selection.native_receipt.as_mut().unwrap().records[0].applied = true;
                assert!(inspect_recovery_native_records(&selection, &locked).is_err());
                let expected_class = if mutation_started {
                    RecoveryImageClass::Post
                } else {
                    RecoveryImageClass::Pre
                };
                assert_eq!(
                    classify_locked_recovery_resources(&selection, &locked).unwrap()[0].class,
                    expected_class
                );
                assert_eq!(
                    fs::read(&source).unwrap(),
                    if mutation_started {
                        b"after".as_slice()
                    } else {
                        b"before".as_slice()
                    }
                );
                assert_eq!(
                    fs::read(&journal_path).unwrap(),
                    journal,
                    "inspection must not reconcile a Prepared record"
                );
                assert_eq!(recovery_inventory_digest(&directory).unwrap(), inventory);
                assert!(inspect_speculation_namespace(&namespace).is_err());
            });
        });
    }
}

#[test]
fn recovery_report_selection_validates_original_spelling_and_exact_resources() {
    let fixture = TestDir::new();
    let root = fixture.path().join("source");
    fs::create_dir(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    fs::write(root.join("a.txt"), b"before").unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("directory")).unwrap();
    let selected =
        resolve_recovery_report_resources(&root, &["a.txt".into(), "missing.txt".into()]).unwrap();
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].path(), root.join("a.txt"));
    for paths in [
        vec!["../outside.txt".into()],
        vec!["./a.txt".into()],
        vec!["*.txt".into()],
        vec!["a.txt".into(), "a.txt".into()],
        vec![".git/config".into()],
        vec!["directory".into()],
        vec![fixture.path().join("outside.txt").to_str().unwrap().into()],
    ] {
        assert!(resolve_recovery_report_resources(&root, &paths).is_err());
    }
    assert!(!root.join("missing.txt").exists());
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"before");
}

#[test]
fn recovery_observation_is_historical_and_never_replaces_receipt_or_verification() {
    let prepared_id = format!("m-{}.json", "b".repeat(64));
    let receipt_id = format!("m-{}.json", "c".repeat(64));
    let mut prepared = sealed_prepared();
    prepared.id = prepared_id.clone();
    let observation = historical_inspection(&prepared_id, None);
    let mut records = vec![prepared, observation];
    validate_inspection_observation(BASE, &records[1], &records).unwrap();
    assert!(classify_fixture(BASE, &records).is_err());
    let mut receipt = receipt("applied-unverified", 1, true);
    receipt.id = receipt_id.clone();
    change_metadata(&mut receipt, |value| {
        value["prepared"] = serde_json::json!(prepared_id)
    });
    records.push(receipt);
    // A no-receipt observation remains a truthful historical record after a later receipt arrives.
    validate_inspection_observation(BASE, &records[1], &records).unwrap();
    assert!(classify_fixture(BASE, &records).is_err());
    records.push(historical_inspection(&prepared_id, Some(&receipt_id)));
    validate_inspection_observation(BASE, &records[3], &records).unwrap();
    assert!(classify_fixture(BASE, &records).is_err());
    let mut started = verification_started();
    change_metadata(&mut started, |value| {
        value["prepared"] = serde_json::json!(prepared_id);
        value["integration"] = serde_json::json!(receipt_id);
    });
    records.push(started);
    let mut verified = verification("verified", true);
    change_metadata(&mut verified, |value| {
        value["prepared"] = serde_json::json!(prepared_id);
        value["integration"] = serde_json::json!(receipt_id);
    });
    records.push(verified);
    assert_eq!(classify_fixture(BASE, &records).unwrap(), (1, 1, 0));
}

#[test]
fn recovery_observation_rejects_foreign_linkage_images_classes_and_native_proofs() {
    let prepared_id = format!("m-{}.json", "b".repeat(64));
    let receipt_id = format!("m-{}.json", "c".repeat(64));
    let mut prepared = prepared();
    prepared.id = prepared_id.clone();
    let mut receipt = receipt("applied-unverified", 1, true);
    receipt.id = receipt_id.clone();
    change_metadata(&mut receipt, |value| {
        value["prepared"] = serde_json::json!(prepared_id)
    });
    let valid = historical_inspection(&prepared_id, Some(&receipt_id));
    let validate = |observation: InspectedRecord| {
        let records = [copy_record(&prepared), copy_record(&receipt), observation];
        validate_inspection_observation(BASE, &records[2], &records)
    };
    validate(copy_record(&valid)).unwrap();
    for mutate in [
        (|value: &mut serde_json::Value| value["owner"] = serde_json::json!("other-owner"))
            as fn(&mut serde_json::Value),
        |value| value["observer"] = serde_json::json!("other-observer"),
        |value| value["base"] = serde_json::json!("f".repeat(32)),
        |value| value["prepared"] = serde_json::json!(format!("m-{}.json", "d".repeat(64))),
        |value| value["receipt"] = serde_json::json!(format!("m-{}.json", "e".repeat(64))),
        |value| value["files"][0]["key"] = serde_json::json!("other-key"),
        |value| value["files"][0]["class"] = serde_json::json!("post"),
        |value| value["files"][0]["fingerprint"] = fingerprint(b"foreign"),
        |value| value["files"] = serde_json::json!([]),
        |value| value["native_records"] = serde_json::json!([]),
        |value| value["native_records"][0]["id"] = serde_json::json!(2),
        |value| value["native_records"][0]["status"] = serde_json::json!("prepared"),
        |value| {
            value["native_records"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"id":2,"status":"settled_applied"}))
        },
    ] {
        let mut invalid = copy_record(&valid);
        change_metadata(&mut invalid, mutate);
        assert!(validate(invalid).is_err());
    }
    let mut invalid = copy_record(&valid);
    invalid.images.push(b"unlinked image".to_vec());
    assert!(validate(invalid).is_err());
    let mut invalid = copy_record(&valid);
    invalid.owner = "other-envelope-owner".into();
    assert!(validate(invalid).is_err());
    let mut duplicated = vec![
        copy_record(&prepared),
        copy_record(&prepared),
        copy_record(&receipt),
        copy_record(&valid),
    ];
    assert!(validate_inspection_observation(BASE, &duplicated[3], &duplicated).is_err());
    duplicated.remove(0);
    duplicated.insert(0, copy_record(&receipt));
    assert!(validate_inspection_observation(BASE, &duplicated[3], &duplicated).is_err());
}

#[test]
fn recovery_observation_selection_retains_no_receipt_history_after_later_receipt() {
    let fixture = TestDir::new();
    let (_root, namespace) = inspection_fixture(&fixture);
    let directory = cohort(&namespace);
    let prepared_record = prepared();
    let prepared_id = durable_record_images(
        &directory,
        BASE,
        "integration-prepared",
        serde_json::from_slice(&prepared_record.metadata).unwrap(),
        &prepared_record.images,
    );
    let observation = historical_inspection(&prepared_id, None);
    durable_owned_record_images(
        &directory,
        BASE,
        &observation.owner,
        &observation.label,
        serde_json::from_slice(&observation.metadata).unwrap(),
        &[],
    );
    let (_, _, selected_receipt, _) =
        read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert!(selected_receipt.is_none());
    assert!(inspect_speculation_namespace(&namespace).is_err());
    let mut receipt_record = receipt("applied-unverified", 1, true);
    change_metadata(&mut receipt_record, |value| {
        value["prepared"] = serde_json::json!(prepared_id)
    });
    let receipt_id = durable_record(
        &directory,
        BASE,
        "integration-receipt",
        serde_json::from_slice(&receipt_record.metadata).unwrap(),
    );
    let (_, _, selected_receipt, _) =
        read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert_eq!(selected_receipt.unwrap().id, receipt_id);
    let observation = historical_inspection(&prepared_id, Some(&receipt_id));
    durable_owned_record_images(
        &directory,
        BASE,
        &observation.owner,
        &observation.label,
        serde_json::from_slice(&observation.metadata).unwrap(),
        &[],
    );
    read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert!(inspect_speculation_namespace(&namespace).is_err());
    let mut started = verification_started();
    change_metadata(&mut started, |value| {
        value["prepared"] = serde_json::json!(prepared_id);
        value["integration"] = serde_json::json!(receipt_id);
    });
    let started_id = durable_record(
        &directory,
        BASE,
        "integration-verification-started",
        serde_json::from_slice(&started.metadata).unwrap(),
    );
    let mut verified = verification("verified", true);
    change_metadata(&mut verified, |value| {
        value["prepared"] = serde_json::json!(prepared_id);
        value["integration"] = serde_json::json!(receipt_id);
        value["verification_started"] = serde_json::json!(started_id);
    });
    durable_record(
        &directory,
        BASE,
        "integration-verification",
        serde_json::from_slice(&verified.metadata).unwrap(),
    );
    read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert!(
        inspect_speculation_namespace(&namespace).is_err(),
        "legacy missing sealed/worker lineage remains unresolved even after accepted verification"
    );
    assert!(
        !namespace.join("ledger.json").exists()
            && !namespace.join("journal.json").exists()
            && !fixture
                .path()
                .join("repo-0123456789abcdef/store.git")
                .exists()
    );
}

#[test]
fn missing_legacy_lineage_or_roster_is_readable_data_but_never_settlement() {
    let records = vec![
        prepared(),
        receipt("applied-unverified", 1, true),
        verification_started(),
        verification("verified", true),
    ];
    crate::core::speculation::verification::validate_verification_records(BASE, &records).unwrap();
    assert!(classify_integration_records(BASE, &records).is_err());
    assert!(
        classify_fixture(BASE, &records).is_err(),
        "a roster cannot upgrade missing immutable lineage"
    );
    let sealed = vec![
        sealed_prepared(),
        receipt("applied-unverified", 1, true),
        verification_started(),
        verification("verified", true),
    ];
    assert!(
        classify_integration_records(BASE, &sealed).is_err(),
        "sealed bytes cannot replace worker closure"
    );
    assert_eq!(classify_fixture(BASE, &sealed).unwrap(), (1, 1, 0));
}
