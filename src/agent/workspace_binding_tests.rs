use super::*;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::test]
async fn stale_planned_calls_refuse_before_repreparation_or_body() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe {
        prepares: Arc<AtomicUsize>,
        bodies: Arc<AtomicUsize>,
        writer: bool,
    }
    impl Tool for Probe {
        fn name(&self) -> &str {
            "binding_probe"
        }
        fn description(&self) -> &str {
            "Track preparation and execution at a binding boundary"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn is_destructive(&self) -> bool {
            self.writer
        }
        fn is_concurrency_safe(&self) -> bool {
            !self.writer
        }
        fn prepare_call(
            &self,
            args: &serde_json::Value,
            _: &crate::core::exec_ctx::ExecutionContext,
        ) -> Result<tools::PreparedToolCall> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(tools::PreparedToolCall::new(args.clone()))
        }
        fn execute(&self, _: &serde_json::Value) -> Result<String> {
            self.bodies.fetch_add(1, Ordering::SeqCst);
            Ok("binding body executed".into())
        }
    }
    for writer in [false, true] {
        let (fixture, original) = placement_executor_fixture();
        let prepares = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(Probe {
            prepares: prepares.clone(),
            bodies: bodies.clone(),
            writer,
        }));
        let calls = vec![ToolCall {
            id: "binding-call".into(),
            kind: "function".into(),
            function: crate::core::types::FunctionCall {
                name: "binding_probe".into(),
                arguments: "{}".into(),
            },
        }];
        let planned = plan_execution_calls(&registry, &calls, &original);
        let child = fixture.path().join("different-root");
        std::fs::create_dir(&child).unwrap();
        let changed = pin_placement_context(&AgentConfig {
            workspace_root: Some(child),
            ..original.clone()
        })
        .unwrap();
        let mut sink = vec![Message::tool_result("binding-call", "")];
        let results = execute_execution_calls(
            &registry,
            &calls,
            &planned,
            true,
            &changed,
            &mut sink,
            Vec::new(),
            &mut None,
            &mut None,
            &[],
        )
        .await;
        assert!(results[0].outcome.failure.is_some(), "writer={writer}");
        assert!(
            results[0].outcome.text.contains("binding is stale"),
            "{}",
            results[0].outcome.text
        );
        assert_eq!(
            prepares.load(Ordering::SeqCst),
            0,
            "stale plans must not bind fresh arguments"
        );
        assert_eq!(bodies.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn run_root_uses_captured_placement_and_refuses_missing_or_file_roots() {
    let (fixture, config) = placement_executor_fixture();
    let root = config.effective_root();
    let captured_only = AgentConfig {
        workspace_root: None,
        ..config.clone()
    };
    assert_eq!(captured_only.effective_root(), root);
    assert_eq!(
        pin_placement_context(&captured_only)
            .unwrap()
            .workspace_root,
        Some(root.clone())
    );

    // A deliberate host-supplied child root can replace the parent's placement, not its cwd.
    let child = fixture.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let child = child.canonicalize().unwrap();
    let explicit_child = AgentConfig {
        workspace_root: Some(child.clone()),
        ..captured_only.clone()
    };
    let pinned = pin_placement_context(&explicit_child).unwrap();
    assert_eq!(pinned.exec_ctx.placement().unwrap().workspace_root(), child);
    assert_eq!(config.exec_ctx.placement().unwrap().workspace_root(), root);

    std::fs::remove_dir(&child).unwrap();
    assert_eq!(explicit_child.effective_root(), child);
    assert!(pin_placement_context(&explicit_child).is_err());
    assert_eq!(pinned.effective_root(), child);
    assert!(pin_placement_context(&pinned).is_err());

    let file = fixture.path().join("not-a-directory");
    std::fs::write(&file, "preserve").unwrap();
    let invalid = AgentConfig {
        workspace_root: Some(file.clone()),
        ..config
    };
    assert!(pin_placement_context(&invalid).is_err());
    assert_eq!(std::fs::read_to_string(file).unwrap(), "preserve");
}

struct BoundRootProbe {
    root: PathBuf,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tool for BoundRootProbe {
    fn name(&self) -> &str {
        "bound_root_probe"
    }
    fn description(&self) -> &str {
        "Read the current host workspace binding for a fixture"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object", "properties":{}, "additionalProperties":false})
    }
    fn execute(&self, _: &serde_json::Value) -> Result<String> {
        assert_bound_root(&self.root);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("bound workspace observed".into())
    }
}

fn assert_bound_root(root: &std::path::Path) {
    let current = crate::core::exec_ctx::current().expect("the loop must bind its context");
    assert_eq!(current.placement().unwrap().workspace_root(), root);
    assert_eq!(crate::core::config::project_root(), root);
    assert_eq!(
        crate::core::config::project_aizen_dir(),
        root.join(".aizen")
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Pin fixture HOME while both child loops run"
)]
async fn run_root_binding_survives_interleaved_model_polls_and_tool_workers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, parent) = placement_executor_fixture();
    let parent_root = parent.effective_root();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", fixture.path().join("home")),
        ("AIZEN_PROJECT_ROOT", parent_root.clone()),
    ]);
    let cwd = std::env::current_dir().unwrap();
    let roots: Vec<_> = ["child-a", "child-b"]
        .into_iter()
        .map(|name| {
            let root = fixture.path().join(name);
            std::fs::create_dir(&root).unwrap();
            root.canonicalize().unwrap()
        })
        .collect();
    let rendezvous = Arc::new(tokio::sync::Barrier::new(2));
    let run = |root: PathBuf| {
        let mut config = cfg();
        config.workspace_root = Some(root.clone());
        config.exec_ctx = config
            .exec_ctx
            .with_placement(parent.exec_ctx.placement().unwrap().clone())
            .with_task(todo::TaskContext::default());
        let rendezvous = rendezvous.clone();
        async move {
            let steps = AtomicUsize::new(0);
            let body_calls = Arc::new(AtomicUsize::new(0));
            let mut registry = ToolRegistry::new();
            registry.register(Box::new(BoundRootProbe {
                root: root.clone(),
                calls: body_calls.clone(),
            }));
            let chat = |_: Vec<Message>, _: Vec<ToolDef>| {
                // Covers the synchronous factory call as well as polls of its returned future.
                assert_bound_root(&root);
                let first = steps.fetch_add(1, Ordering::SeqCst) == 0;
                let root = root.clone();
                let rendezvous = rendezvous.clone();
                async move {
                    assert_bound_root(&root);
                    if first {
                        rendezvous.wait().await;
                    }
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                        assert_bound_root(&root);
                    }
                    Ok(if first {
                        tool_turn("bound_root_probe", "{}")
                    } else {
                        final_turn("observed")
                    })
                }
            };
            let result = run_agent(
                chat,
                &config,
                &registry,
                "fixture",
                "Inspect workspace binding",
            )
            .await
            .unwrap();
            assert_eq!(result.stop, StopReason::Done);
            assert_eq!(steps.load(Ordering::SeqCst), 2);
            assert_eq!(body_calls.load(Ordering::SeqCst), 1);
            assert!(
                crate::core::exec_ctx::current().is_none(),
                "a completed poll must restore the caller"
            );
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(run(roots[0].clone()), run(roots[1].clone()));
    })
    .await
    .expect("bounded interleaved loop fixture");
    assert!(crate::core::exec_ctx::current().is_none());
    assert_eq!(crate::core::config::project_root(), parent_root);
    assert_eq!(
        parent.exec_ctx.placement().unwrap().workspace_root(),
        parent_root
    );
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Pin startup project override across the model loop"
)]
async fn run_root_startup_keeps_project_override_without_rebasing_tool_cwd() {
    use crate::core::{config, exec_ctx};
    let _home = config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let project = fixture.path().join("startup-project");
    let child = fixture.path().join("other-worktree");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&child).unwrap();
    let _env = config::EnvGuard::set([
        ("AIZEN_PROJECT_ROOT", project.clone()),
        ("AIZEN_HOME", fixture.path().join("home")),
    ]);
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let original_key = config::project_key();
    let config = cfg();
    assert!(config.workspace_root.is_none());
    assert!(config.exec_ctx.placement().is_none());
    let registry = ToolRegistry::new();
    let outcome = run_agent(
        |_, _| {
            let expected = project.clone();
            let expected_cwd = cwd.clone();
            let key = original_key.clone();
            async move {
                tokio::task::yield_now().await;
                assert_eq!(config::project_root(), expected);
                assert_eq!(config::project_key(), key);
                let context = exec_ctx::current().unwrap();
                let placement = context.placement().unwrap();
                assert_eq!(placement.workspace_root(), expected_cwd);
                assert_eq!(placement.project_root(), expected);
                Ok(final_turn("observed"))
            }
        },
        &config,
        &registry,
        "fixture",
        "Inspect startup binding",
    )
    .await
    .unwrap();
    assert_eq!(outcome.stop, StopReason::Done);
    let pinned = pin_placement_context(&config).unwrap();
    let inherited = pin_placement_context(&pinned).unwrap();
    assert_eq!(
        inherited.exec_ctx.placement().unwrap().project_root(),
        project
    );
    let moved = pin_placement_context(&AgentConfig {
        workspace_root: Some(child.clone()),
        ..pinned
    })
    .unwrap();
    exec_ctx::with_current(moved.exec_ctx.clone(), || {
        assert_eq!(config::project_root(), child.canonicalize().unwrap());
        assert_ne!(config::project_key(), original_key);
    });
    assert_eq!(config::project_root(), project);
    assert_eq!(
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        cwd
    );
}

#[tokio::test]
async fn run_root_missing_workspace_refuses_before_calling_model() {
    let (fixture, mut config) = placement_executor_fixture();
    let missing = fixture.path().join("missing-worktree");
    config.workspace_root = Some(missing.clone());
    let registry = ToolRegistry::new();
    let result = run_agent(
        |_, _| {
            panic!("missing bound workspace must not reach the model");
            #[allow(unreachable_code)]
            std::future::ready(Ok(final_turn("must not run")))
        },
        &config,
        &registry,
        "fixture",
        "Inspect workspace",
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("bound workspace is unavailable"));
    assert!(!missing.exists());
    assert!(crate::core::exec_ctx::current().is_none());
}
