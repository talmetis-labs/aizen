//! `workflow` — the model-callable fan-out primitive (deterministic orchestration IN conversation).
//!
//! `aizen workflow` (workflow.rs) is CLI-only: the model can't invoke it, so multi-agent patterns had
//! to be narrated serially through `task`. This tool exposes the same bounded fan-out with three
//! modes, keeping control flow in CODE and content in the model (the workflows-over-agents rule):
//!
//! - `fanout`: run tasks concurrently (you decide how many), then one synthesis pass
//!   (mixture-of-agents). A task with `after` waits for those tasks and receives their reports,
//!   so a chain (implement → verify → review) is ONE call. At most ONE writer per wave — parallel
//!   writers in one repo race edits and build locks; the fan-out is for READS, and two writers
//!   are legal only when `after` orders them (see `workflow::schedule`). The gate limits how many
//!   run AT ONCE (machine-derived, see `task_tool::max_parallel_subagents_pub`).
//! - `implement`: that chain prebuilt from one `prompt` — daedalus, then themis with a
//!   `VERDICT: PASS|FAIL` contract and one fix loop back to daedalus, then nemesis.
//! - `verify`: the adversarial-refuter preset — each finding gets a read-only reviewer explicitly
//!   prompted to REFUTE it (industrially measured at ~0.93 accuracy filtering false positives).
//!   No synthesis: the per-finding verdicts return raw.
//!
//! Registered by default at depth 0 so a fresh install always has a real fan-out primitive. Users
//! who prefer the smaller top-level schema can opt out with `workflow_tool: false`. Depth 0 only,
//! like `task`; the tool itself is not concurrency-safe — it IS the parallelism.

use crate::agent::tools::Tool;
#[cfg(test)]
use crate::agent::workflow::task_is_writer;
use crate::agent::workflow::{
    enforce_singular_writer, run_workflow_collect, Synthesis, WorkflowSpec, WorkflowTask,
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

impl WorkflowTool {
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
                    id: t
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("t{}", i + 1)),
                    role,
                    agent: t.get("agent").and_then(|v| v.as_str()).map(str::to_string),
                    prompt: t
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| anyhow::anyhow!("task #{} is missing 'prompt'", i + 1))?
                        .to_string(),
                    model: t.get("model").and_then(|v| v.as_str()).map(str::to_string),
                    boundaries: opt_str("boundaries"),
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
            };
            enforce_singular_writer(&spec)?;
            Ok((spec, true))
        }
        "implement" => {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("implement mode requires a non-empty 'prompt'"))?;
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
                },
                false,
            ))
        }
        other => bail!("unknown workflow mode '{other}' (use fanout, implement or verify)"),
    }
}

impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }
    fn description(&self) -> &str {
        "Run several sub-agents (the harness bounds how many run at once). fanout: tasks in \
         parallel + one synthesized answer; `after` chains tasks (each gets its dependencies' \
         reports; one writer per wave). implement: daedalus → themis → nemesis prebuilt from \
         `prompt`, one fix loop on a themis FAIL. verify: a read-only refuter per finding. For \
         one sub-task use `task`."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["fanout", "implement", "verify"], "description": "fanout · implement (prompt → daedalus, themis, nemesis) · verify (refute findings)"},
                "prompt": {"type": "string", "description": "implement mode: the change to make"},
                "tasks": {"type": "array", "maxItems": 32, "description": "fanout mode: the tasks", "items": {"type": "object", "properties": {
                    "id": {"type": "string"},
                    "prompt": {"type": "string", "description": "complete, self-contained task"},
                    "role": {"type": "string", "enum": ["argus", "metis", "daedalus", "nemesis", "themis", "clio", "mnemosyne"], "description": "default nemesis (read-only); daedalus/themis write — one writer per wave; legacy names accepted"},
                    "agent": {"type": "string", "description": "optional specialist slug from <agents>"},
                    "model": {"type": "string"},
                    "boundaries": {"type": "string", "description": "what this child must NOT do or touch"},
                    "expected_output": {"type": "string", "description": "the shape/content of the answer wanted back"},
                    "context": {"type": "array", "items": {"type": "string"}, "description": "established findings (up to 10 lines) the child need not re-derive"},
                    "max_steps": {"type": "integer", "description": "total step budget (cap 80)"},
                    "expects": {"type": "object", "description": "JSON Schema the child's answer must satisfy"},
                    "after": {"type": "array", "items": {"type": "string"}, "description": "ids this task waits for; it receives their reports"},
                    "retry_on_fail": {"type": "string", "description": "an `after` id to re-run once (then this task) when this report opens with VERDICT: FAIL"}
                }, "required": ["prompt"], "additionalProperties": false}},
                "synthesis": {"type": "string", "description": "fanout mode: optional merge instruction"},
                "findings": {"type": "array", "maxItems": 32, "items": {"type": "string"}, "description": "verify mode: claims to refute, with file:line evidence"}
            },
            "required": ["mode"],
            "additionalProperties": false
        })
    }
    /// Never concurrency-safe: the tool IS the parallelism (its children are the concurrent part),
    /// and its writer arm must keep barrier semantics.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn recovery_effect(&self, _args: &Value) -> bool {
        true
    }
    fn execute(&self, args: &Value) -> Result<String> {
        if self.depth >= 1 {
            bail!(
                "workflow is depth-capped at 1 — a sub-agent cannot orchestrate further fan-outs"
            );
        }
        let (spec, synthesize) = build_spec(args)?;
        // Sub-agent gate is acquired INSIDE run_workflow_collect — one slot per concurrent child
        // (see SubagentSlot::acquire_up_to), so the fan-out is counted against the global cap at its
        // real width rather than as a single slot for the whole call. Over-limit → soft error there.
        let client = self.client.clone();
        let base = self.base_url.clone();
        let key = self.api_key.clone();
        let model = self.model.clone();
        let approval = self.approval_mode;
        let cancel = crate::core::cancel::current().unwrap_or_default();
        // Inherit the parent turn's conversation identity so a fanned-out child's tool body scopes
        // per-conversation resources (the browser session) to the SAME conversation the parent serves.
        // Read by `run_one_task` when it builds each child's `AgentConfig` (both run on this thread's
        // `block_on`, so the thread-local is visible there).
        let exec_ctx = crate::core::exec_ctx::current().unwrap_or_default();
        tokio::task::block_in_place(|| {
            crate::core::cancel::with_current(cancel, || {
                crate::core::exec_ctx::with_current(exec_ctx, || {
                    // EFFORT ISOLATION (same as the `task` tool): disarm the parent's process-global effort
                    // override for this synchronous fan-out so every fanned-out child + the synthesis pass
                    // resolves its own `cfg.reasoning_effort` instead of inheriting the parent's pinned tier.
                    // Restored on drop before control returns to the parent turn.
                    let _effort = crate::core::cli_config::suppress_effort_override();
                    tokio::runtime::Handle::current().block_on(run_workflow_collect(
                        &client,
                        &base,
                        &key,
                        &model,
                        approval,
                        &spec,
                        synthesize,
                        &self.root,
                        self.context_window,
                    ))
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "prompt": "review the auth changes",
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
    fn fanout_rejects_parallel_writers() {
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "edit a", "role": "coder"},
                {"prompt": "edit b", "role": "coder"}
            ]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("write-capable"), "{err}");
        // One coder + readers is fine.
        let (spec, synth) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "edit a", "role": "coder"},
                {"prompt": "review b", "role": "reviewer"}
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
            .map(|i| serde_json::json!({"prompt": format!("t{i}"), "role": "reviewer"}))
            .collect();
        assert!(
            build_spec(&serde_json::json!({"mode": "fanout", "tasks": six})).is_ok(),
            "6 read-only tasks are under the 32 cap → accepted"
        );
        // …and only an absurd batch past the disaster-stop cap is rejected.
        let too_many: Vec<_> = (0..33)
            .map(|i| serde_json::json!({"prompt": format!("t{i}"), "role": "reviewer"}))
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
                {"id": "a", "prompt": "x", "role": "argus"},
                {"id": "b", "prompt": "x", "role": "clio"},
                {"id": "c", "prompt": "x", "after": ["a", " b "]},
                {"id": "d", "prompt": "x", "after": "a, c"}
            ]
        }))
        .unwrap();
        assert_eq!(spec.tasks[2].after, ["a", "b"]);
        assert_eq!(spec.tasks[3].after, ["a", "c"]);
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [{"id": "a", "prompt": "x", "after": ["nope"]}]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown task 'nope'"), "{err}");
    }

    #[test]
    fn fanout_tasks_carry_context_findings() {
        let (spec, _) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "review a", "context": ["- parser is in a.rs", "  ", "b"]},
                {"prompt": "plan b"}
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
                {"prompt": "review a", "role": "reviewer"},
                {"prompt": "plan b", "role": "planner"}
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
