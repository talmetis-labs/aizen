//! Attach-only browser automation over Chrome DevTools Protocol. Connections are lazy, isolated
//! per conversation, and never launch/install a browser or borrow a user's profile. All operations
//! share one cancellable 30-second budget, including discovery, ownership, retries, and capture.

use crate::agent::tools::{ApprovalPreview, Tool, ToolRegistry, WorkspaceEffect};
use crate::core::convo::ConversationId;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

mod capture;
mod config;
mod static_source;
mod transport;
pub use capture::{BrowserScreenshot, BrowserSetViewport};
#[cfg(test)]
mod transport_tests;

/// Per-op wall-clock cap so a wedged page can't freeze the agent loop.
const OP_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on snapshot nodes — the a11y tree of a big SPA is huge; keep the injected slice bounded.
const SNAPSHOT_MAX_NODES: usize = 200;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Upper bound on live browser sessions kept in the registry at once — one CDP websocket per
/// conversation. When a new conversation would exceed this, the least-recently-used idle session is
/// dropped (its websocket closes on drop). Serving is serial process-wide, so only the active
/// session is ever locked; every other handle is safe to evict.
const MAX_SESSIONS: usize = 8;

/// One conversation's browser state: its live CDP websocket (if connected), the profile pinned for
/// that conversation and a last-used stamp for LRU eviction. Kept behind `Arc<Mutex<…>>` so a long CDP op holds only THIS
/// conversation's lock, never a global one — state can never bleed from one conversation to another.
struct BrowserSession {
    client: Option<CdpClient>,
    /// Set true before awaiting transport I/O; a cancelled/aborted op leaves it set so the next op on
    /// this conversation reconnects on a fresh socket instead of reusing one left mid-exchange.
    needs_reconnect: bool,
    last_used: std::time::Instant,
}

impl BrowserSession {
    fn new() -> Self {
        Self {
            client: None,
            needs_reconnect: false,
            last_used: std::time::Instant::now(),
        }
    }
}

type SessionHandle = Arc<Mutex<BrowserSession>>;

/// Per-conversation CDP sessions. The registry lock (a std mutex) is held only to look up / insert a
/// handle, never across CDP I/O — the awaited op locks the per-session tokio mutex instead.
static SESSIONS: Lazy<std::sync::Mutex<HashMap<ConversationId, SessionHandle>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

/// Profile pinned per conversation by the latest navigate (or the config default). Kept independently
/// from the websocket so a transport reconnect stays on the same profile; a profile switch drops that
/// conversation's socket + `@ref`s. Sync access (no await) → a plain `RwLock`, keyed by conversation.
static PINNED: Lazy<std::sync::RwLock<HashMap<ConversationId, String>>> =
    Lazy::new(|| std::sync::RwLock::new(HashMap::new()));
static PINNED_TARGET_IDS: Lazy<std::sync::RwLock<HashMap<ConversationId, String>>> =
    Lazy::new(|| std::sync::RwLock::new(HashMap::new()));

/// Fetch (or lazily create) the session handle for one conversation, evicting the least-recently-used
/// idle session when the registry is at capacity. Never evicts a locked (in-use) session.
fn session_handle(id: &ConversationId) -> SessionHandle {
    let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = map.get(id) {
        return h.clone();
    }
    if map.len() >= MAX_SESSIONS {
        // LRU victim among the sessions we can lock (idle ones); the active session is locked and
        // thus skipped automatically. `try_lock` reads `last_used` without blocking.
        let victim = map
            .iter()
            .filter_map(|(k, h)| h.try_lock().ok().map(|g| (k.clone(), g.last_used)))
            .min_by_key(|(_, t)| *t)
            .map(|(k, _)| k);
        if let Some(v) = victim {
            map.remove(&v);
        }
    }
    let h: SessionHandle = Arc::new(Mutex::new(BrowserSession::new()));
    map.insert(id.clone(), h.clone());
    h
}

/// Release one conversation's browser state (drops its websocket + `@ref`s + pinned profile). Called
/// on `/new`, session deletion, and hostbot route removal so a retired conversation frees its socket.
pub fn release(id: &ConversationId) {
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
    PINNED.write().unwrap_or_else(|e| e.into_inner()).remove(id);
    PINNED_TARGET_IDS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
}

/// Release the conversation currently marked active (the REPL's `/new` / reset path).
pub fn release_active() {
    release(&crate::core::convo::active());
}

struct CdpClient {
    ws: WsStream,
    next_id: u64,
    /// Cross-process ownership for this endpoint/profile/target; held for the websocket lifetime.
    _ownership: crate::core::repo_lock::RepoTxnLock,
    /// Named profile pinned for the lifetime of this websocket session.
    profile: String,
    /// Sanitized endpoint label (host/base only; never headers or credential values).
    endpoint: String,
    selection: config::BrowserProfile,
    target_id: String,
    budget: transport::Budget,
    viewport: Option<capture::Viewport>,
    /// `@ref` number → DOM backendNodeId, rebuilt by each `browser_snapshot`.
    refs: HashMap<u32, i64>,
}

impl CdpClient {
    /// Send/read bounded by the operation's one total deadline, not a fresh timeout per command.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        transport::call(self, method, params).await
    }

    /// Resolve a snapshot `@ref` to a live JS object id (for click/type via `Runtime.callFunctionOn`).
    async fn object_for_ref(&mut self, r: u32) -> Result<String> {
        let backend = *self.refs.get(&r).ok_or_else(|| {
            anyhow!("unknown @ref @{r} — call browser_snapshot first (refs reset every snapshot)")
        })?;
        let resolved = self
            .call("DOM.resolveNode", json!({"backendNodeId": backend}))
            .await?;
        resolved
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("@{r} is no longer in the DOM — re-snapshot the page"))
    }
}

/// Discover + lock + attach within the caller's total deadline. Lock attempts are nonblocking and
/// separated by cancellable async waits, so no synchronous lock retry can strand cancellation.
async fn connect(
    profile_name: String,
    profile: config::BrowserProfile,
    budget: transport::Budget,
) -> Result<CdpClient> {
    let target = transport::discover(&profile_name, &profile, &budget).await?;
    // Identity is the selected transport target, not an alias/profile label. Two configurations or
    // conversations cannot accidentally own the same websocket simultaneously.
    let ownership_path = crate::core::workspace_txn::resource_lock("browser", &target.ws_url);
    let ownership = budget
        .run(async {
            loop {
                match crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                    &ownership_path,
                    Duration::ZERO,
                ) {
                    Ok(lock) => break Ok(lock),
                    Err(e)
                        if e.downcast_ref::<crate::core::repo_lock::LockBusy>()
                            .is_some() =>
                    {
                        tokio::time::sleep(Duration::from_millis(25)).await
                    }
                    Err(e) => break Err(e),
                }
            }
        })
        .await?;
    let ws = transport::ws_connect(&profile_name, &profile, &target.ws_url, &budget).await?;
    let mut c = CdpClient {
        ws,
        next_id: 1,
        _ownership: ownership,
        profile: profile_name,
        endpoint: profile.endpoint.clone(),
        selection: profile,
        target_id: target.id,
        budget,
        viewport: None,
        refs: HashMap::new(),
    };
    c.call("Page.enable", json!({})).await?;
    c.call("DOM.enable", json!({})).await?;
    c.call("Runtime.enable", json!({})).await?;
    Ok(c)
}

/// Run an op against the ACTIVE conversation's profile-pinned connection. `retry_readonly` permits
/// ONE reconnect + replay only for observational operations (`browser_snapshot` / status probes).
/// State-changing navigate/click/type/eval operations drop a broken connection but never replay after
/// transport ambiguity — their effect may already have happened. Switching profiles drops the prior
/// websocket and clears all @refs, making cross-profile refs unambiguously invalid. Every session is
/// keyed by conversation, so one chat's page/refs can never bleed into another's.
async fn with_cdp<T>(
    profile_name: String,
    profile: config::BrowserProfile,
    retry_readonly: bool,
    mut f: impl for<'a> FnMut(&'a mut CdpClient) -> Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
) -> Result<T> {
    let budget = transport::Budget::new(OP_TIMEOUT);
    let bounded = budget.clone();
    bounded
        .run(async move {
            let handle = session_handle(&crate::core::convo::active());
            let mut sess = handle.lock().await;
            sess.last_used = std::time::Instant::now();
            let switched = sess
                .client
                .as_ref()
                .is_some_and(|c| c.profile != profile_name || c.selection != profile);
            let old_target = if !switched {
                sess.client.as_ref().map(|c| c.target_id.clone())
            } else {
                None
            };
            if switched || sess.needs_reconnect {
                sess.client = None;
                sess.needs_reconnect = false;
            }
            if sess.client.is_none() {
                let mut reconnect_profile = profile.clone();
                if reconnect_profile.target_id.is_none() {
                    reconnect_profile.target_id = old_target;
                }
                let mut client =
                    connect(profile_name.clone(), reconnect_profile, budget.clone()).await?;
                client.selection = profile.clone();
                sess.client = Some(client);
            }
            sess.client.as_mut().unwrap().budget = budget.clone();
            // Survives cancellation of this future; the next call cannot reuse a half-read exchange.
            sess.needs_reconnect = true;
            let first = f(sess.client.as_mut().unwrap()).await;
            let broken = first.as_ref().err().is_some_and(connection_error);
            if !broken {
                sess.needs_reconnect = first.is_err();
                return first;
            }
            let target_id = sess.client.as_ref().unwrap().target_id.clone();
            sess.client = None;
            if !retry_readonly {
                return first;
            }
            let mut replay_profile = profile.clone();
            replay_profile.target_id = Some(target_id);
            let mut client = connect(profile_name, replay_profile, budget).await?;
            client.selection = profile;
            sess.client = Some(client);
            sess.needs_reconnect = true;
            let second = f(sess.client.as_mut().unwrap()).await;
            sess.needs_reconnect = second.is_err();
            second
        })
        .await
}

fn connection_error(e: &anyhow::Error) -> bool {
    let m = e.to_string();
    m.contains("closed")
        || m.contains("read error")
        || m.contains("CDP send")
        || m.contains("timed out")
}

/// Resolve explicit profile/target arguments without networking. Pins are conversation-local; switching
/// profile clears target selection and all live refs on the subsequent attach.
fn operation_target(
    args: &Value,
    navigate_url: Option<&str>,
) -> Result<(String, config::BrowserProfile)> {
    // Load the explicit authorization snapshot once, before mutating pins or any network I/O.
    let cfg = config::load_explicit()?;
    operation_target_from_config(&cfg, args, navigate_url)
}

fn operation_target_from_config(
    cfg: &config::BrowserConfig,
    args: &Value,
    navigate_url: Option<&str>,
) -> Result<(String, config::BrowserProfile)> {
    let id = crate::core::convo::active();
    let (name, mut profile) = if let Some(name) = args.get("profile") {
        let name = name.as_str().context("profile must be a string")?;
        (name.to_string(), config::resolve_named_snapshot(cfg, name)?)
    } else if let Some(url) = navigate_url {
        config::resolve_for_url_snapshot(cfg, url)?
    } else {
        let name = PINNED
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .unwrap_or_else(|| cfg.default_profile.clone());
        let profile = config::resolve_named_snapshot(cfg, &name)?;
        (name, profile)
    };
    let changed = PINNED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .is_some_and(|n| n != &name);
    if changed {
        PINNED_TARGET_IDS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
    if let Some(target) = args.get("target_id") {
        let target = target
            .as_str()
            .context("target_id must be a string")?
            .to_string();
        profile.target_id = Some(target.clone());
        config::validate_profile(&profile)?;
        PINNED_TARGET_IDS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), target);
    } else if let Some(target) = PINNED_TARGET_IDS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
    {
        profile.target_id = Some(target.clone());
    }
    config::validate_profile(&profile)?;
    PINNED
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, name.clone());
    Ok((name, profile))
}

fn tool_parameters(mut schema: Value) -> Value {
    let props = schema["properties"].as_object_mut().unwrap();
    props.insert(
        "profile".into(),
        json!({"type":"string","description":"Configured attach profile"}),
    );
    props.insert(
        "target_id".into(),
        json!({"type":"string","description":"Existing CDP page id; required if ambiguous"}),
    );
    schema
}

fn navigate_url(raw: &str) -> Result<url::Url> {
    if raw.len() > 8192 {
        bail!("navigation URL is too long");
    }
    let url = url::Url::parse(raw).context("navigation requires an absolute URL")?;
    if !matches!(url.scheme(), "http" | "https" | "file") {
        bail!("navigation permits only http, https, or explicit file URLs");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("navigation URL userinfo is refused");
    }
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_none() {
        bail!("navigation URL has no host");
    }
    Ok(url)
}

fn mark_design_render_changed() {
    if let Some(design) = crate::core::exec_ctx::current().and_then(|ctx| ctx.design()) {
        design.mark_render_changed();
    }
}

fn browser_preview(name: &str, args: &Value, action: &str) -> Option<ApprovalPreview> {
    let mut lines = vec![action.to_string(),
        format!("profile: {} · target: {}", args.get("profile").and_then(Value::as_str).unwrap_or("conversation/config default"),
            args.get("target_id").and_then(Value::as_str).unwrap_or("pinned/config target; refuse ambiguity")),
        "Attach only; never launches or installs a browser. Attached page state may persist after cancellation.".into()];
    if let Some(url) = args.get("url").and_then(Value::as_str) {
        lines.push(match navigate_url(url) {
            Ok(url) => format!("navigate: {url}"),
            Err(_) => "invalid navigation URL (will refuse without connecting)".into(),
        });
    }
    Some(ApprovalPreview {
        title: name.into(),
        diff: None,
        lines,
    })
}

/// Sync bridge; CDP's own Budget races cancellation, so its retained-state explanation is not
/// hidden by the generic outer bridge's cancellation error.
fn block<T>(f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Parse a `@ref` argument: accepts `3`, `"3"`, or `"@3"`.
fn parse_ref(v: &Value) -> Result<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).map_err(|_| anyhow!("ref out of range"));
    }
    let s = v
        .as_str()
        .context("'ref' must be a number or \"@N\" string")?;
    s.trim()
        .trim_start_matches('@')
        .parse::<u32>()
        .with_context(|| format!("bad ref '{s}'"))
}

/// Roles worth surfacing even without an accessible name (interactive controls).
fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "option"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "switch"
            | "slider"
            | "spinbutton"
            | "textfield"
    )
}

/// Render an `Accessibility.getFullAXTree` node list into a compact, ref-tagged outline, populating
/// `refs` (ref number → backendNodeId). Pure (no IO) so it is unit-testable. Caps at `max` nodes.
fn render_ax_tree(nodes: &[Value], refs: &mut HashMap<u32, i64>, max: usize) -> String {
    refs.clear();
    let mut out = String::new();
    let mut n: u32 = 0;
    for node in nodes {
        if node
            .get("ignored")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let role = node
            .get("role")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let name = node
            .get("name")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if role.is_empty() {
            continue;
        }
        // surface named nodes + interactive controls; skip pure structural noise
        if name.is_empty() && !is_interactive_role(role) {
            continue;
        }
        let backend = match node.get("backendDOMNodeId").and_then(|v| v.as_i64()) {
            Some(b) => b,
            None => continue,
        };
        n += 1;
        refs.insert(n, backend);
        let name_disp: String = name.chars().take(80).collect();
        if name_disp.is_empty() {
            out.push_str(&format!("[@{n}] {role}\n"));
        } else {
            out.push_str(&format!("[@{n}] {role} \"{name_disp}\"\n"));
        }
        if n as usize >= max {
            out.push_str(&format!(
                "… (truncated at {max} nodes — interact via @ref, or browser_eval for more)\n"
            ));
            break;
        }
    }
    if n == 0 {
        return "(no accessible elements found — the page may be blank or still loading)"
            .to_string();
    }
    out.trim_end().to_string()
}

// ── tools ────────────────────────────────────────────────────────────────────────

pub struct BrowserNavigate;
impl Tool for BrowserNavigate {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Open a URL in the connected local browser (Chrome/Edge/Brave launched with \
         --remote-debugging-port). Waits for the page to load, returns its title. Use to start a \
         browsing/debug session (e.g. your localhost dev server); then browser_snapshot to see the page."
    }
    fn parameters(&self) -> Value {
        tool_parameters(
            json!({"type":"object","properties":{"url":{"type":"string","description":"absolute URL (http/https/file)"}},"required":["url"],"additionalProperties":false}),
        )
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::External
    }
    fn preview(&self, args: &Value) -> Option<ApprovalPreview> {
        browser_preview(self.name(), args, "Mutates the explicitly attached browser page; effects are never replayed after transport errors.")
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .context("missing 'url'")?
            .to_string();
        let url = navigate_url(&url)?.to_string();
        let (profile_name, profile) = operation_target(args, Some(&url))?;
        mark_design_render_changed();
        block(with_cdp(profile_name, profile, false, |c| {
            Box::pin({
                let url = url.clone();
                async move {
                    c.refs.clear();
                    let result = c.call("Page.navigate", json!({"url": url})).await?;
                    if result
                        .get("errorText")
                        .and_then(Value::as_str)
                        .is_some_and(|e| !e.is_empty())
                    {
                        bail!("browser navigation failed");
                    }
                    let readiness = capture::settle(c).await?;
                    let state = capture::page_state(c).await?;
                    Ok(json!({"action":"navigated", "url":state["url"], "title":state["title"],
                        "readiness":readiness,"freshness":"unknown", "note":"Source freshness is not inferred from navigation; browser_snapshot to see elements"}).to_string())
                }
            })
        }))
    }
}

pub struct BrowserSnapshot;
impl Tool for BrowserSnapshot {
    fn name(&self) -> &str {
        "browser_snapshot"
    }
    fn description(&self) -> &str {
        "Read the current page's accessibility tree as a compact outline of `[@ref] role \"name\"` \
         lines. The @ref ids are how you act on elements (browser_click/browser_type). Read-only. \
         Re-run after the page changes — refs are reassigned every snapshot."
    }
    fn parameters(&self) -> Value {
        tool_parameters(json!({"type":"object","properties":{},"additionalProperties":false}))
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let (profile_name, profile) = operation_target(args, None)?;
        block(with_cdp(profile_name, profile, true, |c| {
            Box::pin(async move {
                let tree = c.call("Accessibility.getFullAXTree", json!({})).await?;
                let nodes = tree
                    .get("nodes")
                    .and_then(|n| n.as_array())
                    .cloned()
                    .unwrap_or_default();
                Ok(render_ax_tree(&nodes, &mut c.refs, SNAPSHOT_MAX_NODES))
            })
        }))
    }
}

pub struct BrowserClick;
impl Tool for BrowserClick {
    fn name(&self) -> &str {
        "browser_click"
    }
    fn description(&self) -> &str {
        "Click an element by its @ref from the latest browser_snapshot. Scrolls it into view first. \
         Re-snapshot afterwards to see the result."
    }
    fn parameters(&self) -> Value {
        tool_parameters(
            json!({"type":"object","properties":{"ref":{"type":["integer","string"],"description":"the @ref number from browser_snapshot"}},"required":["ref"],"additionalProperties":false}),
        )
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::External
    }
    fn preview(&self, args: &Value) -> Option<ApprovalPreview> {
        browser_preview(self.name(), args, "Mutates the explicitly attached browser page; effects are never replayed after transport errors.")
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let r = parse_ref(args.get("ref").context("missing 'ref'")?)?;
        let (profile_name, profile) = operation_target(args, None)?;
        mark_design_render_changed();
        block(with_cdp(profile_name, profile, false, |c| {
            Box::pin(async move {
                let object_id = c.object_for_ref(r).await?;
                c.call(
                "Runtime.callFunctionOn",
                json!({"objectId": object_id, "functionDeclaration": "function(){ this.scrollIntoView({block:'center'}); this.click(); }"}),
            )
            .await?;
                Ok(format!("clicked @{r} (browser_snapshot to see the result)"))
            })
        }))
    }
}

pub struct BrowserType;
impl Tool for BrowserType {
    fn name(&self) -> &str {
        "browser_type"
    }
    fn description(&self) -> &str {
        "Type text into an input/textarea by its @ref from the latest browser_snapshot (focuses, \
         sets the value, and fires input+change events). For buttons/links use browser_click."
    }
    fn parameters(&self) -> Value {
        tool_parameters(
            json!({"type":"object","properties":{"ref":{"type":["integer","string"]},"text":{"type":"string"}},"required":["ref","text"],"additionalProperties":false}),
        )
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::External
    }
    fn preview(&self, args: &Value) -> Option<ApprovalPreview> {
        browser_preview(self.name(), args, "Mutates the explicitly attached browser page; effects are never replayed after transport errors.")
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let r = parse_ref(args.get("ref").context("missing 'ref'")?)?;
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .context("missing 'text'")?
            .to_string();
        let (profile_name, profile) = operation_target(args, None)?;
        mark_design_render_changed();
        block(with_cdp(profile_name, profile, false, |c| {
            Box::pin({
                let text = text.clone();
                async move {
                    let object_id = c.object_for_ref(r).await?;
                    c.call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function(t){ this.focus(); if('value' in this){ this.value=t; this.dispatchEvent(new Event('input',{bubbles:true})); this.dispatchEvent(new Event('change',{bubbles:true})); } else { this.textContent=t; } }",
                    "arguments": [{"value": text}]
                }),
            )
            .await?;
                    Ok(format!("typed into @{r}"))
                }
            })
        }))
    }
}

pub struct BrowserEval;
impl Tool for BrowserEval {
    fn name(&self) -> &str {
        "browser_eval"
    }
    fn description(&self) -> &str {
        "Run a JavaScript expression in the current page and return its result (JSON when possible). \
         The debug power tool: read DOM/localStorage/app state, await fetches, inspect errors. Prefer \
         browser_click/browser_type for normal interaction; use this when a snapshot can't express it."
    }
    fn parameters(&self) -> Value {
        tool_parameters(
            json!({"type":"object","properties":{"expression":{"type":"string","description":"a JS expression; may be an async expression / Promise"}},"required":["expression"],"additionalProperties":false}),
        )
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::External
    }
    fn preview(&self, args: &Value) -> Option<ApprovalPreview> {
        browser_preview(self.name(), args, "Mutates the explicitly attached browser page; effects are never replayed after transport errors.")
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let expr = args
            .get("expression")
            .and_then(|v| v.as_str())
            .context("missing 'expression'")?
            .to_string();
        let (profile_name, profile) = operation_target(args, None)?;
        mark_design_render_changed();
        block(with_cdp(profile_name, profile, false, |c| {
            Box::pin({
                let expr = expr.clone();
                async move {
                    let r = c
                .call(
                    "Runtime.evaluate",
                    json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
                )
                .await?;
                    if let Some(ex) = r.get("exceptionDetails") {
                        let txt = ex
                            .get("exception")
                            .and_then(|e| e.get("description"))
                            .and_then(|v| v.as_str())
                            .or_else(|| ex.get("text").and_then(|v| v.as_str()))
                            .unwrap_or("JS exception");
                        return Ok(format!("error: {txt}"));
                    }
                    let result = r.get("result").cloned().unwrap_or(Value::Null);
                    // returnByValue → a `value`; else fall back to the type/description.
                    if let Some(v) = result.get("value") {
                        Ok(serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
                    } else {
                        let desc = result
                            .get("description")
                            .and_then(|v| v.as_str())
                            .or_else(|| result.get("type").and_then(|v| v.as_str()))
                            .unwrap_or("undefined");
                        Ok(desc.to_string())
                    }
                }
            })
        }))
    }
}

/// Sanitized browser routing/session status. Never includes Authorization values or environment
/// variable contents — only profile labels, endpoint labels, route names, and whether auth is set.
pub fn status() -> String {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return format!("browser config error: {e}"),
    };
    // Report only the ACTIVE conversation's pin + session — never another chat's state.
    let id = crate::core::convo::active();
    let pinned = PINNED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    let availability = if is_configured() {
        "configured; tools available when enabled"
    } else {
        "unconfigured; tools not advertised (explicit CDP endpoint/profile required)"
    };
    let mut out = format!(
        "browser: {availability}\nbrowser config: {} · default={} · convo={} · pinned={}\n",
        config::config_path().display(),
        cfg.default_profile,
        id,
        pinned.as_deref().unwrap_or("(none)"),
    );
    // Look up (without creating) this conversation's session handle, then read its liveness without
    // blocking on an in-flight op.
    let handle = SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    let (session, endpoint): (&str, String) = match handle {
        Some(h) => match h.try_lock() {
            Ok(g) => g
                .client
                .as_ref()
                .map(|c| ("connected", c.endpoint.clone()))
                .unwrap_or_else(|| ("disconnected", "-".to_string())),
            Err(_) => ("busy", "-".to_string()),
        },
        None => ("disconnected", "-".to_string()),
    };
    let live = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).len();
    out.push_str(&format!(
        "session: {session} · endpoint {endpoint} · {live} live session(s)\n"
    ));
    for (name, p) in &cfg.profiles {
        let auth = p
            .auth_env
            .as_deref()
            .map(|v| {
                if std::env::var_os(v).is_some() {
                    format!("auth-env {v}=set")
                } else {
                    format!("auth-env {v}=missing")
                }
            })
            .unwrap_or_else(|| "no auth".to_string());
        out.push_str(&format!(
            "● {name} · {} · {} · {auth}\n",
            p.provider, p.endpoint
        ));
    }
    for (host, profile) in &cfg.routes {
        out.push_str(&format!("  route {host} → {profile}\n"));
    }
    out.trim_end().to_string()
}

pub async fn doctor() -> String {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return format!("browser config error: {e}"),
    };
    let mut out = String::new();
    for name in cfg.profiles.keys() {
        let profile = match config::resolve_named(&cfg, name) {
            Ok(p) => p,
            Err(e) => {
                out.push_str(&format!("○ {name} · {e}\n"));
                continue;
            }
        };
        let budget = transport::Budget::new(OP_TIMEOUT);
        match transport::discover(name, &profile, &budget).await {
            Ok(_) => out.push_str(&format!("● {name} · reachable · {}\n", profile.endpoint)),
            Err(e) => out.push_str(&format!("○ {name} · unavailable · {e}\n")),
        }
    }
    if out.is_empty() {
        "no browser profiles configured".to_string()
    } else {
        out.trim_end().to_string()
    }
}

/// Purely offline availability check for default registry advertisement. No discovery/attach/probe.
pub fn is_configured() -> bool {
    let endpoint = crate::core::cli_config::branded_env("BROWSER_CDP");
    config::explicitly_configured(&config::config_path(), endpoint.as_deref())
}

/// Register the browser tools into a registry (top-level only). Called from `default_registry_in`
/// under `#[cfg(feature = "browser")]`.
pub fn register_browser_tools(r: &mut ToolRegistry) {
    r.register(Box::new(BrowserNavigate));
    r.register(Box::new(BrowserSnapshot));
    r.register(Box::new(BrowserClick));
    r.register(Box::new(BrowserType));
    r.register(Box::new(BrowserEval));
    r.register(Box::new(BrowserSetViewport));
    r.register(Box::new(BrowserScreenshot));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_execution_loader_refuses_before_pin_and_snapshot_resolution_is_offline() {
        let id = ConversationId::new("browser-explicit-config-test");
        let ctx = crate::core::exec_ctx::ExecutionContext::new(id.clone());
        let root = std::env::temp_dir().join(format!(
            "aizen-browser-pin-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("browser.json");
        release(&id);
        crate::core::exec_ctx::with_current(ctx, || {
            let result = config::load_explicit_at(&path, None)
                .and_then(|cfg| operation_target_from_config(&cfg, &json!({}), None));
            assert!(result.is_err());
            assert!(PINNED.read().unwrap().get(&id).is_none());
            assert!(PINNED_TARGET_IDS.read().unwrap().get(&id).is_none());
            let mut cfg = config::BrowserConfig::default();
            cfg.profiles
                .get_mut(config::DEFAULT_PROFILE)
                .unwrap()
                .endpoint = "127.0.0.1:9666".into();
            std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
            let snapshot = config::load_explicit_at(&path, None).unwrap();
            std::fs::remove_file(&path).unwrap();
            let (_, target) =
                operation_target_from_config(&snapshot, &json!({"target_id":"selected"}), None)
                    .unwrap();
            assert_eq!(target.endpoint, "127.0.0.1:9666");
            assert_eq!(target.target_id.as_deref(), Some("selected"));
        });
        release(&id);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parse_ref_accepts_number_and_at_string() {
        assert_eq!(parse_ref(&json!(3)).unwrap(), 3);
        assert_eq!(parse_ref(&json!("3")).unwrap(), 3);
        assert_eq!(parse_ref(&json!("@7")).unwrap(), 7);
        assert!(parse_ref(&json!("nope")).is_err());
        assert!(parse_ref(&json!(true)).is_err());
    }

    #[test]
    fn render_ax_tree_tags_refs_and_skips_noise() {
        let nodes = vec![
            json!({"role":{"value":"button"},"name":{"value":"Submit"},"backendDOMNodeId":11}),
            json!({"role":{"value":"generic"},"name":{"value":""},"backendDOMNodeId":12}), // unnamed non-interactive → skipped
            json!({"ignored":true,"role":{"value":"link"},"name":{"value":"x"},"backendDOMNodeId":13}), // ignored → skipped
            json!({"role":{"value":"textbox"},"name":{"value":""},"backendDOMNodeId":14}), // interactive, unnamed → kept
            json!({"role":{"value":"heading"},"name":{"value":"Welcome"},"backendDOMNodeId":15}),
        ];
        let mut refs = HashMap::new();
        let out = render_ax_tree(&nodes, &mut refs, 200);
        assert!(out.contains("[@1] button \"Submit\""));
        assert!(
            out.contains("[@2] textbox"),
            "interactive unnamed control is kept: {out}"
        );
        assert!(out.contains("[@3] heading \"Welcome\""));
        assert!(
            !out.contains("generic"),
            "unnamed non-interactive node is skipped"
        );
        // refs map the rendered order to the right backend node ids
        assert_eq!(refs.get(&1), Some(&11));
        assert_eq!(refs.get(&2), Some(&14));
        assert_eq!(refs.get(&3), Some(&15));
    }

    #[test]
    fn render_ax_tree_caps_node_count() {
        let nodes: Vec<Value> = (0..50)
            .map(|i| json!({"role":{"value":"button"},"name":{"value":format!("b{i}")},"backendDOMNodeId":i}))
            .collect();
        let mut refs = HashMap::new();
        let out = render_ax_tree(&nodes, &mut refs, 5);
        assert!(out.contains("truncated at 5 nodes"));
        assert_eq!(refs.len(), 5, "refs bounded to the cap");
    }

    #[test]
    fn render_ax_tree_empty_is_explained() {
        let mut refs = HashMap::new();
        let out = render_ax_tree(&[], &mut refs, 200);
        assert!(out.contains("no accessible elements"));
    }

    #[test]
    fn browser_status_never_prints_credential_values() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("aizen-browser-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("AIZEN_HOME", &root);
        std::env::set_var("BROWSER_TEST_AUTH", "Bearer top-secret-value");
        std::fs::write(
            root.join("browser.json"),
            r#"{"schema":1,"default_profile":"remote","profiles":{"remote":{"provider":"cdp","endpoint":"https://cdp.example","auth_env":"BROWSER_TEST_AUTH"}},"routes":{}}"#,
        )
        .unwrap();
        let out = status();
        assert!(out.contains("BROWSER_TEST_AUTH=set"));
        assert!(!out.contains("top-secret-value"));
        std::env::remove_var("BROWSER_TEST_AUTH");
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sessions_are_keyed_per_conversation_and_released() {
        // Two conversations get DISTINCT session handles — one chat's page/refs can never be the
        // other's. Same id returns the SAME handle (state persists within a conversation).
        let a = ConversationId::new("test-convo-a");
        let b = ConversationId::new("test-convo-b");
        release(&a);
        release(&b);
        let ha = session_handle(&a);
        let hb = session_handle(&b);
        assert!(
            !Arc::ptr_eq(&ha, &hb),
            "distinct conversations must not share a session"
        );
        assert!(
            Arc::ptr_eq(&ha, &session_handle(&a)),
            "same conversation reuses its handle"
        );
        // Release drops the mapping; the next lookup is a fresh handle.
        release(&a);
        assert!(
            !Arc::ptr_eq(&ha, &session_handle(&a)),
            "release retires the old session"
        );
        release(&a);
        release(&b);
    }

    #[test]
    fn pinned_profile_is_per_conversation() {
        // A profile pinned under one conversation is invisible to another (no cross-chat bleed).
        let a = ConversationId::new("test-pin-a");
        let b = ConversationId::new("test-pin-b");
        PINNED
            .write()
            .unwrap()
            .insert(a.clone(), "profile-a".to_string());
        assert_eq!(
            PINNED.read().unwrap().get(&a).map(String::as_str),
            Some("profile-a")
        );
        assert!(
            PINNED.read().unwrap().get(&b).is_none(),
            "b never sees a's pin"
        );
        release(&a);
        assert!(
            PINNED.read().unwrap().get(&a).is_none(),
            "release clears the pin too"
        );
        release(&b);
    }
}
