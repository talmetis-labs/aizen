//! Passive, route-scoped model limits. Metadata is advice from the chosen provider, not a probe of
//! the model's real capacity. Never send a completion, follow catalog URLs, or rewrite user config.

use super::cli_config::{CliConfig, ResolvedEndpoint};
use crate::llm::model_metadata::{selected_model_limits, ModelLimits};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

const POSITIVE_TTL: u64 = 24 * 60 * 60;
const NEGATIVE_TTL: u64 = 10 * 60;
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Configured,
    Provider,
    Estimate,
}

impl Source {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured override",
            Self::Provider => "provider metadata",
            Self::Estimate => "name estimate; provider limit unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextWindow {
    pub tokens: usize,
    pub source: Source,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    version: u8,
    checked_at: u64,
    limits: ModelLimits,
}

impl Entry {
    fn valid(&self, now: u64) -> bool {
        let ttl = if self.limits.effective_window().is_some() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        self.version == 1
            && self.checked_at <= now
            && now.saturating_sub(self.checked_at) < ttl
            && [
                self.limits.context_window,
                self.limits.max_input_tokens,
                self.limits.max_output_tokens,
            ]
            .into_iter()
            .flatten()
            .all(|n| (1..=100_000_000).contains(&n))
    }
}

type Flight = Arc<tokio::sync::Mutex<()>>;
static CACHE: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(Mutex::default);
static FLIGHTS: LazyLock<Mutex<HashMap<String, Flight>>> = LazyLock::new(Mutex::default);
static HTTP: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .ok()
});

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn offline() -> bool {
    matches!(
        crate::llm::replay::mode(),
        crate::llm::replay::Mode::Replay | crate::llm::replay::Mode::Strict
    )
}

fn base(base: &str) -> &str {
    base.trim_end_matches('/')
}

/// Hash the complete route including the credential; neither URLs, model IDs nor credentials are
/// written to the cache. The home directory is part of the in-process key for isolated profiles.
fn identity(ep: &ResolvedEndpoint) -> (String, String) {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for part in [base(&ep.base_url), ep.model.as_str(), ep.api_key.as_str()] {
        digest.update(&(part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    let name: String = digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let memory = format!("{}:{name}", super::config::aizen_home().display());
    (memory, name)
}

fn configured(cfg: &CliConfig, ep: &ResolvedEndpoint) -> Option<usize> {
    // Legacy config has one scalar, with no provenance. Honor it only on the complete saved route,
    // never just because another provider/child happens to use the same model ID.
    let root = (cfg.model.as_deref() == Some(ep.model.as_str())
        && cfg
            .base_url
            .as_deref()
            .is_some_and(|url| base(url) == base(&ep.base_url))
        && cfg.api_key.as_deref().unwrap_or("") == ep.api_key)
        .then_some(cfg.model_context_window)
        .flatten();
    root.or_else(|| {
        cfg.providers.as_ref()?.iter().find_map(|p| {
            (p.model == ep.model
                && base(&p.base_url) == base(&ep.base_url)
                && p.api_key == ep.api_key)
                .then_some(p.model_context_window)
                .flatten()
        })
    })
    .filter(|n| *n > 0)
}

fn remember(key: String, entry: Entry) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= MAX_ENTRIES && !cache.contains_key(&key) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, v)| v.checked_at)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(key, entry);
}

fn cached(key: &str, name: &str) -> Option<Entry> {
    let time = now();
    if let Some(entry) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
    {
        return entry.valid(time).then_some(entry);
    }
    let path = super::config::aizen_home()
        .join("cache/model-context")
        .join(format!("{name}.json"));
    // Cache is untrusted local data, with a bound before allocation. Corruption is just a miss.
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 4096 {
        return None;
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    (&mut file).take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    if !entry.valid(time) {
        return None;
    }
    remember(key.to_string(), entry.clone());
    Some(entry)
}

fn store(key: String, name: &str, limits: ModelLimits) {
    let entry = Entry {
        version: 1,
        checked_at: now(),
        limits,
    };
    remember(key, entry.clone());
    let dir = super::config::aizen_home().join("cache/model-context");
    let path = dir.join(format!("{name}.json"));
    // Bounded disk footprint too. This disposable cache never deletes unrelated files or config.
    if !path.exists()
        && std::fs::read_dir(&dir)
            .ok()
            .is_some_and(|rows| rows.take(MAX_ENTRIES).count() >= MAX_ENTRIES)
    {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&entry) {
        let _ = super::persist::atomic_write(&path, &bytes);
    }
}

pub(crate) fn resolve_in(cfg: &CliConfig, ep: &ResolvedEndpoint) -> ContextWindow {
    if let Some(tokens) = configured(cfg, ep) {
        return ContextWindow {
            tokens,
            source: Source::Configured,
        };
    }
    if !offline() {
        let (key, name) = identity(ep);
        if let Some(tokens) = cached(&key, &name).and_then(|e| e.limits.effective_window()) {
            return ContextWindow {
                tokens,
                source: Source::Provider,
            };
        }
    }
    ContextWindow {
        tokens: estimate(&ep.model),
        source: Source::Estimate,
    }
}

pub(crate) fn output_limit(ep: &ResolvedEndpoint) -> Option<usize> {
    if offline() {
        return None;
    }
    let (key, name) = identity(ep);
    cached(&key, &name).and_then(|entry| entry.limits.max_output_tokens)
}

pub(crate) fn resolve(ep: &ResolvedEndpoint) -> ContextWindow {
    resolve_in(&super::cli_config::load(), ep)
}

/// Called only at live execution boundaries, never help/version/startup or offline tape replay.
/// A cold route costs at most three seconds, failures are negatively cached, concurrent children
/// share one flight. No retry/UI/auth-refresh side effects from completion transport machinery.
pub(crate) async fn discover(ep: &ResolvedEndpoint) -> ContextWindow {
    if offline() {
        return ContextWindow {
            tokens: estimate(&ep.model),
            source: Source::Estimate,
        };
    }
    let cfg = super::cli_config::load();
    if configured(&cfg, ep).is_some() {
        return resolve_in(&cfg, ep);
    }
    let (key, name) = identity(ep);
    if cached(&key, &name).is_some() {
        return resolve_in(&cfg, ep);
    }
    // Codex has no compatible /models endpoint. Do not send its placeholder as a bearer token or
    // open an unrelated public catalog; keep the honest estimate/explicit override path.
    if crate::llm::oauth_codex::is_codex_base_url(&ep.base_url) {
        return resolve_in(&cfg, ep);
    }
    let flight = {
        let mut flights = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
        if flights.len() >= MAX_ENTRIES && !flights.contains_key(&key) {
            flights.retain(|_, f| Arc::strong_count(f) > 1);
            if flights.len() >= MAX_ENTRIES {
                return resolve_in(&cfg, ep);
            }
        }
        flights.entry(key.clone()).or_default().clone()
    };
    let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
    let _guard = match tokio::time::timeout_at(deadline, flight.lock()).await {
        Ok(guard) => guard,
        Err(_) => return resolve_in(&cfg, ep),
    };
    if cached(&key, &name).is_none() {
        let limits = tokio::time::timeout_at(deadline, fetch(ep))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        store(key, &name, limits);
    }
    resolve_in(&cfg, ep)
}

async fn json(url: reqwest::Url, ep: &ResolvedEndpoint) -> Option<serde_json::Value> {
    let http = HTTP.as_ref()?;
    let request = crate::llm::client::with_provider_auth(http.get(url), &ep.base_url, &ep.api_key);
    let mut response = request.send().await.ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_BODY.saturating_sub(body.len()) {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

async fn fetch(ep: &ResolvedEndpoint) -> Option<ModelLimits> {
    let mut url = reqwest::Url::parse(&ep.base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.set_path(&format!("{}/models", url.path().trim_end_matches('/')));
    if let Some(value) = json(url.clone(), ep).await {
        if let Some(limits) =
            selected_model_limits(&value, &ep.model).filter(|l| l.effective_window().is_some())
        {
            return Some(limits);
        }
    }
    // Some first-party APIs expose limits only on the exact model detail endpoint. The ID is one
    // encoded path segment, not a URL or a provider-supplied next/catalog link.
    url.path_segments_mut().ok()?.push(&ep.model);
    let value = json(url, ep).await?;
    selected_model_limits(&value, &ep.model).filter(|l| l.effective_window().is_some())
}

/// Last resort only; deliberately not evidence of a provider's deployed context setting.
pub(crate) fn estimate(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("1m") || m.contains("gemini") || m.contains("gpt-4.1") {
        1_000_000
    } else if ["claude", "opus", "sonnet", "haiku"]
        .iter()
        .any(|name| m.contains(name))
    {
        200_000
    } else if m.contains("deepseek") {
        64_000
    } else {
        128_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(base_url: &str, api_key: &str) -> ResolvedEndpoint {
        ResolvedEndpoint {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: "exact-model".into(),
        }
    }

    #[test]
    fn configured_limits_require_the_complete_route() {
        let cfg = CliConfig {
            base_url: Some("https://fixture.invalid/v1/".into()),
            api_key: Some("fixture-key".into()),
            model: Some("exact-model".into()),
            model_context_window: Some(32123),
            ..Default::default()
        };
        assert_eq!(
            configured(&cfg, &endpoint("https://fixture.invalid/v1", "fixture-key")),
            Some(32123)
        );
        assert_eq!(
            configured(&cfg, &endpoint("https://other.invalid/v1", "fixture-key")),
            None
        );
        assert_eq!(
            configured(&cfg, &endpoint("https://fixture.invalid/v1", "other-key")),
            None
        );
    }

    #[test]
    fn cache_ttls_reject_future_corrupt_and_expired_entries() {
        let mut entry = Entry {
            version: 1,
            checked_at: 100,
            limits: ModelLimits::default(),
        };
        assert!(entry.valid(100 + NEGATIVE_TTL - 1));
        assert!(!entry.valid(100 + NEGATIVE_TTL));
        assert!(!entry.valid(99));
        entry.limits.context_window = Some(64000);
        assert!(entry.valid(100 + POSITIVE_TTL - 1));
        assert!(!entry.valid(100 + POSITIVE_TTL));
        entry.limits.max_input_tokens = Some(0);
        assert!(!entry.valid(101));
    }

    #[test]
    fn cache_identity_is_account_scoped_and_payload_contains_no_route() {
        let _lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ep = endpoint("https://fixture.invalid/v1", "fixture-secret");
        let (_, key) = identity(&ep);
        assert_eq!(key.len(), 64);
        assert_eq!(
            key,
            identity(&endpoint("https://fixture.invalid/v1/", "fixture-secret")).1
        );
        assert_ne!(
            key,
            identity(&endpoint("https://fixture.invalid/v1", "different-secret")).1
        );
        let payload = serde_json::to_string(&Entry {
            version: 1,
            checked_at: 100,
            limits: ModelLimits::default(),
        })
        .unwrap();
        assert!(!payload.contains("fixture"));
        assert!(!payload.contains("exact-model"));
    }

    async fn fixture(
        replies: Vec<(&'static str, String)>,
    ) -> (ResolvedEndpoint, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                    bytes.extend_from_slice(&byte);
                    assert!(bytes.len() < 16384);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (
            endpoint(&format!("http://{address}/v1"), "fixture-only-key"),
            task,
        )
    }

    #[tokio::test]
    async fn passive_fetch_selects_exact_model_and_authenticates_without_a_completion() {
        let (ep, server) = fixture(vec![("200 OK", serde_json::json!({"data":[
            {"id":"other-model", "context_window":999999},
            {"id":"exact-model", "context_window":64000,"max_input_tokens":32000,"max_output_tokens":4096}
        ]}).to_string())]).await;
        let limits = fetch(&ep).await.unwrap();
        assert_eq!(limits.effective_window(), Some(32000));
        assert_eq!(limits.max_output_tokens, Some(4096));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-only-key\r\n"));
    }

    #[tokio::test]
    async fn passive_fetch_uses_an_encoded_exact_model_detail_not_fuzzy_aliases() {
        let (mut ep, server) = fixture(vec![
            (
                "200 OK",
                r#"{"data":[{"id":"exact-model","context_window":999999}]}"#.into(),
            ),
            (
                "200 OK",
                r#"{"id":"vendor/model","context_length":"12345"}"#.into(),
            ),
        ])
        .await;
        ep.model = "vendor/model".into();
        assert_eq!(fetch(&ep).await.unwrap().effective_window(), Some(12345));
        let requests = server.await.unwrap();
        assert!(requests[1].starts_with("GET /v1/models/vendor%2Fmodel HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn metadata_redirects_and_oversized_bodies_are_not_followed_or_parsed() {
        let (ep, server) = fixture(vec![(
            "302 Found\r\nLocation: http://example.invalid/private",
            "{}".into(),
        )])
        .await;
        let url = reqwest::Url::parse(&format!("{}/models", ep.base_url)).unwrap();
        assert!(json(url, &ep).await.is_none());
        assert_eq!(server.await.unwrap().len(), 1);
        let (ep, server) = fixture(vec![("200 OK", " ".repeat(MAX_BODY + 1))]).await;
        let url = reqwest::Url::parse(&format!("{}/models", ep.base_url)).unwrap();
        assert!(json(url, &ep).await.is_none());
        // The client may close immediately after reading the size header; do not require the
        // fixture to finish writing an intentionally rejected multi-megabyte response.
        server.abort();
    }

    #[test]
    fn unknown_reasoning_names_do_not_claim_a_million_tokens() {
        assert_eq!(estimate("o3"), 128_000);
        assert_eq!(estimate("o4-mini"), 128_000);
    }
}
