//! Process scratch namespace. `planned_dir` is pure; legacy `dir` creates lazily and keeps its
//! historical temp fallback. Purpose-managed placement never calls that fallback constructor.
//!
//! Cleanup is conservative: only marked legacy process directories are eligible, and any process
//! directory containing a managed namespace is retained. Managed lifetime locks are held by the
//! placement context; abandoned managed cleanup is deferred rather than risk deleting live output.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const STALE_SECS: u64 = 7 * 24 * 60 * 60;
const OWNER_BYTES: &[u8] = b"aizen-owned-process-scratch-v1\n";
pub(crate) const OWNER_MARKER: &str = ".aizen-scratch-owner";
const MAX_SWEEP_ENTRIES: usize = 8192;
const MAX_SWEEP_DEPTH: usize = 32;

fn scratch_root() -> PathBuf {
    std::env::temp_dir().join("aizen-scratch")
}

pub(crate) fn base36(mut value: u128) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut encoded = String::new();
    loop {
        encoded.push(DIGITS[(value % 36) as usize] as char);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    encoded.chars().rev().collect()
}

fn process_path(root: &Path, nanos: u128, pid: u32) -> PathBuf {
    // Encode the full timestamp and PID, not a truncated prefix or a rounded timestamp.
    root.join(format!("{}-{}", base36(nanos), base36(u128::from(pid))))
}

/// The process namespace, stable and read-only even when unavailable. No mkdir, cleanup, or fallback.
pub fn planned_dir() -> &'static Path {
    static PLANNED: OnceLock<PathBuf> = OnceLock::new();
    PLANNED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        process_path(&scratch_root(), nanos, std::process::id())
    })
}

/// Compatibility constructor for older prompt/shell surfaces, not purpose-managed native writes.
pub fn dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let mine = planned_dir().to_path_buf();
        match fs::create_dir_all(&mine)
            .and_then(|_| mark_owned(&mine).map_err(std::io::Error::other))
        {
            Ok(()) => mine,
            Err(_) => std::env::temp_dir(),
        }
    })
}

/// Called only after checked, approved directory creation, never by preview/reporting.
pub(crate) fn mark_owned(root: &Path) -> Result<()> {
    let md = fs::symlink_metadata(root)?;
    if !md.is_dir() || crate::core::file_placement::is_link_or_reparse(&md) {
        bail!(
            "scratch process namespace is not a plain directory: {}",
            root.display()
        );
    }
    let path = root.join(OWNER_MARKER);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(OWNER_BYTES)
                .context("writing scratch ownership marker")?;
            file.sync_all()
                .context("persisting scratch ownership marker")?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if !valid_marker(root) {
                bail!("invalid scratch ownership marker: {}", path.display());
            }
        }
        Err(error) => return Err(error).context("creating scratch ownership marker"),
    }
    Ok(())
}

fn valid_marker(root: &Path) -> bool {
    let path = root.join(OWNER_MARKER);
    let Ok(md) = fs::symlink_metadata(&path) else {
        return false;
    };
    if !md.is_file()
        || crate::core::file_placement::is_link_or_reparse(&md)
        || md.len() != OWNER_BYTES.len() as u64
    {
        return false;
    }
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(OWNER_BYTES.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes == OWNER_BYTES
}

/// Best-effort marked-legacy cleanup. Managed namespaces (live OR abandoned) are not deleted in
/// this version. Unmarked old scratch directories are also retained rather than assuming ownership.
pub fn sweep_stale() {
    sweep_stale_at(&scratch_root(), std::time::SystemTime::now());
}

fn sweep_stale_at(root: &Path, now: std::time::SystemTime) {
    let Ok(md) = fs::symlink_metadata(root) else {
        return;
    };
    if !md.is_dir() || crate::core::file_placement::is_link_or_reparse(&md) {
        return;
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten().take(MAX_SWEEP_ENTRIES) {
        let path = entry.path();
        // The process that owns this exact path may be older than the TTL but is still live.
        if path == planned_dir() {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.is_dir()
            || crate::core::file_placement::is_link_or_reparse(&md)
            || !valid_marker(&path)
        {
            continue;
        }
        // Any existing managed entry, including an invalid link, makes cleanup refuse this root.
        match fs::symlink_metadata(path.join("managed")) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            _ => continue,
        }
        let stale = md
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age.as_secs() > STALE_SECS);
        if stale {
            let _ = remove_owned_tree(&path);
        }
    }
}

fn remove_owned_tree(root: &Path) -> Result<()> {
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut directories = Vec::new();
    let mut files = Vec::new();
    while let Some((path, depth)) = pending.pop() {
        if depth > MAX_SWEEP_DEPTH || files.len() + directories.len() >= MAX_SWEEP_ENTRIES {
            bail!("scratch cleanup tree exceeds safety bound");
        }
        let md = fs::symlink_metadata(&path)?;
        if crate::core::file_placement::is_link_or_reparse(&md) {
            bail!("scratch cleanup refuses links/reparse points");
        }
        if md.is_dir() {
            directories.push(path.clone());
            for entry in fs::read_dir(path)? {
                if pending.len() + files.len() + directories.len() >= MAX_SWEEP_ENTRIES {
                    bail!("scratch cleanup tree exceeds safety bound");
                }
                pending.push((entry?.path(), depth + 1));
            }
        } else if md.is_file() {
            files.push(path);
        } else {
            bail!("scratch cleanup refuses special files");
        }
    }
    // Recheck instead of recursive deletion: remove_file/remove_dir do not traverse a replaced link.
    // Cross-platform path rechecks still are not kernel-atomic protection against a hostile racer.
    if fs::symlink_metadata(root.join("managed")).is_ok() {
        bail!("managed scratch is retained");
    }
    for path in files {
        let md = fs::symlink_metadata(&path)?;
        if !md.is_file() || crate::core::file_placement::is_link_or_reparse(&md) {
            bail!("scratch file changed before cleanup");
        }
        fs::remove_file(path)?;
    }
    for path in directories.into_iter().rev() {
        let md = fs::symlink_metadata(&path)?;
        if !md.is_dir() || crate::core::file_placement::is_link_or_reparse(&md) {
            bail!("scratch directory changed before cleanup");
        }
        fs::remove_dir(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base36_is_canonical_and_lossless() {
        for (value, expected) in [
            (0, "0"),
            (1, "1"),
            (35, "z"),
            (36, "10"),
            (1295, "zz"),
            (1296, "100"),
            (u128::from(u32::MAX), "1z141z3"),
            (u128::MAX, "f5lxx1zz5pnorynqglhzmsp33"),
        ] {
            let encoded = base36(value);
            assert_eq!(encoded, expected);
            assert_eq!(u128::from_str_radix(&encoded, 36).unwrap(), value);
            assert!(encoded
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()));
            assert!(encoded == "0" || !encoded.starts_with('0'));
            assert!(encoded.len() <= 25);
        }
    }

    #[test]
    fn compact_process_names_preserve_timestamp_and_pid() {
        let root = Path::new("aizen-scratch");
        for (nanos, pid, expected) in [
            (1_791_828_265_005_150_400, 20376, "dm323x5x8o1s-fq0"),
            (1_791_866_255_087_863_200, 13816, "dm3fkpkfm2lc-ans"),
        ] {
            let path = process_path(root, nanos, pid);
            let name = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(name, expected);
            assert_eq!(name.len(), 16);
            assert_eq!(format!("{nanos}-{pid}").len(), 25);
            let (time, process) = name.split_once('-').unwrap();
            assert_eq!(u128::from_str_radix(time, 36).unwrap(), nanos);
            assert_eq!(u32::from_str_radix(process, 36).unwrap(), pid);
            assert!(name
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-'));
        }
        let mut names = std::collections::HashSet::new();
        for nanos in [0, 1, 35, 36, 1295, 1296, u128::MAX] {
            for pid in [0, 1, 35, 36, u32::MAX] {
                let path = process_path(root, nanos, pid);
                let name = path.file_name().unwrap().to_str().unwrap();
                assert!(name.len() <= 33);
                assert_eq!(name.bytes().filter(|&b| b == b'-').count(), 1);
                assert!(names.insert(path));
            }
        }
        assert_eq!(process_path(root, 0, 0), root.join("0-0"));
        assert_eq!(
            process_path(root, u128::MAX, u32::MAX)
                .file_name()
                .unwrap()
                .len(),
            33
        );
    }

    #[test]
    fn planning_compact_process_path_does_not_create_directories() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let root = fixture.path().join("aizen-scratch");
        let path = process_path(&root, 1_791_828_265_005_150_400, 20376);
        assert!(path.is_absolute());
        assert_eq!(path.parent(), Some(root.as_path()));
        assert_eq!(path, process_path(&root, 1_791_828_265_005_150_400, 20376));
        assert!(!root.exists());
        assert!(!path.exists());
    }

    #[test]
    fn planned_process_path_is_stable_without_creation() {
        assert_eq!(planned_dir(), planned_dir());
        assert!(planned_dir().is_absolute());
        assert_eq!(planned_dir().parent(), Some(scratch_root().as_path()));
        let name = planned_dir().file_name().unwrap().to_str().unwrap();
        let (time, process) = name.split_once('-').unwrap();
        let nanos = u128::from_str_radix(time, 36).unwrap();
        assert_eq!(
            u32::from_str_radix(process, 36).unwrap(),
            std::process::id()
        );
        assert_eq!(
            planned_dir(),
            process_path(&scratch_root(), nanos, std::process::id())
        );
        assert!(name.len() <= 33);
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-'));
    }

    #[test]
    fn sweeper_removes_only_marked_legacy_roots_in_both_name_formats() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        for (index, name) in ["1791828265005150400-20376", "dm323x5x8o1s-fq0"]
            .iter()
            .enumerate()
        {
            let group = fixture.path().join(index.to_string());
            fs::create_dir(&group).unwrap();
            for kind in ["legacy", "managed", "unmarked"] {
                let parent = group.join(kind);
                fs::create_dir(&parent).unwrap();
                let root = parent.join(name);
                fs::create_dir(&root).unwrap();
                fs::write(root.join("output.txt"), "fixture").unwrap();
                if kind != "unmarked" {
                    mark_owned(&root).unwrap();
                }
                if kind == "managed" {
                    fs::create_dir(root.join("managed")).unwrap();
                }
                let future =
                    std::time::SystemTime::now() + std::time::Duration::from_secs(STALE_SECS + 60);
                sweep_stale_at(&parent, future);
                if kind == "legacy" {
                    assert!(!root.exists());
                } else {
                    assert!(root.join("output.txt").exists());
                    if kind == "managed" {
                        assert!(root.join(OWNER_MARKER).exists());
                        assert!(root.join("managed").exists());
                    }
                }
            }
        }
    }

    #[test]
    fn marker_and_managed_scratch_are_retained_by_sweeper() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let managed = fixture.path().join("managed-run");
        fs::create_dir(&managed).unwrap();
        mark_owned(&managed).unwrap();
        fs::create_dir(managed.join("managed")).unwrap();
        let unmarked = fixture.path().join("unmarked");
        fs::create_dir(&unmarked).unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(STALE_SECS + 60);
        sweep_stale_at(fixture.path(), future);
        assert!(managed.exists());
        assert!(unmarked.exists());
    }

    #[cfg(unix)]
    #[test]
    fn sweeper_refuses_linked_descendants() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let outside = crate::core::file_placement::tests::TestDir::new();
        fs::write(outside.path().join("keep"), "safe").unwrap();
        for name in ["1791828265005150400-20376", "dm323x5x8o1s-fq0"] {
            let root = fixture.path().join(name);
            fs::create_dir(&root).unwrap();
            mark_owned(&root).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        }
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(STALE_SECS + 60);
        sweep_stale_at(fixture.path(), future);
        for name in ["1791828265005150400-20376", "dm323x5x8o1s-fq0"] {
            assert!(fixture.path().join(name).exists());
        }
        assert!(outside.path().join("keep").exists());
    }
}
