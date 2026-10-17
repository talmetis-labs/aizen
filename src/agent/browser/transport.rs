//! Bounded CDP discovery/transport. No redirect may carry credentials to an unpinned origin.
use super::*;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

pub(super) const MAX_DISCOVERY_BYTES: usize = 256 * 1024;
pub(super) const MAX_TARGETS: usize = 64;
pub(super) const MAX_WS_BYTES: usize = 12 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 128 * 1024;
const MAX_MESSAGES_PER_CALL: usize = 512;

#[derive(Clone)]
pub(super) struct Budget {
    pub deadline: tokio::time::Instant,
    cancel: Option<crate::core::cancel::TurnCancel>,
}
impl Budget {
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: tokio::time::Instant::now() + duration,
            cancel: crate::core::cancel::current(),
        }
    }
    pub async fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        let bounded = tokio::time::timeout_at(self.deadline, future);
        let result = match &self.cancel {
            Some(cancel) => tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("browser operation cancelled; attached target state (including viewport) may be retained; no mutation was replayed"),
                r = bounded => r,
            },
            None => bounded.await,
        };
        result.map_err(|_| anyhow!("browser operation exceeded its total {}s budget; attached target state (including viewport) may be retained; no mutation was replayed", OP_TIMEOUT.as_secs()))?
    }
}

#[derive(Debug)]
pub(super) struct DiscoveredTarget {
    pub id: String,
    pub ws_url: String,
}
fn auth_value(profile_name: &str, profile: &config::BrowserProfile) -> Result<Option<String>> {
    profile
        .auth_env
        .as_deref()
        .map(|var| {
            let value = std::env::var(var).with_context(|| {
                format!("browser profile '{profile_name}' requires environment variable {var}")
            })?;
            // Do not include the source error: header parsers / handshake failures can carry secrets.
            reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| anyhow!("browser auth_env value is not a valid HTTP header"))?;
            Ok(value)
        })
        .transpose()
}

pub(super) fn pin_ws_origin(profile: &config::BrowserProfile, raw: &str) -> Result<url::Url> {
    config::validate_profile(profile)?;
    let endpoint = config::endpoint_url(&profile.endpoint)?;
    let ws = config::ws_url(raw)?;
    let expected_scheme = if endpoint.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    let same = ws.scheme() == expected_scheme
        && ws.host_str() == endpoint.host_str()
        && ws.port_or_known_default() == endpoint.port_or_known_default();
    let explicit = profile.ws_origins.iter().any(|origin| {
        config::ws_url(origin).is_ok_and(|allowed| {
            allowed.scheme() == ws.scheme()
                && allowed.host_str() == ws.host_str()
                && allowed.port_or_known_default() == ws.port_or_known_default()
        })
    });
    if !same && !explicit {
        bail!("CDP advertised a cross-origin websocket; configure its exact ws_origins origin explicitly before attaching or sending authorization");
    }
    // Even an allowlisted origin cannot downgrade a secure HTTP profile to plaintext WS.
    if endpoint.scheme() == "https" && ws.scheme() != "wss" {
        bail!("CDP websocket must not downgrade HTTPS to plaintext");
    }
    Ok(ws)
}

pub(super) fn select_target(targets: Value, selected: Option<&str>) -> Result<DiscoveredTarget> {
    let targets = targets
        .as_array()
        .context("CDP /json did not return a list")?;
    if targets.len() > MAX_TARGETS {
        bail!("CDP target count exceeds {MAX_TARGETS}");
    }
    let pages: Vec<&Value> = targets
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .filter(|t| selected.is_none_or(|id| t.get("id").and_then(Value::as_str) == Some(id)))
        .collect();
    if pages.len() != 1 {
        if pages.is_empty() {
            bail!("no matching open page target; select an existing page with target_id");
        }
        bail!("ambiguous CDP page targets; pass target_id or configure the profile's target_id (never attaching to a random tab)");
    }
    let page = pages[0];
    let id = page
        .get("id")
        .and_then(Value::as_str)
        .context("CDP page has no target id")?;
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        bail!("invalid CDP target id");
    }
    let raw = page
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .context("CDP page has no websocket URL")?;
    config::ws_url(raw)?;
    Ok(DiscoveredTarget {
        id: id.to_owned(),
        ws_url: raw.to_owned(),
    })
}

pub(super) async fn discover(
    profile_name: &str,
    profile: &config::BrowserProfile,
    budget: &Budget,
) -> Result<DiscoveredTarget> {
    config::validate_profile(profile)?;
    budget.run(async {
        let mut url = config::endpoint_url(&profile.endpoint)?;
        let path = format!("{}/json", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5)).timeout(OP_TIMEOUT).no_proxy().build()?;
        let auth = auth_value(profile_name, profile)?;
        let mut request = client.get(url);
        if let Some(value) = auth { request = request.header(reqwest::header::AUTHORIZATION, value); }
        let mut response = request.send().await.map_err(|_| anyhow!("CDP discovery request failed for profile '{profile_name}'"))?;
        if response.status().is_redirection() { bail!("CDP discovery redirects are refused; configure the final HTTP endpoint explicitly"); }
        if !response.status().is_success() { bail!("CDP discovery returned HTTP {}", response.status().as_u16()); }
        if response.content_length().is_some_and(|n| n > MAX_DISCOVERY_BYTES as u64) { bail!("CDP discovery body exceeds {MAX_DISCOVERY_BYTES} bytes"); }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| anyhow!("CDP discovery body read failed"))? {
            if bytes.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BYTES { bail!("CDP discovery body exceeds {MAX_DISCOVERY_BYTES} bytes"); }
            bytes.extend_from_slice(&chunk);
        }
        let target = select_target(serde_json::from_slice(&bytes).context("invalid CDP target list JSON")?, profile.target_id.as_deref())?;
        pin_ws_origin(profile, &target.ws_url)?;
        Ok(target)
    }).await
}

pub(super) async fn ws_connect(
    profile_name: &str,
    profile: &config::BrowserProfile,
    raw: &str,
    budget: &Budget,
) -> Result<WsStream> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    // Pin BEFORE resolving or attaching Authorization, regardless of whether auth is enabled.
    let url = pin_ws_origin(profile, raw)?;
    budget.run(async {
        let mut request = url.as_str().into_client_request().map_err(|_| anyhow!("invalid CDP websocket request"))?;
        if let Some(value) = auth_value(profile_name, profile)? {
            let value = value.parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                .map_err(|_| anyhow!("browser auth_env value is not a valid HTTP header"))?;
            request.headers_mut().insert("authorization", value);
        }
        let config = WebSocketConfig { max_message_size: Some(MAX_WS_BYTES), max_frame_size: Some(MAX_WS_BYTES), ..Default::default() };
        let (ws, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false).await
            .map_err(|_| anyhow!("CDP websocket connect failed for profile '{profile_name}' (details redacted to protect authorization)"))?;
        Ok(ws)
    }).await
}

pub(super) async fn call(client: &mut CdpClient, method: &str, params: Value) -> Result<Value> {
    let budget = client.budget.clone();
    budget
        .run(async {
            let id = client.next_id;
            client.next_id = client
                .next_id
                .checked_add(1)
                .context("CDP request id exhausted")?;
            let msg = json!({"id":id,"method":method,"params":params}).to_string();
            if msg.len() > MAX_COMMAND_BYTES {
                bail!("CDP command exceeds {MAX_COMMAND_BYTES} bytes");
            }
            client
                .ws
                .send(WsMessage::Text(msg))
                .await
                .map_err(|_| anyhow!("CDP send failed"))?;
            let mut total = 0usize;
            for _ in 0..MAX_MESSAGES_PER_CALL {
                match client.ws.next().await {
                    Some(Ok(WsMessage::Text(text))) => {
                        total = total.saturating_add(text.len());
                        if total > MAX_WS_BYTES {
                            bail!("CDP response traffic exceeds {MAX_WS_BYTES} bytes");
                        }
                        let value: Value =
                            serde_json::from_str(&text).context("invalid CDP response JSON")?;
                        if value.get("id").and_then(Value::as_u64) != Some(id) {
                            continue;
                        }
                        if value.get("error").is_some() {
                            bail!("CDP {method} returned a protocol error");
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Some(Ok(WsMessage::Ping(_))) => {
                        client
                            .ws
                            .flush()
                            .await
                            .map_err(|_| anyhow!("CDP send failed"))?;
                    }
                    Some(Ok(WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Close(_))) | None => bail!("CDP connection closed"),
                    Some(Ok(_)) => bail!("unexpected non-text CDP message"),
                    Some(Err(_)) => {
                        bail!("CDP read error (invalid or oversized websocket message)")
                    }
                }
            }
            bail!("CDP message count exceeds {MAX_MESSAGES_PER_CALL}")
        })
        .await
}
