//! Controller-owned exact-file speculation storage. This module NEVER writes live source files.
//!
//! Only capture and explicit controller revalidation inspect live source. Runtime resolution uses
//! sealed lexical bindings, including absent files. Disk manifests are evidence, never capabilities:
//! there is intentionally no API that reconstructs authority from a manifest after a restart.
//!
//! Filesystem checks and advisory OS locks protect cooperating controllers and detect ordinary
//! identity drift. They are not kernel confinement against a hostile same-user path-operation race.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::core::file_placement::{is_link_or_reparse, read_existing_regular, NodeStamp};
use crate::core::native_scope::NativeWriteScope;
use crate::core::persist::{self, FileFingerprint};
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::{
    NativeResource, PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
};

pub(crate) const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_CHILD_FILES: usize = 32;
pub(crate) const MAX_COHORT_FILES: usize = 256;
pub(crate) const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;
const MAX_OVERLAYS: usize = 32;
const MAX_RECORDS: usize = 512;
const MAX_OBSERVATIONS: usize = 512;
const MAX_META_BYTES: usize = 2 * 1024 * 1024;
const MAX_OWNER_BYTES: usize = 1024;
const MAX_OBJECTS: usize = 2048;
pub(crate) const MAX_CONTROLLER_RECORD_META: usize = 64 * 1024;
const MAX_CONTROLLER_RECORDS: usize = 16;
const MAX_CONTROLLER_IMAGES: usize = MAX_COHORT_FILES * 2;
const FORMAT: &str = "aizen-speculation-v1";
const NAMESPACE: &str = "speculation-v1";
const OWNER_MARKER: &[u8] = b"aizen-speculation-owner-v1\n";

/// Non-serializable controller capability. Cloning retains the same immutable base and OS owner.
#[derive(Clone)]
pub(crate) struct BaseSnapshot(Arc<BaseInner>);

struct BaseInner {
    id: String,
    identity: WorkspaceIdentity,
    root_key: String,
    scopes: Vec<NativeWriteScope>,
    files: BTreeMap<String, BoundFile>,
    paths: BTreeMap<String, String>,
    storage: Storage,
    disk: Mutex<DiskState>,
    manifest: String,
}

#[derive(Clone)]
struct BoundFile {
    resource: NativeResource,
    image: Image,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    exists: bool,
    byte_len: u64,
    sha256: [u8; 32],
}

impl Image {
    fn missing() -> Self {
        Self::from_fingerprint(&FileFingerprint::missing())
    }
    fn bytes(bytes: &[u8]) -> Self {
        Self::from_fingerprint(&FileFingerprint::for_bytes(bytes))
    }
    fn from_fingerprint(fp: &FileFingerprint) -> Self {
        Self {
            exists: fp.exists,
            byte_len: fp.byte_len,
            sha256: fp.sha256,
        }
    }
    fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint {
            exists: self.exists,
            byte_len: self.byte_len,
            sha256: self.sha256,
        }
    }
    fn blob_name(&self) -> Option<String> {
        self.exists.then(|| format!("b-{}", hex(&self.sha256)))
    }
}

/// A child receives this opaque capability, not a path to a directory of writable files.
#[derive(Clone)]
pub(crate) struct OverlayHandle(Arc<OverlayInner>);

impl std::fmt::Debug for OverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No source bodies, owner labels, or filesystem paths in ambient context debug output.
        f.debug_struct("OverlayHandle")
            .field("base", &self.0.base.id)
            .field("id", &self.0.id)
            .finish_non_exhaustive()
    }
}

struct OverlayInner {
    base: Arc<BaseInner>,
    id: String,
    owner: String,
    scope: NativeWriteScope,
    lifecycle: std::sync::OnceLock<super::lifecycle::AttemptLifecycle>,
    state: Mutex<OverlayState>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Version {
    generation: u64,
    image: Image,
}

struct OverlayState {
    generation: u64,
    files: BTreeMap<String, Version>,
    observations: Vec<Observation>,
    edits: Vec<Edit>,
    manifest: String,
    active_bodies: usize,
    attempt_admitted: bool,
    bodies_closed: bool,
    sealed: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Observation {
    key: String,
    generation: u64,
    image: Image,
}

/// Localized is verified against exact bytes, not trusted as a model-supplied attribution label.
/// Offsets refer to that edit's immediate preimage, not necessarily the cohort base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum WriteProvenance {
    Rewrite,
    Localized { start: usize, end: usize },
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Edit {
    key: String,
    generation: u64,
    before: Image,
    after: Image,
    provenance: WriteProvenance,
}

/// Resolved using sealed bindings only. Not serializable; never wraps a newly resolved live path.
#[derive(Clone)]
pub(crate) struct OverlayFile {
    overlay: Arc<OverlayInner>,
    key: String,
}

impl std::fmt::Debug for OverlayFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayFile")
            .field("overlay", &self.overlay.id)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl OverlayFile {
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
    pub(crate) fn logical_path(&self) -> &Path {
        self.path()
    }
    pub(crate) fn is_writable(&self) -> bool {
        self.overlay
            .scope
            .write_keys()
            .iter()
            .any(|key| key == &self.key)
    }
    pub(crate) fn resource(&self) -> &NativeResource {
        &self.overlay.base.files[&self.key].resource
    }
    pub(crate) fn path(&self) -> &Path {
        self.resource().path()
    }
}

/// Private read receipt is also CAS authority. Public accessors cannot construct or alter a receipt.
#[derive(Clone)]
pub(crate) struct OverlayRead {
    overlay: Arc<OverlayInner>,
    key: String,
    generation: u64,
    fingerprint: FileFingerprint,
    bytes: Option<Vec<u8>>,
}

impl std::fmt::Debug for OverlayRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayRead")
            .field("generation", &self.generation)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl OverlayRead {
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }
    pub(crate) fn fingerprint(&self) -> &FileFingerprint {
        &self.fingerprint
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

/// One admitted physical body. Clones retain that SAME body's lifetime; they do not admit another.
/// A worker must own a clone until it actually exits, including after cancellation/observer drop.
#[derive(Clone)]
pub(crate) struct OverlayBodyLease(Arc<BodyOwnership>);

impl std::fmt::Debug for OverlayBodyLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayBodyLease")
            .field("overlay", &self.0.overlay.id)
            .finish_non_exhaustive()
    }
}

struct BodyOwnership {
    overlay: Arc<OverlayInner>,
}

impl Drop for BodyOwnership {
    fn drop(&mut self) {
        match self.overlay.state.lock() {
            Ok(mut state) => {
                // An impossible count must never accidentally make a cohort sealable.
                if let Some(next) = state.active_bodies.checked_sub(1) {
                    state.active_bodies = next;
                } else if let Ok(mut disk) = self.overlay.base.disk.lock() {
                    disk.poisoned = true;
                }
            }
            Err(_) => {
                if let Ok(mut disk) = self.overlay.base.disk.lock() {
                    disk.poisoned = true;
                }
            }
        }
    }
}

/// Sealed controller capability. Neither a child summary nor persisted metadata can manufacture it.
#[derive(Clone)]
pub(crate) struct SealedOverlay(Arc<OverlayInner>);

#[derive(Clone, Debug)]
pub(crate) struct ReadObservation {
    resource: NativeResource,
    generation: u64,
    fingerprint: FileFingerprint,
}

impl ReadObservation {
    pub(crate) fn resource(&self) -> &NativeResource {
        &self.resource
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn fingerprint(&self) -> &FileFingerprint {
        &self.fingerprint
    }
}

/// Final changed image and exact operation history for the controller's merge policy.
pub(crate) struct OverlayChange {
    resource: NativeResource,
    base: Option<Arc<[u8]>>,
    after: Arc<[u8]>,
    generation: u64,
    edits: Vec<OverlayEdit>,
}

impl OverlayChange {
    pub(crate) fn resource(&self) -> &NativeResource {
        &self.resource
    }
    pub(crate) fn base_bytes(&self) -> Option<&[u8]> {
        self.base.as_deref()
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.after
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn base_fingerprint(&self) -> FileFingerprint {
        self.base
            .as_deref()
            .map_or_else(FileFingerprint::missing, FileFingerprint::for_bytes)
    }
    pub(crate) fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint::for_bytes(&self.after)
    }
    pub(crate) fn edits(&self) -> &[OverlayEdit] {
        &self.edits
    }
}

/// Offsets plus exact postimage allow replay/coordinate translation without inferring provenance.
pub(crate) struct OverlayEdit {
    generation: u64,
    before: FileFingerprint,
    after: Arc<[u8]>,
    provenance: WriteProvenance,
}

impl OverlayEdit {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn before_fingerprint(&self) -> &FileFingerprint {
        &self.before
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.after
    }
    pub(crate) fn provenance(&self) -> WriteProvenance {
        self.provenance
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StoredRecordId(String);
impl StoredRecordId {
    #[cfg(test)]
    pub(crate) fn id(&self) -> &str {
        &self.0
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One-shot metadata-only publication under a fresh existing exclusive owner. No BaseInner,
/// overlay, live controller reservation or deserializer can be recovered through this type.
pub(crate) struct ExistingObservationOwner {
    storage: Storage,
    disk: DiskState,
    revision: [u8; 32],
    consumed: bool,
    observer: String,
    _writer: WorkspaceWriterLease,
    _physical: PhysicalEffectLease,
}

#[derive(Debug)]
pub(crate) struct ObservationPublication {
    pub(crate) record: String,
    pub(crate) before: [u8; 32],
    pub(crate) after: [u8; 32],
}

impl ExistingObservationOwner {
    pub(crate) fn acquire(
        directory: &Path,
        revision: [u8; 32],
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<Self> {
        check_capture_leases(writer.identity(), writer, physical)?;
        let current = crate::core::exec_ctx::current()
            .context("inspection publication requires a plain parent")?;
        ensure!(
            current.can_restore_workspace()
                && current.native_scope().is_none()
                && current.overlay().is_none(),
            "inspection publication refuses delegated authority"
        );
        ensure!(
            !lexically_within(directory, &writer.identity().canonical_root)?,
            "inspection publication store must be outside workspace"
        );
        let guard = NativeResource::resolve(&directory.join(".store-boundary"))?;
        let lock_guard = NativeResource::resolve(&directory.join("owner.lock"))?;
        let lock_stamp = NodeStamp::capture(
            lock_guard.path(),
            &fs::symlink_metadata(lock_guard.path())?,
            false,
        )?;
        let owner = RepoTxnLock::acquire_existing_exclusive(lock_guard.path(), Duration::ZERO)?;
        let (disk, actual) = Storage::load_existing_inventory(directory)?;
        ensure!(
            actual == revision,
            "inspection publication selection is stale"
        );
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("inspection cohort ID missing")?;
        let value = Self {
            storage: Storage {
                id: id.to_owned(),
                directory: directory.to_path_buf(),
                root_guard: guard.clone(),
                directory_guard: guard,
                lock_guard,
                lock_stamp,
                _lock: owner,
            },
            disk,
            revision,
            consumed: false,
            observer: current.resource_scope(),
            _writer: writer.clone(),
            _physical: physical.clone(),
        };
        value.verify_owner()?;
        Ok(value)
    }

    fn verify_owner(&self) -> Result<()> {
        self.storage
            ._lock
            .verify_existing_path(self.storage.lock_guard.path())?;
        ensure!(
            self.storage
                ._lock
                .read_bounded_exclusive(OWNER_MARKER.len() as u64)?
                == OWNER_MARKER,
            "inspection publication owner marker changed"
        );
        self.storage.verify(&self.disk)
    }

    /// Only this fixed typed nonterminal record may be appended. Failure retains all evidence and
    /// consumes the holder; it never retries, cleans orphan objects or reports a clean no-effect.
    pub(crate) fn append(
        &mut self,
        observation: &super::recovery::InspectionObservation,
    ) -> Result<ObservationPublication> {
        self.append_bounded(observation, MAX_STORE_BYTES)
    }

    // Private budget seam for small owned boundary fixtures; production always uses 64 MiB.
    fn append_bounded(
        &mut self,
        observation: &super::recovery::InspectionObservation,
        expansion_limit: usize,
    ) -> Result<ObservationPublication> {
        ensure!(
            !self.consumed && !self.disk.poisoned,
            "inspection publication holder already consumed"
        );
        self.consumed = true;
        let result = self.append_inner(observation, expansion_limit);
        if result.is_err() {
            self.disk.poisoned = true;
        }
        result.context("inspection observation publication failed; persisted evidence may remain; no retry or cleanup ran")
    }

    fn append_inner(
        &mut self,
        observation: &super::recovery::InspectionObservation,
        expansion_limit: usize,
    ) -> Result<ObservationPublication> {
        observation.validate()?;
        let current =
            crate::core::exec_ctx::current().context("inspection publication lost parent")?;
        ensure!(
            current.can_restore_workspace()
                && current.native_scope().is_none()
                && current.overlay().is_none()
                && current.resource_scope() == self.observer
                && self.observer == observation.observer
                && observation.base == self.storage.id
                && observation.inventory_sha256 == self.revision,
            "inspection publication identity changed"
        );
        self.verify_owner()?;
        ensure!(
            inventory_revision(&self.storage.directory)? == self.revision,
            "inspection publication inventory drift"
        );
        let (records, expanded) =
            inspect_records_with_expansion(&self.storage, &self.disk, &self.storage.id)?;
        ensure!(
            records
                .iter()
                .any(|record| record.id == observation.prepared
                    && record.label == "integration-prepared"
                    && record.owner == observation.owner),
            "inspection publication prepared linkage missing"
        );
        let metadata = serde_json::to_vec(observation)?;
        let envelope = controller_metadata(
            &self.storage.id,
            &observation.observer,
            super::recovery::OBSERVATION_LABEL,
            &metadata,
            &[],
        )?;
        let blob = Image::bytes(&metadata);
        let blob_name = blob
            .blob_name()
            .context("inspection metadata blob absent")?;
        let manifest = manifest_name(&envelope);
        let new_record = usize::from(!self.disk.objects.contains_key(&manifest));
        // Logical expansion counts every returned record, even when its blobs are deduplicated.
        checked_inspection_expansion(
            expanded,
            if new_record == 1 { metadata.len() } else { 0 },
            expansion_limit,
        )?;
        // Check the COMPLETE pair before writing even the metadata blob. Existing orphans count.
        self.storage.reserve(
            &self.disk,
            &[(&blob_name, metadata.len()), (&manifest, envelope.len())],
            new_record,
        )?;
        self.verify_owner()?;
        ensure!(
            inventory_revision(&self.storage.directory)? == self.revision,
            "inspection publication inventory drift"
        );
        self.storage
            .publish(&mut self.disk, &blob_name, &metadata)?;
        fault(Fault::BeforeObservationManifest)?;
        self.storage.publish(&mut self.disk, &manifest, &envelope)?;
        fault(Fault::AfterObservationManifest)?;
        self.disk.records += new_record;
        self.verify_owner()?;
        let (actual, after) = Storage::load_existing_inventory(&self.storage.directory)?;
        ensure!(
            actual.bytes == self.disk.bytes
                && actual.records == self.disk.records
                && actual.objects == self.disk.objects,
            "inspection publication final inventory differs"
        );
        self.verify_owner()?;
        Ok(ObservationPublication {
            record: manifest,
            before: self.revision,
            after,
        })
    }
}

/// Reserved physical and expanded inspection capacity; only this opaque capability may consume it.
pub(crate) struct ControllerReservation {
    base: Arc<BaseInner>,
    owner: String,
    records: usize,
    metadata_bytes: usize,
    image_bytes: usize,
    object_slots: usize,
    images_remaining: usize,
}

impl ControllerReservation {
    pub(crate) fn remaining_records(&self) -> usize {
        self.records
    }
}

impl Drop for ControllerReservation {
    fn drop(&mut self) {
        if let Ok(mut disk) = self.base.disk.lock() {
            match (
                disk.reserved_bytes
                    .checked_sub(self.metadata_bytes.saturating_add(self.image_bytes)),
                disk.reserved_records.checked_sub(self.records),
                disk.reserved_objects.checked_sub(self.object_slots),
            ) {
                (Some(bytes), Some(records), Some(objects)) => {
                    disk.reserved_bytes = bytes;
                    disk.reserved_records = records;
                    disk.reserved_objects = objects;
                }
                _ => disk.poisoned = true,
            }
        }
    }
}

#[derive(Serialize)]
struct ControllerRecord<'a> {
    format: &'static str,
    kind: &'static str,
    base: &'a str,
    owner: &'a str,
    label: &'a str,
    // Raw record JSON/text is a blob, not interpreted as bindings/authority by storage.
    metadata: Image,
    images: Vec<Image>,
}

/// Inspection is untrusted data only; no method on this value confers file or integration rights.
#[derive(Debug)]
pub(crate) struct InspectedRecord {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) label: String,
    pub(crate) metadata: Vec<u8>,
    pub(crate) images: Vec<Vec<u8>>,
}

#[derive(Serialize)]
struct ManifestBinding<'a> {
    key: &'a str,
    path: &'a Path,
    base: &'a Image,
    writable: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestBindingOwned {
    key: String,
    path: PathBuf,
    base: Image,
    writable: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SealedOverlayManifest {
    format: String,
    kind: String,
    base: String,
    overlay: String,
    owner: String,
    generation: u64,
    sealed: bool,
    #[serde(deserialize_with = "deserialize_bindings")]
    bindings: Vec<ManifestBindingOwned>,
    #[serde(deserialize_with = "deserialize_versions")]
    files: BTreeMap<String, Version>,
    #[serde(deserialize_with = "deserialize_observations")]
    observations: Vec<Observation>,
    #[serde(deserialize_with = "deserialize_edits")]
    edits: Vec<Edit>,
}

/// Historical identity only. Deserializing this value never grants overlay or worker authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SealedManifestIdentity {
    pub(crate) base: String,
    pub(crate) overlay: String,
    pub(crate) owner: String,
    pub(crate) generation: u64,
    pub(crate) manifest: String,
}

impl SealedManifestIdentity {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_overlay_identity(&self.base, &self.overlay, &self.owner, self.generation)?;
        ensure!(
            super::recovery::immutable_record_id(&self.manifest),
            "speculation sealed manifest identity invalid"
        );
        Ok(())
    }
}

fn validate_overlay_identity(
    base: &str,
    overlay: &str,
    owner: &str,
    generation: u64,
) -> Result<()> {
    ensure!(
        base.len() == 32
            && base
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')),
        "speculation sealed base identity invalid"
    );
    let ordinal = overlay
        .strip_prefix("overlay-")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|ordinal| (1..=MAX_OVERLAYS).contains(ordinal))
        .context("speculation sealed overlay identity invalid")?;
    ensure!(
        overlay == format!("overlay-{ordinal}") && generation <= MAX_RECORDS as u64,
        "speculation sealed overlay identity invalid"
    );
    check_owner(owner)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseManifestOwned {
    format: String,
    kind: String,
    base: String,
    repo: String,
    worktree: String,
    root: PathBuf,
    bindings: Vec<ManifestBindingOwned>,
}

#[derive(Serialize)]
struct BaseManifest<'a> {
    format: &'static str,
    kind: &'static str,
    base: &'a str,
    repo: &'a str,
    worktree: &'a str,
    root: &'a Path,
    bindings: Vec<ManifestBinding<'a>>,
}

#[derive(Serialize)]
struct OverlayManifest<'a> {
    format: &'static str,
    kind: &'static str,
    base: &'a str,
    overlay: &'a str,
    owner: &'a str,
    generation: u64,
    sealed: bool,
    bindings: Vec<ManifestBinding<'a>>,
    files: &'a BTreeMap<String, Version>,
    observations: &'a [Observation],
    edits: &'a [Edit],
}

impl BaseSnapshot {
    /// `store_root` is an explicitly controller-owned EXISTING directory outside the workspace.
    /// No RepoContext discovery, git operation, scratch allocation, or live-source write occurs.
    /// Physical admission is checked in addition to the caller-owned workspace identity lease.
    pub(crate) fn capture(
        identity: &WorkspaceIdentity,
        scopes: &[NativeWriteScope],
        store_root: &Path,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<Self> {
        check_capture_leases(identity, writer, physical)?;
        ensure!(
            !scopes.is_empty() && scopes.len() <= MAX_OVERLAYS,
            "speculation scope count exceeds limit"
        );
        let root_key = path_key(&identity.canonical_root)?;
        let actual_root = fs::canonicalize(&identity.canonical_root)?;
        ensure!(
            path_key(&actual_root)? == root_key,
            "speculation workspace root has an alias"
        );
        let mut resources: BTreeMap<String, NativeResource> = BTreeMap::new();
        let mut paths = BTreeMap::new();
        for scope in scopes {
            check_scope_size(scope)?;
            for resource in scope.resources() {
                validate_source(identity, store_root, resource)?;
                let key = path_key(resource.path())?;
                if let Some(previous) = paths.insert(key, resource.key().to_owned()) {
                    ensure!(
                        previous == resource.key(),
                        "speculation ambiguous resource identity"
                    );
                }
                if let Some(previous) =
                    resources.insert(resource.key().to_owned(), resource.clone())
                {
                    ensure!(
                        previous == *resource,
                        "speculation inconsistent ancestor binding"
                    );
                }
                ensure!(
                    resources.len() <= MAX_COHORT_FILES,
                    "speculation cohort exceeds 256 files"
                );
            }
        }
        let storage = Storage::create(identity, store_root)?;
        let mut disk = DiskState::default();
        let mut files = BTreeMap::new();
        // Reading twice under caller exclusion detects ordinary non-cooperative capture drift.
        for (key, resource) in resources {
            let (bytes, image) = read_live(&resource)?;
            if let Some(bytes) = bytes {
                storage.publish_blob(&mut disk, &image, &bytes)?;
            }
            files.insert(key, BoundFile { resource, image });
        }
        for file in files.values() {
            ensure!(
                read_live(&file.resource)?.1 == file.image,
                "speculation source changed during capture"
            );
        }
        let id = storage.id.clone();
        let meta = encode_meta(&BaseManifest {
            format: FORMAT,
            kind: "base",
            base: &id,
            repo: &identity.repo_id,
            worktree: &identity.worktree_id,
            root: &identity.canonical_root,
            bindings: files
                .values()
                .map(|file| ManifestBinding {
                    key: file.resource.key(),
                    path: file.resource.path(),
                    base: &file.image,
                    writable: false,
                })
                .collect(),
        })?;
        let manifest = storage.publish_manifest(&mut disk, &meta)?;
        Ok(Self(Arc::new(BaseInner {
            id,
            identity: identity.clone(),
            root_key,
            scopes: scopes.to_vec(),
            files,
            paths,
            storage,
            disk: Mutex::new(disk),
            manifest,
        })))
    }

    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn identity(&self) -> &WorkspaceIdentity {
        &self.0.identity
    }
    pub(crate) fn resources(&self) -> Vec<NativeResource> {
        self.0
            .files
            .values()
            .map(|file| file.resource.clone())
            .collect()
    }

    /// Narrowing is sealed key/path/mode comparison; deliberately does NOT call live scope checks.
    pub(crate) fn create_overlay(
        &self,
        owner: &str,
        scope: &NativeWriteScope,
    ) -> Result<OverlayHandle> {
        check_owner(owner)?;
        check_scope_size(scope)?;
        ensure!(
            self.0
                .scopes
                .iter()
                .any(|approved| sealed_narrowing(approved, scope)),
            "speculation overlay widens controller-approved scope"
        );
        for resource in scope.resources() {
            let bound = self
                .0
                .files
                .get(resource.key())
                .context("speculation resource was not captured")?;
            ensure!(
                bound.resource == *resource,
                "speculation overlay binding differs from captured authority"
            );
        }
        let mut disk = self.0.lock_disk()?;
        ensure!(
            disk.overlays < MAX_OVERLAYS,
            "speculation overlay count exceeds limit"
        );
        let id = format!("overlay-{}", disk.overlays + 1);
        let mut state = OverlayState {
            generation: 0,
            files: scope
                .resources()
                .iter()
                .map(|r| {
                    (
                        r.key().to_owned(),
                        Version {
                            generation: 0,
                            image: self.0.files[r.key()].image.clone(),
                        },
                    )
                })
                .collect(),
            observations: Vec::new(),
            edits: Vec::new(),
            manifest: String::new(),
            active_bodies: 0,
            attempt_admitted: false,
            bodies_closed: false,
            sealed: false,
        };
        let bytes = overlay_metadata(&self.0, &id, owner, scope, &state)?;
        state.manifest = self.0.storage.publish_manifest(&mut disk, &bytes)?;
        disk.overlays += 1;
        Ok(OverlayHandle(Arc::new(OverlayInner {
            base: self.0.clone(),
            id,
            owner: owner.to_owned(),
            scope: scope.clone(),
            lifecycle: std::sync::OnceLock::new(),
            state: Mutex::new(state),
        })))
    }

    /// Integration must revalidate under FRESH caller-owned workspace + exclusive physical leases.
    /// Returns drift as an error, including missing -> present and same-spelling ancestor swaps.
    /// This conservative helper checks the entire base, not merely a selected child's read-set.
    pub(crate) fn revalidate_live(
        &self,
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<()> {
        check_capture_leases(&self.0.identity, writer, physical)?;
        let mut disk = self.0.lock_disk()?;
        if self
            .0
            .storage
            .verify_object(&disk, &self.0.manifest)
            .is_err()
        {
            disk.poisoned = true;
            bail!("speculation base manifest changed; discard this cohort");
        }
        for file in self.0.files.values() {
            ensure!(
                read_live(&file.resource)?.1 == file.image,
                "speculation live source drift; integration refused"
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn directory(&self) -> &Path {
        &self.0.storage.directory
    }

    pub(crate) fn reserve_controller_records(
        &self,
        owner: &str,
        count: usize,
        metadata_bytes: usize,
        image_bytes: usize,
    ) -> Result<ControllerReservation> {
        self.reserve_controller_records_bounded(
            owner,
            count,
            metadata_bytes,
            image_bytes,
            MAX_STORE_BYTES,
        )
    }

    /// Lifecycle receipts have no image capability; reserve only their envelope and metadata blob.
    pub(crate) fn reserve_controller_metadata(
        &self,
        owner: &str,
        count: usize,
        metadata_bytes: usize,
    ) -> Result<ControllerReservation> {
        self.reserve_controller_capacity(owner, count, metadata_bytes, 0, 0, MAX_STORE_BYTES)
    }

    // Production always reserves against 64 MiB; small fixtures exercise the same admission path.
    fn reserve_controller_records_bounded(
        &self,
        owner: &str,
        count: usize,
        metadata_bytes: usize,
        image_bytes: usize,
        expansion_limit: usize,
    ) -> Result<ControllerReservation> {
        self.reserve_controller_capacity(
            owner,
            count,
            metadata_bytes,
            image_bytes,
            MAX_CONTROLLER_IMAGES,
            expansion_limit,
        )
    }

    fn reserve_controller_capacity(
        &self,
        owner: &str,
        count: usize,
        metadata_bytes: usize,
        image_bytes: usize,
        images_remaining: usize,
        expansion_limit: usize,
    ) -> Result<ControllerReservation> {
        check_owner(owner)?;
        ensure!(
            count > 0 && count <= MAX_CONTROLLER_RECORDS,
            "speculation controller reservation count invalid"
        );
        ensure!(
            metadata_bytes <= count.saturating_mul(MAX_META_BYTES)
                && image_bytes <= MAX_STORE_BYTES,
            "speculation controller reservation exceeds bounds"
        );
        let bytes = metadata_bytes
            .checked_add(image_bytes)
            .context("speculation reservation overflow")?;
        let slots = count
            .checked_mul(2)
            .and_then(|slots| slots.checked_add(images_remaining))
            .context("speculation reservation slot overflow")?;
        let mut disk = self.0.lock_disk()?;
        let (_, expanded) = inspect_records_with_expansion(&self.0.storage, &disk, self.id())?;
        // All outstanding allowances include each image occurrence, even if blobs deduplicate.
        // Check future receipt/verification capacity here, not after live effects at publication.
        let remaining = disk
            .reserved_bytes
            .checked_add(bytes)
            .context("speculation reservation overflow")?;
        checked_inspection_expansion(expanded, remaining, expansion_limit)?;
        // Synthetic names reserve bytes once, independently of content-addressed dedup.
        self.0
            .storage
            .reserve(&disk, &[("controller-reservation", bytes)], count)?;
        ensure!(
            disk.objects
                .len()
                .saturating_add(disk.reserved_objects)
                .saturating_add(slots)
                <= MAX_OBJECTS,
            "speculation controller object reservation exceeds capacity"
        );
        disk.reserved_bytes += bytes;
        disk.reserved_records += count;
        disk.reserved_objects += slots;
        Ok(ControllerReservation {
            base: self.0.clone(),
            owner: owner.to_owned(),
            records: count,
            metadata_bytes,
            image_bytes,
            object_slots: slots,
            images_remaining,
        })
    }

    pub(crate) fn controller_record(
        &self,
        owner: &str,
        label: &str,
        metadata: &[u8],
        images: &[&[u8]],
    ) -> Result<StoredRecordId> {
        let encoded = controller_metadata(self.id(), owner, label, metadata, images)?;
        let image_bytes = images
            .iter()
            .try_fold(0usize, |total, bytes| total.checked_add(bytes.len()))
            .context("speculation controller image size overflow")?;
        let mut reservation = if images.is_empty() {
            self.reserve_controller_metadata(owner, 1, encoded.len() + metadata.len())?
        } else {
            self.reserve_controller_records(owner, 1, encoded.len() + metadata.len(), image_bytes)?
        };
        self.controller_record_reserved(&mut reservation, label, metadata, images)
    }

    pub(crate) fn controller_record_reserved(
        &self,
        reservation: &mut ControllerReservation,
        label: &str,
        metadata: &[u8],
        images: &[&[u8]],
    ) -> Result<StoredRecordId> {
        ensure!(
            Arc::ptr_eq(&reservation.base, &self.0),
            "speculation reservation belongs to another base"
        );
        ensure!(
            reservation.records > 0,
            "speculation controller reservation exhausted"
        );
        let encoded = controller_metadata(self.id(), &reservation.owner, label, metadata, images)?;
        let meta_bytes = encoded
            .len()
            .checked_add(metadata.len())
            .context("speculation controller metadata size overflow")?;
        let image_bytes = images
            .iter()
            .try_fold(0usize, |total, bytes| total.checked_add(bytes.len()))
            .context("speculation controller image size overflow")?;
        ensure!(
            meta_bytes <= reservation.metadata_bytes
                && image_bytes <= reservation.image_bytes
                && images.len() <= reservation.images_remaining,
            "speculation controller record exceeds admitted capacity"
        );
        let mut disk = self.0.lock_disk()?;
        // Temporarily remove this reservation from global accounting while consuming it. The
        // disk mutex excludes all other publication; remaining capacity is restored atomically.
        disk.reserved_bytes -= reservation.metadata_bytes + reservation.image_bytes;
        disk.reserved_records -= reservation.records;
        disk.reserved_objects -= reservation.object_slots;
        let result = (|| {
            for bytes in images {
                self.0
                    .storage
                    .publish_blob(&mut disk, &Image::bytes(bytes), bytes)?;
            }
            self.0
                .storage
                .publish_blob(&mut disk, &Image::bytes(metadata), metadata)?;
            let id = self.0.storage.publish_manifest(&mut disk, &encoded)?;
            Ok(StoredRecordId(id))
        })();
        if result.is_ok() {
            reservation.records -= 1;
            reservation.metadata_bytes -= meta_bytes;
            reservation.image_bytes -= image_bytes;
            reservation.images_remaining -= images.len();
            // A reservation also includes metadata blobs. Consume only actually-new slots;
            // conservative excess remains reserved until the capability is dropped.
            reservation.object_slots = reservation.object_slots.saturating_sub(images.len() + 2);
        } else {
            disk.poisoned = true;
        }
        disk.reserved_bytes += reservation.metadata_bytes + reservation.image_bytes;
        disk.reserved_records += reservation.records;
        disk.reserved_objects += reservation.object_slots;
        result
    }

    /// Data-only inspection while this controller still owns the cohort. Restart inspection is
    /// deliberately separate and must never be used to manufacture a BaseSnapshot/OverlayHandle.
    pub(crate) fn inspect_controller_records(&self) -> Result<Vec<InspectedRecord>> {
        let mut disk = self.0.lock_disk()?;
        let result = inspect_records(&self.0.storage, &disk, self.id());
        if result.is_err() {
            disk.poisoned = true;
        }
        result
    }

    pub(crate) fn image(&self, resource: &NativeResource) -> Result<Option<Vec<u8>>> {
        let file = self
            .0
            .files
            .get(resource.key())
            .context("speculation resource not captured")?;
        ensure!(
            file.resource == *resource,
            "speculation base resource binding mismatch"
        );
        let mut disk = self.0.lock_disk()?;
        self.0.read_image(&mut disk, &file.image)
    }
}

impl BaseInner {
    fn lock_disk(&self) -> Result<MutexGuard<'_, DiskState>> {
        let mut disk = self
            .disk
            .lock()
            .map_err(|_| anyhow::anyhow!("speculation ownership poisoned"))?;
        ensure!(
            !disk.poisoned,
            "speculation storage is uncertain; discard this cohort"
        );
        if self.storage.verify(&disk).is_err() {
            disk.poisoned = true;
            bail!("speculation storage identity/integrity changed; discard this cohort");
        }
        Ok(disk)
    }
    fn read_image(&self, disk: &mut DiskState, image: &Image) -> Result<Option<Vec<u8>>> {
        let Some(name) = image.blob_name() else {
            return Ok(None);
        };
        let result = (|| {
            let bytes = self.storage.read_object(disk, &name)?;
            ensure!(
                Image::bytes(&bytes) == *image,
                "speculation blob fingerprint mismatch"
            );
            Ok(Some(bytes))
        })();
        if result.is_err() {
            disk.poisoned = true;
        }
        result
    }
}

impl OverlayHandle {
    pub(crate) fn owner(&self) -> &str {
        &self.0.owner
    }
    pub(crate) fn base_id(&self) -> &str {
        &self.0.base.id
    }
    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn scope(&self) -> &NativeWriteScope {
        &self.0.scope
    }
    pub(crate) fn lifecycle(&self) -> Option<&super::lifecycle::AttemptLifecycle> {
        self.0.lifecycle.get()
    }
    pub(crate) fn bind_lifecycle(
        &self,
        lifecycle: super::lifecycle::AttemptLifecycle,
    ) -> Result<()> {
        lifecycle.check_overlay(self.base_id(), self.id(), self.owner())?;
        self.check_scope(lifecycle.scope())?;
        let state = self.lock_state()?;
        ensure!(
            !state.attempt_admitted && !state.sealed && state.active_bodies == 0,
            "worker roster must precede attempt admission"
        );
        self.0
            .lifecycle
            .set(lifecycle)
            .map_err(|_| anyhow::anyhow!("overlay worker roster already frozen"))
    }
    pub(crate) fn generation(&self) -> Result<u64> {
        let state = self.lock_state()?;
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        Ok(state.generation)
    }
    pub(crate) fn validate(&self, file: &OverlayFile, writable: bool) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&file.overlay, &self.0),
            "speculation file belongs to another overlay"
        );
        ensure!(
            !writable || file.is_writable(),
            "speculation file is read-only"
        );
        let state = self.lock_state()?;
        ensure!(!state.sealed, "speculation overlay is sealed");
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        Ok(())
    }

    pub(crate) fn check_file(&self, file: &OverlayFile) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&file.overlay, &self.0),
            "speculation file belongs to another owner/base/overlay"
        );
        Ok(())
    }

    /// Exact immutable authority comparison; never resolves the current live tree.
    pub(crate) fn check_scope(&self, scope: &NativeWriteScope) -> Result<()> {
        ensure!(
            self.0.scope.same_authority(scope)
                && self
                    .0
                    .scope
                    .resources()
                    .iter()
                    .zip(scope.resources())
                    .all(|(left, right)| left.path() == right.path()),
            "speculation attempt requires the complete exact sealed scope"
        );
        Ok(())
    }

    pub(crate) fn check_owner_scope(&self, owner: &str, scope: &NativeWriteScope) -> Result<()> {
        ensure!(
            owner == self.0.owner && sealed_narrowing(&self.0.scope, scope),
            "speculation owner/scope widens sealed overlay authority"
        );
        Ok(())
    }

    /// Pure lexical lookup. No canonicalize, stat, scope.verify, or source read may be added here.
    pub(crate) fn resolve(&self, root: &Path, logical: &Path) -> Result<OverlayFile> {
        ensure!(
            root.is_absolute() && path_key(root)? == self.0.base.root_key,
            "speculation logical root differs from captured workspace"
        );
        NativeResource::validate_spelling(logical)?;
        let path = if logical.is_absolute() {
            logical.to_path_buf()
        } else {
            root.join(logical)
        };
        let key = self
            .0
            .base
            .paths
            .get(&path_key(&path)?)
            .context("speculation path outside captured exact-file set")?;
        ensure!(
            self.0
                .scope
                .resources()
                .iter()
                .any(|resource| resource.key() == key),
            "speculation path outside child scope"
        );
        Ok(OverlayFile {
            overlay: self.0.clone(),
            key: key.clone(),
        })
    }

    /// One volatile native owner per overlay. Restart records cannot re-open this admission.
    pub(crate) fn admit_attempt(&self, owner: &str, scope: &NativeWriteScope) -> Result<()> {
        self.check_owner_scope(owner, scope)?;
        self.check_scope(scope)?;
        let mut state = self.lock_state()?;
        ensure!(
            !state.sealed
                && !state.attempt_admitted
                && !state.bodies_closed
                && state.active_bodies == 0,
            "speculation overlay attempt already admitted or unsettled"
        );
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        state.attempt_admitted = true;
        Ok(())
    }

    /// Latch body admission closed before releasing the native attempt's charged resources.
    pub(crate) fn finish_attempt(&self, owner: &str) -> Result<u64> {
        ensure!(owner == self.0.owner, "speculation attempt owner mismatch");
        let mut state = self.lock_state()?;
        ensure!(
            state.attempt_admitted
                && !state.bodies_closed
                && !state.sealed
                && state.active_bodies == 0,
            "speculation attempt still owns private bodies or already closed"
        );
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        state.bodies_closed = true;
        Ok(state.generation)
    }

    pub(crate) fn body_lease(&self, owner: &str) -> Result<OverlayBodyLease> {
        ensure!(owner == self.0.owner, "speculation body owner mismatch");
        let mut state = self.lock_state()?;
        ensure!(
            !state.sealed && !state.bodies_closed,
            "speculation overlay body admission is closed"
        );
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        ensure!(
            state.active_bodies < MAX_RECORDS,
            "speculation active body count exceeds limit"
        );
        state.active_bodies += 1;
        Ok(OverlayBodyLease(Arc::new(BodyOwnership {
            overlay: self.0.clone(),
        })))
    }

    pub(crate) fn snapshot(
        &self,
        file: &OverlayFile,
        body: &OverlayBodyLease,
    ) -> Result<OverlayRead> {
        self.check_handles(file, body)?;
        let mut state = self.lock_state()?;
        ensure!(!state.sealed, "speculation overlay is sealed");
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        let version = state
            .files
            .get(&file.key)
            .context("speculation file binding missing")?
            .clone();
        let bytes = self.0.base.read_image(&mut disk, &version.image)?;
        if !state
            .observations
            .iter()
            .any(|r| r.key == file.key && r.generation == version.generation)
        {
            ensure!(
                state.observations.len() < MAX_OBSERVATIONS,
                "speculation read-set exceeds limit"
            );
            state.observations.push(Observation {
                key: file.key.clone(),
                generation: version.generation,
                image: version.image.clone(),
            });
            // Any persistence failure poisons the entire cohort, including this in-memory addition.
            self.save_state(&mut disk, &mut state)?;
        }
        Ok(self.receipt(file, &version, bytes))
    }

    /// Recheck an already-recorded read without adding an observation or reading live source.
    /// Prepared/dry-run operations must use this to reject a stale overlay generation too.
    pub(crate) fn validate_snapshot(
        &self,
        file: &OverlayFile,
        read: &OverlayRead,
        body: &OverlayBodyLease,
    ) -> Result<()> {
        self.check_handles(file, body)?;
        ensure!(
            Arc::ptr_eq(&read.overlay, &self.0) && read.key == file.key,
            "speculation snapshot belongs to another owner/base/overlay/file"
        );
        let state = self.lock_state()?;
        ensure!(!state.sealed, "speculation overlay is sealed");
        let version = state
            .files
            .get(&file.key)
            .context("speculation snapshot file missing")?;
        ensure!(
            version.generation == read.generation
                && version.image.fingerprint() == read.fingerprint,
            "speculation snapshot generation/fingerprint changed"
        );
        ensure!(
            state.observations.iter().any(|r| r.key == read.key
                && r.generation == read.generation
                && r.image.fingerprint() == read.fingerprint),
            "speculation snapshot was not observed"
        );
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        self.0.base.read_image(&mut disk, &version.image)?;
        Ok(())
    }

    /// CAS requires both an unforgeable observed-generation receipt and private durable read-set.
    /// A successful write does NOT grant a new read receipt; explicitly snapshot again to edit again.
    pub(crate) fn compare_write(
        &self,
        file: &OverlayFile,
        expected: &OverlayRead,
        bytes: &[u8],
        provenance: WriteProvenance,
        body: &OverlayBodyLease,
    ) -> Result<u64> {
        self.check_handles(file, body)?;
        ensure!(
            Arc::ptr_eq(&expected.overlay, &self.0) && expected.key == file.key,
            "speculation CAS receipt belongs to another owner/base/overlay/file"
        );
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "speculation file exceeds 4 MiB"
        );
        ensure!(
            self.0.scope.write_keys().iter().any(|key| key == &file.key),
            "speculation file is read-only"
        );
        let mut state = self.lock_state()?;
        ensure!(!state.sealed, "speculation overlay is sealed");
        let version = state
            .files
            .get(&file.key)
            .context("speculation file binding missing")?
            .clone();
        ensure!(
            version.generation == expected.generation
                && version.image.fingerprint() == expected.fingerprint,
            "speculation CAS generation/fingerprint conflict; re-read the overlay"
        );
        ensure!(
            state.observations.iter().any(|r| r.key == file.key
                && r.generation == expected.generation
                && r.image.fingerprint() == expected.fingerprint),
            "speculation CAS requires a recorded read"
        );
        verify_provenance(expected.bytes(), bytes, provenance)?;
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        // Do not let even a no-op CAS succeed after a stored preimage was damaged or replaced.
        self.0.base.read_image(&mut disk, &version.image)?;
        let next_image = Image::bytes(bytes);
        if next_image == version.image {
            return Ok(version.generation);
        }
        ensure!(
            state.edits.len() < MAX_RECORDS,
            "speculation edit record count exceeds limit"
        );
        let generation = state
            .generation
            .checked_add(1)
            .context("speculation generation exhausted")?;
        // Reserve both the blob and FULL metadata before publishing either. Metadata is bounded.
        let edit = Edit {
            key: file.key.clone(),
            generation,
            before: version.image,
            after: next_image.clone(),
            provenance,
        };
        let next = Version {
            generation,
            image: next_image.clone(),
        };
        let old_generation = state.generation;
        state.generation = generation;
        let old_version = state
            .files
            .insert(file.key.clone(), next)
            .context("speculation missing CAS target")?;
        state.edits.push(edit);
        let encoded = overlay_metadata(
            &self.0.base,
            &self.0.id,
            &self.0.owner,
            &self.0.scope,
            &state,
        );
        // Validation/quota failure is known not to have written; restore memory and preserve authority.
        let preflight = encoded.and_then(|meta| {
            self.0
                .base
                .storage
                .reserve_publication(&disk, &next_image, bytes, &meta)?;
            Ok(meta)
        });
        let meta = match preflight {
            Ok(meta) => meta,
            Err(error) => {
                state.edits.pop();
                state.files.insert(file.key.clone(), old_version);
                state.generation = old_generation;
                return Err(error);
            }
        };
        self.0
            .base
            .storage
            .publish_blob(&mut disk, &next_image, bytes)?;
        state.manifest = self.0.base.storage.publish_manifest(&mut disk, &meta)?;
        Ok(generation)
    }

    pub(crate) fn seal(&self, owner: &str) -> Result<SealedOverlay> {
        ensure!(owner == self.0.owner, "speculation seal owner mismatch");
        let mut state = self.lock_state()?;
        ensure!(
            state.active_bodies == 0,
            "speculation seal refused while physical workers are live"
        );
        let mut disk = self.0.base.lock_disk()?;
        self.verify_manifest(&mut disk, &state)?;
        if !state.sealed {
            state.sealed = true;
            self.save_state(&mut disk, &mut state)?;
        }
        Ok(SealedOverlay(self.0.clone()))
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, OverlayState>> {
        self.0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("speculation overlay ownership poisoned"))
    }
    fn check_handles(&self, file: &OverlayFile, body: &OverlayBodyLease) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&file.overlay, &self.0) && Arc::ptr_eq(&body.0.overlay, &self.0),
            "speculation body/file belongs to another owner/base/overlay"
        );
        Ok(())
    }
    fn receipt(
        &self,
        file: &OverlayFile,
        version: &Version,
        bytes: Option<Vec<u8>>,
    ) -> OverlayRead {
        OverlayRead {
            overlay: self.0.clone(),
            key: file.key.clone(),
            generation: version.generation,
            fingerprint: version.image.fingerprint(),
            bytes,
        }
    }
    fn verify_manifest(&self, disk: &mut DiskState, state: &OverlayState) -> Result<()> {
        let result = self.0.base.storage.verify_object(disk, &state.manifest);
        if result.is_err() {
            disk.poisoned = true;
        }
        result
    }
    fn save_state(&self, disk: &mut DiskState, state: &mut OverlayState) -> Result<()> {
        let result = (|| {
            let bytes = overlay_metadata(
                &self.0.base,
                &self.0.id,
                &self.0.owner,
                &self.0.scope,
                state,
            )?;
            state.manifest = self.0.base.storage.publish_manifest(disk, &bytes)?;
            Ok(())
        })();
        if result.is_err() {
            disk.poisoned = true;
        }
        result
    }
}

impl SealedOverlay {
    pub(crate) fn owner(&self) -> &str {
        &self.0.owner
    }
    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn belongs_to(&self, base: &BaseSnapshot) -> bool {
        Arc::ptr_eq(&self.0.base, &base.0)
    }

    pub(crate) fn manifest_identity(&self) -> Result<SealedManifestIdentity> {
        let handle = OverlayHandle(self.0.clone());
        let state = handle.lock_state()?;
        ensure!(state.sealed, "speculation overlay is not sealed");
        ensure!(
            state.active_bodies == 0,
            "speculation overlay is not quiescent"
        );
        let mut disk = self.0.base.lock_disk()?;
        handle.verify_manifest(&mut disk, &state)?;
        let bytes = self.0.base.storage.read_object(&disk, &state.manifest)?;
        ensure!(
            manifest_name(&bytes) == state.manifest,
            "speculation sealed manifest identity changed"
        );
        let manifest = decode_sealed_manifest(&bytes)?;
        validate_sealed_manifest(&self.0, &state, &manifest)?;
        let identity = SealedManifestIdentity {
            base: self.0.base.id.clone(),
            overlay: self.0.id.clone(),
            owner: self.0.owner.clone(),
            generation: state.generation,
            manifest: state.manifest.clone(),
        };
        identity.validate()?;
        Ok(identity)
    }

    pub(crate) fn observations(&self) -> Result<Vec<ReadObservation>> {
        let handle = OverlayHandle(self.0.clone());
        let state = handle.lock_state()?;
        ensure!(
            state.sealed && state.active_bodies == 0,
            "speculation overlay not quiescent/sealed"
        );
        let mut disk = self.0.base.lock_disk()?;
        handle.verify_manifest(&mut disk, &state)?;
        Ok(state
            .observations
            .iter()
            .map(|read| ReadObservation {
                resource: self.0.base.files[&read.key].resource.clone(),
                generation: read.generation,
                fingerprint: read.image.fingerprint(),
            })
            .collect())
    }

    pub(crate) fn changes(&self) -> Result<Vec<OverlayChange>> {
        let handle = OverlayHandle(self.0.clone());
        let state = handle.lock_state()?;
        ensure!(
            state.sealed && state.active_bodies == 0,
            "speculation overlay not quiescent/sealed"
        );
        let mut disk = self.0.base.lock_disk()?;
        handle.verify_manifest(&mut disk, &state)?;
        let mut result = Vec::new();
        let mut images: BTreeMap<String, Arc<[u8]>> = BTreeMap::new();
        for (key, version) in &state.files {
            let bound = &self.0.base.files[key];
            if version.image == bound.image {
                continue;
            }
            let base = shared_image(&self.0.base, &mut disk, &mut images, &bound.image)?;
            let after = shared_image(&self.0.base, &mut disk, &mut images, &version.image)?
                .context("speculation deletion is unsupported")?;
            let mut edits = Vec::new();
            for edit in state.edits.iter().filter(|edit| &edit.key == key) {
                edits.push(OverlayEdit {
                    generation: edit.generation,
                    before: edit.before.fingerprint(),
                    after: shared_image(&self.0.base, &mut disk, &mut images, &edit.after)?
                        .context("speculation invalid edit image")?,
                    provenance: edit.provenance,
                });
            }
            result.push(OverlayChange {
                resource: bound.resource.clone(),
                base,
                after,
                generation: version.generation,
                edits,
            });
        }
        Ok(result)
    }
}

fn deserialize_bounded_vec<'de, D, T, const LIMIT: usize>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> serde::de::Visitor<'de> for Bounded<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("bounded overlay evidence sequence")
        }
        fn visit_seq<S>(self, mut sequence: S) -> std::result::Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == LIMIT {
                    return Err(serde::de::Error::custom(
                        "overlay evidence sequence exceeds bound",
                    ));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, LIMIT>(std::marker::PhantomData))
}

fn deserialize_bindings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<ManifestBindingOwned>, D::Error> {
    deserialize_bounded_vec::<D, _, MAX_CHILD_FILES>(deserializer)
}
fn deserialize_observations<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Observation>, D::Error> {
    deserialize_bounded_vec::<D, _, MAX_OBSERVATIONS>(deserializer)
}
fn deserialize_edits<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Edit>, D::Error> {
    deserialize_bounded_vec::<D, _, MAX_RECORDS>(deserializer)
}

fn deserialize_versions<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, Version>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Versions;
    impl<'de> serde::de::Visitor<'de> for Versions {
        type Value = BTreeMap<String, Version>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("bounded unique overlay file versions")
        }
        fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut files = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if files.len() >= MAX_CHILD_FILES || files.contains_key(&key) {
                    return Err(serde::de::Error::custom(
                        "overlay file count exceeded or key duplicated",
                    ));
                }
                files.insert(key, map.next_value()?);
            }
            Ok(files)
        }
    }
    deserializer.deserialize_map(Versions)
}

fn decode_sealed_manifest(bytes: &[u8]) -> Result<SealedOverlayManifest> {
    ensure!(
        bytes.len() <= MAX_META_BYTES,
        "speculation sealed manifest exceeds byte bound"
    );
    let manifest: SealedOverlayManifest = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("speculation sealed manifest is malformed"))?;
    // Our manifests are canonical encoder output. This also refuses duplicate JSON keys rather
    // than allowing a deserializer's last-value-wins behavior to change historical meaning.
    ensure!(
        encode_meta(&manifest)? == bytes,
        "speculation sealed manifest encoding is not canonical"
    );
    validate_manifest_state(&manifest)?;
    Ok(manifest)
}

fn validate_manifest_state(manifest: &SealedOverlayManifest) -> Result<()> {
    validate_overlay_identity(
        &manifest.base,
        &manifest.overlay,
        &manifest.owner,
        manifest.generation,
    )?;
    ensure!(
        manifest.format == FORMAT && manifest.kind == "overlay" && manifest.sealed,
        "speculation sealed manifest lifecycle invalid"
    );
    ensure!(
        !manifest.bindings.is_empty()
            && manifest.bindings.len() <= MAX_CHILD_FILES
            && manifest.files.len() == manifest.bindings.len()
            && manifest.observations.len() <= MAX_OBSERVATIONS
            && manifest.edits.len() <= MAX_RECORDS
            && manifest.generation == manifest.edits.len() as u64,
        "speculation sealed manifest collection/generation exceeds bound"
    );
    let image_valid = |image: &Image| {
        image.byte_len <= MAX_FILE_BYTES as u64 && (image.exists || *image == Image::missing())
    };
    let mut versions = BTreeMap::new();
    let mut paths = BTreeSet::new();
    for binding in &manifest.bindings {
        ensure!(
            !binding.key.is_empty()
                && binding.key.len() <= 128
                && !binding.key.chars().any(char::is_control)
                && binding.path.is_absolute()
                && paths.insert(path_key(&binding.path)?)
                && image_valid(&binding.base)
                && versions
                    .insert(
                        binding.key.clone(),
                        Version {
                            generation: 0,
                            image: binding.base.clone()
                        }
                    )
                    .is_none(),
            "speculation sealed manifest binding invalid or duplicated"
        );
    }
    let mut seen_versions = BTreeMap::new();
    let mut required_reads = BTreeSet::new();
    for (key, version) in &versions {
        seen_versions.insert((key.clone(), 0), version.image.clone());
    }
    for (index, edit) in manifest.edits.iter().enumerate() {
        let version = versions
            .get_mut(&edit.key)
            .context("speculation sealed edit binding missing")?;
        required_reads.insert((edit.key.clone(), version.generation));
        ensure!(
            edit.generation == index as u64 + 1
                && edit.before == version.image
                && edit.after.exists
                && image_valid(&edit.after)
                && edit.after != edit.before
                && manifest
                    .bindings
                    .iter()
                    .any(|binding| binding.key == edit.key && binding.writable),
            "speculation sealed edit history invalid"
        );
        if let WriteProvenance::Localized { start, end } = edit.provenance {
            ensure!(
                edit.before.exists && start <= end && end as u64 <= edit.before.byte_len,
                "speculation sealed edit provenance invalid"
            );
        }
        *version = Version {
            generation: edit.generation,
            image: edit.after.clone(),
        };
        seen_versions.insert((edit.key.clone(), edit.generation), edit.after.clone());
    }
    ensure!(
        versions == manifest.files,
        "speculation sealed final versions differ from history"
    );
    let mut observations = BTreeSet::new();
    for observation in &manifest.observations {
        let key = (observation.key.clone(), observation.generation);
        ensure!(
            seen_versions.get(&key) == Some(&observation.image) && observations.insert(key),
            "speculation sealed observation differs from history"
        );
    }
    ensure!(
        required_reads.is_subset(&observations),
        "speculation sealed edit preimage was not observed"
    );
    Ok(())
}

/// Parse versioned historical references only. Legacy absence is not upgraded to sealed evidence.
pub(crate) fn sealed_lineage(
    metadata: &serde_json::Value,
) -> Result<Option<Vec<SealedManifestIdentity>>> {
    let Some(marker) = metadata.get("overlay_lineage_version") else {
        if let Some(overlays) = metadata
            .get("overlays")
            .and_then(serde_json::Value::as_array)
        {
            ensure!(
                overlays
                    .iter()
                    .all(|overlay| overlay.get("manifest").is_none()
                        && overlay.get("generation").is_none()),
                "unversioned sealed overlay lineage"
            );
        }
        return Ok(None);
    };
    ensure!(
        marker.as_u64() == Some(1),
        "unsupported sealed overlay lineage version"
    );
    let base = metadata
        .get("base")
        .and_then(serde_json::Value::as_str)
        .context("sealed lineage base missing")?;
    let parent = metadata
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .context("sealed lineage owner missing")?;
    check_owner(parent)?;
    let overlays = metadata
        .get("overlays")
        .and_then(serde_json::Value::as_array)
        .context("sealed lineage overlays missing")?;
    ensure!(
        !overlays.is_empty() && overlays.len() <= MAX_OVERLAYS,
        "sealed lineage count invalid"
    );
    let mut identities = Vec::with_capacity(overlays.len());
    let mut ids = BTreeSet::new();
    for overlay in overlays {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Lineage {
            base: String,
            owner: String,
            id: String,
            generation: u64,
            manifest: String,
            author: String,
        }
        let lineage: Lineage = serde_json::from_value(overlay.clone())?;
        let identity = SealedManifestIdentity {
            base: lineage.base,
            overlay: lineage.id,
            owner: lineage.owner,
            generation: lineage.generation,
            manifest: lineage.manifest,
        };
        identity.validate()?;
        ensure!(
            identity.base == base
                && (identity.owner == parent
                    || identity
                        .owner
                        .strip_prefix(parent)
                        .is_some_and(|tail| tail.starts_with('/')))
                && lineage.author == format!("{}/{}", identity.owner, identity.overlay)
                && ids.insert(identity.overlay.clone()),
            "sealed lineage identity differs or is duplicated"
        );
        identities.push(identity);
    }
    Ok(Some(identities))
}

fn validate_sealed_manifest(
    inner: &OverlayInner,
    state: &OverlayState,
    manifest: &SealedOverlayManifest,
) -> Result<()> {
    ensure!(
        manifest.format.len() <= 64 && manifest.kind.len() <= 64,
        "speculation sealed manifest tags exceed bound"
    );
    ensure!(
        manifest.base.len() <= 4096 && manifest.overlay.len() <= 4096,
        "speculation sealed manifest IDs exceed bound"
    );
    check_owner(&manifest.owner)?;
    ensure!(
        manifest.bindings.len() <= MAX_CHILD_FILES
            && manifest.files.len() <= MAX_CHILD_FILES
            && manifest.observations.len() <= MAX_OBSERVATIONS
            && manifest.edits.len() <= MAX_RECORDS,
        "speculation sealed manifest collection exceeds bound"
    );
    ensure!(
        manifest.format == FORMAT && manifest.kind == "overlay",
        "speculation sealed manifest kind invalid"
    );
    ensure!(
        manifest.base == inner.base.id
            && manifest.overlay == inner.id
            && manifest.owner == inner.owner,
        "speculation sealed manifest identity does not match the live handle"
    );
    ensure!(
        manifest.generation == state.generation && manifest.sealed && state.sealed,
        "speculation sealed manifest lifecycle does not match the live state"
    );
    ensure!(
        manifest.files == state.files
            && manifest.observations == state.observations
            && manifest.edits == state.edits,
        "speculation sealed manifest contents do not match the live state"
    );
    ensure!(
        manifest.bindings.len() == inner.scope.resources().len(),
        "speculation sealed manifest binding count changed"
    );
    for resource in inner.scope.resources() {
        let binding = manifest
            .bindings
            .iter()
            .find(|binding| binding.key == resource.key())
            .context("speculation sealed manifest binding missing")?;
        ensure!(
            binding.path == resource.path()
                && binding.base == inner.base.files[resource.key()].image
                && binding.writable
                    == inner
                        .scope
                        .write_keys()
                        .iter()
                        .any(|key| key == resource.key()),
            "speculation sealed manifest binding changed"
        );
    }
    Ok(())
}

fn shared_image(
    base: &BaseInner,
    disk: &mut DiskState,
    images: &mut BTreeMap<String, Arc<[u8]>>,
    image: &Image,
) -> Result<Option<Arc<[u8]>>> {
    let Some(name) = image.blob_name() else {
        return Ok(None);
    };
    if let Some(bytes) = images.get(&name) {
        return Ok(Some(bytes.clone()));
    }
    let bytes: Arc<[u8]> = base
        .read_image(disk, image)?
        .context("speculation image missing")?
        .into();
    images.insert(name, bytes.clone());
    Ok(Some(bytes))
}

fn overlay_metadata(
    base: &BaseInner,
    id: &str,
    owner: &str,
    scope: &NativeWriteScope,
    state: &OverlayState,
) -> Result<Vec<u8>> {
    encode_meta(&OverlayManifest {
        format: FORMAT,
        kind: "overlay",
        base: &base.id,
        overlay: id,
        owner,
        generation: state.generation,
        sealed: state.sealed,
        bindings: scope
            .resources()
            .iter()
            .map(|resource| ManifestBinding {
                key: resource.key(),
                path: resource.path(),
                base: &base.files[resource.key()].image,
                writable: scope.write_keys().iter().any(|key| key == resource.key()),
            })
            .collect(),
        files: &state.files,
        observations: &state.observations,
        edits: &state.edits,
    })
}

fn verify_provenance(
    before: Option<&[u8]>,
    after: &[u8],
    provenance: WriteProvenance,
) -> Result<()> {
    if let WriteProvenance::Localized { start, end } = provenance {
        let before = before.context("speculation localized edit requires an existing preimage")?;
        ensure!(
            start <= end && end <= before.len(),
            "speculation localized range is invalid"
        );
        let suffix = before.len() - end;
        ensure!(
            after.len() >= start.saturating_add(suffix)
                && before[..start] == after[..start]
                && before[end..] == after[after.len() - suffix..],
            "speculation localized provenance changed bytes outside its range"
        );
    }
    Ok(())
}

fn check_owner(owner: &str) -> Result<()> {
    ensure!(
        !owner.is_empty() && owner.len() <= MAX_OWNER_BYTES && !owner.chars().any(char::is_control),
        "speculation requires a bounded nonempty exact owner"
    );
    Ok(())
}

fn check_scope_size(scope: &NativeWriteScope) -> Result<()> {
    ensure!(
        !scope.resources().is_empty() && scope.resources().len() <= MAX_CHILD_FILES,
        "speculation child requires 1..=32 exact files"
    );
    Ok(())
}

fn sealed_narrowing(parent: &NativeWriteScope, child: &NativeWriteScope) -> bool {
    child
        .resources()
        .iter()
        .all(|r| parent.resources().iter().any(|p| p == r))
        && child
            .write_keys()
            .iter()
            .all(|key| parent.write_keys().contains(key))
}

fn check_capture_leases(
    identity: &WorkspaceIdentity,
    writer: &WorkspaceWriterLease,
    physical: &PhysicalEffectLease,
) -> Result<()> {
    ensure!(
        physical.is_exclusive(),
        "speculation capture/integration requires exclusive physical admission"
    );
    ensure!(
        writer.identity() == identity,
        "speculation capture/integration writer lease identity mismatch"
    );
    ensure!(
        identity.canonical_root.is_absolute()
            && !identity.repo_id.is_empty()
            && !identity.worktree_id.is_empty(),
        "speculation requires explicit canonical workspace identity"
    );
    Ok(())
}

fn validate_source(
    identity: &WorkspaceIdentity,
    store_root: &Path,
    resource: &NativeResource,
) -> Result<()> {
    resource.verify()?;
    let path = resource.path();
    let relative = path
        .strip_prefix(&identity.canonical_root)
        .context("speculation resource is outside workspace root")?;
    ensure!(
        !relative.as_os_str().is_empty(),
        "speculation resource cannot be workspace root"
    );
    for component in relative.components() {
        let text = component
            .as_os_str()
            .to_str()
            .context("speculation non-Unicode source binding")?;
        ensure!(
            ![".git", ".aizen", "native-journal-v1", NAMESPACE]
                .iter()
                .any(|name| text.eq_ignore_ascii_case(name)),
            "speculation refuses internal managed source paths"
        );
    }
    for internal in [
        Some(store_root),
        identity.canonical_git_dir.as_deref(),
        identity.canonical_common_git_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            !lexically_within(path, internal)?,
            "speculation refuses internal storage source paths"
        );
    }
    Ok(())
}

fn path_key(path: &Path) -> Result<String> {
    let text = path.to_str().context("speculation path must be Unicode")?;
    ensure!(
        !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control),
        "speculation path exceeds bound"
    );
    // Keep identical normalization to NativeResource identity, without consulting the filesystem.
    #[cfg(windows)]
    let text = crate::core::config::normalize_path_key(path);
    #[cfg(not(windows))]
    let text = text.to_owned();
    #[cfg(any(windows, target_os = "macos"))]
    {
        ensure!(
            text.is_ascii(),
            "speculation ambiguous non-ASCII path on a case-folding host"
        );
        Ok(text.to_ascii_lowercase())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(text)
    }
}

fn lexically_within(path: &Path, root: &Path) -> Result<bool> {
    let path = path_key(path)?;
    let root = path_key(root)?;
    Ok(path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|suffix| suffix.starts_with(['/', '\\'])))
}

fn read_live(resource: &NativeResource) -> Result<(Option<Vec<u8>>, Image)> {
    resource.verify()?;
    match fs::symlink_metadata(resource.path()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            resource.verify()?;
            // Verify again: a concurrent creation must not be silently captured as missing.
            ensure!(
                matches!(fs::symlink_metadata(resource.path()), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "speculation source changed while capturing missing image"
            );
            Ok((None, Image::missing()))
        }
        Err(_) => bail!("speculation source metadata unreadable"),
        Ok(md) => {
            ensure!(
                md.is_file() && !is_link_or_reparse(&md) && md.len() <= MAX_FILE_BYTES as u64,
                "speculation source must be a non-link file of at most 4 MiB"
            );
            let before = NodeStamp::capture(resource.path(), &md, false)?;
            let bytes = read_existing_regular(resource.path(), MAX_FILE_BYTES as u64)?;
            resource.verify()?;
            let after_md = fs::symlink_metadata(resource.path())?;
            ensure!(
                NodeStamp::capture(resource.path(), &after_md, false)? == before,
                "speculation source changed during capture"
            );
            let image = Image::bytes(&bytes);
            Ok((Some(bytes), image))
        }
    }
}

struct Storage {
    id: String,
    directory: PathBuf,
    root_guard: NativeResource,
    directory_guard: NativeResource,
    lock_guard: NativeResource,
    lock_stamp: NodeStamp,
    _lock: RepoTxnLock,
}

#[derive(Default)]
struct DiskState {
    poisoned: bool,
    bytes: usize,
    records: usize,
    overlays: usize,
    objects: BTreeMap<String, Image>,
    reserved_bytes: usize,
    reserved_records: usize,
    reserved_objects: usize,
}

impl Storage {
    fn create(identity: &WorkspaceIdentity, root: &Path) -> Result<Self> {
        ensure!(
            root.is_absolute(),
            "speculation store root must be explicit and absolute"
        );
        let root_guard = NativeResource::resolve(&root.join(".store-boundary"))?;
        // Explicit storage is never admitted beneath a marked scratch/process namespace. Its GC
        // lifetime is incompatible with durable cohort ownership, even when a caller passes it.
        for ancestor in root.ancestors() {
            ensure!(
                matches!(fs::symlink_metadata(ancestor.join(crate::core::scratch::OWNER_MARKER)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "speculation durable store cannot use a scratch-owned namespace"
            );
        }
        ensure!(
            !lexically_within(root, &identity.canonical_root)?
                && !lexically_within(&identity.canonical_root, root)?,
            "speculation store must be separate from workspace and its ancestors"
        );
        harden_directory(root)?;
        root_guard.verify()?;
        let namespace = root.join(NAMESPACE);
        match fs::create_dir(&namespace) {
            Ok(()) => {
                harden_directory(&namespace)?;
                sync_directory(root)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                harden_directory(&namespace)?
            }
            Err(_) => bail!("speculation private namespace creation failed"),
        }
        let namespace_guard = NativeResource::resolve(&namespace.join(".store-boundary"))?;
        root_guard.verify()?;
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|_| anyhow::anyhow!("speculation cohort ID unavailable"))?;
        let id = hex(&random);
        let directory = namespace.join(&id);
        fs::create_dir(&directory)
            .context("speculation cohort namespace already exists or cannot be created")?;
        harden_directory(&directory)?;
        namespace_guard.verify()?;
        sync_directory(&namespace)?;
        let directory_guard = NativeResource::resolve(&directory.join(".store-boundary"))?;
        let lock_path = directory.join("owner.lock");
        persist::atomic_write_owner_only(&lock_path, OWNER_MARKER)?;
        sync_directory(&directory)?;
        let lock_guard = NativeResource::resolve(&lock_path)?;
        let lock_stamp = NodeStamp::capture(&lock_path, &fs::symlink_metadata(&lock_path)?, false)?;
        let lock = RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO)?;
        let result = Self {
            id,
            directory,
            root_guard,
            directory_guard,
            lock_guard,
            lock_stamp,
            _lock: lock,
        };
        result.verify(&DiskState::default())?;
        Ok(result)
    }

    /// Load bounded untrusted evidence only: no owner acquisition, writes, or handle reconstruction.
    /// The marker uses canonical bytes after a length guard so this also works while its Windows
    /// descriptor is exclusively locked. The owner must check its ACTUAL bytes separately.
    fn load_existing_inventory(directory: &Path) -> Result<(DiskState, [u8; 32])> {
        use ring::digest::{Context as DigestContext, SHA256};
        let guard = NativeResource::resolve(&directory.join(".store-boundary"))?;
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("speculation inspected cohort ID invalid")?;
        ensure!(
            id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "speculation inspected cohort ID invalid"
        );
        ensure!(
            directory
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(NAMESPACE),
            "speculation inspection needs a versioned cohort directory"
        );
        let owner = NativeResource::resolve(&directory.join("owner.lock"))?;
        let owner_md = fs::symlink_metadata(owner.path())?;
        ensure!(
            owner_md.is_file() && owner_md.len() == OWNER_MARKER.len() as u64,
            "unsupported restart owner marker length"
        );
        let owner_stamp = NodeStamp::capture(owner.path(), &owner_md, false)?;
        let mut entries = BTreeMap::new();
        for entry in fs::read_dir(directory)? {
            ensure!(
                entries.len() <= MAX_OBJECTS,
                "speculation inspected object count exceeds bound"
            );
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("speculation inspected filename invalid"))?;
            entries.insert(name, entry.path());
        }
        ensure!(
            entries.contains_key("owner.lock"),
            "speculation inventory owner disappeared"
        );
        ensure!(
            entries.len() <= MAX_OBJECTS + 1,
            "speculation inspected object count exceeds bound"
        );
        let mut disk = DiskState::default();
        let mut total = 0usize;
        let mut digest = DigestContext::new(&SHA256);
        digest.update(b"aizen-tm-recovery-inventory-v1\0");
        for (name, path) in &entries {
            let resource = NativeResource::resolve(path)?;
            let md = fs::symlink_metadata(path)?;
            let bound = if name == "owner.lock" {
                OWNER_MARKER.len()
            } else if name.starts_with("m-") && name.ends_with(".json") {
                disk.records += 1;
                ensure!(
                    disk.records <= MAX_RECORDS,
                    "speculation inspected manifest count exceeds bound"
                );
                MAX_META_BYTES
            } else if name.starts_with("b-") {
                MAX_FILE_BYTES
            } else {
                bail!("speculation inspection found incomplete/crash-state object")
            };
            ensure!(
                md.is_file() && md.len() <= bound as u64,
                "speculation inspected object exceeds bound or is not a file"
            );
            total = total
                .checked_add(md.len() as usize)
                .context("speculation inspection byte overflow")?;
            ensure!(
                total <= MAX_STORE_BYTES,
                "speculation inspected inventory exceeds 64 MiB including owner marker"
            );
            let stamp = NodeStamp::capture(path, &md, false)?;
            let bytes = if name == "owner.lock" {
                ensure!(
                    md.len() == OWNER_MARKER.len() as u64,
                    "unsupported restart owner marker length"
                );
                OWNER_MARKER.to_vec()
            } else {
                let bytes = read_existing_regular(path, md.len())?;
                ensure!(
                    bytes.len() == md.len() as usize,
                    "speculation inspected object changed length"
                );
                bytes
            };
            resource.verify()?;
            ensure!(
                NodeStamp::capture(path, &fs::symlink_metadata(path)?, false)? == stamp,
                "speculation inspected object changed"
            );
            let image = Image::bytes(&bytes);
            if name != "owner.lock" {
                let expected_name = if name.starts_with("m-") {
                    manifest_name(&bytes)
                } else {
                    image
                        .blob_name()
                        .context("speculation inspected image absent")?
                };
                ensure!(
                    *name == expected_name,
                    "speculation inspected content address mismatch"
                );
                disk.bytes = disk
                    .bytes
                    .checked_add(bytes.len())
                    .context("speculation inspection byte overflow")?;
                disk.objects.insert(name.clone(), image.clone());
            }
            digest.update(&(name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
            digest.update(&image.byte_len.to_le_bytes());
            digest.update(&image.sha256);
        }
        let mut count = 0usize;
        for entry in fs::read_dir(directory)? {
            count += 1;
            ensure!(
                count <= entries.len()
                    && entries.contains_key(
                        &entry?
                            .file_name()
                            .into_string()
                            .map_err(|_| anyhow::anyhow!(
                                "speculation inspected filename invalid"
                            ))?
                    ),
                "speculation inventory changed during inspection"
            );
        }
        ensure!(
            count == entries.len(),
            "speculation inventory object disappeared"
        );
        owner.verify()?;
        ensure!(
            NodeStamp::capture(owner.path(), &fs::symlink_metadata(owner.path())?, false)?
                == owner_stamp,
            "speculation inventory owner changed"
        );
        guard.verify()?;
        let mut revision = [0u8; 32];
        revision.copy_from_slice(digest.finish().as_ref());
        Ok((disk, revision))
    }

    fn verify(&self, disk: &DiskState) -> Result<()> {
        ensure!(!disk.poisoned, "speculation store is uncertain");
        self.root_guard.verify()?;
        self.directory_guard.verify()?;
        self.lock_guard.verify()?;
        ensure!(
            NodeStamp::capture(
                self.lock_guard.path(),
                &fs::symlink_metadata(self.lock_guard.path())?,
                false
            )? == self.lock_stamp,
            "speculation OS ownership lock identity changed"
        );
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(&self.directory)? {
            ensure!(
                names.len() <= MAX_OBJECTS,
                "speculation unmanaged storage record overflow"
            );
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("speculation invalid managed filename"))?;
            ensure!(
                name == "owner.lock" || disk.objects.contains_key(&name),
                "speculation unexpected/crash-state storage file"
            );
            NativeResource::resolve(&entry.path())?;
            names.insert(name);
        }
        ensure!(
            names.len() == disk.objects.len() + 1,
            "speculation managed object disappeared"
        );
        self.directory_guard.verify()
    }

    fn reserve(&self, disk: &DiskState, names: &[(&str, usize)], records: usize) -> Result<()> {
        ensure!(!disk.poisoned, "speculation store is uncertain");
        // DiskState tracks immutable object bytes only; the durable marker consumes capacity once.
        let mut bytes = disk
            .bytes
            .checked_add(disk.reserved_bytes)
            .and_then(|bytes| bytes.checked_add(OWNER_MARKER.len()))
            .context("speculation reservation byte overflow")?;
        let mut added = BTreeSet::new();
        for (name, len) in names {
            if !disk.objects.contains_key(*name) && added.insert(*name) {
                bytes = bytes
                    .checked_add(*len)
                    .context("speculation byte quota overflow")?;
            }
        }
        ensure!(
            bytes <= MAX_STORE_BYTES,
            "speculation cohort exceeds 64 MiB aggregate storage"
        );
        ensure!(
            disk.records
                .checked_add(disk.reserved_records)
                .and_then(|count| count.checked_add(records))
                .is_some_and(|count| count <= MAX_RECORDS),
            "speculation durable record limit reached"
        );
        ensure!(
            disk.objects
                .len()
                .saturating_add(disk.reserved_objects)
                .saturating_add(added.len())
                <= MAX_OBJECTS,
            "speculation object count limit reached"
        );
        Ok(())
    }

    fn reserve_publication(
        &self,
        disk: &DiskState,
        image: &Image,
        bytes: &[u8],
        meta: &[u8],
    ) -> Result<()> {
        let blob = image
            .blob_name()
            .context("speculation missing write image")?;
        let manifest = manifest_name(meta);
        self.reserve(disk, &[(&blob, bytes.len()), (&manifest, meta.len())], 1)
    }

    fn publish_blob(&self, disk: &mut DiskState, image: &Image, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_FILE_BYTES && Image::bytes(bytes) == *image,
            "speculation invalid blob image"
        );
        let name = image
            .blob_name()
            .context("speculation invalid missing blob")?;
        self.reserve(disk, &[(&name, bytes.len())], 0)?;
        self.publish(disk, &name, bytes)
    }

    fn publish_manifest(&self, disk: &mut DiskState, bytes: &[u8]) -> Result<String> {
        ensure!(
            bytes.len() <= MAX_META_BYTES,
            "speculation metadata exceeds limit"
        );
        let name = manifest_name(bytes);
        self.reserve(disk, &[(&name, bytes.len())], 1)?;
        self.publish(disk, &name, bytes)?;
        disk.records += 1;
        Ok(name)
    }

    fn publish(&self, disk: &mut DiskState, name: &str, bytes: &[u8]) -> Result<()> {
        let result = (|| {
            self.verify(disk)?;
            if disk.objects.contains_key(name) {
                ensure!(
                    self.read_object(disk, name)? == bytes,
                    "speculation content-address collision/corruption"
                );
                return Ok(());
            }
            let path = self.directory.join(name);
            ensure!(
                matches!(fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "speculation immutable publication target exists"
            );
            fault(Fault::BeforePublish)?;
            persist::atomic_write_owner_only(&path, bytes)?;
            fault(Fault::AfterPublish)?;
            sync_directory(&self.directory)?;
            self.directory_guard.verify()?;
            let resource = NativeResource::resolve(&path)?;
            resource.verify()?;
            let image = Image::bytes(bytes);
            disk.bytes = disk
                .bytes
                .checked_add(bytes.len())
                .context("speculation quota overflow")?;
            disk.objects.insert(name.to_owned(), image);
            self.verify_object(disk, name)?;
            self.verify(disk)?;
            Ok(())
        })();
        if result.is_err() {
            disk.poisoned = true;
        }
        result
    }

    fn read_object(&self, disk: &DiskState, name: &str) -> Result<Vec<u8>> {
        let expected = disk
            .objects
            .get(name)
            .context("speculation unknown object (manifest is not authority)")?;
        let path = self.directory.join(name);
        let resource = NativeResource::resolve(&path)?;
        let md = fs::symlink_metadata(&path)?;
        ensure!(
            md.len() == expected.byte_len && md.len() <= MAX_FILE_BYTES.max(MAX_META_BYTES) as u64,
            "speculation stored object length mismatch"
        );
        let stamp = NodeStamp::capture(&path, &md, false)?;
        let bytes = read_existing_regular(&path, expected.byte_len)?;
        resource.verify()?;
        ensure!(
            NodeStamp::capture(&path, &fs::symlink_metadata(&path)?, false)? == stamp
                && Image::bytes(&bytes) == *expected,
            "speculation stored object changed/corrupt"
        );
        self.directory_guard.verify()?;
        Ok(bytes)
    }

    fn verify_object(&self, disk: &DiskState, name: &str) -> Result<()> {
        self.read_object(disk, name).map(|_| ())
    }
}

fn harden_directory(path: &Path) -> Result<()> {
    let md = fs::symlink_metadata(path)?;
    ensure!(
        md.is_dir() && !is_link_or_reparse(&md),
        "speculation store requires checked non-link directories"
    );
    // Resolving a synthetic leaf checks ALL ancestors without following directory aliases.
    let guard = NativeResource::resolve(&path.join(".store-boundary"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    persist::harden_owner_only_checked(path)?;
    #[cfg(not(any(unix, windows)))]
    bail!("speculation owner-only directory protection unavailable");
    guard.verify()
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path; // persist uses MOVEFILE_WRITE_THROUGH for immutable publications on Windows.
    Ok(())
}

/// Read-only revision of every immutable object, including orphan evidence. This takes no owner
/// lock and represents owner.lock by canonical bytes after a checked length guard, so it also works
/// under exclusive Windows ownership. Callers must separately check the ACTUAL marker through their
/// held owner descriptor. A matching revision is a stale-selection check, never execution authority.
pub(crate) fn inventory_revision(directory: &Path) -> Result<[u8; 32]> {
    Storage::load_existing_inventory(directory).map(|(_, revision)| revision)
}

/// Inspect an abandoned cohort as untrusted evidence. This cannot reopen controller authority.
/// Live controllers own an exclusive OS lock, so inspection refuses until their workers exit.
pub(crate) fn inspect_controller_records(directory: &Path) -> Result<Vec<InspectedRecord>> {
    let guard = NativeResource::resolve(&directory.join(".store-boundary"))?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("speculation inspected cohort ID invalid")?;
    ensure!(
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "speculation inspected cohort ID invalid"
    );
    ensure!(
        directory
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(NAMESPACE),
        "speculation inspection needs a versioned cohort directory"
    );
    let lock_guard = NativeResource::resolve(&directory.join("owner.lock"))?;
    let md = fs::symlink_metadata(lock_guard.path())?;
    let lock_stamp = NodeStamp::capture(lock_guard.path(), &md, false)?;
    let lock = RepoTxnLock::acquire_existing(
        lock_guard.path(),
        crate::core::repo_lock::LockMode::Shared,
        Duration::ZERO,
    )?;
    lock.verify_existing_path(lock_guard.path())?;
    guard.verify()?;
    let storage = Storage {
        id: id.to_owned(),
        directory: directory.to_path_buf(),
        root_guard: guard.clone(),
        directory_guard: guard,
        lock_guard,
        lock_stamp,
        _lock: lock,
    };
    let check_owner = || -> Result<()> {
        storage
            ._lock
            .verify_existing_path(storage.lock_guard.path())?;
        // A shared Windows lock permits this bounded read; exclusive readers use their descriptor.
        let marker = read_existing_regular(storage.lock_guard.path(), OWNER_MARKER.len() as u64)?;
        ensure!(marker == OWNER_MARKER, "unsupported restart owner marker");
        storage
            ._lock
            .verify_existing_path(storage.lock_guard.path())
    };
    check_owner()?;
    let (disk, _) = Storage::load_existing_inventory(directory)?;
    storage.verify(&disk)?;
    let records = inspect_records(&storage, &disk, id)?;
    check_owner()?;
    storage.verify(&disk)?;
    Ok(records)
}

fn controller_metadata(
    base: &str,
    owner: &str,
    label: &str,
    metadata: &[u8],
    images: &[&[u8]],
) -> Result<Vec<u8>> {
    check_owner(owner)?;
    ensure!(
        !label.is_empty()
            && label.len() <= 128
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "speculation controller record label invalid"
    );
    ensure!(
        metadata.len() <= MAX_CONTROLLER_RECORD_META
            && images.len() <= MAX_CONTROLLER_IMAGES
            && images.iter().all(|bytes| bytes.len() <= MAX_FILE_BYTES),
        "speculation controller record exceeds bounded image/meta limit"
    );
    encode_meta(&ControllerRecord {
        format: FORMAT,
        kind: "controller-record",
        base,
        owner,
        label,
        metadata: Image::bytes(metadata),
        images: images.iter().map(|bytes| Image::bytes(bytes)).collect(),
    })
}

fn parsed_image(value: &serde_json::Value) -> Result<Image> {
    ensure!(
        value.get("exists").and_then(serde_json::Value::as_bool) == Some(true),
        "speculation inspected image invalid"
    );
    let byte_len = value
        .get("byte_len")
        .and_then(serde_json::Value::as_u64)
        .context("speculation inspected image length invalid")?;
    ensure!(
        byte_len <= MAX_FILE_BYTES as u64,
        "speculation inspected image exceeds bound"
    );
    let digest = value
        .get("sha256")
        .and_then(serde_json::Value::as_array)
        .context("speculation inspected hash invalid")?;
    ensure!(
        digest.len() == 32,
        "speculation inspected hash length invalid"
    );
    let mut sha256 = [0u8; 32];
    for (index, byte) in digest.iter().enumerate() {
        sha256[index] = byte
            .as_u64()
            .filter(|n| *n <= 255)
            .context("speculation inspected digest invalid")? as u8;
    }
    Ok(Image {
        exists: true,
        byte_len,
        sha256,
    })
}

fn checked_inspection_expansion(current: usize, additional: usize, limit: usize) -> Result<usize> {
    let expanded = current
        .checked_add(additional)
        .context("speculation inspected expansion overflow")?;
    ensure!(
        expanded <= limit && expanded <= MAX_STORE_BYTES,
        "speculation inspected expansion exceeds bound"
    );
    Ok(expanded)
}

// Bounds cover transient validation work as well as retained inspection output. No blob cache
// survives a read; only bounded decoded, content-addressed sealed manifests are retained.
struct InspectionWork {
    read_bytes: u64,
    comparison_bytes: u64,
    merge_work: u64,
    read_limit: u64,
    comparison_limit: u64,
    merge_limit: u64,
    object_reads: usize,
    histories: usize,
}

impl Default for InspectionWork {
    fn default() -> Self {
        Self {
            read_bytes: 0,
            comparison_bytes: 0,
            merge_work: 0,
            read_limit: 256 * 1024 * 1024,
            comparison_limit: 256 * 1024 * 1024,
            merge_limit: 64 * 64 * 1024 * 1024u64,
            object_reads: 0,
            histories: 0,
        }
    }
}

impl InspectionWork {
    fn charge(used: &mut u64, amount: u64, limit: u64) -> Result<()> {
        let next = used
            .checked_add(amount)
            .context("speculation inspection work overflow")?;
        ensure!(next <= limit, "speculation inspection work budget exceeded");
        *used = next;
        Ok(())
    }

    fn compare(&mut self, bytes: u64) -> Result<()> {
        Self::charge(&mut self.comparison_bytes, bytes, self.comparison_limit)
    }

    fn read(&mut self, storage: &Storage, disk: &DiskState, name: &str) -> Result<Vec<u8>> {
        let image = disk
            .objects
            .get(name)
            .context("speculation inspected object missing")?;
        Self::charge(&mut self.read_bytes, image.byte_len, self.read_limit)?;
        self.compare(image.byte_len)?; // read_object hashes and compares the entire object.
        self.object_reads += 1;
        storage.read_object(disk, name)
    }
}

#[cfg(test)]
pub(crate) struct InspectionWorkStats {
    pub(crate) read_bytes: u64,
    pub(crate) comparison_bytes: u64,
    pub(crate) merge_work: u64,
    pub(crate) object_reads: usize,
    pub(crate) histories: usize,
}

#[cfg(test)]
impl BaseSnapshot {
    pub(crate) fn inspect_with_work_budget(
        &self,
        read_limit: u64,
        comparison_limit: u64,
        merge_limit: u64,
    ) -> (Result<Vec<InspectedRecord>>, InspectionWorkStats) {
        let mut work = InspectionWork {
            read_limit,
            comparison_limit,
            merge_limit,
            ..InspectionWork::default()
        };
        let result = self.0.lock_disk().and_then(|disk| {
            inspect_records_budgeted(&self.0.storage, &disk, self.id(), &mut work)
                .map(|(records, _)| records)
        });
        (
            result,
            InspectionWorkStats {
                read_bytes: work.read_bytes,
                comparison_bytes: work.comparison_bytes,
                merge_work: work.merge_work,
                object_reads: work.object_reads,
                histories: work.histories,
            },
        )
    }
}

fn validate_record_overlay(
    metadata: &serde_json::Value,
    identity: &SealedManifestIdentity,
    manifest: &SealedOverlayManifest,
    label: &str,
) -> Result<()> {
    let author = format!("{}/{}", identity.owner, identity.overlay);
    let observations = metadata
        .get("observations")
        .and_then(serde_json::Value::as_array)
        .context("speculation inspected observations missing")?;
    ensure!(
        observations.len() <= MAX_OVERLAYS * MAX_OBSERVATIONS,
        "speculation inspected observation count exceeds bound"
    );
    let selected: Vec<_> = observations
        .iter()
        .filter(|observation| {
            observation
                .get("author")
                .and_then(serde_json::Value::as_str)
                == Some(&author)
        })
        .collect();
    ensure!(
        selected.len() == manifest.observations.len(),
        "speculation inspected observation count differs from sealed manifest"
    );
    for observation in &manifest.observations {
        let binding = manifest
            .bindings
            .iter()
            .find(|binding| binding.key == observation.key)
            .context("speculation inspected observation binding missing")?;
        let expected = serde_json::json!({"author": author, "key": observation.key, "path": binding.path,
            "generation": observation.generation, "fingerprint": observation.image});
        ensure!(
            selected.iter().filter(|value| ***value == expected).count() == 1,
            "speculation inspected observation differs from sealed manifest"
        );
    }
    if label != "integration-prepared" {
        return Ok(());
    }
    let files = metadata
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("speculation inspected prepared files missing")?;
    ensure!(
        files.len() <= MAX_COHORT_FILES,
        "speculation inspected prepared count exceeds bound"
    );
    let mut changed = BTreeSet::new();
    for file in files {
        let lineage = file
            .get("lineage")
            .and_then(serde_json::Value::as_array)
            .context("speculation inspected change lineage missing")?;
        ensure!(
            lineage.len() <= MAX_OVERLAYS,
            "speculation inspected change lineage exceeds bound"
        );
        for change in lineage.iter().filter(|change| {
            change.get("author").and_then(serde_json::Value::as_str) == Some(&author)
        }) {
            let key = file
                .get("key")
                .and_then(serde_json::Value::as_str)
                .context("speculation inspected change key missing")?;
            let binding = manifest
                .bindings
                .iter()
                .find(|binding| binding.key == key)
                .context("speculation inspected change binding missing")?;
            let version = manifest
                .files
                .get(key)
                .context("speculation inspected final version missing")?;
            let rewrite = manifest
                .edits
                .iter()
                .any(|edit| edit.key == key && edit.provenance == WriteProvenance::Rewrite);
            ensure!(
                binding.base != version.image
                    && changed.insert(key)
                    && file.get("path") == Some(&serde_json::to_value(&binding.path)?)
                    && file.get("expected") == Some(&serde_json::to_value(&binding.base)?)
                    && change.get("generation").and_then(serde_json::Value::as_u64)
                        == Some(version.generation)
                    && change.get("operation").and_then(serde_json::Value::as_str)
                        == Some(if rewrite { "rewrite" } else { "localized" }),
                "speculation inspected change differs from sealed manifest"
            );
        }
    }
    ensure!(
        manifest
            .bindings
            .iter()
            .filter(|binding| manifest.files[&binding.key].image != binding.base)
            .all(|binding| changed.contains(binding.key.as_str())),
        "speculation inspected prepared lineage omits sealed change"
    );
    Ok(())
}

struct SealedChangeEvidence {
    author: String,
    path: PathBuf,
    before: Image,
    after: Image,
    operation: super::merge::Operation,
}

fn collect_sealed_changes(
    changes: &mut BTreeMap<String, Vec<SealedChangeEvidence>>,
    manifest: &SealedOverlayManifest,
) {
    for binding in &manifest.bindings {
        let after = &manifest.files[&binding.key].image;
        if after == &binding.base {
            continue;
        }
        changes
            .entry(binding.key.clone())
            .or_default()
            .push(SealedChangeEvidence {
                author: format!("{}/{}", manifest.owner, manifest.overlay),
                path: binding.path.clone(),
                before: binding.base.clone(),
                after: after.clone(),
                operation: if manifest.edits.iter().any(|edit| {
                    edit.key == binding.key && edit.provenance == WriteProvenance::Rewrite
                }) {
                    super::merge::Operation::Rewrite
                } else {
                    super::merge::Operation::Localized
                },
            });
    }
}

fn validate_prepared_mapping(
    value: &serde_json::Value,
    expected: &Image,
    refs: &[Image],
    next: &mut usize,
) -> Result<()> {
    // Compare the complete descriptor, including canonical index/null and absence semantics.
    let mut canonical = serde_json::to_value(expected)?;
    canonical["missing"] = (!expected.exists).into();
    canonical["index"] = if expected.exists {
        serde_json::json!(*next)
    } else {
        serde_json::Value::Null
    };
    ensure!(
        *value == canonical,
        "speculation inspected prepared image mapping is not canonical"
    );
    if expected.exists {
        ensure!(
            refs.get(*next) == Some(expected),
            "speculation inspected prepared image reference differs"
        );
        *next += 1;
    }
    Ok(())
}

fn validate_prepared_candidates(
    storage: &Storage,
    disk: &DiskState,
    metadata: &serde_json::Value,
    mut changes: BTreeMap<String, Vec<SealedChangeEvidence>>,
    refs: &[Image],
    work: &mut InspectionWork,
) -> Result<()> {
    use super::merge::{self, AuthoredChange, Language, Limits, Operation};
    let files = metadata
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("speculation inspected prepared files missing")?;
    ensure!(
        files.len() == changes.len(),
        "speculation inspected candidate membership differs from sealed changes"
    );
    let mut next_image = 0;
    for file in files {
        let key = file
            .get("key")
            .and_then(serde_json::Value::as_str)
            .context("speculation inspected candidate key missing")?;
        let selected = changes
            .remove(key)
            .context("speculation inspected candidate key duplicated or not authored")?;
        let first = selected
            .first()
            .context("speculation inspected candidate has no sealed authors")?;
        // Mirror hard merge input ceilings BEFORE materializing authored image vectors. Store
        // image bounds are larger, and deduplicated references must still count once per author.
        let mut merge_bytes = 0u64;
        for change in &selected {
            ensure!(
                change.before.byte_len <= 1024 * 1024 && change.after.byte_len <= 1024 * 1024,
                "speculation inspected candidate exceeds merge image bound"
            );
            merge_bytes = merge_bytes
                .checked_add(change.before.byte_len + change.after.byte_len)
                .context("speculation inspected merge byte overflow")?;
        }
        ensure!(
            merge_bytes <= 16 * 1024 * 1024,
            "speculation inspected candidate exceeds merge total bound"
        );
        let before = first
            .before
            .blob_name()
            .map(|name| work.read(storage, disk, &name))
            .transpose()?;
        let mut results = Vec::with_capacity(selected.len());
        for change in &selected {
            ensure!(
                change.before == first.before && change.path == first.path,
                "speculation inspected candidate base differs between authors"
            );
            results.push(
                work.read(
                    storage,
                    disk,
                    &change
                        .after
                        .blob_name()
                        .context("speculation inspected candidate postimage missing")?,
                )?,
            );
        }
        let authored: Vec<_> = selected
            .iter()
            .zip(&results)
            .map(|(change, result)| AuthoredChange {
                author: &change.author,
                base: before.as_deref(),
                result,
                operation: change.operation,
            })
            .collect();
        // Replay only the pure deterministic merge. Hard engine ceilings still bound work; no
        // source path is resolved and no persisted identity grants execution/recovery authority.
        let limits = Limits {
            max_changes: usize::MAX,
            max_bytes: usize::MAX,
            max_total_bytes: usize::MAX,
            max_lines: usize::MAX,
            max_diff_cells: usize::MAX,
            max_work: usize::MAX,
            max_hunks: usize::MAX,
            max_tokens: usize::MAX,
            max_depth: usize::MAX,
        };
        // Reserve the engine's full hard work ceiling BEFORE entering it; each merge has its
        // own internal ceilings, while this budget also bounds the number of expensive replays.
        InspectionWork::charge(&mut work.merge_work, 64 * 1024 * 1024, work.merge_limit)?;
        let language = if first.path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            Language::Rust
        } else {
            Language::Other
        };
        let candidate = merge::merge(language, &authored, &limits).map_err(|_| {
            anyhow::anyhow!("speculation inspected candidate does not have a sealed merge")
        })?;
        work.compare(candidate.bytes.len() as u64)?;
        let after = Image::bytes(&candidate.bytes);
        validate_prepared_mapping(
            file.get("preimage")
                .context("speculation inspected preimage mapping missing")?,
            &first.before,
            refs,
            &mut next_image,
        )?;
        validate_prepared_mapping(
            file.get("postimage")
                .context("speculation inspected postimage mapping missing")?,
            &after,
            refs,
            &mut next_image,
        )?;
        let postimage = file
            .get("postimage")
            .context("speculation inspected candidate image mapping missing")?;
        let operation = if selected
            .iter()
            .any(|change| change.operation == Operation::Rewrite)
        {
            "rewrite"
        } else {
            "localized"
        };
        ensure!(
            file.get("after") == Some(&serde_json::to_value(&after)?)
                && parsed_image(postimage)? == after
                && postimage
                    .get("missing")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
                && file.get("contributors")
                    == Some(&serde_json::to_value(&candidate.contributors)?)
                && file.get("operation").and_then(serde_json::Value::as_str) == Some(operation),
            "speculation inspected candidate differs from deterministic sealed merge"
        );
    }
    ensure!(
        next_image == refs.len(),
        "speculation inspected prepared has unreferenced images"
    );
    Ok(())
}

fn validate_manifest_images(
    storage: &Storage,
    disk: &DiskState,
    manifest: &SealedOverlayManifest,
    work: &mut InspectionWork,
) -> Result<()> {
    let read = |image: &Image, work: &mut InspectionWork| -> Result<Option<Vec<u8>>> {
        let Some(name) = image.blob_name() else {
            return Ok(None);
        };
        ensure!(
            disk.objects.get(&name) == Some(image),
            "speculation inspected sealed image missing or changed"
        );
        work.read(storage, disk, &name).map(Some)
    };
    for image in manifest
        .bindings
        .iter()
        .map(|binding| &binding.base)
        .chain(manifest.files.values().map(|version| &version.image))
        .chain(
            manifest
                .edits
                .iter()
                .flat_map(|edit| [&edit.before, &edit.after]),
        )
    {
        if let Some(name) = image.blob_name() {
            ensure!(
                disk.objects.get(&name) == Some(image),
                "speculation inspected sealed image missing or changed"
            );
        }
    }
    for edit in &manifest.edits {
        if matches!(edit.provenance, WriteProvenance::Localized { .. }) {
            // Provenance compares prefix/suffix bytes. Charge their conservative full-image
            // upper bound before either read or comparison, including repeated image references.
            work.compare(edit.before.byte_len.saturating_add(edit.after.byte_len))?;
            let before = read(&edit.before, work)?;
            let after =
                read(&edit.after, work)?.context("speculation inspected edit postimage missing")?;
            verify_provenance(before.as_deref(), &after, edit.provenance)?;
        }
    }
    Ok(())
}

fn inspect_base_manifest(
    storage: &Storage,
    disk: &DiskState,
    base: &str,
    work: &mut InspectionWork,
) -> Result<BaseManifestOwned> {
    let mut captured = None;
    for name in disk.objects.keys().filter(|name| name.starts_with("m-")) {
        let bytes = work.read(storage, disk, name)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if value.get("kind").and_then(serde_json::Value::as_str) != Some("base") {
            continue;
        }
        let manifest: BaseManifestOwned = serde_json::from_slice(&bytes)?;
        ensure!(
            captured.is_none()
                && manifest.format == FORMAT
                && manifest.kind == "base"
                && manifest.base == base
                && manifest_name(&bytes) == *name
                && encode_meta(&manifest)? == bytes
                && !manifest.repo.is_empty()
                && manifest.repo.len() <= 128
                && !manifest.worktree.is_empty()
                && manifest.worktree.len() <= 128
                && manifest.root.is_absolute()
                && !manifest.bindings.is_empty()
                && manifest.bindings.len() <= MAX_COHORT_FILES,
            "speculation inspected base manifest invalid or duplicated"
        );
        let mut keys = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for binding in &manifest.bindings {
            ensure!(
                !binding.key.is_empty()
                    && binding.key.len() <= 128
                    && keys.insert(&binding.key)
                    && !binding.key.chars().any(char::is_control)
                    && binding.path.is_absolute()
                    && binding.path != manifest.root
                    && lexically_within(&binding.path, &manifest.root)?
                    && paths.insert(path_key(&binding.path)?)
                    && !binding.writable
                    && binding.base.byte_len <= MAX_FILE_BYTES as u64
                    && (binding.base.exists || binding.base == Image::missing()),
                "speculation inspected base binding invalid"
            );
        }
        captured = Some(manifest);
    }
    captured.context("speculation inspected immutable base missing")
}

fn inspect_records(
    storage: &Storage,
    disk: &DiskState,
    base: &str,
) -> Result<Vec<InspectedRecord>> {
    inspect_records_with_expansion(storage, disk, base).map(|(records, _)| records)
}

struct SealedInspectionCache {
    manifests: BTreeMap<String, SealedOverlayManifest>,
    encoded_bytes: usize,
}

impl SealedInspectionCache {
    fn get<'a>(
        &'a mut self,
        storage: &Storage,
        disk: &DiskState,
        base: &str,
        identity: &SealedManifestIdentity,
        captured: &BaseManifestOwned,
        work: &mut InspectionWork,
    ) -> Result<&'a SealedOverlayManifest> {
        identity.validate()?;
        if !self.manifests.contains_key(&identity.manifest) {
            let size = disk
                .objects
                .get(&identity.manifest)
                .context("speculation sealed manifest missing")?
                .byte_len;
            let next = self
                .encoded_bytes
                .checked_add(usize::try_from(size)?)
                .context("speculation sealed validation cache overflow")?;
            ensure!(
                self.manifests.len() < MAX_RECORDS && next <= 8 * 1024 * 1024,
                "speculation sealed validation cache exceeds bound"
            );
            let bytes = work.read(storage, disk, &identity.manifest)?;
            work.compare(size)?;
            let manifest = decode_sealed_manifest(&bytes)?;
            ensure!(
                manifest_name(&bytes) == identity.manifest,
                "speculation sealed manifest address changed"
            );
            validate_manifest_images(storage, disk, &manifest, work)?;
            self.encoded_bytes = next;
            work.histories += 1;
            self.manifests.insert(identity.manifest.clone(), manifest);
        }
        let manifest = self
            .manifests
            .get(&identity.manifest)
            .context("speculation sealed cache missing")?;
        // Bound repeated key/path/base comparisons independently of history-cache hits.
        let encoded_size = disk
            .objects
            .get(&identity.manifest)
            .context("speculation sealed manifest missing")?
            .byte_len;
        work.compare(encoded_size.saturating_mul(captured.bindings.len() as u64 + 1))?;
        // A cache hit never skips the reference's identity or immutable-base crosslink.
        ensure!(
            manifest.base == base
                && manifest.base == identity.base
                && manifest.overlay == identity.overlay
                && manifest.owner == identity.owner
                && manifest.generation == identity.generation,
            "speculation inspected sealed lineage differs from manifest"
        );
        for binding in &manifest.bindings {
            ensure!(
                captured
                    .bindings
                    .iter()
                    .any(|original| original.key == binding.key
                        && original.path == binding.path
                        && original.base == binding.base),
                "speculation inspected sealed binding differs from base"
            );
        }
        Ok(manifest)
    }
}

fn inspect_records_with_expansion(
    storage: &Storage,
    disk: &DiskState,
    base: &str,
) -> Result<(Vec<InspectedRecord>, usize)> {
    inspect_records_budgeted(storage, disk, base, &mut InspectionWork::default())
}

fn inspect_records_budgeted(
    storage: &Storage,
    disk: &DiskState,
    base: &str,
    work: &mut InspectionWork,
) -> Result<(Vec<InspectedRecord>, usize)> {
    let mut result = Vec::new();
    let mut expanded = 0usize;
    let mut base_manifest = None;
    let mut sealed_cache = SealedInspectionCache {
        manifests: BTreeMap::new(),
        encoded_bytes: 0,
    };
    for name in disk.objects.keys().filter(|name| name.starts_with("m-")) {
        let bytes = work.read(storage, disk, name)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("speculation inspected manifest corrupt"))?;
        if value.get("kind").and_then(serde_json::Value::as_str) != Some("controller-record") {
            continue;
        }
        ensure!(
            value.get("format").and_then(serde_json::Value::as_str) == Some(FORMAT)
                && value.get("base").and_then(serde_json::Value::as_str) == Some(base),
            "speculation inspected record identity invalid"
        );
        let owner = value
            .get("owner")
            .and_then(serde_json::Value::as_str)
            .context("speculation inspected owner missing")?;
        check_owner(owner)?;
        let label = value
            .get("label")
            .and_then(serde_json::Value::as_str)
            .context("speculation inspected label missing")?;
        ensure!(
            label.len() <= 128,
            "speculation inspected label exceeds bound"
        );
        let metadata_image = parsed_image(
            value
                .get("metadata")
                .context("speculation inspected metadata missing")?,
        )?;
        ensure!(
            metadata_image.byte_len <= MAX_CONTROLLER_RECORD_META as u64,
            "speculation inspected metadata too large"
        );
        let metadata = work.read(
            storage,
            disk,
            &metadata_image
                .blob_name()
                .context("speculation metadata image absent")?,
        )?;
        work.compare(metadata.len() as u64)?;
        ensure!(
            Image::bytes(&metadata) == metadata_image,
            "speculation metadata content mismatch"
        );
        expanded = checked_inspection_expansion(expanded, metadata.len(), MAX_STORE_BYTES)?;
        let refs = value
            .get("images")
            .and_then(serde_json::Value::as_array)
            .context("speculation inspected image refs invalid")?;
        ensure!(
            refs.len() <= MAX_CONTROLLER_IMAGES,
            "speculation inspected image count exceeds bound"
        );
        let refs: Vec<Image> = refs
            .iter()
            .map(|value| {
                let image = parsed_image(value)?;
                ensure!(
                    *value == serde_json::to_value(&image)?,
                    "speculation inspected image descriptor is not canonical"
                );
                Ok(image)
            })
            .collect::<Result<_>>()?;
        if matches!(label, "integration-prepared" | "integration-conflict") {
            let metadata_value: serde_json::Value = serde_json::from_slice(&metadata)?;
            if let Some(lineage) = sealed_lineage(&metadata_value)? {
                if base_manifest.is_none() {
                    base_manifest = Some(inspect_base_manifest(storage, disk, base, work)?);
                }
                let mut changes = BTreeMap::new();
                for identity in &lineage {
                    let captured = base_manifest
                        .as_ref()
                        .context("speculation inspected base missing")?;
                    let manifest =
                        sealed_cache.get(storage, disk, base, identity, captured, work)?;
                    // Record observations/lineage are not part of the manifest cache key.
                    // They must be crosschecked independently on every reference.
                    work.compare(metadata.len() as u64 * (manifest.observations.len() as u64 + 1))?;
                    validate_record_overlay(&metadata_value, identity, manifest, label)?;
                    if label == "integration-prepared" {
                        collect_sealed_changes(&mut changes, manifest);
                    }
                }
                if label == "integration-prepared" {
                    validate_prepared_candidates(
                        storage,
                        disk,
                        &metadata_value,
                        changes,
                        &refs,
                        work,
                    )?;
                }
            }
        }
        let mut images = Vec::new();
        for image in &refs {
            expanded =
                checked_inspection_expansion(expanded, image.byte_len as usize, MAX_STORE_BYTES)?;
            let bytes = work.read(
                storage,
                disk,
                &image.blob_name().context("speculation image missing")?,
            )?;
            work.compare(bytes.len() as u64)?;
            ensure!(
                Image::bytes(&bytes) == *image,
                "speculation inspected image mismatch"
            );
            images.push(bytes);
        }
        result.push(InspectedRecord {
            id: name.clone(),
            owner: owner.to_owned(),
            label: label.to_owned(),
            metadata,
            images,
        });
        ensure!(
            result.len() <= MAX_RECORDS,
            "speculation inspected record count exceeds bound"
        );
    }
    let workers = super::workers::validate_worker_records(base, &result)?;
    for (identity, scope_hash) in &workers.sealed {
        if base_manifest.is_none() {
            base_manifest = Some(inspect_base_manifest(storage, disk, base, work)?);
        }
        let captured = base_manifest
            .as_ref()
            .context("speculation inspected base missing")?;
        let manifest = sealed_cache.get(storage, disk, base, identity, captured, work)?;
        work.compare(
            disk.objects
                .get(&identity.manifest)
                .context("speculation sealed manifest missing")?
                .byte_len,
        )?;
        ensure!(
            super::workers::scope_sha256(manifest.bindings.iter().map(|binding| (
                binding.key.as_str(),
                binding.path.as_path(),
                binding.writable
            )))? == *scope_hash,
            "speculation inspected worker scope differs from sealed manifest"
        );
    }
    for record in result
        .iter()
        .filter(|record| record.label == "integration-prepared")
    {
        work.compare(record.metadata.len() as u64)?;
        workers.check_selected_lineage(&serde_json::from_slice(&record.metadata)?)?;
    }
    Ok((result, expanded))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn manifest_name(bytes: &[u8]) -> String {
    format!("m-{}.json", hex(&FileFingerprint::for_bytes(bytes).sha256))
}

fn encode_meta(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_META_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("speculation metadata exceeds limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut bytes = Bounded(Vec::new());
    serde_json::to_writer(&mut bytes, value)
        .map_err(|_| anyhow::anyhow!("speculation metadata encoding failed/exceeds limit"))?;
    Ok(bytes.0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    BeforePublish,
    AfterPublish,
    BeforeObservationManifest,
    AfterObservationManifest,
}
#[cfg(not(test))]
fn fault(_point: Fault) -> Result<()> {
    Ok(())
}
#[cfg(test)]
thread_local! { static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn fault(point: Fault) -> Result<()> {
    FAULT.with(|slot| {
        if slot.get() == Some(point) {
            slot.set(None);
            bail!("injected speculation persistence failure")
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::workspace_txn::{stable_key, with_test_lock_home};

    struct Fixture {
        temp: crate::core::file_placement::tests::TestDir,
        work: PathBuf,
        home: PathBuf,
        store: PathBuf,
        identity: WorkspaceIdentity,
        /// Serializes against every other test that takes the process-global native-effects gate
        /// (see [`crate::core::speculation::test_support`]). Reentrant, so a test may hold two
        /// fixtures without hanging.
        _gate_lock: crate::core::speculation::test_support::GateTestLock,
    }

    impl Fixture {
        fn new() -> Self {
            let gate_lock = crate::core::speculation::test_support::GateTestLock::acquire();
            let temp = crate::core::file_placement::tests::TestDir::new();
            let work = temp.path().join("work");
            let home = temp.path().join("locks");
            let store = temp.path().join("store");
            for path in [&work, &home, &store] {
                fs::create_dir(path).unwrap();
            }
            let identity = WorkspaceIdentity {
                repo_id: stable_key(&format!("repo:{}", work.display())),
                worktree_id: stable_key(&format!("worktree:{}", work.display())),
                canonical_root: work.clone(),
                canonical_git_dir: None,
                canonical_common_git_dir: None,
            };
            Self {
                temp,
                work,
                home,
                store,
                identity,
                _gate_lock: gate_lock,
            }
        }
        fn scope(&self, reads: &[&str], writes: &[&str]) -> NativeWriteScope {
            NativeWriteScope::resolve_access(
                &self.work,
                &reads.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                &writes.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            )
            .unwrap()
        }
        fn guards(&self) -> (WorkspaceWriterLease, PhysicalEffectLease) {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                self.identity.clone(),
                Duration::ZERO,
                None,
                "speculation fixture",
                "controller",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "capture fixture",
                "controller",
            )
            .unwrap();
            (writer, physical)
        }
        fn capture(&self, scopes: &[NativeWriteScope]) -> Result<BaseSnapshot> {
            with_test_lock_home(&self.home, || {
                let (writer, physical) = self.guards();
                BaseSnapshot::capture(&self.identity, scopes, &self.store, &writer, &physical)
            })
        }
        fn revalidate(&self, base: &BaseSnapshot) -> Result<()> {
            with_test_lock_home(&self.home, || {
                let (writer, physical) = self.guards();
                base.revalidate_live(&writer, &physical)
            })
        }
    }

    fn observation_fixture(
        fixture: &Fixture,
    ) -> (PathBuf, super::super::recovery::InspectionObservation) {
        use super::super::recovery::{InspectionObservation, ObservationState};
        fs::write(fixture.work.join("a.txt"), b"unchanged source").unwrap();
        let base = fixture.capture(&[fixture.scope(&[], &["a.txt"])]).unwrap();
        let prepared = base
            .controller_record("integration-owner", "integration-prepared", b"{}", &[])
            .unwrap();
        let directory = base.directory().to_path_buf();
        let id = base.id().to_owned();
        drop(base);
        let revision = inventory_revision(&directory).unwrap();
        (
            directory,
            InspectionObservation {
                schema: 1,
                base: id,
                prepared: prepared.as_str().to_owned(),
                receipt: None,
                owner: "integration-owner".into(),
                observer: "controller".into(),
                state: ObservationState::ObservedNonterminal,
                inventory_sha256: revision,
                files: Vec::new(),
                native_records: Vec::new(),
            },
        )
    }

    // Do not read owner.lock through a second descriptor while its Windows exclusive lock is held.
    fn observation_objects(directory: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_str().unwrap().to_owned();
                (name != "owner.lock").then(|| (name, fs::read(path).unwrap()))
            })
            .collect()
    }

    fn observation_pair(
        observation: &super::super::recovery::InspectionObservation,
    ) -> (Vec<u8>, String, Vec<u8>, String) {
        let metadata = serde_json::to_vec(observation).unwrap();
        let blob = Image::bytes(&metadata).blob_name().unwrap();
        let envelope = controller_metadata(
            &observation.base,
            &observation.observer,
            super::super::recovery::OBSERVATION_LABEL,
            &metadata,
            &[],
        )
        .unwrap();
        let manifest = manifest_name(&envelope);
        (metadata, blob, envelope, manifest)
    }

    #[test]
    fn existing_observation_owner_acquire_is_existing_only_busy_stale_and_marker_checked() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            exec_ctx::with_current(
                ExecutionContext::default().with_resource_scope("controller"),
                || {
                    let missing = fixture.store.join(NAMESPACE).join("0".repeat(32));
                    assert!(ExistingObservationOwner::acquire(
                        &missing, [0; 32], &writer, &physical
                    )
                    .is_err());
                    assert!(!missing.exists());
                    let before = observation_objects(&directory);
                    let owner = ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    assert!(ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical
                    )
                    .is_err());
                    drop(owner);
                    let mut stale = observation.inventory_sha256;
                    stale[0] ^= 1;
                    assert!(ExistingObservationOwner::acquire(
                        &directory, stale, &writer, &physical
                    )
                    .is_err());
                    assert_eq!(observation_objects(&directory), before);
                    let lock = directory.join("owner.lock");
                    let mut marker = OWNER_MARKER.to_vec();
                    marker[0] ^= 1;
                    fs::write(&lock, &marker).unwrap();
                    // Canonical inventory bytes alone do not bless a same-length corrupt marker.
                    assert_eq!(
                        inventory_revision(&directory).unwrap(),
                        observation.inventory_sha256
                    );
                    assert!(ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical
                    )
                    .is_err());
                    assert_eq!(fs::read(&lock).unwrap(), marker);
                    fs::remove_file(&lock).unwrap();
                    assert!(ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical
                    )
                    .is_err());
                    assert!(!lock.exists());
                    assert_eq!(observation_objects(&directory), before);
                },
            );
        });
        assert_eq!(
            fs::read(fixture.work.join("a.txt")).unwrap(),
            b"unchanged source"
        );
    }

    #[test]
    fn existing_observation_owner_refuses_absent_delegated_and_native_contexts() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            assert!(exec_ctx::current().is_none());
            assert!(ExistingObservationOwner::acquire(
                &directory,
                observation.inventory_sha256,
                &writer,
                &physical
            )
            .is_err());
            let parent = ExecutionContext::default().with_resource_scope("controller");
            let scoped = parent
                .with_native_scope(fixture.scope(&[], &["a.txt"]))
                .unwrap();
            for context in [parent.without_workspace_restore(), scoped] {
                exec_ctx::with_current(context, || {
                    assert!(ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical
                    )
                    .is_err());
                });
            }
            exec_ctx::with_current(parent, || {
                let owner = ExistingObservationOwner::acquire(
                    &directory,
                    observation.inventory_sha256,
                    &writer,
                    &physical,
                )
                .unwrap();
                assert!(!owner.consumed && !owner.disk.poisoned);
            });
        });
        assert_eq!(observation_objects(&directory), before);
    }

    #[test]
    fn existing_observation_owner_success_is_one_shot_exact_pair_and_no_source_effect() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        let (metadata, blob, envelope, manifest) = observation_pair(&observation);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            exec_ctx::with_current(
                ExecutionContext::default().with_resource_scope("controller"),
                || {
                    let mut owner = ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    let (bytes, records, objects) = (
                        owner.disk.bytes,
                        owner.disk.records,
                        owner.disk.objects.len(),
                    );
                    let publication = owner.append(&observation).unwrap();
                    assert_eq!(publication.record, manifest);
                    assert_eq!(publication.before, observation.inventory_sha256);
                    assert_eq!(publication.after, inventory_revision(&directory).unwrap());
                    assert_ne!(publication.before, publication.after);
                    assert_eq!(owner.disk.bytes, bytes + metadata.len() + envelope.len());
                    assert_eq!(owner.disk.records, records + 1);
                    assert_eq!(owner.disk.objects.len(), objects + 2);
                    assert!(owner.consumed && !owner.disk.poisoned);
                    assert!(owner.append(&observation).is_err());
                    let mut expected = before.clone();
                    expected.insert(blob.clone(), metadata.clone());
                    expected.insert(manifest.clone(), envelope.clone());
                    assert_eq!(observation_objects(&directory), expected);
                },
            );
        });
        let records = inspect_controller_records(&directory).unwrap();
        let record = records.iter().find(|record| record.id == manifest).unwrap();
        assert_eq!(record.label, super::super::recovery::OBSERVATION_LABEL);
        assert_eq!(record.owner, "controller");
        assert_eq!(record.metadata, metadata);
        assert!(record.images.is_empty());
        assert_eq!(
            fs::read(fixture.work.join("a.txt")).unwrap(),
            b"unchanged source"
        );
    }

    #[test]
    fn existing_observation_owner_preflights_complete_pair_before_metadata_blob() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        for quota in ["bytes", "records", "objects"] {
            let fixture = Fixture::new();
            let (directory, observation) = observation_fixture(&fixture);
            let before = observation_objects(&directory);
            let (metadata, blob, _, manifest) = observation_pair(&observation);
            with_test_lock_home(&fixture.home, || {
                let (writer, physical) = fixture.guards();
                exec_ctx::with_current(
                    ExecutionContext::default().with_resource_scope("controller"),
                    || {
                        let mut owner = ExistingObservationOwner::acquire(
                            &directory,
                            observation.inventory_sha256,
                            &writer,
                            &physical,
                        )
                        .unwrap();
                        // Owned in-memory accounting injects boundaries without allocating a 64 MiB store.
                        match quota {
                            "bytes" => {
                                owner.disk.reserved_bytes = MAX_STORE_BYTES
                                    - OWNER_MARKER.len()
                                    - owner.disk.bytes
                                    - metadata.len()
                            }
                            "records" => {
                                owner.disk.reserved_records = MAX_RECORDS - owner.disk.records
                            }
                            "objects" => {
                                owner.disk.reserved_objects =
                                    MAX_OBJECTS - owner.disk.objects.len() - 1
                            }
                            _ => unreachable!(),
                        }
                        assert!(owner
                            .storage
                            .reserve(&owner.disk, &[(&blob, metadata.len())], 0)
                            .is_ok());
                        assert!(owner.append(&observation).is_err(), "{quota}");
                        assert!(owner.consumed && owner.disk.poisoned);
                        assert!(owner.append(&observation).is_err());
                        assert!(!directory.join(&blob).exists());
                        assert!(!directory.join(&manifest).exists());
                        assert_eq!(observation_objects(&directory), before);
                    },
                );
            });
        }
    }

    #[test]
    fn existing_observation_owner_publication_fault_retains_evidence_and_consumes_holder() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        for point in [Fault::BeforePublish, Fault::AfterPublish] {
            let fixture = Fixture::new();
            let (directory, observation) = observation_fixture(&fixture);
            let before = observation_objects(&directory);
            let (metadata, blob, _, manifest) = observation_pair(&observation);
            with_test_lock_home(&fixture.home, || {
                let (writer, physical) = fixture.guards();
                exec_ctx::with_current(
                    ExecutionContext::default().with_resource_scope("controller"),
                    || {
                        let mut owner = ExistingObservationOwner::acquire(
                            &directory,
                            observation.inventory_sha256,
                            &writer,
                            &physical,
                        )
                        .unwrap();
                        FAULT.with(|slot| slot.set(Some(point)));
                        let result = owner.append(&observation);
                        let remaining_fault = FAULT.with(|slot| slot.replace(None));
                        assert!(remaining_fault.is_none());
                        let error = result.unwrap_err().to_string();
                        assert!(error.contains("persisted evidence may remain"));
                        assert!(owner.consumed && owner.disk.poisoned);
                        assert!(owner.append(&observation).is_err());
                        let mut expected = before.clone();
                        if point == Fault::AfterPublish {
                            expected.insert(blob.clone(), metadata.clone());
                        }
                        assert_eq!(observation_objects(&directory), expected);
                        assert!(!directory.join(&manifest).exists());
                    },
                );
            });
            assert_eq!(
                fs::read(fixture.work.join("a.txt")).unwrap(),
                b"unchanged source"
            );
            // Current fault hooks reach the first (metadata blob) publication only, not the manifest.
        }
    }

    #[test]
    fn existing_observation_owner_preflights_expanded_records_before_either_object() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        let fixture = Fixture::new();
        let (directory, observation) = observation_fixture(&fixture);
        let before = observation_objects(&directory);
        let (metadata, blob, _, manifest) = observation_pair(&observation);
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            exec_ctx::with_current(
                ExecutionContext::default().with_resource_scope("controller"),
                || {
                    let mut owner = ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    let (_, expanded) = inspect_records_with_expansion(
                        &owner.storage,
                        &owner.disk,
                        &observation.base,
                    )
                    .unwrap();
                    let exact = expanded + metadata.len();
                    // Exercise the real one-shot publication with a smaller private test-only budget.
                    let error = owner.append_bounded(&observation, exact - 1).unwrap_err();
                    assert!(format!("{error:#}").contains("expansion exceeds bound"));
                    assert!(owner.consumed && owner.disk.poisoned);
                    assert_eq!(observation_objects(&directory), before);
                    assert!(!directory.join(&blob).exists() && !directory.join(&manifest).exists());
                    drop(owner);
                    let mut owner = ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    owner.append_bounded(&observation, exact).unwrap();
                    let (records, after) = inspect_records_with_expansion(
                        &owner.storage,
                        &owner.disk,
                        &observation.base,
                    )
                    .unwrap();
                    assert_eq!(after, exact);
                    assert!(records.iter().any(|record| record.id == manifest));
                },
            );
        });
        assert_eq!(inspect_controller_records(&directory).unwrap().len(), 2);
        assert_eq!(
            fs::read(fixture.work.join("a.txt")).unwrap(),
            b"unchanged source"
        );
    }

    #[test]
    fn inspected_expansion_counts_shared_metadata_and_each_image_reference() {
        let fixture = Fixture::new();
        let base = fixture
            .capture(&[fixture.scope(&["read-only.txt"], &[])])
            .unwrap();
        let metadata = b"{\"shared\":true}";
        let image = b"shared image";
        base.controller_record("owner-a", "first", metadata, &[image, image, image])
            .unwrap();
        base.controller_record("owner-b", "second", metadata, &[image, image])
            .unwrap();
        let disk = base.0.lock_disk().unwrap();
        let (records, expanded) =
            inspect_records_with_expansion(&base.0.storage, &disk, base.id()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(expanded, 2 * metadata.len() + 5 * image.len());
        assert_eq!(
            disk.objects
                .keys()
                .filter(|name| name.starts_with("b-"))
                .count(),
            2
        );
        assert_eq!(
            checked_inspection_expansion(MAX_STORE_BYTES - 1, 1, MAX_STORE_BYTES).unwrap(),
            MAX_STORE_BYTES
        );
        assert!(checked_inspection_expansion(MAX_STORE_BYTES, 1, MAX_STORE_BYTES).is_err());
        assert!(checked_inspection_expansion(usize::MAX, 1, MAX_STORE_BYTES).is_err());
        assert!(checked_inspection_expansion(1, 0, 0).is_err());
    }

    #[test]
    fn existing_observation_owner_manifest_faults_retain_objects_without_false_success() {
        use crate::core::exec_ctx::{self, ExecutionContext};
        for point in [
            Fault::BeforeObservationManifest,
            Fault::AfterObservationManifest,
        ] {
            let fixture = Fixture::new();
            let (directory, observation) = observation_fixture(&fixture);
            let before = observation_objects(&directory);
            let (metadata, blob, envelope, manifest) = observation_pair(&observation);
            with_test_lock_home(&fixture.home, || {
                let (writer, physical) = fixture.guards();
                exec_ctx::with_current(
                    ExecutionContext::default().with_resource_scope("controller"),
                    || {
                        let mut owner = ExistingObservationOwner::acquire(
                            &directory,
                            observation.inventory_sha256,
                            &writer,
                            &physical,
                        )
                        .unwrap();
                        FAULT.with(|slot| slot.set(Some(point)));
                        let result = owner.append(&observation);
                        assert!(FAULT.with(|slot| slot.replace(None)).is_none());
                        assert!(result
                            .unwrap_err()
                            .to_string()
                            .contains("persisted evidence may remain"));
                        assert!(owner.consumed && owner.disk.poisoned);
                        assert!(owner.append(&observation).is_err());
                        let mut expected = before.clone();
                        expected.insert(blob.clone(), metadata.clone());
                        if point == Fault::AfterObservationManifest {
                            expected.insert(manifest.clone(), envelope.clone());
                        }
                        assert_eq!(observation_objects(&directory), expected);
                    },
                );
            });
            let records = inspect_controller_records(&directory).unwrap();
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record.label == super::super::recovery::OBSERVATION_LABEL)
                    .count(),
                usize::from(point == Fault::AfterObservationManifest)
            );
            assert_ne!(
                inventory_revision(&directory).unwrap(),
                observation.inventory_sha256
            );
            assert_eq!(
                fs::read(fixture.work.join("a.txt")).unwrap(),
                b"unchanged source"
            );
        }
    }

    #[test]
    fn existing_observation_owner_changed_context_or_missing_prepared_consumes_without_publication()
    {
        use crate::core::exec_ctx::{self, ExecutionContext};
        for failure in ["scope", "delegated", "prepared"] {
            let fixture = Fixture::new();
            let (directory, mut observation) = observation_fixture(&fixture);
            let before = observation_objects(&directory);
            with_test_lock_home(&fixture.home, || {
                let (writer, physical) = fixture.guards();
                let parent = ExecutionContext::default().with_resource_scope("controller");
                exec_ctx::with_current(parent.clone(), || {
                    let mut owner = ExistingObservationOwner::acquire(
                        &directory,
                        observation.inventory_sha256,
                        &writer,
                        &physical,
                    )
                    .unwrap();
                    if failure == "prepared" {
                        observation.prepared = format!("m-{}.json", "f".repeat(64));
                    }
                    let context = match failure {
                        "scope" => parent.with_resource_scope("other-observer"),
                        "delegated" => parent.without_workspace_restore(),
                        _ => parent.clone(),
                    };
                    exec_ctx::with_current(context, || {
                        assert!(owner.append(&observation).is_err());
                    });
                    assert!(owner.consumed && owner.disk.poisoned);
                    assert!(owner.append(&observation).is_err());
                    assert_eq!(observation_objects(&directory), before);
                });
            });
        }
    }

    #[test]
    fn owner_cas_isolation_aba_and_no_live_source_writes() {
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"base").unwrap();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let a = base.create_overlay("parent/task/a", &scope).unwrap();
        let b = base.create_overlay("parent/task/b", &scope).unwrap();
        let narrowed = fixture.scope(&["a.txt"], &[]);
        a.check_owner_scope("parent/task/a", &narrowed).unwrap();
        assert!(a.same_handle(&a.clone()));
        assert!(!a.same_handle(&b));
        assert!(a.check_owner_scope("parent/task/b", &scope).is_err());
        assert!(a.check_owner_scope("parent/task/a/child", &scope).is_err());
        assert!(a.body_lease("parent/task/a/child").is_err());
        let fa = a.resolve(&fixture.work, Path::new("a.txt")).unwrap();
        let fb = b.resolve(&fixture.work, Path::new("a.txt")).unwrap();
        a.check_file(&fa).unwrap();
        assert!(a.check_file(&fb).is_err());
        let ba = a.body_lease(a.owner()).unwrap();
        let bb = b.body_lease(b.owner()).unwrap();
        let ra = a.snapshot(&fa, &ba).unwrap();
        let rb = b.snapshot(&fb, &bb).unwrap();
        assert!(a.snapshot(&fb, &ba).is_err());
        assert!(a.snapshot(&fa, &bb).is_err());
        assert!(a
            .compare_write(&fa, &rb, b"bad", WriteProvenance::Rewrite, &ba)
            .is_err());
        assert_eq!(
            a.compare_write(&fa, &ra, b"one", WriteProvenance::Rewrite, &ba)
                .unwrap(),
            1
        );
        assert!(a.validate_snapshot(&fa, &ra, &ba).is_err());
        assert!(a
            .compare_write(&fa, &ra, b"stale", WriteProvenance::Rewrite, &ba)
            .is_err());
        let next = a.snapshot(&fa, &ba).unwrap();
        a.compare_write(&fa, &next, b"base", WriteProvenance::Rewrite, &ba)
            .unwrap();
        assert!(a
            .compare_write(&fa, &ra, b"ABA", WriteProvenance::Rewrite, &ba)
            .is_err());
        assert_eq!(
            b.snapshot(&fb, &bb).unwrap().bytes(),
            Some(b"base".as_slice())
        );
        assert_eq!(fs::read(fixture.work.join("a.txt")).unwrap(), b"base");
        assert!(a.seal(a.owner()).is_err());
        drop(ba);
        drop(bb);
        assert!(a.seal("parent/task/b").is_err());
        let sealed = a.seal(a.owner()).unwrap();
        assert!(sealed.belongs_to(&base));
        assert!(
            sealed.changes().unwrap().is_empty(),
            "net unchanged bytes are not proposed changes"
        );
        assert_eq!(sealed.observations().unwrap().len(), 2);
        assert!(a.body_lease(a.owner()).is_err());
        fixture.revalidate(&base).unwrap();
    }

    #[test]
    fn sealed_lookup_and_reads_ignore_live_source_but_controller_revalidation_does_not() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.work.join("nested")).unwrap();
        fs::write(fixture.work.join("nested/a.txt"), b"captured\0\xff").unwrap();
        let scope = fixture.scope(&[], &["nested/a.txt", "nested/missing.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        fs::rename(fixture.work.join("nested"), fixture.work.join("old-nested")).unwrap();
        fs::create_dir(fixture.work.join("nested")).unwrap();
        fs::write(fixture.work.join("nested/a.txt"), b"uncaptured live").unwrap();
        fs::write(
            fixture.work.join("nested/missing.txt"),
            b"uncaptured new file",
        )
        .unwrap();
        // Creating child authority also uses sealed bindings, never live scope.check_narrowing.
        let overlay = base.create_overlay("child", &scope).unwrap();
        overlay.check_owner_scope("child", &scope).unwrap();
        let body = overlay.body_lease("child").unwrap();
        let existing = overlay
            .resolve(&fixture.work, Path::new("nested/a.txt"))
            .unwrap();
        let missing = overlay
            .resolve(&fixture.work, Path::new("nested/missing.txt"))
            .unwrap();
        assert_eq!(
            overlay.snapshot(&existing, &body).unwrap().bytes(),
            Some(b"captured\0\xff".as_slice())
        );
        let read = overlay.snapshot(&missing, &body).unwrap();
        assert_eq!(read.bytes(), None);
        assert_eq!(read.fingerprint(), &FileFingerprint::missing());
        overlay
            .compare_write(
                &missing,
                &read,
                b"isolated creation",
                WriteProvenance::Rewrite,
                &body,
            )
            .unwrap();
        assert!(fixture.revalidate(&base).is_err());
        assert_eq!(
            fs::read(fixture.work.join("nested/missing.txt")).unwrap(),
            b"uncaptured new file"
        );
        assert!(overlay
            .resolve(&fixture.work, Path::new("../escape.txt"))
            .is_err());
        assert!(overlay
            .resolve(fixture.temp.path(), Path::new("work/nested/a.txt"))
            .is_err());
    }

    #[test]
    fn missing_images_read_set_and_localized_provenance_are_exact() {
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"prefix OLD suffix").unwrap();
        let scope = fixture.scope(&["read-only.txt"], &["a.txt", "created.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("child", &scope).unwrap();
        let body = overlay.body_lease("child").unwrap();
        let created = overlay
            .resolve(&fixture.work, Path::new("created.txt"))
            .unwrap();
        let missing = overlay.snapshot(&created, &body).unwrap();
        assert!(overlay
            .compare_write(
                &created,
                &missing,
                b"new",
                WriteProvenance::Localized { start: 0, end: 0 },
                &body
            )
            .is_err());
        overlay
            .compare_write(
                &created,
                &missing,
                b"new\0\xfe",
                WriteProvenance::Rewrite,
                &body,
            )
            .unwrap();
        let readonly = overlay
            .resolve(&fixture.work, Path::new("read-only.txt"))
            .unwrap();
        assert!(!readonly.is_writable());
        let read = overlay.snapshot(&readonly, &body).unwrap();
        assert!(overlay
            .compare_write(
                &readonly,
                &read,
                b"forbidden",
                WriteProvenance::Rewrite,
                &body
            )
            .is_err());
        let file = overlay.resolve(&fixture.work, Path::new("a.txt")).unwrap();
        let before = overlay.snapshot(&file, &body).unwrap();
        assert!(overlay
            .compare_write(
                &file,
                &before,
                b"changed everywhere",
                WriteProvenance::Localized { start: 7, end: 10 },
                &body
            )
            .is_err());
        overlay
            .compare_write(
                &file,
                &before,
                b"prefix NEW suffix",
                WriteProvenance::Localized { start: 7, end: 10 },
                &body,
            )
            .unwrap();
        // Write success never records an invented postimage observation.
        assert_eq!(overlay.0.state.lock().unwrap().observations.len(), 3);
        drop(body);
        let sealed = overlay.seal("child").unwrap();
        let observations = sealed.observations().unwrap();
        assert_eq!(observations.len(), 3);
        assert!(
            observations
                .iter()
                .filter(|r| r.fingerprint() == &FileFingerprint::missing())
                .count()
                == 2
        );
        let changes = sealed.changes().unwrap();
        assert_eq!(changes.len(), 2);
        let changed = changes
            .iter()
            .find(|change| change.resource().path().ends_with("a.txt"))
            .unwrap();
        assert_eq!(
            changed.edits()[0].provenance(),
            WriteProvenance::Localized { start: 7, end: 10 }
        );
        assert_eq!(changed.edits()[0].bytes(), b"prefix NEW suffix");
        let new = changes
            .iter()
            .find(|change| change.resource().path().ends_with("created.txt"))
            .unwrap();
        assert_eq!(new.base_bytes(), None);
        assert_eq!(new.bytes(), b"new\0\xfe");
        assert!(!fixture.work.join("created.txt").exists());
        assert!(!fixture.work.join("read-only.txt").exists());
    }

    #[test]
    fn worker_clone_blocks_sealing_and_physically_retains_os_ownership() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("child", &scope).unwrap();
        let body = overlay.body_lease("child").unwrap();
        let worker = body.clone();
        drop(body);
        assert!(overlay.seal("child").is_err());
        let lock_path = base.0.storage.lock_guard.path().to_path_buf();
        let (release, wait) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(worker);
        });
        drop(overlay);
        drop(base);
        assert!(RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO).is_err());
        release.send(()).unwrap();
        thread.join().unwrap();
        RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO).unwrap();
    }

    #[test]
    fn sealed_manifest_identity_is_stable_and_tracks_generation() {
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"base").unwrap();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("parent/task", &scope).unwrap();
        let before = overlay.0.state.lock().unwrap().manifest.clone();
        let body = overlay.body_lease(overlay.owner()).unwrap();
        let file = overlay.resolve(&fixture.work, Path::new("a.txt")).unwrap();
        let read = overlay.snapshot(&file, &body).unwrap();
        overlay
            .compare_write(&file, &read, b"edited", WriteProvenance::Rewrite, &body)
            .unwrap();
        drop(body);
        let sealed = overlay.seal(overlay.owner()).unwrap();
        let identity = sealed.manifest_identity().unwrap();
        assert_eq!(identity.base, base.id());
        assert_eq!(identity.overlay, overlay.id());
        assert_eq!(identity.owner, overlay.owner());
        assert_eq!(identity.generation, 1);
        assert_ne!(identity.manifest, before);
        assert_eq!(sealed.manifest_identity().unwrap(), identity);
    }

    #[test]
    fn sealed_manifest_identity_rejects_corruption_and_unknown_fields() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("parent/task", &scope).unwrap();
        let sealed = overlay.seal(overlay.owner()).unwrap();
        let manifest_name = sealed.0.state.lock().unwrap().manifest.clone();
        let manifest_path = base.0.storage.directory.join(&manifest_name);
        fs::write(&manifest_path, b"not a manifest").unwrap();
        assert!(sealed.manifest_identity().is_err());

        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("parent/task", &scope).unwrap();
        let sealed = overlay.seal(overlay.owner()).unwrap();
        let manifest_name = sealed.0.state.lock().unwrap().manifest.clone();
        let manifest_path = base.0.storage.directory.join(&manifest_name);
        let original = fs::read(&manifest_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        let unknown = serde_json::to_vec(&value).unwrap();
        let mut state = sealed.0.state.lock().unwrap();
        let mut disk = base.0.lock_disk().unwrap();
        let replacement = base
            .0
            .storage
            .publish_manifest(&mut disk, &unknown)
            .unwrap();
        state.manifest = replacement;
        drop(disk);
        drop(state);
        assert!(sealed.manifest_identity().is_err());
    }

    fn publish_test_manifest(base: &BaseSnapshot, sealed: &SealedOverlay, bytes: &[u8]) {
        let mut state = sealed.0.state.lock().unwrap();
        let mut disk = base.0.lock_disk().unwrap();
        state.manifest = base.0.storage.publish_manifest(&mut disk, bytes).unwrap();
    }

    #[test]
    fn sealed_manifest_identity_detects_same_length_corruption_and_content_address_substitution() {
        for replacement in [false, true] {
            let fixture = Fixture::new();
            let scope = fixture.scope(&[], &["a.txt"]);
            let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
            let overlay = base.create_overlay("parent/task", &scope).unwrap();
            let sealed = overlay.seal(overlay.owner()).unwrap();
            let mut state = sealed.0.state.lock().unwrap();
            let path = base.0.storage.directory.join(&state.manifest);
            let mut bytes = fs::read(&path).unwrap();
            if replacement {
                let alias = format!("m-{}.json", "f".repeat(64));
                let mut disk = base.0.lock_disk().unwrap();
                base.0.storage.publish(&mut disk, &alias, &bytes).unwrap();
                state.manifest = alias;
            } else {
                bytes[0] ^= 1;
                fs::write(path, &bytes).unwrap();
            }
            drop(state);
            assert!(
                sealed.manifest_identity().is_err(),
                "replacement: {replacement}"
            );
        }
    }

    #[test]
    fn sealed_manifest_identity_refuses_nonquiescent_or_unsealed_state() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("parent/task", &scope).unwrap();
        let body = overlay.body_lease(overlay.owner()).unwrap();
        let not_sealed = SealedOverlay(overlay.0.clone());
        assert!(not_sealed.manifest_identity().is_err());
        assert!(overlay.seal(overlay.owner()).is_err());
        drop(body);
        let sealed = overlay.seal(overlay.owner()).unwrap();
        sealed.0.state.lock().unwrap().active_bodies = 1;
        assert!(sealed.manifest_identity().is_err());
        sealed.0.state.lock().unwrap().active_bodies = 0;
        sealed.manifest_identity().unwrap();
    }

    #[test]
    fn sealed_manifest_identity_rejects_rebound_canonical_manifests() {
        for field in [
            "base",
            "overlay",
            "owner",
            "generation",
            "sealed",
            "binding",
            "version",
            "observations",
            "edits",
        ] {
            let fixture = Fixture::new();
            let scope = fixture.scope(&[], &["a.txt"]);
            let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
            let overlay = base.create_overlay("parent/task", &scope).unwrap();
            let sealed = overlay.seal(overlay.owner()).unwrap();
            let state = sealed.0.state.lock().unwrap();
            let disk = base.0.lock_disk().unwrap();
            let bytes = base.0.storage.read_object(&disk, &state.manifest).unwrap();
            let mut manifest: SealedOverlayManifest = serde_json::from_slice(&bytes).unwrap();
            drop(disk);
            drop(state);
            match field {
                "base" => manifest.base = "f".repeat(32),
                "overlay" => manifest.overlay = "overlay-2".into(),
                "owner" => manifest.owner = "parent/other".into(),
                "generation" => manifest.generation = 1,
                "sealed" => manifest.sealed = false,
                "binding" => manifest.bindings[0].writable = false,
                "version" => manifest.files.values_mut().next().unwrap().generation = 1,
                "observations" => manifest.observations.push(Observation {
                    key: manifest.bindings[0].key.clone(),
                    generation: 1,
                    image: Image::missing(),
                }),
                "edits" => manifest.edits.push(Edit {
                    key: manifest.bindings[0].key.clone(),
                    generation: 1,
                    before: Image::missing(),
                    after: Image::bytes(b"invented"),
                    provenance: WriteProvenance::Rewrite,
                }),
                _ => unreachable!(),
            }
            publish_test_manifest(&base, &sealed, &encode_meta(&manifest).unwrap());
            assert!(sealed.manifest_identity().is_err(), "{field}");
            assert!(!fixture.work.join("a.txt").exists());
        }
    }

    #[test]
    fn sealed_manifest_decoder_rejects_nested_unknown_missing_duplicate_and_oversized_data() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("parent/task", &scope).unwrap();
        let sealed = overlay.seal(overlay.owner()).unwrap();
        let state = sealed.0.state.lock().unwrap();
        let disk = base.0.lock_disk().unwrap();
        let bytes = base.0.storage.read_object(&disk, &state.manifest).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for pointer in ["", "/bindings/0", "/bindings/0/base"] {
            let mut mutated = value.clone();
            mutated
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unexpected".into(), serde_json::json!(true));
            assert!(
                serde_json::from_value::<SealedOverlayManifest>(mutated).is_err(),
                "{pointer}"
            );
        }
        for field in [
            "generation",
            "sealed",
            "bindings",
            "files",
            "observations",
            "edits",
        ] {
            let mut mutated = value.clone();
            mutated.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<SealedOverlayManifest>(mutated).is_err(),
                "{field}"
            );
        }
        let key = &value["bindings"][0]["key"];
        let version = &value["files"][key.as_str().unwrap()];
        let duplicate_files = format!("{{{key}:{version},{key}:{version}}}");
        let text = String::from_utf8(bytes.clone()).unwrap();
        let manifest: SealedOverlayManifest = serde_json::from_slice(&bytes).unwrap();
        let original_files = serde_json::to_string(&manifest.files).unwrap();
        assert!(text.contains(&original_files));
        assert!(
            decode_sealed_manifest(text.replace(&original_files, &duplicate_files).as_bytes())
                .is_err()
        );
        let duplicate_field =
            text.replacen("\"sealed\":true", "\"sealed\":false,\"sealed\":true", 1);
        assert!(decode_sealed_manifest(duplicate_field.as_bytes()).is_err());
        assert!(decode_sealed_manifest(&vec![b' '; MAX_META_BYTES + 1]).is_err());
        let mut manifest: SealedOverlayManifest = serde_json::from_slice(&bytes).unwrap();
        manifest.owner = "x".repeat(MAX_OWNER_BYTES + 1);
        assert!(decode_sealed_manifest(&encode_meta(&manifest).unwrap()).is_err());
        manifest.owner = overlay.owner().to_owned();
        manifest.observations = vec![
            Observation {
                key: "unknown".into(),
                generation: 0,
                image: Image::missing()
            };
            MAX_OBSERVATIONS + 1
        ];
        assert!(decode_sealed_manifest(&encode_meta(&manifest).unwrap()).is_err());
    }

    #[test]
    fn sealed_lineage_restart_requires_exact_immutable_manifest_and_base() {
        for label in ["integration-prepared", "integration-conflict"] {
            for mismatch in [
                "none",
                "missing",
                "base",
                "owner",
                "overlay",
                "generation",
                "kind",
                "binding",
                "observation",
            ] {
                let fixture = Fixture::new();
                fs::write(fixture.work.join("a.txt"), b"base").unwrap();
                let scope = fixture.scope(&[], &["a.txt"]);
                let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
                let overlay = base.create_overlay("parent/task", &scope).unwrap();
                let sealed = overlay.seal(overlay.owner()).unwrap();
                let identity = sealed.manifest_identity().unwrap();
                let mut selected = identity.manifest.clone();
                if mismatch == "missing" {
                    selected = format!("m-{}.json", "f".repeat(64));
                } else if !matches!(mismatch, "none" | "observation") {
                    let mut disk = base.0.lock_disk().unwrap();
                    let bytes = base.0.storage.read_object(&disk, &selected).unwrap();
                    let mut manifest: SealedOverlayManifest =
                        serde_json::from_slice(&bytes).unwrap();
                    match mismatch {
                        "base" => manifest.base = "f".repeat(32),
                        "owner" => manifest.owner = "parent/other".into(),
                        "overlay" => manifest.overlay = "overlay-2".into(),
                        "generation" => manifest.generation = 1,
                        "kind" => manifest.kind = "base".into(),
                        "binding" => manifest.bindings[0].base = Image::bytes(b"foreign"),
                        _ => unreachable!(),
                    }
                    selected = base
                        .0
                        .storage
                        .publish_manifest(&mut disk, &encode_meta(&manifest).unwrap())
                        .unwrap();
                }
                let metadata = serde_json::to_vec(&serde_json::json!({
                    "overlay_lineage_version": 1, "base": base.id(), "owner": "parent",
                    "observations": if mismatch == "observation" { serde_json::json!([{
                        "author": format!("{}/{}", sealed.owner(), sealed.id()),
                        "key": scope.resources()[0].key(), "path": fixture.work.join("a.txt"),
                        "generation": 0, "fingerprint": Image::bytes(b"base"),
                    }]) } else { serde_json::json!([]) }, "files": [],
                    "overlays": [{"base": identity.base, "id": identity.overlay, "owner": identity.owner,
                        "author": format!("{}/{}", sealed.owner(), sealed.id()), "generation": 0, "manifest": selected}],
                })).unwrap();
                base.controller_record("parent", label, &metadata, &[])
                    .unwrap();
                let directory = base.directory().to_path_buf();
                let before = observation_objects(&directory);
                drop(sealed);
                drop(overlay);
                drop(base);
                let inspected = inspect_controller_records(&directory);
                assert_eq!(inspected.is_ok(), mismatch == "none", "{label}: {mismatch}");
                assert_eq!(observation_objects(&directory), before);
                assert_eq!(fs::read(fixture.work.join("a.txt")).unwrap(), b"base");
            }
        }
    }

    #[test]
    fn sealed_history_restart_rejects_unobserved_cas_and_false_localized_provenance() {
        for mismatch in ["none", "missing-read", "provenance"] {
            let fixture = Fixture::new();
            fs::write(fixture.work.join("a.txt"), b"abcdef").unwrap();
            let scope = fixture.scope(&[], &["a.txt"]);
            let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
            let overlay = base.create_overlay("parent/task", &scope).unwrap();
            let body = overlay.body_lease(overlay.owner()).unwrap();
            let file = overlay.resolve(&fixture.work, Path::new("a.txt")).unwrap();
            for (bytes, start) in [(b"aXcdef".as_slice(), 1), (b"aXYdef".as_slice(), 2)] {
                let read = overlay.snapshot(&file, &body).unwrap();
                overlay
                    .compare_write(
                        &file,
                        &read,
                        bytes,
                        WriteProvenance::Localized {
                            start,
                            end: start + 1,
                        },
                        &body,
                    )
                    .unwrap();
            }
            drop(body);
            let sealed = overlay.seal(overlay.owner()).unwrap();
            let mut identity = sealed.manifest_identity().unwrap();
            let mut manifest: SealedOverlayManifest = {
                let disk = base.0.lock_disk().unwrap();
                serde_json::from_slice(
                    &base
                        .0
                        .storage
                        .read_object(&disk, &identity.manifest)
                        .unwrap(),
                )
                .unwrap()
            };
            match mismatch {
                "missing-read" => manifest.observations.retain(|read| read.generation != 1),
                "provenance" => {
                    manifest.edits[0].provenance = WriteProvenance::Localized { start: 2, end: 3 }
                }
                _ => {}
            }
            let encoded = encode_meta(&manifest).unwrap();
            assert_eq!(
                decode_sealed_manifest(&encoded).is_ok(),
                mismatch != "missing-read"
            );
            identity.manifest = base
                .0
                .storage
                .publish_manifest(&mut base.0.lock_disk().unwrap(), &encoded)
                .unwrap();
            let author = format!("{}/{}", identity.owner, identity.overlay);
            let observations: Vec<_> = manifest
                .observations
                .iter()
                .map(|read| {
                    serde_json::json!({
                        "author": author, "key": read.key, "path": fixture.work.join("a.txt"),
                        "generation": read.generation, "fingerprint": read.image,
                    })
                })
                .collect();
            let metadata = serde_json::to_vec(&serde_json::json!({
                "overlay_lineage_version": 1, "base": base.id(), "owner": "parent", "observations": observations,
                "overlays": [{"base": identity.base, "id": identity.overlay, "owner": identity.owner,
                    "author": author, "generation": identity.generation, "manifest": identity.manifest}],
            })).unwrap();
            base.controller_record("parent", "integration-conflict", &metadata, &[])
                .unwrap();
            let directory = base.directory().to_path_buf();
            let inventory = observation_objects(&directory);
            drop(file);
            drop(sealed);
            drop(overlay);
            drop(base);
            assert_eq!(
                inspect_controller_records(&directory).is_ok(),
                mismatch == "none",
                "{mismatch}"
            );
            assert_eq!(observation_objects(&directory), inventory);
            assert_eq!(fs::read(fixture.work.join("a.txt")).unwrap(), b"abcdef");
        }
    }

    #[test]
    fn known_quota_failures_do_not_mutate_or_poison_overlay() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("child", &scope).unwrap();
        let body = overlay.body_lease("child").unwrap();
        let file = overlay.resolve(&fixture.work, Path::new("a.txt")).unwrap();
        let read = overlay.snapshot(&file, &body).unwrap();
        assert!(overlay
            .compare_write(
                &file,
                &read,
                &vec![0; MAX_FILE_BYTES + 1],
                WriteProvenance::Rewrite,
                &body
            )
            .is_err());
        let before = base.0.disk.lock().unwrap().bytes;
        base.0.disk.lock().unwrap().bytes = MAX_STORE_BYTES;
        assert!(overlay
            .compare_write(
                &file,
                &read,
                b"does not fit",
                WriteProvenance::Rewrite,
                &body
            )
            .is_err());
        assert_eq!(overlay.0.state.lock().unwrap().generation, 0);
        assert!(!base.0.disk.lock().unwrap().poisoned);
        base.0.disk.lock().unwrap().bytes = before;
        let records = base.0.disk.lock().unwrap().records;
        base.0.disk.lock().unwrap().records = MAX_RECORDS;
        assert!(overlay
            .compare_write(
                &file,
                &read,
                b"record overflow",
                WriteProvenance::Rewrite,
                &body
            )
            .is_err());
        assert_eq!(overlay.0.state.lock().unwrap().generation, 0);
        assert!(!base.0.disk.lock().unwrap().poisoned);
        base.0.disk.lock().unwrap().records = records;
        overlay.validate_snapshot(&file, &read, &body).unwrap();
        overlay
            .compare_write(&file, &read, b"now fits", WriteProvenance::Rewrite, &body)
            .unwrap();
        assert!(!file.path().exists());
    }

    #[test]
    fn aggregate_record_and_metadata_quotas_are_bounded_before_allocation() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(&[scope]).unwrap();
        let mut disk = base.0.disk.lock().unwrap();
        disk.bytes = MAX_STORE_BYTES - OWNER_MARKER.len() - 1;
        assert!(base.0.storage.reserve(&disk, &[("new", 2)], 0).is_err());
        assert!(base.0.storage.reserve(&disk, &[("new", 1)], 0).is_ok());
        disk.records = MAX_RECORDS;
        assert!(base.0.storage.reserve(&disk, &[], 1).is_err());
        assert!(encode_meta(&"x".repeat(MAX_META_BYTES + 1)).is_err());
    }

    #[test]
    fn reservation_counts_owner_marker_once_at_capacity_boundaries() {
        let fixture = Fixture::new();
        let base = fixture.capture(&[fixture.scope(&[], &["a.txt"])]).unwrap();
        let mut disk = DiskState {
            bytes: MAX_STORE_BYTES - OWNER_MARKER.len(),
            ..DiskState::default()
        };
        disk.objects
            .insert("deduplicated".into(), Image::bytes(b"existing"));
        let reserve =
            |disk: &DiskState, names: &[(&str, usize)]| base.0.storage.reserve(disk, names, 0);
        assert!(reserve(&disk, &[]).is_ok());
        assert!(reserve(&disk, &[("deduplicated", MAX_STORE_BYTES)]).is_ok());
        assert!(reserve(&disk, &[("zero", 0)]).is_ok());
        assert!(reserve(&disk, &[("new", 1)]).is_err());
        disk.bytes -= 1;
        assert!(
            reserve(&disk, &[("new", 1), ("new", 1)]).is_ok(),
            "new name charged once"
        );
        assert!(reserve(&disk, &[("new", 2)]).is_err());
        disk.reserved_bytes = 1;
        assert!(reserve(&disk, &[]).is_ok());
        assert!(reserve(&disk, &[("deduplicated", 9)]).is_ok());
        assert!(reserve(&disk, &[("new", 1)]).is_err());
        disk.reserved_bytes = 2;
        assert!(reserve(&disk, &[]).is_err());
        disk.bytes = usize::MAX;
        disk.reserved_bytes = 0;
        assert!(reserve(&disk, &[]).is_err());
        assert!(!disk.poisoned);
    }

    #[test]
    fn capture_enforces_child_cohort_and_source_limits() {
        let fixture = Fixture::new();
        let mut scopes = Vec::new();
        for chunk in 0..8 {
            let paths: Vec<_> = (0..MAX_CHILD_FILES)
                .map(|n| format!("f-{}", chunk * MAX_CHILD_FILES + n))
                .collect();
            scopes.push(NativeWriteScope::resolve(&fixture.work, &paths).unwrap());
        }
        let base = fixture.capture(&scopes).unwrap();
        assert_eq!(base.resources().len(), MAX_COHORT_FILES);
        scopes.push(fixture.scope(&[], &["overflow.txt"]));
        assert!(fixture.capture(&scopes).is_err());
        let too_many: Vec<_> = (0..=MAX_CHILD_FILES).map(|n| format!("new-{n}")).collect();
        assert!(NativeWriteScope::resolve(&fixture.work, &too_many).is_err());
        fs::write(fixture.work.join("large.txt"), vec![0; MAX_FILE_BYTES + 1]).unwrap();
        assert!(fixture
            .capture(&[fixture.scope(&[], &["large.txt"])])
            .is_err());
    }

    #[test]
    fn capture_refuses_outside_internal_and_unowned_root_bindings() {
        let fixture = Fixture::new();
        let outside =
            NativeWriteScope::resolve(fixture.temp.path(), &["outside.txt".into()]).unwrap();
        assert!(fixture.capture(&[outside]).is_err());
        fs::create_dir(fixture.work.join(".git")).unwrap();
        assert!(fixture
            .capture(&[fixture.scope(&[], &[".git/config"])])
            .is_err());
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let other = fixture.scope(&[], &["b.txt"]);
        assert!(base.create_overlay("child", &other).is_err());
        let read_only = fixture.scope(&["a.txt"], &[]);
        let read_base = fixture.capture(&[read_only]).unwrap();
        assert!(read_base.create_overlay("child", &scope).is_err());
        with_test_lock_home(&fixture.home, || {
            let (writer, physical) = fixture.guards();
            assert!(BaseSnapshot::capture(
                &fixture.identity,
                std::slice::from_ref(&scope),
                &fixture.work,
                &writer,
                &physical
            )
            .is_err());
            assert!(BaseSnapshot::capture(
                &fixture.identity,
                &[scope],
                fixture.temp.path(),
                &writer,
                &physical
            )
            .is_err());
            let mut mismatched = fixture.identity.clone();
            mismatched.worktree_id.push('x');
            assert!(BaseSnapshot::capture(
                &mismatched,
                &[other],
                &fixture.store,
                &writer,
                &physical
            )
            .is_err());
        });
    }

    #[test]
    fn hardlinks_to_sources_and_store_objects_are_refused() {
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"base").unwrap();
        let scope = fixture.scope(&[], &["a.txt"]);
        fs::hard_link(fixture.work.join("a.txt"), fixture.work.join("alias.txt")).unwrap();
        assert!(fixture.capture(std::slice::from_ref(&scope)).is_err());
        fs::remove_file(fixture.work.join("alias.txt")).unwrap();
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("child", &scope).unwrap();
        let blob = base
            .0
            .files
            .values()
            .next()
            .unwrap()
            .image
            .blob_name()
            .unwrap();
        fs::hard_link(
            base.0.storage.directory.join(&blob),
            fixture.temp.path().join("stolen-blob"),
        )
        .unwrap();
        assert!(overlay.body_lease("child").is_err());
        assert!(base.0.disk.lock().unwrap().poisoned);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_ancestor_leaf_and_store_namespace_attacks_are_refused() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        symlink(fixture.work.join("absent"), fixture.work.join("a.txt")).unwrap();
        assert!(fixture.capture(&[scope]).is_err());
        fs::remove_file(fixture.work.join("a.txt")).unwrap();
        symlink(&fixture.store, fixture.work.join("linked")).unwrap();
        assert!(NativeWriteScope::resolve(&fixture.work, &["linked/a.txt".into()]).is_err());
        symlink(&fixture.home, fixture.store.join(NAMESPACE)).unwrap();
        let scope = fixture.scope(&[], &["a.txt"]);
        assert!(fixture.capture(&[scope]).is_err());
    }

    #[test]
    fn scratch_owned_namespace_is_never_used_for_durable_store() {
        let fixture = Fixture::new();
        fs::write(
            fixture.store.join(crate::core::scratch::OWNER_MARKER),
            b"untrusted marker still refuses",
        )
        .unwrap();
        let scope = fixture.scope(&[], &["a.txt"]);
        assert!(fixture.capture(&[scope]).is_err());
        assert!(!fixture.store.join(NAMESPACE).exists());
    }

    #[test]
    fn tampered_manifest_is_not_authority_and_does_not_gain_scope() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("child", &scope).unwrap();
        let file = overlay.resolve(&fixture.work, Path::new("a.txt")).unwrap();
        let body = overlay.body_lease("child").unwrap();
        let manifest = overlay.0.state.lock().unwrap().manifest.clone();
        fs::write(
            base.0.storage.directory.join(manifest),
            br#"{"owner":"other","writes":["outside"]}"#,
        )
        .unwrap();
        assert!(overlay
            .resolve(&fixture.work, Path::new("outside"))
            .is_err());
        assert!(overlay.snapshot(&file, &body).is_err());
        assert!(base.0.disk.lock().unwrap().poisoned);
        drop(body);
        assert!(overlay.seal("child").is_err());
    }

    #[test]
    fn uncertain_publication_and_crash_artifacts_fail_closed_without_live_effects() {
        for point in [Fault::BeforePublish, Fault::AfterPublish] {
            let fixture = Fixture::new();
            let scope = fixture.scope(&[], &["a.txt"]);
            let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
            let overlay = base.create_overlay("child", &scope).unwrap();
            let body = overlay.body_lease("child").unwrap();
            let file = overlay.resolve(&fixture.work, Path::new("a.txt")).unwrap();
            let read = overlay.snapshot(&file, &body).unwrap();
            FAULT.with(|slot| slot.set(Some(point)));
            assert!(overlay
                .compare_write(
                    &file,
                    &read,
                    b"private bytes",
                    WriteProvenance::Rewrite,
                    &body
                )
                .is_err());
            assert!(base.0.disk.lock().unwrap().poisoned);
            assert!(overlay.snapshot(&file, &body).is_err());
            drop(body);
            assert!(overlay.seal("child").is_err());
            assert!(!fixture.work.join("a.txt").exists());
            // A restart can only allocate a NEW cohort. Old partial disk metadata cannot mint handles.
            let restarted = fixture.capture(&[scope]).unwrap();
            assert_ne!(base.id(), restarted.id());
            assert_eq!(restarted.image(&file.resource().clone()).unwrap(), None);
        }
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let overlay = base.create_overlay("child", &scope).unwrap();
        fs::write(
            base.0.storage.directory.join(".orphan.aizen-tmp-1-2"),
            b"partial",
        )
        .unwrap();
        assert!(overlay.body_lease("child").is_err());
        assert!(base.0.disk.lock().unwrap().poisoned);
    }

    #[test]
    fn controller_capacity_is_reserved_before_effects_and_restart_is_data_only() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(&[scope]).unwrap();
        let directory = base.directory().to_path_buf();
        let mut reserved = base
            .reserve_controller_records("controller", 2, 64 * 1024, 11)
            .unwrap();
        let before_reserved = base.0.disk.lock().unwrap().reserved_bytes;
        assert!(before_reserved >= 64 * 1024 + 10);
        let id = base
            .controller_record_reserved(
                &mut reserved,
                "prepared",
                b"{\"status\":\"prepared\"}",
                &[b"before", b"after"],
            )
            .unwrap();
        assert!(id.id().starts_with("m-"));
        assert_eq!(
            base.inspect_controller_records()
                .unwrap()
                .iter()
                .find(|record| record.label == "prepared")
                .unwrap()
                .images[0],
            b"before"
        );
        base.controller_record_reserved(&mut reserved, "receipt", b"{\"status\":\"settled\"}", &[])
            .unwrap();
        assert!(base
            .controller_record_reserved(&mut reserved, "extra", b"{}", &[])
            .is_err());
        assert!(
            inspect_controller_records(&directory).is_err(),
            "live controller owns OS lock"
        );
        drop(reserved);
        assert_eq!(base.0.disk.lock().unwrap().reserved_bytes, 0);
        drop(base);
        let records = inspect_controller_records(&directory).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .any(|record| record.label == "prepared" && record.owner == "controller"));
        assert!(records
            .iter()
            .any(|record| record.metadata == b"{\"status\":\"settled\"}"));
        assert!(!fixture.work.join("a.txt").exists());
    }

    fn abandoned_directory(fixture: &Fixture) -> PathBuf {
        let base = fixture.capture(&[fixture.scope(&[], &["a.txt"])]).unwrap();
        base.directory().to_path_buf()
    }

    #[test]
    fn existing_inventory_never_recreates_missing_namespace_or_owner() {
        let fixture = Fixture::new();
        let directory = fixture
            .store
            .join(NAMESPACE)
            .join("0123456789abcdef0123456789abcdef");
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        assert!(!fixture.store.join(NAMESPACE).exists());
        let directory = abandoned_directory(&fixture);
        fs::remove_file(directory.join("owner.lock")).unwrap();
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        assert!(!directory.join("owner.lock").exists());
        fs::remove_dir_all(&directory).unwrap();
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        assert!(!directory.exists());
    }

    #[test]
    fn existing_inventory_revision_includes_orphans_and_matches_recovery_encoding() {
        let fixture = Fixture::new();
        let directory = abandoned_directory(&fixture);
        let before = inventory_revision(&directory).unwrap();
        let bytes = b"unreferenced durable evidence";
        let name = Image::bytes(bytes).blob_name().unwrap();
        fs::write(directory.join(&name), bytes).unwrap();
        let after = inventory_revision(&directory).unwrap();
        assert_ne!(before, after);
        let mut paths = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        let mut expected = ring::digest::Context::new(&ring::digest::SHA256);
        expected.update(b"aizen-tm-recovery-inventory-v1\0");
        for path in paths {
            let name = path.file_name().unwrap().to_str().unwrap();
            let image = Image::bytes(&fs::read(&path).unwrap());
            expected.update(&(name.len() as u64).to_le_bytes());
            expected.update(name.as_bytes());
            expected.update(&image.byte_len.to_le_bytes());
            expected.update(&image.sha256);
        }
        assert_eq!(after.as_slice(), expected.finish().as_ref());
        let owner = RepoTxnLock::acquire_existing(
            &directory.join("owner.lock"),
            crate::core::repo_lock::LockMode::Exclusive,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            owner.read_bounded_exclusive(1024).unwrap(),
            b"aizen-speculation-owner-v1\n"
        );
        assert_eq!(
            inventory_revision(&directory).unwrap(),
            after,
            "revision cannot read through a second owner descriptor"
        );
        let start = std::time::Instant::now();
        assert!(inspect_controller_records(&directory).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(owner);
        assert!(inspect_controller_records(&directory).unwrap().is_empty());
        fs::remove_file(directory.join(name)).unwrap();
        assert_eq!(inventory_revision(&directory).unwrap(), before);
    }

    #[test]
    fn existing_inventory_marker_is_canonical_for_revision_but_inspection_checks_actual_bytes() {
        let fixture = Fixture::new();
        let directory = abandoned_directory(&fixture);
        let before = inventory_revision(&directory).unwrap();
        let mut marker = b"aizen-speculation-owner-v1\n".to_vec();
        marker[0] = b'x';
        fs::write(directory.join("owner.lock"), &marker).unwrap();
        assert_eq!(inventory_revision(&directory).unwrap(), before);
        assert!(inspect_controller_records(&directory).is_err());
        assert_eq!(fs::read(directory.join("owner.lock")).unwrap(), marker);
        fs::write(directory.join("owner.lock"), b"short").unwrap();
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
    }

    #[test]
    fn existing_inventory_refuses_unknown_corrupt_special_and_linked_objects() {
        for name in [
            ".orphan.aizen-tmp-1-2",
            "b-not-a-content-address",
            "m-invalid.json",
        ] {
            let fixture = Fixture::new();
            let directory = abandoned_directory(&fixture);
            fs::write(directory.join(name), b"retained invalid evidence").unwrap();
            assert!(inventory_revision(&directory).is_err());
            assert!(inspect_controller_records(&directory).is_err());
            assert_eq!(
                fs::read(directory.join(name)).unwrap(),
                b"retained invalid evidence"
            );
        }
        let fixture = Fixture::new();
        let directory = abandoned_directory(&fixture);
        let name = Image::bytes(b"special").blob_name().unwrap();
        fs::create_dir(directory.join(&name)).unwrap();
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        fs::remove_dir(directory.join(&name)).unwrap();
        let outside = fixture.store.join("retained-foreign-object");
        fs::write(&outside, b"special").unwrap();
        fs::hard_link(&outside, directory.join(name)).unwrap();
        assert!(inventory_revision(&directory).is_err());
        assert!(inspect_controller_records(&directory).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"special");
    }

    #[test]
    fn existing_inventory_enforces_cumulative_object_and_manifest_limits() {
        for manifests in [false, true] {
            let fixture = Fixture::new();
            let directory = abandoned_directory(&fixture);
            let count = if manifests { MAX_RECORDS } else { MAX_OBJECTS };
            for index in 0..count {
                let bytes = format!("{{\"inventory_fixture\":{index}}}").into_bytes();
                let name = if manifests {
                    manifest_name(&bytes)
                } else {
                    Image::bytes(&bytes).blob_name().unwrap()
                };
                fs::write(directory.join(name), bytes).unwrap();
            }
            assert!(inventory_revision(&directory).is_err());
            assert!(inspect_controller_records(&directory).is_err());
            assert_eq!(
                fs::read_dir(&directory).unwrap().count(),
                count + 2,
                "quota refusal retains every object"
            );
        }
    }

    #[test]
    fn existing_inventory_aggregate_byte_limit_includes_owner_marker() {
        let fixture = Fixture::new();
        let directory = abandoned_directory(&fixture);
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() != "owner.lock" {
                fs::remove_file(path).unwrap();
            }
        }
        let mut bytes = vec![0u8; MAX_FILE_BYTES];
        let mut last = PathBuf::new();
        for index in 0..(MAX_STORE_BYTES / MAX_FILE_BYTES) {
            bytes[0] = index as u8;
            last = directory.join(Image::bytes(&bytes).blob_name().unwrap());
            fs::write(&last, &bytes).unwrap();
        }
        assert!(inventory_revision(&directory)
            .unwrap_err()
            .to_string()
            .contains("including owner marker"));
        assert!(inspect_controller_records(&directory).is_err());
        fs::remove_file(last).unwrap();
        assert!(inventory_revision(&directory).is_ok());
        assert!(inspect_controller_records(&directory).unwrap().is_empty());
    }

    #[test]
    fn controller_reservation_preflights_existing_expansion_before_deduplicated_publication() {
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"unchanged source").unwrap();
        let base = fixture.capture(&[fixture.scope(&["a.txt"], &[])]).unwrap();
        let image = vec![b'x'; 1024];
        let images = vec![image.as_slice(); 8];
        let metadata = b"{}";
        let allowance = controller_metadata(base.id(), "controller", "second", metadata, &images)
            .unwrap()
            .len()
            + metadata.len()
            + images.len() * image.len();
        let first_expanded = metadata.len() + images.len() * image.len();
        let exact = first_expanded + allowance;
        let mut first = base
            .reserve_controller_records_bounded(
                "controller",
                1,
                allowance - images.len() * image.len(),
                images.len() * image.len(),
                exact - 1,
            )
            .unwrap();
        base.controller_record_reserved(&mut first, "first", metadata, &images)
            .unwrap();
        drop(first);
        let directory = base.directory().to_path_buf();
        let before = observation_objects(&directory);
        let revision = inventory_revision(&directory).unwrap();
        let error = base
            .reserve_controller_records_bounded(
                "controller",
                1,
                allowance - images.len() * image.len(),
                images.len() * image.len(),
                exact - 1,
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("inspected expansion"));
        assert_eq!(observation_objects(&directory), before);
        assert_eq!(inventory_revision(&directory).unwrap(), revision);
        {
            let disk = base.0.disk.lock().unwrap();
            assert!(!disk.poisoned);
            assert_eq!(
                (
                    disk.reserved_bytes,
                    disk.reserved_records,
                    disk.reserved_objects
                ),
                (0, 0, 0)
            );
        }
        let mut second = base
            .reserve_controller_records_bounded(
                "controller",
                1,
                allowance - images.len() * image.len(),
                images.len() * image.len(),
                exact,
            )
            .unwrap();
        base.controller_record_reserved(&mut second, "second", metadata, &images)
            .unwrap();
        drop(second);
        let (records, expanded) = {
            let disk = base.0.disk.lock().unwrap();
            inspect_records_with_expansion(&base.0.storage, &disk, base.id()).unwrap()
        };
        assert_eq!(records.len(), 2);
        assert_eq!(expanded, 2 * first_expanded);
        assert!(expanded <= exact);
        assert_eq!(
            base.0
                .disk
                .lock()
                .unwrap()
                .objects
                .keys()
                .filter(|name| name.starts_with("b-"))
                .count(),
            3
        );
        drop(base);
        assert_eq!(inspect_controller_records(&directory).unwrap().len(), 2);
        assert_eq!(
            fs::read(fixture.work.join("a.txt")).unwrap(),
            b"unchanged source"
        );
    }

    #[test]
    fn controller_reservation_counts_outstanding_pair_and_verification_allowances() {
        let fixture = Fixture::new();
        let base = fixture.capture(&[fixture.scope(&[], &["a.txt"])]).unwrap();
        base.controller_record("controller", "prior", b"{}", &[b"xx"])
            .unwrap();
        let prior_expanded = 4;
        let verification_bytes = 1024;
        let pair_metadata = 8192;
        let pair_images = 11;
        let exact = prior_expanded + verification_bytes + pair_metadata + pair_images;
        let before = observation_objects(base.directory());
        let verification = base
            .reserve_controller_records_bounded("controller", 1, verification_bytes, 0, exact)
            .unwrap();
        assert!(base
            .reserve_controller_records_bounded(
                "controller",
                2,
                pair_metadata,
                pair_images,
                exact - 1
            )
            .is_err());
        assert_eq!(observation_objects(base.directory()), before);
        let mut pair = base
            .reserve_controller_records_bounded("controller", 2, pair_metadata, pair_images, exact)
            .unwrap();
        assert!(base
            .reserve_controller_records_bounded("controller", 1, 1, 0, exact)
            .is_err());
        base.controller_record_reserved(&mut pair, "prepared", b"{}", &[b"before", b"after"])
            .unwrap();
        {
            let disk = base.0.disk.lock().unwrap();
            let (_, expanded) =
                inspect_records_with_expansion(&base.0.storage, &disk, base.id()).unwrap();
            assert_eq!(expanded, prior_expanded + 2 + pair_images);
            assert!(expanded + disk.reserved_bytes <= exact);
        }
        let remaining = base.0.disk.lock().unwrap().reserved_bytes;
        drop(verification);
        assert_eq!(
            base.0.disk.lock().unwrap().reserved_bytes,
            remaining - verification_bytes
        );
        let replacement = base
            .reserve_controller_records_bounded("controller", 1, verification_bytes, 0, exact)
            .unwrap();
        base.controller_record_reserved(&mut pair, "receipt", b"{}", &[])
            .unwrap();
        drop(pair);
        drop(replacement);
        assert_eq!(base.0.disk.lock().unwrap().reserved_bytes, 0);
        assert_eq!(base.inspect_controller_records().unwrap().len(), 3);
        assert!(!fixture.work.join("a.txt").exists());
    }

    #[test]
    fn controller_metadata_reservations_fit_complete_worker_inventory_without_image_slots() {
        let fixture = Fixture::new();
        let base = fixture.capture(&[fixture.scope(&[], &["a.txt"])]).unwrap();
        // One roster, two terminal records for each of 32 attempts, and intent/closure for 128 bodies.
        let count = 1 + 2 * 32 + 2 * 128;
        let before = observation_objects(base.directory());
        let mut reservations = Vec::new();
        let mut remaining = count;
        while remaining > 0 {
            let chunk = remaining.min(MAX_CONTROLLER_RECORDS);
            reservations.push(
                base.reserve_controller_metadata("controller", chunk, chunk * 2048)
                    .unwrap(),
            );
            remaining -= chunk;
        }
        {
            let disk = base.0.disk.lock().unwrap();
            assert_eq!(disk.reserved_records, count);
            assert_eq!(disk.reserved_objects, 2 * count);
        }
        assert_eq!(
            observation_objects(base.directory()),
            before,
            "reservations do not publish records"
        );
        assert!(
            base.controller_record_reserved(&mut reservations[0], "forbidden-image", b"{}", &[b""])
                .is_err(),
            "zero-byte images still require image capability"
        );
        assert!(!base.0.disk.lock().unwrap().poisoned);
        let record = base
            .controller_record_reserved(&mut reservations[0], "lifecycle-fixture", b"{}", &[])
            .unwrap();
        {
            let disk = base.0.disk.lock().unwrap();
            assert_eq!(disk.reserved_records, count - 1);
            assert_eq!(disk.reserved_objects, 2 * (count - 1));
        }
        drop(reservations);
        {
            let disk = base.0.disk.lock().unwrap();
            assert_eq!(
                (
                    disk.reserved_bytes,
                    disk.reserved_records,
                    disk.reserved_objects
                ),
                (0, 0, 0)
            );
        }
        let records = base.inspect_controller_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, record.as_str());
        assert!(records.iter().all(|record| record.images.is_empty()));
        assert!(!fixture.work.join("a.txt").exists());
    }

    #[test]
    fn controller_metadata_reservation_drop_and_capacity_refusal_preserve_accounting() {
        let fixture = Fixture::new();
        let base = fixture.capture(&[fixture.scope(&[], &["a.txt"])]).unwrap();
        let metadata = base
            .reserve_controller_metadata("controller", 16, 32768)
            .unwrap();
        assert_eq!(metadata.object_slots, 32);
        assert_eq!(metadata.images_remaining, 0);
        let mut images = Vec::new();
        for _ in 0..3 {
            images.push(
                base.reserve_controller_records("controller", 1, 2048, 0)
                    .unwrap(),
            );
        }
        let before = observation_objects(base.directory());
        assert!(base
            .reserve_controller_records("controller", 1, 2048, 0)
            .is_err());
        assert_eq!(observation_objects(base.directory()), before);
        assert!(!base.0.disk.lock().unwrap().poisoned);
        drop(metadata);
        drop(images);
        let disk = base.0.disk.lock().unwrap();
        assert_eq!(
            (
                disk.reserved_bytes,
                disk.reserved_records,
                disk.reserved_objects
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn reservation_cannot_be_consumed_by_other_base_or_oversized_payload() {
        let fixture = Fixture::new();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(std::slice::from_ref(&scope)).unwrap();
        let other = fixture.capture(&[scope]).unwrap();
        let mut reserved = base
            .reserve_controller_records("controller", 2, 8192, 2)
            .unwrap();
        assert!(other
            .controller_record_reserved(&mut reserved, "prepared", b"{}", &[])
            .is_err());
        assert!(base
            .controller_record_reserved(&mut reserved, "prepared", b"{}", &[b"too large"])
            .is_err());
        assert!(!base.0.disk.lock().unwrap().poisoned);
        base.controller_record_reserved(&mut reserved, "prepared", b"{}", &[b"ok"])
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn store_directories_and_raw_blobs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        fs::write(fixture.work.join("a.txt"), b"raw\0\xff bytes").unwrap();
        let scope = fixture.scope(&[], &["a.txt"]);
        let base = fixture.capture(&[scope]).unwrap();
        for directory in [
            &fixture.store,
            &fixture.store.join(NAMESPACE),
            &base.0.storage.directory,
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let blob = base
            .0
            .files
            .values()
            .next()
            .unwrap()
            .image
            .blob_name()
            .unwrap();
        let path = base.0.storage.directory.join(blob);
        assert_eq!(fs::read(&path).unwrap(), b"raw\0\xff bytes");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
