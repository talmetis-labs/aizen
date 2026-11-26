//! Frozen task handoff selection for approved fast-forward integration.
//! Registry metadata selects a candidate; fresh Git inventory and object queries prove its state.
use super::super::owned::TaskHandoff;
use super::*;
use crate::features::coop::FrozenGitTask;

#[derive(Debug)]
pub(super) struct FrozenHandoff {
    pub identity: WorkspaceIdentity,
    pub evidence: TaskHandoff,
    registration: FrozenGitTask,
    witnesses: Vec<NativeResource>,
}

impl FrozenHandoff {
    pub fn select(destination: &WorkspaceIdentity, id: &str) -> Result<Self> {
        let registration = FrozenGitTask::select(destination, id)?;
        let manifest = &registration.manifest;
        let task = manifest
            .git_task
            .as_ref()
            .context("missing Git task state")?;
        let branch = manifest
            .branch
            .as_ref()
            .context("task handoff has no branch")?;
        let source = inventory_source(
            destination,
            Path::new(&manifest.root),
            branch,
            &task.tip,
            None,
        )?;
        let identity = inspect_identity(&source, None)?;
        ensure!(
            identity.repo_id == destination.repo_id
                && identity.repo_id == manifest.repo_id
                && identity.worktree_id == manifest.worktree_id
                && identity.worktree_id != destination.worktree_id
                && identity.canonical_root != destination.canonical_root
                && identity.canonical_common_git_dir == destination.canonical_common_git_dir
                && identity.canonical_root == Path::new(&manifest.root),
            "task handoff source is not a distinct worktree of this repository"
        );
        let witnesses = [
            &identity.canonical_root,
            identity
                .canonical_git_dir
                .as_ref()
                .context("missing source Git directory")?,
            identity
                .canonical_common_git_dir
                .as_ref()
                .context("missing source common directory")?,
        ]
        .into_iter()
        .map(|path| NativeResource::resolve(&path.join(".aizen-handoff-boundary")))
        .collect::<Result<Vec<_>>>()?;
        let evidence = TaskHandoff {
            repo_id: identity.repo_id.clone(),
            worktree_id: identity.worktree_id.clone(),
            root: identity.canonical_root.clone(),
            branch: branch.clone(),
            owner: task.owner.clone(),
            registration: registration.receipt.clone(),
            base: task.base.clone(),
            tip: task.tip.clone(),
            commits: vec![task.tip.clone()],
            paths: task.paths.clone(),
            dirty: false,
            verified: false,
            integrated: false,
            tests: if task.inverse.is_some() {
                "not_run_selected_inverse"
            } else {
                "not_run_native_only"
            }
            .into(),
            inverse: task.inverse.clone(),
        };
        validate_evidence(&evidence, id)?;
        let selected = Self {
            identity,
            evidence,
            registration,
            witnesses,
        };
        selected.revalidate(destination, None)?;
        Ok(selected)
    }

    pub fn revalidate(&self, destination: &WorkspaceIdentity, owner: Option<&Owner>) -> Result<()> {
        self.registration.revalidate()?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        let source = inventory_source(
            destination,
            &self.evidence.root,
            &self.evidence.branch,
            &self.evidence.tip,
            owner,
        )?;
        ensure!(
            inspect_identity(&source, owner)? == self.identity,
            "task worktree identity changed"
        );
        require_clean(&self.identity, owner)?;
        require_no_pending(&self.identity)?;
        crate::features::coop::require_no_other_workspace_owner(&self.identity)?;
        ensure!(
            head(&source, owner)? == self.evidence.tip
                && source_branch(&source, owner)?.as_ref() == Some(&self.evidence.branch)
                && retire::direct_tip(&source, &self.evidence.branch, owner)? == self.evidence.tip,
            "task branch/HEAD moved; prepare integration again"
        );
        ensure!(
            parents(&source, &self.evidence.tip, owner)? == vec![self.evidence.base.clone()],
            "task handoff is not exactly one commit on its recorded base"
        );
        let paths = super::super::owned::nul_paths(&query(
            &source,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "-z",
                self.evidence.base.as_str(),
                self.evidence.tip.as_str(),
                "--",
            ],
            owner,
        )?)?;
        ensure!(
            paths == self.evidence.paths,
            "task commit paths differ from its handoff"
        );
        self.registration.revalidate()?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        Ok(())
    }
}

/// Validate bounded historical evidence without following stored paths or granting replay authority.
pub(super) fn validate_evidence(evidence: &TaskHandoff, id: &str) -> Result<()> {
    let valid_key = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    ensure!(
        id.starts_with("git-task-")
            && valid_operation_id(id)
            && evidence.registration.registration_id == id
            && valid_key(&evidence.repo_id)
            && valid_key(&evidence.worktree_id)
            && !evidence.owner.is_empty()
            && evidence.owner.len() <= 1000
            && evidence.owner.trim() == evidence.owner
            && !evidence.owner.chars().any(char::is_control)
            && evidence.registration.owner == evidence.owner
            && evidence.registration.manifest_sha256 != [0; 32]
            && evidence.root.is_absolute()
            && evidence.root.to_string_lossy().len() <= 4096
            && evidence.branch.starts_with("refs/heads/")
            && evidence.branch.len() <= 256
            && !evidence.branch.chars().any(char::is_control)
            && evidence.base != evidence.tip
            && evidence.base.as_str().len() == evidence.tip.as_str().len()
            && evidence.commits == vec![evidence.tip.clone()]
            && !evidence.dirty
            && !evidence.verified
            && !evidence.integrated
            && match &evidence.inverse {
                Some(inverse) =>
                    evidence.tests == "not_run_selected_inverse"
                        && inverse.validate().is_ok()
                        && evidence
                            .paths
                            .iter()
                            .all(|path| inverse.paths.contains(path)),
                None => evidence.tests == "not_run_native_only",
            }
            && !evidence.paths.is_empty()
            && evidence.paths.len() <= 32
            && evidence.paths.windows(2).all(|paths| paths[0] < paths[1]),
        "invalid or empty task integration handoff"
    );
    // Leave room for the operation envelope within the bounded 128 KiB receipt reader.
    ensure!(
        serde_json::to_vec_pretty(evidence)?.len() <= 64 * 1024,
        "task handoff exceeds the receipt evidence budget"
    );
    NativeResource::validate_spelling(&evidence.root)?;
    for path in &evidence.paths {
        super::super::owned::validate_relative(path)?;
    }
    Ok(())
}

/// Check receipt links without opening a historical source worktree or its registration.
/// Old operation records remain readable after the source has legitimately moved or been removed.
pub(super) fn validate_receipt(receipt: &ChangeReceipt) -> Result<()> {
    match (&receipt.change, &receipt.handoff) {
        (Change::IntegrateTask { receipt: id }, Some(evidence)) => {
            validate_evidence(evidence, id)?;
            let destination = WorkspaceIdentity::from_bound_paths(
                receipt.root.clone(),
                Some(receipt.git_dir.clone()),
                Some(receipt.common_dir.clone()),
            );
            ensure!(
                receipt.schema == 2
                    // A verified receipt must name the record that proved its destination; an
                    // unverified one must not.
                    && receipt.verified == receipt.verification.is_some()
                    && evidence.repo_id == destination.repo_id
                    && evidence.worktree_id != destination.worktree_id
                    && evidence.root != destination.canonical_root
                    && evidence.branch != receipt.branch_before
                    && evidence.base == receipt.before
                    && receipt.targets.as_slice() == std::slice::from_ref(&evidence.tip)
                    && receipt.resulting_commits.is_empty(),
                "task integration receipt does not match its frozen handoff"
            );
        }
        (Change::IntegrateTask { .. }, None) => {
            anyhow::bail!("task integration receipt lost its handoff")
        }
        (_, Some(_)) => anyhow::bail!("unrelated Git operation carries task handoff evidence"),
        (_, None) => ensure!(
            // Only a task integration can be destination-verified; no other outcome may carry a
            // verification link or claim a machine-checked pass.
            receipt.verification.is_none() && !receipt.verified,
            "unrelated Git operation carries destination verification evidence"
        ),
    }
    Ok(())
}

fn inventory_source(
    destination: &WorkspaceIdentity,
    recorded_root: &Path,
    branch: &str,
    tip: &GitOid,
    owner: Option<&Owner>,
) -> Result<PathBuf> {
    ensure!(
        recorded_root.is_absolute(),
        "task handoff root must be absolute"
    );
    let key = crate::core::config::normalize_path_key(recorded_root);
    let matches: Vec<_> = parse_worktrees(&query(
        &destination.canonical_root,
        &["worktree", "list", "--porcelain", "-z"],
        owner,
    )?)?
    .into_iter()
    .filter(|entry| crate::core::config::normalize_path_key(Path::new(&entry.path)) == key)
    .collect();
    ensure!(
        matches.len() == 1,
        "task handoff source is missing or ambiguous in Git inventory"
    );
    let entry = &matches[0];
    ensure!(
        !entry.bare
            && !entry.detached
            && entry.locked.is_none()
            && entry.prunable.is_none()
            && entry.branch.as_deref() == Some(branch)
            && entry.head.as_ref() == Some(tip),
        "task handoff inventory state changed or is unavailable"
    );
    // Only the inventory path is inspected, never an arbitrary path from a stored manifest.
    let source = PathBuf::from(&entry.path);
    ensure!(
        source.is_absolute(),
        "Git inventory returned a relative source"
    );
    Ok(source)
}
