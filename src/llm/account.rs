//! The account session — the OTHER credential, and the API it opens.
//!
//! Aizen has two authentication doors, and from the client side they look identical because both
//! read `Authorization: Bearer`:
//!
//!   * `/v1/*`   on `api.talmetis.com`   — the GATEWAY key minted by device pairing
//!     ([`crate::llm::gateway`]). Model calls, `gateway/config`, `plugins/licenses`.
//!   * `/auth/*` on `aizen.talmetis.com` — a session JWT from `POST /auth/login`. Plans,
//!     marketplace model subscriptions, paid plugins.
//!
//! Sending the gateway key to `/auth/*` does not fail as "wrong kind of key": the JWT parser cannot
//! read it and the server answers `401 {"error":"Not signed in"}`, which reads like "you never
//! logged in" and sends people looking in the wrong place. So the two credentials never share a
//! code path here — every call in this module resolves ONLY the session token and never falls back
//! to the gateway key, and [`login`] sends no `Authorization` header at all.
//!
//! **Two hosts, not one.** nginx opens `/auth/` only on the web host, so the web base URL is stored
//! on its own and is never derived from the gateway URL by swapping a domain: a self-hosted install
//! may put the two anywhere. Same reasoning as the two base URLs in [`crate::llm::gateway`].
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

/// The web host: where `/auth/*` lives. NOT the gateway (`api.talmetis.com`).
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
    /// 401/403 → 4, 429 → 5, 400/404/422 → 2, everything else → 1.
    pub fn exit_code(&self) -> i32 {
        match self {
            AuthError::NotSignedIn => 4,
            AuthError::Net(_) => 1,
            AuthError::Http { status, .. } => match status {
                401 | 403 => 4,
                429 => 5,
                400 | 404 | 422 => 2,
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

async fn authed(method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, AuthError> {
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

pub async fn me() -> Result<Value, AuthError> {
    get(ME_ROUTE).await
}

/* ------------------------------------------------------------- listing ids */

/// A marketplace listing id — `seller/model`, e.g. `nbz/glm-5-air`.
///
/// Matches `^[a-z0-9][a-z0-9-]{0,63}/[a-z0-9][a-z0-9._-]{0,95}$` by hand rather than by pulling in
/// a regex engine for one pattern. A second `/` is rejected because it cannot appear in the model
/// half's allowed set.
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
    match s.split_once('/') {
        Some((seller, model)) => part(seller, "-", 64) && part(model, "._-", 96),
        None => false,
    }
}

/// Percent-encode a listing id for a URL path while KEEPING the `/`.
///
/// The `/` is a real path separator on the server (`/auth/subscriptions/nbz/glm-5-air`), and the
/// route matches on it. Encoding the whole id would emit `%2F` and the route would simply not
/// match — so each segment is encoded on its own and the separator is put back.
pub fn encode_listing(id: &str) -> String {
    id.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
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

    #[test]
    fn a_listing_id_needs_exactly_one_slash() {
        assert!(is_listing_id("nbz/glm-5-air"));
        assert!(is_listing_id("a/b"));
        assert!(!is_listing_id("no-slash"));
        assert!(!is_listing_id("two/slashes/here"));
        assert!(!is_listing_id("/leading"));
        assert!(!is_listing_id("trailing/"));
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
