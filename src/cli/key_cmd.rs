//! `aizen key …` — the API keys on an account, and what the plan key is allowed to call.
//!
//! ## The plan key is a key with a FLAG, not a key with a name
//!
//! Every row of `/auth/keys` carries `plan_key`, and that boolean is the answer. The label is not:
//! an account holds one plan key tied to its owner, and one issued through the admin door is
//! labelled with the owner's email rather than `main`. Matching the name found nothing on those
//! accounts and then announced that the account had no plan key, which was false.
//!
//! A row with no `plan_key` field is a server from before the flag, where the reserved name was the
//! only marker there was — so that stays as the fallback. Both live in one place ([`is_main`])
//! rather than being re-spelled per call site.
//!
//! It then breaks three expectations that an ordinary key sets:
//!
//!   1. **It calls Aizen's own plan items only.** A raw marketplace model through it is a 403
//!      (`Model "x" is not one of Aizen's own — key "main" carries those only`).
//!   2. **An empty loadout means it can call NOTHING.** On any other key an empty allow-list means
//!      "no restriction"; the server carries a separate `PlanKey` flag placed ahead of that branch
//!      precisely to invert it here. A freshly issued plan key with nothing loaded is therefore a
//!      key that 403s on its first call, which is why [`loadout_ls`] says so loudly instead of
//!      printing an empty table.
//!   3. **Its loadout holds plans only** — no bare models, no `*` patterns. Refused on write and
//!      refused again on call; neither check substitutes for the other.
//!
//! ## The plan has no key string. There is nothing here to copy.
//!
//! The subscription is sold by **signing in**: `aizen account login` gets a session token, the
//! token opens `/v1`, and that is the whole of it. One row of `api_keys` still stands behind each
//! account server-side — it is where the loadout, the per-minute ceiling, the budget and every
//! `usage_history.api_key_id` hang — but it is an internal detail. No HTTP route emits its string,
//! no screen shows it, and nothing on this machine stores it.
//!
//! So there is no `key show` and no `key rotate`. There were, briefly, against a contract that had
//! `/auth/plan-key` and `/auth/plan-key/rotate`; those routes were withdrawn before they shipped
//! and answer 404 permanently. `reveal` therefore takes no default either — see [`reveal`].
//!
//! Keys still exist in two other places, and neither changed: a key the **user made themselves**,
//! for models bought from a seller, and their own key at the far end of a BYOK endpoint
//! (`mine/gpt-4o`), which bills there and never touches a plan.
//!
//! ## Order is meaning
//!
//! The loadout is an array, and the array is `auto`'s preference order: the name `auto` resolves to
//! the first entry that is callable *at that moment*. It is not a retry chain — a reply that broke
//! mid-stream is never re-asked of a different model, because two models answering one question
//! without the caller knowing which they are reading is worse than a failed call. Failover changes
//! the ROUTE, never the model.
//!
//! ## The id goes at the END of the path
//!
//! `/auth/keys/reveal/{id}` and `/auth/keys/loadout/{id}` — not `/auth/keys/{id}/…`. This matters
//! more than a spelling usually does, because `/auth/keys/{id}` is the REVOKE route: a client that
//! builds the "natural" path and sends DELETE deletes the user's key. Nothing in this module sends
//! DELETE at all, and there is no revoke subcommand: the `main` name is reserved, so revoking the
//! plan key cannot be undone by making another one.

use crate::cli_args::{KeyCmd, LoadoutCmd};
use crate::llm::account::{self, AuthError};
use anyhow::Result;
use serde_json::{json, Value};

/// The reserved name that means "pick the first callable entry", not a plan you can load.
const AUTO: &str = "auto";
/// The server's ceiling, mirrored so the refusal happens before the round trip.
const MAX_LOADOUT: usize = 10;

fn bail(e: AuthError) -> ! {
    eprintln!("✖ {e}");
    std::process::exit(e.exit_code())
}

pub async fn run(cmd: KeyCmd) -> Result<()> {
    match cmd {
        KeyCmd::Ls { json } => ls(json).await,
        KeyCmd::Reveal { id, json } => reveal(id, json).await,
        KeyCmd::Models { name, json } => models(name.as_deref(), json).await,
        KeyCmd::Loadout { cmd } => match cmd {
            LoadoutCmd::Ls { id, json } => loadout_ls(id, json).await,
            LoadoutCmd::Set { models, id, json } => loadout_set(models, id, json).await,
            LoadoutCmd::Add { models, id, json } => loadout_edit(models, id, json, true).await,
            LoadoutCmd::Rm { models, id, json } => loadout_edit(models, id, json, false).await,
        },
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

/// The single place that decides which key is the plan key.
///
/// The flag wins wherever it is present, including when it is present and `false` — a key a user
/// managed to label `main` is an ordinary key, and treating it as the plan key would aim
/// `loadout set` at the wrong one.
fn is_main(k: &Value) -> bool {
    match k.get("plan_key").and_then(|v| v.as_bool()) {
        Some(flag) => flag,
        None => s(k, "label").eq_ignore_ascii_case("main"),
    }
}

async fn all_keys() -> Vec<Value> {
    let data = account::get("/auth/keys").await.unwrap_or_else(|e| bail(e));
    // The route answers with a bare array; tolerate an envelope in case that changes.
    match data {
        Value::Array(a) => a,
        ref v => v
            .get("keys")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default(),
    }
}

/// Resolve which key to act on: the one given, else the `main` key found by label.
///
/// Two round trips, because there is no "give me my main key" route. That is a fact about the
/// server today, not something to wait for — the join happens here.
async fn resolve_id(explicit: Option<String>) -> String {
    if let Some(id) = explicit {
        return id;
    }
    let keys = all_keys().await;
    match keys.iter().find(|k| is_main(k)) {
        Some(k) => s(k, "id"),
        None => {
            eprintln!(
                "✖ this account has no plan key yet.\n\
                 Run `aizen key ls` to see what you do have, or pass --id to act on another key."
            );
            std::process::exit(2)
        }
    }
}

/* -------------------------------------------------------------------- ls */

async fn ls(json: bool) -> Result<()> {
    let keys = all_keys().await;
    if json {
        return print_json(&Value::Array(keys));
    }
    if keys.is_empty() {
        println!("(no keys)");
        return Ok(());
    }
    println!(
        "{:<12} {:<22} {:<14} {:<9} {:<22} {}",
        "ID", "LABEL", "PREFIX", "ENABLED", "LAST USED", ""
    );
    for k in &keys {
        println!(
            "{:<12} {:<22} {:<14} {:<9} {:<22} {}",
            s(k, "id"),
            // Whatever it says: on the plan key this is often the owner's email address. It is
            // shown, and the marker beside it is what actually answers "which one is the plan key".
            s(k, "label"),
            // The visible head only. The key itself is never in this response.
            s(k, "key_prefix"),
            if k.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
                "yes"
            } else {
                "no"
            },
            s(k, "last_used_at"),
            if is_main(k) { "← your plan" } else { "" },
        );
    }
    if keys.iter().any(is_main) {
        println!();
        println!("The marked row is your plan. It has no string to copy — signing in is what buys");
        println!("it, and `aizen account login` is the whole of it. `aizen key loadout ls` shows");
        println!("what it may call.");
    }
    Ok(())
}

/* ------------------------------------------------------------- the string */

/// The key out of whatever shape carries it: bare, `{key}`, or `{api_key}`.
fn key_string(data: &Value) -> String {
    match data {
        Value::String(k) => k.clone(),
        v => {
            let k = s(v, "key");
            if k.is_empty() {
                s(v, "api_key")
            } else {
                k
            }
        }
    }
}

/// Print the key on stdout, or leave saying the server sent none — a blank line reads as success.
fn print_key(key: &str) -> ! {
    if key.is_empty() {
        eprintln!("✖ the server returned no key string");
        std::process::exit(1);
    }
    println!("{key}");
    std::process::exit(0)
}

/* ---------------------------------------------------------------- reveal */

/// `GET /auth/keys/reveal/{id}` — the full string of a key the **user made themselves**.
///
/// **No default, on purpose.** It used to fall back to the plan key, from the contract where a plan
/// wore a string somebody could paste. It does not: the plan is sold by signing in, no route emits
/// its string, and defaulting here could only turn "there is nothing to reveal" into what looks
/// like a fault in this command. So the id is required, and the plan row is refused by name.
async fn reveal(id: Option<String>, json: bool) -> Result<()> {
    let Some(id) = id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        eprintln!("✖ which key? Pass --id <id> — `aizen key ls` lists them.");
        eprintln!("  There is no key to reveal for your Aizen plan: signing in is what buys it,");
        eprintln!("  and `aizen account login` is the whole of it.");
        std::process::exit(2);
    };
    // One extra read, to turn a server refusal into the sentence that explains it. Asking the
    // server for the plan key's string is not a mistake this command should let somebody make and
    // then have to interpret.
    if all_keys()
        .await
        .iter()
        .any(|k| s(k, "id") == id && is_main(k))
    {
        eprintln!("✖ '{id}' is your plan, and a plan has no key string — nothing emits it.");
        eprintln!("  To call models here, sign in: `aizen account login`.");
        eprintln!(
            "  To hold a key of your own (a seller's model, an SDK), make one in the dashboard."
        );
        std::process::exit(2);
    }
    let path = format!("/auth/keys/reveal/{}", account::encode_segment(&id));
    // No status is special-cased. A key issued before at-rest encryption answers 409, and the
    // server's own sentence says what to do about it; advice added here could only contradict it.
    let data = account::get(&path).await.unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    print_key(&key_string(&data))
}

/* --------------------------------------------------------------- loadout */

fn loadout_route(id: &str) -> String {
    // id LAST. `/auth/keys/{id}` is the revoke route; this is not that.
    format!("/auth/keys/loadout/{}", account::encode_segment(id))
}

/// The entries as plain names, in order.
fn entry_names(data: &Value) -> Vec<String> {
    data.get("loadout")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|e| s(e, "model")).collect())
        .unwrap_or_default()
}

async fn loadout_ls(id: Option<String>, json: bool) -> Result<()> {
    let id = resolve_id(id).await;
    let data = account::get(&loadout_route(&id))
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&data);
    }
    let entries = data
        .get("loadout")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let max = data
        .get("max")
        .and_then(|v| v.as_u64())
        .unwrap_or(MAX_LOADOUT as u64);
    let auto = {
        let a = s(&data, "auto");
        if a.is_empty() {
            AUTO.to_string()
        } else {
            a
        }
    };

    if entries.is_empty() {
        // Not an empty table — a broken key. On a plan key an empty loadout is a hard "no", so the
        // user would otherwise meet it as a 403 on their first call.
        println!(
            "The loadout is EMPTY, and on the plan key that means it can call nothing at all."
        );
        println!(
            "(An empty allow-list means 'no restriction' on ordinary keys — not on this one.)"
        );
        println!();
        println!("Load a plan into it:  aizen key loadout set <plan> [<plan> …]");
        println!("See what you can load: aizen sub plan ls");
        return Ok(());
    }

    println!("{:<4} {:<32} {:<10} {}", "#", "PLAN", "KIND", "CALLABLE");
    for (i, e) in entries.iter().enumerate() {
        let ok = e
            .get("resolvable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "{:<4} {:<32} {:<10} {}",
            i + 1,
            s(e, "model"),
            s(e, "kind"),
            // `resolvable:false` is still a real entry — unbought, delisted, or selling a different
            // unit than the plan. It stays visible, drawn differently.
            if ok { "yes" } else { "no — not right now" },
        );
    }
    println!();
    println!("{}/{max} used. Order is preference order.", entries.len());
    match entries.iter().find(|e| {
        e.get("resolvable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }) {
        Some(first) => println!(
            "`{auto}` resolves to '{}' — the first callable entry.",
            s(first, "model")
        ),
        // Every entry unusable is the same practical state as empty, and just as invisible.
        None => println!("⚠ No entry is callable right now, so `{auto}` resolves to nothing."),
    }
    Ok(())
}

/// What the server refuses on write, refused here first so the user gets a sentence, not a 400.
fn check_loadout(names: &[String]) -> Result<(), String> {
    if names.len() > MAX_LOADOUT {
        return Err(format!(
            "a loadout holds at most {MAX_LOADOUT} plans (got {})",
            names.len()
        ));
    }
    for (i, n) in names.iter().enumerate() {
        let t = n.trim();
        if t.is_empty() {
            return Err("an entry cannot be blank".into());
        }
        if t.eq_ignore_ascii_case(AUTO) {
            return Err(format!(
                "`{AUTO}` is the reserved name for 'first callable entry' — it cannot be loaded as one"
            ));
        }
        if t.contains('*') {
            return Err(format!(
                "patterns are not allowed in a plan loadout (got: {t})"
            ));
        }
        if t.chars().count() > 200 {
            return Err(format!(
                "'{}…' is longer than 200 characters",
                &t.chars().take(30).collect::<String>()
            ));
        }
        if names[..i].iter().any(|p| p == n) {
            return Err(format!("'{t}' is listed twice"));
        }
    }
    Ok(())
}

/// Refuse a bad list immediately. Called before the id lookup as well as before the write, so a
/// typo costs no round trip — `resolve_id` is itself a request.
fn fail_if_bad(names: &[String]) {
    if let Err(why) = check_loadout(names) {
        eprintln!("✖ {why}");
        std::process::exit(2);
    }
}

async fn put_loadout(id: &str, names: Vec<String>, json: bool) -> Result<()> {
    fail_if_bad(&names);
    let empty = names.is_empty();
    let out = account::put(&loadout_route(id), json!({ "models": names }))
        .await
        .unwrap_or_else(|e| bail(e));
    if json {
        return print_json(&out);
    }
    if empty {
        println!(
            "✔ Loadout cleared — the plan key can now call NOTHING until something is loaded."
        );
    } else {
        println!("✔ Loadout set ({} entries).", names.len());
    }
    // Show the result the server actually stored, including which entries are callable.
    loadout_ls(Some(id.to_string()), false).await
}

async fn loadout_set(models: Vec<String>, id: Option<String>, json: bool) -> Result<()> {
    fail_if_bad(&models);
    let id = resolve_id(id).await;
    put_loadout(&id, models, json).await
}

/// `add`/`rm` on a route that only accepts a whole list: read, modify, write.
async fn loadout_edit(
    models: Vec<String>,
    id: Option<String>,
    json: bool,
    adding: bool,
) -> Result<()> {
    if models.is_empty() {
        eprintln!("✖ name at least one plan");
        std::process::exit(2);
    }
    if adding {
        fail_if_bad(&models);
    }
    let id = resolve_id(id).await;
    let current = account::get(&loadout_route(&id))
        .await
        .unwrap_or_else(|e| bail(e));
    let mut names = entry_names(&current);

    if adding {
        for m in models {
            if names.iter().any(|n| n == &m) {
                eprintln!("• '{m}' is already loaded — leaving the order alone");
                continue;
            }
            names.push(m);
        }
    } else {
        let before = names.len();
        names.retain(|n| !models.iter().any(|m| m == n));
        if names.len() == before {
            eprintln!("✖ none of those are in the loadout — `aizen key loadout ls` shows what is");
            std::process::exit(2);
        }
    }
    put_loadout(&id, names, json).await
}

/* ---------------------------------------------------------------- models */

/// A combo's `models[]` may be names or objects; take whichever shape arrives.
fn model_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => {
            let n = s(v, "name");
            if n.is_empty() {
                s(v, "id")
            } else {
                n
            }
        }
    }
}

async fn models(only: Option<&str>, json: bool) -> Result<()> {
    let id = resolve_id(None).await;
    let loadout = account::get(&loadout_route(&id))
        .await
        .unwrap_or_else(|e| bail(e));
    let combos_doc = account::get("/auth/combos")
        .await
        .unwrap_or_else(|e| bail(e));
    let combos = combos_doc
        .get("combos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let entries: Vec<Value> = loadout
        .get("loadout")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| only.is_none_or(|want| s(e, "model") == want))
        .collect();

    if json {
        let joined: Vec<Value> = entries
            .iter()
            .map(|e| {
                let name = s(e, "model");
                let combo = combos
                    .iter()
                    .find(|c| s(c, "listing_id") == name || s(c, "name") == name);
                json!({
                    "plan": name,
                    "resolvable": e.get("resolvable").cloned().unwrap_or(Value::Null),
                    "models": combo.and_then(|c| c.get("models").cloned()).unwrap_or(Value::Array(vec![])),
                })
            })
            .collect();
        return print_json(&Value::Array(joined));
    }

    if entries.is_empty() {
        match only {
            Some(w) => {
                println!("'{w}' is not in the loadout — `aizen key loadout ls` shows what is.")
            }
            None => println!(
                "The loadout is empty, so there is nothing to call. See `aizen key loadout ls`."
            ),
        }
        return Ok(());
    }

    let mut any_models = false;
    for e in &entries {
        let name = s(e, "model");
        let callable = e
            .get("resolvable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "{name}{}",
            if callable {
                ""
            } else {
                "   (not callable right now)"
            }
        );
        let combo = combos
            .iter()
            .find(|c| s(c, "listing_id") == name || s(c, "name") == name);
        match combo
            .and_then(|c| c.get("models"))
            .and_then(|m| m.as_array())
        {
            Some(ms) if !ms.is_empty() => {
                any_models = true;
                for m in ms {
                    println!("  · {}", model_name(m));
                }
            }
            _ => println!("  (no models listed)"),
        }
        println!();
    }

    // An empty answer here is very likely the server's plan→combo mapping being unpopulated rather
    // than anything wrong on this side. Say which, so nobody debugs a correct client.
    if !any_models {
        println!("No plan lists any model yet.");
        println!("That is a server-side mapping that is still empty — a listing can be on sale");
        println!("without any plan having opened it. It is not a fault in this command:");
        println!("`aizen sub combo ls` shows what exists on the shelf today.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The flag decides. It has to: a plan key issued through the admin door is labelled with the
    /// owner's email, and matching `main` reported those accounts as having no plan key at all.
    #[test]
    fn the_plan_key_is_found_by_its_flag() {
        assert!(is_main(
            &json!({ "label": "owner@example.com", "plan_key": true })
        ));
        assert!(!is_main(&json!({ "label": "main", "plan_key": false })));
        assert!(!is_main(&json!({ "label": "laptop", "plan_key": false })));
    }

    /// A row with no flag comes from a server predating it, where the reserved name was the only
    /// marker available. Dropping the fallback would break every account until the deploy lands.
    #[test]
    fn without_the_flag_the_reserved_name_still_answers() {
        assert!(is_main(&json!({ "label": "main" })));
        assert!(is_main(&json!({ "label": "MAIN" })));
        assert!(is_main(&json!({ "label": "Main" })));
        assert!(!is_main(&json!({ "label": "main-2" })));
        assert!(!is_main(&json!({ "label": "" })));
        assert!(!is_main(&json!({})));
    }

    /// `/auth/keys/{id}` is DELETE/revoke — the id must land at the END of these two routes.
    #[test]
    fn the_id_goes_last_in_the_loadout_route() {
        assert_eq!(loadout_route("k1"), "/auth/keys/loadout/k1");
        assert!(!loadout_route("k1").starts_with("/auth/keys/k1"));
    }

    #[test]
    fn a_loadout_refuses_what_the_server_refuses() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(check_loadout(&v(&["a", "b"])).is_ok());
        assert!(
            check_loadout(&v(&[])).is_ok(),
            "clearing is legal, if drastic"
        );
        assert!(check_loadout(&v(&["a", "a"])).is_err(), "duplicate");
        assert!(check_loadout(&v(&["auto"])).is_err(), "reserved name");
        assert!(
            check_loadout(&v(&["AUTO"])).is_err(),
            "reserved name, any case"
        );
        assert!(check_loadout(&v(&["aizen/*"])).is_err(), "pattern");
        assert!(check_loadout(&v(&[""])).is_err(), "blank");
        assert!(check_loadout(&v(&["x"; 11])).is_err(), "over the ceiling");
        let long = "x".repeat(201);
        assert!(check_loadout(&[long]).is_err(), "over 200 chars");
    }

    /// Bare, wrapped, or under the other name — all three shapes have shown up on this route.
    #[test]
    fn the_key_string_survives_every_shape_it_arrives_in() {
        assert_eq!(key_string(&json!("ak_abc")), "ak_abc");
        assert_eq!(key_string(&json!({ "key": "ak_abc" })), "ak_abc");
        assert_eq!(key_string(&json!({ "api_key": "ak_abc" })), "ak_abc");
        assert_eq!(
            key_string(&json!({ "key_prefix": "ak_abc" })),
            "",
            "a prefix is not the key"
        );
    }

    #[test]
    fn model_entries_may_be_strings_or_objects() {
        assert_eq!(model_name(&json!("gpt-4o")), "gpt-4o");
        assert_eq!(model_name(&json!({ "name": "gpt-4o" })), "gpt-4o");
        assert_eq!(model_name(&json!({ "id": "gpt-4o" })), "gpt-4o");
    }
}
