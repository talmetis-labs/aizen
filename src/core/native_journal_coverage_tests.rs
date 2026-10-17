use super::*;
use crate::core::repo_lock::LockMode;
use crate::core::workspace_txn::NativeResourceLease;

struct Fixture {
    _root: crate::core::file_placement::tests::TestDir,
    work: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = crate::core::file_placement::tests::TestDir::new();
        let work = root.path().join("work");
        let home = root.path().join("home");
        fs::create_dir(&work).unwrap();
        fs::create_dir(&home).unwrap();
        Self { _root: root, work, home }
    }

    fn resource(&self, name: &str) -> NativeResource {
        NativeResource::resolve(&self.work.join(name)).unwrap()
    }

    fn lease(&self, resources: &[&NativeResource]) -> NativeResourceLease {
        let requests: Vec<_> = resources.iter().map(|resource| ((*resource).clone(), LockMode::Exclusive)).collect();
        NativeResourceLease::acquire(&self.work, &requests, Duration::ZERO, None, "coverage-fixture").unwrap()
    }

    fn run(&self, f: impl FnOnce()) {
        crate::core::workspace_txn::with_test_lock_home(&self.home, || {
            with_test_storage(&self.home, || {
                let ctx = ExecutionContext::default().with_resource_scope("coverage-parent");
                exec_ctx::with_current(ctx, f);
            });
        });
    }

    fn path(&self) -> PathBuf { self.home.join("native-journal-v1/records.bin") }
}

fn claim<'a>(id: u64, resource: &'a NativeResource, before: &'a FileFingerprint, after: &'a FileFingerprint) -> RecordClaim<'a> {
    RecordClaim { id, expected_owner: "coverage-parent", resource, before, after }
}

// Deliberately bypass validation so decoder rejection is exercised, not only encoder rejection.
fn unchecked_v2(journal: &Journal) -> Vec<u8> {
    let mut bytes = MAGIC_V2.to_vec();
    bytes.extend_from_slice(&journal.epoch.unwrap());
    put_u32(&mut bytes, journal.fences.len() as u32);
    for fence in &journal.fences {
        let metadata = serde_json::to_vec(fence).unwrap();
        put_u32(&mut bytes, metadata.len() as u32);
        bytes.extend_from_slice(&metadata);
    }
    put_u32(&mut bytes, journal.records.len() as u32);
    for record in &journal.records {
        let metadata = serde_json::to_vec(&record.meta).unwrap();
        put_u32(&mut bytes, metadata.len() as u32);
        bytes.extend_from_slice(&metadata);
        for image in [&record.before, &record.after] {
            if let Some(image) = image {
                put_u32(&mut bytes, image.len() as u32);
                bytes.extend_from_slice(image);
            } else { put_u32(&mut bytes, u32::MAX); }
        }
    }
    bytes
}

#[test]
fn identity_is_strict_nonzero_and_unknown_journal_versions_fail_closed() {
    for identity in [
        BatchIdentity { epoch: [0; 32], batch: [1; 32] },
        BatchIdentity { epoch: [1; 32], batch: [0; 32] },
    ] {
        assert!(identity.validate().is_err());
        assert!(inspect_existing_batch(&identity, &[]).is_err());
    }
    let identity = BatchIdentity { epoch: [1; 32], batch: [2; 32] };
    let mut value = serde_json::to_value(&identity).unwrap();
    value["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<BatchIdentity>(value).is_err());
    assert!(Journal::decode(b"AZNJ\0\0\0\x03").is_err());
    let mut zero_epoch = MAGIC_V2.to_vec();
    zero_epoch.extend_from_slice(&[0; 32]);
    put_u32(&mut zero_epoch, 0);
    put_u32(&mut zero_epoch, 0);
    assert!(Journal::decode(&zero_epoch).is_err());
}

#[test]
fn legacy_roundtrip_upgrade_preserves_records_without_retroactive_coverage() {
    let f = Fixture::new();
    f.run(|| {
        let legacy = f.resource("legacy.txt");
        let selected = f.resource("selected.txt");
        let _lease = f.lease(&[&legacy, &selected]);
        let missing = FileFingerprint::missing();
        let after = FileFingerprint::for_bytes(b"after");
        apply(&legacy, &missing, b"legacy", "coverage-parent").unwrap();
        let v1 = fs::read(f.path()).unwrap();
        assert!(v1.starts_with(MAGIC));
        let journal = Journal::decode(&v1).unwrap();
        assert!(journal.epoch.is_none() && journal.fences.is_empty());
        assert_eq!(journal.encode().unwrap(), v1);
        assert!(journal.records[0].meta.coverage.is_none());
        let legacy_metadata = metadata_bytes(&journal.records[0].meta).unwrap();
        let legacy_json: serde_json::Value = serde_json::from_slice(&legacy_metadata).unwrap();
        assert!(!legacy_json.as_object().unwrap().contains_key("coverage"));
        let mut batch = begin_covered_batch("coverage-parent", &[(&selected, &missing, b"after")]).unwrap();
        let identity = batch.coverage_identity().unwrap().clone();
        assert_eq!(batch.apply_with_record(&selected, &missing, b"after").unwrap().native_record_id, Some(2));
        drop(batch);
        let v2 = fs::read(f.path()).unwrap();
        assert!(v2.starts_with(MAGIC_V2));
        let upgraded = Journal::decode(&v2).unwrap();
        assert_eq!(upgraded.encode().unwrap(), v2);
        assert_eq!(upgraded.records.len(), 2);
        assert_eq!(metadata_bytes(&upgraded.records[0].meta).unwrap(), legacy_metadata);
        assert!(upgraded.records[0].meta.coverage.is_none());
        assert_eq!(inspect_existing_batch(&identity, &[claim(2, &selected, &missing, &after)]).unwrap(),
            vec![RecordProof { id: 2, status: RecordProofStatus::SettledApplied }]);
        assert!(inspect_existing_batch(&identity, &[claim(1, &legacy, &missing, &after)]).is_err());
        let ordinary = begin_batch("coverage-parent", &[]).unwrap();
        assert!(ordinary.coverage_identity().is_none());
    });
}

#[test]
fn coverage_is_complete_ordered_noop_free_and_tolerates_unrelated_history() {
    let f = Fixture::new();
    f.run(|| {
        let unrelated = f.resource("unrelated.txt");
        let a = f.resource("a.txt");
        let noop = f.resource("noop.txt");
        let b = f.resource("b.txt");
        fs::write(noop.path(), b"same").unwrap();
        let _lease = f.lease(&[&unrelated, &a, &noop, &b]);
        let missing = FileFingerprint::missing();
        let same = FileFingerprint::for_bytes(b"same");
        let after_a = FileFingerprint::for_bytes(b"after-a");
        let after_b = FileFingerprint::for_bytes(b"after-b");
        let other = apply(&unrelated, &missing, b"unrelated", "coverage-parent").unwrap();
        let mut batch = begin_covered_batch("coverage-parent", &[
            (&a, &missing, b"after-a"), (&noop, &same, b"same"), (&b, &missing, b"after-b"),
        ]).unwrap();
        let identity = batch.coverage_identity().unwrap().clone();
        assert_eq!(batch.journal.fences[0].intents.iter().map(|intent| intent.id).collect::<Vec<_>>(), vec![2, 3]);
        assert!(batch.journal.records.iter().all(|record| record.meta.coverage.is_none()));
        let first = batch.apply_with_record(&a, &missing, b"after-a").unwrap();
        assert_eq!(first.native_record_id, Some(2));
        assert_eq!(batch.apply_with_record(&noop, &same, b"same").unwrap().native_record_id, None);
        assert_eq!(batch.apply_with_record(&b, &missing, b"after-b").unwrap().native_record_id, Some(3));
        drop(batch);
        apply(&unrelated, &other, b"later unrelated", "coverage-parent").unwrap();
        let before = fs::read(f.path()).unwrap();
        let claims = [claim(2, &a, &missing, &after_a), claim(3, &b, &missing, &after_b)];
        let proofs = inspect_existing_batch(&identity, &claims).unwrap();
        assert_eq!(proofs.iter().map(|proof| proof.id).collect::<Vec<_>>(), vec![2, 3]);
        assert!(inspect_existing_batch(&identity, &[]).is_err());
        assert!(inspect_existing_batch(&identity, &claims[..1]).is_err());
        assert!(inspect_existing_batch(&identity, &[
            claim(3, &b, &missing, &after_b), claim(2, &a, &missing, &after_a),
        ]).is_err());
        assert!(inspect_existing_batch(&identity, &[
            claim(2, &a, &missing, &after_a), claim(2, &a, &missing, &after_a),
        ]).is_err());
        assert!(inspect_existing_batch(&identity, &[
            claim(2, &a, &missing, &after_a), claim(3, &b, &missing, &after_b),
            claim(1, &unrelated, &missing, &other),
        ]).is_err());
        assert!(inspect_existing_batch(&identity, &[
            RecordClaim { expected_owner: "wrong-owner", ..claim(2, &a, &missing, &after_a) },
            claim(3, &b, &missing, &after_b),
        ]).is_err());
        assert!(inspect_existing_batch(&identity, &[
            claim(2, &b, &missing, &after_a), claim(3, &a, &missing, &after_b),
        ]).is_err());
        assert!(inspect_existing_batch(&identity, &[
            claim(2, &a, &missing, &after_b), claim(3, &b, &missing, &after_b),
        ]).is_err());
        let wrong_epoch = BatchIdentity { epoch: [1; 32], batch: identity.batch };
        assert!(inspect_existing_batch(&wrong_epoch, &claims).is_err());
        let wrong_batch = BatchIdentity { epoch: identity.epoch, batch: [1; 32] };
        assert!(inspect_existing_batch(&wrong_batch, &claims).is_err());
        // History proof does not read the current source, even if its image exceeds journal limits.
        fs::write(a.path(), vec![b'x'; MAX_SOURCE_BYTES + 1]).unwrap();
        assert!(inspect_existing_batch(&identity, &claims).is_ok());
        assert_eq!(fs::read(f.path()).unwrap(), before);
    });
}

#[test]
fn empty_covered_batch_requires_its_exact_existing_fence_and_nonblocking_lock() {
    let f = Fixture::new();
    f.run(|| {
        let fake = BatchIdentity { epoch: [1; 32], batch: [2; 32] };
        assert!(inspect_existing_batch(&fake, &[]).is_err());
        assert!(!f.home.join("native-journal-v1").exists());
        let batch = begin_covered_batch("coverage-parent", &[]).unwrap();
        let identity = batch.coverage_identity().unwrap().clone();
        let start = std::time::Instant::now();
        assert!(inspect_existing_batch(&identity, &[]).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(batch);
        let before = fs::read(f.path()).unwrap();
        assert!(inspect_existing_batch(&identity, &[]).unwrap().is_empty());
        assert_eq!(fs::read(f.path()).unwrap(), before);
        let storage = Storage::open().unwrap();
        let mut recreated = storage.load().unwrap();
        recreated.epoch = Some(random_identity().unwrap());
        storage.save(&recreated).unwrap();
        drop(storage);
        assert!(inspect_existing_batch(&identity, &[]).is_err());
        let missing_fence = BatchIdentity { epoch: recreated.epoch.unwrap(), batch: [3; 32] };
        assert!(inspect_existing_batch(&missing_fence, &[]).is_err());
        fs::remove_file(f.path()).unwrap();
        let replacement = begin_covered_batch("coverage-parent", &[]).unwrap();
        let replacement_identity = replacement.coverage_identity().unwrap().clone();
        assert_ne!(replacement_identity.epoch, identity.epoch);
        drop(replacement);
        assert!(inspect_existing_batch(&identity, &[]).is_err());
        assert!(inspect_existing_batch(&replacement_identity, &[]).unwrap().is_empty());
    });
}

#[test]
fn before_and_after_fence_publication_never_touch_source() {
    for point in [Fault::BeforeFence, Fault::SaveBeforePublish, Fault::SaveAfterPublish, Fault::AfterFence, Fault::Random] {
        let f = Fixture::new();
        f.run(|| {
            let resource = f.resource("a.txt");
            let _lease = f.lease(&[&resource]);
            let missing = FileFingerprint::missing();
            let error = match with_test_fault(point, || begin_covered_batch("coverage-parent", &[(&resource, &missing, b"after")])) {
                Ok(_) => panic!("fence publication fault must stop admission"),
                Err(error) => error,
            };
            assert!(error.downcast_ref::<PossiblyApplied>().is_none());
            assert!(!resource.path().exists());
            let uncertain = error.downcast_ref::<FencePublicationUncertain>();
            if matches!(point, Fault::SaveBeforePublish | Fault::SaveAfterPublish | Fault::AfterFence) {
                uncertain.unwrap().identity.validate().unwrap();
            } else {
                assert!(uncertain.is_none());
            }
            if matches!(point, Fault::SaveAfterPublish | Fault::AfterFence) {
                let journal = Journal::decode(&fs::read(f.path()).unwrap()).unwrap();
                assert!(journal.records.is_empty());
                assert_eq!(journal.fences.len(), 1);
                let identity = BatchIdentity { epoch: journal.epoch.unwrap(), batch: journal.fences[0].batch };
                assert_eq!(uncertain.unwrap().identity, identity);
                assert!(inspect_existing_batch(&identity, &[]).is_err());
            } else {
                assert!(!f.path().exists());
            }
        });
    }
}

#[test]
fn partial_or_abandoned_fence_reserves_all_ids_and_missing_intents_stay_unresolved() {
    for apply_first in [false, true] {
        let f = Fixture::new();
        f.run(|| {
            let a = f.resource("a.txt");
            let b = f.resource("b.txt");
            let unrelated = f.resource("unrelated.txt");
            let _lease = f.lease(&[&a, &b, &unrelated]);
            let missing = FileFingerprint::missing();
            let after = FileFingerprint::for_bytes(b"after");
            let mut batch = begin_covered_batch("coverage-parent", &[
                (&a, &missing, b"after"), (&b, &missing, b"after"),
            ]).unwrap();
            let identity = batch.coverage_identity().unwrap().clone();
            if apply_first { batch.apply(&a, &missing, b"after").unwrap(); }
            drop(batch);
            assert!(inspect_existing_batch(&identity, &[]).is_err());
            assert!(inspect_existing_batch(&identity, &[
                claim(1, &a, &missing, &after), claim(2, &b, &missing, &after),
            ]).is_err());
            apply(&unrelated, &missing, b"direct", "coverage-parent").unwrap();
            let journal = Journal::decode(&fs::read(f.path()).unwrap()).unwrap();
            assert_eq!(journal.records.last().unwrap().meta.id, 3);
            assert!(journal.records.last().unwrap().meta.coverage.is_none());
            assert_eq!(journal.next_id().unwrap(), 4);
            assert!(inspect_existing_batch(&identity, &[]).is_err());
        });
    }
}

#[test]
fn empty_fence_publication_uncertainty_preserves_inspectable_identity() {
    for point in [Fault::SaveBeforePublish, Fault::SaveAfterPublish, Fault::AfterFence] {
        let f = Fixture::new();
        f.run(|| {
            let error = match with_test_fault(point, || begin_covered_batch("coverage-parent", &[])) {
                Ok(_) => panic!("fence publication fault must stop admission"),
                Err(error) => error,
            };
            let identity = &error.downcast_ref::<FencePublicationUncertain>().unwrap().identity;
            identity.validate().unwrap();
            let display = format!("{error:#}");
            assert!(!display.contains("injected"));
            assert!(display.contains("no source effect or retry authority"));
            if point == Fault::SaveBeforePublish {
                assert!(inspect_existing_batch(identity, &[]).is_err());
                assert!(!f.path().exists());
            } else {
                assert!(inspect_existing_batch(identity, &[]).unwrap().is_empty());
            }
            assert!(fs::read_dir(&f.work).unwrap().next().is_none());
        });
    }
}

#[test]
fn missing_fenced_intent_blocks_direct_batch_noop_and_undo_even_with_matching_bytes() {
    for apply_first in [false, true] {
        for foreign_postimage in [false, true] {
            let f = Fixture::new();
            f.run(|| {
                let a = f.resource("a.txt");
                let b = f.resource("b.txt");
                let _lease = f.lease(&[&a, &b]);
                let missing = FileFingerprint::missing();
                let original = apply(&b, &missing, b"original", "coverage-parent").unwrap();
                let mut batch = begin_covered_batch("coverage-parent", &[
                    (&a, &missing, b"after"), (&b, &original, b"after"),
                ]).unwrap();
                let identity = batch.coverage_identity().unwrap().clone();
                if apply_first { batch.apply(&a, &missing, b"after").unwrap(); }
                drop(batch);
                if foreign_postimage { fs::write(b.path(), b"after").unwrap(); }
                let expected_bytes = fs::read(b.path()).unwrap();
                let expected = FileFingerprint::for_bytes(&expected_bytes);
                let journal_before = fs::read(f.path()).unwrap();
                // There is no durable per-file record to reconcile. Neither image proves closure.
                assert!(reconcile(&b).unwrap().is_empty());
                assert!(apply(&b, &expected, b"new", "coverage-parent").is_err());
                assert!(apply(&b, &expected, &expected_bytes, "coverage-parent").is_err());
                assert!(begin_batch("coverage-parent", &[(&b, &expected, b"new")]).is_err());
                assert!(begin_covered_batch("coverage-parent", &[(&b, &expected, b"new")]).is_err());
                let parent = exec_ctx::current().unwrap();
                assert!(undo_owner(&parent, &b, "coverage-parent").is_err());
                assert_eq!(fs::read(b.path()).unwrap(), expected_bytes);
                assert_eq!(fs::read(f.path()).unwrap(), journal_before);
                assert_eq!(a.path().exists(), apply_first);
                assert!(inspect_existing_batch(&identity, &[]).is_err());
            });
        }
    }
}

#[test]
fn missing_fenced_intent_checks_historical_parent_binding() {
    let f = Fixture::new();
    f.run(|| {
        let resource = f.resource("a.txt");
        let _lease = f.lease(&[&resource]);
        let missing = FileFingerprint::missing();
        drop(begin_covered_batch("coverage-parent", &[(&resource, &missing, b"after")]).unwrap());
        let storage = Storage::open().unwrap();
        let mut journal = storage.load().unwrap();
        journal.fences[0].intents[0].binding.parent_identity = (u64::MAX, u64::MAX);
        storage.save(&journal).unwrap();
        drop(storage);
        let before = fs::read(f.path()).unwrap();
        let error = apply(&resource, &missing, b"after", "coverage-parent").unwrap_err();
        assert!(error.to_string().contains("historical parent identity changed"));
        assert_eq!(fs::read(f.path()).unwrap(), before);
        assert!(!resource.path().exists());
    });
}

#[test]
fn first_record_publication_uncertainty_retains_assigned_identity() {
    for point in [Fault::SaveBeforePublish, Fault::SaveAfterPublish, Fault::Prepared, Fault::Write] {
        let f = Fixture::new();
        f.run(|| {
            let resource = f.resource("a.txt");
            let _lease = f.lease(&[&resource]);
            let missing = FileFingerprint::missing();
            let after = FileFingerprint::for_bytes(b"after");
            let mut batch = begin_covered_batch("coverage-parent", &[(&resource, &missing, b"after")]).unwrap();
            let identity = batch.coverage_identity().unwrap().clone();
            let error = with_test_fault(point, || batch.apply(&resource, &missing, b"after")).unwrap_err();
            assert_eq!(error.downcast_ref::<PossiblyApplied>().unwrap().record_id, 1);
            assert!(batch.apply(&resource, &missing, b"after").is_err());
            drop(batch);
            assert_eq!(resource.path().exists(), point == Fault::Write);
            if point == Fault::SaveBeforePublish {
                assert!(inspect_existing_batch(&identity, &[claim(1, &resource, &missing, &after)]).is_err());
                assert!(inspect_existing_batch(&identity, &[]).is_err());
            } else {
                assert_eq!(inspect_existing_batch(&identity, &[claim(1, &resource, &missing, &after)]).unwrap(),
                    vec![RecordProof { id: 1, status: RecordProofStatus::Prepared }]);
            }
        });
    }
}

#[test]
fn coverage_proofs_preserve_nonterminal_and_reconciled_statuses_without_settling() {
    for (point, reconcile_before, status) in [
        (Fault::Applied, false, RecordProofStatus::Applied),
        (Fault::Prepared, true, RecordProofStatus::SettledNotApplied),
    ] {
        let f = Fixture::new();
        f.run(|| {
            let resource = f.resource("a.txt");
            let _lease = f.lease(&[&resource]);
            let missing = FileFingerprint::missing();
            let after = FileFingerprint::for_bytes(b"after");
            let mut batch = begin_covered_batch("coverage-parent", &[(&resource, &missing, b"after")]).unwrap();
            let identity = batch.coverage_identity().unwrap().clone();
            assert!(with_test_fault(point, || batch.apply(&resource, &missing, b"after")).is_err());
            drop(batch);
            if reconcile_before { reconcile(&resource).unwrap(); }
            let before = fs::read(f.path()).unwrap();
            assert_eq!(inspect_existing_batch(&identity, &[claim(1, &resource, &missing, &after)]).unwrap(),
                vec![RecordProof { id: 1, status }]);
            assert_eq!(fs::read(f.path()).unwrap(), before);
        });
    }
}

#[test]
fn fence_and_record_quotas_are_preflighted_without_source_effects() {
    let f = Fixture::new();
    f.run(|| {
        for _ in 0..MAX_FENCES { drop(begin_covered_batch("coverage-parent", &[]).unwrap()); }
        let before = fs::read(f.path()).unwrap();
        assert!(begin_covered_batch("coverage-parent", &[]).is_err());
        assert_eq!(fs::read(f.path()).unwrap(), before);
    });
    let f = Fixture::new();
    f.run(|| {
        let resources: Vec<_> = (0..MAX_RECORDS).map(|n| f.resource(&format!("{n}.txt"))).collect();
        let refs: Vec<_> = resources.iter().collect();
        let _lease = f.lease(&refs);
        let missing = FileFingerprint::missing();
        let writes: Vec<_> = resources.iter().map(|resource| (resource, &missing, b"after".as_slice())).collect();
        drop(begin_covered_batch("coverage-parent", &writes).unwrap());
        let extra = f.resource("extra.txt");
        assert!(begin_covered_batch("coverage-parent", &[(&extra, &missing, b"after")]).is_err());
        assert!(resources.iter().all(|resource| !resource.path().exists()));
        assert!(!extra.path().exists());
        assert_eq!(Journal::decode(&fs::read(f.path()).unwrap()).unwrap().reserved_records(), MAX_RECORDS);
    });
    let f = Fixture::new();
    f.run(|| {
        let resources: Vec<_> = (0..8).map(|n| f.resource(&format!("large-{n}.txt"))).collect();
        let before = vec![b'a'; MAX_SOURCE_BYTES];
        let after = vec![b'b'; MAX_SOURCE_BYTES];
        for resource in &resources { fs::write(resource.path(), &before).unwrap(); }
        let refs: Vec<_> = resources.iter().collect();
        let _lease = f.lease(&refs);
        let expected = FileFingerprint::for_bytes(&before);
        let writes: Vec<_> = resources.iter().map(|resource| (resource, &expected, after.as_slice())).collect();
        assert!(begin_covered_batch("coverage-parent", &writes).is_err());
        assert!(!f.path().exists());
        assert!(resources.iter().all(|resource| fs::read(resource.path()).unwrap() == before));
        drop(begin_covered_batch("coverage-parent", &writes[..7]).unwrap());
    });
}

#[test]
fn malformed_unknown_or_missing_tags_and_fence_metadata_fail_closed() {
    let f = Fixture::new();
    f.run(|| {
        let resource = f.resource("a.txt");
        let _lease = f.lease(&[&resource]);
        let missing = FileFingerprint::missing();
        let mut batch = begin_covered_batch("coverage-parent", &[(&resource, &missing, b"after")]).unwrap();
        batch.apply(&resource, &missing, b"after").unwrap();
        drop(batch);
        let bytes = fs::read(f.path()).unwrap();
        let mutate = |f: &dyn Fn(&mut Journal)| {
            let mut journal = Journal::decode(&bytes).unwrap();
            f(&mut journal);
            assert!(journal.encode().is_err());
            assert!(Journal::decode(&unchecked_v2(&journal)).is_err());
        };
        mutate(&|journal| journal.records[0].meta.coverage = None);
        mutate(&|journal| journal.records[0].meta.coverage.as_mut().unwrap().batch = [9; 32]);
        mutate(&|journal| journal.records[0].meta.coverage.as_mut().unwrap().ordinal = 1);
        mutate(&|journal| journal.fences[0].version = 2);
        mutate(&|journal| journal.fences[0].batch = [0; 32]);
        mutate(&|journal| journal.fences[0].owner = "other-owner".into());
        mutate(&|journal| journal.fences[0].intents[0].binding.parent_identity = (0, 0));
        mutate(&|journal| journal.records[0].meta.id = 2);
        mutate(&|journal| {
            let duplicate = serde_json::from_slice(&serde_json::to_vec(&journal.fences[0]).unwrap()).unwrap();
            journal.fences.push(duplicate);
        });
        let journal = Journal::decode(&bytes).unwrap();
        let mut value = serde_json::to_value(&journal.fences[0]).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<BatchFence>(value).is_err());
        let mut value = serde_json::to_value(&journal.records[0].meta).unwrap();
        value["coverage"]["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Metadata>(value).is_err());
    });
}
