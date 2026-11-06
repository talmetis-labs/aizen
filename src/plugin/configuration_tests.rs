use super::*;
use serde_json::json;

fn definition(value: Value) -> SettingDefinition {
    serde_json::from_value(value).unwrap()
}

fn schema() -> BTreeMap<String, SettingDefinition> {
    BTreeMap::from([
        (
            "enabled".into(),
            definition(
                json!({"label":"Enabled","description":"Optional matching","type":"boolean","default":true}),
            ),
        ),
        (
            "count".into(),
            definition(
                json!({"label":"Count","description":"","type":"integer","default":12,"min":0,"max":100}),
            ),
        ),
        (
            "days".into(),
            definition(
                json!({"label":"Days","description":"","type":"number","default":30.0,"min":1.0,"max":3650.0}),
            ),
        ),
        (
            "prefix".into(),
            definition(
                json!({"label":"Prefix","description":"Nonsecret text only","type":"string","default":"","max_length":16}),
            ),
        ),
        (
            "mode".into(),
            definition(
                json!({"label":"Mode","description":"","type":"enum","default":"local","choices":["local","off"]}),
            ),
        ),
    ])
}

#[test]
fn flattened_schema_round_trips_and_defaults_merge_without_mutation() {
    let schema = schema();
    validate_schema(&schema).unwrap();
    let wire = serde_json::to_value(&schema).unwrap();
    assert_eq!(wire["count"]["type"], "integer");
    assert!(wire["count"].get("kind").is_none());
    assert_eq!(
        serde_json::from_value::<BTreeMap<String, SettingDefinition>>(wire).unwrap(),
        schema
    );
    let overrides = BTreeMap::from([
        ("enabled".into(), json!(false)),
        ("days".into(), json!(30.5)),
    ]);
    let effective = effective_settings(&schema, &overrides).unwrap();
    assert_eq!(effective["count"], 12);
    assert_eq!(effective["days"], 30.5);
    assert_eq!(effective["enabled"], false);
    assert_eq!(schema["enabled"].default_value(), true);
    assert!(effective_settings(&BTreeMap::new(), &BTreeMap::new())
        .unwrap()
        .is_empty());
}

#[test]
fn schema_rejects_unknown_fields_and_invalid_defaults() {
    let valid =
        json!({"label":"Count","description":"","type":"integer","default":12,"min":0,"max":100});
    for extra in ["secret", "authority", "max_length", "choices"] {
        let mut wire = valid.clone();
        wire[extra] = json!(true);
        assert!(
            serde_json::from_value::<SettingDefinition>(wire).is_err(),
            "accepted {extra}"
        );
    }
    for wire in [
        json!({"label":"","description":"","type":"boolean","default":true}),
        json!({"label":"Bad\u{1b}","description":"","type":"boolean","default":true}),
        json!({"label":"Bad\u{202e}","description":"","type":"boolean","default":true}),
        json!({"label":"Count","description":"","type":"integer","default":12,"min":20,"max":100}),
        json!({"label":"Count","description":"","type":"integer","default":12,"min":100,"max":0}),
        json!({"label":"Mode","description":"","type":"enum","default":"local","choices":["off"]}),
        json!({"label":"Mode","description":"","type":"enum","default":"local","choices":["local","local"]}),
        json!({"label":"Text","description":"","type":"string","default":"too long","max_length":2}),
        json!({"label":"Text","description":"","type":"string","default":"","max_length":4097}),
    ] {
        let value = definition(wire);
        assert!(value.validate().is_err());
    }
}

#[test]
fn overrides_reject_wrong_types_ranges_and_host_authority() {
    let schema = schema();
    for (key, value) in [
        ("enabled", json!("true")),
        ("count", json!(1.5)),
        ("count", json!(101)),
        ("days", json!(0)),
        ("days", json!(3650.1)),
        ("prefix", json!("x".repeat(17))),
        ("prefix", json!("bad\ntext")),
        ("mode", json!("remote")),
        ("unknown", json!(true)),
        ("enabled", json!(null)),
        ("enabled", json!([])),
        ("enabled", json!({})),
    ] {
        assert!(
            effective_settings(&schema, &BTreeMap::from([(key.into(), value)])).is_err(),
            "accepted {key}"
        );
    }
    for key in [
        "prepared_job",
        "graph_recording",
        "graph_expansion",
        "data_root",
        "host_llm",
        "management",
        "permissions",
        "budgets",
        "network",
        "learning",
        "Bad",
        "a-b",
        "0x",
    ] {
        assert!(validate_key(key).is_err(), "accepted {key}");
    }
    let excessive = (0..25)
        .map(|n| (format!("option_{n}"), schema["enabled"].clone()))
        .collect();
    assert!(validate_schema(&excessive).is_err());
    for number in ["1e999", "NaN", "Infinity"] {
        assert!(serde_json::from_str::<Value>(number).is_err());
    }
}

#[test]
fn manifest_schema_is_optional_strict_and_duplicate_free() {
    let mut wire = json!({
        "schema_version":1,"id":"fixture","name":"Fixture","version":"0.1.0","kind":"memory",
        "protocol":{"major":1,"min_minor":0,"max_minor":0},"target":"x86_64-pc-windows-msvc",
        "entrypoint":"provider.exe","capabilities":["context","learning","host_llm"],
        "permissions":{"network":false,"host_llm":true},
        "files":[{"path":"provider.exe","bytes":64,"sha256":"a".repeat(64)}]
    });
    let parse = |wire: &Value| {
        super::super::manifest::parse_manifest(
            &serde_json::to_vec(wire).unwrap(),
            "x86_64-pc-windows-msvc",
        )
    };
    let legacy = parse(&wire).unwrap();
    assert!(legacy.settings.is_empty());
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("settings")
        .is_none());
    wire["settings"] = serde_json::to_value(schema()).unwrap();
    let manifest = parse(&wire).unwrap();
    assert_eq!(manifest.settings.len(), 5);
    let encoded = serde_json::to_string(&wire).unwrap();
    for duplicate in [
        encoded.replace("\"enabled\":{", "\"enabled\":{},\"enabled\":{"),
        encoded.replace(
            "\"label\":\"Enabled\"",
            "\"label\":\"other\",\"label\":\"Enabled\"",
        ),
    ] {
        assert!(super::super::manifest::parse_manifest(
            duplicate.as_bytes(),
            "x86_64-pc-windows-msvc"
        )
        .is_err());
    }
    let mut consent = super::super::settings::ExecutionGrant {
        manifest_sha256: "a".repeat(64),
        guarded_execution: true,
        network: false,
        learning: false,
        turn_model_learning: false,
    };
    validate_grant(&manifest, &consent).unwrap();
    consent.turn_model_learning = true;
    assert!(validate_grant(&manifest, &consent).is_err());
    consent.learning = true;
    validate_grant(&manifest, &consent).unwrap();
    consent.network = true;
    assert!(validate_grant(&manifest, &consent).is_err());
    consent.network = false;
    let mut no_model = manifest.clone();
    no_model.permissions.host_llm = false;
    assert!(validate_grant(&no_model, &consent).is_err());

    let executable_hash = "b".repeat(64);
    let executable = super::super::install::FilePin {
        path: "provider.exe".into(),
        bytes: 64,
        sha256: executable_hash.clone(),
        unix_mode: None,
    };
    let mut package = super::super::install::InstalledPackage {
        directory: "package".into(),
        entrypoint: std::path::Path::new("package").join("provider.exe"),
        manifest,
        pins: super::super::install::InstallPins {
            schema_version: 1,
            id: "fixture".into(),
            target: "x86_64-pc-windows-msvc".into(),
            manifest: super::super::install::FilePin {
                path: "plugin.json".into(),
                sha256: "a".repeat(64),
                ..executable.clone()
            },
            files: vec![executable],
        },
    };
    let disclosure = executable_disclosure(&package).unwrap();
    assert!(disclosure.contains("provider.exe") && disclosure.contains(&executable_hash));
    assert!(!disclosure.contains(&package.pins.manifest.sha256));
    let lines = consent_disclosures(&package, &consent).unwrap();
    assert!(lines.contains(&disclosure));
    assert!(lines.contains(&format!("Pinned manifest SHA256: {}", "a".repeat(64))));
    package.pins.files.clear();
    assert!(executable_disclosure(&package).is_err());
}

#[test]
fn budgets_preserve_effective_defaults_and_bound_injection_and_count() {
    let defaults: ProviderBudgets = serde_json::from_value(json!({})).unwrap();
    assert_eq!(defaults.context_tokens, 1024);
    assert_eq!(defaults.recall_tokens, 1024);
    assert_eq!(defaults.recall_injection(), 300);
    assert_eq!(defaults.recall_count(), 12);
    let mut budgets = ProviderBudgets {
        recall_tokens: 80,
        recall_inject_tokens: 900,
        max_records: 3,
        ..defaults
    };
    assert_eq!(budgets.recall_injection(), 80);
    assert_eq!(budgets.recall_count(), 3);
    budgets.recall_tokens = 0;
    budgets.max_records = 0;
    assert_eq!(budgets.recall_injection(), 0);
    assert_eq!(budgets.recall_count(), 0);
    budgets.context_tokens = 4097;
    assert!(budgets.validate().is_err());
    assert!(serde_json::from_value::<ProviderBudgets>(json!({"authority":true})).is_err());
}
