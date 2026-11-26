use super::*;
use crate::core::{
    exec_ctx::{self, ExecutionContext},
    file_placement::{tests::TestDir, PlacementContext},
    recovery,
    session_control::{ExecutionMode, PlanTarget},
};
use std::path::{Path, PathBuf};

// The caller holds TEST_HOME_LOCK for the complete fixture lifetime.
struct Fixture {
    temp: TestDir,
    env: Option<config::EnvGuard>,
    previous_live: Option<LiveSnapshot>,
    previous_history: Vec<Message>,
    previous_slug: Option<String>,
    previous_delta: Option<DeltaState>,
}

impl Fixture {
    fn new() -> Self {
        let temp = TestDir::new();
        let home = temp.path().join("home");
        let startup = temp.path().join("startup");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&startup).unwrap();
        let env = config::EnvGuard::set([
            ("AIZEN_HOME", home.as_os_str()),
            ("AIZEN_PROJECT_ROOT", startup.as_os_str()),
        ]);
        let previous_live = std::mem::take(
            &mut *LIVE_SNAPSHOT
                .get_or_init(|| Mutex::new(LiveSnapshot::default()))
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let previous_history = std::mem::take(
            &mut *live_history_slot()
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let previous_slug = session_slug_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let previous_delta = DELTA_STATE.lock().unwrap_or_else(|e| e.into_inner()).take();
        Self {
            temp,
            env: Some(env),
            previous_live: Some(previous_live),
            previous_history,
            previous_slug,
            previous_delta,
        }
    }

    fn context(&self, name: &str) -> ExecutionContext {
        let root = self.temp.path().join(name);
        let cwd = root.join("src");
        std::fs::create_dir_all(&cwd).unwrap();
        let placement = PlacementContext::with_paths(
            cwd.canonicalize().unwrap(),
            config::aizen_home(),
            self.temp.path().join("scratch"),
            name,
        )
        .unwrap()
        .with_project_root(root.canonicalize().unwrap())
        .unwrap();
        ExecutionContext::default().with_placement(placement)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        recovery::clear_focus_test_state();
        *LIVE_SNAPSHOT
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.previous_live.take().unwrap();
        *live_history_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.previous_history);
        *session_slug_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.previous_slug.take();
        *DELTA_STATE.lock().unwrap_or_else(|e| e.into_inner()) = self.previous_delta.take();
        drop(self.env.take());
    }
}

fn provenance() -> (String, String, String) {
    (
        config::project_key(),
        config::project_root().display().to_string(),
        config::project_slug(),
    )
}

fn assert_provenance(path: &Path, expected: &(String, String, String)) {
    let meta = read_session_path_with_state(path).unwrap().1.unwrap();
    assert_eq!(meta.project_key.as_ref(), Some(&expected.0));
    assert_eq!(meta.project_root.as_ref(), Some(&expected.1));
    assert_eq!(meta.project_slug.as_ref(), Some(&expected.2));
}

#[test]
fn foreign_exit_flush_retains_workspace_control_and_recovery_scope() {
    let _home = config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let origin = fixture.context("origin");
    let focused = fixture.context("focused");
    let expected = exec_ctx::with_current(focused.clone(), provenance);
    assert_ne!(
        expected,
        provenance(),
        "startup environment differs from focused project"
    );
    let task = todo::TaskContext::private();
    task.replace(vec![todo::Todo::new("keep task", todo::Status::Pending)])
        .unwrap();
    let control = SessionControl {
        mode: ExecutionMode::Planning,
        generation: 3,
        plan_target: Some(PlanTarget {
            root: expected.1.clone(),
            path: "docs/plans/exit.md".into(),
            id: "p-0123456789abcdef0123456789abcdef".into(),
        }),
        ..SessionControl::default()
    };
    task.control().restore(control.clone()).unwrap();
    let state = task.snapshot();
    let history = vec![Message::user("focused conversation")];
    exec_ctx::with_current(origin.clone(), || {
        recovery::begin(config::project_root().display().to_string(), None);
    });
    exec_ctx::with_current(focused.with_task(task.clone()), || {
        let recovery_scope = recovery::prepare_focus_scope(expected.1.clone(), None).unwrap();
        recovery_scope.activate();
        update_live_history_with_design(&history, None);
    });
    // Simulate a notification/exit racing with another active recovery scope. Routing must
    // select the published scope, not whichever recovery lease happens to be ACTIVE now.
    exec_ctx::with_current(origin, || {
        recovery::prepare_focus_scope(config::project_root().display().to_string(), None)
            .unwrap()
            .activate();
    });
    task.replace(vec![todo::Todo::new(
        "later unsaved task",
        todo::Status::Pending,
    )])
    .unwrap();
    let root = PathBuf::from(&expected.1);
    std::fs::remove_dir(root.join("src")).unwrap();
    std::fs::remove_dir(&root).unwrap();
    // A vanished bound root is still provenance; never replace it with the startup checkout.
    for name in ["first-exit", "second-exit"] {
        set_session_slug(Some(name.into()));
        std::thread::spawn(flush_live_session_on_exit)
            .join()
            .unwrap();
        let path = sessions_dir().join(format!("{name}.json"));
        assert_provenance(&path, &expected);
        let saved = read_session_path_with_state(&path).unwrap();
        assert_eq!(saved.3, state);
        assert_eq!(saved.4, control);
        assert_eq!(saved.0[0].content.as_deref(), Some("focused conversation"));
        assert_eq!(
            recovery::focus_test_history_len(),
            0,
            "origin recovery must remain untouched"
        );
    }
    recovery::prepare_focus_scope(expected.1.clone(), None)
        .unwrap()
        .activate();
    assert_eq!(recovery::focus_test_history_len(), history.len());
    assert!(
        task.is_dirty(),
        "exit flush cannot acknowledge a later task mutation"
    );
}

#[test]
fn project_change_rewrites_envelope_before_delta_even_with_unchanged_history() {
    let _home = config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let origin = fixture.context("origin");
    let focused = fixture.context("focused");
    let mut history = vec![Message::user("same conversation")];
    let state = TaskState::default();
    let save = |context: ExecutionContext, history: &[Message]| {
        exec_ctx::with_current(context, || {
            autosave_session_delta_with_state(history, "focus-delta", None, None, &state).unwrap();
            provenance()
        })
    };
    let original = save(origin.clone(), &history);
    let path = sessions_dir().join("focus-delta.json");
    assert_provenance(&path, &original);
    history.push(Message::assistant("before focus"));
    save(origin.clone(), &history);
    let old_delta = std::fs::read(delta_path(&path)).unwrap();
    let before = std::fs::read(&path).unwrap();
    let expected = save(focused.clone(), &history);
    assert_ne!(original, expected);
    assert_provenance(&path, &expected);
    assert_ne!(std::fs::read(&path).unwrap(), before);
    assert!(!delta_path(&path).exists());
    // A stale delta from before focus must no longer replay after the generation rewrite.
    std::fs::write(delta_path(&path), old_delta).unwrap();
    assert_eq!(
        read_session_path_with_state(&path).unwrap().0.len(),
        history.len()
    );
    history.push(Message::assistant("after focus"));
    save(focused, &history);
    assert_eq!(
        read_session_path_with_state(&path).unwrap().0.len(),
        history.len()
    );
    save(origin, &history);
    assert_provenance(&path, &original);
    assert!(!delta_path(&path).exists());
}
