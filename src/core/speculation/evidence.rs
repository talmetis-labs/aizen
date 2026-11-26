//! Bounded existing historical evidence. No capture, overlay, source write or replay authority.
//!
//! The reader acquires only an existing shared owner lock. Recorded paths are lexical data;
//! callers must independently admit exact resources before inspecting live source. Unsupported
//! historical lineage remains diagnostic and unresolved, never reconstructed by a custom merger.

use crate::core::file_placement::{read_existing_regular, NodeStamp};
use crate::core::persist::FileFingerprint;
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::NativeResource;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "evidence_observation.rs"]
mod observation;
pub(crate) use observation::{ExistingObservationAppend, ObservationPublication};

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
const MAX_CONTROLLER_IMAGES: usize = MAX_COHORT_FILES * 2;
const FORMAT: &str = "aizen-speculation-v1";
const NAMESPACE: &str = "speculation-v1";
const OWNER_MARKER: &[u8] = b"aizen-speculation-owner-v1\n";

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
    fn blob_name(&self) -> Option<String> {
        self.exists.then(|| format!("b-{}", hex(&self.sha256)))
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Version {
    generation: u64,
    image: Image,
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

/// Inspection is untrusted data only; no method on this value confers file or integration rights.
#[derive(Debug)]
pub(crate) struct InspectedRecord {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) label: String,
    pub(crate) metadata: Vec<u8>,
    pub(crate) images: Vec<Vec<u8>>,
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

/// Read-only binding to an existing immutable-object directory and its actual owner lock.
struct EvidenceDirectory {
    id: String,
    directory: PathBuf,
    root_guard: NativeResource,
    directory_guard: NativeResource,
    lock_guard: NativeResource,
    lock_stamp: NodeStamp,
    _lock: RepoTxnLock,
}

#[derive(Default)]
struct Inventory {
    bytes: usize,
    records: usize,
    objects: BTreeMap<String, Image>,
}

impl EvidenceDirectory {
    /// Load bounded untrusted evidence only: no owner acquisition, writes, or handle reconstruction.
    /// The marker uses canonical bytes after a length guard so this also works while its Windows
    /// descriptor is exclusively locked. The owner must check its ACTUAL bytes separately.
    fn load_existing_inventory(directory: &Path) -> Result<(Inventory, [u8; 32])> {
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
        let mut disk = Inventory::default();
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

    fn verify(&self, disk: &Inventory) -> Result<()> {
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
            // Retain the admitted parent identity instead of establishing it anew per object.
            // Sibling resolution still rechecks every ancestor and the leaf on every visit.
            self.directory_guard.resolve_sibling(&entry.file_name())?;
            names.insert(name);
        }
        ensure!(
            names.len() == disk.objects.len() + 1,
            "speculation managed object disappeared"
        );
        self.directory_guard.verify()
    }

    fn read_object(&self, disk: &Inventory, name: &str) -> Result<Vec<u8>> {
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
}

/// Read-only revision of every immutable object, including orphan evidence. This takes no owner
/// lock and represents owner.lock by canonical bytes after a checked length guard, so it also works
/// under exclusive Windows ownership. Callers must separately check the ACTUAL marker through their
/// held owner descriptor. A matching revision is a stale-selection check, never execution authority.
pub(crate) fn inventory_revision(directory: &Path) -> Result<[u8; 32]> {
    EvidenceDirectory::load_existing_inventory(directory).map(|(_, revision)| revision)
}

/// A compatibility limitation is not corruption and never grants settlement or retry rights.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum HistoricalLimitation {
    MissingWorkerRoster,
    UnclosedOrUnsuccessfulWorkers,
    MissingSealedLineage { record: String },
    MultipleAuthors { record: String, key: String },
}

#[derive(Debug)]
pub(crate) struct EvidenceInspection {
    pub(crate) records: Vec<InspectedRecord>,
    pub(crate) limitations: Vec<HistoricalLimitation>,
}

impl EvidenceInspection {
    pub(crate) fn require_supported(self) -> Result<Vec<InspectedRecord>> {
        ensure!(self.limitations.is_empty(),
            "legacy evidence unresolved: {:?}; source and evidence retained; no merge replay or authority upgrade", self.limitations);
        Ok(self.records)
    }
}

pub(crate) struct ExistingEvidenceReader {
    directory: EvidenceDirectory,
    inventory: Inventory,
    revision: [u8; 32],
}

impl ExistingEvidenceReader {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let guard = NativeResource::resolve(&directory.join(".store-boundary"))?;
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("historical cohort ID missing")?;
        ensure!(
            id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "historical cohort ID invalid"
        );
        let lock_guard = NativeResource::resolve(&directory.join("owner.lock"))?;
        let lock_stamp = NodeStamp::capture(
            lock_guard.path(),
            &fs::symlink_metadata(lock_guard.path())?,
            false,
        )?;
        let lock = RepoTxnLock::acquire_existing(
            lock_guard.path(),
            crate::core::repo_lock::LockMode::Shared,
            Duration::ZERO,
        )?;
        let directory = EvidenceDirectory {
            id: id.to_owned(),
            directory: directory.to_path_buf(),
            root_guard: guard.clone(),
            directory_guard: guard,
            lock_guard,
            lock_stamp,
            _lock: lock,
        };
        Self::verify_owner(&directory)?;
        let (inventory, revision) =
            EvidenceDirectory::load_existing_inventory(&directory.directory)?;
        directory.verify(&inventory)?;
        Self::verify_owner(&directory)?;
        Ok(Self {
            directory,
            inventory,
            revision,
        })
    }

    fn verify_owner(directory: &EvidenceDirectory) -> Result<()> {
        directory
            ._lock
            .verify_existing_path(directory.lock_guard.path())?;
        ensure!(
            read_existing_regular(directory.lock_guard.path(), OWNER_MARKER.len() as u64)?
                == OWNER_MARKER,
            "unsupported historical owner marker"
        );
        directory
            ._lock
            .verify_existing_path(directory.lock_guard.path())
    }

    pub(crate) fn inspect(&self) -> Result<EvidenceInspection> {
        let mut work = InspectionWork::default();
        self.inspect_budgeted(&mut work)
    }

    fn inspect_budgeted(&self, work: &mut InspectionWork) -> Result<EvidenceInspection> {
        Self::verify_owner(&self.directory)?;
        self.directory.verify(&self.inventory)?;
        ensure!(
            inventory_revision(&self.directory.directory)? == self.revision,
            "historical evidence inventory changed before inspection"
        );
        let (records, _) =
            inspect_records_budgeted(&self.directory, &self.inventory, &self.directory.id, work)?;
        super::verification::validate_verification_records(&self.directory.id, &records)?;
        let limitations = historical_limitations(&self.directory.id, &records)?;
        Self::verify_owner(&self.directory)?;
        self.directory.verify(&self.inventory)?;
        ensure!(
            inventory_revision(&self.directory.directory)? == self.revision,
            "historical evidence inventory changed during inspection"
        );
        Ok(EvidenceInspection {
            records,
            limitations,
        })
    }
}

/// Conservative compatibility entry for callers that require fully linked historical evidence.
/// The reader itself exposes limitations as data; this helper never silently discards them.
pub(crate) fn inspect_controller_records(directory: &Path) -> Result<Vec<InspectedRecord>> {
    ExistingEvidenceReader::open(directory)?
        .inspect()?
        .require_supported()
}

fn historical_limitations(
    base: &str,
    records: &[InspectedRecord],
) -> Result<Vec<HistoricalLimitation>> {
    let workers = super::workers::validate_worker_records(base, records)?;
    let mut result = Vec::new();
    if workers.roster.is_none() {
        result.push(HistoricalLimitation::MissingWorkerRoster);
    }
    if workers.unresolved() {
        result.push(HistoricalLimitation::UnclosedOrUnsuccessfulWorkers);
    }
    for record in records
        .iter()
        .filter(|record| record.label == "integration-prepared")
    {
        let value: serde_json::Value = serde_json::from_slice(&record.metadata)?;
        if sealed_lineage(&value)?.is_none() {
            result.push(HistoricalLimitation::MissingSealedLineage {
                record: record.id.clone(),
            });
            continue;
        }
        let files = value
            .get("files")
            .and_then(serde_json::Value::as_array)
            .context("historical prepared files missing")?;
        ensure!(
            files.len() <= MAX_COHORT_FILES,
            "historical prepared file count exceeds bound"
        );
        for file in files {
            let lineage = file
                .get("lineage")
                .and_then(serde_json::Value::as_array)
                .context("historical prepared file lineage missing")?;
            ensure!(
                !lineage.is_empty() && lineage.len() <= MAX_OVERLAYS,
                "historical prepared author count invalid"
            );
            if lineage.len() > 1 {
                let key = file
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .context("historical prepared key missing")?;
                result.push(HistoricalLimitation::MultipleAuthors {
                    record: record.id.clone(),
                    key: key.to_owned(),
                });
            }
        }
    }
    Ok(result)
}

/// Used by settlement classification of already decoded records. No shape-only historical upgrade.
pub(crate) fn require_supported_history(base: &str, records: &[InspectedRecord]) -> Result<()> {
    let limitations = historical_limitations(base, records)?;
    ensure!(
        limitations.is_empty(),
        "legacy evidence unresolved: {limitations:?}; evidence retained"
    );
    Ok(())
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
    read_limit: u64,
    comparison_limit: u64,
    object_reads: usize,
    histories: usize,
}

impl Default for InspectionWork {
    fn default() -> Self {
        Self {
            read_bytes: 0,
            comparison_bytes: 0,
            read_limit: 256 * 1024 * 1024,
            comparison_limit: 256 * 1024 * 1024,
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

    fn read(
        &mut self,
        storage: &EvidenceDirectory,
        disk: &Inventory,
        name: &str,
    ) -> Result<Vec<u8>> {
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
    pub(crate) object_reads: usize,
    pub(crate) histories: usize,
}

#[cfg(test)]
pub(crate) fn inspect_with_work_budget(
    directory: &Path,
    read_limit: u64,
    comparison_limit: u64,
) -> (Result<EvidenceInspection>, InspectionWorkStats) {
    let mut work = InspectionWork {
        read_limit,
        comparison_limit,
        ..InspectionWork::default()
    };
    let result = ExistingEvidenceReader::open(directory)
        .and_then(|reader| reader.inspect_budgeted(&mut work));
    (
        result,
        InspectionWorkStats {
            read_bytes: work.read_bytes,
            comparison_bytes: work.comparison_bytes,
            object_reads: work.object_reads,
            histories: work.histories,
        },
    )
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
    rewrite: bool,
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
                rewrite: manifest.edits.iter().any(|edit| {
                    edit.key == binding.key && edit.provenance == WriteProvenance::Rewrite
                }),
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
    _storage: &EvidenceDirectory,
    _disk: &Inventory,
    metadata: &serde_json::Value,
    mut changes: BTreeMap<String, Vec<SealedChangeEvidence>>,
    refs: &[Image],
    work: &mut InspectionWork,
) -> Result<()> {
    let files = metadata
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("historical prepared files missing")?;
    ensure!(
        files.len() == changes.len(),
        "historical candidate membership differs from sealed changes"
    );
    let mut next_image = 0;
    for file in files {
        let key = file
            .get("key")
            .and_then(serde_json::Value::as_str)
            .context("historical candidate key missing")?;
        let selected = changes
            .remove(key)
            .context("historical candidate key duplicated or not authored")?;
        let first = selected
            .first()
            .context("historical candidate has no sealed authors")?;
        let mut contributors = Vec::new();
        for change in &selected {
            ensure!(
                change.before == first.before && change.path == first.path,
                "historical candidate base differs between authors"
            );
            // Preserve old engine input ceilings as format compatibility checks, not merge work.
            ensure!(
                change.before.byte_len <= 1024 * 1024 && change.after.byte_len <= 1024 * 1024,
                "historical candidate exceeds supported image bound"
            );
            contributors.push(change.author.clone());
        }
        contributors.sort();
        ensure!(
            contributors.windows(2).all(|pair| pair[0] != pair[1]),
            "historical candidate author duplicated"
        );
        // A single author has one exact final sealed image. Multiple authors are inspected only:
        // their declared candidate stays unproven and is surfaced as an unresolved limitation.
        let after = if selected.len() == 1 {
            first.after.clone()
        } else {
            parsed_image(
                file.get("after")
                    .context("historical declared candidate missing")?,
            )?
        };
        work.compare(first.before.byte_len.saturating_add(after.byte_len))?;
        validate_prepared_mapping(
            file.get("preimage")
                .context("historical preimage mapping missing")?,
            &first.before,
            refs,
            &mut next_image,
        )?;
        validate_prepared_mapping(
            file.get("postimage")
                .context("historical postimage mapping missing")?,
            &after,
            refs,
            &mut next_image,
        )?;
        let rewrite = selected.iter().any(|change| change.rewrite);
        ensure!(
            file.get("after") == Some(&serde_json::to_value(&after)?)
                && file.get("contributors") == Some(&serde_json::to_value(&contributors)?)
                && file.get("operation").and_then(serde_json::Value::as_str)
                    == Some(if rewrite { "rewrite" } else { "localized" }),
            "historical candidate differs from exact sealed image/author mapping"
        );
    }
    ensure!(
        next_image == refs.len(),
        "historical prepared has unreferenced images"
    );
    Ok(())
}

fn validate_manifest_images(
    storage: &EvidenceDirectory,
    disk: &Inventory,
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
    storage: &EvidenceDirectory,
    disk: &Inventory,
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

struct SealedInspectionCache {
    manifests: BTreeMap<String, SealedOverlayManifest>,
    encoded_bytes: usize,
}

impl SealedInspectionCache {
    fn get<'a>(
        &'a mut self,
        storage: &EvidenceDirectory,
        disk: &Inventory,
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
    storage: &EvidenceDirectory,
    disk: &Inventory,
    base: &str,
) -> Result<(Vec<InspectedRecord>, usize)> {
    inspect_records_budgeted(storage, disk, base, &mut InspectionWork::default())
}

fn inspect_records_budgeted(
    storage: &EvidenceDirectory,
    disk: &Inventory,
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
        let metadata: serde_json::Value = serde_json::from_slice(&record.metadata)?;
        if workers.roster.is_some() && !workers.unresolved() && sealed_lineage(&metadata)?.is_some()
        {
            workers.check_selected_lineage(&metadata)?;
        }
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

#[cfg(test)]
#[path = "historical_evidence_fixtures.rs"]
pub(crate) mod fixtures;
#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "historical_evidence_reader_tests.rs"]
mod reader_regressions;
