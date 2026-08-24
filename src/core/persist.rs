//! Crash-safe and conflict-aware byte persistence shared by metadata and workspace stores.
//!
//! Writers never truncate a destination in place: they write a uniquely named sibling, flush it,
//! and only then replace the destination. The fingerprint/CAS helpers add a separate guarantee:
//! content computed from an old generation is never silently committed over a newer one.

use anyhow::{Context, Result};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn unique_sequence() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub exists: bool,
    pub byte_len: u64,
    pub sha256: [u8; 32],
}

impl FileFingerprint {
    pub fn missing() -> Self {
        Self {
            exists: false,
            byte_len: 0,
            sha256: [0; 32],
        }
    }

    pub fn for_bytes(bytes: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(digest.as_ref());
        Self {
            exists: true,
            byte_len: bytes.len() as u64,
            sha256,
        }
    }

    /// Six hex chars of the digest — a fingerprint short enough to put in a message. Conflict
    /// reporting currently names the path instead, which is what a reader can act on.
    #[allow(dead_code)]
    pub fn short_id(&self) -> String {
        self.sha256[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[derive(Debug)]
pub struct WriteConflict {
    path: PathBuf,
    expected: FileFingerprint,
    actual: FileFingerprint,
}

impl WriteConflict {
    /// The path that drifted. Read via the `Display` impl in practice, which formats the whole
    /// conflict; this is the accessor for a caller that wants to branch on the path itself.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for WriteConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match (self.expected.exists, self.actual.exists) {
            (false, true) => "was created by another writer",
            (true, false) => "was removed by another writer",
            _ => "changed after it was read",
        };
        write!(
            f,
            "edit conflict: {} {reason}; nothing was written. Re-read the file and retry",
            self.path.display()
        )
    }
}

impl std::error::Error for WriteConflict {}

fn temp_path(path: &Path) -> PathBuf {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    parent.join(format!(
        ".{name}.aizen-tmp-{}-{}",
        std::process::id(),
        unique_sequence()
    ))
}

/// Remove `.{name}.aizen-tmp-{pid}-{seq}` staging orphans in `dir` — leftovers of a writer killed
/// between the staged write and the rename (the in-process error path never ran, and nothing else
/// knows the name). Age-gated: another process may be mid-write RIGHT NOW and its staging file is
/// indistinguishable by name; a minute is far longer than any staged write. Best-effort.
pub fn sweep_orphan_temps_in(dir: &Path) {
    sweep_orphan_temps_older_than(dir, 60);
}

/// The age knob exists for tests (mtime cannot be set portably without a dependency); production
/// callers go through [`sweep_orphan_temps_in`] and its one-minute gate.
pub(crate) fn sweep_orphan_temps_older_than(dir: &Path, min_age_secs: u64) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with('.') || !name.contains(".aizen-tmp-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|md| md.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() >= min_age_secs);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Sweep `parent` for staging orphans at most once per process. Every `atomic_write` destination
/// dir self-heals this way — including dirs inside the user's repo, which the startup sweep (scoped
/// to the state dir) can never see. The once-gate keeps the added cost to one `read_dir` per
/// distinct directory per run instead of one per write.
fn sweep_parent_once(parent: &Path) {
    use std::sync::{Mutex, OnceLock};
    static SWEPT: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    let swept = SWEPT.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    {
        let mut set = swept.lock().unwrap_or_else(|e| e.into_inner());
        if set.len() >= 512 {
            set.clear(); // bounded; a re-sweep only costs a read_dir
        }
        if !set.insert(parent.to_path_buf()) {
            return;
        }
    }
    sweep_orphan_temps_in(parent);
}

/// Read bytes and return the exact content fingerprint used for a later compare-and-swap.
pub fn read_with_fingerprint(path: &Path) -> Result<(Option<Vec<u8>>, FileFingerprint)> {
    match fs::read(path) {
        Ok(bytes) => {
            let fingerprint = FileFingerprint::for_bytes(&bytes);
            Ok((Some(bytes), fingerprint))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((None, FileFingerprint::missing()))
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn fingerprint(path: &Path) -> Result<FileFingerprint> {
    read_with_fingerprint(path).map(|(_, fingerprint)| fingerprint)
}

/// Commit only when the current destination still equals `expected`.
///
/// Callers that need coordination against other compliant Aizen writers should also hold the
/// appropriate resource/workspace lock. This CAS remains the final defense against editors, old
/// binaries, and other programs that do not honor Aizen's lock protocol.
pub fn compare_and_atomic_write(
    path: &Path,
    expected: &FileFingerprint,
    bytes: &[u8],
) -> Result<FileFingerprint> {
    let actual = fingerprint(path)?;
    if &actual != expected {
        return Err(WriteConflict {
            path: path.to_path_buf(),
            expected: expected.clone(),
            actual,
        }
        .into());
    }
    if expected.exists {
        atomic_write(path, bytes)?;
    } else {
        atomic_create(path, bytes)?;
    }
    Ok(FileFingerprint::for_bytes(bytes))
}

pub fn create_if_absent(path: &Path, bytes: &[u8]) -> Result<FileFingerprint> {
    compare_and_atomic_write(path, &FileFingerprint::missing(), bytes)
}

/// Delete `path` only if it still matches `expected` — the compare-and-swap of removal, so a
/// concurrent writer's version is never destroyed. No deletion path needs the guard yet (retire and
/// purge rewrite the store rather than unlinking files), so it goes unused.
#[allow(dead_code)]
pub fn remove_if_unchanged(path: &Path, expected: &FileFingerprint) -> Result<bool> {
    let actual = fingerprint(path)?;
    if &actual != expected {
        return Err(WriteConflict {
            path: path.to_path_buf(),
            expected: expected.clone(),
            actual,
        }
        .into());
    }
    if !expected.exists {
        return Ok(false);
    }
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    sync_parent(path);
    Ok(true)
}

/// Atomically replace `path` with `bytes`.
///
/// The temporary file is created with `create_new`, so concurrent writers never share a staging
/// path. Existing Unix mode bits are copied before the rename. The final path is never truncated.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    sweep_parent_once(parent);

    let tmp = temp_path(path);
    let result = (|| -> Result<()> {
        write_staged(&tmp, bytes)?;

        // Keep the existing mode on Unix. This is intentionally best-effort: the data durability
        // path must not turn a recoverable write into a data-loss path because chmod failed.
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }

        replace_file(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        sync_parent(path);
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            WriteConflict {
                path: path.to_path_buf(),
                expected: FileFingerprint::missing(),
                actual: fingerprint(path)
                    .unwrap_or_else(|_| FileFingerprint::for_bytes(b"occupied")),
            }
            .into()
        } else {
            anyhow::Error::from(e).context(format!("creating {}", path.display()))
        }
    })?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing {}", path.display()))?;
    drop(file);
    sync_parent(path);
    Ok(())
}

fn write_staged(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("creating temporary {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing temporary {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing temporary {}", path.display()))?;
    Ok(())
}

fn replace_file(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let from: Vec<u16> = tmp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let to: Vec<u16> = dst
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(tmp, dst)
    }
}

fn sync_parent(_path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = _path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// Atomically replace a sensitive file, hardening the staged bytes before they become visible.
pub fn atomic_write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = temp_path(path);
    let result = (|| -> Result<()> {
        write_staged(&tmp, bytes)?;
        harden_owner_only_checked(&tmp)?;
        replace_file(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        sync_parent(path);
        harden_owner_only_checked(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Owner-only hardening for a newly created metadata file. Unix applies mode 0600. Windows replaces
/// the inherited DACL with an SDDL owner-rights ACL (`OW`: object owner) and protects it from parent
/// inheritance. Callers that store transcripts/secrets should propagate this result.
pub fn harden_owner_only_checked(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("hardening {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR,
        };

        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("building owner-only ACL for {}", path.display()));
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let applied = unsafe {
            SetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        };
        unsafe { LocalFree(descriptor) };
        if applied == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("applying owner-only ACL to {}", path.display()));
        }
    }
    Ok(())
}

/// Read a file while distinguishing a missing file from a present-but-invalid/unreadable file.
pub fn read_optional(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove a file only when it exists; other errors are returned.
pub fn remove_if_exists(path: &Path) -> std::io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => {
            sync_parent(path);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path_named(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aizen-persist-{name}-{}-{}",
            std::process::id(),
            unique_sequence()
        ))
    }

    #[test]
    fn cas_rejects_drift_without_overwrite() {
        let path = temp_path_named("drift");
        fs::write(&path, b"base").unwrap();
        let (_, expected) = read_with_fingerprint(&path).unwrap();
        fs::write(&path, b"other writer").unwrap();
        let err = compare_and_atomic_write(&path, &expected, b"stale edit").unwrap_err();
        assert!(err.downcast_ref::<WriteConflict>().is_some(), "{err:#}");
        assert_eq!(fs::read(&path).unwrap(), b"other writer");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn create_if_absent_has_exactly_one_winner() {
        let path = temp_path_named("create");
        create_if_absent(&path, b"first").unwrap();
        let err = create_if_absent(&path, b"second").unwrap_err();
        assert!(err.downcast_ref::<WriteConflict>().is_some(), "{err:#}");
        assert_eq!(fs::read(&path).unwrap(), b"first");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cas_replaces_unchanged_generation() {
        let path = temp_path_named("replace");
        fs::write(&path, b"old").unwrap();
        let (_, expected) = read_with_fingerprint(&path).unwrap();
        let next = compare_and_atomic_write(&path, &expected, b"new").unwrap();
        assert_eq!(next, FileFingerprint::for_bytes(b"new"));
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_file(path);
    }

    /// The orphan sweep must take exactly the killed-writer leftovers and nothing else: staging
    /// names are removed, real files (including dotfiles that merely LOOK similar) survive.
    #[test]
    fn orphan_sweep_removes_staging_names_and_spares_everything_else() {
        let dir = std::env::temp_dir().join(format!("aizen-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let orphan_a = dir.join(".state.json.aizen-tmp-999-0");
        let orphan_b = dir.join(".cfg.toml.aizen-tmp-1-7");
        let real = dir.join("state.json");
        let dotfile = dir.join(".gitignore");
        for p in [&orphan_a, &orphan_b, &real, &dotfile] {
            fs::write(p, b"x").unwrap();
        }
        sweep_orphan_temps_older_than(&dir, 0);
        assert!(!orphan_a.exists() && !orphan_b.exists(), "orphans removed");
        assert!(real.exists() && dotfile.exists(), "real files untouched");
        // The production gate: nothing younger than the threshold is touched, because a sibling
        // process may be mid-write with an identical-looking staging file RIGHT NOW.
        fs::write(&orphan_a, b"x").unwrap();
        sweep_orphan_temps_in(&dir);
        assert!(orphan_a.exists(), "a fresh staging file must survive");
        let _ = fs::remove_dir_all(&dir);
    }
}
