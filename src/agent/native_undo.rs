//! Explicit parent-selected native owner undo. No group/tree restore and no automatic rollback.
//! Preparation freezes exact resources and parent authority; executor owns exclusive resource leases
//! before execution. A failure may leave earlier reversals settled and is never reported as success.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::tools::{
    ApprovalPreview, PreparedFootprint, PreparedToolCall, Tool, ToolObservation, WorkspaceEffect,
};
use crate::core::exec_ctx::{self, ExecutionContext};
use crate::core::native_journal::{self, PossiblyApplied};
use crate::core::repo_lock::LockMode;
use crate::core::workspace_txn::NativeResource;

const MAX_PATHS: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const MAX_OWNER_BYTES: usize = 1024;
const MAX_ERROR_CHARS: usize = 512;

/// Register only in the coordinating parent's toolset, never any delegated/scoped child allowlist.
pub(crate) struct NativeOwnerUndo {
    root: PathBuf,
}

impl NativeOwnerUndo {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

struct FrozenUndo {
    owner: String,
    parent: ExecutionContext,
    resources: Vec<NativeResource>,
    args: Value,
}

/// The report includes settled earlier files and explicitly unknown progress in the failed file.
/// Neither journal/image contents nor source bytes are included in this error.
#[derive(Debug)]
pub(crate) struct NativeUndoFailure {
    pub(crate) report: String,
    pub(crate) possibly_applied_record: Option<u64>,
}

impl std::fmt::Display for NativeUndoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.report)
    }
}
impl std::error::Error for NativeUndoFailure {}

impl Tool for NativeOwnerUndo {
    fn name(&self) -> &str {
        "native_owner_undo"
    }

    fn description(&self) -> &str {
        "Undo one exact owner's native journal on 1-32 selected files. Parent/controller only; \
         destructive approval required. Refuses newer sibling postimages/unresolved attribution. \
         No group/tree restore or automatic rollback; earlier reversals survive later conflicts. \
         Windows created-file deletion unsupported."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string", "minLength": 1, "maxLength": MAX_OWNER_BYTES,
                    "description": "Exact resource_scope in this parent namespace; no prefix/glob."
                },
                "paths": {
                    "type": "array", "minItems": 1, "maxItems": MAX_PATHS,
                    "items": {"type": "string", "minLength": 1, "maxLength": MAX_PATH_BYTES},
                    "description": "Exact files, absolute/workspace-relative; no directories/globs."
                }
            },
            "required": ["owner", "paths"],
            "additionalProperties": false
        })
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn recovery_effect(&self, _args: &Value) -> bool {
        true
    }

    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::Paths
    }

    fn workspace_target(&self, _args: &Value) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn plan_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        validate_parent(ctx, false)?;
        validate_args(args, &ctx.resource_scope())?;
        Ok(PreparedToolCall::new(args.clone()))
    }

    fn prepare_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        validate_parent(ctx, false)?;
        let (owner, paths) = validate_args(args, &ctx.resource_scope())?;
        anyhow::ensure!(
            self.root.as_os_str().len() <= MAX_PATH_BYTES,
            "native owner undo workspace path exceeds bound"
        );
        let root = std::fs::canonicalize(&self.root)
            .context("native owner undo requires an existing workspace root")?;
        let mut resources = Vec::with_capacity(paths.len());
        let mut keys = HashSet::with_capacity(paths.len());
        let mut canonical_paths = Vec::with_capacity(paths.len());
        for path in paths {
            let path = Path::new(path);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let resource = NativeResource::resolve(&absolute)?;
            resource.verify()?;
            anyhow::ensure!(
                keys.insert(resource.key().to_owned()),
                "native owner undo duplicate or aliased file resource"
            );
            let path = resource
                .path()
                .to_str()
                .context("native undo path is not Unicode")?;
            anyhow::ensure!(
                path.len() <= MAX_PATH_BYTES,
                "native undo resolved path exceeds bound"
            );
            canonical_paths.push(Value::String(path.to_owned()));
            resources.push(resource);
        }
        let frozen_args = json!({"owner": owner, "paths": canonical_paths});
        let mut prepared = PreparedToolCall::new(frozen_args.clone());
        prepared.footprint = Some(PreparedFootprint::Native(
            resources
                .iter()
                .map(|r| (r.clone(), LockMode::Exclusive))
                .collect(),
        ));
        prepared.preview = Some(ApprovalPreview {
            title: "Selected native owner undo".into(),
            diff: None,
            lines: vec![
                format!("Owner: {owner}; {} exact file(s), newest selected-owner mutations first", resources.len()),
                "No group restore or automatic rollback. Earlier settled reversals survive a later conflict.".into(),
                "Newer sibling content/ownership conflicts; Windows created-file deletion is unsupported.".into(),
            ],
        });
        prepared.preview_lines = resources
            .iter()
            .enumerate()
            .map(|(i, r)| format!("{}: {}", i + 1, r.path().display()))
            .collect();
        prepared.delegation = Some(Arc::new(FrozenUndo {
            owner: owner.to_owned(),
            parent: ctx.clone(),
            resources,
            args: frozen_args,
        }));
        Ok(prepared)
    }

    fn execute(&self, _args: &Value) -> Result<String> {
        // Direct execution cannot prove executor-owned leases or reproduce an approved frozen call.
        anyhow::bail!("native owner undo requires parent preparation, destructive approval and executor resource leases")
    }

    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenUndo>())
            .context("native owner undo requires internally frozen parent preparation")?;
        validate_parent(&frozen.parent, true)?;
        anyhow::ensure!(
            prepared.args == frozen.args,
            "native owner undo arguments changed; prepare and approve again"
        );
        let footprint = match &prepared.footprint {
            Some(PreparedFootprint::Native(resources)) => resources,
            _ => {
                anyhow::bail!("native owner undo requires its frozen exclusive resource footprint")
            }
        };
        anyhow::ensure!(
            footprint.len() == frozen.resources.len()
                && footprint
                    .iter()
                    .zip(&frozen.resources)
                    .all(|((resource, mode), expected)| {
                        *mode == LockMode::Exclusive && resource == expected
                    }),
            "native owner undo resource footprint changed; prepare and approve again"
        );
        // Revalidate every target BEFORE any reversal; a preparation drift never partially applies.
        for resource in &frozen.resources {
            resource.verify()?;
        }
        let mut rows = Vec::with_capacity(frozen.resources.len());
        let mut settled_total = 0usize;
        for (index, resource) in frozen.resources.iter().enumerate() {
            match native_journal::undo_owner(&frozen.parent, resource, &frozen.owner) {
                Ok(count) => {
                    settled_total = settled_total.saturating_add(count);
                    rows.push(json!({"file": index + 1, "path": resource.path(),
                        "status": "settled", "settled_reversals": count}));
                }
                Err(error) => {
                    let possibly_applied =
                        error.downcast_ref::<PossiblyApplied>().map(|e| e.record_id);
                    let message: String = error
                        .to_string()
                        .chars()
                        .take(MAX_ERROR_CHARS)
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect();
                    rows.push(json!({"file": index + 1, "path": resource.path(),
                        "status": "failed", "settled_reversals": Value::Null,
                        "possibly_applied_record": possibly_applied, "error": message,
                        "progress": "Earlier reversals in this file may already be settled; count is unknown. Reconcile under leases before retrying."}));
                    for (later, resource) in frozen.resources.iter().enumerate().skip(index + 1) {
                        rows.push(json!({"file": later + 1, "path": resource.path(), "status": "not_attempted"}));
                    }
                    let report = format!(
                        "native owner undo partial failure: {}",
                        json!({"status": "partial_failure", "owner": frozen.owner,
                        "settled_reversals_in_completed_files": settled_total, "files": rows,
                        "rollback": "none; earlier actual reversals are retained"})
                    );
                    return Err(NativeUndoFailure {
                        report,
                        possibly_applied_record: possibly_applied,
                    }
                    .into());
                }
            }
        }
        Ok(ToolObservation::text(
            json!({"status": "settled", "owner": frozen.owner,
            "settled_reversals": settled_total, "files": rows, "rollback": "none"})
            .to_string(),
        ))
    }
}

fn validate_parent(ctx: &ExecutionContext, require_ambient: bool) -> Result<()> {
    anyhow::ensure!(
        ctx.can_restore_workspace() && ctx.native_scope().is_none(),
        "delegated or scoped execution cannot undo native owners"
    );
    let current = exec_ctx::current();
    anyhow::ensure!(
        !require_ambient || current.is_some(),
        "native owner undo requires pinned parent execution"
    );
    if let Some(current) = current {
        anyhow::ensure!(
            current.can_restore_workspace()
                && current.native_scope().is_none()
                && current.resource_scope() == ctx.resource_scope()
                && current.conversation() == ctx.conversation(),
            "ambient execution cannot borrow another parent's native undo authority"
        );
    }
    let scope = ctx.resource_scope();
    anyhow::ensure!(
        !scope.is_empty() && scope.len() <= MAX_OWNER_BYTES && !scope.chars().any(char::is_control),
        "native owner undo parent scope is invalid"
    );
    Ok(())
}

fn validate_args<'a>(args: &'a Value, parent: &str) -> Result<(&'a str, Vec<&'a str>)> {
    let object = args
        .as_object()
        .context("native owner undo arguments must be an object")?;
    anyhow::ensure!(
        object.len() == 2 && object.contains_key("owner") && object.contains_key("paths"),
        "native owner undo accepts only owner and exact paths"
    );
    let owner = object["owner"]
        .as_str()
        .context("native owner undo requires an exact owner string")?;
    anyhow::ensure!(
        !owner.is_empty()
            && owner.len() <= MAX_OWNER_BYTES
            && !owner
                .chars()
                .any(|c| c.is_control() || matches!(c, '*' | '?' | '[' | ']' | '{' | '}')),
        "native owner undo owner must be a bounded exact scope, not a pattern"
    );
    anyhow::ensure!(
        owner == parent
            || owner
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1),
        "native owner undo selected owner is outside this parent namespace"
    );
    let paths = object["paths"]
        .as_array()
        .context("native owner undo requires an exact file paths array")?;
    anyhow::ensure!(
        !paths.is_empty() && paths.len() <= MAX_PATHS,
        "native owner undo requires 1-32 exact files"
    );
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path
            .as_str()
            .context("native owner undo file paths must be strings")?;
        anyhow::ensure!(
            !path.is_empty() && path.len() <= MAX_PATH_BYTES && !path.chars().any(char::is_control),
            "native owner undo file path exceeds bound or contains control characters"
        );
        // Validate the original input, not root.join(path): a verbatim Windows workspace root
        // normalizes traversal during the join and would hide an unsafe caller spelling.
        NativeResource::validate_spelling(Path::new(path))?;
        result.push(path);
    }
    Ok((owner, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aizen-owner-undo-tool-{}-{}",
                std::process::id(),
                crate::core::persist::unique_sequence()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn parent() -> ExecutionContext {
        ExecutionContext::default().with_resource_scope("parent")
    }
    fn args() -> Value {
        json!({"owner": "parent/task/a", "paths": ["one.txt"]})
    }

    #[test]
    fn parent_preparation_freezes_owner_paths_context_and_exclusive_footprint() {
        let dir = TestDir::new();
        let tool = NativeOwnerUndo::new(dir.0.clone());
        let mut input = args();
        let prepared = tool.prepare_call(&input, &parent()).unwrap();
        input["owner"] = json!("parent/task/b");
        input["paths"] = json!(["two.txt"]);
        let frozen = prepared
            .delegation
            .as_ref()
            .unwrap()
            .downcast_ref::<FrozenUndo>()
            .unwrap();
        assert_eq!(frozen.owner, "parent/task/a");
        assert_eq!(frozen.parent.resource_scope(), "parent");
        assert_eq!(frozen.resources[0].path(), dir.0.join("one.txt"));
        match &prepared.footprint {
            Some(PreparedFootprint::Native(resources)) => {
                assert_eq!(resources.len(), 1);
                assert_eq!(resources[0].0, frozen.resources[0]);
                assert_eq!(resources[0].1, LockMode::Exclusive);
            }
            _ => panic!("exact exclusive footprint required"),
        }
        assert!(tool.is_destructive());
        assert!(tool.recovery_effect(&prepared.args));
        assert!(!tool.is_concurrency_safe());
        assert_eq!(
            tool.workspace_effect_prepared(&prepared),
            WorkspaceEffect::Paths
        );
        assert!(!dir.0.join("one.txt").exists());
    }

    #[test]
    fn child_cannot_prepare_or_execute_or_borrow_parent_ambient_authority() {
        let dir = TestDir::new();
        let tool = NativeOwnerUndo::new(dir.0.clone());
        let parent = parent();
        let child = parent
            .without_workspace_restore()
            .with_resource_scope("parent/task/a");
        assert!(tool.prepare_call(&args(), &child).is_err());
        assert!(tool.plan_call(&args(), &child).is_err());
        let prepared = tool.prepare_call(&args(), &parent).unwrap();
        exec_ctx::with_current(child.clone(), || {
            assert!(tool.prepare_call(&args(), &parent).is_err());
            assert!(tool.execute_prepared(&prepared).is_err());
            assert!(tool.execute(&args()).is_err());
        });
        exec_ctx::with_current(parent.with_resource_scope("unrelated"), || {
            assert!(tool.execute_prepared(&prepared).is_err());
        });
        assert!(tool.execute_prepared(&prepared).is_err());
    }

    #[test]
    fn changed_frozen_args_or_footprint_refuse_before_storage_effects() {
        let dir = TestDir::new();
        let tool = NativeOwnerUndo::new(dir.0.clone());
        let parent = parent();
        let mut prepared = tool.prepare_call(&args(), &parent).unwrap();
        exec_ctx::with_current(parent.clone(), || {
            prepared.args["owner"] = json!("parent/task/b");
            assert!(tool.execute_prepared(&prepared).is_err());
        });
        let mut prepared = tool.prepare_call(&args(), &parent).unwrap();
        prepared.footprint = Some(PreparedFootprint::Global);
        exec_ctx::with_current(parent.clone(), || {
            assert!(tool.execute_prepared(&prepared).is_err())
        });
        let mut prepared = tool.prepare_call(&args(), &parent).unwrap();
        if let Some(PreparedFootprint::Native(resources)) = &mut prepared.footprint {
            resources[0].1 = LockMode::Shared;
        }
        exec_ctx::with_current(parent, || {
            assert!(tool.execute_prepared(&prepared).is_err())
        });
        assert!(!dir.0.join("one.txt").exists());
    }

    #[test]
    fn later_file_conflict_reports_earlier_settled_undo_without_rollback() {
        use crate::core::persist::FileFingerprint;
        use crate::core::workspace_txn::NativeResourceLease;
        use std::time::Duration;

        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = TestDir::new();
        let workspace = dir.0.join("work");
        let home = dir.0.join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&home).unwrap();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let a = NativeResource::resolve(&workspace.join("a.txt")).unwrap();
        let b = NativeResource::resolve(&workspace.join("b.txt")).unwrap();
        let child = parent()
            .without_workspace_restore()
            .with_resource_scope("parent/task/a");
        exec_ctx::with_current(child.clone(), || {
            let requests = [
                (a.clone(), LockMode::Exclusive),
                (b.clone(), LockMode::Exclusive),
            ];
            let _lease = NativeResourceLease::acquire(
                &workspace,
                &requests,
                Duration::from_secs(1),
                None,
                &child.resource_scope(),
            )
            .unwrap();
            std::fs::write(a.path(), b"base-a").unwrap();
            std::fs::write(b.path(), b"base-b").unwrap();
            native_journal::apply(
                &a,
                &FileFingerprint::for_bytes(b"base-a"),
                b"owned-a",
                &child.resource_scope(),
            )
            .unwrap();
            native_journal::apply(
                &b,
                &FileFingerprint::for_bytes(b"base-b"),
                b"owned-b",
                &child.resource_scope(),
            )
            .unwrap();
        });
        // Non-cooperative editor fixture: b no longer matches the recorded selected-owner postimage.
        std::fs::write(b.path(), b"newer-b").unwrap();
        let tool = NativeOwnerUndo::new(workspace.clone());
        let parent = parent();
        let prepared = tool
            .prepare_call(
                &json!({"owner": "parent/task/a", "paths": ["a.txt", "b.txt"]}),
                &parent,
            )
            .unwrap();
        let requests = match prepared.footprint.as_ref().unwrap() {
            PreparedFootprint::Native(resources) => resources,
            PreparedFootprint::Global | PreparedFootprint::Overlay { .. } => panic!("exact live resources required"),
        };
        let _lease = NativeResourceLease::acquire(
            &workspace,
            requests,
            Duration::from_secs(1),
            None,
            &parent.resource_scope(),
        )
        .unwrap();
        let error =
            exec_ctx::with_current(parent, || tool.execute_prepared(&prepared)).unwrap_err();
        let report = error.downcast_ref::<NativeUndoFailure>().unwrap();
        assert!(report.possibly_applied_record.is_none());
        assert!(report.report.contains("partial_failure"));
        assert!(report
            .report
            .contains("settled_reversals_in_completed_files\":1"));
        assert!(report.report.contains("conflict"));
        assert_eq!(std::fs::read(a.path()).unwrap(), b"base-a");
        assert_eq!(std::fs::read(b.path()).unwrap(), b"newer-b");
    }

    #[test]
    fn native_undo_refuses_raw_traversal_before_canonical_root_join() {
        let dir = TestDir::new();
        let tool = NativeOwnerUndo::new(dir.0.clone());
        let parent = parent();
        let mut absolute = dir.0.as_os_str().to_os_string();
        absolute.push(std::path::MAIN_SEPARATOR_STR);
        absolute.push("../escape.txt");
        for spelling in [
            "../escape.txt",
            "..\\escape.txt",
            "./one.txt",
            "nested/../one.txt",
            absolute.to_str().unwrap(),
        ] {
            let input = json!({"owner": "parent/task/a", "paths": [spelling]});
            for (phase, result) in [
                ("plan", tool.plan_call(&input, &parent)),
                ("prepare", tool.prepare_call(&input, &parent)),
            ] {
                let error = match result {
                    Ok(_) => panic!("unsafe path accepted during {phase}: {input}"),
                    Err(error) => error,
                };
                assert!(
                    error.to_string().contains("traversal"),
                    "wrong refusal during {phase} for {input}: {error:#}"
                );
            }
        }
    }

    #[test]
    fn bounded_exact_arguments_reject_patterns_unrelated_owner_duplicate_paths_and_unknown_fields()
    {
        let dir = TestDir::new();
        let tool = NativeOwnerUndo::new(dir.0.clone());
        let parent = parent();
        for bad in [
            json!({"owner": "parent/task/*", "paths": ["one.txt"]}),
            json!({"owner": "parent/task/a", "paths": []}),
            json!({"owner": "parent-ab/task/a", "paths": ["one.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["one.txt", "one.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["../escape.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["..\\escape.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["./one.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["nested/../one.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["*.txt"]}),
            json!({"owner": "parent/task/a", "paths": ["one.txt"], "group_restore": true}),
            json!({"owner": "parent/task/a", "paths": vec!["one.txt"; MAX_PATHS + 1]}),
            json!({"owner": "x".repeat(MAX_OWNER_BYTES + 1), "paths": ["one.txt"]}),
        ] {
            assert!(
                tool.prepare_call(&bad, &parent).is_err(),
                "invalid argument accepted: {bad}"
            );
        }
        std::fs::create_dir(dir.0.join("directory")).unwrap();
        assert!(tool
            .prepare_call(
                &json!({"owner": "parent/task/a", "paths": ["directory"]}),
                &parent
            )
            .is_err());
    }
}
