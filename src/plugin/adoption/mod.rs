//! Host-owned bridge to the pinned offline Mnemos adoption CLI. No legacy engine lives here.
mod engine;
mod protocol;
mod store;
use super::{install, manager};
use crate::sandbox::request::{paths_overlap, planned_directory};
use anyhow::{ensure, Context, Result};
use protocol::{Operation, State};
pub(crate) use protocol::{Request, View};
use std::path::{Path, PathBuf};
use std::sync::Arc;
pub(crate) use store::registered_storage;

fn warnings() -> Vec<String> {
    vec![
        "The entire legacy store is transferred without merging or changing the current store. Stop all source writers before applying or resuming.".into(),
        "File counts and bytes are not semantic scope counts. Unscoped records may be user-scoped; old hashed scopes without anchors may remain orphaned. Scope repair is separate.".into(),
        "Peak space includes a complete source backup plus staged/published copied data. No free-space estimate is a guarantee.".into(),
        "Publication does not select storage or change Learning. Review Use imported memory separately.".into(),
        "Core adoption execution is currently Windows-only. Filesystem/network restrictions are guarded and advisory, not kernel isolation; a Job Object owns the process tree. No model authority is granted.".into(),
    ]
}
fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn managed_parent() -> PathBuf {
    crate::core::config::aizen_home()
        .join("memory-migrations")
        .join("mnemos")
}
fn authority(
    generation: u64,
    expected: Option<&install::InstallPins>,
) -> Result<(install::InstalledPackage, super::settings::PackageState)> {
    let (id, state) = manager::selected_at(generation)?;
    ensure!(
        id == "mnemos",
        "select and enable Mnemos explicitly before adoption"
    );
    if let Some(expected) = expected {
        ensure!(
            &state.pins == expected,
            "reviewed adoption package changed; create a fresh preview"
        );
    }
    let package = install::verify_installed(
        &manager::packages_root(),
        &state.pins,
        manager::host_target()?,
    )?;
    ensure!(package.manifest.capabilities.iter().any(|cap| cap == "legacy_adoption_v1"),
        "unsupported_capability: this package has no legacy_adoption_v1 declaration; explicitly upgrade Mnemos before adoption");
    let grant = state
        .grant
        .as_ref()
        .context("adoption execution consent missing")?;
    ensure!(
        grant.manifest_sha256 == package.pins.manifest.sha256,
        "adoption package consent changed"
    );
    super::configuration::validate_grant(&package.manifest, grant)?;
    #[cfg(windows)]
    ensure!(
        grant.guarded_execution,
        "Windows adoption requires informed guarded execution consent"
    );
    Ok((package, state))
}

pub(crate) async fn dispatch(request: &Request, workspace: &str, generation: u64) -> Result<View> {
    request.validate()?;
    match request {
        Request::Defaults {} => {
            let state = manager::read_state()?;
            let supported = state.selected.as_deref() == Some("mnemos")
                && manager::info("mnemos").is_ok_and(|p| {
                    p.manifest
                        .capabilities
                        .iter()
                        .any(|cap| cap == "legacy_adoption_v1")
                });
            let attachment_review_supported = supported
                && manager::info("mnemos").is_ok_and(|p| {
                    p.manifest
                        .capabilities
                        .iter()
                        .any(|cap| cap == "legacy_adoption_v2")
                });
            Ok(View::Defaults {
                attachment_review_supported,
                source: display(&crate::core::config::cli_memory_dir()),
                provider: state.selected,
                supported,
                enabled: state.enabled,
                managed_parent: display(&managed_parent()),
                warnings: warnings(),
            })
        }
        Request::Status {
            operation_id: Some(id),
            ..
        } => Ok(View::Operation {
            operation: Box::new(store::public(store::load(id)?)?),
        }),
        Request::Status {
            operation_id: None,
            offset,
            limit,
        } => {
            let ids = store::ids_in(&crate::core::config::aizen_home())?;
            let mut operations = Vec::new();
            let mut bytes = 0;
            for id in ids.iter().skip(*offset).take(*limit) {
                let operation = store::public(store::load(id)?)?;
                bytes += serde_json::to_vec(&operation)?.len();
                if bytes > super::client::MAX_RESPONSE / 2 {
                    break;
                }
                operations.push(operation);
            }
            Ok(View::History {
                operations,
                offset: *offset,
                total: ids.len(),
            })
        }
        Request::Inventory {
            operation_id,
            offset,
            limit,
        } => {
            let record = store::load(operation_id)?;
            let plan = store::load_plan(&record)?;
            let items = plan
                .inventory
                .iter()
                .skip(*offset)
                .take(*limit)
                .map(engine::Item::view)
                .collect();
            Ok(View::Inventory {
                operation_id: operation_id.clone(),
                items,
                offset: *offset,
                total: plan.inventory.len(),
            })
        }
        _ => {
            ensure!(
                crate::core::exec_ctx::current().is_none_or(|ctx| ctx.can_restore_workspace()
                    && ctx.dispatch_label().is_none()
                    && ctx.native_scope().is_none()
                    && ctx.overlay().is_none()),
                "delegated or speculative execution cannot request human adoption"
            );
            ensure!(
                !crate::core::cancel::current().is_some_and(|cancel| cancel.is_cancelled()),
                "adoption cancelled before admission"
            );
            // A lost observer must not drop physical ownership. This task continues through tree
            // and pipe settlement; a process crash leaves the durable marker conservative.
            let request = match request {
                Request::Preview {
                    source,
                    destination_parent,
                    attachments,
                    ..
                } => Work::Preview {
                    source: source.clone(),
                    parent: destination_parent.clone(),
                    attachments: attachments.clone(),
                },
                Request::Apply {
                    operation_id,
                    reviewed_digest,
                    ..
                } => Work::Existing {
                    id: operation_id.clone(),
                    digest: reviewed_digest.clone(),
                    mode: Mode::Apply,
                },
                Request::Resume {
                    operation_id,
                    reviewed_digest,
                    ..
                } => Work::Existing {
                    id: operation_id.clone(),
                    digest: reviewed_digest.clone(),
                    mode: Mode::Resume,
                },
                Request::Recover {
                    operation_id,
                    reviewed_digest,
                    ..
                } => Work::Existing {
                    id: operation_id.clone(),
                    digest: reviewed_digest.clone(),
                    mode: Mode::Recover,
                },
                _ => unreachable!(),
            };
            let workspace = PathBuf::from(workspace);
            tokio::spawn(async move { execute(request, workspace, generation).await })
                .await
                .context("adoption owner task stopped; inspect durable status before any retry")?
        }
    }
}
enum Work {
    Preview {
        source: Option<String>,
        parent: Option<String>,
        attachments: Vec<protocol::Attachment>,
    },
    Existing {
        id: String,
        digest: String,
        mode: Mode,
    },
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Preview,
    Apply,
    Resume,
    Recover,
}
fn append_versioned_args(args: &mut Vec<String>, mode: Mode, v2: bool, attachments: &Path) {
    if v2 {
        args.push("--machine-v2".into());
        if mode == Mode::Preview {
            args.extend(["--attachments".into(), display(attachments)]);
        }
    }
}
async fn execute(work: Work, workspace: PathBuf, generation: u64) -> Result<View> {
    ensure!(cfg!(windows), "adoption execution is unavailable on this platform: descendant settlement ownership is not supported; offline history remains available");
    super::runtime::check_effects()?;
    let lease = crate::core::workspace_txn::MemoryProviderLease::acquire(
        &workspace,
        "memory-adoption",
        None,
    )?;
    let admission = Arc::new(manager::Admission::acquire()?);
    let (package, selected) = authority(generation, None)?;
    let (mut record, mode) = match work {
        Work::Preview {
            source,
            parent,
            attachments,
        } => {
            let v2 = package
                .manifest
                .capabilities
                .iter()
                .any(|cap| cap == "legacy_adoption_v2");
            ensure!(
                v2 || attachments.is_empty(),
                "this package does not support attachment review"
            );
            let source = planned_directory(
                &source
                    .map(PathBuf::from)
                    .unwrap_or_else(crate::core::config::cli_memory_dir),
            )?;
            ensure!(
                source.is_dir(),
                "legacy source must be an existing directory"
            );
            let managed = parent.is_none();
            let parent =
                planned_directory(&parent.map(PathBuf::from).unwrap_or_else(managed_parent))?;
            let id = store::create_id()?;
            let envelope = parent.join(&id);
            store::validate_paths(&source, &envelope)?;
            if !managed {
                ensure!(
                    parent.is_dir(),
                    "external adoption parent must already exist"
                );
                // Authorize the fresh child, not its parent prefix: multiple operations may
                // coexist beneath one dedicated external parent without sharing write grants.
                super::storage::validate(
                    &envelope,
                    "mnemos",
                    &manager::read_state()?,
                    Some(&workspace),
                )?;
                for broad in [
                    planned_directory(&crate::core::config::home_dir())?,
                    planned_directory(&crate::core::config::aizen_home())?,
                    planned_directory(&workspace)?,
                ] {
                    ensure!(!crate::sandbox::request::path_within(&broad, &parent),
                        "external adoption parent must be dedicated, not HOME/Core/workspace or their ancestor");
                }
            }
            ensure!(
                !paths_overlap(&source, &parent),
                "adoption source and destination parent overlap"
            );
            install::ensure_private_root(&parent)?;
            ensure!(
                !envelope.try_exists()?,
                "fresh adoption envelope already exists"
            );
            install::create_private_dir(&envelope)?;
            let envelope = planned_directory(&envelope)?;
            let operation = Operation {
                operation_id: id,
                provider: "mnemos".into(),
                state: State::Planning,
                source: display(&source),
                data_root: display(&envelope.join("data")),
                journal: display(&envelope.join("journal")),
                backup: display(&envelope.join("journal/backup")),
                previous_root: display(&manager::data_root_for("mnemos", &selected)?),
                reviewed_digest: None,
                summary: None,
                message: None,
                attachments: vec![],
                diagnostic: None,
            };
            let record = store::Record {
                schema_version: 1,
                operation,
                pins: package.pins.clone(),
                generation,
                envelope_identity: store::identity(&envelope)?,
                envelope,
                plan_sha256: None,
                published_identity: None,
                engine_format: if v2 {
                    engine::FORMAT_V2
                } else {
                    engine::FORMAT
                }
                .into(),
                authorized_attachments: attachments,
            };
            ensure!(
                !store::directory(&record.operation.operation_id)?.try_exists()?,
                "adoption operation collision"
            );
            store::save(&record)?;
            (record, Mode::Preview)
        }
        Work::Existing { id, digest, mode } => {
            let record = store::load(&id)?;
            ensure!(
                record.operation.reviewed_digest.as_deref() == Some(&digest),
                "adoption confirmation digest changed"
            );
            ensure!(
                record.pins == package.pins,
                "reviewed adoption package changed; no launch permitted"
            );
            ensure!(
                record.operation.state != State::SettlementUnknown,
                "adoption physical settlement unknown; no retry permitted"
            );
            if record.operation.state == State::PublishedVerified {
                ensure!(
                    mode == Mode::Recover,
                    "adoption is already published; use separate Storage review, not replay"
                );
                // Once live, the store can legitimately evolve. The host receipt is authoritative.
                registered_storage(
                    Path::new(&record.operation.data_root),
                    "mnemos",
                    &crate::core::config::aizen_home(),
                )?;
                return Ok(View::Operation {
                    operation: Box::new(record.operation),
                });
            }
            if mode == Mode::Apply {
                ensure!(record.operation.state == State::Preview && record.generation == generation, "adoption apply review changed or operation already attempted; inspect and explicitly resume/recover");
                ensure!(
                    !Path::new(&record.operation.data_root).try_exists()?
                        && !Path::new(&record.operation.journal).try_exists()?,
                    "adoption destination/journal exists; no apply replay permitted"
                );
            } else {
                ensure!(
                    record.operation.state == State::Interrupted,
                    "only an interrupted adoption can be resumed/recovered"
                );
            }
            store::load_plan(&record)?;
            (record, mode)
        }
    };
    store::validate_envelope(&record)?;
    let plan_path = store::plan_path(&record.operation.operation_id)?;
    if mode != Mode::Recover {
        let source = planned_directory(Path::new(&record.operation.source))?;
        ensure!(
            source.is_dir(),
            "legacy source is unavailable; use explicit publication recovery if already completed"
        );
        store::validate_paths(&source, &record.envelope)?;
    }
    let private_temp = record
        .envelope
        .join(format!("temp-{}", crate::core::persist::unique_sequence()));
    ensure!(
        !private_temp.try_exists()?,
        "adoption private temp collision"
    );
    install::ensure_private_root(&private_temp.join("home"))?;
    let v2 = record.engine_format == engine::FORMAT_V2;
    ensure!(
        !v2 || package
            .manifest
            .capabilities
            .iter()
            .any(|cap| cap == "legacy_adoption_v2"),
        "reviewed v2 package capability missing"
    );
    let attachment_path =
        store::directory(&record.operation.operation_id)?.join("attachments.json");
    if mode == Mode::Preview && v2 {
        ensure!(
            !attachment_path.try_exists()?,
            "attachment authorization input already exists"
        );
        crate::core::persist::atomic_write_owner_only(
            &attachment_path,
            &serde_json::to_vec(&record.authorized_attachments)?,
        )?;
    }
    let policy = crate::sandbox::request::NativeAdoptionPolicy::new(
        package.directory.clone(),
        if mode == Mode::Recover {
            None
        } else {
            Some(PathBuf::from(&record.operation.source))
        },
        if mode == Mode::Preview && v2 {
            Some(attachment_path.clone())
        } else if mode == Mode::Preview || mode == Mode::Recover {
            None
        } else {
            Some(plan_path.clone())
        },
        record.envelope.clone(),
        private_temp.clone(),
        matches!(mode, Mode::Apply | Mode::Resume),
    )?;
    let digest = record.operation.reviewed_digest.clone().unwrap_or_default();
    let mut args = match mode {
        Mode::Preview => vec![
            "adoption".into(),
            "plan".into(),
            "--source".into(),
            record.operation.source.clone(),
            "--destination".into(),
            record.operation.data_root.clone(),
            "--dry-run".into(),
        ],
        Mode::Apply => vec![
            "adoption".into(),
            "apply".into(),
            "--plan".into(),
            display(&plan_path),
            "--reviewed-digest".into(),
            digest,
            "--journal".into(),
            record.operation.journal.clone(),
        ],
        Mode::Resume | Mode::Recover => vec![
            "adoption".into(),
            if mode == Mode::Resume {
                "resume"
            } else {
                "inspect"
            }
            .into(),
            "--journal".into(),
            record.operation.journal.clone(),
            "--reviewed-digest".into(),
            digest,
        ],
    };
    append_versioned_args(&mut args, mode, v2, &attachment_path);
    // Fresh generation/pins/grant/path checks are immediately adjacent to physical preparation.
    let (_, fresh) = authority(generation, Some(&record.pins))?;
    store::validate_envelope(&record)?;
    let request =
        crate::sandbox::request::SandboxRequest::native_adoption(package.entrypoint, args, policy)?;
    if mode != Mode::Preview {
        record.operation.state = State::Interrupted;
        record.operation.message = Some("Operation was authorized. Until a verified publication receipt is recorded, effects may exist; inspect status and explicitly resume/recover.".into());
        store::save(&record)?;
    }
    let result = super::runtime::adoption_execute(
        request,
        admission.clone(),
        lease,
        &record.operation.operation_id,
        &private_temp,
        fresh.grant.as_ref().is_some_and(|g| g.guarded_execution),
    )
    .await;
    let processed = result.and_then(|bytes| -> Result<()> {
        let bytes = if v2 {
            match engine::Report::parse(&bytes)? {
                engine::Report::Ok { plan, outcome, .. } => serde_json::to_vec(
                    &if mode == Mode::Preview { plan } else { outcome }
                        .context("adoption report has the wrong result kind")?)?,
                engine::Report::NeedsReview { attachments, .. } => {
                    ensure!(mode == Mode::Preview, "unexpected attachment review after transfer");
                    record.operation.state = State::NeedsReview;
                    record.operation.attachments = attachments;
                    record.operation.message = Some("Review the exact archive attachments. They will be preserved in backup only, not imported as Memory.".into());
                    return Ok(());
                }
                engine::Report::Error { diagnostic, .. } => {
                    record.operation.state = if mode == Mode::Preview { State::CheckFailed } else { State::Interrupted };
                    record.operation.message = Some(diagnostic.message().into());
                    record.operation.diagnostic = Some(diagnostic);
                    return Ok(());
                }
            }
        } else { bytes };
        if mode == Mode::Preview {
            let plan = engine::Plan::parse_authorized(
                &bytes,
                &record.operation.source,
                &record.operation.data_root,
                &record.engine_format,
                &record.authorized_attachments,
            )?;
            ensure!(
                !plan_path.try_exists()?,
                "immutable adoption plan already exists"
            );
            crate::core::persist::atomic_write_owner_only(&plan_path, &bytes)?;
            record.plan_sha256 = Some(super::manifest::sha256(&bytes));
            record.operation.reviewed_digest = Some(plan.digest.clone());
            record.operation.summary = Some(plan.summary());
            record.operation.state = State::Preview;
        } else {
            ensure!(bytes.len() <= 64 * 1024, "adoption outcome exceeds bound");
            let outcome: engine::Outcome = serde_json::from_slice(&bytes).map_err(|_| {
                anyhow::anyhow!("invalid adoption terminal outcome; raw provider output withheld")
            })?;
            ensure!(
                outcome.format == record.engine_format
                    && outcome.state == "published_verified"
                    && !outcome.restored_all_source_files
                    && outcome.reviewed_digest
                        == record
                            .operation
                            .reviewed_digest
                            .as_deref()
                            .unwrap_or_default()
                    && outcome.destination == Path::new(&record.operation.data_root)
                    && outcome.journal == Path::new(&record.operation.journal),
                "adoption publication receipt does not match reviewed operation"
            );
            store::validate_envelope(&record)?;
            record.published_identity =
                Some(store::identity(Path::new(&record.operation.data_root))?);
            record.operation.state = State::PublishedVerified;
        }
        record.operation.message = None;
        record.operation.diagnostic = None;
        Ok(())
    });
    if let Err(error) = processed {
        let unknown = !matches!(
            crate::agent::tool_error_effect_state(&error),
            crate::agent::tools::ToolEffectState::Settled
        ) || manager::transaction_marker().try_exists()?;
        record.operation.state = if unknown {
            State::SettlementUnknown
        } else if mode == Mode::Preview {
            State::CheckFailed
        } else {
            State::Interrupted
        };
        // Only fixed host diagnostics cross the Desktop boundary; error chains stay private.
        record.operation.message = Some(if unknown {
            "Physical settlement is unknown. Do not retry; inspect retained status."
        } else {
            "No validated adoption report was received. Check source layout, access and retained status; provider stderr is withheld."
        }.into());
    }
    store::save(&record)?;
    Ok(View::Operation {
        operation: Box::new(record.operation),
    })
}

#[cfg(test)]
mod tests;
