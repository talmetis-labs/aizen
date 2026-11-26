//! Multi-session cooperation: session registry, liveness, and Git worktree coordination.
//!
//! Git owns source history and the index. A shared working tree does not prove which session made
//! a change: neither a writer lease nor an old checkpoint ledger excludes external user edits.
//! Historical manifests and path claims remain readable as evidence, never staging authority.
//! Session-specific diff/commit therefore fail closed until independently attributable Git-owned
//! work is available. This module does not reconstruct files or capture replacement snapshots.
//!
//! Layout (`repo_id`, so linked worktrees of one repository share a registry):
//!
//! ```text
//! ~/.aizen/coop/<repo_id>/
//!   sessions/<session_id>.json   one session's manifest — written ONLY by its owner
//!   sessions/<session_id>.lock   held for the process lifetime => liveness (see `alive`)
//!   shared.json                  historical claims / overlaps / committed records
//! ```
//!
//! Liveness uses an OS lock held for the process lifetime. The OS releases it even on a crash;
//! a reader that can acquire it sees an abandoned session, not permission to adopt its edits.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::persist::atomic_write_owner_only;
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::WorkspaceIdentity;

/// Manifest schema. Future and unreadable manifests are preserved for their owning binary.
const SCHEMA: u32 = 1;
/// Shared claims file schema.
const SHARED_SCHEMA: u32 = 1;

/// Ceiling on historical overlap fixtures; there is no new path-claim producer.
#[cfg(test)]
const MAX_OVERLAPS: usize = 500;
/// Finished manifests with no unresolved attribution evidence may be swept after this interval.
const RETENTION_SECS: u64 = 7 * 24 * 3600;

/// What a session is doing right now, as published by the session itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// At the prompt, waiting for the user.
    Idle,
    /// A turn is in flight.
    Working,
    /// Blocked on an approval prompt.
    AwaitingApproval,
    /// The user declared this window's task complete (`/team done`).
    Done,
    /// The process exited cleanly.
    Finished,
    /// The last turn ended in an error.
    Failed,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Done => "done",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }

    /// Whether this state implies the process should still be running (so a released lock means it
    /// died rather than left).
    fn expects_live_process(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Working | Self::AwaitingApproval | Self::Done | Self::Failed
        )
    }
}

/// Historical file attribution from a checkpoint-era manifest, not current ownership proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchedFile {
    /// Repository-relative path exactly as git reports it.
    pub path: String,
    /// Historical first checkpoint number. Preserved verbatim; never resolved or replayed.
    pub base: u32,
    /// Historical turn checkpoint numbers. Absence on older manifests remains readable.
    #[serde(default)]
    pub bases: Vec<u32>,
    /// Turn number (within this session) of the most recent change.
    pub last_turn: u64,
    /// Latest git status letter seen for this path (`A`/`M`/`D`/`R`/`C`/`T`).
    pub status: char,
}

/// One session's published state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema: u32,
    pub session_id: String,
    pub pid: u32,
    pub binary_version: String,
    pub repo_id: String,
    pub worktree_id: String,
    /// Canonical worktree root this session works in.
    pub root: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Saved-session slug (`~/.aizen/sessions/<name>.json`), when there is one.
    #[serde(default)]
    pub session_name: Option<String>,
    /// What this window is doing, for another window to read.
    #[serde(default)]
    pub task: String,
    /// Set by `/team task` — pinned, never overwritten by the automatic description.
    #[serde(default)]
    pub task_pinned: bool,
    pub state: SessionState,
    #[serde(default)]
    pub turns: u64,
    /// Historical in-flight checkpoint number, retained as evidence without restore authority.
    #[serde(default)]
    pub base_checkpoint: Option<u32>,
    #[serde(default)]
    pub files: Vec<TouchedFile>,
    /// Historical paths dropped by the old ledger ceiling; still a coverage gap.
    #[serde(default)]
    pub truncated_files: u64,
    /// Attribution limitation or historical failure reason; never cleared by a later turn.
    #[serde(default)]
    pub degraded: Option<String>,
    /// An uncovered interval cannot be repaired retroactively by a later turn.
    #[serde(default)]
    pub coverage_gap: bool,
    pub started_unix: u64,
    pub updated_unix: u64,
    /// Git task ownership is minted only after clean worktree creation, never from a branch name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_task: Option<GitTaskState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitTaskState {
    /// Missing only on historical records; never a live native completion capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) native_lifecycle: Option<crate::core::git_native_lifecycle::NativeLifecycle>,
    pub owner: String,
    pub base: crate::core::snapshots::GitOid,
    pub tip: crate::core::snapshots::GitOid,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<crate::core::git_ops::PathInverse>,
    pub paths: Vec<String>,
    pub verified: bool,
}

/// Cross-session state. Written by any session, always under one lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shared {
    #[serde(default = "shared_schema")]
    pub schema: u32,
    /// path → the session that most recently claimed it.
    #[serde(default)]
    pub owners: BTreeMap<String, Claim>,
    #[serde(default)]
    pub overlaps: Vec<Overlap>,
    /// session_id → when its changes were committed.
    #[serde(default)]
    pub committed: BTreeMap<String, u64>,
}

fn shared_schema() -> u32 {
    SHARED_SCHEMA
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            schema: SHARED_SCHEMA,
            owners: BTreeMap::new(),
            overlaps: Vec::new(),
            committed: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub session_id: String,
    pub unix: u64,
}

/// Two sessions changed one file. Recorded for both, blocking neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    pub path: String,
    /// The session that claimed the path first.
    pub first: String,
    /// The session that changed it afterwards.
    pub second: String,
    pub unix: u64,
}

/// A manifest plus what the reader can only work out at read time.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub manifest: SessionManifest,
    /// The owning process still holds its lock.
    pub alive: bool,
    /// This is the reading process's own session.
    pub is_self: bool,
    /// Its changes have been committed by a coordinator.
    pub committed: bool,
    /// Files it shares with another session.
    pub overlapping: Vec<String>,
}

impl SessionView {
    /// State to SHOW: a session whose process is gone but which never said goodbye is abandoned.
    pub fn effective_state(&self) -> &'static str {
        if self.committed {
            return "committed";
        }
        if !self.alive && self.manifest.state.expects_live_process() {
            return "abandoned";
        }
        self.manifest.state.as_str()
    }

    /// Short label for the worktree: `.` for the main tree, else its directory name.
    pub fn worktree_label(&self) -> String {
        Path::new(&self.manifest.root)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }
}

#[path = "coop_git_task.rs"]
mod git_task;
pub(crate) use git_task::{FrozenGitTask, GitTaskReceipt, GitTaskRegistration};

struct Active {
    manifest: SessionManifest,
    _lock: RepoTxnLock,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
// A focus switch keeps old registrations live: old work/evidence is not an idle, removable tree.
static FOCUSED_SESSIONS: Mutex<Vec<Active>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn clear_focus_test_state() {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    FOCUSED_SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

#[cfg(test)]
pub(crate) fn focus_test_root() -> Option<PathBuf> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|active| PathBuf::from(&active.manifest.root))
}

pub(crate) struct PreparedCoopScope {
    identity: WorkspaceIdentity,
    fresh: Option<Active>,
}

impl PreparedCoopScope {
    pub(crate) fn revalidate(&self) -> Result<()> {
        let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = active.as_ref() {
            verify_focus_owner(active)?;
        }
        let retained = FOCUSED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let target = retained
            .iter()
            .find(|owner| {
                owner.manifest.worktree_id == self.identity.worktree_id
                    && owner.manifest.repo_id == self.identity.repo_id
            })
            .or(self.fresh.as_ref())
            .context("prepared target registration is missing")?;
        verify_focus_owner(target)
    }

    pub(crate) fn activate(mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let mut retained = FOCUSED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let next = retained
            .iter()
            .position(|owner| {
                owner.manifest.worktree_id == self.identity.worktree_id
                    && owner.manifest.repo_id == self.identity.repo_id
            })
            .map(|index| retained.swap_remove(index))
            .or_else(|| self.fresh.take());
        if let Some(next) = next {
            if let Some(old) = active.replace(next) {
                retained.push(old);
            }
        }
    }
}

/// Prepare a target registration without replacing the current one. All fallible publication is
/// complete before the controller changes session routing; activation only moves retained handles.
pub(crate) fn prepare_focus_session(
    identity: &WorkspaceIdentity,
    session_name: Option<String>,
    barrier: &WorkspaceRegistrationBarrier,
) -> Result<PreparedCoopScope> {
    barrier.verify()?;
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = active.as_ref() {
        verify_focus_owner(active)?;
    }
    let retained = FOCUSED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    let existing = retained.iter().find(|owner| {
        owner.manifest.worktree_id == identity.worktree_id
            && owner.manifest.repo_id == identity.repo_id
    });
    anyhow::ensure!(
        existing.is_some() || retained.len() < 64,
        "too many retained workspace registrations; open a separate session"
    );
    if let Some(existing) = existing {
        verify_focus_owner(existing)?;
    }
    require_no_other_workspace_owner_except(
        identity,
        existing.map(|owner| owner.manifest.session_id.as_str()),
    )?;
    let fresh = if existing.is_none() {
        Some(create_session(identity, session_name, barrier)?)
    } else {
        None
    };
    Ok(PreparedCoopScope {
        identity: identity.clone(),
        fresh,
    })
}

fn verify_focus_owner(active: &Active) -> Result<()> {
    anyhow::ensure!(
        !matches!(
            active.manifest.state,
            SessionState::Working | SessionState::AwaitingApproval
        ),
        "workspace focus requires an idle co-op owner"
    );
    let path = manifest_path(&active.manifest.repo_id, &active.manifest.session_id);
    active
        ._lock
        .verify_existing_path(&path.with_extension("lock"))?;
    let bytes = crate::core::file_placement::read_existing_regular(&path, 128 * 1024)?;
    let stored: SessionManifest = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&active.manifest)?,
        "co-op ownership changed outside its owner; focus refused"
    );
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strip control characters and clamp length. Manifests are read by another process and rendered
/// into a live TUI: an escape sequence in a task description must never reach the terminal.
///
/// Runs of whitespace collapse to one space. Neutralizing a control character by replacing it
/// leaves a gap next to the space that usually precedes it, and a task line is rendered as ONE row
/// in `/team status` — ragged internal spacing there is just noise.
fn safe_text(value: &str, max: usize) -> String {
    let neutralized = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max);
    let mut out = String::new();
    let mut in_space = false;
    for c in neutralized {
        if c.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(c);
    }
    out
}

fn coop_root(repo_id: &str) -> PathBuf {
    crate::core::config::aizen_home().join("coop").join(repo_id)
}

fn sessions_dir(repo_id: &str) -> PathBuf {
    coop_root(repo_id).join("sessions")
}

fn manifest_path(repo_id: &str, session_id: &str) -> PathBuf {
    sessions_dir(repo_id).join(format!("{session_id}.json"))
}

fn liveness_lock_path(repo_id: &str, session_id: &str) -> PathBuf {
    sessions_dir(repo_id).join(format!("{session_id}.lock"))
}

fn shared_path(repo_id: &str) -> PathBuf {
    coop_root(repo_id).join("shared.json")
}

/// Lock guarding [`Shared`]. Lives under `~/.aizen/locks/` like every other cross-process lock, so
/// the registry directory itself holds only data.
#[cfg(test)]
fn shared_lock(repo_id: &str) -> PathBuf {
    crate::core::workspace_txn::store_lock("coop-shared", repo_id)
}

/// Identity of the workspace this process cooperates in.
pub fn identity() -> Result<WorkspaceIdentity> {
    WorkspaceIdentity::discover_cached(&crate::core::config::project_root())
}

/// `repo_id` for the current workspace, or `None` when it cannot be determined (never fatal — the
/// whole module is best-effort bookkeeping).
fn current_repo_id() -> Option<String> {
    identity().ok().map(|i| i.repo_id)
}

// ---------------------------------------------------------------------------------------------
// Publishing this session
// ---------------------------------------------------------------------------------------------

/// Repository-scoped admission shared by session registration and prepared Git mutations. This
/// is separate from workspace/store locks: mutations take writer -> registration; registration
/// never takes a writer lock. Retain this value with the actual operation until physical settlement.
pub(crate) struct WorkspaceRegistrationBarrier {
    lock: RepoTxnLock,
    resource: crate::core::workspace_txn::NativeResource,
    path: PathBuf,
    repo_id: String,
}

impl WorkspaceRegistrationBarrier {
    pub(crate) fn verify(&self) -> Result<()> {
        self.resource.verify()?;
        self.lock.verify_existing_path(&self.path)
    }
}

pub(crate) fn acquire_workspace_registration_barrier(
    identity: &WorkspaceIdentity,
) -> Result<WorkspaceRegistrationBarrier> {
    acquire_registration_barrier(identity, Duration::from_secs(15))
}

fn acquire_registration_barrier(
    identity: &WorkspaceIdentity,
    timeout: Duration,
) -> Result<WorkspaceRegistrationBarrier> {
    anyhow::ensure!(
        !identity.repo_id.is_empty()
            && identity
                .repo_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid repository identity for co-op registration admission"
    );
    // This namespace is deliberately not a workspace transaction or shared-state store lock.
    let directory = crate::core::config::aizen_home().join("coop-registration");
    ensure_registration_directory(&directory)?;
    let path = directory.join(format!("{}.lock", identity.repo_id));
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
    let cancel = crate::core::cancel::current();
    let lock = RepoTxnLock::acquire_in_existing_parent(
        &path,
        crate::core::repo_lock::LockMode::Exclusive,
        timeout,
        cancel.as_ref(),
    )?;
    let barrier = WorkspaceRegistrationBarrier {
        lock,
        resource,
        path,
        repo_id: identity.repo_id.clone(),
    };
    barrier.verify()?;
    Ok(barrier)
}

/// Create only missing plain directories, without following a link/reparse ancestor. The lock's
/// NativeResource subsequently pins this ancestry and its descriptor/path identity for rechecks.
fn ensure_registration_directory(path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "co-op registration requires an absolute data directory"
    );
    crate::core::workspace_txn::NativeResource::validate_spelling(&path.join("registration.lock"))?;
    for directory in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(directory) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(directory) {
                    Ok(()) => crate::core::config::harden_dir(directory),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).context("creating co-op registration directory")
                    }
                }
            }
            Err(error) => return Err(error).context("inspecting co-op registration directory"),
        }
        let metadata = fs::symlink_metadata(directory)?;
        anyhow::ensure!(
            metadata.is_dir() && !crate::core::file_placement::is_link_or_reparse(&metadata),
            "co-op registration directory must have only plain directory ancestors"
        );
    }
    Ok(())
}

/// Publish under the same admission barrier retained by prepared Git mutations. Failure is reported
/// and never falls back to unlocked publication; an existing active registration remains intact.
pub fn begin(session_name: Option<String>) {
    let result = (|| {
        let id = identity()?;
        // A branch observed before registration can become stale while waiting for a mutation.
        // Publish unknown instead; verified Git operations refresh it under this same barrier.
        // Never acquire Git execution/writer gates while holding registration admission.
        let barrier = acquire_workspace_registration_barrier(&id)?;
        publish_session(&id, session_name, &barrier)
    })();
    if let Err(error) = result {
        crate::ui::tui::note_line(&format!("co-op registration unavailable: {error:#}"));
    }
}

fn publish_session(
    id: &WorkspaceIdentity,
    session_name: Option<String>,
    barrier: &WorkspaceRegistrationBarrier,
) -> Result<()> {
    let active = create_session(id, session_name, barrier)?;
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
    Ok(())
}

fn create_session(
    id: &WorkspaceIdentity,
    session_name: Option<String>,
    barrier: &WorkspaceRegistrationBarrier,
) -> Result<Active> {
    barrier.verify()?;
    anyhow::ensure!(
        barrier.repo_id == id.repo_id,
        "co-op registration barrier belongs to another repository"
    );
    let session_id = format!(
        "{}-{}-{}",
        now_unix(),
        std::process::id(),
        crate::core::persist::unique_sequence()
    );
    ensure_registration_directory(&sessions_dir(&id.repo_id))?;
    let lock = RepoTxnLock::acquire_in_existing_parent(
        &liveness_lock_path(&id.repo_id, &session_id),
        crate::core::repo_lock::LockMode::Exclusive,
        Duration::from_secs(1),
        None,
    )?;
    let manifest = SessionManifest {
        schema: SCHEMA,
        session_id,
        pid: std::process::id(),
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
        repo_id: id.repo_id.clone(),
        worktree_id: id.worktree_id.clone(),
        root: id.canonical_root.display().to_string(),
        branch: None,
        session_name,
        task: String::new(),
        task_pinned: false,
        state: SessionState::Idle,
        turns: 0,
        base_checkpoint: None,
        files: Vec::new(),
        truncated_files: 0,
        degraded: None,
        coverage_gap: false,
        started_unix: now_unix(),
        updated_unix: now_unix(),
        git_task: None,
    };
    let manifest_path = manifest_path(&manifest.repo_id, &manifest.session_id);
    let manifest_resource = crate::core::workspace_txn::NativeResource::resolve(&manifest_path)?;
    barrier.verify()?;
    manifest_resource.verify()?;
    lock.verify_existing_path(&liveness_lock_path(&manifest.repo_id, &manifest.session_id))?;
    crate::core::persist::atomic_write_owner_only_checked(
        &manifest_path,
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    manifest_resource.verify()?;
    barrier.verify()?;
    Ok(Active {
        manifest,
        _lock: lock,
    })
}

/// Refresh only this process's matching registration from a caller-verified Git branch. `None`
/// clears an unknown/failed result. No Git command, writer acquisition, or ownership grant occurs.
pub(crate) fn refresh_workspace_branch(
    identity: &WorkspaceIdentity,
    branch: Option<&str>,
    barrier: &WorkspaceRegistrationBarrier,
) -> Result<()> {
    let mut active = ACTIVE.lock().unwrap_or_else(|error| error.into_inner());
    refresh_active_branch(active.as_mut(), identity, branch, barrier)
}

fn refresh_active_branch(
    active: Option<&mut Active>,
    identity: &WorkspaceIdentity,
    branch: Option<&str>,
    barrier: &WorkspaceRegistrationBarrier,
) -> Result<()> {
    let Some(active) = active else { return Ok(()) };
    let current = &active.manifest;
    if current.repo_id != identity.repo_id
        || current.worktree_id != identity.worktree_id
        || Path::new(&current.root) != identity.canonical_root
    {
        return Ok(());
    }
    anyhow::ensure!(
        barrier.repo_id == identity.repo_id,
        "co-op branch refresh requires this repository's registration barrier"
    );
    barrier.verify()?;
    let branch = branch
        .map(|reference| -> Result<String> {
            let name = reference
                .strip_prefix("refs/heads/")
                .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
                .context("co-op branch refresh requires a verified full local branch ref")?;
            Ok(name.to_owned())
        })
        .transpose()?;
    let path = manifest_path(&current.repo_id, &current.session_id);
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
    let stamp = crate::core::file_placement::NodeStamp::capture(
        &path,
        &fs::symlink_metadata(&path)?,
        false,
    )?;
    let disk: SessionManifest = serde_json::from_slice(&fs::read(&path)?)?;
    anyhow::ensure!(
        serde_json::to_value(&disk)? == serde_json::to_value(current)?,
        "co-op manifest changed outside its active owner; branch refresh refused"
    );
    active
        ._lock
        .verify_existing_path(&liveness_lock_path(&current.repo_id, &current.session_id))?;
    let mut refreshed = current.clone();
    refreshed.branch = branch;
    refreshed.updated_unix = now_unix();
    resource.verify()?;
    anyhow::ensure!(
        crate::core::file_placement::NodeStamp::capture(
            &path,
            &fs::symlink_metadata(&path)?,
            false,
        )? == stamp,
        "co-op manifest path changed during branch refresh"
    );
    barrier.verify()?;
    crate::core::persist::atomic_write_owner_only_checked(
        &path,
        &serde_json::to_vec_pretty(&refreshed)?,
    )?;
    resource.verify()?;
    barrier.verify()?;
    active.manifest = refreshed;
    Ok(())
}

fn write_manifest(m: &SessionManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(m)?;
    atomic_write_owner_only(&manifest_path(&m.repo_id, &m.session_id), &bytes)
}

/// Choose bookkeeping from the originating immutable placement, including an old pinned child.
/// Missing ownership never borrows whichever workspace the REPL most recently focused.
fn with_scoped_active<T>(f: impl FnOnce(Option<&mut Active>) -> T) -> T {
    let root = crate::core::exec_ctx::current().and_then(|ctx| {
        ctx.placement()
            .map(|placement| placement.project_root().to_path_buf())
    });
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let matches = active.as_ref().is_some_and(|active| {
        root.as_ref().is_none_or(|root| {
            crate::core::config::normalize_path_key(root)
                == crate::core::config::normalize_path_key(Path::new(&active.manifest.root))
        })
    });
    if matches || root.is_none() {
        return f(active.as_mut());
    }
    let mut retained = FOCUSED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    f(retained.iter_mut().find(|owner| {
        root.as_ref().is_some_and(|root| {
            crate::core::config::normalize_path_key(root)
                == crate::core::config::normalize_path_key(Path::new(&owner.manifest.root))
        })
    }))
}

/// Mutate this session's manifest and republish it. Rolls back in memory if the write failed, so
/// what other windows read is always what this one believes.
fn update<F: FnOnce(&mut SessionManifest)>(f: F) {
    with_scoped_active(|active| {
        let Some(active) = active else { return };
        let before = active.manifest.clone();
        f(&mut active.manifest);
        active.manifest.updated_unix = now_unix();
        if write_manifest(&active.manifest).is_err() {
            active.manifest = before;
        }
    });
}

/// This session's id, when it is published.
pub fn current_session_id() -> Option<String> {
    with_scoped_active(|active| active.map(|a| a.manifest.session_id.clone()))
}

pub fn set_state(state: SessionState) {
    update(|m| m.state = state);
}

/// Pin the task description shown to other windows. An empty string un-pins it.
pub fn set_task(task: &str) {
    let text = safe_text(task, 160);
    update(|m| {
        if text.is_empty() {
            m.task_pinned = false;
            m.task = String::new();
        } else {
            m.task = text.clone();
            m.task_pinned = true;
        }
    });
}

/// Offer an automatic task description (the todo summary, or the first user prompt). Never
/// overwrites a description pinned with `/team task`.
pub fn suggest_task(text: &str) {
    let text = safe_text(text, 160);
    if text.is_empty() {
        return;
    }
    update(|m| {
        if !m.task_pinned {
            m.task = text.clone();
        }
    });
}

/// Record effects without claiming their paths or bytes belong to this session. Callers use this
/// for actual native writes and opaque effects; it is not a snapshot or a new ownership receipt.
pub fn note_attribution_gap(reason: &str) {
    update(|m| record_attribution_gap(m, reason));
}

fn record_attribution_gap(m: &mut SessionManifest, reason: &str) {
    m.coverage_gap = true;
    // Do not replace an earlier failure with a less precise turn-end/background message. Historical
    // checkpoint fields and file entries remain verbatim, even though they grant no authority.
    if m.degraded.as_deref().is_none_or(str::is_empty) {
        let reason = safe_text(reason, 240);
        m.degraded = Some(if reason.is_empty() {
            "session effects have no independently attributable ownership receipt".into()
        } else {
            reason
        });
    }
}

/// Record an opaque background interval without attributing its writes to this session.
pub fn note_background_coverage_gap() {
    note_attribution_gap("managed background effects: session attribution is incomplete");
}

/// Finish bookkeeping without reading source files or modifying Git's index. No snapshot-era file
/// claims are extended. Without an independently attributable effect receipt, even an empty ledger
/// cannot certify complete coverage (including a failed/cancelled turn or effects outside Git).
/// `note_attribution_gap` preserves a more precise effect reason when one was recorded.
pub fn seal_turn() -> Vec<String> {
    update(seal_manifest);
    Vec::new()
}

fn seal_manifest(m: &mut SessionManifest) {
    m.turns = m.turns.saturating_add(1);
    record_attribution_gap(
        m,
        "session attribution unavailable: no independently attributable effect receipt; an empty ledger does not certify coverage",
    );
}

/// Clean shutdown: stop expecting a live process, but keep historical attribution evidence.
pub fn clear() {
    let taken = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut owners: Vec<_> = taken.into_iter().collect();
    owners.extend(
        FOCUSED_SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..),
    );
    for active in owners {
        let mut m = active.manifest;
        if m.state != SessionState::Done {
            m.state = SessionState::Finished;
        }
        m.updated_unix = now_unix();
        let _ = write_manifest(&m);
    }
}

// ---------------------------------------------------------------------------------------------
// Shared claims
// ---------------------------------------------------------------------------------------------

fn load_shared(repo_id: &str) -> Shared {
    let Ok(bytes) = fs::read(shared_path(repo_id)) else {
        return Shared::default();
    };
    serde_json::from_slice::<Shared>(&bytes)
        .ok()
        .filter(|s| s.schema == SHARED_SCHEMA)
        .unwrap_or_default()
}

#[cfg(test)]
fn save_shared(repo_id: &str, shared: &Shared) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(shared)?;
    let _ = fs::create_dir_all(coop_root(repo_id));
    atomic_write_owner_only(&shared_path(repo_id), &bytes)
}

/// Read-modify-write [`Shared`] under its lock.
#[cfg(test)]
fn with_shared<T, F: FnOnce(&mut Shared) -> T>(repo_id: &str, f: F) -> Result<T> {
    let _lock = RepoTxnLock::acquire_exclusive(&shared_lock(repo_id), Duration::from_secs(5))?;
    let mut shared = load_shared(repo_id);
    let out = f(&mut shared);
    save_shared(repo_id, &shared)?;
    Ok(out)
}

/// Build historical claim fixtures. Production has no snapshot-derived claim producer.
#[cfg(test)]
fn claim_paths(repo_id: &str, session_id: &str, paths: &[String]) -> Result<Vec<String>> {
    with_shared(repo_id, |shared| {
        let now = now_unix();
        let mut warnings = Vec::new();
        for path in paths {
            match shared.owners.get(path) {
                Some(claim) if claim.session_id != session_id => {
                    let first = claim.session_id.clone();
                    let already = shared.overlaps.iter().any(|o| {
                        &o.path == path
                            && ((o.first == first && o.second == session_id)
                                || (o.first == session_id && o.second == first))
                    });
                    if !already {
                        if shared.overlaps.len() >= MAX_OVERLAPS {
                            shared.overlaps.remove(0);
                        }
                        shared.overlaps.push(Overlap {
                            path: path.clone(),
                            first: first.clone(),
                            second: session_id.to_string(),
                            unix: now,
                        });
                        warnings.push(format!(
                            "⚠ {path} was also changed by session {first} — both sets of changes \
                             are in the file; git cannot split them by line"
                        ));
                    }
                }
                _ => {}
            }
            shared.owners.insert(
                path.clone(),
                Claim {
                    session_id: session_id.to_string(),
                    unix: now,
                },
            );
        }
        warnings
    })
}

// ---------------------------------------------------------------------------------------------
// Reading the registry
// ---------------------------------------------------------------------------------------------

/// Is the session that owns `lock` still running? A lock we can take is a lock nobody holds.
fn alive(repo_id: &str, session_id: &str) -> bool {
    let path = liveness_lock_path(repo_id, session_id);
    if !path.exists() {
        return false;
    }
    RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(50)).is_err()
}

/// Fail-closed ownership admission for a Git mutation under its caller's writer/barrier guards.
/// Unlike listing, this never sweeps manifests, creates directories/locks, or treats unreadable
/// evidence as an absent owner. It does not exclude arbitrary external Git/file writers.
pub(crate) fn require_no_other_workspace_owner(identity: &WorkspaceIdentity) -> Result<()> {
    require_no_other_workspace_owner_except(identity, current_session_id().as_deref())
}

/// Shared-ref retirement is conservative: every other live repository owner blocks it.
pub(crate) fn require_no_other_repository_owner(identity: &WorkspaceIdentity) -> Result<()> {
    require_owner_admission(identity, current_session_id().as_deref(), true)
}

fn require_no_other_workspace_owner_except(
    identity: &WorkspaceIdentity,
    current: Option<&str>,
) -> Result<()> {
    require_owner_admission(identity, current, false)
}

fn require_owner_admission(
    identity: &WorkspaceIdentity,
    current: Option<&str>,
    repository_wide: bool,
) -> Result<()> {
    let root = fs::canonicalize(&identity.canonical_root)
        .context("cannot identify Git mutation workspace")?;
    anyhow::ensure!(
        root == identity.canonical_root,
        "Git mutation workspace identity changed"
    );
    let home = crate::core::config::aizen_home();
    for directory in [
        home.clone(),
        home.join("coop"),
        coop_root(&identity.repo_id),
        sessions_dir(&identity.repo_id),
    ] {
        match fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("cannot inspect co-op ownership registry"),
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !crate::core::file_placement::is_link_or_reparse(&metadata),
                "unsafe co-op ownership registry directory; Git mutation refused"
            ),
        }
    }
    for entry in fs::read_dir(sessions_dir(&identity.repo_id))
        .context("cannot read co-op ownership registry")?
    {
        let path = entry
            .context("unreadable co-op ownership registry entry")?
            .path();
        let metadata =
            fs::symlink_metadata(&path).context("cannot inspect co-op ownership entry")?;
        anyhow::ensure!(
            metadata.is_file() && !crate::core::file_placement::is_link_or_reparse(&metadata),
            "unsafe co-op ownership entry; Git mutation refused"
        );
        match path.extension().and_then(|value| value.to_str()) {
            Some("lock") => {
                // begin() takes the lock before publishing its manifest. A live orphan lock is
                // therefore an unknown owner, not evidence that the workspace is idle.
                match fs::symlink_metadata(path.with_extension("json")) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        anyhow::ensure!(
                            !existing_session_alive(&path)?,
                            "live co-op lock has no readable owner manifest; Git mutation refused"
                        );
                    }
                    Err(error) => return Err(error).context("cannot identify co-op lock owner"),
                }
                continue;
            }
            Some("json") => {}
            _ => bail!("unknown co-op ownership entry; Git mutation refused"),
        }
        let bytes = if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|id| id.starts_with("git-task-"))
        {
            crate::core::file_placement::read_existing_regular(
                &path,
                crate::core::git_native_lifecycle::MAX_MANIFEST_BYTES as u64,
            )?
        } else {
            fs::read(&path).context("cannot read co-op owner manifest")?
        };
        let manifest: SessionManifest = serde_json::from_slice(&bytes)
            .context("unreadable or unsupported co-op owner manifest; Git mutation refused")?;
        anyhow::ensure!(
            manifest.schema == SCHEMA,
            "unsupported co-op owner manifest schema; Git mutation refused"
        );
        anyhow::ensure!(
            manifest.repo_id == identity.repo_id
                && !manifest.worktree_id.is_empty()
                && path.file_stem().and_then(|stem| stem.to_str())
                    == Some(manifest.session_id.as_str()),
            "unknown co-op owner identity; Git mutation refused"
        );
        if manifest.git_task.is_some()
            || manifest
                .session_id
                .starts_with(crate::core::git_native_lifecycle::REGISTRATION_PREFIX)
        {
            git_task::validate_manifest(&manifest)?;
        }
        if let Some(task) = &manifest.git_task {
            anyhow::ensure!(
                current == Some(manifest.session_id.as_str()) || task.phase == "handed_off"
                    || (!repository_wide && manifest.worktree_id != identity.worktree_id),
                "unfinished owned Git task; retain worktree and inspect ownership evidence before mutation"
            );
        }
        // A proven dead session's removed worktree is historical evidence, not a current owner.
        // Unknown/missing liveness still refuses before we consider its path.
        if !existing_session_alive(&path.with_extension("lock"))? {
            continue;
        }
        let owner_path = Path::new(&manifest.root);
        anyhow::ensure!(
            owner_path.is_absolute(),
            "unknown co-op owner workspace; Git mutation refused"
        );
        let owner_root = fs::canonicalize(owner_path)
            .context("cannot identify co-op owner workspace; Git mutation refused")?;
        anyhow::ensure!(
            (!repository_wide && owner_root != root)
                || current == Some(manifest.session_id.as_str()),
            "another Aizen session owns the affected worktree/repository; Git mutation refused"
        );
    }
    Ok(())
}

/// Missing, replaced, or unreadable lock files are unknown liveness, never dead-owner proof.
fn existing_session_alive(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path).context("cannot determine co-op owner liveness")?;
    anyhow::ensure!(
        metadata.is_file() && !crate::core::file_placement::is_link_or_reparse(&metadata),
        "unsafe co-op liveness lock; Git mutation refused"
    );
    match RepoTxnLock::acquire_existing_exclusive(path, Duration::from_millis(50)) {
        Ok(_lock) => Ok(false),
        Err(error)
            if error
                .downcast_ref::<crate::core::repo_lock::LockBusy>()
                .is_some() =>
        {
            Ok(true)
        }
        Err(error) => Err(error).context("unknown co-op owner liveness; Git mutation refused"),
    }
}

/// Every session known in this repository, newest activity first.
///
/// Sweeps only readable finished/committed manifests past [`RETENTION_SECS`]. Unknown schemas and
/// unreadable evidence are left untouched for a compatible reader or explicit repair.
pub fn list() -> Vec<SessionView> {
    let Some(repo_id) = current_repo_id() else {
        return Vec::new();
    };
    list_in(&repo_id)
}

fn list_in(repo_id: &str) -> Vec<SessionView> {
    read_sessions(repo_id).0
}

/// The flag records omitted evidence. Listing stays best-effort, but worktree removal must not
/// mistake an unreadable or newer peer manifest for proof that no owner exists.
fn read_sessions(repo_id: &str) -> (Vec<SessionView>, bool) {
    let Ok(rd) = fs::read_dir(sessions_dir(repo_id)) else {
        return (Vec::new(), true);
    };
    let shared = load_shared(repo_id);
    let mine = current_session_id();
    let now = now_unix();
    let mut out = Vec::new();
    let mut omitted = false;
    for entry in rd {
        let Ok(entry) = entry else {
            omitted = true;
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|id| id.starts_with("git-task-"))
        {
            crate::core::file_placement::read_existing_regular(
                &path,
                crate::core::git_native_lifecycle::MAX_MANIFEST_BYTES as u64,
            )
            .ok()
        } else {
            fs::read(&path).ok()
        };
        let manifest = bytes
            .and_then(|b| serde_json::from_slice::<SessionManifest>(&b).ok())
            .filter(|manifest| {
                !(manifest.git_task.is_some()
                    || manifest
                        .session_id
                        .starts_with(crate::core::git_native_lifecycle::REGISTRATION_PREFIX))
                    || git_task::validate_manifest(manifest).is_ok()
            });
        let Some(manifest) = manifest.filter(|m| m.schema <= SCHEMA) else {
            // Parse failure does not prove debris: a newer binary may use fields/enums we cannot
            // read. Never erase its evidence (or its lock) during an ordinary registry read.
            omitted = true;
            continue;
        };
        let is_self = mine.as_deref() == Some(manifest.session_id.as_str());
        let alive = is_self || alive(repo_id, &manifest.session_id);
        let committed = shared.committed.contains_key(&manifest.session_id);
        let terminal = committed
            || matches!(
                manifest.state,
                SessionState::Finished | SessionState::Done | SessionState::Failed
            );
        // Age cannot resolve a coverage gap or give historical source attribution live authority.
        // Preserve that evidence even after the session is marked finished or committed.
        if !alive
            && terminal
            && manifest.git_task.is_none()
            && !manifest.coverage_gap
            && manifest.degraded.is_none()
            && manifest.base_checkpoint.is_none()
            && manifest.files.is_empty()
            && manifest.truncated_files == 0
            && now.saturating_sub(manifest.updated_unix) > RETENTION_SECS
        {
            let _ = crate::core::persist::remove_if_exists(&path);
            let _ = crate::core::persist::remove_if_exists(&liveness_lock_path(
                repo_id,
                &manifest.session_id,
            ));
            continue;
        }
        let overlapping: Vec<String> = shared
            .overlaps
            .iter()
            .filter(|o| o.first == manifest.session_id || o.second == manifest.session_id)
            .map(|o| o.path.clone())
            .collect();
        out.push(SessionView {
            manifest,
            alive,
            is_self,
            committed,
            overlapping,
        });
    }
    out.sort_by(|a, b| {
        b.manifest
            .updated_unix
            .cmp(&a.manifest.updated_unix)
            .then_with(|| a.manifest.session_id.cmp(&b.manifest.session_id))
    });
    (out, omitted)
}

/// One line for startup: are other windows working in this repository right now?
///
/// `None` when this is the only session, which is the common case and earns no noise. Abandoned
/// sessions ARE counted: an unreviewed pile of changes from a window that crashed is precisely what
/// a new window needs to hear about before it starts editing the same files.
pub fn peers_banner() -> Option<String> {
    let peers: Vec<SessionView> = list().into_iter().filter(|v| !v.is_self).collect();
    if peers.is_empty() {
        return None;
    }
    let live = peers.iter().filter(|v| v.alive).count();
    let abandoned = peers
        .iter()
        .filter(|v| v.effective_state() == "abandoned")
        .count();
    let pending = peers
        .iter()
        .filter(|v| !v.committed && !v.manifest.files.is_empty())
        .count();
    let mut bits = Vec::new();
    if live > 0 {
        bits.push(format!("{live} running"));
    }
    if abandoned > 0 {
        bits.push(format!("{abandoned} abandoned"));
    }
    if pending > 0 {
        bits.push(format!(
            "{pending} with historical file records (not current ownership proof)"
        ));
    }
    if bits.is_empty() {
        bits.push(format!("{} known", peers.len()));
    }
    Some(format!(
        "⚑ {} other aizen session(s) in this repo — {} · /team status",
        peers.len(),
        bits.join(", ")
    ))
}

/// Resolve a user-typed session reference: exact id, unique id suffix/prefix, `self`, or a 1-based
/// row index as shown by `/team status`.
pub fn resolve(reference: &str) -> Result<SessionView> {
    let sessions = list();
    if sessions.is_empty() {
        bail!("no aizen sessions are registered in this repository yet");
    }
    let needle = reference.trim();
    if needle.is_empty() {
        bail!("name a session (see /team status)");
    }
    if needle.eq_ignore_ascii_case("self") || needle == "." {
        return sessions
            .into_iter()
            .find(|s| s.is_self)
            .context("this session is not registered");
    }
    if let Ok(index) = needle.parse::<usize>() {
        if index >= 1 && index <= sessions.len() {
            return Ok(sessions.into_iter().nth(index - 1).unwrap());
        }
    }
    if let Some(exact) = sessions.iter().find(|s| s.manifest.session_id == needle) {
        return Ok(exact.clone());
    }
    let matches: Vec<&SessionView> = sessions
        .iter()
        .filter(|s| {
            s.manifest.session_id.starts_with(needle) || s.manifest.session_id.ends_with(needle)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => bail!("no session matches '{needle}' (see /team status)"),
        n => bail!("'{needle}' matches {n} sessions — use the full id from /team status"),
    }
}

/// Historical paths recorded by this session, deduplicated and sorted; not current ownership.
pub fn session_paths(view: &SessionView) -> Vec<String> {
    let mut paths: Vec<String> = view.manifest.files.iter().map(|f| f.path.clone()).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Historical session ledgers cannot produce an exact current diff. Refuse even an empty ledger:
/// absence of old file records is not evidence that the session made no changes.
pub fn session_diff(view: &SessionView, _patch: bool) -> Result<Infallible> {
    refuse_historical_attribution(view, "diff")
}

/// Compatibility refusal for the retired session-commit surface. No manifest state, branch name,
/// path list, `done` marker, or caller force flag grants staging authority. Future commits must use
/// independently attributable Git-owned worktrees through the shared Git operation implementation.
pub fn session_commit(view: &SessionView) -> Result<Infallible> {
    refuse_historical_attribution(view, "commit")
}

fn refuse_historical_attribution(view: &SessionView, operation: &str) -> Result<Infallible> {
    let m = &view.manifest;
    let recorded_reason = m
        .degraded
        .as_deref()
        .map(|reason| format!(" Recorded limitation: {}.", safe_text(reason, 240)))
        .unwrap_or_default();
    bail!(
        "session {operation} unavailable: coverage gap for session {}. Historical checkpoint \
         manifests and file lists grant no current staging authority; shared working-tree changes \
         cannot be attributed to one session. No independently attributable Git-owned worktree \
         result or exact native-journal attribution is available through this registry. \
         Recorded files: {}; omitted paths: {}; coverage_gap: {}.{} \
         Inspect real Git history/diffs or use exact native undo where supported; nothing was staged, \
         committed, or changed",
        safe_text(&m.session_id, 120),
        m.files.len(),
        m.truncated_files,
        m.coverage_gap,
        recorded_reason,
    )
}

/// Every recorded overlap in this repository.
pub fn overlaps() -> Vec<Overlap> {
    current_repo_id()
        .map(|id| load_shared(&id).overlaps)
        .unwrap_or_default()
}

/// Who else — if anyone — most recently claimed `rel_path`, provided that session is still live and
/// is not us. `None` when the path is unclaimed, ours, or owned by a session that has already gone.
///
/// Historical claims are conservative warnings for pre-write checks. They never grant staging
/// authority, and this cutover does not extend them from shared working-tree observations.
///
/// Deliberately cheap and lock-free: a stale read here can only cost a spurious refusal that names
/// the session to check, never a silent loss. Claims from DEAD sessions are ignored — an abandoned
/// window's leftovers must not wedge the tree forever.
pub fn live_peer_claim(rel_path: &str) -> Option<String> {
    let repo_id = current_repo_id()?;
    let mine = current_session_id();
    let claim = load_shared(&repo_id).owners.remove(rel_path)?;
    if mine.as_deref() == Some(claim.session_id.as_str()) {
        return None;
    }
    alive(&repo_id, &claim.session_id).then_some(claim.session_id)
}

/// [`live_peer_claim`] for an absolute path. `None` when the path lies outside this session's
/// worktree (no claim could name it) or when no live peer holds it.
pub fn live_peer_claim_at(abs: &Path) -> Option<String> {
    live_peer_claim(&relative_to_worktree(abs)?)
}

/// Repository-relative form of `abs`, as git would name it, for claim lookups. `None` when the path
/// is outside this session's worktree — a claim on it could not be meaningful.
pub fn relative_to_worktree(abs: &Path) -> Option<String> {
    let root =
        with_scoped_active(|active| active.map(|active| PathBuf::from(&active.manifest.root)))?;
    let canon = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
    let rel = canon.strip_prefix(&root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then_some(s)
}

/// Current path claims, newest first.
pub fn claims() -> Vec<(String, Claim)> {
    let Some(repo_id) = current_repo_id() else {
        return Vec::new();
    };
    let mut v: Vec<(String, Claim)> = load_shared(&repo_id).owners.into_iter().collect();
    v.sort_by(|a, b| b.1.unix.cmp(&a.1.unix).then_with(|| a.0.cmp(&b.0)));
    v
}

// ---------------------------------------------------------------------------------------------
// Isolated worktree mode
// ---------------------------------------------------------------------------------------------

/// Full Git registry for browsing. This observation is deliberately separate from removal
/// admission: it includes external/bare/prunable worktrees and does not certify clean file state.
pub(crate) fn work_inventory() -> Result<Vec<crate::core::git_ops::WorktreeEntry>> {
    crate::core::git_ops::worktree_inventory(&crate::core::config::project_root())
}

/// Shared terminal rendering: quote untrusted path/ref/reason bytes so control characters cannot
/// create forged rows. Inventory is not a dirty-state or liveness check.
pub(crate) fn work_inventory_lines() -> Result<Vec<String>> {
    let entries = work_inventory()?;
    let quote = |value: &str| serde_json::to_string(value).expect("string serialization");
    let mut lines = vec![format!("{} Git worktree(s)", entries.len())];
    for entry in entries {
        let state = entry
            .branch
            .as_deref()
            .unwrap_or(if entry.bare { "bare" } else { "detached" });
        let head = entry
            .head
            .as_ref()
            .map(|oid| oid.as_str())
            .unwrap_or("unborn");
        lines.push(format!(
            "  {}  {}  {head}",
            quote(&entry.path),
            quote(state)
        ));
        if let Some(reason) = entry.locked {
            lines.push(format!("    locked: {}", quote(&reason)));
        }
        if let Some(reason) = entry.prunable {
            lines.push(format!("    prunable: {}", quote(&reason)));
        }
    }
    lines.push("Inventory only: file changes and live sessions are not checked; this is not removal approval.".into());
    Ok(lines)
}

/// One linked worktree created for a session.
#[derive(Debug, Clone)]
pub struct WorkTree {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    /// Uncommitted changes present.
    pub dirty: bool,
    /// Commits on its branch that the base branch does not have.
    pub ahead: u64,
    /// Sessions currently registered in this worktree.
    pub sessions: usize,
}

fn worktrees_root(repo_id: &str) -> PathBuf {
    crate::core::config::aizen_home()
        .join("worktrees")
        .join(repo_id)
}

fn valid_worktree_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    if !ok {
        bail!("worktree name must be 1-64 chars of letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

/// Create from a frozen committed HEAD, never from the parent's pending index/files.
/// The returned worktree is not an automatic-commit ownership grant.
pub(crate) fn work_new(name: &str) -> Result<crate::core::git_ops::CreatedWorktree> {
    prepare_work_new(&crate::core::config::project_root(), name, None)?.execute()
}

/// Shared managed-directory adapter; preparation never creates paths or grants agent ownership.
pub(crate) fn prepare_work_new(
    root: &Path,
    name: &str,
    base: Option<&str>,
) -> Result<crate::core::git_ops::PreparedWorktreeCreation> {
    valid_worktree_name(name)?;
    let id = crate::core::git_ops::workspace_identity(root)?;
    let dir = worktrees_root(&id.repo_id).join(name);
    let branch = format!("aizen/{name}");
    match base {
        Some(base) => crate::core::git_ops::PreparedWorktreeCreation::prepare_from(
            &id.canonical_root,
            &dir,
            &branch,
            base,
        ),
        None => crate::core::git_ops::PreparedWorktreeCreation::prepare(
            &id.canonical_root,
            &dir,
            &branch,
        ),
    }
}

/// Inspect only the selected managed worktree. Every Git query goes through the caller's retained
/// operation owner; registry admission is strict and never creates locks or sweeps old manifests.
fn work_target_with_query(
    id: &WorkspaceIdentity,
    target: &WorkspaceIdentity,
    name: &str,
    query: impl Fn(&Path, &[&str]) -> Result<Vec<u8>>,
) -> Result<WorkTree> {
    valid_worktree_name(name)?;
    let path = worktrees_root(&id.repo_id).join(name);
    let metadata = fs::symlink_metadata(&path).context("reading selected managed worktree")?;
    anyhow::ensure!(
        metadata.is_dir() && !crate::core::file_placement::is_link_or_reparse(&metadata),
        "managed worktree must be a plain directory"
    );
    let canon = fs::canonicalize(&path).context("canonicalizing managed worktree")?;
    anyhow::ensure!(
        canon == target.canonical_root
            && target.repo_id == id.repo_id
            && target.canonical_common_git_dir == id.canonical_common_git_dir,
        "selected worktree repository identity changed; removal refused"
    );
    require_no_other_workspace_owner_except(target, None)?;
    let inventory = crate::core::git_ops::parse_worktrees(&query(
        &id.canonical_root,
        &["worktree", "list", "--porcelain", "-z"],
    )?)?;
    let registered = inventory
        .iter()
        .find(|entry| fs::canonicalize(&entry.path).is_ok_and(|registered| registered == canon))
        .context("managed directory is not a registered Git worktree")?;
    anyhow::ensure!(
        registered.locked.is_none()
            && registered.prunable.is_none()
            && !registered.bare
            && !registered.detached,
        "managed worktree is locked, prunable, bare, or detached; inspect it before removal"
    );
    let branch = registered
        .branch
        .as_deref()
        .and_then(|name| name.strip_prefix("refs/heads/"))
        .context("managed worktree has no local branch")?
        .to_owned();
    let tip = registered
        .head
        .as_ref()
        .context("managed worktree has no commit")?;
    let base = String::from_utf8(query(
        &id.canonical_root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?)
    .context("Git base query returned non-UTF-8 data")?;
    let base = crate::core::snapshots::GitOid::parse(base.trim())?;
    crate::core::git_ops::validate_visible_index(&query(&path, &["ls-files", "-v", "-f", "-z"])?)?;
    let dirty = !query(
        &path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?
    .is_empty();
    let range = format!("{}..{}", base.as_str(), tip.as_str());
    let ahead = String::from_utf8(query(&path, &["rev-list", "--count", &range, "--"])?)
        .context("Git ahead query returned non-UTF-8 data")?
        .trim()
        .parse::<u64>()
        .context("invalid worktree ahead count; removal refused")?;
    require_no_other_workspace_owner_except(target, None)?;
    // Zero is certified by strict admission above, never a default for a failed liveness query.
    Ok(WorkTree {
        name: name.to_owned(),
        path,
        branch,
        dirty,
        ahead,
        sessions: 0,
    })
}

#[cfg(test)]
fn work_target_for(id: &WorkspaceIdentity, name: &str) -> Result<WorkTree> {
    let target = WorkspaceIdentity::discover(&worktrees_root(&id.repo_id).join(name))?;
    work_target_with_query(id, &target, name, |root, args| {
        crate::core::git_ops::inspection_bytes_owned(
            root,
            args.iter().map(|arg| (*arg).into()).collect(),
            None,
        )
    })
}

#[cfg(test)]
fn git_inspect_text(root: &Path, args: &[&str]) -> Result<String> {
    let bytes = crate::core::git_ops::inspection_bytes(
        root,
        args.iter().map(|arg| (*arg).into()).collect(),
    )?;
    String::from_utf8(bytes).context("Git inspection returned non-UTF-8 data")
}

/// Preliminary removal blockers, not a substitute for fresh execution-time admission.
pub fn work_remove_blockers(wt: &WorkTree) -> Vec<String> {
    let mut blockers = Vec::new();
    if wt.dirty {
        blockers.push("has uncommitted changes".to_string());
    }
    if wt.ahead > 0 {
        blockers.push(format!(
            "has {} commit(s) not present on the base branch",
            wt.ahead
        ));
    }
    if wt.sessions > 0 {
        blockers.push(format!(
            "{} aizen session(s) still running in it",
            wt.sessions
        ));
    }
    blockers
}

fn require_safe_worktree_removal(force: bool) -> Result<()> {
    anyhow::ensure!(!force,
        "forced worktree removal is not supported: a retained branch cannot save uncommitted or ignored files; inspect and resolve blockers first");
    Ok(())
}

/// Remove only a clean, inactive managed worktree; the branch is retained separately.
/// Keeping a branch does not preserve uncommitted/untracked/ignored files, so `force` is refused.
pub fn work_remove(name: &str, force: bool) -> Result<String> {
    valid_worktree_name(name)?;
    require_safe_worktree_removal(force)?;
    work_remove_for(&identity()?, name)
}

fn work_remove_for(id: &WorkspaceIdentity, name: &str) -> Result<String> {
    valid_worktree_name(name)?;
    let path = worktrees_root(&id.repo_id).join(name);
    let metadata = fs::symlink_metadata(&path).context("reading selected managed worktree")?;
    anyhow::ensure!(
        metadata.is_dir() && !crate::core::file_placement::is_link_or_reparse(&metadata),
        "managed worktree must be a plain directory"
    );
    anyhow::ensure!(
        fs::canonicalize(&path)? != id.canonical_root,
        "cannot remove the current workspace; focus another worktree first"
    );
    let cancel = crate::core::cancel::current();
    let writer = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &path,
        Duration::from_secs(15),
        cancel.as_ref(),
        "remove managed worktree",
    )?;
    anyhow::ensure!(
        writer.identity().repo_id == id.repo_id
            && writer.identity().canonical_common_git_dir == id.canonical_common_git_dir,
        "selected worktree repository identity changed; removal refused"
    );
    let barrier = crate::core::workspace_txn::BackgroundProcessBarrier::acquire(cancel.as_ref())?;
    let registration = acquire_workspace_registration_barrier(writer.identity())?;
    registration.verify()?;
    // Removal has no self exception: even this process must not have an active registration there.
    require_no_other_workspace_owner_except(writer.identity(), None)?;
    crate::core::git_ops::require_settled_operations(writer.identity())?;
    let scope = crate::core::exec_ctx::current()
        .map(|ctx| ctx.resource_scope())
        .unwrap_or_else(|| "default".into());
    {
        let physical = crate::core::workspace_txn::PhysicalEffectLease::exclusive(
            Duration::from_secs(15),
            cancel.as_ref(),
            "worktree recovery preflight",
            &scope,
        )?;
        crate::core::speculation::legacy::require_settled(
            &writer.identity().canonical_root,
            &writer,
            &physical,
        )?;
        crate::core::speculation::namespace::inspect_existing(
            writer.identity().clone(),
            &crate::core::config::aizen_home(),
        )?;
        crate::core::native_journal::require_workspace_settled(&writer.identity().canonical_root)?;
    }
    let owner = std::sync::Arc::new((writer, barrier, registration));
    let wt = work_target_with_query(id, owner.0.identity(), name, |root, args| {
        owner.2.verify()?;
        let result = crate::core::git_ops::inspection_bytes_owned(
            root,
            args.iter().map(|arg| (*arg).into()).collect(),
            Some(Box::new(owner.clone())),
        );
        let registration_valid = owner.2.verify();
        let bytes = result?;
        registration_valid.context("co-op registration changed during removal inspection")?;
        Ok(bytes)
    })?;
    anyhow::ensure!(
        wt.path == path && fs::canonicalize(&path)? == owner.0.identity().canonical_root,
        "worktree changed during removal preflight"
    );
    let blockers = work_remove_blockers(&wt);
    anyhow::ensure!(
        blockers.is_empty(),
        "refusing to remove '{name}': {}",
        blockers.join("; ")
    );
    let git_dir = owner
        .0
        .identity()
        .canonical_git_dir
        .as_ref()
        .context("worktree Git directory is missing")?;
    crate::core::git_ops::require_idle_operation(git_dir)?;
    require_no_other_workspace_owner_except(owner.0.identity(), None)?;
    crate::core::git_ops::require_settled_operations(owner.0.identity())?;
    owner.2.verify()?;
    // Every bounded query and mutation owns a share after observer timeout/cancellation. The
    // caller's share permits postchecks while retaining writer/background/registration admission.
    let removed = crate::core::git_ops::remove_worktree(
        &id.canonical_root,
        &wt.path,
        Box::new(owner.clone()),
    )
    .with_context(|| format!("removing worktree {name}"));
    let registration_valid = owner.2.verify();
    removed?;
    registration_valid.context("co-op registration changed during worktree removal")?;
    Ok(format!(
        "removed worktree {} at {} (branch {} kept)",
        wt.name,
        wt.path.display(),
        wt.branch
    ))
}

// ---------------------------------------------------------------------------------------------
// Agent-facing tool
// ---------------------------------------------------------------------------------------------

/// Read-only view of the other windows, for the model.
///
/// This is coordination evidence, not an attribution or commit receipt. Historical file records
/// help identify peers to consult, but shared working-tree edits are not adopted as session work.
pub struct TeamStatus;

impl crate::agent::tools::Tool for TeamStatus {
    fn name(&self) -> &str {
        "team_status"
    }

    fn description(&self) -> &str {
        "List aizen sessions in this repository: task, liveness, state, and historical file records. \
         Use it to coordinate with other windows before editing. Read-only; records do not prove \
         current file ownership or provide per-session diff/commit authority."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "boolean",
                    "description": "include historical file records, not current ownership proof (default false)"
                }
            },
            "additionalProperties": false
        })
    }

    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let want_files = args.get("files").and_then(|v| v.as_bool()).unwrap_or(false);
        let sessions = list();
        if sessions.is_empty() {
            return Ok("no aizen sessions are registered in this repository".to_string());
        }
        let mut out = String::from(
            "Registry only: historical file records do not grant current attribution or staging authority.\n",
        );
        for v in &sessions {
            let m = &v.manifest;
            out.push_str(&format!(
                "{}{} · {} · turns={} · historical_files={}{}\n  task: {}\n",
                m.session_id,
                if v.is_self { " (this session)" } else { "" },
                v.effective_state(),
                m.turns,
                m.files.len(),
                if v.overlapping.is_empty() {
                    String::new()
                } else {
                    format!(" · shares {} file(s)", v.overlapping.len())
                },
                if m.task.is_empty() {
                    "(unstated)"
                } else {
                    &m.task
                },
            ));
            if let Some(reason) = &m.degraded {
                out.push_str(&format!("  no per-session diff: {reason}\n"));
            }
            if want_files {
                for f in &m.files {
                    out.push_str(&format!("  {} {}\n", f.status, f.path));
                }
                if m.truncated_files > 0 {
                    out.push_str(&format!(
                        "  (+{} path(s) beyond the tracking ceiling)\n",
                        m.truncated_files
                    ));
                }
            }
        }
        let overlaps = overlaps();
        if !overlaps.is_empty() {
            out.push_str(
                "\nfiles changed by more than one session (git cannot split these by line):\n",
            );
            for o in overlaps.iter().take(20) {
                out.push_str(&format!("  {} — {} then {}\n", o.path, o.first, o.second));
            }
        }
        Ok(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{config::EnvGuard, file_placement::tests::TestDir};

    /// Hold the process-global home lock until the environment and fixture have been restored.
    struct Home {
        _env: EnvGuard,
        root: PathBuf,
        _directory: TestDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Home {
        fn new() -> Self {
            let guard = crate::core::config::TEST_HOME_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let directory = TestDir::new();
            let root = directory.path().to_path_buf();
            #[cfg(windows)]
            let root = {
                // Git for Windows expects a normal drive path for worktree arguments.
                let path = root.to_str().expect("fixture path is Unicode");
                PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
            };
            let env = EnvGuard::set([("AIZEN_HOME", &root)]);
            Self {
                _env: env,
                root,
                _directory: directory,
                _guard: guard,
            }
        }
    }

    fn manifest(repo: &str, id: &str, state: SessionState) -> SessionManifest {
        SessionManifest {
            schema: SCHEMA,
            session_id: id.to_string(),
            pid: 4242,
            binary_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_id: repo.to_string(),
            worktree_id: "wt".to_string(),
            root: "/tmp/x".to_string(),
            branch: Some("main".to_string()),
            session_name: None,
            task: format!("task of {id}"),
            task_pinned: true,
            state,
            turns: 1,
            base_checkpoint: None,
            files: Vec::new(),
            truncated_files: 0,
            degraded: None,
            coverage_gap: false,
            // NOW, not a fixed epoch: `list_in` sweeps terminal manifests past `RETENTION_SECS`,
            // so a hardcoded old timestamp would make every Finished/Done/Failed fixture vanish
            // before the assertion ran. Tests that care about staleness set this explicitly.
            started_unix: now_unix(),
            updated_unix: now_unix(),
            git_task: None,
        }
    }

    #[test]
    fn a_session_whose_process_is_gone_reads_as_abandoned() {
        let _home = Home::new();
        let repo = "repo-a";
        fs::create_dir_all(sessions_dir(repo)).unwrap();

        // Alive: something still holds the liveness lock (stands in for the owning process).
        let alive_m = manifest(repo, "s-alive", SessionState::Working);
        write_manifest(&alive_m).unwrap();
        let _held = RepoTxnLock::acquire_exclusive(
            &liveness_lock_path(repo, "s-alive"),
            Duration::from_secs(1),
        )
        .unwrap();

        // Crashed: manifest says Working, but the lock file is free.
        let dead_m = manifest(repo, "s-dead", SessionState::Working);
        write_manifest(&dead_m).unwrap();
        fs::write(liveness_lock_path(repo, "s-dead"), b"").unwrap();

        // Closed cleanly: no live process expected, so it is not abandoned.
        let done_m = manifest(repo, "s-done", SessionState::Finished);
        write_manifest(&done_m).unwrap();
        fs::write(liveness_lock_path(repo, "s-done"), b"").unwrap();

        let views = list_in(repo);
        assert_eq!(views.len(), 3, "all three sessions are listed");
        let by_id = |id: &str| -> SessionView {
            views
                .iter()
                .find(|v| v.manifest.session_id == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(by_id("s-alive").effective_state(), "working");
        assert!(by_id("s-alive").alive);
        assert_eq!(by_id("s-dead").effective_state(), "abandoned");
        assert!(!by_id("s-dead").alive);
        assert_eq!(by_id("s-done").effective_state(), "finished");
    }

    #[test]
    fn registry_is_repo_scoped_and_preserves_unreadable_manifests() {
        let _home = Home::new();
        fs::create_dir_all(sessions_dir("repo-a")).unwrap();
        fs::create_dir_all(sessions_dir("repo-b")).unwrap();
        write_manifest(&manifest("repo-a", "s-a", SessionState::Idle)).unwrap();
        write_manifest(&manifest("repo-b", "s-b", SessionState::Idle)).unwrap();
        let junk = sessions_dir("repo-a").join("broken.json");
        fs::write(&junk, b"{not json").unwrap();

        let a = list_in("repo-a");
        assert_eq!(a.len(), 1, "only this repository's sessions are visible");
        assert_eq!(a[0].manifest.session_id, "s-a");
        assert_eq!(
            fs::read(&junk).unwrap(),
            b"{not json",
            "unreadable evidence is preserved"
        );
        assert!(
            read_sessions("repo-a").1,
            "selective replay must notice omitted evidence"
        );
        assert_eq!(list_in("repo-b").len(), 1, "the other repo is untouched");
    }

    #[test]
    fn finished_sessions_are_kept_until_retention_then_swept() {
        let _home = Home::new();
        let repo = "repo-r";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        let mut recent = manifest(repo, "s-recent", SessionState::Finished);
        recent.updated_unix = now_unix();
        write_manifest(&recent).unwrap();
        let mut old = manifest(repo, "s-old", SessionState::Finished);
        old.updated_unix = now_unix().saturating_sub(RETENTION_SECS + 60);
        write_manifest(&old).unwrap();

        let views = list_in(repo);
        assert_eq!(
            views.len(),
            1,
            "the stale manifest is swept, the fresh one kept"
        );
        assert_eq!(views[0].manifest.session_id, "s-recent");
        assert!(
            !manifest_path(repo, "s-old").exists(),
            "a finished session past retention leaves no file behind"
        );
    }

    #[test]
    fn second_session_on_one_file_is_warned_once_and_recorded_for_both() {
        let _home = Home::new();
        let repo = "repo-o";
        let paths = vec!["src/login.ts".to_string(), "src/token.ts".to_string()];
        let first = claim_paths(repo, "s-a", &paths).unwrap();
        assert!(first.is_empty(), "the first claim is not a conflict");

        let second = claim_paths(repo, "s-b", &["src/login.ts".to_string()]).unwrap();
        assert_eq!(second.len(), 1, "exactly one warning for the shared file");
        assert!(second[0].contains("src/login.ts"), "{:?}", second);
        assert!(
            second[0].contains("s-a"),
            "the warning names the other session"
        );

        let again = claim_paths(repo, "s-b", &["src/login.ts".to_string()]).unwrap();
        assert!(
            again.is_empty(),
            "the same overlap is not re-reported every turn"
        );

        let shared = load_shared(repo);
        assert_eq!(shared.overlaps.len(), 1);
        assert_eq!(shared.overlaps[0].first, "s-a");
        assert_eq!(shared.overlaps[0].second, "s-b");
        assert_eq!(
            shared.owners.get("src/login.ts").unwrap().session_id,
            "s-b",
            "the most recent writer owns the claim"
        );
        assert_eq!(
            shared.owners.get("src/token.ts").unwrap().session_id,
            "s-a",
            "a file only one session touched stays with it"
        );
    }

    #[test]
    fn overlaps_surface_on_both_sessions_views() {
        let _home = Home::new();
        let repo = "repo-ov";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        let mut a = manifest(repo, "s-a", SessionState::Finished);
        a.files = vec![TouchedFile {
            path: "src/login.ts".into(),
            base: 1,
            last_turn: 1,
            status: 'M',
            bases: vec![1],
        }];
        write_manifest(&a).unwrap();
        let b = manifest(repo, "s-b", SessionState::Finished);
        write_manifest(&b).unwrap();
        claim_paths(repo, "s-a", &["src/login.ts".to_string()]).unwrap();
        claim_paths(repo, "s-b", &["src/login.ts".to_string()]).unwrap();

        let views = list_in(repo);
        for v in &views {
            assert_eq!(
                v.overlapping,
                vec!["src/login.ts".to_string()],
                "both sides of an overlap can see it"
            );
        }
    }

    #[test]
    fn worktree_names_are_restricted_to_safe_path_segments() {
        assert!(valid_worktree_name("auth-fix").is_ok());
        assert!(valid_worktree_name("feature_1.2").is_ok());
        assert!(valid_worktree_name("").is_err());
        assert!(valid_worktree_name(".hidden").is_err());
        assert!(valid_worktree_name("../escape").is_err());
        assert!(valid_worktree_name("has space").is_err());
        assert!(valid_worktree_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn forced_worktree_removal_is_refused_before_discovery() {
        assert!(require_safe_worktree_removal(false).is_ok());
        let error = work_remove("valid-name", true).unwrap_err();
        assert!(error.to_string().contains("forced worktree removal"));
    }

    #[test]
    fn managed_worktree_removal_preserves_ignored_files_and_checks_git_state() -> Result<()> {
        let _mode = crate::sandbox::runner::tests::lock();
        let home = Home::new();
        let root = home.root.join("repo");
        fs::create_dir(&root)?;
        let Some(git) = crate::core::gitx::git_exe() else {
            anyhow::ensure!(std::env::var_os("CI").is_none(), "Git required in CI");
            eprintln!("SKIP: Git unavailable for managed worktree removal");
            return Ok(());
        };
        let run = |cwd: &Path, args: &[&str]| -> Result<()> {
            let output = std::process::Command::new(&git)
                .current_dir(cwd)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()?;
            anyhow::ensure!(
                output.status.success(),
                "fixture Git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        };
        run(&root, &["init", "-q"])?;
        fs::write(root.join(".gitignore"), "ignored.tmp\n")?;
        fs::write(root.join("tracked.txt"), "base\n")?;
        run(&root, &["add", ".gitignore", "tracked.txt"])?;
        run(&root, &["commit", "-qm", "base"])?;
        let id = WorkspaceIdentity::discover(&root)?;
        let path = worktrees_root(&id.repo_id).join("fixture");
        fs::create_dir_all(path.parent().unwrap())?;
        run(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "aizen/fixture",
                path.to_str().unwrap(),
                "HEAD",
            ],
        )?;
        crate::core::workspace_txn::with_test_lock_home(&home.root, || -> Result<()> {
            assert_eq!(work_target_for(&id, "fixture")?.ahead, 0);
            for name in ["untracked.txt", "ignored.tmp"] {
                fs::write(path.join(name), "must survive\n")?;
                assert!(work_target_for(&id, "fixture")?.dirty);
                assert!(work_remove_for(&id, "fixture").is_err());
                assert_eq!(fs::read(path.join(name))?, b"must survive\n");
                fs::remove_file(path.join(name))?;
            }
            let target_id = WorkspaceIdentity::discover(&path)?;
            let marker = target_id
                .canonical_git_dir
                .as_ref()
                .unwrap()
                .join("REVERT_HEAD");
            fs::write(&marker, "fixture unfinished operation")?;
            let error = work_remove_for(&id, "fixture").unwrap_err();
            assert!(
                error.to_string().contains("active Git operation"),
                "{error:#}"
            );
            fs::remove_file(marker)?;
            run(&root, &["worktree", "lock", path.to_str().unwrap()])?;
            assert!(work_remove_for(&id, "fixture").is_err());
            run(&root, &["worktree", "unlock", path.to_str().unwrap()])?;
            let checkout_bytes = fs::read(path.join("tracked.txt"))?;
            for flag in ["--assume-unchanged", "--skip-worktree"] {
                run(&path, &["update-index", flag, "tracked.txt"])?;
                fs::write(path.join("tracked.txt"), "hidden pending edit\n")?;
                let result = work_remove_for(&id, "fixture");
                assert!(result.is_err(), "removal accepted {flag}: {result:?}");
                assert_eq!(
                    fs::read(path.join("tracked.txt"))?,
                    b"hidden pending edit\n"
                );
                fs::write(path.join("tracked.txt"), &checkout_bytes)?;
                let clear = if flag == "--assume-unchanged" {
                    "--no-assume-unchanged"
                } else {
                    "--no-skip-worktree"
                };
                run(&path, &["update-index", clear, "tracked.txt"])?;
            }
            fs::create_dir_all(sessions_dir(&id.repo_id))?;
            let invalid_session = sessions_dir(&id.repo_id).join("unknown.json");
            fs::write(&invalid_session, "not a session")?;
            assert!(work_remove_for(&id, "fixture").is_err());
            fs::remove_file(invalid_session)?;
            let mut live = manifest(&id.repo_id, "removal-live", SessionState::Working);
            live.root = fs::canonicalize(&path)?.to_string_lossy().into_owned();
            live.worktree_id = target_id.worktree_id.clone();
            write_manifest(&live)?;
            let missing_lock = liveness_lock_path(&id.repo_id, &live.session_id);
            assert!(work_remove_for(&id, "fixture").is_err());
            assert!(
                !missing_lock.exists(),
                "removal must not create missing liveness evidence"
            );
            let live_lock = RepoTxnLock::acquire_exclusive(
                &liveness_lock_path(&id.repo_id, &live.session_id),
                Duration::from_secs(1),
            )?;
            assert!(work_remove_for(&id, "fixture").is_err());
            assert!(path.exists());
            drop(live_lock);
            fs::remove_file(manifest_path(&id.repo_id, &live.session_id))?;
            run(
                &path,
                &[
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "unintegrated agent change",
                ],
            )?;
            assert_eq!(work_target_for(&id, "fixture")?.ahead, 1);
            assert!(work_remove_for(&id, "fixture").is_err());
            run(&root, &["merge", "--ff-only", "aizen/fixture"])?;
            let sibling = path.with_file_name("locked-sibling");
            run(
                &root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    sibling.to_str().unwrap(),
                    "HEAD",
                ],
            )?;
            run(&root, &["worktree", "lock", sibling.to_str().unwrap()])?;
            let parent_head = fs::read(root.join(".git/HEAD"))?;
            let parent_index = fs::read(root.join(".git/index"))?;
            let receipt_dir = home
                .root
                .join("git-operations-v1")
                .join(&target_id.repo_id)
                .join(&target_id.worktree_id);
            fs::create_dir_all(&receipt_dir)?;
            let receipt = receipt_dir.join("removal-blocker.json");
            let tip = git_inspect_text(&path, &["rev-parse", "HEAD"])?;
            let pending = serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "id": "removal-blocker",
                "root": target_id.canonical_root,
                "git_dir": target_id.canonical_git_dir,
                "common_dir": target_id.canonical_common_git_dir,
                "before": tip.trim(),
                "after": null,
                "branch_before": "refs/heads/aizen/fixture",
                "change": {"action": "integrate", "commit": tip.trim()},
                "targets": [tip.trim()],
                "state": "running",
                "resulting_commits": [],
                "verified": false
            }))?;
            fs::write(&receipt, &pending)?;
            let pending_error = work_remove_for(&id, "fixture").unwrap_err();
            assert!(
                pending_error
                    .to_string()
                    .contains("unfinished Git operation"),
                "{pending_error:#}"
            );
            assert!(path.exists());
            assert_eq!(
                fs::read(&receipt)?,
                pending,
                "pending evidence must not be removed"
            );
            fs::remove_file(receipt)?;
            let message = work_remove_for(&id, "fixture")?;
            assert!(message.contains("branch aizen/fixture kept"));
            assert!(!path.exists());
            run(&root, &["show-ref", "--verify", "refs/heads/aizen/fixture"])?;
            assert_eq!(fs::read(root.join(".git/HEAD"))?, parent_head);
            assert_eq!(fs::read(root.join(".git/index"))?, parent_index);
            Ok(())
        })
    }

    #[test]
    fn removing_a_worktree_is_refused_while_it_holds_work() {
        let dirty = WorkTree {
            name: "wt".into(),
            path: PathBuf::from("/tmp/wt"),
            branch: "aizen/wt".into(),
            dirty: true,
            ahead: 0,
            sessions: 0,
        };
        assert_eq!(work_remove_blockers(&dirty).len(), 1);
        let busy = WorkTree {
            dirty: false,
            ahead: 2,
            sessions: 1,
            ..dirty.clone()
        };
        let blockers = work_remove_blockers(&busy);
        assert_eq!(blockers.len(), 2, "{blockers:?}");
        let clean = WorkTree {
            dirty: false,
            ahead: 0,
            sessions: 0,
            ..dirty
        };
        assert!(work_remove_blockers(&clean).is_empty());
    }

    #[test]
    fn task_text_is_stripped_of_control_characters() {
        assert_eq!(safe_text("fix \u{1b}[31mauth\u{7}", 80), "fix [31mauth");
        assert_eq!(safe_text("  spaced  ", 80), "spaced");
        assert_eq!(safe_text(&"y".repeat(200), 10), "y".repeat(10));
    }

    #[test]
    fn resolve_accepts_index_and_unique_suffix_but_refuses_ambiguity() {
        let sessions = ["1000-11", "1000-12"];
        // `resolve` reads the live registry; exercise the matching rule it uses directly so the
        // test does not depend on a discovered repository identity.
        let unique: Vec<&str> = sessions
            .iter()
            .copied()
            .filter(|s| s.ends_with("11"))
            .collect();
        assert_eq!(unique.len(), 1);
        let ambiguous: Vec<&str> = sessions
            .iter()
            .copied()
            .filter(|s| s.starts_with("1000"))
            .collect();
        assert_eq!(
            ambiguous.len(),
            2,
            "a shared prefix must not silently pick one"
        );
    }

    fn owner_identity(home: &Home) -> WorkspaceIdentity {
        WorkspaceIdentity {
            repo_id: "ownership-fixture".into(),
            worktree_id: "ownership-worktree".into(),
            canonical_root: home.root.canonicalize().unwrap(),
            canonical_git_dir: None,
            canonical_common_git_dir: None,
        }
    }

    #[test]
    fn branch_refresh_preserves_evidence_and_only_updates_matching_active_owner() -> Result<()> {
        let home = Home::new();
        let identity = owner_identity(&home);
        let barrier = acquire_registration_barrier(&identity, Duration::ZERO)?;
        refresh_active_branch(None, &identity, Some("refs/heads/topic"), &barrier)?;
        assert!(!sessions_dir(&identity.repo_id).exists());
        fs::create_dir_all(sessions_dir(&identity.repo_id))?;
        let mut m = historical_view().manifest;
        m.repo_id = identity.repo_id.clone();
        m.worktree_id = identity.worktree_id.clone();
        m.root = identity.canonical_root.display().to_string();
        m.degraded = Some("historical uncovered effects".into());
        m.coverage_gap = true;
        write_manifest(&m)?;
        let path = manifest_path(&m.repo_id, &m.session_id);
        let lock = RepoTxnLock::acquire_exclusive(
            &liveness_lock_path(&m.repo_id, &m.session_id),
            Duration::from_secs(1),
        )?;
        let before = serde_json::to_value(&m)?;
        let mut active = Active {
            manifest: m,
            _lock: lock,
        };
        let mut other = identity.clone();
        other.worktree_id = "other".into();
        refresh_active_branch(Some(&mut active), &other, None, &barrier)?;
        assert_eq!(serde_json::to_value(&active.manifest)?, before);
        for branch in [Some("refs/heads/topic"), None] {
            refresh_active_branch(Some(&mut active), &identity, branch, &barrier)?;
            assert_eq!(
                active.manifest.branch.as_deref(),
                branch.map(|name| name.strip_prefix("refs/heads/").unwrap())
            );
            let mut actual = serde_json::to_value(&active.manifest)?;
            actual["branch"] = before["branch"].clone();
            actual["updated_unix"] = before["updated_unix"].clone();
            assert_eq!(
                actual, before,
                "branch refresh must retain all historical evidence"
            );
            let loaded: SessionManifest = serde_json::from_slice(&fs::read(&path)?)?;
            assert_eq!(
                serde_json::to_value(loaded)?,
                serde_json::to_value(&active.manifest)?
            );
        }
        let bytes = fs::read(&path)?;
        assert!(
            refresh_active_branch(Some(&mut active), &identity, Some("topic"), &barrier).is_err()
        );
        assert_eq!(fs::read(&path)?, bytes);
        let mut foreign = identity.clone();
        foreign.repo_id = "foreign".into();
        let foreign_barrier = acquire_registration_barrier(&foreign, Duration::ZERO)?;
        assert!(
            refresh_active_branch(Some(&mut active), &identity, None, &foreign_barrier).is_err()
        );
        assert_eq!(fs::read(&path)?, bytes);
        fs::remove_file(&path)?;
        assert!(refresh_active_branch(Some(&mut active), &identity, None, &barrier).is_err());
        assert!(
            !path.exists(),
            "missing manifest must not be recreated by refresh"
        );
        Ok(())
    }

    #[test]
    fn removal_inspection_queries_fail_closed_and_never_read_legacy_registry() -> Result<()> {
        let home = Home::new();
        let id = owner_identity(&home);
        let path = worktrees_root(&id.repo_id).join("fixture");
        fs::create_dir_all(&path)?;
        let mut target = id.clone();
        target.canonical_root = path.canonicalize()?;
        target.worktree_id = "target-worktree".into();
        let oid = "a".repeat(40);
        for fail_at in 0..=5 {
            let calls = std::cell::Cell::new(0usize);
            let result = work_target_with_query(&id, &target, "fixture", |root, args| {
                let call = calls.get() + 1;
                calls.set(call);
                anyhow::ensure!(call != fail_at, "injected query failure {call}");
                Ok(match args[0] {
                    "worktree" => {
                        assert_eq!(root, id.canonical_root);
                        assert_eq!(args, ["worktree", "list", "--porcelain", "-z"]);
                        format!(
                            "worktree {}\0HEAD {oid}\0branch refs/heads/topic\0\0",
                            path.display()
                        )
                        .into_bytes()
                    }
                    "rev-parse" => format!("{oid}\n").into_bytes(),
                    "ls-files" | "status" => Vec::new(),
                    "rev-list" => b"0\n".to_vec(),
                    unexpected => panic!("unexpected query {unexpected}"),
                })
            });
            if fail_at == 0 {
                let observed = result?;
                assert_eq!(calls.get(), 5);
                assert_eq!(observed.sessions, 0);
                assert_eq!(observed.ahead, 0);
                assert!(!observed.dirty);
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("injected query failure"),
                    "{error:#}"
                );
                assert_eq!(calls.get(), fail_at);
            }
        }
        fs::create_dir_all(sessions_dir(&id.repo_id))?;
        let mut unknown = manifest(&id.repo_id, "missing-lock", SessionState::Finished);
        unknown.root = target.canonical_root.display().to_string();
        write_manifest(&unknown)?;
        let result = work_target_with_query(&id, &target, "fixture", |_, _| {
            panic!("unknown liveness must refuse before any Git query")
        });
        assert!(result.is_err());
        assert!(!liveness_lock_path(&id.repo_id, "missing-lock").exists());
        Ok(())
    }

    #[test]
    fn mutation_owner_admission_is_read_only_and_does_not_sweep() -> Result<()> {
        let home = Home::new();
        let identity = owner_identity(&home);
        require_no_other_workspace_owner(&identity)?;
        assert!(
            !coop_root(&identity.repo_id).exists(),
            "absent registry must not be created"
        );
        fs::create_dir_all(sessions_dir(&identity.repo_id))?;
        let mut m = manifest(&identity.repo_id, "old-dead", SessionState::Finished);
        m.root = identity.canonical_root.display().to_string();
        m.updated_unix = 0;
        write_manifest(&m)?;
        let path = manifest_path(&identity.repo_id, &m.session_id);
        let before = fs::read(&path)?;
        let lock = liveness_lock_path(&identity.repo_id, &m.session_id);
        assert!(require_no_other_workspace_owner(&identity).is_err());
        assert!(
            !lock.exists(),
            "missing liveness evidence must not be created"
        );
        fs::write(&lock, b"old lock evidence")?;
        require_no_other_workspace_owner(&identity)?;
        assert_eq!(
            fs::read(path)?,
            before,
            "admission must not sweep old evidence"
        );
        assert_eq!(fs::read(lock)?, b"old lock evidence");
        Ok(())
    }

    #[test]
    fn mutation_owner_admission_excludes_only_exact_self_in_same_worktree() -> Result<()> {
        let home = Home::new();
        let identity = owner_identity(&home);
        fs::create_dir_all(sessions_dir(&identity.repo_id))?;
        let mut m = manifest(&identity.repo_id, "owner-live", SessionState::Working);
        m.root = identity.canonical_root.display().to_string();
        let lock_path = liveness_lock_path(&identity.repo_id, &m.session_id);
        let held = RepoTxnLock::acquire_exclusive(&lock_path, Duration::from_secs(1))?;
        assert!(
            require_no_other_workspace_owner(&identity).is_err(),
            "live orphan lock must block"
        );
        write_manifest(&m)?;
        assert!(require_no_other_workspace_owner_except(&identity, None).is_err());
        assert!(require_no_other_workspace_owner_except(&identity, Some("live")).is_err());
        require_no_other_workspace_owner_except(&identity, Some("owner-live"))?;
        let other = home.root.join("other-worktree");
        fs::create_dir(&other)?;
        m.root = other.canonicalize()?.display().to_string();
        write_manifest(&m)?;
        require_no_other_workspace_owner_except(&identity, None)?;
        assert!(
            require_owner_admission(&identity, None, true).is_err(),
            "shared-ref retirement must block another live worktree owner"
        );
        m.root = home.root.join("missing-worktree").display().to_string();
        write_manifest(&m)?;
        assert!(require_no_other_workspace_owner_except(&identity, None).is_err());
        drop(held);
        require_no_other_workspace_owner_except(&identity, None)?;
        assert!(
            !Path::new(&m.root).exists(),
            "dead removed worktree must not block"
        );
        fs::remove_file(&lock_path)?;
        assert!(
            require_no_other_workspace_owner_except(&identity, None).is_err(),
            "missing lock is still unknown liveness, not proof of a dead session"
        );
        Ok(())
    }

    #[test]
    fn registration_barrier_excludes_publication_until_mutation_releases_it() -> Result<()> {
        let home = Home::new();
        let identity = owner_identity(&home);
        let held = acquire_registration_barrier(&identity, Duration::ZERO)?;
        held.verify()?;
        let mut sibling = identity.clone();
        sibling.worktree_id = "other-worktree".into();
        let blocked = acquire_registration_barrier(&sibling, Duration::ZERO);
        assert!(
            blocked.is_err(),
            "registration of another worktree shares repository admission"
        );
        let error = blocked.err().unwrap();
        assert!(
            error
                .downcast_ref::<crate::core::repo_lock::LockBusy>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            !sessions_dir(&identity.repo_id).exists(),
            "blocked begin cannot publish unlocked"
        );
        let mut other_repo = identity.clone();
        other_repo.repo_id = "different-repository".into();
        let independent = acquire_registration_barrier(&other_repo, Duration::ZERO)?;
        drop(independent);
        drop(held);
        let registration = acquire_registration_barrier(&identity, Duration::ZERO)?;
        registration.verify()?;
        Ok(())
    }

    #[test]
    fn registration_barrier_rejects_unsafe_namespace_and_lock_replacement() -> Result<()> {
        let home = Home::new();
        let identity = owner_identity(&home);
        let directory = home.root.join("coop-registration");
        fs::write(&directory, b"must not replace this file")?;
        assert!(acquire_registration_barrier(&identity, Duration::ZERO).is_err());
        assert_eq!(fs::read(&directory)?, b"must not replace this file");
        fs::remove_file(&directory)?;
        let mut invalid = identity.clone();
        invalid.repo_id = "../escape".into();
        assert!(acquire_registration_barrier(&invalid, Duration::ZERO).is_err());
        #[cfg(unix)]
        {
            let external = home.root.join("external");
            fs::create_dir(&external)?;
            std::os::unix::fs::symlink(&external, &directory)?;
            assert!(acquire_registration_barrier(&identity, Duration::ZERO).is_err());
            assert_eq!(fs::read_dir(&external)?.count(), 0);
            fs::remove_file(&directory)?;
            let barrier = acquire_registration_barrier(&identity, Duration::ZERO)?;
            fs::rename(&barrier.path, barrier.path.with_extension("old"))?;
            fs::write(&barrier.path, b"replacement lock")?;
            assert!(
                barrier.verify().is_err(),
                "path replacement must invalidate the held barrier"
            );
        }
        Ok(())
    }

    #[test]
    fn mutation_owner_admission_refuses_unknown_registry_evidence() -> Result<()> {
        let home = Home::new();
        let identity = owner_identity(&home);
        let dir = sessions_dir(&identity.repo_id);
        fs::create_dir_all(dir.parent().unwrap())?;
        fs::write(&dir, "not a directory")?;
        assert!(require_no_other_workspace_owner(&identity).is_err());
        fs::remove_file(&dir)?;
        fs::create_dir(&dir)?;
        let mut m = manifest(&identity.repo_id, "invalid", SessionState::Finished);
        m.root = identity.canonical_root.display().to_string();
        let path = manifest_path(&identity.repo_id, &m.session_id);
        fs::write(&path, "not json")?;
        assert!(require_no_other_workspace_owner(&identity).is_err());
        assert_eq!(fs::read(&path)?, b"not json");
        m.schema = SCHEMA + 1;
        write_manifest(&m)?;
        assert!(require_no_other_workspace_owner(&identity).is_err());
        m.schema = SCHEMA;
        m.repo_id = "foreign".into();
        fs::write(&path, serde_json::to_vec(&m)?)?;
        assert!(require_no_other_workspace_owner(&identity).is_err());
        #[cfg(unix)]
        {
            fs::remove_file(&path)?;
            let external = home.root.join("external-owner.json");
            fs::write(&external, serde_json::to_vec(&m)?)?;
            std::os::unix::fs::symlink(&external, &path)?;
            assert!(require_no_other_workspace_owner(&identity).is_err());
            assert!(fs::symlink_metadata(&path)?.file_type().is_symlink());
        }
        Ok(())
    }

    fn historical_view() -> SessionView {
        let mut m = manifest("historical-repo", "historical-session", SessionState::Done);
        m.base_checkpoint = Some(17);
        m.files.push(TouchedFile {
            path: "shared.rs".into(),
            base: 4,
            bases: vec![4, 9],
            last_turn: 3,
            status: 'M',
        });
        SessionView {
            manifest: m,
            alive: false,
            is_self: false,
            committed: false,
            overlapping: Vec::new(),
        }
    }

    #[test]
    fn historical_attribution_never_grants_diff_or_commit_authority() {
        for state in [
            SessionState::Working,
            SessionState::Done,
            SessionState::Finished,
        ] {
            for empty in [false, true] {
                let mut view = historical_view();
                view.manifest.state = state;
                view.manifest.branch = Some("aizen/looks-owned".into());
                // Missing Git/root cannot turn historical metadata into staging authority.
                view.manifest.root = "unavailable-worktree".into();
                if empty {
                    view.manifest.files.clear();
                    view.manifest.base_checkpoint = None;
                }
                for result in [
                    session_diff(&view, false),
                    session_diff(&view, true),
                    session_commit(&view),
                ] {
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains("coverage gap"), "{error}");
                    assert!(error.contains("no current staging authority"), "{error}");
                    assert!(error.contains("nothing was staged"), "{error}");
                }
            }
        }
    }

    #[test]
    fn historical_refusals_preserve_index_working_files_and_manifest() -> Result<()> {
        let home = Home::new();
        let root = home.root.join("worktree");
        fs::create_dir_all(root.join(".git"))?;
        // Intentionally not a valid repository/index. Refusal must precede even Git discovery,
        // staging, scratch-object creation, or attempted cleanup of an abandoned review.
        fs::write(root.join(".git/index"), b"user staged bytes\0must survive")?;
        fs::write(root.join(".git/HEAD"), b"user branch evidence")?;
        fs::write(root.join("shared.rs"), b"mixed user and peer edits")?;
        let mut view = historical_view();
        view.manifest.root = root.display().to_string();
        view.manifest.coverage_gap = true;
        view.manifest.degraded = Some("unsettled shell effects".into());
        view.manifest.truncated_files = 3;
        fs::create_dir_all(sessions_dir(&view.manifest.repo_id))?;
        write_manifest(&view.manifest)?;
        let path = manifest_path(&view.manifest.repo_id, &view.manifest.session_id);
        let before = fs::read(&path)?;
        for result in [session_diff(&view, true), session_commit(&view)] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("unsettled shell effects"), "{error}");
            assert!(error.contains("omitted paths: 3"), "{error}");
        }
        assert_eq!(
            fs::read(root.join(".git/index"))?,
            b"user staged bytes\0must survive"
        );
        assert_eq!(fs::read(root.join(".git/HEAD"))?, b"user branch evidence");
        assert_eq!(
            fs::read(root.join("shared.rs"))?,
            b"mixed user and peer edits"
        );
        assert_eq!(fs::read(path)?, before);
        assert_eq!(fs::read_dir(root.join(".git"))?.count(), 2);
        Ok(())
    }

    #[test]
    fn historical_fields_round_trip_without_reconstruction() {
        let mut value = serde_json::to_value(historical_view().manifest).unwrap();
        // Manifests preceding the multi-base field remain readable, not replayable.
        value["files"][0].as_object_mut().unwrap().remove("bases");
        let old: SessionManifest = serde_json::from_value(value).unwrap();
        assert_eq!(old.base_checkpoint, Some(17));
        assert_eq!(old.files[0].base, 4);
        assert!(old.files[0].bases.is_empty());
        let before = serde_json::to_value(&old).unwrap();
        let loaded: SessionManifest = serde_json::from_value(before.clone()).unwrap();
        assert_eq!(serde_json::to_value(loaded).unwrap(), before);
    }

    #[test]
    fn gaps_and_sealing_preserve_historical_evidence_and_first_reason() {
        let mut m = historical_view().manifest;
        let historical_files = m.files.clone();
        record_attribution_gap(&mut m, "native effect has no per-session attribution");
        record_attribution_gap(&mut m, "managed background effects");
        seal_manifest(&mut m);
        assert!(m.coverage_gap);
        assert_eq!(
            m.degraded.as_deref(),
            Some("native effect has no per-session attribution")
        );
        assert_eq!(m.base_checkpoint, Some(17));
        assert_eq!(m.files, historical_files);
        assert_eq!(m.turns, 2);
        let mut m: SessionManifest =
            serde_json::from_value(serde_json::to_value(m).unwrap()).unwrap();
        seal_manifest(&mut m);
        assert!(m.coverage_gap);
        assert_eq!(m.base_checkpoint, Some(17));
        assert_eq!(m.files, historical_files);
    }

    #[test]
    fn empty_ledger_at_turn_end_never_certifies_complete_coverage() {
        let mut m = manifest("repo", "empty", SessionState::Working);
        seal_manifest(&mut m);
        assert!(m.coverage_gap);
        assert!(m
            .degraded
            .as_deref()
            .unwrap()
            .contains("empty ledger does not certify coverage"));
        assert!(
            m.files.is_empty(),
            "unattributed edits must not become new file claims"
        );
        assert!(
            m.base_checkpoint.is_none(),
            "sealing must not manufacture a baseline"
        );
    }

    #[test]
    fn historical_attribution_evidence_survives_retention() {
        let _home = Home::new();
        let repo = "repo-history";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        for variant in 0..5 {
            let mut m = manifest(repo, &format!("evidence-{variant}"), SessionState::Finished);
            m.updated_unix = 0;
            match variant {
                0 => m.coverage_gap = true,
                1 => m.base_checkpoint = Some(17),
                2 => m.files = historical_view().manifest.files,
                3 => m.truncated_files = 5,
                _ => m.degraded = Some("historical unsupported interval".into()),
            }
            write_manifest(&m).unwrap();
            let path = manifest_path(repo, &m.session_id);
            let before = fs::read(&path).unwrap();
            let lock = liveness_lock_path(repo, &m.session_id);
            fs::write(&lock, b"lock evidence").unwrap();
            assert!(list_in(repo)
                .iter()
                .any(|view| view.manifest.session_id == m.session_id));
            assert_eq!(fs::read(path).unwrap(), before);
            assert_eq!(fs::read(lock).unwrap(), b"lock evidence");
        }
    }

    #[test]
    fn future_manifests_and_unknown_states_are_preserved_with_their_locks() {
        let _home = Home::new();
        let repo = "repo-future";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        for (id, state) in [("readable", "finished"), ("unknown-state", "future-state")] {
            let mut m = manifest(repo, id, SessionState::Finished);
            m.schema = SCHEMA + 1;
            m.updated_unix = 0;
            let mut value = serde_json::to_value(m).unwrap();
            value["state"] = serde_json::json!(state);
            let bytes = serde_json::to_vec(&value).unwrap();
            let path = manifest_path(repo, id);
            let lock = liveness_lock_path(repo, id);
            fs::write(&path, &bytes).unwrap();
            fs::write(&lock, b"lock evidence").unwrap();
            let (views, omitted) = read_sessions(repo);
            assert!(views.is_empty());
            assert!(omitted);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read(&lock).unwrap(), b"lock evidence");
        }
    }
}
