//! Conversation-owned task list and backward-compatible `todo_write` surface.
//! Durable state lives in an owned TaskContext; UI and reminders are projections, not authorities.

mod state;
pub use state::{
    CheckResult, CompletionBasis, EvidenceToken, PlanAttachment, Requirement, TaskContext,
    TaskState, TurnEnd,
};

use crate::agent::tools::Tool;
use anyhow::Result;
use console::style;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    /// Omit for new/legacy rows; repeat the returned ID when renaming or reordering a row.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub content: String,
    #[serde(default = "default_status")]
    pub status: Status,
    /// Honest 0–100 confidence (P0.2). Omit on trivial tasks. Used by the confidence-spike gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
    /// How quantifiable/iterable this goal is, 0–100 (P0.3). Below the gate → reframe nudge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hill_climbable: Option<u8>,
}

fn default_status() -> Status {
    Status::Pending
}

impl Todo {
    /// Construct a todo without optional harness fields (tests + internal seeds).
    pub fn new(content: impl Into<String>, status: Status) -> Self {
        Self {
            id: String::new(),
            content: content.into(),
            status,
            confidence: None,
            hill_climbable: None,
        }
    }
}

tokio::task_local! { static CONTEXT: TaskContext; }

/// The driver scopes its entire conversation future; blocking tools receive the same owned
/// handle through ExecutionContext. A missing production scope never falls back to shared state.
pub async fn with_context<F: std::future::Future>(context: TaskContext, future: F) -> F::Output {
    CONTEXT.scope(context, future).await
}

pub(crate) fn bound_context() -> Option<TaskContext> {
    crate::core::exec_ctx::current()
        .and_then(|c| c.task().cloned())
        .or_else(|| CONTEXT.try_with(Clone::clone).ok())
}

pub fn try_current_context() -> Option<TaskContext> {
    if let Some(context) = bound_context() {
        return Some(context);
    }
    // Compatibility for older loop fixtures only; production has no singleton authority.
    #[cfg(test)]
    {
        Some(TEST_CONTEXT.with(Clone::clone))
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(test)]
thread_local! { static TEST_CONTEXT: TaskContext = TaskContext::private(); }

pub fn current_context() -> TaskContext {
    try_current_context().unwrap_or_default()
}

pub fn durable_snapshot() -> TaskState {
    bound_context().map(|c| c.snapshot()).unwrap_or_default()
}

pub fn mark_saved(state: &TaskState) {
    if let Some(context) = try_current_context() {
        context.mark_saved(state);
    }
}

pub fn restore_in_memory(state: TaskState) -> Result<()> {
    try_current_context()
        .ok_or_else(|| anyhow::anyhow!("task restore requires a conversation owner"))?
        .install(state)
}

pub fn restore(state: TaskState) -> Result<()> {
    try_current_context()
        .ok_or_else(|| anyhow::anyhow!("task restore requires a conversation owner"))?
        .restore(state)
}

/// Seed the current task in unit fixtures.
#[cfg(test)]
pub fn set(items: Vec<Todo>) {
    current_context()
        .replace(items)
        .expect("valid internal task seed");
}

/// Only active rows may steer execution. Historical terminal rows are still persisted/displayed.
pub fn snapshot() -> Vec<Todo> {
    current_context().effective_items()
}

pub fn active_item() -> Option<String> {
    snapshot()
        .into_iter()
        .find(|t| t.status == Status::InProgress)
        .map(|t| t.content)
}

pub fn clear() {
    current_context().clear();
}
pub fn end_turn(abnormal: bool) {
    current_context().end_turn(abnormal);
}
pub fn begin_user_turn() -> bool {
    let context = current_context();
    let cleared = context.begin_user_turn();
    if !cleared && !context.snapshot().active && context.is_dirty() {
        crate::ui::tui::note_line("Task snapshot is not saved; retained as inactive. Retry saving or explicitly clear it before replacing the checklist.");
    }
    cleared
}

/// Presentation-only HUD cache for the input/render thread, never read by workflow decisions.
static HUD: Mutex<Option<String>> = Mutex::new(None);
fn publish(state: &TaskState) {
    let rows: Vec<_> = state
        .items
        .iter()
        .map(|t| {
            (
                match t.status {
                    Status::Pending => 0,
                    Status::InProgress => 1,
                    Status::Done => 2,
                },
                t.content.clone(),
            )
        })
        .collect();
    *HUD.lock().unwrap_or_else(|e| e.into_inner()) = summary(&state.items);
    crate::ui::tui::plan_update_snapshot(&rows, state);
}
fn summary(items: &[Todo]) -> Option<String> {
    (!items.is_empty()).then(|| {
        format!(
            "☑ {}/{}",
            items.iter().filter(|t| t.status == Status::Done).count(),
            items.len()
        )
    })
}

/// True when any item is still pending or in_progress (empty list → false).
pub fn has_incomplete() -> bool {
    snapshot().iter().any(|t| t.status != Status::Done)
}

/// Format incomplete items as `[>]` / `[ ]` lines for the todo-poke inject. `None` if none open.
pub fn incomplete_summary(max_chars: usize) -> Option<String> {
    let items = snapshot();
    let open: Vec<&Todo> = items.iter().filter(|t| t.status != Status::Done).collect();
    if open.is_empty() {
        return None;
    }
    let mut text = String::new();
    for t in open {
        let mark = match t.status {
            Status::Done => "[x]", // filtered out; defensive
            Status::InProgress => "[>]",
            Status::Pending => "[ ]",
        };
        let line = format!("{mark} {}\n", t.content);
        if text.chars().count() + line.chars().count() > max_chars {
            text.push_str("…\n");
            break;
        }
        text.push_str(&line);
    }
    Some(text.trim_end().to_string())
}

/// `☑ done/total` for the status bar (None when there are no todos).
pub fn status_summary() -> Option<String> {
    match try_current_context() {
        Some(context) => summary(&context.snapshot().items),
        None => HUD.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    }
}

/// Detect a phase boundary by stable ID. The content fallback only serves pre-normalization
/// legacy fixtures; canonical rows always have IDs, so renaming a row does not close a phase.
pub fn phase_boundary_crossed(before: &[Todo], after: &[Todo]) -> Option<String> {
    let key = |t: &Todo| {
        if t.id.is_empty() {
            format!("legacy:{}", t.content)
        } else {
            t.id.clone()
        }
    };
    let before_status: std::collections::HashMap<_, _> =
        before.iter().map(|t| (key(t), t.status)).collect();
    if let Some(t) = after.iter().rev().find(|t| {
        t.status == Status::Done && before_status.get(&key(t)).copied() != Some(Status::Done)
    }) {
        return Some(t.content.clone());
    }
    if let Some(prev) = before.iter().find(|t| t.status == Status::InProgress) {
        let prev_still_active = after
            .iter()
            .any(|t| key(t) == key(prev) && t.status == Status::InProgress);
        let other_now_active = after
            .iter()
            .any(|t| t.status == Status::InProgress && key(t) != key(prev));
        if !prev_still_active && other_now_active {
            return Some(prev.content.clone());
        }
    }
    None
}

/// A glyph for a status: done ✓, in-progress ▸, pending ○.
///
/// Used only by [`render_block`], which is itself unused — the retained TUI paints todos as ratatui
/// rows rather than a pre-rendered string.
#[allow(dead_code)]
fn glyph(s: Status) -> &'static str {
    match s {
        Status::Done => "✓",
        Status::InProgress => "▸",
        Status::Pending => "○",
    }
}

/// Render the list as a colored checklist block (one line per item). Empty list → empty string.
#[allow(dead_code)]
pub fn render_block(items: &[Todo]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(
        &style("todos:")
            .color256(crate::ui::splash::ACCENT)
            .to_string(),
    );
    out.push('\n');
    for t in items {
        let g = glyph(t.status);
        let line = match t.status {
            // Design: green ✓ + struck, muted text · moonlight ▸ + bright active row · faint ○ + muted.
            Status::Done => {
                format!(
                    " {} {}",
                    crate::ui::theme::ok(g),
                    style(&t.content).dim().strikethrough()
                )
            }
            Status::InProgress => {
                format!(
                    " {} {}",
                    style(g).color256(crate::ui::splash::ACCENT).bold(),
                    style(&t.content).bold()
                )
            }
            Status::Pending => format!(
                " {} {}",
                crate::ui::theme::faint(g),
                crate::ui::theme::muted(&t.content)
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// A terse one-line summary fed back to the MODEL (so it knows the list took effect without
/// re-echoing the whole thing into context).
fn model_ack(items: &[Todo]) -> String {
    if items.is_empty() {
        return "todo list cleared; removed unfinished work remains an abandoned obligation, not completion".to_string();
    }
    let done = items.iter().filter(|t| t.status == Status::Done).count();
    let doing = items
        .iter()
        .filter(|t| t.status == Status::InProgress)
        .count();
    let ids = items
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}={}", i + 1, t.id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "todo list updated: {} item(s), {done} done, {doing} in progress; IDs (row=id): {ids}. Repeat IDs when renaming/reordering. Done is a completion request: rows lacking matching runtime evidence stay pending. Propose observe/check/report requirements; unspecified remains unverified. This acknowledgement is in-memory, not a disk-save receipt.",
        items.len()
    )
}

/// Clamp optional 0–100 fields after serde (accepts up to u8::MAX from JSON).
fn normalize_todos(mut items: Vec<Todo>) -> Vec<Todo> {
    for t in &mut items {
        if let Some(c) = t.confidence {
            t.confidence = Some(c.min(100));
        }
        if let Some(h) = t.hill_climbable {
            t.hill_climbable = Some(h.min(100));
        }
    }
    items
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoInput {
    #[serde(default)]
    id: String,
    content: String,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default)]
    confidence: Option<u8>,
    #[serde(default)]
    hill_climbable: Option<u8>,
    #[serde(default)]
    requirement: Option<ProposedRequirement>,
    #[serde(default)]
    phase: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProposedRequirement {
    Unspecified,
    Observe { target: String },
    Check { command: String, root: String },
    Report,
}

impl ProposedRequirement {
    fn into_runtime(self) -> Requirement {
        match self {
            Self::Unspecified => Requirement::Unspecified,
            Self::Observe { target } => Requirement::Observe { target },
            Self::Check { command, root } => Requirement::Check {
                command,
                root,
                source_identity: None,
            },
            Self::Report => Requirement::Report,
        }
    }
}

struct TodoProposal {
    items: Vec<Todo>,
    requirements: Vec<(String, Requirement)>,
    plan: Option<String>,
    phases: Vec<(String, String)>,
}

impl TodoProposal {
    fn apply(self, context: &TaskContext) -> Result<Vec<Todo>> {
        let attachment = self
            .plan
            .map(|path| {
                let execution = crate::core::exec_ctx::current().ok_or_else(|| {
                    anyhow::anyhow!("plan attachment requires an execution owner")
                })?;
                let root = execution
                    .placement()
                    .ok_or_else(|| anyhow::anyhow!("plan attachment requires a workspace"))?
                    .workspace_root();
                state::PlanAttachment::load(root, &path)
            })
            .transpose()?;
        context.refresh_plan()?;
        context.replace_with_plan(self.items, self.requirements, attachment, self.phases)
    }
}

/// Parse the additive model DTO. Runtime receipt/fingerprint/waiver fields are intentionally not
/// represented and unknown fields are rejected before any state mutation.
fn parse_todos(args: &serde_json::Value) -> Result<TodoProposal> {
    let envelope = args
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("todo_write expects an object"))?;
    anyhow::ensure!(
        envelope.keys().all(|key| key == "todos" || key == "plan"),
        "unknown todo_write field; runtime evidence and waivers cannot be supplied by the model"
    );
    let value = args
        .get("todos")
        .ok_or_else(|| anyhow::anyhow!("missing `todos` array"))?;
    let inputs: Vec<TodoInput> = serde_json::from_value(value.clone())
        .map_err(|e| anyhow::anyhow!("invalid `todos`: {e}"))?;
    let plan = envelope
        .get("plan")
        .map(|value| {
            let path = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("plan must be a relative path"))?;
            crate::agent::plan_document::validate_relative_path(path)?;
            Ok::<_, anyhow::Error>(path.to_owned())
        })
        .transpose()?;
    let mut items = Vec::with_capacity(inputs.len());
    let mut requirements = Vec::new();
    let mut phases = Vec::new();
    for (index, input) in inputs.into_iter().enumerate() {
        let key = if input.id.is_empty() {
            format!("@index:{index}")
        } else {
            input.id.clone()
        };
        if let Some(phase) = input.phase {
            anyhow::ensure!(
                crate::agent::plan_document::valid_phase_id(&phase),
                "invalid phase ID"
            );
            phases.push((key.clone(), phase));
        }
        if let Some(requirement) = input.requirement {
            requirements.push((key, requirement.into_runtime()));
        }
        items.push(Todo {
            id: input.id,
            content: input.content,
            status: input.status.unwrap_or(Status::Pending),
            confidence: input.confidence.map(|v| v.min(100)),
            hill_climbable: input.hill_climbable.map(|v| v.min(100)),
        });
    }
    Ok(TodoProposal {
        items: normalize_todos(items),
        requirements,
        plan,
        phases,
    })
}

/// The `todos` array JSON Schema — identical for `todo_write` and `ScopedTodo`.
fn todos_schema() -> serde_json::Value {
    serde_json::json!({
        "type":"object", "additionalProperties":false,
        "properties":{
            "plan":{"type":"string","description":"Attach existing docs/plans/*.md contract."},
            "todos":{"type":"array","maxItems":64,"items":{
                "type":"object","additionalProperties":false,
                "properties":{
                    "id":{"type":"string"},
                    "content":{"type":"string"},
                    "status":{"enum":["pending","in_progress","done"]},
                    "confidence":{"type":"integer"},
                    "hill_climbable":{"type":"integer"},
                    "phase":{"type":"string"},
                    "requirement":{
                        "type":"object","additionalProperties":false,
                        "properties":{
                            "kind":{"enum":["unspecified","observe","check","report"]},
                            "target":{"type":"string"},
                            "command":{"type":"string"},
                            "root":{"type":"string"}
                        },
                        "required":["kind"]
                    }
                },
                "required":["content","status"]
            }}
        },
        "required":["todos"]
    })
}

/// `todo_write` — the model sends its full current task list; we replace + show it.
pub struct TodoWrite;

impl Tool for TodoWrite {
    fn planning_capability(&self) -> Option<crate::agent::planning::Capability> {
        Some(crate::agent::planning::Capability::TaskUpdate)
    }
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Replace full checklist; empty abandons. Repeat IDs; omit for new rows. Done requests \
         evidence, not proof. Requirement: observe needs target; check needs trusted command/root; \
         report needs answer delivery; unspecified stays open. Optional scores 0–100; phase links \
         to attached plan. Plan text never grants permission."
    }
    fn parameters(&self) -> serde_json::Value {
        todos_schema()
    }
    // Mutates shared session state → keep it on the serial path (not a parallel read-only batch).
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let context = try_current_context()
            .ok_or_else(|| anyhow::anyhow!("todo_write requires a conversation owner"))?;
        let items = parse_todos(args)?.apply(&context)?;
        Ok(model_ack(&items))
    }
}

/// Private scratch checklist for a child. It uses the same validation/identity rules as
/// the parent, but owns a separate context and never publishes into the parent's UI.
pub struct ScopedTodo {
    context: TaskContext,
}

impl ScopedTodo {
    pub fn new() -> Self {
        Self {
            context: TaskContext::private(),
        }
    }
}

impl Default for ScopedTodo {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ScopedTodo {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        TodoWrite.description()
    }
    fn parameters(&self) -> serde_json::Value {
        todos_schema()
    }
    // Mutates this tool's own list → keep it serial (a sub-agent runs single-threaded anyway).
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let context = bound_context().unwrap_or_else(|| self.context.clone());
        let items = parse_todos(args)?.apply(&context)?;
        // No UI emission: a sub-agent's plan must not leak into the user's scroll region. The ack
        // (the recitation signal) is the whole point.
        Ok(model_ack(&items))
    }
}

/// Legacy fixture serialization guard; production task ownership is carried by TaskContext.
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // The list is a process-global, so the stateful tests must not run concurrently.
    use super::TEST_LOCK;

    #[test]
    fn set_and_summary_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set(vec![
            Todo::new("a", Status::Done),
            Todo::new("b", Status::InProgress),
            Todo::new("c", Status::Pending),
        ]);
        assert_eq!(status_summary().as_deref(), Some("☑ 0/3"));
        assert_eq!(snapshot().len(), 3);
        assert!(has_incomplete());
        clear();
        assert!(status_summary().is_none());
        assert!(snapshot().is_empty());
        assert!(!has_incomplete());
    }

    #[test]
    fn incomplete_summary_lists_open_only() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set(vec![
            Todo::new("done-one", Status::Done),
            Todo::new("doing", Status::InProgress),
            Todo::new("later", Status::Pending),
        ]);
        let s = incomplete_summary(600).expect("open items");
        assert!(s.contains("[>] doing"), "{s}");
        assert!(s.contains("[ ] later"), "{s}");
        assert!(
            s.contains("[ ] done-one"),
            "an unverified claim stays open: {s}"
        );
        clear();
        TodoWrite
            .execute(&serde_json::json!({"todos":[{
                "content":"report", "status":"done", "requirement":{"kind":"report"}
            }]}))
            .unwrap();
        current_context().record_report().unwrap();
        assert!(incomplete_summary(600).is_none());
        clear();
    }

    #[test]
    fn execute_replaces_list() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = TodoWrite;
        t.execute(&serde_json::json!({"todos":[{"content":"x","status":"pending"}]}))
            .unwrap();
        assert_eq!(snapshot().len(), 1);
        // A second call REPLACES (does not append).
        t.execute(&serde_json::json!({"todos":[
            {"content":"y","status":"in_progress"},
            {"content":"z","status":"done"}
        ]}))
        .unwrap();
        let s = snapshot();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].content, "y");
        assert_eq!(status_summary().as_deref(), Some("☑ 0/2"));
        clear();
    }

    #[test]
    fn execute_accepts_confidence_and_hill_climbable() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = TodoWrite;
        t.execute(&serde_json::json!({"todos":[{
            "content":"optimize parser",
            "status":"in_progress",
            "confidence": 40,
            "hill_climbable": 85
        }]}))
        .unwrap();
        let s = snapshot();
        assert_eq!(s[0].confidence, Some(40));
        assert_eq!(s[0].hill_climbable, Some(85));
        // Clamp >100.
        t.execute(&serde_json::json!({"todos":[{
            "content":"x","status":"done","confidence": 200, "hill_climbable": 150
        }]}))
        .unwrap();
        let s = snapshot();
        assert_eq!(s[0].confidence, Some(100));
        assert_eq!(s[0].hill_climbable, Some(100));
        clear();
    }

    #[test]
    fn render_block_is_empty_when_no_todos() {
        assert!(render_block(&[]).is_empty());
        let b = render_block(&[Todo::new("build", Status::InProgress)]);
        assert!(b.contains("build"));
        assert!(b.contains("todos:"));
    }

    #[test]
    fn bad_args_error() {
        let t = TodoWrite;
        assert!(t.execute(&serde_json::json!({})).is_err());
        assert!(t.execute(&serde_json::json!({"todos": "nope"})).is_err());
    }

    #[test]
    fn scoped_todo_keeps_its_own_list_and_never_touches_the_global() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Seed the process-global list (the user's top-level plan).
        set(vec![Todo::new("user-task", Status::InProgress)]);

        // A sub-agent's ScopedTodo writes its OWN plan.
        let sub = ScopedTodo::new();
        let ack = sub
            .execute(&serde_json::json!({"todos":[
                {"content":"sub-step-1","status":"in_progress"},
                {"content":"sub-step-2","status":"pending"}
            ]}))
            .unwrap();
        assert!(
            ack.contains("2 item"),
            "ack reflects the sub-agent's own list: {ack}"
        );

        // The global list is UNTOUCHED — no leak between the sub-agent and the user's plan.
        let global = snapshot();
        assert_eq!(global.len(), 1, "global list unchanged by the sub-agent");
        assert_eq!(global[0].content, "user-task");

        // Two independent sub-agents don't share state (per-instance, not global).
        let other = ScopedTodo::new();
        let other_ack = other
            .execute(&serde_json::json!({"todos":[{"content":"x","status":"done"}]}))
            .unwrap();
        assert!(
            other_ack.contains("1 item"),
            "second sub-agent has its own list"
        );
        // The first sub-agent's list is still its own 2 items.
        assert_eq!(sub.context.snapshot().items.len(), 2);

        clear();
    }

    #[test]
    fn scoped_todo_rejects_bad_args() {
        let sub = ScopedTodo::new();
        assert!(sub.execute(&serde_json::json!({})).is_err());
        assert!(sub.execute(&serde_json::json!({"todos": 5})).is_err());
    }

    #[test]
    fn model_cannot_supply_authority_or_unknown_envelope_fields() {
        let context = TaskContext::private();
        let ctx = crate::core::exec_ctx::ExecutionContext::default().with_task(context.clone());
        crate::core::exec_ctx::with_current(ctx, || {
            for args in [
                serde_json::json!({"todos":[], "waiver":true}),
                serde_json::json!({"todos":[], "checks":[]}),
                serde_json::json!({"todos":[{"content":"x","status":"done","epoch":1}]}),
                serde_json::json!({"todos":[{"content":"x","status":"done","requirement":{"kind":"check","command":"cargo check","root":".","source_identity":"forged"}}]}),
            ] {
                assert!(TodoWrite.execute(&args).is_err());
                assert_eq!(context.snapshot(), TaskState::default());
            }
        });
    }

    #[test]
    fn scoped_tool_uses_exact_execution_task_owner() {
        let task = TaskContext::private();
        let ctx = crate::core::exec_ctx::ExecutionContext::default().with_task(task.clone());
        let tool = ScopedTodo::new();
        crate::core::exec_ctx::with_current(ctx, || {
            tool.execute(&serde_json::json!({"todos":[{
                "content":"report", "status":"done", "requirement":{"kind":"report"}
            }]}))
            .unwrap();
        });
        assert!(tool.context.snapshot().items.is_empty());
        assert_eq!(task.snapshot().items.len(), 1);
        task.record_report().unwrap();
        assert!(task.is_complete());
    }

    #[test]
    fn scoped_todo_and_todo_write_share_a_schema() {
        // Both accept the identical shape — a sub-agent that learned todo_write at top level uses
        // it unchanged.
        assert_eq!(TodoWrite.parameters(), ScopedTodo::new().parameters());
        assert_eq!(TodoWrite.name(), ScopedTodo::new().name());
    }

    // ── phase_boundary_crossed: pure, needs no repo/global state ──
    #[test]
    fn phase_boundary_item_reaches_done() {
        let before = vec![Todo::new("A", Status::InProgress)];
        let after = vec![Todo::new("A", Status::Done)];
        assert_eq!(
            phase_boundary_crossed(&before, &after).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn phase_boundary_focus_moves_to_next() {
        // A done, focus shifts B→? No: A stays, B goes in_progress. The done case (rule 1) wins.
        let before = vec![
            Todo::new("A", Status::InProgress),
            Todo::new("B", Status::Pending),
        ];
        let after = vec![
            Todo::new("A", Status::Done),
            Todo::new("B", Status::InProgress),
        ];
        assert_eq!(
            phase_boundary_crossed(&before, &after).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn phase_boundary_focus_shift_without_done() {
        // No item reaches Done, but the active row changed A→B. A implicitly closed (rule 2).
        let before = vec![
            Todo::new("A", Status::InProgress),
            Todo::new("B", Status::Pending),
        ];
        let after = vec![
            Todo::new("A", Status::Pending),
            Todo::new("B", Status::InProgress),
        ];
        assert_eq!(
            phase_boundary_crossed(&before, &after).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn phase_boundary_none_when_unchanged() {
        let list = vec![
            Todo::new("A", Status::InProgress),
            Todo::new("B", Status::Pending),
        ];
        assert_eq!(phase_boundary_crossed(&list, &list), None);
    }

    #[test]
    fn phase_boundary_none_when_only_pending_added() {
        // Adding a future task without moving focus is not a boundary.
        let before = vec![Todo::new("A", Status::InProgress)];
        let after = vec![
            Todo::new("A", Status::InProgress),
            Todo::new("B", Status::Pending),
        ];
        assert_eq!(phase_boundary_crossed(&before, &after), None);
    }

    #[test]
    fn phase_boundary_batch_done_yields_one_label() {
        // Model batch-marks several done at the end of a run: ONE label (the last), not three.
        let before = vec![
            Todo::new("A", Status::Done),
            Todo::new("B", Status::InProgress),
            Todo::new("C", Status::Pending),
        ];
        let after = vec![
            Todo::new("A", Status::Done),
            Todo::new("B", Status::Done),
            Todo::new("C", Status::Done),
        ];
        // B and C are newly done; the last one scanned wins.
        assert_eq!(
            phase_boundary_crossed(&before, &after).as_deref(),
            Some("C")
        );
    }
}

#[cfg(test)]
mod turn_scope_tests {
    use super::*;

    fn seed() {
        set(vec![
            Todo::new("done thing", Status::Done),
            Todo::new("open thing", Status::Pending),
        ]);
    }

    #[test]
    fn a_verified_report_retires_before_the_next_question() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        TodoWrite
            .execute(&serde_json::json!({"todos":[{
                "content":"answer", "status":"done", "requirement":{"kind":"report"}
            }]}))
            .unwrap();
        current_context().record_report().unwrap();
        end_turn(false);
        mark_saved(&current_context().snapshot());
        assert!(begin_user_turn(), "completed task retired");
        assert!(snapshot().is_empty());
        assert!(!has_incomplete(), "a fresh question owes no todo poke");
        clear();
    }

    #[test]
    fn nominal_normal_end_retains_unverified_work() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        seed();
        end_turn(false);
        assert_eq!(
            current_context().snapshot().terminal,
            Some(TurnEnd::Incomplete)
        );
        mark_saved(&current_context().snapshot());
        assert!(!begin_user_turn());
        assert!(has_incomplete());
        clear();
    }

    #[test]
    fn interrupted_and_unverified_ends_keep_continuation_until_resolved() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        seed();
        end_turn(true);
        assert!(!begin_user_turn(), "kept for the continuation");
        assert!(has_incomplete());
        end_turn(false);
        mark_saved(&current_context().snapshot());
        assert!(!begin_user_turn(), "normal return is not evidence");
        assert!(has_incomplete());
        clear();
    }
}
