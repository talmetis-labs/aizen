//! One agent turn, shared verbatim by both REPL surfaces.
//!
//! The two loops differ in exactly two things — how a line arrives, and whether Esc can race the
//! model call. Everything else was written out twice and drifted, so it lives here once: building
//! the turn's tool registry, seating the user message, running the turn, and finishing it.

use crate::agent::design::{DesignContext, DesignMode};
use crate::agent::prompt_lanes::{
    fold_context_into_query, refresh_dynamic_prompt_lane, refresh_prompt_lanes_in_place,
    update_system_prompt,
};
use crate::agent::{self, AgentConfig, AgentOutcome, StopReason};
use crate::core::session_store::publish_live_history;
use crate::core::types::ToolDef;
use crate::core::{cli_config, types};
use crate::llm::client;
use crate::repl::postturn::{last_turn_slice, maybe_evolve_persona, maybe_run_secretary};
use crate::ui::context_report::{
    ctx_permille, resolve_ctx_window, usage_ctx_tokens, usage_input_tokens,
};
use crate::ui::image_input;
use crate::ui::{splash, theme, tui};
use crate::{approval_mode, cancellable_slash, eager_enabled, summarizer_endpoint};
use anyhow::Result;
use console::style;
use types::Message;

/// One final routing decision consumed by the prompt, registry, context and model request.
/// Harness effort remains the requested tier even when architect deliberately uses a low editor.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTurn {
    pub endpoint: cli_config::ResolvedEndpoint,
    pub harness_effort: Option<String>,
    pub wire_effort: Option<String>,
    pub design: DesignContext,
    pub architect_plan: Option<crate::agent::architect::ArchitectPlan>,
    pub(crate) references: Option<crate::core::reference_media::ReferenceAccess>,
    reference_markers: Vec<crate::core::tool_media::ToolImage>,
    /// Session declarations resolved after user setup, never a config to persist.
    pub(crate) vision_config: cli_config::CliConfig,
}

impl ResolvedTurn {
    pub(crate) fn new(
        endpoint: &cli_config::ResolvedEndpoint,
        harness_effort: Option<String>,
        wire_effort: Option<String>,
        design: DesignContext,
    ) -> Self {
        let routed = cli_config::route_endpoint_for_effort(endpoint, harness_effort.as_deref());
        // Effort mappings may name models on another configured provider: credentials and URL
        // must follow the chosen model, not remain those of the original session endpoint.
        let endpoint = if routed.model != endpoint.model {
            cli_config::endpoint_for_model(&routed.model, &routed)
        } else {
            routed
        };
        Self::from_frozen(
            endpoint,
            harness_effort,
            wire_effort,
            design,
            cli_config::config_with_session_vision(cli_config::load()),
        )
    }

    /// Seat an already resolved route and owned snapshot without loading saved config or credentials.
    /// Replay and offline fixtures use this instead of the interactive routing constructor.
    pub(crate) fn from_frozen(
        endpoint: cli_config::ResolvedEndpoint,
        harness_effort: Option<String>,
        wire_effort: Option<String>,
        design: DesignContext,
        vision_config: cli_config::CliConfig,
    ) -> Self {
        Self {
            endpoint,
            harness_effort,
            wire_effort,
            design,
            architect_plan: None,
            references: None,
            reference_markers: Vec::new(),
            vision_config,
        }
    }

    /// Freeze only after all capability/reference decisions and after any route change.
    pub(crate) fn freeze_vision_config(&mut self) {
        self.vision_config = cli_config::config_with_session_vision(cli_config::load());
    }

    pub(crate) fn execution_context(&self) -> crate::core::exec_ctx::ExecutionContext {
        let state = self.design.snapshot();
        let context = crate::core::exec_ctx::ExecutionContext::new(
            crate::core::convo::ConversationId::new(state.owner_id),
        )
        .with_design(self.design.clone());
        match &self.references {
            Some(access) => context
                .with_resource_scope(access.scope().task_scope.clone())
                .with_references(access.clone()),
            None => context,
        }
    }

    /// Add identity sidecars to the newly seated canonical user occurrence, not another history
    /// message with equal pixels. This does not transmit/grant or materialize another image URL.
    pub(crate) fn seat_reference_markers(&self, history: &mut [Message]) {
        if let Some(message) = history.last_mut().filter(|message| message.role == "user") {
            message
                .tool_images
                .extend(self.reference_markers.iter().cloned());
            if let Some(access) = &self.references {
                let ids = access.ids().join(", ");
                let note = format!("\n<selected_references>Current task reference IDs: {ids}. Inspect authorized originals with reference_read before constructing the implementation. Discover reference_read, reference_compare and reference_trace with tool_search when they are not advertised. Pass only selected reference_ids to same-route delegates. Exact fidelity requires current implementation capture and whole-frame comparison; preview/crop inspection is not original review.</selected_references>");
                message
                    .content
                    .get_or_insert_with(String::new)
                    .push_str(&note);
            }
        }
    }

    pub(crate) fn configure(&self, cfg: &mut AgentConfig, apply_budgets: bool) {
        cfg.context_window = resolve_ctx_window(&self.endpoint.model).0;
        cfg.nudge_role = agent::NudgeRole::for_base_url(&self.endpoint.base_url);
        cfg.wire_reasoning_effort = Some(self.wire_effort.clone());
        cfg.tool_image_destination = Some(crate::llm::media::effective_destination_key(
            &self.endpoint.base_url,
            &self.endpoint.model,
        ));
        cfg.image_config = Some(std::sync::Arc::new(self.vision_config.clone()));
        cfg.tool_image_policy = crate::llm::media::vision_policy_in(
            &self.vision_config,
            &self.endpoint.base_url,
            &self.endpoint.model,
        );
        let (destination_url, destination_model) = crate::llm::media::effective_image_destination(
            &self.endpoint.base_url,
            &self.endpoint.model,
        );
        let label_url = reqwest::Url::parse(&destination_url)
            .ok()
            .map(|mut url| {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            })
            .unwrap_or_else(|| "(invalid endpoint)".to_string());
        let label_model: String = destination_model
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();
        cfg.tool_image_destination_label = Some(format!("{label_url} · {label_model}"));
        cfg.exec_ctx = self.execution_context();
        if apply_budgets {
            if let Some(tier) = self.harness_effort.as_deref() {
                cfg.apply_effort(tier);
            }
        }
    }

    pub(crate) fn refresh_prompt(&self, history: &mut Vec<Message>, effective_model: &mut String) {
        let prompt_model_matches = history
            .first()
            .and_then(|m| m.content.as_deref())
            .and_then(|prefix| prefix.split_once("<environment>"))
            .and_then(|(_, environment)| environment.split_once("</environment>"))
            .and_then(|(environment, _)| {
                environment
                    .lines()
                    .find_map(|line| line.strip_prefix("model: "))
            })
            .is_some_and(|model| model == self.endpoint.model);
        if *effective_model != self.endpoint.model || !prompt_model_matches {
            refresh_prompt_lanes_in_place(history, &self.endpoint.model);
            *effective_model = self.endpoint.model.clone();
        }
    }
}

/// Complete capability setup on the trusted top-level controller before dispatch. A capability
/// declaration is not permission to disclose the attachments; media grants remain separate.
/// The caller freezes request/config state only AFTER this returns, and repeats it on route change.
pub(crate) async fn guided_vision_preflight(
    turn: &ResolvedTurn,
    has_attachments: bool,
) -> Result<bool> {
    if !has_attachments && !turn.design.snapshot().intent.is_visual() {
        return Ok(true);
    }
    let base_url = turn.endpoint.base_url.clone();
    let model = turn.endpoint.model.clone();
    tokio::task::spawn_blocking(move || {
        crate::ui::config_ui::guided_vision_setup(&base_url, &model)
    })
    .await
    .map_err(anyhow::Error::from)?
}

/// Register ONLY this submission's attachments, never grant references from restored history.
/// Attachments become references for this task automatically — no prompt. The grant is still the
/// narrow one (this task, this destination/account route, originals), and disclosure to the
/// provider stays governed by the vision declaration, so nothing about sending became more
/// permissive; the question was asked once per attachment and always answered the same way.
pub(crate) fn prepare_current_references(
    turn: &mut ResolvedTurn,
    images: &[String],
) -> Result<()> {
    if images.is_empty() {
        return Ok(());
    }
    let (registry, descriptors) = register_current_reference_images(turn, images)?;
    validate_current_reference_selection(images, &descriptors)?;
    seat_current_reference_grant(turn, registry, descriptors)
}

/// Explicit headless-controller reference-purpose opt-in. No dialog, saved config, history scan,
/// provider request or disclosure approval: the caller invokes this only for its explicit CLI flag.
pub(crate) fn seat_loaded_reference_images(
    turn: &mut ResolvedTurn,
    images: &[String],
) -> Result<()> {
    if images.is_empty() {
        return Ok(());
    }
    let (registry, descriptors) = register_current_reference_images(turn, images)?;
    validate_current_reference_selection(images, &descriptors)?;
    seat_current_reference_grant(turn, registry, descriptors)
}

fn register_current_reference_images(
    turn: &mut ResolvedTurn,
    images: &[String],
) -> Result<(
    crate::core::reference_media::ReferenceRegistry,
    Vec<crate::core::reference_media::ReferenceDescriptor>,
)> {
    let context = turn.execution_context();
    let registry =
        crate::core::reference_media::ReferenceRegistry::new(context.conversation().to_string());
    let mut current = Message::user_with_images("", images.to_vec());
    let descriptors = registry.register_loaded(std::slice::from_mut(&mut current))?;
    turn.reference_markers = current.tool_images;
    Ok((registry, descriptors))
}

fn validate_current_reference_selection(
    images: &[String],
    descriptors: &[crate::core::reference_media::ReferenceDescriptor],
) -> Result<()> {
    anyhow::ensure!(
        images.len() <= crate::core::tool_media::MAX_REQUEST_IMAGES,
        "Reference setup accepts at most four current attachments; attach fewer images and retry"
    );
    anyhow::ensure!(descriptors.len() == images.len(),
        "Reference setup incomplete: one or more current attachments are unavailable or unsupported; no reference grant was issued");
    Ok(())
}

/// Pure controller grant seating; no prompts, history lookup, persistence or provider request.
fn seat_current_reference_grant(
    turn: &mut ResolvedTurn,
    registry: crate::core::reference_media::ReferenceRegistry,
    descriptors: Vec<crate::core::reference_media::ReferenceDescriptor>,
) -> Result<()> {
    use crate::core::reference_media::{ReferenceDerivatives, ReferenceGrantScope};
    let context = turn.execution_context();
    let route = current_reference_route(&turn.endpoint)?;
    let ids: Vec<String> = descriptors
        .iter()
        .map(|reference| reference.reference_id.clone())
        .collect();
    let access = registry.grant(&ids, ReferenceGrantScope {
        owner: context.conversation().to_string(),
        purpose: "Use explicitly selected current attachments as references for this task and same-route delegates".into(),
        task_scope: context.resource_scope(),
        route,
    }, ReferenceDerivatives::ALL)?;
    turn.design.activate_selected_references(
        descriptors
            .into_iter()
            .map(|reference| (reference.reference_id, reference.original.sha256))
            .collect(),
        crate::agent::design::FidelityMode::Exact,
    )?;
    turn.references = Some(access);
    Ok(())
}

fn current_reference_route(
    endpoint: &cli_config::ResolvedEndpoint,
) -> Result<crate::core::reference_media::ReferenceRoute> {
    if crate::llm::oauth_codex::is_codex_base_url(&endpoint.base_url) {
        let token = crate::llm::oauth_codex::load_token();
        anyhow::ensure!(token.as_ref().is_some_and(|token| !token.access_token.is_empty()),
            "Reference setup required: sign in to the selected Codex account before granting reference access");
    }
    Ok(crate::core::reference_media::ReferenceRoute {
        destination: crate::llm::media::effective_destination_key(
            &endpoint.base_url,
            &endpoint.model,
        ),
        account: crate::llm::media::effective_image_route_key(
            &endpoint.base_url,
            &endpoint.model,
            &endpoint.api_key,
        ),
    })
}

/// A planner-selected editor cannot inherit a reference grant for another destination/account.
/// Returning to the controller is required; never silently drop references or reroute the image.
pub(crate) fn validate_current_reference_route(turn: &ResolvedTurn) -> Result<()> {
    if let Some(access) = &turn.references {
        crate::llm::media::require_reference_vision(crate::llm::media::vision_policy_in(
            &turn.vision_config,
            &turn.endpoint.base_url,
            &turn.endpoint.model,
        ))?;
        let context = turn.execution_context();
        access.validate_scope(
            &context.conversation().to_string(),
            &context.resource_scope(),
        )?;
        anyhow::ensure!(access.scope().route == current_reference_route(&turn.endpoint)?,
            "Reference setup required: the final editor destination/account differs from the approved reference route. Choose that route explicitly and retry for fresh consent");
    }
    Ok(())
}

/// Session-scoped handle: no design state is read from a process-global slot.
pub(crate) fn new_session_design(owner: impl Into<String>) -> DesignContext {
    DesignContext::new(owner, design_workspace(), DesignMode::Auto)
}

pub(crate) fn design_workspace() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string())
}

pub(crate) fn explicit_wire_effort(cfg: &AgentConfig) -> Option<String> {
    cfg.wire_reasoning_effort
        .clone()
        .unwrap_or_else(|| cli_config::load().reasoning_effort)
}

/// Pull drag-dropped / pasted image paths out of a typed line, leaving the prose behind.
///
/// The other half of Ctrl-O clipboard attach: only real image files are lifted, so a message that
/// merely mentions a path keeps its text intact.
pub(crate) fn lift_image_attachments(line: &mut String, images: &mut Vec<String>) {
    if line.is_empty() {
        return;
    }
    let (cleaned, from_line) = image_input::extract_image_attachments(line);
    if !from_line.is_empty() {
        images.extend(from_line);
        *line = cleaned;
    }
}

/// Build this turn's tool registry against the resolved endpoint.
pub(crate) fn build_turn_registry(
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
) -> Result<agent::tools::ToolRegistry> {
    agent::builtin::default_registry_with_task(
        http.clone(),
        ep.base_url.clone(),
        ep.api_key.clone(),
        ep.model.clone(),
        approval_mode(),
        resolve_ctx_window(&ep.model).0,
        None, // cwd IS the project in the REPL
    )
}

/// The agent config for one interactive turn.
///
/// `enable_steering` is the only thing the two surfaces disagree on: the retained REPL has a
/// mailbox the user can type into mid-turn, the plain one does not. Everything else — approval
/// mode, context window, self-review, LSP state, goal mode, the mid-turn snapshot — is a reading of
/// live config that must not be allowed to differ between them, which is why it is written once.
pub(crate) fn turn_agent_config(
    cancel: crate::core::cancel::TurnCancel,
    model: &str,
    enable_steering: bool,
) -> AgentConfig {
    AgentConfig {
        approval_mode: approval_mode(),
        cancel,
        context_window: resolve_ctx_window(model).0,
        // The auto-compact threshold from `/config` (`compact_threshold_pct`, default 80, `0` off)
        // arms the loop's own compaction — the one compaction trigger the REPL has.
        compact_at_pct: crate::compact_threshold_pct(),
        enable_self_review: cli_config::self_review_enabled(&cli_config::load()),
        // Reflect the live manager state (honours `/lsp off` for this turn).
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        // Goal mode (set by `/goal <text>`): threads the live goal into this turn so the loop runs
        // cap-free with smart retry until the goal is declared and verified.
        goal: crate::agent::goal::current_goal(),
        // Only the interactive top-level turn reads the steering mailbox — a course correction the
        // user typed is aimed at THIS task, not at whatever a delegated sub-agent is doing.
        enable_steering,
        // Keep the exit-flush snapshot current DURING the turn, not just at its edges.
        on_progress: Some(publish_live_history),
        // The user is sitting here waiting: retry a 429/5xx blip many times (like the Claude CLI)
        // with FAST backoff (`interactive_backoff_ms`), so the whole chain still fits in ~30–40s and
        // then reports a clear error. 10 is a CEILING, not a cost — a gateway that recovers on try 2
        // stops at 2. `/goal` does not take this branch (goal mode retries transient errors
        // indefinitely, see agent/mod.rs), so raising this never shortens a goal run.
        max_transient_retries: 10,
        // Where the loop's mid-run nudges go: a `system` message on Anthropic-style gateways, the
        // user turn everywhere else (the Codex path hoists every system message into its
        // instructions blob, and many local chat templates reject a mid-history system role).
        nudge_role: nudge_role_for_endpoint(),
        ..AgentConfig::default()
    }
}

/// A multi-file turn may reuse one metis planner at max or when a planner model is configured.
/// The final turn keeps visual/unknown endpoint and effort unless an explicit apply role overrides it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn architect_phase(
    http: &reqwest::Client,
    turn: &mut ResolvedTurn,
    shape: crate::core::turn_shape::TurnShape,
    line: &str,
    original_brief: &str,
    cancel: crate::core::cancel::TurnCancel,
    approval: crate::core::approval::ApprovalMode,
) {
    use crate::agent::architect;
    let cfg = cli_config::load();
    if !architect::applies(
        turn.harness_effort.as_deref(),
        shape,
        cli_config::architect_mode_enabled(),
        &cfg,
    ) {
        return;
    }
    let root = std::path::PathBuf::from(design_workspace());
    let planner = architect::planner_model(&cfg, &turn.endpoint.model);
    tui::emit_line(&theme::faint(&format!("architect: metis planning on {planner}…")).to_string());
    let planner_context = turn.execution_context();
    let Some(plan) = architect::plan(
        http,
        &turn.endpoint,
        approval,
        &root,
        line,
        original_brief,
        cancel,
        resolve_ctx_window(&turn.endpoint.model).0,
        &planner_context,
    )
    .await
    else {
        return;
    };
    // Prepare only: the editor saves through ordinary file tools, never a privileged write here.
    let mut plan = plan;
    architect::prepare_document(
        original_brief,
        &mut plan,
        &planner,
        turn.harness_effort.as_deref(),
    );
    if turn.references.is_none() {
        turn.design.set_intent(plan.design_intent);
    }
    // A selected reference contract is already active before planning. A planner's Nonvisual or
    // Unknown label cannot downgrade that trusted contract or select a cheap nonvisual editor.
    let explicit_apply = cli_config::role_configured("apply").then(|| {
        (
            cli_config::resolve_role("apply", &turn.endpoint),
            cli_config::role_reasoning_effort("apply", turn.wire_effort.clone()),
        )
    });
    let (editor, wire_effort) = architect::select_editor(
        &cfg,
        &turn.endpoint,
        turn.wire_effort.clone(),
        turn.design.snapshot().intent,
        explicit_apply,
    );
    turn.endpoint = if editor.model != turn.endpoint.model && !cli_config::role_configured("apply")
    {
        cli_config::endpoint_for_model(&editor.model, &editor)
    } else {
        editor
    };
    turn.wire_effort = wire_effort;
    tui::emit_line(
        &theme::faint(&architect::status_line(
            &planner,
            &turn.endpoint.model,
            plan.text.chars().count(),
            turn.wire_effort.as_deref(),
        ))
        .to_string(),
    );
    turn.architect_plan = Some(plan);
}

/// The nudge role for the endpoint this REPL talks to (saved config or the `AIZEN_BASE_URL`
/// override — the same two sources `resolve_endpoint` reads first).
fn nudge_role_for_endpoint() -> crate::agent::NudgeRole {
    let base = cli_config::branded_env("BASE_URL")
        .or_else(|| cli_config::load().base_url.clone())
        .unwrap_or_default();
    crate::agent::NudgeRole::for_base_url(&base)
}

/// Fold this turn's retrieved context into the outgoing message and seat it in history.
///
/// `line` stays the clean text the user typed — the fold only affects what is SENT, so the
/// checkpoint, the display and the persisted transcript all keep the original. The dynamic prompt
/// lane is refreshed AFTER the fold, never before: recall seats this turn's handle→id ledger and the
/// `<skills>` lane ranks itself by affinity to exactly those facts, so refreshing first would build
/// the index against the PREVIOUS turn's recall.
pub(crate) fn seat_user_message(
    line: &str,
    images: Vec<String>,
    history: &mut Vec<Message>,
    model: &str,
    design: &DesignContext,
) {
    // Every model call from here until the next seated user message — the loop, its sub-agents,
    // the post-turn chores — is billed to this turn in the usage ledger.
    client::cost_meter().begin_turn();
    // The todo list is scoped to the turn: cleared here unless the previous turn ended abnormally
    // (its open items are then the continuation plan). Assume THIS turn ends abnormally until
    // `finish_turn` says otherwise, so a turn that errors out keeps its plan for the retry.
    crate::agent::todo::begin_user_turn();
    crate::agent::todo::end_turn(true);
    let sent = fold_context_into_query(line);
    refresh_dynamic_prompt_lane(history, model);
    if images.is_empty() {
        history.push(Message::user(sent));
    } else {
        tui::emit_line(
            &style(format!("📎 {} image(s) attached", images.len()))
                .color256(splash::ACCENT)
                .to_string(),
        );
        history.push(Message::user_with_images(sent, images));
    }
    // Refresh the exit-flush snapshot the moment the user turn lands, so an abrupt window close
    // mid-turn still persists the question (the per-turn autosave only runs on success).
    crate::core::session_store::update_live_history_with_design(history, Some(&design.snapshot()));
}

// ── the shared turn: everything both REPL surfaces do identically ─────────────────────────────
// `run_menu_sticky` and `run_menu_plain` differ in exactly two things — how a line arrives, and
// whether Esc can race the model call. Everything else about a turn is the same work, and it used
// to be written out twice. It drifted: the plain loop once ran the skill, persona and memory passes
// in a different order than the retained one, and to this day it was the copy that never learned
// about goal-mode completion, the post-turn timeout ceiling, or the recovery checkpoints. The two
// functions below are the parts that were always meant to be one thing.
//
// They are surface-agnostic because `tui::emit_line` already is: it renders through the retained
// backend when one is running and prints append-only when none is, so the same call reads correctly
// on both. Nothing here may use `println!` — see `tui::note_line`.

/// Run one agent turn against `ep`, with the model wiring both REPLs were building by hand.
///
/// The three closures (stream the turn, summarize for mid-loop compaction, optionally consult the
/// `oracle` role for self-review) are pure functions of the endpoint, so there was never a reason
/// for two copies. The caller keeps ownership of cancellation: the retained REPL races this future
/// against Esc, the plain one simply awaits it.
pub(crate) async fn run_agent_turn(
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
    cfg: &AgentConfig,
    registry: &agent::tools::ToolRegistry,
    history: &mut Vec<Message>,
) -> Result<AgentOutcome> {
    let mut recovery_boundary = crate::core::recovery::TurnBoundary::new();
    let base = ep.base_url.as_str();
    let key = ep.api_key.as_str();
    let model = ep.model.as_str();
    let eager_on = eager_enabled();
    let window = resolve_ctx_window(model).0;
    let wire_effort = explicit_wire_effort(cfg);
    let image_config = cfg.image_config.clone().unwrap_or_else(|| {
        std::sync::Arc::new(cli_config::config_with_session_vision(cli_config::load()))
    });
    let streamed_text = std::sync::Mutex::new(None::<String>);
    let streamed_text_ref = &streamed_text;
    let chat = move |mut msgs: Vec<Message>, defs: Vec<ToolDef>| {
        if let Some(design) = cfg.exec_ctx.design() {
            crate::agent::design::prompt::seat_prompt(&mut msgs, &design);
        }
        let image_config = image_config.clone();
        let wire_effort = wire_effort.clone();
        async move {
            // LIVE context meter + `↑N tok` chip, fed per send: `msgs`+`defs` are exactly the request
            // going out, so the gauge tracks tool results growing the context MID-turn (the idle
            // status refresh never sees that). The chars/4 estimate moves both the moment the request
            // leaves; the provider's real usage on the reply corrects them — and the context size is
            // remembered so the idle refresh keeps the real number instead of stomping it (see
            // `status_text`). This closure is the ONLY writer of either: sub-agent and chore calls
            // answer for other contexts and must not steer the main meter.
            let est = msgs
                .iter()
                .map(agent::estimate_message_tokens)
                .sum::<usize>()
                + agent::estimate_defs_tokens(&defs);
            tui::set_ctx_permille(ctx_permille(est, window));
            tui::set_sent_tokens(est as u64);
            let (turn, receipt) =
                crate::llm::media::with_image_delivery(&cfg.exec_ctx, base, key, model, async {
                    let starter = eager_on.then(|| agent::eager_starter(registry, cfg));
                    client::stream_chat_with_tools_sink_frozen(
                        http,
                        base,
                        key,
                        model,
                        &msgs,
                        &defs,
                        client::StreamSink {
                            render: true,
                            eager: starter
                                .as_ref()
                                .map(|hook| hook as client::EagerStartFn<'_>),
                            ..Default::default()
                        },
                        wire_effort,
                        &image_config,
                    )
                    .await
                })
                .await?;
            if let Some(design) = cfg.exec_ctx.design() {
                agent::mark_image_delivery(&msgs, &receipt, &design);
            }
            *streamed_text_ref.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(turn.content.clone().unwrap_or_default());
            if let Some(u) = &turn.usage {
                let real = usage_ctx_tokens(u);
                if real > 0 {
                    tui::set_ctx_real_tokens(real as u64);
                    tui::set_ctx_permille(ctx_permille(real, window));
                }
                let sent = usage_input_tokens(u);
                if sent > 0 {
                    tui::set_sent_tokens(sent as u64);
                }
            }
            Ok(turn)
        }
    };
    // Non-streaming summarizer for mid-loop auto-compaction (keeps the streamed display clean).
    let sum_ep = summarizer_endpoint(base, key, model);
    let sum_effort =
        cli_config::role_reasoning_effort("summarizer", cli_config::load().reasoning_effort);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        let effort = sum_effort.clone();
        async move {
            tokio::time::timeout(
                crate::repl::postturn::chore_call_timeout(),
                client::chat_with_tools_effort(
                    http,
                    &ep.base_url,
                    &ep.api_key,
                    &ep.model,
                    &msgs,
                    &[],
                    effort,
                ),
            )
            .await
            .map_err(|_| anyhow::anyhow!("summarizer model call timed out"))?
            .map(|t| t.content.unwrap_or_default())
        }
    };
    // Optional oracle for self-review: only when `roles.oracle` names a stronger reviewer model;
    // otherwise the loop falls back to nudge-mode.
    let oracle = cli_config::role_configured("oracle")
        .then(|| cli_config::resolve_role("oracle", ep))
        .map(|role| {
            let effort =
                cli_config::role_reasoning_effort("oracle", cli_config::load().reasoning_effort);
            move |msgs: Vec<Message>| {
                let role = role.clone();
                let effort = effort.clone();
                async move {
                    tokio::time::timeout(
                        crate::repl::postturn::chore_call_timeout(),
                        client::chat_with_tools_effort(
                            http,
                            &role.base_url,
                            &role.api_key,
                            &role.model,
                            &msgs,
                            &[],
                            effort,
                        ),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("oracle model call timed out"))?
                    .map(|t| t.content.unwrap_or_default())
                }
            }
        });
    let result = agent::run_agent_loop_full(chat, summarize, oracle, cfg, registry, history).await;
    if result.as_ref().err().is_some_and(|error| {
        agent::tool_error_effect_state(error) != agent::tools::ToolEffectState::Settled
    }) {
        crate::core::recovery::mark_effects_unsettled();
    }
    recovery_boundary.complete(result.is_ok());
    if let Ok(outcome) = &result {
        let streamed = streamed_text.lock().unwrap_or_else(|e| e.into_inner());
        surface_appended_final_text(outcome.final_text.as_deref(), streamed.as_deref());
    }
    result
}

/// The loop may append an honest evidence verdict AFTER the model stream finished. Emit just that
/// tail when the captured response matches; missing/mismatched captures must not hide a final answer.
pub(crate) fn appended_final_text<'a>(
    final_text: Option<&'a str>,
    streamed_text: Option<&str>,
) -> Option<&'a str> {
    let final_text = final_text?;
    let tail = streamed_text
        .and_then(|streamed| final_text.strip_prefix(streamed))
        .unwrap_or(final_text);
    (!tail.trim().is_empty()).then_some(tail)
}

pub(crate) fn surface_appended_final_text(final_text: Option<&str>, streamed_text: Option<&str>) {
    if let Some(tail) = appended_final_text(final_text, streamed_text) {
        tui::emit_line(tail.trim_start_matches('\n'));
    }
}

/// Everything a turn that reached the model must do afterwards, on either surface.
///
/// Ordering is load-bearing and was the thing that drifted: the learning passes read the FULL detail
/// of the turn, so they must run before auto-compaction summarizes it away, and persistence must
/// happen last and unconditionally — a cancelled learning pass is not a reason to lose the
/// transcript.
pub(crate) async fn finish_turn(
    outcome: &AgentOutcome,
    persona_before: Option<String>,
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
    design: &DesignContext,
) {
    design.finalize();
    crate::core::session_store::update_live_history_with_design(history, Some(&design.snapshot()));
    // ABNORMAL STOP, SAID OUT LOUD. The loop can end for reasons that are NOT success — the repair
    // budget ran out with the tree still broken, the step cap was hit mid-task, the model started
    // repeating itself — and in each case it has usually already streamed a confident closing
    // paragraph. Silence here makes those read exactly like `Done`, and the passes below would then
    // file a red tree as a finished task.
    surface_abnormal_stop(outcome);
    // The user's `stop` hooks see every finished top-level turn (see `agent::hooks`).
    crate::agent::hooks::run_blocking(|| {
        crate::agent::hooks::stop(
            outcome.stop.label(),
            outcome.iters,
            outcome.final_text.as_deref(),
            &crate::agent::hooks::Context::here(false),
        )
    });
    // A turn that did not reach `Done` leaves its plan for the next user turn (see `todo`).
    crate::agent::todo::end_turn(!matches!(outcome.stop, StopReason::Done));
    // Goal mode finishes only on a verify-passing `Done`. Clear it here so the next turn is an
    // ordinary capped turn again; Esc leaves the goal armed on purpose, so the user can retry.
    if crate::agent::goal::current_goal().is_some() && matches!(outcome.stop, StopReason::Done) {
        crate::agent::goal::set_goal(None);
        crate::agent::goal::arm(false);
        crate::agent::goal::clear();
        tui::emit_line(
            &style("🎯 goal complete — verified. goal mode off.")
                .color256(splash::ACCENT)
                .to_string(),
        );
    }
    // An EMPTY answer from a SINGLE model call (no tool work, no streamed text) used to vanish
    // silently — a rate limit swallowed into an empty 200, a content filter, or a gateway that
    // streams `[DONE]` with no deltas looked identical to "still idle". `iters <= 1` keeps a turn
    // that DID do tool work and merely ended without a closing sentence from being flagged.
    let empty = outcome
        .final_text
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty();
    if empty && outcome.iters <= 1 {
        tui::emit_line(&format!(
            "{} the model returned an empty response — no text and no tool calls. Likely a rate limit, content filter, or a gateway that closed the stream early. Try again, or /model to switch.",
            theme::warn("⚠ empty reply:")
        ));
    }
    // The agent may have created or switched personas mid-turn (the `persona_create` tool). Resync
    // the system prompt at the turn boundary so the new character is live from the next message —
    // prefix-cache safe, because index 1 is rewritten between turns rather than during one.
    let persona_after = cli_config::load().persona;
    if persona_after != persona_before {
        update_system_prompt(history, &ep.model);
        if let Some(name) = persona_after {
            tui::emit_line(
                &style(format!("🎭 now playing: {name} (from your next message)"))
                    .color256(splash::ACCENT)
                    .to_string(),
            );
        }
    }
    // The learning passes are model calls made after the turn's own token was disarmed, so without
    // re-arming, Esc would take the idle branch while the REPL sat awaiting them: to the user the
    // turn had visibly ended and the app was wedged anyway. Cancelling here skips the remaining
    // learning, which is always optional work.
    // E4.4: the secretary and the persona reflection read the turn and write the store; nothing
    // in the next prompt waits on them except recall, which the next turn's `learning_queue`
    // drain gives `DRAIN_JOIN` to satisfy. So they run in the background on a copy of the turn.
    // Auto-compaction is not a post-turn pass any more: the loop compacts between its own steps
    // (`compact_at_pct`), so a long turn shrinks mid-flight instead of after it ends.
    {
        let turn = last_turn_slice(history).to_vec();
        let http = http.clone();
        let ep = ep.clone();
        crate::repl::learning_queue::LEARNING.spawn(async move {
            maybe_run_secretary(&turn, &http, &ep.base_url, &ep.api_key, &ep.model).await;
            maybe_evolve_persona(&http, &ep.base_url, &ep.api_key, &ep.model).await;
        });
    }
    // Persistence is NOT optional: a cancelled learning pass must still leave the conversation on
    // disk. `autosave_session` names the session with a model call,
    // so it is cancellable too — the local-only writer keeps the transcript either way.
    if cancellable_slash(crate::core::session_store::autosave_session_with_design(
        history,
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
        Some(&design.snapshot()),
    ))
    .await
    .is_none()
    {
        crate::core::session_store::autosave_last_with_design(
            history,
            Some(&ep.model),
            Some(&design.snapshot()),
        );
    }
}

/// Tell the user, unmistakably, when a turn ended for a reason that is NOT success.
///
/// The agent loop can return with the work unfinished or the tree broken, and in those cases the
/// model has usually ALREADY streamed a confident closing paragraph — so silence here means the
/// failure is indistinguishable from `Done`, and the post-turn passes go on to file it as a normal
/// episode and store it as a normal session. That is the one failure mode worth spending screen
/// space on: a wrong answer the user has no reason to doubt.
///
/// Each line names the recovery move, because the state differs: `VerificationFailed` means edits
/// LANDED and the checker never went green (so the tree is the thing to look at), while `MaxIters`
/// and `Divergence` mean the work simply stopped short (so continuing is the move). `Done` prints
/// nothing — the answer already speaks for itself. `Cancelled` / `AwaitingInput` never reach here:
/// their callers own dedicated arms upstream.
fn surface_abnormal_stop(outcome: &AgentOutcome) {
    let line = match &outcome.stop {
        StopReason::Done => return,
        StopReason::VerificationFailed => format!(
            "⚠ edits were made but verification never passed ({} steps). The tree is likely broken \
             — `/diff` to see what changed, `/rewind` to undo, or tell me to keep fixing.",
            outcome.iters
        ),
        // Reaching here now means the loop ALREADY granted itself every continuation it was allowed
        // (see `AgentConfig::max_continuations`) — so this is a genuinely long task, not the old
        // "cut off at step 50" case. Say that, rather than implying one more nudge would have done it.
        StopReason::MaxIters => format!(
            "⚠ ran out of step budget after {} steps, including the automatic continuations — the \
             task may be incomplete. Say \"continue\" to carry on from here.",
            outcome.iters
        ),
        // Both signature loops and evidence-flat exploration reach here. The final synthesis above
        // has already returned the best answer available; this line states why tool use stopped.
        StopReason::Divergence => format!(
            "⚠ stopped after {} steps: recent attempts added no new evidence. The answer above is the \
             best result from the established facts; say \"continue\" to try a different approach.",
            outcome.iters
        ),
        // Both have dedicated arms in every caller (Esc / `clarify` pause), so reaching this is a
        // wiring slip rather than a real state — still say something instead of swallowing it.
        StopReason::Cancelled => format!("⚠ stopped: cancelled after {} step(s).", outcome.iters),
        StopReason::AwaitingInput(ask) => format!("❓ {}", ask.display()),
        // Only reachable if a wall-clock budget was set on this run (no top-level default), so name
        // the knob — otherwise the user cannot tell a deadline from a step limit or a crash.
        StopReason::Deadline => format!(
            "⚠ stopped: wall-clock budget reached after {} step(s) — the task may be incomplete. \
             Say \"continue\" to carry on, or raise AIZEN_SUBAGENT_WALL_SECS.",
            outcome.iters
        ),
        // The model asked for a fresh thread (its final text opened with `REFRESH:`). The REPL
        // handles the actual distill + reseed; reaching here means the caller did not, so surface
        // the request rather than swallow it.
        StopReason::Refresh => format!(
            "⚠ the model asked to refresh the thread after {} step(s) — run /clear to distill a \
             carry-over seed and start fresh.",
            outcome.iters
        ),
    };
    let painted = theme::err(line).to_string();
    if tui::active() {
        tui::emit_line(&painted);
    } else {
        eprintln!("{painted}");
    }
}

/// Render a `clarify` ask prominently and yield to the input box. Under the retained UI the whole
/// structured ask is handed to the answer PANEL (`tui::ask_panel_open`) — a Claude-Code-style picker
/// with a tab per question, checkboxes for multi-select questions, and a free-text row. Use ↑/↓ to
/// choose, Space to toggle multi-select options, Tab to switch questions, and Enter to submit all;
/// unanswered questions keep the panel open. The rendered text of every question and option is ALSO
/// emitted to the transcript, so dismissing the panel (Esc / typing) still leaves the free-text path
/// completely usable. Every other surface (plain
/// REPL, `aizen agent`, Telegram) prints the same rendered text and the user types the answer.
pub(crate) fn show_clarify(ask: &crate::agent::clarify::Ask) {
    let display = ask.display();
    let mut lines = display.lines();
    let head = format!(
        "{} {}",
        style("❓").color256(splash::ACCENT).bold(),
        style(lines.next().unwrap_or("")).bold()
    );
    let rest: Vec<String> = lines
        .map(|l| style(l).color256(splash::ACCENT).to_string())
        .collect();
    let hint = style("↳ answer below to continue").dim().to_string();
    if tui::active() {
        tui::emit_line(&head);
        for o in &rest {
            tui::emit_line(o);
        }
        // Raise the answer panel. The numbered list above STAYS in the transcript: the panel is
        // dismissible (Esc / just start typing), and the options must survive its dismissal for the
        // free-text path. No-ops when there is nothing to pick (a bare question) — the box is enough.
        tui::ask_panel_open(ask);
        if tui::ask_panel_active() {
            tui::emit_line(
                &style(
                    "↳ ↑/↓ choose · Space toggle multi · Tab question · Enter submit all; unanswered stays open",
                )
                .dim()
                .to_string(),
            );
        } else {
            tui::emit_line(&hint);
        }
    } else {
        println!("{head}");
        for o in &rest {
            println!("{o}");
        }
        println!("{hint}");
    }
}

#[cfg(test)]
mod smoke_tests {
    use super::*;

    #[test]
    fn appended_evidence_surfaces_only_the_unstreamed_suffix() {
        let answer = "The requested change is complete.";
        let evidence =
            "Verification: build/test passed; visual Verified.\nEvidence: synthetic test only";
        let final_text = format!("{answer}\n\n{evidence}");
        assert_eq!(
            appended_final_text(Some(&final_text), Some(answer)),
            Some(format!("\n\n{evidence}").as_str())
        );
        assert_eq!(appended_final_text(Some(answer), Some(answer)), None);
        assert_eq!(
            appended_final_text(Some(evidence), Some("")),
            Some(evidence)
        );
        assert_eq!(appended_final_text(Some(answer), None), Some(answer));
        assert_eq!(appended_final_text(None, Some(answer)), None);
        assert_eq!(
            appended_final_text(Some("different answer"), Some(answer)),
            Some("different answer")
        );
        assert_eq!(
            appended_final_text(Some("answer\n\n"), Some("answer")),
            None
        );
        let incomplete =
            format!("{answer}\n\nVisual verification incomplete: no pixels\n{evidence}");
        let tail = appended_final_text(Some(&incomplete), Some(answer)).unwrap();
        assert!(tail.contains("Visual verification incomplete"));
        assert!(!tail.contains(answer));
        assert_eq!(tail.matches("Verification: build/test").count(), 1);
    }

    #[test]
    fn resolved_model_drives_prompt_context_and_harness_without_erasing_history() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-resolved-turn-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let prior_home = std::env::var_os("AIZEN_HOME");
        std::env::set_var("AIZEN_HOME", &home);
        let design = DesignContext::new("test-owner", ".", DesignMode::Off);
        let resolved = ResolvedTurn {
            endpoint: cli_config::ResolvedEndpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: "unused".into(),
                model: "resolved-model".into(),
            },
            harness_effort: Some("max".into()),
            wire_effort: Some("low".into()),
            design: design.clone(),
            architect_plan: None,
            references: None,
            reference_markers: Vec::new(),
            vision_config: cli_config::CliConfig::default(),
        };
        let mut cfg = AgentConfig::default();
        resolved.configure(&mut cfg, true);
        let mut expected = AgentConfig::default();
        expected.apply_effort("max");
        assert_eq!(cfg.max_iters, expected.max_iters);
        assert_eq!(cfg.context_window, resolve_ctx_window("resolved-model").0);
        assert_eq!(cfg.wire_reasoning_effort, Some(Some("low".into())));
        assert_eq!(
            cfg.exec_ctx.design().unwrap().snapshot().owner_id,
            "test-owner"
        );
        let mut history = vec![
            Message::system("old-model-prefix"),
            Message::user("keep this"),
            Message::assistant("and this"),
        ];
        let mut effective_model = "old-model".into();
        resolved.refresh_prompt(&mut history, &mut effective_model);
        assert_eq!(effective_model, "resolved-model");
        assert!(history[0]
            .content
            .as_deref()
            .unwrap()
            .contains("model: resolved-model\n"));
        assert!(history
            .iter()
            .any(|m| m.content.as_deref() == Some("keep this")));
        assert!(history
            .iter()
            .any(|m| m.content.as_deref() == Some("and this")));
        let before = serde_json::to_string(&history).unwrap();
        resolved.refresh_prompt(&mut history, &mut effective_model);
        assert_eq!(
            serde_json::to_string(&history).unwrap(),
            before,
            "unchanged effective model preserves cached lanes"
        );
        match prior_home {
            Some(value) => std::env::set_var("AIZEN_HOME", value),
            None => std::env::remove_var("AIZEN_HOME"),
        }
        let _ = std::fs::remove_dir_all(home);
    }

    fn current_reference_fixture() -> (ResolvedTurn, Message) {
        use crate::core::tool_media::{encode_rgba_png, ImageProvenance, ToolImage};
        let image = ToolImage::from_bytes(
            &encode_rgba_png(&[9, 8, 7, 255], 1, 1).unwrap(),
            "image/png",
            ImageProvenance::new("fixture", None),
        )
        .unwrap();
        let mut vision_config = cli_config::CliConfig::default();
        cli_config::set_vision_support_in(
            &mut vision_config,
            "https://fixture.invalid/v1",
            "fixture-model",
            Some(true),
        )
        .unwrap();
        let turn = ResolvedTurn::from_frozen(
            cli_config::ResolvedEndpoint {
                base_url: "https://fixture.invalid/v1".into(),
                api_key: "fixture-account-a".into(),
                model: "fixture-model".into(),
            },
            None,
            None,
            DesignContext::new("fixture-owner", ".", DesignMode::Off),
            vision_config,
        );
        (
            turn,
            Message::user_with_images("fixture", vec![image.data_url()]),
        )
    }

    #[test]
    fn current_reference_grant_is_available_before_planning_and_does_not_grant_history() {
        let (mut turn, current) = current_reference_fixture();
        let (_, historical) = current_reference_fixture();
        seat_loaded_reference_images(&mut turn, &current.images).unwrap();
        let ids = turn.references.as_ref().unwrap().ids().to_vec();
        let context = turn.execution_context();
        assert_eq!(context.references_checked().unwrap().unwrap().ids(), ids);
        let state = turn.design.snapshot();
        assert!(state.active && state.intent.is_visual());
        assert_eq!(
            state.comparison_request.unwrap().mode,
            crate::agent::design::FidelityMode::Exact
        );
        let mut history = vec![
            historical,
            Message::user_with_images("new task", current.images.clone()),
        ];
        let original_images = history[1].images.clone();
        turn.seat_reference_markers(&mut history);
        assert!(
            history[0].tool_images.is_empty(),
            "same historical pixels do not acquire a grant"
        );
        assert_eq!(
            history[1].images, original_images,
            "canonical originals are not rewritten"
        );
        assert!(history[1]
            .tool_images
            .iter()
            .all(|image| !image.is_available()));
        let content = history[1].content.as_deref().unwrap();
        assert!(content.contains(&ids[0]));
        assert!(!content.contains("data:image/") && !content.contains("fixture-account"));
        let json = serde_json::to_string(&history).unwrap();
        assert!(!json.contains("fixture-account-a"));
        validate_current_reference_route(&turn).unwrap();
        turn.endpoint.api_key = "fixture-account-b".into();
        assert!(validate_current_reference_route(&turn).is_err());
    }

    #[test]
    fn current_reference_grant_does_not_imply_vision_capability() {
        let (mut turn, current) = current_reference_fixture();
        seat_loaded_reference_images(&mut turn, &current.images).unwrap();
        turn.vision_config = cli_config::CliConfig::default();
        let error = validate_current_reference_route(&turn)
            .unwrap_err()
            .to_string();
        assert!(error.contains("vision support is unknown"));
        cli_config::set_vision_support_in(
            &mut turn.vision_config,
            &turn.endpoint.base_url,
            &turn.endpoint.model,
            Some(false),
        )
        .unwrap();
        assert!(validate_current_reference_route(&turn)
            .unwrap_err()
            .to_string()
            .contains("configured without vision"));
        turn.references = None;
        assert!(
            validate_current_reference_route(&turn).is_ok(),
            "ordinary attach-only requests preserve their legacy policy"
        );
    }

    #[test]
    fn explicit_headless_reference_selection_rejects_excess_without_granting() {
        let (mut turn, current) = current_reference_fixture();
        let images =
            vec![current.images[0].clone(); crate::core::tool_media::MAX_REQUEST_IMAGES + 1];
        assert!(seat_loaded_reference_images(&mut turn, &images).is_err());
        assert!(turn
            .execution_context()
            .references_checked()
            .unwrap()
            .is_none());
        assert!(turn.design.snapshot().references.is_empty());
    }

    #[test]
    fn attach_only_markers_are_identity_not_live_reference_authority() {
        let (mut turn, mut current) = current_reference_fixture();
        let registry = crate::core::reference_media::ReferenceRegistry::new("fixture-owner");
        registry
            .register_loaded(std::slice::from_mut(&mut current))
            .unwrap();
        turn.reference_markers = current.tool_images.clone();
        let mut history = vec![Message::user_with_images("attached only", current.images)];
        turn.seat_reference_markers(&mut history);
        assert!(turn
            .execution_context()
            .references_checked()
            .unwrap()
            .is_none());
        assert!(!history[0]
            .content
            .as_deref()
            .unwrap()
            .contains("selected_references"));
        assert!(!history[0].tool_images.is_empty());
        assert!(turn.design.snapshot().references.is_empty());
    }

    #[test]
    fn explicit_omission_does_not_inherit_a_global_parent_override() {
        let cfg = AgentConfig {
            wire_reasoning_effort: Some(None),
            ..Default::default()
        };
        assert_eq!(explicit_wire_effort(&cfg), None);
    }

    /// E6.2 (quality plan Q4): `src/repl/` had no tests at all. One user turn goes through the same
    /// `run_agent_turn` both REPL surfaces call, with the model's answer replayed from a tape, the
    /// real registry and the real loop — no network, no key. It pins the wiring (config → registry
    /// → loop → outcome) that only a person at a terminal used to exercise.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the home + tape locks must span the taped turn
    async fn one_turn_runs_through_the_repl_wiring_on_a_tape() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-repl-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("AIZEN_HOME", &home);
        let _tape_lock = crate::llm::replay::TAPE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tape = home.join("smoke.jsonl");
        // Fingerprints are deliberately bogus: replay warns and carries on (the documented contract
        // for a harness that changed underneath a recording).
        let line = serde_json::json!({
            "ordinal": 0, "model": "synthetic", "system_fp": "x", "turns_fp": "x",
            "turn": {"content": "The answer is 4.", "finish_reason": "stop",
                "usage": {"prompt": 500, "completion": 8, "cached": 0, "cache_write": 0}}
        });
        std::fs::write(&tape, line.to_string() + "\n").unwrap();
        crate::llm::replay::configure(crate::llm::replay::Mode::Replay, &tape, None).unwrap();

        let http = reqwest::Client::new();
        let ep = cli_config::ResolvedEndpoint {
            base_url: "http://127.0.0.1:9/v1".to_string(),
            api_key: "unused".to_string(),
            model: "synthetic".to_string(),
        };
        let registry = build_turn_registry(&http, &ep).expect("registry");
        let cfg = turn_agent_config(crate::core::cancel::TurnCancel::new(), "synthetic", false);
        let mut history = vec![
            Message::system("You are a test."),
            Message::user("What is 2 + 2?"),
        ];
        let outcome = run_agent_turn(&http, &ep, &cfg, &registry, &mut history)
            .await
            .expect("a taped turn completes");
        crate::llm::replay::disable();
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);

        assert_eq!(outcome.final_text.as_deref(), Some("The answer is 4."));
        assert!(
            matches!(outcome.stop, StopReason::Done),
            "{:?}",
            outcome.stop
        );
        assert!(
            history.iter().any(|m| m.role == "assistant"),
            "the assistant turn was appended to the history"
        );
    }
}
