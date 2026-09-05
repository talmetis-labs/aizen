//! `aizen account …` — the Aizen account session, which is NOT the gateway key.
//!
//! `aizen login` pins this machine to the gateway and comes back with an inference key for `/v1/*`.
//! This is the other door: an email + password login that yields a session JWT for `/auth/*`, where
//! plans, marketplace subscriptions and paid plugins live. The two are stored apart and never
//! substituted for one another — see [`crate::llm::account`].

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
                println!("Signed out. The gateway key is untouched — `aizen logout` drops that one.");
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

    let theme = dialoguer::theme::ColorfulTheme::default();

    let email = match args.email {
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

    let session: Session = match account::login(&web, email.trim(), &password).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✖ login failed: {e}");
            // An account created through Google/GitHub may simply have no password to check —
            // `/auth/login` cannot help there, and "wrong password" would send them in circles.
            if matches!(&e, AuthError::Http { status, .. } if *status == 400 || *status == 401) {
                eprintln!("  If you signed up with Google or GitHub, set a password at {web} first.");
            }
            std::process::exit(e.exit_code());
        }
    };

    account::save(&session)?;
    println!("Signed in as {}.", session.email);
    if web != account::DEFAULT_WEB {
        println!("  host: {web}");
    }
    Ok(())
}

async fn whoami(json: bool) -> Result<()> {
    let me = match account::me().await {
        Ok(v) => v,
        Err(e) => bail(e),
    };
    let s = |k: &str| me.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let b = |k: &str| me.get(k).and_then(|v| v.as_bool());

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
