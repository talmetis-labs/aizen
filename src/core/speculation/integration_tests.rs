use super::*;
use crate::core::cancel::TurnCancel;
use crate::core::exec_ctx::{self, WorkspaceChanges};
use crate::core::file_placement::tests::TestDir;
use crate::core::native_scope::NativeWriteScope;
use crate::core::speculation::store::{InspectedRecord, OverlayHandle};
use crate::core::workspace_txn::{stable_key, with_test_lock_home, WorkspaceIdentity};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

// Every lock, journal, snapshot and source path is below an exclusively-created fixture root.
// Thread-local overrides never mutate AIZEN_HOME, process environment or production state.
struct Fixture {
    _temp: TestDir,
    work: PathBuf,
    home: PathBuf,
    store: PathBuf,
    identity: WorkspaceIdentity,
    changes: Arc<WorkspaceChanges>,
    parent: ExecutionContext,
    /// Serializes against every other test that takes the process-global native-effects gate
    /// (see [`crate::core::speculation::test_support`]). Reentrant, so a test may hold two
    /// fixtures without hanging.
    _gate_lock: crate::core::speculation::test_support::GateTestLock,
}

impl Fixture {
    fn new() -> Self {
        let gate_lock = crate::core::speculation::test_support::GateTestLock::acquire();
        let temp = TestDir::new();
        let work = temp.path().join("workspace");
        let home = temp.path().join("home");
        let store = temp.path().join("store");
        for path in [&work, &home, &store] { fs::create_dir(path).unwrap(); }
        let work = fs::canonicalize(work).unwrap();
        let identity = WorkspaceIdentity {
            repo_id: stable_key(&format!("repo:{}", work.display())),
            worktree_id: stable_key(&format!("worktree:{}", work.display())),
            canonical_root: work.clone(), canonical_git_dir: None, canonical_common_git_dir: None,
        };
        let changes = Arc::new(WorkspaceChanges::default());
        let parent = ExecutionContext::default().with_resource_scope("parent").with_workspace_changes(changes.clone());
        Self { _temp: temp, work, home, store, identity, changes, parent, _gate_lock: gate_lock }
    }

    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        with_test_lock_home(&self.home, || native_journal::with_test_storage(&self.home, || {
            exec_ctx::with_current(self.parent.clone(), f)
        }))
    }

    fn guards(&self) -> (WorkspaceWriterLease, PhysicalEffectLease) {
        let writer = WorkspaceWriterLease::acquire_identity_scoped(
            self.identity.clone(), Duration::ZERO, None, "integration fixture", "parent",
        ).unwrap();
        let physical = PhysicalEffectLease::exclusive(
            Duration::ZERO, None, "integration fixture", "parent",
        ).unwrap();
        (writer, physical)
    }

    fn scope(&self, reads: &[&str], writes: &[&str]) -> NativeWriteScope {
        NativeWriteScope::resolve_access(&self.work,
            &reads.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>(),
            &writes.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>(),
        ).unwrap()
    }

    fn capture(&self, scopes: &[NativeWriteScope]) -> BaseSnapshot {
        self.run(|| {
            let (writer, physical) = self.guards();
            BaseSnapshot::capture(&self.identity, scopes, &self.store, &writer, &physical).unwrap()
        })
    }

    fn overlay(&self, base: &BaseSnapshot, scope: &NativeWriteScope, owner: &str) -> OverlayHandle {
        base.create_overlay(owner, scope).unwrap()
    }

    fn rewrite(&self, overlay: &OverlayHandle, name: &str, bytes: &[u8]) {
        let body = overlay.body_lease(overlay.owner()).unwrap();
        let file = overlay.resolve(&self.work, Path::new(name)).unwrap();
        let before = overlay.snapshot(&file, &body).unwrap();
        overlay.compare_write(&file, &before, bytes, WriteProvenance::Rewrite, &body).unwrap();
    }

    fn replace(&self, overlay: &OverlayHandle, name: &str, old: &[u8], new: &[u8]) {
        let body = overlay.body_lease(overlay.owner()).unwrap();
        let file = overlay.resolve(&self.work, Path::new(name)).unwrap();
        let before = overlay.snapshot(&file, &body).unwrap();
        let bytes = before.bytes().unwrap();
        let start = bytes.windows(old.len()).position(|part| part == old).unwrap();
        let end = start + old.len();
        let mut after = bytes[..start].to_vec();
        after.extend_from_slice(new);
        after.extend_from_slice(&bytes[end..]);
        overlay.compare_write(&file, &before, &after, WriteProvenance::Localized { start, end }, &body).unwrap();
    }

    fn plan(&self, base: &BaseSnapshot, overlays: &[SealedOverlay]) -> IntegrationPlan {
        match IntegrationPlan::build(&self.parent, base, overlays, &Limits::default()).unwrap() {
            PlanningOutcome::Ready(plan) => plan,
            PlanningOutcome::Conflicted(conflicts) => panic!("unexpected conflict: {conflicts:?}"),
        }
    }

    fn apply(&self, plan: &IntegrationPlan, cancel: Option<&TurnCancel>) -> Result<IntegrationReceipt> {
        self.run(|| {
            let (writer, physical) = self.guards();
            plan.apply(&self.parent, &writer, &physical, cancel)
        })
    }
}

struct FaultReset;
impl FaultReset {
    fn set(fault: TestFault) -> Self {
        TEST_FAULT.with(|slot| { assert!(slot.get().is_none()); slot.set(Some(fault)); });
        Self
    }
}
impl Drop for FaultReset {
    fn drop(&mut self) { TEST_FAULT.with(|slot| slot.set(None)); }
}

fn record<'a>(records: &'a [InspectedRecord], label: &str) -> &'a InspectedRecord {
    records.iter().find(|record| record.label == label).unwrap()
}

fn json(record: &InspectedRecord) -> Value { serde_json::from_slice(&record.metadata).unwrap() }

const RUST_BASE: &[u8] = b"fn alpha() -> i32 {\n    101\n}\n\nfn beta() -> i32 {\n    202\n}\n";

#[test]
fn sealed_lineage_allows_distinct_overlays_owned_by_the_same_task() {
    let f = Fixture::new();
    for name in ["a.txt", "b.txt"] { fs::write(f.work.join(name), b"base").unwrap(); }
    let scope = f.scope(&[], &["a.txt", "b.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let a = f.overlay(&base, &scope, "parent/task");
    let b = f.overlay(&base, &scope, "parent/task");
    f.rewrite(&a, "a.txt", b"first");
    f.rewrite(&b, "b.txt", b"second");
    let sealed_a = a.seal(a.owner()).unwrap();
    let sealed_b = b.seal(b.owner()).unwrap();
    drop(a); drop(b);
    let plan = f.plan(&base, &[sealed_a, sealed_b]);
    f.apply(&plan, None).unwrap();
    let records = base.inspect_controller_records().unwrap();
    let meta = json(record(&records, "integration-prepared"));
    let identities = super::super::store::sealed_lineage(&meta).unwrap().unwrap();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0].owner, identities[1].owner);
    assert_ne!(identities[0].overlay, identities[1].overlay);
    let directory = base.directory().to_path_buf();
    drop(plan); drop(base);
    super::super::store::inspect_controller_records(&directory).unwrap();
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"first");
    assert_eq!(fs::read(f.work.join("b.txt")).unwrap(), b"second");
}

#[test]
fn prepared_restart_recomputes_single_and_merged_candidates_from_sealed_images() {
    for multiple in [false, true] {
        for mismatch in ["none", "postimage", "contributors", "omitted", "extra", "postimage-index",
            "preimage-index", "missing-index", "postimage-missing", "preimage-missing",
            "preimage-descriptor", "empty-images", "extra-images"] {
            let f = Fixture::new();
            fs::write(f.work.join("lib.rs"), RUST_BASE).unwrap();
            let scope = f.scope(&[], &["lib.rs"]);
            let base = f.capture(std::slice::from_ref(&scope));
            let a = f.overlay(&base, &scope, "parent/a");
            f.replace(&a, "lib.rs", b"101", b"111");
            let sealed_a = a.seal(a.owner()).unwrap();
            drop(a);
            let mut overlays = vec![sealed_a];
            if multiple {
                let b = f.overlay(&base, &scope, "parent/b");
                f.replace(&b, "lib.rs", b"202", b"222");
                overlays.push(b.seal(b.owner()).unwrap());
            }
            let plan = f.plan(&base, &overlays);
            let mut value: Value = serde_json::from_slice(&plan.prepared_meta).unwrap();
            let mut images = plan.prepared_images().unwrap();
            match mismatch {
                "postimage" => {
                    let file = &mut value["files"][0];
                    let index = file["postimage"]["index"].as_u64().unwrap() as usize;
                    images[index] = b"invented candidate, not the sealed result".to_vec();
                    let fingerprint = FileFingerprint::for_bytes(&images[index]);
                    let image = serde_json::json!({"exists":true,"byte_len":fingerprint.byte_len,"sha256":fingerprint.sha256});
                    file["after"] = image.clone();
                    file["postimage"] = image;
                    file["postimage"]["missing"] = false.into();
                    file["postimage"]["index"] = index.into();
                }
                "contributors" => value["files"][0]["contributors"] = serde_json::json!([]),
                "postimage-index" => value["files"][0]["postimage"]["index"] = 0.into(),
                "preimage-index" => value["files"][0]["preimage"]["index"] = 1.into(),
                "missing-index" => { value["files"][0]["postimage"].as_object_mut().unwrap().remove("index"); },
                "postimage-missing" => value["files"][0]["postimage"]["missing"] = true.into(),
                "preimage-missing" => value["files"][0]["preimage"]["missing"] = true.into(),
                "preimage-descriptor" => value["files"][0]["preimage"]["sha256"] = serde_json::json!(vec![0u8; 32]),
                "empty-images" => images.clear(),
                "extra-images" => images.push(b"unreferenced".to_vec()),
                "omitted" => value["files"] = serde_json::json!([]),
                "extra" => {
                    let duplicate = value["files"][0].clone();
                    value["files"].as_array_mut().unwrap().push(duplicate);
                }
                _ => {},
            }
            base.controller_record("parent", "integration-prepared", &serde_json::to_vec(&value).unwrap(),
                &images.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
            let directory = base.directory().to_path_buf();
            drop(plan); drop(overlays); drop(base);
            assert_eq!(super::super::store::inspect_controller_records(&directory).is_ok(), mismatch == "none",
                "multiple={multiple}, mismatch={mismatch}");
            assert_eq!(fs::read(f.work.join("lib.rs")).unwrap(), RUST_BASE);
        }
    }
}

#[test]
fn prepared_restart_absent_preimage_mapping_is_exact() {
    for mismatch in ["none", "index", "missing", "descriptor"] {
        let f = Fixture::new();
        let scope = f.scope(&[], &["new.txt"]);
        let base = f.capture(std::slice::from_ref(&scope));
        let overlay = f.overlay(&base, &scope, "parent/task");
        f.rewrite(&overlay, "new.txt", b"created");
        let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
        let mut value: Value = serde_json::from_slice(&plan.prepared_meta).unwrap();
        match mismatch {
            "index" => value["files"][0]["preimage"]["index"] = 0.into(),
            "missing" => value["files"][0]["preimage"]["missing"] = false.into(),
            "descriptor" => value["files"][0]["preimage"]["byte_len"] = 1.into(),
            _ => {},
        }
        let images = plan.prepared_images().unwrap();
        base.controller_record("parent", "integration-prepared", &serde_json::to_vec(&value).unwrap(),
            &images.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        let (result, _) = base.inspect_with_work_budget(u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(result.is_ok(), mismatch == "none", "{mismatch}");
        assert!(!f.work.join("new.txt").exists());
    }
}

#[test]
fn inspection_history_cache_and_cumulative_work_refuse_before_next_operation() {
    for unique in [false, true] {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt"] { fs::write(f.work.join(name), b"abcdef").unwrap(); }
        let scope = f.scope(&[], &["a.txt", "b.txt"]);
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.overlay(&base, &scope, "parent/a");
        f.replace(&a, "a.txt", b"b", b"X");
        f.replace(&a, "a.txt", b"X", b"Y");
        let mut sealed = vec![a.seal(a.owner()).unwrap()];
        if unique {
            let b = f.overlay(&base, &scope, "parent/b");
            f.replace(&b, "b.txt", b"c", b"Z");
            sealed.push(b.seal(b.owner()).unwrap());
        }
        let plan = f.plan(&base, &sealed);
        let mut metadata: Value = serde_json::from_slice(&plan.prepared_meta).unwrap();
        let images = plan.prepared_images().unwrap();
        for nonce in 0..3 {
            metadata["inspection_test_nonce"] = nonce.into();
            base.controller_record("parent", "integration-prepared", &serde_json::to_vec(&metadata).unwrap(),
                &images.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        }
        let (result, stats) = base.inspect_with_work_budget(u64::MAX, u64::MAX, u64::MAX);
        assert!(result.is_ok());
        assert_eq!(stats.histories, if unique { 2 } else { 1 }, "history validation must be content-addressed, not per reference");
        assert!(stats.read_bytes > 0 && stats.comparison_bytes > 0 && stats.merge_work > 0);
        let (result, exact) = base.inspect_with_work_budget(stats.read_bytes, stats.comparison_bytes, stats.merge_work);
        assert!(result.is_ok());
        assert_eq!(exact.object_reads, stats.object_reads);
        for (read, compare, merge) in [
            (0, u64::MAX, u64::MAX),
            (stats.read_bytes - 1, u64::MAX, u64::MAX),
            (u64::MAX, stats.comparison_bytes - 1, u64::MAX),
            (u64::MAX, u64::MAX, stats.merge_work - 1),
        ] {
            let (result, refused) = base.inspect_with_work_budget(read, compare, merge);
            assert!(result.unwrap_err().to_string().contains("work budget exceeded"));
            assert!(refused.read_bytes <= read && refused.comparison_bytes <= compare && refused.merge_work <= merge);
            if read == 0 { assert_eq!(refused.object_reads, 0, "charge before even the first object read"); }
        }
        // Repeated content addresses must not bypass identity/record metadata validation.
        // Each fixture appends one bad reference after repeated valid references.
        let mismatch = if unique { "observations" } else { "identity" };
        let mut invalid = metadata.clone();
        invalid["inspection_test_nonce"] = mismatch.into();
        if mismatch == "identity" {
            invalid["overlays"][0]["generation"] = 0.into();
        } else {
            invalid["observations"] = serde_json::json!([]);
        }
        base.controller_record("parent", "integration-prepared", &serde_json::to_vec(&invalid).unwrap(),
            &images.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        assert!(base.inspect_with_work_budget(u64::MAX, u64::MAX, u64::MAX).0.is_err());
        for name in ["a.txt", "b.txt"] { assert_eq!(fs::read(f.work.join(name)).unwrap(), b"abcdef"); }
    }
}

#[test]
fn worker_restart_crosslinks_exact_sealed_scope_generation_and_complete_selection() {
    use crate::core::speculation::workers::{self, AttemptAdmissionClosed, AttemptClosed,
        AttemptMember, DriverResult, NativeWorkerKind, WorkerClosed, WorkerIntent, WorkerReceipt,
        WorkerResult, WorkerRoster};
    use crate::core::speculation::verification::DurableEffectState;

    for mismatch in ["none", "scope", "generation", "manifest", "subset"] {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt"] { fs::write(f.work.join(name), b"base").unwrap(); }
        let scopes = [f.scope(&[], &["a.txt"]), f.scope(&["a.txt"], &["b.txt"])];
        let base = f.capture(&scopes);
        let mut sealed = Vec::new();
        let mut members = Vec::new();
        for (index, scope) in scopes.iter().enumerate() {
            let owner = format!("parent/task-{}/attempt/1", index + 1);
            let overlay = f.overlay(&base, scope, &owner);
            f.rewrite(&overlay, if index == 0 { "a.txt" } else { "b.txt" }, b"private");
            let closed = overlay.seal(&owner).unwrap();
            let scope_sha256 = workers::scope_sha256(scope.resources().iter().map(|resource| {
                (resource.key(), resource.path(), scope.write_keys().iter().any(|key| key == resource.key()))
            })).unwrap();
            members.push(AttemptMember { attempt: index as u32 + 1, owner,
                overlay: overlay.id().to_owned(), scope_sha256 });
            sealed.push(closed);
        }
        if mismatch == "scope" { members[0].scope_sha256[0] ^= 1; }
        let selected = if mismatch == "subset" { &sealed[..1] } else { &sealed[..] };
        let plan = f.plan(&base, selected);
        let images = plan.prepared_images().unwrap();
        // Reserve the entire fixture before inserting deliberately corrupt evidence. An ordinary
        // new reservation re-inspects the inventory and correctly refuses a corrupt prefix.
        let reservation = std::cell::RefCell::new(base.reserve_controller_records(
            "parent", 10, 1024 * 1024, images.iter().map(Vec::len).sum(),
        ).unwrap());
        let publish = |label: &str, value: &dyn Fn() -> Vec<u8>| {
            base.controller_record_reserved(&mut reservation.borrow_mut(), label, &value(), &[])
                .unwrap().as_str().to_owned()
        };
        let roster = WorkerRoster { schema: 1, lifecycle_version: 1, base: base.id().to_owned(),
            owner: "parent".into(), incarnation: "fedcba9876543210fedcba9876543210".into(), members };
        let roster_id = publish(workers::ROSTER_LABEL, &|| serde_json::to_vec(&roster).unwrap());
        for (index, member) in roster.members.iter().enumerate() {
            let intent = WorkerIntent { schema: 1, lifecycle_version: 1, base: base.id().to_owned(),
                owner: "parent".into(), roster: roster_id.clone(), attempt: member.attempt, worker: 1,
                parent_worker: None, kind: NativeWorkerKind::Write, prepared_sha256: [index as u8 + 1; 32] };
            let intent_id = publish(workers::INTENT_LABEL, &|| serde_json::to_vec(&intent).unwrap());
            let closed = WorkerClosed { schema: 1, lifecycle_version: 1, base: base.id().to_owned(),
                owner: "parent".into(), roster: roster_id.clone(), intent: intent_id.clone(),
                result: WorkerResult::Returned, effect_state: DurableEffectState::Settled,
                ownership_released: true, children: Vec::new() };
            let closed_id = publish(workers::CLOSED_LABEL, &|| serde_json::to_vec(&closed).unwrap());
            let admission = AttemptAdmissionClosed { schema: 1, lifecycle_version: 1,
                base: base.id().to_owned(), owner: "parent".into(), roster: roster_id.clone(),
                attempt: member.attempt, driver_result: DriverResult::Completed, intents: vec![intent_id.clone()] };
            let admission_id = publish(workers::ADMISSION_LABEL, &|| serde_json::to_vec(&admission).unwrap());
            let mut identity = sealed[index].manifest_identity().unwrap();
            if index == 0 {
                match mismatch {
                    "generation" => identity.generation += 1,
                    "manifest" => identity.manifest = sealed[1].manifest_identity().unwrap().manifest,
                    _ => {},
                }
            }
            let attempt = AttemptClosed { schema: 1, lifecycle_version: 1, base: base.id().to_owned(),
                owner: "parent".into(), roster: roster_id.clone(), admission: admission_id,
                ownership_released: true, workers: vec![WorkerReceipt { intent: intent_id, closed: closed_id }],
                sealed: identity };
            publish(workers::ATTEMPT_LABEL, &|| serde_json::to_vec(&attempt).unwrap());
        }
        base.controller_record_reserved(&mut reservation.borrow_mut(), "integration-prepared", &plan.prepared_meta,
            &images.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        let directory = base.directory().to_path_buf();
        drop(reservation); drop(plan); drop(sealed); drop(base);
        // Historical inspection must use immutable objects, not reinterpret a changed live tree.
        fs::write(f.work.join("a.txt"), b"foreign live bytes").unwrap();
        let inspected = super::super::store::inspect_controller_records(&directory);
        assert_eq!(inspected.is_ok(), mismatch == "none", "{mismatch}: {inspected:?}");
        assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"foreign live bytes");
        assert_eq!(fs::read(f.work.join("b.txt")).unwrap(), b"base");
    }
}

#[test]
fn same_file_distinct_functions_merge_apply_and_exact_durable_image_mapping() {
    let f = Fixture::new();
    fs::write(f.work.join("lib.rs"), RUST_BASE).unwrap();
    let scope = f.scope(&[], &["lib.rs"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let a = f.overlay(&base, &scope, "parent/task/a");
    let b = f.overlay(&base, &scope, "parent/workflow/b/task/inner");
    f.replace(&a, "lib.rs", b"101", b"111");
    f.replace(&b, "lib.rs", b"202", b"222");
    let a = a.seal(a.owner()).unwrap();
    let b = b.seal(b.owner()).unwrap();
    let identities = [a.manifest_identity().unwrap(), b.manifest_identity().unwrap()];
    let plan = f.plan(&base, &[b.clone(), a.clone()]);
    let reversed = f.plan(&base, &[a, b]);
    assert_eq!(plan.files(), reversed.files());
    assert_eq!(plan.prepared_meta, reversed.prepared_meta);
    assert_eq!(fs::read(f.work.join("lib.rs")).unwrap(), RUST_BASE);
    assert!(!f.changes.changed());
    let expected = b"fn alpha() -> i32 {\n    111\n}\n\nfn beta() -> i32 {\n    222\n}\n";
    assert_eq!(plan.files()[0].bytes(), expected);
    assert_eq!(plan.files()[0].contributors().len(), 2);
    assert_eq!(plan.read_dependencies().len(), 2);
    assert!(plan.read_dependencies().iter().all(|concern| concern.peer_authors.len() == 1));
    assert!(plan.files()[0].contributors().iter().all(|author| author.contains("/overlay-")));
    let receipt = f.apply(&plan, None).unwrap();
    assert_eq!(receipt.state, ReceiptState::AppliedUnverified);
    assert!(receipt.postimage_verified);
    assert_eq!(receipt.read_dependencies, plan.read_dependencies());
    assert_eq!(receipt.applied_paths, vec![f.work.join("lib.rs").to_str().unwrap().to_owned()]);
    assert_eq!(fs::read(f.work.join("lib.rs")).unwrap(), expected);
    assert_eq!(f.changes.paths(), vec![f.work.join("lib.rs")]);
    let records = base.inspect_controller_records().unwrap();
    let prepared = record(&records, "integration-prepared");
    assert_eq!(prepared.id, receipt.prepared.as_str());
    let meta = json(prepared);
    assert_eq!(meta["state"], "prepared");
    assert_eq!(meta["base"], base.id());
    assert_eq!(meta["overlay_lineage_version"], 1);
    assert_eq!(meta["overlays"].as_array().unwrap().len(), 2);
    for identity in &identities {
        let lineage = meta["overlays"].as_array().unwrap().iter()
            .find(|lineage| lineage["id"] == identity.overlay).unwrap();
        assert_eq!(lineage["base"], identity.base);
        assert_eq!(lineage["owner"], identity.owner);
        assert_eq!(lineage["generation"], identity.generation);
        assert_eq!(lineage["manifest"], identity.manifest);
    }
    assert_eq!(meta["observations"].as_array().unwrap().len(), 2);
    assert_eq!(meta["read_dependencies"].as_array().unwrap().len(), 2);
    let file = &meta["files"][0];
    assert_eq!(file["path"], f.work.join("lib.rs").to_str().unwrap());
    assert_eq!(file["key"], plan.files()[0].key());
    assert_eq!(file["lineage"].as_array().unwrap().len(), 2);
    for (field, bytes) in [("preimage", RUST_BASE), ("postimage", expected.as_slice())] {
        let index = file[field]["index"].as_u64().unwrap() as usize;
        assert_eq!(prepared.images[index], bytes);
        assert_eq!(file[field]["missing"], false);
        assert_eq!(file[field]["exists"], true);
        assert_eq!(file[field]["byte_len"], bytes.len());
        assert_eq!(file[field]["sha256"], serde_json::to_value(FileFingerprint::for_bytes(bytes).sha256).unwrap());
    }
    let meta = json(record(&records, "integration-receipt"));
    assert_eq!(meta["state"], "applied-unverified");
    assert_eq!(meta["prepared"], receipt.prepared.as_str());
    assert_eq!(meta["postimage_verified"], true);
    assert_eq!(meta["native_journal_linkage"], 2);
    let coverage: native_journal::BatchIdentity = serde_json::from_value(meta["native_coverage"].clone()).unwrap();
    coverage.validate().unwrap();
    assert_eq!(meta["native_coverage"], json(prepared)["native_coverage"]);
    let file = &plan.files()[0];
    let after = FileFingerprint::for_bytes(file.bytes());
    let proofs = f.run(|| native_journal::inspect_existing_batch(&coverage, &[native_journal::RecordClaim {
        id: 1, expected_owner: "parent", resource: file.resource(), before: file.expected(), after: &after,
    }])).unwrap();
    assert_eq!(proofs, vec![native_journal::RecordProof { id: 1, status: native_journal::RecordProofStatus::SettledApplied }]);
    assert_eq!(meta["files"][0]["state"], "applied");
    assert_eq!(meta["files"][0]["native_record_id"], 1);
}

#[test]
fn integration_receipt_uses_actual_journal_ids_after_unrelated_settled_write() {
    let f = Fixture::new();
    for name in ["a.txt", "b.txt", "unrelated.txt"] {
        fs::write(f.work.join(name), b"base").unwrap();
    }
    f.run(|| {
        let (_writer, _physical) = f.guards();
        let resource = NativeResource::resolve(&f.work.join("unrelated.txt")).unwrap();
        let _lock = LockSet::acquire(vec![LockRequest::new(
            LockClass::Resource,
            crate::core::workspace_txn::resource_lock("native-file", resource.key()),
            LockMode::Exclusive, "integration linkage fixture",
        )], Duration::ZERO, None, &LockOwner::new("parent", "integration linkage fixture")).unwrap();
        native_journal::apply(&resource, &FileFingerprint::for_bytes(b"base"), b"unrelated", "parent").unwrap();
    });
    let scope = f.scope(&[], &["a.txt", "b.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    for name in ["a.txt", "b.txt"] { f.rewrite(&overlay, name, b"after"); }
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    f.apply(&plan, None).unwrap();
    let records = base.inspect_controller_records().unwrap();
    let receipt = json(record(&records, "integration-receipt"));
    assert_eq!(receipt["native_journal_linkage"], 2);
    assert_eq!(receipt["native_coverage"], json(record(&records, "integration-prepared"))["native_coverage"]);
    for (index, file) in plan.files().iter().enumerate() {
        assert_eq!(receipt["files"][index]["key"], file.key());
        assert_eq!(receipt["files"][index]["native_record_id"], (index + 2) as u64);
        assert_eq!(fs::read(file.resource().path()).unwrap(), b"after");
    }
    assert_eq!(fs::read(f.work.join("unrelated.txt")).unwrap(), b"unrelated");
}

#[test]
fn empty_integration_still_persists_and_checks_real_native_batch_coverage() {
    let f = Fixture::new();
    fs::write(f.work.join("read.txt"), b"immutable").unwrap();
    let scope = f.scope(&["read.txt"], &[]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/reader");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    assert!(plan.files().is_empty());
    let receipt = f.apply(&plan, None).unwrap();
    assert!(receipt.postimage_verified && receipt.applied_paths.is_empty());
    assert!(!f.changes.changed());
    let records = base.inspect_controller_records().unwrap();
    let prepared = json(record(&records, "integration-prepared"));
    let receipt = json(record(&records, "integration-receipt"));
    assert_eq!(receipt["native_journal_linkage"], 2);
    assert_eq!(prepared["overlay_lineage_version"], 1);
    assert_eq!(prepared["overlays"][0]["generation"], 0);
    assert_eq!(receipt["native_coverage"], prepared["native_coverage"]);
    let coverage: native_journal::BatchIdentity = serde_json::from_value(prepared["native_coverage"].clone()).unwrap();
    assert!(f.run(|| native_journal::inspect_existing_batch(&coverage, &[])).unwrap().is_empty());
    let mut missing = coverage.clone();
    missing.batch[0] ^= 1;
    assert!(f.run(|| native_journal::inspect_existing_batch(&missing, &[])).is_err());
    assert_eq!(fs::read(f.work.join("read.txt")).unwrap(), b"immutable");
}

#[test]
fn admitted_fence_without_prepared_publication_never_starts_source_mutation() {
    let f = Fixture::new();
    fs::write(f.work.join("file.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["file.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/writer");
    f.rewrite(&overlay, "file.txt", b"after");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    let _fault = FaultReset::set(TestFault::BeforePrepared);
    let error = f.apply(&plan, None).unwrap_err();
    assert!(error.downcast_ref::<IntegrationUnsettled>().is_none());
    assert_eq!(fs::read(f.work.join("file.txt")).unwrap(), b"base");
    assert!(!f.changes.changed());
    let records = base.inspect_controller_records().unwrap();
    assert!(!records.iter().any(|record| record.label == "integration-prepared" || record.label == "integration-receipt"));
    // The admitted fence is retained, not rolled back or interpreted as a published native record.
    assert!(f.home.join("native-journal-v1/records.bin").exists());
    let file = &plan.files()[0];
    assert!(f.run(|| native_journal::inspect_existing_records(&[native_journal::RecordClaim {
        id: 1, expected_owner: "parent", resource: file.resource(), before: file.expected(),
        after: &FileFingerprint::for_bytes(file.bytes()),
    }])).is_err());
}

#[test]
fn same_function_conflict_is_durable_and_never_applies_other_files() {
    let f = Fixture::new();
    fs::write(f.work.join("lib.rs"), RUST_BASE).unwrap();
    fs::write(f.work.join("other.txt"), b"before").unwrap();
    let scope = f.scope(&[], &["lib.rs", "other.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let a = f.overlay(&base, &scope, "parent/a");
    let b = f.overlay(&base, &scope, "parent/b");
    f.replace(&a, "lib.rs", b"101", b"111");
    f.replace(&b, "lib.rs", b"101", b"333");
    f.rewrite(&a, "other.txt", b"candidate");
    let a = a.seal(a.owner()).unwrap();
    let b = b.seal(b.owner()).unwrap();
    let conflicts = match IntegrationPlan::build(&f.parent, &base, &[a, b], &Limits::default()).unwrap() {
        PlanningOutcome::Conflicted(conflicts) => conflicts,
        PlanningOutcome::Ready(_) => panic!("competing edits must conflict"),
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(fs::read(f.work.join("lib.rs")).unwrap(), RUST_BASE);
    assert_eq!(fs::read(f.work.join("other.txt")).unwrap(), b"before");
    assert!(!f.changes.changed());
    let records = base.inspect_controller_records().unwrap();
    let conflict = record(&records, "integration-conflict");
    assert!(conflict.metadata.len() <= MAX_RECORD_META);
    assert_eq!(json(conflict)["overlay_lineage_version"], 1);
    assert!(crate::core::speculation::store::sealed_lineage(&json(conflict)).unwrap().is_some());
    assert_eq!(json(conflict)["conflicts"], serde_json::to_value(conflicts).unwrap());
    assert!(!records.iter().any(|record| record.label == "integration-prepared"));
}

#[test]
fn rewrite_policy_is_per_change_and_cannot_merge_competing_results() {
    let f = Fixture::new();
    fs::write(f.work.join("lib.rs"), RUST_BASE).unwrap();
    let scope = f.scope(&[], &["lib.rs"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let a = f.overlay(&base, &scope, "parent/a");
    let b = f.overlay(&base, &scope, "parent/b");
    f.rewrite(&a, "lib.rs", b"fn replaced() {}\n");
    f.replace(&b, "lib.rs", b"202", b"222");
    assert_eq!(change_operation(&a.seal(a.owner()).unwrap().changes().unwrap()[0]), Operation::Rewrite);
    assert_eq!(change_operation(&b.seal(b.owner()).unwrap().changes().unwrap()[0]), Operation::Localized);
    let result = IntegrationPlan::build(&f.parent, &base,
        &[a.seal(a.owner()).unwrap(), b.seal(b.owner()).unwrap()], &Limits::default()).unwrap();
    let PlanningOutcome::Conflicted(conflicts) = result else { panic!("rewrite must conflict") };
    assert_eq!(conflicts[0].kind, "WholeRewrite");
    assert_eq!(fs::read(f.work.join("lib.rs")).unwrap(), RUST_BASE);
}

#[test]
fn external_drift_of_any_captured_file_prevents_all_live_writes() {
    let f = Fixture::new();
    fs::write(f.work.join("target.txt"), b"base").unwrap();
    fs::write(f.work.join("observed.txt"), b"observed").unwrap();
    let scope = f.scope(&["observed.txt"], &["target.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "target.txt", b"candidate");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    fs::write(f.work.join("observed.txt"), b"external change").unwrap();
    let error = f.apply(&plan, None).unwrap_err();
    assert!(error.downcast_ref::<IntegrationUnsettled>().is_none());
    assert_eq!(fs::read(f.work.join("target.txt")).unwrap(), b"base");
    assert_eq!(fs::read(f.work.join("observed.txt")).unwrap(), b"external change");
    assert!(!f.changes.changed());
    assert!(base.inspect_controller_records().unwrap().is_empty());
}

#[test]
fn absent_to_empty_is_creation_not_noop_and_images_preserve_missing() {
    let f = Fixture::new();
    let scope = f.scope(&[], &["created.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "created.txt", b"");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    assert!(!plan.files()[0].expected().exists);
    let receipt = f.apply(&plan, None).unwrap();
    assert!(receipt.postimage_verified);
    assert!(f.work.join("created.txt").is_file());
    assert!(fs::read(f.work.join("created.txt")).unwrap().is_empty());
    let records = base.inspect_controller_records().unwrap();
    let prepared = record(&records, "integration-prepared");
    let meta = json(prepared);
    assert_eq!(meta["files"][0]["preimage"]["index"], Value::Null);
    assert_eq!(meta["files"][0]["preimage"]["missing"], true);
    assert_eq!(meta["files"][0]["preimage"]["exists"], false);
    assert_eq!(meta["files"][0]["postimage"]["index"], 0);
    assert_eq!(meta["files"][0]["postimage"]["missing"], false);
    assert_eq!(prepared.images, vec![Vec::<u8>::new()]);
    assert_eq!(f.changes.paths(), vec![f.work.join("created.txt")]);
}

#[test]
fn already_cancelled_admission_has_no_effect_or_prepared_record() {
    let f = Fixture::new();
    fs::write(f.work.join("a.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["a.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "a.txt", b"after");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    let cancel = TurnCancel::new();
    cancel.cancel();
    assert!(f.apply(&plan, Some(&cancel)).is_err());
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"base");
    assert!(!f.changes.changed());
    assert!(base.inspect_controller_records().unwrap().is_empty());
}

#[test]
fn partial_effect_is_attributed_durable_and_never_rolled_back() {
    let f = Fixture::new();
    for name in ["a.txt", "b.txt"] { fs::write(f.work.join(name), b"base").unwrap(); }
    let scope = f.scope(&[], &["a.txt", "b.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    for name in ["a.txt", "b.txt"] { f.rewrite(&overlay, name, b"after"); }
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    let first = plan.files()[0].resource().path().to_path_buf();
    let second = plan.files()[1].resource().path().to_path_buf();
    let _fault = FaultReset::set(TestFault::AfterApplied(1));
    let error = f.apply(&plan, None).unwrap_err();
    let unsettled = error.downcast_ref::<IntegrationUnsettled>().unwrap();
    assert!(unsettled.receipt.is_some());
    assert_eq!(fs::read(&first).unwrap(), b"after");
    assert_eq!(fs::read(&second).unwrap(), b"base");
    assert_eq!(f.changes.paths(), vec![first]);
    let records = base.inspect_controller_records().unwrap();
    assert_eq!(record(&records, "integration-prepared").images.len(), 4);
    let receipt = json(record(&records, "integration-receipt"));
    assert_eq!(receipt["state"], "partial");
    assert_eq!(receipt["applied"], 1);
    assert_eq!(receipt["total"], 2);
    assert_eq!(receipt["postimage_verified"], false);
    assert_eq!(receipt["files"][0]["state"], "applied");
    assert_eq!(receipt["files"][0]["native_record_id"], 1);
    assert_eq!(receipt["files"][1]["state"], "not-applied");
    assert!(receipt["files"][1]["native_record_id"].is_null());
    let coverage: native_journal::BatchIdentity = serde_json::from_value(receipt["native_coverage"].clone()).unwrap();
    assert_eq!(receipt["native_coverage"], json(record(&records, "integration-prepared"))["native_coverage"]);
    let file = &plan.files()[0];
    let after = FileFingerprint::for_bytes(file.bytes());
    let claim = native_journal::RecordClaim {
        id: 1, expected_owner: "parent", resource: file.resource(), before: file.expected(), after: &after,
    };
    assert!(f.run(|| native_journal::inspect_existing_records(&[claim])).is_ok());
    let claim = native_journal::RecordClaim {
        id: 1, expected_owner: "parent", resource: file.resource(), before: file.expected(), after: &after,
    };
    assert!(f.run(|| native_journal::inspect_existing_batch(&coverage, &[claim])).is_err(),
        "a valid first record cannot cover the missing second intent");
    assert!(f.run(|| native_journal::inspect_existing_batch(&coverage, &[])).is_err());
}

#[test]
fn native_journal_uncertain_write_is_attributed_and_never_rolled_back() {
    let f = Fixture::new();
    fs::write(f.work.join("a.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["a.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "a.txt", b"after");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    let error = native_journal::with_test_after_write_failure(|| f.apply(&plan, None)).unwrap_err();
    let unsettled = error.downcast_ref::<IntegrationUnsettled>().unwrap();
    assert!(unsettled.possibly_applied_record.is_some());
    assert!(unsettled.receipt.is_some());
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"after");
    assert_eq!(f.changes.paths(), vec![f.work.join("a.txt")]);
    let records = base.inspect_controller_records().unwrap();
    let receipt = json(record(&records, "integration-receipt"));
    assert_eq!(receipt["state"], "partial");
    assert_eq!(receipt["applied"], 0);
    assert_eq!(receipt["files"][0]["state"], "possibly-applied");
    assert_eq!(receipt["possibly_applied_record"].as_u64(), unsettled.possibly_applied_record);
    assert_eq!(receipt["files"][0]["native_record_id"].as_u64(), unsettled.possibly_applied_record);
}

#[test]
fn native_prepared_failure_retains_pending_identity_even_when_receipt_publication_fails() {
    for fail_receipt in [false, true] {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt"] { fs::write(f.work.join(name), b"base").unwrap(); }
        let scope = f.scope(&[], &["a.txt", "b.txt"]);
        let base = f.capture(std::slice::from_ref(&scope));
        let overlay = f.overlay(&base, &scope, "parent/a");
        for name in ["a.txt", "b.txt"] { f.rewrite(&overlay, name, b"after"); }
        let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
        let first = plan.files()[0].resource();
        let _fault = fail_receipt.then(|| FaultReset::set(TestFault::BeforeReceipt));
        let error = native_journal::with_test_prepared_failure(|| f.apply(&plan, None)).unwrap_err();
        let unsettled = error.downcast_ref::<IntegrationUnsettled>().unwrap();
        assert_eq!(unsettled.possibly_applied_record, Some(1));
        assert_eq!(unsettled.receipt.is_none(), fail_receipt);
        for name in ["a.txt", "b.txt"] {
            assert_eq!(fs::read(f.work.join(name)).unwrap(), b"base", "no mutation was attempted");
        }
        assert_eq!(f.changes.paths(), vec![first.path().to_path_buf()], "pending paths remain conservatively attributed");
        let records = base.inspect_controller_records().unwrap();
        let prepared = record(&records, "integration-prepared");
        assert_eq!(prepared.id, unsettled.prepared.as_str());
        assert_eq!(prepared.images, vec![b"base".to_vec(), b"after".to_vec(), b"base".to_vec(), b"after".to_vec()]);
        if !fail_receipt {
            let receipt = json(record(&records, "integration-receipt"));
            assert_eq!(receipt["state"], "partial");
            assert_eq!(receipt["applied"], 0);
            assert_eq!(receipt["postimage_verified"], false);
            assert_eq!(receipt["possibly_applied_record"], 1);
            assert_eq!(receipt["files"][0]["state"], "possibly-applied");
            assert_eq!(receipt["files"][0]["native_record_id"], 1);
            assert_eq!(receipt["files"][1]["state"], "not-applied");
            assert!(receipt["files"][1]["native_record_id"].is_null());
        } else {
            assert!(!records.iter().any(|record| record.label == "integration-receipt"));
        }
        let journal_path = f.home.join("native-journal-v1/records.bin");
        let before = fs::read(&journal_path).unwrap();
        let proofs = f.run(|| native_journal::inspect_existing_records(&[
            native_journal::RecordClaim {
                id: 1, expected_owner: "parent", resource: first,
                before: &FileFingerprint::for_bytes(b"base"), after: &FileFingerprint::for_bytes(b"after"),
            },
        ])).unwrap();
        assert_eq!(proofs, vec![native_journal::RecordProof { id: 1, status: native_journal::RecordProofStatus::Prepared }]);
        assert_eq!(fs::read(journal_path).unwrap(), before);
    }
}

#[test]
fn failed_postimage_read_still_publishes_partial_receipt_and_retains_effect() {
    let f = Fixture::new();
    fs::write(f.work.join("a.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["a.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "a.txt", b"after");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    let _fault = FaultReset::set(TestFault::Postimage);
    let error = f.apply(&plan, None).unwrap_err();
    assert!(error.downcast_ref::<IntegrationUnsettled>().unwrap().receipt.is_some());
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"after");
    assert_eq!(f.changes.paths(), vec![f.work.join("a.txt")]);
    let records = base.inspect_controller_records().unwrap();
    let receipt = json(record(&records, "integration-receipt"));
    assert_eq!(receipt["state"], "partial");
    assert_eq!(receipt["applied"], 1);
    assert_eq!(receipt["postimage_verified"], false);
    assert_eq!(receipt["stop"], "postimage-check");
}

#[test]
fn receipt_persistence_failure_keeps_prepared_images_and_typed_effect() {
    let f = Fixture::new();
    fs::write(f.work.join("a.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["a.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "a.txt", b"after");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    let _fault = FaultReset::set(TestFault::BeforeReceipt);
    let error = f.apply(&plan, None).unwrap_err();
    let unsettled = error.downcast_ref::<IntegrationUnsettled>().unwrap();
    assert!(unsettled.receipt.is_none());
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"after");
    assert_eq!(f.changes.paths(), vec![f.work.join("a.txt")]);
    let records = base.inspect_controller_records().unwrap();
    let prepared = record(&records, "integration-prepared");
    assert_eq!(prepared.id, unsettled.prepared.as_str());
    assert_eq!(prepared.images, vec![b"base".to_vec(), b"after".to_vec()]);
    assert!(!records.iter().any(|record| record.label == "integration-receipt"));
}

#[test]
fn parentage_uses_slash_boundary_and_rejects_duplicate_or_foreign_handles() {
    let f = Fixture::new();
    fs::write(f.work.join("a.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["a.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let outside = f.overlay(&base, &scope, "parent-other/a");
    f.rewrite(&outside, "a.txt", b"outside");
    assert!(IntegrationPlan::build(&f.parent, &base, &[outside.seal(outside.owner()).unwrap()], &Limits::default()).is_err());
    let nested = f.overlay(&base, &scope, "parent/workflow/task/attempt");
    f.rewrite(&nested, "a.txt", b"nested");
    let nested = nested.seal(nested.owner()).unwrap();
    assert!(IntegrationPlan::build(&f.parent, &base, std::slice::from_ref(&nested), &Limits::default()).is_ok());
    assert!(IntegrationPlan::build(&f.parent, &base, &[nested.clone(), nested.clone()], &Limits::default()).is_err());
    let other_base = f.capture(&[scope]);
    assert!(IntegrationPlan::build(&f.parent, &other_base, &[nested], &Limits::default()).is_err());
    assert_eq!(fs::read(f.work.join("a.txt")).unwrap(), b"base");
}

#[test]
fn batch_capacity_preflight_rejects_integration_before_any_candidate_write() {
    let f = Fixture::new();
    fs::write(f.work.join("target.txt"), b"base").unwrap();
    let scope = f.scope(&[], &["target.txt"]);
    let base = f.capture(std::slice::from_ref(&scope));
    let overlay = f.overlay(&base, &scope, "parent/a");
    f.rewrite(&overlay, "target.txt", b"candidate");
    let plan = f.plan(&base, &[overlay.seal(overlay.owner()).unwrap()]);
    // Fill the REAL native journal in this fixture's own storage via actual settled writes.
    // This distinct resource is outside the captured base, so base drift cannot mask capacity.
    let filler = f.work.join("journal-fill.txt");
    fs::write(&filler, b"initial").unwrap();
    f.run(|| {
        let (_writer, _physical) = f.guards();
        let resource = NativeResource::resolve(&filler).unwrap();
        let _lock = LockSet::acquire(vec![LockRequest::new(
            LockClass::Resource,
            crate::core::workspace_txn::resource_lock("native-file", resource.key()),
            LockMode::Exclusive, "integration capacity fixture",
        )], Duration::ZERO, None, &LockOwner::new("parent", "integration capacity fixture")).unwrap();
        for index in 0..128 {
            let expected = persist::fingerprint(&filler).unwrap();
            native_journal::apply(&resource, &expected, format!("journal-{index}").as_bytes(), "parent").unwrap();
        }
    });
    let effects_before = f.changes.paths();
    assert_eq!(effects_before, vec![filler]);
    let error = f.apply(&plan, None).unwrap_err();
    assert!(error.downcast_ref::<IntegrationUnsettled>().is_none());
    assert_eq!(fs::read(f.work.join("target.txt")).unwrap(), b"base");
    assert_eq!(f.changes.paths(), effects_before);
    let records = base.inspect_controller_records().unwrap();
    assert!(!records.iter().any(|record| record.label == "integration-prepared"));
    assert!(!records.iter().any(|record| record.label == "integration-receipt"));
}
