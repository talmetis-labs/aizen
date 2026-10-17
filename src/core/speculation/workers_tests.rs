use super::*;
use crate::core::persist::FileFingerprint;
use crate::core::speculation::verification::DurableEffectState;
use serde_json::{json, Value};

const BASE: &str = "0123456789abcdef0123456789abcdef";
const OWNER: &str = "parent";

fn id(value: &str) -> String {
    let digest = FileFingerprint::for_bytes(value.as_bytes()).sha256;
    format!("m-{}.json", digest.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn record(label: &str, value: &impl Serialize) -> InspectedRecord {
    let metadata = serde_json::to_vec(value).unwrap();
    let name = format!("{}:{label}", String::from_utf8_lossy(&metadata));
    InspectedRecord { id: id(&name), owner: OWNER.into(), label: label.into(), metadata, images: Vec::new() }
}

fn changed(record: &InspectedRecord, change: impl FnOnce(&mut Value)) -> InspectedRecord {
    let mut value: Value = serde_json::from_slice(&record.metadata).unwrap();
    change(&mut value);
    // Re-encode through typed DTOs so negative linkage tests do not accidentally exercise only
    // canonical field ordering. Unknown/missing-field tests deliberately keep their invalid JSON.
    let metadata = match record.label.as_str() {
        ROSTER_LABEL => serde_json::from_value::<WorkerRoster>(value.clone()).ok().map(|v| serde_json::to_vec(&v).unwrap()),
        INTENT_LABEL => serde_json::from_value::<WorkerIntent>(value.clone()).ok().map(|v| serde_json::to_vec(&v).unwrap()),
        CLOSED_LABEL => serde_json::from_value::<WorkerClosed>(value.clone()).ok().map(|v| serde_json::to_vec(&v).unwrap()),
        ADMISSION_LABEL => serde_json::from_value::<AttemptAdmissionClosed>(value.clone()).ok().map(|v| serde_json::to_vec(&v).unwrap()),
        ATTEMPT_LABEL => serde_json::from_value::<AttemptClosed>(value.clone()).ok().map(|v| serde_json::to_vec(&v).unwrap()),
        _ => None,
    }.unwrap_or_else(|| serde_json::to_vec(&value).unwrap());
    InspectedRecord { id: record.id.clone(), owner: record.owner.clone(), label: record.label.clone(), metadata, images: Vec::new() }
}

fn roster(members: usize) -> WorkerRoster {
    WorkerRoster {
        schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
        incarnation: "fedcba9876543210fedcba9876543210".into(),
        members: (1..=members).map(|attempt| AttemptMember {
            attempt: attempt as u32, owner: format!("{OWNER}/task-{attempt}/attempt/1"),
            overlay: format!("overlay-{attempt}"), scope_sha256: [attempt as u8; 32],
        }).collect(),
    }
}

fn intent(roster: &str, worker: u32, parent: Option<u32>) -> WorkerIntent {
    WorkerIntent {
        schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
        roster: roster.into(), attempt: 1, worker, parent_worker: parent,
        kind: NativeWorkerKind::Edit, prepared_sha256: [worker as u8; 32],
    }
}

fn closed(roster: &str, intent: &str, children: Vec<WorkerReceipt>) -> WorkerClosed {
    WorkerClosed {
        schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
        roster: roster.into(), intent: intent.into(), result: WorkerResult::Returned,
        effect_state: DurableEffectState::Settled, ownership_released: true, children,
    }
}

fn sealed(member: &AttemptMember) -> SealedManifestIdentity {
    SealedManifestIdentity { base: BASE.into(), owner: member.owner.clone(), overlay: member.overlay.clone(),
        generation: 1, manifest: id(&format!("sealed:{}", member.owner)) }
}

fn complete_fixture() -> Vec<InspectedRecord> {
    let roster = record(ROSTER_LABEL, &roster(1));
    let first = record(INTENT_LABEL, &intent(&roster.id, 1, None));
    let second = record(INTENT_LABEL, &intent(&roster.id, 2, Some(1)));
    let second_closed = record(CLOSED_LABEL, &closed(&roster.id, &second.id, Vec::new()));
    let first_closed = record(CLOSED_LABEL, &closed(&roster.id, &first.id, vec![WorkerReceipt {
        intent: second.id.clone(), closed: second_closed.id.clone(),
    }]));
    let admission = record(ADMISSION_LABEL, &AttemptAdmissionClosed {
        schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
        roster: roster.id.clone(), attempt: 1, driver_result: DriverResult::Completed,
        intents: vec![first.id.clone(), second.id.clone()],
    });
    let attempt = record(ATTEMPT_LABEL, &AttemptClosed {
        schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
        roster: roster.id.clone(), admission: admission.id.clone(), ownership_released: true,
        workers: vec![WorkerReceipt { intent: first.id.clone(), closed: first_closed.id.clone() },
            WorkerReceipt { intent: second.id.clone(), closed: second_closed.id.clone() }],
        sealed: sealed(&self::roster(1).members[0]),
    });
    vec![roster, first, second, first_closed, second_closed, admission, attempt]
}

#[test]
fn worker_lifecycle_order_independent_exact_closure_and_scope_binding() {
    let mut records = complete_fixture();
    let inspected = validate_worker_records(BASE, &records).unwrap();
    assert!(!inspected.unresolved());
    assert_eq!(inspected.roster.as_deref(), Some(records[0].id.as_str()));
    assert_eq!((inspected.admitted_attempts, inspected.launch_intents, inspected.closed_workers,
        inspected.closed_attempts, inspected.successful_attempts), (1, 2, 2, 1, 1));
    assert_eq!(inspected.sealed[0].0, sealed(&roster(1).members[0]));
    assert_eq!(inspected.sealed[0].1, [1; 32]);
    records.reverse();
    assert!(!validate_worker_records(BASE, &records).unwrap().unresolved());
}

#[test]
fn worker_missing_evidence_never_upgrades_launch_or_attempt_to_closed() {
    let legacy = validate_worker_records(BASE, &[]).unwrap();
    assert!(legacy.roster.is_none());
    let records = complete_fixture();
    for selection in [vec![0], vec![0, 1], vec![0, 1, 2], vec![0, 1, 2, 4], vec![0, 1, 2, 3, 4, 5]] {
        let selected: Vec<_> = selection.into_iter().map(|i| changed(&records[i], |_| {})).collect();
        let inspected = validate_worker_records(BASE, &selected).unwrap();
        assert!(inspected.unresolved());
        assert_eq!(inspected.closed_attempts, 0);
    }
    // A parent closure without the descendant's exact closure is corrupt, not a partial proof.
    let orphan: Vec<_> = records.into_iter().enumerate().filter_map(|(i, r)| (i != 4).then_some(r)).collect();
    assert!(validate_worker_records(BASE, &orphan).is_err());
}

#[test]
fn worker_roster_and_launch_identity_reject_foreign_duplicate_and_noncanonical_data() {
    let cases = [
        (0, "future"), (0, "base"), (0, "incarnation"), (0, "duplicate-member"),
        (0, "alias-overlay"), (0, "foreign-owner"), (0, "gap-attempt"),
        (1, "future"), (1, "roster"), (1, "attempt"), (1, "self-parent"),
        (1, "missing-parent-field"), (1, "unknown"), (2, "gap-worker"),
    ];
    for (index, case) in cases {
        let mut records = complete_fixture();
        records[index] = changed(&records[index], |value| match case {
            "future" => value["lifecycle_version"] = json!(2),
            "base" => value["base"] = json!("1123456789abcdef0123456789abcdef"),
            "incarnation" => value["incarnation"] = json!("ABCDEF9876543210fedcba9876543210"),
            "duplicate-member" => {
                let member = value["members"][0].clone();
                value["members"].as_array_mut().unwrap().push(member);
            },
            "alias-overlay" => value["members"][0]["overlay"] = json!("overlay-01"),
            "foreign-owner" => value["members"][0]["owner"] = json!("parent-ish/task"),
            "gap-attempt" => value["members"][0]["attempt"] = json!(2),
            "roster" => value["roster"] = json!(id("another incarnation")),
            "attempt" => value["attempt"] = json!(2),
            "self-parent" => value["parent_worker"] = json!(1),
            "missing-parent-field" => { value.as_object_mut().unwrap().remove("parent_worker"); },
            "unknown" => value["extra"] = json!(true),
            "gap-worker" => value["worker"] = json!(3),
            _ => unreachable!(),
        });
        assert!(validate_worker_records(BASE, &records).is_err(), "case {case}");
    }
    let mut duplicate = complete_fixture();
    duplicate.push(changed(&duplicate[1], |_| {}));
    assert!(validate_worker_records(BASE, &duplicate).is_err());
    let mut unknown = complete_fixture();
    unknown[1].label = "speculation-worker-future".into();
    assert!(validate_worker_records(BASE, &unknown).is_err());
}

#[test]
fn worker_closure_rejects_unreleased_unknown_omitted_invented_or_rebound_children() {
    for case in ["ownership", "unknown", "self-intent", "omitted", "invented", "rebound", "panic-settled", "not-started"] {
        let mut records = complete_fixture();
        records[3] = changed(&records[3], |v| match case {
            "ownership" => v["ownership_released"] = json!(false),
            "unknown" => v["effect_state"] = json!({"state":"unknown_worker"}),
            "self-intent" => v["intent"] = json!(records[3].id),
            "omitted" => v["children"] = json!([]),
            "invented" => v["children"].as_array_mut().unwrap().push(json!({"intent":id("extra"),"closed":id("extra closed")})),
            "rebound" => v["children"][0]["closed"] = json!(id("another child closure")),
            "panic-settled" => v["result"] = json!("panicked"),
            "not-started" => v["result"] = json!("not_started"),
            _ => unreachable!(),
        });
        assert!(validate_worker_records(BASE, &records).is_err(), "case {case}");
    }
    let mut ambiguous = complete_fixture();
    let mut second = changed(&ambiguous[4], |v| v["result"] = json!("failed"));
    second.id = id("second terminal record");
    ambiguous.push(second);
    assert!(validate_worker_records(BASE, &ambiguous).is_err());
}

#[test]
fn worker_closed_admission_is_exact_and_terminal_does_not_mean_success() {
    for case in ["omitted", "extra", "reordered", "late-admission"] {
        let mut records = complete_fixture();
        records[5] = changed(&records[5], |v| match case {
            "omitted" => { v["intents"].as_array_mut().unwrap().pop(); },
            "extra" => v["intents"].as_array_mut().unwrap().push(json!(id("new worker"))),
            "reordered" => v["intents"].as_array_mut().unwrap().reverse(),
            "late-admission" => v["attempt"] = json!(2),
            _ => unreachable!(),
        });
        assert!(validate_worker_records(BASE, &records).is_err(), "case {case}");
    }
    for (index, field, value) in [(5, "driver_result", json!("failed")), (3, "result", json!("failed")),
        (4, "result", json!("not_started")), (4, "effect_state", json!({"state":"possibly_applied","record":null}))] {
        let mut records = complete_fixture();
        records[index] = changed(&records[index], |v| v[field] = value);
        let inspection = validate_worker_records(BASE, &records).unwrap();
        assert_eq!(inspection.closed_attempts, 1);
        assert_eq!(inspection.successful_attempts, 0);
        assert!(inspection.unresolved());
    }
}

#[test]
fn worker_attempt_closure_requires_exact_manifest_and_all_worker_receipts() {
    for case in ["ownership", "admission", "omitted", "reordered", "owner", "overlay", "base", "generation", "manifest"] {
        let mut records = complete_fixture();
        records[6] = changed(&records[6], |v| match case {
            "ownership" => v["ownership_released"] = json!(false),
            "admission" => v["admission"] = json!(id("foreign admission")),
            "omitted" => { v["workers"].as_array_mut().unwrap().pop(); },
            "reordered" => v["workers"].as_array_mut().unwrap().reverse(),
            "owner" => v["sealed"]["owner"] = json!("parent/foreign"),
            "overlay" => v["sealed"]["overlay"] = json!("overlay-2"),
            "base" => v["sealed"]["base"] = json!("1123456789abcdef0123456789abcdef"),
            "generation" => v["sealed"]["generation"] = json!(513),
            "manifest" => v["sealed"]["manifest"] = json!("../../source.rs"),
            _ => unreachable!(),
        });
        assert!(validate_worker_records(BASE, &records).is_err(), "case {case}");
    }
}

#[test]
fn worker_decoder_and_envelope_are_bounded_and_canonical() {
    let records = complete_fixture();
    for index in 0..records.len() {
        let mut reordered = changed(&records[index], |_| {});
        let value: Value = serde_json::from_slice(&reordered.metadata).unwrap();
        reordered.metadata = serde_json::to_vec_pretty(&value).unwrap();
        let mut selected: Vec<_> = records.iter().map(|r| changed(r, |_| {})).collect();
        selected[index] = reordered;
        assert!(validate_worker_records(BASE, &selected).is_err());
    }
    let mut images = complete_fixture();
    images[1].images.push(b"not allowed".to_vec());
    assert!(validate_worker_records(BASE, &images).is_err());
    let mut wrapper = complete_fixture();
    wrapper[1].owner = "foreign".into();
    assert!(validate_worker_records(BASE, &wrapper).is_err());
    let mut oversized = complete_fixture();
    oversized[1].metadata = vec![b' '; MAX_METADATA_BYTES + 1];
    assert!(validate_worker_records(BASE, &oversized).is_err());
    let mut duplicate_field = complete_fixture();
    let text = String::from_utf8(duplicate_field[1].metadata.clone()).unwrap();
    duplicate_field[1].metadata = text.replacen("\"worker\":1", "\"worker\":1,\"worker\":1", 1).into_bytes();
    assert!(validate_worker_records(BASE, &duplicate_field).is_err());
    let mut many = complete_fixture();
    many[5] = changed(&many[5], |v| v["intents"] = json!(vec![id("overflow"); MAX_WORKERS + 1]));
    assert!(validate_worker_records(BASE, &many).is_err());
}

#[test]
fn worker_empty_attempts_need_explicit_admission_and_closure_for_every_roster_member() {
    let roster_value = roster(2);
    let roster_record = record(ROSTER_LABEL, &roster_value);
    let mut records = vec![roster_record];
    for member in &roster_value.members {
        let admission = record(ADMISSION_LABEL, &AttemptAdmissionClosed {
            schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
            roster: records[0].id.clone(), attempt: member.attempt, driver_result: DriverResult::Completed,
            intents: Vec::new(),
        });
        let attempt = record(ATTEMPT_LABEL, &AttemptClosed {
            schema: 1, lifecycle_version: 1, base: BASE.into(), owner: OWNER.into(),
            roster: records[0].id.clone(), admission: admission.id.clone(), ownership_released: true,
            workers: Vec::new(), sealed: SealedManifestIdentity { generation: 0, ..sealed(member) },
        });
        records.push(admission);
        assert!(validate_worker_records(BASE, &records).unwrap().unresolved());
        records.push(attempt);
    }
    let inspected = validate_worker_records(BASE, &records).unwrap();
    assert!(!inspected.unresolved());
    assert_eq!(inspected.closed_attempts, 2);
    assert_eq!(inspected.closed_workers, 0);
    let rebound = changed(&records[4], |v| v["sealed"]["overlay"] = json!("overlay-1"));
    records[4] = rebound;
    assert!(validate_worker_records(BASE, &records).is_err());
}

#[test]
fn worker_integration_selection_requires_complete_exact_closed_lineage_not_just_matching_bytes() {
    let records = complete_fixture();
    let inspected = validate_worker_records(BASE, &records).unwrap();
    let identity = &inspected.sealed[0].0;
    let value = json!({"overlay_lineage_version":1,"base":BASE,"owner":OWNER,"overlays":[{
        "base":BASE,"owner":identity.owner,"id":identity.overlay,"generation":identity.generation,
        "manifest":identity.manifest,"author":format!("{}/{}", identity.owner, identity.overlay)
    }]});
    assert!(inspected.check_selected_lineage(&value).is_ok());
    for field in ["generation", "manifest", "owner", "id"] {
        let mut other = value.clone();
        other["overlays"][0][field] = match field {
            "generation" => json!(2),
            "manifest" => json!(id("other canonical sealed manifest")),
            "owner" => json!("parent/another"),
            "id" => json!("overlay-2"),
            _ => unreachable!(),
        };
        let owner = other["overlays"][0]["owner"].as_str().unwrap();
        let overlay = other["overlays"][0]["id"].as_str().unwrap();
        other["overlays"][0]["author"] = json!(format!("{owner}/{overlay}"));
        assert!(inspected.check_selected_lineage(&other).is_err(), "field {field}");
    }
    assert!(inspected.check_selected_lineage(&json!({"base":BASE,"owner":OWNER})).is_err());
    let mut unresolved = inspected;
    unresolved.admitted_attempts += 1;
    assert!(unresolved.check_selected_lineage(&value).is_err());
    // Legacy data remains readable, but cannot become a current physical closure receipt.
    let legacy = validate_worker_records(BASE, &[]).unwrap();
    assert!(legacy.check_selected_lineage(&json!({})).is_ok());
    assert!(legacy.roster.is_none());
}

#[test]
fn worker_scope_digest_binds_read_write_bits_paths_order_and_count() {
    let a = std::path::Path::new("src/a.rs");
    let b = std::path::Path::new("src/b.rs");
    let first = scope_sha256([("a", a, false), ("b", b, true)].into_iter()).unwrap();
    for bindings in [vec![("a", a, true), ("b", b, true)], vec![("a", b, false), ("b", b, true)],
        vec![("a", a, false)]] {
        assert_ne!(first, scope_sha256(bindings.into_iter()).unwrap());
    }
    assert!(scope_sha256([("b", b, true), ("a", a, false)].into_iter()).is_err());
    assert!(scope_sha256([("a", a, true), ("a", a, false)].into_iter()).is_err());
    assert!(scope_sha256(std::iter::empty()).is_err());
    assert!(scope_sha256([("a", std::path::Path::new("bad\npath"), true)].into_iter()).is_err());
}
