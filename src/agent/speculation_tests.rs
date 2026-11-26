// Real public TaskTool preparation/executor refusal, not historical overlay backend acceptance
// and not a claim that Git-native isolation is equivalent to the retired source lane.
use super::*;
use crate::agent::task_tool::TaskTool;
use crate::agent::tools::Tool;
use crate::core::persist::FileFingerprint;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MODEL: &str = "owned-speculation-fixture";
const KEY: &str = "owned-loopback-account";
const SOURCE: &str = "pub fn alpha() -> i32 {\n    10\n}\n\npub fn beta() -> i32 {\n    20\n}\n";
const FILE: &str = "owned.rs";

// Only the exclusively created fixture is inspected, never ambient HOME or another repository.
fn fixture_image(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, directory: &Path, image: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(
                !metadata.file_type().is_symlink(),
                "fixture must contain no links"
            );
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if metadata.is_dir() {
                image.insert(relative, None);
                visit(root, &path, image);
            } else {
                assert!(metadata.is_file());
                image.insert(relative, Some(std::fs::read(path).unwrap()));
            }
        }
    }
    let mut image = BTreeMap::new();
    visit(root, root, &mut image);
    image
}

struct LedgerEntry(PathBuf);
impl Drop for LedgerEntry {
    fn drop(&mut self) {
        crate::core::read_ledger::forget(&self.0);
    }
}

fn assert_retired(message: &str, worktree_requested: bool) {
    let expected = if worktree_requested {
        // The existing closed-contract check precedes the generic execution-mode parser.
        "worktree isolation requires direct execution"
    } else {
        "speculative execution is retired"
    };
    assert!(message.contains(expected), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_speculative_task_batch_refuses_before_prepared_dispatch() {
    refused_tasks(false, false, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_speculative_task_batch_refuses_at_planned_boundary() {
    refused_tasks(true, false, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_speculative_reader_and_writers_never_fall_back_to_live_files() {
    refused_tasks(false, true, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_speculative_task_cannot_implicitly_select_git_worktree_isolation() {
    for planned in [false, true] {
        refused_tasks(planned, false, true, None).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_speculative_task_refuses_before_hooks_even_when_hooks_are_disabled() {
    for disabled in [false, true] {
        refused_tasks(false, false, false, Some(disabled)).await;
    }
}

#[allow(clippy::await_holding_lock)] // Pin the disposable HOME through executor refusal.
async fn refused_tasks(
    planned: bool,
    reader: bool,
    worktree_requested: bool,
    hooks_disabled: Option<bool>,
) {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (fixture, mut cfg) = placement_executor_fixture();
    let root = cfg.effective_root();
    let home = cfg.exec_ctx.placement().unwrap().home_root().to_path_buf();
    std::fs::create_dir_all(&home).unwrap();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()),
        ("AIZEN_PROJECT_ROOT", root.as_os_str()),
    ]);
    let _route = crate::core::config::EnvGuard::unset([
        "AIZEN_MODEL_OWNED_SPECULATION_FIXTURE_BASE_URL",
        "AIZEN_MODEL_OWNED_SPECULATION_FIXTURE_API_KEY",
        "AIZEN_NO_HOOKS",
    ]);
    let _toggle = hooks_disabled.filter(|disabled| *disabled).map(|_| {
        crate::core::config::EnvGuard::set([("AIZEN_NO_HOOKS", std::ffi::OsStr::new("1"))])
    });
    std::fs::write(root.join(FILE), SOURCE).unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::create_dir(root.join(".aizen")).unwrap();
    #[cfg(windows)]
    let hook = "echo forbidden> target/hook-ran.txt";
    #[cfg(not(windows))]
    let hook = "printf forbidden > target/hook-ran.txt";
    if let Some(disabled) = hooks_disabled {
        std::fs::write(
            home.join("cli-config.json"),
            json!({"hooks":{"pre_tool":[{"match":"*","run":hook,"timeout_secs":1}]}}).to_string(),
        )
        .unwrap();
        assert_eq!(hooks::configured().is_none(), disabled);
    } else {
        assert!(hooks::configured().is_none());
    }
    #[cfg(windows)]
    let verify = "echo forbidden> target/verify-ran.txt";
    #[cfg(not(windows))]
    let verify = "printf forbidden > target/verify-ran.txt";
    std::fs::write(
        root.join(".aizen/verify.json"),
        json!({"commands":[verify],"timeout_secs":1}).to_string(),
    )
    .unwrap();
    crate::agent::mcp::trust_project().unwrap();
    assert_eq!(
        verify_gate::detect_verify_commands(&root),
        vec![verify_gate::VerifyCommand::Custom(verify.into())]
    );
    cfg.enable_verify_gate = true;
    cfg.verify_gate_timeout_secs = 1;
    cfg.image_config = Some(Arc::new(crate::core::cli_config::CliConfig::default()));
    cfg.deadline = Some(std::time::Instant::now() + Duration::from_secs(2));
    let changes = Arc::new(crate::core::exec_ctx::WorkspaceChanges::default());
    let body_count = Arc::new(AtomicUsize::new(0));
    let observed_bodies = body_count.clone();
    cfg.exec_ctx = cfg
        .exec_ctx
        .with_deadline(cfg.deadline)
        .with_workspace_changes(changes.clone())
        .with_native_body_probe(builtin::NativeBodyTestProbe::new(move |_, _| {
            observed_bodies.fetch_add(1, Ordering::SeqCst);
        }));
    let ledger = LedgerEntry(root.join(FILE));
    let prior_read = FileFingerprint::for_bytes(b"prior observation, not the current source bytes");
    crate::core::read_ledger::note(&ledger.0, &prior_read);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    let task = TaskTool::new(
        client,
        base.clone(),
        KEY.into(),
        MODEL.into(),
        crate::core::approval::ApprovalMode::Yolo,
        root.clone(),
        0,
        0,
    );
    assert_eq!(
        task.parameters()["properties"]["execution"]["enum"],
        json!(["direct"])
    );
    let mut calls = Vec::new();
    for (id, role) in [
        ("first", "daedalus"),
        ("second", "daedalus"),
        ("reader", "argus"),
    ]
    .into_iter()
    .take(if reader { 3 } else { 2 })
    {
        let mut args = json!({"role":role,"model":MODEL,"execution":"speculative","max_steps":1,
            "prompt":"Inspect only owned.rs through native file tools and report the requested result. Do not run commands, delegate, or claim verification."});
        args[if role == "argus" {
            "read_paths"
        } else {
            "write_paths"
        }] = json!([FILE]);
        if worktree_requested {
            args["isolation"] = json!("worktree");
        }
        let dispatch = task.resolve_dispatch(&args);
        assert_eq!(dispatch.base_url, base);
        assert_eq!(dispatch.api_key, KEY);
        assert_eq!(dispatch.model, MODEL);
        calls.push(call(id, "task", &args.to_string()));
    }
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(task));
    let before = fixture_image(fixture.path());
    let prepared = if planned {
        plan_execution_calls(&registry, &calls, &cfg)
    } else {
        prepare_execution_calls(&registry, &calls, &cfg)
    };
    for result in &prepared {
        match result {
            Err(message) => assert_retired(message, worktree_requested),
            Ok(_) => panic!("retired request produced a prepared dispatch capability"),
        }
    }
    let mut sink: Vec<_> = calls
        .iter()
        .map(|call| Message::tool_result(&call.id, "pending"))
        .collect();
    let (mut lease, mut baseline) = (None, None);
    let records = execute_execution_calls(
        &registry,
        &calls,
        &prepared,
        planned,
        &cfg,
        &mut sink,
        Vec::new(),
        &mut lease,
        &mut baseline,
        &[],
    )
    .await;
    assert_eq!(records.len(), calls.len());
    let tool = registry.get("task").unwrap();
    for record in &records {
        assert_eq!(record.outcome.effect_state, tools::ToolEffectState::Settled);
        assert_retired(
            record
                .outcome
                .failure
                .as_deref()
                .expect("retirement is a refusal"),
            worktree_requested,
        );
        assert!(!record.changed_workspace(tool));
    }
    assert!(lease.is_none());
    assert!(baseline.is_none(), "refusal cannot run a baseline verifier");
    assert_eq!(body_count.load(Ordering::SeqCst), 0);
    assert!(!changes.changed());
    assert!(changes.paths().is_empty());
    assert!(cfg.exec_ctx.native_scope().is_none());
    assert!(!cfg.exec_ctx.has_owned_git_task());
    assert!(cfg.exec_ctx.tool_workers().is_none());
    assert!(cfg.exec_ctx.can_restore_workspace());
    assert_eq!(
        crate::core::read_ledger::observed(&ledger.0),
        Some(prior_read)
    );
    assert_eq!(
        fixture_image(fixture.path()),
        before,
        "retirement altered source, hooks, verification, journals, ownership or slot artifacts"
    );
    for path in [
        "native-journal-v1",
        "native-workspaces",
        "timemachine",
        "coop",
        "locks/v1/slots/subagents",
    ] {
        assert!(!home.join(path).exists(), "retirement created {path}");
    }
    // Retained throughout preparation/execution: no responder or background handler can hide
    // an accepted request. This short accept window observes the owned listener only.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "retirement contacted the owned loopback provider"
    );
}
