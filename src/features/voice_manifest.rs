//! Read the voice distribution manifest published by `vbach8/aizen-voice`.
//!
//! The manifest is an optional layer: an unreachable URL, malformed JSON, or a schema this
//! Aizen build does not understand all mean "use the existing upstream install path". A manifest
//! that *does* parse but carries an invalid or too-new `min_aizen` fails closed: the user asked for
//! a pack that requires a newer client and silently falling back would install something else.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Default published manifest. `AIZEN_VOICE_MANIFEST` overrides it for tests, mirrors and
/// air-gapped installs; `net_guard` accepts only http(s).
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/vbach8/aizen-voice/releases/latest/download/manifest.json";

/// The manifest is tiny; a 64 KiB cap matches the validator's contract and catches a wrong URL
/// returning an HTML page or a huge release asset.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    #[allow(dead_code)]
    pub generated_at: String,
    /// Kept as a `Value`: it is provenance, not installation input. Unknown fields inside it are
    /// skipped by serde and it is not allowed to influence a download.
    #[serde(default)]
    #[allow(dead_code)]
    pub upstream: serde_json::Value,
    #[serde(default)]
    pub engines: BTreeMap<String, PackEntry>,
    #[serde(default)]
    pub models: BTreeMap<String, PackEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackEntry {
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
    #[allow(dead_code)]
    pub license: String,
    #[serde(default)]
    pub binaries: Option<Vec<String>>,
    #[serde(default)]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    pub min_aizen: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub languages: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum ManifestError {
    NewerAizen { required: String, current: String },
    InvalidMinAizen { value: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerAizen { required, current } => write!(
                f,
                "this voice pack requires Aizen {required} or newer; this build is {current} — upgrade Aizen"
            ),
            Self::InvalidMinAizen { value } => {
                write!(f, "manifest has an invalid min_aizen value {value:?}")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    pub fn engine(&self, platform: &str) -> Option<&PackEntry> {
        self.engines.get(platform)
    }

    pub fn model(&self, language: &str) -> Option<&PackEntry> {
        self.models.get(language)
    }
}

impl PackEntry {
    fn check_min_aizen(&self, current: &str) -> Result<(), ManifestError> {
        let Some(required) = self.min_aizen.as_deref() else {
            return Ok(());
        };
        let required_version =
            semver::Version::parse(required).map_err(|_| ManifestError::InvalidMinAizen {
                value: required.to_string(),
            })?;
        let current_version =
            semver::Version::parse(current).map_err(|_| ManifestError::InvalidMinAizen {
                value: current.to_string(),
            })?;
        if required_version > current_version {
            return Err(ManifestError::NewerAizen {
                required: required.to_string(),
                current: current.to_string(),
            });
        }
        Ok(())
    }
}

/// Parse manifest bytes. `Ok(None)` means "not usable, use the upstream fallback": malformed JSON,
/// a schema other than 1, or a document over the cap. A parsed document with an invalid or too-new
/// `min_aizen` is `Err` so callers can show the required version instead of silently falling back.
pub fn load_manifest_from_bytes(
    bytes: &[u8],
    current_version: &str,
) -> Result<Option<Manifest>, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Ok(None);
    }
    let manifest = match serde_json::from_slice::<Manifest>(bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if manifest.schema != 1 {
        return Ok(None);
    }
    for entry in manifest.engines.values().chain(manifest.models.values()) {
        entry.check_min_aizen(current_version)?;
    }
    Ok(Some(manifest))
}

fn emit(line: &str) {
    crate::ui::tui::emit_line(line);
}

/// Fetch and parse the manifest. Any transport/parse/schema failure becomes `Ok(None)`; a typed
/// `min_aizen` failure is returned so the install can refuse with a clear message.
pub async fn fetch_manifest() -> Result<Option<Manifest>> {
    let url = std::env::var("AIZEN_VOICE_MANIFEST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MANIFEST_URL.to_string());

    if let Err(e) = crate::core::net_guard::guard_url_async(&url).await {
        emit(&format!(
            "voice manifest unavailable ({e}) — using upstream"
        ));
        return Ok(None);
    }
    let client = match crate::core::download::http_client() {
        Ok(client) => client,
        Err(e) => {
            emit(&format!(
                "voice manifest unavailable ({e}) — using upstream"
            ));
            return Ok(None);
        }
    };
    let response = match client
        .get(&url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            emit(&format!(
                "voice manifest unavailable ({e}) — using upstream"
            ));
            return Ok(None);
        }
    };
    if !response.status().is_success() {
        emit(&format!(
            "voice manifest unavailable (HTTP {}) — using upstream",
            response.status().as_u16()
        ));
        return Ok(None);
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_MANIFEST_BYTES as u64)
    {
        emit("voice manifest too large — using upstream");
        return Ok(None);
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            emit(&format!(
                "voice manifest unavailable ({e}) — using upstream"
            ));
            return Ok(None);
        }
    };
    match load_manifest_from_bytes(&bytes, env!("CARGO_PKG_VERSION")) {
        Ok(Some(manifest)) => Ok(Some(manifest)),
        Ok(None) => {
            emit("voice manifest schema is not 1 — using upstream");
            Ok(None)
        }
        Err(e) => Err(e).context("voice manifest rejected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const VALID: &str = r#"{
        "schema": 1,
        "generated_at": "2026-10-18T00:00:00Z",
        "upstream": {"sherpa_onnx_engine": {"repo": "k2-fsa/sherpa-onnx", "tag": "v1.13.8"}},
        "engines": {
            "windows-x86_64": {
                "url": "https://github.com/vbach8/aizen-voice/releases/download/voice-v0.1.0/aizen-voice-engine-windows-x86_64.tar.bz2",
                "size_bytes": 13207432,
                "sha256": "cd9dd901447660014c02be6f520d2c4b20b0ab07748a4c992e8f581c75c6a615",
                "license": "Apache-2.0",
                "binaries": ["sherpa-onnx-offline.exe", "sherpa-onnx.exe"],
                "upstream": {"repo": "k2-fsa/sherpa-onnx", "tag": "v1.13.8", "asset": "x"},
                "min_aizen": "0.7.0"
            }
        },
        "models": {
            "vi": {
                "url": "https://github.com/vbach8/aizen-voice/releases/download/voice-v0.1.0/aizen-voice-model-vi.tar.bz2",
                "size_bytes": 59806532,
                "sha256": "404d0ba04cdf8bf64f061d8fdd0178a8e3349c4fb50bf4dc8a607304231eaced",
                "license": "Apache-2.0",
                "files": ["tokens.txt"],
                "unknown_model_field": "ignored"
            }
        },
        "unknown_root_field": "ignored"
    }"#;

    #[test]
    fn parses_the_contract_shape_and_ignores_unknown_fields() {
        let manifest = load_manifest_from_bytes(VALID.as_bytes(), "0.7.2")
            .unwrap()
            .expect("schema 1 manifest");
        assert_eq!(manifest.engines.len(), 1);
        assert_eq!(manifest.models.len(), 1);
        let engine = manifest.engine("windows-x86_64").unwrap();
        assert_eq!(engine.size_bytes, 13_207_432);
        assert_eq!(engine.binaries.as_ref().unwrap().len(), 2);
        assert_eq!(
            manifest.model("vi").unwrap().files.as_ref().unwrap(),
            &["tokens.txt"]
        );
    }

    #[test]
    fn unknown_schema_is_a_fallback() {
        let bytes = VALID.replace("\"schema\": 1", "\"schema\": 2");
        assert!(load_manifest_from_bytes(bytes.as_bytes(), "0.7.2")
            .unwrap()
            .is_none());
    }

    #[test]
    fn missing_platform_is_absent_not_an_error() {
        let manifest = load_manifest_from_bytes(VALID.as_bytes(), "0.7.2")
            .unwrap()
            .unwrap();
        assert!(manifest.engine("linux-x86_64").is_none());
    }

    #[test]
    fn newer_min_aizen_fails_closed_with_the_required_version() {
        let bytes = VALID.replace("\"min_aizen\": \"0.7.0\"", "\"min_aizen\": \"9.9.9\"");
        let err = load_manifest_from_bytes(bytes.as_bytes(), "0.7.2").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("9.9.9"), "{text}");
        assert!(text.contains("upgrade Aizen"), "{text}");
    }

    #[test]
    fn invalid_min_aizen_fails_closed() {
        let bytes = VALID.replace(
            "\"min_aizen\": \"0.7.0\"",
            "\"min_aizen\": \"not-a-version\"",
        );
        assert!(matches!(
            load_manifest_from_bytes(bytes.as_bytes(), "0.7.2"),
            Err(ManifestError::InvalidMinAizen { .. })
        ));
    }

    #[test]
    fn oversized_document_is_a_fallback() {
        let mut bytes = VALID.as_bytes().to_vec();
        bytes.resize(MAX_MANIFEST_BYTES + 1, b' ');
        assert!(load_manifest_from_bytes(&bytes, "0.7.2").unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_a_fallback() {
        assert!(load_manifest_from_bytes(b"{", "0.7.2").unwrap().is_none());
    }

    #[test]
    fn parses_the_checked_out_contract_corpus_when_available() {
        let root = std::env::var_os("AIZEN_VOICE_CONTRACT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../aizen-voice/tests/contract/v1")
            });
        let expectations_path = root.join("expectations.json");
        if !expectations_path.is_file() {
            return; // optional local contract checkout; CI pins it explicitly
        }
        let expectations: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(expectations_path).unwrap()).unwrap();
        let cases = expectations["cases"].as_array().unwrap();
        for case in cases {
            let load = case["load"].as_str().unwrap();
            let path = root.join(case["file"].as_str().unwrap());
            let bytes = std::fs::read(&path).unwrap();
            let result = load_manifest_from_bytes(&bytes, "0.7.2");
            match load {
                "none" => assert!(
                    matches!(result, Ok(None)),
                    "{} should be a fallback",
                    case["id"]
                ),
                "some" => assert!(
                    !matches!(result, Ok(None)),
                    "{} should be structurally parseable",
                    case["id"]
                ),
                other => panic!("unknown load expectation {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_manifest_reads_a_local_http_manifest() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let body = VALID.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        std::env::set_var("AIZEN_ALLOW_PRIVATE_NET", "1");
        std::env::set_var(
            "AIZEN_VOICE_MANIFEST",
            format!("http://{address}/manifest.json"),
        );
        let manifest = fetch_manifest().await.unwrap().expect("local manifest");
        std::env::remove_var("AIZEN_VOICE_MANIFEST");
        std::env::remove_var("AIZEN_ALLOW_PRIVATE_NET");
        server.await.unwrap();
        assert_eq!(manifest.schema, 1);
        assert!(manifest.engine("windows-x86_64").is_some());
    }
}
