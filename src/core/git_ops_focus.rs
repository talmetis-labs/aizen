//! Prepared, read-only worktree navigation. Git files/index/HEAD are never changed by focus.
use super::create::{head, inspect_identity, query, source_branch, OperationOwner, Owner};
use super::{parse_worktrees, WorktreeEntry};
use crate::core::{
    snapshots::GitOid,
    workspace_txn::{NativeResource, WorkspaceIdentity},
};
use anyhow::{ensure, Context, Result};
use std::path::{Path, PathBuf};

pub(crate) struct PreparedFocus {
    source: WorkspaceIdentity,
    target: WorkspaceIdentity,
    head: GitOid,
    branch: Option<String>,
    witnesses: Vec<NativeResource>,
}

impl PreparedFocus {
    pub(crate) fn prepare(source: &Path, target: &Path) -> Result<Self> {
        ensure!(
            target.is_absolute(),
            "focus requires an explicit absolute worktree path"
        );
        let source = inspect_identity(source, None)?;
        let target_path = target
            .canonicalize()
            .context("focus target is unavailable")?;
        let entry = target_entry(&source, &target_path, None)?;
        let target = inspect_identity(&target_path, None)?;
        ensure!(
            target.canonical_root == target_path
                && target.canonical_common_git_dir == source.canonical_common_git_dir,
            "focus target is not an exact worktree of this repository"
        );
        let witnesses = [&source, &target]
            .into_iter()
            .flat_map(|id| {
                [
                    Some(&id.canonical_root),
                    id.canonical_git_dir.as_ref(),
                    id.canonical_common_git_dir.as_ref(),
                ]
            })
            .flatten()
            .map(|path| NativeResource::resolve(&path.join(".aizen-focus-boundary")))
            .collect::<Result<Vec<_>>>()?;
        let prepared = Self {
            source,
            target,
            head: entry
                .head
                .context("focus requires a committed worktree HEAD")?,
            branch: entry.branch,
            witnesses,
        };
        prepared.revalidate()?;
        Ok(prepared)
    }

    pub(crate) fn source(&self) -> &WorkspaceIdentity {
        &self.source
    }
    pub(crate) fn target(&self) -> &WorkspaceIdentity {
        &self.target
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        self.revalidate_owned(None)
    }

    /// Once foreground ownership is taken, every inspection runner retains all gates if its
    /// physical completion becomes unknown. Dropping the observer never releases these early.
    pub(crate) fn hold(&self) -> Result<FocusLease> {
        use crate::core::workspace_txn::{BackgroundProcessBarrier, WorkspaceWriterLease};
        let cancel = crate::core::cancel::current();
        let writer = WorkspaceWriterLease::acquire_identity(
            self.source.clone(),
            std::time::Duration::ZERO,
            cancel.as_ref(),
            "session worktree focus",
        )?;
        let target_writer = WorkspaceWriterLease::acquire_identity(
            self.target.clone(),
            std::time::Duration::ZERO,
            cancel.as_ref(),
            "session worktree focus",
        )?;
        let owner = std::sync::Arc::new(OperationOwner {
            writer,
            _additional_writers: vec![target_writer],
            _background: BackgroundProcessBarrier::acquire(cancel.as_ref())?,
            registration: Some(
                crate::features::coop::acquire_workspace_registration_barrier(&self.target)?,
            ),
        });
        self.revalidate_owned(Some(&owner))?;
        Ok(FocusLease(owner))
    }

    fn revalidate_owned(&self, owner: Option<&Owner>) -> Result<()> {
        for witness in &self.witnesses {
            witness.verify()?;
        }
        ensure!(
            inspect_identity(&self.source.canonical_root, owner)? == self.source
                && inspect_identity(&self.target.canonical_root, owner)? == self.target,
            "focus workspace identity changed; prepare again"
        );
        let entry = target_entry(&self.source, &self.target.canonical_root, owner)?;
        ensure!(
            entry.head.as_ref() == Some(&self.head)
                && entry.branch == self.branch
                && head(&self.target.canonical_root, owner)? == self.head
                && source_branch(&self.target.canonical_root, owner)? == self.branch,
            "focus target branch/HEAD moved; prepare again"
        );
        for witness in &self.witnesses {
            witness.verify()?;
        }
        Ok(())
    }
}

pub(crate) struct FocusLease(Owner);
impl FocusLease {
    pub(crate) fn revalidate(&self, prepared: &PreparedFocus) -> Result<()> {
        prepared.revalidate_owned(Some(&self.0))
    }
    pub(crate) fn writer(&self, target: bool) -> &crate::core::workspace_txn::WorkspaceWriterLease {
        if target {
            &self.0._additional_writers[0]
        } else {
            &self.0.writer
        }
    }
    pub(crate) fn registration(&self) -> &crate::features::coop::WorkspaceRegistrationBarrier {
        self.0
            .registration
            .as_ref()
            .expect("focus retains registration")
    }
}

fn target_entry(
    source: &WorkspaceIdentity,
    target: &Path,
    owner: Option<&Owner>,
) -> Result<WorktreeEntry> {
    let mut matches = Vec::new();
    for entry in parse_worktrees(&query(
        &source.canonical_root,
        &["worktree", "list", "--porcelain", "-z"],
        owner,
    )?)? {
        if PathBuf::from(&entry.path)
            .canonicalize()
            .is_ok_and(|path| path == target)
        {
            matches.push(entry);
        }
    }
    ensure!(
        matches.len() == 1,
        "focus target is absent or ambiguous in Git worktree inventory"
    );
    let entry = matches.pop().unwrap();
    ensure!(
        !entry.bare && entry.locked.is_none() && entry.prunable.is_none(),
        "focus target is bare, locked or prunable"
    );
    Ok(entry)
}
