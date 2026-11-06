//! Memory settings. Release networking is explicit; no menu action executes provider code.

use super::*;
use crate::plugin::{
    configuration as options, manager, releases,
    settings::{ExecutionGrant, MemoryPlugins},
};
use anyhow::ensure;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(600)
        .collect()
}

fn note(value: &str) {
    tui::emit_line(&safe(value));
}

pub(super) fn report_error(error: &anyhow::Error) {
    note(&format!("{error:#}"));
}

fn choose(prompt: &str, items: &[String]) -> Result<Option<usize>> {
    Ok(Select::with_theme(&ui_theme())
        .with_prompt(prompt)
        .report(false)
        .items(items)
        .default(0)
        .interact_opt()?)
}

fn action<T>(prompt: &str, rows: Vec<(T, String)>) -> Result<Option<T>> {
    let labels: Vec<_> = rows.iter().map(|(_, label)| label.clone()).collect();
    Ok(choose(prompt, &labels)?
        .and_then(|index| rows.into_iter().nth(index).map(|(value, _)| value)))
}

fn confirm(prompt: &str) -> Result<bool> {
    Ok(choose(prompt, &["No / cancel".into(), "Yes".into()])? == Some(1))
}

fn input(prompt: &str, current: &str) -> Result<String> {
    Ok(Input::<String>::with_theme(&ui_theme())
        .with_prompt(prompt)
        .report(false)
        .default(current.to_owned())
        .allow_empty(true)
        .interact_text()?)
}

fn refresh(cfg: &mut cli_config::CliConfig) -> Result<()> {
    let (plugins, policy) = cli_config::read_memory_configuration()?;
    cfg.memory_plugins = Some(plugins);
    cfg.memory_auto_learn = policy;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HubAction {
    Provider,
    Learning,
    Storage,
    Advanced,
    Back,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderAction {
    None,
    Mnemos,
    Custom(String),
    Import,
    Back,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LearningAction {
    Off,
    On,
    Back,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageAction {
    Default,
    Custom,
    Back,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdvancedAction {
    Status,
    Details,
    Packages,
    Disable,
    Back,
}

fn provider_label(
    name: &str,
    selected: bool,
    enabled: bool,
    verified: bool,
    installed: bool,
) -> String {
    let status = if !installed {
        "not installed"
    } else if !verified {
        "damaged"
    } else if selected && enabled {
        "selected"
    } else {
        "disabled"
    };
    let selection = if selected && status != "selected" {
        " · selected"
    } else {
        ""
    };
    format!("{} · {status}{selection}", safe(name))
}

fn provider_rows(
    state: &MemoryPlugins,
    verified_names: &BTreeMap<String, String>,
) -> Vec<(ProviderAction, String)> {
    let mut rows = vec![(
        ProviderAction::None,
        if state.selected.is_none() {
            "None · selected".into()
        } else {
            "None · disable Memory".into()
        },
    )];
    let mnemos = provider_label(
        "Mnemos",
        state.selected.as_deref() == Some("mnemos"),
        state.enabled && state.packages.get("mnemos").is_some_and(|p| p.enabled),
        verified_names.contains_key("mnemos"),
        state.packages.contains_key("mnemos"),
    );
    // Sanitize metadata before styling; the entire Mnemos row uses the theme accent.
    rows.push((ProviderAction::Mnemos, theme::accent(mnemos).to_string()));
    for (id, package) in &state.packages {
        if id == "mnemos" {
            continue;
        }
        let name = verified_names.get(id).map(String::as_str).unwrap_or(id);
        let label = provider_label(
            name,
            state.selected.as_ref() == Some(id),
            state.enabled && package.enabled,
            verified_names.contains_key(id),
            true,
        );
        rows.push((ProviderAction::Custom(id.clone()), label));
    }
    rows.push((ProviderAction::Import, "Import local package…".into()));
    rows.push((ProviderAction::Back, "Back".into()));
    rows
}

fn saved_grant(review: &manager::PackageReview) -> Option<ExecutionGrant> {
    review
        .state
        .grant
        .as_ref()
        .filter(|grant| options::validate_grant(&review.package.manifest, grant).is_ok())
        .cloned()
}

fn has_learning(review: &manager::PackageReview) -> bool {
    review
        .package
        .manifest
        .capabilities
        .iter()
        .any(|cap| cap == "learning")
}

fn effective_learning(
    policy: bool,
    active: bool,
    capable: bool,
    valid_grant: bool,
    sharing: bool,
) -> bool {
    policy && active && capable && valid_grant && sharing
}

fn learning_enabled(
    state: &MemoryPlugins,
    policy: Option<bool>,
    review: Option<&manager::PackageReview>,
) -> bool {
    let Some(review) = review else {
        return false;
    };
    let grant = saved_grant(review);
    effective_learning(
        policy.unwrap_or(true),
        state.enabled
            && review.state.enabled
            && state.selected.as_deref() == Some(review.package.manifest.id.as_str()),
        has_learning(review),
        grant.is_some(),
        grant.as_ref().is_some_and(|g| g.learning),
    )
}

fn hub_rows(provider: &str, learning: &str, storage: &str) -> Vec<(HubAction, String)> {
    vec![
        (
            HubAction::Provider,
            format!("Provider · {}", safe(provider)),
        ),
        (
            HubAction::Learning,
            format!("Learning · {}", safe(learning)),
        ),
        (HubAction::Storage, format!("Storage · {}", safe(storage))),
        (
            HubAction::Advanced,
            "Advanced · packages, options, consent".into(),
        ),
        (HubAction::Back, "Back".into()),
    ]
}

fn execution_grant(review: &manager::PackageReview) -> Result<Option<ExecutionGrant>> {
    if let Some(grant) = saved_grant(review) {
        return Ok(Some(grant));
    }
    note(&options::executable_disclosure(&review.package)?);
    note(&format!(
        "Pinned manifest SHA256: {}",
        review.package.pins.manifest.sha256
    ));
    note("Hashes verify bytes, not publisher trust. Nothing saves until final confirmation.");
    if !confirm("Authorize future execution?")? {
        return Ok(None);
    }
    let guarded = if cfg!(windows) {
        note("Windows guarded mode does NOT confine filesystem/network. This executable runs as your OS account.");
        if !confirm("Acknowledge guarded limitations?")? {
            return Ok(None);
        }
        true
    } else {
        confirm("Allow guarded fallback if needed?")?
    };
    let network = if review.package.manifest.permissions.network {
        note("This package requires declared network access. Global sandbox policy still applies.");
        if !confirm("Authorize declared network access?")? {
            return Ok(None);
        }
        true
    } else {
        false
    };
    Ok(Some(ExecutionGrant {
        manifest_sha256: review.package.pins.manifest.sha256.clone(),
        guarded_execution: guarded,
        network,
        learning: false,
        turn_model_learning: false,
    }))
}

fn activate(id: &str) -> Result<()> {
    let review = manager::review(id)?;
    let reuse = saved_grant(&review).is_some();
    let Some(grant) = execution_grant(&review)? else {
        return Ok(());
    };
    note(&format!("Provider: {}", review.package.manifest.name));
    if !reuse {
        for line in options::consent_disclosures(&review.package, &grant)? {
            note(&line);
        }
    }
    if confirm(if reuse {
        "Activate with saved execution consent?"
    } else {
        "Activate this provider?"
    })? {
        manager::activate(&review, grant)?;
    }
    Ok(())
}

fn import(expected: Option<&str>) -> Result<()> {
    note("Local directory or ZIP only; no discovery or download. Installing does not activate a provider.");
    let path = input("Package path (blank cancels)", "")?;
    let path = path.trim().trim_matches('"');
    if path.is_empty() {
        return Ok(());
    }
    ensure!(!path.chars().any(char::is_control), "invalid package path");
    note(&format!("Source: {path}"));
    if confirm("Install without execution or selection?")? {
        let package = manager::install_local_expected(Path::new(path), expected)?;
        note(&format!(
            "Installed {}. Activation is a separate choice.",
            package.manifest.name
        ));
        if confirm("Review activation now?")? {
            activate(&package.manifest.id)?;
        }
    }
    Ok(())
}

async fn mnemos_setup() -> Result<()> {
    let mut release: Option<(releases::ReleaseStatus, u64)> = None;
    loop {
        let state = manager::read_state()?;
        let installed = state.packages.contains_key("mnemos");
        note("Mnemos · installed packages follow the shared cached update check; downloading and replacement always require confirmation.");
        match manager::info("mnemos") {
            Ok(package) => note(&format!("Installed: {}", package.manifest.version)),
            Err(_) if installed => note(
                "Installed package is damaged or unverifiable; no replacement will be attempted.",
            ),
            Err(_) => note("Installed: none"),
        }
        if release
            .as_ref()
            .is_some_and(|(_, generation)| *generation != state.generation)
        {
            release = None;
        }
        let mut rows = vec![(0, "Check public Mnemos release".into())];
        if let Some((status, _)) = &release {
            note(&format!("Source: {}", releases::REPOSITORY));
            if let Some(version) = &status.highest_seen_version {
                note(&format!(
                    "Highest published: {version} (metadata may have no installable package)"
                ));
            }
            if let Some(candidate) = &status.candidate {
                note(&format!(
                    "Downloadable: {} · {} · {}",
                    candidate.version, candidate.target, status.comparison
                ));
                note(&format!(
                    "Package: {} ({} bytes)",
                    candidate.filename, candidate.bytes
                ));
                note(&format!("SHA256: {}", candidate.sha256));
                if !installed {
                    rows.push((1, "Download and install reviewed release".into()));
                }
            } else {
                note("No installable package is available. Local ZIP/directory installation remains available.");
            }
        }
        if installed {
            rows.push((2, "Review provider activation (separate consent)".into()));
            rows.push((5, "Check and review a newer Mnemos update".into()));
            note("Updates preserve settings/data but revoke execution and Learning consent. Activation is required again.");
        } else {
            rows.push((3, "Install local directory / ZIP".into()));
        }
        rows.push((4, "Back".into()));
        match action("Mnemos", rows)? {
            Some(0) => {
                let status = releases::check(None).await?;
                ensure!(
                    manager::read_state()?.generation == state.generation,
                    "Memory configuration changed during release check; check again"
                );
                release = Some((status, state.generation));
            }
            Some(1) => {
                let (status, generation) = release.take().context("Check a release first")?;
                let candidate = status.candidate.context("No installable Mnemos package")?;
                note(
                    "Download and install only. Integrity hashes do not establish publisher trust.",
                );
                note("No provider will be executed, authorized, selected or enabled. No data will be transferred.");
                if confirm("Download and install this exact release?")? {
                    let package = releases::install(&candidate, generation).await?;
                    note(&format!(
                        "Installed Mnemos {}. Activation remains a separate choice.",
                        package.manifest.version
                    ));
                }
            }
            Some(2) => {
                activate("mnemos")?;
                release = None;
            }
            Some(3) => {
                import(Some("mnemos"))?;
                release = None;
            }
            Some(5) => {
                crate::cli::plugin_cmd::update_mnemos(None, None, false).await?;
                release = None;
            }
            Some(4) | None => return Ok(()),
            _ => unreachable!("fixed Mnemos setup actions"),
        }
    }
}

async fn provider(state: &MemoryPlugins) -> Result<()> {
    let verified_names = state
        .packages
        .keys()
        .filter_map(|id| {
            manager::review(id)
                .ok()
                .map(|review| (id.clone(), review.package.manifest.name))
        })
        .collect();
    match action("Provider", provider_rows(state, &verified_names))? {
        Some(ProviderAction::None) => {
            if state.selected.is_some()
                && confirm("Choose None and disable Memory (preserve grants/data)?")?
            {
                manager::clear_selection(state.generation)?;
            }
        }
        Some(ProviderAction::Mnemos) => mnemos_setup().await?,
        Some(ProviderAction::Custom(id)) => activate(&id)?,
        Some(ProviderAction::Import) => import(None)?,
        Some(ProviderAction::Back) | None => {}
    }
    Ok(())
}

fn learning(state: &MemoryPlugins, policy: Option<bool>) -> Result<()> {
    match action(
        "Learning",
        vec![
            (LearningAction::Off, "Off · recall stays available".into()),
            (LearningAction::On, "On · completed-turn learning".into()),
            (LearningAction::Back, "Back".into()),
        ],
    )? {
        Some(LearningAction::Off) => {
            if confirm("Turn Learning Off (preserve recall and saved consent)?")? {
                manager::set_learning_policy(false)?;
            }
        }
        Some(LearningAction::On) => {
            let Some(id) = state.selected.as_deref().filter(|_| state.enabled) else {
                note("Choose and activate a provider first. Learning remains Off.");
                return Ok(());
            };
            let review = manager::review(id)?;
            ensure!(
                review.generation == state.generation && review.state.enabled,
                "Memory changed; reopen Learning. Nothing saved"
            );
            ensure!(
                has_learning(&review),
                "This provider does not support Learning"
            );
            let mut grant = saved_grant(&review)
                .context("Activate this provider with valid execution consent first")?;
            if learning_enabled(state, policy, Some(&review)) {
                return Ok(());
            }
            if !grant.learning {
                note("Learning shares bounded completed-turn user/assistant/tool text with this executable. Recall does not require sharing.");
                if !confirm("Authorize bounded completed-turn sharing?")? {
                    return Ok(());
                }
                grant.learning = true;
                grant.turn_model_learning = false;
                if review.package.manifest.permissions.host_llm
                    && review
                        .package
                        .manifest
                        .capabilities
                        .iter()
                        .any(|cap| cap == "host_llm")
                {
                    note("Optional: one extra bounded no-tool call on the completed turn's exact model route, including future route choices. Provider-prepared text may reach remote routes and incur token cost. Core keeps credentials.");
                    grant.turn_model_learning =
                        confirm("Authorize same-turn host-model learning?")?;
                }
            }
            if confirm("Turn Learning On with this consent?")? {
                manager::set_learning(&review, true, grant)?;
            }
        }
        Some(LearningAction::Back) | None => {}
    }
    Ok(())
}

fn storage(state: &MemoryPlugins) -> Result<()> {
    let Some(id) = state.selected.as_deref() else {
        note("Choose a provider first to configure its storage.");
        return Ok(());
    };
    let review = manager::review(id)?;
    ensure!(
        review.generation == state.generation,
        "Memory changed; reopen Storage. Nothing saved"
    );
    note(&format!(
        "Effective path: {}",
        manager::data_root_for(id, &review.state)?.display()
    ));
    let root = match action(
        "Storage",
        vec![
            (
                StorageAction::Default,
                "Default · Aizen-managed directory".into(),
            ),
            (StorageAction::Custom, "Custom · choose directory".into()),
            (StorageAction::Back, "Back".into()),
        ],
    )? {
        Some(StorageAction::Default) => None,
        Some(StorageAction::Custom) => {
            note("Enter a directory path; it is displayed separately from the prompt.");
            let path = input("Directory (blank cancels)", "")?;
            let path = path.trim().trim_matches('"');
            if path.is_empty() {
                return Ok(());
            }
            ensure!(!path.chars().any(char::is_control), "invalid storage path");
            Some(PathBuf::from(path))
        }
        Some(StorageAction::Back) | None => return Ok(()),
    };
    let mut proposed = review.state.clone();
    proposed.data_root = root.clone();
    note(&format!(
        "New path: {}",
        manager::data_root_for(id, &proposed)?.display()
    ));
    note("No migration, copy or deletion occurs. Existing records stay at their old location; the new location may appear empty.");
    if confirm("Use this storage directory without moving data?")? {
        let workspace = std::env::current_dir().ok();
        manager::set_storage(&review, root, workspace.as_deref())?;
    }
    Ok(())
}

fn pick_provider(eligible_only: bool) -> Result<Option<String>> {
    let state = manager::read_state()?;
    let ids: Vec<_> = state
        .packages
        .iter()
        .filter(|(_, p)| !eligible_only || p.enabled)
        .map(|(id, _)| id.clone())
        .collect();
    if ids.is_empty() {
        note("No installed Memory packages. Choose Provider to import one.");
        return Ok(None);
    }
    let mut rows = vec!["Back".into()];
    rows.extend(ids.iter().map(|id| {
        let p = &state.packages[id];
        safe(&format!(
            "{id} · eligible={} · selected={} · saved execution consent={}",
            p.enabled,
            state.selected.as_ref() == Some(id),
            p.grant.is_some()
        ))
    }));
    Ok(choose("Installed provider", &rows)?
        .filter(|n| *n > 0)
        .map(|n| ids[n - 1].clone()))
}

fn show_status() -> Result<()> {
    let status = manager::status()?;
    note(&format!(
        "Memory: {} · selected: {} · enabled: {} · installed: {} · generation: {}",
        status.state,
        status.selected.as_deref().unwrap_or("none"),
        status.enabled,
        status.installed,
        status.generation
    ));
    note("Skills, Persona, Soul, sessions and Core codebase retrieval work without a Memory provider.");
    if status.recovery_required {
        note("Recovery required: physical settlement is unresolved. Activation/admin changes remain blocked; no automatic cleanup or replay.");
    }
    if let Some(outbox) = status.learning_outbox {
        note(&format!("Learning receipts: {}/{} · prepared={} · dispatching={} · done={} · failed={} · available={}", outbox.receipts, outbox.capacity, outbox.prepared, outbox.dispatching, outbox.done, outbox.failed, outbox.available));
    } else {
        note("Learning receipt inventory is invalid/unavailable, not empty. No contents were changed.");
    }
    if let Some(id) = status.selected {
        note(&format!(
            "Package: {}",
            manager::packages_root().join(&id).display()
        ));
        note(&format!(
            "Data (preserved on uninstall): {}",
            manager::data_root(&id)?.display()
        ));
    }
    Ok(())
}

fn details(id: &str) -> Result<()> {
    let review = manager::review(id)?;
    let package = &review.package;
    note(&format!(
        "{} · {} · {}",
        id, package.manifest.name, package.manifest.version
    ));
    note(&format!(
        "Verified pinned manifest SHA256: {}",
        package.pins.manifest.sha256
    ));
    note(&options::executable_disclosure(package)?);
    note("Integrity verification is not publisher trust. No executable was launched.");
    note(&format!("Package: {}", package.directory.display()));
    note(&format!(
        "Data: {}",
        manager::data_root_for(id, &review.state)?.display()
    ));
    note(&format!(
        "Capabilities: {}",
        package.manifest.capabilities.join(", ")
    ));
    note(&format!(
        "Declared network={} · host-model assistance={}",
        package.manifest.permissions.network, package.manifest.permissions.host_llm
    ));
    note(&format!(
        "Eligible={} · saved execution consent={} · learning={} · same-turn model={}",
        review.state.enabled,
        review.state.grant.is_some(),
        review.state.grant.as_ref().is_some_and(|g| g.learning),
        review
            .state
            .grant
            .as_ref()
            .is_some_and(|g| g.turn_model_learning)
    ));
    for (key, value) in
        options::effective_settings(&package.manifest.settings, &review.state.settings)?
    {
        note(&format!("{key} = {value}"));
    }
    let b = review.state.budgets.unwrap_or_default();
    note(&format!("Standing response/render={} tokens · recall response={} · actual recall injection={} · automatic results={} · response records={}", b.context_tokens, b.recall_tokens, b.recall_injection(), b.recall_count(), b.max_records));
    Ok(())
}

fn consent(id: &str, select: bool) -> Result<()> {
    let review = manager::review(id)?;
    note(&options::executable_disclosure(&review.package)?);
    note(&format!(
        "Pinned manifest SHA256: {}",
        review.package.pins.manifest.sha256
    ));
    note("This executable can receive Memory inputs. Hashes do not establish publisher trust. All consent defaults to No; nothing saves until final confirmation.");
    if !confirm("Authorize future execution?")? {
        return Ok(());
    }
    let guarded = if cfg!(windows) {
        note("Windows guarded mode does NOT confine filesystem/network. The executable runs as your OS account.");
        if !confirm("Acknowledge guarded limitations?")? {
            return Ok(());
        }
        true
    } else {
        confirm("Allow guarded fallback if needed?")?
    };
    let network = if review.package.manifest.permissions.network {
        note("This package declares network access. Denying it cannot leave valid execution consent. Global sandbox policy still applies.");
        if !confirm("Authorize declared network access?")? {
            if confirm("Revoke saved execution consent instead?")? {
                manager::revoke(id, review.generation)?;
            }
            return Ok(());
        }
        true
    } else {
        false
    };
    let learning = if has_learning(&review) {
        note("Completed-turn learning shares bounded user/assistant/tool text with this executable. Automatic-learning policy remains separate.");
        confirm("Authorize completed-turn sharing?")?
    } else {
        false
    };
    let turn_model_learning = if learning
        && review.package.manifest.permissions.host_llm
        && review
            .package
            .manifest
            .capabilities
            .iter()
            .any(|cap| cap == "host_llm")
    {
        note("Allows one extra bounded no-tool call on the completed turn's exact chat route, including future route choices. Provider-prepared Memory/conversation text may reach remote routes and incur token cost. Core keeps credentials; no separate summarizer/fallback.");
        confirm("Authorize same-turn model learning?")?
    } else {
        false
    };
    let grant = ExecutionGrant {
        manifest_sha256: review.package.pins.manifest.sha256.clone(),
        guarded_execution: guarded,
        network,
        learning,
        turn_model_learning,
    };
    for line in options::consent_disclosures(&review.package, &grant)? {
        note(&line);
    }
    if confirm(if select {
        "Save consent, select and enable Memory?"
    } else {
        "Save consent without enabling Memory?"
    })? {
        manager::apply_grant(&review, grant, select)?;
        note("Saved. No provider executed; active changes apply at the next user-turn boundary.");
    }
    Ok(())
}

fn edit_value(definition: &options::SettingDefinition, current: &Value) -> Result<Option<Value>> {
    note(&definition.description);
    let value = match &definition.kind {
        options::SettingKind::Boolean { .. } => match choose(
            "Value (Esc cancels)",
            &["Cancel".into(), "false".into(), "true".into()],
        )? {
            Some(1) => Value::Bool(false),
            Some(2) => Value::Bool(true),
            _ => return Ok(None),
        },
        options::SettingKind::Enum { choices, .. } => {
            let mut rows = vec!["Cancel".into()];
            rows.extend(choices.iter().map(|v| safe(v)));
            match choose("Value (Esc cancels)", &rows)? {
                Some(n) if n > 0 => Value::String(choices[n - 1].clone()),
                _ => return Ok(None),
            }
        }
        options::SettingKind::String { max_length, .. } => {
            note(&format!("Nonsecret text only, at most {max_length} UTF-8 bytes. Never enter keys or credentials."));
            Value::String(input("Text value", current.as_str().unwrap_or(""))?)
        }
        options::SettingKind::Integer { min, max, .. } => {
            note(&format!("Integer range: {min}..={max}"));
            crate::plugin::memory_api::strict_json(
                input("Integer value", &current.to_string())?
                    .trim()
                    .as_bytes(),
            )?
        }
        options::SettingKind::Number { min, max, .. } => {
            note(&format!("Finite number range: {min}..={max}"));
            crate::plugin::memory_api::strict_json(
                input("Number value", &current.to_string())?
                    .trim()
                    .as_bytes(),
            )?
        }
    };
    definition.validate_value(&value)?;
    Ok(Some(value))
}

fn configure(id: &str) -> Result<()> {
    let review = manager::review(id)?;
    let schema = &review.package.manifest.settings;
    let mut values = review.state.settings.clone();
    let mut budgets = review.state.budgets.clone();
    note("Draft only until Save. Back/Esc discards the entire draft. Settings are nonsecret tuning values, never execution, model, network or file-access grants.");
    loop {
        let effective = options::effective_settings(schema, &values)?;
        let keys: Vec<_> = schema.keys().cloned().collect();
        let mut rows = vec![
            "Back / discard draft".into(),
            "Save draft".into(),
            "Host response / injection budgets".into(),
            "Reset options and budgets to defaults".into(),
        ];
        rows.extend(keys.iter().map(|key| {
            format!(
                "{} · {} = {}{}",
                safe(&schema[key].label),
                safe(key),
                safe(&effective[key].to_string()),
                if values.contains_key(key) {
                    " (override)"
                } else {
                    " (default)"
                }
            )
        }));
        if keys.is_empty() {
            note("This package declares no provider-specific options; host budgets are still configurable.");
        }
        match choose("Provider configuration", &rows)? {
            None | Some(0) => return Ok(()),
            Some(1) => {
                if confirm("Save provider draft?")? {
                    manager::configure(&review, values, budgets)?;
                    note("Saved for the next user-turn boundary. No data reset or provider execution.");
                    return Ok(());
                }
            }
            Some(2) => {
                let mut b = budgets.clone().unwrap_or_default();
                note("Response allowance is NOT chat injection. Zero skips the corresponding automatic operation. Hard caps: 4096 tokens / 100 records.");
                let fields = [
                    "Standing response/render tokens",
                    "Recall response tokens",
                    "Recall chat injection tokens",
                    "Automatic recall result limit",
                    "Maximum response records",
                ];
                let current = [
                    b.context_tokens,
                    b.recall_tokens,
                    b.recall_inject_tokens,
                    b.recall_limit,
                    b.max_records,
                ];
                let mut rows = vec!["Back".into(), "Reset host budgets to defaults".into()];
                rows.extend(
                    fields
                        .iter()
                        .zip(current)
                        .map(|(name, value)| format!("{name}: {value}")),
                );
                match choose("Host budgets", &rows)? {
                    Some(1) => budgets = None,
                    Some(n) if n >= 2 => {
                        let field = n - 2;
                        let value: usize = input("New limit", &current[field].to_string())?
                            .trim()
                            .parse()
                            .context("expected a nonnegative integer")?;
                        match field {
                            0 => b.context_tokens = value,
                            1 => b.recall_tokens = value,
                            2 => b.recall_inject_tokens = value,
                            3 => b.recall_limit = value,
                            _ => b.max_records = value,
                        }
                        b.validate()?;
                        budgets = Some(b);
                    }
                    _ => {}
                }
            }
            Some(3) => {
                if confirm("Reset draft only (not Memory records)?")? {
                    values = BTreeMap::new();
                    budgets = None;
                }
            }
            Some(n) => {
                let key = &keys[n - 4];
                match choose(
                    "Option action",
                    &[
                        "Back".into(),
                        "Edit value".into(),
                        "Use provider default".into(),
                    ],
                )? {
                    Some(1) => {
                        if let Some(value) = edit_value(&schema[key], &effective[key])? {
                            values.insert(key.clone(), value);
                        }
                    }
                    Some(2) => {
                        values.remove(key);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn packages() -> Result<()> {
    let Some(id) = pick_provider(false)? else {
        return Ok(());
    };
    match choose(
        "Package action",
        &[
            "Back".into(),
            "Inspect verified details".into(),
            "Make eligible (not selected)".into(),
            "Make ineligible / disable".into(),
            "Edit options and budgets".into(),
            "Edit saved execution consent".into(),
            "Revoke execution consent".into(),
            "Uninstall executable; preserve data".into(),
        ],
    )? {
        Some(1) => details(&id)?,
        Some(2) => {
            if confirm("Make package eligible (not select)?")? {
                manager::set_eligible(&id, true)?;
            }
        }
        Some(3) => {
            if confirm("Make package ineligible and disable it?")? {
                manager::set_eligible(&id, false)?;
            }
        }
        Some(4) => configure(&id)?,
        Some(5) => consent(&id, false)?,
        Some(6) => {
            let state = manager::read_state()?;
            note("Revocation preserves selection, package, options and data; selected Memory becomes disabled.");
            if confirm("Revoke saved execution consent?")? {
                manager::revoke(&id, state.generation)?;
            }
        }
        Some(7) => {
            note(&format!(
                "Remove package: {}",
                manager::uninstall_directory(&id)?.display()
            ));
            note(&format!(
                "Preserve data: {}",
                manager::data_root(&id)?.display()
            ));
            note("No provider command runs. Damaged payloads are disabled, with unknown files and pins retained for recovery.");
            if confirm("Uninstall verified executable files?")? {
                manager::uninstall(&id)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn advanced() -> Result<()> {
    match action(
        "Advanced",
        vec![
            (AdvancedAction::Status, "Status and diagnostics".into()),
            (AdvancedAction::Details, "Selected provider details".into()),
            (
                AdvancedAction::Packages,
                "Installed packages / options / consent".into(),
            ),
            (
                AdvancedAction::Disable,
                "Disable Memory · preserve selection, grants and data".into(),
            ),
            (AdvancedAction::Back, "Back".into()),
        ],
    )? {
        Some(AdvancedAction::Status) => show_status()?,
        Some(AdvancedAction::Details) => {
            if let Some(id) = manager::read_state()?.selected {
                details(&id)?;
            } else {
                note("Choose a provider first to inspect its details.");
            }
        }
        Some(AdvancedAction::Packages) => packages()?,
        Some(AdvancedAction::Disable) => {
            if confirm("Disable Memory and preserve selection, grants and data?")? {
                manager::set_memory_enabled(false)?;
            }
        }
        Some(AdvancedAction::Back) | None => {}
    }
    Ok(())
}

pub(super) fn codebase(cfg: &mut cli_config::CliConfig) -> Result<()> {
    note("Core codebase embedding is independent of Memory providers. This does not configure a Memory model or download weights.");
    let paths = crate::retrieval::embed::DiscoveryPaths {
        preferred_name: cfg
            .embed_model
            .clone()
            .unwrap_or_else(crate::core::config::embed_model_name),
        models_dir: crate::core::config::models_dir(),
        hf_roots: crate::retrieval::embed::host_hf_roots(),
    };
    let models = crate::retrieval::embed::list_local_models(&paths);
    if !cfg!(feature = "dense") || models.is_empty() {
        note("Requires a dense build and explicitly provisioned local weights. Use `aizen codebase model-download` separately.");
        return Ok(());
    }
    let mut rows = vec!["Back".into(), "Auto — Core model discovery".into()];
    rows.extend(
        models
            .iter()
            .map(|m| safe(&format!("{} ({})", m.name, m.source))),
    );
    if let Some(n) = choose("Core codebase embedding", &rows)?.filter(|n| *n > 0) {
        if confirm("Save Core embedding choice?")? {
            let mut next = cfg.clone();
            next.embed_model = if n == 1 {
                None
            } else {
                Some(models[n - 2].name.clone())
            };
            cli_config::save(&next)?;
            *cfg = next;
        }
    }
    Ok(())
}

pub(super) fn summary() -> Result<(String, bool)> {
    let (state, policy) = cli_config::read_memory_configuration()?;
    let recovery =
        manager::transaction_marker().try_exists()? || crate::plugin::update::recovery_required()?;
    let review = state
        .selected
        .as_deref()
        .and_then(|id| manager::review(id).ok());
    let provider = match state.selected.as_deref() {
        None => "None".into(),
        Some(id) => provider_label(
            review
                .as_ref()
                .map(|r| r.package.manifest.name.as_str())
                .unwrap_or(id),
            true,
            state.enabled,
            review.is_some(),
            true,
        ),
    };
    Ok((
        if recovery {
            format!("{provider} · recovery required")
        } else {
            provider
        },
        !recovery && learning_enabled(&state, policy, review.as_ref()),
    ))
}

pub(super) async fn edit(cfg: &mut cli_config::CliConfig) -> Result<()> {
    loop {
        refresh(cfg)?; // Fail closed; do not replace damaged host state with an empty configuration.
        let state = cfg
            .memory_plugins
            .as_ref()
            .context("Memory configuration unavailable")?;
        let review = state.selected.as_deref().map(manager::review).transpose();
        if let Err(error) = &review {
            note(&format!(
                "Provider unavailable: {error:#}. Use Advanced to inspect or repair it."
            ));
        }
        let review = review.ok().flatten();
        let recovery = manager::transaction_marker().try_exists()?
            || crate::plugin::update::recovery_required()?;
        if recovery {
            note("Recovery required: unresolved provider work or package update. Inspect Advanced diagnostics / `aizen plugin recover-update mnemos`; no automatic cleanup or replay.");
        }
        let provider_hint = match state.selected.as_deref() {
            None => "None".into(),
            Some(id) => provider_label(
                review
                    .as_ref()
                    .map(|r| r.package.manifest.name.as_str())
                    .unwrap_or(id),
                true,
                state.enabled,
                review.is_some(),
                true,
            ),
        };
        let storage_hint = if let Some(review) = &review {
            format!(
                "{} · {}",
                if review.state.data_root.is_some() {
                    "Custom"
                } else {
                    "Default"
                },
                manager::data_root_for(&review.package.manifest.id, &review.state)?.display()
            )
        } else if state.selected.is_some() {
            "unavailable · inspect provider".into()
        } else {
            "choose provider first".into()
        };
        let learning_hint = if recovery {
            "unavailable · recovery required"
        } else if state.selected.is_none() {
            "Off · choose provider first"
        } else if review.is_none() {
            "unavailable · inspect provider"
        } else if !state.enabled {
            "Off · provider disabled"
        } else if !review.as_ref().is_some_and(has_learning) {
            "unavailable · unsupported"
        } else if learning_enabled(state, cfg.memory_auto_learn, review.as_ref()) {
            "On"
        } else {
            "Off"
        };
        let rows = hub_rows(&provider_hint, learning_hint, &storage_hint);
        let result = match action("Memory", rows)? {
            Some(HubAction::Provider) => provider(state).await,
            Some(HubAction::Learning) => learning(state, cfg.memory_auto_learn),
            Some(HubAction::Storage) => storage(state),
            Some(HubAction::Advanced) => advanced(),
            Some(HubAction::Back) | None => return Ok(()),
        };
        if let Err(error) = result {
            note(&format!("Memory action failed: {error:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_terminal_text_is_bounded_and_cannot_inject_controls() {
        assert_eq!(safe("a\u{1b}[2J\n\u{202e}b"), "a[2Jb");
        assert_eq!(safe(&"x".repeat(2000)).chars().count(), 600);
    }

    #[test]
    fn everyday_hub_has_exactly_five_named_actions() {
        let rows = hub_rows("None", "Off", "choose provider first");
        assert_eq!(
            rows.iter().map(|(action, _)| *action).collect::<Vec<_>>(),
            vec![
                HubAction::Provider,
                HubAction::Learning,
                HubAction::Storage,
                HubAction::Advanced,
                HubAction::Back
            ]
        );
        assert_eq!(rows[1].1, "Learning · Off");
    }

    #[test]
    fn no_provider_choices_keep_mnemos_explicit_without_fake_integrations() {
        let rows = provider_rows(&MemoryPlugins::default(), &BTreeMap::new());
        assert_eq!(
            rows.iter()
                .map(|(action, _)| action.clone())
                .collect::<Vec<_>>(),
            vec![
                ProviderAction::None,
                ProviderAction::Mnemos,
                ProviderAction::Import,
                ProviderAction::Back
            ]
        );
        assert_eq!(rows[0].1, "None · selected");
        assert!(rows[1].1.contains("Mnemos · not installed"));
        assert!(!rows.iter().any(|(_, label)| label.contains("Obsidian")));
    }

    #[test]
    fn provider_labels_distinguish_selection_disabled_damage_and_sanitize_names() {
        assert_eq!(
            provider_label("Mnemos", true, true, true, true),
            "Mnemos · selected"
        );
        assert_eq!(
            provider_label("Mnemos", true, false, true, true),
            "Mnemos · disabled · selected"
        );
        assert_eq!(
            provider_label("Mnemos", true, true, false, true),
            "Mnemos · damaged · selected"
        );
        assert_eq!(
            provider_label("bad\n\u{1b}\u{202e}name", false, false, true, true),
            "badname · disabled"
        );
    }

    #[test]
    fn verified_custom_name_is_sanitized_and_damaged_metadata_is_not_used() {
        let mut state = MemoryPlugins::default();
        let package = serde_json::from_value(serde_json::json!({
            "pins": {
                "schema_version": 1, "id": "custom", "target": "fixture",
                "manifest": { "path": "plugin.json", "bytes": 1, "sha256": "a".repeat(64) },
                "files": []
            },
            "enabled": true, "grant": null
        }))
        .unwrap();
        state.packages.insert("custom".into(), package);
        state.selected = Some("custom".into());
        state.enabled = true;
        let verified = BTreeMap::from([("custom".into(), "Friendly\n\u{1b}\u{202e} name".into())]);
        let rows = provider_rows(&state, &verified);
        assert_eq!(
            rows[2],
            (
                ProviderAction::Custom("custom".into()),
                "Friendly name · selected".into()
            )
        );
        let damaged = provider_rows(&state, &BTreeMap::new());
        assert_eq!(damaged[2].1, "custom · damaged · selected");
    }

    #[test]
    fn learning_on_requires_every_effective_gate_not_just_latent_policy() {
        assert!(effective_learning(true, true, true, true, true));
        for off in 0..5 {
            let mut gates = [true; 5];
            gates[off] = false;
            assert!(!effective_learning(
                gates[0], gates[1], gates[2], gates[3], gates[4]
            ));
        }
        assert!(!learning_enabled(
            &MemoryPlugins::default(),
            Some(true),
            None
        ));
    }

    #[test]
    fn no_provider_refresh_is_read_only_and_preserves_unrelated_draft() {
        let _lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("absent");
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let mut cfg = cli_config::CliConfig {
            model: Some("draft".into()),
            memory_auto_learn: Some(false),
            ..Default::default()
        };
        refresh(&mut cfg).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("draft"));
        assert_eq!(cfg.memory_auto_learn, None);
        assert!(cfg.memory_plugins.unwrap().packages.is_empty());
        assert!(!home.exists());
    }
}
