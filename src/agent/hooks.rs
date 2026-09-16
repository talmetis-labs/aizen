//! User lifecycle hooks: your own commands, run at fixed points of the agent loop.
//!
//! Configured under `hooks` in `~/.aizen/cli-config.json` — the user's file, never the
//! repository's, so a cloned checkout cannot plant a command that runs on the next `aizen agent`:
//!
//! ```json
//! "hooks": {
//!   "pre_tool":  [{ "match": "shell_run",         "run": "python ~/hooks/guard.py" }],
//!   "post_tool": [{ "match": "file_edit|file_write", "run": "cargo fmt --quiet" }],
//!   "stop":      [{ "run": "notify-send 'aizen' 'run finished'" }]
//! }
//! ```
//!
//! Three events. `pre_tool` runs before a tool call — after the hard safety floor, which no hook
//! can override, and before the approval prompt, which a hook can answer: exit `2` (or a
//! `{"decision":"deny"}` line on stdout) refuses the call and the model is told why;
//! `{"decision":"allow"}` runs it without asking the user. `post_tool` runs after the call: its
//! stdout (or a `{"context":"…"}` line) is appended to the tool result the model reads, so a
//! formatter's or checker's output reaches the model on the same step. `stop` runs when a
//! top-level run ends; its output is only traced.
//!
//! Every hook reads one JSON object on stdin — `event`, `cwd`, `session`, `dispatch` (the
//! sub-agent label, if any), and for tool events `tool`, `args`, plus `result` and `ok` after the
//! call — and sees `AIZEN_HOOK_EVENT` / `AIZEN_HOOK_TOOL` in its environment. It runs through
//! the sandbox runner like every other child, with the network allowed (a `stop` hook that posts
//! to a chat channel is the point) and Aizen's own secrets scrubbed from its environment.
//!
//! A hook that fails — cannot start, exits non-zero other than 2, or exceeds its wall clock
//! (`timeout_secs`, default [`DEFAULT_TIMEOUT_SECS`]) — never stops the run: the failure is
//! reported and the call proceeds as if the hook were absent. Only a deliberate deny blocks.
//! `AIZEN_NO_HOOKS=1` switches every hook off without editing the config.

use crate::core::cli_config::{self, Hook, HooksConfig};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Wall clock a hook gets when its entry sets no `timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Grace for the pipe readers after the tree is gone (see `core::proctree`).
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// A tool result longer than this (chars) is cut before it goes to a `post_tool` hook's stdin.
const RESULT_INPUT_CAP: usize = 32 * 1024;
/// A hook's context (its stdout, or its `context` field) is cut to this before it joins the tool
/// result the model reads — the same order of size as the result budget itself.
const CONTEXT_CAP: usize = 4 * 1024;
/// A deny reason is one message to the model, not a log.
const REASON_CAP: usize = 1_000;
/// The final answer handed to a `stop` hook is clipped to this.
const FINAL_TEXT_CAP: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreTool,
    PostTool,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::Stop => "stop",
        }
    }
}

/// What a hook decided, when it decided anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Where and for whom a hook runs.
#[derive(Debug, Clone)]
pub struct Context {
    /// The hook's working directory: the run's workspace root.
    pub cwd: PathBuf,
    /// The delegated sub-agent this call belongs to, if any (`None` for the top-level loop).
    pub dispatch: Option<String>,
    /// Suppress the human trace line (sub-agents run quiet; the report still reaches the JSON
    /// stream when it is on).
    pub quiet: bool,
}

impl Context {
    /// The driver's view: the config's workspace root and its execution context.
    pub fn from_cfg(cfg: &super::AgentConfig) -> Self {
        Self {
            cwd: cfg.effective_root(),
            dispatch: cfg.exec_ctx.dispatch_label(),
            quiet: cfg.quiet,
        }
    }

    /// A tool body's view: the process cwd and the execution context seeded for this call.
    pub fn here(quiet: bool) -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            dispatch: crate::core::exec_ctx::current().and_then(|c| c.dispatch_label()),
            quiet,
        }
    }
}

/// One finished hook, as reported to the transcript or the JSON stream.
#[derive(Debug, Clone)]
pub struct HookRun {
    pub event: HookEvent,
    pub run: String,
    pub decision: Option<Decision>,
    pub reason: Option<String>,
    pub context: Option<String>,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// What the `pre_tool` hooks of one call amount to. The first deny wins and stops the chain; an
/// allow is remembered across the chain; hooks that said nothing leave it `Pass`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolVerdict {
    Pass,
    Allow { run: String },
    Deny { run: String, reason: String },
}

/// The configured hooks, or `None` when there are none or `AIZEN_NO_HOOKS` is set.
pub fn configured() -> Option<HooksConfig> {
    if cli_config::branded_flag("NO_HOOKS") {
        return None;
    }
    cli_config::load().hooks.filter(|h| !h.is_empty())
}

/// Run the matching `pre_tool` hooks for `tool`.
pub fn pre_tool(tool: &str, args: &Value, ctx: &Context) -> PreToolVerdict {
    match configured() {
        Some(cfg) => pre_tool_with(&cfg, tool, args, ctx),
        None => PreToolVerdict::Pass,
    }
}

/// [`pre_tool`] against an explicit config.
pub fn pre_tool_with(cfg: &HooksConfig, tool: &str, args: &Value, ctx: &Context) -> PreToolVerdict {
    let mut verdict = PreToolVerdict::Pass;
    for h in cfg
        .pre_tool
        .iter()
        .filter(|h| matches(h.matches.as_deref(), tool))
    {
        let input = input(
            HookEvent::PreTool,
            ctx,
            json!({ "tool": tool, "args": args }),
        );
        let r = execute(h, HookEvent::PreTool, Some(tool), &input, ctx);
        report(&r, ctx);
        match r.decision {
            Some(Decision::Deny) => {
                return PreToolVerdict::Deny {
                    run: r.run,
                    reason: r.reason.unwrap_or_else(|| "denied by the hook".to_string()),
                }
            }
            Some(Decision::Allow) => verdict = PreToolVerdict::Allow { run: r.run },
            None => {}
        }
    }
    verdict
}

/// Run the matching `post_tool` hooks for a finished call. Returns the text to append to the tool
/// result — each hook's context under a `[hook `…`]` heading — or `None` when no hook said anything.
pub fn post_tool(
    tool: &str,
    args: &Value,
    result: &str,
    ok: bool,
    ctx: &Context,
) -> Option<String> {
    configured().and_then(|cfg| post_tool_with(&cfg, tool, args, result, ok, ctx))
}

/// [`post_tool`] against an explicit config.
pub fn post_tool_with(
    cfg: &HooksConfig,
    tool: &str,
    args: &Value,
    result: &str,
    ok: bool,
    ctx: &Context,
) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    for h in cfg
        .post_tool
        .iter()
        .filter(|h| matches(h.matches.as_deref(), tool))
    {
        let input = input(
            HookEvent::PostTool,
            ctx,
            json!({
                "tool": tool,
                "args": args,
                "result": clip(result, RESULT_INPUT_CAP),
                "ok": ok,
            }),
        );
        let r = execute(h, HookEvent::PostTool, Some(tool), &input, ctx);
        report(&r, ctx);
        if let Some(c) = &r.context {
            notes.push(format!("[hook `{}`]\n{c}", r.run));
        } else if r.decision == Some(Decision::Deny) {
            // A post hook cannot undo the call; a deny here is a flag the model should read.
            notes.push(format!(
                "[hook `{}` flagged this result]\n{}",
                r.run,
                r.reason.as_deref().unwrap_or("no reason given")
            ));
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n\n"))
    }
}

/// Run every `stop` hook for a finished top-level run. `stop` is the loop's stop reason word.
pub fn stop(stop: &str, steps: usize, final_text: Option<&str>, ctx: &Context) {
    let Some(cfg) = configured() else {
        return;
    };
    stop_with(&cfg, stop, steps, final_text, ctx);
}

/// [`stop`] against an explicit config.
pub fn stop_with(
    cfg: &HooksConfig,
    stop: &str,
    steps: usize,
    final_text: Option<&str>,
    ctx: &Context,
) {
    for h in &cfg.stop {
        let input = input(
            HookEvent::Stop,
            ctx,
            json!({
                "stop": stop,
                "steps": steps,
                "final_text": final_text.map(|t| clip(t, FINAL_TEXT_CAP)),
            }),
        );
        let r = execute(h, HookEvent::Stop, None, &input, ctx);
        report(&r, ctx);
    }
}

/// Run `f` from an async context without starving the runtime: hooks block on a child process,
/// and `gate_and_approve` / the turn's end live on the driver. On a multi-thread runtime the
/// worker is handed back for the duration; on a current-thread runtime (tests) the call is
/// inline, where `block_in_place` would panic.
pub fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// The stdin document every hook reads: the common fields plus the event's own.
fn input(event: HookEvent, ctx: &Context, extra: Value) -> Value {
    let mut v = json!({
        "event": event.as_str(),
        "cwd": ctx.cwd.display().to_string(),
        "session": crate::core::session_store::current_session_slug(),
        "dispatch": ctx.dispatch,
        "pid": std::process::id(),
        "time": chrono::Local::now().to_rfc3339(),
        "aizen": env!("CARGO_PKG_VERSION"),
    });
    if let (Some(base), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in more {
            base.insert(k.clone(), val.clone());
        }
    }
    v
}

/// Does a hook's `match` cover `tool`? Absent or blank ⇒ every tool. Otherwise `|`-separated
/// alternatives, each an exact name or a glob with `*` (`file_*`, `*_edit`, `*`).
pub(crate) fn matches(pattern: Option<&str>, tool: &str) -> bool {
    let Some(p) = pattern.map(str::trim).filter(|p| !p.is_empty()) else {
        return true;
    };
    p.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|alt| glob(alt, tool))
}

fn glob(pat: &str, s: &str) -> bool {
    if !pat.contains('*') {
        return pat == s;
    }
    let parts: Vec<&str> = pat.split('*').collect();
    let mut rest = s;
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else {
                return false;
            };
            rest = r;
        } else if i == last {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

fn clip(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(cap).collect();
        t.push_str("\n…[cut]");
        t
    }
}

/// Spawn one hook through the sandbox runner, feed it `input`, and read what it said.
fn execute(
    h: &Hook,
    event: HookEvent,
    tool: Option<&str>,
    input: &Value,
    ctx: &Context,
) -> HookRun {
    let started = Instant::now();
    let run = h.run.trim().to_string();
    let mut out = HookRun {
        event,
        run: run.clone(),
        decision: None,
        reason: None,
        context: None,
        exit: None,
        timed_out: false,
        error: None,
        elapsed_ms: 0,
    };
    if run.is_empty() {
        out.error = Some("empty `run`".to_string());
        return out;
    }
    let timeout = Duration::from_secs(h.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
    let mut env = vec![("AIZEN_HOOK_EVENT".to_string(), event.as_str().to_string())];
    if let Some(t) = tool {
        env.push(("AIZEN_HOOK_TOOL".to_string(), t.to_string()));
    }
    // The user's own command from the user's own file: the network stays open (a `stop` hook that
    // posts somewhere is the point) and the real temp dir is visible, as for a typed `!cmd`. The
    // env scrub still applies — a hook does not inherit Aizen's keys.
    let req = crate::sandbox::request::SandboxRequest::shell(
        crate::sandbox::CommandOrigin::Hook,
        run.as_str(),
        ctx.cwd.clone(),
        ctx.cwd.clone(),
    )
    .network(true)
    .private_tmp(false)
    .wall_timeout(timeout)
    .extra_env(env);
    let mut sbx = match crate::sandbox::runner::prepare_std(req) {
        Ok(s) => s,
        Err(e) => {
            out.error = Some(format!("refused by the sandbox policy: {e}"));
            out.elapsed_ms = started.elapsed().as_millis() as u64;
            return out;
        }
    };
    let bytes = serde_json::to_vec(input).unwrap_or_default();
    let bounded = crate::core::proctree::output_bounded_with_input(
        &mut sbx.command,
        &bytes,
        timeout,
        DRAIN_GRACE,
    );
    sbx.finish(match &bounded {
        Ok(o) if o.timed_out => crate::sandbox::runner::Outcome::Timeout,
        Ok(o) => crate::sandbox::runner::Outcome::Exit(o.code),
        Err(_) => crate::sandbox::runner::Outcome::SpawnFailed,
    });
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    match bounded {
        Err(e) => out.error = Some(format!("could not start: {e}")),
        Ok(o) => {
            out.exit = o.code;
            out.timed_out = o.timed_out;
            let v = interpret(o.code, o.timed_out, timeout, &o.stdout, &o.stderr);
            out.decision = v.decision;
            out.reason = v.reason;
            out.context = v.context;
            out.error = v.error;
        }
    }
    out
}

/// What a finished hook said, read from its exit code and output.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub decision: Option<Decision>,
    pub reason: Option<String>,
    pub context: Option<String>,
    pub error: Option<String>,
}

/// The exit-code contract. Exit `0`: stdout is either a JSON object (`decision`, `reason`,
/// `context`) or plain context text. Exit `2`: deny, with stderr (else stdout) as the reason. Any
/// other exit, a kill, or a timeout: a hook error — reported, never blocking.
pub(crate) fn interpret(
    code: Option<i32>,
    timed_out: bool,
    timeout: Duration,
    stdout: &str,
    stderr: &str,
) -> Verdict {
    if timed_out {
        return Verdict {
            error: Some(format!("timed out after {}s", timeout.as_secs())),
            ..Default::default()
        };
    }
    let text = stdout.trim();
    match code {
        Some(0) => {
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(text) {
                let decision = match m
                    .get("decision")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .as_deref()
                {
                    Some("allow") | Some("approve") => Some(Decision::Allow),
                    Some("deny") | Some("block") => Some(Decision::Deny),
                    _ => None,
                };
                let reason = m
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| clip(s, REASON_CAP));
                let context = m
                    .get("context")
                    .or_else(|| m.get("additional_context"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| clip(s, CONTEXT_CAP));
                Verdict {
                    decision,
                    reason,
                    context,
                    error: None,
                }
            } else {
                Verdict {
                    context: (!text.is_empty()).then(|| clip(text, CONTEXT_CAP)),
                    ..Default::default()
                }
            }
        }
        Some(2) => {
            let why = [stderr.trim(), text]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("hook exited 2");
            Verdict {
                decision: Some(Decision::Deny),
                reason: Some(clip(why, REASON_CAP)),
                ..Default::default()
            }
        }
        Some(c) => {
            let tail = stderr.trim();
            Verdict {
                error: Some(if tail.is_empty() {
                    format!("exited {c}")
                } else {
                    format!("exited {c}: {}", clip(tail, REASON_CAP))
                }),
                ..Default::default()
            }
        }
        None => Verdict {
            error: Some("killed before it exited".to_string()),
            ..Default::default()
        },
    }
}

/// One line per hook run: the JSON `hook` event when the stream is on, else a trace line.
fn report(r: &HookRun, ctx: &Context) {
    if crate::ui::events::on() {
        crate::ui::events::hook(json!({
            "event": r.event.as_str(),
            "run": r.run,
            "decision": r.decision.map(Decision::as_str),
            "reason": r.reason,
            "context": r.context,
            "exit": r.exit,
            "timed_out": r.timed_out,
            "error": r.error,
            "elapsed_ms": r.elapsed_ms,
        }));
        return;
    }
    if ctx.quiet {
        return;
    }
    let what = if let Some(e) = &r.error {
        format!("failed — {e}")
    } else {
        match r.decision {
            Some(Decision::Deny) => match &r.reason {
                Some(why) => format!("denied — {why}"),
                None => "denied".to_string(),
            },
            Some(Decision::Allow) => "allowed".to_string(),
            None if r.context.is_some() && r.event == HookEvent::PostTool => {
                "ok, output attached to the result".to_string()
            }
            None => "ok".to_string(),
        }
    };
    super::emit_trace_public(&format!(
        "→ hook `{}` ({}): {what} · {}ms",
        r.run,
        r.event.as_str(),
        r.elapsed_ms
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(run: &str, matches: Option<&str>) -> Hook {
        Hook {
            run: run.to_string(),
            matches: matches.map(str::to_string),
            timeout_secs: Some(20),
        }
    }

    fn ctx() -> Context {
        Context {
            cwd: std::env::temp_dir(),
            dispatch: None,
            quiet: true,
        }
    }

    #[test]
    fn a_match_is_a_name_a_glob_or_alternatives() {
        assert!(matches(None, "shell_run"));
        assert!(matches(Some(""), "shell_run"));
        assert!(matches(Some(" * "), "shell_run"));
        assert!(matches(Some("shell_run"), "shell_run"));
        assert!(!matches(Some("shell_run"), "shell_runner"));
        assert!(matches(Some("file_*"), "file_edit"));
        assert!(!matches(Some("file_*"), "shell_run"));
        assert!(matches(Some("*_edit"), "file_edit"));
        assert!(matches(Some("f*_e*t"), "file_edit"));
        assert!(!matches(Some("f*_e*x"), "file_edit"));
        assert!(matches(Some("shell_run | file_edit"), "file_edit"));
        assert!(
            !matches(Some("Shell_Run"), "shell_run"),
            "names are case-sensitive"
        );
    }

    #[test]
    fn exit_two_is_a_deny_with_stderr_as_the_reason() {
        let v = interpret(
            Some(2),
            false,
            Duration::from_secs(1),
            "",
            "rm on a tracked file\n",
        );
        assert_eq!(v.decision, Some(Decision::Deny));
        assert_eq!(v.reason.as_deref(), Some("rm on a tracked file"));
        assert!(v.error.is_none());
        let bare = interpret(Some(2), false, Duration::from_secs(1), "", "");
        assert_eq!(bare.reason.as_deref(), Some("hook exited 2"));
    }

    #[test]
    fn exit_zero_reads_a_json_verdict_or_plain_context() {
        let v = interpret(
            Some(0),
            false,
            Duration::from_secs(1),
            r#"{"decision":"ALLOW","reason":"trusted script"}"#,
            "",
        );
        assert_eq!(v.decision, Some(Decision::Allow));
        assert_eq!(v.reason.as_deref(), Some("trusted script"));
        let d = interpret(
            Some(0),
            false,
            Duration::from_secs(1),
            r#"{"decision":"deny","reason":"nope","context":"ignored on a deny? no — kept"}"#,
            "",
        );
        assert_eq!(d.decision, Some(Decision::Deny));
        assert!(d.context.is_some());
        let plain = interpret(
            Some(0),
            false,
            Duration::from_secs(1),
            "  fmt: 2 files\n",
            "",
        );
        assert_eq!(plain.decision, None);
        assert_eq!(plain.context.as_deref(), Some("fmt: 2 files"));
        let silent = interpret(Some(0), false, Duration::from_secs(1), "", "warn on stderr");
        assert_eq!(silent, Verdict::default(), "stderr alone is not context");
    }

    #[test]
    fn other_failures_are_errors_not_decisions() {
        let e = interpret(Some(1), false, Duration::from_secs(1), "", "boom");
        assert_eq!(e.decision, None);
        assert_eq!(e.error.as_deref(), Some("exited 1: boom"));
        let t = interpret(None, true, Duration::from_secs(7), "", "");
        assert_eq!(t.error.as_deref(), Some("timed out after 7s"));
        let k = interpret(None, false, Duration::from_secs(1), "", "");
        assert!(k.error.as_deref().unwrap().contains("killed"));
    }

    #[test]
    fn a_long_context_is_cut() {
        let big = "x".repeat(CONTEXT_CAP + 50);
        let v = interpret(Some(0), false, Duration::from_secs(1), &big, "");
        let c = v.context.unwrap();
        assert!(c.ends_with("…[cut]"));
        assert!(c.chars().count() < big.len());
    }

    #[test]
    fn the_input_carries_the_event_and_the_common_fields() {
        let c = Context {
            cwd: PathBuf::from("/work"),
            dispatch: Some("argus · t1".to_string()),
            quiet: true,
        };
        let v = input(
            HookEvent::PreTool,
            &c,
            json!({ "tool": "shell_run", "args": { "command": "ls" } }),
        );
        assert_eq!(v["event"], "pre_tool");
        assert_eq!(v["tool"], "shell_run");
        assert_eq!(v["args"]["command"], "ls");
        assert_eq!(v["dispatch"], "argus · t1");
        assert_eq!(v["cwd"], "/work");
        assert!(v["time"].is_string());
    }

    /// The contract end to end, through the sandbox runner and a real shell: a hook that exits 2
    /// denies the call, and it did so having READ the event on stdin (the grep is the proof).
    #[test]
    fn a_real_hook_reads_stdin_and_can_deny() {
        let line = if cfg!(windows) {
            "findstr pre_tool >nul && exit 2"
        } else {
            "grep -q pre_tool && exit 2"
        };
        let cfg = HooksConfig {
            pre_tool: vec![hook(line, Some("shell_run"))],
            ..Default::default()
        };
        let v = pre_tool_with(&cfg, "shell_run", &json!({ "command": "ls" }), &ctx());
        match v {
            PreToolVerdict::Deny { run, reason } => {
                assert_eq!(run, line);
                assert_eq!(reason, "hook exited 2");
            }
            other => panic!("expected a deny, got {other:?}"),
        }
        // The same hook does not fire for a tool its `match` excludes.
        let v = pre_tool_with(&cfg, "file_read", &json!({ "path": "x" }), &ctx());
        assert_eq!(v, PreToolVerdict::Pass);
    }

    #[test]
    fn a_post_hook_s_output_joins_the_result() {
        let cfg = HooksConfig {
            post_tool: vec![hook("echo checked by the hook", None)],
            ..Default::default()
        };
        let note = post_tool_with(&cfg, "file_edit", &json!({}), "edited x.rs", true, &ctx())
            .expect("the echo is context");
        assert!(
            note.starts_with("[hook `echo checked by the hook`]\n"),
            "{note}"
        );
        assert!(note.contains("checked by the hook"));
    }

    #[test]
    fn a_failing_hook_never_blocks() {
        let cfg = HooksConfig {
            pre_tool: vec![hook("exit 3", None)],
            ..Default::default()
        };
        assert_eq!(
            pre_tool_with(&cfg, "shell_run", &json!({}), &ctx()),
            PreToolVerdict::Pass
        );
        let empty = HooksConfig {
            pre_tool: vec![hook("   ", None)],
            ..Default::default()
        };
        assert_eq!(
            pre_tool_with(&empty, "shell_run", &json!({}), &ctx()),
            PreToolVerdict::Pass
        );
    }
}
