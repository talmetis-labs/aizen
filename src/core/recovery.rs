//! Crash-aware recovery lease + transactional sidecars.
//!
//! The manifest is the commit record. History/draft sidecars are generation-specific and owner-only;
//! a new manifest is published only after every sidecar is durable. Recovery never replays a model or
//! tool call: it restores the last safe conversation boundary and places the interrupted request back
//! in the input box for review.

use crate::agent::design::TaskDesignState;
use crate::agent::todo::TaskState;
use crate::core::persist::{atomic_write_owner_only, remove_if_exists};
use crate::core::session_control::SessionControl;
use crate::core::session_store::{PersistenceSnapshot, SaveControl};
use crate::core::snapshots::SnapshotRef;
use crate::core::types::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LEGACY_SCHEMA: u32 = 2;
const SCHEMA: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Idle,
    WaitingModel,
    ExecutingTools,
    AwaitingApproval,
    Finalizing,
}

impl RecoveryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::WaitingModel => "waiting_model",
            Self::ExecutingTools => "executing_tools",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Finalizing => "finalizing",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryManifest {
    pub schema: u32,
    pub binary_version: String,
    pub pid: u32,
    pub run_id: String,
    /// Stable project label for diagnostics.
    pub repo: String,
    /// Exact canonical repository/worktree scope used to filter offers.
    pub repo_scope: String,
    pub phase: RecoveryPhase,
    pub session_name: Option<String>,
    pub safe_history_len: usize,
    /// Schema-2 lookup evidence only. Never reinterpret this as a Git task or OID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<u32>,
    /// Schema-3 backend-qualified provenance, not a live pin or restore capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<SnapshotRef>,
    pub sidecar_generation: u64,
    /// State is in the same generation's history envelope, never an unrelated active singleton.
    #[serde(default)]
    pub has_design_state: bool,
    #[serde(default)]
    pub has_task_state: bool,
    pub turn_id: Option<u64>,
    /// Only a completed coordinating loop can clear this; autosave/Idle is not settlement proof.
    pub side_effects_possible: bool,
    /// Sticky across later turns and clean exit until explicit reconciliation; observer return is
    /// not proof that a worker stopped or that a possibly-applied mutation has been reconciled.
    #[serde(default)]
    pub effects_unsettled: bool,
    pub updated_unix: u64,
}

impl RecoveryManifest {
    fn validate_checkpoint_schema(&self) -> Result<()> {
        match self.schema {
            LEGACY_SCHEMA => anyhow::ensure!(
                self.checkpoint_ref.is_none(),
                "schema-2 recovery cannot contain a typed snapshot reference"
            ),
            SCHEMA => anyhow::ensure!(
                self.checkpoint_id.is_none(),
                "schema-3 recovery cannot contain an unbound numeric checkpoint"
            ),
            _ => anyhow::bail!("unsupported recovery schema"),
        }
        if let Some(reference) = &self.checkpoint_ref {
            reference.validate()?;
        }
        Ok(())
    }

    pub(crate) fn checkpoint_label(&self) -> Option<String> {
        self.checkpoint_ref
            .as_ref()
            .map(SnapshotRef::display_label)
            .or_else(|| {
                self.checkpoint_id
                    .map(|id| format!("legacy checkpoint #{id}"))
            })
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryOffer {
    pub path: PathBuf,
    pub manifest: RecoveryManifest,
    pub history: Vec<Message>,
    #[expect(
        dead_code,
        reason = "recovery preview retains design state; acceptance reloads the authoritative generation"
    )]
    pub design_state: Option<TaskDesignState>,
    pub pending_draft: Option<String>,
}

static ACTIVE: Mutex<Option<ActiveLease>> = Mutex::new(None);
// Focus never destroys old conversation generations or releases uncertain recovery evidence.
static FOCUSED_LEASES: Mutex<Vec<ActiveLease>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn clear_focus_test_state() {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    FOCUSED_LEASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

#[cfg(test)]
pub(crate) fn focus_test_scope() -> Option<String> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|active| active.manifest.repo_scope.clone())
}

#[cfg(test)]
pub(crate) fn focus_test_history_len() -> usize {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |active| active.manifest.safe_history_len)
}

pub(crate) struct PreparedRecoveryScope {
    scope: String,
    fresh: Option<ActiveLease>,
}

impl PreparedRecoveryScope {
    pub(crate) fn revalidate(&self) -> Result<()> {
        let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = active.as_ref() {
            require_focus_settled(active)?;
        }
        let retained = FOCUSED_LEASES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = retained
            .iter()
            .find(|lease| lease.manifest.repo_scope == self.scope)
        {
            require_focus_settled(existing)?;
        } else if let Some(fresh) = &self.fresh {
            require_focus_settled(fresh)?;
        }
        Ok(())
    }

    pub(crate) fn save_conversation(
        &mut self,
        history: &[Message],
        design: Option<&TaskDesignState>,
        task: &TaskState,
        snapshot: &PersistenceSnapshot,
    ) -> Result<()> {
        if let Some(fresh) = self.fresh.as_mut() {
            return checkpoint_active(
                fresh,
                history,
                None,
                RecoveryPhase::Idle,
                design,
                task,
                SaveControl::Snapshot(snapshot),
            );
        }
        let mut retained = FOCUSED_LEASES.lock().unwrap_or_else(|e| e.into_inner());
        let target = retained
            .iter_mut()
            .find(|lease| lease.manifest.repo_scope == self.scope)
            .context("prepared recovery target is missing")?;
        checkpoint_active(
            target,
            history,
            None,
            RecoveryPhase::Idle,
            design,
            task,
            SaveControl::Snapshot(snapshot),
        )
    }

    pub(crate) fn activate(mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let mut retained = FOCUSED_LEASES.lock().unwrap_or_else(|e| e.into_inner());
        let next = retained
            .iter()
            .position(|lease| lease.manifest.repo_scope == self.scope)
            .map(|index| retained.swap_remove(index))
            .or_else(|| self.fresh.take());
        if let Some(next) = next {
            if let Some(old) = active.replace(next) {
                retained.push(old);
            }
        }
    }
}

pub(crate) fn prepare_focus_scope(
    scope: String,
    session_name: Option<String>,
) -> Result<PreparedRecoveryScope> {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = active.as_ref() {
        require_focus_settled(active)?;
    }
    let retained = FOCUSED_LEASES.lock().unwrap_or_else(|e| e.into_inner());
    let existing = retained
        .iter()
        .find(|lease| lease.manifest.repo_scope == scope);
    anyhow::ensure!(
        existing.is_some() || retained.len() < 64,
        "too many retained recovery scopes; open a separate session"
    );
    if let Some(existing) = existing {
        require_focus_settled(existing)?;
    }
    let fresh = if existing.is_none() {
        Some(create_lease(scope.clone(), session_name)?)
    } else {
        None
    };
    Ok(PreparedRecoveryScope { scope, fresh })
}

fn require_focus_settled(active: &ActiveLease) -> Result<()> {
    anyhow::ensure!(
        !active.manifest.effects_unsettled && !active.manifest.side_effects_possible,
        "workspace focus blocked by unresolved recovery effects"
    );
    active._lock.verify_existing_path(&lock_path(&active.dir))?;
    let disk = crate::core::file_placement::read_existing_regular(
        &manifest_path(&active.dir),
        128 * 1024,
    )?;
    let stored: RecoveryManifest = serde_json::from_slice(&disk)?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&active.manifest)?,
        "recovery scope changed outside its owner; focus refused"
    );
    Ok(())
}

struct ScopedLease {
    active: std::sync::MutexGuard<'static, Option<ActiveLease>>,
    retained: std::sync::MutexGuard<'static, Vec<ActiveLease>>,
    root: Option<String>,
}
impl ScopedLease {
    fn active_mut(&mut self) -> Option<&mut ActiveLease> {
        let matches = self.active.as_ref().is_some_and(|lease| {
            self.root.as_ref().is_none_or(|root| {
                crate::core::config::normalize_path_key(Path::new(&lease.manifest.repo_scope))
                    == *root
            })
        });
        if matches || self.root.is_none() {
            return self.active.as_mut();
        }
        self.retained.iter_mut().find(|lease| {
            self.root.as_ref().is_some_and(|root| {
                crate::core::config::normalize_path_key(Path::new(&lease.manifest.repo_scope))
                    == *root
            })
        })
    }
}
fn scoped_active() -> ScopedLease {
    let root = crate::core::exec_ctx::current().and_then(|ctx| {
        ctx.placement()
            .map(|placement| crate::core::config::normalize_path_key(placement.project_root()))
    });
    ScopedLease {
        active: ACTIVE.lock().unwrap_or_else(|e| e.into_inner()),
        retained: FOCUSED_LEASES.lock().unwrap_or_else(|e| e.into_inner()),
        root,
    }
}

struct ActiveLease {
    dir: PathBuf,
    manifest: RecoveryManifest,
    _lock: crate::core::repo_lock::RepoTxnLock,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn current_repo_scope() -> String {
    let root = crate::core::config::project_root();
    fs::canonicalize(&root)
        .unwrap_or(root)
        .display()
        .to_string()
}

fn recovery_root() -> PathBuf {
    crate::core::config::aizen_home().join("recovery")
}

fn lease_dir(run_id: &str) -> PathBuf {
    recovery_root().join(run_id)
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("lease.lock")
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn history_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("history.{generation}.json"))
}

fn draft_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("draft.{generation}.txt"))
}

fn new_run_id() -> String {
    format!("{}-{}", now_unix(), std::process::id())
}

/// Begin a recovery lease for the current process. Best-effort: failure never blocks the REPL.
pub fn begin(repo_scope: impl Into<String>, session_name: Option<String>) {
    if let Ok(lease) = create_lease(repo_scope.into(), session_name) {
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(lease);
    }
}

fn create_lease(repo_scope: String, session_name: Option<String>) -> Result<ActiveLease> {
    let run_id = format!(
        "{}-{}",
        new_run_id(),
        crate::core::persist::unique_sequence()
    );
    let dir = lease_dir(&run_id);
    fs::create_dir_all(&dir)?;
    crate::core::config::harden_dir(&dir);
    let lock =
        crate::core::repo_lock::RepoTxnLock::acquire(&lock_path(&dir), Duration::from_secs(1))?;
    let manifest = RecoveryManifest {
        schema: SCHEMA,
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        run_id,
        repo: crate::core::config::project_slug(),
        repo_scope,
        phase: RecoveryPhase::Idle,
        session_name,
        safe_history_len: 0,
        checkpoint_id: None,
        checkpoint_ref: None,
        sidecar_generation: 0,
        has_design_state: false,
        has_task_state: false,
        turn_id: None,
        side_effects_possible: false,
        effects_unsettled: false,
        updated_unix: now_unix(),
    };
    write_manifest(&dir, &manifest)?;
    Ok(ActiveLease {
        dir,
        manifest,
        _lock: lock,
    })
}

fn write_manifest(dir: &Path, manifest: &RecoveryManifest) -> Result<()> {
    manifest.validate_checkpoint_schema()?;
    let bytes = serde_json::to_vec_pretty(manifest)?;
    atomic_write_owner_only(&manifest_path(dir), &bytes)
}

#[derive(Serialize, Deserialize)]
struct HistoryGeneration {
    #[serde(default)]
    version: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::core::session_store::deserialize_session_control"
    )]
    session_control: Option<SessionControl>,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    design_state: Option<TaskDesignState>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::core::session_store::deserialize_task_state"
    )]
    task_state: Option<TaskState>,
    messages: Vec<serde_json::Value>,
}

/// Persist safe conversation, optional draft and design state as one committed generation.
/// Sidecars are written first and the manifest last; incomplete sidecars are removed on failure.
pub fn checkpoint_history_with_design(
    history: &[Message],
    pending_draft: Option<&str>,
    phase: RecoveryPhase,
    design: Option<&TaskDesignState>,
) -> Result<()> {
    let task = crate::agent::todo::durable_snapshot();
    checkpoint_history_with_state(history, pending_draft, phase, design, &task)
}

pub fn checkpoint_history_with_state(
    history: &[Message],
    pending_draft: Option<&str>,
    phase: RecoveryPhase,
    design: Option<&TaskDesignState>,
    task: &TaskState,
) -> Result<()> {
    let snapshot = PersistenceSnapshot::capture(task)?;
    checkpoint_history_with_policy(
        history,
        pending_draft,
        phase,
        design,
        task,
        SaveControl::Snapshot(&snapshot),
    )
}

/// Trusted ingress may persist while holding the control transition mutex; do not reenter it.
#[cfg(test)]
pub(crate) fn checkpoint_history_with_control(
    history: &[Message],
    pending_draft: Option<&str>,
    phase: RecoveryPhase,
    design: Option<&TaskDesignState>,
    task: &TaskState,
    control: &SessionControl,
) -> Result<()> {
    checkpoint_history_with_policy(
        history,
        pending_draft,
        phase,
        design,
        task,
        SaveControl::Explicit(control),
    )
}

pub(crate) fn checkpoint_history_with_policy(
    history: &[Message],
    pending_draft: Option<&str>,
    phase: RecoveryPhase,
    design: Option<&TaskDesignState>,
    task: &TaskState,
    policy: SaveControl<'_>,
) -> Result<()> {
    policy.check(task)?;
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return Ok(());
    };
    checkpoint_active(active, history, pending_draft, phase, design, task, policy)
}

#[allow(clippy::too_many_arguments)]
fn checkpoint_active(
    active: &mut ActiveLease,
    history: &[Message],
    pending_draft: Option<&str>,
    phase: RecoveryPhase,
    design: Option<&TaskDesignState>,
    task: &TaskState,
    policy: SaveControl<'_>,
) -> Result<()> {
    let old = active.manifest.clone();
    // Never turn an unreadable/future committed generation into an apparently normal checkpoint.
    let existing_control = if old.sidecar_generation > 0 {
        Some(read_authoritative_snapshot(&active.dir, &old)?.3)
    } else {
        None
    };
    let control = policy.resolve(existing_control.as_ref())?;
    let generation = old
        .sidecar_generation
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("recovery generation exhausted"))?;
    let hist_path = history_path(&active.dir, generation);
    let draft_path_new = draft_path(&active.dir, generation);
    let result = (|| -> Result<()> {
        let stored = HistoryGeneration {
            version: 1,
            session_control: Some(control.clone()),
            generation,
            design_state: design.cloned(),
            task_state: Some(task.clone()),
            messages: crate::core::session_store::stored_history(history)?,
        };
        policy.check(task)?;
        atomic_write_owner_only(&hist_path, &serde_json::to_vec(&stored)?)?;
        if let Some(draft) = pending_draft {
            atomic_write_owner_only(&draft_path_new, draft.as_bytes())?;
        }
        let mut next = old.clone();
        // Saving a transcript does not settle effects. Only TurnBoundary can close a proven turn.
        next.phase = if phase == RecoveryPhase::Idle
            && (old.side_effects_possible || old.effects_unsettled)
        {
            RecoveryPhase::Finalizing
        } else {
            phase
        };
        next.safe_history_len = history.len();
        next.sidecar_generation = generation;
        next.has_design_state = design.is_some();
        next.has_task_state = true;
        if !old.effects_unsettled {
            next.turn_id = pending_draft.map(|_| generation).or(old.turn_id);
        }
        next.updated_unix = now_unix();
        policy.check(task)?;
        write_manifest(&active.dir, &next)?;
        active.manifest = next;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&hist_path);
        let _ = fs::remove_file(&draft_path_new);
        return Err(e);
    }
    if old.sidecar_generation > 0 {
        let _ = remove_if_exists(&history_path(&active.dir, old.sidecar_generation));
        let _ = remove_if_exists(&draft_path(&active.dir, old.sidecar_generation));
    }
    Ok(())
}

/// The REPL owns one boundary per coordinating loop, never one per child. Dropping a turn future
/// with pending effects cannot be mistaken for successful settlement by a later autosave.
pub(crate) struct TurnBoundary {
    completed: bool,
}

impl TurnBoundary {
    pub(crate) fn new() -> Self {
        Self { completed: false }
    }

    pub(crate) fn complete(&mut self, settled: bool) {
        record_turn_completion(settled);
        self.completed = true;
    }
}

impl Drop for TurnBoundary {
    fn drop(&mut self) {
        if !self.completed {
            record_turn_completion(false);
        }
    }
}

fn record_turn_completion(settled: bool) {
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return;
    };
    if !settled
        && !active.manifest.side_effects_possible
        && active.manifest.phase != RecoveryPhase::ExecutingTools
    {
        return;
    }
    let old = active.manifest.clone();
    if settled && !active.manifest.effects_unsettled {
        active.manifest.side_effects_possible = false;
        active.manifest.checkpoint_id = None;
        active.manifest.checkpoint_ref = None;
        active.manifest.turn_id = None;
    } else {
        active.manifest.effects_unsettled = true;
        active.manifest.side_effects_possible = true;
    }
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() && settled {
        // A failed settlement commit must leave the old durable warning intact. Failed uncertainty
        // commits retain the in-memory latch so the next autosave retries rather than clearing it.
        active.manifest = old;
    }
}

pub(crate) fn mark_effects_unsettled() {
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return;
    };
    active.manifest.effects_unsettled = true;
    active.manifest.side_effects_possible = true;
    active.manifest.updated_unix = now_unix();
    let _ = write_manifest(&active.dir, &active.manifest);
}

pub fn set_phase(phase: RecoveryPhase) {
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return;
    };
    let old = active.manifest.clone();
    active.manifest.phase = if phase == RecoveryPhase::Idle
        && (active.manifest.side_effects_possible || active.manifest.effects_unsettled)
    {
        RecoveryPhase::Finalizing
    } else {
        phase
    };
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

pub fn mark_side_effects_possible() {
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return;
    };
    if active.manifest.side_effects_possible {
        return;
    }
    let old = active.manifest.clone();
    active.manifest.side_effects_possible = true;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

pub fn set_checkpoint(reference: Option<SnapshotRef>) {
    // This API records evidence, never opens Git or upgrades persisted data into authority.
    if reference
        .as_ref()
        .is_some_and(|value| value.validate().is_err())
    {
        return;
    }
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return;
    };
    if active.manifest.effects_unsettled {
        return;
    }
    let old = active.manifest.clone();
    active.manifest.schema = SCHEMA;
    active.manifest.checkpoint_id = None;
    active.manifest.checkpoint_ref = reference;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

pub fn set_session_name(name: Option<String>) {
    let mut guard = scoped_active();
    let Some(active) = guard.active_mut() else {
        return;
    };
    let old = active.manifest.clone();
    active.manifest.session_name = name;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

/// Clean exit retires only proven leases; uncertain effects remain available for inspection and keep
/// their lease lock until this process actually exits.
pub fn clear() {
    // Navigation-retained generations are evidence, not cleanup candidates. Release only their
    // proven-settled locks on clean exit; uncertain ones stay charged until process termination.
    FOCUSED_LEASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|lease| lease.manifest.effects_unsettled || lease.manifest.side_effects_possible);
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else {
        return;
    };
    if active.manifest.effects_unsettled || active.manifest.side_effects_possible {
        return;
    }
    let active = guard.take();
    drop(guard);
    if let Some(active) = active {
        let dir = active.dir.clone();
        drop(active); // Windows cannot delete a directory while its lease handle is open.
        let _ = fs::remove_dir_all(dir);
    }
}

/// How long an abandoned lease directory is kept before it is swept. A lease older than this cannot
/// be a useful offer: its conversation is already in the session pool (autosave writes every turn),
/// and nobody returns to a week-old interrupted turn.
const LEASE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Delete lease directories nobody will ever be offered again.
///
/// [`clear`] only runs on a CLEAN exit and [`scan_stale`] only ever LOOKS at leases whose
/// `repo_scope` matches the current one — so every abrupt shutdown (window closed, Ctrl-C, killed
/// process) left a directory behind, and one belonging to any other project was invisible to every
/// code path that could remove it. They accumulated forever: a real install had **98** of them going
/// back two weeks, with zero `consumed-*` or `quarantine-*` ever cleaned up.
///
/// Deliberately scope-BLIND, which is the whole point — the leak is precisely the leases this
/// process would otherwise never look at. Safety comes from the lock plus the age check, not from
/// scope: a directory is removed only if its lock can be taken exclusively (so no live process owns
/// it) and it is older than [`LEASE_TTL_SECS`]. Unknown or unreadable manifests are retained;
/// only directories without a manifest and already-consumed residue may age out by directory mtime.
///
/// Best-effort throughout: sweeping is housekeeping and must never block or fail a REPL start.
pub fn sweep_expired() {
    let root = recovery_root();
    let Ok(rd) = fs::read_dir(&root) else { return };
    let now = now_unix();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Never touch this process's own live lease.
        if let Ok(g) = ACTIVE.lock() {
            if g.as_ref().is_some_and(|a| a.dir == path) {
                continue;
            }
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `consumed-*` / `quarantine-*` are already-dead byproducts: age them out by directory mtime
        // (they hold no authoritative manifest to read a timestamp from).
        let residue = name.starts_with("consumed-") || name.starts_with("quarantine-");
        let stamp = if residue {
            None
        } else {
            match fs::read_to_string(manifest_path(&path)) {
                Ok(raw) => {
                    // A newer backend/schema or unreadable reference may still own recovery
                    // evidence. Neither an old timestamp nor a parse failure authorizes deletion.
                    let Ok(manifest) = serde_json::from_str::<RecoveryManifest>(&raw) else {
                        continue;
                    };
                    if manifest.validate_checkpoint_schema().is_err()
                        || (manifest.sidecar_generation > 0
                            && read_authoritative_snapshot(&path, &manifest).is_err())
                    {
                        continue;
                    }
                    Some(
                        if manifest.effects_unsettled || manifest.side_effects_possible {
                            now
                        } else {
                            manifest.updated_unix
                        },
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => continue,
            }
        };
        let age_ok = match stamp {
            Some(updated) => now.saturating_sub(updated) > LEASE_TTL_SECS,
            // No readable manifest (residue, or corrupt/half-created): fall back to the directory's
            // own mtime. If the filesystem won't say either, leave it alone rather than guess.
            None => entry
                .metadata()
                .and_then(|md| md.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .is_some_and(|d| now.saturating_sub(d.as_secs()) > LEASE_TTL_SECS),
        };
        if !age_ok {
            continue;
        }
        // The lock is the liveness test: a running process holds its lease lock for its whole
        // lifetime, so failing to acquire means "still in use" and we skip it.
        //
        // Then DROP it before removing. The lock owns an open handle to `lease.lock` INSIDE this
        // directory, and Windows refuses to delete a directory containing an open file — holding the
        // guard across `remove_dir_all` would make the sweep silently no-op on the very platform
        // where the leak was measured. Releasing early is safe here: the directory is already older
        // than the TTL, so there is no live process to race with.
        match crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
            &lock_path(&path),
            Duration::from_millis(50),
        ) {
            Ok(lock) => drop(lock),
            Err(_) => continue,
        }
        let _ = fs::remove_dir_all(&path);
    }
}

/// Scan for stale recovery leases from previous processes in exactly this repository/worktree.
pub fn scan_stale(repo_scope: &str) -> Vec<RecoveryOffer> {
    let root = recovery_root();
    let Ok(rd) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(g) = ACTIVE.lock() {
            if g.as_ref().is_some_and(|a| a.dir == path) {
                continue;
            }
        }
        let Ok(_lock) = crate::core::repo_lock::RepoTxnLock::acquire(
            &lock_path(&path),
            Duration::from_millis(50),
        ) else {
            continue;
        };
        if path.join("consumed-manifest.json").is_file() {
            // Consumption already committed under this same lease lock; interrupted cleanup is
            // TTL-swept residue, never another recovery offer or a corrupt manifest to quarantine.
            continue;
        }
        let Ok(raw) = fs::read_to_string(manifest_path(&path)) else {
            // Preserve unreadable bytes for inspection rather than making them TTL residue.
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<RecoveryManifest>(&raw) else {
            // A backend/schema this reader cannot understand is not disposable evidence.
            continue;
        };
        if manifest.validate_checkpoint_schema().is_err() {
            continue;
        }
        if manifest.repo_scope != repo_scope {
            continue;
        }
        if manifest.sidecar_generation == 0 || manifest.safe_history_len == 0 {
            continue;
        }
        let Ok((history, design_state, _, _)) = read_authoritative_snapshot(&path, &manifest)
        else {
            // Unsupported policy/task generations are evidence, not quarantine/TTL residue.
            continue;
        };
        let pending_draft = fs::read_to_string(draft_path(&path, manifest.sidecar_generation))
            .ok()
            .filter(|s| !s.trim().is_empty());
        out.push(RecoveryOffer {
            path,
            manifest,
            history,
            design_state,
            pending_draft,
        });
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.manifest.updated_unix));
    out
}

pub type RecoveryState = (
    Vec<Message>,
    Option<String>,
    Option<TaskDesignState>,
    TaskState,
    SessionControl,
);

/// Legacy acceptance may consume only a task-free conversation. Inspection can omit tasks; restore
/// must never consume a lease and then silently throw away its execution state.
#[cfg(test)]
pub fn accept_with_design(
    offer: &RecoveryOffer,
) -> Result<(Vec<Message>, Option<String>, Option<TaskDesignState>)> {
    accept_state_inner(offer, false).map(|(history, draft, design, _, _)| (history, draft, design))
}

pub fn accept_with_state(offer: &RecoveryOffer) -> Result<RecoveryState> {
    accept_state_inner(offer, true)
}

fn accept_state_inner(offer: &RecoveryOffer, accepts_task: bool) -> Result<RecoveryState> {
    let lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path(&offer.path),
        Duration::from_secs(1),
    )?;
    let raw = fs::read_to_string(manifest_path(&offer.path))
        .with_context(|| format!("reading recovery offer {}", offer.path.display()))?;
    let current: RecoveryManifest = serde_json::from_str(&raw).context("parsing recovery offer")?;
    if current.run_id != offer.manifest.run_id
        || current.sidecar_generation != offer.manifest.sidecar_generation
        || current.updated_unix != offer.manifest.updated_unix
    {
        anyhow::bail!("recovery offer changed before it could be consumed; rescan and retry");
    }
    let (history, design, task, control) = read_authoritative_snapshot(&offer.path, &current)?;
    anyhow::ensure!(
        accepts_task || (task == TaskState::default() && control == SessionControl::default()),
        "recovery contains task/control state; restore with accept_with_state"
    );
    let draft = fs::read_to_string(draft_path(&offer.path, current.sidecar_generation))
        .ok()
        .filter(|s| !s.trim().is_empty());
    // Restoration refusal must leave the offer consumable and the transcript unchanged.
    let control = if let Some(owner) = crate::agent::todo::bound_context() {
        let context = owner.control();
        context.restore(control)?;
        context.snapshot()
    } else {
        control
    };
    consume_dir(&offer.path, lock)?;
    Ok((history, draft, design, task, control))
}

/// Discard one recovery offer exactly once.
pub fn discard(offer: &RecoveryOffer) -> Result<()> {
    let lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path(&offer.path),
        Duration::from_secs(1),
    )?;
    let raw = fs::read_to_string(manifest_path(&offer.path))
        .with_context(|| format!("reading recovery offer {}", offer.path.display()))?;
    let current: RecoveryManifest = serde_json::from_str(&raw).context("parsing recovery offer")?;
    if current.run_id != offer.manifest.run_id
        || current.updated_unix != offer.manifest.updated_unix
    {
        anyhow::bail!("recovery offer changed before it could be discarded; rescan and retry");
    }
    consume_dir(&offer.path, lock)
}

fn read_authoritative_snapshot(
    path: &Path,
    manifest: &RecoveryManifest,
) -> Result<(
    Vec<Message>,
    Option<TaskDesignState>,
    TaskState,
    SessionControl,
)> {
    manifest.validate_checkpoint_schema()?;
    let bytes = crate::core::session_store::read_bounded(
        &history_path(path, manifest.sidecar_generation),
        128 * 1024 * 1024,
    )
    .map_err(|e| anyhow::anyhow!("recovery history unavailable: {e:?}"))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing recovery history")?;
    let (mut history, design, task, control) = if value.is_array() {
        anyhow::ensure!(
            !manifest.has_design_state && !manifest.has_task_state,
            "recovery generation lost its design/task state"
        );
        (
            crate::core::session_store::parse_stored_history(&bytes)?,
            None,
            TaskState::default(),
            SessionControl::default(),
        )
    } else {
        let stored: HistoryGeneration = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(stored.version <= 1, "unsupported recovery history version");
        anyhow::ensure!(
            stored.version == 0 || stored.session_control.is_some(),
            "missing recovery session_control"
        );
        anyhow::ensure!(
            stored.generation == manifest.sidecar_generation,
            "recovery history/design/task generation mismatch"
        );
        anyhow::ensure!(
            stored.design_state.is_some() == manifest.has_design_state,
            "recovery design state does not match commit record"
        );
        anyhow::ensure!(
            stored.task_state.is_some() == manifest.has_task_state,
            "recovery task state does not match commit record"
        );
        (
            crate::core::session_store::parse_stored_history(&serde_json::to_vec(
                &stored.messages,
            )?)?,
            stored.design_state,
            stored.task_state.unwrap_or_default(),
            stored.session_control.unwrap_or_default(),
        )
    };
    task.validate().context("invalid recovery task state")?;
    control
        .validate()
        .context("invalid recovery session control")?;
    // Truncating a new envelope would pair history from one point with state from a later point.
    anyhow::ensure!(
        history.len() == manifest.safe_history_len,
        "recovery history is incomplete or mismatched"
    );
    history.truncate(manifest.safe_history_len);
    Ok((history, design, task, control))
}

fn consume_dir(path: &Path, lock: crate::core::repo_lock::RepoTxnLock) -> Result<()> {
    // The lock file lives INSIDE the directory: Windows cannot rename/remove that directory while
    // its handle is open. Retire the authoritative manifest instead, while still holding the lock.
    // This is the consumption commit: any other accept/discard sees no manifest and cannot replay.
    // Dropping the lock BEFORE retiring the manifest would introduce a double-accept race.
    fs::rename(manifest_path(path), path.join("consumed-manifest.json"))
        .with_context(|| format!("consuming recovery lease {}", path.display()))?;
    drop(lock);
    // Cleanup is not the commit record. A failed deletion leaves no recoverable manifest and can
    // be collected by sweep_expired later, without making a successful recovery appear to fail.
    let _ = fs::remove_dir_all(path);
    Ok(())
}

/// Human-readable banner for a recovery offer. Draft contents are deliberately not echoed.
pub fn format_offer(offer: &RecoveryOffer) -> String {
    let phase = offer.manifest.phase.as_str();
    let draft = offer
        .pending_draft
        .as_ref()
        .map(|_| " · draft saved")
        .unwrap_or_default();
    let ckpt = offer
        .manifest
        .checkpoint_label()
        .map(|label| format!(" · {label}"))
        .unwrap_or_default();
    let effects = if offer.manifest.effects_unsettled {
        " · effects unsettled; inspect before retry or recovery"
    } else if offer.manifest.side_effects_possible {
        " · side effects possible"
    } else {
        ""
    };
    format!(
        "recoverable session · phase={phase} · {} msgs{ckpt}{effects}{draft}",
        offer.history.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("recovery_control_tests.rs");

    fn manifest(scope: &str, generation: u64, len: usize) -> RecoveryManifest {
        RecoveryManifest {
            schema: SCHEMA,
            binary_version: env!("CARGO_PKG_VERSION").into(),
            pid: 0,
            run_id: "stale".into(),
            repo: "repo".into(),
            repo_scope: scope.into(),
            phase: RecoveryPhase::WaitingModel,
            session_name: None,
            safe_history_len: len,
            checkpoint_id: None,
            checkpoint_ref: None,
            sidecar_generation: generation,
            has_design_state: false,
            has_task_state: false,
            turn_id: Some(generation),
            side_effects_possible: false,
            effects_unsettled: false,
            updated_unix: 1,
        }
    }

    fn reference(id: u32) -> SnapshotRef {
        use crate::core::snapshots::SnapshotBinding;
        let root = std::env::temp_dir().join("recovery-reference-fixture");
        SnapshotRef::Legacy {
            binding: SnapshotBinding {
                repo_id: "repo-fixture".into(),
                worktree_id: "worktree-fixture".into(),
                canonical_git_dir: root.join(".git"),
                canonical_common_git_dir: root.join(".git"),
                canonical_root: root,
            },
            checkpoint_id: id,
        }
    }

    fn git_reference(length: usize) -> SnapshotRef {
        SnapshotRef::Git {
            binding: reference(1).binding().clone(),
            task_id: "task-fixture".into(),
            commit: crate::core::snapshots::GitOid::parse(&"a".repeat(length)).unwrap(),
        }
    }

    struct ActiveFixture {
        _root: crate::core::file_placement::tests::TestDir,
        dir: PathBuf,
        env: Option<crate::core::config::EnvGuard>,
        old_slug: Option<String>,
    }

    impl ActiveFixture {
        fn new() -> Self {
            let root = crate::core::file_placement::tests::TestDir::new();
            let env = crate::core::config::EnvGuard::set([("AIZEN_HOME", root.path().as_os_str())]);
            let dir = root.path().join("recovery").join("owned-active");
            fs::create_dir_all(&dir).unwrap();
            let lock = crate::core::repo_lock::RepoTxnLock::acquire(
                &lock_path(&dir),
                Duration::from_secs(1),
            )
            .unwrap();
            let mut state = manifest("owned-scope", 0, 0);
            state.turn_id = None;
            write_manifest(&dir, &state).unwrap();
            let mut guard = ACTIVE.lock().unwrap();
            assert!(guard.is_none(), "fixture must not replace another lease");
            *guard = Some(ActiveLease {
                dir: dir.clone(),
                manifest: state,
                _lock: lock,
            });
            drop(guard);
            let old_slug = crate::core::session_store::current_session_slug();
            crate::core::session_store::set_session_slug(Some("owned-recovery-fixture".into()));
            Self {
                _root: root,
                dir,
                env: Some(env),
                old_slug,
            }
        }

        fn stored(&self) -> RecoveryManifest {
            serde_json::from_slice(&fs::read(manifest_path(&self.dir)).unwrap()).unwrap()
        }

        fn exit_process(&self) {
            let lease = {
                let mut guard = ACTIVE.lock().unwrap_or_else(|error| error.into_inner());
                if guard.as_ref().is_some_and(|active| active.dir == self.dir) {
                    guard.take()
                } else {
                    None
                }
            };
            drop(lease);
        }
    }

    impl Drop for ActiveFixture {
        fn drop(&mut self) {
            clear();
            // Fixture teardown simulates process exit, even when production clear retains effects.
            self.exit_process();
            crate::core::session_store::set_session_slug(self.old_slug.take());
            drop(self.env.take());
        }
    }

    #[test]
    fn recovery_checkpoint_keeps_text_but_never_persists_input_pixels() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = ActiveFixture::new();
        let url = "data:image/gif;base64,R0lGODlh".to_string();
        let mut compacted = Message::system("compacted text");
        compacted.images.push(url.clone());
        let mut reread = Message::tool_result("read", "ROI observation");
        reread.tool_images.push(
            crate::core::tool_media::ToolImage::from_bytes(
                b"GIF89a-private-input",
                "image/gif",
                crate::core::tool_media::ImageProvenance::new("reference_read", None),
            )
            .unwrap(),
        );
        let history = vec![
            Message::user_with_images("look", vec![url]),
            compacted,
            reread,
        ];
        let live = serde_json::to_value(&history).unwrap();
        checkpoint_history_with_design(
            &history,
            Some("continue"),
            RecoveryPhase::WaitingModel,
            None,
        )
        .unwrap();
        let manifest = fixture.stored();
        let raw =
            fs::read_to_string(history_path(&fixture.dir, manifest.sidecar_generation)).unwrap();
        assert!(
            !raw.contains("base64") && !raw.contains("blob:") && !raw.contains("visual_assets")
        );
        let (restored, _, task, _) = read_authoritative_snapshot(&fixture.dir, &manifest).unwrap();
        assert_eq!(task, TaskState::default());
        assert!(restored.iter().all(|m| m.images.is_empty()));
        assert!(restored
            .iter()
            .flat_map(|m| &m.tool_images)
            .all(|image| !image.is_available()));
        assert!(restored[0].content.as_deref().unwrap().starts_with("look"));
        assert!(restored[1]
            .content
            .as_deref()
            .unwrap()
            .contains("attach the original"));
        assert_eq!(serde_json::to_value(&history).unwrap(), live);
        let blobs = crate::core::config::aizen_home()
            .join("sessions")
            .join("blobs");
        assert!(!blobs.exists());
    }

    #[test]
    fn idle_autosave_preserves_typed_uncertainty_across_later_turn_and_clean_exit() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for effect in [
            crate::agent::tools::ToolEffectState::UnknownWorker,
            crate::agent::tools::ToolEffectState::PossiblyApplied(Some(42)),
        ] {
            let fixture = ActiveFixture::new();
            let mut history = vec![Message::user("owned interrupted request")];
            checkpoint_history_with_design(
                &history,
                Some("owned interrupted request"),
                RecoveryPhase::WaitingModel,
                None,
            )
            .unwrap();
            let turn_id = fixture.stored().turn_id;
            set_checkpoint(Some(reference(12)));
            mark_side_effects_possible();
            let mut boundary = TurnBoundary::new();
            let _outcome = crate::agent::tools::ToolOutcome::failed_with_effect_state(
                "owned uncertain body",
                effect,
            );
            boundary.complete(false);
            crate::core::session_store::autosave_last_with_design(&history, Some("fixture"), None);
            let stored = fixture.stored();
            assert!(stored.effects_unsettled && stored.side_effects_possible);
            assert_eq!(stored.phase, RecoveryPhase::Finalizing);
            assert_eq!(stored.checkpoint_ref, Some(reference(12)));
            assert_eq!(stored.turn_id, turn_id);
            assert_eq!(
                read_authoritative_snapshot(&fixture.dir, &stored)
                    .unwrap()
                    .0
                    .len(),
                history.len()
            );

            // A successful later reader is not reconciliation of the previous uncertain body.
            history.push(Message::assistant("later read completed"));
            checkpoint_history_with_design(
                &history,
                Some("new request"),
                RecoveryPhase::WaitingModel,
                None,
            )
            .unwrap();
            TurnBoundary::new().complete(true);
            set_checkpoint(Some(reference(13)));
            crate::core::session_store::autosave_last_with_design(&history, Some("fixture"), None);
            let mut stored = fixture.stored();
            assert!(stored.effects_unsettled && stored.side_effects_possible);
            assert_eq!(stored.checkpoint_ref, Some(reference(12)));
            assert_eq!(stored.turn_id, turn_id);
            clear();
            assert!(
                manifest_path(&fixture.dir).is_file(),
                "clean exit is not settlement"
            );
            // `clear` deliberately retains the uncertain lease lock until the owning process exits.
            // Release the test's simulated process ownership before scanning as a new process would.
            fixture.exit_process();
            stored.updated_unix = now_unix().saturating_sub(LEASE_TTL_SECS + 3600);
            write_manifest(&fixture.dir, &stored).unwrap();
            sweep_expired();
            assert!(
                manifest_path(&fixture.dir).is_file(),
                "TTL is not reconciliation"
            );
            let offers = scan_stale("owned-scope");
            assert_eq!(offers.len(), 1);
            assert!(format_offer(&offers[0]).contains("effects unsettled"));
        }
    }

    #[test]
    fn dropped_boundary_retains_effects_but_proven_completion_can_close_a_turn() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        {
            let fixture = ActiveFixture::new();
            set_checkpoint(Some(reference(7)));
            mark_side_effects_possible();
            drop(TurnBoundary::new());
            checkpoint_history_with_design(
                &[Message::user("cancelled")],
                None,
                RecoveryPhase::Idle,
                None,
            )
            .unwrap();
            assert!(fixture.stored().effects_unsettled);
            assert_eq!(fixture.stored().checkpoint_ref, Some(reference(7)));
        }
        {
            let fixture = ActiveFixture::new();
            set_checkpoint(Some(reference(8)));
            mark_side_effects_possible();
            checkpoint_history_with_design(
                &[Message::user("still running")],
                None,
                RecoveryPhase::Idle,
                None,
            )
            .unwrap();
            assert!(
                fixture.stored().side_effects_possible,
                "autosave cannot close the turn"
            );
            TurnBoundary::new().complete(true);
            checkpoint_history_with_design(
                &[Message::user("settled")],
                None,
                RecoveryPhase::Idle,
                None,
            )
            .unwrap();
            let stored = fixture.stored();
            assert_eq!(stored.phase, RecoveryPhase::Idle);
            assert!(!stored.effects_unsettled && !stored.side_effects_possible);
            assert!(stored.checkpoint_id.is_none());
            assert!(stored.checkpoint_ref.is_none());
            clear();
            assert!(!manifest_path(&fixture.dir).exists());
        }
    }

    #[test]
    fn typed_checkpoint_write_preserves_full_provenance_and_has_no_numeric_alias() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = ActiveFixture::new();
        for reference in [reference(12), git_reference(40), git_reference(64)] {
            set_checkpoint(Some(reference.clone()));
            let stored = fixture.stored();
            assert_eq!(stored.schema, SCHEMA);
            assert_eq!(stored.checkpoint_ref, Some(reference.clone()));
            assert!(stored.checkpoint_id.is_none());
            let raw: serde_json::Value =
                serde_json::from_slice(&fs::read(manifest_path(&fixture.dir)).unwrap()).unwrap();
            assert!(raw.get("checkpoint_id").is_none());
            assert_eq!(stored.checkpoint_label(), Some(reference.display_label()));
        }
        set_checkpoint(None);
        assert!(fixture.stored().checkpoint_ref.is_none());
    }

    #[test]
    fn typed_git_checkpoint_survives_unsettled_turn_and_clear_attempts() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = ActiveFixture::new();
        let reference = git_reference(64);
        set_checkpoint(Some(reference.clone()));
        mark_side_effects_possible();
        TurnBoundary::new().complete(false);
        set_checkpoint(None);
        set_checkpoint(Some(git_reference(40)));
        TurnBoundary::new().complete(true);
        clear();
        let stored = fixture.stored();
        assert!(stored.effects_unsettled && stored.side_effects_possible);
        assert_eq!(stored.checkpoint_ref, Some(reference));
    }

    #[test]
    fn schema_two_numeric_checkpoint_is_read_without_fabricated_binding() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let mut legacy = manifest("legacy-scope", 1, 1);
        legacy.schema = LEGACY_SCHEMA;
        legacy.checkpoint_id = Some(42);
        let mut json = serde_json::to_value(&legacy).unwrap();
        json.as_object_mut().unwrap().remove("checkpoint_ref");
        let decoded: RecoveryManifest = serde_json::from_value(json).unwrap();
        decoded.validate_checkpoint_schema().unwrap();
        assert_eq!(decoded.checkpoint_id, Some(42));
        assert!(decoded.checkpoint_ref.is_none());
        assert_eq!(
            decoded.checkpoint_label().as_deref(),
            Some("legacy checkpoint #42")
        );
        atomic_write_owner_only(
            &history_path(fixture.path(), 1),
            &serde_json::to_vec(&vec![
                serde_json::to_value(Message::user("old history")).unwrap()
            ])
            .unwrap(),
        )
        .unwrap();
        let (history, _, _, _) = read_authoritative_snapshot(fixture.path(), &decoded).unwrap();
        assert_eq!(history[0].content.as_deref(), Some("old history"));
    }

    #[test]
    fn mixed_future_and_unbound_recovery_checkpoint_schemas_are_rejected() {
        let mut stored = manifest("scope", 1, 0);
        stored.checkpoint_id = Some(1);
        assert!(stored.validate_checkpoint_schema().is_err());
        stored.checkpoint_ref = Some(reference(1));
        assert!(stored.validate_checkpoint_schema().is_err());
        stored.schema = LEGACY_SCHEMA;
        assert!(stored.validate_checkpoint_schema().is_err());
        stored.checkpoint_id = None;
        stored.schema = SCHEMA + 1;
        assert!(stored.validate_checkpoint_schema().is_err());
        stored.schema = SCHEMA;
        stored.checkpoint_ref = Some(reference(0));
        assert!(stored.validate_checkpoint_schema().is_err());
    }

    #[test]
    fn scan_and_accept_read_both_checkpoint_schemas_without_rewriting_identity() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for typed in [false, true] {
            let fixture = ActiveFixture::new();
            checkpoint_history_with_design(
                &[Message::user("saved conversation")],
                Some("draft"),
                RecoveryPhase::WaitingModel,
                None,
            )
            .unwrap();
            let mut stored = fixture.stored();
            if typed {
                stored.checkpoint_ref = Some(git_reference(64));
            } else {
                stored.schema = LEGACY_SCHEMA;
                stored.checkpoint_id = Some(21);
            }
            fixture.exit_process();
            write_manifest(&fixture.dir, &stored).unwrap();
            let bytes = fs::read(manifest_path(&fixture.dir)).unwrap();
            let offers = scan_stale("owned-scope");
            assert_eq!(offers.len(), 1);
            assert_eq!(offers[0].manifest.checkpoint_ref, stored.checkpoint_ref);
            assert_eq!(offers[0].manifest.checkpoint_id, stored.checkpoint_id);
            assert_eq!(fs::read(manifest_path(&fixture.dir)).unwrap(), bytes);
            let (history, draft, _, _, _) = accept_with_state(&offers[0]).unwrap();
            assert_eq!(history[0].content.as_deref(), Some("saved conversation"));
            assert_eq!(draft.as_deref(), Some("draft"));
        }
    }

    #[test]
    fn future_or_unreadable_checkpoint_evidence_is_not_swept_or_quarantined() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = ActiveFixture::new();
        fixture.exit_process();
        let mut future = manifest("owned-scope", 1, 1);
        future.schema = SCHEMA + 1;
        future.updated_unix = 1;
        let mut unknown_backend = serde_json::to_value(&future).unwrap();
        unknown_backend["schema"] = serde_json::json!(SCHEMA);
        unknown_backend["checkpoint_ref"] = serde_json::json!({"backend":"future"});
        for raw in [
            serde_json::to_vec(&future).unwrap(),
            serde_json::to_vec(&unknown_backend).unwrap(),
            b"{truncated".to_vec(),
        ] {
            atomic_write_owner_only(&manifest_path(&fixture.dir), &raw).unwrap();
            assert!(scan_stale("owned-scope").is_empty());
            sweep_expired();
            assert_eq!(fs::read(manifest_path(&fixture.dir)).unwrap(), raw);
        }
    }

    #[test]
    fn legacy_recovery_manifest_defaults_to_no_unsettled_latch() {
        let mut stored = serde_json::to_value(manifest("legacy", 1, 0)).unwrap();
        stored.as_object_mut().unwrap().remove("effects_unsettled");
        let decoded: RecoveryManifest = serde_json::from_value(stored).unwrap();
        assert!(!decoded.effects_unsettled);
    }

    #[test]
    fn recovery_requires_matching_generation_and_state_commit_record() {
        let dir =
            std::env::temp_dir().join(format!("aizen-recovery-generation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let state = crate::agent::design::DesignContext::new(
            "owner",
            "workspace",
            crate::agent::design::DesignMode::On,
        )
        .snapshot();
        let stored = HistoryGeneration {
            version: 0,
            session_control: None,
            generation: 7,
            design_state: Some(state),
            task_state: None,
            messages: vec![serde_json::to_value(Message::user("same-generation")).unwrap()],
        };
        atomic_write_owner_only(
            &history_path(&dir, 7),
            &serde_json::to_vec(&stored).unwrap(),
        )
        .unwrap();
        let mut manifest = manifest("workspace", 7, 1);
        manifest.has_design_state = true;
        let (history, design, task, _) = read_authoritative_snapshot(&dir, &manifest).unwrap();
        assert_eq!(task, TaskState::default());
        assert_eq!(history[0].content.as_deref(), Some("same-generation"));
        assert_eq!(design.unwrap().owner_id, "owner");
        manifest.has_design_state = false;
        assert!(read_authoritative_snapshot(&dir, &manifest).is_err());
        manifest.has_design_state = true;
        let mut mismatched = stored;
        mismatched.generation = 6;
        atomic_write_owner_only(
            &history_path(&dir, 7),
            &serde_json::to_vec(&mismatched).unwrap(),
        )
        .unwrap();
        assert!(read_authoritative_snapshot(&dir, &manifest).is_err());
        // Legacy bare history defaults to no design, never inherits another run's state.
        atomic_write_owner_only(
            &history_path(&dir, 7),
            &serde_json::to_vec(&vec![Message::user("legacy")]).unwrap(),
        )
        .unwrap();
        manifest.has_design_state = false;
        assert!(read_authoritative_snapshot(&dir, &manifest)
            .unwrap()
            .1
            .is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_task_checkpoint_is_coherent_and_legacy_accept_cannot_discard_it() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = ActiveFixture::new();
        let context = crate::agent::todo::TaskContext::private();
        context
            .replace(vec![crate::agent::todo::Todo::new(
                "resume owned checklist",
                crate::agent::todo::Status::InProgress,
            )])
            .unwrap();
        let history = vec![Message::user("owned request")];
        checkpoint_history_with_state(
            &history,
            Some("continue"),
            RecoveryPhase::WaitingModel,
            None,
            &context.snapshot(),
        )
        .unwrap();
        let first_generation = fixture.stored().sidecar_generation;
        context.end_turn(true);
        let task = context.snapshot();
        checkpoint_history_with_state(
            &history,
            Some("continue"),
            RecoveryPhase::WaitingModel,
            None,
            &task,
        )
        .unwrap();
        let manifest = fixture.stored();
        assert!(manifest.has_task_state);
        assert!(manifest.sidecar_generation > first_generation);
        let snapshot = read_authoritative_snapshot(&fixture.dir, &manifest).unwrap();
        assert_eq!(snapshot.0.len(), history.len());
        assert_eq!(snapshot.2, task);
        let before = fs::read(manifest_path(&fixture.dir)).unwrap();
        let mut future = task.clone();
        future.version = 999;
        assert!(
            checkpoint_history_with_state(&history, None, RecoveryPhase::Idle, None, &future)
                .is_err()
        );
        assert_eq!(fs::read(manifest_path(&fixture.dir)).unwrap(), before);
        assert_eq!(
            read_authoritative_snapshot(&fixture.dir, &manifest)
                .unwrap()
                .2,
            task
        );
        fixture.exit_process();
        let offers = scan_stale("owned-scope");
        assert_eq!(offers.len(), 1);
        assert!(accept_with_design(&offers[0]).is_err());
        assert!(
            manifest_path(&fixture.dir).is_file(),
            "legacy rejection must not consume the lease"
        );
        let (restored, draft, _, restored_task, _) = accept_with_state(&offers[0]).unwrap();
        assert_eq!(restored[0].content.as_deref(), Some("owned request"));
        assert_eq!(draft.as_deref(), Some("continue"));
        assert_eq!(restored_task, task);
        assert!(accept_with_state(&offers[0]).is_err());
    }

    #[test]
    fn recovery_rejects_missing_invalid_and_mismatched_task_generation() {
        let dir = crate::core::file_placement::tests::TestDir::new();
        let mut manifest = manifest("tasks", 3, 1);
        manifest.has_task_state = true;
        let valid = serde_json::json!({
            "generation": 3,
            "task_state": TaskState::default(),
            "messages": [Message::user("task generation")],
        });
        atomic_write_owner_only(
            &history_path(dir.path(), 3),
            &serde_json::to_vec(&valid).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_authoritative_snapshot(dir.path(), &manifest)
                .unwrap()
                .2,
            TaskState::default()
        );
        for kind in ["missing", "null", "future", "generation"] {
            let mut invalid = valid.clone();
            match kind {
                "missing" => {
                    invalid.as_object_mut().unwrap().remove("task_state");
                }
                "null" => invalid["task_state"] = serde_json::Value::Null,
                "future" => invalid["task_state"]["version"] = serde_json::json!(999),
                "generation" => invalid["generation"] = serde_json::json!(2),
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec(&invalid).unwrap();
            atomic_write_owner_only(&history_path(dir.path(), 3), &bytes).unwrap();
            assert!(
                read_authoritative_snapshot(dir.path(), &manifest).is_err(),
                "accepted {kind}"
            );
            assert_eq!(fs::read(history_path(dir.path(), 3)).unwrap(), bytes);
        }
        manifest.has_task_state = false;
        atomic_write_owner_only(
            &history_path(dir.path(), 3),
            &serde_json::to_vec(&valid).unwrap(),
        )
        .unwrap();
        assert!(
            read_authoritative_snapshot(dir.path(), &manifest).is_err(),
            "state presence must match manifest"
        );
        let mut legacy = valid;
        legacy.as_object_mut().unwrap().remove("task_state");
        atomic_write_owner_only(
            &history_path(dir.path(), 3),
            &serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_authoritative_snapshot(dir.path(), &manifest)
                .unwrap()
                .2,
            TaskState::default()
        );
    }

    #[test]
    fn concurrent_and_repeated_accepts_have_only_one_successful_consumer() {
        let dir = std::env::temp_dir().join(format!(
            "aizen-consume-lock-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        fs::create_dir_all(&dir).unwrap();
        let manifest = manifest("fixture", 1, 1);
        let history = vec![Message::user("only one restore")];
        atomic_write_owner_only(
            &history_path(&dir, 1),
            &serde_json::to_vec(&history).unwrap(),
        )
        .unwrap();
        write_manifest(&dir, &manifest).unwrap();
        let offer = RecoveryOffer {
            path: dir.clone(),
            manifest,
            history,
            design_state: None,
            pending_draft: None,
        };
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let offer = offer.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    accept_with_design(&offer)
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1,
            "retire authoritative manifest under lock; cleanup must not turn its winner into failure");
        assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
        assert!(
            !manifest_path(&dir).exists(),
            "successful consumption retires the authoritative record"
        );
        assert!(
            accept_with_design(&offer).is_err(),
            "a consumed offer must never restore twice"
        );
        assert!(
            discard(&offer).is_err(),
            "a consumed offer cannot be discarded a second time"
        );
    }

    #[test]
    fn committed_consumption_residue_is_not_a_recovery_offer() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "aizen-consumed-residue-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        struct Cleanup {
            root: PathBuf,
            env: Option<crate::core::config::EnvGuard>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                drop(self.env.take());
                let _ = fs::remove_dir_all(&self.root);
            }
        }
        fs::create_dir_all(&root).unwrap();
        let _cleanup = Cleanup {
            root: root.clone(),
            env: Some(crate::core::config::EnvGuard::set([(
                "AIZEN_HOME",
                root.as_os_str(),
            )])),
        };
        let dir = root.join("recovery").join("consumed-before-cleanup");
        fs::create_dir_all(&dir).unwrap();
        let manifest = manifest("fixture", 1, 1);
        atomic_write_owner_only(
            &dir.join("consumed-manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        atomic_write_owner_only(
            &history_path(&dir, 1),
            &serde_json::to_vec(&vec![Message::user("consumed")]).unwrap(),
        )
        .unwrap();
        assert!(
            scan_stale("fixture").is_empty(),
            "a tombstone is not authoritative manifest.json"
        );
    }

    #[test]
    fn phase_names_are_stable() {
        assert_eq!(RecoveryPhase::ExecutingTools.as_str(), "executing_tools");
        assert_eq!(RecoveryPhase::WaitingModel.as_str(), "waiting_model");
    }

    #[test]
    fn stale_offer_is_repo_scoped_and_requires_complete_generation() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("aizen-recovery-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        std::env::set_var("AIZEN_HOME", &root);
        let dir = recovery_root().join("stale");
        fs::create_dir_all(&dir).unwrap();
        let m = manifest("repo-a", 7, 2);
        write_manifest(&dir, &m).unwrap();
        atomic_write_owner_only(
            &history_path(&dir, 7),
            &serde_json::to_vec(&vec![Message::system("s")]).unwrap(),
        )
        .unwrap();
        assert!(
            scan_stale("repo-a").is_empty(),
            "truncated generation is never offered"
        );
        assert!(
            scan_stale("repo-b").is_empty(),
            "foreign repository lease is invisible"
        );
        std::env::remove_var("AIZEN_HOME");
        let _ = fs::remove_dir_all(&root);
    }

    /// Abandoned leases must actually be collected, INCLUDING the ones belonging to other projects.
    /// Those were the leak: `clear()` only runs on a clean exit and `scan_stale()` filters by
    /// `repo_scope`, so a foreign lease was invisible to every path that could delete it. A real
    /// install had accumulated 98 of them.
    #[test]
    fn sweep_collects_expired_leases_across_every_scope() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("aizen-sweep-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        std::env::set_var("AIZEN_HOME", &root);

        let old_stamp = now_unix().saturating_sub(LEASE_TTL_SECS + 3600);

        // An expired lease from ANOTHER repository — the case scan_stale can never see.
        let foreign = recovery_root().join("expired-foreign");
        fs::create_dir_all(&foreign).unwrap();
        let mut m = manifest("some-other-repo", 3, 2);
        m.updated_unix = old_stamp;
        write_manifest(&foreign, &m).unwrap();
        // Expired valid history is collectible; missing/corrupt policy sidecars are not.
        atomic_write_owner_only(
            &history_path(&foreign, 3),
            &serde_json::to_vec(&vec![Message::user("old"), Message::assistant("done")]).unwrap(),
        )
        .unwrap();

        // A fresh lease in the same scope — must be left alone.
        let fresh = recovery_root().join("fresh-here");
        fs::create_dir_all(&fresh).unwrap();
        let mut fm = manifest("some-other-repo", 3, 2);
        fm.updated_unix = now_unix();
        write_manifest(&fresh, &fm).unwrap();

        // Residue with no manifest at all, dated by directory mtime (NOW here) — the age gate must
        // keep it, which is what pins the rule to age rather than to the name prefix.
        let residue = recovery_root().join("consumed-old-run");
        fs::create_dir_all(&residue).unwrap();

        sweep_expired();

        assert!(!foreign.exists(), "an expired foreign lease must be swept");
        assert!(fresh.exists(), "a fresh lease must survive the sweep");
        assert!(
            residue.exists(),
            "freshly-created residue is not yet expired"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = fs::remove_dir_all(&root);
    }
}
