//! Evidence-only access to the original Time Machine metadata namespace.
//!
//! This adapter never initializes checkpoint storage, resolves/migrates an identity, reads history,
//! or runs checkpoint recovery. Existing registry mappings and physical store/transaction lock paths
//! are preserved. Missing, corrupt or ambiguous evidence is not permission to select a new domain.

use super::inspection::{self, checked_metadata_entries, SpeculationInspection};
use super::namespace::plan_metadata_directory;
use crate::core::repo_lock::{LockMode, RepoTxnLock};
use crate::core::workspace_txn::{
    NativeResource, PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const MAX_REGISTRY_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_WORKTREES: usize = 128;
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

// These are only the old registry's wire shapes. Identity resolution belongs to the old writer,
// not this adapter. In particular, root_commit never reconnects a moved repository here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorktreeEntry {
    pub worktree_id: String,
    pub git_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepoEntry {
    pub repo_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_commit: Option<String>,
    pub common_git_dir: String,
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Registry {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedLegacyNamespace {
    identity: WorkspaceIdentity,
    metadata_root: PathBuf,
    ancestor: NativeResource,
    boundaries: Vec<NativeResource>,
    planned_ids: Option<(String, String)>,
}

/// Live metadata handle, never serialized and never a workspace restore authority.
#[derive(Debug, Clone)]
pub(crate) struct LegacyNamespace {
    plan: PlannedLegacyNamespace,
    repo_id: String,
    worktree_id: String,
    namespace_dir: PathBuf,
    directory: NativeResource,
}

pub(crate) fn plan_existing(root: &Path) -> Result<Option<PlannedLegacyNamespace>> {
    let identity = WorkspaceIdentity::discover(root)?;
    ensure!(
        identity.canonical_root == root && fs::canonicalize(root)? == root,
        "legacy Time Machine binding requires the canonical workspace root"
    );
    match (
        &identity.canonical_git_dir,
        &identity.canonical_common_git_dir,
    ) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => {
            PlannedLegacyNamespace::plan(identity, &crate::core::config::aizen_home()).map(Some)
        }
        _ => bail!("legacy Time Machine binding requires complete Git identity"),
    }
}

impl PlannedLegacyNamespace {
    pub(crate) fn plan(identity: WorkspaceIdentity, home: &Path) -> Result<Self> {
        let (metadata_root, ancestor) = plan_metadata_directory(&home.join("timemachine"))?;
        ensure!(
            !metadata_root.starts_with(&identity.canonical_root),
            "legacy Time Machine metadata must be outside the live workspace"
        );
        let git = identity
            .canonical_git_dir
            .as_ref()
            .context("missing Git directory")?;
        let common = identity
            .canonical_common_git_dir
            .as_ref()
            .context("missing common Git directory")?;
        let boundaries = [&identity.canonical_root, git, common]
            .into_iter()
            .map(|path| NativeResource::resolve(&path.join(".tm-evidence-boundary")))
            .collect::<Result<Vec<_>>>()?;
        let mut plan = Self {
            identity,
            metadata_root,
            ancestor,
            boundaries,
            planned_ids: None,
        };
        plan.planned_ids = plan.registry_ids(&read_registry(&plan.metadata_root)?)?;
        plan.verify()?;
        Ok(plan)
    }

    fn registry_ids(&self, registry: &Registry) -> Result<Option<(String, String)>> {
        // Never replace an invalid Unicode path with a lossy registry key that could name another
        // workspace. The historical registry stores strings, so such paths cannot be reopened.
        let key = |path: Option<&PathBuf>| -> Result<String> {
            Ok(path
                .context("legacy binding has incomplete Git identity")?
                .to_str()
                .context("legacy registry cannot represent this Git path")?
                .replace('\\', "/"))
        };
        let common = key(self.identity.canonical_common_git_dir.as_ref())?;
        let git = key(self.identity.canonical_git_dir.as_ref())?;
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

    fn verify(&self) -> Result<()> {
        self.ancestor.verify()?;
        for boundary in &self.boundaries {
            boundary.verify()?;
        }
        ensure!(
            WorkspaceIdentity::discover(&self.identity.canonical_root)? == self.identity,
            "legacy speculative workspace identity changed"
        );
        Ok(())
    }

    fn verify_admission(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<()> {
        ensure!(
            physical.is_exclusive() && writer.identity() == &self.identity,
            "legacy metadata requires matching writer and exclusive physical admission"
        );
        self.verify()
    }

    /// Read only. An absent directory does not authorize later creation or remapping.
    pub(crate) fn open_existing(&self) -> Result<Option<LegacyNamespace>> {
        self.verify()?;
        let ids = self.registry_ids(&read_registry(&self.metadata_root)?)?;
        ensure!(
            ids == self.planned_ids,
            "legacy speculative registry mapping changed"
        );
        let Some((repo_id, worktree_id)) = ids else {
            return Ok(None);
        };
        let namespace_dir = self
            .metadata_root
            .join(&repo_id)
            .join("worktrees")
            .join(&worktree_id);
        match fs::symlink_metadata(&namespace_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let (canonical, directory) = plan_metadata_directory(&namespace_dir)?;
        ensure!(
            canonical == namespace_dir,
            "legacy speculation namespace changed"
        );
        Ok(Some(LegacyNamespace {
            plan: self.clone(),
            repo_id,
            worktree_id,
            namespace_dir,
            directory,
        }))
    }
}

impl LegacyNamespace {
    pub(crate) fn namespace_dir(&self) -> &Path {
        &self.namespace_dir
    }

    fn check_mapping(&self) -> Result<()> {
        let ids = self
            .plan
            .registry_ids(&read_registry(&self.plan.metadata_root)?)?;
        ensure!(
            ids.as_ref() == Some(&(self.repo_id.clone(), self.worktree_id.clone())),
            "legacy registry mapping no longer matches the admitted binding"
        );
        Ok(())
    }

    /// Preserve store-shared -> actual namespace transaction-exclusive order. No RepoContext,
    /// checkpoint recovery, registry lock upgrade, worker wait or verification spawn in the body.
    pub(crate) fn with_transaction_lock<T>(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.plan.verify_admission(writer, physical)?;
        self.directory.verify()?;
        self.check_mapping()?;
        let start = Instant::now();
        let store_path = self
            .plan
            .metadata_root
            .join(&self.repo_id)
            .join("store.lock");
        let transaction_path = self.namespace_dir.join("transaction.lock");
        let _store = checked_lock(&store_path, LockMode::Shared, LOCK_TIMEOUT)?;
        let _transaction = checked_lock(
            &transaction_path,
            LockMode::Exclusive,
            LOCK_TIMEOUT.saturating_sub(start.elapsed()),
        )?;
        self.plan.verify_admission(writer, physical)?;
        self.directory.verify()?;
        self.check_mapping()?;
        body()
    }

    pub(crate) fn select_speculation_recovery(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        cohort: &str,
        prepared: &str,
    ) -> Result<inspection::SpeculationRecoverySelection> {
        inspection::select_speculation_recovery(self, parent, writer, physical, cohort, prepared)
    }

    #[cfg(test)]
    pub(crate) fn report_speculation_recovery(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        selection: &inspection::SpeculationRecoverySelection,
        approved: &[NativeResource],
    ) -> Result<inspection::SpeculationRecoveryReport> {
        self.inspect_selected_recovery(parent, writer, physical, selection, approved, false)
    }

    pub(crate) fn inspect_selected_recovery(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        selection: &inspection::SpeculationRecoverySelection,
        approved: &[NativeResource],
        record: bool,
    ) -> Result<inspection::SpeculationRecoveryReport> {
        inspection::inspect_selected_recovery(
            self, parent, writer, physical, selection, approved, record,
        )
    }
}

impl inspection::RecoveryNamespace for LegacyNamespace {
    fn identity(&self) -> &WorkspaceIdentity {
        &self.plan.identity
    }
    fn namespace_dir(&self) -> &Path {
        self.namespace_dir()
    }
    fn verify_admission(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<()> {
        self.plan.verify_admission(writer, physical)?;
        self.directory.verify()?;
        self.check_mapping()
    }
    fn with_transaction_lock<T>(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.with_transaction_lock(writer, physical, body)
    }
}

fn checked_lock(path: &Path, mode: LockMode, timeout: Duration) -> Result<RepoTxnLock> {
    // Historical evidence never creates missing lock leaves or namespaces.
    let parent_path = path.parent().context("legacy lock has no parent")?;
    ensure!(
        fs::symlink_metadata(parent_path)?.is_dir(),
        "legacy lock parent is not a directory"
    );
    let (_, parent) = plan_metadata_directory(parent_path)?;
    let resource = NativeResource::resolve(path)?;
    let lock = RepoTxnLock::acquire_existing(path, mode, timeout)?;
    lock.verify_existing_path(path)?;
    resource.verify()?;
    parent.verify()?;
    Ok(lock)
}

pub(crate) fn read_registry(root: &Path) -> Result<Registry> {
    let (_, boundary) = plan_metadata_directory(root)?;
    let path = root.join("registry.json");
    let bytes = match fs::symlink_metadata(&path) {
        Ok(_) => inspection::read_speculation_evidence(&path, MAX_REGISTRY_BYTES)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            require_no_unmapped_evidence(root)?;
            boundary.verify()?;
            return Ok(Registry::default());
        }
        Err(error) => return Err(error).context("reading legacy Time Machine registry"),
    };
    boundary.verify()?;
    let registry: Registry = serde_json::from_slice(&bytes)
        .context("legacy Time Machine registry is corrupt; it was not replaced")?;
    validate_registry(&registry)?;
    if registry.schema_version == 0 {
        require_no_unmapped_evidence(root)?;
    }
    Ok(registry)
}

fn require_no_unmapped_evidence(root: &Path) -> Result<()> {
    for path in checked_metadata_entries(root)? {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("repo-"))
            && store_has_recovery_evidence(&path)?
        {
            bail!("missing/unversioned Time Machine registry cannot re-key recovery evidence");
        }
    }
    Ok(())
}

pub(crate) fn validate_registry(registry: &Registry) -> Result<()> {
    ensure!(
        registry.schema_version <= 1,
        "unsupported future Time Machine registry schema"
    );
    let valid_id = |id: &str, prefix: &str| {
        id.strip_prefix(prefix)
            .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let mut repos = HashSet::new();
    let mut common_dirs = HashSet::new();
    let mut git_dirs = HashSet::new();
    for repo in &registry.repos {
        ensure!(
            valid_id(&repo.repo_id, "repo-")
                && repos.insert(&repo.repo_id)
                && common_dirs.insert(&repo.common_git_dir)
                && Path::new(&repo.common_git_dir).is_absolute(),
            "invalid or ambiguous Time Machine repository registry mapping"
        );
        let mut worktrees = HashSet::new();
        for wt in &repo.worktrees {
            ensure!(
                valid_id(&wt.worktree_id, "wt-")
                    && worktrees.insert(&wt.worktree_id)
                    && git_dirs.insert(&wt.git_dir)
                    && Path::new(&wt.git_dir).is_absolute(),
                "invalid or ambiguous Time Machine worktree registry mapping"
            );
        }
    }
    Ok(())
}

/// Presence protects even corrupt, future and completed evidence. Never parse it as garbage.
#[cfg(test)]
pub(crate) fn store_has_speculation_evidence(root: &Path) -> Result<bool> {
    store_has_evidence(root, false)
}
fn store_has_recovery_evidence(root: &Path) -> Result<bool> {
    store_has_evidence(root, true)
}
fn store_has_evidence(root: &Path, journals: bool) -> Result<bool> {
    let protected = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("speculation-") || (journals && name == "journal.json")
            })
    };
    if checked_metadata_entries(root)?
        .iter()
        .any(|path| protected(path))
    {
        return Ok(true);
    }
    let namespaces = checked_metadata_entries(&root.join("worktrees"))?;
    ensure!(
        namespaces.len() <= MAX_WORKTREES,
        "speculation worktree traversal exceeds bound"
    );
    for namespace in namespaces {
        if checked_metadata_entries(&namespace)?
            .iter()
            .any(|path| protected(path))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Used by current speculation admission. Pending legacy checkpoint journals are also unresolved
/// evidence; do not run the old writer's recovery to remove them before admitting new work.
pub(crate) fn require_settled(
    root: &Path,
    writer: &WorkspaceWriterLease,
    physical: &PhysicalEffectLease,
) -> Result<()> {
    if let Some(plan) = plan_existing(root)? {
        if let Some(binding) = plan.open_existing()? {
            binding.with_transaction_lock(writer, physical, || {
                require_no_checkpoint_journal(binding.namespace_dir())?;
                require_settled_speculation(binding.namespace_dir()).map(|_| ())
            })?;
        }
    }
    Ok(())
}

fn require_no_checkpoint_journal(namespace: &Path) -> Result<()> {
    let path = namespace.join("journal.json");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("inspecting legacy checkpoint journal; evidence retained"),
        Ok(_) => bail!("legacy checkpoint transaction journal is unresolved; source and evidence retained, no automatic recovery ran"),
    }
}

pub(crate) fn require_settled_speculation(namespace: &Path) -> Result<SpeculationInspection> {
    for name in ["ledger.json", "journal.json", "transaction.lock"] {
        let path = namespace.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => NativeResource::resolve(&path)?.verify()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("inspecting legacy metadata; evidence retained")
            }
        }
    }
    inspection::inspect_speculation_namespace(namespace).with_context(|| format!(
        "Workspace mutation refused: speculative integration evidence at {} is unresolved or unsafe. \
         Source and evidence were retained; no automatic rewrite/rollback ran. Inspect `aizen time recovery --help` \
         for the explicit evidence inspection route; selective reconciliation is not implemented. Do not delete the evidence to bypass this guard",
        namespace.display()))
}

#[cfg(test)]
include!("legacy_fixtures.rs");
#[cfg(test)]
mod tests {
    include!("legacy_tests.rs");
}
