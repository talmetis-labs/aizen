//! Exact-ID, existing-only observations. No mutation, reconciliation writer or execution authority.
use super::*;
use crate::core::persist::FileFingerprint;

const NOTICE: &str = "Observation only. Current files, HEAD, clean status and absent markers do not prove an unknown worker stopped. Preview availability is nonbinding: legacy/namespace settlement and co-op registration admission are not fully checked. Reconciliation must be prepared again, explicitly approved, and pass all execution barriers. No receipt was acknowledged, operation retried, or ownership granted.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Digest {
    byte_len: u64,
    sha256: String,
}
impl Digest {
    fn bytes(bytes: &[u8]) -> Self {
        let fingerprint = FileFingerprint::for_bytes(bytes);
        Self {
            byte_len: fingerprint.byte_len,
            sha256: fingerprint
                .sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct IdentityObservation {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    repo_id: String,
    worktree_id: String,
}
impl IdentityObservation {
    fn from_identity(identity: &WorkspaceIdentity) -> Result<Self> {
        Ok(Self {
            root: identity.canonical_root.clone(),
            git_dir: identity
                .canonical_git_dir
                .clone()
                .context("missing Git directory")?,
            common_dir: identity
                .canonical_common_git_dir
                .clone()
                .context("missing common Git directory")?,
            repo_id: identity.repo_id.clone(),
            worktree_id: identity.worktree_id.clone(),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptDisposition {
    RecordedComplete,
    FailedOrConflicted,
    PendingUnknown,
}

#[derive(Debug, Serialize)]
struct ReceiptObservation {
    fingerprint: Digest,
    disposition: ReceiptDisposition,
    record: ChangeReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GitObservation {
    head: GitOid,
    branch: Option<String>,
    index: Digest,
    index_entries: Digest,
    porcelain_status: Digest,
    porcelain_clean: bool,
    sequencer: Option<sequencer::ConflictEvidence>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservationStatus {
    Observed,
    ObservationBlocked,
}

#[derive(Debug, Serialize)]
pub(crate) struct OperationInspection {
    schema: u32,
    selected_receipt: String,
    requested_root: PathBuf,
    identity: Option<IdentityObservation>,
    receipt: Option<ReceiptObservation>,
    status: ObservationStatus,
    current: Option<GitObservation>,
    reconciliation_preview_available: bool,
    execution_barriers_fully_checked: bool,
    reason: Option<String>,
    notice: &'static str,
}

fn reason(error: &anyhow::Error) -> String {
    format!("{error:#}").chars().take(2000).collect()
}

/// Freeze existing metadata only. Fields are private; this is not a settlement capability.
pub(super) struct FrozenOperationObservation {
    identity: WorkspaceIdentity,
    path: PathBuf,
    resource: NativeResource,
    directory: NativeResource,
    fingerprint: FileFingerprint,
    receipt: ChangeReceipt,
}
impl FrozenOperationObservation {
    pub(super) fn select(identity: WorkspaceIdentity, id: &str) -> Result<Self> {
        ensure!(receipts::valid_id(id), "select an exact Git operation ID");
        let (directory, witness) = crate::core::speculation::namespace::plan_metadata_directory(
            &receipt_directory(&identity),
        )?;
        witness.verify()?;
        let resource = NativeResource::resolve(&directory.join(format!("{id}.json")))?;
        resource.verify()?;
        let path = resource.path().to_path_buf();
        let (receipt, bytes) = receipts::read(&identity, &path)?;
        resource.verify()?;
        witness.verify()?;
        Ok(Self {
            identity,
            path,
            resource,
            directory: witness,
            fingerprint: FileFingerprint::for_bytes(&bytes),
            receipt,
        })
    }

    fn revalidate(&self) -> Result<()> {
        self.directory.verify()?;
        self.resource.verify()?;
        let (_, bytes) = receipts::read(&self.identity, &self.path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.fingerprint,
            "selected operation receipt changed; inspect again"
        );
        self.resource.verify()?;
        self.directory.verify()
    }

    pub(super) fn inspect(self, requested_root: &Path) -> Result<OperationInspection> {
        self.revalidate()?;
        let mut report = OperationInspection {
            schema: 1,
            selected_receipt: self.receipt.id.clone(),
            requested_root: requested_root.to_path_buf(),
            identity: Some(IdentityObservation::from_identity(&self.identity)?),
            receipt: None,
            status: ObservationStatus::ObservationBlocked,
            current: None,
            reconciliation_preview_available: false,
            execution_barriers_fully_checked: false,
            reason: None,
            notice: NOTICE,
        };
        let observed = (|| -> Result<(GitObservation, bool, Option<String>)> {
            let cancel = crate::core::cancel::current();
            // This only excludes concurrent writers. It proves neither prior settlement nor ownership.
            // Every query still takes fresh bounded-runner physical admission and retains this owner.
            let writer = WorkspaceWriterLease::acquire_identity(
                self.identity.clone(),
                Duration::ZERO,
                cancel.as_ref(),
                "inspect existing Git operation",
            )?;
            let background = BackgroundProcessBarrier::acquire(cancel.as_ref())?;
            let owner = Arc::new(OperationOwner {
                writer,
                _additional_writers: Vec::new(),
                _background: background,
                registration: None,
            });
            ensure!(
                inspect_identity(&self.identity.canonical_root, Some(&owner))? == self.identity,
                "operation workspace identity changed"
            );
            let before = observe(&self.identity, &owner)?;
            let (available, unavailable) = if self.receipt.state == "failed_or_conflicted"
                && self.receipt.reconciliation.is_none()
                && before.porcelain_clean
                && before.sequencer.is_none()
            {
                let preview = PreparedChange::prepare(
                    &self.identity.canonical_root,
                    Change::Reconcile {
                        receipt: self.receipt.id.clone(),
                    },
                )
                .and_then(|_| {
                    // This existing-only journal read does not call Git. Do not place the legacy
                    // namespace checks here: their identity discovery currently invokes Git.
                    // No runner call occurs until this fresh physical lease has been dropped.
                    let _physical = physical()?;
                    crate::core::native_journal::require_workspace_settled(
                        &self.identity.canonical_root,
                    )
                    .context("native journal preview evidence refused")
                });
                match preview {
                    Ok(_) => (true, None),
                    Err(error)
                        if error
                            .downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
                            .is_some() =>
                    {
                        return Err(error)
                    }
                    Err(error) => (false, Some(reason(&error))),
                }
            } else {
                (false, Some("No eligible manually resolved terminal failure; pending/unknown effects are never acknowledged by inspection.".into()))
            };
            let after = observe(&self.identity, &owner)?;
            ensure!(
                before == after,
                "Git state changed during operation inspection; inspect again"
            );
            ensure!(
                inspect_identity(&self.identity.canonical_root, Some(&owner))? == self.identity,
                "operation workspace identity changed during inspection"
            );
            self.revalidate()?;
            Ok((after, available, unavailable))
        })();
        // Recheck selected bytes even when actual-state inspection was blocked. Never substitute a
        // changed record or turn partial/current observations into authority for the old selection.
        self.revalidate()?;
        match observed {
            Ok((current, available, unavailable)) => {
                report.status = ObservationStatus::Observed;
                report.current = Some(current);
                report.reconciliation_preview_available = available;
                report.reason = unavailable;
            }
            Err(error) => report.reason = Some(reason(&error)),
        }
        let disposition = if receipts::require_completed(&self.receipt).is_ok() {
            ReceiptDisposition::RecordedComplete
        } else if self.receipt.state == "failed_or_conflicted"
            && !self.receipt.verified
            && self.receipt.reconciliation.is_none()
            && !matches!(self.receipt.change, Change::Reconcile { .. })
        {
            ReceiptDisposition::FailedOrConflicted
        } else {
            ReceiptDisposition::PendingUnknown
        };
        report.receipt = Some(ReceiptObservation {
            fingerprint: Digest {
                byte_len: self.fingerprint.byte_len,
                sha256: self
                    .fingerprint
                    .sha256
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            },
            disposition,
            record: self.receipt,
        });
        Ok(report)
    }
}

fn observe(identity: &WorkspaceIdentity, owner: &Owner) -> Result<GitObservation> {
    let root = &identity.canonical_root;
    let head = head(root, Some(owner))?;
    let branch = source_branch(root, Some(owner))?;
    let entries = query(root, &["ls-files", "--stage", "-z"], Some(owner))?;
    let status = query(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
        Some(owner),
    )?;
    let git_dir = identity
        .canonical_git_dir
        .as_ref()
        .context("missing Git directory")?;
    let index_resource = NativeResource::resolve(&git_dir.join("index"))?;
    index_resource.verify()?;
    let index = crate::core::file_placement::read_existing_regular(
        index_resource.path(),
        16 * 1024 * 1024,
    )?;
    index_resource.verify()?;
    let sequencer = sequencer::inspect_evidence(git_dir)?;
    Ok(GitObservation {
        head,
        branch,
        index: Digest::bytes(&index),
        index_entries: Digest::bytes(&entries),
        porcelain_status: Digest::bytes(&status),
        porcelain_clean: status.is_empty(),
        sequencer,
    })
}

/// No selection from a stored root and no metadata initialization. Identity queries use the same
/// guarded runner as all other Git inspection; a refused query is reported, not bypassed.
pub(crate) fn inspect_operation(root: &Path, id: &str) -> Result<OperationInspection> {
    ensure!(receipts::valid_id(id), "select an exact Git operation ID");
    let identity = match inspect_identity(root, None) {
        Ok(identity) => identity,
        Err(error) => {
            return Ok(OperationInspection {
                schema: 1,
                selected_receipt: id.into(),
                requested_root: root.to_path_buf(),
                identity: None,
                receipt: None,
                status: ObservationStatus::ObservationBlocked,
                current: None,
                reconciliation_preview_available: false,
                execution_barriers_fully_checked: false,
                reason: Some(reason(&error)),
                notice: NOTICE,
            })
        }
    };
    FrozenOperationObservation::select(identity, id)?.inspect(root)
}
