//! Manifest v1: strict, bounded JSON and a portable ASCII package namespace.
//! Copyright 2026 Aizen authors. SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MANIFEST_NAME: &str = "plugin.json";
pub const MAX_MANIFEST_BYTES: usize = 128 * 1024;
pub const MAX_FILES: usize = 512;
pub const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 240;
pub const MAX_DEPTH: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub protocol: Protocol,
    pub target: String,
    pub entrypoint: String,
    pub capabilities: Vec<String>,
    pub permissions: Permissions,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, super::configuration::SettingDefinition>,
    pub files: Vec<PackageFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    pub major: u32,
    pub min_minor: u32,
    pub max_minor: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub network: bool,
    pub host_llm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// SHA256 of exactly the supplied bytes; lowercase 64-character hex.
pub fn sha256(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

pub fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "SHA256 must be 64 lowercase hexadecimal digits"
    );
    Ok(())
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 64, "invalid plugin id length");
    ensure!(
        id.as_bytes()[0].is_ascii_lowercase()
            && id.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric),
        "plugin id must begin with a lowercase letter and end alphanumeric"
    );
    ensure!(
        id.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid plugin id"
    );
    validate_path(id)
}

/// Deliberately ASCII-only; avoids Unicode normalization/case-fold disagreements, Windows ADS,
/// DOS aliases/devices and macOS normalization aliases. Hidden components and 8.3 `~` aliases are
/// unsupported, even when declared. This is a package format restriction, not lossy sanitization.
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= MAX_PATH_BYTES,
        "invalid package path length"
    );
    ensure!(
        path.split('/').count() <= MAX_DEPTH,
        "package path too deep"
    );
    for part in path.split('/') {
        ensure!(
            !part.is_empty()
                && part.len() <= 100
                && !part.starts_with('.')
                && !part.ends_with('.')
                && part != "."
                && part != "..",
            "unsafe package path component"
        );
        ensure!(
            part.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
            "nonportable package path component"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN" | "CONOUT" | "CLOCK"
            ),
            "reserved package path component"
        );
        ensure!(
            !["COM", "LPT"].iter().any(|prefix| stem
                .strip_prefix(prefix)
                .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())),
            "DOS device package path"
        );
    }
    Ok(())
}

/// Namespace includes implied directories: `A/a` with `a/b`, a file `a` with `a/b`, and duplicate
/// explicit directory/file entries all fail rather than selecting platform-dependent content.
#[derive(Default)]
pub(crate) struct Namespace {
    nodes: BTreeMap<String, (String, bool)>,
    explicit: BTreeSet<String>,
}
impl Namespace {
    pub fn insert(&mut self, path: &str, directory: bool) -> Result<()> {
        validate_path(path)?;
        ensure!(
            self.explicit.insert(path.to_ascii_lowercase()),
            "duplicate/case-colliding package entry"
        );
        let parts: Vec<_> = path.split('/').collect();
        for end in 1..=parts.len() {
            let node = parts[..end].join("/");
            let key = node.to_ascii_lowercase();
            let is_dir = end < parts.len() || directory;
            if let Some((old, old_dir)) = self.nodes.get(&key) {
                ensure!(
                    old == &node && *old_dir == is_dir,
                    "case/prefix/file-directory collision"
                );
            } else {
                self.nodes.insert(key, (node, is_dir));
            }
        }
        Ok(())
    }
}

pub fn parse_manifest(bytes: &[u8], expected_target: &str) -> Result<Manifest> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_MANIFEST_BYTES,
        "manifest size limit"
    );
    // Scan before serde recursion/allocation. Struct deserialization rejects duplicate fields
    // including escaped equivalent keys; there is no intermediate Value that can erase them.
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    ensure!(depth <= 12, "manifest nesting limit");
                }
                b']' | b'}' => {
                    depth = depth.checked_sub(1).context("unbalanced manifest")?;
                }
                _ => {}
            }
        }
    }
    // serde's derived struct visitor also accepts positional arrays. JSON v1 requires objects at
    // every struct boundary. Value is used only for shape; duplicates are still rejected by parsing
    // the ORIGINAL bytes into deny_unknown_fields structs immediately afterwards.
    let shape: serde_json::Value = serde_json::from_slice(bytes).context("invalid JSON")?;
    let object = shape.as_object().context("manifest must be an object")?;
    ensure!(
        object
            .get("protocol")
            .is_some_and(serde_json::Value::is_object)
            && object
                .get("permissions")
                .is_some_and(serde_json::Value::is_object),
        "protocol/permissions must be objects"
    );
    let files = object
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("files must be an array")?;
    ensure!(
        files.len() <= MAX_FILES && files.iter().all(serde_json::Value::is_object),
        "files must contain bounded objects"
    );
    super::memory_api::strict_json(bytes)
        .context("manifest must be bounded duplicate-free JSON")?;
    let manifest: Manifest =
        serde_json::from_slice(bytes).context("invalid strict plugin manifest")?;
    manifest.validate(expected_target)?;
    Ok(manifest)
}

impl Manifest {
    pub fn validate(&self, expected_target: &str) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.kind == "memory",
            "unsupported manifest version/kind"
        );
        validate_id(&self.id)?;
        super::configuration::validate_schema(&self.settings)?;
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(|c| c.is_control()
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
            "invalid display name"
        );
        validate_version(&self.version)?;
        ensure!(
            self.protocol.major == 1
                && self.protocol.min_minor == 0
                && self.protocol.max_minor <= 65535,
            "protocol 1.0 must be in a bounded supported range"
        );
        ensure!(
            self.target == expected_target,
            "package target does not match caller target"
        );
        target_kind(&self.target)?;
        validate_path(&self.entrypoint)?;
        if self.target.contains("windows") {
            ensure!(
                self.entrypoint.ends_with(".exe"),
                "Windows entrypoint must be a native .exe"
            );
        }
        const CAPABILITIES: &[&str] = &[
            "recall",
            "records",
            "context",
            "learning",
            "associations",
            "host_llm",
            "commands",
            "management",
            "legacy_adoption_v1",
            "legacy_adoption_v2",
        ];
        ensure!(
            self.capabilities.len() <= CAPABILITIES.len(),
            "too many capabilities"
        );
        let mut caps = BTreeSet::new();
        for cap in &self.capabilities {
            ensure!(
                CAPABILITIES.contains(&cap.as_str()) && caps.insert(cap),
                "unknown/duplicate capability"
            );
        }
        ensure!(
            !caps.contains(&"host_llm".to_string()) || self.permissions.host_llm,
            "host_llm capability needs declared permission"
        );
        ensure!(
            !self.files.is_empty() && self.files.len() <= MAX_FILES,
            "package file count limit"
        );
        let mut namespace = Namespace::default();
        namespace.insert(MANIFEST_NAME, false)?;
        let mut total = 0u64;
        let mut entry = false;
        for file in &self.files {
            namespace.insert(&file.path, false)?;
            validate_digest(&file.sha256)?;
            ensure!(file.bytes <= MAX_FILE_BYTES, "package file size limit");
            total = total
                .checked_add(file.bytes)
                .context("package size overflow")?;
            if file.path == self.entrypoint {
                entry = file.bytes > 0;
            }
        }
        ensure!(
            total <= MAX_PACKAGE_BYTES && entry,
            "package total size or declared entrypoint invalid"
        );
        Ok(())
    }
}

fn validate_version(version: &str) -> Result<()> {
    ensure!(
        !version.is_empty() && version.len() <= 64 && version.is_ascii(),
        "invalid version length"
    );
    let (before_build, build) = version
        .split_once('+')
        .map_or((version, None), |(a, b)| (a, Some(b)));
    let (core, pre) = before_build
        .split_once('-')
        .map_or((before_build, None), |(a, b)| (a, Some(b)));
    let parts: Vec<_> = core.split('.').collect();
    ensure!(parts.len() == 3, "version must be semver");
    for part in parts {
        ensure!(
            !part.is_empty()
                && part.len() <= 10
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')),
            "invalid semver component"
        );
        part.parse::<u32>().context("semver overflow")?;
    }
    for (suffix, prerelease) in [(pre, true), (build, false)] {
        if let Some(suffix) = suffix {
            for part in suffix.split('.') {
                ensure!(
                    !part.is_empty()
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "invalid semver suffix"
                );
                ensure!(
                    !prerelease
                        || !part.bytes().all(|b| b.is_ascii_digit())
                        || part.len() == 1
                        || !part.starts_with('0'),
                    "semver prerelease leading zero"
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn target_kind(target: &str) -> Result<(&'static str, bool)> {
    match target {
        "x86_64-pc-windows-msvc" => Ok(("pe", false)),
        "aarch64-pc-windows-msvc" => Ok(("pe", true)),
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => Ok(("elf", false)),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => Ok(("elf", true)),
        "x86_64-apple-darwin" => Ok(("macho", false)),
        "aarch64-apple-darwin" => Ok(("macho", true)),
        _ => bail!("unsupported package target"),
    }
}

#[cfg(test)]
mod adoption_tests {
    use super::*;

    #[test]
    fn legacy_adoption_is_optional_versioned_and_strict() {
        let mut manifest: Manifest = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "id": "fixture", "name": "Fixture", "version": "1.0.0",
            "kind": "memory", "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
            "target": "x86_64-pc-windows-msvc", "entrypoint": "fixture.exe",
            "capabilities": ["records"], "permissions": {"network": false, "host_llm": false},
            "files": [{"path": "fixture.exe", "bytes": 1, "sha256": "a".repeat(64)}]
        }))
        .unwrap();
        manifest.validate("x86_64-pc-windows-msvc").unwrap();
        manifest.capabilities.push("legacy_adoption_v1".into());
        manifest.validate("x86_64-pc-windows-msvc").unwrap();
        manifest.capabilities.push("legacy_adoption_v1".into());
        assert!(manifest.validate("x86_64-pc-windows-msvc").is_err());
        manifest.capabilities.pop();
        *manifest.capabilities.last_mut().unwrap() = "legacy_adoption_v2".into();
        manifest.validate("x86_64-pc-windows-msvc").unwrap();
        *manifest.capabilities.last_mut().unwrap() = "legacy_adoption_v3".into();
        assert!(manifest.validate("x86_64-pc-windows-msvc").is_err());
    }
}
