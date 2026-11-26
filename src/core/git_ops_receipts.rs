//! Checked metadata selection for recorded Git inverses. No source bytes or restore authority.

use super::*;
use crate::core::persist::FileFingerprint;

const MAX_RECEIPT_BYTES: u64 = 128 * 1024;
const MAX_RECEIPTS: usize = 4096;

#[derive(Debug)]
pub(super) struct FrozenUndo {
    path: PathBuf,
    resource: NativeResource,
    fingerprint: FileFingerprint,
}

#[derive(Debug)]
pub(super) struct FrozenFailure {
    pub(super) id: String,
    pub(super) targets: Vec<GitOid>,
    pub(super) original: ChangeReceipt,
    path: PathBuf,
    resource: NativeResource,
    fingerprint: FileFingerprint,
}

impl FrozenFailure {
    pub(super) fn select(identity: &WorkspaceIdentity, branch: &str, id: &str) -> Result<Self> {
        ensure!(valid_id(id), "select an exact failed operation ID");
        let directory = receipt_directory(identity);
        let (_, witness) =
            crate::core::speculation::namespace::plan_metadata_directory(&directory)?;
        witness.verify()?;
        let path = directory.join(format!("{id}.json"));
        let resource = NativeResource::resolve(&path)?;
        let path = resource.path().to_path_buf();
        resource.verify()?;
        let (receipt, bytes) = read(identity, &path)?;
        validate_failure(&receipt, branch)?;
        resource.verify()?;
        witness.verify()?;
        Ok(Self {
            id: id.into(),
            targets: receipt.targets.clone(),
            original: receipt,
            path,
            resource,
            fingerprint: FileFingerprint::for_bytes(&bytes),
        })
    }

    pub(super) fn revalidate(
        &self,
        identity: &WorkspaceIdentity,
        branch: &str,
        owner: Option<&Owner>,
    ) -> Result<()> {
        self.resource.verify()?;
        let (receipt, bytes) = read(identity, &self.path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.fingerprint,
            "selected failed receipt changed; prepare again"
        );
        validate_failure(&receipt, branch)?;
        // Preserve the recorded history rather than certifying a manual history rewrite.
        let current = head(&identity.canonical_root, owner)?;
        for oid in std::iter::once(&receipt.before)
            .chain(receipt.resulting_commits.iter())
            .chain(receipt.after.iter())
        {
            let range = format!("{}..{}", current.as_str(), oid.as_str());
            ensure!(
                query(
                    &identity.canonical_root,
                    &["rev-list", "--max-count=1", &range],
                    owner
                )?
                .is_empty(),
                "recorded operation history is not retained; manual reconciliation required"
            );
        }
        self.resource.verify()
    }

    pub(super) fn acknowledge(
        &self,
        identity: &WorkspaceIdentity,
        branch: &str,
        before: &GitOid,
        owner: &Owner,
    ) -> Result<ChangeReceipt> {
        owner.verify_registration()?;
        self.revalidate(identity, branch, Some(owner))?;
        require_clean(identity, Some(owner))?;
        ensure!(
            head(&identity.canonical_root, Some(owner))? == *before
                && source_branch(&identity.canonical_root, Some(owner))?.as_deref() == Some(branch),
            "workspace moved after reconciliation approval"
        );
        let (_, witness) = crate::core::speculation::namespace::plan_metadata_directory(
            &receipt_directory(identity),
        )?;
        // Queries release their physical leases before metadata publication. The parent writer and
        // registration barrier remain held; retain unknown process effects through the query runner.
        let _physical = physical()?;
        witness.verify()?;
        self.resource.verify()?;
        let (mut receipt, bytes) = read(identity, &self.path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.fingerprint,
            "selected failed receipt changed before acknowledgement"
        );
        validate_failure(&receipt, branch)?;
        receipt.reconciliation = Some(Reconciliation {
            prior_state: receipt.state.clone(),
            observed_head: before.clone(),
            branch: branch.into(),
            recorded_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        });
        // Keep state, before/after, original action and partial commit mappings unchanged. A failed
        // undo does not become redo authority and a failed redo still cannot be replayed.
        persist(&self.path, &receipt, &witness)?;
        crate::features::coop::refresh_workspace_branch(
            identity,
            Some(branch),
            owner
                .registration
                .as_ref()
                .context("missing registration barrier")?,
        )?;
        Ok(receipt)
    }
}

fn validate_failure(receipt: &ChangeReceipt, branch: &str) -> Result<()> {
    ensure!(receipt.state == "failed_or_conflicted" && receipt.reconciliation.is_none(),
        "only an unreconciled, physically settled failed operation can be acknowledged; running/unsettled/unknown effects remain blocked");
    ensure!(
        receipt.branch_before == branch
            && !receipt.verified
            && !matches!(receipt.change, Change::Reconcile { .. }),
        "failed operation branch/outcome binding changed"
    );
    Ok(())
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !id.bytes().all(|b| b.is_ascii_digit())
}

fn read_record(path: &Path) -> Result<(ChangeReceipt, Vec<u8>)> {
    let bytes = crate::core::file_placement::read_existing_regular(path, MAX_RECEIPT_BYTES)?;
    let receipt: ChangeReceipt = serde_json::from_slice(&bytes)
        .context("unreadable Git receipt; inspect without retrying")?;
    let filename = format!("{}.json", receipt.id);
    ensure!(
        receipt.schema
            == if matches!(receipt.change, Change::IntegrateTask { .. }) {
                2
            } else {
                1
            }
            && valid_id(&receipt.id)
            && path.file_name().and_then(|name| name.to_str()) == Some(filename.as_str())
            && receipt.root.is_absolute()
            && receipt.git_dir.is_absolute()
            && receipt.common_dir.is_absolute()
            && receipt
                .verification
                .as_deref()
                .is_none_or(|record| valid_id(record) && record.len() <= 96),
        "Git receipt filename/binding/schema changed"
    );
    handoff::validate_receipt(&receipt)?;
    Ok((receipt, bytes))
}

pub(super) fn read(identity: &WorkspaceIdentity, path: &Path) -> Result<(ChangeReceipt, Vec<u8>)> {
    let (receipt, bytes) = read_record(path)?;
    ensure!(
        receipt.root == identity.canonical_root
            && Some(&receipt.git_dir) == identity.canonical_git_dir.as_ref()
            && Some(&receipt.common_dir) == identity.canonical_common_git_dir.as_ref(),
        "Git receipt workspace binding changed"
    );
    Ok((receipt, bytes))
}

/// Check records in other worktree buckets without following their stored paths. A worktree
/// may have disappeared; its metadata still blocks retirement until explicitly reconciled.
pub(super) fn read_repository(
    identity: &WorkspaceIdentity,
    path: &Path,
) -> Result<(ChangeReceipt, usize)> {
    let (receipt, bytes) = read_record(path)?;
    ensure!(
        Some(&receipt.common_dir) == identity.canonical_common_git_dir.as_ref(),
        "foreign repository receipt"
    );
    let binding = WorkspaceIdentity::from_bound_paths(
        receipt.root.clone(),
        Some(receipt.git_dir.clone()),
        Some(receipt.common_dir.clone()),
    );
    ensure!(
        path.parent() == Some(receipt_directory(&binding).as_path()),
        "Git receipt worktree bucket does not match its recorded binding"
    );
    Ok((receipt, bytes.len()))
}

pub(super) fn require_completed(receipt: &ChangeReceipt) -> Result<()> {
    if let Some(note) = &receipt.reconciliation {
        ensure!(
            receipt.state == "failed_or_conflicted"
                && note.prior_state == receipt.state
                && note.branch == receipt.branch_before
                && note.recorded_at > 0
                && !receipt.verified
                && !matches!(receipt.change, Change::Reconcile { .. }),
            "inconsistent Git operation reconciliation {}",
            receipt.id
        );
        return Ok(());
    }
    let completed = match receipt.state.as_str() {
        "not_started" => receipt.after.is_none() && receipt.resulting_commits.is_empty(),
        "settled_unverified" => match &receipt.change {
            Change::Reconcile { .. } | Change::RevertPaths { .. } => false,
            Change::Revert { commits, .. } | Change::CherryPick { commits, .. } => {
                !receipt.targets.is_empty()
                    && receipt.targets.len() == commits.len()
                    && receipt.targets.len() == receipt.resulting_commits.len()
                    && receipt.after.as_ref() == receipt.resulting_commits.last()
            }
            Change::Merge { .. } => {
                receipt.targets.len() == 1
                    && receipt.resulting_commits.len() == 1
                    && receipt.after.as_ref() == receipt.resulting_commits.last()
                    && receipt.after.as_ref() != Some(&receipt.before)
                    && receipt.after.as_ref() != receipt.targets.first()
            }
            Change::Redo { .. } => {
                !receipt.targets.is_empty()
                    && receipt.targets.len() == receipt.resulting_commits.len()
                    && receipt.after.as_ref() == receipt.resulting_commits.last()
            }
            Change::Switch { .. } | Change::Integrate { .. } | Change::IntegrateTask { .. } => {
                receipt.targets.len() == 1
                    && receipt.after.as_ref() == receipt.targets.first()
                    && receipt.resulting_commits.is_empty()
            }
            // Verification records live beside this receipt; the Git outcome is the integration.
            Change::VerifyTask { .. } => false,
            // A sequencer step moves HEAD to whatever Git produced (abort restores the pre-state;
            // continue adds one commit). It records the resulting OID but no commit mapping, so it
            // is finished only when the sequencer no longer reports an in-progress action.
            Change::Sequencer {
                receipt: selected,
                step,
            } => {
                valid_id(selected)
                    && selected != &receipt.id
                    && matches!(step.as_str(), "continue" | "abort" | "quit")
                    && receipt.targets.len() == 1
                    && receipt.after.as_ref() == receipt.targets.first()
                    && receipt.resulting_commits.is_empty()
                    && if step == "continue" {
                        receipt
                            .after
                            .as_ref()
                            .is_some_and(|after| after != &receipt.before)
                    } else {
                        receipt.after.as_ref() == Some(&receipt.before)
                    }
            }
            Change::DeleteBranch { .. } => {
                receipt.targets.len() == 2
                    && receipt.after.as_ref() == Some(&receipt.before)
                    && receipt.resulting_commits.is_empty()
            }
        },
        _ => false,
    };
    ensure!(
        completed,
        "unfinished Git operation {}; receipt is incomplete or inconsistent; inspect receipt and Git sequencer before another mutation",
        receipt.id
    );
    Ok(())
}

pub(super) fn scan(identity: &WorkspaceIdentity) -> Result<Vec<ChangeReceipt>> {
    let directory = receipt_directory(identity);
    let (_, witness) = crate::core::speculation::namespace::plan_metadata_directory(&directory)?;
    witness.verify()?;
    let entries = match fs::read_dir(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading Git operation receipts"),
        Ok(entries) => entries,
    };
    let mut receipts = Vec::new();
    // Bound total metadata as well as each file. IDs and stored paths are never path authority.
    let mut total_bytes = 0usize;
    for (index, entry) in entries.enumerate() {
        ensure!(
            index < MAX_RECEIPTS,
            "Git operation receipt scan limit reached"
        );
        let (receipt, bytes) = read(identity, &entry?.path())?;
        total_bytes = total_bytes
            .checked_add(bytes.len())
            .context("receipt byte count overflow")?;
        ensure!(
            total_bytes <= 16 * 1024 * 1024,
            "Git receipt scan byte limit reached"
        );
        receipts.push(receipt);
    }
    witness.verify()?;
    Ok(receipts)
}

impl FrozenUndo {
    pub(super) fn select(
        identity: &WorkspaceIdentity,
        branch: &str,
        before: &GitOid,
        id: &str,
    ) -> Result<(Self, Vec<GitOid>)> {
        ensure!(
            valid_id(id),
            "select an exact undo operation ID, not a revision or path"
        );
        let directory = receipt_directory(identity);
        let (_, witness) =
            crate::core::speculation::namespace::plan_metadata_directory(&directory)?;
        witness.verify()?;
        let path = directory.join(format!("{id}.json"));
        let resource = NativeResource::resolve(&path)?;
        let path = resource.path().to_path_buf();
        resource.verify()?;
        let (receipt, bytes) = read(identity, &path)?;
        validate_undo(identity, branch, before, &receipt, None)?;
        resource.verify()?;
        witness.verify()?;
        let targets = receipt.resulting_commits.iter().rev().cloned().collect();
        Ok((
            Self {
                path,
                resource,
                fingerprint: FileFingerprint::for_bytes(&bytes),
            },
            targets,
        ))
    }

    pub(super) fn revalidate(
        &self,
        identity: &WorkspaceIdentity,
        branch: &str,
        before: &GitOid,
        owner: Option<&Owner>,
    ) -> Result<()> {
        self.resource.verify()?;
        let (receipt, bytes) = read(identity, &self.path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.fingerprint,
            "selected undo receipt changed; prepare again"
        );
        validate_undo(identity, branch, before, &receipt, owner)?;
        self.resource.verify()
    }
}

fn validate_undo(
    identity: &WorkspaceIdentity,
    branch: &str,
    before: &GitOid,
    receipt: &ChangeReceipt,
    owner: Option<&Owner>,
) -> Result<()> {
    let Change::Revert { commits, .. } = &receipt.change else {
        anyhow::bail!("redo requires a completed undo receipt, not another operation");
    };
    ensure!(
        receipt.state == "settled_unverified",
        "selected undo did not complete"
    );
    ensure!(
        receipt.branch_before == branch,
        "undo belongs to another branch"
    );
    ensure!(
        !receipt.targets.is_empty()
            && receipt.targets.len() <= 32
            && receipt.targets.len() == commits.len()
            && receipt.targets.len() == receipt.resulting_commits.len()
            && receipt.after.as_ref() == receipt.resulting_commits.last(),
        "undo receipt has incomplete commit mappings"
    );
    let root = &identity.canonical_root;
    let mut expected = receipt.before.clone();
    for inverse in &receipt.resulting_commits {
        ensure!(
            parents(root, inverse, owner)? == vec![expected.clone()] && inverse != &expected,
            "recorded inverse history is missing or not linear"
        );
        expected = inverse.clone();
    }
    let range = format!("{}..{}", before.as_str(), expected.as_str());
    ensure!(
        query(root, &["rev-list", "--max-count=1", &range], owner)?.is_empty(),
        "recorded undo is not reachable from the current destination"
    );
    for prior in scan(identity)? {
        if matches!(&prior.change, Change::Redo { receipt: id } if id == &receipt.id) {
            ensure!(
                prior.state == "not_started"
                    && prior.after.is_none()
                    && prior.resulting_commits.is_empty(),
                "this undo already has a redo attempt; inspect its receipt instead of replaying"
            );
        }
    }
    Ok(())
}
