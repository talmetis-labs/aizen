//! `aizen custom …` — your own provider endpoints, called with your own key (BYOK).
//!
//! An endpoint claims a model prefix. Calling `mine/gpt-4o` makes the gateway strip `mine/` and
//! forward the rest upstream using the key you stored, which has three consequences worth stating
//! before anyone goes looking for them:
//!
//!   * **It spends no karma and eats no plan quota.** The call is billed by the provider, to you,
//!     directly; charging for it again would be charging twice. The ledger records it as
//!     `billed_to: 'byok'` rather than not recording it — a "what did my agent do" table missing
//!     half the calls is a table that lies.
//!   * **A prefix beats a seller slug.** Take the prefix `nbz` and `nbz/…` stops reaching the
//!     seller `nbz` from this account. Nothing errors; the traffic simply goes somewhere else.
//!   * **These models are not in the catalogue.** `GET /v1/models` does not list them, by design.
//!     Call them by `prefix/model` without looking them up.
//!
//! Every write here — add, set, remove — answers with the WHOLE list rather than the one record it
//! touched, so nothing in this file follows a write with a read.

use crate::cli_args::CustomCmd;
use crate::llm::account::{self, AuthError};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::io::IsTerminal;

const ROUTE: &str = "/auth/own-endpoints";

fn bail(e: AuthError) -> ! {
    eprintln!("✖ {e}");
    std::process::exit(e.exit_code())
}

pub async fn run(cmd: CustomCmd) -> Result<()> {
    match cmd {
        CustomCmd::Ls { json } => ls(json).await,
        CustomCmd::Add {
            name,
            prefix,
            base_url,
            key,
            timeout,
            json,
        } => add(&name, &prefix, &base_url, key, timeout, json).await,
        CustomCmd::Set {
            id,
            name,
            prefix,
            base_url,
            key,
            clear_key,
            timeout,
            disable,
            enable,
            json,
        } => {
            set(
                &id, name, prefix, base_url, key, clear_key, timeout, disable, enable, json,
            )
            .await
        }
        CustomCmd::Rm { id, yes, json } => rm(&id, yes, json).await,
    }
}

/* --------------------------------------------------------------- helpers */

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Render the list every write returns, so `add`/`set`/`rm` all end the same way.
fn show(data: &Value) {
    let eps = data
        .get("endpoints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if eps.is_empty() {
        println!("(no endpoints)");
    } else {
        println!(
            "{:<10} {:<16} {:<10} {:<34} {:<6} {:<9} {}",
            "ID", "NAME", "PREFIX", "BASE URL", "KEY", "STATUS", "CALLS/30d"
        );
        for e in &eps {
            println!(
                "{:<10} {:<16} {:<10} {:<34} {:<6} {:<9} {}",
                s(e, "id"),
                s(e, "name"),
                s(e, "prefix"),
                s(e, "base_url"),
                // The server never returns the key, not even masked — only whether one is stored.
                if e.get("has_key").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "yes"
                } else {
                    "no"
                },
                s(e, "status"),
                e.get("calls_30d")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            // A probe error is why an endpoint is not working; it belongs next to the row, not in
            // a --json nobody runs.
            let err = s(e, "last_probe_error");
            if !err.is_empty() {
                println!("{:<10} ↳ last probe: {err}", "");
            }
        }
    }
    if let (Some(used), Some(max)) = (
        data.get("endpoints")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        data.get("max").and_then(|v| v.as_u64()),
    ) {
        eprintln!("{used}/{max} endpoints used");
    }
}

/// Ask for a secret without echoing it, refusing rather than reading a password off a pipe.
fn ask_key(what: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        eprintln!("✖ not a terminal — pass --key to supply the {what}");
        std::process::exit(2);
    }
    Ok(
        dialoguer::Password::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(format!("{what} (hidden)"))
            .interact()?,
    )
}

/// The server is the authority, but its 400 arrives after a round trip and says less than this can.
fn check_prefix(p: &str) -> bool {
    let ok = !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        eprintln!("✖ a prefix is lowercase letters, digits and dashes only (got: {p})");
    }
    ok
}

fn check_url(u: &str) -> bool {
    let ok = u.starts_with("http://") || u.starts_with("https://");
    if !ok {
        eprintln!("✖ the base URL must start with http:// or https:// (got: {u})");
    }
    ok
}

/* ------------------------------------------------------------------ verbs */

async fn ls(json: bool) -> Result<()> {
    let data = account::get(ROUTE).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    show(&data);
    Ok(())
}

async fn add(
    name: &str,
    prefix: &str,
    base_url: &str,
    key: Option<String>,
    timeout: Option<u32>,
    json: bool,
) -> Result<()> {
    if !check_prefix(prefix) || !check_url(base_url) {
        std::process::exit(2);
    }
    let key = match key {
        Some(k) => k,
        None => ask_key("provider key")?,
    };
    let mut body = Map::new();
    body.insert("name".into(), json!(name));
    body.insert("prefix".into(), json!(prefix));
    body.insert("base_url".into(), json!(base_url));
    body.insert("api_key".into(), json!(key));
    if let Some(t) = timeout {
        body.insert("timeout_seconds".into(), json!(t));
    }
    let data = account::post(ROUTE, Value::Object(body))
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    println!("✔ Added '{name}' — call it as `{prefix}/<model>`.");
    show(&data);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn set(
    id: &str,
    name: Option<String>,
    prefix: Option<String>,
    base_url: Option<String>,
    key: Option<String>,
    clear_key: bool,
    timeout: Option<u32>,
    disable: bool,
    enable: bool,
    json: bool,
) -> Result<()> {
    if let Some(p) = &prefix {
        if !check_prefix(p) {
            std::process::exit(2);
        }
    }
    if let Some(u) = &base_url {
        if !check_url(u) {
            std::process::exit(2);
        }
    }

    let mut body = Map::new();
    if let Some(v) = name {
        body.insert("name".into(), json!(v));
    }
    if let Some(v) = prefix {
        body.insert("prefix".into(), json!(v));
    }
    if let Some(v) = base_url {
        body.insert("base_url".into(), json!(v));
    }
    if let Some(t) = timeout {
        body.insert("timeout_seconds".into(), json!(t));
    }
    if disable || enable {
        body.insert("disabled".into(), json!(disable));
    }

    // Three meanings from two JSON states of one field: absent keeps the stored key, `""` erases
    // it. So an omitted --key must leave the field OUT of the body entirely — writing null here
    // would be a third state the server does not have.
    if clear_key {
        body.insert("api_key".into(), json!(""));
    } else if let Some(k) = key {
        // `--key` with no value means "ask me"; `--key <k>` supplies it outright.
        let k = if k.is_empty() {
            ask_key("new provider key")?
        } else {
            k
        };
        body.insert("api_key".into(), json!(k));
    }

    if body.is_empty() {
        eprintln!("✖ nothing to change — pass at least one of --name --prefix --base-url --key --clear-key --timeout --enable --disable");
        std::process::exit(2);
    }

    let path = format!("{ROUTE}/{}", account::encode_segment(id));
    let data = account::patch(&path, Value::Object(body))
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    println!("✔ Updated {id}.");
    show(&data);
    Ok(())
}

async fn rm(id: &str, yes: bool, json: bool) -> Result<()> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("✖ not a terminal — pass --yes to confirm");
            std::process::exit(2);
        }
        let ok = dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(format!("Delete endpoint {id}?"))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !ok {
            println!("Cancelled.");
            return Ok(());
        }
    }
    let path = format!("{ROUTE}/{}", account::encode_segment(id));
    let data = account::delete(&path).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    println!("✔ Deleted {id}.");
    show(&data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_is_lowercase_digits_and_dashes() {
        assert!(check_prefix("mine"));
        assert!(check_prefix("my-openai-2"));
        assert!(!check_prefix(""));
        assert!(!check_prefix("Mine"));
        assert!(!check_prefix("my_openai"));
        assert!(!check_prefix("mine/gpt"));
    }

    #[test]
    fn a_base_url_must_carry_a_scheme() {
        assert!(check_url("https://api.example.com/v1"));
        assert!(check_url("http://localhost:8080/v1"));
        assert!(!check_url("api.example.com"));
        assert!(!check_url("ftp://example.com"));
    }
}
