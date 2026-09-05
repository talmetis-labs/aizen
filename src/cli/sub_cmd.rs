//! `aizen sub …` — buy and manage the three kinds of Aizen subscription:
//!   * `plan`   — the account plan (karma grant + quotas)
//!   * `model`  — a marketplace model listing, priced per token
//!   * `plugin` — a paid plugin
//!
//! Every command goes through the account session (`/auth/*`, [`crate::llm::account`]), never the
//! gateway key. The guards that matter, one per known trap:
//!   * spends carry no retry (a lost answer must not be replayed → double charge);
//!   * listing ids keep their `/` in the URL path (`%2F` would not match the route);
//!   * a coupon price is whatever the server quotes — never recomputed here;
//!   * `needs_review` is surfaced loudly (a blocked sub reads as "active" otherwise);
//!   * `price_change: "gone"` and `status: "cancelled"` are shown plainly, not as errors.

use crate::cli_args::{ModelCmd, PlanCmd, PluginCmd, SubCmd};
use crate::llm::account::{self, AuthError};
use anyhow::Result;
use serde_json::Value;
use std::io::IsTerminal;
use std::path::PathBuf;

/// Print a session error the way the user can act on it, then leave with the documented code.
fn bail(e: AuthError) -> ! {
    eprintln!("✖ {e}");
    std::process::exit(e.exit_code())
}

pub async fn run(cmd: SubCmd) -> Result<()> {
    match cmd {
        SubCmd::Plan { cmd } => match cmd {
            PlanCmd::Ls { json } => plan_ls(json).await,
            PlanCmd::Buy { plan_id, yes, json } => plan_buy(&plan_id, yes, json).await,
        },
        SubCmd::Model { cmd } => match cmd {
            ModelCmd::Ls { all, json } => model_ls(all, json).await,
            ModelCmd::Add { listing_id, yes, json } => model_add(&listing_id, yes, json).await,
            ModelCmd::Confirm { listing_id, yes, json } => {
                model_confirm(&listing_id, yes, json).await
            }
            ModelCmd::Rm { listing_id, yes, json } => model_rm(&listing_id, yes, json).await,
        },
        SubCmd::Plugin { cmd } => match cmd {
            PluginCmd::Ls { json } => plugin_ls(json).await,
            PluginCmd::Quote { slug, code, json } => plugin_quote(&slug, code.as_deref(), json).await,
            PluginCmd::Buy { slug, code, yes, json } => {
                plugin_buy(&slug, code.as_deref(), yes, json).await
            }
            PluginCmd::Download { slug, out } => plugin_download(&slug, out).await,
        },
    }
}

/* --------------------------------------------------------------- helpers */

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// A karma price of `null` means "not sold for karma" — show a dash, never a number to buy at.
fn karma_cell(v: &Value) -> String {
    match v.get("karma_price") {
        Some(Value::Null) | None => "—".to_string(),
        Some(p) => p.to_string(),
    }
}

fn wallet_line(data: &Value) {
    if let Some(w) = data.get("wallet") {
        if !w.is_null() {
            eprintln!("wallet: {w} karma");
        }
    }
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

enum Decision {
    Yes,
    No,
    NeedYes,
}

/// Confirm a spend/cancel. `--yes` skips the prompt; a non-TTY without `--yes` is a hard stop
/// (`NeedYes`), never an implicit yes — a piped `buy` must not spend by default.
fn confirm(lines: &[String], yes: bool) -> Decision {
    for l in lines {
        eprintln!("{l}");
    }
    if yes {
        return Decision::Yes;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("✖ not a terminal — pass --yes to confirm");
        return Decision::NeedYes;
    }
    let ok = dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Proceed?")
        .default(false)
        .interact()
        .unwrap_or(false);
    if ok {
        Decision::Yes
    } else {
        Decision::No
    }
}

/// Map a decision to an early return: `Some(code)` means stop now with that exit code.
fn gate(lines: &[String], yes: bool) -> Option<i32> {
    match confirm(lines, yes) {
        Decision::Yes => None,
        Decision::No => {
            println!("Cancelled.");
            Some(0)
        }
        Decision::NeedYes => Some(2),
    }
}

fn require_listing(id: &str) -> bool {
    if account::is_listing_id(id) {
        return true;
    }
    eprintln!("✖ a listing id looks like seller/model, e.g. nbz/glm-5-air (got: {id})");
    false
}

/* ------------------------------------------------------------------ plan */

async fn plan_ls(json: bool) -> Result<()> {
    let data = account::get("/auth/plans").await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let plans = data.get("plans").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if plans.is_empty() {
        println!("(no plans)");
    } else {
        println!("{:<24} {:>9} {:>10} {:>6}  {}", "ID", "USD/mo", "KARMA", "DAYS", "");
        for p in &plans {
            println!(
                "{:<24} {:>9} {:>10} {:>6}  {}",
                s(p, "id"),
                p.get("monthly_price_usd").map(|v| v.to_string()).unwrap_or_default(),
                karma_cell(p),
                p.get("period_days").map(|v| v.to_string()).unwrap_or_default(),
                if p.get("current").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "← current"
                } else {
                    ""
                },
            );
        }
    }
    wallet_line(&data);
    Ok(())
}

async fn plan_buy(plan_id: &str, yes: bool, json: bool) -> Result<()> {
    // Look the plan up first — its price, karma eligibility, and whether it is already current.
    let data = account::get("/auth/plans").await.unwrap_or_else(|e| bail(e));
    let plans = data.get("plans").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let Some(plan) = plans.iter().find(|p| s(p, "id") == plan_id) else {
        eprintln!("✖ no plan '{plan_id}' — run `aizen sub plan ls`");
        std::process::exit(2);
    };
    if plan.get("current").and_then(|v| v.as_bool()).unwrap_or(false) {
        println!("You are already on '{}'.", s(plan, "name"));
        return Ok(());
    }
    if matches!(plan.get("karma_price"), Some(Value::Null) | None) {
        eprintln!("✖ '{}' is not sold for karma", s(plan, "name"));
        std::process::exit(2);
    }

    let lines = vec![
        format!(
            "Buy plan '{}' for {} karma.",
            s(plan, "name"),
            karma_cell(plan)
        ),
        format!("wallet: {} karma", data.get("wallet").cloned().unwrap_or(Value::Null)),
    ];
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }

    // retry:false is the whole point — account::post never retries.
    let out = account::post(&format!("/auth/plans/{}/buy", account::encode_segment(plan_id)), Value::Null)
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Now on '{}'.", s(plan, "name"));
    wallet_line(&out);
    Ok(())
}

/* ----------------------------------------------------------------- model */

async fn model_ls(all: bool, json: bool) -> Result<()> {
    let data = account::get("/auth/subscriptions").await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let subs = data.get("subscriptions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let shown: Vec<&Value> = subs
        .iter()
        // Cancelled subs come back too; hide them unless --all — a conscious choice, not a filter bug.
        .filter(|x| all || s(x, "status") != "cancelled")
        .collect();
    if shown.is_empty() {
        println!("(no subscriptions)");
    } else {
        println!("{:<28} {:<10} {:<8} {}", "LISTING", "STATUS", "PRICE", "");
        for x in &shown {
            let change = s(x, "price_change");
            // up/shape are BLOCKED until confirmed; "gone" is the seller delisting — not an error.
            let action = if change == "up" || change == "shape" {
                "needs confirm"
            } else {
                ""
            };
            let price = if change == "same" || change.is_empty() { "" } else { change.as_str() };
            println!("{:<28} {:<10} {:<8} {}", s(x, "listing_id"), s(x, "status"), price, action);
        }
    }
    if let Some(n) = data.get("needs_review").and_then(|v| v.as_u64()) {
        if n > 0 {
            eprintln!(
                "⚠ {n} subscription(s) are blocked until you confirm a price change — \
                 run `aizen sub model confirm <listing-id>`"
            );
        }
    }
    Ok(())
}

async fn model_add(listing_id: &str, yes: bool, json: bool) -> Result<()> {
    if !require_listing(listing_id) {
        std::process::exit(2);
    }
    let lines = vec![format!(
        "Subscribe to '{listing_id}' — you agree to its usage pricing."
    )];
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }
    // listing_id goes in the JSON body — no path encoding needed here.
    let out = account::post("/auth/subscriptions", serde_json::json!({ "listing_id": listing_id }))
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Subscribed to '{listing_id}'.");
    println!("  If it later shows a price change, run `aizen sub model confirm {listing_id}`.");
    Ok(())
}

async fn model_confirm(listing_id: &str, yes: bool, json: bool) -> Result<()> {
    if !require_listing(listing_id) {
        std::process::exit(2);
    }
    // Confirming accepts a possibly-higher price, so it is gated too.
    let lines = vec![format!("Accept the current price for '{listing_id}' and unblock it?")];
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }
    // The path keeps the '/' — encode each segment, not the whole id.
    let path = format!("/auth/subscriptions/{}/confirm", account::encode_listing(listing_id));
    let out = account::post(&path, Value::Null).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Confirmed '{listing_id}' — it can be called again.");
    Ok(())
}

async fn model_rm(listing_id: &str, yes: bool, json: bool) -> Result<()> {
    if !require_listing(listing_id) {
        std::process::exit(2);
    }
    let lines = vec![format!("Cancel subscription '{listing_id}'?")];
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }
    let path = format!("/auth/subscriptions/{}", account::encode_listing(listing_id));
    let out = account::delete(&path).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Cancelled '{listing_id}'.");
    Ok(())
}

/* ---------------------------------------------------------------- plugin */

async fn plugin_ls(json: bool) -> Result<()> {
    let data = account::get("/auth/plugins").await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let plugins = data.get("plugins").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if plugins.is_empty() {
        println!("(no plugins)");
    } else {
        println!("{:<22} {:>8} {:>10} {:<8} {}", "SLUG", "USD", "KARMA", "OWNED", "EXPIRES");
        for p in &plugins {
            println!(
                "{:<22} {:>8} {:>10} {:<8} {}",
                s(p, "slug"),
                p.get("price_usd").map(|v| v.to_string()).unwrap_or_default(),
                karma_cell(p),
                if p.get("owned").and_then(|v| v.as_bool()).unwrap_or(false) { "yes" } else { "" },
                s(p, "expires_at"),
            );
        }
    }
    wallet_line(&data);
    Ok(())
}

/// Ask the server for the final price after a coupon. The server owns the math — the client never
/// applies a discount itself, because two formulas drift.
async fn quote(slug: &str, code: Option<&str>) -> Result<Value, AuthError> {
    let body = match code {
        Some(c) => serde_json::json!({ "code": c }),
        None => Value::Null,
    };
    account::post(&format!("/auth/plugins/{}/quote", account::encode_segment(slug)), body).await
}

async fn plugin_quote(slug: &str, code: Option<&str>, json: bool) -> Result<()> {
    let q = quote(slug, code).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&q);
    }
    // The quote's exact shape is the server's to define — print it whole so the real price field
    // is visible whatever it is called.
    print_json(&q)
}

async fn plugin_buy(slug: &str, code: Option<&str>, yes: bool, json: bool) -> Result<()> {
    let data = account::get("/auth/plugins").await.unwrap_or_else(|e| bail(e));
    let plugins = data.get("plugins").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let Some(plugin) = plugins.iter().find(|p| s(p, "slug") == slug) else {
        eprintln!("✖ no plugin '{slug}' — run `aizen sub plugin ls`");
        std::process::exit(2);
    };
    if matches!(plugin.get("karma_price"), Some(Value::Null) | None) {
        eprintln!("✖ '{}' is not sold for karma", s(plugin, "name"));
        std::process::exit(2);
    }

    // With a coupon, the price to show is the server's quote, not the list price.
    let price_line = if let Some(c) = code {
        let q = quote(slug, Some(c)).await.unwrap_or_else(|e| bail(e));
        let price = q
            .get("karma_price")
            .or_else(|| q.get("price"))
            .or_else(|| q.get("total"))
            .cloned()
            .unwrap_or_else(|| plugin.get("karma_price").cloned().unwrap_or(Value::Null));
        format!("Buy '{}' for {price} karma (coupon applied).", s(plugin, "name"))
    } else {
        format!("Buy '{}' for {} karma.", s(plugin, "name"), karma_cell(plugin))
    };

    let lines = vec![
        price_line,
        format!("wallet: {} karma", data.get("wallet").cloned().unwrap_or(Value::Null)),
    ];
    if let Some(exit) = gate(&lines, yes) {
        std::process::exit(exit);
    }

    let body = match code {
        Some(c) => serde_json::json!({ "code": c }),
        None => Value::Null,
    };
    let out = account::post(&format!("/auth/plugins/{}/buy", account::encode_segment(slug)), body)
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Bought '{}'.", s(plugin, "name"));
    wallet_line(&out);
    println!("  Download it with `aizen sub plugin download {slug}`.");
    Ok(())
}

async fn plugin_download(slug: &str, out: Option<PathBuf>) -> Result<()> {
    let (bytes, name) = account::get_file(&format!("/auth/plugins/{}/download", account::encode_segment(slug)))
        .await
        .unwrap_or_else(|e| bail(e));
    let dest = out.unwrap_or_else(|| PathBuf::from(name.unwrap_or_else(|| format!("{slug}.zip"))));
    std::fs::write(&dest, &bytes)?;
    println!("✔ Saved {} ({} bytes) → {}", slug, bytes.len(), dest.display());
    Ok(())
}
