//! Explicit, human-only migration of a selected legacy plan document.
use crate::agent::{plan_control::SelectedPlan, planning_write, prompt_lanes, todo};
use crate::core::{exec_ctx::ExecutionContext, session_store, types::Message};
use anyhow::{Context, Result};

/// Selection is persisted before any file mutation. Decline/error leaves that target selected in
/// planning mode; neither a generic grant nor this operation authorizes implementation.
pub(crate) async fn run(
    explicit_path: Option<&str>,
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&crate::agent::design::DesignContext>,
) -> Result<()> {
    let root = super::planning::workspace_root()?;
    let task = todo::bound_context().context("plan migration requires a conversation owner")?;
    let prior = task.control().snapshot();
    let legacy =
        planning_write::LegacyMigration::read(&root, explicit_path, prior.plan_target.as_ref())?;
    let target = legacy.target().clone();
    super::planning::transition_selected(
        SelectedPlan {
            target: target.clone(),
            observation: None,
        },
        history,
        model,
        design,
    )?;
    crate::ui::tui::emit_line("Migration target saved in planning mode. Decline or failure keeps this selection; no implementation permission is granted.");

    let result = async {
        // Exact native authority routes this write through the normal journal, rather than the
        // ordinary non-scoped atomic-write fallback. It grants no other file or automation tool.
        let scope = crate::core::native_scope::NativeWriteScope::resolve(
            &root,
            std::slice::from_ref(&target.path),
        )?;
        let placement = crate::core::file_placement::PlacementContext::new(
            root.clone(),
            crate::core::config::aizen_home(),
        )?;
        let identity = match design {
            Some(design) => design.snapshot().owner_id,
            None => format!(
                "repl:{}",
                session_store::current_session_slug().context("migration lost its session")?
            ),
        };
        let ctx = ExecutionContext::new(crate::core::convo::ConversationId::new(identity))
            .with_task(task.clone())
            .with_placement(placement)
            .with_native_scope(scope)?;
        let prepared = planning_write::prepare_migration(&legacy, &ctx)?;
        crate::agent::execute_human_plan_migration(prepared, ctx).await
    }
    .await;
    // Success attached only after checked readback. On decline/error refresh only any existing
    // attachment; selection alone must not manufacture a modern document observation.
    let refresh = prompt_lanes::PlanContextRefresh::default().refresh(0, &task, history);
    let design_state = design.map(crate::agent::design::DesignContext::snapshot);
    session_store::update_live_history_with_design(history, design_state.as_ref());
    let output = result.context(
        "migration did not complete; planning mode and its selected target remain saved",
    )?;
    refresh?;
    // Publish the readback attachment and unchanged implementation obligations with the same
    // restricted control, using explicit metadata so persistence cannot discover a Git root.
    let slug = session_store::current_session_slug().context("migration lost its saved session")?;
    session_store::write_session_with_state(
        history,
        &slug,
        session_store::SessionMeta {
            project_key: Some(crate::core::config::normalize_path_key(&root)),
            project_root: Some(root.to_string_lossy().into_owned()),
            model: Some(model.to_owned()),
            ..Default::default()
        },
        design_state.as_ref(),
        &task.snapshot(),
    )
    .context(
        "plan file was migrated and read back, but its session attachment could not be saved",
    )?;
    crate::ui::tui::emit_line(&output);
    Ok(())
}
