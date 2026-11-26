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
    if scenario.starts_with("migration_") {
        migration_stdio_scenario(&root, &scenario)?;
        write_line(&json!({"type":"fixture_done"}).to_string());
        return Ok(());
    }
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

const MIGRATION_PROSE: &str =
    "# Legacy notes\r\n\r\n- [x] prose is not a Check\r\nKeep trailing spaces  ";
const MIGRATION_EDIT: &str = "# Editor changed this while approval was pending\n";

// Called only by the owned helper process; never mutate approval globals in the test parent.
fn migration_stdio_scenario(root: &Path, scenario: &str) -> anyhow::Result<()> {
    let standing = matches!(scenario, "migration_grants" | "migration_yolo");
    if standing {
        ALLOW_ALL.store(true, Ordering::Relaxed);
        crate::core::approval::grant_session("file_write", None);
        assert!(crate::core::approval::granted("file_write", None).is_some());
    }
    let cases: &[(&str, bool)] = match scenario {
        "migration_once" => &[("approved", true), ("later", false)],
        "migration_grants" => &[("denied", false), ("approved", true)],
        "migration_cas" => &[("changed", false)],
        "migration_yolo" => &[("yolo", false)],
        _ => anyhow::bail!("unknown migration fixture scenario"),
    };
    for &(label, expected) in cases {
        migration_attempt(root, label, expected)?;
        assert_eq!(
            allow_all(),
            standing,
            "one-time reply must not widen or clear session authority"
        );
        assert_eq!(
            crate::core::approval::session_grants().len(),
            usize::from(standing)
        );
        assert!(lock().waiting().is_empty());
    }
    Ok(())
}

fn migration_attempt(root: &Path, label: &str, expected: bool) -> anyhow::Result<()> {
    use crate::agent::{planning_write, todo};
    use crate::core::exec_ctx::ExecutionContext;
    use crate::core::native_journal::{inspect_existing_records, RecordClaim, RecordProofStatus};
    use crate::core::persist::FileFingerprint;
    use crate::core::session_control::ExecutionMode;
    use crate::core::workspace_txn::NativeResource;

    std::fs::create_dir_all(root.join("docs/plans"))?;
    let relative = format!("docs/plans/{label}.md");
    let path = root.join(&relative);
    crate::core::persist::atomic_write(&path, MIGRATION_PROSE.as_bytes())?;
    let migration = planning_write::LegacyMigration::read(root, Some(&relative), None)?;
    let task = todo::TaskContext::private();
    let control = task.control();
    control.transition(
        ExecutionMode::Planning,
        Some(migration.target().clone()),
        |_| Ok(()),
    )?;
    task.replace_with_requirements(
        vec![todo::Todo::new(
            "existing implementation",
            todo::Status::Pending,
        )],
        vec![(
            "@index:0".into(),
            todo::Requirement::Check {
                command: "cargo check".into(),
                root: "workspace".into(),
                source_identity: None,
            },
        )],
    )?;
    let ctx = ExecutionContext::default()
        .with_task(task.clone())
        .with_dispatch_label(Some(label.to_owned()))
        .with_deadline(Some(Instant::now() + Duration::from_secs(15)))
        .with_placement(crate::core::file_placement::PlacementContext::with_paths(
            root.to_path_buf(),
            root.join("home"),
            root.join("process"),
            "migration-fixture",
        )?)
        .with_native_scope(crate::core::native_scope::NativeWriteScope::resolve(
            root,
            std::slice::from_ref(&relative),
        )?)?;
    ctx.note_task_effects_at(Some(root));
    let before = task.snapshot();
    assert!(!before.obligations.is_empty());
    let prepared = planning_write::prepare_migration(&migration, &ctx)?;
    let approved_bytes = prepared.args["content"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    let proof = planning_write::proof(&prepared).unwrap().clone();
    let owner = ctx.resource_scope();
    let result = if label == "yolo" {
        // Production bridge pins Ask; separately exercise its real gate under a Yolo config.
        let allowed = crate::agent::migration_yolo_approval_fixture(&prepared, ctx)?;
        anyhow::ensure!(!allowed, "Yolo bypassed explicit migration denial");
        Err(anyhow::anyhow!("fixture observed explicit denial"))
    } else {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        runtime.block_on(crate::agent::execute_human_plan_migration(prepared, ctx))
    };
    assert_eq!(result.is_ok(), expected, "{label}: {result:?}");
    let after = task.snapshot();
    assert_eq!(after.items, before.items);
    assert_eq!(after.requirements, before.requirements);
    assert_eq!(after.obligations, before.obligations);
    assert_eq!(after.checks, before.checks);
    assert!(!task.is_complete());
    assert!(control.snapshot().is_planning());
    assert_eq!(
        control.snapshot().plan_target.as_ref(),
        Some(migration.target())
    );
    let resource = NativeResource::resolve(&path)?;
    let journal = inspect_existing_records(&[RecordClaim {
        // Each child owns a fresh HOME; only the one approved migration can create record 1.
        id: 1,
        expected_owner: &owner,
        resource: &resource,
        before: &FileFingerprint::for_bytes(MIGRATION_PROSE.as_bytes()),
        after: &FileFingerprint::for_bytes(&approved_bytes),
    }]);
    if expected {
        assert!(proof.successful_write());
        assert_eq!(std::fs::read(&path)?, approved_bytes);
        assert!(approved_bytes.ends_with(MIGRATION_PROSE.as_bytes()));
        assert_eq!(after.effect_epoch, before.effect_epoch + 1);
        let attachment = todo::PlanAttachment::load(root, &relative)?;
        assert!(attachment.live);
        assert_eq!(after.plan.as_ref(), Some(&attachment));
        let records = journal?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, RecordProofStatus::SettledApplied);
    } else {
        assert!(!proof.successful_write());
        assert_eq!(after.effect_epoch, before.effect_epoch);
        assert!(after.plan.is_none());
        assert!(
            journal.is_err(),
            "refusal/CAS failure cannot journal an applied write"
        );
        let expected_bytes = if label == "changed" {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("changed"), "{error:#}");
            MIGRATION_EDIT.as_bytes()
        } else {
            MIGRATION_PROSE.as_bytes()
        };
        assert_eq!(std::fs::read(&path)?, expected_bytes);
    }
    report(label, expected);
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
    // The sandbox grants workspace writes, not writes to a sibling fixture HOME. Real tool
    // execution creates workspace locks there after approval, including under Linux Landlock.
    let home = root.join("home");
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

fn drive_migration(scenario: &str, expected: &[(&str, bool)]) -> anyhow::Result<()> {
    fixture(scenario, |driver, root| {
        let root = root.canonicalize()?;
        let mut requests = HashMap::new();
        let mut outcomes = HashMap::new();
        loop {
            let record = driver.next()?;
            match record["type"].as_str() {
                Some("control_request") => {
                    let request = &record["request"];
                    assert_eq!(request["tool_name"], "file_write");
                    let label = request["agent_id"].as_str().unwrap();
                    assert!(expected.iter().any(|(name, _)| name == &label));
                    let id = record["request_id"].as_str().unwrap();
                    assert!(requests.insert(label.to_owned(), id.to_owned()).is_none());
                    let path = root.join(format!("docs/plans/{label}.md"));
                    assert_eq!(Path::new(request["input"]["path"].as_str().unwrap()), path);
                    assert_eq!(std::fs::read(&path)?, MIGRATION_PROSE.as_bytes());
                    let content = request["input"]["content"].as_str().unwrap();
                    assert!(content.ends_with(MIGRATION_PROSE));
                    assert!(content.starts_with("---\naizen_plan: 1\nid: p-"));
                    assert!(request["preview"]["diff"]
                        .as_str()
                        .unwrap()
                        .contains("aizen:phase legacy"));
                    assert!(request["description"]
                        .as_str()
                        .unwrap()
                        .contains("Migrate this exact legacy document once"));
                    if label == "changed" {
                        crate::core::persist::atomic_write(&path, MIGRATION_EDIT.as_bytes())?;
                    }
                    // A requested permanent grant must remain invocation-only for migrations.
                    let behavior = if matches!(label, "approved" | "changed") {
                        "allow"
                    } else {
                        "deny"
                    };
                    driver.reply(id, behavior, true)?;
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
        assert_eq!(
            requests.len(),
            expected.len(),
            "standing approval cannot suppress a migration prompt"
        );
        assert_eq!(outcomes.len(), expected.len());
        assert_eq!(
            requests
                .values()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            expected.len()
        );
        for &(label, success) in expected {
            assert_eq!(outcomes[label], success);
        }
        Ok(())
    })
}

#[test]
fn real_plan_migration_approval_is_one_time_despite_widening_reply() -> anyhow::Result<()> {
    drive_migration("migration_once", &[("approved", true), ("later", false)])
}

#[test]
fn real_plan_migration_requires_approval_despite_standing_grants() -> anyhow::Result<()> {
    drive_migration("migration_grants", &[("denied", false), ("approved", true)])
}

#[test]
fn real_plan_migration_approval_time_cas_preserves_editor_bytes() -> anyhow::Result<()> {
    drive_migration("migration_cas", &[("changed", false)])
}

#[test]
fn real_plan_migration_gate_cannot_inherit_yolo() -> anyhow::Result<()> {
    drive_migration("migration_yolo", &[("yolo", false)])
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
                            "unexpected result for {label}: {record}",
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
