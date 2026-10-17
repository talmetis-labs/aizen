//! Parent-only deterministic integration of sealed speculative overlays.
//!
//! Planning never reads or writes live source. Applying requires caller-owned workspace/exclusive
//! physical admission, then sorted resource-only locks. Prepared images are durable before the first
//! native journal mutation. Partial effects are retained, attributed and typed; never rolled back.

use anyhow::{anyhow, ensure, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use super::merge::{self, AuthoredChange, Candidate, Conflict, Language, Limits, Operation};
use super::store::{BaseSnapshot, OverlayChange, SealedOverlay, StoredRecordId, WriteProvenance};
use crate::core::exec_ctx::ExecutionContext;
use crate::core::native_journal;
use crate::core::persist::{self, FileFingerprint};
use crate::core::repo_lock::LockMode;
use crate::core::workspace_txn::{
    LockClass, LockOwner, LockRequest, LockSet, NativeResource, PhysicalEffectLease,
    WorkspaceWriterLease,
};

const MAX_PLAN_FILES: usize = 256;
const MAX_OVERLAYS: usize = 32;
const MAX_INPUT_CHANGES: usize = MAX_OVERLAYS * 32;
const MAX_RECORD_META: usize = super::store::MAX_CONTROLLER_RECORD_META;
// Each record consumes its metadata blob AND the storage wrapper (including image descriptors).
const MAX_RECORD_RESERVATION_META: usize = 2 * (MAX_RECORD_META + 128 * 1024);
const MAX_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ConflictArtifact {
    pub(crate) key: String,
    pub(crate) kind: String,
    pub(crate) authors: Vec<String>,
    pub(crate) base_range: Option<std::ops::Range<usize>>,
}

#[derive(Debug)]
pub(crate) enum PlanningOutcome {
    Ready(IntegrationPlan),
    // The conflict record has already been durably published before this outcome is returned.
    Conflicted(Vec<ConflictArtifact>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlannedFile {
    key: String,
    resource: NativeResource,
    expected: FileFingerprint,
    bytes: Vec<u8>,
    contributors: Vec<String>,
    operation: Operation,
    lineage: Vec<ChangeLineage>,
}

impl PlannedFile {
    pub(crate) fn key(&self) -> &str { &self.key }
    pub(crate) fn resource(&self) -> &NativeResource { &self.resource }
    pub(crate) fn expected(&self) -> &FileFingerprint { &self.expected }
    pub(crate) fn bytes(&self) -> &[u8] { &self.bytes }
    #[cfg(test)]
    pub(crate) fn contributors(&self) -> &[String] { &self.contributors }
}

pub(crate) struct IntegrationPlan {
    base: BaseSnapshot,
    files: Vec<PlannedFile>,
    parent_scope: String,
    prepared_meta: Vec<u8>,
    read_dependencies: Vec<ReadDependencyConcern>,
}

impl std::fmt::Debug for IntegrationPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntegrationPlan")
            .field("base", &self.base.id())
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptState {
    AppliedUnverified,
    Partial,
}

#[derive(Debug)]
pub(crate) struct IntegrationReceipt {
    pub(crate) state: ReceiptState,
    pub(crate) prepared: StoredRecordId,
    pub(crate) receipt: StoredRecordId,
    pub(crate) applied_paths: Vec<String>,
    // Exact bytes checked, NOT build/test/semantic verification.
    pub(crate) postimage_verified: bool,
    /// A peer changed a file this child observed. Build/test verification must not silently
    /// certify the child's reasoning against the old peer image merely because text merged.
    pub(crate) read_dependencies: Vec<ReadDependencyConcern>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ReadDependencyConcern {
    pub(crate) observer: String,
    pub(crate) key: String,
    pub(crate) peer_authors: Vec<String>,
}

/// Core-layer physical uncertainty, mapped by the tool layer to PossiblyApplied. Evidence remains
/// owned by the cohort even if receipt publication itself failed. This never authorizes a retry.
#[derive(Debug)]
pub(crate) struct IntegrationUnsettled {
    pub(crate) possibly_applied_record: Option<u64>,
    pub(crate) prepared: StoredRecordId,
    pub(crate) receipt: Option<StoredRecordId>,
}

impl std::fmt::Display for IntegrationUnsettled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "integration effects are unsettled; retain prepared {}", self.prepared.as_str())?;
        if let Some(receipt) = &self.receipt {
            write!(f, " and receipt {}", receipt.as_str())?;
        }
        write!(f, "; no retry, rollback or automatic recovery is authorized")
    }
}
impl std::error::Error for IntegrationUnsettled {}

#[derive(Clone, Debug, Serialize)]
struct FingerprintDto {
    exists: bool,
    byte_len: u64,
    sha256: [u8; 32],
}

impl From<&FileFingerprint> for FingerprintDto {
    fn from(value: &FileFingerprint) -> Self {
        Self { exists: value.exists, byte_len: value.byte_len, sha256: value.sha256 }
    }
}

#[derive(Clone, Debug, Serialize)]
struct PreparedImage {
    // Index into the controller record's images array, never inferred from hashes or ordering.
    index: Option<usize>,
    missing: bool,
    #[serde(flatten)]
    fingerprint: FingerprintDto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct OverlayLineage {
    base: String,
    owner: String,
    id: String,
    generation: u64,
    manifest: String,
    author: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ChangeLineage {
    author: String,
    generation: u64,
    operation: &'static str,
}

#[derive(Serialize)]
struct ObservationEvidence {
    author: String,
    key: String,
    path: std::path::PathBuf,
    generation: u64,
    fingerprint: FingerprintDto,
}

#[derive(Serialize)]
struct PreparedFile<'a> {
    key: &'a str,
    path: &'a Path,
    expected: FingerprintDto,
    after: FingerprintDto,
    preimage: PreparedImage,
    postimage: PreparedImage,
    contributors: &'a [String],
    operation: &'static str,
    lineage: &'a [ChangeLineage],
}

#[derive(Serialize)]
struct PreparedMetadata<'a> {
    schema: u8,
    overlay_lineage_version: u8,
    state: &'static str,
    base: &'a str,
    owner: &'a str,
    overlays: &'a [OverlayLineage],
    observations: &'a [ObservationEvidence],
    read_dependencies: &'a [ReadDependencyConcern],
    files: Vec<PreparedFile<'a>>,
}

#[derive(Serialize)]
struct ReceiptFile<'a> {
    key: &'a str,
    state: &'static str,
    postimage_checked: bool,
    native_record_id: Option<u64>,
}

#[derive(Serialize)]
struct ReceiptMetadata<'a> {
    schema: u8,
    native_journal_linkage: u8,
    native_coverage: &'a native_journal::BatchIdentity,
    state: &'static str,
    base: &'a str,
    owner: &'a str,
    prepared: &'a str,
    applied: usize,
    total: usize,
    postimage_verified: bool,
    possibly_applied_record: Option<u64>,
    stop: Option<&'static str>,
    files: Vec<ReceiptFile<'a>>,
}

impl IntegrationPlan {
    /// Build from sealed in-process handles. Cohort admission enforces the complete selected set;
    /// here every supplied handle is checked for the exact same base and slash-bounded parentage.
    /// Immediate-preimage offsets only select policy, and are never used as original-base offsets.
    pub(crate) fn build(
        parent: &ExecutionContext,
        base: &BaseSnapshot,
        overlays: &[SealedOverlay],
        limits: &Limits,
    ) -> Result<PlanningOutcome> {
        ensure!(!overlays.is_empty(), "integration requires at least one sealed overlay");
        ensure!(overlays.len() <= MAX_OVERLAYS, "integration overlay count exceeds bound");
        ensure!(parent.can_restore_workspace() && parent.native_scope().is_none() && parent.overlay().is_none(),
            "integration planning requires the plain coordinating parent");
        let owner = parent.resource_scope();
        ensure!(!owner.is_empty(), "integration parent scope is empty");
        let mut selected = BTreeSet::new();
        let mut lineage = Vec::with_capacity(overlays.len());
        let mut observations = Vec::new();
        let mut input = Vec::new();
        for overlay in overlays {
            ensure!(overlay.belongs_to(base), "sealed overlay belongs to another base");
            ensure!(selected.insert(overlay.id()), "duplicate sealed integration overlay");
            let overlay_owner = overlay.owner();
            ensure!(
                overlay_owner == owner
                    || overlay_owner.strip_prefix(&owner).is_some_and(|tail| tail.starts_with('/')),
                "sealed overlay owner is outside parent scope"
            );
            let author = format!("{}/{}", overlay_owner, overlay.id());
            let identity = overlay.manifest_identity()?;
            ensure!(identity.base == base.id() && identity.overlay == overlay.id()
                && identity.owner == overlay_owner,
                "sealed overlay manifest identity differs from live lineage");
            lineage.push(OverlayLineage {
                base: identity.base,
                owner: identity.owner,
                id: identity.overlay,
                generation: identity.generation,
                manifest: identity.manifest,
                author: author.clone(),
            });
            for observation in overlay.observations()? {
                observations.push(ObservationEvidence {
                    author: author.clone(), key: observation.resource().key().to_owned(),
                    path: observation.resource().path().to_path_buf(), generation: observation.generation(),
                    fingerprint: observation.fingerprint().into(),
                });
            }
            for change in overlay.changes()? {
                validate_change_history(&change)?;
                input.push((author.clone(), change));
                ensure!(input.len() <= MAX_INPUT_CHANGES, "integration changed-image count exceeds bound");
            }
        }
        lineage.sort_by(|a, b| a.author.cmp(&b.author));
        observations.sort_by(|a, b| a.author.cmp(&b.author)
            .then(a.key.cmp(&b.key)).then(a.generation.cmp(&b.generation)));
        let mut dependencies: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for observation in &observations {
            for (author, change) in &input {
                if author != &observation.author && change.resource().key() == observation.key {
                    dependencies.entry((observation.author.clone(), observation.key.clone()))
                        .or_default().insert(author.clone());
                }
            }
        }
        let read_dependencies: Vec<_> = dependencies.into_iter().map(|((observer, key), peers)| {
            ReadDependencyConcern { observer, key, peer_authors: peers.into_iter().collect() }
        }).collect();
        input.sort_by(|a, b| a.1.resource().key().cmp(b.1.resource().key()).then(a.0.cmp(&b.0)));
        let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, (_, change)) in input.iter().enumerate() {
            grouped.entry(change.resource().key().to_owned()).or_default().push(index);
        }
        ensure!(grouped.len() <= MAX_PLAN_FILES, "integration changed-file count exceeds bound");
        let mut files = Vec::new();
        let mut conflicts = Vec::new();
        for (key, members) in grouped {
            let first = &input[members[0]].1;
            let mut authored = Vec::with_capacity(members.len());
            let mut changes = Vec::with_capacity(members.len());
            for index in members {
                let (author, change) = &input[index];
                ensure!(change.resource() == first.resource(), "integration resource identity differs for key");
                // Provenance is per change. One rewrite must not relabel another child's localized edit.
                let operation = change_operation(change);
                changes.push(ChangeLineage {
                    author: author.clone(), generation: change.generation(), operation: operation_name(operation),
                });
                authored.push(AuthoredChange {
                    author, base: change.base_bytes(), result: change.bytes(), operation,
                });
            }
            let operation = if authored.iter().any(|change| change.operation == Operation::Rewrite) {
                Operation::Rewrite
            } else {
                Operation::Localized
            };
            match merge::merge(language_for(first.resource().path()), &authored, limits) {
                Ok(candidate) => files.push(planned_file(key, first, candidate, operation, changes)),
                Err(conflict) => conflicts.push(conflict_artifact(key, conflict)),
            }
        }
        if !conflicts.is_empty() {
            // A conflict outcome must not be the sole, ephemeral copy of conflict evidence.
            let metadata = bounded_metadata(&serde_json::json!({
                "schema": 1, "overlay_lineage_version": 1,
                "state": "conflicted", "base": base.id(), "owner": owner,
                "overlays": lineage, "observations": observations,
                "read_dependencies": read_dependencies, "conflicts": conflicts,
            }), "conflict")?;
            base.controller_record(&owner, "integration-conflict", &metadata, &[])?;
            return Ok(PlanningOutcome::Conflicted(conflicts));
        }
        // BTreeMap traversal has already established deterministic file and image ordering.
        let prepared_meta = prepared_metadata(base, &owner, &files, &lineage, &observations, &read_dependencies)?;
        Ok(PlanningOutcome::Ready(Self {
            base: base.clone(), files, parent_scope: owner, prepared_meta, read_dependencies,
        }))
    }

    #[cfg(test)]
    pub(crate) fn files(&self) -> &[PlannedFile] { &self.files }
    #[cfg(test)]
    pub(crate) fn read_dependencies(&self) -> &[ReadDependencyConcern] { &self.read_dependencies }

    /// The caller acquires writer -> exclusive physical admission. This method only acquires the
    /// later, globally sorted resource locks; it MUST NOT nest another physical lease.
    pub(crate) fn apply(
        &self,
        parent: &ExecutionContext,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
        cancel: Option<&crate::core::cancel::TurnCancel>,
    ) -> Result<IntegrationReceipt> {
        ensure!(physical.is_exclusive(), "integration requires exclusive physical admission");
        ensure!(parent.can_restore_workspace(), "delegated execution cannot integrate a cohort");
        ensure!(parent.native_scope().is_none() && parent.overlay().is_none(), "integration requires the plain parent lane");
        let current = crate::core::exec_ctx::current().context("integration requires the current parent context")?;
        ensure!(current.resource_scope() == parent.resource_scope()
            && current.can_restore_workspace() && current.native_scope().is_none() && current.overlay().is_none(),
            "integration parent is not current");
        ensure!(parent.resource_scope() == self.parent_scope, "integration plan belongs to another parent");
        ensure!(cancel.is_none_or(|token| !token.is_cancelled()), "integration cancelled before admission");
        ensure!(self.files.len() <= MAX_PLAN_FILES, "integration file count exceeds bound");
        ensure!(writer.identity() == self.base.identity(), "integration writer identity differs from base");
        let requests = self.files.iter().map(|file| LockRequest::new(
            LockClass::Resource,
            crate::core::workspace_txn::resource_lock("native-file", file.key()),
            LockMode::Exclusive,
            "speculation integration exact file",
        )).collect();
        let _locks = LockSet::acquire(
            requests, MAX_TIMEOUT, cancel,
            &LockOwner::new(parent.resource_scope(), "speculation integration"),
        )?;
        self.base.revalidate_live(writer, physical)?;
        for file in &self.files { file.resource.verify()?; }
        let images = self.prepared_images()?;
        let image_bytes = images.iter().try_fold(0usize, |sum, bytes| sum.checked_add(bytes.len()))
            .context("integration image reservation overflow")?;
        // Both evidence records and ALL their image capacity are reserved before any live effect.
        let mut reservation = self.base.reserve_controller_records(
            &self.parent_scope, 2, MAX_RECORD_RESERVATION_META, image_bytes,
        )?;
        let writes: Vec<_> = self.files.iter()
            .map(|file| (file.resource(), file.expected(), file.bytes())).collect();
        // Retain the actual late journal lock so every file's capacity is admitted before source
        // effects; no other workspace can consume this batch's records or image bytes mid-apply.
        let mut batch = native_journal::begin_covered_batch(&self.parent_scope, &writes)?;
        let native_coverage = batch.coverage_identity()
            .context("integration batch lacks complete native-intent coverage")?.clone();
        native_coverage.validate()?;
        // Planning is pure/deterministic; attach the admitted durable fence only at materialization.
        // This exact identity must reach the prepared record before any batch entry is applied.
        let mut prepared_value: serde_json::Value = serde_json::from_slice(&self.prepared_meta)?;
        prepared_value.as_object_mut().context("integration prepared metadata is not an object")?
            .insert("native_coverage".into(), serde_json::to_value(&native_coverage)?);
        let prepared_meta = bounded_metadata(&prepared_value, "covered prepared")?;
        let image_refs: Vec<&[u8]> = images.iter().map(Vec::as_slice).collect();
        #[cfg(test)]
        test_fault(TestFault::BeforePrepared)?;
        let prepared = self.base.controller_record_reserved(
            &mut reservation, "integration-prepared", &prepared_meta, &image_refs,
        )?;
        let mut applied = 0usize;
        let mut native_record_ids = vec![None; self.files.len()];
        let mut possible_index = None;
        let mut possible_record = None;
        let mut failure = None;
        let mut stop = None;
        for (index, file) in self.files.iter().enumerate() {
            if cancel.is_some_and(crate::core::cancel::TurnCancel::is_cancelled) {
                failure = Some(anyhow!("integration cancelled after {applied} files"));
                stop = Some("cancelled");
                break;
            }
            match batch.apply_with_record(file.resource(), file.expected(), file.bytes()) {
                Ok(result) => {
                    native_record_ids[index] = result.native_record_id;
                    applied += 1;
                    // Explicitly record in the supplied parent too: the pinned context may be a
                    // clone carrying a different collector, and receipt persistence can fail next.
                    if result.fingerprint != *file.expected() {
                        parent.note_workspace_changes(&[file.resource.path().to_path_buf()]);
                    }
                }
                Err(error) => {
                    if let Some(uncertain) = error.downcast_ref::<native_journal::PossiblyApplied>() {
                        possible_index = Some(index);
                        possible_record = Some(uncertain.record_id);
                        native_record_ids[index] = Some(uncertain.record_id);
                        parent.note_workspace_changes(&[file.resource.path().to_path_buf()]);
                    }
                    failure = Some(error.context(format!("integration stopped after {applied} files")));
                    stop = Some("native-apply");
                    break;
                }
            }
            #[cfg(test)]
            if let Err(error) = test_fault(TestFault::AfterApplied(applied)) {
                failure = Some(error);
                stop = Some("native-apply");
                break;
            }
        }
        drop(batch);
        // No post-effect `?`: even failed reads/ancestor checks must reach the durable receipt path.
        let mut checked = vec![false; self.files.len()];
        if failure.is_none() {
            for (index, file) in self.files.iter().enumerate() {
                let check = (|| -> Result<()> {
                    #[cfg(test)]
                    test_fault(TestFault::Postimage)?;
                    file.resource.verify()?;
                    ensure!(persist::fingerprint(file.resource.path())? == FileFingerprint::for_bytes(file.bytes()),
                        "integration postimage does not match the prepared candidate");
                    Ok(())
                })();
                match check {
                    Ok(()) => checked[index] = true,
                    Err(error) => {
                        failure = Some(error);
                        stop = Some("postimage-check");
                        break;
                    }
                }
            }
        }
        let postimage_verified = failure.is_none() && applied == self.files.len() && checked.iter().all(|checked| *checked);
        let state = if postimage_verified { ReceiptState::AppliedUnverified } else { ReceiptState::Partial };
        let has_effects = applied > 0 || possible_index.is_some();
        let receipt_result = (|| -> Result<StoredRecordId> {
            let receipt_meta = bounded_metadata(&ReceiptMetadata {
                schema: 1, native_journal_linkage: 2, native_coverage: &native_coverage,
                state: match state { ReceiptState::Partial => "partial", ReceiptState::AppliedUnverified => "applied-unverified" },
                base: self.base.id(), owner: &self.parent_scope, prepared: prepared.as_str(), applied,
                total: self.files.len(), postimage_verified, possibly_applied_record: possible_record, stop,
                files: self.files.iter().enumerate().map(|(index, file)| ReceiptFile {
                    key: &file.key,
                    state: if index < applied { "applied" } else if possible_index == Some(index) { "possibly-applied" } else { "not-applied" },
                    postimage_checked: checked[index],
                    native_record_id: native_record_ids[index],
                }).collect(),
            }, "receipt")?;
            #[cfg(test)]
            test_fault(TestFault::BeforeReceipt)?;
            self.base.controller_record_reserved(&mut reservation, "integration-receipt", &receipt_meta, &[])
        })();
        let receipt = match receipt_result {
            Ok(receipt) => receipt,
            Err(error) => {
                if has_effects {
                    return Err(anyhow::Error::new(IntegrationUnsettled {
                        possibly_applied_record: possible_record, prepared, receipt: None,
                    }).context(format!("integration receipt persistence failed: {error:#}")));
                }
                return Err(error.context("integration receipt persistence failed before any live effect"));
            }
        };
        if let Some(error) = failure {
            if has_effects {
                return Err(anyhow::Error::new(IntegrationUnsettled {
                    possibly_applied_record: possible_record, prepared, receipt: Some(receipt),
                }).context(format!("integration stopped: {error:#}")));
            }
            return Err(error.context(format!("integration stopped without live effects; receipt {}", receipt.as_str())));
        }
        Ok(IntegrationReceipt {
            state, prepared, receipt, postimage_verified,
            read_dependencies: self.read_dependencies.clone(),
            applied_paths: self.files.iter().take(applied)
                .map(|file| file.resource.path().to_string_lossy().into_owned()).collect(),
        })
    }

    fn prepared_images(&self) -> Result<Vec<Vec<u8>>> {
        let mut images = Vec::with_capacity(self.files.len() * 2);
        for file in &self.files {
            if file.expected.exists {
                let bytes = self.base.image(file.resource())?.context("integration base image missing")?;
                ensure!(FileFingerprint::for_bytes(&bytes) == file.expected, "integration base image fingerprint mismatch");
                images.push(bytes);
            }
            images.push(file.bytes.clone());
        }
        Ok(images)
    }
}

fn validate_change_history(change: &OverlayChange) -> Result<()> {
    let mut fingerprint = change.base_fingerprint();
    let mut generation = 0;
    for edit in change.edits() {
        ensure!(edit.generation() > generation && edit.generation() <= change.generation(),
            "overlay edit generations are not ordered");
        ensure!(edit.before_fingerprint() == &fingerprint,
            "overlay edit history does not match its exact preimage");
        fingerprint = FileFingerprint::for_bytes(edit.bytes());
        generation = edit.generation();
    }
    ensure!(generation == change.generation() && fingerprint == change.fingerprint(),
        "overlay edit history does not reach its sealed result");
    Ok(())
}

fn change_operation(change: &OverlayChange) -> Operation {
    if change.edits().iter().any(|edit| edit.provenance() == WriteProvenance::Rewrite) {
        Operation::Rewrite
    } else {
        Operation::Localized
    }
}

fn operation_name(operation: Operation) -> &'static str {
    match operation { Operation::Localized => "localized", Operation::Rewrite => "rewrite" }
}

fn planned_file(
    key: String, change: &OverlayChange, candidate: Candidate, operation: Operation, lineage: Vec<ChangeLineage>,
) -> PlannedFile {
    PlannedFile {
        key, resource: change.resource().clone(), expected: change.base_fingerprint(),
        bytes: candidate.bytes, contributors: candidate.contributors, operation, lineage,
    }
}

fn language_for(path: &Path) -> Language {
    if path.extension().and_then(|ext| ext.to_str()) == Some("rs") { Language::Rust } else { Language::Other }
}

fn conflict_artifact(key: String, conflict: Conflict) -> ConflictArtifact {
    ConflictArtifact { key, kind: format!("{:?}", conflict.kind), authors: conflict.authors, base_range: conflict.base_range }
}

fn prepared_metadata(
    base: &BaseSnapshot, owner: &str, files: &[PlannedFile], overlays: &[OverlayLineage],
    observations: &[ObservationEvidence], read_dependencies: &[ReadDependencyConcern],
) -> Result<Vec<u8>> {
    let mut next_image = 0usize;
    let files = files.iter().map(|file| {
        let before_index = if file.expected.exists {
            let index = next_image;
            next_image += 1;
            Some(index)
        } else { None };
        let after_index = next_image;
        next_image += 1;
        let after = FileFingerprint::for_bytes(&file.bytes);
        PreparedFile {
            key: &file.key, path: file.resource.path(), expected: (&file.expected).into(), after: (&after).into(),
            preimage: PreparedImage { index: before_index, missing: !file.expected.exists, fingerprint: (&file.expected).into() },
            postimage: PreparedImage { index: Some(after_index), missing: false, fingerprint: (&after).into() },
            contributors: &file.contributors, operation: operation_name(file.operation), lineage: &file.lineage,
        }
    }).collect();
    bounded_metadata(&PreparedMetadata {
        schema: 1, overlay_lineage_version: 1, state: "prepared", base: base.id(), owner,
        overlays, observations, read_dependencies, files,
    }, "prepared")
}

fn bounded_metadata(value: &impl Serialize, label: &str) -> Result<Vec<u8>> {
    let metadata = serde_json::to_vec(value)?;
    ensure!(metadata.len() <= MAX_RECORD_META, "integration {label} metadata exceeds durable record bound");
    Ok(metadata)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestFault { BeforePrepared, AfterApplied(usize), Postimage, BeforeReceipt }
#[cfg(test)]
thread_local! { static TEST_FAULT: std::cell::Cell<Option<TestFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn test_fault(point: TestFault) -> Result<()> {
    TEST_FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            return Err(anyhow!("injected integration evidence failure"));
        }
        Ok(())
    })
}

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;
