use super::*;
use serde_json::json;

#[test]
fn v1_only_execution_never_receives_v2_flags() {
    let path = Path::new("host-owned-attachments.json");
    for mode in [Mode::Preview, Mode::Apply, Mode::Resume, Mode::Recover] {
        let base = vec!["adoption".to_owned()];
        let mut v1 = base.clone();
        append_versioned_args(&mut v1, mode, false, path);
        assert_eq!(v1, base);
        let mut v2 = base;
        append_versioned_args(&mut v2, mode, true, path);
        assert_eq!(v2[1], "--machine-v2");
        if mode == Mode::Preview {
            assert_eq!(&v2[2..], &["--attachments", "host-owned-attachments.json"]);
        } else {
            assert_eq!(v2.len(), 2);
        }
    }
}

#[test]
fn versioned_plan_requires_exact_host_attachment_authority_and_bounded_policy() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let source = f.path().join("legacy");
    let destination = f.path().join("destination");
    let metadata = json!({"identity":"fixture","length":4,"modified_ns":"0","created_ns":null,"readonly":false,"platform_attributes":0});
    let attachment = protocol::Attachment {
        path: "archive/repair.json".into(),
        bytes: 4,
        sha256: "a".repeat(64),
    };
    let mut plan = json!({
        "format":engine::FORMAT_V2,"source":source,"destination":destination,
        "source_metadata":metadata,"destination_parent_identity":"parent",
        "inventory":[{"path":"archive","directory":true,"disposition":{"action":"copy"},"metadata":metadata,"sha256":null},
            {"path":"archive/repair.json","directory":false,"disposition":{"action":"exclude","reason":"reviewed archive attachment; backup only"},"metadata":metadata,"sha256":"a".repeat(64)},
            {"path":"entries","directory":true,"disposition":{"action":"copy"},"metadata":metadata,"sha256":null},
            {"path":"entries/fact.md","directory":false,"disposition":{"action":"copy"},"metadata":metadata,"sha256":"b".repeat(64)}],
        "total_bytes":8,"copied_bytes":4,"copy_files":1,"excluded_files":1,"digest":"",
        "policy":"bounded-backup-v2","attachments":[attachment]
    });
    fn signed(value: &serde_json::Value) -> Vec<u8> {
        let mut typed: engine::Plan = serde_json::from_value(value.clone()).unwrap();
        typed.digest.clear();
        typed.digest = super::super::manifest::sha256(&serde_json::to_vec(&typed).unwrap());
        serde_json::to_vec(&typed).unwrap()
    }
    let parse = |bytes: &[u8], approved: &[protocol::Attachment]| {
        engine::Plan::parse_authorized(
            bytes,
            &display(&source),
            &display(&destination),
            engine::FORMAT_V2,
            approved,
        )
    };
    assert!(parse(&signed(&plan), std::slice::from_ref(&attachment)).is_ok());
    for field in ["bytes", "sha256", "path"] {
        let mut changed = attachment.clone();
        match field {
            "bytes" => changed.bytes += 1,
            "sha256" => changed.sha256 = "c".repeat(64),
            _ => changed.path = "archive/other.json".into(),
        }
        assert!(parse(&signed(&plan), &[changed]).is_err());
    }
    let mut no_parent = plan.clone();
    no_parent["inventory"].as_array_mut().unwrap().remove(2);
    assert!(parse(&signed(&no_parent), std::slice::from_ref(&attachment)).is_err());
    let mut no_memory = plan.clone();
    no_memory["inventory"].as_array_mut().unwrap().truncate(2);
    no_memory["total_bytes"] = json!(4);
    no_memory["copied_bytes"] = json!(0);
    no_memory["copy_files"] = json!(0);
    assert!(parse(&signed(&no_memory), std::slice::from_ref(&attachment)).is_err());
    assert!(parse(&signed(&plan), &[]).is_err());
    assert!(
        engine::Plan::parse(&signed(&plan), &display(&source), &display(&destination)).is_err()
    );
    plan["inventory"][1]["path"] = json!("entries/corrupt.md");
    assert!(parse(&signed(&plan), std::slice::from_ref(&attachment)).is_err());
}
#[test]
fn machine_reports_are_closed_content_free_and_review_is_bounded() {
    for diagnostic in [
        json!({"code":"invalid_content","remedy":"repair_source","path":"entries/fact.md"}),
        json!({"code":"file_too_large","remedy":"reduce_source","actual_bytes":9000000,"limit_bytes":8388608}),
    ] {
        engine::Report::parse(&serde_json::to_vec(&json!({"format":"mnemos-adoption-report-v2","status":"error","diagnostic":diagnostic})).unwrap()).unwrap();
    }
    for value in [
        json!({"format":"mnemos-adoption-report-v2","status":"error","diagnostic":{"code":"SECRET","remedy":"repair_source"}}),
        json!({"format":"mnemos-adoption-report-v2","status":"error","diagnostic":{"code":"invalid_content","remedy":"repair_source","path":"../secret"}}),
        json!({"format":"mnemos-adoption-report-v2","status":"error","diagnostic":{"code":"invalid_content","remedy":"repair_source","raw":"PRIVATE RECORD"}}),
        json!({"format":"mnemos-adoption-report-v2","status":"ok","plan":{},"outcome":{}}),
        json!({"format":"mnemos-adoption-report-v2","status":"needs_review","attachments":[]}),
    ] {
        assert!(engine::Report::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    for path in [
        "entries/invalid.md",
        "archive/fact.MD",
        "archive/sub/payload.json",
        "archive/../payload.json",
        "archive/CON.json",
    ] {
        assert!(protocol::validate_attachments(&[protocol::Attachment {
            path: path.into(),
            bytes: 1,
            sha256: "a".repeat(64)
        }])
        .is_err());
    }
}

#[test]
fn closed_requests_reject_argv_paths_and_missing_consent() {
    for value in [
        json!({"action":"apply","operation_id":"a".repeat(32),"reviewed_digest":"b".repeat(64),"writers_stopped":true,"consent":true,"argv":[]}),
        json!({"action":"recover","operation_id":"../escape","reviewed_digest":"b".repeat(64),"consent":true}),
        json!({"action":"preview","source":"relative","destination_parent":null,"consent":true}),
        json!({"action":"preview","source":null,"destination_parent":null,"consent":false}),
        json!({"action":"resume","operation_id":"a".repeat(32),"reviewed_digest":"b".repeat(64),"writers_stopped":false,"consent":true}),
        json!({"action":"status","operation_id":null,"offset":0,"limit":101}),
    ] {
        let parsed = serde_json::from_value::<Request>(value);
        assert!(parsed.is_err() || parsed.unwrap().validate().is_err());
    }
    serde_json::from_value::<Request>(json!({"action":"preview","consent":true}))
        .unwrap()
        .validate()
        .unwrap();
    serde_json::from_value::<Request>(
        json!({"action":"status","operation_id":null,"offset":0,"limit":100}),
    )
    .unwrap()
    .validate()
    .unwrap();
}

fn fake_record(home: &Path, envelope: &Path, id: &str, published: bool) -> store::Record {
    std::fs::create_dir_all(envelope).unwrap();
    if published {
        std::fs::create_dir(envelope.join("data")).unwrap();
    }
    let pins = serde_json::from_value(json!({
        "schema_version":1,"id":"mnemos","target":"fixture",
        "manifest":{"path":"plugin.json","bytes":1,"sha256":"a".repeat(64)},"files":[]
    }))
    .unwrap();
    let record = store::Record {
        schema_version: 1,
        generation: 7,
        pins,
        envelope: envelope.canonicalize().unwrap(),
        envelope_identity: store::identity(envelope).unwrap(),
        plan_sha256: None,
        engine_format: engine::FORMAT.into(),
        authorized_attachments: vec![],
        published_identity: if published {
            Some(store::identity(&envelope.join("data")).unwrap())
        } else {
            None
        },
        operation: Operation {
            operation_id: id.into(),
            provider: "mnemos".into(),
            state: if published {
                State::PublishedVerified
            } else {
                State::Preview
            },
            source: display(&home.join("cli-memory")),
            data_root: display(&envelope.canonicalize().unwrap().join("data")),
            journal: display(&envelope.canonicalize().unwrap().join("journal")),
            backup: display(&envelope.canonicalize().unwrap().join("journal/backup")),
            previous_root: display(&home.join("memory-data/mnemos")),
            reviewed_digest: Some("b".repeat(64)),
            summary: None,
            message: None,
            attachments: vec![],
            diagnostic: None,
        },
    };
    let dir = store::root(home).join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("operation.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    record
}
#[test]
fn old_records_default_to_v1_and_failed_previews_do_not_rewrite_history() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let home = f.path().join("host");
    let record = fake_record(&home, &f.path().join("operation"), &"d".repeat(32), false);
    let mut value = serde_json::to_value(&record).unwrap();
    for field in ["engine_format", "authorized_attachments"] {
        value.as_object_mut().unwrap().remove(field);
    }
    let operation = value["operation"].as_object_mut().unwrap();
    operation.remove("attachments");
    operation.remove("diagnostic");
    operation.insert("state".into(), json!("interrupted"));
    operation.insert("reviewed_digest".into(), serde_json::Value::Null);
    let path = store::root(&home)
        .join(&record.operation.operation_id)
        .join("operation.json");
    let bytes = serde_json::to_vec(&value).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let decoded: store::Record = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.engine_format, engine::FORMAT);
    assert!(decoded.authorized_attachments.is_empty());
    assert!(decoded.operation.attachments.is_empty());
    assert!(decoded.operation.diagnostic.is_none());
    let marker = f.path().join("transaction-marker.json");
    let public = store::public_with_marker(decoded.clone(), &marker).unwrap();
    assert_eq!(public.state, State::CheckFailed);
    assert!(public.reviewed_digest.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::write(
        &marker,
        serde_json::to_vec(
            &json!({"kind":"adoption","operation_id":record.operation.operation_id}),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        store::public_with_marker(decoded, &marker).unwrap().state,
        State::SettlementUnknown
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn exact_published_data_leaf_is_the_only_registration_authority() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let home = f.path().join("host");
    let envelope = f.path().join("external/operation");
    let record = fake_record(&home, &envelope, &"a".repeat(32), true);
    let data = Path::new(&record.operation.data_root);
    assert!(registered_storage(data, "mnemos", &home).unwrap());
    for path in [
        envelope.clone(),
        envelope.join("journal"),
        envelope.join("journal/backup"),
        envelope.join("other"),
        data.join("child"),
    ] {
        assert!(
            registered_storage(&planned_directory(&path).unwrap(), "mnemos", &home).is_err(),
            "{}",
            path.display()
        );
    }
    assert!(registered_storage(data, "another", &home).is_err());
    // No package state exists: registrations survive uninstall. Contents may evolve after activation.
    std::fs::write(data.join("new-user-memory"), b"legitimate learning").unwrap();
    assert!(registered_storage(data, "mnemos", &home).unwrap());
    std::fs::rename(data, envelope.join("old-data")).unwrap();
    std::fs::create_dir(data).unwrap();
    assert!(registered_storage(data, "mnemos", &home).is_err());
}
#[test]
fn unpublished_or_provider_authored_receipts_never_authorize_storage() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let home = f.path().join("host");
    let envelope = f.path().join("external/operation");
    let record = fake_record(&home, &envelope, &"b".repeat(32), false);
    std::fs::create_dir(envelope.join("data")).unwrap();
    std::fs::write(
        envelope.join("published.json"),
        b"{\"state\":\"published_verified\"}",
    )
    .unwrap();
    assert!(registered_storage(Path::new(&record.operation.data_root), "mnemos", &home).is_err());
    assert!(!registered_storage(&f.path().join("unrelated"), "mnemos", &home).unwrap());
}
#[test]
fn replaced_parent_invalidates_terminal_host_receipt() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let home = f.path().join("host");
    let envelope = f.path().join("operation");
    let record = fake_record(&home, &envelope, &"c".repeat(32), true);
    std::fs::rename(&envelope, f.path().join("old-operation")).unwrap();
    std::fs::create_dir_all(envelope.join("data")).unwrap();
    assert!(registered_storage(Path::new(&record.operation.data_root), "mnemos", &home).is_err());
}
#[test]
fn inventory_paging_never_exports_record_contents_and_counts_are_exact() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let source = f.path().join("legacy");
    let destination = f.path().join("operation/data");
    let metadata = json!({"identity":"fixture","length":4,"modified_ns":"0","created_ns":null,"readonly":false,"platform_attributes":0});
    let mut plan: engine::Plan = serde_json::from_value(json!({
        "format":"mnemos-adoption-v1","source":source,"destination":destination,
        "source_metadata":metadata,"destination_parent_identity":"parent",
        "inventory":[{"path":"entries","directory":true,"disposition":{"action":"copy"},"metadata":metadata,"sha256":null},
            {"path":"entries/one.json","directory":false,"disposition":{"action":"copy"},"metadata":metadata,"sha256":"a".repeat(64)}],
        "total_bytes":4,"copied_bytes":4,"copy_files":1,"excluded_files":0,"digest":""
    })).unwrap();
    plan.digest = super::super::manifest::sha256(&serde_json::to_vec(&plan).unwrap());
    let bytes = serde_json::to_vec(&plan).unwrap();
    let parsed = engine::Plan::parse(&bytes, &display(&source), &display(&destination)).unwrap();
    assert_eq!(parsed.summary().copy_files, 1);
    assert_eq!(parsed.summary().inventory_items, 2);
    let view = serde_json::to_value(parsed.inventory[1].view()).unwrap();
    assert!(
        view.get("sha256").is_none()
            && view.get("metadata").is_none()
            && view.get("content").is_none()
    );
    plan.copy_files += 1;
    assert!(engine::Plan::parse(
        &serde_json::to_vec(&plan).unwrap(),
        &display(&source),
        &display(&destination)
    )
    .is_err());
}
#[test]
fn bounded_history_rejects_foreign_directory_names_without_scanning_stores() {
    let f = crate::core::file_placement::tests::TestDir::new();
    std::fs::create_dir_all(store::root(f.path()).join("provider-authored")).unwrap();
    assert!(store::ids_in(f.path()).is_err());
}
#[test]
fn adoption_sandbox_cannot_write_source_plan_or_sibling_stores() {
    let f = crate::core::file_placement::tests::TestDir::new();
    let package = f.path().join("package");
    let source = f.path().join("legacy");
    let envelope = f.path().join("operation");
    let temp = envelope.join("temp");
    let plan = f.path().join("plan.json");
    for directory in [&package, &source, &temp.join("home")] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(&plan, b"{}").unwrap();
    crate::sandbox::request::NativeAdoptionPolicy::new(
        package.clone(),
        Some(source.clone()),
        Some(plan.clone()),
        envelope.clone(),
        temp.clone(),
        true,
    )
    .unwrap();
    assert!(crate::sandbox::request::NativeAdoptionPolicy::new(
        package.clone(),
        Some(envelope.clone()),
        Some(plan),
        envelope.clone(),
        temp.clone(),
        true
    )
    .is_err());
    assert!(crate::sandbox::request::NativeAdoptionPolicy::new(
        package,
        Some(source),
        None,
        envelope.clone(),
        envelope,
        true
    )
    .is_err());
}
