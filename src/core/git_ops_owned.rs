//! First Git-owned task lane: fresh clean worktree, exact native writes, explicit-path commit.
//! No shell isolation, adoption, retry, automatic integration or verification is implied.
use super::create::{head, inspect_identity, query, source_branch, Owner};
use super::{
    checked_output, prepare_mutation, CreatedWorktree, PreparedWorktreeCreation, INSPECT_TIMEOUT,
};
use crate::core::git_native_lifecycle::{GitNativeAttempt, NativeBinding};
use crate::core::{
    native_scope::NativeWriteScope,
    snapshots::GitOid,
    workspace_txn::{NativeResource, PhysicalEffectLease, WorkspaceIdentity},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Debug)]
pub(crate) struct PreparedOwnedTask {
    creation: PreparedWorktreeCreation,
    parent_scope: String,
    scope: String,
}

impl PreparedOwnedTask {
    pub(crate) fn prepare(root: &Path, parent_scope: &str) -> Result<Self> {
        Self::prepare_from(root, parent_scope, &head(root, None)?)
    }

    /// The controller supplies an already selected full commit, including the verified destination
    /// tip for a dependent task. Creation still freezes and revalidates the source workspace.
    pub(crate) fn prepare_from(root: &Path, parent_scope: &str, base: &GitOid) -> Result<Self> {
        ensure!(
            !parent_scope.is_empty()
                && parent_scope.trim() == parent_scope
                && !parent_scope.chars().any(char::is_control),
            "task requires an explicit parent owner scope"
        );
        let id = format!(
            "task-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            crate::core::persist::unique_sequence()
        );
        let scope = format!("{parent_scope}/{id}");
        ensure!(scope.len() <= 1000, "task owner scope is too long");
        Ok(Self {
            creation: crate::features::coop::prepare_work_new(root, &id, Some(base.as_str()))?,
            parent_scope: parent_scope.into(),
            scope,
        })
    }
    pub(crate) fn preview(&self) -> Result<String> {
        Ok(format!("{}\nExplicit task ownership will be registered after clean creation. Only exact native file writes may be committed; parent edits are excluded. Commit hooks/signing run. Result remains unverified and unintegrated.", self.creation.preview()?))
    }
    pub(crate) fn execute(self) -> Result<OwnedTask> {
        self.creation.execute_for_task(&self.scope)
    }
}

/// One in-memory workflow allocator retains the same parent writer and exclusive repository
/// registration barrier for all its fresh native children. It grants no adoption or crash recovery.
/// Dropping this controller never releases a child/worker's cloned operation ownership.
pub(crate) struct OwnedTaskAllocator {
    retained: std::sync::Mutex<Option<RetainedAllocationOwner>>,
    cancel: crate::core::cancel::TurnCancel,
}

impl Drop for OwnedTaskAllocator {
    fn drop(&mut self) {
        // A dropped observer stops future cohort work, but live workers keep their owner carriers.
        self.cancel.cancel();
    }
}

struct RetainedAllocationOwner {
    operation: Owner,
    parent_scope: String,
    resource_scope: String,
    binding: Option<crate::core::exec_ctx::WorkspaceBinding>,
}

/// Opaque lifetime carrier only; executable permission is still checked by the runner/context.
pub(crate) struct OwnedTaskAllocationOwner {
    _operation: Owner,
}

impl std::fmt::Debug for OwnedTaskAllocationOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedTaskAllocationOwner")
            .finish_non_exhaustive()
    }
}

impl OwnedTaskAllocator {
    pub(crate) fn new(parent: &crate::core::cancel::TurnCancel) -> Self {
        Self {
            retained: std::sync::Mutex::new(None),
            cancel: parent.child(),
        }
    }

    pub(crate) fn cancel(&self) -> crate::core::cancel::TurnCancel {
        self.cancel.clone()
    }

    /// Stops new native admission and subsequent controller mutations, never certifies settlement.
    pub(crate) fn stop(&self) {
        self.cancel.cancel();
    }

    /// Inspection workers before/after allocation must retain the same repository barrier too.
    pub(crate) fn context(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<crate::core::exec_ctx::ExecutionContext> {
        let context = || -> Result<crate::core::exec_ctx::ExecutionContext> {
            ensure!(
                !self.cancel.is_cancelled(),
                "owned task allocator stopped; no new inspection"
            );
            let retained = self
                .retained
                .lock()
                .map_err(|_| anyhow::anyhow!("owned task allocator poisoned; no retry"))?;
            if let Some(owner) = retained.as_ref() {
                ensure!(
                    owner.resource_scope == parent.resource_scope(),
                    "owned task allocator context scope changed"
                );
                parent.require_current_binding(owner.binding.as_ref())?;
                Ok(
                    parent.with_owned_task_allocation_owner(Arc::new(OwnedTaskAllocationOwner {
                        _operation: owner.operation.clone(),
                    })),
                )
            } else {
                Ok(parent.clone())
            }
        };
        let result = context();
        if result.is_err() {
            self.stop();
        }
        result
    }

    pub(crate) fn execute(&self, prepared: PreparedOwnedTask) -> Result<OwnedTask> {
        let execute = || -> Result<OwnedTask> {
            ensure!(
                !self.cancel.is_cancelled(),
                "owned task allocator stopped; no retry"
            );
            let ctx = crate::core::exec_ctx::current().unwrap_or_default();
            ensure!(
                ctx.can_restore_workspace() && ctx.native_scope().is_none(),
                "owned task allocator requires its unrestricted coordinating parent"
            );
            let mut retained = self
                .retained
                .lock()
                .map_err(|_| anyhow::anyhow!("owned task allocator poisoned; no retry"))?;
            ensure!(
                !self.cancel.is_cancelled(),
                "owned task allocator stopped; no retry"
            );
            let parent_scope = prepared.parent_scope.clone();
            let mut task = if let Some(owner) = retained.as_ref() {
                ensure!(
                    owner.parent_scope == parent_scope
                        && owner.resource_scope == ctx.resource_scope(),
                    "owned task allocator parent scope changed"
                );
                ctx.require_current_binding(owner.binding.as_ref())?;
                prepared
                    .creation
                    .execute_for_task_with_owner(&prepared.scope, owner.operation.clone())?
            } else {
                prepared.execute()?
            };
            task.cohort_cancel = Some(self.cancel.clone());
            if retained.is_none() {
                *retained = Some(RetainedAllocationOwner {
                    operation: task.owner.clone(),
                    parent_scope,
                    resource_scope: ctx.resource_scope(),
                    binding: ctx.workspace_binding().cloned(),
                });
            }
            Ok(task)
        };
        let result = crate::core::cancel::with_current(self.cancel.clone(), execute);
        if result.is_err() {
            self.stop();
        }
        result
    }
}

pub(crate) struct OwnedTask {
    identity: WorkspaceIdentity,
    branch: String,
    base: GitOid,
    scope: String,
    registration: crate::features::coop::GitTaskRegistration,
    // Includes the repository registration barrier through all child/process settlement.
    owner: Owner,
    cohort_cancel: Option<crate::core::cancel::TurnCancel>,
    writer: crate::core::workspace_txn::WorkspaceWriterLease,
    witnesses: Vec<NativeResource>,
}

impl std::fmt::Debug for OwnedTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedTask")
            .field("root", &self.identity.canonical_root)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskHandoff {
    pub repo_id: String,
    pub worktree_id: String,
    pub root: PathBuf,
    pub branch: String,
    pub owner: String,
    pub registration: crate::features::coop::GitTaskReceipt,
    pub base: GitOid,
    pub tip: GitOid,
    pub commits: Vec<GitOid>,
    pub paths: Vec<String>,
    pub dirty: bool,
    pub verified: bool,
    pub integrated: bool,
    pub tests: String,
    /// Git-generated inverse provenance; absent for ordinary native-only task handoffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<super::change::PathInverse>,
}

impl OwnedTask {
    pub(super) fn from_created(
        created: &CreatedWorktree,
        scope: &str,
        owner: &Owner,
    ) -> Result<Self> {
        ensure!(
            !created.dirty,
            "checkout hook left changes; worktree retained without task ownership"
        );
        let identity = inspect_identity(&created.path, Some(owner))?;
        super::change::require_clean(&identity, Some(owner))?;
        ensure!(
            head(&created.path, Some(owner))? == created.base,
            "created task base moved"
        );
        let branch = format!("refs/heads/{}", created.branch);
        ensure!(
            source_branch(&created.path, Some(owner))?.as_deref() == Some(&branch),
            "created task branch moved"
        );
        let witnesses = [
            &identity.canonical_root,
            identity
                .canonical_git_dir
                .as_ref()
                .context("missing task Git dir")?,
            identity
                .canonical_common_git_dir
                .as_ref()
                .context("missing task common dir")?,
        ]
        .into_iter()
        .map(|p| NativeResource::resolve(&p.join(".aizen-task-boundary")))
        .collect::<Result<Vec<_>>>()?;
        let writer = crate::core::workspace_txn::WorkspaceWriterLease::acquire_identity(
            identity.clone(),
            Duration::from_secs(15),
            crate::core::cancel::current().as_ref(),
            "owned task checkout",
        )?;
        let _physical = physical()?;
        let registration = crate::features::coop::GitTaskRegistration::create(
            &identity,
            &branch,
            scope,
            &created.base,
            owner
                .registration
                .as_ref()
                .context("task creation requires registration barrier")?,
        )?;
        Ok(Self {
            identity,
            branch,
            base: created.base.clone(),
            scope: scope.into(),
            registration,
            owner: owner.clone(),
            cohort_cancel: None,
            writer,
            witnesses,
        })
    }
    pub(crate) fn root(&self) -> &Path {
        &self.identity.canonical_root
    }
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    /// Called by the coordinating parent after baseline capture, before a child context or driver
    /// is exposed. Git inspection remains outside the native-only child and outside physical IO.
    pub(crate) fn begin_native_attempt(
        self: &Arc<Self>,
        scope: &NativeWriteScope,
    ) -> Result<GitNativeAttempt> {
        let context = crate::core::exec_ctx::current().unwrap_or_default();
        ensure!(
            context.can_restore_workspace() && context.native_scope().is_none(),
            "native attempt admission requires its coordinating parent"
        );
        self.verify(&self.base)?;
        super::change::require_clean(&self.identity, Some(&self.owner))?;
        let binding = self.registration.begin_native(&self.identity, scope)?;
        Ok(GitNativeAttempt::new(self.clone(), scope.clone(), binding))
    }

    /// Only the live lifecycle ticket calls this metadata-only operation. No Git runner or source
    /// IO is entered while the worker supervisor's admission mutex is held.
    pub(crate) fn publish_native_intent(
        &self,
        binding: &NativeBinding,
        kind: crate::core::worker_evidence::NativeWorkerKind,
        digest: [u8; 32],
    ) -> Result<crate::core::worker_evidence::WorkerJoinFact> {
        self.owner.verify_registration()?;
        binding.require_identity(&self.identity)?;
        self.registration
            .publish_native_intent(binding, kind, digest)
    }

    fn verify(&self, expected_head: &GitOid) -> Result<()> {
        ensure!(
            !self
                .cohort_cancel
                .as_ref()
                .is_some_and(crate::core::cancel::TurnCancel::is_cancelled),
            "owned task cohort stopped; retain worktree and ownership evidence, no commit or retry"
        );
        self.owner.verify_registration()?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        ensure!(
            inspect_identity(self.root(), Some(&self.owner))? == self.identity,
            "owned worktree binding moved"
        );
        super::change::require_unmodified_history(&self.identity, Some(&self.owner))?;
        super::require_idle_operation(
            self.identity
                .canonical_git_dir
                .as_ref()
                .context("missing task Git dir")?,
        )?;
        super::validate_visible_index(&query(
            self.root(),
            &["ls-files", "-v", "-f", "-z"],
            Some(&self.owner),
        )?)?;
        ensure!(
            head(self.root(), Some(&self.owner))? == *expected_head
                && source_branch(self.root(), Some(&self.owner))?.as_deref() == Some(&self.branch),
            "owned branch/HEAD changed; no automatic commit"
        );
        self.registration.verify(&self.identity)?;
        super::require_settled_operations(&self.identity)
    }

    pub(crate) fn capture_baseline(
        &self,
        scope: &NativeWriteScope,
    ) -> Result<Vec<(String, crate::core::persist::FileFingerprint)>> {
        self.verify(&self.base)?;
        super::change::require_clean(&self.identity, Some(&self.owner))?;
        let _physical = physical()?;
        scope
            .resources()
            .iter()
            .map(|resource| {
                ensure!(
                    resource.path().starts_with(self.root()),
                    "task scope escapes worktree"
                );
                resource.verify()?;
                let fingerprint = match std::fs::symlink_metadata(resource.path()) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        crate::core::persist::FileFingerprint::missing()
                    }
                    Err(e) => return Err(e.into()),
                    Ok(_) => crate::core::persist::FileFingerprint::for_bytes(
                        &crate::core::file_placement::read_existing_regular(
                            resource.path(),
                            4 * 1024 * 1024,
                        )?,
                    ),
                };
                resource.verify()?;
                Ok((resource.key().into(), fingerprint))
            })
            .collect()
    }

    /// Consume actual joins/carrier retirement/resource release before publishing terminal facts.
    /// A failed publication may already be durable: poison the attempt and never retry the token.
    pub(crate) fn finish_native(
        self: Arc<Self>,
        scope: &NativeWriteScope,
        baseline: &[(String, crate::core::persist::FileFingerprint)],
        released: crate::agent::git_native_attempt::ReleasedGitAttempt,
    ) -> Result<TaskHandoff> {
        let cohort = self.cohort_cancel.clone();
        let attempt = released.attempt().clone();
        let result = (|| {
            ensure!(
                attempt.belongs_to(&self) && attempt.scope().same_authority(scope),
                "native release belongs to another live owned task or scope"
            );
            let closure = attempt.consume_release(released)?;
            let candidate = self.registration.publish_native_release(closure)?;
            ensure!(candidate, "native attempt physically closed without an eligible candidate; retained, no commit or retry");
            self.finish_inner(scope, baseline)
        })();
        if result.is_err() {
            attempt.poison();
            if let Some(cohort) = cohort {
                cohort.cancel();
            }
        }
        result
    }

    fn finish_inner(
        self: Arc<Self>,
        scope: &NativeWriteScope,
        baseline: &[(String, crate::core::persist::FileFingerprint)],
    ) -> Result<TaskHandoff> {
        self.verify(&self.base)?;
        let paths = changed_paths(&query(
            self.root(),
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
            Some(&self.owner),
        )?)?;
        let resources = paths
            .iter()
            .map(|p| {
                let resource = NativeResource::resolve(&self.root().join(p))?;
                ensure!(
                    resource.path().starts_with(self.root()),
                    "task path escapes owned workspace"
                );
                scope.check_resource(&resource)?;
                Ok(resource)
            })
            .collect::<Result<Vec<_>>>()?;
        // Keep mode-only changes, symlinks and executable-bit changes outside the native byte contract.
        for resource in &resources {
            let path = relative(self.root(), resource.path())?;
            let entries = query(
                self.root(),
                &["ls-files", "--stage", "-z", "--", &path],
                Some(&self.owner),
            )?;
            ensure!(
                entries.is_empty() || entries.starts_with(b"100644 "),
                "unsupported native commit mode"
            );
        }
        let proof = {
            let physical = physical()?;
            crate::core::speculation::legacy::require_settled(
                self.root(),
                &self.writer,
                &physical,
            )?;
            crate::core::native_journal::require_workspace_settled(self.root())?;
            if resources.is_empty() {
                Vec::new()
            } else {
                crate::core::native_journal::prove_current_owner_postimages(
                    &self.scope,
                    &resources,
                )?
            }
        };
        for image in &proof {
            ensure!(
                baseline
                    .iter()
                    .any(|(key, before)| key == &image.key && before == &image.before),
                "task write carries content outside its clean baseline; no automatic commit"
            );
        }
        let tip = if resources.is_empty() {
            self.base.clone()
        } else {
            self.registration
                .record("committing", &self.base, paths.clone())?;
            self.verify(&self.base)?;
            let mut args = vec!["--literal-pathspecs".into(), "add".into(), "--".into()];
            args.extend(paths.clone());
            self.mutate(args)?;
            self.verify(&self.base)?;
            {
                let _physical = physical()?;
                ensure!(
                    crate::core::native_journal::prove_current_owner_postimages(
                        &self.scope,
                        &resources
                    )? == proof,
                    "task postimages changed while staging"
                );
            }
            let staged = nul_paths(&query(
                self.root(),
                &[
                    "diff",
                    "--cached",
                    "--name-only",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "-z",
                ],
                Some(&self.owner),
            )?)?;
            ensure!(
                staged == paths,
                "staged path set differs from proven task paths; retained for inspection"
            );
            let entries = query(
                self.root(),
                &["ls-files", "--stage", "-z"],
                Some(&self.owner),
            )?;
            ensure!(
                !entries
                    .split(|b| *b == 0)
                    .any(|r| r.starts_with(b"160000 ")),
                "submodule task commits are unsupported"
            );
            let summary = query(
                self.root(),
                &[
                    "diff",
                    "--cached",
                    "--summary",
                    "--no-ext-diff",
                    "--no-textconv",
                ],
                Some(&self.owner),
            )?;
            ensure!(
                !String::from_utf8_lossy(&summary).contains("mode change"),
                "mode changes have no native byte attribution"
            );
            let tree = self.mutate(vec!["write-tree".into()])?;
            let tree = GitOid::parse(std::str::from_utf8(&tree)?.trim())?;
            self.verify(&self.base)?;
            self.mutate(vec![
                "commit".into(),
                "-m".into(),
                "Complete isolated native task".into(),
            ])?;
            let tip = head(self.root(), Some(&self.owner))?;
            self.verify(&tip)?;
            let actual = query(
                self.root(),
                &["show", "-s", "--format=%T%n%P", tip.as_str()],
                Some(&self.owner),
            )?;
            let actual = std::str::from_utf8(&actual)?;
            let mut lines = actual.lines();
            ensure!(
                lines.next() == Some(tree.as_str())
                    && lines.next() == Some(self.base.as_str())
                    && lines.next().is_none(),
                "commit hook changed tree/parents; retain commit and inspect, no handoff"
            );
            tip
        };
        self.verify(&tip)?;
        super::change::require_clean(&self.identity, Some(&self.owner))?;
        self.registration
            .record("handed_off", &tip, paths.clone())?;
        let registration = self.registration.handoff_receipt()?;
        ensure!(registration.owner == self.scope, "handoff owner changed");
        Ok(TaskHandoff {
            repo_id: self.identity.repo_id.clone(),
            worktree_id: self.identity.worktree_id.clone(),
            root: self.root().to_path_buf(),
            branch: self.branch.clone(),
            owner: self.scope.clone(),
            registration,
            base: self.base.clone(),
            commits: if tip == self.base {
                Vec::new()
            } else {
                vec![tip.clone()]
            },
            tip,
            paths,
            dirty: false,
            verified: false,
            integrated: false,
            tests: "not_run_native_only".into(),
            inverse: None,
        })
    }
    /// Only this controller can apply the frozen Git delta; native attribution is not fabricated.
    pub(super) fn finish_inverse(
        self: Arc<Self>,
        inverse: &super::change::PathInverse,
        patch: Vec<u8>,
    ) -> Result<TaskHandoff> {
        inverse.validate()?;
        self.verify(&self.base)?;
        super::change::require_clean(&self.identity, Some(&self.owner))?;
        {
            let physical = physical()?;
            crate::core::speculation::legacy::require_settled(
                self.root(),
                &self.writer,
                &physical,
            )?;
            crate::core::native_journal::require_workspace_settled(self.root())?;
            self.registration.start_inverse(inverse)?;
        }
        // Publish the admitting phase before spawn. A crash/failure remains an unfinished owner,
        // even without a Git sequencer marker (git apply can leave an unmerged index).
        if let Some(context) = crate::core::exec_ctx::current() {
            context.note_task_effects_at(Some(self.root()));
        }
        crate::agent::codebase::invalidate_cache();
        self.mutate_input(
            vec![
                "apply".into(),
                "--3way".into(),
                "--index".into(),
                "--whitespace=nowarn".into(),
                "-".into(),
            ],
            Some(patch),
        )?;
        self.verify(&self.base)?;
        ensure!(
            query(
                self.root(),
                &[
                    "diff",
                    "--name-only",
                    "--no-ext-diff",
                    "--no-textconv",
                    "-z"
                ],
                Some(&self.owner)
            )?
            .is_empty(),
            "inverse left unstaged changes; retain owned checkout for inspection"
        );
        let paths = nul_paths(&query(
            self.root(),
            &[
                "diff",
                "--cached",
                "--name-only",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "-z",
            ],
            Some(&self.owner),
        )?)?;
        ensure!(
            !paths.is_empty() && paths.iter().all(|p| inverse.paths.contains(p)),
            "inverse staged unexpected/empty paths; no automatic commit"
        );
        let tree = self.mutate(vec!["write-tree".into()])?;
        let tree = GitOid::parse(std::str::from_utf8(&tree)?.trim())?;
        self.registration
            .record("committing_inverse", &self.base, paths.clone())?;
        self.mutate(vec![
            "commit".into(),
            "-m".into(),
            format!("Undo selected paths from {}", inverse.commit.as_str()),
        ])?;
        let tip = head(self.root(), Some(&self.owner))?;
        self.verify(&tip)?;
        let actual = query(
            self.root(),
            &["show", "-s", "--format=%T%n%P", tip.as_str()],
            Some(&self.owner),
        )?;
        let actual = std::str::from_utf8(&actual)?;
        let mut lines = actual.lines();
        ensure!(
            lines.next() == Some(tree.as_str())
                && lines.next() == Some(self.base.as_str())
                && lines.next().is_none(),
            "inverse commit hook changed tree/parents; no handoff"
        );
        super::change::require_clean(&self.identity, Some(&self.owner))?;
        self.registration
            .record("handed_off", &tip, paths.clone())?;
        Ok(TaskHandoff {
            repo_id: self.identity.repo_id.clone(),
            worktree_id: self.identity.worktree_id.clone(),
            root: self.root().to_path_buf(),
            branch: self.branch.clone(),
            owner: self.scope.clone(),
            registration: self.registration.handoff_receipt()?,
            base: self.base.clone(),
            tip: tip.clone(),
            commits: vec![tip],
            paths,
            dirty: false,
            verified: false,
            integrated: false,
            tests: "not_run_selected_inverse".into(),
            inverse: Some(inverse.clone()),
        })
    }

    fn mutate(self: &Arc<Self>, args: Vec<String>) -> Result<Vec<u8>> {
        self.mutate_input(args, None)
    }
    fn mutate_input(
        self: &Arc<Self>,
        args: Vec<String>,
        input: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        self.owner.verify_registration()?;
        let prepared = prepare_mutation(self.root(), self.root(), args)?;
        let result = checked_output(prepared.output_bounded_owned(
            input,
            INSPECT_TIMEOUT,
            Duration::from_secs(2),
            Some(Box::new(self.clone())),
        ))?;
        self.owner.verify_registration()?;
        Ok(result.stdout)
    }
}

fn physical() -> Result<PhysicalEffectLease> {
    let ctx = crate::core::exec_ctx::current().unwrap_or_default();
    PhysicalEffectLease::exclusive(
        Duration::from_secs(15),
        crate::core::cancel::current().as_ref(),
        "owned Git task proof",
        &ctx.resource_scope(),
    )
}
fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)?
        .to_str()
        .context("non-UTF-8 task path")?
        .replace('\\', "/"))
}
pub(super) fn nul_paths(bytes: &[u8]) -> Result<Vec<String>> {
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "incomplete Git paths"
    );
    let mut paths = BTreeSet::new();
    for path in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(path)?;
        validate_relative(path)?;
        ensure!(paths.insert(path.to_owned()), "duplicate Git path");
    }
    ensure!(paths.len() <= 32, "owned task path budget exceeded");
    Ok(paths.into_iter().collect())
}
fn changed_paths(bytes: &[u8]) -> Result<Vec<String>> {
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "incomplete Git status"
    );
    let mut names = Vec::new();
    for row in bytes.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        ensure!(row.len() > 3 && (&row[..3] == b" M " || &row[..3] == b"?? "), "unexpected staging, deletion, mode/type, rename, conflict or ignored files; no automatic commit");
        names.extend_from_slice(&row[3..]);
        names.push(0);
    }
    nul_paths(&names)
}
pub(super) fn validate_relative(path: &str) -> Result<()> {
    let p = Path::new(path);
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.chars().any(char::is_control)
            && !p.is_absolute()
            && p.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            && !p
                .components()
                .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
        "invalid exact relative task path"
    );
    NativeResource::validate_spelling(p)
}

#[cfg(test)]
#[path = "git_ops_owned_tests.rs"]
mod tests;
