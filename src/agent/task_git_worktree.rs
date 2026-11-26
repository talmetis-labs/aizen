//! Approval-bound task opt-in. Initially native-only: no promise of shell/build isolation.
use super::*;
use crate::core::{
    exec_ctx::{self, ExecutionContext},
    git_ops::PreparedOwnedTask,
};
use std::sync::{Arc, Mutex};

struct Frozen {
    args: Value,
    parent_scope: String,
    parent_root: PathBuf,
    dispatch: PreparedTaskDispatch,
    creation: Mutex<Option<PreparedOwnedTask>>,
}

pub(super) fn requested(args: &Value) -> bool {
    args.get("isolation").and_then(Value::as_str) == Some("worktree")
}

pub(super) fn validate(args: &Value, ctx: &ExecutionContext) -> Result<()> {
    anyhow::ensure!(
        ctx.can_restore_workspace() && ctx.native_scope().is_none(),
        "owned worktree task requires unrestricted coordinating parent"
    );
    anyhow::ensure!(
        args.get("agent").is_none()
            && args
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| crate::agent::roles::canonical(role)
                    .is_some_and(|p| p.name == "daedalus")),
        "worktree isolation currently requires the daedalus native-edit role, not a specialist"
    );
    anyhow::ensure!(
        args.get("execution").is_none_or(|v| v == "direct"),
        "worktree isolation requires direct execution"
    );
    anyhow::ensure!(
        reference_ids(args)?.is_empty() && evidence_ids(args).is_empty(),
        "native worktree tasks do not accept or return media"
    );
    let writes =
        scope_paths(args, "write_paths")?.context("worktree task requires exact write_paths")?;
    let reads = scope_paths(args, "read_paths")?.unwrap_or_default();
    for name in reads.iter().chain(&writes) {
        let path = std::path::Path::new(name);
        anyhow::ensure!(
            !path.is_absolute()
                && !name.contains('\\')
                && !name.contains(':')
                && !name.chars().any(char::is_control)
                && path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                && !path
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
            "worktree task paths must be exact relative files inside the new checkout"
        );
        crate::core::workspace_txn::NativeResource::validate_spelling(path)?;
    }
    Ok(())
}

pub(super) fn prepare(
    tool: &TaskTool,
    args: &Value,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    validate(args, ctx)?;
    let dispatch = tool.prepare_dispatch(args, true)?;
    let creation = exec_ctx::with_current(ctx.clone(), || {
        PreparedOwnedTask::prepare(&tool.root, &ctx.resource_scope())
    })?;
    let mut call = PreparedToolCall::new(args.clone());
    call.preview = Some(crate::agent::tools::ApprovalPreview {
        title: "New Git-owned native task; commit without integration".into(),
        diff: None,
        lines: creation.preview()?.lines().map(str::to_owned).collect(),
    });
    call.delegation = Some(Arc::new(Frozen {
        args: args.clone(),
        parent_scope: ctx.resource_scope(),
        parent_root: tool.root.clone(),
        dispatch,
        creation: Mutex::new(Some(creation)),
    }));
    Ok(call)
}

pub(super) fn frozen(call: &PreparedToolCall) -> bool {
    call.delegation
        .as_ref()
        .is_some_and(|d| d.downcast_ref::<Frozen>().is_some())
}

pub(super) fn execute(tool: &TaskTool, call: &PreparedToolCall) -> Result<ToolObservation> {
    let frozen = call
        .delegation
        .as_ref()
        .and_then(|d| d.downcast_ref::<Frozen>())
        .context("missing owned task intent")?;
    let parent = exec_ctx::current().context("owned task needs bound parent")?;
    validate(&call.args, &parent)?;
    anyhow::ensure!(
        frozen.args == call.args
            && parent.resource_scope() == frozen.parent_scope
            && tool.root == frozen.parent_root,
        "owned task approval changed"
    );
    let creation = frozen
        .creation
        .lock()
        .map_err(|_| anyhow::anyhow!("owned task intent poisoned"))?
        .take()
        .context("owned task intent consumed; inspect retained worktree, do not replay")?;
    let owned = Arc::new(creation.execute()?);
    let mut tool = tool.clone();
    tool.root = owned.root().to_path_buf();
    let scope = resolve_native_scope(&call.args, &tool.root, &parent)?
        .context("missing owned native scope")?;
    let child = child_execution_context(&parent, owned.scope(), &tool.root, "owned native task")
        .for_owned_git_task(owned.clone(), scope.clone())?;
    let mut dispatch = frozen.dispatch.clone();
    // Rebuild tool objects at the child root, keeping endpoint/budget resolution frozen.
    exec_ctx::with_current(child.clone(), || -> Result<()> {
        dispatch.dispatch.registry = crate::agent::builtin::role_registry("daedalus", &tool.root);
        dispatch.dispatch.system = build_subagent_prompt(
            "daedalus",
            &dispatch.dispatch.registry.advertised_names(),
            &tool.root,
            &dispatch.dispatch.model,
            &chrono::Local::now().format("%Y-%m-%d").to_string(),
            Some(&TaskContract::from_args(&call.args)),
            call.args.get("prompt").and_then(Value::as_str),
        );
        seat_native_registry(
            &mut dispatch.dispatch.registry,
            &mut dispatch.dispatch.system,
            &scope,
        )?;
        if let Some(schema) = &dispatch.dispatch.expects {
            append_output_contract(&mut dispatch.dispatch.system, schema);
        }
        Ok(())
    })?;
    let baseline = owned.capture_baseline(&scope)?;
    // The coordinator mints durable native admission before exposing the child driver. The
    // child remains native-only and cannot inspect Git or create its own lifecycle authority.
    let attempt = owned.begin_native_attempt(&scope)?;
    let released = Arc::new(Mutex::new(None));
    let complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
    dispatch.dispatch.native_scope = Some(scope.clone());
    dispatch.owned_task = Some(owned.clone());
    dispatch.owned_complete = Some(complete.clone());
    dispatch.owned_attempt = Some(attempt);
    dispatch.owned_release = Some(released.clone());
    // Keep parent cancellation/policy/physical ownership, but no parent tree evidence collectors.
    let mut observation =
        exec_ctx::with_current(child, || tool.execute_dispatch(&call.args, &dispatch))
            .with_context(|| {
                format!(
                    "owned task retained at {:?}; no automatic retry, cleanup or integration",
                    owned.root()
                )
            })?;
    anyhow::ensure!(
        complete.load(std::sync::atomic::Ordering::Acquire),
        "owned task did not finish an eligible native handoff; changes retained at {:?}, no commit",
        owned.root()
    );
    let released = released
        .lock()
        .map_err(|_| anyhow::anyhow!("Git-owned release state poisoned"))?
        .take()
        .context("Git-owned task returned no physical release proof; retain without commit")?;
    let handoff = owned.clone().finish_native(&scope, &baseline, released)
        .with_context(|| format!("owned task commit/handoff blocked at {:?}; inspect retained index, branch and ownership metadata, no retry", owned.root()))?;
    observation.text = format!(
        "[Git task handoff: UNVERIFIED, NOT INTEGRATED]\n{}\n{}",
        serde_json::to_string_pretty(&handoff)?,
        observation.text
    );
    Ok(observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args() -> Value {
        json!({"isolation": "worktree", "role": "daedalus", "write_paths": ["src/lib.rs"]})
    }

    #[test]
    fn accepts_exact_relative_native_paths_and_role_alias() {
        let ctx = ExecutionContext::default();
        for role in ["daedalus", "coder"] {
            let mut input = args();
            input["role"] = json!(role);
            input["execution"] = json!("direct");
            input["read_paths"] = json!(["Cargo.toml", "docs/guide.md"]);
            validate(&input, &ctx).unwrap();
        }
    }

    #[test]
    fn rejects_unsafe_spelling_in_reads_and_writes() {
        let ctx = ExecutionContext::default();
        for path in [
            "",
            ".",
            "..",
            "../escape.rs",
            "src/../escape.rs",
            "./src/lib.rs",
            "src/./lib.rs",
            "/absolute.rs",
            "C:/absolute.rs",
            "C:relative.rs",
            r"src\lib.rs",
            r"\server\share\lib.rs",
            "src/lib.rs:stream",
            ".git/config",
            ".GiT/config",
            "src/.git/config",
            "src/",
            "src/*.rs",
            "src/?.rs",
            "src/[a].rs",
            "src/{a,b}.rs",
            ":(glob)src/*.rs",
            "src/trailing.",
            "src/trailing ",
            "NUL.rs",
            "src/COM1.txt",
            "src/line\nbreak.rs",
            "src/tab\tname.rs",
            "src/zero\0.rs",
        ] {
            for field in ["read_paths", "write_paths"] {
                let mut input = args();
                input[field] = json!([path]);
                assert!(
                    validate(&input, &ctx).is_err(),
                    "accepted {field}: {path:?}"
                );
            }
        }
    }

    #[test]
    fn requires_bounded_nonempty_arrays_of_path_strings() {
        let ctx = ExecutionContext::default();
        let mut input = args();
        input.as_object_mut().unwrap().remove("write_paths");
        assert!(validate(&input, &ctx).is_err());
        for field in ["read_paths", "write_paths"] {
            for value in [
                Value::Null,
                json!("src/lib.rs"),
                json!([]),
                json!([1]),
                json!(["src/lib.rs", false]),
                json!(vec!["src/lib.rs"; 33]),
            ] {
                let mut input = args();
                input[field] = value;
                assert!(
                    validate(&input, &ctx).is_err(),
                    "accepted malformed {field}: {input}"
                );
            }
            let mut input = args();
            input[field] = json!((0..32)
                .map(|i| format!("src/file{i}.rs"))
                .collect::<Vec<_>>());
            validate(&input, &ctx).unwrap();
        }
    }

    #[test]
    fn rejects_specialists_other_roles_and_speculative_execution() {
        let ctx = ExecutionContext::default();
        for role in [
            Value::Null,
            json!("argus"),
            json!("themis"),
            json!("unknown"),
            json!(1),
        ] {
            let mut input = args();
            input["role"] = role;
            assert!(validate(&input, &ctx).is_err());
        }
        let mut input = args();
        input.as_object_mut().unwrap().remove("role");
        assert!(validate(&input, &ctx).is_err());
        for value in [Value::Null, json!("specialist")] {
            let mut input = args();
            input["agent"] = value;
            assert!(validate(&input, &ctx).is_err());
        }
        for mode in [
            Value::Null,
            json!("speculative"),
            json!("unknown"),
            json!(true),
        ] {
            let mut input = args();
            input["execution"] = mode;
            assert!(validate(&input, &ctx).is_err());
        }
    }

    #[test]
    fn rejects_media_selection() {
        let ctx = ExecutionContext::default();
        for field in ["reference_ids", "return_evidence"] {
            let mut input = args();
            input[field] = json!(["evidence-1"]);
            assert!(validate(&input, &ctx).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn rejects_delegated_and_native_scoped_parents() {
        let parent = ExecutionContext::default();
        assert!(validate(&args(), &parent.without_workspace_restore()).is_err());
        let root = std::env::temp_dir().join(format!(
            "aizen-worktree-input-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["file.rs".to_owned()])
                .unwrap();
        let scoped = parent.with_native_scope(scope).unwrap();
        assert!(scoped.can_restore_workspace());
        assert!(validate(&args(), &scoped).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
