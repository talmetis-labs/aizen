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
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::Instant;

const OFF: u8 = 0;
/// `stream-json`: every record, as it happens.
const STREAM: u8 = 1;
/// `json`: only the closing `result` object.
const FINAL: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(OFF);
static BROWSER_FRAMES: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable_browser_frames() {
    BROWSER_FRAMES.store(true, Ordering::Relaxed);
}

pub(crate) fn browser_frames_enabled() -> bool {
    streaming() && BROWSER_FRAMES.load(Ordering::Relaxed) && who_here().is_none()
}

pub(crate) fn browser_frame(profile: &str, target: &str, title: &str, jpeg: &str) {
    if browser_frames_enabled() {
        emit(
            "system",
            json!({"subtype":"browser_frame","version":1,"profile":profile,
            "target_id":target,"title":title,"jpeg":jpeg}),
        );
    }
}
/// Set by a `bypassPermissions` reply: every later approval is answered without asking.
static ALLOW_ALL: AtomicBool = AtomicBool::new(false);
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);
static MSG_SEQ: AtomicU64 = AtomicU64::new(0);
static HOOK_SEQ: AtomicU64 = AtomicU64::new(0);
/// Lines are written whole: tool bodies emit from blocking threads while the driver streams text,
/// and two half-lines interleaved would be two broken records.
static WRITE: Mutex<()> = Mutex::new(());
static RUN: Mutex<Run> = Mutex::new(Run::new());
// Console presentation is FIFO; the whole-run reader remains stdin's sole owner.
fn console_approval() -> &'static crate::ui::tui::approval::Coordinator {
    static COORDINATOR: std::sync::OnceLock<crate::ui::tui::approval::Coordinator> =
        std::sync::OnceLock::new();
    COORDINATOR.get_or_init(crate::ui::tui::approval::Coordinator::default)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdinReader {
    NotStarted,
    Live,
    Closed,
}

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

/// Single-use correlation carried by the prepared invocation across approval and worker threads.
/// It is not permission authority and cannot be claimed by matching another call's tool/input.
#[derive(Clone, Debug, Default)]
pub(crate) struct ApprovalIdentity(std::sync::Arc<Mutex<Option<(String, String)>>>);

impl ApprovalIdentity {
    fn bind(&self, tool_use_id: &str, request_id: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((tool_use_id.to_owned(), request_id.to_owned()));
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn take(&self) -> Option<(String, String)> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
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
    /// `permission_denials` rows for the result.
    denials: Vec<Value>,
    /// The answer text the loop's own `text` blocks carried so far, so a final answer that never
    /// streamed (a non-streaming provider) still goes out as a block before the result.
    spoken: String,
    /// Open approval requests' reply channel, keyed by `request_id`. The stdin reader thread
    /// (`start_stdin_reader`) parses every line; the one matching a registered id is delivered here
    /// and `ask_approval` — which is usually running on a tool thread, never on stdin's owner —
    /// waits on it. A sender that errors means the reader saw stdin close (a deny follows).
    waiting: Option<HashMap<String, Sender<String>>>,
    stdin_reader: StdinReader,
    /// Explicit host opt-in; ordinary stream-json consumers retain the original wire surface.
    host_preview_diagnostics: bool,
    preview_owner_reported: bool,
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
            denials: Vec::new(),
            spoken: String::new(),
            waiting: None,
            stdin_reader: StdinReader::NotStarted,
            host_preview_diagnostics: false,
            preview_owner_reported: false,
            seed: 0,
        }
    }
}

impl Run {
    /// The `request_id` → reply-sender table, created on first use (the const constructor cannot).
    fn waiting(&mut self) -> &mut HashMap<String, Sender<String>> {
        self.waiting.get_or_insert_with(HashMap::new)
    }

    fn register_waiter(&mut self, request_id: &str) -> Option<std::sync::mpsc::Receiver<String>> {
        if self.stdin_reader != StdinReader::Live
            || request_id.is_empty()
            || self.waiting().contains_key(request_id)
        {
            return None;
        }
        let (tx, rx) = channel();
        self.waiting().insert(request_id.to_string(), tx);
        Some(rx)
    }

    fn deliver_reply(&mut self, request_id: &str, line: String) {
        if self.stdin_reader == StdinReader::Live && !request_id.is_empty() {
            // Take once: duplicates and late replies never accumulate or answer another request.
            if let Some(tx) = self.waiting().remove(request_id) {
                let _ = tx.send(line);
            }
        }
    }

    fn close_stdin(&mut self) {
        self.stdin_reader = StdinReader::Closed;
        self.waiting().clear(); // Drop every sender so outstanding approvals safely deny.
    }
}

/// Switch the process to machine-readable output: the full stream (`stream-json`) or only the
/// closing `result` object (`json`). Irreversible for the process: a one-shot run is one turn.
pub fn enable(streaming: bool) {
    MODE.store(if streaming { STREAM } else { FINAL }, Ordering::Relaxed);
}

/// Enable diagnostics only for a host that explicitly negotiated the CLI flag. This is NOT a
/// retention grant: process lifetimes still own their workspace/global exclusion and containment.
pub(crate) fn enable_host_preview_diagnostics() {
    if streaming() {
        lock().host_preview_diagnostics = true;
    }
}

const PREVIEW_RETENTION_BLOCKER: &str = "process_lifetime_owns_exclusion";
const PREVIEW_PROCESS_CAP: usize = 16;

fn add_preview_capabilities(run: &Run, fields: &mut Value) {
    if run.host_preview_diagnostics {
        fields["capabilities"] = json!({"host_preview": {
            "version": 1,
            "retention": false,
            "reason": PREVIEW_RETENTION_BLOCKER,
            "readiness": "host_http_probe",
            "completion": "result",
        }});
    }
    if BROWSER_FRAMES.load(Ordering::Relaxed) {
        fields["capabilities"]["browser_frame"] = json!(1);
    }
}

fn preview_owner_fields(run: &mut Run, jobs: &[crate::agent::process::JobInfo]) -> Option<Value> {
    if !run.host_preview_diagnostics || run.preview_owner_reported {
        return None;
    }
    run.preview_owner_reported = true;
    if jobs.is_empty() {
        return None;
    }
    // Opaque run-local handles only. No commands, arbitrary PIDs, logs, or guessed URLs. A wrapper
    // that exited may still have live descendants: `running` is NOT HTTP health or tree settlement.
    let processes: Vec<Value> = jobs
        .iter()
        .take(PREVIEW_PROCESS_CAP)
        .map(|job| json!({"id": job.id, "running": job.running}))
        .collect();
    Some(json!({
        "subtype": "preview_owner_exiting",
        "version": 1,
        "retained": false,
        "reason": PREVIEW_RETENTION_BLOCKER,
        "readiness": "unverified",
        "processes": processes,
    }))
}

/// An opt-in advisory immediately before the one-shot result/error, never a second completion or
/// proof that an HTTP server has stopped. The host correlates handles with its own preview evidence.
pub(crate) fn preview_owner_exiting() {
    if !streaming() || !lock().host_preview_diagnostics {
        return;
    }
    // Do not acquire process ownership while holding the stream mutex.
    let jobs = crate::agent::process::snapshot();
    let mut run = lock();
    if let Some(fields) = preview_owner_fields(&mut run, &jobs) {
        emit_locked(&mut run, "system", fields, false);
    }
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

// ── stdin control loop ───────────────────────────────────────────────────────

/// Whether entry received a pipe rather than a terminal. Reader ownership/liveness lives in `Run`,
/// not this configuration bit; a closed pipe must never trigger a competing fallback reader.
static STDIN_IS_PIPED: AtomicBool = AtomicBool::new(false);

/// Called once from `run_agent_cmd`: pipes allow concurrent request presentation, while a console
/// presents one approval at a time. Both use the same sole stdin reader and request-id routing.
/// The human REPL does not enable this stream and retains its own input owner.
pub fn note_stdin_piped(piped: bool) {
    STDIN_IS_PIPED.store(piped, Ordering::SeqCst);
}

fn stdin_is_piped() -> bool {
    STDIN_IS_PIPED.load(Ordering::SeqCst)
}

/// What one stdin line is, for the reader thread.
enum Inbound {
    /// A `control_response` naming this `request_id` — handed to the `ask_approval` waiting on it.
    Reply { request_id: String, line: String },
    /// A `{"type":"steer","text":…}` line: a mid-run course correction from the driving process.
    Steer(String),
    /// Anything else (another record type, not JSON, a reply to no pending request): ignored, as
    /// the old per-approval `read_line` loop ignored it.
    Ignored,
}

/// Classify one stdin line without holding any lock.
fn classify_inbound(line: &str) -> Inbound {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Inbound::Ignored;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("control_response") => {
            let request_id = v
                .get("response")
                .and_then(|r| r.get("request_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Inbound::Reply {
                request_id,
                line: line.trim().to_string(),
            }
        }
        Some("steer") => {
            let text = v.get("text").and_then(Value::as_str).unwrap_or("");
            if text.trim().is_empty() {
                Inbound::Ignored
            } else {
                Inbound::Steer(text.to_string())
            }
        }
        _ => Inbound::Ignored,
    }
}

/// The single owner of `stdin` for the whole run. The old design had `ask_approval` read stdin
/// itself, which drops every line that is not that request's reply — and blocks any second input
/// channel from existing at all. With steering the line reader must be one thread that dispatches:
/// a reply goes to the waiting approval, a steer goes into the mailbox, and anything else is
/// skipped. EOF exits the thread; the waiters then see a closed channel and deny, the same safe
/// default the old loop had on EOF.
pub fn start_stdin_reader() {
    if !streaming() {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Publish ownership before spawn. Requests can queue immediately, even before the new
        // thread runs; a spawn failure closes those channels instead of competing for stdin.
        lock().stdin_reader = StdinReader::Live;
        let started = std::thread::Builder::new()
            .name("aizen-stdin".to_string())
            .spawn(|| {
                let stdin = std::io::stdin();
                let mut handle = stdin.lock();
                let mut line = String::new();
                loop {
                    line.clear();
                    match handle.read_line(&mut line) {
                        // EOF or a read error: no more control traffic. Waiting approvals deny via
                        // the closed channel; nothing else to do.
                        Ok(0) | Err(_) => {
                            lock().close_stdin();
                            return;
                        }
                        Ok(_) => {}
                    }
                    match classify_inbound(&line) {
                        Inbound::Reply { request_id, line } => {
                            lock().deliver_reply(&request_id, line);
                            // A reply to no pending request (stale id, late answer) is ignored.
                        }
                        Inbound::Steer(text) => {
                            let accepted = crate::core::steer::push(&text);
                            let mut run = lock();
                            if accepted {
                                emit_locked(
                                    &mut run,
                                    "system",
                                    json!({
                                        "subtype": "steer_queued",
                                        "text": text,
                                        "pending": crate::core::steer::pending(),
                                    }),
                                    false,
                                );
                            } else {
                                // Refused: no armed turn (arrived between runs or before the loop
                                // started), blank/oversized text, or a full backlog. The driver
                                // must know — it should fall back to queueing for the next run.
                                emit_locked(
                                    &mut run,
                                    "system",
                                    json!({
                                        "subtype": "steer_dropped",
                                        "text": text,
                                        "reason": if crate::core::steer::is_armed() {
                                            "backlog full or oversized"
                                        } else {
                                            "no live turn accepting steers"
                                        },
                                    }),
                                    false,
                                );
                            }
                        }
                        Inbound::Ignored => {}
                    }
                }
            });
        if started.is_err() {
            lock().close_stdin();
        }
    });
}

/// Approval waits are bounded by the calling turn's cancellation and inherited deadline.
fn approval_interrupted() -> bool {
    crate::core::cancel::current().is_some_and(|c| c.is_cancelled())
        || crate::core::exec_ctx::current()
            .and_then(|c| c.deadline())
            .is_some_and(|d| Instant::now() >= d)
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

/// Private transport to the host that owns this child process. Only the compact text reply
/// becomes a tool result. A timeout after dispatch is ambiguous and must never replay input.
pub(crate) fn computer_call(action: &str, args: &Value) -> anyhow::Result<String> {
    anyhow::ensure!(
        streaming() && who_here().is_none(),
        "no foreground computer grant"
    );
    start_stdin_reader();
    let (id, rx) = {
        let mut run = lock();
        let id = format!("computer-{}", uuid(&mut run));
        let rx = run
            .register_waiter(&id)
            .ok_or_else(|| anyhow::anyhow!("computer host unavailable"))?;
        emit_locked(
            &mut run,
            "system",
            json!({"subtype":"computer_request","version":1,
            "request_id":id,"action":action,"args":args}),
            false,
        );
        (id, rx)
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(12);
    let result = loop {
        if approval_interrupted() || Instant::now() >= deadline {
            break Err(anyhow::anyhow!(
                "computer request interrupted; observe again before any further action"
            ));
        }
        match rx.recv_timeout(std::time::Duration::from_millis(80)) {
            Ok(line) => break Ok(line),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("computer host disconnected"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    lock().waiting().remove(&id);
    let reply: Value = serde_json::from_str(&result?)?;
    let response = &reply["response"];
    anyhow::ensure!(
        response["request_id"] == id && response["subtype"] == "computer",
        "invalid host reply"
    );
    let text = response["text"]
        .as_str()
        .filter(|s| s.len() <= 32000)
        .ok_or_else(|| anyhow::anyhow!("invalid computer observation"))?;
    anyhow::ensure!(response["ok"] == true, "{text}");
    Ok(text.to_owned())
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
    let mut fields = json!({
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
    });
    add_preview_capabilities(&run, &mut fields);
    emit_locked(&mut run, "system", fields, false);
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

/// A line the human transcript would have shown as progress (a verification-status update, a retry
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

/// A tool call is starting: an `assistant` record with one `tool_use` block. `seq` ties the
/// later result to this call; `dispatch` names the delegated child whose call it is, `None`
/// for the loop's own. A child's calls share the stream with its parent's and its siblings',
/// interleaved — pair a result with its call by `tool_use_id`, never by order.
pub(crate) fn tool_call(
    seq: u64,
    name: &str,
    args: &Value,
    target: &str,
    dispatch: Option<&str>,
    identity: Option<&ApprovalIdentity>,
) {
    if !streaming() {
        return;
    }
    let mut run = lock();
    let msg_id = flush_locked(&mut run, dispatch, false);
    let approved = identity.and_then(ApprovalIdentity::take);
    let id = approved
        .as_ref()
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| format!("toolu_{seq}"));
    run.ids.push((seq, id.clone()));
    let parent = parent_for(&run, dispatch);
    let mut extra = Map::new();
    extra.insert("target".to_string(), json!(target));
    if let Some((_, request_id)) = approved {
        // Resolve only this invocation's question; refused calls also emit a result pair.
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

/// The runtime's effective checklist — `system` / `plan`. Status stays compatible with the
/// panel; stable IDs and completion basis are additive, never inferred from a model Done claim.
#[allow(dead_code)] // Compatibility entry point; owned task publications use plan_snapshot.
pub fn plan(items: &[(u8, String)]) {
    let state = crate::agent::todo::try_current_context().map(|context| context.snapshot());
    plan_snapshot(items, state.as_ref());
}

pub(crate) fn plan_snapshot(items: &[(u8, String)], state: Option<&crate::agent::todo::TaskState>) {
    emit("system", plan_projection(items, state));
}

pub(crate) fn plan_snapshot_aligned(
    items: &[(u8, String)],
    state: &crate::agent::todo::TaskState,
) -> bool {
    state.items.len() == items.len()
        && state.items.iter().zip(items).all(|(row, (status, text))| {
            let effective = match row.status {
                crate::agent::todo::Status::Pending => 0,
                crate::agent::todo::Status::InProgress => 1,
                crate::agent::todo::Status::Done => 2,
            };
            effective == *status && row.content == *text
        })
}

fn plan_projection(items: &[(u8, String)], state: Option<&crate::agent::todo::TaskState>) -> Value {
    let mut rows = plan_rows(items);
    let mut event = json!({ "subtype": "plan" });
    if let Some(state) = state.filter(|state| plan_snapshot_aligned(items, state)) {
        for (projection, row) in rows.iter_mut().zip(&state.items) {
            projection["id"] = json!(row.id);
            projection["completion_basis"] =
                json!(completion_basis_label(state.completion_basis(&row.id)));
            if let Some(link) = state.item_phases.iter().find(|link| link.item_id == row.id) {
                projection["phase"] = json!({
                    "plan_id": link.plan_id, "phase_id": link.phase_id,
                    "resolved": state.phase_resolved(&row.id),
                });
            }
        }
        event["task_id"] = json!(state.id);
        event["task_revision"] = json!(state.revision);
        event["evidence_epoch"] = json!(state.effect_epoch);
        event["document"] = state.plan.as_ref().map_or(Value::Null, |plan| json!({
            "path": plan.path, "id": plan.id, "fingerprint": plan.fingerprint,
            "freshness": plan.freshness, "usable": plan.usable(), "phases": plan.phases,
            "validation": if plan.usable() { "validated at last refresh" } else { "not currently usable" },
        }));
    }
    event["items"] = json!(rows);
    event
}

pub(crate) fn plan_document_summary(state: &crate::agent::todo::TaskState) -> Option<String> {
    let plan = state.plan.as_ref()?;
    // JSON quoting keeps a hostile path from injecting terminal control characters/newlines.
    let path = json!(plan.path);
    let status = if plan.usable() {
        "validated at last refresh".to_owned()
    } else {
        format!(
            "not usable ({}; saved metadata may be historical)",
            json!(plan.freshness)
        )
    };
    Some(format!(
        "plan document {} {path} — {status}; not implementation-check evidence",
        plan.id
    ))
}

#[cfg(test)]
#[path = "plan_event_tests.rs"]
mod plan_event_tests;

fn completion_basis_label(basis: crate::agent::todo::CompletionBasis) -> &'static str {
    use crate::agent::todo::CompletionBasis;
    match basis {
        CompletionBasis::Observed => "observed",
        CompletionBasis::Reported => "reported",
        CompletionBasis::Verified => "verified",
        CompletionBasis::NotRequired => "not_required",
        CompletionBasis::Unverified => "unverified",
    }
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
#[allow(dead_code)] // Keep the original two-argument event API for existing callers.
pub fn compact_boundary(pre_tokens: usize, post_tokens: usize) {
    emit(
        "system",
        json!({
            "subtype": "compact_boundary",
            "compact_metadata": { "trigger": "auto", "pre_tokens": pre_tokens, "post_tokens": post_tokens },
        }),
    );
}

/// One automatic compaction, scoped to the speaker at start (not whichever child runs last).
/// Dropping an unfinished cycle reports failure, including early `?` returns. Cooperative
/// cancellation is explicit; a hard process kill is detected by the stream consumer at exit.
pub(crate) struct CompactCycle {
    id: String,
    who: Option<String>,
    parent: Option<String>,
    terminal: bool,
}

impl CompactCycle {
    pub(crate) fn start() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let who = who_here();
        let parent = parent_for(&lock(), who.as_deref());
        let cycle = Self {
            id: format!("compact_{}", SEQ.fetch_add(1, Ordering::Relaxed)),
            who,
            parent,
            terminal: false,
        };
        cycle.status("started");
        cycle
    }

    fn fields(&self, subtype: &str, mut metadata: Value) -> Value {
        metadata["cycle_id"] = json!(self.id);
        metadata["trigger"] = json!("auto");
        json!({
            "subtype": subtype,
            "compact_metadata": metadata,
            "parent_tool_use_id": self.parent,
            "dispatch": self.who,
        })
    }

    fn status(&self, phase: &str) {
        let mut metadata = json!({ "phase": phase });
        if phase == "failed" {
            // Never serialize an upstream error: providers may echo credentials or private input.
            metadata["error"] = json!("Automatic compaction failed.");
        }
        emit_compaction(self.fields("compact_status", metadata));
    }

    pub(crate) fn success(
        mut self,
        pre_tokens: usize,
        post_tokens: usize,
        request_tokens: usize,
        context_window: usize,
    ) {
        self.terminal = true;
        emit_compaction(self.fields(
            "compact_boundary",
            json!({
                "pre_tokens": pre_tokens, "post_tokens": post_tokens,
                "request_tokens": request_tokens, "context_window": context_window,
            }),
        ));
    }

    pub(crate) fn failed(mut self) {
        self.terminal = true;
        self.status("failed");
    }

    pub(crate) fn cancelled(mut self) {
        self.terminal = true;
        self.status("cancelled");
    }
}

impl Drop for CompactCycle {
    fn drop(&mut self) {
        if !self.terminal {
            self.status("failed");
        }
    }
}

fn emit_compaction(fields: Value) {
    #[cfg(test)]
    if COMPACT_TEST_EVENTS
        .try_with(|events| {
            events
                .borrow_mut()
                .push(record(&mut lock(), "system", fields.clone()))
        })
        .is_ok()
    {
        return;
    }
    emit("system", fields);
}

#[cfg(test)]
tokio::task_local! {
    static COMPACT_TEST_EVENTS: std::cell::RefCell<Vec<Value>>;
}

/// Task-local capture avoids changing the process-wide JSON mode or racing unrelated loop tests.
#[cfg(test)]
pub(crate) async fn capture_compaction<F: std::future::Future>(
    future: F,
) -> (F::Output, Vec<Value>) {
    COMPACT_TEST_EVENTS
        .scope(std::cell::RefCell::new(Vec::new()), async {
            let result = future.await;
            (result, COMPACT_TEST_EVENTS.with(|events| events.take()))
        })
        .await
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

/// A steer the turn never drained before it ended — `system` / `steer_dropped` with
/// `reason: "turn ended"`. The desktop resume-chains the text into the follow-up run.
pub fn steer_leftover(text: &str) {
    emit(
        "system",
        json!({ "subtype": "steer_dropped", "text": text, "reason": "turn ended" }),
    );
}

/// The SDK `result` subtype and `is_error` for one of aizen's stop words (`done`, `incomplete`,
/// `divergence`, `max_iters`, `verification_failed`, `awaiting_input`, `cancelled`, `deadline`).
pub(crate) fn result_kind(stop: &str) -> (&'static str, bool) {
    match stop {
        "done" | "awaiting_input" => ("success", false),
        "max_iters" => ("error_max_turns", true),
        _ => ("error_during_execution", true),
    }
}

pub(crate) const INCOMPLETE_VERDICT: &str =
    "[incomplete: runtime completion requirements are not satisfied; the work above is not confirmed complete.]";

/// Keep captured, hostbot and JSON replies honest even after optimistic model-authored prose.
/// This verdict is runtime-owned, never inferred from or suppressed by the model's answer.
pub(crate) fn completion_result_text<'a>(stop: &str, answer: &'a str) -> std::borrow::Cow<'a, str> {
    if stop == "incomplete" {
        std::borrow::Cow::Owned(format!("{answer}\n\n{INCOMPLETE_VERDICT}"))
    } else {
        std::borrow::Cow::Borrowed(answer)
    }
}

fn completion_fields(stop: &str) -> Value {
    json!({ "status": stop, "completed": stop == "done" })
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
/// stop reason; `question` is the ask's text rendering, set with `awaiting_input`; `ask` is that
/// same ask STRUCTURED (questions/options/multi-select), so a front-end with a picker can render
/// it instead of parsing text; `session` is the saved session's slug; `usage` sums the provider's
/// reported tokens over the run.
/// Includes the loop's typed verification snapshot and exact last streamed response.
#[allow(clippy::too_many_arguments)]
pub fn done_with_verification(
    stop: &str,
    steps: usize,
    final_text: Option<&str>,
    question: Option<&str>,
    ask: Option<Value>,
    session: Option<&str>,
    usage: Value,
    verification: Option<Value>,
    last_streamed: Option<&str>,
    speech_final_verified: bool,
) {
    if !on() {
        return;
    }
    let mut run = lock();
    flush_all(&mut run);
    // Append only the loop-added suffix when the provider's final response already streamed.
    if let Some(answer) = final_text.filter(|t| !t.trim().is_empty()) {
        if !run.spoken.ends_with(answer) {
            let text = unstreamed_result_text(answer, last_streamed, &run.spoken);
            if !text.trim().is_empty() {
                let i = pending_index(&mut run, None);
                run.pending[i].text.push_str(text);
                flush_locked(&mut run, None, true);
            }
        }
    }
    let (subtype, is_error) = result_kind(stop);
    let result_text = completion_result_text(stop, final_text.unwrap_or(""));
    let mut fields = json!({
        "subtype": subtype,
        "is_error": is_error,
        "duration_ms": elapsed_ms(&run),
        "num_turns": steps,
        "result": result_text,
        "completion": completion_fields(stop),
        "stop_reason": Value::Null,
        "usage": usage_fields(&usage),
        "permission_denials": run.denials,
        "stop": stop,
        "question": question,
        "ask": ask,
        "session": session,
        "calls": usage.get("calls").cloned().unwrap_or(json!(0)),
    });
    if let Some(media) = usage.get("media") {
        fields["media"] = media.clone();
    }
    if let Some(verification) = verification {
        fields["verification"] = verification;
    }
    if is_error {
        fields["errors"] = json!([stop]);
    }
    if let Some(proof) = speech_final_proof(
        streaming(),
        stop,
        session,
        final_text,
        speech_final_verified,
    ) {
        fields["speech_final"] = proof;
    }
    emit_locked(&mut run, "result", fields, true);
}

fn speech_final_proof(
    is_stream_json: bool,
    stop: &str,
    session: Option<&str>,
    text: Option<&str>,
    verified: bool,
) -> Option<Value> {
    if !is_stream_json || stop != "done" || !verified || session.is_none_or(str::is_empty) {
        return None;
    }
    text.filter(|s| !s.trim().is_empty() && s.len() <= 24 * 1024)
        .map(|s| json!({ "version": 1, "text": s }))
}

#[cfg(test)]
#[test]
fn speech_final_requires_exact_saved_root_done_provenance() {
    let value = speech_final_proof(true, "done", Some("fixture"), Some(" Final answer. "), true);
    assert_eq!(
        value,
        Some(json!({ "version": 1, "text": " Final answer. " }))
    );
    assert_eq!(
        speech_final_proof(false, "done", Some("fixture"), Some("answer"), true),
        None
    );
    assert_eq!(
        speech_final_proof(
            true,
            "awaiting_input",
            Some("fixture"),
            Some("answer"),
            true
        ),
        None
    );
    assert_eq!(
        speech_final_proof(true, "incomplete", Some("fixture"), Some("answer"), true),
        None
    );
    assert_eq!(
        speech_final_proof(true, "done", None, Some("answer"), true),
        None
    );
    assert_eq!(
        speech_final_proof(true, "done", Some("fixture"), Some("reasoning-only"), false),
        None
    );
}

fn unstreamed_result_text<'a>(
    answer: &'a str,
    last_response: Option<&str>,
    spoken: &str,
) -> &'a str {
    last_response
        .filter(|text| !text.is_empty() && spoken.ends_with(text))
        .and_then(|text| answer.strip_prefix(text))
        .unwrap_or(answer)
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
        "completion": completion_fields("error"),
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

/// Whether generic session grants may authorize this approval or be widened by its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalScope {
    Session,
    Invocation,
}

impl ApprovalScope {
    fn bypasses(self, prior_allow_all: bool) -> bool {
        self == Self::Session && prior_allow_all
    }

    /// Invocation-bound consent cannot create a reusable tool or session grant.
    fn decision(self, decision: Decision) -> Decision {
        match (self, decision) {
            (Self::Invocation, Decision::AllowTool | Decision::AllowAll) => Decision::Allow,
            _ => decision,
        }
    }
}

/// Whether an earlier `bypassPermissions` reply stands for generic approvals.
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
pub(crate) fn ask_approval(
    tool: &str,
    args: &Value,
    who: Option<&str>,
    preview: Option<Value>,
    identity: &ApprovalIdentity,
) -> bool {
    ask_approval_scoped(tool, args, who, preview, ApprovalScope::Session, identity)
}

/// Ask for explicit consent to this invocation, even after a generic allow-all grant.
/// Replies containing `addRules` or `bypassPermissions` authorize only this call and never
/// record reusable grants. Intended for destination-bound media transmission.
/// Without a stream-json approval channel this consent is refused, not inherited from `--yes`.
pub(crate) fn ask_approval_once(
    tool: &str,
    args: &Value,
    who: Option<&str>,
    preview: Option<Value>,
    identity: &ApprovalIdentity,
) -> bool {
    ask_approval_scoped(
        tool,
        args,
        who,
        preview,
        ApprovalScope::Invocation,
        identity,
    )
}

fn ask_approval_scoped(
    tool: &str,
    args: &Value,
    who: Option<&str>,
    preview: Option<Value>,
    scope: ApprovalScope,
    identity: &ApprovalIdentity,
) -> bool {
    if !approval_interrupted() && scope.bypasses(allow_all()) {
        return true;
    }
    let n = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let request_id = format!("req_{n}");
    let tool_use_id = format!("toolu_a{n}");
    // Correlation belongs to this invocation, including its refused/unrun result. This token
    // confers no permission; only the returned decision authorizes launching the body.
    identity.bind(&tool_use_id, &request_id);
    if !streaming() {
        let mut run = lock();
        record_denial(
            &mut run,
            tool,
            &tool_use_id,
            &request_id,
            args,
            if scope == ApprovalScope::Invocation {
                "explicit invocation approval requires --output-format stream-json"
            } else {
                "no approval channel under --output-format json (pass --yes to pre-approve)"
            },
        );
        return false;
    }
    if approval_interrupted() {
        let mut run = lock();
        record_denial(
            &mut run,
            tool,
            &tool_use_id,
            &request_id,
            args,
            "approval cancelled",
        );
        return false;
    }
    start_stdin_reader();
    // Serialize only console presentation. Cancellation while queued never waits for another
    // request's answer; stdin itself always belongs to the same long-lived reader.
    let _console = if !stdin_is_piped() {
        let Some(owner) = console_approval().acquire(approval_interrupted) else {
            let mut run = lock();
            record_denial(
                &mut run,
                tool,
                &tool_use_id,
                &request_id,
                args,
                "approval cancelled",
            );
            return false;
        };
        Some(owner)
    } else {
        None
    };
    if approval_interrupted() {
        let mut run = lock();
        record_denial(
            &mut run,
            tool,
            &tool_use_id,
            &request_id,
            args,
            "approval cancelled",
        );
        return false;
    }
    if scope.bypasses(allow_all()) {
        return true;
    }
    let reply_rx = {
        let mut run = lock();
        let Some(reply_rx) = run.register_waiter(&request_id) else {
            record_denial(
                &mut run,
                tool,
                &tool_use_id,
                &request_id,
                args,
                "stdin closed or reader unavailable",
            );
            return false;
        };
        flush_all(&mut run);
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
        reply_rx
    };
    // Registration and publication hold the same state lock: an immediate driver reply cannot
    // arrive in the old gap between printing the request and creating its waiter.
    struct WaiterCleanup<'a>(&'a str);
    impl Drop for WaiterCleanup<'_> {
        fn drop(&mut self) {
            lock().waiting().remove(self.0);
        }
    }
    let _cleanup = WaiterCleanup(&request_id);
    let reply = wait_for_approval_reply(&reply_rx, approval_interrupted);
    let line = match reply {
        Ok(line) => line,
        Err(why) => {
            let mut run = lock();
            record_denial(&mut run, tool, &tool_use_id, &request_id, args, why);
            return false;
        }
    };
    if let Some((d, message)) = parse_reply(&line, &request_id, tool) {
        return finish_decision(d, message, tool, &tool_use_id, &request_id, args, scope);
    }
    // A line claiming our id without a recognized decision must never authorize this call.
    let mut run = lock();
    record_denial(
        &mut run,
        tool,
        &tool_use_id,
        &request_id,
        args,
        "unrecognised control_response",
    );
    false
}

fn wait_for_approval_reply(
    rx: &std::sync::mpsc::Receiver<String>,
    interrupted: impl Fn() -> bool,
) -> Result<String, &'static str> {
    loop {
        if interrupted() {
            return Err("approval cancelled");
        }
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(line) if !interrupted() => return Ok(line),
            Ok(_) => return Err("approval cancelled"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err("stdin closed"),
        }
    }
}

/// Apply a parsed approval decision within its scope, record a refusal, and return
/// whether the call may run. Only session-scoped approvals can widen generic grants.
fn finish_decision(
    d: Decision,
    message: Option<String>,
    tool: &str,
    tool_use_id: &str,
    request_id: &str,
    args: &Value,
    scope: ApprovalScope,
) -> bool {
    if approval_interrupted() {
        let mut run = lock();
        record_denial(
            &mut run,
            tool,
            tool_use_id,
            request_id,
            args,
            "approval cancelled",
        );
        return false;
    }
    let d = scope.decision(d);
    match d {
        Decision::AllowTool => crate::core::approval::grant_session(tool, None),
        Decision::AllowAll => ALLOW_ALL.store(true, Ordering::Relaxed),
        Decision::Allow | Decision::Deny => {}
    }
    if d == Decision::Deny {
        let mut run = lock();
        let why = message.unwrap_or_else(|| "the user declined this action".to_string());
        record_denial(&mut run, tool, tool_use_id, request_id, args, &why);
        return false;
    }
    true
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
    if r.get("request_id").and_then(Value::as_str) != Some(request_id) || request_id.is_empty() {
        return None;
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
#[path = "events/approval_tests.rs"]
pub(crate) mod approval_integration_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        let mut r = Run::new();
        r.model = "m".to_string();
        r
    }

    #[test]
    fn host_preview_diagnostic_is_opt_in_and_opaque() {
        let mut ordinary = run();
        let jobs = vec![crate::agent::process::JobInfo {
            id: "proc_7".into(),
            running: true,
            elapsed: "3s".into(),
            command: "secret command must not cross protocol".into(),
        }];
        assert!(preview_owner_fields(&mut ordinary, &jobs).is_none());

        let mut host = run();
        host.host_preview_diagnostics = true;
        let mut init = json!({"subtype": "init"});
        add_preview_capabilities(&host, &mut init);
        assert_eq!(init["capabilities"]["host_preview"]["version"], 1);
        assert_eq!(init["capabilities"]["host_preview"]["retention"], false);
        assert_eq!(
            init["capabilities"]["host_preview"]["reason"],
            PREVIEW_RETENTION_BLOCKER
        );
        let fields = preview_owner_fields(&mut host, &jobs).expect("one bounded owner advisory");
        assert_eq!(fields["subtype"], "preview_owner_exiting");
        assert_eq!(fields["version"], 1);
        assert_eq!(fields["retained"], false);
        assert_eq!(fields["readiness"], "unverified");
        assert_eq!(fields["processes"][0]["id"], "proc_7");
        assert_eq!(fields["processes"][0]["running"], true);
        assert!(fields["processes"][0].get("command").is_none());
        assert!(preview_owner_fields(&mut host, &jobs).is_none());
    }

    #[test]
    fn host_preview_diagnostic_empty_pool_is_not_repeated() {
        let mut host = run();
        host.host_preview_diagnostics = true;
        let mut ordinary_init = json!({"subtype": "init"});
        add_preview_capabilities(&run(), &mut ordinary_init);
        assert!(ordinary_init.get("capabilities").is_none());
        assert!(preview_owner_fields(&mut host, &[]).is_none());
        assert!(preview_owner_fields(
            &mut host,
            &[crate::agent::process::JobInfo {
                id: "proc_late".into(),
                running: true,
                elapsed: "1s".into(),
                command: "not exposed".into(),
            },]
        )
        .is_none());
    }

    #[test]
    fn host_preview_diagnostic_caps_owner_handles_at_sixteen() {
        let mut host = run();
        host.host_preview_diagnostics = true;
        let jobs: Vec<_> = (0..20)
            .map(|index| crate::agent::process::JobInfo {
                id: format!("proc_{index}"),
                running: index % 2 == 0,
                elapsed: "1s".into(),
                command: "not exposed".into(),
            })
            .collect();
        let fields = preview_owner_fields(&mut host, &jobs).expect("bounded owner advisory");
        let processes = fields["processes"].as_array().expect("process array");
        assert_eq!(processes.len(), PREVIEW_PROCESS_CAP);
        assert_eq!(processes[0]["id"], "proc_0");
        assert_eq!(processes[15]["id"], "proc_15");
        assert!(processes
            .iter()
            .all(|process| process.get("command").is_none()));
    }

    #[tokio::test]
    async fn compact_cycles_have_unique_ids_and_exactly_one_terminal_event() {
        let (_, events) = capture_compaction(async {
            CompactCycle::start().success(100, 40, 72, 4096);
            CompactCycle::start().failed();
            CompactCycle::start().cancelled();
            drop(CompactCycle::start()); // Early error/abandoned future cannot leave a live spinner.
        })
        .await;
        assert_eq!(events.len(), 8);
        let mut ids = std::collections::HashSet::new();
        for pair in events.chunks_exact(2) {
            let start = &pair[0];
            let end = &pair[1];
            assert_eq!(start["type"], "system");
            assert_eq!(start["subtype"], "compact_status");
            assert_eq!(start["compact_metadata"]["phase"], "started");
            let id = start["compact_metadata"]["cycle_id"].as_str().unwrap();
            assert!(ids.insert(id));
            assert_eq!(end["compact_metadata"]["cycle_id"], id);
            assert_eq!(start["session_id"], end["session_id"]);
            assert_eq!(start["compact_metadata"]["trigger"], "auto");
            assert_eq!(end["compact_metadata"]["trigger"], "auto");
            assert_ne!(start["uuid"], end["uuid"]);
            assert!(start["parent_tool_use_id"].is_null());
        }
        assert_eq!(events[1]["subtype"], "compact_boundary");
        assert_eq!(events[1]["compact_metadata"]["pre_tokens"], 100);
        assert_eq!(events[1]["compact_metadata"]["post_tokens"], 40);
        assert_eq!(events[1]["compact_metadata"]["request_tokens"], 72);
        assert_eq!(events[1]["compact_metadata"]["context_window"], 4096);
        assert_eq!(events[3]["compact_metadata"]["phase"], "failed");
        assert_eq!(
            events[3]["compact_metadata"]["error"],
            "Automatic compaction failed."
        );
        assert_eq!(events[5]["compact_metadata"]["phase"], "cancelled");
        assert_eq!(events[7]["compact_metadata"]["phase"], "failed");
    }

    #[test]
    fn compact_cycle_preserves_parent_identity_when_other_children_start() {
        let mut run = run();
        run.parents.push((1, "toolu_owner".into()));
        let cycle = CompactCycle {
            id: "fixture-cycle".into(),
            who: Some("child".into()),
            parent: parent_for(&run, Some("child")),
            terminal: true,
        };
        run.parents.push((2, "toolu_unrelated".into()));
        let fields = cycle.fields("compact_status", json!({ "phase": "failed" }));
        let event = record(&mut run, "system", fields);
        assert_eq!(event["parent_tool_use_id"], "toolu_owner");
        assert_eq!(event["dispatch"], "child");
        assert_eq!(event["compact_metadata"]["cycle_id"], "fixture-cycle");
        assert_eq!(event["compact_metadata"]["phase"], "failed");
    }

    #[test]
    fn immediate_approval_replies_are_registered_before_publication() {
        let mut r = run();
        r.stdin_reader = StdinReader::Live;
        let rx = r.register_waiter("req_fast").unwrap();
        // The production path retains this Run lock through publication; delivery starts with
        // the waiter already present, even when the driver answers its first readable byte.
        let request = record(&mut r, "control_request", json!({"request_id": "req_fast"}));
        r.deliver_reply(request["request_id"].as_str().unwrap(), "allow".into());
        assert_eq!(rx.try_recv().unwrap(), "allow");
        assert!(r.waiting().is_empty());
    }

    #[test]
    fn piped_approvals_match_ids_once_and_eof_closes_every_waiter() {
        let mut r = run();
        r.stdin_reader = StdinReader::Live;
        let a = r.register_waiter("req_a").unwrap();
        let b = r.register_waiter("req_b").unwrap();
        assert!(
            r.register_waiter("req_a").is_none(),
            "never replace a live waiter"
        );
        assert!(r.register_waiter("").is_none());
        for id in ["", "stale", "req_unknown"] {
            r.deliver_reply(id, "wrong".into());
        }
        assert!(matches!(
            a.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        r.deliver_reply("req_b", "b".into());
        r.deliver_reply("req_b", "duplicate".into());
        assert_eq!(b.try_recv().unwrap(), "b");
        r.close_stdin();
        assert!(matches!(
            a.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(r.waiting().is_empty());
        assert!(r.register_waiter("after_eof").is_none());
        r.deliver_reply("req_a", "late".into());
        assert_eq!(r.stdin_reader, StdinReader::Closed);
    }

    #[test]
    fn unavailable_reader_never_registers_an_approval() {
        let mut r = run();
        assert!(r.register_waiter("not_started").is_none());
        r.stdin_reader = StdinReader::Live;
        let rx = r.register_waiter("spawn_failed").unwrap();
        r.close_stdin(); // Spawn failure and EOF use the same terminal transition.
        assert!(rx.recv().is_err());
        assert!(r.register_waiter("new_request").is_none());
    }

    #[test]
    fn interrupted_approval_waits_never_consume_a_late_grant() {
        let (tx, rx) = channel();
        tx.send("late allow-all".to_string()).unwrap();
        assert_eq!(
            wait_for_approval_reply(&rx, || true),
            Err("approval cancelled")
        );
        assert_eq!(rx.try_recv().unwrap(), "late allow-all");
        tx.send("cancelled after delivery".to_string()).unwrap();
        let checks = AtomicU64::new(0);
        assert_eq!(
            wait_for_approval_reply(&rx, || checks.fetch_add(1, Ordering::SeqCst) != 0),
            Err("approval cancelled")
        );
        drop(tx);
        assert_eq!(wait_for_approval_reply(&rx, || false), Err("stdin closed"));
    }

    #[test]
    fn cancelled_waiter_cleanup_does_not_touch_its_sibling() {
        let mut r = run();
        r.stdin_reader = StdinReader::Live;
        let a = r.register_waiter("cancelled").unwrap();
        let b = r.register_waiter("live").unwrap();
        r.waiting().remove("cancelled");
        r.deliver_reply("cancelled", "late grant".into());
        r.deliver_reply("live", "own decision".into());
        assert!(a.try_recv().is_err());
        assert_eq!(b.try_recv().unwrap(), "own decision");
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

    #[test]
    fn approval_identity_is_invocation_local_and_single_use_across_threads() {
        let first = ApprovalIdentity::default();
        let second = ApprovalIdentity::default();
        first.bind("toolu_a1", "req_1");
        second.bind("toolu_a2", "req_2");
        let worker = second.clone();
        assert_eq!(
            std::thread::spawn(move || worker.take()).join().unwrap(),
            Some(("toolu_a2".into(), "req_2".into()))
        );
        assert_eq!(second.take(), None, "worker claims this invocation once");
        assert_eq!(
            first.take(),
            Some(("toolu_a1".into(), "req_1".into())),
            "out-of-order consumption cannot take another request"
        );
        assert_eq!(first.take(), None);
        let abandoned = ApprovalIdentity::default();
        abandoned.bind("toolu_a3", "req_3");
        drop(abandoned);
        assert_eq!(
            ApprovalIdentity::default().take(),
            None,
            "no global stale identity"
        );
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
    fn result_emits_only_the_appended_verification_tail() {
        let final_text = "Answer\n\nVerification: pending";
        assert_eq!(
            unstreamed_result_text(final_text, Some("Answer"), "earlier textAnswer"),
            "\n\nVerification: pending"
        );
        assert_eq!(
            unstreamed_result_text("Answer", Some("Answer"), "Answer"),
            ""
        );
        assert_eq!(unstreamed_result_text(final_text, None, ""), final_text);
        assert_eq!(
            unstreamed_result_text(final_text, Some("unrelated"), "unrelated"),
            final_text
        );
        assert_eq!(unstreamed_result_text(final_text, Some(""), ""), final_text);
    }

    #[test]
    fn result_preserves_nonstreamed_and_partial_provider_answers() {
        let final_text = "Answer\n\nVerification: pending";
        assert_eq!(
            unstreamed_result_text(final_text, Some("Answer"), ""),
            final_text,
            "a returned completion is not proof that it streamed"
        );
        assert_eq!(
            unstreamed_result_text(final_text, Some("Answer"), "Ans"),
            final_text,
            "partial fragments must not hide the remainder"
        );
    }

    #[test]
    fn stop_words_map_to_result_subtypes() {
        assert_eq!(result_kind("done"), ("success", false));
        assert_eq!(result_kind("awaiting_input"), ("success", false));
        assert_eq!(result_kind("incomplete"), ("error_during_execution", true));
        assert_eq!(result_kind("max_iters"), ("error_max_turns", true));
        assert_eq!(result_kind("divergence"), ("error_during_execution", true));
        assert_eq!(result_kind("cancelled"), ("error_during_execution", true));
    }

    #[test]
    fn completion_basis_labels_preserve_unverified_and_noncheck_distinctions() {
        use crate::agent::todo::CompletionBasis;
        for (basis, label) in [
            (CompletionBasis::Observed, "observed"),
            (CompletionBasis::Reported, "reported"),
            (CompletionBasis::Verified, "verified"),
            (CompletionBasis::NotRequired, "not_required"),
            (CompletionBasis::Unverified, "unverified"),
        ] {
            assert_eq!(completion_basis_label(basis), label);
        }
    }

    #[test]
    fn incomplete_verdict_travels_with_captured_and_json_answers() {
        let answer = "All changes are complete.";
        let result = completion_result_text("incomplete", answer);
        assert!(result.starts_with(answer));
        assert!(result.ends_with(INCOMPLETE_VERDICT));
        assert!(completion_result_text("incomplete", "").contains(INCOMPLETE_VERDICT));
        for stop in ["done", "awaiting_input", "cancelled", "deadline"] {
            assert_eq!(completion_result_text(stop, answer), answer);
        }
        assert_eq!(completion_fields("done")["completed"], true);
        for stop in ["incomplete", "awaiting_input", "verification_failed"] {
            let completion = completion_fields(stop);
            assert_eq!(completion["status"], stop);
            assert_eq!(completion["completed"], false);
            assert!(completion.get("verified").is_none());
        }
    }

    /// A steer line keeps its text, a reply keeps its id, and neither is confused for the other.
    #[test]
    fn stdin_lines_are_sorted_by_their_type_key() {
        match classify_inbound(r#"{"type":"steer","text":"skip the tests"}"#) {
            Inbound::Steer(text) => assert_eq!(text, "skip the tests"),
            _ => panic!("a steer line is a steer"),
        }
        let reply = r#"{"type":"control_response","response":{"request_id":"req_9","response":{"behavior":"allow"}}}"#;
        match classify_inbound(reply) {
            Inbound::Reply { request_id, line } => {
                assert_eq!(request_id, "req_9");
                assert_eq!(line, reply, "the waiter re-parses the whole line");
            }
            _ => panic!("a control_response is a reply"),
        }
        assert!(
            matches!(
                classify_inbound(r#"{"type":"steer","text":"  "}"#),
                Inbound::Ignored
            ),
            "a blank steer is ignored"
        );
        assert!(
            matches!(classify_inbound("not json"), Inbound::Ignored),
            "not JSON is ignored"
        );
        assert!(
            matches!(
                classify_inbound(r#"{"type":"user","message":{}}"#),
                Inbound::Ignored
            ),
            "another record type is ignored"
        );
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
            None,
            "a reply without an id cannot authorize any request"
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
    fn invocation_approval_never_inherits_generic_allow_all() {
        assert!(ApprovalScope::Session.bypasses(true));
        assert!(!ApprovalScope::Session.bypasses(false));
        assert!(!ApprovalScope::Invocation.bypasses(true));
        assert!(!ApprovalScope::Invocation.bypasses(false));
    }

    #[test]
    fn invocation_permission_updates_do_not_create_persistent_grants() {
        for update in [
            json!({"type": "setMode", "mode": "bypassPermissions", "destination": "session"}),
            json!({"type": "addRules", "rules": [{"toolName": "image_read"}], "behavior": "allow", "destination": "session"}),
        ] {
            let line = json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "req_media",
                    "response": {"behavior": "allow", "updatedPermissions": [update]},
                },
            })
            .to_string();
            let (decision, _) = parse_reply(&line, "req_media", "image_read").unwrap();
            assert!(matches!(decision, Decision::AllowTool | Decision::AllowAll));
            assert_eq!(
                ApprovalScope::Invocation.decision(decision),
                Decision::Allow
            );
            assert_eq!(ApprovalScope::Session.decision(decision), decision);
        }
        assert_eq!(
            ApprovalScope::Invocation.decision(Decision::Deny),
            Decision::Deny
        );
        assert_eq!(
            ApprovalScope::Invocation.decision(Decision::Allow),
            Decision::Allow
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
