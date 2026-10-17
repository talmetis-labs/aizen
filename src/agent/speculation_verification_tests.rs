use super::*;
use crate::agent::tools::ToolEffectState;
use crate::agent::verify_gate::VerifyGateResult;
use crate::core::exec_ctx;
use crate::core::file_placement::tests::TestDir;
use crate::core::speculation::store::{InspectedRecord, WriteProvenance};
use crate::core::workspace_txn::with_test_lock_home;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

// Publications use the real parent admission/storage path. Only the opaque gate is substituted;
// these fixtures test lifecycle/effect accounting, not process settlement or command correctness.
struct Fixture {
    _temp: TestDir,
    home: PathBuf,
    parent: ExecutionContext,
    cohort: Arc<Cohort>,
    sealed: Vec<ReleasedOverlay>,
}

impl Fixture {
    fn new() -> Self {
        let temp = TestDir::new();
        let work = temp.path().join("workspace");
        let home = temp.path().join("home");
        let store = temp.path().join("store");
        for path in [&work, &home, &store] { fs::create_dir(path).unwrap(); }
        let work = work.canonicalize().unwrap();
        fs::write(work.join("owned.rs"), "fn owned() -> i32 {\n    10\n}\n").unwrap();
        let identity = WorkspaceIdentity::discover(&work).unwrap();
        assert_eq!(identity.canonical_root, work);
        assert!(identity.canonical_git_dir.is_none());
        let scope = NativeWriteScope::resolve(&work, &["owned.rs".into()]).unwrap();
        let parent = ExecutionContext::default().with_resource_scope("lifecycle-parent");
        let base = with_test_lock_home(&home, || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(identity.clone(), Duration::ZERO,
                None, "verification lifecycle fixture", "lifecycle-parent").unwrap();
            let physical = PhysicalEffectLease::exclusive(Duration::ZERO, None,
                "verification lifecycle fixture", "lifecycle-parent").unwrap();
            BaseSnapshot::capture(&identity, std::slice::from_ref(&scope), &store, &writer, &physical).unwrap()
        });
        let overlay = base.create_overlay("lifecycle-parent/child", &scope).unwrap();
        let body = overlay.body_lease(overlay.owner()).unwrap();
        let file = overlay.resolve(&work, Path::new("owned.rs")).unwrap();
        let before = overlay.snapshot(&file, &body).unwrap();
        let bytes = before.bytes().unwrap();
        let start = bytes.windows(2).position(|part| part == b"10").unwrap();
        let after = String::from_utf8(bytes.to_vec()).unwrap().replacen("10", "11", 1);
        overlay.compare_write(&file, &before, after.as_bytes(),
            WriteProvenance::Localized { start, end: start + 2 }, &body).unwrap();
        drop(body);
        let released = super::super::speculative_attempt::release_idle_for_test(&work, &overlay, &scope).unwrap();
        let cohort = Arc::new(Cohort {
            parent: parent.resource_scope(), base, binding: None, members: Mutex::new(vec![overlay.clone()]),
            lifecycle: Mutex::new(None),
            integration_started: Mutex::new(false), applied_identity: Mutex::new(None),
            verification_reservation: Mutex::new(None),
            verification_test: VerificationTest { lock_home: Some(home.clone()), fault: Mutex::new(None) },
        });
        let sealed = vec![cohort.seal_released(&parent, &overlay, released).unwrap()];
        Self { _temp: temp, home, parent, cohort, sealed }
    }

    fn integrate(&self) -> Result<IntegrationOutcome> {
        with_test_lock_home(&self.home, || crate::core::native_journal::with_test_storage(&self.home, || {
            exec_ctx::with_current(self.parent.clone(), || self.cohort.integrate(&self.parent, &self.sealed, None))
        }))
    }

    fn records(&self) -> Vec<InspectedRecord> { self.cohort.base.inspect_controller_records().unwrap() }

    fn fault(&self, point: VerificationFault) { *self.cohort.verification_test.fault.lock().unwrap() = Some(point); }

    fn assert_live_postimage(&self) {
        assert_eq!(fs::read(self.cohort.base.identity().canonical_root.join("owned.rs")).unwrap(),
            b"fn owned() -> i32 {\n    11\n}\n");
    }
}

#[test]
fn speculation_parent_seal_refuses_foreign_parent_handle_and_live_private_body() {
    let fixture = Fixture::new();
    let root = &fixture.cohort.base.identity().canonical_root;
    let scope = NativeWriteScope::resolve(root, &["owned.rs".into()]).unwrap();
    let owner = format!("{}/fresh", fixture.parent.resource_scope());
    let overlay = fixture.cohort.overlay(&owner, &scope).unwrap();
    let foreign = fixture.cohort.base.create_overlay(&owner, &scope).unwrap();
    assert!(fixture.cohort.seal_after_driver(&fixture.parent, &foreign).is_err());
    let other_parent = fixture.parent.with_resource_scope("different-parent");
    assert!(fixture.cohort.seal_after_driver(&other_parent, &overlay).is_err());
    let body = overlay.body_lease(&owner).unwrap();
    assert!(fixture.cohort.seal_after_driver(&fixture.parent, &overlay).is_err());
    drop(body);
    let released = super::super::speculative_attempt::release_idle_for_test(root, &overlay, &scope).unwrap();
    let sealed = fixture.cohort.seal_released(&fixture.parent, &overlay, released).unwrap();
    assert_eq!(sealed.sealed.manifest_identity().unwrap().overlay, overlay.id());
    assert!(overlay.body_lease(&owner).is_err(), "released overlays cannot acquire new physical bodies");
    assert_eq!(fs::read(root.join("owned.rs")).unwrap(), b"fn owned() -> i32 {\n    10\n}\n");
}

fn gate_result(effect_state: ToolEffectState) -> VerifyGateResult {
    VerifyGateResult { passed: true, command: "owned-lifecycle-gate".into(), output: String::new(),
        duration_ms: 0, stable: true, effect_state }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Isolated fixture HOME, never a production admission guard.
async fn speculation_verification_start_is_durable_before_gate_and_guards_do_not_cross_await() {
    let _home = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
    let outcome = fixture.integrate().unwrap();
    let mut start_id = None;
    let raw = fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None, || async {
        let records = fixture.records();
        let starts: Vec<_> = records.iter().filter(|record| record.label == START_LABEL).collect();
        assert_eq!(starts.len(), 1);
        let started: VerificationStarted = serde_json::from_slice(&starts[0].metadata).unwrap();
        started.validate().unwrap();
        assert_eq!(started.prepared, outcome.prepared.as_str());
        assert_eq!(started.integration, outcome.receipt.as_str());
        start_id = Some(starts[0].id.clone());
        // A retained start's guards cannot authorize this fresh physical body or block it.
        with_test_lock_home(&fixture.home, || {
            let _writer = WorkspaceWriterLease::acquire_identity_scoped(fixture.cohort.base.identity().clone(),
                Duration::ZERO, None, "fresh owned gate body", &fixture.parent.resource_scope()).unwrap();
            let _physical = PhysicalEffectLease::exclusive(Duration::ZERO, None,
                "fresh owned gate body", &fixture.parent.resource_scope()).unwrap();
        });
        tokio::task::yield_now().await;
        Some(gate_result(ToolEffectState::Settled))
    }).await.unwrap();
    let summary = crate::core::speculation::verification::parse_success_summary(&raw, fixture.cohort.base.id()).unwrap();
    assert_eq!(Some(summary.verification_started.as_str()), start_id.as_deref());
    let records = fixture.records();
    let finished: VerificationFinished = serde_json::from_slice(&records.iter()
        .find(|record| record.label == FINISH_LABEL).unwrap().metadata).unwrap();
    assert_eq!(finished.verification_started, summary.verification_started);
    assert_eq!(finished.effect_state, DurableEffectState::Settled);
    assert!(finished.accepted);
    crate::core::speculation::verification::validate_verification_records(fixture.cohort.base.id(), &records).unwrap();
    fixture.assert_live_postimage();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn speculation_verification_start_errors_and_panic_never_launch_or_retry_gate() {
    let _home = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for point in [VerificationFault::BeforeStart, VerificationFault::AfterStart, VerificationFault::StartPanic] {
        let fixture = Fixture::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
        let outcome = fixture.integrate().unwrap();
        fixture.fault(point);
        let launches = AtomicUsize::new(0);
        for _ in 0..2 {
            fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None, || async {
                launches.fetch_add(1, Ordering::SeqCst);
                Some(gate_result(ToolEffectState::Settled))
            }).await.unwrap_err();
        }
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        let records = fixture.records();
        assert_eq!(records.iter().filter(|record| record.label == START_LABEL).count(),
            usize::from(point == VerificationFault::AfterStart));
        assert!(!records.iter().any(|record| record.label == FINISH_LABEL));
        assert!(fixture.cohort.verification_reservation.lock().unwrap().is_none());
        fixture.assert_live_postimage();
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn speculation_verification_concurrent_callers_claim_only_one_gate() {
    let _home = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
    let outcome = fixture.integrate().unwrap();
    let launches = AtomicUsize::new(0);
    let gate = || async {
        launches.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        Some(gate_result(ToolEffectState::Settled))
    };
    let (first, second) = tokio::join!(
        fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None, gate),
        fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None, gate),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    let records = fixture.records();
    assert_eq!(records.iter().filter(|record| record.label == START_LABEL).count(), 1);
    assert_eq!(records.iter().filter(|record| record.label == FINISH_LABEL).count(), 1);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn speculation_verification_finish_persists_effect_and_uncertainty_precedes_publication_failure() {
    let _home = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for effect in [ToolEffectState::PossiblyApplied(Some(17)), ToolEffectState::PossiblyApplied(None), ToolEffectState::UnknownWorker] {
        for fault in [None, Some(VerificationFault::BeforeFinish), Some(VerificationFault::FinishPanic)] {
            let fixture = Fixture::new();
            let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
            let outcome = fixture.integrate().unwrap();
            if let Some(point) = fault { fixture.fault(point); }
            let error = fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None,
                || async { Some(gate_result(effect)) }).await.unwrap_err();
            assert_eq!(super::super::tool_error_effect_state(&error), effect);
            let records = fixture.records();
            assert_eq!(records.iter().filter(|record| record.label == START_LABEL).count(), 1);
            let finished = records.iter().find(|record| record.label == FINISH_LABEL);
            assert_eq!(finished.is_some(), fault.is_none());
            if let Some(finished) = finished {
                let value: VerificationFinished = serde_json::from_slice(&finished.metadata).unwrap();
                assert_eq!(value.effect_state, durable_verifier_effect(effect).unwrap());
                assert!(!value.accepted);
                assert_ne!(value.state, "verified");
            }
            crate::core::speculation::verification::validate_verification_records(fixture.cohort.base.id(), &records).unwrap();
            fixture.assert_live_postimage();
        }
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn speculation_verification_skip_and_dropped_gate_do_not_invent_success_or_completion() {
    let _home = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for drop_gate in [false, true] {
        let fixture = Fixture::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
        let outcome = fixture.integrate().unwrap();
        if drop_gate {
            let (entered, observed) = tokio::sync::oneshot::channel();
            let attempted = fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None, || async {
                assert!(fixture.records().iter().any(|record| record.label == START_LABEL));
                entered.send(()).unwrap();
                std::future::pending::<Option<VerifyGateResult>>().await
            });
            tokio::pin!(attempted);
            tokio::select! {
                result = &mut attempted => panic!("owned pending gate unexpectedly finished: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(15), observed) => {
                    result.expect("owned gate must observe durable start within bound").unwrap();
                }
            }
            // Dropping this future cannot manufacture a finish record or authorize another run.
        } else {
            fixture.cohort.verify_applied_with_gate(&fixture.parent, &outcome, None,
                || async { None }).await.unwrap_err();
        }
        let records = fixture.records();
        assert_eq!(records.iter().filter(|record| record.label == START_LABEL).count(), 1);
        let finished = records.iter().find(|record| record.label == FINISH_LABEL);
        assert_eq!(finished.is_some(), !drop_gate);
        if let Some(finished) = finished {
            let value: VerificationFinished = serde_json::from_slice(&finished.metadata).unwrap();
            assert_eq!(value.state, "unverified");
            assert_eq!(value.effect_state, DurableEffectState::Settled);
            assert!(!value.accepted);
        }
        assert!(fixture.cohort.verification_reservation.lock().unwrap().is_none());
        crate::core::speculation::verification::validate_verification_records(fixture.cohort.base.id(), &records).unwrap();
        fixture.assert_live_postimage();
    }
}

#[test]
fn speculation_verification_pair_capacity_is_admitted_before_native_source_effects() {
    let _home = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.home.as_os_str())]);
    let _held = fixture.cohort.base.reserve_controller_records("capacity-fixture", 1, 0,
        64 * 1024 * 1024 - 192 * 1024).unwrap();
    fixture.integrate().unwrap_err();
    assert_eq!(fs::read(fixture.cohort.base.identity().canonical_root.join("owned.rs")).unwrap(),
        b"fn owned() -> i32 {\n    10\n}\n");
    assert!(fixture.records().is_empty());
}
