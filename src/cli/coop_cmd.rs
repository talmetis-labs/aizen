//! `aizen zone …`, `aizen team …` and `aizen work …` — the non-interactive twins of `/zone`,
//! `/team` and `/work`.
//!
//! Same registry and same plan as the slash surfaces; these print with `println!` because no
//! retained frame is up when a subcommand runs.

use crate::cli_args::*;
use crate::features::coop;
use crate::features::slash_handlers::team_status_lines;
use crate::ui::{splash, theme};
use anyhow::Result;
use console::style;

pub(crate) fn run_zone(cmd: ZoneCmd) -> Result<()> {
    match cmd {
        ZoneCmd::Migrate { apply } => {
            let plan = crate::features::zones::plan()?;
            println!("current zone: {}", plan.current_slug);
            if plan.legacy.is_empty() {
                println!("no legacy zones found for this project — nothing to merge.");
                return Ok(());
            }
            println!("legacy zones of this project:");
            for z in &plan.legacy {
                println!("  {}", z.summary());
            }
            if !apply {
                println!("\ndry-run — nothing was changed. Re-run with `aizen zone migrate --apply` to merge into {}.", plan.current_slug);
                return Ok(());
            }
            let rep = crate::features::zones::apply(&plan);
            for a in &rep.actions {
                println!("  ✓ {a}");
            }
            for w in &rep.warnings {
                eprintln!("  ⚠ {w}");
            }
            println!(
                "merged {} legacy zone(s) into {}: {} action(s), {} warning(s).",
                plan.legacy.len(),
                plan.current_slug,
                rep.actions.len(),
                rep.warnings.len()
            );
            if !rep.warnings.is_empty() {
                anyhow::bail!("zone migrate finished with warnings — each one above states exactly what moved and what didn't");
            }
            Ok(())
        }
    }
}

/// `aizen team …` — the non-interactive twin of `/team`. Same registry, same plan, `println!` instead
/// of `tui::emit_line` (no retained frame is up here, so a raw print is the correct surface).
pub(crate) fn run_team(cmd: TeamCmd) -> Result<()> {
    match cmd {
        TeamCmd::Status => {
            for line in team_status_lines() {
                println!("{line}");
            }
            Ok(())
        }
        TeamCmd::Diff { session, patch } => {
            let view = coop::resolve(&session)?;
            match coop::session_diff(&view, patch) {
                Err(error) => Err(error),
                Ok(impossible) => match impossible {},
            }
        }
        TeamCmd::Claims => {
            let claims = coop::claims();
            if claims.is_empty() {
                println!("{}", style("no path claims recorded yet").dim());
                return Ok(());
            }
            for (path, claim) in claims {
                println!("  {path}  ← {}", style(&claim.session_id).dim());
            }
            for o in coop::overlaps() {
                println!(
                    "  {} {}  ({} → {})",
                    style("⚠").color256(theme::WARN),
                    o.path,
                    o.first,
                    o.second
                );
            }
            Ok(())
        }
        TeamCmd::Commit { session, .. } => {
            let view = coop::resolve(&session)?;
            match coop::session_commit(&view) {
                Err(error) => Err(error),
                Ok(impossible) => match impossible {},
            }
        }
    }
}

/// `aizen work …` — isolated worktrees, for when two windows should not share one tree at all.
pub(crate) fn run_work(cmd: WorkCmd) -> Result<()> {
    match cmd {
        WorkCmd::New { name } => {
            let wt = coop::work_new(&name)?;
            println!(
                "{} {}\n  branch {}\n  committed base {} (parent's pending edits are not copied)\n  open a session there:  cd {} && aizen",
                style("✓ worktree").color256(splash::ACCENT),
                wt.path.display(),
                wt.branch,
                wt.base.as_str(),
                wt.path.display()
            );
            if wt.dirty {
                println!("  checkout left uncommitted/untracked/ignored files; inspect before using this worktree");
            }
            Ok(())
        }
        WorkCmd::List => {
            for line in coop::work_inventory_lines()? {
                println!("{line}");
            }
            Ok(())
        }
        WorkCmd::Remove { name, force } => {
            let msg = coop::work_remove(&name, force)?;
            println!("{} {msg}", style("✓").color256(splash::ACCENT));
            Ok(())
        }
    }
}
