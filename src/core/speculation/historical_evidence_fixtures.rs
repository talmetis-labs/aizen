//! Explicit historical bytes for evidence-reader tests, never live source/worker capabilities.
//!
//! These builders serialize supplied images and histories into disposable directories. They do not
//! capture a workspace, resolve writes, merge candidates, run bodies, or manufacture release tokens.

use super::*;
use crate::core::workspace_txn::{NativeResource, WorkspaceIdentity};
use serde_json::{json, Value};

pub(crate) const FIXTURE_BASE: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone)]
pub(crate) struct HistoricalFile {
    pub(crate) key: String,
    pub(crate) path: PathBuf,
    pub(crate) before: Option<Vec<u8>>,
    pub(crate) writable: bool,
}

impl HistoricalFile {
    /// Bind only the fixture's explicit path spelling; never read bytes from that source path.
    pub(crate) fn explicit(path: PathBuf, before: Option<&[u8]>, writable: bool) -> Self {
        let key = NativeResource::resolve(&path).unwrap().key().to_owned();
        Self {
            key,
            path,
            before: before.map(<[u8]>::to_vec),
            writable,
        }
    }
}

pub(crate) struct HistoricalEdit {
    pub(crate) key: String,
    pub(crate) after: Vec<u8>,
    pub(crate) provenance: WriteProvenance,
}

#[derive(Clone)]
pub(crate) struct HistoricalAuthor {
    pub(crate) identity: SealedManifestIdentity,
    pub(crate) bindings: Vec<(String, PathBuf, bool)>,
    pub(crate) observations: Vec<Value>,
    changes: Vec<HistoricalChange>,
    worker_images: Vec<(crate::core::worker_evidence::NativeWorkerKind, [u8; 32])>,
}

#[derive(Clone)]
struct HistoricalChange {
    key: String,
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    generation: u64,
    operation: &'static str,
}

fn descriptor(bytes: Option<&[u8]>) -> Image {
    bytes.map(Image::bytes).unwrap_or_else(Image::missing)
}

pub(crate) fn write_blob(directory: &Path, bytes: &[u8]) {
    let image = Image::bytes(bytes);
    fs::write(directory.join(image.blob_name().unwrap()), bytes).unwrap();
}

pub(crate) fn write_manifest(directory: &Path, bytes: &[u8]) -> String {
    let name = manifest_name(bytes);
    fs::write(directory.join(&name), bytes).unwrap();
    name
}

pub(crate) fn write_base(
    namespace: &Path,
    identity: &WorkspaceIdentity,
    files: &[HistoricalFile],
) -> PathBuf {
    let mut files = files.to_vec();
    files.sort_by(|a, b| a.key.cmp(&b.key));
    let directory = namespace.join(NAMESPACE).join(FIXTURE_BASE);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("owner.lock"), OWNER_MARKER).unwrap();
    for file in &files {
        if let Some(bytes) = &file.before {
            write_blob(&directory, bytes);
        }
    }
    let manifest = BaseManifestOwned {
        format: FORMAT.into(),
        kind: "base".into(),
        base: FIXTURE_BASE.into(),
        repo: identity.repo_id.clone(),
        worktree: identity.worktree_id.clone(),
        root: identity.canonical_root.clone(),
        bindings: files
            .iter()
            .map(|file| ManifestBindingOwned {
                key: file.key.clone(),
                path: file.path.clone(),
                base: descriptor(file.before.as_deref()),
                writable: false,
            })
            .collect(),
    };
    write_manifest(&directory, &encode_meta(&manifest).unwrap());
    directory
}

/// Decode only previously serialized test documents, not the recorded source paths.
pub(crate) fn base_files(directory: &Path) -> Vec<HistoricalFile> {
    let manifest = fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            if !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("m-")
            {
                return None;
            }
            let bytes = fs::read(path).unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            (value["kind"] == "base")
                .then(|| serde_json::from_slice::<BaseManifestOwned>(&bytes).unwrap())
        })
        .next()
        .unwrap();
    manifest
        .bindings
        .into_iter()
        .map(|binding| HistoricalFile {
            key: binding.key,
            path: binding.path,
            before: binding
                .base
                .blob_name()
                .map(|name| fs::read(directory.join(name)).unwrap()),
            writable: true,
        })
        .collect()
}

/// Every edit and byte image is supplied as test data. This is deliberately not an overlay API:
/// there is no CAS/snapshot handle, body lease, sealing capability, or source mutation operation.
pub(crate) fn write_sealed_history(
    directory: &Path,
    ordinal: usize,
    owner: &str,
    files: &[HistoricalFile],
    edits: &[HistoricalEdit],
) -> HistoricalAuthor {
    let mut files = files.to_vec();
    files.sort_by(|a, b| a.key.cmp(&b.key));
    let mut versions: BTreeMap<_, _> = files
        .iter()
        .map(|file| {
            (
                file.key.clone(),
                Version {
                    generation: 0,
                    image: descriptor(file.before.as_deref()),
                },
            )
        })
        .collect();
    let mut observations = Vec::new();
    let mut history = Vec::new();
    let mut final_bytes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (index, edit) in edits.iter().enumerate() {
        let before = versions[&edit.key].clone();
        if !observations
            .iter()
            .any(|seen: &Observation| seen.key == edit.key && seen.generation == before.generation)
        {
            observations.push(Observation {
                key: edit.key.clone(),
                generation: before.generation,
                image: before.image.clone(),
            });
        }
        let after = Image::bytes(&edit.after);
        write_blob(directory, &edit.after);
        let generation = index as u64 + 1;
        history.push(Edit {
            key: edit.key.clone(),
            generation,
            before: before.image,
            after: after.clone(),
            provenance: edit.provenance,
        });
        versions.insert(
            edit.key.clone(),
            Version {
                generation,
                image: after,
            },
        );
        final_bytes.insert(edit.key.clone(), edit.after.clone());
    }
    let overlay = format!("overlay-{ordinal}");
    let manifest = SealedOverlayManifest {
        format: FORMAT.into(),
        kind: "overlay".into(),
        base: FIXTURE_BASE.into(),
        overlay: overlay.clone(),
        owner: owner.into(),
        generation: edits.len() as u64,
        sealed: true,
        bindings: files
            .iter()
            .map(|file| ManifestBindingOwned {
                key: file.key.clone(),
                path: file.path.clone(),
                base: descriptor(file.before.as_deref()),
                writable: file.writable,
            })
            .collect(),
        files: versions,
        observations,
        edits: history,
    };
    let name = write_manifest(directory, &encode_meta(&manifest).unwrap());
    let author = format!("{owner}/{overlay}");
    let observations = manifest
        .observations
        .iter()
        .map(|observation| {
            let file = files
                .iter()
                .find(|file| file.key == observation.key)
                .unwrap();
            json!({"author":author,"key":observation.key,"path":file.path,
            "generation":observation.generation,"fingerprint":observation.image})
        })
        .collect();
    let changes = files
        .iter()
        .filter_map(|file| {
            let after = final_bytes.get(&file.key)?;
            if file.before.as_deref() == Some(after.as_slice()) {
                return None;
            }
            Some(HistoricalChange {
                key: file.key.clone(),
                path: file.path.clone(),
                before: file.before.clone(),
                after: after.clone(),
                generation: manifest.files[&file.key].generation,
                operation: if edits
                    .iter()
                    .any(|edit| edit.key == file.key && edit.provenance == WriteProvenance::Rewrite)
                {
                    "rewrite"
                } else {
                    "localized"
                },
            })
        })
        .collect();
    HistoricalAuthor {
        identity: SealedManifestIdentity {
            base: FIXTURE_BASE.into(),
            overlay,
            owner: owner.into(),
            generation: edits.len() as u64,
            manifest: name,
        },
        bindings: files
            .iter()
            .map(|file| (file.key.clone(), file.path.clone(), file.writable))
            .collect(),
        observations,
        changes,
        worker_images: edits
            .iter()
            .map(|edit| {
                (
                    if edit.provenance == WriteProvenance::Rewrite {
                        crate::core::worker_evidence::NativeWorkerKind::Write
                    } else {
                        crate::core::worker_evidence::NativeWorkerKind::Edit
                    },
                    Image::bytes(&edit.after).sha256,
                )
            })
            .collect(),
    }
}

/// The caller explicitly supplies a historical candidate for every key. No candidate construction
/// or merge algorithm is hidden here, including for deliberately unsupported multi-author input.
pub(crate) fn prepared_document(
    authors: &[HistoricalAuthor],
    candidates: &BTreeMap<String, Vec<u8>>,
) -> (Value, Vec<Vec<u8>>) {
    let mut changes: BTreeMap<&str, Vec<(&HistoricalAuthor, &HistoricalChange)>> = BTreeMap::new();
    for author in authors {
        for change in &author.changes {
            changes
                .entry(&change.key)
                .or_default()
                .push((author, change));
        }
    }
    assert_eq!(candidates.len(), changes.len());
    let mut images = Vec::new();
    let mut files = Vec::new();
    for (key, selected) in &changes {
        let first = selected[0].1;
        let before = descriptor(first.before.as_deref());
        let mut preimage = serde_json::to_value(&before).unwrap();
        preimage["missing"] = json!(first.before.is_none());
        preimage["index"] = if let Some(bytes) = &first.before {
            let index = images.len();
            images.push(bytes.clone());
            json!(index)
        } else {
            Value::Null
        };
        let candidate = &candidates[*key];
        if selected.len() == 1 {
            assert_eq!(
                candidate, &first.after,
                "single-author baseline candidate must be explicit exact historical bytes"
            );
        }
        let after = Image::bytes(candidate);
        let mut postimage = serde_json::to_value(&after).unwrap();
        postimage["missing"] = json!(false);
        postimage["index"] = json!(images.len());
        images.push(candidate.clone());
        let mut contributors: Vec<_> = selected
            .iter()
            .map(|(author, _)| format!("{}/{}", author.identity.owner, author.identity.overlay))
            .collect();
        contributors.sort();
        contributors.dedup();
        let lineage: Vec<_> = selected
            .iter()
            .map(|(author, change)| {
                json!({
                    "author":format!("{}/{}",author.identity.owner,author.identity.overlay),
                    "generation":change.generation,"operation":change.operation
                })
            })
            .collect();
        files.push(json!({"key":key,"path":first.path,"expected":before,"after":after,
            "preimage":preimage,"postimage":postimage,"contributors":contributors,
            "operation":if selected.iter().any(|(_,change)| change.operation=="rewrite") {"rewrite"} else {"localized"},"lineage":lineage}));
    }
    let observations: Vec<_> = authors
        .iter()
        .flat_map(|author| author.observations.clone())
        .collect();
    let mut concerns: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for observation in &observations {
        let observer = observation["author"].as_str().unwrap();
        let key = observation["key"].as_str().unwrap();
        if let Some(selected) = changes.get(key) {
            for (author, _) in selected {
                let peer = format!("{}/{}", author.identity.owner, author.identity.overlay);
                if peer != observer {
                    concerns
                        .entry((observer.into(), key.into()))
                        .or_default()
                        .insert(peer);
                }
            }
        }
    }
    let concerns: Vec<_> = concerns
        .into_iter()
        .map(|((observer, key), peers)| json!({"observer":observer,"key":key,"peer_authors":peers}))
        .collect();
    (
        json!({"schema":1,"overlay_lineage_version":1,"base":FIXTURE_BASE,"owner":"parent","state":"prepared",
        "overlays":authors.iter().map(|author| json!({"base":FIXTURE_BASE,"owner":author.identity.owner,
            "id":author.identity.overlay,"author":format!("{}/{}",author.identity.owner,author.identity.overlay),
            "generation":author.identity.generation,"manifest":author.identity.manifest})).collect::<Vec<_>>(),
        "observations":observations,"read_dependencies":concerns,"files":files}),
        images,
    )
}

pub(crate) fn write_record<T: serde::Serialize>(
    directory: &Path,
    owner: &str,
    label: &str,
    value: &T,
    images: &[Vec<u8>],
) -> String {
    let metadata = serde_json::to_vec(value).unwrap();
    write_blob(directory, &metadata);
    for image in images {
        write_blob(directory, image);
    }
    let envelope = json!({"format":FORMAT,"kind":"controller-record","base":FIXTURE_BASE,
        "owner":owner,"label":label,"metadata":Image::bytes(&metadata),
        "images":images.iter().map(|image| Image::bytes(image)).collect::<Vec<_>>()});
    write_manifest(directory, &serde_json::to_vec(&envelope).unwrap())
}

/// Fully linked historical worker records. These bytes are test inputs to the legacy validator,
/// never runtime facts and never a constructor for ReleasedGitAttempt or NativeClosure.
pub(crate) fn write_worker_history(directory: &Path, authors: &[HistoricalAuthor]) {
    use crate::core::speculation::verification::DurableEffectState;
    use crate::core::speculation::workers::*;
    let roster = WorkerRoster {
        schema: 1,
        lifecycle_version: 1,
        base: FIXTURE_BASE.into(),
        owner: "parent".into(),
        incarnation: "fedcba9876543210fedcba9876543210".into(),
        members: authors
            .iter()
            .enumerate()
            .map(|(index, author)| AttemptMember {
                attempt: index as u32 + 1,
                owner: author.identity.owner.clone(),
                overlay: author.identity.overlay.clone(),
                scope_sha256: scope_sha256(
                    author
                        .bindings
                        .iter()
                        .map(|(key, path, writable)| (key.as_str(), path.as_path(), *writable)),
                )
                .unwrap(),
            })
            .collect(),
    };
    // Serialize the typed historical DTO directly: Value would reorder canonical fields.
    let roster_id = write_record(directory, "parent", ROSTER_LABEL, &roster, &[]);
    for (index, author) in authors.iter().enumerate() {
        let mut intents = Vec::new();
        let mut receipts = Vec::new();
        // One historical body per explicitly supplied edit, including intermediate generations.
        // Empty authored histories have zero bodies; neither case creates a runtime proof.
        for (worker, (kind, digest)) in author.worker_images.iter().enumerate() {
            let intent = WorkerIntent {
                schema: 1,
                lifecycle_version: 1,
                base: FIXTURE_BASE.into(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                attempt: index as u32 + 1,
                worker: worker as u32 + 1,
                parent_worker: None,
                kind: *kind,
                prepared_sha256: *digest,
            };
            let intent_id = write_record(directory, "parent", INTENT_LABEL, &intent, &[]);
            let closed = WorkerClosed {
                schema: 1,
                lifecycle_version: 1,
                base: FIXTURE_BASE.into(),
                owner: "parent".into(),
                roster: roster_id.clone(),
                intent: intent_id.clone(),
                result: WorkerResult::Returned,
                effect_state: DurableEffectState::Settled,
                ownership_released: true,
                children: Vec::new(),
            };
            let closed_id = write_record(directory, "parent", CLOSED_LABEL, &closed, &[]);
            intents.push(intent_id.clone());
            receipts.push(WorkerReceipt {
                intent: intent_id,
                closed: closed_id,
            });
        }
        let admission = AttemptAdmissionClosed {
            schema: 1,
            lifecycle_version: 1,
            base: FIXTURE_BASE.into(),
            owner: "parent".into(),
            roster: roster_id.clone(),
            attempt: index as u32 + 1,
            driver_result: DriverResult::Completed,
            intents,
        };
        let admission_id = write_record(directory, "parent", ADMISSION_LABEL, &admission, &[]);
        let closed = AttemptClosed {
            schema: 1,
            lifecycle_version: 1,
            base: FIXTURE_BASE.into(),
            owner: "parent".into(),
            roster: roster_id.clone(),
            admission: admission_id,
            ownership_released: true,
            workers: receipts,
            sealed: author.identity.clone(),
        };
        write_record(directory, "parent", ATTEMPT_LABEL, &closed, &[]);
    }
}
