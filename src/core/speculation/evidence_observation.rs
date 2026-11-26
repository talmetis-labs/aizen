//! Explicit one-shot append of a nonterminal historical observation, not a read-only API.
//! This cannot create a cohort, publish a source image, settle an attempt or grant replay authority.
use super::*;
use crate::core::persist;
use crate::core::workspace_txn::{PhysicalEffectLease, WorkspaceWriterLease};

/// One-shot metadata-only publication under a fresh existing exclusive owner. No source capability, live controller reservation or execution authority can be recovered through this type.
#[derive(Clone, Copy)]
struct ObservationLimits {
    bytes: usize,
    records: usize,
    objects: usize,
    expansion: usize,
}
impl Default for ObservationLimits {
    fn default() -> Self {
        Self {
            bytes: MAX_STORE_BYTES,
            records: MAX_RECORDS,
            objects: MAX_OBJECTS,
            expansion: MAX_STORE_BYTES,
        }
    }
}

pub(crate) struct ExistingObservationAppend {
    storage: EvidenceDirectory,
    disk: Inventory,
    revision: [u8; 32],
    consumed: bool,
    poisoned: bool,
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

impl ExistingObservationAppend {
    pub(crate) fn acquire(
        directory: &Path,
        revision: [u8; 32],
        writer: &WorkspaceWriterLease,
        physical: &PhysicalEffectLease,
    ) -> Result<Self> {
        ensure!(physical.is_exclusive() && writer.identity().canonical_root.is_absolute()
            && !writer.identity().repo_id.is_empty() && !writer.identity().worktree_id.is_empty(),
            "historical observation requires exact writer identity and exclusive physical admission");
        let current = crate::core::exec_ctx::current()
            .context("inspection publication requires a plain parent")?;
        ensure!(
            current.can_restore_workspace() && current.native_scope().is_none(),
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
        let (disk, actual) = EvidenceDirectory::load_existing_inventory(directory)?;
        ensure!(
            actual == revision,
            "inspection publication selection is stale"
        );
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("inspection cohort ID missing")?;
        let value = Self {
            storage: EvidenceDirectory {
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
            poisoned: false,
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
        ensure!(
            !self.poisoned,
            "historical observation publication is uncertain"
        );
        self.storage.verify(&self.disk)
    }

    /// Only this fixed typed nonterminal record may be appended. Failure retains all evidence and
    /// consumes the holder; it never retries, cleans orphan objects or reports a clean no-effect.
    pub(crate) fn append(
        &mut self,
        observation: &super::super::recovery::InspectionObservation,
    ) -> Result<ObservationPublication> {
        self.append_bounded(observation, MAX_STORE_BYTES)
    }

    // Private budget seam for small owned boundary fixtures; production always uses 64 MiB.
    fn append_bounded(
        &mut self,
        observation: &super::super::recovery::InspectionObservation,
        expansion_limit: usize,
    ) -> Result<ObservationPublication> {
        self.append_with_limits(
            observation,
            ObservationLimits {
                expansion: expansion_limit,
                ..ObservationLimits::default()
            },
        )
    }

    fn append_with_limits(
        &mut self,
        observation: &super::super::recovery::InspectionObservation,
        limits: ObservationLimits,
    ) -> Result<ObservationPublication> {
        ensure!(
            limits.bytes <= MAX_STORE_BYTES
                && limits.records <= MAX_RECORDS
                && limits.objects <= MAX_OBJECTS
                && limits.expansion <= MAX_STORE_BYTES,
            "historical observation budget cannot exceed hard bounds"
        );
        ensure!(
            !self.consumed && !self.poisoned,
            "inspection publication holder already consumed"
        );
        self.consumed = true;
        let result = self.append_inner(observation, limits);
        if result.is_err() {
            self.poisoned = true;
        }
        result.context("inspection observation publication failed; persisted evidence may remain; no retry or cleanup ran")
    }

    fn append_inner(
        &mut self,
        observation: &super::super::recovery::InspectionObservation,
        limits: ObservationLimits,
    ) -> Result<ObservationPublication> {
        observation.validate()?;
        let current =
            crate::core::exec_ctx::current().context("inspection publication lost parent")?;
        ensure!(
            current.can_restore_workspace()
                && current.native_scope().is_none()
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
        let envelope = observation_envelope(observation, &metadata)?;
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
            limits.expansion,
        )?;
        // Check the COMPLETE pair before writing even the metadata blob. Existing orphans count.
        self.reserve_pair(
            &[(&blob_name, metadata.len()), (&manifest, envelope.len())],
            new_record,
            limits,
        )?;
        self.verify_owner()?;
        ensure!(
            inventory_revision(&self.storage.directory)? == self.revision,
            "inspection publication inventory drift"
        );
        self.publish_metadata_object(&blob_name, &metadata)?;
        fault(Fault::BeforeObservationManifest)?;
        self.publish_metadata_object(&manifest, &envelope)?;
        fault(Fault::AfterObservationManifest)?;
        self.disk.records += new_record;
        self.verify_owner()?;
        let (actual, after) = EvidenceDirectory::load_existing_inventory(&self.storage.directory)?;
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

impl ExistingObservationAppend {
    fn reserve_pair(
        &self,
        names: &[(&str, usize)],
        records: usize,
        limits: ObservationLimits,
    ) -> Result<()> {
        ensure!(
            !self.poisoned,
            "historical observation publication is uncertain"
        );
        let mut bytes = self
            .disk
            .bytes
            .checked_add(OWNER_MARKER.len())
            .context("historical observation quota overflow")?;
        let mut added = BTreeSet::new();
        for (name, len) in names {
            if !self.disk.objects.contains_key(*name) && added.insert(*name) {
                bytes = bytes
                    .checked_add(*len)
                    .context("historical observation quota overflow")?;
            }
        }
        ensure!(
            bytes <= limits.bytes,
            "historical observation exceeds 64 MiB inventory bound"
        );
        ensure!(
            self.disk
                .records
                .checked_add(records)
                .is_some_and(|count| count <= limits.records),
            "historical observation record limit reached"
        );
        ensure!(
            self.disk
                .objects
                .len()
                .checked_add(added.len())
                .is_some_and(|count| count <= limits.objects),
            "historical observation object limit reached"
        );
        Ok(())
    }

    // This private helper has exactly two callers in append_inner: the typed observation's JSON
    // blob and its fixed-label envelope. No caller can publish source bytes or arbitrary records.
    fn publish_metadata_object(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.verify_owner()?;
        ensure!(
            bytes.len() <= MAX_CONTROLLER_RECORD_META,
            "historical observation object exceeds metadata bound"
        );
        let image = Image::bytes(bytes);
        ensure!(
            name == manifest_name(bytes) || image.blob_name().as_deref() == Some(name),
            "historical observation immutable name differs"
        );
        if self.disk.objects.contains_key(name) {
            ensure!(
                self.storage.read_object(&self.disk, name)? == bytes,
                "historical observation content-address collision/corruption"
            );
            return Ok(());
        }
        let path = self.storage.directory.join(name);
        ensure!(
            matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "historical observation publication target exists"
        );
        fault(Fault::BeforePublish)?;
        persist::atomic_write_owner_only(&path, bytes)?;
        fault(Fault::AfterPublish)?;
        sync_directory(&self.storage.directory)?;
        self.storage.directory_guard.verify()?;
        let resource = NativeResource::resolve(&path)?;
        resource.verify()?;
        self.disk.bytes = self
            .disk
            .bytes
            .checked_add(bytes.len())
            .context("historical observation quota overflow")?;
        self.disk.objects.insert(name.to_owned(), image);
        self.storage.read_object(&self.disk, name)?;
        self.verify_owner()
    }
}

fn observation_envelope(
    observation: &super::super::recovery::InspectionObservation,
    metadata: &[u8],
) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct ObservationEnvelope<'a> {
        format: &'static str,
        kind: &'static str,
        base: &'a str,
        owner: &'a str,
        label: &'static str,
        metadata: Image,
        images: Vec<Image>,
    }
    ensure!(
        metadata.len() <= MAX_CONTROLLER_RECORD_META,
        "historical observation metadata exceeds bound"
    );
    encode_meta(&ObservationEnvelope {
        format: FORMAT,
        kind: "controller-record",
        base: &observation.base,
        owner: &observation.observer,
        label: super::super::recovery::OBSERVATION_LABEL,
        metadata: Image::bytes(metadata),
        images: Vec::new(),
    })
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path; // persist uses MOVEFILE_WRITE_THROUGH for immutable publications on Windows.
    Ok(())
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
#[path = "evidence_observation_tests.rs"]
mod tests;
