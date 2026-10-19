//! Time Machine — crash-recoverable, git-backed working-tree checkpoints.
//!
//! Checkpoints live in a private store under `~/.aizen/timemachine/<repo-id>/`, fully outside the
//! source repository's `.git`. Each linked worktree owns its own ledger/journal/chat namespace while
//! sharing a bare object store. Metadata is fail-closed, writes are atomic, and every mutating
//! operation is serialized by an OS lock. Git is invoked with hooks/fsmonitor/filters disabled so
//! checkpointing cannot execute repository-controlled code before an approval gate.
//!
//! # Relationship to the source repo — restore-tree always works; full independence is best-effort
//!
//! New checkpoint objects (the checkpoint commit, its tree, and every blob that CHANGED) and all Time
//! Machine refs are written into the private store, so restoring a checkpoint's working tree does not
//! depend on the source repo staying alive. It is NOT a fully self-contained clone, though: the store
//! keeps a sealed `objects/info/alternates` pointer back to the source `.git/objects`, and Git's
//! alternates mechanism means an UNCHANGED blob (one already present in a source commit) may never be
//! copied into the private store — the checkpoint tree references it only through the source. The
//! first checkpoint of a timeline also parents on the source `HEAD` commit, which is not materialized.
//! Consequences if the source `.git` is later deleted: `read-tree`/restore still succeeds for the
//! materialized trees, but history walks (`git log`) and `git fsck` over the store can report missing
//! objects, and any blob that lived only in the source is gone. A future `aizen time seal` (copy the
//! full object closure, drop alternates) would make a store archival-independent; today it is not.

use crate::core::types::Message;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const DEFAULT_KEEP: usize = 50;
/// How many low-value SAFETY-NET checkpoints (`before agent edits`) to retain, independent of the
/// `DEFAULT_KEEP` total budget. These are per-run recovery nets: one is stamped before the first
/// destructive edit of every agent run, so they pile up fast (81 on this machine's aizen store) and
/// bury the descriptive `phase: …` / `phase N verified` MILESTONES the user actually browses to.
/// Retention drops the oldest safety-nets past this floor FIRST, so the timeline stays readable
/// without ever touching a milestone until the total budget itself is exceeded. Override with
/// `timemachine_keep_safety` in config.
const DEFAULT_KEEP_SAFETY: usize = 5;
/// How many LOOSE objects may pile up in a store before a save pays for a compaction pass.
///
/// Every checkpoint writes its commit, its tree, and one blob per changed file as separate loose
/// files, and nothing in git's own housekeeping will ever pack them here — see
/// [`RepoContext::compact_objects`] for why `gc` cannot run against this store. Left alone the pile
/// grows without bound: 14,868 loose objects for 30 live checkpoints on the author's machine.
///
/// The threshold trades a rare ~1 s pause against unbounded growth. Below it a store costs a few MB,
/// so compacting would spend more time than it saves.
const LOOSE_COMPACT_THRESHOLD: usize = 2048;
/// Label of the once-per-run pre-edit auto-checkpoint. Shared with the agent loop's call site so the
/// SafetyNet classification keys off a single source of truth, not a copy-pasted string literal.
pub(crate) const PRE_EDIT_LABEL: &str = "before agent edits";
const LEDGER_SCHEMA: u32 = 3;
const MAX_SPECULATION_REGISTRY_BYTES: u64 = 2 * 1024 * 1024;
// Restart inspection is bounded independently of how much stale metadata a user has accumulated.
const MAX_SPECULATION_COHORTS: usize = 32;
const MAX_SPECULATION_WORKTREES: usize = 128;
const MAX_SPECULATION_SCAN_ENTRIES: usize = 4096;
const MAX_SPECULATION_INSPECTION_BYTES: u64 = 64 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
/// Wall-clock ceiling on ONE internal git invocation.
///
/// Every mutating Time Machine op holds `transaction.lock` (and a shared `store.lock`) across its git
/// spawns — see `save_in`. An OS advisory lock lives on the open FILE HANDLE and is released only by
/// `Drop`, so a git child that never reaches pipe EOF parks the thread, the guard never drops, and the
/// byte range stays locked for the life of the process. The observable result was every subsequent
/// edit failing after exactly `LOCK_TIMEOUT` with "resource is busy … transaction.lock", while the
/// lock file appeared absent on disk and no other process was running — deleting the file could not
/// help, because the lock was never in the file. Bounding the spawn converts that permanent strand
/// into an ordinary `Err` that unwinds through `Drop` and frees the lock.
///
/// Sized generously: `add -A` + `write-tree` over a large worktree is legitimately slow. The point is
/// that it is FINITE and well under no user's patience, not that it is short. Override with
/// `AIZEN_GIT_OP_TIMEOUT_SECS`.
const GIT_OP_TIMEOUT: Duration = Duration::from_secs(120);
/// Grace for draining a git child's pipes after it exits (or is killed at the deadline).
const GIT_DRAIN_GRACE: Duration = Duration::from_secs(5);
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// Max agent-driven rewinds per agent run — enough to abandon a bad approach twice, not thrash.
const MAX_RUN_REWINDS: u8 = 2;
static TM_INDEX_SEQ: AtomicU64 = AtomicU64::new(0);
static OP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Logical-run recovery. Clones preserve anchors and budget; a new handle starts independent work.
/// Repository namespaces are separate even when their checkpoint numbers happen to be equal.
#[derive(Debug, Clone)]
pub struct RunRecovery(std::sync::Arc<Mutex<RecoveryState>>, bool);

impl Default for RunRecovery {
    fn default() -> Self {
        Self(
            std::sync::Arc::new(Mutex::new(RecoveryState::default())),
            true,
        )
    }
}

const CHILD_RESTORE_DENIED: &str = "delegated children cannot restore or undo workspace state; retain partial changes and report them to the coordinating parent";

fn require_workspace_restore() -> Result<()> {
    if crate::core::exec_ctx::current().is_some_and(|ctx| !ctx.can_restore_workspace()) {
        bail!(CHILD_RESTORE_DENIED);
    }
    Ok(())
}

#[derive(Debug, Default)]
struct RecoveryState {
    repositories: std::collections::HashMap<PathBuf, RecoveryAnchors>,
    rewinds_used: u8,
    rewind_in_flight: bool,
}

#[derive(Debug, Default)]
struct RecoveryAnchors {
    pre_edit: Option<SnapshotHandle>,
    last_good: Option<SnapshotHandle>,
    pre_edit_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoBinding {
    root: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    store_git_dir: PathBuf,
    namespace_dir: PathBuf,
    repo_id: String,
    worktree_id: String,
    ref_prefix: String,
    hooks_dir: PathBuf,
}

/// A checkpoint's identity AND lifetime pin. A bare id is never authority for agent rollback.
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    binding: RepoBinding,
    snapshot: Snapshot,
    restore_authorized: bool,
    _pin: std::sync::Arc<SnapshotPin>,
}

struct SnapshotPin {
    _lock: crate::core::repo_lock::RepoTxnLock,
}

impl std::fmt::Debug for SnapshotPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotPin").finish_non_exhaustive()
    }
}

impl std::ops::Deref for SnapshotHandle {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.snapshot
    }
}

impl SnapshotHandle {
    pub fn root(&self) -> &Path {
        &self.binding.root
    }

    /// The caller supplies the lease it actually holds, rather than claiming ambient coverage.
    pub fn restore_under_lease(
        &self,
        lease: &crate::core::workspace_txn::WorkspaceWriterLease,
    ) -> Result<Snapshot> {
        if !self.restore_authorized {
            bail!(CHILD_RESTORE_DENIED);
        }
        require_workspace_restore()?;
        let ctx = self.binding.context()?;
        let identity = lease.identity();
        if identity.canonical_root != ctx.root
            || identity.canonical_git_dir.as_deref() != Some(ctx.git_dir.as_path())
            || identity.canonical_common_git_dir.as_deref() != Some(ctx.common_git_dir.as_path())
        {
            bail!("checkpoint restore lease does not cover its bound worktree");
        }
        restore_in_reported_checked(&ctx, self.id, Some(self), Some(lease))
            .map(|(snapshot, _)| snapshot)
    }
}

impl RunRecovery {
    /// A denied view shares bookkeeping without revoking the coordinating parent's authority.
    pub fn without_workspace_restore(&self) -> Self {
        Self(self.0.clone(), false)
    }

    pub fn can_restore_workspace(&self) -> bool {
        self.1
    }

    fn require_workspace_restore(&self) -> Result<()> {
        if !self.can_restore_workspace() {
            bail!(CHILD_RESTORE_DENIED);
        }
        require_workspace_restore()
    }

    /// Preserve historical anchors, but do not reuse them as coverage after opaque background work.
    pub(crate) fn defer_background_coverage(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        for anchors in state.repositories.values_mut() {
            anchors.pre_edit_current = false;
        }
    }

    fn publish(&self, mut handle: SnapshotHandle, pre_edit: bool) -> Result<()> {
        handle.restore_authorized &= self.can_restore_workspace();
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?;
        let anchors = state
            .repositories
            .entry(handle.binding.namespace_dir.clone())
            .or_default();
        if pre_edit {
            if !anchors.pre_edit_current {
                anchors.pre_edit = Some(handle.clone());
                anchors.last_good = Some(handle);
                anchors.pre_edit_current = true;
            } else {
                anchors.pre_edit.get_or_insert_with(|| handle.clone());
                anchors.last_good.get_or_insert(handle);
            }
        } else {
            anchors.last_good = Some(handle);
        }
        Ok(())
    }

    fn anchors<'a>(state: &'a RecoveryState, root: &Path) -> Option<&'a RecoveryAnchors> {
        state
            .repositories
            .values()
            .filter(|anchors| {
                anchors
                    .pre_edit
                    .as_ref()
                    .or(anchors.last_good.as_ref())
                    .is_some_and(|handle| root.starts_with(handle.root()))
            })
            .max_by_key(|anchors| {
                anchors
                    .pre_edit
                    .as_ref()
                    .or(anchors.last_good.as_ref())
                    .map(|h| h.root().components().count())
                    .unwrap_or(0)
            })
    }

    pub fn status(&self, root: &Path) -> RecoveryStatus {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let anchors = Self::anchors(&state, &root);
        RecoveryStatus {
            pre_edit: anchors.and_then(|a| a.pre_edit.as_ref()).map(|h| h.id),
            last_good: anchors.and_then(|a| a.last_good.as_ref()).map(|h| h.id),
            rewinds_used: state.rewinds_used,
            rewinds_left: MAX_RUN_REWINDS.saturating_sub(state.rewinds_used),
        }
    }

    pub fn anchor(&self, root: &Path, target: RewindTarget) -> Result<SnapshotHandle> {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?;
        let anchors = Self::anchors(&state, &root);
        anchors
            .and_then(|a| match target {
                RewindTarget::PreEdit => a.pre_edit.as_ref(),
                RewindTarget::LastGood => a.last_good.as_ref().or(a.pre_edit.as_ref()),
            })
            .cloned()
            .map(|mut handle| {
                // Anchors store snapshot data. Only an authorized coordinating view may rebind
                // that data for restore; delegated views and returned handles remain denied.
                handle.restore_authorized = self.can_restore_workspace()
                    && crate::core::exec_ctx::current()
                        .is_none_or(|ctx| ctx.can_restore_workspace());
                handle
            })
            .with_context(|| format!("no {} anchor for this run/worktree", target.as_str()))
    }

    pub fn hint(&self, root: &Path) -> Option<String> {
        self.can_restore_workspace()
            .then(|| recovery_hint_for(self.status(root)))
            .flatten()
    }

    /// Reserve budget before I/O. Even a partially applied failing restore consumes its reservation.
    pub fn rewind(&self, handle: &SnapshotHandle, scope: &str) -> Result<Snapshot> {
        self.require_workspace_restore()?;
        if !handle.restore_authorized {
            bail!(CHILD_RESTORE_DENIED);
        }
        let _background = crate::core::workspace_txn::BackgroundProcessBarrier::acquire(
            crate::core::cancel::current().as_ref(),
        )?;
        let reservation = {
            let mut state = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?;
            if state.rewind_in_flight || state.rewinds_used >= MAX_RUN_REWINDS {
                bail!("run rewind budget exhausted or another rewind is in flight");
            }
            let belongs = state
                .repositories
                .get(&handle.binding.namespace_dir)
                .is_some_and(|a| {
                    [a.pre_edit.as_ref(), a.last_good.as_ref()]
                        .into_iter()
                        .flatten()
                        .any(|h| {
                            h.binding == handle.binding
                                && h.id == handle.id
                                && h.commit == handle.commit
                        })
                });
            if !belongs {
                bail!("checkpoint is not an anchor of this logical run");
            }
            state.rewinds_used += 1;
            state.rewind_in_flight = true;
            RewindReservation(self.clone())
        };
        let lease = crate::core::workspace_txn::WorkspaceWriterLease::acquire_scoped(
            handle.root(),
            LOCK_TIMEOUT,
            crate::core::cancel::current().as_ref(),
            "checkpoint rewind",
            scope,
        )?;
        let snapshot = handle.restore_under_lease(&lease)?;
        self.publish(handle.clone(), false)?;
        drop(reservation);
        Ok(snapshot)
    }
}

struct RewindReservation(RunRecovery);
impl Drop for RewindReservation {
    fn drop(&mut self) {
        self.0
             .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rewind_in_flight = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryStatus {
    pub pre_edit: Option<u32>,
    pub last_good: Option<u32>,
    pub rewinds_used: u8,
    pub rewinds_left: u8,
}

/// Where the agent is allowed to rewind within the current run. Free-form restore stays human/CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindTarget {
    /// Tree as it was before the first destructive edit of this run.
    PreEdit,
    /// Tree after the last successful edit step of this run.
    LastGood,
}

impl RewindTarget {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pre_edit" | "pre-edit" | "start" | "run" => Some(Self::PreEdit),
            "last_good" | "last-good" | "step" | "undo" => Some(Self::LastGood),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreEdit => "pre_edit",
            Self::LastGood => "last_good",
        }
    }
}

fn recovery_hint_for(s: RecoveryStatus) -> Option<String> {
    if s.rewinds_left == 0 {
        return None;
    }
    match (s.pre_edit, s.last_good) {
        (None, None) => None,
        (Some(p), Some(l)) if p == l => Some(format!(
            "If this approach is wrong, call `checkpoint` action=\"rewind\" target=\"pre_edit\" to restore checkpoint #{p} \
             (working tree before this run's edits; {left} rewind left).",
            left = s.rewinds_left
        )),
        (Some(p), Some(l)) => Some(format!(
            "If this approach is wrong, call `checkpoint` action=\"rewind\": target=\"last_good\" → #{l} (last good step) or \
             target=\"pre_edit\" → #{p} (before this run's edits). {left} rewind(s) left this run.",
            left = s.rewinds_left
        )),
        (Some(p), None) => Some(format!(
            "If this approach is wrong, call `checkpoint` action=\"rewind\" target=\"pre_edit\" to restore checkpoint #{p} \
             ({left} rewind left).",
            left = s.rewinds_left
        )),
        (None, Some(l)) => Some(format!(
            "If this approach is wrong, call `checkpoint` action=\"rewind\" target=\"last_good\" to restore checkpoint #{l} \
             ({left} rewind left).",
            left = s.rewinds_left
        )),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Coverage {
    #[serde(default)]
    pub file_count: u64,
    #[serde(default)]
    pub byte_count: u64,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Retention value tier of a checkpoint. The timeline treats these very differently: a MILESTONE is
/// a place the user deliberately browses back to, a SAFETY-NET is a transient per-run recovery net,
/// and a RECOVERY preimage is created implicitly by restore. Retention keeps many milestones but only
/// a few recent safety-nets, so the list does not fill with `before agent edits` noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// Worth keeping: a manual `/checkpoint`, a closed todo phase (`phase: …`), or a clean verify
    /// gate (`phase N verified`). These are the "chỗ cần check" the user navigates to.
    Milestone,
    /// The once-per-run pre-edit net (`before agent edits`). Useful during and just after its own run
    /// (via process-local run anchors); afterward it is mostly timeline clutter, so it is pruned first.
    SafetyNet,
    /// A preimage created by restore/time-travel (`recovery == true`). Restorable but excluded from
    /// redo branch selection.
    Recovery,
}

/// Evidence linking a live checkpoint to a durable speculative integration receipt. These strings
/// never authorize integration, verification, recovery or restore by themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationReceiptLink {
    pub schema_version: u32,
    pub integration_id: String,
    pub receipt_sha256: String,
}

impl IntegrationReceiptLink {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported Time Machine integration receipt linkage schema");
        }
        if self.integration_id.is_empty()
            || self.integration_id.len() > 128
            || !self
                .integration_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || self.integration_id == "."
            || self.integration_id == ".."
            || self.receipt_sha256.len() != 64
            || !self
                .receipt_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("invalid Time Machine integration receipt linkage");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u32,
    pub commit: String,
    pub tree: String,
    pub label: String,
    pub created: String,
    pub auto: bool,
    #[serde(default)]
    pub has_chat: bool,
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub worktree_id: String,
    #[serde(default)]
    pub coverage: Coverage,
    /// Recovery-only preimage created by restore. It remains directly restorable but is excluded from
    /// normal redo branch selection so safety snapshots do not hijack timeline navigation.
    #[serde(default)]
    pub recovery: bool,
    /// Retention tier. `None` on checkpoints written before this field existed (older ledgers); read
    /// through [`Snapshot::kind`], which INFERS the tier from `recovery`/`auto`/`label` so legacy rows
    /// classify correctly without a migration pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SnapshotKind>,
    /// Absent on legacy and ordinary checkpoints. A receipt link is provenance, not a verified claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_receipt: Option<IntegrationReceiptLink>,
}

impl Snapshot {
    /// Effective retention tier. Uses the stored `kind` when present; otherwise infers it from the
    /// legacy signals so pre-`kind` ledgers (and the dead `after agent edit` label from the old
    /// per-edit binary) still sort into the right bucket. `recovery` wins first (it is authoritative),
    /// then the auto pre-edit / dead-per-edit labels are SafetyNet, and everything else is a Milestone.
    pub fn kind(&self) -> SnapshotKind {
        if let Some(k) = self.kind {
            return k;
        }
        if self.recovery {
            SnapshotKind::Recovery
        } else if self.auto && (self.label == PRE_EDIT_LABEL || self.label == "after agent edit") {
            SnapshotKind::SafetyNet
        } else {
            SnapshotKind::Milestone
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default = "ledger_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub snapshots: Vec<Snapshot>,
    /// Legacy cursor index. Retained in the wire schema for migration compatibility; new code uses
    /// `cursor_id` so pruning/reordering cannot silently move the active checkpoint.
    #[serde(default)]
    pub cursor: Option<usize>,
    #[serde(default)]
    pub cursor_id: Option<u32>,
    #[serde(default)]
    pub next_id: u32,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            schema_version: LEDGER_SCHEMA,
            generation: 0,
            snapshots: Vec::new(),
            cursor: None,
            cursor_id: None,
            next_id: 1,
        }
    }
}

fn ledger_schema() -> u32 {
    LEDGER_SCHEMA
}

impl Ledger {
    fn normalize(&mut self) -> Result<()> {
        if self.schema_version > LEDGER_SCHEMA {
            bail!(
                "time-machine ledger schema {} is newer than this binary supports ({LEDGER_SCHEMA})",
                self.schema_version
            );
        }
        if self.cursor_id.is_none() {
            self.cursor_id = self
                .cursor
                .and_then(|i| self.snapshots.get(i))
                .map(|s| s.id);
        }
        self.cursor = self
            .cursor_id
            .and_then(|id| self.snapshots.iter().position(|s| s.id == id));
        self.schema_version = LEDGER_SCHEMA;
        let mut ids = HashSet::new();
        for s in &self.snapshots {
            if s.id == 0 || !ids.insert(s.id) {
                bail!(
                    "time-machine ledger contains duplicate/invalid checkpoint id #{}",
                    s.id
                );
            }
            if let Some(link) = &s.integration_receipt {
                link.validate()?;
            }
            if s.commit.len() != 40 || s.tree.len() != 40 {
                bail!(
                    "time-machine checkpoint #{} contains an invalid Git object id",
                    s.id
                );
            }
        }
        if let Some(id) = self.cursor_id {
            if !ids.contains(&id) {
                bail!("time-machine cursor points at missing checkpoint #{id}");
            }
        }
        let min_next = self
            .snapshots
            .iter()
            .map(|s| s.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        if self.next_id == 0 {
            self.next_id = min_next;
        } else if self.next_id < min_next {
            bail!(
                "time-machine ledger next_id {} is behind existing checkpoint ids (need at least {min_next})",
                self.next_id
            );
        }
        Ok(())
    }

    fn set_cursor(&mut self, id: Option<u32>) {
        self.cursor_id = id;
        self.cursor = id.and_then(|needle| self.snapshots.iter().position(|s| s.id == needle));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalKind {
    Save,
    Restore,
    Prune,
    Clear,
    Doctor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Prepared,
    RefCreated,
    Applying,
    LedgerCommitted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    operation_id: String,
    kind: JournalKind,
    phase: JournalPhase,
    expected_generation: u64,
    #[serde(default)]
    checkpoint_id: Option<u32>,
    #[serde(default)]
    target_id: Option<u32>,
    #[serde(default)]
    preimage_id: Option<u32>,
    #[serde(default)]
    ref_name: Option<String>,
    #[serde(default)]
    new_oid: Option<String>,
    /// Paths a restore removes because the target tree never had them (E5.1) — written before
    /// the update touches the tree, so a crash mid-restore leaves the list for `time doctor`.
    #[serde(default)]
    removed: Vec<String>,
}

impl Journal {
    fn new(kind: JournalKind, generation: u64) -> Self {
        Self {
            schema_version: 1,
            operation_id: format!(
                "{}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_millis(),
                OP_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            kind,
            phase: JournalPhase::Prepared,
            expected_generation: generation,
            checkpoint_id: None,
            target_id: None,
            preimage_id: None,
            ref_name: None,
            new_oid: None,
            removed: Vec::new(),
        }
    }
}

struct RepoContext {
    root: PathBuf,
    /// Source worktree Git dir (`.git` or `.git/worktrees/<name>`). Used only for seed/migration.
    git_dir: PathBuf,
    /// Source common Git dir. Used for alternates + legacy migration.
    common_git_dir: PathBuf,
    /// Private bare store under `~/.aizen/timemachine/<repo_id>/store.git`.
    store_git_dir: PathBuf,
    /// Worktree-scoped metadata (ledger/journal/lock/chat/temp index).
    namespace_dir: PathBuf,
    repo_id: String,
    worktree_id: String,
    ref_prefix: String,
    hooks_dir: PathBuf,
    /// Cached filter safety probe for this process/context.
    filters_checked: std::cell::Cell<bool>,
    /// Cached reparse/nested-repo walk for this process/context. A `RepoContext` is built per public
    /// operation, so this dedupes the 2-3 walks a single save/restore performs (`current_tree` runs
    /// twice in `restore_in_reported`, plus `apply_tree`) without letting a stale result survive the operation.
    reparse_checked: std::cell::Cell<bool>,
    /// Cached ignored-directory set (see `uncovered_dirs`). Same per-operation lifetime as
    /// `reparse_checked`: one `ls-files` probe instead of one per walk.
    skip_dirs: std::cell::RefCell<Option<HashSet<PathBuf>>>,
}

impl RepoBinding {
    fn from_context(ctx: &RepoContext) -> Self {
        Self {
            root: ctx.root.clone(),
            git_dir: ctx.git_dir.clone(),
            common_git_dir: ctx.common_git_dir.clone(),
            store_git_dir: ctx.store_git_dir.clone(),
            namespace_dir: ctx.namespace_dir.clone(),
            repo_id: ctx.repo_id.clone(),
            worktree_id: ctx.worktree_id.clone(),
            ref_prefix: ctx.ref_prefix.clone(),
            hooks_dir: ctx.hooks_dir.clone(),
        }
    }

    fn context(&self) -> Result<RepoContext> {
        let root =
            fs::canonicalize(&self.root).context("bound checkpoint workspace is unavailable")?;
        let git_dir = fs::canonicalize(absolute_git_path(
            &root,
            &raw_git(&root, ["rev-parse", "--git-dir"])?,
        ))?;
        let common = fs::canonicalize(absolute_git_path(
            &root,
            &raw_git(&root, ["rev-parse", "--git-common-dir"])?,
        ))?;
        if root != self.root || git_dir != self.git_dir || common != self.common_git_dir {
            bail!("bound checkpoint repository/worktree identity changed; restore refused");
        }
        Ok(RepoContext {
            root,
            git_dir,
            common_git_dir: common,
            store_git_dir: self.store_git_dir.clone(),
            namespace_dir: self.namespace_dir.clone(),
            repo_id: self.repo_id.clone(),
            worktree_id: self.worktree_id.clone(),
            ref_prefix: self.ref_prefix.clone(),
            hooks_dir: self.hooks_dir.clone(),
            filters_checked: std::cell::Cell::new(false),
            reparse_checked: std::cell::Cell::new(false),
            skip_dirs: std::cell::RefCell::new(None),
        })
    }
}

/// Read-only plan, not permission to create a store or change live bytes. Registry IDs intentionally
/// remain distinct from the path-derived workspace admission IDs. No deserializer creates a binding.
#[derive(Debug, Clone)]
pub(crate) struct PlannedTimeMachineBinding {
    identity: crate::core::workspace_txn::WorkspaceIdentity,
    metadata_root: PathBuf,
    ancestor: crate::core::workspace_txn::NativeResource,
    workspace: crate::core::workspace_txn::NativeResource,
    git_ancestors: Vec<crate::core::workspace_txn::NativeResource>,
    planned_ids: Option<(String, String)>,
}

/// In-process metadata binding only. This is neither restore authority nor evidence of integration.
#[derive(Debug, Clone)]
pub(crate) struct TimeMachineBinding {
    plan: PlannedTimeMachineBinding,
    binding: RepoBinding,
    namespace: crate::core::workspace_txn::NativeResource,
}

/// Plan from an existing canonical workspace root without `RepoContext::discover`, registry writes,
/// directory creation, Git store initialization, checkpoint capture or recovery. Non-Git roots have
/// no Time Machine binding; their native speculation store must use a separate workspace namespace.
pub(crate) fn plan_speculation_binding(
    canonical_root: &Path,
) -> Result<Option<PlannedTimeMachineBinding>> {
    let identity = crate::core::workspace_txn::WorkspaceIdentity::discover(canonical_root)?;
    if identity.canonical_root != canonical_root
        || fs::canonicalize(canonical_root)? != canonical_root
    {
        bail!("Time Machine binding requires the canonical workspace root");
    }
    match (
        &identity.canonical_git_dir,
        &identity.canonical_common_git_dir,
    ) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {}
        _ => bail!("Time Machine binding requires a complete Git workspace identity"),
    }
    PlannedTimeMachineBinding::plan(identity, &crate::core::config::aizen_home()).map(Some)
}

impl PlannedTimeMachineBinding {
    fn plan(identity: crate::core::workspace_txn::WorkspaceIdentity, home: &Path) -> Result<Self> {
        let (metadata_root, ancestor) = plan_metadata_directory(&home.join("timemachine"))?;
        if metadata_root.starts_with(&identity.canonical_root) {
            bail!("speculative Time Machine metadata must be outside the live workspace");
        }
        let workspace = crate::core::workspace_txn::NativeResource::resolve(
            &identity.canonical_root.join(".tm-workspace-boundary"),
        )?;
        let git_ancestors = [
            identity.canonical_git_dir.as_ref(),
            identity.canonical_common_git_dir.as_ref(),
        ]
        .into_iter()
        .map(|path| {
            crate::core::workspace_txn::NativeResource::resolve(
                &path
                    .context("Time Machine binding requires complete Git directories")?
                    .join(".tm-git-boundary"),
            )
        })
        .collect::<Result<Vec<_>>>()?;
        let mut plan = Self {
            identity,
            metadata_root,
            ancestor,
            workspace,
            git_ancestors,
            planned_ids: None,
        };
        plan.planned_ids = plan.registry_ids(&read_speculation_registry(&plan.metadata_root)?)?;
        plan.ancestor.verify()?;
        Ok(plan)
    }

    fn registry_ids(&self, registry: &identity::Registry) -> Result<Option<(String, String)>> {
        let common = self
            .identity
            .canonical_common_git_dir
            .as_ref()
            .context("Time Machine binding has no common Git directory")?;
        let git = self
            .identity
            .canonical_git_dir
            .as_ref()
            .context("Time Machine binding has no worktree Git directory")?;
        let common = common.to_string_lossy().replace('\\', "/");
        let git = git.to_string_lossy().replace('\\', "/");
        Ok(registry
            .repos
            .iter()
            .find(|repo| repo.common_git_dir == common)
            .and_then(|repo| {
                repo.worktrees
                    .iter()
                    .find(|wt| wt.git_dir == git)
                    .map(|wt| (repo.repo_id.clone(), wt.worktree_id.clone()))
            }))
    }

    fn verify_admission(
        &self,
        writer: &crate::core::workspace_txn::WorkspaceWriterLease,
        physical: &crate::core::workspace_txn::PhysicalEffectLease,
    ) -> Result<()> {
        if !physical.is_exclusive() || writer.identity() != &self.identity {
            bail!(
                "Time Machine metadata requires matching writer and exclusive physical admission"
            );
        }
        self.ancestor.verify()?;
        self.workspace.verify()?;
        for ancestor in &self.git_ancestors {
            ancestor.verify()?;
        }
        // Never use the cached identity here: Git init/worktree replacement must invalidate a plan.
        if crate::core::workspace_txn::WorkspaceIdentity::discover(&self.identity.canonical_root)?
            != self.identity
        {
            bail!("Time Machine workspace identity changed after planning");
        }
        Ok(())
    }

    /// Explicit admitted metadata creation. The caller already owns writer admission followed by a
    /// fresh exclusive physical body. This method never acquires/upgrades either live gate, invokes
    /// ordinary save/recovery, initializes `store.git`, or changes live workspace bytes.
    pub(crate) fn admit(
        &self,
        writer: &crate::core::workspace_txn::WorkspaceWriterLease,
        physical: &crate::core::workspace_txn::PhysicalEffectLease,
    ) -> Result<TimeMachineBinding> {
        self.verify_admission(writer, physical)?;
        let root_guard = create_metadata_directory(&self.metadata_root)?;
        self.ancestor.verify()?;
        let registry_lock = self.metadata_root.join("registry.lock");
        let registry_guard = crate::core::workspace_txn::NativeResource::resolve(&registry_lock)?;
        let cancel = crate::core::cancel::current();
        let registry_lock = crate::core::repo_lock::RepoTxnLock::acquire_mode(
            &registry_lock,
            crate::core::repo_lock::LockMode::Exclusive,
            LOCK_TIMEOUT,
            cancel.as_ref(),
        )?;
        registry_guard.verify()?;
        let mut registry = read_speculation_registry(&self.metadata_root)?;
        if self.planned_ids.is_some() && self.registry_ids(&registry)? != self.planned_ids {
            bail!("Time Machine registry mapping changed after planning");
        }
        let common = self
            .identity
            .canonical_common_git_dir
            .as_ref()
            .context("Time Machine binding has no common Git directory")?;
        let git = self
            .identity
            .canonical_git_dir
            .as_ref()
            .context("Time Machine binding has no worktree Git directory")?;
        let common_key = common.to_string_lossy().replace('\\', "/");
        let git_key = git.to_string_lossy().replace('\\', "/");
        let legacy_repo_id = format!("repo-{:016x}", fnv1a64(&common_key));
        let legacy_wt_id = format!("wt-{:016x}", fnv1a64(&git_key));
        let path_exists = |path: &str| Path::new(path).exists();
        let new_id = |prefix: &str| random_id(prefix);
        let resolved = identity::resolve(
            &mut registry,
            &identity::ResolveInput {
                common_git_dir: common_key,
                git_dir: git_key,
                root_commit: root_commit_of(&self.identity.canonical_root),
                legacy_store_exists: self
                    .metadata_root
                    .join(&legacy_repo_id)
                    .join("store.git")
                    .is_dir(),
                legacy_repo_id,
                legacy_wt_id,
                path_exists: &path_exists,
                new_id: &new_id,
            },
        );
        validate_speculation_registry(&registry)?;
        let ids = (resolved.repo_id, resolved.worktree_id);
        if self.registry_ids(&registry)?.as_ref() != Some(&ids) {
            bail!("Time Machine registry does not map the admitted canonical workspace");
        }
        if resolved.changed || registry.schema_version == 0 {
            registry.schema_version = 1;
            let path = self.metadata_root.join("registry.json");
            let guard = crate::core::workspace_txn::NativeResource::resolve(&path)?;
            let bytes = serde_json::to_vec_pretty(&registry)?;
            if bytes.len() as u64 > MAX_SPECULATION_REGISTRY_BYTES {
                bail!("Time Machine registry exceeds safety bound");
            }
            crate::core::persist::atomic_write(&path, &bytes)?;
            guard.verify()?;
        }
        root_guard.verify()?;
        // The registry lock is BEFORE, and never nested inside, store/transaction ownership.
        drop(registry_lock);
        let repo_root = self.metadata_root.join(&ids.0);
        let namespace_dir = repo_root.join("worktrees").join(&ids.1);
        let namespace = create_metadata_directory(&namespace_dir)?;
        root_guard.verify()?;
        let binding = RepoBinding {
            root: self.identity.canonical_root.clone(),
            git_dir: git.clone(),
            common_git_dir: common.clone(),
            store_git_dir: repo_root.join("store.git"),
            hooks_dir: namespace_dir.join("empty-hooks"),
            namespace_dir,
            repo_id: ids.0,
            ref_prefix: format!("refs/ng/tm/{}", ids.1),
            worktree_id: ids.1,
        };
        Ok(TimeMachineBinding {
            plan: self.clone(),
            binding,
            namespace,
        })
    }
}

impl TimeMachineBinding {
    pub(crate) fn namespace_dir(&self) -> &Path {
        &self.binding.namespace_dir
    }

    #[cfg(test)]
    pub(crate) fn repo_id(&self) -> &str {
        &self.binding.repo_id
    }

    #[cfg(test)]
    pub(crate) fn worktree_id(&self) -> &str {
        &self.binding.worktree_id
    }

    /// Select only validated evidence. The callback runs in the ordinary actual TM lock domain;
    /// no recorded source path is resolved, and selection cannot authorize mutation or settlement.
    pub(crate) fn select_speculation_recovery(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        writer: &crate::core::workspace_txn::WorkspaceWriterLease,
        physical: &crate::core::workspace_txn::PhysicalEffectLease,
        cohort: &str,
        prepared: &str,
    ) -> Result<SpeculationRecoverySelection> {
        require_recovery_report_parent(parent)?;
        self.with_transaction_lock(writer, physical, || {
            let directory = recovery_cohort_directory(self.namespace_dir(), cohort)?;
            let (inventory_sha256, files, native_receipt) =
                read_recovery_selection(&directory, cohort, prepared)?;
            let envelope: serde_json::Value = serde_json::from_slice(&read_speculation_evidence(
                &directory.join(prepared),
                2 * 1024 * 1024,
            )?)?;
            let owner = evidence_text(&envelope, "owner")?.to_owned();
            validate_recovery_base_identity(&directory, &self.plan.identity)?;
            if recovery_inventory_digest(&directory)? != inventory_sha256 {
                bail!("restart base evidence changed during identity inspection");
            }
            Ok(SpeculationRecoverySelection {
                identity: writer.identity().clone(),
                namespace: self.namespace_dir().to_path_buf(),
                cohort: cohort.to_owned(),
                prepared: prepared.to_owned(),
                inventory_sha256,
                files,
                native_receipt,
                owner,
            })
        })
    }

    /// Classify a previously selected record using fresh caller-supplied exact resources, NEVER
    /// resources resolved from recorded paths. This report cannot clear the ordinary pending guard.
    pub(crate) fn report_speculation_recovery(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        writer: &crate::core::workspace_txn::WorkspaceWriterLease,
        physical: &crate::core::workspace_txn::PhysicalEffectLease,
        selection: &SpeculationRecoverySelection,
        approved: &[crate::core::workspace_txn::NativeResource],
    ) -> Result<SpeculationRecoveryReport> {
        self.inspect_selected_recovery(parent, writer, physical, selection, approved, false)
    }

    fn inspect_selected_recovery(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        writer: &crate::core::workspace_txn::WorkspaceWriterLease,
        physical: &crate::core::workspace_txn::PhysicalEffectLease,
        selection: &SpeculationRecoverySelection,
        approved: &[crate::core::workspace_txn::NativeResource],
        record_observation: bool,
    ) -> Result<SpeculationRecoveryReport> {
        require_recovery_report_parent(parent)?;
        if selection.identity != *writer.identity() || selection.namespace != self.namespace_dir() {
            bail!("restart report selection belongs to another admitted workspace/namespace");
        }
        self.plan.verify_admission(writer, physical)?;
        // Sorted exact-resource exclusion precedes the actual TM transaction and cohort journal.
        let resources =
            acquire_recovery_resources(&self.plan.identity.canonical_root, selection, approved)?;
        self.with_transaction_lock(writer, physical, || {
            let directory = recovery_cohort_directory(self.namespace_dir(), &selection.cohort)?;
            // Current store inspection takes a shared owner probe, so inspect before taking our
            // exclusive descriptor. Both are ZERO-time attempts; no shared->exclusive upgrade.
            let (revision, _, _) =
                read_recovery_selection(&directory, &selection.cohort, &selection.prepared)?;
            validate_recovery_base_identity(&directory, &self.plan.identity)?;
            if revision != selection.inventory_sha256 {
                bail!("restart report selection is stale; evidence retained");
            }
            let inspect = || -> Result<SpeculationRecoveryReport> {
                if recovery_inventory_digest(&directory)? != selection.inventory_sha256 {
                    bail!("restart evidence changed before exclusive ownership; evidence retained");
                }
                let native_records = inspect_recovery_native_records(selection, &resources)?;
                let observations = classify_locked_recovery_resources(selection, &resources)?;
                if recovery_inventory_digest(&directory)? != selection.inventory_sha256 {
                    bail!("restart evidence changed during classification; evidence retained");
                }
                Ok(SpeculationRecoveryReport {
                    cohort: selection.cohort.clone(),
                    prepared: selection.prepared.clone(),
                    inventory_sha256: selection.inventory_sha256,
                    status: RecoveryReportStatus::InspectionOnly,
                    files: observations,
                    native_records,
                    native_coverage: selection
                        .native_receipt
                        .as_ref()
                        .and_then(|receipt| receipt.coverage.clone()),
                    observation: None,
                })
            };
            if record_observation {
                let mut owner = crate::core::speculation::store::ExistingObservationOwner::acquire(
                    &directory,
                    selection.inventory_sha256,
                    writer,
                    physical,
                )?;
                let mut report = inspect()?;
                let observation = report.nonterminal_observation(parent, selection)?;
                // No cached/deserialized report becomes fresh authority. All exact resources and
                // metadata admission remain held through this final recheck and immutable append.
                for file in &report.files {
                    if recovery_live_fingerprint(resources.resources[file.key.as_str()])?
                        != file.current
                    {
                        bail!("restart source drifted before observation publication");
                    }
                }
                report.observation = Some(owner.append(&observation)?);
                report.status = RecoveryReportStatus::ObservedNonterminal;
                Ok(report)
            } else {
                with_recovery_owner_claim(&directory, inspect)
            }
        })
    }

    /// Same store-shared -> actual `transaction.lock` protocol as `snapshot_admission`'s body.
    /// Live admission MUST precede this call; no upgrade or normal save/recovery happens inside it.
    /// Do not wait for workers, run verification processes, or reenter save/restore in `body`.
    pub(crate) fn with_transaction_lock<T>(
        &self,
        writer: &crate::core::workspace_txn::WorkspaceWriterLease,
        physical: &crate::core::workspace_txn::PhysicalEffectLease,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.plan.verify_admission(writer, physical)?;
        self.namespace.verify()?;
        let ids = self
            .plan
            .registry_ids(&read_speculation_registry(&self.plan.metadata_root)?)?;
        if ids.as_ref()
            != Some(&(
                self.binding.repo_id.clone(),
                self.binding.worktree_id.clone(),
            ))
        {
            bail!("Time Machine registry mapping no longer matches the admitted binding");
        }
        let ctx = self.binding.context()?;
        let store_guard =
            crate::core::workspace_txn::NativeResource::resolve(&ctx.store_lock_path())?;
        let transaction_guard =
            crate::core::workspace_txn::NativeResource::resolve(&ctx.lock_path())?;
        let _store = ctx.store_shared()?;
        let _transaction = ctx.lock()?;
        store_guard.verify()?;
        transaction_guard.verify()?;
        self.namespace.verify()?;
        body()
    }
}

/// Bind the nearest existing non-link parent without creating the missing suffix.
fn plan_metadata_directory(
    path: &Path,
) -> Result<(PathBuf, crate::core::workspace_txn::NativeResource)> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut parent = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(parent) {
            Ok(metadata) => {
                if !metadata.is_dir() || crate::core::file_placement::is_link_or_reparse(&metadata)
                {
                    bail!("Time Machine metadata requires non-link directories");
                }
                let guard = crate::core::workspace_txn::NativeResource::resolve(
                    &parent.join(".tm-boundary"),
                )?;
                let mut canonical = fs::canonicalize(parent)?;
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok((canonical, guard));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = parent
                    .file_name()
                    .context("invalid Time Machine metadata directory")?;
                suffix.push(name.to_os_string());
                parent = parent
                    .parent()
                    .context("Time Machine metadata has no existing parent")?;
            }
            Err(error) => return Err(error).context("checking Time Machine metadata directory"),
        }
    }
}

fn create_metadata_directory(path: &Path) -> Result<crate::core::workspace_txn::NativeResource> {
    let (canonical, mut guard) = plan_metadata_directory(path)?;
    if canonical != path {
        bail!("Time Machine metadata directory is not canonical");
    }
    let parent = guard
        .path()
        .parent()
        .context("Time Machine metadata boundary has no parent")?;
    let suffix: Vec<_> = path.strip_prefix(parent)?.components().collect();
    let mut current = parent.to_path_buf();
    for component in suffix {
        guard.verify()?;
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("creating admitted Time Machine metadata directory")
            }
        }
        let next =
            crate::core::workspace_txn::NativeResource::resolve(&current.join(".tm-boundary"))?;
        harden_metadata_directory(&current)?;
        guard.verify()?;
        guard = next;
    }
    harden_metadata_directory(path)?;
    guard.verify()?;
    Ok(guard)
}

fn harden_metadata_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    crate::core::persist::harden_owner_only_checked(path)?;
    #[cfg(not(any(unix, windows)))]
    bail!("Time Machine owner-only directory protection is unavailable");
    Ok(())
}

fn read_speculation_registry(root: &Path) -> Result<identity::Registry> {
    use std::io::Read;
    let (_, boundary) = plan_metadata_directory(root)?;
    let path = root.join("registry.json");
    let bytes = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            let guard = crate::core::workspace_txn::NativeResource::resolve(&path)?;
            if metadata.len() > MAX_SPECULATION_REGISTRY_BYTES {
                bail!("Time Machine registry exceeds safety bound");
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(MAX_SPECULATION_REGISTRY_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_SPECULATION_REGISTRY_BYTES {
                bail!("Time Machine registry exceeds safety bound");
            }
            guard.verify()?;
            bytes
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            require_no_unmapped_speculation(root)?;
            return Ok(identity::Registry::default());
        }
        Err(error) => return Err(error).context("reading speculative Time Machine registry"),
    };
    boundary.verify()?;
    let registry: identity::Registry = serde_json::from_slice(&bytes)
        .context("speculative Time Machine registry is corrupt; it was not replaced")?;
    validate_speculation_registry(&registry)?;
    if registry.schema_version == 0 {
        require_no_unmapped_speculation(root)?;
    }
    Ok(registry)
}

fn require_no_unmapped_speculation(root: &Path) -> Result<()> {
    for path in checked_metadata_entries(root)? {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("repo-"))
            && store_has_speculation_evidence(&path)?
        {
            bail!("missing/unversioned Time Machine registry cannot re-key speculative evidence");
        }
    }
    Ok(())
}

fn validate_speculation_registry(registry: &identity::Registry) -> Result<()> {
    if registry.schema_version > 1 {
        bail!("unsupported future Time Machine registry schema");
    }
    let valid_id = |id: &str, prefix: &str| {
        id.strip_prefix(prefix)
            .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    let mut repos = HashSet::new();
    let mut common_dirs = HashSet::new();
    let mut git_dirs = HashSet::new();
    for repo in &registry.repos {
        if !valid_id(&repo.repo_id, "repo-")
            || !repos.insert(&repo.repo_id)
            || !common_dirs.insert(&repo.common_git_dir)
            || !Path::new(&repo.common_git_dir).is_absolute()
        {
            bail!("invalid or ambiguous Time Machine repository registry mapping");
        }
        let mut worktrees = HashSet::new();
        for wt in &repo.worktrees {
            if !valid_id(&wt.worktree_id, "wt-")
                || !worktrees.insert(&wt.worktree_id)
                || !git_dirs.insert(&wt.git_dir)
                || !Path::new(&wt.git_dir).is_absolute()
            {
                bail!("invalid or ambiguous Time Machine worktree registry mapping");
            }
        }
    }
    Ok(())
}

/// Restart inspection is DATA ONLY. It deliberately cannot recreate a BaseSnapshot, writer lease,
/// integration plan, or restore authority from a manifest. No source path in these records is opened.
#[derive(Debug, Default)]
struct SpeculationInspection {
    cohorts: usize,
    prepared: usize,
    verified: usize,
    read_dependency_concerns: usize,
}

fn checked_metadata_entries(path: &Path) -> Result<Vec<PathBuf>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading speculation metadata directory"),
        Ok(metadata)
            if !metadata.is_dir() || crate::core::file_placement::is_link_or_reparse(&metadata) =>
        {
            bail!("speculation metadata requires non-link directories");
        }
        Ok(_) => {}
    }
    let boundary =
        crate::core::workspace_txn::NativeResource::resolve(&path.join(".tm-inspection-boundary"))?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        if entries.len() >= MAX_SPECULATION_SCAN_ENTRIES {
            bail!("speculation metadata traversal exceeds {MAX_SPECULATION_SCAN_ENTRIES} entries; evidence retained");
        }
        entries.push(entry?.path());
    }
    boundary.verify()?;
    Ok(entries)
}

fn read_speculation_evidence(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::core::file_placement::read_existing_regular(path, limit)
}

fn evidence_text<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("speculation record has invalid {field}"))
}

fn evidence_digest(value: &serde_json::Value) -> bool {
    value.as_array().is_some_and(|bytes| {
        bytes.len() == 32
            && bytes
                .iter()
                .all(|byte| byte.as_u64().is_some_and(|byte| byte <= 255))
    })
}

fn evidence_fingerprint(value: &serde_json::Value) -> bool {
    value
        .get("exists")
        .and_then(serde_json::Value::as_bool)
        .is_some()
        && value
            .get("byte_len")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|len| len <= crate::core::speculation::store::MAX_FILE_BYTES as u64)
        && value.get("sha256").is_some_and(evidence_digest)
}

fn parsed_evidence_fingerprint(
    value: &serde_json::Value,
) -> Result<crate::core::persist::FileFingerprint> {
    if !evidence_fingerprint(value) {
        bail!("invalid integration fingerprint evidence");
    }
    let exists = value["exists"]
        .as_bool()
        .context("invalid integration existence evidence")?;
    let byte_len = value["byte_len"]
        .as_u64()
        .context("invalid integration image length")?;
    let mut sha256 = [0u8; 32];
    for (index, byte) in value["sha256"]
        .as_array()
        .context("invalid integration digest")?
        .iter()
        .enumerate()
    {
        sha256[index] = byte.as_u64().context("invalid integration digest byte")? as u8;
    }
    let fingerprint = crate::core::persist::FileFingerprint {
        exists,
        byte_len,
        sha256,
    };
    if !exists && fingerprint != crate::core::persist::FileFingerprint::missing() {
        bail!("missing integration image has nonempty fingerprint");
    }
    Ok(fingerprint)
}

fn evidence_array<'a>(
    value: &'a serde_json::Value,
    field: &str,
    bound: usize,
) -> Result<&'a [serde_json::Value]> {
    let rows = value
        .get(field)
        .and_then(serde_json::Value::as_array)
        .with_context(|| format!("integration {field} evidence missing"))?;
    if rows.len() > bound {
        bail!("integration {field} evidence exceeds bound");
    }
    Ok(rows)
}

fn evidence_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        bail!("invalid bounded integration name/path evidence");
    }
    Ok(())
}

fn validate_prepared_image(
    descriptor: &serde_json::Value,
    fingerprint: &crate::core::persist::FileFingerprint,
    images: &[Vec<u8>],
    next_image: &mut usize,
) -> Result<()> {
    if parsed_evidence_fingerprint(descriptor)? != *fingerprint
        || descriptor
            .get("missing")
            .and_then(serde_json::Value::as_bool)
            != Some(!fingerprint.exists)
    {
        bail!("prepared image descriptor differs from its file fingerprint");
    }
    if !fingerprint.exists {
        if descriptor.get("index") != Some(&serde_json::Value::Null) {
            bail!("absent prepared preimage must not reference an image");
        }
        return Ok(());
    }
    if descriptor.get("index").and_then(serde_json::Value::as_u64) != Some(*next_image as u64) {
        bail!("prepared image index is missing, reused or out of deterministic order");
    }
    let image = images
        .get(*next_image)
        .context("prepared image bytes are missing")?;
    if crate::core::persist::FileFingerprint::for_bytes(image) != *fingerprint {
        bail!("prepared image bytes/hash differ from declared exact image");
    }
    *next_image += 1;
    Ok(())
}

/// Inspect caller-selected exact resources under fresh parent admission. Only metadata admission
/// bookkeeping may be created; no source bytes, speculative records, or native journal are changed.
/// The caller must already be the current plain parent; only the top-level CLI may seat one.
pub(crate) fn inspect_speculation_recovery(
    root: &Path,
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
) -> Result<SpeculationRecoveryReport> {
    inspect_or_record_speculation_recovery(root, cohort, prepared, resource_paths, false)
}

/// Record a freshly admitted inspection in an existing cohort. This is nonterminal historical
/// evidence only; source/native journal bytes and ordinary pending guards remain unchanged.
pub(crate) fn record_speculation_recovery_observation(
    root: &Path,
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
) -> Result<SpeculationRecoveryReport> {
    inspect_or_record_speculation_recovery(root, cohort, prepared, resource_paths, true)
}

fn inspect_or_record_speculation_recovery(
    root: &Path,
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
    record_observation: bool,
) -> Result<SpeculationRecoveryReport> {
    use crate::core::{
        exec_ctx,
        workspace_txn::{PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease},
    };
    let parent = exec_ctx::current().context("restart report requires the current plain parent")?;
    require_recovery_report_parent(&parent)?;
    validate_recovery_report_request(cohort, prepared, resource_paths)?;
    let identity = WorkspaceIdentity::discover(root)?;
    let approved = resolve_recovery_report_resources(&identity.canonical_root, resource_paths)?;
    let plan = plan_speculation_binding(&identity.canonical_root)?
        .context("restart report requires a Git Time Machine binding; native non-Git recovery is not implemented")?;
    let cancel = crate::core::cancel::current();
    let start = std::time::Instant::now();
    let scope = parent.resource_scope();
    let writer = WorkspaceWriterLease::acquire_identity_scoped(
        identity,
        LOCK_TIMEOUT,
        cancel.as_ref(),
        "restart inspection",
        &scope,
    )?;
    let physical = PhysicalEffectLease::exclusive_without_background(
        LOCK_TIMEOUT.saturating_sub(start.elapsed()),
        cancel.as_ref(),
        "restart inspection",
        &scope,
    )?;
    let body_ctx = parent
        .with_writer_lease(writer.clone())
        .with_physical_effect_lease(physical.clone());
    exec_ctx::with_current(body_ctx, || {
        let binding = plan.admit(&writer, &physical)?;
        let selection =
            binding.select_speculation_recovery(&parent, &writer, &physical, cohort, prepared)?;
        // Selection releases its TM locks. Report then takes Resource -> TimeMachine -> Journal.
        if record_observation {
            binding
                .inspect_selected_recovery(&parent, &writer, &physical, &selection, &approved, true)
        } else {
            binding.report_speculation_recovery(&parent, &writer, &physical, &selection, &approved)
        }
    })
}

fn validate_recovery_report_request(
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
) -> Result<()> {
    if cohort.len() != 32 || !cohort.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("restart report needs an exact bounded cohort identifier");
    }
    let digest = prepared
        .strip_prefix("m-")
        .and_then(|name| name.strip_suffix(".json"))
        .context("restart report needs an exact immutable prepared-record identifier")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("restart report needs an exact immutable prepared-record identifier");
    }
    if resource_paths.len() > crate::core::speculation::store::MAX_COHORT_FILES {
        bail!("restart report resource selection exceeds cohort capacity");
    }
    for path in resource_paths {
        crate::core::workspace_txn::NativeResource::validate_spelling(Path::new(path))?;
    }
    Ok(())
}

fn resolve_recovery_report_resources(
    root: &Path,
    resource_paths: &[String],
) -> Result<Vec<crate::core::workspace_txn::NativeResource>> {
    use crate::core::workspace_txn::NativeResource;
    let mut approved = Vec::with_capacity(resource_paths.len());
    let mut keys = std::collections::BTreeSet::new();
    for supplied in resource_paths {
        let supplied = Path::new(supplied);
        // Check original spelling before joining a Windows verbatim root can erase traversal.
        NativeResource::validate_spelling(supplied)?;
        let path = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            root.join(supplied)
        };
        let resource = NativeResource::resolve(&path)?;
        let relative = resource
            .path()
            .strip_prefix(root)
            .context("restart selected resource is outside workspace")?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                component.as_os_str().to_str().is_none_or(|part| {
                    [".git", ".aizen", "native-journal-v1", "speculation-v1"]
                        .iter()
                        .any(|internal| part.eq_ignore_ascii_case(internal))
                })
            })
            || !keys.insert(resource.key().to_owned())
        {
            bail!("restart report refuses internal or duplicate resource selection");
        }
        approved.push(resource);
    }
    Ok(approved)
}

/// A sealed DATA-ONLY selection of exact immutable evidence. It is intentionally neither
/// deserializable nor an append/source capability; admission and inventory are checked again.
#[derive(Debug, Clone)]
pub(crate) struct SpeculationRecoverySelection {
    identity: crate::core::workspace_txn::WorkspaceIdentity,
    namespace: PathBuf,
    cohort: String,
    prepared: String,
    inventory_sha256: [u8; 32],
    files: Vec<PreparedFileEvidence>,
    native_receipt: Option<RecoveryNativeReceipt>,
    owner: String,
}

#[derive(Debug, Clone)]
struct RecoveryNativeReceipt {
    id: String,
    owner: String,
    coverage: Option<crate::core::native_journal::BatchIdentity>,
    records: Vec<RecoveryNativeRecord>,
}

#[derive(Debug, Clone)]
struct RecoveryNativeRecord {
    file: usize,
    id: u64,
    applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryImageClass {
    Pre,
    Post,
    SamePreAndPost,
    Foreign,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveryFileObservation {
    pub(crate) key: String,
    pub(crate) class: RecoveryImageClass,
    pub(crate) current: crate::core::persist::FileFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryReportStatus {
    InspectionOnly,
    ObservedNonterminal,
}

/// Data-only restart report. An optional explicit observation records inspection only, never
/// source/overlay authority, terminal settlement, accepted verification or permission to replay.
#[derive(Debug)]
pub(crate) struct SpeculationRecoveryReport {
    pub(crate) cohort: String,
    pub(crate) prepared: String,
    pub(crate) inventory_sha256: [u8; 32],
    pub(crate) status: RecoveryReportStatus,
    /// Exact evidence fingerprints at this inspection, never effect attribution or undo authority.
    pub(crate) files: Vec<RecoveryFileObservation>,
    /// Historical exact journal evidence only; never a live-tip, no-effect, or undo capability.
    pub(crate) native_records: Vec<crate::core::native_journal::RecordProof>,
    /// Present only after exact complete historical batch inspection, never physical-worker closure.
    pub(crate) native_coverage: Option<crate::core::native_journal::BatchIdentity>,
    pub(crate) observation: Option<crate::core::speculation::store::ObservationPublication>,
}

impl SpeculationRecoveryReport {
    fn nonterminal_observation(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        selection: &SpeculationRecoverySelection,
    ) -> Result<crate::core::speculation::recovery::InspectionObservation> {
        use crate::core::speculation::recovery::{
            InspectionObservation, ObservationState, ObservedFile, ObservedImageClass,
            ObservedNativeRecord,
        };
        let owner = selection
            .native_receipt
            .as_ref()
            .map(|receipt| receipt.owner.clone())
            .unwrap_or_else(|| selection.owner.clone());
        let files = self
            .files
            .iter()
            .map(|file| {
                let class = match file.class {
                    RecoveryImageClass::Pre => ObservedImageClass::Pre,
                    RecoveryImageClass::Post => ObservedImageClass::Post,
                    RecoveryImageClass::SamePreAndPost => ObservedImageClass::SamePreAndPost,
                    RecoveryImageClass::Foreign => {
                        bail!("foreign image cannot become an inspection observation")
                    }
                };
                Ok(ObservedFile {
                    key: file.key.clone(),
                    class,
                    fingerprint: (&file.current).into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(InspectionObservation {
            schema: 1,
            base: self.cohort.clone(),
            prepared: self.prepared.clone(),
            owner,
            receipt: selection
                .native_receipt
                .as_ref()
                .map(|receipt| receipt.id.clone()),
            observer: parent.resource_scope(),
            state: ObservationState::ObservedNonterminal,
            inventory_sha256: self.inventory_sha256,
            files,
            native_records: self
                .native_records
                .iter()
                .map(|proof| ObservedNativeRecord {
                    id: proof.id,
                    status: proof.status.into(),
                })
                .collect(),
        })
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        use crate::core::native_journal::RecordProofStatus;
        let status = match self.status {
            RecoveryReportStatus::InspectionOnly => "inspection_only",
            RecoveryReportStatus::ObservedNonterminal => "observed_nonterminal",
        };
        let publication = self.observation.as_ref().map(|publication| serde_json::json!({
            "record":publication.record,"before_inventory_sha256":publication.before,"after_inventory_sha256":publication.after,
        }));
        let notice = if publication.is_some() {
            "A nonterminal inspection observation was appended. Pending guard unchanged. No replay, settlement, rollback or undo is authorized. Historical native proof is not current-tip or semantic verification authority."
        } else {
            "Evidence retained. No replay, append, settlement, rollback or undo is authorized. Missing native proof is not proof of no effect; postimage matches do not prove attribution or current journal tip. Admission may create metadata bookkeeping only."
        };
        let files: Vec<_> = self.files.iter().map(|file| {
            let class = match file.class {
                RecoveryImageClass::Pre => "pre", RecoveryImageClass::Post => "post",
                RecoveryImageClass::SamePreAndPost => "same_pre_and_post", RecoveryImageClass::Foreign => "foreign",
            };
            serde_json::json!({"key":file.key,"class":class,"fingerprint":{
                "exists":file.current.exists,"byte_len":file.current.byte_len,"sha256":file.current.sha256,
            }})
        }).collect();
        let native_records: Vec<_> = self
            .native_records
            .iter()
            .map(|record| {
                let status = match record.status {
                    RecordProofStatus::Prepared => "prepared",
                    RecordProofStatus::Applied => "applied",
                    RecordProofStatus::SettledApplied => "settled_applied",
                    RecordProofStatus::SettledNotApplied => "settled_not_applied",
                };
                serde_json::json!({"id":record.id,"status":status})
            })
            .collect();
        serde_json::json!({
            "schema":1,"cohort":self.cohort,"prepared":self.prepared,"inventory_sha256":self.inventory_sha256,
            "status":status,"inspection_only":publication.is_none(),"pending_guard_unchanged":true,
            "observation":publication,"notice":notice,
            "files":files,"native_records":native_records,"native_coverage":self.native_coverage,
            "native_proof_scope":if self.native_coverage.is_some() { "complete_historical_batch" } else { "selected_records_only_or_missing" },
        })
    }

    pub(crate) fn to_text(&self) -> String {
        let value = self.to_json();
        let mut out = format!(
            "Speculation recovery inspection: {}\ncohort {}\nprepared {}\n",
            value["status"].as_str().unwrap(),
            self.cohort,
            self.prepared
        );
        for file in value["files"].as_array().unwrap() {
            out.push_str(&format!(
                "  resource {}: {}\n",
                file["key"].as_str().unwrap(),
                file["class"].as_str().unwrap()
            ));
        }
        for record in value["native_records"].as_array().unwrap() {
            out.push_str(&format!(
                "  historical native record {}: {}\n",
                record["id"],
                record["status"].as_str().unwrap()
            ));
        }
        if self.native_coverage.is_some() {
            out.push_str("  complete historical native batch checked; not physical-worker closure or recovery authority\n");
        } else {
            out.push_str("  native evidence is selected-record-only or missing; complete coverage unproven\n");
        }
        if let Some(publication) = &self.observation {
            out.push_str(&format!(
                "  nonterminal observation {}\n",
                publication.record
            ));
            out.push_str("Inspection recorded; pending guard unchanged. ");
        } else {
            out.push_str("Inspection only; pending guard unchanged. ");
        }
        out.push_str(value["notice"].as_str().unwrap());
        out
    }
}

#[derive(Debug, Clone)]
struct PreparedFileEvidence {
    key: String,
    path: String,
    preimage: crate::core::persist::FileFingerprint,
    postimage: crate::core::persist::FileFingerprint,
}

struct PreparedEvidence {
    keys: Vec<String>,
    paths: Vec<String>,
    files: Vec<PreparedFileEvidence>,
    native_coverage: Option<crate::core::native_journal::BatchIdentity>,
    read_dependencies: Vec<serde_json::Value>,
}

/// Validate evidence relationships without resolving any recorded source path. Peer-read findings
/// are preserved as concerns, never interpreted as semantic reconciliation by a build/test verdict.
fn validate_prepared_evidence(
    base: &str,
    owner: &str,
    value: &serde_json::Value,
    images: &[Vec<u8>],
) -> Result<PreparedEvidence> {
    use std::collections::{BTreeMap, BTreeSet};
    let sealed = crate::core::speculation::store::sealed_lineage(value)?;
    let generations: BTreeMap<_, _> = sealed
        .iter()
        .flatten()
        .map(|identity| {
            (
                format!("{}/{}", identity.owner, identity.overlay),
                identity.generation,
            )
        })
        .collect();
    let overlays = evidence_array(value, "overlays", 32)?;
    if overlays.is_empty() {
        bail!("prepared integration has no overlay lineage");
    }
    let mut authors = BTreeSet::new();
    let mut overlay_ids = BTreeSet::new();
    for overlay in overlays {
        let overlay_owner = evidence_text(overlay, "owner")?;
        let id = evidence_text(overlay, "id")?;
        let author = evidence_text(overlay, "author")?;
        evidence_name(overlay_owner)?;
        evidence_name(id)?;
        if evidence_text(overlay, "base")? != base
            || !(overlay_owner == owner
                || overlay_owner
                    .strip_prefix(owner)
                    .is_some_and(|tail| tail.starts_with('/')))
            || author != format!("{overlay_owner}/{id}")
            || !overlay_ids.insert(id)
            || !authors.insert(author)
        {
            bail!("invalid or ambiguous prepared overlay lineage");
        }
    }
    let files = evidence_array(
        value,
        "files",
        crate::core::speculation::store::MAX_COHORT_FILES,
    )?;
    if images.len() > files.len() * 2 {
        bail!("prepared integration has unreferenced images");
    }
    let mut next_image = 0usize;
    let mut keys = Vec::new();
    let mut applied_paths = Vec::new();
    let mut file_images = Vec::new();
    let mut paths = BTreeMap::new();
    let mut expected_images = BTreeMap::new();
    let mut changes: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for file in files {
        let key = evidence_text(file, "key")?;
        let path = evidence_text(file, "path")?;
        evidence_name(key)?;
        evidence_name(path)?;
        if keys.iter().any(|previous| previous == key) {
            bail!("duplicate prepared file key");
        }
        keys.push(key.to_owned());
        applied_paths.push(path.to_owned());
        paths.insert(key, path);
        let expected = parsed_evidence_fingerprint(
            file.get("expected")
                .context("prepared preimage fingerprint missing")?,
        )?;
        let after = parsed_evidence_fingerprint(
            file.get("after")
                .context("prepared postimage fingerprint missing")?,
        )?;
        if !after.exists {
            bail!("prepared postimage cannot be absent");
        }
        validate_prepared_image(
            file.get("preimage")
                .context("prepared preimage mapping missing")?,
            &expected,
            images,
            &mut next_image,
        )?;
        validate_prepared_image(
            file.get("postimage")
                .context("prepared postimage mapping missing")?,
            &after,
            images,
            &mut next_image,
        )?;
        file_images.push(PreparedFileEvidence {
            key: key.to_owned(),
            path: path.to_owned(),
            preimage: expected.clone(),
            postimage: after,
        });
        expected_images.insert(key, expected);
        let mut changed_authors = BTreeSet::new();
        let mut rewrite = false;
        for change in evidence_array(file, "lineage", 32)? {
            let author = evidence_text(change, "author")?;
            let operation = evidence_text(change, "operation")?;
            if !authors.contains(author)
                || !changed_authors.insert(author)
                || change
                    .get("generation")
                    .and_then(serde_json::Value::as_u64)
                    .is_none_or(|generation| {
                        generation == 0
                            || generations
                                .get(author)
                                .is_some_and(|maximum| generation > *maximum)
                    })
                || !matches!(operation, "localized" | "rewrite")
            {
                bail!("invalid prepared change lineage");
            }
            rewrite |= operation == "rewrite";
        }
        if changed_authors.is_empty()
            || evidence_text(file, "operation")? != if rewrite { "rewrite" } else { "localized" }
        {
            bail!("prepared operation disagrees with lineage");
        }
        let mut contributors = BTreeSet::new();
        for author in evidence_array(file, "contributors", 32)? {
            let author = author.as_str().context("invalid prepared contributor")?;
            if !changed_authors.contains(author) || !contributors.insert(author) {
                bail!("prepared contributor is missing from exact lineage or duplicated");
            }
        }
        changes.insert(key, changed_authors);
    }
    if next_image != images.len() {
        bail!("prepared integration has unused image bytes");
    }
    let mut expected_dependencies: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    let mut observed_versions = BTreeSet::new();
    let mut base_observers = BTreeSet::new();
    let mut base_fingerprints = BTreeMap::new();
    for observation in evidence_array(value, "observations", 32 * 512)? {
        let author = evidence_text(observation, "author")?;
        let key = evidence_text(observation, "key")?;
        let path = evidence_text(observation, "path")?;
        let generation = observation
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .context("prepared read observation generation missing")?;
        evidence_name(key)?;
        evidence_name(path)?;
        if !authors.contains(author)
            || !observed_versions.insert((author, key, generation))
            || generations
                .get(author)
                .is_some_and(|maximum| generation > *maximum)
            || paths.get(key).is_some_and(|previous| *previous != path)
        {
            bail!("invalid or duplicate prepared read observation evidence");
        }
        paths.insert(key, path);
        let fingerprint = parsed_evidence_fingerprint(
            observation
                .get("fingerprint")
                .context("prepared observation fingerprint missing")?,
        )?;
        if generation == 0 {
            if expected_images
                .get(key)
                .is_some_and(|expected| *expected != fingerprint)
                || base_fingerprints
                    .get(key)
                    .is_some_and(|expected| *expected != fingerprint)
            {
                bail!("prepared base read differs from exact preimage evidence");
            }
            base_fingerprints.insert(key, fingerprint);
            base_observers.insert((author, key));
        }
        if let Some(peers) = changes.get(key) {
            for peer in peers {
                if *peer != author {
                    expected_dependencies
                        .entry((author, key))
                        .or_default()
                        .insert(peer);
                }
            }
        }
    }
    for (key, changed_authors) in &changes {
        for author in changed_authors {
            if !base_observers.contains(&(*author, *key)) {
                bail!("prepared change is missing its exact base read observation");
            }
        }
    }
    let read_dependencies = evidence_array(
        value,
        "read_dependencies",
        32 * crate::core::speculation::store::MAX_COHORT_FILES,
    )?;
    let mut declared = BTreeMap::new();
    for dependency in read_dependencies {
        let observer = evidence_text(dependency, "observer")?;
        let key = evidence_text(dependency, "key")?;
        let mut peers = BTreeSet::new();
        for peer in evidence_array(dependency, "peer_authors", 32)? {
            let peer = peer
                .as_str()
                .context("invalid integration peer-read author")?;
            if peer == observer || !authors.contains(peer) || !peers.insert(peer) {
                bail!("invalid integration peer-read dependency");
            }
        }
        if !authors.contains(observer)
            || peers.is_empty()
            || declared.insert((observer, key), peers).is_some()
        {
            bail!("invalid or duplicate integration read dependency");
        }
    }
    if declared != expected_dependencies {
        bail!("prepared read dependencies disagree with observations/change lineage");
    }
    let native_coverage = evidence_native_coverage(value)?;
    Ok(PreparedEvidence {
        keys,
        paths: applied_paths,
        files: file_images,
        native_coverage,
        read_dependencies: read_dependencies.to_vec(),
    })
}

fn require_recovery_report_parent(parent: &crate::core::exec_ctx::ExecutionContext) -> Result<()> {
    let current = crate::core::exec_ctx::current()
        .context("restart report requires the current plain parent")?;
    if !parent.can_restore_workspace()
        || parent.native_scope().is_some()
        || parent.overlay().is_some()
        || !current.can_restore_workspace()
        || current.native_scope().is_some()
        || current.overlay().is_some()
        || current.resource_scope() != parent.resource_scope()
        || parent.resource_scope().is_empty()
    {
        bail!(
            "restart report requires fresh coordinating-parent admission, not delegated authority"
        );
    }
    Ok(())
}

fn recovery_cohort_directory(namespace: &Path, cohort: &str) -> Result<PathBuf> {
    if cohort.len() != 32 || !cohort.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("restart report needs an exact bounded cohort identifier");
    }
    Ok(namespace.join("speculation-v1").join(cohort))
}

/// Hash every immutable metadata object, including unreferenced objects, in a bounded sorted
/// inventory. The owner marker is represented by its validated canonical bytes: callers must check
/// its actual bytes under exclusive ownership before relying on a revision during classification.
/// This revision is only a stale-selection check, never authority derived from a hash.
fn recovery_inventory_digest(directory: &Path) -> Result<[u8; 32]> {
    crate::core::speculation::store::inventory_revision(directory)
}

fn validate_recovery_base_identity(
    directory: &Path,
    identity: &crate::core::workspace_txn::WorkspaceIdentity,
) -> Result<()> {
    for path in checked_metadata_entries(directory)? {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("m-"))
        {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_slice(&read_speculation_evidence(&path, 2 * 1024 * 1024)?)?;
        if evidence_text(&value, "kind")? != "base" {
            continue;
        }
        if evidence_text(&value, "repo")? != identity.repo_id
            || evidence_text(&value, "worktree")? != identity.worktree_id
            || Some(evidence_text(&value, "root")?) != identity.canonical_root.to_str()
        {
            bail!("restart base evidence does not match fresh workspace identity");
        }
    }
    Ok(())
}

fn validate_inspection_observation(
    base: &str,
    record: &crate::core::speculation::store::InspectedRecord,
    records: &[crate::core::speculation::store::InspectedRecord],
) -> Result<()> {
    use crate::core::speculation::recovery::{InspectionObservation, ObservedNativeStatus};
    let observation: InspectionObservation = serde_json::from_slice(&record.metadata)?;
    observation.validate()?;
    if observation.base != base || observation.observer != record.owner || !record.images.is_empty()
    {
        bail!("inspection observation identity/images mismatch");
    }
    let mut candidates = records.iter().filter(|record| {
        record.id == observation.prepared && record.label == "integration-prepared"
    });
    let prepared = candidates
        .next()
        .context("inspection observation prepared record missing")?;
    if candidates.next().is_some() || prepared.owner != observation.owner {
        bail!("inspection observation prepared owner ambiguous");
    }
    let value: serde_json::Value = serde_json::from_slice(&prepared.metadata)?;
    let evidence = validate_prepared_evidence(base, &prepared.owner, &value, &prepared.images)?;
    if evidence.files.len() != observation.files.len() {
        bail!("inspection observation file count mismatch");
    }
    let mut pre = false;
    let mut post = false;
    for (file, expected) in observation.files.iter().zip(&evidence.files) {
        let fingerprint = file.fingerprint.fingerprint();
        let class = recovery_image_class(&fingerprint, &expected.preimage, &expected.postimage);
        let recorded = match file.class {
            crate::core::speculation::recovery::ObservedImageClass::Pre => RecoveryImageClass::Pre,
            crate::core::speculation::recovery::ObservedImageClass::Post => {
                RecoveryImageClass::Post
            }
            crate::core::speculation::recovery::ObservedImageClass::SamePreAndPost => {
                RecoveryImageClass::SamePreAndPost
            }
        };
        if file.key != expected.key || class != recorded {
            bail!("inspection observation fingerprint/class mismatch");
        }
        pre |= class == RecoveryImageClass::Pre;
        post |= class == RecoveryImageClass::Post;
    }
    if pre && post {
        bail!("inspection observation cannot certify mixed pre/post images");
    }
    let mut receipt = records.iter().filter_map(|record| {
        if record.label != "integration-receipt"
            || observation.receipt.as_deref() != Some(record.id.as_str())
        {
            return None;
        }
        let value: serde_json::Value = match serde_json::from_slice(&record.metadata) {
            Ok(value) => value,
            Err(error) => return Some(Err(anyhow::Error::from(error))),
        };
        (value.get("prepared").and_then(serde_json::Value::as_str)
            == Some(observation.prepared.as_str()))
        .then_some(Ok((record, value)))
    });
    if let Some(entry) = receipt.next() {
        let (record, value) = entry?;
        if receipt.next().is_some() || record.owner != observation.owner {
            bail!("inspection observation receipt ambiguous");
        }
        validate_receipt_evidence(&value, &evidence)?;
        let files = evidence_array(
            &value,
            "files",
            crate::core::speculation::store::MAX_COHORT_FILES,
        )?;
        let mut proofs = observation.native_records.iter();
        for file in files {
            if let Some(id) = file
                .get("native_record_id")
                .and_then(serde_json::Value::as_u64)
            {
                let proof = proofs
                    .next()
                    .context("inspection observation exact native proof missing")?;
                if proof.id != id
                    || (evidence_text(file, "state")? == "applied"
                        && proof.status != ObservedNativeStatus::SettledApplied)
                {
                    bail!("inspection observation native proof mismatch");
                }
            }
        }
        if proofs.next().is_some() {
            bail!("inspection observation has unlinked native proof");
        }
    } else if observation.receipt.is_some() || !observation.native_records.is_empty() {
        bail!("inspection observation lacks exact receipt linkage");
    }
    Ok(())
}

fn read_recovery_selection(
    directory: &Path,
    cohort: &str,
    prepared_id: &str,
) -> Result<(
    [u8; 32],
    Vec<PreparedFileEvidence>,
    Option<RecoveryNativeReceipt>,
)> {
    let before = recovery_inventory_digest(directory)?;
    if read_speculation_evidence(&directory.join("owner.lock"), 1024)?
        != b"aizen-speculation-owner-v1\n"
    {
        bail!("unsupported restart owner marker");
    }
    let records = crate::core::speculation::store::inspect_controller_records(directory)?;
    crate::core::speculation::verification::validate_verification_records(cohort, &records)?;
    crate::core::speculation::workers::validate_worker_records(cohort, &records)?;
    let mut bases = 0usize;
    for path in checked_metadata_entries(directory)? {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("m-"))
        {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_slice(&read_speculation_evidence(&path, 2 * 1024 * 1024)?)?;
        if evidence_text(&value, "format")? != "aizen-speculation-v1"
            || evidence_text(&value, "base")? != cohort
        {
            bail!("unsupported or foreign restart manifest");
        }
        match evidence_text(&value, "kind")? {
            "base" => bases += 1,
            "overlay" | "controller-record" => {}
            _ => bail!("unknown restart manifest kind"),
        }
    }
    if bases != 1 {
        bail!("restart report requires one exact base manifest");
    }
    let prepared = records
        .iter()
        .find(|record| record.id == prepared_id && record.label == "integration-prepared")
        .context("selected immutable prepared record is missing")?;
    let prepared_value: serde_json::Value = serde_json::from_slice(&prepared.metadata)?;
    let evidence =
        validate_prepared_evidence(cohort, &prepared.owner, &prepared_value, &prepared.images)?;
    crate::core::speculation::workers::validate_worker_records(cohort, &records)?
        .check_selected_lineage(&prepared_value)?;
    let mut prepared_count = 0;
    let mut receipt_id = None;
    let mut native_receipt = None;
    let mut verification = None;
    for record in &records {
        if crate::core::speculation::workers::is_worker_label(&record.label) {
            // Complete inventory linkage was validated above. Physical evidence is historical;
            // neither an unclosed intent nor a closure authorizes source replay or ownership.
            continue;
        }
        if record.label == crate::core::speculation::recovery::OBSERVATION_LABEL {
            validate_inspection_observation(cohort, record, &records)?;
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&record.metadata)?;
        if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
            || evidence_text(&value, "base")? != cohort
            || evidence_text(&value, "owner")? != prepared.owner
            || record.owner != prepared.owner
        {
            bail!("foreign or unsupported restart record identity");
        }
        match record.label.as_str() {
            "integration-prepared" => {
                prepared_count += 1;
                if record.id != prepared_id || evidence_text(&value, "state")? != "prepared" {
                    bail!("ambiguous restart prepared evidence");
                }
            }
            "integration-receipt" => {
                if evidence_text(&value, "prepared")? != prepared_id
                    || receipt_id.replace(record.id.as_str()).is_some()
                    || !matches!(
                        evidence_text(&value, "state")?,
                        "partial" | "applied-unverified"
                    )
                    || !record.images.is_empty()
                {
                    bail!("ambiguous or unknown restart receipt");
                }
                validate_receipt_evidence(&value, &evidence)?;
                let applied = value
                    .get("applied")
                    .and_then(serde_json::Value::as_u64)
                    .context("restart receipt application count missing")?;
                let files = evidence_array(
                    &value,
                    "files",
                    crate::core::speculation::store::MAX_COHORT_FILES,
                )?;
                if value.get("total").and_then(serde_json::Value::as_u64)
                    != Some(evidence.files.len() as u64)
                    || applied > evidence.files.len() as u64
                    || files.len() != evidence.files.len()
                {
                    bail!("restart receipt file count differs from prepared evidence");
                }
                let mut exact_records = Vec::new();
                for (index, (file, prepared)) in files.iter().zip(&evidence.files).enumerate() {
                    if evidence_text(file, "key")? != prepared.key
                        || !matches!(
                            evidence_text(file, "state")?,
                            "applied" | "possibly-applied" | "not-applied"
                        )
                        || file
                            .get("postimage_checked")
                            .and_then(serde_json::Value::as_bool)
                            .is_none()
                    {
                        bail!("restart receipt file evidence is unknown or mismatched");
                    }
                    if let Some(id) = file
                        .get("native_record_id")
                        .and_then(serde_json::Value::as_u64)
                    {
                        exact_records.push(RecoveryNativeRecord {
                            file: index,
                            id,
                            applied: evidence_text(file, "state")? == "applied",
                        });
                    }
                }
                native_receipt = Some(RecoveryNativeReceipt {
                    id: record.id.clone(),
                    owner: prepared.owner.clone(),
                    coverage: evidence.native_coverage.clone(),
                    records: exact_records,
                });
            }
            "integration-verification-started" => {
                if evidence_text(&value, "prepared")? != prepared_id {
                    bail!("restart verification start belongs to another prepared record");
                }
                // Strict lifecycle linkage was checked across the complete, unordered inventory.
            }
            "integration-verification" => {
                if evidence_text(&value, "prepared")? != prepared_id
                    || verification.replace(value).is_some()
                    || !record.images.is_empty()
                {
                    bail!("ambiguous restart verification");
                }
            }
            _ => bail!("unsupported restart controller record; no recovery action is authorized"),
        }
    }
    if prepared_count != 1 {
        bail!("ambiguous restart prepared selection");
    }
    if let Some(value) = verification {
        if Some(evidence_text(&value, "integration")?) != receipt_id
            || !matches!(
                evidence_text(&value, "state")?,
                "verified" | "stale" | "verification_failed" | "unverified"
            )
        {
            bail!("orphan or unknown restart verification");
        }
    }
    let after = recovery_inventory_digest(directory)?;
    if before != after {
        bail!("restart evidence changed during selection");
    }
    Ok((after, evidence.files, native_receipt))
}

fn with_recovery_owner_claim<T>(directory: &Path, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock = directory.join("owner.lock");
    let resource = crate::core::workspace_txn::NativeResource::resolve(&lock)?;
    let stamp = crate::core::file_placement::NodeStamp::capture(
        &lock,
        &fs::symlink_metadata(&lock)?,
        false,
    )?;
    let owner =
        crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(&lock, Duration::ZERO)?;
    owner.verify_existing_path(&lock)?;
    if owner.read_bounded_exclusive(1024)? != b"aizen-speculation-owner-v1\n" {
        bail!("unsupported restart owner marker under exclusive ownership");
    }
    resource.verify()?;
    if crate::core::file_placement::NodeStamp::capture(&lock, &fs::symlink_metadata(&lock)?, false)?
        != stamp
    {
        bail!("restart owner lock identity changed");
    }
    let result = body()?;
    owner.verify_existing_path(&lock)?;
    if owner.read_bounded_exclusive(1024)? != b"aizen-speculation-owner-v1\n" {
        bail!("restart owner marker changed during report");
    }
    resource.verify()?;
    if crate::core::file_placement::NodeStamp::capture(&lock, &fs::symlink_metadata(&lock)?, false)?
        != stamp
    {
        bail!("restart owner lock changed during report");
    }
    Ok(result)
}

fn recovery_live_fingerprint(
    resource: &crate::core::workspace_txn::NativeResource,
) -> Result<crate::core::persist::FileFingerprint> {
    resource.verify()?;
    match fs::symlink_metadata(resource.path()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            resource.verify()?;
            if !matches!(fs::symlink_metadata(resource.path()), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                bail!("restart source changed during missing-file inspection");
            }
            Ok(crate::core::persist::FileFingerprint::missing())
        }
        Err(error) => Err(error).context("restart source metadata unreadable"),
        Ok(_) => {
            let bytes = read_speculation_evidence(
                resource.path(),
                crate::core::speculation::store::MAX_FILE_BYTES as u64,
            )?;
            resource.verify()?;
            Ok(crate::core::persist::FileFingerprint::for_bytes(&bytes))
        }
    }
}

fn recovery_image_class(
    actual: &crate::core::persist::FileFingerprint,
    before: &crate::core::persist::FileFingerprint,
    after: &crate::core::persist::FileFingerprint,
) -> RecoveryImageClass {
    match (actual == before, actual == after) {
        (true, true) => RecoveryImageClass::SamePreAndPost,
        (true, false) => RecoveryImageClass::Pre,
        (false, true) => RecoveryImageClass::Post,
        (false, false) => RecoveryImageClass::Foreign,
    }
}

struct LockedRecoveryResources<'a> {
    resources: std::collections::BTreeMap<&'a str, &'a crate::core::workspace_txn::NativeResource>,
    _locks: crate::core::workspace_txn::LockSet,
}

fn acquire_recovery_resources<'a>(
    root: &Path,
    selection: &SpeculationRecoverySelection,
    approved: &'a [crate::core::workspace_txn::NativeResource],
) -> Result<LockedRecoveryResources<'a>> {
    use crate::core::workspace_txn::{LockClass, LockOwner, LockRequest, LockSet};
    if approved.len() != selection.files.len() {
        bail!("restart report requires exact fresh resource selection");
    }
    let mut resources = std::collections::BTreeMap::new();
    for resource in approved {
        resource.verify()?;
        let relative = resource
            .path()
            .strip_prefix(root)
            .context("restart approved resource is outside workspace")?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                component.as_os_str().to_str().is_none_or(|part| {
                    [".git", ".aizen", "native-journal-v1", "speculation-v1"]
                        .iter()
                        .any(|internal| part.eq_ignore_ascii_case(internal))
                })
            })
            || resources.insert(resource.key(), resource).is_some()
        {
            bail!("restart report refuses internal or duplicate resources");
        }
    }
    for file in &selection.files {
        if resources
            .get(file.key.as_str())
            .is_none_or(|resource| resource.path().to_str() != Some(file.path.as_str()))
        {
            bail!("restart report recorded key/path differs from fresh approved binding");
        }
    }
    let requests = approved
        .iter()
        .map(|resource| {
            LockRequest::new(
                LockClass::Resource,
                crate::core::workspace_txn::resource_lock("native-file", resource.key()),
                crate::core::repo_lock::LockMode::Exclusive,
                "restart data-only classification",
            )
        })
        .collect();
    let locks = LockSet::acquire(
        requests,
        Duration::ZERO,
        None,
        &LockOwner::new("restart-report", "data-only classification"),
    )?;
    for resource in approved {
        resource.verify()?;
    }
    Ok(LockedRecoveryResources {
        resources,
        _locks: locks,
    })
}

fn inspect_recovery_native_records(
    selection: &SpeculationRecoverySelection,
    locked: &LockedRecoveryResources<'_>,
) -> Result<Vec<crate::core::native_journal::RecordProof>> {
    use crate::core::native_journal::{RecordClaim, RecordProofStatus};
    // A missing receipt/ID is lack of proof, NEVER evidence that no live effect occurred.
    let Some(receipt) = &selection.native_receipt else {
        return Ok(Vec::new());
    };
    let mut claims = Vec::with_capacity(receipt.records.len());
    for record in &receipt.records {
        let file = selection
            .files
            .get(record.file)
            .context("restart record file is missing")?;
        let resource = locked
            .resources
            .get(file.key.as_str())
            .context("restart record resource is not freshly approved")?;
        claims.push(RecordClaim {
            id: record.id,
            expected_owner: &receipt.owner,
            resource,
            before: &file.preimage,
            after: &file.postimage,
        });
    }
    // Resource -> actual TM -> cohort owner -> Journal; the reader drops journal ownership here.
    // Covered evidence enumerates the complete fence even when the receipt lists no records.
    // Missing intents/claims remain unresolved, never inferred to be NotApplied from absent bytes.
    let proofs = match &receipt.coverage {
        Some(identity) => crate::core::native_journal::inspect_existing_batch(identity, &claims)?,
        None => crate::core::native_journal::inspect_existing_records(&claims)?,
    };
    if proofs.len() != receipt.records.len() {
        bail!("restart native record proof count differs from receipt");
    }
    for (proof, record) in proofs.iter().zip(&receipt.records) {
        if proof.id != record.id
            || (record.applied && proof.status != RecordProofStatus::SettledApplied)
        {
            bail!("restart applied receipt lacks exact settled native record evidence");
        }
    }
    Ok(proofs)
}

#[cfg(test)]
fn classify_recovery_resources(
    root: &Path,
    selection: &SpeculationRecoverySelection,
    approved: &[crate::core::workspace_txn::NativeResource],
) -> Result<Vec<RecoveryFileObservation>> {
    let resources = acquire_recovery_resources(root, selection, approved)?;
    classify_locked_recovery_resources(selection, &resources)
}

fn classify_locked_recovery_resources(
    selection: &SpeculationRecoverySelection,
    locked: &LockedRecoveryResources<'_>,
) -> Result<Vec<RecoveryFileObservation>> {
    let resources = &locked.resources;
    let mut observations = Vec::new();
    let mut pre = false;
    let mut post = false;
    for file in &selection.files {
        let resource = resources[file.key.as_str()];
        let current = recovery_live_fingerprint(resource)?;
        let class = recovery_image_class(&current, &file.preimage, &file.postimage);
        pre |= class == RecoveryImageClass::Pre;
        post |= class == RecoveryImageClass::Post;
        if class == RecoveryImageClass::Foreign {
            bail!("restart source has foreign bytes; evidence retained, no effect authorized");
        }
        observations.push(RecoveryFileObservation {
            key: file.key.clone(),
            class,
            current,
        });
    }
    if pre && post {
        bail!("restart source contains mixed pre/post images; evidence retained, no effect authorized");
    }
    for observation in &observations {
        if recovery_live_fingerprint(resources[observation.key.as_str()])? != observation.current {
            bail!("restart source drifted during classification");
        }
    }
    Ok(observations)
}

fn evidence_native_coverage(
    value: &serde_json::Value,
) -> Result<Option<crate::core::native_journal::BatchIdentity>> {
    let Some(value) = value.get("native_coverage") else {
        return Ok(None);
    };
    let identity: crate::core::native_journal::BatchIdentity =
        serde_json::from_value(value.clone()).context("invalid native batch coverage identity")?;
    identity.validate()?;
    Ok(Some(identity))
}

/// Legacy selected-ID linkage stays legacy. A covered receipt must repeat its exact prepared fence;
/// neither a matching numeric record ID nor an unsupported version can manufacture complete coverage.
fn validate_receipt_coverage(
    value: &serde_json::Value,
    prepared: Option<&crate::core::native_journal::BatchIdentity>,
) -> Result<Option<crate::core::native_journal::BatchIdentity>> {
    let coverage = evidence_native_coverage(value)?;
    match value
        .get("native_journal_linkage")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) if prepared.is_none() && coverage.is_none() => Ok(None),
        Some(2) if prepared.is_some() && coverage.as_ref() == prepared => Ok(coverage),
        _ => bail!(
            "integration receipt native coverage differs from prepared evidence; evidence retained"
        ),
    }
}

fn validate_receipt_evidence(value: &serde_json::Value, prepared: &PreparedEvidence) -> Result<()> {
    validate_receipt_coverage(value, prepared.native_coverage.as_ref())?;
    validate_receipt_record_ids(value, &prepared.files)
}

/// Validate copied journal identities as evidence only. Neither an ID nor a matching image grants
/// restart mutation/undo authority. Older receipts without supported linkage remain unresolved.
fn validate_receipt_record_ids(
    value: &serde_json::Value,
    prepared: &[PreparedFileEvidence],
) -> Result<()> {
    match value
        .get("native_journal_linkage")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) if evidence_native_coverage(value)?.is_none() => {}
        Some(2) if evidence_native_coverage(value)?.is_some() => {}
        _ => bail!(
            "integration receipt lacks supported exact native-journal linkage; evidence retained"
        ),
    }
    let applied = value
        .get("applied")
        .and_then(serde_json::Value::as_u64)
        .context("integration receipt application count missing")?;
    let possible = match value.get("possibly_applied_record") {
        Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|id| *id > 0)
                .context("invalid possible native record ID")?,
        ),
        None => bail!("integration receipt possible record evidence missing"),
    };
    let files = evidence_array(
        value,
        "files",
        crate::core::speculation::store::MAX_COHORT_FILES,
    )?;
    if applied > prepared.len() as u64
        || files.len() != prepared.len()
        || value.get("total").and_then(serde_json::Value::as_u64) != Some(prepared.len() as u64)
        || (possible.is_some() && applied == prepared.len() as u64)
    {
        bail!("integration receipt journal linkage has inconsistent file counts");
    }
    let mut previous = 0u64;
    for (index, (file, expected)) in files.iter().zip(prepared).enumerate() {
        let is_applied = (index as u64) < applied;
        let is_possible = index as u64 == applied && possible.is_some();
        let state = if is_applied {
            "applied"
        } else if is_possible {
            "possibly-applied"
        } else {
            "not-applied"
        };
        if evidence_text(file, "key")? != expected.key || evidence_text(file, "state")? != state {
            bail!("integration receipt record linkage differs from prepared file order");
        }
        let record = match file.get("native_record_id") {
            Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|id| *id > 0)
                    .context("invalid native record ID")?,
            ),
            None => bail!("integration receipt file lacks exact native record evidence"),
        };
        let changed = expected.preimage != expected.postimage;
        if is_possible {
            if !changed || record != possible {
                bail!("possible native record differs from its exact file");
            }
        } else if record.is_some() != (is_applied && changed) {
            bail!("native record attribution disagrees with the file effect (including no-op)");
        }
        if let Some(id) = record {
            if id <= previous {
                bail!("native record IDs are duplicate or out of application order");
            }
            previous = id;
        }
    }
    Ok(())
}

fn classify_integration_records(
    base: &str,
    records: &[crate::core::speculation::store::InspectedRecord],
) -> Result<(usize, usize, usize)> {
    use std::collections::BTreeMap;
    crate::core::speculation::verification::validate_verification_records(base, records)?;
    let workers = crate::core::speculation::workers::validate_worker_records(base, records)?;
    if workers.unresolved() {
        bail!("speculative physical-worker lifecycle is unresolved or unsuccessful; evidence retained");
    }
    let mut prepared = BTreeMap::new();
    let mut receipts = BTreeMap::new();
    let mut verifications = BTreeMap::new();
    for record in records {
        if record.label == crate::core::speculation::recovery::OBSERVATION_LABEL {
            validate_inspection_observation(base, record, records)?;
            continue;
        }
        if !record.label.starts_with("integration-") {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&record.metadata)
            .context("corrupt speculative integration metadata")?;
        if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
            || evidence_text(&value, "base")? != base
            || evidence_text(&value, "owner")? != record.owner
        {
            bail!(
                "unsupported or mismatched speculative integration record {}",
                record.id
            );
        }
        match record.label.as_str() {
            "integration-prepared" => {
                if evidence_text(&value, "state")? != "prepared" {
                    bail!("invalid integration prepared state");
                }
                let keys = validate_prepared_evidence(base, &record.owner, &value, &record.images)?;
                workers.check_selected_lineage(&value)?;
                if prepared
                    .insert(record.id.as_str(), (record.owner.as_str(), keys))
                    .is_some()
                {
                    bail!("duplicate integration prepared evidence");
                }
            }
            "integration-receipt" => {
                let prepared_id = evidence_text(&value, "prepared")?.to_owned();
                if receipts.insert(prepared_id, (record, value)).is_some() {
                    bail!("ambiguous integration receipts for one prepared record");
                }
            }
            "integration-verification-started" => {
                // A durable start alone never settles an integration.
            }
            "integration-verification" => {
                let prepared_id = evidence_text(&value, "prepared")?.to_owned();
                if verifications.insert(prepared_id, (record, value)).is_some() {
                    bail!("ambiguous integration verification evidence");
                }
            }
            _ => bail!("unsupported speculative integration record label; evidence retained"),
        }
    }
    for id in receipts.keys().chain(verifications.keys()) {
        if !prepared.contains_key(id.as_str()) {
            bail!("orphan speculative integration receipt/verification; evidence retained");
        }
    }
    for (id, (owner, evidence)) in &prepared {
        let keys = &evidence.keys;
        let (receipt, value) = receipts
            .get(*id)
            .with_context(|| format!("prepared integration {id} has no durable receipt"))?;
        if receipt.owner != *owner
            || evidence_text(value, "state")? != "applied-unverified"
            || value.get("applied").and_then(serde_json::Value::as_u64) != Some(keys.len() as u64)
            || value.get("total").and_then(serde_json::Value::as_u64) != Some(keys.len() as u64)
            || value
                .get("postimage_verified")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            bail!("integration {id} is partial or its postimage is unconfirmed; evidence retained");
        }
        validate_receipt_evidence(value, evidence)?;
        let receipt_files = evidence_array(
            value,
            "files",
            crate::core::speculation::store::MAX_COHORT_FILES,
        )?;
        if receipt_files.len() != keys.len()
            || !receipt.images.is_empty()
            || value.get("possibly_applied_record") != Some(&serde_json::Value::Null)
            || value.get("stop") != Some(&serde_json::Value::Null)
        {
            bail!("settled receipt has incomplete file evidence or possible effects");
        }
        for (file, key) in receipt_files.iter().zip(keys) {
            if evidence_text(file, "key")? != key
                || evidence_text(file, "state")? != "applied"
                || file
                    .get("postimage_checked")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
            {
                bail!("receipt file application differs from prepared order/images");
            }
        }
        let (verification, value) = verifications.get(*id).with_context(|| {
            format!("integration {id} is applied but has no accepted verification")
        })?;
        // Legacy verdicts lack physical lifecycle evidence and remain unresolved even if accepted.
        let finished: crate::core::speculation::verification::VerificationFinished =
            serde_json::from_slice(&verification.metadata)
                .context("integration verification lacks supported durable lifecycle evidence")?;
        finished.validate()?;
        if finished.effect_state
            != crate::core::speculation::verification::DurableEffectState::Settled
        {
            bail!("integration verifier has unsettled physical effects; evidence retained");
        }
        let dependency_status = if evidence.read_dependencies.is_empty() {
            "none"
        } else {
            "requires_semantic_review"
        };
        if verification.owner != *owner
            || evidence_text(value, "integration")? != receipt.id
            || evidence_text(value, "state")? != "verified"
            || value.get("accepted").and_then(serde_json::Value::as_bool) != Some(true)
            || !value.get("source_identity").is_some_and(evidence_digest)
            || !verification.images.is_empty()
            || evidence_text(value, "read_dependency_status")? != dependency_status
            || evidence_array(
                value,
                "read_dependencies",
                32 * crate::core::speculation::store::MAX_COHORT_FILES,
            )? != evidence.read_dependencies.as_slice()
            || evidence_array(
                value,
                "applied_paths",
                crate::core::speculation::store::MAX_COHORT_FILES,
            )? != evidence
                .paths
                .iter()
                .map(|path| serde_json::Value::String(path.clone()))
                .collect::<Vec<_>>()
                .as_slice()
        {
            bail!("integration {id} verification is stale, failed, unverified or mismatched (including retained read concerns); evidence retained");
        }
    }
    let concerns = prepared
        .values()
        .map(|(_, evidence)| evidence.read_dependencies.len())
        .sum();
    Ok((prepared.len(), verifications.len(), concerns))
}

fn inspect_speculation_namespace(namespace: &Path) -> Result<SpeculationInspection> {
    let mut summary = SpeculationInspection::default();
    let mut scan_bytes = 0u64;
    for version in checked_metadata_entries(namespace)? {
        let Some(name) = version.file_name().and_then(|name| name.to_str()) else {
            bail!("invalid speculation namespace name");
        };
        if !name.starts_with("speculation-") {
            continue;
        }
        if name != "speculation-v1" {
            bail!("unsupported future speculation namespace; evidence retained");
        }
        for directory in checked_metadata_entries(&version)? {
            if summary.cohorts >= MAX_SPECULATION_COHORTS {
                bail!("speculation cohort inspection exceeds {MAX_SPECULATION_COHORTS}; evidence retained");
            }
            summary.cohorts += 1;
            let base = directory
                .file_name()
                .and_then(|name| name.to_str())
                .context("invalid speculation cohort name")?;
            let paths = checked_metadata_entries(&directory)?;
            if paths.len() > 2049 {
                bail!("speculation cohort object count exceeds inspection bound");
            }
            // Bound aggregate bytes BEFORE the store inspector materializes any record/image data.
            for path in &paths {
                let resource = crate::core::workspace_txn::NativeResource::resolve(path)?;
                scan_bytes = scan_bytes
                    .checked_add(fs::symlink_metadata(path)?.len())
                    .context("speculation inspection byte overflow")?;
                if scan_bytes > MAX_SPECULATION_INSPECTION_BYTES {
                    bail!("speculation namespace inspection exceeds 64 MiB; evidence retained");
                }
                resource.verify()?;
            }
            // Nonblocking ownership probe in the store inspector; never wait for a worker under TM locks.
            let records = crate::core::speculation::store::inspect_controller_records(&directory)
                .context("cannot inspect speculative cohort (active owner or corrupt/incomplete evidence); let workers exit before retrying")?;
            let mut bases = 0usize;
            for path in paths {
                let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("m-"))
                {
                    // The store inspector checks content addresses/images. Check every envelope too:
                    // its data-only API deliberately skips non-controller manifests, including future kinds.
                    let bytes = read_speculation_evidence(&path, 2 * 1024 * 1024)?;
                    let value: serde_json::Value =
                        serde_json::from_slice(&bytes).context("corrupt speculation manifest")?;
                    if evidence_text(&value, "format")? != "aizen-speculation-v1"
                        || evidence_text(&value, "base")? != base
                    {
                        bail!("future or mismatched speculation manifest; evidence retained");
                    }
                    match evidence_text(&value, "kind")? {
                        "base" => bases += 1,
                        "overlay" | "controller-record" => {}
                        _ => bail!("unsupported speculation manifest kind; evidence retained"),
                    }
                }
                resource.verify()?;
            }
            if bases != 1 {
                bail!("speculation cohort has missing/ambiguous base evidence");
            }
            let (prepared, verified, concerns) = classify_integration_records(base, &records)?;
            summary.prepared += prepared;
            summary.verified += verified;
            summary.read_dependency_concerns += concerns;
        }
    }
    Ok(summary)
}

fn require_settled_speculation(namespace: &Path) -> Result<SpeculationInspection> {
    // Reopening metadata never recovers authority, even for the coordinating parent after restart.
    for name in ["ledger.json", "journal.json", "transaction.lock"] {
        let path = namespace.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            crate::core::workspace_txn::NativeResource::resolve(&path)?.verify()?;
        }
    }
    inspect_speculation_namespace(namespace).with_context(|| format!(
        "Time Machine mutation refused: speculative integration evidence at {} is unresolved or unsafe. \
         Source and evidence were retained; no automatic rewrite/rollback ran. Inspect `aizen time doctor` \
         or `checkpoint_view action=list`; selective reconciliation is not implemented. Do not delete the evidence to bypass this guard",
        namespace.display(),
    ))
}

/// Whole-store operations hold store-exclusive first. Each actual transaction namespace is probed
/// without waiting; controllers must publish through TimeMachineBinding::with_transaction_lock.
fn require_settled_store_speculation(root: &Path, already_locked: Option<&Path>) -> Result<()> {
    let namespaces = checked_metadata_entries(&root.join("worktrees"))?;
    if namespaces.len() > MAX_SPECULATION_WORKTREES {
        bail!("speculation worktree traversal exceeds bound");
    }
    for namespace in namespaces {
        let boundary = crate::core::workspace_txn::NativeResource::resolve(
            &namespace.join(".tm-inspection-boundary"),
        )?;
        let _transaction = if Some(namespace.as_path()) == already_locked {
            None
        } else {
            let lock = crate::core::workspace_txn::NativeResource::resolve(
                &namespace.join("transaction.lock"),
            )?;
            let guard = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                lock.path(),
                Duration::ZERO,
            )?;
            lock.verify()?;
            Some(guard)
        };
        require_settled_speculation(&namespace)?;
        boundary.verify()?;
    }
    Ok(())
}

/// Presence, not successful parsing, protects evidence from whole-store/trash deletion. Completed
/// records are immutable history too. Unknown versions/corrupt directories must never become garbage.
fn store_has_speculation_evidence(root: &Path) -> Result<bool> {
    for path in checked_metadata_entries(root)? {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("speculation-"))
        {
            return Ok(true);
        }
    }
    let namespaces = checked_metadata_entries(&root.join("worktrees"))?;
    if namespaces.len() > MAX_SPECULATION_WORKTREES {
        bail!("speculation worktree traversal exceeds bound");
    }
    for namespace in namespaces {
        for path in checked_metadata_entries(&namespace)? {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("speculation-"))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn trash_has_speculation_evidence(stamp: &Path) -> Result<bool> {
    for store in checked_metadata_entries(stamp)? {
        if !store
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("repo-"))
        {
            bail!("unrecognized trash entry; cannot rule out speculation evidence");
        }
        if store_has_speculation_evidence(&store)? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl RepoContext {
    fn pin_path(&self, id: u32) -> PathBuf {
        self.namespace_dir.join("pins").join(format!("{id}.lock"))
    }

    fn pin(&self, snapshot: Snapshot) -> Result<SnapshotHandle> {
        self.validate_snapshot_ref(&snapshot)?;
        let pin = crate::core::repo_lock::RepoTxnLock::acquire_shared(
            &self.pin_path(snapshot.id),
            LOCK_TIMEOUT,
        )?;
        Ok(SnapshotHandle {
            binding: RepoBinding::from_context(self),
            snapshot,
            restore_authorized: crate::core::exec_ctx::current()
                .is_none_or(|ctx| ctx.can_restore_workspace()),
            _pin: std::sync::Arc::new(SnapshotPin { _lock: pin }),
        })
    }

    /// Probe while holding metadata locks and retain exclusive guards through deletion.
    fn deletion_pins(
        &self,
        ids: impl IntoIterator<Item = u32>,
    ) -> Result<(Vec<u32>, Vec<crate::core::repo_lock::RepoTxnLock>)> {
        let mut protected = Vec::new();
        let mut guards = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            // A second descriptor would conflict with our own retained exclusive guard.
            if !seen.insert(id) {
                continue;
            }
            match crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                &self.pin_path(id),
                Duration::ZERO,
            ) {
                Ok(guard) => guards.push(guard),
                Err(error)
                    if error
                        .downcast_ref::<crate::core::repo_lock::LockBusy>()
                        .is_some() =>
                {
                    protected.push(id)
                }
                Err(error) => return Err(error).context("probing live checkpoint pin"),
            }
        }
        Ok((protected, guards))
    }

    fn discover(start: &Path) -> Result<Self> {
        // Only a *genuine* absence of a work tree earns the friendly "run `git init`" message.
        // Any other git failure — dubious ownership, safe.directory, git missing — is a real,
        // usually one-command-fixable problem, so surface git's own stderr verbatim rather than
        // telling the user to run a command they already ran. Callers keying off the
        // "not a git repository" substring (save_protected_change) still treat the benign
        // case as "checkpoints simply off", while real errors now propagate with their true cause.
        let root = match raw_git(start, ["rev-parse", "--show-toplevel"]) {
            Ok(r) if !r.trim().is_empty() => r,
            // Bare repo or empty toplevel → no work tree to checkpoint; treat as no-TM.
            Ok(_) => bail!("not a git repository (run `git init` first to use the time machine)"),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not a git repository") || msg.contains("not a work tree") {
                    bail!("not a git repository (run `git init` first to use the time machine)");
                }
                // git absent ≠ git failed: keep the typed GitMissing chain untouched so the
                // benign-path callers can recognize it without substring guessing.
                if crate::core::gitx::is_git_missing(&e) {
                    return Err(e);
                }
                return Err(e).context("time machine could not use git in this directory");
            }
        };
        let root = PathBuf::from(root)
            .canonicalize()
            .context("canonicalizing repository root")?;
        let git_dir = absolute_git_path(&root, &raw_git(&root, ["rev-parse", "--git-dir"])?);
        let common_git_dir =
            absolute_git_path(&root, &raw_git(&root, ["rev-parse", "--git-common-dir"])?);
        let common_canon =
            fs::canonicalize(&common_git_dir).unwrap_or_else(|_| common_git_dir.clone());
        let wt_canon = fs::canonicalize(&git_dir).unwrap_or_else(|_| git_dir.clone());

        let home = crate::core::config::aizen_home();
        // Native admission refuses an alias whose canonical parent differs from its spelling, and
        // `AIZEN_HOME` is taken verbatim (an 8.3 short name or mixed case is common on Windows).
        // Bind one canonical spelling here so every metadata path derived below agrees with the
        // guard instead of failing on the user's first checkpoint. `discover` already creates the
        // store directories under this home.
        fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
        let home = fs::canonicalize(&home)
            .map(|path| strip_windows_verbatim(&path))
            .unwrap_or(home);
        // Stable identity via the home-level registry (survives repo moves; distinguishes clones).
        // Fails open to the legacy `repo-<fnv(path)>` ids if the registry is unavailable.
        let (repo_id, worktree_id) = resolve_identity(&home, &common_canon, &wt_canon, &root)?;
        let repo_store_root = home.join("timemachine").join(&repo_id);
        let store_git_dir = repo_store_root.join("store.git");
        let namespace_dir = repo_store_root.join("worktrees").join(&worktree_id);
        let hooks_dir = namespace_dir.join("empty-hooks");

        fs::create_dir_all(&hooks_dir)
            .with_context(|| format!("creating {}", hooks_dir.display()))?;
        crate::core::config::harden_dir(&repo_store_root);
        crate::core::config::harden_dir(&namespace_dir);
        ensure_private_store(&store_git_dir, &common_git_dir)?;

        Ok(Self {
            root,
            git_dir: wt_canon,
            common_git_dir: common_canon,
            store_git_dir,
            namespace_dir,
            repo_id,
            ref_prefix: format!("refs/ng/tm/{worktree_id}"),
            worktree_id,
            hooks_dir,
            filters_checked: std::cell::Cell::new(false),
            reparse_checked: std::cell::Cell::new(false),
            skip_dirs: std::cell::RefCell::new(None),
        })
    }

    fn current() -> Result<Self> {
        // Prefer the session's workspace root (where the edit will land) over the harness's cwd,
        // which may be the user's home directory. Without this, a pre-edit checkpoint spawned from
        // `C:\Users\admin` probes git there, gets "not a git repository", and blocks the edit.
        //
        // The harness sets `AIZEN_WORKSPACE_ROOT` when it knows the target project. Fall back to
        // cwd, then to walking up from cwd looking for `.git` (handles nested non-repo dirs).
        if let Ok(root) = std::env::var("AIZEN_WORKSPACE_ROOT") {
            return Self::discover(Path::new(&root));
        }
        let cwd = std::env::current_dir().context("resolving cwd")?;
        Self::discover(&cwd)
    }

    fn ledger_path(&self) -> PathBuf {
        self.namespace_dir.join("ledger.json")
    }

    /// Previous hardening location: `<common-git-dir>/aizen-timemachine/<worktree-id>/ledger.json`.
    fn inrepo_namespace_ledger_path(&self) -> PathBuf {
        self.common_git_dir
            .join("aizen-timemachine")
            .join(&self.worktree_id)
            .join("ledger.json")
    }

    fn inrepo_namespace_dir(&self) -> PathBuf {
        self.common_git_dir
            .join("aizen-timemachine")
            .join(&self.worktree_id)
    }

    /// Oldest legacy ledger: `<git-dir>/ng_timemachine.json`.
    fn legacy_ledger_path(&self) -> PathBuf {
        self.git_dir.join("ng_timemachine.json")
    }

    fn journal_path(&self) -> PathBuf {
        self.namespace_dir.join("journal.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.namespace_dir.join("transaction.lock")
    }

    fn chat_dir(&self) -> PathBuf {
        self.namespace_dir.join("chat")
    }

    fn chat_path(&self, id: u32) -> PathBuf {
        self.chat_dir().join(format!("{id}.json"))
    }

    fn inrepo_chat_path(&self, id: u32) -> PathBuf {
        self.inrepo_namespace_dir()
            .join("chat")
            .join(format!("{id}.json"))
    }

    fn legacy_chat_path(&self, id: u32) -> PathBuf {
        self.git_dir.join(format!("ng_tm_chat_{id}.json"))
    }

    fn ref_name(&self, id: u32) -> String {
        format!("{}/{id}", self.ref_prefix)
    }

    fn recovery_ref_name(&self, id: u32) -> String {
        format!("{}/recovery/{id}", self.ref_prefix)
    }

    fn temp_index(&self) -> PathBuf {
        let dir = strip_windows_verbatim(&self.namespace_dir);
        dir.join(format!(
            "index-{}-{}",
            std::process::id(),
            TM_INDEX_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn git<I, S>(&self, index: Option<&Path>, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git_output(index, args)?;
        if !output.status.success() {
            bail!(
                "internal git operation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run a plumbing command against the **private** store + source worktree.
    fn git_output<I, S>(&self, index: Option<&Path>, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.git_cmd_for(index, args);
        run_git_bounded(&mut cmd, "internal git operation")
    }

    /// Same command hygiene as [`RepoContext::git_output`], but feeds `stdin_bytes` to the process.
    /// Used by [`RepoContext::compact_objects`], which hands `pack-objects` an object list far too
    /// long for an argument vector.
    fn git_output_stdin<I, S>(&self, args: I, stdin_bytes: &[u8], what: &str) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.git_cmd_for(None, args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_git_piped_bounded(&mut cmd, stdin_bytes, what)
    }

    /// The plumbing `Command` shared by both runners: private store as `GIT_DIR`, source tree as the
    /// work tree, and every hook/filter/config channel that could run repository-controlled code shut
    /// off before an approval gate.
    fn git_cmd_for<I, S>(&self, index: Option<&Path>, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = git_cmd();
        let hooks = strip_windows_verbatim(&self.hooks_dir)
            .to_string_lossy()
            .replace('\\', "/");
        let store = strip_windows_verbatim(&self.store_git_dir);
        let work_tree = strip_windows_verbatim(&self.root);
        cmd.current_dir(&self.root)
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .env("GIT_DIR", &store)
            .env("GIT_WORK_TREE", &work_tree)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Aizen Time Machine")
            .env("GIT_AUTHOR_EMAIL", "timemachine@localhost")
            .env("GIT_COMMITTER_NAME", "Aizen Time Machine")
            .env("GIT_COMMITTER_EMAIL", "timemachine@localhost")
            .arg("-c")
            .arg(format!("core.hooksPath={hooks}"))
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
            ])
            .args([
                "-c",
                "filter.lfs.required=false",
                "-c",
                "filter.lfs.clean=",
                "-c",
                "filter.lfs.smudge=",
                "-c",
                "filter.lfs.process=",
                "-c",
                "filter.lfs.delay=false",
            ])
            .args(args);
        if let Some(idx) = index {
            cmd.env("GIT_INDEX_FILE", idx);
        } else {
            cmd.env_remove("GIT_INDEX_FILE");
        }
        cmd
    }

    /// Probe the **source** repository for external filter drivers (not the private store).
    fn source_git_output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = git_cmd();
        let git_dir = strip_windows_verbatim(&self.git_dir);
        let work_tree = strip_windows_verbatim(&self.root);
        cmd.current_dir(&self.root)
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", &work_tree)
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .args(args);
        run_git_bounded(&mut cmd, "source git probe")
    }

    fn source_git<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.source_git_output(args)?;
        if !output.status.success() {
            bail!(
                "source git probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn ensure_safe_filters(&self) -> Result<()> {
        if self.filters_checked.get() {
            return Ok(());
        }
        // `.gitattributes` can name arbitrary clean/smudge/process drivers. Git offers no global
        // "disable every filter" switch, so reject configured external drivers before plumbing
        // commands touch the worktree. The built-in `lfs` name is neutralized in `git_output`.
        let out = self
            .source_git_output([
                "config",
                "--local",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process|required)$",
            ])
            .context("checking repository Git filters")?;
        if out.status.success() {
            let lines = String::from_utf8_lossy(&out.stdout);
            for line in lines.lines() {
                let key = line.split_whitespace().next().unwrap_or("");
                if !key.starts_with("filter.lfs.") {
                    bail!(
                        "checkpoint refused: repository config defines external Git filter `{key}`; disable it or snapshot manually"
                    );
                }
            }
        }
        self.filters_checked.set(true);
        Ok(())
    }

    /// Take the per-worktree metadata lock, observing the turn's cancel token.
    ///
    /// Cancel-aware rather than a blind 15s spin: the token is checked between non-blocking OS
    /// attempts, so Esc during contention returns at once instead of making the user wait out
    /// `LOCK_TIMEOUT` before the tool reports failure.
    fn lock(&self) -> Result<crate::core::repo_lock::RepoTxnLock> {
        // Check before opening: RepoTxnLock itself follows paths and is not a metadata validator.
        let (_, parent) = plan_metadata_directory(&self.namespace_dir)?;
        let path = self.lock_path();
        if fs::symlink_metadata(&self.namespace_dir).is_ok() {
            crate::core::workspace_txn::NativeResource::resolve(&path)?;
        }
        let cancel = crate::core::cancel::current();
        let lock = crate::core::repo_lock::RepoTxnLock::acquire_mode(
            &path,
            crate::core::repo_lock::LockMode::Exclusive,
            LOCK_TIMEOUT,
            cancel.as_ref(),
        )?;
        parent.verify()?;
        crate::core::workspace_txn::NativeResource::resolve(&path)?.verify()?;
        Ok(lock)
    }

    /// Repository-store lock path, shared by ALL linked worktrees (one level above the per-worktree
    /// namespace). Ref reads/writes take it SHARED so sibling worktrees coexist; a store-wide sweep
    /// (`doctor_gc`, which walks every worktree's `refs/ng/tm/**`) takes it EXCLUSIVE so it can never
    /// race a sibling `save` mid-scan. Ordered before the per-worktree metadata lock (`lock`).
    fn store_lock_path(&self) -> PathBuf {
        self.store_git_dir
            .parent()
            .map(|p| p.join("store.lock"))
            .unwrap_or_else(|| self.store_git_dir.join("store.lock"))
    }

    /// Shared store lease held by ordinary ref-mutating ops (save/restore/prune/clear) so they can
    /// run concurrently across linked worktrees while still blocking a store-exclusive GC sweep.
    fn store_shared(&self) -> Result<crate::core::repo_lock::RepoTxnLock> {
        self.checked_store_lock(crate::core::repo_lock::LockMode::Shared)
    }

    fn checked_store_lock(
        &self,
        mode: crate::core::repo_lock::LockMode,
    ) -> Result<crate::core::repo_lock::RepoTxnLock> {
        let path = self.store_lock_path();
        let parent_path = path
            .parent()
            .context("Time Machine store lock has no parent")?;
        let (_, parent) = plan_metadata_directory(parent_path)?;
        if fs::symlink_metadata(parent_path).is_ok() {
            crate::core::workspace_txn::NativeResource::resolve(&path)?;
        }
        let cancel = crate::core::cancel::current();
        let lock = crate::core::repo_lock::RepoTxnLock::acquire_mode(
            &path,
            mode,
            LOCK_TIMEOUT,
            cancel.as_ref(),
        )?;
        parent.verify()?;
        crate::core::workspace_txn::NativeResource::resolve(&path)?.verify()?;
        Ok(lock)
    }

    /// Exclusive store lease for a whole-store sweep (`doctor_gc`): no other worktree may create or
    /// delete refs while it enumerates and reaps orphans across every namespace.
    fn store_exclusive(&self) -> Result<crate::core::repo_lock::RepoTxnLock> {
        self.checked_store_lock(crate::core::repo_lock::LockMode::Exclusive)
    }

    fn load_ledger(&self) -> Result<Ledger> {
        let path = self.ledger_path();
        if fs::symlink_metadata(&path).is_ok() {
            crate::core::workspace_txn::NativeResource::resolve(&path)?.verify()?;
        }
        let bytes = match crate::core::persist::read_optional(&path)
            .with_context(|| format!("reading time-machine ledger {}", path.display()))?
        {
            Some(bytes) => bytes,
            None => return self.load_migratable_ledger(),
        };
        let mut ledger: Ledger = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "time-machine ledger {} is corrupt; run `aizen time doctor`",
                path.display()
            )
        })?;
        ledger.normalize()?;
        Ok(ledger)
    }

    /// Read-only fallback: in-repo namespaced ledger, then oldest legacy ledger. Migration is
    /// finalized later under the transaction lock by `migrate_legacy_refs`.
    fn load_migratable_ledger(&self) -> Result<Ledger> {
        for path in [
            self.inrepo_namespace_ledger_path(),
            self.legacy_ledger_path(),
        ] {
            let Some(bytes) = crate::core::persist::read_optional(&path).with_context(|| {
                format!("reading migratable time-machine ledger {}", path.display())
            })?
            else {
                continue;
            };
            let mut ledger: Ledger = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "migratable time-machine ledger {} is corrupt; it was not replaced",
                    path.display()
                )
            })?;
            ledger.normalize()?;
            return Ok(ledger);
        }
        Ok(Ledger::default())
    }

    fn save_ledger(&self, ledger: &mut Ledger) -> Result<()> {
        ledger.normalize()?;
        ledger.generation = ledger.generation.saturating_add(1);
        let bytes = serde_json::to_vec_pretty(ledger)?;
        crate::core::persist::atomic_write(&self.ledger_path(), &[bytes, b"\n".to_vec()].concat())?;
        crate::core::persist::harden_owner_only_checked(&self.ledger_path())?;
        Ok(())
    }

    fn load_journal(&self) -> Result<Option<Journal>> {
        let path = self.journal_path();
        if fs::symlink_metadata(&path).is_ok() {
            crate::core::workspace_txn::NativeResource::resolve(&path)?.verify()?;
        }
        let Some(bytes) = crate::core::persist::read_optional(&path)
            .with_context(|| format!("reading transaction journal {}", path.display()))?
        else {
            return Ok(None);
        };
        let journal: Journal = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "transaction journal {} is corrupt; run `aizen time doctor`",
                path.display()
            )
        })?;
        if journal.schema_version != 1 {
            bail!("unsupported Time Machine journal schema; recovery evidence retained");
        }
        Ok(Some(journal))
    }

    fn save_journal(&self, journal: &Journal) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(journal)?;
        bytes.push(b'\n');
        crate::core::persist::atomic_write(&self.journal_path(), &bytes)?;
        crate::core::persist::harden_owner_only_checked(&self.journal_path())?;
        Ok(())
    }

    fn clear_journal(&self) -> Result<()> {
        crate::core::persist::remove_if_exists(&self.journal_path()).with_context(|| {
            format!(
                "removing transaction journal {}",
                self.journal_path().display()
            )
        })?;
        Ok(())
    }

    fn migrate_legacy_refs(&self, ledger: &mut Ledger) -> Result<()> {
        // Already on the external store — nothing to import.
        if self.ledger_path().exists() {
            return Ok(());
        }
        let has_inrepo = self.inrepo_namespace_ledger_path().exists();
        let has_legacy = self.legacy_ledger_path().exists();
        if !has_inrepo && !has_legacy && ledger.snapshots.is_empty() {
            return Ok(());
        }

        for snap in &mut ledger.snapshots {
            let new_ref = self.ref_name(snap.id);
            let existing = self.git(None, ["rev-parse", "--verify", &new_ref]).ok();
            if let Some(oid) = existing.as_deref() {
                if oid != snap.commit {
                    bail!(
                        "cannot migrate checkpoint #{}: private store ref already exists with another object",
                        snap.id
                    );
                }
            } else {
                // Prefer the previous namespaced in-repo ref, then the oldest flat ref. Objects are
                // still reachable via the sealed alternates pointer into the source object store.
                let candidates = [
                    format!("refs/ng/tm/{}/{}", self.worktree_id, snap.id),
                    format!("refs/ng/tm/{}", snap.id),
                ];
                let mut imported = false;
                for candidate in &candidates {
                    // Look up the candidate in the *source* repo first; if present, create the same
                    // ref name in the private store (objects resolve via alternates).
                    if let Ok(oid) = self.source_git(["rev-parse", "--verify", candidate.as_str()])
                    {
                        if oid != snap.commit {
                            bail!(
                                "cannot migrate checkpoint #{}: source ref {candidate} does not match ledger",
                                snap.id
                            );
                        }
                        // Materialize the object into the private store so the timeline survives if
                        // the source repository is later rewritten or deleted.
                        self.materialize_commit(&oid)?;
                        self.update_ref_create(&new_ref, &oid)?;
                        imported = true;
                        break;
                    }
                    // Also accept a ref already present in the private store under a temporary
                    // migration name (e.g. after a partial previous attempt).
                    if let Ok(oid) = self.git(None, ["rev-parse", "--verify", candidate.as_str()]) {
                        if oid != snap.commit {
                            bail!(
                                "cannot migrate checkpoint #{}: private candidate {candidate} does not match ledger",
                                snap.id
                            );
                        }
                        if candidate != &new_ref {
                            self.update_ref_create(&new_ref, &oid)?;
                        }
                        imported = true;
                        break;
                    }
                }
                if !imported {
                    // Last resort: object may still be reachable through alternates by OID alone.
                    if self
                        .git(
                            None,
                            ["cat-file", "-e", &format!("{}^{{commit}}", snap.commit)],
                        )
                        .is_ok()
                    {
                        self.materialize_commit(&snap.commit)?;
                        self.update_ref_create(&new_ref, &snap.commit)?;
                    } else {
                        bail!(
                            "cannot migrate checkpoint #{}: source ref/object is missing",
                            snap.id
                        );
                    }
                }
            }

            // Prefer newest chat location first.
            for old_chat in [
                self.inrepo_chat_path(snap.id),
                self.legacy_chat_path(snap.id),
            ] {
                if old_chat.exists() && !self.chat_path(snap.id).exists() {
                    fs::create_dir_all(self.chat_dir())?;
                    fs::copy(&old_chat, self.chat_path(snap.id)).with_context(|| {
                        format!("migrating conversation sidecar for checkpoint #{}", snap.id)
                    })?;
                    crate::core::persist::harden_owner_only_checked(&self.chat_path(snap.id))?;
                    break;
                }
            }
            snap.worktree_id = self.worktree_id.clone();
        }
        Ok(())
    }

    /// Copy a commit (and its reachable trees/blobs) from alternates into the private object store
    /// so checkpoints remain independent of the source repository's lifetime.
    fn materialize_commit(&self, commit: &str) -> Result<()> {
        // `cat-file --batch-check` validates reachability; `repack -a -d --window=0` is too heavy.
        // Use `pack-objects` of a single commit via stdin — pure plumbing, no hooks.
        let mut child = git_cmd();
        let store = strip_windows_verbatim(&self.store_git_dir);
        let hooks = strip_windows_verbatim(&self.hooks_dir)
            .to_string_lossy()
            .replace('\\', "/");
        child
            .current_dir(&self.root)
            .env("GIT_DIR", &store)
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .arg("-c")
            .arg(format!("core.hooksPath={hooks}"))
            .args(["-c", "core.fsmonitor=false"])
            .args([
                "pack-objects",
                "--revs",
                "--all-progress-implied",
                "-q",
                "--stdout",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Prefer a lighter path: `git fetch` from self is awkward for bare. Instead, ensure the
        // object is present via `cat-file` (alternates) then write a thin local ref pin — objects
        // stay in alternates until `doctor_gc` optionally copies. For independence, use
        // `git unpack-objects` of a generated pack.
        // rev-list style input: the commit, then a blank line to terminate. Bounded so a wedged
        // `pack-objects` cannot park this thread while `transaction.lock` is held.
        let pack_input = format!("{commit}\n\n");
        let pack_out = run_git_piped_bounded(
            &mut child,
            pack_input.as_bytes(),
            "private-store pack-objects",
        )?;
        if !pack_out.status.success() {
            // Fall back: object remains reachable via alternates. Migration still creates the ref;
            // independence is best-effort when pack-objects cannot run (e.g. empty tree edge).
            let _ = self.git(None, ["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
            return Ok(());
        }
        if pack_out.stdout.is_empty() {
            let _ = self.git(None, ["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
            return Ok(());
        }
        let mut unpack = git_cmd();
        unpack
            .current_dir(&self.root)
            .env("GIT_DIR", &store)
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .args(["unpack-objects", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let out = run_git_piped_bounded(
            &mut unpack,
            &pack_out.stdout,
            "unpack-objects for materialize",
        )?;
        if !out.status.success() {
            // Alternates still provide reachability; ref create below is enough for restore.
            let _ = self.git(None, ["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
        }
        Ok(())
    }

    /// Every loose object this store PHYSICALLY owns, as full OIDs.
    ///
    /// Read off the filesystem rather than asked of git, because that is the one enumeration with no
    /// reachability walk in it and no dependence on alternates: `objects/<2 hex>/<rest hex>` is a
    /// loose object of this store and nothing else. `pack/`, `info/`, and git's `tmp_obj_*` scratch
    /// files fail the hex test and are skipped.
    fn loose_object_ids(&self) -> Vec<String> {
        let objects = self.store_git_dir.join("objects");
        let mut oids = Vec::new();
        let Ok(fanout) = fs::read_dir(&objects) else {
            return oids;
        };
        for dir in fanout.flatten() {
            let name = dir.file_name();
            let Some(prefix) = name.to_str() else {
                continue;
            };
            if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(rd) = fs::read_dir(dir.path()) else {
                continue;
            };
            for obj in rd.flatten() {
                let rest = obj.file_name();
                let Some(rest) = rest.to_str() else { continue };
                if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                oids.push(format!("{prefix}{rest}"));
            }
        }
        oids
    }

    /// Objects in the store's OWN packs, read from each `.idx` with `show-index` — like
    /// [`RepoContext::loose_object_ids`], an enumeration with no walk and no alternates in it.
    fn packed_object_ids(&self) -> Vec<String> {
        let pack_dir = self.store_git_dir.join("objects").join("pack");
        let mut oids = Vec::new();
        let Ok(rd) = fs::read_dir(&pack_dir) else {
            return oids;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "idx") {
                continue;
            }
            let Ok(bytes) = fs::read(&p) else {
                continue;
            };
            let Ok(out) = self.git_output_stdin(["show-index"], &bytes, "private-store show-index")
            else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(oid) = line.split_whitespace().nth(1) {
                    oids.push(oid.to_string());
                }
            }
        }
        oids
    }

    /// Objects reachable from the store's refs. `--missing=allow-any` keeps the walk alive across
    /// a parent that lives only in the source repository — the case that kills `git gc` here.
    fn reachable_object_ids(&self) -> Result<HashSet<String>> {
        let out = self.git(
            None,
            ["rev-list", "--objects", "--missing=allow-any", "--all"],
        )?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_string)
            .collect())
    }

    /// Drop every object the store owns that no ref reaches (E5.4, quality plan S8: retention
    /// deleted refs, never bytes, so a store only ever grew). Selection is by OID, never by the
    /// history walk the alternates cannot complete: the reachable set comes from a walk that
    /// tolerates a missing parent, the owned set from the filesystem, and what is kept is
    /// re-packed from that list before anything is deleted.
    fn prune_unreachable(&self) -> Result<PruneReport> {
        let objects = self.store_git_dir.join("objects");
        let before_bytes = dir_size_bytes(&objects);
        let owned: HashSet<String> = self
            .loose_object_ids()
            .into_iter()
            .chain(self.packed_object_ids())
            .collect();
        let reachable = self.reachable_object_ids()?;
        let drop: Vec<&String> = owned.iter().filter(|o| !reachable.contains(*o)).collect();
        if drop.is_empty() {
            return Ok(PruneReport {
                owned: owned.len(),
                pruned: 0,
                before_bytes,
                after_bytes: before_bytes,
            });
        }
        let keep: Vec<&String> = owned.iter().filter(|o| reachable.contains(*o)).collect();
        let pack_dir = objects.join("pack");
        fs::create_dir_all(&pack_dir)
            .with_context(|| format!("creating pack dir {}", pack_dir.display()))?;
        let old_packs: Vec<PathBuf> = fs::read_dir(&pack_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "pack" || x == "idx"))
                    .collect()
            })
            .unwrap_or_default();
        // The kept objects go into one fresh pack, named by git after their checksum. When the
        // kept set equals an old pack's exactly, the names collide and the new pack IS the old
        // one — it must survive the sweep below, so its name is read back and excluded.
        let mut fresh_name: Option<String> = None;
        if !keep.is_empty() {
            let mut stdin = String::with_capacity(keep.len() * 41);
            for oid in &keep {
                stdin.push_str(oid);
                stdin.push('\n');
            }
            let pack_base = strip_windows_verbatim(&pack_dir.join("pack"))
                .to_string_lossy()
                .replace('\\', "/");
            let out = self.git_output_stdin(
                ["pack-objects", "--non-empty", "-q", &pack_base],
                stdin.as_bytes(),
                "private-store pack-objects (prune)",
            )?;
            if !out.status.success() {
                bail!(
                    "re-packing the reachable objects failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !hash.is_empty() {
                fresh_name = Some(format!("pack-{hash}"));
            }
        }
        // Only now: the packs listed BEFORE the fresh one, and the unreachable loose copies.
        for p in old_packs {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if fresh_name.as_deref() == Some(stem) {
                continue;
            }
            let _ = fs::remove_file(p);
        }
        for oid in &drop {
            if oid.len() > 2 {
                let _ = fs::remove_file(objects.join(&oid[..2]).join(&oid[2..]));
            }
        }
        let _ = self.git_output(None, ["prune-packed", "-q"]);
        Ok(PruneReport {
            owned: owned.len(),
            pruned: drop.len(),
            before_bytes,
            after_bytes: dir_size_bytes(&objects),
        })
    }

    /// Compact the store's loose objects into a pack.
    ///
    /// `git gc` and `git repack` cannot do this job here, and that is not a tuning problem — it is
    /// structural. Both select objects by WALKING history, and a checkpoint commit's parent often
    /// lives only in the source repository, reachable through the sealed alternates pointer. As soon
    /// as the source prunes that commit — or is moved, renamed, or deleted — the walk dies with
    /// `failed to traverse parents of commit <oid>` and repack aborts having packed nothing. Retention
    /// still deletes refs, so the ledger looks tidy while not one byte is ever reclaimed. Measured on
    /// the author's machine before this fix: 14,868 loose objects and 0 packs backing 30 live
    /// checkpoints, 88 MB across 17,336 files.
    ///
    /// So select by OID instead of by reachability. [`RepoContext::loose_object_ids`] lists exactly
    /// what the store owns, and `pack-objects` takes that list on stdin without walking anything, so a
    /// dangling parent cannot abort it. `prune-packed` then drops the loose copies — it removes only
    /// objects it has confirmed are in a pack, so an interrupted run leaves duplicates, never a gap.
    ///
    /// Unreachable objects are preserved on purpose: they are the recovery evidence an interrupted
    /// prune or restore leaves behind, and `doctor` reports them. This reclaims space, it does not
    /// decide what is garbage.
    ///
    /// Caller must hold the store lease. Errors are the caller's to swallow — failing to compact is
    /// never a reason to fail the operation that triggered it.
    fn compact_objects(&self) -> Result<CompactReport> {
        let objects = self.store_git_dir.join("objects");
        let before_bytes = dir_size_bytes(&objects);
        let oids = self.loose_object_ids();
        if oids.is_empty() {
            return Ok(CompactReport {
                packed: 0,
                before_bytes,
                after_bytes: before_bytes,
            });
        }
        let mut stdin = String::with_capacity(oids.len() * 41);
        for oid in &oids {
            stdin.push_str(oid);
            stdin.push('\n');
        }
        // Write into the store's own pack dir. `pack-objects <base>` renames the finished pack into
        // place, so a crash mid-write leaves a `tmp_pack_*` git itself ignores, not a torn pack.
        let pack_base = strip_windows_verbatim(&objects.join("pack").join("pack"))
            .to_string_lossy()
            .replace('\\', "/");
        fs::create_dir_all(objects.join("pack"))
            .with_context(|| format!("creating pack dir {}", objects.join("pack").display()))?;
        let out = self.git_output_stdin(
            ["pack-objects", "--non-empty", "-q", &pack_base],
            stdin.as_bytes(),
            "private-store pack-objects (compact)",
        )?;
        if !out.status.success() {
            bail!(
                "compacting the time-machine object store failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // Only now are the loose copies redundant. `prune-packed` re-verifies each one is packed
        // before unlinking it, so this cannot outrun the write above.
        let pruned = self.git_output(None, ["prune-packed", "-q"])?;
        if !pruned.status.success() {
            bail!(
                "dropping packed-away loose objects failed: {}",
                String::from_utf8_lossy(&pruned.stderr).trim()
            );
        }
        Ok(CompactReport {
            packed: oids.len(),
            before_bytes,
            after_bytes: dir_size_bytes(&objects),
        })
    }

    fn update_ref_create(&self, name: &str, oid: &str) -> Result<()> {
        self.git(None, ["update-ref", name, oid, ZERO_OID])
            .map(|_| ())
    }

    fn update_ref_delete(&self, name: &str, expected: &str) -> Result<()> {
        self.git(None, ["update-ref", "-d", name, expected])
            .map(|_| ())
    }

    fn validate_snapshot_ref(&self, snap: &Snapshot) -> Result<()> {
        let expected = &snap.commit;
        let new_ref = self.ref_name(snap.id);
        let actual = self
            .git(None, ["rev-parse", "--verify", &new_ref])
            .or_else(|_| {
                // Migration windows may still resolve the in-repo namespaced form via alternates.
                self.source_git([
                    "rev-parse",
                    "--verify",
                    &format!("refs/ng/tm/{}/{}", self.worktree_id, snap.id),
                ])
                .or_else(|_| {
                    self.source_git(["rev-parse", "--verify", &format!("refs/ng/tm/{}", snap.id)])
                })
            })?;
        if &actual != expected {
            bail!(
                "checkpoint #{} ref does not match its ledger commit",
                snap.id
            );
        }
        self.git(
            None,
            ["cat-file", "-e", &format!("{}^{{commit}}", snap.commit)],
        )?;
        self.git(None, ["cat-file", "-e", &format!("{}^{{tree}}", snap.tree)])?;
        Ok(())
    }

    fn recover_pending(&self, ledger: &mut Ledger) -> Result<()> {
        self.recover_pending_with_restore(ledger, false)
    }

    /// Automatic checkpoint/retention paths settle metadata only. Tree recovery is an explicit
    /// coordinating-parent operation under workspace exclusion, never a side effect of saving.
    fn recover_pending_with_restore(&self, ledger: &mut Ledger, restore: bool) -> Result<()> {
        // This precedes even the no-journal fast path: a speculative partial apply must not be
        // mistaken for an ordinary interrupted restore, nor lose refs/chat/recovery evidence.
        require_settled_speculation(&self.namespace_dir)?;
        let Some(journal) = self.load_journal()? else {
            return Ok(());
        };
        if journal.expected_generation > ledger.generation {
            bail!("time-machine journal belongs to a future ledger generation; run `aizen time doctor`");
        }
        match journal.kind {
            JournalKind::Save => {
                let id = journal
                    .checkpoint_id
                    .context("save journal is missing checkpoint id")?;
                let committed = ledger.snapshots.iter().any(|s| s.id == id);
                if committed {
                    self.clear_journal()?;
                    return Ok(());
                }
                if let (Some(name), Some(oid)) =
                    (journal.ref_name.as_deref(), journal.new_oid.as_deref())
                {
                    if self
                        .git(None, ["rev-parse", "--verify", name])
                        .ok()
                        .as_deref()
                        == Some(oid)
                    {
                        self.update_ref_delete(name, oid)?;
                    }
                }
                let _ = crate::core::persist::remove_if_exists(&self.chat_path(id));
                self.clear_journal()?;
            }
            JournalKind::Restore => {
                let target_id = journal
                    .target_id
                    .context("restore journal is missing target id")?;
                let target = ledger
                    .snapshots
                    .iter()
                    .find(|s| s.id == target_id)
                    .cloned()
                    .with_context(|| {
                        format!("restore journal targets missing checkpoint #{target_id}")
                    })?;
                let current = current_tree(self).map(|(tree, _)| tree).ok();
                if current.as_deref() == Some(target.tree.as_str()) {
                    ledger.set_cursor(Some(target_id));
                    self.save_ledger(ledger)?;
                    self.clear_journal()?;
                    return Ok(());
                }
                if let Some(preimage_id) = journal.preimage_id {
                    let preimage = ledger
                        .snapshots
                        .iter()
                        .find(|s| s.id == preimage_id)
                        .cloned()
                        .with_context(|| {
                            format!("restore journal preimage checkpoint #{preimage_id} is missing")
                        })?;
                    if current.as_deref() == Some(preimage.tree.as_str()) {
                        ledger.set_cursor(Some(preimage_id));
                        self.save_ledger(ledger)?;
                        self.clear_journal()?;
                        return Ok(());
                    }
                    if !restore {
                        bail!("interrupted restore requires explicit parent reconciliation; workspace and recovery journal were retained, no automatic rollback ran");
                    }
                    require_workspace_restore()?;
                    apply_tree(self, &preimage.commit)
                        .context("rolling back an interrupted restore to its pinned preimage")?;
                    let rolled_back = current_tree(self)?.0;
                    if rolled_back != preimage.tree {
                        bail!("interrupted restore rollback did not reproduce its preimage; run `aizen time doctor`");
                    }
                    ledger.set_cursor(Some(preimage_id));
                    self.save_ledger(ledger)?;
                    self.clear_journal()?;
                    return Ok(());
                }
                bail!("interrupted restore has no recovery preimage; run `aizen time doctor`");
            }
            JournalKind::Prune | JournalKind::Clear | JournalKind::Doctor => {
                // Deletion operations commit the ledger before reaping refs. If interrupted, the
                // remaining refs are safe orphan recovery pins; clearing the journal unblocks normal
                // work and `doctor` reports the orphan set for explicit cleanup.
                self.clear_journal()?;
            }
        }
        Ok(())
    }
}

fn absolute_git_path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// Create (or repair) the private bare store under `~/.aizen/timemachine/<repo>/store.git`.
///
/// The store points at the source repository's object directory through a sealed `objects/info/alternates`
/// entry so historical source objects remain readable, while every new Time Machine object and ref is
/// written only into the private store.
fn ensure_private_store(store_git_dir: &Path, common_git_dir: &Path) -> Result<()> {
    if !store_git_dir.join("HEAD").exists() {
        fs::create_dir_all(store_git_dir).with_context(|| {
            format!(
                "creating private time-machine store {}",
                store_git_dir.display()
            )
        })?;
        let mut cmd = git_cmd();
        cmd.args(["init", "--bare", "-q"]).arg(store_git_dir);
        let out = run_git_bounded(&mut cmd, "initializing private time-machine store")?;
        if !out.status.success() {
            bail!(
                "failed to initialize private time-machine store: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        crate::core::config::harden_dir(store_git_dir);
    }

    // Seal alternates: one absolute path to the source object store. Rewrite on every open so a moved
    // worktree still resolves, and refuse unexpected extra entries that could smuggle objects.
    let objects = store_git_dir.join("objects");
    let info = objects.join("info");
    fs::create_dir_all(&info).with_context(|| format!("creating {}", info.display()))?;
    let alternates = info.join("alternates");
    let source_objects = {
        let cand = common_git_dir.join("objects");
        fs::canonicalize(&cand).unwrap_or(cand)
    };
    let source_line = strip_windows_verbatim(&source_objects)
        .to_string_lossy()
        .replace('\\', "/");
    let desired = format!("{source_line}\n");
    let current = crate::core::persist::read_optional(&alternates)
        .with_context(|| format!("reading {}", alternates.display()))?
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    if current.as_deref() != Some(desired.as_str()) {
        crate::core::persist::atomic_write(&alternates, desired.as_bytes())
            .with_context(|| format!("writing sealed alternates {}", alternates.display()))?;
        let _ = crate::core::persist::harden_owner_only_checked(&alternates);
    }

    // Neutralize config inside the private store itself (no shared hooks/fsmonitor).
    // Best-effort (the `let _`), but still BOUNDED: a wedged git here would hang store setup, which
    // runs on the pre-edit checkpoint path for every protected write.
    {
        let mut cmd = git_cmd();
        cmd.env("GIT_DIR", strip_windows_verbatim(store_git_dir))
            .args(["config", "core.hooksPath", "hooks-disabled"]);
        let _ = run_git_bounded(&mut cmd, "disabling hooks in private store");
    }
    let hooks_disabled = store_git_dir.join("hooks-disabled");
    let _ = fs::create_dir_all(&hooks_disabled);
    Ok(())
}

fn strip_windows_verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Builder for every git spawn in this module: the executable resolved by `core::gitx` (PATH or a
/// well-known install location), so the time machine keeps working in shells where git isn't on
/// PATH. Falls back to the literal name only when resolution says Missing — the spawn then fails
/// with the same ENOENT it always had, and `raw_git`/`discover` classify it as GitMissing.
fn git_cmd() -> Command {
    // Hardened like every internal git spawn: checkpointing a repository must never execute its
    // hooks/fsmonitor/credential helpers (see `gitx::harden`).
    crate::core::gitx::harden(match crate::core::gitx::git_exe() {
        Some(p) => Command::new(p),
        None => Command::new("git"),
    })
}

/// Per-invocation git deadline, overridable for slow trees / CI. See [`GIT_OP_TIMEOUT`].
fn git_op_timeout() -> Duration {
    std::env::var("AIZEN_GIT_OP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(GIT_OP_TIMEOUT)
}

/// Run a fully-configured git `Command` under a wall-clock deadline, with its whole process tree
/// contained, and shape the result like `Command::output()` so callers are unchanged.
///
/// This replaces every bare `cmd.output()` in this module. `Command::output()` blocks until every
/// pipe reaches EOF, and on Windows a grandchild that outlives its parent keeps the inherited write
/// end open, so EOF may never arrive — the caller then blocks forever WHILE HOLDING
/// `transaction.lock` (see [`GIT_OP_TIMEOUT`] for the full failure chain). Bytes are preserved
/// verbatim: `cat-file blob` output is file content, and `-z` output is NUL-delimited, so neither may
/// go through a lossy UTF-8 decode.
fn run_git_bounded(cmd: &mut Command, what: &str) -> Result<Output> {
    let timeout = git_op_timeout();
    let out = crate::core::proctree::output_bounded_bytes(cmd, timeout, GIT_DRAIN_GRACE)
        .with_context(|| format!("running {what} (is git installed?)"))?;
    if out.timed_out {
        bail!(
            "{what} exceeded {}s and was terminated (set AIZEN_GIT_OP_TIMEOUT_SECS to raise the \
             limit); nothing was changed",
            timeout.as_secs()
        );
    }
    Ok(Output {
        status: exit_status_from_code(out.code),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// Run a git `Command` that must be FED THROUGH STDIN, under the same deadline and tree containment
/// as [`run_git_bounded`].
///
/// `wait_with_output()` has no timeout, so the wait is rebuilt here. All three pipes are serviced on
/// their OWN threads, which is what makes the deadline total rather than partial. Two deadlocks are
/// closed by that, and both were live at some point in this function's history:
///
/// * The original code wrote the whole input and only then called `wait_with_output`, so a child
///   emitting more than one pipe buffer of OUTPUT while we were still writing blocked us and itself
///   forever. Starting the stdout/stderr drains first fixed that one.
/// * Writing the input on THIS thread, ahead of the wait loop, reintroduced the mirror image on the
///   INPUT side: a pipe buffer is ~64 KiB, so `write_all` of a larger payload blocks until the child
///   drains it — and the deadline had not started counting yet, so nothing could break the tie. That
///   is not hypothetical here: `unpack-objects` is fed a whole packfile (megabytes for any real
///   checkpoint), while holding `transaction.lock`. The write now runs on its own thread, so the
///   loop below is reached immediately and the deadline governs the write as well.
///
/// The caller configures stdio: `stdin` MUST be piped; stdout/stderr may be piped or null.
fn run_git_piped_bounded(cmd: &mut Command, stdin_bytes: &[u8], what: &str) -> Result<Output> {
    run_git_piped_bounded_within(cmd, stdin_bytes, what, git_op_timeout())
}

/// `run_git_piped_bounded` with the deadline passed in, so a test can shorten it for one child
/// without setting `AIZEN_GIT_OP_TIMEOUT_SECS` for every git call in the process.
fn run_git_piped_bounded_within(
    cmd: &mut Command,
    stdin_bytes: &[u8],
    what: &str,
    timeout: Duration,
) -> Result<Output> {
    crate::core::proctree::prepare(cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {what} (is git installed?)"))?;
    let containment = crate::core::proctree::contain(&child);

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let oh = spawn_git_pipe_worker(move || read_pipe_bytes(out_pipe));
    let eh = spawn_git_pipe_worker(move || read_pipe_bytes(err_pipe));

    // Feed stdin on its own thread, then CLOSE it — git plumbing waits for EOF on its input before it
    // will exit. A write error (child already gone) is deliberately not propagated as the primary
    // failure: the exit code and stderr explain the real cause far better than "broken pipe".
    let stdin = child.stdin.take();
    let payload = stdin_bytes.to_vec();
    let wh = spawn_git_pipe_worker(move || {
        use std::io::Write;
        let mut stdin = stdin;
        let err = match stdin.as_mut() {
            Some(pipe) => pipe.write_all(&payload).err(),
            None => None,
        };
        drop(stdin); // close the pipe so the child sees EOF
        err
    });

    let start = std::time::Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    crate::core::proctree::kill_tree(&mut child, &containment);
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(e).with_context(|| format!("waiting for {what}")),
        }
    };
    // Join the writer too: killing the tree unblocks a parked `write_all` (the read end is gone), so
    // this returns promptly. `None` on a lapsed grace means the thread is still parked, which we treat
    // as no reportable write error — the exit code below is the better story either way.
    let write_err = crate::core::proctree::join_drain(wh, GIT_DRAIN_GRACE).0;
    let (stdout, _) = crate::core::proctree::join_drain(oh, GIT_DRAIN_GRACE);
    let (stderr, _) = crate::core::proctree::join_drain(eh, GIT_DRAIN_GRACE);
    if timed_out {
        // Deliberately NOT "nothing was changed": a child killed mid-flight may already have written
        // part of its work (`unpack-objects` lands loose objects as it goes). Saying otherwise would
        // talk the reader out of the one check worth doing.
        bail!(
            "{what} exceeded {}s and was terminated (set AIZEN_GIT_OP_TIMEOUT_SECS to raise the \
             limit); it may have completed partially — run `aizen time doctor` to check the store",
            timeout.as_secs()
        );
    }
    if let Some(e) = write_err {
        return Err(e).with_context(|| format!("writing input to {what}"));
    }
    Ok(Output {
        status: exit_status_from_code(code),
        stdout,
        stderr,
    })
}

/// Retain the calling body's context (including admission and physical leases) on the actual pipe
/// worker, even when the caller returns on error or abandons its join at the drain deadline. This
/// only extends existing ownership; pipe service must never acquire a second physical admission.
fn spawn_git_pipe_worker<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    let context = crate::core::exec_ctx::current();
    std::thread::spawn(move || match context {
        Some(context) => crate::core::exec_ctx::with_current(context, f),
        None => f(),
    })
}

/// Drain a child pipe to raw bytes on a worker thread (byte-exact: pack data and `-z` output).
fn read_pipe_bytes<R: std::io::Read>(pipe: Option<R>) -> Vec<u8> {
    match pipe {
        Some(mut p) => {
            let mut bytes = Vec::new();
            let _ = p.read_to_end(&mut bytes);
            bytes
        }
        None => Vec::new(),
    }
}

/// Rebuild an `ExitStatus` from a raw code so [`run_git_bounded`] can return a plain `Output`.
///
/// There is no portable constructor, so each platform uses its own `from_raw`. On Unix the raw value
/// is a *wait status*, where the exit code occupies the high byte (`code << 8`) — passing the code
/// directly would report every success as a signal death.
fn exit_status_from_code(code: Option<i32>) -> std::process::ExitStatus {
    let code = code.unwrap_or(1);
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }
}

fn fnv1a64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stable repo/worktree identity, decoupled from the filesystem path.
///
/// The store id used to be `repo-<fnv(path-of-.git)>`. That made identity a function of LOCATION, so
/// renaming or moving a repo minted a brand-new (empty) store and the whole timeline appeared to
/// vanish — the old store became an unreachable orphan (`repo-d137…` on this machine). This module
/// keeps the store OUTSIDE `.git` (all the isolation reasons stand) but remembers each repo's id in a
/// home-level registry keyed for reconnection by its ROOT COMMIT, so a move keeps the timeline while a
/// `cp -r`/clone (same root commit, but the ORIGINAL path still exists) correctly gets a fresh id.
///
/// The [`resolve`] core is a pure function over an in-memory [`Registry`] so every branch — exact hit,
/// moved-repo reconnect, legacy grandfather, brand-new — is unit-tested without touching git or disk.
mod identity {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct WorktreeEntry {
        pub worktree_id: String,
        /// Absolute canonical worktree git dir (`.git` or `.git/worktrees/<name>`) last seen.
        pub git_dir: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RepoEntry {
        /// Immutable once assigned. Never recomputed from a path.
        pub repo_id: String,
        /// Reconnection key across moves. `None` only for a repo with no commits yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub root_commit: Option<String>,
        /// Absolute canonical common git dir last seen; updated (not re-keyed) when the repo moves.
        pub common_git_dir: String,
        #[serde(default)]
        pub worktrees: Vec<WorktreeEntry>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Registry {
        #[serde(default)]
        pub schema_version: u32,
        #[serde(default)]
        pub repos: Vec<RepoEntry>,
    }

    /// Everything [`resolve`] needs, with side effects injected as closures so it stays pure/testable.
    pub struct ResolveInput<'a> {
        pub common_git_dir: String,
        pub git_dir: String,
        pub root_commit: Option<String>,
        /// `repo-<fnv(common)>` — the pre-registry id; used to grandfather an existing on-disk store.
        pub legacy_repo_id: String,
        /// `wt-<fnv(wt)>` — the pre-registry worktree id, grandfathered alongside a legacy store.
        pub legacy_wt_id: String,
        /// Does `~/.aizen/timemachine/<legacy_repo_id>/store.git` already exist on disk?
        pub legacy_store_exists: bool,
        /// Does this recorded path still exist? (Used to tell a MOVE from a live clone.)
        pub path_exists: &'a dyn Fn(&str) -> bool,
        /// Mint a fresh random id for the given prefix (`"repo"` / `"wt"`).
        pub new_id: &'a dyn Fn(&str) -> String,
    }

    pub struct Resolved {
        pub repo_id: String,
        pub worktree_id: String,
        /// True when the registry was mutated and must be persisted.
        pub changed: bool,
    }

    /// Assign (or look up) the worktree id within an already-selected repo entry.
    fn resolve_worktree(entry: &mut RepoEntry, input: &ResolveInput) -> (String, bool) {
        // Exact git-dir hit — the common case.
        if let Some(w) = entry.worktrees.iter().find(|w| w.git_dir == input.git_dir) {
            return (w.worktree_id.clone(), false);
        }
        // Reclaim a stale worktree whose recorded git-dir is gone (the moved-checkout case), but only
        // when EXACTLY one is stale, so we never guess among several dead worktrees.
        let stale: Vec<usize> = entry
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, w)| !(input.path_exists)(&w.git_dir))
            .map(|(i, _)| i)
            .collect();
        if stale.len() == 1 {
            let i = stale[0];
            entry.worktrees[i].git_dir = input.git_dir.clone();
            return (entry.worktrees[i].worktree_id.clone(), true);
        }
        // Otherwise a genuinely new (linked) worktree.
        let wt_id = (input.new_id)("wt");
        entry.worktrees.push(WorktreeEntry {
            worktree_id: wt_id.clone(),
            git_dir: input.git_dir.clone(),
        });
        (wt_id, true)
    }

    /// Resolve identity against the registry, mutating it as needed. Pure: all I/O is in the caller.
    pub fn resolve(reg: &mut Registry, input: &ResolveInput) -> Resolved {
        // 1. Exact path hit — return the stored id; backfill root_commit if we only now learned it.
        if let Some(idx) = reg
            .repos
            .iter()
            .position(|r| r.common_git_dir == input.common_git_dir)
        {
            let (worktree_id, mut changed) = resolve_worktree(&mut reg.repos[idx], input);
            if reg.repos[idx].root_commit.is_none() && input.root_commit.is_some() {
                reg.repos[idx].root_commit = input.root_commit.clone();
                changed = true;
            }
            let repo_id = reg.repos[idx].repo_id.clone();
            return Resolved {
                repo_id,
                worktree_id,
                changed,
            };
        }

        // 2. Moved repo: a UNIQUE entry with the same root commit whose recorded path is now gone.
        //    Uniqueness + "old path gone" is what separates a move from a live clone/`cp -r`.
        if let Some(rc) = input.root_commit.as_deref() {
            let matches: Vec<usize> = reg
                .repos
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    r.root_commit.as_deref() == Some(rc) && !(input.path_exists)(&r.common_git_dir)
                })
                .map(|(i, _)| i)
                .collect();
            if matches.len() == 1 {
                let idx = matches[0];
                reg.repos[idx].common_git_dir = input.common_git_dir.clone();
                let (worktree_id, _) = resolve_worktree(&mut reg.repos[idx], input);
                let repo_id = reg.repos[idx].repo_id.clone();
                return Resolved {
                    repo_id,
                    worktree_id,
                    changed: true,
                };
            }
        }

        // 3. New to the registry. Grandfather a legacy hash-path store if one is already on disk (so
        //    existing timelines keep their id + namespace with ZERO data migration); else mint random.
        let (repo_id, worktree_id) = if input.legacy_store_exists {
            (input.legacy_repo_id.clone(), input.legacy_wt_id.clone())
        } else {
            ((input.new_id)("repo"), (input.new_id)("wt"))
        };
        reg.repos.push(RepoEntry {
            repo_id: repo_id.clone(),
            root_commit: input.root_commit.clone(),
            common_git_dir: input.common_git_dir.clone(),
            worktrees: vec![WorktreeEntry {
                worktree_id: worktree_id.clone(),
                git_dir: input.git_dir.clone(),
            }],
        });
        Resolved {
            repo_id,
            worktree_id,
            changed: true,
        }
    }
}

/// Mint a random `<prefix>-<16 hex>` id. Random (not path-derived) so it is stable for a repo's life
/// and can never collide with a `cp -r` sibling. Falls back to a time-seeded value if the system RNG
/// is somehow unavailable — uniqueness still holds well enough for a per-repo store name.
fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        let n = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64
            ^ OP_SEQ.fetch_add(1, Ordering::Relaxed);
        bytes = n.to_le_bytes();
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

/// The single root-commit OID of a repo's current HEAD history, used as the move-reconnection key.
/// `--max-parents=0` lists root commits; a repo with several roots (merged histories) or none (no
/// commits yet) yields `None`, which simply disables reconnect for that repo — never an error.
fn root_commit_of(root: &Path) -> Option<String> {
    let out = raw_git(root, ["rev-list", "--max-parents=0", "HEAD"]).ok()?;
    let mut roots = out.split_whitespace();
    let first = roots.next()?;
    // Exactly one root — ambiguous multi-root histories are not a stable key.
    if roots.next().is_some() {
        return None;
    }
    Some(first.to_string())
}

/// Resolve `(repo_id, worktree_id)` through the home registry. Legacy direct-mode fallback remains
/// available on registry failure only when a bounded scan rules out speculation evidence; losing a
/// stable mapping must not make a pending integration disappear behind a newly chosen namespace.
fn resolve_identity(
    home: &Path,
    common_canon: &Path,
    wt_canon: &Path,
    root: &Path,
) -> Result<(String, String)> {
    let common_key = common_canon.to_string_lossy().replace('\\', "/");
    let git_key = wt_canon.to_string_lossy().replace('\\', "/");
    let legacy_repo_id = format!("repo-{:016x}", fnv1a64(&common_key));
    let legacy_wt_id = format!("wt-{:016x}", fnv1a64(&git_key));

    let attempt = || -> Result<(String, String)> {
        let tm_root = home.join("timemachine");
        fs::create_dir_all(&tm_root)?;
        // Serialize registry read-modify-write across processes. Short-lived, taken BEFORE any store
        // lock (different namespace), released when this function returns → no lock nesting.
        let reg_lock_path = tm_root.join("registry.lock");
        let reg_guard = crate::core::workspace_txn::NativeResource::resolve(&reg_lock_path)?;
        let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
            &reg_lock_path,
            Duration::from_secs(5),
        )?;
        reg_guard.verify()?;
        let reg_path = tm_root.join("registry.json");
        // Do not replace corrupt/future evidence with a fresh empty mapping. A fallback namespace
        // could otherwise hide an unresolved speculative integration in the previously assigned id.
        let mut reg = read_speculation_registry(&tm_root)?;
        if reg.schema_version == 0 {
            reg.schema_version = 1;
        }
        let legacy_store_exists = tm_root.join(&legacy_repo_id).join("store.git").is_dir();
        let path_exists = |p: &str| Path::new(p).exists();
        let new_id = |prefix: &str| random_id(prefix);
        let input = identity::ResolveInput {
            common_git_dir: common_key.clone(),
            git_dir: git_key.clone(),
            root_commit: root_commit_of(root),
            legacy_repo_id: legacy_repo_id.clone(),
            legacy_wt_id: legacy_wt_id.clone(),
            legacy_store_exists,
            path_exists: &path_exists,
            new_id: &new_id,
        };
        let resolved = identity::resolve(&mut reg, &input);
        if resolved.changed {
            let bytes = serde_json::to_vec_pretty(&reg)?;
            crate::core::persist::atomic_write(&reg_path, &bytes)?;
        }
        Ok((resolved.repo_id, resolved.worktree_id))
    };

    match attempt() {
        Ok(ids) => Ok(ids),
        Err(error) => {
            // Preserve legacy direct-mode availability only when a bounded, alias-checked scan can
            // rule out speculation evidence anywhere the unreadable registry might have named.
            for path in checked_metadata_entries(&home.join("timemachine"))? {
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("repo-"))
                    && store_has_speculation_evidence(&path)?
                {
                    return Err(error).context("Time Machine registry is unavailable while speculative evidence exists; identity fallback refused, metadata retained");
                }
            }
            Ok((legacy_repo_id, legacy_wt_id))
        }
    }
}

fn raw_git<I, S>(root: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Absence of a git executable is a TYPED error (GitMissing), not a spawn ENOENT dressed as a
    // repo problem — `save_protected_change`/`preflight` treat it as "checkpoints simply off"
    // instead of refusing every file edit on a machine without git.
    let mut cmd = crate::core::gitx::command()?;
    cmd.current_dir(root).args(args);
    // Bounded like every other git spawn here: this one runs during discovery, BEFORE any lock is
    // taken, so a hang strands no lock — but it does park the turn's thread forever, which looks
    // identical to a frozen agent. A deadline turns that into a reportable error.
    let out = run_git_bounded(&mut cmd, "git repository probe")?;
    if !out.status.success() {
        bail!(
            "git repository probe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

struct TempIndex(PathBuf);
impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("lock"));
    }
}

fn seed_index(ctx: &RepoContext) -> Result<TempIndex> {
    fs::create_dir_all(&ctx.namespace_dir)?;
    let path = ctx.temp_index();
    let _ = fs::remove_file(&path);
    // Seed from the source worktree's real index so tracked modes/skip-worktree/sparse bits survive.
    // The private store does not own that index; it only receives the alternate-index snapshot.
    let source = ctx.git_dir.join("index");
    if source.is_file() {
        fs::copy(&source, &path)
            .with_context(|| format!("seeding temporary index from {}", source.display()))?;
    } else if let Ok(head) = ctx.source_git(["rev-parse", "--verify", "HEAD"]) {
        // HEAD may only exist in the source repo; objects resolve via sealed alternates.
        ctx.git(Some(&path), ["read-tree", &head])?;
    }
    Ok(TempIndex(path))
}

/// Directory subtrees the timeline provably never covers: fully-untracked, fully-ignored trees like
/// `target/` or `node_modules/`.
///
/// `ls-files --others --ignored --directory` only *collapses* a directory when nothing inside it is
/// tracked, so every path it names contributes zero bytes to the snapshot tree — descending into one
/// can only cost wall-clock. That cost is not marginal: on this repo the full walk visits 41k entries
/// (0.66s) of which 39k live under `target/`, and `restore` runs the walk three times (two
/// `current_tree` + one `apply_tree`), so a rewind paid ~2s of pure `readdir`.
///
/// Skipping them is also safe for the two things the walk is looking for. A nested `.git` or a
/// junction inside an ignored tree is outside coverage either way: `add -A` never stages it, so a
/// restore cannot write through it. The bail still fires for uncovered paths that are *not* ignored,
/// which is the case a user would actually be surprised by.
fn uncovered_dirs(ctx: &RepoContext) -> HashSet<PathBuf> {
    // Cached per context: `restore` calls the walk three times and the ignored-dir set cannot
    // meaningfully change inside one operation.
    if let Some(cached) = ctx.skip_dirs.borrow().as_ref() {
        return cached.clone();
    }
    let found = compute_uncovered_dirs(ctx);
    *ctx.skip_dirs.borrow_mut() = Some(found.clone());
    found
}

fn compute_uncovered_dirs(ctx: &RepoContext) -> HashSet<PathBuf> {
    // Best-effort: on any git hiccup, fall back to walking everything (correct, just slower).
    let Ok(out) = ctx.source_git_output([
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ]) else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    // `-z` avoids git's quoting of non-ASCII paths; only entries ending in `/` are collapsed dirs.
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| s.ends_with('/'))
        .map(|s| ctx.root.join(s.trim_end_matches('/')))
        .collect()
}

fn reparse_preflight(ctx: &RepoContext) -> Result<()> {
    // One walk per public operation: `restore_in_reported` calls `current_tree` twice plus `apply_tree`, and
    // the tree cannot sprout a junction mid-operation while we hold the workspace writer lease.
    if ctx.reparse_checked.get() {
        return Ok(());
    }
    let root = &ctx.root;
    let skip = uncovered_dirs(ctx);
    let root_git = root.join(".git");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("scanning {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if skip.contains(&path) {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                if path == root_git {
                    continue;
                }
                bail!(
                    "checkpoint refused: nested repository metadata at {} is outside this timeline's coverage",
                    path.display()
                );
            }
            let meta = fs::symlink_metadata(&path)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    bail!(
                        "checkpoint refused: reparse/junction path {} could escape the repository; remove or ignore it first",
                        path.display()
                    );
                }
            }
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    ctx.reparse_checked.set(true);
    Ok(())
}

fn index_blob_bytes(ctx: &RepoContext, index: &Path) -> Result<u64> {
    let listing = ctx.git(Some(index), ["ls-files", "-s"])?;
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let _mode = fields.next();
        let Some(oid) = fields.next() else { continue };
        if seen.insert(oid.to_string()) {
            ordered.push(oid.to_string());
        }
    }
    if ordered.is_empty() {
        return Ok(0);
    }

    // One `cat-file --batch-check` process instead of N `cat-file -s` spawns.
    let mut child = git_cmd();
    let store = strip_windows_verbatim(&ctx.store_git_dir);
    let hooks = strip_windows_verbatim(&ctx.hooks_dir)
        .to_string_lossy()
        .replace('\\', "/");
    child
        .current_dir(&ctx.root)
        .env("GIT_DIR", &store)
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .arg("-c")
        .arg(format!("core.hooksPath={hooks}"))
        .args(["-c", "core.fsmonitor=false"])
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // One newline-terminated oid per line. Bounded: this runs under `transaction.lock`, so a wedged
    // `cat-file` must fail rather than park the thread and strand the lock for the whole session.
    let mut batch_input = String::new();
    for oid in &ordered {
        batch_input.push_str(oid);
        batch_input.push('\n');
    }
    let out = run_git_piped_bounded(&mut child, batch_input.as_bytes(), "cat-file --batch-check")?;
    if !out.status.success() {
        bail!(
            "internal git operation failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let max_single = crate::core::cli_config::load()
        .timemachine_max_file_bytes
        .unwrap_or(512 * 1024 * 1024);
    let mut total = 0u64;
    let body = String::from_utf8_lossy(&out.stdout);
    for line in body.lines() {
        // Formats: "<oid> <type> <size>" or "<oid> missing"
        let mut fields = line.split_whitespace();
        let Some(oid) = fields.next() else { continue };
        let Some(kind_or_missing) = fields.next() else {
            continue;
        };
        if kind_or_missing == "missing" {
            bail!("checkpoint budget probe: blob {oid} is missing from the private store");
        }
        let size: u64 = fields
            .next()
            .with_context(|| format!("parsing Git blob size for {oid}"))?
            .parse()
            .with_context(|| format!("parsing Git blob size for {oid}"))?;
        if size > max_single {
            bail!("checkpoint budget exceeded: one file/blob is {size} bytes > configured limit {max_single}");
        }
        total = total
            .checked_add(size)
            .context("checkpoint byte count overflow")?;
    }
    Ok(total)
}

fn current_tree(ctx: &RepoContext) -> Result<(String, Coverage)> {
    ctx.ensure_safe_filters()?;
    reparse_preflight(ctx)?;
    let idx = seed_index(ctx)?;
    // The seeded index preserves tracked entries and modes. `add -A` updates tracked files even when
    // a later .gitignore rule matches them, while untracked ignored paths remain outside coverage.
    ctx.git(Some(&idx.0), ["add", "-A", "--", "."])?;
    let tree = ctx.git(Some(&idx.0), ["write-tree"])?;
    let list = ctx.git(Some(&idx.0), ["ls-files", "-z"])?;
    let count = if list.is_empty() {
        0
    } else {
        list.as_bytes().iter().filter(|b| **b == 0).count() as u64
    };
    let byte_count = index_blob_bytes(ctx, &idx.0)?;
    let cfg = crate::core::cli_config::load();
    let max_files = cfg.timemachine_max_files.unwrap_or(100_000);
    let max_total = cfg.timemachine_max_bytes.unwrap_or(2 * 1024 * 1024 * 1024);
    if count > max_files {
        bail!("checkpoint budget exceeded: {count} files > configured limit {max_files}");
    }
    if byte_count > max_total {
        bail!("checkpoint budget exceeded: {byte_count} bytes > configured limit {max_total}");
    }
    Ok((
        tree,
        Coverage {
            file_count: count,
            byte_count,
            notes: vec![
                "git-visible repository tree; ignored/outside/nested repositories are not covered"
                    .to_string(),
            ],
        },
    ))
}

fn write_chat(ctx: &RepoContext, id: u32, chat: &[Message]) -> Result<()> {
    fs::create_dir_all(ctx.chat_dir())?;
    crate::core::config::harden_dir(&ctx.chat_dir());
    let bytes = serde_json::to_vec(chat)?;
    crate::core::persist::atomic_write(&ctx.chat_path(id), &bytes)?;
    crate::core::persist::harden_owner_only_checked(&ctx.chat_path(id))?;
    Ok(())
}

fn read_chat_in(ctx: &RepoContext, id: u32) -> Result<Option<Vec<Message>>> {
    for path in [
        ctx.chat_path(id),
        ctx.inrepo_chat_path(id),
        ctx.legacy_chat_path(id),
    ] {
        if let Some(bytes) = crate::core::persist::read_optional(&path)? {
            return Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("conversation sidecar for checkpoint #{id} is corrupt")
            })?));
        }
    }
    Ok(None)
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// "Checkpoints are simply off here" — either the directory isn't a repo, or there is no git
/// executable at all. Both must degrade to no-checkpoint instead of failing the caller: the
/// git-missing case used to propagate as a hard error, and because the protected-edit gate runs
/// before every mutation, that refused EVERY `file_write`/`file_edit` on a gitless machine
/// while blaming "the pre-edit checkpoint".
fn benign_no_checkpoint(e: &anyhow::Error) -> bool {
    e.to_string().contains("not a git repository") || crate::core::gitx::is_git_missing(e)
}

/// Actionable trailer for a checkpoint failure that blocked an edit, or `""` when the error already
/// explains itself.
///
/// The gate is deliberately FAIL-CLOSED for a real checkpoint failure — an edit that lands with no
/// pre-image has no rewind, so proceeding would trade a visible error for silent data risk. But
/// "fail closed" must not mean "dead end", and one case was exactly that: a busy `transaction.lock`
/// reported a path the user then found ABSENT on disk, with no aizen process running, and deleting
/// the file changed nothing. All three observations are consistent, and the reason is that the lock
/// is an OS byte-range lock bound to an open HANDLE, not to the path: it can only be held by a live
/// process, it is invisible in the directory listing, and unlinking the path cannot release it.
/// The holder was this same process — a thread stranded inside an unbounded `git` child from an
/// earlier turn (now bounded; see [`GIT_OP_TIMEOUT`]). Naming that turns an impasse into one step.
pub fn checkpoint_failure_hint(e: &anyhow::Error) -> String {
    let busy = e.chain().any(|c| {
        c.downcast_ref::<crate::core::repo_lock::LockBusy>()
            .is_some()
    });
    if !busy {
        return String::new();
    }
    " — this lock is held on an open handle, not by the file on disk, so deleting it has no effect \
     and an absent file does not mean it is free. It is most likely held by THIS aizen session (a \
     stuck internal git call from an earlier turn). Run `aizen time doctor`, or restart this \
     session to clear it; edits are refused rather than run unprotected because a change with no \
     pre-edit checkpoint cannot be rewound."
        .to_string()
}

/// Validate Time Machine metadata without capturing a tree. Kept for CLI diagnostics; protected
/// mutations should call [`save_protected_change`] while their workspace writer lease is held.
#[allow(dead_code)]
pub fn preflight_protected_change() -> Result<bool> {
    match RepoContext::current() {
        Ok(ctx) => {
            let _store = ctx.store_shared()?;
            let _lock = ctx.lock()?;
            let mut ledger = ctx.load_ledger()?;
            ctx.recover_pending(&mut ledger)?;
            Ok(true)
        }
        Err(e) if benign_no_checkpoint(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Capture the preimage for an already-leased workspace mutation. The caller must hold the
/// `WorkspaceWriterLease` across this call and the subsequent tool body, closing the old
/// preflight→save→mutation gap without acquiring a second workspace lock.
///
/// Callers all reach the `_in` form directly because they already know the repo root they resolved;
/// this is the cwd-discovering convenience wrapper.
#[allow(dead_code)]
pub fn save_protected_change(label: &str) -> Result<Option<Snapshot>> {
    save_protected_change_in(label, None)
}

/// As [`save_protected_change`], but discovering the repository from `start` — the directory the
/// pending write will actually land in — instead of the process's cwd.
///
/// The two are different questions, and conflating them produced a wrong answer with dangerous
/// advice: a session launched from `C:\Users\admin` editing `Desktop/mini_project/aizen_web/...`
/// asked "is my CWD a repo?", got no, and told the user to `git init` — in their HOME DIRECTORY,
/// while the project one level down had a perfectly good `.git`. The right question is whether the
/// thing being changed is in a work tree, so ask git from there. `None` preserves the old
/// cwd-relative behavior for callers with no particular path (`shell_run`, opaque effects).
pub fn save_protected_change_in(label: &str, start: Option<&Path>) -> Result<Option<Snapshot>> {
    let found = match start {
        Some(dir) => RepoContext::discover(dir),
        None => RepoContext::current(),
    };
    match found {
        Ok(ctx) => save_in(&ctx, label, true, None).map(Some),
        Err(e) if benign_no_checkpoint(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Capture and publish under the same metadata transaction, including auto-save deduplication.
pub fn save_for(
    recovery: &RunRecovery,
    root: &Path,
    label: &str,
    auto: bool,
    pre_edit: bool,
) -> Result<Option<SnapshotHandle>> {
    let execution_ctx = crate::core::exec_ctx::current().unwrap_or_default();
    save_for_in_context(recovery, root, label, auto, pre_edit, &execution_ctx)
}

/// Driver callers pass their context explicitly so snapshot admission can reenter their dispatch
/// writer lease. A driver is not installed in the synchronous tool thread-local.
pub fn save_for_in_context(
    recovery: &RunRecovery,
    root: &Path,
    label: &str,
    auto: bool,
    pre_edit: bool,
    execution_ctx: &crate::core::exec_ctx::ExecutionContext,
) -> Result<Option<SnapshotHandle>> {
    let ctx = match RepoContext::discover(root) {
        Ok(ctx) => ctx,
        Err(error) if benign_no_checkpoint(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let _snapshot_admission = snapshot_admission(
        || crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&ctx.root),
        execution_ctx,
        crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED,
        LOCK_TIMEOUT,
    )?;
    with_snapshot_admission(execution_ctx, &_snapshot_admission, || {
        let _store = ctx.store_shared()?;
        let _lock = ctx.lock()?;
        let mut ledger = ctx.load_ledger()?;
        {
            let binding = RepoBinding::from_context(&ctx);
            let state = recovery
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?;
            if state
                .repositories
                .values()
                .flat_map(|a| [a.pre_edit.as_ref(), a.last_good.as_ref()])
                .flatten()
                .any(|h| h.root() == ctx.root && h.binding != binding)
            {
                bail!("bound run/worktree namespace changed; protected change refused rather than replacing its baseline");
            }
        }
        ctx.recover_pending(&mut ledger)?;
        if pre_edit {
            let existing = recovery
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?
                .repositories
                .get(&ctx.namespace_dir)
                .filter(|anchors| anchors.pre_edit_current)
                .and_then(|a| a.pre_edit.clone());
            if let Some(handle) = existing {
                if handle.binding != RepoBinding::from_context(&ctx) {
                    bail!("pre-edit anchor binding changed; protected change refused");
                }
                let row = ledger
                    .snapshots
                    .iter()
                    .find(|s| s.id == handle.id)
                    .context("pinned pre-edit anchor missing from ledger")?;
                if row.commit != handle.commit
                    || row.tree != handle.tree
                    || row.worktree_id != handle.worktree_id
                {
                    bail!("pre-edit anchor ledger identity changed; protected change refused");
                }
                ctx.validate_snapshot_ref(row)?;
                let mut handle = handle;
                handle.restore_authorized = recovery.can_restore_workspace()
                    && crate::core::exec_ctx::current()
                        .is_none_or(|ctx| ctx.can_restore_workspace());
                return Ok(Some(handle));
            }
        }
        let snapshot = save_locked(&ctx, label, auto, None)?;
        let mut handle = ctx.pin(snapshot)?;
        handle.restore_authorized &= recovery.can_restore_workspace();
        recovery.publish(handle.clone(), pre_edit)?;
        drop(_lock);
        drop(_store);
        let cfg = crate::core::cli_config::load();
        prune_after_save(
            &ctx,
            cfg.timemachine_keep.unwrap_or(DEFAULT_KEEP),
            cfg.timemachine_keep_safety.unwrap_or(DEFAULT_KEEP_SAFETY),
            &[handle.id],
        )?;
        Ok(Some(handle))
    })
}

pub fn save(label: &str, auto: bool) -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let snap = save_in(&ctx, label, auto, None)?;
    let cfg = crate::core::cli_config::load();
    let keep = cfg.timemachine_keep.unwrap_or(DEFAULT_KEEP);
    let keep_safety = cfg.timemachine_keep_safety.unwrap_or(DEFAULT_KEEP_SAFETY);
    prune_after_save(&ctx, keep, keep_safety, &[snap.id])?;
    compact_if_loose(&ctx);
    Ok(snap)
}

pub fn save_with_chat(label: &str, auto: bool, chat: &[Message]) -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let snap = save_in(&ctx, label, auto, Some(chat))?;
    let cfg = crate::core::cli_config::load();
    let keep = cfg.timemachine_keep.unwrap_or(DEFAULT_KEEP);
    let keep_safety = cfg.timemachine_keep_safety.unwrap_or(DEFAULT_KEEP_SAFETY);
    prune_after_save(&ctx, keep, keep_safety, &[snap.id])?;
    compact_if_loose(&ctx);
    Ok(snap)
}

/// Pack the store's loose objects once they pass [`LOOSE_COMPACT_THRESHOLD`].
///
/// Called after a save has RELEASED its leases: compaction wants the store exclusively, and taking it
/// while `prune_after_save` still holds the shared lease would deadlock against ourselves.
///
/// Best-effort throughout. Housekeeping must never be able to fail a checkpoint that already
/// succeeded, so a busy store, a missing git, or a failed pack all just mean "not this time" — the
/// next save re-checks and the pile is still bounded.
fn compact_if_loose(ctx: &RepoContext) {
    if ctx.loose_object_ids().len() < LOOSE_COMPACT_THRESHOLD {
        return;
    }
    let Ok(_store) = ctx.store_exclusive() else {
        return;
    };
    // Re-count under the lease: a sibling worktree may have compacted this store while we queued.
    let Some(root) = ctx.store_git_dir.parent() else {
        return;
    };
    if require_settled_store_speculation(root, None).is_err() {
        return; // Housekeeping never mutates a store whose integration evidence is uncertain.
    }
    if ctx.loose_object_ids().len() < LOOSE_COMPACT_THRESHOLD {
        return;
    }
    let _ = ctx.compact_objects();
}

fn prune_after_save(
    ctx: &RepoContext,
    keep: usize,
    keep_safety: usize,
    protected: &[u32],
) -> Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    if ledger.snapshots.len() <= keep {
        return Ok(());
    }
    let (live, _pins) = ctx.deletion_pins(ledger.snapshots.iter().map(|s| s.id))?;
    let mut protected: Vec<u32> = protected.to_vec();
    protected.extend(live);
    let mut journal = Journal::new(JournalKind::Prune, ledger.generation);
    ctx.save_journal(&journal)?;
    let dropped = enforce_retention_plan(&mut ledger, keep, keep_safety, &protected);
    // Commit the new authoritative ledger first. Old refs remain recovery pins if cleanup is
    // interrupted; doctor can safely report/reap those orphans later.
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(ctx, &dropped)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(())
}

/// Outer snapshot ownership only. OFF must not discover/acquire any new gate; ON orders writer
/// before fresh physical exclusion, then the caller takes store/time locks. Internal save/preimage
/// paths reuse their outer restore or snapshot admission instead of acquiring a second body gate.
fn snapshot_admission(
    identity: impl FnOnce() -> Result<crate::core::workspace_txn::WorkspaceIdentity>,
    execution_ctx: &crate::core::exec_ctx::ExecutionContext,
    enabled: bool,
    timeout: Duration,
) -> Result<
    Option<(
        crate::core::workspace_txn::PhysicalEffectLease,
        crate::core::workspace_txn::WorkspaceWriterLease,
    )>,
> {
    if !enabled {
        return Ok(None);
    }
    let scope = execution_ctx.resource_scope();
    let cancel = crate::core::cancel::current();
    let start = std::time::Instant::now();
    let writer = crate::core::workspace_txn::WorkspaceWriterLease::acquire_identity_scoped(
        identity()?,
        timeout,
        cancel.as_ref(),
        "time checkpoint",
        &scope,
    )?;
    let physical = crate::core::workspace_txn::PhysicalEffectLease::exclusive_without_background(
        timeout.saturating_sub(start.elapsed()),
        cancel.as_ref(),
        "time checkpoint snapshot",
        &scope,
    )?;
    // Release physical exclusion before dispatch bookkeeping, both after the snapshot body.
    Ok(Some((physical, writer)))
}

fn with_snapshot_admission<T>(
    execution_ctx: &crate::core::exec_ctx::ExecutionContext,
    admission: &Option<(
        crate::core::workspace_txn::PhysicalEffectLease,
        crate::core::workspace_txn::WorkspaceWriterLease,
    )>,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let Some((physical, writer)) = admission else {
        return f();
    };
    // Transfer the existing body ownership to bounded child/drain owners, without a nested acquire.
    let body_ctx = execution_ctx
        .with_writer_lease(writer.clone())
        .with_physical_effect_lease(physical.clone());
    crate::core::exec_ctx::with_current(body_ctx, f)
}

fn save_in(
    ctx: &RepoContext,
    label: &str,
    auto: bool,
    chat: Option<&[Message]>,
) -> Result<Snapshot> {
    let execution_ctx = crate::core::exec_ctx::current().unwrap_or_default();
    let _snapshot_admission = snapshot_admission(
        || crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&ctx.root),
        &execution_ctx,
        crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED,
        LOCK_TIMEOUT,
    )?;
    with_snapshot_admission(&execution_ctx, &_snapshot_admission, || {
        let _store = ctx.store_shared()?;
        let _lock = ctx.lock()?;
        save_locked(ctx, label, auto, chat)
    })
}

fn save_locked(
    ctx: &RepoContext,
    label: &str,
    auto: bool,
    chat: Option<&[Message]>,
) -> Result<Snapshot> {
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let (tree, coverage) = current_tree(ctx)?;

    // Auto files-only saves may reuse the newest tree. Explicit/user/chat checkpoints are immutable
    // events and therefore always get a fresh ID even when the bytes are identical.
    if auto && chat.is_none() {
        if let Some(last) = ledger.snapshots.last().filter(|s| s.tree == tree) {
            let last = last.clone();
            ledger.set_cursor(Some(last.id));
            ctx.save_ledger(&mut ledger)?;
            return Ok(last);
        }
    }
    capture_checkpoint_locked(ctx, &mut ledger, tree, coverage, label, auto, chat, false)
}

pub fn load_chat_checked(id: u32) -> Result<Vec<Message>> {
    let ctx = RepoContext::current()?;
    read_chat_in(&ctx, id)?.with_context(|| format!("checkpoint #{id} has no saved conversation"))
}

/// Value-aware retention. Two knobs: `keep` caps the TOTAL snapshots, `keep_safety` caps how many
/// low-value SafetyNet checkpoints (`before agent edits`) may occupy that budget. The point is that
/// not all checkpoints are worth the same — a descriptive `phase: …` milestone the user browses to
/// must not be evicted by a pile of per-run pre-edit nets.
///
/// Order of eviction (oldest-first within each pass; `protected` and the cursor are never dropped):
///   Pass 1 — drop SafetyNet snapshots beyond the newest `keep_safety`.
///   Pass 2 — if still over `keep`, drop the remaining droppable snapshots oldest-first, SafetyNet
///            before Milestone/Recovery, so the surviving set skews toward milestones.
///
/// `keep == 0` means unlimited (matches the old contract).
fn enforce_retention_plan(
    ledger: &mut Ledger,
    keep: usize,
    keep_safety: usize,
    protected: &[u32],
) -> Vec<Snapshot> {
    if keep == 0 {
        return Vec::new();
    }
    let mut protected: HashSet<u32> = protected.iter().copied().collect();
    if let Some(id) = ledger.cursor_id {
        protected.insert(id);
    }

    let mut drop_ids: HashSet<u32> = HashSet::new();

    // Pass 1 — thin SafetyNet nets to the newest `keep_safety`. Snapshots are stored oldest→newest,
    // so iterating in reverse keeps the most recent nets and marks the older overflow for eviction.
    let mut safety_kept = 0usize;
    for snap in ledger.snapshots.iter().rev() {
        if protected.contains(&snap.id) || snap.kind() != SnapshotKind::SafetyNet {
            continue;
        }
        if safety_kept < keep_safety {
            safety_kept += 1;
        } else {
            drop_ids.insert(snap.id);
        }
    }

    // Pass 2 — if the total (after Pass 1 removals) still exceeds `keep`, evict the oldest droppable
    // snapshots until it fits. SafetyNet outranks Milestone/Recovery for eviction so milestones die last.
    let surviving = ledger.snapshots.len() - drop_ids.len();
    if surviving > keep {
        let mut need = surviving - keep;
        let mut candidates: Vec<(usize, u32)> = ledger
            .snapshots
            .iter()
            .filter(|s| !protected.contains(&s.id) && !drop_ids.contains(&s.id))
            .map(|s| {
                // Lower rank = evicted sooner. SafetyNet (0) before Milestone/Recovery (1).
                let rank = if s.kind() == SnapshotKind::SafetyNet {
                    0
                } else {
                    1
                };
                (rank, s.id)
            })
            .collect();
        // Sort by (rank asc, id asc) → oldest of the cheapest tier first.
        candidates.sort_unstable();
        for (_, id) in candidates {
            if need == 0 {
                break;
            }
            drop_ids.insert(id);
            need -= 1;
        }
    }

    if drop_ids.is_empty() {
        return Vec::new();
    }
    let mut dropped = Vec::new();
    let mut i = 0;
    while i < ledger.snapshots.len() {
        if drop_ids.contains(&ledger.snapshots[i].id) {
            dropped.push(ledger.snapshots.remove(i));
        } else {
            i += 1;
        }
    }
    ledger.set_cursor(ledger.cursor_id);
    dropped
}

fn delete_snapshots(ctx: &RepoContext, dropped: &[Snapshot]) -> Result<()> {
    for snap in dropped {
        let ref_name = ctx.ref_name(snap.id);
        if let Ok(actual) = ctx.git(None, ["rev-parse", "--verify", &ref_name]) {
            if actual != snap.commit {
                bail!("ref for checkpoint #{} changed concurrently", snap.id);
            }
            ctx.update_ref_delete(&ref_name, &actual)?;
        }
        // A legacy ref may still pin this object after migration. It is deliberately not deleted
        // here unless it is the only representation and matches exactly; preserving an extra pin is
        // safer than making a failed cleanup destroy recovery history.
        let _ = crate::core::persist::remove_if_exists(&ctx.chat_path(snap.id))?;
    }
    Ok(())
}

pub fn prune(keep: usize) -> Result<usize> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let mut journal = Journal::new(JournalKind::Prune, ledger.generation);
    ctx.save_journal(&journal)?;
    // Manual prune honours the same value-aware order: drop safety-nets before milestones. The safety
    // floor cannot exceed the total budget the user asked for.
    let keep_safety = DEFAULT_KEEP_SAFETY.min(keep);
    let (protected, _pins) = ctx.deletion_pins(ledger.snapshots.iter().map(|s| s.id))?;
    let dropped = enforce_retention_plan(&mut ledger, keep, keep_safety, &protected);
    // Ledger-first deletion: an interrupted cleanup leaves harmless orphan refs, never ledger entries
    // pointing at objects we already made unreachable.
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(&ctx, &dropped)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(dropped.len())
}

/// The named checkpoints out of the ledger, validated before anything is touched: an unknown id
/// fails the whole call (a typo must not half-delete), and the cursor is refused — the working
/// tree is standing on that point, and deleting the ground under it would leave `undo`/`redo`
/// with no anchor.
fn remove_plan(ledger: &mut Ledger, ids: &[u32]) -> Result<Vec<Snapshot>> {
    let want: HashSet<u32> = ids.iter().copied().collect();
    for id in &want {
        if !ledger.snapshots.iter().any(|s| s.id == *id) {
            bail!("no checkpoint #{id} — see `aizen time list`");
        }
    }
    if let Some(cur) = ledger.cursor_id.filter(|c| want.contains(c)) {
        bail!("checkpoint #{cur} is the active point — restore another checkpoint first, then delete this one");
    }
    let mut dropped = Vec::new();
    let mut i = 0;
    while i < ledger.snapshots.len() {
        if want.contains(&ledger.snapshots[i].id) {
            dropped.push(ledger.snapshots.remove(i));
        } else {
            i += 1;
        }
    }
    ledger.set_cursor(ledger.cursor_id);
    Ok(dropped)
}

/// Delete exactly the checkpoints named — `aizen time rm 3 7`. Same transaction shape as `prune`:
/// the plan is validated in memory first, then journal, then ledger-first deletion, so an
/// interrupted run leaves harmless orphan refs and never ledger rows pointing at reaped objects.
pub fn remove(ids: &[u32]) -> Result<usize> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let (protected, _pins) = ctx.deletion_pins(ids.iter().copied())?;
    if !protected.is_empty() {
        bail!("checkpoints {protected:?} are pinned by live runs; deletion refused");
    }
    let dropped = remove_plan(&mut ledger, ids)?;
    let mut journal = Journal::new(JournalKind::Prune, ledger.generation);
    ctx.save_journal(&journal)?;
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(&ctx, &dropped)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(dropped.len())
}

pub fn clear() -> Result<usize> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let (protected, _pins) = ctx.deletion_pins(ledger.snapshots.iter().map(|s| s.id))?;
    if !protected.is_empty() {
        bail!("checkpoints {protected:?} are pinned by live runs; clear refused");
    }
    let mut journal = Journal::new(JournalKind::Clear, ledger.generation);
    ctx.save_journal(&journal)?;
    let snapshots = ledger.snapshots.clone();
    let n = snapshots.len();
    ledger.snapshots.clear();
    ledger.set_cursor(None);
    // Empty ledger becomes authoritative before refs are reaped. A crash during cleanup leaves
    // recoverable orphan pins rather than an apparently valid timeline with missing objects.
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(&ctx, &snapshots)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(n)
}

/// Bring the worktree to `commit`. Returns the paths that were in the worktree (tracked, or
/// untracked and not ignored — a checkpoint's own coverage) and are absent from `commit`,
/// which the update removed.
///
/// The temporary index is seeded from the source `.git/index` and then staged with `add -A`,
/// exactly as a checkpoint is captured. Before E5.1 it was only seeded: a file created since
/// the checkpoint was never in the source index, `read-tree -u` removes only what the index it
/// starts from knows about, so the file survived and the verification in `restore_in_reported` rolled
/// the whole restore back — `/undo` failed on the file the agent had just created (quality plan
/// S2). [`restore_in_reported`] stages and journals the list itself before applying; this is
/// the one-shot form the rollback paths use.
fn apply_tree(ctx: &RepoContext, commit: &str) -> Result<Vec<String>> {
    require_workspace_restore()?;
    let staged = stage_for_apply(ctx)?;
    let removed = paths_absent_from(ctx, &staged.0, commit)?;
    apply_staged(ctx, &staged, commit)?;
    Ok(removed)
}

/// The preflight and the staged temporary index a restore applies from.
fn stage_for_apply(ctx: &RepoContext) -> Result<TempIndex> {
    ctx.ensure_safe_filters()?;
    reparse_preflight(ctx)?;
    let idx = seed_index(ctx)?;
    ctx.git(Some(&idx.0), ["add", "-A", "--", "."])?;
    Ok(idx)
}

/// Paths in `index` that `commit`'s tree does not have, sorted — what the update will remove.
fn paths_absent_from(ctx: &RepoContext, index: &Path, commit: &str) -> Result<Vec<String>> {
    let present = ctx.git(Some(index), ["ls-files", "-z"])?;
    let target = ctx.git(None, ["ls-tree", "-r", "-z", "--name-only", commit])?;
    let target: HashSet<&str> = target.split('\0').filter(|s| !s.is_empty()).collect();
    let mut removed: Vec<String> = present
        .split('\0')
        .filter(|p| !p.is_empty() && !target.contains(p))
        .map(str::to_string)
        .collect();
    removed.sort();
    Ok(removed)
}

/// The update itself: entries in the staged index that `commit` lacks are removed from the
/// worktree along with every other change.
fn apply_staged(ctx: &RepoContext, staged: &TempIndex, commit: &str) -> Result<()> {
    require_workspace_restore()?;
    ctx.git(Some(&staged.0), ["read-tree", "--reset", "-u", commit])?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "checkpoint transaction inputs remain explicit under the caller-held lock"
)]
fn capture_checkpoint_locked(
    ctx: &RepoContext,
    ledger: &mut Ledger,
    tree: String,
    coverage: Coverage,
    label: &str,
    auto: bool,
    chat: Option<&[Message]>,
    recovery: bool,
) -> Result<Snapshot> {
    let id = ledger.next_id.max(1);
    let ref_name = ctx.ref_name(id);
    let mut journal = Journal::new(JournalKind::Save, ledger.generation);
    journal.checkpoint_id = Some(id);
    journal.ref_name = Some(ref_name.clone());
    ctx.save_journal(&journal)?;

    let parent_commit = ledger
        .cursor_id
        .and_then(|pid| ledger.snapshots.iter().find(|s| s.id == pid))
        .map(|s| s.commit.clone())
        .or_else(|| ctx.source_git(["rev-parse", "--verify", "HEAD"]).ok());
    let msg = format!(
        "aizen checkpoint: {}",
        if label.is_empty() {
            "(no label)"
        } else {
            label
        }
    );
    let mut args = vec![
        "commit-tree".to_string(),
        tree.clone(),
        "-m".to_string(),
        msg,
    ];
    if let Some(parent) = parent_commit {
        args.push("-p".to_string());
        args.push(parent);
    }
    let commit = ctx.git(None, args)?;
    journal.new_oid = Some(commit.clone());
    ctx.save_journal(&journal)?;
    ctx.update_ref_create(&ref_name, &commit)?;
    journal.phase = JournalPhase::RefCreated;
    ctx.save_journal(&journal)?;

    let has_chat = match chat {
        Some(chat) => {
            if let Err(e) = write_chat(ctx, id, chat) {
                let _ = ctx.update_ref_delete(&ref_name, &commit);
                let _ = ctx.clear_journal();
                return Err(e).context("saving checkpoint conversation sidecar");
            }
            true
        }
        None => false,
    };
    // Classify for retention at write time. `recovery` is authoritative; the once-per-run pre-edit
    // net is a SafetyNet; a manual save or a closed/verified phase is a Milestone.
    let kind = if recovery {
        SnapshotKind::Recovery
    } else if auto && label == PRE_EDIT_LABEL {
        SnapshotKind::SafetyNet
    } else {
        SnapshotKind::Milestone
    };
    let snap = Snapshot {
        id,
        commit,
        tree,
        label: label.to_string(),
        created: now_string(),
        auto,
        has_chat,
        parent: ledger.cursor_id,
        worktree_id: ctx.worktree_id.clone(),
        coverage,
        recovery,
        kind: Some(kind),
        integration_receipt: None,
    };
    ledger.next_id = id.checked_add(1).context("checkpoint id space exhausted")?;
    ledger.snapshots.push(snap.clone());
    ledger.set_cursor(Some(id));
    if let Err(e) = ctx.save_ledger(ledger) {
        // Keep the journal + ref as recovery evidence. `recover_pending` will remove the orphan when
        // the old ledger generation is loaded on the next operation.
        return Err(e).context("committing checkpoint ledger");
    }
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(snap)
}

pub fn restore(id: u32) -> Result<Snapshot> {
    restore_with_report(id).map(|(snap, _)| snap)
}

/// [`restore`], also naming the files it removed because the checkpoint never had them.
pub fn restore_with_report(id: u32) -> Result<(Snapshot, Vec<String>)> {
    require_workspace_restore()?;
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time restore",
    )?;
    restore_in_reported(&ctx, id)
}

/// Restore a lifetime-pinned checkpoint without reacquiring the caller-held workspace lease.
fn restore_in_reported(ctx: &RepoContext, id: u32) -> Result<(Snapshot, Vec<String>)> {
    restore_in_reported_checked(ctx, id, None, None)
}

fn restore_in_reported_checked(
    ctx: &RepoContext,
    id: u32,
    expected: Option<&SnapshotHandle>,
    supplied_lease: Option<&crate::core::workspace_txn::WorkspaceWriterLease>,
) -> Result<(Snapshot, Vec<String>)> {
    require_workspace_restore()?;
    // Explicitly supplied, binding-checked ownership is already held. Reacquiring under ambient
    // default scope would deadlock against the caller rather than add physical exclusion.
    let _workspace = if supplied_lease.is_none() {
        Some(crate::core::workspace_txn::WorkspaceWriterLease::acquire(
            &ctx.root,
            LOCK_TIMEOUT,
            None,
            "time restore",
        )?)
    } else {
        None
    };
    let _physical = crate::core::workspace_txn::PhysicalEffectLease::exclusive_without_background(
        LOCK_TIMEOUT,
        None,
        "time restore",
        &crate::core::exec_ctx::current()
            .unwrap_or_default()
            .resource_scope(),
    )?;
    let body_context = crate::core::exec_ctx::current()
        .unwrap_or_default()
        .with_physical_effect_lease(_physical.clone());
    let body_context = if let Some(writer) = supplied_lease.or(_workspace.as_ref()) {
        body_context.with_writer_lease(writer.clone())
    } else {
        body_context
    };
    crate::core::exec_ctx::with_current(body_context, || {
        let _quiescence = crate::core::workspace_txn::WorkspaceRestoreGuard::acquire(&ctx.root)?;
        let _store = ctx.store_shared()?;
        let _lock = ctx.lock()?;
        let mut ledger = ctx.load_ledger()?;
        ctx.recover_pending_with_restore(&mut ledger, true)?;
        ctx.migrate_legacy_refs(&mut ledger)?;
        let target = ledger
            .snapshots
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .with_context(|| format!("no checkpoint #{id} (see `aizen time list`)"))?;
        ctx.validate_snapshot_ref(&target)?;
        if let Some(handle) = expected {
            if handle.binding != RepoBinding::from_context(ctx)
                || target.commit != handle.commit
                || target.tree != handle.tree
                || target.worktree_id != handle.worktree_id
            {
                bail!("bound checkpoint identity no longer matches ledger/ref; restore refused");
            }
        }

        let (cur_tree, cur_coverage) = current_tree(ctx)?;
        let preimage_id = if cur_tree != target.tree {
            // Restore always creates a fresh immutable preimage event. Reusing an older equal-tree
            // checkpoint makes `/redo` ambiguous after branching and can leave no forward recovery point.
            Some(save_preimage_locked(
                ctx,
                &mut ledger,
                cur_tree,
                cur_coverage,
            )?)
        } else {
            ledger.cursor_id
        };
        if let Some(pid) = preimage_id {
            if let Some(preimage) = ledger.snapshots.iter().find(|s| s.id == pid) {
                let recovery_ref = ctx.recovery_ref_name(pid);
                if ctx
                    .git(None, ["rev-parse", "--verify", &recovery_ref])
                    .is_err()
                {
                    ctx.update_ref_create(&recovery_ref, &preimage.commit)?;
                }
            }
        }

        let mut journal = Journal::new(JournalKind::Restore, ledger.generation);
        journal.target_id = Some(id);
        journal.preimage_id = preimage_id;
        // E5.1: stage the worktree the way a checkpoint sees it and journal what the restore will
        // remove BEFORE it removes anything.
        let staged = stage_for_apply(ctx)?;
        let removed = paths_absent_from(ctx, &staged.0, &target.commit)?;
        journal.removed = removed.clone();
        journal.phase = JournalPhase::Applying;
        ctx.save_journal(&journal)?;

        let rollback = |original: anyhow::Error| -> anyhow::Error {
            let result = preimage_id
                .and_then(|pid| ledger.snapshots.iter().find(|s| s.id == pid))
                .context("restore preimage unavailable")
                .and_then(|preimage| {
                    apply_tree(ctx, &preimage.commit)?;
                    let (actual, _) = current_tree(ctx)?;
                    if actual != preimage.tree {
                        bail!("rollback tree verification failed");
                    }
                    Ok(preimage.id)
                });
            let detail = match result {
            Ok(id) => format!("rollback to preimage #{id} verified; recovery journal preserved"),
            Err(error) => format!("rollback FAILED or unverifiable ({error:#}); partial tree changes may remain; recovery journal preserved"),
        };
            original.context(detail)
        };
        if let Err(apply_error) = apply_staged(ctx, &staged, &target.commit) {
            return Err(rollback(apply_error));
        }
        let (actual, _) = current_tree(ctx).map_err(&rollback)?;
        if actual != target.tree {
            return Err(rollback(anyhow::anyhow!("restore verification failed")));
        }

        ledger.set_cursor(Some(id));
        ctx.save_ledger(&mut ledger).context(
            "target tree applied and verified, but restore ledger commit failed; journal preserved",
        )?;
        if let Some(pid) = preimage_id {
            let recovery_ref = ctx.recovery_ref_name(pid);
            if let Ok(oid) = ctx.git(None, ["rev-parse", "--verify", &recovery_ref]) {
                let _ = ctx.update_ref_delete(&recovery_ref, &oid);
            }
        }
        journal.phase = JournalPhase::LedgerCommitted;
        ctx.save_journal(&journal)?;
        ctx.clear_journal()?;
        Ok((target, removed))
    })
}

fn save_preimage_locked(
    ctx: &RepoContext,
    ledger: &mut Ledger,
    tree: String,
    coverage: Coverage,
) -> Result<u32> {
    Ok(capture_checkpoint_locked(
        ctx,
        ledger,
        tree,
        coverage,
        "before time-travel",
        true,
        None,
        true,
    )?
    .id)
}

/// The checkpoints an `undo` would move between — `(current, target)` — without moving. The
/// REPL shows the change this implies (a diff stat from the working tree to `target`) and asks
/// before applying when the tree holds work no checkpoint has.
pub fn undo_target() -> Result<(u32, u32)> {
    let ctx = RepoContext::current()?;
    let ledger = ctx.load_ledger()?;
    let current = ledger
        .cursor_id
        .or_else(|| ledger.snapshots.last().map(|s| s.id))
        .context("no checkpoints yet — save one with `aizen time save`")?;
    let parent = ledger
        .snapshots
        .iter()
        .find(|s| s.id == current)
        .and_then(|s| s.parent)
        .context("already at the oldest checkpoint")?;
    Ok((current, parent))
}

/// Does the working tree differ from checkpoint `id`? `true` means a rewind would discard work
/// that no checkpoint holds.
pub fn working_tree_differs_from(id: u32) -> Result<bool> {
    Ok(
        !diff(&DiffSide::Checkpoint(id), &DiffSide::Working, &[], None)?
            .files
            .is_empty(),
    )
}

/// Rewind one checkpoint, also naming the files it removed because the checkpoint never had them.
pub fn undo_with_report() -> Result<(Snapshot, Vec<String>)> {
    require_workspace_restore()?;
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time undo",
    )?;
    let ledger = ctx.load_ledger()?;
    let current = ledger
        .cursor_id
        .or_else(|| ledger.snapshots.last().map(|s| s.id))
        .context("no checkpoints yet — save one with `aizen time save`")?;
    let parent = ledger
        .snapshots
        .iter()
        .find(|s| s.id == current)
        .and_then(|s| s.parent)
        .context("already at the oldest checkpoint")?;
    restore_in_reported(&ctx, parent)
}

pub fn redo() -> Result<Snapshot> {
    redo_with_report().map(|(snap, _)| snap)
}

/// [`redo`], also naming the files it removed because the checkpoint never had them.
pub fn redo_with_report() -> Result<(Snapshot, Vec<String>)> {
    require_workspace_restore()?;
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time redo",
    )?;
    let ledger = ctx.load_ledger()?;
    let current = ledger
        .cursor_id
        .or_else(|| ledger.snapshots.last().map(|s| s.id))
        .context("no checkpoints yet")?;
    let child = ledger
        .snapshots
        .iter()
        .filter(|s| s.parent == Some(current) && !s.recovery)
        .max_by_key(|s| s.id)
        .map(|s| s.id)
        .context("already at the newest checkpoint on this branch")?;
    restore_in_reported(&ctx, child)
}

fn timeline_in(ctx: &RepoContext) -> Result<(Vec<Snapshot>, Option<usize>)> {
    let mut ledger = ctx.load_ledger()?;
    ledger.normalize()?;
    Ok((ledger.snapshots, ledger.cursor))
}

pub fn timeline() -> Result<(Vec<Snapshot>, Option<usize>)> {
    timeline_in(&RepoContext::current()?)
}

// ───────────────────────────── diff between points in time ─────────────────────────────

/// One changed path between two timeline points. `added`/`deleted` are `None` for a binary file,
/// which is exactly how Git reports it (`-` in `--numstat`) — the distinction matters because "0
/// lines changed" and "not a text file" would otherwise read identically.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    /// Git status letter: `A`dded, `M`odified, `D`eleted, `R`enamed, `C`opied, `T`ype-changed.
    pub status: char,
    pub path: String,
    /// Present only for renames/copies: where the content came from.
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// Which point in time a diff side refers to. The working tree is a first-class side so "what have I
/// changed since checkpoint #5" needs no intermediate checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSide {
    Checkpoint(u32),
    Working,
}

impl DiffSide {
    /// Accepts a checkpoint id, or `working`/`now`/`wt` for the live tree.
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "working" | "worktree" | "wt" | "now" | "current" | "disk" => Some(Self::Working),
            _ => t
                .trim_start_matches('#')
                .parse::<u32>()
                .ok()
                .map(Self::Checkpoint),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Checkpoint(id) => format!("#{id}"),
            Self::Working => "working tree".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub from: String,
    pub to: String,
    pub files: Vec<FileChange>,
    /// Unified patch text, when requested. Truncated at a byte ceiling — see `patch_truncated`.
    pub patch: Option<String>,
    pub patch_truncated: bool,
}

impl DiffReport {
    pub fn total_added(&self) -> u64 {
        self.files.iter().filter_map(|f| f.added).sum()
    }
    pub fn total_deleted(&self) -> u64 {
        self.files.iter().filter_map(|f| f.deleted).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Resolve a diff side to a Git tree oid. `Working` writes a fresh tree object into the private
/// store (unreferenced, reclaimed by ordinary Git maintenance) so the live worktree can be diffed
/// with the same plumbing as any checkpoint — no special-casing downstream.
fn side_tree(ctx: &RepoContext, ledger: &Ledger, side: &DiffSide) -> Result<String> {
    match side {
        DiffSide::Checkpoint(id) => {
            let snap = ledger
                .snapshots
                .iter()
                .find(|s| s.id == *id)
                .with_context(|| format!("no checkpoint #{id} (see `aizen time list`)"))?;
            ctx.validate_snapshot_ref(snap)?;
            Ok(snap.tree.clone())
        }
        DiffSide::Working => Ok(current_tree(ctx)?.0),
    }
}

/// Split a `-z` (NUL-delimited) plumbing stream. Git only quotes paths in the non-`-z` forms, so
/// this is the parse that survives non-ASCII and embedded-newline paths intact.
fn nul_fields(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Merge git's two machine-readable diff streams into one row per changed path.
///
/// Two calls are needed because git has no single format carrying both the status letter (with rename
/// pairing) and the line counts: `--name-status` has the former, `--numstat` the latter. Kept pure
/// and byte-in so the `-z` framing — the part that silently mis-parses on renames — is unit-testable
/// without a repository.
///
/// Framing, both with `-z`:
///   - `--name-status`: `<status>\0<path>\0`, or `R<score>\0<old>\0<new>\0` for renames/copies.
///   - `--numstat`: `<add>\t<del>\t<path>\0` normally, but for a rename the trailing path is EMPTY and
///     the pair follows as `\0<old>\0<new>\0` — so a naive one-field-per-row read desyncs the whole
///     remaining stream.
///
/// A `-` count means binary; it maps to `None` rather than `0` so "no text change" stays distinct
/// from "not text".
fn merge_diff_streams(name_status: &[u8], numstat: &[u8]) -> Vec<FileChange> {
    let parse_count = |v: &str| {
        if v == "-" {
            None
        } else {
            v.parse::<u64>().ok()
        }
    };

    let mut counts: std::collections::HashMap<String, (Option<u64>, Option<u64>)> =
        std::collections::HashMap::new();
    let num_fields = nul_fields(numstat);
    let mut i = 0;
    while i < num_fields.len() {
        let mut parts = num_fields[i].split('\t');
        let add = parts.next().unwrap_or("");
        let del = parts.next().unwrap_or("");
        let inline = parts.next().unwrap_or("");
        if !inline.is_empty() {
            counts.insert(inline.to_string(), (parse_count(add), parse_count(del)));
            i += 1;
        } else if i + 2 < num_fields.len() {
            // Rename/copy: counts row, then old path, then new path. Key on the new path, which is
            // what `--name-status` reports as the row's path.
            counts.insert(
                num_fields[i + 2].clone(),
                (parse_count(add), parse_count(del)),
            );
            i += 3;
        } else {
            break;
        }
    }

    let name_fields = nul_fields(name_status);
    let mut files = Vec::new();
    let mut j = 0;
    while j + 1 < name_fields.len() {
        let status = name_fields[j].chars().next().unwrap_or('M');
        let paired = matches!(status, 'R' | 'C');
        let (path, old_path, step) = if paired && j + 2 < name_fields.len() {
            (
                name_fields[j + 2].clone(),
                Some(name_fields[j + 1].clone()),
                3,
            )
        } else {
            (name_fields[j + 1].clone(), None, 2)
        };
        let (added, deleted) = counts.get(&path).copied().unwrap_or((None, None));
        files.push(FileChange {
            status,
            path,
            old_path,
            added,
            deleted,
        });
        j += step;
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Diff two points in the timeline: checkpoint↔checkpoint, or checkpoint↔working tree.
///
/// This is the read half of the time machine. Restoring already worked, but without a diff the only
/// way to react to a bad edit was to discard the whole tree — you could not see *which* file went
/// wrong, so a one-line mistake cost every good change made alongside it.
///
/// `patch_limit` caps the unified-patch bytes; `None` means stat-only (always cheap, so it stays
/// usable on a huge change set). `paths` narrows the diff, which is what makes a broad rewind
/// unnecessary in the common case.
pub fn diff(
    from: &DiffSide,
    to: &DiffSide,
    paths: &[String],
    patch_limit: Option<usize>,
) -> Result<DiffReport> {
    diff_in(&RepoContext::current()?, from, to, paths, patch_limit)
}

fn diff_in(
    ctx: &RepoContext,
    from: &DiffSide,
    to: &DiffSide,
    paths: &[String],
    patch_limit: Option<usize>,
) -> Result<DiffReport> {
    // Shared lease only: diffing writes no refs and mutates no worktree, but it must not race a
    // store-exclusive GC sweep that could reap an object mid-read.
    let _store = ctx.store_shared()?;
    let mut ledger = ctx.load_ledger()?;
    ledger.normalize()?;
    let from_tree = side_tree(ctx, &ledger, from)?;
    let to_tree = side_tree(ctx, &ledger, to)?;

    if from_tree == to_tree {
        return Ok(DiffReport {
            from: from.label(),
            to: to.label(),
            files: Vec::new(),
            patch: None,
            patch_truncated: false,
        });
    }

    let pathspec: Vec<String> = if paths.is_empty() {
        Vec::new()
    } else {
        let mut v = vec!["--".to_string()];
        v.extend(paths.iter().cloned());
        v
    };

    // Two plumbing calls rather than one: `--name-status` carries the status letter (and rename
    // pairing), `--numstat` carries the line counts. Git has no combined machine format that gives
    // both, and merging them here is cheaper than making callers run git twice.
    let mut name_args = vec![
        "diff-tree".to_string(),
        "-r".to_string(),
        "-z".to_string(),
        "--find-renames".to_string(),
        "--no-commit-id".to_string(),
        "--name-status".to_string(),
        from_tree.clone(),
        to_tree.clone(),
    ];
    name_args.extend(pathspec.iter().cloned());
    let name_out = ctx.git_output(None, &name_args)?;
    if !name_out.status.success() {
        bail!(
            "diff failed: {}",
            String::from_utf8_lossy(&name_out.stderr).trim()
        );
    }

    let mut num_args = vec![
        "diff-tree".to_string(),
        "-r".to_string(),
        "-z".to_string(),
        "--find-renames".to_string(),
        "--no-commit-id".to_string(),
        "--numstat".to_string(),
        from_tree.clone(),
        to_tree.clone(),
    ];
    num_args.extend(pathspec.iter().cloned());
    let num_out = ctx.git_output(None, &num_args)?;
    if !num_out.status.success() {
        bail!(
            "diff failed: {}",
            String::from_utf8_lossy(&num_out.stderr).trim()
        );
    }

    let files = merge_diff_streams(&name_out.stdout, &num_out.stdout);

    let (patch, patch_truncated) = match patch_limit {
        None => (None, false),
        Some(limit) => {
            let mut args = vec![
                "diff-tree".to_string(),
                "-r".to_string(),
                "-p".to_string(),
                "--find-renames".to_string(),
                "--no-commit-id".to_string(),
                from_tree,
                to_tree,
            ];
            args.extend(pathspec.iter().cloned());
            let out = ctx.git_output(None, &args)?;
            if !out.status.success() {
                bail!(
                    "diff failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            if text.len() > limit {
                // Cut on a char boundary so multi-byte content can't produce invalid UTF-8.
                let mut cut = limit;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                (Some(text[..cut].to_string()), true)
            } else {
                (Some(text), false)
            }
        }
    };

    Ok(DiffReport {
        from: from.label(),
        to: to.label(),
        files,
        patch,
        patch_truncated,
    })
}

// ---------------------------------------------------------------------------------------------
// Reading one file at one point in the timeline
// ---------------------------------------------------------------------------------------------

/// One entry from a tree: the mode Git recorded, the blob's oid, and its exact bytes.
#[derive(Debug, Clone)]
pub struct TreeBlob {
    pub mode: String,
    pub oid: String,
    /// Verbatim blob content. Never decoded, so a file that is not valid UTF-8 survives a round trip.
    pub bytes: Vec<u8>,
}

/// Tree oid behind a timeline side.
///
/// Public because resolving `Working` writes a fresh tree from the whole index: a caller reading
/// several files from one point in time has to pay that once, not once per path.
pub fn resolve_tree(side: &DiffSide) -> Result<String> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let mut ledger = ctx.load_ledger()?;
    ledger.normalize()?;
    side_tree(&ctx, &ledger, side)
}

/// Exact bytes of `path` inside `tree`; `None` when the tree holds no blob there.
///
/// Reads through the private store, whose sealed alternates reach the repository's own objects — so
/// trees written by checkpoints and trees that are plain repository history both resolve here.
pub fn blob_in_tree(tree: &str, path: &str) -> Result<Option<TreeBlob>> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let listing = ctx.git(None, ["ls-tree", "-r", "-z", tree, "--", path])?;
    let Some(entry) = listing.split('\0').find(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // `<mode> SP <type> SP <oid> TAB <path>`. Only the metadata ahead of the tab is needed, so the
    // trailing path — the one field `-z` leaves unquoted and untrimmed — is never parsed.
    let meta = entry.split('\t').next().unwrap_or(entry);
    let mut fields = meta.split_whitespace();
    let mode = fields.next().unwrap_or_default().to_string();
    let kind = fields.next().unwrap_or_default().to_string();
    let oid = fields.next().unwrap_or_default().to_string();
    if kind != "blob" || oid.is_empty() {
        // A gitlink (submodule) or a subtree holds no content a caller could compose.
        return Ok(None);
    }
    let out = ctx.git_output(None, ["cat-file", "blob", &oid])?;
    if !out.status.success() {
        bail!(
            "reading {path} at tree {tree}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(Some(TreeBlob {
        mode,
        oid,
        bytes: out.stdout,
    }))
}

/// Every checkpoint id in this worktree's ledger, ascending.
///
/// Ids are allocated monotonically, so ascending id IS chronological order, and two consecutive ids
/// bracket exactly the work done between those two snapshots. Since the workspace writer lease is
/// held for a whole turn, that interval is one turn by one session — which is what lets a caller
/// attribute it, and reconstruct one session's version of a file the other also edited.
pub fn checkpoint_ids() -> Result<Vec<u32>> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let mut ledger = ctx.load_ledger()?;
    ledger.normalize()?;
    let mut ids: Vec<u32> = ledger.snapshots.iter().map(|s| s.id).collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub repo_id: String,
    pub worktree_id: String,
    pub store: String,
    pub checkpoints: usize,
    /// Loose objects sitting in the store right now. Reported because this is the number that used to
    /// grow forever with nothing surfacing it: a store can be perfectly healthy and still be tens of
    /// thousands of files. `aizen time gc` packs them.
    pub loose_objects: usize,
    pub issues: Vec<String>,
}

pub fn doctor() -> Result<DoctorReport> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut issues = Vec::new();
    match require_settled_speculation(&ctx.namespace_dir) {
        Ok(summary) if summary.read_dependency_concerns > 0 => issues.push(format!(
            "{} retained speculative peer-read concern(s) require semantic review; historical byte/command verification is not semantic reconciliation or current-source verification",
            summary.read_dependency_concerns,
        )),
        Ok(_) => {} // Accepted historical evidence is not a re-verification of today's source.
        Err(error) => issues.push(format!("{error:#}")),
    }
    if !ctx.store_git_dir.join("HEAD").exists() {
        issues.push(format!(
            "private store missing at {}",
            ctx.store_git_dir.display()
        ));
    }
    let alternates = ctx
        .store_git_dir
        .join("objects")
        .join("info")
        .join("alternates");
    match crate::core::persist::read_optional(&alternates) {
        Ok(None) => issues.push("private store alternates pointer is missing".into()),
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.len() != 1 {
                issues.push(format!(
                    "private store alternates is not sealed (expected 1 entry, found {})",
                    lines.len()
                ));
            }
        }
        Err(e) => issues.push(format!("private store alternates unreadable: {e}")),
    }
    // Counted before the report is built: both constructions below move fields out of `ctx`.
    let loose_objects = ctx.loose_object_ids().len();
    let ledger = match ctx.load_ledger() {
        Ok(l) => l,
        Err(e) => {
            issues.push(e.to_string());
            return Ok(DoctorReport {
                ok: false,
                repo_id: ctx.repo_id,
                worktree_id: ctx.worktree_id,
                store: ctx.store_git_dir.display().to_string(),
                checkpoints: 0,
                loose_objects,
                issues,
            });
        }
    };
    match ctx.load_journal() {
        Ok(Some(journal)) => issues.push(format!(
            "unfinished {:?} transaction {} ({:?})",
            journal.kind, journal.operation_id, journal.phase
        )),
        Ok(None) => {}
        Err(error) => issues.push(format!("transaction journal: {error:#}")),
    }
    for snap in &ledger.snapshots {
        if let Err(e) = ctx.validate_snapshot_ref(snap) {
            issues.push(format!("checkpoint #{}: {e}", snap.id));
        }
        if snap.has_chat {
            match read_chat_in(&ctx, snap.id) {
                Ok(Some(_)) => {}
                Ok(None) => issues.push(format!(
                    "checkpoint #{} claims chat but sidecar is missing",
                    snap.id
                )),
                Err(e) => issues.push(format!("checkpoint #{} chat: {e}", snap.id)),
            }
        }
    }
    Ok(DoctorReport {
        ok: issues.is_empty(),
        repo_id: ctx.repo_id,
        worktree_id: ctx.worktree_id,
        store: ctx.store_git_dir.display().to_string(),
        checkpoints: ledger.snapshots.len(),
        loose_objects,
        issues,
    })
}

pub fn doctor_repair() -> Result<DoctorReport> {
    require_workspace_restore()?;
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time doctor repair",
    )?;
    let _physical = crate::core::workspace_txn::PhysicalEffectLease::exclusive_without_background(
        LOCK_TIMEOUT,
        None,
        "time restore",
        &crate::core::exec_ctx::current()
            .unwrap_or_default()
            .resource_scope(),
    )?;
    let body_context = crate::core::exec_ctx::current()
        .unwrap_or_default()
        .with_physical_effect_lease(_physical.clone())
        .with_writer_lease(_workspace.clone());
    crate::core::exec_ctx::with_current(body_context, || {
        let _quiescence = crate::core::workspace_txn::WorkspaceRestoreGuard::acquire(&ctx.root)?;
        let _store = ctx.store_shared()?;
        let _lock = ctx.lock()?;
        let mut ledger = ctx.load_ledger()?;
        ctx.recover_pending_with_restore(&mut ledger, true)?;
        drop(_lock);
        drop(_store);
        doctor()
    })
}

/// Returns the health report plus what compaction reclaimed, so the CLI can report bytes rather than
/// only "cleaned". The compaction half is `None` when packing could not run — a store that will not
/// compact is still a store worth reporting on.
pub fn doctor_gc() -> Result<(DoctorReport, Option<CompactReport>, Option<PruneReport>)> {
    let ctx = RepoContext::current()?;
    // Whole-store sweep: block every sibling worktree's ref creation/deletion for the scan+reap so a
    // concurrent `save` can't slip a ref past `for-each-ref`. Store-exclusive ordered before the
    // per-worktree metadata lock.
    let _store = ctx.store_exclusive()?;
    let _lock = ctx.lock()?;
    require_settled_store_speculation(
        ctx.store_git_dir
            .parent()
            .context("Time Machine store has no root")?,
        Some(&ctx.namespace_dir),
    )?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    let mut journal = Journal::new(JournalKind::Doctor, ledger.generation);
    ctx.save_journal(&journal)?;

    let live_ids: HashSet<u32> = ledger.snapshots.iter().map(|s| s.id).collect();
    // Sweep the whole private store's `refs/ng/tm/**` (all worktrees). Orphans from other worktree
    // ids or recovery pins must not survive just because this process is bound to one prefix.
    let refs = ctx.git(
        None,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/ng/tm",
        ],
    )?;
    for line in refs.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let Some(oid) = fields.next() else { continue };
        let pin_guard = if let Some(id) =
            name.rsplit('/').next().and_then(|s| s.parse::<u32>().ok())
        {
            let worktree = name
                .strip_prefix("refs/ng/tm/")
                .and_then(|rest| rest.split('/').next());
            let pin_path = worktree.map(|wt| {
                ctx.store_git_dir
                    .parent()
                    .unwrap()
                    .join("worktrees")
                    .join(wt)
                    .join("pins")
                    .join(format!("{id}.lock"))
            });
            match pin_path.map(|path| {
                crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&path, Duration::ZERO)
            }) {
                Some(Ok(guard)) => Some(guard),
                Some(Err(error))
                    if error
                        .downcast_ref::<crate::core::repo_lock::LockBusy>()
                        .is_some() =>
                {
                    continue
                }
                Some(Err(error)) => return Err(error).context("GC checkpoint pin probe failed"),
                None => None,
            }
        } else {
            None
        };
        let _pin_guard = pin_guard;
        if name.contains("/recovery/") {
            // Only delete recovery pins that belong to this worktree's namespace, or that have no
            // matching live snapshot id anywhere.
            let id = name.rsplit('/').next().and_then(|s| s.parse::<u32>().ok());
            let ours = name.starts_with(&(ctx.ref_prefix.clone() + "/"));
            if ours || id.is_some_and(|id| !live_ids.contains(&id)) {
                ctx.update_ref_delete(name, oid)?;
            }
            continue;
        }
        let id = name.rsplit('/').next().and_then(|s| s.parse::<u32>().ok());
        let ours = name.starts_with(&(ctx.ref_prefix.clone() + "/")) || name == ctx.ref_prefix;
        // Delete: (a) orphan ids under our prefix, or (b) any ref under a dead worktree prefix that
        // is not our live prefix (foreign worktree namespaces keep their own live ledgers — only
        // delete ids that are under our prefix and not live).
        if ours {
            if id.is_some_and(|id| !live_ids.contains(&id)) {
                ctx.update_ref_delete(name, oid)?;
                if let Some(id) = id {
                    let _ = crate::core::persist::remove_if_exists(&ctx.chat_path(id));
                }
            }
        } else if id.is_some() {
            // Foreign worktree ref: leave alone — that worktree owns its ledger. The failpoint
            // probe plants `refs/ng/tm/wt-dead/999`; treat any non-matching worktree id as orphan
            // only when its path segment is not a known sibling ledger on disk.
            let foreign_wt = name
                .trim_start_matches("refs/ng/tm/")
                .split('/')
                .next()
                .unwrap_or("");
            if !foreign_wt.is_empty() && foreign_wt != ctx.worktree_id {
                let sibling_ledger = ctx
                    .store_git_dir
                    .parent()
                    .map(|p| p.join("worktrees").join(foreign_wt).join("ledger.json"));
                let sibling_exists = sibling_ledger.as_ref().is_some_and(|p| p.exists());
                if !sibling_exists {
                    ctx.update_ref_delete(name, oid)?;
                }
            }
        }
    }
    if ctx.chat_dir().is_dir() {
        for entry in fs::read_dir(ctx.chat_dir())? {
            let path = entry?.path();
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok());
            if let Some(id) = id.filter(|id| !live_ids.contains(id)) {
                let (protected, _pin) = ctx.deletion_pins([id])?;
                if protected.is_empty() {
                    let _ = crate::core::persist::remove_if_exists(&path)?;
                }
            }
        }
    }
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    // Refs are reaped; now reclaim the bytes. This is the one place holding the store EXCLUSIVELY,
    // and an explicit `gc` is exactly when the user is asking for space back — so compact
    // unconditionally here rather than waiting for `LOOSE_COMPACT_THRESHOLD`. Best-effort: a store
    // that cannot be packed is still a healthy store, and `doctor()` below reports what it finds.
    // E5.4: drop what no ref reaches first (re-packing the rest by OID), then pack whatever
    // loose objects remain. Both best-effort: a store that cannot be pruned is still healthy.
    let pruned = ctx.prune_unreachable().ok();
    let compacted = ctx.compact_objects().ok();
    drop(_lock);
    drop(_store);
    Ok((doctor()?, compacted, pruned))
}

/// One private store found while sweeping `~/.aizen/timemachine/`. Two ways a store is an ORPHAN:
/// `source_exists == false` (the repo it alternates onto is gone), or `superseded_by` is set (the
/// repo is alive, but the identity registry serves it through a DIFFERENT store, so no resolve can
/// ever key back to this one). Either way, dead weight no discovery will reach again.
#[derive(Debug, Clone, Serialize)]
pub struct StoreEntry {
    pub repo_id: String,
    /// Absolute source `.git/objects` path recorded in the store's sealed alternates pointer.
    pub source: Option<String>,
    pub source_exists: bool,
    /// Named by the home identity registry. An unregistered store is reachable only through
    /// legacy-id grandfathering — which stops working the moment its repo registers under another
    /// id. `true` also when the registry itself was unreadable: unknown is not reportable garbage.
    pub registered: bool,
    /// The registered store now serving this store's source repo. Set only for an unregistered
    /// store whose source still exists: `resolve` answers that path with the registered id (exact
    /// hit wins) and grandfathering hashes only the current path, so this store has been
    /// permanently out-keyed — the id-migration leftovers `gc --all` exists to find.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Whole-store/trash collection must retain speculative evidence, including completed records.
    /// A parse/traversal failure is a preservation reason too, never proof that a store is disposable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preservation_reason: Option<String>,
    pub bytes: u64,
    pub checkpoints: usize,
}

/// What one [`RepoContext::prune_unreachable`] pass reclaimed.
#[derive(Debug, Clone, Serialize)]
pub struct PruneReport {
    /// Objects the store owned before the prune (loose + in its own packs).
    pub owned: usize,
    /// Of those, the ones no live ref reached — removed.
    pub pruned: usize,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// What one [`RepoContext::compact_objects`] pass moved into a pack.
#[derive(Debug, Clone, Serialize)]
pub struct CompactReport {
    /// Loose objects handed to `pack-objects`.
    pub packed: usize,
    /// Size of `objects/` before and after, so a caller can report what was actually reclaimed
    /// rather than what was attempted.
    pub before_bytes: u64,
    pub after_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcAllReport {
    /// Every store under the home root, newest-scan order.
    pub stores: Vec<StoreEntry>,
    /// Orphan stores: subset of `stores` whose source repo is gone, or whose source is alive but
    /// served by a different registered store (`superseded_by`).
    pub orphans: Vec<StoreEntry>,
    /// True when `--apply` actually moved orphans to `.trash/`; false for a dry run.
    pub applied: bool,
    /// Where orphans were moved to on `--apply` (the `.trash/<timestamp>` dir), if any.
    pub trash_dir: Option<String>,
}

/// Bounded best-effort size estimate. Never follow links/reparse directories during a GC report;
/// inspection failures/truncation cannot make evidence collectible (classification is separate).
fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut visited = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = checked_metadata_entries(&directory) else {
            continue;
        };
        for path in entries {
            if visited >= MAX_SPECULATION_SCAN_ENTRIES {
                return total;
            }
            visited += 1;
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if crate::core::file_placement::is_link_or_reparse(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

/// Count checkpoints across every worktree ledger in a store (sum of `snapshots`). Reads the ledger
/// JSON directly rather than through `Ledger` so a schema the current binary cannot fully parse still
/// yields a usable count. Best-effort: a missing/garbage ledger contributes 0.
fn store_checkpoint_count(store_root: &Path) -> usize {
    let Ok(namespaces) = checked_metadata_entries(&store_root.join("worktrees")) else {
        return 0;
    };
    let mut count = 0usize;
    for namespace in namespaces.into_iter().take(MAX_SPECULATION_WORKTREES) {
        let ledger = namespace.join("ledger.json");
        if let Ok(bytes) = read_speculation_evidence(&ledger, MAX_SPECULATION_REGISTRY_BYTES) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(rows) = value.get("snapshots").and_then(serde_json::Value::as_array) {
                    count = count.saturating_add(rows.len());
                }
            }
        }
    }
    count
}

/// One directory, as every writer spells it: the registry stores `//?/C:/…` (canonicalized, then
/// backslashes forwarded), an alternates file stores plain `C:/…`, and older writers used `\\?\C:\…`.
/// Verbatim prefix dropped, slashes forwarded, trailing slashes trimmed, case folded on Windows —
/// where paths compare case-insensitively; on Unix case stays significant, so it is kept.
fn dir_key(raw: &str) -> String {
    let mut s = raw.trim().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/UNC/") {
        s = format!("//{rest}");
    } else if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Classify one swept store against the identity registry: is it registered, and if not, which
/// registered store has taken over its source repo. A `None` registry (missing, unreadable,
/// corrupt) disables the superseded class entirely and reports the store as registered — the same
/// fail-open posture as [`resolve_identity`]: a broken registry must never be able to turn every
/// legacy store on the machine into reportable garbage.
fn classify_store(
    repo_id: &str,
    source: Option<&str>,
    source_exists: bool,
    registry: Option<&identity::Registry>,
) -> (bool, Option<String>) {
    let Some(reg) = registry else {
        return (true, None);
    };
    if reg.repos.iter().any(|r| r.repo_id == repo_id) {
        return (true, None);
    }
    // Superseded needs a LIVE source: a dead-source store is already the first orphan class, and
    // matching a registered entry against a path that no longer exists would prove nothing.
    let Some(src) = source.filter(|_| source_exists) else {
        return (false, None);
    };
    // The alternates line is `<common_git_dir>/objects`; the registry keys repos by the common
    // git dir itself.
    let Some(common) = dir_key(src).strip_suffix("/objects").map(str::to_string) else {
        return (false, None);
    };
    let by = reg
        .repos
        .iter()
        .find(|r| dir_key(&r.common_git_dir) == common)
        .map(|r| r.repo_id.clone());
    (false, by)
}

/// Home-level store sweep. Unlike [`doctor_gc`] (which cleans refs/sidecars WITHIN the current repo's
/// store), this walks EVERY `~/.aizen/timemachine/repo-*` and finds ORPHAN stores: those whose source
/// repo no longer exists on disk, and those out-keyed by an identity migration — the repo is alive
/// but registered under a different store id, so no resolve can reach the old store again (exact
/// path hit wins, and grandfathering hashes only the current path). Dry-run by default
/// (`apply == false`) — it only reports. With `apply == true` it moves each orphan into
/// `~/.aizen/timemachine/.trash/<timestamp>/` (reversible, not an irrecoverable delete) and drops
/// any registry entry that named a reaped store. A store that is still reachable is never touched.
fn store_deletion_pins(root: &Path) -> Result<(bool, Vec<crate::core::repo_lock::RepoTxnLock>)> {
    let namespaces = checked_metadata_entries(&root.join("worktrees"))?;
    if namespaces.len() > MAX_SPECULATION_WORKTREES {
        bail!("store GC worktree traversal exceeds bound");
    }
    let mut live = false;
    let mut guards = Vec::new();
    for namespace in namespaces {
        for pin in checked_metadata_entries(&namespace.join("pins"))? {
            let resource = crate::core::workspace_txn::NativeResource::resolve(&pin)?;
            match crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&pin, Duration::ZERO) {
                Ok(guard) => guards.push(guard),
                Err(error)
                    if error
                        .downcast_ref::<crate::core::repo_lock::LockBusy>()
                        .is_some() =>
                {
                    live = true
                }
                Err(error) => return Err(error).context("store GC lifetime pin probe failed"),
            }
            resource.verify()?;
        }
    }
    Ok((live, guards))
}

pub fn gc_all(apply: bool) -> Result<GcAllReport> {
    let root = crate::core::config::aizen_home().join("timemachine");
    let mut stores = Vec::new();
    let mut orphans = Vec::new();

    // Read once for the whole sweep. Classification is a pure function over this parse; the apply
    // half re-reads under the registry lease before writing anything back.
    let registry = read_speculation_registry(&root).ok();

    let entries = checked_metadata_entries(&root)?;
    for store_root in entries {
        let name = store_root
            .file_name()
            .context("invalid Time Machine store name")?
            .to_string_lossy()
            .into_owned();
        // Only sweep repo stores. Skip our own `.trash/` and any unrelated dirs.
        if !name.starts_with("repo-") {
            continue;
        }
        let alternates = store_root
            .join("store.git")
            .join("objects")
            .join("info")
            .join("alternates");
        // The recorded source object dir, and whether it still exists. A MISSING alternates file is
        // treated as unknown (not an orphan): a store mid-creation must not be reaped.
        let (source, source_exists, known) = match read_speculation_evidence(&alternates, 16 * 1024)
        {
            Ok(bytes) => {
                let line = String::from_utf8_lossy(&bytes).trim().to_string();
                let exists = !line.is_empty() && Path::new(&line).exists();
                (Some(line), exists, true)
            }
            _ => (None, false, false),
        };
        let (registered, superseded_by) =
            classify_store(&name, source.as_deref(), source_exists, registry.as_ref());
        let preservation_reason = match store_has_speculation_evidence(&store_root) {
            Ok(true) => Some("speculation evidence retained (including completed records); whole-store collection is disabled".into()),
            Ok(false) => None,
            Err(error) => Some(format!("cannot safely rule out speculation evidence: {error:#}")),
        };
        let se = StoreEntry {
            repo_id: name,
            source,
            source_exists,
            registered,
            superseded_by,
            preservation_reason,
            bytes: dir_size_bytes(&store_root),
            checkpoints: store_checkpoint_count(&store_root),
        };
        // Orphan = alternates was readable AND its source path is gone, OR the source is alive but
        // another store now owns it in the registry. `known == false` (no alternates) is left
        // alone: a store mid-creation must not be reaped.
        if se.preservation_reason.is_none()
            && ((known && !source_exists) || se.superseded_by.is_some())
        {
            orphans.push(se.clone());
        }
        stores.push(se);
    }

    let mut trash_dir = None;
    if apply {
        // The trash is a reversible holding pen, not an archive: stamps a month old have had every
        // realistic "oops" window pass, and nothing else ever empties this dir. Purged only on
        // apply so a dry-run stays a pure report.
        const TRASH_TTL_SECS: u64 = 30 * 24 * 60 * 60;
        let now = std::time::SystemTime::now();
        for path in checked_metadata_entries(&root.join(".trash"))? {
            let metadata = fs::symlink_metadata(&path)?;
            let expired = metadata
                .modified()
                .ok()
                .and_then(|time| now.duration_since(time).ok())
                .is_some_and(|age| age.as_secs() > TRASH_TTL_SECS);
            if expired {
                // A failed inspection is NOT permission to delete. Aged evidence is evidence too.
                if !matches!(trash_has_speculation_evidence(&path), Ok(false)) {
                    continue;
                }
                let boundary = crate::core::workspace_txn::NativeResource::resolve(
                    &path.join(".tm-trash-boundary"),
                )?;
                boundary.verify()?;
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
    if apply && !orphans.is_empty() {
        // The same exclusive lease every identity resolve takes, held across the moves and the
        // registry rewrite: a concurrent save must not be able to register or re-key a store while
        // it is being carried out of the building.
        let reg_lock_path = root.join("registry.lock");
        let reg_guard = crate::core::workspace_txn::NativeResource::resolve(&reg_lock_path)?;
        let _reg_lock =
            crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&reg_lock_path, LOCK_TIMEOUT)?;
        reg_guard.verify()?;
        let locked_registry = Some(read_speculation_registry(&root)?);
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let trash = root.join(".trash").join(&stamp);
        fs::create_dir_all(&trash)
            .with_context(|| format!("creating trash dir {}", trash.display()))?;
        let mut retained_orphans = Vec::new();
        for o in &orphans {
            let from = root.join(&o.repo_id);
            let store_guard =
                crate::core::workspace_txn::NativeResource::resolve(&from.join("store.lock"))?;
            let _store = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                store_guard.path(),
                LOCK_TIMEOUT,
            )?;
            store_guard.verify()?;
            // Reclassify after both leases: a source may reconnect or a resolver may re-register
            // this store between the report scan and apply. A stale candidate is never authority.
            let alternates = from.join("store.git/objects/info/alternates");
            let bytes = read_speculation_evidence(&alternates, 16 * 1024)?;
            let source = String::from_utf8(bytes).context("store GC alternates is not UTF-8")?;
            let source = source.trim();
            if source.is_empty() {
                continue;
            }
            let source_exists = Path::new(source).exists();
            let (_, superseded) = classify_store(
                &o.repo_id,
                Some(source),
                source_exists,
                locked_registry.as_ref(),
            );
            if source_exists && superseded.is_none() {
                continue;
            }
            // Evidence can appear after the report scan; recheck while publication is excluded.
            if store_has_speculation_evidence(&from)? {
                continue;
            }
            let (pinned, _pins) = store_deletion_pins(&from)?;
            if pinned {
                continue;
            }
            let to = trash.join(&o.repo_id);
            fs::rename(&from, &to).with_context(|| {
                format!("moving orphan store {} to {}", from.display(), to.display())
            })?;
            retained_orphans.push(o.clone());
        }
        orphans = retained_orphans;
        // A registered store reaped for a dead source leaves its registry entry behind; drop those
        // so the registry only names stores that exist. Reloaded under the lease rather than
        // reusing the sweep's parse — the sweep read without it. Best-effort: the bytes are
        // already moved, and a registry bookkeeping failure must not report the gc as failed.
        let reg_path = root.join("registry.json");
        if let Ok(Some(bytes)) = crate::core::persist::read_optional(&reg_path) {
            if let Ok(mut reg) = serde_json::from_slice::<identity::Registry>(&bytes) {
                let before = reg.repos.len();
                reg.repos
                    .retain(|r| !orphans.iter().any(|o| o.repo_id == r.repo_id));
                if reg.repos.len() != before {
                    if let Ok(out) = serde_json::to_vec_pretty(&reg) {
                        let _ = crate::core::persist::atomic_write(&reg_path, &out);
                    }
                }
            }
        }
        if !orphans.is_empty() {
            trash_dir = Some(trash.to_string_lossy().replace('\\', "/"));
        }
    }

    Ok(GcAllReport {
        stores,
        orphans,
        applied: apply && trash_dir.is_some(),
        trash_dir,
    })
}

/// Which mutating Time Machine operation a `checkpoint` call selects. All three share
/// `is_destructive` (approval-gated) and the serial path, which is exactly why they can live in ONE
/// tool: the trait's `is_destructive` is a constant, so mixing in the read-only `list`/`diff` would
/// have forced those behind an approval prompt. They stay in `checkpoint_view`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointAction {
    Save,
    Rewind,
    Restore,
}

impl CheckpointAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "save" | "create" | "pin" | "snapshot" => Some(Self::Save),
            "rewind" | "undo" => Some(Self::Rewind),
            "restore" => Some(Self::Restore),
            _ => None,
        }
    }
}

/// Read the `action` of a `checkpoint` call. Kept module-level (not a method) so the agent loop can
/// ask "is this call a rewind?" without duplicating the alias table — see [`is_rewind_call`].
fn checkpoint_action(args: &serde_json::Value) -> Option<CheckpointAction> {
    args.get("action")
        .and_then(|v| v.as_str())
        .and_then(CheckpointAction::parse)
}

/// Is this tool call the run-scoped rewind — the RECOVERY path? The agent loop needs to know two
/// things about it that no other call shares: it must take the workspace writer lease even though
/// it is not a plain path write, and it must NOT be preceded by a pre-edit checkpoint (snapshotting
/// the broken tree before undoing it is exactly what the rewind is escaping). Before the merge those
/// were `tool.name() == "checkpoint_rewind"` string checks; now the action decides.
pub fn is_rewind_call(tool_name: &str, args: &serde_json::Value) -> bool {
    tool_name == "checkpoint" && checkpoint_action(args) == Some(CheckpointAction::Rewind)
}

/// The mutating half of the Time Machine surface: `save` (pin a restore point), `rewind` (undo this
/// run's own edits from a recovery anchor), `restore` (go back to any checkpoint by id). One tool
/// rather than three — the model picks an `action` instead of picking between three sibling names,
/// and the schema is advertised once per turn instead of three times.
pub struct Checkpoint {
    root: PathBuf,
}

#[derive(Clone)]
struct PreparedCheckpoint {
    root: PathBuf,
    recovery: RunRecovery,
    scope: String,
    anchor: Option<SnapshotHandle>,
}

impl Checkpoint {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl crate::agent::tools::Tool for Checkpoint {
    fn name(&self) -> &str {
        "checkpoint"
    }
    fn description(&self) -> &str {
        "Time Machine: save before risky work (edits auto-checkpoint); rewind this run (max 2/run); \
         restore id, even earlier turns. Inspect via checkpoint_view first. Files change, not chat; re-read."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["save", "rewind", "restore"],
                    "description": "save=pin restore point; rewind=undo run from anchor; restore=checkpoint id"
                },
                "label": {"type": "string", "description": "save: short note"},
                "target": {
                    "type": "string",
                    "enum": ["last_good", "pre_edit"],
                    "description": "rewind: last_good=run's last successful step; pre_edit=tree before run's first edit"
                },
                "id": {"type": "integer", "minimum": 1, "description": "restore: id from checkpoint_view"},
                "reason": {
                    "type": "string",
                    "description": "rewind/restore: short reason shown in result"
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &serde_json::Value) -> crate::agent::tools::WorkspaceEffect {
        // Args-aware, unlike `is_destructive`: a save only writes the private store (RepoMetadata),
        // while rewind/restore rewrite the working tree (Paths) and must take the writer lease. An
        // unparseable action falls back to Paths — the stricter of the two.
        match checkpoint_action(args) {
            Some(CheckpointAction::Save) => crate::agent::tools::WorkspaceEffect::RepoMetadata,
            _ => crate::agent::tools::WorkspaceEffect::Paths,
        }
    }
    fn prepare_call(
        &self,
        args: &serde_json::Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<crate::agent::tools::PreparedToolCall> {
        if matches!(
            checkpoint_action(args),
            Some(CheckpointAction::Rewind | CheckpointAction::Restore)
        ) {
            if !ctx.can_restore_workspace() {
                bail!(CHILD_RESTORE_DENIED);
            }
            ctx.recovery().require_workspace_restore()?;
        }
        let anchor = if is_rewind_call(self.name(), args) {
            let target = args
                .get("target")
                .and_then(|v| v.as_str())
                .and_then(RewindTarget::parse)
                .context("rewind target must be pre_edit or last_good")?;
            Some(ctx.recovery().anchor(&self.root, target)?)
        } else {
            None
        };
        let mut prepared = crate::agent::tools::PreparedToolCall::new(args.clone());
        prepared.delegation = Some(std::sync::Arc::new(PreparedCheckpoint {
            root: anchor
                .as_ref()
                .map(|h| h.root().to_path_buf())
                .unwrap_or_else(|| self.root.clone()),
            recovery: ctx.recovery().clone(),
            scope: ctx.resource_scope(),
            anchor,
        }));
        Ok(prepared)
    }

    fn workspace_target_prepared(
        &self,
        call: &crate::agent::tools::PreparedToolCall,
    ) -> Option<PathBuf> {
        call.delegation
            .as_ref()
            .and_then(|p| p.downcast_ref::<PreparedCheckpoint>())
            .map(|p| p.root.clone())
            .or_else(|| Some(self.root.clone()))
    }

    fn execute_prepared(
        &self,
        call: &crate::agent::tools::PreparedToolCall,
    ) -> Result<crate::agent::tools::ToolObservation> {
        let prepared = call
            .delegation
            .as_ref()
            .and_then(|p| p.downcast_ref::<PreparedCheckpoint>())
            .context("checkpoint has no internal execution preparation")?;
        self.execute_bound(&call.args, prepared)
            .map(crate::agent::tools::ToolObservation::text)
    }

    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let ctx = crate::core::exec_ctx::current().unwrap_or_default();
        let call = self.prepare_call(args, &ctx)?;
        self.execute_prepared(&call).map(|o| o.text)
    }
}

impl Checkpoint {
    fn execute_bound(
        &self,
        args: &serde_json::Value,
        prepared: &PreparedCheckpoint,
    ) -> Result<String> {
        let Some(action) = checkpoint_action(args) else {
            let raw = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            return Ok(format!(
                "error: `action` must be \"save\", \"rewind\", or \"restore\" (got {raw:?}). \
                 To list or diff checkpoints use `checkpoint_view`."
            ));
        };
        if matches!(action, CheckpointAction::Rewind | CheckpointAction::Restore) {
            prepared.recovery.require_workspace_restore()?;
        }
        match action {
            CheckpointAction::Save => self.save_point(args, prepared),
            CheckpointAction::Rewind => self.rewind(args, prepared),
            CheckpointAction::Restore => self.restore(args, prepared),
        }
    }
}

impl Checkpoint {
    fn save_point(
        &self,
        args: &serde_json::Value,
        prepared: &PreparedCheckpoint,
    ) -> Result<String> {
        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let snap = save_for(&prepared.recovery, &prepared.root, label, false, false)?
            .context("no git-visible checkpoint coverage for this workspace")?;
        Ok(format!(
            "checkpoint #{} saved ({}). Run-scoped rewind: action=rewind target=last_good|pre_edit. \
             Human free-form: `aizen time restore {}`.",
            snap.id,
            if snap.label.is_empty() { "no label" } else { &snap.label },
            snap.id
        ))
    }

    /// Agent-only, run-scoped rewind. Restores ONLY the pre-edit or last-good anchor of the current
    /// agent run — never arbitrary snapshot ids (those go through `action=restore`, or the human
    /// CLI). Cap: [`MAX_RUN_REWINDS`].
    fn rewind(&self, args: &serde_json::Value, prepared: &PreparedCheckpoint) -> Result<String> {
        let raw = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let Some(target) = RewindTarget::parse(raw) else {
            return Ok(
                "error: target must be \"last_good\" or \"pre_edit\" (run-scoped only; for a specific id use action=restore)"
                    .to_string(),
            );
        };
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let anchor = prepared
            .anchor
            .as_ref()
            .context("rewind anchor was not prepared")?;
        match prepared.recovery.rewind(anchor, &prepared.scope) {
            Ok(snap) => {
                let st = prepared.recovery.status(&prepared.root);
                let why = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" reason: {reason}.")
                };
                Ok(format!(
                    "rewound working tree to checkpoint #{} ({}, label={:?}).{why} \
                     rewinds left this run: {}. Re-read files you care about — contents changed on disk. \
                     Chat history was NOT restored.",
                    snap.id,
                    target.as_str(),
                    if snap.label.is_empty() { "none" } else { &snap.label },
                    st.rewinds_left,
                ))
            }
            Err(e) => Ok(format!("error: {e}")),
        }
    }

    /// Restore-by-id. Unlike `rewind` (run-scoped anchors only), this reaches ANY checkpoint in the
    /// ledger — what's needed when the user says "go back to how it was before" across turns. The
    /// loop takes the workspace writer lease for `Paths`, so the body uses the lease-free
    /// `restore_under_lease` (re-acquiring `workspace.lock` here would self-deadlock — see
    /// `rewind_run`).
    fn restore(&self, args: &serde_json::Value, prepared: &PreparedCheckpoint) -> Result<String> {
        let Some(id) = args.get("id").and_then(|v| v.as_u64()) else {
            return Ok(
                "error: `id` is required for action=restore (an integer checkpoint id — see `checkpoint_view`)"
                    .to_string(),
            );
        };
        let id = id as u32;
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let ctx = RepoContext::discover(&prepared.root)?;
        let lease = crate::core::workspace_txn::WorkspaceWriterLease::acquire_scoped(
            &ctx.root,
            LOCK_TIMEOUT,
            crate::core::cancel::current().as_ref(),
            "checkpoint restore",
            &prepared.scope,
        )?;
        let handle = {
            let _store = ctx.store_shared()?;
            let _lock = ctx.lock()?;
            let mut ledger = ctx.load_ledger()?;
            ctx.recover_pending(&mut ledger)?;
            let snap = ledger
                .snapshots
                .iter()
                .find(|s| s.id == id)
                .cloned()
                .context("checkpoint id not found")?;
            ctx.pin(snap)?
        };
        match handle.restore_under_lease(&lease) {
            Ok(snap) => {
                prepared.recovery.publish(handle, false)?;
                let why = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" reason: {reason}.")
                };
                Ok(format!(
                    "restored working tree to checkpoint #{} ({}).{why} Files only — chat history was NOT \
                     changed. The pre-restore tree was checkpointed; inspect the timeline for its exact \
                     recovery point. Ignored paths and external effects are not covered. Re-read files after restore.",
                    snap.id,
                    if snap.label.is_empty() { "no label" } else { &snap.label },
                ))
            }
            Err(e) => Ok(format!(
                "error: {e:#}. Restore did not commit successfully; partial changes may remain. Inspect the recovery journal with `aizen time doctor`."
            )),
        }
    }
}

/// One timeline row shaped for the model: id, whether it is the current cursor, label, whether a
/// chat sidecar exists, and the creation time. Absolute paths / store internals never leak here.
fn format_timeline_row(snap: &Snapshot, is_cursor: bool) -> String {
    let marker = if is_cursor { "▸" } else { " " };
    let label = if snap.label.is_empty() {
        "(no label)"
    } else {
        &snap.label
    };
    let kind = if snap.auto { "auto" } else { "manual" };
    format!("{marker} #{} — {label} [{kind}, {}]", snap.id, snap.created)
}

/// The READ half of Time Machine: timeline, diff, and parent-only restart inspection.
/// The restart report serializes admission internally but never mutates source or settles pending
/// evidence. Keeping reads separate from [`Checkpoint`] avoids approval/checkpoint side effects.
pub struct CheckpointView {
    root: PathBuf,
}

impl CheckpointView {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    /// Which read action was requested. Defaults to `diff`: "what have my edits done so far" is the
    /// overwhelmingly common question, and it is the one the model should ask BEFORE a rewind.
    fn action_of(args: &serde_json::Value) -> Option<&'static str> {
        match args
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("") | Some("diff") => Some("diff"),
            Some("list") | Some("timeline") => Some("list"),
            Some("recovery_report") => Some("recovery_report"),
            _ => None,
        }
    }
}

impl crate::agent::tools::Tool for CheckpointView {
    fn name(&self) -> &str {
        "checkpoint_view"
    }
    fn description(&self) -> &str {
        "Read-only Time Machine: diff (default; run changes or two points), list restore IDs, \
         recovery_report (parent-only). Inspect before rewind. Recovery needs cohort, prepared and \
         explicit resource_paths; retains pending evidence, never replays or settles."
    }
    fn is_concurrency_safe_for(&self, args: &serde_json::Value) -> bool {
        Self::action_of(args) != Some("recovery_report")
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["diff", "list", "recovery_report"],
                    "description": "diff=changes (default); list=timeline; recovery_report=inspection only"
                },
                "from": {
                    "type": "string",
                    "description": "diff: id or working=live tree; default run pre_edit, else newest checkpoint"
                },
                "to": {
                    "type": "string",
                    "description": "diff: id or working=live tree (default)"
                },
                "patch": {
                    "type": "boolean",
                    "description": "diff: unified line diff, not only file stats (default false)"
                },
                "paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "diff: repo-relative paths; strongly advised with patch=true"
                },
                "cohort": {"type":"string","description":"recovery_report: exact 32-hex cohort ID"},
                "prepared": {"type":"string","description":"recovery_report: exact immutable prepared-record ID"},
                "resource_paths": {
                    "type":"array","items":{"type":"string"},"maxItems":256,
                    "description":"recovery_report: exact paths required, even empty; never inferred"
                }
            },
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        if Self::action_of(args) != Some("recovery_report")
            && ["cohort", "prepared", "resource_paths"]
                .iter()
                .any(|key| args.get(key).is_some())
        {
            bail!("restart report fields require action=recovery_report");
        }
        match Self::action_of(args) {
            Some("list") => self.list(),
            Some("diff") => self.diff_report(args),
            Some("recovery_report") => {
                if ["from", "to", "patch", "paths"]
                    .iter()
                    .any(|key| args.get(key).is_some())
                {
                    bail!("recovery_report does not accept diff fields; select exact files with resource_paths");
                }
                let cohort = args
                    .get("cohort")
                    .and_then(serde_json::Value::as_str)
                    .context("recovery_report requires cohort")?;
                let prepared = args
                    .get("prepared")
                    .and_then(serde_json::Value::as_str)
                    .context("recovery_report requires prepared")?;
                let paths = args
                    .get("resource_paths")
                    .and_then(serde_json::Value::as_array)
                    .context("recovery_report requires an explicit resource_paths array")?;
                if paths.len() > crate::core::speculation::store::MAX_COHORT_FILES {
                    bail!("restart report resource selection exceeds cohort capacity");
                }
                let paths = paths
                    .iter()
                    .map(|path| {
                        path.as_str().map(str::to_owned).context(
                            "recovery_report resource_paths must contain only exact file strings",
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(inspect_speculation_recovery(&self.root, cohort, prepared, &paths)?.to_text())
            }
            _ => {
                let raw = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
                Ok(format!(
                    "error: `action` must be \"diff\", \"list\" or \"recovery_report\" (got {raw:?}). To save, rewind, \
                     or restore, use the `checkpoint` tool."
                ))
            }
        }
    }
}

impl CheckpointView {
    /// The checkpoint timeline. Gives the model the ids `checkpoint` action=restore needs when the
    /// user asks for a state from an earlier turn (run-scoped rewind has no anchor there).
    fn list(&self) -> Result<String> {
        let ctx = RepoContext::discover(&self.root)?;
        let inspection = {
            let _store = ctx.store_shared()?;
            let _transaction = ctx.lock()?;
            match require_settled_speculation(&ctx.namespace_dir) {
                Ok(summary) if summary.cohorts > 0 => format!(
                    "Speculation evidence: {} cohort(s), {} prepared integration(s), {} accepted byte/command verification(s). \
                     Retained peer-read concerns requiring semantic review: {} (not semantic acknowledgement/reconciliation). \
                     Retained immutable history only; current source has not been re-verified.\n\n",
                    summary.cohorts, summary.prepared, summary.verified, summary.read_dependency_concerns,
                ),
                Ok(_) => String::new(),
                Err(error) => format!("Speculation recovery guard: {error:#}\n\n"),
            }
        };
        let (snaps, cursor) = timeline_in(&ctx)?;
        if snaps.is_empty() {
            return Ok(format!("{inspection}no checkpoints yet — the runtime auto-checkpoints before/after edits, or use `checkpoint` to pin one."));
        }
        // Cap the tail so a long timeline can't blow the tool-result budget; ids are contiguous so
        // the model can still ask for older ones by number if it needs them.
        const MAX_ROWS: usize = 40;
        let cursor_id = cursor.and_then(|i| snaps.get(i)).map(|s| s.id);
        let start = snaps.len().saturating_sub(MAX_ROWS);
        let mut out = inspection;
        if start > 0 {
            out.push_str(&format!("… {} older checkpoint(s) not shown\n", start));
        }
        for snap in &snaps[start..] {
            out.push_str(&format_timeline_row(snap, Some(snap.id) == cursor_id));
            out.push('\n');
        }
        out.push_str(
            "\nGo back to one with `checkpoint action=restore id=<n>` (files only — chat is untouched, and reversible).",
        );
        Ok(out)
    }

    /// Diff between two points in the timeline. Without it, reacting to a bad edit meant discarding
    /// the whole tree: the model could not see which file went wrong, so one bad line cost every good
    /// change made beside it. With it, the normal move is read the diff → fix the one file, and a
    /// rewind becomes the fallback it should be.
    fn diff_report(&self, args: &serde_json::Value) -> Result<String> {
        let ctx = RepoContext::discover(&self.root)?;
        let recovery = crate::core::exec_ctx::current()
            .map(|c| c.recovery().clone())
            .unwrap_or_default();
        let side = |key: &str| -> Option<DiffSide> {
            args.get(key)
                .and_then(|v| v.as_str())
                .and_then(DiffSide::parse)
        };
        // "what have I changed" is the overwhelmingly common question, so both ends default toward it:
        // `to` = the live tree, `from` = this run's pre-edit anchor (falling back to the newest
        // checkpoint when there is no anchor, e.g. a fresh turn that hasn't edited yet).
        let to = side("to").unwrap_or(DiffSide::Working);
        let from = match side("from") {
            Some(s) => s,
            None => match recovery.status(&self.root).pre_edit {
                Some(id) => DiffSide::Checkpoint(id),
                None => {
                    let (snaps, _) = timeline_in(&ctx)?;
                    match snaps.last() {
                        Some(s) => DiffSide::Checkpoint(s.id),
                        None => return Ok(
                            "no checkpoints yet — nothing to diff against. The runtime auto-checkpoints before the first edit."
                                .to_string(),
                        ),
                    }
                }
            },
        };
        if from == to {
            return Ok(format!(
                "error: `from` and `to` are the same point ({}) — nothing to compare.",
                from.label()
            ));
        }
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let want_patch = args.get("patch").and_then(|v| v.as_bool()).unwrap_or(false);
        // Patch bytes are the one unbounded part of this result; cap them so a broad `patch=true`
        // degrades to a truncated read instead of blowing the tool-result budget.
        let limit = if want_patch { Some(24_000) } else { None };
        match diff_in(&ctx, &from, &to, &paths, limit) {
            Ok(report) => Ok(format_diff(&report, 60)),
            Err(e) => Ok(format!("error: {e:#}")),
        }
    }
}

/// Render a diff report as compact text for a tool result / CLI. Kept pure (no git, no I/O) so the
/// shaping is unit-testable and identical for the agent and the human CLI.
fn format_diff(report: &DiffReport, max_rows: usize) -> String {
    if report.is_empty() {
        return format!(
            "no changes between {} and {} — the trees are identical.",
            report.from, report.to
        );
    }
    let mut out = format!(
        "{} → {}: {} file(s) changed, +{} -{}\n",
        report.from,
        report.to,
        report.files.len(),
        report.total_added(),
        report.total_deleted()
    );
    let shown = report.files.len().min(max_rows);
    for f in &report.files[..shown] {
        let counts = match (f.added, f.deleted) {
            (Some(a), Some(d)) => format!("+{a} -{d}"),
            // Git reports `-` for both columns on a binary file; say so rather than printing "+0 -0",
            // which would read as "nothing changed".
            _ => "binary".to_string(),
        };
        match &f.old_path {
            Some(old) => out.push_str(&format!("  {} {old} → {}  ({counts})\n", f.status, f.path)),
            None => out.push_str(&format!("  {} {}  ({counts})\n", f.status, f.path)),
        }
    }
    if report.files.len() > shown {
        out.push_str(&format!(
            "  … {} more file(s)\n",
            report.files.len() - shown
        ));
    }
    if let Some(patch) = &report.patch {
        out.push('\n');
        out.push_str(patch);
        if report.patch_truncated {
            out.push_str(
                "\n… patch truncated. Narrow it with `paths` to see the rest of a specific file.\n",
            );
        }
    }
    out
}

#[cfg(test)]
#[path = "timemachine_speculation_tests.rs"]
mod speculation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    struct GitPipeWorkerCompletion(std::sync::mpsc::Sender<()>);

    impl Drop for GitPipeWorkerCompletion {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    fn assert_git_pipe_workers_retain_admission(observer_timeout: bool) {
        use crate::agent::task_tool::{SlotDenied, SubagentGate};
        use crate::core::exec_ctx::{with_current, ExecutionContext};
        use crate::core::workspace_txn::{with_test_lock_home, PhysicalEffectLease};
        use std::sync::{atomic::AtomicUsize, mpsc, Arc};

        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        with_test_lock_home(fixture.path(), || {
            let gate = SubagentGate::isolated(&ACTIVE, 1, fixture.path().join("slots"));
            let slot = gate
                .try_acquire_with(None, |path| {
                    crate::core::repo_lock::RepoTxnLock::acquire_exclusive(path, Duration::ZERO)
                })
                .unwrap();
            let physical = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "git pipe fixture",
                "tm-pipe-caller",
            )
            .unwrap();
            let context = ExecutionContext::default()
                .with_resource_scope("tm-pipe-caller")
                .with_attempt_slot(Arc::new(slot))
                .with_physical_effect_lease(physical);
            let context_alive = Arc::downgrade(&context.recovery().0);
            let workers = with_current(context, || {
                ["stdout", "stderr", "stdin"].map(|name| {
                    let (started, ready) = mpsc::channel();
                    let (release, wait) = mpsc::channel();
                    let (finished, done) = mpsc::channel();
                    let worker = spawn_git_pipe_worker(move || {
                        let scope = crate::core::exec_ctx::current()
                            .expect("pipe worker inherits the calling context")
                            .resource_scope();
                        started.send(scope).unwrap();
                        // Channel handshakes keep the observer deadline/error deterministic. The
                        // timeout is only a failure guard; no process, provider, or sleep is used.
                        wait.recv_timeout(Duration::from_secs(5)).unwrap();
                        let result = if name == "stdout" {
                            Ok(())
                        } else {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "fixture pipe closed",
                            ))
                        };
                        // This result is dropped AFTER with_current releases its context, even
                        // when a detached joiner cannot deliver it to the observer anymore.
                        Some((result, GitPipeWorkerCompletion(finished)))
                    });
                    assert_eq!(
                        ready.recv_timeout(Duration::from_secs(5)).unwrap(),
                        "tm-pipe-caller",
                        "{name} must run under the calling context"
                    );
                    (worker, release, done)
                })
            });
            let workers = workers.map(|(worker, release, done)| {
                if observer_timeout {
                    let (_, timed_out) = crate::core::proctree::join_drain(worker, Duration::ZERO);
                    assert!(timed_out, "blocked pipe must outlive the observer deadline");
                } else {
                    // The try_wait error path abandons all handles without joining workers.
                    drop(worker);
                }
                (release, done)
            });
            for (release, done) in workers {
                assert!(context_alive.upgrade().is_some());
                assert_eq!(ACTIVE.load(Ordering::SeqCst), 1);
                assert!(matches!(
                    gate.try_acquire_with(None, |_| panic!("live admission must reject first")),
                    Err(SlotDenied::Full)
                ));
                assert!(PhysicalEffectLease::exclusive_without_background(
                    Duration::ZERO,
                    None,
                    "competing body",
                    "tm-pipe-sibling"
                )
                .is_err());
                release.send(()).unwrap();
                done.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            assert!(context_alive.upgrade().is_none());
            assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
            let after = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "after actual pipe completion",
                "tm-pipe-sibling",
            )
            .unwrap();
            drop(after);
            gate.try_acquire_with(None, |path| {
                crate::core::repo_lock::RepoTxnLock::acquire_exclusive(path, Duration::ZERO)
            })
            .unwrap();
        });
    }

    #[test]
    fn git_pipe_workers_retain_admission_after_observer_error() {
        assert_git_pipe_workers_retain_admission(false);
    }

    #[test]
    fn git_pipe_workers_retain_admission_after_observer_timeout() {
        assert_git_pipe_workers_retain_admission(true);
    }

    /// A stdin payload LARGER than a pipe buffer, fed to a child that never reads it, must still
    /// return at the deadline instead of parking this thread forever.
    ///
    /// This is a regression test for a bug introduced by the very fix that bounded these spawns.
    /// `write_all` ran on the calling thread, ahead of the wait loop, so it blocked once the ~64 KiB
    /// pipe buffer filled — and the deadline had not begun counting, so nothing could break the tie.
    /// It mattered in production rather than in theory: `unpack-objects` is fed a whole packfile
    /// (megabytes for any real checkpoint) while `transaction.lock` is held, which is precisely the
    /// permanent-strand-holding-a-lock failure this module exists to prevent.
    #[test]
    fn a_child_that_never_reads_stdin_cannot_park_a_large_write() {
        // 1 MiB: comfortably past any platform's pipe buffer, so `write_all` MUST block partway
        // through unless the child drains it — and this child deliberately never does.
        let payload = vec![b'x'; 1024 * 1024];
        let mut cmd = if cfg!(windows) {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            let mut c = Command::new(format!(
                r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe"
            ));
            c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 40"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 40"]);
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // The deadline is passed in, not set through `AIZEN_GIT_OP_TIMEOUT_SECS`: that variable
        // is process-wide, and a two-second cap would also cut short any git call another test
        // is running at the same moment.
        let start = std::time::Instant::now();
        let res = run_git_piped_bounded_within(
            &mut cmd,
            &payload,
            "stdin-hostile child",
            Duration::from_secs(2),
        );
        let elapsed = start.elapsed();

        assert!(
            res.is_err(),
            "a child killed at its deadline must report an error, not success"
        );
        assert!(
            elapsed < Duration::from_secs(25),
            "returned only after {elapsed:?} — the write blocked outside the deadline's reach"
        );
        // The message must not claim innocence: a child killed mid-flight may already have written
        // part of its work, and telling the user otherwise talks them out of checking.
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            !msg.contains("nothing was changed"),
            "a killed child's partial work must not be described as no change: {msg}"
        );
    }

    /// Is a usable `git` reachable the same way production reaches it? The object-store tests below
    /// are pure git plumbing, so on a machine without git they are skipped rather than failed — a
    /// missing toolchain is not a regression in this module.
    fn git_available() -> bool {
        git_cmd()
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Scratch dir for one test, following this crate's convention (no `tempfile` dependency — the
    /// shipped binary stays dependency-thin). Wiped first so a killed run cannot poison the next.
    ///
    /// Canonicalized on creation: native resource admission refuses an alias whose canonical parent
    /// differs from its supplied spelling, and a CI temp root carrying an 8.3 short component
    /// (`C:\Users\RUNNER~1\AppData\Local\Temp`) is exactly such an alias. Returning the canonical
    /// path keeps the fixture honest instead of weakening that check. The Windows verbatim prefix is
    /// dropped afterwards so git and the other fixtures see an ordinary drive path.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aizen-tm-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("creating scratch dir");
        let canonical = fs::canonicalize(&dir).expect("canonical scratch dir");
        strip_windows_verbatim(&canonical)
    }

    /// Native admission requires one canonical spelling. A raw `temp_dir()` on a host whose profile
    /// has an 8.3 short name (`RUNNER~1`) is an alias, and every fixture path derived from it is then
    /// refused — this pins the fixture invariant, not a relaxation of the guard.
    #[test]
    fn scratch_fixtures_are_canonical_for_native_admission() {
        let fixture = scratch("canonical-fixture");
        let home = fixture.join("home");
        fs::create_dir_all(&home).unwrap();
        assert_eq!(
            strip_windows_verbatim(&fs::canonicalize(&home).unwrap()),
            home,
            "fixture home must already be in canonical spelling"
        );
        let (planned, guard) = plan_metadata_directory(&home.join("timemachine")).unwrap();
        assert_eq!(strip_windows_verbatim(&planned), home.join("timemachine"));
        guard.verify().unwrap();
        let _ = fs::remove_dir_all(&fixture);
    }

    #[test]
    fn outer_snapshot_waits_for_same_tree_physical_workers() {
        use crate::core::exec_ctx::ExecutionContext;
        use crate::core::workspace_txn::{
            PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
        };
        let fixture = scratch("snapshot-physical-admission");
        crate::core::workspace_txn::with_test_lock_home(&fixture, || {
            // Handcrafted identity: admission test launches no git or other process.
            let identity = WorkspaceIdentity {
                repo_id: format!("snapshot-repo-{}", std::process::id()),
                worktree_id: format!("snapshot-worktree-{}", std::process::id()),
                canonical_root: fixture.clone(),
                canonical_git_dir: None,
                canonical_common_git_dir: None,
            };
            let execution_ctx = ExecutionContext::default().with_resource_scope("snapshot-parent");
            let dispatch = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "dispatch",
                "snapshot-parent",
            )
            .unwrap();
            let worker = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "live worker",
                "snapshot-parent/task/worker",
            )
            .unwrap();
            assert!(
                snapshot_admission(
                    || Ok(identity.clone()),
                    &execution_ctx,
                    true,
                    Duration::ZERO
                )
                .is_err(),
                "an inherited writer lease must not allow snapshot during a live physical body"
            );
            drop(worker);
            let snapshot = snapshot_admission(
                || Ok(identity.clone()),
                &execution_ctx,
                true,
                Duration::ZERO,
            )
            .unwrap()
            .unwrap();
            assert!(PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "sibling body",
                "snapshot-parent/task/sibling"
            )
            .is_err());
            assert!(
                PhysicalEffectLease::exclusive_without_background(
                    Duration::ZERO,
                    None,
                    "nested snapshot",
                    "snapshot-parent"
                )
                .is_err(),
                "save_locked/preimages must reuse outer admission, never acquire it again"
            );
            drop(snapshot);
            let after = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "after snapshot",
                "snapshot-parent/task/sibling",
            )
            .unwrap();
            drop(after);
            drop(dispatch);
        });
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn checkpoint_gate_off_keeps_legacy_without_identity_or_physical_admission() {
        let execution_ctx = crate::core::exec_ctx::ExecutionContext::default();
        assert!(snapshot_admission(
            || panic!("gate OFF must not discover a workspace or launch git"),
            &execution_ctx,
            false,
            Duration::ZERO,
        )
        .unwrap()
        .is_none());
    }

    /// When the harness sets `AIZEN_WORKSPACE_ROOT`, `RepoContext::current()` must use it instead
    /// of the process cwd — the fix for pre-edit checkpoints failing when the harness runs from
    /// the user's home directory instead of the project root.
    #[test]
    fn repo_context_current_prefers_aizen_workspace_root_env() {
        if !git_available() {
            eprintln!("skipping: no usable git on this machine");
            return;
        }
        // `AIZEN_WORKSPACE_ROOT` is process-global: another test that pins its own workspace must
        // not observe this override, and a concurrent test's removal must not make a later
        // `RepoContext::current()` silently discover from the harness cwd instead of its fixture.
        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = scratch("repo-context-env");
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_cmd()
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .arg("--initial-branch=main")
            .output()
            .expect("git init failed");
        git_cmd()
            .arg("-C")
            .arg(&repo)
            .arg("commit")
            .arg("--allow-empty")
            .arg("-m")
            .arg("init")
            .output()
            .expect("git commit failed");

        // Set the env var and verify current() picks it up.
        let _env = crate::core::config::EnvGuard::set([("AIZEN_WORKSPACE_ROOT", &repo)]);
        let ctx = RepoContext::current().unwrap();
        assert_eq!(ctx.root, repo.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Run git against `git_dir`, returning trimmed stdout. Panics with git's own stderr so a broken
    /// fixture reports the real reason instead of an unwrap backtrace.
    fn git_in(git_dir: &Path, args: &[&str]) -> String {
        git_in_index(git_dir, None, args)
    }

    /// As [`git_in`], with an explicit index file for the `update-index`/`write-tree` pair that
    /// builds a real tree in the fixture.
    fn git_in_index(git_dir: &Path, index: Option<&Path>, args: &[&str]) -> String {
        let mut cmd = git_cmd();
        cmd.env("GIT_DIR", git_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@localhost")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@localhost")
            .args(args);
        match index {
            Some(idx) => {
                cmd.env("GIT_INDEX_FILE", idx);
            }
            None => {
                cmd.env_remove("GIT_INDEX_FILE");
            }
        }
        let out = cmd.output().expect("spawning git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A `RepoContext` pointing at a hand-built store. Only the object-store fields matter here;
    /// the rest are placeholders, because compaction touches `objects/` and nothing else.
    fn ctx_for_store(root: &Path, store_git_dir: &Path) -> RepoContext {
        RepoContext {
            root: root.to_path_buf(),
            git_dir: root.join(".git"),
            common_git_dir: root.join(".git"),
            store_git_dir: store_git_dir.to_path_buf(),
            namespace_dir: store_git_dir.join("worktrees").join("wt-test"),
            repo_id: "repo-test".to_string(),
            worktree_id: "wt-test".to_string(),
            ref_prefix: "refs/ng/tm/wt-test".to_string(),
            hooks_dir: store_git_dir.join("no-hooks"),
            filters_checked: std::cell::Cell::new(true),
            reparse_checked: std::cell::Cell::new(true),
            skip_dirs: std::cell::RefCell::new(None),
        }
    }

    /// E5.1 (quality plan S2): a file created after a checkpoint is untracked until someone
    /// commits it; restoring that checkpoint must remove it, name it, and the checkpoint taken
    /// before the restore must bring it back.
    #[test]
    fn restore_removes_a_file_the_target_checkpoint_never_had_and_reports_it() {
        if !git_available() {
            return;
        }
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = scratch("restore-home");
        // EnvGuard, not set_var/remove_var: a panicking assertion must not leak AIZEN_HOME into the
        // rest of the process, and a restored prior value must not depend on reaching the last line.
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
        let root = scratch("restore-repo");
        let init = git_cmd()
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .expect("git init");
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        fs::write(root.join("f1.txt"), "one").unwrap();
        let ctx = RepoContext::discover(&root).expect("context");
        let first = save_in(&ctx, "first", false, None).expect("first checkpoint");
        fs::write(root.join("f2.txt"), "two").unwrap(); // never `git add`ed
        let second = save_in(&ctx, "second", false, None).expect("second checkpoint");
        let (snap, removed) = restore_in_reported(&ctx, first.id).expect("restore to first");
        assert_eq!(snap.id, first.id);
        assert_eq!(
            removed,
            vec!["f2.txt".to_string()],
            "the file the target never had is named"
        );
        assert!(!root.join("f2.txt").exists(), "…and gone");
        assert_eq!(fs::read_to_string(root.join("f1.txt")).unwrap(), "one");
        let (back, removed2) = restore_in_reported(&ctx, second.id).expect("forward again");
        assert_eq!(back.id, second.id);
        assert!(removed2.is_empty(), "{removed2:?}");
        assert_eq!(
            fs::read_to_string(root.join("f2.txt")).unwrap(),
            "two",
            "the preimage checkpoint brought it back"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    /// Only genuine loose objects count. `pack/`, `info/`, and git's `tmp_obj_*` scratch files share
    /// the `objects/` directory, and feeding any of them to `pack-objects` would abort the whole
    /// compaction on a bad OID — so the filter is the thing that has to be exact, not approximate.
    #[test]
    fn loose_object_ids_lists_objects_and_nothing_else() {
        let tmp = scratch("loose-ids");
        let store = tmp.join("store.git");
        let objects = store.join("objects");
        // One real loose object: fanout dir of 2 hex chars, 38 hex chars of name.
        let fanout = objects.join("ab");
        fs::create_dir_all(&fanout).unwrap();
        let rest = "c".repeat(38);
        fs::write(fanout.join(&rest), b"x").unwrap();
        // Everything that must NOT be picked up.
        fs::create_dir_all(objects.join("pack")).unwrap();
        fs::write(objects.join("pack").join("pack-abc.pack"), b"x").unwrap();
        fs::create_dir_all(objects.join("info")).unwrap();
        fs::write(objects.join("info").join("alternates"), b"/somewhere").unwrap();
        fs::write(fanout.join("tmp_obj_QWERTY"), b"x").unwrap();

        let ctx = ctx_for_store(&tmp, &store);
        let ids = ctx.loose_object_ids();
        assert_eq!(ids, vec![format!("ab{rest}")], "got {ids:?}");
    }

    /// The regression this compactor exists for.
    ///
    /// A checkpoint commit parents on a commit that lives only in the SOURCE repo, borrowed through
    /// alternates. Delete the source — the ordinary case of a repo being moved, renamed, or removed —
    /// and `git gc` can no longer walk history, so it packs NOTHING and the store's loose objects grow
    /// forever. Compaction must still work, because it selects by OID rather than by reachability.
    #[test]
    fn a_store_with_a_dangling_parent_still_compacts_where_gc_cannot() {
        if !git_available() {
            eprintln!("skipping: no usable git on this machine");
            return;
        }
        let tmp = scratch("dangling-parent");
        let src = tmp.join("src");
        let src_git = src.join(".git");
        fs::create_dir_all(&src).unwrap();
        git_in(&src_git, &["init", "--quiet", "--bare"]);
        // A commit in the SOURCE that the checkpoint will parent onto.
        let empty_tree = git_in(&src_git, &["mktree"]);
        let src_commit = git_in(&src_git, &["commit-tree", &empty_tree, "-m", "source"]);

        // The private store, alternating onto the source exactly as `open_store` seals it.
        let store = tmp.join("store.git");
        git_in(&store, &["init", "--quiet", "--bare"]);
        let info = store.join("objects").join("info");
        fs::create_dir_all(&info).unwrap();
        fs::write(
            info.join("alternates"),
            src_git
                .join("objects")
                .to_string_lossy()
                .replace('\\', "/")
                .as_bytes(),
        )
        .unwrap();

        // Objects the STORE owns (a changed blob and the tree holding it), then the checkpoint commit
        // whose parent is borrowed. This is the shape every real checkpoint has: changed blobs land
        // in the private store, the parent stays in the source.
        let payload = tmp.join("a.txt");
        fs::write(&payload, b"contents that only the private store holds\n").unwrap();
        let blob = git_in(
            &store,
            &["hash-object", "-w", "--", &payload.to_string_lossy()],
        );
        let index = tmp.join("fixture.idx");
        git_in_index(
            &store,
            Some(&index),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},a.txt"),
            ],
        );
        let tree = git_in_index(&store, Some(&index), &["write-tree"]);
        let commit = git_in(
            &store,
            &["commit-tree", &tree, "-p", &src_commit, "-m", "checkpoint"],
        );
        git_in(&store, &["update-ref", "refs/ng/tm/wt-test/1", &commit]);

        // The source disappears. Its objects are gone; the checkpoint's parent is now dangling.
        fs::remove_dir_all(&src).unwrap();

        let ctx = ctx_for_store(&tmp, &store);
        let before = ctx.loose_object_ids().len();
        assert!(
            before >= 3,
            "fixture should have left at least blob+tree+commit loose, found {before}"
        );

        // Document the reason this module cannot just call `git gc`: it fails outright here.
        let gc = git_cmd()
            .env("GIT_DIR", &store)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .args(["gc", "--prune=now"])
            .output()
            .expect("spawning git gc");
        assert!(
            !gc.status.success(),
            "git gc unexpectedly succeeded — if git learned to survive a dangling parent, \
             compact_objects can be replaced by it"
        );

        let report = ctx
            .compact_objects()
            .expect("compaction must survive gc's failure");
        assert_eq!(report.packed, before, "every loose object should be packed");
        assert_eq!(
            ctx.loose_object_ids().len(),
            0,
            "prune-packed should have dropped every loose copy"
        );
        let packs: Vec<_> = fs::read_dir(store.join("objects").join("pack"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(packs.len(), 1, "expected exactly one pack, got {packs:?}");
        // The objects the store OWNS must still be readable — compaction moves bytes, it never
        // decides what is garbage.
        let out = git_cmd()
            .env("GIT_DIR", &store)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .args(["cat-file", "-e", &format!("{commit}^{{tree}}")])
            .output()
            .unwrap();
        assert!(out.status.success(), "checkpoint tree became unreadable");
        // E5.4: the prune's reachability walk must survive the same dangling parent, and it
        // must keep everything the checkpoint reaches.
        let prune = ctx
            .prune_unreachable()
            .expect("prune must survive a parent that lives only in the (gone) source");
        assert_eq!(prune.pruned, 0, "everything here is reachable from the ref");
        let out = git_cmd()
            .env("GIT_DIR", &store)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .args(["cat-file", "-e", &format!("{commit}^{{tree}}")])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "prune must not touch reachable objects"
        );
    }

    /// E5.4 (quality plan S8): an object no ref reaches is removed — loose or packed — and the
    /// reachable ones come out the other side readable, in one fresh pack.
    #[test]
    fn prune_drops_unreachable_objects_and_keeps_reachable_ones() {
        if !git_available() {
            return;
        }
        let tmp = scratch("prune");
        let store = tmp.join("store.git");
        git_in(&tmp, &["init", "-q", "--bare", &store.to_string_lossy()]);
        let kept_file = tmp.join("kept.txt");
        fs::write(&kept_file, b"kept\n").unwrap();
        let blob = git_in(
            &store,
            &["hash-object", "-w", "--", &kept_file.to_string_lossy()],
        );
        let index = tmp.join("prune.idx");
        git_in_index(
            &store,
            Some(&index),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},kept.txt"),
            ],
        );
        let tree = git_in_index(&store, Some(&index), &["write-tree"]);
        let commit = git_in(&store, &["commit-tree", &tree, "-m", "checkpoint"]);
        git_in(&store, &["update-ref", "refs/ng/tm/wt-test/1", &commit]);
        // A blob nothing references: the shape retention leaves behind when it deletes a ref.
        let orphan_file = tmp.join("orphan.txt");
        fs::write(&orphan_file, b"nobody points at me\n").unwrap();
        let orphan = git_in(
            &store,
            &["hash-object", "-w", "--", &orphan_file.to_string_lossy()],
        );
        let ctx = ctx_for_store(&tmp, &store);
        // Pack everything first so the orphan sits INSIDE a pack, the harder case.
        ctx.compact_objects().expect("compaction");
        assert_eq!(ctx.loose_object_ids().len(), 0);
        let report = ctx.prune_unreachable().expect("prune");
        assert_eq!(report.owned, 4, "blob, tree, commit and the orphan");
        assert_eq!(report.pruned, 1, "only the orphan goes");
        let exists = |oid: &str| {
            git_cmd()
                .env("GIT_DIR", &store)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", null_device())
                .args(["cat-file", "-e", oid])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(!exists(&orphan), "the orphan is gone");
        assert!(
            exists(&blob) && exists(&tree) && exists(&commit),
            "the checkpoint is intact"
        );
        let packs: Vec<_> = fs::read_dir(store.join("objects").join("pack"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(
            packs.len(),
            1,
            "one fresh pack, the old one swept: {packs:?}"
        );
        // A second prune finds nothing and changes nothing.
        let again = ctx.prune_unreachable().expect("prune again");
        assert_eq!(again.pruned, 0);
    }

    /// An empty store is a no-op, not an error: `pack-objects` with no input would fail, and a save
    /// on a brand-new store must not be able to trip over housekeeping.
    #[test]
    fn compacting_an_empty_store_is_a_no_op() {
        let tmp = scratch("empty-store");
        let store = tmp.join("store.git");
        fs::create_dir_all(store.join("objects")).unwrap();
        let ctx = ctx_for_store(&tmp, &store);
        let report = ctx.compact_objects().expect("empty store must not error");
        assert_eq!(report.packed, 0);
        assert_eq!(report.before_bytes, report.after_bytes);
    }

    fn mk(id: u32, parent: Option<u32>) -> Snapshot {
        Snapshot {
            id,
            commit: format!("{id:040x}"),
            tree: format!("{:040x}", id + 100),
            label: String::new(),
            created: "now".into(),
            auto: false,
            has_chat: false,
            parent,
            worktree_id: "wt-test".into(),
            coverage: Coverage::default(),
            recovery: false,
            kind: Some(SnapshotKind::Milestone),
            integration_receipt: None,
        }
    }

    /// Build a SafetyNet snapshot (the once-per-run `before agent edits` net) for retention tests.
    fn mk_safety(id: u32, parent: Option<u32>) -> Snapshot {
        Snapshot {
            auto: true,
            label: PRE_EDIT_LABEL.into(),
            kind: Some(SnapshotKind::SafetyNet),
            ..mk(id, parent)
        }
    }

    /// A busy `transaction.lock` must produce an ACTIONABLE hint, and nothing else may.
    ///
    /// This error used to be a dead end: the user saw "the pre-edit checkpoint failed: resource is
    /// busy …transaction.lock", found no other aizen process and no lock file on disk, and had no
    /// way to learn that an OS lock lives on an open HANDLE — so it can be held by a stranded thread
    /// in the very process reporting the error, and deleting the path cannot release it.
    #[test]
    fn lock_busy_gets_a_self_diagnosing_hint_and_other_errors_do_not() {
        let busy = anyhow::Error::new(crate::core::repo_lock::LockBusy {
            path: PathBuf::from("C:/x/transaction.lock"),
            mode: crate::core::repo_lock::LockMode::Exclusive,
            timeout: Duration::from_secs(15),
        });
        let hint = checkpoint_failure_hint(&busy);
        assert!(!hint.is_empty(), "a lock-busy failure must explain itself");
        for expected in ["handle", "restart"] {
            assert!(
                hint.to_ascii_lowercase().contains(expected),
                "hint should mention {expected:?} so the user can act on it; got {hint:?}"
            );
        }

        // An unrelated failure must stay clean — a hint about locks on a filter-driver refusal
        // would send the reader chasing the wrong cause.
        let other = anyhow::anyhow!(
            "checkpoint refused: repository config defines external Git filter `filter.foo.clean`"
        );
        assert!(
            checkpoint_failure_hint(&other).is_empty(),
            "only lock contention earns the lock hint"
        );
    }

    #[test]
    fn ledger_rejects_duplicate_ids_and_regressed_counter() {
        let mut duplicate = Ledger {
            snapshots: vec![mk(1, None), mk(1, None)],
            next_id: 2,
            ..Default::default()
        };
        assert!(duplicate.normalize().is_err());
        let mut regressed = Ledger {
            snapshots: vec![mk(9, None)],
            next_id: 2,
            ..Default::default()
        };
        assert!(regressed.normalize().is_err());
    }

    #[test]
    fn retention_protects_cursor_and_explicit_ids() {
        let mut l = Ledger {
            snapshots: vec![mk(1, None), mk(2, Some(1)), mk(3, Some(2)), mk(4, Some(3))],
            next_id: 5,
            ..Default::default()
        };
        l.set_cursor(Some(1));
        let dropped = enforce_retention_plan(&mut l, 2, DEFAULT_KEEP_SAFETY, &[4]);
        assert_eq!(
            l.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(dropped.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(l.cursor_id, Some(1));
    }

    #[test]
    fn rm_takes_exactly_the_named_ids_and_nothing_else() {
        let mut l = Ledger {
            snapshots: vec![mk(1, None), mk(2, Some(1)), mk(3, Some(2)), mk(4, Some(3))],
            next_id: 5,
            ..Default::default()
        };
        l.set_cursor(Some(4));
        let dropped = remove_plan(&mut l, &[2, 3]).unwrap();
        assert_eq!(dropped.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(
            l.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(l.cursor_id, Some(4));
    }

    #[test]
    fn deletion_pins_deduplicates_ids_without_releasing_live_guards() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let ctx = ctx_for_store(fixture.path(), &fixture.path().join("store.git"));
        let (protected, guards) = ctx.deletion_pins([7, 7, 9, 9]).unwrap();
        assert!(protected.is_empty(), "duplicate ids must not self-pin");
        assert_eq!(guards.len(), 2);
        for id in [7, 9] {
            let error = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                &ctx.pin_path(id),
                Duration::ZERO,
            )
            .unwrap_err();
            assert!(error
                .downcast_ref::<crate::core::repo_lock::LockBusy>()
                .is_some());
        }
        drop(guards);
        let pin =
            crate::core::repo_lock::RepoTxnLock::acquire_shared(&ctx.pin_path(7), Duration::ZERO)
                .unwrap();
        let (protected, guards) = ctx.deletion_pins([7, 7, 9, 9]).unwrap();
        assert_eq!(
            protected,
            vec![7],
            "live pins are still refused exactly once"
        );
        assert_eq!(guards.len(), 1);
        drop(guards);
        drop(pin);
        let (protected, guards) = ctx.deletion_pins([7, 7, 9, 9]).unwrap();
        assert!(protected.is_empty());
        assert_eq!(guards.len(), 2);
    }

    #[test]
    fn rm_duplicate_ids_deletes_once_and_preserves_live_pins() {
        assert!(git_available(), "regression requires installed Git");
        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", home.as_os_str()),
            ("AIZEN_WORKSPACE_ROOT", root.as_os_str()),
        ]);
        let init = git_cmd()
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        fs::write(root.join("f.txt"), "first").unwrap();
        let ctx = RepoContext::discover(&root).unwrap();
        let first = save_in(&ctx, "first", false, None).unwrap();
        fs::write(root.join("f.txt"), "second").unwrap();
        let second = save_in(&ctx, "second", false, None).unwrap();
        let pin = ctx.pin(first.clone()).unwrap();
        assert!(remove(&[first.id, first.id])
            .unwrap_err()
            .to_string()
            .contains("pinned"));
        assert_eq!(ctx.load_ledger().unwrap().snapshots.len(), 2);
        drop(pin);
        assert_eq!(remove(&[first.id, first.id]).unwrap(), 1);
        let ledger = ctx.load_ledger().unwrap();
        assert_eq!(
            ledger.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second.id]
        );
        assert_eq!(ledger.cursor_id, Some(second.id));
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "second");
    }

    /// Both refusals must land BEFORE the ledger is touched: `rm 2 9` with 9 unknown deletes
    /// nothing, and the cursor cannot be removed however it is spelled.
    #[test]
    fn rm_refuses_the_cursor_and_unknown_ids_before_touching_anything() {
        let mut l = Ledger {
            snapshots: vec![mk(1, None), mk(2, Some(1))],
            next_id: 3,
            ..Default::default()
        };
        l.set_cursor(Some(2));
        assert!(
            remove_plan(&mut l, &[2]).is_err(),
            "the active point must survive"
        );
        assert!(
            remove_plan(&mut l, &[1, 9]).is_err(),
            "a typo must not half-delete"
        );
        assert_eq!(
            l.snapshots.len(),
            2,
            "a refused plan leaves the ledger whole"
        );
    }

    #[test]
    fn kind_inferred_for_legacy_rows_without_field() {
        // A pre-`kind` ledger row: field absent, inferred from auto+label.
        let pre_edit = Snapshot {
            auto: true,
            label: PRE_EDIT_LABEL.into(),
            kind: None,
            ..mk(1, None)
        };
        assert_eq!(pre_edit.kind(), SnapshotKind::SafetyNet);
        // The dead per-edit label from the old binary is also low value.
        let dead = Snapshot {
            auto: true,
            label: "after agent edit".into(),
            kind: None,
            ..mk(2, None)
        };
        assert_eq!(dead.kind(), SnapshotKind::SafetyNet);
        // A descriptive phase checkpoint is a milestone even when auto.
        let phase = Snapshot {
            auto: true,
            label: "phase: wire up auth".into(),
            kind: None,
            ..mk(3, None)
        };
        assert_eq!(phase.kind(), SnapshotKind::Milestone);
        // recovery wins regardless of label/auto.
        let rec = Snapshot {
            recovery: true,
            kind: None,
            ..mk(4, None)
        };
        assert_eq!(rec.kind(), SnapshotKind::Recovery);
    }

    #[test]
    fn retention_thins_safetynets_before_milestones() {
        // 8 safety-nets + 2 milestones. keep is generous (100) but keep_safety = 2 ⇒ only the newest
        // two safety-nets survive; BOTH milestones are untouched even though they are older.
        let mut snaps = Vec::new();
        for id in 1..=8 {
            snaps.push(mk_safety(id, if id == 1 { None } else { Some(id - 1) }));
        }
        snaps.push(Snapshot {
            label: "phase: important A".into(),
            ..mk(9, Some(8))
        });
        snaps.push(Snapshot {
            label: "phase: important B".into(),
            ..mk(10, Some(9))
        });
        let mut l = Ledger {
            snapshots: snaps,
            next_id: 11,
            ..Default::default()
        };
        l.set_cursor(Some(10));
        let dropped = enforce_retention_plan(&mut l, 100, 2, &[]);
        // Dropped: safety-nets 1..=6 (oldest six). Kept: newest two nets (7,8) + both milestones.
        assert_eq!(
            dropped.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            l.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![7, 8, 9, 10]
        );
    }

    #[test]
    fn retention_pass2_evicts_safetynet_before_milestone_when_over_total() {
        // 3 milestones + 3 safety-nets, all within safety floor (keep_safety=3) so Pass 1 drops none.
        // Total budget keep=4 forces Pass 2 to shed 2 — and it must take the oldest SAFETY-NETS, not
        // the milestones, so the readable timeline is preserved.
        let mut snaps = vec![
            Snapshot {
                label: "phase: m1".into(),
                ..mk(1, None)
            },
            mk_safety(2, Some(1)),
            Snapshot {
                label: "phase: m2".into(),
                ..mk(3, Some(2))
            },
            mk_safety(4, Some(3)),
            Snapshot {
                label: "phase: m3".into(),
                ..mk(5, Some(4))
            },
            mk_safety(6, Some(5)),
        ];
        // Cursor on the newest so it is protected; does not affect the point being tested.
        let _ = &mut snaps;
        let mut l = Ledger {
            snapshots: snaps,
            next_id: 7,
            ..Default::default()
        };
        l.set_cursor(Some(5));
        let dropped = enforce_retention_plan(&mut l, 4, 3, &[]);
        // Two oldest safety-nets (2,4) evicted; all three milestones + newest net (6) survive.
        assert_eq!(dropped.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(
            l.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 3, 5, 6]
        );
    }

    #[test]
    fn retention_protects_run_anchors_passed_in() {
        // Even an OLD safety-net survives if it is the current run's protected pre-edit anchor.
        let mut l = Ledger {
            snapshots: vec![
                mk_safety(1, None),
                mk_safety(2, Some(1)),
                mk_safety(3, Some(2)),
                mk_safety(4, Some(3)),
            ],
            next_id: 5,
            ..Default::default()
        };
        l.set_cursor(Some(4));
        // keep_safety=0 keeps ZERO historical safety-nets, so only PROTECTED ones survive: the cursor
        // (#4) and the explicitly-protected run anchor (#1). This is the property that matters — an old
        // safety-net that is the current run's anchor is never pruned out from under an in-run rewind.
        let dropped = enforce_retention_plan(&mut l, 100, 0, &[1]);
        assert_eq!(dropped.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(
            l.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 4]
        );
    }

    #[test]
    fn legacy_cursor_migrates_to_id() {
        let mut l = Ledger {
            snapshots: vec![mk(7, None), mk(9, Some(7))],
            cursor: Some(1),
            cursor_id: None,
            next_id: 10,
            ..Default::default()
        };
        l.normalize().unwrap();
        assert_eq!(l.cursor_id, Some(9));
    }

    #[test]
    fn redo_prefers_non_recovery_child() {
        // After restore creates recovery preimage #3 under parent #1, redo from #1 must pick the
        // real branch tip #2, not the recovery safety snapshot.
        let snaps = [
            mk(1, None),
            Snapshot {
                recovery: false,
                ..mk(2, Some(1))
            },
            Snapshot {
                recovery: true,
                label: "before time-travel".into(),
                ..mk(3, Some(1))
            },
        ];
        let child = snaps
            .iter()
            .filter(|s| s.parent == Some(1) && !s.recovery)
            .max_by_key(|s| s.id)
            .map(|s| s.id);
        assert_eq!(child, Some(2));
    }

    #[test]
    fn run_recovery_anchors_and_budget() {
        let run = RunRecovery::default();
        let root = Path::new("unused-fixture-root");
        assert_eq!(run.status(root).pre_edit, None);
        assert!(run.hint(root).is_none());
        let continuation = run.clone();
        {
            let mut state = run.0.lock().unwrap();
            state.rewinds_used = 1;
        }
        assert_eq!(continuation.status(root).rewinds_left, 1);
        let child = RunRecovery::default();
        assert_eq!(child.status(root).rewinds_used, 0);
        assert_eq!(child.status(root).rewinds_left, 2);
    }

    #[test]
    fn background_refuses_checkpoint_and_rewind_then_refreshes_pre_edit_coverage() {
        use crate::core::workspace_txn::{BackgroundProcessBusy, SidecarEffectLease, SidecarKind};
        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", home.as_os_str()),
            ("AIZEN_WORKSPACE_ROOT", root.as_os_str()),
        ]);
        assert!(git_cmd()
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .unwrap()
            .status
            .success());
        let path = root.join("f.txt");
        fs::write(&path, "historical").unwrap();
        let run = RunRecovery::default();
        let historical = save_for(&run, &root, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        let background =
            SidecarEffectLease::acquire(SidecarKind::BackgroundProcess, Duration::ZERO, None)
                .unwrap();
        fs::write(&path, "background interval").unwrap();
        let error = save_for(&run, &root, PRE_EDIT_LABEL, true, true).unwrap_err();
        assert!(
            error.downcast_ref::<BackgroundProcessBusy>().is_some(),
            "{error:#}"
        );
        let error = run.rewind(&historical, "background-recovery").unwrap_err();
        assert!(
            error.downcast_ref::<BackgroundProcessBusy>().is_some(),
            "{error:#}"
        );
        assert_eq!(run.status(&root).rewinds_used, 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "background interval");
        run.defer_background_coverage();
        assert_eq!(run.status(&root).pre_edit, Some(historical.id));
        drop(background);
        let fresh = save_for(&run, &root, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        assert_ne!(
            fresh.id, historical.id,
            "historical coverage cannot certify a new interval"
        );
        assert_eq!(run.status(&root).pre_edit, Some(fresh.id));
        assert!(historical
            .binding
            .context()
            .unwrap()
            .load_ledger()
            .unwrap()
            .snapshots
            .iter()
            .any(|s| s.id == historical.id));
    }

    #[test]
    fn delegated_recovery_handles_and_checkpoint_primitives_refuse_restore() {
        use crate::agent::tools::Tool as _;
        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", home.as_os_str()),
            ("AIZEN_WORKSPACE_ROOT", root.as_os_str()),
        ]);
        assert!(git_cmd()
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .unwrap()
            .status
            .success());
        let path = root.join("f.txt");
        fs::write(&path, "baseline").unwrap();
        let parent = crate::core::exec_ctx::ExecutionContext::default();
        let child = parent.without_workspace_restore();
        let denied = save_for(child.recovery(), &root, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        let returned_again = save_for(child.recovery(), &root, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        let lease = crate::core::workspace_txn::WorkspaceWriterLease::acquire_scoped(
            &root,
            Duration::from_secs(1),
            None,
            "restore authority fixture",
            "parent-fixture",
        )
        .unwrap();
        fs::write(&path, "retained child edit").unwrap();
        for handle in [&denied, &returned_again] {
            assert!(handle
                .restore_under_lease(&lease)
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
            assert!(child
                .recovery()
                .rewind(handle, "child")
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
            assert!(parent
                .recovery()
                .rewind(handle, "parent-fixture")
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
        }
        assert_eq!(parent.recovery().status(&root).rewinds_used, 0);
        assert!(child.recovery().hint(&root).is_none());
        // Snapshot data remains usable by a trusted parent view, not by escalating a child handle.
        let parent_anchor = parent
            .recovery()
            .anchor(&root, RewindTarget::PreEdit)
            .unwrap();
        assert!(parent_anchor.restore_authorized);
        assert!(
            !child
                .recovery()
                .anchor(&root, RewindTarget::PreEdit)
                .unwrap()
                .restore_authorized
        );
        let checkpoint = Checkpoint::new(root.clone());
        for args in [
            serde_json::json!({"action":"rewind", "target":"pre_edit"}),
            serde_json::json!({"action":"restore", "id":denied.id}),
        ] {
            assert!(checkpoint
                .prepare_call(&args, &child)
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
        }
        // A prepared child save cannot be changed into restore outside the child's TLS scope.
        let mut save = checkpoint
            .prepare_call(&serde_json::json!({"action":"save"}), &child)
            .unwrap();
        save.args = serde_json::json!({"action":"restore", "id":denied.id});
        assert!(checkpoint
            .execute_prepared(&save)
            .unwrap_err()
            .to_string()
            .contains(CHILD_RESTORE_DENIED));
        crate::core::exec_ctx::with_current(child, || {
            assert!(restore_with_report(denied.id)
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
            assert!(undo_with_report()
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
            assert!(redo_with_report()
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
            assert!(doctor_repair()
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
            assert!(parent_anchor
                .restore_under_lease(&lease)
                .unwrap_err()
                .to_string()
                .contains(CHILD_RESTORE_DENIED));
        });
        assert_eq!(fs::read_to_string(&path).unwrap(), "retained child edit");
        parent_anchor.restore_under_lease(&lease).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "baseline");
        assert!(parent.can_restore_workspace());
    }

    #[test]
    fn pending_restore_never_undoes_during_automatic_child_save_or_retention() {
        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", home.as_os_str()),
            ("AIZEN_WORKSPACE_ROOT", root.as_os_str()),
        ]);
        assert!(git_cmd()
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .unwrap()
            .status
            .success());
        let path = root.join("f.txt");
        let run = RunRecovery::default();
        fs::write(&path, "target").unwrap();
        let target = save_for(&run, &root, "target", false, true)
            .unwrap()
            .unwrap();
        fs::write(&path, "preimage").unwrap();
        let preimage = save_for(&run, &root, "preimage", false, false)
            .unwrap()
            .unwrap();
        let ctx = target.binding.context().unwrap();
        let ledger = ctx.load_ledger().unwrap();
        let mut pending = Journal::new(JournalKind::Restore, ledger.generation);
        pending.target_id = Some(target.id);
        pending.preimage_id = Some(preimage.id);
        pending.phase = JournalPhase::Applying;
        ctx.save_journal(&pending).unwrap();
        let original_journal = fs::read(ctx.journal_path()).unwrap();
        fs::write(&path, "possibly applied child edit").unwrap();
        fs::write(root.join("sibling.txt"), "completed sibling work").unwrap();
        let denied = run.without_workspace_restore();
        let error = save_for(&denied, &root, "child checkpoint", true, false).unwrap_err();
        assert!(
            error.to_string().contains("explicit parent reconciliation"),
            "{error:#}"
        );
        assert!(prune_after_save(&ctx, 1, 0, &[])
            .unwrap_err()
            .to_string()
            .contains("explicit parent reconciliation"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "possibly applied child edit"
        );
        assert_eq!(
            fs::read_to_string(root.join("sibling.txt")).unwrap(),
            "completed sibling work"
        );
        assert_eq!(fs::read(ctx.journal_path()).unwrap(), original_journal);
        assert_eq!(ctx.load_ledger().unwrap().generation, ledger.generation);
        // A finished target/preimage needs only metadata settlement, not undo authority.
        fs::remove_file(root.join("sibling.txt")).unwrap();
        for (content, expected_cursor) in [("target", target.id), ("preimage", preimage.id)] {
            fs::write(&path, content).unwrap();
            pending.expected_generation = ctx.load_ledger().unwrap().generation;
            ctx.save_journal(&pending).unwrap();
            let mut live = ctx.load_ledger().unwrap();
            ctx.recover_pending(&mut live).unwrap();
            assert_eq!(live.cursor_id, Some(expected_cursor));
            assert!(ctx.load_journal().unwrap().is_none());
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
        // Explicit parent repair remains available after exclusion in this owned fixture.
        fs::write(&path, "interrupted content").unwrap();
        pending.expected_generation = ctx.load_ledger().unwrap().generation;
        ctx.save_journal(&pending).unwrap();
        doctor_repair().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "preimage");
        assert!(ctx.load_journal().unwrap().is_none());
    }

    #[test]
    fn run_recovery_bound_roots_pins_and_independent_controller_runs() {
        assert!(git_available(), "regression requires installed Git");
        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        for root in [&first, &second] {
            fs::create_dir(root).unwrap();
            let init = git_cmd()
                .current_dir(root)
                .args(["init", "-q"])
                .output()
                .unwrap();
            assert!(
                init.status.success(),
                "{}",
                String::from_utf8_lossy(&init.stderr)
            );
            fs::write(
                root.join("f.txt"),
                root.file_name().unwrap().to_string_lossy().as_bytes(),
            )
            .unwrap();
        }
        // Ambient discovery points at the OTHER repository throughout the bound operations.
        let _ambient =
            crate::core::config::EnvGuard::set([("AIZEN_WORKSPACE_ROOT", second.as_os_str())]);
        let parent = crate::core::exec_ctx::ExecutionContext::default();
        let child = parent.fork_recovery();
        let sibling = parent.fork_recovery();
        let a = save_for(parent.recovery(), &first, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        let b = save_for(child.recovery(), &second, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            a.id, b.id,
            "overlapping ids are independent namespace identities"
        );
        assert!(sibling
            .recovery()
            .anchor(&first, RewindTarget::PreEdit)
            .is_err());
        let c = save_for(sibling.recovery(), &first, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            c.commit, a.commit,
            "same repository snapshot may be pinned by independent runs"
        );
        fs::write(first.join("f.txt"), "changed first").unwrap();
        fs::write(second.join("f.txt"), "changed second").unwrap();
        let changed = save_for(parent.recovery(), &first, "phase", true, false)
            .unwrap()
            .unwrap();
        let repeated = save_for(parent.recovery(), &first, PRE_EDIT_LABEL, true, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            repeated.commit, a.commit,
            "pre-edit is sticky across continuation/dedup"
        );
        let continuation = parent.with_resource_scope("continued");
        continuation.recovery().rewind(&a, "test-parent").unwrap();
        assert_eq!(fs::read_to_string(first.join("f.txt")).unwrap(), "first");
        assert_eq!(
            fs::read_to_string(second.join("f.txt")).unwrap(),
            "changed second"
        );
        assert_eq!(parent.recovery().status(&first).rewinds_used, 1);
        assert_eq!(child.recovery().status(&second).rewinds_used, 0);
        child.recovery().rewind(&b, "test-child").unwrap();
        assert_eq!(fs::read_to_string(second.join("f.txt")).unwrap(), "second");
        fs::write(first.join("f.txt"), "sibling change").unwrap();
        sibling.recovery().rewind(&c, "test-sibling").unwrap();
        assert_eq!(fs::read_to_string(first.join("f.txt")).unwrap(), "first");
        assert_eq!(parent.recovery().status(&first).rewinds_used, 1);
        assert_eq!(child.recovery().status(&second).rewinds_used, 1);
        assert_eq!(sibling.recovery().status(&first).rewinds_used, 1);
        let wrong_lease = crate::core::workspace_txn::WorkspaceWriterLease::acquire_scoped(
            &second,
            Duration::from_millis(200),
            None,
            "wrong lease",
            "test-wrong",
        )
        .unwrap();
        assert!(a
            .restore_under_lease(&wrong_lease)
            .unwrap_err()
            .to_string()
            .contains("lease"));
        drop(wrong_lease);
        let ctx = a.binding.context().unwrap();
        prune_after_save(&ctx, 1, 0, &[]).unwrap();
        let live = ctx.load_ledger().unwrap();
        assert!(live.snapshots.iter().any(|s| s.id == a.id));
        assert!(live.snapshots.iter().any(|s| s.id == changed.id));
        let (protected, _guards) = ctx.deletion_pins([a.id, changed.id]).unwrap();
        assert_eq!(protected.len(), 2);
        assert!(
            store_deletion_pins(ctx.store_git_dir.parent().unwrap())
                .unwrap()
                .0
        );
        // Public destructive wrappers fail closed rather than deleting active child anchors.
        assert!(clear().unwrap_err().to_string().contains("pinned"));
        assert!(remove(&[b.id]).unwrap_err().to_string().contains("pinned"));
        prune(1).unwrap();
        assert!(b
            .binding
            .context()
            .unwrap()
            .load_ledger()
            .unwrap()
            .snapshots
            .iter()
            .any(|s| s.id == b.id));
        // A failed binding validation consumes a reservation conservatively, but releases in-flight.
        let mut bad = a.clone();
        bad.binding.git_dir = fixture.path().join("not-the-git-dir");
        assert!(parent.recovery().rewind(&bad, "test-parent").is_err());
        assert_eq!(
            parent.recovery().status(&first).rewinds_used,
            1,
            "foreign handles are refused before reservation"
        );
        parent.recovery().rewind(&a, "test-parent").unwrap();
        assert!(continuation.recovery().rewind(&a, "test-parent").is_err());
        assert_eq!(continuation.recovery().status(&first).rewinds_left, 0);
        assert_eq!(child.recovery().status(&second).rewinds_left, 1);
    }

    #[test]
    fn diff_side_parses_ids_and_working_aliases() {
        assert_eq!(DiffSide::parse("5"), Some(DiffSide::Checkpoint(5)));
        // The timeline prints ids as `#5`, so pasting one back must work.
        assert_eq!(DiffSide::parse("#12"), Some(DiffSide::Checkpoint(12)));
        for alias in [
            "working", "worktree", "wt", "now", "current", "disk", "WORKING",
        ] {
            assert_eq!(
                DiffSide::parse(alias),
                Some(DiffSide::Working),
                "alias {alias}"
            );
        }
        assert_eq!(DiffSide::parse("nonsense"), None);
        // `0` is not a valid checkpoint id (the ledger starts at 1) but parses as one; the ledger
        // lookup is what rejects it, with a message naming `aizen time list`.
        assert_eq!(DiffSide::parse("0"), Some(DiffSide::Checkpoint(0)));
    }

    #[test]
    fn merge_diff_streams_pairs_counts_with_status() {
        // Byte-for-byte what `git diff-tree -z --name-status` / `--numstat` emit for plain edits:
        // status and path are separate NUL fields; numstat keeps the path in the tab-split field.
        let name = b"M\0Cargo.lock\0M\0Cargo.toml\0";
        let num = b"1\t1\tCargo.lock\x002\t3\tCargo.toml\0";
        let files = merge_diff_streams(name, num);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "Cargo.lock");
        assert_eq!(
            (files[0].status, files[0].added, files[0].deleted),
            ('M', Some(1), Some(1))
        );
        assert_eq!(
            (files[1].status, files[1].added, files[1].deleted),
            ('M', Some(2), Some(3))
        );
        assert!(files.iter().all(|f| f.old_path.is_none()));
    }

    #[test]
    fn merge_diff_streams_handles_renames_and_binary() {
        // Rename: name-status is `R080\0old\0new`, numstat leaves the inline path EMPTY and follows
        // with old\0new as their own fields — the two streams disagree in shape, which is the whole
        // reason this merge exists. Counts must key on the NEW path.
        let name = b"R080\0old.txt\0new.txt\0";
        let num = b"1\t1\t\0old.txt\0new.txt\0";
        let files = merge_diff_streams(name, num);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, 'R');
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!((files[0].added, files[0].deleted), (Some(1), Some(1)));

        // Binary: git reports `-` for both counts. `None` must survive as "not text", not as 0.
        let files = merge_diff_streams(b"M\0logo.png\0", b"-\t-\tlogo.png\0");
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].added, files[0].deleted), (None, None));
    }

    #[test]
    fn merge_diff_streams_survives_truncated_and_empty_input() {
        assert!(merge_diff_streams(b"", b"").is_empty());
        // A status with no following path must not panic or invent an entry.
        assert!(merge_diff_streams(b"M\0", b"").is_empty());
        // Missing numstat side → counts unknown, but the file is still reported.
        let files = merge_diff_streams(b"A\0new.rs\0", b"");
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].status, files[0].added), ('A', None));
    }

    #[test]
    fn diff_report_totals_ignore_binary_files() {
        let report = DiffReport {
            from: "#1".into(),
            to: "working tree".into(),
            files: vec![
                FileChange {
                    status: 'M',
                    path: "a.rs".into(),
                    old_path: None,
                    added: Some(3),
                    deleted: Some(1),
                },
                FileChange {
                    status: 'M',
                    path: "b.png".into(),
                    old_path: None,
                    added: None,
                    deleted: None,
                },
            ],
            patch: None,
            patch_truncated: false,
        };
        assert_eq!(report.total_added(), 3);
        assert_eq!(report.total_deleted(), 1);
        assert!(!report.is_empty());
    }

    #[test]
    fn rewind_target_parse() {
        assert_eq!(RewindTarget::parse("pre_edit"), Some(RewindTarget::PreEdit));
        assert_eq!(
            RewindTarget::parse("last-good"),
            Some(RewindTarget::LastGood)
        );
        assert_eq!(RewindTarget::parse("undo"), Some(RewindTarget::LastGood));
        assert_eq!(RewindTarget::parse("42"), None);
    }

    #[test]
    fn speculation_metadata_planning_is_read_only_and_does_not_create_suffix() {
        let fixture = scratch("speculation-plan-read-only");
        let home = fixture.join("home");
        fs::create_dir_all(&home).unwrap();
        let target = home.join("timemachine");
        let (planned, guard) = plan_metadata_directory(&target).unwrap();
        // The missing suffix cannot itself be canonicalized. Its existing parent is the identity
        // anchor; on Windows canonicalization deliberately preserves the verbatim \\?\ prefix.
        assert_eq!(
            planned,
            fs::canonicalize(&home).unwrap().join("timemachine")
        );
        assert_eq!(
            guard.path().parent(),
            Some(fs::canonicalize(&home).unwrap().as_path())
        );
        assert!(
            !target.exists(),
            "planning must not create the metadata suffix"
        );
        guard.verify().unwrap();
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn speculation_registry_validation_keeps_registry_ids_separate_from_workspace_ids() {
        let registry = identity::Registry {
            schema_version: 1,
            repos: vec![identity::RepoEntry {
                repo_id: "repo-0123456789abcdef".into(),
                root_commit: None,
                common_git_dir: if cfg!(windows) {
                    r"C:\repo\.git".into()
                } else {
                    "/repo/.git".into()
                },
                worktrees: vec![identity::WorktreeEntry {
                    worktree_id: "wt-fedcba9876543210".into(),
                    git_dir: if cfg!(windows) {
                        r"C:\repo\.git".into()
                    } else {
                        "/repo/.git".into()
                    },
                }],
            }],
        };
        validate_speculation_registry(&registry).unwrap();
        let workspace = crate::core::workspace_txn::WorkspaceIdentity {
            repo_id: "workspace-repo-not-registry-id".into(),
            worktree_id: "workspace-worktree-not-registry-id".into(),
            canonical_root: PathBuf::from(if cfg!(windows) { r"C:\repo" } else { "/repo" }),
            canonical_git_dir: None,
            canonical_common_git_dir: None,
        };
        assert_ne!(workspace.repo_id, registry.repos[0].repo_id);
        assert_ne!(
            workspace.worktree_id,
            registry.repos[0].worktrees[0].worktree_id
        );
    }

    #[test]
    fn snapshot_receipt_link_is_optional_backward_compatible_and_fail_closed() {
        let legacy = serde_json::json!({
            "id": 1,
            "commit": "0000000000000000000000000000000000000001",
            "tree": "0000000000000000000000000000000000000002",
            "label": "legacy",
            "created": "now",
            "auto": false,
            "parent": null,
            "worktree_id": "wt-test",
            "recovery": false
        });
        let parsed: Snapshot = serde_json::from_value(legacy).unwrap();
        assert!(parsed.integration_receipt.is_none());
        let valid = IntegrationReceiptLink {
            schema_version: 1,
            integration_id: "integration-1".into(),
            receipt_sha256: "a".repeat(64),
        };
        valid.validate().unwrap();
        assert!(IntegrationReceiptLink {
            schema_version: 2,
            ..valid.clone()
        }
        .validate()
        .is_err());
        assert!(IntegrationReceiptLink {
            integration_id: "../escape".into(),
            ..valid.clone()
        }
        .validate()
        .is_err());
        assert!(IntegrationReceiptLink {
            receipt_sha256: "not-a-hash".into(),
            ..valid
        }
        .validate()
        .is_err());
    }

    #[test]
    fn future_ledger_schema_remains_rejected_after_linkage_migration() {
        let mut ledger = Ledger {
            schema_version: LEDGER_SCHEMA + 1,
            ..Default::default()
        };
        assert!(ledger.normalize().is_err());
    }

    #[test]
    fn admitted_binding_uses_actual_transaction_namespace_not_workspace_lock_name() {
        let fixture = scratch("speculation-namespace-shape");
        let root = fixture
            .join("tm")
            .join("repo-0123456789abcdef")
            .join("worktrees")
            .join("wt-fedcba9876543210");
        let binding = RepoBinding {
            root: fixture.join("repo"),
            git_dir: fixture.join("repo").join(".git"),
            common_git_dir: fixture.join("repo").join(".git"),
            store_git_dir: fixture
                .join("tm")
                .join("repo-0123456789abcdef")
                .join("store.git"),
            namespace_dir: root.clone(),
            repo_id: "repo-0123456789abcdef".into(),
            worktree_id: "wt-fedcba9876543210".into(),
            ref_prefix: "refs/ng/tm/wt-fedcba9876543210".into(),
            hooks_dir: root.join("empty-hooks"),
        };
        let ctx = RepoContext {
            root: binding.root.clone(),
            git_dir: binding.git_dir.clone(),
            common_git_dir: binding.common_git_dir.clone(),
            store_git_dir: binding.store_git_dir.clone(),
            namespace_dir: binding.namespace_dir.clone(),
            repo_id: binding.repo_id.clone(),
            worktree_id: binding.worktree_id.clone(),
            ref_prefix: binding.ref_prefix.clone(),
            hooks_dir: binding.hooks_dir.clone(),
            filters_checked: std::cell::Cell::new(true),
            reparse_checked: std::cell::Cell::new(true),
            skip_dirs: std::cell::RefCell::new(None),
        };
        assert_eq!(ctx.lock_path(), root.join("transaction.lock"));
        assert_eq!(
            ctx.store_lock_path(),
            binding.store_git_dir.parent().unwrap().join("store.lock")
        );
        assert_ne!(
            ctx.lock_path(),
            fixture.join("locks").join("timemachine.lock")
        );
        fs::remove_dir_all(fixture).unwrap();
    }

    pub(super) fn speculation_git_fixture(
        tag: &str,
    ) -> Option<(
        crate::core::file_placement::tests::TestDir,
        crate::core::workspace_txn::WorkspaceIdentity,
        PathBuf,
    )> {
        if !git_available() {
            return None;
        }
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let root = fixture.path().join(tag);
        fs::create_dir(&root).unwrap();
        raw_git(&root, ["init", "--quiet"]).unwrap();
        fs::write(root.join("live.txt"), b"unchanged live bytes").unwrap();
        let identity = crate::core::workspace_txn::WorkspaceIdentity::discover(&root).unwrap();
        let home = fixture.path().join("private-home");
        Some((fixture, identity, home))
    }

    #[test]
    fn speculation_binding_admission_is_metadata_only_and_locks_actual_transaction() {
        use crate::core::repo_lock::{LockBusy, RepoTxnLock};
        use crate::core::workspace_txn::{PhysicalEffectLease, WorkspaceWriterLease};
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some((fixture, identity, home)) = speculation_git_fixture("binding-admission") else {
            return;
        };
        let plan = PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
        assert!(
            !home.exists(),
            "planning cannot create even the home directory"
        );
        crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "binding fixture",
                "binding-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "binding metadata",
                "binding-parent",
            )
            .unwrap();
            let binding = plan.admit(&writer, &physical).unwrap();
            assert!(binding.namespace_dir().is_dir());
            assert_ne!(binding.repo_id(), identity.repo_id);
            assert_ne!(binding.worktree_id(), identity.worktree_id);
            assert!(
                !binding.binding.store_git_dir.exists(),
                "admission must not initialize Git storage"
            );
            assert!(!binding.namespace_dir().join("ledger.json").exists());
            assert!(!binding.namespace_dir().join("journal.json").exists());
            let ctx = binding.binding.context().unwrap();
            binding
                .with_transaction_lock(&writer, &physical, || {
                    let transaction =
                        RepoTxnLock::acquire_exclusive(&ctx.lock_path(), Duration::ZERO)
                            .unwrap_err();
                    assert!(transaction.downcast_ref::<LockBusy>().is_some());
                    let store =
                        RepoTxnLock::acquire_exclusive(&ctx.store_lock_path(), Duration::ZERO)
                            .unwrap_err();
                    assert!(store.downcast_ref::<LockBusy>().is_some());
                    Ok(())
                })
                .unwrap();
            RepoTxnLock::acquire_exclusive(&ctx.lock_path(), Duration::ZERO).unwrap();
            assert_eq!(
                fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                b"unchanged live bytes"
            );
            assert!(
                !ctx.journal_path().exists(),
                "adapter must not call recovery or normal save"
            );
            assert!(!ctx.ledger_path().exists());
        });
    }

    #[test]
    fn speculation_binding_refuses_wrong_writer_and_replaced_git_ancestor_before_creation() {
        use crate::core::workspace_txn::{PhysicalEffectLease, WorkspaceWriterLease};
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some((fixture, identity, home)) = speculation_git_fixture("binding-replacement") else {
            return;
        };
        let plan = PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
        crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
            let mut wrong = identity.clone();
            wrong.worktree_id.push_str("-wrong");
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                wrong,
                Duration::ZERO,
                None,
                "wrong binding fixture",
                "wrong-binding-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "wrong binding metadata",
                "wrong-binding-parent",
            )
            .unwrap();
            assert!(plan.admit(&writer, &physical).is_err());
            assert!(!home.exists());
            drop(physical);
            drop(writer);
            let git = identity.canonical_git_dir.as_ref().unwrap();
            fs::rename(git, identity.canonical_root.join("saved-git")).unwrap();
            fs::create_dir(git).unwrap();
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "stale binding fixture",
                "stale-binding-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "stale binding metadata",
                "stale-binding-parent",
            )
            .unwrap();
            assert!(plan.admit(&writer, &physical).is_err());
            assert!(
                !home.exists(),
                "stale identity must refuse before metadata creation"
            );
            assert_eq!(
                fs::read(identity.canonical_root.join("live.txt")).unwrap(),
                b"unchanged live bytes"
            );
        });
    }

    #[test]
    fn speculation_registry_corrupt_future_and_changed_mappings_fail_closed() {
        use crate::core::workspace_txn::{PhysicalEffectLease, WorkspaceWriterLease};
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some((fixture, identity, home)) = speculation_git_fixture("binding-registry") else {
            return;
        };
        let root = home.join("timemachine");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("registry.json");
        for bytes in [
            b"not-json".as_slice(),
            br#"{"schema_version":2,"repos":[]}"#.as_slice(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(PlannedTimeMachineBinding::plan(identity.clone(), &home).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(
                fs::read_dir(&root).unwrap().count(),
                1,
                "read-only planning creates no lock file"
            );
        }
        let mut registry = identity::Registry {
            schema_version: 1,
            repos: vec![identity::RepoEntry {
                repo_id: "repo-0123456789abcdef".into(),
                root_commit: None,
                common_git_dir: identity
                    .canonical_common_git_dir
                    .as_ref()
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                worktrees: vec![identity::WorktreeEntry {
                    worktree_id: "wt-0123456789abcdef".into(),
                    git_dir: identity
                        .canonical_git_dir
                        .as_ref()
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                }],
            }],
        };
        fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
        let plan = PlannedTimeMachineBinding::plan(identity.clone(), &home).unwrap();
        registry.repos[0].worktrees[0].worktree_id = "wt-fedcba9876543210".into();
        let changed = serde_json::to_vec(&registry).unwrap();
        fs::write(&path, &changed).unwrap();
        crate::core::workspace_txn::with_test_lock_home(fixture.path(), || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                identity.clone(),
                Duration::ZERO,
                None,
                "registry fixture",
                "registry-binding-parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive_without_background(
                Duration::ZERO,
                None,
                "registry metadata",
                "registry-binding-parent",
            )
            .unwrap();
            assert!(plan.admit(&writer, &physical).is_err());
            assert_eq!(fs::read(&path).unwrap(), changed);
            assert!(!root.join("repo-0123456789abcdef").exists());
        });
        registry.repos[0].repo_id = "../escape".into();
        assert!(validate_speculation_registry(&registry).is_err());
    }

    #[test]
    fn speculation_non_git_planning_has_no_binding_or_metadata() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let root = fs::canonicalize(fixture.path()).unwrap();
        assert!(plan_speculation_binding(&root).unwrap().is_none());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn private_store_path_is_under_aizen_home() {
        // Identity hashing is pure; the store root must never be inside the source .git.
        let common = PathBuf::from(r"C:\work\proj\.git");
        let repo_id = format!("repo-{:016x}", fnv1a64(&common.to_string_lossy()));
        let home = PathBuf::from(r"C:\Users\me\.aizen");
        let store = home.join("timemachine").join(&repo_id).join("store.git");
        assert!(store.starts_with(&home));
        assert!(!store.components().any(|c| c.as_os_str() == ".git"));
        assert!(repo_id.starts_with("repo-"));
    }

    #[test]
    fn journal_roundtrip_shape() {
        let j = Journal::new(JournalKind::Save, 7);
        assert_eq!(j.expected_generation, 7);
        assert!(matches!(j.phase, JournalPhase::Prepared));
        let bytes = serde_json::to_vec(&j).unwrap();
        let back: Journal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.operation_id, j.operation_id);
        assert!(matches!(back.kind, JournalKind::Save));
    }

    // ── identity::resolve — pure, no git/disk. Each move/clone/legacy branch is exercised here. ──
    /// The three spellings one directory gets — registry (`//?/C:/…`), alternates (`C:/…`), legacy
    /// verbatim (`\\?\C:\…`) — must compare equal, or the superseded match never fires on Windows.
    #[test]
    fn dir_key_folds_every_writer_spelling_of_one_directory() {
        assert_eq!(
            dir_key("//?/C:/Users/a/p/.git"),
            dir_key(r"C:\Users\a\p\.git")
        );
        assert_eq!(
            dir_key(r"\\?\C:\Users\a\p\.git"),
            dir_key("C:/Users/a/p/.git/")
        );
        #[cfg(windows)]
        assert_eq!(dir_key("C:/USERS/a/p/.git"), dir_key("c:/users/a/p/.git"));
        #[cfg(not(windows))]
        assert_ne!(dir_key("/home/A/p/.git"), dir_key("/home/a/p/.git"));
    }

    fn reg_of(entries: &[(&str, &str)]) -> identity::Registry {
        identity::Registry {
            schema_version: 1,
            repos: entries
                .iter()
                .map(|(id, common)| identity::RepoEntry {
                    repo_id: id.to_string(),
                    root_commit: None,
                    common_git_dir: common.to_string(),
                    worktrees: Vec::new(),
                })
                .collect(),
        }
    }

    /// The id-migration leftover: an unregistered store whose live source repo is registered under
    /// a DIFFERENT id is superseded — and the registered store itself must never be flagged.
    #[test]
    fn a_superseded_store_is_flagged_and_the_registered_one_never_is() {
        let reg = reg_of(&[("repo-new", "//?/c:/users/a/proj/.git")]);
        let (registered, by) = classify_store(
            "repo-old",
            Some("c:/users/a/proj/.git/objects"),
            true,
            Some(&reg),
        );
        assert!(!registered);
        assert_eq!(by.as_deref(), Some("repo-new"));
        let (registered, by) = classify_store(
            "repo-new",
            Some("c:/users/a/proj/.git/objects"),
            true,
            Some(&reg),
        );
        assert!(registered);
        assert_eq!(by, None);
    }

    /// A dormant legacy store — unregistered, but no registered sibling owns its repo — is NOT
    /// superseded: the next `aizen` run in that repo grandfathers it back in, so reaping it would
    /// destroy a timeline that is merely asleep.
    #[test]
    fn a_dormant_legacy_store_is_left_alone() {
        let reg = reg_of(&[("repo-other", "//?/c:/users/a/elsewhere/.git")]);
        let (registered, by) = classify_store(
            "repo-old",
            Some("c:/users/a/proj/.git/objects"),
            true,
            Some(&reg),
        );
        assert!(!registered);
        assert_eq!(by, None);
    }

    /// A broken registry disables the superseded class outright — fail-open, like
    /// `resolve_identity`. And a dead source never matches through the registry either: such a
    /// store is the first orphan class already, and matching a vanished path proves nothing.
    #[test]
    fn a_missing_registry_never_manufactures_superseded_stores() {
        let (registered, by) =
            classify_store("repo-old", Some("c:/users/a/proj/.git/objects"), true, None);
        assert!(registered);
        assert_eq!(by, None);
        let reg = reg_of(&[("repo-new", "//?/c:/users/a/proj/.git")]);
        let (_, by) = classify_store(
            "repo-old",
            Some("c:/users/a/proj/.git/objects"),
            false,
            Some(&reg),
        );
        assert_eq!(by, None);
    }

    mod identity_resolve {
        use super::super::identity::*;

        /// Deterministic id minter: `repo`/`wt` prefix + a monotonic counter, so assertions on the
        /// exact minted ids are stable across runs.
        fn minter(counter: &std::cell::Cell<u32>) -> impl Fn(&str) -> String + '_ {
            move |prefix: &str| {
                counter.set(counter.get() + 1);
                format!("{prefix}-{}", counter.get())
            }
        }

        /// A path_exists probe backed by a fixed set of "present" paths.
        fn presence(set: &std::collections::HashSet<String>) -> impl Fn(&str) -> bool + '_ {
            move |p: &str| set.contains(p)
        }

        #[allow(clippy::too_many_arguments)]
        fn input<'a>(
            common: &str,
            git: &str,
            root: Option<&str>,
            legacy_store_exists: bool,
            path_exists: &'a dyn Fn(&str) -> bool,
            new_id: &'a dyn Fn(&str) -> String,
        ) -> ResolveInput<'a> {
            ResolveInput {
                common_git_dir: common.to_string(),
                git_dir: git.to_string(),
                root_commit: root.map(|s| s.to_string()),
                legacy_repo_id: "repo-legacyaaaaaaaa".to_string(),
                legacy_wt_id: "wt-legacybbbbbbbb".to_string(),
                legacy_store_exists,
                path_exists,
                new_id,
            }
        }

        #[test]
        fn brand_new_repo_gets_random_ids_and_records_entry() {
            let mut reg = Registry::default();
            let empty = std::collections::HashSet::new();
            let present = presence(&empty);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let r = resolve(
                &mut reg,
                &input("/a/.git", "/a/.git", Some("root1"), false, &present, &mint),
            );
            assert!(r.changed);
            assert_eq!(r.repo_id, "repo-1");
            assert_eq!(r.worktree_id, "wt-2");
            assert_eq!(reg.repos.len(), 1);
            assert_eq!(reg.repos[0].root_commit.as_deref(), Some("root1"));
        }

        #[test]
        fn exact_path_hit_is_stable_and_unchanged() {
            let mut reg = Registry::default();
            let empty = std::collections::HashSet::new();
            let present = presence(&empty);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let first = resolve(
                &mut reg,
                &input("/a/.git", "/a/.git", Some("root1"), false, &present, &mint),
            );
            // Second discovery at the SAME path returns the SAME ids and reports no change.
            let again = resolve(
                &mut reg,
                &input("/a/.git", "/a/.git", Some("root1"), false, &present, &mint),
            );
            assert_eq!(first.repo_id, again.repo_id);
            assert_eq!(first.worktree_id, again.worktree_id);
            assert!(!again.changed);
            assert_eq!(reg.repos.len(), 1);
        }

        #[test]
        fn moved_repo_reconnects_by_root_commit() {
            let mut reg = Registry::default();
            let empty = std::collections::HashSet::new();
            let present = presence(&empty);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let orig = resolve(
                &mut reg,
                &input(
                    "/old/.git",
                    "/old/.git",
                    Some("root1"),
                    false,
                    &present,
                    &mint,
                ),
            );
            // Repo moved to /new; /old no longer exists (empty presence), same root commit.
            let moved = resolve(
                &mut reg,
                &input(
                    "/new/.git",
                    "/new/.git",
                    Some("root1"),
                    false,
                    &present,
                    &mint,
                ),
            );
            // Same repo id — timeline preserved — and the path was rewritten, not re-keyed.
            assert_eq!(orig.repo_id, moved.repo_id);
            assert_eq!(reg.repos.len(), 1);
            assert_eq!(reg.repos[0].common_git_dir, "/new/.git");
        }

        #[test]
        fn cp_r_copy_gets_new_id_because_original_path_still_exists() {
            let mut reg = Registry::default();
            // The ORIGINAL path exists on disk → the copy is NOT a move.
            let mut set = std::collections::HashSet::new();
            set.insert("/orig/.git".to_string());
            let present = presence(&set);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let orig = resolve(
                &mut reg,
                &input(
                    "/orig/.git",
                    "/orig/.git",
                    Some("root1"),
                    false,
                    &present,
                    &mint,
                ),
            );
            // `cp -r` to /copy: same root commit, but /orig is still present → distinct id.
            let copy = resolve(
                &mut reg,
                &input(
                    "/copy/.git",
                    "/copy/.git",
                    Some("root1"),
                    false,
                    &present,
                    &mint,
                ),
            );
            assert_ne!(orig.repo_id, copy.repo_id);
            assert_eq!(reg.repos.len(), 2);
        }

        #[test]
        fn legacy_store_on_disk_is_grandfathered_not_re_minted() {
            let mut reg = Registry::default();
            let empty = std::collections::HashSet::new();
            let present = presence(&empty);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let r = resolve(
                &mut reg,
                &input("/a/.git", "/a/.git", Some("root1"), true, &present, &mint),
            );
            // Adopts the existing hash-path store id/namespace with no data migration.
            assert_eq!(r.repo_id, "repo-legacyaaaaaaaa");
            assert_eq!(r.worktree_id, "wt-legacybbbbbbbb");
        }

        #[test]
        fn no_root_commit_cannot_reconnect_and_stays_separate() {
            let mut reg = Registry::default();
            let empty = std::collections::HashSet::new();
            let present = presence(&empty);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let a = resolve(
                &mut reg,
                &input("/old/.git", "/old/.git", None, false, &present, &mint),
            );
            // A repo with no commits (root_commit None) that "moves" cannot be recognized → new id.
            let b = resolve(
                &mut reg,
                &input("/new/.git", "/new/.git", None, false, &present, &mint),
            );
            assert_ne!(a.repo_id, b.repo_id);
            assert_eq!(reg.repos.len(), 2);
        }

        #[test]
        fn linked_worktree_shares_repo_id_new_worktree_id() {
            let mut reg = Registry::default();
            // Main worktree exists; a linked worktree has a different git_dir under the same common dir.
            let mut set = std::collections::HashSet::new();
            set.insert("/a/.git".to_string());
            let present = presence(&set);
            let counter = std::cell::Cell::new(0u32);
            let mint = minter(&counter);
            let main = resolve(
                &mut reg,
                &input("/a/.git", "/a/.git", Some("root1"), false, &present, &mint),
            );
            let linked = resolve(
                &mut reg,
                &input(
                    "/a/.git",
                    "/a/.git/worktrees/feature",
                    Some("root1"),
                    false,
                    &present,
                    &mint,
                ),
            );
            assert_eq!(main.repo_id, linked.repo_id);
            assert_ne!(main.worktree_id, linked.worktree_id);
            assert_eq!(reg.repos.len(), 1);
            assert_eq!(reg.repos[0].worktrees.len(), 2);
        }
    }
}
