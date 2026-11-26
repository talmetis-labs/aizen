use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn task(id: &str, role: &str, after: &[&str]) -> WorkflowTask {
    let kind = if super::super::task_is_writer(role, None) {
        "opaque"
    } else {
        "read"
    };
    let mut task =
        super::scoped_admission_tests::frozen_task(std::path::Path::new("."), id, kind, &[]);
    task.role = role.into();
    task.prompt = format!("Complete the scoped task {id} without changing its authorization.");
    task.after = after.iter().map(|id| id.to_string()).collect();
    task
}

fn outcome(intent: &DispatchIntent, status: &str, summary: &str) -> TaskOutcome {
    TaskOutcome {
        id: intent.key.display(),
        attempt: None,
        selected_for: None,
        role: intent.task.role.clone(),
        model: intent.task.model.clone().unwrap_or_else(|| "m".into()),
        status: status.into(),
        summary: summary.into(),
        iters: 1,
        tokens_in: 11,
        tokens_out: 3,
        evidence_images: Vec::new(),
        git_handoff: None,
    }
}

fn policy(metis: bool) -> Policy {
    Policy {
        metis,
        parent_deadline: None,
        admission_timeout: Duration::from_millis(100),
        restart_backoff: Duration::ZERO,
        probe_interval: Duration::from_millis(2),
    }
}

fn dispatch(
    state: &mut State<'_>,
    id: &str,
    kind: AttemptKind,
    events: &mut Vec<Event>,
) -> DispatchIntent {
    let pos = state
        .queue
        .iter()
        .position(|p| p.intent.key.task_id.0 == id && p.intent.key.kind == kind)
        .unwrap();
    let pending = state.queue.remove(pos).unwrap();
    assert!(state.eligible(&pending));
    let mut intent = pending.intent;
    state.materialize(&mut intent).unwrap();
    state.freeze_deadline(&mut intent);
    events.push(Event::DispatchIntent {
        seq: 0,
        intent: intent.clone(),
    });
    intent
}

fn complete(
    state: &mut State<'_>,
    intent: DispatchIntent,
    status: &str,
    summary: &str,
    events: &mut Vec<Event>,
) {
    let result = outcome(&intent, status, summary);
    events.push(completion_event(&intent, &result));
    state.complete(intent, result).unwrap();
}

fn settle(state: &mut State<'_>, events: &mut Vec<Event>) {
    while let Some((goal, key)) = state.settlements.front().cloned() {
        events.push(Event::TaskSettled {
            seq: 0,
            task_id: goal.clone(),
            selected: key.clone(),
        });
        state.settle(goal, key).unwrap();
    }
}

#[tokio::test]
async fn terminal_transport_statuses_never_restart_or_replan_after_resume() {
    for status in [
        "deadline",
        "retry-exhausted",
        "protocol-error",
        "authentication-error",
        "permanent-error",
        "cancelled",
    ] {
        let tasks = vec![task("a", "argus", &[])];
        let mut state = State::new(&tasks, policy(true));
        let mut events = Vec::new();
        let intent = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
        complete(
            &mut state,
            intent,
            status,
            "terminal provider outcome",
            &mut events,
        );
        assert!(
            state.queue.is_empty(),
            "{status} must not grant recovery authority"
        );
        let events: Vec<Event> =
            serde_json::from_str(&serde_json::to_string(&events).unwrap()).unwrap();
        let results = schedule(
            &tasks,
            1,
            "m",
            &TurnCancel::new(),
            policy(true),
            &events,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
            |intent, ()| async move {
                panic!("terminal outcome dispatched again: {:?}", intent.key);
            },
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, status);
        assert_eq!(results[0].selected_for, Some(TaskId("a".into())));
    }
}

#[tokio::test]
async fn expired_recovery_does_not_enter_admission_or_buy_new_time() {
    let tasks = vec![task("a", "argus", &[])];
    let mut state = State::new(&tasks, policy(true));
    let mut events = Vec::new();
    let intent = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
    complete(
        &mut state,
        intent,
        "error",
        "transient before interruption",
        &mut events,
    );
    if let Event::DispatchIntent { intent, .. } = &mut events[0] {
        intent.deadline_unix_ms = Some(1);
        intent.live_deadline = None;
    }
    let admitted = Cell::new(0);
    let results = schedule(
        &tasks,
        1,
        "m",
        &TurnCancel::new(),
        policy(true),
        &events,
        |_| Ok(()),
        |_| {
            admitted.set(admitted.get() + 1);
            Ok(())
        },
        |_| Ok(()),
        |intent, ()| async move { panic!("expired recovery ran: {:?}", intent.key) },
    )
    .await
    .unwrap();
    assert_eq!(admitted.get(), 0);
    let statuses: Vec<_> = results
        .iter()
        .map(|r| (r.id.clone(), r.status.clone()))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("a".to_string(), "error".to_string()),
            ("a [Restart #2]".to_string(), "deadline".to_string()),
        ],
        "{statuses:?}"
    );
    assert_eq!(super::super::logical_outcomes(&results).len(), 1);
}

#[test]
fn recovery_deadline_is_frozen_and_legacy_pending_recovery_is_refused() {
    let tasks = vec![task("a", "argus", &[])];
    let parent = std::time::Instant::now() + Duration::from_secs(30);
    let policy = Policy {
        parent_deadline: Some(parent),
        ..policy(true)
    };
    let mut state = State::new(&tasks, policy);
    let mut events = Vec::new();
    let first = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
    let wall = first.deadline_unix_ms;
    assert_eq!(first.deadline(), Some(parent));
    complete(&mut state, first, "error", "transient", &mut events);
    let restart = dispatch(&mut state, "a", AttemptKind::Restart, &mut events);
    assert_eq!(restart.deadline_unix_ms, wall);
    assert_eq!(restart.deadline(), Some(parent));
    complete(&mut state, restart, "error", "transient", &mut events);
    let planner = dispatch(&mut state, "a", AttemptKind::Metis, &mut events);
    assert_eq!(planner.deadline_unix_ms, wall);
    assert_eq!(planner.deadline(), Some(parent));
    complete(
        &mut state,
        planner,
        "done",
        "replacement brief",
        &mut events,
    );
    let replacement = state.queue.front().unwrap();
    assert_eq!(replacement.intent.deadline_unix_ms, wall);
    assert_eq!(replacement.intent.deadline(), Some(parent));

    let mut legacy = events[..2].to_vec();
    if let Event::DispatchIntent { intent, .. } = &mut legacy[0] {
        intent.deadline_unix_ms = None;
        intent.live_deadline = None;
    }
    let error = State::new(&tasks, policy).replay(&legacy).unwrap_err();
    assert!(error.to_string().contains("no durable execution deadline"));
    if let Event::AttemptCompleted { status, .. } = &mut legacy[1] {
        *status = "done".into();
    }
    State::new(&tasks, policy).replay(&legacy).unwrap();
}

#[tokio::test]
async fn literal_suffix_ids_and_completed_prefix_survive_repeated_resume() {
    let tasks = vec![
        task("a", "argus", &[]),
        task("a#2", "argus", &["a"]),
        task("a#restart", "argus", &["a#2"]),
    ];
    let mut state = State::new(&tasks, policy(false));
    let mut events = Vec::new();
    let a = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
    complete(&mut state, a, "done", "literal a only", &mut events);
    settle(&mut state, &mut events);
    // Each interruption resumes from the last durable logical transition, with no paid alias.
    for expected in ["a#2", "a#restart"] {
        let mut resumed = State::new(&tasks, policy(false));
        resumed.replay(&events).unwrap();
        assert_eq!(
            resumed.results.iter().map(|r| r.tokens_in).sum::<u64>(),
            if expected == "a#2" { 11 } else { 22 }
        );
        let next = dispatch(&mut resumed, expected, AttemptKind::Initial, &mut events);
        let dependency = if expected == "a#2" { "a" } else { "a#2" };
        assert_eq!(next.inputs[0].task_id.0, dependency);
        assert!(next.task.prompt.contains(&format!("=== {dependency} (")));
        complete(&mut resumed, next, "done", expected, &mut events);
        settle(&mut resumed, &mut events);
    }
    let ran = RefCell::new(Vec::new());
    let results = schedule(
        &tasks,
        3,
        "m",
        &TurnCancel::new(),
        policy(false),
        &events,
        |_| Ok(()),
        |_| Ok(()),
        |_| Ok(()),
        |intent, ()| {
            ran.borrow_mut().push(intent.key.clone());
            async move { outcome(&intent, "done", "unexpected rerun") }
        },
    )
    .await
    .unwrap();
    assert!(ran.borrow().is_empty());
    assert_eq!(super::super::logical_outcomes(&results).len(), 3);
    assert_eq!(results.iter().map(|r| r.tokens_in).sum::<u64>(), 33);
}

#[test]
fn dependencies_read_settled_results_not_unpublished_failed_fixes() {
    let mut verifier = task("verify", "themis", &["a"]);
    verifier.retry_on_fail = Some("a".into());
    let tasks = vec![
        task("a", "daedalus", &[]),
        verifier,
        task("slow", "argus", &[]),
        task("downstream", "argus", &["a", "slow"]),
    ];
    let mut state = State::new(&tasks, policy(false));
    let mut events = Vec::new();
    let a = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
    let selected = a.key.clone();
    complete(&mut state, a, "done", "settled implementation", &mut events);
    settle(&mut state, &mut events);
    let verifier = dispatch(&mut state, "verify", AttemptKind::Initial, &mut events);
    complete(
        &mut state,
        verifier,
        "done",
        "VERDICT: FAIL\nfix the implementation",
        &mut events,
    );
    let fix = dispatch(&mut state, "a", AttemptKind::Fix, &mut events);
    complete(
        &mut state,
        fix,
        "error",
        "unpublished failed fix",
        &mut events,
    );
    let slow = dispatch(&mut state, "slow", AttemptKind::Initial, &mut events);
    complete(&mut state, slow, "done", "slow evidence", &mut events);
    settle(&mut state, &mut events);
    let downstream = dispatch(&mut state, "downstream", AttemptKind::Initial, &mut events);
    assert_eq!(downstream.inputs[0], selected);
    assert!(downstream.task.prompt.contains("settled implementation"));
    assert!(!downstream.task.prompt.contains("unpublished failed fix"));
    // The admitted downstream is unknown until its completion is durable; refuse blind replay.
    let mut resumed = State::new(&tasks, policy(false));
    assert!(resumed
        .replay(&events)
        .unwrap_err()
        .to_string()
        .contains("unknown outcome"));
}

#[tokio::test]
async fn fix_completion_resumes_only_recheck_and_preserves_selection_spend() {
    let mut verifier = task("verify", "themis", &["implement"]);
    verifier.retry_on_fail = Some("implement".into());
    let tasks = vec![
        task("implement", "daedalus", &[]),
        verifier,
        task("review", "nemesis", &["verify"]),
    ];
    let mut state = State::new(&tasks, policy(false));
    let mut events = Vec::new();
    let initial = dispatch(&mut state, "implement", AttemptKind::Initial, &mut events);
    complete(&mut state, initial, "done", "initial", &mut events);
    settle(&mut state, &mut events);
    let verify = dispatch(&mut state, "verify", AttemptKind::Initial, &mut events);
    complete(
        &mut state,
        verify,
        "done",
        "VERDICT: FAIL\nrepair once",
        &mut events,
    );
    let fix = dispatch(&mut state, "implement", AttemptKind::Fix, &mut events);
    complete(&mut state, fix, "done", "fixed implementation", &mut events);
    // Crash after completion, before settlement: the fix is never rerun.
    let ran = RefCell::new(Vec::new());
    let results = schedule(
        &tasks,
        2,
        "m",
        &TurnCancel::new(),
        policy(false),
        &events,
        |_| Ok(()),
        |_| Ok(()),
        |_| Ok(()),
        |intent, ()| {
            ran.borrow_mut().push(intent.key.clone());
            assert!(intent.task.prompt.contains("fixed implementation"));
            async move { outcome(&intent, "done", "VERDICT: PASS\nfixed implementation") }
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ran.borrow().iter().map(|key| key.kind).collect::<Vec<_>>(),
        [AttemptKind::Recheck, AttemptKind::Initial]
    );
    assert_eq!(results.len(), 5);
    assert_eq!(results.iter().map(|r| r.tokens_in).sum::<u64>(), 55);
    let selected = super::super::logical_outcomes(&results);
    assert_eq!(selected.len(), 3);
    assert!(selected
        .iter()
        .any(|r| r.attempt.as_ref().unwrap().kind == AttemptKind::Fix));
    assert!(!selected
        .iter()
        .any(|r| r.summary.starts_with("VERDICT: FAIL")));
}

#[tokio::test]
async fn metis_completion_runs_exactly_one_original_contract_replacement() {
    let mut original = task("a#restart", "daedalus", &[]);
    original.model = Some("worker-model".into());
    original.boundaries = Some("Only edit src/parser.rs".into());
    original.expected_output = Some("Scoped evidence".into());
    original.expects = Some(serde_json::json!({"type": "object"}));
    original.max_steps = Some(7);
    let tasks = vec![original.clone()];
    let mut state = State::new(&tasks, policy(true));
    let mut events = Vec::new();
    for kind in [AttemptKind::Initial, AttemptKind::Restart] {
        let intent = dispatch(&mut state, &original.id, kind, &mut events);
        complete(&mut state, intent, "error", "provider failure", &mut events);
    }
    let metis = dispatch(&mut state, &original.id, AttemptKind::Metis, &mut events);
    assert_eq!(metis.task.role, "metis");
    assert!(
        metis.task.model.is_none(),
        "planner must honor Metis routing, not the failed model"
    );
    complete(
        &mut state,
        metis,
        "done",
        "Use a different approach within src/parser.rs",
        &mut events,
    );
    assert!(
        super::super::logical_outcomes(&state.results).is_empty(),
        "planner success cannot settle the goal"
    );
    let ran = Cell::new(0);
    let results = schedule(
        &tasks,
        1,
        "m",
        &TurnCancel::new(),
        policy(true),
        &events,
        |_| Ok(()),
        |_| Ok(()),
        |_| Ok(()),
        |intent, ()| {
            ran.set(ran.get() + 1);
            assert_eq!(intent.key.kind, AttemptKind::Replacement);
            assert_eq!(intent.task.role, original.role);
            assert_eq!(intent.task.model, original.model);
            assert_eq!(intent.task.boundaries, original.boundaries);
            assert_eq!(intent.task.expected_output, original.expected_output);
            assert_eq!(intent.task.expects, original.expects);
            assert_eq!(intent.task.max_steps, original.max_steps);
            assert_eq!(
                intent.task.prompt,
                "Use a different approach within src/parser.rs"
            );
            async move { outcome(&intent, "error", "replacement failed") }
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ran.get(),
        1,
        "replacement must not silently restart or replan again"
    );
    assert_eq!(results.len(), 4);
    assert_eq!(results.iter().map(|r| r.tokens_in).sum::<u64>(), 44);
    assert_eq!(super::super::logical_failures(&results), 1);
    assert_eq!(super::super::logical_outcomes(&results)[0].status, "error");
}

#[test]
fn replay_refuses_unknown_intent_and_modified_prompt_inputs_or_trigger() {
    let tasks = vec![task("a", "argus", &[])];
    let mut state = State::new(&tasks, policy(false));
    let mut events = Vec::new();
    let intent = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
    assert!(State::new(&tasks, policy(false))
        .replay(&events)
        .unwrap_err()
        .to_string()
        .contains("unknown outcome"));
    complete(&mut state, intent, "done", "report", &mut events);
    for field in ["prompt", "inputs", "trigger"] {
        let mut changed = events.clone();
        if let Event::DispatchIntent { intent, .. } = &mut changed[0] {
            match field {
                "prompt" => intent.task.prompt.push_str(" expanded authorization"),
                "inputs" => intent.inputs.push(intent.key.clone()),
                _ => intent.trigger = Some(intent.key.clone()),
            }
        }
        assert!(
            State::new(&tasks, policy(false))
                .replay(&changed)
                .unwrap_err()
                .to_string()
                .contains("payload/transition mismatch"),
            "{field}"
        );
    }
    State::new(&tasks, policy(false)).replay(&events).unwrap();
}

#[test]
fn replay_refuses_duplicate_or_out_of_order_durable_transitions() {
    let tasks = vec![task("a", "argus", &[])];
    let mut state = State::new(&tasks, policy(false));
    let mut events = Vec::new();
    let intent = dispatch(&mut state, "a", AttemptKind::Initial, &mut events);
    complete(&mut state, intent, "done", "settled report", &mut events);
    settle(&mut state, &mut events);
    State::new(&tasks, policy(false)).replay(&events).unwrap();
    for changed in [
        vec![events[0].clone(), events[0].clone()],
        vec![events[1].clone()],
        vec![events[0].clone(), events[1].clone(), events[1].clone()],
        vec![events[0].clone(), events[2].clone(), events[1].clone()],
        vec![
            events[0].clone(),
            events[1].clone(),
            events[2].clone(),
            events[2].clone(),
        ],
    ] {
        assert!(
            State::new(&tasks, policy(false)).replay(&changed).is_err(),
            "malformed events cannot authorize replay: {changed:?}"
        );
    }
}

#[tokio::test]
async fn refresh_settles_incomplete_without_restart_replan_or_successful_resume_skip() {
    let tasks = vec![task("a", "argus", &[])];
    let events = RefCell::new(vec![Event::WorkflowStarted {
        seq: 0,
        version: crate::agent::runlog::WORKFLOW_VERSION,
        fingerprint: 1,
        dispatch_date: Some("2026-10-12".into()),
        task_ids: vec![TaskId("a".into())],
        metis: true,
    }]);
    let ran = Cell::new(0);
    let results = schedule(
        &tasks,
        1,
        "m",
        &TurnCancel::new(),
        policy(true),
        &[],
        |_| Ok(()),
        |_| Ok(()),
        |event| {
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, ()| {
            ran.set(ran.get() + 1);
            async move {
                outcome(
                    &intent,
                    "refresh",
                    "[INCOMPLETE: parent must explicitly dispatch remaining work]",
                )
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(ran.get(), 1);
    assert_eq!(super::super::logical_failures(&results), 1);
    assert!(matches!(
        crate::agent::runlog::plan(&events.borrow(), &["a"]).as_slice(),
        [crate::agent::runlog::Action::Reconcile { .. }]
    ));
    assert!(!super::super::completed_status(&results[0].status));
}

#[tokio::test]
async fn admission_io_stops_new_dispatch_but_journals_drained_completion() {
    let tasks = vec![
        task("admitted", "argus", &[]),
        task("refused", "argus", &[]),
    ];
    let events = RefCell::new(Vec::new());
    let finished = Cell::new(false);
    let error = schedule(
        &tasks,
        2,
        "m",
        &TurnCancel::new(),
        policy(false),
        &[],
        |_| Ok(()),
        |intent| {
            if intent.key.task_id.0 == "refused" {
                Err(SlotDenied::Io("injected".into()))
            } else {
                Ok(())
            }
        },
        |event| {
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, ()| {
            let finished = &finished;
            async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                finished.set(true);
                outcome(&intent, "done", "known completion")
            }
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("infrastructure"));
    assert!(format!("{error:#}").contains("observed spend: 11→3 tok"));
    assert!(format!("{error:#}").contains("known completion"));
    assert!(format!("{error:#}").contains("Workflow incomplete"));
    assert!(finished.get());
    assert!(matches!(
        events.borrow().as_slice(),
        [
            Event::DispatchIntent { .. },
            Event::AttemptCompleted { tokens_in: 11, .. }
        ]
    ));
    State::new(&tasks, policy(false))
        .replay(&events.borrow())
        .unwrap();
}

#[tokio::test]
async fn failed_intent_or_completion_sink_never_releases_dependents_or_retries_sink() {
    for failing_kind in ["intent", "completion", "settlement"] {
        let tasks = vec![task("a", "argus", &[]), task("b", "argus", &["a"])];
        let ran = Cell::new(0);
        let sink_failed = Cell::new(false);
        let error = schedule(
            &tasks,
            2,
            "m",
            &TurnCancel::new(),
            policy(false),
            &[],
            |_| Ok(()),
            |_| Ok(()),
            |event| {
                assert!(
                    !sink_failed.get(),
                    "never append blindly after a torn write"
                );
                let kind = match event {
                    Event::DispatchIntent { .. } => "intent",
                    Event::AttemptCompleted { .. } => "completion",
                    _ => "settlement",
                };
                if kind == failing_kind {
                    sink_failed.set(true);
                    bail!("injected sink failure");
                }
                Ok(())
            },
            |intent, ()| {
                ran.set(ran.get() + 1);
                assert_eq!(intent.key.task_id.0, "a");
                async move { outcome(&intent, "done", "done") }
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("persisting"));
        assert_eq!(ran.get(), usize::from(failing_kind != "intent"));
        if failing_kind != "intent" {
            assert!(format!("{error:#}").contains("observed spend: 11→3 tok"));
            assert!(format!("{error:#}").contains("── a (argus/m) — done ──\ndone"));
        }
    }
}

#[tokio::test]
async fn full_admission_is_finite_and_other_models_progress_without_width_charge() {
    let mut limited = task("limited", "argus", &[]);
    limited.model = Some("full-model".into());
    let tasks = vec![limited, task("other", "argus", &[])];
    let ran = RefCell::new(Vec::new());
    let started = Instant::now();
    let short = Policy {
        admission_timeout: Duration::from_millis(25),
        ..policy(false)
    };
    let error = schedule(
        &tasks,
        1,
        "m",
        &TurnCancel::new(),
        short,
        &[],
        |_| Ok(()),
        |intent| {
            if intent.task.model.as_deref() == Some("full-model") {
                Err(SlotDenied::Full)
            } else {
                Ok(())
            }
        },
        |_| Ok(()),
        |intent, ()| {
            ran.borrow_mut().push(intent.key.task_id.0.clone());
            async move { outcome(&intent, "done", "other model progressed") }
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("bounded admission deadline"));
    assert_eq!(*ran.borrow(), ["other"]);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn cancellation_drains_actual_outcomes_once_without_new_dispatch() {
    let tasks = vec![
        task("a", "argus", &[]),
        task("b", "argus", &[]),
        task("c", "argus", &["a"]),
    ];
    let cancel = TurnCancel::new();
    let events = RefCell::new(Vec::new());
    let ran = RefCell::new(Vec::new());
    let results = schedule(
        &tasks,
        2,
        "m",
        &cancel,
        policy(false),
        &[],
        |_| Ok(()),
        |_| Ok(()),
        |event| {
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, ()| {
            ran.borrow_mut().push(intent.key.task_id.0.clone());
            let cancel = cancel.clone();
            async move {
                if intent.key.task_id.0 == "a" {
                    cancel.cancel();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
                outcome(&intent, "done", "actual outcome after cancel")
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(*ran.borrow(), ["a", "b"]);
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, Event::AttemptCompleted { .. }))
            .count(),
        2
    );
    assert_eq!(results.iter().map(|r| r.tokens_in).sum::<u64>(), 22);
    assert_eq!(super::super::logical_failures(&results), 3);
}

#[tokio::test]
async fn live_blocking_worker_refuses_retry_dependencies_and_replay_but_drains_siblings() {
    use crate::agent::task_tool::{ensure_attempt_finished, SubagentGate};
    use crate::core::repo_lock::RepoTxnLock;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let gate = SubagentGate::isolated(&ACTIVE, 2, fixture.path().join("slots"));
    let model = Arc::new(AtomicUsize::new(0));
    let tasks = vec![
        super::scoped_admission_tests::frozen_task(fixture.path(), "writer", "scoped", &["a.rs"]),
        super::scoped_admission_tests::frozen_task(fixture.path(), "sibling", "scoped", &["b.rs"]),
        task("dependent", "argus", &["writer"]),
    ];
    let events = RefCell::new(Vec::new());
    let ran = RefCell::new(Vec::new());
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let wait = RefCell::new(Some(wait));
    let (finished, done) = tokio::sync::oneshot::channel();
    let finished = RefCell::new(Some(finished));
    let error = schedule(
        &tasks,
        2,
        "m",
        &TurnCancel::new(),
        policy(true),
        &[],
        |_| Ok(()),
        |intent| {
            gate.try_acquire_with(
                (intent.key.task_id.0 == "writer").then(|| (model.clone(), 1)),
                |path| RepoTxnLock::acquire_exclusive(path, Duration::ZERO),
            )
        },
        |event| {
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, permit| {
            ran.borrow_mut().push(intent.key.clone());
            let wait = &wait;
            let finished = &finished;
            async move {
                if intent.key.task_id.0 == "writer" {
                    let slot = Arc::new(permit);
                    let worker_slot = slot.clone();
                    let wait = wait.borrow_mut().take().unwrap();
                    let finished = finished.borrow_mut().take().unwrap();
                    let (started, ready) = tokio::sync::oneshot::channel();
                    let observer = tokio::task::spawn_blocking(move || {
                        let _slot = worker_slot;
                        let _ = started.send(());
                        let result = wait.recv_timeout(Duration::from_secs(5));
                        drop(_slot);
                        let _ = finished.send(result);
                    });
                    ready.await.unwrap();
                    drop(observer);
                    assert!(ensure_attempt_finished(&slot).is_err());
                    outcome(
                        &intent,
                        "unknown-worker",
                        "physical worker still owns the attempt",
                    )
                } else {
                    let _permit = permit;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    outcome(&intent, "done", "drained sibling report")
                }
            }
        },
    )
    .await
    .unwrap_err();
    let still_charged = ACTIVE.load(Ordering::SeqCst);
    let model_charged = model.load(Ordering::SeqCst);
    let blocked = gate.try_acquire_with(Some((model.clone(), 1)), |_| {
        panic!("model cap still owned")
    });
    release.send(()).unwrap();
    done.await.unwrap().unwrap();
    assert_eq!(still_charged, 1);
    assert_eq!(model_charged, 1);
    assert!(matches!(blocked, Err(SlotDenied::Full)));
    assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
    assert_eq!(model.load(Ordering::SeqCst), 0);
    assert_eq!(
        ran.borrow()
            .iter()
            .map(|k| (k.task_id.0.as_str(), k.kind))
            .collect::<Vec<_>>(),
        [
            ("writer", AttemptKind::Initial),
            ("sibling", AttemptKind::Initial)
        ]
    );
    assert!(format!("{error:#}").contains("observed spend: 22→6 tok"));
    assert!(format!("{error:#}").contains("live blocking worker"));
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, Event::AttemptCompleted { .. }))
            .count(),
        2
    );
    assert!(!events
        .borrow()
        .iter()
        .any(|e| matches!(e, Event::TaskSettled { .. })));
    assert!(State::new(&tasks, policy(true))
        .replay(&events.borrow())
        .unwrap_err()
        .to_string()
        .contains("live worker"));
}

const COHORT_BASE: &str = "00000000000000000000000000000001";

fn cohort_summary(base: &str) -> String {
    crate::core::speculation::verification::success_summary_fixture(base)
}

struct RejectRetiredLifecycle {
    calls: Cell<usize>,
}
impl GitAcceptance for RejectRetiredLifecycle {
    fn accept_git(
        &self,
        _: &crate::core::git_ops::TaskHandoff,
    ) -> Result<crate::core::git_ops::VerifiedTaskIntegration> {
        self.calls.set(self.calls.get() + 1);
        anyhow::bail!("retired execution reached Git acceptance")
    }
}

async fn assert_retired_before_callbacks(tasks: &[WorkflowTask], width: usize, replay: &[Event]) {
    let lifecycle = RejectRetiredLifecycle {
        calls: Cell::new(0),
    };
    let error = schedule_with_git_acceptance(
        tasks,
        width,
        "fixture",
        &TurnCancel::new(),
        policy(true),
        replay,
        |_| panic!("retired execution reached preparation"),
        |_| -> std::result::Result<(), SlotDenied> {
            panic!("retired execution reached admission")
        },
        |_| panic!("retired execution appended a journal event"),
        |_, ()| async { panic!("retired execution dispatched a child") },
        &lifecycle,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("retired"), "{error:#}");
    assert_eq!(
        lifecycle.calls.get(),
        0,
        "retired execution cannot reach Git acceptance"
    );
}

// Historical data only. These helpers never enter scheduler admission or reconstruct authority.
fn historical_candidate(tasks: &[WorkflowTask]) -> DispatchIntent {
    State::new(tasks, policy(false))
        .queue
        .into_iter()
        .find(|pending| pending.intent.task.execution == ExecutionMode::Speculative)
        .unwrap()
        .intent
}

fn historical_prefix(intent: &DispatchIntent, status: &str, report: &str) -> Vec<Event> {
    vec![
        Event::SpeculativeCohortCaptured {
            seq: 0,
            cohort_id: COHORT_BASE.into(),
            members: vec![intent.key.clone()],
        },
        Event::DispatchIntent {
            seq: 0,
            intent: intent.clone(),
        },
        completion_event(intent, &outcome(intent, status, report)),
    ]
}

fn historical_integration(intent: &DispatchIntent, receipt: String) -> Event {
    Event::SpeculativeCohortIntegrated {
        seq: 0,
        cohort_id: COHORT_BASE.into(),
        members: vec![intent.key.clone()],
        receipt,
    }
}

fn speculative_task(id: &str, after: &[&str]) -> WorkflowTask {
    let mut task = task(id, "daedalus", after);
    task.execution = ExecutionMode::Speculative;
    let dispatch = std::sync::Arc::make_mut(task.prepared.as_mut().unwrap());
    dispatch.execution = ExecutionMode::Speculative;
    task
}

#[tokio::test]
async fn retired_speculative_ready_sets_refuse_before_direct_or_private_callbacks() {
    let mut tasks = vec![
        speculative_task("first", &[]),
        speculative_task("second", &[]),
        speculative_task("third", &[]),
        task("direct", "argus", &[]),
        task("dependent", "argus", &["first", "second"]),
    ];
    for width in [1, 2, 32] {
        assert_retired_before_callbacks(&tasks, width, &[]).await;
    }
    // A ready direct task cannot hide a later retired selection in the same request.
    tasks.swap(0, 3);
    assert_retired_before_callbacks(&tasks, 2, &[]).await;
}

#[test]
fn historical_speculative_integration_requires_the_complete_frozen_membership() {
    let tasks = vec![
        speculative_task("first", &[]),
        speculative_task("second", &[]),
        task("dependent", "argus", &["first", "second"]),
    ];
    let mut state = State::new(&tasks, policy(true));
    let members: Vec<_> = state
        .queue
        .iter()
        .take(2)
        .map(|p| p.intent.key.clone())
        .collect();
    let mut events = vec![Event::SpeculativeCohortCaptured {
        seq: 0,
        cohort_id: COHORT_BASE.into(),
        members: members.clone(),
    }];
    let first = dispatch(&mut state, "first", AttemptKind::Initial, &mut events);
    complete(
        &mut state,
        first,
        "done",
        "first historical report",
        &mut events,
    );
    let integrated = Event::SpeculativeCohortIntegrated {
        seq: 0,
        cohort_id: COHORT_BASE.into(),
        members: members.clone(),
        receipt: cohort_summary(COHORT_BASE),
    };
    let mut premature = events.clone();
    premature.push(integrated.clone());
    assert!(State::new(&tasks, policy(true)).replay(&premature).is_err());
    let second = dispatch(&mut state, "second", AttemptKind::Initial, &mut events);
    complete(
        &mut state,
        second,
        "done",
        "second historical report",
        &mut events,
    );
    assert!(state.settled.is_empty());
    assert!(!state.eligible(state.queue.front().unwrap()));
    let mut mismatch = events.clone();
    mismatch.push(Event::SpeculativeCohortIntegrated {
        seq: 0,
        cohort_id: COHORT_BASE.into(),
        members: vec![members[0].clone()],
        receipt: cohort_summary(COHORT_BASE),
    });
    assert!(State::new(&tasks, policy(true)).replay(&mismatch).is_err());
    events.push(integrated);
    let mut historical = State::new(&tasks, policy(true));
    historical.replay(&events).unwrap();
    assert_eq!(historical.settlements.len(), 2);
    assert!(historical.settled.is_empty());
    settle(&mut historical, &mut events);
    assert!(historical.eligible(historical.queue.front().unwrap()));
    assert_eq!(
        historical.results.iter().map(|r| r.tokens_in).sum::<u64>(),
        22
    );
    State::new(&tasks, policy(true)).replay(&events).unwrap();
}

#[test]
fn historical_speculative_failures_do_not_enqueue_retries_or_release_dependencies() {
    for (status, report) in [
        ("error", "retained private report"),
        (
            "incomplete",
            "retained private report; incomplete-unverified-handoff",
        ),
        ("deadline", "retained private report"),
        ("max-iters", "retained private report"),
        ("done", "VERDICT: FAIL\nretained private report"),
        ("done", "retained private report"),
    ] {
        let tasks = vec![
            speculative_task("candidate", &[]),
            task("dependent", "argus", &["candidate"]),
        ];
        let mut state = State::new(&tasks, policy(true));
        let mut ignored_dispatch = Vec::new();
        let intent = dispatch(
            &mut state,
            "candidate",
            AttemptKind::Initial,
            &mut ignored_dispatch,
        );
        let events = historical_prefix(&intent, status, report);
        state
            .complete(intent.clone(), outcome(&intent, status, report))
            .unwrap();
        assert_eq!(state.results.len(), 1);
        assert_eq!(state.results[0].summary, report);
        assert_eq!(state.results[0].tokens_in, 11);
        assert!(state.results[0].selected_for.is_none());
        assert_eq!(
            state.settlements.len(),
            1,
            "completion queues, but does not publish, settlement"
        );
        assert!(state.settled.is_empty());
        assert_eq!(
            state.queue.len(),
            1,
            "no retry, fix or planner after {status}"
        );
        assert_eq!(state.queue[0].intent.key.task_id.0, "dependent");
        assert!(!state.eligible(&state.queue[0]));
        assert!(State::new(&tasks, policy(true)).replay(&events).is_err());
        let mut claimed = events;
        claimed.push(historical_integration(&intent, cohort_summary(COHORT_BASE)));
        let candidate = status == "done" && !report.starts_with("VERDICT: FAIL");
        assert_eq!(
            State::new(&tasks, policy(true)).replay(&claimed).is_ok(),
            candidate,
            "accepted-looking metadata cannot rescue failed historical work: {status}"
        );
    }
}

#[tokio::test]
async fn historical_unverified_handoff_needs_acceptance_and_remains_non_success() {
    let tasks = vec![
        speculative_task("candidate", &[]),
        task("dependent", "argus", &["candidate"]),
    ];
    let intent = historical_candidate(&tasks);
    let mut events = historical_prefix(
        &intent,
        super::super::UNVERIFIED_HANDOFF_STATUS,
        "unverified child report",
    );
    assert!(State::new(&tasks, policy(true)).replay(&events).is_err());
    assert_retired_before_callbacks(&tasks, 2, &events).await;
    events.push(historical_integration(&intent, cohort_summary(COHORT_BASE)));
    let mut historical = State::new(&tasks, policy(true));
    historical.replay(&events).unwrap();
    assert!(historical.settled.is_empty());
    assert!(!historical.eligible(historical.queue.front().unwrap()));
    settle(&mut historical, &mut events);
    assert!(historical.eligible(historical.queue.front().unwrap()));
    assert_eq!(
        historical.results[0].status,
        super::super::UNVERIFIED_HANDOFF_STATUS
    );
    assert_eq!(historical.results[0].summary, "unverified child report");
    assert!(!super::super::completed_status(
        &historical.results[0].status
    ));
    assert_eq!(super::super::logical_failures(&historical.results), 1);
    State::new(&tasks, policy(true)).replay(&events).unwrap();
    assert_retired_before_callbacks(&tasks, 2, &events).await;
}

#[test]
fn completed_graph_metadata_releases_git_owner_without_publishing_settlement() {
    super::super::attempt_tests::with_fixture(|fixture| {
        let owned = fixture.allocate();
        let root = owned.root().to_path_buf();
        let owner = std::sync::Arc::downgrade(&owned);
        let tasks = vec![
            task("candidate", "daedalus", &[]),
            task("dependent", "argus", &["candidate"]),
        ];
        let mut state = State::new(&tasks, policy(true));
        let mut events = Vec::new();
        let mut intent = dispatch(&mut state, "candidate", AttemptKind::Initial, &mut events);
        let key = intent.key.clone();
        // No event, prepared task, context or fixture retains this live allocation.
        intent.task.owned = Some(owned);
        intent.task.isolation = super::super::WorkflowIsolation::Worktree;
        assert!(
            owner.upgrade().is_some(),
            "fixture must own the real allocation"
        );
        let result = outcome(&intent, "done", "report alone is not a Git handoff");
        let error = state.complete(intent, result).unwrap_err();
        assert!(
            format!("{error:#}").contains("no typed handoff"),
            "{error:#}"
        );
        assert!(state.intents[&key].task.owned.is_none());
        assert!(
            owner.upgrade().is_none(),
            "completed graph must not retain live Git ownership"
        );
        // Releasing graph ownership is independent of integration or logical settlement.
        assert_eq!(state.results.len(), 1);
        assert!(state.results[0].git_handoff.is_none());
        assert!(state.results[0].selected_for.is_none());
        assert!(state.settled.is_empty());
        assert!(state.settlements.is_empty());
        assert!(!state.eligible(state.queue.front().unwrap()));
        assert_eq!(
            std::fs::read(root.join("owned.txt")).unwrap(),
            b"untouched native live source\n"
        );
        assert_eq!(
            std::fs::read(fixture.work.join("owned.txt")).unwrap(),
            b"untouched native live source\n"
        );
    });
}

#[test]
fn historical_speculative_empty_receipt_cannot_publish_or_settle() {
    let tasks = vec![
        speculative_task("candidate", &[]),
        task("dependent", "argus", &["candidate"]),
    ];
    let intent = historical_candidate(&tasks);
    let mut events = historical_prefix(&intent, "done", "private candidate");
    events.push(historical_integration(&intent, String::new()));
    assert!(
        crate::core::speculation::verification::parse_success_summary("", COHORT_BASE).is_err()
    );
    assert!(crate::agent::runlog::validate_speculative_cohorts(&events).is_err());
    let mut historical = State::new(&tasks, policy(true));
    let error = historical.replay(&events).unwrap_err();
    assert!(format!("{error:#}").contains("lifecycle evidence is unsupported"));
    assert!(historical.results.is_empty());
    assert!(historical.completed.is_empty());
    assert!(historical.settled.is_empty());
    assert!(historical.settlements.is_empty());
    *events.last_mut().unwrap() = historical_integration(&intent, cohort_summary(COHORT_BASE));
    State::new(&tasks, policy(true)).replay(&events).unwrap();
}

#[tokio::test]
async fn historical_captured_prefix_stays_unresolved_and_retired_entry_never_reopens_it() {
    let tasks = vec![speculative_task("candidate", &[])];
    let intent = historical_candidate(&tasks);
    let completed = historical_prefix(&intent, "done", "private candidate");
    for length in 1..=completed.len() {
        let prefix = &completed[..length];
        let mut historical = State::new(&tasks, policy(true));
        let error = historical.replay(prefix).unwrap_err();
        assert!(format!("{error:#}").contains("no accepted integration"));
        assert!(historical.results.is_empty());
        assert!(historical.intents.is_empty());
        assert!(historical.settled.is_empty());
        assert_retired_before_callbacks(&tasks, 1, prefix).await;
    }
}

#[tokio::test]
async fn historical_integrated_prefix_settles_once_but_never_reopens_retired_execution() {
    let tasks = vec![
        speculative_task("candidate", &[]),
        task("dependent", "argus", &["candidate"]),
    ];
    let intent = historical_candidate(&tasks);
    let key = intent.key.clone();
    let mut prefix = historical_prefix(&intent, "done", "private candidate");
    prefix.push(historical_integration(&intent, cohort_summary(COHORT_BASE)));
    for resume in 0..3 {
        let mut historical = State::new(&tasks, policy(true));
        historical.replay(&prefix).unwrap();
        assert!(historical.completed.contains(&key));
        assert!(!historical.queue.iter().any(|p| p.intent.key == key));
        let mut appended = Vec::new();
        if resume == 0 {
            assert!(!historical.eligible(historical.queue.front().unwrap()));
            settle(&mut historical, &mut appended);
            let dependent = dispatch(
                &mut historical,
                "dependent",
                AttemptKind::Initial,
                &mut appended,
            );
            assert_eq!(dependent.inputs, std::slice::from_ref(&key));
            assert!(dependent.task.prompt.contains("private candidate"));
            complete(
                &mut historical,
                dependent,
                "done",
                "consumed integrated dependency",
                &mut appended,
            );
            settle(&mut historical, &mut appended);
        } else {
            assert!(historical.queue.is_empty());
            assert!(historical.settlements.is_empty());
        }
        assert_eq!(super::super::logical_outcomes(&historical.results).len(), 2);
        assert_eq!(
            historical.results.iter().map(|r| r.tokens_in).sum::<u64>(),
            22
        );
        prefix.extend(appended);
        assert_eq!(
            prefix
                .iter()
                .filter(|event| matches!(event,
            Event::TaskSettled { selected, .. } if selected == &key))
                .count(),
            1
        );
        assert_eq!(
            prefix
                .iter()
                .filter(|event| matches!(event,
            Event::DispatchIntent { intent, .. } if intent.key == key))
                .count(),
            1
        );
        assert_retired_before_callbacks(&tasks, 2, &prefix).await;
    }
}

#[test]
fn speculative_replay_refuses_missing_duplicate_or_early_integration_evidence() {
    let tasks = vec![speculative_task("candidate", &[])];
    let key = AttemptKey {
        task_id: TaskId("candidate".into()),
        ordinal: 1,
        kind: AttemptKind::Initial,
    };
    let captured = Event::SpeculativeCohortCaptured {
        seq: 0,
        cohort_id: COHORT_BASE.into(),
        members: vec![key.clone()],
    };
    let intent = State::new(&tasks, policy(false))
        .queue
        .pop_front()
        .unwrap()
        .intent;
    let dispatch = Event::DispatchIntent {
        seq: 0,
        intent: intent.clone(),
    };
    let completion = completion_event(&intent, &outcome(&intent, "done", "private candidate"));
    let integrated = Event::SpeculativeCohortIntegrated {
        seq: 0,
        cohort_id: COHORT_BASE.into(),
        members: vec![key.clone()],
        receipt: cohort_summary(COHORT_BASE),
    };
    let settled = Event::TaskSettled {
        seq: 0,
        task_id: key.task_id.clone(),
        selected: key,
    };
    for events in [
        vec![dispatch.clone(), completion.clone()],
        vec![captured.clone(), dispatch.clone(), completion.clone()],
        vec![
            captured.clone(),
            dispatch.clone(),
            integrated.clone(),
            completion.clone(),
        ],
        vec![
            captured.clone(),
            dispatch.clone(),
            completion.clone(),
            settled.clone(),
            integrated.clone(),
        ],
        vec![
            captured.clone(),
            dispatch.clone(),
            completion.clone(),
            integrated.clone(),
            integrated.clone(),
        ],
    ] {
        assert!(State::new(&tasks, policy(false)).replay(&events).is_err());
    }
    State::new(&tasks, policy(false))
        .replay(&[captured, dispatch, completion, integrated, settled])
        .unwrap();
}

fn unsupported_lifecycle_summaries() -> Vec<String> {
    let valid: serde_json::Value = serde_json::from_str(&cohort_summary(COHORT_BASE)).unwrap();
    let mut summaries = vec![
        String::new(),
        "verified-tree".into(),
        "x".repeat(64 * 1024 + 1),
    ];
    for key in [
        "summary_version",
        "lifecycle_version",
        "verification_started",
        "effect_state",
        "command",
    ] {
        let mut value = valid.clone();
        value.as_object_mut().unwrap().remove(key);
        summaries.push(value.to_string());
    }
    for (key, replacement) in [
        ("summary_version", serde_json::json!(2)),
        ("lifecycle_version", serde_json::json!(2)),
        (
            "base",
            serde_json::json!("00000000000000000000000000000002"),
        ),
        (
            "verification_started",
            serde_json::json!("not-an-immutable-record"),
        ),
        (
            "effect_state",
            serde_json::json!({"state":"unknown_worker"}),
        ),
        (
            "effect_state",
            serde_json::json!({"state":"possibly_applied","record":null}),
        ),
        ("effect_state", serde_json::json!({"state":"future_state"})),
        ("unexpected_authority", serde_json::json!(true)),
    ] {
        let mut value = valid.clone();
        value[key] = replacement;
        summaries.push(value.to_string());
    }
    summaries
}

#[tokio::test]
async fn historical_verifier_lifecycle_remains_fail_closed_and_cannot_reopen_execution() {
    let tasks = vec![
        speculative_task("candidate", &[]),
        task("dependent", "argus", &["candidate"]),
    ];
    let intent = historical_candidate(&tasks);
    for receipt in unsupported_lifecycle_summaries() {
        assert!(
            crate::core::speculation::verification::parse_success_summary(&receipt, COHORT_BASE)
                .is_err()
        );
        let mut recorded = historical_prefix(&intent, "done", "private candidate");
        recorded.push(historical_integration(&intent, receipt));
        assert!(crate::agent::runlog::validate_speculative_cohorts(&recorded).is_err());
        let mut historical = State::new(&tasks, policy(true));
        assert!(
            historical.replay(&recorded).is_err(),
            "historical validation must reject before any state publication"
        );
        assert!(historical.results.is_empty());
        assert!(historical.intents.is_empty());
        assert!(historical.completed.is_empty());
        assert!(historical.settled.is_empty());
        assert!(historical.settlements.is_empty());
        assert_retired_before_callbacks(&tasks, 2, &recorded).await;
    }
    let mut valid = historical_prefix(&intent, "done", "private candidate");
    valid.push(historical_integration(&intent, cohort_summary(COHORT_BASE)));
    State::new(&tasks, policy(true)).replay(&valid).unwrap();
}

struct Permit {
    counts: Rc<RefCell<(usize, usize)>>,
    writer: bool,
    _slot: super::super::super::task_tool::SubagentSlot,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut counts = self.counts.borrow_mut();
        counts.0 -= 1;
        counts.1 -= usize::from(self.writer);
    }
}

#[tokio::test]
async fn every_attempt_kind_uses_one_admission_and_writer_lane_without_blocking_readers() {
    let mut verifier = task("verify", "themis", &["implement"]);
    verifier.retry_on_fail = Some("implement".into());
    let tasks = vec![
        task("implement", "daedalus", &[]),
        verifier,
        task("flaky", "argus", &[]),
        task("reader", "argus", &["implement"]),
    ];
    use crate::agent::task_tool::SubagentGate;
    use crate::core::repo_lock::RepoTxnLock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let gate = SubagentGate::isolated(&ACTIVE, 3, fixture.path().join("slots"));
    let model_counter = std::sync::Arc::new(AtomicUsize::new(0));
    // A task-tool caller holds the exact same limiter before the workflow starts. Different
    // models can progress; every initial/repair/planner/replacement must respect cap 1.
    let task_slot = RefCell::new(Some(
        gate.try_acquire_with(Some((model_counter.clone(), 1)), |path| {
            RepoTxnLock::acquire_exclusive(path, Duration::ZERO)
        })
        .unwrap(),
    ));
    let counts = Rc::new(RefCell::new((0usize, 0usize)));
    let admitted = RefCell::new(Vec::new());
    let completed = RefCell::new(Vec::new());
    let results = schedule(
        &tasks,
        3,
        "m",
        &TurnCancel::new(),
        policy(true),
        &[],
        |_| Ok(()),
        |intent| {
            let limit = (!(intent.key.task_id.0 == "flaky"
                && intent.key.kind == AttemptKind::Initial))
                .then(|| (model_counter.clone(), 1));
            let limited = limit.is_some();
            let slot = gate.try_acquire_with(limit, |path| {
                RepoTxnLock::acquire_exclusive(path, Duration::ZERO)
            })?;
            assert!(model_counter.load(Ordering::SeqCst) <= 1);
            let writer = super::super::workflow_task_is_writer(&intent.task);
            let mut count = counts.borrow_mut();
            assert!(!writer || count.1 == 0, "all repairs share the writer lane");
            count.0 += 1;
            count.1 += usize::from(writer);
            admitted.borrow_mut().push((intent.key.kind, limited));
            Ok(Permit {
                counts: counts.clone(),
                writer,
                _slot: slot,
            })
        },
        |_| Ok(()),
        |intent, permit| {
            let completed = &completed;
            let task_slot = &task_slot;
            async move {
                let _permit = permit;
                if intent.key.task_id.0 == "flaky" && intent.key.kind == AttemptKind::Initial {
                    assert!(
                        task_slot.borrow_mut().take().is_some(),
                        "other model ran while task held cap 1"
                    );
                }
                let summary = if intent.key.task_id.0 == "verify"
                    && intent.key.kind == AttemptKind::Initial
                {
                    "VERDICT: FAIL\nfix once"
                } else if intent.stage == AttemptKind::Metis {
                    "Use an alternative within the existing scope"
                } else {
                    "VERDICT: PASS"
                };
                let status =
                    if intent.key.task_id.0 == "flaky" && intent.stage == AttemptKind::Initial {
                        "error"
                    } else {
                        "done"
                    };
                if intent.key.kind == AttemptKind::Fix {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                completed.borrow_mut().push(intent.key.clone());
                outcome(&intent, status, summary)
            }
        },
    )
    .await
    .unwrap();
    for kind in [
        AttemptKind::Initial,
        AttemptKind::Restart,
        AttemptKind::Fix,
        AttemptKind::Recheck,
        AttemptKind::Metis,
        AttemptKind::Replacement,
    ] {
        assert!(
            admitted.borrow().contains(&(kind, true)),
            "{kind:?} must use the shared model cap 1"
        );
    }
    assert_eq!(*counts.borrow(), (0, 0));
    assert_eq!(
        ACTIVE.load(Ordering::SeqCst),
        0,
        "no global double charge or leak"
    );
    assert_eq!(model_counter.load(Ordering::SeqCst), 0);
    assert_eq!(results.len(), admitted.borrow().len());
    let completed = completed.borrow();
    let reader = completed
        .iter()
        .position(|key| key.task_id.0 == "reader")
        .unwrap();
    let fix = completed
        .iter()
        .position(|key| key.kind == AttemptKind::Fix)
        .unwrap();
    assert!(
        reader < fix,
        "a queued/slow fix must not freeze an independent reader"
    );
}

#[tokio::test]
async fn retired_speculative_scheduler_refuses_before_any_callback() {
    let mut retired = task("legacy", "daedalus", &[]);
    retired.execution = ExecutionMode::Speculative;
    let tasks = [retired];
    let result = schedule(
        &tasks,
        1,
        "fixture",
        &TurnCancel::new(),
        policy(false),
        &[],
        |_| panic!("retired execution reached preparation"),
        |_| -> std::result::Result<(), SlotDenied> {
            panic!("retired execution reached admission")
        },
        |_| panic!("retired execution wrote a journal event"),
        |_, ()| async { panic!("retired execution dispatched a child") },
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("retired"));
    assert_eq!(tasks[0].isolation, super::super::WorkflowIsolation::Shared);
    let json = serde_json::to_string(&tasks[0]).unwrap();
    let historical: WorkflowTask = serde_json::from_str(&json).unwrap();
    assert_eq!(historical.execution, ExecutionMode::Speculative);
    assert!(historical.execution.require_supported().is_err());
}
