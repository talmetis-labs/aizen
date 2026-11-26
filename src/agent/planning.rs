//! Host-side planning admission. Model names/arguments never confer permission.
use super::tools::{PreparedToolCall, Tool};
use crate::core::exec_ctx::ExecutionContext;
use crate::core::session_control::ExecutionPolicy;
use anyhow::{ensure, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    NativeRead,
    TaskUpdate,
    PlanWrite,
}

/// Construction-time lookup: a frozen execution context takes precedence over its owner's
/// newer state. A failed transition yields an empty registry, never the normal surface.
pub(crate) fn current_policy() -> Option<ExecutionPolicy> {
    crate::core::exec_ctx::current()
        .and_then(|ctx| ctx.policy().cloned())
        .or_else(|| super::todo::bound_context().map(|task| task.control().policy()))
}

pub(crate) fn check_tool<T: Tool + ?Sized>(ctx: &ExecutionContext, tool: &T) -> Result<()> {
    let explicit = ctx.policy().cloned();
    let inherited = current_policy();
    if let (Some(outer), Some(inner)) = (&inherited, &explicit) {
        outer.check_inheritance(inner)?;
    }
    let policy = explicit.or(inherited);
    let Some(policy) = policy else { return Ok(()) };
    policy.validate_current()?;
    if policy.is_planning() {
        ensure!(
            ctx.policy().is_some(),
            "planning execution lost its frozen policy"
        );
        ensure!(
            policy.permits_tool(tool.name()),
            "tool is unavailable in planning mode"
        );
        ensure!(
            matches!(
                (tool.planning_capability(), tool.name()),
                (
                    Some(Capability::NativeRead),
                    "file_read" | "file_glob" | "search_files"
                ) | (Some(Capability::TaskUpdate), "todo_write")
                    | (Some(Capability::PlanWrite), "file_write")
            ),
            "tool has no audited planning capability"
        );
    }
    Ok(())
}

/// Last-resort admission for process/discovery boundaries outside model tool dispatch.
/// This does not replace caller-owned worker leases or captured policy across asynchronous work.
pub(crate) fn check_automation_boundary() -> Result<()> {
    if let Some(policy) = current_policy() {
        policy.validate_current()?;
        ensure!(
            !policy.is_planning(),
            "automatic effects and processes are unavailable in planning mode"
        );
    }
    Ok(())
}

/// Native entrypoints also defend direct calls that do not use the registry/executor wrapper.
/// An ambient restriction cannot be replaced with a missing or separately owned context.
pub(crate) fn check_direct<T: Tool + ?Sized>(
    tool: &T,
    explicit: Option<&ExecutionContext>,
) -> Result<()> {
    let ctx = explicit
        .cloned()
        .or_else(crate::core::exec_ctx::current)
        .unwrap_or_default();
    check_tool(&ctx, tool)
}

/// Direct synchronous native bodies retain admission until their physical operation settles.
pub(crate) fn admit_direct<T: Tool + ?Sized>(
    tool: &T,
    explicit: Option<&ExecutionContext>,
) -> Result<Option<ExecutionContext>> {
    let ctx = explicit
        .cloned()
        .or_else(crate::core::exec_ctx::current)
        .unwrap_or_default();
    check_tool(&ctx, tool)?;
    ctx.policy().map(|_| ctx.admit_policy()).transpose()
}

/// Called before preparation, which may otherwise discover integrations or allocate workspaces.
pub(crate) fn prepare<T: Tool + ?Sized>(
    tool: &T,
    args: &serde_json::Value,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    check_tool(ctx, tool)?;
    let mut prepared = tool.prepare_call(args, ctx)?;
    prepared.planning_policy = ctx.policy().cloned();
    prepared.workspace_binding = ctx.workspace_binding().cloned();
    Ok(prepared)
}

pub(crate) fn check_prepared(
    ctx: &ExecutionContext,
    tool: &dyn Tool,
    prepared: &PreparedToolCall,
) -> Result<()> {
    check_tool(ctx, tool)?;
    ctx.require_current_binding(prepared.workspace_binding.as_ref())?;
    if ctx.is_planning() && tool.name() == "file_write" {
        crate::agent::builtin::check_plan_write_prepared(ctx, prepared)?;
    }
    if let Some(frozen) = &prepared.planning_policy {
        let active = ctx
            .policy()
            .ok_or_else(|| anyhow::anyhow!("prepared policy lost its owner"))?;
        frozen.check_inheritance(active)?;
    } else {
        ensure!(
            !ctx.is_planning(),
            "planning operation was not prepared under this policy"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "planning_tests.rs"]
mod tests;
