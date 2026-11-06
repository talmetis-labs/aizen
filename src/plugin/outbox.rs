//! Host-owned completed-turn delivery receipts. Prepared work is never automatically replayed;
//! a dispatch claim survives crashes, even when the provider outcome was not observed.

use super::{install, manager, manifest, memory_api as api};
use anyhow::{ensure, Context, Result};
use api::Validate;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_RECEIPT: usize = 256 * 1024;
const MAX_RECEIPTS: usize = 256;
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Dispatching,
    Done,
    Failed,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    provider_id: String,
    manifest_sha256: String,
    context: api::ProviderContext,
    params: api::ProcessTurn,
    phase: Phase,
    result: Option<api::ProcessTurnResult>,
}

fn root() -> PathBuf {
    crate::core::config::aizen_home().join("plugin-state/memory-outbox")
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct Status {
    pub prepared: usize,
    pub dispatching: usize,
    pub done: usize,
    pub failed: usize,
    pub receipts: usize,
    pub reserved_bytes: u64,
    pub capacity: usize,
    pub available: usize,
}

/// Read-only advisory inventory, not settlement proof. Do not expose stored conversation text or
/// create a lock/root just to inspect absent Memory. Concurrent transitions can change these counts.
pub(crate) fn status() -> Result<Status> {
    status_at(&root())
}
fn status_at(root: &Path) -> Result<Status> {
    let mut status = Status {
        capacity: MAX_RECEIPTS.min((MAX_BYTES / MAX_RECEIPT as u64) as usize),
        ..Status::default()
    };
    if install::host_directory_exists(root)? {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            status.receipts = status.receipts.saturating_add(1);
            ensure!(
                status.receipts <= status.capacity,
                "Memory outbox inventory exceeds its limit"
            );
            let receipt =
                read(&entry.path())?.context("Memory outbox changed during inspection")?;
            ensure!(
                entry.path() == path(root, &receipt.params.operation_id),
                "Memory outbox filename does not match its operation"
            );
            match receipt.phase {
                Phase::Prepared => status.prepared += 1,
                Phase::Dispatching => status.dispatching += 1,
                Phase::Done => status.done += 1,
                Phase::Failed => status.failed += 1,
            }
        }
    }
    status.reserved_bytes = status.receipts as u64 * MAX_RECEIPT as u64;
    status.available = status.capacity.saturating_sub(status.receipts);
    Ok(status)
}

fn lock() -> Result<crate::core::repo_lock::RepoTxnLock> {
    let path = crate::core::workspace_txn::store_lock("memory-plugins", "learning-outbox");
    install::ensure_private_root(path.parent().context("outbox lock has no parent")?)?;
    crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&path, Duration::ZERO)
}
fn path(root: &Path, operation: &str) -> PathBuf {
    root.join(format!("{}.json", manifest::sha256(operation.as_bytes())))
}
fn read(path: &Path) -> Result<Option<Receipt>> {
    install::read_host_file(path, MAX_RECEIPT)?
        .map(|bytes| -> Result<Receipt> {
            let receipt: Receipt = serde_json::from_value(api::strict_json(&bytes)?)?;
            ensure!(
                receipt.schema_version == 1,
                "unsupported Memory outbox version"
            );
            manifest::validate_id(&receipt.provider_id)?;
            manifest::validate_digest(&receipt.manifest_sha256)?;
            receipt.context.validate()?;
            receipt.params.validate()?;
            ensure!(
                receipt.params.payload_sha256.as_deref()
                    == Some(
                        manifest::sha256(&receipt.params.payload_bytes(&receipt.context)?).as_str()
                    ),
                "Memory outbox payload integrity mismatch"
            );
            ensure!(
                (receipt.phase == Phase::Done) == receipt.result.is_some(),
                "invalid Memory outbox phase"
            );
            if let Some(result) = &receipt.result {
                result.validate()?;
                ensure!(
                    result.operation_id == receipt.params.operation_id
                        && result.payload_sha256 == receipt.params.payload_sha256,
                    "Memory outbox result binding mismatch"
                );
            }
            Ok(receipt)
        })
        .transpose()
}
fn save(path: &Path, receipt: &Receipt) -> Result<()> {
    let bytes = serde_json::to_vec(receipt)?;
    ensure!(
        bytes.len() <= MAX_RECEIPT,
        "Memory outbox receipt exceeds its limit"
    );
    crate::core::persist::atomic_write_owner_only(path, &bytes)?;
    // Core's generic persistence helper is best-effort for directory fsync; a delivery claim must
    // propagate that failure. Windows replacement already requests MOVEFILE_WRITE_THROUGH.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
        if let Some(grandparent) = parent.parent() {
            std::fs::File::open(grandparent)?.sync_all()?;
        }
    }
    Ok(())
}

/// Returns false for an identical already-recorded occurrence, regardless of its phase. Reading an
/// old receipt never runs its payload. Same identity with changed text/scope/pins is a conflict.
pub(super) fn prepare(context: &api::ProviderContext, params: &api::ProcessTurn) -> Result<bool> {
    context.validate()?;
    params.validate()?;
    ensure!(
        params.payload_sha256.as_deref()
            == Some(manifest::sha256(&params.payload_bytes(context)?).as_str()),
        "Memory completed-turn digest mismatch"
    );
    let _activation = manager::Admission::acquire()?;
    let (provider_id, package) = manager::selected_at(context.generation)?;
    let _lock = lock()?;
    prepare_at(
        &root(),
        Receipt {
            schema_version: 1,
            provider_id,
            manifest_sha256: package.pins.manifest.sha256,
            context: context.clone(),
            params: params.clone(),
            phase: Phase::Prepared,
            result: None,
        },
    )
}
fn prepare_at(root: &Path, receipt: Receipt) -> Result<bool> {
    install::ensure_private_root(root)?;
    let destination = path(root, &receipt.params.operation_id);
    if let Some(existing) = read(&destination)? {
        ensure!(
            existing.provider_id == receipt.provider_id
                && existing.manifest_sha256 == receipt.manifest_sha256
                && serde_json::to_value(&existing.context)?
                    == serde_json::to_value(&receipt.context)?
                && serde_json::to_value(&existing.params)?
                    == serde_json::to_value(&receipt.params)?,
            "Memory completed-turn identity already has a different payload or provider binding"
        );
        return Ok(false);
    }
    let prepared_size = serde_json::to_vec(&receipt)?.len();
    ensure!(
        prepared_size
            .saturating_add(api::MAX_LEARNING_RESULT)
            .saturating_add(16)
            <= MAX_RECEIPT,
        "Memory completed-turn input leaves insufficient space for its bounded acknowledgement"
    );
    let mut count = 0usize;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "invalid Memory outbox entry"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "reparse Memory outbox entry"
            );
        }
        count = count.saturating_add(1);
        // Reserve every admitted receipt's maximum, including queued work whose result can grow.
        ensure!(
            metadata.len() <= MAX_RECEIPT as u64,
            "oversized Memory outbox entry"
        );
        bytes = bytes.saturating_add(MAX_RECEIPT as u64);
    }
    ensure!(
        count < MAX_RECEIPTS && bytes.saturating_add(MAX_RECEIPT as u64) <= MAX_BYTES,
        "Memory learning outbox capacity reached; receipts preserved for explicit management"
    );
    save(&destination, &receipt)?;
    Ok(true)
}

pub(super) fn claim(context: &api::ProviderContext, params: &api::ProcessTurn) -> Result<()> {
    let _lock = lock()?;
    claim_at(&root(), context, params)
}
fn claim_at(root: &Path, context: &api::ProviderContext, params: &api::ProcessTurn) -> Result<()> {
    let destination = path(root, &params.operation_id);
    let mut receipt = read(&destination)?.context("Memory outbox receipt disappeared")?;
    ensure!(
        receipt.phase == Phase::Prepared,
        "Memory completed turn was already dispatched; no replay permitted"
    );
    ensure!(
        serde_json::to_value(&receipt.context)? == serde_json::to_value(context)?
            && serde_json::to_value(&receipt.params)? == serde_json::to_value(params)?,
        "Memory outbox binding changed"
    );
    receipt.phase = Phase::Dispatching;
    save(&destination, &receipt)
}

pub(super) fn finish(
    params: &api::ProcessTurn,
    result: Option<api::ProcessTurnResult>,
) -> Result<()> {
    let _lock = lock()?;
    finish_at(&root(), params, result)
}
fn finish_at(
    root: &Path,
    params: &api::ProcessTurn,
    result: Option<api::ProcessTurnResult>,
) -> Result<()> {
    let destination = path(root, &params.operation_id);
    let mut receipt = read(&destination)?.context("Memory outbox receipt disappeared")?;
    ensure!(
        receipt.phase == Phase::Dispatching,
        "Memory outbox claim changed"
    );
    ensure!(
        serde_json::to_value(&receipt.params)? == serde_json::to_value(params)?,
        "Memory outbox payload changed"
    );
    if let Some(result) = &result {
        result.validate()?;
        ensure!(
            result.operation_id == params.operation_id
                && result.payload_sha256 == params.payload_sha256,
            "Memory outbox acknowledgement mismatch"
        );
    }
    receipt.phase = if result.is_some() {
        Phase::Done
    } else {
        Phase::Failed
    };
    receipt.result = result;
    save(&destination, &receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_placement::tests::TestDir;
    use serde_json::json;
    struct Fixture {
        root: PathBuf,
        _parent: TestDir,
    }
    impl Fixture {
        fn new() -> Self {
            // Keep the outbox absent, but give it an existing owned parent: /tmp has a sticky
            // bit on Linux, and macOS temp paths may enter through the /var symlink.
            let parent = TestDir::new();
            let root = parent.path().join("outbox");
            Self {
                root,
                _parent: parent,
            }
        }
        fn receipt(&self) -> Receipt {
            let context: api::ProviderContext = serde_json::from_value(json!({
                "generation":1,"user_id":"fixture-user","workspace_id":"fixture-workspace",
                "workspace_root":self.root,"cwd":self.root,"device_id":"fixture-device","device_aliases":[],
                "session_id":"fixture-session","turn_id":"fixture-turn","boundary_id":"fixture-session",
                "data_root":self.root.join("data"),"settings":{},"granted_capabilities":["learning"],
                "budgets":{"context_tokens":100,"recall_tokens":100,"max_records":10}
            })).unwrap();
            let mut params = api::ProcessTurn {
                operation_id: "turn-fixture".into(),
                user_text: "fixture user request".into(),
                transcript: "fixture transcript".into(),
                dry_run: false,
                confirmed: false,
                payload_sha256: None,
                rendered_evidence: None,
            };
            params.payload_sha256 =
                Some(manifest::sha256(&params.payload_bytes(&context).unwrap()));
            Receipt {
                schema_version: 1,
                provider_id: "fixture".into(),
                manifest_sha256: "a".repeat(64),
                context,
                params,
                phase: Phase::Prepared,
                result: None,
            }
        }
    }
    #[test]
    fn duplicate_turns_never_requeue_in_any_durable_phase() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        assert!(prepare_at(&fixture.root, receipt.clone()).unwrap());
        let destination = path(&fixture.root, &receipt.params.operation_id);
        for phase in [
            Phase::Prepared,
            Phase::Dispatching,
            Phase::Failed,
            Phase::Done,
        ] {
            let mut stored = receipt.clone();
            stored.phase = phase.clone();
            if phase == Phase::Done {
                stored.result = Some(serde_json::from_value(json!({"accepted":true,
                    "operation_id":receipt.params.operation_id,"payload_sha256":receipt.params.payload_sha256,
                    "learning_mode":"offline","added":[],"reinforced":[],"review":[],"skipped_passive":true,"session_candidates":[]})).unwrap());
            }
            save(&destination, &stored).unwrap();
            assert!(!prepare_at(&fixture.root, receipt.clone()).unwrap());
            assert_eq!(read(&destination).unwrap().unwrap().phase, phase);
        }
    }
    #[test]
    fn claims_survive_reload_and_acknowledgements_are_bound_and_bounded() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        prepare_at(&fixture.root, receipt.clone()).unwrap();
        claim_at(&fixture.root, &receipt.context, &receipt.params).unwrap();
        assert!(claim_at(&fixture.root, &receipt.context, &receipt.params).is_err());
        let destination = path(&fixture.root, &receipt.params.operation_id);
        let before = std::fs::read(&destination).unwrap();
        let mut result: api::ProcessTurnResult = serde_json::from_value(json!({"accepted":true,
            "operation_id":receipt.params.operation_id,"payload_sha256":receipt.params.payload_sha256,
            "learning_mode":"offline","added":[],"reinforced":[],"review":[],
            "skipped_passive":false,"session_candidates":(0..24).map(|_| json!({"text":"x".repeat(1600),"anchor":null,"importance":5})).collect::<Vec<_>>()
        })).unwrap();
        result.operation_id = "wrong-operation".into();
        assert!(finish_at(&fixture.root, &receipt.params, Some(result.clone())).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), before);
        result.operation_id = receipt.params.operation_id.clone();
        finish_at(&fixture.root, &receipt.params, Some(result)).unwrap();
        assert_eq!(read(&destination).unwrap().unwrap().phase, Phase::Done);
        assert!(!prepare_at(&fixture.root, receipt.clone()).unwrap());
        assert!(finish_at(&fixture.root, &receipt.params, None).is_err());
        std::fs::write(&destination, b"{corrupt").unwrap();
        assert!(prepare_at(&fixture.root, receipt).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"{corrupt");
    }

    #[test]
    fn queued_receipts_reserve_terminal_capacity_before_dispatch() {
        let fixture = Fixture::new();
        let original = fixture.receipt();
        let capacity = (MAX_BYTES / MAX_RECEIPT as u64) as usize;
        for index in 0..capacity {
            let mut receipt = original.clone();
            receipt.params.operation_id = format!("turn-{index}");
            assert!(prepare_at(&fixture.root, receipt).unwrap());
        }
        let before = std::fs::read_dir(&fixture.root).unwrap().count();
        assert_eq!(before, capacity);
        assert!(prepare_at(&fixture.root, original).is_err());
        assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), before);
    }

    #[test]
    fn offline_status_counts_bounded_receipts_without_creating_or_exposing_content() {
        let fixture = Fixture::new();
        let empty = status_at(&fixture.root).unwrap();
        assert_eq!(empty.receipts, 0);
        assert_eq!(empty.available, 64);
        assert!(!fixture.root.exists());
        for (index, phase) in [
            Phase::Prepared,
            Phase::Dispatching,
            Phase::Done,
            Phase::Failed,
        ]
        .into_iter()
        .enumerate()
        {
            let mut receipt = fixture.receipt();
            receipt.params.operation_id = format!("turn-{index}");
            receipt.params.transcript = "PRIVATE_TRANSCRIPT_CANARY".into();
            receipt.params.payload_sha256 = Some(manifest::sha256(
                &receipt.params.payload_bytes(&receipt.context).unwrap(),
            ));
            prepare_at(&fixture.root, receipt.clone()).unwrap();
            receipt.phase = phase.clone();
            if phase == Phase::Done {
                receipt.result = Some(serde_json::from_value(json!({"accepted":true,
                    "operation_id":receipt.params.operation_id,"payload_sha256":receipt.params.payload_sha256,
                    "learning_mode":"offline","added":[],"reinforced":[],"review":[],"useful":[],"already_useful":[],
                    "skipped_passive":true,"session_candidates":[]})).unwrap());
            }
            save(&path(&fixture.root, &receipt.params.operation_id), &receipt).unwrap();
        }
        let before: Vec<_> = std::fs::read_dir(&fixture.root)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        let status = status_at(&fixture.root).unwrap();
        assert_eq!(
            (
                status.prepared,
                status.dispatching,
                status.done,
                status.failed
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(status.receipts, 4);
        assert_eq!(status.available, 60);
        assert_eq!(status.reserved_bytes, 4 * MAX_RECEIPT as u64);
        assert!(!serde_json::to_string(&status)
            .unwrap()
            .contains("PRIVATE_TRANSCRIPT_CANARY"));
        for (path, bytes) in before {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        std::fs::write(fixture.root.join("corrupt.json"), b"{broken").unwrap();
        assert!(status_at(&fixture.root).is_err());
        assert_eq!(
            std::fs::read(fixture.root.join("corrupt.json")).unwrap(),
            b"{broken"
        );
    }

    #[test]
    fn offline_status_rejects_misnamed_receipts_and_special_roots() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        prepare_at(&fixture.root, receipt.clone()).unwrap();
        std::fs::rename(
            path(&fixture.root, &receipt.params.operation_id),
            fixture.root.join("wrong.json"),
        )
        .unwrap();
        assert!(status_at(&fixture.root).is_err());
        let file = Fixture::new();
        std::fs::write(&file.root, b"not a directory").unwrap();
        assert!(status_at(&file.root).is_err());
        assert_eq!(std::fs::read(&file.root).unwrap(), b"not a directory");
        std::fs::remove_file(&file.root).unwrap();
    }

    #[test]
    fn changed_payload_scope_or_pins_cannot_replace_existing_receipts() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        prepare_at(&fixture.root, receipt.clone()).unwrap();
        let destination = path(&fixture.root, &receipt.params.operation_id);
        let before = std::fs::read(&destination).unwrap();
        for field in ["payload", "scope", "pins"] {
            let mut changed = receipt.clone();
            match field {
                "payload" => changed.params.user_text.push_str(" changed"),
                "scope" => changed.context.workspace_id.push_str(" changed"),
                "pins" => changed.manifest_sha256 = "b".repeat(64),
                _ => unreachable!(),
            }
            changed.params.payload_sha256 = Some(manifest::sha256(
                &changed.params.payload_bytes(&changed.context).unwrap(),
            ));
            assert!(prepare_at(&fixture.root, changed).is_err());
            assert_eq!(std::fs::read(&destination).unwrap(), before);
        }
    }
}
