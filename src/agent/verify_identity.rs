//! Resource-bounded local drift detection, not a snapshot or complete build-input provenance.
//!
//! Elapsed-time checks are cooperative: blocking filesystem calls have no hard deadline.
//! Output/dependency stores are explicitly excluded. Never use this identity to authorize
//! shell/editor overlap or reuse a build: external inputs, environment and toolchains are absent.

use crate::core::file_placement::{is_link_or_reparse, NodeStamp};
use crate::core::persist::FileFingerprint;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    file_bytes: u64,
    total_bytes: u64,
    depth: usize,
    duration: Duration,
}

const LIMITS: Limits = Limits {
    entries: 20_000,
    file_bytes: 16 * 1024 * 1024,
    total_bytes: 256 * 1024 * 1024,
    depth: 32,
    duration: Duration::from_secs(3),
};

struct Entry {
    path: PathBuf,
    name: String,
    stamp: NodeStamp,
    directory: bool,
    len: u64,
    mode: u32,
}

fn mode(md: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        u32::from(md.permissions().readonly())
    }
}

fn excluded(relative: &Path) -> bool {
    let first = relative
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str());
    if matches!(
        first,
        Some(".git" | "target" | "node_modules" | ".venv" | "venv")
    ) {
        return true;
    }
    // Only runtime-owned stores, not all .aizen: verify.json, mcp.json, commands and skills
    // affect planning/execution and must participate. Unknown entries remain inputs.
    if first == Some(".aizen") {
        let second = relative
            .components()
            .nth(1)
            .and_then(|c| c.as_os_str().to_str());
        return matches!(
            second,
            Some("locks" | "sessions" | "cli-memory" | "device.json")
        );
    }
    relative
        .file_name()
        .is_some_and(|name| name == "__pycache__")
}

fn relative_name(relative: &Path) -> Option<String> {
    // Join components, not characters: a literal Unix backslash is not a path separator.
    let mut name = String::new();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return None;
        };
        if !name.is_empty() {
            name.push('/');
        }
        name.push_str(component.to_str()?);
    }
    Some(name)
}

fn collect(root: &Path, limits: Limits, deadline: Instant) -> Option<Vec<Entry>> {
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    while let Some((path, depth)) = pending.pop() {
        if Instant::now() >= deadline || depth > limits.depth || entries.len() >= limits.entries {
            return None;
        }
        let md = fs::symlink_metadata(&path).ok()?;
        if is_link_or_reparse(&md) || (!md.is_file() && !md.is_dir()) {
            return None;
        }
        let relative = path.strip_prefix(root).ok()?;
        let name = relative_name(relative)?;
        let directory = md.is_dir();
        if depth == 0 && !directory {
            return None;
        }
        if !directory {
            total_bytes = total_bytes.checked_add(md.len())?;
            if md.len() > limits.file_bytes || total_bytes > limits.total_bytes {
                return None;
            }
        }
        entries.push(Entry {
            stamp: NodeStamp::capture(&path, &md, false).ok()?,
            name,
            directory,
            len: md.len(),
            mode: mode(&md),
            path: path.clone(),
        });
        if directory {
            for child in fs::read_dir(&path).ok()? {
                let path = child.ok()?.path();
                if !excluded(path.strip_prefix(root).ok()?) {
                    // Bound the queue too: a wide directory cannot allocate before the entry cap.
                    if entries.len().checked_add(pending.len())? >= limits.entries {
                        return None;
                    }
                    pending.push((path, depth + 1));
                }
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Some(entries)
}

pub(super) fn capture(root: &Path) -> Option<[u8; 32]> {
    capture_with_limits(root, LIMITS)
}

fn capture_with_limits(root: &Path, limits: Limits) -> Option<[u8; 32]> {
    let deadline = Instant::now().checked_add(limits.duration)?;
    let entries = collect(root, limits, deadline)?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(b"aizen-local-drift-v1\0");
    for entry in &entries {
        if Instant::now() >= deadline {
            return None;
        }
        digest.update(&(entry.name.len() as u64).to_le_bytes());
        digest.update(entry.name.as_bytes());
        digest.update(&[u8::from(entry.directory)]);
        digest.update(&entry.mode.to_le_bytes());
        if !entry.directory {
            let md = fs::symlink_metadata(&entry.path).ok()?;
            if NodeStamp::capture(&entry.path, &md, false).ok()? != entry.stamp {
                return None;
            }
            let file = fs::File::open(&entry.path).ok()?;
            let opened = file.metadata().ok()?;
            if !opened.is_file() || opened.len() != entry.len || mode(&opened) != entry.mode {
                return None;
            }
            let mut bytes = Vec::with_capacity(entry.len as usize);
            file.take(limits.file_bytes.checked_add(1)?)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 != entry.len || Instant::now() >= deadline {
                return None;
            }
            let md = fs::symlink_metadata(&entry.path).ok()?;
            if NodeStamp::capture(&entry.path, &md, false).ok()? != entry.stamp {
                return None;
            }
            let fingerprint = FileFingerprint::for_bytes(&bytes);
            digest.update(&fingerprint.byte_len.to_le_bytes());
            digest.update(&fingerprint.sha256);
        }
    }
    // Fail closed on concurrent membership/replacement/metadata changes during capture. This is
    // still an observation of a live tree, never isolation against a hostile racing process.
    let after = collect(root, limits, deadline)?;
    if entries.len() != after.len()
        || entries
            .iter()
            .zip(&after)
            .any(|(a, b)| a.name != b.name || a.stamp != b.stamp || a.mode != b.mode)
    {
        return None;
    }
    let mut result = [0; 32];
    result.copy_from_slice(digest.finish().as_ref());
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_placement::tests::TestDir;

    #[test]
    fn local_identity_tracks_membership_assets_scripts_and_verify_configuration() {
        let temp = TestDir::new();
        let root = temp.path();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let baseline = capture(root).unwrap();
        assert_eq!(capture(root), Some(baseline));
        for (name, bytes) in [("asset.bin", b"asset".as_slice()), ("build.sh", b"exit 0")] {
            fs::write(root.join(name), bytes).unwrap();
            assert_ne!(capture(root), Some(baseline));
            fs::remove_file(root.join(name)).unwrap();
            assert_eq!(capture(root), Some(baseline));
        }
        fs::create_dir(root.join(".aizen")).unwrap();
        fs::write(
            root.join(".aizen/verify.json"),
            "{\"commands\":[\"build\"]}",
        )
        .unwrap();
        let configured = capture(root).unwrap();
        fs::write(
            root.join(".aizen/verify.json"),
            "{\"commands\":[\"other\"]}",
        )
        .unwrap();
        assert_ne!(capture(root), Some(configured));
    }

    #[test]
    fn local_identity_quotas_and_errors_never_return_partial_digests() {
        let temp = TestDir::new();
        let root = temp.path();
        fs::write(root.join("input"), "12345").unwrap();
        assert!(capture_with_limits(
            root,
            Limits {
                entries: 1,
                ..LIMITS
            }
        )
        .is_none());
        assert!(capture_with_limits(
            root,
            Limits {
                file_bytes: 4,
                ..LIMITS
            }
        )
        .is_none());
        assert!(capture_with_limits(
            root,
            Limits {
                total_bytes: 4,
                ..LIMITS
            }
        )
        .is_none());
        assert!(capture_with_limits(root, Limits { depth: 0, ..LIMITS }).is_none());
        assert!(capture_with_limits(
            root,
            Limits {
                duration: Duration::ZERO,
                ..LIMITS
            }
        )
        .is_none());
        assert!(capture(&root.join("missing")).is_none());
        assert!(
            capture(&root.join("input")).is_none(),
            "a file is not a workspace tree"
        );
        assert!(capture(root).is_some());
    }

    #[test]
    fn local_identity_ignores_only_documented_output_and_runtime_stores() {
        let temp = TestDir::new();
        let root = temp.path();
        fs::create_dir(root.join("target")).unwrap();
        fs::create_dir(root.join(".aizen")).unwrap();
        fs::create_dir(root.join(".aizen/locks")).unwrap();
        let baseline = capture(root).unwrap();
        fs::write(root.join("target/output"), "generated").unwrap();
        fs::write(root.join(".aizen/locks/live"), "runtime").unwrap();
        assert_eq!(capture(root), Some(baseline));
        fs::write(root.join(".aizen/custom-input"), "input").unwrap();
        assert_ne!(capture(root), Some(baseline));
    }

    #[test]
    fn local_identity_relative_names_are_component_exact() {
        assert_eq!(relative_name(Path::new("")), Some(String::new()));
        assert_eq!(
            relative_name(&Path::new("nested").join("input")),
            Some("nested/input".into())
        );
        assert!(relative_name(Path::new("../input")).is_none());
        #[cfg(unix)]
        assert_ne!(
            relative_name(Path::new(r"nested\input")),
            relative_name(Path::new("nested/input"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_identity_refuses_links_and_tracks_permission_changes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = TestDir::new();
        let input = temp.path().join("script");
        fs::write(&input, "exit 0").unwrap();
        let baseline = capture(temp.path()).unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o700)).unwrap();
        assert_ne!(capture(temp.path()), Some(baseline));
        symlink(&input, temp.path().join("alias")).unwrap();
        assert!(capture(temp.path()).is_none());
    }
}
