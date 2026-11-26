//! Task owners use the existing co-op registry and liveness protocol, not a branch-name heuristic.
use super::*;
use crate::core::git_native_lifecycle::{
    NativeBinding, NativeClosure, NativeLifecycle, MAX_MANIFEST_BYTES, REGISTRATION_PREFIX,
};
use crate::core::native_scope::NativeWriteScope;
use crate::core::worker_evidence::{NativeWorkerKind, WorkerJoinFact};
use crate::core::{persist::FileFingerprint, snapshots::GitOid, workspace_txn::NativeResource};
use std::sync::atomic::{AtomicBool, Ordering};

/// Strict v2 metadata validation is shared by live ownership and read-only admission barriers.
/// Legacy absence remains readable; it cannot create a live registration or release capability.
pub(super) fn validate_manifest(manifest: &SessionManifest) -> Result<()> {
    let task = manifest
        .git_task
        .as_ref()
        .context("Git task metadata missing")?;
    if manifest.session_id.starts_with(REGISTRATION_PREFIX) {
        anyhow::ensure!(
            task.native_lifecycle.is_some(),
            "v2 Git task is missing required native lifecycle evidence"
        );
    }
    if let Some(native) = &task.native_lifecycle {
        native.validate_manifest(manifest)?;
        if task.phase == "handed_off" {
            anyhow::ensure!(
                manifest.state == SessionState::Finished,
                "native handoff state is inconsistent"
            );
            native.require_transition(task.inverse.is_some())?;
        }
    }
    Ok(())
}

fn encode_manifest(manifest: &SessionManifest) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    anyhow::ensure!(
        bytes.len() <= MAX_MANIFEST_BYTES,
        "Git task manifest exceeds 128 KiB; retained, no launch or retry"
    );
    Ok(bytes)
}

/// Historical ownership evidence only. Deserializing this record never adopts a workspace or
/// grants commit/integration authority; consumers must check the current registry and Git state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitTaskReceipt {
    pub registration_id: String,
    pub owner: String,
    pub manifest_sha256: [u8; 32],
}

/// Exact registry bytes selected before integration approval. Stored workspace paths are data;
/// the Git layer must resolve them against its own repository inventory before inspecting files.
#[derive(Debug)]
pub(crate) struct FrozenGitTask {
    pub manifest: SessionManifest,
    pub receipt: GitTaskReceipt,
    path: PathBuf,
    resource: NativeResource,
    fingerprint: FileFingerprint,
}

impl FrozenGitTask {
    pub(crate) fn select(identity: &WorkspaceIdentity, id: &str) -> Result<Self> {
        anyhow::ensure!(
            id.starts_with("git-task-") && crate::core::git_ops::valid_operation_id(id),
            "select an exact Git task registration ID from its handoff"
        );
        let path = manifest_path(&identity.repo_id, id);
        let (_, boundary) = crate::core::speculation::namespace::plan_metadata_directory(
            path.parent().context("missing task registry directory")?,
        )?;
        boundary.verify()?;
        let resource = NativeResource::resolve(&path)?;
        resource.verify()?;
        let bytes = crate::core::file_placement::read_existing_regular(&path, 128 * 1024)?;
        let manifest: SessionManifest = serde_json::from_slice(&bytes)
            .context("Git task registration is unreadable; no integration")?;
        let task = manifest
            .git_task
            .as_ref()
            .context("registration is not a Git-owned task")?;
        validate_manifest(&manifest)?;
        anyhow::ensure!(
            manifest.schema == SCHEMA
                && manifest.repo_id == identity.repo_id
                && manifest.session_id == id
                && !manifest.worktree_id.is_empty()
                && manifest.state == SessionState::Finished
                && task.phase == "handed_off"
                && !task.verified
                && !manifest.coverage_gap
                && manifest.degraded.is_none()
                && manifest.base_checkpoint.is_none()
                && manifest.files.is_empty()
                && manifest.truncated_files == 0
                && !task.owner.is_empty()
                && task.owner.len() <= 1000
                && task.owner.trim() == task.owner
                && !task.owner.chars().any(char::is_control),
            "registration is not a settled Git task handoff in this repository"
        );
        let fingerprint = FileFingerprint::for_bytes(&bytes);
        let receipt = GitTaskReceipt {
            registration_id: id.into(),
            owner: task.owner.clone(),
            manifest_sha256: fingerprint.sha256,
        };
        let selected = Self {
            manifest,
            receipt,
            path,
            resource,
            fingerprint,
        };
        selected.revalidate()?;
        boundary.verify()?;
        Ok(selected)
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        self.resource.verify()?;
        let bytes = crate::core::file_placement::read_existing_regular(&self.path, 128 * 1024)?;
        anyhow::ensure!(
            FileFingerprint::for_bytes(&bytes) == self.fingerprint,
            "selected task registration changed; prepare integration again"
        );
        // Unknown liveness is an error. A completed report cannot release a still-running owner.
        anyhow::ensure!(
            !existing_session_alive(&self.path.with_extension("lock"))?,
            "Git task owner is still live; wait for physical settlement before integration"
        );
        self.resource.verify()
    }
}

struct NativeCapacity {
    bytes: usize,
    paths: Vec<String>,
}

impl NativeCapacity {
    fn require_paths(&self, paths: &[String]) -> Result<()> {
        let mut selected = std::collections::BTreeSet::new();
        for path in paths {
            let index = self
                .paths
                .iter()
                .position(|approved| {
                    // Match NativeResource's host identity semantics. Case-folding hosts permit
                    // ASCII case aliases only, which preserve the reserved JSON byte budget.
                    #[cfg(any(windows, target_os = "macos"))]
                    {
                        approved.eq_ignore_ascii_case(path)
                    }
                    #[cfg(not(any(windows, target_os = "macos")))]
                    {
                        approved == path
                    }
                })
                .context("Git task path is outside its reserved native authority")?;
            anyhow::ensure!(
                selected.insert(index),
                "duplicate native handoff path identity"
            );
        }
        Ok(())
    }
}

pub(crate) struct GitTaskRegistration {
    lock: RepoTxnLock,
    path: PathBuf,
    resource: NativeResource,
    manifest: Mutex<(SessionManifest, FileFingerprint, Option<NativeCapacity>)>,
    publication_poisoned: AtomicBool,
    #[cfg(test)]
    publication_fault: Mutex<Option<bool>>,
}

impl GitTaskRegistration {
    /// Called only by the allocator after verifying a newly-created, clean worktree.
    pub(crate) fn create(
        identity: &WorkspaceIdentity,
        branch: &str,
        owner: &str,
        base: &GitOid,
        barrier: &WorkspaceRegistrationBarrier,
    ) -> Result<Self> {
        barrier.verify()?;
        anyhow::ensure!(
            barrier.repo_id == identity.repo_id,
            "wrong registration repository"
        );
        require_no_other_workspace_owner_except(identity, None)?;
        let session_id = format!(
            "{REGISTRATION_PREFIX}{}-{}-{}",
            now_unix(),
            std::process::id(),
            crate::core::persist::unique_sequence()
        );
        ensure_registration_directory(&sessions_dir(&identity.repo_id))?;
        let path = manifest_path(&identity.repo_id, &session_id);
        anyhow::ensure!(!path.try_exists()?, "task registration already exists");
        let lock = RepoTxnLock::acquire_in_existing_parent(
            &liveness_lock_path(&identity.repo_id, &session_id),
            crate::core::repo_lock::LockMode::Exclusive,
            Duration::from_secs(1),
            None,
        )?;
        let manifest = SessionManifest {
            schema: SCHEMA,
            session_id,
            pid: std::process::id(),
            binary_version: env!("CARGO_PKG_VERSION").into(),
            repo_id: identity.repo_id.clone(),
            worktree_id: identity.worktree_id.clone(),
            root: identity.canonical_root.display().to_string(),
            branch: Some(branch.into()),
            session_name: None,
            task: "Git-owned native task; no integration or verification".into(),
            task_pinned: true,
            state: SessionState::Working,
            turns: 0,
            base_checkpoint: None,
            files: Vec::new(),
            truncated_files: 0,
            degraded: None,
            coverage_gap: false,
            started_unix: now_unix(),
            updated_unix: now_unix(),
            git_task: Some(GitTaskState {
                native_lifecycle: Some(NativeLifecycle::required()),
                owner: owner.into(),
                base: base.clone(),
                tip: base.clone(),
                phase: "running".into(),
                inverse: None,
                paths: Vec::new(),
                verified: false,
            }),
        };
        validate_manifest(&manifest)?;
        let bytes = encode_manifest(&manifest)?;
        let resource = NativeResource::resolve(&path)?;
        resource.verify()?;
        crate::core::persist::atomic_write_owner_only_checked(&path, &bytes)?;
        resource.verify()?;
        barrier.verify()?;
        Ok(Self {
            lock,
            path,
            resource,
            manifest: Mutex::new((manifest, FileFingerprint::for_bytes(&bytes), None)),
            publication_poisoned: AtomicBool::new(false),
            #[cfg(test)]
            publication_fault: Mutex::new(None),
        })
    }

    pub(crate) fn verify(&self, identity: &WorkspaceIdentity) -> Result<()> {
        let state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        anyhow::ensure!(
            state.0.repo_id == identity.repo_id
                && state.0.worktree_id == identity.worktree_id
                && Path::new(&state.0.root) == identity.canonical_root,
            "task workspace binding changed"
        );
        if let Some(binding) = state
            .0
            .git_task
            .as_ref()
            .and_then(|task| task.native_lifecycle.as_ref())
            .and_then(NativeLifecycle::binding)
        {
            binding.require_identity(identity)?;
        }
        require_no_other_workspace_owner_except(identity, Some(&state.0.session_id))
    }

    /// Read back the exact settled registration after handoff publication. The digest is evidence
    /// of these bytes, not a lease and not authority to adopt or replay a task after restart.
    pub(crate) fn handoff_receipt(&self) -> Result<GitTaskReceipt> {
        let state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        let task = state
            .0
            .git_task
            .as_ref()
            .context("task ownership missing")?;
        anyhow::ensure!(
            state.0.state == SessionState::Finished && task.phase == "handed_off" && !task.verified,
            "task registration has no completed unverified handoff"
        );
        Ok(GitTaskReceipt {
            registration_id: state.0.session_id.clone(),
            owner: task.owner.clone(),
            manifest_sha256: state.1.sha256,
        })
    }

    fn verify_locked(
        &self,
        state: &(SessionManifest, FileFingerprint, Option<NativeCapacity>),
    ) -> Result<()> {
        anyhow::ensure!(
            !self.publication_poisoned.load(Ordering::Acquire),
            "task metadata publication uncertain; retain evidence, no retry"
        );
        validate_manifest(&state.0)?;
        self.resource.verify()?;
        self.lock
            .verify_existing_path(&self.path.with_extension("lock"))?;
        let bytes = crate::core::file_placement::read_existing_regular(&self.path, 128 * 1024)?;
        anyhow::ensure!(
            FileFingerprint::for_bytes(&bytes) == state.1,
            "task ownership metadata changed; no adoption or retry"
        );
        Ok(())
    }

    /// Freeze exact metadata and the complete future terminal envelope before exposing a child.
    pub(crate) fn begin_native(
        &self,
        identity: &WorkspaceIdentity,
        scope: &NativeWriteScope,
    ) -> Result<NativeBinding> {
        let mut state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        let task = state
            .0
            .git_task
            .as_ref()
            .context("task ownership missing")?;
        anyhow::ensure!(
            task.phase == "running" && task.base == task.tip && task.inverse.is_none(),
            "native task cannot adopt or replay an existing owner"
        );
        task.native_lifecycle
            .as_ref()
            .context("historical task lacks live native lifecycle")?
            .require_unstarted()?;
        let binding = NativeBinding::new(
            identity,
            &state.0.session_id,
            state.0.branch.as_deref().context("task branch missing")?,
            &task.owner,
            &task.base,
            scope,
        )?;
        binding.require_manifest(&state.0)?;
        let mut manifest = state.0.clone();
        manifest
            .git_task
            .as_mut()
            .context("task ownership missing")?
            .native_lifecycle
            .as_mut()
            .context("native lifecycle missing")?
            .begin(binding.clone())?;
        let mut terminal = manifest.clone();
        terminal.state = SessionState::Finished;
        terminal.started_unix = u64::MAX;
        terminal.updated_unix = u64::MAX;
        let terminal_task = terminal
            .git_task
            .as_mut()
            .context("task ownership missing")?;
        terminal_task.native_lifecycle =
            Some(NativeLifecycle::reservation_envelope(binding.clone()));
        terminal_task.phase = "committing_inverse".into(); // Longest allowed registration phase.
        terminal_task.tip = GitOid::parse(&"f".repeat(64))?;
        terminal_task.paths = scope
            .resources()
            .iter()
            .filter(|resource| scope.write_keys().iter().any(|key| key == resource.key()))
            .map(|resource| -> Result<String> {
                let relative = resource.path().strip_prefix(&identity.canonical_root)?;
                let path = relative
                    .to_str()
                    .context("native task requires UTF-8 relative paths")?
                    .replace('\\', "/");
                anyhow::ensure!(
                    !path.is_empty() && path.len() <= 4096,
                    "native task path exceeds terminal reservation bound"
                );
                Ok(path)
            })
            .collect::<Result<Vec<_>>>()?;
        terminal_task.paths.sort();
        let approved_paths = terminal_task.paths.clone();
        let reserved = encode_manifest(&terminal)?.len();
        // This is a logical capacity reservation for this exclusive per-task manifest, not a
        // promise of free disk space. IO failure still poisons the owner and retains its evidence.
        state.2 = Some(NativeCapacity {
            bytes: reserved,
            paths: approved_paths,
        });
        manifest.updated_unix = now_unix();
        self.save_locked(&mut state, manifest)?;
        Ok(binding)
    }

    pub(crate) fn publish_native_intent(
        &self,
        binding: &NativeBinding,
        kind: NativeWorkerKind,
        digest: [u8; 32],
    ) -> Result<WorkerJoinFact> {
        let mut state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        anyhow::ensure!(
            state.2.is_some(),
            "native terminal capacity was not reserved"
        );
        let mut manifest = state.0.clone();
        let task = manifest
            .git_task
            .as_mut()
            .context("task ownership missing")?;
        anyhow::ensure!(
            task.phase == "running",
            "native intent cannot follow commit or handoff"
        );
        let fact = task
            .native_lifecycle
            .as_mut()
            .context("native lifecycle missing")?
            .append_intent(binding, kind, digest)?;
        manifest.updated_unix = now_unix();
        self.save_locked(&mut state, manifest)?;
        Ok(fact)
    }

    /// No raw facts/booleans accepted: NativeClosure can only consume a real runtime release token.
    pub(crate) fn publish_native_release(&self, closure: NativeClosure) -> Result<bool> {
        let mut state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        anyhow::ensure!(
            state.2.is_some(),
            "native terminal capacity was not reserved"
        );
        let mut manifest = state.0.clone();
        let task = manifest
            .git_task
            .as_mut()
            .context("task ownership missing")?;
        anyhow::ensure!(
            task.phase == "running",
            "native release cannot follow commit or handoff"
        );
        let candidate = closure.apply(
            task.native_lifecycle
                .as_mut()
                .context("native lifecycle missing")?,
        )?;
        manifest.updated_unix = now_unix();
        self.save_locked(&mut state, manifest)?;
        Ok(candidate)
    }

    pub(crate) fn start_inverse(&self, inverse: &crate::core::git_ops::PathInverse) -> Result<()> {
        inverse.validate()?;
        let mut state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        let mut manifest = state.0.clone();
        let task = manifest
            .git_task
            .as_mut()
            .context("task ownership missing")?;
        anyhow::ensure!(
            task.phase == "running" && task.base == task.tip && task.inverse.is_none(),
            "selected inverse cannot adopt or replay an existing task"
        );
        task.native_lifecycle
            .as_mut()
            .context("inverse requires a fresh typed task registration")?
            .start_inverse(inverse)?;
        task.inverse = Some(inverse.clone());
        task.phase = "applying_inverse".into();
        manifest.updated_unix = now_unix();
        self.save_locked(&mut state, manifest)
    }

    pub(crate) fn record(&self, phase: &str, tip: &GitOid, paths: Vec<String>) -> Result<()> {
        let mut state = self
            .manifest
            .lock()
            .map_err(|_| anyhow::anyhow!("task registration poisoned"))?;
        self.verify_locked(&state)?;
        let mut manifest = state.0.clone();
        let task = manifest
            .git_task
            .as_mut()
            .context("task ownership missing")?;
        let inverse = task.inverse.is_some();
        if !inverse {
            state
                .2
                .as_ref()
                .context("native terminal capacity was not reserved")?
                .require_paths(&paths)?;
        }
        task.native_lifecycle
            .as_ref()
            .context("historical metadata cannot authorize new task completion")?
            .require_transition(inverse)?;
        let valid = matches!(
            (inverse, task.phase.as_str(), phase),
            (false, "running", "committing" | "handed_off")
                | (false, "committing", "handed_off")
                | (true, "applying_inverse", "committing_inverse")
                | (true, "committing_inverse", "handed_off")
        );
        anyhow::ensure!(valid, "invalid or repeated Git task lifecycle transition");
        anyhow::ensure!(
            paths.len() <= 32
                && paths
                    .iter()
                    .all(|path| !path.is_empty() && path.len() <= 4096),
            "Git task handoff paths exceed bound"
        );
        task.phase = phase.into();
        task.tip = tip.clone();
        task.paths = paths;
        manifest.updated_unix = now_unix();
        if phase == "handed_off" {
            manifest.state = SessionState::Finished;
        }
        self.save_locked(&mut state, manifest)
    }

    #[cfg(test)]
    pub(crate) fn check_reserved_native_paths_for_test(&self, paths: &[String]) -> Result<()> {
        self.manifest
            .lock()
            .unwrap()
            .2
            .as_ref()
            .context("native reservation missing")?
            .require_paths(paths)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_native_publication_for_test(&self, after_replace: bool) {
        *self.publication_fault.lock().unwrap() = Some(after_replace);
    }

    #[cfg(test)]
    fn publication_fault_for_test(&self, after_replace: bool) -> Result<()> {
        let mut fault = self.publication_fault.lock().unwrap();
        if *fault == Some(after_replace) {
            *fault = None;
            anyhow::bail!("injected native metadata publication fault");
        }
        Ok(())
    }

    fn save_locked(
        &self,
        state: &mut (SessionManifest, FileFingerprint, Option<NativeCapacity>),
        manifest: SessionManifest,
    ) -> Result<()> {
        // Set BEFORE any IO. An error before or after rename is never permission to retry,
        // including when the old fingerprint still happens to match the visible file.
        anyhow::ensure!(
            !self.publication_poisoned.swap(true, Ordering::AcqRel),
            "task metadata publication already uncertain"
        );
        validate_manifest(&manifest)?;
        let bytes = encode_manifest(&manifest)?;
        if let Some(reserved) = &state.2 {
            anyhow::ensure!(
                bytes.len() <= reserved.bytes,
                "native terminal reservation exceeded; retain evidence, no retry"
            );
        }
        self.resource.verify()?;
        self.lock
            .verify_existing_path(&self.path.with_extension("lock"))?;
        #[cfg(test)]
        self.publication_fault_for_test(false)?;
        crate::core::persist::atomic_write_owner_only_checked(&self.path, &bytes)?;
        #[cfg(test)]
        self.publication_fault_for_test(true)?;
        self.resource.verify()?;
        self.lock
            .verify_existing_path(&self.path.with_extension("lock"))?;
        state.0 = manifest;
        state.1 = FileFingerprint::for_bytes(&bytes);
        self.publication_poisoned.store(false, Ordering::Release);
        Ok(())
    }
}
