//! Approval-bound adapter to the same prepared Git operations used by `aizen time`.
//! The core owns Git leases, receipts and settlement; this layer never rebuilds approved intent.

use super::tools::{ApprovalPreview, PreparedToolCall, Tool, ToolObservation, WorkspaceEffect};
use crate::core::exec_ctx::{self, ExecutionContext};
use crate::core::git_ops::{Change, Prepared, PreparedOutcome};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) struct GitChange {
    root: PathBuf,
}

impl GitChange {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Cloned dispatcher calls share one consuming core payload, never a model-serializable token.
struct FrozenChange {
    parent: ExecutionContext,
    root: PathBuf,
    args: Value,
    change: Mutex<Option<Prepared>>,
}

impl Tool for GitChange {
    fn name(&self) -> &str {
        "git_change"
    }

    fn description(&self) -> &str {
        "Parent-only Git operations; conflicts stay unverified."
    }

    fn parameters(&self) -> Value {
        // Required fields plus exact cardinality close fixed shapes. Only mainline is optional;
        // those two shapes also enumerate keys. Core parsing rechecks every type/bound at runtime.
        let variant = |actions: &[&str], required: &[&str], fields: &[&str]| {
            let mut branch = json!({"properties":{"action":{"enum":actions}},
                "required":required, "maxProperties":fields.len()});
            if fields.contains(&"mainline") {
                branch["propertyNames"] = json!({"enum":fields});
            }
            branch
        };
        json!({
            "type":"object",
            "properties": {
                "action":{"type":"string"},
                "branch":{"type":"string"},
                "commits":{"type":"array","items":{"type":"string"}},
                "mainline":{"type":["integer","null"]},
                "receipt":{"type":"string"},
                "commit":{"type":"string"},
                "paths":{"type":"array","items":{"type":"string"}},
                "step":{"enum":["continue","abort","quit"]},
                "retained_ref":{"type":"string"}
            },
            "required":["action"],
            "additionalProperties":false,
            "oneOf":[
                variant(&["switch"], &["branch"], &["action", "branch"]),
                variant(&["revert_paths"], &["commit", "paths"], &["action", "commit", "paths", "mainline"]),
                variant(&["revert", "cherry_pick"], &["commits"], &["action", "commits", "mainline"]),
                variant(&["redo", "reconcile", "integrate_task", "verify_task"], &["receipt"], &["action", "receipt"]),
                variant(&["sequencer"], &["receipt", "step"], &["action", "receipt", "step"]),
                variant(&["integrate", "merge"], &["commit"], &["action", "commit"]),
                variant(&["delete_branch"], &["branch", "retained_ref"], &["action", "branch", "retained_ref"])
            ]
        })
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::OpaqueWorkspace
    }

    fn workspace_target(&self, _args: &Value) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn plan_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        validate_parent(ctx, false)?;
        super::planning::check_tool(ctx, self)?;
        parse_change(args)?;
        // Resolve Git state only at the serial execution boundary, not when scheduling the batch.
        Ok(PreparedToolCall::new(args.clone()))
    }

    fn prepare_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        validate_parent(ctx, false)?;
        super::planning::check_tool(ctx, self)?;
        let change = parse_change(args)?;
        let change = exec_ctx::with_current(ctx.clone(), || Prepared::prepare(&self.root, change))?;
        let preview = change.preview()?;
        let mut prepared = PreparedToolCall::new(args.clone());
        prepared.preview = Some(ApprovalPreview {
            title: "Git change (project unverified)".into(),
            diff: None,
            lines: preview.lines().map(str::to_owned).collect(),
        });
        prepared.planning_policy = ctx.policy().cloned();
        // No generic Global footprint: core execution owns the writer/physical/background leases.
        prepared.delegation = Some(Arc::new(FrozenChange {
            parent: ctx.clone(),
            root: self.root.clone(),
            args: args.clone(),
            change: Mutex::new(Some(change)),
        }));
        Ok(prepared)
    }

    fn execute(&self, _args: &Value) -> Result<String> {
        anyhow::bail!("git_change requires frozen parent preparation and dispatcher approval")
    }

    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenChange>())
            .context("git_change requires internally frozen parent preparation")?;
        validate_parent(&frozen.parent, true)?;
        let current = exec_ctx::current().context("git_change requires pinned parent execution")?;
        super::planning::check_prepared(&current, self, prepared)?;
        ensure!(
            prepared.args == frozen.args && self.root == frozen.root,
            "Git change arguments or workspace changed; prepare and approve again"
        );
        ensure!(
            prepared.footprint.is_none() && prepared.file.is_none(),
            "Git change admission changed; core Git operations must own their leases"
        );
        let change = frozen
            .change
            .lock()
            .map_err(|_| {
                anyhow::anyhow!("Git change payload is poisoned; inspect before retrying")
            })?
            .take()
            .context("Git change payload already consumed; inspect its receipt, never retry it")?;
        // Consume the EXACT approved payload once. Keep typed ToolEffectsUnsettled in the error
        // chain, including hook/conflict failures; no re-prepare, reset, retry or implicit rollback.
        let outcome = change.execute()?;
        let body = match &outcome {
            PreparedOutcome::Change(receipt) => serde_json::to_string_pretty(receipt)?,
            PreparedOutcome::Verify(record) => serde_json::to_string_pretty(record)?,
            PreparedOutcome::Handoff(handoff) => serde_json::to_string_pretty(handoff)?,
        };
        Ok(ToolObservation::text(body))
    }
}

fn validate_parent(ctx: &ExecutionContext, require_ambient: bool) -> Result<()> {
    ensure!(
        ctx.can_restore_workspace() && ctx.native_scope().is_none(),
        "Git changes require coordinating-parent authority"
    );
    let current = exec_ctx::current();
    ensure!(
        !require_ambient || current.is_some(),
        "Git changes require pinned parent execution"
    );
    if let Some(current) = current {
        ensure!(
            current.can_restore_workspace()
                && current.native_scope().is_none()
                && current.resource_scope() == ctx.resource_scope()
                && current.conversation() == ctx.conversation(),
            "ambient execution cannot borrow another parent's Git authority"
        );
    }
    Ok(())
}

fn parse_change(args: &Value) -> Result<Change> {
    // Core's deny_unknown_fields is also the runtime boundary for yes/force/command/cwd fields.
    let change: Change = serde_json::from_value(args.clone())
        .context("git_change requires one closed Git action and its exact fields")?;
    for value in args
        .as_object()
        .context("Git change must be an object")?
        .values()
    {
        if let Some(text) = value.as_str() {
            ensure!(!text.is_empty(), "Git change fields cannot be empty");
        }
    }
    if let Change::Revert { commits, mainline } | Change::CherryPick { commits, mainline } = &change
    {
        ensure!(
            !commits.is_empty() && commits.len() <= 32 && commits.iter().all(|s| !s.is_empty()),
            "revert/cherry-pick requires 1-32 explicitly ordered commits"
        );
        ensure!(mainline.is_none_or(|n| n > 0), "mainline must be positive");
    }
    if let Change::RevertPaths {
        paths, mainline, ..
    } = &change
    {
        ensure!(
            !paths.is_empty() && paths.len() <= 32 && paths.iter().all(|p| !p.is_empty()),
            "selected inverse requires 1-32 exact paths"
        );
        ensure!(mainline.is_none_or(|n| n > 0), "mainline must be positive");
    }
    Ok(change)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_and_core_parser_close_every_action() {
        let tool = GitChange::new(PathBuf::from("unused"));
        let schema = tool.parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["action"]));
        let actions = schema["oneOf"].as_array().unwrap();
        let inputs = [
            json!({"action":"switch","branch":"topic"}),
            json!({"action":"revert_paths","commit":"HEAD","paths":["file.txt"]}),
            json!({"action":"revert","commits":["HEAD"],"mainline":1}),
            json!({"action":"redo","receipt":"undo-operation-1"}),
            json!({"action":"reconcile","receipt":"failed-operation-1"}),
            json!({"action":"integrate_task","receipt":"git-task-1-2-3"}),
            json!({"action":"verify_task","receipt":"123-456-7"}),
            json!({"action":"sequencer","receipt":"failed-operation-1","step":"continue"}),
            json!({"action":"integrate","commit":"refs/heads/topic"}),
            json!({"action":"merge","commit":"refs/heads/topic"}),
            json!({"action":"cherry_pick","commits":["refs/heads/topic"],"mainline":null}),
            json!({"action":"delete_branch","branch":"topic","retained_ref":"refs/heads/main"}),
        ];
        let action_count: usize = actions
            .iter()
            .map(|v| v["properties"]["action"]["enum"].as_array().unwrap().len())
            .sum();
        assert_eq!(action_count, inputs.len());
        for input in inputs {
            let variants: Vec<_> = actions
                .iter()
                .filter(|v| {
                    v["properties"]["action"]["enum"]
                        .as_array()
                        .unwrap()
                        .contains(&input["action"])
                })
                .collect();
            assert_eq!(
                variants.len(),
                1,
                "action must select exactly one closed branch"
            );
            let variant = variants[0];
            assert!(parse_change(&input).is_ok());
            let allowed = variant["propertyNames"]["enum"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| {
                    std::iter::once(json!("action"))
                        .chain(variant["required"].as_array().unwrap().iter().cloned())
                        .collect()
                });
            assert_eq!(variant["maxProperties"], allowed.len());
            for key in input.as_object().unwrap().keys() {
                assert!(allowed.contains(&json!(key)));
            }
            for key in ["yes", "force", "command", "cwd", "unexpected"] {
                let mut bad = input.clone();
                bad[key] = json!(true);
                assert!(parse_change(&bad).is_err(), "accepted {bad}");
                assert!(!allowed.contains(&json!(key)));
            }
            for key in variant["required"].as_array().unwrap() {
                let mut bad = input.clone();
                bad.as_object_mut().unwrap().remove(key.as_str().unwrap());
                assert!(parse_change(&bad).is_err(), "accepted {bad}");
            }
        }
        for bad in [
            json!({"action":"reset","commit":"HEAD"}),
            json!({"action":"switch","branch":"topic","receipt":"receipt-1"}),
            json!({"action":"delete_branch","branch":"topic","retained_ref":"main","commit":"HEAD"}),
            json!({"action":"revert","commits":[],"mainline":1}),
            json!({"action":"revert","commits":["HEAD"],"mainline":0}),
            json!({"action":"revert","commits":["HEAD"],"mainline":1.5}),
            json!({"action":"redo","receipt":true}),
            json!({"branch":"topic"}),
        ] {
            assert!(parse_change(&bad).is_err(), "accepted {bad}");
        }
        assert!(tool.is_destructive());
        assert!(!tool.is_concurrency_safe());
        assert!(tool.recovery_effect(&Value::Null));
        assert_eq!(
            tool.workspace_effect(&Value::Null),
            WorkspaceEffect::OpaqueWorkspace
        );
        assert!(tool
            .execute(&json!({"action":"switch","branch":"topic","yes":true}))
            .is_err());
        assert!(tool
            .execute_prepared(&PreparedToolCall::new(Value::Null))
            .is_err());
    }

    struct Fixture {
        root: PathBuf,
        git: PathBuf,
    }

    impl Fixture {
        fn git(&self, args: &[&str]) -> Result<String> {
            // Fixed test-only commands in a disposable repository, never model-supplied argv.
            let output = std::process::Command::new(&self.git)
                .current_dir(&self.root)
                .args(args)
                .output()?;
            ensure!(
                output.status.success(),
                "fixture Git: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)?.trim().into())
        }
    }

    fn fixture(
        test: impl FnOnce(&Fixture, &GitChange, &ExecutionContext) -> Result<()>,
    ) -> Result<()> {
        let _mode = crate::sandbox::runner::tests::lock();
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct Mode(crate::sandbox::SandboxMode);
        impl Drop for Mode {
            fn drop(&mut self) {
                crate::sandbox::set_mode(self.0);
            }
        }
        let _restore = Mode(crate::sandbox::mode());
        crate::sandbox::set_mode(crate::sandbox::SandboxMode::Guarded);
        let temp = crate::core::file_placement::tests::TestDir::new();
        let root = temp.path().join("repo");
        let home = temp.path().join("home");
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&home)?;
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let f = Fixture {
            root,
            git: crate::core::gitx::git_exe().context("Git required for Git tool fixtures")?,
        };
        f.git(&["init", "-q"])?;
        // Only the fixture disables hooks/signing; production mutations preserve user config.
        f.git(&["config", "user.name", "Fixture"])?;
        f.git(&["config", "user.email", "fixture@example.invalid"])?;
        f.git(&["config", "commit.gpgsign", "false"])?;
        f.git(&["config", "core.hooksPath", ".git/hooks"])?;
        std::fs::write(f.root.join("a.txt"), b"base\n")?;
        f.git(&["add", "a.txt"])?;
        f.git(&["commit", "-qm", "base"])?;
        f.git(&["branch", "topic"])?;
        let tool = GitChange::new(f.root.clone());
        let parent = ExecutionContext::default().with_resource_scope(format!(
            "git-tool/{}",
            crate::core::persist::unique_sequence()
        ));
        crate::core::workspace_txn::with_test_lock_home(&home, || {
            exec_ctx::with_current(parent.clone(), || test(&f, &tool, &parent))
        })
    }

    #[test]
    fn preview_is_read_only_and_runtime_cannot_borrow_parent_authority() -> Result<()> {
        fixture(|f, tool, parent| {
            let args = json!({"action":"switch","branch":"topic"});
            let head = f.git(&["rev-parse", "HEAD"])?;
            let branch = f.git(&["symbolic-ref", "HEAD"])?;
            let planned = tool.plan_call(&args, parent)?;
            assert!(planned.delegation.is_none());
            let prepared = tool.prepare_checked(&args, parent)?;
            let preview = prepared.preview.as_ref().unwrap().lines.join("\n");
            assert!(preview.contains(&head));
            assert!(preview.contains("frozen_targets"));
            assert_eq!(f.git(&["symbolic-ref", "HEAD"])?, branch);
            assert_eq!(f.git(&["rev-parse", "HEAD"])?, head);
            assert!(crate::core::git_ops::operation_receipts(&f.root)?.is_empty());
            let child = parent.clone().without_workspace_restore();
            exec_ctx::with_current(child.clone(), || {
                assert!(tool.plan_call(&args, &child).is_err());
                assert!(tool.prepare_call(&args, parent).is_err());
                assert!(tool.execute_prepared(&prepared).is_err());
            });
            exec_ctx::with_current(parent.clone().with_resource_scope("foreign"), || {
                assert!(tool.execute_prepared(&prepared).is_err());
            });
            let mut changed = prepared.clone();
            changed.args["branch"] = json!("other");
            assert!(tool.execute_prepared(&changed).is_err());
            assert_eq!(f.git(&["symbolic-ref", "HEAD"])?, branch);
            assert!(crate::core::git_ops::operation_receipts(&f.root)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn stale_target_refuses_without_repreparing_or_consuming_another_operation() -> Result<()> {
        fixture(|f, tool, parent| {
            let prepared =
                tool.prepare_checked(&json!({"action":"switch","branch":"topic"}), parent)?;
            let before = f.git(&["rev-parse", "HEAD"])?;
            let branch = f.git(&["symbolic-ref", "HEAD"])?;
            // Move only the target ref after preview; don't change the active worktree.
            let tree = f.git(&["rev-parse", "HEAD^{tree}"])?;
            let next = f.git(&["commit-tree", &tree, "-p", &before, "-m", "moved target"])?;
            f.git(&["update-ref", "refs/heads/topic", &next])?;
            assert!(tool.execute_prepared(&prepared).is_err());
            assert!(tool
                .execute_prepared(&prepared.clone())
                .unwrap_err()
                .to_string()
                .contains("already consumed"));
            assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
            assert_eq!(f.git(&["symbolic-ref", "HEAD"])?, branch);
            assert!(crate::core::git_ops::operation_receipts(&f.root)?.is_empty());
            Ok(())
        })
    }

    #[test]
    fn approved_core_payload_executes_once_with_unverified_receipt() -> Result<()> {
        fixture(|f, tool, parent| {
            let mut input = json!({"action":"switch","branch":"topic"});
            let prepared = tool.prepare_checked(&input, parent)?;
            input["branch"] = json!("never-resolved");
            let mut registry = crate::agent::tools::ToolRegistry::new();
            registry.register(Box::new(GitChange::new(f.root.clone())));
            let cfg = crate::agent::AgentConfig {
                exec_ctx: parent.clone(),
                workspace_root: Some(f.root.clone()),
                quiet: true,
                enable_verify_gate: false,
                approval_mode: crate::core::approval::ApprovalMode::Yolo,
                ..Default::default()
            };
            let call = crate::core::types::ToolCall {
                id: "approved-git".into(),
                kind: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "git_change".into(),
                    // Dispatch must consume preparation, not this now-moving wire argument.
                    arguments: input.to_string(),
                },
            };
            let mut sink = vec![crate::core::types::Message::tool_result(
                &call.id, "pending",
            )];
            let mut writer = None;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let output = runtime.block_on(crate::agent::execute_prepared_calls(
                &registry,
                &[call],
                &[Ok(prepared.clone())],
                &cfg,
                &mut sink,
                Vec::new(),
                &mut writer,
            ));
            assert!(
                writer.is_none(),
                "Git core, not the dispatcher, owns this writer lease"
            );
            let receipt: Value = serde_json::from_str(&output[0].1)
                .with_context(|| format!("Git dispatcher output: {}", output[0].1))?;
            assert_eq!(receipt["verified"], false);
            assert_eq!(receipt["state"], "settled_unverified");
            assert_eq!(receipt["change"]["branch"], "topic");
            assert_eq!(f.git(&["symbolic-ref", "HEAD"])?, "refs/heads/topic");
            assert!(tool.execute_prepared(&prepared.clone()).is_err());
            assert_eq!(crate::core::git_ops::operation_receipts(&f.root)?.len(), 1);
            Ok(())
        })
    }
}
