// Acceptance coverage through the real TaskTool, central executor, native bodies, and verify
// runner. Only the provider wire is mocked, on an owned loopback listener. In particular, the
// native-body probe observes/pauses real overlay edits; it never grants a scheduling capability.

use super::*;
use crate::agent::task_tool::TaskTool;
use crate::core::speculation::store::{OverlayFile, OverlayHandle};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MODEL: &str = "owned-speculation-fixture";
const KEY: &str = "owned-loopback-account";
const SOURCE: &str = "pub fn alpha() -> i32 {\n    10\n}\n\npub fn beta() -> i32 {\n    20\n}\n";
const FILE: &str = "owned.rs";
const WAIT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speculative_real_tasks_merge_distinct_functions_after_all_children_settle() {
    real_speculative_tasks(false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speculative_real_tasks_merge_distinct_functions_at_planned_boundary() {
    real_speculative_tasks(true, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speculative_real_argus_reads_base_while_writers_read_own_private_changes() {
    real_speculative_tasks(false, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speculative_real_tasks_same_function_conflict_keeps_live_bytes() {
    real_speculative_tasks(false, false, true).await;
}

#[derive(Clone)]
struct BodyArrival {
    label: String,
    path: PathBuf,
    overlay: OverlayHandle,
    file: OverlayFile,
}

#[derive(Clone)]
struct ReleaseBodies(Arc<(Mutex<bool>, Condvar)>);

impl ReleaseBodies {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    fn release(&self) {
        let (lock, changed) = &*self.0;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        changed.notify_all();
    }

    fn wait(&self) {
        let (lock, changed) = &*self.0;
        let ready = lock.lock().unwrap();
        let (ready, _) = changed
            .wait_timeout_while(ready, WAIT, |ready| !*ready)
            .unwrap();
        assert!(
            *ready,
            "both actual speculative file_edit bodies must enter before release"
        );
    }
}

// A failing fixture assertion must not leave a worker waiting on its test-only rendezvous.
struct ReleaseOnDrop(ReleaseBodies);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct ProviderState {
    root: PathBuf,
    same_function: bool,
    reader: bool,
    bodies: Arc<Mutex<Vec<BodyArrival>>>,
    writes_seen: AtomicUsize,
    own_reads_seen: AtomicUsize,
    reader_saw_base: AtomicBool,
    joined_drivers: Arc<AtomicUsize>,
    second_settled_before_first: AtomicBool,
}

impl ProviderState {
    fn private_image(&self, first: bool) -> String {
        if first {
            SOURCE.replacen("    10\n", "    11\n", 1)
        } else if self.same_function {
            SOURCE.replacen("    10\n", "    12\n", 1)
        } else {
            SOURCE.replacen("    20\n", "    21\n", 1)
        }
    }

    fn assert_live_base(&self) {
        assert_eq!(
            std::fs::read(self.root.join(FILE)).unwrap(),
            SOURCE.as_bytes(),
            "live source must stay byte-identical while any private child remains unsettled"
        );
    }
}

async fn bounded_until(mut predicate: impl FnMut() -> bool, why: &str) {
    tokio::time::timeout(WAIT, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("bounded fixture wait failed: {why}"));
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0, "fixture request ended before its body");
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() <= 512 * 1024, "bounded fixture request");
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec())
                .unwrap()
                .to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("owned provider requests have Content-Length");
            assert!(length <= 512 * 1024, "bounded fixture request body");
            if bytes.len() >= end + 4 + length {
                return (
                    headers,
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap(),
                );
            }
        }
    }
}

fn sse(delta: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}

fn native_call(id: &str, name: &str, args: Value) -> String {
    sse(
        json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}),
        "tool_calls",
    )
}

async fn send_sse(socket: &mut tokio::net::TcpStream, response: &str) {
    let wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    socket.write_all(wire.as_bytes()).await.unwrap();
}

async fn provider_request(
    mut socket: tokio::net::TcpStream,
    state: Arc<ProviderState>,
) -> String {
    let (headers, request) = read_request(&mut socket).await;
    assert!(headers.starts_with("post /chat/completions "));
    assert!(headers.contains(&format!("authorization: bearer {KEY}")));
    assert_eq!(request["model"], MODEL);
    assert_eq!(request["stream"], true);
    let messages = request["messages"].as_array().unwrap();
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .unwrap()
        .to_string();
    assert!(system.contains("<private_overlay>"));
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()
        .to_string();
    // Context packs are prepended to the brief. Pick the final owned marker, not an earlier
    // sibling's blackboard excerpt that happens to contain another marker.
    let label = [("SPEC_FIRST", "first"), ("SPEC_SECOND", "second"), ("SPEC_READER", "reader")]
        .iter()
        .filter_map(|(marker, label)| user.rfind(marker).map(|index| (index, *label)))
        .max_by_key(|(index, _)| *index)
        .expect("request must belong to an owned child")
        .1;
    let reader = label == "reader";
    assert!(!reader || state.reader);
    let first = label == "first";
    let mut names: Vec<_> = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        if reader { vec!["file_read"] } else { vec!["file_edit", "file_read", "file_write"] },
        "exact native capabilities only: no shell, restore, delegation, or role upgrade"
    );
    let last = messages.iter().rev().find(|message| message["role"] == "tool");
    let response = match last {
        None => {
            if reader {
                bounded_until(
                    || state.writes_seen.load(Ordering::SeqCst) == 3,
                    "Argus starts its actual read after both private writes completed",
                )
                .await;
            }
            state.assert_live_base();
            native_call(
                &format!("read-initial-{label}"),
                "file_read",
                json!({"path":state.root.join(FILE)}),
            )
        }
        Some(result) => {
            let text = result["content"].as_str().expect("native text tool result");
            assert!(!text.contains("error:"), "native child body failed: {result}");
            state.assert_live_base();
            match result["tool_call_id"].as_str().unwrap() {
                id if id == format!("read-initial-{label}") => {
                    assert!(text.contains(SOURCE), "every child starts from the same immutable base: {text}");
                    assert!(text.contains("private overlay; not yet integrated"));
                    if reader {
                        assert_eq!(state.writes_seen.load(Ordering::SeqCst), 3);
                        state.reader_saw_base.store(true, Ordering::SeqCst);
                        sse(json!({"content":"SPEC_READER_COMPLETE: observed only the captured base, no private sibling changes."}), "stop")
                    } else {
                        let (old, new) = if first {
                            ("    10\n", "    11\n")
                        } else if state.same_function {
                            ("    10\n", "    12\n")
                        } else {
                            ("    20\n", "    21\n")
                        };
                        native_call(
                            &format!("edit-{label}"),
                            "file_edit",
                            json!({"path":state.root.join(FILE),"old_string":old,"new_string":new}),
                        )
                    }
                }
                id if !reader && id == format!("edit-{label}") => {
                    assert!(text.contains("private overlay; not yet integrated"));
                    state.writes_seen.fetch_or(if first { 1 } else { 2 }, Ordering::SeqCst);
                    native_call(
                        &format!("read-own-{label}"),
                        "file_read",
                        json!({"path":state.root.join(FILE)}),
                    )
                }
                id if !reader && id == format!("read-own-{label}") => {
                    let own = state.private_image(first);
                    assert!(text.contains(&own), "read own writes, never a sibling's overlay: {text}");
                    assert!(!text.contains(&state.private_image(!first)));
                    state.own_reads_seen.fetch_or(if first { 1 } else { 2 }, Ordering::SeqCst);
                    bounded_until(
                        || state.own_reads_seen.load(Ordering::SeqCst) == 3
                            && (!state.reader || state.reader_saw_base.load(Ordering::SeqCst)),
                        "all private readbacks and optional sibling Argus read",
                    )
                    .await;
                    if first {
                        // The join probe runs only after the actual blocking driver JoinHandle
                        // completes. Neither a final wire response nor logical Done is that proof.
                        bounded_until(
                            || state.joined_drivers.load(Ordering::SeqCst) & 2 != 0,
                            "second actual TaskTool driver must join before first receives its final response",
                        )
                        .await;
                        assert_eq!(state.joined_drivers.load(Ordering::SeqCst) & 1, 0);
                        {
                            let bodies = state.bodies.lock().unwrap();
                            for body in bodies.iter() {
                                body.overlay.validate(&body.file, false).expect(
                                    "only the parent seals, after every driver has joined",
                                );
                            }
                        }
                        state.assert_live_base();
                        state.second_settled_before_first.store(true, Ordering::SeqCst);
                    }
                    sse(json!({"content":format!("SPEC_{}_COMPLETE: native private edit and own readback complete; parent owns integration and verification.", label.to_ascii_uppercase())}), "stop")
                }
                unexpected => panic!("unexpected owned provider tool result: {unexpected}"),
            }
        }
    };
    send_sse(&mut socket, &response).await;
    label.to_owned()
}

fn configure_verification(root: &Path, expected: &str) -> String {
    std::fs::create_dir(root.join(".aizen")).unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::write(root.join("expected-base.txt"), SOURCE).unwrap();
    std::fs::write(root.join("expected-merged.txt"), expected).unwrap();
    // Exercise the real parent verify runner without recursively running Cargo under cargo test's
    // build lock, installing a runtime, or calling npm. This is an honest byte-fixture assertion,
    // not a claim that shell comparison is a Rust typecheck or a semantic-serializability proof.
    #[cfg(windows)]
    let command = "(fc /b owned.rs expected-base.txt >nul 2>&1 || fc /b owned.rs expected-merged.txt >nul 2>&1) && type owned.rs >> target/verify-observed.txt";
    #[cfg(not(windows))]
    let command = "(cmp -s owned.rs expected-base.txt || cmp -s owned.rs expected-merged.txt) && cat owned.rs >> target/verify-observed.txt";
    std::fs::write(
        root.join(".aizen/verify.json"),
        json!({"commands":[command],"timeout_secs":5}).to_string(),
    )
    .unwrap();
    crate::agent::mcp::trust_project().unwrap();
    assert_eq!(
        verify_gate::detect_verify_commands(root),
        vec![verify_gate::VerifyCommand::Custom(command.into())],
        "unknown fixture verification must fail, never silently skip"
    );
    command.into()
}

#[allow(clippy::await_holding_lock)] // Fixture HOME, then replay; retained until real workers settle.
async fn real_speculative_tasks(planned: bool, reader: bool, same_function: bool) {
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
        ("AIZEN_MAX_SUBAGENTS", std::ffi::OsStr::new("4")),
        ("AIZEN_SUBAGENT_CALL_SECS", std::ffi::OsStr::new("15")),
    ]);
    let _route = crate::core::config::EnvGuard::unset([
        "AIZEN_MODEL_OWNED_SPECULATION_FIXTURE_BASE_URL",
        "AIZEN_MODEL_OWNED_SPECULATION_FIXTURE_API_KEY",
        "AIZEN_NO_HOOKS",
    ]);
    let _tape = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    crate::llm::replay::disable();
    assert!(hooks::configured().is_none(), "owned HOME must not inherit lifecycle hooks");
    std::fs::write(root.join(FILE), SOURCE).unwrap();
    let merged = SOURCE.replacen("    10\n", "    11\n", 1).replacen("    20\n", "    21\n", 1);
    let verify_command = configure_verification(&root, &merged);
    cfg.enable_verify_gate = true;
    cfg.verify_gate_timeout_secs = 5;
    cfg.harness_check_after_edits = 1;
    cfg.image_config = Some(Arc::new(crate::core::cli_config::CliConfig::default()));
    cfg.deadline = Some(std::time::Instant::now() + Duration::from_secs(45));
    cfg.exec_ctx = cfg.exec_ctx.with_deadline(cfg.deadline);

    let bodies = Arc::new(Mutex::new(Vec::<BodyArrival>::new()));
    let release = ReleaseBodies::new();
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let probe_release = release.clone();
    cfg.exec_ctx = cfg.exec_ctx.with_native_body_probe(builtin::NativeBodyTestProbe::new(move |tool, path| {
        if tool != "file_edit" {
            return;
        }
        let context = crate::core::exec_ctx::current().expect("actual child body context");
        assert!(!context.can_restore_workspace(), "private children cannot restore live source");
        let overlay = context.overlay().expect("real speculative child must use an overlay").clone();
        let file = overlay.resolve(context.placement().unwrap().workspace_root(), path).unwrap();
        tx.send(BodyArrival {
            label: context.dispatch_label().unwrap(),
            path: path.to_path_buf(),
            overlay,
            file,
        })
        .unwrap();
        probe_release.wait();
    }));
    let observed_bodies = bodies.clone();
    let rendezvous_root = root.clone();
    let controller = tokio::spawn(async move {
        let _release_on_return = ReleaseOnDrop(release);
        let mut entered = Vec::new();
        for _ in 0..2 {
            entered.push(tokio::time::timeout(WAIT, rx.recv()).await.expect("both actual overlay mutation bodies must overlap").expect("actual native body sender"));
        }
        assert_eq!(entered[0].path, entered[1].path);
        assert_eq!(entered[0].path, rendezvous_root.canonicalize().unwrap().join(FILE));
        assert_ne!(entered[0].overlay.id(), entered[1].overlay.id());
        assert_eq!(entered[0].overlay.base_id(), entered[1].overlay.base_id());
        assert_ne!(entered[0].overlay.owner(), entered[1].overlay.owner());
        for label in ["owned-first", "owned-second"] {
            assert_eq!(entered.iter().filter(|body| body.label.contains(label)).count(), 1);
        }
        assert_eq!(std::fs::read(rendezvous_root.join(FILE)).unwrap(), SOURCE.as_bytes());
        *observed_bodies.lock().unwrap() = entered;
    });
    let joined_drivers = Arc::new(AtomicUsize::new(0));
    let joined_probe = joined_drivers.clone();
    cfg.exec_ctx = cfg.exec_ctx.with_speculative_driver_join_probe(tools::workers::WorkerJoinTestProbe::new(move |index| {
        joined_probe.fetch_or(1 << index, Ordering::SeqCst);
    }));
    let state = Arc::new(ProviderState {
        root: root.clone(), same_function, reader, bodies: bodies.clone(),
        writes_seen: AtomicUsize::new(0), own_reads_seen: AtomicUsize::new(0),
        reader_saw_base: AtomicBool::new(false), joined_drivers,
        second_settled_before_first: AtomicBool::new(false),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let expected_requests = 8 + if reader { 2 } else { 0 };
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        let mut handlers = Vec::new();
        let mut accept_error = None;
        for _ in 0..expected_requests {
            match tokio::time::timeout(Duration::from_secs(20), listener.accept()).await {
                Ok(Ok((socket, peer))) => {
                    assert!(peer.ip().is_loopback());
                    let state = server_state.clone();
                    handlers.push(tokio::spawn(async move {
                        tokio::time::timeout(Duration::from_secs(25), provider_request(socket, state))
                            .await
                            .expect("bounded owned provider handler")
                    }));
                }
                other => {
                    accept_error = Some(format!("owned provider expected {expected_requests} requests: {other:?}"));
                    break;
                }
            }
        }
        // Never abandon an accepted provider handler, even when another handler failed.
        let mut outcomes = Vec::new();
        for handler in handlers {
            outcomes.push(handler.await);
        }
        (accept_error, outcomes)
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let task = TaskTool::new(client, base.clone(), KEY.into(), MODEL.into(), crate::core::approval::ApprovalMode::Yolo, root.clone(), 0, 0);
    let mut calls = Vec::new();
    for (id, label, marker, role) in [
        ("spec-first", "owned-first", "SPEC_FIRST", "daedalus"),
        ("spec-second", "owned-second", "SPEC_SECOND", "daedalus"),
        ("spec-reader", "owned-reader", "SPEC_READER", "argus"),
    ].into_iter().take(if reader { 3 } else { 2 }) {
        let mut args = json!({"id":id,"label":label,"role":role,"model":MODEL,"execution":"speculative","max_steps":8,
            "prompt":format!("{marker}: Work only on the exact owned.rs fixture through the available native capabilities. Read the captured base; editors make one localized file_edit then explicitly read their own result. Readers only report their file_read result. No shell, discovery, restore, delegation, other files, or claimed verification; the parent integrates and verifies the complete cohort.")});
        args[if role == "argus" { "read_paths" } else { "write_paths" }] = json!([FILE]);
        let dispatch = task.resolve_dispatch(&args);
        assert_eq!(dispatch.base_url, base, "owned child must never route to an external provider");
        assert_eq!(dispatch.api_key, KEY);
        assert_eq!(dispatch.model, MODEL);
        calls.push(call(id, "task", &args.to_string()));
    }
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(task));
    let prepared = if planned { plan_execution_calls(&registry, &calls, &cfg) } else { prepare_execution_calls(&registry, &calls, &cfg) };
    let tool = registry.get("task").unwrap();
    for prepared in &prepared {
        let prepared = prepared.as_ref().unwrap();
        let scope = task_tool::speculative_scope(prepared).expect("central executor speculative lane");
        assert_eq!(scope.resources().len(), 1);
        assert!(prepared.preview.as_ref().unwrap().title.contains("Private"));
        assert!(!tool.is_concurrency_safe_prepared(prepared));
        assert!(tool.parallel_native_scope_prepared(prepared).is_none(), "do not bypass the direct same-file conflict policy");
    }
    let mut sink: Vec<_> = calls.iter().map(|call| Message::tool_result(&call.id, "pending")).collect();
    let (mut checkpointed, mut lease, mut baseline) = (false, None, None);
    // Do not timeout/drop this observer: cfg/HTTP/body/verify deadlines bound the real children,
    // and the central executor must retain containment until their physical settlement is known.
    let records = execute_execution_calls(
        &registry, &calls, &prepared, planned, &cfg, &mut sink, Vec::new(),
        &mut checkpointed, &mut lease, &mut baseline, &[],
    ).await;
    let controller_result = controller.await;
    let server_result = server.await;
    if records.iter().any(|record| record.outcome.effect_state != tools::ToolEffectState::Settled) {
        // Unknown physical settlement is a hard failure. Retain, rather than delete, only this
        // owned fixture; never destroy files underneath a potentially-contained child.
        std::mem::forget(fixture);
        panic!("speculative acceptance worker did not physically settle; owned fixture retained at {}: {:?}", root.display(), records.iter().map(|record| (&record.call_id, record.outcome.effect_state, &record.outcome.observation.text)).collect::<Vec<_>>());
    }
    controller_result.expect("actual-body rendezvous controller");
    let (accept_error, handlers) = server_result.expect("owned provider server");
    assert!(accept_error.is_none(), "{accept_error:?}; child records: {:?}", records.iter().map(|record| &record.outcome.observation.text).collect::<Vec<_>>());
    assert_eq!(handlers.len(), expected_requests);
    let labels: Vec<_> = handlers.into_iter().map(|handler| handler.expect("owned provider handler assertions")).collect();
    assert_eq!(labels.iter().filter(|label| label.as_str() == "first").count(), 4);
    assert_eq!(labels.iter().filter(|label| label.as_str() == "second").count(), 4);
    assert_eq!(labels.iter().filter(|label| label.as_str() == "reader").count(), if reader { 2 } else { 0 });
    assert_eq!(state.writes_seen.load(Ordering::SeqCst), 3);
    assert_eq!(state.own_reads_seen.load(Ordering::SeqCst), 3);
    assert_eq!(state.reader_saw_base.load(Ordering::SeqCst), reader);
    assert!(state.second_settled_before_first.load(Ordering::SeqCst));
    assert_eq!(records.len(), calls.len());
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.call_id, calls[index].id, "input order survives reversed actual settlement");
        assert_eq!(sink[index].tool_call_id.as_deref(), Some(calls[index].id.as_str()));
        let expected_label = ["SPEC_FIRST_COMPLETE", "SPEC_SECOND_COMPLETE", "SPEC_READER_COMPLETE"][index];
        assert!(record.outcome.observation.text.contains(expected_label), "{}", record.outcome.observation.text);
        if same_function {
            let failure = record.outcome.failure.as_deref().expect("same-function edits must conflict");
            assert!(failure.contains("speculative integration conflict"), "{failure}");
            assert!(failure.contains("SameFunction") || failure.contains("Overlap"), "concrete merge conflict: {failure}");
            assert!(!record.changed_workspace(tool), "private conflicts are not live changes");
        } else {
            assert!(record.outcome.failure.is_none(), "{}", record.outcome.observation.text);
            assert!(record.outcome.observation.text.starts_with("[speculative integration verified:"));
            assert!(record.outcome.observation.text.contains("\"state\":\"verified\""));
            assert!(record.outcome.observation.text.contains(&serde_json::to_string(&verify_command).unwrap()));
            assert_eq!(record.changed_workspace(tool), index < 2, "only actual integrated writer paths arm parent edit tracking");
        }
    }
    assert_eq!(std::fs::read(root.join(FILE)).unwrap(), if same_function { SOURCE.as_bytes() } else { merged.as_bytes() });
    assert_eq!(
        std::fs::read(root.join("target/verify-observed.txt")).unwrap(),
        if same_function { SOURCE.to_owned() } else { format!("{SOURCE}{merged}") }.as_bytes(),
        "real baseline then parent verification of the merged live bytes; conflicts never verify as integrated"
    );
    assert!(baseline.is_some(), "actual pre-dispatch verify baseline must run");
    assert!(!checkpointed, "this owned non-git fixture does not create live project checkpoints");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn speculative_configured_hooks_refuse_before_dispatch_even_with_no_hooks_toggle() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (_fixture, mut cfg) = placement_executor_fixture();
    let root = cfg.effective_root();
    let home = cfg.exec_ctx.placement().unwrap().home_root().to_path_buf();
    std::fs::create_dir_all(&home).unwrap();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()), ("AIZEN_PROJECT_ROOT", root.as_os_str()),
    ]);
    let _route = crate::core::config::EnvGuard::unset([
        "AIZEN_MODEL_OWNED_SPECULATION_FIXTURE_BASE_URL", "AIZEN_MODEL_OWNED_SPECULATION_FIXTURE_API_KEY", "AIZEN_NO_HOOKS",
    ]);
    std::fs::write(root.join(FILE), SOURCE).unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    #[cfg(windows)]
    let hook = "echo forbidden> target/hook-ran.txt";
    #[cfg(not(windows))]
    let hook = "printf forbidden > target/hook-ran.txt";
    // A real hook configuration in the isolated HOME, not a mock policy function. Refusal must
    // inspect it without running or silently suppressing a user's lifecycle guard/formatter.
    std::fs::write(home.join("cli-config.json"), json!({"hooks":{"pre_tool":[{"match":"*","run":hook,"timeout_secs":2}]}}).to_string()).unwrap();
    cfg.image_config = Some(Arc::new(crate::core::cli_config::CliConfig::default()));
    cfg.deadline = Some(std::time::Instant::now() + WAIT);
    cfg.exec_ctx = cfg.exec_ctx.with_deadline(cfg.deadline);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(WAIT).build().unwrap();
    let task = TaskTool::new(client, format!("http://{}", listener.local_addr().unwrap()), KEY.into(), MODEL.into(), crate::core::approval::ApprovalMode::Yolo, root.clone(), 0, 0);
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(task));
    let args = json!({"role":"daedalus","model":MODEL,"execution":"speculative","write_paths":[FILE],"max_steps":4,
        "prompt":"Read owned.rs, change only alpha() through the exact native scope, and leave integration and verification to the parent; no shell or other files."});
    let calls = vec![call("hook-refusal", "task", &args.to_string())];
    for disabled in [false, true] {
        let _toggle = disabled.then(|| crate::core::config::EnvGuard::set([("AIZEN_NO_HOOKS", std::ffi::OsStr::new("1"))]));
        assert_eq!(hooks::configured().is_none(), disabled);
        let prepared = prepare_execution_calls(&registry, &calls, &cfg);
        let refusal = match &prepared[0] {
            Err(refusal) => refusal,
            Ok(_) => panic!("configured hooks must refuse before a speculative child is admitted"),
        };
        assert!(refusal.contains("lifecycle hooks are configured"), "{refusal}");
        let mut sink = vec![Message::tool_result("hook-refusal", "pending")];
        let (mut checkpointed, mut lease, mut baseline) = (false, None, None);
        let records = execute_execution_calls(&registry, &calls, &prepared, false, &cfg, &mut sink, Vec::new(), &mut checkpointed, &mut lease, &mut baseline, &[]).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome.effect_state, tools::ToolEffectState::Settled);
        assert!(records[0].outcome.failure.as_deref().unwrap().contains("lifecycle hooks are configured"));
        assert!(!records[0].changed_workspace(registry.get("task").unwrap()));
        assert_eq!(std::fs::read(root.join(FILE)).unwrap(), SOURCE.as_bytes());
        assert!(!root.join("target/hook-ran.txt").exists(), "policy refusal must not execute a hook");
        assert!(!checkpointed && baseline.is_none());
    }
    assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err(), "hook refusal must not contact even the owned loopback provider");
}
