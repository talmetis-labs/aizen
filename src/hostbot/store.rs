//! `~/.aizen/hostbot/` — the host-bot feature's own data directory, separate from `cli-config.json`.
//! Holds two things:
//!   * `bots.json` — the extra bots hosted by the Telegram daemon (`/addbot` writes here). The PRIMARY
//!     bot's identity stays in `cli-config.json` (it's the daemon's own auth, not a "hosted" bot).
//!   * `sessions/` — one self-describing JSON file per (platform, route, chat) conversation, so a daemon
//!     restart (the whole point of `Restart=always` self-host) keeps its context instead of losing it.
//!
//! Everything here is secret-bearing (bot tokens, conversation history) → the dir is 0700 and every
//! file 0600 on Unix (no-op on Windows, where the profile ACL governs), matching the token-cache
//! discipline used elsewhere.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::core::config;
use crate::core::session_control::SessionControl;
use crate::core::session_store::{PersistenceSnapshot, SaveControl};
use crate::core::types::Message;

/// `~/.aizen/hostbot/` — created + hardened to 0700 on first touch. Best-effort hardening (never fails
/// the caller); creation errors surface where the path is used to write.
pub fn hostbot_dir() -> PathBuf {
    let dir = config::aizen_home().join("hostbot");
    let _ = std::fs::create_dir_all(&dir);
    config::harden_dir(&dir);
    dir
}

fn sessions_dir() -> PathBuf {
    let dir = hostbot_dir().join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    config::harden_dir(&dir);
    dir
}

fn bots_path() -> PathBuf {
    hostbot_dir().join("bots.json")
}

fn lanes_path() -> PathBuf {
    hostbot_dir().join("lanes.json")
}

// ── hosted bots (bots.json) ──────────────────────────────────────────────────────────

/// One EXTRA bot hosted by the daemon (moved here from `cli_config::TelegramBot`). Each has its own
/// @BotFather token but shares the owner's `allowed_chat_ids` (a private chat id == the owner's user
/// id, identical across all their bots) unless it carries its own override. Managed live via
/// `/addbot` / `/rmbot`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HostedBot {
    /// Short unique label used by `/bots` / `/rmbot` (never the reserved name "default").
    pub name: String,
    /// This bot's @BotFather token (stored plaintext; the file is 0600 on Unix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Per-bot allowlist override. Empty ⇒ inherit the primary bot's `allowed_chat_ids`.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    /// Persona this sub-bot wears (name of a persona card). `None` ⇒ the daemon's global persona
    /// (the "default" bot's voice). Persistent Memory is separate: any context comes from the
    /// selected provider under its configured scope and grants, not a built-in global frozen store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// This bot answers to its OWN owner instead of inheriting the primary's allowlist.
    ///
    /// The default (`false`) is what a private-chat setup wants: a private chat id EQUALS the owner's
    /// user id, identical across every bot they own, so inheriting is both correct and convenient.
    /// Set `true` to hand a sub-bot to somebody else: it then boots with an empty allowlist in pairing
    /// mode and only the chat that answers its code can drive it. Note what this does NOT change —
    /// the bot still runs shell commands on THIS machine, so a paired stranger holds real power; it
    /// exists so the *primary* owner's chats aren't also exposed on that bot.
    #[serde(default)]
    pub own_owner: bool,
    /// Which host runs this bot, matched against `core::device` (hostname, or the stable device id).
    /// `None` ⇒ any host may run it (the single-machine default, and what every existing file says).
    /// Telegram allows exactly ONE `getUpdates` poller per token, so two machines starting the same
    /// bot is not a redundancy win — it's a 409 fight. Naming the host is how a fleet divides them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Per-lane settings — one entry per hosted-bot route, INCLUDING the primary `"default"`.
///
/// Why this is a separate file from `bots.json`: that file is identity (token, owner, persona) and is
/// rewritten when bots are added or removed; this one is per-conversation *preferences* a chat
/// changes constantly via `/cd`, `/model`, `/effort`. Keeping them apart means a `/cd` can never
/// rewrite a token, and the primary bot — which has no `bots.json` entry at all — still gets lanes.
///
/// Every field is `None` ⇒ "inherit the process-wide config", so an absent file (every install before
/// this feature) behaves exactly as it did when these commands wrote `cli-config.json` globally.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LaneSettings {
    /// Route this applies to (`"default"` for the primary bot).
    pub route: String,
    /// Working directory for this lane's agent + `/sh`. `None` ⇒ the process cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `low`|`medium`|`high`|`xhigh`|`max`, or `"off"` to send no effort field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ultimate: Option<bool>,
}

/// Load every lane's settings. Missing / corrupt file ⇒ `vec![]` (all lanes inherit the global config).
pub fn load_lanes() -> Vec<LaneSettings> {
    match std::fs::read_to_string(lanes_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// One lane's settings, or an all-`None` default when it has never set anything.
pub fn load_lane(route: &str) -> LaneSettings {
    load_lanes()
        .into_iter()
        .find(|l| l.route == route)
        .unwrap_or_else(|| LaneSettings {
            route: route.to_string(),
            ..LaneSettings::default()
        })
}

/// Read-modify-write ONE lane under the store lock, creating its entry if absent. Locked because
/// concurrent lanes each persist their own settings into the same file.
pub fn update_lane<T>(
    route: &str,
    mutate: impl FnOnce(&mut LaneSettings) -> Result<T>,
) -> Result<T> {
    let path = lanes_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "lanes");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let mut lanes: Vec<LaneSettings> = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if !lanes.iter().any(|l| l.route == route) {
        lanes.push(LaneSettings {
            route: route.to_string(),
            ..LaneSettings::default()
        });
    }
    let entry = lanes
        .iter_mut()
        .find(|l| l.route == route)
        .expect("just inserted");
    let result = mutate(entry)?;
    let json = serde_json::to_string_pretty(&lanes)?;
    crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes())?;
    Ok(result)
}

/// Forget a lane's settings (a removed bot). Best-effort: a failure leaves a harmless stale entry
/// that the next `update_lane` on that route would simply reuse.
pub fn drop_lane(route: &str) {
    let path = lanes_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "lanes");
    let Ok(_lock) = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    ) else {
        return;
    };
    let mut lanes: Vec<LaneSettings> = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => return,
    };
    lanes.retain(|l| l.route != route);
    if let Ok(json) = serde_json::to_string_pretty(&lanes) {
        let _ = crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes());
    }
}

/// The daemon writes an array. Older desktop versions wrote keyed maps, optionally under "bots".
fn decode_bots(raw: &str) -> Result<Vec<HostedBot>> {
    let root: serde_json::Value = serde_json::from_str(raw)?;
    if root.is_array() {
        return Ok(serde_json::from_value(root)?);
    }
    let mut map = root
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("invalid bot registry"))?;
    if map.len() == 1
        && map
            .get("bots")
            .is_some_and(|entry| entry.is_object() && entry.get("token").is_none())
    {
        map = map
            .remove("bots")
            .and_then(|v| v.as_object().cloned())
            .unwrap();
    }
    map.into_iter()
        .map(|(name, mut entry)| {
            let obj = entry
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid hosted bot"))?;
            obj.insert("name".into(), name.into());
            Ok(serde_json::from_value(entry)?)
        })
        .collect()
}

fn read_bots(path: &std::path::Path) -> Result<Vec<HostedBot>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => decode_bots(&raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn load_bots() -> Vec<HostedBot> {
    read_bots(&bots_path()).unwrap_or_default()
}

pub fn update_bots<T>(mutate: impl FnOnce(&mut Vec<HostedBot>) -> Result<T>) -> Result<T> {
    let path = bots_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "bots");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let mut bots = read_bots(&path)?;
    let result = mutate(&mut bots)?;
    let json = serde_json::to_string_pretty(&bots)?;
    crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes())?;
    Ok(result)
}

/// Persist the hosted-bot list to `hostbot/bots.json`, hardened to owner-only.
///
/// Every mutation goes through the read-modify-write helpers in this module, which hold the store
/// lock across the whole change; a bare save would let a concurrent edit be overwritten. Kept as the
/// single place that knows the file's permissions and layout.
#[allow(dead_code)]
pub fn save_bots(bots: &[HostedBot]) -> Result<()> {
    let path = bots_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "bots");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let json = serde_json::to_string_pretty(bots)?;
    crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes())?;
    Ok(())
}

// ── persisted sessions (sessions/*.json) ───────────────────────────────────────────────

/// A conversation session on disk. Self-describing (carries its own keys) so we never parse identity
/// out of the file NAME — the filename is just a sanitized, collision-free handle.
#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    #[serde(default)]
    version: u32,
    #[serde(
        default,
        deserialize_with = "crate::core::session_store::deserialize_session_control"
    )]
    session_control: Option<SessionControl>,
    platform: String,
    route: String,
    chat: String,
    // Explicit Core storage projection also removes input pixels and bounds tool-media retention.
    // Generic Message serde is the model-wire shape and intentionally drops host turn identity.
    #[serde(
        serialize_with = "crate::core::session_store::serialize_stored_messages",
        deserialize_with = "crate::core::session_store::deserialize_stored_messages"
    )]
    messages: Vec<Message>,
    #[serde(default)]
    task_state: crate::agent::todo::TaskState,
}

impl SessionFile {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version <= 1, "unsupported hostbot session version");
        anyhow::ensure!(
            self.version == 0 || self.session_control.is_some(),
            "missing hostbot session_control"
        );
        self.task_state.validate()?;
        if let Some(control) = &self.session_control {
            control.validate()?;
        }
        Ok(())
    }
}

/// Sanitize a component for use in a filename: keep `[a-z0-9_-]`, map everything else (incl. the `-`
/// of a negative chat id → kept; `.` path separators → `_`) so no value can escape the sessions dir.
/// Platform slugs + bot names are already `[a-z0-9_]`; a chat id is `-?\d+`; this is defense in depth.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn session_path(platform: &str, route: &str, chat: &str) -> PathBuf {
    let name = format!(
        "{}.{}.{}.json",
        sanitize(platform),
        sanitize(route),
        sanitize(chat)
    );
    sessions_dir().join(name)
}

/// Legacy projection used by storage fixtures. Live lane startup must retain the owned task state.
#[cfg(test)]
pub fn load_sessions(platform: &str) -> Vec<(String, String, Vec<Message>)> {
    load_sessions_with_state(platform)
        .into_iter()
        .map(|(route, chat, messages, _, _)| (route, chat, messages))
        .collect()
}

/// Load validated lane envelopes; old files without a task start with an empty owned state.
/// Corrupt / foreign files are skipped (a bad lane must not crash the daemon).
#[cfg(test)]
pub fn load_sessions_with_state(
    platform: &str,
) -> Vec<(
    String,
    String,
    Vec<Message>,
    crate::agent::todo::TaskState,
    SessionControl,
)> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let mut rejected = 0usize;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(s) = std::fs::read_to_string(&path) else {
            rejected = rejected.saturating_add(1);
            continue;
        };
        let Ok(sf) = serde_json::from_str::<SessionFile>(&s) else {
            rejected = rejected.saturating_add(1);
            continue;
        };
        if sf.platform != platform {
            continue;
        }
        if sf.validate().is_err() {
            rejected = rejected.saturating_add(1);
            continue;
        }
        out.push((
            sf.route,
            sf.chat,
            sf.messages,
            sf.task_state,
            sf.session_control.unwrap_or_default(),
        ));
    }
    if rejected > 0 {
        // One bounded diagnostic per scan; never echo filenames, parser errors or stored content.
        eprintln!("[hostbot] skipped {rejected} unreadable or unsupported session file(s); existing files remain protected from overwrite");
    }
    out
}

/// Authoritative lazy lane read. An absent scan seed is NOT permission to start normally: scan
/// projections skip invalid files, whereas execution startup must call this and propagate errors.
pub(crate) fn load_session_with_state(
    platform: &str,
    route: &str,
    chat: &str,
) -> Result<Option<(Vec<Message>, crate::agent::todo::TaskState, SessionControl)>> {
    let path = session_path(platform, route, chat);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("hostbot session is unavailable"),
        Ok(_) => {}
    }
    let bytes = crate::core::session_store::read_bounded(&path, 128 * 1024 * 1024)
        .map_err(|_| anyhow::anyhow!("hostbot session is unavailable"))?;
    let sf: SessionFile = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("hostbot session is unreadable or unsupported"))?;
    sf.validate()?;
    anyhow::ensure!(
        sf.platform == platform && sf.route == route && sf.chat == chat,
        "hostbot session identity mismatch"
    );
    Ok(Some((
        sf.messages,
        sf.task_state,
        sf.session_control.unwrap_or_default(),
    )))
}

/// An older host saved every topic under the numeric chat ID. Never copy its history into a
/// new topic or silently widen an unreadable or restricted legacy session.
pub(crate) fn load_lane_session(
    platform: &str,
    route: &str,
    chat: &str,
    legacy_parent: Option<&str>,
) -> Result<Option<(Vec<Message>, crate::agent::todo::TaskState, SessionControl)>> {
    if let Some(parent) = legacy_parent {
        if let Some((_, _, policy)) = load_session_with_state(platform, route, parent)? {
            anyhow::ensure!(
                !policy.is_planning(),
                "legacy chat has an active planning restriction"
            );
        }
    }
    load_session_with_state(platform, route, chat)
}

/// Persist one session after a turn. `chat` is the `Display` form of the platform's chat id.
pub fn save_session(platform: &str, route: &str, chat: &str, messages: &[Message]) -> Result<()> {
    let task_state = crate::agent::todo::durable_snapshot();
    let snapshot = PersistenceSnapshot::capture(&task_state)?;
    save_session_inner(
        platform,
        route,
        chat,
        messages,
        &task_state,
        SaveControl::Snapshot(&snapshot),
    )
}

#[cfg(test)]
pub(crate) fn save_session_with_control(
    platform: &str,
    route: &str,
    chat: &str,
    messages: &[Message],
    task: &crate::agent::todo::TaskState,
    control: &SessionControl,
) -> Result<()> {
    save_session_inner(
        platform,
        route,
        chat,
        messages,
        task,
        SaveControl::Explicit(control),
    )
}

/// Resetting a transcript is not a trusted permission transition: preserve/conflict-check control
/// under the session write lock, without reentering the caller's policy admission context.
pub(crate) fn reset_session_with_control(
    platform: &str,
    route: &str,
    chat: &str,
    control: &SessionControl,
) -> Result<()> {
    save_session_inner(
        platform,
        route,
        chat,
        &[],
        &crate::agent::todo::TaskState::default(),
        SaveControl::Captured(Some(control)),
    )
}

fn save_session_inner(
    platform: &str,
    route: &str,
    chat: &str,
    messages: &[Message],
    task: &crate::agent::todo::TaskState,
    policy: SaveControl<'_>,
) -> Result<()> {
    policy.check(task)?;
    let path = session_path(platform, route, chat);
    let lock_key = format!("{platform}:{route}:{chat}");
    let lock_path = crate::core::workspace_txn::store_lock("hostbot_session", &lock_key);
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    // Startup may have refused this lane's saved state. Validate the existing target under the
    // same write lock so a fresh in-memory lane cannot erase unreadable or future-format data.
    const TARGET_ERROR: &str = "existing hostbot session cannot be safely overwritten";
    let existing_control = match std::fs::read(&path) {
        Ok(bytes) => {
            let existing: SessionFile =
                serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!(TARGET_ERROR))?;
            existing
                .validate()
                .map_err(|_| anyhow::anyhow!(TARGET_ERROR))?;
            anyhow::ensure!(
                existing.platform == platform && existing.route == route && existing.chat == chat,
                TARGET_ERROR
            );
            existing.session_control
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => anyhow::bail!(TARGET_ERROR),
    };
    let control = policy.resolve(existing_control.as_ref())?;
    let sf = SessionFile {
        version: 1,
        session_control: Some(control),
        platform: platform.to_string(),
        route: route.to_string(),
        chat: chat.to_string(),
        messages: messages.to_vec(),
        task_state: task.clone(),
    };
    let json = serde_json::to_string(&sf)?;
    policy.check(task)?;
    crate::core::persist::atomic_write_owner_only(&path, json.as_bytes())?;
    policy.check(task)?;
    crate::agent::todo::mark_saved(&sf.task_state);
    Ok(())
}

/// Delete a validated normal session. A reset is not permission to erase a saved restriction.
/// The reset caller must propagate failure before clearing its in-memory conversation.
pub fn drop_session(platform: &str, route: &str, chat: &str) -> Result<()> {
    reset_saved_session(platform, route, chat, false)
}

fn reset_saved_session(
    platform: &str,
    route: &str,
    chat: &str,
    keep_restriction: bool,
) -> Result<()> {
    let lock_key = format!("{platform}:{route}:{chat}");
    let lock_path = crate::core::workspace_txn::store_lock("hostbot_session", &lock_key);
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let Some((_, _, control)) = load_session_with_state(platform, route, chat)? else {
        return Ok(());
    };
    let path = session_path(platform, route, chat);
    if control.is_planning() {
        anyhow::ensure!(
            keep_restriction,
            "reset refused: saved planning policy cannot be deleted"
        );
        let empty = SessionFile {
            version: 1,
            session_control: Some(control),
            platform: platform.to_owned(),
            route: route.to_owned(),
            chat: chat.to_owned(),
            messages: Vec::new(),
            task_state: crate::agent::todo::TaskState::default(),
        };
        crate::core::persist::atomic_write_owner_only(&path, &serde_json::to_vec(&empty)?)?;
    } else {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Reset saved route conversations after persona changes/removal. Restricted records retain an
/// empty policy-bearing envelope; malformed records are left intact. Normal files are deleted.
/// Best-effort for compatibility with route-management callers, never a permission transition.
pub fn drop_route_sessions(platform: &str, route: &str) {
    let prefix = format!("{}.{}.", sanitize(platform), sanitize(route));
    let dir = sessions_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let Ok(bytes) = crate::core::session_store::read_bounded(&path, 128 * 1024 * 1024) else {
            continue;
        };
        let Ok(session) = serde_json::from_slice::<SessionFile>(&bytes) else {
            continue;
        };
        if session.platform == platform
            && session.route == route
            && session_path(platform, route, &session.chat) == path
        {
            // Re-read and validate under the same lock used by saves before clearing anything.
            let _ = reset_saved_session(platform, route, &session.chat, true);
        }
    }
}

/// Days a session file is kept after its last write before startup GC removes it.
const SESSION_TTL_DAYS: u64 = 30;

/// Remove session files untouched for `SESSION_TTL_DAYS`, returning how many were dropped.
///
/// A long-lived daemon accumulates one file per chat that ever messaged it, forever — each holding
/// conversation text. This bounds both the directory and how long a stranger's message is retained.
/// Best-effort and silent: `mtime` is unavailable on some filesystems, and a daemon must not fail to
/// start over housekeeping. Override the window with `AIZEN_SESSION_TTL_DAYS` (`0` disables GC).
pub fn gc_sessions() -> usize {
    let days = std::env::var("AIZEN_SESSION_TTL_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(SESSION_TTL_DAYS);
    if days == 0 {
        return 0;
    }
    let max_age = std::time::Duration::from_secs(days * 24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return 0;
    };
    let mut dropped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(age) = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
        else {
            continue; // unknown mtime (or a clock skew making it "in the future") ⇒ keep it
        };
        if age <= max_age {
            continue;
        }
        // A lane key is reused on its next message: expiring its restriction would silently grant
        // normal execution. Unknown/corrupt records likewise need explicit repair, not deletion.
        let Ok(bytes) = crate::core::session_store::read_bounded(&path, 128 * 1024 * 1024) else {
            continue;
        };
        let Ok(session) = serde_json::from_slice::<SessionFile>(&bytes) else {
            continue;
        };
        if session.validate().is_err()
            || session
                .session_control
                .as_ref()
                .is_some_and(SessionControl::is_planning)
        {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("store_control_tests.rs");

    #[test]
    fn legacy_desktop_bot_registry_round_trips_to_core_array() {
        for raw in [
            r#"{"bots":{"ops":{"token":"test","allowed_chat_ids":[-12]}}}"#,
            r#"{"ops":{"token":"test","allowed_chat_ids":[-12]}}"#,
        ] {
            let bots = decode_bots(raw).unwrap();
            assert_eq!(bots[0].name, "ops");
            assert_eq!(bots[0].allowed_chat_ids, [-12]);
            let array = serde_json::to_string(&bots).unwrap();
            assert!(array.starts_with('['));
            assert_eq!(decode_bots(&array).unwrap()[0].name, "ops");
        }
        assert!(decode_bots("{broken").is_err());
    }

    #[test]
    fn session_storage_round_trip_preserves_turn_identity_and_projects_input_pixels() {
        let mut message =
            Message::user_with_images("request", vec!["data:image/png;base64,AAAA".into()]);
        message.user_turn_id = Some(crate::core::types::UserTurnId::new().unwrap());
        let id = message.user_turn_id.clone();
        let task = crate::agent::todo::TaskContext::private();
        task.replace(vec![crate::agent::todo::Todo::new(
            "resume this lane only",
            crate::agent::todo::Status::InProgress,
        )])
        .unwrap();
        task.end_turn(true);
        let expected_task = task.snapshot();
        let session = SessionFile {
            version: 1,
            session_control: Some(SessionControl::default()),
            platform: "fixture".into(),
            route: "default".into(),
            chat: "1".into(),
            messages: vec![message],
            task_state: expected_task.clone(),
        };
        let bytes = serde_json::to_vec(&session).unwrap();
        let stored: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            stored["messages"][0]["aizen_user_turn_id"],
            id.as_ref().unwrap().as_str()
        );
        assert!(!String::from_utf8_lossy(&bytes).contains("data:image/png"));
        let loaded: SessionFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.messages[0].user_turn_id, id);
        assert!(loaded.messages[0].images.is_empty());
        assert_eq!(loaded.task_state, expected_task);
        loaded.task_state.validate().unwrap();
        let legacy: SessionFile = serde_json::from_str(
            r#"{"platform":"fixture","route":"default","chat":"1","messages":[{"role":"user","content":"legacy"}]}"#,
        ).unwrap();
        assert!(legacy.messages[0].user_turn_id.is_none());
        assert_eq!(legacy.task_state, crate::agent::todo::TaskState::default());
    }

    #[test]
    fn concurrent_lane_scopes_persist_only_their_own_task() {
        use crate::agent::todo::{self, Status, TaskContext, Todo};
        let (_home_lock, _home) = with_temp_home();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let first = TaskContext::private();
        let second = TaskContext::private();
        runtime.block_on(async {
            let save_lane = |chat: &'static str| async move {
                todo::set(vec![Todo::new(
                    format!("work for {chat}"),
                    Status::InProgress,
                )]);
                tokio::task::yield_now().await;
                todo::end_turn(true);
                save_session("fixture", "shared-route", chat, &[Message::user(chat)]).unwrap();
            };
            tokio::join!(
                todo::with_context(first.clone(), save_lane("first")),
                todo::with_context(second.clone(), save_lane("second")),
            );
        });
        let saved = load_sessions_with_state("fixture");
        assert_eq!(saved.len(), 2);
        for (_, chat, _, state, _) in &saved {
            let expected = if chat == "first" {
                first.snapshot()
            } else {
                second.snapshot()
            };
            assert_eq!(*state, expected);
            assert_eq!(state.items[0].content, format!("work for {chat}"));
            assert!(state.active && state.carry_over);
        }
        first.clear();
        assert!(
            !second.snapshot().items.is_empty(),
            "one lane's /new cannot clear a sibling"
        );

        let path = session_path("fixture", "shared-route", "first");
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        envelope["task_state"]["version"] = serde_json::json!(999);
        std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert_eq!(
            load_sessions_with_state("fixture").len(),
            1,
            "invalid task is not silently dropped from an otherwise loaded lane"
        );
    }

    #[test]
    fn invalid_existing_session_is_not_overwritten_by_a_fresh_lane() {
        use crate::agent::todo::{self, Status, TaskContext, TaskState, Todo};
        let (_home_lock, _home) = with_temp_home();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = TaskContext::private();
        let path = session_path("fixture", "route", "1");
        let valid = SessionFile {
            version: 1,
            session_control: Some(SessionControl::default()),
            platform: "fixture".into(),
            route: "route".into(),
            chat: "1".into(),
            messages: vec![Message::user("existing private conversation")],
            task_state: TaskState::default(),
        };
        let mut future = serde_json::to_value(&valid).unwrap();
        future["task_state"]["version"] = serde_json::json!(999);
        let mut wrong_owner = serde_json::to_value(&valid).unwrap();
        wrong_owner["chat"] = serde_json::json!("other-chat");
        for bytes in [
            b"{malformed private conversation".to_vec(),
            serde_json::to_vec(&future).unwrap(),
            serde_json::to_vec(&wrong_owner).unwrap(),
        ] {
            std::fs::write(&path, &bytes).unwrap();
            runtime.block_on(todo::with_context(owner.clone(), async {
                todo::set(vec![Todo::new("fresh lane task", Status::InProgress)]);
                let error = save_session("fixture", "route", "1", &[Message::user("new turn")])
                    .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "existing hostbot session cannot be safely overwritten"
                );
                assert!(
                    owner.is_dirty(),
                    "failed persistence must not acknowledge the new task"
                );
            }));
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "rejected target must remain byte-identical"
            );
        }
    }

    /// Pin `AIZEN_HOME` to a fresh tempdir so the store reads/writes there, not the real `~/.aizen`.
    /// Returns the guard dir (kept alive for the test's lifetime). Serialized via the SHARED
    /// `TEST_HOME_LOCK` — the whole crate uses it to keep `HOME`-mutating tests from racing. We set
    /// `AIZEN_HOME` (not `AIZEN_HOME`) to match every other test: `aizen_home()` gives `AIZEN_HOME`
    /// HIGHER precedence, so a leaked `AIZEN_HOME` would override the `AIZEN_HOME` those tests set and
    /// break them; `AIZEN_HOME` is the shared convention each test overwrites at its own start.
    fn with_temp_home() -> (std::sync::MutexGuard<'static, ()>, tempdir_guard::TempDir) {
        let guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempdir_guard::TempDir::new();
        std::env::set_var("AIZEN_HOME", dir.path());
        (guard, dir)
    }

    #[test]
    fn hosted_bots_round_trip() {
        let (_g, _home) = with_temp_home();
        assert!(load_bots().is_empty(), "fresh store has no bots");
        let bots = vec![
            HostedBot {
                name: "work".into(),
                token: Some("t1".into()),
                allowed_chat_ids: vec![],
                persona: None,
                own_owner: false,
                host: None,
            },
            HostedBot {
                name: "ops".into(),
                token: Some("t2".into()),
                allowed_chat_ids: vec![7, 9],
                persona: Some("Aria".into()),
                own_owner: true,
                host: Some("vps-2".into()),
            },
        ];
        save_bots(&bots).unwrap();
        let round = load_bots();
        assert_eq!(round.len(), 2);
        assert_eq!(round[0].name, "work");
        assert_eq!(round[0].persona, None);
        assert!(!round[0].own_owner);
        assert_eq!(round[0].host, None);
        assert_eq!(round[1].allowed_chat_ids, vec![7, 9]);
        assert_eq!(round[1].persona.as_deref(), Some("Aria"));
        assert!(round[1].own_owner, "own_owner survives a round trip");
        assert_eq!(round[1].host.as_deref(), Some("vps-2"));
    }

    #[test]
    fn a_bots_file_written_before_these_fields_existed_still_loads() {
        // Back-compat is the whole reason both new fields are `#[serde(default)]`: an install from
        // before this change must keep hosting its bots, with the old inherit-the-owner behaviour.
        let (_g, _home) = with_temp_home();
        let legacy = r#"[{"name":"work","token":"t1","allowed_chat_ids":[5]}]"#;
        std::fs::write(bots_path(), legacy).unwrap();
        let round = load_bots();
        assert_eq!(round.len(), 1, "legacy file parses");
        assert_eq!(round[0].name, "work");
        assert!(
            !round[0].own_owner,
            "absent own_owner means inherit, the pre-existing behaviour"
        );
        assert_eq!(
            round[0].host, None,
            "absent host means any machine may run it"
        );
    }

    #[test]
    fn a_lane_with_no_entry_inherits_everything() {
        // No lanes.json (every install before this feature) ⇒ all-None ⇒ every command falls back to
        // the process-wide config exactly as it did when these wrote `cli-config.json`.
        let (_g, _home) = with_temp_home();
        let lane = load_lane("default");
        assert_eq!(lane.route, "default");
        assert_eq!(lane.cwd, None);
        assert_eq!(lane.model, None);
        assert_eq!(lane.effort, None);
        assert_eq!(lane.approval, None);
        assert_eq!(lane.ultimate, None);
    }

    #[test]
    fn lane_settings_are_per_route_and_do_not_bleed() {
        // The bug this prevents: bot A's `/cd` moving bot B's working directory, which is what a
        // process-wide `set_current_dir` did.
        let (_g, _home) = with_temp_home();
        update_lane("default", |l| {
            l.cwd = Some(PathBuf::from("/srv/projA"));
            l.model = Some("model-a".into());
            Ok(())
        })
        .unwrap();
        update_lane("work", |l| {
            l.cwd = Some(PathBuf::from("/srv/projB"));
            Ok(())
        })
        .unwrap();

        assert_eq!(load_lane("default").cwd, Some(PathBuf::from("/srv/projA")));
        assert_eq!(load_lane("work").cwd, Some(PathBuf::from("/srv/projB")));
        assert_eq!(
            load_lane("work").model,
            None,
            "a model pinned on `default` must NOT apply to `work`"
        );
        assert_eq!(load_lanes().len(), 2);
    }

    #[test]
    fn update_lane_edits_in_place_without_dropping_siblings() {
        let (_g, _home) = with_temp_home();
        update_lane("a", |l| {
            l.model = Some("m1".into());
            Ok(())
        })
        .unwrap();
        update_lane("b", |l| {
            l.model = Some("m2".into());
            Ok(())
        })
        .unwrap();
        update_lane("a", |l| {
            l.effort = Some("max".into());
            Ok(())
        })
        .unwrap();

        let a = load_lane("a");
        assert_eq!(a.model.as_deref(), Some("m1"), "existing field preserved");
        assert_eq!(a.effort.as_deref(), Some("max"), "new field added");
        assert_eq!(
            load_lane("b").model.as_deref(),
            Some("m2"),
            "sibling intact"
        );
    }

    #[test]
    fn drop_lane_removes_only_that_route() {
        let (_g, _home) = with_temp_home();
        update_lane("keep", |l| {
            l.model = Some("m".into());
            Ok(())
        })
        .unwrap();
        update_lane("gone", |l| {
            l.model = Some("m".into());
            Ok(())
        })
        .unwrap();
        drop_lane("gone");
        let lanes = load_lanes();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].route, "keep");
    }

    #[test]
    fn gc_keeps_fresh_sessions_and_can_be_disabled() {
        // Files just written are far inside any retention window, so GC must not touch them — an
        // over-eager sweep here silently erases live conversations.
        let (_g, _home) = with_temp_home();
        let msgs = vec![Message::user("hi")];
        save_session("telegram", "default", "1", &msgs).unwrap();
        save_session("telegram", "work", "2", &msgs).unwrap();
        assert_eq!(gc_sessions(), 0, "fresh files survive");
        assert_eq!(load_sessions("telegram").len(), 2);

        // `0` disables GC entirely, for an operator who wants unbounded history.
        std::env::set_var("AIZEN_SESSION_TTL_DAYS", "0");
        assert_eq!(gc_sessions(), 0);
        std::env::remove_var("AIZEN_SESSION_TTL_DAYS");
        assert_eq!(
            load_sessions("telegram").len(),
            2,
            "nothing was dropped either way"
        );
    }

    #[test]
    fn gc_drops_sessions_past_the_window() {
        // A daemon running for months otherwise keeps one file per chat that ever messaged it,
        // forever — each holding conversation text.
        let (_g, _home) = with_temp_home();
        save_session("telegram", "old", "9", &[Message::user("ancient")]).unwrap();
        assert_eq!(load_sessions("telegram").len(), 1);
        // A one-day window plus a backdated mtime is the portable way to test this without waiting.
        std::env::set_var("AIZEN_SESSION_TTL_DAYS", "1");
        let path = session_path("telegram", "old", "9");
        let two_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(two_days_ago).unwrap();
        drop(f);

        assert_eq!(gc_sessions(), 1, "the stale file was dropped");
        assert!(load_sessions("telegram").is_empty());
        std::env::remove_var("AIZEN_SESSION_TTL_DAYS");
    }

    #[test]
    fn session_save_load_drop() {
        let (_g, _home) = with_temp_home();
        let msgs = vec![Message::user("hello"), Message::assistant("hi")];
        save_session("telegram", "default", "42", &msgs).unwrap();
        save_session("telegram", "work", "-100", &msgs).unwrap();
        save_session("discord", "default", "999", &msgs).unwrap();

        let tg = load_sessions("telegram");
        assert_eq!(tg.len(), 2, "only telegram's two sessions, not discord's");
        assert!(tg.iter().any(|(r, c, _)| r == "default" && c == "42"));
        assert!(tg.iter().any(|(r, c, _)| r == "work" && c == "-100"));

        drop_session("telegram", "default", "42").unwrap();
        let tg = load_sessions("telegram");
        assert_eq!(tg.len(), 1, "dropped one → one telegram session left");
        assert_eq!(tg[0].0, "work");

        drop_route_sessions("telegram", "work");
        assert!(
            load_sessions("telegram").is_empty(),
            "route drop removed the last telegram session"
        );
        assert_eq!(
            load_sessions("discord").len(),
            1,
            "discord session untouched"
        );
    }

    #[test]
    fn restored_topics_keep_their_own_history_and_respect_legacy_policy() {
        let (_lock, _home) = with_temp_home();
        for key in ["-55", "-55_topic_1", "-55_topic_2"] {
            save_session("telegram", "default", key, &[Message::user(key)]).unwrap();
        }
        for key in ["-55_topic_1", "-55_topic_2"] {
            let (rows, _, _) = load_lane_session("telegram", "default", key, Some("-55"))
                .unwrap()
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].content.as_deref(), Some(key));
        }
        let old = session_path("telegram", "default", "-55");
        let mut record: SessionFile =
            serde_json::from_slice(&std::fs::read(&old).unwrap()).unwrap();
        record.session_control.as_mut().unwrap().mode =
            crate::core::session_control::ExecutionMode::Planning;
        std::fs::write(&old, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(load_lane_session("telegram", "default", "-55_topic_1", Some("-55")).is_err());
        std::fs::write(&old, b"invalid").unwrap();
        assert!(load_lane_session("telegram", "default", "-55_topic_3", Some("-55")).is_err());
    }

    #[test]
    fn sanitize_blocks_path_escape() {
        assert_eq!(sanitize("../etc"), "___etc");
        assert_eq!(sanitize("a.b/c"), "a_b_c");
        assert_eq!(sanitize("-100"), "-100"); // a negative chat id survives intact
        assert_eq!(sanitize("work_bot"), "work_bot");
    }
}

/// A tiny self-contained tempdir for tests (no `tempfile` dep — keeps the pure-Rust posture). Creates
/// a uniquely-named dir under the OS temp dir and removes it on drop.
#[cfg(test)]
mod tempdir_guard {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "aizen-hostbot-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("create tempdir");
            TempDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
