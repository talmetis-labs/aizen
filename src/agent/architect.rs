//! Architect mode: a strong model thinks, a fast model types.
//!
//! Multi-file turns at `max` (or with a configured planner) are split in two. `metis` on the selected
//! planner model drafts a spec; the editor saves it through ordinary file tools before applying it.
//! Only confirmed nonvisual work may
//! automatically select the cheap editor at low wire effort; visual and unknown intent preserve
//! the effective endpoint and wire effort unless the user configured an explicit apply role. The
//! plan is folded into the request and the `max` tier's harness budgets (steps, verify rounds,
//! self-review) untouched. Aider measured +3 to +5 pp for this split on its benchmark; the strong
//! model never emits code tokens and the editor streams fast. Single-file turns, questions and
//! research never enter it, and a planner that produces nothing usable leaves the turn exactly as
//! it would have run without architect mode.
//!
//! The verify leg of the plan's "metis → daedalus → themis" is the loop's own verify gate and the
//! harness check after edits, which already run in the editor phase — a separate themis child
//! would re-run the same commands a second time.

use crate::agent::design::DesignIntent;
use crate::core::cli_config::{CliConfig, ResolvedEndpoint};

/// The existing planner supplies semantic intent alongside its plan. An absent or ungrounded
/// classification is unknown; it is never permission to silently downgrade a visual task.
#[derive(Debug, Clone)]
pub struct ArchitectPlan {
    pub text: String,
    pub design_intent: DesignIntent,
    /// Suggested destination only. The editor must save it through the normal file tool before
    /// implementation; preparing a plan never grants permission to write the workspace.
    pub path: Option<std::path::PathBuf>,
}

/// Choose the apply endpoint without treating a cheap effort mapping as visual-design consent.
/// An explicitly configured apply role is user policy and may select an editor for any intent.
pub fn select_editor(
    cfg: &CliConfig,
    endpoint: &ResolvedEndpoint,
    effort: Option<String>,
    intent: DesignIntent,
    explicit_apply: Option<(ResolvedEndpoint, Option<String>)>,
) -> (ResolvedEndpoint, Option<String>) {
    if let Some(apply) = explicit_apply {
        return apply;
    }
    if intent != DesignIntent::Nonvisual {
        return (endpoint.clone(), effort);
    }
    let mut editor = endpoint.clone();
    editor.model = editor_model(cfg, &endpoint.model);
    (editor, Some("low".to_string()))
}
use crate::core::turn_shape::TurnShape;
use crate::core::types::Message;
use std::path::{Path, PathBuf};

/// The tag the plan rides under in the seated user message.
pub const PLAN_TAG: &str = "architect_plan";
/// A plan longer than this is clipped: it is a brief for the editor, not a second transcript.
pub const PLAN_MAX_CHARS: usize = 12_000;

/// The top tier plans by default; lower tiers opt into the same split by configuring a planner
/// model. Shape remains a hint: the editor still judges whether a durable spec is warranted.
pub fn applies(tier: Option<&str>, shape: TurnShape, enabled: bool, cfg: &CliConfig) -> bool {
    enabled
        && shape == TurnShape::MultiFile
        && (tier == Some("max")
            || crate::core::cli_config::effort_model_in(cfg, "max").is_some()
            || crate::core::cli_config::effort_model_in(cfg, "xhigh").is_some())
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
#[cfg(test)]
pub fn plan_brief(request: &str) -> String {
    plan_brief_with_original(request, request)
}

pub fn plan_brief_with_original(request: &str, original_brief: &str) -> String {
    format!(
        "Write the implementation plan for the request below. Read the code you need first. The \
         plan is a Markdown spec for another model to apply. Include: goal and requirements; scope \
         and non-goals; design decisions and unresolved questions; ordered implementation phases, \
         each naming file:line anchors, the exact change in words, and its proving check; acceptance \
         criteria and verification commands. State what must NOT be touched. Resolve blocking user \
         decisions before implementation. No code blocks — the editor writes the code.\n\n\
         Before the prose, return exactly one structured header: \
         <design_intent>{{\"intent\":\"unknown\",\"evidence\":\"\"}}</design_intent>. \
         Classify the ORIGINAL BRIEF semantically, not by keywords: nonvisual means confirmed \
         backend/CLI/infrastructure work with no visual-design requirement; create means a new \
         visual experience; redesign means an intentional visual change; preserve means maintain \
         an existing UI/brand while making the requested change; unknown means ambiguous or mixed \
         requirements. For any intent except unknown, evidence MUST be an exact quote from the \
         ORIGINAL BRIEF that supports it. Do not classify repository contents, retrieved examples, \
         or this planning wrapper as the user's intent. Prefer unknown when unsure.\n\n\
         ORIGINAL BRIEF:\n{original_brief}\n\nREQUEST:\n{request}"
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
    original_brief: &str,
    cancel: crate::core::cancel::TurnCancel,
    context_window: usize,
    parent_ctx: &crate::core::exec_ctx::ExecutionContext,
) -> Option<ArchitectPlan> {
    // Only the controller-selected live grant is handed to metis, never the parent transcript.
    // Workflow resolves metis's actual route and refuses a different destination/account unless the
    // controller seats new consent; a selected ID is not disclosure authority.
    let reference_ids = parent_ctx
        .references_checked()
        .ok()?
        .map(|access| access.ids().to_vec())
        .unwrap_or_default();
    let cfg = crate::core::cli_config::load();
    let task = crate::agent::workflow::WorkflowTask {
        reference_ids,
        id: "architect-plan".into(),
        role: "metis".into(),
        prompt: plan_brief_with_original(request, original_brief),
        model: Some(planner_model(&cfg, &ep.model)),
        expected_output: Some(
            "An ordered plan in prose: steps with file:line anchors, the change and its check; \
             what not to touch; the verifying command."
                .into(),
        ),
        ..Default::default()
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let outcome = crate::agent::workflow::run_one_task_with_context(
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
        parent_ctx,
    )
    .await;
    let text = usable_plan(&outcome.status, &outcome.summary)?;
    let parsed = parse_plan(&text, original_brief);
    (!parsed.text.trim().is_empty()).then_some(parsed)
}

/// Only a well-formed leading header with a quote grounded in the original brief is usable intent.
/// Old planners, malformed JSON and unsupported labels all conservatively keep the selected model.
pub fn parse_plan(text: &str, original_brief: &str) -> ArchitectPlan {
    let mut design_intent = DesignIntent::Unknown;
    let mut prose = text.trim();
    if let Some(header) = prose.strip_prefix("<design_intent>") {
        if let Some((json, remainder)) = header.split_once("</design_intent>") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
                let evidence = value.get("evidence").and_then(|v| v.as_str()).unwrap_or("");
                if !evidence.trim().is_empty() && original_brief.contains(evidence) {
                    design_intent = match value.get("intent").and_then(|v| v.as_str()) {
                        Some("nonvisual") => DesignIntent::Nonvisual,
                        Some("create") => DesignIntent::Create,
                        Some("redesign") => DesignIntent::Redesign,
                        Some("preserve") => DesignIntent::Preserve,
                        _ => DesignIntent::Unknown,
                    };
                }
            }
            prose = remainder.trim();
        }
    }
    ArchitectPlan {
        text: prose.to_string(),
        design_intent,
        path: None,
    }
}

/// Only a completed plan or an explicitly caveated step-limited draft may reach the editor.
pub fn usable_plan(status: &str, summary: &str) -> Option<String> {
    let partial = status
        .split('·')
        .next()
        .is_some_and(|s| s.trim() == "max-iters");
    if (!super::workflow::completed_status(status) && !partial) || status.contains("json:invalid") {
        return None;
    }
    let text = summary.trim();
    if text.is_empty() || text.starts_with("error:") {
        return None;
    }
    let text = if partial {
        format!("[Incomplete planner draft: step budget exhausted. Validate every step before applying.]\n{text}")
    } else {
        text.to_string()
    };
    if text.chars().count() <= PLAN_MAX_CHARS {
        return Some(text);
    }
    let mut clipped: String = text.chars().take(PLAN_MAX_CHARS).collect();
    clipped.push_str("\n…[plan clipped]");
    Some(clipped)
}

/// Fold the plan into the seated user message — the last `user` message in `history` — ahead of
/// the request, so the editor reads it as part of what it was asked. The persisted line upstream
/// stays the clean text the user typed. `false` when there is no user message to attach to.
///
/// `plan_path` is a suggestion, NOT evidence that a file exists. Saving goes through the editor's
/// ordinary file tools, with the same approval, checkpoint and ownership checks as any other edit.
pub fn attach_plan(history: &mut [Message], plan: &str, plan_path: Option<&Path>) -> bool {
    let Some(msg) = history.iter_mut().rev().find(|m| m.role == "user") else {
        return false;
    };
    let body = msg.content.take().unwrap_or_default();
    let on_disk = match plan_path {
        Some(path) => format!(
            "\nBefore implementation, save this spec with file_write (purpose: project) at `{}`. \
             file_write requires an existing parent: if docs/plans is missing, create that directory \
             through the normal approved shell tool first. \
             This is a suggested NEW path, not an existing file: inspect it first and never overwrite \
             an unrelated plan; choose a new name if occupied. If the request already has a saved \
             plan, read and reuse it instead of creating a duplicate. Keep the saved path in progress and \
             handoff summaries, and re-read it after compaction or resume. If approval is denied or \
             writing fails, report that explicitly; the in-memory plan remains available, but do \
             not bypass the refusal using another write path.\n",
            path.display()
        ),
        None => String::new(),
    };
    msg.content = Some(format!(
        "<{PLAN_TAG}>\nA planner wrote this spec for the request below. Review it against the \
         request; deviate where the code proves it wrong, and say so. A plan-only request never \
         authorizes implementation.\n{on_disk}\n{plan}\n</{PLAN_TAG}>\n\n{body}"
    ));
    true
}

/// Where durable plans live, relative to the workspace root. A repo path, deliberately: the user
/// reviews and commits it like any other document, and it survives the session. Never the process
/// scratch namespace — that is swept (see `core::scratch`).
pub const PLAN_DIR: &str = "docs/plans";

/// Prepare a draft without filesystem effects. A timestamp, process ID and sequence separate
/// requests; the ordinary file tool still owns collision checks, approval and the atomic write.
pub fn prepare_document(
    request: &str,
    plan: &mut ArchitectPlan,
    planner: &str,
    effort: Option<&str>,
) {
    let title = first_line(request);
    let slug = crate::core::session_store::sanitize_name(&title);
    let slug = if slug.is_empty() { "plan" } else { &slug };
    let stamp = chrono::Utc::now().timestamp_micros();
    let pid = std::process::id();
    let seq = crate::core::persist::unique_sequence();
    plan.path = Some(PathBuf::from(PLAN_DIR).join(format!("{slug}-{stamp}-{pid}-{seq}.md")));
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    plan.text = format!(
        "# Plan: {title}\n\n- Date: {date}\n- Planner: {planner}\n- Effort: {}\n\n\
         ## Request\n\n{}\n\n{}",
        effort.unwrap_or("default"),
        request.trim(),
        plan.text.trim(),
    );
}

/// A one-line label for the plan's title: the request's first line, capped so a pasted blob cannot
/// blow up the heading.
fn first_line(request: &str) -> String {
    let line = request.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    let capped: String = line.chars().take(80).collect();
    // Compare CHAR counts: `capped.len()` is bytes, which would clip a multi-byte line early.
    if capped.chars().count() < line.chars().count() {
        format!("{capped}…")
    } else {
        capped
    }
}

/// The one-line status the REPL prints when the split is in effect.
pub fn status_line(planner: &str, editor: &str, plan_chars: usize, effort: Option<&str>) -> String {
    let effort = effort.unwrap_or("default");
    format!(
        "architect: plan by metis on {planner} ({plan_chars} chars) → applying on {editor} at {effort} effort"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::design::{DesignBrief, DesignContext, DesignMode, GateDecision};

    #[derive(serde::Deserialize)]
    struct FixtureSuite {
        version: u32,
        kind: String,
        protocol: serde_json::Value,
        assets: Vec<String>,
        cases: Vec<FixtureCase>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureCase {
        id: String,
        language: String,
        category: String,
        input: String,
        reference: Option<String>,
        brief: String,
        intent: DesignIntent,
        evidence: String,
        acceptance: String,
    }

    fn fixture_suite() -> FixtureSuite {
        serde_json::from_str(include_str!(
            "../../bench-fixtures/visual-design/cases.json"
        ))
        .expect("valid offline fixture manifest")
    }

    #[test]
    fn visual_fixture_assets_are_bounded_local_inputs_not_pixel_evidence() {
        let suite = fixture_suite();
        assert_eq!(suite.version, 1);
        assert_eq!(suite.kind, "synthetic_offline_contract");
        for key in ["network", "live_model", "uploads", "pixel_evidence"] {
            assert_eq!(suite.protocol[key], false, "{key}");
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench-fixtures/visual-design");
        let limits = &suite.protocol["limits"];
        assert!(suite.cases.len() <= limits["max_cases"].as_u64().unwrap() as usize);
        let mut total = 0;
        let mut names = std::collections::BTreeSet::new();
        for asset in &suite.assets {
            assert!(names.insert(asset));
            assert!(!asset.contains(['/', '\\', ':']));
            assert!(matches!(
                Path::new(asset).extension().and_then(|e| e.to_str()),
                Some("html" | "css" | "svg")
            ));
            let source = std::fs::read_to_string(root.join(asset)).expect("local fixture asset");
            total += source.len();
            assert!(source.len() <= limits["max_asset_bytes"].as_u64().unwrap() as usize);
            assert!(source.to_ascii_lowercase().contains("synthetic offline"));
            let lower = source.to_ascii_lowercase();
            for forbidden in [
                "<script",
                "<iframe",
                "<form",
                "@import",
                "src=\"http",
                "href=\"http",
                "url(http",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "remote/active input in {asset}: {forbidden}"
                );
            }
            if asset.ends_with(".html") {
                assert!(source.starts_with("<!doctype html>"));
                assert!(source.contains("name=\"viewport\""));
                assert!(source.contains("href=\"styles.css\""));
            }
        }
        total += include_str!("../../bench-fixtures/visual-design/cases.json").len();
        assert!(total <= limits["max_suite_bytes"].as_u64().unwrap() as usize);
        let mut ids = std::collections::BTreeSet::new();
        let mut categories = std::collections::BTreeSet::new();
        let mut languages = std::collections::BTreeSet::new();
        for case in &suite.cases {
            assert!(ids.insert(&case.id));
            assert!(names.contains(&case.input));
            if let Some(reference) = &case.reference {
                assert!(names.contains(reference));
            }
            categories.insert(case.category.as_str());
            languages.insert(case.language.as_str());
            assert!(!case.acceptance.trim().is_empty());
            assert!(!case.brief.trim().is_empty());
        }
        for category in [
            "create",
            "redesign",
            "preserve",
            "dashboard",
            "chart",
            "reference",
            "nonvisual",
            "ambiguous",
        ] {
            assert!(categories.contains(category), "missing {category}");
        }
        for language in ["en", "vi", "ja", "es"] {
            assert!(languages.contains(language), "missing {language}");
        }
    }

    #[test]
    fn multilingual_fixture_headers_pin_routing_and_scoped_activation_not_classifier_accuracy() {
        // Headers below are synthetic oracle inputs: this exercises contracts, not an LLM's ability
        // to infer intent in any language. Reading HTML/SVG is never proof that pixels were shown.
        let suite = fixture_suite();
        let cfg = CliConfig {
            models_by_effort: Some(std::collections::BTreeMap::from([(
                "low".into(),
                "cheap".into(),
            )])),
            ..Default::default()
        };
        let selected = ResolvedEndpoint {
            base_url: "https://selected.invalid/v1".into(),
            api_key: "synthetic-unused-key".into(),
            model: "selected".into(),
        };
        for case in suite.cases {
            let header = serde_json::json!({"intent": case.intent, "evidence": case.evidence});
            let plan = parse_plan(
                &format!("<design_intent>{header}</design_intent>\n1. Follow the original brief."),
                &case.brief,
            );
            assert_eq!(plan.design_intent, case.intent, "{}", case.id);
            assert_eq!(plan.text, "1. Follow the original brief.");
            let (editor, wire) = select_editor(
                &cfg,
                &selected,
                Some("max".into()),
                plan.design_intent,
                None,
            );
            let nonvisual = case.intent == DesignIntent::Nonvisual;
            assert_eq!(
                editor.model,
                if nonvisual { "cheap" } else { "selected" },
                "{}",
                case.id
            );
            assert_eq!(wire.as_deref(), Some(if nonvisual { "low" } else { "max" }));
            assert_eq!(editor.base_url, selected.base_url);
            assert_eq!(editor.api_key, selected.api_key);

            let ctx = DesignContext::new(
                format!("fixture/{}", case.id),
                "offline-fixtures",
                DesignMode::Auto,
            );
            ctx.begin_turn(&case.id, "offline-fixtures");
            ctx.set_intent(plan.design_intent);
            assert_eq!(ctx.snapshot().active, case.intent.is_visual());
            if case.intent.is_visual() {
                ctx.apply_brief(
                    DesignBrief {
                        intent: case.intent,
                        brief: case.brief.clone(),
                        acceptance: case.acceptance.clone(),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap();
                assert!(matches!(ctx.gate(), GateDecision::Continue(_)));
                ctx.set_intent(DesignIntent::Unknown);
                assert_eq!(
                    ctx.snapshot().intent,
                    case.intent,
                    "grounded visual brief cannot be erased"
                );
            } else {
                assert_eq!(ctx.gate(), GateDecision::NotRequired);
            }
            let history = vec![Message::system("frozen prefix"), Message::user(&case.brief)];
            let durable = serde_json::to_string(&history).unwrap();
            let mut request = history.clone();
            crate::agent::design::prompt::seat_prompt(&mut request, &ctx);
            assert_eq!(request[0].content, history[0].content);
            assert_eq!(
                request[1]
                    .content
                    .as_deref()
                    .unwrap()
                    .starts_with(crate::agent::design::prompt::MARKER),
                case.intent.is_visual()
            );
            assert_eq!(serde_json::to_string(&history).unwrap(), durable);
            assert!(ctx.snapshot().evidence.is_empty());
            assert!(!ctx.gate().is_verified());
            let child = ctx.fork_child("child", "offline-fixtures");
            assert_ne!(child.scope(), ctx.scope());
            assert!(!child.snapshot().active);
            assert!(child.snapshot().evidence.is_empty());
            ctx.set_mode(DesignMode::Off);
            crate::agent::design::prompt::seat_prompt(&mut request, &ctx);
            assert_eq!(request[1].content, history[1].content);
            assert_eq!(ctx.gate(), GateDecision::Disabled);
            assert!(ctx.capsule().is_none());
            if case.intent == DesignIntent::Unknown {
                ctx.set_mode(DesignMode::On);
                assert!(ctx.capsule().is_some());
                assert_eq!(ctx.gate(), GateDecision::NotRequired);
            }
            ctx.set_mode(DesignMode::Auto);
            ctx.begin_turn("unrelated-next-turn", "offline-fixtures");
            assert!(!ctx.snapshot().active);
            assert_eq!(ctx.gate(), GateDecision::NotRequired);
            assert!(ctx.snapshot().evidence.is_empty());
        }
    }

    #[test]
    fn applies_by_default_only_to_max_multi_file_turns() {
        let cfg = CliConfig::default();
        assert!(applies(Some("max"), TurnShape::MultiFile, true, &cfg));
        assert!(!applies(Some("max"), TurnShape::MultiFile, false, &cfg));
        assert!(!applies(Some("xhigh"), TurnShape::MultiFile, true, &cfg));
        assert!(!applies(None, TurnShape::MultiFile, true, &cfg));
        assert!(!applies(Some("max"), TurnShape::SmallEdit, true, &cfg));
        assert!(!applies(Some("max"), TurnShape::Question, true, &cfg));
        assert!(!applies(Some("max"), TurnShape::Research, true, &cfg));
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
        assert!(attach_plan(&mut history, "1. edit src/a.rs:10", None));
        let last = history[3].content.as_deref().unwrap();
        assert!(last.starts_with("<architect_plan>\n"), "{last}");
        assert!(last.contains("1. edit src/a.rs:10"));
        assert!(last.ends_with("</architect_plan>\n\nrefactor the parser across three files"));
        assert_eq!(history[1].content.as_deref(), Some("earlier"));
        assert_eq!(history[0].content.as_deref(), Some("sys"));
        let mut none = vec![Message::system("sys")];
        assert!(!attach_plan(&mut none, "plan", None));
    }

    #[test]
    fn usable_plan_rejects_failures_and_clips_long_plans() {
        assert_eq!(usable_plan("done", "  1. do x  "), Some("1. do x".into()));
        let partial = usable_plan("max-iters", "partial plan").unwrap();
        assert!(partial.contains("Incomplete planner draft") && partial.ends_with("partial plan"));
        for status in [
            "admission-error",
            "deadline",
            "incomplete-refresh",
            "verification-failed",
            "done · json:invalid",
        ] {
            assert_eq!(usable_plan(status, "1. do x"), None, "{status}");
        }
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
    fn structured_intent_requires_original_brief_evidence_and_defaults_conservatively() {
        let original = "Refactor the CLI parser and its tests";
        let grounded = parse_plan(
            "<design_intent>{\"intent\":\"nonvisual\",\"evidence\":\"CLI parser\"}</design_intent>\n1. change parser",
            original,
        );
        assert_eq!(grounded.design_intent, DesignIntent::Nonvisual);
        assert_eq!(grounded.text, "1. change parser");
        for text in [
            "1. legacy prose plan",
            "<design_intent>{\"intent\":\"nonvisual\",\"evidence\":\"backend only\"}</design_intent>\nplan",
            "<design_intent>{broken}</design_intent>\nplan",
            "<design_intent>{\"intent\":\"nonvisual\",\"evidence\":\"\"}</design_intent>\nplan",
        ] {
            assert_eq!(parse_plan(text, original).design_intent, DesignIntent::Unknown);
        }
        let visual = parse_plan(
            "<design_intent>{\"intent\":\"preserve\",\"evidence\":\"existing UI\"}</design_intent>\nplan",
            "Fix behavior while preserving the existing UI",
        );
        assert_eq!(visual.design_intent, DesignIntent::Preserve);
    }

    #[test]
    fn only_confirmed_nonvisual_may_automatically_downgrade_the_editor() {
        let cfg = CliConfig {
            models_by_effort: Some(std::collections::BTreeMap::from([(
                "low".into(),
                "cheap".into(),
            )])),
            ..Default::default()
        };
        let endpoint = ResolvedEndpoint {
            base_url: "https://selected.invalid/v1".into(),
            api_key: "selected-key".into(),
            model: "selected".into(),
        };
        for intent in [
            DesignIntent::Create,
            DesignIntent::Redesign,
            DesignIntent::Preserve,
            DesignIntent::Unknown,
        ] {
            let (editor, effort) = select_editor(&cfg, &endpoint, Some("max".into()), intent, None);
            assert_eq!(editor.model, endpoint.model);
            assert_eq!(editor.base_url, endpoint.base_url);
            assert_eq!(editor.api_key, endpoint.api_key);
            assert_eq!(effort.as_deref(), Some("max"));
        }
        let (editor, effort) = select_editor(
            &cfg,
            &endpoint,
            Some("max".into()),
            DesignIntent::Nonvisual,
            None,
        );
        assert_eq!(editor.model, "cheap");
        assert_eq!(effort.as_deref(), Some("low"));
        let configured = ResolvedEndpoint {
            base_url: "https://apply.invalid/v1".into(),
            api_key: "apply-key".into(),
            model: "user-editor".into(),
        };
        let (editor, effort) = select_editor(
            &cfg,
            &endpoint,
            Some("max".into()),
            DesignIntent::Create,
            Some((configured, None)),
        );
        assert_eq!(editor.model, "user-editor");
        assert_eq!(editor.base_url, "https://apply.invalid/v1");
        assert_eq!(effort, None);
    }

    #[test]
    fn the_brief_asks_for_prose_and_carries_the_request() {
        let b = plan_brief("split the parser");
        assert!(b.contains("REQUEST:\nsplit the parser"));
        assert!(b.contains("No code blocks"));
        assert!(b.contains("file:line"));
    }

    #[test]
    fn configured_planner_widens_multi_file_turns_only() {
        let cfg = CliConfig {
            models_by_effort: Some(std::collections::BTreeMap::from([(
                "xhigh".into(),
                "planner".into(),
            )])),
            ..Default::default()
        };
        for tier in [Some("high"), Some("low"), None] {
            assert!(applies(tier, TurnShape::MultiFile, true, &cfg));
            assert!(!applies(tier, TurnShape::SmallEdit, true, &cfg));
            assert!(!applies(tier, TurnShape::Research, true, &cfg));
            assert!(!applies(tier, TurnShape::MultiFile, false, &cfg));
        }
    }

    #[test]
    fn draft_has_metadata_and_distinct_safe_suggested_paths_without_writing() {
        let mut first = parse_plan("## Acceptance\nRun cargo test", "request");
        let mut second = first.clone();
        prepare_document("../../etc/passwd CON", &mut first, "planner", Some("high"));
        prepare_document("../../etc/passwd CON", &mut second, "planner", Some("high"));
        let path = first.path.as_ref().unwrap();
        assert_eq!(path.parent().unwrap(), Path::new(PLAN_DIR));
        assert_eq!(path.extension().unwrap(), "md");
        assert!(!path.to_string_lossy().contains(".."));
        assert_ne!(first.path, second.path);
        assert!(!path.exists(), "preparation must not write");
        for text in [
            "- Date:",
            "- Planner: planner",
            "- Effort: high",
            "## Acceptance",
            "Run cargo test",
        ] {
            assert!(first.text.contains(text), "{}", first.text);
        }
    }

    /// Attachment must not claim the suggested path has already been written.
    #[test]
    fn attach_plan_requests_an_ordinary_save_without_claiming_success() {
        let mut with = vec![Message::user("do the thing")];
        let path = Path::new("docs/plans/x.md");
        assert!(attach_plan(&mut with, "PLAN BODY", Some(path)));
        let text = with[0].content.as_deref().unwrap();
        assert!(text.contains("docs/plans/x.md"), "{text}");
        assert!(text.contains("file_write (purpose: project)"));
        assert!(text.contains("not an existing file"));
        assert!(text.contains("approval is denied"));
        assert!(!text.contains("also saved"));
        assert!(text.contains("PLAN BODY"));

        let mut without = vec![Message::user("do the thing")];
        assert!(attach_plan(&mut without, "PLAN BODY", None));
        let text = without[0].content.as_deref().unwrap();
        assert!(!text.contains("docs/plans/"), "{text}");
        assert!(text.contains("PLAN BODY"));

        // No user message to attach to is a clean `false`, not a panic.
        assert!(!attach_plan(&mut [], "PLAN BODY", None));
    }
}
