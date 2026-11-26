//! Prepared steps for single-commit conflicts with durable controller provenance.
//! Legacy/unowned conflict state is preserved and refused, never inferred from a matching OID.

use super::*;
use crate::core::{exec_ctx, file_placement, persist::FileFingerprint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Abort,
    Quit,
}

impl Step {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "continue" => Ok(Self::Continue),
            "abort" => Ok(Self::Abort),
            "quit" => Ok(Self::Quit),
            _ => anyhow::bail!("sequencer step must be continue, abort or quit"),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Abort => "abort",
            Self::Quit => "quit",
        }
    }
}

/// Metadata only: no marker/source bytes and no permission to replay an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MarkerEvidence {
    identity: (u64, u64),
    digest: String,
    modified_ns: Option<u128>,
    created_ns: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConflictEvidence {
    pub(super) action: String,
    pub(super) head: GitOid,
    marker: MarkerEvidence,
    auxiliaries: Vec<(String, Option<MarkerEvidence>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Recognized {
    action: &'static str,
    head: GitOid,
    evidence: ConflictEvidence,
}

fn digest(bytes: &[u8]) -> String {
    FileFingerprint::for_bytes(bytes)
        .sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn marker(path: &Path) -> Result<Option<(MarkerEvidence, Vec<u8>)>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspecting conflict evidence"),
        Ok(_) => {}
    }
    let (bytes, metadata) = file_placement::read_existing_regular_with_metadata(path, 64 * 1024)?;
    let stamp = file_placement::NodeStamp::capture(path, &metadata, false)?;
    let nanos = |time: std::io::Result<std::time::SystemTime>| {
        time.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
    };
    Ok(Some((
        MarkerEvidence {
            identity: stamp.identity(),
            digest: digest(&bytes),
            modified_ns: nanos(metadata.modified()),
            created_ns: nanos(metadata.created()),
        },
        bytes,
    )))
}

/// Unsupported, malformed, multi-head, link or ambiguous state is an error, never idle.
pub(super) fn inspect(git_dir: &Path) -> Result<Option<Recognized>> {
    for name in [
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_LOG",
        "MERGE_AUTOSTASH",
    ] {
        match fs::symlink_metadata(git_dir.join(name)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspecting worktree operation state"),
            Ok(_) => anyhow::bail!(
                "worktree has unsupported Git operation state ({name}); manual inspection required"
            ),
        }
    }
    let mut found = Vec::new();
    for (action, name) in [
        ("merge", "MERGE_HEAD"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
        ("revert", "REVERT_HEAD"),
    ] {
        if let Some((marker, bytes)) = marker(&git_dir.join(name))? {
            let text = std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']);
            // Parsing the WHOLE value refuses empty/multiple heads and extra marker content.
            let head = GitOid::parse(text).context("invalid single-commit conflict marker")?;
            let mut auxiliaries = Vec::new();
            for name in ["MERGE_MSG", "MERGE_MODE", "ORIG_HEAD", "AUTO_MERGE"] {
                auxiliaries.push((name.into(), self::marker(&git_dir.join(name))?.map(|v| v.0)));
            }
            found.push(Recognized {
                action,
                head: head.clone(),
                evidence: ConflictEvidence {
                    action: action.into(),
                    head,
                    marker,
                    auxiliaries,
                },
            });
        }
    }
    ensure!(
        found.len() <= 1,
        "worktree has conflicting operation markers; inspect manually"
    );
    Ok(found.pop())
}

/// Strict observation only; the returned marker evidence never grants sequencer authority.
pub(super) fn inspect_evidence(git_dir: &Path) -> Result<Option<ConflictEvidence>> {
    Ok(inspect(git_dir)?.map(|recognized| recognized.evidence))
}

pub(super) fn capture_failure(
    identity: &WorkspaceIdentity,
    receipt: &ChangeReceipt,
) -> Result<Option<ConflictEvidence>> {
    let Some(recognized) = inspect(
        identity
            .canonical_git_dir
            .as_ref()
            .context("missing Git directory")?,
    )?
    else {
        return Ok(None);
    };
    ensure!(
        matches_action(&receipt.change, recognized.action)
            && receipt.targets.contains(&recognized.head),
        "conflict does not match the submitted Git operation"
    );
    Ok(Some(recognized.evidence))
}

fn matches_action(change: &Change, action: &str) -> bool {
    matches!(
        (change, action),
        (Change::Merge { .. }, "merge")
            | (Change::CherryPick { .. }, "cherry-pick")
            | (Change::Revert { .. } | Change::Redo { .. }, "revert")
    )
}

#[derive(Debug, PartialEq, Eq)]
struct WorkspaceState {
    fingerprint: String,
    index_entries: Vec<u8>,
    untracked: String,
}

/// Bounded approval witness only, not source storage. Includes ignored/untracked files so abort
/// approval cannot silently cover edits made after the preview. Unsupported files fail closed.
fn workspace_state(identity: &WorkspaceIdentity, owner: Option<&Owner>) -> Result<WorkspaceState> {
    let root = &identity.canonical_root;
    let flags = query(root, &["ls-files", "-v", "-f", "-z"], owner)?;
    ensure!(
        flags.is_empty() || flags.ends_with(&[0]),
        "incomplete Git index flags"
    );
    for entry in flags.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        // Uppercase M is an ordinary unmerged entry. Lowercase letters, S and other tags
        // still refuse assume-unchanged/skip-worktree/fsmonitor or unsupported index state.
        ensure!(
            entry.len() > 2 && (entry.starts_with(b"H ") || entry.starts_with(b"M ")),
            "conflict index has hidden or unsupported flags"
        );
    }
    let index_entries = query(root, &["ls-files", "--stage", "-z"], owner)?;
    ensure!(
        !index_entries
            .split(|b| *b == 0)
            .any(|r| r.starts_with(b"160000 ")),
        "submodule conflict handling requires manual Git inspection"
    );
    let mut tracked = query(root, &["ls-files", "--cached", "-z"], owner)?;
    let others = query(root, &["ls-files", "--others", "-z"], owner)?;
    tracked.extend_from_slice(&others);
    let mut paths = tracked
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    ensure!(paths.len() <= 4096, "sequencer approval file limit reached");
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut untracked = ring::digest::Context::new(&ring::digest::SHA256);
    let mut total = 0usize;
    for name in paths {
        let relative = Path::new(std::str::from_utf8(name)?);
        ensure!(
            !relative.is_absolute()
                && relative
                    .components()
                    .all(|p| matches!(p, std::path::Component::Normal(_))),
            "unsupported Git path in sequencer approval"
        );
        let path = root.join(relative);
        let fingerprint = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".into(),
            Err(error) => return Err(error).context("inspecting conflict worktree"),
            Ok(_) => {
                let (bytes, md) =
                    file_placement::read_existing_regular_with_metadata(&path, 16 * 1024 * 1024)?;
                total = total
                    .checked_add(bytes.len())
                    .context("sequencer byte count overflow")?;
                ensure!(
                    total <= 64 * 1024 * 1024,
                    "sequencer approval byte limit reached"
                );
                format!("{}:{:?}", digest(&bytes), md.permissions())
            }
        };
        for target in [&mut hash].into_iter().chain(
            others
                .split(|b| *b == 0)
                .any(|p| p == name)
                .then_some(&mut untracked),
        ) {
            target.update(&(name.len() as u64).to_le_bytes());
            target.update(name);
            target.update(fingerprint.as_bytes());
        }
    }
    let index = file_placement::read_existing_regular(
        &identity
            .canonical_git_dir
            .as_ref()
            .context("missing Git directory")?
            .join("index"),
        16 * 1024 * 1024,
    )?;
    hash.update(&index);
    hash.update(&index_entries);
    Ok(WorkspaceState {
        fingerprint: digest(hash.finish().as_ref()),
        index_entries,
        untracked: digest(untracked.finish().as_ref()),
    })
}

#[derive(Debug)]
pub(crate) struct PreparedSequencer {
    identity: WorkspaceIdentity,
    witnesses: Vec<NativeResource>,
    binding: Option<exec_ctx::WorkspaceBinding>,
    selected: receipts::FrozenFailure,
    recognized: Recognized,
    workspace: WorkspaceState,
    step: Step,
    branch: String,
    before: GitOid,
}

impl PreparedSequencer {
    pub(crate) fn prepare(root: &Path, receipt: &str, step: &str) -> Result<Self> {
        require_parent()?;
        let step = Step::parse(step)?;
        let identity = inspect_identity(root, None)?;
        require_unmodified_history(&identity, None)?;
        let branch = source_branch(&identity.canonical_root, None)?
            .context("sequencer requires a local branch")?;
        let git_dir = identity
            .canonical_git_dir
            .as_ref()
            .context("missing Git directory")?;
        let recognized = inspect(git_dir)?.context("worktree has no recognized Git operation")?;
        let selected = receipts::FrozenFailure::select(&identity, &branch, receipt)
            .context("no recorded failed operation for the selected receipt")?;
        let before = head(&identity.canonical_root, None)?;
        let witnesses = [
            &identity.canonical_root,
            git_dir,
            identity
                .canonical_common_git_dir
                .as_ref()
                .context("missing common Git directory")?,
        ]
        .into_iter()
        .map(|p| NativeResource::resolve(&p.join(".aizen-sequencer-boundary")))
        .collect::<Result<Vec<_>>>()?;
        let workspace = workspace_state(&identity, None)?;
        let prepared = Self {
            identity,
            witnesses,
            binding: exec_ctx::current().and_then(|ctx| ctx.workspace_binding().cloned()),
            selected,
            recognized,
            workspace,
            step,
            branch,
            before,
        };
        prepared.revalidate(None)?;
        Ok(prepared)
    }

    pub(crate) fn preview(&self) -> Result<String> {
        let effect = match self.step {
            Step::Continue => "Commits the approved staged resolution using Git, with configured hooks/signing. No unresolved entries or hook-added content is accepted as success.",
            Step::Abort => "Asks Git to abort this recorded single-commit conflict. Conflict resolutions may be discarded. Requires unchanged approval witnesses; it is not a snapshot restore.",
            Step::Quit => "Asks Git to forget this recorded operation, preserving index/worktree contents. Does not acknowledge the original failed receipt.",
        };
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "workspace": self.identity.canonical_root, "branch": self.branch,
            "operation": self.recognized.action, "step": self.step.as_str(),
            "owned_receipt": self.selected.id, "before": self.before,
            "conflicted_commit": self.recognized.head, "effects": effect,
            "verification": "Git postconditions only; project remains unverified. The original failure requires separate reconciliation."
        }))?)
    }

    fn revalidate(&self, owner: Option<&Owner>) -> Result<()> {
        require_parent()?;
        exec_ctx::current()
            .unwrap_or_default()
            .require_current_binding(self.binding.as_ref())?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        ensure!(
            inspect_identity(&self.identity.canonical_root, owner)? == self.identity,
            "repository/worktree binding changed; prepare again"
        );
        require_unmodified_history(&self.identity, owner)?;
        ensure!(
            head(&self.identity.canonical_root, owner)? == self.before
                && source_branch(&self.identity.canonical_root, owner)?.as_deref()
                    == Some(self.branch.as_str()),
            "destination HEAD/branch moved; approval is stale"
        );
        let recognized = inspect(
            self.identity
                .canonical_git_dir
                .as_ref()
                .context("missing Git directory")?,
        )?
        .context("Git sequencer is no longer in progress")?;
        ensure!(
            recognized == self.recognized,
            "Git conflict evidence changed; prepare again"
        );
        self.selected
            .revalidate(&self.identity, &self.branch, owner)?;
        let original = &self.selected.original;
        ensure!(original.sequencer.as_ref() == Some(&recognized.evidence)
            && matches_action(&original.change, recognized.action)
            && original.targets.contains(&recognized.head),
            "selected receipt has no matching controller conflict evidence; manual inspection required");
        ensure!(
            self.before
                == *original
                    .resulting_commits
                    .last()
                    .unwrap_or(&original.before),
            "destination moved after the failed command; manual inspection required"
        );
        crate::features::coop::require_no_other_workspace_owner(&self.identity)?;
        for receipt in receipts::scan(&self.identity)? {
            if receipt.id != self.selected.id {
                receipts::require_completed(&receipt)?;
            }
        }
        verification::require_settled(&self.identity, None)?;
        ensure!(
            workspace_state(&self.identity, owner)? == self.workspace,
            "index/worktree changed after sequencer approval; prepare again"
        );
        Ok(())
    }

    pub(crate) fn execute(self) -> Result<ChangeReceipt> {
        require_parent()?;
        let cancel = crate::core::cancel::current();
        let writer = WorkspaceWriterLease::acquire_identity(
            self.identity.clone(),
            Duration::from_secs(15),
            cancel.as_ref(),
            "approved Git sequencer step",
        )?;
        let barrier = BackgroundProcessBarrier::acquire(cancel.as_ref())?;
        let registration =
            crate::features::coop::acquire_workspace_registration_barrier(&self.identity)?;
        let owner = Arc::new(OperationOwner {
            writer,
            _additional_writers: Vec::new(),
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
        }
        let root = &self.identity.canonical_root;
        if self.step == Step::Continue {
            ensure!(
                query(root, &["ls-files", "--unmerged", "-z"], Some(&owner))?.is_empty(),
                "conflict is still unresolved; resolve and stage files first"
            );
        }
        // Git documents `commit` as the equivalent completion of a resolved merge. Unlike
        // `merge --continue`, `commit --no-edit` accepts the explicit non-interactive message flag.
        let args = if self.step == Step::Continue && self.recognized.action == "merge" {
            vec!["commit".into(), "--no-edit".into()]
        } else {
            let mut args = vec![
                self.recognized.action.into(),
                format!("--{}", self.step.as_str()),
            ];
            if self.step == Step::Continue {
                args.push("--no-edit".into());
            }
            args
        };
        let prepared = prepare_mutation(root, root, args)?;
        self.revalidate(Some(&owner))?;
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            crate::core::persist::unique_sequence()
        );
        let mut receipt = ChangeReceipt {
            schema: 1,
            id,
            root: root.clone(),
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
            change: Change::Sequencer {
                receipt: self.selected.id.clone(),
                step: self.step.as_str().into(),
            },
            targets: vec![self.recognized.head.clone()],
            state: "running".into(),
            resulting_commits: Vec::new(),
            verified: false,
            reconciliation: None,
            handoff: None,
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
            ensure!(!path.try_exists()?, "sequencer attempt already exists");
            persist(&path, &receipt, &witness)?;
            witness
        };
        if let Some(context) = exec_ctx::current() {
            context.note_task_effects_at(Some(root));
        } else if let Some(task) = crate::agent::todo::bound_context() {
            task.invalidate_effects_at(Some(root));
        }
        crate::agent::codebase::invalidate_cache();
        // A running receipt is durable BEFORE submission. Unknown effects retain it and ownership.
        let result = (|| {
            owner.verify_registration()?;
            checked_output(prepared.output_bounded_owned(
                None,
                INSPECT_TIMEOUT,
                Duration::from_secs(2),
                Some(Box::new(Arc::clone(&owner))),
            ))?;
            owner.verify_registration()?;
            for witness in &self.witnesses {
                witness.verify()?;
            }
            ensure!(
                inspect_identity(root, Some(&owner))? == self.identity
                    && source_branch(root, Some(&owner))?.as_deref() == Some(self.branch.as_str()),
                "workspace identity/branch changed during sequencer step"
            );
            require_unmodified_history(&self.identity, Some(&owner))?;
            let after = head(root, Some(&owner))?;
            receipt.after = Some(after.clone());
            ensure!(
                inspect(&receipt.git_dir)?.is_none(),
                "Git operation is still in progress"
            );
            let current = workspace_state(&self.identity, Some(&owner))?;
            ensure!(
                current.untracked == self.workspace.untracked,
                "sequencer step changed untracked/ignored files"
            );
            match self.step {
                Step::Continue => {
                    let expected = if self.recognized.action == "merge" {
                        vec![self.before.clone(), self.recognized.head.clone()]
                    } else {
                        vec![self.before.clone()]
                    };
                    ensure!(
                        after != self.before && parents(root, &after, Some(&owner))? == expected,
                        "continue did not create exactly the expected commit parents"
                    );
                    ensure!(
                        current.index_entries == self.workspace.index_entries,
                        "hooks changed the approved staged resolution"
                    );
                    ensure!(
                        query(
                            root,
                            &["status", "--porcelain=v1", "-z", "--untracked-files=no"],
                            Some(&owner)
                        )?
                        .is_empty(),
                        "continue left tracked changes"
                    );
                }
                Step::Abort => {
                    ensure!(
                        after == self.before,
                        "abort moved HEAD outside this single failed command"
                    );
                    ensure!(
                        query(
                            root,
                            &["status", "--porcelain=v1", "-z", "--untracked-files=no"],
                            Some(&owner)
                        )?
                        .is_empty(),
                        "abort did not leave a clean tracked index/worktree"
                    );
                }
                Step::Quit => ensure!(
                    after == self.before && current == self.workspace,
                    "quit changed HEAD/index/worktree contents"
                ),
            }
            receipt.targets = vec![after];
            Ok(())
        })();
        if result.as_ref().err().is_some_and(|e| {
            e.downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
                .is_some()
        }) {
            return result
                .map(|_| receipt)
                .context("sequencer effects unknown; running attempt retained; do not retry");
        }
        receipt.state = if result.is_ok() {
            "settled_unverified"
        } else {
            "failed_or_conflicted"
        }
        .into();
        let publication = (|| {
            let _physical = physical()?;
            persist(&path, &receipt, &witness)
        })();
        result.context(format!(
            "sequencer attempt {} failed; effects retained; outcome publication: {}",
            receipt.id,
            if publication.is_ok() {
                "recorded"
            } else {
                "running receipt retained"
            }
        ))?;
        publication.context(
            "Git finished but sequencer outcome publication failed; running attempt blocks retry",
        )?;
        Ok(receipt)
    }
}
