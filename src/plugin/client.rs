//! Versioned Desktop boundary: offline snapshots, explicit release actions and reviewed settings.
use super::{
    configuration,
    install::InstallPins,
    manager, memory_api as api,
    settings::{ExecutionGrant, MemoryPlugins},
};
use anyhow::{ensure, Context, Result};
use api::Validate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};
#[path = "client_rich_protocol.rs"]
pub(crate) mod rich;
#[path = "client_rich_validation.rs"]
mod rich_validation;

pub(crate) const MAX_REQUEST: usize = 256 * 1024;
pub(crate) const MAX_RESPONSE: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub version: u32,
    pub request_id: String,
    pub workspace: String,
    pub action: Action,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Adoption {
        binding: Binding,
        request: super::adoption::Request,
    },
    Snapshot {},
    Read {
        binding: Binding,
        query: Query,
    },
    Inspect {
        binding: Binding,
        query: rich::Inspect,
    },
    Manage {
        binding: Binding,
        operation_id: String,
        command: rich::Manage,
    },
    Review {
        binding: Binding,
        change: Change,
    },
    Apply {
        review: Review,
    },
    InstallLocal {
        binding: Binding,
        source: String,
        expected_id: String,
    },
    ReleaseCheck {
        binding: Binding,
    },
    ReleaseInstall {
        binding: Binding,
        candidate: super::releases::Candidate,
        confirmed: bool,
    },
    UpdateStatus {
        binding: Binding,
        mode: super::update_check::Mode,
    },
    ReleaseUpdate {
        binding: Binding,
        candidate: super::releases::Candidate,
        expected_pins: InstallPins,
        confirmed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    workspace: String,
    workspace_root: String,
    workspace_id: String,
    generation: u64,
    provider: Option<String>,
    manifest_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Query {
    List {
        scope: api::Scope,
        limit: usize,
    },
    Search {
        scope: api::Scope,
        limit: usize,
        query: String,
    },
    Get {
        scope: api::Scope,
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Change {
    Activate {
        provider: String,
        grant: ExecutionGrant,
    },
    ClearSelection {},
    Disable {},
    Learning {
        enabled: bool,
        #[serde(default)]
        grant: Option<ExecutionGrant>,
    },
    Storage {
        provider: String,
        root: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Review {
    binding: Binding,
    change: Change,
    packages: BTreeMap<String, InstallPins>,
    disclosures: Vec<String>,
    /// Integrity of the exact review roundtrip, not a credential or a publisher signature.
    review_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClientError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effects: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Response {
    version: u32,
    request_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ClientError>,
}

#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

fn fail(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    Failure {
        code,
        message: message.into(),
    }
    .into()
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Request> {
    ensure!(
        bytes.len() <= MAX_REQUEST,
        "Memory client request exceeds 256 KiB"
    );
    api::strict_json(bytes)?;
    let request: Request = serde_json::from_slice(bytes)?;
    if request.version != 1 {
        return Err(fail(
            "incompatible_protocol",
            "Memory client requires version 1",
        ));
    }
    api::opaque_id(&request.request_id)?;
    api::text(&request.workspace, 32768, false)?;
    ensure!(
        Path::new(&request.workspace).is_absolute(),
        "workspace must be absolute"
    );
    match &request.action {
        Action::Read { query, .. } => validate_query(query)?,
        Action::Inspect { query, .. } => {
            query.validate()?;
            rich_validation::scoped(query.filter())?;
        }
        Action::Manage {
            operation_id,
            command,
            ..
        } => {
            api::opaque_id(operation_id)?;
            command.validate()?;
            rich_validation::scoped(Some(&command.filter))?;
        }
        Action::Review { change, .. } => validate_change(change)?,
        Action::Apply { review } => validate_change(&review.change)?,
        Action::InstallLocal {
            source,
            expected_id,
            ..
        } => validate_install(source, expected_id)?,
        Action::ReleaseCheck { .. } | Action::UpdateStatus { .. } => {}
        Action::ReleaseUpdate {
            candidate,
            expected_pins,
            confirmed,
            ..
        } => {
            ensure!(*confirmed, "release update requires explicit confirmation");
            candidate.validate()?;
            ensure!(
                expected_pins.id == "mnemos"
                    && expected_pins.schema_version == 1
                    && expected_pins.target == candidate.target
                    && expected_pins.files.len() <= 512,
                "invalid reviewed installed Mnemos identity"
            );
            super::manifest::validate_digest(&expected_pins.manifest.sha256)?;
        }
        Action::ReleaseInstall {
            candidate,
            confirmed,
            ..
        } => {
            ensure!(
                *confirmed,
                "release installation requires explicit confirmation"
            );
            candidate.validate()?;
        }
        Action::Adoption { request, .. } => request.validate()?,
        Action::Snapshot {} => {}
    }
    Ok(request)
}

fn validate_query(query: &Query) -> Result<()> {
    match query {
        Query::List { limit, .. } | Query::Search { limit, .. } => {
            if !(1..=100).contains(limit) {
                return Err(fail("invalid_request", "limit must be between 1 and 100"));
            }
        }
        Query::Get { id, .. } => api::opaque_id(id)?,
    }
    if let Query::Search { query, .. } = query {
        api::text(query, api::MAX_TEXT, true)?;
        if query.trim().is_empty() {
            return Err(fail("invalid_request", "search query must not be empty"));
        }
    }
    Ok(())
}

fn validate_change(change: &Change) -> Result<()> {
    match change {
        Change::Activate { provider, .. } | Change::Storage { provider, .. } => {
            super::manifest::validate_id(provider)?;
        }
        Change::Learning {
            enabled: true,
            grant: None,
        } => {
            return Err(fail(
                "invalid_request",
                "Learning On requires explicit sharing consent",
            ));
        }
        Change::Learning {
            enabled: false,
            grant: Some(_),
        } => {
            return Err(fail(
                "invalid_request",
                "Learning Off changes policy only; omit grant",
            ));
        }
        _ => {}
    }
    if let Change::Storage {
        root: Some(root), ..
    } = change
    {
        api::text(root, 32768, false)?;
        ensure!(
            Path::new(root).is_absolute(),
            "storage root must be absolute"
        );
    }
    Ok(())
}

#[derive(Debug)]
struct NotDispatched(anyhow::Error);
impl std::fmt::Display for NotDispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for NotDispatched {}

pub(crate) fn failure(request_id: String, error: anyhow::Error, default_code: &str) -> Response {
    let (error, effects) = match error.downcast::<NotDispatched>() {
        Ok(error) => (error.0, Some("not_dispatched".to_owned())),
        Err(error) => (error, None),
    };
    let mut message = format!("{error:#}");
    // Serde diagnostics can quote invalid values from the whole CLI configuration, including
    // unrelated account/model fields. The client is not a configuration-export interface.
    if message.contains("CLI config") {
        message =
            "Memory configuration is invalid or unreadable; repair it locally before retrying"
                .into();
    }
    let code = if let Some(error) = error.downcast_ref::<Failure>() {
        error.code.to_owned()
    } else if let Some(error) = error.downcast_ref::<api::MemoryError>() {
        serde_json::to_value(error.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| default_code.into())
    } else if message.to_lowercase().contains("recovery required") {
        "recovery_required".into()
    } else if message.to_lowercase().contains("busy") {
        "busy".into()
    } else if message.contains("changed") || message.contains("after review") {
        "stale".into()
    } else {
        default_code.into()
    };
    Response {
        version: 1,
        request_id,
        ok: false,
        data: None,
        error: Some(ClientError {
            code,
            message: message.chars().take(4096).collect(),
            effects,
        }),
    }
}

pub(crate) async fn execute(request: Request) -> Response {
    let result = dispatch(&request).await;
    match result {
        Ok(data) => Response {
            version: 1,
            request_id: request.request_id,
            ok: true,
            data: Some(data),
            error: None,
        },
        Err(error) => failure(request.request_id, error, "unavailable"),
    }
}

fn binding(workspace: &str, state: &MemoryPlugins) -> Result<Binding> {
    let cwd = std::fs::canonicalize(workspace).context("workspace is unavailable")?;
    ensure!(cwd.is_dir(), "workspace must be an existing directory");
    let identity = crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&cwd)?;
    Ok(Binding {
        workspace: cwd.to_str().context("workspace must be UTF-8")?.into(),
        workspace_root: identity
            .canonical_root
            .to_str()
            .context("workspace root must be UTF-8")?
            .into(),
        workspace_id: identity.worktree_id,
        generation: state.generation,
        provider: state.selected.clone(),
        manifest_sha256: state
            .selected
            .as_ref()
            .and_then(|id| state.packages.get(id))
            .map(|p| p.pins.manifest.sha256.clone()),
    })
}

fn check_binding(expected: &Binding, actual: &Binding) -> Result<()> {
    if expected != actual {
        return Err(fail(
            "stale",
            "Memory provider or workspace changed; refresh and review again",
        ));
    }
    Ok(())
}

fn coherent(state: &MemoryPlugins, policy: Option<bool>, recovery: bool) -> Result<()> {
    let next = crate::core::cli_config::read_memory_configuration()?;
    if next != (state.clone(), policy)
        || (manager::transaction_marker().try_exists()? || super::update::recovery_required()?)
            != recovery
    {
        return Err(fail(
            "stale",
            "Memory configuration changed during inspection; refresh",
        ));
    }
    Ok(())
}

/// No provider context is constructed here: device::current() can create identity on disk.
fn snapshot(workspace: &str) -> Result<Value> {
    let (state, policy) = crate::core::cli_config::read_memory_configuration()?;
    let current = binding(workspace, &state)?;
    let status = manager::status()?;
    if status.generation != state.generation {
        return Err(fail(
            "stale",
            "Memory configuration changed during inspection",
        ));
    }
    let mut packages = Vec::new();
    let mut learning_reasons = Vec::new();
    if !policy.unwrap_or(true) {
        learning_reasons.push("Learning policy is off".to_owned());
    }
    if !state.enabled {
        learning_reasons.push("No enabled provider is selected".to_owned());
    }
    if status.recovery_required {
        learning_reasons.push("Provider recovery is required".to_owned());
    }
    let mut selected_valid = false;
    for (id, saved) in &state.packages {
        let reviewed = manager::review(id);
        let storage = manager::data_root_for(id, saved);
        let mut row = json!({
            "id":id,"name":id,"version":"","enabled":saved.enabled,"valid":false,"error":null,
            "pins":saved.pins,"manifest_sha256":saved.pins.manifest.sha256,
            "capabilities":[],"operations":[],"permissions":null,"grant":saved.grant,
            "settings_schema":{},"effective_settings":{},"budgets":saved.budgets.clone().unwrap_or_default(),
            "storage":{"root":storage.as_ref().ok().map(|p|p.to_string_lossy().into_owned()),"custom":saved.data_root.is_some(),"error":storage.err().map(|e|format!("{e:#}"))},
            "disclosures":[]
        });
        match reviewed {
            Ok(review) => {
                if review.generation != state.generation || review.state != *saved {
                    return Err(fail("stale", "Memory package changed during inspection"));
                }
                let manifest = &review.package.manifest;
                let effective =
                    configuration::effective_settings(&manifest.settings, &saved.settings);
                let valid = effective.is_ok();
                row["valid"] = json!(valid);
                row["error"] = json!(effective.as_ref().err().map(|e| format!("{e:#}")));
                row["name"] = json!(manifest.name);
                row["version"] = json!(manifest.version);
                row["capabilities"] = json!(manifest.capabilities);
                row["permissions"] = json!(manifest.permissions);
                row["settings_schema"] = json!(manifest.settings);
                row["effective_settings"] = json!(effective.unwrap_or_default());
                if valid && manifest.capabilities.iter().any(|cap| cap == "records") {
                    row["operations"] = json!(["list", "search", "get"]);
                }
                let grant_valid = saved.grant.as_ref().is_some_and(|grant| {
                    grant.manifest_sha256 == saved.pins.manifest.sha256
                        && configuration::validate_grant(manifest, grant).is_ok()
                });
                // A disclosure preview does not grant anything. It shows required platform/network
                // execution authority with learning/model sharing off unless previously authorized.
                let preview =
                    saved
                        .grant
                        .clone()
                        .filter(|_| grant_valid)
                        .unwrap_or(ExecutionGrant {
                            manifest_sha256: saved.pins.manifest.sha256.clone(),
                            guarded_execution: cfg!(windows),
                            network: manifest.permissions.network,
                            learning: false,
                            turn_model_learning: false,
                        });
                row["disclosures"] = json!(configuration::consent_disclosures(
                    &review.package,
                    &preview
                )?);
                if state.selected.as_ref() == Some(id) {
                    selected_valid = valid;
                    if !valid {
                        learning_reasons.push("Provider settings are invalid".into());
                    }
                    if !saved.enabled {
                        learning_reasons.push("Provider is not eligible".into());
                    }
                    if !manifest.capabilities.iter().any(|cap| cap == "learning") {
                        learning_reasons.push("Provider does not support learning".into());
                    }
                    if !grant_valid {
                        learning_reasons.push("Execution consent is missing or invalid".into());
                    }
                    if !saved.grant.as_ref().is_some_and(|g| g.learning) {
                        learning_reasons.push("Completed-turn sharing is not authorized".into());
                    }
                    if row["storage"]["error"].is_string() {
                        learning_reasons.push("Approved storage is unavailable".into());
                    }
                }
            }
            Err(error) => {
                row["error"] = json!(format!("{error:#}"));
                if state.selected.as_ref() == Some(id) {
                    learning_reasons.push("Selected package is damaged or unavailable".into());
                }
            }
        }
        packages.push(row);
    }
    coherent(&state, policy, status.recovery_required)?;
    let label = if status.recovery_required {
        "recovery_required"
    } else if state.selected.is_none() {
        "unavailable"
    } else if !selected_valid {
        "damaged"
    } else if !state.enabled {
        "disabled"
    } else {
        "selected"
    };
    Ok(
        json!({"binding":current,"enabled":state.enabled,"selected":state.selected,"state":label,
        "recovery_required":status.recovery_required,"learning":{"policy":policy.unwrap_or(true),"effective":learning_reasons.is_empty(),"reasons":learning_reasons},
        "packages":packages,"client_features":["install_local","inspect","manage","adoption","mnemos_releases_v1","mnemos_updates_v1"],"limits":{"max_records":100,"max_request_bytes":MAX_REQUEST,"max_response_bytes":MAX_RESPONSE}}),
    )
}

fn review_change(workspace: &str, expected: &Binding, change: &Change) -> Result<Review> {
    validate_change(change)?;
    let (state, policy) = crate::core::cli_config::read_memory_configuration()?;
    let current = binding(workspace, &state)?;
    check_binding(expected, &current)?;
    let recovery =
        manager::transaction_marker().try_exists()? || super::update::recovery_required()?;
    if recovery {
        return Err(fail(
            "recovery_required",
            "Memory provider recovery required before settings changes",
        ));
    }
    let mut disclosures = Vec::new();
    match change {
        Change::Activate { grant, .. } | Change::Learning { enabled: true, grant: Some(grant) } => {
            let id = match change { Change::Activate {provider, ..} => provider.as_str(), _ => state.selected.as_deref().context("choose an active Memory provider first")? };
            let package = manager::review(id)?;
            ensure!(package.generation == state.generation, "Memory configuration changed during review");
            disclosures = configuration::consent_disclosures(&package.package, grant)?;
            if matches!(change, Change::Learning {..}) {
                ensure!(state.enabled && grant.learning, "Learning On requires an active provider and completed-turn sharing consent");
                disclosures.push("Learning policy and explicitly accepted permissions will be saved together.".into());
            }
        }
        Change::Learning { enabled: true, grant: None } => return Err(fail("invalid_request", "Learning On requires explicit sharing consent")),
        Change::Learning { enabled: false, grant } => {
            ensure!(grant.is_none(), "Learning Off changes policy only; omit grant");
            disclosures.push("Disable automatic Learning policy only; saved execution and sharing permissions are unchanged.".into());
        }
        Change::Storage { provider, root } => {
            let package = manager::review(provider)?;
            ensure!(package.generation == state.generation, "Memory configuration changed during review");
            let requested = root.as_ref().map(std::path::PathBuf::from).unwrap_or(manager::default_data_root(provider)?);
            let normalized = super::storage::validate(&requested, provider, &state, Some(Path::new(&current.workspace)))?;
            disclosures.push(format!("Use storage directory {}. No data is moved, copied, adopted or deleted; existing data is preserved.", normalized.display()));
        }
        Change::ClearSelection {} => disclosures.push("Select None and disable Memory. Installed packages and their data are preserved.".into()),
        Change::Disable {} => disclosures.push("Disable Memory without clearing the selected provider. Data and permissions are preserved.".into()),
    }
    coherent(&state, policy, recovery)?;
    let mut review = Review {
        binding: current,
        change: change.clone(),
        packages: state
            .packages
            .into_iter()
            .map(|(id, p)| (id, p.pins))
            .collect(),
        disclosures,
        review_id: String::new(),
    };
    review.review_id = review_digest(&review)?;
    Ok(review)
}

fn review_digest(review: &Review) -> Result<String> {
    Ok(super::manifest::sha256(&serde_json::to_vec(&(
        "aizen-memory-client-review-v1",
        &review.binding,
        &review.change,
        &review.packages,
        &review.disclosures,
    ))?))
}

fn apply(workspace: &str, reviewed: &Review) -> Result<Value> {
    if review_digest(reviewed)? != reviewed.review_id {
        return Err(fail("stale", "Reviewed action was modified; review again"));
    }
    let fresh = review_change(workspace, &reviewed.binding, &reviewed.change)?;
    if &fresh != reviewed {
        return Err(fail(
            "stale",
            "Reviewed action or complete package context changed; review again",
        ));
    }
    // Manager re-verifies payload pins and generation under its own Admission. Never nest locks.
    match &reviewed.change {
        Change::Activate { provider, grant } => {
            let package = verified_review(reviewed, provider)?;
            manager::activate(&package, grant.clone())?;
        }
        Change::Learning {
            enabled: true,
            grant: Some(grant),
        } => {
            let id = reviewed
                .binding
                .provider
                .as_deref()
                .context("no selected provider")?;
            let package = verified_review(reviewed, id)?;
            manager::set_learning(&package, true, grant.clone())?;
        }
        Change::Learning { enabled: false, .. } => {
            manager::set_learning_policy_expected(false, reviewed.binding.generation)?
        }
        Change::Learning {
            enabled: true,
            grant: None,
        } => return Err(fail("invalid_request", "missing learning consent")),
        Change::Storage { provider, root } => {
            let package = verified_review(reviewed, provider)?;
            manager::set_storage(
                &package,
                root.as_ref().map(std::path::PathBuf::from),
                Some(Path::new(&reviewed.binding.workspace)),
            )?;
        }
        Change::ClearSelection {} => manager::clear_selection(reviewed.binding.generation)?,
        Change::Disable {} => manager::disable_expected(reviewed.binding.generation)?,
    }
    // A failure after commit is explicitly ambiguous; clients must refresh, never auto-retry.
    let snapshot = snapshot(workspace).map_err(|e| {
        fail(
            "ambiguous_result",
            format!("Settings committed, but refresh failed: {e:#}"),
        )
    })?;
    Ok(json!({"binding":snapshot["binding"],"snapshot":snapshot}))
}

fn verified_review(reviewed: &Review, id: &str) -> Result<manager::PackageReview> {
    let fresh = manager::review(id)?;
    if fresh.generation != reviewed.binding.generation
        || reviewed.packages.get(id) != Some(&fresh.package.pins)
    {
        return Err(fail("stale", "Package changed after client review"));
    }
    Ok(fresh)
}

fn scope_safe(record: &api::Record, scope: &api::Scope, workspace_id: &str) -> bool {
    &record.scope == scope
        && match scope {
            api::Scope::Project => record.workspace_id.as_deref() == Some(workspace_id),
            api::Scope::User | api::Scope::Device => record.workspace_id.is_none(),
        }
}

async fn read(
    workspace: &str,
    expected: &Binding,
    query: &Query,
    request_id: &str,
) -> Result<Value> {
    let state = manager::read_state()?;
    let current = binding(workspace, &state)?;
    check_binding(expected, &current)?;
    if manager::transaction_marker().try_exists()? || super::update::recovery_required()? {
        return Err(fail(
            "recovery_required",
            "Memory provider recovery required",
        ));
    }
    let id = state
        .selected
        .as_deref()
        .ok_or_else(|| fail("unavailable", "No Memory provider selected"))?;
    if !state.enabled {
        return Err(fail("disabled", "Memory provider is disabled"));
    }
    let package = manager::review(id)?;
    if package.generation != current.generation {
        return Err(fail("stale", "Memory package changed before read"));
    }
    if !package
        .package
        .manifest
        .capabilities
        .iter()
        .any(|cap| cap == "records")
    {
        return Err(fail(
            "unsupported_capability",
            "Provider does not support record browsing",
        ));
    }
    validate_query(query)?;
    let (method, params, scope) = match query {
        Query::List { scope, limit } => (
            api::Method::List,
            json!({"scope":scope,"limit":limit}),
            scope,
        ),
        Query::Search {
            scope,
            limit,
            query,
        } => (
            api::Method::Search,
            json!({"scope":scope,"limit":limit,"query":query}),
            scope,
        ),
        Query::Get { scope, id } => (api::Method::Get, json!({"id":id}), scope),
    };
    let context = super::context::selected(
        Path::new(&current.workspace),
        "memory-client",
        request_id,
        request_id,
    )?
    .ok_or_else(|| fail("disabled", "Memory was disabled before read"))?;
    if context.generation != current.generation
        || context.workspace_id != current.workspace_id
        || context.cwd != current.workspace
        || context.workspace_root != current.workspace_root
    {
        return Err(fail("stale", "Memory context changed before read"));
    }
    let value = super::runtime::request(
        super::runtime::ApprovedInvocation::explicit_cli().without_management(),
        context,
        method,
        params,
        None,
        crate::core::cancel::TurnCancel::new(),
    )
    .await?;
    let records: Vec<api::Record> = if let Query::Get { id, .. } = query {
        let result: api::RecordResult = serde_json::from_value(value)?;
        if &result.record.id != id {
            return Err(fail(
                "invalid_response",
                "Provider returned a different record ID",
            ));
        }
        if !scope_safe(&result.record, scope, &current.workspace_id) {
            return Err(fail(
                "not_found",
                "Record is not in the requested scope/workspace",
            ));
        }
        vec![result.record]
    } else {
        let result: api::Hits = serde_json::from_value(value)?;
        if result
            .items
            .iter()
            .any(|hit| !scope_safe(&hit.record, scope, &current.workspace_id))
        {
            return Err(fail(
                "invalid_response",
                "Provider returned records outside the requested scope/workspace",
            ));
        }
        result.items.into_iter().map(|hit| hit.record).collect()
    };
    check_binding(&current, &binding(workspace, &manager::read_state()?)?)?;
    Ok(json!({"binding":current,"scope":scope,"records":records}))
}

pub(crate) fn validate_rich_result(params: &Value, value: &Value, workspace: &str) -> Result<()> {
    match params.get("command").and_then(Value::as_str) {
        Some("inspect") => {
            let query: rich::Inspect = serde_json::from_value(params["arguments"].clone())?;
            let result: rich::Inspection = serde_json::from_value(value.clone())?;
            rich_validation::inspection(&result, &query, workspace)?;
        }
        Some("manage") => {
            let command: rich::Manage = serde_json::from_value(params["arguments"].clone())?;
            let operation_id = params["operation_id"]
                .as_str()
                .context("manage requires operation identity")?;
            let result: rich::Receipt = serde_json::from_value(value.clone())?;
            rich_validation::receipt(&result, operation_id, Some(&command))?;
        }
        _ => {}
    }
    Ok(())
}

async fn rich_request(
    workspace: &str,
    expected: &Binding,
    query: Option<&rich::Inspect>,
    management: Option<(&str, &rich::Manage)>,
    request_id: &str,
) -> Result<Value> {
    let mut dispatched = false;
    let result = rich_request_inner(
        workspace,
        expected,
        query,
        management,
        request_id,
        &mut dispatched,
    )
    .await;
    result.map_err(|error| {
        if management.is_some() && !dispatched && super::runtime::check_effects().is_ok() {
            NotDispatched(error).into()
        } else {
            error
        }
    })
}

async fn rich_request_inner(
    workspace: &str,
    expected: &Binding,
    query: Option<&rich::Inspect>,
    management: Option<(&str, &rich::Manage)>,
    request_id: &str,
    dispatched: &mut bool,
) -> Result<Value> {
    let state = manager::read_state()?;
    let current = binding(workspace, &state)?;
    check_binding(expected, &current)?;
    if super::update::recovery_required()? {
        return Err(fail("recovery_required", "Mnemos package update requires explicit recovery; inspect `aizen plugin recover-update mnemos`"));
    }
    if manager::transaction_marker().try_exists()? {
        if let Some(rich::Inspect::Outcome { operation_id }) = query {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(manager::transaction_marker())?
                .take(65537)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 65536, "host outcome marker exceeds limit");
            let marker = api::strict_json(&bytes)?;
            if marker["operation_id"].as_str() == Some(operation_id)
                && marker["provider"].as_str() == current.provider.as_deref()
                && marker["generation"].as_u64() == Some(current.generation)
            {
                let result=rich::Inspection::Outcome {operation_id:operation_id.clone(),status:rich::OutcomeStatus::RecoveryRequired,recovery_required:true,receipt:None,error:Some(api::MemoryError::new(api::ErrorCode::RecoveryRequired,"Host could not prove the transaction outcome; explicit recovery is required, never retry automatically"))};
                return Ok(json!({"binding":current,"result":result}));
            }
        }
        return Err(fail("recovery_required", "Host transaction requires explicit recovery before provider execution; no action was retried"));
    }
    if !state.enabled {
        return Err(fail("disabled", "Memory provider is disabled"));
    }
    let id = state
        .selected
        .as_deref()
        .ok_or_else(|| fail("unavailable", "No Memory provider selected"))?;
    let package = manager::review(id)?;
    if package.generation != current.generation {
        return Err(fail("stale", "Memory package changed before request"));
    }
    let caps = &package.package.manifest.capabilities;
    if !caps.iter().any(|c| c == "commands")
        || (management.is_some() && !caps.iter().any(|c| c == "management"))
    {
        return Err(fail(
            "unsupported_capability",
            "Provider does not declare this rich Memory operation",
        ));
    }
    if let Some(q) = query {
        q.validate()?;
        rich_validation::scoped(q.filter())?;
    }
    if let Some((operation_id, command)) = management {
        api::opaque_id(operation_id)?;
        command.validate()?;
        rich_validation::scoped(Some(&command.filter))?;
    }
    let context = super::context::selected(
        Path::new(&current.workspace),
        "memory-client",
        request_id,
        request_id,
    )?
    .ok_or_else(|| fail("disabled", "Memory disabled before request"))?;
    if context.generation != current.generation
        || context.workspace_id != current.workspace_id
        || context.cwd != current.workspace
        || context.workspace_root != current.workspace_root
    {
        return Err(fail("stale", "Memory context changed before request"));
    }
    // Outcome inspection must also work while the engine itself has a recovery barrier;
    // it does not require a schema preflight or the background Learning capability.
    if let Some(query @ rich::Inspect::Outcome { .. }) = query {
        let value = super::runtime::request(
            super::runtime::ApprovedInvocation::explicit_cli().without_management(),
            context,
            api::Method::Command,
            json!({"command":"inspect","arguments":query}),
            None,
            crate::core::cancel::TurnCancel::new(),
        )
        .await?;
        let result: rich::Inspection = serde_json::from_value(value)?;
        rich_validation::inspection(&result, query, &current.workspace)?;
        check_binding(&current, &binding(workspace, &manager::read_state()?)?)?;
        return Ok(json!({"binding":current,"result":result}));
    }
    // Negotiation is an explicitly authorized provider read, never part of offline snapshot.
    let schema = super::runtime::request(
        super::runtime::ApprovedInvocation::explicit_cli().without_management(),
        context.clone(),
        api::Method::Command,
        json!({"command":"inspect","arguments":{"action":"schema"}}),
        None,
        crate::core::cancel::TurnCancel::new(),
    )
    .await?;
    let schema: rich::Inspection = serde_json::from_value(schema).map_err(|_| {
        fail(
            "unsupported_capability",
            "Provider does not support rich inspection v1",
        )
    })?;
    rich_validation::inspection(&schema, &rich::Inspect::Schema, &current.workspace)?;
    if management.is_some() && !matches!(schema, rich::Inspection::Schema { manage: true, .. }) {
        return Err(fail(
            "unsupported_capability",
            "Provider does not support rich management v1",
        ));
    }
    let result = if let Some(query) = query {
        if matches!(query, rich::Inspect::Schema) {
            serde_json::to_value(schema)?
        } else {
            let value = super::runtime::request(
                super::runtime::ApprovedInvocation::explicit_cli().without_management(),
                context,
                api::Method::Command,
                json!({"command":"inspect","arguments":query}),
                None,
                crate::core::cancel::TurnCancel::new(),
            )
            .await?;
            let result: rich::Inspection = serde_json::from_value(value)?;
            rich_validation::inspection(&result, query, &current.workspace)?;
            serde_json::to_value(result)?
        }
    } else if let Some((operation_id, command)) = management {
        *dispatched = true;
        let value = super::runtime::request(
            super::runtime::ApprovedInvocation::explicit_cli(),
            context,
            api::Method::Command,
            json!({"command":"manage","arguments":command,"operation_id":operation_id}),
            None,
            crate::core::cancel::TurnCancel::new(),
        )
        .await?;
        let result: rich::Receipt = serde_json::from_value(value)?;
        rich_validation::receipt(&result, operation_id, Some(command))?;
        serde_json::to_value(result)?
    } else {
        unreachable!("typed caller supplies an inspect or manage request")
    };
    check_binding(&current, &binding(workspace, &manager::read_state()?)?).map_err(|e| {
        if management.is_some() {
            fail(
                "ambiguous_result",
                format!(
                    "Mutation returned but binding changed; inspect outcome, never retry: {e:#}"
                ),
            )
        } else {
            e
        }
    })?;
    let mut response = json!({"binding":current,"result":result});
    if let Some((operation_id, _)) = management {
        response["operation_id"] = json!(operation_id);
    }
    Ok(response)
}

fn validate_install(source: &str, expected_id: &str) -> Result<()> {
    api::text(source, 32768, false)?;
    ensure!(
        Path::new(source).is_absolute(),
        "package source must be absolute"
    );
    ensure!(
        expected_id == "mnemos",
        "this setup flow requires package ID mnemos"
    );
    Ok(())
}

fn install_local(
    workspace: &str,
    expected: &Binding,
    source: &str,
    expected_id: &str,
) -> Result<Value> {
    validate_install(source, expected_id)?;
    check_binding(expected, &binding(workspace, &manager::read_state()?)?)?;
    if manager::transaction_marker().try_exists()? || super::update::recovery_required()? {
        return Err(fail(
            "recovery_required",
            "Memory provider recovery required before installation",
        ));
    }
    // Offline manager verifies source identity and refuses existing paths. It neither launches
    // nor selects the package. A partial install is retained for manual recovery, never retried.
    let installed = manager::install_local_expected(Path::new(source), Some(expected_id))?;
    let snapshot = snapshot(workspace).map_err(|e| {
        fail(
            "ambiguous_result",
            format!("Package installed, but refresh failed; do not retry installation: {e:#}"),
        )
    })?;
    Ok(json!({"binding":snapshot["binding"],"snapshot":snapshot,"installed_id":installed.pins.id}))
}

async fn release_check(workspace: &str, expected: &Binding) -> Result<Value> {
    check_binding(expected, &binding(workspace, &manager::read_state()?)?)?;
    let release = super::releases::check(None)
        .await
        .map_err(release_failure)?;
    check_binding(expected, &binding(workspace, &manager::read_state()?)?)?;
    Ok(json!({"binding":expected,"release":release}))
}

fn release_failure(error: anyhow::Error) -> anyhow::Error {
    fail(super::releases::error_code(&error), format!("{error:#}"))
}

async fn release_install(
    workspace: &str,
    expected: &Binding,
    candidate: &super::releases::Candidate,
    confirmed: bool,
) -> Result<Value> {
    ensure!(
        confirmed,
        "release installation requires explicit confirmation"
    );
    candidate.validate()?;
    check_binding(expected, &binding(workspace, &manager::read_state()?)?)?;
    let installed = super::releases::install_checked(candidate, expected.generation, || {
        let current = binding(workspace, &manager::read_state()?).map_err(|_| super::releases::fail(
            "stale", "Workspace binding is unavailable after download; review installation again",
        ))?;
        check_binding(expected, &current).map_err(|_| super::releases::fail(
            "stale", "Workspace or Memory configuration changed during download; review installation again",
        ))
    }).await.map_err(release_failure)?;
    let snapshot = snapshot(workspace).map_err(|_| {
        fail("ambiguous_result",
        "Mnemos was installed, but refresh failed. Refresh settings; do not retry installation.")
    })?;
    Ok(json!({"binding":snapshot["binding"],"snapshot":snapshot,"installed_id":installed.pins.id}))
}

async fn update_status(
    workspace: &str,
    expected: &Binding,
    mode: super::update_check::Mode,
) -> Result<Value> {
    check_binding(expected, &binding(workspace, &manager::read_state()?)?)?;
    let update = super::update_check::status(mode)
        .await
        .map_err(release_failure)?;
    check_binding(expected, &binding(workspace, &manager::read_state()?)?)?;
    Ok(json!({"binding":expected,"update":update}))
}

async fn release_update(
    workspace: &str,
    expected: &Binding,
    candidate: &super::releases::Candidate,
    expected_pins: &InstallPins,
    confirmed: bool,
) -> Result<Value> {
    ensure!(confirmed, "release update requires explicit confirmation");
    candidate.validate()?;
    let state = manager::read_state()?;
    check_binding(expected, &binding(workspace, &state)?)?;
    ensure!(
        state
            .packages
            .get("mnemos")
            .is_some_and(|p| &p.pins == expected_pins),
        "installed Mnemos pins changed; refresh and review again"
    );
    let updated = super::releases::update_checked(candidate, expected.generation, expected_pins, || {
        let current = binding(workspace, &manager::read_state()?).map_err(|_| super::releases::fail(
            "stale", "Workspace binding is unavailable after download; review update again"))?;
        check_binding(expected, &current).map_err(|_| super::releases::fail(
            "stale", "Workspace or Memory configuration changed during download; review update again"))
    }).await.map_err(release_failure)?;
    let snapshot = snapshot(workspace).map_err(|_| fail("ambiguous_result",
        "Mnemos may have been updated, but refresh failed. Inspect state; never retry automatically."))?;
    Ok(
        json!({"binding":snapshot["binding"],"snapshot":snapshot,"updated_id":"mnemos",
        "previous_pins":expected_pins,"new_pins":updated.pins}),
    )
}

async fn dispatch(request: &Request) -> Result<Value> {
    match &request.action {
        Action::Adoption {
            binding: expected,
            request: adoption,
        } => {
            let state = manager::read_state()?;
            let current = binding(&request.workspace, &state)?;
            check_binding(expected, &current)?;
            let view =
                super::adoption::dispatch(adoption, &current.workspace_root, current.generation)
                    .await?;
            Ok(json!({"binding":current,"adoption":view}))
        }
        Action::Snapshot {} => snapshot(&request.workspace),
        Action::Read { binding, query } => {
            read(&request.workspace, binding, query, &request.request_id).await
        }
        Action::Inspect { binding, query } => {
            rich_request(
                &request.workspace,
                binding,
                Some(query),
                None,
                &request.request_id,
            )
            .await
        }
        Action::Manage {
            binding,
            operation_id,
            command,
        } => {
            rich_request(
                &request.workspace,
                binding,
                None,
                Some((operation_id, command)),
                &request.request_id,
            )
            .await
        }
        Action::Review { binding, change } => {
            let review = review_change(&request.workspace, binding, change)?;
            Ok(json!({"binding":review.binding,"review":review}))
        }
        Action::Apply { review } => apply(&request.workspace, review),
        Action::InstallLocal {
            binding,
            source,
            expected_id,
        } => install_local(&request.workspace, binding, source, expected_id),
        Action::ReleaseCheck { binding } => release_check(&request.workspace, binding).await,
        Action::ReleaseInstall {
            binding,
            candidate,
            confirmed,
        } => release_install(&request.workspace, binding, candidate, *confirmed).await,
        Action::UpdateStatus { binding, mode } => {
            update_status(&request.workspace, binding, *mode).await
        }
        Action::ReleaseUpdate {
            binding,
            candidate,
            expected_pins,
            confirmed,
        } => {
            release_update(
                &request.workspace,
                binding,
                candidate,
                expected_pins,
                *confirmed,
            )
            .await
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
