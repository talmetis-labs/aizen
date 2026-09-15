//! Record / replay tape for model calls.
//!
//! Every model call in the process funnels through three client functions
//! (`chat_with_tools_effort`, `stream_chat_with_tools_eager`, `stream_chat_with_visual_contract`).
//! Each asks this module first: in `replay` the answer comes off a tape and no request is sent; in
//! `record` the live answer is appended to the tape after it arrives. That is what lets the task
//! suite (`aizen bench tasks`) drive the REAL loop with REAL tools — edits land, `cargo test`
//! runs — while the model's decisions come from a file, so CI needs no key and the run is
//! deterministic and free.
//!
//! ```text
//! AIZEN_TAPE=record  AIZEN_TAPE_FILE=tapes/x.jsonl aizen agent "…"   # writes one line per call
//! AIZEN_TAPE=replay  AIZEN_TAPE_FILE=tapes/x.jsonl aizen agent "…"   # answers from the tape, warns on drift
//! AIZEN_TAPE=strict  AIZEN_TAPE_FILE=tapes/x.jsonl aizen agent "…"   # answers from the tape, FAILS on drift
//! ```
//!
//! Matching is by ORDINAL: the Nth model call of the run gets the Nth line. A fingerprint of what
//! the model was shown travels with each line so a changed prompt is noticed rather than silently
//! answered with a stale reply. Two fingerprints, because they drift for different reasons:
//! `system_fp` covers the system messages (a prompt edit, a different OS or tool surface), and
//! `turns_fp` covers the conversation plus the advertised tool names (a tool result that changed,
//! a different tool set). Both are computed over SCRUBBED text — the workspace root, dates, clock
//! times, durations, long hex hashes and tool-call ids are replaced by placeholders — so the same
//! recording replays cleanly from a different temp directory or a day later.
//!
//! What a tape does NOT capture: the streaming render (a replayed turn prints nothing live), eager
//! tool starts (the executor runs every replayed call normally), and the ORDER of concurrent calls.
//! Record single-threaded flows — the bench runner, a one-shot `aizen agent` — not a REPL with
//! background chores racing the turn.

use crate::core::types::{Message, ToolCall, ToolDef, Usage};
use crate::llm::client::ChatTurn;
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What the tape does this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No tape: every call goes to the provider (the default).
    Off,
    /// Calls go to the provider and each answer is appended to the tape.
    Record,
    /// Answers come off the tape; a fingerprint mismatch is reported and the run continues.
    Replay,
    /// Answers come off the tape; a fingerprint mismatch (or an exhausted tape) fails the call.
    Strict,
}

impl Mode {
    /// Parse the `AIZEN_TAPE` value. Unknown words are `None` so a typo cannot silently mean "off".
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "false" => Some(Mode::Off),
            "record" => Some(Mode::Record),
            "replay" => Some(Mode::Replay),
            "strict" => Some(Mode::Strict),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Record => "record",
            Mode::Replay => "replay",
            Mode::Strict => "strict",
        }
    }

    fn reads_tape(self) -> bool {
        matches!(self, Mode::Replay | Mode::Strict)
    }
}

/// Usage as recorded: the raw provider numbers, so the accounting shape (Anthropic-style
/// exclusive vs OpenAI-style inclusive `prompt`) survives the round trip.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapedUsage {
    #[serde(default)]
    pub prompt: u64,
    #[serde(default)]
    pub completion: u64,
    #[serde(default)]
    pub cached: u64,
    #[serde(default)]
    pub cache_write: u64,
}

/// The serializable half of a [`ChatTurn`] (no eager handles).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TapedTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TapedUsage>,
}

impl TapedTurn {
    fn from_turn(t: &ChatTurn) -> Self {
        Self {
            content: t.content.clone(),
            tool_calls: t.tool_calls.clone(),
            finish_reason: t.finish_reason.clone(),
            usage: t.usage.as_ref().map(|u| TapedUsage {
                prompt: u.prompt_tokens.unwrap_or(0),
                completion: u.completion_tokens.unwrap_or(0),
                cached: u.cache_read(),
                cache_write: u.cache_write(),
            }),
        }
    }

    fn into_turn(self) -> ChatTurn {
        let usage = self.usage.map(|u| Usage {
            prompt_tokens: Some(u.prompt),
            completion_tokens: Some(u.completion),
            total_tokens: Some(u.prompt + u.completion),
            cache_read_input_tokens: (u.cached > 0).then_some(u.cached),
            cache_creation_input_tokens: (u.cache_write > 0).then_some(u.cache_write),
            prompt_tokens_details: None,
        });
        ChatTurn {
            content: self.content,
            tool_calls: self.tool_calls,
            finish_reason: self.finish_reason,
            usage,
            eager: Vec::new(),
        }
    }
}

/// One line of the tape: one model call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeEntry {
    /// 0-based position in the run.
    pub ordinal: usize,
    /// The model id the call was made with (informational — never a drift criterion, since a
    /// replay legitimately runs without any model configured).
    #[serde(default)]
    pub model: String,
    /// fnv1a64 over the scrubbed system messages.
    #[serde(default)]
    pub system_fp: String,
    /// fnv1a64 over the scrubbed non-system messages plus the sorted advertised tool names.
    #[serde(default)]
    pub turns_fp: String,
    pub turn: TapedTurn,
}

/// A read-only view of the active tape for reports (`aizen bench tasks`, tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub mode: Mode,
    pub path: PathBuf,
    /// Calls served or recorded so far.
    pub position: usize,
    /// Entries on the tape (replay/strict only; 0 while recording).
    pub total: usize,
    /// Replayed calls whose fingerprint differed from the recording.
    pub drift: usize,
}

struct TapeState {
    mode: Mode,
    path: PathBuf,
    /// The workspace root, in the spellings it appears in messages; replaced by `<root>`.
    scrub_roots: Vec<String>,
    entries: Vec<TapeEntry>,
    next: usize,
    drift: usize,
    /// A replay tape that could not be read: surfaced on the first call, not at configure time,
    /// so env-driven runs fail where the user looks (the call) rather than in a silent init.
    load_error: Option<String>,
}

/// `None` ⇒ off. Initialized from the environment on first use; `configure` replaces it.
static STATE: Mutex<Option<TapeState>> = Mutex::new(None);
static ENV_READ: Mutex<bool> = Mutex::new(false);

fn with_state<R>(f: impl FnOnce(&mut Option<TapeState>) -> R) -> R {
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut env_read = ENV_READ.lock().unwrap_or_else(|e| e.into_inner());
    if !*env_read {
        *env_read = true;
        if st.is_none() {
            *st = state_from_env();
        }
    }
    drop(env_read);
    f(&mut st)
}

fn state_from_env() -> Option<TapeState> {
    let raw = std::env::var("AIZEN_TAPE").ok()?;
    let mode = match Mode::parse(&raw) {
        Some(Mode::Off) => return None,
        Some(m) => m,
        None => {
            emit_note(&format!(
                "AIZEN_TAPE={raw:?} is not one of record|replay|strict — tape is OFF"
            ));
            return None;
        }
    };
    let path = std::env::var("AIZEN_TAPE_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_tape_path);
    let root = std::env::var("AIZEN_TAPE_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    match build_state(mode, path, root.as_deref()) {
        Ok(s) => Some(s),
        Err(e) => {
            emit_note(&format!("tape disabled: {e:#}"));
            None
        }
    }
}

fn default_tape_path() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(".aizen")
        .join("tapes")
        .join(format!("{stamp}-{}.jsonl", std::process::id()))
}

fn build_state(mode: Mode, path: PathBuf, root: Option<&Path>) -> Result<TapeState> {
    let mut entries = Vec::new();
    let mut load_error = None;
    match mode {
        Mode::Record => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
            }
            // A recording starts a FRESH tape: appending to a stale one would interleave two runs.
            std::fs::File::create(&path)
                .with_context(|| format!("creating tape {}", path.display()))?;
        }
        Mode::Replay | Mode::Strict => match load_entries(&path) {
            Ok(v) => entries = v,
            Err(e) => load_error = Some(format!("{e:#}")),
        },
        Mode::Off => {}
    }
    Ok(TapeState {
        mode,
        path,
        scrub_roots: root.map(root_spellings).unwrap_or_default(),
        entries,
        next: 0,
        drift: 0,
        load_error,
    })
}

/// Read a tape. Blank lines are skipped; a malformed line names its number.
pub fn load_entries(path: &Path) -> Result<Vec<TapeEntry>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading tape {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let e: TapeEntry = serde_json::from_str(line)
            .with_context(|| format!("tape {} line {}", path.display(), i + 1))?;
        out.push(e);
    }
    Ok(out)
}

/// Every spelling a root directory takes inside messages: as given, canonical, with the Windows
/// `\\?\` verbatim prefix stripped, and each of those with forward slashes. Longest first so the
/// verbatim form is replaced before its suffix would match.
fn root_spellings(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut push = |s: String| {
        let s = s.trim_end_matches(['/', '\\']).to_string();
        if s.len() > 1 && !v.contains(&s) {
            v.push(s);
        }
    };
    let given = root.display().to_string();
    push(given.clone());
    push(given.replace('\\', "/"));
    if let Ok(c) = root.canonicalize() {
        let c = c.display().to_string();
        let plain = c.strip_prefix(r"\\?\").unwrap_or(&c).to_string();
        push(c.clone());
        push(plain.clone());
        push(plain.replace('\\', "/"));
    }
    v.sort_by_key(|s| std::cmp::Reverse(s.len()));
    v
}

/// Point the process at a tape programmatically (the bench runner). Replaces any env-derived
/// state and resets the ordinal. `root` is the workspace the messages will mention.
pub fn configure(mode: Mode, path: impl Into<PathBuf>, root: Option<&Path>) -> Result<()> {
    let path = path.into();
    let state = if mode == Mode::Off {
        None
    } else {
        Some(build_state(mode, path, root)?)
    };
    with_state(|st| *st = state);
    Ok(())
}

/// Turn the tape off (after a bench task, so the next one starts clean).
pub fn disable() {
    with_state(|st| *st = None);
}

pub fn mode() -> Mode {
    with_state(|st| st.as_ref().map(|s| s.mode).unwrap_or(Mode::Off))
}

pub fn status() -> Option<Status> {
    with_state(|st| {
        st.as_ref().map(|s| Status {
            mode: s.mode,
            path: s.path.clone(),
            position: s.next,
            total: s.entries.len(),
            drift: s.drift,
        })
    })
}

/// In `replay`/`strict`: the next recorded turn, or an error (exhausted tape, unreadable tape,
/// drift under `strict`). `Ok(None)` means "no tape — make the live call".
pub fn replay_turn(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<Option<ChatTurn>> {
    with_state(|st| {
        let Some(s) = st.as_mut() else {
            return Ok(None);
        };
        if !s.mode.reads_tape() {
            return Ok(None);
        }
        if let Some(e) = &s.load_error {
            bail!("{e}");
        }
        let i = s.next;
        let Some(entry) = s.entries.get(i) else {
            bail!(
                "tape {} exhausted: the run made call #{} but only {} were recorded",
                s.path.display(),
                i + 1,
                s.entries.len()
            );
        };
        s.next += 1;
        let (system_fp, turns_fp) = fingerprints(messages, tools, &s.scrub_roots);
        let mut differs: Vec<&str> = Vec::new();
        if entry.system_fp != system_fp {
            differs.push("system prompt");
        }
        if entry.turns_fp != turns_fp {
            differs.push("conversation/tools");
        }
        if !differs.is_empty() {
            s.drift += 1;
            let note = format!(
                "⟲ tape call #{} of {}: {} differ(s) from the recording ({}, model {})",
                i + 1,
                s.entries.len(),
                differs.join(" + "),
                s.path.display(),
                if entry.model.is_empty() {
                    "?"
                } else {
                    &entry.model
                }
            );
            if s.mode == Mode::Strict {
                bail!("{note} — AIZEN_TAPE=strict");
            }
            emit_note(&note);
        }
        let turn = entry.turn.clone().into_turn();
        // Keep the cost meter (and so `/cost` and the session ledger) honest on a replayed run.
        if let Some(u) = &turn.usage {
            crate::llm::client::cost_meter().record(u);
        }
        let _ = model;
        Ok(Some(turn))
    })
}

/// In `record`: append this call. Anything else: no-op.
pub fn record_turn(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    turn: &ChatTurn,
) -> Result<()> {
    with_state(|st| {
        let Some(s) = st.as_mut() else {
            return Ok(());
        };
        if s.mode != Mode::Record {
            return Ok(());
        }
        let (system_fp, turns_fp) = fingerprints(messages, tools, &s.scrub_roots);
        let entry = TapeEntry {
            ordinal: s.next,
            model: model.to_string(),
            system_fp,
            turns_fp,
            turn: TapedTurn::from_turn(turn),
        };
        s.next += 1;
        let line = serde_json::to_string(&entry).context("serializing tape entry")?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&s.path)
            .with_context(|| format!("opening tape {}", s.path.display()))?;
        f.write_all(line.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .with_context(|| format!("appending to tape {}", s.path.display()))
    })
}

/// Plain-text calls (no tools) share the tape: a text answer is a turn with content only.
pub fn replay_text(model: &str, messages: &[Message]) -> Result<Option<String>> {
    Ok(replay_turn(model, messages, &[])?.map(|t| t.content.unwrap_or_default()))
}

pub fn record_text(model: &str, messages: &[Message], text: &str) -> Result<()> {
    let turn = ChatTurn {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        finish_reason: Some("stop".into()),
        usage: None,
        eager: Vec::new(),
    };
    record_turn(model, messages, &[], &turn)
}

// ── fingerprinting ────────────────────────────────────────────────────────────

static RE_DATE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{4}-\d{2}-\d{2}\b").unwrap());
static RE_TIME: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{1,2}:\d{2}(?::\d{2})?\b").unwrap());
static RE_DURATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d+(?:\.\d+)?\s?(?:ns|µs|us|ms|s|sec|secs)\b").unwrap());
static RE_HEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[0-9a-f]{16,}\b").unwrap());
static RE_CALL_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bcall_[A-Za-z0-9_-]+").unwrap());
static RE_WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// Replace the parts of a message that legitimately differ between a recording and a replay.
pub fn scrub(s: &str, roots: &[String]) -> String {
    let mut out = s.to_string();
    for r in roots {
        if !r.is_empty() {
            out = out.replace(r.as_str(), "<root>");
        }
    }
    let out = RE_DATE.replace_all(&out, "<date>");
    let out = RE_TIME.replace_all(&out, "<time>");
    let out = RE_DURATION.replace_all(&out, "<dur>");
    let out = RE_HEX.replace_all(&out, "<hash>");
    let out = RE_CALL_ID.replace_all(&out, "<call>");
    RE_WS.replace_all(&out, " ").trim().to_string()
}

/// FNV-1a, 64-bit. Small, dependency-free, and stable across platforms — a tape recorded on one
/// machine must fingerprint identically on another.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn feed_message(buf: &mut String, m: &Message, roots: &[String]) {
    buf.push_str(&m.role);
    buf.push('\u{1f}');
    if let Some(c) = &m.content {
        buf.push_str(&scrub(c, roots));
    }
    for tc in &m.tool_calls {
        buf.push('\u{1f}');
        buf.push_str(&tc.function.name);
        buf.push('(');
        buf.push_str(&scrub(&tc.function.arguments, roots));
        buf.push(')');
    }
    if !m.images.is_empty() {
        // Base64 payloads are huge and stable; their count and size identify them well enough.
        buf.push_str(&format!(
            "\u{1f}images:{}:{}",
            m.images.len(),
            m.images.iter().map(String::len).sum::<usize>()
        ));
    }
    buf.push('\u{1e}');
}

/// `(system_fp, turns_fp)` for what the model is about to see.
pub fn fingerprints(messages: &[Message], tools: &[ToolDef], roots: &[String]) -> (String, String) {
    let mut sys = String::new();
    let mut turns = String::new();
    for m in messages {
        if m.role == "system" {
            feed_message(&mut sys, m, roots);
        } else {
            feed_message(&mut turns, m, roots);
        }
    }
    let mut names: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
    names.sort_unstable();
    turns.push_str("\u{1d}tools:");
    turns.push_str(&names.join(","));
    (
        format!("fnv1a64:{:016x}", fnv1a64(sys.as_bytes())),
        format!("fnv1a64:{:016x}", fnv1a64(turns.as_bytes())),
    )
}

fn emit_note(note: &str) {
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
    } else {
        eprintln!("{note}");
    }
}

/// The tape state is process-global; tests that touch it (here and in `bench::task_eval`) take
/// this lock so they never interleave.
#[cfg(test)]
pub(crate) static TAPE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::FunctionCall;

    fn temp_tape(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aizen-tape-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.jsonl"))
    }

    fn call_turn(name: &str, args: &str) -> ChatTurn {
        ChatTurn {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_abc123".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: args.into(),
                },
            }],
            finish_reason: Some("tool_calls".into()),
            usage: Some(Usage {
                prompt_tokens: Some(120),
                completion_tokens: Some(9),
                total_tokens: Some(129),
                cache_read_input_tokens: Some(100),
                cache_creation_input_tokens: None,
                prompt_tokens_details: None,
            }),
            eager: Vec::new(),
        }
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef::function(name, "d", serde_json::json!({"type":"object"}))
    }

    #[test]
    fn mode_parse_rejects_typos_instead_of_meaning_off() {
        assert_eq!(Mode::parse("record"), Some(Mode::Record));
        assert_eq!(Mode::parse(" REPLAY "), Some(Mode::Replay));
        assert_eq!(Mode::parse("strict"), Some(Mode::Strict));
        assert_eq!(Mode::parse(""), Some(Mode::Off));
        assert_eq!(Mode::parse("0"), Some(Mode::Off));
        assert_eq!(Mode::parse("recrod"), None);
    }

    #[test]
    fn scrub_replaces_the_volatile_parts_only() {
        let roots = root_spellings(Path::new(r"C:\work\proj"));
        let s = scrub(
            "read C:/work/proj/src/lib.rs at 2026-09-15 14:03:22, took 0.19s, id call_x9 hash 0123456789abcdef01",
            &roots,
        );
        assert_eq!(
            s,
            "read <root>/src/lib.rs at <date> <time>, took <dur>, id <call> hash <hash>"
        );
        // Plain identifiers, versions and small numbers survive.
        assert_eq!(
            scrub("v0.6.7 has 3 tests and 12 files", &[]),
            "v0.6.7 has 3 tests and 12 files"
        );
    }

    #[test]
    fn fingerprints_split_system_from_conversation_and_include_tool_names() {
        let sys = Message::system("you are a coder");
        let user = Message::user("fix it");
        let a = fingerprints(&[sys.clone(), user.clone()], &[tool("file_read")], &[]);
        let b = fingerprints(
            &[Message::system("you are a poet"), user.clone()],
            &[tool("file_read")],
            &[],
        );
        let c = fingerprints(&[sys.clone(), user.clone()], &[tool("file_edit")], &[]);
        let d = fingerprints(&[sys, Message::user("fix that")], &[tool("file_read")], &[]);
        assert_ne!(a.0, b.0, "a prompt edit changes the system fingerprint");
        assert_eq!(a.1, b.1, "…and leaves the conversation fingerprint alone");
        assert_eq!(a.0, c.0);
        assert_ne!(
            a.1, c.1,
            "a different tool surface changes the conversation fingerprint"
        );
        assert_ne!(a.1, d.1);
    }

    #[test]
    fn fnv1a64_matches_the_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn record_then_replay_returns_the_same_turns_in_order_and_records_usage() {
        let _g = TAPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_tape("roundtrip");
        let root = std::env::temp_dir();
        let msgs = vec![Message::system("sys"), Message::user("go")];
        let tools = vec![tool("file_read")];

        configure(Mode::Record, &path, Some(&root)).unwrap();
        assert_eq!(mode(), Mode::Record);
        assert!(
            replay_turn("m", &msgs, &tools).unwrap().is_none(),
            "record mode makes live calls"
        );
        record_turn(
            "m",
            &msgs,
            &tools,
            &call_turn("file_read", r#"{"path":"a.rs"}"#),
        )
        .unwrap();
        let mut msgs2 = msgs.clone();
        msgs2.push(Message::tool_result("call_abc123", "fn main(){}"));
        let fin = ChatTurn {
            content: Some("done".into()),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            eager: Vec::new(),
        };
        record_turn("m", &msgs2, &tools, &fin).unwrap();
        assert_eq!(status().unwrap().position, 2);

        configure(Mode::Replay, &path, Some(&root)).unwrap();
        let st = status().unwrap();
        assert_eq!((st.total, st.position, st.drift), (2, 0, 0));
        let before = crate::llm::client::cost_meter().snapshot();
        let t1 = replay_turn("other-model", &msgs, &tools).unwrap().unwrap();
        assert_eq!(t1.tool_calls.len(), 1);
        assert_eq!(t1.tool_calls[0].function.name, "file_read");
        assert_eq!(t1.tool_calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        let u = t1.usage.as_ref().unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.cache_read()),
            (Some(120), Some(9), 100)
        );
        let after = crate::llm::client::cost_meter().snapshot();
        assert!(
            after.0 >= before.0 + 120,
            "replayed usage reaches the cost meter"
        );
        let t2 = replay_turn("other-model", &msgs2, &tools).unwrap().unwrap();
        assert_eq!(t2.content.as_deref(), Some("done"));
        assert!(t2.tool_calls.is_empty());
        assert_eq!(
            status().unwrap().drift,
            0,
            "identical inputs replay without drift"
        );
        assert!(
            replay_turn("m", &msgs2, &tools).is_err(),
            "a third call runs off the end of a two-line tape"
        );
        disable();
        assert_eq!(mode(), Mode::Off);
    }

    #[test]
    fn replay_warns_on_a_prompt_edit_and_strict_fails() {
        let _g = TAPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_tape("drift");
        let msgs = vec![Message::system("prompt v1"), Message::user("go")];
        configure(Mode::Record, &path, None).unwrap();
        record_turn("m", &msgs, &[], &call_turn("echo", "{}")).unwrap();

        let edited = vec![Message::system("prompt v2"), Message::user("go")];
        configure(Mode::Replay, &path, None).unwrap();
        let t = replay_turn("m", &edited, &[]).unwrap().unwrap();
        assert_eq!(
            t.tool_calls[0].function.name, "echo",
            "replay still answers"
        );
        assert_eq!(status().unwrap().drift, 1, "…but counts the drift");

        configure(Mode::Strict, &path, None).unwrap();
        let err = replay_turn("m", &edited, &[])
            .err()
            .expect("strict mode refuses a drifted call")
            .to_string();
        assert!(err.contains("system prompt"), "{err}");
        assert!(err.contains("strict"), "{err}");
        disable();
    }

    #[test]
    fn a_missing_replay_tape_fails_at_the_first_call_not_at_configure() {
        let _g = TAPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_tape("does-not-exist-zz");
        let _ = std::fs::remove_file(&path);
        configure(Mode::Replay, &path, None).unwrap();
        let err = replay_turn("m", &[Message::user("x")], &[])
            .err()
            .expect("an unreadable tape fails the call")
            .to_string();
        assert!(err.contains("reading tape"), "{err}");
        disable();
    }

    #[test]
    fn record_starts_a_fresh_tape_and_text_calls_share_it() {
        let _g = TAPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_tape("fresh");
        std::fs::write(
            &path,
            "{\"ordinal\":0,\"turn\":{}}\n{\"ordinal\":1,\"turn\":{}}\n",
        )
        .unwrap();
        configure(Mode::Record, &path, None).unwrap();
        record_text("m", &[Message::user("summarize")], "a summary").unwrap();
        let entries = load_entries(&path).unwrap();
        assert_eq!(entries.len(), 1, "the stale two-line tape was truncated");
        assert_eq!(entries[0].turn.content.as_deref(), Some("a summary"));
        configure(Mode::Replay, &path, None).unwrap();
        assert_eq!(
            replay_text("m", &[Message::user("summarize")])
                .unwrap()
                .as_deref(),
            Some("a summary")
        );
        disable();
    }

    #[test]
    fn root_spellings_cover_both_slash_styles_longest_first() {
        let v = root_spellings(Path::new(r"C:\a\b"));
        assert!(v.contains(&"C:\\a\\b".to_string()));
        assert!(v.contains(&"C:/a/b".to_string()));
        for w in v.windows(2) {
            assert!(w[0].len() >= w[1].len());
        }
    }
}
