//! REPL Memory commands use the same provider manager and async actor as CLI/tools.
use super::{context, memory_api as api, runtime};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub(crate) async fn remember(text: &str) -> Result<String> {
    invoke_in(
        &std::env::current_dir()?,
        None,
        api::Method::Store,
        remember_params(text)?,
    )
    .await
}

fn remember_params(text: &str) -> Result<Value> {
    let text = text.trim();
    let (scope, body) = text
        .strip_prefix("global:")
        .map_or(("project", text), |body| ("user", body.trim()));
    ensure!(!body.is_empty(), "empty Memory content");
    let name = format!("fact-{}", &super::manifest::sha256(body.as_bytes())[..16]);
    Ok(json!({"content":body,"scope":scope,"metadata":{"name":name,"type":"reference"}}))
}

pub(crate) async fn slash(arg: &str) -> Result<String> {
    slash_in(arg, &std::env::current_dir()?, None).await
}

pub(crate) async fn slash_in(
    arg: &str,
    root: &std::path::Path,
    execution: Option<crate::core::exec_ctx::ExecutionContext>,
) -> Result<String> {
    let (sub, rest) = split(arg);
    if matches!(sub, "" | "status") {
        ensure!(rest.is_empty(), "usage: /memory status");
        return Ok(serde_json::to_string_pretty(&super::manager::status()?)?);
    }
    if matches!(sub, "enable" | "disable" | "use") {
        ensure!(
            execution.is_none() && root == std::env::current_dir()?,
            "Memory activation is host-global; use the local Aizen CLI to change selection or enablement"
        );
        match sub {
            "enable" | "disable" => {
                ensure!(rest.is_empty(), "usage: /memory {sub}");
                super::manager::set_memory_enabled(sub == "enable")?;
                return Ok(if sub == "enable" {
                    "Memory enabled for the previously selected provider."
                } else {
                    "Memory disabled; selection and data preserved."
                }
                .into());
            }
            _ => anyhow::bail!("explicit executable consent is configured with `aizen memory use <id> --help`; installation alone never activates Memory"),
        }
    }
    let (method, params) = operation(arg)?;
    invoke_in(root, execution, method, params).await
}

fn split(arg: &str) -> (&str, &str) {
    let arg = arg.trim();
    arg.split_once(char::is_whitespace)
        .map_or((arg, ""), |(sub, rest)| (sub, rest.trim()))
}

fn record_id(rest: &str, usage: &str) -> Result<()> {
    ensure!(
        !rest.is_empty() && !rest.chars().any(char::is_whitespace),
        "usage: {usage}"
    );
    Ok(())
}

// Hosted management actions accept exact safe IDs, never display names, indexes or paths.
// The provider still validates its schema and sealed human management authority.
fn management_id(id: &str, usage: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 256
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "usage: {usage}; use an exact provider-owned id"
    );
    Ok(())
}

fn operation(arg: &str) -> Result<(api::Method, Value)> {
    let (sub, rest) = split(arg);
    let command = |name: &str, arguments: Value| {
        (
            api::Method::Command,
            json!({"command":name,"arguments":arguments}),
        )
    };
    Ok(match sub {
        "remember" | "add" => (api::Method::Store, remember_params(rest)?),
        "list" | "ls" => {
            let words: Vec<_> = rest.split_whitespace().collect();
            let mut scope = "current";
            let mut scope_seen = false;
            let mut superseded = false;
            let mut words = words.into_iter();
            while let Some(word) = words.next() {
                match word {
                    "--superseded" if !superseded => superseded = true,
                    "--scope" if !scope_seen => {
                        scope = words.next().context(
                            "usage: /memory list [scope | --scope <scope>] [--superseded]",
                        )?;
                        ensure!(!scope.starts_with('-'), "invalid Memory scope selector");
                        scope_seen = true;
                    }
                    value if !scope_seen && !value.starts_with('-') => {
                        scope = value;
                        scope_seen = true;
                    }
                    _ => anyhow::bail!(
                        "usage: /memory list [scope | --scope <scope>] [--superseded]"
                    ),
                }
            }
            let mut params = json!({"limit":50,"scope":scope});
            if superseded {
                params["superseded"] = json!(true);
            }
            (api::Method::List, params)
        }
        "show" | "cat" => {
            record_id(rest, "/memory show <id>")?;
            (api::Method::Get, json!({"id":rest}))
        }
        "edit" | "update" => {
            let (id, body) = rest
                .split_once(char::is_whitespace)
                .context("usage: /memory edit <id> <body>")?;
            ensure!(!body.trim().is_empty(), "usage: /memory edit <id> <body>");
            (api::Method::Update, json!({"id":id,"content":body.trim()}))
        }
        "forget" | "rm" => {
            record_id(rest, "/memory forget <id>")?;
            (api::Method::Forget, json!({"id":rest}))
        }
        "profile" => {
            ensure!(
                matches!(rest, "" | "--json"),
                "usage: /memory profile [--json]"
            );
            command("profile", json!({"json":rest == "--json"}))
        }
        "style" | "archive" => {
            ensure!(rest.is_empty(), "usage: /memory {sub}");
            command(sub, json!({}))
        }
        "frozen" => {
            ensure!(
                matches!(rest, "" | "--rebuild"),
                "usage: /memory frozen [--rebuild]"
            );
            command("frozen", json!({"rebuild":rest == "--rebuild"}))
        }
        "ask" => {
            ensure!(!rest.is_empty(), "usage: /memory ask <question>");
            command("ask", json!({"question":rest,"json":false}))
        }
        "as-of" => {
            record_id(rest, "/memory as-of YYYY-MM-DD")?;
            command("as_of", json!({"date":rest}))
        }
        "review" => {
            let words: Vec<_> = rest.split_whitespace().collect();
            let mut args = json!({"promote":null,"drop":null,"clear":false});
            match words.as_slice() {
                [] => {}
                ["--clear"] => args["clear"] = json!(true),
                [action @ ("--promote" | "--drop"), id] => {
                    management_id(
                        id,
                        "/memory review [--promote <id> | --drop <id> | --clear]",
                    )?;
                    args[action.trim_start_matches('-')] = json!(id);
                }
                _ => {
                    anyhow::bail!("usage: /memory review [--promote <id> | --drop <id> | --clear]")
                }
            }
            command("review", args)
        }
        "revive" => {
            management_id(rest, "/memory revive <id>")?;
            command("revive", json!({"id":rest}))
        }
        "supersede" => {
            let words: Vec<_> = rest.split_whitespace().collect();
            let [old, new] = words.as_slice() else {
                anyhow::bail!("usage: /memory supersede <old-id> <new-id>");
            };
            management_id(old, "/memory supersede <old-id> <new-id>")?;
            management_id(new, "/memory supersede <old-id> <new-id>")?;
            ensure!(old != new, "cannot supersede a record with itself");
            command("supersede", json!({"old":old,"new":new}))
        }
        "restore" => {
            let words: Vec<_> = rest.split_whitespace().collect();
            let (id, as_id) = match words.as_slice() {
                [id] => (*id, None),
                [id, "--as", target] => {
                    management_id(target, "/memory restore <id> [--as <new-id>]")?;
                    (*id, Some(*target))
                }
                _ => anyhow::bail!("usage: /memory restore <id> [--as <new-id>]"),
            };
            management_id(id, "/memory restore <id> [--as <new-id>]")?;
            command("restore", json!({"id":id,"as_id":as_id}))
        }
        "purge" => {
            let words: Vec<_> = rest.split_whitespace().collect();
            let id = match words.as_slice() {
                [id, "--yes"] | ["--yes", id] => *id,
                _ => anyhow::bail!(
                    "usage: /memory purge <archived-id> --yes; deletion is irreversible"
                ),
            };
            management_id(id, "/memory purge <archived-id> --yes")?;
            command("purge", json!({"id":id,"confirmed":true}))
        }
        "compact" | "doctor" | "model-list" => {
            ensure!(rest.is_empty(), "usage: /memory {sub}");
            command(
                if sub == "model-list" {
                    "model_list"
                } else {
                    sub
                },
                json!({}),
            )
        }
        "consolidate" => {
            ensure!(
                matches!(rest, "" | "--apply"),
                "usage: /memory consolidate [--apply]"
            );
            command("consolidate", json!({"apply":rest == "--apply"}))
        }
        "neighbors" => {
            let words: Vec<_> = rest.split_whitespace().collect();
            let (id, limit) = match words.as_slice() {
                [id] => (*id, 10),
                [id, "--k" | "-k", limit] => (
                    *id,
                    limit
                        .parse::<usize>()
                        .context("neighbor count must be 1..=100")?,
                ),
                _ => anyhow::bail!("usage: /memory neighbors <id> [-k <1..=100>]"),
            };
            management_id(id, "/memory neighbors <id> [-k <1..=100>]")?;
            ensure!((1..=100).contains(&limit), "neighbor count must be 1..=100");
            command("neighbors", json!({"id":id,"limit":limit}))
        }
        "model-download" => anyhow::bail!(
            "Mnemos is offline and refuses downloads; provision local weights explicitly"
        ),
        "reconcile" => {
            let (apply, rest) = rest
                .strip_prefix("--apply ")
                .map_or((false, rest), |rest| (true, rest.trim()));
            let path = if rest.is_empty() {
                None
            } else {
                let path = rest
                    .strip_prefix("--reply ")
                    .context("usage: /memory reconcile [--apply] [--reply <file>]")?
                    .trim();
                ensure!(!path.is_empty(), "reconcile reply path is required");
                Some(std::path::Path::new(path))
            };
            command(
                "reconcile",
                crate::cli::memory_cmd::reconcile_arguments(apply, path)?,
            )
        }
        _ => (api::Method::Search, json!({"query":arg.trim(),"limit":5})),
    })
}

async fn invoke_in(
    root: &std::path::Path,
    execution: Option<crate::core::exec_ctx::ExecutionContext>,
    method: api::Method,
    mut params: Value,
) -> Result<String> {
    let event = format!(
        "manual-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    );
    let session = match &execution {
        Some(ctx) => ctx.conversation().to_string(),
        None if root == std::env::current_dir()? => {
            crate::core::session_store::current_session_slug()
                .unwrap_or_else(|| format!("repl-{}", std::process::id()))
        }
        None => event.clone(),
    };
    let context = context::selected(root, &session, &event, &session)?.context(
        "Memory unavailable or disabled; explicitly install and select a provider first",
    )?;
    if matches!(
        method,
        api::Method::Store | api::Method::Update | api::Method::Forget | api::Method::Command
    ) {
        params["operation_id"] = json!(event);
    }
    let cancel = crate::core::cancel::TurnCancel::new();
    if matches!(method, api::Method::Update | api::Method::Forget) {
        let value = runtime::request(
            runtime::ApprovedInvocation::explicit_cli(),
            context.clone(),
            api::Method::Get,
            json!({"id":params.get("id")}),
            execution.clone(),
            cancel.clone(),
        )
        .await?;
        let previous: api::RecordResult = serde_json::from_value(value)?;
        params["expected_revision"] = json!(previous.record.revision);
    }
    let value = runtime::request(
        runtime::ApprovedInvocation::explicit_cli(),
        context,
        method,
        params,
        execution,
        cancel,
    )
    .await?;
    Ok(serde_json::to_string_pretty(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_commands_use_typed_arguments_not_a_generic_id() {
        for (input, expected) in [
            (
                "profile",
                json!({"command":"profile","arguments":{"json":false}}),
            ),
            (
                " profile --json ",
                json!({"command":"profile","arguments":{"json":true}}),
            ),
            ("style", json!({"command":"style","arguments":{}})),
            ("archive", json!({"command":"archive","arguments":{}})),
            (
                "frozen",
                json!({"command":"frozen","arguments":{"rebuild":false}}),
            ),
            (
                "frozen --rebuild",
                json!({"command":"frozen","arguments":{"rebuild":true}}),
            ),
            (
                "ask Which package manager?",
                json!({"command":"ask","arguments":{"question":"Which package manager?","json":false}}),
            ),
            (
                "as-of 2026-10-30",
                json!({"command":"as_of","arguments":{"date":"2026-10-30"}}),
            ),
            (
                "review",
                json!({"command":"review","arguments":{"promote":null,"drop":null,"clear":false}}),
            ),
            (
                "restore actual-archive-handle",
                json!({"command":"restore","arguments":{"id":"actual-archive-handle","as_id":null}}),
            ),
        ] {
            let (method, params) = operation(input).unwrap();
            assert_eq!(method.name(), "command", "{input}");
            assert_eq!(params, expected, "{input}");
        }
    }

    #[test]
    fn manual_records_preserve_scope_ids_and_replacement_text() {
        let (method, params) = operation(" add global: Use Cargo. ").unwrap();
        assert_eq!(method.name(), "store");
        assert_eq!(params, remember_params("global: Use Cargo.").unwrap());
        assert_eq!(params["scope"], "user");
        assert_eq!(remember_params("Use Cargo.").unwrap()["scope"], "project");
        for (input, method, params) in [
            (
                "list",
                api::Method::List,
                json!({"limit":50,"scope":"current"}),
            ),
            (
                "ls current",
                api::Method::List,
                json!({"limit":50,"scope":"current"}),
            ),
            (
                "list --scope all --superseded",
                api::Method::List,
                json!({"limit":50,"scope":"all","superseded":true}),
            ),
            (
                "list --superseded user",
                api::Method::List,
                json!({"limit":50,"scope":"user","superseded":true}),
            ),
            ("cat exact-id", api::Method::Get, json!({"id":"exact-id"})),
            (
                " update exact-id  replacement\ntext ",
                api::Method::Update,
                json!({"id":"exact-id","content":"replacement\ntext"}),
            ),
            ("rm exact-id", api::Method::Forget, json!({"id":"exact-id"})),
            (
                " package manager ",
                api::Method::Search,
                json!({"query":"package manager","limit":5}),
            ),
        ] {
            let actual = operation(input).unwrap();
            assert_eq!(actual.0.name(), method.name(), "{input}");
            assert_eq!(actual.1, params, "{input}");
        }
    }

    #[test]
    fn explicit_management_commands_match_strict_provider_schemas() {
        for (input, name, arguments) in [
            (
                "review --promote exact-id",
                "review",
                json!({"promote":"exact-id","drop":null,"clear":false}),
            ),
            (
                "review --drop exact-id",
                "review",
                json!({"promote":null,"drop":"exact-id","clear":false}),
            ),
            (
                "review --clear",
                "review",
                json!({"promote":null,"drop":null,"clear":true}),
            ),
            ("revive exact-id", "revive", json!({"id":"exact-id"})),
            (
                "supersede old-id new-id",
                "supersede",
                json!({"old":"old-id","new":"new-id"}),
            ),
            (
                "restore archived-id --as restored-id",
                "restore",
                json!({"id":"archived-id","as_id":"restored-id"}),
            ),
            (
                "purge archived-id --yes",
                "purge",
                json!({"id":"archived-id","confirmed":true}),
            ),
            (
                "purge --yes archived-id",
                "purge",
                json!({"id":"archived-id","confirmed":true}),
            ),
            ("compact", "compact", json!({})),
            ("consolidate", "consolidate", json!({"apply":false})),
            ("consolidate --apply", "consolidate", json!({"apply":true})),
            ("reconcile", "reconcile", json!({"apply":false})),
            ("doctor", "doctor", json!({})),
            (
                "neighbors exact-id",
                "neighbors",
                json!({"id":"exact-id","limit":10}),
            ),
            (
                "neighbors exact-id -k 1",
                "neighbors",
                json!({"id":"exact-id","limit":1}),
            ),
            (
                "neighbors exact-id --k 100",
                "neighbors",
                json!({"id":"exact-id","limit":100}),
            ),
            ("model-list", "model_list", json!({})),
        ] {
            let (method, params) = operation(input).unwrap();
            assert_eq!(method.name(), "command", "{input}");
            assert_eq!(
                params,
                json!({"command":name,"arguments":arguments}),
                "{input}"
            );
        }
    }

    #[test]
    fn malformed_known_commands_are_not_silently_searched() {
        for input in [
            "remember",
            "add global:",
            "show",
            "show one two",
            "edit id",
            "edit id   ",
            "forget",
            "profile extra",
            "style extra",
            "archive extra",
            "frozen extra",
            "ask",
            "as-of",
            "list one two",
            "list --scope",
            "list --scope all current",
            "list --scope --superseded",
            "list --superseded --superseded",
            "review --promote",
            "review --promote id --clear",
            "review --drop one --promote two",
            "review --promote Display Name",
            "review --drop ../id",
            "review --clear extra",
            "revive",
            "revive one two",
            "supersede id",
            "supersede id id",
            "supersede ../old new",
            "restore",
            "restore one two",
            "restore one --as ../target",
            "purge id",
            "purge id --yes extra",
            "purge ../id --yes",
            "compact extra",
            "consolidate --yes",
            "consolidate --apply extra",
            "doctor extra",
            "neighbors",
            "neighbors id -k 0",
            "neighbors id --k 101",
            "neighbors id -k nope",
            "neighbors id --limit 10",
            "model-list extra",
            "model-download",
            "reconcile --apply",
        ] {
            assert!(operation(input).is_err(), "{input}");
        }
    }

    #[test]
    fn hosted_activation_is_refused_before_config_or_provider_access() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("missing-home");
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for command in ["enable", "disable", "use mnemos"] {
            let error = runtime
                .block_on(slash_in(
                    command,
                    &fixture.path().join("missing-workspace"),
                    Some(crate::core::exec_ctx::ExecutionContext::default()),
                ))
                .unwrap_err();
            assert!(error.to_string().contains("host-global"), "{error:#}");
        }
        assert!(
            !home.exists(),
            "refused hosted controls must not create state"
        );
    }

    #[test]
    fn hosted_status_is_offline_without_a_selected_provider() {
        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("missing-home");
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for command in ["", "status", " status "] {
            let status: Value = serde_json::from_str(
                &runtime
                    .block_on(slash_in(
                        command,
                        &fixture.path().join("missing-workspace"),
                        Some(crate::core::exec_ctx::ExecutionContext::default()),
                    ))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(status["state"], "unavailable");
            assert_eq!(status["installed"], 0);
            assert_eq!(status["enabled"], false);
        }
        assert!(
            !home.exists(),
            "offline status must not initialize provider state"
        );
    }
}
