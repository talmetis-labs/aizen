//! Host-only durable authority. Provider-authored files can never register a storage root.
use super::super::{install, storage};
use super::{
    engine,
    protocol::{operation_id_valid, Operation, State},
};
use crate::sandbox::request::{path_within, paths_overlap, planned_directory};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
const MAX_HISTORY: usize = 4096;
const MAX_RECORD: u64 = 256 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub schema_version: u32,
    pub operation: Operation,
    pub pins: install::InstallPins,
    pub generation: u64,
    pub envelope: PathBuf,
    pub envelope_identity: (u64, u64),
    pub plan_sha256: Option<String>,
    /// Terminal host receipt. Normal health checks never rerun historical source validation.
    pub published_identity: Option<(u64, u64)>,
    #[serde(default = "default_format")]
    pub engine_format: String,
    #[serde(default)]
    pub authorized_attachments: Vec<super::protocol::Attachment>,
}
fn default_format() -> String {
    engine::FORMAT.into()
}
pub(super) fn root(home: &Path) -> PathBuf {
    home.join("plugin-state").join("adoption")
}
pub(super) fn current_root() -> PathBuf {
    root(&crate::core::config::aizen_home())
}
pub(super) fn directory(id: &str) -> Result<PathBuf> {
    operation_id_valid(id)?;
    Ok(current_root().join(id))
}
pub(super) fn plan_path(id: &str) -> Result<PathBuf> {
    Ok(directory(id)?.join("plan.json"))
}
pub(super) fn identity(path: &Path) -> Result<(u64, u64)> {
    let resolved = planned_directory(path)?;
    let md = std::fs::symlink_metadata(&resolved)?;
    ensure!(md.is_dir(), "adoption identity must be a directory");
    Ok(crate::core::file_placement::NodeStamp::capture(&resolved, &md, false)?.identity())
}
pub(super) fn read(path: &Path, max: u64) -> Result<Vec<u8>> {
    crate::core::file_placement::read_existing_regular(path, max)
}
fn load_in(home: &Path, id: &str) -> Result<Record> {
    operation_id_valid(id)?;
    let path = root(home).join(id).join("operation.json");
    let record: Record = serde_json::from_slice(&read(&path, MAX_RECORD)?)?;
    ensure!(
        record.schema_version == 1
            && record.operation.operation_id == id
            && record.operation.provider == "mnemos"
            && record.pins.id == "mnemos",
        "invalid host adoption record"
    );
    ensure!(
        Path::new(&record.operation.data_root) == record.envelope.join("data")
            && Path::new(&record.operation.journal) == record.envelope.join("journal")
            && Path::new(&record.operation.backup) == record.envelope.join("journal/backup"),
        "invalid host adoption lifecycle paths"
    );
    Ok(record)
}
pub(super) fn load(id: &str) -> Result<Record> {
    load_in(&crate::core::config::aizen_home(), id)
}
pub(super) fn save(record: &Record) -> Result<()> {
    let directory = directory(&record.operation.operation_id)?;
    install::ensure_private_root(&directory)?;
    let bytes = serde_json::to_vec(record)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD,
        "adoption host record exceeds bound"
    );
    crate::core::persist::atomic_write_owner_only(&directory.join("operation.json"), &bytes)
}
pub(super) fn ids_in(home: &Path) -> Result<Vec<String>> {
    let root = root(home);
    if !root.try_exists()? {
        return Ok(vec![]);
    }
    planned_directory(&root)?;
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        ensure!(
            ids.len() < MAX_HISTORY,
            "adoption history bound reached; manual archival required"
        );
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid adoption history name"))?;
        operation_id_valid(&id)?;
        ensure!(
            entry.file_type()?.is_dir(),
            "invalid adoption history entry"
        );
        ids.push(id);
    }
    ids.sort();
    ids.reverse();
    Ok(ids)
}
pub(super) fn create_id() -> Result<String> {
    ensure!(
        ids_in(&crate::core::config::aizen_home())?.len() < MAX_HISTORY,
        "adoption history is full"
    );
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random)
        .map_err(|_| anyhow::anyhow!("adoption operation entropy unavailable"))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}
pub(super) fn load_plan(record: &Record) -> Result<engine::Plan> {
    let bytes = read(
        &plan_path(&record.operation.operation_id)?,
        engine::MAX_PLAN,
    )?;
    ensure!(
        record.plan_sha256.as_deref() == Some(&super::super::manifest::sha256(&bytes)),
        "immutable adoption plan changed"
    );
    let plan = engine::Plan::parse_authorized(
        &bytes,
        &record.operation.source,
        &record.operation.data_root,
        &record.engine_format,
        &record.authorized_attachments,
    )?;
    ensure!(
        record.operation.reviewed_digest.as_deref() == Some(&plan.digest),
        "adoption plan differs from host review"
    );
    Ok(plan)
}
pub(super) fn public(record: Record) -> Result<Operation> {
    public_with_marker(record, &super::super::manager::transaction_marker())
}
pub(super) fn public_with_marker(mut record: Record, marker: &Path) -> Result<Operation> {
    if marker.try_exists()? {
        let value: serde_json::Value = serde_json::from_slice(&read(marker, MAX_RECORD)?)?;
        if value.get("kind").and_then(|v| v.as_str()) == Some("adoption")
            && value.get("operation_id").and_then(|v| v.as_str())
                == Some(record.operation.operation_id.as_str())
        {
            record.operation.state = State::SettlementUnknown;
            record.operation.message = Some("Physical settlement is not durably proven. No automatic retry or recovery launch is permitted.".into());
        }
    } else if matches!(record.operation.state, State::Planning | State::Interrupted)
        && record.operation.reviewed_digest.is_none()
    {
        record.operation.state = State::CheckFailed;
        record.operation.message = Some(
            "Check did not produce a reviewed plan. Check again; there is no transfer to resume."
                .into(),
        );
    }
    Ok(record.operation)
}
/// Return true only for an exact, published, host-registered data leaf. Also reserve all envelopes
/// (including external/unpublished ones), so siblings, prefixes and another provider cannot inherit
/// authority from a successful migration. Called by every normal storage review and launch.
pub(crate) fn registered_storage(root: &Path, provider: &str, home: &Path) -> Result<bool> {
    let mut permitted = false;
    for id in ids_in(home)? {
        let record = load_in(home, &id)?;
        if !paths_overlap(root, &record.envelope) {
            continue;
        }
        let data = Path::new(&record.operation.data_root);
        ensure!(
            provider == record.operation.provider
                && same(root, data)
                && record.operation.state == State::PublishedVerified
                && record.published_identity.is_some(),
            "adoption envelope is reserved; only its exact verified data root may be selected"
        );
        ensure!(
            identity(&record.envelope)? == record.envelope_identity
                && Some(identity(data)?) == record.published_identity,
            "registered adoption root or parent identity changed"
        );
        permitted = true;
    }
    Ok(permitted)
}
pub(super) fn same(a: &Path, b: &Path) -> bool {
    path_within(a, b) && path_within(b, a)
}
pub(super) fn validate_envelope(record: &Record) -> Result<()> {
    ensure!(
        same(&planned_directory(&record.envelope)?, &record.envelope)
            && identity(&record.envelope)? == record.envelope_identity,
        "adoption operation parent changed"
    );
    ensure!(
        Path::new(&record.operation.data_root) == record.envelope.join("data"),
        "adoption destination changed"
    );
    Ok(())
}
pub(super) fn validate_paths(source: &Path, envelope: &Path) -> Result<()> {
    ensure!(
        !paths_overlap(source, envelope),
        "adoption source and destination overlap"
    );
    let home = planned_directory(&crate::core::config::aizen_home())?;
    ensure!(
        !path_within(&home, source)
            && !path_within(&planned_directory(&crate::core::config::home_dir())?, source),
        "source cannot contain HOME or AIZEN_HOME; select the whole legacy cli-memory directory instead"
    );
    let state = super::super::manager::read_state()?;
    for (id, package) in &state.packages {
        for path in [storage::default_root(id)?, storage::resolve(id, package)?] {
            let path = planned_directory(&path)?;
            ensure!(
                !paths_overlap(envelope, &path) && !paths_overlap(source, &path),
                "adoption overlaps an existing provider store"
            );
        }
    }
    for name in [
        "plugins",
        "plugin-state",
        "plugin-temp",
        "models",
        "memory-migrations",
        "memory-data",
    ] {
        let path = home.join(name);
        ensure!(
            !paths_overlap(source, &path),
            "legacy source overlaps host-managed provider/control storage"
        );
    }
    Ok(())
}
