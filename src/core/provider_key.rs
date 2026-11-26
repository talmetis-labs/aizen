//! Secret-only provider rotation. Never round-trip the config through `CliConfig`: doing so would
//! discard unknown fields and normalize unrelated Memory/role settings. All diagnostics here are
//! closed codes, never errors derived from input, persisted keys, paths or serde messages.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::time::Duration;

const MAX_INPUT: usize = 64 * 1024;
const MAX_CONFIG: usize = 2 * 1024 * 1024;

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Error {
    InvalidInput,
    InvalidConfig,
    StaleTarget,
    Busy,
    WriteUncertain,
}

// Intentionally no Debug implementation on either secret-bearing request type.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    target: Target,
    key: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Profile {
        name: String,
        expected_url: String,
        expected_model: String,
    },
    Root {
        expected_url: String,
        expected_model: String,
    },
}

/// This command owns its exit and stdout; returning an anyhow error would invoke main's error
/// printer. Stdin is consumed before acquiring the lock so a slow sender cannot hold other writers.
pub(crate) fn run(stdin_flag: bool, output_format: Option<&str>) -> ! {
    let stdin = std::io::stdin();
    let result = read_request(stdin.lock(), stdin.is_terminal(), stdin_flag, output_format)
        .and_then(|request| {
            let path = crate::core::cli_config::config_path();
            let lock = crate::core::workspace_txn::store_lock("cli_config", "global");
            rotate(&path, &lock, request)
        });
    let exit = emit_result(&mut std::io::stdout().lock(), result);
    std::process::exit(exit);
}

/// Clap's ordinary diagnostics may echo unrecognized arguments. Even accidental attempts to pass
/// a key on argv must get only the fixed machine error; the dedicated help remains available.
pub(crate) fn handle_parse_error(
    error: clap::Error,
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> ! {
    let key_command = ["config", "provider", "key"]
        .into_iter()
        .all(|word| args.next().is_some_and(|arg| arg == word));
    if key_command && error.use_stderr() {
        std::process::exit(emit_result(
            &mut std::io::stdout().lock(),
            Err(Error::InvalidInput),
        ));
    }
    error.exit();
}

fn response(result: Result<()>) -> &'static str {
    match result {
        Ok(()) => r#"{"version":1,"ok":true}"#,
        Err(Error::InvalidInput) => r#"{"version":1,"ok":false,"error":"invalid_input"}"#,
        Err(Error::InvalidConfig) => r#"{"version":1,"ok":false,"error":"invalid_config"}"#,
        Err(Error::StaleTarget) => r#"{"version":1,"ok":false,"error":"stale_target"}"#,
        Err(Error::Busy) => r#"{"version":1,"ok":false,"error":"busy"}"#,
        Err(Error::WriteUncertain) => r#"{"version":1,"ok":false,"error":"write_uncertain"}"#,
    }
}

fn emit_result(writer: &mut impl Write, result: Result<()>) -> i32 {
    // Exactly one JSON object, no newline or secret-bearing diagnostics, even on a broken pipe.
    let failed = result.is_err();
    if writer.write_all(response(result).as_bytes()).is_err() || writer.flush().is_err() || failed {
        1
    } else {
        0
    }
}

fn read_request(
    reader: impl Read,
    terminal: bool,
    stdin_flag: bool,
    output_format: Option<&str>,
) -> Result<Request> {
    if terminal || !stdin_flag || output_format != Some("json") {
        return Err(Error::InvalidInput);
    }
    let mut bytes = Vec::new();
    reader
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidInput)?;
    if bytes.len() > MAX_INPUT {
        return Err(Error::InvalidInput);
    }
    let value = crate::plugin::memory_api::strict_json(&bytes).map_err(|_| Error::InvalidInput)?;
    let mut request: Request = serde_json::from_value(value).map_err(|_| Error::InvalidInput)?;
    // Check before trimming: a pasted newline must not become an apparently valid credential.
    if request.key.contains(['\r', '\n', '\0']) {
        return Err(Error::InvalidInput);
    }
    request.key = request.key.trim().to_owned();
    let (name, url) = match &request.target {
        Target::Profile {
            name, expected_url, ..
        } => (Some(name.as_str()), expected_url),
        Target::Root { expected_url, .. } => (None, expected_url),
    };
    if request.key.is_empty()
        || name.is_some_and(|name| name.trim().is_empty())
        || url.trim().is_empty()
    {
        return Err(Error::InvalidInput);
    }
    Ok(request)
}

fn rotate(path: &Path, lock_path: &Path, request: Request) -> Result<()> {
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(lock_path, Duration::ZERO)
        .map_err(|error| {
        if error
            .downcast_ref::<crate::core::repo_lock::LockBusy>()
            .is_some()
        {
            Error::Busy
        } else {
            Error::InvalidConfig
        }
    })?;
    // Read only after exclusion, and never recover missing/corrupt data as defaults.
    let before = crate::plugin::install::read_host_file(path, MAX_CONFIG)
        .map_err(|_| Error::InvalidConfig)?
        .ok_or(Error::InvalidConfig)?;
    let mut config = crate::plugin::memory_api::strict_json_bounded(&before, MAX_CONFIG)
        .map_err(|_| Error::InvalidConfig)?;
    patch(&mut config, request)?;
    let mut after = serde_json::to_vec_pretty(&config).map_err(|_| Error::WriteUncertain)?;
    after.push(b'\n');
    if after.len() > MAX_CONFIG {
        return Err(Error::InvalidConfig);
    }
    if before != after {
        // Same rolling, best-effort backup as cli_config::save; both files remain owner-only.
        let _ = crate::core::persist::atomic_write_owner_only_checked(
            &path.with_extension("prev.json"),
            &before,
        );
    }
    // Failure may occur after rename (directory fsync/ACL), so never promise nothing was written.
    crate::core::persist::atomic_write_owner_only_checked(path, &after)
        .map_err(|_| Error::WriteUncertain)
}

fn required_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(Error::InvalidConfig)
}

fn optional_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(Error::InvalidConfig),
    }
}

fn patch(config: &mut Value, request: Request) -> Result<()> {
    let root = config.as_object_mut().ok_or(Error::InvalidConfig)?;
    // Validate only the endpoint registry we own. Unknown/Memory/role values are opaque and are
    // never decoded, migrated, resolved, normalized or reserialized through a typed config.
    let root_url = optional_string(root, "base_url")?;
    let root_model = optional_string(root, "model")?;
    optional_string(root, "api_key")?;
    let active = optional_string(root, "active_provider")?.filter(|name| !name.is_empty());
    let profiles = match root.get("providers") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(profiles)) => profiles.as_slice(),
        _ => return Err(Error::InvalidConfig),
    };
    let mut names = std::collections::HashSet::new();
    let mut active_index = None;
    for (index, profile) in profiles.iter().enumerate() {
        let profile = profile.as_object().ok_or(Error::InvalidConfig)?;
        let name = required_string(profile, "name")?;
        let url = required_string(profile, "base_url")?;
        required_string(profile, "model")?;
        required_string(profile, "api_key")?;
        if name.trim().is_empty()
            || url.trim().is_empty()
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err(Error::InvalidConfig);
        }
        if active.is_some_and(|active| active.eq_ignore_ascii_case(name)) {
            active_index = Some(index);
        }
    }
    if active.is_some() && active_index.is_none() {
        return Err(Error::InvalidConfig); // dangling owner: never fall back to a root write
    }
    let (profile_index, mirror_root) = match request.target {
        Target::Root {
            expected_url,
            expected_model,
        } => {
            if active.is_some() {
                return Err(Error::StaleTarget);
            }
            let (url, model) = (
                root_url.ok_or(Error::InvalidConfig)?,
                root_model.unwrap_or_default(),
            );
            if url != expected_url || model != expected_model {
                return Err(Error::StaleTarget);
            }
            (None, true)
        }
        Target::Profile {
            name,
            expected_url,
            expected_model,
        } => {
            let index = profiles
                .iter()
                .position(|profile| {
                    profile["name"]
                        .as_str()
                        .is_some_and(|saved| saved.eq_ignore_ascii_case(&name))
                })
                .ok_or(Error::StaleTarget)?;
            if profiles[index]["base_url"].as_str() != Some(&expected_url)
                || profiles[index]["model"].as_str() != Some(&expected_model)
            {
                return Err(Error::StaleTarget);
            }
            let mirror = active_index == Some(index);
            if mirror && root_url != Some(expected_url.as_str()) {
                return Err(Error::StaleTarget);
            }
            (Some(index), mirror)
        }
    };
    if let Some(index) = profile_index {
        root.get_mut("providers").ok_or(Error::InvalidConfig)?[index]["api_key"] =
            Value::String(request.key.clone());
    }
    if mirror_root {
        root.insert("api_key".to_string(), Value::String(request.key));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "base_url": "https://primary.invalid/v1", "model": "main-model", "api_key": "old-main",
            "active_provider": "Primary",
            "providers": [
                {"name":"Primary", "base_url":"https://primary.invalid/v1", "model":"main-model", "api_key":"old-main", "future":{"kept":true}},
                {"name":"Backup", "base_url":"https://backup.invalid/v1", "model":"backup-model", "api_key":"old-backup"}
            ],
            "roles":{"oracle":{"provider":"Primary","api_key_ref":"env:ROLE_KEY","future":[1,null,false]}},
            "memory_plugins":{"future_schema":42,"opaque":{"secret":"fixture-memory-secret"}},
            "unknown":[{"nil":null,"integer":18446744073709551615u64},true,0.25]
        })
    }

    fn profile(name: &str) -> Request {
        let (url, model) = if name.eq_ignore_ascii_case("primary") {
            ("https://primary.invalid/v1", "main-model")
        } else {
            ("https://backup.invalid/v1", "backup-model")
        };
        Request {
            target: Target::Profile {
                name: name.to_owned(),
                expected_url: url.to_owned(),
                expected_model: model.to_owned(),
            },
            key: "new-fixture-key".to_owned(),
        }
    }

    fn root() -> Request {
        Request {
            target: Target::Root {
                expected_url: "https://primary.invalid/v1".into(),
                expected_model: "main-model".into(),
            },
            key: "new-fixture-key".into(),
        }
    }

    #[test]
    fn active_profile_case_insensitive_only_keys_change() {
        let mut actual = fixture();
        let mut expected = actual.clone();
        expected["api_key"] = json!("new-fixture-key");
        expected["providers"][0]["api_key"] = json!("new-fixture-key");
        patch(&mut actual, profile("pRiMaRy")).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn inactive_profile_does_not_touch_root_or_roles() {
        let mut actual = fixture();
        let mut expected = actual.clone();
        expected["providers"][1]["api_key"] = json!("new-fixture-key");
        patch(&mut actual, profile("BACKUP")).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn root_requires_no_owner_and_exact_endpoint_identity() {
        for owner in [None, Some(Value::Null), Some(json!(""))] {
            let mut actual = fixture();
            actual.as_object_mut().unwrap().remove("active_provider");
            if let Some(owner) = owner {
                actual["active_provider"] = owner;
            }
            let mut expected = actual.clone();
            expected["api_key"] = json!("new-fixture-key");
            patch(&mut actual, root()).unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(patch(&mut fixture(), root()), Err(Error::StaleTarget));
        let mut dangling = fixture();
        dangling["active_provider"] = json!("deleted");
        assert_eq!(patch(&mut dangling, root()), Err(Error::InvalidConfig));
    }

    #[test]
    fn changed_url_model_deleted_profile_and_root_mirror_fail_closed() {
        for field in ["base_url", "model"] {
            let mut actual = fixture();
            actual["providers"][0][field] = json!("changed");
            let before = actual.clone();
            assert_eq!(
                patch(&mut actual, profile("Primary")),
                Err(Error::StaleTarget)
            );
            assert_eq!(actual, before);
            actual = fixture();
            actual["active_provider"] = Value::Null;
            actual[field] = json!("changed");
            let before = actual.clone();
            assert_eq!(patch(&mut actual, root()), Err(Error::StaleTarget));
            assert_eq!(actual, before);
        }
        let mut actual = fixture();
        actual["base_url"] = json!("https://other.invalid/v1");
        let before = actual.clone();
        assert_eq!(
            patch(&mut actual, profile("Primary")),
            Err(Error::StaleTarget)
        );
        assert_eq!(actual, before);
        assert_eq!(
            patch(&mut fixture(), profile("Deleted")),
            Err(Error::StaleTarget)
        );
    }

    #[test]
    fn duplicate_names_and_malformed_registry_rejected_without_changes() {
        let mut duplicate = fixture();
        duplicate["providers"][1]["name"] = json!("PRIMARY");
        let mut bad_type = fixture();
        bad_type["providers"][1]["api_key"] = json!(false);
        let mut bad_active = fixture();
        bad_active["active_provider"] = json!({"name":"Primary"});
        for mut actual in [duplicate, bad_type, bad_active, json!(false)] {
            let before = actual.clone();
            assert_eq!(
                patch(&mut actual, profile("Primary")),
                Err(Error::InvalidConfig)
            );
            assert_eq!(actual, before);
        }
    }

    fn request_bytes(key: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"target":{"kind":"root","expected_url":"https://primary.invalid/v1","expected_model":"main-model"},"key":key})).unwrap()
    }

    #[test]
    fn stdin_trims_key_but_rejects_blank_controls_and_ambiguous_payloads() {
        let bytes = request_bytes("  fixture-key\t ");
        assert_eq!(
            read_request(bytes.as_slice(), false, true, Some("json"))
                .ok()
                .unwrap()
                .key,
            "fixture-key"
        );
        for key in [
            "",
            " \t ",
            "fixture\n",
            "\rfixture",
            "fi\0xture",
            "fi\nxture",
        ] {
            let bytes = request_bytes(key);
            assert!(matches!(
                read_request(bytes.as_slice(), false, true, Some("json")),
                Err(Error::InvalidInput)
            ));
        }
        for bytes in [
            br#"{"target":{"kind":"root","expected_url":"url"},"key":"secret"}"#.as_slice(),
            br#"{"target":{"kind":"root","expected_url":"url","expected_model":"model","name":"ignored"},"key":"secret"}"#,
            br#"{"target":{"kind":"root","kind":"profile"},"key":"secret"}"#,
            br#"{"target":{"kind":"root","expected_url":"url","expected_model":"model"},"key":"secret","key":"other"}"#,
            br#"{"key":"secret"} trailing-secret"#,
        ] {
            assert!(matches!(read_request(bytes, false, true, Some("json")), Err(Error::InvalidInput)));
        }
    }

    #[test]
    fn stdin_is_bounded_and_terminal_and_wrong_flags_read_nothing() {
        struct MustNotRead;
        impl Read for MustNotRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("must not read")
            }
        }
        for (terminal, flag, format) in [
            (true, true, Some("json")),
            (false, false, Some("json")),
            (false, true, None),
            (false, true, Some("text")),
        ] {
            assert!(matches!(
                read_request(MustNotRead, terminal, flag, format),
                Err(Error::InvalidInput)
            ));
        }
        let mut source = std::io::Cursor::new(vec![b'x'; MAX_INPUT * 2]);
        assert!(matches!(
            read_request(&mut source, false, true, Some("json")),
            Err(Error::InvalidInput)
        ));
        assert_eq!(source.position(), (MAX_INPUT + 1) as u64);
        let mut exact = request_bytes("fixture");
        exact.resize(MAX_INPUT, b' ');
        assert!(read_request(exact.as_slice(), false, true, Some("json")).is_ok());
    }

    #[test]
    fn rotation_persists_unknown_values_and_previous_bytes() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let path = dir.path().join("cli-config.json");
        let lock = dir.path().join("config.lock");
        let before = serde_json::to_vec(&fixture()).unwrap();
        std::fs::write(&path, &before).unwrap();
        rotate(&path, &lock, profile("Primary")).unwrap();
        assert_eq!(
            std::fs::read(path.with_extension("prev.json")).unwrap(),
            before
        );
        let mut expected = fixture();
        patch(&mut expected, profile("Primary")).unwrap();
        let actual: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(actual, expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [path.clone(), path.with_extension("prev.json")] {
                assert_eq!(
                    std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn corrupt_duplicate_oversize_and_missing_config_never_created_or_repaired() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let path = dir.path().join("cli-config.json");
        let lock = dir.path().join("config.lock");
        assert_eq!(rotate(&path, &lock, root()), Err(Error::InvalidConfig));
        assert!(!path.exists());
        for bytes in [
            b"not-json-fixture-secret".to_vec(),
            br#"{"api_key":"one","api_key":"two"}"#.to_vec(),
            br#"{"roles":{"oracle":{"api_key_ref":"one","api_key_ref":"two"}}}"#.to_vec(),
            b"[]".to_vec(),
            vec![b' '; MAX_CONFIG + 1],
        ] {
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(rotate(&path, &lock, root()), Err(Error::InvalidConfig));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(!path.with_extension("prev.json").exists());
        }
    }

    #[test]
    fn busy_lock_precedes_even_corrupt_config_read() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let path = dir.path().join("cli-config.json");
        let lock_path = dir.path().join("config.lock");
        std::fs::write(&path, b"corrupt-fixture-secret").unwrap();
        let _lock =
            crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO)
                .unwrap();
        assert_eq!(rotate(&path, &lock_path, root()), Err(Error::Busy));
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt-fixture-secret");
    }

    #[test]
    fn wire_responses_are_exact_static_secret_free_and_nonzero_on_failure() {
        let mut bytes = Vec::new();
        assert_eq!(emit_result(&mut bytes, Ok(())), 0);
        assert_eq!(bytes, br#"{"version":1,"ok":true}"#);
        for (error, code) in [
            (Error::InvalidInput, "invalid_input"),
            (Error::InvalidConfig, "invalid_config"),
            (Error::StaleTarget, "stale_target"),
            (Error::Busy, "busy"),
            (Error::WriteUncertain, "write_uncertain"),
        ] {
            bytes.clear();
            assert_eq!(emit_result(&mut bytes, Err(error)), 1);
            assert_eq!(
                bytes,
                format!(r#"{{"version":1,"ok":false,"error":"{code}"}}"#).into_bytes()
            );
        }
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture-secret-error"))
            }
        }
        let result = read_request(FailedReader, false, true, Some("json")).map(|_| ());
        assert!(!response(result).contains("fixture-secret"));
    }

    #[test]
    fn inactive_literal_rotation_ignores_env_override_and_role_references_resolve_new_key() {
        use crate::core::cli_config::{resolve_role, ResolvedEndpoint};
        use crate::core::config::{EnvGuard, TEST_HOME_LOCK};
        let _home_lock = TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = crate::core::file_placement::tests::TestDir::new();
        let _home = EnvGuard::set([("AIZEN_HOME", dir.path())]);
        let _api = EnvGuard::set([("AIZEN_API_KEY", "inherited-fixture-key")]);
        let _roles = EnvGuard::unset([
            "AIZEN_ORACLE_MODEL",
            "AIZEN_ORACLE_BASE_URL",
            "AIZEN_ORACLE_API_KEY",
            "AIZEN_APPLY_MODEL",
            "AIZEN_APPLY_BASE_URL",
            "AIZEN_APPLY_API_KEY",
        ]);
        let path = crate::core::cli_config::config_path();
        let lock = crate::core::workspace_txn::store_lock("cli_config", "global");
        let mut config = fixture();
        config.as_object_mut().unwrap().remove("memory_plugins");
        config["roles"] = json!({
            "oracle": {"provider":"Backup"},
            "apply": {"provider":"Backup", "api_key_ref":"independent-fixture-key"}
        });
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        rotate(&path, &lock, profile("Backup")).unwrap();
        let main = ResolvedEndpoint {
            base_url: "https://main.invalid/v1".into(),
            model: "main".into(),
            api_key: "main-fixture-key".into(),
        };
        let linked = resolve_role("oracle", &main);
        assert_eq!(linked.api_key, "new-fixture-key");
        assert_eq!(linked.base_url, "https://backup.invalid/v1");
        assert_eq!(
            resolve_role("apply", &main).api_key,
            "independent-fixture-key"
        );
        assert_eq!(
            std::env::var("AIZEN_API_KEY").unwrap(),
            "inherited-fixture-key"
        );
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["roles"], config["roles"]);
        assert_eq!(after["api_key"], config["api_key"]);
    }

    #[cfg(windows)]
    #[test]
    fn denied_atomic_replace_reports_uncertainty_without_raw_error() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = crate::core::file_placement::tests::TestDir::new();
        let path = dir.path().join("cli-config.json");
        let lock = dir.path().join("config.lock");
        let before = serde_json::to_vec(&fixture()).unwrap();
        std::fs::write(&path, &before).unwrap();
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        assert_eq!(
            rotate(&path, &lock, profile("Primary")),
            Err(Error::WriteUncertain)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn empty_model_precondition_matches_unselected_models_without_adding_a_model() {
        for model in [None, Some(Value::Null), Some(json!(""))] {
            let mut actual = json!({"base_url":"https://primary.invalid/v1", "api_key":"old"});
            if let Some(model) = model {
                actual["model"] = model;
            }
            let mut expected = actual.clone();
            expected["api_key"] = json!("new-fixture-key");
            let bytes = serde_json::to_vec(&json!({"target":{"kind":"root", "expected_url":"https://primary.invalid/v1", "expected_model":""}, "key":"new-fixture-key"})).unwrap();
            let request = read_request(bytes.as_slice(), false, true, Some("json"))
                .ok()
                .unwrap();
            patch(&mut actual, request).unwrap();
            assert_eq!(actual, expected);
        }
        let mut actual = fixture();
        actual["providers"][0]["model"] = json!("");
        let request = Request {
            target: Target::Profile {
                name: "Primary".into(),
                expected_url: "https://primary.invalid/v1".into(),
                expected_model: String::new(),
            },
            key: "new-fixture-key".into(),
        };
        patch(&mut actual, request).unwrap();
        for model in [json!(12), json!({}), json!(false)] {
            let mut actual = json!({"base_url":"https://primary.invalid/v1", "model":model});
            assert_eq!(patch(&mut actual, root()), Err(Error::InvalidConfig));
        }
    }

    #[test]
    fn current_active_selection_not_stale_snapshot_controls_root_mirroring() {
        let mut actual = fixture();
        actual["active_provider"] = json!("Backup");
        actual["base_url"] = actual["providers"][1]["base_url"].clone();
        actual["model"] = actual["providers"][1]["model"].clone();
        actual["api_key"] = actual["providers"][1]["api_key"].clone();
        let mut expected = actual.clone();
        expected["providers"][0]["api_key"] = json!("new-fixture-key");
        patch(&mut actual, profile("Primary")).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn config_bound_is_two_mib_without_relaxing_memory_wire_bound() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let path = dir.path().join("cli-config.json");
        let lock = dir.path().join("config.lock");
        let mut config = fixture();
        config["large_unknown"] = json!(vec!["x".repeat(96 * 1024); 12]);
        let bytes = serde_json::to_vec(&config).unwrap();
        assert!(bytes.len() > crate::plugin::memory_api::MAX_FRAME);
        assert!(bytes.len() < MAX_CONFIG);
        assert!(crate::plugin::memory_api::strict_json(&bytes).is_err());
        std::fs::write(&path, &bytes).unwrap();
        rotate(&path, &lock, profile("Primary")).unwrap();
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["large_unknown"], config["large_unknown"]);
    }

    #[test]
    fn key_command_help_advertises_machine_capability() {
        use clap::Parser;
        let result =
            crate::cli_args::Cli::try_parse_from(["aizen", "config", "provider", "key", "--help"]);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        for required in ["--stdin", "--output-format", "provider-key-v1"] {
            assert!(help.contains(required));
        }
        let parsed = crate::cli_args::Cli::try_parse_from([
            "aizen",
            "config",
            "provider",
            "key",
            "--stdin",
            "--output-format",
            "json",
        ])
        .unwrap();
        assert!(
            matches!(parsed.command, Some(crate::cli_args::Commands::Config { cmd: Some(crate::cli_args::ConfigCmd::Provider { cmd: crate::cli_args::ProviderConfigCmd::Key { stdin:true, output_format:Some(value) }}) }) if value == "json")
        );
    }
}
