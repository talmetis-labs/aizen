//! Deterministic journal/dependency fixtures: no provider requests or real branch mutations.
use super::*;
use crate::agent::workflow::WorkflowIsolation;
use crate::core::{
    git_ops::{TaskHandoff, VerifiedTaskIntegration},
    snapshots::GitOid,
};
use std::cell::{Cell, RefCell};

fn oid(c: char) -> GitOid {
    GitOid::parse(&c.to_string().repeat(40)).unwrap()
}
fn handoff(id: &str, base: GitOid) -> TaskHandoff {
    TaskHandoff {
        repo_id: "a".repeat(64),
        worktree_id: "b".repeat(64),
        root: std::env::temp_dir().join(format!("owned-{id}")),
        branch: format!("refs/heads/{id}"),
        owner: format!("parent/{id}"),
        registration: crate::features::coop::GitTaskReceipt {
            registration_id: format!("git-task-{id}"),
            owner: format!("parent/{id}"),
            manifest_sha256: [1; 32],
        },
        base,
        tip: oid('2'),
        commits: vec![oid('2')],
        paths: vec!["src/lib.rs".into()],
        dirty: false,
        verified: false,
        integrated: false,
        tests: "not_run_native_only".into(),
        inverse: None,
    }
}
fn tasks() -> Vec<WorkflowTask> {
    ["first", "dependent"]
        .into_iter()
        .map(|id| WorkflowTask {
            id: id.into(),
            role: "daedalus".into(),
            prompt: "Complete the exact native task, without widening authority.".into(),
            isolation: WorkflowIsolation::Worktree,
            write_paths: Some(vec!["src/lib.rs".into()]),
            after: if id == "dependent" {
                vec!["first".into()]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
        .collect()
}
struct Lifecycle {
    allow: bool,
}
impl GitAcceptance for Lifecycle {
    fn accept_git(&self, h: &TaskHandoff) -> Result<VerifiedTaskIntegration> {
        anyhow::ensure!(self.allow, "parent verification missing");
        Ok(VerifiedTaskIntegration {
            integration: format!("integration-{}", h.registration.registration_id),
            verification: "verify-accepted".into(),
            destination: h.tip.clone(),
        })
    }
}
fn outcome(intent: &DispatchIntent) -> TaskOutcome {
    TaskOutcome {
        id: intent.key.display(),
        attempt: Some(intent.key.clone()),
        selected_for: None,
        role: "daedalus".into(),
        model: "fixture".into(),
        status: "git-handoff-pending".into(),
        summary: "Child changes remain unverified until parent checks.".into(),
        iters: 2,
        tokens_in: 10,
        tokens_out: 3,
        evidence_images: Vec::new(),
        git_handoff: Some(handoff(
            &intent.task.id,
            intent.task.git_base.clone().unwrap(),
        )),
    }
}
#[tokio::test]
async fn git_pending_handoff_blocks_release_then_resume_never_redispatches_it() -> Result<()> {
    let tasks = tasks();
    let events = RefCell::new(Vec::new());
    let ran = RefCell::new(Vec::new());
    let result = schedule_with_git_acceptance(
        &tasks,
        1,
        "fixture",
        &TurnCancel::new(),
        Policy::default(),
        &[],
        |intent| {
            intent.task.git_base = Some(oid('1'));
            Ok(())
        },
        |_| Ok(()),
        |event| {
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, ()| {
            ran.borrow_mut().push(intent.task.id.clone());
            async move { outcome(&intent) }
        },
        &Lifecycle { allow: false },
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("parent verification missing"));
    assert_eq!(*ran.borrow(), ["first"]);
    assert!(!events
        .borrow()
        .iter()
        .any(|e| matches!(e, Event::TaskSettled { .. } | Event::GitTaskAccepted { .. })));
    let replay = events.borrow().clone();
    let calls = Cell::new(0);
    let resumed = schedule_with_git_acceptance(
        &tasks,
        1,
        "fixture",
        &TurnCancel::new(),
        Policy::default(),
        &replay,
        |intent| {
            assert_eq!(intent.task.id, "dependent");
            intent.task.git_base = Some(oid('2'));
            Ok(())
        },
        |_| Ok(()),
        |event| {
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, ()| {
            calls.set(calls.get() + 1);
            assert_eq!(intent.task.git_base, Some(oid('2')));
            async move {
                let mut o = outcome(&intent);
                let h = o.git_handoff.as_mut().unwrap();
                h.tip = oid('3');
                h.commits = vec![oid('3')];
                o
            }
        },
        &Lifecycle { allow: true },
    )
    .await?;
    assert_eq!(calls.get(), 1, "completed first child must not be replayed");
    assert!(resumed.iter().all(|o| o.status == "done"));
    let events = events.borrow();
    let accepted = events
        .iter()
        .position(|e| matches!(e, Event::GitTaskAccepted { key, .. } if key.task_id.0 == "first"))
        .unwrap();
    let settled = events
        .iter()
        .position(|e| matches!(e, Event::TaskSettled { task_id, .. } if task_id.0 == "first"))
        .unwrap();
    let dependent = events
        .iter()
        .position(
            |e| matches!(e, Event::DispatchIntent { intent, .. } if intent.task.id == "dependent"),
        )
        .unwrap();
    assert!(accepted < settled && settled < dependent);
    State::new(&tasks, Policy::default()).replay(&events)?;
    Ok(())
}

#[test]
fn git_replay_rejects_settlement_without_exact_acceptance() -> Result<()> {
    let tasks = tasks();
    let key = AttemptKey {
        task_id: TaskId("first".into()),
        ordinal: 1,
        kind: AttemptKind::Initial,
    };
    let mut task = tasks[0].clone();
    task.git_base = Some(oid('1'));
    let intent = DispatchIntent {
        key: key.clone(),
        goal: key.task_id.clone(),
        task,
        inputs: Vec::new(),
        stage: AttemptKind::Initial,
        trigger: None,
        deadline_unix_ms: None,
        live_deadline: None,
    };
    let o = outcome(&intent);
    let events = vec![
        Event::DispatchIntent { seq: 0, intent },
        Event::AttemptCompleted {
            seq: 1,
            key: key.clone(),
            role: o.role,
            model: o.model,
            status: o.status,
            summary: o.summary,
            iters: o.iters,
            tokens_in: 0,
            tokens_out: 0,
            git_handoff: o.git_handoff,
        },
        Event::TaskSettled {
            seq: 2,
            task_id: key.task_id.clone(),
            selected: key,
        },
    ];
    assert!(crate::agent::runlog::validate_git_workflow(&events).is_err());
    assert!(State::new(&tasks, Policy::default())
        .replay(&events)
        .is_err());
    Ok(())
}

#[tokio::test]
async fn git_failed_acceptance_journal_never_publishes_or_releases_dependents() -> Result<()> {
    let tasks = tasks();
    let events = RefCell::new(Vec::new());
    let ran = RefCell::new(Vec::new());
    let sink_failed = Cell::new(false);
    let error = schedule_with_git_acceptance(
        &tasks,
        2,
        "fixture",
        &TurnCancel::new(),
        Policy::default(),
        &[],
        |intent| {
            intent.task.git_base = Some(oid('1'));
            Ok(())
        },
        |_| Ok(()),
        |event| {
            assert!(
                !sink_failed.get(),
                "never append after a failed Git acceptance write"
            );
            if matches!(&event, Event::GitTaskAccepted { .. }) {
                sink_failed.set(true);
                bail!("injected Git acceptance persistence failure");
            }
            events.borrow_mut().push(event);
            Ok(())
        },
        |intent, ()| {
            ran.borrow_mut().push(intent.task.id.clone());
            async move { outcome(&intent) }
        },
        &Lifecycle { allow: true },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("persisting Git acceptance; dependencies not released"));
    assert!(sink_failed.get());
    assert_eq!(*ran.borrow(), ["first"]);
    assert!(!events.borrow().iter().any(|event| matches!(
        event,
        Event::GitTaskAccepted { .. } | Event::TaskSettled { .. }
    )));
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| matches!(event, Event::AttemptCompleted { .. }))
            .count(),
        1
    );
    let mut historical = State::new(&tasks, Policy::default());
    historical.replay(&events.borrow())?;
    assert_eq!(historical.results.len(), 1);
    assert_eq!(historical.results[0].status, "git-handoff-pending");
    assert!(historical.results[0].git_handoff.is_some());
    assert!(historical.results[0].selected_for.is_none());
    assert!(historical.git_accepted.is_empty());
    assert!(historical.settled.is_empty());
    assert_eq!(historical.settlements.len(), 1);
    assert_eq!(historical.queue.len(), 1);
    assert_eq!(historical.queue[0].intent.task.id, "dependent");
    assert!(!historical.eligible(&historical.queue[0]));
    Ok(())
}
