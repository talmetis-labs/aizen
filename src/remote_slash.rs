//! Commands on an explicitly shared topic. The host selects the workspace, executable and argv.
use crate::{
    assistants::transfer,
    core::session_store,
    features::{commands, slash},
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

struct Report(&'static str, &'static str, &'static [&'static str]);
const REPORTS: &[Report] = &[
    Report("where", "", &["where"]),
    Report("reach", "", &["reach", "status"]),
    Report("timemachine", "", &["time", "history"]),
    Report("time", "history", &["time", "history"]),
    Report("time", "branches", &["time", "branches"]),
    Report("time", "worktrees", &["time", "worktrees"]),
    Report("work", "list", &["work", "list"]),
    Report("skills", "list", &["skill", "list"]),
    Report("agents", "list", &["agents", "list"]),
    Report("files", "", &["files"]),
    Report("files", "artifacts", &["files", "artifacts"]),
    Report("files", "scratch", &["files", "scratch"]),
    Report("work", "", &["work", "list"]),
    Report("team", "", &["team", "status"]),
    Report("team", "status", &["team", "status"]),
    Report("skills", "", &["skill", "list"]),
    Report("agents", "", &["agents", "list"]),
    Report("mcp", "", &["mcp", "list"]),
    Report("apps", "", &["apps", "list"]),
    Report("time", "", &["time", "status"]),
    Report("sandbox", "", &["sandbox", "status"]),
    Report("init", "--status", &["init", "--status"]),
    Report("plugins", "", &["plugin", "list"]),
    Report("memory", "status", &["memory", "status"]),
    Report("persona", "", &["persona", "list"]),
    Report("changes", "", &["team", "diff"]),
    Report("diff", "", &["team", "diff"]),
];
const SESSION: &[&str] = &[
    "model", "context", "tokens", "memory", "plan", "tools", "approval", "browser",
];
fn custom(root: &Path) -> Vec<commands::CustomCommand> {
    let private = root.join(".aizen");
    let local = if private.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
        // A linked project configuration is outside the granted workspace.
        return Vec::new();
    } else {
        private.join("commands")
    };
    commands::list_from(local, true)
        .into_iter()
        .filter(|c| {
            slash::resolve(&c.name).is_none()
                && c.name.len() <= 128
                && c.name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
        })
        .collect()
}
pub(crate) fn menu(root: &Path, after: &str) -> Value {
    let mut reports = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for Report(name, arg, _) in REPORTS {
        reports.entry(name).or_default().push(arg);
    }
    for name in SESSION {
        reports.entry(name).or_default().push("");
    }
    // A page must fit both the VPS relay and the Desktop reply envelope.
    let registry = custom(root);
    let mut entries = registry
        .iter()
        .filter(|c| c.name.as_str() > after)
        .peekable();
    for name in ["plan", "approval", "browser"] {
        reports.entry(name).or_default().push("status");
    }
    let customs: Vec<_> = entries.by_ref().take(24).map(|c| json!({
        "name": c.name, "source": c.source, "description": c.description.chars().take(200).collect::<String>(),
        "hint": c.argument_hint.chars().take(100).collect::<String>(), "local": needs_local_expansion(c),
    })).collect();
    let next = entries
        .peek()
        .and_then(|_| customs.last())
        .and_then(|v| v["name"].as_str());
    json!({"version":1,"reports":reports,"custom":customs,"next":next})
}
fn needs_local_expansion(c: &commands::CustomCommand) -> bool {
    c.body.contains("!`")
        || c.body
            .split_whitespace()
            .any(|s| s.starts_with('@') && s.len() > 1)
}
async fn report(root: &Path, argv: &[&str]) -> Result<String> {
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .current_dir(root)
        .args(argv)
        .env("NO_COLOR", "1")
        .env("AIZEN_NO_UPDATE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("no command output")?
            .take(48_001)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 48_000, "report exceeds the web output limit");
        ensure!(
            child.wait().await?.success(),
            "Core could not complete this command"
        );
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.trim().to_string())
    })
    .await
    .context("Core command timed out")?
}
pub(crate) async fn execute(
    root: &Path,
    path: &Path,
    bot: Option<(&str, &str)>,
    name: &str,
    args: &str,
) -> Result<Value> {
    ensure!(args.len() <= 16_000, "command arguments too long");
    if let Some(c) = custom(root).into_iter().find(|c| c.name == name) {
        ensure!(
            !needs_local_expansion(&c),
            "Run this template on the host to expand local references"
        );
        let prompt = commands::substitute(&c, args);
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= 96_000,
            "invalid expanded command"
        );
        return Ok(json!({"prompt":prompt}));
    }
    let answer =
        if let Some(Report(_, _, argv)) = REPORTS.iter().find(|r| r.0 == name && r.1 == args) {
            report(root, argv).await?
        } else if (args.is_empty()
            || matches!(name, "plan" | "approval" | "browser") && args == "status")
            && SESSION.contains(&name)
        {
            let (messages, meta, _, _, control) =
                session_store::read_session_path_with_state(path).map_err(anyhow::Error::msg)?;
            let meta = meta.unwrap_or_default();
            match name {
                "plan" => {
                    if control.is_planning() {
                        format!(
                            "Planning · {}",
                            control.plan_target.map(|p| p.path).unwrap_or_default()
                        )
                    } else {
                        "Normal execution".into()
                    }
                }
                "tools" => crate::agent::toolsets::format_config_status(),
                "approval" => format!(
                    "Saved approval policy: {}",
                    crate::core::cli_config::load().persisted_approval_mode()
                ),
                "browser" => {
                    #[cfg(feature = "browser")]
                    {
                        crate::agent::browser::status()
                    }
                    #[cfg(not(feature = "browser"))]
                    {
                        "Browser tools are not included in this Core build.".to_owned()
                    }
                }
                "model" => meta
                    .model
                    .unwrap_or_else(|| "No model recorded for this session.".into()),
                "context" => format!(
                    "{} messages · {} user turns",
                    messages.len(),
                    messages.iter().filter(|m| m.role == "user").count()
                ),
                "tokens" => meta
                    .usage
                    .map(|u| {
                        format!(
                            "{} input · {} output · {} cached · {} cache write · {} API calls",
                            u.input, u.output, u.cached, u.cache_write, u.calls
                        )
                    })
                    .unwrap_or_else(|| "No provider-reported usage for this session.".into()),
                "memory" => {
                    if let Some((bot, thread)) = bot {
                        let rows = transfer::inventory(bot, thread).await?;
                        let names = rows
                            .as_array()
                            .context("invalid Memory inventory")?
                            .iter()
                            .filter_map(|r| r["name"].as_str())
                            .collect::<Vec<_>>();
                        if names.is_empty() {
                            "No saved memories in this scope.".into()
                        } else {
                            names.join("\n")
                        }
                    } else {
                        report(root, &["memory", "status"]).await?
                    }
                }
                _ => unreachable!(),
            }
        } else {
            bail!("This command or argument requires the host interface")
        };
    Ok(json!({"answer":answer}))
}
