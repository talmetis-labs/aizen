//! The machine-readable stream behind `aizen agent --output-format stream-json`, and the single
//! closing object of `--output-format json`.
//!
//! The wire shape is Claude Code's `stream-json` — the message types of the Claude Agent SDK —
//! so whatever already reads Claude Code (a desktop app, an editor extension, a CI script, a
//! `jq` one-liner) reads an aizen run without a second parser:
//!
//! - `system` / `init` first: model, cwd, the advertised tools, the permission mode.
//! - `stream_event` per streamed fragment (`content_block_delta` with a `text_delta` or a
//!   `thinking_delta`), then one `assistant` record per finished content block — a `text`,
//!   `thinking` or `tool_use` block inside a Messages-API-shaped message. The blocks of one model
//!   turn share a `message.id`.
//! - `user` records carry `tool_result` blocks, paired to their `tool_use` by `tool_use_id`.
//!   Aizen's own facts about the call (name, target, digest, elapsed, dispatch) ride in
//!   `tool_use_result`, the SDK's per-tool field.
//! - A delegated child's records set `parent_tool_use_id` to the `task` / `workflow` call that
//!   spawned it and name the child in `dispatch`; the loop's own records carry `null`.
//! - A destructive call asks with `control_request` (`can_use_tool`) and blocks until a
//!   `control_response` arrives on stdin; a refusal is a `system` / `permission_denied` record
//!   and a row in the result's `permission_denials`.
//! - `system` subtypes the SDK defines are used wherever aizen has the fact (`compact_boundary`,
//!   `hook_response`, `informational` for trace and warning lines); aizen-only facts are extra
//!   subtypes (`plan`, `diff`, `verify`, `session_saved`, `session_not_saved`) a reader may skip.
//! - `result` last: `success` / `error_max_turns` / `error_during_execution`, the answer, the
//!   step count, this run's tokens — plus aizen's `stop` word and the saved session's name.
//!
//! Fields are only ever added; a reader ignores what it does not know. Off by default:
//! [`enable`] is called once by the one-shot runner, and every surface that would otherwise print
//! for a human — trace lines, tool rows, the streamed answer, the plan box, the approval
//! question — checks [`on`] first and hands the same information here as data.

use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const OFF: u8 = 0;
/// `stream-json`: every record, as it happens.
const STREAM: u8 = 1;
/// `json`: only the closing `result` object.
const FINAL: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(OFF);
/// Set by a `bypassPermissions` reply: every later approval is answered without asking.
static ALLOW_ALL: AtomicBool = AtomicBool::new(false);
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);
static MSG_SEQ: AtomicU64 = AtomicU64::new(0);
static HOOK_SEQ: AtomicU64 = AtomicU64::new(0);
/// Lines are written whole: tool bodies emit from blocking threads while the driver streams text,
/// and two half-lines interleaved would be two broken records.
static WRITE: Mutex<()> = Mutex::new(());
static RUN: Mutex<Run> = Mutex::new(Run::new());

/// A tool result longer than this (chars) is cut in the `tool_result` block, with
/// `tool_use_result.truncated: true`. The model's own copy is budgeted separately
/// (`AgentConfig::max_tool_result_chars`).
pub const OUTPUT_CAP: usize = 64 * 1024;

/// The fragments streamed so far by one speaker — the loop itself (`who: None`) or one delegated
/// child — flushed as whole content blocks of one assistant message.
struct Pending {
    who: Option<String>,
    msg_id: Option<String>,
    text: String,
    thinking: String,
}

/// Everything the stream remembers about the run in progress.
struct Run {
    session_id: Option<String>,
    started: Option<Instant>,
    model: String,
    pending: Vec<Pending>,
    /// `seq` of an open tool call → its `tool_use_id`.
    ids: Vec<(u64, String)>,
    /// Open `task` / `workflow` calls of the loop's own, newest last: a child's records point at
    /// the newest one.
    parents: Vec<(u64, String)>,
    /// Ids handed out at approval time — `(tool, args, tool_use_id, request_id)` — claimed by
    /// the matching `tool_call` right after.
    approved: Vec<(String, Value, String, String)>,
    /// `permission_denials` rows for the result.
    denials: Vec<Value>,
    /// The answer text the loop's own `text` blocks carried so far, so a final answer that never
    /// streamed (a non-streaming provider) still goes out as a block before the result.
    spoken: String,
    seed: u64,
}

impl Run {
    const fn new() -> Self {
        Self {
            session_id: None,
            started: None,
            model: String::new(),
            pending: Vec::new(),
            ids: Vec::new(),
            parents: Vec::new(),
            approved: Vec::new(),
            denials: Vec::new(),
            spoken: String::new(),
            seed: 0,
        }
    }
}

/// Switch the process to machine-readable output: the full stream (`stream-json`) or only the
/// closing `result` object (`json`). Irreversible for the process: a one-shot run is one turn.
pub fn enable(streaming: bool) {
    MODE.store(if streaming { STREAM } else { FINAL }, Ordering::Relaxed);
}

/// Whether machine-readable output owns stdout (either format).
pub fn on() -> bool {
    MODE.load(Ordering::Relaxed) != OFF
}

/// Whether every record goes out as it happens (`stream-json`), not only the closing `result`.
pub fn streaming() -> bool {
    MODE.load(Ordering::Relaxed) == STREAM
}

fn lock() -> std::sync::MutexGuard<'static, Run> {
    RUN.lock().unwrap_or_else(|e| e.into_inner())
}

// ── ids ──────────────────────────────────────────────────────────────────────

/// SplitMix64 over a seed drawn once from the clock and the pid — good enough for ids that only
/// have to be unique within one process's output, without a dependency for it.
fn rnd(run: &mut Run) -> u64 {
    if run.seed == 0 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        run.seed = nanos ^ ((std::process::id() as u64) << 32) ^ 0x2545_F491_4F6C_DD1D;
        if run.seed == 0 {
            run.seed = 1;
        }
    }
    run.seed = run.seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = run.seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A version-4-shaped uuid.
fn uuid(run: &mut Run) -> String {
    let a = rnd(run);
    let b = rnd(run);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xfff,
        0x8000 | ((b >> 48) & 0x3fff),
        b & 0xffff_ffff_ffff
    )
}

fn session_id(run: &mut Run) -> String {
    if let Some(s) = &run.session_id {
        return s.clone();
    }
    let s = uuid(run);
    run.session_id = Some(s.clone());
    s
}

fn new_msg_id() -> String {
    format!("msg_{:04}", MSG_SEQ.fetch_add(1, Ordering::Relaxed) + 1)
}

// ── writing ──────────────────────────────────────────────────────────────────

fn write_line(line: &str) {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// The record `kind` + `fields` serialize to, stamped with `uuid` and `session_id`: compact JSON,
/// so an embedded newline in any string is escaped and one record is always one physical line.
fn record(run: &mut Run, kind: &str, fields: Value) -> Value {
    let mut obj = match fields {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    obj.insert("type".to_string(), Value::String(kind.to_string()));
    obj.insert("uuid".to_string(), Value::String(uuid(run)));
    obj.insert("session_id".to_string(), Value::String(session_id(run)));
    Value::Object(obj)
}

/// Write one record while holding the run state. `always` records (the closing `result`) go out
/// in both formats; everything else only on the stream.
fn emit_locked(run: &mut Run, kind: &str, fields: Value, always: bool) {
    if !on() || (!always && !streaming()) {
        return;
    }
    let line = record(run, kind, fields).to_string();
    write_line(&line);
}

fn emit(kind: &str, fields: Value) {
    if !streaming() {
        return;
    }
    let mut run = lock();
    emit_locked(&mut run, kind, fields, false);
}

/// `text` with every ANSI sequence removed, trailing whitespace dropped per line, and blank lines
/// gone — what a trace line looks like once it is data rather than a styled terminal row.
pub(crate) fn plain(text: &str) -> String {
    let stripped = console::strip_ansi_codes(text);
    stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn clip(s: &str, cap: usize) -> (String, bool) {
    if s.chars().count() <= cap {
        (s.to_string(), false)
    } else {
        (s.chars().take(cap).collect(), true)
    }
}

/// Which speaker the current thread is: a delegated child's label, or `None` for the loop's own.
fn who_here() -> Option<String> {
    crate::core::exec_ctx::current().and_then(|c| c.dispatch_label())
}

// ── assistant messages ───────────────────────────────────────────────────────

fn pending_index(run: &mut Run, who: Option<&str>) -> usize {
    if let Some(i) = run.pending.iter().position(|p| p.who.as_deref() == who) {
        return i;
    }
    run.pending.push(Pending {
        who: who.map(str::to_string),
        msg_id: None,
        text: String::new(),
        thinking: String::new(),
    });
    run.pending.len() - 1
}

/// The `parent_tool_use_id` of a record spoken by `who`: a child's records point at the newest
/// open `task` / `workflow` call; the loop's own carry `null`.
fn parent_for(run: &Run, who: Option<&str>) -> Option<String> {
    who?;
    run.parents.last().map(|(_, id)| id.clone())
}

/// One `assistant` record: a Messages-API-shaped message holding exactly one content block.
fn assistant_record(
    run: &mut Run,
    msg_id: &str,
    block: Value,
    parent: Option<&str>,
    who: Option<&str>,
    extra: Map<String, Value>,
) -> Value {
    let mut fields = extra;
    fields.insert(
        "message".to_string(),
        json!({
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "model": run.model,
            "content": [block],
            "stop_reason": Value::Null,
        }),
    );
    fields.insert("parent_tool_use_id".to_string(), json!(parent));
    fields.insert("dispatch".to_string(), json!(who));
    record(run, "assistant", Value::Object(fields))
}

/// Flush `who`'s streamed fragments as whole `thinking` / `text` blocks. Returns the message id
/// they went out under (allocated here when the first fragment of a turn arrives), so a
/// `tool_use` block that follows can share it. `close` forgets the id afterwards: the next
/// fragment starts a new message.
fn flush_locked(run: &mut Run, who: Option<&str>, close: bool) -> String {
    let i = pending_index(run, who);
    let thinking = std::mem::take(&mut run.pending[i].thinking);
    let text = std::mem::take(&mut run.pending[i].text);
    let msg_id = run.pending[i].msg_id.clone().unwrap_or_else(new_msg_id);
    run.pending[i].msg_id = if close { None } else { Some(msg_id.clone()) };
    let parent = parent_for(run, who);
    if !thinking.trim().is_empty() {
        let v = assistant_record(
            run,
            &msg_id,
            json!({ "type": "thinking", "thinking": thinking }),
            parent.as_deref(),
            who,
            Map::new(),
        );
        if streaming() {
            write_line(&v.to_string());
        }
    }
    if !text.is_empty() {
        if who.is_none() {
            run.spoken.push_str(&text);
        }
        let v = assistant_record(
            run,
            &msg_id,
            json!({ "type": "text", "text": text }),
            parent.as_deref(),
            who,
            Map::new(),
        );
        if streaming() {
            write_line(&v.to_string());
        }
    }
    msg_id
}

fn flush_all(run: &mut Run) {
    let speakers: Vec<Option<String>> = run.pending.iter().map(|p| p.who.clone()).collect();
    for who in speakers {
        flush_locked(run, who.as_deref(), true);
    }
}

/// A streamed fragment: one `stream_event` carrying a Messages API `content_block_delta`.
fn delta_locked(run: &mut Run, who: Option<&str>, delta: Value) {
    let parent = parent_for(run, who);
    let i = pending_index(run, who);
    if run.pending[i].msg_id.is_none() {
        run.pending[i].msg_id = Some(new_msg_id());
    }
    emit_locked(
        run,
        "stream_event",
        json!({
            "event": { "type": "content_block_delta", "index": 0, "delta": delta },
            "parent_tool_use_id": parent,
            "dispatch": who,
        }),
        false,
    );
}

// ── the records ──────────────────────────────────────────────────────────────

/// The first record of a run: `system` / `init` — what is about to happen and with what.
/// `tools` are the names advertised to the model on the first request; `approval` is aizen's
/// approval mode (`ask` / `smart` / `yolo`), also expressed as the SDK's `permissionMode`.
pub fn start(
    model: &str,
    cwd: &str,
    effort: Option<&str>,
    images: usize,
    tools: &[String],
    approval: &str,
) {
    if !on() {
        return;
    }
    let mut run = lock();
    run.started = Some(Instant::now());
    run.model = model.to_string();
    let permission_mode = match approval {
        "yolo" => "bypassPermissions",
        _ => "default",
    };
    emit_locked(
        &mut run,
        "system",
        json!({
            "subtype": "init",
            "cwd": cwd,
            "model": model,
            "tools": tools,
            "mcp_servers": [],
            "permissionMode": permission_mode,
            "approval_mode": approval,
            "agent": "aizen",
            "version": env!("CARGO_PKG_VERSION"),
            "effort": effort,
            "images": images,
            "pid": std::process::id(),
        }),
        false,
    );
}

/// A fragment of the assistant's answer, in the order it streamed: a `text_delta`. The whole
/// block follows as an `assistant` record when the turn moves on.
pub fn text(delta: &str) {
    if delta.is_empty() || !streaming() {
        return;
    }
    let who = who_here();
    let mut run = lock();
    let i = pending_index(&mut run, who.as_deref());
    run.pending[i].text.push_str(delta);
    delta_locked(
        &mut run,
        who.as_deref(),
        json!({ "type": "text_delta", "text": delta }),
    );
}

/// A fragment of the model's reasoning channel, when the provider exposes one: a
/// `thinking_delta`, then a `thinking` block. Never part of the answer.
pub fn reasoning(delta: &str) {
    if delta.is_empty() || !streaming() {
        return;
    }
    let who = who_here();
    let mut run = lock();
    let i = pending_index(&mut run, who.as_deref());
    run.pending[i].thinking.push_str(delta);
    delta_locked(
        &mut run,
        who.as_deref(),
        json!({ "type": "thinking_delta", "thinking": delta }),
    );
}

/// A line the human transcript would have shown as progress (`→ checkpoint #3 saved`, a retry
/// note): `system` / `informational` at level `info`. Styling stripped; empty lines dropped.
pub fn trace(text: &str) {
    let clean = plain(text);
    if !clean.is_empty() {
        emit(
            "system",
            json!({ "subtype": "informational", "level": "info", "content": clean }),
        );
    }
}

/// A safety-floor or sandbox notice about a call — `system` / `informational` at level
/// `warning`; `kind` is `blocked`, `caution` or `network`.
pub fn warning(kind: &str, text: &str) {
    emit(
        "system",
        json!({ "subtype": "informational", "level": "warning", "kind": kind, "content": plain(text) }),
    );
}

/// The ids a `tool_call` with this `seq` goes out under, when the call was approved a moment
/// ago: the approval's `tool_use_id` and `request_id`, so the `control_request` and the
/// `tool_use` block agree and a reader can close the question it drew.
fn claim_approved_id(run: &mut Run, name: &str, args: &Value) -> Option<(String, String)> {
    let i = run
        .approved
        .iter()
        .position(|(n, a, _, _)| n == name && a == args)?;
    let (_, _, tool_use_id, request_id) = run.approved.remove(i);
    Some((tool_use_id, request_id))
}

/// A tool call is starting: an `assistant` record with one `tool_use` block. `seq` ties the
/// later result to this call; `dispatch` names the delegated child whose call it is, `None`
/// for the loop's own. A child's calls share the stream with its parent's and its siblings',
/// interleaved — pair a result with its call by `tool_use_id`, never by order.
pub fn tool_call(seq: u64, name: &str, args: &Value, target: &str, dispatch: Option<&str>) {
    if !streaming() {
        return;
    }
    let mut run = lock();
    let msg_id = flush_locked(&mut run, dispatch, false);
    let approved = claim_approved_id(&mut run, name, args);
    let id = approved
        .as_ref()
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| format!("toolu_{seq}"));
    run.ids.push((seq, id.clone()));
    let parent = parent_for(&run, dispatch);
    let mut extra = Map::new();
    extra.insert("target".to_string(), json!(target));
    if let Some((_, request_id)) = approved {
        // The call a `control_request` asked about is running: the question is answered.
        extra.insert("request_id".to_string(), json!(request_id));
    }
    let v = assistant_record(
        &mut run,
        &msg_id,
        json!({ "type": "tool_use", "id": id, "name": name, "input": args }),
        parent.as_deref(),
        dispatch,
        extra,
    );
    write_line(&v.to_string());
    if dispatch.is_none() && (name == "task" || name == "workflow") {
        run.parents.push((seq, id));
    }
}

/// A tool call finished: a `user` record with one `tool_result` block. `content` is the raw
/// result (cut at [`OUTPUT_CAP`]) before the model's own budget is applied; `tool_use_result`
/// carries the one-line digest the transcript shows, the target, the elapsed time and the
/// dispatch label.
#[allow(clippy::too_many_arguments)]
pub fn tool_result(
    seq: u64,
    name: &str,
    target: &str,
    ok: bool,
    digest: &str,
    elapsed_ms: Option<u64>,
    output: &str,
    dispatch: Option<&str>,
) {
    if !streaming() {
        return;
    }
    let mut run = lock();
    // The model's turn ended before its tools ran: close its message so the next fragment
    // starts a new one.
    flush_locked(&mut run, dispatch, true);
    let id = match run.ids.iter().position(|(s, _)| *s == seq) {
        Some(i) => run.ids.remove(i).1,
        None => format!("toolu_{seq}"),
    };
    let parent = parent_for(&run, dispatch);
    if let Some(i) = run.parents.iter().position(|(s, _)| *s == seq) {
        run.parents.remove(i);
    }
    let (out, truncated) = clip(output, OUTPUT_CAP);
    emit_locked(
        &mut run,
        "user",
        json!({
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": out,
                    "is_error": !ok,
                }],
            },
            "parent_tool_use_id": parent,
            "dispatch": dispatch,
            "tool_use_result": {
                "name": name,
                "target": target,
                "digest": digest,
                "elapsed_ms": elapsed_ms,
                "truncated": truncated,
                "dispatch": dispatch,
            },
        }),
        false,
    );
}

/// The plan checklist as the agent last wrote it — `system` / `plan`. `items` = `(status, text)`
/// with status 0 / 1 / 2 = pending / in progress / done, the same rows the panel renders.
pub fn plan(items: &[(u8, String)]) {
    emit(
        "system",
        json!({ "subtype": "plan", "items": plan_rows(items) }),
    );
}

/// The `plan` record's rows: the panel's numeric status as a word.
pub(crate) fn plan_rows(items: &[(u8, String)]) -> Vec<Value> {
    items
        .iter()
        .map(|(s, t)| {
            let status = match s {
                2 => "done",
                1 => "in_progress",
                _ => "pending",
            };
            json!({ "status": status, "text": t })
        })
        .collect()
}

/// A verify-gate line — `system` / `verify`: the command that ran and its verdict.
pub fn verify(command: &str, detail: &str) {
    emit(
        "system",
        json!({ "subtype": "verify", "command": command, "detail": plain(detail) }),
    );
}

/// The size of an edit — `system` / `diff` — for a reader that draws its own diff from the
/// tool result.
pub fn diff(path: &str, added: usize, removed: usize) {
    emit(
        "system",
        json!({ "subtype": "diff", "path": path, "added": added, "removed": removed }),
    );
}

/// A user hook ran (see `agent::hooks`) — the SDK's `system` / `hook_response`, with aizen's
/// `decision`, `reason`, `timed_out` and `elapsed_ms` alongside. `fields` is the hook's own
/// report: `event`, `run`, `decision`, `reason`, `context`, `exit`, `timed_out`, `error`,
/// `elapsed_ms`.
pub fn hook(fields: Value) {
    if !streaming() {
        return;
    }
    let s = |k: &str| fields.get(k).and_then(Value::as_str).map(str::to_string);
    let event = s("event").unwrap_or_default();
    let error = s("error");
    let timed_out = fields
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let context = s("context").unwrap_or_default();
    let outcome = if error.is_some() || timed_out {
        "error"
    } else {
        "success"
    };
    let n = HOOK_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    emit(
        "system",
        json!({
            "subtype": "hook_response",
            "hook_id": format!("{event}#{n}"),
            "hook_name": s("run"),
            "hook_event": event,
            "output": context,
            "stdout": context,
            "stderr": error.clone().unwrap_or_default(),
            "exit_code": fields.get("exit"),
            "outcome": outcome,
            "decision": fields.get("decision"),
            "reason": fields.get("reason"),
            "timed_out": timed_out,
            "error": error,
            "elapsed_ms": fields.get("elapsed_ms"),
        }),
    );
}

/// Older turns were summarized in place — the SDK's `system` / `compact_boundary`.
pub fn compact_boundary(pre_tokens: usize, post_tokens: usize) {
    emit(
        "system",
        json!({
            "subtype": "compact_boundary",
            "compact_metadata": { "trigger": "auto", "pre_tokens": pre_tokens, "post_tokens": post_tokens },
        }),
    );
}

/// The finished conversation was written to the session pool — `system` / `session_saved`.
pub fn session_saved(slug: &str, path: &str) {
    emit(
        "system",
        json!({ "subtype": "session_saved", "slug": slug, "path": path }),
    );
}

/// The conversation could not be saved — `system` / `session_not_saved`. Said, never silent: a
/// caller may be about to reopen it.
pub fn session_not_saved(error: &str) {
    emit(
        "system",
        json!({ "subtype": "session_not_saved", "error": error }),
    );
}

/// The SDK `result` subtype and `is_error` for one of aizen's stop words (`done`, `divergence`,
/// `max_iters`, `verification_failed`, `awaiting_input`, `cancelled`, `deadline`).
pub(crate) fn result_kind(stop: &str) -> (&'static str, bool) {
    match stop {
        "done" | "awaiting_input" => ("success", false),
        "max_iters" => ("error_max_turns", true),
        _ => ("error_during_execution", true),
    }
}

/// Aizen's usage summary (`calls`, `input` with cache reads and writes included, `output`,
/// `cached`, `cache_write`) as the Messages API `usage` object: `input_tokens` EXCLUDES what
/// was read from or written to the cache, exactly as the API reports it.
pub(crate) fn usage_fields(usage: &Value) -> Value {
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = n("cached");
    let cache_write = n("cache_write");
    json!({
        "input_tokens": n("input").saturating_sub(cached).saturating_sub(cache_write),
        "output_tokens": n("output"),
        "cache_read_input_tokens": cached,
        "cache_creation_input_tokens": cache_write,
    })
}

fn elapsed_ms(run: &Run) -> u64 {
    run.started
        .map(|t| t.elapsed().as_millis() as u64)
        .unwrap_or(0)
}

/// The closing `result` record of a run that finished (in either format). `stop` is the loop's
/// stop reason; `question` is set with `awaiting_input`; `session` is the saved session's slug;
/// `usage` sums the provider's reported tokens over the run.
pub fn done(
    stop: &str,
    steps: usize,
    final_text: Option<&str>,
    question: Option<&str>,
    session: Option<&str>,
    usage: Value,
) {
    if !on() {
        return;
    }
    let mut run = lock();
    flush_all(&mut run);
    // A provider that did not stream left the whole answer for here: say it as the block the
    // stream would have carried, so a reader that renders `assistant` text sees it.
    if let Some(answer) = final_text.filter(|t| !t.trim().is_empty()) {
        if !run.spoken.ends_with(answer) {
            let i = pending_index(&mut run, None);
            run.pending[i].text.push_str(answer);
            flush_locked(&mut run, None, true);
        }
    }
    let (subtype, is_error) = result_kind(stop);
    let mut fields = json!({
        "subtype": subtype,
        "is_error": is_error,
        "duration_ms": elapsed_ms(&run),
        "num_turns": steps,
        "result": final_text.unwrap_or(""),
        "stop_reason": Value::Null,
        "usage": usage_fields(&usage),
        "permission_denials": run.denials,
        "stop": stop,
        "question": question,
        "session": session,
        "calls": usage.get("calls").cloned().unwrap_or(json!(0)),
    });
    if is_error {
        fields["errors"] = json!([stop]);
    }
    emit_locked(&mut run, "result", fields, true);
}

/// A fatal error: the run ends after this record with a non-zero exit. A `result` of subtype
/// `error_during_execution` whose `errors` names the failure.
pub fn error(message: &str) {
    if !on() {
        return;
    }
    let mut run = lock();
    flush_all(&mut run);
    let fields = json!({
        "subtype": "error_during_execution",
        "is_error": true,
        "duration_ms": elapsed_ms(&run),
        "num_turns": 0,
        "result": "",
        "stop_reason": Value::Null,
        "usage": usage_fields(&Value::Null),
        "permission_denials": run.denials,
        "errors": [message],
        "stop": "error",
    });
    emit_locked(&mut run, "result", fields, true);
}

// ── approvals over stdin ─────────────────────────────────────────────────────

/// What a caller answered to a `can_use_tool` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run this call.
    Allow,
    /// Run this call, and every later call of the same tool without asking (`addRules`).
    AllowTool,
    /// Run this call, and every later destructive call without asking (`setMode` to
    /// `bypassPermissions` — like `-y` from here on).
    AllowAll,
    /// Refuse this call; the model is told the user declined.
    Deny,
}

/// Whether an earlier `bypassPermissions` reply stands.
pub fn allow_all() -> bool {
    ALLOW_ALL.load(Ordering::Relaxed)
}

fn record_denial(
    run: &mut Run,
    tool: &str,
    tool_use_id: &str,
    request_id: &str,
    args: &Value,
    message: &str,
) {
    run.denials.push(json!({
        "tool_name": tool,
        "tool_use_id": tool_use_id,
        "tool_input": args,
        "message": message,
    }));
    emit_locked(
        run,
        "system",
        json!({
            "subtype": "permission_denied",
            "tool_name": tool,
            "tool_use_id": tool_use_id,
            "request_id": request_id,
            "message": message,
        }),
        false,
    );
}

/// Ask the caller to approve a destructive call: write a `control_request` (`can_use_tool`)
/// and block until the matching `control_response` arrives on stdin. Lines that are not a reply
/// to this request are ignored. EOF or a read error is a deny — the same safe default a non-TTY
/// run has always had. Under `--output-format json` there is no channel to ask on, so the call
/// is refused outright (pass `--yes` to pre-approve).
///
/// `who` names a delegated child when the request is a child's; `preview` is the call's
/// pre-flight payload (`{title, lines, diff}`), when the tool can compute one.
pub fn ask_approval(tool: &str, args: &Value, who: Option<&str>, preview: Option<Value>) -> bool {
    if allow_all() {
        return true;
    }
    let n = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let request_id = format!("req_{n}");
    let tool_use_id = format!("toolu_a{n}");
    if !streaming() {
        let mut run = lock();
        record_denial(
            &mut run,
            tool,
            &tool_use_id,
            &request_id,
            args,
            "no approval channel under --output-format json (pass --yes to pre-approve)",
        );
        return false;
    }
    {
        let mut run = lock();
        flush_all(&mut run);
        run.approved.push((
            tool.to_string(),
            args.clone(),
            tool_use_id.clone(),
            request_id.clone(),
        ));
        let title = preview
            .as_ref()
            .and_then(|p| p.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string);
        emit_locked(
            &mut run,
            "control_request",
            json!({
                "request_id": request_id,
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": tool,
                    "input": args,
                    "tool_use_id": tool_use_id,
                    "agent_id": who,
                    "title": title,
                    "description": preview.as_ref().and_then(|p| p.get("lines")).and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")),
                    "preview": preview,
                },
            }),
            false,
        );
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => {
                let mut run = lock();
                record_denial(
                    &mut run,
                    tool,
                    &tool_use_id,
                    &request_id,
                    args,
                    "stdin closed",
                );
                return false;
            }
            Ok(_) => {}
        }
        if let Some((d, message)) = parse_reply(&line, &request_id, tool) {
            match d {
                Decision::AllowTool => crate::core::approval::grant_session(tool, None),
                Decision::AllowAll => ALLOW_ALL.store(true, Ordering::Relaxed),
                Decision::Allow | Decision::Deny => {}
            }
            if d == Decision::Deny {
                let mut run = lock();
                let why = message.unwrap_or_else(|| "the user declined this action".to_string());
                record_denial(&mut run, tool, &tool_use_id, &request_id, args, &why);
                return false;
            }
            return true;
        }
    }
}

/// Parse one stdin line as the `control_response` to `request_id`. `None` for anything that is
/// not a reply to this request (another type, another id, not JSON) — the caller keeps reading.
/// `behavior: allow` with `updatedPermissions` widens the grant: a `setMode` to
/// `bypassPermissions` is allow-all, an `addRules` allow rule naming `tool` is allow-tool. An
/// `error` response, a `deny`, or a behaviour that is not a known word is a deny: an
/// unrecognised answer must never run the call. The deny's `message` comes back with it.
pub(crate) fn parse_reply(
    line: &str,
    request_id: &str,
    tool: &str,
) -> Option<(Decision, Option<String>)> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let r = v.get("response")?;
    if let Some(id) = r.get("request_id").and_then(Value::as_str) {
        if id != request_id {
            return None;
        }
    }
    if r.get("subtype").and_then(Value::as_str) == Some("error") {
        let why = r.get("error").and_then(Value::as_str).map(str::to_string);
        return Some((Decision::Deny, why));
    }
    let inner = r.get("response").cloned().unwrap_or(Value::Null);
    match inner.get("behavior").and_then(Value::as_str) {
        Some("allow") => {
            let mut d = Decision::Allow;
            for u in inner
                .get("updatedPermissions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let kind = u.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "setMode"
                    && u.get("mode").and_then(Value::as_str) == Some("bypassPermissions")
                {
                    d = Decision::AllowAll;
                    break;
                }
                if kind == "addRules"
                    && u.get("behavior").and_then(Value::as_str) == Some("allow")
                    && u.get("rules")
                        .and_then(Value::as_array)
                        .is_some_and(|rules| {
                            rules
                                .iter()
                                .any(|r| r.get("toolName").and_then(Value::as_str) == Some(tool))
                        })
                {
                    d = Decision::AllowTool;
                }
            }
            Some((d, None))
        }
        Some("deny") => Some((
            Decision::Deny,
            inner
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
        )),
        _ => Some((Decision::Deny, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        let mut r = Run::new();
        r.model = "m".to_string();
        r
    }

    #[test]
    fn a_record_is_one_line_stamped_with_its_type_uuid_and_session() {
        let mut r = run();
        let v = record(
            &mut r,
            "system",
            json!({ "subtype": "plan", "content": "a\nb" }),
        );
        let line = v.to_string();
        assert!(
            !line.contains('\n'),
            "embedded newlines must be escaped: {line}"
        );
        assert_eq!(v["type"], "system");
        assert_eq!(v["subtype"], "plan");
        let sid = v["session_id"].as_str().unwrap().to_string();
        assert_eq!(sid.len(), 36, "uuid-shaped: {sid}");
        assert_eq!(&sid[14..15], "4");
        let again = record(&mut r, "system", json!({}));
        assert_eq!(again["session_id"], sid, "one session id per run");
        assert_ne!(again["uuid"], v["uuid"], "one uuid per record");
    }

    #[test]
    fn the_type_key_belongs_to_the_stream_not_the_fields() {
        let mut r = run();
        let v = record(&mut r, "user", json!({ "type": "impostor", "x": 1 }));
        assert_eq!(v["type"], "user");
    }

    #[test]
    fn a_non_object_payload_still_serializes() {
        let mut r = run();
        let v = record(&mut r, "odd", json!(3));
        assert_eq!(v["type"], "odd");
        assert_eq!(v["value"], 3);
    }

    /// The blocks of one model turn share a message id; a result closes the turn.
    #[test]
    fn text_then_tool_use_share_a_message_and_a_result_closes_it() {
        let mut r = run();
        let i = pending_index(&mut r, None);
        r.pending[i].text.push_str("hello");
        let first = flush_locked(&mut r, None, false);
        assert!(r.pending[i].text.is_empty(), "flushed");
        assert_eq!(r.pending[i].msg_id.as_deref(), Some(first.as_str()));
        let again = flush_locked(&mut r, None, false);
        assert_eq!(again, first, "an empty flush keeps the open message");
        let closed = flush_locked(&mut r, None, true);
        assert_eq!(closed, first);
        assert!(r.pending[i].msg_id.is_none(), "closed");
        let next = flush_locked(&mut r, None, false);
        assert_ne!(next, first, "the next turn is a new message");
    }

    /// A child's record points at the newest open `task`; the loop's own carries `null`.
    #[test]
    fn a_child_points_at_its_parent_call() {
        let mut r = run();
        assert_eq!(parent_for(&r, None), None);
        assert_eq!(parent_for(&r, Some("argus")), None, "no parent open yet");
        r.parents.push((1, "toolu_1".into()));
        assert_eq!(parent_for(&r, Some("argus")).as_deref(), Some("toolu_1"));
        assert_eq!(
            parent_for(&r, None),
            None,
            "the loop's own record has no parent"
        );
    }

    /// An approval hands out the id its `tool_use` block will carry, so the `control_request`
    /// and the block agree; an unrelated call keeps the seq-derived id.
    #[test]
    fn an_approved_call_claims_the_id_the_request_named() {
        let mut r = run();
        r.approved.push((
            "shell_run".into(),
            json!({"command": "rm x"}),
            "toolu_a1".into(),
            "req_1".into(),
        ));
        assert_eq!(
            claim_approved_id(&mut r, "shell_run", &json!({"command": "ls"})),
            None
        );
        assert_eq!(
            claim_approved_id(&mut r, "shell_run", &json!({"command": "rm x"})),
            Some(("toolu_a1".to_string(), "req_1".to_string()))
        );
        assert!(r.approved.is_empty(), "claimed once");
    }

    #[test]
    fn the_assistant_record_is_a_messages_api_message_with_one_block() {
        let mut r = run();
        let v = assistant_record(
            &mut r,
            "msg_0001",
            json!({ "type": "tool_use", "id": "toolu_3", "name": "file_read", "input": {"path": "a"} }),
            Some("toolu_1"),
            Some("argus"),
            Map::new(),
        );
        assert_eq!(v["type"], "assistant");
        assert_eq!(v["message"]["id"], "msg_0001");
        assert_eq!(v["message"]["role"], "assistant");
        assert_eq!(v["message"]["model"], "m");
        assert_eq!(v["message"]["content"][0]["name"], "file_read");
        assert_eq!(v["parent_tool_use_id"], "toolu_1");
        assert_eq!(v["dispatch"], "argus");
    }

    #[test]
    fn stop_words_map_to_result_subtypes() {
        assert_eq!(result_kind("done"), ("success", false));
        assert_eq!(result_kind("awaiting_input"), ("success", false));
        assert_eq!(result_kind("max_iters"), ("error_max_turns", true));
        assert_eq!(result_kind("divergence"), ("error_during_execution", true));
        assert_eq!(result_kind("cancelled"), ("error_during_execution", true));
    }

    /// Aizen counts cache reads and writes inside `input`; the API's `input_tokens` excludes them.
    #[test]
    fn usage_is_reshaped_to_the_messages_api_fields() {
        let u = usage_fields(
            &json!({ "calls": 3, "input": 1000, "output": 40, "cached": 600, "cache_write": 100 }),
        );
        assert_eq!(u["input_tokens"], 300);
        assert_eq!(u["output_tokens"], 40);
        assert_eq!(u["cache_read_input_tokens"], 600);
        assert_eq!(u["cache_creation_input_tokens"], 100);
        let none = usage_fields(&Value::Null);
        assert_eq!(none["input_tokens"], 0);
    }

    #[test]
    fn a_reply_is_matched_by_type_and_request_id() {
        let allow = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_3","response":{"behavior":"allow"}}}"#;
        assert_eq!(
            parse_reply(allow, "req_3", "shell_run"),
            Some((Decision::Allow, None))
        );
        assert_eq!(
            parse_reply(allow, "req_2", "shell_run"),
            None,
            "a reply to another request is not ours"
        );
        let no_id = r#"{"type":"control_response","response":{"subtype":"success","response":{"behavior":"allow"}}}"#;
        assert_eq!(
            parse_reply(no_id, "req_3", "shell_run"),
            Some((Decision::Allow, None)),
            "a reply without an id answers the pending request"
        );
        assert_eq!(
            parse_reply(r#"{"type":"user","message":{}}"#, "req_3", "x"),
            None
        );
        assert_eq!(parse_reply("not json", "req_3", "x"), None);
    }

    #[test]
    fn permission_updates_widen_the_grant() {
        let all = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{"behavior":"allow","updatedPermissions":[{"type":"setMode","mode":"bypassPermissions","destination":"session"}]}}}"#;
        assert_eq!(
            parse_reply(all, "req_1", "shell_run"),
            Some((Decision::AllowAll, None))
        );
        let tool = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{"behavior":"allow","updatedPermissions":[{"type":"addRules","rules":[{"toolName":"shell_run"}],"behavior":"allow","destination":"session"}]}}}"#;
        assert_eq!(
            parse_reply(tool, "req_1", "shell_run"),
            Some((Decision::AllowTool, None))
        );
        assert_eq!(
            parse_reply(tool, "req_1", "file_edit"),
            Some((Decision::Allow, None)),
            "a rule for another tool is a plain allow of this call"
        );
    }

    #[test]
    fn anything_but_a_known_allow_is_a_deny() {
        let deny = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{"behavior":"deny","message":"not today"}}}"#;
        assert_eq!(
            parse_reply(deny, "req_1", "x"),
            Some((Decision::Deny, Some("not today".to_string())))
        );
        let odd = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{"behavior":"maybe"}}}"#;
        assert_eq!(parse_reply(odd, "req_1", "x"), Some((Decision::Deny, None)));
        let err = r#"{"type":"control_response","response":{"subtype":"error","request_id":"req_1","error":"boom"}}"#;
        assert_eq!(
            parse_reply(err, "req_1", "x"),
            Some((Decision::Deny, Some("boom".to_string())))
        );
    }

    #[test]
    fn plan_rows_name_their_status() {
        let rows = plan_rows(&[(0u8, "a".to_string()), (1, "b".into()), (2, "c".into())]);
        assert_eq!(rows[0]["status"], "pending");
        assert_eq!(rows[1]["status"], "in_progress");
        assert_eq!(rows[2]["status"], "done");
        assert_eq!(rows[2]["text"], "c");
    }

    #[test]
    fn trace_text_is_stripped_of_styling_and_blank_lines() {
        let styled = format!(
            "{}\n\n   \n  → second  ",
            console::style("→ first").red().bold()
        );
        assert_eq!(plain(&styled), "→ first\n  → second");
    }
}
