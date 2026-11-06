//! Construct minimal explicit provider context only at host orchestration boundaries.

use super::{manager, memory_api as api};
use anyhow::{ensure, Result};
use serde_json::json;
use std::path::Path;

pub(crate) fn selected(
    root: &Path,
    session: &str,
    turn: &str,
    boundary: &str,
) -> Result<Option<api::ProviderContext>> {
    let state = manager::read_state()?;
    if !state.enabled {
        return Ok(None);
    }
    let (id, saved) = manager::selected_at(state.generation)?;
    let installed = manager::info(&id)?;
    let budgets = saved.budgets.clone().unwrap_or_default();
    budgets.validate()?;
    let mut settings =
        super::configuration::effective_settings(&installed.manifest.settings, &saved.settings)?;
    settings.insert(
        "graph_recording".into(),
        json!(!crate::core::config::graph_disabled()),
    );
    settings.insert(
        "graph_expansion".into(),
        json!(crate::core::config::graph_expand_enabled()),
    );
    let capabilities = installed
        .manifest
        .capabilities
        .iter()
        .map(|capability| serde_json::from_value::<api::Capability>(json!(capability)))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|capability| {
            !matches!(
                capability,
                api::Capability::HostLlm
                    | api::Capability::Management
                    | api::Capability::LegacyAdoptionV1
                    | api::Capability::LegacyAdoptionV2
            )
        })
        .collect();
    let cwd = std::fs::canonicalize(root)?;
    ensure!(
        cwd.is_dir(),
        "Memory workspace must be an existing directory"
    );
    let workspace = crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&cwd)?;
    let device = crate::core::device::current();
    let context = api::ProviderContext {
        generation: state.generation,
        user_id: "local".into(),
        workspace_id: workspace.worktree_id,
        workspace_root: workspace.canonical_root.to_string_lossy().into_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        device_id: device.id.clone(),
        device_aliases: device.also_read.clone(),
        session_id: session.into(),
        turn_id: turn.into(),
        boundary_id: boundary.into(),
        data_root: manager::data_root_for(&id, &saved)?
            .to_string_lossy()
            .into_owned(),
        settings,
        granted_capabilities: capabilities,
        budgets: api::Budgets {
            context_tokens: budgets.context_tokens,
            recall_tokens: budgets.recall_tokens,
            max_records: budgets.max_records,
        },
    };
    api::Validate::validate(&context)?;
    Ok(Some(context))
}
