//! Cross-process advisory locks used by repository and shared-state transactions.
//!
//! A Rust `Mutex` only serializes threads in one process. Aizen state is also reachable from sibling
//! agents, workflows, CLI invocations, daemons, and linked worktrees, so coordination that protects
//! persistent state must use an OS lock. The lock handle is the authority: diagnostic owner files may
//! become stale after a hard kill, but an OS lock is released automatically when its process exits.

use anyhow::{Context, Result};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub struct LockBusy {
    pub path: PathBuf,
    pub mode: LockMode,
    pub timeout: Duration,
}

impl fmt::Display for LockBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            LockMode::Shared => "shared",
            LockMode::Exclusive => "exclusive",
        };
        write!(
            f,
            "resource is busy (could not acquire {mode} lock {} within {}s); nothing was changed",
            self.path.display(),
            self.timeout.as_secs_f64()
        )
    }
}

impl std::error::Error for LockBusy {}

#[derive(Debug)]
pub struct RepoTxnLock {
    file: File,
    /// The OS holds the real lock state; this mode also guards descriptor-only marker reads.
    /// `Drop` releases whichever lock was taken.
    #[allow(dead_code)]
    mode: LockMode,
}

impl RepoTxnLock {
    /// Backward-compatible exclusive acquisition used by existing Time Machine and recovery callers.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_mode(path, LockMode::Exclusive, timeout, None)
    }

    pub fn acquire_shared(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_mode(path, LockMode::Shared, timeout, None)
    }

    pub fn acquire_exclusive(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_mode(path, LockMode::Exclusive, timeout, None)
    }

    /// Acquire only an already-existing regular lock file. Restart inspection must never recreate
    /// a missing parent or marker. The descriptor is checked against the pathname after acquisition;
    /// callers retaining it across operations must retain their own ancestor guard too.
    pub(crate) fn acquire_existing(path: &Path, mode: LockMode, timeout: Duration) -> Result<Self> {
        let resource = crate::core::workspace_txn::NativeResource::resolve(path)?;
        let stamp = crate::core::file_placement::NodeStamp::capture(
            path,
            &std::fs::symlink_metadata(path)?,
            false,
        )?;
        let file = open_existing_lock(path)?;
        let lock = Self::acquire_open_file(path, mode, timeout, None, file)?;
        resource.verify()?;
        lock.verify_existing_stamp(path, &stamp)?;
        Ok(lock)
    }

    pub(crate) fn acquire_existing_exclusive(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_existing(path, LockMode::Exclusive, timeout)
    }

    /// Compare the owned descriptor with the existing pathname without reading locked byte ranges.
    pub(crate) fn verify_existing_path(&self, path: &Path) -> Result<()> {
        let resource = crate::core::workspace_txn::NativeResource::resolve(path)?;
        let stamp = crate::core::file_placement::NodeStamp::capture(
            path,
            &std::fs::symlink_metadata(path)?,
            false,
        )?;
        self.verify_existing_stamp(path, &stamp)?;
        resource.verify()
    }

    fn verify_existing_stamp(
        &self,
        path: &Path,
        stamp: &crate::core::file_placement::NodeStamp,
    ) -> Result<()> {
        let owned = lock_file_identity(&self.file)?;
        anyhow::ensure!(
            (owned.0, owned.1) == stamp.identity(),
            "existing transaction lock descriptor/path identity changed"
        );
        anyhow::ensure!(
            crate::core::file_placement::NodeStamp::capture(
                path,
                &std::fs::symlink_metadata(path)?,
                false,
            )? == *stamp,
            "existing transaction lock changed during inspection"
        );
        Ok(())
    }

    fn acquire_open_file(
        path: &Path,
        mode: LockMode,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        file: File,
    ) -> Result<Self> {
        let start = Instant::now();
        loop {
            if cancel.is_some_and(crate::core::cancel::TurnCancel::is_cancelled) {
                anyhow::bail!(
                    "cancelled while waiting for lock {}; nothing was changed",
                    path.display()
                );
            }
            match try_lock(&file, mode) {
                Ok(true) => return Ok(Self { file, mode }),
                Ok(false) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(false) => {
                    return Err(LockBusy {
                        path: path.to_path_buf(),
                        mode,
                        timeout,
                    }
                    .into());
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
            }
        }
    }

    /// Acquire while observing an optional turn cancellation token. Cancellation is checked between
    /// non-blocking OS lock attempts, so waiting never strands a tool after Esc.
    pub fn acquire_mode(
        path: &Path,
        mode: LockMode,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening transaction lock {}", path.display()))?;
        Self::acquire_open_file(path, mode, timeout, cancel, file)
    }

    /// Read bounded bytes through the descriptor that owns exclusive exclusion. A second
    /// descriptor cannot read a Windows byte-range-locked marker, even in the same process.
    pub(crate) fn read_bounded_exclusive(&self, limit: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        anyhow::ensure!(
            self.mode == LockMode::Exclusive,
            "lock marker read requires exclusive ownership"
        );
        anyhow::ensure!(
            self.file.metadata()?.len() <= limit,
            "lock marker exceeds byte bound"
        );
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(
            limit
                .checked_add(1)
                .context("lock marker byte bound overflow")?,
        )
        .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= limit,
            "lock marker exceeds byte bound"
        );
        Ok(bytes)
    }

    /// Which mode this lock was taken in.
    #[allow(dead_code)]
    pub fn mode(&self) -> LockMode {
        self.mode
    }
}

impl Drop for RepoTxnLock {
    fn drop(&mut self) {
        let _ = unlock(&self.file);
    }
}

#[cfg(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
))]
fn open_existing_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Platform O_NOFOLLOW | O_NONBLOCK: refuse a substituted link without waiting on a FIFO.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0x20000 | 0x800);
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        options.custom_flags(0x100 | 0x4);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening existing transaction lock {}", path.display()))?;
    lock_file_identity(&file)?;
    Ok(file)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)))]
fn open_existing_lock(_path: &Path) -> Result<File> {
    anyhow::bail!("existing-only lock acquisition unavailable on this platform")
}

#[cfg(windows)]
fn lock_file_identity(file: &File) -> Result<(u64, u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading transaction lock identity");
    }
    anyhow::ensure!(
        file.metadata()?.is_file()
            && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && info.nNumberOfLinks == 1,
        "existing transaction lock must be a regular non-link single-link file"
    );
    Ok((
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        info.nNumberOfLinks as u64,
    ))
}

#[cfg(unix)]
fn lock_file_identity(file: &File) -> Result<(u64, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = file.metadata()?;
    anyhow::ensure!(
        md.is_file() && md.nlink() == 1,
        "existing transaction lock must be a regular non-link single-link file"
    );
    Ok((md.dev(), md.ino(), md.nlink()))
}

#[cfg(not(any(unix, windows)))]
fn lock_file_identity(_file: &File) -> Result<(u64, u64, u64)> {
    anyhow::bail!("transaction lock identity unavailable on this platform")
}

#[cfg(windows)]
fn try_lock(file: &File, mode: LockMode) -> std::io::Result<bool> {
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if mode == LockMode::Exclusive {
            LOCKFILE_EXCLUSIVE_LOCK
        } else {
            0
        };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(err)
    }
}

#[cfg(windows)]
fn unlock(file: &File) -> std::io::Result<()> {
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    let ok = unsafe { UnlockFileEx(file.as_raw_handle(), 0, u32::MAX, u32::MAX, &mut overlapped) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn try_lock(file: &File, mode: LockMode) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    const LOCK_SH: i32 = 1;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let operation = match mode {
        LockMode::Shared => LOCK_SH,
        LockMode::Exclusive => LOCK_EX,
    } | LOCK_NB;
    let rc = unsafe { flock(file.as_raw_fd(), operation) };
    if rc == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if matches!(e.kind(), std::io::ErrorKind::WouldBlock) {
        Ok(false)
    } else {
        Err(e)
    }
}

#[cfg(unix)]
fn unlock(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const LOCK_UN: i32 = 8;
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn lock_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aizen-repo-lock-{name}-{}-{}.lock",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ))
    }

    #[test]
    fn acquiring_shared_and_exclusive_preserves_existing_bytes() {
        use std::io::Write;

        let fixture = crate::core::file_placement::tests::TestDir::new();
        let path = fixture.path().join("transaction.lock");
        let sentinel = b"existing lock-file bytes\0must survive acquisition\n";
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(sentinel).unwrap();
        drop(file);

        for mode in [LockMode::Shared, LockMode::Exclusive] {
            let guard = RepoTxnLock::acquire_mode(&path, mode, Duration::ZERO, None).unwrap();
            assert_eq!(guard.mode(), mode);
            // Windows byte-range locks also gate reads through other handles. Read only after
            // releasing the lock, so this checks opening/acquiring without relying on Unix semantics.
            drop(guard);
            assert_eq!(std::fs::read(&path).unwrap(), sentinel, "{mode:?}");
        }
    }

    #[test]
    fn existing_locks_never_create_missing_paths_and_preserve_bytes() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let missing_parent = fixture.path().join("missing").join("owner.lock");
        let path = fixture.path().join("owner.lock");
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            assert!(RepoTxnLock::acquire_existing(&missing_parent, mode, Duration::ZERO).is_err());
            assert!(!missing_parent.parent().unwrap().exists());
            assert!(RepoTxnLock::acquire_existing(&path, mode, Duration::ZERO).is_err());
            assert!(!path.exists());
        }
        std::fs::write(&path, b"marker").unwrap();
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            let guard = RepoTxnLock::acquire_existing(&path, mode, Duration::ZERO).unwrap();
            guard.verify_existing_path(&path).unwrap();
            if mode == LockMode::Exclusive {
                assert_eq!(guard.read_bounded_exclusive(64).unwrap(), b"marker");
                assert!(guard.read_bounded_exclusive(5).is_err());
                assert!(guard.read_bounded_exclusive(u64::MAX).is_err());
            } else {
                assert!(guard.read_bounded_exclusive(64).is_err());
            }
            drop(guard);
            assert_eq!(std::fs::read(&path).unwrap(), b"marker");
        }
    }

    #[test]
    fn existing_shared_locks_coexist_and_refuse_exclusive_nonblocking() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let path = fixture.path().join("owner.lock");
        std::fs::write(&path, b"marker").unwrap();
        let first = RepoTxnLock::acquire_existing(&path, LockMode::Shared, Duration::ZERO).unwrap();
        let second =
            RepoTxnLock::acquire_existing(&path, LockMode::Shared, Duration::ZERO).unwrap();
        let error = RepoTxnLock::acquire_existing_exclusive(&path, Duration::ZERO).unwrap_err();
        assert!(error.downcast_ref::<LockBusy>().is_some(), "{error:#}");
        first.verify_existing_path(&path).unwrap();
        second.verify_existing_path(&path).unwrap();
        drop((first, second));
        let owner = RepoTxnLock::acquire_existing_exclusive(&path, Duration::ZERO).unwrap();
        let error =
            RepoTxnLock::acquire_existing(&path, LockMode::Shared, Duration::ZERO).unwrap_err();
        assert!(error.downcast_ref::<LockBusy>().is_some(), "{error:#}");
        drop(owner);
        assert_eq!(std::fs::read(&path).unwrap(), b"marker");
    }

    #[test]
    fn existing_lock_rejects_replaced_path_and_hardlinks() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let path = fixture.path().join("owner.lock");
        let retained = fixture.path().join("retained.lock");
        std::fs::write(&path, b"marker").unwrap();
        let owner = RepoTxnLock::acquire_existing_exclusive(&path, Duration::ZERO).unwrap();
        std::fs::rename(&path, &retained).unwrap();
        assert!(owner.verify_existing_path(&path).is_err());
        assert!(!path.exists());
        std::fs::write(&path, b"marker").unwrap();
        assert!(
            owner.verify_existing_path(&path).is_err(),
            "identical bytes cannot replace lock identity"
        );
        owner.verify_existing_path(&retained).unwrap();
        drop(owner);
        std::fs::remove_file(&retained).unwrap();
        let owner = RepoTxnLock::acquire_existing_exclusive(&path, Duration::ZERO).unwrap();
        std::fs::hard_link(&path, &retained).unwrap();
        assert!(owner.verify_existing_path(&path).is_err());
        drop(owner);
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            assert!(RepoTxnLock::acquire_existing(&path, mode, Duration::ZERO).is_err());
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"marker");
        assert_eq!(std::fs::read(&retained).unwrap(), b"marker");
    }

    #[test]
    fn existing_lock_rejects_aba_descriptor_against_initial_stamp() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let path = fixture.path().join("owner.lock");
        let original = fixture.path().join("original.lock");
        let substituted = fixture.path().join("substituted.lock");
        std::fs::write(&path, b"same bytes").unwrap();
        let stamp = crate::core::file_placement::NodeStamp::capture(
            &path,
            &std::fs::symlink_metadata(&path).unwrap(),
            false,
        )
        .unwrap();
        std::fs::rename(&path, &original).unwrap();
        std::fs::write(&path, b"same bytes").unwrap();
        let file = open_existing_lock(&path).unwrap();
        let owner =
            RepoTxnLock::acquire_open_file(&path, LockMode::Exclusive, Duration::ZERO, None, file)
                .unwrap();
        owner.verify_existing_path(&path).unwrap();
        std::fs::rename(&path, &substituted).unwrap();
        std::fs::rename(&original, &path).unwrap();
        assert_eq!(
            crate::core::file_placement::NodeStamp::capture(
                &path,
                &std::fs::symlink_metadata(&path).unwrap(),
                false,
            )
            .unwrap(),
            stamp,
            "pathname ABA restores the exact original node"
        );
        assert!(
            owner.verify_existing_stamp(&path, &stamp).is_err(),
            "a matching pathname stamp cannot bless a different locked descriptor"
        );
        drop(owner);
        assert_eq!(std::fs::read(&path).unwrap(), b"same bytes");
        assert_eq!(std::fs::read(&substituted).unwrap(), b"same bytes");
    }

    #[test]
    fn existing_lock_refuses_directory_without_mutating_it() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let path = fixture.path().join("owner.lock");
        std::fs::create_dir(&path).unwrap();
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            assert!(RepoTxnLock::acquire_existing(&path, mode, Duration::ZERO).is_err());
        }
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn existing_lock_refuses_symlinks_and_nonregular_descriptor() {
        use std::os::unix::fs::symlink;
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let target = fixture.path().join("retained.lock");
        let link = fixture.path().join("owner.lock");
        std::fs::write(&target, b"marker").unwrap();
        symlink(&target, &link).unwrap();
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            assert!(RepoTxnLock::acquire_existing(&link, mode, Duration::ZERO).is_err());
        }
        assert!(open_existing_lock(&link).is_err());
        assert!(lock_file_identity(&File::open(fixture.path()).unwrap()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"marker");
    }

    #[test]
    fn shared_locks_coexist_and_exclusive_waits() {
        let path = lock_path("modes");
        let a = RepoTxnLock::acquire_shared(&path, Duration::from_millis(100)).unwrap();
        let b = RepoTxnLock::acquire_shared(&path, Duration::from_millis(100)).unwrap();
        assert_eq!(a.mode(), LockMode::Shared);
        assert_eq!(b.mode(), LockMode::Shared);
        let err = RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(30)).unwrap_err();
        assert!(err.downcast_ref::<LockBusy>().is_some(), "{err:#}");
        drop((a, b));
        RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancellation_stops_a_waiter_without_releasing_owner() {
        let path = lock_path("cancel");
        let owner = RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
        let cancel = crate::core::cancel::TurnCancel::new();
        let (tx, rx) = mpsc::channel();
        let path2 = path.clone();
        let cancel2 = cancel.clone();
        let waiter = std::thread::spawn(move || {
            tx.send(()).unwrap();
            RepoTxnLock::acquire_mode(
                &path2,
                LockMode::Exclusive,
                Duration::from_secs(5),
                Some(&cancel2),
            )
        });
        rx.recv().unwrap();
        cancel.cancel();
        let err = match waiter.join().unwrap() {
            Ok(_) => panic!("cancelled waiter unexpectedly acquired lock"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("cancelled"), "{err:#}");
        let still_busy = RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(30));
        assert!(still_busy.is_err());
        drop(owner);
        let _ = std::fs::remove_file(path);
    }
}
