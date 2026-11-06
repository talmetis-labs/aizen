//! Offline, non-executing Mnemos replacement and explicit evidence-based recovery.
//! Admission protects host-controlled paths, not arbitrary same-user filesystem races. Checked
//! file flushes / Unix directory fsync / Windows write-through renames provide process-crash
//! recovery on supported filesystems, not an unconditional hardware power-loss guarantee.
use super::{
    configuration, install, manager, releases,
    settings::{MemoryPlugins, PackageState},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const MAX_JOURNAL: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    operation_id: String,
    phase: String,
    old_generation: u64,
    old_state_sha256: String,
    new_state_sha256: String,
    restored_state_sha256: String,
    selected: Option<String>,
    enabled: bool,
    old: PackageState,
    new: PackageState,
    stage: String,
    backup: String,
    quarantine: String,
    old_identity: install::Identity,
    new_identity: install::Identity,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RecoveryStatus {
    pub recovery_required: bool,
    pub operation_id: Option<String>,
    pub state: String,
    pub detail: String,
}

pub(crate) fn marker_path() -> PathBuf {
    crate::core::config::aizen_home()
        .join("plugin-state")
        .join("mnemos-update.json")
}
pub(crate) fn recovery_required() -> Result<bool> {
    exists(&marker_path())
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn status(j: Option<&Journal>, state: &str, detail: &str, required: bool) -> RecoveryStatus {
    RecoveryStatus {
        recovery_required: required,
        operation_id: j.map(|j| j.operation_id.clone()),
        state: state.into(),
        detail: detail.into(),
    }
}
fn operation_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot create update identity"))?;
    Ok(format!("{:032x}", u128::from_le_bytes(bytes)))
}
fn leaf(path: &Path) -> Result<String> {
    Ok(path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid transaction leaf")?
        .into())
}
fn state_digest(state: &MemoryPlugins) -> Result<String> {
    Ok(super::manifest::sha256(&serde_json::to_vec(state)?))
}
fn validate(j: &Journal) -> Result<()> {
    for digest in [
        &j.old_state_sha256,
        &j.new_state_sha256,
        &j.restored_state_sha256,
    ] {
        super::manifest::validate_digest(digest)?;
    }
    ensure!(
        j.schema_version == 1
            && j.operation_id.len() == 32
            && j.operation_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid update operation identity"
    );
    ensure!(
        [
            "prepared",
            "backed_up",
            "published",
            "committed",
            "aborted",
            "restored",
            "restoring"
        ]
        .contains(&j.phase.as_str()),
        "invalid update phase"
    );
    ensure!(
        j.backup == format!(".mnemos-old-{}", j.operation_id)
            && j.quarantine == format!(".mnemos-new-{}", j.operation_id),
        "unconfined update evidence name"
    );
    let stage = j
        .stage
        .strip_prefix(".install-stage-")
        .context("invalid stage name")?;
    ensure!(
        !stage.is_empty()
            && stage.len() <= 64
            && stage.bytes().all(|b| b.is_ascii_digit() || b == b'-'),
        "invalid stage name"
    );
    ensure!(
        j.old.pins.id == "mnemos"
            && j.new.pins.id == "mnemos"
            && j.old.pins.target == j.new.pins.target,
        "invalid update package identity"
    );
    let mut state = MemoryPlugins::default();
    state.packages.insert("mnemos".into(), j.old.clone());
    state.validate()?;
    let mut expected = j.old.clone();
    expected.pins = j.new.pins.clone();
    expected.enabled = false;
    expected.grant = None;
    ensure!(
        j.new == expected && j.old_generation.checked_add(1).is_some(),
        "invalid intended package transition"
    );
    state.packages.insert("mnemos".into(), j.new.clone());
    state.validate()?;
    Ok(())
}
fn load() -> Result<Option<Journal>> {
    let Some(bytes) = install::read_host_file(&marker_path(), MAX_JOURNAL)? else {
        return Ok(None);
    };
    super::memory_api::strict_json(&bytes)?;
    let j: Journal = serde_json::from_slice(&bytes)?;
    validate(&j)?;
    Ok(Some(j))
}
fn bytes(j: &Journal) -> Result<Vec<u8>> {
    validate(j)?;
    let bytes = serde_json::to_vec(j)?;
    ensure!(
        bytes.len() <= MAX_JOURNAL,
        "update journal exceeds byte bound"
    );
    Ok(bytes)
}
fn create_journal(j: &Journal) -> Result<()> {
    let bytes = bytes(j)?;
    let path = marker_path();
    let parent = path.parent().context("missing journal parent")?;
    install::ensure_private_root(parent)?;
    crate::core::persist::sync_parent_checked(parent)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    crate::core::persist::harden_owner_only_checked(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    crate::core::persist::sync_parent_checked(&path)
}
fn save(j: &Journal) -> Result<()> {
    // Check the existing marker without following links before atomic replacement.
    let old = load()?.context("update marker disappeared")?;
    ensure!(old.operation_id == j.operation_id, "update marker changed");
    crate::core::persist::atomic_write_owner_only_checked(&marker_path(), &bytes(j)?)
}
fn milestone(j: &mut Journal, phase: &str) -> Result<()> {
    fault(match phase {
        "backed_up" => "before_backed_up",
        "published" => "before_published",
        "committed" => "before_committed",
        "restoring" => "before_restoring",
        _ => "before_recovery_receipt",
    })?;
    j.phase = phase.into();
    save(j)
}
fn verify(
    path: &Path,
    state: &PackageState,
    identity: &install::Identity,
) -> Result<install::InstalledPackage> {
    install::verify_transaction_package(path, &state.pins, identity)
}
fn move_package(
    from: &Path,
    to: &Path,
    state: &PackageState,
    identity: &install::Identity,
) -> Result<()> {
    verify(from, state, identity)?;
    ensure!(!exists(to)?, "transaction destination occupied");
    install::publish_no_replace(from, to)?;
    crate::core::persist::sync_parent_checked(to)?;
    verify(to, state, identity)?;
    Ok(())
}
fn retire(j: &Journal) -> Result<()> {
    // Retain a completed receipt forever; there is deliberately no automatic evidence sweeping.
    fault("before_retire")?;
    let receipt = marker_path().with_file_name(format!("mnemos-update-{}.json", j.operation_id));
    if exists(&receipt)? {
        ensure!(
            install::read_host_file(&receipt, MAX_JOURNAL)? == Some(bytes(j)?),
            "completed receipt conflicts"
        );
    } else {
        if let Err(error) = install::publish_no_replace(&marker_path(), &receipt) {
            // Write-through can fail after a namespace move. Restore a marker if the source
            // vanished rather than returning an ambiguous error with admission accidentally open.
            if !exists(&marker_path()).unwrap_or(true) {
                let _ = create_journal(j);
            }
            return Err(error);
        }
        if let Err(error) = crate::core::persist::sync_parent_checked(&receipt) {
            let _ = create_journal(j); // Restore blocking evidence if terminal rename durability fails.
            return Err(error);
        }
        return Ok(());
    }
    fs::remove_file(marker_path())?;
    if let Err(error) = crate::core::persist::sync_parent_checked(&marker_path()) {
        let _ = create_journal(j);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn update_release(
    source: &Path,
    candidate: &releases::Candidate,
    expected_generation: u64,
    expected_pins: &install::InstallPins,
) -> Result<install::InstalledPackage> {
    candidate.validate()?;
    let _admission = manager::Admission::acquire().map_err(|e| {
        if recovery_required().unwrap_or(true)
            || exists(&manager::transaction_marker()).unwrap_or(true)
        {
            releases::fail(
                "recovery_required",
                "Memory recovery is required before updating",
            )
        } else {
            releases::fail("busy", format!("Memory provider admission is busy ({e:#})"))
        }
    })?;
    let old = manager::read_state()?;
    if old.generation != expected_generation
        || !old
            .packages
            .get("mnemos")
            .is_some_and(|p| &p.pins == expected_pins)
    {
        return Err(releases::fail(
            "stale",
            "Memory configuration changed; review the Mnemos update again",
        ));
    }
    let target = manager::host_target()?;
    ensure!(
        candidate.target == target && expected_pins.target == target,
        "update target differs from this Core"
    );
    let installed = install::verify_installed(&manager::packages_root(), expected_pins, target)?;
    ensure!(
        releases::version(&candidate.version)? > releases::version(&installed.manifest.version)?,
        "update must be strictly newer than canonical installed version"
    );
    let expectation = install::ReleaseExpectation {
        id: "mnemos",
        version: &candidate.version,
        archive_sha256: &candidate.sha256,
    };
    let mut staged = install::stage_release(
        &manager::packages_root(),
        &std::path::absolute(source)?,
        target,
        &expectation,
        candidate.bytes,
    )?;
    let old_package = old.packages["mnemos"].clone();
    configuration::effective_settings(&staged.package.manifest.settings, &old_package.settings)?;
    if let Some(budgets) = &old_package.budgets {
        budgets.validate()?;
    }
    let mut intended = old.clone();
    intended.replace_package(
        expected_generation,
        &old_package,
        staged.package.pins.clone(),
    )?;
    ensure!(
        manager::read_state()? == old,
        "full Memory configuration changed during staging"
    );
    let id = operation_id()?;
    let mut restored = old.clone();
    restored.replace_package(expected_generation, &old_package, old_package.pins.clone())?;
    let mut j = Journal {
        schema_version: 1,
        operation_id: id.clone(),
        phase: "prepared".into(),
        old_generation: expected_generation,
        old_state_sha256: state_digest(&old)?,
        new_state_sha256: state_digest(&intended)?,
        restored_state_sha256: state_digest(&restored)?,
        selected: old.selected.clone(),
        enabled: old.enabled,
        old: old_package,
        new: intended.packages["mnemos"].clone(),
        stage: leaf(&staged.package.directory)?,
        backup: format!(".mnemos-old-{id}"),
        quarantine: format!(".mnemos-new-{id}"),
        old_identity: install::directory_identity(&installed.directory)?,
        new_identity: install::directory_identity(&staged.package.directory)?,
    };
    validate(&j)?;
    ensure!(
        !exists(&manager::packages_root().join(&j.backup))?
            && !exists(&manager::packages_root().join(&j.quarantine))?,
        "update evidence already exists"
    );
    // From this point any error/cancellation preserves the stage, even a partial journal write.
    staged.retain_for_recovery();
    let result = (|| {
        create_journal(&j)?;
        fault("prepared")?;
        move_package(
            &installed.directory,
            &manager::packages_root().join(&j.backup),
            &j.old,
            &j.old_identity,
        )?;
        fault("old_renamed")?;
        milestone(&mut j, "backed_up")?;
        fault("backed_up")?;
        move_package(
            &staged.package.directory,
            &installed.directory,
            &j.new,
            &j.new_identity,
        )?;
        fault("new_renamed")?;
        milestone(&mut j, "published")?;
        fault("published")?;
        fault("before_config_write")?;
        crate::core::cli_config::edit_memory_plugins_checked(|current| {
            ensure!(
                *current == old,
                "full Memory configuration changed before update commit"
            );
            current.replace_package(expected_generation, &j.old, j.new.pins.clone())
        })?;
        fault("config_written")?;
        ensure!(
            manager::read_state()? == intended,
            "updated package/configuration pair changed"
        );
        let package = verify(&installed.directory, &j.new, &j.new_identity)?;
        milestone(&mut j, "committed")?;
        fault("committed")?;
        retire(&j)?;
        Ok(package)
    })();
    result.map_err(|e: anyhow::Error| releases::fail("recovery_required", format!("Mnemos update settlement is uncertain; preserve all evidence and run explicit recover-update ({e:#})")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    Abort,
    Restore,
    Rollback,
    Finalize,
    Restored,
}
fn matches_config(state: &MemoryPlugins, j: &Journal, new: bool) -> bool {
    state_digest(state).ok().as_ref()
        == Some(if new {
            &j.new_state_sha256
        } else {
            &j.old_state_sha256
        })
        && state.generation == j.old_generation + u64::from(new)
        && state.selected == j.selected
        && state.enabled == (j.enabled && !(new && j.selected.as_deref() == Some("mnemos")))
        && state.packages.get("mnemos") == Some(if new { &j.new } else { &j.old })
}
fn restored_config(state: &MemoryPlugins, j: &Journal) -> bool {
    let mut restored = j.old.clone();
    restored.enabled = false;
    restored.grant = None;
    state_digest(state).ok().as_ref() == Some(&j.restored_state_sha256)
        && state.generation == j.old_generation + 1
        && state.selected == j.selected
        && state.enabled == (j.enabled && j.selected.as_deref() != Some("mnemos"))
        && state.packages.get("mnemos") == Some(&restored)
}
fn evidence(j: &Journal, state: &MemoryPlugins) -> Result<Evidence> {
    let root = manager::packages_root();
    let live = root.join("mnemos");
    let backup = root.join(&j.backup);
    let stage = root.join(&j.stage);
    let quarantine = root.join(&j.quarantine);
    let (l, b, s, q) = (
        exists(&live)?,
        exists(&backup)?,
        exists(&stage)?,
        exists(&quarantine)?,
    );
    // Inspect all present evidence, not just the path needed by a guessed phase.
    if b {
        verify(&backup, &j.old, &j.old_identity)?;
    }
    if s {
        verify(&stage, &j.new, &j.new_identity)?;
    }
    if q {
        verify(&quarantine, &j.new, &j.new_identity)?;
    }
    if matches_config(state, j, true) && l && b && !s && !q {
        verify(&live, &j.new, &j.new_identity)?;
        return Ok(Evidence::Finalize);
    }
    if restored_config(state, j) && l && !b && (s ^ q) {
        verify(&live, &j.old, &j.old_identity)?;
        return Ok(Evidence::Restored);
    }
    ensure!(
        matches_config(state, j, false),
        "configuration conflicts with update evidence"
    );
    if l && !b && s && !q {
        verify(&live, &j.old, &j.old_identity)?;
        return Ok(if j.phase == "restoring" {
            Evidence::Restored
        } else {
            Evidence::Abort
        });
    }
    if !l && b && (s ^ q) {
        return Ok(Evidence::Restore);
    }
    if l && b && !s && !q {
        verify(&live, &j.new, &j.new_identity)?;
        return Ok(Evidence::Rollback);
    }
    // Recovery itself may have crashed after restoring old bytes but before revoking consent.
    if l && !b && !s && q {
        verify(&live, &j.old, &j.old_identity)?;
        return Ok(Evidence::Restored);
    }
    anyhow::bail!("missing/conflicting update evidence; no repair guessed")
}

/// Offline and read-only. Malformed/unknown evidence is a blocked result, not an implicit repair.
pub(crate) fn inspect_recovery() -> Result<RecoveryStatus> {
    let j = match load() {
        Ok(None) => return Ok(status(None, "none", "No active Mnemos update", false)),
        Ok(Some(j)) => j,
        Err(_) => {
            return Ok(status(
                None,
                "blocked",
                "Update journal is unreadable or invalid; preserve all evidence",
                true,
            ))
        }
    };
    if exists(&manager::transaction_marker())? {
        return Ok(status(
            Some(&j),
            "blocked",
            "Physical provider recovery is required first",
            true,
        ));
    }
    match manager::read_state().and_then(|state| evidence(&j, &state)) {
        Ok(e) => Ok(status(
            Some(&j),
            match e {
                Evidence::Abort => "abort_prepared",
                Evidence::Restore => "restore_old",
                Evidence::Rollback => "rollback_uncommitted",
                Evidence::Finalize => "finalize_committed",
                Evidence::Restored => "finalize_restored",
            },
            "Explicit recovery confirmation and matching operation ID are required",
            true,
        )),
        Err(_) => Ok(status(
            Some(&j),
            "blocked",
            "Package identities or configuration conflict; preserve all evidence",
            true,
        )),
    }
}

pub(crate) fn recover(expected_operation_id: &str) -> Result<RecoveryStatus> {
    recover_inner(expected_operation_id).map_err(|error| {
        releases::fail(
            "recovery_required",
            format!("Mnemos update recovery did not settle; preserve all evidence ({error:#})"),
        )
    })
}
fn recover_inner(expected_operation_id: &str) -> Result<RecoveryStatus> {
    let _admission = manager::Admission::acquire_update_recovery()?;
    let mut j = load()?.context("no active Mnemos update")?;
    ensure!(
        j.operation_id == expected_operation_id,
        "update operation changed; inspect and confirm again"
    );
    let before = manager::read_state()?;
    let action = evidence(&j, &before)?;
    let root = manager::packages_root();
    let live = root.join("mnemos");
    let backup = root.join(&j.backup);
    if action == Evidence::Rollback {
        move_package(&live, &root.join(&j.quarantine), &j.new, &j.new_identity)?;
        fault("quarantined")?;
    }
    if matches!(action, Evidence::Restore | Evidence::Rollback) {
        milestone(&mut j, "restoring")?;
        move_package(&backup, &live, &j.old, &j.old_identity)?;
        fault("old_restored")?;
    }
    let physical = action != Evidence::Abort && action != Evidence::Finalize;
    if physical && !restored_config(&before, &j) {
        crate::core::cli_config::edit_memory_plugins_checked(|current| {
            ensure!(
                *current == before,
                "full Memory configuration changed during recovery"
            );
            current.replace_package(j.old_generation, &j.old, j.old.pins.clone())
        })?;
        fault("recovery_config_written")?;
    }
    let now = manager::read_state()?;
    // A previous write may have reached the namespace but failed its flush. Re-prove durability
    // without advancing generation again before retiring blocking evidence.
    if action == Evidence::Finalize || (physical && restored_config(&before, &j)) {
        crate::core::cli_config::edit_memory_plugins_checked(|current| {
            ensure!(
                *current == now,
                "configuration changed before recovery durability check"
            );
            Ok(())
        })?;
    }
    crate::core::persist::sync_parent_checked(&live)?;
    if action == Evidence::Finalize {
        ensure!(
            matches_config(&now, &j, true),
            "updated configuration changed"
        );
        verify(&live, &j.new, &j.new_identity)?;
        milestone(&mut j, "committed")?;
    } else {
        ensure!(
            if physical {
                restored_config(&now, &j)
            } else {
                matches_config(&now, &j, false)
            },
            "recovered configuration changed"
        );
        verify(&live, &j.old, &j.old_identity)?;
        milestone(&mut j, if physical { "restored" } else { "aborted" })?;
    }
    retire(&j)?;
    Ok(status(
        Some(&j),
        &j.phase,
        "Update settled; retained payload evidence was not deleted",
        false,
    ))
}

#[cfg(test)]
thread_local! { static FAULT: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) }; }
fn fault(_boundary: &'static str) -> Result<()> {
    #[cfg(test)]
    FAULT.with(|point| -> Result<()> {
        ensure!(
            *point.borrow() != Some(_boundary),
            "injected update boundary failure: {_boundary}"
        );
        Ok(())
    })?;
    Ok(())
}
#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
