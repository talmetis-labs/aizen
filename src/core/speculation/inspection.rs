//! Data-only speculative recovery inspection, independent of checkpoint storage.
//!
//! Durable records never reconstruct execution capabilities. A live namespace adapter supplies
//! admission and metadata exclusion; caller-selected resource locks precede that transaction,
//! followed by the cohort owner and native journal. No recorded source path grants authority.

use crate::core::workspace_txn::{PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

// Restart inspection is bounded independently of accumulated stale metadata.
const MAX_SPECULATION_COHORTS: usize = 32;
pub(crate) const MAX_SPECULATION_SCAN_ENTRIES: usize = 4096;
const MAX_SPECULATION_INSPECTION_BYTES: u64 = 64 * 1024 * 1024;

/// A live namespace/lock adapter, not a serialized storage locator or restore capability.
/// The adapter verifies admission before resources are locked and again inside the transaction.
/// Its callback retains the actual namespace transaction lock; legacy adapters additionally acquire
/// their store-shared lock first. Never wait on workers or upgrade locks inside the callback.
/// Namespace layout and registry identity remain adapter-owned.
pub(crate) trait RecoveryNamespace {
    fn identity(&self) -> &WorkspaceIdentity;
    fn namespace_dir(&self) -> &Path;
    fn verify_admission(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<()>;
    fn with_transaction_lock<T>(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T>;
}

/// Route by cohort presence, not by whether one selected record happens to be readable. A broken
/// or partial cohort in either namespace cannot be mistaken for absence in favor of the other.
/// This is a bounded metadata probe under the namespace's actual lock; it grants no authority.
pub(crate) fn contains_recovery_cohort(
    namespace: &impl RecoveryNamespace,
    writer: &WorkspaceWriterLease,
    physical: &PhysicalEffectLease,
    cohort: &str,
) -> Result<bool> {
    let directory = recovery_cohort_directory(namespace.namespace_dir(), cohort)?;
    namespace.with_transaction_lock(writer, physical, || {
        let version = directory
            .parent()
            .context("missing cohort version directory")?;
        // Validate every existing ancestor without suppressing permission/type/link errors.
        let namespaces = checked_metadata_entries(namespace.namespace_dir())?;
        for path in namespaces {
            let name = path.file_name().and_then(|name| name.to_str())
                .context("invalid speculation namespace name")?;
            if name.starts_with("speculation-") && name != "speculation-v1" {
                bail!("unsupported future speculation namespace; cannot rule out ambiguous recovery evidence");
            }
        }
        let entries = checked_metadata_entries(version)?;
        Ok(entries.iter().any(|path| path == &directory))
    })
}

/// Select only validated evidence; each adapter retains its actual transaction lock domain.
/// No recorded source path is resolved, and selection cannot authorize mutation or settlement.
pub(crate) fn select_speculation_recovery(
    namespace: &impl RecoveryNamespace,
    parent: &crate::core::exec_ctx::ExecutionContext,
    writer: &crate::core::workspace_txn::WorkspaceWriterLease,
    physical: &crate::core::workspace_txn::PhysicalEffectLease,
    cohort: &str,
    prepared: &str,
) -> Result<SpeculationRecoverySelection> {
    require_recovery_report_parent(parent)?;
    namespace.with_transaction_lock(writer, physical, || {
        let directory = recovery_cohort_directory(namespace.namespace_dir(), cohort)?;
        let (inventory_sha256, files, native_receipt, limitations) =
            read_recovery_selection(&directory, cohort, prepared)?;
        let envelope: serde_json::Value = serde_json::from_slice(&read_speculation_evidence(
            &directory.join(prepared),
            2 * 1024 * 1024,
        )?)?;
        let owner = evidence_text(&envelope, "owner")?.to_owned();
        validate_recovery_base_identity(&directory, namespace.identity())?;
        if recovery_inventory_digest(&directory)? != inventory_sha256 {
            bail!("restart base evidence changed during identity inspection");
        }
        Ok(SpeculationRecoverySelection {
            identity: writer.identity().clone(),
            namespace: namespace.namespace_dir().to_path_buf(),
            cohort: cohort.to_owned(),
            prepared: prepared.to_owned(),
            inventory_sha256,
            files,
            native_receipt,
            limitations,
            owner,
        })
    })
}

pub(crate) fn inspect_selected_recovery(
    namespace: &impl RecoveryNamespace,
    parent: &crate::core::exec_ctx::ExecutionContext,
    writer: &crate::core::workspace_txn::WorkspaceWriterLease,
    physical: &crate::core::workspace_txn::PhysicalEffectLease,
    selection: &SpeculationRecoverySelection,
    approved: &[crate::core::workspace_txn::NativeResource],
    record_observation: bool,
) -> Result<SpeculationRecoveryReport> {
    require_recovery_report_parent(parent)?;
    if selection.identity != *writer.identity() || selection.namespace != namespace.namespace_dir()
    {
        bail!("restart report selection belongs to another admitted workspace/namespace");
    }
    namespace.verify_admission(writer, physical)?;
    // Sorted exact-resource exclusion precedes the actual TM transaction and cohort journal.
    let resources =
        acquire_recovery_resources(&namespace.identity().canonical_root, selection, approved)?;
    namespace.with_transaction_lock(writer, physical, || {
        let directory = recovery_cohort_directory(namespace.namespace_dir(), &selection.cohort)?;
        // Current store inspection takes a shared owner probe, so inspect before taking our
        // exclusive descriptor. Both are ZERO-time attempts; no shared->exclusive upgrade.
        let (revision, _, _, limitations) =
            read_recovery_selection(&directory, &selection.cohort, &selection.prepared)?;
        validate_recovery_base_identity(&directory, namespace.identity())?;
        if revision != selection.inventory_sha256 || limitations != selection.limitations {
            bail!("restart report selection is stale; evidence retained");
        }
        let inspect = || -> Result<SpeculationRecoveryReport> {
            if recovery_inventory_digest(&directory)? != selection.inventory_sha256 {
                bail!("restart evidence changed before exclusive ownership; evidence retained");
            }
            let native_records = inspect_recovery_native_records(selection, &resources)?;
            let observations = classify_locked_recovery_resources(selection, &resources)?;
            if recovery_inventory_digest(&directory)? != selection.inventory_sha256 {
                bail!("restart evidence changed during classification; evidence retained");
            }
            Ok(SpeculationRecoveryReport {
                cohort: selection.cohort.clone(),
                prepared: selection.prepared.clone(),
                inventory_sha256: selection.inventory_sha256,
                status: RecoveryReportStatus::InspectionOnly,
                files: observations,
                native_records,
                native_coverage: selection
                    .native_receipt
                    .as_ref()
                    .and_then(|receipt| receipt.coverage.clone()),
                observation: None,
                limitations: selection.limitations.clone(),
            })
        };
        if record_observation {
            let mut owner = crate::core::speculation::evidence::ExistingObservationAppend::acquire(
                &directory,
                selection.inventory_sha256,
                writer,
                physical,
            )?;
            let mut report = inspect()?;
            let observation = report.nonterminal_observation(parent, selection)?;
            // No cached/deserialized report becomes fresh authority. All exact resources and
            // metadata admission remain held through this final recheck and immutable append.
            for file in &report.files {
                if recovery_live_fingerprint(resources.resources[file.key.as_str()])?
                    != file.current
                {
                    bail!("restart source drifted before observation publication");
                }
            }
            report.observation = Some(owner.append(&observation)?);
            report.status = RecoveryReportStatus::ObservedNonterminal;
            Ok(report)
        } else {
            with_recovery_owner_claim(&directory, inspect)
        }
    })
}

/// Restart inspection is DATA ONLY. It deliberately cannot recreate a source writer capability, workspace lease,
/// integration plan, or restore authority from a manifest. No source path in these records is opened.
#[derive(Debug, Default)]
pub(crate) struct SpeculationInspection {
    pub(crate) cohorts: usize,
    pub(crate) prepared: usize,
    pub(crate) verified: usize,
    pub(crate) read_dependency_concerns: usize,
}

pub(crate) fn checked_metadata_entries(path: &Path) -> Result<Vec<PathBuf>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading speculation metadata directory"),
        Ok(metadata)
            if !metadata.is_dir() || crate::core::file_placement::is_link_or_reparse(&metadata) =>
        {
            bail!("speculation metadata requires non-link directories");
        }
        Ok(_) => {}
    }
    let boundary =
        crate::core::workspace_txn::NativeResource::resolve(&path.join(".tm-inspection-boundary"))?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        if entries.len() >= MAX_SPECULATION_SCAN_ENTRIES {
            bail!("speculation metadata traversal exceeds {MAX_SPECULATION_SCAN_ENTRIES} entries; evidence retained");
        }
        entries.push(entry?.path());
    }
    boundary.verify()?;
    Ok(entries)
}

pub(crate) fn read_speculation_evidence(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::core::file_placement::read_existing_regular(path, limit)
}

fn evidence_text<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("speculation record has invalid {field}"))
}

fn evidence_digest(value: &serde_json::Value) -> bool {
    value.as_array().is_some_and(|bytes| {
        bytes.len() == 32
            && bytes
                .iter()
                .all(|byte| byte.as_u64().is_some_and(|byte| byte <= 255))
    })
}

fn evidence_fingerprint(value: &serde_json::Value) -> bool {
    value
        .get("exists")
        .and_then(serde_json::Value::as_bool)
        .is_some()
        && value
            .get("byte_len")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|len| len <= crate::core::speculation::evidence::MAX_FILE_BYTES as u64)
        && value.get("sha256").is_some_and(evidence_digest)
}

fn parsed_evidence_fingerprint(
    value: &serde_json::Value,
) -> Result<crate::core::persist::FileFingerprint> {
    if !evidence_fingerprint(value) {
        bail!("invalid integration fingerprint evidence");
    }
    let exists = value["exists"]
        .as_bool()
        .context("invalid integration existence evidence")?;
    let byte_len = value["byte_len"]
        .as_u64()
        .context("invalid integration image length")?;
    let mut sha256 = [0u8; 32];
    for (index, byte) in value["sha256"]
        .as_array()
        .context("invalid integration digest")?
        .iter()
        .enumerate()
    {
        sha256[index] = byte.as_u64().context("invalid integration digest byte")? as u8;
    }
    let fingerprint = crate::core::persist::FileFingerprint {
        exists,
        byte_len,
        sha256,
    };
    if !exists && fingerprint != crate::core::persist::FileFingerprint::missing() {
        bail!("missing integration image has nonempty fingerprint");
    }
    Ok(fingerprint)
}

fn evidence_array<'a>(
    value: &'a serde_json::Value,
    field: &str,
    bound: usize,
) -> Result<&'a [serde_json::Value]> {
    let rows = value
        .get(field)
        .and_then(serde_json::Value::as_array)
        .with_context(|| format!("integration {field} evidence missing"))?;
    if rows.len() > bound {
        bail!("integration {field} evidence exceeds bound");
    }
    Ok(rows)
}

fn evidence_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        bail!("invalid bounded integration name/path evidence");
    }
    Ok(())
}

fn validate_prepared_image(
    descriptor: &serde_json::Value,
    fingerprint: &crate::core::persist::FileFingerprint,
    images: &[Vec<u8>],
    next_image: &mut usize,
) -> Result<()> {
    if parsed_evidence_fingerprint(descriptor)? != *fingerprint
        || descriptor
            .get("missing")
            .and_then(serde_json::Value::as_bool)
            != Some(!fingerprint.exists)
    {
        bail!("prepared image descriptor differs from its file fingerprint");
    }
    if !fingerprint.exists {
        if descriptor.get("index") != Some(&serde_json::Value::Null) {
            bail!("absent prepared preimage must not reference an image");
        }
        return Ok(());
    }
    if descriptor.get("index").and_then(serde_json::Value::as_u64) != Some(*next_image as u64) {
        bail!("prepared image index is missing, reused or out of deterministic order");
    }
    let image = images
        .get(*next_image)
        .context("prepared image bytes are missing")?;
    if crate::core::persist::FileFingerprint::for_bytes(image) != *fingerprint {
        bail!("prepared image bytes/hash differ from declared exact image");
    }
    *next_image += 1;
    Ok(())
}

pub(crate) fn validate_recovery_report_request(
    cohort: &str,
    prepared: &str,
    resource_paths: &[String],
) -> Result<()> {
    if cohort.len() != 32 || !cohort.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("restart report needs an exact bounded cohort identifier");
    }
    let digest = prepared
        .strip_prefix("m-")
        .and_then(|name| name.strip_suffix(".json"))
        .context("restart report needs an exact immutable prepared-record identifier")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("restart report needs an exact immutable prepared-record identifier");
    }
    if resource_paths.len() > crate::core::speculation::evidence::MAX_COHORT_FILES {
        bail!("restart report resource selection exceeds cohort capacity");
    }
    for path in resource_paths {
        crate::core::workspace_txn::NativeResource::validate_spelling(Path::new(path))?;
    }
    Ok(())
}

pub(crate) fn resolve_recovery_report_resources(
    root: &Path,
    resource_paths: &[String],
) -> Result<Vec<crate::core::workspace_txn::NativeResource>> {
    use crate::core::workspace_txn::NativeResource;
    let mut approved = Vec::with_capacity(resource_paths.len());
    let mut keys = std::collections::BTreeSet::new();
    for supplied in resource_paths {
        let supplied = Path::new(supplied);
        // Check original spelling before joining a Windows verbatim root can erase traversal.
        NativeResource::validate_spelling(supplied)?;
        let path = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            root.join(supplied)
        };
        let resource = NativeResource::resolve(&path)?;
        let relative = resource
            .path()
            .strip_prefix(root)
            .context("restart selected resource is outside workspace")?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                component.as_os_str().to_str().is_none_or(|part| {
                    [".git", ".aizen", "native-journal-v1", "speculation-v1"]
                        .iter()
                        .any(|internal| part.eq_ignore_ascii_case(internal))
                })
            })
            || !keys.insert(resource.key().to_owned())
        {
            bail!("restart report refuses internal or duplicate resource selection");
        }
        approved.push(resource);
    }
    Ok(approved)
}

/// A sealed DATA-ONLY selection of exact immutable evidence. It is intentionally neither
/// deserializable nor an append/source capability; admission and inventory are checked again.
#[derive(Debug, Clone)]
pub(crate) struct SpeculationRecoverySelection {
    identity: crate::core::workspace_txn::WorkspaceIdentity,
    namespace: PathBuf,
    cohort: String,
    prepared: String,
    inventory_sha256: [u8; 32],
    files: Vec<PreparedFileEvidence>,
    native_receipt: Option<RecoveryNativeReceipt>,
    limitations: Vec<crate::core::speculation::evidence::HistoricalLimitation>,
    owner: String,
}

#[derive(Debug, Clone)]
struct RecoveryNativeReceipt {
    id: String,
    owner: String,
    coverage: Option<crate::core::native_journal::BatchIdentity>,
    records: Vec<RecoveryNativeRecord>,
}

#[derive(Debug, Clone)]
struct RecoveryNativeRecord {
    file: usize,
    id: u64,
    applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryImageClass {
    Pre,
    Post,
    SamePreAndPost,
    Foreign,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveryFileObservation {
    pub(crate) key: String,
    pub(crate) class: RecoveryImageClass,
    pub(crate) current: crate::core::persist::FileFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryReportStatus {
    InspectionOnly,
    ObservedNonterminal,
}

/// Data-only restart report. An optional explicit observation records inspection only, never
/// source/overlay authority, terminal settlement, accepted verification or permission to replay.
#[derive(Debug)]
pub(crate) struct SpeculationRecoveryReport {
    pub(crate) cohort: String,
    pub(crate) prepared: String,
    pub(crate) inventory_sha256: [u8; 32],
    pub(crate) status: RecoveryReportStatus,
    /// Exact evidence fingerprints at this inspection, never effect attribution or undo authority.
    pub(crate) files: Vec<RecoveryFileObservation>,
    /// Historical exact journal evidence only; never a live-tip, no-effect, or undo capability.
    pub(crate) native_records: Vec<crate::core::native_journal::RecordProof>,
    /// Present only after exact complete historical batch inspection, never physical-worker closure.
    pub(crate) native_coverage: Option<crate::core::native_journal::BatchIdentity>,
    pub(crate) observation: Option<crate::core::speculation::evidence::ObservationPublication>,
    pub(crate) limitations: Vec<crate::core::speculation::evidence::HistoricalLimitation>,
}

impl SpeculationRecoveryReport {
    fn nonterminal_observation(
        &self,
        parent: &crate::core::exec_ctx::ExecutionContext,
        selection: &SpeculationRecoverySelection,
    ) -> Result<crate::core::speculation::recovery::InspectionObservation> {
        use crate::core::speculation::recovery::{
            InspectionObservation, ObservationState, ObservedFile, ObservedImageClass,
            ObservedNativeRecord,
        };
        let owner = selection
            .native_receipt
            .as_ref()
            .map(|receipt| receipt.owner.clone())
            .unwrap_or_else(|| selection.owner.clone());
        let files = self
            .files
            .iter()
            .map(|file| {
                let class = match file.class {
                    RecoveryImageClass::Pre => ObservedImageClass::Pre,
                    RecoveryImageClass::Post => ObservedImageClass::Post,
                    RecoveryImageClass::SamePreAndPost => ObservedImageClass::SamePreAndPost,
                    RecoveryImageClass::Foreign => {
                        bail!("foreign image cannot become an inspection observation")
                    }
                };
                Ok(ObservedFile {
                    key: file.key.clone(),
                    class,
                    fingerprint: (&file.current).into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(InspectionObservation {
            schema: 1,
            base: self.cohort.clone(),
            prepared: self.prepared.clone(),
            owner,
            receipt: selection
                .native_receipt
                .as_ref()
                .map(|receipt| receipt.id.clone()),
            observer: parent.resource_scope(),
            state: ObservationState::ObservedNonterminal,
            inventory_sha256: self.inventory_sha256,
            files,
            native_records: self
                .native_records
                .iter()
                .map(|proof| ObservedNativeRecord {
                    id: proof.id,
                    status: proof.status.into(),
                })
                .collect(),
        })
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        use crate::core::native_journal::RecordProofStatus;
        let status = match self.status {
            RecoveryReportStatus::InspectionOnly => "inspection_only",
            RecoveryReportStatus::ObservedNonterminal => "observed_nonterminal",
        };
        let publication = self.observation.as_ref().map(|publication| serde_json::json!({
            "record":publication.record,"before_inventory_sha256":publication.before,"after_inventory_sha256":publication.after,
        }));
        let notice = if publication.is_some() {
            "A nonterminal inspection observation was appended. Pending guard unchanged. No replay, settlement, rollback or undo is authorized. Historical native proof is not current-tip or semantic verification authority."
        } else {
            "Evidence retained. No replay, append, settlement, rollback or undo is authorized. Missing native proof is not proof of no effect; postimage matches do not prove attribution or current journal tip. Existing evidence and lock files are never initialized by this inspection."
        };
        let files: Vec<_> = self.files.iter().map(|file| {
            let class = match file.class {
                RecoveryImageClass::Pre => "pre", RecoveryImageClass::Post => "post",
                RecoveryImageClass::SamePreAndPost => "same_pre_and_post", RecoveryImageClass::Foreign => "foreign",
            };
            serde_json::json!({"key":file.key,"class":class,"fingerprint":{
                "exists":file.current.exists,"byte_len":file.current.byte_len,"sha256":file.current.sha256,
            }})
        }).collect();
        let native_records: Vec<_> = self
            .native_records
            .iter()
            .map(|record| {
                let status = match record.status {
                    RecordProofStatus::Prepared => "prepared",
                    RecordProofStatus::Applied => "applied",
                    RecordProofStatus::SettledApplied => "settled_applied",
                    RecordProofStatus::SettledNotApplied => "settled_not_applied",
                };
                serde_json::json!({"id":record.id,"status":status})
            })
            .collect();
        serde_json::json!({
            "schema":1,"cohort":self.cohort,"prepared":self.prepared,"inventory_sha256":self.inventory_sha256,
            "status":status,"inspection_only":publication.is_none(),"pending_guard_unchanged":true,
            "observation":publication,"notice":notice,"legacy_limitations":self.limitations,
            "files":files,"native_records":native_records,"native_coverage":self.native_coverage,
            "native_proof_scope":if self.native_coverage.is_some() { "complete_historical_batch" } else { "selected_records_only_or_missing" },
        })
    }

    pub(crate) fn to_text(&self) -> String {
        let value = self.to_json();
        let mut out = format!(
            "Speculation recovery inspection: {}\ncohort {}\nprepared {}\n",
            value["status"].as_str().unwrap(),
            self.cohort,
            self.prepared
        );
        for file in value["files"].as_array().unwrap() {
            out.push_str(&format!(
                "  resource {}: {}\n",
                file["key"].as_str().unwrap(),
                file["class"].as_str().unwrap()
            ));
        }
        for record in value["native_records"].as_array().unwrap() {
            out.push_str(&format!(
                "  historical native record {}: {}\n",
                record["id"],
                record["status"].as_str().unwrap()
            ));
        }
        if !self.limitations.is_empty() {
            out.push_str(&format!(
                "  legacy evidence unresolved: {:?}; no merger replay or settlement authority\n",
                self.limitations
            ));
        }
        if self.native_coverage.is_some() {
            out.push_str("  complete historical native batch checked; not physical-worker closure or recovery authority\n");
        } else {
            out.push_str("  native evidence is selected-record-only or missing; complete coverage unproven\n");
        }
        if let Some(publication) = &self.observation {
            out.push_str(&format!(
                "  nonterminal observation {}\n",
                publication.record
            ));
            out.push_str("Inspection recorded; pending guard unchanged. ");
        } else {
            out.push_str("Inspection only; pending guard unchanged. ");
        }
        out.push_str(value["notice"].as_str().unwrap());
        out
    }
}

#[derive(Debug, Clone)]
struct PreparedFileEvidence {
    key: String,
    path: String,
    preimage: crate::core::persist::FileFingerprint,
    postimage: crate::core::persist::FileFingerprint,
}

struct PreparedEvidence {
    keys: Vec<String>,
    paths: Vec<String>,
    files: Vec<PreparedFileEvidence>,
    native_coverage: Option<crate::core::native_journal::BatchIdentity>,
    read_dependencies: Vec<serde_json::Value>,
}

/// Validate evidence relationships without resolving any recorded source path. Peer-read findings
/// are preserved as concerns, never interpreted as semantic reconciliation by a build/test verdict.
fn validate_prepared_evidence(
    base: &str,
    owner: &str,
    value: &serde_json::Value,
    images: &[Vec<u8>],
) -> Result<PreparedEvidence> {
    use std::collections::{BTreeMap, BTreeSet};
    let sealed = crate::core::speculation::evidence::sealed_lineage(value)?;
    let generations: BTreeMap<_, _> = sealed
        .iter()
        .flatten()
        .map(|identity| {
            (
                format!("{}/{}", identity.owner, identity.overlay),
                identity.generation,
            )
        })
        .collect();
    let overlays = evidence_array(value, "overlays", 32)?;
    if overlays.is_empty() {
        bail!("prepared integration has no overlay lineage");
    }
    let mut authors = BTreeSet::new();
    let mut overlay_ids = BTreeSet::new();
    for overlay in overlays {
        let overlay_owner = evidence_text(overlay, "owner")?;
        let id = evidence_text(overlay, "id")?;
        let author = evidence_text(overlay, "author")?;
        evidence_name(overlay_owner)?;
        evidence_name(id)?;
        if evidence_text(overlay, "base")? != base
            || !(overlay_owner == owner
                || overlay_owner
                    .strip_prefix(owner)
                    .is_some_and(|tail| tail.starts_with('/')))
            || author != format!("{overlay_owner}/{id}")
            || !overlay_ids.insert(id)
            || !authors.insert(author)
        {
            bail!("invalid or ambiguous prepared overlay lineage");
        }
    }
    let files = evidence_array(
        value,
        "files",
        crate::core::speculation::evidence::MAX_COHORT_FILES,
    )?;
    if images.len() > files.len() * 2 {
        bail!("prepared integration has unreferenced images");
    }
    let mut next_image = 0usize;
    let mut keys = Vec::new();
    let mut applied_paths = Vec::new();
    let mut file_images = Vec::new();
    let mut paths = BTreeMap::new();
    let mut expected_images = BTreeMap::new();
    let mut changes: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for file in files {
        let key = evidence_text(file, "key")?;
        let path = evidence_text(file, "path")?;
        evidence_name(key)?;
        evidence_name(path)?;
        if keys.iter().any(|previous| previous == key) {
            bail!("duplicate prepared file key");
        }
        keys.push(key.to_owned());
        applied_paths.push(path.to_owned());
        paths.insert(key, path);
        let expected = parsed_evidence_fingerprint(
            file.get("expected")
                .context("prepared preimage fingerprint missing")?,
        )?;
        let after = parsed_evidence_fingerprint(
            file.get("after")
                .context("prepared postimage fingerprint missing")?,
        )?;
        if !after.exists {
            bail!("prepared postimage cannot be absent");
        }
        validate_prepared_image(
            file.get("preimage")
                .context("prepared preimage mapping missing")?,
            &expected,
            images,
            &mut next_image,
        )?;
        validate_prepared_image(
            file.get("postimage")
                .context("prepared postimage mapping missing")?,
            &after,
            images,
            &mut next_image,
        )?;
        file_images.push(PreparedFileEvidence {
            key: key.to_owned(),
            path: path.to_owned(),
            preimage: expected.clone(),
            postimage: after,
        });
        expected_images.insert(key, expected);
        let mut changed_authors = BTreeSet::new();
        let mut rewrite = false;
        for change in evidence_array(file, "lineage", 32)? {
            let author = evidence_text(change, "author")?;
            let operation = evidence_text(change, "operation")?;
            if !authors.contains(author)
                || !changed_authors.insert(author)
                || change
                    .get("generation")
                    .and_then(serde_json::Value::as_u64)
                    .is_none_or(|generation| {
                        generation == 0
                            || generations
                                .get(author)
                                .is_some_and(|maximum| generation > *maximum)
                    })
                || !matches!(operation, "localized" | "rewrite")
            {
                bail!("invalid prepared change lineage");
            }
            rewrite |= operation == "rewrite";
        }
        if changed_authors.is_empty()
            || evidence_text(file, "operation")? != if rewrite { "rewrite" } else { "localized" }
        {
            bail!("prepared operation disagrees with lineage");
        }
        let mut contributors = BTreeSet::new();
        for author in evidence_array(file, "contributors", 32)? {
            let author = author.as_str().context("invalid prepared contributor")?;
            if !changed_authors.contains(author) || !contributors.insert(author) {
                bail!("prepared contributor is missing from exact lineage or duplicated");
            }
        }
        changes.insert(key, changed_authors);
    }
    if next_image != images.len() {
        bail!("prepared integration has unused image bytes");
    }
    let mut expected_dependencies: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    let mut observed_versions = BTreeSet::new();
    let mut base_observers = BTreeSet::new();
    let mut base_fingerprints = BTreeMap::new();
    for observation in evidence_array(value, "observations", 32 * 512)? {
        let author = evidence_text(observation, "author")?;
        let key = evidence_text(observation, "key")?;
        let path = evidence_text(observation, "path")?;
        let generation = observation
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .context("prepared read observation generation missing")?;
        evidence_name(key)?;
        evidence_name(path)?;
        if !authors.contains(author)
            || !observed_versions.insert((author, key, generation))
            || generations
                .get(author)
                .is_some_and(|maximum| generation > *maximum)
            || paths.get(key).is_some_and(|previous| *previous != path)
        {
            bail!("invalid or duplicate prepared read observation evidence");
        }
        paths.insert(key, path);
        let fingerprint = parsed_evidence_fingerprint(
            observation
                .get("fingerprint")
                .context("prepared observation fingerprint missing")?,
        )?;
        if generation == 0 {
            if expected_images
                .get(key)
                .is_some_and(|expected| *expected != fingerprint)
                || base_fingerprints
                    .get(key)
                    .is_some_and(|expected| *expected != fingerprint)
            {
                bail!("prepared base read differs from exact preimage evidence");
            }
            base_fingerprints.insert(key, fingerprint);
            base_observers.insert((author, key));
        }
        if let Some(peers) = changes.get(key) {
            for peer in peers {
                if *peer != author {
                    expected_dependencies
                        .entry((author, key))
                        .or_default()
                        .insert(peer);
                }
            }
        }
    }
    for (key, changed_authors) in &changes {
        for author in changed_authors {
            if !base_observers.contains(&(*author, *key)) {
                bail!("prepared change is missing its exact base read observation");
            }
        }
    }
    let read_dependencies = evidence_array(
        value,
        "read_dependencies",
        32 * crate::core::speculation::evidence::MAX_COHORT_FILES,
    )?;
    let mut declared = BTreeMap::new();
    for dependency in read_dependencies {
        let observer = evidence_text(dependency, "observer")?;
        let key = evidence_text(dependency, "key")?;
        let mut peers = BTreeSet::new();
        for peer in evidence_array(dependency, "peer_authors", 32)? {
            let peer = peer
                .as_str()
                .context("invalid integration peer-read author")?;
            if peer == observer || !authors.contains(peer) || !peers.insert(peer) {
                bail!("invalid integration peer-read dependency");
            }
        }
        if !authors.contains(observer)
            || peers.is_empty()
            || declared.insert((observer, key), peers).is_some()
        {
            bail!("invalid or duplicate integration read dependency");
        }
    }
    if declared != expected_dependencies {
        bail!("prepared read dependencies disagree with observations/change lineage");
    }
    let native_coverage = evidence_native_coverage(value)?;
    Ok(PreparedEvidence {
        keys,
        paths: applied_paths,
        files: file_images,
        native_coverage,
        read_dependencies: read_dependencies.to_vec(),
    })
}

pub(crate) fn require_recovery_report_parent(
    parent: &crate::core::exec_ctx::ExecutionContext,
) -> Result<()> {
    let current = crate::core::exec_ctx::current()
        .context("restart report requires the current plain parent")?;
    if !parent.can_restore_workspace()
        || parent.native_scope().is_some()
        || !current.can_restore_workspace()
        || current.native_scope().is_some()
        || current.resource_scope() != parent.resource_scope()
        || parent.resource_scope().is_empty()
    {
        bail!(
            "restart report requires fresh coordinating-parent admission, not delegated authority"
        );
    }
    Ok(())
}

fn recovery_cohort_directory(namespace: &Path, cohort: &str) -> Result<PathBuf> {
    if cohort.len() != 32 || !cohort.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("restart report needs an exact bounded cohort identifier");
    }
    Ok(namespace.join("speculation-v1").join(cohort))
}

/// Hash every immutable metadata object, including unreferenced objects, in a bounded sorted
/// inventory. The owner marker is represented by its validated canonical bytes: callers must check
/// its actual bytes under exclusive ownership before relying on a revision during classification.
/// This revision is only a stale-selection check, never authority derived from a hash.
fn recovery_inventory_digest(directory: &Path) -> Result<[u8; 32]> {
    crate::core::speculation::evidence::inventory_revision(directory)
}

fn validate_recovery_base_identity(
    directory: &Path,
    identity: &crate::core::workspace_txn::WorkspaceIdentity,
) -> Result<()> {
    for path in checked_metadata_entries(directory)? {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("m-"))
        {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_slice(&read_speculation_evidence(&path, 2 * 1024 * 1024)?)?;
        if evidence_text(&value, "kind")? != "base" {
            continue;
        }
        if evidence_text(&value, "repo")? != identity.repo_id
            || evidence_text(&value, "worktree")? != identity.worktree_id
            || Some(evidence_text(&value, "root")?) != identity.canonical_root.to_str()
        {
            bail!("restart base evidence does not match fresh workspace identity");
        }
    }
    Ok(())
}

fn validate_inspection_observation(
    base: &str,
    record: &crate::core::speculation::evidence::InspectedRecord,
    records: &[crate::core::speculation::evidence::InspectedRecord],
) -> Result<()> {
    use crate::core::speculation::recovery::{InspectionObservation, ObservedNativeStatus};
    let observation: InspectionObservation = serde_json::from_slice(&record.metadata)?;
    observation.validate()?;
    if observation.base != base || observation.observer != record.owner || !record.images.is_empty()
    {
        bail!("inspection observation identity/images mismatch");
    }
    let mut candidates = records.iter().filter(|record| {
        record.id == observation.prepared && record.label == "integration-prepared"
    });
    let prepared = candidates
        .next()
        .context("inspection observation prepared record missing")?;
    if candidates.next().is_some() || prepared.owner != observation.owner {
        bail!("inspection observation prepared owner ambiguous");
    }
    let value: serde_json::Value = serde_json::from_slice(&prepared.metadata)?;
    let evidence = validate_prepared_evidence(base, &prepared.owner, &value, &prepared.images)?;
    if evidence.files.len() != observation.files.len() {
        bail!("inspection observation file count mismatch");
    }
    let mut pre = false;
    let mut post = false;
    for (file, expected) in observation.files.iter().zip(&evidence.files) {
        let fingerprint = file.fingerprint.fingerprint();
        let class = recovery_image_class(&fingerprint, &expected.preimage, &expected.postimage);
        let recorded = match file.class {
            crate::core::speculation::recovery::ObservedImageClass::Pre => RecoveryImageClass::Pre,
            crate::core::speculation::recovery::ObservedImageClass::Post => {
                RecoveryImageClass::Post
            }
            crate::core::speculation::recovery::ObservedImageClass::SamePreAndPost => {
                RecoveryImageClass::SamePreAndPost
            }
        };
        if file.key != expected.key || class != recorded {
            bail!("inspection observation fingerprint/class mismatch");
        }
        pre |= class == RecoveryImageClass::Pre;
        post |= class == RecoveryImageClass::Post;
    }
    if pre && post {
        bail!("inspection observation cannot certify mixed pre/post images");
    }
    let mut receipt = records.iter().filter_map(|record| {
        if record.label != "integration-receipt"
            || observation.receipt.as_deref() != Some(record.id.as_str())
        {
            return None;
        }
        let value: serde_json::Value = match serde_json::from_slice(&record.metadata) {
            Ok(value) => value,
            Err(error) => return Some(Err(anyhow::Error::from(error))),
        };
        (value.get("prepared").and_then(serde_json::Value::as_str)
            == Some(observation.prepared.as_str()))
        .then_some(Ok((record, value)))
    });
    if let Some(entry) = receipt.next() {
        let (record, value) = entry?;
        if receipt.next().is_some() || record.owner != observation.owner {
            bail!("inspection observation receipt ambiguous");
        }
        validate_receipt_evidence(&value, &evidence)?;
        let files = evidence_array(
            &value,
            "files",
            crate::core::speculation::evidence::MAX_COHORT_FILES,
        )?;
        let mut proofs = observation.native_records.iter();
        for file in files {
            if let Some(id) = file
                .get("native_record_id")
                .and_then(serde_json::Value::as_u64)
            {
                let proof = proofs
                    .next()
                    .context("inspection observation exact native proof missing")?;
                if proof.id != id
                    || (evidence_text(file, "state")? == "applied"
                        && proof.status != ObservedNativeStatus::SettledApplied)
                {
                    bail!("inspection observation native proof mismatch");
                }
            }
        }
        if proofs.next().is_some() {
            bail!("inspection observation has unlinked native proof");
        }
    } else if observation.receipt.is_some() || !observation.native_records.is_empty() {
        bail!("inspection observation lacks exact receipt linkage");
    }
    Ok(())
}

type RecoverySelectionEvidence = (
    [u8; 32],
    Vec<PreparedFileEvidence>,
    Option<RecoveryNativeReceipt>,
    Vec<crate::core::speculation::evidence::HistoricalLimitation>,
);

fn read_recovery_selection(
    directory: &Path,
    cohort: &str,
    prepared_id: &str,
) -> Result<RecoverySelectionEvidence> {
    let before = recovery_inventory_digest(directory)?;
    if read_speculation_evidence(&directory.join("owner.lock"), 1024)?
        != b"aizen-speculation-owner-v1\n"
    {
        bail!("unsupported restart owner marker");
    }
    let inspected =
        crate::core::speculation::evidence::ExistingEvidenceReader::open(directory)?.inspect()?;
    let limitations = inspected.limitations;
    let records = inspected.records;
    crate::core::speculation::verification::validate_verification_records(cohort, &records)?;
    crate::core::speculation::workers::validate_worker_records(cohort, &records)?;
    let mut bases = 0usize;
    for path in checked_metadata_entries(directory)? {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("m-"))
        {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_slice(&read_speculation_evidence(&path, 2 * 1024 * 1024)?)?;
        if evidence_text(&value, "format")? != "aizen-speculation-v1"
            || evidence_text(&value, "base")? != cohort
        {
            bail!("unsupported or foreign restart manifest");
        }
        match evidence_text(&value, "kind")? {
            "base" => bases += 1,
            "overlay" | "controller-record" => {}
            _ => bail!("unknown restart manifest kind"),
        }
    }
    if bases != 1 {
        bail!("restart report requires one exact base manifest");
    }
    let prepared = records
        .iter()
        .find(|record| record.id == prepared_id && record.label == "integration-prepared")
        .context("selected immutable prepared record is missing")?;
    let prepared_value: serde_json::Value = serde_json::from_slice(&prepared.metadata)?;
    let evidence =
        validate_prepared_evidence(cohort, &prepared.owner, &prepared_value, &prepared.images)?;
    let workers = crate::core::speculation::workers::validate_worker_records(cohort, &records)?;
    if workers.roster.is_some()
        && !workers.unresolved()
        && crate::core::speculation::evidence::sealed_lineage(&prepared_value)?.is_some()
    {
        workers.check_selected_lineage(&prepared_value)?;
    }
    let mut prepared_count = 0;
    let mut receipt_id = None;
    let mut native_receipt = None;
    let mut verification = None;
    for record in &records {
        if crate::core::speculation::workers::is_worker_label(&record.label) {
            // Complete inventory linkage was validated above. Physical evidence is historical;
            // neither an unclosed intent nor a closure authorizes source replay or ownership.
            continue;
        }
        if record.label == crate::core::speculation::recovery::OBSERVATION_LABEL {
            validate_inspection_observation(cohort, record, &records)?;
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&record.metadata)?;
        if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
            || evidence_text(&value, "base")? != cohort
            || evidence_text(&value, "owner")? != prepared.owner
            || record.owner != prepared.owner
        {
            bail!("foreign or unsupported restart record identity");
        }
        match record.label.as_str() {
            "integration-prepared" => {
                prepared_count += 1;
                if record.id != prepared_id || evidence_text(&value, "state")? != "prepared" {
                    bail!("ambiguous restart prepared evidence");
                }
            }
            "integration-receipt" => {
                if evidence_text(&value, "prepared")? != prepared_id
                    || receipt_id.replace(record.id.as_str()).is_some()
                    || !matches!(
                        evidence_text(&value, "state")?,
                        "partial" | "applied-unverified"
                    )
                    || !record.images.is_empty()
                {
                    bail!("ambiguous or unknown restart receipt");
                }
                validate_receipt_evidence(&value, &evidence)?;
                let applied = value
                    .get("applied")
                    .and_then(serde_json::Value::as_u64)
                    .context("restart receipt application count missing")?;
                let files = evidence_array(
                    &value,
                    "files",
                    crate::core::speculation::evidence::MAX_COHORT_FILES,
                )?;
                if value.get("total").and_then(serde_json::Value::as_u64)
                    != Some(evidence.files.len() as u64)
                    || applied > evidence.files.len() as u64
                    || files.len() != evidence.files.len()
                {
                    bail!("restart receipt file count differs from prepared evidence");
                }
                let mut exact_records = Vec::new();
                for (index, (file, prepared)) in files.iter().zip(&evidence.files).enumerate() {
                    if evidence_text(file, "key")? != prepared.key
                        || !matches!(
                            evidence_text(file, "state")?,
                            "applied" | "possibly-applied" | "not-applied"
                        )
                        || file
                            .get("postimage_checked")
                            .and_then(serde_json::Value::as_bool)
                            .is_none()
                    {
                        bail!("restart receipt file evidence is unknown or mismatched");
                    }
                    if let Some(id) = file
                        .get("native_record_id")
                        .and_then(serde_json::Value::as_u64)
                    {
                        exact_records.push(RecoveryNativeRecord {
                            file: index,
                            id,
                            applied: evidence_text(file, "state")? == "applied",
                        });
                    }
                }
                native_receipt = Some(RecoveryNativeReceipt {
                    id: record.id.clone(),
                    owner: prepared.owner.clone(),
                    coverage: evidence.native_coverage.clone(),
                    records: exact_records,
                });
            }
            "integration-verification-started" => {
                if evidence_text(&value, "prepared")? != prepared_id {
                    bail!("restart verification start belongs to another prepared record");
                }
                // Strict lifecycle linkage was checked across the complete, unordered inventory.
            }
            "integration-verification" => {
                if evidence_text(&value, "prepared")? != prepared_id
                    || verification.replace(value).is_some()
                    || !record.images.is_empty()
                {
                    bail!("ambiguous restart verification");
                }
            }
            _ => bail!("unsupported restart controller record; no recovery action is authorized"),
        }
    }
    if prepared_count != 1 {
        bail!("ambiguous restart prepared selection");
    }
    if let Some(value) = verification {
        if Some(evidence_text(&value, "integration")?) != receipt_id
            || !matches!(
                evidence_text(&value, "state")?,
                "verified" | "stale" | "verification_failed" | "unverified"
            )
        {
            bail!("orphan or unknown restart verification");
        }
    }
    let after = recovery_inventory_digest(directory)?;
    if before != after {
        bail!("restart evidence changed during selection");
    }
    Ok((after, evidence.files, native_receipt, limitations))
}

fn with_recovery_owner_claim<T>(directory: &Path, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock = directory.join("owner.lock");
    let resource = crate::core::workspace_txn::NativeResource::resolve(&lock)?;
    let stamp = crate::core::file_placement::NodeStamp::capture(
        &lock,
        &fs::symlink_metadata(&lock)?,
        false,
    )?;
    let owner =
        crate::core::repo_lock::RepoTxnLock::acquire_existing_exclusive(&lock, Duration::ZERO)?;
    owner.verify_existing_path(&lock)?;
    if owner.read_bounded_exclusive(1024)? != b"aizen-speculation-owner-v1\n" {
        bail!("unsupported restart owner marker under exclusive ownership");
    }
    resource.verify()?;
    if crate::core::file_placement::NodeStamp::capture(&lock, &fs::symlink_metadata(&lock)?, false)?
        != stamp
    {
        bail!("restart owner lock identity changed");
    }
    let result = body()?;
    owner.verify_existing_path(&lock)?;
    if owner.read_bounded_exclusive(1024)? != b"aizen-speculation-owner-v1\n" {
        bail!("restart owner marker changed during report");
    }
    resource.verify()?;
    if crate::core::file_placement::NodeStamp::capture(&lock, &fs::symlink_metadata(&lock)?, false)?
        != stamp
    {
        bail!("restart owner lock changed during report");
    }
    Ok(result)
}

fn recovery_live_fingerprint(
    resource: &crate::core::workspace_txn::NativeResource,
) -> Result<crate::core::persist::FileFingerprint> {
    resource.verify()?;
    match fs::symlink_metadata(resource.path()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            resource.verify()?;
            if !matches!(fs::symlink_metadata(resource.path()), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                bail!("restart source changed during missing-file inspection");
            }
            Ok(crate::core::persist::FileFingerprint::missing())
        }
        Err(error) => Err(error).context("restart source metadata unreadable"),
        Ok(_) => {
            let bytes = read_speculation_evidence(
                resource.path(),
                crate::core::speculation::evidence::MAX_FILE_BYTES as u64,
            )?;
            resource.verify()?;
            Ok(crate::core::persist::FileFingerprint::for_bytes(&bytes))
        }
    }
}

fn recovery_image_class(
    actual: &crate::core::persist::FileFingerprint,
    before: &crate::core::persist::FileFingerprint,
    after: &crate::core::persist::FileFingerprint,
) -> RecoveryImageClass {
    match (actual == before, actual == after) {
        (true, true) => RecoveryImageClass::SamePreAndPost,
        (true, false) => RecoveryImageClass::Pre,
        (false, true) => RecoveryImageClass::Post,
        (false, false) => RecoveryImageClass::Foreign,
    }
}

struct LockedRecoveryResources<'a> {
    resources: std::collections::BTreeMap<&'a str, &'a crate::core::workspace_txn::NativeResource>,
    _locks: crate::core::workspace_txn::LockSet,
}

fn acquire_recovery_resources<'a>(
    root: &Path,
    selection: &SpeculationRecoverySelection,
    approved: &'a [crate::core::workspace_txn::NativeResource],
) -> Result<LockedRecoveryResources<'a>> {
    use crate::core::workspace_txn::{LockClass, LockOwner, LockRequest, LockSet};
    if approved.len() != selection.files.len() {
        bail!("restart report requires exact fresh resource selection");
    }
    let mut resources = std::collections::BTreeMap::new();
    for resource in approved {
        resource.verify()?;
        let relative = resource
            .path()
            .strip_prefix(root)
            .context("restart approved resource is outside workspace")?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                component.as_os_str().to_str().is_none_or(|part| {
                    [".git", ".aizen", "native-journal-v1", "speculation-v1"]
                        .iter()
                        .any(|internal| part.eq_ignore_ascii_case(internal))
                })
            })
            || resources.insert(resource.key(), resource).is_some()
        {
            bail!("restart report refuses internal or duplicate resources");
        }
    }
    for file in &selection.files {
        if resources
            .get(file.key.as_str())
            .is_none_or(|resource| resource.path().to_str() != Some(file.path.as_str()))
        {
            bail!("restart report recorded key/path differs from fresh approved binding");
        }
    }
    let requests = approved
        .iter()
        .map(|resource| {
            LockRequest::new(
                LockClass::Resource,
                crate::core::workspace_txn::resource_lock("native-file", resource.key()),
                crate::core::repo_lock::LockMode::Exclusive,
                "restart data-only classification",
            )
        })
        .collect();
    let locks = LockSet::acquire(
        requests,
        Duration::ZERO,
        None,
        &LockOwner::new("restart-report", "data-only classification"),
    )?;
    for resource in approved {
        resource.verify()?;
    }
    Ok(LockedRecoveryResources {
        resources,
        _locks: locks,
    })
}

fn inspect_recovery_native_records(
    selection: &SpeculationRecoverySelection,
    locked: &LockedRecoveryResources<'_>,
) -> Result<Vec<crate::core::native_journal::RecordProof>> {
    use crate::core::native_journal::{RecordClaim, RecordProofStatus};
    // A missing receipt/ID is lack of proof, NEVER evidence that no live effect occurred.
    let Some(receipt) = &selection.native_receipt else {
        return Ok(Vec::new());
    };
    let mut claims = Vec::with_capacity(receipt.records.len());
    for record in &receipt.records {
        let file = selection
            .files
            .get(record.file)
            .context("restart record file is missing")?;
        let resource = locked
            .resources
            .get(file.key.as_str())
            .context("restart record resource is not freshly approved")?;
        claims.push(RecordClaim {
            id: record.id,
            expected_owner: &receipt.owner,
            resource,
            before: &file.preimage,
            after: &file.postimage,
        });
    }
    // Resource -> actual TM -> cohort owner -> Journal; the reader drops journal ownership here.
    // Covered evidence enumerates the complete fence even when the receipt lists no records.
    // Missing intents/claims remain unresolved, never inferred to be NotApplied from absent bytes.
    let proofs = match &receipt.coverage {
        Some(identity) => crate::core::native_journal::inspect_existing_batch(identity, &claims)?,
        None => crate::core::native_journal::inspect_existing_records(&claims)?,
    };
    if proofs.len() != receipt.records.len() {
        bail!("restart native record proof count differs from receipt");
    }
    for (proof, record) in proofs.iter().zip(&receipt.records) {
        if proof.id != record.id
            || (record.applied && proof.status != RecordProofStatus::SettledApplied)
        {
            bail!("restart applied receipt lacks exact settled native record evidence");
        }
    }
    Ok(proofs)
}

#[cfg(test)]
fn classify_recovery_resources(
    root: &Path,
    selection: &SpeculationRecoverySelection,
    approved: &[crate::core::workspace_txn::NativeResource],
) -> Result<Vec<RecoveryFileObservation>> {
    let resources = acquire_recovery_resources(root, selection, approved)?;
    classify_locked_recovery_resources(selection, &resources)
}

fn classify_locked_recovery_resources(
    selection: &SpeculationRecoverySelection,
    locked: &LockedRecoveryResources<'_>,
) -> Result<Vec<RecoveryFileObservation>> {
    let resources = &locked.resources;
    let mut observations = Vec::new();
    let mut pre = false;
    let mut post = false;
    for file in &selection.files {
        let resource = resources[file.key.as_str()];
        let current = recovery_live_fingerprint(resource)?;
        let class = recovery_image_class(&current, &file.preimage, &file.postimage);
        pre |= class == RecoveryImageClass::Pre;
        post |= class == RecoveryImageClass::Post;
        if class == RecoveryImageClass::Foreign {
            bail!("restart source has foreign bytes; evidence retained, no effect authorized");
        }
        observations.push(RecoveryFileObservation {
            key: file.key.clone(),
            class,
            current,
        });
    }
    if pre && post {
        bail!("restart source contains mixed pre/post images; evidence retained, no effect authorized");
    }
    for observation in &observations {
        if recovery_live_fingerprint(resources[observation.key.as_str()])? != observation.current {
            bail!("restart source drifted during classification");
        }
    }
    Ok(observations)
}

fn evidence_native_coverage(
    value: &serde_json::Value,
) -> Result<Option<crate::core::native_journal::BatchIdentity>> {
    let Some(value) = value.get("native_coverage") else {
        return Ok(None);
    };
    let identity: crate::core::native_journal::BatchIdentity =
        serde_json::from_value(value.clone()).context("invalid native batch coverage identity")?;
    identity.validate()?;
    Ok(Some(identity))
}

/// Legacy selected-ID linkage stays legacy. A covered receipt must repeat its exact prepared fence;
/// neither a matching numeric record ID nor an unsupported version can manufacture complete coverage.
fn validate_receipt_coverage(
    value: &serde_json::Value,
    prepared: Option<&crate::core::native_journal::BatchIdentity>,
) -> Result<Option<crate::core::native_journal::BatchIdentity>> {
    let coverage = evidence_native_coverage(value)?;
    match value
        .get("native_journal_linkage")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) if prepared.is_none() && coverage.is_none() => Ok(None),
        Some(2) if prepared.is_some() && coverage.as_ref() == prepared => Ok(coverage),
        _ => bail!(
            "integration receipt native coverage differs from prepared evidence; evidence retained"
        ),
    }
}

fn validate_receipt_evidence(value: &serde_json::Value, prepared: &PreparedEvidence) -> Result<()> {
    validate_receipt_coverage(value, prepared.native_coverage.as_ref())?;
    validate_receipt_record_ids(value, &prepared.files)
}

/// Validate copied journal identities as evidence only. Neither an ID nor a matching image grants
/// restart mutation/undo authority. Older receipts without supported linkage remain unresolved.
fn validate_receipt_record_ids(
    value: &serde_json::Value,
    prepared: &[PreparedFileEvidence],
) -> Result<()> {
    match value
        .get("native_journal_linkage")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) if evidence_native_coverage(value)?.is_none() => {}
        Some(2) if evidence_native_coverage(value)?.is_some() => {}
        _ => bail!(
            "integration receipt lacks supported exact native-journal linkage; evidence retained"
        ),
    }
    let applied = value
        .get("applied")
        .and_then(serde_json::Value::as_u64)
        .context("integration receipt application count missing")?;
    let possible = match value.get("possibly_applied_record") {
        Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|id| *id > 0)
                .context("invalid possible native record ID")?,
        ),
        None => bail!("integration receipt possible record evidence missing"),
    };
    let files = evidence_array(
        value,
        "files",
        crate::core::speculation::evidence::MAX_COHORT_FILES,
    )?;
    if applied > prepared.len() as u64
        || files.len() != prepared.len()
        || value.get("total").and_then(serde_json::Value::as_u64) != Some(prepared.len() as u64)
        || (possible.is_some() && applied == prepared.len() as u64)
    {
        bail!("integration receipt journal linkage has inconsistent file counts");
    }
    let mut previous = 0u64;
    for (index, (file, expected)) in files.iter().zip(prepared).enumerate() {
        let is_applied = (index as u64) < applied;
        let is_possible = index as u64 == applied && possible.is_some();
        let state = if is_applied {
            "applied"
        } else if is_possible {
            "possibly-applied"
        } else {
            "not-applied"
        };
        if evidence_text(file, "key")? != expected.key || evidence_text(file, "state")? != state {
            bail!("integration receipt record linkage differs from prepared file order");
        }
        let record = match file.get("native_record_id") {
            Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|id| *id > 0)
                    .context("invalid native record ID")?,
            ),
            None => bail!("integration receipt file lacks exact native record evidence"),
        };
        let changed = expected.preimage != expected.postimage;
        if is_possible {
            if !changed || record != possible {
                bail!("possible native record differs from its exact file");
            }
        } else if record.is_some() != (is_applied && changed) {
            bail!("native record attribution disagrees with the file effect (including no-op)");
        }
        if let Some(id) = record {
            if id <= previous {
                bail!("native record IDs are duplicate or out of application order");
            }
            previous = id;
        }
    }
    Ok(())
}

fn classify_integration_records(
    base: &str,
    records: &[crate::core::speculation::evidence::InspectedRecord],
) -> Result<(usize, usize, usize)> {
    use std::collections::BTreeMap;
    crate::core::speculation::verification::validate_verification_records(base, records)?;
    crate::core::speculation::evidence::require_supported_history(base, records)?;
    let workers = crate::core::speculation::workers::validate_worker_records(base, records)?;
    if workers.unresolved() {
        bail!("speculative physical-worker lifecycle is unresolved or unsuccessful; evidence retained");
    }
    let mut prepared = BTreeMap::new();
    let mut receipts = BTreeMap::new();
    let mut verifications = BTreeMap::new();
    for record in records {
        if record.label == crate::core::speculation::recovery::OBSERVATION_LABEL {
            validate_inspection_observation(base, record, records)?;
            continue;
        }
        if !record.label.starts_with("integration-") {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&record.metadata)
            .context("corrupt speculative integration metadata")?;
        if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
            || evidence_text(&value, "base")? != base
            || evidence_text(&value, "owner")? != record.owner
        {
            bail!(
                "unsupported or mismatched speculative integration record {}",
                record.id
            );
        }
        match record.label.as_str() {
            "integration-prepared" => {
                if evidence_text(&value, "state")? != "prepared" {
                    bail!("invalid integration prepared state");
                }
                let keys = validate_prepared_evidence(base, &record.owner, &value, &record.images)?;
                workers.check_selected_lineage(&value)?;
                if prepared
                    .insert(record.id.as_str(), (record.owner.as_str(), keys))
                    .is_some()
                {
                    bail!("duplicate integration prepared evidence");
                }
            }
            "integration-receipt" => {
                let prepared_id = evidence_text(&value, "prepared")?.to_owned();
                if receipts.insert(prepared_id, (record, value)).is_some() {
                    bail!("ambiguous integration receipts for one prepared record");
                }
            }
            "integration-verification-started" => {
                // A durable start alone never settles an integration.
            }
            "integration-verification" => {
                let prepared_id = evidence_text(&value, "prepared")?.to_owned();
                if verifications.insert(prepared_id, (record, value)).is_some() {
                    bail!("ambiguous integration verification evidence");
                }
            }
            _ => bail!("unsupported speculative integration record label; evidence retained"),
        }
    }
    for id in receipts.keys().chain(verifications.keys()) {
        if !prepared.contains_key(id.as_str()) {
            bail!("orphan speculative integration receipt/verification; evidence retained");
        }
    }
    for (id, (owner, evidence)) in &prepared {
        let keys = &evidence.keys;
        let (receipt, value) = receipts
            .get(*id)
            .with_context(|| format!("prepared integration {id} has no durable receipt"))?;
        if receipt.owner != *owner
            || evidence_text(value, "state")? != "applied-unverified"
            || value.get("applied").and_then(serde_json::Value::as_u64) != Some(keys.len() as u64)
            || value.get("total").and_then(serde_json::Value::as_u64) != Some(keys.len() as u64)
            || value
                .get("postimage_verified")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            bail!("integration {id} is partial or its postimage is unconfirmed; evidence retained");
        }
        validate_receipt_evidence(value, evidence)?;
        let receipt_files = evidence_array(
            value,
            "files",
            crate::core::speculation::evidence::MAX_COHORT_FILES,
        )?;
        if receipt_files.len() != keys.len()
            || !receipt.images.is_empty()
            || value.get("possibly_applied_record") != Some(&serde_json::Value::Null)
            || value.get("stop") != Some(&serde_json::Value::Null)
        {
            bail!("settled receipt has incomplete file evidence or possible effects");
        }
        for (file, key) in receipt_files.iter().zip(keys) {
            if evidence_text(file, "key")? != key
                || evidence_text(file, "state")? != "applied"
                || file
                    .get("postimage_checked")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
            {
                bail!("receipt file application differs from prepared order/images");
            }
        }
        let (verification, value) = verifications.get(*id).with_context(|| {
            format!("integration {id} is applied but has no accepted verification")
        })?;
        // Legacy verdicts lack physical lifecycle evidence and remain unresolved even if accepted.
        let finished: crate::core::speculation::verification::VerificationFinished =
            serde_json::from_slice(&verification.metadata)
                .context("integration verification lacks supported durable lifecycle evidence")?;
        finished.validate()?;
        if finished.effect_state
            != crate::core::speculation::verification::DurableEffectState::Settled
        {
            bail!("integration verifier has unsettled physical effects; evidence retained");
        }
        let dependency_status = if evidence.read_dependencies.is_empty() {
            "none"
        } else {
            "requires_semantic_review"
        };
        if verification.owner != *owner
            || evidence_text(value, "integration")? != receipt.id
            || evidence_text(value, "state")? != "verified"
            || value.get("accepted").and_then(serde_json::Value::as_bool) != Some(true)
            || !value.get("source_identity").is_some_and(evidence_digest)
            || !verification.images.is_empty()
            || evidence_text(value, "read_dependency_status")? != dependency_status
            || evidence_array(
                value,
                "read_dependencies",
                32 * crate::core::speculation::evidence::MAX_COHORT_FILES,
            )? != evidence.read_dependencies.as_slice()
            || evidence_array(
                value,
                "applied_paths",
                crate::core::speculation::evidence::MAX_COHORT_FILES,
            )? != evidence
                .paths
                .iter()
                .map(|path| serde_json::Value::String(path.clone()))
                .collect::<Vec<_>>()
                .as_slice()
        {
            bail!("integration {id} verification is stale, failed, unverified or mismatched (including retained read concerns); evidence retained");
        }
    }
    let concerns = prepared
        .values()
        .map(|(_, evidence)| evidence.read_dependencies.len())
        .sum();
    Ok((prepared.len(), verifications.len(), concerns))
}

pub(crate) fn inspect_speculation_namespace(namespace: &Path) -> Result<SpeculationInspection> {
    let mut summary = SpeculationInspection::default();
    let mut scan_bytes = 0u64;
    for version in checked_metadata_entries(namespace)? {
        let Some(name) = version.file_name().and_then(|name| name.to_str()) else {
            bail!("invalid speculation namespace name");
        };
        if !name.starts_with("speculation-") {
            continue;
        }
        if name != "speculation-v1" {
            bail!("unsupported future speculation namespace; evidence retained");
        }
        for directory in checked_metadata_entries(&version)? {
            // Descriptor identity checks also need the canonical Windows verbatim spelling for
            // long content-addressed paths. Retain the ancestor guard through inspection.
            let directory_guard = crate::core::workspace_txn::NativeResource::resolve(
                &directory.join(".tm-inspection-boundary"),
            )?;
            let directory = directory_guard
                .path()
                .parent()
                .context("missing cohort directory")?;
            if summary.cohorts >= MAX_SPECULATION_COHORTS {
                bail!("speculation cohort inspection exceeds {MAX_SPECULATION_COHORTS}; evidence retained");
            }
            summary.cohorts += 1;
            let base = directory
                .file_name()
                .and_then(|name| name.to_str())
                .context("invalid speculation cohort name")?;
            let paths = checked_metadata_entries(directory)?;
            if paths.len() > 2049 {
                bail!("speculation cohort object count exceeds inspection bound");
            }
            // Bound aggregate bytes BEFORE the store inspector materializes any record/image data.
            for path in &paths {
                let resource = crate::core::workspace_txn::NativeResource::resolve(path)?;
                scan_bytes = scan_bytes
                    .checked_add(fs::symlink_metadata(path)?.len())
                    .context("speculation inspection byte overflow")?;
                if scan_bytes > MAX_SPECULATION_INSPECTION_BYTES {
                    bail!("speculation namespace inspection exceeds 64 MiB; evidence retained");
                }
                resource.verify()?;
            }
            // Nonblocking ownership probe in the store inspector; never wait for a worker under TM locks.
            let records = crate::core::speculation::evidence::inspect_controller_records(directory)
                .context("cannot inspect speculative cohort (active owner or corrupt/incomplete evidence); let workers exit before retrying")?;
            let mut bases = 0usize;
            for path in paths {
                let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("m-"))
                {
                    // The store inspector checks content addresses/images. Check every envelope too:
                    // its data-only API deliberately skips non-controller manifests, including future kinds.
                    let bytes = read_speculation_evidence(&path, 2 * 1024 * 1024)?;
                    let value: serde_json::Value =
                        serde_json::from_slice(&bytes).context("corrupt speculation manifest")?;
                    if evidence_text(&value, "format")? != "aizen-speculation-v1"
                        || evidence_text(&value, "base")? != base
                    {
                        bail!("future or mismatched speculation manifest; evidence retained");
                    }
                    match evidence_text(&value, "kind")? {
                        "base" => bases += 1,
                        "overlay" | "controller-record" => {}
                        _ => bail!("unsupported speculation manifest kind; evidence retained"),
                    }
                }
                resource.verify()?;
            }
            if bases != 1 {
                bail!("speculation cohort has missing/ambiguous base evidence");
            }
            let (prepared, verified, concerns) = classify_integration_records(base, &records)?;
            summary.prepared += prepared;
            summary.verified += verified;
            summary.read_dependency_concerns += concerns;
            directory_guard.verify()?;
        }
    }
    Ok(summary)
}

#[cfg(test)]
#[path = "inspection_fixtures.rs"]
pub(crate) mod fixtures;

#[cfg(test)]
#[path = "inspection_tests.rs"]
mod tests;
