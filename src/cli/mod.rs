//! One module per top-level `aizen <subcommand>`: the argument shapes live in `cli_args`, the
//! behaviour lives here. Split out of `main.rs`, which now only dispatches.

pub mod account_cmd;
pub mod agents_cmd;
pub mod apps;
pub mod coop_cmd;
pub mod custom_cmd;
pub mod key_cmd;
pub mod sub_cmd;
pub mod memory_cmd;
pub mod persona_cmd;
pub mod run_cmds;
pub mod sandbox_cmd;
pub mod sessions;
pub mod skill_cmd;
pub mod time;
pub mod where_report;

enum Decision {
    Yes,
    No,
    NeedYes,
}

/// Confirm something irreversible. `--yes` skips the prompt; a non-TTY without `--yes` is a hard
/// stop (`NeedYes`) rather than an implicit yes — a piped `buy` must not spend, and a piped
/// `rotate` must not invalidate the key every other machine is holding.
fn confirm(lines: &[String], yes: bool) -> Decision {
    for l in lines {
        eprintln!("{l}");
    }
    if yes {
        return Decision::Yes;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
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
pub(crate) fn gate(lines: &[String], yes: bool) -> Option<i32> {
    match confirm(lines, yes) {
        Decision::Yes => None,
        Decision::No => {
            println!("Cancelled.");
            Some(0)
        }
        Decision::NeedYes => Some(2),
    }
}

/// Read all of stdin, for the `-`/omitted body argument every "save a document" subcommand accepts.
pub(crate) fn read_stdin(ctx: &'static str) -> anyhow::Result<String> {
    use anyhow::Context;
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context(ctx)?;
    // Strip a leading UTF-8 BOM (PowerShell's `|` prepends one) before trimming.
    Ok(buf
        .strip_prefix('\u{FEFF}')
        .unwrap_or(&buf)
        .trim()
        .to_string())
}
