use super::*;
use crate::core::file_placement::tests::TestDir;
use crate::core::persist::FileFingerprint;
use crate::core::speculation::store::InspectedRecord;

const BASE: &str = "0123456789abcdef0123456789abcdef";
const PREPARED: &str = "m-1111111111111111111111111111111111111111111111111111111111111111.json";
const RECEIPT: &str = "m-2222222222222222222222222222222222222222222222222222222222222222.json";
const VERIFY_START: &str =
    "m-3333333333333333333333333333333333333333333333333333333333333333.json";
const VERIFY_FINISH: &str =
    "m-4444444444444444444444444444444444444444444444444444444444444444.json";

fn fingerprint(bytes: &[u8]) -> serde_json::Value {
    let fp = FileFingerprint::for_bytes(bytes);
    serde_json::json!({"exists":fp.exists,"byte_len":fp.byte_len,"sha256":fp.sha256})
}

fn record(id: &str, label: &str, value: serde_json::Value) -> InspectedRecord {
    InspectedRecord {
        id: id.into(),
        owner: "parent".into(),
        label: label.into(),
        metadata: serde_json::to_vec(&value).unwrap(),
        images: Vec::new(),
    }
}

const AUTHOR: &str = "parent/child/overlay-one";
const SOURCE_PATH: &str = "evidence-only/source.txt";

fn overlay(owner: &str, id: &str) -> serde_json::Value {
    serde_json::json!({"base":BASE,"owner":owner,"id":id,"author":format!("{owner}/{id}")})
}

fn image_descriptor(bytes: &[u8], index: usize) -> serde_json::Value {
    let mut value = fingerprint(bytes);
    value["index"] = serde_json::json!(index);
    value["missing"] = serde_json::json!(false);
    value
}

fn empty_prepared() -> serde_json::Value {
    serde_json::json!({
        "schema":1,"base":BASE,"owner":"parent","state":"prepared",
        "overlays":[overlay("parent/child", "overlay-one")],
        "observations":[],"read_dependencies":[],"files":[]
    })
}

fn prepared() -> InspectedRecord {
    let mut prepared = record(
        PREPARED,
        "integration-prepared",
        serde_json::json!({
            "schema":1,"base":BASE,"owner":"parent","state":"prepared",
            "overlays":[overlay("parent/child", "overlay-one")],
            "observations":[{"author":AUTHOR,"key":"exact-file","path":SOURCE_PATH,
                "generation":0,"fingerprint":fingerprint(b"before")}],
            "read_dependencies":[],
            "files":[{"key":"exact-file","path":SOURCE_PATH,
                "expected":fingerprint(b"before"),"after":fingerprint(b"after"),
                "preimage":image_descriptor(b"before", 0),"postimage":image_descriptor(b"after", 1),
                "contributors":[AUTHOR],"operation":"localized",
                "lineage":[{"author":AUTHOR,"generation":1,"operation":"localized"}]}]
        }),
    );
    prepared.images = vec![b"before".to_vec(), b"after".to_vec()];
    prepared
}

fn receipt(state: &str, applied: usize, checked: bool) -> InspectedRecord {
    record(
        RECEIPT,
        "integration-receipt",
        serde_json::json!({
            "schema":1,"native_journal_linkage":1,"base":BASE,"owner":"parent","state":state,"prepared":PREPARED,
            "applied":applied,"total":1,"postimage_verified":checked,
            "possibly_applied_record":null,"stop":null,
            "files":[{"key":"exact-file","state":"applied","postimage_checked":checked,"native_record_id":1}]
        }),
    )
}

fn verification_started() -> InspectedRecord {
    record(
        VERIFY_START,
        "integration-verification-started",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"state":"started","base":BASE,"owner":"parent",
            "prepared":PREPARED,"integration":RECEIPT,"source_identity":vec![1u8;32]
        }),
    )
}

fn verification(state: &str, accepted: bool) -> InspectedRecord {
    record(
        VERIFY_FINISH,
        "integration-verification",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"base":BASE,"owner":"parent","state":state,"prepared":PREPARED,
            "integration":RECEIPT,"verification_started":VERIFY_START,"accepted":accepted,
            "source_identity":vec![1u8;32],"command":"test fixture only","effect_state":{"state":"settled"},
            "read_dependencies":[],"read_dependency_status":"none","applied_paths":[SOURCE_PATH],
            "scope":"fixture evidence only"
        }),
    )
}

fn copy_record(record: &InspectedRecord) -> InspectedRecord {
    InspectedRecord {
        id: record.id.clone(),
        owner: record.owner.clone(),
        label: record.label.clone(),
        metadata: record.metadata.clone(),
        images: record.images.clone(),
    }
}

fn change_metadata(record: &mut InspectedRecord, change: impl FnOnce(&mut serde_json::Value)) {
    let mut value = serde_json::from_slice(&record.metadata).unwrap();
    change(&mut value);
    record.metadata = serde_json::to_vec(&value).unwrap();
}

#[test]
fn only_complete_cross_linked_accepted_verification_settles_integration() {
    assert_eq!(classify_integration_records(BASE, &[]).unwrap(), (0, 0, 0));
    assert!(classify_integration_records(BASE, &[prepared()]).is_err());
    assert!(
        classify_integration_records(BASE, &[prepared(), receipt("partial", 0, false)]).is_err()
    );
    assert!(classify_integration_records(
        BASE,
        &[prepared(), receipt("applied-unverified", 1, true)]
    )
    .is_err());
    for state in ["unverified", "stale", "verification_failed"] {
        assert!(classify_integration_records(
            BASE,
            &[
                prepared(),
                receipt("applied-unverified", 1, true),
                verification_started(),
                verification(state, false),
            ]
        )
        .is_err());
    }
    assert_eq!(
        classify_integration_records(
            BASE,
            &[
                prepared(),
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
    let mut future = prepared();
    let mut value: serde_json::Value = serde_json::from_slice(&future.metadata).unwrap();
    value["schema"] = serde_json::json!(2);
    future.metadata = serde_json::to_vec(&value).unwrap();
    assert!(classify_integration_records(BASE, &[future]).is_err());
    let mut corrupt = prepared();
    corrupt.metadata = b"not-json".to_vec();
    assert!(classify_integration_records(BASE, &[corrupt]).is_err());
    assert!(classify_integration_records(BASE, &[receipt("applied-unverified", 1, true)]).is_err());
    let mut wrong = verification("verified", true);
    let mut value: serde_json::Value = serde_json::from_slice(&wrong.metadata).unwrap();
    value["integration"] = serde_json::json!("other-receipt");
    wrong.metadata = serde_json::to_vec(&value).unwrap();
    assert!(classify_integration_records(
        BASE,
        &[
            prepared(),
            receipt("applied-unverified", 1, true),
            verification_started(),
            wrong
        ]
    )
    .is_err());
    assert!(classify_integration_records(
        BASE,
        &[
            prepared(),
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
    assert!(classify_integration_records(BASE, &[unknown]).is_err());
    assert!(classify_integration_records(
        BASE,
        &[
            prepared(),
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
        assert!(classify_integration_records(
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
        let mut invalid = prepared();
        change_metadata(&mut invalid, mutate);
        reject(invalid);
    }
    let mut wrong_bytes = prepared();
    wrong_bytes.images[1] = b"other".to_vec();
    reject(wrong_bytes);
    let mut missing = prepared();
    missing.images.pop();
    reject(missing);
    let mut unused = prepared();
    unused.images.push(b"unused".to_vec());
    reject(unused);

    let mut created = prepared();
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
        classify_integration_records(
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
        let mut invalid = prepared();
        change_metadata(&mut invalid, mutate);
        assert!(classify_integration_records(
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
    assert!(classify_integration_records(
        BASE,
        &[
            prepared(),
            invalid_receipt,
            verification_started(),
            verification("verified", true),
        ]
    )
    .is_err());
}

fn sealed_prepared() -> InspectedRecord {
    let mut record = prepared();
    change_metadata(&mut record, |value| {
        // Change only the test author's ID; all existing evidence relationships stay exact.
        let old = AUTHOR;
        let author = "parent/child/overlay-1";
        value["overlay_lineage_version"] = serde_json::json!(1);
        value["overlays"][0]["id"] = serde_json::json!("overlay-1");
        value["overlays"][0]["author"] = serde_json::json!(author);
        value["overlays"][0]["generation"] = serde_json::json!(1);
        value["overlays"][0]["manifest"] = serde_json::json!(PREPARED);
        assert_eq!(value["observations"][0]["author"], old);
        value["observations"][0]["author"] = serde_json::json!(author);
        value["files"][0]["contributors"][0] = serde_json::json!(author);
        value["files"][0]["lineage"][0]["author"] = serde_json::json!(author);
    });
    record
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
    let mut prepared = prepared();
    let dependencies = serde_json::json!([{"observer":"parent/peer/overlay-two","key":"exact-file","peer_authors":[AUTHOR]}]);
    change_metadata(&mut prepared, |value| {
        value["overlays"]
            .as_array_mut()
            .unwrap()
            .push(overlay("parent/peer", "overlay-two"));
        value["observations"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "author":"parent/peer/overlay-two","key":"exact-file","path":SOURCE_PATH,
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
        classify_integration_records(
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
        assert!(classify_integration_records(
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
    assert!(classify_integration_records(
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
    let prepared = prepared();
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
        assert!(classify_integration_records(
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
    let mut prepared = prepared();
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
        classify_integration_records(
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

fn ctx_at(fixture: &TestDir) -> RepoContext {
    let root = fixture.path().join("source");
    let repo = fixture.path().join("repo-0123456789abcdef");
    let namespace = repo.join("worktrees/wt-0123456789abcdef");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&namespace).unwrap();
    RepoContext {
        root: root.clone(),
        git_dir: root.join(".git"),
        common_git_dir: root.join(".git"),
        store_git_dir: repo.join("store.git"),
        namespace_dir: namespace.clone(),
        repo_id: "repo-0123456789abcdef".into(),
        worktree_id: "wt-0123456789abcdef".into(),
        ref_prefix: "refs/ng/tm/wt-0123456789abcdef".into(),
        hooks_dir: namespace.join("empty-hooks"),
        filters_checked: std::cell::Cell::new(true),
        reparse_checked: std::cell::Cell::new(true),
        skip_dirs: std::cell::RefCell::new(None),
    }
}

fn durable_record(directory: &Path, id: &str, label: &str, metadata: serde_json::Value) -> String {
    durable_record_images(directory, id, label, metadata, &[])
}

fn durable_record_images(
    directory: &Path,
    id: &str,
    label: &str,
    metadata: serde_json::Value,
    images: &[Vec<u8>],
) -> String {
    durable_owned_record_images(directory, id, "parent", label, metadata, images)
}

fn durable_owned_record_images(
    directory: &Path,
    id: &str,
    owner: &str,
    label: &str,
    metadata: serde_json::Value,
    images: &[Vec<u8>],
) -> String {
    let metadata = serde_json::to_vec(&metadata).unwrap();
    let image = fingerprint(&metadata);
    let mut image_refs = Vec::new();
    for bytes in images {
        let fp = FileFingerprint::for_bytes(bytes);
        let hash = fp
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        fs::write(directory.join(format!("b-{hash}")), bytes).unwrap();
        image_refs.push(fingerprint(bytes));
    }
    let fp = FileFingerprint::for_bytes(&metadata);
    let digest = fp
        .sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs::write(directory.join(format!("b-{digest}")), metadata).unwrap();
    let envelope = serde_json::to_vec(&serde_json::json!({
        "format":"aizen-speculation-v1","kind":"controller-record","base":id,
        "owner":owner,"label":label,"metadata":image,"images":image_refs
    }))
    .unwrap();
    durable_manifest(directory, &envelope)
}

fn durable_manifest(directory: &Path, bytes: &[u8]) -> String {
    let fp = FileFingerprint::for_bytes(bytes);
    let digest = fp
        .sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = format!("m-{digest}.json");
    fs::write(directory.join(&name), bytes).unwrap();
    name
}

fn cohort(ctx: &RepoContext) -> PathBuf {
    let directory = ctx.namespace_dir.join("speculation-v1").join(BASE);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("owner.lock"),
        b"aizen-speculation-owner-v1\n",
    )
    .unwrap();
    let base = serde_json::to_vec(&serde_json::json!({
        "format":"aizen-speculation-v1","kind":"base","base":BASE,
        "repo":"fixture","worktree":"fixture","root":"evidence only","bindings":[]
    }))
    .unwrap();
    durable_manifest(&directory, &base);
    directory
}

fn write_verified_chain(directory: &Path) {
    let prepared = durable_record(directory, BASE, "integration-prepared", empty_prepared());
    let receipt = durable_record(
        directory,
        BASE,
        "integration-receipt",
        serde_json::json!({
            "schema":1,"native_journal_linkage":1,"base":BASE,"owner":"parent","state":"applied-unverified","prepared":prepared,
            "applied":0,"total":0,"postimage_verified":true,"files":[],"possibly_applied_record":null,"stop":null
        }),
    );
    let started = durable_record(
        directory,
        BASE,
        "integration-verification-started",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"base":BASE,"owner":"parent","state":"started",
            "prepared":prepared,"integration":receipt,"source_identity":vec![1u8;32]
        }),
    );
    durable_record(
        directory,
        BASE,
        "integration-verification",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"base":BASE,"owner":"parent","state":"verified","prepared":prepared,
            "integration":receipt,"verification_started":started,"accepted":true,"source_identity":vec![1u8;32],
            "command":"test fixture only","effect_state":{"state":"settled"},"scope":"fixture evidence only",
            "read_dependencies":[],"read_dependency_status":"none","applied_paths":[]
        }),
    );
}

#[test]
fn verification_lifecycle_is_order_independent_exact_and_nonterminal_until_settled_success() {
    use crate::core::speculation::verification::validate_verification_records;
    let chain = || {
        vec![
            verification("verified", true),
            receipt("applied-unverified", 1, true),
            verification_started(),
            prepared(),
        ]
    };
    validate_verification_records(BASE, &chain()).unwrap();
    assert_eq!(
        classify_integration_records(BASE, &chain()).unwrap(),
        (1, 1, 0)
    );
    let start_only = vec![
        prepared(),
        receipt("applied-unverified", 1, true),
        verification_started(),
    ];
    validate_verification_records(BASE, &start_only).unwrap();
    assert!(classify_integration_records(BASE, &start_only).is_err());
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
        assert!(classify_integration_records(BASE, &records).is_err());
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
        assert!(classify_integration_records(BASE, &records).is_err());
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
        prepared(),
        receipt("applied-unverified", 1, true),
        verification_started(),
        verification("verified", true),
    ];
    assert_eq!(
        classify_integration_records(BASE, &records).unwrap(),
        (1, 1, 0)
    );
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
    let mut records = vec![prepared(), receipt("applied-unverified", 1, true), legacy];
    validate_verification_records(BASE, &records).unwrap();
    assert!(classify_integration_records(BASE, &records).is_err());
    records.push(verification_started());
    assert!(
        validate_verification_records(BASE, &records).is_err(),
        "legacy cannot borrow a later lifecycle start"
    );
}

#[test]
fn recovery_selection_inspects_start_only_and_unknown_verifier_without_settling_or_writing() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
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
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
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
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
    assert_eq!(after, recovery_inventory_digest(&directory).unwrap());
    assert!(
        !ctx.ledger_path().exists() && !ctx.journal_path().exists() && !ctx.store_git_dir.exists()
    );
}

#[test]
fn pending_integration_freezes_recovery_save_and_restore_without_touching_source_or_journal() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let source = ctx.root.join("source.txt");
    fs::write(&source, b"partial integration must remain").unwrap();
    let directory = cohort(&ctx);
    durable_record(&directory, BASE, "integration-prepared", empty_prepared());
    let _store = ctx.store_shared().unwrap();
    let _transaction = ctx.lock().unwrap();
    let mut ledger = Ledger::default();
    ctx.save_journal(&Journal::new(JournalKind::Clear, 0))
        .unwrap();
    let original = fs::read(ctx.journal_path()).unwrap();
    assert!(ctx.recover_pending(&mut ledger).is_err());
    assert!(ctx.recover_pending_with_restore(&mut ledger, true).is_err());
    assert!(save_locked(&ctx, "blocked", false, None).is_err());
    assert_eq!(
        fs::read(&source).unwrap(),
        b"partial integration must remain"
    );
    assert_eq!(fs::read(ctx.journal_path()).unwrap(), original);
    assert_eq!(ledger.generation, 0);
    assert!(!ctx.ledger_path().exists());
    assert!(
        !ctx.store_git_dir.exists(),
        "guard refuses before any git operation"
    );
}

#[test]
fn direct_mode_and_completed_records_survive_metadata_recovery_and_gc_preservation() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let _store = ctx.store_shared().unwrap();
    let _transaction = ctx.lock().unwrap();
    let mut ledger = Ledger::default();
    ctx.save_journal(&Journal::new(JournalKind::Clear, 0))
        .unwrap();
    ctx.recover_pending(&mut ledger).unwrap();
    assert!(!ctx.journal_path().exists());
    let directory = cohort(&ctx);
    write_verified_chain(&directory);
    let before = checked_metadata_entries(&directory).unwrap().len();
    let inspection = require_settled_speculation(&ctx.namespace_dir).unwrap();
    assert_eq!(
        (inspection.cohorts, inspection.prepared, inspection.verified),
        (1, 1, 1)
    );
    ctx.save_journal(&Journal::new(JournalKind::Prune, 0))
        .unwrap();
    ctx.recover_pending(&mut ledger).unwrap();
    assert!(!ctx.journal_path().exists());
    assert_eq!(checked_metadata_entries(&directory).unwrap().len(), before);
    assert!(store_has_speculation_evidence(ctx.store_git_dir.parent().unwrap()).unwrap());
}

#[test]
fn owner_probe_never_waits_and_future_namespace_or_manifest_retains_evidence() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
    let owner = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &directory.join("owner.lock"),
        Duration::ZERO,
    )
    .unwrap();
    let start = std::time::Instant::now();
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(owner);
    assert!(require_settled_speculation(&ctx.namespace_dir).is_ok());
    let future = serde_json::to_vec(
        &serde_json::json!({"format":"aizen-speculation-v2","kind":"base","base":BASE}),
    )
    .unwrap();
    let path = durable_manifest(&directory, &future);
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
    fs::remove_file(directory.join(path)).unwrap();
    let future = ctx.namespace_dir.join("speculation-v2");
    fs::create_dir(&future).unwrap();
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
    assert!(store_has_speculation_evidence(ctx.store_git_dir.parent().unwrap()).unwrap());
}

#[test]
fn hardlinked_metadata_and_oversized_cohort_traversal_are_refused() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
    let marker = fixture.path().join("outside-metadata");
    fs::write(&marker, b"outside untouched").unwrap();
    fs::hard_link(&marker, directory.join("b-alias")).unwrap();
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
    assert_eq!(fs::read(&marker).unwrap(), b"outside untouched");
    fs::remove_file(directory.join("b-alias")).unwrap();
    let version = directory.parent().unwrap();
    for index in 0..MAX_SPECULATION_COHORTS {
        fs::create_dir(version.join(format!("{index:032x}"))).unwrap();
    }
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
}

#[cfg(unix)]
#[test]
fn linked_speculation_namespace_is_refused_not_followed() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let outside = fixture.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, ctx.namespace_dir.join("speculation-v1")).unwrap();
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
    assert!(store_has_speculation_evidence(ctx.store_git_dir.parent().unwrap()).unwrap());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn bounded_store_scan_and_trash_preserve_completed_or_unknown_evidence() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
    write_verified_chain(&directory);
    let root = ctx.store_git_dir.parent().unwrap();
    let _store = ctx.store_exclusive().unwrap();
    let _transaction = ctx.lock().unwrap();
    require_settled_store_speculation(root, Some(&ctx.namespace_dir)).unwrap();
    let other = root.join("worktrees/wt-fedcba9876543210");
    fs::create_dir_all(other.join("speculation-v9")).unwrap();
    assert!(require_settled_store_speculation(root, Some(&ctx.namespace_dir)).is_err());
    let stamp = fixture.path().join("trash-stamp");
    let preserved =
        stamp.join("repo-fedcba9876543210/worktrees/wt-fedcba9876543210/speculation-v9");
    fs::create_dir_all(&preserved).unwrap();
    assert!(trash_has_speculation_evidence(&stamp).unwrap());
    assert!(directory.exists());
}

#[test]
fn corrupt_or_missing_registry_cannot_hide_speculation_behind_legacy_fallback() {
    let fixture = TestDir::new();
    let home = fixture.path().join("home");
    let root = home.join("timemachine");
    let evidence = root.join("repo-0123456789abcdef/worktrees/wt-0123456789abcdef/speculation-v1");
    fs::create_dir_all(&evidence).unwrap();
    let source = fixture.path().join("source");
    fs::create_dir(&source).unwrap();
    let common = source.join(".git");
    for bytes in [
        None,
        Some(b"corrupt registry".as_slice()),
        Some(br#"{"schema_version":2,"repos":[]}"#.as_slice()),
    ] {
        if let Some(bytes) = bytes {
            fs::write(root.join("registry.json"), bytes).unwrap();
        }
        assert!(resolve_identity(&home, &common, &common, &source).is_err());
        if let Some(bytes) = bytes {
            assert_eq!(fs::read(root.join("registry.json")).unwrap(), bytes);
        } else {
            assert!(!root.join("registry.json").exists());
        }
        assert!(evidence.exists());
    }
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
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
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
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
    let prepared = durable_record(&directory, BASE, "integration-prepared", empty_prepared());
    let (inventory, _, _) = read_recovery_selection(&directory, BASE, &prepared).unwrap();
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
    assert!(!ctx.ledger_path().exists());
    assert!(!ctx.journal_path().exists());
    assert!(!ctx.store_git_dir.exists());
}

#[test]
fn recovery_selection_rejects_stale_unknown_and_arbitrary_paths_without_writes() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
    let record = prepared();
    let prepared = durable_record_images(
        &directory,
        BASE,
        "integration-prepared",
        serde_json::from_slice(&record.metadata).unwrap(),
        &record.images,
    );
    let (before, files, native_receipt) =
        read_recovery_selection(&directory, BASE, &prepared).unwrap();
    assert_eq!(files.len(), 1);
    assert!(
        native_receipt.is_none(),
        "a missing receipt is retained as missing proof"
    );
    assert!(recovery_cohort_directory(&ctx.namespace_dir, "../outside").is_err());
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
    assert!(!ctx.ledger_path().exists());
    assert!(!ctx.journal_path().exists());
    assert!(!ctx.store_git_dir.exists());
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
fn recovery_report_requires_parent_and_keeps_pending_guard_and_metadata_unchanged() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some((fixture, identity, home)) = super::tests::speculation_git_fixture("recovery-report")
    else {
        return;
    };
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        let parent = crate::core::exec_ctx::ExecutionContext::default()
            .with_resource_scope("recovery-parent");
        crate::core::exec_ctx::with_current(parent.clone(), || {
            let plan = PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
            let writer = crate::core::workspace_txn::WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "recovery fixture",
                "recovery-parent",
            )
            .unwrap();
            let physical = crate::core::workspace_txn::PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "recovery fixture",
                "recovery-parent",
            )
            .unwrap();
            let binding = plan.admit(&writer, &physical).unwrap();
            let ctx = binding.binding.context().unwrap();
            let directory = cohort(&ctx);
            for path in checked_metadata_entries(&directory).unwrap() {
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("m-")
                {
                    fs::remove_file(path).unwrap();
                }
            }
            durable_manifest(&directory, &serde_json::to_vec(&serde_json::json!({
                "format":"aizen-speculation-v1","kind":"base","base":BASE,
                "repo":identity.repo_id,"worktree":identity.worktree_id,"root":identity.canonical_root,"bindings":[]
            })).unwrap());
            let prepared =
                durable_record(&directory, BASE, "integration-prepared", empty_prepared());
            let inventory = recovery_inventory_digest(&directory).unwrap();
            let selection = binding
                .select_speculation_recovery(&parent, &writer, &physical, BASE, &prepared)
                .unwrap();
            let report = binding
                .report_speculation_recovery(&parent, &writer, &physical, &selection, &[])
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
                .select_speculation_recovery(&delegated, &writer, &physical, BASE, &prepared)
                .is_err());
            let mut stale = selection.clone();
            stale.inventory_sha256[0] ^= 1;
            assert!(binding
                .report_speculation_recovery(&parent, &writer, &physical, &stale, &[])
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
                .report_speculation_recovery(&parent, &writer, &physical, &selection, &[])
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
            assert!(!ctx.ledger_path().exists());
            assert!(!ctx.journal_path().exists());
            assert!(!ctx.store_git_dir.exists());
        });
    });
}

fn apply_recovery_fixture_write(root: &Path, path: &Path, owner: &str) -> u64 {
    use crate::core::{
        exec_ctx, native_journal,
        repo_lock::LockMode,
        workspace_txn::{NativeResource, NativeResourceLease},
    };
    let resource = NativeResource::resolve(path).unwrap();
    let context = exec_ctx::ExecutionContext::default().with_resource_scope(owner);
    exec_ctx::with_current(context, || {
        let _lease = NativeResourceLease::acquire(
            root,
            &[(resource.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            owner,
        )
        .unwrap();
        let before = FileFingerprint::for_bytes(b"before");
        let mut batch =
            native_journal::begin_batch(owner, &[(&resource, &before, b"after")]).unwrap();
        batch
            .apply_with_record(&resource, &before, b"after")
            .unwrap()
            .native_record_id
            .unwrap()
    })
}

fn durable_recovery_receipt_for_resource(
    directory: &Path,
    resource: &crate::core::workspace_txn::NativeResource,
    id: u64,
    possible: bool,
) -> String {
    durable_recovery_evidence(directory, resource, id, possible, None)
}

fn durable_recovery_evidence(
    directory: &Path,
    resource: &crate::core::workspace_txn::NativeResource,
    id: u64,
    possible: bool,
    coverage: Option<&crate::core::native_journal::BatchIdentity>,
) -> String {
    let mut prepared_record = prepared();
    change_metadata(&mut prepared_record, |value| {
        if let Some(coverage) = coverage {
            value["native_coverage"] = serde_json::to_value(coverage).unwrap();
        }
        value["observations"][0]["key"] = serde_json::json!(resource.key());
        value["observations"][0]["path"] = serde_json::json!(resource.path());
        value["files"][0]["key"] = serde_json::json!(resource.key());
        value["files"][0]["path"] = serde_json::json!(resource.path());
    });
    let prepared = durable_record_images(
        directory,
        BASE,
        "integration-prepared",
        serde_json::from_slice(&prepared_record.metadata).unwrap(),
        &prepared_record.images,
    );
    let mut receipt_record = receipt(
        if possible {
            "partial"
        } else {
            "applied-unverified"
        },
        usize::from(!possible),
        !possible,
    );
    change_metadata(&mut receipt_record, |value| {
        if let Some(coverage) = coverage {
            value["native_journal_linkage"] = serde_json::json!(2);
            value["native_coverage"] = serde_json::to_value(coverage).unwrap();
        }
        value["prepared"] = serde_json::json!(prepared);
        value["files"][0]["key"] = serde_json::json!(resource.key());
        value["files"][0]["native_record_id"] = serde_json::json!(id);
        if possible {
            value["possibly_applied_record"] = serde_json::json!(id);
            value["files"][0]["state"] = serde_json::json!("possibly-applied");
        }
    });
    durable_record(
        directory,
        BASE,
        "integration-receipt",
        serde_json::from_slice(&receipt_record.metadata).unwrap(),
    );
    prepared
}

#[test]
fn recovery_native_receipt_joins_exact_journal_record_and_refuses_mismatches_without_writes() {
    use crate::core::{native_journal, workspace_txn::NativeResource};
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let root = fs::canonicalize(&ctx.root).unwrap();
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
            let directory = cohort(&ctx);
            let prepared =
                durable_recovery_receipt_for_resource(&directory, &resources[0], id, false);
            let (inventory, files, native_receipt) =
                read_recovery_selection(&directory, BASE, &prepared).unwrap();
            let mut selection = recovery_selection_for_resources(&root, &resources);
            selection.files = files;
            selection.native_receipt = native_receipt;
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
            assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
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
        let ctx = ctx_at(&fixture);
        let root = fs::canonicalize(&ctx.root).unwrap();
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
                let directory = cohort(&ctx);
                let prepared = durable_recovery_evidence(
                    &directory,
                    &resources[0],
                    id,
                    false,
                    Some(&coverage),
                );
                let (inventory, files, native_receipt) =
                    read_recovery_selection(&directory, BASE, &prepared).unwrap();
                let mut selection = recovery_selection_for_resources(&root, &resources[..1]);
                selection.files = files;
                selection.native_receipt = native_receipt;
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
                assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
            });
        });
    }
}

#[test]
fn recovery_empty_covered_receipt_still_checks_the_existing_fence() {
    use crate::core::{exec_ctx, native_journal};
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let root = fs::canonicalize(&ctx.root).unwrap();
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
            let directory = cohort(&ctx);
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
            let (_, files, native_receipt) =
                read_recovery_selection(&directory, BASE, &prepared).unwrap();
            let mut selection = recovery_selection_for_resources(&root, &[]);
            selection.files = files;
            selection.native_receipt = native_receipt;
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
            assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
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
        let ctx = ctx_at(&fixture);
        let root = fs::canonicalize(&ctx.root).unwrap();
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
                let directory = cohort(&ctx);
                let prepared =
                    durable_recovery_receipt_for_resource(&directory, &resources[0], id, true);
                let (inventory, files, native_receipt) =
                    read_recovery_selection(&directory, BASE, &prepared).unwrap();
                let mut selection = recovery_selection_for_resources(&root, &resources);
                selection.files = files;
                selection.native_receipt = native_receipt;
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
                assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
            });
        });
    }
}

#[test]
fn recovery_report_surface_requires_explicit_resources_and_plain_parent_without_creating_storage() {
    use crate::agent::tools::Tool;
    use crate::core::exec_ctx::{self, ExecutionContext};
    let fixture = TestDir::new();
    let root = fixture.path().join("source");
    fs::create_dir(&root).unwrap();
    let view = CheckpointView::new(root.clone());
    let prepared = format!("m-{}.json", "a".repeat(64));
    let args = serde_json::json!({"action":"recovery_report","cohort":BASE,"prepared":prepared,"resource_paths":[]});
    assert!(!view.is_concurrency_safe_for(&args));
    assert!(view.is_concurrency_safe_for(&serde_json::json!({"action":"diff"})));
    assert!(view.is_concurrency_safe_for(&serde_json::json!({"action":"list"})));
    assert!(
        view.execute(&args).is_err(),
        "tool cannot fabricate a default parent"
    );
    let parent = ExecutionContext::default().with_resource_scope("report-parent");
    exec_ctx::with_current(parent.without_workspace_restore(), || {
        let error = view.execute(&args).unwrap_err();
        assert!(error.to_string().contains("coordinating-parent"));
    });
    exec_ctx::with_current(parent, || {
        for mutate in [
            (|value: &mut serde_json::Value| {
                value.as_object_mut().unwrap().remove("resource_paths");
            }) as fn(&mut serde_json::Value),
            |value| value["resource_paths"] = serde_json::json!([null]),
            |value| value["resource_paths"] = serde_json::json!(vec!["a.txt"; 257]),
            |value| value["paths"] = serde_json::json!(["a.txt"]),
            |value| value["action"] = serde_json::json!("list"),
            |value| value["cohort"] = serde_json::json!("../outside"),
            |value| value["prepared"] = serde_json::json!("../outside"),
        ] {
            let mut invalid = args.clone();
            mutate(&mut invalid);
            assert!(view.execute(&invalid).is_err());
        }
        assert!(view
            .execute(&args)
            .unwrap_err()
            .to_string()
            .contains("Git Time Machine binding"));
        assert!(
            record_speculation_recovery_observation(&root, BASE, &prepared, &[])
                .unwrap_err()
                .to_string()
                .contains("Git Time Machine binding")
        );
    });
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
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
fn recovery_report_uses_real_exact_receipt_under_parent_admission_without_settlement() {
    use crate::core::{
        exec_ctx, native_journal,
        workspace_txn::{NativeResource, PhysicalEffectLease, WorkspaceWriterLease},
    };
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some((fixture, identity, home)) =
        super::tests::speculation_git_fixture("recovery-native-report")
    else {
        return;
    };
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
            let (prepared, inventory, directory, journal) = exec_ctx::with_current(
                parent.clone(),
                || {
                    let plan = PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
                    let writer = WorkspaceWriterLease::acquire_identity_scoped(
                        identity.clone(),
                        Duration::ZERO,
                        None,
                        "recovery proof fixture",
                        "parent",
                    )
                    .unwrap();
                    let physical = PhysicalEffectLease::exclusive(
                        Duration::ZERO,
                        None,
                        "recovery proof fixture",
                        "parent",
                    )
                    .unwrap();
                    let binding = plan.admit(&writer, &physical).unwrap();
                    let ctx = binding.binding.context().unwrap();
                    let directory = cohort(&ctx);
                    for path in checked_metadata_entries(&directory).unwrap() {
                        if path
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with("m-")
                        {
                            fs::remove_file(path).unwrap();
                        }
                    }
                    durable_manifest(&directory, &serde_json::to_vec(&serde_json::json!({
                    "format":"aizen-speculation-v1","kind":"base","base":BASE,
                    "repo":identity.repo_id,"worktree":identity.worktree_id,"root":identity.canonical_root,"bindings":[]
                })).unwrap());
                    let prepared =
                        durable_recovery_receipt_for_resource(&directory, &resources[0], id, false);
                    let inventory = recovery_inventory_digest(&directory).unwrap();
                    let journal_path = home.join("native-journal-v1/records.bin");
                    let journal = fs::read(&journal_path).unwrap();
                    let selection = binding
                        .select_speculation_recovery(&parent, &writer, &physical, BASE, &prepared)
                        .unwrap();
                    let report = binding
                        .report_speculation_recovery(
                            &parent, &writer, &physical, &selection, &resources,
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
                    assert!(!ctx.ledger_path().exists());
                    assert!(!ctx.journal_path().exists());
                    assert!(!ctx.store_git_dir.exists());
                    (prepared, inventory, directory, journal)
                },
            );
            struct HomeRestore(Option<std::ffi::OsString>);
            impl Drop for HomeRestore {
                fn drop(&mut self) {
                    match &self.0 {
                        Some(value) => std::env::set_var("AIZEN_HOME", value),
                        None => std::env::remove_var("AIZEN_HOME"),
                    }
                }
            }
            let _restore_home = HomeRestore(std::env::var_os("AIZEN_HOME"));
            std::env::set_var("AIZEN_HOME", &home);
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
                let view = CheckpointView::new(identity.canonical_root.clone());
                let text = crate::agent::tools::Tool::execute(&view, &serde_json::json!({
                    "action":"recovery_report","cohort":BASE,"prepared":prepared,"resource_paths":["source.txt"]
                })).unwrap();
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

fn historical_inspection(prepared: &str, receipt: Option<&str>) -> InspectedRecord {
    use crate::core::speculation::recovery::OBSERVATION_LABEL;
    let observation_id = if receipt.is_some() {
        "inspection-with-receipt"
    } else {
        "inspection-record"
    };
    let mut observation = record(
        observation_id,
        OBSERVATION_LABEL,
        serde_json::json!({
            "schema":1,"base":BASE,"prepared":prepared,"receipt":receipt,
            "owner":"parent","observer":"fresh-observer","state":"observed_nonterminal",
            "inventory_sha256":vec![9u8;32],"files":[{
                "key":"exact-file","class":"pre","fingerprint":fingerprint(b"before")
            }],"native_records":if receipt.is_some() { serde_json::json!([{"id":1,"status":"settled_applied"}]) } else { serde_json::json!([]) }
        }),
    );
    observation.owner = "fresh-observer".into();
    observation
}

#[test]
fn recovery_observation_is_historical_and_never_replaces_receipt_or_verification() {
    let prepared_id = format!("m-{}.json", "b".repeat(64));
    let receipt_id = format!("m-{}.json", "c".repeat(64));
    let mut prepared = prepared();
    prepared.id = prepared_id.clone();
    let observation = historical_inspection(&prepared_id, None);
    let mut records = vec![prepared, observation];
    validate_inspection_observation(BASE, &records[1], &records).unwrap();
    assert!(classify_integration_records(BASE, &records).is_err());
    let mut receipt = receipt("applied-unverified", 1, true);
    receipt.id = receipt_id.clone();
    change_metadata(&mut receipt, |value| {
        value["prepared"] = serde_json::json!(prepared_id)
    });
    records.push(receipt);
    // A no-receipt observation remains a truthful historical record after a later receipt arrives.
    validate_inspection_observation(BASE, &records[1], &records).unwrap();
    assert!(classify_integration_records(BASE, &records).is_err());
    records.push(historical_inspection(&prepared_id, Some(&receipt_id)));
    validate_inspection_observation(BASE, &records[3], &records).unwrap();
    assert!(classify_integration_records(BASE, &records).is_err());
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
    assert_eq!(
        classify_integration_records(BASE, &records).unwrap(),
        (1, 1, 0)
    );
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
    let ctx = ctx_at(&fixture);
    let directory = cohort(&ctx);
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
    let (_, _, selected_receipt) = read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
    assert!(selected_receipt.is_none());
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
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
    let (_, _, selected_receipt) = read_recovery_selection(&directory, BASE, &prepared_id).unwrap();
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
    assert!(require_settled_speculation(&ctx.namespace_dir).is_err());
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
    require_settled_speculation(&ctx.namespace_dir).unwrap();
    assert!(
        !ctx.ledger_path().exists() && !ctx.journal_path().exists() && !ctx.store_git_dir.exists()
    );
}

#[test]
fn recovery_observation_missing_receipt_remains_pending_and_busy_owner_prevents_append() {
    use crate::core::{
        exec_ctx,
        repo_lock::RepoTxnLock,
        workspace_txn::{NativeResource, PhysicalEffectLease, WorkspaceWriterLease},
    };
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some((fixture, identity, home)) =
        super::tests::speculation_git_fixture("observation-no-receipt")
    else {
        return;
    };
    let source = identity.canonical_root.join("source.txt");
    fs::write(&source, b"before").unwrap();
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
        let parent = exec_ctx::ExecutionContext::default().with_resource_scope("fresh-inspector");
        exec_ctx::with_current(parent.clone(), || {
            let plan = PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "missing receipt observation fixture",
                "fresh-inspector",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "missing receipt observation fixture",
                "fresh-inspector",
            )
            .unwrap();
            let binding = plan.admit(&writer, &physical).unwrap();
            let ctx = binding.binding.context().unwrap();
            let directory = cohort(&ctx);
            for path in checked_metadata_entries(&directory).unwrap() {
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("m-")
                {
                    fs::remove_file(path).unwrap();
                }
            }
            durable_manifest(&directory, &serde_json::to_vec(&serde_json::json!({
                "format":"aizen-speculation-v1","kind":"base","base":BASE,
                "repo":identity.repo_id,"worktree":identity.worktree_id,"root":identity.canonical_root,"bindings":[]
            })).unwrap());
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
                .select_speculation_recovery(&parent, &writer, &physical, BASE, &prepared)
                .unwrap();
            assert!(selection.native_receipt.is_none());
            let before = selection.inventory_sha256;
            let owner = RepoTxnLock::acquire_existing_exclusive(
                &directory.join("owner.lock"),
                Duration::ZERO,
            )
            .unwrap();
            let start = std::time::Instant::now();
            assert!(binding
                .inspect_selected_recovery(
                    &parent,
                    &writer,
                    &physical,
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
                    &writer,
                    &physical,
                    &selection,
                    &[resource],
                    true,
                )
                .unwrap();
            assert_eq!(report.files[0].class, RecoveryImageClass::Pre);
            assert!(report.native_records.is_empty());
            assert_eq!(report.status, RecoveryReportStatus::ObservedNonterminal);
            let records =
                crate::core::speculation::store::inspect_controller_records(&directory).unwrap();
            let record = records
                .iter()
                .find(|record| record.id == report.observation.as_ref().unwrap().record)
                .unwrap();
            let observation: crate::core::speculation::recovery::InspectionObservation =
                serde_json::from_slice(&record.metadata).unwrap();
            assert!(observation.receipt.is_none() && observation.native_records.is_empty());
            assert_eq!(observation.owner, "parent");
            assert_eq!(observation.observer, "fresh-inspector");
            read_recovery_selection(&directory, BASE, &prepared).unwrap();
            assert!(require_settled_speculation(binding.namespace_dir()).is_err());
            assert_eq!(fs::read(&source).unwrap(), b"before");
            assert!(!home.join("native-journal-v1").exists());
            assert!(
                !ctx.ledger_path().exists()
                    && !ctx.journal_path().exists()
                    && !ctx.store_git_dir.exists()
            );
        });
    });
}

#[test]
fn recovery_observation_appends_under_fresh_parent_without_settling_native_effects() {
    use crate::core::{
        exec_ctx, native_journal,
        repo_lock::LockMode,
        workspace_txn::{
            NativeResource, NativeResourceLease, PhysicalEffectLease, WorkspaceWriterLease,
        },
    };
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    struct HomeRestore(Option<std::ffi::OsString>);
    impl Drop for HomeRestore {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("AIZEN_HOME", value),
                None => std::env::remove_var("AIZEN_HOME"),
            }
        }
    }
    let _restore_home = HomeRestore(std::env::var_os("AIZEN_HOME"));
    for possible in [false, true] {
        let Some((fixture, identity, home)) =
            super::tests::speculation_git_fixture("recovery-observation")
        else {
            return;
        };
        let source = identity.canonical_root.join("source.txt");
        fs::write(&source, b"before").unwrap();
        std::env::set_var("AIZEN_HOME", &home);
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
                        let plan =
                            PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
                        let writer = WorkspaceWriterLease::acquire_identity_scoped(
                            identity.clone(),
                            Duration::ZERO,
                            None,
                            "observation fixture",
                            "new-inspection-parent",
                        )
                        .unwrap();
                        let physical = PhysicalEffectLease::exclusive(
                            Duration::ZERO,
                            None,
                            "observation fixture",
                            "new-inspection-parent",
                        )
                        .unwrap();
                        let binding = plan.admit(&writer, &physical).unwrap();
                        let ctx = binding.binding.context().unwrap();
                        let directory = cohort(&ctx);
                        for path in checked_metadata_entries(&directory).unwrap() {
                            if path
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with("m-")
                            {
                                fs::remove_file(path).unwrap();
                            }
                        }
                        durable_manifest(&directory, &serde_json::to_vec(&serde_json::json!({
                        "format":"aizen-speculation-v1","kind":"base","base":BASE,
                        "repo":identity.repo_id,"worktree":identity.worktree_id,"root":identity.canonical_root,"bindings":[]
                    })).unwrap());
                        let prepared = durable_recovery_receipt_for_resource(
                            &directory,
                            &resources[0],
                            id,
                            possible,
                        );
                        let selection = binding
                            .select_speculation_recovery(
                                &observer, &writer, &physical, BASE, &prepared,
                            )
                            .unwrap();
                        let journal = fs::read(home.join("native-journal-v1/records.bin")).unwrap();
                        let initial = selection.inventory_sha256;
                        let report = binding
                            .inspect_selected_recovery(
                                &observer, &writer, &physical, &selection, &resources, true,
                            )
                            .unwrap();
                        assert_eq!(report.status, RecoveryReportStatus::ObservedNonterminal);
                        let publication = report.observation.as_ref().unwrap();
                        assert_eq!(publication.before, initial);
                        assert_ne!(publication.after, initial);
                        assert_eq!(
                            publication.after,
                            recovery_inventory_digest(&directory).unwrap()
                        );
                        assert_eq!(report.to_json()["inspection_only"], false);
                        assert_eq!(report.to_json()["pending_guard_unchanged"], true);
                        let records =
                            crate::core::speculation::store::inspect_controller_records(&directory)
                                .unwrap();
                        let observed = records
                            .iter()
                            .find(|record| record.id == publication.record)
                            .unwrap();
                        let data: crate::core::speculation::recovery::InspectionObservation =
                            serde_json::from_slice(&observed.metadata).unwrap();
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
                                &observer, &writer, &physical, &selection, &resources, true
                            )
                            .is_err());
                        assert!(require_settled_speculation(binding.namespace_dir()).is_err());
                        assert!(
                            !ctx.ledger_path().exists()
                                && !ctx.journal_path().exists()
                                && !ctx.store_git_dir.exists()
                        );
                        (prepared, directory, initial, publication.after, journal)
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
                    let view = CheckpointView::new(identity.canonical_root.clone());
                    let text = crate::agent::tools::Tool::execute(&view, &serde_json::json!({
                        "action":"recovery_report","cohort":BASE,"prepared":prepared,"resource_paths":["source.txt"]
                    })).unwrap();
                    assert!(text.contains("Inspection only; pending guard unchanged"));
                    assert_eq!(
                        recovery_inventory_digest(&directory).unwrap(),
                        before_report
                    );
                    let records =
                        crate::core::speculation::store::inspect_controller_records(&directory)
                            .unwrap();
                    assert_eq!(
                        records
                            .iter()
                            .filter(|record| record.label
                                == crate::core::speculation::recovery::OBSERVATION_LABEL)
                            .count(),
                        2
                    );
                    read_recovery_selection(&directory, BASE, &prepared).unwrap();
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
fn future_journal_is_retained_and_never_recovered_as_clear() {
    let fixture = TestDir::new();
    let ctx = ctx_at(&fixture);
    let _store = ctx.store_shared().unwrap();
    let _transaction = ctx.lock().unwrap();
    let mut journal = Journal::new(JournalKind::Clear, 0);
    journal.schema_version = 2;
    ctx.save_journal(&journal).unwrap();
    let bytes = fs::read(ctx.journal_path()).unwrap();
    assert!(ctx.recover_pending(&mut Ledger::default()).is_err());
    assert_eq!(fs::read(ctx.journal_path()).unwrap(), bytes);
}
