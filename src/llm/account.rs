//! The account session — the OTHER credential, and the API it opens.
//!
//! Aizen has two authentication doors, and from the client side they look identical because both
//! read `Authorization: Bearer`:
//!
//!   * `/v1/*`   — the GATEWAY key minted by device pairing ([`crate::llm::gateway`]). Model calls,
//!     the public catalogue, `gateway/config`, `plugins/licenses`.
//!   * `/auth/*` — a session JWT from `POST /auth/login`. Plans, combos, marketplace model
//!     subscriptions, paid plugins, and your own BYOK endpoints.
//!
//! Both prefixes answer on `aizen.talmetis.com`, so the doors are told apart by PREFIX, not by
//! domain. Presenting the wrong paper is the mistake that survives the hosts being merged, and it
//! does not say what is wrong: a gateway key on `/auth/*` gives `401 {"error":"Not signed in"}`,
//! which reads as "you never logged in" when it means "wrong KIND of credential", and sends people
//! to re-run a login that was never the problem.
//!
//! So the two credentials never share a code path here: every call in this module resolves ONLY the
//! session token and never falls back to the gateway key, and [`login`] sends no `Authorization`
//! header at all.
//!
//! **The traffic goes one way, though: since 2026-09-06 the session token also opens `/v1`.** The
//! subscription is sold by signing in — there is no plan key to fetch, no string to paste, and
//! nothing written to disk but the token itself. `/v1` accepts either credential and tells them
//! apart by SHAPE (a JWT is three dot-separated segments; a key is a prefix plus hex and never
//! contains a dot), so [`v1_token`] is the one sanctioned crossing and [`crate::core::endpoint`]
//! is its only caller. Nothing crosses the other way: a gateway key is still never sent to
//! `/auth/*`, where it reads as "Not signed in" and sends people to redo a login that was fine.
//!
//! A `401` on that crossing means **sign in again** — not a network hiccup to retry. The token
//! lives 30 days, there is no refresh route, and a password change anywhere kills every token
//! minted before it.
//!
//! **One host today, still two fields.** They agree on the live deployment; they are not the same
//! setting. The web base URL is kept on its own and is never derived from the gateway URL by
//! swapping a domain — a self-hosted install may put the two anywhere, and the older deployment did
//! exactly that, opening `/auth/` on the web host alone while `api.talmetis.com` 404'd it. Nor is it
//! read out of `GET /v1/gateway/config`: that response's `base_url` matches the web host only
//! because `AIZEN_GATEWAY_URL` is unset upstream and it falls back to `AIZEN_PUBLIC_URL`. The day
//! that variable is set, a client that derived one from the other starts sending session calls to
//! the gateway and reading 404s.
//!
//! The session token lives 30 days, and a password change kills every token issued before it — so a
//! sudden `401` here is as likely to be a password change on another machine as an expiry.
//!
//! **Nothing here retries.** A `buy` that ran on the server but lost its answer must not be sent
//! twice — the second attempt spends karma again, and the server's 409 ("you already own X") is a
//! last fence rather than a contract, since a plugin *renewal* is legitimately repeatable. reqwest
//! does not retry on its own; this module deliberately adds none.

use crate::core::cli_config;
use crate::core::config::{aizen_home, harden_file};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// The web host: where `/auth/*` lives. The same name as [`crate::llm::gateway::DEFAULT_GATEWAY`],
/// which serves `/v1/*` — one domain, two prefixes, two credentials. Separate constants on purpose:
/// an override may move either one alone.
pub const DEFAULT_WEB: &str = "https://aizen.talmetis.com";

const LOGIN_ROUTE: &str = "/auth/login";
const ME_ROUTE: &str = "/auth/me";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/* ------------------------------------------------------------------ storage */

/// What a signed-in machine keeps. Deliberately separate from `gateway.json`, which holds the
/// gateway key: one file per credential means neither can be handed to the wrong door by accident.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// The web host this token belongs to. Stored rather than recomputed so a token minted against
    /// staging is never replayed at production.
    #[serde(default)]
    pub web_base_url: String,
    /// The session JWT. Printed nowhere, logged nowhere, and never included in `--json` output.
    #[serde(default)]
    pub token: String,
    /// Who the token belongs to, for display only.
    #[serde(default)]
    pub email: String,
}

fn session_path() -> PathBuf {
    aizen_home().join("session.json")
}

pub fn load() -> Option<Session> {
    let raw = std::fs::read_to_string(session_path()).ok()?;
    let s: Session = serde_json::from_str(&raw).ok()?;
    (!s.token.trim().is_empty()).then_some(s)
}

pub fn save(s: &Session) -> Result<()> {
    let path = session_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let body = serde_json::to_string_pretty(s).context("serializing session")?;
    std::fs::write(&path, body + "\n").with_context(|| format!("writing {}", path.display()))?;
    // The token is a bearer credential — owner-only, same as the gateway key.
    harden_file(&path);
    Ok(())
}

pub fn clear() -> bool {
    let path = session_path();
    path.exists() && std::fs::remove_file(&path).is_ok()
}

/// The web host to talk to: `AIZEN_WEB_URL` > what the saved session was minted against > default.
pub fn web_url() -> String {
    if let Some(v) = cli_config::branded_env("WEB_URL") {
        return v.trim_end_matches('/').to_string();
    }
    if let Some(s) = load() {
        let saved = s.web_base_url.trim().trim_end_matches('/');
        if !saved.is_empty() {
            return saved.to_string();
        }
    }
    DEFAULT_WEB.to_string()
}

fn token() -> Option<String> {
    load().map(|s| s.token).filter(|t| !t.trim().is_empty())
}

/// The session token as a **`/v1`** credential — the subscription itself, with no key in the middle.
///
/// Deliberately a second name for [`token`] rather than making that one public. Everything else in
/// this module sends the session to `/auth/*`; this is the single sanctioned crossing to the other
/// prefix, and giving it its own name means `grep v1_token` finds every place a session token is
/// handed to the gateway. Its only caller is [`crate::core::endpoint`], which sends it only when
/// the base URL is an Aizen gateway root — a session token posted to somebody else's provider is a
/// bearer credential handed to a stranger.
pub fn v1_token() -> Option<String> {
    token()
}

/// Is this machine signed in? A local read, no request.
pub fn signed_in() -> bool {
    token().is_some()
}

/* -------------------------------------------------------------------- error */

/// Why a session call did not produce an answer. Kept as a status rather than a string so the
/// command layer can map it to the documented exit codes without re-parsing a message.
#[derive(Debug)]
pub enum AuthError {
    /// No session on this machine at all — "run `aizen account login`", not "401".
    NotSignedIn,
    /// The server answered, and said no.
    Http { status: u16, message: String },
    /// Never reached the server.
    Net(String),
}

impl AuthError {
    /// 401/403 → 4, 429 → 5, 400/404/409/410/422 → 2, everything else → 1.
    ///
    /// 409 and 410 are here for the browser exchange: a code already spent and a code past its two
    /// minutes are both "the thing you sent is stale", the same class as a 404 on a code that never
    /// existed. Grouping them with a plain server error would tell a script to retry, which is the
    /// one thing that cannot work — every one of them needs a fresh sign-in.
    pub fn exit_code(&self) -> i32 {
        match self {
            AuthError::NotSignedIn => 4,
            AuthError::Net(_) => 1,
            AuthError::Http { status, .. } => match status {
                401 | 403 => 4,
                429 => 5,
                400 | 404 | 409 | 410 | 422 => 2,
                _ => 1,
            },
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // A dead token and a missing one lead to the same command, but only one of them is a
            // surprise — say which happened.
            AuthError::NotSignedIn => write!(f, "not signed in — run `aizen account login`"),
            AuthError::Http { status: 401, .. } => {
                write!(f, "session expired — run `aizen account login`")
            }
            AuthError::Http { message, .. } => write!(f, "{message}"),
            AuthError::Net(e) => write!(f, "could not reach {}: {e}", web_url()),
        }
    }
}

/// The most useful sentence in an error body. Every `/auth/*` failure carries `{"error": "..."}`;
/// the server's own words beat anything invented here.
fn server_says(status: u16, json: &Value, raw: &str) -> String {
    for key in ["error", "message", "detail"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if json.is_null() && !raw.trim().is_empty() {
        return format!("HTTP {status} (and the answer was not JSON — a proxy may have replied)");
    }
    format!("HTTP {status}")
}

fn client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AuthError::Net(e.to_string()))
}

/* ---------------------------------------------------------------- the wire */

async fn send(rb: reqwest::RequestBuilder) -> Result<Value, AuthError> {
    let res = rb.send().await.map_err(|e| AuthError::Net(e.to_string()))?;
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status >= 400 {
        return Err(AuthError::Http {
            status,
            message: server_says(status, &json, &text),
        });
    }
    Ok(json)
}

async fn authed(
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, AuthError> {
    let token = token().ok_or(AuthError::NotSignedIn)?;
    let url = format!("{}{}", web_url(), path);
    let mut rb = client()?.request(method, &url).bearer_auth(&token);
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    send(rb).await
}

pub async fn get(path: &str) -> Result<Value, AuthError> {
    authed(reqwest::Method::GET, path, None).await
}

pub async fn post(path: &str, body: Value) -> Result<Value, AuthError> {
    authed(reqwest::Method::POST, path, Some(body)).await
}

/// A whole-resource replacement. The loadout route takes the complete list every time, so callers
/// read-modify-write rather than sending a delta.
pub async fn put(path: &str, body: Value) -> Result<Value, AuthError> {
    authed(reqwest::Method::PUT, path, Some(body)).await
}

/// A partial update. The body is significant down to which keys are *present*: on own-endpoints an
/// absent `api_key` keeps the stored one and an empty-string `api_key` erases it, so this takes the
/// body already built rather than filtering nulls out of it.
pub async fn patch(path: &str, body: Value) -> Result<Value, AuthError> {
    authed(reqwest::Method::PATCH, path, Some(body)).await
}

pub async fn delete(path: &str) -> Result<Value, AuthError> {
    authed(reqwest::Method::DELETE, path, None).await
}

/// Download a file the session is entitled to. Returns the bytes and the server's filename, if it
/// offered one.
pub async fn get_file(path: &str) -> Result<(Vec<u8>, Option<String>), AuthError> {
    let token = token().ok_or(AuthError::NotSignedIn)?;
    let url = format!("{}{}", web_url(), path);
    let res = client()?
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AuthError::Net(e.to_string()))?;
    let status = res.status().as_u16();
    let name = res
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_of);
    if status >= 400 {
        let text = res.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        return Err(AuthError::Http {
            status,
            message: server_says(status, &json, &text),
        });
    }
    let bytes = res
        .bytes()
        .await
        .map_err(|e| AuthError::Net(e.to_string()))?
        .to_vec();
    Ok((bytes, name))
}

fn filename_of(cd: &str) -> Option<String> {
    let lower = cd.to_ascii_lowercase();
    let at = lower.find("filename")?;
    let rest = &cd[at..];
    let eq = rest.find('=')?;
    let v = rest[eq + 1..].trim().trim_matches('"');
    let v = v.split(';').next().unwrap_or(v).trim().trim_matches('"');
    (!v.is_empty()).then(|| v.to_string())
}

/// Trade an email + password for a session JWT.
///
/// Sends NO `Authorization` header. This is the one call that runs before a session exists, and the
/// `/auth` middleware parses any bearer it is given as a JWT — handing it the gateway key here
/// would be refused as 401 before the login logic ever ran.
pub async fn login(web: &str, email: &str, password: &str) -> Result<Session, AuthError> {
    let url = format!("{}{}", web.trim_end_matches('/'), LOGIN_ROUTE);
    let json = send(
        client()?
            .post(&url)
            .json(&serde_json::json!({ "email": email, "password": password })),
    )
    .await?;
    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err(AuthError::Http {
            status: 500,
            message: "the server accepted the login but returned no token".into(),
        });
    }
    Ok(Session {
        web_base_url: web.trim_end_matches('/').to_string(),
        token,
        email: email.to_string(),
    })
}

/* --------------------------------------------------- browser sign-in */

/* The door most accounts actually come through.

`POST /auth/login` only works for an account that HAS a password. Most do not: they were created
through Google or GitHub, `password_hash` is NULL, and no string typed at a prompt can ever match
it. A password form serves a minority and tells the majority they got their own password wrong.

So the real flow is a loopback redirect, the same shape as `mcp_oauth`:

  1. bind `127.0.0.1:0` — the port is IN the URL, so it must exist before the URL does;
  2. open `/auth/cli/authorize?port=…&state=…`;
  3. the browser comes back to `http://127.0.0.1:<port>/?code=…&state=…`;
  4. `POST /auth/cli/exchange {"code"}` → the same JWT `POST /auth/login` would have returned.

This mints no API key. The `ak_…` key is still a separate errand (`/auth/keys` + reveal, or device
pairing) — signing in and holding a key are two different things here. */

const CLI_AUTHORIZE_ROUTE: &str = "/auth/cli/authorize";
const CLI_EXCHANGE_ROUTE: &str = "/auth/cli/exchange";

/// How long the loopback listener waits for the browser to come back.
///
/// Deliberately far longer than the code's own two minutes. That clock starts only once the person
/// HAS a session: for most accounts step one is a Google or GitHub round trip, and the server 302s
/// them through it before it mints a code at all. Timing out at two minutes would drop the listener
/// while they are still typing a password on somebody else's site.
const BROWSER_WAIT: Duration = Duration::from_secs(10 * 60);

/// A browser sign-in that has been started but not finished.
///
/// Split in two so the caller can print the URL between the halves — the browser does not always
/// open (a remote shell, a locked-down desktop), and a flow that only opens a window strands
/// everyone it failed for.
pub struct BrowserFlow {
    listener: tokio::net::TcpListener,
    state: String,
    /// The page to open, and to print.
    pub url: String,
}

/// 24 bytes of system randomness as hex. Hex rather than base64url so nothing in it ever needs
/// percent-encoding on the way out or unescaping on the way back.
fn rand_state() -> Result<String, AuthError> {
    let mut buf = [0u8; 24];
    getrandom::getrandom(&mut buf)
        .map_err(|e| AuthError::Net(format!("system RNG unavailable: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Bind the loopback port and build the page to open.
///
/// The listener is bound BEFORE the URL is built because the port is part of the URL. Building the
/// URL first and binding after leaves a window where the server can redirect to a port nothing
/// holds — a race that only ever shows up on a slow machine.
pub async fn browser_start(web: &str) -> Result<BrowserFlow, AuthError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| AuthError::Net(format!("binding a loopback port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| AuthError::Net(e.to_string()))?
        .port();
    let state = rand_state()?;
    let url = format!(
        "{}{CLI_AUTHORIZE_ROUTE}?port={port}&state={}",
        web.trim_end_matches('/'),
        encode_segment(&state)
    );
    Ok(BrowserFlow {
        listener,
        state,
        url,
    })
}

/// Wait for the redirect, then trade the code for a session.
pub async fn browser_finish(web: &str, flow: BrowserFlow) -> Result<Session, AuthError> {
    let BrowserFlow {
        listener, state, ..
    } = flow;
    let code = wait_for_code(listener, &state).await?;
    let web = web.trim_end_matches('/');

    // No `Authorization` header: the code IS the credential, and this call is what turns it into
    // one. The three refusals — 404 unknown, 409 already spent, 410 expired — come back as the
    // server's own sentence and are NOT flattened into "invalid code": they lead to different
    // actions, and only one of them means "run the command again" twice over.
    let json = send(
        client()?
            .post(format!("{web}{CLI_EXCHANGE_ROUTE}"))
            .json(&serde_json::json!({ "code": code })),
    )
    .await?;

    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err(AuthError::Http {
            status: 500,
            message: "the exchange succeeded but returned no token".into(),
        });
    }

    // The exchange answers with the token, the tenant and the role — but not the address. Ask, so
    // `whoami` and every "signed in as …" line has something to say. A failure here is not a failed
    // sign-in: the token is good, and the address is only a label.
    let email = me_with(web, &token).await;
    Ok(Session {
        web_base_url: web.to_string(),
        token,
        email,
    })
}

/// `/auth/me` with a token that is not on disk yet. Returns the address, or an empty string.
async fn me_with(web: &str, token: &str) -> String {
    let Ok(c) = client() else {
        return String::new();
    };
    let Ok(v) = send(c.get(format!("{web}{ME_ROUTE}")).bearer_auth(token)).await else {
        return String::new();
    };
    // Same trap as everywhere else on this route: it answers 200 with `authenticated: false`. A
    // token that just came out of a successful exchange should never be rejected — but if it is,
    // returning the email off that body would be quoting a stranger.
    if v.get("authenticated").and_then(|x| x.as_bool()) == Some(false) {
        return String::new();
    }
    v.get("email")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// One HTML page back to the browser, so the tab says something instead of failing to connect.
///
/// `message` is always a fixed sentence of ours. Nothing from the query string is ever formatted
/// into it — see the `?error=` branch in [`wait_for_code`].
async fn respond(sock: &mut tokio::net::TcpStream, message: &str) {
    use tokio::io::AsyncWriteExt;
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Aizen</title>\
         <body style=\"font-family:system-ui;background:#0b0b0c;color:#eee;display:grid;place-items:center;height:100vh;margin:0\">\
         <div style=\"text-align:center\"><h2 style=\"color:#e3b341\">Aizen</h2><p>{message}</p></div>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.flush().await;
}

/// Wait for the redirect and return the `code`, having checked `state`.
///
/// **`state` is the only defence this flow has.** A page in the same browser can point at
/// `/auth/cli/authorize` while this listener is up, and the redirect that follows is
/// indistinguishable from ours at the socket — same host, same port, a code the server really did
/// mint. `state` is the one thing that says the code came back from the request WE sent. A mismatch
/// is discarded and never exchanged: exchanging it would burn a stranger's code and adopt whatever
/// account it belonged to.
async fn wait_for_code(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String, AuthError> {
    use tokio::io::AsyncReadExt;

    let deadline = tokio::time::Instant::now() + BROWSER_WAIT;
    let timed_out = || {
        AuthError::Net(format!(
            "timed out after {}s waiting for the browser to come back",
            BROWSER_WAIT.as_secs()
        ))
    };
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let (mut sock, _) = match tokio::time::timeout(remaining, listener.accept()).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(AuthError::Net(format!("loopback accept failed: {e}"))),
            Err(_) => return Err(timed_out()),
        };

        // Read until the request line is whole. One read() is not guaranteed to deliver it, and a
        // truncated line parses to a path with no query — which used to look exactly like a favicon
        // hit and hang the sign-in until the timeout.
        let mut data: Vec<u8> = Vec::with_capacity(8192);
        let mut tmp = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(m)) => {
                    data.extend_from_slice(&tmp[..m]);
                    if data.windows(2).any(|w| w == b"\r\n") || data.len() >= 16 * 1024 {
                        break;
                    }
                }
                _ => break,
            }
        }
        let req = String::from_utf8_lossy(&data);
        let path = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/");
        let (code, state, err) = parse_callback(path);

        if code.is_none() && err.is_none() {
            // A favicon request, a preflight, a browser prefetch. Keep listening.
            respond(&mut sock, "Waiting for the sign-in redirect…").await;
            continue;
        }
        if let Some(e) = err {
            // A fixed sentence, never the server's `?error=` value: that string is chosen by
            // whoever provoked the redirect, and a page on 127.0.0.1 running a stranger's script is
            // a real door, however small. The detail travels in the error below, where the terminal
            // prints it as text.
            respond(&mut sock, "Sign-in failed. You can close this tab.").await;
            return Err(AuthError::Http {
                status: 400,
                message: format!("the sign-in page returned an error: {e}"),
            });
        }
        if state.as_deref() != Some(expected_state) {
            respond(&mut sock, "State mismatch — sign-in aborted.").await;
            return Err(AuthError::Http {
                status: 400,
                message: "state mismatch — that redirect did not come from this sign-in, so the code was discarded".into(),
            });
        }
        respond(
            &mut sock,
            "Signed in. You can close this tab and go back to Aizen.",
        )
        .await;
        return Ok(code.unwrap_or_default());
    }
}

/// Pull `code`, `state` and `error` out of the request target.
///
/// The server redirects to `/` with the query on it — but nothing here depends on the path, so a
/// server that later moves to `/callback` needs no change.
fn parse_callback(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut err) = (None, None, None);
    if let Ok(u) = url::Url::parse(&format!("http://127.0.0.1{path}")) {
        for (k, v) in u.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                "error" => err = Some(v.to_string()),
                _ => {}
            }
        }
    }
    (code, state, err)
}

/// Who the session belongs to — and the one `/auth/*` route that does NOT 401 without one.
///
/// It answers `200 {"authenticated": false}` instead, so an `Ok` here is not proof of a live
/// session: callers must read that field. Everything else in this module can trust the status code;
/// this cannot. `cli::account_cmd::me_says_live` is where that decision is made and tested.
pub async fn me() -> Result<Value, AuthError> {
    get(ME_ROUTE).await
}

/* ------------------------------------------------------------- listing ids */

/// A marketplace listing id — `namespace/model`, e.g. `nbz/glm-5-air`.
///
/// Matches `^[a-z0-9][a-z0-9-]{0,63}(/…)*/[a-z0-9][a-z0-9._-]{0,95}$` by hand rather than by
/// pulling in a regex engine for one pattern.
///
/// **The model name is the LAST segment, and the namespace before it may hold further `/`.** An id
/// is cut from the END, never at the first `/` — the server cuts the same way when it strips a
/// trailing `/confirm` (see [`encode_listing`]). Demanding exactly one `/` here refused a legal id
/// with a sentence calling it malformed, which sends the user off to fix an id already correct.
pub fn is_listing_id(s: &str) -> bool {
    fn part(s: &str, extra: &str, max: usize) -> bool {
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return false;
        }
        if s.len() > max {
            return false;
        }
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(c))
    }
    match s.rsplit_once('/') {
        Some((ns, model)) => {
            !ns.is_empty() && ns.split('/').all(|p| part(p, "-", 64)) && part(model, "._-", 96)
        }
        None => false,
    }
}

/// Percent-encode a listing id for a URL path while KEEPING the `/`.
///
/// The `/` is a real path separator on the server (`/auth/subscriptions/nbz/glm-5-air`), and the
/// route matches on it. Encoding the whole id would emit `%2F` and the route would simply not
/// match — so each segment is encoded on its own and the separator is put back.
///
/// This is exactly what the server expects: its handler takes everything after
/// `/auth/subscriptions/` as the id and strips a trailing `/confirm` from the END, rather than
/// splitting at the first `/`. An id with a slash in it is the normal case there, not an edge one.
pub fn encode_listing(id: &str) -> String {
    id.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Encode one path segment. The unreserved set of RFC 3986 passes through untouched.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing id is cut from the END: the last segment is the model, the rest is the namespace.
    /// A deeper id used to be refused as malformed, which is a wrong sentence about a correct id.
    #[test]
    fn a_listing_id_is_cut_from_the_end() {
        assert!(is_listing_id("nbz/glm-5-air"));
        assert!(is_listing_id("a/b"));
        assert!(
            is_listing_id("two/slashes/here"),
            "a deeper namespace is legal"
        );
        assert!(
            is_listing_id("mine/gpt-4o"),
            "well-formed — just not a subscription"
        );
        assert!(!is_listing_id("no-slash"));
        assert!(!is_listing_id("/leading"));
        assert!(!is_listing_id("trailing/"));
        assert!(
            !is_listing_id("a//b"),
            "an empty middle segment is not a namespace"
        );
        assert!(!is_listing_id("-starts-with-dash/x"));
        assert!(!is_listing_id("UPPER/case"));
    }

    /// The bug this guards: `%2F` makes the cancel route stop matching.
    #[test]
    fn encoding_a_listing_id_keeps_the_separator() {
        assert_eq!(encode_listing("nbz/glm-5-air"), "nbz/glm-5-air");
        assert!(!encode_listing("nbz/glm-5-air").contains("%2F"));
        assert_eq!(encode_listing("a b/c d"), "a%20b/c%20d");
    }

    #[test]
    fn exit_codes_follow_the_documented_table() {
        let http = |status| AuthError::Http {
            status,
            message: String::new(),
        };
        assert_eq!(AuthError::NotSignedIn.exit_code(), 4);
        assert_eq!(http(401).exit_code(), 4);
        assert_eq!(http(403).exit_code(), 4);
        assert_eq!(http(429).exit_code(), 5);
        assert_eq!(http(404).exit_code(), 2);
        assert_eq!(http(422).exit_code(), 2);
        assert_eq!(http(500).exit_code(), 1);
        // The browser exchange: spent and expired are stale input, not server trouble. Reported as
        // errors, a script would retry them, and a retry is the one thing that cannot work.
        assert_eq!(http(409).exit_code(), 2);
        assert_eq!(http(410).exit_code(), 2);
    }

    #[test]
    fn the_callback_query_is_read_off_the_request_line() {
        let (code, state, err) = parse_callback("/?code=cl_abc&state=deadbeef");
        assert_eq!(code.as_deref(), Some("cl_abc"));
        assert_eq!(state.as_deref(), Some("deadbeef"));
        assert!(err.is_none());

        // The path is not load-bearing: the server may move off `/` without a client change.
        let (code, _, _) = parse_callback("/callback?code=cl_abc&state=x");
        assert_eq!(code.as_deref(), Some("cl_abc"));
    }

    /// The bug this guards: a favicon hit parsed as a callback with no code, and the old loop
    /// treated "no code" as fatal instead of carrying on listening.
    #[test]
    fn a_hit_with_no_query_carries_nothing() {
        assert_eq!(parse_callback("/favicon.ico"), (None, None, None));
        assert_eq!(parse_callback("/"), (None, None, None));
    }

    #[test]
    fn an_error_in_the_query_is_kept_apart_from_a_code() {
        let (code, _, err) = parse_callback("/?error=access_denied");
        assert!(code.is_none());
        assert_eq!(err.as_deref(), Some("access_denied"));
    }

    /// `state` is the only thing separating our redirect from one a hostile page provoked, so it
    /// has to be long, hex, and different every time.
    #[test]
    fn the_state_is_fresh_hex_every_time() {
        let a = rand_state().expect("system RNG");
        let b = rand_state().expect("system RNG");
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        // It goes into a query string untouched, so it must need no escaping.
        assert_eq!(encode_segment(&a), a);
    }

    #[test]
    fn a_filename_comes_out_of_content_disposition() {
        assert_eq!(
            filename_of(r#"attachment; filename="pack.zip""#).as_deref(),
            Some("pack.zip")
        );
        assert_eq!(filename_of("inline").as_deref(), None);
    }
}
