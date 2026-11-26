//! Prepared, approved Git porcelain operations. Git owns history and conflict state.
//! Receipts contain only intent/OIDs/outcome; they never store source bytes or authorize replay.

use super::create::{head, inspect_identity, query, source_branch, OperationOwner, Owner};
use super::{
    checked_output, parse_worktrees, prepare_mutation, require_idle_operation,
    validate_visible_index, INSPECT_TIMEOUT,
};
use crate::core::{
    snapshots::GitOid,
    workspace_txn::{
        BackgroundProcessBarrier, NativeResource, PhysicalEffectLease, WorkspaceIdentity,
        WorkspaceWriterLease,
    },
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[path = "git_ops_receipts.rs"]
mod receipts;
pub(crate) use receipts::valid_id as valid_operation_id;
#[path = "git_ops_operation_inspection.rs"]
mod operation_inspection;
pub(crate) use operation_inspection::inspect_operation;
#[path = "git_ops_handoff.rs"]
mod handoff;
#[path = "git_ops_retire.rs"]
mod retire;
#[path = "git_ops_verify.rs"]
mod verification;
pub(crate) use verification::{
    accepted_task_integration, require_verified_destination, verification_records,
    PreparedVerification, VerificationRecord, VerifiedTaskIntegration,
};
#[path = "git_ops_sequencer.rs"]
mod sequencer;
pub(crate) use sequencer::PreparedSequencer;
#[path = "git_ops_inverse.rs"]
mod inverse;
pub(crate) use inverse::{PathInverse, PreparedInverse};

pub(super) fn require_workflow_ancestors(
    root: &Path,
    tip: &GitOid,
    ancestors: &[GitOid],
) -> Result<()> {
    let identity = inspect_identity(root, None)?;
    require_unmodified_history(&identity, None)?;
    ensure!(
        ancestors.len() <= 32,
        "workflow prerequisite bound exceeded"
    );
    for ancestor in ancestors {
        let range = format!("{}..{}", tip.as_str(), ancestor.as_str());
        ensure!(
            query(root, &["rev-list", "--max-count=1", &range], None)?.is_empty(),
            "workflow launch base does not contain an accepted prerequisite"
        );
    }
    Ok(())
}

pub(super) fn require_workflow_clean(identity: &WorkspaceIdentity) -> Result<()> {
    verification::require_tracked_clean(identity, None)
}

pub(crate) fn validate_task_handoff(evidence: &super::owned::TaskHandoff) -> Result<()> {
    handoff::validate_evidence(evidence, &evidence.registration.registration_id)
}

/// One approved operation: a Git mutation, or destination verification of a completed integration.
/// Both share preparation, a non-mutating preview and consume-once execution. The payloads are
/// boxed because a prepared change is far larger than a prepared verification.
#[derive(Debug)]
pub(crate) enum Prepared {
    Change(Box<PreparedChange>),
    Verify(Box<PreparedVerification>),
    Sequencer(Box<PreparedSequencer>),
    Inverse(Box<PreparedInverse>),
}

#[derive(Debug)]
pub(crate) enum PreparedOutcome {
    Change(Box<ChangeReceipt>),
    Verify(Box<VerificationRecord>),
    Handoff(Box<super::owned::TaskHandoff>),
}

impl Prepared {
    pub(crate) fn prepare(root: &Path, change: Change) -> Result<Self> {
        match change {
            Change::RevertPaths {
                commit,
                paths,
                mainline,
            } => Ok(Self::Inverse(Box::new(PreparedInverse::prepare(
                root, &commit, paths, mainline,
            )?))),
            Change::VerifyTask { receipt } => Ok(Self::Verify(Box::new(
                PreparedVerification::prepare(root, &receipt)?,
            ))),
            Change::Sequencer { receipt, step } => Ok(Self::Sequencer(Box::new(
                PreparedSequencer::prepare(root, &receipt, &step)?,
            ))),
            change => Ok(Self::Change(Box::new(PreparedChange::prepare(
                root, change,
            )?))),
        }
    }

    pub(crate) fn preview(&self) -> Result<String> {
        match self {
            Self::Change(change) => change.preview(),
            Self::Verify(verification) => verification.preview(),
            Self::Sequencer(sequencer) => sequencer.preview(),
            Self::Inverse(inverse) => inverse.preview(),
        }
    }

    pub(crate) fn execute(self) -> Result<PreparedOutcome> {
        match self {
            Self::Change(change) => Ok(PreparedOutcome::Change(Box::new(change.execute()?))),
            Self::Verify(verification) => {
                Ok(PreparedOutcome::Verify(Box::new(verification.execute()?)))
            }
            Self::Inverse(inverse) => Ok(PreparedOutcome::Handoff(Box::new(inverse.execute()?))),
            Self::Sequencer(sequencer) => {
                Ok(PreparedOutcome::Change(Box::new(sequencer.execute()?)))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Change {
    Switch {
        branch: String,
    },
    Revert {
        commits: Vec<String>,
        mainline: Option<u32>,
    },
    /// Build a selected inverse in a fresh owned checkout; never change the destination here.
    RevertPaths {
        commit: String,
        paths: Vec<String>,
        mainline: Option<u32>,
    },
    /// Invert only commits mapped by one completed undo receipt, in reverse execution order.
    Redo {
        receipt: String,
    },
    /// Fast-forward only; a merge commit or ordered cherry-pick must be selected explicitly.
    Integrate {
        commit: String,
    },
    /// Fast-forward an exact registered task handoff from its recorded base.
    IntegrateTask {
        receipt: String,
    },
    /// Run frozen verification commands at the exact destination of a completed task integration.
    VerifyTask {
        receipt: String,
    },
    /// Create a two-parent merge commit; no unrelated histories, squash or implicit fast-forward.
    Merge {
        commit: String,
    },
    /// Apply an explicitly ordered frozen list, recording each source/result commit mapping.
    CherryPick {
        commits: Vec<String>,
        mainline: Option<u32>,
    },
    /// Acknowledge a physically settled failure after manual Git resolution; never replay it.
    Reconcile {
        receipt: String,
    },
    /// Delete only an un-checked-out local branch whose commits remain reachable elsewhere.
    DeleteBranch {
        branch: String,
        retained_ref: String,
    },
    /// Continue, abort or quit a recognized merge/cherry-pick/revert owned by one failed receipt.
    Sequencer {
        receipt: String,
        step: String,
    },
}

#[derive(Debug)]
pub(crate) struct PreparedChange {
    identity: WorkspaceIdentity,
    witnesses: Vec<NativeResource>,
    before: GitOid,
    branch: String,
    change: Change,
    targets: Vec<GitOid>,
    undo: Option<receipts::FrozenUndo>,
    reconcile: Option<receipts::FrozenFailure>,
    handoff: Option<handoff::FrozenHandoff>,
}

/// An explicit acknowledgement of manual resolution, not a claim that the original work succeeded.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconciliation {
    prior_state: String,
    observed_head: GitOid,
    branch: String,
    recorded_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeReceipt {
    schema: u32,
    pub id: String,
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    before: GitOid,
    pub after: Option<GitOid>,
    branch_before: String,
    change: Change,
    targets: Vec<GitOid>,
    pub state: String,
    pub resulting_commits: Vec<GitOid>,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reconciliation: Option<Reconciliation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<super::owned::TaskHandoff>,
    /// The verification record that proved this exact destination, once one passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verification: Option<String>,
    /// Exact marker instance left by a settled failed controller command. Legacy records grant
    /// no sequencer authority; matching a commit OID alone cannot identify who started a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequencer: Option<sequencer::ConflictEvidence>,
}

impl ChangeReceipt {
    /// The recorded action, for the module's own tests and the shared admission rules.
    #[cfg(test)]
    pub(crate) fn action(&self) -> &Change {
        &self.change
    }
}

impl PreparedChange {
    pub(crate) fn prepare(root: &Path, change: Change) -> Result<Self> {
        require_parent()?;
        let identity = inspect_identity(root, None)?;
        require_unmodified_history(&identity, None)?;
        let root = &identity.canonical_root;
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
        .map(|p| NativeResource::resolve(&p.join(".aizen-git-boundary")))
        .collect::<Result<Vec<_>>>()?;
        let before = head(root, None)?;
        let branch = source_branch(root, None)?
            .context("mutation requires a local branch; detached/unborn inspection is supported")?;
        let mut undo = None;
        let mut reconcile = None;
        let mut handoff = None;
        let targets = match &change {
            Change::Switch { branch } => {
                ensure!(
                    !branch.starts_with('-')
                        && !branch.contains('@')
                        && !branch.chars().any(char::is_control),
                    "invalid literal local branch"
                );
                let reference = format!("refs/heads/{branch}");
                query(root, &["check-ref-format", &reference], None)?;
                vec![resolve(root, &reference, None)?]
            }
            Change::Revert { commits, mainline } | Change::CherryPick { commits, mainline } => {
                ensure!(
                    !commits.is_empty() && commits.len() <= 32,
                    "revert/cherry-pick requires 1-32 explicitly ordered commits"
                );
                ensure!(
                    mainline.is_none_or(|n| n > 0),
                    "merge mainline must be positive"
                );
                let mut targets = Vec::new();
                for revision in commits {
                    let oid = resolve(root, revision, None)?;
                    ensure!(!targets.contains(&oid), "duplicate selected commit");
                    let parents = parents(root, &oid, None)?;
                    ensure!(
                        !parents.is_empty(),
                        "root-commit inversion/cherry-pick is not supported by this operation"
                    );
                    if parents.len() > 1 {
                        ensure!(mainline.is_some_and(|n| n as usize <= parents.len()), "selected merge commit requires explicit valid mainline; reverting it affects future merges");
                    } else {
                        ensure!(mainline.is_none(), "mainline is accepted only for a list containing merge commits exclusively");
                    }
                    targets.push(oid);
                }
                targets
            }
            Change::RevertPaths { .. } => anyhow::bail!(
                "selected-path inverses require their own prepared owned-worktree path"
            ),
            Change::Redo { receipt } => {
                let (frozen, targets) =
                    receipts::FrozenUndo::select(&identity, &branch, &before, receipt)?;
                undo = Some(frozen);
                targets
            }
            Change::Reconcile { receipt } => {
                let frozen = receipts::FrozenFailure::select(&identity, &branch, receipt)?;
                let targets = frozen.targets.clone();
                reconcile = Some(frozen);
                targets
            }
            Change::DeleteBranch {
                branch,
                retained_ref,
            } => retire::targets(root, branch, retained_ref, None)?,
            Change::IntegrateTask { receipt } => {
                let selected = handoff::FrozenHandoff::select(&identity, receipt)?;
                let targets = vec![selected.evidence.tip.clone()];
                handoff = Some(selected);
                targets
            }
            Change::Integrate { commit } | Change::Merge { commit } => {
                vec![resolve(root, commit, None)?]
            }
            // Destination verification has no Git mutation; it is prepared and executed through
            // `PreparedVerification`, never as a change with frozen targets.
            Change::VerifyTask { .. } => anyhow::bail!(
                "destination verification is not a Git mutation; prepare it through its own path"
            ),
            // Sequencer steps are prepared and executed through `PreparedSequencer`, which binds
            // them to the exact recorded failed operation that left the sequencer behind.
            Change::Sequencer { .. } => anyhow::bail!(
                "a sequencer step is not a plain Git mutation; prepare it through its own path"
            ),
        };
        let prepared = Self {
            identity,
            witnesses,
            before,
            branch,
            change,
            targets,
            undo,
            reconcile,
            handoff,
        };
        prepared.revalidate(None)?;
        Ok(prepared)
    }

    pub(crate) fn preview(&self) -> Result<String> {
        let effects = if matches!(self.change, Change::Reconcile { .. }) {
            "Acknowledge manual resolution of this exact physically settled failed operation. Updates metadata only; keeps its original outcome and commit mappings. Does not replay, undo, mark successful or verify the work. Running/unknown effects cannot be acknowledged."
        } else if matches!(self.change, Change::DeleteBranch { .. }) {
            "Deletes the selected local branch ref and its Git reflog/config; no files or worktrees are removed. Frozen targets are [deleted tip, retained tip]. Unique history is refused; no force deletion."
        } else {
            "Changes this clean worktree and may create Git commits. Configured hooks/signing remain enabled. No stash, force, reset or snapshot fallback."
        };
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "workspace":self.identity.canonical_root, "branch":self.branch, "before":self.before,
            "operation":self.change, "frozen_targets":self.targets,
            "selected_failed_operation":self.reconcile.as_ref().map(|selected| &selected.original),
            "task_handoff":self.handoff.as_ref().map(|selected| &selected.evidence),
            "effects":effects,
            "verification":"Git postconditions only; project tests must be run on the resulting destination. Conflicts/partial effects remain for inspection."
        }))?)
    }

    fn revalidate(&self, owner: Option<&Owner>) -> Result<()> {
        require_parent()?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        let root = &self.identity.canonical_root;
        ensure!(
            inspect_identity(root, owner)? == self.identity,
            "repository/worktree binding changed; prepare again"
        );
        ensure!(
            head(root, owner)? == self.before
                && source_branch(root, owner)?.as_ref() == Some(&self.branch),
            "HEAD or branch moved; approval is stale"
        );
        require_clean(&self.identity, owner)?;
        verification::require_settled(&self.identity, None)?;
        crate::features::coop::require_no_other_workspace_owner(&self.identity)?;
        if let Some(selected) = &self.reconcile {
            selected.revalidate(&self.identity, &self.branch, owner)?;
            for receipt in receipts::scan(&self.identity)? {
                if receipt.id != selected.id {
                    receipts::require_completed(&receipt)?;
                }
            }
        } else {
            require_no_pending(&self.identity)?;
        }
        match &self.change {
            Change::Switch { branch } => {
                let reference = format!("refs/heads/{branch}");
                ensure!(reference != self.branch, "already on the selected branch");
                ensure!(
                    resolve(root, &reference, owner)? == self.targets[0],
                    "target branch moved; prepare again"
                );
                for entry in parse_worktrees(&query(
                    root,
                    &["worktree", "list", "--porcelain", "-z"],
                    owner,
                )?)? {
                    ensure!(
                        entry.branch.as_ref() != Some(&reference),
                        "target branch is already checked out in a registered worktree"
                    );
                }
            }
            Change::IntegrateTask { .. } => {
                let selected = self
                    .handoff
                    .as_ref()
                    .context("missing frozen task handoff")?;
                selected.revalidate(&self.identity, owner)?;
                ensure!(
                    self.before == selected.evidence.base
                        && self.branch != selected.evidence.branch,
                    "destination is not the frozen task base or shares the source branch"
                );
            }
            Change::Integrate { .. } => {
                let range = format!("{}..{}", self.targets[0].as_str(), self.before.as_str());
                ensure!(
                    query(root, &["rev-list", "--max-count=1", &range], owner)?.is_empty(),
                    "destination is not an ancestor; fast-forward integration refused"
                );
                ensure!(
                    self.before != self.targets[0],
                    "target is already integrated"
                );
            }
            Change::Merge { .. } => {
                let target = self.targets[0].as_str();
                // An already-integrated target must not turn an approved merge into a no-op.
                let range = format!("{}..{target}", self.before.as_str());
                ensure!(
                    !query(root, &["rev-list", "--max-count=1", &range], owner)?.is_empty(),
                    "target is already integrated"
                );
                // Refuse unrelated histories even if branch.mergeOptions would otherwise allow it.
                ensure!(
                    !query(
                        root,
                        &["merge-base", "--all", self.before.as_str(), target],
                        owner
                    )?
                    .is_empty(),
                    "merge requires a shared Git ancestor"
                );
            }
            Change::Redo { .. } => self
                .undo
                .as_ref()
                .context("missing frozen undo selection")?
                .revalidate(&self.identity, &self.branch, &self.before, owner)?,
            Change::DeleteBranch { .. } => retire::revalidate(self, owner)?,
            Change::Revert { .. }
            | Change::RevertPaths { .. }
            | Change::CherryPick { .. }
            | Change::Reconcile { .. }
            | Change::VerifyTask { .. }
            | Change::Sequencer { .. } => {}
        }
        Ok(())
    }

    pub(crate) fn execute(self) -> Result<ChangeReceipt> {
        require_parent()?;
        let cancel = crate::core::cancel::current();
        let writer = WorkspaceWriterLease::acquire_identity(
            self.identity.clone(),
            Duration::from_secs(15),
            cancel.as_ref(),
            "approved Git change",
        )?;
        let mut source_writers = Vec::new();
        if let Some(selected) = &self.handoff {
            source_writers.push(WorkspaceWriterLease::acquire_identity(
                selected.identity.clone(),
                Duration::ZERO,
                cancel.as_ref(),
                "approved Git task integration source",
            )?);
        }
        let barrier = BackgroundProcessBarrier::acquire(cancel.as_ref())?;
        let registration =
            crate::features::coop::acquire_workspace_registration_barrier(&self.identity)?;
        let owner = Arc::new(OperationOwner {
            writer,
            _additional_writers: source_writers,
            _background: barrier,
            registration: Some(registration),
        });
        owner.verify_registration()?;
        self.revalidate(Some(&owner))?;
        {
            let physical = physical()?;
            crate::core::speculation::legacy::require_settled(
                &self.identity.canonical_root,
                &owner.writer,
                &physical,
            )?;
            crate::core::speculation::namespace::inspect_existing(
                self.identity.clone(),
                &crate::core::config::aizen_home(),
            )?;
            crate::core::native_journal::require_workspace_settled(&self.identity.canonical_root)?;
            if let Some(selected) = &self.handoff {
                let source_writer = owner
                    ._additional_writers
                    .first()
                    .context("task integration lost its source lease")?;
                crate::core::speculation::legacy::require_settled(
                    &selected.identity.canonical_root,
                    source_writer,
                    &physical,
                )?;
                crate::core::speculation::namespace::inspect_existing(
                    selected.identity.clone(),
                    &crate::core::config::aizen_home(),
                )?;
                crate::core::native_journal::require_workspace_settled(
                    &selected.identity.canonical_root,
                )?;
            }
        }
        if let Some(selected) = &self.reconcile {
            // No Git mutation and no new success receipt. Keep the failed intent/outcome intact.
            return selected.acknowledge(&self.identity, &self.branch, &self.before, &owner);
        }
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            crate::core::persist::unique_sequence()
        );
        let mut receipt = ChangeReceipt {
            schema: if self.handoff.is_some() { 2 } else { 1 },
            id,
            root: self.identity.canonical_root.clone(),
            git_dir: self
                .identity
                .canonical_git_dir
                .clone()
                .context("missing Git directory")?,
            common_dir: self
                .identity
                .canonical_common_git_dir
                .clone()
                .context("missing common directory")?,
            before: self.before.clone(),
            after: None,
            branch_before: self.branch.clone(),
            change: self.change.clone(),
            targets: self.targets.clone(),
            state: "prepared".into(),
            resulting_commits: Vec::new(),
            verified: false,
            reconciliation: None,
            handoff: self
                .handoff
                .as_ref()
                .map(|selected| selected.evidence.clone()),
            verification: None,
            sequencer: None,
        };
        let (directory, ancestor) = crate::core::speculation::namespace::plan_metadata_directory(
            &receipt_directory(&self.identity),
        )?;
        let path = directory.join(format!("{}.json", receipt.id));
        let witness = {
            let _physical = physical()?;
            ancestor.verify()?;
            let witness =
                crate::core::speculation::namespace::create_metadata_directory(&directory)?;
            ancestor.verify()?;
            ensure!(!path.try_exists()?, "operation receipt already exists");
            persist(&path, &receipt, &witness)?;
            witness
        };
        let mut submitted = false;
        let result = self
            .apply(&owner, &mut receipt, &path, &witness, &mut submitted)
            .and_then(|()| {
                let _physical = physical()?;
                let branch = match &self.change {
                    Change::Switch { branch } => format!("refs/heads/{branch}"),
                    _ => self.branch.clone(),
                };
                crate::features::coop::refresh_workspace_branch(
                    &self.identity,
                    Some(&branch),
                    owner
                        .registration
                        .as_ref()
                        .context("missing registration barrier")?,
                )
            });
        receipt.state = outcome_state(&result, submitted).into();
        if receipt.state == "failed_or_conflicted" {
            // Only a fully observed, physically settled command can authorize a later sequencer
            // step. Inspection errors/unsupported state leave the failure recorded without authority.
            receipt.sequencer =
                sequencer::capture_failure(&self.identity, &receipt).unwrap_or(None);
        }
        // A process with unknown physical settlement still owns the barrier through the runner.
        // Leave the running receipt untouched if metadata admission is unsafe; it blocks replay.
        let publication = (|| {
            let _physical = physical()?;
            persist(&path, &receipt, &witness)
        })();
        if let Err(error) = result {
            return Err(error).context(format!("Git operation {} failed; receipt {:?}; possible partial commits/index/conflicts are retained, no automatic rollback; outcome publication: {}", receipt.id, path, if publication.is_ok() { "recorded" } else { "running receipt retained" }));
        }
        publication.context("Git completed but outcome publication failed; inspect the retained running receipt before any retry")?;
        Ok(receipt)
    }

    fn apply(
        &self,
        owner: &Owner,
        receipt: &mut ChangeReceipt,
        path: &Path,
        receipt_witness: &NativeResource,
        submitted: &mut bool,
    ) -> Result<()> {
        let root = &self.identity.canonical_root;
        let commands = match &self.change {
            Change::Reconcile { .. } => anyhow::bail!("reconciliation cannot execute Git commands"),
            Change::RevertPaths { .. } => anyhow::bail!("selected inverse uses its owned checkout"),
            Change::VerifyTask { .. } => {
                anyhow::bail!("destination verification cannot execute Git commands")
            }
            Change::Sequencer { .. } => {
                anyhow::bail!("sequencer steps execute through their own prepared path")
            }
            Change::Switch { branch } => vec![vec![
                "switch".into(),
                "--no-guess".into(),
                "--".into(),
                branch.clone(),
            ]],
            Change::IntegrateTask { .. } | Change::Integrate { .. } => vec![vec![
                "merge".into(),
                "--no-squash".into(),
                "--commit".into(),
                "--ff-only".into(),
                "--no-edit".into(),
                "--no-autostash".into(),
                self.targets[0].as_str().into(),
            ]],
            Change::Merge { .. } => vec![vec![
                "merge".into(),
                "--no-squash".into(),
                "--commit".into(),
                "--no-ff".into(),
                "--no-edit".into(),
                "--no-autostash".into(),
                self.targets[0].as_str().into(),
            ]],
            Change::DeleteBranch { branch, .. } => vec![vec![
                "branch".into(),
                "--delete".into(),
                "--".into(),
                branch.clone(),
            ]],
            Change::Revert { .. } | Change::Redo { .. } | Change::CherryPick { .. } => self
                .targets
                .iter()
                .map(|oid| {
                    let command = if matches!(self.change, Change::CherryPick { .. }) {
                        "cherry-pick"
                    } else {
                        "revert"
                    };
                    let mut args = vec![command.into(), "--no-edit".into()];
                    if let Change::Revert {
                        mainline: Some(n), ..
                    }
                    | Change::CherryPick {
                        mainline: Some(n), ..
                    } = &self.change
                    {
                        args.extend(["--mainline".into(), n.to_string()]);
                    }
                    args.push(oid.as_str().into());
                    args
                })
                .collect(),
        };
        let mut expected = self.before.clone();
        for args in commands {
            for witness in &self.witnesses {
                witness.verify()?;
            }
            ensure!(
                head(root, Some(owner))? == expected,
                "HEAD changed between operations; remaining commits were not applied"
            );
            ensure!(
                source_branch(root, Some(owner))?.as_ref() == Some(&self.branch),
                "branch changed before execution"
            );
            require_clean(&self.identity, Some(owner))?;
            if matches!(self.change, Change::DeleteBranch { .. }) {
                retire::revalidate_refs(self, Some(owner))?;
            }
            let prepared = prepare_mutation(root, root, args)?;
            if let Some(selected) = &self.handoff {
                selected.revalidate(&self.identity, Some(owner))?;
                ensure!(
                    head(root, Some(owner))? == expected
                        && source_branch(root, Some(owner))?.as_ref() == Some(&self.branch),
                    "destination moved during handoff inspection"
                );
                require_clean(&self.identity, Some(owner))?;
            }
            owner.verify_registration()?;
            {
                let _physical = physical()?;
                receipt.state = "running".into();
                persist(path, receipt, receipt_witness)?;
                crate::features::coop::refresh_workspace_branch(
                    &self.identity,
                    None,
                    owner
                        .registration
                        .as_ref()
                        .context("missing registration barrier")?,
                )?;
            }
            // Retire evidence before possible effects, including hook failure/unknown settlement.
            // In-flight children retain their own root; this does not reroute execution contexts.
            if let Some(context) = crate::core::exec_ctx::current() {
                context.note_task_effects_at(Some(root));
            } else if let Some(task) = crate::agent::todo::bound_context() {
                task.invalidate_effects_at(Some(root));
            }
            crate::agent::codebase::invalidate_cache();
            *submitted = true;
            checked_output(prepared.output_bounded_owned(
                None,
                INSPECT_TIMEOUT,
                Duration::from_secs(2),
                Some(Box::new(Arc::clone(owner))),
            ))?;
            owner.verify_registration()?;
            for witness in &self.witnesses {
                witness.verify()?;
            }
            ensure!(
                inspect_identity(root, Some(owner))? == self.identity,
                "workspace identity changed during Git execution"
            );
            let actual = head(root, Some(owner))?;
            receipt.after = Some(actual.clone());
            if matches!(
                self.change,
                Change::Revert { .. } | Change::Redo { .. } | Change::CherryPick { .. }
            ) {
                ensure!(parents(root, &actual, Some(owner))? == vec![expected.clone()] && actual != expected, "revert/cherry-pick did not create exactly one linear commit; inspect hooks and history");
                receipt.resulting_commits.push(actual.clone());
            } else if matches!(self.change, Change::Merge { .. }) {
                ensure!(
                    parents(root, &actual, Some(owner))? == vec![self.before.clone(), self.targets[0].clone()],
                    "merge did not create exactly the approved two-parent commit; inspect hooks and history"
                );
                receipt.resulting_commits.push(actual.clone());
            } else if matches!(self.change, Change::DeleteBranch { .. }) {
                ensure!(
                    actual == self.before,
                    "branch deletion changed the current HEAD"
                );
                retire::postcondition(self, owner)?;
            } else {
                ensure!(
                    actual == self.targets[0],
                    "resulting HEAD differs from approved target"
                );
            }
            let expected_branch = match &self.change {
                Change::Switch { branch } => format!("refs/heads/{branch}"),
                _ => self.branch.clone(),
            };
            ensure!(
                source_branch(root, Some(owner))?.as_ref() == Some(&expected_branch),
                "resulting branch differs from approval"
            );
            require_clean(&self.identity, Some(owner))?;
            if let Some(selected) = &self.handoff {
                // Shared hooks may have touched the source even though merge targeted the parent.
                // Keep the actual destination OID above when this postcondition fails.
                selected.revalidate(&self.identity, Some(owner))?;
                require_clean(&self.identity, Some(owner))?;
                ensure!(
                    head(root, Some(owner))? == actual
                        && source_branch(root, Some(owner))?.as_ref() == Some(&expected_branch),
                    "destination moved during handoff postinspection"
                );
            }
            expected = actual;
        }
        Ok(())
    }
}

fn outcome_state(result: &Result<()>, submitted: bool) -> &'static str {
    match result {
        Ok(()) => "settled_unverified",
        Err(error)
            if error
                .downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
                .is_some() =>
        {
            "unsettled"
        }
        // Preparation/policy refusal before submission has no Git mutation to reconcile.
        // An unsettled inspection still blocks above. Submission is not proof of process startup.
        Err(_) if !submitted => "not_started",
        Err(_) => "failed_or_conflicted",
    }
}

fn require_parent() -> Result<()> {
    crate::agent::planning::check_automation_boundary()?;
    ensure!(
        crate::core::exec_ctx::current()
            .is_none_or(|ctx| ctx.can_restore_workspace() && ctx.native_scope().is_none()),
        "Git history mutation requires coordinating-parent authority"
    );
    Ok(())
}

pub(super) fn resolve(root: &Path, revision: &str, owner: Option<&Owner>) -> Result<GitOid> {
    ensure!(
        !revision.is_empty()
            && revision.len() <= 256
            && !revision.starts_with('-')
            && !revision.starts_with('#')
            && !revision.chars().any(char::is_control)
            && !revision.bytes().all(|b| b.is_ascii_digit()),
        "supply an explicit Git revision, not a legacy checkpoint number"
    );
    let expression = format!("{revision}^{{commit}}");
    let bytes = query(
        root,
        &["rev-parse", "--verify", "--end-of-options", &expression],
        owner,
    )?;
    GitOid::parse(std::str::from_utf8(&bytes)?.trim())
}

fn parents(root: &Path, oid: &GitOid, owner: Option<&Owner>) -> Result<Vec<GitOid>> {
    let bytes = query(
        root,
        &["rev-list", "--parents", "-n", "1", oid.as_str()],
        owner,
    )?;
    let text = std::str::from_utf8(&bytes)?;
    let mut ids = text.split_whitespace();
    ensure!(
        ids.next() == Some(oid.as_str()),
        "invalid commit-parent inspection"
    );
    ids.map(GitOid::parse).collect()
}

/// Ancestry authority comes from commit objects, not replace refs, grafts or truncated history.
/// Do not silently disable those overlays: mutating a different graph than the user sees is unsafe.
pub(super) fn require_unmodified_history(
    identity: &WorkspaceIdentity,
    owner: Option<&Owner>,
) -> Result<()> {
    for directory in [
        identity.canonical_git_dir.as_ref(),
        identity.canonical_common_git_dir.as_ref(),
    ] {
        let directory = directory.context("missing Git history directory")?;
        for marker in ["info/grafts", "shallow"] {
            match fs::symlink_metadata(directory.join(marker)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("checking Git history overlays"),
                Ok(_) => anyhow::bail!(
                    "Git history has grafts or shallow boundaries ({marker}); mutation requires unmodified commit ancestry"
                ),
            }
        }
    }
    ensure!(
        query(
            &identity.canonical_root,
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/replace/"
            ],
            owner,
        )?
        .is_empty(),
        "Git replace refs override commit history; mutation requires unmodified commit ancestry"
    );
    Ok(())
}

pub(super) fn require_clean(identity: &WorkspaceIdentity, owner: Option<&Owner>) -> Result<()> {
    require_unmodified_history(identity, owner)?;
    let root = &identity.canonical_root;
    require_idle_operation(
        identity
            .canonical_git_dir
            .as_ref()
            .context("missing Git directory")?,
    )?;
    validate_visible_index(&query(root, &["ls-files", "-v", "-f", "-z"], owner)?)?;
    let entries = query(root, &["ls-files", "--stage", "-z"], owner)?;
    ensure!(
        !entries
            .split(|b| *b == 0)
            .any(|r| r.starts_with(b"160000 ")),
        "submodule worktrees require manual Git handling"
    );
    ensure!(query(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching"], owner)?.is_empty(), "worktree/index must be clean including untracked/ignored files; nothing is stashed or discarded");
    Ok(())
}

fn receipt_directory(identity: &WorkspaceIdentity) -> PathBuf {
    crate::core::config::aizen_home()
        .join("git-operations-v1")
        .join(&identity.repo_id)
        .join(&identity.worktree_id)
}

pub(super) fn require_no_pending(identity: &WorkspaceIdentity) -> Result<()> {
    for receipt in receipts::scan(identity)? {
        receipts::require_completed(&receipt)?;
    }
    verification::require_settled(identity, None)
}

/// A settled conflict can be parked by focus without granting replay or mutation authority.
pub(super) fn require_focus_settled(identity: &WorkspaceIdentity) -> Result<()> {
    for receipt in receipts::scan(identity)? {
        if receipt.state == "failed_or_conflicted" && receipt.reconciliation.is_none() {
            ensure!(
                !receipt.verified && !matches!(receipt.change, Change::Reconcile { .. }),
                "inconsistent failed operation; focus refused"
            );
        } else {
            receipts::require_completed(&receipt)?;
        }
    }
    verification::require_settled(identity, None)
}

/// Existing metadata only, including incomplete outcomes. Listing never authorizes replay.
pub(crate) fn operation_receipts(root: &Path) -> Result<Vec<ChangeReceipt>> {
    let identity = inspect_identity(root, None)?;
    let mut receipts = receipts::scan(&identity)?;
    receipts.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(receipts)
}

fn physical() -> Result<PhysicalEffectLease> {
    let scope = crate::core::exec_ctx::current()
        .map(|ctx| ctx.resource_scope())
        .unwrap_or_else(|| "default".into());
    PhysicalEffectLease::exclusive(
        Duration::from_secs(15),
        crate::core::cancel::current().as_ref(),
        "Git operation receipt",
        &scope,
    )
}

fn persist(path: &Path, receipt: &ChangeReceipt, witness: &NativeResource) -> Result<()> {
    witness.verify()?;
    crate::core::persist::atomic_write_owner_only_checked(
        path,
        &serde_json::to_vec_pretty(receipt)?,
    )?;
    witness.verify()
}

#[cfg(test)]
#[path = "git_ops_change_tests.rs"]
mod tests;
