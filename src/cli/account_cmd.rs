//! `aizen account …` — signing in, which is how an Aizen subscription is bought.
//!
//! This is the front door now. A sign-in yields a session JWT, and that token opens both prefixes:
//! `/auth/*`, where plans, marketplace subscriptions and paid plugins live, and `/v1/*`, where
//! models are called. Nothing else is needed and nothing else is stored — the plan has no key
//! string, and no route emits one.
//!
//! `aizen login` is the OTHER door, and it is the older one: it pins this machine to the gateway
//! and comes back with a key. It stays for the machines that cannot open a browser at all — an SSH
//! session, a container, a CI box. With a browser, this door is the one to use, because it leaves
//! no string on disk.
//!
//! **The browser is the default here, and `--password` is the exception.** It used to be the other
//! way round, which served the minority: most accounts are created through Google or GitHub, their
//! `password_hash` is NULL, and a password prompt tells them they typed their own password wrong.
//!
//! The token lives 30 days and there is no refresh route. A `401` in the middle of a run means
//! sign in again — never a retry — and a password change anywhere kills every token minted before
//! it, so a sudden `401` is as likely to be that as an expiry.

use crate::cli_args::{AccountCmd, AccountLoginArgs};
use crate::llm::account::{self, AuthError, Session};
use anyhow::Result;
use std::io::IsTerminal;

/// Print the failure the way the user can act on it, then leave with the documented code.
fn bail(e: AuthError) -> ! {
    eprintln!("✖ {e}");
    std::process::exit(e.exit_code())
}

pub async fn run(cmd: AccountCmd) -> Result<()> {
    match cmd {
        AccountCmd::Login(args) => login(args).await,
        AccountCmd::Whoami { json } => whoami(json).await,
        AccountCmd::Logout => {
            if account::clear() {
                println!("Signed out — the token is gone from this machine.");
                println!("  Nothing was revoked: it stays valid elsewhere until it expires.");
                println!("  A pinned gateway key, if this machine has one, is untouched — `aizen logout` drops that.");
            } else {
                println!("Not signed in — nothing to do.");
            }
            Ok(())
        }
    }
}

async fn login(args: AccountLoginArgs) -> Result<()> {
    let web = args
        .web_url
        .as_deref()
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(account::web_url);

    let session = if args.password {
        password_login(&args, &web).await?
    } else {
        // The server's own sentence, not a summary of it: "no such code", "already used" and
        // "expired" send you to three different places, and only `AuthError`'s status keeps them
        // apart once they are on the screen.
        browser_login(&web).await.unwrap_or_else(|e| bail(e))
    };

    account::save(&session)?;
    if session.email.is_empty() {
        println!("Signed in.");
    } else {
        println!("Signed in as {}.", session.email);
    }
    if web != account::DEFAULT_WEB {
        println!("  host: {web}");
    }
    // What signing in bought, said here rather than leaving the first model call to imply it. No
    // key is mentioned because there is none: the session is the credential at `/v1` too.
    println!("  Your plan is ready to call — no key needed. Try `aizen models`.");
    Ok(())
}

/// The door most accounts come through: a browser, a loopback redirect, and a code traded for a JWT.
///
/// Returns its failure rather than leaving: `aizen config` calls this in the middle of a setup, and
/// exiting there would throw away every answer given so far. `aizen account login` turns the same
/// error into the documented exit code at its own call site.
pub(crate) async fn browser_login(web: &str) -> Result<Session, AuthError> {
    let flow = account::browser_start(web).await?;

    // Printed as well as opened. Over SSH, in a container, or with a browser that refuses to be
    // launched, the printed line is the whole flow — and it costs nothing when the window did open.
    //
    // `AIZEN_NO_BROWSER` prints and waits without spawning anything. That is the honest mode for a
    // headless box and for testing the loopback half on its own; a flow that can only be exercised
    // by popping a window is a flow nobody checks.
    let quiet = crate::core::cli_config::branded_flag("NO_BROWSER");
    if quiet {
        println!("Open this link to sign in:");
    } else {
        println!("Opening your browser to sign in. If it did not open, use this link:");
    }
    println!("  {}", flow.url);
    println!("Waiting…");
    if !quiet {
        crate::llm::oauth_codex::open_browser(&flow.url);
    }

    account::browser_finish(web, flow).await
}

/// The older door, kept behind a flag for accounts that really do have a password.
async fn password_login(args: &AccountLoginArgs, web: &str) -> Result<Session> {
    let theme = dialoguer::theme::ColorfulTheme::default();

    let email = match args.email.clone() {
        Some(e) => e,
        None => {
            if !std::io::stdin().is_terminal() {
                eprintln!("✖ no email — pass --email <you@example.com> (stdin is not a terminal)");
                std::process::exit(2);
            }
            dialoguer::Input::<String>::with_theme(&theme)
                .with_prompt("Email")
                .interact_text()?
        }
    };
    if email.trim().is_empty() {
        eprintln!("✖ an email is required");
        std::process::exit(2);
    }

    if !std::io::stdin().is_terminal() {
        eprintln!("✖ cannot ask for a password: stdin is not a terminal");
        std::process::exit(2);
    }
    // Never echoed, never stored — only the token it buys is written to disk.
    let password = dialoguer::Password::with_theme(&theme)
        .with_prompt("Password")
        .interact()?;
    if password.is_empty() {
        eprintln!("✖ a password is required");
        std::process::exit(2);
    }

    match account::login(web, email.trim(), &password).await {
        Ok(s) => Ok(s),
        Err(e) => {
            eprintln!("✖ login failed: {e}");
            // An account created through Google/GitHub has no password to check — `/auth/login`
            // cannot help there, and "wrong password" would send them in circles. Point at the door
            // that does work rather than at a password reset they do not need.
            if matches!(&e, AuthError::Http { status, .. } if *status == 400 || *status == 401) {
                eprintln!("  If you signed up with Google or GitHub, drop --password: `aizen account login` opens the browser.");
            }
            std::process::exit(e.exit_code());
        }
    }
}

/// Whether `/auth/me` says the session is live.
///
/// The trap this exists for: that route is the ONE `/auth/*` route that does not answer 401 without
/// a session — it answers `200 {"authenticated": false}`. Reading the status code instead would
/// print a table of empty strings and exit 0 for a token the server has already stopped honouring,
/// which is the exact opposite of what `whoami` is for. The field is the answer; the status is not.
///
/// An absent field counts as live: a server build that never sends it would otherwise lock people
/// out of a session that works. Only an explicit `false` is a refusal.
///
/// A token the server rejects usually means the password was changed somewhere else — that kills
/// every token issued before it, and no amount of retrying reaches a different answer.
fn me_says_live(me: &serde_json::Value) -> bool {
    me.get("authenticated").and_then(|v| v.as_bool()) != Some(false)
}

async fn whoami(json: bool) -> Result<()> {
    let me = match account::me().await {
        Ok(v) => v,
        Err(e) => bail(e),
    };
    let s = |k: &str| me.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let b = |k: &str| me.get(k).and_then(|v| v.as_bool());

    if !me_says_live(&me) {
        bail(AuthError::Http {
            status: 401,
            message: "session expired — run `aizen account login`".into(),
        });
    }

    // The token identifies the session; it is never part of the answer.
    let out = serde_json::json!({
        "email": s("email"),
        "userId": s("userId"),
        "tenantId": s("tenantId"),
        "role": s("role"),
        "emailVerified": b("emailVerified"),
        "hasPassword": b("hasPassword"),
        "host": account::web_url(),
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("email     {}", s("email"));
    println!("tenant    {}", s("tenantId"));
    println!("role      {}", s("role"));
    println!("host      {}", account::web_url());
    if b("hasPassword") == Some(false) {
        println!("password  not set (this account signs in with Google/GitHub)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The regression: `whoami` read the status code, and `/auth/me` answers 200 with no session.
    #[test]
    fn a_dead_session_is_the_field_not_the_status() {
        assert!(!me_says_live(&json!({ "authenticated": false })));
        assert!(me_says_live(&json!({ "authenticated": true })));
        // No field at all — an older server. Live, rather than locking a working session out.
        assert!(me_says_live(&json!({ "email": "a@b.c" })));
    }
}
