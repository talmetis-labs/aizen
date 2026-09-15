//! `aizen bench tasks` — the task suite: the REAL loop, the REAL tools, a real repo, and a model
//! whose answers come off a tape.
//!
//! `bench loop` proves loop discipline against a scripted model that never looks at its input;
//! its "verified-done" means the state machine reached `Done`. This suite is the other half: each
//! task under `bench-tasks/<id>/` is a small cargo crate plus a prompt, the loop runs with the
//! verify gate ON and every built-in tool rooted in a throwaway copy of the crate, and the run is
//! judged on what happened to the files — `cargo test` exits 0, only the allowed files changed,
//! the loop stopped on `Done`, and it did so within the recorded budget of steps and tokens.
//!
//! The model is the tape (`llm::replay`): `--record` makes the real calls once and writes them
//! down, and from then on the same decisions replay offline, so CI runs the suite without a key
//! and a change to the harness that alters the outcome is caught — a truncation that hides the
//! failing line, a guard that starts blocking `cargo test`, a verify gate that stops running.
//! A drifted fingerprint (the model was shown something different from the recording) is
//! reported per task; it is a warning, not a failure, because the harness edits are exactly what
//! the suite exists to measure.
//!
//! Metrics per task: steps, model calls, tool calls, repeated tool calls (same name and
//! arguments as an earlier call in the run), input/output tokens (from the tape's recorded
//! usage), the verify command's exit code, and the changed-file set. `bench-fixtures/loop-
//! baseline.json` holds the steps/tokens each task took when its tape was recorded; a later run
//! may use at most [`BASELINE_SLACK`] times either.

use crate::agent::{self, AgentConfig, StopReason};
use crate::core::approval::ApprovalMode;
use crate::core::types::{Message, ToolDef};
use crate::llm::client;
use crate::llm::replay::{self, Mode};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A task may use this many times the baseline's steps and tokens before it counts as a regression.
pub const BASELINE_SLACK: f64 = 1.25;
/// Repeated tool calls (identical name + arguments) as a share of all tool calls, suite-wide.
pub const MAX_REPEAT_RATE: f64 = 0.02;
/// The verify command's wall-clock cap (a fixture compiles in seconds; this is a hang guard).
const VERIFY_TIMEOUT: Duration = Duration::from_secs(600);

/// What `aizen bench tasks` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Run only this task id.
    pub task: Option<String>,
    /// Make the real model calls and write a fresh tape per task.
    pub record: bool,
    /// Make the real model calls and leave the tapes alone.
    pub live: bool,
    /// Tape name under `bench-tasks/<id>/tapes/` (`default` unless given).
    pub tape: String,
    /// Write the passing tasks' steps/tokens as the new baseline.
    pub update_baseline: bool,
    /// Print the report as JSON instead of the table.
    pub json: bool,
}

/// `bench-tasks/<id>/task.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub prompt: String,
    /// Repo-relative paths (forward slashes) the run may create or change.
    #[serde(default)]
    pub allowed_files: Vec<String>,
    /// `done` (default) or `no-edit`.
    #[serde(default = "default_expect")]
    pub expect: String,
    /// argv run in the copy after the loop; exit 0 is the pass condition.
    #[serde(default = "default_verify")]
    pub verify: Vec<String>,
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
}

fn default_expect() -> String {
    "done".into()
}
fn default_verify() -> Vec<String> {
    vec!["cargo".into(), "test".into(), "--quiet".into()]
}
fn default_max_steps() -> usize {
    30
}

/// One task's measured outcome.
#[derive(Debug, Clone, Serialize)]
pub struct TaskResult {
    pub id: String,
    /// `pass` · `fail` · `skip`.
    pub status: String,
    /// `replay` · `record` · `live` · `skip`.
    pub mode: String,
    /// Why it failed (empty on pass/skip), or why it was skipped.
    pub reasons: Vec<String>,
    pub stop: String,
    pub steps: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub repeat_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub verify_exit: Option<i32>,
    pub changed_files: Vec<String>,
    pub disallowed_files: Vec<String>,
    pub tape_drift: usize,
    pub tape_total: usize,
    pub seconds: f64,
}

impl TaskResult {
    fn skipped(id: &str, why: &str) -> Self {
        Self {
            id: id.into(),
            status: "skip".into(),
            mode: "skip".into(),
            reasons: vec![why.into()],
            stop: String::new(),
            steps: 0,
            model_calls: 0,
            tool_calls: 0,
            repeat_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            verify_exit: None,
            changed_files: Vec::new(),
            disallowed_files: Vec::new(),
            tape_drift: 0,
            tape_total: 0,
            seconds: 0.0,
        }
    }

    pub fn passed(&self) -> bool {
        self.status == "pass"
    }
}

/// `bench-fixtures/loop-baseline.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Baseline {
    #[serde(default)]
    pub tasks: BTreeMap<String, BaselineRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineRow {
    pub steps: usize,
    pub tokens: u64,
}

/// The whole report, as `--json` prints it.
#[derive(Debug, Serialize)]
pub struct SuiteReport {
    pub tasks: Vec<TaskResult>,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Tasks expected to reach Done that did, over those expected to.
    pub verified_done: (usize, usize),
    pub wrong_file_edits: usize,
    pub repeat_rate: f64,
    /// `pass` · `regressed` · `missing` (no baseline file yet) · `n/a` (nothing ran).
    pub baseline: String,
    pub baseline_notes: Vec<String>,
}

pub fn tasks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench-tasks")
}

pub fn baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench-fixtures/loop-baseline.json")
}

/// Every `bench-tasks/<id>/task.json`, sorted by id.
pub fn load_specs(dir: &Path) -> Result<Vec<(PathBuf, TaskSpec)>> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        let spec = p.join("task.json");
        if !spec.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&spec)
            .with_context(|| format!("reading {}", spec.display()))?;
        let s: TaskSpec =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", spec.display()))?;
        out.push((p, s));
    }
    out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    Ok(out)
}

/// `aizen bench tasks` entry point. Exit non-zero when any task fails or the baseline regressed.
pub async fn run(opts: Options) -> Result<()> {
    if opts.record && opts.live {
        bail!("--record and --live are exclusive");
    }
    let dir = tasks_dir();
    let specs = load_specs(&dir)?;
    let specs: Vec<_> = specs
        .into_iter()
        .filter(|(_, s)| opts.task.as_deref().is_none_or(|t| t == s.id))
        .collect();
    if specs.is_empty() {
        bail!(
            "no task matched{} under {}",
            opts.task
                .as_deref()
                .map(|t| format!(" `{t}`"))
                .unwrap_or_default(),
            dir.display()
        );
    }
    let tape_name = if opts.tape.trim().is_empty() {
        "default".to_string()
    } else {
        opts.tape.clone()
    };

    let mut results = Vec::with_capacity(specs.len());
    for (task_dir, spec) in &specs {
        let tape_path = task_dir.join("tapes").join(format!("{tape_name}.jsonl"));
        let mode = if opts.record {
            Mode::Record
        } else if opts.live {
            Mode::Off
        } else if tape_path.is_file() {
            Mode::Replay
        } else {
            results.push(TaskResult::skipped(
                &spec.id,
                &format!(
                    "no tape at {} — record one with `aizen bench tasks --record --task {}`",
                    tape_path.display(),
                    spec.id
                ),
            ));
            if !opts.json {
                println!(
                    "  {:<20} SKIP    no tape ({})",
                    spec.id,
                    tape_path.display()
                );
            }
            continue;
        };
        let r = run_task(spec, task_dir, mode, &tape_path).await?;
        if !opts.json {
            print_row(&r);
        }
        results.push(r);
    }

    let mut report = summarize(results);
    let baseline = read_baseline()?;
    apply_baseline(&mut report, baseline.as_ref());
    if opts.update_baseline {
        let mut b = baseline.unwrap_or_default();
        for r in report.tasks.iter().filter(|r| r.passed()) {
            b.tasks.insert(
                r.id.clone(),
                BaselineRow {
                    steps: r.steps,
                    tokens: r.input_tokens + r.output_tokens,
                },
            );
        }
        let json = serde_json::to_string_pretty(&b)?;
        std::fs::write(baseline_path(), json + "\n")
            .with_context(|| format!("writing {}", baseline_path().display()))?;
        report.baseline = "updated".into();
        report.baseline_notes.clear();
        if !opts.json {
            println!("baseline updated: {}", baseline_path().display());
        }
    }

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_summary(&report);
    }
    let regressed = report.baseline == "regressed";
    if report.failed > 0 || regressed {
        bail!(
            "task suite: {} failed, baseline {}",
            report.failed,
            report.baseline
        );
    }
    Ok(())
}

/// Run one task: copy the fixture, point the tape at it, drive the real loop, judge the files.
///
/// The todo lock is a std mutex shared with the synchronous loop tests, and it has to stay held
/// across the loop's awaits — that exclusion is the point. Tasks run one at a time, so nothing
/// else can wait on it while the future is parked.
#[allow(clippy::await_holding_lock)]
pub async fn run_task(
    spec: &TaskSpec,
    task_dir: &Path,
    mode: Mode,
    tape_path: &Path,
) -> Result<TaskResult> {
    let started = Instant::now();
    let fixture = task_dir.join("repo");
    if !fixture.is_dir() {
        bail!("{}: no repo/ directory", spec.id);
    }
    let work = std::env::temp_dir().join("aizen-bench-tasks").join(format!(
        "{}-{}",
        spec.id,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work);
    copy_tree(&fixture, &work)?;
    let work = work.canonicalize().unwrap_or(work);

    // Creds: a replayed run never sends a request, so placeholders are fine — and a machine with
    // no endpoint configured (CI) must still be able to run the suite.
    let (base_url, api_key, model) = match mode {
        Mode::Record | Mode::Off => crate::core::endpoint::resolve_endpoint(None, None, None)
            .context("recording/live runs need a configured endpoint")?,
        Mode::Replay | Mode::Strict => (
            "http://tape.invalid".to_string(),
            "tape".to_string(),
            "tape".to_string(),
        ),
    };
    replay::configure(mode, tape_path, Some(&work))?;

    // The loop's process-global state (todo list, read cache) is per run; serialize with the
    // other loop drivers in this process.
    let _g = crate::agent::todo::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::agent::todo::clear();

    // Registry BEFORE prompt, as every real entry point does: the prompt's tool-routing map is
    // generated from the published surface. `task`/`workflow` are left out — the suite measures
    // the single loop, and a fixture crate has nothing to delegate.
    let mut registry = agent::builtin::default_registry_in(&work);
    // Same deferred set a real turn of this shape would get, so the suite measures the lean
    // surface the user sees.
    agent::builtin::defer_builtins_for_shape(
        &mut registry,
        Some(crate::core::turn_shape::classify(&spec.prompt)),
    );
    agent::builtin::publish_active_tools(&registry);
    let system = agent::build_top_level_system_prompt(
        &work.display().to_string(),
        std::env::consts::OS,
        "2026-01-01",
        &model,
        None,
    );
    let cfg = AgentConfig {
        max_iters: spec.max_steps,
        auto_extend_to: spec.max_steps,
        approval_mode: ApprovalMode::Yolo,
        quiet: true,
        enable_verify_gate: true,
        auto_checkpoint: false,
        checkpoint_each_edit: false,
        workspace_root: Some(work.clone()),
        ..AgentConfig::default()
    };

    let http = reqwest::Client::new();
    let calls = AtomicUsize::new(0);
    let chat = |msgs: Vec<Message>, defs: Vec<ToolDef>| {
        calls.fetch_add(1, Ordering::Relaxed);
        let (http, base, key, model) = (&http, &base_url, &api_key, &model);
        async move { client::chat_with_tools(http, base, key, model, &msgs, &defs).await }
    };

    let meter = client::cost_meter();
    let (in0, out0, _) = meter.snapshot();
    let mut messages = vec![Message::system(&system), Message::user(&spec.prompt)];
    let outcome = agent::run_agent_loop(chat, &cfg, &registry, &mut messages).await;
    let (in1, out1, _) = meter.snapshot();
    let tape = replay::status();
    replay::disable();
    crate::agent::todo::clear();

    let (tool_calls, repeat_calls) = count_tool_calls(&messages);
    let changed_files = changed_files(&fixture, &work)?;
    let allowed: HashSet<&str> = spec.allowed_files.iter().map(String::as_str).collect();
    let disallowed_files: Vec<String> = changed_files
        .iter()
        .filter(|f| !allowed.contains(f.as_str()))
        .cloned()
        .collect();
    let verify_exit = run_verify(&spec.verify, &work).await;

    let mut reasons = Vec::new();
    let (stop, steps) = match &outcome {
        Ok(o) => (format!("{:?}", o.stop), o.iters),
        Err(e) => {
            reasons.push(format!("loop error: {e:#}"));
            ("Error".to_string(), 0)
        }
    };
    if let Ok(o) = &outcome {
        if o.stop != StopReason::Done {
            reasons.push(format!("stopped on {:?}, not Done", o.stop));
        }
    }
    match verify_exit {
        Some(0) => {}
        Some(code) => reasons.push(format!("verify command exited {code}")),
        None => reasons.push("verify command did not run".into()),
    }
    if !disallowed_files.is_empty() {
        reasons.push(format!(
            "changed files outside allowed_files: {}",
            disallowed_files.join(", ")
        ));
    }
    if spec.expect == "no-edit" && !changed_files.is_empty() {
        reasons.push(format!(
            "expected no edits, but changed: {}",
            changed_files.join(", ")
        ));
    }

    let _ = std::fs::remove_dir_all(&work);
    Ok(TaskResult {
        id: spec.id.clone(),
        status: if reasons.is_empty() { "pass" } else { "fail" }.into(),
        mode: match mode {
            Mode::Off => "live",
            m => m.as_str(),
        }
        .into(),
        reasons,
        stop,
        steps,
        model_calls: calls.load(Ordering::Relaxed),
        tool_calls,
        repeat_calls,
        input_tokens: in1.saturating_sub(in0),
        output_tokens: out1.saturating_sub(out0),
        verify_exit,
        changed_files,
        disallowed_files,
        tape_drift: tape.as_ref().map(|s| s.drift).unwrap_or(0),
        tape_total: tape.as_ref().map(|s| s.total).unwrap_or(0),
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// `(tool calls, repeated tool calls)` over the assistant turns: a repeat is a call whose name
/// and arguments match an earlier call in the same run.
pub fn count_tool_calls(messages: &[Message]) -> (usize, usize) {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let (mut total, mut repeats) = (0, 0);
    for m in messages.iter().filter(|m| m.role == "assistant") {
        for tc in &m.tool_calls {
            total += 1;
            let key = (tc.function.name.clone(), tc.function.arguments.clone());
            if !seen.insert(key) {
                repeats += 1;
            }
        }
    }
    (total, repeats)
}

/// Directories a fixture copy and its diff ignore: build output and aizen's own state.
fn skip_dir(name: &str) -> bool {
    matches!(name, "target" | ".git" | ".aizen")
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for e in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let e = e?;
        let name = e.file_name();
        let src = e.path();
        let dst = to.join(&name);
        if src.is_dir() {
            if skip_dir(&name.to_string_lossy()) {
                continue;
            }
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {} → {}", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !skip_dir(&name) {
                walk_files(root, &p, out)?;
            }
        } else if name != "Cargo.lock" {
            let rel = p.strip_prefix(root).unwrap_or(&p);
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Files added, modified or deleted in `work` relative to `fixture`, repo-relative with `/`.
pub fn changed_files(fixture: &Path, work: &Path) -> Result<Vec<String>> {
    let mut before = Vec::new();
    let mut after = Vec::new();
    walk_files(fixture, fixture, &mut before)?;
    walk_files(work, work, &mut after)?;
    let before: HashSet<String> = before.into_iter().collect();
    let after: HashSet<String> = after.into_iter().collect();
    let mut changed: Vec<String> = Vec::new();
    for f in before.union(&after) {
        let a = std::fs::read(fixture.join(f)).ok();
        let b = std::fs::read(work.join(f)).ok();
        if a != b {
            changed.push(f.clone());
        }
    }
    changed.sort();
    Ok(changed)
}

/// Run the verify argv in `cwd`; `None` when it could not be spawned or timed out. Under
/// `cargo test` the `CARGO` env var names the running cargo, which is more reliable than PATH.
async fn run_verify(argv: &[String], cwd: &Path) -> Option<i32> {
    let (program, args) = argv.split_first()?;
    let program = if program == "cargo" {
        std::env::var("CARGO").unwrap_or_else(|_| program.clone())
    } else {
        program.clone()
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().ok()?;
    match tokio::time::timeout(VERIFY_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        Ok(Err(_)) => None,
        Err(_) => {
            let _ = child.kill().await;
            None
        }
    }
}

fn read_baseline() -> Result<Option<Baseline>> {
    let p = baseline_path();
    if !p.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(Some(
        serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))?,
    ))
}

fn summarize(tasks: Vec<TaskResult>) -> SuiteReport {
    let passed = tasks.iter().filter(|t| t.status == "pass").count();
    let failed = tasks.iter().filter(|t| t.status == "fail").count();
    let skipped = tasks.iter().filter(|t| t.status == "skip").count();
    let ran: Vec<&TaskResult> = tasks.iter().filter(|t| t.status != "skip").collect();
    let done = ran.iter().filter(|t| t.stop == "Done").count();
    let wrong_file_edits = ran.iter().map(|t| t.disallowed_files.len()).sum();
    let tool_calls: usize = ran.iter().map(|t| t.tool_calls).sum();
    let repeats: usize = ran.iter().map(|t| t.repeat_calls).sum();
    let repeat_rate = if tool_calls == 0 {
        0.0
    } else {
        repeats as f64 / tool_calls as f64
    };
    SuiteReport {
        passed,
        failed,
        skipped,
        verified_done: (done, ran.len()),
        wrong_file_edits,
        repeat_rate,
        baseline: if ran.is_empty() { "n/a" } else { "missing" }.into(),
        baseline_notes: Vec::new(),
        tasks,
    }
}

/// Compare each passing task with its baseline row; a task with no row is noted, not failed.
fn apply_baseline(report: &mut SuiteReport, baseline: Option<&Baseline>) {
    if report.baseline == "n/a" {
        return;
    }
    let Some(b) = baseline else {
        report.baseline_notes.push(format!(
            "no baseline yet — run `aizen bench tasks --update-baseline` to capture one at {}",
            baseline_path().display()
        ));
        return;
    };
    let mut regressed = false;
    for t in report.tasks.iter().filter(|t| t.status != "skip") {
        let Some(row) = b.tasks.get(&t.id) else {
            report
                .baseline_notes
                .push(format!("{}: no baseline row", t.id));
            continue;
        };
        let steps_cap = (row.steps as f64 * BASELINE_SLACK).ceil() as usize;
        let tokens_cap = (row.tokens as f64 * BASELINE_SLACK).ceil() as u64;
        let tokens = t.input_tokens + t.output_tokens;
        if t.steps > steps_cap {
            regressed = true;
            report.baseline_notes.push(format!(
                "{}: {} steps > {} (baseline {} × {BASELINE_SLACK})",
                t.id, t.steps, steps_cap, row.steps
            ));
        }
        if tokens > tokens_cap {
            regressed = true;
            report.baseline_notes.push(format!(
                "{}: {} tokens > {} (baseline {} × {BASELINE_SLACK})",
                t.id, tokens, tokens_cap, row.tokens
            ));
        }
    }
    if report.repeat_rate > MAX_REPEAT_RATE {
        regressed = true;
        report.baseline_notes.push(format!(
            "repeat-call rate {:.1}% > {:.0}%",
            report.repeat_rate * 100.0,
            MAX_REPEAT_RATE * 100.0
        ));
    }
    report.baseline = if regressed { "regressed" } else { "pass" }.into();
}

fn print_row(r: &TaskResult) {
    println!(
        "  {:<20} {:<7} {:<5} stop={:<12} steps={:<3} calls={:<3} tools={:<3} repeats={:<2} tokens={}/{} verify={} drift={}/{} {:.1}s",
        r.id,
        r.mode,
        r.status.to_uppercase(),
        r.stop,
        r.steps,
        r.model_calls,
        r.tool_calls,
        r.repeat_calls,
        r.input_tokens,
        r.output_tokens,
        r.verify_exit
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into()),
        r.tape_drift,
        r.tape_total,
        r.seconds
    );
    if !r.changed_files.is_empty() {
        println!("  {:<20} changed: {}", "", r.changed_files.join(", "));
    }
    for why in &r.reasons {
        println!("  {:<20} ! {why}", "");
    }
}

fn print_summary(report: &SuiteReport) {
    println!();
    println!(
        "tasks: {} passed, {} failed, {} skipped · verified-done {}/{} · wrong-file edits {} · repeat rate {:.1}%",
        report.passed,
        report.failed,
        report.skipped,
        report.verified_done.0,
        report.verified_done.1,
        report.wrong_file_edits,
        report.repeat_rate * 100.0
    );
    println!("baseline: {}", report.baseline.to_uppercase());
    for n in &report.baseline_notes {
        println!("  - {n}");
    }
    if report.passed + report.failed == 0 {
        println!("nothing measured: every task was skipped (no tapes recorded yet — see bench-tasks/README.md)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionCall, ToolCall};

    fn assistant_call(name: &str, args: &str) -> Message {
        Message::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }])
    }

    #[test]
    fn every_shipped_task_spec_parses_and_has_a_repo() {
        let specs = load_specs(&tasks_dir()).unwrap();
        let ids: Vec<&str> = specs.iter().map(|(_, s)| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "add-feature",
                "fix-build-error",
                "fix-failing-test",
                "no-edit-control"
            ]
        );
        for (dir, s) in &specs {
            assert!(dir.join("repo/Cargo.toml").is_file(), "{}", s.id);
            assert!(!s.prompt.trim().is_empty());
            assert!(s.max_steps > 0);
            assert!(matches!(s.expect.as_str(), "done" | "no-edit"), "{}", s.id);
            let manifest = std::fs::read_to_string(dir.join("repo/Cargo.toml")).unwrap();
            assert!(
                manifest.contains("[workspace]"),
                "{}: fixture must carry an empty [workspace] so cargo never climbs into this repo",
                s.id
            );
        }
    }

    #[test]
    fn repeat_calls_count_identical_name_and_arguments_only() {
        let msgs = vec![
            Message::system("s"),
            assistant_call("file_read", r#"{"path":"a"}"#),
            Message::tool_result("call_1", "x"),
            assistant_call("file_read", r#"{"path":"b"}"#),
            Message::tool_result("call_1", "y"),
            assistant_call("file_read", r#"{"path":"a"}"#),
            Message::tool_result("call_1", "x"),
            Message::assistant("done"),
        ];
        assert_eq!(count_tool_calls(&msgs), (3, 1));
    }

    #[test]
    fn changed_files_sees_adds_edits_and_deletes_and_ignores_target() {
        let base = std::env::temp_dir().join(format!("aizen-taskeval-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(a.join("src")).unwrap();
        std::fs::write(a.join("src/lib.rs"), "one").unwrap();
        std::fs::write(a.join("gone.txt"), "x").unwrap();
        copy_tree(&a, &b).unwrap();
        std::fs::write(b.join("src/lib.rs"), "two").unwrap();
        std::fs::remove_file(b.join("gone.txt")).unwrap();
        std::fs::write(b.join("new.rs"), "n").unwrap();
        std::fs::create_dir_all(b.join("target")).unwrap();
        std::fs::write(b.join("target/junk"), "j").unwrap();
        std::fs::write(b.join("Cargo.lock"), "lock").unwrap();
        assert_eq!(
            changed_files(&a, &b).unwrap(),
            vec!["gone.txt", "new.rs", "src/lib.rs"]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn baseline_gate_allows_the_slack_and_flags_beyond_it() {
        let mk = |id: &str, steps: usize, tokens: u64| TaskResult {
            steps,
            input_tokens: tokens,
            status: "pass".into(),
            stop: "Done".into(),
            ..TaskResult::skipped(id, "")
        };
        let mut b = Baseline::default();
        b.tasks.insert(
            "t1".into(),
            BaselineRow {
                steps: 4,
                tokens: 1000,
            },
        );
        b.tasks.insert(
            "t2".into(),
            BaselineRow {
                steps: 4,
                tokens: 1000,
            },
        );
        let mut r = summarize(vec![mk("t1", 5, 1250), mk("t2", 6, 1000), mk("t3", 1, 1)]);
        apply_baseline(&mut r, Some(&b));
        assert_eq!(r.baseline, "regressed");
        assert!(
            r.baseline_notes
                .iter()
                .any(|n| n.starts_with("t2: 6 steps")),
            "{:?}",
            r.baseline_notes
        );
        assert!(r.baseline_notes.iter().any(|n| n == "t3: no baseline row"));
        assert!(
            !r.baseline_notes.iter().any(|n| n.starts_with("t1")),
            "5 ≤ ceil(4×1.25)"
        );

        let mut ok = summarize(vec![mk("t1", 5, 1250)]);
        apply_baseline(&mut ok, Some(&b));
        assert_eq!(ok.baseline, "pass");

        let mut none = summarize(vec![mk("t1", 5, 1250)]);
        apply_baseline(&mut none, None);
        assert_eq!(none.baseline, "missing");

        let mut empty = summarize(vec![TaskResult::skipped("t1", "no tape")]);
        apply_baseline(&mut empty, Some(&b));
        assert_eq!(empty.baseline, "n/a");
    }

    /// The end-to-end proof: a hand-written tape drives the REAL loop and the REAL tools on the
    /// `fix-failing-test` fixture — the edit lands, the verify gate and the verify command run
    /// `cargo test` in the copy, and the file diff is exactly the allowed file. Needs cargo.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the tape lock must span the run; see `run_task`
    async fn synthetic_tape_drives_real_tools_on_the_fix_failing_test_fixture() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let have_cargo = std::process::Command::new(&cargo)
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if !have_cargo {
            assert!(
                std::env::var("CI").is_err(),
                "CI must have cargo on PATH for the task suite"
            );
            eprintln!("skipping: cargo not runnable from this test process");
            return;
        }
        let _tape_lock = crate::llm::replay::TAPE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let task_dir = tasks_dir().join("fix-failing-test");
        let spec: TaskSpec =
            serde_json::from_str(&std::fs::read_to_string(task_dir.join("task.json")).unwrap())
                .unwrap();
        let tape = std::env::temp_dir().join(format!(
            "aizen-taskeval-synthetic-{}.jsonl",
            std::process::id()
        ));
        // Fingerprints are deliberately bogus: replay mode warns and carries on, which is the
        // documented contract for a harness that changed underneath a recording.
        let edit = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "(1..n).sum()",
            "new_string": "(1..=n).sum()"
        });
        let lines = [
            serde_json::json!({
                "ordinal": 0, "model": "synthetic", "system_fp": "x", "turns_fp": "x",
                "turn": {"tool_calls": [{"id": "call_edit", "type": "function",
                    "function": {"name": "file_edit", "arguments": edit.to_string()}}],
                    "finish_reason": "tool_calls",
                    "usage": {"prompt": 900, "completion": 40, "cached": 0, "cache_write": 0}}
            }),
            serde_json::json!({
                "ordinal": 1, "model": "synthetic", "system_fp": "x", "turns_fp": "x",
                "turn": {"content": "Fixed: the range excluded n itself; `1..=n` includes it. All three tests pass.",
                    "finish_reason": "stop",
                    "usage": {"prompt": 1100, "completion": 30, "cached": 0, "cache_write": 0}}
            }),
        ];
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        std::fs::write(&tape, text).unwrap();

        let r = run_task(&spec, &task_dir, Mode::Replay, &tape)
            .await
            .unwrap();
        let _ = std::fs::remove_file(&tape);
        assert_eq!(r.status, "pass", "{:?}", r.reasons);
        assert_eq!(r.stop, "Done");
        assert_eq!(r.verify_exit, Some(0));
        assert_eq!(r.changed_files, vec!["src/lib.rs"]);
        assert!(r.disallowed_files.is_empty());
        assert_eq!(r.model_calls, 2, "one edit turn and one final answer");
        assert_eq!((r.tool_calls, r.repeat_calls), (1, 0));
        assert_eq!(r.input_tokens, 2000, "usage comes off the tape");
        assert_eq!(r.tape_total, 2);
    }
}
