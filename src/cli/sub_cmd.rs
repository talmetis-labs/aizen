//! `aizen sub …` — buy and manage what an account can hold:
//!   * `plan`   — the account plan (karma grant + quotas)
//!   * `combo`  — a marketplace combo on offer
//!   * `model`  — the listing subscriptions you hold; combos and seller models alike
//!   * `plugin` — a paid plugin
//!
//! Every command goes through the account session (`/auth/*`, [`crate::llm::account`]), never the
//! gateway key.
//!
//! ## Buying a thing and being able to call it are two questions
//!
//! Nothing here mints, reveals or loads a key, and that separation is the source of the API's most
//! confusing 403. The plan key carries **Aizen's own plans only**; a seller's model bought through
//! `model add` is subscribed but uncallable through it and needs a key of the user's own. And an
//! EMPTY loadout on the plan key means it can call *nothing*, inverting what an empty allow-list
//! means on every other key. Both are one `aizen key …` away, so the sentences that say so are
//! printed at the moment of the purchase rather than left for the first failed call to explain.
//!
//! Own endpoints (`mine/…`, BYOK) do not pass through this door at any point: they are billed by
//! the provider at the far end, spend no karma, and hold no subscription. `model add mine/x` is
//! refused here rather than at the server, because the answer is a different command entirely.
//!
//! The guards that matter, one per known trap:
//!
//!   * spends carry no retry — a lost answer must not be replayed into a second charge, and the
//!     server's 409 is a last fence rather than a contract (a plugin *renewal* is legitimately
//!     repeatable, so 409 cannot be relied on to catch a double send);
//!   * listing ids keep their `/` in the URL path — `%2F` would not match the route, and an id is
//!     cut from the END (the model is the last segment) so a deeper namespace is legal, not a typo;
//!   * a coupon price is whatever the server quotes, never recomputed here;
//!   * `needs_review` is surfaced loudly — a blocked subscription reads as "active" otherwise;
//!   * `null` is not `0` anywhere: a null `karma_price` means "not sold for karma" rather than
//!     "free", and a null `quota_units` means "no cap" while `0` is a real cap of zero;
//!   * a plugin's `owned` is the string `none`/`active`/`expired`, not a boolean — read as a
//!     boolean it is truthy in three cases out of three;
//!   * `price_change: "gone"` and `status: "cancelled"` are states to show plainly, not errors.

use crate::cli::gate;
use crate::cli_args::{ComboCmd, ModelCmd, PlanCmd, PluginCmd, SubCmd};
use crate::llm::account::{self, AuthError};
use anyhow::Result;
use serde_json::Value;
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
        SubCmd::Combo { cmd } => match cmd {
            ComboCmd::Ls { json } => combo_ls(json).await,
        },
        SubCmd::Model { cmd } => match cmd {
            ModelCmd::Ls { all, json } => model_ls(all, json).await,
            ModelCmd::Add {
                listing_id,
                yes,
                json,
            } => model_add(&listing_id, yes, json).await,
            ModelCmd::Confirm {
                listing_id,
                yes,
                json,
            } => model_confirm(&listing_id, yes, json).await,
            ModelCmd::Rm {
                listing_id,
                yes,
                json,
            } => model_rm(&listing_id, yes, json).await,
        },
        SubCmd::Plugin { cmd } => match cmd {
            PluginCmd::Ls { json } => plugin_ls(json).await,
            PluginCmd::Quote { slug, code, json } => {
                plugin_quote(&slug, code.as_deref(), json).await
            }
            PluginCmd::Buy {
                slug,
                code,
                yes,
                json,
            } => plugin_buy(&slug, code.as_deref(), yes, json).await,
            PluginCmd::Download { slug, out } => plugin_download(&slug, out).await,
        },
    }
}

/* --------------------------------------------------------------- helpers */

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// A number for display, with a distinct word for `null`.
///
/// The whole point is that `null` and `0` are different answers everywhere in this API, and the
/// difference usually inverts the meaning: a null cap is "no cap", a cap of `0` forbids everything.
fn num(v: &Value, k: &str, when_null: &str) -> String {
    match v.get(k) {
        None | Some(Value::Null) => when_null.to_string(),
        Some(x) => x.to_string(),
    }
}

/// `null` here means "not sold for karma", which is the opposite of free — never show a number to
/// buy at, and never let a caller mistake the dash for zero.
fn karma_cell(v: &Value) -> String {
    num(v, "karma_price", "—")
}

fn sellable_for_karma(v: &Value) -> bool {
    !matches!(v.get("karma_price"), None | Some(Value::Null))
}

fn wallet_line(data: &Value) {
    if let Some(w) = data.get("wallet") {
        if !w.is_null() {
            eprintln!("wallet: {w} karma");
        }
    }
}

/// The footer both catalogues share: what karma is worth, and whether usage comes out of the wallet.
fn rate_line(data: &Value) {
    let mut bits = Vec::new();
    if let Some(r) = data.get("karma_per_usd").filter(|v| !v.is_null()) {
        bits.push(format!("{r} karma/USD"));
    }
    // Absent is not false — say nothing rather than claim the wallet is safe.
    if let Some(b) = data.get("wallet_pays_usage").and_then(|v| v.as_bool()) {
        bits.push(
            if b {
                "usage is charged to the wallet"
            } else {
                "usage is not charged to the wallet"
            }
            .to_string(),
        );
    }
    if !bits.is_empty() {
        eprintln!("({})", bits.join(" · "));
    }
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn arr(data: &Value, k: &str) -> Vec<Value> {
    data.get(k)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Own endpoints are called by prefix, and they are billed at the far end — never through here.
const OWN_ENDPOINT_NS: &str = "mine/";

/// Two refusals, deliberately not one sentence: a malformed id is a typo, an own-endpoint id is a
/// category error that no amount of retyping fixes.
fn require_listing(id: &str) -> bool {
    if id.starts_with(OWN_ENDPOINT_NS) {
        eprintln!("✖ '{id}' is one of your own endpoints, and those hold no subscription.");
        eprintln!("  They are billed by your provider directly — see `aizen custom ls`.");
        return false;
    }
    if account::is_listing_id(id) {
        return true;
    }
    eprintln!("✖ a listing id looks like seller/name, e.g. aizen/deepseek (got: {id})");
    false
}

/* ------------------------------------------------------------------ plan */

async fn plan_ls(json: bool) -> Result<()> {
    let data = account::get("/auth/plans")
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let plans = arr(&data, "plans");
    if plans.is_empty() {
        println!("(no plans)");
    } else {
        println!(
            "{:<18} {:>7} {:>8} {:>8} {:<7} {:>9} {:>5} {:>5}  {}",
            "ID", "USD/mo", "KARMA", "GRANT", "UNIT", "QUOTA", "RPM", "CONC", ""
        );
        for p in &plans {
            println!(
                "{:<18} {:>7} {:>8} {:>8} {:<7} {:>9} {:>5} {:>5}  {}",
                s(p, "id"),
                num(p, "monthly_price_usd", "—"),
                karma_cell(p),
                num(p, "karma_grant", "—"),
                s(p, "sell_unit"),
                // null = no cap at all; 0 is a real cap and means the opposite.
                num(p, "quota_units", "unlimited"),
                // Two different ceilings — calls per minute, and calls at once. Averaging them into
                // one "rate limit" column loses whichever one is actually going to bite.
                num(p, "max_requests_per_minute", "—"),
                num(p, "max_concurrent_requests", "—"),
                if p.get("current").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "← current"
                } else {
                    ""
                },
            );
        }
        println!();
        println!("QUOTA is per period; `unlimited` is a null cap, `0` is a real cap of zero.");
        println!("RPM = calls per minute; CONC = calls at once. Different ceilings.");
        println!("What a plan lets you call: `aizen key models` · full detail: `--json`.");
    }
    wallet_line(&data);
    rate_line(&data);
    Ok(())
}

async fn plan_buy(plan_id: &str, yes: bool, json: bool) -> Result<()> {
    // Look the plan up first — its price, karma eligibility, and whether it is already current.
    let data = account::get("/auth/plans")
        .await
        .unwrap_or_else(|e| bail(e));
    let plans = arr(&data, "plans");
    let Some(plan) = plans.iter().find(|p| s(p, "id") == plan_id) else {
        eprintln!("✖ no plan '{plan_id}' — run `aizen sub plan ls`");
        std::process::exit(2);
    };
    if plan
        .get("current")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        println!("You are already on '{}'.", s(plan, "name"));
        return Ok(());
    }
    if !sellable_for_karma(plan) {
        eprintln!("✖ '{}' is not sold for karma", s(plan, "name"));
        std::process::exit(2);
    }

    let mut lines = vec![format!(
        "Buy plan '{}' for {} karma.",
        s(plan, "name"),
        karma_cell(plan)
    )];
    // The price comes out of the karma WALLET, not the period balance — worth saying, because the
    // two are different pots and only one of them is being spent here.
    lines.push(format!(
        "Paid from the wallet ({} karma), not the period balance.",
        data.get("wallet").cloned().unwrap_or(Value::Null)
    ));
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }

    // retry:false is the whole point — account::post never retries.
    let out = account::post(
        &format!("/auth/plans/{}/buy", account::encode_segment(plan_id)),
        Value::Null,
    )
    .await
    .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Now on '{}'.", s(plan, "name"));
    wallet_line(&out);
    // Being on a plan is not yet being able to call it. The plan key calls what its loadout holds,
    // and on that one key an empty loadout means nothing rather than everything — so a fresh plan
    // with nothing loaded 403s on the first call, which reads as a billing fault and is not one.
    println!("  A plan becomes callable once the plan key loads it: `aizen key loadout ls`.");
    Ok(())
}

/* ----------------------------------------------------------------- combo */

/// What one combo costs, phrased for the unit it is actually sold in.
///
/// On a per-token combo a null rate does NOT mean free — it means the combo has no flat rate of its
/// own and is charged at the rate of whichever model it routes to.
fn combo_price(c: &Value) -> String {
    match s(c, "sell_unit").as_str() {
        "request" => format!("{}/req", num(c, "karma_per_request", "per-model")),
        _ => {
            let (i, o) = (
                num(c, "karma_in_per_1m", "per-model"),
                num(c, "karma_out_per_1m", "per-model"),
            );
            format!("{i} in / {o} out per 1M")
        }
    }
}

async fn combo_ls(json: bool) -> Result<()> {
    let data = account::get("/auth/combos")
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let combos = arr(&data, "combos");
    if combos.is_empty() {
        println!("(no combos)");
    } else {
        println!("{:<22} {:<24} {:<26} {}", "NAME", "LISTING", "PRICE", "");
        for c in &combos {
            // `subscribed` and `usable` are different questions: a combo you have bought can still
            // be uncallable because your plan sells a different unit. Show the wall, not the sale.
            let usable = c.get("usable").and_then(|v| v.as_bool()).unwrap_or(false);
            let subscribed = c
                .get("subscribed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut flags = Vec::new();
            if subscribed {
                flags.push("subscribed".to_string());
            }
            if !usable {
                let why = s(c, "reason");
                flags.push(if why.is_empty() {
                    "not callable".to_string()
                } else {
                    format!("not callable: {why}")
                });
            }
            let listing = s(c, "listing_id");
            println!(
                "{:<22} {:<24} {:<26} {}",
                s(c, "name"),
                // An empty listing id is a real answer: the combo is not sold on its own.
                if listing.is_empty() {
                    "(not sold separately)".to_string()
                } else {
                    listing
                },
                combo_price(c),
                flags.join(" · "),
            );
        }
        println!();
        println!("Subscribe with `aizen sub model add <listing-id>` — combos and seller models share one door.");
    }
    // A null plan is "no plan running", which is not the same as a plan with an empty name: it
    // means usage is paid from the wallet.
    match data.get("plan") {
        Some(p) if !p.is_null() => {
            eprintln!("plan: {} (sold by {})", s(p, "name"), s(p, "sell_unit"))
        }
        _ => eprintln!("plan: none — usage is paid from the wallet"),
    }
    wallet_line(&data);
    rate_line(&data);
    Ok(())
}

/* ----------------------------------------------------------------- model */

async fn model_ls(all: bool, json: bool) -> Result<()> {
    let data = account::get("/auth/subscriptions")
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let subs = arr(&data, "subscriptions");
    let shown: Vec<&Value> = subs
        .iter()
        // Cancelled subs come back too; hide them unless --all — a conscious choice, not a filter bug.
        .filter(|x| all || s(x, "status") != "cancelled")
        .collect();
    if shown.is_empty() {
        println!("(no subscriptions)");
    } else {
        println!(
            "{:<24} {:<18} {:<10} {:<8} {}",
            "LISTING", "NAME", "STATUS", "PRICE", ""
        );
        for x in &shown {
            let change = s(x, "price_change");
            // The server holds no "awaiting confirmation" column: it recomputes this on every read
            // by comparing the agreed price against the seller's current one, leg by leg. Dearer on
            // ANY leg is `up`. So this word is a fact about today, not a stored flag.
            let note = match change.as_str() {
                "up" | "shape" => "needs confirm",
                // The seller withdrew the listing. A state, not a failure.
                "gone" => "seller withdrew it",
                "down" => "cheaper now",
                _ => "",
            };
            let display = s(x, "display_name");
            let seller = s(x, "seller_name");
            println!(
                "{:<24} {:<18} {:<10} {:<8} {}",
                s(x, "listing_id"),
                if display.is_empty() { seller } else { display },
                s(x, "status"),
                if change == "same" {
                    ""
                } else {
                    change.as_str()
                },
                note,
            );
        }
    }
    if let Some(n) = data.get("needs_review").and_then(|v| v.as_u64()) {
        if n > 0 {
            eprintln!(
                "⚠ {n} subscription(s) are blocked until you accept a price change — \
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
    let lines = vec![
        format!("Subscribe to '{listing_id}' — you agree to its usage pricing."),
        // Worth stating before the prompt: the wallet does not move here. Somebody refusing this
        // because they read it as a purchase would be refusing the wrong thing.
        "Subscribing itself is free; usage is charged when you call it.".to_string(),
    ];
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }
    // listing_id goes in the JSON body — no path encoding needed here.
    let out = account::post(
        "/auth/subscriptions",
        serde_json::json!({ "listing_id": listing_id }),
    )
    .await
    .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Subscribed to '{listing_id}'.");
    println!("  If it later shows a price change, run `aizen sub model confirm {listing_id}`.");
    // The plan key carries Aizen's own plans only, so a seller's listing is subscribed here and
    // still 403s through that key. Which side this id falls on is the server's to say — so say the
    // rule rather than guess the verdict from the namespace.
    println!(
        "  A seller's model is not callable through the plan key: put it on a key of your own"
    );
    println!("  (`aizen key ls`). The plan key carries Aizen's own plans only.");
    Ok(())
}

async fn model_confirm(listing_id: &str, yes: bool, json: bool) -> Result<()> {
    if !require_listing(listing_id) {
        std::process::exit(2);
    }
    // Confirming accepts a possibly-higher price, so it is gated too.
    let lines = vec![format!(
        "Accept the seller's current price for '{listing_id}' and unblock it?"
    )];
    if let Some(code) = gate(&lines, yes) {
        std::process::exit(code);
    }
    // The path keeps the '/' — encode each segment, not the whole id.
    let path = format!(
        "/auth/subscriptions/{}/confirm",
        account::encode_listing(listing_id)
    );
    let out = account::post(&path, Value::Null)
        .await
        .unwrap_or_else(|e| bail(e));
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
    let path = format!(
        "/auth/subscriptions/{}",
        account::encode_listing(listing_id)
    );
    let out = account::delete(&path).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Cancelled '{listing_id}'.");
    Ok(())
}

/* ---------------------------------------------------------------- plugin */

/// `owned` is `none` / `active` / `expired` — a string with three answers, not a flag.
///
/// Read as a boolean it is truthy in all three cases, so an unbought plugin shows as owned; read as
/// two cases it loses the renewal path, which is the one that costs money.
fn owned_state(p: &Value) -> &str {
    match s(p, "owned").as_str() {
        "active" => "active",
        "expired" => "expired",
        _ => "none",
    }
}

async fn plugin_ls(json: bool) -> Result<()> {
    let data = account::get("/auth/plugins")
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let plugins = arr(&data, "plugins");
    if plugins.is_empty() {
        println!("(no plugins)");
    } else {
        println!(
            "{:<20} {:>7} {:>9} {:<9} {:<12} {}",
            "SLUG", "USD", "KARMA", "OWNED", "EXPIRES", ""
        );
        for p in &plugins {
            // No approved version means the download is empty even after paying — say so on the
            // shelf, not after the charge.
            let note = if s(p, "live_version").is_empty() {
                "no released version yet"
            } else {
                ""
            };
            println!(
                "{:<20} {:>7} {:>9} {:<9} {:<12} {}",
                s(p, "slug"),
                num(p, "price_usd", "—"),
                karma_cell(p),
                owned_state(p),
                s(p, "expires_at"),
                note,
            );
        }
    }
    wallet_line(&data);
    rate_line(&data);
    Ok(())
}

/// Ask the server for the final price after a coupon. The server owns the math — the client never
/// applies a discount itself, because two formulas drift.
async fn quote(slug: &str, code: Option<&str>) -> Result<Value, AuthError> {
    let body = match code {
        Some(c) => serde_json::json!({ "code": c }),
        None => Value::Null,
    };
    account::post(
        &format!("/auth/plugins/{}/quote", account::encode_segment(slug)),
        body,
    )
    .await
}

async fn plugin_quote(slug: &str, code: Option<&str>, json: bool) -> Result<()> {
    let q = quote(slug, code).await.unwrap_or_else(|e| bail(e));
    let _ = json; // The quote's shape is the server's to define — print it whole either way.
    print_json(&q)
}

async fn plugin_buy(slug: &str, code: Option<&str>, yes: bool, json: bool) -> Result<()> {
    let data = account::get("/auth/plugins")
        .await
        .unwrap_or_else(|e| bail(e));
    let plugins = arr(&data, "plugins");
    let Some(plugin) = plugins.iter().find(|p| s(p, "slug") == slug) else {
        eprintln!("✖ no plugin '{slug}' — run `aizen sub plugin ls`");
        std::process::exit(2);
    };
    let name = s(plugin, "name");
    // Already held: the server answers 409, so spend the round trip here instead of the money.
    if owned_state(plugin) == "active" {
        let until = s(plugin, "expires_at");
        println!(
            "You already own '{name}'{}.",
            if until.is_empty() {
                String::new()
            } else {
                format!(" until {until}")
            }
        );
        return Ok(());
    }
    if !sellable_for_karma(plugin) {
        eprintln!("✖ '{name}' is not sold for karma");
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
        format!("Buy '{name}' for {price} karma (coupon applied).")
    } else {
        format!("Buy '{name}' for {} karma.", karma_cell(plugin))
    };

    let mut lines = vec![price_line];
    if owned_state(plugin) == "expired" {
        lines.push("This renews a licence that has expired.".to_string());
    }
    if s(plugin, "live_version").is_empty() {
        // Paying for something with nothing to download is a legitimate choice (backing an author),
        // but it must be a choice.
        lines
            .push("⚠ No version has been released yet — there is nothing to download.".to_string());
    }
    lines.push(format!(
        "wallet: {} karma",
        data.get("wallet").cloned().unwrap_or(Value::Null)
    ));
    if let Some(exit) = gate(&lines, yes) {
        std::process::exit(exit);
    }

    let body = match code {
        Some(c) => serde_json::json!({ "code": c }),
        None => Value::Null,
    };
    let out = account::post(
        &format!("/auth/plugins/{}/buy", account::encode_segment(slug)),
        body,
    )
    .await
    .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    println!("✔ Bought '{name}'.");
    wallet_line(&out);
    println!("  Download it with `aizen sub plugin download {slug}`.");
    Ok(())
}

async fn plugin_download(slug: &str, out: Option<PathBuf>) -> Result<()> {
    let (bytes, name) = account::get_file(&format!(
        "/auth/plugins/{}/download",
        account::encode_segment(slug)
    ))
    .await
    .unwrap_or_else(|e| bail(e));
    let dest = out.unwrap_or_else(|| PathBuf::from(name.unwrap_or_else(|| format!("{slug}.zip"))));
    std::fs::write(&dest, &bytes)?;
    println!(
        "✔ Saved {} ({} bytes) → {}",
        slug,
        bytes.len(),
        dest.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The trap: `null` and `0` mean opposite things for a quota, and both are falsy.
    #[test]
    fn null_and_zero_are_different_answers() {
        assert_eq!(
            num(&json!({ "quota_units": null }), "quota_units", "unlimited"),
            "unlimited"
        );
        assert_eq!(
            num(&json!({ "quota_units": 0 }), "quota_units", "unlimited"),
            "0"
        );
        assert_eq!(num(&json!({}), "quota_units", "unlimited"), "unlimited");
    }

    /// A null karma price is "not sold for karma", never "free".
    #[test]
    fn a_null_karma_price_is_not_a_price() {
        assert_eq!(karma_cell(&json!({ "karma_price": null })), "—");
        assert!(!sellable_for_karma(&json!({ "karma_price": null })));
        assert!(!sellable_for_karma(&json!({})));
        assert!(sellable_for_karma(&json!({ "karma_price": 0 })));
    }

    /// Read as a boolean, every one of these would have been "owned".
    #[test]
    fn owned_has_three_states() {
        assert_eq!(owned_state(&json!({ "owned": "none" })), "none");
        assert_eq!(owned_state(&json!({ "owned": "active" })), "active");
        assert_eq!(owned_state(&json!({ "owned": "expired" })), "expired");
        assert_eq!(owned_state(&json!({})), "none");
    }

    /// A per-token combo with no flat rate is priced by the model it picks — not free.
    #[test]
    fn a_null_combo_rate_means_per_model() {
        let c = json!({ "sell_unit": "token", "karma_in_per_1m": null, "karma_out_per_1m": null });
        assert_eq!(combo_price(&c), "per-model in / per-model out per 1M");
        let r = json!({ "sell_unit": "request", "karma_per_request": 3 });
        assert_eq!(combo_price(&r), "3/req");
    }

    /// The id is cut from the END, so a deeper namespace is a legal listing rather than a typo —
    /// refusing it spent a real purchase on a sentence telling the user to fix a correct id.
    #[test]
    fn a_deeper_listing_id_is_legal() {
        assert!(require_listing("aizen/deepseek"));
        assert!(require_listing("two/slashes/here"));
        assert!(!require_listing("no-slash"));
    }

    /// `mine/…` is well-formed and still wrong here: own endpoints are billed by the provider and
    /// hold no subscription at all, so this is a different command, not a retry.
    #[test]
    fn an_own_endpoint_is_refused_before_the_round_trip() {
        assert!(!require_listing("mine/gpt-4o"));
        assert!(
            account::is_listing_id("mine/gpt-4o"),
            "well-formed — refused on meaning, not shape"
        );
    }
}
