//! Persisted Memory-extension state. Installation, eligibility, selection and use are independent.

use super::install::InstallPins;
use super::manifest::{validate_digest, validate_id};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPlugins {
    pub schema_version: u32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub selected: Option<String>,
    #[serde(default)]
    pub packages: BTreeMap<String, PackageState>,
}

impl Default for MemoryPlugins {
    fn default() -> Self {
        Self {
            schema_version: 1,
            generation: 0,
            enabled: false,
            selected: None,
            packages: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageState {
    pub pins: InstallPins,
    pub enabled: bool,
    pub grant: Option<ExecutionGrant>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<super::configuration::ProviderBudgets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<std::path::PathBuf>,
}

/// Explicit local executable consent, bound to the installed manifest, never publisher identity.
/// Model-assisted learning requires its own opt-in; legacy learning grants remain offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionGrant {
    pub manifest_sha256: String,
    pub guarded_execution: bool,
    pub network: bool,
    /// Explicit opt-in to sending bounded completed-turn text to the selected executable.
    #[serde(default)]
    pub learning: bool,
    /// Permit one bounded no-tool job on each completed turn's already-used model route.
    #[serde(default)]
    pub turn_model_learning: bool,
}

impl MemoryPlugins {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported Memory plugin configuration version"
        );
        ensure!(
            self.packages.len() <= 64,
            "too many installed Memory plugins"
        );
        for (id, package) in &self.packages {
            validate_id(id)?;
            ensure!(
                id == &package.pins.id && package.pins.schema_version == 1,
                "package pin identity mismatch"
            );
            ensure!(package.pins.files.len() <= 512, "excessive package pins");
            validate_digest(&package.pins.manifest.sha256)?;
            super::configuration::validate_overrides(&package.settings)?;
            if let Some(budgets) = &package.budgets {
                budgets.validate()?;
            }
            if let Some(root) = &package.data_root {
                ensure!(
                    root.is_absolute() && root.to_str().is_some(),
                    "Memory data directory must be an absolute UTF-8 path"
                );
            }
            if let Some(grant) = &package.grant {
                validate_digest(&grant.manifest_sha256)?;
                ensure!(
                    !grant.turn_model_learning || grant.learning,
                    "same-turn model learning requires completed-turn sharing consent"
                );
                ensure!(
                    grant.manifest_sha256 == package.pins.manifest.sha256,
                    "execution grant does not match installed bytes"
                );
            }
        }
        if let Some(selected) = &self.selected {
            validate_id(selected)?;
            ensure!(
                self.packages.contains_key(selected),
                "selected Memory package is not installed"
            );
        }
        if self.enabled {
            let package = self.selected.as_ref().and_then(|id| self.packages.get(id));
            ensure!(package.is_some_and(|p| p.enabled && p.grant.is_some()), "enabled Memory requires a selected eligible package and explicit execution consent");
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Memory provider generation exhausted"))?;
        Ok(())
    }

    /// One generation, no implicit activation or consent rebinding. The caller verifies bytes and
    /// validates retained settings against the new manifest before invoking this pure transition.
    pub(crate) fn replace_package(
        &mut self,
        expected_generation: u64,
        expected: &PackageState,
        pins: InstallPins,
    ) -> Result<()> {
        ensure!(
            self.generation == expected_generation,
            "Memory generation changed after review"
        );
        ensure!(
            expected.pins.id == "mnemos" && pins.id == "mnemos",
            "update is Mnemos-only"
        );
        ensure!(
            self.packages.get("mnemos") == Some(expected),
            "full package state changed after review"
        );
        let mut next = self.clone();
        let package = next.packages.get_mut("mnemos").expect("checked package");
        package.pins = pins;
        package.enabled = false;
        package.grant = None;
        if next.selected.as_deref() == Some("mnemos") {
            next.enabled = false;
        }
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn record_install(&mut self, pins: InstallPins) -> Result<()> {
        validate_id(&pins.id)?;
        ensure!(
            !self.packages.contains_key(&pins.id),
            "package already recorded; replacement requires a separate operation"
        );
        ensure!(
            self.packages.len() < 64,
            "too many installed Memory plugins"
        );
        let mut next = self.clone();
        next.packages.insert(
            pins.id.clone(),
            PackageState {
                pins,
                enabled: false,
                grant: None,
                settings: BTreeMap::new(),
                budgets: None,
                data_root: None,
            },
        );
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn set_eligible(&mut self, id: &str, enabled: bool) -> Result<()> {
        let mut next = self.clone();
        let package = next
            .packages
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Memory package is not installed"))?;
        if package.enabled == enabled {
            return Ok(());
        }
        package.enabled = enabled;
        if !enabled && next.selected.as_deref() == Some(id) {
            next.enabled = false;
        }
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn select(&mut self, id: &str, grant: ExecutionGrant) -> Result<()> {
        let mut next = self.clone();
        let package = next
            .packages
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Memory package is not installed"))?;
        ensure!(
            package.enabled,
            "plugin is not eligible; run `aizen plugin enable <id>` first"
        );
        ensure!(
            grant.manifest_sha256 == package.pins.manifest.sha256,
            "consent is for different package bytes"
        );
        package.grant = Some(grant);
        next.selected = Some(id.to_owned());
        next.enabled = true;
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn activate(&mut self, id: &str, grant: ExecutionGrant) -> Result<()> {
        let mut next = self.clone();
        next.set_eligible(id, true)?;
        next.select(id, grant)?;
        *self = next;
        Ok(())
    }

    pub fn set_storage(&mut self, id: &str, root: Option<std::path::PathBuf>) -> Result<()> {
        let mut next = self.clone();
        let package = next
            .packages
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Memory package is not installed"))?;
        if package.data_root == root {
            return Ok(());
        }
        package.data_root = root;
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Policy changes invalidate queued learning even when the saved executable grant is unchanged.
    pub(crate) fn learning_policy_changed(&mut self) -> Result<()> {
        self.advance()
    }

    pub fn clear_selection(&mut self) -> Result<()> {
        if self.selected.is_none() && !self.enabled {
            return Ok(());
        }
        let mut next = self.clone();
        next.selected = None;
        next.enabled = false;
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn update_grant(&mut self, id: &str, grant: Option<ExecutionGrant>) -> Result<()> {
        let mut next = self.clone();
        let package = next
            .packages
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Memory package is not installed"))?;
        if package.grant == grant {
            return Ok(());
        }
        package.grant = grant;
        if package.grant.is_none() && next.selected.as_deref() == Some(id) {
            next.enabled = false;
        }
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn configure(
        &mut self,
        id: &str,
        settings: BTreeMap<String, serde_json::Value>,
        budgets: Option<super::configuration::ProviderBudgets>,
    ) -> Result<()> {
        let mut next = self.clone();
        let package = next
            .packages
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Memory package is not installed"))?;
        if package.settings == settings && package.budgets == budgets {
            return Ok(());
        }
        package.settings = settings;
        package.budgets = budgets;
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn set_memory_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.enabled == enabled {
            return Ok(());
        }
        let mut next = self.clone();
        next.enabled = enabled;
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Only called after the manager has settled physical work and removed the package payload.
    /// It intentionally has no data-directory argument or data-deletion operation.
    pub fn record_uninstall(&mut self, id: &str) -> Result<()> {
        let mut next = self.clone();
        ensure!(
            next.packages.remove(id).is_some(),
            "Memory package is not recorded as installed"
        );
        if next.selected.as_deref() == Some(id) {
            next.selected = None;
            next.enabled = false;
        }
        next.advance()?;
        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::install::FilePin;

    fn pins(id: &str) -> InstallPins {
        InstallPins {
            schema_version: 1,
            id: id.into(),
            target: "x86_64-pc-windows-msvc".into(),
            manifest: FilePin {
                path: "plugin.json".into(),
                bytes: 1,
                sha256: "a".repeat(64),
                unix_mode: None,
            },
            files: vec![],
        }
    }
    fn grant() -> ExecutionGrant {
        ExecutionGrant {
            manifest_sha256: "a".repeat(64),
            guarded_execution: true,
            network: false,
            learning: false,
            turn_model_learning: false,
        }
    }

    #[test]
    fn legacy_learning_consent_never_grants_model_jobs() {
        let legacy: ExecutionGrant = serde_json::from_value(serde_json::json!({
            "manifest_sha256":"a".repeat(64),"guarded_execution":true,
            "network":false,"learning":true
        }))
        .unwrap();
        assert!(legacy.learning);
        assert!(!legacy.turn_model_learning);
        let mut state = MemoryPlugins::default();
        state.record_install(pins("fixture")).unwrap();
        state.set_eligible("fixture", true).unwrap();
        let before = state.clone();
        let invalid = ExecutionGrant {
            turn_model_learning: true,
            ..grant()
        };
        assert!(state.select("fixture", invalid).is_err());
        assert_eq!(state, before);
        state
            .select(
                "fixture",
                ExecutionGrant {
                    learning: true,
                    turn_model_learning: true,
                    ..grant()
                },
            )
            .unwrap();
    }

    #[test]
    fn install_and_enable_never_select_or_activate() {
        let mut state = MemoryPlugins::default();
        state.validate().unwrap();
        state.record_install(pins("fixture")).unwrap();
        assert!(!state.enabled && state.selected.is_none());
        assert!(state.packages["fixture"].grant.is_none());
        state.set_eligible("fixture", true).unwrap();
        assert!(!state.enabled && state.selected.is_none());
        assert!(state.set_memory_enabled(true).is_err());
    }

    #[test]
    fn selection_switch_disable_and_uninstall_advance_generation() {
        let mut state = MemoryPlugins::default();
        for id in ["one", "two"] {
            state.record_install(pins(id)).unwrap();
            state.set_eligible(id, true).unwrap();
        }
        state.select("one", grant()).unwrap();
        let selected_epoch = state.generation;
        state.set_memory_enabled(false).unwrap();
        assert_eq!(state.selected.as_deref(), Some("one"));
        assert!(state.generation > selected_epoch);
        state.set_memory_enabled(true).unwrap();
        state.select("two", grant()).unwrap();
        state.set_eligible("two", false).unwrap();
        assert!(!state.enabled);
        assert!(state.set_memory_enabled(true).is_err());
        state.record_uninstall("two").unwrap();
        assert!(state.selected.is_none() && !state.enabled);
        assert!(state.packages.contains_key("one"));
    }

    #[test]
    fn provider_configuration_revocation_and_unselection_are_isolated() {
        let mut state = MemoryPlugins::default();
        for id in ["one", "two"] {
            state.record_install(pins(id)).unwrap();
            state.set_eligible(id, true).unwrap();
        }
        state.select("one", grant()).unwrap();
        let before = state.generation;
        let values = BTreeMap::from([("enable_fuzzy".into(), serde_json::json!(false))]);
        state
            .configure("one", values.clone(), Some(Default::default()))
            .unwrap();
        assert!(state.generation > before && state.enabled);
        assert!(state.packages["two"].settings.is_empty());
        state.set_memory_enabled(false).unwrap();
        let mut consent = grant();
        consent.learning = true;
        state.update_grant("one", Some(consent.clone())).unwrap();
        assert!(
            !state.enabled,
            "editing consent must not enable disabled Memory"
        );
        state.clear_selection().unwrap();
        assert!(state.selected.is_none() && !state.enabled);
        assert_eq!(state.packages["one"].grant, Some(consent));
        assert_eq!(state.packages["one"].settings, values);
        state.select("two", grant()).unwrap();
        state.update_grant("two", None).unwrap();
        assert!(!state.enabled);
        assert_eq!(state.selected.as_deref(), Some("two"));
        assert!(state.packages["two"].grant.is_none());
        assert!(state.packages["one"].grant.is_some());
        let before = state.clone();
        assert!(state
            .configure(
                "two",
                BTreeMap::from([("prepared_job".into(), serde_json::json!(true))]),
                None
            )
            .is_err());
        assert_eq!(state, before);
    }

    #[test]
    fn legacy_package_state_omits_optional_configuration() {
        let legacy = serde_json::json!({"pins":pins("fixture"),"enabled":false,"grant":null});
        let state: PackageState = serde_json::from_value(legacy.clone()).unwrap();
        assert!(state.settings.is_empty() && state.budgets.is_none());
        assert_eq!(serde_json::to_value(state).unwrap(), legacy);
    }

    #[test]
    fn storage_and_picker_activation_are_atomic_and_provider_local() {
        let mut state = MemoryPlugins::default();
        for id in ["one", "two"] {
            state.record_install(pins(id)).unwrap();
        }
        let before = state.clone();
        let mut bad = grant();
        bad.manifest_sha256 = "b".repeat(64);
        assert!(state.activate("one", bad).is_err());
        assert_eq!(state, before);
        state.activate("one", grant()).unwrap();
        assert!(state.enabled && state.packages["one"].enabled);
        let directory = std::env::temp_dir().join("owned-memory-settings-no-io");
        state.set_storage("one", Some(directory.clone())).unwrap();
        assert_eq!(state.packages["one"].data_root.as_ref(), Some(&directory));
        assert!(state.packages["two"].data_root.is_none());
        let generation = state.generation;
        state.set_storage("one", Some(directory)).unwrap();
        assert_eq!(state.generation, generation);
        let before = state.clone();
        assert!(state.set_storage("one", Some("relative".into())).is_err());
        assert_eq!(state, before);
        state.set_storage("one", None).unwrap();
        assert!(state.generation > generation && state.enabled);
    }

    #[test]
    fn invalid_transition_does_not_partially_mutate_state() {
        let mut state = MemoryPlugins::default();
        state.record_install(pins("fixture")).unwrap();
        let before = state.clone();
        assert!(state.select("fixture", grant()).is_err());
        assert_eq!(state, before);
        state.set_eligible("fixture", true).unwrap();
        let before = state.clone();
        let mut invalid = grant();
        invalid.manifest_sha256 = "b".repeat(64);
        assert!(state.select("fixture", invalid).is_err());
        assert_eq!(state, before);
        state.generation = u64::MAX;
        let before = state.clone();
        assert!(state.select("fixture", grant()).is_err());
        assert_eq!(state, before);
    }
}
