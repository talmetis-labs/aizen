//! Offline configuration projection. Reading this module never connects, spawns or enrolls an app.

use anyhow::{bail, Result};
use serde::Serialize;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CONNECTORS: usize = 256;

#[derive(Debug, Serialize)]
pub(crate) struct ConfiguredConnector {
    pub key: String,
    pub enabled: bool,
    pub transport: ConfiguredTransport,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfiguredTransport {
    Stdio,
    Http,
    Invalid,
}

/// HOME scope deliberately matches Apps' installed list. Effective project resolution can spawn
/// git, so it is not used here. Configuration is neither enrollment nor evidence of a live app.
pub(crate) fn configured(path: &Path, key: Option<&str>) -> Result<Vec<ConfiguredConnector>> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => bail!("HOME MCP configuration is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return select(Vec::new(), key)
        }
        Err(_) => bail!("cannot inspect HOME MCP configuration"),
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return select(Vec::new(), key)
        }
        Err(_) => bail!("cannot read HOME MCP configuration"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot read HOME MCP configuration"))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("HOME MCP configuration exceeds the offline status limit (1 MiB)");
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return select(Vec::new(), key);
    }
    // Do not forward serde's error: invalid values may contain credentials from the config.
    let config: super::super::mcp::McpConfig = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid HOME MCP configuration; inspect mcp.json locally"))?;
    if config.servers.len() > MAX_CONNECTORS {
        bail!("HOME MCP configuration exceeds the offline status limit (256 connectors)");
    }
    let mut connectors = Vec::with_capacity(config.servers.len());
    for (key, server) in config.servers {
        if key.is_empty() || key.len() > 256 || key.chars().any(char::is_control) {
            bail!("HOME MCP configuration contains an invalid connector key");
        }
        let command = server
            .command
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let url = server.url.as_deref().is_some_and(|s| !s.trim().is_empty());
        let transport = match (command, url) {
            (true, false) => ConfiguredTransport::Stdio,
            (false, true) => ConfiguredTransport::Http,
            _ => ConfiguredTransport::Invalid,
        };
        connectors.push(ConfiguredConnector {
            key,
            enabled: server.enabled,
            transport,
        });
    }
    select(connectors, key)
}

fn select(
    mut connectors: Vec<ConfiguredConnector>,
    key: Option<&str>,
) -> Result<Vec<ConfiguredConnector>> {
    if let Some(key) = key {
        connectors.retain(|connector| connector.key == key);
        if connectors.is_empty() {
            bail!("connector is not configured in HOME mcp.json");
        }
    }
    Ok(connectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut id = [0u8; 16];
            getrandom::getrandom(&mut id).unwrap();
            let name: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
            Self(std::env::temp_dir().join(format!("aizen-app-status-{name}")))
        }
        fn write(&self, bytes: &[u8]) -> PathBuf {
            std::fs::create_dir_all(&self.0).unwrap();
            let path = self.0.join("mcp.json");
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_configuration_has_no_filesystem_side_effects() {
        let fixture = Fixture::new();
        assert!(configured(&fixture.0.join("mcp.json"), None)
            .unwrap()
            .is_empty());
        assert!(!fixture.0.exists());
        assert!(configured(&fixture.0.join("mcp.json"), Some("missing")).is_err());
        assert!(!fixture.0.exists());
    }

    #[test]
    fn projection_does_not_expose_secrets_or_execute_configured_command() {
        let fixture = Fixture::new();
        let path = fixture.write(br#"{"mcpServers":{
            "fixture":{"command":"never-execute-this","args":["private-arg"],"env":{"TOKEN":"private-env"}},
            "remote":{"url":"https://user:private-url@example.invalid/private-path","headers":{"Authorization":"private-header"},"enabled":false},
            "invalid":{"command":"one","url":"two"}
        }}"#);
        let rows = configured(&path, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].transport, ConfiguredTransport::Stdio);
        assert_eq!(rows[1].transport, ConfiguredTransport::Invalid);
        assert_eq!(rows[2].transport, ConfiguredTransport::Http);
        assert!(!rows[2].enabled);
        let json = serde_json::to_string(&rows).unwrap();
        for secret in [
            "never-execute",
            "private-arg",
            "private-env",
            "private-url",
            "private-path",
            "private-header",
        ] {
            assert!(!json.contains(secret));
        }
        assert_eq!(configured(&path, Some("fixture")).unwrap().len(), 1);
        assert!(configured(&path, Some("missing")).is_err());
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn malformed_config_never_echoes_invalid_secret_values() {
        let fixture = Fixture::new();
        let path = fixture.write(br#"{"mcpServers":{"fixture":{"enabled":"private-token"}}}"#);
        let error = configured(&path, None).unwrap_err().to_string();
        assert!(!error.contains("private-token"));
        assert!(error.contains("invalid HOME MCP configuration"));
    }

    #[test]
    fn bounded_reader_rejects_oversize_before_parsing() {
        let fixture = Fixture::new();
        let path = fixture.write(&vec![b' '; MAX_CONFIG_BYTES as usize + 1]);
        assert!(configured(&path, None)
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
    }

    #[test]
    fn status_rejects_directory_and_excessive_connector_count() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.0).unwrap();
        assert!(configured(&fixture.0, None).is_err());
        let servers: serde_json::Map<String, serde_json::Value> = (0..=MAX_CONNECTORS)
            .map(|index| {
                (
                    format!("fixture-{index}"),
                    serde_json::json!({"command":"never"}),
                )
            })
            .collect();
        let path =
            fixture.write(&serde_json::to_vec(&serde_json::json!({"mcpServers":servers})).unwrap());
        assert!(configured(&path, None)
            .unwrap_err()
            .to_string()
            .contains("256 connectors"));
    }

    #[test]
    fn control_characters_in_keys_are_not_terminal_output() {
        let fixture = Fixture::new();
        let path = fixture.write(br#"{"mcpServers":{"bad\u001b[2J":{"command":"never"}}}"#);
        assert!(configured(&path, None)
            .unwrap_err()
            .to_string()
            .contains("invalid connector key"));
    }
}
