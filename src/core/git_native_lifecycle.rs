//! Metadata-only worker accounting for a live Git-owned task.
//!
//! Source bytes and history remain in Git/native_journal. Serialized records cannot construct an
//! attempt, launch ticket, physical release proof, or permission to resume an abandoned task.
use crate::core::{
    git_ops::OwnedTask,
    native_scope::NativeWriteScope,
    snapshots::GitOid,
    worker_evidence::{
        DriverResult, DurableEffectState, NativeWorkerKind, WorkerJoinFact, WorkerResult,
    },
    workspace_txn::WorkspaceIdentity,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) const MAX_WORKERS: usize = 128;
pub(crate) const MAX_MANIFEST_BYTES: usize = 128 * 1024;
pub(crate) const REGISTRATION_PREFIX: &str = "git-task-v2-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeBinding {
    registration: String,
    repo_id: String,
    worktree_id: String,
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    branch: String,
    owner: String,
    base: GitOid,
    scope_sha256: [u8; 32],
}

impl NativeBinding {
    pub(crate) fn new(
        identity: &WorkspaceIdentity,
        registration: &str,
        branch: &str,
        owner: &str,
        base: &GitOid,
        scope: &NativeWriteScope,
    ) -> Result<Self> {
        for resource in scope.resources() {
            ensure!(
                resource.path().starts_with(&identity.canonical_root),
                "native lifecycle scope escapes owned root"
            );
            resource.verify()?;
        }
        let binding = Self {
            registration: registration.into(),
            repo_id: identity.repo_id.clone(),
            worktree_id: identity.worktree_id.clone(),
            root: identity.canonical_root.clone(),
            git_dir: identity
                .canonical_git_dir
                .clone()
                .context("native lifecycle needs Git directory")?,
            common_dir: identity
                .canonical_common_git_dir
                .clone()
                .context("native lifecycle needs common Git directory")?,
            branch: branch.into(),
            owner: owner.into(),
            base: base.clone(),
            scope_sha256: scope_digest(scope)?,
        };
        binding.validate()?;
        Ok(binding)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.registration.starts_with(REGISTRATION_PREFIX)
                && crate::core::git_ops::valid_operation_id(&self.registration),
            "invalid native lifecycle registration"
        );
        for path in [&self.root, &self.git_dir, &self.common_dir] {
            ensure!(
                path.is_absolute() && path.to_string_lossy().len() <= 4096,
                "invalid native lifecycle workspace path"
            );
            crate::core::workspace_txn::NativeResource::validate_spelling(path)?;
        }
        let identity = WorkspaceIdentity::from_bound_paths(
            self.root.clone(),
            Some(self.git_dir.clone()),
            Some(self.common_dir.clone()),
        );
        ensure!(
            identity.repo_id == self.repo_id && identity.worktree_id == self.worktree_id,
            "native lifecycle workspace keys differ from exact paths"
        );
        ensure!(
            !self.owner.is_empty()
                && self.owner.len() <= 1000
                && self.owner.trim() == self.owner
                && !self.owner.chars().any(char::is_control),
            "invalid native lifecycle owner"
        );
        ensure!(
            self.branch.starts_with("refs/heads/")
                && self.branch.len() <= 256
                && !self.branch.chars().any(char::is_control),
            "invalid native lifecycle branch"
        );
        Ok(())
    }

    pub(crate) fn require_manifest(
        &self,
        manifest: &crate::features::coop::SessionManifest,
    ) -> Result<()> {
        self.validate()?;
        let task = manifest
            .git_task
            .as_ref()
            .context("native lifecycle task missing")?;
        ensure!(
            self.registration == manifest.session_id
                && self.repo_id == manifest.repo_id
                && self.worktree_id == manifest.worktree_id
                && self.root == Path::new(&manifest.root)
                && manifest.branch.as_deref() == Some(self.branch.as_str())
                && self.owner == task.owner
                && self.base == task.base,
            "native lifecycle registration binding changed"
        );
        Ok(())
    }

    pub(crate) fn require_identity(&self, identity: &WorkspaceIdentity) -> Result<()> {
        ensure!(
            self.repo_id == identity.repo_id
                && self.worktree_id == identity.worktree_id
                && self.root == identity.canonical_root
                && Some(&self.git_dir) == identity.canonical_git_dir.as_ref()
                && Some(&self.common_dir) == identity.canonical_common_git_dir.as_ref(),
            "native lifecycle exact workspace binding changed"
        );
        Ok(())
    }

    fn intent(&self, ordinal: u32) -> String {
        format!("{}/native-worker/{ordinal}", self.registration)
    }
}

pub(crate) fn scope_digest(scope: &NativeWriteScope) -> Result<[u8; 32]> {
    crate::core::worker_evidence::scope_sha256(scope.resources().iter().map(|resource| {
        (
            resource.key(),
            resource.path(),
            scope.write_keys().iter().any(|key| key == resource.key()),
        )
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerClosed {
    result: WorkerResult,
    effect: DurableEffectState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRecord {
    ordinal: u32,
    kind: NativeWorkerKind,
    prepared_sha256: [u8; 32],
    // Explicit null distinguishes an admitted intent from a closure; absence is malformed.
    #[serde(deserialize_with = "required_option")]
    closed: Option<WorkerClosed>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum EvidenceState {
    Required {},
    Open {
        binding: NativeBinding,
        #[serde(deserialize_with = "bounded_workers")]
        workers: Vec<WorkerRecord>,
    },
    Released {
        binding: NativeBinding,
        #[serde(deserialize_with = "bounded_workers")]
        workers: Vec<WorkerRecord>,
        driver: DriverResult,
        ownership_released: bool,
    },
    // Only the existing typed inverse controller can select this before its Git runner starts.
    Inverse {
        inverse_sha256: [u8; 32],
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeLifecycle {
    version: u32,
    max_workers: usize,
    max_manifest_bytes: usize,
    evidence: EvidenceState,
}

impl NativeLifecycle {
    pub(crate) fn required() -> Self {
        Self {
            version: 1,
            max_workers: MAX_WORKERS,
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            evidence: EvidenceState::Required {},
        }
    }
    pub(crate) fn require_unstarted(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            matches!(self.evidence, EvidenceState::Required {}),
            "native lifecycle already admitted; no retry or inverse substitution"
        );
        Ok(())
    }
    pub(crate) fn begin(&mut self, binding: NativeBinding) -> Result<()> {
        self.require_unstarted()?;
        binding.validate()?;
        self.evidence = EvidenceState::Open {
            binding,
            workers: Vec::new(),
        };
        Ok(())
    }
    pub(crate) fn start_inverse(
        &mut self,
        inverse: &crate::core::git_ops::PathInverse,
    ) -> Result<()> {
        self.require_unstarted()?;
        inverse.validate()?;
        self.evidence = EvidenceState::Inverse {
            inverse_sha256: inverse_digest(inverse)?,
        };
        Ok(())
    }
    pub(crate) fn binding(&self) -> Option<&NativeBinding> {
        match &self.evidence {
            EvidenceState::Open { binding, .. } | EvidenceState::Released { binding, .. } => {
                Some(binding)
            }
            _ => None,
        }
    }
    pub(crate) fn append_intent(
        &mut self,
        binding: &NativeBinding,
        kind: NativeWorkerKind,
        prepared_sha256: [u8; 32],
    ) -> Result<WorkerJoinFact> {
        self.validate()?;
        let EvidenceState::Open {
            binding: expected,
            workers,
        } = &mut self.evidence
        else {
            anyhow::bail!("native worker admission is not open");
        };
        ensure!(
            expected == binding && workers.len() < MAX_WORKERS,
            "native worker binding changed or inventory exceeds 128; nothing started"
        );
        let ordinal = u32::try_from(workers.len() + 1)?;
        workers.push(WorkerRecord {
            ordinal,
            kind,
            prepared_sha256,
            closed: None,
        });
        Ok(WorkerJoinFact {
            worker: ordinal,
            intent: binding.intent(ordinal),
            result: WorkerResult::NotStarted,
            effect: DurableEffectState::Settled,
        })
    }
    pub(crate) fn reservation_envelope(binding: NativeBinding) -> Self {
        // Worst encoded terminal shapes, not a release proof or a publishable transition.
        let workers = (1..=MAX_WORKERS)
            .map(|ordinal| WorkerRecord {
                ordinal: ordinal as u32,
                kind: NativeWorkerKind::Write,
                prepared_sha256: [255; 32],
                closed: Some(WorkerClosed {
                    result: WorkerResult::NotStarted,
                    effect: DurableEffectState::PossiblyApplied {
                        record: std::num::NonZeroU64::new(u64::MAX),
                    },
                }),
            })
            .collect();
        Self {
            evidence: EvidenceState::Released {
                binding,
                workers,
                driver: DriverResult::Completed,
                ownership_released: true,
            },
            ..Self::required()
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.max_workers == MAX_WORKERS
                && self.max_manifest_bytes == MAX_MANIFEST_BYTES,
            "unsupported native lifecycle version or reservation bounds"
        );
        match &self.evidence {
            EvidenceState::Required {} | EvidenceState::Inverse { .. } => {}
            EvidenceState::Open { binding, workers }
            | EvidenceState::Released {
                binding, workers, ..
            } => {
                binding.validate()?;
                ensure!(
                    workers.len() <= MAX_WORKERS,
                    "native worker inventory exceeds bound"
                );
                let released = matches!(self.evidence, EvidenceState::Released { .. });
                for (index, worker) in workers.iter().enumerate() {
                    ensure!(
                        worker.ordinal as usize == index + 1 && worker.closed.is_some() == released,
                        "native worker inventory or closure is incomplete"
                    );
                }
                if let EvidenceState::Released {
                    ownership_released, ..
                } = &self.evidence
                {
                    ensure!(
                        *ownership_released,
                        "native ownership release is unresolved"
                    );
                }
            }
        }
        Ok(())
    }
    pub(crate) fn validate_manifest(
        &self,
        manifest: &crate::features::coop::SessionManifest,
    ) -> Result<()> {
        self.validate()?;
        let task = manifest
            .git_task
            .as_ref()
            .context("native lifecycle task missing")?;
        if let Some(binding) = self.binding() {
            binding.require_manifest(manifest)?;
        }
        match &self.evidence {
            EvidenceState::Inverse { inverse_sha256 } => {
                let inverse = task
                    .inverse
                    .as_ref()
                    .context("native inverse exemption lacks typed inverse")?;
                ensure!(
                    *inverse_sha256 == inverse_digest(inverse)?,
                    "native inverse exemption changed"
                );
                ensure!(
                    matches!(
                        task.phase.as_str(),
                        "applying_inverse" | "committing_inverse" | "handed_off"
                    ),
                    "invalid inverse lifecycle phase"
                );
            }
            EvidenceState::Required {} | EvidenceState::Open { .. } => {
                ensure!(
                    task.inverse.is_none() && task.phase == "running",
                    "unfinished native lifecycle cannot publish a handoff"
                );
            }
            EvidenceState::Released { .. } => {
                ensure!(
                    task.inverse.is_none()
                        && matches!(task.phase.as_str(), "running" | "committing" | "handed_off"),
                    "invalid released native lifecycle phase"
                );
                if task.phase != "running" {
                    self.require_candidate()?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn require_candidate(&self) -> Result<()> {
        self.validate()?;
        let EvidenceState::Released {
            workers, driver, ..
        } = &self.evidence
        else {
            anyhow::bail!("native task has no live-certified physical release");
        };
        ensure!(*driver == DriverResult::Completed && workers.iter().all(|worker| worker.closed.as_ref().is_some_and(|closed| {
            closed.result == WorkerResult::Returned && closed.effect == DurableEffectState::Settled
        })), "native attempt physically closed without an eligible candidate; retained, no commit or retry");
        Ok(())
    }
    pub(crate) fn require_transition(&self, inverse: bool) -> Result<()> {
        if inverse {
            ensure!(
                matches!(self.evidence, EvidenceState::Inverse { .. }),
                "inverse requires explicit native-lifecycle exemption"
            );
            self.validate()
        } else {
            self.require_candidate()
        }
    }
}

fn inverse_digest(inverse: &crate::core::git_ops::PathInverse) -> Result<[u8; 32]> {
    Ok(crate::core::persist::FileFingerprint::for_bytes(&serde_json::to_vec(inverse)?).sha256)
}

fn required_option<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
fn bounded_workers<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<WorkerRecord>, D::Error> {
    struct Bounded;
    impl<'de> serde::de::Visitor<'de> for Bounded {
        type Value = Vec<WorkerRecord>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 128 native worker records")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut records = Vec::new();
            while let Some(record) = seq.next_element()? {
                if records.len() == MAX_WORKERS {
                    return Err(serde::de::Error::custom(
                        "native worker inventory exceeds 128",
                    ));
                }
                records.push(record);
            }
            Ok(records)
        }
    }
    d.deserialize_seq(Bounded)
}

#[derive(Default)]
struct LiveState {
    claimed: bool,
    admission_closed: bool,
    tickets: usize,
    launched: Vec<String>,
    closed: bool,
    poisoned: bool,
}
struct LiveAttempt {
    owned: Arc<OwnedTask>,
    scope: NativeWriteScope,
    binding: NativeBinding,
    state: Mutex<LiveState>,
}

/// Cloning retains the same live owner. No deserializer or historical-record constructor exists.
#[derive(Clone)]
pub(crate) struct GitNativeAttempt(Arc<LiveAttempt>);
impl std::fmt::Debug for GitNativeAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitNativeAttempt")
            .field("registration", &self.0.binding.registration)
            .finish_non_exhaustive()
    }
}
impl GitNativeAttempt {
    pub(super) fn new(
        owned: Arc<OwnedTask>,
        scope: NativeWriteScope,
        binding: NativeBinding,
    ) -> Self {
        Self(Arc::new(LiveAttempt {
            owned,
            scope,
            binding,
            state: Mutex::new(LiveState::default()),
        }))
    }
    pub(crate) fn same_attempt(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn belongs_to(&self, owner: &Arc<OwnedTask>) -> bool {
        Arc::ptr_eq(&self.0.owned, owner)
    }
    pub(crate) fn root(&self) -> &Path {
        self.0.owned.root()
    }
    pub(crate) fn owner_scope(&self) -> &str {
        self.0.owned.scope()
    }
    pub(crate) fn scope(&self) -> &NativeWriteScope {
        &self.0.scope
    }
    pub(crate) fn check_owned_context(
        &self,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<()> {
        ctx.require_owned_git_task(&self.0.owned)?;
        ensure!(
            ctx.resource_scope() == self.owner_scope()
                && ctx
                    .native_scope()
                    .is_some_and(|scope| scope.same_authority(self.scope())),
            "native lifecycle context scope differs"
        );
        Ok(())
    }
    pub(crate) fn claim_owner(&self) -> Result<()> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("native lifecycle poisoned"))?;
        ensure!(
            !state.claimed && !state.closed && !state.poisoned,
            "native attempt owner already claimed or closed"
        );
        state.claimed = true;
        Ok(())
    }
    pub(crate) fn reserve_launch(&self) -> Result<GitNativeLaunchTicket> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("native lifecycle poisoned"))?;
        ensure!(
            state.claimed && !state.admission_closed && !state.closed && !state.poisoned,
            "native launch admission is closed"
        );
        ensure!(
            state.tickets < MAX_WORKERS,
            "native worker inventory exceeds 128; nothing started"
        );
        state.tickets += 1;
        Ok(GitNativeLaunchTicket {
            attempt: self.clone(),
        })
    }
    /// Close future launch admission only. This is not a physical release or durable closure.
    pub(crate) fn close_admission(&self) -> Result<()> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("native lifecycle poisoned"))?;
        ensure!(
            state.claimed && !state.closed && !state.poisoned,
            "native lifecycle is not open"
        );
        state.admission_closed = true;
        Ok(())
    }
    pub(crate) fn consume_release(
        &self,
        released: crate::agent::git_native_attempt::ReleasedGitAttempt,
    ) -> Result<NativeClosure> {
        let joined = released.into_joined_for(self);
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("native lifecycle poisoned"))?;
        ensure!(
            state.claimed && state.admission_closed && !state.closed && !state.poisoned,
            "native release is single-use and requires closed admission"
        );
        state.closed = true;
        state.poisoned = true;
        let (mut facts, driver) = joined?;
        facts.sort_by_key(|fact| fact.worker);
        ensure!(
            facts.len() == state.tickets
                && facts.len() == state.launched.len()
                && facts
                    .iter()
                    .enumerate()
                    .all(|(index, fact)| fact.worker as usize == index + 1
                        && fact.intent == state.launched[index]),
            "native joined bodies differ from exact launch inventory"
        );
        state.poisoned = false;
        Ok(NativeClosure {
            binding: self.0.binding.clone(),
            facts,
            driver,
        })
    }
    pub(crate) fn poison(&self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.poisoned = true;
        state.closed = true;
    }
}

/// Single-use and non-serializable. Publication precedes actual body spawn.
pub(crate) struct GitNativeLaunchTicket {
    attempt: GitNativeAttempt,
}
impl GitNativeLaunchTicket {
    pub(crate) fn publish(
        self,
        kind: NativeWorkerKind,
        digest: [u8; 32],
    ) -> Result<WorkerJoinFact> {
        let mut state = self
            .attempt
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("native lifecycle poisoned"))?;
        ensure!(
            state.claimed && !state.closed && !state.poisoned,
            "native attempt closed before intent publication"
        );
        match self
            .attempt
            .0
            .owned
            .publish_native_intent(&self.attempt.0.binding, kind, digest)
        {
            Ok(fact) => {
                state.launched.push(fact.intent.clone());
                Ok(fact)
            }
            Err(error) => {
                state.poisoned = true;
                Err(error)
            }
        }
    }
}

/// Only consumption of the runtime's private real-release token can create this publication.
pub(crate) struct NativeClosure {
    binding: NativeBinding,
    facts: Vec<WorkerJoinFact>,
    driver: DriverResult,
}
impl NativeClosure {
    pub(crate) fn apply(self, evidence: &mut NativeLifecycle) -> Result<bool> {
        evidence.validate()?;
        let EvidenceState::Open { binding, workers } = &evidence.evidence else {
            anyhow::bail!("native terminal publication is single-use");
        };
        ensure!(
            binding == &self.binding && workers.len() == self.facts.len(),
            "native terminal inventory binding changed"
        );
        let mut workers = workers.clone();
        for (worker, fact) in workers.iter_mut().zip(self.facts) {
            ensure!(
                worker.ordinal == fact.worker && fact.intent == binding.intent(worker.ordinal),
                "native terminal intent identity changed"
            );
            worker.closed = Some(WorkerClosed {
                result: fact.result,
                effect: fact.effect,
            });
        }
        evidence.evidence = EvidenceState::Released {
            binding: binding.clone(),
            workers,
            driver: self.driver,
            ownership_released: true,
        };
        evidence.validate()?;
        Ok(evidence.require_candidate().is_ok())
    }
}

#[cfg(test)]
#[path = "git_native_lifecycle_tests.rs"]
mod tests;
