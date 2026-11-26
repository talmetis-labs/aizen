//! Exact, host-prepared planning writes. Neither document text nor a pathname is authority.

use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::plan_document;
use super::tools::{
    ApprovalPreview, FilePreparation, PreparedFootprint, PreparedToolCall, WorkspaceEffect,
};
use crate::core::exec_ctx::{self, ExecutionContext};
use crate::core::file_placement::{
    prepare_destination, read_existing_regular, FilePurpose, ResolvedDestination,
};
use crate::core::persist::FileFingerprint;
use crate::core::repo_lock::LockMode;
use crate::core::session_control::{ExecutionPolicy, PlanTarget};
use crate::core::workspace_txn::NativeResource;

#[derive(Debug, Default)]
struct ProofState {
    started: bool,
    active_thread: Option<std::thread::ThreadId>,
    mutations: Vec<PathBuf>,
    failed: bool,
    successful: bool,
}

/// No public constructor, Deserialize, or model-carried waiver. All authority is frozen at prepare.
#[derive(Debug)]
pub(crate) struct PlanWriteProof {
    policy: ExecutionPolicy,
    controller: PlanTarget,
    root: PathBuf,
    target: PathBuf,
    destination: ResolvedDestination,
    expected: FileFingerprint,
    content: String,
    resource: Option<NativeResource>,
    missing_parents: Vec<PathBuf>,
    state: Mutex<ProofState>,
}

struct FrozenPlanWrite {
    proof: Arc<PlanWriteProof>,
    ctx: ExecutionContext,
    human_migration: bool,
}

/// A trusted human selection, not tool JSON. Keep the complete observed legacy bytes across
/// durable target selection, preview and execution; a later prepare may not silently reread them.
pub(crate) struct LegacyMigration {
    target: PlanTarget,
    before: Vec<u8>,
    content: String,
}

impl LegacyMigration {
    pub(crate) fn read(
        workspace: &Path,
        explicit_path: Option<&str>,
        selected: Option<&PlanTarget>,
    ) -> Result<Self> {
        let path = explicit_path
            .or_else(|| selected.map(|target| target.path.as_str()))
            .context("no selected legacy plan; use /plan migrate docs/plans/name.md")?;
        plan_document::validate_relative_path(path)?;
        ensure!(workspace.is_absolute(), "plan workspace must be absolute");
        NativeResource::validate_spelling(workspace)?;
        checked_parents(&workspace.join(path), workspace)?;
        let root = workspace.canonicalize()?;
        ensure!(
            crate::core::config::normalize_path_key(workspace)
                == crate::core::config::normalize_path_key(&root),
            "plan workspace needs its canonical spelling"
        );
        let selected = selected.filter(|target| {
            target.path == path
                && crate::core::config::normalize_path_key(Path::new(&target.root))
                    == crate::core::config::normalize_path_key(&root)
        });
        ensure!(
            explicit_path.is_some() || selected.is_some(),
            "selected plan belongs to another workspace; select a migration path explicitly"
        );
        let id = match selected {
            Some(target) => {
                target.validate()?;
                target.id.clone()
            }
            None => plan_document::new_id()?,
        };
        let before = plan_document::read_bytes(&root, path)?;
        let content = plan_document::migrate_legacy(&before, &id)?;
        crate::core::read_ledger::note(&root.join(path), &FileFingerprint::for_bytes(&before));
        let target = PlanTarget {
            root: root
                .to_str()
                .context("plan workspace must be Unicode")?
                .to_owned(),
            path: path.to_owned(),
            id,
        };
        target.validate()?;
        Ok(Self {
            target,
            before,
            content,
        })
    }

    pub(crate) fn target(&self) -> &PlanTarget {
        &self.target
    }
}

pub(crate) fn is_human_migration(call: &PreparedToolCall) -> bool {
    frozen(call).is_some_and(|write| write.human_migration)
}

pub(crate) fn prepare_migration(
    migration: &LegacyMigration,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    ensure!(
        ctx.native_scope().is_some(),
        "legacy migration requires exact journaled native authority"
    );
    prepare_inner(
        Path::new(&migration.target.root),
        &serde_json::json!({
            "path": migration.target.path,
            "content": migration.content,
            "purpose": "project"
        }),
        ctx,
        Some(migration),
    )
}

fn frozen(call: &PreparedToolCall) -> Option<&FrozenPlanWrite> {
    call.delegation.as_ref()?.downcast_ref::<FrozenPlanWrite>()
}

pub(crate) fn proof(call: &PreparedToolCall) -> Option<&Arc<PlanWriteProof>> {
    frozen(call).map(|write| &write.proof)
}

impl PlanWriteProof {
    fn check_policy(&self, ctx: &ExecutionContext) -> Result<()> {
        self.policy.validate_current()?;
        let current = ctx
            .policy()
            .context("plan write lost its execution policy")?;
        self.policy.check_inheritance(current)?;
        Ok(())
    }

    /// Called by admission and again by the body. Public preview/argument fields cannot replace
    /// the internally frozen bytes, policy, CAS baseline or resource footprint.
    pub(crate) fn check_prepared(
        &self,
        ctx: &ExecutionContext,
        call: &PreparedToolCall,
    ) -> Result<()> {
        self.check_policy(ctx)?;
        let write = frozen(call).context("plan write needs host preparation provenance")?;
        ctx.require_current_binding(write.ctx.workspace_binding())?;
        ctx.require_current_binding(call.workspace_binding.as_ref())?;
        ensure!(
            std::ptr::eq(self, write.proof.as_ref()),
            "plan write provenance mismatch"
        );
        ensure!(
            write.ctx.resource_scope() == ctx.resource_scope(),
            "plan write resource owner changed"
        );
        match (write.ctx.native_scope(), ctx.native_scope()) {
            (None, None) => {}
            (Some(before), Some(now)) if before.same_authority(now) => {}
            _ => anyhow::bail!("plan write native authority changed; prepare again"),
        }
        if let Some(scope) = ctx.native_scope() {
            scope.check_resource(
                self.resource
                    .as_ref()
                    .context("native scope cannot create plan parents")?,
            )?;
        }
        ensure!(
            call.args.as_object().is_some_and(|args| args.len() == 3)
                && call.args.get("path").and_then(Value::as_str) == self.target.to_str()
                && call.args.get("content").and_then(Value::as_str) == Some(self.content.as_str())
                && call.args.get("purpose").and_then(Value::as_str) == Some("project"),
            "prepared plan arguments changed; prepare again"
        );
        match (&self.resource, &call.footprint) {
            (Some(expected), Some(PreparedFootprint::Native(resources))) => {
                ensure!(
                    resources.len() == 1
                        && &resources[0].0 == expected
                        && resources[0].1 == LockMode::Exclusive,
                    "plan resource footprint changed"
                );
            }
            (None, Some(PreparedFootprint::Global)) if !self.missing_parents.is_empty() => {}
            _ => anyhow::bail!("plan write lost its frozen resource footprint"),
        }
        let file = call
            .file
            .as_ref()
            .context("plan write lost destination preparation")?;
        ensure!(
            !file.destination.is_managed()
                && file.destination.actual_path() == self.target
                && file.expected.as_ref() == Some(&self.expected)
                && file.source.is_none()
                && !file.source_managed
                && file.source_expected.is_none()
                && file.source_tree.is_none()
                && file.destination_tree.is_none(),
            "plan file preparation changed"
        );
        Ok(())
    }

    /// Consumed by native effect accounting, only inside this invocation's actual native body.
    /// Fixed parent creations are physical effects too, but do not independently need a Check.
    pub(crate) fn record_mutation(&self, ctx: &ExecutionContext, paths: &[PathBuf]) -> bool {
        let valid_policy = self.check_policy(ctx).is_ok();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_thread != Some(std::thread::current().id()) {
            return false;
        }
        let expected = valid_policy
            && paths.len() == 1
            && (paths[0] == self.target || self.missing_parents.contains(&paths[0]))
            && !state.mutations.contains(&paths[0])
            && !state.failed;
        state.mutations.extend_from_slice(paths);
        if !expected {
            state.failed = true;
        }
        #[cfg(test)]
        if expected && self.missing_parents.contains(&paths[0]) {
            tests::after_parent_mutation();
        }
        expected
    }

    /// Loop verification consumes this only alongside a successful, settled tool outcome.
    /// A validated no-op deliberately has no mutation receipt.
    pub(crate) fn successful_write(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.successful && !state.failed && state.mutations.contains(&self.target)
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }
}

/// Fails conservatively after any physical effect, even on unwind. Never removes created parents.
struct OperationGuard<'a> {
    proof: &'a PlanWriteProof,
    ctx: &'a ExecutionContext,
    settled: bool,
}
impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.proof.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active_thread = None;
        let unresolved = !self.settled && !state.mutations.is_empty();
        if !self.settled {
            state.failed = true;
            state.successful = false;
        }
        drop(state);
        if unresolved {
            self.ctx.note_task_effects_at(Some(&self.proof.root));
        }
    }
}

/// Canonical native resource spelling is checked before canonicalization can hide links.
fn checked_parents(target: &Path, root: &Path) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    for parent in target
        .parent()
        .context("plan target has no parent")?
        .ancestors()
    {
        match std::fs::symlink_metadata(parent) {
            Ok(md) => {
                ensure!(
                    md.is_dir() && !crate::core::file_placement::is_link_or_reparse(&md),
                    "plan ancestry must contain only non-link directories"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(parent == root.join("docs") || parent == root.join("docs/plans"),
                    "only the fixed docs and docs/plans parents may be created; nested plan directories must exist");
                missing.push(parent.to_path_buf());
            }
            Err(error) => return Err(error).context("checking plan ancestry"),
        }
    }
    missing.reverse();
    Ok(missing)
}

fn read_before(target: &Path) -> Result<(Option<Vec<u8>>, FileFingerprint)> {
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            let bytes = read_existing_regular(target, plan_document::MAX_DOCUMENT_BYTES as u64)?;
            let expected = FileFingerprint::for_bytes(&bytes);
            Ok((Some(bytes), expected))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((None, FileFingerprint::missing()))
        }
        Err(error) => Err(error).context("checking plan target"),
    }
}

pub(crate) fn prepare(
    root: &Path,
    args: &Value,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    prepare_inner(root, args, ctx, None)
}

fn prepare_inner(
    root: &Path,
    args: &Value,
    ctx: &ExecutionContext,
    migration: Option<&LegacyMigration>,
) -> Result<PreparedToolCall> {
    let policy = ctx
        .policy()
        .context("plan write requires an execution policy")?;
    policy.validate_current()?;
    ensure!(
        policy.is_planning(),
        "plan write needs live planning authority"
    );
    if let Some(ambient) = exec_ctx::current() {
        policy.check_inheritance(
            ambient
                .policy()
                .context("ambient execution policy missing")?,
        )?;
    }
    let controller = policy
        .state()
        .plan_target
        .as_ref()
        .context("planning mode has no controller-selected plan target")?
        .clone();
    controller.validate()?;
    let fields = args
        .as_object()
        .context("file_write arguments must be an object")?;
    ensure!(
        fields
            .keys()
            .all(|key| matches!(key.as_str(), "path" | "content" | "purpose")),
        "unexpected planning file_write argument"
    );
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .context("file_write needs path")?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("file_write needs content")?;
    ensure!(
        matches!(args.get("purpose"), None | Some(Value::Null))
            || args.get("purpose").and_then(Value::as_str) == Some("project"),
        "plan write requires project placement, never artifact or scratch"
    );
    let parsed = plan_document::parse(content.as_bytes())?;
    ensure!(
        parsed.id == controller.id,
        "plan document ID differs from controller-selected ID"
    );

    let selected_root = Path::new(&controller.root);
    NativeResource::validate_spelling(selected_root)?;
    NativeResource::validate_spelling(root)?;
    NativeResource::validate_spelling(Path::new(path))?;
    // Inspect supplied roots before following any alias.
    checked_parents(&selected_root.join(&controller.path), selected_root)?;
    checked_parents(&root.join(&controller.path), root)?;
    let root = root.canonicalize()?;
    ensure!(
        selected_root.canonicalize()? == root,
        "file tool workspace differs from plan target"
    );
    let target = root.join(&controller.path);
    let requested = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        plan_document::validate_relative_path(path)?;
        root.join(path)
    };
    ensure!(
        crate::core::config::normalize_path_key(&requested)
            == crate::core::config::normalize_path_key(&target),
        "file_write may write only the exact controller-selected plan"
    );
    let missing_parents = checked_parents(&target, &root)?;
    let resource = if missing_parents.is_empty() {
        Some(NativeResource::resolve(&target)?)
    } else {
        None
    };
    if let Some(scope) = ctx.native_scope() {
        scope.check_resource(
            resource
                .as_ref()
                .context("native scope cannot create plan parents")?,
        )?;
    }
    crate::core::native_journal::validate_mutation_target(&target)?;
    let destination = prepare_destination(
        None,
        target.to_str().context("plan path must be Unicode")?,
        Some(FilePurpose::Project),
    )?;
    let (before, expected) = read_before(&target)?;
    if let Some(migration) = migration {
        ensure!(
            controller == migration.target
                && before.as_deref() == Some(migration.before.as_slice())
                && content == migration.content,
            "legacy plan changed after selection; nothing written; select and approve again"
        );
        ensure!(
            plan_document::migrate_legacy(&migration.before, &controller.id)? == content,
            "legacy migration must preserve the exact observed document"
        );
    } else if let Some(before) = &before {
        ensure!(
            plan_document::parse(before)?.id == controller.id,
            "existing plan identity differs; explicit controller reattachment is required"
        );
    }
    destination.verify()?;
    if let Some(resource) = &resource {
        resource.verify()?;
    }
    let no_op = before.as_deref() == Some(content.as_bytes());
    let mut call = PreparedToolCall::new(serde_json::json!({
        "path": target.to_string_lossy(), "content": content, "purpose": "project"
    }));
    call.planning_policy = Some(policy.clone());
    call.workspace_binding = ctx.workspace_binding().cloned();
    call.preview = Some(ApprovalPreview {
        title: target.to_string_lossy().into_owned(),
        diff: before
            .as_ref()
            .filter(|_| !no_op)
            .map(|before| {
                crate::agent::builtin::plan_write_diff(
                    std::str::from_utf8(before).expect("validated UTF-8"),
                    content,
                )
            })
            .transpose()?,
        lines: vec![format!(
            "{} exact plan {} ({} bytes); implementation permission is unchanged",
            if no_op {
                "unchanged"
            } else if expected.exists {
                "overwrite"
            } else {
                "create"
            },
            controller.path,
            content.len()
        )],
    });
    call.preview_lines = missing_parents
        .iter()
        .map(|parent| format!("create fixed plan parent {}", parent.display()))
        .collect();
    call.footprint = Some(match &resource {
        Some(resource) => PreparedFootprint::Native(vec![(resource.clone(), LockMode::Exclusive)]),
        None => PreparedFootprint::Global,
    });
    call.file = Some(FilePreparation {
        destination: destination.clone(),
        source: None,
        source_managed: false,
        expected: Some(expected.clone()),
        source_expected: None,
        source_tree: None,
        destination_tree: None,
    });
    call.delegation = Some(Arc::new(FrozenPlanWrite {
        proof: Arc::new(PlanWriteProof {
            policy: policy.clone(),
            controller,
            root,
            target,
            destination,
            expected,
            content: content.to_owned(),
            resource,
            missing_parents,
            state: Mutex::new(ProofState::default()),
        }),
        ctx: ctx.clone(),
        human_migration: migration.is_some(),
    }));
    Ok(call)
}

pub(crate) fn effect(call: &PreparedToolCall) -> Option<WorkspaceEffect> {
    let proof = proof(call)?;
    Some(
        if proof.expected == FileFingerprint::for_bytes(proof.content.as_bytes()) {
            WorkspaceEffect::None
        } else {
            WorkspaceEffect::Paths
        },
    )
}

pub(crate) fn execute(call: &PreparedToolCall) -> Option<Result<String>> {
    let write = frozen(call)?;
    Some((|| {
        let ambient =
            exec_ctx::current().context("prepared plan write requires its execution context")?;
        let proof = &write.proof;
        proof.check_prepared(&ambient, call)?;
        // Retain the policy lease across direct native execution as well as executor dispatch.
        let ctx = ambient.admit_policy()?.with_plan_write_proof(proof.clone());
        {
            let mut state = proof.state.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                !state.started,
                "plan preparation was already consumed; prepare and approve again"
            );
            state.started = true;
            state.active_thread = Some(std::thread::current().id());
        }
        let mut guard = OperationGuard {
            proof,
            ctx: &ctx,
            settled: false,
        };
        let result = exec_ctx::with_current(ctx.clone(), || -> Result<String> {
            proof.check_prepared(&ctx, call)?;
            ensure!(
                checked_parents(&proof.target, &proof.root)? == proof.missing_parents,
                "plan parents changed after preparation; prepare again"
            );
            proof.destination.verify()?;
            if let Some(resource) = &proof.resource {
                resource.verify()?;
            }
            let (_, expected) = read_before(&proof.target)?;
            ensure!(
                expected == proof.expected,
                "plan changed after preparation; nothing written"
            );
            let no_op = expected == FileFingerprint::for_bytes(proof.content.as_bytes());
            if !no_op {
                if let Some(stale) =
                    crate::core::read_ledger::overwrite_conflict(&proof.target, &expected)
                {
                    anyhow::bail!(crate::core::read_ledger::conflict_message(
                        &proof.target.to_string_lossy(),
                        &stale
                    ));
                }
                // This existing helper checks each frozen ancestor before creating precisely the
                // missing fixed parents admitted above. Native callbacks account for partial creation.
                proof.destination.ensure_project_parents()?;
                proof.destination.verify()?;
                ensure!(
                    checked_parents(&proof.target, &proof.root)?.is_empty(),
                    "plan parents missing after creation"
                );
                let resource = NativeResource::resolve(&proof.target)?;
                crate::core::native_journal::validate_mutation_target(&proof.target)?;
                let written = if let Some(scope) = ctx.native_scope() {
                    scope.check_resource(&resource)?;
                    crate::core::native_journal::apply(
                        &resource,
                        &expected,
                        proof.content.as_bytes(),
                        &ctx.resource_scope(),
                    )?
                } else {
                    crate::core::persist::compare_and_atomic_write_observed(
                        &proof.target,
                        &expected,
                        proof.content.as_bytes(),
                        || {
                            ctx.note_native_mutation(std::slice::from_ref(&proof.target));
                        },
                    )?
                };
                resource.verify()?;
                ensure!(
                    written == FileFingerprint::for_bytes(proof.content.as_bytes()),
                    "unexpected plan write fingerprint"
                );
            }
            // Re-resolve each operation: editor atomic saves are allowed after a fresh prepare.
            #[cfg(test)]
            tests::before_readback(&proof.target, proof.content.as_bytes());
            let attachment =
                super::todo::PlanAttachment::load(&proof.root, &proof.controller.path)?;
            ensure!(
                attachment.fingerprint == plan_document::fingerprint(proof.content.as_bytes()),
                "plan readback differs from approved bytes"
            );
            ensure!(
                attachment.id == proof.controller.id,
                "plan readback identity changed"
            );
            proof.check_policy(&ctx)?;
            let written = FileFingerprint::for_bytes(proof.content.as_bytes());
            if let Some(task) = ctx.task() {
                task.attach_plan(attachment)?;
            }
            // The observation is captured after mutation/attachment epochs. It cannot satisfy Check.
            if let Some(observation) = ctx.capture_task_observation(&proof.target, &proof.root) {
                observation.record(&written);
            }
            if !no_op {
                crate::core::read_ledger::note(&proof.target, &written);
            }
            {
                let mut state = proof.state.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(!state.failed, "plan write had unexpected physical effects");
                if !no_op {
                    ensure!(
                        state.mutations.contains(&proof.target),
                        "plan write has no mutation accounting receipt"
                    );
                    state.successful = true;
                }
            }
            let outstanding = ctx
                .task()
                .is_some_and(|task| !task.verification_roots().is_empty());
            if no_op {
                Ok(format!(
                    "{}: {} already holds this exact plan; validated readback",
                    super::builtin::NOOP_WRITE_PREFIX,
                    proof.target.display()
                ))
            } else {
                Ok(format!(
                    "Plan saved: {} ({} bytes); {}",
                    proof.target.display(),
                    proof.content.len(),
                    if outstanding {
                        "implementation checks remain outstanding"
                    } else {
                        "planning mode remains active; implementation is not authorized"
                    }
                ))
            }
        });
        guard.settled = result.is_ok();
        result
    })())
}

#[cfg(test)]
#[path = "planning_write_tests.rs"]
mod tests;
