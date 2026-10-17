//! Versioned browser backend/profile configuration.
//!
//! Only profile labels, CDP endpoints, host routes, and ENVIRONMENT VARIABLE NAMES are persisted.
//! Credential values are resolved from the environment at connect time and never serialized,
//! logged, returned by tools, or embedded in their schemas.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const SCHEMA: u32 = 1;
pub const DEFAULT_PROFILE: &str = "local";
pub const CDP_DEFAULT: &str = "127.0.0.1:9222";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserConfig {
    #[serde(default = "schema_one")]
    pub schema: u32,
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, BrowserProfile>,
    /// URL host (exact or `*.example.com`) → profile name. Longest matching suffix wins.
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserProfile {
    #[serde(default = "default_provider")]
    pub provider: String,
    /// CDP host:port or http(s) base URL. Legacy `AIZEN_BROWSER_CDP` overrides the local default.
    pub endpoint: String,
    /// Environment variable whose VALUE is sent as an Authorization header. The value itself is
    /// never accepted in browser.json.
    #[serde(default)]
    pub auth_env: Option<String>,
    /// Attach to this exact CDP page target id. Without a selector, exactly ONE page must exist.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Explicit WS origins allowed when discovery advertises a different origin. Authorization is
    /// sent only after matching this list or the HTTP endpoint's equivalent WS origin.
    #[serde(default)]
    pub ws_origins: Vec<String>,
}

fn schema_one() -> u32 {
    SCHEMA
}
fn default_profile_name() -> String {
    DEFAULT_PROFILE.to_string()
}
fn default_provider() -> String {
    "cdp".to_string()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            DEFAULT_PROFILE.to_string(),
            BrowserProfile {
                provider: default_provider(),
                endpoint: CDP_DEFAULT.to_string(),
                auth_env: None,
                target_id: None,
                ws_origins: Vec::new(),
            },
        );
        Self {
            schema: SCHEMA,
            default_profile: DEFAULT_PROFILE.to_string(),
            profiles,
            routes: BTreeMap::new(),
        }
    }
}

pub fn config_path() -> PathBuf {
    crate::core::config::aizen_home().join("browser.json")
}

/// Offline availability: implicit localhost defaults do not advertise tools. An explicit validated
/// env endpoint or a bounded browser.json with nonempty explicit profiles is required.
pub(super) fn explicitly_configured(path: &std::path::Path, endpoint: Option<&str>) -> bool {
    load_explicit_at(path, endpoint).is_ok()
}

/// Consume one bounded config-file read and one captured environment value. No existence check then
/// reload, and no implicit localhost fallback: the returned profiles are the actual authorized
/// snapshot used by the operation even if the file/env changes while it is resolving its target.
pub(super) fn load_explicit_at(
    path: &std::path::Path,
    endpoint: Option<&str>,
) -> Result<BrowserConfig> {
    use std::io::Read;
    let endpoint = endpoint.filter(|s| !s.trim().is_empty());
    if let Some(endpoint) = endpoint {
        endpoint_url(endpoint)?;
    }
    let bytes = match std::fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .context("reading explicit browser configuration")?;
            if bytes.len() > 64 * 1024 {
                bail!("explicit browser configuration exceeds 64 KiB");
            }
            bytes
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => bail!("cannot read explicit browser configuration"),
    };
    let mut cfg = if bytes.iter().all(u8::is_ascii_whitespace) {
        if endpoint.is_none() {
            bail!("browser configuration is missing or empty; explicit CDP endpoint/profile required before attaching");
        }
        BrowserConfig::default()
    } else {
        let cfg: BrowserConfig =
            serde_json::from_slice(&bytes).context("invalid explicit browser configuration")?;
        if cfg.schema != SCHEMA {
            bail!("unsupported explicit browser configuration schema");
        }
        if cfg.profiles.is_empty() {
            bail!("browser configuration has no explicit profiles; refusing implicit localhost attachment");
        }
        cfg
    };
    if let Some(endpoint) = endpoint {
        if let Some(local) = cfg.profiles.get_mut(DEFAULT_PROFILE) {
            local.endpoint = endpoint.to_string();
        }
    }
    validate(&cfg)?;
    Ok(cfg)
}

pub(super) fn load_explicit() -> Result<BrowserConfig> {
    let endpoint = crate::core::cli_config::branded_env("BROWSER_CDP");
    load_explicit_at(&config_path(), endpoint.as_deref())
}

pub fn load() -> Result<BrowserConfig> {
    let path = config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BrowserConfig::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(BrowserConfig::default());
    }
    let mut cfg: BrowserConfig =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if cfg.schema != SCHEMA {
        bail!(
            "unsupported browser.json schema {} (this binary supports {SCHEMA})",
            cfg.schema
        );
    }
    if cfg.profiles.is_empty() {
        cfg.profiles = BrowserConfig::default().profiles;
    }
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &BrowserConfig) -> Result<()> {
    if !cfg.profiles.contains_key(&cfg.default_profile) {
        bail!(
            "browser default_profile '{}' is not defined",
            cfg.default_profile
        );
    }
    for (name, profile) in &cfg.profiles {
        if profile.provider != "cdp" {
            bail!(
                "browser profile '{name}' uses unsupported provider '{}' (supported: cdp)",
                profile.provider
            );
        }
        validate_profile(profile)
            .with_context(|| format!("browser profile '{name}' is invalid"))?;
    }
    for (route, profile) in &cfg.routes {
        if route.trim().is_empty() {
            bail!("browser route host cannot be empty");
        }
        if !cfg.profiles.contains_key(profile) {
            bail!("browser route '{route}' references missing profile '{profile}'");
        }
    }
    Ok(())
}

/// Parse only HTTP(S) discovery endpoints; shorthand is restricted to host:port, not arbitrary
/// schemes. Userinfo/query/fragment are forbidden even after the legacy environment override.
pub(super) fn endpoint_url(endpoint: &str) -> Result<url::Url> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() || endpoint.len() > 2048 {
        bail!("CDP endpoint is empty or too long");
    }
    let raw = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    let parsed = url::Url::parse(&raw).context("invalid CDP endpoint")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("CDP discovery endpoint must use http or https");
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("CDP endpoint must not contain userinfo, query credentials, or a fragment; use auth_env");
    }
    Ok(parsed)
}

pub(super) fn ws_url(raw: &str) -> Result<url::Url> {
    if raw.len() > 4096 {
        bail!("CDP websocket URL is too long");
    }
    let parsed = url::Url::parse(raw).context("invalid CDP websocket URL")?;
    if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
        bail!("CDP websocket endpoint must use ws or wss");
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("CDP websocket URL must not contain userinfo, query credentials, or a fragment");
    }
    Ok(parsed)
}

pub(super) fn validate_profile(profile: &BrowserProfile) -> Result<()> {
    if profile.provider != "cdp" {
        bail!("unsupported browser provider (supported: cdp)");
    }
    endpoint_url(&profile.endpoint)?;
    if let Some(var) = profile.auth_env.as_deref() {
        if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("auth_env must be an environment variable name");
        }
    }
    if profile
        .target_id
        .as_deref()
        .is_some_and(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
    {
        bail!("target_id must be a nonempty CDP target id of at most 256 bytes");
    }
    if profile.ws_origins.len() > 16 {
        bail!("at most 16 explicit websocket origins are allowed");
    }
    for origin in &profile.ws_origins {
        let parsed = ws_url(origin)?;
        if parsed.path() != "/" {
            bail!("ws_origins must contain origins only, without target paths");
        }
    }
    Ok(())
}

/// Resolve a profile for a URL. Exact route wins, then the longest `*.suffix` match, then default.
pub(super) fn resolve_for_url_snapshot(
    cfg: &BrowserConfig,
    url: &str,
) -> Result<(String, BrowserProfile)> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
    let mut selected: Option<(&str, &str)> = None;
    if let Some(host) = host.as_deref() {
        for (route, profile) in &cfg.routes {
            let route_l = route.to_ascii_lowercase();
            let matched = if let Some(suffix) = route_l.strip_prefix("*.") {
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                host == route_l
            };
            if matched && selected.is_none_or(|(prev, _)| route_l.len() > prev.len()) {
                selected = Some((route.as_str(), profile.as_str()));
            }
        }
    }
    let name = selected
        .map(|(_, p)| p)
        .unwrap_or(cfg.default_profile.as_str());
    let profile = resolve_named_snapshot(cfg, name)?;
    Ok((name.to_string(), profile))
}

pub(super) fn resolve_named_snapshot(cfg: &BrowserConfig, name: &str) -> Result<BrowserProfile> {
    cfg.profiles
        .get(name)
        .cloned()
        .with_context(|| format!("browser profile '{name}' not found"))
}

#[cfg(test)]
pub fn resolve_for_url(cfg: &BrowserConfig, url: &str) -> Result<(String, BrowserProfile)> {
    let (name, _) = resolve_for_url_snapshot(cfg, url)?;
    Ok((name.clone(), resolve_named(cfg, &name)?))
}

pub fn resolve_named(cfg: &BrowserConfig, name: &str) -> Result<BrowserProfile> {
    let mut profile = cfg
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("browser profile '{name}' not found"))?;
    if name == DEFAULT_PROFILE {
        if let Some(endpoint) = crate::core::cli_config::branded_env("BROWSER_CDP") {
            if !endpoint.trim().is_empty() {
                profile.endpoint = endpoint;
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_execution_snapshot_never_reverts_after_registry_config_disappears() {
        let root = std::env::temp_dir().join(format!(
            "aizen-browser-explicit-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("browser.json");
        let mut cfg = BrowserConfig::default();
        cfg.profiles.get_mut(DEFAULT_PROFILE).unwrap().endpoint = "127.0.0.1:9444".into();
        std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        assert!(explicitly_configured(&path, None));
        let snapshot = load_explicit_at(&path, None).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(load_explicit_at(&path, None).is_err());
        assert_eq!(
            resolve_named_snapshot(&snapshot, DEFAULT_PROFILE)
                .unwrap()
                .endpoint,
            "127.0.0.1:9444"
        );
        for invalid in ["", " ", "{}", "{\"profiles\":{}}"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(load_explicit_at(&path, None).is_err());
        }
        std::fs::remove_file(&path).unwrap();
        let env_snapshot = load_explicit_at(&path, Some("127.0.0.1:9555")).unwrap();
        assert_eq!(
            resolve_named_snapshot(&env_snapshot, DEFAULT_PROFILE)
                .unwrap()
                .endpoint,
            "127.0.0.1:9555"
        );
        assert!(
            load_explicit_at(&path, None).is_err(),
            "removed env endpoint must not reuse implicit default"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn availability_requires_explicit_valid_endpoint_or_profile_without_live_env() {
        let root = std::env::temp_dir().join(format!(
            "aizen-browser-availability-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("browser.json");
        assert!(!explicitly_configured(&path, None));
        assert!(explicitly_configured(&path, Some("127.0.0.1:9222")));
        assert!(!explicitly_configured(&path, Some("")));
        assert!(!explicitly_configured(&path, Some("https://u:p@host")));
        for invalid in [
            "",
            "{}",
            "{\"profiles\":{}}",
            "{\"schema\":2,\"profiles\":{}}",
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(!explicitly_configured(&path, None));
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&BrowserConfig::default()).unwrap(),
        )
        .unwrap();
        assert!(
            explicitly_configured(&path, None),
            "explicitly persisted local profile counts, unlike implicit default"
        );
        std::fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(!explicitly_configured(&path, None));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn routes_choose_exact_then_longest_wildcard_then_default() {
        let mut cfg = BrowserConfig::default();
        cfg.profiles.insert(
            "work".into(),
            BrowserProfile {
                provider: "cdp".into(),
                endpoint: "127.0.0.1:9333".into(),
                auth_env: None,
                target_id: None,
                ws_origins: Vec::new(),
            },
        );
        cfg.profiles.insert(
            "docs".into(),
            BrowserProfile {
                provider: "cdp".into(),
                endpoint: "127.0.0.1:9444".into(),
                auth_env: None,
                target_id: None,
                ws_origins: Vec::new(),
            },
        );
        cfg.routes.insert("*.example.com".into(), "work".into());
        cfg.routes.insert("docs.example.com".into(), "docs".into());
        assert_eq!(
            resolve_for_url(&cfg, "https://docs.example.com/x")
                .unwrap()
                .0,
            "docs"
        );
        assert_eq!(
            resolve_for_url(&cfg, "https://api.example.com/x")
                .unwrap()
                .0,
            "work"
        );
        assert_eq!(
            resolve_for_url(&cfg, "https://other.test/x").unwrap().0,
            DEFAULT_PROFILE
        );
    }

    #[test]
    fn config_rejects_literal_like_auth_field_and_unknown_provider() {
        let raw = r#"{"schema":1,"default_profile":"x","profiles":{"x":{"provider":"playwright","endpoint":"x","auth_env":"TOKEN"}}}"#;
        let cfg: BrowserConfig = serde_json::from_str(raw).unwrap();
        assert!(validate(&cfg)
            .unwrap_err()
            .to_string()
            .contains("unsupported provider"));
        let raw2 = r#"{"schema":1,"default_profile":"x","profiles":{"x":{"provider":"cdp","endpoint":"x","auth_env":"Bearer secret"}}}"#;
        let cfg2: BrowserConfig = serde_json::from_str(raw2).unwrap();
        assert!(
            validate(&cfg2).is_err(),
            "only env names are accepted, never literal credentials"
        );
        let raw3 = r#"{"schema":1,"default_profile":"x","profiles":{"x":{"provider":"cdp","endpoint":"https://user:secret@cdp.example"}}}"#;
        let cfg3: BrowserConfig = serde_json::from_str(raw3).unwrap();
        assert!(
            validate(&cfg3).is_err(),
            "endpoint userinfo must never carry credentials"
        );
    }
}
