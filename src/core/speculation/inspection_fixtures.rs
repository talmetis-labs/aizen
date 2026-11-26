//! Shared test data builders; these do not confer production recovery authority.

use super::*;
use crate::core::persist::FileFingerprint;
use crate::core::speculation::evidence::InspectedRecord;

pub(crate) const BASE: &str = "0123456789abcdef0123456789abcdef";
pub(crate) const PREPARED: &str =
    "m-1111111111111111111111111111111111111111111111111111111111111111.json";
pub(crate) const RECEIPT: &str =
    "m-2222222222222222222222222222222222222222222222222222222222222222.json";
pub(crate) const VERIFY_START: &str =
    "m-3333333333333333333333333333333333333333333333333333333333333333.json";
pub(crate) const VERIFY_FINISH: &str =
    "m-4444444444444444444444444444444444444444444444444444444444444444.json";

pub(crate) const AUTHOR: &str = "parent/child/overlay-1";
pub(crate) const SOURCE_PATH: &str = "evidence-only/source.txt";

pub(crate) fn fingerprint(bytes: &[u8]) -> serde_json::Value {
    let fp = FileFingerprint::for_bytes(bytes);
    serde_json::json!({"exists":fp.exists,"byte_len":fp.byte_len,"sha256":fp.sha256})
}

pub(crate) fn record(id: &str, label: &str, value: serde_json::Value) -> InspectedRecord {
    InspectedRecord {
        id: id.into(),
        owner: "parent".into(),
        label: label.into(),
        metadata: serde_json::to_vec(&value).unwrap(),
        images: Vec::new(),
    }
}

pub(crate) fn overlay(owner: &str, id: &str) -> serde_json::Value {
    serde_json::json!({"base":BASE,"owner":owner,"id":id,"author":format!("{owner}/{id}")})
}

pub(crate) fn image_descriptor(bytes: &[u8], index: usize) -> serde_json::Value {
    let mut value = fingerprint(bytes);
    value["index"] = serde_json::json!(index);
    value["missing"] = serde_json::json!(false);
    value
}

pub(crate) fn empty_prepared() -> serde_json::Value {
    serde_json::json!({
        "schema":1,"base":BASE,"owner":"parent","state":"prepared",
        "overlays":[overlay("parent/child", "overlay-1")],
        "observations":[],"read_dependencies":[],"files":[]
    })
}

pub(crate) fn prepared() -> InspectedRecord {
    let mut prepared = record(
        PREPARED,
        "integration-prepared",
        serde_json::json!({
            "schema":1,"base":BASE,"owner":"parent","state":"prepared",
            "overlays":[overlay("parent/child", "overlay-1")],
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

pub(crate) fn receipt(state: &str, applied: usize, checked: bool) -> InspectedRecord {
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

pub(crate) fn verification_started() -> InspectedRecord {
    record(
        VERIFY_START,
        "integration-verification-started",
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"state":"started","base":BASE,"owner":"parent",
            "prepared":PREPARED,"integration":RECEIPT,"source_identity":vec![1u8;32]
        }),
    )
}

pub(crate) fn verification(state: &str, accepted: bool) -> InspectedRecord {
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

pub(crate) fn copy_record(record: &InspectedRecord) -> InspectedRecord {
    InspectedRecord {
        id: record.id.clone(),
        owner: record.owner.clone(),
        label: record.label.clone(),
        metadata: record.metadata.clone(),
        images: record.images.clone(),
    }
}

pub(crate) fn change_metadata(
    record: &mut InspectedRecord,
    change: impl FnOnce(&mut serde_json::Value),
) {
    let mut value = serde_json::from_slice(&record.metadata).unwrap();
    change(&mut value);
    record.metadata = serde_json::to_vec(&value).unwrap();
}

pub(crate) fn sealed_prepared() -> InspectedRecord {
    let mut record = prepared();
    change_metadata(&mut record, |value| {
        // Change only the test author's ID; all existing evidence relationships stay exact.
        let old = AUTHOR;
        let author = "parent/child/overlay-1";
        value["overlay_lineage_version"] = serde_json::json!(1);
        value["overlays"][0]["id"] = serde_json::json!("overlay-1");
        value["overlays"][0]["author"] = serde_json::json!(author);
        value["overlays"][0]["generation"] = serde_json::json!(1);
        value["overlays"][0]["manifest"] = serde_json::json!(SEALED);
        assert_eq!(value["observations"][0]["author"], old);
        value["observations"][0]["author"] = serde_json::json!(author);
        value["files"][0]["contributors"][0] = serde_json::json!(author);
        value["files"][0]["lineage"][0]["author"] = serde_json::json!(author);
    });
    record
}

pub(crate) fn durable_record(
    directory: &Path,
    id: &str,
    label: &str,
    metadata: serde_json::Value,
) -> String {
    durable_record_images(directory, id, label, metadata, &[])
}

pub(crate) fn durable_record_images(
    directory: &Path,
    id: &str,
    label: &str,
    metadata: serde_json::Value,
    images: &[Vec<u8>],
) -> String {
    durable_owned_record_images(directory, id, "parent", label, metadata, images)
}

pub(crate) fn durable_owned_record_images(
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

pub(crate) fn durable_manifest(directory: &Path, bytes: &[u8]) -> String {
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

pub(crate) fn cohort(namespace: &Path) -> PathBuf {
    let root = fs::canonicalize(namespace).unwrap();
    let identity =
        crate::core::workspace_txn::WorkspaceIdentity::from_bound_paths(root.clone(), None, None);
    let file = crate::core::speculation::evidence::fixtures::HistoricalFile::explicit(
        root.join("historical-empty-source.txt"),
        None,
        true,
    );
    crate::core::speculation::evidence::fixtures::write_base(namespace, &identity, &[file])
}

pub(crate) fn write_pending_prepared(directory: &Path) -> String {
    use crate::core::speculation::evidence::fixtures as history;
    let files = history::base_files(directory);
    let author = history::write_sealed_history(directory, 1, "parent/child", &files, &[]);
    history::write_worker_history(directory, std::slice::from_ref(&author));
    let (prepared_value, images) =
        history::prepared_document(&[author], &std::collections::BTreeMap::new());
    assert!(images.is_empty());
    durable_record(directory, BASE, "integration-prepared", prepared_value)
}

pub(crate) fn write_verified_chain(directory: &Path) {
    let prepared = write_pending_prepared(directory);
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

pub(crate) fn apply_recovery_fixture_write(root: &Path, path: &Path, owner: &str) -> u64 {
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

pub(crate) fn durable_recovery_receipt_for_resource(
    directory: &Path,
    resource: &crate::core::workspace_txn::NativeResource,
    id: u64,
    possible: bool,
) -> String {
    durable_recovery_evidence(directory, resource, id, possible, None)
}

pub(crate) fn durable_recovery_evidence(
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

pub(crate) fn historical_inspection(prepared: &str, receipt: Option<&str>) -> InspectedRecord {
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

// Narrow test-only access for legacy bridge tests. Production selections remain opaque.
pub(crate) fn stale_selection(
    mut selection: SpeculationRecoverySelection,
) -> SpeculationRecoverySelection {
    selection.inventory_sha256[0] ^= 1;
    selection
}

pub(crate) fn selection_inventory(selection: &SpeculationRecoverySelection) -> [u8; 32] {
    selection.inventory_sha256
}

pub(crate) fn assert_no_native_receipt(selection: &SpeculationRecoverySelection) {
    assert!(selection.native_receipt.is_none());
}

pub(crate) fn assert_valid_selection(directory: &Path, cohort: &str, prepared: &str) {
    read_recovery_selection(directory, cohort, prepared).unwrap();
}

/// Classifier inputs have already passed the separate immutable-image reader. These fixed raw
/// historical records complete that data fixture; they never mint a live physical release token.
const SEALED: &str = "m-6666666666666666666666666666666666666666666666666666666666666666.json";
const PEER_SEALED: &str = "m-7777777777777777777777777777777777777777777777777777777777777777.json";

pub(crate) fn sealed_peer() -> serde_json::Value {
    serde_json::json!({"base":BASE,"owner":"parent/peer","id":"overlay-2",
        "author":"parent/peer/overlay-2","generation":0,"manifest":PEER_SEALED})
}

pub(crate) fn historical_worker_records(peer: bool) -> Vec<InspectedRecord> {
    use crate::core::speculation::evidence::SealedManifestIdentity;
    use crate::core::speculation::verification::DurableEffectState;
    use crate::core::speculation::workers::*;
    let mut records = Vec::new();
    let mut publish = |label: &str, bytes: Vec<u8>| {
        let fp = FileFingerprint::for_bytes(&bytes);
        let hash = fp
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let id = format!("m-{hash}.json");
        records.push(InspectedRecord {
            id: id.clone(),
            owner: "parent".into(),
            label: label.into(),
            metadata: bytes,
            images: Vec::new(),
        });
        id
    };
    let mut members = vec![AttemptMember {
        attempt: 1,
        owner: "parent/child".into(),
        overlay: "overlay-1".into(),
        scope_sha256: [1; 32],
    }];
    if peer {
        members.push(AttemptMember {
            attempt: 2,
            owner: "parent/peer".into(),
            overlay: "overlay-2".into(),
            scope_sha256: [2; 32],
        });
    }
    let roster = WorkerRoster {
        schema: 1,
        lifecycle_version: 1,
        base: BASE.into(),
        owner: "parent".into(),
        incarnation: "fedcba9876543210fedcba9876543210".into(),
        members,
    };
    let roster_id = publish(ROSTER_LABEL, serde_json::to_vec(&roster).unwrap());
    for member in roster.members {
        let mut intents = Vec::new();
        let mut closed_workers = Vec::new();
        if member.attempt == 1 {
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
                prepared_sha256: [3; 32],
            };
            let intent_id = publish(INTENT_LABEL, serde_json::to_vec(&intent).unwrap());
            let closed = WorkerClosed {
                schema: 1,
                lifecycle_version: 1,
                base: BASE.into(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                intent: intent_id.clone(),
                result: WorkerResult::Returned,
                effect_state: DurableEffectState::Settled,
                ownership_released: true,
                children: Vec::new(),
            };
            let closed_id = publish(CLOSED_LABEL, serde_json::to_vec(&closed).unwrap());
            intents.push(intent_id.clone());
            closed_workers.push(WorkerReceipt {
                intent: intent_id,
                closed: closed_id,
            });
        }
        let admission = AttemptAdmissionClosed {
            schema: 1,
            lifecycle_version: 1,
            base: BASE.into(),
            owner: "parent".into(),
            roster: roster_id.clone(),
            attempt: member.attempt,
            driver_result: DriverResult::Completed,
            intents,
        };
        let admission_id = publish(ADMISSION_LABEL, serde_json::to_vec(&admission).unwrap());
        let closed = AttemptClosed {
            schema: 1,
            lifecycle_version: 1,
            base: BASE.into(),
            owner: "parent".into(),
            roster: roster_id.clone(),
            admission: admission_id,
            ownership_released: true,
            workers: closed_workers,
            sealed: SealedManifestIdentity {
                base: BASE.into(),
                overlay: member.overlay,
                owner: member.owner,
                generation: if member.attempt == 1 { 1 } else { 0 },
                manifest: if member.attempt == 1 {
                    SEALED
                } else {
                    PEER_SEALED
                }
                .into(),
            },
        };
        publish(ATTEMPT_LABEL, serde_json::to_vec(&closed).unwrap());
    }
    records
}
