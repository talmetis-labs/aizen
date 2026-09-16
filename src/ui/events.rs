//! The machine-readable event stream behind `aizen agent --output-format json`.
//!
//! One JSON object per line on stdout, and nothing else on stdout. Every line carries `"type"`;
//! the other fields are per type and documented in `docs/REFERENCE.md` ("Machine-readable
//! output"). Off by default: [`enable`] is called once by the one-shot runner when the flag is
//! set, and every surface that would otherwise print for a human — the loop's trace lines, the
//! tool-call rows, the streamed answer, the plan box, the approval question — checks [`on`] first
//! and hands the same information here as data instead.
//!
//! This stream is the contract a front-end builds on (the desktop app, an editor extension, a CI
//! script), which is why it exists: before it, those callers parsed the human transcript by its
//! leading glyphs, and every cosmetic change to a line broke them without an error. Fields are
//! only ever ADDED to an event; a consumer must ignore types and fields it does not know.
//!
//! Approvals ride the same channel in both directions: an `approval_request` line goes out, and
//! the caller answers on stdin with `{"type":"approval","id":N,"decision":"allow"}` (see
//! [`ask_approval`]). A caller that closes stdin gets a safe deny, exactly like a non-TTY run.

use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

static ON: AtomicBool = AtomicBool::new(false);
/// Set by an `allow_all` reply: every later approval is answered without asking.
static ALLOW_ALL: AtomicBool = AtomicBool::new(false);
static APPROVAL_SEQ: AtomicU64 = AtomicU64::new(0);
/// Lines are written whole: tool bodies emit from blocking threads while the driver streams text,
/// and two half-lines interleaved would be two broken events.
static WRITE: Mutex<()> = Mutex::new(());

/// A tool result longer than this (chars) is cut in the `tool_result` event, with `truncated: true`.
/// The model's own copy is budgeted separately (`AgentConfig::max_tool_result_chars`).
pub const OUTPUT_CAP: usize = 64 * 1024;

/// Switch the process to JSON output. Irreversible for the process: a one-shot run is one turn.
pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

/// Whether the JSON stream owns stdout.
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Write one event. `fields` should be an object; `type` is set here and wins over any `type`
/// key in `fields`. A no-op unless [`on`].
pub fn emit(kind: &str, fields: Value) {
    if !on() {
        return;
    }
    let line = line_for(kind, fields);
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// The exact text an event serializes to (no trailing newline). Compact JSON, so an embedded
/// newline in any string is escaped and one event is always one physical line.
pub(crate) fn line_for(kind: &str, fields: Value) -> String {
    let mut obj = match fields {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    obj.insert("type".to_string(), Value::String(kind.to_string()));
    Value::Object(obj).to_string()
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

// ── the events ───────────────────────────────────────────────────────────────

/// The first line of a run: what is about to happen and with what.
pub fn start(model: &str, cwd: &str, effort: Option<&str>, images: usize) {
    emit(
        "start",
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "model": model,
            "cwd": cwd,
            "effort": effort,
            "images": images,
            "pid": std::process::id(),
        }),
    );
}

/// A fragment of the assistant's answer, in the order it streamed. Concatenate the deltas of a
/// run to get the raw markdown the transcript keeps.
pub fn text(delta: &str) {
    if !delta.is_empty() {
        emit("text", json!({ "delta": delta }));
    }
}

/// A fragment of the model's reasoning channel, when the provider exposes one. Never part of the
/// answer; shown by front-ends that want to, ignored by everyone else.
pub fn reasoning(delta: &str) {
    if !delta.is_empty() {
        emit("reasoning", json!({ "delta": delta }));
    }
}

/// A line the human transcript would have shown as progress (`→ checkpoint #3 saved`, a retry
/// note, a context-clearing note). Styling stripped; empty lines dropped.
pub fn trace(text: &str) {
    let clean = plain(text);
    if !clean.is_empty() {
        emit("trace", json!({ "text": clean }));
    }
}

/// A safety-floor or sandbox notice about a call: `kind` is `blocked`, `caution` or `network`.
pub fn warning(kind: &str, text: &str) {
    emit("warning", json!({ "kind": kind, "text": plain(text) }));
}

/// A tool call is starting. `seq` ties the later `tool_result` to this call.
pub fn tool_call(seq: u64, name: &str, args: &Value, target: &str) {
    emit(
        "tool_call",
        json!({ "seq": seq, "name": name, "args": args, "target": target }),
    );
}

/// A tool call finished. `digest` is the one-line summary the transcript shows; `output` is the
/// raw result (cut at [`OUTPUT_CAP`]) before the model's own budget is applied.
#[allow(clippy::too_many_arguments)]
pub fn tool_result(
    seq: u64,
    name: &str,
    target: &str,
    ok: bool,
    digest: &str,
    elapsed_ms: Option<u64>,
    output: &str,
) {
    let (out, truncated) = clip(output, OUTPUT_CAP);
    emit(
        "tool_result",
        json!({
            "seq": seq,
            "name": name,
            "target": target,
            "ok": ok,
            "digest": digest,
            "elapsed_ms": elapsed_ms,
            "output": out,
            "truncated": truncated,
        }),
    );
}

/// The plan checklist as the agent last wrote it. `items` = `(status, text)` with status
/// 0 / 1 / 2 = pending / in progress / done — the same rows the panel renders.
pub fn plan(items: &[(u8, String)]) {
    emit("plan", json!({ "items": plan_rows(items) }));
}

/// The `plan` event's rows: the panel's numeric status as a word.
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

/// A verify-gate line: the command that ran and its verdict.
pub fn verify(command: &str, detail: &str) {
    emit(
        "verify",
        json!({ "command": command, "detail": plain(detail) }),
    );
}

/// The size of an edit, for a front-end that draws its own diff from the tool result.
pub fn diff(path: &str, added: usize, removed: usize) {
    emit(
        "diff",
        json!({ "path": path, "added": added, "removed": removed }),
    );
}

/// A user hook ran (see `agent::hooks`). `fields` is the hook's own report.
pub fn hook(fields: Value) {
    emit("hook", fields);
}

/// The finished conversation was written to the session pool.
pub fn session_saved(slug: &str, path: &str) {
    emit("session", json!({ "slug": slug, "path": path }));
}

/// The conversation could not be saved. Said, never silent: a caller may be about to reopen it.
pub fn session_not_saved(error: &str) {
    emit("session", json!({ "error": error }));
}

/// The last line of a successful run. `stop` is the loop's stop reason (`done`, `max_iters`,
/// `divergence`, `verification_failed`, `awaiting_input`, `cancelled`, `deadline`); `question` is
/// set with `awaiting_input`; `usage` sums the provider's reported tokens over the run.
pub fn done(
    stop: &str,
    steps: usize,
    final_text: Option<&str>,
    question: Option<&str>,
    session: Option<&str>,
    usage: Value,
) {
    emit(
        "done",
        json!({
            "stop": stop,
            "steps": steps,
            "final_text": final_text,
            "question": question,
            "session": session,
            "usage": usage,
        }),
    );
}

/// A fatal error: the run ends after this line with a non-zero exit.
pub fn error(message: &str) {
    emit("error", json!({ "message": message }));
}

// ── approvals over stdin ─────────────────────────────────────────────────────

/// What a caller answered to an `approval_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run this call.
    Allow,
    /// Run this call, and every later call of the same tool without asking.
    AllowTool,
    /// Run this call, and every later destructive call without asking (like `-y` from here on).
    AllowAll,
    /// Refuse this call; the model is told the user declined.
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowTool => "allow_tool",
            Self::AllowAll => "allow_all",
            Self::Deny => "deny",
        }
    }
}

/// Whether an earlier `allow_all` reply stands.
pub fn allow_all() -> bool {
    ALLOW_ALL.load(Ordering::Relaxed)
}

/// Ask the caller to approve a destructive call: write `approval_request` and block until a
/// matching `approval` reply arrives on stdin. Lines that are not a reply for this request are
/// ignored. EOF or a read error is a deny — the same safe default a non-TTY run has always had.
///
/// `who` names a delegated sub-agent when the request is a child's; `preview` is the call's
/// pre-flight payload (`{title, lines, diff}`), when the tool can compute one.
pub fn ask_approval(tool: &str, args: &Value, who: Option<&str>, preview: Option<Value>) -> bool {
    if allow_all() {
        return true;
    }
    let id = APPROVAL_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    emit(
        "approval_request",
        json!({
            "id": id,
            "tool": tool,
            "args": args,
            "who": who,
            "preview": preview,
            "reply": "write {\"type\":\"approval\",\"id\":<id>,\"decision\":\"allow|allow_tool|allow_all|deny\"} as one line on stdin",
        }),
    );
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => {
                emit(
                    "approval",
                    json!({ "id": id, "decision": "deny", "reason": "stdin closed" }),
                );
                return false;
            }
            Ok(_) => {}
        }
        if let Some(d) = parse_reply(&line, id) {
            match d {
                Decision::AllowTool => crate::core::approval::grant_session(tool, None),
                Decision::AllowAll => ALLOW_ALL.store(true, Ordering::Relaxed),
                Decision::Allow | Decision::Deny => {}
            }
            emit("approval", json!({ "id": id, "decision": d.as_str() }));
            return d != Decision::Deny;
        }
    }
}

/// Parse one stdin line as a reply to request `id`. `None` for anything that is not a reply to
/// this request (another type, another id, not JSON) — the caller keeps reading. A reply whose
/// decision is not a known word is a deny: an unrecognised answer must never run the call.
pub(crate) fn parse_reply(line: &str, id: u64) -> Option<Decision> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("approval") {
        return None;
    }
    if let Some(got) = v.get("id").and_then(Value::as_u64) {
        if got != id {
            return None;
        }
    }
    let word = v
        .get("decision")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    Some(match word.as_str() {
        "allow" | "yes" | "y" | "approve" => Decision::Allow,
        "allow_tool" | "tool" => Decision::AllowTool,
        "allow_all" | "all" => Decision::AllowAll,
        _ => Decision::Deny,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_one_line_with_its_type() {
        let line = line_for("text", json!({ "delta": "a\nb" }));
        assert!(
            !line.contains('\n'),
            "embedded newlines must be escaped: {line}"
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["delta"], "a\nb");
    }

    #[test]
    fn the_type_key_belongs_to_the_stream_not_the_fields() {
        let line = line_for("trace", json!({ "type": "impostor", "text": "x" }));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "trace");
    }

    #[test]
    fn a_non_object_payload_still_serializes() {
        let v: Value = serde_json::from_str(&line_for("odd", json!(3))).unwrap();
        assert_eq!(v["type"], "odd");
        assert_eq!(v["value"], 3);
    }

    #[test]
    fn trace_text_is_stripped_of_styling_and_blank_lines() {
        let styled = format!(
            "{}\n\n   \n  → second  ",
            console::style("→ first").red().bold()
        );
        assert_eq!(plain(&styled), "→ first\n  → second");
    }

    #[test]
    fn a_reply_is_matched_by_type_and_id() {
        assert_eq!(
            parse_reply(r#"{"type":"approval","id":3,"decision":"allow"}"#, 3),
            Some(Decision::Allow)
        );
        assert_eq!(
            parse_reply(r#"{"type":"approval","id":2,"decision":"allow"}"#, 3),
            None,
            "a reply to another request is not ours"
        );
        assert_eq!(
            parse_reply(r#"{"type":"approval","decision":"allow_all"}"#, 3),
            Some(Decision::AllowAll),
            "a reply without an id answers the pending request"
        );
        assert_eq!(parse_reply(r#"{"type":"steer","text":"x"}"#, 3), None);
        assert_eq!(parse_reply("not json", 3), None);
    }

    #[test]
    fn an_unknown_decision_word_is_a_deny() {
        assert_eq!(
            parse_reply(r#"{"type":"approval","id":1,"decision":"maybe"}"#, 1),
            Some(Decision::Deny)
        );
        assert_eq!(
            parse_reply(r#"{"type":"approval","id":1}"#, 1),
            Some(Decision::Deny)
        );
        assert_eq!(
            parse_reply(r#"{"type":"approval","id":1,"decision":"TOOL"}"#, 1),
            Some(Decision::AllowTool)
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
}
