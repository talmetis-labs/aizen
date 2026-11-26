use super::*;
use crate::core::file_placement::{tests::TestDir, PlacementContext};
use crate::core::workspace_txn::TestLockNamespace;
use crate::core::{
    cancel::{self, TurnCancel},
    exec_ctx::{self, ExecutionContext},
};
use anyhow::Context;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fixture(body: impl FnOnce(&ExecutionContext, &TurnCancel) -> Result<()>) -> Result<()> {
    fixture_with_policy(true, body)
}
fn fixture_with_policy(
    admit_policy: bool,
    body: impl FnOnce(&ExecutionContext, &TurnCancel) -> Result<()>,
) -> Result<()> {
    let _runner = crate::sandbox::runner::tests::lock();
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct Mode(crate::sandbox::SandboxMode);
    impl Drop for Mode {
        fn drop(&mut self) {
            crate::sandbox::set_mode(self.0);
        }
    }
    let _mode = Mode(crate::sandbox::mode());
    crate::sandbox::set_mode(crate::sandbox::SandboxMode::Guarded);
    let temp = TestDir::new();
    let base = temp.path();
    let root = base.join("workspace");
    let home = base.join("home");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    std::fs::write(root.join("fixture.txt"), b"frozen reader fixture\n")?;
    let empty_git_config = home.join("empty.gitconfig");
    std::fs::write(&empty_git_config, b"")?;
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()),
        ("HOME", home.as_os_str()),
        ("USERPROFILE", home.as_os_str()),
        ("GIT_CONFIG_GLOBAL", empty_git_config.as_os_str()),
        ("GIT_CONFIG_NOSYSTEM", std::ffi::OsStr::new("1")),
    ]);
    let parent = ExecutionContext::default()
        .with_resource_scope("workflow-tool-origin")
        .with_placement(PlacementContext::new(root, home)?)
        .with_test_lock_namespace(TestLockNamespace::fixture(base));
    // Match the actual driver's task binding and policy admission before dispatch.
    // Resource scope creates the private task, but does not admit its policy.
    let actual_task = parent.task().unwrap().clone();
    assert!(parent.policy().is_none());
    let parent = if admit_policy {
        let admitted = parent.with_task(actual_task.clone()).admit_policy()?;
        assert!(admitted.task().unwrap().same_owner(&actual_task));
        admitted
            .policy()
            .unwrap()
            .check_inheritance(&actual_task.control().policy())?;
        admitted
    } else {
        parent
    };
    let token = TurnCancel::new();
    crate::core::workspace_txn::with_test_lock_home(base, || -> Result<()> {
        // Save through the actual public configuration API in this private home.
        // No private ResolvedSynthesis fields or classification flags are fabricated.
        crate::core::cli_config::save(&crate::core::cli_config::CliConfig {
            response_visuals: Some(crate::core::cli_config::ResponseVisuals::Off),
            ..Default::default()
        })?;
        anyhow::ensure!(
            crate::core::cli_config::load().response_visuals()
                == crate::core::cli_config::ResponseVisuals::Off,
            "fixture configuration did not freeze text-only responses"
        );
        body(&parent, &token)?;
        // Read the latch under the same test namespace, before restoring controls.
        crate::core::proctree::check_bounded_effects()
            .context("workflow fixture left unsettled process effects")
    })
}
fn enter<T>(ctx: &ExecutionContext, token: &TurnCancel, body: impl FnOnce() -> T) -> T {
    cancel::with_current(token.clone(), || exec_ctx::with_current(ctx.clone(), body))
}
fn adaptive_observer_call(
    parent: &ExecutionContext,
    token: &TurnCancel,
) -> Result<PreparedToolCall> {
    let root = parent
        .placement()
        .context("actual fixture placement")?
        .workspace_root()
        .to_path_buf();
    let tool = WorkflowTool::new(
        reqwest::Client::builder().no_proxy().build()?,
        "http://127.0.0.1:9/v1".into(),
        "fixture-only".into(),
        "fixture-origin-model".into(),
        crate::core::approval::ApprovalMode::Ask,
        0,
        root,
        32_768,
    );
    enter(parent, token, || {
        tool.prepare_call(&serde_json::json!({
        "mode":"fanout", "synthesis":"Summarize both reports",
        "tasks":[
            {"id":"first","role":"nemesis","prompt":"Read fixture.txt","read_paths":["fixture.txt"],"max_steps":1},
            {"id":"second","role":"nemesis","prompt":"Review fixture.txt","read_paths":["fixture.txt"],"max_steps":80,"after":["first"]}
        ]
    }),parent)
    })
}

#[test]
fn adaptive_workflow_observer_covers_frozen_dependency_workload_and_parent() -> Result<()> {
    fixture(|parent, token| {
        let _time = crate::core::config::EnvGuard::set([
            ("AIZEN_SUBAGENT_WALL_SECS", ""),
            ("AIZEN_SUBAGENT_CALL_SECS", "60"),
        ]);
        let mut call = adaptive_observer_call(parent, token)?;
        let frozen = call
            .delegation
            .as_ref()
            .unwrap()
            .downcast_ref::<PreparedWorkflowDispatch>()
            .unwrap();
        assert!(
            frozen.synthesize && frozen.read_only && workflow_has_no_outward_effects(&frozen.spec)
        );
        call.args["tasks"] = serde_json::json!([]);
        let before = Instant::now();
        let until =
            super::super::prepared_tool_deadline(&crate::agent::AgentConfig::default(), &call)
                .into_std();
        let budget = Duration::from_secs(6720);
        assert!((before + budget..=Instant::now() + budget).contains(&until));
        let cap = Instant::now() + Duration::from_secs(15);
        let cfg = crate::agent::AgentConfig {
            deadline: Some(cap),
            ..Default::default()
        };
        assert_eq!(
            super::super::prepared_tool_deadline(&cfg, &call).into_std(),
            cap
        );
        let parent = parent.with_deadline(Some(cap));
        let capped = adaptive_observer_call(&parent, token)?;
        assert_eq!(prepared_wall_deadline(&capped), Some(cap));
        Ok(())
    })
}

#[test]
fn adaptive_workflow_observer_keeps_fixed_override_and_opt_out() -> Result<()> {
    fixture(|parent, token| {
        let _time = crate::core::config::EnvGuard::set([
            ("AIZEN_SUBAGENT_WALL_SECS", "3600"),
            ("AIZEN_SUBAGENT_CALL_SECS", "60"),
        ]);
        let call = adaptive_observer_call(parent, token)?;
        let before = Instant::now();
        let until = prepared_wall_deadline(&call).unwrap();
        let budget = Duration::from_secs(7260);
        assert!((before + budget..=Instant::now() + budget).contains(&until));
        let _zero = crate::core::config::EnvGuard::set([("AIZEN_SUBAGENT_WALL_SECS", "0")]);
        assert!(
            prepared_wall_deadline(&call).is_none(),
            "zero retains the independent generic observer policy"
        );
        Ok(())
    })
}

#[test]
fn adaptive_workflow_observer_refuses_unprepared_or_forged_workload() -> Result<()> {
    fixture(|parent, token| {
        let _time = crate::core::config::EnvGuard::set([("AIZEN_SUBAGENT_WALL_SECS", "")]);
        let call = adaptive_observer_call(parent, token)?;
        let mut frozen = call
            .delegation
            .as_ref()
            .unwrap()
            .downcast_ref::<PreparedWorkflowDispatch>()
            .unwrap()
            .clone();
        frozen.spec.tasks[0].prepared = None;
        let mut altered = PreparedToolCall::new(call.args.clone());
        altered.delegation = Some(Arc::new(frozen));
        assert!(prepared_wall_deadline(&altered).is_none());
        altered.delegation = Some(Arc::new(call.args.clone()));
        assert!(prepared_wall_deadline(&altered).is_none());
        altered.delegation = None;
        assert!(prepared_wall_deadline(&altered).is_none());
        Ok(())
    })
}
