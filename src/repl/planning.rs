//! Trusted REPL planning ingress. Never call this for expanded, replayed, or model-generated text.
use crate::agent::{design::DesignContext, plan_control, prompt_lanes, todo};
use crate::core::{session_control::ExecutionMode, session_store, types::Message};
use crate::ui::tui;
use anyhow::{Context, Result};

/// Conservative convenience detector, not a general natural-language permission parser.
/// Only the first raw line may narrow; quotes, documents, expansions and later lines never do.
pub(crate) fn asks_plan_only(raw: &str) -> bool {
    let first = raw.lines().next().unwrap_or("").trim().to_lowercase();
    [
        "plan only",
        "plan-only",
        "do not implement",
        "don't implement",
        "chỉ lên plan",
        "chỉ lập kế hoạch",
        "chỉ lập plan",
        "chỉ lên kế hoạch",
        "chưa triển khai",
    ]
    .iter()
    .any(|prefix| {
        first.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with(|c: char| {
                    c.is_whitespace() || matches!(c, ':' | ',' | '.' | ';' | '—' | '-')
                })
        })
    })
}

/// Call before preprocessing genuine keyboard text. Never grants implementation permission.
pub(crate) fn narrow_from_human(
    raw: &str,
    history: &mut Vec<Message>,
    model: &str,
    design: &DesignContext,
) -> Result<()> {
    if asks_plan_only(raw) && !todo::current_context().control().snapshot().is_planning() {
        transition(true, None, history, model, Some(design))?;
        tui::emit_line("Planning-only request: restriction saved before preparation. Use /plan off explicitly to allow implementation.");
    }
    Ok(())
}

pub(crate) async fn command(
    arg: &str,
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&DesignContext>,
) -> Result<()> {
    let (verb, path) = arg
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((arg.trim(), ""));
    match verb {
        "" | "status" if path.is_empty() => status(history),
        "on" => transition(
            true,
            (!path.trim().is_empty()).then_some(path.trim()),
            history,
            model,
            design,
        ),
        "off" if path.is_empty() => transition(false, None, history, model, design),
        "migrate" => crate::repl::planning_migration::run(
            (!path.trim().is_empty()).then_some(path.trim()),
            history,
            model,
            design,
        )
        .await,
        _ => anyhow::bail!("usage: /plan on [docs/plans/name.md] | /plan status | /plan off | /plan migrate [docs/plans/name.md]"),
    }
}

pub(super) fn workspace_root() -> Result<std::path::PathBuf> {
    let root = crate::core::exec_ctx::current()
        .and_then(|ctx| {
            ctx.placement()
                .map(|placement| placement.workspace_root().to_path_buf())
        })
        .map(Ok)
        .unwrap_or_else(|| std::env::current_dir().context("reading plan workspace"))?;
    anyhow::ensure!(
        root.is_dir(),
        "plan workspace is unavailable: {}",
        root.display()
    );
    root.canonicalize().context("binding plan workspace")
}

fn transition(
    planning: bool,
    path: Option<&str>,
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&DesignContext>,
) -> Result<()> {
    let root = workspace_root()?;
    transition_in(
        planning,
        path,
        history,
        model,
        design,
        &root,
        session_store::current_session_slug(),
        || {
            if planning {
                settle_sidecars();
            }
        },
    )
}

fn settle_sidecars() {
    // Closing the manager starts physical shutdown; retained runner leases keep the transition
    // blocked until the stdio monitor and drains settle. A retry must not release them early.
    crate::agent::mcp::invalidate();
    crate::agent::lsp::LSP.suspend();
}

#[allow(clippy::too_many_arguments)] // Explicit workspace/session and teardown keep ingress testable.
fn transition_in(
    planning: bool,
    path: Option<&str>,
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&DesignContext>,
    root: &std::path::Path,
    saved_slug: Option<String>,
    settle: impl FnOnce(),
) -> Result<()> {
    let task =
        todo::bound_context().context("planning transition requires a conversation owner")?;
    let control = task.control();
    let prior = control.snapshot();
    let selected = if planning {
        Some(plan_control::select_target(
            root,
            path,
            prior.plan_target.as_ref(),
        )?)
    } else {
        None
    };
    transition_selected_in(
        planning, selected, history, model, design, root, saved_slug, settle, true,
    )
}

/// Trusted migration ingress supplies an already checked exact legacy target; never model DTO data.
/// No document observation is invented here. Migration performs its guarded write after persistence.
pub(super) fn transition_selected(
    selected: plan_control::SelectedPlan,
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&DesignContext>,
) -> Result<()> {
    let root = workspace_root()?;
    transition_selected_in(
        true,
        Some(selected),
        history,
        model,
        design,
        &root,
        session_store::current_session_slug(),
        settle_sidecars,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn transition_selected_in(
    planning: bool,
    selected: Option<plan_control::SelectedPlan>,
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&DesignContext>,
    root: &std::path::Path,
    saved_slug: Option<String>,
    settle: impl FnOnce(),
    refresh: bool,
) -> Result<()> {
    let task =
        todo::bound_context().context("planning transition requires a conversation owner")?;
    let control = task.control();
    let prior = control.snapshot();
    let target = selected
        .as_ref()
        .map(|s| s.target.clone())
        .or_else(|| prior.plan_target.clone());
    let slug = saved_slug
        .clone()
        .unwrap_or_else(|| session_store::allocate_session_slug(history));
    // Metadata and image projection happen before transition takes the owner mutex. In particular
    // no project-root Git discovery or current-policy lookup is allowed from its save callback.
    let meta = session_store::SessionMeta {
        project_key: Some(crate::core::config::normalize_path_key(root)),
        project_root: Some(root.to_string_lossy().into_owned()),
        model: Some(model.to_owned()),
        ..Default::default()
    };
    let design_state = design.map(DesignContext::snapshot);
    let prepared = session_store::prepare_control_transition(
        history,
        &slug,
        meta,
        design_state.as_ref(),
        &task.snapshot(),
        &prior,
        None,
    )
    .and_then(|prepared| {
        if saved_slug.is_none() {
            prepared.require_absent()
        } else {
            Ok(prepared)
        }
    });
    // Teardown retains admitted physical owners until settlement; transition below refuses while
    // any remain. A timeout or a second command must never release an active worker's lease early.
    settle();
    control.transition(
        if planning {
            ExecutionMode::Planning
        } else {
            ExecutionMode::Normal
        },
        target.clone(),
        |next| prepared?.save(next).map(|_| ()),
    )?;
    session_store::set_session_slug(Some(slug.clone()));
    // Selection of an existing contract may attach only after a checked same-ID disk observation.
    if let Some(selection) = selected.filter(|selection| selection.observation.is_some()) {
        let attachment = todo::PlanAttachment::load(
            std::path::Path::new(&selection.target.root),
            &selection.target.path,
        )?;
        anyhow::ensure!(
            attachment.id == selection.target.id,
            "plan changed during entry; retry /plan on explicitly"
        );
        task.attach_plan(attachment)?;
    }
    if refresh {
        prompt_lanes::PlanContextRefresh::default().refresh(0, &task, history)?;
    }
    let design_state = design.map(DesignContext::snapshot);
    session_store::update_live_history_with_design(history, design_state.as_ref());
    tui::emit_line(&format!(
        "Planning {} — session {slug}{}; task completion and implementation checks are unchanged.",
        if planning { "on" } else { "off" },
        target
            .map(|t| format!("; target {} ({})", t.path, t.id))
            .unwrap_or_default()
    ));
    Ok(())
}

fn status(history: &mut Vec<Message>) -> Result<()> {
    tui::emit_line(&status_text(history)?);
    Ok(())
}

fn status_text(history: &mut Vec<Message>) -> Result<String> {
    let task = todo::bound_context().context("planning status requires a conversation owner")?;
    let control = task.control();
    let saved = control.snapshot();
    let ready = control.policy().validate_current().is_ok();
    // Status remains available after failed persistence. Do not ask a blocked owner to admit a
    // document refresh or mutate its historical observations just to explain why work is stopped.
    let state = if ready {
        prompt_lanes::PlanContextRefresh::default().refresh(0, &task, history)?
    } else {
        task.snapshot()
    };
    let target = saved
        .plan_target
        .as_ref()
        .map(|t| format!("{} ({})", t.path, t.id))
        .unwrap_or_else(|| "none".into());
    let attachment = state
        .plan
        .as_ref()
        .map(|p| {
            format!(
                "{:?}; usable={}; validated at last refresh only",
                p.freshness,
                ready && p.usable()
            )
        })
        .unwrap_or_else(|| "no saved attachment (a selected target may still be unsaved)".into());
    let open = state
        .items
        .iter()
        .filter(|item| item.status != todo::Status::Done)
        .count();
    Ok(format!("Planning: {}{}\nTarget: {target}\nAttachment: {attachment}{}\nOutstanding task items: {open}; implementation obligations: {}",
        if saved.is_planning() { "on" } else { "off" }, if ready { "" } else { " (execution blocked: transition not durably saved)" },
        if ready { "" } else { "; historical snapshot only; refresh suppressed while blocked" }, state.obligations.len()))
}

#[cfg(test)]
#[path = "planning_control_tests.rs"]
mod planning_control_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_raw_anchored_requests_narrow_and_prose_never_widens() {
        for text in [
            "Plan only: inspect this repo",
            "Chỉ lên plan cho công việc này",
            "don't implement yet",
        ] {
            assert!(asks_plan_only(text), "{text}");
        }
        for text in [
            "now implement",
            "Read this quote: plan only",
            "\"plan only\"",
            "> plan only",
            "```\nplan only\n```",
            "Explain this:\nplan only",
            "plan onlyish",
        ] {
            assert!(!asks_plan_only(text), "{text}");
        }
    }
}
