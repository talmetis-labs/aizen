//! One prepared Git worktree creation, not a source snapshot or agent ownership grant.

use super::{
    checked_output, parse_worktrees, prepare_inspection, prepare_mutation, INSPECT_TIMEOUT,
};
use crate::core::snapshots::GitOid;
use crate::core::workspace_txn::{
    BackgroundProcessBarrier, NativeResource, PhysicalEffectLease, WorkspaceIdentity,
    WorkspaceWriterLease,
};
use anyhow::{ensure, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub(super) type Owner = Arc<OperationOwner>;

pub(super) struct OperationOwner {
    pub writer: WorkspaceWriterLease,
    /// Additional worktree gates retained when one controller action spans multiple checkouts.
    pub _additional_writers: Vec<WorkspaceWriterLease>,
    pub _background: BackgroundProcessBarrier,
    pub registration: Option<crate::features::coop::WorkspaceRegistrationBarrier>,
}

impl OperationOwner {
    pub fn verify_registration(&self) -> Result<()> {
        if let Some(registration) = &self.registration {
            registration.verify()?;
        }
        Ok(())
    }
}

pub(crate) fn workspace_identity(root: &Path) -> Result<WorkspaceIdentity> {
    inspect_identity(root, None)
}

/// In-memory intent. Preparing does not create directories or branches. Consuming it does not
/// authorize a later commit, cleanup, retry, or automatic adoption by an agent.
#[derive(Debug)]
pub(crate) struct PreparedWorktreeCreation {
    identity: WorkspaceIdentity,
    witnesses: Vec<NativeResource>,
    parent_witness: NativeResource,
    target: PathBuf,
    branch: String,
    source_branch: Option<String>,
    source_head: GitOid,
    base: GitOid,
}

#[derive(Debug)]
pub(crate) struct CreatedWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: GitOid,
    /// Includes ignored/untracked files left by checkout hooks; never assume creation is clean.
    pub dirty: bool,
}

impl PreparedWorktreeCreation {
    pub(crate) fn prepare(root: &Path, target: &Path, branch: &str) -> Result<Self> {
        Self::prepare_from(root, target, branch, "HEAD")
    }

    /// Freeze a revision once; later source-ref movement never changes the approved base.
    pub(crate) fn prepare_from(
        root: &Path,
        target: &Path,
        branch: &str,
        revision: &str,
    ) -> Result<Self> {
        crate::agent::planning::check_automation_boundary()?;
        let identity = inspect_identity(root, None)?;
        super::change::require_unmodified_history(&identity, None)?;
        let witnesses = [
            &identity.canonical_root,
            identity
                .canonical_git_dir
                .as_ref()
                .context("missing Git directory")?,
            identity
                .canonical_common_git_dir
                .as_ref()
                .context("missing common Git directory")?,
        ]
        .into_iter()
        .map(|directory| NativeResource::resolve(&directory.join(".aizen-git-boundary")))
        .collect::<Result<Vec<_>>>()?;
        let target = crate::sandbox::request::planned_directory(target)?;
        require_absent(&target)?;
        ensure!(
            !crate::sandbox::request::paths_overlap(&target, &identity.canonical_root),
            "new worktree must be outside the source checkout"
        );
        let parent = target
            .parent()
            .context("worktree destination has no parent")?;
        let (_, parent_witness) =
            crate::core::speculation::namespace::plan_metadata_directory(parent)?;
        check_new_branch(&identity.canonical_root, branch, None)?;
        let source_head = head(&identity.canonical_root, None)?;
        let base = super::change::resolve(&identity.canonical_root, revision, None)?;
        let source_branch = source_branch(&identity.canonical_root, None)?;
        let plan = Self {
            identity,
            witnesses,
            parent_witness,
            target,
            branch: branch.into(),
            source_branch,
            source_head,
            base,
        };
        plan.revalidate(None)?;
        Ok(plan)
    }

    pub(crate) fn preview(&self) -> Result<String> {
        self.revalidate(None)?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "action": "create_worktree",
            "source": self.identity.canonical_root,
            "source_branch": self.source_branch,
            "source_head": self.source_head,
            "destination": self.target,
            "branch": self.branch,
            "base": self.base,
            "effects": "Create a new branch and worktree at the frozen commit; configured checkout hooks may run. Pending parent edits and ignored files are not copied. Partial effects are retained on failure.",
            "ownership": "No agent ownership, automatic commit authority, or session focus is granted."
        }))?)
    }

    pub(crate) fn execute(self) -> Result<CreatedWorktree> {
        self.execute_inner(None).map(|(created, _)| created)
    }

    pub(super) fn execute_for_task(self, scope: &str) -> Result<super::owned::OwnedTask> {
        let (_, task) = self.execute_inner(Some(scope))?;
        task.context("owned worktree registration was not created")
    }

    fn execute_inner(
        self,
        task_scope: Option<&str>,
    ) -> Result<(CreatedWorktree, Option<super::owned::OwnedTask>)> {
        let cancel = crate::core::cancel::current();
        let writer = WorkspaceWriterLease::acquire_identity(
            self.identity.clone(),
            Duration::from_secs(15),
            cancel.as_ref(),
            "create Git worktree",
        )?;
        let barrier = BackgroundProcessBarrier::acquire(cancel.as_ref())?;
        let owner = Arc::new(OperationOwner {
            writer,
            _additional_writers: Vec::new(),
            _background: barrier,
            registration: task_scope
                .map(|_| {
                    crate::features::coop::acquire_workspace_registration_barrier(&self.identity)
                })
                .transpose()?,
        });
        self.execute_with_owner(task_scope, owner)
    }

    /// A single workflow controller may retain one parent/registration owner across fresh native
    /// children. The allocator, not a model argument or branch prefix, supplies this authority.
    pub(super) fn execute_for_task_with_owner(
        self,
        scope: &str,
        owner: Owner,
    ) -> Result<super::owned::OwnedTask> {
        ensure!(
            owner.writer.identity() == &self.identity && owner.registration.is_some(),
            "shared task allocator has a different parent or no registration authority"
        );
        owner.verify_registration()?;
        let (_, task) = self.execute_with_owner(Some(scope), owner)?;
        task.context("owned worktree registration was not created")
    }

    fn execute_with_owner(
        self,
        task_scope: Option<&str>,
        owner: Owner,
    ) -> Result<(CreatedWorktree, Option<super::owned::OwnedTask>)> {
        let cancel = crate::core::cancel::current();
        self.revalidate(Some(&owner))?;
        let scope = crate::core::exec_ctx::current()
            .map(|ctx| ctx.resource_scope())
            .unwrap_or_else(|| "default".into());
        let destination_witness = {
            // Host directory effects must settle before the runner takes its nonreentrant gate.
            let _physical = PhysicalEffectLease::exclusive(
                Duration::from_secs(15),
                cancel.as_ref(),
                "prepare worktree directory",
                &scope,
            )?;
            self.verify_witnesses()?;
            require_absent(&self.target)?;
            let parent = self
                .target
                .parent()
                .context("worktree destination has no parent")?;
            let parent_guard =
                crate::core::speculation::namespace::create_metadata_directory(parent)?;
            self.parent_witness.verify()?;
            parent_guard.verify()?;
            // Atomic reservation: even an existing empty directory is not ours to adopt.
            fs::create_dir(&self.target)
                .context("reserving new worktree directory; no existing destination is reused")?;
            let guard = NativeResource::resolve(&self.target.join(".aizen-git-boundary"))?;
            parent_guard.verify()?;
            guard
        };
        let created = self.execute_reserved(&owner, &destination_witness).with_context(|| format!(
            "worktree creation at {:?} on {:?} did not complete verification; directory/branch or hook effects may remain; inspect before retrying (no automatic cleanup)",
            self.target, self.branch,
        ))?;
        let task = task_scope
            .map(|scope| super::owned::OwnedTask::from_created(&created, scope, &owner))
            .transpose()
            .with_context(|| {
                format!(
                    "owned worktree registration at {} on {} (base {}) failed; no task authority was returned; retain worktree and evidence; no automatic retry, adoption or cleanup",
                    created.path.display(),
                    created.branch,
                    created.base.as_str(),
                )
            })?;
        Ok((created, task))
    }

    fn execute_reserved(
        &self,
        owner: &Owner,
        destination_witness: &NativeResource,
    ) -> Result<CreatedWorktree> {
        self.verify_witnesses()?;
        destination_witness.verify()?;
        ensure!(
            fs::read_dir(&self.target)?.next().is_none(),
            "reserved worktree directory is no longer empty"
        );
        self.require_source_unchanged(Some(owner))?;
        check_new_branch(&self.identity.canonical_root, &self.branch, Some(owner))?;
        let prepared = prepare_mutation(
            &self.target,
            &self.identity.canonical_root,
            vec![
                "-C".into(),
                git_path(&self.identity.canonical_root)?,
                "worktree".into(),
                "add".into(),
                "--no-track".into(),
                "-b".into(),
                self.branch.clone(),
                "--".into(),
                git_path(&self.target)?,
                self.base.as_str().into(),
            ],
        )?;
        self.verify_witnesses()?;
        destination_witness.verify()?;
        checked_output(prepared.output_bounded_owned(
            None,
            INSPECT_TIMEOUT,
            Duration::from_secs(2),
            Some(Box::new(Arc::clone(owner))),
        ))?;
        self.verify_witnesses()?;
        destination_witness.verify()?;
        self.require_source_unchanged(Some(owner))?;
        let entries = parse_worktrees(&query(
            &self.identity.canonical_root,
            &["worktree", "list", "--porcelain", "-z"],
            Some(owner),
        )?)?;
        let target = fs::canonicalize(&self.target)?;
        let entry = entries
            .iter()
            .find(|entry| fs::canonicalize(&entry.path).is_ok_and(|p| p == target))
            .context("created directory is not registered with Git")?;
        let full_ref = format!("refs/heads/{}", self.branch);
        ensure!(
            entry.branch.as_deref() == Some(full_ref.as_str())
                && entry.head.as_ref() == Some(&self.base)
                && !entry.bare
                && !entry.detached
                && entry.locked.is_none()
                && entry.prunable.is_none(),
            "created worktree does not match prepared branch/base"
        );
        let actual = inspect_identity(&target, Some(owner))?;
        ensure!(
            actual.canonical_root == target
                && actual.canonical_common_git_dir == self.identity.canonical_common_git_dir,
            "created worktree belongs to a different repository"
        );
        super::require_idle_operation(
            actual
                .canonical_git_dir
                .as_ref()
                .context("created Git directory is missing")?,
        )?;
        super::validate_visible_index(&query(
            &target,
            &["ls-files", "-v", "-f", "-z"],
            Some(owner),
        )?)?;
        let dirty = !query(
            &target,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
            Some(owner),
        )?
        .is_empty();
        destination_witness.verify()?;
        Ok(CreatedWorktree {
            path: target,
            branch: self.branch.clone(),
            base: self.base.clone(),
            dirty,
        })
    }

    fn verify_witnesses(&self) -> Result<()> {
        self.parent_witness.verify()?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        Ok(())
    }

    fn require_source_unchanged(&self, owner: Option<&Owner>) -> Result<()> {
        super::change::require_unmodified_history(&self.identity, owner)?;
        ensure!(
            inspect_identity(&self.identity.canonical_root, owner)? == self.identity,
            "source repository binding changed"
        );
        ensure!(
            head(&self.identity.canonical_root, owner)? == self.source_head,
            "source HEAD moved; prepare creation again"
        );
        ensure!(
            super::change::resolve(&self.identity.canonical_root, self.base.as_str(), owner)?
                == self.base,
            "frozen base commit is no longer available"
        );
        ensure!(
            source_branch(&self.identity.canonical_root, owner)? == self.source_branch,
            "source branch changed; prepare creation again"
        );
        Ok(())
    }

    fn revalidate(&self, owner: Option<&Owner>) -> Result<()> {
        self.verify_witnesses()?;
        require_absent(&self.target)?;
        self.require_source_unchanged(owner)?;
        check_new_branch(&self.identity.canonical_root, &self.branch, owner)?;
        let entries = parse_worktrees(&query(
            &self.identity.canonical_root,
            &["worktree", "list", "--porcelain", "-z"],
            owner,
        )?)?;
        for entry in entries {
            let path = crate::sandbox::request::planned_directory(Path::new(&entry.path))?;
            ensure!(
                !crate::sandbox::request::paths_overlap(&path, &self.target),
                "destination overlaps a registered worktree (possibly missing or prunable)"
            );
        }
        Ok(())
    }
}

pub(super) fn query(root: &Path, args: &[&str], owner: Option<&Owner>) -> Result<Vec<u8>> {
    if let Some(owner) = owner {
        owner.verify_registration()?;
    }
    let prepared = prepare_inspection(root, args.iter().map(|s| (*s).into()).collect())?;
    // The caller's barriers must survive unknown settlement of inspection workers too.
    let output = checked_output(prepared.output_bounded_owned(
        None,
        INSPECT_TIMEOUT,
        Duration::from_secs(2),
        owner.map(|owner| Box::new(Arc::clone(owner)) as crate::core::proctree::BoundedOwner),
    ))?;
    if let Some(owner) = owner {
        owner.verify_registration()?;
    }
    Ok(output.stdout)
}

fn line(bytes: &[u8]) -> Result<&str> {
    let value = std::str::from_utf8(bytes).context("Git identity is not UTF-8")?;
    let value = value
        .strip_suffix('\n')
        .context("incomplete Git identity output")?;
    let value = value.strip_suffix('\r').unwrap_or(value);
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "unsupported Git identity output"
    );
    Ok(value)
}

pub(super) fn inspect_identity(root: &Path, owner: Option<&Owner>) -> Result<WorkspaceIdentity> {
    let path = |flag| -> Result<PathBuf> {
        let bytes = query(root, &["rev-parse", "--path-format=absolute", flag], owner)?;
        let path = PathBuf::from(line(&bytes)?);
        ensure!(path.is_absolute(), "Git returned a relative identity path");
        let path = crate::sandbox::request::planned_directory(&path)?;
        ensure!(path.is_dir(), "Git identity directory is missing");
        Ok(path)
    };
    Ok(WorkspaceIdentity::from_bound_paths(
        path("--show-toplevel")?,
        Some(path("--absolute-git-dir")?),
        Some(path("--git-common-dir")?),
    ))
}

pub(super) fn head(root: &Path, owner: Option<&Owner>) -> Result<GitOid> {
    let bytes = query(
        root,
        &["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
        owner,
    )?;
    GitOid::parse(line(&bytes)?)
}

pub(super) fn source_branch(root: &Path, owner: Option<&Owner>) -> Result<Option<String>> {
    let bytes = query(root, &["rev-parse", "--symbolic-full-name", "HEAD"], owner)?;
    let reference = line(&bytes)?;
    if reference == "HEAD" {
        return Ok(None);
    }
    ensure!(
        reference.starts_with("refs/heads/"),
        "source HEAD is not a local branch"
    );
    Ok(Some(reference.into()))
}

fn check_new_branch(root: &Path, branch: &str, owner: Option<&Owner>) -> Result<()> {
    ensure!(
        !branch.is_empty()
            && branch.len() <= 256
            && !branch.starts_with('-')
            && !branch.chars().any(char::is_control),
        "invalid new worktree branch"
    );
    let reference = format!("refs/heads/{branch}");
    query(root, &["check-ref-format", &reference], owner)?;
    ensure!(
        query(
            root,
            &["for-each-ref", "--format=%(refname)", &reference],
            owner
        )?
        .is_empty(),
        "worktree branch already exists; it will not be reset or reused"
    );
    Ok(())
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("checking worktree destination"),
        Ok(_) => anyhow::bail!("worktree destination already exists; nothing will be overwritten"),
    }
}

fn git_path(path: &Path) -> Result<String> {
    let value = path.to_str().context("unsupported non-UTF-8 Git path")?;
    #[cfg(windows)]
    let value = value.strip_prefix(r"\\?\").unwrap_or(value);
    Ok(value.into())
}

#[cfg(test)]
#[path = "git_ops_create_tests.rs"]
mod tests;
