//! Receipt-linked destination verification for one completed task integration.
//!
//! Selection is metadata plus fresh Git identity. The command list is frozen before approval and
//! runs against the exact recorded destination. A record carries no source bytes, grants no replay
//! authority, and a failed check is a settled but unverified outcome rather than a Git failure.

use super::*;
use crate::agent::verify_gate::{FrozenVerifyPlan, VerifyGateResult};
use crate::core::persist::FileFingerprint;

const MAX_RECORDS: usize = 4096;
const MAX_RECORD_BYTES: u64 = 128 * 1024;

/// Historical acceptance links, not a live capability or permission to modify a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifiedTaskIntegration {
    pub(crate) integration: String,
    pub(crate) verification: String,
    pub(crate) destination: GitOid,
}

/// Resolve from fresh bounded records, never a child summary or caller-provided success flag.
/// Historical acceptance survives later verified integrations; launch checks the current tree below.
pub(crate) fn accepted_task_integration(
    root: &Path,
    expected: &super::super::owned::TaskHandoff,
) -> Result<Option<VerifiedTaskIntegration>> {
    handoff::validate_evidence(expected, &expected.registration.registration_id)?;
    let identity = inspect_identity(root, None)?;
    require_unmodified_history(&identity, None)?;
    require_no_pending(&identity)?;
    require_settled(&identity, None)?;
    let records = records(&identity)?;
    let mut accepted = None;
    for receipt in receipts::scan(&identity)? {
        if !matches!(&receipt.change, Change::IntegrateTask { receipt: id } if id == &expected.registration.registration_id)
        {
            continue;
        }
        ensure!(
            receipt.handoff.as_ref() == Some(expected),
            "integration handoff differs from the workflow's exact result"
        );
        if !receipt.verified {
            continue;
        }
        receipts::require_completed(&receipt)?;
        let id = receipt
            .verification
            .as_deref()
            .context("verified integration lost its record link")?;
        let record = records
            .iter()
            .find(|r| r.id == id)
            .context("linked verification record is missing")?;
        ensure!(
            record.verified
                && record.integration == receipt.id
                && Some(&record.destination) == receipt.after.as_ref()
                && record.branch == receipt.branch_before
                && source_branch(root, None)?.as_deref() == Some(receipt.branch_before.as_str()),
            "integration/verification destination links do not match"
        );
        let range = format!(
            "{}..{}",
            head(root, None)?.as_str(),
            record.destination.as_str()
        );
        ensure!(
            query(root, &["rev-list", "--max-count=1", &range], None)?.is_empty(),
            "verified integration history is not retained at the workflow destination"
        );
        ensure!(
            accepted.is_none(),
            "multiple verified integrations claim one workflow handoff"
        );
        accepted = Some(VerifiedTaskIntegration {
            integration: receipt.id,
            verification: id.into(),
            destination: record.destination.clone(),
        });
    }
    Ok(accepted)
}

/// A dependent Git task starts from an exact verified destination, never merely an ancestor or
/// the workflow's original base. Untracked build output is permitted; tracked drift is not.
pub(crate) fn require_verified_destination(root: &Path, expected: &GitOid) -> Result<()> {
    let identity = inspect_identity(root, None)?;
    require_no_pending(&identity)?;
    require_settled(&identity, None)?;
    require_tracked_clean(&identity, None)?;
    ensure!(
        head(root, None)? == *expected,
        "workflow destination moved after verification"
    );
    let branch =
        source_branch(root, None)?.context("workflow destination requires a local branch")?;
    let source = crate::agent::verify_gate::source_fingerprint(root)
        .context("cannot inspect verified destination source")?;
    let source = source
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let records = records(&identity)?;
    let receipts = receipts::scan(&identity)?;
    ensure!(
        records.iter().any(|record| record.verified
            && record.destination == *expected
            && record.branch == branch
            && record.source_identity.as_deref() == Some(source.as_str())
            && receipts
                .iter()
                .any(|receipt| receipt.id == record.integration
                    && receipt.verified
                    && receipt.verification.as_deref() == Some(record.id.as_str())
                    && receipt.after.as_ref() == Some(expected)
                    && receipt.branch_before == branch)),
        "current workflow destination has no linked verification for its exact source identity"
    );
    Ok(())
}

/// One bounded attempt record. `output` is the tail of the last command, never a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationRecord {
    schema: u32,
    pub(crate) id: String,
    /// The integration receipt whose destination was verified.
    pub(crate) integration: String,
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    branch: String,
    destination: GitOid,
    pub(crate) commands: Vec<String>,
    pub(crate) state: String,
    pub(crate) verified: bool,
    pub(crate) termination: String,
    pub(crate) passed_commands: Vec<String>,
    pub(crate) output: String,
    pub(crate) duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_identity: Option<String>,
    pub(crate) started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<u64>,
}

#[derive(Debug)]
struct FrozenVerification {
    /// Our own pending record id, fixed at selection so revalidation can ignore only itself.
    id: String,
    integration: ChangeReceipt,
    /// The integration's recorded resulting commit; every record must name this destination.
    destination: GitOid,
    integration_path: PathBuf,
    integration_resource: NativeResource,
    integration_fingerprint: FileFingerprint,
    plan: FrozenVerifyPlan,
    /// A settled pass persisted before a crash prevented its integration link. Exact bytes are
    /// frozen for metadata-only relinking; running/unknown attempts can never fill this field.
    durable_pass: Option<FrozenPass>,
}

#[derive(Debug)]
struct FrozenPass {
    record: VerificationRecord,
    path: PathBuf,
    resource: NativeResource,
    fingerprint: FileFingerprint,
}

impl FrozenPass {
    fn select(identity: &WorkspaceIdentity, record: VerificationRecord) -> Result<Self> {
        let path = verification_directory(identity).join(format!("{}.json", record.id));
        let resource = NativeResource::resolve(&path)?;
        let path = resource.path().to_path_buf();
        resource.verify()?;
        let (current, bytes) = read_record(identity, &path)?;
        ensure!(
            current == record,
            "durable verification pass changed during selection"
        );
        resource.verify()?;
        Ok(Self {
            record,
            path,
            resource,
            fingerprint: FileFingerprint::for_bytes(&bytes),
        })
    }

    fn revalidate(&self, identity: &WorkspaceIdentity) -> Result<()> {
        self.resource.verify()?;
        let (record, bytes) = read_record(identity, &self.path)?;
        ensure!(
            record == self.record && FileFingerprint::for_bytes(&bytes) == self.fingerprint,
            "selected durable verification pass changed; prepare relinking again"
        );
        self.resource.verify()
    }
}

/// Frozen plan for one exact settled handoff integration.
#[derive(Debug)]
pub(crate) struct PreparedVerification {
    identity: WorkspaceIdentity,
    witnesses: Vec<NativeResource>,
    selected: FrozenVerification,
}

impl PreparedVerification {
    pub(crate) fn prepare(root: &Path, integration_id: &str) -> Result<Self> {
        require_parent()?;
        let identity = inspect_identity(root, None)?;
        require_unmodified_history(&identity, None)?;
        let witnesses = [
            &identity.canonical_root,
            identity
                .canonical_git_dir
                .as_ref()
                .context("missing Git directory")?,
            identity
                .canonical_common_git_dir
                .as_ref()
                .context("missing common Git directory")?,
        ]
        .into_iter()
        .map(|path| NativeResource::resolve(&path.join(".aizen-verify-boundary")))
        .collect::<Result<Vec<_>>>()?;
        let selected = FrozenVerification::select(&identity, integration_id)?;
        let prepared = Self {
            identity,
            witnesses,
            selected,
        };
        prepared.revalidate(None)?;
        Ok(prepared)
    }

    pub(crate) fn preview(&self) -> Result<String> {
        let integration = &self.selected.integration;
        let relinking = self.selected.durable_pass.is_some();
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "mode":if relinking { "relink_durable_pass" } else { "run_commands" },
            "verification_record":self.selected.durable_pass.as_ref().map(|pass| &pass.record.id),
            "workspace":self.identity.canonical_root,
            "branch":integration.branch_before,
            "destination":integration.after,
            "integration_receipt":integration.id,
            "task_handoff":integration.handoff,
            "commands":self.selected.plan.commands(),
            "timeout_secs":self.selected.plan.timeout_secs(),
            "effects":if relinking {
                "Metadata only: links this exact durable settled pass to its integration after fresh source, command, trust, Git and settlement checks. Runs no verification command and creates no new attempt."
            } else {
                "Runs the listed supported verification commands in this destination worktree. Build output and dependency stores may change; tracked files must stay unchanged. No Git commit, branch, merge or reset is performed."
            },
            "verification":"A recorded pass proves only that these commands succeeded on this exact source identity. A failure stays reported and is never inferred as success."
        }))?)
    }

    fn revalidate(&self, owner: Option<&Owner>) -> Result<()> {
        require_parent()?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        ensure!(
            inspect_identity(&self.identity.canonical_root, owner)? == self.identity,
            "repository/worktree binding changed; prepare verification again"
        );
        crate::features::coop::require_no_other_workspace_owner(&self.identity)?;
        self.selected.revalidate(&self.identity, owner)?;
        // Verification records live outside the operation bucket, so every existing operation
        // receipt must already be settled; a running attempt of our own is checked below.
        require_no_pending(&self.identity)?;
        require_settled(&self.identity, Some(&self.selected.id))?;
        for witness in &self.witnesses {
            witness.verify()?;
        }
        Ok(())
    }

    /// Execute the frozen plan once. Publication of a RUNNING record precedes any command, so an
    /// interrupted attempt is visible as unfinished instead of looking like no attempt at all.
    pub(crate) fn execute(self) -> Result<VerificationRecord> {
        require_parent()?;
        if self.selected.durable_pass.is_none() {
            FrozenVerifyPlan::require_runtime()?;
        }
        let cancel = crate::core::cancel::current();
        let writer = WorkspaceWriterLease::acquire_identity(
            self.identity.clone(),
            Duration::from_secs(15),
            cancel.as_ref(),
            "approved destination verification",
        )?;
        let barrier = BackgroundProcessBarrier::acquire(cancel.as_ref())?;
        let registration =
            crate::features::coop::acquire_workspace_registration_barrier(&self.identity)?;
        self.revalidate(None)?;
        let ceiling = physical()?;
        crate::core::speculation::legacy::require_settled(
            &self.identity.canonical_root,
            &writer,
            &ceiling,
        )?;
        crate::core::speculation::namespace::inspect_existing(
            self.identity.clone(),
            &crate::core::config::aizen_home(),
        )?;
        crate::core::native_journal::require_workspace_settled(&self.identity.canonical_root)?;
        drop(ceiling);
        let owner = Arc::new(VerificationOwner {
            _writer: writer,
            _background: barrier,
            _registration: registration,
        });
        if let Some(pass) = &self.selected.durable_pass {
            // This branch never enters the command runner and never rewrites the pass record.
            // The same writer/background/registration/physical and recovery gates still apply.
            self.selected.record_verified(&self.identity)?;
            return Ok(pass.record.clone());
        }
        let (directory, ancestor) = crate::core::speculation::namespace::plan_metadata_directory(
            &verification_directory(&self.identity),
        )?;
        let path = directory.join(format!("{}.json", self.selected.id));
        let mut record = self.selected.started_record(&self.identity);
        let started_at = record.started_at;
        let witness = {
            let _physical = physical()?;
            ancestor.verify()?;
            let witness =
                crate::core::speculation::namespace::create_metadata_directory(&directory)?;
            ancestor.verify()?;
            ensure!(!path.try_exists()?, "verification record already exists");
            persist_record(&path, &record, &witness)?;
            witness
        };
        let ctx = crate::core::exec_ctx::current().unwrap_or_default();
        let plan_result = self
            .selected
            .plan
            .execute(&ctx, owner.clone() as Arc<dyn Send + Sync>);
        let mut verified = false;
        match plan_result {
            Ok(result) if result.effect_state == crate::agent::tools::ToolEffectState::Settled => {
                verified = result.accepted();
                record =
                    self.selected
                        .finished_record(&self.identity, &result, verified, started_at);
                if verified {
                    // Recheck source, tracked index, identity and the exact selected receipt after
                    // process settlement, not only inside the command loop.
                    if let Err(error) = self.revalidate(None) {
                        if error
                            .downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
                            .is_some()
                        {
                            return Err(error).context("final verification inspection did not settle; running record retained");
                        }
                        verified = false;
                        record.verified = false;
                        record.state = "failed".into();
                        record.termination = "error".into();
                        record.output = tail(
                            &format!("final verification inspection failed: {error:#}"),
                            4_000,
                        );
                    }
                }
            }
            Ok(result) => {
                // The runner quarantined its physical ownership because a descendant's exit was
                // not proven. Keep the running record authoritative; never publish a settled pass.
                return Err(anyhow::Error::new(
                    crate::agent::tools::ToolEffectsUnsettled(result.effect_state),
                )
                .context(format!(
                    "verification {} did not settle; its running record is retained and blocks a retry ({})",
                    record.id, result.output
                )));
            }
            Err(error) => {
                let unsettled = error
                    .downcast_ref::<crate::agent::tools::ToolEffectsUnsettled>()
                    .is_some();
                record.state = if unsettled {
                    "unsettled".into()
                } else {
                    "failed".into()
                };
                record.termination = if unsettled {
                    "unsettled".into()
                } else {
                    "error".into()
                };
                record.output = tail(&format!("{error:#}"), 4_000);
                record.finished_at = Some(now_unix());
                if unsettled {
                    // Physical ownership is still held by the process tree; keep the running
                    // record authoritative instead of publishing a settled-looking result.
                    return Err(error).context(format!(
                        "verification {} did not settle; its running record is retained and blocks a retry",
                        record.id
                    ));
                }
            }
        }
        let publication = (|| {
            let _physical = physical()?;
            persist_record(&path, &record, &witness)
        })();
        publication.context(
            "verification finished but its record could not be published; inspect the retained running record before retrying",
        )?;
        if verified {
            // The record is already durable at this point. A failure to link it leaves a settled
            // pass recorded on the integration's behalf with no verified marker; report that
            // explicitly instead of returning a verified-looking success.
            self.selected
                .record_verified(&self.identity)
                .context(format!(
                    "verification {} passed and its record is durable, but the integration receipt could not be marked verified; inspect both records before retrying",
                    record.id
                ))?;
        }
        Ok(record)
    }
}

struct VerificationOwner {
    _writer: WorkspaceWriterLease,
    _background: BackgroundProcessBarrier,
    _registration: crate::features::coop::WorkspaceRegistrationBarrier,
}

impl FrozenVerification {
    fn select(identity: &WorkspaceIdentity, id: &str) -> Result<Self> {
        ensure!(
            valid_operation_id(id),
            "select the exact completed integration receipt to verify"
        );
        let directory = receipt_directory(identity);
        let path = directory.join(format!("{id}.json"));
        let resource = NativeResource::resolve(&path)?;
        // Reuse the validated canonical spelling for later atomic publication and Windows ACLs.
        let path = resource.path().to_path_buf();
        resource.verify()?;
        let (integration, bytes) = receipts::read(identity, &path)?;
        validate_destination(identity, id, &integration, None)?;
        require_tracked_clean(identity, None)?;
        let destination = integration
            .after
            .clone()
            .context("integration receipt has no resulting commit")?;
        let prior = prior_verification(identity, id, &destination, None)?;
        let plan = FrozenVerifyPlan::prepare(
            &identity.canonical_root,
            crate::agent::verify_gate::destination_verify_timeout(&identity.canonical_root),
        )?;
        let durable_pass = prior
            .map(|record| FrozenPass::select(identity, record))
            .transpose()?;
        resource.verify()?;
        Ok(Self {
            id: durable_pass
                .as_ref()
                .map_or_else(new_record_id, |pass| pass.record.id.clone()),
            integration,
            destination,
            integration_path: path,
            integration_resource: resource,
            integration_fingerprint: FileFingerprint::for_bytes(&bytes),
            plan,
            durable_pass,
        })
    }

    fn revalidate(&self, identity: &WorkspaceIdentity, owner: Option<&Owner>) -> Result<()> {
        self.integration_resource.verify()?;
        let (integration, bytes) = receipts::read(identity, &self.integration_path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.integration_fingerprint,
            "selected integration receipt changed; prepare verification again"
        );
        validate_destination(identity, &integration.id, &integration, owner)?;
        require_tracked_clean(identity, owner)?;
        let destination = integration
            .after
            .clone()
            .context("integration receipt has no resulting commit")?;
        let prior = prior_verification(identity, &integration.id, &destination, Some(&self.id))?;
        ensure!(
            prior.is_none(),
            "another durable pass appeared after preparation; inspect before relinking"
        );
        self.plan.revalidate()?;
        if let Some(pass) = &self.durable_pass {
            pass.revalidate(identity)?;
            ensure!(pass.record.integration == integration.id
                && pass.record.destination == destination
                && pass.record.branch == integration.branch_before
                && pass.record.commands == self.plan.commands()
                && pass.record.source_identity.as_deref() == Some(self.plan.source_identity().as_str()),
                "durable pass source, commands or destination no longer match; no commands were replayed");
        }
        self.integration_resource.verify()
    }

    fn started_record(&self, identity: &WorkspaceIdentity) -> VerificationRecord {
        VerificationRecord {
            schema: 1,
            id: self.id.clone(),
            integration: self.integration.id.clone(),
            root: identity.canonical_root.clone(),
            git_dir: identity.canonical_git_dir.clone().unwrap_or_default(),
            common_dir: identity
                .canonical_common_git_dir
                .clone()
                .unwrap_or_default(),
            branch: self.integration.branch_before.clone(),
            destination: self.destination.clone(),
            commands: self.plan.commands(),
            state: "running".into(),
            verified: false,
            termination: "running".into(),
            passed_commands: Vec::new(),
            output: String::new(),
            duration_ms: 0,
            source_identity: None,
            started_at: now_unix(),
            finished_at: None,
        }
    }

    fn finished_record(
        &self,
        identity: &WorkspaceIdentity,
        result: &VerifyGateResult,
        verified: bool,
        started_at: u64,
    ) -> VerificationRecord {
        let mut record = self.started_record(identity);
        record.started_at = started_at;
        record.state = "settled".into();
        record.verified = verified;
        record.termination = serde_json::to_value(result.termination)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        record.passed_commands = result.passed_commands.clone();
        record.output = tail(&result.output, 4_000);
        record.duration_ms = u64::try_from(result.duration_ms).unwrap_or(u64::MAX);
        record.source_identity = result.source_identity.map(|digest| {
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        });
        record.finished_at = Some(now_unix());
        record
    }

    /// Mark the integration receipt verified only after the machine-checked pass was published.
    fn record_verified(&self, identity: &WorkspaceIdentity) -> Result<()> {
        let (_, witness) = crate::core::speculation::namespace::plan_metadata_directory(
            &receipt_directory(identity),
        )?;
        witness.verify()?;
        self.integration_resource.verify()?;
        let (mut integration, bytes) = receipts::read(identity, &self.integration_path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.integration_fingerprint,
            "integration receipt changed before its verification was recorded"
        );
        ensure!(
            inspect_identity(&identity.canonical_root, None)? == *identity,
            "workspace identity changed before verification certification"
        );
        self.plan.revalidate()?;
        require_tracked_clean(identity, None)?;
        validate_destination(identity, &integration.id, &integration, None)?;
        integration.verified = true;
        integration.verification = Some(self.id.clone());
        self.revalidate(identity, None)?;
        let _physical = physical()?;
        witness.verify()?;
        self.integration_resource.verify()?;
        let (_, bytes) = receipts::read(identity, &self.integration_path)?;
        ensure!(
            FileFingerprint::for_bytes(&bytes) == self.integration_fingerprint,
            "integration receipt changed before verification-link publication"
        );
        if let Some(pass) = &self.durable_pass {
            pass.revalidate(identity)?;
        }
        persist(&self.integration_path, &integration, &witness)?;
        self.integration_resource.verify()
    }
}

/// The integration receipt must describe exactly the destination this verification would cover.
fn validate_destination(
    identity: &WorkspaceIdentity,
    id: &str,
    receipt: &ChangeReceipt,
    owner: Option<&Owner>,
) -> Result<()> {
    let Change::IntegrateTask {
        receipt: integration_id,
    } = &receipt.change
    else {
        anyhow::bail!("verification requires a completed task integration receipt");
    };
    let evidence = receipt
        .handoff
        .as_ref()
        .context("integration receipt lost its frozen handoff")?;
    ensure!(
        !receipt.verified && receipt.verification.is_none(),
        "this integration is already verified; prepare a new change instead of re-running its checks"
    );
    ensure!(
        // `integration_id` names the task registration the merge fast-forwarded; the operation's
        // own receipt id is the one the operator selects.
        integration_id == &evidence.registration.registration_id
            && receipt.id == id
            && receipt.schema == 2
            && receipt.state == "settled_unverified"
            && receipt.reconciliation.is_none()
            && receipt.targets.as_slice() == std::slice::from_ref(&evidence.tip)
            && receipt.after.as_ref() == Some(&evidence.tip)
            && evidence.base == receipt.before
            && !evidence.integrated
            && receipt.resulting_commits.is_empty(),
        "integration receipt is not a settled, unverified handoff integration"
    );
    let root = &identity.canonical_root;
    ensure!(
        head(root, owner)? == evidence.tip,
        "destination HEAD moved after integration; verification approval is stale"
    );
    ensure!(
        source_branch(root, owner)?.as_deref() == Some(receipt.branch_before.as_str()),
        "destination branch changed after integration; prepare verification again"
    );
    Ok(())
}

/// A settled failure may be repeated. One durable pass may only be relinked, never rerun;
/// running/unknown attempts remain barriers even if another record claims a pass.
fn prior_verification(
    identity: &WorkspaceIdentity,
    integration: &str,
    destination: &GitOid,
    except: Option<&str>,
) -> Result<Option<VerificationRecord>> {
    let mut durable_pass = None;
    for record in records(identity)? {
        if record.integration != integration || Some(record.id.as_str()) == except {
            continue;
        }
        ensure!(
            &record.destination == destination,
            "an existing verification record for this integration covers another destination; inspect it before retrying"
        );
        ensure!(
            !matches!(record.state.as_str(), "running" | "unsettled"),
            "this integration already has an unfinished or unknown verification attempt; inspect its record before retrying"
        );
        if record.verified {
            ensure!(
                durable_pass.is_none(),
                "multiple durable passes claim this integration; inspect without replaying"
            );
            durable_pass = Some(record);
        }
    }
    Ok(durable_pass)
}

/// Tracked state only. Build output and dependency stores are ordinary here, unlike a mutation.
pub(super) fn require_tracked_clean(
    identity: &WorkspaceIdentity,
    owner: Option<&Owner>,
) -> Result<()> {
    require_unmodified_history(identity, owner)?;
    require_idle_operation(
        identity
            .canonical_git_dir
            .as_ref()
            .context("missing Git directory")?,
    )?;
    let root = &identity.canonical_root;
    validate_visible_index(&query(root, &["ls-files", "-v", "-f", "-z"], owner)?)?;
    ensure!(
        query(
            root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=no"],
            owner
        )?
        .is_empty(),
        "destination has pending tracked edits; verification must cover the integrated commit exactly"
    );
    Ok(())
}

/// Every recorded operation must be settled. Verification is read-only for Git state, so no
/// record of its own ever lives in this bucket.
fn require_no_pending(identity: &WorkspaceIdentity) -> Result<()> {
    for receipt in receipts::scan(identity)? {
        receipts::require_completed(&receipt)?;
    }
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn tail(text: &str, limit: usize) -> String {
    let mut tail: Vec<char> = text.chars().rev().take(limit).collect();
    tail.reverse();
    tail.into_iter().collect()
}

fn new_record_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "verify-{}-{nanos}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    )
}

fn verification_directory(identity: &WorkspaceIdentity) -> PathBuf {
    crate::core::config::aizen_home()
        .join("verification-v1")
        .join(&identity.repo_id)
        .join(&identity.worktree_id)
}

fn persist_record(
    path: &Path,
    record: &VerificationRecord,
    witness: &NativeResource,
) -> Result<()> {
    witness.verify()?;
    crate::core::persist::atomic_write_owner_only_checked(
        path,
        &serde_json::to_vec_pretty(record)?,
    )?;
    witness.verify()
}

/// Durable running/unknown verification attempts block every workspace mutation/focus/removal,
/// not only another verification of the same integration. The executor excludes only its own ID.
pub(super) fn require_settled(identity: &WorkspaceIdentity, except: Option<&str>) -> Result<()> {
    for record in records(identity)? {
        if Some(record.id.as_str()) != except {
            require_record_settled(&record)?;
        }
    }
    Ok(())
}

fn require_record_settled(record: &VerificationRecord) -> Result<()> {
    ensure!(
        !matches!(record.state.as_str(), "running" | "unsettled"),
        "unfinished destination verification {}; inspect before workspace mutation or navigation",
        record.id
    );
    Ok(())
}

/// Branch retirement affects shared refs, so every recorded worktree must be settled. Do not
/// discover the stored roots: a missing worktree or released liveness lock proves no settlement.
pub(super) fn require_repository_settled(identity: &WorkspaceIdentity) -> Result<()> {
    let directory = verification_directory(identity)
        .parent()
        .context("missing verification repository directory")?
        .to_path_buf();
    let (directory, witness) =
        crate::core::speculation::namespace::plan_metadata_directory(&directory)?;
    witness.verify()?;
    let entries = match fs::read_dir(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("reading repository verification records"),
        Ok(entries) => entries,
    };
    let mut count = 0usize;
    let mut total = 0u64;
    for (index, entry) in entries.enumerate() {
        ensure!(
            index < MAX_RECORDS,
            "repository verification directory limit reached"
        );
        let (bucket, boundary) =
            crate::core::speculation::namespace::plan_metadata_directory(&entry?.path())?;
        boundary.verify()?;
        for entry in fs::read_dir(&bucket)? {
            count += 1;
            ensure!(
                count <= MAX_RECORDS,
                "repository verification scan limit reached"
            );
            let path = entry?.path();
            let (record, bytes) = read_record_contents(&path)?;
            ensure!(
                Some(&record.common_dir) == identity.canonical_common_git_dir.as_ref(),
                "foreign repository verification record"
            );
            let binding = WorkspaceIdentity::from_bound_paths(
                record.root.clone(),
                Some(record.git_dir.clone()),
                Some(record.common_dir.clone()),
            );
            let (expected, expected_boundary) =
                crate::core::speculation::namespace::plan_metadata_directory(
                    &verification_directory(&binding),
                )?;
            expected_boundary.verify()?;
            ensure!(
                path.parent() == Some(expected.as_path()),
                "verification worktree bucket does not match its recorded binding"
            );
            total = total
                .checked_add(bytes.len() as u64)
                .context("repository verification byte count overflow")?;
            ensure!(
                total <= 16 * 1024 * 1024,
                "repository verification byte limit reached"
            );
            require_record_settled(&record)?;
        }
        boundary.verify()?;
    }
    witness.verify()
}

/// Existing records only. Reading never authorizes a retry or a verified claim.
pub(crate) fn verification_records(root: &Path) -> Result<Vec<VerificationRecord>> {
    let identity = inspect_identity(root, None)?;
    let mut records = records(&identity)?;
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records)
}

fn records(identity: &WorkspaceIdentity) -> Result<Vec<VerificationRecord>> {
    let directory = verification_directory(identity);
    let (_, witness) = crate::core::speculation::namespace::plan_metadata_directory(&directory)?;
    witness.verify()?;
    let entries = match fs::read_dir(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading verification records"),
        Ok(entries) => entries,
    };
    let mut records = Vec::new();
    let mut total = 0u64;
    for (index, entry) in entries.enumerate() {
        ensure!(
            index < MAX_RECORDS,
            "verification record scan limit reached"
        );
        let path = entry?.path();
        let (record, bytes) = read_record(identity, &path)?;
        total = total
            .checked_add(bytes.len() as u64)
            .context("verification record byte count overflow")?;
        ensure!(
            total <= 16 * 1024 * 1024,
            "verification record scan byte limit reached"
        );
        records.push(record);
    }
    witness.verify()?;
    Ok(records)
}

fn read_record(identity: &WorkspaceIdentity, path: &Path) -> Result<(VerificationRecord, Vec<u8>)> {
    let (record, bytes) = read_record_contents(path)?;
    ensure!(
        record.root == identity.canonical_root
            && Some(&record.git_dir) == identity.canonical_git_dir.as_ref()
            && Some(&record.common_dir) == identity.canonical_common_git_dir.as_ref(),
        "verification record binding/schema changed"
    );
    Ok((record, bytes))
}

fn read_record_contents(path: &Path) -> Result<(VerificationRecord, Vec<u8>)> {
    let bytes = crate::core::file_placement::read_existing_regular(path, MAX_RECORD_BYTES)?;
    let record: VerificationRecord =
        serde_json::from_slice(&bytes).context("unreadable verification record")?;
    ensure!(
        record.schema == 1
            && valid_operation_id(&record.id)
            && valid_operation_id(&record.integration)
            && path.file_name().and_then(|name| name.to_str())
                == Some(format!("{}.json", record.id).as_str())
            && record.root.is_absolute()
            && record.git_dir.is_absolute()
            && record.common_dir.is_absolute()
            && !record.commands.is_empty()
            && record.commands.len() <= 8
            && record.passed_commands.len() <= record.commands.len()
            && matches!(
                record.state.as_str(),
                "running" | "settled" | "failed" | "unsettled"
            )
            && !(record.verified && record.state != "settled"),
        "verification record binding/schema changed"
    );
    validate_record_outcome(&record)?;
    Ok((record, bytes))
}

fn validate_record_outcome(record: &VerificationRecord) -> Result<()> {
    ensure!(
        record.started_at > 0
            && record
                .finished_at
                .is_none_or(|end| end >= record.started_at)
            && record
                .commands
                .iter()
                .all(|c| !c.trim().is_empty() && c.len() <= 4096)
            && record
                .source_identity
                .as_ref()
                .is_none_or(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "verification record has invalid timestamps/commands/source identity"
    );
    let running = record.state == "running";
    ensure!(
        !running
            || (record.termination == "running"
                && !record.verified
                && record.finished_at.is_none()
                && record.source_identity.is_none()
                && record.passed_commands.is_empty()
                && record.duration_ms == 0),
        "running verification record carries a terminal result"
    );
    ensure!(
        running || record.finished_at.is_some(),
        "terminal verification record has no end time"
    );
    if record.state == "failed" {
        ensure!(
            !record.verified && record.termination == "error",
            "failed verification claims success"
        );
    }
    if record.state == "unsettled" {
        ensure!(
            !record.verified && record.termination == "unsettled",
            "unknown verification claims success"
        );
    }
    if record.state == "settled" {
        let termination: crate::agent::verify_gate::VerifyTermination =
            serde_json::from_value(serde_json::Value::String(record.termination.clone()))?;
        ensure!(
            termination != crate::agent::verify_gate::VerifyTermination::Unsettled,
            "settled verification claims unknown effects"
        );
        ensure!(
            record.verified
                == (termination == crate::agent::verify_gate::VerifyTermination::Passed),
            "verification pass/verified state is inconsistent"
        );
    }
    ensure!(
        !record.verified
            || (record.state == "settled"
                && record.termination == "passed"
                && record.passed_commands == record.commands
                && record.source_identity.is_some()),
        "verified record lacks an exact complete command pass/source identity"
    );
    Ok(())
}
