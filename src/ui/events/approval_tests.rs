//! Process-isolated tests of the real stream-json reader and approval entry points.

use super::*;
use crate::sandbox::request::SandboxRequest;
use crate::sandbox::{runner, CommandOrigin};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

const HELPER: &str = "ui::events::approval_integration_tests::approval_stdio_helper";
const ROOT_ENV: &str = "AIZEN_APPROVAL_STDIO_TEST_ROOT";

fn report(name: &str, allowed: bool) {
    write_line(
        &json!({"type":"fixture_approval_result","name":name,"allowed":allowed}).to_string(),
    );
}

fn request_worker(
    name: &'static str,
    cancel: crate::core::cancel::TurnCancel,
) -> std::thread::JoinHandle<()> {
    let ctx = crate::core::exec_ctx::ExecutionContext::default()
        .with_deadline(Some(Instant::now() + Duration::from_secs(15)));
    std::thread::spawn(move || {
        crate::core::cancel::with_current(cancel, || {
            crate::core::exec_ctx::with_current(ctx, || {
                report(
                    name,
                    ask_approval_once(
                        name,
                        &json!({"fixture":name}),
                        Some(name),
                        None,
                        &ApprovalIdentity::default(),
                    ),
                );
            });
        });
    })
}

#[test]
fn approval_stdio_helper() -> anyhow::Result<()> {
    if !std::env::args().any(|arg| arg == HELPER) {
        return Ok(());
    }
    let Some(root) = std::env::var_os(ROOT_ENV) else {
        return Ok(());
    };
    let root = std::fs::canonicalize(root)?;
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    anyhow::ensure!(
        root.starts_with(&temp) && root != temp,
        "owned fixture root required"
    );
    let scenario = std::env::var("AIZEN_APPROVAL_STDIO_TEST_SCENARIO")?;
    if scenario == "tui_grants" {
        crate::ui::tui::tests::assert_session_allow_lifecycle();
        crate::ui::tui::tests::assert_approval_publication_order();
        write_line(&json!({"type":"fixture_done"}).to_string());
        return Ok(());
    }
    enable(true);
    note_stdin_piped(scenario != "console_cancel");
    start_stdin_reader();
    if matches!(scenario.as_str(), "identical_deny" | "identical_allow") {
        let worker = |label: &'static str, abandon| {
            let root = root.clone();
            std::thread::spawn(move || -> anyhow::Result<()> {
                let allowed = crate::agent::approval_identity_fixture(&root, label, abandon)?;
                report(label, allowed);
                Ok(())
            })
        };
        let first = worker("a", false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock().waiting().is_empty() {
            anyhow::ensure!(
                Instant::now() < deadline,
                "first identical request not published"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let second = worker("b", false);
        first.join().unwrap()?;
        second.join().unwrap()?;
        worker("abandoned", true).join().unwrap()?;
        worker("cancelled", false).join().unwrap()?;
        worker("later", false).join().unwrap()?;
        write_line(&json!({"type":"fixture_done"}).to_string());
        return Ok(());
    }
    if scenario == "console_cancel" {
        let cancel = crate::core::cancel::TurnCancel::new();
        let first = request_worker("a", cancel.clone());
        // Wait for actual publication, not an elapsed-time assumption about thread scheduling.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !lock().waiting().is_empty() {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "first console request not published"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let second = request_worker("b", crate::core::cancel::TurnCancel::new());
        while !root.join("cancel-now").is_file() {
            anyhow::ensure!(
                Instant::now() < deadline,
                "driver never cancelled its request"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        first.join().unwrap();
        second.join().unwrap();
        anyhow::ensure!(
            !allow_all(),
            "late cancelled reply created a reusable grant"
        );
        ALLOW_ALL.store(true, Ordering::Relaxed);
        crate::core::cancel::with_current(cancel, || {
            report(
                "cancelled_allow_all",
                ask_approval(
                    "cancelled",
                    &json!({}),
                    None,
                    None,
                    &ApprovalIdentity::default(),
                ),
            );
        });
        ALLOW_ALL.store(false, Ordering::Relaxed);
    } else {
        let first = request_worker("a", crate::core::cancel::TurnCancel::new());
        let second = request_worker("b", crate::core::cancel::TurnCancel::new());
        first.join().unwrap();
        second.join().unwrap();
    }
    if scenario != "fast" {
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock().stdin_reader != StdinReader::Closed {
            anyhow::ensure!(
                Instant::now() < deadline,
                "stdin did not reach terminal closure"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        report(
            "after_eof",
            ask_approval_once(
                "after_eof",
                &json!({}),
                None,
                None,
                &ApprovalIdentity::default(),
            ),
        );
        anyhow::ensure!(
            lock().waiting().is_empty(),
            "closed reader retained waiters"
        );
    }
    write_line(&json!({"type":"fixture_done"}).to_string());
    Ok(())
}

struct Driver {
    child: Child,
    containment: crate::core::proctree::Containment,
    prepared: runner::Sandboxed<std::process::Command>,
    stdin: Option<ChildStdin>,
    records: Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
    settled: bool,
}

impl Driver {
    fn start(root: &Path, home: &Path, scenario: &str) -> anyhow::Result<Self> {
        let request = SandboxRequest::exec(
            CommandOrigin::InternalTrusted("approval stdio fixture"),
            std::env::current_exe()?,
            vec![
                "--exact".into(),
                HELPER.into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            root.to_path_buf(),
            root.to_path_buf(),
        )
        .private_tmp(false)
        .background(false)
        .extra_env(vec![
            (ROOT_ENV.into(), root.to_str().unwrap().into()),
            ("AIZEN_HOME".into(), home.to_str().unwrap().into()),
            ("AIZEN_APPROVAL_STDIO_TEST_SCENARIO".into(), scenario.into()),
        ]);
        let mut prepared = runner::prepare_std(request)?;
        prepared
            .command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let (mut child, containment) = prepared.spawn_contained()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, records) = channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Some(start) = line.find('{') else {
                    continue;
                };
                if let Ok(value) = serde_json::from_str::<Value>(&line[start..]) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            containment,
            prepared,
            stdin,
            records,
            reader: Some(reader),
            settled: false,
        })
    }

    fn next(&self) -> anyhow::Result<Value> {
        match self.records.recv_timeout(Duration::from_secs(10)) {
            Ok(record) => Ok(record),
            Err(error) => {
                anyhow::bail!("owned approval helper did not produce its next record: {error}")
            }
        }
    }

    fn reply(&mut self, id: &str, behavior: &str, widen: bool) -> anyhow::Result<()> {
        let updates = if widen {
            json!([{"type":"setMode","mode":"bypassPermissions"}])
        } else {
            json!([])
        };
        self.send(json!({"type":"control_response","response":{"request_id":id,"subtype":"success","response":{"behavior":behavior,"updatedPermissions":updates}}}))
    }

    fn send(&mut self, record: Value) -> anyhow::Result<()> {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{record}")?;
        stdin.flush()?;
        Ok(())
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            anyhow::ensure!(Instant::now() < deadline, "owned helper did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        self.prepared.finish(runner::Outcome::Exit(status.code()));
        anyhow::ensure!(status.success(), "owned approval helper failed: {status}");
        #[cfg(windows)]
        while self.containment.known_quiescent()? != Some(true) {
            anyhow::ensure!(
                Instant::now() < deadline,
                "owned approval helper tree did not settle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.settled = true;
        self.reader.take().unwrap().join().unwrap();
        Ok(())
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.stdin.take();
        if !self.settled {
            // Only this fixture-owned contained process is eligible for failure cleanup.
            crate::core::proctree::kill_tree(&mut self.child, &self.containment);
            self.prepared.finish(runner::Outcome::Cancelled);
        }
    }
}

fn fixture(
    scenario: &str,
    drive: impl FnOnce(&mut Driver, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let directory = crate::core::file_placement::tests::TestDir::new();
    let root: PathBuf = directory.path().join("workspace");
    let home = directory.path().join("home");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let mut driver = Driver::start(&root, &home, scenario)?;
    drive(&mut driver, &root)?;
    driver.finish()
}

pub(crate) fn verify_tui_grants() -> anyhow::Result<()> {
    fixture("tui_grants", |driver, _| {
        let record = driver.next()?;
        anyhow::ensure!(
            record["type"] == "fixture_done",
            "owned TUI grant fixture failed"
        );
        Ok(())
    })
}

#[test]
fn real_stdio_immediate_replies_match_concurrent_requests() -> anyhow::Result<()> {
    fixture("fast", |driver, _| {
        let mut requests = HashMap::new();
        let mut outcomes = HashMap::new();
        loop {
            let record = driver.next()?;
            match record["type"].as_str() {
                Some("control_request") => {
                    let id = record["request_id"].as_str().unwrap();
                    let tool = record["request"]["tool_name"].as_str().unwrap();
                    anyhow::ensure!(matches!(tool, "a" | "b"));
                    anyhow::ensure!(requests.insert(tool.to_owned(), id.to_owned()).is_none());
                    driver.reply("unknown-id", "allow", true)?;
                    driver.send(json!({"type":"control_response","response":{"response":{"behavior":"allow"}}}))?;
                    driver.reply(id, if tool == "a" { "allow" } else { "deny" }, false)?;
                    driver.reply(id, "allow", true)?; // Duplicate never answers another request.
                }
                Some("fixture_approval_result") => {
                    outcomes.insert(
                        record["name"].as_str().unwrap().to_owned(),
                        record["allowed"].as_bool().unwrap(),
                    );
                }
                Some("fixture_done") => break,
                _ => {}
            }
        }
        assert_eq!(requests.len(), 2);
        assert_ne!(requests["a"], requests["b"]);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes["a"]);
        assert!(!outcomes["b"]);
        Ok(())
    })
}

#[test]
fn real_identical_calls_keep_ids_when_second_runs_before_first_denial() -> anyhow::Result<()> {
    identical_calls(false)
}

#[test]
fn real_identical_calls_keep_ids_when_second_runs_before_first_allow() -> anyhow::Result<()> {
    identical_calls(true)
}

fn identical_calls(first_allowed: bool) -> anyhow::Result<()> {
    fixture(
        if first_allowed {
            "identical_allow"
        } else {
            "identical_deny"
        },
        |driver, _| {
            let mut requests = HashMap::<String, (String, String)>::new();
            let mut outcomes = HashMap::new();
            let mut calls = HashMap::<String, String>::new();
            let mut results = std::collections::HashSet::new();
            let mut first_replied = false;
            let mut denial = None;
            loop {
                let record = driver.next()?;
                match record["type"].as_str() {
                    Some("control_request") => {
                        assert_eq!(record["request"]["tool_name"], "approval_identity_probe");
                        assert_eq!(record["request"]["input"], json!({}));
                        let label = record["request"]["agent_id"].as_str().unwrap().to_owned();
                        let id = record["request_id"].as_str().unwrap().to_owned();
                        let tool_id = record["request"]["tool_use_id"]
                            .as_str()
                            .unwrap()
                            .to_owned();
                        assert!(requests
                            .insert(label.clone(), (id.clone(), tool_id))
                            .is_none());
                        if label != "a" {
                            assert!(requests.contains_key("a"));
                            driver.reply(&id, "allow", false)?;
                        }
                    }
                    Some("assistant") => {
                        let block = &record["message"]["content"][0];
                        if block["type"] != "tool_use" {
                            continue;
                        }
                        let label = record["dispatch"].as_str().unwrap().to_owned();
                        let tool_id = block["id"].as_str().unwrap().to_owned();
                        if label == "unbound" {
                            assert!(
                                record.get("request_id").is_none(),
                                "abandoned id cannot leak to an unbound call"
                            );
                            assert!(requests.values().all(|(_, id)| id != &tool_id));
                        } else {
                            let (id, expected_tool_id) = &requests[&label];
                            assert_eq!(record["request_id"], *id);
                            assert_eq!(&tool_id, expected_tool_id);
                        }
                        if label == "b" {
                            assert!(!first_replied, "B must start while identical A is pending");
                            assert!(!calls.contains_key("a"));
                        }
                        assert!(calls.insert(label, tool_id).is_none());
                    }
                    Some("user") => {
                        let block = &record["message"]["content"][0];
                        if block["type"] != "tool_result" {
                            continue;
                        }
                        let label = record["dispatch"].as_str().unwrap();
                        assert_eq!(block["tool_use_id"], calls[label]);
                        assert_eq!(
                            block["is_error"],
                            (label == "a" && !first_allowed) || label == "cancelled",
                        );
                        if label == "cancelled" {
                            assert!(!block["content"].as_str().unwrap().contains("body ran"));
                        }
                        assert!(results.insert(label.to_owned()));
                        if label == "b" {
                            assert!(!first_replied);
                            driver.reply(
                                &requests["a"].0,
                                if first_allowed { "allow" } else { "deny" },
                                false,
                            )?;
                            first_replied = true;
                        }
                    }
                    Some("system") if record["subtype"] == "permission_denied" => {
                        assert!(!first_allowed);
                        assert_eq!(record["request_id"], requests["a"].0);
                        assert_eq!(record["tool_use_id"], requests["a"].1);
                        assert!(denial.replace(record["tool_use_id"].clone()).is_none());
                    }
                    Some("fixture_approval_result") => {
                        let label = record["name"].as_str().unwrap().to_owned();
                        assert!(outcomes
                            .insert(label, record["allowed"].as_bool().unwrap())
                            .is_none());
                    }
                    Some("fixture_done") => break,
                    _ => {}
                }
            }
            assert!(first_replied);
            assert_eq!(requests.len(), 5);
            assert_eq!(calls.len(), 5);
            assert!(!calls.contains_key("abandoned"));
            assert_eq!(results.len(), 5);
            assert_eq!(outcomes.len(), 5);
            assert_eq!(outcomes["a"], first_allowed);
            assert!(!outcomes["cancelled"]);
            assert!(outcomes["b"] && outcomes["abandoned"] && outcomes["later"]);
            assert_eq!(denial.is_some(), !first_allowed);
            assert_eq!(
                requests
                    .values()
                    .map(|(id, _)| id)
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                5
            );
            assert_eq!(
                calls
                    .values()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                5
            );
            Ok(())
        },
    )
}

#[test]
fn real_stdio_eof_denies_outstanding_and_future_requests() -> anyhow::Result<()> {
    fixture("eof", |driver, _| {
        let mut ids = std::collections::HashSet::new();
        let mut outcomes = HashMap::new();
        loop {
            let record = driver.next()?;
            match record["type"].as_str() {
                Some("control_request") => {
                    anyhow::ensure!(ids.insert(record["request_id"].as_str().unwrap().to_owned()));
                    if ids.len() == 2 {
                        driver.stdin.take();
                    }
                }
                Some("fixture_approval_result") => {
                    outcomes.insert(
                        record["name"].as_str().unwrap().to_owned(),
                        record["allowed"].as_bool().unwrap(),
                    );
                }
                Some("fixture_done") => break,
                _ => {}
            }
        }
        assert_eq!(
            ids.len(),
            2,
            "request-after-EOF cannot publish a new prompt"
        );
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.values().all(|allowed| !allowed));
        assert!(outcomes.contains_key("after_eof"));
        Ok(())
    })
}

#[test]
fn real_console_queue_cancels_without_input_and_rejects_late_grants() -> anyhow::Result<()> {
    fixture("console_cancel", |driver, root| {
        let mut first_id = None;
        let mut outcomes = HashMap::new();
        let mut requests = 0;
        loop {
            let record = driver.next()?;
            match record["type"].as_str() {
                Some("control_request") => {
                    requests += 1;
                    let id = record["request_id"].as_str().unwrap();
                    if record["request"]["tool_name"] == "a" {
                        assert!(first_id.is_none());
                        first_id = Some(id.to_owned());
                        std::fs::write(root.join("cancel-now"), "owned cancellation")?;
                    } else {
                        assert_eq!(record["request"]["tool_name"], "b");
                        driver.reply(first_id.as_deref().unwrap(), "allow", true)?;
                        driver.reply(id, "deny", false)?;
                        driver.stdin.take();
                    }
                }
                Some("fixture_approval_result") => {
                    outcomes.insert(
                        record["name"].as_str().unwrap().to_owned(),
                        record["allowed"].as_bool().unwrap(),
                    );
                }
                Some("fixture_done") => break,
                _ => {}
            }
        }
        assert_eq!(requests, 2);
        assert_eq!(outcomes.len(), 4);
        assert!(outcomes.values().all(|allowed| !allowed));
        assert!(outcomes.contains_key("cancelled_allow_all"));
        assert!(outcomes.contains_key("after_eof"));
        assert!(matches!(
            driver.records.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected)
        ));
        Ok(())
    })
}
