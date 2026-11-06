use super::*;
use crate::core::{
    config::{EnvGuard, TEST_HOME_LOCK},
    file_placement::tests::TestDir,
};

#[test]
fn rich_client_requires_finite_vocabulary_scoped_filters_and_cas() {
    let root = TestDir::new();
    let binding = binding(root.path().to_str().unwrap(), &MemoryPlugins::default()).unwrap();
    let filter = json!({"scope":"current","lifecycle":"all","query":"","kind":null});
    let inspect = wire(
        json!({"type":"inspect","binding":binding,"query":{"action":"records","filter":filter,"page":{"limit":100,"max_bytes":262144,"cursor":null,"expected_snapshot":null}}}),
    );
    assert!(parse(&serde_json::to_vec(&inspect).unwrap()).is_ok());
    for (pointer, replacement) in [
        ("/action/query/filter/scope", json!("all")),
        ("/action/query/page/limit", json!(101)),
        (
            "/action/query/page/cursor",
            json!("cursor-without-snapshot"),
        ),
    ] {
        let mut v = inspect.clone();
        *v.pointer_mut(pointer).unwrap() = replacement;
        assert!(parse(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let manage = wire(
        json!({"type":"manage","binding":binding,"operation_id":"confirmed-1","command":{"filter":filter,"expected_snapshot":"viewed-state","action":{"action":"forget","target":{"reference":{"partition":"entries","id":"fact"},"expected_revision":1}}}}),
    );
    assert!(parse(&serde_json::to_vec(&manage).unwrap()).is_ok());
    for (pointer, replacement) in [
        ("/action/operation_id", json!("")),
        ("/action/command/expected_snapshot", json!("")),
        ("/action/command/action/target/expected_revision", json!(0)),
        ("/action/command/action/action", json!("execute")),
    ] {
        let mut v = manage.clone();
        *v.pointer_mut(pointer).unwrap() = replacement;
        assert!(parse(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let mut forged = manage;
    forged["action"]["capabilities"] = json!(["management"]);
    assert!(parse(&serde_json::to_vec(&forged).unwrap()).is_err());
}

#[test]
fn client_local_install_is_offline_expected_identity_and_never_activates() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("home");
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let _fixture = manager::tests::native_review_fixture(root.path());
    let source = root.path().join("source");
    let manifest = source.join("plugin.json");
    let manifest = if manifest.exists() {
        manifest
    } else {
        source.join("aizen-plugin.json")
    };
    let mut value: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    value["id"] = json!("mnemos");
    std::fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    let ws = root.path().to_str().unwrap();
    let expected = current(root.path());
    assert!(install_local(ws, &expected, source.to_str().unwrap(), "fixture").is_err());
    let installed = install_local(ws, &expected, source.to_str().unwrap(), "mnemos").unwrap();
    assert_eq!(installed["installed_id"], "mnemos");
    let state = manager::read_state().unwrap();
    assert!(state.selected.is_none());
    assert!(!state.enabled);
    assert!(!manager::transaction_marker().exists());
    assert!(install_local(
        ws,
        &current(root.path()),
        source.to_str().unwrap(),
        "mnemos"
    )
    .is_err());
    assert_eq!(manager::read_state().unwrap(), state);
}

#[test]
fn management_stale_preflight_has_no_dispatch_and_outcome_marker_is_read_only() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = TestDir::new();
        let home = root.path().join("home");
        let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
        let current = current(root.path());
        let mut stale = current.clone();
        stale.generation += 1;
        let command = rich::Manage {
            filter: rich::Filter::default(),
            expected_snapshot: "viewed".into(),
            action: rich::Action::Worklist,
        };
        let error = rich_request(
            root.path().to_str().unwrap(),
            &stale,
            None,
            Some(("op", &command)),
            "request",
        )
        .await
        .unwrap_err();
        let response =
            serde_json::to_value(failure("request".into(), error, "request_failed")).unwrap();
        assert_eq!(response["error"]["effects"], "not_dispatched");
        assert_eq!(response["error"]["code"], "stale");
        assert!(!home.exists());
        let marker = manager::transaction_marker();
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let bytes =
            serde_json::to_vec(&json!({"operation_id":"op","provider":null,"generation":0}))
                .unwrap();
        std::fs::write(&marker, &bytes).unwrap();
        let outcome = rich_request(
            root.path().to_str().unwrap(),
            &current,
            Some(&rich::Inspect::Outcome {
                operation_id: "op".into(),
            }),
            None,
            "outcome",
        )
        .await
        .unwrap();
        assert_eq!(outcome["result"]["status"], "recovery_required");
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        assert!(rich_request(
            root.path().to_str().unwrap(),
            &current,
            Some(&rich::Inspect::Outcome {
                operation_id: "foreign".into()
            }),
            None,
            "outcome"
        )
        .await
        .is_err());
    });
}

fn wire(action: Value) -> Value {
    json!({"version":1,"request_id":"fixture-1","workspace":std::env::temp_dir(),"action":action})
}
fn current(root: &Path) -> Binding {
    binding(root.to_str().unwrap(), &manager::read_state().unwrap()).unwrap()
}
fn grant(package: &super::super::install::InstalledPackage) -> ExecutionGrant {
    ExecutionGrant {
        manifest_sha256: package.pins.manifest.sha256.clone(),
        guarded_execution: true,
        network: false,
        learning: false,
        turn_model_learning: false,
    }
}
fn activate_fixture(root: &Path) -> super::super::install::InstalledPackage {
    let package = manager::tests::native_review_fixture(root);
    manager::activate(&manager::review("fixture").unwrap(), grant(&package)).unwrap();
    package
}

fn release_candidate() -> super::super::releases::Candidate {
    super::super::releases::Candidate {
        release_id: 1,
        version: "0.1.0".into(),
        target: "x86_64-pc-windows-msvc".into(),
        asset_id: 2,
        filename: "mnemos-0.1.0-x86_64-pc-windows-msvc.zip".into(),
        bytes: 1024,
        sha256: "a".repeat(64),
        checksums_asset_id: 3,
        checksums_sha256: "b".repeat(64),
    }
}

#[test]
fn release_install_requires_strict_candidate_and_explicit_confirmation() {
    let root = TestDir::new();
    let binding = binding(root.path().to_str().unwrap(), &MemoryPlugins::default()).unwrap();
    let candidate = release_candidate();
    let valid = wire(
        json!({"type":"release_install","binding":binding,"candidate":candidate,"confirmed":true}),
    );
    assert!(parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
    for (pointer, value) in [
        ("/action/confirmed", json!(false)),
        ("/action/candidate/version", json!("01.0.0")),
        ("/action/candidate/sha256", json!("missing")),
        ("/action/candidate/target", json!("arbitrary-target")),
        ("/action/candidate/filename", json!("source.zip")),
    ] {
        let mut bad = valid.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
            "{pointer}"
        );
    }
    let mut url = valid;
    url["action"]["candidate"]["url"] = json!("https://example.com/package");
    assert!(parse(&serde_json::to_vec(&url).unwrap()).is_err());
}

#[test]
fn release_stale_and_unconfirmed_preflight_never_network_or_create_home() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = TestDir::new();
        let home = root.path().join("release-home");
        let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
        let expected = current(root.path());
        let mut stale = expected.clone();
        stale.generation += 1;
        let workspace = root.path().to_str().unwrap();
        assert!(release_check(workspace, &stale).await.is_err());
        assert!(
            release_install(workspace, &stale, &release_candidate(), true)
                .await
                .is_err()
        );
        assert!(
            release_install(workspace, &expected, &release_candidate(), false)
                .await
                .is_err()
        );
        assert!(!home.exists(), "rejected release action initialized home");
    });
}

#[test]
fn update_status_modes_are_negotiated_and_absent_package_is_offline() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = TestDir::new();
        let home = root.path().join("update-home");
        let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
        let expected = current(root.path());
        for mode in ["cached", "automatic", "force"] {
            let request = wire(json!({"type":"update_status","binding":expected,"mode":mode}));
            assert!(parse(&serde_json::to_vec(&request).unwrap()).is_ok());
        }
        let bad = wire(json!({"type":"update_status","binding":expected,"mode":"install"}));
        assert!(parse(&serde_json::to_vec(&bad).unwrap()).is_err());
        if manager::host_target().is_ok() {
            for mode in [
                super::super::update_check::Mode::Cached,
                super::super::update_check::Mode::Automatic,
                super::super::update_check::Mode::Force,
            ] {
                let status = update_status(root.path().to_str().unwrap(), &expected, mode)
                    .await
                    .unwrap();
                assert!(status["update"]["release"]["installed"].is_null());
                assert_eq!(status["update"]["cache"], "empty");
            }
        }
        let mut stale = expected;
        stale.generation += 1;
        assert!(update_status(
            root.path().to_str().unwrap(),
            &stale,
            super::super::update_check::Mode::Force
        )
        .await
        .is_err());
        assert!(!home.exists(), "absent package check initialized HOME");
    });
}

#[test]
fn update_action_requires_explicit_consent_and_old_identity() {
    let root = TestDir::new();
    let expected = binding(root.path().to_str().unwrap(), &MemoryPlugins::default()).unwrap();
    let pins = json!({"schema_version":1,"id":"mnemos","target":"x86_64-pc-windows-msvc",
        "manifest":{"path":"plugin.json","bytes":100,"sha256":"a".repeat(64),"unix_mode":null},"files":[]});
    let valid = wire(
        json!({"type":"release_update","binding":expected,"candidate":release_candidate(),"expected_pins":pins,"confirmed":true}),
    );
    assert!(parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
    for (pointer, value) in [
        ("/action/confirmed", json!(false)),
        ("/action/expected_pins/id", json!("other")),
        ("/action/expected_pins/manifest/sha256", json!("bad")),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(
            parse(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn strict_bounded_client_parser_rejects_duplicates_unknown_actions_and_fields() {
    let valid = wire(json!({"type":"snapshot"}));
    assert!(parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
    for invalid in [
        wire(json!({"type":"rpc","method":"memory.forget"})),
        wire(json!({"type":"snapshot","extra":true})),
        {
            let mut v = valid.clone();
            v["extra"] = json!(true);
            v
        },
        {
            let mut v = valid.clone();
            v["version"] = json!(2);
            v
        },
        {
            let mut v = valid.clone();
            v["request_id"] = json!("x\n");
            v
        },
        {
            let mut v = valid.clone();
            v["workspace"] = json!("relative");
            v
        },
    ] {
        assert!(
            parse(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "{invalid}"
        );
    }
    assert!(parse(br#"{"version":1,"version":1}"#).is_err());
    for action in ["disable", "clear_selection"] {
        assert!(serde_json::from_value::<Change>(json!({"type":action,"extra":true})).is_err());
    }
    assert!(parse(&vec![b' '; MAX_REQUEST + 1]).is_err());
    assert!(parse(&[serde_json::to_vec(&valid).unwrap(), b" {}".to_vec()].concat()).is_err());
}

#[test]
fn offline_absent_snapshot_does_not_create_home_or_device() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("absent-home");
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let value = snapshot(root.path().to_str().unwrap()).unwrap();
    assert_eq!(value["state"], "unavailable");
    assert_eq!(value["learning"]["effective"], false);
    assert_eq!(value["binding"]["generation"], 0);
    assert!(value["packages"].as_array().unwrap().is_empty());
    assert!(value["client_features"]
        .as_array()
        .unwrap()
        .contains(&json!("mnemos_releases_v1")));
    assert!(value["client_features"]
        .as_array()
        .unwrap()
        .contains(&json!("mnemos_updates_v1")));
    assert!(!home.exists(), "offline inspection initialized home");
}

#[test]
fn review_cancel_and_tampering_do_not_write_configuration() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("home");
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let workspace = root.path().to_str().unwrap();
    let reviewed = review_change(workspace, &current(root.path()), &Change::Disable {}).unwrap();
    assert!(!home.exists());
    let mut tampered = reviewed.clone();
    tampered.change = Change::ClearSelection {};
    assert!(apply(workspace, &tampered).is_err());
    assert!(!home.exists());
    let mut tampered = reviewed;
    tampered.disclosures.clear();
    assert!(apply(workspace, &tampered).is_err());
    assert!(!home.exists());
}

#[test]
fn bindings_reject_provider_generation_manifest_and_workspace_changes() {
    let root = TestDir::new();
    let original = binding(root.path().to_str().unwrap(), &MemoryPlugins::default()).unwrap();
    for field in [
        "workspace",
        "workspace_root",
        "workspace_id",
        "provider",
        "manifest_sha256",
        "generation",
    ] {
        let mut changed = serde_json::to_value(&original).unwrap();
        changed[field] = if field == "generation" {
            json!(1)
        } else {
            json!("different")
        };
        let changed: Binding = serde_json::from_value(changed).unwrap();
        assert!(check_binding(&changed, &original).is_err(), "{field}");
    }
}

#[test]
fn detail_scope_guard_never_exposes_foreign_project_or_other_scope() {
    let record: api::Record = serde_json::from_value(
        json!({"id":"one","content":"secret","scope":"project","workspace_id":"a","revision":1}),
    )
    .unwrap();
    assert!(scope_safe(&record, &api::Scope::Project, "a"));
    assert!(!scope_safe(&record, &api::Scope::Project, "b"));
    assert!(!scope_safe(&record, &api::Scope::User, "a"));
    assert!(!scope_safe(&record, &api::Scope::Device, "a"));
}

#[test]
fn snapshot_activation_learning_and_storage_are_offline_and_atomic() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let package = manager::tests::native_review_fixture(root.path());
    let ws = workspace.to_str().unwrap();
    let before = snapshot(ws).unwrap();
    assert_eq!(before["packages"][0]["valid"], true);
    assert_eq!(
        before["packages"][0]["operations"],
        json!(["list", "search", "get"])
    );
    let reviewed = review_change(
        ws,
        &current(&workspace),
        &Change::Activate {
            provider: "fixture".into(),
            grant: grant(&package),
        },
    )
    .unwrap();
    let bytes = std::fs::read(crate::core::cli_config::config_path()).unwrap();
    let mut bad = reviewed.clone();
    bad.packages.get_mut("fixture").unwrap().files[0].sha256 = "b".repeat(64);
    bad.review_id = review_digest(&bad).unwrap();
    assert!(apply(ws, &bad).is_err());
    assert_eq!(
        std::fs::read(crate::core::cli_config::config_path()).unwrap(),
        bytes
    );
    let activated = apply(ws, &reviewed).unwrap();
    assert_eq!(activated["snapshot"]["enabled"], true);
    assert_eq!(activated["snapshot"]["learning"]["effective"], false);
    assert!(apply(ws, &reviewed).is_err());
    let mut sharing = grant(&package);
    sharing.learning = true;
    sharing.turn_model_learning = true;
    let reviewed = review_change(
        ws,
        &current(&workspace),
        &Change::Learning {
            enabled: true,
            grant: Some(sharing.clone()),
        },
    )
    .unwrap();
    let on = apply(ws, &reviewed).unwrap();
    assert_eq!(on["snapshot"]["learning"]["effective"], true);
    assert_eq!(
        manager::read_state().unwrap().packages["fixture"].grant,
        Some(sharing)
    );
    let custom = root.path().join("custom-data");
    let reviewed = review_change(
        ws,
        &current(&workspace),
        &Change::Storage {
            provider: "fixture".into(),
            root: Some(custom.to_str().unwrap().into()),
        },
    )
    .unwrap();
    assert!(reviewed.disclosures[0].contains("No data is moved"));
    apply(ws, &reviewed).unwrap();
    assert!(!custom.exists());
    assert!(!home.join("memory-data").exists());
    assert!(!home.join("device.json").exists());
    std::fs::create_dir(&custom).unwrap();
    std::fs::write(custom.join("keep"), b"preserved").unwrap();
    let reviewed = review_change(
        ws,
        &current(&workspace),
        &Change::Storage {
            provider: "fixture".into(),
            root: None,
        },
    )
    .unwrap();
    apply(ws, &reviewed).unwrap();
    assert_eq!(std::fs::read(custom.join("keep")).unwrap(), b"preserved");
}

#[test]
fn damaged_payload_stays_visible_and_guarded_off_disable_still_work() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("home");
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let package = activate_fixture(root.path());
    std::fs::remove_file(package.entrypoint).unwrap();
    let ws = root.path().to_str().unwrap();
    let value = snapshot(ws).unwrap();
    assert_eq!(value["state"], "damaged");
    assert_eq!(value["packages"][0]["valid"], false);
    let generation = current(root.path()).generation;
    assert!(manager::set_learning_policy_expected(false, generation + 1).is_err());
    assert!(manager::disable_expected(generation + 1).is_err());
    assert_eq!(current(root.path()).generation, generation);
    let reviewed = review_change(
        ws,
        &current(root.path()),
        &Change::Learning {
            enabled: false,
            grant: None,
        },
    )
    .unwrap();
    let saved_grant = manager::read_state().unwrap().packages["fixture"]
        .grant
        .clone();
    apply(ws, &reviewed).unwrap();
    assert_eq!(
        crate::core::cli_config::read_memory_configuration()
            .unwrap()
            .1,
        Some(false)
    );
    assert_eq!(
        manager::read_state().unwrap().packages["fixture"].grant,
        saved_grant
    );
    let reviewed = review_change(ws, &current(root.path()), &Change::Disable {}).unwrap();
    apply(ws, &reviewed).unwrap();
    assert!(!manager::read_state().unwrap().enabled);
    let reviewed = review_change(ws, &current(root.path()), &Change::ClearSelection {}).unwrap();
    apply(ws, &reviewed).unwrap();
    assert!(manager::read_state().unwrap().selected.is_none());
}

#[test]
fn recovery_and_busy_never_partially_apply_reviewed_settings() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("home");
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let ws = root.path().to_str().unwrap();
    let review = review_change(
        ws,
        &current(root.path()),
        &Change::Learning {
            enabled: false,
            grant: None,
        },
    )
    .unwrap();
    let lock = manager::Admission::acquire().unwrap();
    let error = apply(ws, &review).unwrap_err();
    assert_eq!(
        failure("x".into(), error, "internal").error.unwrap().code,
        "busy"
    );
    assert!(!crate::core::cli_config::config_path().exists());
    drop(lock);
    let marker = manager::transaction_marker();
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, b"pending").unwrap();
    assert_eq!(snapshot(ws).unwrap()["state"], "recovery_required");
    assert!(apply(ws, &review).is_err());
    assert!(!crate::core::cli_config::config_path().exists());
}

#[test]
fn stale_reads_are_rejected_before_context_device_or_launch() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = TestDir::new();
        let home = root.path().join("home");
        let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
        let mut expected = current(root.path());
        expected.generation += 1;
        let response = execute(Request {
            version: 1,
            request_id: "read-1".into(),
            workspace: root.path().to_str().unwrap().into(),
            action: Action::Read {
                binding: expected,
                query: Query::List {
                    scope: api::Scope::Project,
                    limit: 10,
                },
            },
        })
        .await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "stale");
        assert!(!home.exists());
    });
}

#[test]
fn disabled_and_unsupported_reads_do_not_construct_provider_context() {
    if manager::host_target().is_err() {
        return;
    }
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let home = root.path().join("home");
    let _env = EnvGuard::set([("AIZEN_HOME", home.as_path())]);
    let package = activate_fixture(root.path());
    manager::disable_expected(manager::read_state().unwrap().generation).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let query = Query::List {
        scope: api::Scope::Project,
        limit: 10,
    };
    let error = runtime
        .block_on(read(
            root.path().to_str().unwrap(),
            &current(root.path()),
            &query,
            "disabled-1",
        ))
        .unwrap_err();
    assert_eq!(
        failure("x".into(), error, "internal").error.unwrap().code,
        "disabled"
    );
    assert!(!home.join("device.json").exists());
    assert!(!home.join("memory-data").exists());
    manager::uninstall(&package.manifest.id).unwrap();
    let manifest_path = root.path().join("source/plugin.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["capabilities"] = json!([]);
    crate::core::persist::atomic_write_owner_only(
        &manifest_path,
        &serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let package = manager::install_local(&root.path().join("source")).unwrap();
    manager::activate(&manager::review("fixture").unwrap(), grant(&package)).unwrap();
    let error = runtime
        .block_on(read(
            root.path().to_str().unwrap(),
            &current(root.path()),
            &query,
            "unsupported-1",
        ))
        .unwrap_err();
    assert_eq!(
        failure("x".into(), error, "internal").error.unwrap().code,
        "unsupported_capability"
    );
    assert_eq!(
        snapshot(root.path().to_str().unwrap()).unwrap()["packages"][0]["operations"],
        json!([])
    );
    assert!(!home.join("device.json").exists());
    assert!(!home.join("memory-data").exists());
}

#[test]
fn corrupt_configuration_is_not_an_empty_success_or_repaired() {
    let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TestDir::new();
    let _env = EnvGuard::set([("AIZEN_HOME", root.path())]);
    let path = crate::core::cli_config::config_path();
    let bytes = b"{\"memory_auto_learn\":false,\"memory_auto_learn\":true}";
    std::fs::write(&path, bytes).unwrap();
    assert!(snapshot(root.path().to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let sensitive = b"{\"memory_auto_learn\":\"synthetic-secret-do-not-echo\"}";
    std::fs::write(&path, sensitive).unwrap();
    let error = snapshot(root.path().to_str().unwrap()).unwrap_err();
    let response = failure("snapshot-1".into(), error, "unavailable");
    let wire = serde_json::to_string(&response).unwrap();
    assert!(!wire.contains("synthetic-secret-do-not-echo"));
    assert!(wire.contains("repair it locally"));
    assert_eq!(std::fs::read(&path).unwrap(), sensitive);
}

#[test]
fn cli_client_is_distinct_and_preserves_existing_status_and_list_arguments() {
    use crate::cli_args::{Cli, Commands, MemoryCmd};
    use clap::Parser;
    assert!(matches!(
        Cli::try_parse_from(["aizen", "memory", "client"])
            .unwrap()
            .command,
        Some(Commands::Memory {
            cmd: MemoryCmd::Client
        })
    ));
    assert!(Cli::try_parse_from(["aizen", "memory", "client", "--method", "forget"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["aizen", "memory", "status", "--json"])
            .unwrap()
            .command,
        Some(Commands::Memory {
            cmd: MemoryCmd::Status { json: true }
        })
    ));
    assert!(Cli::try_parse_from(["aizen", "memory", "list", "current"]).is_ok());
}
