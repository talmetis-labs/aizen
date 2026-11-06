//! Host-owned package lifecycle and provider-generation admission. Offline methods never launch code.

use super::install::{self, InstalledPackage};
use super::settings::{ExecutionGrant, MemoryPlugins};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) fn host_target() -> Result<&'static str> {
    #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
    return Ok("x86_64-pc-windows-msvc");
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    return Ok("x86_64-unknown-linux-musl");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Ok("aarch64-apple-darwin");
    #[cfg(not(any(
        all(windows, target_arch = "x86_64", target_env = "msvc"),
        all(target_os = "linux", target_arch = "x86_64", target_env = "musl"),
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    anyhow::bail!("native Memory packages are not supported for this Core target")
}

pub(crate) fn packages_root() -> PathBuf {
    crate::core::config::aizen_home().join("plugins")
}

pub(crate) fn default_data_root(id: &str) -> Result<PathBuf> {
    super::storage::default_root(id)
}

pub(crate) fn data_root_for(id: &str, package: &super::settings::PackageState) -> Result<PathBuf> {
    super::storage::resolve(id, package)
}

pub(crate) fn data_root(id: &str) -> Result<PathBuf> {
    let state = read_state()?;
    match state.packages.get(id) {
        Some(package) => data_root_for(id, package),
        None => default_data_root(id),
    }
}

pub(crate) fn read_state() -> Result<MemoryPlugins> {
    crate::core::cli_config::read_memory_plugins()
}

pub(crate) fn transaction_marker() -> PathBuf {
    crate::core::config::aizen_home()
        .join("plugin-state")
        .join("memory-transaction.json")
}

/// The same independent exclusive OS lock is held by admin changes and every physical provider
/// transaction. It is never pooled/reentered, including within the same process. Zero-wait avoids
/// lock-order cycles with callers that already own workspace/repository gates.
pub(crate) struct Admission {
    _lock: crate::core::repo_lock::RepoTxnLock,
}

impl Admission {
    pub(crate) fn acquire() -> Result<Self> {
        Self::acquire_inner(false)
    }

    /// Recovery bypasses only the updater marker, never unresolved physical provider work.
    pub(crate) fn acquire_update_recovery() -> Result<Self> {
        Self::acquire_inner(true)
    }

    fn acquire_inner(update_recovery: bool) -> Result<Self> {
        ensure!(crate::core::exec_ctx::current().is_none_or(|ctx| ctx.native_scope().is_none()),
            "Memory provider is busy: native-scoped execution cannot reenter a provider transaction");
        let path = crate::core::workspace_txn::store_lock("memory-plugins", "activation");
        install::ensure_private_root(path.parent().context("activation lock has no parent")?)?;
        crate::core::workspace_txn::NativeResource::resolve(&path)?.verify()?;
        let lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&path, Duration::ZERO)
            .context("Memory plugin activation or physical work is busy; no state changed")?;
        ensure!(!transaction_marker().try_exists()?,
            "Memory provider recovery required: a previous physical transaction has no durable settlement receipt; activation and uninstall are blocked");
        ensure!(
            update_recovery || !super::update::recovery_required()?,
            "Memory package update recovery required; run `aizen plugin recover-update mnemos`"
        );
        Ok(Self { _lock: lock })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Status {
    pub enabled: bool,
    pub selected: Option<String>,
    pub generation: u64,
    pub installed: usize,
    pub recovery_required: bool,
    pub state: &'static str,
    pub learning_outbox: Option<super::outbox::Status>,
    pub learning_outbox_invalid: bool,
}

pub(crate) fn status() -> Result<Status> {
    let settings = read_state()?;
    let recovery_required =
        transaction_marker().try_exists()? || super::update::recovery_required()?;
    let learning_outbox = super::outbox::status();
    let learning_outbox_invalid = learning_outbox.is_err();
    Ok(Status {
        state: if recovery_required {
            "recovery_required"
        } else if settings.enabled {
            "selected"
        } else if settings.selected.is_some() {
            "disabled"
        } else {
            "unavailable"
        },
        enabled: settings.enabled,
        selected: settings.selected,
        generation: settings.generation,
        installed: settings.packages.len(),
        recovery_required,
        learning_outbox: learning_outbox.ok(),
        learning_outbox_invalid,
    })
}

pub(crate) fn install_local(source: &Path) -> Result<InstalledPackage> {
    install_local_expected(source, None)
}

pub(crate) fn install_local_expected(
    source: &Path,
    expected_id: Option<&str>,
) -> Result<InstalledPackage> {
    let _admission = Admission::acquire()?;
    let target = host_target()?;
    install::ensure_private_root(&packages_root())?;
    let source = std::path::absolute(source)?;
    let package = match (source.is_dir(), expected_id) {
        (true, None) => install::install_directory(&packages_root(), &source, target)?,
        (false, None) => install::install_zip(&packages_root(), &source, target)?,
        (true, Some(id)) => {
            install::install_directory_expected(&packages_root(), &source, target, Some(id))?
        }
        (false, Some(id)) => {
            install::install_zip_expected(&packages_root(), &source, target, Some(id))?
        }
    };
    crate::core::cli_config::edit_memory_plugins(|state| state.record_install(package.pins.clone()))
        .context("package payload was installed but could not be recorded; it is NOT enabled or selected; keep the payload for manual recovery")?;
    Ok(package)
}

/// Fresh release installation only. Network and prompts occur before admission.
pub(crate) fn install_release(
    source: &Path,
    candidate: &super::releases::Candidate,
    expected_generation: u64,
) -> Result<InstalledPackage> {
    use super::releases::fail;
    candidate.validate()?;
    let _admission = Admission::acquire().map_err(|error| {
        if transaction_marker().try_exists().unwrap_or(true) {
            fail(
                "recovery_required",
                "Memory provider recovery is required before installation",
            )
        } else {
            error
        }
    })?;
    let state = read_state()?;
    if state.generation != expected_generation {
        return Err(fail(
            "stale",
            "Memory configuration changed; review installation again",
        ));
    }
    let path_exists = match std::fs::symlink_metadata(packages_root().join("mnemos")) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspecting existing Mnemos package path"),
    };
    if state.packages.contains_key("mnemos") || path_exists {
        return Err(fail("already_installed", "Mnemos is already installed or its package path exists; automatic replacement is not supported"));
    }
    let target = host_target().map_err(|_| {
        fail(
            "unsupported_target",
            "Mnemos packages are unavailable for this Core platform",
        )
    })?;
    ensure!(
        candidate.target == target,
        "reviewed Mnemos target differs from this Core"
    );
    install::ensure_private_root(&packages_root())?;
    let expectation = install::ReleaseExpectation {
        id: "mnemos",
        version: &candidate.version,
        archive_sha256: &candidate.sha256,
    };
    let result = install::install_zip_release(
        &packages_root(),
        &std::path::absolute(source)?,
        target,
        &expectation,
    );
    let package = result.map_err(|error| {
        if std::fs::symlink_metadata(packages_root().join("mnemos")).is_ok() {
            fail("ambiguous_result", "Mnemos package publication may have occurred; refresh and inspect the package, never retry automatically")
        } else { error }
    })?;
    crate::core::cli_config::edit_memory_plugins(|current| {
        ensure!(current.generation == expected_generation, "Memory configuration changed before package registration");
        current.record_install(package.pins.clone())
    }).map_err(|_| fail("ambiguous_result", "Mnemos payload was installed but registration failed; it is not enabled or selected. Preserve the payload for manual recovery, never retry automatically"))?;
    Ok(package)
}

pub(crate) fn info(id: &str) -> Result<InstalledPackage> {
    super::manifest::validate_id(id)?;
    let state = read_state()?;
    let package = state
        .packages
        .get(id)
        .context("Memory plugin is not installed")?;
    install::verify_installed(&packages_root(), &package.pins, host_target()?)
}

pub(crate) fn set_eligible(id: &str, enabled: bool) -> Result<()> {
    let _admission = Admission::acquire()?;
    if enabled {
        info(id)?;
    }
    crate::core::cli_config::edit_memory_plugins(|state| state.set_eligible(id, enabled))
}

/// A prompt reviews this exact generation and pinned payload; never silently commit a later one.
#[derive(Debug, Clone)]
pub(crate) struct PackageReview {
    pub generation: u64,
    pub package: InstalledPackage,
    pub state: super::settings::PackageState,
}

pub(crate) fn review(id: &str) -> Result<PackageReview> {
    let state = read_state()?;
    let package = info(id)?;
    let saved = state
        .packages
        .get(id)
        .context("Memory package is not installed")?
        .clone();
    ensure!(
        saved.pins == package.pins && read_state()?.generation == state.generation,
        "Memory configuration changed during review; reopen the provider"
    );
    Ok(PackageReview {
        generation: state.generation,
        package,
        state: saved,
    })
}

fn check_review(state: &MemoryPlugins, review: &PackageReview) -> Result<()> {
    ensure!(
        state.generation == review.generation
            && state
                .packages
                .get(&review.package.manifest.id)
                .is_some_and(|saved| saved.pins == review.package.pins),
        "Memory configuration changed after review; no changes saved, reopen the provider"
    );
    Ok(())
}

pub(crate) fn apply_grant(
    review: &PackageReview,
    consent: ExecutionGrant,
    select_and_enable: bool,
) -> Result<()> {
    let _admission = Admission::acquire()?;
    let id = &review.package.manifest.id;
    let package = info(id)?;
    ensure!(
        package.pins == review.package.pins,
        "package changed after consent review"
    );
    super::configuration::validate_grant(&package.manifest, &consent)?;
    crate::core::cli_config::edit_memory_plugins(|state| {
        check_review(state, review)?;
        if select_and_enable {
            state.select(id, consent)
        } else {
            state.update_grant(id, Some(consent))
        }
    })
}

/// The simple picker makes eligibility and activation one reviewed, all-or-nothing action.
pub(crate) fn activate(review: &PackageReview, consent: ExecutionGrant) -> Result<()> {
    let _admission = Admission::acquire()?;
    let package = info(&review.package.manifest.id)?;
    ensure!(
        package.pins == review.package.pins,
        "package changed after consent review"
    );
    super::configuration::validate_grant(&package.manifest, &consent)?;
    crate::core::cli_config::edit_memory_plugins(|state| {
        check_review(state, review)?;
        state.activate(&package.manifest.id, consent)
    })
}

pub(crate) fn set_learning(
    review: &PackageReview,
    enabled: bool,
    consent: ExecutionGrant,
) -> Result<()> {
    let _admission = Admission::acquire()?;
    let id = &review.package.manifest.id;
    let package = info(id)?;
    ensure!(
        package.pins == review.package.pins,
        "package changed after learning review"
    );
    super::configuration::validate_grant(&package.manifest, &consent)?;
    crate::core::cli_config::edit_memory_configuration(|state, policy| {
        check_review(state, review)?;
        ensure!(
            state.enabled && state.selected.as_ref() == Some(id),
            "choose an active Memory provider first"
        );
        if enabled {
            ensure!(
                consent.learning
                    && package
                        .manifest
                        .capabilities
                        .iter()
                        .any(|c| c == "learning"),
                "provider learning requires completed-turn sharing consent"
            );
        }
        state.update_grant(id, Some(consent))?;
        if policy.unwrap_or(true) != enabled {
            state.learning_policy_changed()?;
        }
        *policy = Some(enabled);
        Ok(())
    })
}

/// Policy-only changes never mint execution or sharing consent, and work with damaged payloads.
pub(crate) fn set_learning_policy(enabled: bool) -> Result<()> {
    let _admission = Admission::acquire()?;
    crate::core::cli_config::edit_memory_configuration(|state, policy| {
        if policy.unwrap_or(true) != enabled {
            state.learning_policy_changed()?;
        }
        *policy = Some(enabled);
        Ok(())
    })
}

/// Guarded policy-only change for a previously reviewed client snapshot. Damaged payloads do not
/// prevent turning learning off; generation validation and the policy edit share one transaction.
pub(crate) fn set_learning_policy_expected(enabled: bool, expected_generation: u64) -> Result<()> {
    let _admission = Admission::acquire()?;
    crate::core::cli_config::edit_memory_configuration(|state, policy| {
        ensure!(
            state.generation == expected_generation,
            "Memory configuration changed after learning review"
        );
        ensure!(
            !enabled,
            "client policy-only changes may only turn Learning Off"
        );
        if policy.unwrap_or(true) != enabled {
            state.learning_policy_changed()?;
        }
        *policy = Some(enabled);
        Ok(())
    })
}

/// Disable without verifying a possibly damaged payload, but never disable a newer selection.
pub(crate) fn disable_expected(expected_generation: u64) -> Result<()> {
    let _admission = Admission::acquire()?;
    crate::core::cli_config::edit_memory_plugins(|state| {
        ensure!(
            state.generation == expected_generation,
            "Memory configuration changed after disable review"
        );
        state.set_memory_enabled(false)
    })
}

pub(crate) fn set_storage(
    review: &PackageReview,
    root: Option<PathBuf>,
    workspace: Option<&Path>,
) -> Result<()> {
    let _admission = Admission::acquire()?;
    let id = &review.package.manifest.id;
    let package = info(id)?;
    ensure!(
        package.pins == review.package.pins,
        "package changed after storage review"
    );
    crate::core::cli_config::edit_memory_plugins(|state| {
        check_review(state, review)?;
        let requested = root
            .as_deref()
            .map(Path::to_path_buf)
            .unwrap_or(default_data_root(id)?);
        let normalized = super::storage::validate(&requested, id, state, workspace)?;
        let default = super::storage::validate(&default_data_root(id)?, id, state, workspace)?;
        state.set_storage(
            id,
            if normalized == default {
                None
            } else {
                Some(normalized)
            },
        )
    })
}

pub(crate) fn configure(
    review: &PackageReview,
    values: std::collections::BTreeMap<String, serde_json::Value>,
    budgets: Option<super::configuration::ProviderBudgets>,
) -> Result<()> {
    let _admission = Admission::acquire()?;
    let package = info(&review.package.manifest.id)?;
    ensure!(
        package.pins == review.package.pins,
        "package changed after configuration review"
    );
    super::configuration::effective_settings(&package.manifest.settings, &values)?;
    if let Some(budgets) = &budgets {
        budgets.validate()?;
    }
    crate::core::cli_config::edit_memory_plugins(|state| {
        check_review(state, review)?;
        state.configure(&package.manifest.id, values, budgets)
    })
}

pub(crate) fn clear_selection(expected_generation: u64) -> Result<()> {
    let _admission = Admission::acquire()?;
    crate::core::cli_config::edit_memory_plugins(|state| {
        ensure!(
            state.generation == expected_generation,
            "Memory configuration changed; selection was not cleared"
        );
        state.clear_selection()
    })
}

/// Revocation does not trust or execute the payload, including when its files are damaged.
pub(crate) fn revoke(id: &str, expected_generation: u64) -> Result<()> {
    super::manifest::validate_id(id)?;
    let _admission = Admission::acquire()?;
    crate::core::cli_config::edit_memory_plugins(|state| {
        ensure!(
            state.generation == expected_generation,
            "Memory configuration changed; consent was not revoked"
        );
        state.update_grant(id, None)
    })
}

pub(crate) fn set_memory_enabled(enabled: bool) -> Result<()> {
    let _admission = Admission::acquire()?;
    if enabled {
        let state = read_state()?;
        let id = state
            .selected
            .as_deref()
            .context("no Memory provider selected; use `aizen memory use <id>` explicitly")?;
        info(id)?;
    }
    crate::core::cli_config::edit_memory_plugins(|state| state.set_memory_enabled(enabled))
}

/// Confirmation uses the registered identity, not executable verification: a missing or damaged
/// package must still be disableable/removable without running or trusting its remaining payload.
pub(crate) fn uninstall_directory(id: &str) -> Result<PathBuf> {
    super::manifest::validate_id(id)?;
    ensure!(
        read_state()?.packages.contains_key(id),
        "Memory plugin is not installed"
    );
    Ok(packages_root().join(id))
}

pub(crate) fn uninstall(id: &str) -> Result<()> {
    super::manifest::validate_id(id)?;
    let _admission = Admission::acquire()?;
    let state = read_state()?;
    let package = state
        .packages
        .get(id)
        .context("Memory plugin is not installed")?;
    // Disable before examining payload bytes. Corrupt/partial removals keep their host pins and
    // selection for recovery, but never remain eligible for future execution.
    crate::core::cli_config::edit_memory_plugins(|state| state.set_eligible(id, false))?;
    let removal: Result<()> = (|| {
        if install::host_directory_exists(&packages_root().join(id))? {
            install::uninstall_verified(&packages_root(), &package.pins)?;
        }
        Ok(())
    })();
    removal.context(
        "uninstall incomplete; plugin disabled, data preserved, host pins retained for recovery",
    )?;
    crate::core::cli_config::edit_memory_plugins(|state| state.record_uninstall(id))
}

/// A queued event names its original generation. Dispatch must fail instead of redirecting it.
pub(crate) fn selected_at(generation: u64) -> Result<(String, super::settings::PackageState)> {
    let state = read_state()?;
    ensure!(
        state.generation == generation,
        "Memory provider generation changed; queued event discarded"
    );
    ensure!(state.enabled, "Memory is disabled");
    let id = state
        .selected
        .as_ref()
        .context("no Memory provider selected")?;
    let package = state
        .packages
        .get(id)
        .context("selected Memory provider is not installed")?;
    ensure!(
        package.enabled && package.grant.is_some(),
        "Memory provider has no current execution consent"
    );
    Ok((id.clone(), package.clone()))
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::install::{FilePin, InstallPins};
    use super::*;

    fn selected_fixture() -> MemoryPlugins {
        let pins = InstallPins {
            schema_version: 1,
            id: "fixture".into(),
            target: "x86_64-pc-windows-msvc".into(),
            manifest: FilePin {
                path: "plugin.json".into(),
                bytes: 1,
                sha256: "a".repeat(64),
                unix_mode: None,
            },
            files: vec![],
        };
        crate::core::cli_config::edit_memory_plugins(|state| {
            state.record_install(pins)?;
            state.set_eligible("fixture", true)?;
            state.select(
                "fixture",
                ExecutionGrant {
                    manifest_sha256: "a".repeat(64),
                    guarded_execution: true,
                    network: false,
                    learning: false,
                    turn_model_learning: false,
                },
            )
        })
        .unwrap();
        read_state().unwrap()
    }

    fn data_sentinel() -> PathBuf {
        let data = data_root("fixture").unwrap();
        install::ensure_private_root(&data).unwrap();
        let sentinel = data.join("preserve.txt");
        crate::core::persist::atomic_write_owner_only(&sentinel, b"preserved fixture data")
            .unwrap();
        sentinel
    }

    fn foreign_target_fixture(root: &Path) -> InstalledPackage {
        use std::io::{Cursor, Write};
        use zip::write::SimpleFileOptions;

        // Synthetic headers exercise package verification only; no test launches this payload.
        let mut native = [0u8; 64];
        let target = if cfg!(target_os = "linux") {
            native[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
            native[4..8].copy_from_slice(&0x0100000cu32.to_le_bytes());
            native[12..16].copy_from_slice(&2u32.to_le_bytes());
            "aarch64-apple-darwin"
        } else {
            native[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
            native[16..18].copy_from_slice(&2u16.to_le_bytes());
            native[18..20].copy_from_slice(&62u16.to_le_bytes());
            native[24..32].copy_from_slice(&1u64.to_le_bytes());
            "x86_64-unknown-linux-musl"
        };
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "id": "fixture",
            "name": "Foreign target fixture",
            "version": "0.1.0",
            "kind": "memory",
            "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
            "target": target,
            "entrypoint": "provider",
            "capabilities": [],
            "permissions": {"network": false, "host_llm": false},
            "files": [{"path": "provider", "bytes": native.len(),
                       "sha256": super::super::manifest::sha256(&native)}]
        }))
        .unwrap();
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes, mode) in [
            ("plugin.json", manifest.as_slice(), 0o600),
            ("provider", native.as_slice(), 0o700),
        ] {
            archive
                .start_file(
                    name,
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored)
                        .unix_permissions(mode),
                )
                .unwrap();
            archive.write_all(bytes).unwrap();
        }
        let source = root.join("foreign.zip");
        crate::core::persist::atomic_write_owner_only(
            &source,
            &archive.finish().unwrap().into_inner(),
        )
        .unwrap();
        let _admission = Admission::acquire().unwrap();
        install::ensure_private_root(&packages_root()).unwrap();
        let package = install::install_zip(&packages_root(), &source, target).unwrap();
        crate::core::cli_config::edit_memory_plugins(|state| {
            state.record_install(package.pins.clone())?;
            state.set_eligible("fixture", true)?;
            state.select(
                "fixture",
                ExecutionGrant {
                    manifest_sha256: package.pins.manifest.sha256.clone(),
                    guarded_execution: true,
                    network: false,
                    learning: false,
                    turn_model_learning: false,
                },
            )
        })
        .unwrap();
        package
    }

    pub(crate) fn native_review_fixture(root: &Path) -> InstalledPackage {
        let target = host_target().unwrap();
        let source = root.join("source");
        install::ensure_private_root(&source).unwrap();
        let mut bytes = vec![0u8; 256];
        let executable = if target.ends_with("windows-msvc") {
            bytes[..2].copy_from_slice(b"MZ");
            bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
            bytes[64..68].copy_from_slice(b"PE\0\0");
            bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
            bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
            bytes[84..86].copy_from_slice(&112u16.to_le_bytes());
            bytes[86..88].copy_from_slice(&2u16.to_le_bytes());
            bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
            bytes[104..108].copy_from_slice(&1u32.to_le_bytes());
            bytes[156..158].copy_from_slice(&3u16.to_le_bytes());
            "provider.exe"
        } else if target.contains("linux") {
            bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
            bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
            bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
            bytes[24..32].copy_from_slice(&1u64.to_le_bytes());
            "provider"
        } else {
            bytes[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
            bytes[4..8].copy_from_slice(&0x0100000cu32.to_le_bytes());
            bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
            "provider"
        };
        // Header-only fixture for offline administration; never execute it.
        crate::core::persist::atomic_write_owner_only(&source.join(executable), &bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                source.join(executable),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema_version":1,"id":"fixture","name":"Fixture","version":"0.1.0","kind":"memory",
            "protocol":{"major":1,"min_minor":0,"max_minor":0},"target":target,"entrypoint":executable,
            "capabilities":["records","context","learning","host_llm"],
            "permissions":{"network":false,"host_llm":true},
            "files":[{"path":executable,"bytes":bytes.len(),"sha256":super::super::manifest::sha256(&bytes)}]
        })).unwrap();
        crate::core::persist::atomic_write_owner_only(&source.join("plugin.json"), &manifest)
            .unwrap();
        install_local(&source).unwrap()
    }

    #[test]
    fn simple_activation_learning_and_storage_are_reviewed_atomic_and_nonexecuting() {
        if host_target().is_err() {
            return;
        }
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", home.as_path()),
            ("USERPROFILE", fixture.path()),
            ("HOME", fixture.path()),
        ]);
        let package = native_review_fixture(fixture.path());
        let review = review("fixture").unwrap();
        let consent = ExecutionGrant {
            manifest_sha256: package.pins.manifest.sha256.clone(),
            guarded_execution: true,
            network: false,
            learning: false,
            turn_model_learning: false,
        };
        let before = read_state().unwrap();
        let mut bad = consent.clone();
        bad.manifest_sha256 = "b".repeat(64);
        assert!(activate(&review, bad).is_err());
        assert_eq!(read_state().unwrap(), before);
        activate(&review, consent.clone()).unwrap();
        assert!(read_state().unwrap().enabled && read_state().unwrap().packages["fixture"].enabled);
        let bytes = std::fs::read(crate::core::cli_config::config_path()).unwrap();
        assert!(set_learning(&review, true, consent.clone()).is_err());
        assert!(set_storage(&review, Some(fixture.path().join("stale")), None).is_err());
        assert_eq!(
            std::fs::read(crate::core::cli_config::config_path()).unwrap(),
            bytes
        );
        let current = self::review("fixture").unwrap();
        assert!(set_learning(&current, true, consent.clone()).is_err());
        assert_eq!(
            std::fs::read(crate::core::cli_config::config_path()).unwrap(),
            bytes
        );
        let mut learning = consent;
        learning.learning = true;
        learning.turn_model_learning = true;
        set_learning(&current, true, learning.clone()).unwrap();
        let on = read_state().unwrap();
        set_learning_policy(false).unwrap();
        let off = read_state().unwrap();
        assert!(off.enabled && off.generation > on.generation);
        assert_eq!(off.packages["fixture"].grant, Some(learning.clone()));
        assert!(selected_at(on.generation).is_err());
        set_learning(&self::review("fixture").unwrap(), true, learning).unwrap();
        let stale = crate::core::cli_config::load();
        let custom = fixture.path().join("custom").join("records");
        set_storage(
            &self::review("fixture").unwrap(),
            Some(custom.clone()),
            None,
        )
        .unwrap();
        assert!(!custom.exists() && !home.join("memory-data").exists());
        let saved = read_state().unwrap();
        crate::core::cli_config::save(&stale).unwrap();
        assert_eq!(read_state().unwrap(), saved);
        set_storage(
            &self::review("fixture").unwrap(),
            Some(custom.clone()),
            None,
        )
        .unwrap();
        assert_eq!(read_state().unwrap().generation, saved.generation);
        assert!(selected_at(on.generation).is_err());
        let current = self::review("fixture").unwrap();
        let _busy = Admission::acquire().unwrap();
        assert!(set_storage(&current, None, None).is_err());
        assert_eq!(read_state().unwrap(), saved);
        drop(_busy);
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("keep"), b"custom data").unwrap();
        set_storage(&self::review("fixture").unwrap(), None, None).unwrap();
        let default = default_data_root("fixture").unwrap();
        std::fs::create_dir_all(&default).unwrap();
        std::fs::write(default.join("keep"), b"default data").unwrap();
        uninstall("fixture").unwrap();
        assert_eq!(std::fs::read(custom.join("keep")).unwrap(), b"custom data");
        assert_eq!(
            std::fs::read(default.join("keep")).unwrap(),
            b"default data"
        );
        assert!(!transaction_marker().exists() && !home.join("plugin-temp").exists());
    }

    #[test]
    fn reviewed_configuration_rejects_generation_and_pin_changes_without_mutating_state() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let package = foreign_target_fixture(fixture.path());
        let before = read_state().unwrap();
        let review = PackageReview {
            generation: before.generation,
            package,
            state: before.packages["fixture"].clone(),
        };
        check_review(&before, &review).unwrap();
        let mut next = before.clone();
        next.configure(
            "fixture",
            Default::default(),
            Some(super::super::configuration::ProviderBudgets {
                context_tokens: 0,
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(check_review(&next, &review).is_err());
        assert_eq!(read_state().unwrap(), before);
        let mut changed_pins = before.clone();
        changed_pins
            .packages
            .get_mut("fixture")
            .unwrap()
            .pins
            .manifest
            .sha256 = "b".repeat(64);
        let unchanged = changed_pins.clone();
        assert!(check_review(&changed_pins, &review).is_err());
        assert_eq!(changed_pins, unchanged);
    }

    #[test]
    fn revocation_and_unselection_do_not_need_trusted_payload_or_touch_data() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let before = selected_fixture();
        let sentinel = data_sentinel();
        assert!(info("fixture").is_err());
        assert!(revoke("fixture", before.generation + 1).is_err());
        assert_eq!(read_state().unwrap(), before);
        revoke("fixture", before.generation).unwrap();
        let revoked = read_state().unwrap();
        assert!(!revoked.enabled && revoked.selected.as_deref() == Some("fixture"));
        assert!(revoked.packages["fixture"].grant.is_none());
        assert!(clear_selection(before.generation).is_err());
        clear_selection(revoked.generation).unwrap();
        let cleared = read_state().unwrap();
        assert!(!cleared.enabled && cleared.selected.is_none());
        assert_eq!(cleared.packages, revoked.packages);
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserved fixture data");
    }

    #[test]
    fn foreign_target_uninstall_verifies_pinned_target_and_preserves_data() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let package = foreign_target_fixture(fixture.path());
        let before = read_state().unwrap();
        let sentinel = data_sentinel();
        assert!(info("fixture").is_err());
        uninstall("fixture").unwrap();
        let after = read_state().unwrap();
        assert!(after.packages.is_empty() && after.selected.is_none() && !after.enabled);
        assert!(after.generation > before.generation);
        assert!(!package.directory.exists() && !transaction_marker().exists());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserved fixture data");
    }

    #[test]
    fn corrupt_foreign_target_uninstall_disables_and_preserves_payload_pins_and_data() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let package = foreign_target_fixture(fixture.path());
        let before = read_state().unwrap();
        let sentinel = data_sentinel();
        let manifest = package.directory.join("plugin.json");
        let receipt = package.directory.join(".install-pins.json");
        let manifest_before = std::fs::read(&manifest).unwrap();
        let receipt_before = std::fs::read(&receipt).unwrap();
        let mut damaged = std::fs::read(&package.entrypoint).unwrap();
        damaged[63] ^= 1;
        std::fs::write(&package.entrypoint, &damaged).unwrap();
        for _ in 0..2 {
            let error = uninstall("fixture").unwrap_err();
            assert!(
                error.to_string().contains("host pins retained"),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("package SHA256 mismatch"),
                "{error:#}"
            );
            let after = read_state().unwrap();
            assert!(!after.enabled && !after.packages["fixture"].enabled);
            assert_eq!(after.selected, before.selected);
            assert_eq!(
                after.packages["fixture"].pins,
                before.packages["fixture"].pins
            );
            assert!(after.generation > before.generation);
            assert_eq!(std::fs::read(&package.entrypoint).unwrap(), damaged);
            assert_eq!(std::fs::read(&manifest).unwrap(), manifest_before);
            assert_eq!(std::fs::read(&receipt).unwrap(), receipt_before);
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserved fixture data");
            assert!(!transaction_marker().exists());
        }
    }

    #[test]
    fn missing_package_uninstall_preserves_data_and_retires_selection() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let before = selected_fixture();
        let sentinel = data_sentinel();
        assert_eq!(
            uninstall_directory("fixture").unwrap(),
            packages_root().join("fixture")
        );
        assert!(!packages_root().exists());
        uninstall("fixture").unwrap();
        let after = read_state().unwrap();
        assert!(after.packages.is_empty() && after.selected.is_none() && !after.enabled);
        assert!(after.generation > before.generation);
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserved fixture data");
        assert!(!packages_root().exists() && !transaction_marker().exists());
    }

    #[test]
    fn corrupt_package_uninstall_disables_without_removing_unknown_payload() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let before = selected_fixture();
        let sentinel = data_sentinel();
        let directory = packages_root().join("fixture");
        install::ensure_private_root(&directory).unwrap();
        let manifest = directory.join("plugin.json");
        let unknown = directory.join("unknown.txt");
        crate::core::persist::atomic_write_owner_only(&manifest, b"{").unwrap();
        crate::core::persist::atomic_write_owner_only(&unknown, b"do not delete").unwrap();
        assert_eq!(uninstall_directory("fixture").unwrap(), directory);
        for _ in 0..2 {
            let error = uninstall("fixture").unwrap_err();
            assert!(
                error.to_string().contains("host pins retained"),
                "{error:#}"
            );
            let after = read_state().unwrap();
            assert!(!after.enabled && !after.packages["fixture"].enabled);
            assert_eq!(after.selected, before.selected);
            assert_eq!(
                after.packages["fixture"].pins,
                before.packages["fixture"].pins
            );
            assert!(after.generation > before.generation);
            assert_eq!(std::fs::read(&manifest).unwrap(), b"{");
            assert_eq!(std::fs::read(&unknown).unwrap(), b"do not delete");
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserved fixture data");
        }
        // Simulate explicit repair completing payload removal after a partial uninstall.
        std::fs::remove_file(manifest).unwrap();
        std::fs::remove_file(unknown).unwrap();
        std::fs::remove_dir(directory).unwrap();
        uninstall("fixture").unwrap();
        assert!(read_state().unwrap().packages.is_empty());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserved fixture data");
    }

    #[test]
    fn non_directory_package_uninstall_disables_without_deletion() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        selected_fixture();
        install::ensure_private_root(&packages_root()).unwrap();
        let path = packages_root().join("fixture");
        crate::core::persist::atomic_write_owner_only(&path, b"not a package directory").unwrap();
        assert!(uninstall("fixture").is_err());
        let after = read_state().unwrap();
        assert!(!after.enabled && !after.packages["fixture"].enabled);
        assert_eq!(std::fs::read(path).unwrap(), b"not a package directory");
    }

    #[test]
    fn recovery_marker_blocks_uninstall_even_when_package_is_missing() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path())]);
        let before = selected_fixture();
        let marker = transaction_marker();
        install::ensure_private_root(marker.parent().unwrap()).unwrap();
        crate::core::persist::atomic_write_owner_only(&marker, b"unsettled fixture").unwrap();
        let error = uninstall("fixture").unwrap_err();
        assert!(error.to_string().contains("recovery required"), "{error:#}");
        assert_eq!(read_state().unwrap(), before);
        assert_eq!(std::fs::read(marker).unwrap(), b"unsettled fixture");
    }

    #[test]
    fn uninstall_confirmation_is_offline_and_requires_a_registered_id() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("missing-home");
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        for id in ["unknown", "../escape", "", "FIXTURE"] {
            assert!(uninstall_directory(id).is_err(), "{id}");
        }
        assert!(!home.exists());
    }
}
