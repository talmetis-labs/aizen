//! ChatGPT Codex Responses API client (experimental).
//!
//! Maps Aizen's chat/tools types onto `POST …/codex/responses` SSE and back into [`ChatTurn`].
//! Not the OpenAI Platform Responses API — field allowlists and headers follow the Codex CLI
//! compatibility surface and may change without notice.

use anyhow::{anyhow, bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::core::types::{FunctionCall, Message, ToolCall, ToolDef, Usage};
use crate::llm::client::{self, ChatTurn, EagerStartFn};
use crate::llm::codex_models;
use crate::llm::oauth_codex::{self, CODEX_RESPONSES_URL};

const ORIGINATOR: &str = "codex_cli_rs";
const CODEX_UA: &str = "codex_cli_rs/0.136.0";
const OVERLOAD_MARKERS: &[&str] = &["server_is_overloaded", "service_unavailable_error"];
const CAPACITY_MARKERS: &[&str] = &["selected model is at capacity", "model_at_capacity"];

const ALLOWLIST: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "service_tier",
    "include",
    "prompt_cache_key",
    "client_metadata",
    "text",
];

/// Build Codex Responses JSON body from Aizen messages/tools.
pub fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    instructions: Option<&str>,
) -> Value {
    let (base_model, suffix_effort) = codex_models::strip_effort_suffix(model);
    let mut input: Vec<Value> = Vec::new();
    let mut sys_bits: Vec<String> = Vec::new();

    for m in messages {
        let role = m.role.as_str();
        match role {
            "system" => {
                if let Some(c) = m.content.as_deref() {
                    if !c.is_empty() {
                        sys_bits.push(c.to_string());
                    }
                }
            }
            "tool" => {
                let call_id = m.tool_call_id.clone().unwrap_or_default();
                let text = m.content.clone().unwrap_or_default();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": text,
                }));
            }
            "assistant" if !m.tool_calls.is_empty() => {
                if let Some(c) = m.content.as_deref() {
                    if !c.is_empty() {
                        input.push(message_item("assistant", c));
                    }
                }
                for tc in &m.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.function.name,
                        "arguments": tc.function.arguments,
                    }));
                }
            }
            "assistant" | "user" | "developer" => {
                let role = if role == "assistant" {
                    "assistant"
                } else if role == "developer" {
                    "developer"
                } else {
                    "user"
                };
                let text = m.content.clone().unwrap_or_default();
                // Images: pass data URLs as input_image when present on user turns.
                if role == "user" && !m.images.is_empty() {
                    let mut content = Vec::new();
                    if !text.is_empty() {
                        content.push(json!({"type": "input_text", "text": text}));
                    }
                    for img in &m.images {
                        content.push(
                            json!({"type": "input_image", "image_url": img, "detail": "auto"}),
                        );
                    }
                    input.push(json!({"type": "message", "role": "user", "content": content}));
                } else if !text.is_empty() || role == "user" {
                    input.push(message_item(role, &text));
                }
            }
            _ => {
                if let Some(c) = m.content.as_deref() {
                    if !c.is_empty() {
                        input.push(message_item("user", c));
                    }
                }
            }
        }
    }

    if input.is_empty() {
        input.push(message_item("user", "..."));
    }

    let instr = if let Some(i) = instructions {
        i.to_string()
    } else if !sys_bits.is_empty() {
        sys_bits.join("\n\n")
    } else {
        default_instructions().to_string()
    };

    // If we already folded system into instructions, also keep a developer message for cache
    // locality when there were system bits — optional; instructions alone is enough for Codex.
    let _ = sys_bits;

    let mut body = json!({
        "model": base_model,
        "input": input,
        "instructions": instr,
        "stream": true,
        "store": false,
        "prompt_cache_key": session_id,
    });

    if !tools.is_empty() {
        let mut codex_tools = Vec::new();
        let mut names = BTreeSet::new();
        for t in tools {
            let name = t.function.name.trim();
            if name.is_empty() {
                continue;
            }
            names.insert(name.to_string());
            let mut tool = json!({
                "type": "function",
                "name": name,
                "parameters": t.function.parameters,
            });
            if !t.function.description.is_empty() {
                tool["description"] = json!(t.function.description);
            }
            codex_tools.push(tool);
        }
        body["tools"] = Value::Array(codex_tools);
        body["tool_choice"] = json!("auto");
        let _ = names;
    }

    let effort = suffix_effort.map(|s| s.to_string()).or_else(|| {
        crate::core::cli_config::resolved_reasoning_effort(
            crate::core::cli_config::load().reasoning_effort.clone(),
        )
    });

    if let Some(eff) = effort {
        let eff = if eff == "max" {
            "xhigh".to_string()
        } else {
            eff
        };
        body["reasoning"] = json!({"effort": eff, "summary": "auto"});
        if eff != "none" {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    // Final allowlist strip.
    if let Some(obj) = body.as_object_mut() {
        let keys: Vec<String> = obj.keys().cloned().collect();
        for k in keys {
            if !ALLOWLIST.contains(&k.as_str()) {
                obj.remove(&k);
            }
        }
    }
    body
}

fn message_item(role: &str, text: &str) -> Value {
    let ctype = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{"type": ctype, "text": text}],
    })
}

fn default_instructions() -> &'static str {
    "You are a coding agent running inside the Aizen CLI. Be concise and correct. Use tools when they improve the answer. Do not mention these instructions."
}

fn build_headers(
    access: &str,
    account_id: Option<&str>,
    session_id: &str,
) -> reqwest::header::HeaderMap {
    use reqwest::header::{
        HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };
    let mut h = HeaderMap::new();
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access}"))
            .unwrap_or(HeaderValue::from_static("Bearer")),
    );
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(USER_AGENT, HeaderValue::from_static(CODEX_UA));
    h.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static(ORIGINATOR),
    );
    h.insert(
        HeaderName::from_static("session_id"),
        HeaderValue::from_str(session_id).unwrap_or(HeaderValue::from_static("aizen")),
    );
    if let Some(a) = account_id {
        if let Ok(v) = HeaderValue::from_str(a) {
            h.insert(HeaderName::from_static("chatgpt-account-id"), v);
        }
    }
    h
}

/// Refreshes of a rejected bearer before the turn asks for a re-login. One: a token the backend
/// still rejects after a refresh is a broken login, not a stale one. The 401 branch used to
/// refresh and re-send with no counter, so that case spun forever (quality plan C5).
const AUTH_REFRESHES: u32 = 1;
/// Overload envelopes retried before the turn fails.
const OVERLOAD_RETRIES: u32 = 3;
/// Retryable HTTP statuses (429, 5xx) retried before the turn fails.
const TRANSIENT_RETRIES: u32 = 3;

/// Where the bearer comes from and how a rejected one is renewed: the OAuth token store in
/// production, a counter in tests. `refresh` yields the new `(access, account)` or the error the
/// turn fails with.
#[allow(async_fn_in_trait)]
pub(crate) trait CodexAuth {
    async fn bearer(&self) -> Result<(String, Option<String>)>;
    async fn refresh(&self) -> Result<(String, Option<String>)>;
}

/// The production token source: `provider-tokens/codex.json` through [`oauth_codex`].
struct TokenStore;

impl CodexAuth for TokenStore {
    async fn bearer(&self) -> Result<(String, Option<String>)> {
        oauth_codex::bearer_token().await
    }

    async fn refresh(&self) -> Result<(String, Option<String>)> {
        let Some(set) = oauth_codex::load_token() else {
            bail!("Codex HTTP 401 — run `aizen auth login codex`");
        };
        match oauth_codex::refresh_token(&set).await {
            oauth_codex::RefreshOutcome::Ok(s) => Ok((s.access_token, s.account_id)),
            oauth_codex::RefreshOutcome::ReauthRequired(m) => {
                bail!("Codex auth failed (re-login): {m}")
            }
            oauth_codex::RefreshOutcome::Transient(m) => bail!("Codex auth failed: {m}"),
        }
    }
}

/// What a streamed turn does with its frames as they arrive. The chat path paints text as it
/// streams and offers each completed call to the eager starter, exactly like the
/// chat-completions path; the non-streaming callers (chores, the hosted bot) want the finished
/// turn only and print it themselves.
#[derive(Clone, Copy, Default)]
pub struct StreamSink<'a> {
    /// Paint text deltas into the transcript as they arrive.
    pub render: bool,
    /// Offered each tool call the moment its arguments close.
    pub eager: Option<EagerStartFn<'a>>,
}

/// The two-phase stall deadline of one streamed turn — the same `AIZEN_STREAM_FIRST_FRAME_SECS`
/// / `AIZEN_STREAM_STALL_SECS` pair the chat-completions path runs on. Injectable so a test
/// measures the watchdog in milliseconds.
#[derive(Clone, Copy)]
pub(crate) struct StreamCaps {
    pub first_frame: Duration,
    pub idle: Duration,
}

impl StreamCaps {
    fn live() -> Self {
        StreamCaps {
            first_frame: client::stream_first_frame_timeout(),
            idle: client::stream_stall_timeout(),
        }
    }
}

/// One Codex Responses turn → ChatTurn (tools + text). The SSE is consumed as it arrives on the
/// shared watchdog: text paints live, completed calls start eagerly, a stall ends the turn
/// instead of parking it on the 300 s read timeout. This path used to buffer the whole body
/// first, so a Codex turn showed nothing until it had finished (quality plan C4).
pub async fn stream_turn(
    client: &reqwest::Client,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    sink: StreamSink<'_>,
) -> Result<ChatTurn> {
    if oauth_codex::codex_disabled() {
        bail!("Codex OAuth disabled via AIZEN_DISABLE_CODEX");
    }
    stream_turn_at(
        client,
        CODEX_RESPONSES_URL,
        &TokenStore,
        model,
        messages,
        tools,
        session_id,
        sink,
        StreamCaps::live(),
    )
    .await
}

/// [`stream_turn`] against an explicit URL, token source and watchdog — the seam the tests use.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_turn_at(
    client: &reqwest::Client,
    url: &str,
    auth: &impl CodexAuth,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    sink: StreamSink<'_>,
    caps: StreamCaps,
) -> Result<ChatTurn> {
    let mut access_account = auth.bearer().await?;
    let body = build_request_body(model, messages, tools, session_id, None);

    // Request-layer retries mirror `send_with_retry`: jittered backoff on 429/5xx, ONE token
    // refresh on 401. Stream-layer retries mirror the chat-completions path: a stream that died
    // before producing anything is replayed a bounded number of times; one that produced
    // anything is never re-sent (that would duplicate text on screen or resurrect a call).
    let mut auth_refreshes = 0u32;
    let mut overload_attempt = 0u32;
    let mut transient_attempt = 0u32;
    let mut blank_attempt = 0u32;
    loop {
        let (access, account) = &access_account;
        let headers = build_headers(access, account.as_deref(), session_id);
        let resp = client
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .context("codex responses POST")?;

        let status = resp.status();
        if status.as_u16() == 401 {
            if auth_refreshes >= AUTH_REFRESHES {
                bail!("Codex HTTP 401 after a token refresh — run `aizen auth login codex`");
            }
            auth_refreshes += 1;
            access_account = auth.refresh().await?;
            continue;
        }
        if !status.is_success() {
            let code = status.as_u16();
            let text = resp.text().await.unwrap_or_default();
            // A spent QUOTA is permanent for this window — retrying burns nothing but time.
            if code == 429 {
                if let Some(msg) = parse_usage_limit(&text) {
                    bail!("{msg}");
                }
            }
            match classify_envelope(&body_envelope(&text)) {
                Upstream::Capacity => {
                    bail!("Selected model is at capacity. Try a different Codex model.")
                }
                Upstream::Overloaded if overload_attempt < OVERLOAD_RETRIES => {
                    let delay = client::backoff_ms(overload_attempt, 1_000, 15_000);
                    overload_attempt += 1;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                Upstream::Overloaded => bail!("Codex upstream overloaded — retries exhausted"),
                Upstream::Other => {}
            }
            if client::is_retryable_status(code) && transient_attempt < TRANSIENT_RETRIES {
                let delay = client::backoff_ms(transient_attempt, 500, 15_000);
                transient_attempt += 1;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            if code == 429 {
                bail!("Codex rate limited (HTTP 429): {}", truncate(&text, 300));
            }
            bail!("Codex HTTP {status}: {}", truncate(&text, 500));
        }

        let (acc, run) = consume_stream(resp, sink, caps).await;
        let StreamRun {
            err,
            produced,
            eager,
        } = run;
        let blank = !produced && acc.finish.is_none();

        // Transport error or stall. Blank ⇒ replay (bounded); otherwise salvage the completed
        // calls, or fail the turn — partial text alone is never returned as a finished answer.
        if let Some(e) = err {
            if blank && blank_attempt < client::STREAM_BLANK_RETRIES {
                blank_attempt += 1;
                let delay = client::backoff_ms(blank_attempt - 1, 400, 4_000);
                client::stream_retry_note(
                    &e.to_string(),
                    blank_attempt,
                    client::STREAM_BLANK_RETRIES,
                    delay,
                );
                abort_all(eager);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            let calls = acc.completed_calls();
            if calls.is_empty() {
                abort_all(eager);
                return Err(e);
            }
            let note = format!(
                "⟳ stream dropped mid-turn — continuing with {} completed tool call(s)",
                calls.len()
            );
            if crate::ui::tui::active() {
                crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
            } else {
                eprintln!("{note}");
            }
            let eager = eager_by_position(&calls, eager);
            return Ok(ChatTurn {
                content: acc.content(),
                tool_calls: calls,
                finish_reason: Some("tool_calls".into()),
                usage: acc.usage.clone(),
                eager,
            });
        }

        // The stream carried an error envelope and never completed. Classified on the ENVELOPE:
        // the old check lowercased the whole body, so an answer that merely quoted
        // `server_is_overloaded` was discarded and re-billed (quality plan C6).
        if let Some(env) = acc.failure() {
            abort_all(eager);
            match classify_envelope(env) {
                Upstream::Capacity => {
                    bail!("Selected model is at capacity. Try a different Codex model.")
                }
                Upstream::Overloaded if !produced && overload_attempt < OVERLOAD_RETRIES => {
                    let delay = client::backoff_ms(overload_attempt, 1_000, 15_000);
                    overload_attempt += 1;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                Upstream::Overloaded if !produced => {
                    bail!("Codex upstream overloaded — retries exhausted")
                }
                _ => bail!("Codex error: {}", env.message),
            }
        }

        let mut turn = acc.into_turn();
        // Codex turns used to be invisible to `/cost` and the cache HUD — the one chat path that
        // never recorded usage. Same meter as the OpenAI-dialect paths.
        if let Some(u) = &turn.usage {
            client::cost_meter().record(u);
        }
        turn.eager = eager_by_position(&turn.tool_calls, eager);
        return Ok(turn);
    }
}

/// What the live read of one accepted response came to, beyond the accumulator's state.
struct StreamRun {
    /// The transport error or stall that ended the stream early, if any.
    err: Option<anyhow::Error>,
    /// Text, a completed call or a usage report reached the caller: a replay would duplicate it.
    produced: bool,
    /// Eager handles by call id.
    eager: BTreeMap<String, tokio::task::JoinHandle<String>>,
}

/// Read one accepted response frame by frame on the two-phase watchdog, painting text and
/// starting eager calls as `sink` asks. Never fails: the accumulator keeps what arrived and
/// `StreamRun.err` says how the read ended, so the caller can salvage or replay.
async fn consume_stream(
    resp: reqwest::Response,
    sink: StreamSink<'_>,
    caps: StreamCaps,
) -> (SseAccumulator, StreamRun) {
    let mut stream = resp.bytes_stream().eventsource();
    let mut acc = SseAccumulator::default();
    let mut run = StreamRun {
        err: None,
        produced: false,
        eager: BTreeMap::new(),
    };
    // Spinner during the "thinking" gap, TTY-only; the retained TUI shows its own indicator.
    let mut spin = if sink.render && !crate::ui::tui::active() {
        Some(crate::ui::spinner::Spinner::start("thinking"))
    } else {
        None
    };
    let retained = sink.render && crate::ui::tui::retained_active();
    let decorate = sink.render
        && !retained
        && (crate::ui::tui::active() || std::io::IsTerminal::is_terminal(&std::io::stdout()));
    let mut md = crate::ui::markdown::MarkdownStream::new(decorate, crate::ui::tui::width());
    let mut painted = false;
    // Until the first frame PARSES the generous first-frame deadline applies; after that the
    // inter-frame cap. Keepalive noise re-arms neither.
    let mut seen_frame = false;
    let mut last_useful = Instant::now();
    let mut slot = 0usize;
    loop {
        let cap = if seen_frame {
            caps.idle
        } else {
            caps.first_frame
        };
        let remaining = cap.saturating_sub(last_useful.elapsed());
        let event = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(e))) => e,
            Ok(Some(Err(e))) => {
                run.err = Some(anyhow!("SSE stream error: {e}"));
                break;
            }
            Ok(None) => break,
            Err(_) => {
                run.err = Some(anyhow!(
                    "SSE stream error: timeout — no data for {}s{}",
                    cap.as_secs(),
                    if run.produced {
                        " (stream stalled mid-response)"
                    } else {
                        " (stream never started)"
                    }
                ));
                break;
            }
        };
        let data = event.data.trim();
        if data == "[DONE]" {
            break;
        }
        if data.is_empty() {
            continue;
        }
        let fx = acc.ingest(data);
        if !fx.useful {
            continue;
        }
        last_useful = Instant::now();
        seen_frame = true;
        if fx.thinking {
            spin.take(); // the model IS producing, just nothing to show yet
        }
        if !fx.text.is_empty() {
            spin.take();
            run.produced = true;
            if sink.render {
                painted = true;
                crate::ui::tui::add_stream_chars(fx.text.chars().count() as u64);
                if retained {
                    crate::ui::tui::assistant_stream_delta(&fx.text);
                } else {
                    let rendered = md.push(&fx.text);
                    if !rendered.is_empty() {
                        crate::ui::tui::emit(&rendered);
                    }
                }
            }
        }
        if !fx.completed.is_empty() {
            spin.take();
            run.produced = true;
            if let Some(hook) = sink.eager {
                for tc in &fx.completed {
                    if let Some(h) = hook(slot, tc) {
                        run.eager.insert(tc.id.clone(), h);
                    }
                    slot += 1;
                }
            }
        }
        if fx.usage {
            run.produced = true;
        }
    }
    spin.take();
    if sink.render {
        if retained {
            crate::ui::tui::assistant_stream_finish(run.err.is_some());
        } else {
            let closing = md.finish();
            if !closing.is_empty() {
                crate::ui::tui::emit(&closing);
            }
            if painted {
                crate::ui::tui::emit("\n");
            }
        }
    }
    (acc, run)
}

/// Stop the eager work of a turn that no longer exists. Read-only by policy, so cutting it short
/// is safe; aborting rather than detaching stops it billing for an abandoned attempt.
fn abort_all(eager: BTreeMap<String, tokio::task::JoinHandle<String>>) {
    for h in eager.into_values() {
        h.abort();
    }
}

/// Map eager handles from call id to the POSITION in the returned `tool_calls` (what the
/// executor stitches by). A handle whose call did not make the turn is aborted.
fn eager_by_position(
    calls: &[ToolCall],
    mut eager: BTreeMap<String, tokio::task::JoinHandle<String>>,
) -> Vec<(usize, tokio::task::JoinHandle<String>)> {
    let out = calls
        .iter()
        .enumerate()
        .filter_map(|(pos, tc)| eager.remove(&tc.id).map(|h| (pos, h)))
        .collect();
    abort_all(eager);
    out
}

fn parse_usage_limit(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let err = v.get("error")?;
    if err.get("type").and_then(|t| t.as_str()) != Some("usage_limit_reached") {
        return None;
    }
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("Codex usage limit reached");
    if let Some(ts) = err.get("resets_at").and_then(|x| x.as_i64()) {
        return Some(format!("{msg} (resets_at={ts})"));
    }
    if let Some(s) = err.get("resets_in_seconds").and_then(|x| x.as_u64()) {
        return Some(format!("{msg} (resets in {s}s)"));
    }
    Some(msg.to_string())
}

/// A parsed error envelope: the message and the machine code, which is what the overload and
/// capacity markers are matched against. Model output never reaches this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorEnvelope {
    pub message: String,
    pub code: String,
}

/// What an error envelope says about the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upstream {
    /// The chosen model has no capacity: permanent for this turn, switch models.
    Capacity,
    /// The backend is overloaded: retry with backoff.
    Overloaded,
    Other,
}

/// Classify an error envelope by its code and message — and nothing else. The markers used to
/// be matched against the whole SSE body, model output included, so a turn that discussed
/// `server_is_overloaded` was thrown away and re-billed.
pub(crate) fn classify_envelope(env: &ErrorEnvelope) -> Upstream {
    let hay = format!("{} {}", env.code, env.message).to_ascii_lowercase();
    if CAPACITY_MARKERS.iter().any(|m| hay.contains(m)) {
        Upstream::Capacity
    } else if OVERLOAD_MARKERS.iter().any(|m| hay.contains(m)) {
        Upstream::Overloaded
    } else {
        Upstream::Other
    }
}

/// The error envelope of a non-2xx body: its `error` object when the body is JSON, else the
/// body itself — a gateway's plain-text 503 is all envelope, no model output rides in it.
fn body_envelope(text: &str) -> ErrorEnvelope {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| error_envelope(&v))
        .unwrap_or_else(|| ErrorEnvelope {
            message: text.trim().to_string(),
            code: String::new(),
        })
}

/// The error envelope inside one frame, if it carries one: `error` (an object, or a bare
/// string) at the top level or under `response`, or a top-level `type: "error"` frame.
/// `"error": null` on a healthy `response.completed` is not an envelope.
fn error_envelope(v: &Value) -> Option<ErrorEnvelope> {
    let obj = [v.get("error"), v.pointer("/response/error")]
        .into_iter()
        .flatten()
        .find(|e| e.is_object() || e.is_string())
        .or_else(|| (v.get("type").and_then(|t| t.as_str()) == Some("error")).then_some(v))?;
    if let Some(s) = obj.as_str() {
        return Some(ErrorEnvelope {
            message: s.to_string(),
            code: String::new(),
        });
    }
    let message = obj
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let code = obj
        .get("code")
        .and_then(|c| c.as_str())
        .or_else(|| obj.get("type").and_then(|c| c.as_str()))
        .unwrap_or("")
        .to_string();
    if message.is_empty() && code.is_empty() {
        return None;
    }
    Some(ErrorEnvelope { message, code })
}

/// Codex SSE frames folded into one turn, frame by frame. `ingest` reports what each frame
/// contributed so the live reader can paint it and start calls without re-deriving anything;
/// [`parse_sse_to_chat_turn`] feeds a whole buffered body through the same state.
#[derive(Default)]
pub(crate) struct SseAccumulator {
    text: String,
    /// call_id → (name, arguments), in call-id order (the order the turn has always returned).
    tools: BTreeMap<String, (String, String)>,
    /// Call ids whose arguments have closed: a `done` event named them, or the completed
    /// response listed them. Only these are ever started eagerly or salvaged.
    done: BTreeSet<String>,
    finish: Option<String>,
    usage: Option<Usage>,
    error: Option<ErrorEnvelope>,
}

/// What one frame contributed.
#[derive(Debug, Default)]
pub(crate) struct FrameEffect {
    /// Text that arrived with this frame, to paint now.
    pub text: String,
    /// Calls whose arguments closed with this frame, each reported once.
    pub completed: Vec<ToolCall>,
    /// The frame carried a usage report.
    pub usage: bool,
    /// The frame was reasoning output: the model is working, nothing to show.
    pub thinking: bool,
    /// The frame parsed as a Responses event at all. Keepalive noise re-arms no watchdog.
    pub useful: bool,
}

impl SseAccumulator {
    /// Fold one `data:` payload in and report what it contributed.
    pub(crate) fn ingest(&mut self, data: &str) -> FrameEffect {
        let mut fx = FrameEffect::default();
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return fx,
        };
        fx.useful = true;
        if let Some(env) = error_envelope(&v) {
            self.error = Some(env);
        }
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let text_before = self.text.len();
        let mut closed: Vec<String> = Vec::new();
        match ty {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    self.text.push_str(d);
                } else if let Some(d) = v.pointer("/delta/text").and_then(|d| d.as_str()) {
                    self.text.push_str(d);
                }
            }
            "response.output_text.done" => {
                if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                    if self.text.is_empty() {
                        self.text.push_str(t);
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                fx.thinking = true;
            }
            "response.function_call_arguments.delta" => {
                let id = v
                    .get("call_id")
                    .or_else(|| v.get("item_id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("call")
                    .to_string();
                let name = v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let delta = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
                let e = self
                    .tools
                    .entry(id)
                    .or_insert_with(|| (name.clone(), String::new()));
                if e.0.is_empty() && !name.is_empty() {
                    e.0 = name;
                }
                e.1.push_str(delta);
            }
            "response.function_call_arguments.done" => {
                // Full item may carry name/arguments/call_id
                if let Some(item) = v.get("item") {
                    if let Some(id) = ingest_output_item(item, &mut self.tools, &mut self.text) {
                        closed.push(id);
                    }
                }
                let id = v
                    .get("call_id")
                    .or_else(|| v.get("item_id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("call")
                    .to_string();
                let name = v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = v
                    .get("arguments")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !args.is_empty() || !name.is_empty() {
                    let e = self
                        .tools
                        .entry(id.clone())
                        .or_insert_with(|| (name.clone(), String::new()));
                    if e.0.is_empty() {
                        e.0 = name;
                    }
                    if e.1.is_empty() {
                        e.1 = args;
                    }
                }
                closed.push(id);
            }
            "response.output_item.done" => {
                if let Some(item) = v.get("item") {
                    if let Some(id) = ingest_output_item(item, &mut self.tools, &mut self.text) {
                        closed.push(id);
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = v.get("item") {
                    ingest_output_item(item, &mut self.tools, &mut self.text);
                }
            }
            "response.completed" => {
                self.finish = Some("stop".into());
                if let Some(u) = v.pointer("/response/usage") {
                    self.usage = parse_usage(u);
                    fx.usage = true;
                } else if let Some(u) = v.get("usage") {
                    self.usage = parse_usage(u);
                    fx.usage = true;
                }
                if let Some(arr) = v.pointer("/response/output").and_then(|o| o.as_array()) {
                    for item in arr {
                        ingest_output_item(item, &mut self.tools, &mut self.text);
                    }
                }
                // Everything the completed response lists is final.
                closed.extend(self.tools.keys().cloned());
            }
            "response.failed" | "error" => {
                if self.error.is_none() {
                    self.error = Some(ErrorEnvelope {
                        message: format!("codex {ty}"),
                        code: ty.to_string(),
                    });
                }
            }
            _ => {
                // Some gateways put output text under response.output without type prefixing every delta.
                if let Some(arr) = v.pointer("/response/output").and_then(|o| o.as_array()) {
                    for item in arr {
                        ingest_output_item(item, &mut self.tools, &mut self.text);
                    }
                }
            }
        }
        fx.text = self.text[text_before..].to_string();
        for id in closed {
            if !self.done.insert(id.clone()) {
                continue;
            }
            if let Some((name, args)) = self.tools.get(&id) {
                if !name.is_empty() {
                    fx.completed.push(tool_call(&id, name, args));
                }
            }
        }
        fx
    }

    /// The error envelope of a stream that failed and never completed. A failure event fails
    /// the turn unless the stream ALSO completed normally afterwards: an error the turn
    /// recovered from is still fine, while a stream cut down mid-answer must not come back as
    /// a clean `stop` turn the caller cannot tell from a finished one.
    fn failure(&self) -> Option<&ErrorEnvelope> {
        if self.finish.is_none() {
            self.error.as_ref()
        } else {
            None
        }
    }

    fn content(&self) -> Option<String> {
        if self.text.is_empty() {
            None
        } else {
            Some(self.text.clone())
        }
    }

    /// The calls whose arguments closed — what a dropped stream can safely hand the executor.
    fn completed_calls(&self) -> Vec<ToolCall> {
        self.tools
            .iter()
            .filter(|(id, (name, _))| !name.is_empty() && self.done.contains(*id))
            .map(|(id, (name, args))| tool_call(id, name, args))
            .collect()
    }

    /// The turn as it stands. Nameless entries (argument deltas that never met their item) are
    /// dropped: they cannot be executed.
    pub(crate) fn into_turn(self) -> ChatTurn {
        let content = self.content();
        let tool_calls: Vec<ToolCall> = self
            .tools
            .iter()
            .filter(|(_, (name, _))| !name.is_empty())
            .map(|(id, (name, args))| tool_call(id, name, args))
            .collect();
        let finish_reason = if tool_calls.is_empty() {
            self.finish
        } else {
            Some("tool_calls".into())
        };
        ChatTurn {
            content,
            tool_calls,
            finish_reason,
            usage: self.usage,
            eager: Default::default(),
        }
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        kind: "function".into(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: if args.is_empty() {
                "{}".into()
            } else {
                args.to_string()
            },
        },
    }
}

/// Parse a whole buffered Codex Responses SSE body into a ChatTurn — the same fold the live
/// reader applies frame by frame. The live path never buffers; this is the tests' seam.
#[cfg(test)]
pub(crate) fn parse_sse_to_chat_turn(sse: &str) -> Result<ChatTurn> {
    let mut acc = SseAccumulator::default();
    for raw_line in sse.lines() {
        let Some(data) = raw_line.trim_end().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        acc.ingest(data);
    }
    if let Some(env) = acc.failure() {
        bail!("Codex error: {}", env.message);
    }
    Ok(acc.into_turn())
}

/// Fold one output item (`message` or `function_call`) into the state. Returns the call id of a
/// function call so the caller can mark it closed.
fn ingest_output_item(
    item: &Value,
    tools: &mut BTreeMap<String, (String, String)>,
    text_out: &mut String,
) -> Option<String> {
    let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "message" => {
            if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                for part in content {
                    let pt = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if pt == "output_text" || pt == "text" {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if text_out.is_empty() {
                                text_out.push_str(t);
                            }
                        }
                    }
                }
            }
            None
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|x| x.as_str())
                .unwrap_or("call")
                .to_string();
            let name = item
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let mut args = item
                .get("arguments")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            // Argument deltas are keyed by the ITEM id when the event carries no call id; fold
            // that entry into the call-id one so the turn returns one call, not a nameless twin.
            if let Some(item_id) = item.get("id").and_then(|x| x.as_str()) {
                if item_id != call_id {
                    if let Some((_, streamed)) = tools.remove(item_id) {
                        if args.is_empty() {
                            args = streamed;
                        }
                    }
                }
            }
            let e = tools
                .entry(call_id.clone())
                .or_insert_with(|| (name.clone(), String::new()));
            if e.0.is_empty() {
                e.0 = name;
            }
            if e.1.is_empty() {
                e.1 = args;
            }
            Some(call_id)
        }
        _ => None,
    }
}

fn parse_usage(u: &Value) -> Option<Usage> {
    let mut usage = Usage::default();
    usage.prompt_tokens = u
        .get("input_tokens")
        .or_else(|| u.get("prompt_tokens"))
        .and_then(|x| x.as_u64());
    usage.completion_tokens = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"))
        .and_then(|x| x.as_u64());
    usage.total_tokens = u.get("total_tokens").and_then(|x| x.as_u64());
    // The Responses dialect reports cached prompt tokens under `input_tokens_details`; keep the
    // flat spelling too for gateways that mirror the Anthropic shape.
    usage.cache_read_input_tokens = u
        .get("cache_read_input_tokens")
        .and_then(|x| x.as_u64())
        .or_else(|| {
            u.pointer("/input_tokens_details/cached_tokens")
                .and_then(|x| x.as_u64())
        });
    Some(usage)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionDef, ToolDef};

    #[test]
    fn build_body_maps_tools_and_strips_disallowed() {
        let msgs = vec![Message::user("hi")];
        let tools = vec![ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
            cache_control: None,
        }];
        let body = build_request_body("gpt-5.4-mini-high", &msgs, &tools, "sess", None);
        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body["tools"].as_array().unwrap().len() == 1);
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["prompt_cache_key"], "sess");
    }

    #[test]
    fn parse_text_delta_sse() {
        let sse = "\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n\
";
        let turn = parse_sse_to_chat_turn(sse).unwrap();
        assert_eq!(turn.content.as_deref(), Some("Hello"));
        assert!(turn.tool_calls.is_empty());
        assert_eq!(turn.usage.unwrap().prompt_tokens, Some(3));
    }

    #[test]
    fn parse_function_call_sse() {
        let sse = r#"
data: {"type":"response.function_call_arguments.delta","call_id":"c1","name":"shell","delta":"{\"cmd\":"}
data: {"type":"response.function_call_arguments.delta","call_id":"c1","delta":"\"ls\"}"}
data: {"type":"response.function_call_arguments.done","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}
data: {"type":"response.completed","response":{"output":[]}}
"#;
        let turn = parse_sse_to_chat_turn(sse).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.name, "shell");
        assert!(turn.tool_calls[0].function.arguments.contains("ls"));
    }

    #[test]
    fn system_becomes_instructions() {
        let msgs = vec![Message::system("be brief"), Message::user("hi")];
        let body = build_request_body("gpt-5.4", &msgs, &[], "s", None);
        assert_eq!(body["instructions"], "be brief");
        let input = body["input"].as_array().unwrap();
        assert!(input
            .iter()
            .all(|i| i.get("role").and_then(|r| r.as_str()) != Some("system")));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Read one HTTP request off the socket: the headers, then `Content-Length` bytes of body.
    async fn read_request(sock: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let want = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + want {
                    return;
                }
            }
        }
    }

    /// A stub Codex backend answering each request with the next scripted `(status, body)` (the
    /// last one repeats), counting requests.
    async fn stub_backend(replies: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                read_request(&mut sock).await;
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let (status, body) = replies[n.min(replies.len() - 1)];
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (url, hits)
    }

    /// A stub that answers 200, writes `first` (possibly nothing), then holds the socket open for
    /// `hold` without another byte — the stall the watchdog exists for.
    async fn stalling_backend(first: &'static str, hold: Duration) -> (String, Arc<AtomicUsize>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    read_request(&mut sock).await;
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(first.as_bytes()).await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(hold).await;
                    drop(sock);
                });
            }
        });
        (url, hits)
    }

    #[derive(Default)]
    struct CountingAuth {
        refreshes: AtomicUsize,
    }

    impl CodexAuth for CountingAuth {
        async fn bearer(&self) -> Result<(String, Option<String>)> {
            Ok(("tok-0".into(), None))
        }

        async fn refresh(&self) -> Result<(String, Option<String>)> {
            let n = self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok((format!("tok-{}", n + 1), None))
        }
    }

    fn err_text(r: Result<ChatTurn>) -> String {
        match r {
            Err(e) => e.to_string(),
            Ok(t) => panic!("expected an error, got a turn with content {:?}", t.content),
        }
    }

    fn fast_caps() -> StreamCaps {
        StreamCaps {
            first_frame: Duration::from_millis(150),
            idle: Duration::from_millis(150),
        }
    }

    async fn run(url: &str, auth: &CountingAuth, sink: StreamSink<'_>) -> Result<ChatTurn> {
        let client = reqwest::Client::new();
        stream_turn_at(
            &client,
            url,
            auth,
            "gpt-5.4-mini",
            &[Message::user("hi")],
            &[],
            "sess",
            sink,
            fast_caps(),
        )
        .await
    }

    const SSE_QUOTES_MARKER: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"the docs say server_is_overloaded means retry\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"error\":null,\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";

    const SSE_OVERLOAD_ENVELOPE: &str = "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"Please retry\"}}}\n\n";

    /// The official Responses shape: argument deltas keyed by the ITEM id, the call id only on
    /// the item itself, and the completed response listing the call again.
    const SSE_ONE_CALL: &str = "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"\"}}\n\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"cmd\\\":\"}\n\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"\\\"ls\\\"}\"}\n\n\
data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}\n\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"error\":null,\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}]}}\n\n";

    const SSE_FIRST_DELTA: &str =
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n";

    /// C5: the 401 branch refreshed and re-sent with no counter, so a refreshed token the backend
    /// still rejected looped forever. One refresh, then the turn asks for a re-login.
    #[tokio::test]
    async fn a_rejected_refreshed_token_asks_for_login_instead_of_looping() {
        let (url, hits) =
            stub_backend(vec![(401, "{\"error\":{\"message\":\"token expired\"}}")]).await;
        let auth = CountingAuth::default();
        let err = err_text(run(&url, &auth, StreamSink::default()).await);
        assert!(err.contains("aizen auth login codex"), "{err}");
        assert_eq!(
            auth.refreshes.load(Ordering::SeqCst),
            1,
            "one refresh, then stop"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the refreshed token is tried once"
        );
    }

    /// C6: the overload markers were matched against the whole body, model output included, so
    /// an answer that quoted `server_is_overloaded` was discarded and re-billed.
    #[tokio::test]
    async fn an_answer_that_quotes_the_overload_marker_is_not_retried() {
        let (url, hits) = stub_backend(vec![(200, SSE_QUOTES_MARKER)]).await;
        let auth = CountingAuth::default();
        let turn = run(&url, &auth, StreamSink::default()).await.unwrap();
        assert!(turn
            .content
            .unwrap_or_default()
            .contains("server_is_overloaded"));
        assert_eq!(turn.usage.unwrap().prompt_tokens, Some(3));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a finished answer is never re-sent"
        );
    }

    /// The same marker on the ERROR ENVELOPE of a failed stream is an overload: retried with
    /// backoff, then failed as such.
    #[tokio::test]
    async fn an_overload_envelope_is_retried_then_fails() {
        let (url, hits) = stub_backend(vec![(200, SSE_OVERLOAD_ENVELOPE)]).await;
        let auth = CountingAuth::default();
        let err = err_text(run(&url, &auth, StreamSink::default()).await);
        assert!(err.contains("overloaded — retries exhausted"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            (OVERLOAD_RETRIES + 1) as usize,
            "the first try plus every retry"
        );
    }

    /// C4: the stream is read frame by frame on the shared watchdog. A backend that sends one
    /// delta and then goes quiet ends the turn at the idle cap — not at the 300 s read timeout —
    /// and, because text already arrived, the turn is not replayed.
    #[tokio::test]
    async fn a_stalled_stream_fails_on_the_watchdog_not_the_read_timeout() {
        let (url, hits) = stalling_backend(SSE_FIRST_DELTA, Duration::from_secs(3)).await;
        let auth = CountingAuth::default();
        let started = Instant::now();
        let err = err_text(run(&url, &auth, StreamSink::default()).await);
        assert!(err.contains("stream stalled mid-response"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the watchdog fired, not the socket timeout: {:?}",
            started.elapsed()
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "partial output is never re-sent"
        );
    }

    /// A stream that dies before producing ANYTHING is replayed a bounded number of times, like
    /// the chat-completions path — the manual "it froze, I retried, it was fine" case.
    #[tokio::test]
    async fn a_stream_that_dies_blank_is_replayed_then_fails() {
        let (url, hits) = stalling_backend("", Duration::from_secs(3)).await;
        let auth = CountingAuth::default();
        let err = err_text(run(&url, &auth, StreamSink::default()).await);
        assert!(err.contains("stream never started"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            (client::STREAM_BLANK_RETRIES + 1) as usize
        );
    }

    /// A completed call is offered to the eager starter the moment its arguments close — once,
    /// even though the item's `done` and the completed response both list it — and the handle
    /// comes back keyed by the call's position in the turn.
    #[tokio::test]
    async fn completed_calls_start_eagerly_and_ride_by_position() {
        let (url, _hits) = stub_backend(vec![(200, SSE_ONE_CALL)]).await;
        let auth = CountingAuth::default();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let hook = move |slot: usize, tc: &ToolCall| -> Option<tokio::task::JoinHandle<String>> {
            seen2
                .lock()
                .unwrap()
                .push(format!("{slot}:{}:{}", tc.id, tc.function.name));
            Some(tokio::spawn(async { "ran".to_string() }))
        };
        let sink = StreamSink {
            render: false,
            eager: Some(&hook),
        };
        let mut turn = run(&url, &auth, sink).await.unwrap();
        assert_eq!(
            turn.tool_calls.len(),
            1,
            "one call, not a nameless twin: {:?}",
            turn.tool_calls
        );
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].function.name, "shell");
        assert!(turn.tool_calls[0].function.arguments.contains("ls"));
        assert_eq!(*seen.lock().unwrap(), vec!["0:call_1:shell".to_string()]);
        assert_eq!(turn.eager.len(), 1);
        let (pos, handle) = turn.eager.remove(0);
        assert_eq!(pos, 0);
        assert_eq!(handle.await.unwrap(), "ran");
    }

    #[test]
    fn frame_effects_report_text_once_and_a_call_once() {
        let mut acc = SseAccumulator::default();
        let fx = acc.ingest(r#"{"type":"response.output_text.delta","delta":"Hel"}"#);
        assert!(fx.useful);
        assert_eq!(fx.text, "Hel");
        let fx = acc.ingest(r#"{"type":"response.output_text.done","text":"Hello"}"#);
        assert_eq!(fx.text, "", "the done frame repeats what already streamed");
        let fx = acc.ingest(
            r#"{"type":"response.function_call_arguments.done","call_id":"c1","name":"shell","arguments":"{}"}"#,
        );
        assert_eq!(fx.completed.len(), 1);
        let fx = acc.ingest(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"c1","call_id":"c1","name":"shell","arguments":"{}"}}"#,
        );
        assert!(fx.completed.is_empty(), "a call closes once");
        let fx = acc.ingest(": keepalive");
        assert!(!fx.useful, "noise re-arms nothing");
        let fx = acc.ingest(
            r#"{"type":"response.completed","response":{"error":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        );
        assert!(fx.usage && fx.completed.is_empty());
        assert!(acc.failure().is_none());
        let turn = acc.into_turn();
        assert_eq!(turn.content.as_deref(), Some("Hel"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn markers_match_the_envelope_not_the_output() {
        let env =
            body_envelope(r#"{"error":{"code":"server_is_overloaded","message":"try later"}}"#);
        assert_eq!(classify_envelope(&env), Upstream::Overloaded);
        let env = body_envelope("<html>503 service_unavailable_error</html>");
        assert_eq!(
            classify_envelope(&env),
            Upstream::Overloaded,
            "a plain-text 5xx body is all envelope"
        );
        let env = body_envelope(r#"{"error":{"message":"The selected model is at capacity"}}"#);
        assert_eq!(classify_envelope(&env), Upstream::Capacity);
        let env = body_envelope(r#"{"error":{"message":"bad request"}}"#);
        assert_eq!(classify_envelope(&env), Upstream::Other);
        // A healthy completed frame carries `error: null` — not an envelope.
        let v: Value =
            serde_json::from_str(r#"{"type":"response.completed","response":{"error":null}}"#)
                .unwrap();
        assert!(error_envelope(&v).is_none());
        // Output text is never consulted.
        let mut acc = SseAccumulator::default();
        acc.ingest(r#"{"type":"response.output_text.delta","delta":"server_is_overloaded"}"#);
        acc.ingest(r#"{"type":"response.completed","response":{"error":null}}"#);
        assert!(acc.failure().is_none());
        // A failure that the stream did not recover from is reported with its message.
        let mut acc = SseAccumulator::default();
        acc.ingest(r#"{"type":"response.failed","response":{"error":{"code":"rate_limit","message":"slow down"}}}"#);
        assert_eq!(acc.failure().map(|e| e.message.as_str()), Some("slow down"));
    }
}
