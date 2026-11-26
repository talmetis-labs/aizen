use super::*;

fn advice_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| is_nudge_of_kind(message, PLAN_NUDGE_PREFIX))
        .count()
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "Serialize clarify pending cell across pause/resume"
)]
async fn plan_nudge_clarify_preserves_budget_with_fresh_configs_and_repeated_questions() {
    let _lock = clarify::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = clarify::take_pending();
    let task = todo::TaskContext::private();
    let config = || {
        let mut config = cfg();
        config.enable_plan_nudge = true;
        config.exec_ctx = config.exec_ctx.with_task(task.clone());
        config
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(clarify::Clarify));
    let mut messages = vec![Message::system("sys"), Message::user("Migrate storage")];
    for (step, expected) in [(0, 1), (1, 0), (2, 0)] {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let chat = |history: Vec<Message>, _: Vec<ToolDef>| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(advice_count(&history), expected);
            std::future::ready(Ok(if step < 2 {
                tool_turn("clarify", r#"{"question":"A or B?","options":["A","B"]}"#)
            } else {
                final_turn("done")
            }))
        };
        let out = run_agent_loop(chat, &config(), &registry, &mut messages)
            .await
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_valid_history(&messages);
        if step < 2 {
            assert!(matches!(out.stop, StopReason::AwaitingInput(_)));
            messages.push(Message::user("B"));
            assert_eq!(clarify_continuation_id(&messages), Some("call_clarify"));
        } else {
            assert_eq!(out.stop, StopReason::Done);
        }
    }
    // A later request starts a fresh budget, even in the same TaskContext and transcript.
    messages.push(Message::user("Migrate another storage format"));
    let out = run_agent_loop(
        |history, _| {
            assert_eq!(advice_count(&history), 1);
            std::future::ready(Ok(final_turn("done")))
        },
        &config(),
        &registry,
        &mut messages,
    )
    .await
    .unwrap();
    assert_eq!(out.stop, StopReason::Done);
    assert!(task.resume_workflow_advice(Some("call_clarify")).is_none());
}

#[test]
fn plan_nudge_clarify_handoff_is_one_shot_and_never_restored_from_task_wire() {
    let task = todo::TaskContext::private();
    let paused = || {
        let advice = plan_nudge::AdviceContext::default();
        assert!(advice
            .begin("Migrate storage", false)
            .take_reminder(&[])
            .is_some());
        task.pause_workflow_advice("ask-1".into(), advice.take());
        // Reusing the original config cannot inherit the detached budget.
        assert!(advice
            .begin("Migrate other storage", false)
            .take_reminder(&[])
            .is_some());
    };
    paused();
    assert!(task.resume_workflow_advice(Some("wrong-id")).is_none());
    assert!(task.resume_workflow_advice(Some("ask-1")).is_none());
    paused();
    let resumed = task.resume_workflow_advice(Some("ask-1")).unwrap();
    assert!(resumed.begin("B", false).take_reminder(&[]).is_none());
    assert!(task.resume_workflow_advice(Some("ask-1")).is_none());
    paused();
    task.clear();
    assert!(task.resume_workflow_advice(Some("ask-1")).is_none());
    paused();
    task.restore(task.snapshot()).unwrap();
    assert!(task.resume_workflow_advice(Some("ask-1")).is_none());
}

#[tokio::test]
async fn plan_nudge_compaction_and_empty_retry_add_no_calls_or_duplicate_advice() {
    for enabled in [false, true] {
        let mut config = cfg();
        config.enable_plan_nudge = enabled;
        config.context_window = 100;
        config.compact_at_pct = 80;
        config.max_transient_retries = 3;
        config.exec_ctx = config.exec_ctx.with_task(todo::TaskContext::private());
        let mut history = vec![
            Message::system("sys"),
            Message::user(format!("older {}", "x".repeat(200))),
            Message::assistant("old answer"),
            Message::user(format!("previous {}", "y".repeat(200))),
            Message::assistant("previous answer"),
            Message::user("Migrate storage"),
        ];
        let summaries = std::sync::atomic::AtomicUsize::new(0);
        let summarize = |_: Vec<Message>| {
            summaries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok("Old context summarized".to_owned()))
        };
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let chat = |messages: Vec<Message>, _: Vec<ToolDef>| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(advice_count(&messages), usize::from(enabled));
            std::future::ready(Ok(if call < 2 {
                empty_turn()
            } else {
                final_turn("done")
            }))
        };
        let out =
            run_agent_loop_compacting(chat, summarize, &config, &ToolRegistry::new(), &mut history)
                .await
                .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(advice_count(&history), usize::from(enabled));
    }
}

#[test]
fn plan_nudge_clarify_pairing_rejects_unrelated_tool_and_intervening_turn() {
    let mut assistant = Message::assistant("");
    assistant.tool_calls = vec![call("ask-1", "clarify", r#"{"question":"A?"}"#)];
    let mut messages = vec![
        Message::user("Migrate storage"),
        assistant,
        Message::tool_result("ask-1", "waiting"),
        Message::user("A"),
    ];
    assert_eq!(clarify_continuation_id(&messages), Some("ask-1"));
    // Host plan metadata may be inserted between the tool result and the user's answer.
    // It is not a new request and cannot sever an otherwise matching in-process pause.
    let dir = crate::core::file_placement::tests::TestDir::new();
    std::fs::create_dir_all(dir.path().join("docs/plans")).unwrap();
    std::fs::write(dir.path().join("docs/plans/current.md"),
        "---\naizen_plan: 1\nid: p-0123456789abcdef0123456789abcdef\n---\n<!-- aizen:phase inspect -->\n"
    ).unwrap();
    let task = todo::TaskContext::private();
    task.attach_plan(
        todo::PlanAttachment::load(&dir.path().canonicalize().unwrap(), "docs/plans/current.md")
            .unwrap(),
    )
    .unwrap();
    prompt_lanes::PlanContextRefresh::default()
        .refresh(0, &task, &mut messages)
        .unwrap();
    assert!(messages.iter().any(prompt_lanes::is_plan_context));
    assert_eq!(clarify_continuation_id(&messages), Some("ask-1"));
    messages.retain(|message| !prompt_lanes::is_plan_context(message));
    messages[1].tool_calls[0].function.name = "echo".into();
    assert!(clarify_continuation_id(&messages).is_none());
    messages[1].tool_calls[0].function.name = "clarify".into();
    messages[2].tool_call_id = Some("wrong-id".into());
    assert!(clarify_continuation_id(&messages).is_none());
    messages[2].tool_call_id = Some("ask-1".into());
    messages.push(Message::assistant("finished"));
    messages.push(Message::user("Migrate something else"));
    assert!(clarify_continuation_id(&messages).is_none());
}
