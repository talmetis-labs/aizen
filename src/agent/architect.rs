//! Architect mode: a strong model thinks, a fast model types.
//!
//! Under `max` effort a multi-file turn is split in two. `metis` on the strongest configured
//! model writes the plan in prose — ordered steps, each naming its files and its check — and the
//! turn's own loop then applies it on the fastest configured model at low wire effort, with the
//! plan folded into the request and the `max` tier's harness budgets (steps, verify rounds,
//! self-review) untouched. Aider measured +3 to +5 pp for this split on its benchmark; the strong
//! model never emits code tokens and the editor streams fast. Single-file turns, questions and
//! research never enter it, and a planner that produces nothing usable leaves the turn exactly as
//! it would have run without architect mode.
//!
//! The verify leg of the plan's "metis → daedalus → themis" is the loop's own verify gate and the
//! harness check after edits, which already run in the editor phase — a separate themis child
//! would re-run the same commands a second time.

use crate::core::cli_config::{CliConfig, ResolvedEndpoint};
use crate::core::turn_shape::TurnShape;
use crate::core::types::Message;
use std::path::Path;

/// The tag the plan rides under in the seated user message.
pub const PLAN_TAG: &str = "architect_plan";
/// A plan longer than this is clipped: it is a brief for the editor, not a second transcript.
pub const PLAN_MAX_CHARS: usize = 12_000;

/// Does this turn take the architect path? Only `max` effort on a multi-file turn, and only
/// while the mode is on.
pub fn applies(tier: Option<&str>, shape: TurnShape, enabled: bool) -> bool {
    enabled && tier == Some("max") && shape == TurnShape::MultiFile
}

/// The planner's model: `models_by_effort` `max`, else `xhigh`, else `fallback` (the turn's own
/// model, already routed for the `max` tier).
pub fn planner_model(cfg: &CliConfig, fallback: &str) -> String {
    crate::core::cli_config::effort_model_in(cfg, "max")
        .or_else(|| crate::core::cli_config::effort_model_in(cfg, "xhigh"))
        .unwrap_or_else(|| fallback.to_string())
}

/// The editor's model: `models_by_effort` `low`, else `fallback`. With no cheap tier configured
/// both phases run on one model — still the plan/apply split, which Aider found to help on its
/// own.
pub fn editor_model(cfg: &CliConfig, fallback: &str) -> String {
    crate::core::cli_config::effort_model_in(cfg, "low").unwrap_or_else(|| fallback.to_string())
}

/// The metis brief: a plan in prose that an editor can follow without re-deriving it.
pub fn plan_brief(request: &str) -> String {
    format!(
        "Write the implementation plan for the request below. Read the code you need first. The \
         plan is prose for another model to apply, so: an ordered list of steps, each naming the \
         file(s) and the file:line anchors it touches, the exact change in words, and the check \
         that proves it; then what must NOT be touched; then the one command that verifies the \
         whole change. No code blocks — the editor writes the code.\n\nREQUEST:\n{request}"
    )
}

/// Run the planner: one `metis` child on the strong model. `None` when it produced nothing
/// usable (error, cancelled, empty) — the turn then runs as it would have without architect mode.
#[allow(clippy::too_many_arguments)]
pub async fn plan(
    http: &reqwest::Client,
    ep: &ResolvedEndpoint,
    approval: crate::core::approval::ApprovalMode,
    root: &Path,
    request: &str,
    cancel: crate::core::cancel::TurnCancel,
    context_window: usize,
) -> Option<String> {
    let cfg = crate::core::cli_config::load();
    let task = crate::agent::workflow::WorkflowTask {
        id: "architect-plan".into(),
        role: "metis".into(),
        prompt: plan_brief(request),
        model: Some(planner_model(&cfg, &ep.model)),
        expected_output: Some(
            "An ordered plan in prose: steps with file:line anchors, the change and its check; \
             what not to touch; the verifying command."
                .into(),
        ),
        ..Default::default()
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let outcome = crate::agent::workflow::run_one_task(
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
        approval,
        root,
        &date,
        &task,
        None,
        cancel,
        context_window,
    )
    .await;
    usable_plan(&outcome.status, &outcome.summary)
}

/// A plan worth handing on: the child finished (a `max-iters` planner still holds a partial plan,
/// an errored or cancelled one holds nothing) and said something.
pub fn usable_plan(status: &str, summary: &str) -> Option<String> {
    if matches!(status, "error" | "cancelled") {
        return None;
    }
    let text = summary.trim();
    if text.is_empty() || text.starts_with("error:") {
        return None;
    }
    if text.chars().count() <= PLAN_MAX_CHARS {
        return Some(text.to_string());
    }
    let mut clipped: String = text.chars().take(PLAN_MAX_CHARS).collect();
    clipped.push_str("\n…[plan clipped]");
    Some(clipped)
}

/// Fold the plan into the seated user message — the last `user` message in `history` — ahead of
/// the request, so the editor reads it as part of what it was asked. The persisted line upstream
/// stays the clean text the user typed. `false` when there is no user message to attach to.
pub fn attach_plan(history: &mut [Message], plan: &str) -> bool {
    let Some(msg) = history.iter_mut().rev().find(|m| m.role == "user") else {
        return false;
    };
    let body = msg.content.take().unwrap_or_default();
    msg.content = Some(format!(
        "<{PLAN_TAG}>\nA planner on a stronger model wrote this plan for the request below. Apply \
         it step by step; deviate only where the code proves it wrong, and say so.\n{plan}\n</{PLAN_TAG}>\n\n{body}"
    ));
    true
}

/// The one-line status the REPL prints when the split is in effect.
pub fn status_line(planner: &str, editor: &str, plan_chars: usize) -> String {
    format!(
        "architect: plan by metis on {planner} ({plan_chars} chars) → applying on {editor} at low effort"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_only_to_max_effort_multi_file_turns_while_on() {
        assert!(applies(Some("max"), TurnShape::MultiFile, true));
        assert!(!applies(Some("max"), TurnShape::MultiFile, false));
        assert!(!applies(Some("xhigh"), TurnShape::MultiFile, true));
        assert!(!applies(None, TurnShape::MultiFile, true));
        assert!(!applies(Some("max"), TurnShape::SmallEdit, true));
        assert!(!applies(Some("max"), TurnShape::Question, true));
        assert!(!applies(Some("max"), TurnShape::Research, true));
    }

    #[test]
    fn phase_models_come_from_models_by_effort_with_the_turn_model_as_fallback() {
        let mut cfg = CliConfig::default();
        assert_eq!(planner_model(&cfg, "main"), "main");
        assert_eq!(editor_model(&cfg, "main"), "main");
        let mut m = std::collections::BTreeMap::new();
        m.insert("xhigh".to_string(), "strong-ish".to_string());
        m.insert("low".to_string(), "fast".to_string());
        cfg.models_by_effort = Some(m.clone());
        assert_eq!(
            planner_model(&cfg, "main"),
            "strong-ish",
            "xhigh stands in for max"
        );
        assert_eq!(editor_model(&cfg, "main"), "fast");
        m.insert("max".to_string(), "strongest".to_string());
        cfg.models_by_effort = Some(m);
        assert_eq!(planner_model(&cfg, "main"), "strongest");
    }

    #[test]
    fn attach_plan_folds_into_the_last_user_message_only() {
        let mut history = vec![
            Message::system("sys"),
            Message::user("earlier"),
            Message::assistant("ok"),
            Message::user("refactor the parser across three files"),
        ];
        assert!(attach_plan(&mut history, "1. edit src/a.rs:10"));
        let last = history[3].content.as_deref().unwrap();
        assert!(last.starts_with("<architect_plan>\n"), "{last}");
        assert!(last.contains("1. edit src/a.rs:10"));
        assert!(last.ends_with("</architect_plan>\n\nrefactor the parser across three files"));
        assert_eq!(history[1].content.as_deref(), Some("earlier"));
        assert_eq!(history[0].content.as_deref(), Some("sys"));
        let mut none = vec![Message::system("sys")];
        assert!(!attach_plan(&mut none, "plan"));
    }

    #[test]
    fn usable_plan_rejects_failures_and_clips_long_plans() {
        assert_eq!(usable_plan("done", "  1. do x  "), Some("1. do x".into()));
        assert_eq!(
            usable_plan("max-iters", "partial plan"),
            Some("partial plan".into())
        );
        assert_eq!(usable_plan("error", "1. do x"), None);
        assert_eq!(usable_plan("cancelled", "1. do x"), None);
        assert_eq!(usable_plan("done", "   "), None);
        assert_eq!(usable_plan("done", "error: unknown agent"), None);
        let long = "p".repeat(PLAN_MAX_CHARS + 10);
        let got = usable_plan("done", &long).unwrap();
        assert!(got.ends_with("…[plan clipped]"));
        assert!(got.chars().count() < PLAN_MAX_CHARS + 20);
    }

    #[test]
    fn the_brief_asks_for_prose_and_carries_the_request() {
        let b = plan_brief("split the parser");
        assert!(b.contains("REQUEST:\nsplit the parser"));
        assert!(b.contains("No code blocks"));
        assert!(b.contains("file:line"));
    }
}
