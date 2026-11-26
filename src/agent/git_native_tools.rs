//! Typed launch evidence for ordinary native operations in an explicitly owned Git task.
//! No source snapshot, overlay, executable context carrier, or deserialized authority lives here.

use super::*;
use crate::core::exec_ctx::WorkspaceBinding;
use crate::core::git_ops::GitNativeAttempt;
use crate::core::worker_evidence::{scope_sha256, NativeWorkerKind};
use ring::digest::{Context as DigestContext, SHA256};

pub(crate) struct PreparedGitNativeWorker {
    attempt: GitNativeAttempt,
    binding: WorkspaceBinding,
    kind: NativeWorkerKind,
    digest: [u8; 32],
}

impl PreparedGitNativeWorker {
    pub(crate) fn check_lifecycle(
        &self,
        attempt: &GitNativeAttempt,
        binding: &WorkspaceBinding,
    ) -> Result<()> {
        anyhow::ensure!(
            self.attempt.same_attempt(attempt) && &self.binding == binding,
            "prepared native worker belongs to another Git attempt or workspace generation"
        );
        Ok(())
    }

    pub(crate) fn kind(&self) -> NativeWorkerKind {
        self.kind
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

struct OperationDigest(DigestContext);

impl OperationDigest {
    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.0.update(bytes);
    }
    fn u64(&mut self, value: u64) {
        self.0.update(&value.to_le_bytes());
    }
    fn flag(&mut self, value: bool) {
        self.0.update(&[u8::from(value)]);
    }
    fn optional_u64(&mut self, value: Option<u64>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.u64(value);
        }
    }
    fn path(&mut self, path: &Path) -> Result<()> {
        self.bytes(
            path.to_str()
                .context("native worker path is not UTF-8")?
                .as_bytes(),
        );
        Ok(())
    }
    fn fingerprint(&mut self, fingerprint: &crate::core::persist::FileFingerprint) {
        self.flag(fingerprint.exists);
        self.u64(fingerprint.byte_len);
        self.bytes(&fingerprint.sha256);
    }
    fn destination(&mut self, destination: &ResolvedDestination) -> Result<()> {
        self.path(destination.actual_path())?;
        self.flag(destination.is_managed());
        self.flag(destination.exists_at_preparation());
        self.flag(destination.fingerprint().is_some());
        if let Some(fingerprint) = destination.fingerprint() {
            self.fingerprint(fingerprint);
        }
        Ok(())
    }
}

fn begin(
    prepared: &PreparedToolCall,
    ctx: Option<&ExecutionContext>,
    kind: NativeWorkerKind,
    resources: &[(&NativeResource, LockMode)],
) -> Result<Option<(PreparedGitNativeWorker, OperationDigest)>> {
    let Some(ctx) = ctx.filter(|ctx| ctx.has_owned_git_task()) else {
        return Ok(None);
    };
    let (attempt, binding) = ctx
        .tool_workers()
        .and_then(|producer| producer.git_binding())
        .context("owned Git operation has no bound native lifecycle; no direct fallback")?;
    attempt.check_owned_context(ctx)?;
    ctx.require_current_binding(Some(binding))?;
    let Some(PreparedFootprint::Native(footprint)) = &prepared.footprint else {
        bail!("Git-native operation requires its exact live native footprint");
    };
    anyhow::ensure!(
        !resources.is_empty() && resources.len() == footprint.len(),
        "Git-native footprint differs from the frozen operation"
    );
    for ((expected, mode), (actual, actual_mode)) in resources.iter().zip(footprint) {
        anyhow::ensure!(
            expected.key() == actual.key()
                && expected.path() == actual.path()
                && mode == actual_mode,
            "Git-native resource/access differs from the frozen operation"
        );
        if *mode == LockMode::Exclusive {
            attempt.scope().check_resource(expected)?;
        } else {
            attempt.scope().check_read(expected)?;
        }
    }
    let scope = attempt.scope();
    let scope_hash = scope_sha256(scope.resources().iter().map(|resource| {
        (
            resource.key(),
            resource.path(),
            scope.write_keys().iter().any(|key| key == resource.key()),
        )
    }))?;
    let mut digest = OperationDigest(DigestContext::new(&SHA256));
    digest.bytes(b"aizen-git-owned-prepared-native-v1");
    digest.bytes(match kind {
        NativeWorkerKind::Read => b"read",
        NativeWorkerKind::Edit => b"edit",
        NativeWorkerKind::Write => b"write",
    });
    digest.bytes(attempt.owner_scope().as_bytes());
    digest.path(attempt.root())?;
    digest.bytes(&scope_hash);
    for (resource, mode) in resources {
        digest.bytes(resource.key().as_bytes());
        digest.path(resource.path())?;
        digest.flag(*mode == LockMode::Exclusive);
    }
    Ok(Some((
        PreparedGitNativeWorker {
            attempt: attempt.clone(),
            binding: binding.clone(),
            kind,
            digest: [0; 32],
        },
        digest,
    )))
}

/// Encode host-frozen state, not mutable tool arguments. The descriptor retains no context carrier.
pub(crate) fn prepared_git_native_worker(
    prepared: &PreparedToolCall,
) -> Result<Option<PreparedGitNativeWorker>> {
    let Some(frozen) = prepared.delegation.as_ref() else {
        return Ok(None);
    };
    let (mut worker, digest) = if let Some(read) = frozen.downcast_ref::<FrozenFileRead>() {
        // Ordinary unowned calls keep their existing behavior, including unresolved/global reads.
        if !read
            .ctx
            .as_ref()
            .is_some_and(ExecutionContext::has_owned_git_task)
        {
            return Ok(None);
        }
        let resources = read
            .entries
            .iter()
            .map(|entry| {
                Ok((
                    entry
                        .resource
                        .as_ref()
                        .context("Git-native read lost its frozen resource")?,
                    LockMode::Shared,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let Some((worker, mut digest)) = begin(
            prepared,
            read.ctx.as_ref(),
            NativeWorkerKind::Read,
            &resources,
        )?
        else {
            return Ok(None);
        };
        digest.flag(read.number);
        digest.flag(read.batch);
        digest.u64(read.omitted as u64);
        digest.u64(read.entries.len() as u64);
        for entry in &read.entries {
            digest.bytes(entry.label.as_bytes());
            digest.optional_u64(entry.start);
            digest.optional_u64(entry.end);
            digest.path(&entry.alias)?;
            digest.path(
                entry
                    .target
                    .as_deref()
                    .context("Git-native read lost its target")?,
            )?;
            digest.flag(entry.identity.is_some());
            if let Some(identity) = entry.identity {
                digest.u64(identity.0);
                digest.u64(identity.1);
            }
            anyhow::ensure!(
                entry.error.is_none(),
                "Git-native read has unresolved preparation"
            );
        }
        (worker, digest)
    } else if let Some(edit) = frozen.downcast_ref::<FrozenFileEdit>() {
        if !edit
            .ctx
            .as_ref()
            .is_some_and(ExecutionContext::has_owned_git_task)
        {
            return Ok(None);
        }
        let resource = edit
            .resource
            .as_ref()
            .context("Git-native edit lost its frozen resource")?;
        let Some((worker, mut digest)) = begin(
            prepared,
            edit.ctx.as_ref(),
            NativeWorkerKind::Edit,
            &[(resource, LockMode::Exclusive)],
        )?
        else {
            return Ok(None);
        };
        digest.destination(&edit.alias)?;
        digest.destination(&edit.destination)?;
        digest.path(&edit.target)?;
        digest.fingerprint(&edit.expected);
        digest.bytes(edit.content.as_bytes());
        digest.bytes(edit.output.as_bytes());
        digest.flag(edit.dry_run);
        digest.flag(edit.no_op);
        digest.flag(edit.managed);
        (worker, digest)
    } else if let Some(write) = frozen.downcast_ref::<FrozenFileWrite>() {
        if !write
            .ctx
            .as_ref()
            .is_some_and(ExecutionContext::has_owned_git_task)
        {
            return Ok(None);
        }
        let resource = write
            .resource
            .as_ref()
            .context("Git-native write lost its frozen resource")?;
        let Some((worker, mut digest)) = begin(
            prepared,
            write.ctx.as_ref(),
            NativeWorkerKind::Write,
            &[(resource, LockMode::Exclusive)],
        )?
        else {
            return Ok(None);
        };
        digest.destination(&write.destination)?;
        digest.path(&write.target)?;
        digest.fingerprint(&write.expected);
        digest.bytes(write.content.as_bytes());
        digest.bytes(write.output.as_bytes());
        digest.flag(write.no_op);
        digest.flag(write.managed);
        (worker, digest)
    } else {
        return Ok(None);
    };
    worker.digest.copy_from_slice(digest.0.finish().as_ref());
    Ok(Some(worker))
}
