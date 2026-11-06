//! Noninteractive `/compact` for saved conversations (including Desktop callers).

use crate::core::endpoint::{http_client, resolve_endpoint_with_model_fallback};
use crate::core::session_store::{read_session_for_edit, write_session_if_unchanged};
use crate::core::types::Message;
use anyhow::Result;
use std::future::Future;

#[derive(clap::Args, Debug)]
pub(crate) struct CompactArgs {
    /// Exact saved-session slug, without the .json extension (not a path).
    pub(crate) slug: String,
    /// Main model override; the configured summarizer role is still honored.
    #[arg(long, value_name = "MODEL")]
    pub(crate) model: Option<String>,
    #[arg(long, value_name = "URL")]
    pub(crate) base_url: Option<String>,
    #[arg(long, value_name = "KEY", hide_env_values = true)]
    pub(crate) api_key: Option<String>,
    /// JSON writes exactly one object; before/after are approximate token counts.
    #[arg(long, value_enum, default_value = "text")]
    pub(crate) output_format: OutputFormat,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default)]
pub(crate) enum OutputFormat {
    Json,
    #[default]
    Text,
}

#[derive(serde::Serialize)]
struct CompactResult {
    slug: String,
    before: usize,
    after: usize,
}

pub(crate) async fn run(args: CompactArgs) -> Result<()> {
    let result = compact_saved_with(&args.slug, |mut history, saved_model| async move {
        let (base, key, model) = resolve_endpoint_with_model_fallback(
            args.base_url,
            args.api_key,
            args.model,
            saved_model,
        )?;
        let http = http_client()?;
        // Same role routing and bounded summary operation as `/compact`.
        let counts =
            crate::repl::postturn::compact_history(&mut history, &http, &base, &key, &model)
                .await
                // Provider error bodies may echo request credentials. Never print them from this
                // automation surface; the unchanged session is the only durable result of a failure.
                .map_err(|error| anyhow::anyhow!(compact_failure_message(&error)))?;
        Ok((history, counts))
    })
    .await?;
    match args.output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string(&result)?),
        OutputFormat::Text => println!(
            "Compacted {}: ~{} -> ~{} tokens",
            result.slug, result.before, result.after
        ),
    }
    Ok(())
}

/// Preserve the cause without exposing a provider body (which can echo private input or keys).
fn compact_failure_message(error: &anyhow::Error) -> &'static str {
    let detail = format!("{error:#}");
    if detail.contains("empty summary") {
        return "compaction failed: summarizer returned an empty summary; session unchanged";
    }
    if detail.contains("chore model call exceeded") || detail.contains("compaction exceeded") {
        return "compaction failed: summarizer timed out; session unchanged (check AIZEN_CHORE_CALL_SECS)";
    }
    match crate::llm::client::classify_api_error(&anyhow::anyhow!(detail)) {
        crate::llm::client::ApiErrorKind::SignedOut =>
            "compaction failed: summarizer authentication rejected; session unchanged",
        crate::llm::client::ApiErrorKind::ContextOverflow =>
            "compaction failed: summarizer context window exceeded; session unchanged",
        crate::llm::client::ApiErrorKind::Permanent =>
            "compaction failed: summarizer rejected the request; check endpoint/model settings; session unchanged",
        crate::llm::client::ApiErrorKind::Transient =>
            "compaction failed: summarizer connection or response failed; session unchanged",
    }
}

/// The closure owns a private copy. Failure (even after mutation), a short history, or a stale
/// revision cannot write any session bytes. Metadata/design come from disk, never today's cwd.
async fn compact_saved_with<F, Fut>(slug: &str, compact: F) -> Result<CompactResult>
where
    F: FnOnce(Vec<Message>, Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<Message>, (usize, usize))>>,
{
    let ((history, meta, design), revision) = read_session_for_edit(slug)?;
    anyhow::ensure!(
        crate::agent::compact::plan_compact_cut_at(
            &history,
            crate::COMPACT_KEEP_TURNS,
            crate::agent::compact::KEEP_STEPS
        )
        .is_some(),
        "conversation too short to compact"
    );
    let meta = meta.unwrap_or_default();
    let (history, (before, after)) = compact(history, meta.model.clone()).await?;
    write_session_if_unchanged(&history, slug, meta, design.as_ref(), &revision)?;
    Ok(CompactResult {
        slug: slug.to_string(),
        before,
        after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{config, session_store::*};
    use clap::{CommandFactory, Parser};
    use serde_json::json;

    fn with_pool(f: impl FnOnce()) {
        let _guard = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-compact-test-{}", std::process::id()));
        let previous = std::env::var_os("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        std::fs::create_dir_all(sessions_dir()).unwrap();
        f();
        match previous {
            Some(value) => std::env::set_var("AIZEN_HOME", value),
            None => std::env::remove_var("AIZEN_HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    fn history() -> Vec<Message> {
        let mut h = vec![Message::system("original project system lane")];
        for n in 0..6 {
            h.push(Message::user(format!(
                "turn {n}: {}",
                "detail ".repeat(300)
            )));
            h.push(Message::assistant("done".to_string()));
        }
        h
    }

    async fn fake_compact(
        mut h: Vec<Message>,
        _: Option<String>,
    ) -> Result<(Vec<Message>, (usize, usize))> {
        let counts = crate::agent::compact::compact_history(
            &mut h,
            |_| async { Ok("User goal, decisions and open work".to_string()) },
            crate::COMPACT_KEEP_TURNS,
        )
        .await?;
        Ok((h, counts))
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn bytes(slug: &str, extension: &str) -> Vec<u8> {
        std::fs::read(sessions_dir().join(format!("{slug}.{extension}"))).unwrap()
    }

    #[test]
    fn compact_diagnostics_preserve_causes_but_never_echo_provider_bodies() {
        let cases = [
            (
                "upstream returned HTTP 401: private-fixture-body",
                "authentication rejected",
            ),
            (
                "upstream returned HTTP 400: context_length_exceeded private-fixture-body",
                "context window exceeded",
            ),
            (
                "upstream returned HTTP 404: private-fixture-body",
                "rejected the request",
            ),
            (
                "request failed after retries: private-fixture-body",
                "connection or response failed",
            ),
            (
                "the model returned an empty summary: private-fixture-body",
                "empty summary",
            ),
            (
                "chore model call exceeded 60s private-fixture-body",
                "timed out",
            ),
            ("compaction exceeded 300s private-fixture-body", "timed out"),
            (
                "the model returned an empty summary during compaction: private-fixture-body",
                "empty summary",
            ),
        ];
        for (raw, expected) in cases {
            let error = anyhow::anyhow!(raw).context("summarization call failed");
            let message = compact_failure_message(&error);
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("session unchanged"));
            assert!(!message.contains("private-fixture-body"));
        }
    }

    #[test]
    fn compact_saved_supports_the_same_single_turn_shape_as_interactive_compact() {
        with_pool(|| {
            let mut h = vec![Message::system("SYS"), Message::user("one long task")];
            for step in 0..10 {
                h.push(Message::assistant(format!("step {step}")));
            }
            write_session(&h, "single", SessionMeta::default()).unwrap();
            let result = block_on(compact_saved_with("single", fake_compact)).unwrap();
            assert_eq!(result.slug, "single");
            let (restored, _, _) =
                read_session_path_with_design(&sessions_dir().join("single.json")).unwrap();
            assert!(restored
                .iter()
                .any(|m| m.content.as_deref() == Some("one long task")));
            assert!(restored.iter().any(|m| m
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with(crate::agent::compact::COMPACT_MARKER_PREFIX)));
            assert_eq!(restored.last().unwrap().content.as_deref(), Some("step 9"));
        });
    }

    #[test]
    fn compact_cli_parses_and_is_advertised() {
        let cli = crate::cli_args::Cli::try_parse_from([
            "aizen",
            "compact",
            "session-1",
            "--model",
            "chosen",
            "--base-url",
            "https://example.invalid/v1",
            "--api-key",
            "test-placeholder",
            "--output-format",
            "json",
        ])
        .unwrap();
        let Some(crate::cli_args::Commands::Compact(args)) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.slug, "session-1");
        assert_eq!(args.model.as_deref(), Some("chosen"));
        assert!(matches!(args.output_format, OutputFormat::Json));
        assert!(crate::cli_args::Cli::command()
            .render_long_help()
            .to_string()
            .contains("compact"));
        assert!(crate::cli_args::Cli::try_parse_from(["aizen", "compact"]).is_err());
        assert!(crate::cli_args::Cli::try_parse_from([
            "aizen",
            "compact",
            "x",
            "--output-format",
            "yaml"
        ])
        .is_err());
        let default = crate::cli_args::Cli::try_parse_from(["aizen", "compact", "x"]).unwrap();
        assert!(matches!(
            default.command,
            Some(crate::cli_args::Commands::Compact(CompactArgs {
                output_format: OutputFormat::Text,
                ..
            }))
        ));
    }

    #[test]
    fn compact_saved_preserves_metadata_design_and_replaces_delta_generation() {
        with_pool(|| {
            let design = crate::agent::design::DesignContext::new(
                "owner",
                "original-project",
                crate::agent::design::DesignMode::On,
            )
            .snapshot();
            let meta = SessionMeta {
                project_key: Some("original-key".into()),
                project_root: Some("original-root".into()),
                project_slug: Some("original-project".into()),
                model: Some("historical-model".into()),
                created: Some("2020-01-01T00:00:00Z".into()),
                ..Default::default()
            };
            write_session_with_design(&history(), "saved", meta, Some(&design)).unwrap();
            let mut file: serde_json::Value =
                serde_json::from_slice(&bytes("saved", "json")).unwrap();
            file["meta"]["usage"] =
                json!({"epoch":999,"last_seq":7,"calls":7,"input":123,"output":45});
            std::fs::write(
                sessions_dir().join("saved.json"),
                serde_json::to_vec(&file).unwrap(),
            )
            .unwrap();
            let old_generation = file["generation"].clone();
            let delta = json!({"role":"assistant","content":"committed delta", "_session_generation":old_generation});
            let delta_bytes = format!("{delta}\n");
            std::fs::write(sessions_dir().join("saved.jsonl"), &delta_bytes).unwrap();
            let result = block_on(compact_saved_with("saved", fake_compact)).unwrap();
            assert!(result.before > result.after);
            let new: serde_json::Value = serde_json::from_slice(&bytes("saved", "json")).unwrap();
            for key in [
                "project_key",
                "project_root",
                "project_slug",
                "model",
                "created",
            ] {
                assert_eq!(new["meta"][key], file["meta"][key]);
            }
            assert!(new["meta"]["usage"]["calls"].as_u64().unwrap() >= 7);
            assert!(new["meta"]["usage"]["input"].as_u64().unwrap() >= 123);
            assert_eq!(new["design_state"], file["design_state"]);
            assert_eq!(new["messages"][0], file["messages"][0]);
            assert_ne!(new["generation"], old_generation);
            assert!(!sessions_dir().join("saved.jsonl").exists());
            // A delayed append by a stale process cannot replay against the new envelope.
            std::fs::write(sessions_dir().join("saved.jsonl"), delta_bytes).unwrap();
            let (loaded, _, _) =
                read_session_path_with_design(&sessions_dir().join("saved.json")).unwrap();
            assert_eq!(loaded.len(), new["messages"].as_array().unwrap().len());
            let json = serde_json::to_value(result).unwrap();
            assert_eq!(json.as_object().unwrap().len(), 3);
            assert_eq!(json["slug"], "saved");
        });
    }

    #[test]
    fn compact_failure_short_invalid_and_missing_do_not_write() {
        with_pool(|| {
            write_session(&history(), "saved", SessionMeta::default()).unwrap();
            let before = bytes("saved", "json");
            let failed = block_on(compact_saved_with("saved", |mut h, _| async move {
                h.clear();
                anyhow::bail!("injected failure")
            }));
            assert!(failed.is_err());
            assert_eq!(bytes("saved", "json"), before);
            let empty = block_on(compact_saved_with("saved", |mut h, _| async move {
                let counts = crate::agent::compact::compact_history(
                    &mut h,
                    |_| async { Ok(String::new()) },
                    crate::COMPACT_KEEP_TURNS,
                )
                .await?;
                Ok((h, counts))
            }));
            assert!(empty.err().unwrap().to_string().contains("empty summary"));
            assert_eq!(bytes("saved", "json"), before);
            write_session(
                &[Message::system("lane"), Message::user("short")],
                "short",
                SessionMeta::default(),
            )
            .unwrap();
            let short = bytes("short", "json");
            let error = block_on(compact_saved_with("short", |_, _| async {
                panic!("must not call model")
            }))
            .err()
            .unwrap();
            assert!(error.to_string().contains("too short"));
            assert_eq!(bytes("short", "json"), short);
            for slug in [
                "../saved",
                "..\\saved",
                "saved.json",
                "",
                "CON",
                "saved/child",
            ] {
                assert!(block_on(compact_saved_with(slug, fake_compact)).is_err());
            }
            assert!(block_on(compact_saved_with("missing", fake_compact)).is_err());
            assert!(!sessions_dir().join("missing.json").exists());
        });
    }

    #[test]
    fn compact_rejects_envelope_and_delta_changes_during_summary() {
        with_pool(|| {
            for extension in ["json", "jsonl"] {
                write_session(&history(), "saved", SessionMeta::default()).unwrap();
                let concurrent = b"concurrent edit";
                let result = block_on(compact_saved_with("saved", |h, model| async move {
                    std::fs::write(
                        sessions_dir().join(format!("saved.{extension}")),
                        concurrent,
                    )
                    .unwrap();
                    fake_compact(h, model).await
                }));
                assert!(result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("session changed"));
                assert_eq!(bytes("saved", extension), concurrent);
            }
        });
    }
}
