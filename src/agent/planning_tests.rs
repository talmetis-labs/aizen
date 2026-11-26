use super::*;
use crate::agent::todo::TaskContext;
use crate::agent::tools::Tool;
use crate::core::session_control::{ExecutionMode, PlanTarget};
use std::sync::atomic::{AtomicUsize, Ordering};

fn context() -> (TaskContext, ExecutionContext) {
    let task = TaskContext::private();
    task.control()
        .transition(
            ExecutionMode::Planning,
            Some(PlanTarget {
                root: std::env::temp_dir().to_string_lossy().into_owned(),
                path: "docs/plans/test.md".into(),
                id: "p-0123456789abcdef0123456789abcdef".into(),
            }),
            |_| Ok(()),
        )
        .unwrap();
    let ctx = ExecutionContext::default().with_task(task.clone());
    (task, ctx)
}
struct Spoof(AtomicUsize);
impl Tool for Spoof {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Untrusted name-only capability"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn prepare_call(
        &self,
        _: &serde_json::Value,
        _: &ExecutionContext,
    ) -> Result<PreparedToolCall> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("preparation must never run")
    }
    fn execute(&self, _: &serde_json::Value) -> Result<String> {
        panic!("body must never run")
    }
}

#[test]
fn prepared_call_rejects_a_rebound_workspace_generation() {
    use crate::core::file_placement::{tests::TestDir, PlacementContext};
    let first = TestDir::new();
    let second = TestDir::new();
    let home = first.path().join("home");
    let first_placement = PlacementContext::with_paths(
        first.path().to_path_buf(),
        home.clone(),
        first.path().join("scratch"),
        "binding-first",
    )
    .unwrap();
    let second_placement = PlacementContext::with_paths(
        second.path().to_path_buf(),
        home,
        second.path().join("scratch"),
        "binding-second",
    )
    .unwrap();
    let ctx = ExecutionContext::default().with_placement(first_placement);
    let tool = crate::agent::todo::TodoWrite;
    let prepared = prepare(&tool, &serde_json::json!({"todos": []}), &ctx).unwrap();
    check_prepared(&ctx, &tool, &prepared).unwrap();
    let rebound = ctx.with_placement(second_placement);
    assert!(check_prepared(&rebound, &tool, &prepared)
        .unwrap_err()
        .to_string()
        .contains("stale"));
    assert!(check_prepared(&ExecutionContext::default(), &tool, &prepared).is_err());
    let unbound = PreparedToolCall::new(serde_json::json!({"todos": []}));
    assert!(check_prepared(&ctx, &tool, &unbound).is_err());
    let unchanged = ctx.with_placement(ctx.placement().unwrap().clone());
    check_prepared(&unchanged, &tool, &prepared).unwrap();
    // Returning to the same pathname must not revive the earlier token (A -> B -> A).
    let returned = rebound.with_placement(ctx.placement().unwrap().clone());
    assert!(check_prepared(&returned, &tool, &prepared).is_err());
    // Children pinned before a future focus keep their immutable preparation, never reroute it.
    let child = ctx.with_resource_scope("original-child");
    check_prepared(&child, &tool, &prepared).unwrap();
}

#[test]
fn prepared_call_without_binding_remains_compatible_with_unbound_contexts() {
    let ctx = ExecutionContext::default();
    let tool = crate::agent::todo::TodoWrite;
    let prepared = prepare(&tool, &serde_json::json!({"todos": []}), &ctx).unwrap();
    check_prepared(&ctx, &tool, &prepared).unwrap();
}

#[test]
fn prepared_binding_includes_project_identity_and_home_not_only_tool_cwd() {
    use crate::core::file_placement::{tests::TestDir, PlacementContext};
    let fixture = TestDir::new();
    let root = fixture.path().canonicalize().unwrap();
    let placement = PlacementContext::with_paths(
        root.clone(),
        root.join("home"),
        root.join("scratch"),
        "project-binding",
    )
    .unwrap();
    let ctx = ExecutionContext::default().with_placement(placement.clone());
    let tool = crate::agent::todo::TodoWrite;
    let call = prepare(&tool, &serde_json::json!({"todos": []}), &ctx).unwrap();
    let moved_project =
        ctx.with_placement(placement.with_project_root(root.join("project")).unwrap());
    assert!(check_prepared(&moved_project, &tool, &call).is_err());
    let moved_home = ctx.with_placement(
        PlacementContext::with_paths(
            root.clone(),
            root.join("other-home"),
            root.join("scratch"),
            "project-binding",
        )
        .unwrap(),
    );
    assert!(check_prepared(&moved_home, &tool, &call).is_err());
}

#[test]
fn planning_denies_spoofed_reader_before_preparation_and_shared_body() {
    let (_task, ctx) = context();
    let tool = std::sync::Arc::new(Spoof(AtomicUsize::new(0)));
    assert!(prepare(tool.as_ref(), &serde_json::json!({}), &ctx).is_err());
    assert_eq!(tool.0.load(Ordering::SeqCst), 0);
    let result = crate::core::exec_ctx::with_current(ctx, || {
        crate::agent::run_prepared_tool_body(
            tool.clone(),
            &PreparedToolCall::new(serde_json::json!({})),
            true,
            crate::agent::AgentConfig::default().result_budgets(),
            None,
        )
    });
    assert!(result.text.starts_with("error:"), "{}", result.text);
    assert_eq!(tool.0.load(Ordering::SeqCst), 0);
}

#[test]
fn planning_registry_has_only_audited_native_entries_and_no_deferred_dispatch() {
    let (_task, ctx) = context();
    let registry = crate::core::exec_ctx::with_current(ctx.clone(), || {
        crate::agent::builtin::default_registry_in(&std::env::temp_dir())
    });
    let names = registry.names();
    assert_eq!(
        names,
        vec![
            "file_read",
            "file_glob",
            "search_files",
            "file_write",
            "todo_write"
        ]
    );
    for tool in registry.tools() {
        check_tool(&ctx, tool.as_ref()).unwrap();
    }
    for name in [
        "mcp_read",
        "shell_run",
        "task",
        "workflow",
        "file_edit",
        "checkpoint",
        "native_owner_undo",
    ] {
        assert!(registry.get(name).is_none());
    }
    assert!(registry.deferred_summary().is_none());
}

#[test]
fn prepared_call_cannot_cross_policy_generation_or_lose_context() {
    let (task, ctx) = context();
    let tool = crate::agent::todo::TodoWrite;
    let prepared = prepare(&tool, &serde_json::json!({"todos":[]}), &ctx).unwrap();
    check_prepared(&ctx, &tool, &prepared).unwrap();
    assert!(check_prepared(&ExecutionContext::default(), &tool, &prepared).is_err());
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
    assert!(check_prepared(&ctx, &tool, &prepared).is_err());
    let new = ExecutionContext::default().with_task(task);
    assert!(check_prepared(&new, &tool, &prepared).is_err());
}

#[test]
fn private_task_and_builders_cannot_strip_policy_or_worker_admission() {
    let (task, ctx) = context();
    let admitted = ctx.admit_policy().unwrap();
    let child = admitted
        .with_task(TaskContext::private())
        .without_workspace_restore()
        .with_resource_scope("child");
    assert!(child.is_planning());
    assert!(child.task().unwrap().control().snapshot().is_planning());
    drop(admitted);
    assert!(task
        .control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .is_err());
    drop(child);
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
    assert!(ctx.is_planning());
    assert!(ctx.admit_policy().is_err());
}

#[test]
fn missing_or_foreign_context_cannot_escape_bound_planning_owner() {
    let (_task, ctx) = context();
    crate::core::exec_ctx::with_current(ctx, || {
        assert!(check_tool(&ExecutionContext::default(), &crate::agent::todo::TodoWrite).is_err());
        let foreign = ExecutionContext::default().with_task(TaskContext::private());
        assert!(foreign.admit_policy().is_err());
    });
}

#[test]
fn scoped_unbound_parent_freezes_one_owner_before_child_forks() {
    let parent = ExecutionContext::default().with_resource_scope("workflow");
    let child = parent.with_resource_scope("workflow/child");
    let grandchild = child.with_resource_scope("workflow/child/worker");
    let parent_policy = parent.task().unwrap().control().policy();
    parent_policy
        .check_inheritance(child.policy().unwrap())
        .unwrap();
    parent_policy
        .check_inheritance(grandchild.policy().unwrap())
        .unwrap();
    let child = child
        .with_task(child.task().unwrap().clone())
        .admit_policy()
        .unwrap();
    let call = prepare(
        &crate::agent::todo::TodoWrite,
        &serde_json::json!({"todos":[]}),
        &child,
    )
    .unwrap();
    crate::core::exec_ctx::with_current(grandchild, || {
        check_prepared(&child, &crate::agent::todo::TodoWrite, &call).unwrap();
    });
    assert!(parent
        .task()
        .unwrap()
        .control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .is_err());
}

#[test]
fn planning_shared_body_writes_only_exact_document_and_preserves_normal_writer() {
    use crate::core::file_placement::{tests::TestDir, PlacementContext};
    let fixture = TestDir::new();
    let root = fixture.path().canonicalize().unwrap();
    let task = TaskContext::private();
    let id = "p-0123456789abcdef0123456789abcdef";
    task.control()
        .transition(
            ExecutionMode::Planning,
            Some(PlanTarget {
                root: root.to_string_lossy().into_owned(),
                path: "docs/plans/test.md".into(),
                id: id.into(),
            }),
            |_| Ok(()),
        )
        .unwrap();
    let ctx = ExecutionContext::default()
        .with_task(task.clone())
        .with_placement(
            PlacementContext::with_paths(
                root.clone(),
                root.join("home"),
                root.join("process"),
                "planning",
            )
            .unwrap(),
        );
    crate::core::workspace_txn::with_test_lock_home(&root, || {
        crate::core::exec_ctx::with_current(ctx.clone(), || {
            let registry = crate::agent::builtin::default_registry_in(&root);
            let writer = registry.get_arc("file_write").unwrap();
            let args = serde_json::json!({"path":"docs/plans/test.md", "content":format!("---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase inspect -->\n# Inspect\n")});
            let call = writer.prepare_checked(&args, &ctx).unwrap();
            let outcome = crate::agent::run_prepared_tool_body(
                writer.clone(),
                &call,
                true,
                crate::agent::AgentConfig::default().result_budgets(),
                None,
            );
            assert!(outcome.failure.is_none(), "{}", outcome.text);
            let execution = crate::agent::tools::ToolExecution {
                call_id: "plan".into(),
                prepared: Some(call),
                outcome,
            };
            assert!(execution.delivered_plan());
            assert!(execution.changed_workspace(writer.as_ref()));
            assert!(task.snapshot().obligations.is_empty());
            let forbidden =
                serde_json::json!({"path":"other.rs", "content":"oops", "purpose":"project"});
            assert!(writer.prepare_checked(&forbidden, &ctx).is_err());
            // Existing parents take the Native footprint, not the Global creation path.
            let mut revised = args;
            revised["content"] = format!(
                "---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase inspect -->\n# Revised\n"
            )
            .into();
            let call = writer.prepare_checked(&revised, &ctx).unwrap();
            let outcome = crate::agent::run_prepared_tool_body(
                writer,
                &call,
                true,
                crate::agent::AgentConfig::default().result_budgets(),
                None,
            );
            assert!(outcome.failure.is_none(), "{}", outcome.text);
        });
    });
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
    let normal = ExecutionContext::default()
        .with_task(task)
        .with_placement(ctx.placement().unwrap().clone());
    crate::core::exec_ctx::with_current(normal.clone(), || {
        let registry = crate::agent::builtin::default_registry_in(&root);
        let writer = registry.get("file_write").unwrap();
        let call = writer
            .prepare_checked(
                &serde_json::json!({"path":"normal.rs", "content":"normal", "purpose":"project"}),
                &normal,
            )
            .unwrap();
        check_prepared(&normal, writer, &call).unwrap();
        assert!(crate::agent::builtin::plan_write_proof(&call).is_none());
    });
}

#[tokio::test]
async fn planning_task_binding_cannot_be_shadowed_by_default_or_foreign_tls() {
    let (task, ctx) = context();
    crate::agent::todo::with_context(task.clone(), async {
        assert!(!ExecutionContext::default().permits_automation());
        assert!(!ExecutionContext::default()
            .with_task(TaskContext::private())
            .permits_automation());
        assert!(crate::agent::observe::spill_to_scratch("planning", "must not persist").is_none());
        crate::core::exec_ctx::with_current(
            ExecutionContext::default().with_task(TaskContext::private()),
            || {
                let bound = crate::core::exec_ctx::current().unwrap();
                assert!(bound.is_planning());
                task.control()
                    .policy()
                    .check_inheritance(bound.policy().unwrap())
                    .unwrap();
                assert!(!bound.permits_automation());
            },
        );
    })
    .await;
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
    crate::core::exec_ctx::with_current(ctx, || {
        assert!(!ExecutionContext::default().permits_automation());
        assert!(crate::agent::observe::spill_to_scratch("stale", "must not replay").is_none());
    });
}

#[test]
fn planning_search_caps_untrusted_limits_and_refuses_unbounded_patterns() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let root = fixture.path().canonicalize().unwrap();
    std::fs::write(root.join("hits.txt"), "needle\n".repeat(500)).unwrap();
    let (_, ctx) = context();
    crate::core::exec_ctx::with_current(ctx, || {
        let search = crate::agent::search::SearchFiles::new(root);
        let output = search
            .execute(&serde_json::json!({"pattern":"needle", "max_results":u64::MAX}))
            .unwrap();
        assert!(output.contains("capped at 200"), "{output}");
        assert!(
            output
                .lines()
                .filter(|line| line.contains("needle"))
                .count()
                <= 200
        );
        assert!(search
            .execute(&serde_json::json!({"pattern":"a".repeat(4097)}))
            .is_err());
    });
}

#[test]
fn planning_direct_native_writers_cannot_use_normal_preparations() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let root = fixture.path().canonicalize().unwrap();
    std::fs::write(root.join("source.txt"), "original").unwrap();
    let registry = crate::agent::builtin::file_registry_for_test(&root);
    let write = registry.get("file_write").unwrap();
    let move_tool = registry.get("file_move").unwrap();
    let write_args =
        serde_json::json!({"path":"other.txt", "content":"forbidden", "purpose":"project"});
    let move_args = serde_json::json!({"from":"source.txt", "to":"moved.txt", "purpose":"project"});
    let prepared_write = write
        .prepare_call(&write_args, &ExecutionContext::default())
        .unwrap();
    let prepared_move = move_tool
        .prepare_call(&move_args, &ExecutionContext::default())
        .unwrap();
    let (_, ctx) = context();
    crate::core::exec_ctx::with_current(ctx, || {
        for name in ["file_edit", "file_move"] {
            let tool = registry.get(name).unwrap();
            let error = tool.execute(&serde_json::json!({})).unwrap_err();
            assert!(
                error.to_string().contains("unavailable in planning mode"),
                "{error:#}"
            );
            assert!(tool
                .prepare_call(&serde_json::json!({}), &ExecutionContext::default())
                .is_err());
        }
        assert!(write.execute(&write_args).is_err());
        assert!(write.execute_prepared(&prepared_write).is_err());
        assert!(move_tool.execute_prepared(&prepared_move).is_err());
        assert!(crate::core::gitx::command().is_err());
        assert!(crate::core::gitx::git_exe().is_none());
        assert!(crate::core::gitx::resolution_note().is_none());
    });
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).unwrap(),
        "original"
    );
    assert!(!root.join("other.txt").exists());
    assert!(!root.join("moved.txt").exists());
}

#[test]
fn planning_shared_reader_keeps_admission_until_body_settles() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let root = fixture.path().canonicalize().unwrap();
    std::fs::write(root.join("read.txt"), "read only").unwrap();
    let (task, ctx) = context();
    let ctx = ctx.with_placement(
        crate::core::file_placement::PlacementContext::with_paths(
            root.clone(),
            root.join("home"),
            root.join("process"),
            "planning-read",
        )
        .unwrap(),
    );
    crate::core::workspace_txn::with_test_lock_home(&root, || {
        crate::core::exec_ctx::with_current(ctx.clone(), || {
            let registry = crate::agent::builtin::file_registry_for_test(&root);
            let reader = registry.get_arc("file_read").unwrap();
            let call = reader
                .prepare_checked(&serde_json::json!({"path":"read.txt"}), &ctx)
                .unwrap();
            let control = task.control();
            let result = crate::agent::builtin::with_native_body_hook_for_test(
                Box::new(move |_, _| {
                    assert!(control
                        .transition(ExecutionMode::Normal, None, |_| Ok(()))
                        .is_err());
                }),
                || {
                    crate::agent::run_prepared_tool_body(
                        reader,
                        &call,
                        true,
                        crate::agent::AgentConfig::default().result_budgets(),
                        None,
                    )
                },
            );
            assert!(result.failure.is_none(), "{}", result.text);
        });
    });
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
}

#[test]
fn planning_nested_context_keeps_own_admission_when_parent_has_none() {
    let (task, ctx) = context();
    let admitted = ctx.admit_policy().unwrap();
    let retained = crate::core::exec_ctx::with_current(ctx, || {
        crate::core::exec_ctx::with_current(admitted, || crate::core::exec_ctx::current().unwrap())
    });
    assert!(task
        .control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .is_err());
    drop(retained);
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
}

#[test]
fn planning_async_context_restores_tls_between_polls_and_retains_worker_admission() {
    use std::future::Future;
    use std::task::{Context, Poll};
    let (task, ctx) = context();
    let admitted = ctx.admit_policy().unwrap();
    let check = task.control().policy();
    let mut polls = 0;
    let mut worker = Box::pin(crate::core::exec_ctx::with_current_future(
        admitted,
        std::future::poll_fn(move |_| {
            let active = crate::core::exec_ctx::current().unwrap();
            check.check_inheritance(active.policy().unwrap()).unwrap();
            assert!(!active.permits_automation());
            polls += 1;
            if polls == 1 {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
    ));
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(worker.as_mut().poll(&mut cx).is_pending());
    assert!(crate::core::exec_ctx::current().is_none());
    assert!(task
        .control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .is_err());
    assert!(worker.as_mut().poll(&mut cx).is_ready());
    drop(worker);
    assert!(crate::core::exec_ctx::current().is_none());
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();

    let normal = ExecutionContext::default().with_task(task.clone());
    let waiting = Box::pin(crate::core::exec_ctx::with_current_future(
        normal.admit_policy().unwrap(),
        std::future::pending::<()>(),
    ));
    assert!(task
        .control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .is_err());
    drop(waiting); // An unpolled/cancelled job also releases its owned admission.
    task.control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .unwrap();
}

#[test]
fn failed_transition_constructs_no_normal_registry_fallback() {
    let task = TaskContext::private();
    assert!(task
        .control()
        .transition(ExecutionMode::Planning, None, |_| anyhow::bail!(
            "disk full"
        ))
        .is_err());
    let ctx = ExecutionContext::default().with_task(task);
    let registry = crate::core::exec_ctx::with_current(ctx, || {
        crate::agent::builtin::default_registry_in(&std::env::temp_dir())
    });
    assert!(registry.names().is_empty());
}
