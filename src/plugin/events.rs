//! Generation-bound completed-turn events. This is host orchestration, not fact extraction.

use super::{context, manifest, memory_api as api, runtime};
use anyhow::{ensure, Context, Result};
use api::Validate;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Captured from the route that executes this turn, never resolved from a background role.
/// Deliberately has no Debug/Serialize implementation: credentials remain only in Core memory.
pub(crate) struct TurnModel {
    endpoint: crate::core::cli_config::ResolvedEndpoint,
    effort: Option<String>,
    config: crate::core::cli_config::CliConfig,
}

impl TurnModel {
    pub(crate) fn new(
        endpoint: &crate::core::cli_config::ResolvedEndpoint,
        effort: Option<String>,
        config: &crate::core::cli_config::CliConfig,
    ) -> Self {
        Self {
            endpoint: endpoint.clone(),
            effort,
            config: config.clone(),
        }
    }
}

pub(crate) struct CompletedTurn {
    context: api::ProviderContext,
    params: api::ProcessTurn,
    execution: crate::core::exec_ctx::ExecutionContext,
    cancel: crate::core::cancel::TurnCancel,
    notes_owner: Option<std::sync::Arc<super::snapshots::NotesOwner>>,
    model: Option<TurnModel>,
    tool_calls: u32,
    recovered: bool,
}

/// Capture selection BEFORE queueing background work. A later switch/disable cannot redirect an
/// old transcript to a different executable. No provider means no event, process or data directory.
pub(crate) fn prepare(
    stop: &crate::agent::StopReason,
    history: &[crate::core::types::Message],
    root: &Path,
    execution: crate::core::exec_ctx::ExecutionContext,
    cancel: crate::core::cancel::TurnCancel,
    snapshot: Option<&super::snapshots::Snapshot>,
    model: Option<TurnModel>,
) -> Result<Option<CompletedTurn>> {
    // Enforce this once for every surface, before reading config or reserving an outbox slot.
    // Skill/Persona learning has its own policy and is deliberately not gated here.
    if !matches!(stop, crate::agent::StopReason::Done)
        || learning_live(&execution, &cancel).is_err()
    {
        return Ok(None);
    }
    if !crate::repl::postturn::memory_auto_learn_enabled() {
        return Ok(None);
    }
    let state = super::manager::read_state()?;
    if !state.enabled
        || !state
            .selected
            .as_ref()
            .and_then(|id| state.packages.get(id))
            .and_then(|package| package.grant.as_ref())
            .is_some_and(|grant| grant.learning)
    {
        return Ok(None);
    }
    let Some(turn) = crate::core::types::Message::logical_turn(history) else {
        // Legacy/imported transcripts cannot reliably distinguish real requests from harness nudges.
        return Ok(None);
    };
    let user = turn
        .first()
        .and_then(crate::core::memory_context::clean_content)
        .unwrap_or("")
        .trim();
    if user.is_empty() {
        return Ok(None);
    }
    if crate::core::turn_signals::turn_authored_persona(turn.iter().map(|message| {
        (
            message.role.as_str(),
            message.user_turn_id.is_some(),
            message
                .tool_calls
                .iter()
                .map(|call| call.function.name.as_str()),
        )
    })) {
        return Ok(None);
    }
    let session = execution.conversation().to_string();
    let event = turn[0]
        .user_turn_id
        .as_ref()
        .expect("logical turn has an identity")
        .as_str();
    let Some(context) = context::selected(root, &session, event, &session)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        context.generation == state.generation,
        "Memory selection changed while preparing learning event"
    );
    if !context
        .granted_capabilities
        .contains(&api::Capability::Learning)
    {
        return Ok(None);
    }
    // The provider owns gates/prompts/decisions. Core only bounds what can leave its process.
    let user = bounded_text(user);
    let transcript = bounded_text(&crate::repl::postturn::render_transcript(turn));
    let operation_id = format!(
        "turn-{}",
        manifest::sha256(
            format!(
                "{}:{}:{}:{}",
                context.generation, context.workspace_id, session, event
            )
            .as_bytes()
        )
    );
    let mut params = api::ProcessTurn {
        operation_id,
        user_text: user,
        transcript,
        dry_run: false,
        confirmed: false,
        payload_sha256: None,
        rendered_evidence: None,
    };
    params.payload_sha256 = Some(manifest::sha256(&params.payload_bytes(&context)?));
    params.validate()?;
    learning_live(&execution, &cancel)?;
    if !super::outbox::prepare(&context, &params)? {
        return Ok(None);
    }
    let notes_owner = snapshot.and_then(|snapshot| snapshot.learning_owner(&context));
    let model = model.filter(|_| {
        state
            .selected
            .as_ref()
            .and_then(|id| state.packages.get(id))
            .and_then(|package| package.grant.as_ref())
            .is_some_and(|grant| grant.turn_model_learning)
    });
    let tool_calls = turn
        .iter()
        .filter(|message| message.role == "assistant")
        .map(|message| message.tool_calls.len())
        .sum::<usize>()
        .min(100_000) as u32;
    let recovered = crate::repl::postturn::turn_recovered_from_dead_end(turn);
    Ok(Some(CompletedTurn {
        context,
        params,
        execution,
        cancel,
        notes_owner,
        model,
        tool_calls,
        recovered,
    }))
}

fn bounded_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .take(8192)
        .collect()
}

fn learning_live(
    execution: &crate::core::exec_ctx::ExecutionContext,
    cancel: &crate::core::cancel::TurnCancel,
) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "turn cancelled before Memory learning"
    );
    ensure!(
        execution
            .deadline()
            .is_none_or(|deadline| deadline > std::time::Instant::now()),
        "Memory learning deadline elapsed"
    );
    Ok(())
}

impl CompletedTurn {
    async fn wait_model<T>(
        &self,
        timeout: Duration,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        learning_live(&self.execution, &self.cancel)?;
        let timeout = self.execution.deadline().map_or(timeout, |deadline| {
            timeout.min(deadline.saturating_duration_since(std::time::Instant::now()))
        });
        ensure!(!timeout.is_zero(), "Memory learning deadline elapsed");
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => anyhow::bail!("turn cancelled during Memory model request; no retry"),
            result = tokio::time::timeout(timeout, future) => {
                result.context("same-turn Memory model request timed out; no retry")?
            },
        };
        learning_live(&self.execution, &self.cancel)?;
        result
    }
}

pub(crate) async fn dispatch(event: CompletedTurn) -> Result<()> {
    learning_live(&event.execution, &event.cancel)?;
    super::outbox::claim(&event.context, &event.params)?;
    let result = dispatch_inner(&event).await;
    match result {
        Ok(receipt) => {
            receipt.validate()?;
            ensure!(
                receipt.operation_id == event.params.operation_id
                    && receipt.payload_sha256 == event.params.payload_sha256,
                "Memory learning result binding mismatch"
            );
            super::outbox::finish(&event.params, Some(receipt.clone()))?;
            super::snapshots::adopt_candidates(
                event.notes_owner.as_ref(),
                &event.context,
                &receipt.session_candidates,
            )
        }
        Err(error) => {
            // An error is never a retry signal. The physical runtime's independent recovery
            // barrier remains authoritative when dispatch might have had effects.
            let _ = super::outbox::finish(&event.params, None);
            Err(error)
        }
    }
}

async fn offline(event: &CompletedTurn) -> Result<api::ProcessTurnResult> {
    learning_live(&event.execution, &event.cancel)?;
    let value = runtime::request(
        runtime::ApprovedInvocation::saved_learning(),
        event.context.clone(),
        api::Method::ProcessTurn,
        serde_json::to_value(&event.params)?,
        Some(event.execution.clone()),
        event.cancel.clone(),
    )
    .await?;
    Ok(serde_json::from_value(value)?)
}

async fn dispatch_inner(event: &CompletedTurn) -> Result<api::ProcessTurnResult> {
    learning_live(&event.execution, &event.cancel)?;
    let Some(model) = &event.model else {
        return offline(event).await;
    };
    let (id, _) = super::manager::selected_at(event.context.generation)?;
    let package = super::manager::info(&id)?;
    ensure!(
        package.manifest.permissions.host_llm
            && package
                .manifest
                .capabilities
                .iter()
                .any(|capability| capability == "host_llm"),
        "selected provider does not declare same-turn model jobs"
    );
    let mut context = event.context.clone();
    context.granted_capabilities.push(api::Capability::HostLlm);
    context
        .settings
        .insert("prepared_job".into(), serde_json::json!(true));
    context.validate()?;
    let request = api::PrepareJob {
        operation_id: format!("{}-prepare", event.params.operation_id),
        turn: event.params.clone(),
        tool_calls: event.tool_calls,
        recovered: event.recovered,
    };
    request.validate()?;
    let value = runtime::request(
        runtime::ApprovedInvocation::saved_learning(),
        context.clone(),
        api::Method::PrepareJob,
        serde_json::to_value(&request)?,
        Some(event.execution.clone()),
        event.cancel.clone(),
    )
    .await?;
    let prepared: api::PrepareJobResult = serde_json::from_value(value)?;
    prepared.validate()?;
    let Some(job) = prepared.job else {
        // A provider-owned gate skipped model extraction. Offline learning still needs its own
        // journaled operation; no model is silently substituted when a request fails.
        return offline(event).await;
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    ensure!(
        job.expires_unix_secs > now && job.expires_unix_secs <= now.saturating_add(600),
        "Memory prepared job expiry is invalid"
    );
    let completion = {
        // Pin selection/consent through network dispatch. Admin transitions fail busy rather than
        // revoke a generation while its conversation is being sent. Release before the next RPC.
        let _admission = super::manager::Admission::acquire()?;
        let (_, state) = super::manager::selected_at(context.generation)?;
        let grant = state.grant.as_ref().context("Memory consent disappeared")?;
        ensure!(
            grant.learning
                && grant.turn_model_learning
                && crate::repl::postturn::memory_auto_learn_enabled(),
            "same-turn model learning was disabled before model dispatch"
        );
        let remaining = job
            .expires_unix_secs
            .saturating_sub(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
        let mut timeout = Duration::from_secs(remaining).min(Duration::from_secs(60));
        if let Some(deadline) = event.execution.deadline() {
            timeout = timeout.min(deadline.saturating_duration_since(std::time::Instant::now()));
        }
        ensure!(!timeout.is_zero(), "Memory learning deadline elapsed");
        let messages = [
            crate::core::types::Message::system(&job.system),
            crate::core::types::Message::user(&job.input),
        ];
        event
            .wait_model(
                timeout,
                crate::llm::client::extension_completion(
                    &model.endpoint,
                    model.effort.clone(),
                    &model.config,
                    &messages,
                    job.max_output_tokens,
                ),
            )
            .await?
    };
    let complete = api::CompleteJob {
        operation_id: event.params.operation_id.clone(),
        prepare_operation_id: request.operation_id,
        job_id: job.id,
        payload_sha256: prepared.payload_sha256,
        completion,
    };
    complete.validate()?;
    let value = runtime::request(
        runtime::ApprovedInvocation::saved_learning(),
        context,
        api::Method::CompleteJob,
        serde_json::to_value(&complete)?,
        Some(event.execution.clone()),
        event.cancel.clone(),
    )
    .await?;
    Ok(serde_json::from_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued_event(
        root: &Path,
        execution: crate::core::exec_ctx::ExecutionContext,
        cancel: crate::core::cancel::TurnCancel,
    ) -> CompletedTurn {
        let context: api::ProviderContext = serde_json::from_value(serde_json::json!({
            "generation":1,"user_id":"fixture-user","workspace_id":"fixture-workspace",
            "workspace_root":root,"cwd":root,"device_id":"fixture-device","device_aliases":[],
            "session_id":"fixture-session","turn_id":"fixture-turn","boundary_id":"fixture-boundary",
            "data_root":root.join("data"),"settings":{},"granted_capabilities":["learning"],
            "budgets":{"context_tokens":100,"recall_tokens":100,"max_records":10}
        }))
        .unwrap();
        let mut params = api::ProcessTurn {
            operation_id: "cancelled-turn-fixture".into(),
            user_text: "Remember this project uses Cargo.".into(),
            transcript: String::new(),
            dry_run: false,
            confirmed: false,
            payload_sha256: None,
            rendered_evidence: None,
        };
        params.payload_sha256 = Some(manifest::sha256(&params.payload_bytes(&context).unwrap()));
        CompletedTurn {
            context,
            params,
            execution,
            cancel,
            notes_owner: None,
            model: None,
            tool_calls: 0,
            recovered: false,
        }
    }

    #[test]
    fn cancelled_or_expired_done_turns_skip_config_and_outbox_reservation() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = crate::core::file_placement::tests::TestDir::new();
        let old_home = std::env::var_os("AIZEN_HOME");
        std::env::set_var("AIZEN_HOME", home.path());
        let config = home.path().join("cli-config.json");
        std::fs::write(&config, b"{invalid-owned-fixture").unwrap();
        let cancelled = crate::core::cancel::TurnCancel::new();
        cancelled.cancel();
        let execution = crate::core::exec_ctx::ExecutionContext::default();
        let history = [crate::core::types::Message::user("Remember Cargo.")];
        let results = [
            prepare(
                &crate::agent::StopReason::Done,
                &history,
                home.path(),
                execution.clone(),
                cancelled,
                None,
                None,
            ),
            prepare(
                &crate::agent::StopReason::Done,
                &history,
                home.path(),
                execution.with_deadline(Some(std::time::Instant::now())),
                crate::core::cancel::TurnCancel::new(),
                None,
                None,
            ),
        ];
        if let Some(old) = old_home {
            std::env::set_var("AIZEN_HOME", old);
        } else {
            std::env::remove_var("AIZEN_HOME");
        }
        assert!(results.into_iter().all(|result| matches!(result, Ok(None))));
        assert_eq!(std::fs::read(config).unwrap(), b"{invalid-owned-fixture");
        assert!(!home.path().join("plugin-state").exists());
        assert!(!home.path().join("memory-data").exists());
    }

    #[test]
    fn queued_learning_observes_original_turn_cancel_and_deadline_before_claim() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = crate::core::file_placement::tests::TestDir::new();
        let old_home = std::env::var_os("AIZEN_HOME");
        std::env::set_var("AIZEN_HOME", home.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let parent = crate::core::cancel::TurnCancel::new();
        let unrelated = crate::core::cancel::TurnCancel::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let event = queued_event(
            home.path(),
            crate::core::exec_ctx::ExecutionContext::default().with_deadline(Some(deadline)),
            parent.clone(),
        );
        assert!(event.cancel.same_turn(&parent));
        assert_eq!(event.execution.deadline(), Some(deadline));
        parent.cancel();
        assert!(!unrelated.is_cancelled());
        let cancelled_error = runtime.block_on(dispatch(event)).unwrap_err();
        let expired = queued_event(
            home.path(),
            crate::core::exec_ctx::ExecutionContext::default()
                .with_deadline(Some(std::time::Instant::now())),
            unrelated,
        );
        let expired_error = runtime.block_on(dispatch(expired)).unwrap_err();
        if let Some(old) = old_home {
            std::env::set_var("AIZEN_HOME", old);
        } else {
            std::env::remove_var("AIZEN_HOME");
        }
        assert!(format!("{cancelled_error:#}").contains("cancelled before Memory learning"));
        assert!(format!("{expired_error:#}").contains("Memory learning deadline elapsed"));
        assert!(!home.path().join("plugin-state").exists());
        assert!(!home.path().join("memory-data").exists());
    }

    #[tokio::test]
    async fn model_wait_cancels_in_flight_and_rejects_a_racing_completion() {
        let parent = crate::core::cancel::TurnCancel::new();
        let event = queued_event(
            &std::env::temp_dir(),
            crate::core::exec_ctx::ExecutionContext::default(),
            parent.clone(),
        );
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let request = event.wait_model(Duration::from_secs(2), async {
            started.send(()).unwrap();
            let _ = receiver.await;
            Ok(())
        });
        let cancel = async {
            waiting.await.unwrap();
            parent.cancel();
        };
        let (result, ()) = tokio::join!(request, cancel);
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled during Memory model"));
        assert!(sender.is_closed(), "cancel must drop the in-flight future");

        let parent = crate::core::cancel::TurnCancel::new();
        let event = queued_event(
            &std::env::temp_dir(),
            crate::core::exec_ctx::ExecutionContext::default(),
            parent.clone(),
        );
        let result = event
            .wait_model(Duration::from_secs(2), async {
                parent.cancel();
                Ok("completion that raced cancellation")
            })
            .await;
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled before Memory learning"));
    }

    #[tokio::test]
    async fn model_wait_clamps_to_the_original_execution_deadline() {
        let event = queued_event(
            &std::env::temp_dir(),
            crate::core::exec_ctx::ExecutionContext::default()
                .with_deadline(Some(std::time::Instant::now() + Duration::from_millis(100))),
            crate::core::cancel::TurnCancel::new(),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            event.wait_model(
                Duration::from_secs(60),
                std::future::pending::<Result<()>>(),
            ),
        )
        .await
        .expect("the inherited deadline must not be refreshed to sixty seconds");
        assert!(format!("{:#}", result.unwrap_err()).contains("timed out; no retry"));
    }

    #[test]
    fn learning_route_is_owned_and_never_follows_later_route_changes() {
        let mut endpoint = crate::core::cli_config::ResolvedEndpoint {
            base_url: "https://original.invalid/v1".into(),
            api_key: "fixture-original".into(),
            model: "chat-model".into(),
        };
        let mut config = crate::core::cli_config::CliConfig {
            max_tokens: Some(512),
            ..Default::default()
        };
        let model = TurnModel::new(&endpoint, Some("high".into()), &config);
        endpoint.base_url = "https://other.invalid/v1".into();
        endpoint.api_key = "fixture-other".into();
        endpoint.model = "summarizer-model".into();
        config.max_tokens = Some(9999);
        assert_eq!(model.endpoint.base_url, "https://original.invalid/v1");
        assert_eq!(model.endpoint.api_key, "fixture-original");
        assert_eq!(model.endpoint.model, "chat-model");
        assert_eq!(model.effort.as_deref(), Some("high"));
        assert_eq!(model.config.max_tokens, Some(512));
        assert_ne!(model.endpoint.model, endpoint.model);
        assert_ne!(model.config.max_tokens, config.max_tokens);
    }

    #[test]
    fn persona_authoring_with_synthetic_continuation_never_reserves_learning() {
        use super::super::install::{FilePin, InstallPins};
        use super::super::settings::{ExecutionGrant, MemoryPlugins};
        use crate::core::types::{FunctionCall, Message, ToolCall, UserTurnId};

        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = crate::core::file_placement::tests::TestDir::new();
        let old_home = std::env::var_os("AIZEN_HOME");
        std::env::set_var("AIZEN_HOME", home.path());
        let mut state = MemoryPlugins::default();
        state
            .record_install(InstallPins {
                schema_version: 1,
                id: "fixture".into(),
                target: "x86_64-pc-windows-msvc".into(),
                manifest: FilePin {
                    path: "plugin.json".into(),
                    bytes: 1,
                    sha256: "a".repeat(64),
                    unix_mode: None,
                },
                files: vec![],
            })
            .unwrap();
        state.set_eligible("fixture", true).unwrap();
        state
            .select(
                "fixture",
                ExecutionGrant {
                    manifest_sha256: "a".repeat(64),
                    guarded_execution: true,
                    network: false,
                    learning: true,
                    turn_model_learning: false,
                },
            )
            .unwrap();
        std::fs::write(
            home.path().join("cli-config.json"),
            serde_json::to_vec(&serde_json::json!({
                "memory_auto_learn": true, "memory_plugins": state,
            }))
            .unwrap(),
        )
        .unwrap();
        let mut user = Message::user("Remember this fictional character's preferences.");
        user.user_turn_id = Some(UserTurnId::new().unwrap());
        let mut history = vec![
            user,
            Message::assistant_tool_calls(vec![ToolCall {
                id: "fixture-persona".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "persona_create".into(),
                    arguments: "{}".into(),
                },
            }]),
            Message::user("Internal continuation, not a new request."),
            Message::assistant("Done."),
        ];
        let execution = crate::core::exec_ctx::ExecutionContext::default();
        let skipped = prepare(
            &crate::agent::StopReason::Done,
            &history,
            home.path(),
            execution.clone(),
            crate::core::cancel::TurnCancel::new(),
            None,
            None,
        );
        let mut next = Message::user("Remember the real project uses Cargo.");
        next.user_turn_id = Some(UserTurnId::new().unwrap());
        history.push(next);
        // A new canonical user must get past the Persona guard and validate the missing package.
        let next_result = prepare(
            &crate::agent::StopReason::Done,
            &history,
            home.path(),
            execution,
            crate::core::cancel::TurnCancel::new(),
            None,
            None,
        );
        if let Some(old) = old_home {
            std::env::set_var("AIZEN_HOME", old);
        } else {
            std::env::remove_var("AIZEN_HOME");
        }
        assert!(matches!(skipped, Ok(None)));
        assert!(next_result.is_err());
        assert!(!home.path().join("plugin-state").exists());
        assert!(!home.path().join("memory-data").exists());
    }

    #[test]
    fn incomplete_turns_never_read_selection_or_reserve_learning_receipts() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let home = crate::core::file_placement::tests::TestDir::new();
        let old_home = std::env::var_os("AIZEN_HOME");
        std::env::set_var("AIZEN_HOME", home.path());
        let config = home.path().join("cli-config.json");
        std::fs::write(&config, b"{invalid-owned-fixture").unwrap();
        let mut user = crate::core::types::Message::user("Remember this project uses Cargo.");
        user.user_turn_id = Some(crate::core::types::UserTurnId::new().unwrap());
        let execution = crate::core::exec_ctx::ExecutionContext::default();
        let history = [user];
        let results: Vec<_> = [
            crate::agent::StopReason::Cancelled,
            crate::agent::StopReason::Deadline,
            crate::agent::StopReason::MaxIters,
            crate::agent::StopReason::Divergence,
            crate::agent::StopReason::VerificationFailed,
            crate::agent::StopReason::Refresh,
            crate::agent::StopReason::AwaitingInput(crate::agent::clarify::Ask {
                questions: vec![],
            }),
        ]
        .iter()
        .map(|stop| {
            prepare(
                stop,
                &history,
                home.path(),
                execution.clone(),
                crate::core::cancel::TurnCancel::new(),
                None,
                None,
            )
        })
        .collect();
        let completed = prepare(
            &crate::agent::StopReason::Done,
            &history,
            home.path(),
            execution,
            crate::core::cancel::TurnCancel::new(),
            None,
            None,
        );
        if let Some(old) = old_home {
            std::env::set_var("AIZEN_HOME", old);
        } else {
            std::env::remove_var("AIZEN_HOME");
        }
        assert!(results.into_iter().all(|result| matches!(result, Ok(None))));
        assert!(
            completed.is_err(),
            "Done must still validate saved selection"
        );
        assert!(!home.path().join("plugin-state").exists());
        assert_eq!(std::fs::read(config).unwrap(), b"{invalid-owned-fixture");
    }
}
