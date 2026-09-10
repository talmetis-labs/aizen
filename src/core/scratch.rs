//! Per-run scratch directory — the one place the model is TOLD to put throwaway files.
//!
//! Before this existed the system prompt said "use a temp dir" without naming one, so helper
//! scripts, probes and notes landed in the repo or the user's cwd and stayed there (`scan_junk.ps1`
//! beside the user's home directory was a real casualty). The fix has two halves: a concrete path
//! advertised in `<environment>` (this module), and a sweeper so abandoned runs cost nothing.
//!
//! The directory is per-RUN (`{unix}-{pid}` under the OS temp dir), not per-session-slug: two
//! windows never collide, and the name needs no sanitizing. It is created lazily on first use —
//! a run that never asks for it never touches the disk.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Anything under `aizen-scratch/` older than this is an abandoned run's leavings. Matches the
/// recovery lease TTL: long enough that a file referenced across a multi-day task survives, short
/// enough that the temp dir never becomes an archive.
const STALE_SECS: u64 = 7 * 24 * 60 * 60;

fn scratch_root() -> PathBuf {
    std::env::temp_dir().join("aizen-scratch")
}

/// This run's scratch directory, created on first call. Falls back to the OS temp dir itself when
/// creation fails (read-only temp is rare but real) — the advertised path must always exist,
/// because the model will write to it on the strength of the prompt alone.
pub fn dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mine = scratch_root().join(format!("{unix}-{}", std::process::id()));
        match std::fs::create_dir_all(&mine) {
            Ok(()) => mine,
            Err(_) => std::env::temp_dir(),
        }
    })
}

/// Remove abandoned run directories. Best-effort, age-gated so a live sibling window (whose dir is
/// by definition younger than the TTL unless the run truly is a week old and still active — in
/// which case its next write recreates what it needs) is never yanked mid-task.
pub fn sweep_stale() {
    let Ok(rd) = std::fs::read_dir(scratch_root()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let stale = entry
            .metadata()
            .and_then(|md| md.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() > STALE_SECS);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_exists_and_is_stable() {
        let a = dir();
        assert!(a.exists(), "advertised scratch path must exist");
        assert_eq!(a, dir(), "same path for the whole run");
    }
}
