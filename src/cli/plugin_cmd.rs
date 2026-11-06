//! Native package administration. None of these commands executes a provider.

use crate::cli_args::{MemoryCmd, NativeMemoryPluginCmd};
use crate::plugin::{manager, releases, settings::ExecutionGrant};
use anyhow::{ensure, Context, Result};
use std::io::IsTerminal;

pub(crate) async fn run(command: &NativeMemoryPluginCmd) -> Result<()> {
    match command {
        NativeMemoryPluginCmd::List { json } => {
            let state = manager::read_state()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else if state.packages.is_empty() {
                println!("No Memory plugins installed. Memory is optional.");
            } else {
                for (id, package) in state.packages {
                    println!(
                        "{id}\teligible={}\tselected={}\tmemory_enabled={}",
                        package.enabled,
                        state.selected.as_deref() == Some(&id),
                        state.enabled
                    );
                }
            }
        }
        NativeMemoryPluginCmd::Install { source } => {
            let package = manager::install_local(source)?;
            println!(
                "Installed {} {} at {}.",
                package.manifest.id,
                package.manifest.version,
                package.directory.display()
            );
            println!("Not enabled, selected or executed. Integrity hashes do not establish publisher trust.");
        }
        NativeMemoryPluginCmd::Releases { id, json } => {
            ensure!(
                id == "mnemos",
                "only the public Mnemos release channel is supported"
            );
            let status = releases::check(None).await?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_release_status(&status);
            }
        }
        NativeMemoryPluginCmd::InstallRelease {
            id,
            version,
            sha256,
            yes,
        } => {
            ensure!(
                id == "mnemos",
                "only the public Mnemos release channel is supported"
            );
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            validate_release_confirmation(
                interactive,
                *yes,
                version.as_deref(),
                sha256.as_deref(),
            )?;
            let before = manager::read_state()?;
            ensure!(!before.packages.contains_key("mnemos"),
                "Mnemos is already installed. Use `aizen plugin update mnemos`; existing settings and data were preserved.");
            let status = releases::check(version.as_deref()).await?;
            print_release_status(&status);
            let candidate = status
                .candidate
                .as_ref()
                .context("No installable Mnemos package is available from the public channel")?;
            if let Some(expected) = sha256 {
                ensure!(
                    &candidate.sha256 == expected,
                    "release archive differs from the explicitly pinned SHA256"
                );
            }
            if !*yes {
                let confirmed =
                    dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
                        .with_prompt("Download and install Mnemos without activation?")
                        .default(false)
                        .report(false)
                        .interact()?;
                if !confirmed {
                    return Ok(());
                }
            }
            let installed = releases::install(candidate, before.generation).await?;
            println!(
                "Installed Mnemos {} at {}.",
                installed.manifest.version,
                installed.directory.display()
            );
            println!("Not enabled, selected, authorized or executed. Learning and data adoption remain separate choices.");
        }
        NativeMemoryPluginCmd::Update {
            id,
            version,
            sha256,
            yes,
        } => {
            ensure!(
                id == "mnemos",
                "only the public Mnemos release channel is supported"
            );
            update_mnemos(version.as_deref(), sha256.as_deref(), *yes).await?;
        }
        NativeMemoryPluginCmd::RecoverUpdate {
            id,
            apply,
            operation_id,
            yes,
        } => {
            ensure!(
                id == "mnemos",
                "only Mnemos package-update recovery is supported"
            );
            let status = crate::plugin::update::inspect_recovery()?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            if !*apply || !status.recovery_required {
                return Ok(());
            }
            let operation = status.operation_id.as_deref().context("Update recovery evidence has no valid operation ID; preserve it for manual inspection")?;
            if let Some(expected) = operation_id {
                ensure!(
                    expected == operation,
                    "Recovery operation changed; inspect again"
                );
            }
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            ensure!(
                interactive || (*yes && operation_id.is_some()),
                "Noninteractive recovery requires --apply --operation-id ID --yes"
            );
            ensure!(
                !*yes || operation_id.is_some(),
                "--yes recovery requires the inspected --operation-id"
            );
            if !*yes && !dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt("Settle this exact interrupted package update without touching Memory data?")
                .default(false).report(false).interact()? { return Ok(()); }
            let settled = crate::plugin::update::recover(operation)?;
            println!("{}", serde_json::to_string_pretty(&settled)?);
            println!("No provider was executed and no Memory data was rolled back. Review activation separately.");
        }
        NativeMemoryPluginCmd::Info { id } => {
            let package = manager::info(id)?;
            println!("{}", serde_json::to_string_pretty(&package.manifest)?);
            println!("Verified against host-owned pins; executable was not started.");
        }
        NativeMemoryPluginCmd::Enable { id } => {
            manager::set_eligible(id, true)?;
            println!("Plugin {id} is eligible, but was not selected or started.");
        }
        NativeMemoryPluginCmd::Disable { id } => {
            manager::set_eligible(id, false)?;
            println!("Plugin {id} disabled; selection and provider data preserved.");
        }
        NativeMemoryPluginCmd::Revoke { id } => {
            let state = manager::read_state()?;
            manager::revoke(id, state.generation)?;
            println!("Revoked {id} execution consent; selection/package/data preserved. Selected Memory is disabled.");
        }
        NativeMemoryPluginCmd::Storage { id, path, default } => {
            let review = manager::review(id)?;
            if path.is_some() || *default {
                let cwd = std::env::current_dir()?;
                manager::set_storage(&review, path.clone(), Some(&cwd))?;
            }
            let state = manager::read_state()?;
            let saved = state
                .packages
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("Memory package is not installed"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "provider": id, "automatic": saved.data_root.is_none(),
                    "data_root": manager::data_root_for(id, saved)?,
                    "data_moved": false
                }))?
            );
        }
        NativeMemoryPluginCmd::Configure {
            id,
            values,
            reset,
            context_tokens,
            recall_tokens,
            recall_inject_tokens,
            recall_limit,
            max_records,
        } => {
            let review = manager::review(id)?;
            let mut settings = review.state.settings.clone();
            let mut seen = std::collections::BTreeSet::new();
            for key in reset {
                ensure!(seen.insert(key.as_str()), "duplicate setting edit");
                ensure!(
                    review.package.manifest.settings.contains_key(key),
                    "provider does not declare setting {key}"
                );
                settings.remove(key);
            }
            for assignment in values {
                let (key, value) = assignment
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("expected KEY=JSON_SCALAR"))?;
                ensure!(seen.insert(key), "duplicate or conflicting setting edit");
                let parsed = crate::plugin::memory_api::strict_json(value.as_bytes())?;
                settings.insert(key.to_owned(), parsed);
            }
            let budget_edits = context_tokens.is_some()
                || recall_tokens.is_some()
                || recall_inject_tokens.is_some()
                || recall_limit.is_some()
                || max_records.is_some();
            let mut budgets = review.state.budgets.clone().unwrap_or_default();
            if let Some(value) = context_tokens {
                budgets.context_tokens = usize::from(*value);
            }
            if let Some(value) = recall_tokens {
                budgets.recall_tokens = usize::from(*value);
            }
            if let Some(value) = recall_inject_tokens {
                budgets.recall_inject_tokens = usize::from(*value);
            }
            if let Some(value) = recall_limit {
                budgets.recall_limit = usize::from(*value);
            }
            if let Some(value) = max_records {
                budgets.max_records = usize::from(*value);
            }
            let effective = crate::plugin::configuration::effective_settings(
                &review.package.manifest.settings,
                &settings,
            )?;
            if !values.is_empty() || !reset.is_empty() || budget_edits {
                manager::configure(
                    &review,
                    settings,
                    if budget_edits {
                        Some(budgets.clone())
                    } else {
                        review.state.budgets.clone()
                    },
                )?;
                println!("Provider configuration saved; applies at the next user-turn boundary. No provider executed.");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "provider":id, "schema":review.package.manifest.settings, "effective_settings":effective,
                    "budgets":budgets
                }))?
            );
        }
        NativeMemoryPluginCmd::Uninstall { id, yes } => {
            let directory = manager::uninstall_directory(id)?;
            if let Some(code) = super::gate(
                &[
                    format!("Remove executable package {} at {}?", id, directory.display()),
                    "Provider data is preserved. No provider command will run. Damaged payloads remain disabled with pins retained for recovery.".into(),
                ],
                *yes,
            ) {
                ensure!(
                    code == 0,
                    "uninstall requires explicit confirmation (--yes)"
                );
                return Ok(());
            }
            manager::uninstall(id)?;
            println!("Uninstalled {id}; provider data preserved.");
        }
    }
    Ok(())
}

/// Shared explicit update flow for the CLI, retained slash menu and Memory settings.
pub(crate) async fn update_mnemos(
    version: Option<&str>,
    sha256: Option<&str>,
    yes: bool,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    validate_release_confirmation(interactive, yes, version, sha256)?;
    let review = manager::review("mnemos")?;
    let status = releases::check(version).await?;
    emit_release_status(&status);
    ensure!(
        status.comparison == "newer" && status.discovery == "complete",
        "No strictly newer verified Mnemos package is available; no package was changed"
    );
    let candidate = status
        .candidate
        .context("No installable Mnemos update is available")?;
    if let Some(expected) = sha256 {
        ensure!(
            candidate.sha256 == expected,
            "Release differs from the explicitly pinned SHA256"
        );
    }
    crate::tui::emit_line("Updates the installed package only. Settings, budgets and data location are preserved; Memory data is not opened or migrated.");
    crate::tui::emit_line("Mnemos execution and Learning consent will be revoked. Selected Mnemos is disabled until separately authorized again. No provider will run.");
    if !yes
        && !dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Update this exact Mnemos release and require activation again?")
            .default(false)
            .report(false)
            .interact()?
    {
        return Ok(());
    }
    let package =
        releases::update_checked(&candidate, review.generation, &review.package.pins, || {
            Ok(())
        })
        .await?;
    crate::tui::emit_line(&format!("Updated Mnemos {} — activation required. No provider executed; settings and data preserved.", package.manifest.version));
    Ok(())
}

fn emit_release_status(status: &releases::ReleaseStatus) {
    crate::tui::emit_line(&format!(
        "Mnemos public channel: {} · target {}",
        releases::REPOSITORY,
        status.target
    ));
    crate::tui::emit_line(&format!(
        "Installed: {} · comparison: {}",
        status
            .installed
            .as_ref()
            .and_then(|i| i.version.as_deref())
            .unwrap_or("unverified / absent"),
        status.comparison
    ));
    if let Some(candidate) = &status.candidate {
        crate::tui::emit_line(&format!(
            "Downloadable: {}\nPackage: {}\nBytes: {}\nSHA256: {}",
            candidate.version, candidate.filename, candidate.bytes, candidate.sha256
        ));
    }
    if let Some(reason) = &status.unavailable_reason {
        crate::tui::emit_line(reason);
    }
}

fn validate_release_confirmation(
    interactive: bool,
    yes: bool,
    version: Option<&str>,
    sha256: Option<&str>,
) -> Result<()> {
    if !interactive || yes {
        ensure!(yes && version.is_some() && sha256.is_some(),
            "Noninteractive release installation/update requires --version X.Y.Z --sha256 HEX --yes; no unpinned latest mutation is allowed");
    }
    if let Some(digest) = sha256 {
        crate::plugin::manifest::validate_digest(digest)?;
    }
    Ok(())
}

fn print_release_status(status: &releases::ReleaseStatus) {
    println!("Mnemos public channel: {}", releases::REPOSITORY);
    println!("Target: {}", status.target);
    match &status.installed {
        Some(installed) if installed.verified => println!(
            "Installed: {}",
            installed.version.as_deref().unwrap_or("unknown")
        ),
        Some(_) => {
            println!("Installed: damaged or unverifiable; repair locally before installation")
        }
        None => println!("Installed: none"),
    }
    if let Some(version) = &status.highest_seen_version {
        println!("Highest published version: {version} (metadata alone does not imply an installable package)");
    }
    if let Some(candidate) = &status.candidate {
        println!(
            "Downloadable: {}\nPackage: {}\nBytes: {}\nSHA256: {}",
            candidate.version, candidate.filename, candidate.bytes, candidate.sha256
        );
        println!(
            "Comparison: {}. Installed updates require `aizen plugin update mnemos`.",
            status.comparison
        );
    } else {
        println!(
            "Downloadable: unavailable ({}). Local ZIP/directory installation remains available.",
            status
                .unavailable_reason
                .as_deref()
                .unwrap_or("no installable release")
        );
    }
    println!("Integrity checks are not publisher authentication. Installation never activates or executes a provider.");
}

pub(crate) fn memory_control(command: &MemoryCmd) -> Option<Result<()>> {
    Some(match command {
        MemoryCmd::Status { json } => (|| {
            let status = manager::status()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "Memory: {}\nSelected: {}\nInstalled: {}\nGeneration: {}",
                    status.state,
                    status.selected.as_deref().unwrap_or("none"),
                    status.installed,
                    status.generation
                );
                if status.selected.is_none() {
                    println!("No provider selected. Ordinary coding, Skills, Persona and sessions do not require Memory.");
                }
                if let Some(outbox) = &status.learning_outbox {
                    println!(
                        "Learning receipts: {}/{} (prepared={}, dispatching={}, done={}, failed={}); available={}",
                        outbox.receipts, outbox.capacity, outbox.prepared, outbox.dispatching,
                        outbox.done, outbox.failed, outbox.available
                    );
                    if outbox.receipts > 0 {
                        println!("Advisory inventory only; no replay or cleanup performed. Physical recovery status is independent.");
                    }
                    if outbox.available == 0 {
                        println!("Learning capacity reached. Receipts are preserved; new learning is refused.");
                    }
                } else {
                    println!("Learning outbox cannot be validated. No contents were exposed or changed; inspect the host-owned receipts explicitly.");
                }
            }
            Ok(())
        })(),
        MemoryCmd::Use {
            id,
            yes,
            allow_guarded,
            allow_network,
            allow_learning,
            allow_turn_model,
        } => (|| {
            let review = manager::review(id)?;
            let consent = ExecutionGrant {
                manifest_sha256: review.package.pins.manifest.sha256.clone(),
                guarded_execution: *allow_guarded,
                network: *allow_network,
                learning: *allow_learning,
                turn_model_learning: *allow_turn_model,
            };
            let warnings =
                crate::plugin::configuration::consent_disclosures(&review.package, &consent)?;
            if let Some(code) = super::gate(&warnings, *yes) {
                ensure!(
                    code == 0,
                    "selection requires explicit confirmation (--yes)"
                );
                return Ok(());
            }
            manager::apply_grant(&review, consent, true)?;
            println!(
                "Selected {id}; Memory enabled. Selection itself did not start the executable."
            );
            Ok(())
        })(),
        MemoryCmd::Unselect => (|| {
            let state = manager::read_state()?;
            manager::clear_selection(state.generation)?;
            println!("Memory selection cleared and disabled; saved grants/packages/data preserved. Use `aizen plugin revoke <id>` to revoke consent.");
            Ok(())
        })(),
        MemoryCmd::Enable => manager::set_memory_enabled(true)
            .map(|()| println!("Memory enabled for the explicitly selected provider.")),
        MemoryCmd::Disable => manager::set_memory_enabled(false)
            .map(|()| println!("Memory disabled; selection and data preserved.")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn release_commands_are_under_native_plugin_root_only() {
        use crate::cli_args::{Cli, Commands};
        let checked =
            Cli::try_parse_from(["aizen", "plugin", "releases", "mnemos", "--json"]).unwrap();
        assert!(matches!(
            checked.command,
            Some(Commands::Plugin {
                cmd: NativeMemoryPluginCmd::Releases { json: true, .. }
            })
        ));
        assert!(Cli::try_parse_from(["aizen", "plugin", "releases", "arbitrary"]).is_err());
        let pinned = Cli::try_parse_from([
            "aizen",
            "plugin",
            "install-release",
            "mnemos",
            "--version",
            "0.1.0",
            "--sha256",
            &"a".repeat(64),
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            pinned.command,
            Some(Commands::Plugin {
                cmd: NativeMemoryPluginCmd::InstallRelease { yes: true, .. }
            })
        ));
    }

    #[test]
    fn update_and_recovery_commands_keep_explicit_mutation_boundaries() {
        use crate::cli_args::{Cli, Commands};
        let command = Cli::try_parse_from(["aizen", "plugin", "update", "mnemos"]).unwrap();
        assert!(matches!(
            command.command,
            Some(Commands::Plugin {
                cmd: NativeMemoryPluginCmd::Update { yes: false, .. }
            })
        ));
        let inspect = Cli::try_parse_from(["aizen", "plugin", "recover-update", "mnemos"]).unwrap();
        assert!(matches!(
            inspect.command,
            Some(Commands::Plugin {
                cmd: NativeMemoryPluginCmd::RecoverUpdate {
                    apply: false,
                    yes: false,
                    ..
                }
            })
        ));
        assert!(
            Cli::try_parse_from(["aizen", "plugin", "recover-update", "mnemos", "--yes"]).is_err()
        );
        assert!(Cli::try_parse_from(["aizen", "plugin", "update", "arbitrary"]).is_err());
    }

    #[test]
    fn release_noninteractive_install_requires_explicit_pinned_consent() {
        let sha = "a".repeat(64);
        for (interactive, yes, version, digest) in [
            (false, false, None, None),
            (false, true, None, Some(sha.as_str())),
            (false, true, Some("0.1.0"), None),
            (true, true, None, None),
        ] {
            assert!(validate_release_confirmation(interactive, yes, version, digest).is_err());
        }
        assert!(validate_release_confirmation(false, true, Some("0.1.0"), Some(&sha)).is_ok());
        assert!(validate_release_confirmation(true, false, None, None).is_ok());
        assert!(validate_release_confirmation(true, false, Some("0.1.0"), Some("bad")).is_err());
    }
}
