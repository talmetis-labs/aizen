//! One real owned child, a loopback provider, exact native commit, then an explicit parked handoff.
use super::*;
use crate::agent::task_tool::SubagentGate;
use crate::core::{
    convo::ConversationId,
    file_placement::{tests::TestDir, PlacementContext},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() < 512 * 1024);
        if let Some(end) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let len = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            if bytes.len() >= end + 4 + len {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap();
            }
        }
    }
}
async fn send(socket: &mut tokio::net::TcpStream, delta: Value, finish: &str) {
    let body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    );
    let wire=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    socket.write_all(wire.as_bytes()).await.unwrap();
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new(crate::core::gitx::git_exe().unwrap())
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn owned_workflow_child_commits_in_its_root_and_parks_unverified() -> Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
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
    let temp = TestDir::new();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    let root = root.canonicalize()?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "Fixture"]);
    git(&root, &["config", "user.email", "fixture@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("tracked.txt"), b"base\n")?;
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-qm", "base"]);
    let before = git(&root, &["rev-parse", "HEAD"]);
    let index = std::fs::read(root.join(".git/index"))?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let server = tokio::spawn(async move {
        for stage in 0..3 {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(90), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let request = request(&mut socket).await;
            let names = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert!(names
                .iter()
                .all(|name| ["file_read", "file_edit", "file_write"].contains(name)));
            let delta = match stage {
                0 => {
                    json!({"tool_calls":[{"index":0,"id":"owned-read","type":"function","function":{"name":"file_read","arguments":json!({"path":"tracked.txt"}).to_string()}}]})
                }
                1 => {
                    let text = request["messages"].to_string();
                    assert!(text.contains("base"), "{text}");
                    json!({"tool_calls":[{"index":0,"id":"owned-edit","type":"function","function":{"name":"file_edit","arguments":json!({"path":"tracked.txt","old_string":"base","new_string":"owned child"}).to_string()}}]})
                }
                _ => {
                    let tools = request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .collect::<Vec<_>>();
                    assert_eq!(tools.len(), 2);
                    assert!(tools.iter().all(|message| !message["content"]
                        .as_str()
                        .unwrap()
                        .starts_with("error:")));
                    assert!(tools[1]["content"]
                        .as_str()
                        .unwrap()
                        .starts_with("edited tracked.txt"));
                    json!({"content":"Updated tracked.txt in the owned checkout. Parent verification is required."})
                }
            };
            send(
                &mut socket,
                delta,
                if stage == 2 { "stop" } else { "tool_calls" },
            )
            .await;
        }
    });
    let parent = exec_ctx::ExecutionContext::new(ConversationId::new("workflow-fixture"))
        .with_placement(PlacementContext::new(root.clone(), home.clone())?);
    let mut task=WorkflowTask { id:"owned".into(),role:"daedalus".into(),isolation:WorkflowIsolation::Worktree,
        write_paths:Some(vec!["tracked.txt".into()]),max_steps:Some(4),prompt:"Update only tracked.txt from base to owned child using the exact native edit tools. Return changed paths and leave verification to the coordinating parent.".into(),..Default::default() };
    task.prepared = Some(Arc::new(exec_ctx::with_current(parent.clone(), || {
        resolve_workflow_dispatch(
            &root,
            "2026-11-15",
            &task,
            &url,
            "fixture",
            "fixture-model",
            &parent,
        )
    })?));
    exec_ctx::with_current(parent.clone(), || freeze_base(&root, &mut task))?;
    static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let gate = SubagentGate::isolated(&ACTIVE, 1, home.join("slots"));
    let slot = gate.try_acquire().unwrap();
    let outcome = run(
        &reqwest::Client::new(),
        &url,
        "fixture",
        "fixture-model",
        crate::core::approval::ApprovalMode::Yolo,
        &root,
        "2026-11-15",
        &task,
        None,
        &git_ops::OwnedTaskAllocator::new(&crate::core::cancel::TurnCancel::new()),
        0,
        &parent,
        slot,
    )
    .await;
    if outcome.git_handoff.is_some() {
        server.await?;
    } else {
        server.abort();
    }
    assert_eq!(outcome.status, "git-handoff-pending", "{}", outcome.summary);
    let handoff = outcome.git_handoff.unwrap();
    assert_ne!(handoff.root, root);
    assert_eq!(handoff.base.as_str(), before);
    assert!(!handoff.verified && !handoff.integrated);
    assert_eq!(std::fs::read(root.join("tracked.txt"))?, b"base\n");
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), before);
    assert_eq!(std::fs::read(root.join(".git/index"))?, index);
    assert_eq!(
        std::fs::read(handoff.root.join("tracked.txt"))?,
        b"owned child\n"
    );
    assert_eq!(
        git(&handoff.root, &["rev-parse", "HEAD"]),
        handoff.tip.as_str()
    );
    // Successful controller Git work must not give the child opaque process authority.
    let scope = crate::core::native_scope::NativeWriteScope::resolve(
        &handoff.root,
        &["tracked.txt".into()],
    )?;
    let placement = parent
        .placement()
        .unwrap()
        .fork_for(handoff.root.clone(), &handoff.owner)?;
    let native = parent
        .with_resource_scope(&handoff.owner)
        .with_placement(placement)
        .with_native_scope(scope)?;
    let error =
        exec_ctx::with_current(native, || git_ops::workspace_identity(&handoff.root)).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("opaque bounded execution is unavailable inside native scope"),
        "{error:#}"
    );
    assert_eq!(parent.placement().unwrap().project_root(), root);
    assert!(parent.native_scope().is_none());
    assert_eq!(ACTIVE.load(std::sync::atomic::Ordering::SeqCst), 0);
    Ok(())
}
