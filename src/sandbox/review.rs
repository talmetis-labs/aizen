//! Text artifact handoff. The document and its decision share one bounded, atomic journal.
use crate::core::{persist, repo_lock::RepoTxnLock};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const DOCUMENT_BYTES: usize = 64 * 1024;
const STORE_BYTES: usize = 2 * 1024 * 1024;
const SAMPLE: &str = "# Project handoff\n\n## Deliverable\nA reviewable Markdown document.\n\n## Acceptance\nOpen the exported file and compare it with this preview.\n";
const SAMPLE_REVISION: &str = "# Project handoff\n\n## Deliverable\nA revised Markdown document.\n\n## Acceptance\n1. Open the exported file.\n2. Compare its SHA-256 digest with the reviewed version.\n";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Read {
        version: Option<u32>,
    },
    Sample {
        expected: u32,
    },
    Submit {
        expected: u32,
        document: String,
        sha256: String,
    },
    Revise {
        version: u32,
        note: String,
    },
    Discard {
        version: u32,
    },
    Deliver {
        version: u32,
        directory: Option<PathBuf>,
    },
}
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    Revision,
    Delivering,
    Delivered,
    Discarded,
}
#[derive(Serialize, Deserialize)]
struct Version {
    number: u32,
    sample: bool,
    status: Status,
    document: String,
    sha256: String,
    note: String,
    destination: Option<PathBuf>,
}
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    versions: Vec<Version>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit as u64,
        "Expected a bounded regular file"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "File exceeds its size limit");
    Ok(bytes)
}
/// Publish a complete file without replacing an existing destination. The private staging
/// file stays on the same volume; a crash after linking can be reconciled from the journal.
fn publish_new(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut random = [0; 16];
    getrandom::getrandom(&mut random)
        .map_err(|_| anyhow::anyhow!("Cannot allocate export staging file"))?;
    let name = format!(".aizen-review-{}.tmp", digest(&random));
    let temp = path.with_file_name(name);
    let mut opts = fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp)?;
    let result: Result<()> = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&temp, path)
            .context("Choose a destination on a filesystem supporting hard links")?;
        persist::sync_parent_checked(path)?;
        Ok(())
    })();
    fs::remove_file(&temp)?;
    result
}

fn reserve_document_slot(root: &Path, journal: &Path, ledger: &Ledger) -> Result<()> {
    // Leave room for the destination receipt and revision note of admitted jobs.
    ensure!(
        serde_json::to_vec(ledger)?.len() + 16 * 1024 <= STORE_BYTES,
        "Conversation review quota reached. Discard older document versions."
    );
    let mut reserved = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let candidate = entry.path().join("documents.json");
            if candidate != journal && candidate.try_exists()? {
                reserved =
                    reserved.saturating_add(fs::symlink_metadata(candidate)?.len() + 16 * 1024);
            }
        }
    }
    ensure!(
        reserved + serde_json::to_vec(ledger)?.len() as u64 + 16 * 1024 <= 64 * 1024 * 1024,
        "Sandbox review quota reached. Discard older document versions."
    );
    Ok(())
}
fn save(path: &Path, ledger: &Ledger) -> Result<()> {
    let bytes = serde_json::to_vec(ledger)?;
    ensure!(
        bytes.len() <= STORE_BYTES,
        "Conversation review storage is full"
    );
    persist::atomic_write_owner_only_checked(path, &bytes)
}
fn checked_document(text: &str, hash: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= DOCUMENT_BYTES && !text.contains('\0'),
        "Expected a UTF-8 text document of at most 64 KiB"
    );
    ensure!(digest(text.as_bytes()) == hash, "Artifact SHA-256 mismatch");
    Ok(())
}

fn apply(home: &Path, workspace: &Path, topic: &str, request: Request) -> Result<Value> {
    ensure!(
        !topic.trim().is_empty() && topic.len() <= 256 && !topic.chars().any(char::is_control),
        "Invalid conversation identity"
    );
    let root = workspace.canonicalize()?;
    ensure!(root.is_dir(), "Workspace directory is required");
    let mut key = root.as_os_str().as_encoded_bytes().to_vec();
    key.push(0);
    key.extend_from_slice(topic.as_bytes());
    let scope = digest(&key);
    let directory = home.join("sandbox-reviews").join(&scope);
    let journal = directory.join("documents.json");
    if matches!(request, Request::Read { .. }) && !journal.try_exists()? {
        return Ok(json!({"latest":0,"history":[],"review":null}));
    }
    let catalog = home.join("sandbox-reviews");
    let _budget = if matches!(request, Request::Submit { .. } | Request::Sample { .. }) {
        Some(RepoTxnLock::acquire(
            &catalog.join("admission.lock"),
            Duration::from_secs(5),
        )?)
    } else {
        None
    };
    let _lock = RepoTxnLock::acquire(&directory.join("review.lock"), Duration::from_secs(5))?;
    let mut ledger: Ledger = if journal.try_exists()? {
        serde_json::from_slice(&read_bounded(&journal, STORE_BYTES)?)?
    } else {
        Ledger::default()
    };
    let latest = ledger.versions.last().map_or(0, |v| v.number);
    let selected = match request {
        Request::Read { version } => version.unwrap_or(latest),
        Request::Sample { expected } => {
            let revised = ledger
                .versions
                .last()
                .is_some_and(|v| v.status == Status::Revision && v.sample);
            let document = if revised { SAMPLE_REVISION } else { SAMPLE };
            append(
                &mut ledger,
                latest,
                expected,
                document.to_owned(),
                digest(document.as_bytes()),
                true,
            )?;
            reserve_document_slot(&catalog, &journal, &ledger)?;
            save(&journal, &ledger)?;
            latest + 1
        }
        Request::Submit {
            expected,
            document,
            sha256,
        } => {
            append(&mut ledger, latest, expected, document, sha256, false)?;
            reserve_document_slot(&catalog, &journal, &ledger)?;
            save(&journal, &ledger)?;
            latest + 1
        }
        decision => {
            let current = ledger
                .versions
                .last_mut()
                .context("No document to review")?;
            match decision {
                Request::Revise { version, note } => {
                    ensure!(
                        version == latest && current.status == Status::Pending,
                        "Refresh this review before requesting changes"
                    );
                    ensure!(
                        !note.trim().is_empty() && note.len() <= 2048,
                        "Provide a revision note of at most 2048 bytes"
                    );
                    current.note = note.trim().to_owned();
                    current.status = Status::Revision;
                }
                Request::Discard { version } => {
                    let current = ledger
                        .versions
                        .iter_mut()
                        .find(|v| v.number == version)
                        .context("Unknown document version")?;
                    ensure!(
                        current.status != Status::Delivering,
                        "Finish the export before discarding the stored document"
                    );
                    current.status = Status::Discarded;
                    current.document.clear();
                    current.note.clear();
                }
                Request::Deliver { version, directory } => {
                    ensure!(
                        version == latest
                            && matches!(
                                current.status,
                                Status::Pending | Status::Delivering | Status::Delivered
                            ),
                        "This version cannot be delivered"
                    );
                    checked_document(&current.document, &current.sha256)?;
                    if current.status == Status::Pending {
                        let parent = directory
                            .context("Choose a destination folder")?
                            .canonicalize()?;
                        ensure!(parent.is_dir(), "Destination is not a directory");
                        let dest = parent.join(format!("aizen-{}-v{version}.md", &scope[..12]));
                        ensure!(
                            !dest.try_exists()?,
                            "A file with that name already exists. Choose another folder."
                        );
                        current.destination = Some(dest);
                        current.status = Status::Delivering;
                        save(&journal, &ledger)?;
                    }
                    let current = ledger.versions.last_mut().context("Missing version")?;
                    let path = current
                        .destination
                        .as_deref()
                        .context("Missing delivery receipt")?;
                    if !path.try_exists()? && current.status == Status::Delivering {
                        publish_new(path, &current.document)?;
                    }
                    ensure!(
                        digest(&read_bounded(path, DOCUMENT_BYTES)?) == current.sha256,
                        "The destination differs from the reviewed version"
                    );
                    current.status = Status::Delivered;
                }
                _ => unreachable!(),
            }
            save(&journal, &ledger)?;
            latest
        }
    };
    let current = ledger.versions.iter().find(|v| v.number == selected);
    ensure!(
        selected == 0 || current.is_some(),
        "Unknown document version"
    );
    if let Some(v) = current.filter(|v| v.status != Status::Discarded) {
        checked_document(&v.document, &v.sha256)?;
    }
    Ok(
        json!({"latest":ledger.versions.last().map_or(0,|v|v.number),"history":ledger.versions.iter().map(|v|v.number).collect::<Vec<_>>(),"review":current}),
    )
}
fn append(
    ledger: &mut Ledger,
    latest: u32,
    expected: u32,
    document: String,
    sha256: String,
    sample: bool,
) -> Result<()> {
    ensure!(
        expected == latest,
        "The conversation already has a newer version"
    );
    if let Some(v) = ledger.versions.last() {
        ensure!(
            matches!(v.status, Status::Discarded | Status::Delivered)
                || (v.status == Status::Revision && v.sample == sample),
            "Finish the current review first"
        );
    }
    checked_document(&document, &sha256)?;
    // Bound tombstones in long-lived topics; retained artifacts always keep their version number.
    ledger
        .versions
        .retain(|v| v.status != Status::Discarded || v.number > latest.saturating_sub(32));
    ledger.versions.push(Version {
        number: latest.checked_add(1).context("Version limit reached")?,
        sample,
        status: Status::Pending,
        document,
        sha256,
        note: String::new(),
        destination: None,
    });
    Ok(())
}
pub(crate) fn run(topic: &str) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        bail!("Review request exceeds its limit")
    }
    let result = apply(
        &crate::core::config::aizen_home(),
        &std::env::current_dir()?,
        topic,
        serde_json::from_slice(&bytes)?,
    );
    match result {
        Ok(view) => println!("{}", serde_json::to_string(&view)?),
        Err(error) => {
            println!("{}", json!({"error":error.to_string()}));
            return Err(error);
        }
    }
    Ok(())
}
