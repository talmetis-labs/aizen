use super::*;
use crate::core::exec_ctx::ExecutionContext;
use crate::core::session_control::{ExecutionMode, SessionControl};
use clap::Parser;

fn args(flags: &[&str]) -> AgentArgs {
    AgentArgs::try_parse_from(["agent"].into_iter().chain(flags.iter().copied())).unwrap()
}

fn with_home(f: impl FnOnce()) {
    let _lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = crate::core::file_placement::tests::TestDir::new();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", dir.path().as_os_str())]);
    f();
}

fn with_owner(owner: agent::todo::TaskContext, f: impl FnOnce()) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(agent::todo::with_context(owner, async { f() }));
}

#[test]
fn fresh_plan_is_durable_before_startup_and_builds_only_restricted_registry() {
    with_home(|| {
        let owner = agent::todo::TaskContext::private();
        with_owner(owner.clone(), || {
            let mut args = args(&["--plan", "inspect without implementing"]);
            let old = ExecutionContext::default().with_task(owner.clone());
            prepare_agent_control(&mut args).unwrap();
            let slug = args.resume.as_deref().unwrap();
            assert!(args.save_session);
            let (saved, _) = read_agent_resume_state(slug).unwrap();
            assert!(
                saved.0.is_empty(),
                "the submitted task has not executed yet"
            );
            assert_eq!(saved.4, owner.control().snapshot());
            assert!(saved.4.is_planning());
            assert!(
                !std::path::Path::new(&saved.4.plan_target.as_ref().unwrap().root)
                    .join(&saved.4.plan_target.as_ref().unwrap().path)
                    .exists()
            );
            assert!(old.policy().unwrap().validate_current().is_err());
            let fresh = ExecutionContext::default()
                .with_task(owner.clone())
                .admit_policy()
                .unwrap();
            assert!(!fresh.permits_automation());
            let registry = agent::builtin::default_registry_with_task(
                reqwest::Client::new(),
                String::new(),
                String::new(),
                String::new(),
                ApprovalMode::Yolo,
                8192,
                None,
            )
            .unwrap();
            let names = registry.advertised_names();
            assert!(!names.is_empty());
            for name in names {
                assert!(fresh.policy().unwrap().permits_tool(&name), "{name}");
            }
            assert!(crate::agent::planning::check_automation_boundary().is_err());
        });
    });
}

#[test]
fn omitted_flags_preserve_restriction_and_exit_needs_fresh_execution_context() {
    with_home(|| {
        let owner = agent::todo::TaskContext::private();
        let mut entry = args(&["--plan", "inspect"]);
        with_owner(owner, || {
            prepare_agent_control(&mut entry).unwrap();
        });
        let slug = entry.resume.unwrap();
        let restored = agent::todo::TaskContext::private();
        with_owner(restored.clone(), || {
            let mut resume = args(&["--resume", &slug, "now implement"]);
            prepare_agent_control(&mut resume).unwrap();
            let old = ExecutionContext::default().with_task(restored.clone());
            assert!(
                old.is_planning(),
                "ordinary prose cannot lift the saved restriction"
            );
            let lease = old.admit_policy().unwrap();
            let mut exit = args(&["--exit-plan", "--resume", &slug, "implement"]);
            assert!(prepare_agent_control(&mut exit).is_err());
            drop(lease);
            prepare_agent_control(&mut exit).unwrap();
            assert_eq!(exit.resume.as_deref(), Some(slug.as_str()));
            assert!(exit.save_session);
            assert!(old.is_planning());
            assert!(old.policy().unwrap().validate_current().is_err());
            assert!(!old.permits_automation());
            let fresh = ExecutionContext::default()
                .with_task(restored.clone())
                .admit_policy()
                .unwrap();
            assert!(!fresh.is_planning());
            assert!(fresh.permits_automation());
            let (saved, _) = read_agent_resume_state(&slug).unwrap();
            assert_eq!(saved.4.mode, ExecutionMode::Normal);
            assert!(
                saved.4.plan_target.is_some(),
                "permission exit does not erase plan identity"
            );
        });
    });
}

#[test]
fn unsupported_exit_and_corrupt_resume_abort_before_any_startup() {
    with_home(|| {
        std::fs::create_dir_all(session_store::sessions_dir()).unwrap();
        session_store::write_session_with_control(
            &[Message::user("normal history")],
            "normal",
            Default::default(),
            None,
            &Default::default(),
            &SessionControl::default(),
        )
        .unwrap();
        let corrupt = session_store::sessions_dir().join("corrupt.json");
        let bytes = br#"{"version":3,"messages":[],"session_control":null}"#;
        std::fs::write(&corrupt, bytes).unwrap();
        for flags in [
            vec!["--exit-plan", "implement"],
            vec!["--exit-plan", "--resume", "normal", "implement"],
            vec!["--resume", "missing", "inspect"],
            vec!["--plan", "--resume", "corrupt", "inspect"],
            vec!["--exit-plan", "--resume", "corrupt", "implement"],
            vec!["--resume", "normal.json", "inspect"],
        ] {
            with_owner(agent::todo::TaskContext::private(), || {
                assert!(
                    prepare_agent_control(&mut args(&flags)).is_err(),
                    "{flags:?}"
                );
            });
        }
        assert_eq!(std::fs::read(&corrupt).unwrap(), bytes);
        assert!(!read_agent_resume_state("normal")
            .unwrap()
            .0
             .4
            .is_planning());
    });
}

#[test]
fn selected_existing_plan_rechecks_identity_before_attachment() {
    with_home(|| {
        let workspace = crate::core::file_placement::tests::TestDir::new();
        let root = workspace.path().canonicalize().unwrap();
        let path = root.join("docs/plans/selected.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let id = "p-0123456789abcdef0123456789abcdef";
        let text = format!("---\naizen_plan: 1\nid: {id}\n---\n\n<!-- aizen:phase investigate -->\n## Investigate\n");
        std::fs::write(&path, &text).unwrap();
        let selected =
            crate::agent::plan_control::select_target(&root, Some("docs/plans/selected.md"), None)
                .unwrap();
        let owner = agent::todo::TaskContext::private();
        owner
            .control()
            .restore(SessionControl {
                mode: ExecutionMode::Planning,
                plan_target: Some(selected.target.clone()),
                ..Default::default()
            })
            .unwrap();
        with_owner(owner.clone(), || {
            attach_agent_selected_plan(&selected.target).unwrap();
            assert_eq!(owner.snapshot().plan.unwrap().id, id);
            let before = owner.snapshot();
            std::fs::write(
                &path,
                text.replace(id, "p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            )
            .unwrap();
            assert!(attach_agent_selected_plan(&selected.target).is_err());
            assert_eq!(
                owner.snapshot(),
                before,
                "changed identity is not silently attached"
            );
            std::fs::remove_file(&path).unwrap();
            assert!(attach_agent_selected_plan(&selected.target).is_err());
        });
    });
}

#[test]
fn failed_initial_session_persistence_never_reaches_startup() {
    with_home(|| {
        std::fs::write(session_store::sessions_dir(), b"not a directory").unwrap();
        let owner = agent::todo::TaskContext::private();
        with_owner(owner, || {
            let mut request = args(&["--plan", "inspect"]);
            let started = std::cell::Cell::new(false);
            let result: Result<()> = (|| {
                prepare_agent_control(&mut request)?;
                started.set(true);
                Ok(())
            })();
            assert!(result.is_err());
            assert!(!started.get());
            assert!(request.resume.is_none());
        });
    });
}
