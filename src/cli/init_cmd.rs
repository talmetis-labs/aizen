//! `aizen init` — the non-interactive twin of `/init`.
//!
//! Same engine (`codebase::build_index`), same flags (`--force` / `--status`); these print with
//! `println!` because no retained frame is up when a subcommand runs. Two deliberate differences
//! from the slash surface:
//!
//! - `--progress` emits machine-readable `PROGRESS {json}` lines (one per decile + phase changes)
//!   so the desktop app can drive a progress bar without scraping human prose.
//! - The verify-command measurement that `/init` always runs afterwards is OFF unless `--verify`
//!   is passed: `verify_gate::init_verify` executes detected build/test commands, and a headless
//!   run (script, desktop button) must not surprise-launch a build.

use crate::agent::{codebase, verify_gate};
use anyhow::Result;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) async fn run_init(
    status: bool,
    force: bool,
    progress: bool,
    verify: bool,
) -> Result<()> {
    // `--status`: print the current index state, no scan.
    if status {
        match codebase::load() {
            Some(idx) => {
                println!(
                    "✓ codebase index: {} file(s), {} chunk(s) — indexed {}",
                    idx.files.len(),
                    idx.chunks.len(),
                    crate::fmt_time_ago(idx.built_unix)
                );
                let summary = codebase::analysis_summary(&idx.analysis);
                if !summary.trim().is_empty() {
                    println!("{summary}");
                }
            }
            None => println!("no codebase index yet — run `aizen init` to build it"),
        }
        return Ok(());
    }

    let incremental = !force;
    println!(
        "{}",
        if force {
            "rebuilding codebase index…"
        } else {
            "indexing codebase…"
        }
    );

    // Phase progress: the same decile throttle as the REPL surface, but each emission is one
    // `PROGRESS {json}` line the desktop parses positionally; the human text stays on the line
    // after it so a plain terminal run still reads fine.
    let last_decile = std::sync::Arc::new(AtomicUsize::new(usize::MAX));
    let ld = last_decile.clone();
    let progress_cb = move |phase: codebase::Phase| {
        let (name, done, total) = match phase {
            codebase::Phase::Scanning { done, total } => {
                if total == 0 {
                    return;
                }
                let decile = done * 10 / total.max(1);
                if ld.swap(decile, Ordering::Relaxed) == decile {
                    return;
                }
                ("scanning", done, total)
            }
            codebase::Phase::Chunking => ("chunking", 0, 0),
            codebase::Phase::Building => ("building", 0, 0),
        };
        if progress {
            println!("PROGRESS {{\"phase\":\"{name}\",\"done\":{done},\"total\":{total}}}");
        } else {
            match phase {
                codebase::Phase::Scanning { done, total } => {
                    println!("  scanning… {}%", done * 10 / total.max(1) * 10)
                }
                codebase::Phase::Chunking => println!("  chunking symbols…"),
                codebase::Phase::Building => println!("  building index…"),
            }
        }
    };

    let result =
        tokio::task::spawn_blocking(move || codebase::build_index(incremental, None, &progress_cb))
            .await;

    match result {
        Ok(Ok(stats)) => {
            let mut parts = vec![
                format!("{} file(s)", stats.indexed),
                format!("{} chunk(s)", stats.chunks),
            ];
            if stats.reused > 0 {
                parts.push(format!("{} reused", stats.reused));
            }
            if stats.added > 0 {
                parts.push(format!("{} updated", stats.added));
            }
            if stats.removed > 0 {
                parts.push(format!("{} removed", stats.removed));
            }
            println!(
                "✓ codebase indexed: {} in {}ms",
                parts.join(", "),
                stats.elapsed_ms
            );
            let mut notes = Vec::new();
            if stats.sensitive > 0 {
                notes.push(format!(
                    "{} sensitive file(s) stored path-only",
                    stats.sensitive
                ));
            }
            if stats.redacted > 0 {
                notes.push(format!("{} file(s) had secrets redacted", stats.redacted));
            }
            if stats.skipped_large > 0 {
                notes.push(format!("{} oversized skipped", stats.skipped_large));
            }
            if stats.skipped_binary > 0 {
                notes.push(format!("{} binary skipped", stats.skipped_binary));
            }
            if stats.capped {
                notes.push("scan hit the file cap (coverage bounded)".to_string());
            }
            if !notes.is_empty() {
                println!("  {}", notes.join(" · "));
            }
            let summary = codebase::analysis_summary(
                &codebase::load().map(|i| i.analysis).unwrap_or_default(),
            );
            if !summary.trim().is_empty() {
                println!("{summary}");
            }
            if verify {
                let root = crate::core::config::project_root();
                println!("  measuring verify commands…");
                for line in verify_gate::init_verify(&root, verify_gate::DEFAULT_TIMEOUT_SECS).await
                {
                    println!("  {line}");
                }
            } else {
                println!(
                    "  verify: skipped (headless default) — pass --verify to measure this project's build/test commands"
                );
            }
            Ok(())
        }
        Ok(Err(e)) => Err(e.context("aizen init")),
        Err(e) => Err(anyhow::anyhow!("scan task failed: {e}")),
    }
}
