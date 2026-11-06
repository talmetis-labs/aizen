//! Everything that happens AFTER a turn's last token: the secretary pass, passive memory
//! learning, persona evolution, and the `/compact` entry point. Auto-compaction is not here: it
//! runs inside the turn, between the agent's steps (`compact_at_pct` — one trigger, armed by the
//! `/config` threshold, for the REPL and the one-shot alike).
//!
//! All of it is best-effort and bounded — a failed or slow learning pass must leave the conversation
//! exactly as the turn left it, which is why the whole block runs under one overall timeout.

use crate::agent;
use crate::core::endpoint::{http_client, resolve_endpoint};
use crate::core::types::ToolDef;
use crate::core::{cli_config, types};
use crate::llm::client;
use crate::persona;
use crate::skills as skill;
use crate::ui::{icons, splash, tui};
use crate::{
    auto_skill_learn_enabled, extract_json_object, persona_evolve_enabled, summarizer_endpoint,
    COMPACT_KEEP_TURNS,
};
use anyhow::Result;
use console::style;
use types::Message;

/// A short background chore (secretary, persona reflection or reconcile judgement) gets a shorter
/// per-call cap than a sub-agent. Compaction has a separate whole-operation deadline because it may
/// need several bounded transcript portions. `AIZEN_CHORE_CALL_SECS` overrides either default;
/// the sub-agent ceiling still bounds it.
const CHORE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) fn chore_call_timeout() -> std::time::Duration {
    std::env::var("AIZEN_CHORE_CALL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(CHORE_CALL_TIMEOUT)
        .min(crate::agent::task_tool::subagent_call_timeout())
}

/// The most recent logical user turn, including synthetic continuations. Legacy unstamped
/// history keeps its last-user fallback for Core identity learning only.
pub(crate) fn last_turn_slice(history: &[Message]) -> &[Message] {
    Message::logical_turn(history).unwrap_or_else(|| {
        match history.iter().rposition(|m| m.role == "user") {
            Some(i) => &history[i..],
            None => &[],
        }
    })
}

/// Wrap a background / chore model call in a per-call wall-clock deadline
/// ([`chore_call_timeout`], default 60s, `AIZEN_CHORE_CALL_SECS`).
///
/// Every one of these is a NON-streaming `chat_with_tools` call — end-of-turn secretary,
/// persona reflection, memory reconcile, the oracle reviewer and persona distill.
/// None is streamed, so the streaming path's inter-event stall watchdog never applies; the shared
/// client carries no total-request ceiling (removed so a long *streamed* turn isn't cut — see
/// `http_client`); and `read_timeout` resets on every byte, so a gateway that keepalive-drips without
/// ever finishing the body parks the background task (or, for the by-hand ones, the CLI) forever. A
/// flat per-call cap is exactly right here — one call, one answer, no legitimate multi-minute stream.
/// On timeout it returns an ordinary `Err`, which every caller already treats as best-effort.
pub(crate) async fn chore_chat(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
) -> Result<client::ChatTurn> {
    let deadline = chore_call_timeout();
    match tokio::time::timeout(
        deadline,
        client::chat_with_tools(http, base, key, model, msgs, tools),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "chore model call exceeded {}s with no response (raise AIZEN_CHORE_CALL_SECS)",
            deadline.as_secs()
        )),
    }
}

/// Is the `summarizer` role pointed at its OWN endpoint, or does it fall through to the main model?
///
/// Decides the secretary's input ceiling. When it falls through, every chore call bills the model
/// the user is actually coding with — on a large-context model that is the difference between a
/// chore and a real cost — so the transcript is capped much harder.
fn summarizer_is_dedicated(base: &str, key: &str, model: &str) -> bool {
    let ep = summarizer_endpoint(base, key, model);
    ep.model != model || ep.base_url != base
}

/// Core Skill/Persona extraction is independent of Memory flags and provider availability.
pub(crate) async fn maybe_run_secretary(
    history: &[Message],
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    use crate::core::turn_signals;
    use crate::persona::secretary;

    let skills_enabled = auto_skill_learn_enabled();
    let evolve_enabled = persona_evolve_enabled();
    let active_persona = if evolve_enabled {
        persona::active()
    } else {
        None
    };
    let flags = secretary::IdentityLearning {
        skills: skills_enabled,
        persona_evolve: evolve_enabled,
        persona_active: active_persona.is_some(),
    };
    if !flags.enabled() {
        return;
    }
    let turn = last_turn_slice(history);
    let user_text = turn
        .first()
        .and_then(crate::core::memory_context::clean_content)
        .unwrap_or("")
        .trim();
    let tool_calls = turn
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.tool_calls.len())
        .sum();
    let authored_persona = turn_signals::turn_authored_persona(turn.iter().map(|m| {
        (
            m.role.as_str(),
            m.user_turn_id.is_some(),
            m.tool_calls.iter().map(|tc| tc.function.name.as_str()),
        )
    }));
    let recovered = turn_recovered_from_dead_end(turn);
    let reason =
        secretary::should_extract(flags, user_text, tool_calls, recovered, authored_persona);
    if !reason.fires() {
        return;
    }
    let cap = secretary::input_cap(reason, summarizer_is_dedicated(base, key, model));
    let input = secretary::build_input(user_text, &render_transcript(turn), cap);
    let ep = summarizer_endpoint(base, key, model);
    let messages = [
        Message::system(secretary::system_prompt()),
        Message::user(input),
    ];
    let response =
        match chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &messages, &[]).await {
            Ok(response) => response,
            Err(_) => return,
        };
    let output = flags.parse(&response.content.unwrap_or_default());
    if output.is_empty() {
        return;
    }
    // Owners are rechecked after the await; they still enforce validation and formative floors.
    if persona_evolve_enabled() {
        if let (Some(proposal), Some(active)) = (output.episode.as_ref(), persona::active()) {
            let slug = skill::sanitize_name(&active.name);
            let _ = persona::self_mem::record_episode(&slug, &proposal.text, proposal.importance);
        }
    }
    if let Some(proposal) = output.skill.as_ref().filter(|_| auto_skill_learn_enabled()) {
        let all_skills = skill::list();
        let target =
            auto_learn_skill_target(&all_skills, &proposal.name, &proposal.when, &proposal.steps);
        let done = match target {
            Some((key, true)) => {
                proposal.refine
                    && skill::refine(key, &proposal.steps, None, Some(&proposal.when)).is_ok()
            }
            Some((key, false)) => {
                skill::refine(key, &proposal.steps, None, Some(&proposal.when)).is_ok()
            }
            None => skill::save_scoped(&proposal.name, "", &proposal.when, &proposal.steps, true)
                .is_ok(),
        };
        if done {
            let label = if target.is_some() {
                "refined"
            } else {
                "learned"
            };
            let name = target.map(|(key, _)| key).unwrap_or(&proposal.name);
            tui::emit_line(
                &style(format!(
                    "{}{label} skill '{name}' — /skills to view",
                    icons::g(icons::learned())
                ))
                .color256(splash::ACCENT)
                .to_string(),
            );
        }
    }
}

/// Return the install key and whether it is an exact identity match; upstream names are display
/// metadata, so they must not choose the filesystem target of automatic refinement.
fn auto_learn_skill_target<'a>(
    skills: &'a [skill::Skill],
    name: &str,
    when: &str,
    steps: &str,
) -> Option<(&'a str, bool)> {
    use crate::persona::secretary;
    let views: Vec<_> = skills
        .iter()
        .map(|skill| secretary::SkillView {
            key: skill.key(),
            when: &skill.when,
            steps: &skill.body,
        })
        .collect();
    secretary::auto_learn_skill_target(&views, &skill::sanitize_name(name), when, steps)
}

/// Did this turn RECOVER from a dead end — a tool result errored, then a LATER tool result in the
/// same turn succeeded? That recovery is a hard-won procedure worth distilling even on a short turn.
/// Tool errors are fed back as result strings starting with `error:` (the loop's convention).
pub(crate) fn turn_recovered_from_dead_end(turn: &[Message]) -> bool {
    crate::core::turn_signals::recovered_from_dead_end(
        turn.iter()
            .filter(|message| message.role == "tool")
            .map(|message| message.content.as_deref().unwrap_or("")),
    )
}

/// The Core settings flag controls only persistent provider learning, never Skills or Persona.
pub(crate) fn memory_auto_learn_enabled() -> bool {
    cli_config::load().memory_auto_learn.unwrap_or(true)
}

/// After a completed turn: if a persona is active, distill its accumulated episodes into durable
/// character insights when enough formative weight has piled up.
///
/// This used to also RECORD the turn's episode from a regex gate. That half moved into
/// [`maybe_run_secretary`], which already reads the finished turn — two writers meant one formative
/// moment landed twice, once as the gate's templated body and once in the model's own words. What
/// remains is the periodic tier: reflection is about the accumulation, not about this turn, so it
/// needs no `history` at all.
///
/// Best-effort + visible — never disrupts the REPL.
pub(crate) async fn maybe_evolve_persona(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    if !persona_evolve_enabled() {
        return;
    }
    let persona = match persona::active() {
        Some(p) => p,
        None => return, // no character active → nothing to evolve
    };
    let slug = skill::sanitize_name(&persona.name);
    if persona::self_mem::should_reflect(&slug) {
        run_persona_reflection(&persona, &slug, http, base, key, model).await;
    }
}

/// The reflection call: synthesize recent episodes into 1-3 durable insights for this character.
async fn run_persona_reflection(
    persona: &persona::Persona,
    slug: &str,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    let episodes = persona::self_mem::recent_episode_bodies(slug, 20);
    if episodes.len() < persona::self_mem::REFLECT_MIN_EPISODES {
        return;
    }
    let (sys, usr) =
        persona::reflect::build_reflection_prompt(&persona.name, &persona.role, &episodes);
    // Chore-class synthesis call → billed to the summarizer role, like every other harness chore.
    let ep = summarizer_endpoint(base, key, model);
    let resp = match chore_chat(
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
        &[Message::system(sys), Message::user(usr)],
        &[],
    )
    .await
    {
        Ok(t) => t,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    let content = resp.content.unwrap_or_default();
    let json = match extract_json_object(&content) {
        Some(j) => j,
        None => return,
    };
    let insights = persona::reflect::parse_insights(json);
    if insights.is_empty() {
        return;
    }
    let mut saved = 0usize;
    for ins in &insights {
        if let Ok(id) = persona::self_mem::save_insight(slug, &ins.text, ins.importance) {
            saved += 1;
            // Cross-kind Hebbian edge: an insight distilled while these facts were in play is
            // associated with them. Best-effort — a graph write never affects the reflection.
            persona::self_mem::note_insight_cofire(slug, &id);
        }
    }
    if saved > 0 {
        tui::emit_line(
            &style(format!(
                "{}{} reflected — +{saved} insight(s) from recent sessions (/persona to view)",
                icons::g(icons::learned()),
                persona.name
            ))
            .color256(splash::ACCENT)
            .to_string(),
        );
    }
}

/// Render conversation messages into a compact transcript (delegates to the shared compaction core).
pub(crate) fn render_transcript(msgs: &[Message]) -> String {
    agent::compact::render_transcript(msgs)
}

/// Summarize older turns to free context. Thin wrapper over [`agent::compact::compact_history`] that
/// supplies the shared bounded, NON-streaming summarizer for this session's route. Returns
/// (tokens_before, tokens_after). Same core the agent loop uses, so the REPL and `aizen serve` compact
/// identically.
pub(crate) async fn compact_history(
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) -> Result<(usize, usize)> {
    let sum_ep = summarizer_endpoint(base, key, model);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move { crate::llm::compact_summary::summarize(http, &ep, msgs).await }
    };
    // The shared core strips recall before the model call; keep manual failures transactional too.
    let mut staged = history.clone();
    let out = agent::compact::compact_history(&mut staged, summarize, COMPACT_KEEP_TURNS).await;
    if out.is_ok() {
        *history = staged;
        // The recorded real context size predates the cut — let the estimate carry the meter until
        // the next model call reports usage for the compacted history.
        tui::clear_ctx_real_tokens();
    }
    out
}

/// Commit a manual compact before reporting success: other clients can only resume disk history.
async fn compact_live_with<F, Fut>(
    history: &mut Vec<Message>,
    model: &str,
    design: Option<&crate::agent::design::TaskDesignState>,
    compact: F,
) -> Result<(usize, usize)>
where
    F: FnOnce(Vec<Message>) -> Fut,
    Fut: std::future::Future<Output = Result<(Vec<Message>, (usize, usize))>>,
{
    use crate::core::session_store;
    let saved = session_store::current_session_slug()
        .map(|slug| {
            session_store::read_session_for_edit(&slug)
                .map(|((_, meta, _), revision)| (slug, meta.unwrap_or_default(), revision))
        })
        .transpose()?;
    // Allocate from the original first turn, not the tail that survives compaction.
    let name = saved
        .as_ref()
        .map(|(slug, _, _)| slug.clone())
        .unwrap_or_else(|| session_store::allocate_session_slug(history));
    let (staged, counts) = compact(history.clone()).await?;
    if let Some((_, meta, revision)) = saved {
        session_store::write_session_if_unchanged(&staged, &name, meta, design, &revision)?;
    } else {
        session_store::save_session_with_design(&staged, &name, Some(model), design)?;
    }
    *history = staged;
    session_store::set_session_slug(Some(name));
    session_store::update_live_history_with_design(history, design);
    tui::clear_ctx_real_tokens();
    Ok(counts)
}

/// `/compact` — summarize, then durably replace the same saved session before returning success.
pub(crate) async fn compact_now(
    history: &mut Vec<Message>,
    model_label: &str,
    design: Option<&crate::agent::design::TaskDesignState>,
) -> Result<(usize, usize)> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    compact_live_with(history, model_label, design, |mut staged| async move {
        let counts = compact_history(&mut staged, &http, &base, &key, &model).await?;
        Ok((staged, counts))
    })
    .await
}

/// `/clear` carry-over — distill the WHOLE current conversation into one dense seed note (routed
/// through the summarizer role, like compaction) so a refresh starts a fresh thread that still
/// REMEMBERS what the previous one was doing. Returns the distilled text; the caller rebuilds the
/// thread and seeds it behind [`agent::compact::HANDOFF_MARKER_PREFIX`]. `Err` when there is nothing
/// worth carrying or the model returns nothing — the caller then falls back to a plain wipe.
pub(crate) async fn refresh_distill_now(history: &[Message]) -> Result<String> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    if history.len() < 2 {
        anyhow::bail!("nothing to carry forward — the conversation is empty");
    }
    let ep = summarizer_endpoint(&base, &key, &model);
    let prompt = agent::compact::carryover_prompt(history);
    let summary = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &prompt, &[])
        .await?
        .content
        .unwrap_or_default();
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty carry-over summary");
    }
    Ok(summary.trim().to_string())
}

#[cfg(test)]
mod compact_persistence_tests {
    use super::*;
    use crate::core::{config::EnvGuard, session_store::*};

    #[tokio::test]
    #[expect(
        clippy::await_holding_lock,
        reason = "isolated session home and replay state across compact"
    )]
    async fn manual_compact_is_durable_before_success_and_transactional_on_failure() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _tape = crate::llm::replay::TAPE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "aizen-live-compact-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let env = EnvGuard::set([
            ("AIZEN_HOME", root.as_os_str()),
            ("AIZEN_PROJECT_ROOT", root.as_os_str()),
        ]);
        let previous = current_session_slug();
        let mut history = vec![Message::system("fixture instructions")];
        for i in 0..6 {
            history.push(Message::user(format!(
                "Original topic {i}: {}",
                "older detail ".repeat(80)
            )));
            history.push(Message::assistant(format!("Answer {i}")));
        }
        save_session(&history, "same-session", Some("historical-model")).unwrap();
        set_session_slug(Some("same-session".into()));
        let path = sessions_dir().join("same-session.json");
        let before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let counts = compact_live_with(
            &mut history,
            "current-model",
            None,
            |mut staged| async move {
                let counts = agent::compact::compact_history(
                    &mut staged,
                    |_| async { Ok("Saved compact summary".into()) },
                    COMPACT_KEEP_TURNS,
                )
                .await?;
                Ok((staged, counts))
            },
        )
        .await
        .unwrap();
        assert!(counts.0 > counts.1);
        assert_eq!(current_session_slug().as_deref(), Some("same-session"));
        let (saved, meta, _) = read_session_path_with_design(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&saved).unwrap(),
            serde_json::to_value(&history).unwrap()
        );
        assert!(saved.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Saved compact summary"))));
        assert_eq!(meta.unwrap().model.as_deref(), Some("historical-model"));
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_ne!(before["generation"], after["generation"]);
        assert_eq!(before["meta"]["created"], after["meta"]["created"]);

        let unchanged = serde_json::to_value(&history).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let error = compact_live_with(&mut history, "current-model", None, |_| async {
            anyhow::bail!("summary failed")
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("summary failed"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(serde_json::to_value(&history).unwrap(), unchanged);

        let conflict = compact_live_with(&mut history, "current-model", None, |staged| async {
            save_session(
                &[Message::user("concurrent writer")],
                "same-session",
                Some("other"),
            )
            .unwrap();
            Ok((staged, (1000, 100)))
        })
        .await;
        assert!(
            conflict.is_err(),
            "do not overwrite an external save during summarization"
        );
        assert_eq!(serde_json::to_value(&history).unwrap(), unchanged);
        let (disk, _, _) = read_session_path_with_design(&path).unwrap();
        assert_eq!(disk[0].content.as_deref(), Some("concurrent writer"));

        set_session_slug(previous);
        drop(env);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod learning_tests {
    use super::*;

    #[test]
    fn the_last_turn_slice_starts_at_the_last_user_message() {
        let h = vec![
            Message::user("one".to_string()),
            Message::assistant("a".to_string()),
            Message::user("two".to_string()),
            Message::assistant("b".to_string()),
        ];
        let t = last_turn_slice(&h);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].content.as_deref(), Some("two"));
        assert!(last_turn_slice(&[]).is_empty());
        assert!(last_turn_slice(&[Message::assistant("x".to_string())]).is_empty());
    }

    #[test]
    fn logical_turn_keeps_synthetic_continuations_until_the_next_real_user() {
        let mut user = Message::user("Remember that this project uses Cargo.");
        user.user_turn_id = Some(types::UserTurnId::new().unwrap());
        let mut history = vec![
            Message::user("legacy turn"),
            user,
            Message::assistant("Working on it."),
            Message::user("Internal continuation, not a new user request."),
            Message::assistant("Done."),
        ];
        let turn = last_turn_slice(&history);
        assert_eq!(turn.len(), 4);
        assert_eq!(
            turn[0].content.as_deref(),
            Some("Remember that this project uses Cargo.")
        );
        assert!(turn[0].user_turn_id.is_some());
        assert!(turn[2].user_turn_id.is_none());
        let mut next = Message::user("New real request.");
        next.user_turn_id = Some(types::UserTurnId::new().unwrap());
        history.push(next);
        assert_eq!(last_turn_slice(&history).len(), 1);
        assert_eq!(
            last_turn_slice(&history)[0].content.as_deref(),
            Some("New real request.")
        );
    }

    #[test]
    fn auto_learn_refines_install_keys_not_upstream_display_names() {
        let mut aliased = skill::parse_markdown(
            "---\nname: upstream\nwhen: verify the build\n---\nRun cargo test before release.\n",
            "local-alias",
        );
        aliased.slug = "local-alias".into();
        let mut other = skill::parse_markdown(
            "---\nname: local-alias\nwhen: deploy documentation\n---\nPublish the documentation site.\n",
            "another-copy",
        );
        other.slug = "another-copy".into();
        let skills = [other, aliased];
        assert_eq!(
            auto_learn_skill_target(&skills, "local-alias", "", ""),
            Some(("local-alias", true))
        );
        assert_eq!(
            auto_learn_skill_target(&skills, "upstream", &skills[1].when, &skills[1].body),
            Some(("local-alias", false))
        );
        assert_eq!(
            auto_learn_skill_target(&skills, "another-copy", "", ""),
            Some(("another-copy", true))
        );
        assert_eq!(auto_learn_skill_target(&skills, "upstream", "", ""), None);
    }

    #[test]
    fn the_chore_cap_never_exceeds_the_subagent_ceiling() {
        assert!(chore_call_timeout() <= crate::agent::task_tool::subagent_call_timeout());
        assert!(chore_call_timeout() <= CHORE_CALL_TIMEOUT);
    }
}
