//! Bounded, nonsecret provider configuration. These values never grant host authority.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_SETTINGS: usize = 24;

#[cfg(test)]
#[path = "configuration_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingDefinition {
    pub label: String,
    pub description: String,
    #[serde(flatten)]
    pub kind: SettingKind,
}

impl<'de> Deserialize<'de> for SettingDefinition {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // `flatten` plus `deny_unknown_fields` rejects valid tagged-enum fields. Remove only the
        // two shared fields, then let the strict tagged variant validate every remaining key.
        let mut object = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        let label = object
            .remove("label")
            .ok_or_else(|| serde::de::Error::missing_field("label"))?;
        let description = object
            .remove("description")
            .ok_or_else(|| serde::de::Error::missing_field("description"))?;
        Ok(Self {
            label: serde_json::from_value(label).map_err(serde::de::Error::custom)?,
            description: serde_json::from_value(description).map_err(serde::de::Error::custom)?,
            kind: serde_json::from_value(Value::Object(object))
                .map_err(serde::de::Error::custom)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingKind {
    Boolean {
        default: bool,
    },
    Integer {
        default: i64,
        min: i64,
        max: i64,
    },
    Number {
        default: serde_json::Number,
        min: serde_json::Number,
        max: serde_json::Number,
    },
    String {
        default: String,
        max_length: usize,
    },
    Enum {
        default: String,
        choices: Vec<String>,
    },
}

fn safe_text(value: &str, limit: usize, empty: bool) -> Result<()> {
    ensure!(
        (empty || !value.trim().is_empty())
            && value.len() <= limit
            && !value.chars().any(|c| c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        "invalid setting text"
    );
    Ok(())
}

pub fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 64
            && key.as_bytes()[0].is_ascii_lowercase()
            && key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "setting key must be lowercase ASCII snake_case"
    );
    ensure!(
        !matches!(
            key,
            "prepared_job"
                | "graph_recording"
                | "graph_expansion"
                | "management"
                | "capabilities"
                | "granted_capabilities"
                | "permissions"
                | "authority"
                | "budgets"
                | "generation"
                | "data_root"
                | "workspace_root"
                | "cwd"
                | "user_id"
                | "device_id"
                | "device_aliases"
                | "workspace_id"
                | "session_id"
                | "turn_id"
                | "boundary_id"
                | "host_llm"
                | "network"
                | "learning"
                | "turn_model_learning"
                | "guarded_execution"
        ),
        "setting key is reserved for host policy"
    );
    Ok(())
}

impl SettingDefinition {
    pub fn default_value(&self) -> Value {
        match &self.kind {
            SettingKind::Boolean { default } => Value::Bool(*default),
            SettingKind::Integer { default, .. } => Value::from(*default),
            SettingKind::Number { default, .. } => Value::Number(default.clone()),
            SettingKind::String { default, .. } | SettingKind::Enum { default, .. } => {
                Value::String(default.clone())
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        safe_text(&self.label, 80, false)?;
        safe_text(&self.description, 400, true)?;
        match &self.kind {
            SettingKind::Integer { min, max, .. } => ensure!(min <= max, "invalid integer bounds"),
            SettingKind::Number { min, max, .. } => {
                let min = min.as_f64().context("invalid numeric bound")?;
                let max = max.as_f64().context("invalid numeric bound")?;
                ensure!(
                    min.is_finite() && max.is_finite() && min <= max,
                    "invalid number bounds"
                );
            }
            SettingKind::String { max_length, .. } => {
                ensure!(*max_length <= 4096, "string limit exceeds protocol cap")
            }
            SettingKind::Enum { choices, .. } => {
                ensure!(
                    !choices.is_empty() && choices.len() <= 32,
                    "invalid enum choices"
                );
                let mut seen = std::collections::BTreeSet::new();
                for choice in choices {
                    safe_text(choice, 128, false)?;
                    ensure!(seen.insert(choice), "duplicate enum choice");
                }
            }
            SettingKind::Boolean { .. } => {}
        }
        self.validate_value(&self.default_value())
    }

    pub fn validate_value(&self, value: &Value) -> Result<()> {
        ensure!(
            serde_json::to_vec(value)?.len() <= 4096,
            "setting exceeds protocol value cap"
        );
        match &self.kind {
            SettingKind::Boolean { .. } => ensure!(value.is_boolean(), "expected a boolean"),
            SettingKind::Integer { min, max, .. } => {
                let number = value.as_i64().context("expected an integer")?;
                ensure!(
                    (*min..=*max).contains(&number),
                    "integer outside declared bounds"
                );
            }
            SettingKind::Number { min, max, .. } => {
                let number = value.as_f64().context("expected a finite number")?;
                ensure!(
                    number.is_finite()
                        && number >= min.as_f64().context("invalid minimum")?
                        && number <= max.as_f64().context("invalid maximum")?,
                    "number outside declared bounds"
                );
            }
            SettingKind::String { max_length, .. } => safe_text(
                value.as_str().context("expected a string")?,
                *max_length,
                true,
            )?,
            SettingKind::Enum { choices, .. } => ensure!(
                choices
                    .iter()
                    .any(|choice| value.as_str() == Some(choice.as_str())),
                "unknown enum choice"
            ),
        }
        Ok(())
    }
}

pub fn validate_schema(schema: &BTreeMap<String, SettingDefinition>) -> Result<()> {
    ensure!(schema.len() <= MAX_SETTINGS, "too many provider settings");
    for (key, definition) in schema {
        validate_key(key)?;
        definition
            .validate()
            .with_context(|| format!("invalid setting {key}"))?;
    }
    Ok(())
}

pub fn effective_settings(
    schema: &BTreeMap<String, SettingDefinition>,
    overrides: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>> {
    validate_schema(schema)?;
    validate_overrides(overrides)?;
    let mut result: BTreeMap<_, _> = schema
        .iter()
        .map(|(key, definition)| (key.clone(), definition.default_value()))
        .collect();
    for (key, value) in overrides {
        let definition = schema
            .get(key)
            .with_context(|| format!("provider does not declare setting {key}"))?;
        definition
            .validate_value(value)
            .with_context(|| format!("invalid setting {key}"))?;
        result.insert(key.clone(), value.clone());
    }
    Ok(result)
}

pub fn validate_overrides(values: &BTreeMap<String, Value>) -> Result<()> {
    ensure!(values.len() <= MAX_SETTINGS, "too many provider overrides");
    for (key, value) in values {
        validate_key(key)?;
        ensure!(
            matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_)),
            "settings must be nonsecret JSON scalars"
        );
        ensure!(
            serde_json::to_vec(value)?.len() <= 4096,
            "setting exceeds protocol value cap"
        );
        if let Some(value) = value.as_str() {
            safe_text(value, 4096, true)?;
        }
    }
    Ok(())
}

/// Consent is validated identically for CLI, UI and the final manager commit.
pub fn validate_grant(
    manifest: &super::manifest::Manifest,
    grant: &super::settings::ExecutionGrant,
) -> Result<()> {
    ensure!(manifest.permissions.network == grant.network,
        "network execution consent must match the package declaration; revoke execution to withdraw required network access");
    #[cfg(windows)]
    ensure!(grant.guarded_execution, "Windows cannot confine plugin filesystem/network access; explicit guarded-execution acknowledgement is required");
    ensure!(
        !grant.learning || manifest.capabilities.iter().any(|cap| cap == "learning"),
        "package does not declare a learning capability"
    );
    ensure!(!grant.turn_model_learning || (grant.learning && manifest.permissions.host_llm
        && manifest.capabilities.iter().any(|cap| cap == "host_llm")),
        "same-turn model learning requires learning consent and declared host_llm capability/permission");
    Ok(())
}

pub fn executable_disclosure(package: &super::install::InstalledPackage) -> Result<String> {
    let executable = package
        .pins
        .files
        .iter()
        .find(|file| file.path == package.manifest.entrypoint)
        .context("verified executable pin is missing")?;
    Ok(format!(
        "Executable: {} {} · {} · SHA256 {}",
        package.manifest.id,
        package.manifest.version,
        package.entrypoint.display(),
        executable.sha256
    ))
}

pub fn consent_disclosures(
    package: &super::install::InstalledPackage,
    grant: &super::settings::ExecutionGrant,
) -> Result<Vec<String>> {
    validate_grant(&package.manifest, grant)?;
    ensure!(
        grant.manifest_sha256 == package.pins.manifest.sha256,
        "consent is for different package bytes"
    );
    let mut lines = vec![
        format!("Authorize executable {} {} for future Memory operations?", package.manifest.id, package.manifest.version),
        executable_disclosure(package)?,
        format!("Pinned manifest SHA256: {}", package.pins.manifest.sha256),
        "This permits future execution and sharing selected Memory inputs with this local executable. Hashes are integrity, not publisher trust.".into(),
        "No legacy data is adopted or moved. Selection itself does not launch the executable.".into(),
    ];
    if grant.guarded_execution {
        lines.push("Guarded mode does NOT confine filesystem/network access. The executable runs as your OS account.".into());
    }
    if grant.network {
        lines.push("Declared network access is authorized, still subject to global sandbox policy. Windows guarded mode does not kernel-enforce network restrictions.".into());
    }
    lines.push(if grant.learning {
        "Completed-turn learning: bounded user/assistant/tool text may be sent to this executable when automatic learning policy permits."
    } else { "Completed-turn learning is NOT authorized." }.into());
    lines.push(if grant.turn_model_learning {
        "One additional bounded no-tool model call per completed turn is authorized on its exact chat endpoint/model, including future chat route choices. Provider-prepared Memory/conversation text can reach remote routes and incur token costs. Credentials stay in Core; no summarizer/fallback is used."
    } else { "Additional same-turn model learning calls are NOT authorized." }.into());
    Ok(lines)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderBudgets {
    pub context_tokens: usize,
    pub recall_tokens: usize,
    pub recall_inject_tokens: usize,
    pub recall_limit: usize,
    pub max_records: usize,
}

impl Default for ProviderBudgets {
    fn default() -> Self {
        Self {
            context_tokens: 1024,
            recall_tokens: 1024,
            recall_inject_tokens: 300,
            recall_limit: 12,
            max_records: 100,
        }
    }
}

impl ProviderBudgets {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.context_tokens <= 4096
                && self.recall_tokens <= 4096
                && self.recall_inject_tokens <= 4096
                && self.recall_limit <= 100
                && self.max_records <= 100,
            "Memory budgets exceed host hard limits"
        );
        Ok(())
    }
    pub fn recall_injection(&self) -> usize {
        self.recall_inject_tokens.min(self.recall_tokens)
    }
    pub fn recall_count(&self) -> usize {
        self.recall_limit.min(self.max_records)
    }
}
