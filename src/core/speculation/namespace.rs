//! Existing historical speculative evidence, independent of checkpoint history.
//!
//! This adapter never creates a speculative namespace. Generic checked metadata-directory helpers
//! remain shared by real Git operations; they do not create or authorize speculative source stores.

use super::inspection::{self, RecoveryNamespace, SpeculationInspection};
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::{
    NativeResource, PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
};
use anyhow::{ensure, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct PlannedNamespace {
    identity: WorkspaceIdentity,
    path: PathBuf,
    ancestor: NativeResource,
    boundaries: Vec<NativeResource>,
}

#[derive(Debug, Clone)]
pub(crate) struct NativeNamespace {
    plan: PlannedNamespace,
    directory: NativeResource,
}

impl PlannedNamespace {
    /// Read-only: bind existing ancestors, never initialize checkpoint storage or a registry.
    pub(crate) fn plan(identity: WorkspaceIdentity, home: &Path) -> Result<Self> {
        ensure!(
            !identity.worktree_id.is_empty()
                && identity
                    .worktree_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid native namespace identity"
        );
        let (path, ancestor) =
            plan_metadata_directory(&home.join("native-workspaces").join(&identity.worktree_id))?;
        ensure!(
            !path.starts_with(&identity.canonical_root)
                && !identity.canonical_root.starts_with(&path),
            "native speculative metadata must be outside the live workspace"
        );
        ensure!(
            identity.canonical_git_dir.is_some() == identity.canonical_common_git_dir.is_some(),
            "incomplete native workspace Git identity"
        );
        let boundaries = std::iter::once(&identity.canonical_root)
            .chain(identity.canonical_git_dir.iter())
            .chain(identity.canonical_common_git_dir.iter())
            .map(|path| NativeResource::resolve(&path.join(".speculation-boundary")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            identity,
            path,
            ancestor,
            boundaries,
        })
    }

    fn verify(&self) -> Result<()> {
        self.ancestor.verify()?;
        for boundary in &self.boundaries {
            boundary.verify()?;
        }
        ensure!(
            WorkspaceIdentity::discover(&self.identity.canonical_root)? == self.identity,
            "native workspace identity changed after planning"
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
            "native metadata requires matching writer and exclusive physical admission"
        );
        self.verify()
    }

    /// Existing evidence only. Absence does not create directories or authorize a later mutation.
    pub(crate) fn open_existing(&self) -> Result<Option<NativeNamespace>> {
        self.verify()?;
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
            Ok(_) => {
                let (path, directory) = plan_metadata_directory(&self.path)?;
                ensure!(path == self.path, "native namespace path changed");
                Ok(Some(NativeNamespace {
                    plan: self.clone(),
                    directory,
                }))
            }
        }
    }
}

impl NativeNamespace {
    pub(crate) fn namespace_dir(&self) -> &Path {
        &self.plan.path
    }

    /// This namespace has no repository-wide store/GC lock. Its actual transaction lock precedes
    /// cohort-owner/native-journal locks. Never nest a legacy lock acquisition inside this body.
    fn transaction<T>(&self, timeout: Duration, body: impl FnOnce() -> Result<T>) -> Result<T> {
        self.plan.verify()?;
        self.directory.verify()?;
        let lock = NativeResource::resolve(&self.plan.path.join("transaction.lock"))?;
        let transaction = RepoTxnLock::acquire_existing_exclusive(lock.path(), timeout)?;
        transaction.verify_existing_path(lock.path())?;
        lock.verify()?;
        self.directory.verify()?;
        body()
    }

    pub(crate) fn require_settled(&self) -> Result<SpeculationInspection> {
        self.transaction(Duration::ZERO, || {
            inspection::inspect_speculation_namespace(self.namespace_dir()).context(
                "native speculative evidence is unresolved or unsafe; source and evidence retained",
            )
        })
    }
}

impl RecoveryNamespace for NativeNamespace {
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
        self.directory.verify()
    }
    fn with_transaction_lock<T>(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.verify_admission(writer, physical)?;
        self.transaction(Duration::from_secs(15), || {
            self.verify_admission(writer, physical)?;
            body()
        })
    }
}

/// Called before checkpoint mutations, including when no legacy journal exists. This does not
/// create absent namespaces; the caller retains its normal workspace/metadata exclusion.
pub(crate) fn inspect_existing(
    identity: WorkspaceIdentity,
    home: &Path,
) -> Result<SpeculationInspection> {
    match PlannedNamespace::plan(identity, home)?.open_existing()? {
        Some(namespace) => namespace.require_settled(),
        None => Ok(SpeculationInspection::default()),
    }
}

/// Bind the nearest existing non-link parent without creating the missing suffix. Shared with the
/// legacy adapter so both layouts use the same ancestor/reparse protection, not a second resolver.
pub(crate) fn plan_metadata_directory(path: &Path) -> Result<(PathBuf, NativeResource)> {
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
                ensure!(
                    metadata.is_dir()
                        && !crate::core::file_placement::is_link_or_reparse(&metadata),
                    "speculation metadata requires non-link directories"
                );
                let guard = NativeResource::resolve(&parent.join(".tm-boundary"))?;
                let mut canonical = fs::canonicalize(parent)?;
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok((canonical, guard));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    parent
                        .file_name()
                        .context("invalid speculation metadata directory")?
                        .to_os_string(),
                );
                parent = parent
                    .parent()
                    .context("speculation metadata has no existing parent")?;
            }
            Err(error) => return Err(error).context("checking speculation metadata directory"),
        }
    }
}

pub(crate) fn create_metadata_directory(path: &Path) -> Result<NativeResource> {
    let (canonical, mut guard) = plan_metadata_directory(path)?;
    ensure!(
        canonical == path,
        "speculation metadata directory is not canonical"
    );
    let parent = guard
        .path()
        .parent()
        .context("speculation metadata boundary has no parent")?;
    let suffix: Vec<_> = path.strip_prefix(parent)?.components().collect();
    let mut current = parent.to_path_buf();
    for component in suffix {
        guard.verify()?;
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("creating admitted speculation metadata directory")
            }
        }
        let next = NativeResource::resolve(&current.join(".tm-boundary"))?;
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
    anyhow::bail!("speculation owner-only directory protection is unavailable");
    Ok(())
}

#[cfg(test)]
#[path = "namespace_tests.rs"]
mod tests;
