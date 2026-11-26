use super::*;
use crate::core::{config, file_placement::PlacementContext, git_ops, recovery, workspace_txn};
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Fixture {
    _temp: crate::core::file_placement::tests::TestDir,
    root: PathBuf,
    linked: PathBuf,
    home: PathBuf,
    git: PathBuf,
    env: Option<config::EnvGuard>,
    prior_input: Option<PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let root = temp.path().join("repo");
        let linked = temp.path().join("linked with spaces");
        let home = temp.path().join("home");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&home).unwrap();
        let env = config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let f = Self {
            _temp: temp,
            root,
            linked,
            home,
            git: crate::core::gitx::git_exe().unwrap(),
            env: Some(env),
            prior_input: crate::ui::tui::input_workspace(),
        };
        f.git(&f.root, &["init", "-q"]);
        f.git(&f.root, &["config", "user.name", "Fixture"]);
        f.git(
            &f.root,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(f.root.join("same.txt"), "original\n").unwrap();
        f.git(&f.root, &["add", "--", "same.txt"]);
        f.git(&f.root, &["commit", "-qm", "base"]);
        f.git(
            &f.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked",
                "../linked with spaces",
            ],
        );
        f
    }
    fn git(&self, root: &Path, args: &[&str]) -> Vec<u8> {
        let out = std::process::Command::new(&self.git)
            .current_dir(root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        crate::features::coop::clear_focus_test_state();
        recovery::clear_focus_test_state();
        crate::ui::tui::swap_input_workspace_for_test(self.prior_input.take());
        drop(self.env.take());
    }
}

#[test]
fn focus_round_trip_preserves_dirty_index_conflict_and_routes_next_turn() {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = config::TEST_HOME_LOCK
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
    let f = Fixture::new();
    // A real index conflict marker plus independent staged/unstaged/untracked bytes survive focus.
    fs::write(f.root.join("same.txt"), "staged\n").unwrap();
    f.git(&f.root, &["add", "--", "same.txt"]);
    fs::write(f.root.join("same.txt"), "unstaged\n").unwrap();
    fs::write(f.root.join("pending.txt"), "untracked\n").unwrap();
    let base = f.git(&f.root, &["rev-parse", "HEAD"]);
    fs::write(f.root.join(".git/MERGE_HEAD"), &base).unwrap();
    let index = fs::read(f.root.join(".git/index")).unwrap();
    let canonical = f.root.canonicalize().unwrap();
    let linked = f.linked.canonicalize().unwrap();
    let placement = PlacementContext::new(canonical.clone(), f.home.clone()).unwrap();
    let original = ExecutionContext::default().with_placement(placement);
    let old = original.workspace_binding().cloned();
    let session = SessionWorkspace::new(&original).unwrap();
    let task = crate::agent::todo::TaskContext::private();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    workspace_txn::with_test_lock_home(&f.home, || {
        runtime.block_on(crate::agent::todo::with_context(
            task.clone(),
            in_session(session, original.clone(), async {
                recovery::begin(canonical.display().to_string(), None);
                crate::features::coop::begin(None);
                let stale = git_ops::PreparedFocus::prepare(&canonical, &linked).unwrap();
                f.git(&f.linked, &["commit", "--allow-empty", "-qm", "moved"]);
                assert!(stale.revalidate().is_err());
                let epoch = task.effect_epoch();
                let history = vec![crate::core::types::Message::user(
                    "retain this conversation across focus",
                )];
                let next = focus(linked.to_str().unwrap(), &history, None).unwrap();
                assert_eq!(recovery::focus_test_history_len(), 1);
                // No yield: a queued ready submission must already see B in the current poll.
                assert_eq!(config::project_root(), linked);
                assert_eq!(
                    exec_ctx::current().unwrap().workspace_binding(),
                    next.workspace_binding()
                );
                assert!(next.require_current_binding(old.as_ref()).is_err());
                assert!(task.effect_epoch() > epoch);
                assert_eq!(
                    recovery::focus_test_scope().as_deref(),
                    Some(linked.display().to_string().as_str())
                );
                assert_eq!(
                    crate::features::coop::focus_test_root(),
                    Some(linked.clone())
                );
                let turn = crate::repl::turn::ResolvedTurn::from_frozen(
                    crate::core::cli_config::ResolvedEndpoint {
                        base_url: "http://127.0.0.1:9/v1".into(),
                        api_key: "unused".into(),
                        model: "fixture".into(),
                    },
                    None,
                    None,
                    crate::agent::design::DesignContext::default(),
                    crate::core::cli_config::CliConfig::default(),
                );
                assert_eq!(
                    turn.execution_context().workspace_binding(),
                    next.workspace_binding()
                );
                tokio::task::yield_now().await;
                assert_eq!(config::project_root(), linked);
                f.git(&f.root, &["worktree", "lock", "../linked with spaces"]);
                assert!(git_ops::PreparedFocus::prepare(&canonical, &linked).is_err());
                f.git(&f.root, &["worktree", "unlock", "../linked with spaces"]);
                let registry = crate::agent::builtin::default_registry_in(&linked);
                let read = registry.get("file_read").unwrap();
                let call = crate::agent::planning::prepare(
                    read,
                    &serde_json::json!({"path":"same.txt"}),
                    &next,
                )
                .unwrap();
                assert!(read
                    .execute_prepared(&call)
                    .unwrap()
                    .text
                    .contains("original"));
                exec_ctx::with_current_future(original.clone(), async {
                    tokio::task::yield_now().await;
                    assert_eq!(config::project_root(), canonical);
                })
                .await;
                let mut before_busy = None;
                let admitted =
                    crate::agent::hooks::admit_automation(&turn.execution_context()).unwrap();
                exec_ctx::with_current(admitted, || {
                    before_busy = Some(
                        focus(canonical.to_str().unwrap(), &[], None)
                            .unwrap_err()
                            .to_string(),
                    );
                });
                let error = before_busy.unwrap();
                assert!(
                    error.contains("settled workers"),
                    "unexpected refusal: {error}"
                );
                assert_eq!(config::project_root(), linked);
                let returned = focus(canonical.to_str().unwrap(), &[], None).unwrap();
                assert_eq!(config::project_root(), canonical);
                assert!(
                    returned.require_current_binding(old.as_ref()).is_err(),
                    "A-B-A must not revive an old plan"
                );
                assert_eq!(fs::read(f.root.join("same.txt")).unwrap(), b"unstaged\n");
                assert_eq!(
                    fs::read(f.root.join("pending.txt")).unwrap(),
                    b"untracked\n"
                );
                assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), index);
                assert_eq!(fs::read(f.root.join(".git/MERGE_HEAD")).unwrap(), base);
                // A later notification from an old pinned context must mark B, never current A.
                exec_ctx::with_current(next, || {
                    crate::features::coop::set_task("old pinned B notification");
                    recovery::mark_effects_unsettled();
                });
                assert_eq!(
                    crate::features::coop::focus_test_root(),
                    Some(canonical.clone())
                );
                assert!(focus(linked.to_str().unwrap(), &[], None).is_err());
                assert_eq!(config::project_root(), canonical);
            }),
        ))
    });
}
