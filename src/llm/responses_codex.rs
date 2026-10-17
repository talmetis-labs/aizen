//! ChatGPT Codex Responses API client (experimental).
//!
//! Maps Aizen's chat/tools types onto `POST …/codex/responses` SSE and back into [`ChatTurn`].
//! Not the OpenAI Platform Responses API — field allowlists and headers follow the Codex CLI
//! compatibility surface and may change without notice.

use anyhow::{anyhow, bail, Result};
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
#[cfg(test)]
pub fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    instructions: Option<&str>,
) -> Value {
    build_request_body_with_policy(
        model,
        messages,
        tools,
        session_id,
        instructions,
        crate::llm::media::VisionPolicy::Unknown,
    )
}

#[cfg(test)]
pub fn build_request_body_with_policy(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    instructions: Option<&str>,
    policy: crate::llm::media::VisionPolicy,
) -> Value {
    build_request_body_projected(
        model,
        messages,
        tools,
        session_id,
        instructions,
        policy,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments, clippy::option_option)]
fn build_request_body_projected(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    instructions: Option<&str>,
    policy: crate::llm::media::VisionPolicy,
    effort_override: Option<Option<String>>,
    destination: Option<&str>,
) -> Value {
    let (projected, receipt) = crate::llm::media::project_messages_for_send(
        messages,
        policy,
        destination,
        crate::llm::media::request_send_policy(),
    );
    crate::llm::media::stage_image_delivery(receipt);
    let messages = &projected.messages;
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

    let effort = effort_override.unwrap_or_else(|| {
        suffix_effort.map(|s| s.to_string()).or_else(|| {
            crate::core::cli_config::resolved_reasoning_effort(
                crate::core::cli_config::load().reasoning_effort.clone(),
            )
        })
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
    /// Latest cumulative usage, once when an accepted response ends (including drop/error).
    /// Internal accounting only: must not panic, block, render, or record into CostMeter again.
    pub observed_usage: Option<&'a (dyn Fn(&Usage) + Send + Sync)>,
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
#[expect(
    dead_code,
    reason = "compatibility Codex streaming entry point; production uses explicit-effort dispatch"
)]
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

/// Explicit per-call effort, without consulting another lane's process-global override.
#[expect(
    dead_code,
    reason = "compatibility Codex API; dispatch uses the frozen configuration entry point"
)]
#[allow(clippy::too_many_arguments)]
pub async fn stream_turn_effort(
    client: &reqwest::Client,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    sink: StreamSink<'_>,
    effort: Option<String>,
) -> Result<ChatTurn> {
    let cfg = crate::core::cli_config::load();
    stream_turn_effort_frozen(
        client, model, messages, tools, session_id, sink, effort, &cfg,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_turn_effort_frozen(
    client: &reqwest::Client,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    sink: StreamSink<'_>,
    effort: Option<String>,
    cfg: &crate::core::cli_config::CliConfig,
) -> Result<ChatTurn> {
    if oauth_codex::codex_disabled() {
        bail!("Codex OAuth disabled via AIZEN_DISABLE_CODEX");
    }
    let (base_model, _) = codex_models::strip_effort_suffix(model);
    let policy =
        crate::core::cli_config::vision_support_in(cfg, CODEX_RESPONSES_URL, base_model).into();
    stream_turn_at_projected(
        client,
        CODEX_RESPONSES_URL,
        &TokenStore,
        model,
        messages,
        tools,
        session_id,
        sink,
        StreamCaps::live(),
        policy,
        Some(effort),
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
    let (base_model, _) = codex_models::strip_effort_suffix(model);
    let policy = crate::core::cli_config::vision_support_for(url, base_model).into();
    stream_turn_at_projected(
        client, url, auth, model, messages, tools, session_id, sink, caps, policy, None,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::option_option)]
async fn stream_turn_at_projected(
    client: &reqwest::Client,
    url: &str,
    auth: &impl CodexAuth,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    sink: StreamSink<'_>,
    caps: StreamCaps,
    policy: crate::llm::media::VisionPolicy,
    effort_override: Option<Option<String>>,
) -> Result<ChatTurn> {
    let _request_memory = crate::llm::media::request_memory_lease(messages)?;
    let mut access_account = auth.bearer().await?;
    let (base_model, _) = codex_models::strip_effort_suffix(model);
    let destination = crate::llm::media::destination_key(url, base_model);
    crate::llm::media::validate_reference_vision(policy)?;
    let body = build_request_body_projected(
        model,
        messages,
        tools,
        session_id,
        None,
        policy,
        effort_override,
        Some(&destination),
    );

    let body = serde_json::to_vec(&body)?;
    let send_started = Instant::now();
    let send_budget = Duration::from_secs(600);
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
        let remaining = send_budget.saturating_sub(send_started.elapsed());
        crate::llm::media::validate_image_request()?;
        super::delegated::before_send()?;
        let resp = tokio::time::timeout(
            remaining,
            client
                .post(url)
                .headers(headers)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send(),
        )
        .await
        .map_err(|_| anyhow!("Codex request timeout (stage=send)"))?
        .map_err(|_| anyhow!("Codex request failed (stage=send)"))?;

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
            let correlation = client::response_correlation(&resp);
            let text = client::bounded_error_body(
                resp,
                send_budget.saturating_sub(send_started.elapsed()),
            )
            .await;
            let failure = client::transport_failure("http", Some(code), &text, correlation);
            // A spent QUOTA is permanent for this window — retrying burns nothing but time.
            if code == 429 && parse_usage_limit(&text).is_some() {
                bail!("Codex usage limit reached (HTTP 429): {failure}");
            }
            match classify_envelope(&body_envelope(&text)) {
                Upstream::Capacity => {
                    bail!("Selected model is at capacity. Try a different Codex model.")
                }
                Upstream::Overloaded if overload_attempt < OVERLOAD_RETRIES => {
                    let delay = client::backoff_ms(overload_attempt, 1_000, 15_000);
                    overload_attempt += 1;
                    super::delegated::sleep(Duration::from_millis(delay)).await?;
                    continue;
                }
                Upstream::Overloaded => bail!("Codex upstream overloaded — retries exhausted"),
                Upstream::Other => {}
            }
            if client::is_retryable_status(code) && transient_attempt < TRANSIENT_RETRIES {
                let delay = client::backoff_ms(transient_attempt, 500, 15_000);
                transient_attempt += 1;
                super::delegated::sleep(Duration::from_millis(delay)).await?;
                continue;
            }
            return Err(failure.into());
        }

        let (acc, run) = consume_stream(resp, sink, caps).await;
        let StreamRun {
            err,
            produced,
            eager,
        } = run;
        let blank = !produced && !acc.invalid && acc.finish.is_none();

        // Transport error or stall. Blank ⇒ replay (bounded); otherwise salvage the completed
        // calls, or fail the turn — partial text alone is never returned as a finished answer.
        if let Some(e) = err {
            if blank && blank_attempt < client::STREAM_BLANK_RETRIES {
                blank_attempt += 1;
                let delay = client::backoff_ms(blank_attempt - 1, 400, 4_000);
                if sink.render {
                    client::stream_retry_note(
                        &e.to_string(),
                        blank_attempt,
                        client::STREAM_BLANK_RETRIES,
                        delay,
                    );
                }
                abort_all(eager);
                super::delegated::sleep(Duration::from_millis(delay)).await?;
                continue;
            }
            let calls = acc.completed_calls();
            if calls.is_empty() || acc.failure().is_some() || acc.invalid || acc.finish.is_some() {
                abort_all(eager);
                return Err(e);
            }
            // Tool salvage does not certify successful delivery of this request's images.
            crate::llm::media::stage_image_delivery(Default::default());
            let note = format!(
                "⟳ stream dropped mid-turn — continuing with {} completed tool call(s)",
                calls.len()
            );
            if sink.render {
                if crate::ui::tui::active() {
                    crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
                } else {
                    crate::ui::tui::note_line(&note);
                }
            }
            let eager = eager_by_position(&calls, eager);
            return Ok(ChatTurn {
                content: acc.content(),
                tool_calls: calls,
                finish_reason: Some("tool_calls".into()),
                usage: acc.usage.clone(),
                empty_wire_note: None,
                empty_wire_raw: None,
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
                    super::delegated::sleep(Duration::from_millis(delay)).await?;
                    continue;
                }
                Upstream::Overloaded if !produced => {
                    bail!("Codex upstream overloaded — retries exhausted")
                }
                _ => {
                    let body = serde_json::json!({"error": {"code": env.code}}).to_string();
                    let mut failure = client::transport_failure(
                        "sse",
                        Some(200),
                        &body,
                        env.correlation_id.clone(),
                    );
                    failure.provider_status = env.provider_status;
                    return Err(failure.into());
                }
            }
        }

        let mut turn = acc.into_turn();
        // The stream tail records observed usage even on cancellation/error, once per response.
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
    eager: BTreeMap<String, crate::agent::tools::StartedSafeCall>,
}

/// Read one accepted response frame by frame on the two-phase watchdog, painting text and
/// starting eager calls as `sink` asks. Never fails: the accumulator keeps what arrived and
/// `StreamRun.err` says how the read ended, so the caller can salvage or replay.
async fn consume_stream(
    resp: reqwest::Response,
    sink: StreamSink<'_>,
    caps: StreamCaps,
) -> (SseAccumulator, StreamRun) {
    let correlation = client::response_correlation(&resp);
    let mut stream = client::bounded_sse_bytes(resp.bytes_stream()).eventsource();
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
    let md = crate::ui::markdown::MarkdownStream::new(
        decorate,
        if sink.render {
            crate::ui::tui::width()
        } else {
            80
        },
    );
    let mut tail = client::StreamTail::new(
        client::cost_meter(),
        md,
        crate::ui::tui::emit,
        sink.render,
        retained,
        false,
    )
    .with_usage_observer(sink.observed_usage);
    // Until the first frame PARSES the generous first-frame deadline applies; after that the
    // inter-frame cap. Keepalive noise re-arms neither.
    let mut seen_frame = false;
    let mut last_useful = Instant::now();
    let mut slot = 0usize;
    let mut payload_bytes = 0usize;
    loop {
        let cap = if seen_frame {
            caps.idle
        } else {
            caps.first_frame
        };
        let remaining = cap.saturating_sub(last_useful.elapsed());
        let event = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(e))) => e,
            Ok(Some(Err(error))) => {
                let (failure, terminal) = client::sse_read_failure(error, correlation.clone());
                if terminal {
                    acc.invalid = true;
                    run.produced = true; // malformed/oversized input is never a blank replay.
                }
                run.err = Some(failure.into());
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
        payload_bytes = payload_bytes.saturating_add(event.data.len());
        if payload_bytes > 16 * 1024 * 1024 {
            acc.invalid = true;
            run.err = Some(anyhow!("SSE stream error: response exceeds 16 MiB limit"));
            break;
        }
        if event.event == "error" {
            let failure =
                client::transport_failure("sse", Some(200), &event.data, correlation.clone());
            // This is a provider-declared failure, even if its data is plain text or a
            // misleading [DONE]. It cannot be retried as blank or salvaged as completed tools.
            acc.invalid = true;
            run.produced = true;
            run.err = Some(failure.into());
            break;
        }
        let data = event.data.trim();
        if data == "[DONE]" {
            break;
        }
        if data.is_empty() {
            continue;
        }
        let fx = acc.ingest(data);
        if !acc.tools.is_empty() {
            run.produced = true; // a partial tool fragment must never replay the generation.
        }
        if let Some(error) = acc.error.as_mut() {
            if error.correlation_id.is_none() {
                error.correlation_id = correlation.clone();
            }
        }
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
                crate::ui::tui::add_stream_chars(fx.text.chars().count() as u64);
                tail.push(&fx.text);
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
            if let Some(usage) = &acc.usage {
                tail.observe_usage(usage);
            }
        }
        // Error envelopes are terminal: a later completed event cannot erase a real failure.
        if acc.error.is_some() || acc.invalid {
            break;
        }
    }
    if acc.error.is_none() && run.err.is_none() {
        run.err = acc.validate().err();
    }
    spin.take();
    tail.finish(run.err.is_some());
    (acc, run)
}

/// Abandon observations from a turn that no longer exists. The calls are read-only by policy;
/// aborting their watchers does not prove that a blocking worker or provider call has stopped.
fn abort_all(eager: BTreeMap<String, crate::agent::tools::StartedSafeCall>) {
    for h in eager.into_values() {
        h.abort();
    }
}

/// Map eager handles from call id to the POSITION in the returned `tool_calls` (what the
/// executor stitches by). A handle whose call did not make the turn is aborted.
fn eager_by_position(
    calls: &[ToolCall],
    mut eager: BTreeMap<String, crate::agent::tools::StartedSafeCall>,
) -> Vec<(usize, crate::agent::tools::StartedSafeCall)> {
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
    pub correlation_id: Option<String>,
    pub provider_status: Option<u16>,
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
            correlation_id: None,
            provider_status: None,
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
            correlation_id: None,
            provider_status: None,
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
    let evidence = client::transport_failure("sse", Some(200), &v.to_string(), None);
    Some(ErrorEnvelope {
        message,
        code,
        correlation_id: evidence.correlation_id,
        provider_status: evidence.provider_status,
    })
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
    invalid: bool,
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
    fn validate(&self) -> Result<()> {
        if self.invalid
            || self.tools.iter().any(|(id, (name, args))| {
                name.is_empty() || !self.done.contains(id) || !client::args_complete(args)
            })
        {
            bail!("SSE stream error: incomplete tool arguments");
        }
        if self.finish.as_deref() != Some("stop") {
            bail!("SSE stream error: partial EOF without response.completed");
        }
        Ok(())
    }

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
                        correlation_id: None,
                        provider_status: None,
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
                if !name.is_empty() && client::args_complete(args) {
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
        self.error.as_ref()
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
            .filter(|(id, (name, args))| {
                !name.is_empty() && self.done.contains(*id) && client::args_complete(args)
            })
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
            empty_wire_note: None,
            empty_wire_raw: None,
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
    acc.validate()?;
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
    Some(Usage {
        prompt_tokens: u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(|x| x.as_u64()),
        completion_tokens: u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(|x| x.as_u64()),
        total_tokens: u.get("total_tokens").and_then(|x| x.as_u64()),
        // Responses reports cached prompt tokens under input_tokens_details; retain the flat
        // spelling too for gateways that mirror the Anthropic shape.
        cache_read_input_tokens: u
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_u64())
            .or_else(|| {
                u.pointer("/input_tokens_details/cached_tokens")
                    .and_then(|x| x.as_u64())
            }),
        ..Usage::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionDef, ToolDef};

    #[test]
    fn completion_requires_valid_closed_tools_and_no_error_envelope() {
        for sse in [
            "data: [DONE]\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"code\":\"failed\",\"message\":\"no\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        ] {
            assert!(parse_sse_to_chat_turn(sse).is_err(), "false Done: {sse}");
        }
    }

    #[tokio::test]
    async fn quiet_codex_usage_observer_reports_latest_once_on_success_and_failure() {
        use crate::llm::client::tests::raw_sse_backend;
        const PREFIX: &str = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n";
        for (ending, success) in [
            ("", true),
            ("event: error\ndata: private failure\n\n", false),
        ] {
            let (base, hits, server) =
                raw_sse_backend(format!("{PREFIX}{ending}").into_bytes()).await;
            let url = format!("{base}/codex/responses");
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let auth = CountingAuth::default();
            let dispatch = crate::agent::orchestration::DispatchUsage::default();
            let calls = AtomicUsize::new(0);
            let observer = |usage: &Usage| {
                calls.fetch_add(1, Ordering::SeqCst);
                dispatch.add(usage.input_total(), usage.completion_tokens.unwrap_or(0));
            };
            let sink = StreamSink {
                observed_usage: Some(&observer),
                ..Default::default()
            };
            let caps = StreamCaps {
                first_frame: Duration::from_secs(10),
                idle: Duration::from_secs(10),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(20),
                stream_turn_at(
                    &http,
                    &url,
                    &auth,
                    "gpt-5.4-mini",
                    &[Message::user("hi")],
                    &[],
                    "sess",
                    sink,
                    caps,
                ),
            )
            .await
            .unwrap();
            assert_eq!(result.is_ok(), success);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(dispatch.finish(), (11, 3));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn quiet_codex_partial_eof_and_invalid_arguments_are_not_done() {
        const INCOMPLETE: &str = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        for reply in [SSE_FIRST_DELTA, INCOMPLETE] {
            let (url, hits) = stub_backend(vec![(200, reply)]).await;
            let auth = CountingAuth::default();
            assert!(run(&url, &auth, StreamSink::default()).await.is_err());
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn quiet_codex_error_keeps_correlation_not_echoed_body() {
        let (url, hits) = stub_backend(vec![(200, "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"UPSTREAM_BAD_RESPONSE\",\"status\":0,\"correlation_id\":\"req-codex-1\",\"message\":\"sk-secret data:image/png;base64,PRIVATE\"}}}\n\n")]).await;
        let auth = CountingAuth::default();
        let err = run(&url, &auth, StreamSink::default()).await.err().unwrap();
        let failure = err.downcast_ref::<client::TransportFailure>().unwrap();
        assert_eq!(failure.http_status, Some(200));
        assert_eq!(failure.provider_status, Some(0));
        assert_eq!(failure.correlation_id.as_deref(), Some("req-codex-1"));
        assert!(!err.to_string().contains("sk-secret"));
        assert!(!err.to_string().contains("PRIVATE"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_json_error_event_cannot_be_erased_by_later_completed_response() {
        use crate::llm::media::with_image_delivery;
        for reply in [
            "event: error\ndata: sk-secret data:image/png;base64,PRIVATE\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\nevent: error\ndata: sk-secret data:image/png;base64,PRIVATE\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\nevent: error\ndata: [DONE]\n\n",
        ] {
            let (url, hits) = stub_backend(vec![(200, reply)]).await;
            let auth = CountingAuth::default();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let messages = vec![Message::user_with_images("inspect", vec!["data:legacy".into()])];
            let ctx = crate::core::exec_ctx::ExecutionContext::default();
            let error = with_image_delivery(&ctx, &url, "fixture-key", "gpt-5.4-mini", stream_turn_at(
                &client, &url, &auth, "gpt-5.4-mini", &messages, &[], "sess", StreamSink::default(), fast_caps(),
            )).await.err().unwrap();
            assert!(error.downcast_ref::<client::TransportFailure>().is_some());
            assert!(!error.to_string().contains("sk-secret"));
            assert!(!error.to_string().contains("PRIVATE"));
            assert_eq!(hits.load(Ordering::SeqCst), 1, "declared errors cannot blank-retry");
        }
    }

    #[tokio::test]
    async fn codex_raw_limit_and_malformed_are_not_replayed_or_tool_salvaged() {
        use crate::llm::{
            client::tests::{raw_limit_reply, raw_sse_backend},
            media::with_image_delivery,
        };
        const TOOL: &[u8] = b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n";
        const FINISH: &[u8] =
            b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        for prefix in [b"".as_slice(), TOOL] {
            for limit in [false, true] {
                let reply = if limit {
                    raw_limit_reply(prefix, FINISH)
                } else {
                    [
                        prefix,
                        b"data: \xffsk-secret PRIVATE\n\n".as_slice(),
                        FINISH,
                    ]
                    .concat()
                };
                let (base, hits, server) = raw_sse_backend(reply).await;
                let url = format!("{base}/codex/responses");
                let http = reqwest::Client::builder().no_proxy().build().unwrap();
                let auth = CountingAuth::default();
                let ctx = crate::core::exec_ctx::ExecutionContext::default();
                let messages = [Message::user_with_images(
                    "inspect",
                    vec!["data:legacy".into()],
                )];
                // These fixtures exercise the byte cap, not the 150 ms stall seam.
                let caps = StreamCaps {
                    first_frame: Duration::from_secs(20),
                    idle: Duration::from_secs(20),
                };
                let result = with_image_delivery(
                    &ctx,
                    &url,
                    "fixture-key",
                    "gpt-5.4-mini",
                    stream_turn_at(
                        &http,
                        &url,
                        &auth,
                        "gpt-5.4-mini",
                        &messages,
                        &[],
                        "sess",
                        StreamSink::default(),
                        caps,
                    ),
                );
                let error = tokio::time::timeout(Duration::from_secs(30), result)
                    .await
                    .unwrap()
                    .err()
                    .unwrap();
                let failure = error.downcast_ref::<client::TransportFailure>().unwrap();
                assert_eq!(
                    failure.provider_code.as_deref(),
                    Some(if limit {
                        "SSE_RAW_LIMIT"
                    } else {
                        "SSE_MALFORMED"
                    })
                );
                assert_eq!(failure.correlation_id.as_deref(), Some("req-raw-bound"));
                assert!(!error.to_string().contains("PRIVATE"));
                assert!(!error.to_string().contains("sk-secret"));
                assert_eq!(hits.load(Ordering::SeqCst), 1);
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn salvaged_codex_tools_clear_live_image_receipt() {
        use crate::llm::media::{with_image_delivery, ImageDeliveryReceipt};
        const REPLY: &str = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n";
        let (url, hits) = stub_backend(vec![(200, REPLY)]).await;
        let auth = CountingAuth::default();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let messages = vec![Message::user_with_images(
            "inspect",
            vec!["data:legacy".into()],
        )];
        let ctx = crate::core::exec_ctx::ExecutionContext::default();
        let (turn, receipt) = with_image_delivery(
            &ctx,
            &url,
            "fixture-key",
            "gpt-5.4-mini",
            stream_turn_at(
                &client,
                &url,
                &auth,
                "gpt-5.4-mini",
                &messages,
                &[],
                "sess",
                StreamSink::default(),
                fast_caps(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(receipt, ImageDeliveryReceipt::default());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

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

    #[tokio::test]
    async fn actual_codex_receiver_gets_every_output_before_exact_image_bytes_without_metadata() {
        use crate::llm::media::{destination_key, tests as media_tests, VisionPolicy};
        const REPLY: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"reviewed\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";
        let (base, receiver, server) =
            media_tests::request_receiver(REPLY, "text/event-stream").await;
        let url = format!("{base}/codex/responses");
        let image =
            media_tests::image(43).with_approved_destination(destination_key(&url, "vision-test"));
        let expected = image.bytes().unwrap();
        let messages = media_tests::observed_group(image);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let turn = stream_turn_at_projected(
            &client,
            &url,
            &CountingAuth::default(),
            "vision-test-high",
            &messages,
            &[],
            "test-session",
            StreamSink::default(),
            fast_caps(),
            VisionPolicy::Supported,
            Some(Some("low".into())),
        )
        .await
        .unwrap();
        assert_eq!(turn.content.as_deref(), Some("reviewed"));
        let wire = receiver.await.unwrap();
        server.await.unwrap();
        let input = wire["input"].as_array().unwrap();
        assert_eq!(input.len(), 7);
        for item in input.iter().take(3) {
            assert_eq!(item["type"], "function_call");
        }
        for (position, text) in [(3, "image text"), (4, "other text"), (5, "last text")] {
            assert_eq!(input[position]["type"], "function_call_output");
            assert_eq!(input[position]["output"], text);
        }
        assert_eq!(input[6]["role"], "user");
        let image_url = input[6]["content"][1]["image_url"].as_str().unwrap();
        assert_eq!(media_tests::decode_url(image_url), expected);
        assert_eq!(wire["model"], "vision-test");
        assert_eq!(
            wire["reasoning"]["effort"], "low",
            "explicit per-call effort beats model suffix/global"
        );
        media_tests::assert_no_storage_metadata(&wire);
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
        let hook =
            move |slot: usize, tc: &ToolCall| -> Option<crate::agent::tools::StartedSafeCall> {
                seen2
                    .lock()
                    .unwrap()
                    .push(format!("{slot}:{}:{}", tc.id, tc.function.name));
                Some(crate::agent::tools::StartedSafeCall::start(
                    tc.id.clone(),
                    crate::agent::tools::PreparedToolCall::new(serde_json::json!({})),
                    crate::core::cancel::TurnCancel::new(),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                    None,
                    || crate::agent::tools::ToolOutcome {
                        observation: crate::agent::tools::ToolObservation::text("ran"),
                        failure: None,
                        effect_state: crate::agent::tools::ToolEffectState::Settled,
                    },
                ))
            };
        let sink = StreamSink {
            render: false,
            eager: Some(&hook),
            observed_usage: None,
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
        assert_eq!(handle.await.unwrap().text, "ran");
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
