//! OpenAI-compatible streaming chat client — the "call API like hermes" layer.
//! POST {base_url}/chat/completions with Bearer auth, parse the SSE stream, emit content deltas.

use anyhow::{anyhow, bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Deserialize;

use crate::core::types::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, FunctionCall, Message, StreamOptions,
    ToolCall, ToolCallDelta, ToolDef, Usage, UsageRow,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global REAL token accounting. `client` is the single choke point every model call goes
/// through (REPL, sub-agents, compaction, skill/persona learning), so accumulating here gives an
/// honest session cost without threading `usage` through the agent loop. Only populated when the
/// provider actually reports `usage` (OpenAI-compatible endpoints with `include_usage`); when it
/// never does, `calls_with_usage` stays 0 and callers fall back to the chars/4 estimate.
#[derive(Default)]
pub struct CostMeter {
    prompt_tokens: AtomicU64,
    completion_tokens: AtomicU64,
    /// Model calls that returned a usage object (so the UI knows the numbers are real, not estimated).
    calls_with_usage: AtomicU64,
    /// Prompt-cache input tokens read back this session (the prompt_cache breakpoint payoff). 0 ⇒
    /// caching off / unsupported by the provider — the `/cost` cache line only shows when > 0.
    cache_read_tokens: AtomicU64,
    /// Prompt-cache WRITE tokens this session (Anthropic-style gateways bill these at a premium).
    cache_write_tokens: AtomicU64,
    /// Input tokens that actually went out with each request, cache reads and writes included
    /// (`Usage::input_total`, shape-corrected) — the honest denominator for "N % cached".
    input_tokens: AtomicU64,
    /// The MOST RECENT usage-carrying call (input / cached / completion) — the live cache-hit-rate
    /// signal for the status line. Session totals above answer "what did this cost"; these answer
    /// "is the prompt cache warm RIGHT NOW".
    last_input_tokens: AtomicU64,
    last_cached_tokens: AtomicU64,
    last_completion_tokens: AtomicU64,
    /// The user turn the next recorded call belongs to. Bumped by the REPL at each user turn and
    /// NEVER reset (see [`CostMeter::reset`]): a process-monotonic number keeps a session file's
    /// rows ordered even when the conversation is resumed after another one in the same process.
    turn: AtomicU64,
    /// Sequence number of the last recorded row, process-monotonic and never reset — together with
    /// [`process_epoch`] it is the cursor a session file keeps so a later save appends only the
    /// rows it has not persisted yet.
    seq: AtomicU64,
    /// Per-call rows since the last [`CostMeter::reset`], `(seq, row)`, capped at [`USAGE_ROWS_CAP`].
    rows: std::sync::Mutex<Vec<(u64, UsageRow)>>,
}

/// Rows the in-memory ledger keeps before dropping the oldest. A 165-call turn (the largest seen
/// on a real machine) is 166 rows; 5,000 covers weeks of one conversation at ~50 B per row.
pub const USAGE_ROWS_CAP: usize = 5_000;

static COST_METER: CostMeter = CostMeter {
    prompt_tokens: AtomicU64::new(0),
    completion_tokens: AtomicU64::new(0),
    calls_with_usage: AtomicU64::new(0),
    cache_read_tokens: AtomicU64::new(0),
    cache_write_tokens: AtomicU64::new(0),
    input_tokens: AtomicU64::new(0),
    last_input_tokens: AtomicU64::new(0),
    last_cached_tokens: AtomicU64::new(0),
    last_completion_tokens: AtomicU64::new(0),
    turn: AtomicU64::new(0),
    seq: AtomicU64::new(0),
    rows: std::sync::Mutex::new(Vec::new()),
};

/// The process-global cost meter (real provider-reported tokens this session).
pub fn cost_meter() -> &'static CostMeter {
    &COST_METER
}

/// The observed tail belongs to the future: timeout/drop still bills received usage and closes
/// its renderer. Provider-reported cumulative usage is recorded once, never estimated on drop.
pub(crate) struct StreamTail<'a, F: FnMut(&str)> {
    meter: &'a CostMeter,
    usage: Option<Usage>,
    usage_observer: Option<&'a (dyn Fn(&Usage) + Send + Sync)>,
    markdown: crate::ui::markdown::MarkdownStream,
    emit: F,
    render: bool,
    retained: bool,
    events: bool,
    newline: bool,
    closed: bool,
}

impl<'a, F: FnMut(&str)> StreamTail<'a, F> {
    pub(crate) fn new(
        meter: &'a CostMeter,
        markdown: crate::ui::markdown::MarkdownStream,
        emit: F,
        render: bool,
        retained: bool,
        newline: bool,
    ) -> Self {
        Self {
            meter,
            usage: None,
            usage_observer: None,
            markdown,
            emit,
            render,
            retained,
            events: false,
            newline,
            closed: false,
        }
    }

    pub(crate) fn with_usage_observer(
        mut self,
        observer: Option<&'a (dyn Fn(&Usage) + Send + Sync)>,
    ) -> Self {
        self.usage_observer = observer;
        self
    }

    pub(crate) fn observe_usage(&mut self, usage: &Usage) {
        self.usage = Some(usage.clone());
    }

    pub(crate) fn push(&mut self, text: &str) {
        if !self.render || text.is_empty() {
            return;
        }
        self.newline = true;
        if self.events {
            crate::ui::events::text(text);
        } else if self.retained {
            crate::ui::tui::assistant_stream_delta(text);
        } else {
            let rendered = self.markdown.push(text);
            if !rendered.is_empty() {
                (self.emit)(&rendered);
            }
        }
    }

    pub(crate) fn finish(&mut self, interrupted: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(usage) = self.usage.take() {
            self.meter.record(&usage);
            if let Some(observer) = self.usage_observer {
                observer(&usage);
            }
        }
        if self.render && !self.events {
            if self.retained {
                crate::ui::tui::assistant_stream_finish(interrupted);
            } else {
                let closing = self.markdown.finish();
                if !closing.is_empty() {
                    (self.emit)(&closing);
                }
                if self.newline {
                    (self.emit)("\n");
                }
            }
        }
    }
}

impl<F: FnMut(&str)> Drop for StreamTail<'_, F> {
    fn drop(&mut self) {
        self.finish(true);
    }
}

/// This process's identity for the usage-ledger cursor: nanoseconds at first use. A session file
/// records the epoch it was last written under; a different epoch means "another process wrote
/// this", so every row the live meter holds is new to the file regardless of sequence numbers.
pub fn process_epoch() -> u64 {
    static EPOCH: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            .max(1)
    })
}

/// Does this model name route to an Anthropic model (which honors `cache_control` and rejects
/// `temperature`)? Drives AUTO prompt-cache so non-Anthropic providers get a field-free wire.
///
/// `claude`/`anthropic` match anywhere (always first-party). The bare gateway ids the product uses
/// (`opus-4-8`, `sonnet-4-6`, `haiku-4-5`, `fable-5` — all also exist with a `claude-` prefix) are
/// matched as WHOLE TOKENS so a community model that merely embeds the word (e.g. `fable13b`,
/// `haikuwriter`, the Llama `mythos` merge) is NOT misclassified into sending `cache_control` that
/// many OpenAI-compatible gateways reject with a 400.
pub fn is_anthropic_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("claude") || m.contains("anthropic") {
        return true;
    }
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .any(|k| contains_word(&m, k))
}

/// `haystack` contains `needle` delimited by non-alphanumeric boundaries (so `-`, `_`, `.`, space
/// count as separators but an adjacent letter/digit does not). ASCII-only (model ids are ASCII).
/// Shared with `agent::prompt_tier_for` (the strict-tier model heuristic).
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let i = start + pos;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after = i + needle.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = i + 1;
    }
    false
}

/// Whether to send prompt-cache breakpoints: explicit config wins; `None` ⇒ AUTO (on only for
/// Anthropic-looking models). Off ⇒ the request is byte-identical to before (zero risk).
pub fn cache_enabled(flag: Option<bool>, model: &str) -> bool {
    flag.unwrap_or_else(|| is_anthropic_model(model))
}

/// Stamp ephemeral cache breakpoints (≤4 of Anthropic's 4): the LAST tool def (caches the whole
/// tool block), the first stable system message, an optional second dynamic system message, and the
/// last stable assistant/tool message before the newest turn. The volatile newest user turn stays
/// uncached. Computed per call, never stored, so mid-history `system` nudges don't disturb it.
fn apply_cache_breakpoints(messages: &mut [Message], tools: &mut [ToolDef]) {
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }
    // First system message = stable lane.
    if let Some(first) = messages.first_mut() {
        if first.role == "system" {
            first.cache_control = Some(CacheControl::ephemeral());
        }
    }
    // Second leading system message = dynamic lane (persona/memory/skills/…); stamp when present.
    if messages.len() >= 2 && messages[0].role == "system" && messages[1].role == "system" {
        messages[1].cache_control = Some(CacheControl::ephemeral());
    }
    let n = messages.len();
    if n >= 3 {
        if let Some(idx) = (0..n)
            .rev()
            .find(|&i| matches!(messages[i].role.as_str(), "assistant" | "tool"))
        {
            if idx != 0 && !(idx == 1 && messages[0].role == "system") {
                messages[idx].cache_control = Some(CacheControl::ephemeral());
            }
        }
    }
}

impl CostMeter {
    pub(crate) fn record(&self, u: &Usage) {
        // Only count a call as "real" if it carried at least one token field.
        if u.prompt_tokens.is_none() && u.completion_tokens.is_none() && u.total_tokens.is_none() {
            return;
        }
        self.prompt_tokens
            .fetch_add(u.prompt_tokens.unwrap_or(0), Ordering::Relaxed);
        self.completion_tokens
            .fetch_add(u.completion_tokens.unwrap_or(0), Ordering::Relaxed);
        let input = u.input_total();
        let cached = u.cache_read();
        let cache_write = u.cache_write();
        let output = u.completion_tokens.unwrap_or(0);
        self.cache_read_tokens.fetch_add(cached, Ordering::Relaxed);
        self.cache_write_tokens
            .fetch_add(cache_write, Ordering::Relaxed);
        self.input_tokens.fetch_add(input, Ordering::Relaxed);
        self.calls_with_usage.fetch_add(1, Ordering::Relaxed);
        self.last_input_tokens.store(input, Ordering::Relaxed);
        self.last_cached_tokens.store(cached, Ordering::Relaxed);
        self.last_completion_tokens.store(output, Ordering::Relaxed);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let row = UsageRow {
            turn: self.turn.load(Ordering::Relaxed),
            input,
            output,
            cached,
            cache_write,
        };
        if let Ok(mut rows) = self.rows.lock() {
            rows.push((seq, row));
            if rows.len() > USAGE_ROWS_CAP {
                let excess = rows.len() - USAGE_ROWS_CAP;
                rows.drain(..excess);
            }
        }
    }

    /// Mark the start of a user turn: every call recorded until the next one is attributed to it.
    pub fn begin_turn(&self) {
        self.turn.fetch_add(1, Ordering::Relaxed);
    }

    /// The turn number the next recorded call will carry (0 before the first `begin_turn`).
    pub fn current_turn(&self) -> u64 {
        self.turn.load(Ordering::Relaxed)
    }

    /// `(input, cached, completion)` of the most recent usage-carrying call; `None` before any.
    /// `input` is the shape-corrected total (cache reads and writes included), so `cached * 100 /
    /// input` is a real percentage on every provider.
    pub fn last_call(&self) -> Option<(u64, u64, u64)> {
        if self.calls_with_usage.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some((
            self.last_input_tokens.load(Ordering::Relaxed),
            self.last_cached_tokens.load(Ordering::Relaxed),
            self.last_completion_tokens.load(Ordering::Relaxed),
        ))
    }
    /// `(prompt, completion, calls_with_usage)`. `calls == 0` ⇒ no real usage seen (use the estimate).
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.prompt_tokens.load(Ordering::Relaxed),
            self.completion_tokens.load(Ordering::Relaxed),
            self.calls_with_usage.load(Ordering::Relaxed),
        )
    }
    /// Prompt-cache input tokens read back this session (0 ⇒ caching off / unsupported).
    pub fn cache_read(&self) -> u64 {
        self.cache_read_tokens.load(Ordering::Relaxed)
    }
    /// Prompt-cache input tokens WRITTEN this session (0 on providers that do not report writes).
    pub fn cache_write(&self) -> u64 {
        self.cache_write_tokens.load(Ordering::Relaxed)
    }
    /// Input tokens sent this session, cache reads and writes included — the denominator that
    /// makes `cache_read() * 100 / input_total()` the session's real cache hit rate.
    pub fn input_total(&self) -> u64 {
        self.input_tokens.load(Ordering::Relaxed)
    }

    /// The ledger cursor `(epoch, seq)` after the most recent record — what a session file stores
    /// so its next save can ask [`CostMeter::rows_since`] for only the rows it has not seen.
    pub fn cursor(&self) -> (u64, u64) {
        (process_epoch(), self.seq.load(Ordering::Relaxed))
    }

    /// Rows a session file has not persisted yet. Same epoch: the rows after `last_seq`. A
    /// different epoch (the file was last written by another process, or never): every row the
    /// meter holds, since none of them can be in the file.
    pub fn rows_since(&self, epoch: u64, last_seq: u64) -> Vec<UsageRow> {
        let same_process = epoch == process_epoch();
        self.rows
            .lock()
            .map(|rows| {
                rows.iter()
                    .filter(|(seq, _)| !same_process || *seq > last_seq)
                    .map(|(_, row)| row.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `(turn, calls, input, cached)` summed over the rows of the CURRENT turn; `None` when no call
    /// of this turn reported usage. This is the "did the prefix cache hold across the turn" probe.
    pub fn last_turn_summary(&self) -> Option<(u64, u64, u64, u64)> {
        let turn = self.current_turn();
        let rows = self.rows.lock().ok()?;
        let mut calls = 0u64;
        let mut input = 0u64;
        let mut cached = 0u64;
        for (_, row) in rows.iter().filter(|(_, r)| r.turn == turn) {
            calls += 1;
            input += row.input;
            cached += row.cached;
        }
        (calls > 0).then_some((turn, calls, input, cached))
    }

    /// Reset on `/clear` (a fresh conversation starts a fresh cost tally). `turn` and `seq` are
    /// deliberately NOT reset: both are process-monotonic cursors, and a session file resumed
    /// after another conversation relies on them to keep its rows ordered and unduplicated.
    pub fn reset(&self) {
        self.prompt_tokens.store(0, Ordering::Relaxed);
        self.completion_tokens.store(0, Ordering::Relaxed);
        self.calls_with_usage.store(0, Ordering::Relaxed);
        self.cache_read_tokens.store(0, Ordering::Relaxed);
        self.cache_write_tokens.store(0, Ordering::Relaxed);
        self.input_tokens.store(0, Ordering::Relaxed);
        self.last_input_tokens.store(0, Ordering::Relaxed);
        self.last_cached_tokens.store(0, Ordering::Relaxed);
        self.last_completion_tokens.store(0, Ordering::Relaxed);
        if let Ok(mut rows) = self.rows.lock() {
            rows.clear();
        }
    }
}

/// Fetch the provider's advertised model ids (`GET {base}/models`). Used by `aizen models` and to
/// validate a freshly-set key. Returns the ids in the order the provider lists them.
/// One model row from `GET {base}/models`, plus the context window if the provider advertises it.
/// The bare OpenAI schema is just `id`; richer gateways (OpenRouter, LiteLLM) add a context length
/// under one of several field names — we read them all so the HUD can show a real number, not a guess.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// Context window in tokens, if the provider reported it. `None` ⇒ fall back to a name heuristic.
    pub context_length: Option<usize>,
    /// Free-tier model when the provider reported zero pricing (OpenRouter) or the id matches a
    /// known free-tier suffix. Tagged in the picker and `aizen models`.
    pub is_free: bool,
}

/// Does `base_url` point at Anthropic's own API host?
///
/// Anthropic exposes an OpenAI-compatible surface at `https://api.anthropic.com/v1/`, so the chat
/// path needs nothing special — `authorization: Bearer` is supported there. But `GET /v1/models` is
/// the NATIVE endpoint and authenticates with `x-api-key` + `anthropic-version`; a Bearer-only
/// request gets a 401 that looks exactly like a wrong key. Since the model list is what config
/// validation uses to prove a key works, that false 401 would make a perfectly good Anthropic key
/// unusable in setup.
///
/// Matched on host, not on a substring of the whole URL, so a proxy whose PATH mentions anthropic
/// (`https://gw.example.com/anthropic/v1`) is not misread as first-party.
pub fn is_anthropic_endpoint(base_url: &str) -> bool {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@') // strip any userinfo
        .next_back()
        .unwrap_or("")
        .split(':') // strip the port
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "api.anthropic.com" || host.ends_with(".api.anthropic.com")
}

/// Same host test for OpenCode's zen gateway. Its free tier is reachable with the literal shared
/// token `public` but only identifies clients that say who they are, so requests to this host carry
/// `x-opencode-client` — see [`with_provider_auth`]. Host-matched for the same reason as
/// [`is_anthropic_endpoint`]: a proxy whose path mentions opencode is not first-party.
pub fn is_opencode_endpoint(base_url: &str) -> bool {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@') // strip any userinfo
        .next_back()
        .unwrap_or("")
        .split(':') // strip the port
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "opencode.ai" || host.ends_with(".opencode.ai")
}

/// Free-tier variants on shared gateways (OpenCode zen, OpenRouter) carry a `-free` id suffix.
/// Names the convention once for the places that care: the key-probe model choice below and the
/// free tags in model listings.
pub fn pricing_is_free(prompt: &str, completion: &str) -> bool {
    fn zeroish(s: &str) -> bool {
        let t = s.trim();
        t == "0" || t == "0.0" || t == "0.00" || t.parse::<f64>().map(|n| n == 0.0).unwrap_or(false)
    }
    zeroish(prompt) && zeroish(completion)
}

pub fn is_free_model_id(id: &str) -> bool {
    // Shared gateway free-tier conventions: OpenCode zen uses *-free (plus the unsuffixed
    // big-pickle), OpenRouter uses :free. This helper is the fallback for picker/probe when we
    // don't have the pricing-derived is_free field (or the id suffix is enough).
    id.ends_with(":free") || id.ends_with("-free") || id == "big-pickle"
}

/// Attach auth for `base_url`: always Bearer (what every OpenAI-compatible gateway wants), plus
/// Anthropic's native pair when the host is theirs. Sending both is safe — each side ignores the
/// header it doesn't use — and it means one code path serves both wire dialects. OpenCode's gateway
/// gets its client-identification header the same way.
fn with_provider_auth(
    rb: reqwest::RequestBuilder,
    base_url: &str,
    api_key: &str,
) -> reqwest::RequestBuilder {
    let rb = rb.bearer_auth(api_key);
    if is_anthropic_endpoint(base_url) {
        rb.header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else if is_opencode_endpoint(base_url) {
        rb.header("x-opencode-client", "desktop")
    } else {
        rb
    }
}

/// What a one-shot endpoint check found. The point of the split is that the three failures need
/// DIFFERENT fixes from the user, and a single "it didn't work" string can't say which: a bad host is
/// a base-URL problem, a 401 is a key problem, and a 404 usually means the `/v1` suffix is missing.
#[derive(Debug)]
pub enum EndpointCheck {
    /// Reachable and authenticated. Carries the model list so a caller can offer it immediately.
    Ok(Vec<ModelInfo>),
    /// Reachable, but the credential was rejected (401/403) — re-ask for the key, not the URL.
    Auth(String),
    /// Reachable, but no model list at this path (404/405) — almost always a missing `/v1`.
    NotFound(String),
    /// No HTTP response at all: DNS, TLS, connection refused, timeout. The URL itself is suspect.
    Unreachable(String),
    /// Any other HTTP status. Reported verbatim rather than guessed at.
    Http(u16, String),
}

/// One-shot `GET {base}/models`, classified — the validation behind interactive setup.
///
/// Deliberately NOT `send_with_retry`: this runs while a human waits, and retrying a 401 three times
/// with backoff just makes a wrong key take eight seconds to report. Pass a short-timeout client.
/// `api_key` is optional so a base URL can be checked before a key exists — an endpoint that answers
/// 401 without credentials has still proven it is reachable and speaks the right protocol.
///
/// When a key IS supplied, a 200 from `/models` is not accepted as proof on its own: many gateways
/// serve their model list unauthenticated, so the key is additionally checked against the endpoint
/// that actually matters — see [`chat_auth_rejection`].
pub async fn check_endpoint(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> EndpointCheck {
    check_endpoint_with_deadline(client, base_url, api_key, CHAT_AUTH_PROBE_TIMEOUT).await
}

/// [`check_endpoint`] with the key-probe deadline injected, so a test can assert the timeout path
/// without holding a real connection open for the production 20s.
async fn check_endpoint_with_deadline(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    probe_deadline: std::time::Duration,
) -> EndpointCheck {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut rb = client.get(&url);
    if let Some(k) = api_key {
        rb = with_provider_auth(rb, base_url, k);
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        // Strip the reqwest chain down to its root: the outer layers repeat the URL the user just
        // typed, and the cause ("dns error", "connection refused") is the actionable part.
        Err(e) => {
            let root = std::error::Error::source(&e)
                .map(|s| s.to_string())
                .unwrap_or_else(|| e.to_string());
            return EndpointCheck::Unreachable(root);
        }
    };
    let status = resp.status();
    let code = status.as_u16();
    if status.is_success() {
        let infos = match parse_models_body(resp).await {
            Ok(infos) => infos,
            // A 200 that isn't a model list means this path isn't a models endpoint, even though
            // something answered — same user-facing fix as a 404.
            Err(e) => return EndpointCheck::NotFound(e.to_string()),
        };
        // A 200 here does NOT prove the key: plenty of gateways (kizeai among them) serve `/models`
        // to anyone, so a corrupted key sails through validation and only fails on the first real
        // turn. When we have a key, confirm it on the endpoint that enforces auth.
        if let Some(k) = api_key {
            // Prefer a `-free` variant when the list has one: free-tier gateways (OpenCode zen,
            // OpenRouter) mix paid models into the same list, and probing a paid one with a
            // free-tier credential can draw a 403 — which would read here as "bad key" and send
            // the user off to re-paste a token that was fine.
            let probe_model = infos
                .iter()
                .find(|m| m.is_free || is_free_model_id(&m.id))
                .or_else(|| infos.first())
                .map(|m| m.id.as_str())
                .unwrap_or("gpt-4o-mini");
            if let Some(detail) =
                chat_auth_rejection(client, base_url, k, probe_model, probe_deadline).await
            {
                return EndpointCheck::Auth(detail);
            }
        }
        return EndpointCheck::Ok(infos);
    }
    let detail = snippet_of(resp.text().await.unwrap_or_default());
    match code {
        401 | 403 => EndpointCheck::Auth(detail),
        404 | 405 => EndpointCheck::NotFound(detail),
        _ => EndpointCheck::Http(code, detail),
    }
}

/// Deadline for the key-confirmation probe below. A rejection is a single fast round-trip (an
/// upstream 401 needs no model work), so this only has to cover network latency — not generation.
const CHAT_AUTH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Does `{base}/chat/completions` reject this key? `Some(detail)` on a 401/403, `None` otherwise.
///
/// The point is to catch a bad credential that `GET /models` waved through, so ONLY an explicit auth
/// rejection counts. Every other outcome — 400 for a model this account can't use, 404 on an odd
/// path, a timeout, a transport error — returns `None`, because none of them prove the key is wrong
/// and a false "bad key" here would send the user off to re-paste a credential that was fine.
///
/// Kept as cheap as possible: one token, and the reply is discarded unread.
///
/// Independently deadlined. Callers pass the shared client, whose read timeout is sized for a full
/// streamed turn (300s) — far too long to hold up interactive setup if a gateway stalls. A key that
/// is genuinely bad answers 401 immediately; anything slower is not worth waiting on, since a
/// timeout here means "unproven", not "bad".
async fn chat_auth_rejection(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    deadline: std::time::Duration,
) -> Option<String> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "." }],
    });
    let send = with_provider_auth(client.post(&url), base_url, api_key)
        .json(&body)
        .send();
    let resp = tokio::time::timeout(deadline, send)
        .await
        .ok()? // deadline hit ⇒ unproven
        .ok()?; // transport error ⇒ unproven
    match resp.status().as_u16() {
        401 | 403 => Some(snippet_of(resp.text().await.unwrap_or_default())),
        _ => None,
    }
}

/// First line, ≤200 chars — provider error bodies are often a full HTML page or a deep JSON blob,
/// and neither belongs in a prompt the user is reading between keystrokes.
fn snippet_of(body: String) -> String {
    let first = body.trim().lines().next().unwrap_or("").trim().to_string();
    if first.chars().count() > 200 {
        let cut: String = first.chars().take(200).collect();
        format!("{cut}…")
    } else {
        first
    }
}

/// Lightweight reachability probe for the TUI health chip: a single `GET {base}/models` with **no**
/// retry/backoff (unlike [`fetch_models_info`]). Callers should pass a short-timeout client so a dead
/// endpoint fails fast. Success = 2xx (body is discarded). Failures surface as the same
/// `upstream returned HTTP {status}: …` / transport error shapes [`classify_api_error`] already knows.
pub async fn probe_models(client: &reqwest::Client, base_url: &str, api_key: &str) -> Result<()> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = with_provider_auth(client.get(&url), base_url, api_key)
        .send()
        .await
        .context("health probe request failed")?;
    let status = resp.status();
    if status.is_success() {
        // Drain the body so the connection can be reused; ignore parse — status alone is the signal.
        let _ = resp.bytes().await;
        return Ok(());
    }
    let detail = resp.text().await.unwrap_or_default();
    bail!("upstream returned HTTP {status}: {detail}");
}

/// Fetch the provider's model list with context windows when available (see [`ModelInfo`]).
pub async fn fetch_models_info(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = send_with_retry(|| with_provider_auth(client.get(&url), base_url, api_key)).await?;
    parse_models_body(resp).await
}

/// Parse a `/models` response body into [`ModelInfo`]s. Split out of [`fetch_models_info`] so the
/// interactive [`check_endpoint`] can reuse the exact same schema tolerance instead of re-deriving
/// it — a validator that accepts a narrower set of providers than the real fetch would reject
/// endpoints that actually work.
async fn parse_models_body(resp: reqwest::Response) -> Result<Vec<ModelInfo>> {
    #[derive(Deserialize)]
    struct ModelsResp {
        #[serde(default)]
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct TopProvider {
        #[serde(default)]
        context_length: Option<usize>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
        // OpenRouter: `context_length`; others: `context_window` / `max_context_length`.
        #[serde(default)]
        context_length: Option<usize>,
        #[serde(default)]
        context_window: Option<usize>,
        #[serde(default)]
        max_context_length: Option<usize>,
        // Anthropic's native `/v1/models` names it `max_input_tokens`. Without this the Claude models
        // would all fall back to the name heuristic even though the provider stated the real number.
        #[serde(default)]
        max_input_tokens: Option<usize>,
        // OpenRouter nests it here too: `top_provider.context_length`.
        #[serde(default)]
        top_provider: Option<TopProvider>,
        #[serde(default)]
        pricing: Option<Pricing>,
    }
    #[derive(Deserialize)]
    struct Pricing {
        #[serde(default)]
        prompt: String,
        #[serde(default)]
        completion: String,
    }

    let parsed: ModelsResp = resp.json().await.context("parsing models response")?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| {
            let ctx = m
                .context_length
                .or(m.context_window)
                .or(m.max_context_length)
                .or(m.max_input_tokens)
                .or_else(|| m.top_provider.and_then(|t| t.context_length))
                .filter(|&n| n > 0);
            let is_free = m
                .pricing
                .as_ref()
                .map(|p| pricing_is_free(&p.prompt, &p.completion))
                .unwrap_or(false);
            ModelInfo {
                id: m.id,
                context_length: ctx,
                is_free,
            }
        })
        .collect())
}

/// Transient upstream statuses worth a retry (rate-limit + gateway/server errors). A permanent 4xx
/// (auth, bad request) is NOT here — retrying it just wastes a round-trip and money.
///
/// Beyond the obvious five: 408 (the server timed us out — same shape as a transport timeout),
/// 425 (told to come back later), 522/524 (Cloudflare's origin-timeout pair, common in front of
/// self-hosted endpoints), and 529 — Anthropic's own "Overloaded" status, which used to skip this
/// fast `Retry-After`-aware layer and surface as a fatal error.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(
        status,
        408 | 425 | 429 | 500 | 502 | 503 | 504 | 522 | 524 | 529
    )
}

/// Exponential backoff CEILING (ms): `base * 2^attempt`, capped, floored at 1. Pure + monotonic so
/// it can be unit-tested; the live delay adds jitter under this ceiling (see `backoff_ms`).
fn backoff_ceiling_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    base_ms
        .saturating_mul(1u64 << attempt.min(20))
        .min(cap_ms)
        .max(1)
}

/// A small random u64 from the OS CSPRNG (already a dep) — for backoff jitter only.
fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    u64::from_le_bytes(b)
}

/// Longest SILENCE tolerated inside a live SSE stream before we declare it dead.
///
/// This is the fix for a confirmed hang: `send_with_retry` protects the request, but once the
/// provider answers 200 the `while let Some(event) = stream.next().await` loop had NO deadline at
/// all. A gateway that accepts the request and then stops writing (dropped upstream socket, a
/// load-balancer holding the connection open, a rate-limit stall) parks that loop forever — the
/// turn never fails, never returns, and the only way out is Esc + asking the model to continue.
/// reqwest's `read_timeout` is the backstop at 300s, which is far past the point where a person
/// concludes the tool is broken.
const STREAM_STALL_SECS: u64 = 90;

/// Env override for the stall deadline, clamped to a sane band. A reasoning model can legitimately
/// think for a long time before its first token, so the floor stays generous.
const STREAM_STALL_ENV: &str = "AIZEN_STREAM_STALL_SECS";

/// Resolve the inter-event stall deadline: env override (clamped 15s..=1800s) or the default.
pub(crate) fn stream_stall_timeout() -> std::time::Duration {
    let secs = std::env::var(STREAM_STALL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(15, 1800))
        .unwrap_or(STREAM_STALL_SECS);
    std::time::Duration::from_secs(secs)
}

/// Deadline for the FIRST useful output. A reasoning model that streams nothing until its answer
/// starts may be silent for minutes. Metadata/pings do not shorten or renew this allowance; once
/// text, reasoning or tool fragments arrive, the useful-progress idle deadline takes over.
const STREAM_FIRST_FRAME_SECS: u64 = 600;

/// Env override for the first-frame deadline, clamped 15s..=3600s.
const STREAM_FIRST_FRAME_ENV: &str = "AIZEN_STREAM_FIRST_FRAME_SECS";

pub(crate) fn stream_first_frame_timeout() -> std::time::Duration {
    let secs = std::env::var(STREAM_FIRST_FRAME_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(15, 3600))
        .unwrap_or(STREAM_FIRST_FRAME_SECS);
    std::time::Duration::from_secs(secs)
}

/// How many times a stream that died BEFORE producing anything is replayed. Bounded and only ever
/// on the blank case — see `stream_chat_with_tools_eager`.
pub(crate) const STREAM_BLANK_RETRIES: u32 = 2;

/// Historical truncation bound retained only for UTF-8 fixture coverage; live diagnostics never
/// echo frame payloads.
#[cfg(test)]
const FRAME_WARN_MAX: usize = 160;

/// Shorten a frame payload for a warn line, cutting on a char boundary (frames carry UTF-8 text).
#[cfg(test)]
fn truncate_frame(data: &str) -> String {
    let data = data.trim();
    if data.chars().count() <= FRAME_WARN_MAX {
        return data.to_string();
    }
    let head: String = data.chars().take(FRAME_WARN_MAX).collect();
    format!("{head}… ({} bytes total)", data.len())
}

/// Deserialize one SSE data frame, rejecting malformed completion frames and other API dialects.
/// Routing through `Value` preserves last-wins duplicate keys used by some gateways, without
/// echoing a rejected payload in diagnostics. Parseable metadata (including JSON pings) has
/// default-empty fields: only the semantic watchdog decides whether the model made progress.
fn parse_chunk(data: &str) -> Result<ChatChunk> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|_| super::delegated::Failure::Protocol)?;
    if matches!(value.get("type").and_then(|v| v.as_str()),
        Some("message_start" | "message_delta" | "message_stop" | "content_block_start"
            | "content_block_delta" | "content_block_stop"))
        || value.get("type").and_then(|v| v.as_str()).is_some_and(|t| t.starts_with("response."))
        || value.get("choices").and_then(|v| v.as_array()).is_some_and(|choices| {
            choices.iter().any(|choice| choice.get("message").is_some() && choice.get("delta").is_none())
        })
    {
        return Err(super::delegated::Failure::Protocol.into());
    }
    serde_json::from_value(value).map_err(|_| super::delegated::Failure::Protocol.into())
}

/// Tell the user the stream stalled and is being replayed. Routed through the TUI funnel: a raw
/// `eprintln!` here would be painted over by the retained render thread.
/// `rate-limited — retrying in 43s (2/3)` / `HTTP 503 — retrying in 2s (1/3)`.
pub(crate) fn retry_caption(status: u16, attempt: u32, max: u32, delay_ms: u64) -> String {
    let what = match status {
        429 => "rate-limited".to_string(),
        s => format!("HTTP {s}"),
    };
    format!(
        "{what} — retrying in {}s ({attempt}/{max})",
        delay_ms.div_ceil(1000)
    )
}

/// Publish a retried send. The caption rides the working line, which REPLACES itself each time — so
/// a burst of 429s reads as one changing line (`rate-limited — retrying in 2s (3/3)`) instead of a
/// column of near-identical notes that buried the turn. Nothing is appended to the transcript: the
/// retry is transient state, not part of the conversation.
fn send_retry_note(status: u16, attempt: u32, max: u32, delay_ms: u64) {
    let text = retry_caption(status, attempt, max, delay_ms);
    crate::ui::tui::set_work_caption(&text);
    // Off-TTY there is no working line to carry it, so the one line is all the user gets.
    if !crate::ui::tui::active() {
        crate::ui::tui::note_line(&format!("⟳ {text}"));
    }
}

/// Same contract as [`send_retry_note`]: caption only, never a transcript line per attempt.
pub(crate) fn stream_retry_note(reason: &str, attempt: u32, max: u32, delay_ms: u64) {
    let line = format!(
        "⟳ stream died before any output ({reason}) — retrying {attempt}/{max} in {delay_ms}ms"
    );
    crate::ui::tui::set_work_caption(&line);
    if !crate::ui::tui::active() {
        crate::ui::tui::note_line(&line);
    }
}

/// Full-jitter backoff: a uniform delay in `[ceil/2, ceil]`. Jitter spreads retries so a fleet of
/// callers doesn't thunder back in lock-step after a shared 503.
pub(crate) fn backoff_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    let ceil = backoff_ceiling_ms(attempt, base_ms, cap_ms);
    let half = ceil / 2;
    half + (rand_u64() % (ceil - half + 1))
}

/// `Retry-After` as ms (integer-seconds form only), capped at 30s so a hostile header can't park us.
/// The HTTP-date form is ignored (falls back to exponential backoff).
fn retry_after_ms(resp: &reqwest::Response) -> Option<u64> {
    let v = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: u64 = v.trim().parse().ok()?;
    Some(secs.saturating_mul(1000).min(30_000))
}

/// Send a request with bounded retry + exponential backoff. `build` is re-invoked per attempt
/// because `.send()` consumes the `RequestBuilder`. Retries transport errors and transient upstream
/// statuses (429/5xx), honoring `Retry-After`; a permanent non-2xx (or exhausted retries) reads the
/// body and returns the SAME `upstream returned HTTP {status}: {detail}` error the call sites used
/// before this layer existed. A 2xx `Response` is returned untouched for the caller to parse/stream.
/// Shape the gateway returns for /v1/* errors: {"error":{"message","type","code","correlation_id"}}.
/// Prefer its message verbatim (pilot already differentiates expired/revoked/unpinned/malformed)
/// and surface correlation_id so the operator can grep pilot+nginx logs with one id.
/// Safe evidence that can cross a child boundary. Provider strings are never printed verbatim:
/// gateways sometimes echo credentials, image data, prompts, or the whole rejected request.
#[derive(Debug, Clone)]
pub struct TransportFailure {
    pub stage: &'static str,
    pub http_status: Option<u16>,
    pub provider_code: Option<String>,
    pub provider_status: Option<u16>,
    pub correlation_id: Option<String>,
    pub message: String,
}

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (stage={}", self.message, self.stage)?;
        if let Some(status) = self.http_status {
            write!(f, " HTTP {status}")?;
        }
        if let Some(code) = &self.provider_code {
            write!(f, " code={code}")?;
        }
        if let Some(status) = self.provider_status {
            write!(f, " provider_status={status}")?;
        }
        if let Some(id) = &self.correlation_id {
            write!(f, " correlation_id={id}")?;
        }
        write!(f, ")")
    }
}

impl std::error::Error for TransportFailure {}

fn safe_identifier(value: &str) -> Option<String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        || lower.contains("sk-")
        || lower.contains("bearer")
        || lower.starts_with("eyj")
        || lower.starts_with("data:")
    {
        return None;
    }
    Some(value.to_string())
}

pub(crate) fn response_correlation(resp: &reqwest::Response) -> Option<String> {
    ["x-request-id", "x-correlation-id"]
        .into_iter()
        .find_map(|name| {
            resp.headers()
                .get(name)?
                .to_str()
                .ok()
                .and_then(safe_identifier)
        })
}

/// Retain only recognized diagnostic semantics, not user/provider-supplied text. In particular
/// known field-quirk and context-overflow recovery must still work after sanitizing the error.
fn safe_failure_message(detail: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    if is_overflow_message(&lower) {
        return "provider rejected the request: context_length_exceeded".into();
    }
    let mut message = "provider rejected the request".to_string();
    for field in [
        "reasoning_effort",
        "max_completion_tokens",
        "max_tokens",
        "parallel_tool_calls",
        "tool_choice",
        "cache_control",
    ] {
        if lower.contains(field) {
            message.push_str("; unsupported parameter ");
            message.push_str(field);
        }
    }
    if lower.contains("reasoning effort") && !lower.contains("reasoning_effort") {
        message.push_str("; unsupported reasoning_effort");
    }
    message
}

pub(crate) fn transport_failure(
    stage: &'static str,
    status: Option<u16>,
    body: &str,
    correlation: Option<String>,
) -> TransportFailure {
    let value = serde_json::from_str::<serde_json::Value>(body).ok();
    let error = value
        .as_ref()
        .and_then(|v| v.get("error").or_else(|| v.pointer("/response/error")))
        .filter(|e| !e.is_null());
    let provider_code = error
        .and_then(|e| e.get("code").or_else(|| e.get("type")))
        .and_then(|v| v.as_str())
        .and_then(safe_identifier);
    let provider_status = error
        .and_then(|e| e.get("status").or_else(|| e.get("status_code")))
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .and_then(|n| u16::try_from(n).ok());
    let correlation_id = error
        .and_then(|e| e.get("correlation_id").or_else(|| e.get("request_id")))
        .and_then(|v| v.as_str())
        .and_then(safe_identifier)
        .or(correlation);
    TransportFailure {
        stage,
        http_status: status,
        provider_code,
        provider_status,
        correlation_id,
        message: safe_failure_message(body),
    }
}

/// Bound bytes before the SSE parser buffers lines/events (including unfinished data and comments).
pub(crate) const SSE_RAW_BYTES_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum BoundedSseError<E> {
    Read(E),
    Limit,
}

/// No payload or reqwest diagnostic is ever formatted by this adapter.
impl<E> std::fmt::Display for BoundedSseError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Read(_) => "SSE transport read failed",
            Self::Limit => "SSE response exceeds 16 MiB raw-byte limit",
        })
    }
}

pub(crate) struct BoundedSseStream<S> {
    source: std::pin::Pin<Box<S>>,
    remaining: usize,
    terminated: bool,
}

impl<S, B, E> futures_util::Stream for BoundedSseStream<S>
where
    S: futures_util::Stream<Item = std::result::Result<B, E>>,
    B: AsRef<[u8]>,
{
    type Item = std::result::Result<B, BoundedSseError<E>>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }
        match this.source.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let bytes = chunk.as_ref().len();
                if bytes > this.remaining {
                    // Drop the ENTIRE crossing chunk before eventsource can allocate for it.
                    this.terminated = true;
                    Poll::Ready(Some(Err(BoundedSseError::Limit)))
                } else {
                    this.remaining -= bytes;
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            Poll::Ready(Some(Err(error))) => {
                this.terminated = true;
                Poll::Ready(Some(Err(BoundedSseError::Read(error))))
            }
            Poll::Ready(None) => {
                this.terminated = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(crate) fn bounded_sse_bytes<S>(source: S) -> BoundedSseStream<S> {
    bounded_sse_bytes_with_limit(source, SSE_RAW_BYTES_LIMIT)
}

fn bounded_sse_bytes_with_limit<S>(source: S, limit: usize) -> BoundedSseStream<S> {
    BoundedSseStream {
        source: Box::pin(source),
        remaining: limit,
        terminated: false,
    }
}

/// Parser/UTF-8/limit failures are terminal, unlike a transport drop eligible for narrow salvage.
/// Never format the parser error: it may embed the buffered provider data.
pub(crate) fn sse_read_failure<E>(
    error: eventsource_stream::EventStreamError<BoundedSseError<E>>,
    correlation_id: Option<String>,
) -> (TransportFailure, bool) {
    use eventsource_stream::EventStreamError;
    let (code, message, terminal) = match error {
        EventStreamError::Transport(BoundedSseError::Limit) => (
            "SSE_RAW_LIMIT",
            "SSE stream error: response exceeds 16 MiB raw-byte limit",
            true,
        ),
        EventStreamError::Parser(_) | EventStreamError::Utf8(_) => (
            "SSE_MALFORMED",
            "SSE stream error: malformed framing or UTF-8",
            true,
        ),
        EventStreamError::Transport(BoundedSseError::Read(_)) => (
            "SSE_READ_FAILED",
            "SSE stream error: transport read failed",
            false,
        ),
    };
    (
        TransportFailure {
            stage: "sse",
            http_status: Some(200),
            provider_code: Some(code.into()),
            provider_status: None,
            correlation_id,
            message: message.into(),
        },
        terminal,
    )
}

/// Bound error reads as well as error display. A hostile response must not allocate its entire
/// body or trickle forever after headers; the logical send's remaining deadline is the ceiling.
pub(crate) async fn bounded_error_body(
    resp: reqwest::Response,
    remaining: std::time::Duration,
) -> String {
    let read = async {
        let mut bytes = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(Ok(chunk)) = stream.next().await {
            let take = chunk.len().min(16_384usize.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk[..take]);
            if bytes.len() == 16_384 {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    tokio::time::timeout(remaining.min(std::time::Duration::from_secs(10)), read)
        .await
        .unwrap_or_default()
}

async fn send_with_retry<F>(build: F) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    send_with_retry_render(build, true).await
}

async fn send_with_retry_render<F>(build: F, render: bool) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    const MAX_RETRIES: u32 = 3;
    const BASE_MS: u64 = 400;
    const CAP_MS: u64 = 8_000;
    // TOTAL wall-clock budget for one logical send, retries and backoff included. This is the only
    // ceiling on the PRE-response phase: the endpoint deliberately has no whole-request `.timeout()`
    // (a streamed response lives longer than any sane value), and `read_timeout` fires per READ —
    // a gateway that completes the handshake and then trickles held a call for up to 4×300s before
    // this existed, and the loop above multiplied that by its own retries. Generous on purpose: a
    // non-streaming completion legitimately spends its whole generation time before headers arrive.
    let budget = send_total_budget();
    let start = std::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        let remaining = budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            bail!(
                "request exceeded the {}s total send budget (no response headers) — timeout",
                budget.as_secs()
            );
        }
        crate::llm::media::validate_image_request()?;
        super::delegated::before_send()?;
        match tokio::time::timeout(remaining, build().send()).await {
            Err(_) => bail!(
                "request exceeded the {}s total send budget (no response headers) — timeout",
                budget.as_secs()
            ),
            Ok(Ok(resp)) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                if is_retryable_status(status.as_u16()) && attempt < MAX_RETRIES {
                    let delay = retry_after_ms(&resp)
                        .unwrap_or_else(|| backoff_ms(attempt, BASE_MS, CAP_MS));
                    attempt += 1;
                    // Say so: a sleep on `Retry-After` used to be indistinguishable from a
                    // hang. The caption rides the working line; the note lands once in the
                    // transcript.
                    if render {
                        send_retry_note(status.as_u16(), attempt, MAX_RETRIES, delay);
                    }
                    super::delegated::sleep(std::time::Duration::from_millis(delay)).await?;
                    continue;
                }
                /* A 401 from the Aizen gateway is not "the key in the config is wrong", it is
                "the credential this call used is finished" — and on a machine that only ever runs
                the CLI it is the only chance to find that out, since nothing here polls the gateway
                on a timer. Which credential it was decides the sentence, and only one of the two
                leaves anything to clean up locally. The URL is taken before the body, because
                reading the body consumes the response — and only the gateway's own host qualifies,
                so a wrong key at any other provider still reads as a wrong key. */
                use crate::llm::gateway::Unauthorized;
                let url = resp.url().clone();
                let verdict = (status.as_u16() == 401)
                    .then(|| crate::llm::gateway::on_unauthorized(url.as_str()));
                let correlation = response_correlation(&resp);
                let raw = bounded_error_body(resp, budget.saturating_sub(start.elapsed())).await;
                let failure = transport_failure("http", Some(status.as_u16()), &raw, correlation);
                let detail = failure.to_string();
                match verdict {
                    // Ending the session here means the NEXT run stops at `resolve_endpoint` with
                    // the sentence that names the fix, instead of failing at the provider with a
                    // bare 401 every time.
                    // Since 2026-09-06 each machine carries its OWN credential and the gateway
                    // reads its device row on every call, so a 401 here is most often somebody
                    // cutting this machine on purpose rather than a clock running out. Naming the
                    // three causes is the difference between "run login again" and a hunt.
                    Some(Unauthorized::PinEnded) => bail!(
                        "this machine has been unpinned (HTTP 401), so its credential was removed \
                         from here. That is what an unpin in the dashboard, a logout from another \
                         machine, or an account password change all do. \
                         Run `aizen login` to pin it again. The gateway said: {detail}"
                    ),
                    // Nothing was removed, because nothing was stored: the plan is sold by signing
                    // in and this call carried the session token. Tokens last 30 days, there is no
                    // refresh route, and a password change anywhere kills every one minted before
                    // it — so the fix is always the same command, and never a retry.
                    //
                    // Since pilot 401-p1 the gateway stamps a per-request correlation_id (x-request-id)
                    // and returns a reasoned message (expired/revoked/unpinned/malformed). Show it
                    // verbatim — it already says which aizen command fixes it.
                    Some(Unauthorized::SignInAgain) => bail!(
                        "the Aizen gateway rejected the account session (HTTP 401). A 401 here \
                         means the session is over — not a hiccup to retry. \
                         Run `aizen account login` to sign in again. The gateway said: {detail}"
                    ),
                    _ => {}
                }
                return Err(failure.into());
            }
            Ok(Err(e)) => {
                if attempt < MAX_RETRIES {
                    let delay = backoff_ms(attempt, BASE_MS, CAP_MS);
                    attempt += 1;
                    super::delegated::sleep(std::time::Duration::from_millis(delay)).await?;
                    continue;
                }
                let _ = e; // reqwest errors may include credential-bearing URLs.
                bail!("request failed after retries (stage=send)");
            }
        }
    }
}

/// The total send budget: `AIZEN_SEND_BUDGET_SECS` clamped to 60s..=3600s, default 600s. The floor
/// keeps a typo from strangling every call; the ceiling keeps a typo from disabling the guard.
fn send_total_budget() -> std::time::Duration {
    let secs = std::env::var("AIZEN_SEND_BUDGET_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(60, 3600))
        .unwrap_or(600);
    std::time::Duration::from_secs(secs)
}

/// Classification of a `chat()` error for GOAL MODE's smart retry (see the goal loop in
/// `src/agent/mod.rs`). Goal mode keeps working through API flakiness, but must distinguish a
/// transient upstream hiccup (retry indefinitely with backoff) from a permanent client error (bad
/// key/request/endpoint — retry only a few times then give up, since hammering it just burns calls).
/// Ordinary (non-goal) turns never call this — their errors stay fatal after the HTTP client's own
/// bounded retries inside `send_with_retry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 429 / 5xx / transport / timeout / mid-stream drop — safe to retry indefinitely with backoff.
    Transient,
    /// 4xx client/config errors retrying can't fix (bad request, payment, unprocessable).
    /// Goal mode retries a small bounded number of times (the provider may be briefly misbehaving)
    /// then stops with the error.
    Permanent,
    /// 401/403 — authentication is over, and it is over for good.
    ///
    /// Its own kind rather than a `Permanent`, because the bounded retries that are right for a
    /// briefly misbehaving provider are wrong here twice over. The credential is already gone from
    /// disk by the time this is classified (`gateway::on_unauthorized` takes it at the first 401),
    /// so every retry is a call that cannot succeed; and since 2026-09-06 a `401` from Aizen is
    /// usually not an expiry at all but an unpinning — from the dashboard, from another machine's
    /// logout, or from a password change. A person who did that is waiting to be told to run
    /// `aizen login`, and a backoff loop shows them a hung window instead.
    SignedOut,
    /// The request was too big for the model's window — a 413, or a 400 whose body says so.
    /// Neither retrying (it will 4xx identically) nor giving up (the transcript can be shrunk) is
    /// right: the loop's recovery is an emergency clear/compact, then ONE more try.
    ContextOverflow,
}

/// Classify a `chat()` error for loop-level retry. `send_with_retry` surfaces upstream failures as
/// `upstream returned HTTP {status}: {body}` (where `{status}` renders like `400 Bad Request`) and
/// transport failures as `request failed after retries`; the streaming path adds `SSE stream error:
/// …`. Overflow is tested FIRST because it arrives wearing a permanent status (400/413) while having
/// its own recovery. After that the real status number decides; anything without one — transport,
/// timeout, stream drop — is Transient, because goal mode's whole point is surviving a flaky API.
pub fn classify_api_error(e: &anyhow::Error) -> ApiErrorKind {
    if e.downcast_ref::<super::delegated::Failure>().is_some() {
        return ApiErrorKind::Permanent;
    }
    let msg = e.to_string();
    if is_overflow_message(&msg) {
        return ApiErrorKind::ContextOverflow;
    }
    let structured = e.downcast_ref::<TransportFailure>();
    let status = structured
        .and_then(|f| f.http_status.filter(|s| *s >= 400).or(f.provider_status))
        .or_else(|| extract_http_status(&msg));
    match status {
        // 413 without an overflow-shaped body is still "the request is too large" for a chat
        // call — same recovery. 402 (billing), 422 (unprocessable), 451 (blocked) used to fall
        // through to Transient and burn every retry the budget had.
        Some(413) => ApiErrorKind::ContextOverflow,
        // The transient 4xx trio comes BEFORE the blanket 4xx arm: 429 is the single most common
        // retryable status there is.
        Some(408 | 425 | 429) => ApiErrorKind::Transient,
        // And auth comes before it too, for the opposite reason: not "try again sooner" but
        // "do not try again at all".
        Some(401 | 403) => ApiErrorKind::SignedOut,
        Some(400..=499) => ApiErrorKind::Permanent,
        // 5xx and any parsed status that isn't a client error: worth another try.
        Some(_) => ApiErrorKind::Transient,
        None => ApiErrorKind::Transient,
    }
}

/// The first `HTTP {digits}` in a rendered error message, as produced by `send_with_retry`.
fn extract_http_status(msg: &str) -> Option<u16> {
    let rest = &msg[msg.find("HTTP ")? + 5..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Does this error body say the prompt outgrew the model's window? Providers word it differently;
/// the phrases below cover OpenAI (`context_length_exceeded`, "maximum context length"), Anthropic
/// ("prompt is too long"), and the common gateway paraphrases. Deliberately narrow — a false
/// positive here would shrink a healthy transcript on an unrelated 400.
fn is_overflow_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "exceeds the maximum number of tokens",
    ]
    .iter()
    .any(|phrase| m.contains(phrase))
}

/// Full-jitter backoff (ms) for GOAL MODE's loop-level retry, indexed by attempt. Reuses the exact
/// full-jitter formula as the HTTP client's own `send_with_retry` (spreads retries, no thundering
/// herd) but with a higher ceiling: goal mode may retry a flaky endpoint many times over a long run,
/// so the per-attempt wait grows to a ~30s plateau instead of the client's 8s (which is tuned for 3
/// quick tries inside a single call).
pub fn goal_backoff_ms(attempt: u32) -> u64 {
    const BASE_MS: u64 = 500;
    const CAP_MS: u64 = 30_000;
    backoff_ms(attempt, BASE_MS, CAP_MS)
}

/// Full-jitter backoff (ms) for an INTERACTIVE turn's loop-level retry — a person is sitting there
/// watching. Base 300ms, cap 4s: the whole ~10-attempt chain lands around ~30–40s total (enough for
/// a gateway to recover from a 429/5xx blip), while no SINGLE wait ever reaches the 30s that
/// `goal_backoff_ms` allows — a lone 30s pause reads as a hang, which is exactly the feel this avoids.
/// `goal_backoff_ms` stays higher-ceilinged for `/goal`, which runs long with nobody watching.
pub fn interactive_backoff_ms(attempt: u32) -> u64 {
    const BASE_MS: u64 = 300;
    const CAP_MS: u64 = 4_000;
    backoff_ms(attempt, BASE_MS, CAP_MS)
}

/// A request feature some model or gateway rejects with a 400. Learned REACTIVELY, per model,
/// for the session — never guessed from the model name (a name heuristic mis-serves every gateway
/// that renames models). The first 400 that names the field records the quirk; every later request
/// to that model is built without it, so a quirk costs one failed call per model per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Quirk {
    /// `reasoning_effort` unknown to the model, or the tier we sent out of its range.
    ReasoningEffort,
    /// o-series / gpt-5 style: `max_tokens` rejected in favour of `max_completion_tokens`.
    MaxCompletionTokens,
    /// Strict local servers that reject `parallel_tool_calls`…
    ParallelToolCalls,
    /// …or `tool_choice`.
    ToolChoice,
    /// A gateway that does not accept Anthropic `cache_control` breakpoints on the OpenAI wire.
    CacheControl,
}

impl Quirk {
    /// The wire field the provider complained about.
    pub(crate) fn field(self) -> &'static str {
        match self {
            Quirk::ReasoningEffort => "reasoning_effort",
            Quirk::MaxCompletionTokens => "max_tokens",
            Quirk::ParallelToolCalls => "parallel_tool_calls",
            Quirk::ToolChoice => "tool_choice",
            Quirk::CacheControl => "cache_control",
        }
    }

    fn remedy(self) -> &'static str {
        match self {
            Quirk::MaxCompletionTokens => "as max_completion_tokens",
            Quirk::CacheControl => "without cache breakpoints",
            _ => "without it",
        }
    }
}

type QuirkMap = std::collections::HashMap<String, std::collections::HashSet<Quirk>>;

static MODEL_QUIRKS: std::sync::OnceLock<std::sync::Mutex<QuirkMap>> = std::sync::OnceLock::new();

fn model_quirks() -> &'static std::sync::Mutex<QuirkMap> {
    MODEL_QUIRKS.get_or_init(|| std::sync::Mutex::new(QuirkMap::new()))
}

/// Has `model` already rejected `quirk`'s field this session?
#[cfg(test)]
pub(crate) fn quirk_known(model: &str, quirk: Quirk) -> bool {
    model_quirks()
        .lock()
        .map(|m| m.get(model).is_some_and(|s| s.contains(&quirk)))
        .unwrap_or(false)
}

pub(crate) fn mark_quirk(model: &str, quirk: Quirk) {
    if let Ok(mut m) = model_quirks().lock() {
        m.entry(model.to_string()).or_default().insert(quirk);
    }
}

/// Does `body` still carry a cache breakpoint anywhere?
fn has_cache_control(body: &ChatRequest) -> bool {
    body.messages.iter().any(|m| m.cache_control.is_some())
        || body.tools.iter().any(|t| t.cache_control.is_some())
}

/// Which request feature does this 400 body blame, among those the request ACTUALLY carried?
/// Field names are matched loosely (case-insensitive); a field we never sent cannot be "ours", so
/// an error that merely mentions `max_tokens` on a request without one is left alone.
pub(crate) fn quirk_blamed(detail: &str, body: &ChatRequest) -> Option<Quirk> {
    let d = detail.to_ascii_lowercase();
    if body.reasoning_effort.is_some() && body_blames_effort(&d) {
        return Some(Quirk::ReasoningEffort);
    }
    // OpenAI: "Unsupported parameter: 'max_tokens' is not supported with this model. Use
    // 'max_completion_tokens' instead." A body that only says the VALUE is too large names no
    // replacement and says nothing about support — that one is a real error, not a quirk.
    if body.max_tokens.is_some()
        && (d.contains("max_completion_tokens")
            || (d.contains("max_tokens")
                && (d.contains("unsupported") || d.contains("not supported"))))
    {
        return Some(Quirk::MaxCompletionTokens);
    }
    if body.parallel_tool_calls.is_some() && d.contains("parallel_tool_calls") {
        return Some(Quirk::ParallelToolCalls);
    }
    if body.tool_choice.is_some() && d.contains("tool_choice") {
        return Some(Quirk::ToolChoice);
    }
    if has_cache_control(body) && d.contains("cache_control") {
        return Some(Quirk::CacheControl);
    }
    None
}

/// Rebuild `body` without the feature `quirk` names.
pub(crate) fn apply_quirk(body: &mut ChatRequest, quirk: Quirk) {
    match quirk {
        Quirk::ReasoningEffort => body.reasoning_effort = None,
        Quirk::MaxCompletionTokens => {
            if let Some(n) = body.max_tokens.take() {
                body.max_completion_tokens = Some(n);
            }
        }
        Quirk::ParallelToolCalls => body.parallel_tool_calls = None,
        Quirk::ToolChoice => body.tool_choice = None,
        Quirk::CacheControl => {
            for m in &mut body.messages {
                m.cache_control = None;
            }
            for t in &mut body.tools {
                t.cache_control = None;
            }
        }
    }
}

/// Strip everything `body.model` is already known to reject, before the first byte goes out.
fn apply_known_quirks(body: &mut ChatRequest) {
    let known: Vec<Quirk> = model_quirks()
        .lock()
        .map(|m| {
            m.get(&body.model)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    for q in known {
        apply_quirk(body, q);
    }
}

/// How many distinct quirks one call may learn (one 400 each) before the 400 is treated as real.
const MAX_QUIRK_RETRIES: usize = 4;

/// Does this error detail blame `reasoning_effort`? Providers word it differently ("unknown parameter
/// reasoning_effort", "reasoning_effort: unsupported value", "does not support reasoning effort"), so
/// match the field name loosely (case-insensitive, with/without the underscore).
fn body_blames_effort(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("reasoning_effort") || d.contains("reasoning effort")
}

/// POST a chat request with WIRE-LEVEL capability fallback: send the body as built; if the
/// provider 400s SPECIFICALLY because of a field we sent — `reasoning_effort` (unknown, or the tier
/// out of range), `max_tokens` on a model that wants `max_completion_tokens`, `parallel_tool_calls`
/// or `tool_choice` on a strict local server, `cache_control` on a gateway that refuses it — rebuild
/// without that field, remember the model for the rest of the session, and retry. A call can learn
/// up to `MAX_QUIRK_RETRIES` such fields in a row; any other error propagates unchanged. Quirks
/// already learned are stripped up front, so each costs one failed call per model, once.
struct PreparedChatRequest {
    body: ChatRequest,
    wire: Vec<u8>,
}

impl PreparedChatRequest {
    fn new(mut body: ChatRequest) -> Result<Self> {
        apply_known_quirks(&mut body);
        let wire = serde_json::to_vec(&body)?;
        Ok(Self { body, wire })
    }

    fn apply_quirk(&mut self, quirk: Quirk) -> Result<()> {
        apply_quirk(&mut self.body, quirk);
        self.wire = serde_json::to_vec(&self.body)?;
        Ok(())
    }
}

async fn send_chat(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: ChatRequest,
) -> Result<reqwest::Response> {
    let mut prepared = PreparedChatRequest::new(body)?;
    send_prepared_chat(client, url, api_key, &mut prepared, true).await
}

async fn send_prepared_chat(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    prepared: &mut PreparedChatRequest,
    render: bool,
) -> Result<reqwest::Response> {
    let mut learned = 0usize;
    loop {
        // `with_provider_auth`, NOT bare `bearer_auth`: Anthropic's native surface wants
        // `x-api-key` + `anthropic-version`, and OpenCode zen wants `x-opencode-client`. The
        // `/models` probe already sent them — the actual chat traffic must not be the one path
        // that forgets. The host test parses the host out of the full URL, so passing the chat
        // URL (not the base) is fine.
        let err = match send_with_retry_render(
            || {
                with_provider_auth(client.post(url), url, api_key)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(prepared.wire.clone())
            },
            render,
        )
        .await
        {
            Ok(r) => return Ok(r),
            Err(e) => e,
        };
        let msg = err.to_string();
        let quirk = if msg.contains("HTTP 400") {
            quirk_blamed(&msg, &prepared.body)
        } else {
            None
        };
        let Some(q) = quirk else {
            return Err(err);
        };
        if learned >= MAX_QUIRK_RETRIES {
            return Err(err);
        }
        learned += 1;
        mark_quirk(&prepared.body.model, q);
        prepared.apply_quirk(q)?;
        if !render {
            continue;
        }
        let note = format!(
            "{} not accepted by {} — retrying {} (won't send it again this session)",
            q.field(),
            prepared.body.model,
            q.remedy()
        );
        if crate::ui::tui::active() {
            crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
        } else {
            crate::ui::tui::note_line(&crate::ui::theme::faint(note).to_string());
        }
    }
}

/// The outcome of one non-streaming tool-calling turn.
pub struct ChatTurn {
    /// Natural-language content (the final answer when `tool_calls` is empty).
    pub content: Option<String>,
    /// Tool calls the model wants executed. NON-EMPTY ⇒ tools path (the source of truth —
    /// do not branch on `finish_reason`).
    pub tool_calls: Vec<ToolCall>,
    /// Advisory only — the loop classifies by `tool_calls`, not this. Kept for diagnostics.
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
    /// Provider-reported usage for THIS call, when sent (the final chunk in streaming). The loop's
    /// context guards prefer this real number over the chars/4 estimate.
    pub usage: Option<Usage>,
    /// One-line summary of what the wire ACTUALLY carried when the turn looks empty: unknown
    /// message/choice keys, reasoning-channel length, usage. Populated only on the non-streaming
    /// path when there is no text and no tool call — an empty turn then fails with a diagnosis that
    /// NAMES the shape gap instead of "empty response".
    pub empty_wire_note: Option<String>,
    /// A bounded printable prefix of the RAW response body, populated on the same condition as
    /// [`Self::empty_wire_note`]. The note names which keys were read; THIS is the evidence that
    /// settles the two cases the note cannot tell apart — a billed-completion reply whose text
    /// sits in a key the parser has never read (a NEW shape gap: fix the parser) vs a body that
    /// genuinely carries no text (a silent provider). Printed ONLY when the budget dies, never
    /// per retry — a mid-loop reply can carry user data, so it must not scroll by unwatched.
    pub empty_wire_raw: Option<String>,
    /// EAGERLY-STARTED tool executions from the streaming path: `(position in tool_calls, handle)`.
    /// A read-only call whose arguments finished streaming may already be running before the
    /// response ends — the executor ADOPTS these instead of re-spawning. Discarding a `ChatTurn`
    /// (divergence, error) just detaches them; eager calls are read-only, so that is harmless.
    pub eager: Vec<(usize, crate::agent::tools::StartedSafeCall)>,
}

/// Hook offered each tool call the moment its streamed arguments complete: return a running
/// handle to START it early (read-only + prefix-safe only — the caller owns that policy), or
/// `None` to let the executor run it normally after the stream ends.
pub type EagerStartFn<'a> =
    &'a (dyn Fn(usize, &ToolCall) -> Option<crate::agent::tools::StartedSafeCall> + Send + Sync);

/// Stream a chat completion. Prints content deltas to stdout as they arrive and
/// returns the full concatenated assistant text. Returns a typed error on a non-2xx
/// response (so the caller can decide retry/stop) instead of panicking.
///
/// Superseded by the tool-aware streaming path: every live caller needs tool calls out of the same
/// stream, and printing straight to stdout would corrupt the retained frame anyway. Kept as the
/// plain-text reference shape of an SSE read.
#[allow(dead_code)]
pub async fn stream_chat(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<Message>,
) -> Result<String> {
    stream_chat_with_visual_contract(client, base_url, api_key, model, messages, false).await
}

/// Stream one top-level answer with optional visual-response guidance and terminal Markdown rendering.
pub async fn stream_chat_with_visual_contract(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<Message>,
    visual_contract: bool,
) -> Result<String> {
    let cfg = crate::core::cli_config::load();
    let effort = crate::core::cli_config::resolved_reasoning_effort(cfg.reasoning_effort.clone());
    stream_chat_with_visual_contract_frozen(
        client,
        base_url,
        api_key,
        model,
        messages,
        visual_contract,
        &cfg,
        effort,
    )
    .await
}

/// Use the caller's immutable request policy rather than reloading it after delegated work.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_chat_with_visual_contract_frozen(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<Message>,
    visual_contract: bool,
    cfg: &crate::core::cli_config::CliConfig,
    effort: Option<String>,
) -> Result<String> {
    // Tape first (plain-text calls share the tape as content-only turns). See `llm::replay`.
    if let Some(text) = crate::llm::replay::replay_text(model, &messages)? {
        return Ok(text);
    }
    let recorded_view = if crate::llm::replay::mode() == crate::llm::replay::Mode::Record {
        Some(messages.clone())
    } else {
        None
    };
    let text = stream_chat_with_visual_contract_live(
        client,
        base_url,
        api_key,
        model,
        messages,
        visual_contract,
        cfg,
        effort,
    )
    .await?;
    if let Some(view) = recorded_view {
        crate::llm::replay::record_text(model, &view, &text)?;
    }
    Ok(text)
}

#[allow(clippy::too_many_arguments)]
async fn stream_chat_with_visual_contract_live(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    mut messages: Vec<Message>,
    visual_contract: bool,
    cfg: &crate::core::cli_config::CliConfig,
    effort: Option<String>,
) -> Result<String> {
    if visual_contract {
        if let Some(block) = crate::agent::response_visuals_prompt_block(cfg.response_visuals()) {
            messages.insert(0, Message::system(block));
        }
    }
    if crate::llm::oauth_codex::is_codex_base_url(base_url) {
        let session = std::env::var("AIZEN_CODEX_SESSION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("aizen-{}-{model}", std::process::id()));
        return crate::llm::responses_codex::stream_turn_effort_frozen(
            client,
            model,
            &messages,
            &[],
            &session,
            crate::llm::responses_codex::StreamSink {
                render: true,
                eager: None,
                observed_usage: None,
            },
            effort,
            cfg,
        )
        .await
        .map(|turn| turn.content.unwrap_or_default());
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let _request_memory = crate::llm::media::request_memory_lease(&messages)?;
    let policy = crate::core::cli_config::vision_support_in(cfg, base_url, model).into();
    crate::llm::media::validate_reference_vision(policy)?;
    // The ONE body builder, like the tool-calling paths: this surface used to hand-roll its
    // request without `stream_options.include_usage`, then read `chunk.usage` — so `aizen chat`
    // turns never reached `/cost` on providers that only send usage when asked.
    let body = build_chat_body_for_destination(
        cfg,
        model,
        &messages,
        &[],
        true,
        effort,
        policy,
        Some(&crate::llm::media::destination_key(base_url, model)),
    );

    let mut spin = Some(crate::ui::spinner::Spinner::start("thinking"));

    let resp = match send_chat(client, &url, api_key, body).await {
        Ok(r) => r,
        Err(e) => {
            spin.take();
            return Err(e);
        }
    };

    let correlation = response_correlation(&resp);
    let mut stream = bounded_sse_bytes(resp.bytes_stream()).eventsource();
    let mut full = String::new();
    let decorate = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let md = crate::ui::markdown::MarkdownStream::new(decorate, crate::ui::tui::width());
    let mut tail = StreamTail::new(
        cost_meter(),
        md,
        |text: &str| {
            print!("{text}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        },
        true,
        false,
        true,
    );

    // Capture a mid-stream transport error and break rather than `?`-ing out, so the closing
    // newline below still runs (a clean line break before the error surfaces) — same invariant
    // as `stream_chat_with_tools`.
    let mut stream_err: Option<anyhow::Error> = None;
    // Same stall watchdog as the tool-calling path: a provider that answers 200 and then goes quiet
    // would otherwise park this loop until reqwest's 300s read timeout — and only if it withholds
    // BYTES, which keepalive frames defeat entirely. No replay here: this one-shot surface has no
    // eager handles to detach and its caller already prints the error.
    let mut watchdog = super::stream_watch::Watchdog::new(
        stream_first_frame_timeout(),
        stream_stall_timeout(),
    );
    loop {
        let event = match watchdog.wait(stream.next()).await {
            Ok(Some(Ok(e))) => e,
            Ok(Some(Err(error))) => {
                let (failure, _) = sse_read_failure(error, correlation.clone());
                stream_err = Some(failure.into());
                break;
            }
            Ok(None) => break,
            // A validated finish with the socket still open is a completed answer: wait only the
            // bounded trailer window (which still surfaces late errors/usage), never a full window.
            Err(_) if watchdog.finished() => break,
            Err(_) => {
                stream_err = Some(anyhow!(
                    "SSE stream timeout waiting for {}",
                    watchdog.stage()
                ));
                break;
            }
        };

        if event.event == "error" {
            stream_err = Some(transport_failure("sse", Some(200), &event.data, None).into());
            break;
        }
        // OpenAI signals end-of-stream with a literal `data: [DONE]`.
        if event.data.trim() == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }

        match parse_chunk(&event.data) {
            Ok(chunk) => {
                watchdog.observe(&chunk);
                // Keep the LAST usage report whatever chunk carries it — the final empty-choices
                // chunk (OpenAI), the last content chunk (llama.cpp, Ollama shims), or a cumulative
                // object on every chunk (vLLM/LiteLLM/OpenRouter). Recorded once, after the loop.
                if let Some(u) = &chunk.usage {
                    tail.observe_usage(u);
                }
                if let Some(choice) = chunk.choices.first() {
                    if let Some(content) = &choice.delta.content {
                        spin.take(); // clear before the first token prints
                        tail.push(content);
                        full.push_str(content);
                    }
                }
            }
            Err(e) => {
                stream_err = Some(e);
                break;
            }
        }
    }

    spin.take();
    tail.finish(stream_err.is_some());
    if let Some(e) = stream_err {
        return Err(e);
    }
    Ok(full)
}

/// Build the `/chat/completions` request body — the ONE place the OpenAI-compatible wire shape is
/// assembled, shared by the streaming and non-streaming paths.
///
/// The registry's tool definitions go out through the provider's NATIVE `tools` field (each a
/// `{"type":"function","function":{name,description,parameters}}` object with a real JSON Schema);
/// they are never rendered into the system prompt as prose. `tool_choice`/`parallel_tool_calls` are
/// omitted entirely when the surface is empty so a plain chat request stays byte-identical to one
/// from a build without tools.
///
/// Anthropic is served through this same path: `api.anthropic.com/v1` exposes an OpenAI-compatible
/// chat-completions surface, and [`with_provider_auth`] adds its native `x-api-key` +
/// `anthropic-version` headers. The ChatGPT Codex endpoint does NOT come through here — it speaks the
/// Responses dialect and is branched off upstream to
/// [`crate::llm::responses_codex::build_request_body`], which maps the same `ToolDef`s onto that
/// provider's own native tool shape.
///
/// `cfg` is passed in rather than re-read: `load()` hits the filesystem, and both call sites already
/// hold the config they resolved `effort` from.
#[cfg(test)]
pub(crate) fn build_chat_body(
    cfg: &crate::core::cli_config::CliConfig,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    stream: bool,
    effort: Option<String>,
) -> ChatRequest {
    build_chat_body_with_policy(
        cfg,
        model,
        messages,
        tools,
        stream,
        effort,
        crate::llm::media::VisionPolicy::Unknown,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_chat_body_with_policy(
    cfg: &crate::core::cli_config::CliConfig,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    stream: bool,
    effort: Option<String>,
    policy: crate::llm::media::VisionPolicy,
) -> ChatRequest {
    build_chat_body_for_destination(cfg, model, messages, tools, stream, effort, policy, None)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_chat_body_for_destination(
    cfg: &crate::core::cli_config::CliConfig,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    stream: bool,
    effort: Option<String>,
    policy: crate::llm::media::VisionPolicy,
    destination: Option<&str>,
) -> ChatRequest {
    let (projected, receipt) = crate::llm::media::project_messages_for_send(
        messages,
        policy,
        destination,
        crate::llm::media::request_send_policy(),
    );
    crate::llm::media::stage_image_delivery(receipt);
    let mut msgs = projected.messages;
    let mut tool_defs = tools.to_vec();
    // Stamp on cache_enabled alone — the system/history breakpoints pay off even with no tools
    // (summaries, persona/skill learning, sub-tasks); the last-tool-def breakpoint inside is a
    // no-op when tools is empty.
    if cache_enabled(cfg.prompt_cache, model) {
        apply_cache_breakpoints(&mut msgs, &mut tool_defs);
    }
    ChatRequest {
        model: model.to_string(),
        messages: msgs,
        stream,
        temperature: None,
        max_tokens: cfg.max_tokens,
        max_completion_tokens: None,
        tools: tool_defs,
        tool_choice: if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        },
        parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
        // Streaming asks for a final usage chunk; non-streaming responses carry `usage` natively.
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
        reasoning_effort: effort,
    }
}

/// One non-streaming chat turn WITH tools advertised. Returns the assistant's content
/// and/or the tool calls it wants executed. Used by the `task` sub-agent (which runs silently
/// and returns only its final text) and the workflow fan-out; the streaming counterpart
/// (`stream_chat_with_tools`) drives the top-level `aizen agent` for live output. Both return
/// `ChatTurn`, so the loop is agnostic.
pub async fn chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    let effort = crate::core::cli_config::resolved_reasoning_effort(
        crate::core::cli_config::load().reasoning_effort,
    );
    chat_with_tools_effort(client, base_url, api_key, model, messages, tools, effort).await
}

/// `chat_with_tools` with the `reasoning_effort` decided by the CALLER rather than read from the
/// process-global override.
///
/// Concurrent `serve` lanes each carry their own effort tier: reading the global here would stamp
/// whichever lane armed it last onto every in-flight request. The two-level `Option` matches
/// [`crate::core::cli_config::resolved_reasoning_effort`] — `None` omits the field entirely, keeping
/// the request byte-identical for providers that reject it.
#[allow(clippy::option_option, clippy::too_many_arguments)]
pub async fn chat_with_tools_effort(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    effort: Option<String>,
) -> Result<ChatTurn> {
    let cfg = crate::core::cli_config::load();
    chat_with_tools_frozen(
        client, base_url, api_key, model, messages, tools, effort, &cfg,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_with_tools_frozen(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    effort: Option<String>,
    cfg: &crate::core::cli_config::CliConfig,
) -> Result<ChatTurn> {
    // Tape first: a replayed run never builds a request. See `llm::replay`.
    if let Some(turn) = crate::llm::replay::replay_turn(model, messages, tools)? {
        return Ok(turn);
    }
    let turn = chat_with_tools_effort_live(
        client, base_url, api_key, model, messages, tools, effort, cfg,
    )
    .await?;
    crate::llm::replay::record_turn(model, messages, tools, &turn)?;
    Ok(turn)
}

#[allow(clippy::option_option, clippy::too_many_arguments)]
async fn chat_with_tools_effort_live(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    effort: Option<String>,
    cfg: &crate::core::cli_config::CliConfig,
) -> Result<ChatTurn> {
    if crate::llm::oauth_codex::is_codex_base_url(base_url) {
        let _ = api_key;
        let session = std::env::var("AIZEN_CODEX_SESSION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("aizen-{}-{model}", std::process::id()));
        // The finished turn only: this path's callers print the answer themselves.
        return crate::llm::responses_codex::stream_turn_effort_frozen(
            client,
            model,
            messages,
            tools,
            &session,
            crate::llm::responses_codex::StreamSink::default(),
            effort,
            cfg,
        )
        .await;
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let _request_memory = crate::llm::media::request_memory_lease(messages)?;
    let policy = crate::core::cli_config::vision_support_in(cfg, base_url, model).into();
    crate::llm::media::validate_reference_vision(policy)?;
    let body = build_chat_body_for_destination(
        cfg,
        model,
        messages,
        tools,
        false,
        effort,
        policy,
        Some(&crate::llm::media::destination_key(base_url, model)),
    );

    let resp = send_chat(client, &url, api_key, body).await?;

    // Bytes first, parse second: when the body does not fit `ChatResponse`, the parse error gets the
    // first bytes attached — otherwise serde discards the evidence and the failure is a bare
    // "missing field"/"invalid type" with no way to see what the gateway actually sent.
    let raw = resp
        .bytes()
        .await
        .context("reading chat-completions response body")?;
    let mut parsed: ChatResponse = serde_json::from_slice(&raw).with_context(|| {
        format!(
            "parsing chat-completions response (first 400 B: {})",
            summarize_bytes(&raw, 400)
        )
    })?;
    if let Some(u) = &parsed.usage {
        cost_meter().record(u);
    }
    let usage = parsed.usage;
    let choice = parsed
        .choices
        .first_mut()
        .ok_or_else(|| anyhow!("response had no choices"))?;
    // A gateway answering non-streaming with `content: [{"type":"text",…}]` (the parts spelling)
    // used to fail the whole message parse here — `content` was `Option<String>`, and a sequence
    // did not fit it. `RespMessage`'s hand-rolled `Deserialize` now joins text parts at the
    // boundary, so by this point `content` already carries the text regardless of spelling.
    let content = resolve_content(&choice.message);
    let empty_wire_note = if choice.message.tool_calls.is_empty()
        && content.as_deref().is_none_or(|s| s.trim().is_empty())
    {
        Some(empty_wire_note(
            &choice.message,
            choice.finish_reason.as_deref(),
            &choice.extra,
            usage.as_ref(),
        ))
    } else {
        None
    };
    // Captured on the same condition as the note, bounded + ASCII-escaped (never raw control
    // bytes). NOT printed here: this function is called per retry, and a mid-loop reply can carry
    // user data that must not scroll by in a transcript nobody is watching. The retry loop prints
    // it exactly once — attached to the error when the budget dies.
    let empty_wire_raw = empty_wire_note.as_ref().map(|_| summarize_bytes(&raw, 400));
    Ok(ChatTurn {
        content,
        tool_calls: std::mem::take(&mut choice.message.tool_calls),
        finish_reason: choice.finish_reason.clone(),
        usage,
        empty_wire_note,
        empty_wire_raw,
        eager: Vec::new(),
    })
}

/// A printable prefix of a response body: ASCII as-is (escaped), non-ASCII as `len=N bytes`.
/// Never more than `cap` bytes, and never raw control bytes into a transcript or error chain.
fn summarize_bytes(raw: &[u8], cap: usize) -> String {
    let head = &raw[..raw.len().min(cap)];
    let mut out = String::with_capacity(head.len() + 32);
    for &b in head {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    if raw.len() > cap {
        out.push_str(&format!("… ({} B total)", raw.len()));
    }
    out
}

/// One line naming everything an empty-looking turn DID carry, so the report distinguishes
/// "the provider went silent" (`finish_reason=stop, usage=…, no unknown keys` — a real silence)
/// from "the provider answered in a shape we do not read" (unknown keys listed by name).
fn empty_wire_note(
    m: &crate::core::types::RespMessage,
    finish_reason: Option<&str>,
    choice_extra: &BTreeMap<String, serde_json::Value>,
    usage: Option<&Usage>,
) -> String {
    let mut bits: Vec<String> = Vec::new();
    bits.push(format!(
        "finish_reason={}",
        finish_reason.unwrap_or("<absent>")
    ));
    if !m.extra.is_empty() {
        bits.push(format!(
            "message keys [{}]",
            m.extra.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !choice_extra.is_empty() {
        bits.push(format!(
            "choice keys [{}]",
            choice_extra.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if m.reasoning_content.is_some() || m.reasoning.is_some() {
        // Blank-whitespace reasoning is not text (reasoning_text() already filtered it) but it IS
        // evidence the model generated something — name it.
        bits.push("reasoning channel present but whitespace-only".to_string());
    }
    match usage {
        Some(u) => bits.push(format!(
            "usage prompt={} completion={}",
            u.prompt_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into()),
            u.completion_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into())
        )),
        None => bits.push("no usage".to_string()),
    }
    bits.join(", ")
}

/// The message's text, falling back to its REASONING channel when `content` is empty.
///
/// Some providers put the whole reply in `reasoning_content` (DeepSeek) / `reasoning` (OpenRouter)
/// and leave `content` null. With no tool calls that deserializes to "no text, no calls" — which
/// every caller classifies as an empty-200 provider failure — so the loop retried an identical
/// request, got the identical "empty" answer, and burned its whole budget before reporting silence.
/// The model DID speak.
///
/// The asymmetry only bit SUB-AGENTS: [`Delta`] (streaming) has always read this field, and a
/// sub-agent is the one caller that runs exclusively on the non-streaming [`chat_with_tools`]. The
/// same model answering the same way over the streaming path was fine, which is why this looked like
/// "sub-agents get empty responses" rather than a provider-shape gap.
///
/// Narrow on purpose. The fallback applies ONLY when there are no tool calls AND `content` is
/// absent/blank, so a normal turn is untouched and reasoning is never APPENDED to real content —
/// appending would leak the chain-of-thought the CLI deliberately suppresses on the streaming path.
/// A blank `content` alongside tool calls stays blank: that is the canonical tool-call turn shape.
fn resolve_content(m: &crate::core::types::RespMessage) -> Option<String> {
    let has_text = m.content.as_deref().is_some_and(|s| !s.trim().is_empty());
    if has_text || !m.tool_calls.is_empty() {
        return m.content.clone();
    }
    m.reasoning_text()
        .map(str::to_string)
        .or_else(|| m.content.clone())
}

/// Reassembles streamed `delta.tool_calls[]` fragments into whole `ToolCall`s, keyed by index.
/// `id`/`name` land once (first fragment for an index); `arguments` pieces concatenate.
///
/// Robustness: spec-compliant providers tag every fragment with an `index`, but `ToolCallDelta.index`
/// is `#[serde(default)]` → a provider that OMITS index sends every fragment as index 0. Two distinct
/// parallel tool calls would then merge onto slot 0 (second id/name clobbers the first; the two
/// argument strings concatenate into one broken JSON blob). So when a fragment carries a non-empty
/// `id`, we route by id (via `id_index`) and allocate a fresh slot if index 0 is already claimed by a
/// different id — keeping the common compliant case (id+index first, then index-only arg fragments)
/// unchanged.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<usize, AccCall>,
    /// Maps a seen tool-call `id` → the slot key it owns (for index-omitting providers).
    id_index: std::collections::HashMap<String, usize>,
    /// The slot the CURRENT fragment stream is filling — a fragment landing on a different slot
    /// means the previous call's arguments are complete (spec: one call streams contiguously).
    active: Option<usize>,
}

#[derive(Default)]
struct AccCall {
    id: String,
    name: String,
    args: String,
}

/// Are these streamed `arguments` whole enough to RUN — i.e. do they parse as a JSON object?
///
/// The empty string is deliberately NOT complete. A genuinely argument-less call still streams
/// `"{}"`; `""` only ever means "the arguments haven't arrived yet". Treating it as complete is what
/// let a truncated fragment reach a tool as `{}` (see [`ToolCallAccumulator::snapshot`]).
pub(crate) fn args_complete(raw: &str) -> bool {
    let t = raw.trim();
    !t.is_empty()
        && serde_json::from_str::<serde_json::Value>(t)
            .map(|v| v.is_object())
            .unwrap_or(false)
}

impl ToolCallAccumulator {
    fn has_incomplete(&self) -> bool {
        self.calls
            .values()
            .any(|c| c.name.is_empty() || !args_complete(&c.args))
    }

    /// Merge a batch of fragments. Returns the calls whose arguments COMPLETED because a later
    /// slot started (the eager-execution trigger). The FINAL call only completes at stream end —
    /// `finish`/`finish_indexed` covers it.
    pub fn ingest(&mut self, deltas: &[ToolCallDelta]) -> Vec<(usize, ToolCall)> {
        let mut completed = Vec::new();
        for d in deltas {
            // Resolve which slot this fragment belongs to. Default: key by `index`. But a non-empty
            // `id` takes precedence — if we've seen the id, reuse its slot; if it's new and `d.index`
            // is already held by a DIFFERENT id (the index-omitting collision), allocate a fresh slot.
            let key = match d.id.as_deref().filter(|s| !s.is_empty()) {
                Some(id) => {
                    if let Some(&k) = self.id_index.get(id) {
                        k
                    } else {
                        let collides = self
                            .calls
                            .get(&d.index)
                            .map(|e| !e.id.is_empty() && e.id != id)
                            .unwrap_or(false);
                        let k = if collides {
                            self.calls.keys().last().map(|m| m + 1).unwrap_or(d.index)
                        } else {
                            d.index
                        };
                        self.id_index.insert(id.to_string(), k);
                        k
                    }
                }
                // An `arguments` fragment carries NO id (only the opening fragment does). If the
                // provider also omits `index`, `d.index` defaults to 0 — so the second call's
                // arguments would pour into slot 0 instead of the slot its id was rerouted to, both
                // corrupting slot 0's JSON and bouncing `active` (which snapshots a half-built
                // call). An id-less fragment belongs to the call currently streaming: the spec
                // guarantees one call streams contiguously. Kept narrow (`index == 0` and an active
                // slot that isn't 0) so compliant providers — where that fragment carries its real
                // `index: 1` — are routed exactly as before.
                None => match self.active {
                    Some(a) if d.index == 0 && a != 0 => a,
                    _ => d.index,
                },
            };
            if let Some(prev) = self.active {
                if prev != key {
                    if let Some(tc) = self.snapshot(prev) {
                        completed.push((prev, tc));
                    }
                }
            }
            self.active = Some(key);
            let e = self.calls.entry(key).or_default();
            if let Some(id) = &d.id {
                if !id.is_empty() {
                    e.id = id.clone();
                }
            }
            if let Some(f) = &d.function {
                if let Some(n) = &f.name {
                    if !n.is_empty() {
                        e.name = n.clone();
                    }
                }
                if let Some(a) = &f.arguments {
                    e.args.push_str(a);
                }
            }
        }
        completed
    }

    /// A clone of slot `key`'s call as accumulated so far (None when it has no name yet, or when its
    /// arguments are not yet a WHOLE JSON object). The id may still be empty here — eager adoption
    /// keys by POSITION, not id, so that's fine.
    ///
    /// The args-completeness check is what keeps a half-streamed call from being dispatched: the
    /// "a fragment landed on another slot ⇒ the previous call is done" rule is only true for
    /// providers that tag every fragment with an `index`. When one doesn't, an argument fragment
    /// (which carries no `id`) defaults to index 0 and can bounce the active slot, snapshotting a
    /// call whose `args` is still `""` or a partial `{"path":`. An empty string then reads as a
    /// valid `{}` in `parse_call_args`, so the tool would run with NO ARGUMENTS and fail with
    /// `missing required string arg` — while the transcript line, built later from the complete
    /// call, showed the argument right there. Refusing to snapshot costs only the eager head start
    /// (the call still runs normally in `execute_calls` with its full arguments).
    fn snapshot(&self, key: usize) -> Option<ToolCall> {
        let c = self.calls.get(&key)?;
        if c.name.is_empty() || !args_complete(&c.args) {
            return None;
        }
        Some(ToolCall {
            id: c.id.clone(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: c.name.clone(),
                arguments: c.args.clone(),
            },
        })
    }

    /// Finalize, keeping each call's SLOT KEY alongside — the eager path maps slot → final
    /// position through this (positions are what the executor stitches by).
    ///
    /// Like [`snapshot`], we reject calls whose arguments are not a complete JSON object.  A stream
    /// that was cut in the middle of `arguments` leaves `args` as `""` or a partial fragment; both
    /// fail `args_complete`.  Dispatching them would produce a spurious `missing required arg` error
    /// (and a wasted round-trip) even though the model would have sent correct arguments had the
    /// stream finished.  We drop the truncated call instead — the executor will see it is missing
    /// from the results and the model retries with full context.
    pub fn finish_indexed(self) -> Vec<(usize, ToolCall)> {
        self.calls
            .into_iter()
            .filter(|(_, c)| !c.name.is_empty() && args_complete(&c.args))
            .enumerate()
            .map(|(i, (k, c))| {
                (
                    k,
                    ToolCall {
                        id: if c.id.is_empty() {
                            format!("call_{i}")
                        } else {
                            c.id
                        },
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: c.name,
                            arguments: c.args,
                        },
                    },
                )
            })
            .collect()
    }

    /// Slot-key-free finalize (the streaming path itself uses `finish_indexed` for eager-handle
    /// position mapping; this stays as the plain API + test surface).
    #[allow(dead_code)]
    pub fn finish(self) -> Vec<ToolCall> {
        self.finish_indexed()
            .into_iter()
            .map(|(_, tc)| tc)
            .collect()
    }
}

/// Streaming variant of `chat_with_tools`: prints assistant content deltas live as they
/// arrive AND reassembles any tool-call fragments, returning the same `ChatTurn` the loop
/// consumes. (The loop is unchanged — only the live UX differs.)
#[expect(
    dead_code,
    reason = "compatibility streaming entry point; production dispatch passes explicit per-call effort"
)]
pub async fn stream_chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    stream_chat_with_tools_eager(client, base_url, api_key, model, messages, tools, None).await
}

/// [`stream_chat_with_tools`] plus EAGER tool execution: each tool call is offered to `eager_hook`
/// the moment its streamed arguments complete, so read-only work overlaps the rest of the
/// generation (first-tool latency hides inside stream time). Handles come back on
/// `ChatTurn.eager` keyed by final POSITION; a mid-stream transport error drops them (detached —
/// eager calls are read-only by policy, so discarding is safe).
pub async fn stream_chat_with_tools_eager(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    eager_hook: Option<EagerStartFn<'_>>,
) -> Result<ChatTurn> {
    let effort = crate::core::cli_config::resolved_reasoning_effort(
        crate::core::cli_config::load().reasoning_effort,
    );
    stream_chat_with_tools_eager_effort(
        client, base_url, api_key, model, messages, tools, eager_hook, effort,
    )
    .await
}

/// Streaming without consulting process-global effort overrides. `None` explicitly omits effort.
#[expect(
    dead_code,
    reason = "compatibility streaming API; dispatch uses explicit frozen sinks"
)]
#[allow(clippy::too_many_arguments)]
pub async fn stream_chat_with_tools_effort(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    effort: Option<String>,
) -> Result<ChatTurn> {
    stream_chat_with_tools_eager_effort(
        client, base_url, api_key, model, messages, tools, None, effort,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantTextSink {
    Events,
    Retained,
    Markdown,
}

fn assistant_text_sink(events: bool, retained: bool) -> AssistantTextSink {
    if events {
        AssistantTextSink::Events
    } else if retained {
        AssistantTextSink::Retained
    } else {
        AssistantTextSink::Markdown
    }
}

fn emit_assistant_delta(
    text: &str,
    sink: AssistantTextSink,
    md: &mut crate::ui::markdown::MarkdownStream,
) {
    match sink {
        AssistantTextSink::Events => crate::ui::events::text(text),
        AssistantTextSink::Retained => crate::ui::tui::assistant_stream_delta(text),
        AssistantTextSink::Markdown => {
            let rendered = md.push(text);
            if !rendered.is_empty() {
                crate::ui::tui::emit(&rendered);
            }
        }
    }
}

fn finish_assistant_text(
    sink: AssistantTextSink,
    md: &mut crate::ui::markdown::MarkdownStream,
    interrupted: bool,
    had_text: bool,
) {
    match sink {
        AssistantTextSink::Events => {}
        AssistantTextSink::Retained => crate::ui::tui::assistant_stream_finish(interrupted),
        AssistantTextSink::Markdown => {
            let closing = md.finish();
            if !closing.is_empty() {
                crate::ui::tui::emit(&closing);
            }
            if had_text {
                crate::ui::tui::emit("\n");
            }
        }
    }
}

/// Only selected answer text enters this funnel, not suppressed reasoning or tool arguments.
/// A taped answer is already selected/filtered content, and never starts an eager tool here.
fn emit_completed_assistant_text_with(
    text: Option<&str>,
    sink: AssistantTextSink,
    mut emit: impl FnMut(AssistantTextSink, &str),
) {
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        emit(sink, text);
    }
}

fn emit_completed_assistant_text(text: Option<&str>) {
    let sink = assistant_text_sink(crate::ui::events::on(), crate::ui::tui::retained_active());
    emit_completed_assistant_text_with(text, sink, |sink, text| {
        let decorate = sink == AssistantTextSink::Markdown
            && (crate::ui::tui::active() || std::io::IsTerminal::is_terminal(&std::io::stdout()));
        let mut md = crate::ui::markdown::MarkdownStream::new(decorate, crate::ui::tui::width());
        crate::ui::tui::add_stream_chars(text.chars().count() as u64);
        emit_assistant_delta(text, sink, &mut md);
        finish_assistant_text(sink, &mut md, false, true);
    });
}

fn reasoning_only_answer<'a>(full: &str, has_tools: bool, reasoning: &'a str) -> Option<&'a str> {
    let text = reasoning.trim();
    (full.is_empty() && !has_tools && !text.is_empty()).then_some(text)
}

#[allow(clippy::too_many_arguments)]
pub async fn stream_chat_with_tools_eager_effort(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    eager_hook: Option<EagerStartFn<'_>>,
    effort: Option<String>,
) -> Result<ChatTurn> {
    stream_chat_with_tools_sink_effort(
        client,
        base_url,
        api_key,
        model,
        messages,
        tools,
        StreamSink {
            render: true,
            eager: eager_hook,
            observed_usage: None,
        },
        effort,
    )
    .await
}

/// One immutable, per-call display/eager policy. Quiet children use `StreamSink::default()`.
/// Rendering is never controlled through process-global switches.
pub use crate::llm::responses_codex::StreamSink;

#[allow(clippy::too_many_arguments)]
pub async fn stream_chat_with_tools_sink_effort(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    sink: StreamSink<'_>,
    effort: Option<String>,
) -> Result<ChatTurn> {
    let cfg = crate::core::cli_config::load();
    stream_chat_with_tools_sink_frozen(
        client, base_url, api_key, model, messages, tools, sink, effort, &cfg,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_chat_with_tools_sink_frozen(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    sink: StreamSink<'_>,
    effort: Option<String>,
    cfg: &crate::core::cli_config::CliConfig,
) -> Result<ChatTurn> {
    if let Some(turn) =
        crate::llm::replay::replay_turn_with_quiet(model, messages, tools, !sink.render)?
    {
        return Ok(complete_replayed_turn(turn, sink));
    }
    let turn = stream_chat_with_tools_eager_live(
        client, base_url, api_key, model, messages, tools, sink, effort, cfg,
    )
    .await?;
    crate::llm::replay::record_turn_with_quiet(model, messages, tools, &turn, !sink.render)?;
    Ok(turn)
}

/// Replay already records its own usage. Only forward the observation, never re-bill the meter.
fn complete_replayed_turn(turn: ChatTurn, sink: StreamSink<'_>) -> ChatTurn {
    if let (Some(observer), Some(usage)) = (sink.observed_usage, turn.usage.as_ref()) {
        observer(usage);
    }
    if sink.render {
        emit_completed_assistant_text(turn.content.as_deref());
    }
    turn
}

#[expect(
    clippy::too_many_arguments,
    reason = "live transport mirrors the explicit-effort streaming entry point without interface refactoring"
)]
async fn stream_chat_with_tools_eager_live(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    sink: StreamSink<'_>,
    effort: Option<String>,
    cfg: &crate::core::cli_config::CliConfig,
) -> Result<ChatTurn> {
    // Experimental ChatGPT Codex path — full Responses dialect (not /chat/completions).
    if crate::llm::oauth_codex::is_codex_base_url(base_url) {
        let _ = api_key; // bearer comes from the OAuth token store
        let session = std::env::var("AIZEN_CODEX_SESSION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("aizen-{}-{model}", std::process::id()));
        // Same contract as the chat-completions stream below: text paints as it arrives,
        // completed calls go to the eager starter, the shared watchdog bounds a stall.
        return crate::llm::responses_codex::stream_turn_effort_frozen(
            client, model, messages, tools, &session, sink, effort, cfg,
        )
        .await;
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let _request_memory = crate::llm::media::request_memory_lease(messages)?;
    let policy = crate::core::cli_config::vision_support_in(cfg, base_url, model).into();
    crate::llm::media::validate_reference_vision(policy)?;
    // The caller freezes config for the entire dispatch, including Codex/media routing.
    let body = build_chat_body_for_destination(
        cfg,
        model,
        messages,
        tools,
        true,
        effort,
        policy,
        Some(&crate::llm::media::destination_key(base_url, model)),
    );
    let mut prepared = PreparedChatRequest::new(body)?;

    // BLANK-STREAM REPLAY. The request layer (`send_with_retry`) only covers failures that happen
    // before the 200; a stream that is accepted and then dies or goes silent used to leave the turn
    // parked forever with nothing to show. Retry the whole call, but ONLY while the stream has
    // produced literally nothing (no text, no tool-call fragment, no usage): re-sending after
    // partial output would duplicate text in the transcript or resurrect a half-streamed tool call.
    // Once anything has arrived we fall through to the existing error path and let the caller decide.
    let mut blank_attempt: u32 = 0;
    loop {
        // Spinner during the "thinking" gap: from request send until the first token / tool delta
        // streams back. TTY-only (silent no-op on pipes/CI). Cleared before any output is printed.
        // Suppressed under the sticky TUI — its box shows the "⚡ working…" indicator instead, and a
        // carriage-return spinner would fight the pinned footer.
        let mut spin = if !sink.render || crate::ui::tui::active() {
            None
        } else {
            Some(crate::ui::spinner::Spinner::start("thinking"))
        };

        let resp = match send_prepared_chat(client, &url, api_key, &mut prepared, sink.render).await
        {
            Ok(r) => r,
            Err(e) => {
                spin.take(); // clear the spinner before surfacing the error
                return Err(e);
            }
        };

        let correlation = response_correlation(&resp);
        let mut stream = bounded_sse_bytes(resp.bytes_stream()).eventsource();
        let mut full = String::new();
        let mut done = false;
        let mut provider_failed = false;
        let mut payload_bytes = 0usize;
        let mut acc = ToolCallAccumulator::default();
        let mut finish_reason: Option<String> = None;
        let mut final_usage: Option<Usage> = None; // the final-chunk usage report, threaded to the loop
                                                   // Eager starts, keyed by accumulator SLOT until the final position mapping exists.
        let mut eager_by_slot: std::collections::HashMap<
            usize,
            crate::agent::tools::StartedSafeCall,
        > = std::collections::HashMap::new();
        let mut think = ThinkFilter::default(); // suppress `<think>…</think>` reasoning from the display
                                                // Retained mode owns raw assistant blocks and reparses the whole active message; classic/one-shot
                                                // paths keep the line-buffered display renderer. History always keeps the same raw Markdown.
        let retained_display = sink.render && crate::ui::tui::retained_active();
        let decorate = sink.render
            && !retained_display
            && (crate::ui::tui::active() || std::io::IsTerminal::is_terminal(&std::io::stdout()));
        let cols = if sink.render {
            crate::ui::tui::width()
        } else {
            80
        };
        let md = crate::ui::markdown::MarkdownStream::new(decorate, cols);
        let mut tail_guard = StreamTail::new(
            cost_meter(),
            md,
            crate::ui::tui::emit,
            sink.render,
            retained_display,
            false,
        )
        .with_usage_observer(sink.observed_usage);
        tail_guard.events = sink.render && crate::ui::events::on();

        // A mid-stream transport error (timeout, gateway drop, truncated body) must NOT short-circuit
        // with `?` here: that would skip the `think.finish()` / `md.finish()` cleanup below, stranding
        // the terminal with a half-rendered line or an UNCLOSED code-fence box that corrupts every
        // subsequent turn. Capture the error, break, flush the display to a clean state, THEN propagate.
        let mut stream_err: Option<anyhow::Error> = None;
        // Bytes and parseable metadata are not model progress. Keep the generous first-output
        // allowance until content/reasoning/tool fragments arrive, then enforce an idle deadline.
        let mut watchdog = super::stream_watch::Watchdog::new(
            stream_first_frame_timeout(),
            stream_stall_timeout(),
        );
        // Has this stream produced anything a retry would duplicate? Text, a tool-call fragment, or a
        // usage report all count. Governs both the watchdog's error wording and blank-stream replay.
        let mut produced = false;
        // The reasoning channel, accumulated but never displayed. If the model puts its whole answer
        // there and leaves `content` empty — the historical "provider spoke, we reported silence" bug,
        // fixed on the non-streaming path in `resolve_content` — the streaming path must not
        // re-introduce it. Deliberately NOT counted as `produced`: nothing from this buffer reaches
        // the display or history mid-stream, so a replay after a mid-reasoning stall duplicates nothing.
        let mut reasoning = String::new();
        loop {
            let event = match watchdog.wait(stream.next()).await {
                Ok(Some(Ok(e))) => e,
                Ok(Some(Err(error))) => {
                    let (failure, terminal) = sse_read_failure(error, correlation.clone());
                    provider_failed |= terminal; // no blank retry or tool salvage on parser/limit.
                    stream_err = Some(failure.into());
                    break;
                }
                Ok(None) => break, // stream ended without [DONE]
                Err(_) if watchdog.finished() => break,
                Err(_) => {
                    stream_err = Some(anyhow!(
                        "SSE stream timeout waiting for {}",
                        watchdog.stage()
                    ));
                    break;
                }
            };
            payload_bytes = payload_bytes.saturating_add(event.data.len());
            if payload_bytes > 16 * 1024 * 1024 {
                provider_failed = true;
                stream_err = Some(anyhow!("SSE stream error: response exceeds 16 MiB limit"));
                break;
            }
            if event.event == "error" {
                provider_failed = true;
                stream_err = Some(
                    transport_failure("sse", Some(200), &event.data, correlation.clone()).into(),
                );
                break;
            }
            if event.data.trim() == "[DONE]" {
                done = true;
                break;
            }
            if event.data.trim().is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&event.data) {
                if value.get("error").is_some_and(|e| !e.is_null())
                    || value.get("type").and_then(|v| v.as_str()) == Some("error")
                {
                    provider_failed = true;
                    stream_err = Some(
                        transport_failure("sse", Some(200), &event.data, correlation.clone())
                            .into(),
                    );
                    break;
                }
            }
            match parse_chunk(&event.data) {
                Ok(chunk) => {
                    if watchdog.observe(&chunk) {
                        super::delegated::progress();
                    }
                    // Keep the LAST usage report whatever chunk carries it: the final empty-choices
                    // chunk (spec OpenAI), the last content chunk (llama.cpp, Ollama shims, LiteLLM
                    // in some modes — these never registered before, so `/cost` stayed blank and
                    // the real-usage anchor never armed), or a cumulative object on EVERY chunk
                    // (vLLM/OpenRouter). It is recorded ONCE after the loop, so the cumulative
                    // shape is not summed N times. A report with completion tokens, or the final
                    // chunk shape, proves the call was billed — a zero-token report on the role
                    // chunk does not, and must not disarm the blank-stream replay.
                    if let Some(u) = &chunk.usage {
                        final_usage = Some(u.clone());
                        tail_guard.observe_usage(u);
                        if chunk.choices.is_empty() || u.completion_tokens.unwrap_or(0) > 0 {
                            produced = true;
                        }
                    }
                    if let Some(choice) = chunk.choices.first() {
                        // A dedicated reasoning channel (`reasoning_content`/`reasoning`) is the model
                        // thinking out loud — suppressed from the display so output is uniform across
                        // models, but ACCUMULATED: see `reasoning` above for the empty-content salvage.
                        // Raw fragments, not `reasoning_text()` — that helper trims each delta, which
                        // would eat the spaces between streamed words. `.or()` reads whichever spelling
                        // arrived; a gateway that mirrors both carries the same text, not more of it.
                        // (Clear the spinner: the model IS producing, just not user-facing text yet.)
                        if let Some(raw) = choice
                            .delta
                            .reasoning_content
                            .as_deref()
                            .or(choice.delta.reasoning.as_deref())
                        {
                            if !raw.trim().is_empty() {
                                spin.take();
                            }
                            reasoning.push_str(raw);
                            if sink.render {
                                crate::ui::events::reasoning(raw);
                            }
                        }
                        if let Some(content) = &choice.delta.content {
                            spin.take(); // stop+clear the spinner before the first token prints
                            // Nonempty text is real output even when the display filter drops it
                            // (`<think>` spans). An EMPTY delta is metadata: it must not disarm the
                            // blank-stream replay, or a provider that only ever emits empty deltas
                            // would be reported as a finished answer instead of being retried.
                            produced |= !content.is_empty();
                            let shown = think.push(content);
                            if !shown.is_empty() {
                                full.push_str(&shown); // history keeps the RAW markdown
                                if sink.render {
                                    crate::ui::tui::add_stream_chars(shown.chars().count() as u64);
                                }
                                tail_guard.push(&shown);
                            }
                        }
                        if !choice.delta.tool_calls.is_empty() {
                            spin.take(); // a tool-only turn: clear before the loop prints tool traces
                            produced = true; // a tool-call fragment must never be re-requested
                            let completed = acc.ingest(&choice.delta.tool_calls);
                            if let Some(hook) = sink.eager {
                                for (slot, tc) in completed {
                                    if let Some(h) = hook(slot, &tc) {
                                        eager_by_slot.insert(slot, h);
                                    }
                                }
                            }
                        }
                        if choice.finish_reason.is_some() {
                            finish_reason = choice.finish_reason.clone();
                        }
                    }
                }
                Err(e) => {
                    provider_failed = true;
                    stream_err = Some(e);
                    break;
                }
            }
        }
        if !provider_failed && stream_err.is_none() {
            if acc.has_incomplete() {
                provider_failed = true; // invalid tool structures cannot be salvaged as Done.
                stream_err = Some(anyhow!("SSE stream error: incomplete tool arguments"));
            } else if !matches!(
                finish_reason.as_deref(),
                Some("stop" | "tool_calls" | "function_call")
            ) {
                stream_err = Some(anyhow!(if done {
                    "SSE stream error: [DONE] without a validated finish reason"
                } else {
                    "SSE stream error: partial EOF without a validated finish reason"
                }));
            }
        }
        spin.take();
        let tail = think.finish();
        if !tail.is_empty() {
            full.push_str(&tail);
            tail_guard.push(&tail);
        }
        tail_guard.finish(stream_err.is_some());

        // BLANK STREAM ⇒ REPLAY, not a dead turn. A stall or drop that produced NOTHING is exactly the
        // case the user hits as "it froze; I killed the turn and told it to continue, then it was fine" —
        // the manual retry worked because there was nothing wrong with the request. Do that retry here.
        // Guarded on `!produced`, so a stream that emitted any text/tool-fragment/usage never replays
        // (that would duplicate output). Also skipped once the model claimed `finish_reason`: an
        // intentionally empty completion is a real answer, not a failure.
        let blank_failure = !provider_failed && !produced && finish_reason.is_none();
        if blank_failure && blank_attempt < STREAM_BLANK_RETRIES {
            blank_attempt += 1;
            let delay = backoff_ms(blank_attempt - 1, 400, 4_000);
            let why = stream_err
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "stream closed with no output".to_string());
            if sink.render {
                stream_retry_note(&why, blank_attempt, STREAM_BLANK_RETRIES, delay);
            }
            // Abort any eager handles from the abandoned attempt. They are read-only by policy (the
            // eager starter refuses destructive/unsafe calls), so cutting them short is safe — and
            // aborting rather than detaching means they stop billing/working for a turn that no
            // longer exists. (A handle backed by `spawn_blocking` finishes its current body; abort
            // still prevents anything after it.)
            for h in eager_by_slot.into_values() {
                h.abort();
            }
            super::delegated::sleep(std::time::Duration::from_millis(delay)).await?;
            continue;
        }

        // Map eager handles from accumulator SLOT to final POSITION (what the executor stitches by).
        // COMPLETE calls only: `finish_indexed` drops any call whose streamed arguments never closed,
        // so everything here is executable regardless of how the stream ended.
        let has_incomplete = acc.has_incomplete();
        let indexed = acc.finish_indexed();

        // The display is now clean (fence closed, partial line flushed) — deal with a transport error
        // that interrupted the stream. Two cases:
        //  * At least one COMPLETE tool call arrived → SALVAGE the turn: return it as a tools turn.
        //    The executor runs the calls and the next request continues naturally — strictly better
        //    than discarding finished work and re-paying the whole generation. The partial text (if
        //    any) rides along: it is already on screen, and history matching the display beats a
        //    retry that re-prints it.
        //  * No complete call → fail the turn. Partial TEXT alone is not returned as success: that
        //    would present a cut-off answer as a finished one.
        if let Some(e) = stream_err {
            if !provider_failed && has_incomplete {
                provider_failed = true;
                for handle in eager_by_slot.values() {
                    handle.abort();
                }
            }
            if indexed.is_empty() || provider_failed {
                if produced && sink.render {
                    // The loop above will likely retry this turn; the partial text stays on screen
                    // (retained blocks are append-only), so say why the answer may appear twice.
                    let note = "⚠ stream dropped mid-answer — on retry the text above may repeat \
                                on screen (the saved transcript stays clean)";
                    if crate::ui::tui::active() {
                        crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
                    } else {
                        crate::ui::tui::note_line(note);
                    }
                }
                for h in eager_by_slot.into_values() {
                    h.abort();
                }
                return Err(e);
            }
            // Completed tools can survive a transport drop, but this was not a validated live
            // completion and cannot attest that any uploaded images were delivered.
            crate::llm::media::stage_image_delivery(Default::default());
            let note = format!(
                "⟳ stream dropped mid-turn — continuing with {} completed tool call(s)",
                indexed.len()
            );
            if sink.render {
                if crate::ui::tui::active() {
                    crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
                } else {
                    crate::ui::tui::note_line(&note);
                }
            }
            if finish_reason.is_none() {
                finish_reason = Some("tool_calls".to_string());
            }
        }

        let eager: Vec<(usize, crate::agent::tools::StartedSafeCall)> = indexed
            .iter()
            .enumerate()
            .filter_map(|(pos, (slot, _))| eager_by_slot.remove(slot).map(|h| (pos, h)))
            .collect();
        // Streaming twin of `resolve_content` (non-streaming): a model that answered ONLY in the
        // reasoning channel produced an answer, not an empty turn. Same narrow gate — no real
        // content, no tool calls — so chain-of-thought is never APPENDED to a real answer.
        let fallback = reasoning_only_answer(&full, !indexed.is_empty(), &reasoning);
        let content = if !full.is_empty() {
            Some(full)
        } else if let Some(answer) = fallback {
            // Transport errors without complete tools returned above. Only the already-selected
            // successful fallback becomes answer text; reasoning never supplements a real answer.
            if sink.render {
                emit_completed_assistant_text(Some(answer));
            }
            Some(answer.to_string())
        } else {
            None
        };
        return Ok(ChatTurn {
            content,
            tool_calls: indexed.into_iter().map(|(_, tc)| tc).collect(),
            finish_reason,
            usage: final_usage,
            empty_wire_note: None,
            empty_wire_raw: None,
            eager,
        });
    } // end blank-stream replay loop
}

/// A streaming filter that suppresses `<think>…</think>` reasoning blocks from the printed output.
/// Some models emit literal think tags into the content channel (sometimes empty `<think></think>`),
/// which is visual noise in the TUI. `push` buffers across SSE deltas so a tag split between chunks
/// is still caught; it holds back only the trailing bytes that could be a partial tag, so latency
/// stays one short tag-length behind at most.
#[derive(Default)]
struct ThinkFilter {
    buf: String,
    in_think: bool,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl ThinkFilter {
    /// Feed a content delta; return the text safe to print now (think blocks removed).
    fn push(&mut self, s: &str) -> String {
        self.buf.push_str(s);
        let mut out = String::new();
        loop {
            if self.in_think {
                if let Some(i) = self.buf.find(THINK_CLOSE) {
                    self.buf.drain(..i + THINK_CLOSE.len());
                    self.in_think = false;
                } else {
                    let keep = partial_suffix(&self.buf, THINK_CLOSE);
                    let drop_to = self.buf.len() - keep;
                    self.buf.drain(..drop_to);
                    break;
                }
            } else if let Some(i) = self.buf.find(THINK_OPEN) {
                out.push_str(&self.buf[..i]);
                self.buf.drain(..i + THINK_OPEN.len());
                self.in_think = true;
            } else {
                let keep = partial_suffix(&self.buf, THINK_OPEN);
                let emit_to = self.buf.len() - keep;
                out.push_str(&self.buf[..emit_to]);
                self.buf.drain(..emit_to);
                break;
            }
        }
        out
    }

    /// Stream end: emit any buffered non-think remainder (a dangling, never-closed `<think>` is dropped).
    fn finish(&mut self) -> String {
        if self.in_think {
            return String::new();
        }
        std::mem::take(&mut self.buf)
    }
}

/// Length of the longest suffix of `s` that equals a prefix of `tag` (both compared as bytes; `tag`
/// is ASCII so any match lands on a char boundary). Used to hold back a possible partial tag.
fn partial_suffix(s: &str, tag: &str) -> usize {
    let max = tag.len().min(s.len());
    for k in (1..=max).rev() {
        if s.as_bytes()[s.len() - k..] == tag.as_bytes()[..k] {
            return k;
        }
    }
    0
}

#[test]
fn openrouter_pricing_free_detection() {
    assert!(pricing_is_free("0", "0"));
    assert!(pricing_is_free("0.0", "0.00"));
    assert!(pricing_is_free("0.00000000", "0"));
    assert!(!pricing_is_free("0", "0.0000001"));
    assert!(!pricing_is_free("abc", "0"));
}

#[test]
fn free_model_id_matches_openrouter_and_opencode() {
    assert!(is_free_model_id("meta-llama/llama-3.3-70b:free"));
    assert!(is_free_model_id("deepseek-v4-flash-free"));
    assert!(!is_free_model_id("gpt-4o"));
    assert!(!is_free_model_id(":freebie"));
}

#[test]
fn key_probe_prefers_free_pricing_over_paid() {
    // Simulate an OpenRouter list where a paid id appears before the free counterpart; the
    // probe must still choose the free one so a real free-tier credential doesn't fail as
    // "bad key".
    let infos = [
        ModelInfo {
            id: "qwen/qwen3-8b".into(),
            context_length: Some(8000),
            is_free: false,
        },
        ModelInfo {
            id: "openrouter/free".into(),
            context_length: Some(128000),
            is_free: true,
        },
    ];
    let picked = infos
        .iter()
        .find(|m| m.is_free || is_free_model_id(&m.id))
        .map(|m| m.id.as_str())
        .unwrap();
    assert_eq!(picked, "openrouter/free");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::types::FunctionDelta;

    #[tokio::test]
    async fn raw_sse_bound_rejects_unterminated_event_before_assembly_and_fuses() {
        use futures_util::stream;
        let source = stream::iter([
            Ok::<_, ()>(b"data: ".to_vec()),
            Ok(vec![b'x'; SSE_RAW_BYTES_LIMIT]),
            Ok(b"\n\ndata: [DONE]\n\n".to_vec()),
        ]);
        let mut events = bounded_sse_bytes(source).eventsource();
        let error = events.next().await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            eventsource_stream::EventStreamError::Transport(BoundedSseError::Limit)
        ));
        assert!(
            events.next().await.is_none(),
            "crossing chunk/later finish must not reach parser"
        );

        // Exact boundary is allowed; comments/framing consume the same byte budget as data.
        let mut raw = bounded_sse_bytes_with_limit(
            stream::iter([
                Ok::<_, ()>(b":ping\r\n".to_vec()),
                Ok(b"x".to_vec()),
                Ok(b"later".to_vec()),
            ]),
            7,
        );
        assert_eq!(raw.next().await.unwrap().unwrap(), b":ping\r\n");
        assert!(matches!(
            raw.next().await,
            Some(Err(BoundedSseError::Limit))
        ));
        assert!(raw.next().await.is_none());
    }

    #[tokio::test]
    async fn raw_sse_bound_preserves_split_crlf_and_multibyte_utf8() {
        use futures_util::stream;
        let wire = ": comment\r\nevent: message\r\ndata: Xin chào 世界\r\n\r\n";
        // Every UTF-8 codepoint and CRLF pair crosses transport chunk boundaries.
        let chunks = wire.as_bytes().iter().map(|byte| Ok::<_, ()>(vec![*byte]));
        let mut events =
            bounded_sse_bytes_with_limit(stream::iter(chunks), wire.len()).eventsource();
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event.event, "message");
        assert_eq!(event.data, "Xin chào 世界");
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn raw_sse_malformed_error_is_safe_terminal_and_cannot_reach_later_finish() {
        use futures_util::stream;
        let chunks = [
            Ok::<_, ()>(b"data: ".to_vec()),
            Ok(vec![0xff]),
            Ok(b"sk-secret data:image/png;base64,PRIVATE\n\ndata: [DONE]\n\n".to_vec()),
        ];
        let error = bounded_sse_bytes(stream::iter(chunks))
            .eventsource()
            .next()
            .await
            .unwrap()
            .unwrap_err();
        let (failure, terminal) = sse_read_failure(error, Some("req-utf8".into()));
        assert!(terminal);
        assert_eq!(failure.provider_code.as_deref(), Some("SSE_MALFORMED"));
        assert_eq!(failure.correlation_id.as_deref(), Some("req-utf8"));
        assert!(!failure.to_string().contains("PRIVATE"));
        assert!(!failure.to_string().contains("sk-secret"));
    }

    /// Fixture-owned raw receiver shared by both transport modules. No live provider/config.
    pub(crate) async fn raw_sse_backend(
        reply: Vec<u8>,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0u8; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|n| n.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Request-Id: req-raw-bound\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len());
                if socket.write_all(headers.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(&reply).await;
                }
            }
        });
        (base, hits, server)
    }

    pub(crate) fn raw_limit_reply(prefix: &[u8], later_finish: &[u8]) -> Vec<u8> {
        let mut reply = prefix.to_vec();
        // Short comments exercise the raw (not assembled data) cap without quadratic long-line
        // parsing in the dependency. The oversized unterminated line is covered by the unit seam.
        let comment = format!(": {}\r\n", "x".repeat(4092));
        while reply.len() <= SSE_RAW_BYTES_LIMIT {
            reply.extend_from_slice(comment.as_bytes());
        }
        reply.extend_from_slice(later_finish);
        reply
    }

    #[tokio::test]
    async fn chat_raw_limit_and_malformed_are_not_replayed_or_tool_salvaged() {
        use crate::llm::media::with_image_delivery;
        const TOOL: &[u8] = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n";
        const FINISH: &[u8] = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
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
                let http = reqwest::Client::builder().no_proxy().build().unwrap();
                let cfg = crate::core::cli_config::CliConfig::default();
                let ctx = crate::core::exec_ctx::ExecutionContext::default();
                let messages = [Message::user_with_images(
                    "inspect",
                    vec!["data:legacy".into()],
                )];
                let result = with_image_delivery(
                    &ctx,
                    &base,
                    "fixture-key",
                    "raw-bound-chat",
                    stream_chat_with_tools_eager_live(
                        &http,
                        &base,
                        "fixture-key",
                        "raw-bound-chat",
                        &messages,
                        &[],
                        StreamSink::default(),
                        None,
                        &cfg,
                    ),
                );
                let error = tokio::time::timeout(std::time::Duration::from_secs(30), result)
                    .await
                    .unwrap()
                    .err()
                    .unwrap();
                let failure = error.downcast_ref::<TransportFailure>().unwrap();
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
    async fn plain_chat_raw_limit_and_malformed_fail_closed() {
        for limit in [false, true] {
            let reply = if limit {
                raw_limit_reply(b"", b"data: [DONE]\n\n")
            } else {
                b"data: \xffPRIVATE\n\ndata: [DONE]\n\n".to_vec()
            };
            let (base, hits, server) = raw_sse_backend(reply).await;
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let cfg = crate::core::cli_config::CliConfig::default();
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                stream_chat_with_visual_contract_live(
                    &http,
                    &base,
                    "fixture-key",
                    "plain-raw-bound",
                    vec![Message::user("hi")],
                    false,
                    &cfg,
                    None,
                ),
            )
            .await
            .unwrap()
            .err()
            .unwrap();
            let failure = error.downcast_ref::<TransportFailure>().unwrap();
            assert_eq!(
                failure.provider_code.as_deref(),
                Some(if limit {
                    "SSE_RAW_LIMIT"
                } else {
                    "SSE_MALFORMED"
                })
            );
            assert!(!error.to_string().contains("PRIVATE"));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn stream_tail_closes_and_records_observed_usage_once_on_deadline_or_drop() {
        use std::cell::RefCell;
        for interrupted in [false, true] {
            let meter = CostMeter::default();
            let output = RefCell::new(String::new());
            let observations = std::sync::Mutex::new(Vec::<Usage>::new());
            let observer = |usage: &Usage| observations.lock().unwrap().push(usage.clone());
            let call = async {
                let md = crate::ui::markdown::MarkdownStream::new(false, 80);
                let mut tail = StreamTail::new(
                    &meter,
                    md,
                    |text: &str| output.borrow_mut().push_str(text),
                    true,
                    false,
                    false,
                )
                .with_usage_observer(Some(&observer));
                tail.observe_usage(&Usage {
                    prompt_tokens: Some(5),
                    completion_tokens: Some(1),
                    ..Default::default()
                });
                tail.observe_usage(&Usage {
                    prompt_tokens: Some(11),
                    completion_tokens: Some(3),
                    ..Default::default()
                });
                tail.push("partial output");
                if interrupted {
                    std::future::pending::<()>().await;
                }
                tail.finish(false);
                tail.finish(false);
            };
            let result = tokio::time::timeout(std::time::Duration::from_millis(10), call).await;
            assert_eq!(result.is_err(), interrupted);
            assert_eq!(*output.borrow(), "partial output\n");
            assert_eq!(meter.calls_with_usage.load(Ordering::Relaxed), 1);
            assert_eq!(meter.prompt_tokens.load(Ordering::Relaxed), 11);
            assert_eq!(meter.completion_tokens.load(Ordering::Relaxed), 3);
            let observations = observations.lock().unwrap();
            assert_eq!(observations.len(), 1);
            assert_eq!(observations[0].prompt_tokens, Some(11));
            assert_eq!(observations[0].completion_tokens, Some(3));
        }
        let meter = CostMeter::default();
        drop(StreamTail::new(
            &meter,
            crate::ui::markdown::MarkdownStream::new(false, 80),
            |_: &str| panic!("non-rendering empty tail emitted output"),
            false,
            false,
            false,
        ));
        assert_eq!(meter.calls_with_usage.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn quiet_usage_observer_dispatch_and_local_meter_count_latest_once_on_finish_or_drop() {
        for drop_call in [false, true] {
            let meter = CostMeter::default();
            let dispatch = crate::agent::orchestration::DispatchUsage::default();
            let calls = AtomicU64::new(0);
            let observer = |usage: &Usage| {
                calls.fetch_add(1, Ordering::Relaxed);
                dispatch.add(usage.input_total(), usage.completion_tokens.unwrap_or(0));
            };
            let call = async {
                let mut tail = StreamTail::new(
                    &meter,
                    crate::ui::markdown::MarkdownStream::new(false, 80),
                    |_: &str| panic!("quiet usage observer rendered output"),
                    false,
                    false,
                    false,
                )
                .with_usage_observer(Some(&observer));
                tail.push("private child text");
                tail.observe_usage(&Usage {
                    prompt_tokens: Some(5),
                    completion_tokens: Some(1),
                    ..Default::default()
                });
                tail.observe_usage(&Usage {
                    prompt_tokens: Some(11),
                    completion_tokens: Some(3),
                    ..Default::default()
                });
                if drop_call {
                    std::future::pending::<()>().await;
                }
                tail.finish(false);
                tail.finish(true);
            };
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_millis(10), call)
                    .await
                    .is_err(),
                drop_call
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(dispatch.finish(), (11, 3));
            assert_eq!(dispatch.finish(), (11, 3));
            assert_eq!(meter.calls_with_usage.load(Ordering::Relaxed), 1);
            assert_eq!(meter.prompt_tokens.load(Ordering::Relaxed), 11);
            assert_eq!(meter.completion_tokens.load(Ordering::Relaxed), 3);
        }
        let meter = CostMeter::default();
        let observer = |_: &Usage| panic!("no observed usage must not be estimated");
        drop(
            StreamTail::new(
                &meter,
                crate::ui::markdown::MarkdownStream::new(false, 80),
                |_: &str| {},
                false,
                false,
                false,
            )
            .with_usage_observer(Some(&observer)),
        );
        assert_eq!(meter.calls_with_usage.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn replay_usage_observer_forwards_once_without_rebilling_local_meter() {
        let meter = CostMeter::default();
        let dispatch = crate::agent::orchestration::DispatchUsage::default();
        let calls = AtomicU64::new(0);
        let observer = |usage: &Usage| {
            calls.fetch_add(1, Ordering::Relaxed);
            dispatch.add(usage.input_total(), usage.completion_tokens.unwrap_or(0));
        };
        for reported in [true, false] {
            let usage = reported.then_some(Usage {
                prompt_tokens: Some(7),
                completion_tokens: Some(2),
                ..Default::default()
            });
            // Replay's own meter recording occurs before this transport forwarding seam.
            if let Some(usage) = &usage {
                meter.record(usage);
            }
            let turn = ChatTurn {
                content: Some("private tape answer".into()),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage,
                empty_wire_note: None,
                empty_wire_raw: None,
                eager: Vec::new(),
            };
            let turn = complete_replayed_turn(
                turn,
                StreamSink {
                    observed_usage: Some(&observer),
                    ..Default::default()
                },
            );
            assert_eq!(turn.content.as_deref(), Some("private tape answer"));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(dispatch.finish(), (7, 2));
        assert_eq!(meter.calls_with_usage.load(Ordering::Relaxed), 1);
        assert_eq!(meter.prompt_tokens.load(Ordering::Relaxed), 7);
    }

    #[tokio::test]
    async fn quiet_chat_usage_observer_reports_latest_once_on_success_and_failure() {
        const PREFIX: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"private reasoning\"}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\ndata: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\n\n";
        for (ending, success) in [
            ("data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", true),
            ("event: error\ndata: private failure\n\n", false),
            ("", false),
        ] {
            let (base, hits, server) = raw_sse_backend(format!("{PREFIX}{ending}").into_bytes()).await;
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let cfg = crate::core::cli_config::CliConfig::default();
            let dispatch = crate::agent::orchestration::DispatchUsage::default();
            let calls = AtomicU64::new(0);
            let observer = |usage: &Usage| {
                calls.fetch_add(1, Ordering::Relaxed);
                dispatch.add(usage.input_total(), usage.completion_tokens.unwrap_or(0));
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(20), stream_chat_with_tools_eager_live(&http, &base, "fixture-key", "usage-observer", &[Message::user("hi")], &[],
                StreamSink { observed_usage: Some(&observer), ..Default::default() }, None, &cfg)).await.unwrap();
            assert_eq!(result.is_ok(), success);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(dispatch.finish(), (11, 3));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[test]
    fn frozen_synthesis_body_uses_supplied_config_effort_and_image_destination() {
        use crate::llm::media::{destination_key, tests as media_tests, VisionPolicy};

        let destination = destination_key("https://merge.example/v1", "claude-test");
        let image = media_tests::image(17).with_approved_destination(destination.clone());
        let image_url = image.data_url();
        let mut messages = vec![Message::system("frozen policy")];
        messages.extend(media_tests::observed_group(image));
        let frozen = crate::core::cli_config::CliConfig {
            max_tokens: Some(512),
            prompt_cache: Some(false),
            ..Default::default()
        };
        let other_destination = destination_key("https://other.example/v1", "claude-test");
        for stream in [false, true] {
            for (policy, target, disclosed) in [
                (VisionPolicy::Unsupported, &destination, false),
                (VisionPolicy::Supported, &other_destination, false),
                (VisionPolicy::Supported, &destination, true),
            ] {
                let body = build_chat_body_for_destination(
                    &frozen,
                    "claude-test",
                    &messages,
                    &[],
                    stream,
                    Some("low".into()),
                    policy,
                    Some(target),
                );
                let wire = serde_json::to_value(&body).unwrap();
                assert_eq!(wire["max_tokens"], 512);
                assert_eq!(wire["reasoning_effort"], "low");
                assert_eq!(wire["stream"], stream);
                assert!(!wire.to_string().contains("cache_control"));
                assert_eq!(wire.to_string().contains(&image_url), disclosed);
                assert_eq!(wire.get("stream_options").is_some(), stream);
                media_tests::assert_no_storage_metadata(&wire);
            }
        }
        let cached = crate::core::cli_config::CliConfig {
            prompt_cache: Some(true),
            ..frozen
        };
        let body = build_chat_body_for_destination(
            &cached,
            "claude-test",
            &messages,
            &[],
            true,
            None,
            VisionPolicy::Unsupported,
            Some(&destination),
        );
        let wire = serde_json::to_value(&body).unwrap();
        assert!(wire.to_string().contains("cache_control"));
        assert!(wire.get("reasoning_effort").is_none());
        assert_eq!(
            messages[2].tool_images[0].data_url(),
            image_url,
            "projection never mutates parent evidence"
        );
    }

    #[test]
    fn completed_answer_uses_one_selected_sink_without_global_state() {
        assert_eq!(assistant_text_sink(true, true), AssistantTextSink::Events);
        assert_eq!(assistant_text_sink(true, false), AssistantTextSink::Events);
        assert_eq!(
            assistant_text_sink(false, true),
            AssistantTextSink::Retained
        );
        assert_eq!(
            assistant_text_sink(false, false),
            AssistantTextSink::Markdown
        );
        for sink in [
            AssistantTextSink::Events,
            AssistantTextSink::Retained,
            AssistantTextSink::Markdown,
        ] {
            let answer = "A replayed answer with **markdown**.\nNo live request.";
            let mut emitted = Vec::new();
            emit_completed_assistant_text_with(Some(answer), sink, |sink, text| {
                emitted.push((sink, text.to_string()));
            });
            assert_eq!(emitted, vec![(sink, answer.to_string())]);
            for empty in [None, Some("")] {
                emit_completed_assistant_text_with(empty, sink, |_, _| {
                    panic!("empty answer emitted")
                });
            }
        }
    }

    #[test]
    fn only_selected_reasoning_fallback_is_emitted_as_answer() {
        assert_eq!(
            reasoning_only_answer("real answer", false, "private reasoning"),
            None
        );
        assert_eq!(reasoning_only_answer("", true, "tool reasoning"), None);
        assert_eq!(reasoning_only_answer("", false, "  \n"), None);
        let answer = reasoning_only_answer("", false, "  fallback answer  ").unwrap();
        let mut emitted = Vec::new();
        emit_completed_assistant_text_with(
            Some(answer),
            AssistantTextSink::Events,
            |sink, text| {
                emitted.push((sink, text.to_string()));
            },
        );
        assert_eq!(
            emitted,
            vec![(AssistantTextSink::Events, "fallback answer".to_string())]
        );
    }

    #[test]
    fn stall_timeout_defaults_and_clamps_the_env_override() {
        // Serialize with every other env-touching test (the shared process environment is global).
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(STREAM_STALL_ENV);
        assert_eq!(
            stream_stall_timeout().as_secs(),
            STREAM_STALL_SECS,
            "no override → the default"
        );
        std::env::set_var(STREAM_STALL_ENV, "240");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            240,
            "a sane override is honoured"
        );
        // A typo must not disable the deadline that keeps a stalled turn from hanging forever.
        std::env::set_var(STREAM_STALL_ENV, "0");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            15,
            "clamped up to the floor"
        );
        std::env::set_var(STREAM_STALL_ENV, "999999");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            1800,
            "clamped down to the ceiling"
        );
        std::env::set_var(STREAM_STALL_ENV, "not-a-number");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            STREAM_STALL_SECS,
            "garbage → the default"
        );
        std::env::remove_var(STREAM_STALL_ENV);
    }

    #[test]
    fn a_stalled_stream_classifies_as_transient_so_goal_mode_retries() {
        // The watchdog's wording must land on the retryable side of `classify_api_error` — a stall is
        // exactly the flakiness goal mode exists to survive.
        let e = anyhow!("SSE stream error: timeout — no data for 90s (stream never started)");
        assert_eq!(classify_api_error(&e), ApiErrorKind::Transient);
        let mid =
            anyhow!("SSE stream error: timeout — no data for 90s (stream stalled mid-response)");
        assert_eq!(classify_api_error(&mid), ApiErrorKind::Transient);
    }

    #[test]
    fn cost_meter_last_call_tracks_most_recent() {
        let m = CostMeter::default();
        assert_eq!(m.last_call(), None, "no usage seen yet");
        m.record(&Usage {
            prompt_tokens: Some(1000),
            completion_tokens: Some(50),
            cache_read_input_tokens: Some(800),
            ..Default::default()
        });
        assert_eq!(m.last_call(), Some((1000, 800, 50)));
        m.record(&Usage {
            prompt_tokens: Some(2000),
            completion_tokens: Some(10),
            ..Default::default()
        });
        assert_eq!(
            m.last_call(),
            Some((2000, 0, 10)),
            "most recent call wins; no cache → 0"
        );
        m.reset();
        assert_eq!(m.last_call(), None, "reset clears the last-call signal");
    }

    #[test]
    fn body_blames_effort_matches_provider_wordings() {
        // The three shapes real providers use — with/without the underscore, various phrasings.
        assert!(body_blames_effort("Unknown parameter: reasoning_effort"));
        assert!(body_blames_effort(
            "reasoning_effort: unsupported value 'max'"
        ));
        assert!(body_blames_effort(
            "This model does not support reasoning effort"
        ));
        assert!(
            body_blames_effort("REASONING_EFFORT is invalid"),
            "case-insensitive"
        );
        // A 400 about something else must NOT be mistaken for an effort rejection.
        assert!(!body_blames_effort("Unknown parameter: temperature"));
        assert!(!body_blames_effort("context_length_exceeded"));
    }

    #[test]
    fn effort_unsupported_set_records_and_reports_per_model() {
        // A model is "supported" until it 400s once; then it's remembered for the session.
        let model = "test-only-effort-model-xyz"; // unique so it can't collide with another test
        assert!(
            !quirk_known(model, Quirk::ReasoningEffort),
            "unseen model starts supported"
        );
        mark_quirk(model, Quirk::ReasoningEffort);
        assert!(
            quirk_known(model, Quirk::ReasoningEffort),
            "marked model is remembered"
        );
        // An unrelated model is unaffected by the mark above.
        assert!(!quirk_known("some-other-model-abc", Quirk::ReasoningEffort));
    }

    #[test]
    fn first_frame_timeout_defaults_and_clamps_the_env_override() {
        std::env::remove_var(STREAM_FIRST_FRAME_ENV);
        assert_eq!(
            stream_first_frame_timeout().as_secs(),
            STREAM_FIRST_FRAME_SECS
        );
        std::env::set_var(STREAM_FIRST_FRAME_ENV, "900");
        assert_eq!(stream_first_frame_timeout().as_secs(), 900);
        std::env::set_var(STREAM_FIRST_FRAME_ENV, "1");
        assert_eq!(stream_first_frame_timeout().as_secs(), 15, "floor");
        std::env::set_var(STREAM_FIRST_FRAME_ENV, "99999");
        assert_eq!(stream_first_frame_timeout().as_secs(), 3600, "ceiling");
        std::env::set_var(STREAM_FIRST_FRAME_ENV, "soon");
        assert_eq!(
            stream_first_frame_timeout().as_secs(),
            STREAM_FIRST_FRAME_SECS
        );
        std::env::remove_var(STREAM_FIRST_FRAME_ENV);
        std::env::remove_var(STREAM_STALL_ENV);
        assert!(
            stream_first_frame_timeout() > stream_stall_timeout(),
            "the first frame gets the longer wait by default"
        );
    }

    fn quirk_body(model: &str) -> ChatRequest {
        let cfg = crate::core::cli_config::CliConfig::default();
        let tool = ToolDef::function("t", "d", serde_json::json!({"type":"object"}));
        let mut b = build_chat_body(
            &cfg,
            model,
            &[Message::user("hi")],
            &[tool],
            false,
            Some("high".into()),
        );
        b.max_tokens = Some(2048);
        b.messages[0].cache_control = Some(CacheControl::ephemeral());
        b
    }

    #[test]
    fn quirk_blamed_names_only_fields_the_request_carried() {
        let body = quirk_body("quirk-test-model");
        assert_eq!(
            quirk_blamed("upstream returned HTTP 400 Bad Request: Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.", &body),
            Some(Quirk::MaxCompletionTokens)
        );
        assert_eq!(
            quirk_blamed("HTTP 400: max_tokens must be at most 4096", &body),
            None,
            "a value complaint is a real error, not a quirk"
        );
        assert_eq!(
            quirk_blamed("HTTP 400: unknown field `parallel_tool_calls`", &body),
            Some(Quirk::ParallelToolCalls)
        );
        assert_eq!(
            quirk_blamed("HTTP 400: tool_choice is not supported", &body),
            Some(Quirk::ToolChoice)
        );
        assert_eq!(
            quirk_blamed(
                "HTTP 400: messages[0].cache_control: extra fields not permitted",
                &body
            ),
            Some(Quirk::CacheControl)
        );
        assert_eq!(
            quirk_blamed(
                "HTTP 400: Unsupported value: 'reasoning_effort' does not support 'high'",
                &body
            ),
            Some(Quirk::ReasoningEffort)
        );
        let mut bare = quirk_body("quirk-test-model");
        bare.max_tokens = None;
        assert_eq!(
            quirk_blamed(
                "HTTP 400: max_tokens is not supported, use max_completion_tokens",
                &bare
            ),
            None,
            "a field we never sent cannot be ours"
        );
    }

    #[test]
    fn apply_quirk_rebuilds_the_body_without_the_field() {
        let mut body = quirk_body("quirk-test-model");
        apply_quirk(&mut body, Quirk::MaxCompletionTokens);
        assert_eq!(
            (body.max_tokens, body.max_completion_tokens),
            (None, Some(2048))
        );
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("max_tokens").is_none());
        assert_eq!(v["max_completion_tokens"], 2048);
        apply_quirk(&mut body, Quirk::ParallelToolCalls);
        apply_quirk(&mut body, Quirk::ToolChoice);
        assert!(body.parallel_tool_calls.is_none() && body.tool_choice.is_none());
        assert!(has_cache_control(&body));
        apply_quirk(&mut body, Quirk::CacheControl);
        assert!(!has_cache_control(&body));
        apply_quirk(&mut body, Quirk::ReasoningEffort);
        assert!(body.reasoning_effort.is_none());
    }

    #[test]
    fn known_quirks_are_stripped_before_the_first_byte_and_stay_per_model() {
        let model = "quirk-test-model-known-zz";
        assert!(!quirk_known(model, Quirk::MaxCompletionTokens));
        mark_quirk(model, Quirk::MaxCompletionTokens);
        mark_quirk(model, Quirk::CacheControl);
        assert!(quirk_known(model, Quirk::MaxCompletionTokens));
        assert!(!quirk_known(
            "quirk-test-other-model",
            Quirk::MaxCompletionTokens
        ));
        let mut body = quirk_body(model);
        apply_known_quirks(&mut body);
        assert_eq!(
            (body.max_tokens, body.max_completion_tokens),
            (None, Some(2048))
        );
        assert!(!has_cache_control(&body));
        assert!(body.parallel_tool_calls.is_some(), "unlearned fields stay");
    }

    #[test]
    fn retry_classifier_only_retries_transient_statuses() {
        for s in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
        for s in [200u16, 400, 401, 403, 404, 422, 501] {
            assert!(!is_retryable_status(s), "{s} should NOT be retryable");
        }
    }

    #[test]
    fn retry_captions_name_the_wait_and_the_attempt() {
        assert_eq!(
            retry_caption(429, 2, 3, 43_000),
            "rate-limited — retrying in 43s (2/3)"
        );
        assert_eq!(
            retry_caption(503, 1, 3, 1_500),
            "HTTP 503 — retrying in 2s (1/3)"
        );
    }

    #[test]
    fn backoff_ceiling_is_monotonic_and_capped() {
        let (base, cap) = (400u64, 8_000u64);
        let seq: Vec<u64> = (0..6).map(|a| backoff_ceiling_ms(a, base, cap)).collect();
        // base * 2^attempt until the cap, never decreasing, never above the cap, never zero.
        assert_eq!(seq[0], 400);
        assert_eq!(seq[1], 800);
        assert_eq!(seq[2], 1600);
        for w in seq.windows(2) {
            assert!(w[1] >= w[0], "ceiling must not decrease: {seq:?}");
        }
        assert!(
            seq.iter().all(|&d| d >= 1 && d <= cap),
            "every delay in [1, cap]: {seq:?}"
        );
        assert_eq!(*seq.last().unwrap(), cap, "saturates at the cap");
    }

    #[test]
    fn backoff_jitter_stays_within_half_to_ceiling() {
        let (base, cap) = (400u64, 8_000u64);
        for attempt in 0..6 {
            let ceil = backoff_ceiling_ms(attempt, base, cap);
            for _ in 0..50 {
                let d = backoff_ms(attempt, base, cap);
                assert!(
                    d >= ceil / 2 && d <= ceil,
                    "jitter {d} outside [{}, {ceil}]",
                    ceil / 2
                );
            }
        }
    }

    #[test]
    fn interactive_backoff_is_faster_than_goal() {
        // The whole point of splitting the two: an interactive turn (someone waiting) must never sit
        // in a lone 30s pause — that reads as a hang. Goal mode (nobody watching, runs long) keeps the
        // higher ceiling. Pin BOTH the absolute cap and the relative ordering so a future edit to
        // either constant can't quietly collapse them back together.
        for attempt in 0..12 {
            let iv = interactive_backoff_ms(attempt);
            assert!(
                iv <= 4_000,
                "interactive delay {iv} (attempt {attempt}) exceeded its 4s cap — a lone pause this \
                 long is the hang feel this split exists to remove"
            );
        }
        // At a high attempt both have saturated: interactive at 4s, goal at 30s. Compare CEILINGS
        // (not the jittered draw) so the assertion is deterministic.
        let iv_ceil = backoff_ceiling_ms(6, 300, 4_000);
        let goal_ceil = backoff_ceiling_ms(6, 500, 30_000);
        assert_eq!(iv_ceil, 4_000, "interactive saturates at 4s");
        assert_eq!(goal_ceil, 30_000, "goal saturates at 30s");
        assert!(
            iv_ceil < goal_ceil,
            "interactive must back off faster than goal ({iv_ceil} vs {goal_ceil})"
        );
    }

    #[test]
    fn classify_api_error_splits_permanent_from_transient() {
        // Permanent 4xx — retrying can't fix these.
        for code in ["HTTP 400", "HTTP 404"] {
            let e = anyhow::anyhow!("upstream returned {code} Bad Request: nope");
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::Permanent,
                "{code} must be Permanent"
            );
        }
        // Auth is its own kind, and it is NOT Permanent: `Permanent` buys a bounded run of
        // retries, which for a 401 means a run of calls that cannot be built — the credential was
        // taken from disk at the first one. Since 2026-09-06 a 401 from Aizen usually means the
        // machine was unpinned on purpose, and the person who did it is waiting for the sentence
        // that names `aizen login`, not for a backoff to finish.
        for code in ["HTTP 401", "HTTP 403"] {
            let e = anyhow::anyhow!("upstream returned {code} Unauthorized: nope");
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::SignedOut,
                "{code} must stop at once"
            );
        }
        // 402/422/451 were the substring-matcher's blind spot: unfixable client errors that fell
        // through to Transient and burned every retry the budget had (forever, under /goal).
        for code in ["HTTP 402", "HTTP 405", "HTTP 422", "HTTP 451"] {
            let e = anyhow::anyhow!("upstream returned {code} Nope: denied");
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::Permanent,
                "{code} must be Permanent"
            );
        }
        // Everything else leans Transient (goal mode's survival bias).
        for msg in [
            "upstream returned HTTP 429 Too Many Requests: slow down",
            "upstream returned HTTP 408 Request Timeout: too slow",
            "upstream returned HTTP 503 Service Unavailable: try later",
            "upstream returned HTTP 529 Overloaded: busy",
            "request failed after retries",
            "SSE stream error: connection reset",
            "health probe request failed",
        ] {
            let e = anyhow::anyhow!(msg);
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::Transient,
                "{msg} must be Transient"
            );
        }
    }

    #[test]
    fn classify_api_error_names_context_overflow() {
        // Overflow wears a permanent status (400/413) but has its own recovery — the loop shrinks
        // the transcript and retries once. Both routes must classify as ContextOverflow: the
        // status route (413) and the body route (a 400 whose message says the context overflowed).
        for msg in [
            "upstream returned HTTP 413 Payload Too Large: request entity too large",
            "upstream returned HTTP 400 Bad Request: {\"error\":{\"code\":\"context_length_exceeded\"}}",
            "upstream returned HTTP 400 Bad Request: this model's maximum context length is 128000",
            "upstream returned HTTP 400 Bad Request: prompt is too long: 210000 tokens",
        ] {
            let e = anyhow::anyhow!(msg);
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::ContextOverflow,
                "{msg} must be ContextOverflow"
            );
        }
        // …while an unrelated 400 stays Permanent: a false positive here would shrink a healthy
        // transcript instead of surfacing a real config error.
        let e = anyhow::anyhow!("upstream returned HTTP 400 Bad Request: unknown parameter foo");
        assert_eq!(classify_api_error(&e), ApiErrorKind::Permanent);
    }

    #[test]
    fn cost_meter_sums_real_usage_and_ignores_empty() {
        // A LOCAL meter (not the global) so parallel tests don't race on it.
        let m = CostMeter::default();
        m.record(&Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(40),
            total_tokens: Some(140),
            ..Default::default()
        });
        m.record(&Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: None,
            ..Default::default()
        });
        m.record(&Usage::default()); // all-None → NOT counted as a usage-reporting call
        assert_eq!(m.snapshot(), (110, 45, 2));
        m.reset();
        assert_eq!(m.snapshot(), (0, 0, 0));
    }

    #[test]
    fn cost_meter_tracks_cache_reads() {
        let m = CostMeter::default();
        m.record(&Usage {
            prompt_tokens: Some(100),
            cache_read_input_tokens: Some(80),
            ..Default::default()
        });
        assert_eq!(
            m.cache_read(),
            80,
            "cache-read tokens accumulate for the /cost probe"
        );
        m.reset();
        assert_eq!(m.cache_read(), 0);
    }

    #[test]
    fn cost_meter_last_call_uses_the_shape_corrected_input() {
        // Anthropic-style: `prompt_tokens` EXCLUDES the cache read. Dividing cached by prompt
        // would report 4000 % — the denominator must be the total that actually went out.
        let m = CostMeter::default();
        m.record(&Usage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(20),
            cache_read_input_tokens: Some(40_000),
            cache_creation_input_tokens: Some(500),
            ..Default::default()
        });
        assert_eq!(m.last_call(), Some((41_500, 40_000, 20)));
        assert_eq!(m.input_total(), 41_500);
        assert_eq!(m.cache_write(), 500);
    }

    #[test]
    fn cost_meter_ledger_rows_follow_turns_and_cursor() {
        let m = CostMeter::default();
        let usage = |p: u64, c: u64| Usage {
            prompt_tokens: Some(p),
            completion_tokens: Some(5),
            prompt_tokens_details: Some(crate::core::types::PromptTokensDetails {
                cached_tokens: Some(c),
            }),
            ..Default::default()
        };
        m.begin_turn();
        m.record(&usage(1_000, 0));
        m.record(&usage(1_200, 900));
        m.begin_turn();
        m.record(&usage(1_400, 1_100));
        m.record(&Usage::default()); // all-None: not a call, not a row

        let (epoch, seq) = m.cursor();
        assert_eq!(seq, 3, "three usage-carrying calls → three rows");
        assert_eq!(epoch, process_epoch());

        let all = m.rows_since(0, 0);
        assert_eq!(
            all.iter().map(|r| r.turn).collect::<Vec<_>>(),
            vec![1, 1, 2],
            "rows carry the turn that was current when they were recorded"
        );
        assert_eq!(all[1].cached, 900);
        assert_eq!(
            all[1].input, 1_200,
            "OpenAI shape: cached is a subset of prompt"
        );
        // A file written by THIS process at seq 2 gets only the row after it.
        assert_eq!(m.rows_since(epoch, 2).len(), 1);
        // A file written by ANOTHER process gets everything, whatever seq it recorded.
        assert_eq!(m.rows_since(epoch.wrapping_add(1), 99).len(), 3);
        // The current turn's probe sums its own rows only.
        assert_eq!(m.last_turn_summary(), Some((2, 1, 1_400, 1_100)));

        m.reset();
        assert!(m.rows_since(0, 0).is_empty(), "reset drops the rows");
        assert_eq!(m.last_turn_summary(), None);
        assert_eq!(
            m.cursor().1,
            3,
            "…but never the sequence: a resumed file must not re-append the same rows"
        );
        assert_eq!(
            m.current_turn(),
            2,
            "…nor the turn counter, so row order stays monotonic"
        );
    }

    #[test]
    fn anthropic_model_detection() {
        for m in [
            "opus-4-8",
            "claude-3-5",
            "Sonnet",
            "fable-5",
            "anthropic/claude",
            "claude-haiku-4-5",
        ] {
            assert!(is_anthropic_model(m), "{m} should be detected as Anthropic");
        }
        for m in ["gpt-4o", "deepseek-chat", "gemini-1.5", "llama-3"] {
            assert!(!is_anthropic_model(m), "{m} should NOT be Anthropic");
        }
        // Word-boundary guard: community models that merely EMBED a token must not trip AUTO cache.
        for m in [
            "fable13b",
            "haikuwriter",
            "mythos-13b",
            "opusculum-7b",
            "sonnetizer",
        ] {
            assert!(
                !is_anthropic_model(m),
                "{m} embeds a token but is not Anthropic"
            );
        }
    }

    #[test]
    fn anthropic_endpoint_is_matched_on_host_not_substring() {
        // First-party, with and without a path/port/scheme variation.
        for b in [
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
            "https://API.Anthropic.COM/v1",
            "http://api.anthropic.com:443/v1",
        ] {
            assert!(is_anthropic_endpoint(b), "{b} is first-party Anthropic");
        }
        // A proxy that merely MENTIONS anthropic in its path is NOT first-party. Getting this wrong
        // would attach `x-api-key` + `anthropic-version` to a third-party gateway, and would make the
        // check misreport which dialect the endpoint speaks.
        for b in [
            "https://gw.example.com/anthropic/v1",
            "https://openrouter.ai/api/v1",
            "https://api.anthropic.com.evil.test/v1",
            "http://localhost:11434/v1",
        ] {
            assert!(!is_anthropic_endpoint(b), "{b} must not be first-party");
        }
        // A subdomain of the real host still is.
        assert!(is_anthropic_endpoint("https://eu.api.anthropic.com/v1"));
    }

    #[test]
    fn opencode_endpoint_is_matched_on_host_not_substring() {
        // First-party zen paths, with scheme/case/port variations.
        for b in [
            "https://opencode.ai/zen/v1",
            "https://opencode.ai/zen/v1/",
            "https://OpenCode.AI/zen/v1",
            "http://opencode.ai:443/zen/go/v1",
        ] {
            assert!(is_opencode_endpoint(b), "{b} is first-party OpenCode");
        }
        // A proxy that merely MENTIONS opencode in its path is NOT first-party — misclassifying it
        // would pin zen-gateway behaviour (the client header) onto a third-party server.
        for b in [
            "https://gw.example.com/opencode/v1",
            "https://opencode.ai.evil.test/v1",
            "https://api.anthropic.com/v1",
            "http://localhost:11434/v1",
        ] {
            assert!(!is_opencode_endpoint(b), "{b} must not be first-party");
        }
    }

    #[test]
    fn free_model_id_is_the_suffix_convention() {
        assert!(is_free_model_id("deepseek-v4-flash-free"));
        assert!(is_free_model_id("mimo-v2.5-free"));
        // OpenCode zen also ships one unsuffixed free id.
        assert!(is_free_model_id("big-pickle"));
        // Position matters: a leading or embedded "free" is not the tier marker.
        assert!(!is_free_model_id("free-big-pickle"));
        assert!(!is_free_model_id("claude-fable-5"));
    }

    #[test]
    fn cache_enabled_auto_and_forced() {
        assert!(cache_enabled(None, "opus-4-8"), "AUTO on for Anthropic");
        assert!(!cache_enabled(None, "gpt-4o"), "AUTO off for non-Anthropic");
        assert!(!cache_enabled(Some(false), "opus-4-8"), "explicit off wins");
        assert!(cache_enabled(Some(true), "gpt-4o"), "explicit on wins");
    }

    #[test]
    fn cache_breakpoints_stamp_tools_system_and_last_stable_msg() {
        let mut msgs = vec![
            Message::system("sys"),
            Message::user("first"),
            Message::assistant("answer"),
            Message::tool_result("c1", "result"),
            Message::user("newest"), // volatile — must NOT be stamped
        ];
        let mut tools = vec![
            ToolDef::function("a", "d", serde_json::json!({"type":"object"})),
            ToolDef::function("b", "d", serde_json::json!({"type":"object"})),
        ];
        apply_cache_breakpoints(&mut msgs, &mut tools);
        assert!(
            tools[0].cache_control.is_none(),
            "only the LAST tool def is stamped"
        );
        assert!(
            tools[1].cache_control.is_some(),
            "last tool def caches the whole tool block"
        );
        assert!(msgs[0].cache_control.is_some(), "system message stamped");
        assert!(
            msgs[3].cache_control.is_some(),
            "last stable assistant/tool message stamped"
        );
        assert!(
            msgs[4].cache_control.is_none(),
            "the newest user turn stays uncached"
        );
    }

    #[test]
    fn cache_breakpoints_short_history_only_system_and_tools() {
        let mut msgs = vec![Message::system("sys"), Message::user("hi")];
        let mut tools = vec![ToolDef::function(
            "a",
            "d",
            serde_json::json!({"type":"object"}),
        )];
        apply_cache_breakpoints(&mut msgs, &mut tools);
        assert!(msgs[0].cache_control.is_some());
        assert!(tools[0].cache_control.is_some());
        assert!(
            msgs[1].cache_control.is_none(),
            "no history breakpoint when n < 3"
        );
    }

    #[test]
    fn accumulator_does_not_complete_a_call_with_partial_args() {
        // THE BUG: a call is only "done" once its arguments are whole. Slot 0's JSON is still open
        // when slot 1 starts, so completing it would hand the executor `{"path":` — which
        // `parse_call_args` cannot parse, or (when the fragment is empty) reads as `{}`, running the
        // tool with NO arguments and failing with `missing required string arg` while the transcript
        // line, built later from the full call, showed the argument plainly.
        let mut acc = ToolCallAccumulator::default();
        assert!(acc
            .ingest(&[delta(0, Some("a"), Some("file_read"), Some(r#"{"path":"#))])
            .is_empty());
        // The rest of slot 0's arguments arrive (id-less, still the active call)…
        assert!(acc
            .ingest(&[delta(0, None, None, Some(r#""x.rs"}"#))])
            .is_empty());
        // …and only NOW, whole, does the next slot's start complete it.
        let done = acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))]);
        assert_eq!(done.len(), 1, "a whole call completes");
        assert_eq!(done[0].1.function.arguments, r#"{"path":"x.rs"}"#);

        // And the failing shape: a call still mid-JSON when the next slot starts is NOT dispatched.
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, Some("a"), Some("file_read"), Some(r#"{"path":"#))]);
        assert!(
            acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))])
                .is_empty(),
            "a half-streamed call must not be dispatched"
        );
    }

    #[test]
    fn accumulator_does_not_complete_a_call_with_empty_args() {
        // An id+name opening fragment with no arguments yet: `""` is indistinguishable from "no
        // arguments", and `parse_call_args` turns it into a valid `{}`. Never eager-dispatch it.
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, Some("a"), Some("web_fetch"), Some(""))]);
        assert!(
            acc.ingest(&[delta(1, Some("b"), Some("web_search"), Some("{}"))])
                .is_empty(),
            "empty args are 'not yet', not 'no arguments'"
        );
    }

    #[test]
    fn accumulator_routes_idless_fragments_to_the_active_slot() {
        // An index-omitting provider: every fragment claims index 0. The second call's `id` reroutes
        // it to a fresh slot, but its ARGUMENT fragments carry no id — they must follow the active
        // slot rather than pouring back into slot 0 (which corrupted slot 0's JSON and left call #2
        // argument-less).
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(
            0,
            Some("a"),
            Some("file_read"),
            Some(r#"{"path":"a.rs"}"#),
        )]);
        acc.ingest(&[delta(0, Some("b"), Some("web_fetch"), Some(""))]);
        acc.ingest(&[delta(0, None, None, Some(r#"{"url":"#))]);
        acc.ingest(&[delta(0, None, None, Some(r#""https://x.dev"}"#))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 2, "both calls survive");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(
            calls[1].function.arguments, r#"{"url":"https://x.dev"}"#,
            "id-less argument fragments followed the active call"
        );
    }

    #[test]
    fn accumulator_still_completes_when_args_are_whole() {
        // The eager fast path must survive the tightening: whole arguments + a later slot ⇒ complete.
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(
            0,
            Some("a"),
            Some("file_read"),
            Some(r#"{"path":"x.rs"}"#),
        )]);
        let done = acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))]);
        assert_eq!(done.len(), 1, "a complete call still dispatches early");
        assert_eq!(done[0].1.function.arguments, r#"{"path":"x.rs"}"#);
    }

    #[test]
    fn args_complete_accepts_only_whole_objects() {
        assert!(args_complete("{}"), "an argument-less call streams `{{}}`");
        assert!(args_complete(r#"{"a":1}"#));
        assert!(!args_complete(""), "`\"\"` means 'not yet', never '{{}}'");
        assert!(!args_complete("   "));
        assert!(!args_complete(r#"{"a":"#), "truncated");
        assert!(!args_complete("[1,2]"), "an array is not an args object");
        assert!(!args_complete(r#""a string""#));
    }

    fn delta(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(String::from),
            function: Some(FunctionDelta {
                name: name.map(String::from),
                arguments: args.map(String::from),
            }),
        }
    }

    #[test]
    fn accumulator_emits_completion_when_next_slot_starts() {
        let mut acc = ToolCallAccumulator::default();
        // First call streams over two batches — nothing completes yet.
        assert!(acc
            .ingest(&[delta(0, Some("a"), Some("file_read"), Some(r#"{"path":"#))])
            .is_empty());
        assert!(acc
            .ingest(&[delta(0, None, None, Some(r#""x.rs"}"#))])
            .is_empty());
        // Slot 1 starts → slot 0 completes with its FULL arguments.
        let done = acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))]);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, 0, "completed slot key");
        assert_eq!(done[0].1.function.name, "file_read");
        assert_eq!(done[0].1.function.arguments, r#"{"path":"x.rs"}"#);
        // The LAST call never emits mid-stream — finish_indexed covers it, slot keys intact.
        let indexed = acc.finish_indexed();
        assert_eq!(indexed.len(), 2);
        assert_eq!(
            (indexed[0].0, indexed[0].1.function.name.as_str()),
            (0, "file_read")
        );
        assert_eq!(
            (indexed[1].0, indexed[1].1.function.name.as_str()),
            (1, "file_glob")
        );
    }

    #[test]
    fn accumulator_completion_survives_id_reroute() {
        // Index-omitting provider: both calls claim index 0; the second id reroutes to a fresh
        // slot — and that reroute must still emit the first call's completion.
        let mut acc = ToolCallAccumulator::default();
        assert!(acc
            .ingest(&[delta(0, Some("a"), Some("t_one"), Some("{}"))])
            .is_empty());
        let done = acc.ingest(&[delta(0, Some("b"), Some("t_two"), Some("{}"))]);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].1.function.name, "t_one");
        let calls = acc.finish();
        assert_eq!(calls.len(), 2, "both calls survive the collision");
    }

    #[test]
    fn accumulator_reassembles_streamed_tool_call() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(
            0,
            Some("call_1"),
            Some("memory_search"),
            Some("{\"que"),
        )]);
        acc.ingest(&[delta(0, None, None, Some("ry\":\"x\"}"))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "memory_search");
        assert_eq!(calls[0].function.arguments, "{\"query\":\"x\"}");
    }

    #[test]
    fn accumulator_handles_two_parallel_calls() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[
            delta(0, Some("a"), Some("f"), Some("{}")),
            delta(1, Some("b"), Some("g"), Some("{}")),
        ]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "f");
        assert_eq!(calls[1].function.name, "g");
    }

    #[test]
    fn delta_captures_both_reasoning_field_names() {
        // DeepSeek-style `reasoning_content` and OpenRouter-style `reasoning` both resolve to the
        // same channel, whichever spelling the provider used.
        let a: crate::core::types::Delta =
            serde_json::from_str(r#"{"reasoning_content":"thinking…"}"#).unwrap();
        assert_eq!(a.reasoning_text(), Some("thinking…"));
        let b: crate::core::types::Delta =
            serde_json::from_str(r#"{"reasoning":"thinking…"}"#).unwrap();
        assert_eq!(b.reasoning_text(), Some("thinking…"));
        // Plain content chunk leaves the reasoning channel empty.
        let c: crate::core::types::Delta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert!(c.reasoning_text().is_none());
    }

    #[test]
    fn delta_with_both_reasoning_spellings_still_parses() {
        // THE BUG. The two spellings used to be ONE field plus `#[serde(alias = "reasoning")]`, so a
        // provider that mirrors its reasoning text into BOTH keys of a single delta — which
        // OpenRouter-shaped gateways do — was a serde `duplicate field` error. serde rejects the
        // whole object, so the frame was DISCARDED, and the loop printed
        // `[warn] unparseable stream frame (duplicate field ...)` once per streamed reasoning token
        // over the live UI. Two separate fields cannot collide.
        let d: crate::core::types::Delta = serde_json::from_str(
            r#"{"content":null,"reasoning":" ch","reasoning_content":" ch",
                "reasoning_details":[{"format":"unknown"}]}"#,
        )
        .expect("both spellings in one delta must parse");
        assert_eq!(d.reasoning_text(), Some("ch"));

        // The real cost was never the noise: anything riding in the SAME delta died with the frame.
        // A tool call lost this way is a step the agent never takes.
        let d: crate::core::types::Delta = serde_json::from_str(
            r#"{"reasoning":"x","reasoning_content":"x",
                "tool_calls":[{"index":0,"id":"c1","function":{"name":"file_read","arguments":"{}"}}]}"#,
        )
        .expect("a tool-call fragment must survive a mirrored reasoning delta");
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(
            d.tool_calls[0].function.as_ref().unwrap().name.as_deref(),
            Some("file_read")
        );

        // Same shape on the non-streaming path, where a dropped message is a whole dead turn.
        let m = resp_msg(
            r#"{"content":null,"reasoning":"the answer","reasoning_content":"the answer"}"#,
        );
        assert_eq!(resolve_content(&m).as_deref(), Some("the answer"));
    }

    #[test]
    fn parse_chunk_recovers_a_duplicate_key_frame() {
        // A gateway that mirrors one value into two spellings of the same channel emits a literal
        // duplicate key once the two spellings map to one field. Deriving `Deserialize` rejects the
        // whole object there — dropping whatever `content`/`tool_calls` shared that delta — so the
        // lenient pass routes through `Value`, whose object map takes duplicate keys last-wins.
        let dup = r#"{"choices":[{"delta":{"content":"hi","content":"hi"}}]}"#;
        assert!(
            serde_json::from_str::<ChatChunk>(dup).is_err(),
            "precondition: the strict derive must reject a duplicate key",
        );
        let chunk = parse_chunk(dup).expect("lenient pass recovers the frame");
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("hi"),
            "the payload riding in that delta must survive",
        );

        // The shape from the report: both reasoning spellings plus a tool-call fragment in one delta.
        // The fragment is the part that matters — a lost one is a step the agent never takes.
        let real = r#"{"choices":[{"delta":{"reasoning":" ch","reasoning_content":" ch",
            "tool_calls":[{"index":0,"id":"c1","function":{"name":"file_read","arguments":"{}"}}]}}]}"#;
        let chunk = parse_chunk(real).expect("mirrored reasoning + tool call must parse");
        assert_eq!(chunk.choices[0].delta.reasoning_text(), Some("ch"));
        assert_eq!(chunk.choices[0].delta.tool_calls.len(), 1);

        // Well-shaped frames still take the strict path unchanged.
        let ok = r#"{"choices":[{"delta":{"content":"x"}}]}"#;
        assert_eq!(
            parse_chunk(ok).unwrap().choices[0].delta.content.as_deref(),
            Some("x"),
        );

        // Genuine non-chunks stay errors — the caller drops them, it must not treat them as content.
        assert!(parse_chunk(": keepalive").is_err());
        assert!(parse_chunk(r#"{"choices":"not-an-array"}"#).is_err());
        // Invalid frames fail closed without echoing provider payloads (which may carry secrets).
        let e = parse_chunk(r#"{"choices":[{"delta":{"content":"a","content":1}}]}"#).unwrap_err();
        assert!(e.is::<super::super::delegated::Failure>(), "err: {e}");
    }

    #[test]
    fn frame_warn_payload_is_truncated_on_a_char_boundary() {
        // Frames carry UTF-8 (the user's own prose comes back as reasoning), so a byte-index cut
        // would panic mid-codepoint on exactly the streams this warn exists to describe.
        let short = r#"{"choices":[]}"#;
        assert_eq!(truncate_frame(short), short);

        let long = format!(r#"{{"reasoning":"{}"}}"#, "ườ".repeat(200));
        let cut = truncate_frame(&long);
        assert!(cut.chars().count() <= FRAME_WARN_MAX + 32, "cut: {cut}");
        assert!(cut.contains("bytes total"), "cut: {cut}");
    }

    /// Deserialize a non-streaming `message` object the way `chat_with_tools` does.
    fn resp_msg(json: &str) -> crate::core::types::RespMessage {
        serde_json::from_str(json).expect("valid message json")
    }

    #[test]
    fn reasoning_only_reply_is_not_mistaken_for_provider_silence() {
        // THE BUG: the streaming `Delta` has always read `reasoning_content`/`reasoning`, but the
        // non-streaming `RespMessage` did not — and SUB-AGENTS are the one caller that runs
        // exclusively on the non-streaming path. A provider that puts the whole reply in the
        // reasoning channel therefore deserialized to `content: None, tool_calls: []`, which every
        // caller classifies as an empty-200 provider failure. The model spoke; we could not hear it.
        assert_eq!(
            resolve_content(&resp_msg(
                r#"{"content":null,"reasoning_content":"the answer"}"#
            ))
            .as_deref(),
            Some("the answer"),
        );
        // OpenRouter spells the same channel `reasoning` — a separate field, resolved together.
        assert_eq!(
            resolve_content(&resp_msg(r#"{"reasoning":"the answer"}"#)).as_deref(),
            Some("the answer"),
        );
        // Whitespace-only `content` is as silent as null.
        assert_eq!(
            resolve_content(&resp_msg(
                r#"{"content":"   ","reasoning_content":"the answer"}"#
            ))
            .as_deref(),
            Some("the answer"),
        );
    }

    #[test]
    fn reasoning_never_displaces_or_appends_to_a_real_reply() {
        // The fallback is exactly that — a fallback. Real content wins untouched: appending reasoning
        // to it would leak the chain-of-thought the CLI deliberately suppresses on the streaming path.
        assert_eq!(
            resolve_content(&resp_msg(
                r#"{"content":"real answer","reasoning_content":"private thinking"}"#
            ))
            .as_deref(),
            Some("real answer"),
        );
        // A TOOL-CALL turn legitimately has no content. Substituting reasoning there would turn a
        // tool call into a text turn and lose the step.
        let with_call = resp_msg(
            r#"{"content":null,"reasoning_content":"private thinking",
                "tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]}"#,
        );
        assert!(
            resolve_content(&with_call).is_none(),
            "a tool-call turn must stay a tool-call turn"
        );
        // Genuine silence stays silence — the empty-200 path must still fire.
        assert!(resolve_content(&resp_msg(r#"{"content":null}"#)).is_none());
        assert!(resolve_content(&resp_msg(r#"{"reasoning_content":"  "}"#)).is_none());
    }

    #[test]
    fn think_filter_strips_a_whole_block() {
        let mut f = ThinkFilter::default();
        let out = f.push("Hello <think>secret reasoning</think>world") + &f.finish();
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn think_filter_strips_empty_tags() {
        let mut f = ThinkFilter::default();
        let out = f.push("<think></think>answer") + &f.finish();
        assert_eq!(out, "answer");
    }

    #[test]
    fn think_filter_handles_a_tag_split_across_deltas() {
        // The open/close tags arrive in pieces — the classic SSE-delta hazard.
        let mut f = ThinkFilter::default();
        let mut out = String::new();
        for piece in ["A<thi", "nk>hidden</thi", "nk>B"] {
            out.push_str(&f.push(piece));
        }
        out.push_str(&f.finish());
        assert_eq!(out, "AB");
    }

    #[test]
    fn think_filter_passes_plain_text_through_unchanged() {
        let mut f = ThinkFilter::default();
        let out = f.push("just a normal answer, no tags") + &f.finish();
        assert_eq!(out, "just a normal answer, no tags");
    }

    #[test]
    fn think_filter_drops_an_unclosed_block() {
        let mut f = ThinkFilter::default();
        let out = f.push("before <think>dangling forever") + &f.finish();
        assert_eq!(out, "before ");
    }

    #[test]
    fn accumulator_synthesizes_missing_id() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, None, Some("f"), Some("{}"))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].id.is_empty(), "a missing id must be synthesized");
    }

    #[tokio::test]
    async fn actual_chat_receiver_gets_tool_text_group_then_exact_image_bytes_without_metadata() {
        use crate::llm::media::{destination_key, tests as media_tests, VisionPolicy};
        let (base, receiver, server) = media_tests::request_receiver(
            r#"{"choices":[{"message":{"role":"assistant","content":"reviewed"}}]}"#,
            "application/json",
        )
        .await;
        let image =
            media_tests::image(31).with_approved_destination(destination_key(&base, "vision-test"));
        let expected = image.bytes().unwrap();
        let messages = media_tests::observed_group(image);
        let cfg = crate::core::cli_config::CliConfig::default();
        let destination = destination_key(&base, "vision-test");
        let body = build_chat_body_for_destination(
            &cfg,
            "vision-test",
            &messages,
            &[],
            false,
            Some("low".into()),
            VisionPolicy::Supported,
            Some(&destination),
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = send_chat(
            &client,
            &format!("{base}/chat/completions"),
            "test-key",
            body,
        )
        .await
        .unwrap();
        assert!(response.status().is_success());
        let wire = receiver.await.unwrap();
        server.await.unwrap();
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["content"], "image text");
        assert_eq!(messages[2]["content"], "other text");
        assert_eq!(messages[3]["content"], "last text");
        assert_eq!(messages[4]["role"], "user");
        let url = messages[4]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert_eq!(media_tests::decode_url(url), expected);
        media_tests::assert_no_storage_metadata(&wire);
        assert_eq!(wire["reasoning_effort"], "low");
    }

    #[test]
    fn transport_evidence_drops_echoed_secrets_but_keeps_machine_evidence() {
        let raw = r#"{"error":{"code":"UPSTREAM_BAD_RESPONSE","status":0,"correlation_id":"req-17","message":"sk-live-secret data:image/png;base64,SECRET Authorization: Bearer secret"},"request":{"prompt":"private"}}"#;
        let error = transport_failure("sse", Some(200), raw, None);
        assert_eq!(error.http_status, Some(200));
        assert_eq!(error.provider_status, Some(0));
        assert_eq!(
            error.provider_code.as_deref(),
            Some("UPSTREAM_BAD_RESPONSE")
        );
        assert_eq!(error.correlation_id.as_deref(), Some("req-17"));
        let printed = error.to_string();
        for secret in [
            "sk-live",
            "base64",
            "Authorization",
            "Bearer",
            "private",
            "SECRET",
        ] {
            assert!(!printed.contains(secret), "{printed}");
        }
        let error = transport_failure(
            "http",
            Some(400),
            r#"{"error":{"message":"Unsupported max_tokens; use max_completion_tokens. sk-hidden"}}"#,
            None,
        );
        assert!(error.message.contains("max_completion_tokens"));
        assert!(!error.message.contains("sk-hidden"));
    }

    #[tokio::test]
    async fn quiet_stream_only_receiver_preserves_effort_and_validates_finish() {
        use crate::llm::media::tests as media_tests;
        const REPLY: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"private reasoning\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        let (base, receiver, server) =
            media_tests::request_receiver(REPLY, "text/event-stream").await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cfg = crate::core::cli_config::CliConfig::default();
        let turn = stream_chat_with_tools_eager_live(
            &client,
            &base,
            "fixture-key",
            "quiet-stream-only",
            &[Message::user("hi")],
            &[],
            StreamSink::default(),
            Some("high".into()),
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(turn.content.as_deref(), Some("answer"));
        assert_eq!(turn.usage.unwrap().completion_tokens, Some(2));
        assert!(turn.eager.is_empty());
        let body = receiver.await.unwrap();
        server.await.unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    /// One logical delegated step gets THREE actual HTTP sends, counting every retry layer. Without
    /// a control the top-level path keeps its own budget unchanged.
    #[tokio::test]
    async fn delegated_step_spends_three_wire_sends_across_all_retry_layers() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let seen: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let counter = seen.clone();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                *counter.lock().unwrap() += 1;
                let body = r#"{"error":{"message":"busy"}}"#;
                let reply = format!(
                    "HTTP/1.1 503 Busy\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let control = crate::llm::delegated::Control::new(
            std::time::Duration::from_secs(30),
            None,
            None,
        );
        let error = crate::llm::delegated::run(Some(&control), send_with_retry_render(
            || client.post(&url).body("{}"),
            false,
        ))
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<crate::llm::delegated::Failure>(),
            Some(&crate::llm::delegated::Failure::Exhausted),
            "{error}"
        );
        assert_eq!(*seen.lock().unwrap(), 3, "HTTP retry, blank replay and outer retry share one budget");
        srv.abort();
    }

    /// A healthy delegated step is untouched by the shared budget: one send, one answer.
    #[tokio::test]
    async fn a_healthy_delegated_step_still_spends_exactly_one_send() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const REPLY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let counter = seen.clone();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                *counter.lock().unwrap() += 1;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{REPLY}",
                    REPLY.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cfg = crate::core::cli_config::CliConfig::default();
        let control = crate::llm::delegated::Control::new(
            std::time::Duration::from_secs(30),
            None,
            None,
        );
        let turn = crate::llm::delegated::run(Some(&control), stream_chat_with_tools_eager_live(
            &client, &base, "fixture-key", "healthy-delegated", &[Message::user("hi")], &[],
            StreamSink::default(), None, &cfg,
        ))
        .await
        .unwrap();
        assert_eq!(turn.content.as_deref(), Some("ok"));
        assert_eq!(*seen.lock().unwrap(), 1);
        assert_eq!(control.sends(), 1);
        srv.abort();
    }

    /// A provider that answers 200 and streams only metadata must not produce a successful empty
    /// turn. It is not a validated finish either, so the turn fails — after a BOUNDED number of
    /// blank replays, never an unbounded retry storm.
    #[tokio::test]
    async fn metadata_only_stream_never_completes_a_turn() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const PINGS: &str = "data: {\"type\":\"ping\"}\n\ndata: {}\n\ndata: {\"choices\":[]}\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let counter = seen.clone();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                *counter.lock().unwrap() += 1;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PINGS}",
                    PINGS.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cfg = crate::core::cli_config::CliConfig::default();
        let turn = stream_chat_with_tools_eager_live(
            &client, &base, "fixture-key", "metadata-only", &[Message::user("hi")], &[],
            StreamSink::default(), None, &cfg,
        ).await;
        let error = turn.err().expect("metadata is not an answer");
        assert!(error.to_string().contains("validated finish reason"), "{error}");
        let requests = *seen.lock().unwrap();
        assert_eq!(
            requests,
            1 + STREAM_BLANK_RETRIES as usize,
            "blank replay stays bounded"
        );
        srv.abort();
    }

    /// A native Messages/Responses frame (or a non-streaming body) on the Chat Completions route is
    /// a protocol mismatch: it must be reported, not silently parsed as an empty answer.
    #[tokio::test]
    async fn a_foreign_dialect_is_a_protocol_error_not_an_empty_answer() {
        use crate::llm::media::tests as media_tests;
        for reply in [
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"content\":[]}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"choices\":[{\"message\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]\n\n",
        ] {
            let (base, receiver, server) = media_tests::request_receiver(reply, "text/event-stream").await;
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let cfg = crate::core::cli_config::CliConfig::default();
            let error = stream_chat_with_tools_eager_live(
                &client, &base, "fixture-key", "foreign-dialect", &[Message::user("hi")], &[],
                StreamSink::default(), None, &cfg,
            ).await.err().unwrap();
            assert!(
                error.downcast_ref::<crate::llm::delegated::Failure>()
                    == Some(&crate::llm::delegated::Failure::Protocol),
                "{error}"
            );
            assert!(error.to_string().contains("Chat Completions"), "{error}");
            // The single-connection fixture also proves no blank replay followed a protocol failure.
            receiver.await.unwrap();
            server.await.unwrap();
        }
    }

    /// `finish_reason` followed by an open connection: a bounded trailer wait that still surfaces a
    /// late error, instead of either waiting a whole response window or declaring success instantly.
    #[tokio::test]
    async fn finish_reason_with_an_open_connection_waits_only_a_bounded_trailer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (tail, expect_ok) in [
            ("", true),
            (
                "event: error\ndata: {\"error\":{\"code\":\"UPSTREAM_BAD_RESPONSE\",\"message\":\"sk-secret\"}}\n\n",
                false,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let srv = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                socket.write_all(head.as_bytes()).await.unwrap();
                socket
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n")
                    .await
                    .unwrap();
                if !tail.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    socket.write_all(tail.as_bytes()).await.unwrap();
                }
                socket.flush().await.unwrap();
                // Hold the connection open far past the trailer window, sending nothing more.
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            });
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let cfg = crate::core::cli_config::CliConfig::default();
            let started = std::time::Instant::now();
            let turn = stream_chat_with_tools_eager_live(
                &client, &base, "fixture-key", "finish-then-open", &[Message::user("hi")], &[],
                StreamSink::default(), None, &cfg,
            ).await;
            let elapsed = started.elapsed();
            assert!(elapsed < std::time::Duration::from_secs(60), "{elapsed:?}");
            match (expect_ok, turn) {
                (true, Ok(turn)) => assert_eq!(turn.content.as_deref(), Some("answer")),
                (false, Err(error)) => {
                    assert!(error.to_string().contains("UPSTREAM_BAD_RESPONSE"), "{error}");
                    assert!(!error.to_string().contains("sk-secret"));
                }
                (true, Err(error)) => panic!("healthy finish must not fail: {error}"),
                (false, Ok(_)) => panic!("a late provider error cannot be erased by an earlier finish"),
            }
            srv.abort();
        }
    }

    #[tokio::test]
    async fn quiet_stream_rejects_partial_eof_sse_errors_and_incomplete_calls() {
        use crate::llm::media::tests as media_tests;
        for (reply, expected) in [
            ("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n", "partial EOF"),
            ("data: {\"error\":{\"code\":\"UPSTREAM_BAD_RESPONSE\",\"status\":0,\"message\":\"sk-secret\",\"correlation_id\":\"req-19\"}}\n\n", "UPSTREAM_BAD_RESPONSE"),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n", "incomplete tool arguments"),
        ] {
            let (base, receiver, server) = media_tests::request_receiver(reply, "text/event-stream").await;
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let cfg = crate::core::cli_config::CliConfig::default();
            let error = stream_chat_with_tools_eager_live(&client, &base, "fixture-key", "quiet-failure", &[Message::user("hi")], &[], StreamSink::default(), None, &cfg).await.err().unwrap();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!error.to_string().contains("sk-secret"));
            receiver.await.unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn non_json_error_event_cannot_be_erased_by_later_finish_or_done() {
        use crate::llm::media::{tests as media_tests, with_image_delivery};
        for reply in [
            "event: error\ndata: sk-secret data:image/png;base64,PRIVATE\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"later\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\nevent: error\ndata: sk-secret data:image/png;base64,PRIVATE\n\ndata: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\nevent: error\ndata: [DONE]\n\n",
        ] {
            let (base, receiver, server) = media_tests::request_receiver(reply, "text/event-stream").await;
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let cfg = crate::core::cli_config::CliConfig::default();
            let messages = vec![Message::user_with_images("inspect", vec!["data:legacy".into()])];
            let ctx = crate::core::exec_ctx::ExecutionContext::default();
            let error = with_image_delivery(&ctx, &base, "fixture-key", "raw-sse-error", stream_chat_with_tools_eager_live(
                &client, &base, "fixture-key", "raw-sse-error", &messages, &[], StreamSink::default(), None, &cfg,
            )).await.err().unwrap();
            assert!(error.downcast_ref::<TransportFailure>().is_some());
            assert!(!error.to_string().contains("sk-secret"));
            assert!(!error.to_string().contains("PRIVATE"));
            receiver.await.unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn salvaged_chat_tools_clear_live_image_receipt() {
        use crate::llm::media::{tests as media_tests, with_image_delivery, ImageDeliveryReceipt};
        const REPLY: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n";
        let (base, receiver, server) =
            media_tests::request_receiver(REPLY, "text/event-stream").await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cfg = crate::core::cli_config::CliConfig::default();
        let messages = vec![Message::user_with_images(
            "inspect",
            vec!["data:legacy".into()],
        )];
        let ctx = crate::core::exec_ctx::ExecutionContext::default();
        let (turn, receipt) = with_image_delivery(
            &ctx,
            &base,
            "fixture-key",
            "salvage-image",
            stream_chat_with_tools_eager_live(
                &client,
                &base,
                "fixture-key",
                "salvage-image",
                &messages,
                &[],
                StreamSink::default(),
                None,
                &cfg,
            ),
        )
        .await
        .unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(receipt, ImageDeliveryReceipt::default());
        receiver.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retries_reuse_exact_compact_bytes_and_quirks_only_change_fields() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for (status, response) in [
                (503, "busy"),
                (
                    400,
                    "{\"error\":{\"message\":\"unsupported reasoning_effort\"}}",
                ),
                (200, "ok"),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0u8; 4096];
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let size: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + size {
                            bodies.push(bytes[end + 4..end + 4 + size].to_vec());
                            break;
                        }
                    }
                }
                let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            bodies
        });
        let cfg = crate::core::cli_config::CliConfig::default();
        let body = build_chat_body(
            &cfg,
            "quiet-quirk-bytes-fixture",
            &[Message::user("hi")],
            &[],
            true,
            Some("high".into()),
        );
        let mut prepared = PreparedChatRequest::new(body).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        send_prepared_chat(&client, &url, "fixture-key", &mut prepared, false)
            .await
            .unwrap();
        let bodies = server.await.unwrap();
        assert_eq!(
            bodies[0], bodies[1],
            "ordinary retries reuse the wire verbatim"
        );
        let mut first: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
        first.as_object_mut().unwrap().remove("reasoning_effort");
        assert_eq!(
            first,
            serde_json::from_slice::<serde_json::Value>(&bodies[2]).unwrap()
        );
        assert!(!bodies[0].contains(&b'\n'));
    }

    /// A one-request-per-connection stub gateway. `models_status`/`chat_status` let a test model the
    /// shape that actually bit us: a gateway that serves `/models` to anyone but enforces auth on
    /// `/chat/completions`.
    async fn stub_gateway(
        models_status: u16,
        chat_status: u16,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.contains("/chat/completions") {
                    match chat_status {
                        200 => (
                            200,
                            r#"{"choices":[{"message":{"content":"ok"}}]}"#.to_string(),
                        ),
                        c => (c, r#"{"error":{"message":"Invalid API key"}}"#.to_string()),
                    }
                } else {
                    match models_status {
                        200 => (200, r#"{"object":"list","data":[{"id":"m1"}]}"#.to_string()),
                        c => (c, r#"{"error":{"message":"nope"}}"#.to_string()),
                    }
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, handle)
    }

    /// The regression: a gateway that hands out its model list unauthenticated must NOT let a bad
    /// key pass config validation. Before the chat-probe, this returned `Ok` and the user only found
    /// out on their first real turn — which reads as "the endpoint and key are right but it doesn't
    /// work".
    #[tokio::test]
    async fn open_models_list_does_not_certify_a_key_the_chat_path_rejects() {
        let (base, srv) = stub_gateway(200, 401).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("sk-corrupted")).await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Auth(_)),
            "a 401 on /chat/completions must surface as Auth, got {got:?}"
        );
    }

    #[tokio::test]
    async fn a_key_both_endpoints_accept_still_validates() {
        let (base, srv) = stub_gateway(200, 200).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("sk-good")).await;
        srv.abort();
        match got {
            EndpointCheck::Ok(infos) => {
                assert_eq!(infos.len(), 1, "the model list is carried back")
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Only 401/403 counts. A 400 (e.g. the probe model isn't enabled for this account) must not be
    /// reported as a bad key, or we'd send the user off to re-paste a credential that was fine.
    #[tokio::test]
    async fn a_non_auth_chat_failure_does_not_condemn_the_key() {
        let (base, srv) = stub_gateway(200, 400).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("sk-good")).await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Ok(_)),
            "a 400 proves nothing about the key, got {got:?}"
        );
    }

    /// The chat-path key probe must prefer a `-free` variant when the list has one. Free-tier
    /// gateways (OpenCode zen, OpenRouter) mix paid ids into the same `/models` list, and probing a
    /// paid one with the shared free-tier credential can draw a 403 — which setup would report as
    /// "bad key" even though the token is exactly what that tier hands out.
    #[tokio::test]
    async fn key_probe_prefers_a_free_variant_over_the_first_paid_model() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let probed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = probed.clone();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.contains("/chat/completions") {
                    seen.lock().unwrap().push(req.clone());
                    (
                        200,
                        r#"{"choices":[{"message":{"content":"ok"}}]}"#.to_string(),
                    )
                } else {
                    // Paid model FIRST, so `infos.first()` — the old probe choice — is the paid id.
                    (
                        200,
                        r#"{"object":"list","data":[{"id":"claude-fable-5"},{"id":"deepseek-v4-flash-free"}]}"#
                            .to_string(),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("public")).await;
        srv.abort();
        assert!(matches!(got, EndpointCheck::Ok(_)), "got {got:?}");
        let posts = probed.lock().unwrap();
        assert_eq!(posts.len(), 1, "exactly one probe POST expected");
        assert!(
            posts[0].contains("\"model\":\"deepseek-v4-flash-free\""),
            "probe must target the -free variant, got: {}",
            posts[0]
        );
    }

    /// A gateway that accepts the connection and then says nothing must not hang interactive setup:
    /// the probe has its own deadline (the shared client's read timeout is sized for a streamed turn),
    /// and a timeout means "unproven", never "bad key".
    #[tokio::test]
    async fn a_stalled_chat_probe_times_out_without_condemning_the_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        // Answer /models, then accept the chat connection and never reply.
        let srv = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut held = Vec::new();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n]).contains("/chat/completions") {
                    held.push(sock); // hold it open, send nothing
                    continue;
                }
                let body = r#"{"object":"list","data":[{"id":"m1"}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::new();
        // A short injected deadline: the production 20s would just make this test slow, and what
        // matters is that the deadline is enforced at all and classified as inconclusive.
        let got = check_endpoint_with_deadline(
            &http,
            &base,
            Some("sk-unknown"),
            std::time::Duration::from_millis(300),
        )
        .await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Ok(_)),
            "a stalled probe is inconclusive, not a rejection, got {got:?}"
        );
    }

    /// With no key there is nothing to certify, so the base-URL-only check must stay a single request
    /// and keep classifying purely on the model list.
    #[tokio::test]
    async fn base_url_only_check_skips_the_chat_probe() {
        let (base, srv) = stub_gateway(200, 401).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, None).await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Ok(_)),
            "no key ⇒ reachability only, got {got:?}"
        );
    }
}
