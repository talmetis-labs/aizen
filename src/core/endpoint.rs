//! Resolving which endpoint a call goes to, and the shared HTTP client it goes through.
//!
//! Precedence is explicit flag/env first, then saved config, then the account session — one place,
//! so the REPL, the one-shot subcommands, cron and the host bot cannot disagree about which
//! provider is live.
//!
//! **The session is last on purpose.** A machine that pinned a key, or that was pointed at another
//! provider by hand, keeps the endpoint its owner chose; signing in adds a way to work, it does not
//! quietly move traffic. The session only fills a hole — and it fills the whole hole at once
//! (endpoint, credential, model), because filling half of it means a successful sign-in ending at
//! "no model — run `aizen config`", which is the dead end this exists to remove.

use crate::core::cli_config;
use crate::llm::account;
use anyhow::{Context, Result};

/// What `auto` means here: let the server pick the first plan entry callable right now. Used when
/// the session supplies the endpoint and nobody has named a model — the alternative is hard-coding
/// a model name that breaks on the day the plan changes.
const SESSION_MODEL: &str = "auto";

/// The Aizen subscription is sold by signing in: since 2026-09-06 `/v1` takes the session JWT
/// directly, and no endpoint hands out a key string for a plan any more.
///
/// Gated on the base URL, and that gate is the whole safety of it. The token is a bearer credential
/// good for the account's money; sending it anywhere but an Aizen gateway root would hand it to
/// whichever provider the config happens to name. `is_gateway_base` errs towards yes about what
/// counts as the gateway, which is the right direction for a *warning*, so pair it with the fact
/// that this only ever runs when no other credential was found at all.
fn session_key(base_url: &str) -> Option<String> {
    crate::llm::gateway::is_gateway_base(base_url)
        .then(account::v1_token)
        .flatten()
}

/// The endpoint a signed-in machine gets for free: the gateway's own OpenAI root.
fn session_base() -> Option<String> {
    account::signed_in().then(|| crate::llm::gateway::openai_base(None))
}

/// A saved credential that is present but blank is not a credential. The desktop writes the `aizen`
/// profile with no key now (the session is the credential), and an empty string would otherwise be
/// sent as `Authorization: Bearer ` and come back as a 401 naming the wrong problem.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Resolve base URL + API key + model: explicit flag/env (clap) > saved config. Errors name all
/// three ways to provide a missing value.
pub(crate) fn resolve_endpoint(
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<(String, String, String)> {
    let cfg = cli_config::load();
    // Precedence: explicit `--flag` (already folded into the args) > env (`AIZEN_*`) > saved config.
    // Reading env here (not just via clap) means the bare REPL honors it too.
    let base_url = non_empty(
        base_url
            .or_else(|| cli_config::branded_env("BASE_URL"))
            .or(cfg.base_url),
    )
    .or_else(session_base);
    // A gateway session the gateway itself ended has to be said in those words, and this is the one
    // place every caller passes through. Two shapes of the same problem: the endpoint is still the
    // gateway, or there is no endpoint at all because ending the session took it away — the second
    // is why the check runs on `None` too, since otherwise the line below sends somebody to
    // `aizen config` for something only `aizen login` fixes. Local read; no request.
    if base_url
        .as_deref()
        .map(crate::llm::gateway::is_gateway_base)
        .unwrap_or(true)
    {
        crate::llm::gateway::guard_session()?;
    }
    let base_url = base_url
        .context("no base URL — sign in with `aizen account login`, run `aizen config` (interactive setup), or pass --base-url / set AIZEN_BASE_URL")?;
    let api_key = non_empty(
        api_key
            .or_else(|| cli_config::branded_env("API_KEY"))
            .or(cfg.api_key),
    )
    .or_else(|| session_key(&base_url))
    .context("no API key — sign in with `aizen account login` (the Aizen plan needs no key), run `aizen config`, or pass --api-key / set AIZEN_API_KEY")?;
    // Session pin sits between env and disk: a REPL window stays on the model IT resolved, so a
    // sibling window running `/model` (which rewrites the shared cli-config.json) can't switch this
    // one out from under it on the next turn. Non-REPL callers never pin ⇒ they read `cfg.model`.
    let model = non_empty(
        model
            .or_else(|| cli_config::branded_env("MODEL"))
            .or_else(cli_config::session_model)
            .or(cfg.model),
    )
    // Only when the session is what got us here: `auto` is a name the Aizen gateway resolves, and
    // handing it to somebody else's provider would 400 with a model nobody chose.
    .or_else(|| {
        crate::llm::gateway::is_gateway_base(&base_url)
            .then(|| account::signed_in().then(|| SESSION_MODEL.to_string()))
            .flatten()
    })
    .context("no model — run `aizen config` (interactive setup) or `aizen models` to list, or pass --model / set AIZEN_MODEL")?;
    Ok((base_url, api_key, model))
}

/// Codex signs requests with OAuth tokens stored out of band, so there is no API key to resolve.
/// `cli_config::Provider::new` already stores this same placeholder for Codex profiles — returning it
/// here keeps a Codex user from being stopped by "no API key" when the key genuinely does not exist.
fn codex_oauth_api_key(base_url: &str) -> Option<String> {
    crate::llm::oauth_codex::is_codex_base_url(base_url).then(|| "codex-oauth".to_string())
}

pub(crate) fn resolve_base_key(
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<(String, String)> {
    let cfg = cli_config::load();
    let base_url = non_empty(
        base_url
            .or_else(|| cli_config::branded_env("BASE_URL"))
            .or(cfg.base_url),
    )
    .or_else(session_base);
    // Same guard, same reason: `aizen models` on a machine whose session ended should name the
    // command that fixes it rather than the one that does not.
    if base_url
        .as_deref()
        .map(crate::llm::gateway::is_gateway_base)
        .unwrap_or(true)
    {
        crate::llm::gateway::guard_session()?;
    }
    let base_url = base_url
        .context("no base URL — sign in with `aizen account login`, or run `aizen config`")?;
    let api_key = non_empty(
        api_key
            .or_else(|| cli_config::branded_env("API_KEY"))
            .or(cfg.api_key),
    )
    .or_else(|| session_key(&base_url))
    .or_else(|| codex_oauth_api_key(&base_url))
    .context("no API key — sign in with `aizen account login`, or run `aizen config`")?;
    Ok((base_url, api_key))
}

/// Why this client carries NO total-request `timeout`.
///
/// 0.5.2 added `.timeout(1800s)` here as "a backstop under any path nobody has enumerated". That was
/// a bug, and the reason is worth keeping: reqwest's total timeout is applied "from when the request
/// starts connecting until the response body has finished" — a whole-response deadline, not a
/// header-phase one. This very client is what the REPL hands to `stream_chat_with_tools_eager` for
/// every turn, so the ceiling did not merely cap pathological hangs: it cut off a HEALTHY stream that
/// was still emitting tokens, 30 minutes in, losing the entire turn. A deep reasoning run with many
/// tool calls reaches that legitimately.
///
/// The stall protection that a streaming path actually needs is shaped per-event, not per-response,
/// and already exists in two layers: `read_timeout` below (the socket going byte-silent) and
/// `llm::client`'s inter-event watchdog, which re-arms on every SSE event and so distinguishes "the
/// gateway stopped writing" from "the answer is long". A total deadline cannot make that distinction,
/// which is exactly why it is wrong here.
///
/// One-shot clients (health probe, update check, model discovery) DO set a total timeout — nothing
/// they fetch streams, so "the whole response took too long" is a meaningful failure there.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session token is a bearer credential good for the account's money. The gate that keeps
    /// it off every other provider is a base-URL check, and this is the half of it that can be
    /// asserted from a test: no session on disk or not, another provider's root gets nothing.
    #[test]
    fn the_session_token_never_goes_to_another_provider() {
        assert_eq!(session_key("https://api.openai.com/v1"), None);
        assert_eq!(
            session_key("https://generativelanguage.googleapis.com/v1beta"),
            None
        );
        assert_eq!(session_key(""), None);
    }

    /// And the other half: the endpoint a signed-in machine is handed for free must be one that
    /// same gate will open. Split apart, they would silently resolve an endpoint with no credential
    /// and fail at "no API key" on a machine that is signed in.
    #[test]
    fn the_endpoint_the_session_supplies_is_one_the_session_may_open() {
        let root = crate::llm::gateway::openai_base(None);
        assert!(
            crate::llm::gateway::is_gateway_base(&root),
            "session_base() hands out {root}, which session_key() would refuse to open"
        );
    }

    /// A saved-but-blank credential is not a credential: the desktop writes the `aizen` profile
    /// with no key now, and `Some("")` would go out as `Authorization: Bearer ` and come back a 401
    /// naming the wrong problem.
    #[test]
    fn a_blank_saved_value_counts_as_absent() {
        assert_eq!(non_empty(Some("".to_string())), None);
        assert_eq!(non_empty(Some("   ".to_string())), None);
        assert_eq!(
            non_empty(Some("ak_x".to_string())),
            Some("ak_x".to_string())
        );
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn codex_base_uses_oauth_placeholder_without_api_key() {
        assert_eq!(
            codex_oauth_api_key(crate::llm::oauth_codex::CODEX_BASE_URL).as_deref(),
            Some("codex-oauth")
        );
        assert_eq!(codex_oauth_api_key("https://api.openai.com/v1"), None);
    }
}
