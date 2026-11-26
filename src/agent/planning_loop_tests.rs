use super::*;
use crate::core::session_control::{ExecutionMode, PlanTarget};

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Isolate HOME for native plan writes and session bookkeeping"
)]
async fn planning_loop_saves_exact_target_and_refreshes_lane_without_implementation_checks() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, mut config) = placement_executor_fixture();
    let root = config.effective_root().canonicalize().unwrap();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path().join("home"))]);
    std::fs::write(root.join("source.txt"), "native exploration").unwrap();
    let task = todo::TaskContext::private();
    let id = "p-0123456789abcdef0123456789abcdef";
    let path = "docs/plans/loop.md";
    task.control()
        .transition(
            ExecutionMode::Planning,
            Some(PlanTarget {
                root: root.to_string_lossy().into_owned(),
                path: path.into(),
                id: id.into(),
            }),
            |_| Ok(()),
        )
        .unwrap();
    config.exec_ctx = config.exec_ctx.with_task(task.clone());
    let registry = crate::core::exec_ctx::with_current(config.exec_ctx.clone(), || {
        builtin::default_registry_in(&root)
    });
    let document = format!("---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase inspect -->\n# Investigate\nOnly a plan.\n");
    let mut round = 0;
    let turns = Mutex::new(VecDeque::from([
        tool_turn("file_read", r#"{"path":"source.txt"}"#),
        tool_turn(
            "file_write",
            &serde_json::json!({"path":path,"content":document,"purpose":"project"}).to_string(),
        ),
        tool_turn(
            "file_write",
            r#"{"path":"implementation.rs","content":"forbidden"}"#,
        ),
        final_turn("Plan saved; implementation was not performed."),
    ]));
    let round = Mutex::new(&mut round);
    let chat = |messages: Vec<Message>, definitions: Vec<ToolDef>| {
        assert_eq!(
            definitions.len(),
            5,
            "restricted registry excludes deferred/process tools"
        );
        let lane = messages
            .iter()
            .filter(|message| prompt_lanes::is_plan_context(message))
            .collect::<Vec<_>>();
        assert_eq!(lane.len(), 1);
        let text = lane[0].content.as_deref().unwrap();
        assert!(text.contains(id) && text.contains(path), "{text}");
        let mut round = round.lock().unwrap();
        if **round >= 2 {
            assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), document);
            assert!(task.snapshot().plan.as_ref().unwrap().usable());
            assert!(
                text.contains("Only a plan."),
                "saved plan excerpt must enter the next model round"
            );
        }
        if **round == 3 {
            assert!(messages.iter().any(|message| message.role == "tool"
                && message
                    .content
                    .as_deref()
                    .is_some_and(|text| text.starts_with("error:"))));
        }
        **round += 1;
        std::future::ready(Ok(turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("no extra mandatory model call")))
    };
    let mut history = vec![
        Message::system("fixture"),
        Message::user("Plan only: investigate source.txt"),
    ];
    let outcome = run_agent_loop(chat, &config, &registry, &mut history)
        .await
        .unwrap();
    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.iters, 4);
    assert!(!root.join("implementation.rs").exists());
    assert!(task.snapshot().obligations.is_empty());
    assert!(turns.lock().unwrap().is_empty());
    assert_valid_history(&history);
    task.control()
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
    assert!(
        config.exec_ctx.is_planning(),
        "human exit cannot widen a frozen run"
    );
    assert!(config.exec_ctx.admit_policy().is_err());
    let fresh = crate::core::exec_ctx::ExecutionContext::default().with_task(task);
    let normal =
        crate::core::exec_ctx::with_current(fresh, || builtin::file_registry_for_test(&root));
    assert!(normal.get("file_edit").is_some());
}

#[tokio::test]
#[expect(clippy::await_holding_lock, reason = "Isolate HOME for native writes")]
async fn normal_architect_draft_attaches_only_after_exact_native_save() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, mut config) = placement_executor_fixture();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path().join("home"))]);
    let root = config.effective_root();
    std::fs::create_dir_all(root.join("docs/plans")).unwrap();
    let path = "docs/plans/architect.md";
    let id = "p-0123456789abcdef0123456789abcdef";
    let document =
        format!("---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase inspect -->\n# Draft\n");
    let task = todo::TaskContext::private();
    config.exec_ctx = config.exec_ctx.with_task(task.clone());
    config.architect_document = Some((path.into(), id.into()));
    let registry = builtin::file_registry_for_test(&root);
    let turns = Mutex::new(VecDeque::from([
        tool_turn(
            "file_write",
            &serde_json::json!({"path":path,"content":document,"purpose":"project"}).to_string(),
        ),
        final_turn("Plan saved; implementation checks remain outstanding."),
    ]));
    let round = std::cell::Cell::new(0);
    let chat = |_: Vec<Message>, _: Vec<ToolDef>| {
        if round.get() == 0 {
            assert!(task.snapshot().plan.is_none(), "draft is not saved");
        } else {
            assert!(task.snapshot().plan.as_ref().unwrap().usable());
            assert_eq!(task.snapshot().plan.as_ref().unwrap().id, id);
        }
        round.set(round.get() + 1);
        std::future::ready(Ok(turns.lock().unwrap().pop_front().unwrap()))
    };
    let mut history = vec![Message::user("Save the architect plan")];
    run_agent_loop(chat, &config, &registry, &mut history)
        .await
        .unwrap();
    assert_eq!(round.get(), 2);
    assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), document);
    assert!(
        task.snapshot().checks.is_empty(),
        "attachment is not Check evidence"
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Isolate HOME for planning bookkeeping"
)]
async fn planning_loop_refreshes_external_edit_and_keeps_removed_phase_unfinished() {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, mut config) = placement_executor_fixture();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", fixture.path().join("home"))]);
    let root = config.effective_root().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("docs/plans")).unwrap();
    std::fs::write(root.join("source.txt"), "read").unwrap();
    let path = "docs/plans/external.md";
    let id = "p-0123456789abcdef0123456789abcdef";
    let original =
        format!("---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase inspect -->\nBefore edit\n");
    let edited = format!(
        "---\naizen_plan: 1\nid: {id}\n---\n<!-- aizen:phase different -->\nAfter human edit\n"
    );
    std::fs::write(root.join(path), original).unwrap();
    let task = todo::TaskContext::private();
    task.replace_with_plan(
        vec![todo::Todo::new("report", todo::Status::Done)],
        vec![("@index:0".into(), todo::Requirement::Report)],
        Some(todo::PlanAttachment::load(&root, path).unwrap()),
        vec![("@index:0".into(), "inspect".into())],
    )
    .unwrap();
    task.control()
        .transition(
            ExecutionMode::Planning,
            Some(PlanTarget {
                root: root.to_string_lossy().into_owned(),
                path: path.into(),
                id: id.into(),
            }),
            |_| Ok(()),
        )
        .unwrap();
    config.exec_ctx = config.exec_ctx.with_task(task.clone());
    config.enable_todo_poke = false;
    config.enable_confidence_gate = false;
    let registry = crate::core::exec_ctx::with_current(config.exec_ctx.clone(), || {
        builtin::default_registry_in(&root)
    });
    let round = std::cell::Cell::new(0);
    let chat = |messages: Vec<Message>, _: Vec<ToolDef>| {
        let lane = messages
            .iter()
            .find(|m| prompt_lanes::is_plan_context(m))
            .unwrap()
            .content
            .as_deref()
            .unwrap();
        let turn = if round.get() == 0 {
            assert!(lane.contains("Before edit"));
            // Simulate an independent editor while the model request is in flight.
            std::fs::write(root.join(path), &edited).unwrap();
            tool_turn("file_read", r#"{"path":"source.txt"}"#)
        } else {
            assert!(lane.contains("After human edit") && !lane.contains("Before edit"));
            final_turn("The linked phase was removed; work remains unfinished.")
        };
        round.set(round.get() + 1);
        assert!(round.get() <= 2, "no mandatory extra planning call");
        std::future::ready(Ok(turn))
    };
    let mut history = vec![Message::user("Inspect only")];
    let result = run_agent_loop(chat, &config, &registry, &mut history)
        .await
        .unwrap();
    assert_eq!(result.stop, StopReason::Incomplete);
    assert!(!task.is_complete());
    assert_eq!(task.snapshot().item_phases[0].phase_id, "inspect");
    assert_eq!(task.snapshot().plan.unwrap().phases, vec!["different"]);
    assert_valid_history(&history);
}
