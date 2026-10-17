//! Bounded, private native mutation journal. Callers hold the global/workspace gate and an
//! exclusive NativeResourceLease throughout apply, reconciliation, and explicitly requested undo.
//! This module only acquires the later storage lock; it never acquires resource locks or awaits.
//! Advisory leases and bounded check/replacement are cooperative CAS, not kernel CAS against editors.
//! No record is pruned: capacity exhaustion fails before a workspace mutation, preserving undo.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::exec_ctx::{self, ExecutionContext};
use crate::core::persist::{self, FileFingerprint};
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::NativeResource;

pub(crate) const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 128;
const MAX_META_BYTES: usize = 32 * 1024;
const MAX_BINDING_BYTES: usize = 4096;
const MAX_OWNER_BYTES: usize = 1024;
const MAGIC: &[u8; 8] = b"AZNJ\0\0\0\x01";
const MAGIC_V2: &[u8; 8] = b"AZNJ\0\0\0\x02";
const MAX_FENCES: usize = 128;
const MAX_FENCE_META_BYTES: usize = MAX_RECORDS * MAX_META_BYTES;

/// Durable history identity only; possession never conveys source mutation or retry authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchIdentity {
    pub(crate) epoch: [u8; 32],
    pub(crate) batch: [u8; 32],
}

impl BatchIdentity {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.epoch != [0; 32] && self.batch != [0; 32],
            "native journal invalid batch identity");
        Ok(())
    }
}

fn random_identity() -> Result<[u8; 32]> {
    fault(Fault::Random)?;
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| anyhow::anyhow!("native journal random identity unavailable"))?;
    anyhow::ensure!(bytes != [0; 32], "native journal random identity invalid");
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum State {
    Prepared,
    Applied,
    Settled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Outcome {
    Applied,
    NotApplied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    exists: bool,
    byte_len: u64,
    sha256: [u8; 32],
}

impl From<&FileFingerprint> for Fingerprint {
    fn from(value: &FileFingerprint) -> Self {
        Self {
            exists: value.exists,
            byte_len: value.byte_len,
            sha256: value.sha256,
        }
    }
}

impl Fingerprint {
    fn file(&self) -> FileFingerprint {
        FileFingerprint {
            exists: self.exists,
            byte_len: self.byte_len,
            sha256: self.sha256,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    path: PathBuf,
    key: String,
    /// Restart must not bless an identically spelled directory swapped since Prepared was flushed.
    parent_identity: (u64, u64),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Operation {
    Write,
    /// Undo is another ordered durable mutation, attributed to the requesting parent, not the child.
    Undo {
        target: u64,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    id: u64,
    owner: String,
    binding: Binding,
    operation: Operation,
    state: State,
    outcome: Option<Outcome>,
    before: Fingerprint,
    after: Fingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coverage: Option<IntentTag>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentTag {
    batch: [u8; 32],
    ordinal: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedIntent {
    id: u64,
    binding: Binding,
    before: Fingerprint,
    after: Fingerprint,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchFence {
    version: u32,
    batch: [u8; 32],
    owner: String,
    #[serde(deserialize_with = "deserialize_intents")]
    intents: Vec<ExpectedIntent>,
}

fn deserialize_intents<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<ExpectedIntent>, D::Error> {
    struct Intents;
    impl<'de> serde::de::Visitor<'de> for Intents {
        type Value = Vec<ExpectedIntent>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded native intent manifest")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
            let mut intents = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_RECORDS));
            while let Some(intent) = seq.next_element()? {
                if intents.len() == MAX_RECORDS {
                    return Err(serde::de::Error::custom("native journal intent count exceeds limit"));
                }
                intents.push(intent);
            }
            Ok(intents)
        }
    }
    deserializer.deserialize_seq(Intents)
}

struct Record {
    meta: Metadata,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

#[derive(Default)]
struct Journal {
    // None is a legacy v1 journal. Existing records are never retroactively covered.
    epoch: Option<[u8; 32]>,
    fences: Vec<BatchFence>,
    records: Vec<Record>,
}

/// Inspectable without exposing source bytes, paths, owners, or the underlying secret-bearing error.
/// Includes uncertain journal publication before mutation. The assigned intent ID may not have
/// reached disk if its first save failed; a missing exact record is not proof of no effect.
#[derive(Debug)]
pub(crate) struct PossiblyApplied {
    pub(crate) record_id: u64,
}

impl std::fmt::Display for PossiblyApplied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native mutation {} possibly applied; retain the resource lease and reconcile before retrying", self.record_id)
    }
}

impl std::error::Error for PossiblyApplied {}

/// The complete covered fence may have reached durable storage, but the caller did not receive a
/// usable batch handle. Retain the exact identity for inspection; it never conveys mutation or
/// retry authority. This is deliberately separate from [`PossiblyApplied`]: no source effect was
/// admitted before a covered fence was published.
#[derive(Debug)]
pub(crate) struct FencePublicationUncertain {
    pub(crate) identity: BatchIdentity,
}

impl std::fmt::Display for FencePublicationUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native journal covered fence ")?;
        for byte in self.identity.epoch { write!(f, "{byte:02x}")?; }
        write!(f, "/")?;
        for byte in self.identity.batch { write!(f, "{byte:02x}")?; }
        write!(f, " publication is uncertain; retain this identity for inspection; no source effect or retry authority")
    }
}

impl std::error::Error for FencePublicationUncertain {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reconciliation {
    Before { record_id: u64 },
    After { record_id: u64 },
    Conflict { record_id: u64 },
}

/// Data-only expectations for one exact historical write. These never convey mutation authority.
pub(crate) struct RecordClaim<'a> {
    pub(crate) id: u64,
    pub(crate) expected_owner: &'a str,
    pub(crate) resource: &'a NativeResource,
    pub(crate) before: &'a FileFingerprint,
    pub(crate) after: &'a FileFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordProofStatus {
    Prepared,
    Applied,
    SettledApplied,
    SettledNotApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordProof {
    pub(crate) id: u64,
    pub(crate) status: RecordProofStatus,
}

/// Inspect exact historical writes without creating, repairing, settling, or hardening storage.
/// Missing evidence is an error, not proof of no effect. Even a settled applied record proves only
/// history: it grants no undo/retry authority and does not establish the current source image.
/// Callers acquire any lower-order locks before this late journal lock; no execution context needed.
pub(crate) fn inspect_existing_records(claims: &[RecordClaim<'_>]) -> Result<Vec<RecordProof>> {
    if claims.is_empty() {
        return Ok(Vec::new());
    }
    // Never expose paths, owners, source images, JSON parse details, or underlying storage errors.
    inspect_existing_records_inner(None, claims)
        .map_err(|_| anyhow::anyhow!("native journal proof unavailable or exact claim mismatch"))
}

/// Inspect complete durable native-intent accounting, never current source content or retry rights.
/// Missing expected intents remain unresolved errors, including a batch with no published records.
pub(crate) fn inspect_existing_batch(
    identity: &BatchIdentity,
    claims: &[RecordClaim<'_>],
) -> Result<Vec<RecordProof>> {
    identity.validate()?;
    inspect_existing_records_inner(Some(identity), claims)
        .map_err(|_| anyhow::anyhow!("native journal batch proof unavailable or incomplete exact coverage"))
}

fn inspect_existing_records_inner(
    coverage: Option<&BatchIdentity>, claims: &[RecordClaim<'_>],
) -> Result<Vec<RecordProof>> {
    use crate::core::file_placement::NodeStamp;
    use crate::core::repo_lock::LockMode;
    use std::io::Read;

    anyhow::ensure!(claims.len() <= MAX_RECORDS, "too many native journal claims");
    let mut ids = std::collections::BTreeSet::new();
    for claim in claims {
        anyhow::ensure!(claim.id > 0 && ids.insert(claim.id), "invalid native journal claim ID");
        check_text(claim.expected_owner, MAX_OWNER_BYTES, "owner")?;
        claim.resource.verify()?;
    }
    let dir = storage_directory()?;
    let dir_meta = fs::symlink_metadata(&dir)?;
    anyhow::ensure!(dir_meta.is_dir(), "native journal directory missing");
    let dir_stamp = NodeStamp::capture(&dir, &dir_meta, false)?;
    let path = dir.join("records.bin");
    let lock_path = dir.join("storage.lock");
    // These owned guards pin the complete existing ancestry, including the storage directory.
    let record_guard = NativeResource::resolve(&path)?;
    let lock_guard = NativeResource::resolve(&lock_path)?;
    let lock_stamp = NodeStamp::capture(&lock_path, &fs::symlink_metadata(&lock_path)?, false)?;
    let lock = RepoTxnLock::acquire_existing(&lock_path, LockMode::Shared, Duration::ZERO)?;
    record_guard.verify()?;
    lock_guard.verify()?;
    anyhow::ensure!(NodeStamp::capture(&dir, &fs::symlink_metadata(&dir)?, false)? == dir_stamp,
        "native journal directory changed");
    let record_meta = fs::symlink_metadata(&path)?;
    anyhow::ensure!(record_meta.is_file() && record_meta.len() <= MAX_JOURNAL_BYTES as u64,
        "native journal records missing or oversized");
    let record_stamp = NodeStamp::capture(&path, &record_meta, false)?;
    let mut file = open_existing_records(&path)?;
    let identity = proof_file_identity(&file)?;
    anyhow::ensure!(identity == record_stamp.identity(), "native journal descriptor changed");
    let descriptor_stamp = file.metadata()?;
    anyhow::ensure!(descriptor_stamp.len() <= MAX_JOURNAL_BYTES as u64,
        "native journal records oversized");
    let mut bytes = Vec::new();
    (&mut file).take(MAX_JOURNAL_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= MAX_JOURNAL_BYTES
        && bytes.len() as u64 == descriptor_stamp.len(), "native journal records oversized or changed");
    let journal = Journal::decode(&bytes)?;
    if let Some(identity) = coverage {
        anyhow::ensure!(journal.epoch == Some(identity.epoch), "native journal epoch mismatch");
        let fence = journal.fences.iter().find(|fence| fence.batch == identity.batch)
            .context("native journal coverage fence missing")?;
        anyhow::ensure!(fence.intents.len() == claims.len(), "native journal incomplete coverage claims");
        for (ordinal, (intent, claim)) in fence.intents.iter().zip(claims).enumerate() {
            anyhow::ensure!(intent.id == claim.id && fence.owner == claim.expected_owner,
                "native journal coverage claim order mismatch");
            let record = journal.records.iter().find(|record| record.meta.id == intent.id)
                .context("native journal expected intent unresolved")?;
            anyhow::ensure!(record.meta.coverage == Some(IntentTag { batch: identity.batch, ordinal: ordinal as u32 }),
                "native journal coverage tag mismatch");
        }
    }
    let mut proofs = Vec::with_capacity(claims.len());
    for claim in claims {
        let record = journal.records.iter().find(|record| record.meta.id == claim.id)
            .context("native journal claimed record missing")?;
        anyhow::ensure!(record.meta.owner == claim.expected_owner
            && matches!(record.meta.operation, Operation::Write)
            && matches_resource(record, claim.resource)?
            && record.meta.before.file() == *claim.before
            && record.meta.after.file() == *claim.after,
            "native journal exact claim mismatch");
        let status = match (record.meta.state, record.meta.outcome) {
            (State::Prepared, None) => RecordProofStatus::Prepared,
            (State::Applied, Some(Outcome::Applied)) => RecordProofStatus::Applied,
            (State::Settled, Some(Outcome::Applied)) => RecordProofStatus::SettledApplied,
            (State::Settled, Some(Outcome::NotApplied)) => RecordProofStatus::SettledNotApplied,
            _ => anyhow::bail!("native journal invalid proof status"),
        };
        proofs.push(RecordProof { id: claim.id, status });
        claim.resource.verify()?;
    }
    let after_meta = file.metadata()?;
    anyhow::ensure!(proof_file_identity(&file)? == identity
        && after_meta.len() == descriptor_stamp.len()
        && after_meta.modified().ok() == descriptor_stamp.modified().ok(),
        "native journal descriptor drift");
    anyhow::ensure!(NodeStamp::capture(&path, &fs::symlink_metadata(&path)?, false)? == record_stamp,
        "native journal records changed");
    record_guard.verify()?;
    lock_guard.verify()?;
    lock.verify_existing_path(&lock_path)?;
    anyhow::ensure!(NodeStamp::capture(&lock_path, &fs::symlink_metadata(&lock_path)?, false)? == lock_stamp
        && NodeStamp::capture(&dir, &fs::symlink_metadata(&dir)?, false)? == dir_stamp,
        "native journal storage changed");
    Ok(proofs)
}

// Never follow a substituted link or wait on a substituted FIFO. No create or write access.
fn open_existing_records(path: &Path) -> Result<fs::File> {
    #[cfg(any(windows, target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let mut options = OpenOptions::new();
        options.read(true).create(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(any(target_os = "linux", target_os = "android"))]
            options.custom_flags(0x20000 | 0x800);
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            options.custom_flags(0x100 | 0x4);
        }
        let file = options.open(path)?;
        proof_file_identity(&file)?;
        Ok(file)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    {
        let _ = path;
        anyhow::bail!("existing native journal inspection unavailable")
    }
}

fn proof_file_identity(file: &fs::File) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        anyhow::ensure!(meta.is_file() && meta.nlink() == 1, "unsafe native journal record file");
        Ok((meta.dev(), meta.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        anyhow::ensure!(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "native journal record identity unavailable");
        anyhow::ensure!(file.metadata()?.is_file() && info.nNumberOfLinks == 1
            && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "unsafe native journal record file");
        Ok((info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        anyhow::bail!("native journal record identity unavailable")
    }
}

/// Exact native approved bytes, with Prepared -> replacement -> verified Applied -> Settled.
/// Owner MUST equal the pinned physical execution context's exact resource_scope (not an ancestor).
/// Caller retains its exclusive resource lease even when this returns PossiblyApplied.
pub(crate) fn apply(
    resource: &NativeResource,
    expected: &FileFingerprint,
    bytes: &[u8],
    owner: &str,
) -> Result<FileFingerprint> {
    check_owner(owner)?;
    resource.verify()?;
    validate_mutation_target(resource.path())?;
    anyhow::ensure!(
        bytes.len() <= MAX_SOURCE_BYTES,
        "native journal source exceeds 4 MiB"
    );
    binding(resource)?;
    let storage = Storage::open()?;
    storage.check_resource(resource)?;
    let mut journal = storage.load()?;
    reconcile_locked(resource, &storage, &mut journal)?;
    ensure_resolved(resource, &journal)?;
    let (before, actual) = read_image(resource.path())?;
    anyhow::ensure!(
        &actual == expected,
        "native journal edit conflict; nothing was written"
    );
    let after = FileFingerprint::for_bytes(bytes);
    // A content no-op has no ownership effect and must not turn coincident images into undo rights.
    if actual == after {
        return Ok(actual);
    }
    journal.reserve(before.as_ref().map_or(0, Vec::len), bytes.len())?;
    let record = Record {
        meta: Metadata {
            id: journal.next_id()?,
            owner: owner.to_owned(),
            binding: binding(resource)?,
            operation: Operation::Write,
            coverage: None,
            state: State::Prepared,
            outcome: None,
            before: (&actual).into(),
            after: (&after).into(),
        },
        before,
        after: Some(bytes.to_vec()),
    };
    journal.append(record)?;
    commit_last(resource, &storage, &mut journal)?;
    // Durable settlement, not a child's final text, is the source of parent edit attribution.
    exec_ctx::current()
        .context("native journal lost its pinned execution context")?
        .note_workspace_changes(&[resource.path().to_path_buf()]);
    Ok(after)
}

/// A bounded cohort reservation retains the actual storage lock. Other workspaces cannot consume
/// its capacity between files. Callers acquire all resource leases BEFORE this later journal lock,
/// and drop the batch before acquiring any further workspace/metadata/resource lock.
pub(crate) struct ApplyBatch {
    storage: Storage,
    journal: Journal,
    owner: String,
    entries: Vec<BatchEntry>,
    next: usize,
    coverage: Option<BatchIdentity>,
}

struct BatchEntry {
    resource: NativeResource,
    expected: FileFingerprint,
    after: FileFingerprint,
    intent: Option<Metadata>,
}

/// Check every preimage and cumulative capacity before the first live effect. The input borrows
/// controller-owned candidate bytes; only hashes/bindings are retained as authority for apply.
#[cfg(test)]
pub(crate) fn begin_batch(
    owner: &str,
    writes: &[(&NativeResource, &FileFingerprint, &[u8])],
) -> Result<ApplyBatch> {
    begin_batch_inner(owner, writes, false)
}

/// Publish the complete ordered changed-write manifest before allowing any batch source effect.
/// A published fence with missing records is unresolved evidence, never evidence of NotApplied.
pub(crate) fn begin_covered_batch(
    owner: &str,
    writes: &[(&NativeResource, &FileFingerprint, &[u8])],
) -> Result<ApplyBatch> {
    begin_batch_inner(owner, writes, true)
}

fn begin_batch_inner(
    owner: &str,
    writes: &[(&NativeResource, &FileFingerprint, &[u8])],
    covered: bool,
) -> Result<ApplyBatch> {
    check_owner(owner)?;
    anyhow::ensure!(writes.len() <= MAX_RECORDS, "native journal batch exceeds record capacity; nothing was written");
    let storage = Storage::open()?;
    let mut journal = storage.load()?;
    let coverage = if covered {
        anyhow::ensure!(journal.fences.len() < MAX_FENCES, "native journal fence capacity exhausted");
        let epoch = match journal.epoch { Some(epoch) => epoch, None => random_identity()? };
        let batch = random_identity()?;
        anyhow::ensure!(!journal.fences.iter().any(|fence| fence.batch == batch),
            "native journal batch identity collision");
        Some(BatchIdentity { epoch, batch })
    } else { None };
    let mut keys = std::collections::BTreeSet::new();
    let mut entries = Vec::with_capacity(writes.len());
    let mut sizes = Vec::with_capacity(writes.len());
    let mut planned = Vec::with_capacity(writes.len());
    for (resource, expected, bytes) in writes {
        resource.verify()?;
        storage.check_resource(resource)?;
        anyhow::ensure!(keys.insert(resource.key()), "native journal batch has duplicate resources");
        anyhow::ensure!(bytes.len() <= MAX_SOURCE_BYTES, "native journal source exceeds 4 MiB");
        let binding = binding(resource)?;
        reconcile_locked(resource, &storage, &mut journal)?;
        ensure_resolved(resource, &journal)?;
        let (before, actual) = read_image(resource.path())?;
        anyhow::ensure!(&actual == *expected, "native journal batch preimage conflict; nothing was written");
        let after = FileFingerprint::for_bytes(bytes);
        let intent = if actual != after {
            let id = journal.next_id()?.checked_add(planned.len() as u64)
                .context("native journal batch record ID exhausted")?;
            let meta = Metadata {
                id, owner: owner.to_owned(), binding,
                operation: Operation::Write, state: State::Prepared, outcome: None,
                before: (&actual).into(), after: (&after).into(),
                coverage: coverage.as_ref().map(|identity| IntentTag {
                    batch: identity.batch, ordinal: planned.len() as u32,
                }),
            };
            metadata_bytes(&meta)?;
            sizes.push((before.as_ref().map_or(0, Vec::len), bytes.len()));
            planned.push(meta.clone());
            Some(meta)
        } else { None };
        entries.push(BatchEntry { resource: (*resource).clone(), expected: actual, after, intent });
    }
    if let Some(identity) = &coverage {
        journal.epoch = Some(identity.epoch);
        let fence = BatchFence {
            version: 1, batch: identity.batch, owner: owner.to_owned(),
            intents: planned.iter().map(|meta| ExpectedIntent {
                id: meta.id, binding: meta.binding.clone(), before: meta.before.clone(), after: meta.after.clone(),
            }).collect(),
        };
        fence_bytes(&fence)?;
        journal.fences.push(fence);
    }
    if coverage.is_some() {
        journal.reserve_batch(&[], &[])?;
    } else {
        journal.reserve_batch(&planned, &sizes)?;
    }
    if let Some(identity) = &coverage {
        fault(Fault::BeforeFence)?;
        // Atomic replacement or its later hardening/sync may fail after publication. Preserve the
        // assigned identity without exposing the underlying error or manufacturing retry authority.
        storage.save(&journal).and_then(|()| fault(Fault::AfterFence))
            .map_err(|_| FencePublicationUncertain { identity: identity.clone() })?;
    }
    Ok(ApplyBatch { storage, journal, owner: owner.to_owned(), entries, next: 0, coverage })
}

pub(crate) struct BatchApply {
    pub(crate) fingerprint: FileFingerprint,
    /// The exact durable native-journal record for this changed entry. A no-op has no record.
    pub(crate) native_record_id: Option<u64>,
}

impl ApplyBatch {
    pub(crate) fn coverage_identity(&self) -> Option<&BatchIdentity> {
        self.coverage.as_ref()
    }

    /// Consume only the next exact planned write and return its durable record identity when it
    /// changed the source. Recheck live bytes immediately before the CAS; a foreign editor can
    /// still race cooperative admission, so this is not an OS transaction.
    pub(crate) fn apply_with_record(
        &mut self,
        resource: &NativeResource,
        expected: &FileFingerprint,
        bytes: &[u8],
    ) -> Result<BatchApply> {
        check_owner(&self.owner)?;
        anyhow::ensure!(bytes.len() <= MAX_SOURCE_BYTES, "native journal source exceeds 4 MiB");
        let entry = self.entries.get(self.next).context("native journal batch exhausted")?;
        anyhow::ensure!(entry.resource == *resource && entry.expected == *expected
            && entry.after == FileFingerprint::for_bytes(bytes),
            "native journal batch write differs from admitted candidate");
        let after = entry.after.clone();
        let intent = entry.intent.clone();
        let next = self.next + 1;
        // A failed CAS or uncertain write consumes this capability; callers must not retry it.
        self.next = self.entries.len();
        resource.verify()?;
        self.storage.check_resource(resource)?;
        let (before, actual) = read_image(resource.path())?;
        anyhow::ensure!(&actual == expected, "native journal batch edit conflict; nothing was written");
        let native_record_id = if actual != after {
            let meta = intent.context("native journal batch changed intent missing")?;
            anyhow::ensure!(meta.binding == binding(resource)?, "native journal batch historical binding changed");
            let record = Record { meta, before, after: Some(bytes.to_vec()) };
            self.journal.append(record)?;
            Some(commit_last(resource, &self.storage, &mut self.journal)?)
        } else {
            None
        };
        if native_record_id.is_some() {
            exec_ctx::current().context("native journal lost its pinned execution context")?
                .note_workspace_changes(&[resource.path().to_path_buf()]);
        }
        self.next = next;
        Ok(BatchApply { fingerprint: after, native_record_id })
    }

    /// Compatibility wrapper for tests that only need the resulting image.
    #[cfg(test)]
    pub(crate) fn apply(
        &mut self,
        resource: &NativeResource,
        expected: &FileFingerprint,
        bytes: &[u8],
    ) -> Result<FileFingerprint> {
        self.apply_with_record(resource, expected, bytes)
            .map(|result| result.fingerprint)
    }
}


/// Reconcile only this leased resource. Never restore/undo bytes. A conflict remains unresolved and
/// is returned explicitly; apply/undo refuse to proceed while any unresolved intent remains.
#[cfg(test)]
pub(crate) fn reconcile(resource: &NativeResource) -> Result<Vec<Reconciliation>> {
    resource.verify()?;
    validate_mutation_target(resource.path())?;
    let storage = Storage::open()?;
    storage.check_resource(resource)?;
    let mut journal = storage.load()?;
    reconcile_locked(resource, &storage, &mut journal)
}

/// Parent-selected, per-resource undo. Caller must first acquire THIS resource's exclusive lease
/// (in normal global -> resource -> journal order); never call while holding a journal storage lock.
/// Only the selected owner's topmost logical writes are reversed. A sibling tip is an attribution
/// conflict EVEN IF its bytes equal the selected owner's postimage. Disjoint resources are untouched.
/// Partial completion is durable and reported as an error, never automatically rolled back.
pub(crate) fn undo_owner(
    ctx: &ExecutionContext,
    resource: &NativeResource,
    owner: &str,
) -> Result<usize> {
    check_undo_authority(ctx)?;
    check_text(owner, MAX_OWNER_BYTES, "owner")?;
    resource.verify()?;
    validate_mutation_target(resource.path())?;
    let storage = Storage::open()?;
    storage.check_resource(resource)?;
    let mut journal = storage.load()?;
    reconcile_locked(resource, &storage, &mut journal)?;
    ensure_resolved(resource, &journal)?;
    let mut resource = fresh_resource(resource)?;
    let mut count = 0;
    loop {
        let stack = journal.active_stack(resource.key())?;
        if !stack
            .iter()
            .any(|&i| journal.records[i].meta.owner == owner)
        {
            return Ok(count);
        }
        let target = *stack.last().context("native journal missing undo tip")?;
        let original = &journal.records[target];
        anyhow::ensure!(original.meta.owner == owner,
            "native journal undo conflict: newer sibling mutation; {count} earlier reversals settled");
        let (before, current) = read_image(resource.path())?;
        anyhow::ensure!(current == original.meta.after.file(),
            "native journal undo conflict: current postimage changed; {count} earlier reversals settled");
        if original.before.is_none() {
            ensure_durable_deletion_supported()?;
        }
        journal.reserve(
            before.as_ref().map_or(0, Vec::len),
            original.before.as_ref().map_or(0, Vec::len),
        )?;
        let record = Record {
            meta: Metadata {
                id: journal.next_id()?,
                owner: ctx.resource_scope(),
                binding: binding(&resource)?,
                operation: Operation::Undo {
                    target: original.meta.id,
                },
                coverage: None,
                state: State::Prepared,
                outcome: None,
                before: (&current).into(),
                after: original.meta.before.clone(),
            },
            before,
            after: original.before.clone(),
        };
        journal.append(record)?;
        commit_last(&resource, &storage, &mut journal)?;
        count += 1;
        // Atomic replacement/restoration changes file identity. Resolve anew under the SAME lease;
        // only replacement-stable path+key are allowed to survive. This also covers file deletion.
        resource = fresh_resource(&resource).map_err(|_| PossiblyApplied {
            record_id: journal.records.last().map_or(0, |r| r.meta.id),
        })?;
    }
}

fn check_text(value: &str, limit: usize, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= limit && !value.contains('\0'),
        "invalid native journal {label}"
    );
    Ok(())
}

fn check_owner(owner: &str) -> Result<()> {
    check_text(owner, MAX_OWNER_BYTES, "owner")?;
    let ctx = exec_ctx::current().context("native journal requires a pinned execution context")?;
    anyhow::ensure!(
        ctx.resource_scope() == owner,
        "native journal owner does not match physical execution scope"
    );
    Ok(())
}

fn check_undo_authority(ctx: &ExecutionContext) -> Result<()> {
    anyhow::ensure!(
        ctx.can_restore_workspace(),
        "delegated execution cannot undo native mutations"
    );
    let current =
        exec_ctx::current().context("native journal undo requires a pinned parent context")?;
    anyhow::ensure!(
        current.can_restore_workspace() && current.resource_scope() == ctx.resource_scope(),
        "current execution has no parent native undo authority"
    );
    check_text(&ctx.resource_scope(), MAX_OWNER_BYTES, "parent owner")
}

fn binding(resource: &NativeResource) -> Result<Binding> {
    let path = resource.path();
    let text = path
        .to_str()
        .context("native journal resource path must be losslessly serializable")?;
    check_text(text, MAX_BINDING_BYTES, "path")?;
    anyhow::ensure!(
        path.is_absolute(),
        "native journal resource path must be absolute"
    );
    check_text(resource.key(), MAX_BINDING_BYTES, "key")?;
    Ok(Binding {
        path: path.to_path_buf(),
        key: resource.key().to_owned(),
        parent_identity: directory_identity(
            path.parent()
                .context("native journal resource parent missing")?,
        )?,
    })
}

fn fresh_resource(resource: &NativeResource) -> Result<NativeResource> {
    resource.verify()?; // Do not bless a swapped ancestor by resolving a new identity after replacement.
    let next = NativeResource::resolve(resource.path())?;
    anyhow::ensure!(
        next.path() == resource.path() && next.key() == resource.key(),
        "native journal replacement changed resource binding"
    );
    next.verify()?;
    Ok(next)
}

fn matches_binding(binding: &Binding, resource: &NativeResource) -> Result<bool> {
    if binding.key != resource.key() {
        anyhow::ensure!(
            binding.path != resource.path(),
            "native journal resource key drift"
        );
        return Ok(false);
    }
    anyhow::ensure!(
        binding.path == resource.path(),
        "native journal ambiguous resource binding"
    );
    anyhow::ensure!(
        directory_identity(
            resource
                .path()
                .parent()
                .context("native journal resource parent missing")?
        )? == binding.parent_identity,
        "native journal historical parent identity changed"
    );
    Ok(true)
}

fn matches_resource(record: &Record, resource: &NativeResource) -> Result<bool> {
    matches_binding(&record.meta.binding, resource)
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        meta.is_dir() && !is_link(&meta),
        "native journal parent is not a plain directory"
    );
    Ok((meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn directory_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let name: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(
        handle != INVALID_HANDLE_VALUE,
        "native journal parent identity unavailable"
    );
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe {
        CloseHandle(handle);
    }
    anyhow::ensure!(
        ok != 0 && info.dwFileAttributes & 0x400 == 0 && info.dwFileAttributes & 0x10 != 0,
        "native journal parent identity is ambiguous"
    );
    Ok((
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

#[cfg(not(any(unix, windows)))]
fn directory_identity(_path: &Path) -> Result<(u64, u64)> {
    anyhow::bail!("native journal parent identity is unsupported")
}

/// Existing bounded helper checks metadata AND caps actual reads, including concurrent growth.
fn read_image(path: &Path) -> Result<(Option<Vec<u8>>, FileFingerprint)> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((None, FileFingerprint::missing()))
        }
        Err(_) => anyhow::bail!("native journal destination is unreadable"),
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_file() && !is_link(&meta),
                "native journal destination is not a plain file"
            );
            let bytes =
                crate::core::session_store::read_bounded(path, MAX_SOURCE_BYTES).map_err(|_| {
                    anyhow::anyhow!("native journal destination read failed or exceeds 4 MiB")
                })?;
            let fingerprint = FileFingerprint::for_bytes(&bytes);
            Ok((Some(bytes), fingerprint))
        }
    }
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn ensure_resolved(resource: &NativeResource, journal: &Journal) -> Result<()> {
    for record in &journal.records {
        if matches_resource(record, resource)? {
            anyhow::ensure!(
                record.meta.state == State::Settled,
                "native journal unresolved conflict in record {}; mutation refused",
                record.meta.id
            );
        }
    }
    for fence in &journal.fences {
        for intent in &fence.intents {
            if !matches_binding(&intent.binding, resource)? {
                continue;
            }
            anyhow::ensure!(
                journal.records.iter().any(|record| record.meta.id == intent.id),
                "native journal covered intent {} has no durable record; mutation refused",
                intent.id
            );
        }
    }
    Ok(())
}

fn reconcile_locked(
    resource: &NativeResource,
    storage: &Storage,
    journal: &mut Journal,
) -> Result<Vec<Reconciliation>> {
    let unresolved: Vec<usize> = journal
        .records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| (r.meta.state != State::Settled).then_some(i))
        .collect();
    let mut result = Vec::new();
    for index in unresolved {
        let record = &journal.records[index];
        if !matches_resource(record, resource)? {
            continue;
        }
        resource.verify()?;
        let (_, actual) = read_image(resource.path())?;
        let id = record.meta.id;
        let outcome = if actual == record.meta.after.file() {
            Outcome::Applied
        } else if actual == record.meta.before.file() && record.meta.state == State::Prepared {
            Outcome::NotApplied
        } else {
            result.push(Reconciliation::Conflict { record_id: id });
            continue;
        };
        if outcome == Outcome::Applied {
            journal.records[index].meta.state = State::Applied;
            journal.records[index].meta.outcome = Some(outcome);
            storage
                .save(journal)
                .map_err(|_| PossiblyApplied { record_id: id })?;
        }
        journal.records[index].meta.state = State::Settled;
        journal.records[index].meta.outcome = Some(outcome);
        storage
            .save(journal)
            .map_err(|_| PossiblyApplied { record_id: id })?;
        result.push(match outcome {
            Outcome::Applied => Reconciliation::After { record_id: id },
            Outcome::NotApplied => Reconciliation::Before { record_id: id },
        });
    }
    Ok(result)
}

fn commit_last(resource: &NativeResource, storage: &Storage, journal: &mut Journal) -> Result<u64> {
    fault(Fault::BeforePrepared)?;
    let index = journal.records.len() - 1;
    let id = journal.records[index].meta.id;
    // A failed save can already have published Prepared. Preserve its assigned ID from the first
    // publication attempt onward, even when source mutation has not started. Never expose bytes
    // or infer that a save error proves either durable intent or absence of an effect.
    let completed = (|| -> Result<()> {
        storage.save(journal)?; // full capacity reserved; no mutation before durable intent
        fault(Fault::Prepared)?;
        {
            let record = &journal.records[index];
            guarded_mutation(
                resource,
                &record.meta.before.file(),
                record.after.as_deref(),
            )?;
            fault(Fault::Write)?;
            let next = fresh_resource(resource)?;
            let (_, actual) = read_image(next.path())?;
            anyhow::ensure!(
                actual == record.meta.after.file(),
                "native journal postimage verification failed"
            );
        }
        journal.records[index].meta.state = State::Applied;
        journal.records[index].meta.outcome = Some(Outcome::Applied);
        storage.save(journal)?;
        fault(Fault::Applied)?;
        fault(Fault::Completion)?;
        journal.records[index].meta.state = State::Settled;
        storage.save(journal)?;
        Ok(())
    })();
    completed.map(|()| id).map_err(|_| PossiblyApplied { record_id: id }.into())
}

fn guarded_mutation(
    resource: &NativeResource,
    expected: &FileFingerprint,
    after: Option<&[u8]>,
) -> Result<()> {
    resource.verify()?;
    let (_, actual) = read_image(resource.path())?;
    anyhow::ensure!(&actual == expected, "native journal CAS conflict");
    match after {
        Some(bytes) if expected.exists => persist::atomic_write(resource.path(), bytes)?,
        Some(bytes) => {
            // No-clobber creation. Partial writes are covered by the durable Prepared record.
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(resource.path())?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        None => {
            ensure_durable_deletion_supported()?;
            anyhow::ensure!(expected.exists, "native journal refuses redundant deletion");
            fs::remove_file(resource.path())?;
        }
    }
    sync_directory(
        resource
            .path()
            .parent()
            .context("native journal destination parent missing")?,
    )
}

fn ensure_durable_deletion_supported() -> Result<()> {
    // Windows persist has a write-through replacement primitive, but no checked durable unlink.
    // Do not advertise creation undo there until one exists; refusing occurs before its intent/CAS.
    anyhow::ensure!(cfg!(unix), "native journal created-file deletion undo is unsupported without a durable unlink primitive");
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path; // Windows replacement uses persist's MOVEFILE_WRITE_THROUGH.
    Ok(())
}

/// Read-only destination validation shared by ordinary and scoped file mutations. Resolves the
/// nearest existing ancestors without opening storage, creating directories, or acquiring locks.
/// Both the replaced node and its followed alias are reserved, including absent nested targets.
pub(crate) fn validate_mutation_target(target: &Path) -> Result<()> {
    let home = canonical_nearest_existing(&storage_home())?;
    let reserved = home.join("native-journal-v1");
    let reserved_alias = canonical_nearest_existing(&reserved)?;
    let parent = target
        .parent()
        .context("mutation destination has no parent")?;
    let leaf = target
        .file_name()
        .context("mutation destination has no basename")?;
    let replaced_node = canonical_nearest_existing(parent)?.join(leaf);
    let followed = canonical_nearest_existing(target)?;
    for destination in [&replaced_node, &followed] {
        for root in [&reserved, &reserved_alias] {
            anyhow::ensure!(
                !path_within(destination, root),
                "native journal storage cannot be a mutation destination"
            );
        }
    }
    Ok(())
}

fn path_within(path: &Path, root: &Path) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let path = crate::core::config::normalize_path_key(path).to_lowercase();
        let root = crate::core::config::normalize_path_key(root).to_lowercase();
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|suffix| suffix.starts_with(['/', '\\']))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        path.starts_with(root)
    }
}

fn canonical_nearest_existing(path: &Path) -> Result<PathBuf> {
    canonical_nearest_existing_with(path, |path| fs::canonicalize(path))
}

fn canonical_nearest_existing_with(
    path: &Path,
    mut canonicalize: impl FnMut(&Path) -> std::io::Result<PathBuf>,
) -> Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute() && path.to_string_lossy().len() <= MAX_BINDING_BYTES,
        "journal destination validation requires a bounded absolute path"
    );
    let mut normalized = PathBuf::new();
    let mut depth = 0;
    for component in path.components() {
        depth += 1;
        anyhow::ensure!(
            depth <= 128,
            "journal destination ancestry exceeds safety limit"
        );
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                anyhow::ensure!(
                    normalized.pop(),
                    "journal destination traverses above its root"
                );
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    let canonical = loop {
        match canonicalize(existing) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(existing) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(meta) if !is_link(&meta) => {
                        // Another native body may create journal storage between these probes.
                        // Retry this now-existing plain node once; never ascend on a stale miss.
                        break canonicalize(existing)
                            .context("journal destination changed during validation")?;
                    }
                    // A dangling alias is unknown, not permission to bless its textual destination.
                    _ => anyhow::bail!(
                        "journal destination validation refuses a dangling or unreadable alias"
                    ),
                }
                missing.push(
                    existing
                        .file_name()
                        .context("journal destination has no existing ancestor")?
                        .to_os_string(),
                );
                existing = existing
                    .parent()
                    .context("journal destination has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("resolving journal mutation destination"),
        }
    };
    let mut result = canonical;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

fn storage_home() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_STORAGE.with(|slot| slot.borrow().clone()) {
        return root;
    }
    crate::core::config::aizen_home()
}

#[cfg(test)]
thread_local! {
    static TEST_STORAGE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Override only this thread's journal storage with an explicitly owned fixture root, restoring on
/// return/panic. Does not alter AIZEN_HOME, ordinary resource locks, or any process-global config.
#[cfg(test)]
pub(crate) fn with_test_storage<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_STORAGE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let old = TEST_STORAGE.with(|slot| slot.replace(Some(root.to_path_buf())));
    let _restore = Restore(old);
    f()
}

fn storage_directory() -> Result<PathBuf> {
    Ok(fs::canonicalize(storage_home())
        .context("resolving native journal home")?
        .join("native-journal-v1"))
}

struct Storage {
    _lock: RepoTxnLock,
    path: PathBuf,
}

impl Storage {
    fn open() -> Result<Self> {
        let home = storage_home();
        fs::create_dir_all(&home).context("creating native journal home")?;
        let dir = storage_directory()?;
        let home = dir.parent().context("native journal home missing")?;
        match fs::create_dir(&dir) {
            Ok(()) => sync_directory(home)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => anyhow::bail!("creating private native journal storage failed"),
        }
        let meta = fs::symlink_metadata(&dir)?;
        anyhow::ensure!(
            meta.is_dir() && !is_link(&meta),
            "native journal storage is not a plain directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        persist::harden_owner_only_checked(&dir)?;
        let lock_path = dir.join("storage.lock");
        reject_link(&lock_path)?;
        let lock = RepoTxnLock::acquire_exclusive(&lock_path, Duration::from_secs(5))?;
        persist::harden_owner_only_checked(&lock_path)?;
        let path = dir.join("records.bin");
        reject_link(&path)?;
        Ok(Self { _lock: lock, path })
    }

    fn check_resource(&self, resource: &NativeResource) -> Result<()> {
        validate_mutation_target(resource.path())?;
        let dir = self
            .path
            .parent()
            .context("native journal storage parent missing")?;
        // A source file may never target the journal or its OS lock (self-overwrite would destroy
        // durable attribution). This managed subtree is reserved, not an approved native destination.
        anyhow::ensure!(
            !resource.path().starts_with(dir),
            "native journal storage cannot be a mutation destination"
        );
        Ok(())
    }

    fn load(&self) -> Result<Journal> {
        reject_link(&self.path)?;
        match fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Journal::default()),
            Err(_) => anyhow::bail!("native journal storage is unreadable"),
            Ok(meta) => anyhow::ensure!(meta.is_file(), "native journal storage is not a file"),
        }
        persist::harden_owner_only_checked(&self.path)?;
        let bytes = crate::core::session_store::read_bounded(&self.path, MAX_JOURNAL_BYTES)
            .map_err(|_| anyhow::anyhow!("native journal read failed or exceeds capacity"))?;
        Journal::decode(&bytes)
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        let bytes = journal.encode()?;
        reject_link(&self.path)?;
        fault(Fault::SaveBeforePublish)?;
        persist::atomic_write_owner_only(&self.path, &bytes)?;
        fault(Fault::SaveAfterPublish)?;
        sync_directory(
            self.path
                .parent()
                .context("native journal storage parent missing")?,
        )
    }
}

fn reject_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => anyhow::ensure!(
            !is_link(&meta),
            "native journal refuses linked managed storage"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("native journal managed storage is unreadable"),
    }
    Ok(())
}

impl Journal {
    fn next_id(&self) -> Result<u64> {
        self.records.iter().map(|record| record.meta.id)
            .chain(self.fences.iter().flat_map(|fence| fence.intents.iter().map(|intent| intent.id)))
            .max().map_or(Some(1), |id| id.checked_add(1))
            .context("native journal record ID exhausted")
    }

    fn reserved_records(&self) -> usize {
        self.records.len() + self.fences.iter().flat_map(|fence| &fence.intents)
            .filter(|intent| !self.records.iter().any(|record| record.meta.id == intent.id)).count()
    }

    fn reserve_batch(&self, planned: &[Metadata], sizes: &[(usize, usize)]) -> Result<()> {
        anyhow::ensure!(planned.len() == sizes.len(), "native journal reservation mismatch");
        anyhow::ensure!(self.reserved_records().checked_add(planned.len()).is_some_and(|total| total <= MAX_RECORDS),
            "native journal batch full; nothing was written");
        let mut total = self.reserved_size()?;
        for (meta, (before_len, after_len)) in planned.iter().zip(sizes) {
            anyhow::ensure!(*before_len <= MAX_SOURCE_BYTES && *after_len <= MAX_SOURCE_BYTES,
                "native journal image exceeds limit");
            total = total.checked_add(record_reserved_size(meta, *before_len, *after_len)?)
                .context("native journal batch size overflow")?;
        }
        anyhow::ensure!(total <= MAX_JOURNAL_BYTES, "native journal batch full; nothing was written");
        Ok(())
    }

    fn reserve(&self, before_len: usize, after_len: usize) -> Result<()> {
        anyhow::ensure!(before_len <= MAX_SOURCE_BYTES && after_len <= MAX_SOURCE_BYTES,
            "native journal image exceeds limit");
        anyhow::ensure!(self.reserved_records() < MAX_RECORDS, "native journal full; nothing was written");
        let total = self.reserved_size()?.checked_add(MAX_META_BYTES + 12 + before_len + after_len)
            .context("native journal size overflow")?;
        anyhow::ensure!(total <= MAX_JOURNAL_BYTES, "native journal full; nothing was written");
        Ok(())
    }

    fn append(&mut self, record: Record) -> Result<()> {
        validate_record(&record)?;
        anyhow::ensure!(self.records.last().is_none_or(|last| last.meta.id < record.meta.id),
            "native journal record IDs out of order");
        metadata_bytes(&record.meta)?;
        if record.meta.coverage.is_some() {
            self.validate_tagged_record(&record)?;
            anyhow::ensure!(!self.records.iter().any(|other| other.meta.id == record.meta.id),
                "native journal duplicate covered intent");
            anyhow::ensure!(self.reserved_records() <= MAX_RECORDS && self.reserved_size()? <= MAX_JOURNAL_BYTES,
                "native journal full; nothing was written");
        } else {
            self.reserve_batch(std::slice::from_ref(&record.meta), &[(
                record.before.as_ref().map_or(0, Vec::len), record.after.as_ref().map_or(0, Vec::len),
            )])?;
            anyhow::ensure!(!self.fences.iter().flat_map(|fence| &fence.intents).any(|intent| intent.id == record.meta.id),
                "native journal reserved intent ID reused");
        }
        self.records.push(record);
        Ok(())
    }

    fn reserved_size(&self) -> Result<usize> {
        let mut total = if self.epoch.is_some() { MAGIC_V2.len() + 32 + 8 } else { MAGIC.len() + 4 };
        for fence in &self.fences {
            total = total.checked_add(4 + fence_bytes(fence)?.len()).context("native journal size overflow")?;
            for (ordinal, intent) in fence.intents.iter().enumerate() {
                if !self.records.iter().any(|record| record.meta.id == intent.id) {
                    let meta = fence_metadata(fence, ordinal, intent);
                    total = total.checked_add(record_reserved_size(&meta, intent.before.byte_len as usize,
                        intent.after.byte_len as usize)?).context("native journal size overflow")?;
                }
            }
        }
        for record in &self.records {
            total = total.checked_add(record_reserved_size(&record.meta,
                record.before.as_ref().map_or(0, Vec::len), record.after.as_ref().map_or(0, Vec::len))?)
                .context("native journal size overflow")?;
        }
        Ok(total)
    }

    fn validate_tagged_record(&self, record: &Record) -> Result<()> {
        let tag = record.meta.coverage.as_ref().context("native journal missing intent tag")?;
        let fence = self.fences.iter().find(|fence| fence.batch == tag.batch)
            .context("native journal unknown intent tag")?;
        let intent = fence.intents.get(tag.ordinal as usize).context("native journal unknown intent ordinal")?;
        anyhow::ensure!(record.meta.id == intent.id && record.meta.owner == fence.owner
            && matches!(record.meta.operation, Operation::Write) && record.meta.binding == intent.binding
            && record.meta.before == intent.before && record.meta.after == intent.after,
            "native journal covered intent mismatch");
        Ok(())
    }

    fn validate_coverage(&self) -> Result<()> {
        anyhow::ensure!(self.fences.len() <= MAX_FENCES, "native journal fence capacity exceeded");
        anyhow::ensure!(self.records.len() <= MAX_RECORDS && self.reserved_records() <= MAX_RECORDS,
            "native journal reserved record capacity exceeded");
        if let Some(epoch) = self.epoch {
            anyhow::ensure!(epoch != [0; 32], "native journal invalid epoch");
        } else {
            anyhow::ensure!(self.fences.is_empty() && self.records.iter().all(|record| record.meta.coverage.is_none()),
                "native journal legacy coverage invalid");
        }
        let mut batches = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        let mut previous = 0;
        for fence in &self.fences {
            anyhow::ensure!(fence.version == 1 && fence.batch != [0; 32] && batches.insert(fence.batch)
                && fence.intents.len() <= MAX_RECORDS, "native journal invalid coverage fence");
            check_text(&fence.owner, MAX_OWNER_BYTES, "owner")?;
            let mut keys = std::collections::BTreeSet::new();
            let mut paths = std::collections::BTreeSet::new();
            for (ordinal, intent) in fence.intents.iter().enumerate() {
                validate_binding(&intent.binding)?;
                validate_fingerprint(&intent.before)?;
                validate_fingerprint(&intent.after)?;
                anyhow::ensure!(intent.id > previous && ids.insert(intent.id) && intent.before != intent.after
                    && intent.after.exists && keys.insert(&intent.binding.key) && paths.insert(&intent.binding.path),
                    "native journal invalid expected intent");
                if ordinal > 0 {
                    anyhow::ensure!(fence.intents[ordinal - 1].id.checked_add(1) == Some(intent.id),
                        "native journal expected intent order invalid");
                }
                previous = intent.id;
                if let Some(record) = self.records.iter().find(|record| record.meta.id == intent.id) {
                    anyhow::ensure!(record.meta.coverage == Some(IntentTag { batch: fence.batch, ordinal: ordinal as u32 }),
                        "native journal intent tag absent or changed");
                }
            }
            fence_bytes(fence)?;
        }
        for record in &self.records {
            if record.meta.coverage.is_some() {
                self.validate_tagged_record(record)?;
            } else {
                anyhow::ensure!(!ids.contains(&record.meta.id), "native journal expected intent tag missing");
            }
        }
        let bindings: Vec<_> = self.fences.iter().flat_map(|fence| fence.intents.iter().map(|intent| &intent.binding))
            .chain(self.records.iter().map(|record| &record.meta.binding)).collect();
        for (index, binding) in bindings.iter().enumerate() {
            for earlier in &bindings[..index] {
                let same_key = binding.key == earlier.key;
                anyhow::ensure!(same_key == (binding.path == earlier.path)
                    && (!same_key || binding.parent_identity == earlier.parent_identity),
                    "native journal inconsistent fenced binding");
            }
        }
        anyhow::ensure!(self.reserved_records() <= MAX_RECORDS, "native journal reserved record capacity exceeded");
        Ok(())
    }

    fn active_stack(&self, key: &str) -> Result<Vec<usize>> {
        let mut stack = Vec::new();
        for (i, record) in self.records.iter().enumerate() {
            if record.meta.binding.key != key {
                continue;
            }
            match record.meta.operation {
                Operation::Write => {
                    if record.meta.outcome == Some(Outcome::Applied) {
                        stack.push(i);
                    }
                }
                Operation::Undo { target } => {
                    let tip = *stack.last().context("native journal invalid undo chain")?;
                    anyhow::ensure!(
                        self.records[tip].meta.id == target,
                        "native journal undo target is not the logical tip"
                    );
                    anyhow::ensure!(
                        record.meta.before == self.records[tip].meta.after
                            && record.meta.after == self.records[tip].meta.before,
                        "native journal undo image chain mismatch"
                    );
                    if record.meta.outcome == Some(Outcome::Applied) {
                        stack.pop();
                    }
                }
            }
        }
        Ok(stack)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate_coverage()?;
        anyhow::ensure!(
            self.records.len() <= MAX_RECORDS && self.reserved_size()? <= MAX_JOURNAL_BYTES,
            "native journal full; nothing was written"
        );
        let mut out = Vec::with_capacity(self.reserved_size()?);
        if let Some(epoch) = self.epoch {
            out.extend_from_slice(MAGIC_V2);
            out.extend_from_slice(&epoch);
            put_u32(&mut out, self.fences.len() as u32);
            for fence in &self.fences {
                let bytes = fence_bytes(fence)?;
                put_u32(&mut out, bytes.len() as u32);
                out.extend_from_slice(&bytes);
            }
        } else {
            out.extend_from_slice(MAGIC);
        }
        put_u32(&mut out, self.records.len() as u32);
        for record in &self.records {
            validate_record(record)?;
            let meta = metadata_bytes(&record.meta)?;
            put_u32(&mut out, meta.len() as u32);
            out.extend_from_slice(&meta);
            for image in [&record.before, &record.after] {
                if let Some(bytes) = image {
                    put_u32(&mut out, bytes.len() as u32);
                    out.extend_from_slice(bytes);
                } else {
                    put_u32(&mut out, u32::MAX);
                }
            }
        }
        anyhow::ensure!(out.len() <= MAX_JOURNAL_BYTES, "native journal full");
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_JOURNAL_BYTES,
            "native journal exceeds limit"
        );
        let mut cursor = Cursor(bytes);
        let magic = cursor.take(MAGIC.len())?;
        let (epoch, fences) = if magic == MAGIC_V2 {
            let epoch: [u8; 32] = cursor.take(32)?.try_into()
                .map_err(|_| anyhow::anyhow!("native journal epoch truncated"))?;
            anyhow::ensure!(epoch != [0; 32], "native journal invalid epoch");
            let count = cursor.u32()? as usize;
            anyhow::ensure!(count <= MAX_FENCES, "native journal fence count exceeds limit");
            let mut fences = Vec::with_capacity(count);
            for _ in 0..count {
                let len = cursor.u32()? as usize;
                anyhow::ensure!(len <= MAX_FENCE_META_BYTES, "native journal fence metadata exceeds limit");
                let fence: BatchFence = serde_json::from_slice(cursor.take(len)?)
                    .map_err(|_| anyhow::anyhow!("native journal fence metadata corrupt"))?;
                fences.push(fence);
            }
            (Some(epoch), fences)
        } else {
            anyhow::ensure!(magic == MAGIC, "native journal version invalid");
            (None, Vec::new())
        };
        let count = cursor.u32()? as usize;
        anyhow::ensure!(
            count <= MAX_RECORDS,
            "native journal record count exceeds limit"
        );
        let mut records = Vec::with_capacity(count);
        let mut previous = 0;
        for _ in 0..count {
            let len = cursor.u32()? as usize;
            anyhow::ensure!(
                len <= MAX_META_BYTES,
                "native journal metadata exceeds limit"
            );
            // Bounded metadata errors are deliberately not interpolated: JSON can contain secrets.
            let meta: Metadata = serde_json::from_slice(cursor.take(len)?)
                .map_err(|_| anyhow::anyhow!("native journal metadata corrupt"))?;
            anyhow::ensure!(meta.id > previous, "native journal record IDs out of order");
            previous = meta.id;
            let before = cursor.image()?;
            let after = cursor.image()?;
            let record = Record {
                meta,
                before,
                after,
            };
            validate_record(&record)?;
            records.push(record);
        }
        anyhow::ensure!(cursor.0.is_empty(), "native journal trailing bytes");
        let journal = Self { epoch, fences, records };
        journal.validate_coverage()?;
        anyhow::ensure!(
            journal.reserved_size()? <= MAX_JOURNAL_BYTES,
            "native journal reserved capacity invalid"
        );
        let mut seen_keys = std::collections::HashSet::new();
        for record in &journal.records {
            if seen_keys.insert(&record.meta.binding.key) {
                journal.active_stack(&record.meta.binding.key)?;
            }
        }
        for (i, record) in journal.records.iter().enumerate() {
            for earlier in &journal.records[..i] {
                let same_key = earlier.meta.binding.key == record.meta.binding.key;
                let same_path = earlier.meta.binding.path == record.meta.binding.path;
                anyhow::ensure!(
                    same_key == same_path,
                    "native journal inconsistent resource binding"
                );
                anyhow::ensure!(
                    !same_key
                        || earlier.meta.binding.parent_identity
                            == record.meta.binding.parent_identity,
                    "native journal inconsistent historical parent identity"
                );
                anyhow::ensure!(
                    !(same_key && earlier.meta.state != State::Settled),
                    "native journal overlapping unresolved intents"
                );
            }
        }
        Ok(journal)
    }
}

fn fence_metadata(fence: &BatchFence, ordinal: usize, intent: &ExpectedIntent) -> Metadata {
    Metadata {
        id: intent.id, owner: fence.owner.clone(), binding: intent.binding.clone(),
        operation: Operation::Write, state: State::Prepared, outcome: None,
        before: intent.before.clone(), after: intent.after.clone(),
        coverage: Some(IntentTag { batch: fence.batch, ordinal: ordinal as u32 }),
    }
}

fn record_reserved_size(meta: &Metadata, before_len: usize, after_len: usize) -> Result<usize> {
    anyhow::ensure!(before_len <= MAX_SOURCE_BYTES && after_len <= MAX_SOURCE_BYTES,
        "native journal image exceeds limit");
    let mut future = meta.clone();
    let mut len = metadata_bytes(meta)?.len();
    for (state, outcome) in [
        (State::Prepared, None), (State::Applied, Some(Outcome::Applied)),
        (State::Settled, Some(Outcome::Applied)), (State::Settled, Some(Outcome::NotApplied)),
    ] {
        future.state = state;
        future.outcome = outcome;
        len = len.max(metadata_bytes(&future)?.len());
    }
    len.checked_add(12).and_then(|n| n.checked_add(before_len)).and_then(|n| n.checked_add(after_len))
        .context("native journal size overflow")
}

fn fence_bytes(fence: &BatchFence) -> Result<Vec<u8>> {
    bounded_metadata_bytes(fence, MAX_FENCE_META_BYTES)
}

fn metadata_bytes(meta: &Metadata) -> Result<Vec<u8>> {
    bounded_metadata_bytes(meta, MAX_META_BYTES)
}

fn bounded_metadata_bytes(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Bounded { bytes: Vec<u8>, limit: usize }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("native journal metadata exceeds limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut out = Bounded { bytes: Vec::with_capacity(limit.min(MAX_META_BYTES)), limit };
    serde_json::to_writer(&mut out, value)
        .map_err(|_| anyhow::anyhow!("native journal metadata encoding failed or exceeds limit"))?;
    Ok(out.bytes)
}

fn validate_binding(binding: &Binding) -> Result<()> {
    check_text(&binding.key, MAX_BINDING_BYTES, "key")?;
    let path = binding.path.to_str().context("native journal invalid path encoding")?;
    check_text(path, MAX_BINDING_BYTES, "path")?;
    anyhow::ensure!(binding.path.is_absolute(), "native journal invalid binding");
    Ok(())
}

fn validate_fingerprint(fingerprint: &Fingerprint) -> Result<()> {
    anyhow::ensure!(fingerprint.byte_len <= MAX_SOURCE_BYTES as u64
        && (fingerprint.exists || fingerprint.file() == FileFingerprint::missing()),
        "native journal invalid expected fingerprint");
    Ok(())
}

fn validate_record(record: &Record) -> Result<()> {
    let meta = &record.meta;
    check_text(&meta.owner, MAX_OWNER_BYTES, "owner")?;
    validate_binding(&meta.binding)?;
    anyhow::ensure!(meta.id > 0, "native journal invalid record ID");
    for (image, fingerprint) in [(&record.before, &meta.before), (&record.after, &meta.after)] {
        anyhow::ensure!(
            image.as_ref().is_none_or(|v| v.len() <= MAX_SOURCE_BYTES),
            "native journal image exceeds limit"
        );
        let expected = image
            .as_deref()
            .map_or_else(FileFingerprint::missing, FileFingerprint::for_bytes);
        anyhow::ensure!(
            expected == fingerprint.file(),
            "native journal image fingerprint mismatch"
        );
    }
    anyhow::ensure!(
        meta.before != meta.after,
        "native journal ambiguous identical images"
    );
    anyhow::ensure!(
        match meta.state {
            State::Prepared => meta.outcome.is_none(),
            State::Applied => meta.outcome == Some(Outcome::Applied),
            State::Settled => meta.outcome.is_some(),
        },
        "native journal invalid state/outcome"
    );
    match meta.operation {
        Operation::Write => anyhow::ensure!(meta.after.exists, "native write cannot delete"),
        Operation::Undo { target } => anyhow::ensure!(
            target > 0 && target < meta.id,
            "native journal invalid undo target"
        ),
    }
    Ok(())
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        anyhow::ensure!(len <= self.0.len(), "native journal truncated");
        let (head, tail) = self.0.split_at(len);
        self.0 = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(
            |_| anyhow::anyhow!("native journal truncated"),
        )?))
    }
    fn image(&mut self) -> Result<Option<Vec<u8>>> {
        let len = self.u32()?;
        if len == u32::MAX {
            return Ok(None);
        }
        anyhow::ensure!(
            len as usize <= MAX_SOURCE_BYTES,
            "native journal image exceeds limit"
        );
        Ok(Some(self.take(len as usize)?.to_vec()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    Random,
    BeforeFence,
    AfterFence,
    BeforePrepared,
    SaveBeforePublish,
    SaveAfterPublish,
    Prepared,
    Write,
    Applied,
    Completion,
}

#[cfg(not(test))]
fn fault(_point: Fault) -> Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
}

/// One deterministic failure after actual replacement and before Applied; restores any enclosing
/// injection on return/panic. Source bytes never enter the diagnostic error.
#[cfg(test)]
pub(crate) fn with_test_after_write_failure<T>(f: impl FnOnce() -> T) -> T {
    with_test_fault(Fault::Write, f)
}

/// Fail after durable Prepared and before mutation, preserving the pending intent identity.
#[cfg(test)]
pub(crate) fn with_test_prepared_failure<T>(f: impl FnOnce() -> T) -> T {
    with_test_fault(Fault::Prepared, f)
}

#[cfg(test)]
fn with_test_fault<T>(point: Fault, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Fault>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FAULT.with(|slot| slot.set(self.0));
        }
    }
    let old = FAULT.with(|slot| slot.replace(Some(point)));
    let _restore = Restore(old);
    f()
}

#[cfg(test)]
fn fault(point: Fault) -> Result<()> {
    FAULT.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            anyhow::bail!("injected native journal failure");
        }
        Ok(())
    })
}

#[cfg(test)]
#[path = "native_journal_coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::repo_lock::LockMode;
    use crate::core::workspace_txn::NativeResourceLease;

    #[test]
    fn nearest_existing_validation_retries_concurrent_plain_creation() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        for (name, directory) in [
            ("home", true),
            ("native-journal-v1", true),
            ("new.txt", false),
        ] {
            let created = temp.path().join(name);
            let target = if directory {
                created.join("nested/new.txt")
            } else {
                created.clone()
            };
            let mut probes = 0;
            let resolved = canonical_nearest_existing_with(&target, |path| {
                let result = fs::canonicalize(path);
                if path == created {
                    probes += 1;
                    if probes == 1 {
                        assert_eq!(
                            result.as_ref().unwrap_err().kind(),
                            std::io::ErrorKind::NotFound
                        );
                        if directory {
                            fs::create_dir(&created).unwrap();
                        } else {
                            fs::write(&created, b"owned fixture").unwrap();
                        }
                    }
                }
                result
            })
            .unwrap();
            let expected = if directory {
                fs::canonicalize(&created).unwrap().join("nested/new.txt")
            } else {
                fs::canonicalize(&created).unwrap()
            };
            assert_eq!(resolved, expected);
            assert_eq!(probes, 2, "creation retries only the now-existing node");
        }
    }

    #[test]
    fn nearest_existing_validation_refuses_drift_after_bounded_retry() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let created = temp.path().join("journal");
        let mut probes = 0;
        let result = canonical_nearest_existing_with(&created, |path| {
            assert_eq!(path, created);
            probes += 1;
            if probes == 2 {
                fs::remove_dir(&created).unwrap();
            }
            let result = fs::canonicalize(path);
            if probes == 1 {
                fs::create_dir(&created).unwrap();
            }
            result
        });
        assert!(
            result.is_err(),
            "a second miss must not bless a textual alias"
        );
        assert_eq!(probes, 2, "validation never spins on changing paths");
    }

    #[cfg(unix)]
    #[test]
    fn nearest_existing_validation_refuses_concurrent_dangling_alias() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let alias = temp.path().join("alias");
        let mut probes = 0;
        let result = canonical_nearest_existing_with(&alias, |path| {
            probes += 1;
            let result = fs::canonicalize(path);
            std::os::unix::fs::symlink(temp.path().join("missing"), &alias).unwrap();
            result
        });
        assert!(
            result.is_err(),
            "a dangling link never qualifies for the plain-node retry"
        );
        assert_eq!(probes, 1);
    }

    #[test]
    fn reserved_storage_target_validation_is_read_only_even_with_missing_home() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let home = temp.path().join("missing").join("home");
        with_test_storage(&home, || {
            for target in [
                home.join("native-journal-v1").join("records.bin"),
                home.join("native-journal-v1")
                    .join("nested")
                    .join("new.txt"),
            ] {
                assert!(validate_mutation_target(&target).is_err());
            }
            assert!(
                validate_mutation_target(&home.join("native-journal-v10").join("source.rs"))
                    .is_ok()
            );
            let resource =
                NativeResource::resolve(&home.join("native-journal-v1").join("records.bin"));
            assert!(
                resource.is_err(),
                "uncreated parent is never a valid native source"
            );
            assert!(
                !home.exists(),
                "validation must not create home, storage, or lock"
            );
            assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        });
    }

    #[cfg(unix)]
    #[test]
    fn reserved_storage_validation_follows_parent_and_leaf_aliases_read_only() {
        use std::os::unix::fs::symlink;
        let temp = crate::core::file_placement::tests::TestDir::new();
        let home = temp.path().join("home");
        let work = temp.path().join("work");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&work).unwrap();
        let storage = home.join("native-journal-v1");
        fs::create_dir(&storage).unwrap();
        let record = storage.join("records.bin");
        fs::write(&record, b"unchanged journal").unwrap();
        symlink(&home, temp.path().join("home-alias")).unwrap();
        symlink(&storage, work.join("storage-alias")).unwrap();
        symlink(&record, work.join("leaf-alias")).unwrap();
        with_test_storage(&temp.path().join("home-alias"), || {
            assert!(validate_mutation_target(&record).is_err());
            assert!(validate_mutation_target(&work.join("storage-alias/new.txt")).is_err());
            assert!(validate_mutation_target(&work.join("leaf-alias")).is_err());
            assert!(validate_mutation_target(&work.join("ordinary.txt")).is_ok());
            assert_eq!(fs::read(&record).unwrap(), b"unchanged journal");
            assert!(!storage.join("storage.lock").exists());
        });
    }

    struct TestDir {
        root: PathBuf,
        workspace: PathBuf,
        home: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "aizen-native-journal-{}-{}",
                std::process::id(),
                persist::unique_sequence()
            ));
            fs::create_dir(&root).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let workspace = root.join("work");
            let home = root.join("home");
            fs::create_dir(&workspace).unwrap();
            fs::create_dir(&home).unwrap();
            Self {
                root,
                workspace,
                home,
            }
        }

        fn resource(&self, name: &str) -> NativeResource {
            NativeResource::resolve(&self.workspace.join(name)).unwrap()
        }

        fn lease(&self, resources: &[&NativeResource]) -> NativeResourceLease {
            let requests: Vec<_> = resources
                .iter()
                .map(|r| ((*r).clone(), LockMode::Exclusive))
                .collect();
            NativeResourceLease::acquire(
                &self.workspace,
                &requests,
                Duration::from_secs(1),
                None,
                "journal-fixture",
            )
            .unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(f: impl FnOnce(&TestDir)) {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &dir.home)]);
        struct ResetFault;
        impl Drop for ResetFault {
            fn drop(&mut self) {
                FAULT.with(|slot| slot.set(None));
            }
        }
        let _fault = ResetFault;
        f(&dir);
    }

    fn owner(scope: &str) -> ExecutionContext {
        ExecutionContext::default()
            .without_workspace_restore()
            .with_resource_scope(scope)
    }

    fn write(
        resource: &NativeResource,
        expected: &FileFingerprint,
        bytes: &[u8],
        scope: &str,
    ) -> Result<FileFingerprint> {
        exec_ctx::with_current(owner(scope), || apply(resource, expected, bytes, scope))
    }

    fn undo(resource: &NativeResource, selected: &str) -> Result<usize> {
        let parent = ExecutionContext::default().with_resource_scope("parent");
        exec_ctx::with_current(parent.clone(), || undo_owner(&parent, resource, selected))
    }

    fn states() -> Vec<(u64, State, Option<Outcome>)> {
        Storage::open()
            .unwrap()
            .load()
            .unwrap()
            .records
            .iter()
            .map(|r| (r.meta.id, r.meta.state, r.meta.outcome))
            .collect()
    }

    // New batch fixtures isolate both journal storage and advisory locks per thread. Unlike older
    // single-write fixtures above, they do not modify the process environment or production home.
    fn batch_fixture(f: impl FnOnce(&TestDir)) {
        let dir = TestDir::new();
        crate::core::workspace_txn::with_test_lock_home(&dir.home, || {
            with_test_storage(&dir.home, || {
                exec_ctx::with_current(owner("parent/batch"), || f(&dir))
            })
        });
    }

    fn proof_claim<'a>(
        id: u64,
        expected_owner: &'a str,
        resource: &'a NativeResource,
        before: &'a FileFingerprint,
        after: &'a FileFingerprint,
    ) -> RecordClaim<'a> {
        RecordClaim { id, expected_owner, resource, before, after }
    }

    fn proof_fixture(f: impl FnOnce(&TestDir)) {
        let dir = TestDir::new();
        crate::core::workspace_txn::with_test_lock_home(&dir.home, || {
            with_test_storage(&dir.home, || f(&dir))
        });
    }

    #[test]
    fn proof_exact_record_after_unrelated_write_without_execution_context() {
        proof_fixture(|dir| {
            let unrelated = dir.resource("unrelated.txt");
            let selected = dir.resource("selected.txt");
            let _lease = dir.lease(&[&unrelated, &selected]);
            let before = FileFingerprint::missing();
            let after = FileFingerprint::for_bytes(b"selected");
            write(&unrelated, &before, b"unrelated", "proof-owner").unwrap();
            write(&selected, &before, b"selected", "proof-owner").unwrap();
            assert!(exec_ctx::current().is_none());
            let path = storage_directory().unwrap().join("records.bin");
            let bytes = fs::read(&path).unwrap();
            // A foreign source edit does not change what this historical proof says. Inspection
            // must not read source bytes or confuse history with the current image.
            fs::write(selected.path(), b"foreign").unwrap();
            let proofs = inspect_existing_records(&[
                proof_claim(2, "proof-owner", &selected, &before, &after),
            ]).unwrap();
            assert_eq!(proofs, vec![RecordProof { id: 2, status: RecordProofStatus::SettledApplied }]);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read(selected.path()).unwrap(), b"foreign");
        });
    }

    #[test]
    fn proof_rejects_wrong_owner_id_resource_images_and_duplicate_claims() {
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            let other = dir.resource("other.txt");
            let _lease = dir.lease(&[&selected, &other]);
            let before = FileFingerprint::missing();
            let after = FileFingerprint::for_bytes(b"selected");
            let wrong = FileFingerprint::for_bytes(b"wrong");
            write(&selected, &before, b"selected", "proof-owner").unwrap();
            for claim in [
                proof_claim(0, "proof-owner", &selected, &before, &after),
                proof_claim(2, "proof-owner", &selected, &before, &after),
                proof_claim(1, "another-owner", &selected, &before, &after),
                proof_claim(1, "proof-owner", &other, &before, &after),
                proof_claim(1, "proof-owner", &selected, &wrong, &after),
                proof_claim(1, "proof-owner", &selected, &before, &wrong),
            ] {
                let error = inspect_existing_records(&[claim]).unwrap_err();
                assert_eq!(error.to_string(), "native journal proof unavailable or exact claim mismatch");
            }
            assert!(inspect_existing_records(&[
                proof_claim(1, "proof-owner", &selected, &before, &after),
                proof_claim(1, "proof-owner", &selected, &before, &after),
            ]).is_err());
            let oversized: Vec<_> = (1..=MAX_RECORDS + 1)
                .map(|id| proof_claim(id as u64, "proof-owner", &selected, &before, &after)).collect();
            assert!(inspect_existing_records(&oversized).is_err());
            assert_eq!(fs::read(selected.path()).unwrap(), b"selected");
        });
    }

    #[test]
    fn proof_rejects_undo_record_even_with_matching_owner_and_images() {
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            fs::write(selected.path(), b"base").unwrap();
            let _lease = dir.lease(&[&selected]);
            let base = FileFingerprint::for_bytes(b"base");
            let after = write(&selected, &base, b"selected", "proof-owner").unwrap();
            assert_eq!(undo(&selected, "proof-owner").unwrap(), 1);
            assert!(inspect_existing_records(&[
                proof_claim(2, "parent", &selected, &after, &base),
            ]).is_err());
            assert_eq!(inspect_existing_records(&[
                proof_claim(1, "proof-owner", &selected, &base, &after),
            ]).unwrap()[0].status, RecordProofStatus::SettledApplied);
        });
    }

    #[test]
    fn proof_unresolved_statuses_are_observed_without_reconciliation() {
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            let _lease = dir.lease(&[&selected]);
            let before = FileFingerprint::missing();
            let after = write(&selected, &before, b"selected", "proof-owner").unwrap();
            for (state, outcome, expected) in [
                (State::Prepared, None, RecordProofStatus::Prepared),
                (State::Applied, Some(Outcome::Applied), RecordProofStatus::Applied),
                (State::Settled, Some(Outcome::Applied), RecordProofStatus::SettledApplied),
                (State::Settled, Some(Outcome::NotApplied), RecordProofStatus::SettledNotApplied),
            ] {
                let path = {
                    let storage = Storage::open().unwrap();
                    let mut journal = storage.load().unwrap();
                    journal.records[0].meta.state = state;
                    journal.records[0].meta.outcome = outcome;
                    storage.save(&journal).unwrap();
                    storage.path.clone()
                };
                let bytes = fs::read(&path).unwrap();
                let metadata = fs::metadata(&path).unwrap();
                let proof = inspect_existing_records(&[
                    proof_claim(1, "proof-owner", &selected, &before, &after),
                ]).unwrap();
                assert_eq!(proof[0].status, expected);
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert_eq!(fs::metadata(&path).unwrap().modified().ok(), metadata.modified().ok());
                assert_eq!(fs::read(selected.path()).unwrap(), b"selected");
            }
        });
    }

    #[test]
    fn proof_missing_storage_never_recreates_and_empty_claims_do_not_open() {
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            let before = FileFingerprint::missing();
            let after = FileFingerprint::for_bytes(b"selected");
            let check = || inspect_existing_records(&[
                proof_claim(1, "proof-owner", &selected, &before, &after),
            ]);
            let absent_home = dir.home.join("absent");
            with_test_storage(&absent_home, || {
                assert!(inspect_existing_records(&[]).unwrap().is_empty());
                assert!(check().is_err());
                assert!(!absent_home.exists());
            });
            let dir_path = storage_directory().unwrap();
            assert!(check().is_err());
            assert!(!dir_path.exists());
            fs::create_dir(&dir_path).unwrap();
            assert!(check().is_err());
            assert!(fs::read_dir(&dir_path).unwrap().next().is_none());
            fs::write(dir_path.join("storage.lock"), b"").unwrap();
            assert!(check().is_err());
            assert!(!dir_path.join("records.bin").exists());
            fs::remove_file(dir_path.join("storage.lock")).unwrap();
            fs::write(dir_path.join("records.bin"), Journal::default().encode().unwrap()).unwrap();
            assert!(check().is_err());
            assert!(!dir_path.join("storage.lock").exists());
        });
    }

    #[test]
    fn proof_rejects_historical_parent_identity_drift() {
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            let _lease = dir.lease(&[&selected]);
            let before = FileFingerprint::missing();
            let after = write(&selected, &before, b"selected", "proof-owner").unwrap();
            let path = {
                let storage = Storage::open().unwrap();
                let mut journal = storage.load().unwrap();
                journal.records[0].meta.binding.parent_identity.1 ^= 1;
                storage.save(&journal).unwrap();
                storage.path.clone()
            };
            let bytes = fs::read(&path).unwrap();
            assert!(inspect_existing_records(&[
                proof_claim(1, "proof-owner", &selected, &before, &after),
            ]).is_err());
            assert_eq!(fs::read(path).unwrap(), bytes);
        });
    }

    #[cfg(unix)]
    #[test]
    fn proof_rejects_linked_records_and_storage_directory() {
        use std::os::unix::fs::symlink;
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            let _lease = dir.lease(&[&selected]);
            let before = FileFingerprint::missing();
            let after = write(&selected, &before, b"selected", "proof-owner").unwrap();
            let check = || inspect_existing_records(&[
                proof_claim(1, "proof-owner", &selected, &before, &after),
            ]);
            let directory = storage_directory().unwrap();
            let path = directory.join("records.bin");
            let moved = directory.join("moved-records.bin");
            fs::rename(&path, &moved).unwrap();
            symlink(&moved, &path).unwrap();
            assert!(check().is_err());
            fs::remove_file(&path).unwrap();
            fs::rename(&moved, &path).unwrap();
            let moved_directory = dir.home.join("moved-journal");
            fs::rename(&directory, &moved_directory).unwrap();
            symlink(&moved_directory, &directory).unwrap();
            assert!(check().is_err());
            fs::remove_file(&directory).unwrap();
            fs::rename(&moved_directory, &directory).unwrap();
            assert_eq!(check().unwrap()[0].status, RecordProofStatus::SettledApplied);
        });
    }

    #[test]
    fn proof_busy_corrupt_and_hardlinked_storage_fail_closed() {
        proof_fixture(|dir| {
            let selected = dir.resource("selected.txt");
            let _lease = dir.lease(&[&selected]);
            let before = FileFingerprint::missing();
            let after = write(&selected, &before, b"selected", "proof-owner").unwrap();
            let check = || inspect_existing_records(&[
                proof_claim(1, "proof-owner", &selected, &before, &after),
            ]);
            {
                let storage = Storage::open().unwrap();
                let bytes = fs::read(&storage.path).unwrap();
                assert!(check().is_err());
                assert_eq!(fs::read(&storage.path).unwrap(), bytes);
            }
            let path = storage_directory().unwrap().join("records.bin");
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, b"corrupt").unwrap();
            assert!(check().is_err());
            assert_eq!(fs::read(&path).unwrap(), b"corrupt");
            // A valid selected entry does not excuse corruption elsewhere in the file.
            let mut trailing = bytes.clone();
            trailing.push(0);
            fs::write(&path, &trailing).unwrap();
            assert!(check().is_err());
            fs::write(&path, &bytes).unwrap();
            let alias = storage_directory().unwrap().join("records-alias.bin");
            fs::hard_link(&path, &alias).unwrap();
            assert!(check().is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            fs::remove_file(&alias).unwrap();
            let lock_path = storage_directory().unwrap().join("storage.lock");
            let lock_alias = storage_directory().unwrap().join("lock-alias");
            fs::hard_link(&lock_path, &lock_alias).unwrap();
            assert!(check().is_err());
            fs::remove_file(lock_alias).unwrap();
            assert_eq!(check().unwrap()[0].status, RecordProofStatus::SettledApplied);
        });
    }

    #[test]
    fn batch_cumulative_record_capacity_refuses_before_any_candidate_effect() {
        batch_fixture(|dir| {
            let filler = dir.resource("filler.txt");
            let a = dir.resource("a.txt");
            let b = dir.resource("b.txt");
            let _lease = dir.lease(&[&filler, &a, &b]);
            let mut expected = FileFingerprint::missing();
            for index in 0..MAX_RECORDS - 1 {
                expected = apply(&filler, &expected, &(index as u64).to_le_bytes(), "parent/batch").unwrap();
            }
            let missing = FileFingerprint::missing();
            // Either candidate individually fits, but admitting BOTH must fail before a.txt exists.
            drop(begin_batch("parent/batch", &[(&a, &missing, b"after-a")]).unwrap());
            drop(begin_batch("parent/batch", &[(&b, &missing, b"after-b")]).unwrap());
            assert!(begin_batch("parent/batch", &[
                (&a, &missing, b"after-a"), (&b, &missing, b"after-b"),
            ]).is_err());
            assert!(!a.path().exists());
            assert!(!b.path().exists());
            assert_eq!(persist::fingerprint(filler.path()).unwrap(), expected);
            assert_eq!(states().len(), MAX_RECORDS - 1);
        });
    }

    #[test]
    fn batch_cumulative_image_bytes_refuse_before_any_candidate_effect() {
        batch_fixture(|dir| {
            // Eight individually valid 4 MiB pre/post pairs exceed the 64 MiB encoded journal
            // ceiling together (including future metadata), while the first seven fit.
            let before = vec![b'a'; MAX_SOURCE_BYTES];
            let after = vec![b'b'; MAX_SOURCE_BYTES];
            let expected = FileFingerprint::for_bytes(&before);
            let resources: Vec<_> = (0..8).map(|index| {
                let resource = dir.resource(&format!("large-{index}.txt"));
                fs::write(resource.path(), &before).unwrap();
                resource
            }).collect();
            let resource_refs: Vec<_> = resources.iter().collect();
            let _lease = dir.lease(&resource_refs);
            let writes: Vec<_> = resources.iter().map(|resource| (resource, &expected, after.as_slice())).collect();
            drop(begin_batch("parent/batch", &writes[..7]).unwrap());
            assert!(begin_batch("parent/batch", &writes).is_err());
            for resource in &resources {
                assert_eq!(persist::fingerprint(resource.path()).unwrap(), expected);
            }
            assert!(states().is_empty());
        });
    }

    #[test]
    fn batch_rejects_duplicate_or_late_bad_preimage_and_exact_candidate_substitution() {
        batch_fixture(|dir| {
            let a = dir.resource("a.txt");
            let b = dir.resource("b.txt");
            fs::write(a.path(), b"base-a").unwrap();
            fs::write(b.path(), b"base-b").unwrap();
            let _lease = dir.lease(&[&a, &b]);
            let before_a = FileFingerprint::for_bytes(b"base-a");
            let before_b = FileFingerprint::for_bytes(b"base-b");
            let wrong = FileFingerprint::for_bytes(b"not-b");
            assert!(begin_batch("parent/batch", &[
                (&a, &before_a, b"after-a"), (&a, &before_a, b"after-a"),
            ]).is_err());
            // A bad SECOND entry must not publish intent or mutate the otherwise valid first file.
            assert!(begin_batch("parent/batch", &[
                (&a, &before_a, b"after-a"), (&b, &wrong, b"after-b"),
            ]).is_err());
            assert_eq!(fs::read(a.path()).unwrap(), b"base-a");
            assert_eq!(fs::read(b.path()).unwrap(), b"base-b");
            assert!(states().is_empty());
            let mut batch = begin_batch("parent/batch", &[
                (&a, &before_a, b"after-a"), (&b, &before_b, b"after-b"),
            ]).unwrap();
            assert!(batch.apply(&b, &before_b, b"after-b").is_err(), "order is authority");
            assert!(batch.apply(&a, &wrong, b"after-a").is_err(), "preimage is authority");
            assert!(batch.apply(&a, &before_a, b"substituted").is_err(), "candidate bytes are authority");
            assert!(batch.journal.records.is_empty());
            assert_eq!(fs::read(a.path()).unwrap(), b"base-a");
            assert_eq!(fs::read(b.path()).unwrap(), b"base-b");
        });
    }

    #[test]
    fn batch_applies_exact_order_with_noop_and_absent_empty_creation() {
        batch_fixture(|dir| {
            let a = dir.resource("a.txt");
            let unchanged = dir.resource("unchanged.txt");
            let empty = dir.resource("empty.txt");
            let b = dir.resource("b.txt");
            fs::write(a.path(), b"before-a").unwrap();
            fs::write(unchanged.path(), b"same").unwrap();
            fs::write(b.path(), b"before-b").unwrap();
            let _lease = dir.lease(&[&a, &unchanged, &empty, &b]);
            let before_a = FileFingerprint::for_bytes(b"before-a");
            let same = FileFingerprint::for_bytes(b"same");
            let missing = FileFingerprint::missing();
            let before_b = FileFingerprint::for_bytes(b"before-b");
            let changes = std::sync::Arc::new(crate::core::exec_ctx::WorkspaceChanges::default());
            let ctx = owner("parent/batch").with_workspace_changes(changes.clone());
            exec_ctx::with_current(ctx, || {
                let mut batch = begin_batch("parent/batch", &[
                    (&a, &before_a, b"after-a"), (&unchanged, &same, b"same"),
                    (&empty, &missing, b""), (&b, &before_b, b"after-b"),
                ]).unwrap();
                assert!(batch.journal.records.is_empty());
                let first = batch.apply_with_record(&a, &before_a, b"after-a").unwrap();
                assert_eq!(first.fingerprint, FileFingerprint::for_bytes(b"after-a"));
                assert_eq!(first.native_record_id, Some(1));
                let noop = batch.apply_with_record(&unchanged, &same, b"same").unwrap();
                assert_eq!(noop.fingerprint, same);
                assert_eq!(noop.native_record_id, None);
                assert_eq!(batch.journal.records.len(), 1, "no-op never gains undo ownership");
                let created = batch.apply_with_record(&empty, &missing, b"").unwrap();
                assert_eq!(created.fingerprint, FileFingerprint::for_bytes(b""));
                assert_eq!(created.native_record_id, Some(2));
                let last = batch.apply_with_record(&b, &before_b, b"after-b").unwrap();
                assert_eq!(last.fingerprint, FileFingerprint::for_bytes(b"after-b"));
                assert_eq!(last.native_record_id, Some(3));
                let durable = batch.storage.load().unwrap();
                assert_eq!(durable.records.iter().map(|record| record.meta.id).collect::<Vec<_>>(), vec![1, 2, 3]);
                assert!(durable.records.iter().all(|record| record.meta.state == State::Settled
                    && record.meta.outcome == Some(Outcome::Applied)));
                assert!(batch.apply(&b, &before_b, b"after-b").is_err(), "batch cannot be replayed");
                assert_eq!(batch.journal.records.len(), 3);
                assert_eq!(batch.journal.records[0].meta.binding.key, a.key());
                assert_eq!(batch.journal.records[1].meta.binding.key, empty.key());
                assert!(batch.journal.records[1].before.is_none());
                assert_eq!(batch.journal.records[1].after.as_deref(), Some(b"".as_slice()));
                assert_eq!(batch.journal.records[2].meta.binding.key, b.key());
            });
            assert_eq!(fs::read(a.path()).unwrap(), b"after-a");
            assert_eq!(fs::read(unchanged.path()).unwrap(), b"same");
            assert!(empty.path().is_file());
            assert!(fs::read(empty.path()).unwrap().is_empty());
            assert_eq!(fs::read(b.path()).unwrap(), b"after-b");
            assert_eq!(changes.paths(), vec![a.path().to_path_buf(), empty.path().to_path_buf(), b.path().to_path_buf()]);
            assert_eq!(states(), vec![
                (1, State::Settled, Some(Outcome::Applied)),
                (2, State::Settled, Some(Outcome::Applied)),
                (3, State::Settled, Some(Outcome::Applied)),
            ]);
        });
    }

    #[test]
    fn batch_post_admission_drift_consumes_remaining_authority_without_rollback() {
        batch_fixture(|dir| {
            let a = dir.resource("a.txt");
            let b = dir.resource("b.txt");
            fs::write(a.path(), b"base-a").unwrap();
            fs::write(b.path(), b"base-b").unwrap();
            let _lease = dir.lease(&[&a, &b]);
            let before_a = FileFingerprint::for_bytes(b"base-a");
            let before_b = FileFingerprint::for_bytes(b"base-b");
            let mut batch = begin_batch("parent/batch", &[
                (&a, &before_a, b"after-a"), (&b, &before_b, b"after-b"),
            ]).unwrap();
            batch.apply(&a, &before_a, b"after-a").unwrap();
            fs::write(b.path(), b"external").unwrap();
            let error = batch.apply(&b, &before_b, b"after-b").unwrap_err();
            assert!(error.downcast_ref::<PossiblyApplied>().is_none());
            assert_eq!(batch.next, batch.entries.len());
            // Restoring matching bytes cannot refresh the already-consumed capability.
            fs::write(b.path(), b"base-b").unwrap();
            assert!(batch.apply(&b, &before_b, b"after-b").is_err());
            assert_eq!(fs::read(a.path()).unwrap(), b"after-a");
            assert_eq!(fs::read(b.path()).unwrap(), b"base-b");
            drop(batch);
            assert_eq!(states(), vec![(1, State::Settled, Some(Outcome::Applied))]);
        });
    }

    #[test]
    fn batch_possibly_applied_failure_consumes_batch_and_preserves_both_evidence_and_bytes() {
        batch_fixture(|dir| {
            let a = dir.resource("a.txt");
            let b = dir.resource("b.txt");
            let c = dir.resource("c.txt");
            let _lease = dir.lease(&[&a, &b, &c]);
            let missing = FileFingerprint::missing();
            let mut batch = begin_batch("parent/batch", &[
                (&a, &missing, b"after-a"), (&b, &missing, b"after-b"), (&c, &missing, b"after-c"),
            ]).unwrap();
            batch.apply(&a, &missing, b"after-a").unwrap();
            let error = with_test_after_write_failure(|| batch.apply(&b, &missing, b"after-b")).unwrap_err();
            assert_eq!(error.downcast_ref::<PossiblyApplied>().unwrap().record_id, 2);
            assert_eq!(batch.next, batch.entries.len());
            assert!(batch.apply(&b, &missing, b"after-b").is_err(), "uncertain entry cannot retry");
            assert!(batch.apply(&c, &missing, b"after-c").is_err(), "later entry cannot continue");
            assert_eq!(batch.journal.records.len(), 2);
            assert_eq!(batch.journal.records[0].meta.state, State::Settled);
            assert_eq!(batch.journal.records[1].meta.state, State::Prepared);
            drop(batch);
            assert_eq!(fs::read(a.path()).unwrap(), b"after-a");
            assert_eq!(fs::read(b.path()).unwrap(), b"after-b");
            assert!(!c.path().exists());
            assert_eq!(states(), vec![(1, State::Settled, Some(Outcome::Applied)), (2, State::Prepared, None)]);
        });
    }

    #[test]
    fn batch_prepared_failure_preserves_exact_pending_id_without_mutating_failed_entry() {
        batch_fixture(|dir| {
            let a = dir.resource("a.txt");
            let b = dir.resource("b.txt");
            let c = dir.resource("c.txt");
            let _lease = dir.lease(&[&a, &b, &c]);
            let missing = FileFingerprint::missing();
            let mut batch = begin_batch("parent/batch", &[
                (&a, &missing, b"after-a"), (&b, &missing, b"after-b"), (&c, &missing, b"after-c"),
            ]).unwrap();
            assert_eq!(batch.apply_with_record(&a, &missing, b"after-a").unwrap().native_record_id, Some(1));
            let error = with_test_prepared_failure(|| batch.apply(&b, &missing, b"after-b")).unwrap_err();
            assert_eq!(error.downcast_ref::<PossiblyApplied>().unwrap().record_id, 2);
            assert_eq!(batch.next, batch.entries.len());
            assert!(batch.apply(&b, &missing, b"after-b").is_err());
            assert!(batch.apply(&c, &missing, b"after-c").is_err());
            drop(batch);
            assert_eq!(fs::read(a.path()).unwrap(), b"after-a");
            assert!(!b.path().exists());
            assert!(!c.path().exists());
            let path = storage_directory().unwrap().join("records.bin");
            let before = fs::read(&path).unwrap();
            assert_eq!(inspect_existing_records(&[
                proof_claim(1, "parent/batch", &a, &missing, &FileFingerprint::for_bytes(b"after-a")),
                proof_claim(2, "parent/batch", &b, &missing, &FileFingerprint::for_bytes(b"after-b")),
            ]).unwrap(), vec![
                RecordProof { id: 1, status: RecordProofStatus::SettledApplied },
                RecordProof { id: 2, status: RecordProofStatus::Prepared },
            ]);
            assert_eq!(fs::read(path).unwrap(), before, "inspection cannot settle the pending intent");
            assert_eq!(states(), vec![(1, State::Settled, Some(Outcome::Applied)), (2, State::Prepared, None)]);
        });
    }

    #[test]
    fn batch_first_save_errors_preserve_assigned_id_without_claiming_durability() {
        for point in [Fault::SaveBeforePublish, Fault::SaveAfterPublish] {
            batch_fixture(|dir| {
                let resource = dir.resource("edit.txt");
                fs::write(resource.path(), b"before").unwrap();
                let _lease = dir.lease(&[&resource]);
                let before = FileFingerprint::for_bytes(b"before");
                let after = FileFingerprint::for_bytes(b"after");
                let mut batch = begin_batch("parent/batch", &[(&resource, &before, b"after")]).unwrap();
                let error = with_test_fault(point, || batch.apply(&resource, &before, b"after")).unwrap_err();
                assert_eq!(error.downcast_ref::<PossiblyApplied>().unwrap().record_id, 1);
                assert!(batch.apply(&resource, &before, b"after").is_err());
                drop(batch);
                assert_eq!(fs::read(resource.path()).unwrap(), b"before");
                let path = storage_directory().unwrap().join("records.bin");
                if point == Fault::SaveAfterPublish {
                    let bytes = fs::read(&path).unwrap();
                    assert_eq!(inspect_existing_records(&[
                        proof_claim(1, "parent/batch", &resource, &before, &after),
                    ]).unwrap(), vec![RecordProof { id: 1, status: RecordProofStatus::Prepared }]);
                    assert_eq!(fs::read(&path).unwrap(), bytes);
                    assert_eq!(states(), vec![(1, State::Prepared, None)]);
                } else {
                    assert!(!path.exists());
                    assert!(inspect_existing_records(&[
                        proof_claim(1, "parent/batch", &resource, &before, &after),
                    ]).is_err());
                    assert!(!path.exists(), "read-only proof cannot recreate absent intent");
                }
            });
        }
    }

    #[test]
    fn batch_owner_transplant_is_refused_at_admission_and_application() {
        batch_fixture(|dir| {
            let resource = dir.resource("owned.txt");
            let _lease = dir.lease(&[&resource]);
            let missing = FileFingerprint::missing();
            assert!(begin_batch("parent/batch/child", &[(&resource, &missing, b"after")]).is_err());
            let mut batch = begin_batch("parent/batch", &[(&resource, &missing, b"after")]).unwrap();
            exec_ctx::with_current(owner("parent/batch/sibling"), || {
                assert!(batch.apply(&resource, &missing, b"after").is_err());
            });
            assert!(!resource.path().exists());
            assert!(batch.journal.records.is_empty());
            drop(batch);
            assert!(states().is_empty());
        });
    }

    #[test]
    fn durable_settlement_precedes_success_and_restarts_with_exact_binding() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            let result = write(
                &resource,
                &FileFingerprint::missing(),
                b"after",
                "parent/task/a",
            )
            .unwrap();
            assert_eq!(result, FileFingerprint::for_bytes(b"after"));
            assert_eq!(states(), vec![(1, State::Settled, Some(Outcome::Applied))]);
            let journal = Storage::open().unwrap().load().unwrap();
            assert_eq!(journal.records[0].meta.owner, "parent/task/a");
            assert_eq!(journal.records[0].meta.binding.path, resource.path());
            assert_eq!(journal.records[0].meta.binding.key, resource.key());
            assert!(journal.records[0].before.is_none());
            assert_eq!(
                journal.records[0].after.as_deref(),
                Some(b"after".as_slice())
            );
            assert!(reconcile(&resource).unwrap().is_empty());
        });
    }

    #[test]
    fn failure_before_prepared_and_at_prepared_never_writes_destination() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"before").unwrap();
            let before = FileFingerprint::for_bytes(b"before");
            FAULT.with(|slot| slot.set(Some(Fault::BeforePrepared)));
            let before_error = write(&resource, &before, b"after", "a").unwrap_err();
            assert!(before_error.downcast_ref::<PossiblyApplied>().is_none());
            assert!(states().is_empty());
            FAULT.with(|slot| slot.set(Some(Fault::Prepared)));
            let error = write(&resource, &before, b"after", "a").unwrap_err();
            assert_eq!(error.downcast_ref::<PossiblyApplied>().unwrap().record_id, 1);
            assert_eq!(fs::read(resource.path()).unwrap(), b"before");
            assert_eq!(states(), vec![(1, State::Prepared, None)]);
            assert_eq!(
                reconcile(&resource).unwrap(),
                vec![Reconciliation::Before { record_id: 1 }]
            );
            assert_eq!(
                states(),
                vec![(1, State::Settled, Some(Outcome::NotApplied))]
            );
            assert_eq!(undo(&resource, "a").unwrap(), 0);
        });
    }

    #[test]
    fn write_applied_and_completion_faults_are_possibly_applied_and_reconcile_on_restart() {
        for (point, expected_state) in [
            (Fault::Write, State::Prepared),
            (Fault::Applied, State::Applied),
            (Fault::Completion, State::Applied),
        ] {
            fixture(|dir| {
                let resource = dir.resource("edit.txt");
                let _lease = dir.lease(&[&resource]);
                fs::write(resource.path(), b"before").unwrap();
                FAULT.with(|slot| slot.set(Some(point)));
                let error = write(
                    &resource,
                    &FileFingerprint::for_bytes(b"before"),
                    b"after",
                    "a",
                )
                .unwrap_err();
                assert_eq!(
                    error.downcast_ref::<PossiblyApplied>().unwrap().record_id,
                    1
                );
                assert_eq!(fs::read(resource.path()).unwrap(), b"after");
                assert_eq!(states()[0].1, expected_state);
                // A completely reopened journal reconstructs the intent; no process memory is used.
                assert_eq!(
                    reconcile(&resource).unwrap(),
                    vec![Reconciliation::After { record_id: 1 }]
                );
                assert_eq!(states(), vec![(1, State::Settled, Some(Outcome::Applied))]);
                assert_eq!(undo(&resource, "a").unwrap(), 1);
                assert_eq!(fs::read(resource.path()).unwrap(), b"before");
            });
        }
    }

    #[test]
    fn unresolved_conflict_is_retained_and_blocks_next_mutation() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"before").unwrap();
            FAULT.with(|slot| slot.set(Some(Fault::Write)));
            write(
                &resource,
                &FileFingerprint::for_bytes(b"before"),
                b"after",
                "a",
            )
            .unwrap_err();
            // Simulate a non-cooperative editor inside this owned fixture.
            fs::write(resource.path(), b"sibling").unwrap();
            assert_eq!(
                reconcile(&resource).unwrap(),
                vec![Reconciliation::Conflict { record_id: 1 }]
            );
            assert_eq!(states(), vec![(1, State::Prepared, None)]);
            assert!(write(
                &resource,
                &FileFingerprint::for_bytes(b"sibling"),
                b"next",
                "a"
            )
            .is_err());
            assert!(undo(&resource, "a").is_err());
            assert_eq!(fs::read(resource.path()).unwrap(), b"sibling");
            assert_eq!(states().len(), 1);
        });
    }

    #[test]
    fn applied_record_reverted_externally_is_conflict_not_cancelled_intent() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"before").unwrap();
            FAULT.with(|slot| slot.set(Some(Fault::Applied)));
            write(
                &resource,
                &FileFingerprint::for_bytes(b"before"),
                b"after",
                "a",
            )
            .unwrap_err();
            fs::write(resource.path(), b"before").unwrap();
            assert_eq!(
                reconcile(&resource).unwrap(),
                vec![Reconciliation::Conflict { record_id: 1 }]
            );
            assert_eq!(states()[0].1, State::Applied);
        });
    }

    #[test]
    fn repeated_writes_reverse_guarded_chain_and_do_not_repeat_undo() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"base").unwrap();
            let first =
                write(&resource, &FileFingerprint::for_bytes(b"base"), b"one", "a").unwrap();
            write(&resource, &first, b"two", "a").unwrap();
            assert_eq!(undo(&resource, "a").unwrap(), 2);
            assert_eq!(fs::read(resource.path()).unwrap(), b"base");
            assert_eq!(undo(&resource, "a").unwrap(), 0);
            let journal = Storage::open().unwrap().load().unwrap();
            assert_eq!(journal.records.len(), 4);
            assert!(matches!(
                journal.records[2].meta.operation,
                Operation::Undo { target: 2 }
            ));
            assert!(matches!(
                journal.records[3].meta.operation,
                Operation::Undo { target: 1 }
            ));
            assert!(journal.active_stack(resource.key()).unwrap().is_empty());
        });
    }

    #[test]
    fn undo_restart_reconciles_its_own_intent_without_misattributing_restore_to_child() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"base").unwrap();
            let first =
                write(&resource, &FileFingerprint::for_bytes(b"base"), b"one", "a").unwrap();
            write(&resource, &first, b"two", "a").unwrap();
            FAULT.with(|slot| slot.set(Some(Fault::Write)));
            assert_eq!(
                undo(&resource, "a")
                    .unwrap_err()
                    .downcast_ref::<PossiblyApplied>()
                    .unwrap()
                    .record_id,
                3
            );
            assert_eq!(fs::read(resource.path()).unwrap(), b"one");
            assert_eq!(
                reconcile(&resource).unwrap(),
                vec![Reconciliation::After { record_id: 3 }]
            );
            let journal = Storage::open().unwrap().load().unwrap();
            assert_eq!(journal.records[2].meta.owner, "parent");
            assert!(matches!(
                journal.records[2].meta.operation,
                Operation::Undo { target: 2 }
            ));
            assert_eq!(undo(&resource, "a").unwrap(), 1);
            assert_eq!(fs::read(resource.path()).unwrap(), b"base");
        });
    }

    #[test]
    fn selected_owner_undo_preserves_disjoint_sibling_and_refuses_same_resource_sibling_tip() {
        fixture(|dir| {
            let a = dir.resource("a.txt");
            let b = dir.resource("b.txt");
            let _lease = dir.lease(&[&a, &b]);
            fs::write(a.path(), b"base-a").unwrap();
            fs::write(b.path(), b"base-b").unwrap();
            write(&a, &FileFingerprint::for_bytes(b"base-a"), b"one", "a").unwrap();
            write(&b, &FileFingerprint::for_bytes(b"base-b"), b"sibling", "b").unwrap();
            assert_eq!(undo(&a, "a").unwrap(), 1);
            assert_eq!(fs::read(b.path()).unwrap(), b"sibling");
            let next = write(&a, &FileFingerprint::for_bytes(b"base-a"), b"one", "a").unwrap();
            let next = write(&a, &next, b"two", "b").unwrap();
            // Restore coincident bytes in a NEW sibling mutation. Hash equality is NOT attribution.
            write(&a, &next, b"one", "b").unwrap();
            assert!(undo(&a, "a").is_err());
            assert_eq!(fs::read(a.path()).unwrap(), b"one");
        });
    }

    #[test]
    fn owner_and_undo_authority_are_exact_and_ambient_child_cannot_borrow_parent() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"base").unwrap();
            let base = FileFingerprint::for_bytes(b"base");
            assert!(apply(&resource, &base, b"one", "a").is_err());
            exec_ctx::with_current(owner("a"), || {
                assert!(apply(&resource, &base, b"one", "a/task/b").is_err());
            });
            write(&resource, &base, b"one", "a").unwrap();
            let parent = ExecutionContext::default().with_resource_scope("parent");
            let child = parent
                .without_workspace_restore()
                .with_resource_scope("child");
            assert!(undo_owner(&parent, &resource, "a").is_err());
            exec_ctx::with_current(child.clone(), || {
                assert!(undo_owner(&child, &resource, "a").is_err());
                assert!(undo_owner(&parent, &resource, "a").is_err());
            });
            assert_eq!(fs::read(resource.path()).unwrap(), b"one");
            assert_eq!(undo(&resource, "a").unwrap(), 1);
        });
    }

    #[test]
    fn no_op_and_stale_expected_have_no_ownership_or_journal_effect() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"same").unwrap();
            let same = FileFingerprint::for_bytes(b"same");
            assert_eq!(write(&resource, &same, b"same", "a").unwrap(), same);
            assert!(write(&resource, &FileFingerprint::missing(), b"new", "b").is_err());
            assert!(states().is_empty());
        });
    }

    #[test]
    fn oversized_before_and_after_refuse_before_record_or_mutation() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            let large = vec![0u8; MAX_SOURCE_BYTES + 1];
            assert!(write(&resource, &FileFingerprint::missing(), &large, "a").is_err());
            assert!(!resource.path().exists());
            fs::write(resource.path(), &large).unwrap();
            assert!(write(
                &resource,
                &FileFingerprint::for_bytes(&large),
                b"small",
                "a"
            )
            .is_err());
            assert!(states().is_empty());
        });
    }

    #[test]
    fn record_cap_reserves_before_mutation_and_never_prunes_unresolved() {
        fixture(|dir| {
            let resource = dir.resource("edit.txt");
            let _lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"base").unwrap();
            let mut expected = FileFingerprint::for_bytes(b"base");
            for i in 0..MAX_RECORDS {
                let bytes = (i as u64).to_le_bytes();
                expected = write(&resource, &expected, &bytes, "a").unwrap();
            }
            let previous = fs::read(resource.path()).unwrap();
            let error = write(&resource, &expected, b"over-cap", "a").unwrap_err();
            assert!(error.to_string().contains("full"));
            assert_eq!(fs::read(resource.path()).unwrap(), previous);
            assert_eq!(states().len(), MAX_RECORDS);
            assert!(undo(&resource, "a")
                .unwrap_err()
                .to_string()
                .contains("full"));
            // Capacity accounting includes both images and future metadata transitions without allocation.
            let mut journal = Journal::default();
            journal.records.push(Record {
                meta: Metadata {
                    id: 1,
                    owner: "a".into(),
                    binding: binding(&resource).unwrap(),
                    operation: Operation::Write,
                    coverage: None,
                    state: State::Prepared,
                    outcome: None,
                    before: (&FileFingerprint::missing()).into(),
                    after: (&FileFingerprint::for_bytes(b"a")).into(),
                },
                before: None,
                after: Some(b"a".to_vec()),
            });
            assert!(journal.reserve(MAX_SOURCE_BYTES + 1, 1).is_err());
            assert_eq!(journal.records[0].meta.state, State::Prepared);
        });
    }

    #[test]
    fn encoded_byte_capacity_refuses_before_clone_or_serialize() {
        let source = vec![1u8; MAX_SOURCE_BYTES];
        let mut journal = Journal::default();
        let mut records_that_fit = 0;
        while journal.reserve(MAX_SOURCE_BYTES, MAX_SOURCE_BYTES).is_ok() {
            let id = journal.next_id().unwrap();
            journal.records.push(Record {
                meta: Metadata {
                    id,
                    owner: "a".into(),
                    binding: Binding {
                        path: std::env::temp_dir().join("bounded.txt"),
                        key: "key".into(),
                        parent_identity: (0, 0),
                    },
                    operation: Operation::Write,
                    coverage: None,
                    state: State::Prepared,
                    outcome: None,
                    before: (&FileFingerprint::for_bytes(&source)).into(),
                    after: (&FileFingerprint::for_bytes(b"different")).into(),
                },
                // This fixture tests size reservation only; no encode/storage/file effects are attempted.
                before: Some(source.clone()),
                after: Some(source.clone()),
            });
            records_that_fit += 1;
        }
        assert_eq!(records_that_fit, 7);
        assert!(journal.reserved_size().unwrap() < MAX_JOURNAL_BYTES);
        assert!(journal.reserve(MAX_SOURCE_BYTES, MAX_SOURCE_BYTES).is_err());
    }

    #[test]
    fn creation_undo_guard_refuses_newer_content_on_every_platform() {
        fixture(|dir| {
            let resource = dir.resource("created.txt");
            let _lease = dir.lease(&[&resource]);
            write(&resource, &FileFingerprint::missing(), b"mine", "a").unwrap();
            fs::write(resource.path(), b"newer").unwrap();
            assert!(undo(&resource, "a")
                .unwrap_err()
                .to_string()
                .contains("conflict"));
            assert_eq!(fs::read(resource.path()).unwrap(), b"newer");
            assert_eq!(states().len(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn owner_created_file_deletion_is_durable_guarded_and_restart_reconciles_missing_after() {
        fixture(|dir| {
            let resource = dir.resource("created.txt");
            let _lease = dir.lease(&[&resource]);
            let created = write(&resource, &FileFingerprint::missing(), b"one", "a").unwrap();
            write(&resource, &created, b"two", "a").unwrap();
            assert_eq!(undo(&resource, "a").unwrap(), 2);
            assert!(!resource.path().exists());
            assert_eq!(undo(&resource, "a").unwrap(), 0);
            write(&resource, &FileFingerprint::missing(), b"new", "a").unwrap();
            FAULT.with(|slot| slot.set(Some(Fault::Write)));
            undo(&resource, "a").unwrap_err();
            assert!(!resource.path().exists());
            assert_eq!(
                reconcile(&resource).unwrap(),
                vec![Reconciliation::After { record_id: 6 }]
            );
            assert_eq!(undo(&resource, "a").unwrap(), 0);
        });
    }

    #[cfg(windows)]
    #[test]
    fn owner_created_deletion_without_durable_unlink_is_explicitly_unsupported_before_mutation() {
        fixture(|dir| {
            let resource = dir.resource("created.txt");
            let _lease = dir.lease(&[&resource]);
            write(&resource, &FileFingerprint::missing(), b"mine", "a").unwrap();
            let error = undo(&resource, "a").unwrap_err();
            assert!(error.to_string().contains("unsupported"));
            assert_eq!(fs::read(resource.path()).unwrap(), b"mine");
            assert_eq!(states().len(), 1);
        });
    }

    #[test]
    fn decoder_refuses_truncation_trailing_bytes_and_oversized_declared_lengths() {
        let empty = Journal::default().encode().unwrap();
        assert!(Journal::decode(&empty).is_ok());
        assert!(Journal::decode(&empty[..empty.len() - 1]).is_err());
        let mut trailing = empty.clone();
        trailing.push(0);
        assert!(Journal::decode(&trailing).is_err());
        let mut oversized = MAGIC.to_vec();
        put_u32(&mut oversized, (MAX_RECORDS + 1) as u32);
        assert!(Journal::decode(&oversized).is_err());
        let mut metadata = MAGIC.to_vec();
        put_u32(&mut metadata, 1);
        put_u32(&mut metadata, (MAX_META_BYTES + 1) as u32);
        assert!(Journal::decode(&metadata).is_err());
        let declared_image_len = ((MAX_SOURCE_BYTES + 1) as u32).to_le_bytes();
        let mut image = Cursor(&declared_image_len);
        assert!(image.image().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn restart_refuses_replaced_parent_directory_even_with_same_path_and_bytes() {
        fixture(|dir| {
            let nested = dir.workspace.join("nested");
            fs::create_dir(&nested).unwrap();
            let resource = NativeResource::resolve(&nested.join("edit.txt")).unwrap();
            let lease = dir.lease(&[&resource]);
            fs::write(resource.path(), b"base").unwrap();
            FAULT.with(|slot| slot.set(Some(Fault::Write)));
            write(
                &resource,
                &FileFingerprint::for_bytes(b"base"),
                b"after",
                "a",
            )
            .unwrap_err();
            drop(lease);
            fs::rename(&nested, dir.workspace.join("old-nested")).unwrap();
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("edit.txt"), b"after").unwrap();
            let new_resource = NativeResource::resolve(&nested.join("edit.txt")).unwrap();
            assert_eq!(new_resource.key(), resource.key());
            let _lease = dir.lease(&[&new_resource]);
            assert!(reconcile(&new_resource).is_err());
            assert_eq!(states(), vec![(1, State::Prepared, None)]);
            assert_eq!(fs::read(new_resource.path()).unwrap(), b"after");
        });
    }

    #[test]
    fn thread_local_fixture_overrides_restore_on_nested_return_and_panic() {
        let outer = std::env::temp_dir().join("journal-override-outer");
        let inner = std::env::temp_dir().join("journal-override-inner");
        with_test_storage(&outer, || {
            assert_eq!(storage_home(), outer);
            with_test_storage(&inner, || assert_eq!(storage_home(), inner));
            assert_eq!(storage_home(), outer);
            let result = std::panic::catch_unwind(|| {
                with_test_storage(&inner, || panic!("fixture panic"));
            });
            assert!(result.is_err());
            assert_eq!(storage_home(), outer);
        });
        assert!(TEST_STORAGE.with(|slot| slot.borrow().is_none()));
    }

    #[test]
    fn managed_storage_is_never_an_approved_mutation_destination() {
        fixture(|dir| {
            let ordinary = dir.resource("edit.txt");
            let _lease = dir.lease(&[&ordinary]);
            write(&ordinary, &FileFingerprint::missing(), b"one", "a").unwrap();
            let storage = Storage::open().unwrap();
            let path = storage.path.clone();
            drop(storage);
            let journal_resource = NativeResource::resolve(&path).unwrap();
            exec_ctx::with_current(owner("a"), || {
                assert!(apply(
                    &journal_resource,
                    &FileFingerprint::missing(),
                    b"corrupt",
                    "a"
                )
                .is_err());
            });
            assert_eq!(states().len(), 1);
        });
    }
}
