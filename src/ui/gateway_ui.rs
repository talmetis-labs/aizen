//! `aizen login` / `aizen gateway …` — the terminal side of pinning this machine to the gateway.
//!
//! Presentation only; the protocol, the persistence and every rule about what may be printed live
//! in [`crate::llm::gateway`]. What this file decides is what a person sees, and there are three
//! decisions in it worth naming:
//!
//! **The code is printed even when a browser opened.** The link opening in the wrong browser
//! profile is the ordinary case, not the exotic one, and a server over SSH has no browser at all.
//!
//! **The code is printed with a sentence saying THIS machine asked for it.** The short code is a
//! funnel: an attacker runs step 1 on their own machine and sends the code to a victim with a
//! story. Nothing here ever accepts a code from the user — it only ever hands one out — and the
//! line above the code says so, because that is the only defence a display can offer.
//!
//! **A wait is not an error.** `pending` comes back as HTTP 200 and is drawn as a quiet dot, not as
//! a warning. Any HTTP library will call a 4xx an error; the thing that would make this feel broken
//! is thirty lines of red at somebody who is doing exactly the right thing.

use crate::cli_args::{
    GatewayCmd, GatewayEnvArgs, GatewayLoginArgs, GatewayLogoutArgs, GatewayStatusArgs,
};
use crate::llm::gateway::{self, Adopted, GatewayConfig, Pin, Start};
use crate::llm::oauth_codex::open_browser;
use anyhow::{bail, Result};
use console::style;

pub(crate) async fn run_gateway(cmd: GatewayCmd) -> Result<()> {
    match cmd {
        GatewayCmd::Login(args) => login(args).await,
        GatewayCmd::Status(args) => status(args).await,
        GatewayCmd::Env(args) => env(args),
        GatewayCmd::Logout(args) => logout(args).await,
    }
}

/* ------------------------------------------------------------------ login */

pub(crate) async fn login(args: GatewayLoginArgs) -> Result<()> {
    if let Some(url) = args.gateway.as_deref() {
        gateway::set_gateway(url);
    }
    let root = gateway::gateway_url();
    let quiet = args.json;

    if !quiet {
        println!();
        println!(
            "{} {}",
            style("Pinning this machine to").dim(),
            style(&root).cyan()
        );
        // Said, not asked: somebody who typed this command asked for a key and gets one. But since
        // 2026-09-06 a signed-in machine already calls models without one, and a person who does
        // not know that is about to mint a string they will then have to look after.
        if crate::llm::account::signed_in() {
            println!(
                "  {}",
                style(
                    "You are already signed in, and the plan works without a key — this pins one \
                     as well. `aizen account logout` if that is not what you wanted."
                )
                .dim()
            );
        }
    }

    let opts = gateway::PairOpts {
        name: args.name.clone(),
        kind: "cli".into(),
        profile: args.profile.clone(),
        activate: !args.no_activate,
    };

    let adopted = gateway::pair(&opts, |start| {
        if quiet {
            return;
        }
        show_code(start);
        if !args.no_browser {
            open_browser(start.link());
        }
        println!(
            "  {}",
            style("Waiting for you to approve it… (Ctrl-C to stop)").dim()
        );
    })
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&adopted.pin)?);
        return Ok(());
    }
    report(&adopted);
    Ok(())
}

/// The one screen this whole command exists to draw. Both halves — the page and the code — because
/// either one alone is a dead end for somebody.
pub(crate) fn show_code(start: &Start) {
    let code = start.code();
    println!();
    println!(
        "  1. Open  {}",
        style(start.verification_uri.trim()).cyan().underlined()
    );
    println!("  2. Enter this code:  {}", style(code).bold().yellow());
    println!();
    // The one sentence that stands between a user and a phished pin.
    println!(
        "  {}",
        style("This code was asked for BY THIS MACHINE, just now. If it reached you any other")
            .dim()
    );
    println!(
        "  {}",
        style("way — a message, an email, someone reading it out — do not approve it.").dim()
    );
    println!();
    let mins = start.expires_in / 60;
    if mins > 0 {
        println!(
            "  {}",
            style(format!("The code stops working in {mins} minutes.")).dim()
        );
    }
}

fn report(adopted: &Adopted) {
    let pin = &adopted.pin;
    println!();
    println!(
        "{} pinned to {}",
        style("✓").green().bold(),
        style(&pin.gateway).cyan()
    );
    // The prefix belongs to the PLAN KEY, not to what this machine now carries — pairing hands
    // back a credential of this device alone, and printing its head under the word "key" named a
    // string that is nowhere near this machine.
    if !pin.key_prefix.is_empty() {
        println!(
            "  plan key {}…  (the row on your keys screen; this machine holds its own credential)",
            pin.key_prefix
        );
    }

    // Name and row id on ONE line: they are two halves of one answer to "which machine is this
    // in the dashboard", and two rows both labelled `device` read as two different devices.
    match (pin.label.as_str(), pin.device_id.as_str()) {
        ("", "") => {}
        (name, "") => println!("  device   {name}"),
        ("", id) => println!("  device   {id} — unpin this row to cut this machine"),
        (name, id) => println!("  device   {name} ({id}) — unpin this row to cut this machine"),
    }
    println!("  profile  {}", pin.profile);
    println!("  endpoint {}", pin.openai_base_url);
    if !pin.model.is_empty() {
        println!("  model    {}", pin.model);
    }
    if !pin.plan.is_empty() {
        println!("  plan     {}", pin.plan);
    }
    println!(
        "  config   {}",
        crate::core::cli_config::config_path().display()
    );

    match (&adopted.warning, adopted.activated) {
        (Some(w), _) => println!("\n{} {w}", style("!").yellow().bold()),
        (None, true) => {
            println!("\n  This endpoint is now the one `aizen` uses. Try: aizen chat \"hello\"")
        }
        (None, false) => println!(
            "\n  Saved, not switched on. Use it with: aizen config provider use {}",
            pin.profile
        ),
    }
}

/* ----------------------------------------------------------------- status */

async fn status(args: GatewayStatusArgs) -> Result<()> {
    if let Some(url) = args.gateway.as_deref() {
        gateway::set_gateway(url);
    }
    let pin = gateway::load_pin();
    let Some((profile, key)) = gateway::key_for(args.profile.as_deref()) else {
        // "The session ended" and "this machine was never pinned" look identical from here — both
        // have no key — and they send a person down two different paths. The pin file is what tells
        // them apart, so this branch asks it rather than guessing.
        let expired = gateway::session() == Some(gateway::Session::Expired);
        if args.json {
            println!(
                "{}",
                serde_json::json!({
                    "pinned": false,
                    "session": if expired { "expired" } else { "none" },
                })
            );
            return Ok(());
        }
        println!(
            "{}",
            if expired {
                "The gateway ended this machine's session, so its key was removed. \
                 Run `aizen login` to pin it again."
            } else {
                "Not pinned to a gateway on this machine. Run `aizen login` to pin it."
            }
        );
        return Ok(());
    };

    let cfg = gateway::config(&key).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    show_status(pin.as_ref(), &profile, &cfg);
    Ok(())
}

fn show_status(pin: Option<&Pin>, profile: &str, cfg: &GatewayConfig) {
    println!();
    println!(
        "{} {}",
        style("gateway").dim(),
        style(gateway::gateway_url()).cyan()
    );
    if !profile.is_empty() {
        println!("{}  {profile}", style("profile").dim());
    } else if let Some(p) = pin {
        println!("{}  {}", style("profile").dim(), p.profile);
    }
    // Read after the call above, not from the `pin` this function was handed: that copy was loaded
    // before `config` stamped it, so it would report the session as one recheck older than it is.
    if let Some(line) = gateway::session_line() {
        println!("{}  {line}", style("session").dim());
    }
    if !cfg.key.key_prefix.is_empty() {
        let label = if cfg.key.label.is_empty() {
            String::new()
        } else {
            format!("  ({})", cfg.key.label)
        };
        println!("{} {}…{label}", style("plan key").dim(), cfg.key.key_prefix);
    }
    if let Some(pin) = gateway::load_pin() {
        if !pin.device_id.is_empty() {
            println!("{}   {}", style("device").dim(), pin.device_id);
        }
    }
    // The one line that has to reach a machine pinned before per-device credentials: it works, it
    // just cannot be switched off from anywhere. Said here rather than on every run, because
    // nothing is broken and nothing is urgent.
    if gateway::pin_is_legacy() == Some(true) {
        println!();
        println!(
            "{}",
            style(
                "This machine was pinned before per-device credentials, so it carries the \
                 account's shared key: unpinning it in the dashboard will not block it. Run \
                 `aizen login` again to swap it for one that can be cut."
            )
            .yellow()
        );
    }

    println!();
    println!("{}", style("endpoints").dim());
    println!("  OpenAI-shaped     {}", gateway::openai_base(Some(cfg)));
    println!("  Anthropic-shaped  {}", gateway::anthropic_base(Some(cfg)));

    println!();
    println!("{}", style("models").dim());
    if cfg.models.default.is_empty() {
        println!("  default   {}", style("(none set on this key)").dim());
    } else {
        println!("  default   {}", cfg.models.default);
    }
    if !cfg.models.loadout.is_empty() {
        println!("  loadout   {}", cfg.models.loadout.join(", "));
    }
    // `auto` only means something when the key has a loadout behind it. Saying so beats a user
    // typing `auto` and reading a 404 as a broken gateway.
    println!(
        "  auto      {}",
        if cfg.models.auto {
            style("usable on this key").green().to_string()
        } else {
            style("not usable on this key (no loadout)")
                .dim()
                .to_string()
        }
    );

    if cfg.karma.plan.is_empty() && cfg.karma.spendable.is_none() {
        // nothing to say
    } else {
        println!();
        println!("{}", style("karma").dim());
        if !cfg.karma.plan.is_empty() {
            println!("  plan      {}", cfg.karma.plan);
        }
        if let Some(k) = cfg.karma.spendable {
            println!(
                "  spendable {k}{}",
                if cfg.karma.enforced {
                    ""
                } else {
                    "  (not enforced)"
                }
            );
        }
    }

    let has_limits = cfg.limits.requests_per_minute.is_some()
        || cfg.limits.requests_per_day.is_some()
        || !cfg.limits.allowed_endpoints.is_empty();
    if has_limits {
        println!();
        println!("{}", style("limits").dim());
        if let Some(n) = cfg.limits.requests_per_minute {
            println!("  per minute  {n}");
        }
        if let Some(n) = cfg.limits.requests_per_day {
            println!("  per day     {n}");
        }
        if !cfg.limits.allowed_endpoints.is_empty() {
            println!("  endpoints   {}", cfg.limits.allowed_endpoints.join(", "));
        }
    }

    if !cfg.checked_at.is_empty() {
        println!();
        println!("{}", style(format!("as of {}", cfg.checked_at)).dim());
    }
}

/* -------------------------------------------------------------------- env */

/// The two roots as environment variables, for the tools that are not this CLI.
///
/// Printed from what the gateway said (the pin file remembers both), never assembled — the whole
/// point of the gateway stating two roots is that the second one is not the first one plus a
/// suffix a client guesses.
fn env(args: GatewayEnvArgs) -> Result<()> {
    let Some(pin) = gateway::load_pin() else {
        bail!("no gateway pin on this machine — run `aizen login` first");
    };
    let mut lines = vec![
        ("OPENAI_BASE_URL", pin.openai_base_url.clone()),
        ("ANTHROPIC_BASE_URL", pin.anthropic_base_url.clone()),
    ];
    if args.with_key {
        // Opt-in, and opt-in for a reason: this writes the key into terminal scrollback, shell
        // history if it is captured, and whatever is scraping the CI log.
        let Some((_, key)) = gateway::key_for(Some(&pin.profile)) else {
            bail!("the pin exists but its key is not in the config any more — run `aizen login`");
        };
        lines.push(("OPENAI_API_KEY", key.clone()));
        lines.push(("ANTHROPIC_API_KEY", key));
    }
    for (name, value) in lines {
        if value.is_empty() {
            continue;
        }
        if args.export {
            #[cfg(windows)]
            println!("$env:{name} = \"{value}\"");
            #[cfg(not(windows))]
            println!("export {name}=\"{value}\"");
        } else {
            println!("{name}={value}");
        }
    }
    if !args.with_key {
        eprintln!(
            "{}",
            style(
                "(the key is not printed; add --with-key if you really want it in this terminal)"
            )
            .dim()
        );
    }
    Ok(())
}

/* ----------------------------------------------------------------- logout */

/// `aizen logout` — leave Aizen on this machine: the account session **and** the local gateway key.
///
/// One word for both, because from outside they are one thing: being signed in to Aizen. That was
/// not true while the two doors bought different things, and it became true on 2026-09-06 when the
/// plan started riding the session — after which a `logout` that dropped only the key left somebody
/// who typed the word still able to spend. `aizen gateway logout` is still the narrow one, for the
/// key alone, and `aizen account logout` for the session alone.
///
/// Neither half revokes anything, and both say so: the token stays valid on other machines until it
/// expires, and the key stays live until the device is unpinned in the dashboard.
pub(crate) async fn logout_all(args: GatewayLogoutArgs) -> Result<()> {
    for line in left_lines(&gateway::leave(args.profile.as_deref()).await) {
        println!("{line}");
    }
    Ok(())
}

/// What the gateway said when this machine asked to be cut, as lines.
///
/// Three outcomes and three different things for a person to do next, which is the whole reason
/// this is not one sentence: **cut** is finished (the row stops working on its next call);
/// **legacy** cannot be cut from here at all and needs a re-pair to become cuttable; **trouble**
/// means the local half is done and the row is still in the dashboard.
///
/// `None` — a caller that had no runtime to ask from — says nothing, rather than implying either.
pub(crate) fn unpair_lines(unpaired: Option<&gateway::Unpaired>) -> Vec<String> {
    let Some(u) = unpaired else {
        return Vec::new();
    };
    if u.cut {
        let which = if u.device_id.is_empty() {
            String::new()
        } else {
            format!(" (device {})", u.device_id)
        };
        return vec![
            format!(
                "{} this machine is unpinned at the gateway{which}",
                style("✓").green().bold()
            ),
            format!(
                "  {}",
                style(
                    "Its credential stops working on the next call. No other machine is affected."
                )
                .dim()
            ),
        ];
    }
    if u.legacy {
        return vec![
            format!(
                "{} {}",
                style("!").yellow().bold(),
                style("this pairing predates per-device credentials, so nothing could be cut here")
                    .yellow()
            ),
            format!(
                "  {}",
                style(
                    "It carries the account's shared plan key. Unpinning it in the dashboard does \
                     not block it either — sign in again with `aizen login` on any machine you \
                     want to be able to cut."
                )
                .dim()
            ),
        ];
    }
    if let Some(why) = &u.trouble {
        return vec![
            format!(
                "{} {}",
                style("!").yellow().bold(),
                style(format!("the gateway was not told: {why}")).yellow()
            ),
            format!(
                "  {}",
                style(
                    "The local half is done either way. Unpin this device in the dashboard to cut \
                     it at the server."
                )
                .dim()
            ),
        ];
    }
    Vec::new()
}

/// What a `leave` is worth saying, as lines — so `aizen logout` can `println!` them and the REPL's
/// `/logout` can push the same words through `tui::emit_line` without a second copy of the wording.
///
/// Already styled: the callers differ in how a line REACHES the screen, not in what it says.
pub(crate) fn left_lines(left: &gateway::Left) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(unpair_lines(left.unpaired.as_ref()));
    if left.signed_out {
        out.push(format!(
            "{} signed out — the account session is gone from this machine",
            style("✓").green().bold()
        ));
        out.push(format!(
            "  {}",
            style("Nothing was revoked: the token stays valid elsewhere until it expires.").dim()
        ));
    }
    if let Some(name) = &left.key_profile {
        out.push(format!(
            "{} local credential for `{name}` removed",
            style("✓").green().bold()
        ));
        // The old sentence sent people to the dashboard to finish the job. For a per-device token
        // the job is finished — the gateway cut the row above, and it stops working on the next
        // call. Only a pre-2026-09-06 pin still needs that trip, and it gets it in `unpair_lines`.
    }
    // Only ever a fault, never the ordinary "nothing was pinned" — that one is `key_profile: None`.
    if let Some(e) = &left.key_error {
        out.push(format!(
            "  {}",
            style(format!("the pinned key was left alone: {e}")).dim()
        ));
    }
    // Said only when nothing else happened. Beside a logout that did what was asked, "nothing
    // pinned here" is noise.
    if out.is_empty() {
        out.push("Not signed in, and nothing pinned on this machine — nothing to do.".to_string());
    }
    out
}

pub(crate) async fn logout(args: GatewayLogoutArgs) -> Result<()> {
    // The narrow command still cuts the machine. It has to: "logout" means the same thing here as
    // in `aizen logout`, and since each machine carries its own credential the gateway can now
    // honour it. Leaving this one local-only would make the narrower word do the weaker thing
    // silently, and the device row would stay live in the dashboard.
    let unpaired = gateway::unpair(args.profile.as_deref()).await;
    let Some(name) = gateway::forget(args.profile.as_deref())? else {
        println!("Nothing pinned on this machine — nothing to do.");
        return Ok(());
    };
    for line in unpair_lines(Some(&unpaired)) {
        println!("{line}");
    }
    println!(
        "{} local credential for `{name}` removed",
        style("✓").green().bold()
    );
    Ok(())
}
