use super::*;

fn origin_context(root: &std::path::Path) -> api::ProviderContext {
    serde_json::from_value(json!({
        "generation":1,"user_id":"fixture-user","workspace_id":"fixture-workspace",
        "workspace_root":root,"cwd":root,"device_id":"fixture-device","device_aliases":[],
        "session_id":"fixture-session","turn_id":"fixture-turn","boundary_id":"fixture-boundary",
        "data_root":root.join("data"),"settings":{},"granted_capabilities":["records"],
        "budgets":{"context_tokens":100,"recall_tokens":100,"max_records":10}
    }))
    .unwrap()
}

#[test]
fn response_budgets_enforce_user_caps_independently_of_provider_claims() {
    let root = std::env::temp_dir().join("memory-budget-no-io");
    let mut context = origin_context(&root);
    context.budgets.max_records = 1;
    let one = json!({"items":[{"text":"small"}]});
    validate_response_budget(&context, Method::BuildContext, &json!({}), &one).unwrap();
    assert!(validate_response_budget(
        &context,
        Method::BuildContext,
        &json!({}),
        &json!({"items":[{},{}]})
    )
    .is_err());
    assert!(validate_response_budget(&context, Method::Recall, &json!({"limit":0}), &one).is_err());
    context.budgets.context_tokens = 0;
    assert!(validate_response_budget(&context, Method::BuildContext, &json!({}), &one).is_err());
    validate_response_budget(
        &context,
        Method::BuildContext,
        &json!({}),
        &json!({"items":[]}),
    )
    .unwrap();
    context.budgets.recall_tokens = 1;
    assert!(validate_response_budget(&context, Method::Recall, &json!({}), &one).is_err());
    validate_response_budget(
        &context,
        Method::Health,
        &json!({}),
        &json!({"status":"ready"}),
    )
    .unwrap();
}

#[test]
fn human_management_is_optional_and_never_granted_to_model_or_background_callers() {
    let root = std::env::temp_dir().join("memory-origin-no-io");
    let base = origin_context(&root);
    for approval in [
        ApprovedInvocation::model_tool(),
        ApprovedInvocation::explicit_cli().without_management(),
        ApprovedInvocation::saved_context(),
        ApprovedInvocation::saved_learning(),
    ] {
        check_invocation_origin(&approval, &base, None).unwrap();
        let mut context = base.clone();
        grant_management(
            &approval,
            &mut context,
            &[api::Capability::Records, api::Capability::Management],
        );
        assert_eq!(
            serde_json::to_value(context).unwrap(),
            serde_json::to_value(&base).unwrap()
        );
    }
    let human = ApprovedInvocation::explicit_cli();
    check_invocation_origin(&human, &base, None).unwrap();
    let mut legacy = base.clone();
    grant_management(&human, &mut legacy, &[api::Capability::Records]);
    assert_eq!(
        serde_json::to_value(legacy).unwrap(),
        serde_json::to_value(&base).unwrap()
    );
    let mut extended = base;
    grant_management(
        &human,
        &mut extended,
        &[api::Capability::Records, api::Capability::Management],
    );
    assert_eq!(
        extended.granted_capabilities,
        [api::Capability::Records, api::Capability::Management]
    );
    extended.validate().unwrap();
}

#[test]
fn human_management_cannot_be_inherited_by_delegated_or_native_scoped_execution() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let context = origin_context(fixture.path());
    let human = ApprovedInvocation::explicit_cli();
    let execution = crate::core::exec_ctx::ExecutionContext::default();
    check_invocation_origin(&human, &context, Some(&execution)).unwrap();
    let native = crate::core::native_scope::NativeWriteScope::resolve(
        fixture.path(),
        &["owned.txt".to_owned()],
    )
    .unwrap();
    for restricted in [
        execution.without_workspace_restore(),
        execution.with_dispatch_label(Some("fixture-child".into())),
        execution.with_native_scope(native).unwrap(),
    ] {
        assert!(check_invocation_origin(&human, &context, Some(&restricted)).is_err());
        assert!(check_invocation_origin(
            &ApprovedInvocation::model_tool(),
            &context,
            Some(&restricted)
        )
        .is_ok());
    }
}

#[test]
fn forged_management_context_is_rejected_by_both_bridges_before_selection_or_storage() {
    let _guard = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let mut context = origin_context(fixture.path());
    context
        .granted_capabilities
        .push(api::Capability::Management);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for approval in [
        ApprovedInvocation::explicit_cli(),
        ApprovedInvocation::saved_context(),
        ApprovedInvocation::saved_learning(),
    ] {
        let error = runtime
            .block_on(request(
                approval,
                context.clone(),
                Method::Get,
                json!({"id":"foreign"}),
                None,
                crate::core::cancel::TurnCancel::new(),
            ))
            .unwrap_err();
        assert!(format!("{error:#}").contains("authority cannot be supplied"));
    }
    let error = request_blocking(context, Method::Get, json!({"id":"foreign"})).unwrap_err();
    assert!(format!("{error:#}").contains("authority cannot be supplied"));
    assert!(!home.exists());
    assert!(!fixture.path().join("data").exists());
}

#[test]
fn affinity_results_are_typed_bounded_and_cannot_name_non_skill_authority() {
    let params = json!({"ids":["fact"],"limit":1});
    validate_result(
        Method::Affinity,
        &params,
        &json!({"items":[{"node":"skill:fixture","score":0.75}]}),
    )
    .unwrap();
    for invalid in [
        json!({"items":[{"node":"skill:fixture","score":2.0}]}),
        json!({"items":[{"node":"persona:fixture","score":0.5}]}),
        json!({"items":[{"node":"skill:","score":0.5}]}),
        json!({"items":[{"node":"skill:a","score":0.5},{"node":"skill:b","score":0.5}]}),
        json!({"items":[{"node":"skill:a","score":0.5,"authority":"load"}]}),
        json!({"items":[{"node":"skill:a","score":0.5},{"node":"skill:a","score":0.5}]}),
    ] {
        assert!(validate_result(Method::Affinity, &params, &invalid).is_err());
    }
}

#[test]
fn completed_turn_receipts_must_match_the_operation_and_payload() {
    let params = json!({"operation_id":"turn-fixture","payload_sha256":"a".repeat(64)});
    let good = json!({"accepted":true,"operation_id":"turn-fixture","payload_sha256":"a".repeat(64),
        "learning_mode":"offline","added":[],"reinforced":[],"review":[],
        "skipped_passive":true,"session_candidates":[]});
    validate_result(Method::ProcessTurn, &params, &good).unwrap();
    for (key, value) in [
        ("operation_id", json!("another-turn")),
        ("payload_sha256", json!("b".repeat(64))),
        ("payload_sha256", Value::Null),
        ("accepted", json!(false)),
        (
            "session_candidates",
            json!([{"text":"unsafe\u{001b}","anchor":null,"importance":5}]),
        ),
    ] {
        let mut invalid = good.clone();
        invalid[key] = value;
        assert!(
            validate_result(Method::ProcessTurn, &params, &invalid).is_err(),
            "{key}"
        );
    }
}

#[test]
fn prepared_receipts_are_bound_and_cannot_mislabel_the_learning_mode() {
    let params = json!({"operation_id":"turn-fixture","payload_sha256":"a".repeat(64)});
    let mut receipt = json!({"accepted":true,"operation_id":"turn-fixture","payload_sha256":"a".repeat(64),
        "learning_mode":"same_turn_model","added":[],"reinforced":[],"review":[],
        "skipped_passive":false,"session_candidates":[]});
    validate_result(Method::CompleteJob, &params, &receipt).unwrap();
    assert!(validate_result(Method::ProcessTurn, &params, &receipt).is_err());
    receipt["learning_mode"] = json!("offline");
    assert!(validate_result(Method::CompleteJob, &params, &receipt).is_err());
    let prepare = json!({"operation_id":"turn-fixture-prepare","turn":{
        "operation_id":"turn-fixture","user_text":"fixture","payload_sha256":"a".repeat(64)
    },"tool_calls":1,"recovered":false});
    let good = json!({"operation_id":"turn-fixture-prepare","turn_operation_id":"turn-fixture",
        "payload_sha256":"a".repeat(64),"job":null});
    validate_result(Method::PrepareJob, &prepare, &good).unwrap();
    for (key, value) in [
        ("operation_id", json!("different-prepare")),
        ("turn_operation_id", json!("different")),
        ("payload_sha256", json!("b".repeat(64))),
    ] {
        let mut invalid = good.clone();
        invalid[key] = value;
        assert!(
            validate_result(Method::PrepareJob, &prepare, &invalid).is_err(),
            "{key}"
        );
    }
}

#[test]
fn saved_learning_rechecks_auto_learning_policy_before_launch() {
    let _guard = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let old_home = std::env::var_os("AIZEN_HOME");
    let home = crate::core::file_placement::tests::TestDir::new();
    std::env::set_var("AIZEN_HOME", home.path());
    let mut cfg = crate::core::cli_config::CliConfig {
        memory_auto_learn: Some(true),
        ..Default::default()
    };
    crate::core::cli_config::save(&cfg).unwrap();
    let saved = ApprovedInvocation::saved_learning();
    assert!(check_learning_policy(&saved, Method::ProcessTurn, true).is_ok());
    cfg.memory_auto_learn = Some(false);
    crate::core::cli_config::save_with_memory_policy(&cfg, Some(false)).unwrap();
    assert!(check_learning_policy(&saved, Method::ProcessTurn, true).is_err());
    assert!(
        check_learning_policy(&ApprovedInvocation::saved_context(), Method::Recall, false).is_ok()
    );
    assert!(!manager::transaction_marker().exists());
    assert!(!home.path().join("memory-data").exists());
    if let Some(old) = old_home {
        std::env::set_var("AIZEN_HOME", old);
    } else {
        std::env::remove_var("AIZEN_HOME");
    }
}

#[test]
fn optional_reads_have_scoped_host_operation_ids_without_overwriting_supplied_ids() {
    let root = std::env::temp_dir().join("memory-operation-no-io");
    let mut context: api::ProviderContext = serde_json::from_value(json!({
        "generation":1,"user_id":"fixture-user","workspace_id":"fixture-workspace",
        "workspace_root":root,"cwd":root,"device_id":"fixture-device","device_aliases":[],
        "session_id":"fixture-session","turn_id":"fixture-turn","boundary_id":"fixture-boundary",
        "data_root":root,"settings":{},"granted_capabilities":[],
        "budgets":{"context_tokens":100,"recall_tokens":100,"max_records":10}
    }))
    .unwrap();
    let params = json!({"query":"fixture","limit":3});
    let first = operation_params(&context, Method::Recall, params.clone()).unwrap();
    assert!(first["operation_id"].as_str().unwrap().starts_with("host-"));
    assert_eq!(
        first,
        operation_params(&context, Method::Recall, params.clone()).unwrap()
    );
    assert_ne!(
        first["operation_id"],
        operation_params(&context, Method::Search, params.clone()).unwrap()["operation_id"]
    );
    context.generation += 1;
    assert_ne!(
        first["operation_id"],
        operation_params(&context, Method::Recall, params).unwrap()["operation_id"]
    );
    assert!(
        operation_params(&context, Method::BuildContext, json!({})).unwrap()["operation_id"]
            .is_string()
    );
    let supplied = json!({"query":"fixture","operation_id":"explicit-operation"});
    assert_eq!(
        operation_params(&context, Method::Search, supplied.clone()).unwrap(),
        supplied
    );
    assert_eq!(
        operation_params(&context, Method::Get, json!({"id":"record"})).unwrap(),
        json!({"id":"record"})
    );
}

#[test]
fn cancelled_or_expired_async_requests_stop_before_selection_or_enqueue() {
    let _guard = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let context: api::ProviderContext = serde_json::from_value(json!({
        "generation":1,"user_id":"fixture-user","workspace_id":"fixture-workspace",
        "workspace_root":fixture.path(),"cwd":fixture.path(),"device_id":"fixture-device",
        "device_aliases":[],"session_id":"fixture-session","turn_id":"fixture-turn",
        "boundary_id":"fixture-boundary","data_root":fixture.path().join("data"),
        "settings":{},"granted_capabilities":[],
        "budgets":{"context_tokens":100,"recall_tokens":100,"max_records":10}
    }))
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancelled = crate::core::cancel::TurnCancel::new();
    cancelled.cancel();
    let error = runtime
        .block_on(request(
            ApprovedInvocation::saved_context(),
            context.clone(),
            Method::Health,
            json!({}),
            None,
            cancelled,
        ))
        .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled before enqueue"));
    let error = runtime
        .block_on(request(
            ApprovedInvocation::saved_context(),
            context,
            Method::Health,
            json!({}),
            Some(
                crate::core::exec_ctx::ExecutionContext::default()
                    .with_deadline(Some(Instant::now())),
            ),
            crate::core::cancel::TurnCancel::new(),
        ))
        .unwrap_err();
    assert!(format!("{error:#}").contains("deadline elapsed before enqueue"));
    assert!(!home.exists());
    assert!(!fixture.path().join("data").exists());
}

#[test]
fn observer_deadline_never_extends_the_turn_or_transaction_ceiling() {
    let execution = crate::core::exec_ctx::ExecutionContext::default();
    let earlier = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        observer_deadline(Some(&execution.with_deadline(Some(earlier)))),
        earlier
    );
    let before = Instant::now();
    let later = execution.with_deadline(Some(before + Duration::from_secs(300)));
    let deadline = observer_deadline(Some(&later));
    assert!(deadline >= before + MAX_TRANSACTION);
    assert!(deadline <= Instant::now() + MAX_TRANSACTION + Duration::from_secs(10));
    assert!(deadline < later.deadline().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn observer_deadline_cancels_only_its_child_and_retains_physical_ownership() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let settlement = observer.settlement.clone();
    let physical = Arc::new(SettlementLifetime::new((), |_| {}));
    let stdout = physical.drain();
    let stderr = physical.drain();
    physical.on_release(move || settlement.prove_tree_settled());
    let parent = crate::core::cancel::TurnCancel::new();
    let child = parent.child();
    let sibling = parent.child();
    let (reply, response) = tokio::sync::oneshot::channel();
    let execution = crate::core::exec_ctx::ExecutionContext::default()
        .with_deadline(Some(Instant::now() + Duration::from_millis(100)));
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        observe_response(
            response,
            &parent,
            child.clone(),
            observer_deadline(Some(&execution)),
            observer,
        ),
    )
    .await
    .expect("logical observation must not wait for actor cleanup")
    .unwrap_err();
    assert!(format!("{error:#}").contains("observer deadline elapsed"));
    assert_eq!(
        crate::agent::tool_error_effect_state(&error),
        ToolEffectState::UnknownWorker
    );
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled() && !sibling.is_cancelled());
    assert!(reply.is_closed());
    assert!(
        tracker.is_unsettled(),
        "a deadline is not physical settlement"
    );
    physical.prove_tree_settled();
    drop(stdout);
    assert!(
        tracker.is_unsettled(),
        "stderr still owns the physical transaction"
    );
    drop(stderr);
    assert!(!tracker.is_unsettled());
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_observation_preserves_queued_work_until_no_spawn_is_proved() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let proof = BeforeSpawnProof::new(observer.settlement.clone());
    let parent = crate::core::cancel::TurnCancel::new();
    let child = parent.child();
    let (reply, response) = tokio::sync::oneshot::channel();
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        observe_response(
            response,
            &parent,
            child.clone(),
            Instant::now() + Duration::from_secs(10),
            observer
        ),
    )
    .await
    .is_err());
    assert!(child.is_cancelled() && !parent.is_cancelled());
    assert!(reply.is_closed());
    assert!(
        tracker.is_unsettled(),
        "queued work still belongs to the actor"
    );
    drop(proof);
    assert!(!tracker.is_unsettled());

    let observer = Observer::new(&tracker);
    let settlement = observer.settlement.clone();
    let child = parent.child();
    let (reply, response) = tokio::sync::oneshot::channel();
    settlement.prove_tree_settled();
    reply.send(Ok(json!({"settled":true}))).unwrap();
    let value = observe_response(response, &parent, child, Instant::now(), observer)
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({"settled":true}),
        "an available terminal result wins over timeout"
    );
    assert!(!tracker.is_unsettled());
}

#[test]
fn lost_observer_before_marker_blocks_until_actor_proves_no_spawn() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let proof = BeforeSpawnProof::new(observer.settlement.clone());
    assert!(
        !tracker.is_unsettled(),
        "a live caller still owns the request"
    );
    drop(observer);
    assert!(
        tracker.is_unsettled(),
        "marker absence is not queue settlement"
    );
    drop(proof);
    assert!(
        !tracker.is_unsettled(),
        "cancelled pre-spawn work has no child"
    );
}

#[test]
fn cancellation_after_preparation_never_reaches_marker_or_spawn() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let mut proof = BeforeSpawnProof::new(observer.settlement.clone());
    let cancel = crate::core::cancel::TurnCancel::new();
    let prepared = Arc::new(std::sync::Barrier::new(2));
    let cancelled = Arc::new(std::sync::Barrier::new(2));
    let worker_cancel = cancel.clone();
    let worker_prepared = prepared.clone();
    let worker_cancelled = cancelled.clone();
    let worker = std::thread::spawn(move || {
        worker_prepared.wait();
        worker_cancel.cancel();
        worker_cancelled.wait();
    });
    // Synchronous preparation finishes while the caller drops/cancels its observation.
    prepared.wait();
    cancelled.wait();
    let mut marked = false;
    let mut spawned = false;
    let result =
        check_before_spawn(&cancel, Instant::now() + Duration::from_secs(1)).and_then(|()| {
            marked = true;
            proof.spawn_if_live(&cancel, Instant::now() + Duration::from_secs(1), || {
                spawned = true;
            })
        });
    assert!(result.is_err());
    assert!(!marked && !spawned);
    drop(observer);
    assert!(tracker.is_unsettled());
    drop(proof);
    assert!(!tracker.is_unsettled());
    worker.join().unwrap();
}

#[test]
fn cancellation_or_deadline_after_marker_retires_only_our_no_spawn_receipt() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    for expired in [false, true] {
        let tracker = Arc::new(UnsettledOwnership::new());
        let observer = Observer::new(&tracker);
        let mut proof = BeforeSpawnProof::new(observer.settlement.clone());
        let marker = fixture
            .path()
            .join(if expired { "expired" } else { "cancelled" });
        let bytes = b"owned no-spawn receipt";
        proof.marker = Some((
            marker.clone(),
            crate::core::persist::FileFingerprint::for_bytes(bytes),
        ));
        crate::core::persist::atomic_write_owner_only(&marker, bytes).unwrap();
        let cancel = crate::core::cancel::TurnCancel::new();
        let deadline = if expired {
            Instant::now()
        } else {
            cancel.cancel();
            Instant::now() + Duration::from_secs(1)
        };
        let mut spawned = false;
        assert!(proof
            .spawn_if_live(&cancel, deadline, || spawned = true)
            .is_err());
        assert!(!spawned);
        drop(observer);
        assert!(tracker.is_unsettled());
        drop(proof);
        assert!(!tracker.is_unsettled());
        assert!(!marker.exists());
    }
}

#[test]
fn no_spawn_cleanup_preserves_a_replaced_receipt() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let mut proof = BeforeSpawnProof::new(observer.settlement.clone());
    let marker = fixture.path().join("receipt");
    proof.marker = Some((
        marker.clone(),
        crate::core::persist::FileFingerprint::for_bytes(b"our receipt"),
    ));
    std::fs::write(&marker, b"different receipt").unwrap();
    drop(observer);
    drop(proof);
    assert!(!tracker.is_unsettled());
    assert_eq!(std::fs::read(marker).unwrap(), b"different receipt");
}

#[test]
fn spawn_attempt_disarms_no_child_proof_until_physical_release() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let settlement = observer.settlement.clone();
    let mut proof = BeforeSpawnProof::new(settlement.clone());
    let mut spawned = false;
    proof
        .spawn_if_live(
            &crate::core::cancel::TurnCancel::new(),
            Instant::now() + Duration::from_secs(1),
            || spawned = true,
        )
        .unwrap();
    assert!(spawned);
    drop(observer);
    drop(proof);
    assert!(tracker.is_unsettled());
    settlement.prove_tree_settled();
    assert!(!tracker.is_unsettled());
}

#[test]
fn lost_observer_keeps_charge_through_tree_and_all_pipe_drains() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let settlement = observer.settlement.clone();
    let physical = Arc::new(SettlementLifetime::new((), |_| {}));
    let stdout = physical.drain();
    let stderr = physical.drain();
    let classification = physical.drain();
    physical.on_release(move || settlement.prove_tree_settled());
    drop(observer);
    assert!(tracker.is_unsettled());
    physical.prove_tree_settled();
    drop(stdout);
    drop(stderr);
    assert!(
        tracker.is_unsettled(),
        "terminal classification still owns work"
    );
    drop(classification);
    assert!(!tracker.is_unsettled());
}

#[test]
fn unverified_spawn_without_proof_path_permanently_blocks() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    tracker.charge_permanent();
    drop(observer);
    assert!(!tracker.wait_for_recent_settlement(Duration::ZERO));
}

#[test]
fn actor_loss_without_reply_cannot_retire_observer_ownership() {
    let tracker = Arc::new(UnsettledOwnership::new());
    let observer = Observer::new(&tracker);
    let (reply, mut response) = tokio::sync::oneshot::channel::<Result<Value>>();
    drop(reply);
    assert!(matches!(
        response.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    drop(observer);
    assert!(tracker.is_unsettled());
    let error = unknown_worker("lost actor");
    assert_eq!(
        crate::agent::tool_error_effect_state(&error),
        ToolEffectState::UnknownWorker
    );
}

#[test]
fn ambiguous_provider_reply_never_retires_receipt_but_conflict_does() {
    for code in [ErrorCode::AmbiguousResult, ErrorCode::RecoveryRequired] {
        let result = Ok(Err(MemoryError::new(code, "unresolved")));
        assert!(!clean_terminal_reply(&result, true));
    }
    for code in [
        ErrorCode::Conflict,
        ErrorCode::NotFound,
        ErrorCode::InvalidRequest,
    ] {
        let result = Ok(Err(MemoryError::new(code, "terminal semantic failure")));
        assert!(clean_terminal_reply(&result, true));
        assert!(!clean_terminal_reply(&result, false));
    }
    assert!(clean_terminal_reply(&Ok(Ok(json!({}))), true));
    assert!(!clean_terminal_reply(
        &Err(anyhow::anyhow!("malformed transport")),
        true
    ));
    assert_eq!(
        crate::agent::tool_error_effect_state(&unresolved_outcome("clean tree, unknown commit")),
        ToolEffectState::PossiblyApplied(None),
    );
}

#[test]
fn optional_failure_preserves_uncertainty_before_any_marker_exists() {
    let _guard = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    assert!(!manager::transaction_marker().exists());
    check_effects().unwrap();
    optional_failure(anyhow::anyhow!("optional recall unavailable")).unwrap();
    let error = optional_failure(unknown_worker("actor cancellation before marker")).unwrap_err();
    assert_eq!(
        crate::agent::tool_error_effect_state(&error),
        ToolEffectState::UnknownWorker
    );
    assert!(
        !home.exists(),
        "the boundary check must not initialize Memory data"
    );
}

#[test]
fn recovery_receipt_blocks_new_work_and_is_preserved() {
    let _guard = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let marker = manager::transaction_marker();
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    let receipt = br#"{"schema_version":1,"operation_id":"original-operation"}"#;
    std::fs::write(&marker, receipt).unwrap();
    let error = check_effects().unwrap_err();
    assert_eq!(
        crate::agent::tool_error_effect_state(&error),
        ToolEffectState::PossiblyApplied(None)
    );
    assert_eq!(std::fs::read(&marker).unwrap(), receipt);
    assert!(optional_failure(anyhow::anyhow!("optional recall unavailable")).is_err());
    assert_eq!(std::fs::read(&marker).unwrap(), receipt);
}

#[tokio::test(flavor = "current_thread")]
async fn matching_ambiguous_error_with_clean_shutdown_is_not_terminal_success() {
    use tokio::io::{duplex, AsyncWriteExt};
    let (host, mut provider) = duplex(4096);
    let (reader, writer) = tokio::io::split(host);
    // Preloaded bounded fixture replies, no subprocess or external model.
    provider.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"unknown\",\"data\":{\"code\":\"ambiguous_result\",\"message\":\"unknown commit\"}}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"stopping\":true}}\n").await.unwrap();
    provider.shutdown().await.unwrap();
    let mut connection = transport::Connection::new(BufReader::new(reader), writer);
    let result: api::Result<Value> = connection
        .request(
            "store",
            &json!({"operation_id":"original-operation"}),
            Duration::from_secs(1),
        )
        .await;
    assert_eq!(
        result.as_ref().unwrap_err().code,
        ErrorCode::AmbiguousResult
    );
    assert!(
        !connection.is_poisoned(),
        "valid semantic failure is not malformed transport"
    );
    let shutdown: Value = connection
        .request("shutdown", &json!({}), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(shutdown, json!({"stopping":true}));
    connection
        .finish_stdout(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!clean_terminal_reply(&Ok(result), true));
}
