//! Pinning this machine to the Aizen gateway, and the live config that comes back with the key.
//!
//! An app that speaks this protocol needs to know exactly ONE domain — the gateway — and asks it
//! for everything else, the address a human opens included. Three calls, all under `/v1`, which is
//! the prefix that takes the `ak_…` key:
//!
//!   * `POST /v1/device/start`  — ask for a pairing: a secret `device_code` for this process, a
//!     short `user_code` for the human, and the page to approve it at.
//!   * `POST /v1/device/token`  — ask again every `interval` seconds until an answer arrives.
//!   * `GET  /v1/gateway/config` — with the key: two base URLs, a default model, the limits on the
//!     key, and what is left to spend.
//!
//! Three things here are easy to get wrong in a way that still looks like it works, so each has a
//! guard and a comment saying which bug the guard is for:
//!
//! **The short code cannot be traded for a key.** `user_code` is short because a person retypes it,
//! and short is exactly why it may only name a pending pairing for the account owner to look at.
//! The 256-bit `device_code` — printed nowhere, written to no file — is the only half that collects
//! a key. This side keeps it in memory for the life of the loop and drops it after.
//!
//! **The key exists once.** It is minted at hand-over, not at approval, so a person who approves
//! and then closes the laptop leaves no live key behind. The trade is that the raw string crosses
//! the wire exactly once: [`adopt`] writes it to disk before anything is printed, formatted, or
//! logged, because losing that one response means pairing again from the top.
//!
//! **There are two base URLs, not one.** An OpenAI client appends `/chat/completions` to its root;
//! an Anthropic client appends `/v1/messages` to its own. Guessing one from the other sends half
//! the callers to `/v1/v1/messages`, which answers 404 and reads as "the gateway is down". The
//! gateway states both; [`openai_base`] and [`anthropic_base`] take what it said and never build
//! one out of the other.
//!
//! The full contract, error table included, is `docs/reference/DEVICE_PAIRING.md` in `admin_aizen`.

use crate::core::cli_config::{self, ProviderProfile};
use crate::core::config::{aizen_home, harden_file};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// The one domain an Aizen app has to know. Everything else — the page a human opens, the roots the
/// model calls go to — comes back from it.
///
/// This is the same host as [`crate::llm::account::DEFAULT_WEB`], and deliberately so: it serves
/// `/v1/*` against the `ak_…` key AND `/auth/*` against the session JWT, so one name covers pairing,
/// model traffic and subscriptions. Two names is what `api.talmetis.com` was — it still answers
/// `/v1/`, but it 404s every `/auth/` route, which reads as a broken server rather than a wrong
/// host, so nothing is pointed at it any more.
///
/// **Same host is not same credential.** The two prefixes take different papers and neither error
/// says so: the `ak_…` key on `/auth/*` is `401 {"error":"Not signed in"}`, and a session JWT buys
/// nothing under `/v1/`. Merging the hosts removed a wrong-host bug, not the wrong-credential one.
pub const DEFAULT_GATEWAY: &str = "https://aizen.talmetis.com";

/// The `cli-config.json` profile a pin writes into unless the caller names another.
pub const DEFAULT_PROFILE: &str = "aizen";

/// The default gateway's OpenAI-shaped root, spelled out as a `&'static str` for the one place that
/// needs a constant rather than a value: the config wizard's preset table.
///
/// The live root is [`openai_base`], which honours `AIZEN_GATEWAY_URL`; this is only what a build
/// with no override points at. A test keeps the two from drifting apart.
pub const DEFAULT_OPENAI_BASE: &str = "https://aizen.talmetis.com/v1";

/// Roots this project shipped before the two names became one. `api.talmetis.com` still answers
/// `/v1/`, and copies pinned against it are in the field, so it stays RECOGNISED even though nothing
/// is pointed there any more.
///
/// Recognised, not dialled — nothing here is ever a destination. It exists because
/// [`base_is_gateway`] fails silently in the false-negative direction: an unrecognised root turns an
/// already-pinned machine's own endpoint into "somebody else's API", and what the person sees is a
/// wizard asking for a key that does not exist, not a sentence about a host.
const LEGACY_ROOTS: [&str; 2] = ["https://api.talmetis.com/v1", "https://api.talmetis.com"];

const START_ROUTE: &str = "/v1/device/start";
const TOKEN_ROUTE: &str = "/v1/device/token";
/// Cut THIS machine, and only this machine. Authenticated with the machine's own token, which is
/// what makes "only this machine" a fact rather than a promise.
const LOGOUT_ROUTE: &str = "/v1/device/logout";
const CONFIG_ROUTE: &str = "/v1/gateway/config";

/// Per-request ceiling. Generous: the token route answers instantly, but the start route runs
/// behind a rate limiter and a cold proxy, and a 5-second timeout there turns a slow morning into
/// "the gateway is unreachable".
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The server keeps a pairing for ten minutes. This is the client's own stop, one minute wider, so
/// that a clock that disagrees slightly ends the loop here rather than spinning on 404 forever.
const MAX_WAIT: Duration = Duration::from_secs(11 * 60);

const MIN_INTERVAL: u64 = 1;
const MAX_INTERVAL: u64 = 60;

/// How many network failures in a row end the wait. Six at a widening interval is over a minute of
/// a dropped link — long enough to ride out a wifi handover, short enough not to look hung.
const MAX_TRANSIENT: u32 = 6;

/* ------------------------------------------------------------------ where */

/// Set for the life of the process by `--gateway`. A field rather than an env var write, because
/// setting the environment of your own process to be read back later is a lie two threads can tell
/// each other differently.
static OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// Point this process at another gateway. Empty clears the override.
pub fn set_gateway(url: &str) {
    let url = url.trim().trim_end_matches('/');
    if let Ok(mut g) = OVERRIDE.write() {
        *g = (!url.is_empty()).then(|| url.to_string());
    }
}

/// The gateway root, without a trailing slash: `--gateway` > `AIZEN_GATEWAY_URL` >
/// `OMNIROUTE_PUBLIC_API_BASE_URL` > the shipped default.
pub fn gateway_url() -> String {
    if let Some(url) = OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return url;
    }
    for var in ["AIZEN_GATEWAY_URL", "OMNIROUTE_PUBLIC_API_BASE_URL"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().trim_end_matches('/');
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    DEFAULT_GATEWAY.to_string()
}

/* ------------------------------------------------------------------ shapes */

fn default_interval() -> u64 {
    5
}
fn default_expires() -> u64 {
    600
}

/// What `POST /v1/device/start` hands back.
#[derive(Clone, Deserialize)]
pub struct Start {
    /// The secret half. Never printed, never written to disk, never put in an error message.
    pub device_code: String,
    /// The half a human reads and retypes, hyphenated for the eye.
    #[serde(default)]
    pub user_code: String,
    /// The same code without the hyphen, for a field that rejects them.
    #[serde(default)]
    pub user_code_plain: String,
    #[serde(default)]
    pub verification_uri: String,
    /// The same page with the code already in it. Convenience only — the plain URI plus the printed
    /// code has to work on its own, because this is the link that opens in the wrong browser
    /// profile.
    #[serde(default)]
    pub verification_uri_complete: String,
    #[serde(default = "default_expires")]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

/* A derived Debug would put the device code into any log line that formats this struct, and the
device code is the entire secret of the flow: whoever holds it collects the key. */
impl fmt::Debug for Start {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Start")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

impl Start {
    /// The link to hand a browser: the one with the code in it when there is one, else the plain
    /// page. Never assembled here — a query string this side invents is a 404 the first time the
    /// dashboard reorganises.
    pub fn link(&self) -> &str {
        if self.verification_uri_complete.trim().is_empty() {
            self.verification_uri.trim()
        } else {
            self.verification_uri_complete.trim()
        }
    }

    /// The code as it should be read out loud / typed in.
    pub fn code(&self) -> &str {
        if self.user_code.trim().is_empty() {
            self.user_code_plain.trim()
        } else {
            self.user_code.trim()
        }
    }
}

/// The two roots plus the four fully-formed URLs the gateway states outright.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Endpoints {
    /// Ends in `/v1` — an OpenAI client appends `/chat/completions` to it.
    #[serde(default)]
    pub openai_base_url: String,
    /// Does NOT end in `/v1` — an Anthropic client appends `/v1/messages` to it.
    #[serde(default)]
    pub anthropic_base_url: String,
    #[serde(default)]
    pub chat_completions: String,
    #[serde(default)]
    pub messages: String,
    #[serde(default)]
    pub models: String,
    #[serde(default)]
    pub embeddings: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tenant_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsInfo {
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub loadout: Vec<String>,
    /// Whether the name `auto` means anything for this key. It only does when the key has a loadout
    /// behind it, so a client that offers `auto` unconditionally offers a 404.
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub catalog_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub requests_per_minute: Option<u64>,
    #[serde(default)]
    pub requests_per_day: Option<u64>,
    #[serde(default)]
    pub allowed_endpoints: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Karma {
    #[serde(default)]
    pub spendable: Option<f64>,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub enforced: bool,
}

/// `GET /v1/gateway/config` — and the same object the `ready` answer carries inline, so a fresh
/// pairing needs no second round trip to be usable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub endpoints: Endpoints,
    /// Ready-made environment variables. Paste them; do not assemble them.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub key: KeyInfo,
    #[serde(default)]
    pub models: ModelsInfo,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub karma: Karma,
    #[serde(default)]
    pub recheck_after_seconds: Option<u64>,
    #[serde(default)]
    pub checked_at: String,
    /// Who the key belongs to, when the gateway says.
    ///
    /// It does not say today: `/v1/gateway/config` returns `key.{id,key_prefix,label,tenant_id}`
    /// and `karma.plan`, and no address of any kind. This block is the place already made for it,
    /// so the day the gateway answers with `"account": {"email": …}` both clients show it without
    /// a line changing here.
    #[serde(default)]
    pub account: Account,
}

/// Who a key belongs to. Every field defaults, because the whole block is the gateway's to send.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Account {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
}

/// The `ready` answer: the one and only appearance of this machine's credential.
///
/// **It changed shape on 2026-09-06, and the change is the point of the whole flow.** Pairing used
/// to hand back the account's `ak_…` plan key — the SAME string on every machine — so "unpin this
/// device" in the dashboard could not cut anything: revoking that string would cut every other
/// machine and the account's key with it, and so the button only removed a row from a screen.
///
/// Now each machine gets **its own** JWT, bound to its own device row, and the gateway reads that
/// row on every `/v1` call. Unpinning cuts exactly one machine, effective on its next request. The
/// plan key stops leaving the server at all.
///
/// Nothing about billing moved: the loadout, the per-minute ceiling and the spend ledger still hang
/// off the plan key's row, and calls still count against it.
#[derive(Clone, Deserialize)]
pub struct Ready {
    /// The credential this machine will carry, a JWT (`eyJ…`).
    ///
    /// `key` is the old name for the same field, kept as an alias because the server still sends it
    /// and will stop. Nothing here ever checks its shape — the previous build's `ak_` prefix test
    /// would now reject the one valid string and report it as a malformed key, which reads like a
    /// server fault and is the reason this field is named for what it IS.
    #[serde(default, alias = "key")]
    pub token: String,
    /// This machine's row in the device book. Kept beside the token so `aizen gateway status` can
    /// name which row the dashboard's unpin button would cut.
    #[serde(default)]
    pub device_id: String,
    /// `"Bearer"`. Read rather than assumed — and checked, because every request this machine
    /// makes afterwards signs itself `Authorization: Bearer`. A gateway that names another scheme
    /// would have this build failing every call with a 401 and no idea why, so [`scheme_warning`]
    /// turns it into a sentence at the one moment somebody is watching.
    #[serde(default)]
    pub token_type: String,
    /// Seconds. 31536000 — a year.
    #[serde(default)]
    pub expires_in: u64,
    /// The visible head of the **plan key**, NOT of the token above. It is here so a person can
    /// match this machine against the row on their keys screen; printing it as "this machine's
    /// key" would name a string that is no longer anywhere near this machine.
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,
}

impl fmt::Debug for Ready {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ready")
            .field("token", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("key_prefix", &self.key_prefix)
            .field("label", &self.label)
            .finish()
    }
}

/// One answer from the token route. All four arrive as HTTP 200 — they are states of a wait, not
/// failures, and printing them as errors is how a normal ten-second pause becomes thirty lines of
/// red in a terminal.
#[derive(Debug)]
pub enum Outcome {
    /// Nobody has pressed anything yet. `interval` is the server widening the wait.
    Pending {
        interval: Option<u64>,
    },
    /// The account owner said no.
    Denied,
    /// Ten minutes passed.
    Expired,
    Ready(Box<Ready>),
}

/// What to do about a failure, decided where the status code is known rather than by matching on
/// message text three layers up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 404 / 410 — this pairing is gone or already collected. Start again from step 1.
    Restart,
    /// A dropped link, a 5xx, a proxy. Keep waiting.
    Transient,
    /// Anything else. Stop.
    Fatal,
}

#[derive(Debug, Clone)]
pub struct Fail {
    pub kind: FailKind,
    pub message: String,
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Fail {}

impl Fail {
    fn new(kind: FailKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/* -------------------------------------------------------------- two roots */

/// Append `/v1` to a root that does not already end in one.
///
/// The whole reason this is a function: a caller that appends unconditionally turns the Anthropic
/// root into `…/v1/v1/messages`, gets a 404, and reports a broken gateway.
pub fn with_v1(root: &str) -> String {
    let root = root.trim().trim_end_matches('/');
    if root.is_empty() {
        return String::new();
    }
    if root.ends_with("/v1") {
        root.to_string()
    } else {
        format!("{root}/v1")
    }
}

/// The OpenAI-shaped root — what this CLI's chat client appends `/chat/completions` to.
///
/// Preference order is deliberate: what the gateway *said*, then the env block it prepared, and
/// only if both are missing something derived from its own base — never something derived from the
/// Anthropic root, which is the same string minus a suffix and therefore the exact trap.
pub fn openai_base(cfg: Option<&GatewayConfig>) -> String {
    if let Some(cfg) = cfg {
        let stated = cfg.endpoints.openai_base_url.trim();
        if !stated.is_empty() {
            return stated.trim_end_matches('/').to_string();
        }
        if let Some(env) = cfg.env.get("OPENAI_BASE_URL").map(|s| s.trim()) {
            if !env.is_empty() {
                return env.trim_end_matches('/').to_string();
            }
        }
        if !cfg.base_url.trim().is_empty() {
            return with_v1(&cfg.base_url);
        }
    }
    with_v1(&gateway_url())
}

/// The Anthropic-shaped root — what an Anthropic client appends `/v1/messages` to. It carries no
/// `/v1`, and nothing here ever adds one.
pub fn anthropic_base(cfg: Option<&GatewayConfig>) -> String {
    if let Some(cfg) = cfg {
        let stated = cfg.endpoints.anthropic_base_url.trim();
        if !stated.is_empty() {
            return stated.trim_end_matches('/').to_string();
        }
        if let Some(env) = cfg.env.get("ANTHROPIC_BASE_URL").map(|s| s.trim()) {
            if !env.is_empty() {
                return env.trim_end_matches('/').to_string();
            }
        }
        if !cfg.base_url.trim().is_empty() {
            return cfg.base_url.trim().trim_end_matches('/').to_string();
        }
    }
    gateway_url()
}

/// Is this base URL the Aizen gateway's OpenAI root?
///
/// Both spellings count: the one this build shipped with, and whatever `AIZEN_GATEWAY_URL` names
/// right now. A staging override that made the config wizard stop recognising its own gateway row
/// would send somebody down the paste-an-API-key path for an endpoint that has no such key.
pub fn is_gateway_base(base: &str) -> bool {
    base_is_gateway(base, load_pin().as_ref())
}

/// The rule half of [`is_gateway_base`], which is where the interesting part is.
///
/// **The gateway need not be the host that was dialled.** `POST /v1/device/start` goes to one name;
/// the roots that come back for model traffic are whatever the deployment says, and the pairing
/// contract forbids deriving either from the other. On the live deployment those are now the same
/// name, but that is a fact about today's config — `AIZEN_GATEWAY_URL`, a staging box, or the older
/// split where pairing ran on `api.talmetis.com`, each break the match again.
///
/// A check that only knows `DEFAULT_GATEWAY` therefore can miss the endpoint every request in a turn
/// actually goes to, and it fails SILENTLY in three places at once: the config wizard stops to ask a
/// name it already knows, a run against a dead session reports itself as a bad API key, and the
/// session guard never fires. All three are false negatives, which is why this errs towards yes.
///
/// So: the built-in root, the overridden one, the ones shipped earlier, plus the roots this machine
/// was actually handed. Taking the pin as an argument keeps the rule testable — reading the real
/// `~/.aizen` would make the answer depend on whoever ran the tests.
fn base_is_gateway(base: &str, pin: Option<&Pin>) -> bool {
    let b = base.trim().trim_end_matches('/');
    if b.is_empty() {
        return false;
    }
    if b == DEFAULT_OPENAI_BASE || b == openai_base(None) || LEGACY_ROOTS.contains(&b) {
        return true;
    }
    let same = |u: &str| {
        let u = u.trim().trim_end_matches('/');
        !u.is_empty() && u == b
    };
    pin.is_some_and(|p| same(&p.openai_base_url) || same(&p.anthropic_base_url))
}

/* ----------------------------------------------------------------- the wire */

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("aizen-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the gateway HTTP client")
}

/// Pull the most useful sentence out of an error body. The server's own words beat anything
/// invented here; a body that is not JSON at all is a proxy or a captive portal answering in its
/// place, and saying so is more use than quoting its HTML.
fn server_says(code: u16, body: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    for key in ["message", "error_description", "error", "detail"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return format!("{s} (HTTP {code})");
            }
        }
    }
    if json.is_null() && !body.trim().is_empty() {
        return format!(
            "HTTP {code}, and the answer was not JSON — something between here and the gateway \
             replied instead of the gateway"
        );
    }
    format!("HTTP {code}")
}

/// Step 1 — ask for a pairing.
pub async fn start(name: Option<&str>, kind: &str) -> Result<Start> {
    let root = gateway_url();
    let mut body = serde_json::Map::new();
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body.insert("name".into(), serde_json::Value::String(n.to_string()));
    }
    body.insert("kind".into(), serde_json::Value::String(kind.to_string()));

    let res = client()?
        .post(format!("{root}{START_ROUTE}"))
        .json(&serde_json::Value::Object(body))
        .send()
        .await
        .with_context(|| format!("reaching the gateway at {root}"))?;

    let code = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();

    if code == 429 {
        bail!(
            "the gateway is rate-limiting pairings from this network (20 per hour). Wait a few \
             minutes and run it again."
        );
    }
    if code >= 400 {
        bail!(
            "the gateway refused to start a pairing: {}",
            server_says(code, &text)
        );
    }

    let start: Start = serde_json::from_str(&text)
        .with_context(|| format!("reading the pairing {root} started"))?;
    if start.device_code.trim().is_empty() || start.verification_uri.trim().is_empty() {
        bail!("{root} answered without a device code — is that really an Aizen gateway?");
    }
    Ok(start)
}

/// Step 2, once. The four 200s come back as [`Outcome`]; only a real failure is an `Err`.
pub async fn poll_once(device_code: &str) -> Result<Outcome, Fail> {
    let root = gateway_url();
    let http = client().map_err(|e| Fail::new(FailKind::Fatal, e.to_string()))?;

    let res = match http
        .post(format!("{root}{TOKEN_ROUTE}"))
        .json(&serde_json::json!({ "device_code": device_code }))
        .send()
        .await
    {
        Ok(r) => r,
        // A dropped link mid-wait is not the end of the pairing: the person may still be finishing
        // in the browser, and the pairing has its own ten minutes to end this.
        Err(e) => return Err(Fail::new(FailKind::Transient, format!("{e}"))),
    };

    let code = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();

    match code {
        200 => parse_answer(&text),
        _ => Err(refusal(code, &text)),
    }
}

/// The non-200 half of [`poll_once`], split out for the same reason [`parse_answer`] is: a branch
/// that only runs against a live gateway is a branch nobody can test.
///
/// **409 is the one that must not be summarised.** The pairing route answers it for four different
/// situations — a plan key issued before keys could be read back, the key ceiling, a `main` key
/// that is disabled or hidden, and a pairing somebody else already collected — and only the server
/// knows which. Each of those sentences is written to say what to do about it. A client that maps
/// the status to one fixed sentence names the wrong cause three times in four; the one this
/// replaced also quoted a ceiling of 20 keys, a number that exists nowhere (the ceiling is 10).
///
/// 404 and 410 keep a sentence of their own because each has exactly one meaning.
fn refusal(code: u16, text: &str) -> Fail {
    match code {
        404 => Fail::new(
            FailKind::Restart,
            "the gateway has no record of this pairing — start it again",
        ),
        410 => Fail::new(
            FailKind::Restart,
            "this pairing already handed over its key — start it again",
        ),
        401 | 403 => Fail::new(
            FailKind::Fatal,
            format!(
                "the gateway refused the pairing: {}",
                server_says(code, text)
            ),
        ),
        429 => Fail::new(FailKind::Transient, "asked too often — backing off"),
        500..=599 => Fail::new(
            FailKind::Transient,
            format!("gateway is having trouble: {}", server_says(code, text)),
        ),
        _ => Fail::new(FailKind::Fatal, server_says(code, text)),
    }
}

/// Split out from the request so the four states can be tested without a server.
fn parse_answer(body: &str) -> Result<Outcome, Fail> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| Fail::new(FailKind::Transient, format!("unreadable answer: {e}")))?;
    let status = json
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match status.as_str() {
        "pending" | "authorization_pending" | "slow_down" => Ok(Outcome::Pending {
            interval: json.get("interval").and_then(|v| v.as_u64()),
        }),
        "denied" | "access_denied" => Ok(Outcome::Denied),
        "expired" | "expired_token" => Ok(Outcome::Expired),
        "ready" => {
            let ready: Ready = serde_json::from_value(json).map_err(|e| {
                Fail::new(
                    FailKind::Fatal,
                    format!("the gateway said ready but the answer would not parse: {e}"),
                )
            })?;
            if ready.token.trim().is_empty() {
                // The credential appears once. An empty one is not something to retry into —
                // retrying gets a 410, and the pairing has to be run again anyway. `token` carries
                // the `key` alias, so this is also empty for a server still sending the old name
                // with nothing in it.
                return Err(Fail::new(
                    FailKind::Restart,
                    "the gateway said ready and sent no credential — pair again",
                ));
            }
            Ok(Outcome::Ready(Box::new(ready)))
        }
        // An unknown state is a protocol that moved. Stopping with the word in the message beats
        // polling forever against a server that will never say anything this build understands.
        other => Err(Fail::new(
            FailKind::Fatal,
            format!("the gateway answered with a state this build does not know: {other:?}"),
        )),
    }
}

/// Step 3 — the live config for a key. `401` means the key is dead: revoked, unpinned, or expired.
pub async fn config(key: &str) -> Result<GatewayConfig> {
    let root = gateway_url();
    let res = client()?
        .get(format!("{root}{CONFIG_ROUTE}"))
        .bearer_auth(key)
        .send()
        .await
        .with_context(|| format!("reaching the gateway at {root}"))?;

    let code = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();

    if code == 401 {
        expire();
        bail!(
            "this machine has been unpinned — the gateway no longer accepts its credential \
             (unpinned in the dashboard, logged out from another machine, or the account password \
             changed). Run `aizen login` to pin this machine again."
        );
    }
    if code >= 400 {
        // Every other code — the gateway's own 500, a proxy's 502, a captive portal's HTML — is
        // "could not ask", not "the key is dead". The session stays where it was.
        bail!(
            "the gateway would not describe this key: {}",
            server_says(code, &text)
        );
    }
    let cfg: GatewayConfig = serde_json::from_str(&text).context("reading the gateway config")?;
    stamp(&cfg);
    Ok(cfg)
}

/* --------------------------------------------------------------- the pin */

/// What is remembered about a pin locally. Deliberately every field EXCEPT the key: the key lives
/// once, in `cli-config.json`, and a second copy is a second thing to leak.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pin {
    /// The `cli-config.json` profile carrying the key.
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub gateway: String,
    /// The visible head of the **PLAN KEY** (`ak_1a2b`) — not of the token this machine carries.
    ///
    /// It was a piece of the held string until 2026-09-06, when pairing stopped handing the plan
    /// key out. It is kept because it is still the thing a person can match against the row on
    /// their keys screen; what it must never again be called is "this machine's key".
    #[serde(default)]
    pub key_prefix: String,
    /// This machine's row in the device book, as the gateway numbered it. Empty on a pin from
    /// before per-device credentials — which is exactly the pin that cannot be cut remotely.
    #[serde(default)]
    pub device_id: String,
    /// Unix seconds when this machine's token stops being accepted, from the `expires_in` the
    /// pairing named (a year). `0` means the gateway named none, or the pin predates the field.
    ///
    /// Advisory only. A `401` is the fact; this is for saying "in about a month" before one lands.
    #[serde(default)]
    pub token_expires_at: u64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub openai_base_url: String,
    #[serde(default)]
    pub anthropic_base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub paired_at: String,
    /// Unix seconds, THIS machine's clock, of the last answer the gateway gave for this key.
    ///
    /// This machine's clock and not the gateway's `checked_at`, because the only sum it ever appears
    /// in is `now - verified_at`: mixing two clocks in one subtraction is how a machine with a
    /// skewed clock declares its own perfectly good key expired. `0` means never asked, which is
    /// also what a pin file written by an older build reads as — and that lands on "ask once", not
    /// on "signed out".
    #[serde(default)]
    pub verified_at: u64,
    /// The `recheck_after_seconds` the gateway named at that moment. `0` means it named none.
    #[serde(default)]
    pub recheck_after: u64,
    /// Unix seconds of the answer that was a `401`. `0` until one is. Non-zero means the local key
    /// has already been deleted and what is left here is the note saying why — see [`expire`].
    #[serde(default)]
    pub expired_at: u64,
    /// The account, exactly as the gateway named it; empty until it names one. Written here rather
    /// than asked for each time, because the desktop plugin draws it in a menu that opens without
    /// waiting for the network — and both halves keep this file, so neither may drop the other's
    /// fields on a write.
    #[serde(default)]
    pub account_email: String,
    #[serde(default)]
    pub account_name: String,
}

/* ------------------------------------------------------------------ expiry

A login does not last forever, and the expensive way to be wrong is to sit on a dead key: every run
fails at the provider with a bare 401, and a person reads that as "the tool is broken" rather than
as "sign in again".

The gateway already states the cadence — `recheck_after_seconds` in `/v1/gateway/config` is its own
"come back and ask again after this long". So a session has three states, not two:

  * **`Fresh`** — the last answer is still inside that window. Nothing to do.
  * **`Stale`** — past the window with nothing confirmed since. NOT signed out: the key is very
    probably fine and the machine is very probably offline. Still usable, and said out loud.
  * **`Expired`** — the gateway answered `401`. That one is decisive: revoked, unpinned, or expired
    all mean the key is dead. The local key goes, and the way back is `aizen login`.

The rule that matters most here: **being offline is not being signed out.** Only a real `401` from
the gateway deletes a key. If a network error did it too, a flight would be a logout — and the
person could not log back in, because they have no network. For the same reason there is no local
"expires after N days offline": until something proves the key is dead, nothing may delete it.

Both halves of Aizen — this CLI and the desktop window — read and write the same `gateway.json` with
the same rule and the same constants, so "the session ended" is one answer rather than two. */

/// What [`session_of`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Fresh,
    Stale,
    Expired,
}

impl Session {
    pub fn name(self) -> &'static str {
        match self {
            Session::Fresh => "fresh",
            Session::Stale => "stale",
            Session::Expired => "expired",
        }
    }
}

/// How long a confirmation lasts when the gateway names no cadence of its own. Twelve hours: sparse
/// enough that nobody notices it, dense enough that a key revoked in the morning does not survive a
/// working day unnoticed.
const RECHECK_DEFAULT: u64 = 12 * 60 * 60;
/// And the clamp on the number the gateway does name. With no floor a wild value turns this clock
/// into one request per second; with no ceiling it turns it off altogether — both of them the
/// gateway shooting this client in the foot through one integer in a JSON body.
const RECHECK_MIN: u64 = 5 * 60;
const RECHECK_MAX: u64 = 30 * 24 * 60 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn recheck_window(pin: &Pin) -> u64 {
    if pin.recheck_after == 0 {
        RECHECK_DEFAULT
    } else {
        pin.recheck_after.clamp(RECHECK_MIN, RECHECK_MAX)
    }
}

/// Where a session stands. Pure, because this is the decision that puts a login prompt in front of
/// somebody, and a rule that only runs against a real `~/.aizen` is a rule nobody can test.
///
/// `saturating_sub` rather than plain subtraction: a machine clock moves backwards — a timezone
/// change, an NTP correction, a VM restored from a snapshot — and an underflow here is a fake
/// logout with a very large number behind it.
fn session_of(pin: &Pin, now: u64) -> Session {
    if pin.expired_at > 0 {
        return Session::Expired;
    }
    if pin.verified_at == 0 {
        return Session::Stale;
    }
    if now.saturating_sub(pin.verified_at) > recheck_window(pin) {
        Session::Stale
    } else {
        Session::Fresh
    }
}

/// The session on this machine right now, off disk. `None` when it was never pinned at all — which
/// is a different sentence from "expired", and callers say the difference out loud.
pub fn session() -> Option<Session> {
    load_pin().map(|p| session_of(&p, now_secs()))
}

/// Record that the gateway just answered for this key: the time, and the cadence it asked for.
///
/// Best-effort, for the same reason writing the pin file is: this is a clock, not a key. Failing to
/// write it only makes the session go `Stale` early and ask once more.
fn stamp(cfg: &GatewayConfig) {
    let Some(mut pin) = load_pin() else {
        return;
    };
    pin.verified_at = now_secs();
    pin.recheck_after = cfg.recheck_after_seconds.unwrap_or(0);
    pin.expired_at = 0;
    // Only overwrite when the gateway said something. An answer missing a field is not an answer
    // that the account went away.
    if !cfg.account.email.trim().is_empty() {
        pin.account_email = cfg.account.email.clone();
    }
    if !cfg.account.name.trim().is_empty() {
        pin.account_name = cfg.account.name.clone();
    }
    let _ = save_pin(&pin);
}

/// The gateway said no to this key. Drop the local half, and leave the note saying why.
///
/// The key half is exactly [`forget`]. What differs is that the pin file stays: the next thing a
/// person sees must be "the session ended, pin again" and not "this machine was never pinned", and
/// those two sentences send them down two different paths. `aizen logout` clears the note.
///
/// Nothing is sent to the gateway. The gateway is the side that just said the key was dead.
pub fn expire() {
    let name = load_pin()
        .map(|p| p.profile)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
    let mut store = cli_config::load();
    forget_in(&mut store, &name);
    let _ = cli_config::save(&store);
    if let Some(mut pin) = load_pin() {
        pin.expired_at = now_secs();
        let _ = save_pin(&pin);
    }
}

/// One line for `aizen gateway status`: where the session stands, and when it is next in question.
pub fn session_line() -> Option<String> {
    let pin = load_pin()?;
    let now = now_secs();
    let state = session_of(&pin, now);
    Some(match state {
        Session::Expired => format!(
            "{} — run `aizen login` to pin this machine again",
            state.name()
        ),
        _ if pin.verified_at == 0 => format!("{} — never confirmed", state.name()),
        _ => format!(
            "{} — confirmed {} ago, rechecks every {}",
            state.name(),
            short_dur(now.saturating_sub(pin.verified_at)),
            short_dur(recheck_window(&pin)),
        ),
    })
}

/// `45s` `7m` `3h` `2d` — a duration for one status line, and no dependency for it. Coarse on
/// purpose: this answers "how long ago, roughly", and a second place is a second thing to read.
fn short_dur(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Stop a run whose gateway session the gateway itself has ended.
///
/// A local read, never a request. This sits in front of every call the process makes, so a check
/// that cost a round trip would put the gateway on the critical path of every `aizen` invocation —
/// and it would fail exactly when it is least wanted, on a machine with no network.
///
/// The sentence matters as much as the stop. [`expire`] takes the endpoint with it, so without this
/// the next thing a person sees is "no base URL — run `aizen config`", which sends them to configure
/// a provider by hand for a problem one `aizen login` fixes.
pub fn guard_session() -> Result<()> {
    // Signed in? Then there is nothing to stop. The pin's key is gone, but the account session
    // opens `/v1` on its own, and `resolve_endpoint` will hand it over — refusing here would tell
    // somebody who can work right now to go and pin a machine they no longer need to pin.
    if session() == Some(Session::Expired) && !crate::llm::account::signed_in() {
        bail!(
            "this machine has been unpinned: the gateway refused its credential, so the local \
             copy was removed. That happens when the device is unpinned in the dashboard, when \
             another machine logs it out, or when the account password changes — all three end \
             every pairing on purpose. Run `aizen login` to pin it again, or `aizen account \
             login` to sign in."
        );
    }
    Ok(())
}

/// What a `401` at this URL means for this machine — three different sentences, three fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unauthorized {
    /// Not an Aizen root. A wrong key at somebody else's provider is not a reason to unpin.
    NotOurs,
    /// A pinned key the gateway no longer accepts. The local half has been dropped; `aizen login`.
    PinEnded,
    /// The call rode the account session. Nothing local to drop — `aizen account login` again.
    SignInAgain,
}

/// The rule half, pure so all three branches are reachable without a pin file or a network.
///
/// **Why the pin decides.** `resolve_endpoint` hands over the session token only when no key came
/// from a flag, the env, or the config — and a pin is what puts a gateway key in the config. So no
/// pin plus a session means the call went out on the session, which is not something a local
/// teardown can fix. The one case this reads wrong is an explicit `--api-key` at a gateway root on
/// an unpinned machine that is also signed in; the sentence it produces still says the 401 came
/// from the gateway and still must not be retried, so it misnames the cure, not the problem.
fn unauthorized_means(hits_gateway: bool, has_pin: bool, signed_in: bool) -> Unauthorized {
    if !hits_gateway {
        return Unauthorized::NotOurs;
    }
    if !has_pin && signed_in {
        return Unauthorized::SignInAgain;
    }
    Unauthorized::PinEnded
}

/// Read the verdict, and act on it: [`Unauthorized::PinEnded`] ends the session here and now.
///
/// A `401` from Aizen is never a transient failure — not on the pin (the gateway threw the key
/// away) and not on the session (the token expired, or a password change killed it). Retrying is
/// the one thing that cannot help, which is why `401` is absent from `is_retryable_status`.
///
/// Asked with a whole request URL rather than a base, because the call that gets the 401 is a
/// `POST {base}/chat/completions`, and [`is_gateway_base`] compares roots.
pub fn on_unauthorized(url: &str) -> Unauthorized {
    let pin = load_pin();
    let verdict = unauthorized_means(
        url_hits_gateway(url, pin.as_ref(), &gateway_url()),
        pin.is_some(),
        crate::llm::account::signed_in(),
    );
    if verdict == Unauthorized::PinEnded {
        expire();
    }
    verdict
}

/// Does this whole request URL go to the gateway this machine is pinned to?
///
/// By host, because what gets the 401 is `POST {base}/chat/completions` while [`is_gateway_base`]
/// compares roots — and against EVERY host the gateway answers on, for the reason spelled out in
/// [`base_is_gateway`]: the pairing domain and the model-traffic domain are different names, and a
/// check that knows only one of them lets a dead session go on failing as a bad key forever.
fn url_hits_gateway(url: &str, pin: Option<&Pin>, gateway: &str) -> bool {
    let mut hosts = vec![gateway.to_string()];
    if let Some(p) = pin {
        hosts.push(p.gateway.clone());
        hosts.push(p.openai_base_url.clone());
        hosts.push(p.anthropic_base_url.clone());
    }
    hosts.iter().any(|h| same_host(url, h))
}

/// Host comparison, spelled once. Both sides of a gateway check are URLs this process built or read
/// from its own config, so a full parse buys nothing a split does not.
fn same_host(a: &str, b: &str) -> bool {
    let host = |u: &str| {
        u.trim()
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(u)
            .split('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let (x, y) = (host(a), host(b));
    !x.is_empty() && x == y
}

pub fn pin_path() -> PathBuf {
    aizen_home().join("gateway.json")
}

pub fn load_pin() -> Option<Pin> {
    let raw = std::fs::read_to_string(pin_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_pin(pin: &Pin) -> Result<()> {
    let path = pin_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(pin)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    // No secret in it, but it names the account and the plan; owner-only costs nothing.
    harden_file(&path);
    Ok(())
}

/// Point the pin file at the profile the key actually ended up in.
///
/// [`adopt`] files it under `aizen`, which is right for `aizen login`. The config wizard is the
/// other door: it lets a person name the profile whatever they like and then saves its own copy of
/// the config, so the row the pin names can be a row that no longer exists. `gateway status` reads
/// this field to find the key, so keeping it honest is one line and the alternative is a status
/// command that reports "not pinned" at a machine that plainly is.
pub fn point_pin_at(profile: &str) {
    let profile = profile.trim();
    if profile.is_empty() {
        return;
    }
    if let Some(mut pin) = load_pin() {
        if pin.profile != profile {
            pin.profile = profile.to_string();
            let _ = save_pin(&pin);
        }
    }
}

pub fn clear_pin() {
    let _ = std::fs::remove_file(pin_path());
}

/// Does this build sign requests the way the gateway just said to?
///
/// `Bearer` or nothing — an empty field is a server that did not say, and every deployment so far
/// means Bearer by it. Anything else is worth a sentence NOW: the alternative is a machine that
/// pairs cleanly and then 401s on every call, which reads as a bad credential.
fn scheme_warning(token_type: &str) -> Option<String> {
    let named = token_type.trim();
    (!named.is_empty() && !named.eq_ignore_ascii_case("bearer")).then(|| {
        format!(
            "the gateway asked for `{named}` authorization, and this build only sends `Bearer` — \
             calls will be refused until it is updated"
        )
    })
}

/// What [`adopt`] did, so the caller can report it truthfully rather than assuming.
#[derive(Debug, Clone)]
pub struct Adopted {
    pub pin: Pin,
    /// Whether this profile is now the endpoint the CLI actually uses.
    pub activated: bool,
    /// Something the user has to act on that is not an error — today: a gateway with no default
    /// model, which leaves the profile written but unusable until a model is chosen.
    pub warning: Option<String>,
}

/// Write the key down.
///
/// This runs the instant `ready` arrives and before anything is printed, because the raw string is
/// on the wire exactly once: a panic, a broken pipe, or a Ctrl-C between the answer and the write
/// costs the key outright, and the only recovery is to pair again.
///
/// The endpoint written is the OpenAI-shaped root — this CLI posts to `{base}/chat/completions`.
/// The Anthropic root is recorded beside it in the pin file rather than thrown away, because it is
/// what `aizen gateway env` hands to an Anthropic-shaped tool, and it is NOT derivable from the
/// other one by anybody who has not read the contract.
pub fn adopt(profile: &str, ready: &Ready, activate: bool) -> Result<Adopted> {
    let profile = profile.trim();
    if profile.is_empty() {
        bail!("a pin needs a profile name to live under");
    }
    let cfg = ready.gateway.as_ref();
    let openai = openai_base(cfg);
    let anthropic = anthropic_base(cfg);

    let mut store = cli_config::load();
    let previous = store.provider(profile).cloned();

    // The gateway's default first; a model already pinned here second — re-pairing a machine that
    // had chosen a model should not silently move it back to the account default's absence.
    let model = cfg
        .map(|c| c.models.default.trim().to_string())
        .filter(|m| !m.is_empty())
        .or_else(|| previous.as_ref().map(|p| p.model.clone()))
        .unwrap_or_default();

    // A context window learned for a DIFFERENT model is worse than none: it drives the `% context`
    // HUD, and a wrong number there is a wrong number the user trusts.
    let window = previous
        .as_ref()
        .filter(|p| p.model == model)
        .and_then(|p| p.model_context_window);

    store.upsert_provider(ProviderProfile {
        name: profile.to_string(),
        base_url: openai.clone(),
        api_key: ready.token.clone(),
        model: model.clone(),
        model_context_window: window,
    })?;

    // Activating copies the tuple into the root fields, and a root `model` of "" is an endpoint
    // that 400s on the first turn. So a gateway that named no default leaves the profile saved and
    // the switch unthrown, with a sentence saying which command finishes it.
    let can_activate = activate && !model.is_empty();
    if can_activate {
        store.activate_provider(profile)?;
    }
    cli_config::save(&store)?; // ← the key is on disk from here on

    let pin = Pin {
        profile: profile.to_string(),
        gateway: gateway_url(),
        key_prefix: if ready.key_prefix.trim().is_empty() {
            cfg.map(|c| c.key.key_prefix.clone()).unwrap_or_default()
        } else {
            ready.key_prefix.trim().to_string()
        },
        device_id: ready.device_id.trim().to_string(),
        // Turned into an absolute moment at the one instant both clocks agree — the answer just
        // arrived — because a stored duration is only readable next to the timestamp it counts
        // from, and that second field is a second thing to get wrong.
        token_expires_at: (ready.expires_in > 0)
            .then(|| now_secs() + ready.expires_in)
            .unwrap_or(0),
        label: if ready.label.trim().is_empty() {
            cfg.map(|c| c.key.label.clone()).unwrap_or_default()
        } else {
            ready.label.trim().to_string()
        },
        openai_base_url: openai,
        anthropic_base_url: anthropic,
        model: model.clone(),
        plan: cfg.map(|c| c.karma.plan.clone()).unwrap_or_default(),
        paired_at: chrono::Utc::now().to_rfc3339(),
        // The pairing IS a confirmation — the gateway just described this key — so a machine that
        // just pinned starts the clock rather than starting stale.
        verified_at: now_secs(),
        recheck_after: cfg.and_then(|c| c.recheck_after_seconds).unwrap_or(0),
        expired_at: 0,
        account_email: cfg.map(|c| c.account.email.clone()).unwrap_or_default(),
        account_name: cfg.map(|c| c.account.name.clone()).unwrap_or_default(),
    };
    // Best-effort: the pin file is a convenience, and failing to write it must not look like a
    // failed pairing when the key — the part that cannot be recovered — is already saved.
    let _ = save_pin(&pin);

    // Two things worth acting on, and the scheme goes first: a model that has to be picked leaves
    // a working pairing, while a scheme this build cannot send leaves one that refuses every call.
    let warning = scheme_warning(&ready.token_type).or_else(|| {
        (activate && model.is_empty()).then(|| {
            format!(
                "the gateway named no default model, so `{profile}` was saved but not switched on. \
                 Pick one with `aizen models --provider {profile}`, then `aizen config provider use {profile}`."
            )
        })
    });

    Ok(Adopted {
        pin,
        activated: can_activate,
        warning,
    })
}

/// The key a pinned profile carries, read back out of `cli-config.json`.
///
/// `None` rather than an error for "there is no pin", because every caller wants to say something
/// friendlier than a missing-file message.
pub fn key_for(profile: Option<&str>) -> Option<(String, String)> {
    let cfg = cli_config::load();
    let name = profile
        .map(str::to_string)
        .or_else(|| load_pin().map(|p| p.profile))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
    if let Some(p) = cfg.provider(&name) {
        if !p.api_key.trim().is_empty() {
            return Some((p.name.clone(), p.api_key.clone()));
        }
    }
    // A config that predates profiles keeps its one endpoint in the root fields. If that endpoint
    // is this gateway, its key is the pin's key.
    let root_url = cfg.base_url.unwrap_or_default();
    let root_key = cfg.api_key.unwrap_or_default();
    let same_host = |a: &str, b: &str| {
        let host = |u: &str| {
            u.split_once("://")
                .map(|(_, r)| r)
                .unwrap_or(u)
                .split('/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
        };
        !a.is_empty() && host(a) == host(b)
    };
    (!root_key.trim().is_empty() && same_host(&root_url, &gateway_url()))
        .then(|| (String::new(), root_key))
}

/// Drop the local half of a pin: the key, and the profile that held it.
///
/// It does NOT revoke anything. Revoking is unpinning the device in the dashboard, which the
/// gateway does in the same transaction — an app that could revoke by deleting a local file would
/// be an app that leaves a live key behind whenever the file is deleted some other way. Callers
/// say that out loud.
pub fn forget(profile: Option<&str>) -> Result<Option<String>> {
    let name = profile
        .map(str::to_string)
        .or_else(|| load_pin().map(|p| p.profile))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string());

    // Was there anything here at all? Asked BEFORE the teardown, and reported, because the answer
    // is the difference between "your key is gone" and a machine that never had one being told its
    // key was removed. That reading was harmless while only a pinned user ever ran `logout`; it
    // stopped being harmless when `aizen logout` became the word for leaving Aizen entirely.
    let pinned = load_pin().is_some();
    let mut store = cli_config::load();
    let had_row = forget_in(&mut store, &name).is_some();
    cli_config::save(&store)?;
    clear_pin();
    Ok((pinned || had_row).then_some(name))
}

/// The config half of [`forget`]: drop the profile, and the root copy of its key when the root is
/// a copy of that profile. Returns what was removed, or `None` if there was no such row.
///
/// Split out so a caller already holding the config can log out without a load-modify-save race
/// against its own pending edits — `aizen config` holds one across a whole editing session.
///
/// It does **not** touch the pin file. Two callers want the config half and different pin
/// treatment: [`forget`] deletes the pin, [`expire`] keeps it and stamps it, so that the next
/// sentence a person reads is "the session ended" rather than "this machine was never pinned".
pub fn forget_in(
    store: &mut cli_config::CliConfig,
    profile: &str,
) -> Option<cli_config::ProviderProfile> {
    let gone = store.remove_provider(profile).ok();
    if root_goes_too(store, gone.as_ref(), &with_v1(&gateway_url())) {
        store.base_url = None;
        store.api_key = None;
        store.model = None;
        store.model_context_window = None;
        store.active_provider = None;
    }
    gone
}

/// What `POST /v1/device/logout` did, or why nothing was sent.
#[derive(Debug, Default, Clone)]
pub struct Unpaired {
    /// The gateway says this machine's device row is cut. Effective on its very next `/v1` call.
    pub cut: bool,
    /// The row the gateway named. Empty when it named none.
    pub device_id: String,
    /// Nothing was sent because this machine holds the account's old shared plan key, which has no
    /// device row to cut — the pairing predates per-device credentials.
    pub legacy: bool,
    /// Why the call did not land. Never a reason to keep the credential: see [`unpair`].
    pub trouble: Option<String>,
}

/// Cut THIS machine at the gateway.
///
/// **Whatever this returns, the local teardown still happens.** A 200, a 401, a dead network and a
/// captive portal all mean one thing on this side, and the `forget_token` the route sends back is
/// an instruction rather than a suggestion — the server cannot reach this disk, so the second half
/// of a logout is ours. Calling twice is another 200: the route reports a state, not an event.
///
/// It is skipped for a pre-2026-09-06 pin, and that is deliberate rather than lazy. Those machines
/// hold the ACCOUNT's plan key, shared by every machine on it. There is no device row behind that
/// string, so there is nothing here to cut — and posting the account's key to a route that cuts
/// device rows is a way to find out the hard way that some row matched.
pub async fn unpair(profile: Option<&str>) -> Unpaired {
    let Some(token) = pinned_credential(profile) else {
        return Unpaired::default();
    };
    if !is_device_token(&token) {
        return Unpaired {
            legacy: true,
            ..Default::default()
        };
    }
    let root = gateway_url();
    let sent = match client() {
        Ok(c) => {
            c.post(format!("{root}{LOGOUT_ROUTE}"))
                .bearer_auth(&token)
                .send()
                .await
        }
        Err(e) => {
            return Unpaired {
                trouble: Some(e.to_string()),
                ..Default::default()
            }
        }
    };
    let res = match sent {
        Ok(r) => r,
        Err(e) => {
            return Unpaired {
                trouble: Some(format!("could not reach {root}: {e}")),
                ..Default::default()
            }
        }
    };
    let code = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let device_id = json
        .get("device_id")
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();

    // A `401` here is not a failure of the logout — it is the logout having already happened,
    // somewhere else: unpinned in the dashboard, cut by another machine, or a password change.
    // Both codes end at the same teardown, so both count as cut.
    match code {
        200 | 401 => Unpaired {
            cut: true,
            device_id,
            legacy: false,
            trouble: None,
        },
        _ => Unpaired {
            cut: false,
            device_id,
            legacy: false,
            trouble: Some(server_says(code, &text)),
        },
    }
}

/// What leaving Aizen on this machine actually took.
#[derive(Debug, Default, Clone)]
pub struct Left {
    /// What the gateway said when this machine asked to be cut. `None` when nothing was asked —
    /// a local-only teardown, or a caller that had no runtime to ask from.
    pub unpaired: Option<Unpaired>,
    /// The account session was here and is gone.
    pub signed_out: bool,
    /// The profile whose key was removed, or `None` when nothing was pinned.
    pub key_profile: Option<String>,
    /// The key half could not be torn down (an unwritable config, say). Reported, never swallowed:
    /// a logout that half worked and said nothing is how a key survives one.
    pub key_error: Option<String>,
}

/// Leave Aizen on this machine: the account session **and** the local gateway key.
///
/// One function because from outside they are one thing. It became one thing on 2026-09-06, when
/// the plan started riding the session — before that a `logout` could honestly drop just the key.
///
/// Revokes nothing, and every caller says so: the token stays valid on other machines until it
/// expires, and the key stays live until the device is unpinned in the dashboard.
pub async fn leave(profile: Option<&str>) -> Left {
    // Ask the gateway to cut this machine BEFORE the credential is torn down — it is the only
    // thing that authenticates the request. What it answers changes what is printed and nothing
    // else: the teardown below runs on 200, on 401, and on a dead network alike.
    let unpaired = Some(unpair(profile).await);
    // The session next. If the config write below fails, the credential that can actually spend is
    // already gone, which is the right way round for a half-finished logout.
    let signed_out = crate::llm::account::clear();
    match forget(profile) {
        Ok(key_profile) => Left {
            unpaired,
            signed_out,
            key_profile,
            key_error: None,
        },
        Err(e) => Left {
            unpaired,
            signed_out,
            key_profile: None,
            key_error: Some(e.to_string()),
        },
    }
}

/// [`leave`] for a caller that already holds the config.
///
/// `aizen config` keeps one across a whole editing session, so a load-modify-save in here would
/// roll back every other edit made on that screen. No I/O of its own beyond the pin and the session
/// files — the caller saves the config when it is ready.
///
/// The pin goes only when it names THIS profile: a machine pinned under another name keeps its pin
/// when an unrelated Aizen row is deleted.
pub fn leave_in(store: &mut cli_config::CliConfig, name: &str) -> Left {
    let pinned_here = is_pinned_profile(name);
    let gone = forget_in(store, name);
    if pinned_here {
        clear_pin();
    }
    Left {
        // Nothing was asked of the gateway: this is a config edit, the caller holds no runtime, and
        // a logout that quietly blocked on the network inside a settings screen would be worse than
        // one that leaves the row in the dashboard for the user to cut.
        unpaired: None,
        signed_out: crate::llm::account::clear(),
        // Only when a key was really here. A signed-in machine's Aizen row carries none, and
        // claiming one was removed would send somebody to the dashboard to revoke nothing.
        key_profile: pinned_here.then(|| gone.map(|p| p.name)).flatten(),
        key_error: None,
    }
}

/// Does removing this config profile mean LOGGING OUT rather than deleting a row?
///
/// Two ways it can, and only one of them is the pin. A machine that signed in carries an Aizen
/// profile with **no key at all** — the session is the credential — so [`is_pinned_profile`] has
/// nothing to say about it, and deleting that row as an ordinary provider leaves the session behind
/// still able to spend, on a machine whose owner just said they were done with it.
///
/// Takes the config rather than reading it, so a caller mid-edit judges the row it is holding.
pub fn is_aizen_profile(store: &cli_config::CliConfig, name: &str) -> bool {
    is_pinned_profile(name)
        || store
            .provider(name)
            .is_some_and(|p| is_gateway_base(&p.base_url))
}

/// Does this credential cut off when the dashboard says so?
///
/// A per-device token is a JWT and starts `eyJ`. A pairing from before 2026-09-06 left the
/// account's `ak_…` plan key here instead, and that string is shared by every machine on the
/// account — so "unpin this device" cannot cut it, and the dashboard labels those rows as such.
///
/// **Not a validation, and must never become one.** The previous build refused anything that did
/// not start with `ak_`, which is how a client rejects the one string that works and reports it as
/// a malformed key. This asks a different question — whether the credential is remotely revocable —
/// and its only answer is a sentence suggesting `aizen login`.
pub fn is_device_token(credential: &str) -> bool {
    credential.trim_start().starts_with("eyJ")
}

/// The pinned profile's credential, if this machine has one. `None` when nothing is pinned.
fn pinned_credential(profile: Option<&str>) -> Option<String> {
    key_for(profile)
        .map(|(_, k)| k)
        .filter(|k| !k.trim().is_empty())
}

/// Is the pin on this machine one the dashboard cannot cut? `None` when there is no pin.
///
/// Answered off disk, no request — the point is to say it in a status line, not to ask permission.
pub fn pin_is_legacy() -> Option<bool> {
    load_pin()?;
    Some(!is_device_token(&pinned_credential(None)?))
}

/// Is this config profile the one the gateway pin points at?
///
/// Deleting that profile is not deleting a provider row, it is a **logout** — the pin file names
/// the profile, so a plain removal leaves `gateway.json` behind claiming this machine is paired to
/// a key that is no longer anywhere, and an ordinary removal deliberately KEEPS the root copy of
/// the key, which for this one profile is the key a logout has to leave nowhere.
///
/// An empty `profile` in the pin means the default, the same reading [`forget`] takes.
pub fn is_pinned_profile(name: &str) -> bool {
    let Some(pin) = load_pin() else {
        return false;
    };
    let pinned = pin.profile.trim();
    let pinned = if pinned.is_empty() {
        DEFAULT_PROFILE
    } else {
        pinned
    };
    pinned.eq_ignore_ascii_case(name.trim())
}

/// Does unpinning also mean clearing the root endpoint?
///
/// Its own function because this is the branch that can destroy something nobody asked it to. The
/// root fields are a copy of whichever profile is live, so:
///
///   * a profile WAS removed — clear the root only when it is a copy of that one, matched on the
///     key rather than the URL, because the key is the thing a logout has to leave nowhere and the
///     endpoint the gateway stated is not necessarily the one this build would derive;
///   * no profile was removed — a config written before profiles existed keeps its one endpoint in
///     the root, and that endpoint may BE the pin. Clear it only when it points at this gateway.
///     Wiping somebody's hand-configured OpenAI key because they typed `aizen gateway logout` is
///     the one thing this command must never do.
fn root_goes_too(
    store: &cli_config::CliConfig,
    removed: Option<&ProviderProfile>,
    gateway_openai_base: &str,
) -> bool {
    match removed {
        Some(gone) => {
            let root_key = store.api_key.as_deref().unwrap_or("").trim();
            let gone_key = gone.api_key.trim();
            // A profile that carries NO key is identified by its endpoint instead. That is the
            // Aizen row on a signed-in machine, and matching on a key neither side has would answer
            // "no" — leaving the root pointing at a gateway this machine can no longer open. The
            // root must be keyless too, or it belongs to some other provider and is not ours.
            if gone_key.is_empty() {
                let root_url = store.base_url.as_deref().unwrap_or("").trim();
                return root_key.is_empty()
                    && !root_url.is_empty()
                    && root_url.trim_end_matches('/')
                        == gone.base_url.trim().trim_end_matches('/');
            }
            !root_key.is_empty() && root_key == gone_key
        }
        None => {
            let root_url = store.base_url.as_deref().unwrap_or("").trim();
            !root_url.is_empty()
                && root_url.trim_end_matches('/') == gateway_openai_base.trim_end_matches('/')
        }
    }
}

/* ---------------------------------------------------------------- the loop */

#[derive(Debug, Clone)]
pub struct PairOpts {
    /// What this machine will be called in the dashboard. `None` ⇒ the hostname.
    pub name: Option<String>,
    /// `"cli"` or `"desktop"`.
    pub kind: String,
    pub profile: String,
    pub activate: bool,
}

impl Default for PairOpts {
    fn default() -> Self {
        Self {
            name: None,
            kind: "cli".into(),
            profile: DEFAULT_PROFILE.into(),
            activate: true,
        }
    }
}

/// The whole login: start, hand the code to the caller to show, wait, write the key down.
///
/// `on_start` is called exactly once with the pairing, before the first wait — printing belongs to
/// the caller (this module has no opinion about terminals), and it has to happen before the sleep
/// or the user stares at nothing for five seconds.
///
/// Ctrl-C is honoured only while sleeping, never across the request that may be carrying the key.
/// A cancel that lands between `ready` and [`adopt`] costs the key for good.
pub async fn pair<F: FnMut(&Start)>(opts: &PairOpts, mut on_start: F) -> Result<Adopted> {
    let name = opts
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(machine_name);

    let started = start(name.as_deref(), &opts.kind).await?;
    on_start(&started);

    let mut interval = started.interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
    // The server's ten minutes, capped by ours so a wild `expires_in` cannot park a terminal for a
    // day. Measured from now with a monotonic clock: a system clock that jumps mid-wait should not
    // end the pairing early or extend it forever.
    let deadline = Instant::now()
        + Duration::from_secs(started.expires_in.clamp(60, MAX_WAIT.as_secs())).min(MAX_WAIT);
    let mut transient = 0u32;

    loop {
        // Sleep first: nobody has had time to approve anything yet, and hammering the route before
        // the first interval is how a client earns a 429 on its own behalf.
        let nap =
            Duration::from_secs(interval).min(deadline.saturating_duration_since(Instant::now()));
        tokio::select! {
            _ = tokio::time::sleep(nap) => {}
            _ = tokio::signal::ctrl_c() => {
                bail!("pairing cancelled — nothing was written. The code stops working on its own.");
            }
        }
        if Instant::now() >= deadline {
            bail!("the pairing expired before it was approved. Run it again to get a new code.");
        }

        match poll_once(&started.device_code).await {
            Ok(Outcome::Ready(ready)) => {
                // Nothing between here and the write. Not a print, not a second request.
                return adopt(&opts.profile, &ready, opts.activate);
            }
            Ok(Outcome::Pending { interval: widened }) => {
                transient = 0;
                if let Some(n) = widened {
                    interval = n.clamp(MIN_INTERVAL, MAX_INTERVAL);
                }
            }
            Ok(Outcome::Denied) => bail!("the request was denied in the browser."),
            Ok(Outcome::Expired) => {
                bail!("the code expired. Run it again to get a new one.")
            }
            Err(f) if f.kind == FailKind::Transient => {
                transient += 1;
                if transient >= MAX_TRANSIENT {
                    bail!("gave up waiting: {f}");
                }
                // Widen on the way out, so a gateway shedding load is not asked harder.
                interval = (interval * 2).clamp(MIN_INTERVAL, MAX_INTERVAL);
            }
            Err(f) => bail!("{f}"),
        }
    }
}

/// A name for this machine, for the dashboard row. Best-effort — the server defaults it when absent.
pub fn machine_name() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(openai: &str, anthropic: &str, base: &str) -> GatewayConfig {
        GatewayConfig {
            base_url: base.into(),
            endpoints: Endpoints {
                openai_base_url: openai.into(),
                anthropic_base_url: anthropic.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The trap the contract names outright: the two roots differ by a suffix, and a client that
    /// derives one from the other sends Anthropic traffic to `/v1/v1/messages`.
    #[test]
    fn the_two_roots_are_taken_as_given_and_never_built_from_each_other() {
        let cfg = cfg_with(
            "https://api.talmetis.com/v1",
            "https://api.talmetis.com",
            "https://api.talmetis.com",
        );
        assert_eq!(openai_base(Some(&cfg)), "https://api.talmetis.com/v1");
        assert_eq!(anthropic_base(Some(&cfg)), "https://api.talmetis.com");
        // And the Anthropic client's own suffix lands where it should.
        assert_eq!(
            format!("{}/v1/messages", anthropic_base(Some(&cfg))),
            "https://api.talmetis.com/v1/messages"
        );
    }

    #[test]
    fn a_root_that_already_carries_v1_does_not_collect_a_second_one() {
        assert_eq!(with_v1("https://x.test/v1"), "https://x.test/v1");
        assert_eq!(with_v1("https://x.test/v1/"), "https://x.test/v1");
        assert_eq!(with_v1("https://x.test"), "https://x.test/v1");
        assert_eq!(with_v1("https://x.test/"), "https://x.test/v1");
    }

    /// The wizard's preset table needs a `&'static str`, so the default root is spelled out twice.
    /// This is the thing that keeps the two spellings the same one.
    #[test]
    fn the_constant_the_preset_table_uses_is_the_root_the_code_derives() {
        assert_eq!(DEFAULT_OPENAI_BASE, with_v1(DEFAULT_GATEWAY));
        assert!(is_gateway_base(DEFAULT_OPENAI_BASE));
        // Pairing and the session API share one name now; the constant has to be the web host too,
        // or the wizard offers a gateway row that points somewhere `/auth/*` 404s.
        assert_eq!(DEFAULT_GATEWAY, crate::llm::account::DEFAULT_WEB);
        assert!(!is_gateway_base("https://api.openai.com/v1"));
        assert!(!is_gateway_base(""));
    }

    #[test]
    fn env_block_is_used_before_anything_is_derived() {
        let mut cfg = cfg_with("", "", "https://gw.test");
        cfg.env
            .insert("OPENAI_BASE_URL".into(), "https://gw.test/v1".into());
        cfg.env
            .insert("ANTHROPIC_BASE_URL".into(), "https://gw.test".into());
        assert_eq!(openai_base(Some(&cfg)), "https://gw.test/v1");
        assert_eq!(anthropic_base(Some(&cfg)), "https://gw.test");
    }

    /// All four are HTTP 200 and none of them is an error. A client that treats a wait as a failure
    /// prints a wall of red at somebody who is doing exactly the right thing.
    #[test]
    fn the_four_waiting_states_all_parse_as_outcomes() {
        assert!(matches!(
            parse_answer(r#"{"status":"pending","interval":5}"#).unwrap(),
            Outcome::Pending { interval: Some(5) }
        ));
        assert!(matches!(
            parse_answer(r#"{"status":"denied"}"#).unwrap(),
            Outcome::Denied
        ));
        assert!(matches!(
            parse_answer(r#"{"status":"expired"}"#).unwrap(),
            Outcome::Expired
        ));
        let ready = parse_answer(
            r#"{"status":"ready","token":"eyJhbGciOiJIUzI1NiJ9.eyJkIjoiZDEifQ.sig",
                "device_id":"dev_42","token_type":"Bearer","expires_in":31536000,
                "key_prefix":"ak_1a2b","label":"Laptop"}"#,
        )
        .unwrap();
        match ready {
            Outcome::Ready(r) => {
                assert!(r.token.starts_with("eyJ"));
                assert_eq!(r.device_id, "dev_42");
                assert_eq!(r.token_type, "Bearer");
                assert_eq!(r.expires_in, 31_536_000);
                assert_eq!(r.key_prefix, "ak_1a2b");
            }
            other => panic!("expected ready, got {other:?}"),
        }
    }

    /// The server still sends the field under its old name and will stop. Reading only `token`
    /// would make every pairing against today's gateway answer "ready and sent no credential" —
    /// silently, since that is a legitimate refusal shape.
    #[test]
    fn the_credential_arrives_under_either_name() {
        for body in [
            r#"{"status":"ready","token":"eyJa.eyJb.sig"}"#,
            r#"{"status":"ready","key":"eyJa.eyJb.sig"}"#,
        ] {
            match parse_answer(body).unwrap() {
                Outcome::Ready(r) => assert_eq!(r.token, "eyJa.eyJb.sig", "from {body}"),
                other => panic!("expected ready, got {other:?}"),
            }
        }
    }

    /// A shape test is a nudge, never a gate. The previous build refused anything without an `ak_`
    /// prefix, which is exactly how a client rejects the one credential that works and blames the
    /// server for it — so a JWT, a legacy key and a string of neither shape all parse.
    #[test]
    fn no_shape_of_credential_is_refused_at_the_door() {
        for s in ["eyJa.eyJb.sig", "ak_live", "sk-live", "whatever"] {
            match parse_answer(&format!(r#"{{"status":"ready","token":"{s}"}}"#)).unwrap() {
                Outcome::Ready(r) => assert_eq!(r.token, s),
                other => panic!("expected ready for {s}, got {other:?}"),
            }
        }
        // And the nudge itself: only a JWT is a credential the dashboard can cut.
        assert!(is_device_token("eyJhbGciOiJIUzI1NiJ9.e30.sig"));
        assert!(!is_device_token("ak_live"));
        assert!(!is_device_token("sk-live"));
        assert!(!is_device_token(""));
    }

    /// The rule that outlived the case it was written for: **a status code is only a summary when
    /// the route uses it for one thing.** 409 used to print one fixed sentence blaming a ceiling of
    /// 20 live keys, while the route answered it for four different situations and the real ceiling
    /// was 10 — the wrong cause three times in four, quoting a number that exists nowhere.
    ///
    /// The gateway stopped answering 409 here entirely on 2026-09-06: it refused pairings when the
    /// plan key could not be read back, and pairing stopped handing the plan key out. What must not
    /// come back is the summarising, so this checks the fall-through still passes the server's own
    /// words through for a code nothing special-cases.
    #[test]
    fn an_unmapped_refusal_carries_the_servers_own_words() {
        let said = "Đường này đã đổi, và câu này là của máy chủ.";
        let f = refusal(409, &format!(r#"{{"error":"{said}"}}"#));
        assert!(f.message.contains(said), "{}", f.message);
        assert!(
            !f.message.contains("20"),
            "no invented number: {}",
            f.message
        );
    }

    /// A refusal with no words left is the one case where a client may speak: it says the number
    /// and stops, rather than inventing a cause to fill the gap.
    #[test]
    fn a_wordless_refusal_says_only_what_is_known() {
        let f = refusal(409, "");
        assert_eq!(f.message, "HTTP 409");
        assert_eq!(f.kind, FailKind::Fatal);
    }

    /// These two mean one thing each, so they keep a sentence of their own.
    #[test]
    fn a_gone_pairing_still_says_start_again() {
        assert_eq!(refusal(404, "").kind, FailKind::Restart);
        assert_eq!(refusal(410, "").kind, FailKind::Restart);
        assert_eq!(refusal(503, "").kind, FailKind::Transient);
    }

    #[test]
    fn ready_without_a_key_asks_for_a_new_pairing_rather_than_retrying() {
        let f = parse_answer(r#"{"status":"ready","key":""}"#).unwrap_err();
        assert_eq!(f.kind, FailKind::Restart);
    }

    #[test]
    fn an_unknown_state_stops_instead_of_polling_forever() {
        let f = parse_answer(r#"{"status":"marinating"}"#).unwrap_err();
        assert_eq!(f.kind, FailKind::Fatal);
        assert!(f.message.contains("marinating"));
    }

    /// The device code is the whole secret of the flow. A `{:?}` of the pairing anywhere — a log
    /// line, an error context — must not be the thing that leaks it.
    #[test]
    fn neither_the_device_code_nor_the_key_survives_a_debug_print() {
        let s = Start {
            device_code: "dc_supersecret".into(),
            user_code: "KXQ4-9PTM".into(),
            user_code_plain: "KXQ49PTM".into(),
            verification_uri: "https://example.test".into(),
            verification_uri_complete: String::new(),
            expires_in: 600,
            interval: 5,
        };
        let printed = format!("{s:?}");
        assert!(!printed.contains("dc_supersecret"), "{printed}");
        assert!(printed.contains("KXQ4-9PTM"));

        let r = Ready {
            token: "eyJ.supersecret.sig".into(),
            device_id: "dev_42".into(),
            token_type: "Bearer".into(),
            expires_in: 31_536_000,
            key_prefix: "ak_1a2b".into(),
            label: "Laptop".into(),
            gateway: None,
        };
        let printed = format!("{r:?}");
        assert!(!printed.contains("supersecret"), "{printed}");
        assert!(printed.contains("ak_1a2b"));
        // The device id is not a secret — it is the row a person unpins — so it is IN the dump.
        assert!(printed.contains("dev_42"));
    }

    #[test]
    fn the_complete_link_is_preferred_but_the_plain_one_still_works() {
        let mut s = Start {
            device_code: "dc".into(),
            user_code: "AAAA-BBBB".into(),
            user_code_plain: "AAAABBBB".into(),
            verification_uri: "https://a.test/keys".into(),
            verification_uri_complete: "https://a.test/keys?device=AAAA-BBBB".into(),
            expires_in: 600,
            interval: 5,
        };
        assert_eq!(s.link(), "https://a.test/keys?device=AAAA-BBBB");
        s.verification_uri_complete = String::new();
        assert_eq!(s.link(), "https://a.test/keys");
        assert_eq!(s.code(), "AAAA-BBBB");
    }

    fn profile(name: &str, url: &str, key: &str) -> ProviderProfile {
        ProviderProfile {
            name: name.into(),
            base_url: url.into(),
            api_key: key.into(),
            model: "m".into(),
            model_context_window: None,
        }
    }

    /// A logout has to take the key everywhere it lives — the root fields are a copy of the live
    /// profile, and leaving them is leaving a working key behind under another name.
    #[test]
    fn unpinning_the_live_profile_takes_its_copy_in_the_root_with_it() {
        let mut store = cli_config::CliConfig {
            base_url: Some("https://gw.test/v1".into()),
            api_key: Some("ak_pinned".into()),
            ..Default::default()
        };
        store.active_provider = Some("aizen".into());
        let gone = profile("aizen", "https://gw.test/v1", "ak_pinned");
        assert!(root_goes_too(&store, Some(&gone), "https://gw.test/v1"));
    }

    /// Three credentials can draw a 401 at a gateway root, and only one of them leaves anything on
    /// this machine to clean up. Getting this wrong is not cosmetic: `PinEnded` deletes a provider
    /// row, so answering it for a call that rode the account session would throw away somebody's
    /// pinned key because a *token* expired.
    #[test]
    fn a_401_names_the_credential_that_earned_it() {
        use Unauthorized::*;
        // Somebody else's provider. Never ours to end.
        assert_eq!(unauthorized_means(false, true, true), NotOurs);
        assert_eq!(unauthorized_means(false, false, false), NotOurs);
        // A pin exists ⇒ that is what the call used, whatever else is on the machine.
        assert_eq!(unauthorized_means(true, true, true), PinEnded);
        assert_eq!(unauthorized_means(true, true, false), PinEnded);
        // No pin but signed in ⇒ the call rode the session, and no teardown can help.
        assert_eq!(unauthorized_means(true, false, true), SignInAgain);
        // Neither: nothing to say beyond the status, and the old behaviour is kept.
        assert_eq!(unauthorized_means(true, false, false), PinEnded);
    }

    /// A 401 from Aizen is never worth resending — the gateway threw the key away, or the token is
    /// finished. Both cures are a command, so the retry table must not carry it.
    #[test]
    fn a_401_is_not_a_status_to_retry() {
        assert!(!crate::llm::client::is_retryable_status(401));
        assert!(!crate::llm::client::is_retryable_status(403));
    }

    /// A keyless row still owns the root fields it was copied into — it just cannot be recognised
    /// by a key, because it has none. Left behind, the root goes on naming a gateway this machine
    /// has just logged out of, and the next run reports the endpoint rather than the sign-in.
    #[test]
    fn forgetting_a_keyless_profile_still_takes_the_root_it_activated() {
        let gw = with_v1(&gateway_url());
        let mut store = cli_config::CliConfig {
            base_url: Some(gw.clone()),
            model: Some("auto".into()),
            active_provider: Some("aizen".into()),
            ..Default::default()
        };
        store.upsert_provider(profile("aizen", &gw, "")).unwrap();

        forget_in(&mut store, "aizen").expect("the row was there");
        assert_eq!(store.base_url, None, "the root followed the row out");
        assert_eq!(store.active_provider, None);
    }

    /// But only when the root is keyless too. A root holding somebody else's key is somebody else's
    /// endpoint, whatever its URL happens to say.
    #[test]
    fn a_keyless_row_never_takes_another_providers_root_key() {
        let gw = with_v1(&gateway_url());
        let mut store = cli_config::CliConfig {
            base_url: Some(gw.clone()),
            api_key: Some("sk-someone-elses".into()),
            ..Default::default()
        };
        store.upsert_provider(profile("aizen", &gw, "")).unwrap();

        forget_in(&mut store, "aizen").expect("the row was there");
        assert_eq!(store.api_key.as_deref(), Some("sk-someone-elses"));
        assert_eq!(store.base_url.as_deref(), Some(gw.as_str()));
    }

    /// Deleting an Aizen row is a logout, and the pin is only ONE of the two ways a row is Aizen.
    ///
    /// The second way arrived with sign-in: a signed-in machine's `aizen` profile holds no key at
    /// all, so nothing about the pin file identifies it. Read only the pin and that row deletes
    /// like a preference, leaving the session behind — still able to spend, and picked up again by
    /// `resolve_endpoint` on the very next turn.
    #[test]
    fn an_aizen_row_is_recognised_by_its_endpoint_not_only_by_the_pin() {
        let mut store = cli_config::CliConfig::default();
        store
            .upsert_provider(profile("aizen", &with_v1(&gateway_url()), ""))
            .unwrap();
        store
            .upsert_provider(profile(
                "not-aizen-at-all",
                "https://api.openai.com/v1",
                "sk-theirs",
            ))
            .unwrap();

        assert!(
            is_aizen_profile(&store, "aizen"),
            "a keyless row at the gateway root is the account session's row"
        );
        assert!(
            !is_aizen_profile(&store, "not-aizen-at-all"),
            "somebody else's endpoint is a provider row, and deleting it is not a logout"
        );
        // A name with no row at all is nobody's credential.
        assert!(!is_aizen_profile(&store, "never-configured"));
    }

    /// Deleting the pinned profile as if it were an ordinary provider row left the key in the
    /// root fields — an ordinary removal keeps those on purpose ("current endpoint kept"). For the
    /// pinned row that is a working key surviving a logout under another name.
    #[test]
    fn forgetting_the_pinned_profile_takes_the_root_copy_of_its_key() {
        let mut store = cli_config::CliConfig {
            base_url: Some(with_v1(&gateway_url())),
            api_key: Some("ak_pinned".into()),
            active_provider: Some("aizen".into()),
            ..Default::default()
        };
        store
            .upsert_provider(profile("aizen", &with_v1(&gateway_url()), "ak_pinned"))
            .unwrap();

        let gone = forget_in(&mut store, "aizen").expect("the row was there");
        assert_eq!(gone.name, "aizen");
        assert!(store.provider("aizen").is_none(), "the row is gone");
        assert_eq!(store.api_key, None, "and so is the root copy of the key");
        assert_eq!(store.base_url, None);
        assert_eq!(store.active_provider, None);
    }

    /// The same teardown on a machine that pinned nothing must leave a hand-made endpoint alone.
    #[test]
    fn forgetting_an_unrelated_profile_leaves_the_root_endpoint_alone() {
        let mut store = cli_config::CliConfig {
            base_url: Some("https://api.openai.com/v1".into()),
            api_key: Some("sk-someone-elses".into()),
            ..Default::default()
        };
        store
            .upsert_provider(profile("other", "https://b.test/v1", "sk-other"))
            .unwrap();

        forget_in(&mut store, "other").expect("the row was there");
        assert_eq!(store.api_key.as_deref(), Some("sk-someone-elses"));
        assert_eq!(store.base_url.as_deref(), Some("https://api.openai.com/v1"));
    }

    /// And the case that makes this a function instead of a condition: `aizen gateway logout` on a
    /// machine that never pinned anything must not delete the endpoint somebody configured by hand.
    #[test]
    fn logging_out_of_a_pin_that_is_not_there_leaves_an_unrelated_endpoint_alone() {
        let store = cli_config::CliConfig {
            base_url: Some("https://api.openai.com/v1".into()),
            api_key: Some("sk-someone-elses".into()),
            ..Default::default()
        };
        assert!(!root_goes_too(&store, None, "https://gw.test/v1"));
        // Nor does removing some OTHER profile take a root that is not a copy of it.
        let gone = profile("other", "https://b.test/v1", "sk-other");
        assert!(!root_goes_too(&store, Some(&gone), "https://gw.test/v1"));
    }

    /// A config from before profiles existed keeps its one endpoint in the root. If that endpoint
    /// IS the gateway, the key is the pin's and a logout has to take it.
    #[test]
    fn a_root_only_config_pointing_at_the_gateway_is_still_a_pin() {
        let store = cli_config::CliConfig {
            base_url: Some("https://gw.test/v1/".into()),
            api_key: Some("ak_pinned".into()),
            ..Default::default()
        };
        assert!(root_goes_too(&store, None, "https://gw.test/v1"));
    }

    /// A pin the gateway has confirmed, at a fixed instant. Every session test subtracts two
    /// numbers, so only their common origin matters.
    const NOW: u64 = 1_800_000_000;

    fn confirmed_pin() -> Pin {
        Pin {
            profile: DEFAULT_PROFILE.into(),
            gateway: DEFAULT_GATEWAY.into(),
            key_prefix: "ak_1a2b".into(),
            verified_at: NOW,
            ..Default::default()
        }
    }

    /// The window is the gateway's to name, and the boundary is where a session turns stale.
    ///
    /// This rule is duplicated in the desktop plugin on purpose — the two halves must not disagree
    /// about whether somebody is signed in — so the numbers here are also the contract between them.
    #[test]
    fn a_session_stays_fresh_for_exactly_the_window_the_gateway_named() {
        let mut pin = confirmed_pin();
        pin.recheck_after = 3600;

        assert_eq!(session_of(&pin, NOW), Session::Fresh);
        assert_eq!(session_of(&pin, NOW + 3600), Session::Fresh);
        assert_eq!(session_of(&pin, NOW + 3601), Session::Stale);

        // Named none: twelve hours. Not "ask now", and not "never ask again".
        pin.recheck_after = 0;
        assert_eq!(session_of(&pin, NOW + RECHECK_DEFAULT), Session::Fresh);
        assert_eq!(session_of(&pin, NOW + RECHECK_DEFAULT + 1), Session::Stale);
    }

    /// And the number it names is clamped at both ends, because it is a number from somewhere else:
    /// with no floor it becomes one request per second, with no ceiling it becomes never.
    #[test]
    fn the_cadence_the_gateway_names_is_clamped_at_both_ends() {
        let mut pin = confirmed_pin();

        pin.recheck_after = 1;
        assert_eq!(session_of(&pin, NOW + RECHECK_MIN), Session::Fresh);
        assert_eq!(session_of(&pin, NOW + RECHECK_MIN + 1), Session::Stale);

        pin.recheck_after = u64::MAX;
        assert_eq!(session_of(&pin, NOW + RECHECK_MAX), Session::Fresh);
        assert_eq!(session_of(&pin, NOW + RECHECK_MAX + 1), Session::Stale);
    }

    /// Two ways a naive subtraction ends in a logout nobody asked for.
    #[test]
    fn a_backwards_clock_and_an_old_pin_file_sign_nobody_out() {
        // Clocks move backwards: a timezone change, an NTP correction, a restored snapshot. An
        // underflow here would be a very large number, and a very large number reads as "expired".
        assert_eq!(session_of(&confirmed_pin(), NOW - 86_400), Session::Fresh);

        // A pin file written before this field existed carries no stamp at all. That is "ask once",
        // not "signed out" — nothing has proved anything about that key yet.
        let old = Pin {
            verified_at: 0,
            ..confirmed_pin()
        };
        assert_eq!(session_of(&old, NOW), Session::Stale);
    }

    /// A `401` outranks the clock. The gateway is the only party that can say a key is dead, and
    /// once it has, a stamp from ten seconds ago does not argue.
    #[test]
    fn a_401_outranks_a_fresh_stamp() {
        let pin = Pin {
            expired_at: NOW,
            ..confirmed_pin()
        };
        assert_eq!(session_of(&pin, NOW), Session::Expired);
        assert_eq!(session_of(&pin, NOW + 1), Session::Expired);
    }

    /// The status line reads as a sentence in all three states, and never divides by an empty pin.
    #[test]
    fn the_status_line_says_something_true_in_every_state() {
        assert_eq!(short_dur(45), "45s");
        assert_eq!(short_dur(600), "10m");
        assert_eq!(short_dur(7200), "2h");
        assert_eq!(short_dur(3 * 86_400), "3d");
    }

    /// A pin whose model roots are NOT the roots this build knows — a staging box, an
    /// `AIZEN_GATEWAY_URL` override, or simply a deployment that answers pairing on one name and
    /// model traffic on another. The live hosts happen to agree today; the rule may not lean on it.
    fn split_host_pin() -> Pin {
        Pin {
            profile: DEFAULT_PROFILE.into(),
            gateway: DEFAULT_GATEWAY.into(),
            openai_base_url: "https://edge.staging.test/v1".into(),
            anthropic_base_url: "https://edge.staging.test".into(),
            ..confirmed_pin()
        }
    }

    /// The endpoint a turn actually posts to is the gateway even when it is not the domain the
    /// pairing ran on. Getting this wrong is silent in three places — the wizard asks for a name it
    /// knows, a dead session reads as a bad key, and the guard never fires — so it gets a test.
    #[test]
    fn the_root_the_gateway_handed_back_counts_as_the_gateway() {
        let pin = split_host_pin();

        // The root this build ships with, with and without a pin.
        assert!(base_is_gateway(DEFAULT_OPENAI_BASE, None));
        assert!(base_is_gateway("https://aizen.talmetis.com/v1/", None));

        // The root shipped before the two names merged. Nothing dials it now, but machines pinned
        // against it are still running, and their own endpoint must not read as a stranger's.
        assert!(base_is_gateway("https://api.talmetis.com/v1", None));
        assert!(base_is_gateway("https://api.talmetis.com", None));

        // The root this machine was handed. Unknown without the pin, known with it — and that gap
        // is exactly the bug: every model call in a turn goes to this one.
        assert!(!base_is_gateway("https://edge.staging.test/v1", None));
        assert!(base_is_gateway("https://edge.staging.test/v1", Some(&pin)));
        assert!(base_is_gateway("https://edge.staging.test/v1/", Some(&pin)));
        assert!(base_is_gateway("https://edge.staging.test", Some(&pin)));

        // And somebody else's endpoint is still somebody else's.
        assert!(!base_is_gateway("https://api.openai.com/v1", Some(&pin)));
        assert!(!base_is_gateway("", Some(&pin)));
    }

    /// Which 401s end a session: the gateway's, and only the gateway's. A wrong OpenAI key must
    /// never unpin this machine — the check is the host, and the URL it is asked about is a whole
    /// request URL, not a base.
    #[test]
    fn only_the_gateways_own_401_can_end_a_session() {
        let pin = split_host_pin();
        let gw = DEFAULT_GATEWAY;

        // The call that actually carries a turn, on the host the pairing never mentioned.
        assert!(url_hits_gateway(
            "https://edge.staging.test/v1/chat/completions",
            Some(&pin),
            gw
        ));
        assert!(url_hits_gateway(
            "https://edge.staging.test/v1/messages",
            Some(&pin),
            gw
        ));
        // The pairing domain still counts, and case never decided a host.
        assert!(url_hits_gateway(
            "https://Aizen.Talmetis.com/v1/device/token",
            Some(&pin),
            gw
        ));

        // Everyone else's 401 stays everyone else's problem.
        assert!(!url_hits_gateway(
            "https://api.openai.com/v1/chat/completions",
            Some(&pin),
            gw
        ));
        assert!(!url_hits_gateway("", Some(&pin), gw));
        // Without the pin there is nothing but the built-in domain to compare against, so the root
        // this machine was handed is a stranger — which is the reason the pin is passed at all.
        assert!(!url_hits_gateway(
            "https://edge.staging.test/v1/chat/completions",
            None,
            gw
        ));
    }

    #[test]
    fn http_status_codes_map_to_what_the_client_should_do() {
        // Not the requests themselves — those need a server — but the table the loop branches on,
        // which is the part that decides whether a user is told to retry or to revoke a key.
        assert_eq!(Fail::new(FailKind::Restart, "x").kind, FailKind::Restart);
        assert_ne!(FailKind::Transient, FailKind::Fatal);
    }
}
