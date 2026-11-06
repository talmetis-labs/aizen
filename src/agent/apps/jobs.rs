//! Bounded, offline job bookkeeping. No adapter is enabled by this module.
//!
//! An `OwnedJob` is process-local authority, never reconstructed from a journal or an operation ID.
//! Persisted Prepared/Unknown is an intent fence, NOT proof that dispatch has or has not happened.
//! The controller must hold its lifetime workspace/sidecar admission guards before calling `begin`
//! and through effect settlement. This journal alone neither excludes workspace edits nor physically
//! cancels an application. Locks and the final fingerprint check coordinate compliant writers; path
//! checks/CAS are best effort against non-cooperating mutation, not kernel confinement or an atomic
//! conditional rename. No lease expiry, automatic replay, recovery adoption, or record eviction exists.

use super::contract::{BatchRequest, CapabilityManifest, TargetBinding};
use crate::core::file_placement::{is_link_or_reparse, NodeStamp};
use crate::core::persist::{self, FileFingerprint};
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::NativeResource;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const VERSION: u32 = 1;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_RECORDS: usize = 128;
const MAX_SCOPE: usize = 64;
const JOURNAL: &str = "operations.json";
const LOCK: &str = "operations.lock";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Prepared,
    Running,
    Succeeded,
    Partial,
    Failed,
    CancelRequested,
    Cancelled,
    Unknown,
}

impl ExecutionState {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Partial | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    NotStarted,
    Active,
    Settled,
    Unknown,
}

/// A terminal execution outcome is separate from proof that all effects have settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalState {
    Succeeded,
    Partial,
    Failed,
    Cancelled,
}

impl From<TerminalState> for ExecutionState {
    fn from(value: TerminalState) -> Self {
        match value {
            TerminalState::Succeeded => Self::Succeeded,
            TerminalState::Partial => Self::Partial,
            TerminalState::Failed => Self::Failed,
            TerminalState::Cancelled => Self::Cancelled,
        }
    }
}

/// Durable metadata only: no source operations, credentials, screenshots, or adapter prose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRecord {
    version: u32,
    operation_id: String,
    request_digest: String,
    target: TargetBinding,
    canonical_project_root: PathBuf,
    owner_scope: String,
    execution: ExecutionState,
    effects: EffectState,
    sequence: u64,
}

impl JobRecord {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }
    pub fn target(&self) -> &TargetBinding {
        &self.target
    }
    pub fn owner_scope(&self) -> &str {
        &self.owner_scope
    }
    pub fn execution(&self) -> ExecutionState {
        self.execution
    }
    pub fn effects(&self) -> EffectState {
        self.effects
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn unresolved(&self) -> bool {
        !self.execution.terminal()
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.version == VERSION, "unsupported job record version");
        ensure!(
            hex64(&self.operation_id) && hex64(&self.request_digest),
            "invalid job identity/digest"
        );
        validate_scope(&self.owner_scope)?;
        self.target.validate()?;
        ensure!(
            root_key(&self.target.project_root)? == root_key(&self.canonical_project_root)?,
            "job project root binding is inconsistent"
        );
        ensure!(
            self.sequence > 0 && self.sequence < u64::MAX,
            "invalid or exhausted job sequence"
        );
        ensure!(
            (self.execution == ExecutionState::Prepared) == (self.sequence == 1),
            "invalid prepared job generation"
        );
        if self.effects == EffectState::NotStarted {
            ensure!(self.sequence == 2, "invalid no-dispatch generation");
        }
        let valid = match self.execution {
            ExecutionState::Prepared | ExecutionState::Unknown => {
                self.effects == EffectState::Unknown
            }
            ExecutionState::Running => self.effects == EffectState::Active,
            ExecutionState::CancelRequested => {
                matches!(self.effects, EffectState::Active | EffectState::Unknown)
            }
            ExecutionState::Failed => {
                matches!(self.effects, EffectState::Settled | EffectState::NotStarted)
            }
            ExecutionState::Succeeded | ExecutionState::Partial | ExecutionState::Cancelled => {
                self.effects == EffectState::Settled
            }
        };
        ensure!(valid, "inconsistent job execution/effect states");
        Ok(())
    }
}

/// Root-free display projection. This is never mutation or recovery authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobStatus {
    pub operation_id: String,
    pub execution: ExecutionState,
    pub effects: EffectState,
    pub sequence: u64,
}

/// Non-serializable and constructible only here. Even a second Journal for the same directory cannot
/// use this handle; only Journal clones share the process-local nonce map.
pub struct OwnedJob {
    operation_id: String,
    nonce: [u8; 32],
}

impl OwnedJob {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

impl std::fmt::Debug for OwnedJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedJob")
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

/// Controller-only evidence envelope, NOT a validator of arbitrary adapter/MCP text.
///
/// Before constructing this, the controller must validate a structured receipt from the bound
/// adapter instance, its authenticated provenance and request correlation, and the asserted effect
/// state. `expected_sequence` is the journal generation against which that observation was made.
/// Settled means no remaining application effects, not merely a completed/cancelled RPC response.
/// No production constructor is exposed until authenticated enrollment/transport exists; tests use
/// a module-local fixture constructor. The controller boundary alone would not prove provenance.
#[derive(Clone, Debug)]
pub(super) struct VerifiedReceipt {
    operation_id: String,
    target: TargetBinding,
    request_digest: String,
    owner_scope: String,
    expected_sequence: u64,
    effects: EffectState,
}

impl VerifiedReceipt {
    // No production receipt constructor exists until authenticated enrollment/transport is wired.
    #[cfg(test)]
    pub(super) fn from_verified_adapter(
        operation_id: String,
        target: TargetBinding,
        request_digest: String,
        owner_scope: String,
        expected_sequence: u64,
        effects: EffectState,
    ) -> Result<Self> {
        target.validate()?;
        validate_scope(&owner_scope)?;
        ensure!(
            hex64(&operation_id) && hex64(&request_digest),
            "invalid receipt identity/digest"
        );
        ensure!(expected_sequence > 0, "invalid receipt sequence");
        ensure!(
            matches!(effects, EffectState::Active | EffectState::Settled),
            "receipt must prove active or settled effects"
        );
        Ok(Self {
            operation_id,
            target,
            request_digest,
            owner_scope,
            expected_sequence,
            effects,
        })
    }

    fn matches(&self, record: &JobRecord) -> Result<()> {
        ensure!(
            self.operation_id == record.operation_id
                && self.target == record.target
                && self.target.project_root.as_os_str() == record.target.project_root.as_os_str()
                && self.request_digest == record.request_digest
                && self.owner_scope == record.owner_scope
                && self.expected_sequence == record.sequence,
            "receipt binding or sequence does not match the owned job"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct Journal {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    local: Mutex<Local>,
}

#[derive(Default)]
struct Local {
    storage: Option<Storage>,
    owned: HashMap<String, Authority>,
}

struct Authority {
    nonce: [u8; 32],
    record: JobRecord,
}

#[derive(Clone)]
struct Storage {
    journal: NativeResource,
    lock: NativeResource,
    marker_identity: (u64, u64),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalData {
    version: u32,
    records: Vec<JobRecord>,
}

impl JournalData {
    fn empty() -> Self {
        Self {
            version: VERSION,
            records: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported operations journal version"
        );
        ensure!(
            !self.records.is_empty() && self.records.len() <= MAX_RECORDS,
            "operations journal must contain 1..=128 records"
        );
        let mut ids = HashSet::new();
        let mut digests = HashSet::new();
        for record in &self.records {
            record.validate()?;
            ensure!(
                ids.insert(&record.operation_id),
                "duplicate operation ID in journal"
            );
            ensure!(
                digests.insert(&record.request_digest),
                "duplicate request digest in journal"
            );
        }
        for (index, a) in self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.unresolved())
        {
            for b in self.records[index + 1..].iter().filter(|r| r.unresolved()) {
                ensure!(
                    !overlapping(&a.canonical_project_root, &b.canonical_project_root)?,
                    "conflicting unresolved jobs in journal"
                );
            }
        }
        Ok(())
    }
}

impl Journal {
    /// Explicit storage root; construction is inert and never consults AIZEN_HOME.
    pub fn new(root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                local: Mutex::new(Local::default()),
            }),
        }
    }

    /// The controller must complete lifecycle admission BEFORE begin, and may dispatch only after
    /// this returns successfully. A crash/lost response leaves Prepared/Unknown blocking the root.
    pub(super) fn begin(
        &self,
        batch: &BatchRequest,
        manifest: &CapabilityManifest,
        owner_scope: &str,
    ) -> Result<OwnedJob> {
        batch.validate(manifest)?;
        validate_scope(owner_scope)?;
        let digest = batch.digest()?;
        ensure!(hex64(&digest), "invalid request digest");
        let project_guard = DirectoryGuard::capture(&batch.target.project_root, false)?;
        // Reuse exact spelling validation without opening or creating any project file.
        NativeResource::validate_spelling(&batch.target.project_root)?;
        NativeResource::validate_spelling(&batch.target.project_root.join(JOURNAL))?;
        let canonical_project_root = fs::canonicalize(&batch.target.project_root)
            .context("canonicalizing job project root")?;
        project_guard.verify()?;
        root_key(&canonical_project_root)?;
        let operation_id = random_hex()?;
        let nonce = random_bytes()?;
        let mut local = self
            .inner
            .local
            .lock()
            .map_err(|_| anyhow::anyhow!("job ownership mutex poisoned"))?;
        let (storage, created_marker) = self.prepare_storage(&mut local)?;
        let lock = RepoTxnLock::acquire_existing_exclusive(storage.lock.path(), Duration::ZERO)?;
        storage.verify()?;
        lock.verify_existing_path(storage.lock.path())?;
        let (loaded, fingerprint) = storage.read()?;
        let mut data = match (loaded, created_marker) {
            (None, true) => JournalData::empty(),
            (Some(data), false) => data,
            (None, false) => {
                bail!("operations marker exists but journal is absent; recovery is required")
            }
            (Some(_), true) => {
                bail!("journal appeared during initialization; refusing to overwrite")
            }
        };
        ensure!(
            data.records.len() < MAX_RECORDS,
            "operations journal is full; no records were evicted"
        );
        for old in &data.records {
            ensure!(
                old.request_digest != digest,
                "request was already recorded; automatic replay is forbidden"
            );
            ensure!(old.operation_id != operation_id, "operation ID collision");
            if old.unresolved() {
                ensure!(
                    !overlapping(&old.canonical_project_root, &canonical_project_root)?,
                    "an unresolved job already covers this project root"
                );
            }
        }
        let record = JobRecord {
            version: VERSION,
            operation_id: operation_id.clone(),
            request_digest: digest,
            target: batch.target.clone(),
            canonical_project_root,
            owner_scope: owner_scope.to_owned(),
            execution: ExecutionState::Prepared,
            effects: EffectState::Unknown,
            sequence: 1,
        };
        data.records.push(record.clone());
        project_guard.verify()?;
        storage.write(&lock, &fingerprint, &data)?;
        local
            .owned
            .insert(operation_id.clone(), Authority { nonce, record });
        Ok(OwnedJob {
            operation_id,
            nonce,
        })
    }

    /// Ordinary acceptance cannot resurrect Unknown, cancellation, or terminal records.
    pub(super) fn mark_running(&self, job: &OwnedJob, receipt: &VerifiedReceipt) -> Result<()> {
        self.update(job, |record| {
            receipt.matches(record)?;
            ensure!(
                record.execution == ExecutionState::Prepared
                    && receipt.effects == EffectState::Active,
                "only a prepared job may accept an active receipt"
            );
            record.execution = ExecutionState::Running;
            record.effects = EffectState::Active;
            Ok(())
        })
    }

    /// Separate reconciliation entry point: the caller must requery the SAME bound adapter instance
    /// and prove this operation is active. It does not create restart ownership or permit replay.
    pub(super) fn reconcile_running(
        &self,
        job: &OwnedJob,
        receipt: &VerifiedReceipt,
    ) -> Result<()> {
        self.update(job, |record| {
            receipt.matches(record)?;
            ensure!(
                record.execution == ExecutionState::Unknown
                    && receipt.effects == EffectState::Active,
                "reconciliation requires unknown execution and active evidence"
            );
            record.execution = ExecutionState::Running;
            record.effects = EffectState::Active;
            Ok(())
        })
    }

    /// An intent only. The caller still has to send cancellation and independently prove settlement.
    pub(super) fn request_cancel(&self, job: &OwnedJob) -> Result<()> {
        self.update(job, |record| {
            ensure!(
                !record.execution.terminal() && record.execution != ExecutionState::CancelRequested,
                "job cannot enter cancellation requested"
            );
            record.execution = ExecutionState::CancelRequested;
            Ok(())
        })
    }

    pub(super) fn mark_unknown(&self, job: &OwnedJob) -> Result<()> {
        self.update(job, |record| {
            ensure!(
                !record.execution.terminal(),
                "terminal job cannot become unknown"
            );
            ensure!(
                record.execution != ExecutionState::Unknown
                    && !(record.execution == ExecutionState::CancelRequested
                        && record.effects == EffectState::Unknown),
                "job is already unknown"
            );
            // Do not lose a pending cancellation intent when its response is lost.
            if record.execution != ExecutionState::CancelRequested {
                record.execution = ExecutionState::Unknown;
            }
            record.effects = EffectState::Unknown;
            Ok(())
        })
    }

    pub(super) fn settle(
        &self,
        job: &OwnedJob,
        receipt: &VerifiedReceipt,
        terminal: TerminalState,
    ) -> Result<()> {
        self.update(job, |record| {
            receipt.matches(record)?;
            ensure!(
                !record.execution.terminal(),
                "terminal transition is irreversible"
            );
            ensure!(
                receipt.effects == EffectState::Settled,
                "terminal receipt must prove effect settlement"
            );
            record.execution = terminal.into();
            record.effects = EffectState::Settled;
            Ok(())
        })
    }

    /// Controller proof obligation: dispatch was DEFINITELY never attempted. An unaccepted/lost
    /// response is insufficient. This is unavailable after acceptance, uncertainty, or cancellation.
    pub(super) fn definite_not_dispatched(&self, job: &OwnedJob) -> Result<()> {
        self.update(job, |record| {
            ensure!(
                record.execution == ExecutionState::Prepared,
                "cannot assert no dispatch after a state transition"
            );
            record.execution = ExecutionState::Failed;
            record.effects = EffectState::NotStarted;
            Ok(())
        })
    }

    /// Atomic read snapshot only: no mkdir, lock acquisition, file creation, ACL change, or adapter
    /// call. A racing first initialization can return an error; it must never look safely empty.
    pub fn snapshot(&self) -> Result<Vec<JobRecord>> {
        let mut local = self
            .inner
            .local
            .lock()
            .map_err(|_| anyhow::anyhow!("job ownership mutex poisoned"))?;
        let Some(storage) = self.inspect_storage(&mut local)? else {
            return Ok(Vec::new());
        };
        let (data, _) = storage.read()?;
        Ok(data
            .context("operations marker exists but journal is absent; recovery is required")?
            .records)
    }

    pub fn status(&self) -> Result<Vec<JobStatus>> {
        Ok(self
            .snapshot()?
            .into_iter()
            .map(|r| JobStatus {
                operation_id: r.operation_id,
                execution: r.execution,
                effects: r.effects,
                sequence: r.sequence,
            })
            .collect())
    }

    pub fn unresolved_count(&self) -> Result<usize> {
        Ok(self.snapshot()?.iter().filter(|r| r.unresolved()).count())
    }

    fn update(
        &self,
        job: &OwnedJob,
        change: impl FnOnce(&mut JobRecord) -> Result<()>,
    ) -> Result<()> {
        let mut local = self
            .inner
            .local
            .lock()
            .map_err(|_| anyhow::anyhow!("job ownership mutex poisoned"))?;
        let owned = local
            .owned
            .get(&job.operation_id)
            .context("job is not owned by this Journal instance; restart adoption is forbidden")?;
        ensure!(owned.nonce == job.nonce, "job handle nonce does not match");
        let expected_record = owned.record.clone();
        let storage = local
            .storage
            .clone()
            .context("owned job has no journal binding")?;
        storage.verify()?;
        let lock = RepoTxnLock::acquire_existing_exclusive(storage.lock.path(), Duration::ZERO)?;
        let (loaded, fingerprint) = storage.read()?;
        let mut data = loaded.context("owned job journal is absent; refusing recreation")?;
        let record = data
            .records
            .iter_mut()
            .find(|r| r.operation_id == job.operation_id)
            .context("owned job is absent from journal")?;
        ensure!(
            *record == expected_record,
            "owned job generation changed; refusing stale update"
        );
        ensure!(
            !record.execution.terminal(),
            "terminal transition is irreversible"
        );
        change(record)?;
        record.sequence = record
            .sequence
            .checked_add(1)
            .context("job sequence exhausted")?;
        record.validate()?;
        let next = record.clone();
        storage.write(&lock, &fingerprint, &data)?;
        local
            .owned
            .get_mut(&job.operation_id)
            .context("job ownership disappeared")?
            .record = next;
        Ok(())
    }

    fn inspect_storage(&self, local: &mut Local) -> Result<Option<Storage>> {
        validate_storage_root(&self.inner.root)?;
        if let Some(storage) = &local.storage {
            storage.verify()?;
            return Ok(Some(storage.clone()));
        }
        let guard = DirectoryGuard::capture(&self.inner.root, true)?;
        let journal_present = regular_exists(&self.inner.root.join(JOURNAL))?;
        let marker_present = regular_exists(&self.inner.root.join(LOCK))?;
        guard.verify()?;
        ensure!(
            journal_present == marker_present,
            "operations journal/marker pair is incomplete; recovery is required"
        );
        if !marker_present {
            return Ok(None);
        }
        let storage = Storage::resolve(&self.inner.root)?;
        local.storage = Some(storage.clone());
        Ok(Some(storage))
    }

    fn prepare_storage(&self, local: &mut Local) -> Result<(Storage, bool)> {
        if let Some(storage) = self.inspect_storage(local)? {
            return Ok((storage, false));
        }
        // Make a new marker only for a pristine journal. Any crash after marker creation is fenced
        // on restart, including a crash before the initial Prepared record becomes durable.
        create_storage_root(&self.inner.root)?;
        let journal = NativeResource::resolve(&self.inner.root.join(JOURNAL))?;
        let marker = NativeResource::resolve(&self.inner.root.join(LOCK))?;
        ensure!(
            !regular_exists(journal.path())? && !regular_exists(marker.path())?,
            "operations state appeared during initialization; retry inspection"
        );
        journal.verify()?;
        marker.verify()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(marker.path())
            .context("creating operations initialization marker")?;
        file.sync_all()?;
        drop(file);
        persist::harden_owner_only_checked(marker.path())?;
        let storage = Storage::resolve(&self.inner.root)?;
        local.storage = Some(storage.clone());
        Ok((storage, true))
    }
}

impl Storage {
    fn resolve(root: &Path) -> Result<Self> {
        let journal = NativeResource::resolve(&root.join(JOURNAL))?;
        let lock = NativeResource::resolve(&root.join(LOCK))?;
        let marker_identity =
            NodeStamp::capture(lock.path(), &fs::symlink_metadata(lock.path())?, false)?.identity();
        let storage = Self {
            journal,
            lock,
            marker_identity,
        };
        storage.verify()?;
        Ok(storage)
    }

    fn verify(&self) -> Result<()> {
        self.journal.verify()?;
        self.lock.verify()?;
        let stamp = NodeStamp::capture(
            self.lock.path(),
            &fs::symlink_metadata(self.lock.path()).context("operations marker disappeared")?,
            false,
        )?;
        ensure!(
            stamp.identity() == self.marker_identity,
            "operations marker identity changed"
        );
        Ok(())
    }

    fn read(&self) -> Result<(Option<JournalData>, FileFingerprint)> {
        self.verify()?;
        let Some(bytes) = read_bounded(&self.journal)? else {
            return Ok((None, FileFingerprint::missing()));
        };
        let fingerprint = FileFingerprint::for_bytes(&bytes);
        let data: JournalData =
            serde_json::from_slice(&bytes).context("invalid operations journal")?;
        data.validate()?;
        self.verify()?;
        Ok((Some(data), fingerprint))
    }

    fn write(
        &self,
        lock: &RepoTxnLock,
        expected: &FileFingerprint,
        data: &JournalData,
    ) -> Result<()> {
        data.validate()?;
        let bytes = serde_json::to_vec(data)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "operations journal byte bound exceeded"
        );
        self.verify()?;
        lock.verify_existing_path(self.lock.path())?;
        // Do not call persist::fingerprint: it performs an unbounded read. This final comparison is
        // intentionally cooperative CAS; a hostile writer can still race the subsequent rename.
        let actual = read_bounded(&self.journal)?
            .map(|b| FileFingerprint::for_bytes(&b))
            .unwrap_or_else(FileFingerprint::missing);
        ensure!(
            &actual == expected,
            "operations journal changed before commit"
        );
        self.verify()?;
        lock.verify_existing_path(self.lock.path())?;
        persist::atomic_write_owner_only(self.journal.path(), &bytes)?;
        self.verify()?;
        lock.verify_existing_path(self.lock.path())?;
        Ok(())
    }
}

fn read_bounded(resource: &NativeResource) -> Result<Option<Vec<u8>>> {
    resource.verify()?;
    let before = match fs::symlink_metadata(resource.path()) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("inspecting operations journal"),
    };
    let stamp = NodeStamp::capture(resource.path(), &before, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(resource.path())
        .context("opening operations journal snapshot")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !is_link_or_reparse(&metadata),
        "journal snapshot is not a regular file"
    );
    ensure!(
        descriptor_identity(&file)? == stamp.identity(),
        "journal changed while opening snapshot; retry"
    );
    ensure!(
        metadata.len() <= MAX_BYTES as u64,
        "operations journal byte bound exceeded"
    );
    let mut bytes = Vec::new();
    (&file).take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "operations journal byte bound exceeded"
    );
    let after = file.metadata()?;
    ensure!(
        metadata.len() == after.len() && metadata.modified().ok() == after.modified().ok(),
        "journal was modified in place during snapshot"
    );
    resource.verify()?;
    Ok(Some(bytes))
}

#[cfg(unix)]
fn descriptor_identity(file: &File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = file.metadata()?;
    Ok((md.dev(), md.ino()))
}

#[cfg(windows)]
fn descriptor_identity(file: &File) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("identifying journal descriptor");
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// NodeStamp is reused for non-link checks and directory identity. Directory length/mtime naturally
/// change during creation/writes, so only identity and creation time are pinned here.
struct DirectoryGuard(Vec<(PathBuf, (u64, u64), Option<std::time::SystemTime>)>);

impl DirectoryGuard {
    fn capture(path: &Path, allow_missing: bool) -> Result<Self> {
        ensure!(path.is_absolute(), "job directory must be absolute");
        let mut entries = Vec::new();
        for (depth, ancestor) in path.ancestors().enumerate() {
            ensure!(depth < 128, "job directory ancestry exceeds bound");
            match fs::symlink_metadata(ancestor) {
                Ok(md) => {
                    ensure!(
                        md.is_dir() && !is_link_or_reparse(&md),
                        "job directory ancestor must be a non-link directory"
                    );
                    let stamp = NodeStamp::capture(ancestor, &md, false)?;
                    entries.push((ancestor.to_path_buf(), stamp.identity(), md.created().ok()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && allow_missing => {}
                Err(e) => return Err(e).context("inspecting job directory ancestors"),
            }
        }
        Ok(Self(entries))
    }

    fn verify(&self) -> Result<()> {
        for (path, identity, created) in &self.0 {
            let md = fs::symlink_metadata(path)?;
            ensure!(
                md.is_dir() && !is_link_or_reparse(&md),
                "job directory ancestor changed"
            );
            let stamp = NodeStamp::capture(path, &md, false)?;
            ensure!(
                stamp.identity() == *identity && md.created().ok() == *created,
                "job directory ancestor identity changed"
            );
        }
        Ok(())
    }
}

fn validate_storage_root(root: &Path) -> Result<()> {
    ensure!(
        root.is_absolute(),
        "operations storage root must be absolute"
    );
    NativeResource::validate_spelling(&root.join(JOURNAL))
}

fn create_storage_root(root: &Path) -> Result<()> {
    validate_storage_root(root)?;
    let mut guard = DirectoryGuard::capture(root, true)?;
    let mut missing = Vec::new();
    for path in root.ancestors() {
        match fs::symlink_metadata(path) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(path.to_path_buf()),
            Err(e) => return Err(e).context("inspecting operations storage root"),
        }
    }
    for path in missing.into_iter().rev() {
        guard.verify()?;
        fs::create_dir(&path).context("creating operations storage directory")?;
        let md = fs::symlink_metadata(&path)?;
        let stamp = NodeStamp::capture(&path, &md, false)?;
        ensure!(md.is_dir(), "operations storage directory changed");
        guard.0.push((path, stamp.identity(), md.created().ok()));
    }
    guard.verify()
}

fn regular_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(md) => {
            ensure!(
                md.is_file() && !is_link_or_reparse(&md),
                "operations path must be a regular non-link file"
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("inspecting operations state"),
    }
}

fn root_key(path: &Path) -> Result<String> {
    ensure!(path.is_absolute(), "job project root must be absolute");
    let text = path.to_str().context("job project root must be Unicode")?;
    ensure!(
        !text.is_empty() && text.len() <= 4096,
        "job project root exceeds bound"
    );
    ensure!(
        !path.components().any(|c| matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )),
        "job project root contains traversal"
    );
    #[cfg(any(windows, target_os = "macos"))]
    {
        // Unknown Unicode case equivalences must not admit two jobs on one physical root.
        ensure!(
            text.is_ascii(),
            "ambiguous non-ASCII job project root on a case-folding host"
        );
    }
    #[cfg(windows)]
    let key = crate::core::config::normalize_path_key(path).to_ascii_lowercase();
    #[cfg(target_os = "macos")]
    let key = text.trim_end_matches('/').to_ascii_lowercase();
    #[cfg(not(any(windows, target_os = "macos")))]
    let key = text.trim_end_matches('/').to_owned();
    Ok(if key.is_empty() { "/".to_owned() } else { key })
}

fn overlapping(a: &Path, b: &Path) -> Result<bool> {
    let a = root_key(a)?;
    let b = root_key(b)?;
    let contains = |parent: &str, child: &str| {
        parent == child
            || (child.starts_with(parent)
                && (parent.ends_with('/') || child.as_bytes().get(parent.len()) == Some(&b'/')))
    };
    Ok(contains(&a, &b) || contains(&b, &a))
}

fn validate_scope(scope: &str) -> Result<()> {
    ensure!(
        !scope.is_empty()
            && scope.len() <= MAX_SCOPE
            && scope
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "owner scope must be a bounded diagnostic safe ID"
    );
    Ok(())
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn random_bytes() -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("system RNG unavailable: {e}"))?;
    Ok(bytes)
}

fn random_hex() -> Result<String> {
    Ok(random_bytes()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::contract::{AppKind, AppOperation, OperationKind};
    use super::*;

    struct Fixture {
        base: PathBuf,
        journal: Journal,
        batch: BatchRequest,
        manifest: CapabilityManifest,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!(
                "aizen-app-jobs-{}-{}",
                std::process::id(),
                random_hex().unwrap()
            ));
            fs::create_dir_all(base.join("project")).unwrap();
            let base = fs::canonicalize(base).unwrap();
            let target = TargetBinding {
                adapter_id: "fixture".into(),
                adapter_version: "1.0".into(),
                app: AppKind::Blender,
                app_version: "4.2".into(),
                instance_epoch: "epoch-1".into(),
                project_root: fs::canonicalize(base.join("project")).unwrap(),
                document_id: "Scene".into(),
                revision: "rev-1".into(),
                schema_digest: "a".repeat(64),
            };
            let manifest = CapabilityManifest {
                protocol_version: 1,
                target: target.clone(),
                operations: vec![OperationKind::RunScenario],
                supports_cancel: true,
            };
            let batch = BatchRequest {
                protocol_version: 1,
                target,
                operations: vec![AppOperation::RunScenario {
                    scenario_id: "smoke".into(),
                    seed: 1,
                }],
            };
            let journal = Journal::new(base.join("state"));
            Self {
                base,
                journal,
                batch,
                manifest,
            }
        }
        fn begin(&self) -> OwnedJob {
            self.journal
                .begin(&self.batch, &self.manifest, "test_scope")
                .unwrap()
        }
        fn receipt(&self, effects: EffectState) -> VerifiedReceipt {
            let record = self.journal.snapshot().unwrap().pop().unwrap();
            VerifiedReceipt::from_verified_adapter(
                record.operation_id,
                record.target,
                record.request_digest,
                record.owner_scope,
                record.sequence,
                effects,
            )
            .unwrap()
        }
        fn other_request(&self, name: &str, seed: u64) -> (BatchRequest, CapabilityManifest) {
            let mut request = self.batch.clone();
            let root = self.base.join(name);
            fs::create_dir_all(&root).unwrap();
            request.target.project_root = fs::canonicalize(root).unwrap();
            // Vary request identity independently from the project name.
            request.target.revision = format!("revision-{seed}");
            let mut manifest = self.manifest.clone();
            manifest.target = request.target.clone();
            (request, manifest)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn prepared_is_conservative_and_lost_response_blocks_replay_after_restart() {
        let f = Fixture::new();
        let job = f.begin();
        let first = f.journal.snapshot().unwrap().pop().unwrap();
        assert_eq!(
            (first.execution(), first.effects(), first.sequence()),
            (ExecutionState::Prepared, EffectState::Unknown, 1)
        );
        f.journal.mark_unknown(&job).unwrap();
        let restart = Journal::new(f.base.join("state"));
        assert_eq!(restart.unresolved_count().unwrap(), 1);
        assert!(restart.begin(&f.batch, &f.manifest, "test_scope").is_err());
        assert!(restart.mark_unknown(&job).is_err());
        let projection = restart.status().unwrap();
        assert_eq!(projection[0].operation_id, job.operation_id());
        assert_eq!(projection[0].execution, ExecutionState::Unknown);
        assert!(!format!("{projection:?}").contains("project_root"));
    }

    #[test]
    fn handle_nonce_journal_and_persisted_snapshot_are_not_authority() {
        let f = Fixture::new();
        let job = f.begin();
        let record = f.journal.snapshot().unwrap().pop().unwrap();
        let forged = OwnedJob {
            operation_id: record.operation_id.clone(),
            nonce: [0; 32],
        };
        assert!(f.journal.mark_unknown(&forged).is_err());
        let independent = Journal::new(f.base.join("state"));
        assert!(independent.request_cancel(&job).is_err());
        let elsewhere = Journal::new(f.base.join("elsewhere"));
        assert!(elsewhere.request_cancel(&job).is_err());
        assert!(!f.base.join("elsewhere").exists());
        f.journal.clone().mark_unknown(&job).unwrap();
    }

    #[test]
    fn unresolved_same_nested_and_parent_roots_conflict_but_siblings_do_not() {
        let f = Fixture::new();
        let _job = f.begin();
        for name in ["project", "project/nested", ""] {
            let (request, manifest) = f.other_request(name, 2);
            assert!(f.journal.begin(&request, &manifest, "other_scope").is_err());
        }
        let (request, manifest) = f.other_request("project-sibling", 3);
        f.journal.begin(&request, &manifest, "other_scope").unwrap();
        assert_eq!(f.journal.unresolved_count().unwrap(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_root_keys_case_fold_and_ignore_verbatim_prefix() {
        let f = Fixture::new();
        let canonical = f.batch.target.project_root.clone();
        let upper = PathBuf::from(canonical.to_str().unwrap().to_ascii_uppercase());
        assert!(overlapping(&canonical, &upper).unwrap());
        let plain = PathBuf::from(
            canonical
                .to_str()
                .unwrap()
                .strip_prefix(r"\\?\")
                .unwrap_or(canonical.to_str().unwrap()),
        );
        assert!(overlapping(&canonical, &plain).unwrap());
    }

    #[test]
    fn cancel_is_not_settlement_and_partial_terminal_is_irreversible() {
        let f = Fixture::new();
        let job = f.begin();
        f.journal
            .mark_running(&job, &f.receipt(EffectState::Active))
            .unwrap();
        f.journal.request_cancel(&job).unwrap();
        let record = f.journal.snapshot().unwrap().pop().unwrap();
        assert_eq!(
            (record.execution(), record.effects()),
            (ExecutionState::CancelRequested, EffectState::Active)
        );
        assert_eq!(f.journal.unresolved_count().unwrap(), 1);
        f.journal.mark_unknown(&job).unwrap();
        assert_eq!(
            f.journal.snapshot().unwrap()[0].execution(),
            ExecutionState::CancelRequested
        );
        assert!(f
            .journal
            .settle(
                &job,
                &f.receipt(EffectState::Active),
                TerminalState::Partial
            )
            .is_err());
        let settled = f.receipt(EffectState::Settled);
        f.journal
            .settle(&job, &settled, TerminalState::Partial)
            .unwrap();
        let terminal = f.journal.snapshot().unwrap().pop().unwrap();
        assert_eq!(
            (terminal.execution(), terminal.effects()),
            (ExecutionState::Partial, EffectState::Settled)
        );
        assert_eq!(f.journal.unresolved_count().unwrap(), 0);
        assert!(f
            .journal
            .settle(&job, &settled, TerminalState::Succeeded)
            .is_err());
        assert!(f.journal.request_cancel(&job).is_err());
        assert!(f.journal.mark_unknown(&job).is_err());
        assert!(f
            .journal
            .begin(&f.batch, &f.manifest, "test_scope")
            .is_err());
    }

    #[test]
    fn wrong_receipt_scope_digest_epoch_target_id_and_sequence_are_refused() {
        let f = Fixture::new();
        let job = f.begin();
        let receipt = f.receipt(EffectState::Active);
        for index in 0..6 {
            let mut wrong = receipt.clone();
            match index {
                0 => wrong.owner_scope = "other_scope".into(),
                1 => wrong.request_digest = "b".repeat(64),
                2 => wrong.target.instance_epoch = "epoch-2".into(),
                3 => wrong.target.revision = "rev-2".into(),
                4 => wrong.operation_id = "c".repeat(64),
                _ => wrong.expected_sequence += 1,
            }
            assert!(f.journal.mark_running(&job, &wrong).is_err());
        }
        assert_eq!(f.journal.snapshot().unwrap()[0].sequence(), 1);
        f.journal.mark_running(&job, &receipt).unwrap();
        assert!(f.journal.mark_running(&job, &receipt).is_err());
        let mut stale_terminal = receipt;
        stale_terminal.effects = EffectState::Settled;
        assert!(f
            .journal
            .settle(&job, &stale_terminal, TerminalState::Succeeded)
            .is_err());
    }

    #[test]
    fn unknown_only_returns_to_running_through_explicit_reconciliation() {
        let f = Fixture::new();
        let job = f.begin();
        f.journal.mark_unknown(&job).unwrap();
        let receipt = f.receipt(EffectState::Active);
        assert!(f.journal.mark_running(&job, &receipt).is_err());
        assert!(f.journal.definite_not_dispatched(&job).is_err());
        f.journal.reconcile_running(&job, &receipt).unwrap();
        f.journal
            .settle(
                &job,
                &f.receipt(EffectState::Settled),
                TerminalState::Succeeded,
            )
            .unwrap();
    }

    #[test]
    fn definite_no_dispatch_is_prepared_only_and_never_an_active_cancel_shortcut() {
        let f = Fixture::new();
        let job = f.begin();
        f.journal.definite_not_dispatched(&job).unwrap();
        let record = f.journal.snapshot().unwrap().pop().unwrap();
        assert_eq!(
            (record.execution(), record.effects()),
            (ExecutionState::Failed, EffectState::NotStarted)
        );
        let (request, manifest) = f.other_request("other", 4);
        let active = f.journal.begin(&request, &manifest, "other").unwrap();
        f.journal
            .mark_running(&active, &f.receipt(EffectState::Active))
            .unwrap();
        assert!(f.journal.definite_not_dispatched(&active).is_err());
        f.journal.request_cancel(&active).unwrap();
        f.journal
            .settle(
                &active,
                &f.receipt(EffectState::Settled),
                TerminalState::Cancelled,
            )
            .unwrap();
    }

    #[test]
    fn offline_status_is_inert_even_when_parent_does_not_exist() {
        let f = Fixture::new();
        let root = f.base.join("absent/deep/state");
        let journal = Journal::new(root);
        assert!(journal.snapshot().unwrap().is_empty());
        assert!(journal.status().unwrap().is_empty());
        assert_eq!(journal.unresolved_count().unwrap(), 0);
        assert!(!f.base.join("absent").exists());
        let _job = f.begin();
        let path = f.base.join("state").join(JOURNAL);
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        f.journal.status().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn marker_without_journal_and_journal_without_marker_never_look_empty() {
        let f = Fixture::new();
        fs::create_dir(f.base.join("state")).unwrap();
        fs::write(f.base.join("state").join(LOCK), b"").unwrap();
        assert!(f.journal.snapshot().is_err());
        assert!(f.journal.begin(&f.batch, &f.manifest, "scope").is_err());
        assert!(!f.base.join("state").join(JOURNAL).exists());
        fs::remove_file(f.base.join("state").join(LOCK)).unwrap();
        fs::write(
            f.base.join("state").join(JOURNAL),
            br#"{"version":1,"records":[]}"#,
        )
        .unwrap();
        assert!(f.journal.snapshot().is_err());
        assert!(!f.base.join("state").join(LOCK).exists());
    }

    #[test]
    fn corrupt_unknown_version_unknown_fields_and_oversized_journals_fail_closed() {
        let f = Fixture::new();
        let _job = f.begin();
        let path = f.base.join("state").join(JOURNAL);
        let valid = fs::read(&path).unwrap();
        for bytes in [
            b"not-json".to_vec(),
            br#"{"version":2,"records":[]}"#.to_vec(),
            br#"{"version":1,"records":[],"extra":1}"#.to_vec(),
            vec![b' '; MAX_BYTES + 1],
        ] {
            persist::atomic_write_owner_only(&path, &bytes).unwrap();
            assert!(f.journal.snapshot().is_err());
            assert!(f.journal.begin(&f.batch, &f.manifest, "scope").is_err());
        }
        let mut unknown: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        unknown["records"][0]["extra"] = serde_json::json!(true);
        persist::atomic_write_owner_only(&path, &serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(f.journal.snapshot().is_err());
        persist::atomic_write_owner_only(&path, &valid).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(f.journal.snapshot().is_err());
        assert!(f.journal.begin(&f.batch, &f.manifest, "scope").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn capacity_never_evicts_unresolved_records_and_disk_generations_are_checked() {
        let f = Fixture::new();
        let job = f.begin();
        let path = f.base.join("state").join(JOURNAL);
        let mut data: JournalData = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let template = data.records[0].clone();
        for index in 1..MAX_RECORDS {
            let mut record = template.clone();
            record.operation_id = format!("{index:064x}");
            record.request_digest = format!("{:064x}", index + MAX_RECORDS);
            record.canonical_project_root = f.base.join(format!("reserved-{index}"));
            record.target.project_root = record.canonical_project_root.clone();
            data.records.push(record);
        }
        persist::atomic_write_owner_only(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
        assert_eq!(f.journal.unresolved_count().unwrap(), MAX_RECORDS);
        let (request, manifest) = f.other_request("next", 999);
        assert!(f.journal.begin(&request, &manifest, "scope").is_err());
        assert_eq!(f.journal.snapshot().unwrap().len(), MAX_RECORDS);
        data.records[0].sequence += 1;
        persist::atomic_write_owner_only(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(f.journal.mark_unknown(&job).is_err());
        data.records.push(template);
        persist::atomic_write_owner_only(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(f.journal.snapshot().is_err());
    }

    #[test]
    fn lock_contention_is_bounded_and_status_does_not_acquire_lock() {
        let f = Fixture::new();
        let job = f.begin();
        let lock = RepoTxnLock::acquire_existing_exclusive(
            &f.base.join("state").join(LOCK),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(f.journal.unresolved_count().unwrap(), 1);
        assert!(f.journal.request_cancel(&job).is_err());
        drop(lock);
        f.journal.request_cancel(&job).unwrap();
    }

    #[test]
    fn root_ancestor_replacement_is_detected_before_update() {
        let f = Fixture::new();
        let job = f.begin();
        fs::rename(f.base.join("state"), f.base.join("old-state")).unwrap();
        fs::create_dir(f.base.join("state")).unwrap();
        assert!(f.journal.request_cancel(&job).is_err());
        assert!(f.journal.snapshot().is_err());
        assert!(!f.base.join("state").join(LOCK).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_files_and_ancestors_are_refused_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        fs::create_dir(f.base.join("external")).unwrap();
        symlink(f.base.join("external"), f.base.join("state")).unwrap();
        assert!(f.journal.snapshot().is_err());
        assert!(f.journal.begin(&f.batch, &f.manifest, "scope").is_err());
        assert!(!f.base.join("external").join(LOCK).exists());
        fs::remove_file(f.base.join("state")).unwrap();
        let _job = f.begin();
        let journal_path = f.base.join("state").join(JOURNAL);
        fs::remove_file(&journal_path).unwrap();
        let outside = f.base.join("outside.json");
        fs::write(&outside, b"untouched").unwrap();
        symlink(&outside, &journal_path).unwrap();
        assert!(f.journal.snapshot().is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    }

    #[test]
    fn invalid_scope_never_initializes_storage_and_no_operations_are_serialized() {
        let f = Fixture::new();
        for scope in ["", "a/b", "credential=value", "a\nb"] {
            assert!(f.journal.begin(&f.batch, &f.manifest, scope).is_err());
        }
        assert!(!f.base.join("state").exists());
        let _job = f.begin();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(f.base.join("state").join(JOURNAL)).unwrap()).unwrap();
        assert!(value["records"][0].get("operations").is_none());
        assert!(value["records"][0].get("nonce").is_none());
        assert!(!serde_json::to_string(&value)
            .unwrap()
            .contains("scenario_id"));
    }
}
