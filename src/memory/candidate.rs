//! The inferred→durable **ratchet** ledger — the fix for the dead park-in-RAM path.
//!
//! # Why this exists
//!
//! Before this module, an INFERRED fact routed `Store`/`Review` was parked in
//! [`crate::memory::session_mem`] (an in-process `Vec`) and **lost on process exit** — the only
//! promotion path (`session_mem::candidates_for_promote`) had zero callers. So a preference the
//! model quietly inferred never survived a restart, and the whole `decay`/`caps` machinery (which
//! only acts on `source==Inferred` durable rows) ran on an empty population.
//!
//! This ledger makes inference *stick when it recurs*. Each inferred candidate is journaled to its
//! own `*.md` under `cli-memory/candidates/`, keyed deterministically by `(scope, normalized body)`
//! so re-noting the same fact in a LATER session finds the same file and bumps a **distinct-session**
//! counter. Once a candidate has recurred across [`config::ratchet_promote_sessions`] separate
//! sessions it is *ripe*: the caller promotes it into the live entry store (via the normal
//! `apply_store` consolidation path) and [`remove`]s its ledger file. A one-off phrasing that never
//! recurs simply ages out under the ledger cap — it never pollutes the durable store.
//!
//! Design posture mirrors the rest of the brain: best-effort (an I/O error never breaks a turn),
//! atomic writes, a hard cap so the ledger stays bounded for years, and a single env kill-switch
//! ([`config::ratchet_disabled`]) that restores the old RAM-only behavior.

use crate::core::config;
use crate::memory::frontmatter;
use crate::memory::store::{self, MemoryType};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// Hard cap on journaled candidates. Over this, the lowest-recurrence / oldest are evicted on write
/// so a chatty install can't grow the ledger without bound. Generous — a candidate is small and
/// most are promoted or age out quickly.
const LEDGER_CAP: usize = 400;

const KEY_ORDER: &[&str] = &[
    "name", "type", "scope", "subpath", "confidence", "sessions", "lastSession", "created", "updated",
];

/// One journaled inferred candidate (parsed from its ledger file).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub body: String,
    pub mtype: MemoryType,
    pub scope: Option<String>,
    pub subpath: Option<String>,
    pub confidence: f64,
    /// Distinct sessions this candidate has been observed in.
    pub sessions: u32,
    pub last_session: String,
    pub created: Option<String>,
    pub mtime_ms: u128,
}

/// What [`record`] did, so the caller knows whether to promote now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub id: String,
    /// Distinct-session count AFTER this observation.
    pub sessions: u32,
    /// True once `sessions >= config::ratchet_promote_sessions()` — the caller should durably store
    /// the fact and call [`remove`] on this id.
    pub ripe: bool,
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Whitespace-collapsed, lowercased body — the dedup key so trivial re-spacing/casing of the same
/// inferred fact lands on the same ledger file.
fn normalize(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Deterministic ledger id for `(scope, body)`: a readable slug of the name plus a hash of the
/// normalized `(scope, body)` so re-noting the same candidate in a later session hits the SAME file
/// (that is what accumulates the distinct-session count), while two genuinely different facts with
/// the same name never collide.
fn ledger_id(name: &str, scope: Option<&str>, body: &str) -> String {
    let key = format!("{}\u{1f}{}", scope.unwrap_or(""), normalize(body));
    format!("{}-{:08x}", store::slugify(name), fnv1a64(&key) as u32)
}

fn scope_field(scope: Option<&str>) -> Option<String> {
    scope
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("global"))
        .map(str::to_string)
}

fn from_file(path: &Path) -> Option<Candidate> {
    let raw = std::fs::read_to_string(path).ok()?;
    let fm = frontmatter::parse(&raw);
    let id = path.file_stem().and_then(|s| s.to_str())?.to_lowercase();
    let name = fm.get("name").map(str::to_string).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| id.clone());
    let mtype = MemoryType::parse(fm.get("type").unwrap_or("user"));
    let scope = fm.get("scope").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let subpath = fm.get("subpath").map(str::trim).filter(|s| !s.is_empty()).map(|s| s.replace('\\', "/"));
    let confidence = fm.get("confidence").and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.5).clamp(0.0, 1.0);
    let sessions = fm.get("sessions").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
    let last_session = fm.get("lastSession").unwrap_or("").trim().to_string();
    let created = fm.get("created").map(str::to_string).filter(|s| !s.trim().is_empty());
    let mtime_ms = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Some(Candidate {
        id,
        name,
        body: fm.body,
        mtype,
        scope,
        subpath,
        confidence,
        sessions,
        last_session,
        created,
        mtime_ms,
    })
}

/// Every journaled candidate (missing dir → empty; never errors).
pub fn list() -> Vec<Candidate> {
    let dir = config::candidates_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("md")) != Some(true) {
            continue;
        }
        if let Some(c) = from_file(&p) {
            out.push(c);
        }
    }
    out
}

fn render(c: &Candidate) -> String {
    let mut fields = BTreeMap::new();
    fields.insert("name".to_string(), c.name.trim().to_string());
    fields.insert("type".to_string(), c.mtype.as_str().to_string());
    if let Some(s) = c.scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        fields.insert("scope".to_string(), s.to_string());
    }
    if let Some(sp) = c.subpath.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        fields.insert("subpath".to_string(), sp.to_string());
    }
    fields.insert("confidence".to_string(), format!("{:.2}", c.confidence.clamp(0.0, 1.0)));
    fields.insert("sessions".to_string(), c.sessions.to_string());
    if !c.last_session.trim().is_empty() {
        fields.insert("lastSession".to_string(), c.last_session.trim().to_string());
    }
    fields.insert("created".to_string(), c.created.clone().unwrap_or_else(today));
    fields.insert("updated".to_string(), today());
    frontmatter::serialize(&fields, &c.body, KEY_ORDER)
}

/// Journal one observation of an inferred candidate. If a ledger file already exists for this
/// `(scope, body)`, bump its **distinct-session** count when `session_id` differs from the last
/// session that touched it (a candidate re-stated many times in ONE session stays at one session).
/// Returns [`Recorded`] with the post-observation session count and whether it is now ripe for
/// promotion. Best-effort — a write failure surfaces as `Err` and the caller ignores it.
pub fn record(
    name: &str,
    body: &str,
    mtype: MemoryType,
    scope: Option<&str>,
    subpath: Option<&str>,
    confidence: f64,
    session_id: &str,
) -> Result<Recorded> {
    let body_trim = body.trim();
    let dir = config::candidates_dir();
    std::fs::create_dir_all(&dir)?;
    let id = ledger_id(name, scope, body_trim);
    let path = dir.join(format!("{id}.md"));

    let (sessions, created) = if let Some(existing) = from_file(&path) {
        // Distinct-session count: only a NEW session id advances the ratchet.
        let sessions = if existing.last_session == session_id {
            existing.sessions.max(1)
        } else {
            existing.sessions.saturating_add(1).max(1)
        };
        (sessions, existing.created.clone())
    } else {
        (1, Some(today()))
    };

    let cand = Candidate {
        id: id.clone(),
        name: name.trim().to_string(),
        body: body_trim.to_string(),
        mtype,
        scope: scope_field(scope),
        subpath: subpath.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.replace('\\', "/")),
        confidence,
        sessions,
        last_session: session_id.to_string(),
        created,
        mtime_ms: 0,
    };
    store::write_atomic(&path, &render(&cand))?;
    enforce_cap(&dir);

    let ripe = sessions >= config::ratchet_promote_sessions();
    Ok(Recorded { id, sessions, ripe })
}

/// Remove a candidate's ledger file (after it has been promoted to the durable store, or pruned).
/// Idempotent — a missing file is fine.
pub fn remove(id: &str) -> Result<()> {
    let path = config::candidates_dir().join(format!("{}.md", id));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Keep the ledger under [`LEDGER_CAP`] by evicting the lowest-recurrence, then oldest, candidates.
/// Best-effort maintenance — a failure here never breaks a learn.
fn enforce_cap(dir: &Path) {
    let mut all = list();
    if all.len() <= LEDGER_CAP {
        return;
    }
    // Keep the strongest: most distinct sessions first, then most recently touched.
    all.sort_by(|a, b| b.sessions.cmp(&a.sessions).then(b.mtime_ms.cmp(&a.mtime_ms)));
    for victim in all.into_iter().skip(LEDGER_CAP) {
        let _ = std::fs::remove_file(dir.join(format!("{}.md", victim.id)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-cand-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::remove_var("AIZEN_RATCHET_SESSIONS");
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn first_note_is_one_session_not_ripe() {
        with_home("first", || {
            let r = record("prefer pnpm", "the user prefers pnpm over npm", MemoryType::User, None, None, 0.8, "s1").unwrap();
            assert_eq!(r.sessions, 1);
            assert!(!r.ripe, "a single mention must not promote");
            assert_eq!(list().len(), 1);
        });
    }

    #[test]
    fn same_session_restate_does_not_advance() {
        with_home("same-sess", || {
            record("prefer pnpm", "prefers pnpm over npm", MemoryType::User, None, None, 0.8, "s1").unwrap();
            let r2 = record("prefer pnpm", "prefers pnpm over npm", MemoryType::User, None, None, 0.8, "s1").unwrap();
            assert_eq!(r2.sessions, 1, "restating in the SAME session stays at one session");
            assert!(!r2.ripe);
            assert_eq!(list().len(), 1, "same (scope,body) reuses one ledger file");
        });
    }

    #[test]
    fn recurring_across_sessions_becomes_ripe() {
        with_home("recur", || {
            let r1 = record("prefer pnpm", "prefers pnpm over npm", MemoryType::User, None, None, 0.8, "s1").unwrap();
            assert!(!r1.ripe);
            // A LATER session re-observing the same fact advances the ratchet to the default threshold (2).
            let r2 = record("prefer pnpm", "prefers pnpm over npm", MemoryType::User, None, None, 0.8, "s2").unwrap();
            assert_eq!(r2.sessions, 2);
            assert!(r2.ripe, "recurrence across 2 sessions ripens for promotion");
        });
    }

    #[test]
    fn threshold_is_configurable() {
        with_home("threshold", || {
            std::env::set_var("AIZEN_RATCHET_SESSIONS", "3");
            record("x", "some inferred fact body", MemoryType::User, None, None, 0.8, "s1").unwrap();
            let r2 = record("x", "some inferred fact body", MemoryType::User, None, None, 0.8, "s2").unwrap();
            assert!(!r2.ripe, "still below the 3-session threshold");
            let r3 = record("x", "some inferred fact body", MemoryType::User, None, None, 0.8, "s3").unwrap();
            assert!(r3.ripe, "third distinct session ripens");
            std::env::remove_var("AIZEN_RATCHET_SESSIONS");
        });
    }

    #[test]
    fn different_scope_is_a_separate_candidate() {
        with_home("scope", || {
            record("deploy note", "deploy uses fly", MemoryType::Project, Some("proj-a"), None, 0.8, "s1").unwrap();
            record("deploy note", "deploy uses fly", MemoryType::Project, Some("proj-b"), None, 0.8, "s1").unwrap();
            assert_eq!(list().len(), 2, "same body in different zones are distinct candidates");
        });
    }

    #[test]
    fn remove_deletes_the_ledger_file() {
        with_home("remove", || {
            let r = record("x", "a fact to promote", MemoryType::User, None, None, 0.8, "s1").unwrap();
            assert_eq!(list().len(), 1);
            remove(&r.id).unwrap();
            assert!(list().is_empty(), "promoted candidate's ledger file is gone");
            remove(&r.id).unwrap(); // idempotent
        });
    }

    #[test]
    fn cap_evicts_lowest_recurrence() {
        with_home("cap", || {
            // One high-recurrence candidate we expect to survive.
            record("keep", "the durable recurring fact", MemoryType::User, None, None, 0.8, "s1").unwrap();
            record("keep", "the durable recurring fact", MemoryType::User, None, None, 0.8, "s2").unwrap();
            for i in 0..(LEDGER_CAP + 20) {
                record(&format!("f{i}"), &format!("one-off fact number {i}"), MemoryType::User, None, None, 0.6, "s1").unwrap();
            }
            assert!(list().len() <= LEDGER_CAP, "ledger stays under cap");
            assert!(list().iter().any(|c| c.body.contains("durable recurring")), "the high-recurrence candidate survives");
        });
    }
}
