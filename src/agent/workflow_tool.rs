//! `workflow` — the model-callable fan-out primitive (deterministic orchestration IN conversation).
//!
//! `aizen workflow` (workflow.rs) is CLI-only: the model can't invoke it, so multi-agent patterns had
//! to be narrated serially through `task`. This tool exposes the same bounded fan-out with three
//! modes, keeping control flow in CODE and content in the model (the workflows-over-agents rule):
//!
//! - `fanout`: run tasks concurrently (you decide how many), then one synthesis pass
//!   (mixture-of-agents). A task with `after` waits for those tasks and receives their reports,
//!   so a chain (implement → verify → review) is ONE call. Frozen, disjoint exact `write_paths`
//!   allow native editors to overlap. Same-file, shell/build and unrestricted dispatches remain
//!   barriers; `after` orders dependencies. The parent consolidates and verifies actual edits.
//!   Admission limits how many run AT ONCE (see `task_tool::max_parallel_subagents_pub`).
//! - `implement`: that chain prebuilt from one `prompt` — daedalus, then themis with a
//!   `VERDICT: PASS|FAIL` contract and one fix loop back to daedalus, then nemesis.
//! - `verify`: the adversarial-refuter preset — each finding gets a read-only reviewer explicitly
//!   prompted to REFUTE it (industrially measured at ~0.93 accuracy filtering false positives).
//!   No synthesis: the per-finding verdicts return raw.
//!
//! Registered by default at depth 0 so a fresh install always has a real fan-out primitive. Users
//! who prefer the smaller top-level schema can opt out with `workflow_tool: false`. Depth 0 only,
//! like `task`; the tool itself is not concurrency-safe — it IS the parallelism.

use crate::agent::tools::{PreparedToolCall, Tool, ToolObservation};
#[cfg(test)]
use crate::agent::workflow::task_is_writer;
use crate::agent::workflow::{
    enforce_singular_writer, run_workflow_collect_observation, Synthesis, WorkflowSpec,
    WorkflowTask,
};
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::PathBuf;

pub struct WorkflowTool {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    approval_mode: crate::core::approval::ApprovalMode,
    depth: usize,
    root: PathBuf,
    context_window: usize,
}

#[derive(Clone)]
struct PreparedWorkflowDispatch {
    spec: WorkflowSpec,
    synthesize: bool,
    read_only: bool,
    parent_ctx: crate::core::exec_ctx::ExecutionContext,
}

impl WorkflowTool {
    #[expect(
        clippy::too_many_arguments,
        reason = "Constructor captures the existing endpoint, approval, scope and budget interface without a warning-only API redesign"
    )]
    pub fn new(
        client: reqwest::Client,
        base_url: String,
        api_key: String,
        model: String,
        approval_mode: crate::core::approval::ApprovalMode,
        depth: usize,
        root: PathBuf,
        context_window: usize,
    ) -> Self {
        Self {
            client,
            base_url,
            api_key,
            model,
            approval_mode,
            depth,
            root,
            context_window,
        }
    }
}

/// The refuter template for `verify` mode: adversarial framing + a fixed verdict contract.
fn refuter_prompt(finding: &str) -> String {
    format!(
        "Adversarially try to REFUTE the following finding. Read the cited code yourself; do not \
         take the claim at face value. Reply with exactly one line `verdict: confirmed` or \
         `verdict: refuted` or `verdict: uncertain`, then your evidence with file:line.\n\nFINDING:\n{finding}"
    )
}

/// The `implement` preset: daedalus makes the change, themis verifies it (report opening with
/// `VERDICT: PASS|FAIL`; a FAIL re-dispatches daedalus once with the failure), nemesis reviews
/// the result. One call instead of three hand-chained turns, each re-briefed through the
/// parent. The same spec as a file: `bench-fixtures/workflows/implement.json`.
pub(crate) fn implement_spec(prompt: &str) -> WorkflowSpec {
    let task =
        |id: &str, role: &str, after: &[&str], retry: Option<&str>, brief: String| WorkflowTask {
            id: id.to_string(),
            role: role.to_string(),
            prompt: brief,
            after: after.iter().map(|s| s.to_string()).collect(),
            retry_on_fail: retry.map(str::to_string),
            ..Default::default()
        };
    let mut implement = task("implement", "daedalus", &[], None, prompt.to_string());
    implement.expected_output =
        Some("What changed, with file:line, and the build or test command you ran.".into());
    let mut verify = task(
        "verify",
        "themis",
        &["implement"],
        Some("implement"),
        format!(
            "Verify the change reported above, made for this request: {prompt}\n\nRun the \
             project's fast check and the narrowest tests that cover it (git_inspect diff shows \
             what changed). Your FIRST line must be exactly `VERDICT: PASS` or `VERDICT: FAIL` \
             (`VERDICT: INCONCLUSIVE` only if nothing could run), then the exact commands, exit \
             codes and the decisive failing output with file:line."
        ),
    );
    verify.boundaries = Some("Do not edit files.".into());
    let review = task(
        "review",
        "nemesis",
        &["verify"],
        None,
        format!(
            "Review the change made for this request: {prompt}\n\nUse git_inspect diff for the \
             actual change and the verify report above for its test status. Findings with \
             severity and file:line, then one line: safe to merge, or not, and why."
        ),
    );
    WorkflowSpec {
        name: "implement".into(),
        tasks: vec![implement, verify, review],
        synthesis: Some(Synthesis {
            model: None,
            prompt: Some(
                "Report in order: what was implemented (implement), the test verdict with its \
                 evidence (verify — if it failed after the retry, say so first), and the review \
                 findings (review)."
                    .into(),
            ),
        }),
        prepared_synthesis: None,
    }
}

/// Build the spec for one call. Pure for role-only tasks; a task naming an `agent` resolves that
/// specialist from disk to classify its write-capability (see [`task_is_writer`]).
pub(crate) fn build_spec(args: &Value) -> Result<(WorkflowSpec, bool)> {
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("fanout");
    match mode {
        "fanout" => {
            let tasks_in = args
                .get("tasks")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| anyhow::anyhow!("fanout mode requires a non-empty 'tasks' array"))?;
            if tasks_in.len() > 32 {
                bail!(
                    "workflow caps at 32 tasks per call (got {})",
                    tasks_in.len()
                );
            }
            let mut tasks = Vec::new();
            for (i, t) in tasks_in.iter().enumerate() {
                let role = t
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("nemesis")
                    .to_string();
                // An unknown role is refused at spec build, not silently run read-only — same
                // discipline as the `task` tool's role guard.
                if crate::agent::roles::canonical(&role).is_none() {
                    bail!(crate::agent::task_tool::unknown_role_error(&role));
                }
                let opt_str = |k: &str| {
                    t.get(k)
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                };
                tasks.push(WorkflowTask {
                    prepared: None,
                    prepared_metis: None,
                    attempt: None,
                    overlay: None,
                    execution: crate::agent::speculation::ExecutionMode::from_args(t)?,
                    id: t
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("t{}", i + 1)),
                    role,
                    agent: t.get("agent").and_then(|v| v.as_str()).map(str::to_string),
                    prompt: {
                        let p = t
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                            .ok_or_else(|| {
                                anyhow::anyhow!("task #{} is missing 'prompt'", i + 1)
                            })?;
                        // Same bar as a `task` dispatch: a child cannot work from a line.
                        if let Some(why) = crate::agent::task_tool::thin_brief(p) {
                            bail!("task #{}: {why}", i + 1);
                        }
                        p.to_string()
                    },
                    model: t.get("model").and_then(|v| v.as_str()).map(str::to_string),
                    boundaries: opt_str("boundaries"),
                    write_paths: crate::agent::workflow::parse_workflow_paths(t.get("write_paths"), "write_paths")?,
                    read_paths: crate::agent::workflow::parse_workflow_paths(t.get("read_paths"), "read_paths")?,
                    expected_output: opt_str("expected_output"),
                    max_steps: t
                        .get("max_steps")
                        .and_then(|v| v.as_u64())
                        .map(|n| n as usize),
                    expects: t.get("expects").filter(|v| v.is_object()).cloned(),
                    context: {
                        let c = crate::agent::context_pack::findings_from_args(t);
                        (!c.is_empty()).then_some(c)
                    },
                    after: match t.get("after") {
                        Some(Value::Array(a)) => a
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        Some(Value::String(s)) => s
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        _ => Vec::new(),
                    },
                    retry_on_fail: opt_str("retry_on_fail"),
                    reference_ids: match t.get("reference_ids") {
                        None => Vec::new(),
                        Some(Value::Array(ids)) if ids.len() <= 4 => {
                            let ids = ids.iter().map(|id| id.as_str().map(str::to_owned)
                                .ok_or_else(|| anyhow::anyhow!("reference_ids requires scoped reference IDs")))
                                .collect::<Result<Vec<_>>>()?;
                            crate::core::reference_media::validate_reference_ids(&ids)?;
                            ids
                        }
                        _ => bail!("reference_ids requires up to four unique scoped reference IDs"),
                    },
                    return_evidence: match t.get("return_evidence") {
                        None => Vec::new(),
                        Some(Value::Array(ids)) if ids.len() <= 4 => ids.iter().map(|id| {
                            let id = id.as_str().filter(|s| !s.is_empty() && s.len() <= 128)
                                .ok_or_else(|| anyhow::anyhow!("return_evidence requires up to four bounded evidence IDs"))?;
                            Ok(id.to_string())
                        }).collect::<Result<Vec<_>>>()?,
                        _ => bail!("return_evidence requires up to four evidence IDs"),
                    },
                });
            }
            // Singular-writer invariant — shared with CLI `run_workflow` via
            // `workflow::enforce_singular_writer` so the two paths cannot drift.
            let synthesis = args
                .get("synthesis")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|p| Synthesis {
                    model: None,
                    prompt: Some(p.to_string()),
                });
            let spec = WorkflowSpec {
                name: "fanout".into(),
                tasks,
                synthesis,
                prepared_synthesis: None,
            };
            // Validate the graph before resolving scopes; writer admission is checked again
            // after preparation freezes the exact native resources.
            crate::agent::workflow::waves(&spec.tasks)?;
            if !crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED
                || spec
                    .tasks
                    .iter()
                    .filter(|task| {
                        crate::agent::workflow::task_is_writer(&task.role, task.agent.as_deref())
                    })
                    .any(|task| task.write_paths.is_none() && task.read_paths.is_none())
            {
                enforce_singular_writer(&spec)?;
            }
            Ok((spec, true))
        }
        "implement" => {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("implement mode requires a non-empty 'prompt'"))?;
            if let Some(why) = crate::agent::task_tool::thin_brief(prompt) {
                bail!("implement: {why}");
            }
            Ok((implement_spec(prompt), true))
        }
        "verify" => {
            let findings = args
                .get("findings")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("verify mode requires a non-empty 'findings' array")
                })?;
            if findings.len() > 32 {
                bail!(
                    "verify caps at 32 findings per call (got {})",
                    findings.len()
                );
            }
            let tasks = findings
                .iter()
                .enumerate()
                .filter_map(|(i, f)| f.as_str().map(|s| (i, s)))
                .map(|(i, f)| WorkflowTask {
                    id: format!("refute-{}", i + 1),
                    role: "nemesis".to_string(), // read-only → these fan out safely
                    agent: None,
                    prompt: refuter_prompt(f),
                    model: None,
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            if tasks.is_empty() {
                bail!("verify mode: 'findings' must be strings");
            }
            // No synthesis: per-finding verdicts return raw (a merge would launder the evidence).
            Ok((
                WorkflowSpec {
                    name: "verify".into(),
                    tasks,
                    synthesis: None,
                    prepared_synthesis: None,
                },
                false,
            ))
        }
        other => bail!("unknown workflow mode '{other}' (use fanout, implement or verify)"),
    }
}

fn workflow_has_no_outward_effects(spec: &WorkflowSpec) -> bool {
    crate::agent::workflow::workflow_spec_synthesis_text_only(spec)
        && spec.tasks.iter().all(|task| {
            task.reference_ids.is_empty()
                && task.return_evidence.is_empty()
                && task.execution == crate::agent::speculation::ExecutionMode::Direct
                && crate::agent::workflow::workflow_task_text_only_effect_free(task)
        })
}

impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }
    fn description(&self) -> &str {
        "Fanout tasks; after passes reports. Native exact scopes overlap; opaque work serializes. \
         Parent verifies. implement: daedalus→themis→nemesis + one FAIL retry; \
         verify: read-only refuter. One child→task; no restore/undo."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["fanout", "implement", "verify"], "description": "fanout=tasks; implement=prompt; verify=findings"},
                "prompt": {"type": "string"},
                "tasks": {"type": "array", "maxItems": 32, "items": {"type": "object", "properties": {
                    "id": {"type": "string"},
                    "prompt": {"type": "string"},
                    "role": {"type": "string", "enum": ["argus", "metis", "daedalus", "nemesis", "themis", "clio", "mnemosyne"], "description": "nemesis=read(default); daedalus=edit; themis=test"},
                    "agent": {"type": "string", "description": "<agents> slug"},
                    "model": {"type": "string"},
                    "execution": {"type":"string", "enum":["direct", "speculative"], "description":"Native text files only; private same-base edits, parent integrates/verifies; no hooks/shell/media."},
                    "reference_ids": {"type":"array","maxItems":4,"uniqueItems":true,"items":{"type":"string","maxLength":128},"description":"Conversation refs; live grant."},
                    "return_evidence": {"type":"array","maxItems":4,"items":{"type":"string","maxLength":128}},
                    "boundaries": {"type": "string"},
                    "read_paths": {"type": "array", "minItems": 1, "maxItems": 32, "uniqueItems": true, "items": {"type": "string", "minLength": 1, "maxLength": 4096}, "description":"reads; without write_paths read-only; union <=32 native files"},
                    "write_paths": {"type": "array", "minItems": 1, "maxItems": 32, "uniqueItems": true, "items": {"type": "string", "minLength": 1, "maxLength": 4096}, "description":"read/write union; parent integrates"},
                    "expected_output": {"type": "string"},
                    "context": {"type": "array", "items": {"type": "string"}, "description":"known findings, <=10 lines"},
                    "max_steps": {"type": "integer", "description": "steps; cap 80"},
                    "expects": {"type": "object", "description": "JSON-only answer schema"},
                    "after": {"type": "array", "items": {"type": "string"}, "description":"prerequisite task IDs"},
                    "retry_on_fail": {"type": "string", "description":"after ID: retry+rereview on VERDICT: FAIL"}
                }, "required": ["prompt"], "additionalProperties": false}},
                "synthesis": {"type": "string"},
                "findings": {"type": "array", "maxItems": 32, "items": {"type": "string"}, "description":"verify claims: file:line"}
            },
            "required": ["mode"],
            "additionalProperties": false
        })
    }
    fn result_is_error(&self, result: &str) -> Option<bool> {
        Some(result.contains("[Workflow incomplete:") || result.starts_with("error:"))
    }
    /// Unknown/argument-only workflows remain barriers. Only frozen text-only effect-free readers
    /// may join an outer parallel batch, and only after the Phase C validation gate is enabled.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_destructive_prepared(&self, call: &PreparedToolCall) -> bool {
        self.delegation_read_only_prepared(call) != Some(true)
    }
    fn is_concurrency_safe_prepared(&self, call: &PreparedToolCall) -> bool {
        crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED
            && call
                .delegation
                .as_ref()
                .and_then(|dispatch| dispatch.downcast_ref::<PreparedWorkflowDispatch>())
                .is_some_and(|dispatch| {
                    dispatch.read_only && workflow_has_no_outward_effects(&dispatch.spec)
                })
    }
    fn plan_call(
        &self,
        args: &Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        self.prepare_call(args, ctx)
    }
    fn prepare_call(
        &self,
        args: &Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        if self.depth >= 1 {
            bail!(
                "workflow is depth-capped at 1 — a sub-agent cannot orchestrate further fan-outs"
            );
        }
        let (mut spec, synthesize) = build_spec(args)?;
        crate::agent::workflow::prepare_workflow_spec_in_context(
            &mut spec,
            &self.root,
            &self.base_url,
            &self.api_key,
            &self.model,
            ctx,
        )?;
        let read_only = spec
            .tasks
            .iter()
            .all(crate::agent::workflow::workflow_task_read_only);
        let mut call = PreparedToolCall::new(args.clone());
        let changes = std::sync::Arc::new(crate::core::exec_ctx::WorkspaceChanges::default());
        call.workspace_changes = Some(changes.clone());
        call.delegation = Some(std::sync::Arc::new(PreparedWorkflowDispatch {
            spec,
            synthesize,
            read_only,
            parent_ctx: ctx.with_workspace_changes(changes),
        }));
        Ok(call)
    }
    fn delegation_read_only_prepared(&self, call: &PreparedToolCall) -> Option<bool> {
        call.delegation
            .as_ref()?
            .downcast_ref::<PreparedWorkflowDispatch>()
            .map(|d| d.read_only)
    }
    fn workspace_effect_prepared(&self, call: &PreparedToolCall) -> super::tools::WorkspaceEffect {
        match self.delegation_read_only_prepared(call) {
            Some(false) => super::tools::WorkspaceEffect::OpaqueWorkspace,
            _ => super::tools::WorkspaceEffect::None,
        }
    }
    fn workspace_target_prepared(&self, call: &PreparedToolCall) -> Option<PathBuf> {
        self.workspace_effect_prepared(call)
            .needs_checkpoint()
            .then(|| self.root.clone())
    }
    fn execute_prepared(&self, call: &PreparedToolCall) -> Result<ToolObservation> {
        let dispatch = call
            .delegation
            .as_ref()
            .and_then(|d| d.downcast_ref::<PreparedWorkflowDispatch>())
            .ok_or_else(|| anyhow::anyhow!("workflow requires its internally prepared dispatch"))?;
        self.execute_dispatch(dispatch)
    }
    fn recovery_effect(&self, _args: &Value) -> bool {
        true
    }
    fn may_return_images(&self, args: &Value) -> bool {
        args.get("tasks")
            .and_then(Value::as_array)
            .is_some_and(|tasks| {
                tasks.iter().any(|t| {
                    t.get("return_evidence")
                        .and_then(Value::as_array)
                        .is_some_and(|ids| !ids.is_empty())
                })
            })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        self.execute_observation(args)
            .map(|observation| observation.text)
    }
    fn execute_observation(&self, args: &Value) -> Result<ToolObservation> {
        let ctx = crate::core::exec_ctx::current().unwrap_or_default();
        let call = self.prepare_call(args, &ctx)?;
        self.execute_prepared(&call)
    }
}

impl WorkflowTool {
    fn execute_dispatch(&self, dispatch: &PreparedWorkflowDispatch) -> Result<ToolObservation> {
        let spec = &dispatch.spec;
        let synthesize = dispatch.synthesize;
        // Each physical attempt is admitted by the scheduler, including its model quota.
        // The workflow wrapper does not reserve an unbudgeted batch of child slots.
        let client = self.client.clone();
        let base = self.base_url.clone();
        let key = self.api_key.clone();
        let model = self.model.clone();
        let approval = self.approval_mode;
        let cancel = crate::core::cancel::current().unwrap_or_default();
        // Keep the actual preparation carrier through the async driver; driver TLS cannot narrow or
        // silently add authority to this already-approved workflow.
        let exec_ctx = dispatch.parent_ctx.clone();
        finish_workflow_dispatch(tokio::task::block_in_place(|| {
            crate::core::cancel::with_current(cancel, || {
                crate::core::exec_ctx::with_current(exec_ctx, || {
                    // Every child and synthesis captures its own wire effort without touching a
                    // process-global override shared with concurrent runs.
                    tokio::runtime::Handle::current().block_on(run_workflow_collect_observation(
                        &client,
                        &base,
                        &key,
                        &model,
                        approval,
                        spec,
                        synthesize,
                        &self.root,
                        self.context_window,
                    ))
                })
            })
        }))
    }
}

/// Never turn unsettled physical effects into an ordinary partial observation. The driver keeps
/// completed summaries as error context, while the marker remains available to central classification.
fn finish_workflow_dispatch(result: Result<ToolObservation>) -> Result<ToolObservation> {
    match result {
        Err(error)
            if crate::agent::tool_error_effect_state(&error)
                != crate::agent::tools::ToolEffectState::Settled =>
        {
            Err(error.context(
                "Workflow incomplete: physical child effects are unsettled; retained child reports",
            ))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_workflow_uncertainty_remains_typed_with_completed_reports() {
        let retained = "── completed-child ──\nretained completed child summary";
        let error =
            anyhow::Error::new(crate::agent::task_tool::TaskWorkerStillRunning).context(retained);
        let error = finish_workflow_dispatch(Err(error)).unwrap_err();
        assert!(error.is::<crate::agent::task_tool::TaskWorkerStillRunning>());
        let report = format!("{error:#}");
        assert!(report.contains(retained));
        assert!(report.contains("physical child effects are unsettled"));
        let partial = anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(
            crate::agent::tools::ToolEffectState::PossiblyApplied(Some(42)),
        ))
        .context(retained);
        let partial = finish_workflow_dispatch(Err(partial)).unwrap_err();
        assert_eq!(
            crate::agent::tool_error_effect_state(&partial),
            crate::agent::tools::ToolEffectState::PossiblyApplied(Some(42))
        );
        assert!(format!("{partial:#}").contains(retained));
        let observation =
            finish_workflow_dispatch(Ok(ToolObservation::from("completed".to_owned()))).unwrap();
        assert_eq!(observation.text, "completed");
        let ordinary =
            finish_workflow_dispatch(Err(anyhow::anyhow!("ordinary preparation refusal")))
                .unwrap_err();
        assert!(!ordinary.is::<crate::agent::task_tool::TaskWorkerStillRunning>());
    }

    #[test]
    fn outer_unknown_workflow_is_neither_safe_nor_a_hidden_reader() {
        let tool = WorkflowTool::new(
            reqwest::Client::new(),
            "https://unused.example".into(),
            "unused".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            0,
            std::env::temp_dir(),
            0,
        );
        let call = PreparedToolCall::new(
            serde_json::json!({"mode": "verify", "findings": ["reader-looking JSON is not authority"]}),
        );
        assert!(!tool.is_concurrency_safe_prepared(&call));
        assert!(tool.is_destructive_prepared(&call));
        assert!(tool.parallel_write_keys_prepared(&call).is_none());
    }

    #[test]
    fn incomplete_workflow_observations_are_explicit_tool_failures() {
        let tool = WorkflowTool::new(
            reqwest::Client::new(),
            "https://unused.example".into(),
            "unused".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            0,
            std::env::temp_dir(),
            0,
        );
        for body in [
            "raw partial report\n[Workflow incomplete: cancelled; reports retained]",
            "merged partial report\n[Workflow incomplete: tasks unresolved]",
            "error: upstream failed",
        ] {
            assert_eq!(tool.result_is_error(body), Some(true));
        }
        assert_eq!(
            tool.result_is_error("VERDICT: PASS\nscoped findings"),
            Some(false)
        );
    }

    #[test]
    fn verify_mode_builds_refuter_tasks_no_synthesis() {
        let (spec, synth) = build_spec(&serde_json::json!({
            "mode": "verify",
            "findings": ["off-by-one in src/a.rs:10", "race in src/b.rs:99"]
        }))
        .unwrap();
        assert!(
            !synth,
            "verify returns raw verdicts, never a merged narrative"
        );
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.tasks[0].id, "refute-1");
        assert_eq!(
            spec.tasks[0].role, "nemesis",
            "refuters are read-only → they fan out"
        );
        assert!(spec.tasks[0].prompt.contains("REFUTE"));
        assert!(spec.tasks[0].prompt.contains("off-by-one in src/a.rs:10"));
        assert!(
            spec.tasks[1].prompt.contains("verdict: confirmed"),
            "fixed verdict contract"
        );
    }

    #[test]
    fn fanout_parses_contract_fields_and_refuses_unknown_roles() {
        // The per-task contract fields ride into the spec verbatim — full propagation is proven
        // downstream (run_one_task builds the same TaskContract the `task` tool does).
        let (spec, _) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [{
                "id": "review-auth",
                "role": "nemesis",
                "prompt": "review the auth changes in src/auth.rs",
                "boundaries": "Do not edit files",
                "expected_output": "Findings with severity and file:line",
                "max_steps": 12,
                "expects": {"type": "object", "required": ["verdict"]}
            }]
        }))
        .unwrap();
        let t = &spec.tasks[0];
        assert_eq!(t.boundaries.as_deref(), Some("Do not edit files"));
        assert_eq!(
            t.expected_output.as_deref(),
            Some("Findings with severity and file:line")
        );
        assert_eq!(t.max_steps, Some(12));
        assert!(t.expects.as_ref().unwrap().is_object());
        // An unknown role is refused at spec build with the real list — never run read-only.
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [{"prompt": "x", "role": "wizard"}]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown role"), "{err}");
        assert!(err.contains("argus"), "the real list is named: {err}");
    }

    #[test]
    fn fanout_write_paths_are_bounded_exact_strings_and_owned_by_the_spec() {
        let mut args = serde_json::json!({"mode": "fanout", "tasks": [{
            "role": "daedalus", "prompt": "edit the parser implementation in src/parser.rs", "write_paths": ["src/parser.rs"]
        }]});
        let (spec, _) = build_spec(&args).unwrap();
        args["tasks"][0]["write_paths"] = serde_json::json!(["outside.rs"]);
        assert_eq!(
            spec.tasks[0].write_paths.as_deref(),
            Some(&["src/parser.rs".to_owned()][..])
        );
        for invalid in [
            serde_json::json!(null),
            serde_json::json!("a.rs"),
            serde_json::json!([]),
            serde_json::json!([true]),
            serde_json::json!([""]),
            serde_json::json!([" a.rs"]),
            serde_json::json!(["a".repeat(4097)]),
            serde_json::json!((0..33).map(|n| format!("{n}.rs")).collect::<Vec<_>>()),
        ] {
            args["tasks"][0]["write_paths"] = invalid;
            assert!(build_spec(&args).is_err());
        }
        args["tasks"][0]["write_paths"] =
            serde_json::json!((0..32).map(|n| format!("{n}.rs")).collect::<Vec<_>>());
        assert_eq!(
            build_spec(&args).unwrap().0.tasks[0]
                .write_paths
                .as_ref()
                .unwrap()
                .len(),
            32
        );
    }

    #[test]
    fn scoped_fanout_still_refuses_parallel_writers() {
        if crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED {
            // Preparation, not the declaration parser, proves disjoint filesystem authority.
            assert!(build_spec(&serde_json::json!({"mode": "fanout", "tasks": [
                {"role": "daedalus", "prompt": "edit the first parser implementation in a.rs", "write_paths": ["a.rs"]},
                {"role": "daedalus", "prompt": "edit the second parser implementation in b.rs", "write_paths": ["b.rs"]}
            ]})).is_ok());
            return;
        }
        let err = build_spec(&serde_json::json!({"mode": "fanout", "tasks": [
            {"role": "daedalus", "prompt": "edit the first parser implementation in a.rs", "write_paths": ["a.rs"]},
            {"role": "daedalus", "prompt": "edit the second parser implementation in b.rs", "write_paths": ["b.rs"]}
        ]})).unwrap_err().to_string();
        assert!(err.contains("write-capable"), "{err}");
    }

    #[test]
    fn fanout_rejects_parallel_writers() {
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "edit src/a.rs", "role": "coder"},
                {"prompt": "edit src/b.rs", "role": "coder"}
            ]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("write-capable"), "{err}");
        // One coder + readers is fine.
        let (spec, synth) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "edit src/a.rs", "role": "coder"},
                {"prompt": "review src/b.rs", "role": "reviewer"}
            ]
        }))
        .unwrap();
        assert!(synth);
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.tasks[0].id, "t1", "ids default positionally");
    }

    #[test]
    fn spec_validation_rejects_junk() {
        assert!(
            build_spec(&serde_json::json!({"mode": "fanout"})).is_err(),
            "no tasks"
        );
        assert!(
            build_spec(&serde_json::json!({"mode": "verify"})).is_err(),
            "no findings"
        );
        assert!(
            build_spec(&serde_json::json!({"mode": "dag"})).is_err(),
            "unknown mode"
        );
        // The per-call cap is now 32 (the model requests what the work needs; concurrent WIDTH is
        // bounded separately by the machine-derived gate). A batch under the cap is accepted…
        let six: Vec<_> = (0..6)
            .map(|i| serde_json::json!({"prompt": format!("review src/t{i}.rs"), "role": "reviewer"}))
            .collect();
        assert!(
            build_spec(&serde_json::json!({"mode": "fanout", "tasks": six})).is_ok(),
            "6 read-only tasks are under the 32 cap → accepted"
        );
        // …and only an absurd batch past the disaster-stop cap is rejected.
        let too_many: Vec<_> = (0..33)
            .map(|i| serde_json::json!({"prompt": format!("review src/t{i}.rs"), "role": "reviewer"}))
            .collect();
        assert!(
            build_spec(&serde_json::json!({"mode": "fanout", "tasks": too_many})).is_err(),
            "cap 32"
        );
    }

    #[test]
    fn task_is_writer_classifies_by_capability() {
        // Bare roles: coder and tester write (file_edit / shell_run); planner and reviewer read.
        assert!(task_is_writer("coder", None));
        assert!(task_is_writer("tester", None));
        assert!(!task_is_writer("reviewer", None));
        assert!(!task_is_writer("planner", None));
        // An unresolvable agent slug falls back to coder (write) scope at run time → count as writer.
        assert!(task_is_writer("reviewer", Some("__no_such_agent__")));
    }

    #[test]
    fn implement_mode_builds_the_chain_with_one_fix_loop() {
        let (spec, synth) = build_spec(&serde_json::json!({
            "mode": "implement",
            "prompt": "add parse_pairs to src/lib.rs"
        }))
        .unwrap();
        assert!(synth);
        let ids: Vec<&str> = spec.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["implement", "verify", "review"]);
        let roles: Vec<&str> = spec.tasks.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(roles, ["daedalus", "themis", "nemesis"]);
        assert_eq!(spec.tasks[1].after, ["implement"]);
        assert_eq!(spec.tasks[1].retry_on_fail.as_deref(), Some("implement"));
        assert_eq!(spec.tasks[2].after, ["verify"]);
        assert!(
            spec.tasks[1].prompt.contains("VERDICT: PASS"),
            "verdict contract"
        );
        assert!(spec.tasks[0].prompt.contains("add parse_pairs"));
        // Two writers (daedalus, themis) — legal because `after` puts them in different waves.
        enforce_singular_writer(&spec).unwrap();
        assert!(
            build_spec(&serde_json::json!({"mode": "implement"})).is_err(),
            "a prompt is required"
        );
    }

    #[test]
    fn fanout_parses_after_as_array_or_string_and_refuses_bad_chains() {
        let (spec, _) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"id": "a", "prompt": "scan src/x.rs", "role": "argus"},
                {"id": "b", "prompt": "scan src/x.rs", "role": "clio"},
                {"id": "c", "prompt": "scan src/x.rs", "after": ["a", " b "]},
                {"id": "d", "prompt": "scan src/x.rs", "after": "a, c"}
            ]
        }))
        .unwrap();
        assert_eq!(spec.tasks[2].after, ["a", "b"]);
        assert_eq!(spec.tasks[3].after, ["a", "c"]);
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [{"id": "a", "prompt": "scan src/x.rs", "after": ["nope"]}]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown task 'nope'"), "{err}");
    }

    #[test]
    fn thin_task_briefs_are_refused_in_fanout_and_implement() {
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [{"id": "a", "prompt": "review it", "role": "nemesis"}]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("task #1: brief too thin"), "{err}");
        let err = build_spec(&serde_json::json!({"mode": "implement", "prompt": "fix it"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("implement: brief too thin"), "{err}");
        assert!(build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [{"id": "a", "prompt": "review src/parser.rs for off-by-one errors"}]
        }))
        .is_ok());
    }

    #[test]
    fn fanout_tasks_carry_context_findings() {
        let (spec, _) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "review src/a.rs", "context": ["- parser is in a.rs", "  ", "b"]},
                {"prompt": "plan src/b.rs"}
            ]
        }))
        .unwrap();
        assert_eq!(
            spec.tasks[0].context.as_deref(),
            Some(&["parser is in a.rs".to_string(), "b".to_string()][..])
        );
        assert!(spec.tasks[1].context.is_none(), "absent stays absent");
    }

    #[test]
    fn fanout_allows_two_readers() {
        // Two read-only tasks are NOT writers → they fan out freely (the invariant is one WRITER).
        let (spec, _) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "review src/a.rs", "role": "reviewer"},
                {"prompt": "plan src/b.rs", "role": "planner"}
            ]
        }))
        .unwrap();
        assert_eq!(spec.tasks.len(), 2);
    }

    #[test]
    fn depth_guard_refuses_nested_fanout() {
        let t = WorkflowTool::new(
            reqwest::Client::new(),
            "http://x".into(),
            "k".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            1,
            std::path::PathBuf::from("."),
            0,
        );
        let err = t
            .execute(&serde_json::json!({"mode": "verify", "findings": ["x"]}))
            .unwrap_err();
        assert!(err.to_string().contains("depth-capped"), "{err}");
    }
}
