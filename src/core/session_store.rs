//! Where a conversation LIVES: the on-disk session store under `~/.aizen/sessions/`.
//!
//! One concern only — turning a `Vec<Message>` into a durable, named, project-tagged file and back
//! again, plus the live snapshot that survives an abrupt terminal close. It knows nothing about how
//! sessions are picked or displayed; `/sessions` and the restore menus are presentation and stay in
//! the REPL layer. Anything that writes to `sessions/` should go through here so the naming,
//! provenance and crash-flush rules cannot fork.

use crate::agent::design::TaskDesignState;
use crate::core::tool_media::{ImageUnavailable, ToolImage, MAX_IMAGE_BYTES};
use crate::core::types::{Message, UsageRow};
use crate::core::{cli_config, config};
use crate::memory;
use crate::ui::{theme, tui};
use crate::{fmt_time_ago, refresh_prompt_lanes_for_thread_switch};
use anyhow::{Context, Result};
use console::style;
use serde_json::Value;
use std::io::Read as _;
use std::sync::{Mutex, OnceLock};

pub(crate) fn sessions_dir() -> std::path::PathBuf {
    config::aizen_home().join("sessions")
}
pub(crate) fn sanitize_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let n = n.trim_matches(['.', ' ']);
    // Windows device names resolve specially even with an extension (`CON.json`, `NUL.json`, …).
    // Prefix them so a saved/restored session can never target a device path.
    let upper = n.to_ascii_uppercase();
    let numbered_device = |prefix: &str| {
        upper
            .strip_prefix(prefix)
            .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && d != "0")
    };
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered_device("COM")
        || numbered_device("LPT");
    if n.is_empty() {
        "session".to_string()
    } else if reserved {
        format!("session_{n}")
    } else {
        n.to_string()
    }
}
/// Why a save-as name can't be used, or `None` if it can. Split out of the picker's arm so the rule
/// is testable without driving the interactive prompt.
///
/// Only `last` is refused, and only here: it is the retired legacy-pointer name, which
/// [`scan_sessions`] deliberately skips. Accepting it would print "saved" for a file the picker can
/// neither restore nor delete, and pin every later autosave to it. Not folded into
/// [`sanitize_name`], which must keep mapping `last` verbatim so the legacy pointer stays loadable
/// and re-homable.
pub(crate) fn session_save_name_error(raw: &str) -> Option<&'static str> {
    (sanitize_name(raw.trim()) == "last").then_some(
        "“last” is the retired pointer name — pick another (it would not show up in /sessions)",
    )
}

/// Suggest a human-readable session name from the conversation's first user turn, so the "Save as"
/// prompt comes PRE-FILLED with the topic (Enter to accept, or edit) instead of a blank box. A short
/// hyphenated slug of the first few meaningful words + a date suffix to keep same-topic saves distinct.
///
/// Two properties this has to hold, both learned from what was on disk:
///
/// **No credential ever becomes a filename.** A key pasted as the first line of a chat used to pass
/// straight through — any token of 2+ chars was kept — and the derived stem is not just written to
/// disk but PRINTED by `/sessions`. A real machine had a session file named after a 40-char
/// vendor-prefixed token. Secret-shaped tokens are dropped here, before the name exists.
/// This is a name-derivation guard only; it does not redact the transcript body.
///
/// **ASCII whole words.** Names are folded through [`core::slug`] like every other id, so a
/// Vietnamese topic reads as `nguoi-dung-giao-tiep` rather than carrying diacritics into a filename
/// that then differs by normalization form between platforms. Folding happens per word, so no word
/// is ever cut apart. Existing accented files stay loadable — [`sanitize_name`] is unchanged, so it
/// still maps an on-disk name to itself.
pub(crate) fn suggest_session_name(history: &[Message]) -> String {
    let date = chrono::Local::now().format("%m%d").to_string();
    let first = history
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    // Skip slash-command / leading noise; take the first line, lowercase, keep word-ish chars.
    let line = first
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let words: Vec<String> = line
        .split_whitespace()
        .filter(|w| !crate::core::slug::looks_like_credential(w))
        // Intra-word punctuation is not a word boundary: `don't` is one word, so the apostrophe is
        // deleted rather than folded to `-` (which would leave a one-letter `t` fragment — exactly
        // the shredding this pass exists to remove).
        .map(|w| w.replace(['\'', '\u{2019}', '\u{02BC}'], ""))
        .map(|w| crate::core::slug::slug_words(&w, 40))
        .filter(|w| w.chars().count() >= 2)
        .take(5)
        .collect();
    let slug = words.join("-");
    // Cap length so long first messages don't produce an unwieldy default.
    let slug = crate::core::slug::truncate_at_word(&slug, 40);
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("chat-{date}")
    } else {
        format!("{slug}-{date}")
    }
}
/// Provenance stamped into every saved session so a file can answer "which project, which model,
/// when?" without the user cross-referencing anything. Every field is optional: a hand-edited or
/// pre-provenance file still parses, absent just means "unknown".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionMeta {
    /// Normalized canonical path key of the project root — the exact string `project_slug()`
    /// hashes, so "same project?" agrees byte-for-byte with zone identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated: Option<String>,
    /// Provider-reported token usage of every model call this conversation made, appended on each
    /// save (see [`merge_usage_ledger`]). Absent until the first call that reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<UsageLedger>,
}

/// The per-conversation usage ledger persisted in [`SessionMeta`]: running totals plus the
/// per-call rows behind them. Totals are kept separately so capping `rows` never loses money.
///
/// `epoch` / `last_seq` are the cost meter's cursor at the last save: the next save appends only
/// the rows recorded after it (same process), or every row the meter holds (another process — its
/// rows cannot be in this file). `turn_base` is added to the meter's process-local turn numbers so
/// a file resumed across processes keeps its `turn` column monotonic instead of restarting at 1.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UsageLedger {
    #[serde(default)]
    pub(crate) epoch: u64,
    #[serde(default)]
    pub(crate) last_seq: u64,
    #[serde(default)]
    pub(crate) turn_base: u64,
    #[serde(default)]
    pub(crate) calls: u64,
    #[serde(default)]
    pub(crate) input: u64,
    #[serde(default)]
    pub(crate) output: u64,
    #[serde(default)]
    pub(crate) cached: u64,
    #[serde(default)]
    pub(crate) cache_write: u64,
    #[serde(default)]
    pub(crate) rows: Vec<UsageRow>,
}

/// Rows a session file keeps; the oldest are dropped past this, the totals above stay exact.
pub(crate) const USAGE_ROWS_ON_DISK: usize = crate::llm::client::USAGE_ROWS_CAP;

/// Fold the rows the meter recorded since `existing` was written into a new ledger. Pure so the
/// cursor arithmetic is testable without the process-global meter: `cursor` is the meter's
/// `(epoch, seq)` now, `fresh` is what `rows_since(existing.epoch, existing.last_seq)` returned.
/// Returns `None` only when there is nothing at all to write.
pub(crate) fn merge_usage_ledger(
    existing: Option<UsageLedger>,
    cursor: (u64, u64),
    fresh: Vec<UsageRow>,
) -> Option<UsageLedger> {
    if fresh.is_empty() {
        // Nothing new: keep what the file has (or keep it absent) and leave its cursor alone, so
        // a save that raced ahead of the first model call does not stamp an empty ledger.
        return existing;
    }
    let mut ledger = existing.unwrap_or_default();
    if ledger.epoch != cursor.0 {
        // Another process (or none) wrote this file: its turn numbers restart at 1, so shift them
        // past whatever the file already holds.
        ledger.turn_base = ledger.rows.iter().map(|r| r.turn).max().unwrap_or(0);
    }
    for mut row in fresh {
        row.turn += ledger.turn_base;
        ledger.calls += 1;
        ledger.input += row.input;
        ledger.output += row.output;
        ledger.cached += row.cached;
        ledger.cache_write += row.cache_write;
        ledger.rows.push(row);
    }
    if ledger.rows.len() > USAGE_ROWS_ON_DISK {
        let excess = ledger.rows.len() - USAGE_ROWS_ON_DISK;
        ledger.rows.drain(..excess);
    }
    ledger.epoch = cursor.0;
    ledger.last_seq = cursor.1;
    Some(ledger)
}

/// On-disk shape of a saved session: `{"version":2,"meta":{…},"messages":[…]}`. The
/// pre-provenance format was a bare `Vec<Message>` array — [`parse_session_bytes`] accepts both.
/// `version` and `meta` both default: a future writer may bump the version, and a hand-written or
/// partially-written file that still has `messages` is worth loading. Only a missing/unparsable
/// `messages` makes a file unreadable.
#[derive(serde::Deserialize)]
struct SessionFile {
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    meta: SessionMeta,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    design_state: Option<TaskDesignState>,
    messages: Vec<Message>,
}

#[derive(serde::Serialize)]
struct SessionFileRef<'a> {
    version: u32,
    generation: u64,
    meta: &'a SessionMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    design_state: Option<&'a TaskDesignState>,
    messages: &'a [Value],
}

fn new_generation() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(1);
    let old = SEQ
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |last| Some(now.max(last.saturating_add(1))),
        )
        .unwrap_or(0);
    now.max(old.saturating_add(1))
}

/// An image data URL at or above this size is stored once under `sessions/blobs/<sha256>` and
/// referenced from the transcript as `blob:<sha256>`; smaller ones stay inline (E5.3, quality plan
/// S3: base64 screenshots were rewritten into the transcript on every turn).
const BLOB_INLINE_MAX: usize = 4 * 1024;
/// The per-turn autosave appends to `<slug>.jsonl` until the delta holds this many lines or bytes;
/// then the envelope is rewritten in full and the delta starts over.
const DELTA_MAX_LINES: usize = 20;
const DELTA_MAX_BYTES: u64 = 1024 * 1024;
const SESSIONS_KEEP_DEFAULT: usize = 200;
const SESSIONS_MAX_BYTES_DEFAULT: u64 = 256 * 1024 * 1024;

fn blobs_dir() -> std::path::PathBuf {
    sessions_dir().join("blobs")
}

/// The per-turn delta beside an envelope: `<slug>.jsonl`, one `Message` per line.
fn delta_path(envelope: &std::path::Path) -> std::path::PathBuf {
    envelope.with_extension("jsonl")
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Maximum encoded user-image blob size. Tool-image blobs hold raw bytes and use MAX_IMAGE_BYTES.
const MAX_USER_BLOB_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4 + 256;
const MAX_HISTORY_BYTES: usize = 128 * 1024 * 1024;

fn valid_blob_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Bound both the metadata check and the actual read (a file can grow between them).
pub(crate) fn read_bounded(
    path: &std::path::Path,
    limit: usize,
) -> Result<Vec<u8>, ImageUnavailable> {
    let file = std::fs::File::open(path).map_err(|_| ImageUnavailable::Missing)?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= limit as u64)
    {
        return Err(ImageUnavailable::Oversize);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ImageUnavailable::Corrupt)?;
    if bytes.len() > limit {
        return Err(ImageUnavailable::Oversize);
    }
    Ok(bytes)
}

fn read_blob(id: &str, limit: usize) -> Result<Vec<u8>, ImageUnavailable> {
    if !valid_blob_id(id) {
        return Err(ImageUnavailable::Corrupt);
    }
    let bytes = read_bounded(&blobs_dir().join(id), limit)?;
    if sha256_hex(&bytes) != id.to_ascii_lowercase() {
        return Err(ImageUnavailable::Corrupt);
    }
    Ok(bytes)
}

fn write_blob(id: &str, bytes: &[u8], limit: usize) -> Result<()> {
    anyhow::ensure!(
        valid_blob_id(id) && sha256_hex(bytes) == id,
        "invalid image blob hash"
    );
    anyhow::ensure!(bytes.len() <= limit, "image blob exceeds size limit");
    let dir = blobs_dir();
    std::fs::create_dir_all(&dir)?;
    config::harden_dir(&dir);
    let path = dir.join(id);
    // Never trust a filename as proof of integrity. Repair a damaged existing CAS entry atomically.
    if read_blob(id, limit).is_err() {
        crate::core::persist::atomic_write_owner_only(&path, bytes)
            .with_context(|| format!("writing image blob {}", path.display()))?;
    }
    Ok(())
}

fn image_diagnostic(message: &mut Value, id: &str, reason: ImageUnavailable) {
    let note = format!("[image unavailable: {reason:?}; blob:{id}]");
    let content = message.get_mut("content");
    match content {
        Some(Value::String(s)) => {
            s.push('\n');
            s.push_str(&note);
        }
        Some(Value::Array(parts)) => parts.push(serde_json::json!({"type":"text", "text":note})),
        _ => {
            message["content"] = Value::String(note);
        }
    }
}

/// Shared durable representation for session envelopes, deltas and recovery generations. Media is
/// content-addressed once; history carries metadata + a pointer, never repeated screenshot base64.
/// Payload retention is applied across the WHOLE transcript before an append decision is made.
pub(crate) fn stored_history(history: &[Message]) -> Result<Vec<Value>> {
    let mut count = 0usize;
    let mut bytes = 0usize;
    let mut out = Vec::with_capacity(history.len());
    for m in history.iter().rev() {
        let mut value = serde_json::to_value(m)?;
        let mut oversized = Vec::new();
        if let Some(parts) = value.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                let Some(url) = part
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if url.starts_with("blob:") || url.len() < BLOB_INLINE_MAX {
                    continue;
                }
                let id = sha256_hex(url.as_bytes());
                if url.len() > MAX_USER_BLOB_BYTES {
                    *part = Value::Null;
                    oversized.push(id);
                    // No allocation/write for oversized image data; keep an explicit diagnostic.
                } else {
                    write_blob(&id, url.as_bytes(), MAX_USER_BLOB_BYTES)?;
                    part["image_url"]["url"] = Value::String(format!("blob:{id}"));
                }
            }
        }
        if let Some(parts) = value.get_mut("content").and_then(Value::as_array_mut) {
            parts.retain(|part| !part.is_null());
        }
        for id in oversized {
            image_diagnostic(&mut value, &id, ImageUnavailable::Oversize);
        }
        if !m.tool_images.is_empty() {
            let mut images = Vec::with_capacity(m.tool_images.len());
            for image in m.tool_images.iter().rev() {
                if m.role == "user"
                    && image.metadata().provenance.source
                        == crate::core::reference_media::REFERENCE_SOURCE
                {
                    // Reference identity sidecars share the canonical user URL/blob. Never write a
                    // second raw-image blob or persist any volatile use/transmission grant here.
                    images.push(serde_json::to_value(
                        image.without_payload(ImageUnavailable::MetadataOnly),
                    )?);
                    continue;
                }
                let size = image.metadata().byte_len;
                let keep = image.is_available()
                    && count < crate::agent::observe::MAX_TOOL_IMAGES
                    && bytes
                        .checked_add(size)
                        .is_some_and(|n| n <= crate::agent::observe::MAX_TOOL_IMAGE_BYTES);
                if keep {
                    count += 1;
                    bytes += size;
                    let payload = image.bytes()?;
                    let id = &image.metadata().sha256;
                    write_blob(id, &payload, MAX_IMAGE_BYTES)?;
                    let mut stored = serde_json::to_value(
                        image.without_payload(ImageUnavailable::MetadataOnly),
                    )?;
                    stored["blob"] = Value::String(format!("blob:{id}"));
                    images.push(stored);
                } else {
                    let reason = image
                        .unavailable_reason()
                        .unwrap_or(ImageUnavailable::Retention);
                    images.push(serde_json::to_value(image.without_payload(reason))?);
                }
            }
            images.reverse();
            value["tool_images"] = Value::Array(images);
        }
        out.push(value);
    }
    out.reverse();
    Ok(out)
}

/// Hydrate bounded, hash-verified media before Message deserialization. A missing/corrupt payload
/// keeps its typed metadata and diagnostic instead of making the whole session unreadable.
fn resolve_stored_message(value: &mut Value) {
    let mut unavailable = Vec::new();
    if let Some(parts) = value.get_mut("content").and_then(Value::as_array_mut) {
        parts.retain_mut(|part| {
            let Some(url) = part.pointer("/image_url/url").and_then(Value::as_str) else {
                return true;
            };
            let Some(id) = url.strip_prefix("blob:") else {
                return true;
            };
            let id = id.to_owned();
            match read_blob(&id, MAX_USER_BLOB_BYTES)
                .and_then(|b| String::from_utf8(b).map_err(|_| ImageUnavailable::Corrupt))
            {
                Ok(url) if url.starts_with("data:image/") => {
                    part["image_url"]["url"] = Value::String(url);
                    true
                }
                Ok(_) => {
                    unavailable.push((id, ImageUnavailable::Corrupt));
                    false
                }
                Err(reason) => {
                    unavailable.push((id, reason));
                    false
                }
            }
        });
    }
    for (id, reason) in unavailable {
        image_diagnostic(value, &id, reason);
    }
    if let Some(images) = value.get_mut("tool_images").and_then(Value::as_array_mut) {
        for stored in images {
            let Some(reference) = stored
                .get("blob")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let metadata = stored
                .get("metadata")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok());
            let Some(metadata) = metadata else { continue };
            let unavailable = ToolImage::unavailable(metadata, ImageUnavailable::MetadataOnly);
            let loaded = reference
                .strip_prefix("blob:")
                .filter(|id| *id == unavailable.metadata().sha256)
                .ok_or(ImageUnavailable::Corrupt)
                .and_then(|id| read_blob(id, MAX_IMAGE_BYTES))
                .and_then(|bytes| {
                    unavailable
                        .hydrate(&bytes)
                        .map_err(|_| ImageUnavailable::Corrupt)
                });
            *stored = serde_json::to_value(match loaded {
                Ok(image) => image,
                Err(reason) => unavailable.without_payload(reason),
            })
            .unwrap_or(Value::Null);
        }
    }
}

pub(crate) fn parse_stored_history(bytes: &[u8]) -> Result<Vec<Message>> {
    anyhow::ensure!(
        bytes.len() <= MAX_HISTORY_BYTES,
        "history exceeds storage size limit"
    );
    let mut values: Vec<Value> = serde_json::from_slice(bytes)?;
    for value in &mut values {
        resolve_stored_message(value);
    }
    let mut messages: Vec<Message> = values
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    crate::agent::observe::retain_tool_images(&mut messages);
    Ok(messages)
}

pub(crate) type SessionSnapshot = (Vec<Message>, Option<SessionMeta>, Option<TaskDesignState>);
type SessionGeneration = (
    Vec<Message>,
    Option<SessionMeta>,
    Option<TaskDesignState>,
    u64,
);

/// Read envelope + only the deltas committed against that exact envelope generation. Legacy
/// untagged Message lines are accepted ONLY with a legacy generation-zero envelope.
pub(crate) fn read_session_path_with_design(
    path: &std::path::Path,
) -> Result<SessionSnapshot, String> {
    let bytes =
        read_bounded(path, MAX_HISTORY_BYTES).map_err(|e| format!("history unavailable: {e:?}"))?;
    let (mut msgs, meta, design_state, generation) = parse_session_full(&bytes)?;
    if let Ok(bytes) = read_bounded(&delta_path(path), MAX_HISTORY_BYTES) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            for line in text.lines().filter(|s| !s.trim().is_empty()) {
                let Ok(mut value) = serde_json::from_str::<Value>(line) else {
                    break;
                };
                let line_generation = value
                    .get("_session_generation")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if line_generation != generation {
                    continue;
                }
                resolve_stored_message(&mut value);
                match serde_json::from_value::<Message>(value) {
                    Ok(message) => msgs.push(message),
                    Err(_) => break,
                }
            }
        }
    }
    crate::agent::observe::retain_tool_images(&mut msgs);
    Ok((msgs, meta, design_state))
}

pub(crate) fn read_session_path_reason(
    path: &std::path::Path,
) -> Result<(Vec<Message>, Option<SessionMeta>), String> {
    read_session_path_with_design(path).map(|(m, meta, _)| (m, meta))
}

pub(crate) fn read_session_path(
    path: &std::path::Path,
) -> Option<(Vec<Message>, Option<SessionMeta>)> {
    read_session_path_reason(path).ok()
}

/// What the last full write or delta append left on disk for one slug, so the next autosave can
/// tell whether `history` merely grew (append) or was rewritten (compaction, `/undo` of a turn —
/// full write).
struct DeltaState {
    slug: String,
    saved_len: usize,
    prefix_fp: u64,
    design_fp: u64,
    generation: u64,
}

static DELTA_STATE: Mutex<Option<DeltaState>> = Mutex::new(None);

fn fingerprint(value: &impl serde::Serialize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(value).unwrap_or_default().hash(&mut h);
    h.finish()
}

fn note_saved(name: &str, stored: &[Value], design: Option<&TaskDesignState>, generation: u64) {
    let state = DeltaState {
        slug: sessions_dir()
            .join(format!("{}.json", sanitize_name(name)))
            .display()
            .to_string(),
        saved_len: stored.len(),
        prefix_fp: fingerprint(&stored),
        design_fp: fingerprint(&design),
        generation,
    };
    if let Ok(mut g) = DELTA_STATE.lock() {
        *g = Some(state);
    }
}

#[cfg(test)]
pub(crate) fn autosave_session_delta(
    history: &[Message],
    name: &str,
    model: Option<&str>,
) -> Result<String> {
    autosave_session_delta_with_design(history, name, model, None)
}

/// State-only mutations and any prefix/media-retention rewrite require a full coherent envelope.
/// JSONL entries remain flat Message objects for legacy readers, tagged with their base generation.
pub(crate) fn autosave_session_delta_with_design(
    history: &[Message],
    name: &str,
    model: Option<&str>,
    design: Option<&TaskDesignState>,
) -> Result<String> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    let delta = delta_path(&path);
    let stored = stored_history(history)?;
    let appendable = DELTA_STATE.lock().ok().and_then(|g| {
        g.as_ref().and_then(|s| {
            let same = s.slug == path.display().to_string() && path.is_file();
            let intact = stored
                .get(..s.saved_len)
                .is_some_and(|prefix| fingerprint(&prefix) == s.prefix_fp);
            (same && intact && fingerprint(&design) == s.design_fp)
                .then_some((s.saved_len, s.generation))
        })
    });
    if let Some((saved_len, generation)) = appendable {
        let existing = std::fs::read_to_string(&delta).unwrap_or_default();
        let torn = !existing.is_empty() && !existing.ends_with('\n');
        if !torn
            && existing.lines().count() < DELTA_MAX_LINES
            && existing.len() < DELTA_MAX_BYTES as usize
        {
            if stored.len() > saved_len {
                let mut buf = Vec::new();
                for m in &stored[saved_len..] {
                    let mut value = m.clone();
                    value["_session_generation"] = Value::from(generation);
                    serde_json::to_writer(&mut buf, &value)?;
                    buf.push(b'\n');
                }
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&delta)
                    .with_context(|| format!("opening {}", delta.display()))?;
                crate::core::persist::harden_owner_only_checked(&delta)?;
                f.write_all(&buf)
                    .with_context(|| format!("appending to {}", delta.display()))?;
                f.sync_all()?;
                crate::core::persist::harden_owner_only_checked(&delta)?;
                note_saved(name, &stored, design, generation);
            }
            return Ok(path.display().to_string());
        }
    }
    save_session_with_design(history, name, model, design)
}

/// Remove the oldest conversations beyond `sessions_keep` / `sessions_max_bytes`, never the one
/// being written. Returns how many were removed. Runs after every autosave; a pool under both
/// caps costs one directory stat.
pub(crate) fn prune_session_pool(live: Option<&str>) -> usize {
    let cfg = cli_config::load();
    prune_session_pool_with(
        cfg.sessions_keep.unwrap_or(SESSIONS_KEEP_DEFAULT),
        cfg.sessions_max_bytes.unwrap_or(SESSIONS_MAX_BYTES_DEFAULT),
        live,
    )
}

fn prune_session_pool_with(keep: usize, max_bytes: u64, live: Option<&str>) -> usize {
    let stats = stat_sessions(); // newest first
    let live = live.map(sanitize_name);
    let size_of = |p: &std::path::Path| {
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(delta_path(p))
                .map(|m| m.len())
                .unwrap_or(0)
    };
    let blob_bytes: u64 = std::fs::read_dir(blobs_dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    let mut total: u64 = blob_bytes + stats.iter().map(|s| size_of(&s.path)).sum::<u64>();
    let mut count = stats.len();
    let mut removed = 0usize;
    for s in stats.iter().rev() {
        if count <= keep && total <= max_bytes {
            break;
        }
        if live.as_deref() == Some(s.name.as_str()) {
            continue;
        }
        let bytes = size_of(&s.path);
        let _ = std::fs::remove_file(&s.path);
        let _ = std::fs::remove_file(delta_path(&s.path));
        count -= 1;
        total = total.saturating_sub(bytes);
        removed += 1;
    }
    if removed > 0 {
        gc_blobs();
    }
    removed
}

/// Remove image blobs no remaining transcript or delta references. A reference is the literal
/// `blob:<64 hex>` inside the file, so this is a byte scan, never a parse.
fn gc_blobs() {
    let dir = blobs_dir();
    let Ok(blobs) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        files.extend(rd.flatten().map(|e| e.path()).filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("json") | Some("jsonl")
            )
        }));
    }
    // Recovery generations reuse this CAS pool: deleting a session cannot invalidate a stale offer.
    if let Ok(rd) = std::fs::read_dir(config::aizen_home().join("recovery")) {
        for dir in rd.flatten() {
            if let Ok(rd) = std::fs::read_dir(dir.path()) {
                files.extend(
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json")),
                );
            }
        }
    }
    for p in files {
        // An unreadable reference source is not evidence of no references: fail closed.
        let Ok(text) = std::fs::read_to_string(&p) else {
            return;
        };
        let mut rest = text.as_str();
        while let Some(i) = rest.find("blob:") {
            let cand = &rest[i + 5..];
            let id: String = cand.chars().take(64).collect();
            if valid_blob_id(&id) {
                referenced.insert(id);
            }
            rest = &rest[i + 5..];
        }
    }
    for b in blobs.flatten() {
        let name = b.file_name().to_string_lossy().into_owned();
        let old_enough = b
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 60);
        // A writer publishes blobs before its envelope/manifest. A short grace period protects
        // those not-yet-referenced entries from another run's concurrent pool GC.
        if !referenced.contains(&name) && old_enough {
            let _ = std::fs::remove_file(b.path());
        }
    }
}

/// Parse either session format, reporting WHY a file failed. `Err` = unreadable/corrupt (callers
/// surface that explicitly — a corrupt file must never masquerade as an empty conversation).
///
/// The reason is not decoration. A silently-swallowed `serde` error is exactly how a real bug hid
/// for months: our own `Serialize` wrote `content` as a multimodal parts array for any turn with a
/// pasted image, `Option<String>` refused to read it back, and every such conversation simply became
/// "(unreadable)" with nothing anywhere saying "invalid type: sequence". Naming the error makes the
/// next asymmetry visible the first time it happens instead of after 29 files.
pub(crate) fn parse_session_reason(
    bytes: &[u8],
) -> Result<(Vec<Message>, Option<SessionMeta>), String> {
    parse_session_full(bytes).map(|(m, meta, _, _)| (m, meta))
}

fn parse_session_full(bytes: &[u8]) -> Result<SessionGeneration, String> {
    if bytes.len() > MAX_HISTORY_BYTES {
        return Err("session exceeds storage size limit".into());
    }
    let mut value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let messages = if value.is_array() {
        &mut value
    } else {
        value
            .get_mut("messages")
            .ok_or_else(|| "missing messages".to_owned())?
    };
    let array = messages
        .as_array_mut()
        .ok_or_else(|| "messages must be an array".to_owned())?;
    for message in array {
        resolve_stored_message(message);
    }
    if value.is_array() {
        let mut msgs: Vec<Message> = serde_json::from_value(value).map_err(|e| e.to_string())?;
        crate::agent::observe::retain_tool_images(&mut msgs);
        return Ok((msgs, None, None, 0));
    }
    let mut file: SessionFile = serde_json::from_value(value).map_err(|e| e.to_string())?;
    crate::agent::observe::retain_tool_images(&mut file.messages);
    Ok((
        file.messages,
        Some(file.meta),
        file.design_state,
        file.generation,
    ))
}

/// [`parse_session_reason`] for the callers that only branch on success.
pub(crate) fn parse_session_bytes(bytes: &[u8]) -> Option<(Vec<Message>, Option<SessionMeta>)> {
    parse_session_reason(bytes).ok()
}

pub(crate) fn save_session(history: &[Message], name: &str, model: Option<&str>) -> Result<String> {
    save_session_with_design(history, name, model, None)
}

pub(crate) fn save_session_with_design(
    history: &[Message],
    name: &str,
    model: Option<&str>,
    design: Option<&TaskDesignState>,
) -> Result<String> {
    // All three come from one cached identity lookup, so a file's key and slug can never disagree.
    write_session_with_design(
        history,
        name,
        SessionMeta {
            project_key: Some(config::project_key()),
            project_root: Some(config::project_root().display().to_string()),
            project_slug: Some(config::project_slug()),
            model: model
                .map(str::to_string)
                .or_else(|| cli_config::load().model),
            created: None,
            updated: None,
            // Filled by `write_session` from the cost meter, never by the caller.
            usage: None,
        },
        design,
    )
}

/// Re-home an UNATTRIBUTED transcript — the legacy `last.json` pointer copy — under a real slug
/// without inventing provenance for it. The pointer never recorded which project it came from, so
/// stamping the current one would assert on disk that another repo's conversation belongs here:
/// that lie then silences [`load_session`]'s cross-project warning, makes the file read as `here`
/// forever, and can never be undone (there is no original path left to restore). Absent fields are
/// the truth. Whatever the source DID record is carried through verbatim.
pub(crate) fn rehome_session(
    history: &[Message],
    name: &str,
    carried: Option<SessionMeta>,
    model: Option<&str>,
) -> Result<String> {
    let mut meta = carried.unwrap_or_default();
    meta.model = model.map(str::to_string).or(meta.model);
    write_session(history, name, meta)
}

/// How many saved sessions still carry a LEGACY zone slug — for `aizen zone migrate`'s plan.
/// Sessions are a flat pool keyed by the provenance INSIDE each file, so they are invisible to the
/// slug-directory sweep the rest of the migration does.
pub(crate) fn count_sessions_of_slug(legacy_slug: &str) -> usize {
    stat_sessions()
        .iter()
        .filter(|s| {
            read_session_row(&s.path)
                .1
                .and_then(|m| m.project_slug)
                .is_some_and(|sl| sl == legacy_slug)
        })
        .count()
}

/// Re-stamp every session recorded under `legacy_slug` with the CURRENT project identity — the
/// session leg of `aizen zone migrate`. Without it, a moved/re-cloned checkout (or a pre-fix
/// twin-zone population) left every one of the user's OWN transcripts reading as another project:
/// labeled `from <old dir>` in the picker and warned about on restore, permanently, because nothing
/// else in the migration touches provenance stored inside files.
///
/// `updated` is preserved: a bookkeeping rewrite must not make a stale conversation look freshly
/// used, exactly as memory retagging preserves its aging clock. (The file's mtime does move — std
/// has no portable way to set it, and adding a C dependency for cosmetics isn't worth it — so
/// `updated` stays the honest record of when the conversation itself last changed.)
pub(crate) fn retag_sessions_of_slug(legacy_slug: &str, on_error: &mut dyn FnMut(String)) -> usize {
    let key = config::project_key();
    let root = config::project_root().display().to_string();
    let slug = config::project_slug();
    let mut n = 0usize;
    for s in stat_sessions() {
        let Ok((msgs, Some(mut meta), design)) = read_session_path_with_design(&s.path) else {
            continue;
        };
        if meta.project_slug.as_deref() != Some(legacy_slug) {
            continue;
        }
        meta.project_key = Some(key.clone());
        meta.project_root = Some(root.clone());
        meta.project_slug = Some(slug.clone());
        let stored = match stored_history(&msgs) {
            Ok(stored) => stored,
            Err(e) => {
                on_error(format!("session {}: {e:#}", s.name));
                continue;
            }
        };
        let file = SessionFileRef {
            version: 2,
            generation: new_generation(),
            meta: &meta,
            design_state: design.as_ref(),
            messages: &stored,
        };
        let bytes = match serde_json::to_vec_pretty(&file) {
            Ok(mut b) => {
                b.push(b'\n');
                b
            }
            Err(e) => {
                on_error(format!("session {}: {e:#}", s.name));
                continue;
            }
        };
        match crate::core::persist::atomic_write(&s.path, &bytes)
            .and_then(|_| crate::core::persist::harden_owner_only_checked(&s.path))
        {
            Ok(_) => {
                let _ = std::fs::remove_file(delta_path(&s.path));
                n += 1;
            }
            Err(e) => on_error(format!("session {}: {e:#}", s.name)),
        }
    }
    n
}

/// Write a session file. `created` is preserved across the per-turn re-saves of one conversation
/// (existing file's stamp wins, then the caller's carried one, then now); `updated` is always now.
pub(crate) fn write_session(history: &[Message], name: &str, meta: SessionMeta) -> Result<String> {
    write_session_with_design(history, name, meta, None)
}

pub(crate) fn write_session_with_design(
    history: &[Message],
    name: &str,
    mut meta: SessionMeta,
    design: Option<&TaskDesignState>,
) -> Result<String> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    config::harden_dir(&dir);
    let path = dir.join(format!("{}.json", sanitize_name(name)));
    let existing_meta = std::fs::read(&path)
        .ok()
        .and_then(|b| parse_session_bytes(&b))
        .and_then(|(_, m)| m);
    let existing_created = existing_meta.as_ref().and_then(|m| m.created.clone());
    let now = chrono::Local::now().to_rfc3339();
    meta.created = existing_created
        .or(meta.created)
        .or_else(|| Some(now.clone()));
    meta.updated = Some(now);
    // Append the model calls recorded since this file was last written. The file's own cursor
    // decides what is new, so a save-as under another name gets the whole ledger and a resumed
    // conversation never re-appends rows it already holds.
    let existing_usage = existing_meta.and_then(|m| m.usage);
    let (seen_epoch, seen_seq) = existing_usage
        .as_ref()
        .map(|u| (u.epoch, u.last_seq))
        .unwrap_or((0, 0));
    let meter = crate::llm::client::cost_meter();
    meta.usage = merge_usage_ledger(
        existing_usage,
        meter.cursor(),
        meter.rows_since(seen_epoch, seen_seq),
    );
    let stored = stored_history(history)?;
    let generation = new_generation();
    let file = SessionFileRef {
        version: 2,
        generation,
        meta: &meta,
        design_state: design,
        messages: &stored,
    };
    let mut bytes = serde_json::to_vec_pretty(&file)?;
    bytes.push(b'\n');
    crate::core::persist::atomic_write(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    // The transcript can contain pasted secrets / .env contents → owner-only.
    crate::core::persist::harden_owner_only_checked(&path)?;
    // A full write supersedes any delta beside it, and is where the next delta starts from.
    let _ = std::fs::remove_file(delta_path(&path));
    note_saved(name, &stored, design, generation);
    Ok(path.display().to_string())
}
#[cfg(test)]
pub(crate) fn load_session(history: &mut Vec<Message>, name: &str, model: &str) -> Result<usize> {
    load_session_with_design(history, name, model).map(|(count, _)| count)
}

pub(crate) fn load_session_with_design(
    history: &mut Vec<Message>,
    name: &str,
    model: &str,
) -> Result<(usize, Option<TaskDesignState>)> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    if !path.is_file() {
        anyhow::bail!("no saved session '{name}'");
    }
    let (loaded, meta, design) = read_session_path_with_design(&path)
        .map_err(|why| anyhow::anyhow!("session '{name}' is unreadable: {why}"))?;
    *history = loaded;
    // Rebuild BOTH prompt lanes for the CURRENT project + model. The stable lane saved in the file
    // reflects wherever the session was recorded — replaying it verbatim in another checkout
    // grafted the OTHER project's <project_context>/frozen core onto this cwd: every tool ran here
    // while the model was told it was there. The splice keeps the conversation tail (and any
    // handoff seed) intact. Thread-switch resets (todos/cost/grants) are the CALLER's job — this
    // function only loads, so tests and backup paths can use it without mutating global state.
    refresh_prompt_lanes_for_thread_switch(history, model);
    // Cross-project restore is allowed but must be LOUD: name the source so "why does the model
    // think it's in the other repo?" never needs source-diving. Files without provenance stay
    // silent — there is nothing truthful to warn with.
    if let Some(theirs) = meta.as_ref().and_then(|m| m.project_key.as_ref()) {
        if *theirs != config::project_key() {
            let from = meta
                .as_ref()
                .and_then(|m| m.project_root.clone().or_else(|| m.project_slug.clone()))
                .unwrap_or_else(|| "unknown".to_string());
            // If the recorded directory is GONE, "another project" is the wrong accusation: the
            // overwhelmingly likely story is that this very checkout was moved or renamed, so the
            // conversation is the user's own history. Same facts, honest reading — and the phrasing
            // still says what was rebuilt, because the lane rewrite happened either way.
            let vanished = meta
                .as_ref()
                .and_then(|m| m.project_root.as_deref())
                .is_some_and(|r| !std::path::Path::new(r).exists());
            let headline = if vanished {
                format!("⚠ this session was saved at {from}, which no longer exists — moved or renamed project?")
            } else {
                format!("⚠ this session was saved in another project: {from}")
            };
            tui::emit_line(&style(headline).color256(theme::WARN).to_string());
            tui::emit_line(
                &style(format!(
                    "  system context rebuilt for the current project: {}",
                    config::project_root().display()
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
    }
    // Continue autosaving into the SAME file we just restored (don't spawn a fresh slug next turn)
    // — EXCEPT the legacy `last` pointer copy: pinning the live slug to `last` would make every
    // later turn overwrite the pointer instead of a real conversation, so re-home it first.
    let slug = sanitize_name(name);
    if slug == "last" {
        let fresh = allocate_session_slug(history);
        // The transcript is already restored into `history` at this point, so a failed re-home must
        // not fail the restore — report it and leave the slug unpinned, which makes the next autosave
        // allocate a fresh name (and say so) instead of overwriting the pointer.
        match save_session_with_design(history, &fresh, Some(model), design.as_ref()) {
            Ok(_) => set_session_slug(Some(fresh)),
            Err(e) => {
                set_session_slug(None);
                tui::emit_line(&format!(
                    "{} could not re-home the legacy `last` pointer: {e:#} — this chat will be saved under a new name on the next turn",
                    theme::warn("⚠")
                ));
            }
        }
    } else {
        set_session_slug(Some(slug));
    }
    // Keep the exit-flush snapshot in step with what was just restored, so an abrupt window close
    // right after re-saves this conversation, not a stale one.
    update_live_history_with_design(history, design.as_ref());
    Ok((conversation_len(history), design))
}
static SESSION_SLUG: OnceLock<Mutex<Option<String>>> = OnceLock::new();

pub(crate) fn session_slug_slot() -> &'static Mutex<Option<String>> {
    SESSION_SLUG.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_session_slug(slug: Option<String>) {
    let slug = slug.map(|s| sanitize_name(&s));
    *session_slug_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = slug.clone();
    crate::core::recovery::set_session_name(slug);
}

pub(crate) fn current_session_slug() -> Option<String> {
    session_slug_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}
pub(crate) fn pretty_session_name(name: &str) -> String {
    name.replace('-', " ")
}

/// Remove `atomic_write` staging files abandoned by a killed process.
///
/// `persist::atomic_write` deletes its temp only when the write fails IN-PROCESS; a process killed
/// between the staged write and the rename leaves `.{name}.aizen-tmp-{pid}-{seq}` behind with nothing
/// to ever collect it. Harmless to readers (`stat_sessions` filters on the `.json` extension) but it
/// accumulates silently for the life of the install.
///
/// Age-gated because the name is not enough: another aizen window may be mid-write RIGHT NOW, and its
/// staging file is invisible to us. A minute is far longer than any staged write and far shorter than
/// the interval at which anyone would notice clutter. Best-effort — never blocks startup.
pub(crate) fn sweep_orphan_temps() {
    crate::core::persist::sweep_orphan_temps_in(&sessions_dir());
}

/// One row of the session pool, as scanned from disk.
pub(crate) struct SessionInfo {
    pub(crate) name: String,
    /// Real conversation turns (leading system lanes excluded); `None` = unreadable/corrupt file —
    /// distinct from a readable empty one, so the picker can say "(unreadable)" instead of "0 msgs".
    pub(crate) msgs: Option<usize>,
    pub(crate) meta: Option<SessionMeta>,
    /// Modification time in Unix MILLIseconds. `None` = the filesystem wouldn't say (network share,
    /// FUSE mount, transient ACL error) — rendered as "age unknown" rather than posing as fresh.
    /// Milliseconds, not seconds, because two saves inside one second are routine (a `/clear` and
    /// the seeded turn's autosave) and a second-granularity tie fell through to ALPHABETICAL order,
    /// which points the wrong way as often as not while the picker still claims "newest first".
    pub(crate) mtime_ms: Option<u64>,
    /// Saved from THIS project? `None` = no provenance (pre-provenance file, project unknown).
    pub(crate) here: Option<bool>,
}

/// One session file as seen WITHOUT reading it. Statting a directory is cheap; deserializing every
/// multi-MB transcript in it is not — and the startup hint runs before the first prompt is even
/// accepted, so it must not pay for the whole pool just to name one conversation.
pub(crate) struct SessionStat {
    pub(crate) name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) mtime_ms: Option<u64>,
    /// Sort key: mtime clamped to now. A stamp in the FUTURE (clock skew from a VM resume, a
    /// pre-NTP boot, a dual-boot clock) would otherwise pin one file to the top of every launch's
    /// offer forever. `None` (filesystem wouldn't say) sorts last.
    pub(crate) recency: Option<u64>,
}

/// The pool, newest first, without reading any transcript. One ordering, shared by the hint and the
/// picker, so the two surfaces can never disagree about which conversation is the newest. The legacy
/// `last.json` pointer is a duplicate COPY of some conversation, not a session of its own — it never
/// appears as a row.
pub(crate) fn stat_sessions() -> Vec<SessionStat> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|x| x.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if name == "last" {
                continue;
            }
            let to_ms = |t: std::time::SystemTime| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)
            };
            let envelope_ms = e
                .metadata()
                .ok()
                .and_then(|md| md.modified().ok())
                .and_then(to_ms);
            // A conversation that only appended deltas since its last full write is as fresh as
            // its delta, not its envelope.
            let delta_ms = std::fs::metadata(delta_path(&path))
                .ok()
                .and_then(|md| md.modified().ok())
                .and_then(to_ms);
            let mtime_ms = match (envelope_ms, delta_ms) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            out.push(SessionStat {
                name,
                path,
                mtime_ms,
                recency: None,
            });
        }
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    for s in &mut out {
        s.recency = s.mtime_ms.map(|ms| ms.min(now_ms));
    }
    // Milliseconds, not seconds: two saves inside one second are routine (a `/clear` and the
    // seeded turn's autosave), and a second-granularity tie fell through to ALPHABETICAL order,
    // which points the wrong way as often as not while the prompt still says "newest first". Name
    // order remains the last resort so the sort is total and stable.
    out.sort_by(|a, b| b.recency.cmp(&a.recency).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Read one row's transcript-derived fields. `(None, None)` = unreadable/corrupt.
pub(crate) fn read_session_row(path: &std::path::Path) -> (Option<usize>, Option<SessionMeta>) {
    match read_session_path(path) {
        Some((m, meta)) => (Some(conversation_len(&m)), meta),
        None => (None, None),
    }
}

/// Read the whole pool, newest first — for the `/sessions` picker, which shows every row and so
/// genuinely needs every file parsed. The startup hint uses [`most_recent_session`] instead, which
/// parses lazily in the same order.
pub(crate) fn scan_sessions() -> Vec<SessionInfo> {
    let here_key = config::project_key();
    stat_sessions()
        .into_iter()
        .map(|s| {
            let (msgs, meta) = read_session_row(&s.path);
            let here = meta
                .as_ref()
                .and_then(|m| m.project_key.as_ref())
                .map(|k| *k == here_key);
            SessionInfo {
                name: s.name,
                msgs,
                meta,
                mtime_ms: s.mtime_ms,
                here,
            }
        })
        .collect()
}

/// Age of a session file for the picker. Distinct from [`fmt_time_ago`], which was written for
/// `/init --status` and maps both 0 and future stamps to "just now" — for a session row that would
/// print "just now" on an unreadable mtime and on a clock-skewed file, i.e. exactly the two cases
/// the user needs told apart from a genuinely fresh save.
pub(crate) fn fmt_session_age(mtime_ms: Option<u64>) -> String {
    let Some(ms) = mtime_ms else {
        return "age unknown".to_string();
    };
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if ms > now_ms.saturating_add(60_000) {
        return "future timestamp (clock skew)".to_string();
    }
    fmt_time_ago((ms / 1000).max(1)) // .max(1): second 0 is fmt_time_ago's "unknown" sentinel
}

/// Age in at most three cells: `now`, `5m`, `19h`, `62d`, `2y`, or `?` when the mtime is unreadable.
///
/// For a LIST, not a status line. `fmt_session_age` spells the unit out, which is right when one age
/// stands alone but wrong down a column: `19 hour(s) ago` against `1 min ago` is a six-character
/// jitter sitting directly in front of the subject, so nothing lines up and the eye has to re-find
/// the text on every row. A clock-skewed file reads `now` rather than announcing the skew — in a
/// 240-row picker that sentence is longer than the row it describes, and the file still sorts first.
pub(crate) fn fmt_session_age_compact(mtime_ms: Option<u64>) -> String {
    let Some(ms) = mtime_ms else {
        return "?".to_string();
    };
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let secs = now_ms.saturating_sub(ms) / 1000;
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=31_535_999 => format!("{}d", secs / 86_400),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// Short human label for where a foreign or unlabeled session came from — the picker/hint suffix.
/// Takes the meta rather than the row so the `last.json` re-home path (which has no row) can use it.
///
/// A recorded root that no longer EXISTS is flagged as such rather than presented as a live sibling
/// project: the usual cause is this checkout being moved or renamed, in which case a bare
/// "from <old dir>" is the picker calling the user's own history someone else's.
pub(crate) fn session_origin_label(meta: Option<&SessionMeta>) -> String {
    let root = meta.and_then(|m| m.project_root.as_deref());
    let named = root
        .and_then(|r| std::path::Path::new(r).file_name().and_then(|n| n.to_str()))
        .map(str::to_string)
        .or_else(|| meta.and_then(|m| m.project_slug.clone()));
    match named {
        Some(n) if root.is_some_and(|r| !std::path::Path::new(r).exists()) => {
            format!("from {n} (path gone)")
        }
        Some(n) => format!("from {n}"),
        None => "project unknown".to_string(),
    }
}

/// The session to offer for bare `/resume` and the startup hint: the newest one saved FROM THIS
/// project. Only when this project has none does it fall back to the pool's newest, labeled with
/// its origin — a cross-project resume must be a visible choice, never a trap.
///
/// Returns `(slug, conversation_turns, origin_label)`; the label is `None` for a same-project
/// offer. `None` overall when nothing restorable has been saved yet.
pub(crate) fn most_recent_session() -> Option<(String, usize, Option<String>)> {
    let here_key = config::project_key();
    // Parse in newest-first order and STOP at the first same-project hit, rather than deserializing
    // the whole pool to name one conversation. This runs before the first prompt is accepted, and a
    // long-lived pool is dozens of multi-MB transcripts (autosave per turn + a slug per handoff) —
    // on an AV-scanned or cloud-synced profile dir the eager scan was seconds of silent dead time.
    let stats = stat_sessions();
    let mut rows: Vec<(usize, usize, Option<SessionMeta>)> = Vec::new(); // (index, turns, meta)
    let mut best: Option<usize> = None; // index into `rows` of the best tier seen so far
    let tier = |meta: &Option<SessionMeta>| match meta.as_ref().and_then(|m| m.project_key.as_ref())
    {
        // Three tiers, not two. A file with NO provenance is not evidence of a foreign project — on
        // the first launch after upgrading EVERY file is keyless, so folding `None` in with
        // `Some(false)` made the whole pool "project unknown" and left the prefer-this-project rule
        // dead until each file had been resumed once. Unlabeled ranks between mine and theirs.
        Some(k) if *k == here_key => 0u8,
        None => 1,
        Some(_) => 2,
    };
    for (i, s) in stats.iter().enumerate() {
        let (msgs, meta) = read_session_row(&s.path);
        let Some(turns) = msgs else { continue }; // unreadable/corrupt — never offered
        let t = tier(&meta);
        rows.push((i, turns, meta));
        if best.is_none_or(|b| t < tier(&rows[b].2)) {
            best = Some(rows.len() - 1);
        }
        if t == 0 {
            break; // newest same-project file — nothing later in the order can beat it
        }
    }
    if let Some(b) = best {
        let (i, turns, meta) = &rows[b];
        // Only a file that PROVES it came from elsewhere gets the origin suffix. `None` provenance
        // has nothing truthful to say — the same rule `load_session` applies to its warning.
        let label = (tier(meta) == 2).then(|| session_origin_label(meta.as_ref()));
        return Some((stats[*i].name.clone(), *turns, label));
    }
    // Nothing restorable in the pool — which is NOT the same as an empty pool: one stray unparsable
    // `.json` in the dir used to make this fallback unreachable, hiding a perfectly readable
    // pointer-era transcript from the hint AND (since `last` is never a row) from the picker too.
    // Pre-provenance pool where only the shared `last.json` copy ever existed: re-home that
    // transcript into a real named file once, so it shows up in /sessions from now on.
    let (msgs, carried) = read_session_path(&sessions_dir().join("last.json"))?;
    if !msgs.iter().any(|m| m.role == "user") {
        return None;
    }
    let fresh = allocate_session_slug(&msgs);
    // Carry the pointer's own meta through rather than stamping THIS project onto it: the pointer
    // was project-blind, so claiming it as ours would be a lie that also silences load_session's
    // cross-project warning forever. Unattributed → offered as "project unknown", honestly.
    let label = carried
        .as_ref()
        .and_then(|m| m.project_key.as_ref())
        .map_or_else(
            || Some("project unknown".to_string()),
            |k| (*k != config::project_key()).then(|| session_origin_label(carried.as_ref())),
        );
    // Re-homing is a convenience, not a precondition: if the dir is unwritable the transcript
    // is still READABLE, so keep offering it under the legacy `last` name rather than pretending
    // there is nothing to resume. `load_session` re-homes on restore (and reports if that fails).
    match rehome_session(&msgs, &fresh, carried, None) {
        Ok(_) => Some((fresh, conversation_len(&msgs), label)),
        Err(_) => Some(("last".to_string(), conversation_len(&msgs), label)),
    }
}

/// Char-safe clip with an ellipsis, collapsed to one line — session text goes into prompt blocks
/// where a stray newline would break the row shape.
fn clip_line(s: &str, max_chars: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut out: String = flat.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Calendar day of a millisecond mtime (`2026-09-14`), for the `<sessions>` rows. A day and not
/// an age on purpose: "3m ago" re-renders on every turn, and one changed byte in the dynamic lane
/// invalidates the cached transcript behind it.
fn day_label(mtime_ms: Option<u64>) -> String {
    mtime_ms
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|| "date unknown".to_string())
}

/// One `<sessions>` row's worth of a transcript.
type SessionBrief = (usize, Option<SessionMeta>, String);
/// A transcript file's identity for the row cache: modification time and size.
type BriefStamp = (Option<std::time::SystemTime>, u64);
/// The row cache: path → (stamp it was parsed at, row).
type BriefCache = std::collections::HashMap<std::path::PathBuf, (BriefStamp, SessionBrief)>;

/// Row cache for [`read_session_brief`], valid while the file's `(mtime, len)` matches (E4.5,
/// quality plan M7: up to eight 400–750 KB transcripts were parsed at every conversation
/// boundary to render three rows). Keyed by path, so a test home's pool never collides.
fn session_brief_cache() -> &'static Mutex<BriefCache> {
    static CACHE: OnceLock<Mutex<BriefCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Transcripts parsed for a row since process start — a test's way to see the cache work.
#[cfg(test)]
static BRIEF_PARSES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The `<sessions>` row for one pool file: served from the row cache while the file's
/// `(mtime, len)` is unchanged, parsed once otherwise.
fn read_session_brief(path: &std::path::Path) -> Option<SessionBrief> {
    let fp = std::fs::metadata(path)
        .ok()
        .map(|m| (m.modified().ok(), m.len()));
    if let Some(fp) = fp {
        if let Ok(cache) = session_brief_cache().lock() {
            if let Some((seen, row)) = cache.get(path) {
                if *seen == fp {
                    return Some(row.clone());
                }
            }
        }
    }
    #[cfg(test)]
    BRIEF_PARSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let row = parse_session_brief(path)?;
    if let Some(fp) = fp {
        if let Ok(mut cache) = session_brief_cache().lock() {
            cache.insert(path.to_path_buf(), (fp, row.clone()));
        }
    }
    Some(row)
}

/// Parse one pool file just far enough for a `<sessions>` row: turn count, provenance, and the
/// first user message as the topic snippet.
fn parse_session_brief(path: &std::path::Path) -> Option<SessionBrief> {
    let (msgs, meta) = read_session_path(path)?;
    let snippet = msgs
        .iter()
        .find(|m| m.role == "user" && m.content.as_deref().is_some_and(|c| !c.trim().is_empty()))
        .and_then(|m| m.content.as_deref())
        .map(|c| clip_line(c, 100))
        .unwrap_or_default();
    Some((conversation_len(&msgs), meta, snippet))
}

/// The `<sessions>` block a fresh conversation's dynamic lane carries: up to three recent
/// same-project conversations, so "continue the most recent session" is something the model can SEE
/// and act on instead of groveling the filesystem for transcripts.
///
/// Until 0.5.0 the retired `last.json` pointer made that request work by accident — one fixed path
/// holding the newest transcript, readable blind. Its retirement (right for provenance) silently
/// removed the model's only route to prior work: the startup resume hint prints to the terminal,
/// which the model never sees. This block is the deliberate replacement; `session_recall` is the
/// matching fetch.
///
/// Same honesty rules as [`most_recent_session`]: prefer files that prove they are this project's,
/// and when only foreign/unlabeled ones exist, offer the single best with its origin stated. Cost
/// is bounded the same way too — newest-first, at most a handful of files parsed.
/// The `<sessions>` block adopted for the current conversation, keyed by the pool directory it was
/// built from. Built once per conversation boundary (see [`clear_sessions_block_cache`]) rather
/// than once per turn: the block rides in the dynamic system lane, which sits ahead of the whole
/// transcript in the request, and a prompt cache matches bytes — so a block that re-rendered on
/// every turn (it used to carry "3m ago" ages and the live message count of the very conversation
/// being autosaved) re-billed the entire transcript once per user turn. The key makes the cache
/// safe for tests that point `AIZEN_HOME` at a fresh directory.
static SESSIONS_BLOCK_CACHE: Mutex<Option<(std::path::PathBuf, Option<String>)>> = Mutex::new(None);

/// Forget the adopted `<sessions>` block so the next build re-reads the pool. Called at every
/// conversation boundary (startup, `/clear`, `/resume`, `/handoff`, restore).
pub(crate) fn clear_sessions_block_cache() {
    if let Ok(mut cache) = SESSIONS_BLOCK_CACHE.lock() {
        *cache = None;
    }
}

pub(crate) fn recent_sessions_block() -> Option<String> {
    let dir = sessions_dir();
    if let Ok(cache) = SESSIONS_BLOCK_CACHE.lock() {
        if let Some((cached_dir, block)) = cache.as_ref() {
            if *cached_dir == dir {
                return block.clone();
            }
        }
    }
    let block = build_recent_sessions_block();
    if let Ok(mut cache) = SESSIONS_BLOCK_CACHE.lock() {
        *cache = Some((dir, block.clone()));
    }
    block
}

/// Render the `<sessions>` rows from the pool. Byte-stable across a conversation by construction:
/// rows carry the file's calendar day (not an age), no message count, and never the conversation
/// currently being autosaved (whose file changes every turn).
fn build_recent_sessions_block() -> Option<String> {
    const EXAMINE_CAP: usize = 8;
    const ROWS_CAP: usize = 3;
    let here_key = config::project_key();
    let current = current_session_slug().map(|s| sanitize_name(&s));
    let stats = stat_sessions();
    let mut mine: Vec<String> = Vec::new();
    let mut fallback: Option<String> = None;
    for s in stats
        .iter()
        .filter(|s| current.as_deref() != Some(s.name.as_str()))
        .take(EXAMINE_CAP)
    {
        let Some((_turns, meta, snippet)) = read_session_brief(&s.path) else {
            continue;
        };
        let same = meta
            .as_ref()
            .and_then(|m| m.project_key.as_ref())
            .is_some_and(|k| *k == here_key);
        let row = |origin: &str| {
            let snip = if snippet.is_empty() {
                String::new()
            } else {
                format!(": \"{snippet}\"")
            };
            format!("- {} ({}{origin}){snip}", s.name, day_label(s.mtime_ms))
        };
        if same {
            mine.push(row(""));
            if mine.len() >= ROWS_CAP {
                break;
            }
        } else if fallback.is_none() {
            let foreign = meta
                .as_ref()
                .and_then(|m| m.project_key.as_ref())
                .is_some_and(|k| *k != here_key);
            let origin = if foreign {
                format!(", {}", session_origin_label(meta.as_ref()))
            } else {
                ", project unknown".to_string()
            };
            fallback = Some(row(&origin));
        }
    }
    let rows = if mine.is_empty() {
        vec![fallback?]
    } else {
        mine
    };
    let mut out = String::from(
        "<sessions>\nSaved conversations from earlier runs, newest first. When the user asks to \
         continue a previous or most-recent session, call `session_recall` (add name:\"…\" for any \
         but the newest) and continue the work from its digest — never go hunting through the \
         filesystem for transcripts. Restoring a full transcript is the user's move: /resume.\n",
    );
    for r in rows {
        out.push_str(&r);
        out.push('\n');
    }
    out.push_str("</sessions>\n");
    Some(out)
}

/// Model-facing digest of one saved conversation — the fetch half of "continue the most recent
/// session". Returns the opening request plus the latest exchanges, clipped hard: enough to resume
/// the WORK, deliberately not the transcript (that is `/resume`, and it is the user's call).
pub(crate) fn session_digest(name: Option<&str>) -> anyhow::Result<String> {
    let resolved = match name {
        Some(n) if !n.trim().is_empty() => sanitize_name(n.trim()),
        _ => {
            most_recent_session()
                .ok_or_else(|| anyhow::anyhow!("no saved conversations to recall"))?
                .0
        }
    };
    let path = sessions_dir().join(format!("{resolved}.json"));
    if !path.is_file() {
        return Err(anyhow::anyhow!(
            "no saved conversation named \"{resolved}\""
        ));
    }
    let (msgs, meta) = read_session_path(&path)
        .ok_or_else(|| anyhow::anyhow!("\"{resolved}\" is unreadable (corrupt session file)"))?;

    let mut out = format!(
        "Digest of saved conversation \"{resolved}\" — {} conversation messages",
        conversation_len(&msgs)
    );
    if let Some(m) = meta.as_ref() {
        if let Some(u) = m.updated.as_deref() {
            out.push_str(&format!(", last saved {}", clip_line(u, 25)));
        }
        if let Some(md) = m.model.as_deref() {
            out.push_str(&format!(", model {md}"));
        }
    }
    out.push_str(".\n");
    let foreign = meta
        .as_ref()
        .and_then(|m| m.project_key.as_ref())
        .is_some_and(|k| *k != config::project_key());
    if foreign {
        out.push_str(&format!(
            "NOTE: this conversation is {} — not this project.\n",
            session_origin_label(meta.as_ref())
        ));
    }

    let convo: Vec<&Message> = msgs
        .iter()
        .filter(|m| {
            (m.role == "user" || m.role == "assistant")
                && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
        })
        .collect();
    if let Some(first_user) = convo.iter().find(|m| m.role == "user") {
        out.push_str("\nOpening request:\n");
        out.push_str(&format!(
            "  user: {}\n",
            clip_line(first_user.content.as_deref().unwrap_or_default(), 500)
        ));
    }
    if convo.len() > 1 {
        out.push_str("\nLatest exchanges:\n");
        let tail_start = convo.len().saturating_sub(6);
        for m in &convo[tail_start..] {
            out.push_str(&format!(
                "  {}: {}\n",
                m.role,
                clip_line(m.content.as_deref().unwrap_or_default(), 700)
            ));
        }
    }
    out.push_str(
        "\nContinue the work from this digest plus the current repo state. The full transcript \
         loads only when the USER types /resume — no tool restores it.\n",
    );
    Ok(out)
}

/// Count of real conversation turns (excluding the leading system lanes) — for the resume hint, so
/// it reports what the user recognizes as "messages" rather than raw vector length.
pub(crate) fn conversation_len(history: &[Message]) -> usize {
    history
        .len()
        .saturating_sub(crate::agent::compact::leading_system_count(history))
}

pub(crate) async fn autosave_session_with_design(
    history: &[Message],
    _http: &reqwest::Client,
    _base_url: &str,
    _api_key: &str,
    model: &str,
    design: Option<&TaskDesignState>,
) {
    autosave_last_with_design(history, Some(model), design);
}

pub(crate) fn delete_session(name: &str) -> Result<()> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    let _ = std::fs::remove_file(delta_path(&path));
    Ok(())
}
/// Pick a distinct on-disk slug for a brand-new (unnamed) conversation: the topic suggestion, plus a
/// numeric suffix if a session with that name already exists. Without this every unnamed chat collided
/// on the shared `last` slug and overwrote the previous one, so `/sessions` only ever showed the latest.
pub(crate) fn allocate_session_slug(history: &[Message]) -> String {
    let base = sanitize_name(&suggest_session_name(history));
    let dir = sessions_dir();
    if !dir.join(format!("{base}.json")).exists() {
        return base;
    }
    for n in 2..1000 {
        let cand = format!("{base}-{n}");
        if !dir.join(format!("{cand}.json")).exists() {
            return cand;
        }
    }
    base
}

/// A process-global snapshot of the live conversation, kept fresh so the Windows console control
/// handler (window ✕ / logoff / shutdown) can flush the current chat to disk from its own thread
/// before the process is killed. The main thread never reads it back — it's write-for-the-handler.
static LIVE_HISTORY: OnceLock<Mutex<Vec<Message>>> = OnceLock::new();

/// Exit-only publication, never a source for choosing another run's active design handle.
#[derive(Clone, Default)]
struct LiveSnapshot {
    history: Vec<Message>,
    design_state: Option<TaskDesignState>,
}
static LIVE_SNAPSHOT: OnceLock<Mutex<LiveSnapshot>> = OnceLock::new();

pub(crate) fn update_live_history_with_design(
    history: &[Message],
    design: Option<&TaskDesignState>,
) {
    *LIVE_SNAPSHOT
        .get_or_init(|| Mutex::new(LiveSnapshot::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = LiveSnapshot {
        history: history.to_vec(),
        design_state: design.cloned(),
    };
    *live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = history.to_vec();
}

/// Read one retained payload only from an explicitly matching coherent conversation publication.
/// This is not a fallback for a missing ExecutionContext: owner/task/workspace and image provenance
/// must all match. A delegated run cannot read a parent or another window's screenshot.
pub(crate) fn scoped_live_tool_image(
    design: &crate::agent::design::DesignContext,
    evidence_id: &str,
) -> Option<ToolImage> {
    let requested = design.snapshot();
    let scope = design.evidence_scope();
    let guard = LIVE_SNAPSHOT
        .get_or_init(|| Mutex::new(LiveSnapshot::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let published = guard.design_state.as_ref()?;
    if published.owner_id != requested.owner_id
        || published.task_id != requested.task_id
        || published.workspace != requested.workspace
    {
        return None;
    }
    guard
        .history
        .iter()
        .rev()
        .flat_map(|m| m.tool_images.iter().rev())
        .find(|image| {
            let provenance = &image.metadata().provenance;
            provenance.evidence_id.as_deref() == Some(evidence_id)
                && provenance.design_scope.as_deref() == Some(scope.as_str())
        })
        .cloned()
}

pub(crate) fn live_history_slot() -> &'static Mutex<Vec<Message>> {
    LIVE_HISTORY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Refresh the snapshot the exit-flush path saves. Called after every user push (mid-turn safety) and
/// at the end of each autosave (so the snapshot is always at least as new as what's on disk).
pub(crate) fn update_live_history(history: &[Message]) {
    update_live_history_with_design(history, None);
}

/// Persist the live conversation on any exit — graceful (/quit, Ctrl-D) or abrupt (window ✕ via the
/// Windows console handler). Safe to call from a foreign thread: it only does synchronous file I/O.
pub(crate) fn flush_live_session_on_exit() {
    // Route the notices for teardown: the "· saving as" line is pointless when the render thread may
    // already be gone, and a failure must go to stderr instead of the TUI. Cleared on the way out so
    // this can't permanently mute a process that keeps running (the unix SIGHUP handler and the
    // test suite both call this without exiting).
    EXIT_FLUSHING.store(true, std::sync::atomic::Ordering::Relaxed);
    let snapshot = LIVE_SNAPSHOT
        .get_or_init(|| Mutex::new(LiveSnapshot::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // No REPL model label on this thread — `save_session` falls back to the configured model.
    autosave_last_with_design(&snapshot.history, None, snapshot.design_state.as_ref());
    append_memory_stats_sample();
    EXIT_FLUSHING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Append this session's §8 sample to `cli-memory/stats.jsonl`.
///
/// Runs on the exit path rather than per turn for two reasons: the populations are a directory scan
/// (three `load_*` calls) that has no business inside a turn, and one line per session keeps the
/// series readable by hand. The in-process counters are cumulative across the session, so a single
/// line at the end loses nothing but the intra-session shape — which no metric asks about.
///
/// Wholly best-effort. A failure to read the store means no sample, never a failed exit; and
/// `stats::append` itself declines to write when the session ran zero turns.
pub(crate) fn append_memory_stats_sample() {
    let all = memory::store::load_all().unwrap_or_default();
    let live = memory::bloat::supersede::active(&all).len();
    let archived = memory::bloat::caps::list_archive()
        .unwrap_or_default()
        .len();
    let review = memory::store::load_from(&crate::core::config::review_dir())
        .unwrap_or_default()
        .len();
    memory::stats::append(live, archived, all.len().saturating_sub(live), review);
}

/// Set while the exit-flush path is writing, so autosave stays silent during teardown.
static EXIT_FLUSHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mid-turn progress hook hnaded to the agent loop as a plain `fn` pointer (so `AgentConfig` keeps
/// its `Clone + Debug` derives and stays `dyn`-free).
///
/// Without this the exit-flush snapshot only advanced when a turn STARTED or FINISHED: the loop owns
/// `history` mutably for the whole turn, so a terminal closed mid-turn saved the user's question and
/// threw away every assistant reply and tool result the turn had already produced. The loop calls
/// this at each iteration boundary — the same point steering drains, where history is guaranteed
/// coherent (no `tool_calls` awaiting results) — so what lands on disk is always a valid transcript.
/// Memory-only by design; the actual file write happens once, on exit.
pub(crate) fn publish_live_history(history: &[Message]) {
    update_live_history(history);
}

/// Catch the terminal window being closed (✕), user logoff and system shutdown so the live chat is
/// flushed to `/sessions` before the OS terminates us. Ctrl-C / Ctrl-Break are deliberately left to
/// the existing in-app cancel handling (we return FALSE for them, changing nothing).
///
/// Both halves are needed because "the user closed the terminal" is a different OS event per
/// platform: a console control event on Windows, `SIGHUP` (pty hangup) or `SIGTERM` on unix. Without
/// the unix half, closing a terminal there killed the process with no flush at all and the whole
/// conversation was lost — the exact failure this handler exists to prevent.
pub(crate) fn install_exit_flush_handler() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::BOOL;
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        };
        unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
            if matches!(
                ctrl_type,
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
            ) {
                // Never let a panic unwind across the FFI boundary into the OS caller.
                let _ = std::panic::catch_unwind(flush_live_session_on_exit);
            }
            0 // FALSE — let the system proceed with default termination in all cases.
        }
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
    // Unix: closing the terminal emulator hangs up the pty (`SIGHUP`); a session manager or
    // `kill` sends `SIGTERM`. Default disposition for both is immediate termination, so the
    // transcript needs saving from the handler task. Watched on the tokio runtime (async signal
    // handling, no `unsafe`), then we exit ourselves — the default action is what the sender asked
    // for, so restoring the terminal and leaving is the honest response to it.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for kind in [SignalKind::hangup(), SignalKind::terminate()] {
            if let Ok(mut sig) = signal(kind) {
                tokio::spawn(async move {
                    if sig.recv().await.is_some() {
                        flush_live_session_on_exit();
                        // Give the alt-screen back before dying, else the user's shell is left in
                        // raw mode with no cursor.
                        tui::deactivate();
                        std::process::exit(0);
                    }
                });
            }
        }
    }
}

/// Best-effort auto-save of the live conversation (called after each turn and on exit) so you can
/// always come back to it via `/sessions` without ever running an explicit save. A brand-new chat is
/// given its own distinct file on first save so it never overwrites another unnamed conversation.
#[cfg(test)]
pub(crate) fn autosave_last(history: &[Message], model: Option<&str>) {
    autosave_last_with_design(history, model, None);
}

pub(crate) fn autosave_last_with_design(
    history: &[Message],
    model: Option<&str>,
    design: Option<&TaskDesignState>,
) {
    if history.iter().any(|m| m.role == "user") {
        let name = match current_session_slug() {
            Some(n) => n,
            None => {
                let slug = allocate_session_slug(history);
                set_session_slug(Some(slug.clone()));
                // Name the file OUT LOUD the moment it exists: "which file is THIS conversation
                // being written to?" must be answerable from the screen, not from source.
                if !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
                    tui::emit_line(&style(format!("· saving as “{slug}”")).dim().to_string());
                }
                slug
            }
        };
        // "auto-saves as you go" is a PROMISE (/help says so). When the write fails — full disk,
        // ACL damage, OneDrive/AV lock on ~/.aizen — swallowing it meant the user worked for hours
        // believing the transcript was on disk and found nothing to resume. Say it once per failure
        // streak (not every turn), and say it again after a recovery so the state is never stale.
        match autosave_session_delta_with_design(history, &name, model, design) {
            Ok(_) => {
                if AUTOSAVE_BROKEN.swap(false, std::sync::atomic::Ordering::Relaxed)
                    && !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed)
                {
                    tui::emit_line(
                        &style("· autosave recovered — this conversation is being saved again")
                            .dim()
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                if !AUTOSAVE_BROKEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let msg = format!(
                        "⚠ autosave failed: {e:#} — this conversation is NOT being saved. Fix the path or use /sessions to save elsewhere."
                    );
                    // The TUI may already be torn down on the exit-flush path; stderr still lands.
                    if EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("{msg}");
                    } else {
                        tui::emit_line(&style(msg).color256(theme::WARN).to_string());
                    }
                }
            }
        }
        let pruned = prune_session_pool(Some(&name));
        if pruned > 0 && !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
            tui::emit_line(
                &style(format!(
                    "· pruned {pruned} old conversation(s) from the pool (sessions_keep / sessions_max_bytes)"
                ))
                .dim()
                .to_string(),
            );
        }
        update_live_history_with_design(history, design);
        let _ = crate::core::recovery::checkpoint_history_with_design(
            history,
            None,
            crate::core::recovery::RecoveryPhase::Idle,
            design,
        );
    }
}

/// Latch so a persistent autosave failure warns ONCE per streak instead of every turn (and reports
/// once when writes start working again).
static AUTOSAVE_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod sessions_block_tests {
    use super::{
        clear_sessions_block_cache, recent_sessions_block, save_session, sessions_dir,
        set_session_slug,
    };
    use crate::core::types::Message;

    fn history(topic: &str) -> Vec<Message> {
        vec![
            Message::system("lane".to_string()),
            Message::user(topic.to_string()),
            Message::assistant("ok".to_string()),
        ]
    }

    #[test]
    fn sessions_block_is_byte_stable_for_the_conversation_and_skips_the_live_file() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-sess-stable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        std::fs::create_dir_all(sessions_dir()).unwrap();

        save_session(&history("fix the parser"), "fix-parser-0901", Some("m")).unwrap();
        clear_sessions_block_cache();
        let first = recent_sessions_block().expect("one saved file renders a block");
        assert!(first.contains("fix-parser-0901"));
        assert!(
            crate::ui::context_report::volatile_markers(&first).is_empty(),
            "no ages, counts or clocks may ride in the lane: {first}"
        );
        assert!(
            first.contains("(20"),
            "rows carry the file's calendar day: {first}"
        );

        // A file that appears mid-conversation (another window's autosave) must NOT re-render the
        // block this conversation already adopted — that is the byte-stability the cache needs.
        save_session(&history("add a feature"), "add-feature-0902", Some("m")).unwrap();
        let parsed_before = super::BRIEF_PARSES.load(std::sync::atomic::Ordering::Relaxed);
        let again = recent_sessions_block().unwrap();
        assert_eq!(
            again, first,
            "adopted block is reused verbatim within a conversation"
        );

        // At the next conversation boundary the pool is re-read — but the unchanged transcript is
        // NOT re-parsed: its row comes from the (mtime, len) cache; only the new file is parsed.
        clear_sessions_block_cache();
        let refreshed = recent_sessions_block().unwrap();
        assert!(refreshed.contains("add-feature-0902"));
        assert_eq!(
            super::BRIEF_PARSES.load(std::sync::atomic::Ordering::Relaxed) - parsed_before,
            1,
            "one new transcript parsed, the unchanged one served from the row cache"
        );

        // …and the conversation currently being autosaved never lists itself: its file changes
        // every turn, so a row for it could never be stable.
        set_session_slug(Some("add-feature-0902".to_string()));
        clear_sessions_block_cache();
        let without_self = recent_sessions_block().unwrap();
        assert!(without_self.contains("fix-parser-0901"));
        assert!(!without_self.contains("add-feature-0902"), "{without_self}");

        set_session_slug(None);
        clear_sessions_block_cache();
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod usage_ledger_tests {
    use super::{merge_usage_ledger, SessionMeta, UsageLedger, USAGE_ROWS_ON_DISK};
    use crate::core::types::UsageRow;

    fn row(turn: u64, input: u64, cached: u64) -> UsageRow {
        UsageRow {
            turn,
            input,
            output: 10,
            cached,
            cache_write: 0,
        }
    }

    #[test]
    fn nothing_fresh_leaves_the_file_alone() {
        assert_eq!(merge_usage_ledger(None, (7, 3), vec![]), None);
        let existing = UsageLedger {
            epoch: 1,
            last_seq: 4,
            calls: 2,
            ..Default::default()
        };
        assert_eq!(
            merge_usage_ledger(Some(existing.clone()), (7, 9), vec![]),
            Some(existing),
            "an empty append must not advance the cursor or touch the totals"
        );
    }

    #[test]
    fn first_save_starts_a_ledger_and_stamps_the_cursor() {
        let led = merge_usage_ledger(None, (7, 2), vec![row(1, 1_000, 0), row(1, 1_200, 900)])
            .expect("fresh rows create a ledger");
        assert_eq!((led.epoch, led.last_seq), (7, 2));
        assert_eq!(
            (led.calls, led.input, led.output, led.cached),
            (2, 2_200, 20, 900)
        );
        assert_eq!(led.turn_base, 0, "an empty file has nothing to shift past");
        assert_eq!(led.rows.len(), 2);
    }

    #[test]
    fn same_process_appends_without_shifting_turns() {
        let first =
            merge_usage_ledger(None, (7, 2), vec![row(1, 100, 0), row(1, 100, 80)]).unwrap();
        let second = merge_usage_ledger(Some(first), (7, 3), vec![row(2, 100, 90)]).unwrap();
        assert_eq!(second.last_seq, 3);
        assert_eq!(second.calls, 3);
        assert_eq!(
            second.rows.iter().map(|r| r.turn).collect::<Vec<_>>(),
            vec![1, 1, 2],
            "the meter's turn counter is already monotonic within one process"
        );
    }

    #[test]
    fn another_process_shifts_turns_past_the_file() {
        // Written by process 7 through turn 3; process 8 resumes it and its meter counts from 1.
        let old = merge_usage_ledger(None, (7, 5), vec![row(2, 100, 0), row(3, 100, 0)]).unwrap();
        let resumed = merge_usage_ledger(Some(old), (8, 1), vec![row(1, 100, 50)]).unwrap();
        assert_eq!(resumed.turn_base, 3);
        assert_eq!(resumed.rows.last().unwrap().turn, 4, "1 + base 3");
        assert_eq!((resumed.epoch, resumed.last_seq), (8, 1));
        // The second save from process 8 keeps the same base rather than re-deriving it, so the
        // column stays contiguous: 2, 3, 4, 5.
        let again = merge_usage_ledger(Some(resumed), (8, 2), vec![row(2, 100, 60)]).unwrap();
        assert_eq!(again.turn_base, 3);
        assert_eq!(
            again.rows.iter().map(|r| r.turn).collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
    }

    #[test]
    fn rows_are_capped_but_totals_are_not() {
        let fresh: Vec<UsageRow> = (0..(USAGE_ROWS_ON_DISK as u64 + 10))
            .map(|_| row(1, 1, 0))
            .collect();
        let led = merge_usage_ledger(None, (7, 1), fresh).unwrap();
        assert_eq!(led.rows.len(), USAGE_ROWS_ON_DISK);
        assert_eq!(led.calls, USAGE_ROWS_ON_DISK as u64 + 10);
        assert_eq!(led.input, USAGE_ROWS_ON_DISK as u64 + 10);
    }

    #[test]
    fn meta_round_trips_the_ledger_and_tolerates_its_absence() {
        let meta = SessionMeta {
            model: Some("m".into()),
            usage: merge_usage_ledger(None, (7, 1), vec![row(1, 5, 2)]),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&meta).unwrap();
        let back: SessionMeta = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.usage, meta.usage);
        // A pre-ledger file (no `usage` key) still parses, with the ledger simply absent.
        let legacy: SessionMeta = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(legacy.usage, None);
        // …and a ledger written by a newer aizen with extra fields does not take the file down.
        let newer: SessionMeta = serde_json::from_str(
            r#"{"usage":{"epoch":1,"last_seq":1,"rows":[{"turn":1,"input":5,"future":9}]}}"#,
        )
        .unwrap();
        assert_eq!(newer.usage.unwrap().rows[0].input, 5);
    }
}

#[cfg(test)]
mod bounded_sessions_tests {
    use super::*;

    fn with_pool<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-pool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        std::fs::create_dir_all(sessions_dir()).unwrap();
        let out = f();
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
        out
    }

    fn talk(topic: &str) -> Vec<Message> {
        vec![
            Message::system("lane"),
            Message::user(topic.to_string()),
            Message::assistant("ok".to_string()),
        ]
    }

    #[test]
    fn autosave_appends_a_delta_and_rewrites_only_when_it_must() {
        with_pool("delta", || {
            let mut h = talk("one");
            autosave_session_delta(&h, "delta-0915", Some("m")).unwrap();
            let json = sessions_dir().join("delta-0915.json");
            let jsonl = json.with_extension("jsonl");
            assert!(
                json.is_file() && !jsonl.exists(),
                "the first save is a full envelope"
            );
            let envelope_len = std::fs::metadata(&json).unwrap().len();

            h.push(Message::user("two".to_string()));
            h.push(Message::assistant("b".to_string()));
            autosave_session_delta(&h, "delta-0915", Some("m")).unwrap();
            assert_eq!(
                std::fs::metadata(&json).unwrap().len(),
                envelope_len,
                "a turn that only grew the history is appended, not rewritten"
            );
            assert_eq!(std::fs::read_to_string(&jsonl).unwrap().lines().count(), 2);
            let (msgs, _) = read_session_path(&json).unwrap();
            assert_eq!(msgs.len(), 5);
            assert_eq!(msgs[4].content.as_deref(), Some("b"));
            assert_eq!(load_session_len(&json), 5);

            // A history rewritten under us (compaction, a rewound turn) cannot be appended to.
            h.remove(1);
            autosave_session_delta(&h, "delta-0915", Some("m")).unwrap();
            assert!(!jsonl.exists(), "the delta was folded into a full write");
            assert_eq!(read_session_path(&json).unwrap().0.len(), 4);

            // The delta compacts once it holds DELTA_MAX_LINES lines.
            for i in 0..(DELTA_MAX_LINES + 2) {
                h.push(Message::user(format!("m{i}")));
                autosave_session_delta(&h, "delta-0915", Some("m")).unwrap();
            }
            let lines = std::fs::read_to_string(&jsonl)
                .map(|t| t.lines().count())
                .unwrap_or(0);
            assert!(lines < DELTA_MAX_LINES, "compacted: {lines} lines left");
            assert_eq!(
                read_session_path(&json).unwrap().0.len(),
                4 + DELTA_MAX_LINES + 2
            );
        });
    }

    fn load_session_len(json: &std::path::Path) -> usize {
        read_session_path(json).map(|(m, _)| m.len()).unwrap_or(0)
    }

    #[test]
    fn large_images_live_in_blobs_and_come_back_on_read() {
        with_pool("blobs", || {
            let big = format!(
                "data:image/png;base64,{}",
                "A".repeat(BLOB_INLINE_MAX + 100)
            );
            let h = vec![
                Message::system("lane"),
                Message::user_with_images("look", vec![big.clone()]),
                Message::assistant("ok"),
            ];
            save_session(&h, "img-0915", Some("m")).unwrap();
            let json = sessions_dir().join("img-0915.json");
            let raw = std::fs::read_to_string(&json).unwrap();
            assert!(
                raw.contains("blob:") && !raw.contains(&big),
                "the transcript holds a reference, not the bytes"
            );
            assert_eq!(std::fs::read_dir(blobs_dir()).unwrap().count(), 1);
            let (msgs, _) = read_session_path(&json).unwrap();
            assert_eq!(msgs[1].images, vec![big.clone()], "resolved on read");
            // The same image again is the same blob (content-addressed) …
            save_session(&h, "img2-0915", Some("m")).unwrap();
            assert_eq!(std::fs::read_dir(blobs_dir()).unwrap().count(), 1);
            // … and a blob that is gone drops the image, never the conversation.
            for e in std::fs::read_dir(blobs_dir()).unwrap().flatten() {
                std::fs::remove_file(e.path()).unwrap();
            }
            let (msgs, _) = read_session_path(&json).unwrap();
            assert!(msgs[1].images.is_empty());
            assert!(msgs[1]
                .content
                .as_deref()
                .unwrap()
                .contains("image unavailable: Missing"));
            assert!(msgs[1].content.as_deref().unwrap().starts_with("look"));
        });
    }

    fn design_state() -> TaskDesignState {
        crate::agent::design::DesignContext::new(
            "session-a",
            "workspace-a",
            crate::agent::design::DesignMode::On,
        )
        .snapshot()
    }

    fn image_message() -> Message {
        let mut message = Message::tool_result("capture", "");
        message.tool_images.push(
            ToolImage::from_bytes(
                b"GIF89a-retained",
                "image/gif",
                crate::core::tool_media::ImageProvenance::new("test", None),
            )
            .unwrap(),
        );
        message
    }

    #[test]
    fn design_state_only_change_rewrites_and_old_generation_delta_never_replays() {
        with_pool("design-delta", || {
            let mut h = talk("design");
            let mut state = design_state();
            autosave_session_delta_with_design(&h, "design", None, Some(&state)).unwrap();
            let path = sessions_dir().join("design.json");
            h.push(Message::assistant("new"));
            autosave_session_delta_with_design(&h, "design", None, Some(&state)).unwrap();
            let stale_delta = std::fs::read(delta_path(&path)).unwrap();
            state.render_revision = 9;
            autosave_session_delta_with_design(&h, "design", None, Some(&state)).unwrap();
            assert!(!delta_path(&path).exists());
            // Simulate death after atomic envelope rename but before removing the old delta.
            std::fs::write(delta_path(&path), stale_delta).unwrap();
            let (messages, _, saved) = read_session_path_with_design(&path).unwrap();
            assert_eq!(messages.len(), h.len());
            assert_eq!(saved.unwrap().render_revision, 9);
            let legacy = serde_json::to_vec(&talk("old")).unwrap();
            assert!(parse_session_full(&legacy).unwrap().2.is_none());
        });
    }

    #[test]
    fn user_reference_sidecars_never_duplicate_original_blob_or_restore_consent() {
        with_pool("reference-sidecar", || {
            let png = crate::core::tool_media::encode_rgba_png(&[1, 3, 5, 255], 1, 1).unwrap();
            let image = ToolImage::from_bytes(
                &png,
                "image/png",
                crate::core::tool_media::ImageProvenance::new("fixture", None),
            )
            .unwrap();
            let mut messages = vec![Message::user_with_images(
                "reference",
                vec![image.data_url()],
            )];
            let registry = crate::core::reference_media::ReferenceRegistry::new("owner");
            let descriptor = registry.register_loaded(&mut messages).unwrap().remove(0);
            // Even a live hydrated sidecar must be persisted metadata-only, sharing the user URL.
            messages[0].tool_images[0] = messages[0].tool_images[0].hydrate(&png).unwrap();
            save_session(&messages, "reference-sidecar", None).unwrap();
            assert!(!blobs_dir().join(&descriptor.original.sha256).exists());
            let mut loaded = read_session_path(&sessions_dir().join("reference-sidecar.json"))
                .unwrap()
                .0;
            assert!(!loaded[0].tool_images[0].is_available());
            let restored = crate::core::reference_media::ReferenceRegistry::new("owner");
            assert_eq!(
                restored.register_loaded(&mut loaded).unwrap(),
                vec![descriptor]
            );
            assert!(crate::core::exec_ctx::ExecutionContext::new(
                crate::core::convo::ConversationId::new("owner")
            )
            .references()
            .is_none());
        });
    }

    #[test]
    fn tool_media_roundtrip_corruption_and_path_traversal_keep_metadata() {
        with_pool("typed-media", || {
            let history = vec![Message::user("look"), image_message()];
            save_session(&history, "media", None).unwrap();
            let path = sessions_dir().join("media.json");
            let raw = std::fs::read_to_string(&path).unwrap();
            assert!(raw.contains("blob:"));
            assert!(!raw.contains("base64"));
            let (loaded, _) = read_session_path(&path).unwrap();
            assert_eq!(loaded[1].tool_images, history[1].tool_images);
            let id = &history[1].tool_images[0].metadata().sha256;
            std::fs::write(blobs_dir().join(id), b"corrupt").unwrap();
            let (loaded, _) = read_session_path(&path).unwrap();
            assert_eq!(
                loaded[1].tool_images[0].unavailable_reason(),
                Some(ImageUnavailable::Corrupt)
            );
            assert_eq!(
                loaded[1].tool_images[0].metadata(),
                history[1].tool_images[0].metadata()
            );
            assert_eq!(
                read_blob("../secret", MAX_IMAGE_BYTES),
                Err(ImageUnavailable::Corrupt)
            );
            // Saving a valid payload repairs a corrupt existing content-addressed entry.
            save_session(&history, "media", None).unwrap();
            assert!(read_session_path(&path).unwrap().0[1].tool_images[0].is_available());
        });
    }

    #[test]
    fn retention_rewrite_happens_before_delta_and_scope_lookup_is_not_global_fallback() {
        with_pool("retained", || {
            let design = crate::agent::design::DesignContext::new(
                "owner",
                "workspace",
                crate::agent::design::DesignMode::On,
            );
            let mut state = design.snapshot();
            state.task_id = "task".into();
            design.restore(state.clone()).unwrap();
            let mut history = vec![Message::user("look")];
            for _ in 0..crate::agent::observe::MAX_TOOL_IMAGES {
                history.push(image_message());
            }
            autosave_session_delta_with_design(&history, "retained", None, Some(&state)).unwrap();
            history.push(image_message());
            autosave_session_delta_with_design(&history, "retained", None, Some(&state)).unwrap();
            let path = sessions_dir().join("retained.json");
            assert!(
                !delta_path(&path).exists(),
                "eviction rewrites the old prefix coherently"
            );
            let loaded = read_session_path(&path).unwrap().0;
            assert_eq!(
                loaded[1].tool_images[0].unavailable_reason(),
                Some(ImageUnavailable::Retention)
            );
            assert_eq!(
                loaded
                    .iter()
                    .flat_map(|m| &m.tool_images)
                    .filter(|i| i.is_available())
                    .count(),
                32
            );
            let mut image = image_message();
            let mut provenance = image.tool_images[0].metadata().provenance.clone();
            provenance.evidence_id = Some("ev".into());
            provenance.design_scope = Some(design.evidence_scope());
            image.tool_images[0] = ToolImage::from_bytes(
                &image.tool_images[0].bytes().unwrap(),
                "image/gif",
                provenance,
            )
            .unwrap();
            update_live_history_with_design(&[image], Some(&state));
            assert!(scoped_live_tool_image(&design, "ev").is_some());
            let other = crate::agent::design::DesignContext::new(
                "other",
                "workspace",
                crate::agent::design::DesignMode::On,
            );
            assert!(scoped_live_tool_image(&other, "ev").is_none());
            assert!(scoped_live_tool_image(&design, "foreign-id").is_none());
        });
    }

    #[test]
    fn blob_limits_and_recovery_references_are_respected() {
        with_pool("blob-limits", || {
            let path = sessions_dir().join("oversized");
            let file = std::fs::File::create(&path).unwrap();
            file.set_len((MAX_IMAGE_BYTES + 1) as u64).unwrap();
            assert_eq!(
                read_bounded(&path, MAX_IMAGE_BYTES),
                Err(ImageUnavailable::Oversize)
            );
            let h = vec![Message::user("keep"), image_message()];
            let stored = stored_history(&h).unwrap();
            let id = &h[1].tool_images[0].metadata().sha256;
            let recovery = config::aizen_home().join("recovery").join("other-run");
            std::fs::create_dir_all(&recovery).unwrap();
            std::fs::write(
                recovery.join("history.1.json"),
                serde_json::to_vec(&stored).unwrap(),
            )
            .unwrap();
            gc_blobs();
            assert!(
                blobs_dir().join(id).exists(),
                "recovery and sessions share one CAS reference graph"
            );
            std::fs::remove_file(blobs_dir().join(id)).unwrap();
            let loaded = parse_stored_history(&serde_json::to_vec(&stored).unwrap()).unwrap();
            assert_eq!(
                loaded[1].tool_images[0].unavailable_reason(),
                Some(ImageUnavailable::Missing)
            );
        });
    }

    #[test]
    fn the_pool_is_pruned_to_the_caps_but_never_the_live_session() {
        with_pool("prune", || {
            for i in 0..5 {
                save_session(
                    &talk(&format!("topic {i}")),
                    &format!("s{i}-0915"),
                    Some("m"),
                )
                .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            // Keep two, but s0 (the oldest) is the live one and must survive.
            let removed = prune_session_pool_with(2, u64::MAX, Some("s0-0915"));
            assert_eq!(removed, 3, "s1, s2, s3 go; s0 is live, s4 is newest");
            let mut names: Vec<String> = stat_sessions().into_iter().map(|s| s.name).collect();
            names.sort();
            assert_eq!(names, vec!["s0-0915".to_string(), "s4-0915".to_string()]);
            // A byte cap the pool cannot meet removes everything but the live conversation.
            let removed = prune_session_pool_with(100, 1, Some("s0-0915"));
            assert_eq!(removed, 1);
            assert_eq!(stat_sessions().len(), 1);
            // No caps hit → nothing removed, nothing scanned away.
            assert_eq!(prune_session_pool_with(100, u64::MAX, None), 0);
        });
    }
}
