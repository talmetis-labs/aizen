//! Historical execution labels and explicit retirement admission.
//! Source-store execution has no runtime authority or implicit Git migration.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    #[default]
    Direct,
    Speculative,
}

impl ExecutionMode {
    /// Historical log labels remain readable, but cannot authorize fresh source effects.
    pub(crate) fn require_supported(self) -> Result<()> {
        ensure!(self == Self::Direct,
            "speculative execution is retired; request explicit Git-owned isolation=worktree instead. No automatic migration, dispatch, commit or integration is authorized");
        Ok(())
    }

    pub(crate) fn from_args(args: &serde_json::Value) -> Result<Self> {
        let mode: Self = match args.get("execution") {
            None => Self::Direct,
            Some(value) => {
                serde_json::from_value(value.clone()).context("execution must be direct")?
            }
        };
        mode.require_supported()?;
        Ok(mode)
    }
}

#[cfg(test)]
mod retirement_tests {
    #[test]
    fn retired_speculative_execution_has_no_implicit_git_or_live_fallback() {
        let error = super::ExecutionMode::from_args(&serde_json::json!({
            "execution": "speculative", "isolation": "worktree"
        }))
        .expect_err("retired custom overlay must refuse before allocation or dispatch");
        assert!(error.to_string().contains("retired"));
        assert_eq!(
            super::ExecutionMode::from_args(&serde_json::json!({})).unwrap(),
            super::ExecutionMode::Direct
        );
        assert_eq!(
            super::ExecutionMode::from_args(&serde_json::json!({"execution":"direct"})).unwrap(),
            super::ExecutionMode::Direct
        );
    }
}
