//! REPL-owned workspace routing. No global root, process chdir, or mutable child placement.

use crate::core::exec_ctx::{self, ExecutionContext, WorkspacePlacement};
use anyhow::{Context, Result};
use std::future::Future;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct SessionWorkspace(Arc<Mutex<WorkspacePlacement>>);

tokio::task_local! {
    static SESSION: SessionWorkspace;
}

impl SessionWorkspace {
    pub(crate) fn new(context: &ExecutionContext) -> Result<Self> {
        Ok(Self(Arc::new(Mutex::new(
            context
                .workspace_snapshot()
                .context("interactive session requires an explicit workspace")?,
        ))))
    }

    fn snapshot(&self) -> WorkspacePlacement {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Bind the session's latest placement at the REPL boundary on each poll. Individual turns and
/// spawned jobs capture immutable contexts inside this boundary and never consult this handle.
pub(crate) async fn in_session<F: Future>(
    session: SessionWorkspace,
    context: ExecutionContext,
    future: F,
) -> F::Output {
    SESSION
        .scope(session.clone(), async move {
            let mut future = Box::pin(future);
            std::future::poll_fn(move |cx| {
                let context = context.with_workspace(&session.snapshot());
                exec_ctx::with_current(context, || future.as_mut().poll(cx))
            })
            .await
        })
        .await
}

/// Human-only navigation. It neither grants implementation permission nor executes a Git mutation.
/// All fallible preparation precedes activation; refusal keeps the original session placement.
pub(crate) fn focus(
    path: &str,
    history: &[crate::core::types::Message],
    design: Option<&crate::agent::design::DesignContext>,
) -> Result<ExecutionContext> {
    let result = focus_inner(path, history, design);
    if result.as_ref().is_err_and(|error| {
        crate::agent::tool_error_effect_state(error)
            != crate::agent::tools::ToolEffectState::Settled
    }) {
        crate::core::recovery::mark_effects_unsettled();
    }
    result
}

fn focus_inner(
    path: &str,
    history: &[crate::core::types::Message],
    design: Option<&crate::agent::design::DesignContext>,
) -> Result<ExecutionContext> {
    use crate::core::{
        git_ops,
        workspace_txn::{PhysicalEffectLease, WorkspaceRestoreGuard},
    };
    use std::{path::Path, time::Duration};
    let session = SESSION.try_with(Clone::clone).context(
        "focus requires an interactive REPL session; a one-shot CLI cannot change another session",
    )?;
    let original = exec_ctx::current().context("focus requires a bound session")?;
    anyhow::ensure!(
        original.can_restore_workspace() && original.native_scope().is_none(),
        "focus is available only to the unrestricted coordinating session"
    );
    let task =
        crate::agent::todo::bound_context().context("focus requires a conversation owner")?;
    anyhow::ensure!(
        crate::agent::hooks::automation_context(None).permits_automation(),
        "workspace focus is unavailable while execution is restricted"
    );
    let control = task.control();
    // Shutdown retains physical runner ownership until actual exit. A retry may proceed once settled.
    crate::agent::mcp::invalidate();
    crate::agent::lsp::LSP.suspend();
    // Refuse live foreground work before target metadata. Final activation closes this admission
    // again after the read-only Git runners have released their policies.
    drop(control.hold_workspace_transition()?);
    let placement = original
        .placement()
        .context("session workspace is missing")?;
    let prepared = git_ops::PreparedFocus::prepare(placement.workspace_root(), Path::new(path))?;
    let target = &prepared.target().canonical_root;
    if target == placement.workspace_root() {
        return Ok(original);
    }
    let next =
        original.with_placement(placement.fork_for(target.clone(), &original.resource_scope())?);
    let cancel = crate::core::cancel::current();
    let lease = prepared.hold()?;
    let registration = lease.registration();
    {
        let physical = PhysicalEffectLease::exclusive(
            Duration::ZERO,
            cancel.as_ref(),
            "session worktree focus",
            &original.resource_scope(),
        )?;
        for (identity, writer) in [
            (prepared.source(), lease.writer(false)),
            (prepared.target(), lease.writer(true)),
        ] {
            crate::core::native_journal::require_workspace_settled(&identity.canonical_root)?;
            crate::core::speculation::namespace::inspect_existing(
                identity.clone(),
                placement.home_root(),
            )?;
            crate::core::speculation::legacy::require_settled(
                &identity.canonical_root,
                writer,
                &physical,
            )?;
        }
    }
    // Do not hold the physical gate while Git inspection runners acquire their own body admission.
    for identity in [prepared.source(), prepared.target()] {
        git_ops::require_focus_settled(identity)?;
    }
    lease.revalidate(&prepared)?;
    let state = task.snapshot();
    let persistence = crate::core::session_store::PersistenceSnapshot::capture(&state)?;
    let (recovery, coop) = exec_ctx::with_current(next.clone(), || -> Result<_> {
        let mut recovery = crate::core::recovery::prepare_focus_scope(
            target.display().to_string(),
            crate::core::session_store::current_session_slug(),
        )?;
        // Save the carried conversation as historical state; focus invalidates evidence before
        // publication. Resume already treats historical visual/check evidence as non-live authority.
        let design_state = design.map(crate::agent::design::DesignContext::snapshot);
        recovery.save_conversation(history, design_state.as_ref(), &state, &persistence)?;
        let coop = crate::features::coop::prepare_focus_session(
            prepared.target(),
            crate::core::session_store::current_session_slug(),
            registration,
        )?;
        Ok((recovery, coop))
    })?;
    drop(persistence); // Its save admission must retire before closing foreground admission.
    lease.revalidate(&prepared)?;
    registration.verify()?;
    // No await or fallible host operation after this point: scopes move as one controller action.
    let _admission = control.hold_workspace_transition()?;
    let activity = WorkspaceRestoreGuard::acquire(&prepared.source().canonical_root)?;
    activity.require_identity_quiescent(prepared.target())?;
    recovery.revalidate()?;
    coop.revalidate()?;
    recovery.activate();
    coop.activate();
    let workspace = next
        .workspace_snapshot()
        .expect("focused placement is bound");
    *session.0.lock().unwrap_or_else(|e| e.into_inner()) = workspace.clone();
    exec_ctx::rebind_current_workspace(&workspace);
    task.invalidate_workspace_evidence();
    crate::ui::tui::set_input_workspace(target.clone());
    if let Some(design) = original.design() {
        design.begin_turn("workspace-focus", target.display().to_string());
    }
    crate::agent::codebase::invalidate_cache();
    Ok(next)
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod focus_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_placement::PlacementContext;

    #[tokio::test]
    async fn session_poll_routing_is_local_and_children_remain_immutable() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let a = fixture.path().join("a");
        let b = fixture.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let placement = PlacementContext::new(a.clone(), fixture.path().join("home")).unwrap();
        let original = ExecutionContext::default().with_placement(placement.clone());
        let child = original.with_resource_scope("child");
        let moved = original.with_placement(placement.fork_for(b.clone(), "focus").unwrap());
        let session = SessionWorkspace::new(&original).unwrap();
        let cwd = std::env::current_dir().unwrap();
        in_session(session.clone(), original.clone(), async {
            assert_eq!(crate::core::config::project_root(), a);
            *session.0.lock().unwrap() = moved.workspace_snapshot().unwrap();
            tokio::task::yield_now().await;
            assert_eq!(crate::core::config::project_root(), b);
            exec_ctx::with_current_future(child, async {
                tokio::task::yield_now().await;
                assert_eq!(crate::core::config::project_root(), a);
            })
            .await;
            assert_eq!(crate::core::config::project_root(), b);
        })
        .await;
        assert!(exec_ctx::current().is_none());
        assert_eq!(std::env::current_dir().unwrap(), cwd);
        assert_eq!(original.placement().unwrap().workspace_root(), a);
    }
}
