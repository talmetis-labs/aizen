// Real Task/Workflow dispatches against an owned loopback SSE server, never a live provider.

use super::*;
use crate::agent::task_tool::TaskTool;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0, "fixture request ended before body");
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() <= 512 * 1024, "bounded fixture request");
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec())
                .unwrap()
                .to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap();
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

async fn send_sse(socket: &mut tokio::net::TcpStream, response: &str) {
    let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
    socket.write_all(wire.as_bytes()).await.unwrap();
}

fn native_call(id: &str, name: &str, args: Value) -> String {
    sse(
        json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}),
        "tool_calls",
    )
}

/// Inspect only bounded metadata from the owned journal; do not surface source images.
fn journal_metadata(home: &std::path::Path) -> Vec<Value> {
    let bytes = std::fs::read(home.join("native-journal-v1/records.bin")).unwrap();
    assert_eq!(&bytes[..8], b"AZNJ\0\0\0\x01");
    let mut offset = 8;
    fn next_u32(bytes: &[u8], offset: &mut usize) -> u32 {
        let value = u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap());
        *offset += 4;
        value
    }
    let count = next_u32(&bytes, &mut offset);
    assert_eq!(count, 2);
    let mut records = Vec::new();
    for _ in 0..count {
        let length = next_u32(&bytes, &mut offset) as usize;
        records.push(serde_json::from_slice(&bytes[offset..offset + length]).unwrap());
        offset += length;
        for _ in 0..2 {
            let length = next_u32(&bytes, &mut offset);
            if length != u32::MAX {
                offset += length as usize;
            }
        }
    }
    assert_eq!(offset, bytes.len());
    records
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_real_native_tasks_overlap_in_approved_central_batch() {
    real_native_dispatch(false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_real_native_tasks_overlap_at_planning_boundary() {
    real_native_dispatch(true, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_workflow_scoped_children_overlap_in_native_bodies() {
    real_native_dispatch(true, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_same_file_tasks_preserve_committed_postimage_after_approval() {
    real_native_dispatch(false, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_same_file_tasks_preserve_committed_postimage_at_planning_boundary() {
    real_native_dispatch(true, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_verifies_parallel_tasks_before_next_model_turn() {
    real_native_dispatch_case(true, false, false, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_verifies_parallel_workflow_before_next_model_turn() {
    real_native_dispatch_case(true, true, false, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_verifies_settled_edits_even_after_child_provider_failure() {
    real_native_dispatch_case(true, false, false, true, true).await;
}

#[derive(Clone, Copy, Debug)]
enum AccessCase {
    Disjoint,
    SameFile,
    SharedReads,
    CrossRead,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_mixed_roles_at_prepared_task_boundary() {
    for case in [
        AccessCase::Disjoint,
        AccessCase::SameFile,
        AccessCase::SharedReads,
        AccessCase::CrossRead,
    ] {
        real_mixed_access(false, false, case).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_mixed_roles_at_planned_task_boundary() {
    for case in [
        AccessCase::Disjoint,
        AccessCase::SameFile,
        AccessCase::SharedReads,
        AccessCase::CrossRead,
    ] {
        real_mixed_access(true, false, case).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_mixed_roles_in_workflow_native_bodies() {
    for case in [
        AccessCase::Disjoint,
        AccessCase::SameFile,
        AccessCase::SharedReads,
        AccessCase::CrossRead,
    ] {
        real_mixed_access(true, true, case).await;
    }
}

#[allow(clippy::await_holding_lock)] // Fixture-owned HOME, then tape, through actual settlement.
async fn real_mixed_access(planned: bool, workflow: bool, case: AccessCase) {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, mut cfg) = placement_executor_fixture();
    let root = cfg.effective_root();
    let home = fixture.path().join("mixed-home");
    std::fs::create_dir(&home).unwrap();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()),
        ("AIZEN_PROJECT_ROOT", root.as_os_str()),
    ]);
    let _route = crate::core::config::EnvGuard::unset([
        "AIZEN_MODEL_OWNED_TASK_BATCH_FIXTURE_BASE_URL",
        "AIZEN_MODEL_OWNED_TASK_BATCH_FIXTURE_API_KEY",
    ]);
    let _tape = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::llm::replay::disable();
    std::fs::write(root.join("a.txt"), "before-a").unwrap();
    std::fs::write(root.join("b.txt"), "before-b").unwrap();
    let shared = matches!(case, AccessCase::SharedReads);
    let cross = matches!(case, AccessCase::CrossRead);
    let serial = matches!(case, AccessCase::SameFile | AccessCase::CrossRead);
    let reader_file = if serial || shared { "a.txt" } else { "b.txt" };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    if !serial {
        let release = release.clone();
        cfg.exec_ctx = cfg
            .exec_ctx
            .with_native_body_probe(builtin::NativeBodyTestProbe::new(move |tool, path| {
                assert!(matches!(tool, "file_read" | "file_write"));
                tx.send((tool.to_owned(), path.to_path_buf())).unwrap();
                let (lock, changed) = &*release;
                let ready = lock.lock().unwrap();
                let (ready, _) = changed
                    .wait_timeout_while(ready, Duration::from_secs(8), |v| !*v)
                    .unwrap();
                assert!(*ready, "both scoped native bodies must overlap: {case:?}");
            }));
    }
    let rendezvous = if serial {
        None
    } else {
        Some(tokio::spawn(async move {
            let mut entered = Vec::new();
            for _ in 0..2 {
                entered.push(
                    tokio::time::timeout(Duration::from_secs(10), rx.recv())
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
            let (lock, changed) = &*release;
            *lock.lock().unwrap() = true;
            changed.notify_all();
            entered
        }))
    };
    const MODEL: &str = "owned-task-batch-fixture";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let first_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_root = root.clone();
    let server = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for _ in 0..(4 + usize::from(workflow)) {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let root = server_root.clone();
            let first_done = first_done.clone();
            handlers.push(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(12), async move {
                    let (_, request) = read_request(&mut socket).await;
                    assert_eq!(request["model"], MODEL);
                    let names = request["tools"].as_array();
                    if names.is_none_or(Vec::is_empty) {
                        assert!(workflow);
                        let body = json!({"choices":[{"message":{"role":"assistant","content":"Owned mixed-role fixture settled."},"finish_reason":"stop"}]}).to_string();
                        let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        socket.write_all(wire.as_bytes()).await.unwrap();
                        return;
                    }
                    let messages = request["messages"].as_array().unwrap();
                    let first = messages.iter().find(|m| m["role"] == "user").unwrap().to_string().contains("FIRST_NATIVE");
                    let writable = (first && !shared) || (!first && cross);
                    let mut names: Vec<_> = names.unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
                    names.sort();
                    assert_eq!(names, if writable { vec!["file_edit", "file_read", "file_write"] } else { vec!["file_read"] });
                    let last = messages.iter().rev().find(|m| m["role"] == "tool");
                    let response = if let Some(result) = last {
                        assert!(!result.to_string().contains("error:"), "native tool failed: {result}");
                        if !writable {
                            let expected = if serial { "after-a" } else if shared { "before-a" } else { "before-b" };
                            assert!(result.to_string().contains(expected), "reader must see committed contents: {result}");
                        }
                        if first { first_done.store(true, std::sync::atomic::Ordering::SeqCst); }
                        sse(json!({"content":"Owned exact operation complete; parent retains verification."}), "stop")
                    } else {
                        if serial && !first {
                            assert!(first_done.load(std::sync::atomic::Ordering::SeqCst), "conflicting child must not even start its model loop early");
                            assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "after-a");
                        }
                        let file = if first { "a.txt" } else if cross { "b.txt" } else { reader_file };
                        if writable {
                            let content = if first { "after-a" } else { "after-b" };
                            native_call("owned-write", "file_write", json!({"path":root.join(file),"purpose":"project","content":content}))
                        } else {
                            native_call("owned-read", "file_read", json!({"path":root.join(file)}))
                        }
                    };
                    send_sse(&mut socket, &response).await;
                }).await.expect("bounded mixed-role provider handler");
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let mut args = vec![
        json!({"id":"first","role":"daedalus","model":MODEL,"max_steps":4,
            "prompt":"FIRST_NATIVE: Work only on the controller-approved owned a.txt fixture. Use the available native file capability exactly once, then report the result and leave combined verification to the parent. Never use shell, delegation, restore or other files."}),
        json!({"id":"reader","role":"argus","model":MODEL,"max_steps":4,"read_paths":[reader_file],
            "prompt":"SECOND_NATIVE: Read only the exact controller-approved fixture file using file_read. Report the observed contents without changing anything. No discovery, shell, delegation, restore or other files; this is a bounded read-only task."}),
    ];
    args[0][if shared { "read_paths" } else { "write_paths" }] = json!(["a.txt"]);
    if cross {
        // Writes are disjoint, but each editor reads the other's file: this is not parallel-safe.
        args[0]["read_paths"] = json!(["b.txt"]);
        args[1]["role"] = json!("daedalus");
        args[1]["write_paths"] = json!(["b.txt"]);
        args[1]["prompt"] = json!("SECOND_NATIVE: Write after-b to the exact approved b.txt fixture. The controller also grants read access to a.txt; leave combined verification to the parent. No discovery, shell, delegation, restore or unapproved files.");
    }
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(TaskTool::new(
        client.clone(),
        base.clone(),
        "owned-fixture".into(),
        MODEL.into(),
        crate::core::approval::ApprovalMode::Yolo,
        root.clone(),
        0,
        0,
    )));
    let calls = if workflow {
        registry.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            client,
            base,
            "owned-fixture".into(),
            MODEL.into(),
            crate::core::approval::ApprovalMode::Yolo,
            0,
            root.clone(),
            0,
        )));
        vec![call(
            "mixed-workflow",
            "workflow",
            &json!({"mode":"fanout","tasks":args}).to_string(),
        )]
    } else {
        args.iter()
            .enumerate()
            .map(|(i, args)| call(&format!("mixed-{i}"), "task", &args.to_string()))
            .collect()
    };
    cfg.image_config = Some(Arc::new(crate::core::cli_config::CliConfig::default()));
    cfg.deadline = Some(std::time::Instant::now() + Duration::from_secs(25));
    cfg.exec_ctx = cfg.exec_ctx.with_deadline(cfg.deadline);
    let prepared = if planned {
        plan_execution_calls(&registry, &calls, &cfg)
    } else {
        prepare_execution_calls(&registry, &calls, &cfg)
    };
    assert!(
        prepared.iter().all(Result::is_ok),
        "{case:?}: {:?}",
        prepared
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>()
    );
    let mut sink: Vec<_> = calls
        .iter()
        .map(|c| Message::tool_result(&c.id, "pending"))
        .collect();
    let (mut checkpointed, mut lease, mut baseline) = (false, None, None);
    let records = execute_execution_calls(
        &registry,
        &calls,
        &prepared,
        planned,
        &cfg,
        &mut sink,
        Vec::new(),
        &mut checkpointed,
        &mut lease,
        &mut baseline,
        &[],
    )
    .await;
    server.await.unwrap();
    if let Some(rendezvous) = rendezvous {
        let entered = rendezvous.await.unwrap();
        assert_eq!(entered.len(), 2);
        assert_eq!(
            entered.iter().filter(|(t, _)| t == "file_write").count(),
            usize::from(!shared)
        );
    }
    assert_eq!(records.len(), calls.len());
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.call_id, calls[index].id);
        assert_eq!(record.outcome.effect_state, tools::ToolEffectState::Settled);
        assert!(
            record.outcome.failure.is_none(),
            "{}",
            record.outcome.observation.text
        );
        let tool = registry
            .get(if workflow { "workflow" } else { "task" })
            .unwrap();
        assert_eq!(
            record.changed_workspace(tool),
            !shared && (workflow || index == 0 || cross)
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        if shared { "before-a" } else { "after-a" }
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        if cross { "after-b" } else { "before-b" }
    );
    assert!(!checkpointed);
    assert!(baseline.is_none());
}

async fn real_native_dispatch(bind_at_boundary: bool, workflow: bool, same_file: bool) {
    real_native_dispatch_case(bind_at_boundary, workflow, same_file, false, false).await;
}

#[allow(clippy::await_holding_lock)] // Owned HOME, then tape; held until all real workers settle.
async fn real_native_dispatch_case(
    bind_at_boundary: bool,
    workflow: bool,
    same_file: bool,
    parent_verify: bool,
    fail_after_edit: bool,
) {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fixture, mut c) = placement_executor_fixture();
    let root = c.effective_root();
    let home = fixture.path().join("owned-task-home");
    std::fs::create_dir(&home).unwrap();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_os_str()),
        ("AIZEN_PROJECT_ROOT", root.as_os_str()),
    ]);
    let _route_env = crate::core::config::EnvGuard::unset([
        "AIZEN_MODEL_OWNED_TASK_BATCH_FIXTURE_BASE_URL",
        "AIZEN_MODEL_OWNED_TASK_BATCH_FIXTURE_API_KEY",
    ]);
    let _tape = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::llm::replay::disable();
    std::fs::write(root.join("a.txt"), "before").unwrap();
    assert!(!root.join("b.txt").exists());
    if parent_verify {
        std::fs::create_dir(root.join(".aizen")).unwrap();
        std::fs::write(
            root.join("verification-fixture.rs"),
            "// Owned stable fingerprint input.\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        #[cfg(windows)]
        let command = "type a.txt >> target/verify-order.txt & echo. >> target/verify-order.txt & if exist b.txt (type b.txt >> target/verify-order.txt & echo. >> target/verify-order.txt)";
        #[cfg(not(windows))]
        let command = "cat a.txt >> target/verify-order.txt; printf '\\n' >> target/verify-order.txt; if test -f b.txt; then cat b.txt >> target/verify-order.txt; printf '\\n' >> target/verify-order.txt; fi";
        std::fs::write(
            root.join(".aizen/verify.json"),
            json!({"commands":[command]}).to_string(),
        )
        .unwrap();
        crate::agent::mcp::trust_project().unwrap(); // Only the fixture home/root.
        c.enable_verify_gate = true;
        c.harness_check_after_edits = 1;
    }
    const MODEL: &str = "owned-task-batch-fixture";
    const KEY: &str = "owned-task-account";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let arrival = Arc::new(tokio::sync::Barrier::new(2));
    let body_controller = if !same_file {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let probe_release = release.clone();
        c.exec_ctx = c
            .exec_ctx
            .with_native_body_probe(builtin::NativeBodyTestProbe::new(move |tool, path| {
                if !matches!(tool, "file_edit" | "file_write") {
                    return;
                }
                tx.send((tool.to_owned(), path.to_path_buf())).unwrap();
                let (lock, changed) = &*probe_release;
                let ready = lock.lock().unwrap();
                let (ready, _) = changed
                    .wait_timeout_while(ready, Duration::from_secs(8), |ready| !*ready)
                    .unwrap();
                assert!(
                    *ready,
                    "both actual native mutation bodies must enter before release"
                );
            }));
        Some(tokio::spawn(async move {
            let mut entered = Vec::new();
            for _ in 0..2 {
                entered.push(
                    tokio::time::timeout(Duration::from_secs(10), rx.recv())
                        .await
                        .expect("bounded actual-body rendezvous")
                        .expect("native body sender"),
                );
            }
            let (lock, changed) = &*release;
            *lock.lock().unwrap() = true;
            changed.notify_all();
            entered.sort();
            entered
        }))
    } else {
        None
    };
    let server_root = root.clone();
    let server = tokio::spawn(async move {
        let mut handlers = Vec::new();
        // Existing file: read/edit/report; creation: write/report; same file: two read/edit/report.
        for _ in 0..(5 + usize::from(same_file) + usize::from(workflow)) {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .expect("bounded fixture accept")
                .unwrap();
            let arrival = arrival.clone();
            let root = server_root.clone();
            handlers.push(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(12), async move {
                    let (headers, request) = read_request(&mut socket).await;
                    assert!(headers.starts_with("post /chat/completions "));
                    assert!(headers.contains(&format!("authorization: bearer {KEY}")));
                    assert_eq!(request["model"], MODEL);
                    if workflow && request["tools"].as_array().is_none_or(|tools| tools.is_empty()) {
                        assert_ne!(request["stream"], true, "synthesis uses the non-streaming wire contract");
                        let response = json!({"choices":[{"message":{"role":"assistant","content":"Both owned files changed; parent verifies."},"finish_reason":"stop"}]}).to_string();
                        let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
                        socket.write_all(wire.as_bytes()).await.unwrap();
                        return ("synthesis".to_owned(), "done".to_owned());
                    }
                    let mut names: Vec<_> = request["tools"].as_array().unwrap().iter()
                        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned()).collect();
                    names.sort();
                    assert_eq!(names, ["file_edit", "file_read", "file_write"]);
                    let messages = request["messages"].as_array().unwrap();
                    let user = messages.iter().find(|m| m["role"] == "user").unwrap().to_string();
                    let a = user.contains("OWNED_A");
                    assert!(a || user.contains("OWNED_B"));
                    let label = if a { "a" } else { "b" };
                    let file = if a || same_file { "a.txt" } else { "b.txt" };
                    let before = if a { "before" } else { "after-a.txt" };
                    let after = if !a && same_file { "after-a.txt+b" } else if a { "after-a.txt" } else { "after-b.txt" };
                    let last = messages.iter().rev().find(|m| m["role"] == "tool");
                    let (stage, response) = match last {
                        None => {
                            if !same_file {
                                tokio::time::timeout(Duration::from_secs(8), arrival.wait()).await
                                    .expect("both real child loops must overlap before response");
                            } else {
                                assert_eq!(std::fs::read_to_string(root.join(file)).unwrap(), before,
                                    "second child starts only after first child's committed postimage");
                            }
                            if a || same_file {
                                ("read", native_call(&format!("read-{label}"), "file_read", json!({"path":root.join(file)})))
                            } else {
                                ("mutate", native_call(&format!("mutate-{label}"), "file_write", json!({"path":root.join(file),"purpose":"project","content":after})))
                            }
                        }
                        Some(result) if result["tool_call_id"] == format!("read-{label}") => {
                            assert!(!result.to_string().contains("error:"), "read must succeed: {result}");
                            assert!(result.to_string().contains(before), "child reads committed contents: {result}");
                            ("mutate", native_call(&format!("mutate-{label}"), "file_edit", json!({"path":root.join(file),"old_string":before,"new_string":after})))
                        }
                        Some(result) => {
                            assert_eq!(result["tool_call_id"], format!("mutate-{label}"));
                            assert!(!result.to_string().contains("error:"), "native body must succeed: {result}");
                            assert_eq!(std::fs::read_to_string(root.join(file)).unwrap(), after);
                            if fail_after_edit && a {
                                let body = json!({"error":{"message":"Owned fixture failure after committed native edit","type":"invalid_request_error"}}).to_string();
                                let wire = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                socket.write_all(wire.as_bytes()).await.unwrap();
                                return (label.to_owned(), "failed-after-edit".to_owned());
                            }
                            ("done", sse(json!({"content":format!("Finished owned file {file}; parent verification remains required.")}), "stop"))
                        }
                    };
                    send_sse(&mut socket, &response).await;
                    (label.to_owned(), stage.to_owned())
                }).await.expect("bounded owned SSE handler")
            }));
        }
        let mut receipts = Vec::new();
        for handler in handlers {
            receipts.push(handler.await.unwrap());
        }
        receipts.sort();
        receipts
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let task = TaskTool::new(
        client.clone(),
        base.clone(),
        KEY.into(),
        MODEL.into(),
        crate::core::approval::ApprovalMode::Yolo,
        root.clone(),
        0,
        0,
    );
    let mut r = ToolRegistry::new();
    let mut calls = Vec::new();
    let mut task_args = Vec::new();
    for (id, marker, file) in [
        ("task-a", "OWNED_A", "a.txt"),
        (
            "task-b",
            "OWNED_B",
            if same_file { "a.txt" } else { "b.txt" },
        ),
    ] {
        let args = json!({"id":id,"role":"daedalus","model":MODEL,"max_steps":6,"write_paths":[file],
            "prompt":format!("{marker}: Update only the controller-approved owned fixture file {file} using native tools. Read existing contents before an exact file_edit, or create b.txt with file_write. Then report changed files and leave verification to the parent. No shell commands or other files."),
            "boundaries":"Only selected exact file; no shell, restore, undo, or delegation."});
        let dispatch = task.resolve_dispatch(&args);
        assert_eq!(dispatch.base_url, base);
        assert_eq!(dispatch.api_key, KEY);
        assert_eq!(dispatch.model, MODEL);
        calls.push(call(id, "task", &args.to_string()));
        task_args.push(args);
    }
    r.register(Box::new(task));
    if workflow {
        r.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            client,
            base,
            KEY.into(),
            MODEL.into(),
            crate::core::approval::ApprovalMode::Yolo,
            0,
            root.clone(),
            0,
        )));
        calls = vec![call(
            "workflow",
            "workflow",
            &json!({"mode":"fanout","tasks":task_args}).to_string(),
        )];
    }
    c.image_config = Some(Arc::new(crate::core::cli_config::CliConfig::default()));
    c.deadline = Some(std::time::Instant::now() + Duration::from_secs(25));
    c.exec_ctx = c.exec_ctx.with_deadline(c.deadline);
    let prepared = if bind_at_boundary {
        plan_execution_calls(&r, &calls, &c)
    } else {
        prepare_execution_calls(&r, &calls, &c)
    };
    let tool = r.get(if workflow { "workflow" } else { "task" }).unwrap();
    let mut keys = std::collections::HashSet::new();
    for prepared in &prepared {
        let prepared = prepared.as_ref().unwrap();
        assert!(tool.is_destructive_prepared(prepared));
        assert_eq!(tool.delegation_read_only_prepared(prepared), Some(false));
        if workflow {
            assert!(
                !tool.is_concurrency_safe_prepared(prepared),
                "outer writer workflow remains a barrier"
            );
            assert!(tool.parallel_write_keys_prepared(prepared).is_none());
        } else {
            assert!(prepared.preview.is_some());
            let owned = tool.parallel_write_keys_prepared(prepared).unwrap();
            assert_eq!(owned.len(), 1);
            if !same_file {
                assert!(keys.insert(owned[0].clone()));
            }
            let mut image_call = prepared.clone();
            image_call.args["return_evidence"] = json!(["capture"]);
            assert!(tool.parallel_write_keys_prepared(&image_call).is_none());
        }
    }
    if parent_verify {
        let turns = std::sync::atomic::AtomicUsize::new(0);
        let parent_calls = calls.clone();
        let verify_root = root.clone();
        let chat = move |messages: Vec<Message>, _: Vec<ToolDef>| {
            let turn = turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let next = if turn == 1 {
                ChatTurn {
                    content: None,
                    tool_calls: parent_calls.clone(),
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                    empty_wire_note: None,
                    empty_wire_raw: None,
                    eager: Vec::new(),
                }
            } else {
                assert_eq!(
                    turn, 2,
                    "parent needs no extra verify turn after the actual-edit check"
                );
                let phases: Vec<_> =
                    std::fs::read_to_string(verify_root.join("target/verify-order.txt"))
                        .unwrap()
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(str::to_owned)
                        .collect();
                assert_eq!(phases, ["before", "after-a.txt", "after-b.txt"], "baseline precedes child writes, combined verification precedes the next model call");
                let check = messages
                    .iter()
                    .filter_map(|message| message.content.as_deref())
                    .find(|text| text.contains("[harness check]"))
                    .expect("actual delegated edits must trigger immediate parent verification");
                assert!(
                    check.contains("passed after this edit batch"),
                    "owned parent verification observation: {check}"
                );
                if fail_after_edit {
                    assert!(messages.iter().any(|message| message.role == "tool"
                        && message
                            .content
                            .as_deref()
                            .is_some_and(|text| text.contains("error:"))));
                }
                final_turn("Parent verified the settled combined tree")
            };
            std::future::ready(Ok(next))
        };
        let mut messages = vec![
            Message::system("owned parent fixture"),
            Message::user("Apply these independent scoped changes and verify the combined tree"),
        ];
        let outcome = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(outcome.stop, StopReason::Done);
        assert_eq!(
            outcome.final_text.as_deref(),
            Some("Parent verified the settled combined tree")
        );
        assert_valid_history(&messages);
    }
    let mut sink: Vec<_> = calls
        .iter()
        .map(|call| Message::tool_result(&call.id, "pending"))
        .collect();
    let mut checkpointed = false;
    let mut lease = None;
    let mut baseline = None;
    let records = if parent_verify {
        Vec::new()
    } else {
        execute_execution_calls(
            &r,
            &calls,
            &prepared,
            bind_at_boundary,
            &c,
            &mut sink,
            Vec::new(),
            &mut checkpointed,
            &mut lease,
            &mut baseline,
            &[],
        )
        .await
    };
    let receipts = server.await.unwrap();
    assert_eq!(
        receipts.len(),
        5 + usize::from(same_file) + usize::from(workflow)
    );
    if let Some(controller) = body_controller {
        let entered = controller.await.unwrap();
        let canonical_root = root.canonicalize().unwrap();
        assert_eq!(
            entered,
            [
                ("file_edit".into(), canonical_root.join("a.txt")),
                ("file_write".into(), canonical_root.join("b.txt"))
            ]
        );
    }
    assert_eq!(records.len(), if parent_verify { 0 } else { calls.len() });
    for (index, (record, call)) in records.iter().zip(&calls).enumerate() {
        assert_eq!(record.call_id, call.id);
        assert_eq!(record.outcome.effect_state, tools::ToolEffectState::Settled);
        assert!(
            record.outcome.failure.is_none(),
            "{}",
            record.outcome.observation.text
        );
        assert!(
            record.changed_workspace(tool),
            "actual child changes must arm the parent"
        );
        let mut paths = prepared_edit_paths(&root, tool.name(), record.prepared.as_ref().unwrap());
        paths.sort();
        let canonical_root = root.canonicalize().unwrap();
        let expected = if workflow {
            vec![canonical_root.join("a.txt"), canonical_root.join("b.txt")]
        } else {
            vec![canonical_root.join(if index == 0 || same_file {
                "a.txt"
            } else {
                "b.txt"
            })]
        };
        assert_eq!(
            paths, expected,
            "per-dispatch exact changes must not leak from siblings"
        );
    }
    assert!(
        !checkpointed,
        "scoped children must not create whole-worktree checkpoints"
    );
    assert!(
        baseline.is_none(),
        "this fixture disables parent verification"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        if same_file {
            "after-a.txt+b"
        } else {
            "after-a.txt"
        }
    );
    if !same_file {
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "after-b.txt"
        );
    }
    let metadata = journal_metadata(&home);
    let mut owners = std::collections::HashSet::new();
    for record in metadata {
        assert_eq!(record["state"], "Settled");
        assert_eq!(record["outcome"], "Applied");
        let owner = record["owner"].as_str().unwrap();
        let lane = if workflow { "workflow" } else { "task" };
        assert!(owner.starts_with(&format!("{}/{lane}/", c.exec_ctx.resource_scope())));
        assert!(owner.ends_with(if workflow { "/1" } else { "/attempt/1" }));
        assert!(owners.insert(owner.to_owned()));
        let path: std::path::PathBuf =
            serde_json::from_value(record["binding"]["path"].clone()).unwrap();
        assert_eq!(
            record["before"]["exists"],
            same_file || path.file_name().unwrap() == "a.txt"
        );
        assert!(path.starts_with(root.canonicalize().unwrap()));
        let child = task_tool::child_execution_context(&c.exec_ctx, owner, &root, "fixture");
        assert!(!child.can_restore_workspace());
        let resource = crate::core::workspace_txn::NativeResource::resolve(&path).unwrap();
        assert!(
            crate::core::native_journal::undo_owner(&child, &resource, owner).is_err(),
            "journal refuses actual child undo authority before accessing storage"
        );
    }
}
