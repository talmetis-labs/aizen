//! Fresh local installation only; never execute, select, enable, update or touch provider data.
//! Copyright 2026 Aizen authors. SPDX-License-Identifier: Apache-2.0
//!
//! PRECONDITION: caller holds exclusive cross-process manager admission across all installed-root
//! mutations, verification and launch, with old process settlement already proved. Root/ancestors
//! must be controlled by the caller (not adversarially writable). Path preflights are not a defense
//! against arbitrary same-user filesystem races. Package sources may be untrusted; files are held
//! no-follow, fingerprinted, identity-checked and bounded while copied. On unsupported filesystems
//! or operating systems installation fails closed. There is no stale-stage sweep or existing-path
//! removal. Failed cleanup retains ONLY the operation's exclusive stage and reports its path.

use super::manifest::{
    self, Manifest, Namespace, MANIFEST_NAME, MAX_FILES, MAX_MANIFEST_BYTES, MAX_PACKAGE_BYTES,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use zip::{CompressionMethod, ZipArchive};

const RECEIPT: &str = ".install-pins.json";
const MAX_ARCHIVE_BYTES: u64 = 272 * 1024 * 1024;
const MAX_TREE_ENTRIES: usize = MAX_FILES * (manifest::MAX_DEPTH + 1) + 2;
const MAX_RATIO: u64 = 200;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    /// Exact low permission bits from a Unix source or ZIP. No setuid/setgid/sticky bits.
    /// Windows directory packages have no POSIX mode. This field is host metadata, not a grant.
    pub unix_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallPins {
    pub schema_version: u32,
    pub id: String,
    pub target: String,
    pub manifest: FilePin,
    pub files: Vec<FilePin>,
}

#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub directory: PathBuf,
    pub entrypoint: PathBuf,
    pub manifest: Manifest,
    pub pins: InstallPins,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    volume: u64,
    index: u64,
    links: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    identity: Identity,
    bytes: u64,
    mode: Option<u32>,
    modified: std::time::SystemTime,
}

fn snapshot(file: &File, directory: bool) -> Result<Snapshot> {
    let metadata = file.metadata()?;
    check_metadata(&metadata, directory)?;
    let mut identity = identity(file)?;
    ensure!(
        directory || identity.links == 1,
        "hardlinked package files are unsupported"
    );
    // Directory nlink reflects child directories, not hardlink aliases. Never compare it as a
    // stable identity across construction/publication/cleanup of an owned tree.
    if directory {
        identity.links = 0;
    }
    Ok(Snapshot {
        identity,
        bytes: metadata.len(),
        mode: mode(&metadata),
        modified: metadata
            .modified()
            .context("file modification identity unavailable")?,
    })
}

#[cfg(unix)]
fn identity(file: &File) -> Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(Identity {
        volume: m.dev(),
        index: m.ino(),
        links: m.nlink(),
    })
}
#[cfg(windows)]
fn identity(file: &File) -> Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } != 0,
        "file identity unavailable: {}",
        std::io::Error::last_os_error()
    );
    Ok(Identity {
        volume: information.dwVolumeSerialNumber as u64,
        index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
        links: information.nNumberOfLinks as u64,
    })
}
#[cfg(not(any(unix, windows)))]
fn identity(_: &File) -> Result<Identity> {
    bail!("platform file identity unsupported")
}

#[cfg(unix)]
fn mode(metadata: &Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}
#[cfg(not(unix))]
fn mode(_: &Metadata) -> Option<u32> {
    None
}

fn check_metadata(metadata: &Metadata, directory: bool) -> Result<()> {
    ensure!(
        !metadata.file_type().is_symlink(),
        "symlink in package path"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & (0x400 | 0x40 | 0x2 | 0x4) == 0,
            "reparse/device/hidden/system package path"
        );
    }
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "unsupported package file type"
    );
    if let Some(mode) = mode(metadata) {
        ensure!(mode & 0o7000 == 0, "special Unix permissions unsupported");
    }
    Ok(())
}

/// Every existing ancestor is checked, including the explicit caller root. No ambient cwd use.
fn guard(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "caller paths must be absolute");
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::ParentDir | Component::CurDir),
            "noncanonical caller path"
        );
        current.push(component.as_os_str());
        #[cfg(windows)]
        if matches!(component, Component::Prefix(_)) {
            // A verbatim disk prefix alone (\\?\C:) is not a filesystem node. Inspect the
            // drive root once RootDir is appended rather than querying the prefix as a path.
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(m) => {
                ensure!(
                    !m.file_type().is_symlink(),
                    "symlink ancestor: {}",
                    current.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        m.file_attributes() & 0x400 == 0,
                        "reparse ancestor: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", current.display()))
            }
        }
    }
    Ok(())
}

fn open_checked(path: &Path, directory: bool) -> Result<(File, Snapshot)> {
    guard(path)?;
    check_metadata(&fs::symlink_metadata(path)?, directory)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Read sharing only: prevent writing/deleting this source while the handle is retained.
        options
            .share_mode(1)
            .custom_flags(0x00200000 | if directory { 0x02000000 } else { 0 });
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let snap = snapshot(&file, directory)?;
    check_streams(path)?;
    Ok((file, snap))
}

fn check_streams(_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{
            GetLastError, ERROR_HANDLE_EOF, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard,
            WIN32_FIND_STREAM_DATA,
        };
        let path: Vec<u16> = _path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut data: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
        let handle = unsafe {
            FindFirstStreamW(
                path.as_ptr(),
                FindStreamInfoStandard,
                (&mut data as *mut WIN32_FIND_STREAM_DATA).cast(),
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            ensure!(
                unsafe { GetLastError() } == ERROR_HANDLE_EOF,
                "filesystem cannot enumerate alternate streams"
            );
            return Ok(());
        }
        let result = (|| {
            loop {
                let len = data
                    .cStreamName
                    .iter()
                    .position(|&v| v == 0)
                    .context("invalid stream name")?;
                ensure!(
                    String::from_utf16(&data.cStreamName[..len])? == "::$DATA",
                    "alternate data streams unsupported"
                );
                if unsafe {
                    FindNextStreamW(handle, (&mut data as *mut WIN32_FIND_STREAM_DATA).cast())
                } == 0
                {
                    ensure!(
                        unsafe { GetLastError() } == ERROR_HANDLE_EOF,
                        "stream enumeration failed"
                    );
                    break;
                }
            }
            Ok(())
        })();
        unsafe { FindClose(handle) };
        result?;
    }
    Ok(())
}

fn recheck(path: &Path, file: &File, expected: &Snapshot, directory: bool) -> Result<()> {
    ensure!(
        &snapshot(file, directory)? == expected,
        "source changed during read/copy"
    );
    let (_, actual) = open_checked(path, directory)?;
    ensure!(
        &actual == expected,
        "source path identity changed during read/copy"
    );
    Ok(())
}

fn read_bounded(file: &mut File, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "bounded file read exceeded");
    Ok(bytes)
}

fn read_manifest(path: &Path, target: &str) -> Result<(Manifest, Vec<u8>, Snapshot)> {
    let (mut file, snap) = open_checked(path, false)?;
    ensure!(
        snap.bytes <= MAX_MANIFEST_BYTES as u64,
        "manifest size limit"
    );
    let bytes = read_bounded(&mut file, MAX_MANIFEST_BYTES)?;
    ensure!(
        snap.bytes == bytes.len() as u64,
        "manifest changed while reading"
    );
    let manifest = manifest::parse_manifest(&bytes, target)?;
    recheck(path, &file, &snap, false)?;
    Ok((manifest, bytes, snap))
}

fn expected_dirs(manifest: &Manifest) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for file in &manifest.files {
        let mut current = file.path.as_str();
        while let Some((parent, _)) = current.rsplit_once('/') {
            dirs.insert(parent.to_string());
            current = parent;
        }
    }
    dirs
}

fn inventory(
    root: &Path,
    manifest: &Manifest,
    installed: bool,
) -> Result<BTreeMap<String, Snapshot>> {
    struct WalkState<'a> {
        manifest: &'a Manifest,
        installed: bool,
        dirs: BTreeSet<String>,
        namespace: Namespace,
        result: BTreeMap<String, Snapshot>,
        count: usize,
    }
    fn walk(root: &Path, relative: &str, state: &mut WalkState<'_>) -> Result<()> {
        let path = if relative.is_empty() {
            root.to_path_buf()
        } else {
            root.join(relative)
        };
        let (directory, snap) = open_checked(&path, true)?;
        for entry in fs::read_dir(&path)? {
            state.count += 1;
            ensure!(state.count <= MAX_TREE_ENTRIES, "package entry count limit");
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("non-UTF8 package filename")?;
            let rel = if relative.is_empty() {
                name.to_string()
            } else {
                format!("{relative}/{name}")
            };
            if state.installed && rel == RECEIPT {
                let (_, receipt) = open_checked(&entry.path(), false)?;
                ensure!(
                    receipt.bytes <= MAX_MANIFEST_BYTES as u64 * 2,
                    "install metadata size limit"
                );
                state.result.insert(rel, receipt);
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            let is_dir = metadata.is_dir();
            state.namespace.insert(&rel, is_dir)?;
            if is_dir {
                ensure!(
                    state.dirs.contains(&rel),
                    "undeclared/empty package directory"
                );
                walk(root, &rel, state)?;
            } else {
                let (_, file) = open_checked(&entry.path(), false)?;
                if rel == MANIFEST_NAME {
                    ensure!(
                        file.bytes <= MAX_MANIFEST_BYTES as u64,
                        "manifest size limit"
                    );
                } else {
                    let declared = state
                        .manifest
                        .files
                        .iter()
                        .find(|f| f.path == rel)
                        .context("undeclared package file")?;
                    ensure!(
                        declared.bytes == file.bytes,
                        "declared file length mismatch"
                    );
                }
                state.result.insert(rel, file);
            }
        }
        recheck(&path, &directory, &snap, true)?;
        Ok(())
    }
    let mut state = WalkState {
        manifest,
        installed,
        dirs: expected_dirs(manifest),
        namespace: Namespace::default(),
        result: BTreeMap::new(),
        count: 0,
    };
    walk(root, "", &mut state)?;
    ensure!(
        state.result.len() == manifest.files.len() + 1 + usize::from(installed),
        "missing package files/metadata"
    );
    Ok(state.result)
}

fn owner_only(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
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
        // OW = object owner, protected DACL; directory ACE also inherits to new descendants.
        let sddl = if directory {
            "D:P(A;OICI;FA;;;OW)"
        } else {
            "D:P(A;;FA;;;OW)"
        };
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } != 0,
            "cannot create owner-only ACL: {}",
            std::io::Error::last_os_error()
        );
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let ok = unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        };
        unsafe { LocalFree(descriptor) };
        ensure!(
            ok != 0,
            "cannot apply owner-only ACL: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(not(any(unix, windows)))]
    bail!("owner-only permissions unsupported");
    Ok(())
}

fn verify_private(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.permissions().mode() & 0o777 == if directory { 0o700 } else { 0o600 },
            "install directory/metadata permissions widened"
        );
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
            GetFileSecurityW, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, ACL,
            DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut needed = 0;
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut needed,
            )
        };
        ensure!(
            (20..=65536).contains(&needed),
            "install security descriptor unavailable/oversized"
        );
        // u32 storage gives the self-relative descriptor its required DWORD alignment.
        let mut storage = vec![0u32; (needed as usize).div_ceil(4)];
        let actual: PSECURITY_DESCRIPTOR = storage.as_mut_ptr().cast();
        ensure!(
            unsafe {
                GetFileSecurityW(
                    wide.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    actual,
                    needed,
                    &mut needed,
                )
            } != 0,
            "install ACL unavailable"
        );
        let sddl: Vec<u16> = if directory {
            "D:P(A;OICI;FA;;;OW)"
        } else {
            "D:P(A;;FA;;;OW)"
        }
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let mut expected: PSECURITY_DESCRIPTOR = null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut expected,
                    null_mut(),
                )
            } != 0,
            "owner-only ACL descriptor unavailable"
        );
        let result = (|| {
            let mut control = 0u16;
            let mut revision = 0;
            ensure!(
                unsafe { GetSecurityDescriptorControl(actual, &mut control, &mut revision) } != 0
                    && control & 0x1000 != 0,
                "install ACL inheritance protection removed"
            );
            let mut present = 0;
            let mut defaulted = 0;
            let mut actual_acl: *mut ACL = null_mut();
            let mut expected_acl: *mut ACL = null_mut();
            ensure!(
                unsafe {
                    GetSecurityDescriptorDacl(actual, &mut present, &mut actual_acl, &mut defaulted)
                } != 0
                    && present != 0
                    && !actual_acl.is_null(),
                "missing install ACL"
            );
            ensure!(
                unsafe {
                    GetSecurityDescriptorDacl(
                        expected,
                        &mut present,
                        &mut expected_acl,
                        &mut defaulted,
                    )
                } != 0
                    && present != 0
                    && !expected_acl.is_null(),
                "missing expected ACL"
            );
            let actual_size = unsafe { (*actual_acl).AclSize as usize };
            let expected_size = unsafe { (*expected_acl).AclSize as usize };
            ensure!(
                actual_size == expected_size && actual_size <= storage.len() * 4,
                "install ACL widened"
            );
            ensure!(
                unsafe { std::slice::from_raw_parts(actual_acl.cast::<u8>(), actual_size) }
                    == unsafe {
                        std::slice::from_raw_parts(expected_acl.cast::<u8>(), expected_size)
                    },
                "install ACL differs from owner-only policy"
            );
            Ok(())
        })();
        unsafe { LocalFree(expected) };
        result?;
    }
    Ok(())
}

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    guard(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
        // Apply a protected inheritable owner-only ACL in the actual create, not in a second
        // syscall after the directory has briefly been visible with a permissive inherited DACL.
        let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } != 0,
            "cannot create directory ACL: {}",
            std::io::Error::last_os_error()
        );
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let ok = unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) };
        let error = std::io::Error::last_os_error();
        unsafe { LocalFree(descriptor) };
        if ok == 0 {
            return Err(error)
                .with_context(|| format!("creating private directory {}", path.display()));
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    bail!("private directory creation unsupported");
    #[cfg(unix)]
    owner_only(path, true)
}

struct Stage {
    path: PathBuf,
    identity: Identity,
    owned: Vec<(PathBuf, bool, Identity)>,
    published: bool,
}
impl Stage {
    fn new(root: &Path) -> Result<Self> {
        for _ in 0..32 {
            let path = root.join(format!(
                ".install-stage-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match create_private_dir(&path) {
                Ok(()) => {
                    let (_, snap) = open_checked(&path, true)?;
                    return Ok(Self {
                        path,
                        identity: snap.identity,
                        owned: vec![],
                        published: false,
                    });
                }
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            }
        }
        bail!("exclusive install stage unavailable")
    }
    fn prepare_dirs(&mut self, manifest: &Manifest) -> Result<()> {
        for relative in expected_dirs(manifest) {
            let path = self.path.join(relative);
            create_private_dir(&path)?;
            let (_, snap) = open_checked(&path, true)?;
            self.owned.push((path, true, snap.identity));
        }
        Ok(())
    }
    fn create_file(&mut self, relative: &str) -> Result<File> {
        let path = self.path.join(relative);
        guard(&path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        self.owned.push((path.clone(), false, identity(&file)?));
        owner_only(&path, false)?;
        Ok(file)
    }
    fn write(&mut self, relative: &str, bytes: &[u8]) -> Result<()> {
        let mut file = self.create_file(relative)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        if self.published {
            return Ok(());
        }
        let (_, actual) = open_checked(&self.path, true)?;
        ensure!(
            actual.identity == self.identity,
            "owned stage identity changed; refusing cleanup"
        );
        // Never recursively delete unknown entries, even inside a stage. Only exact paths created
        // by this operation are removed. Empty-dir removal fails if somebody planted an extra.
        for (path, directory, expected) in self.owned.iter().rev() {
            guard(path)?;
            let (_, actual) = open_checked(path, *directory)?;
            ensure!(
                &actual.identity == expected,
                "stage entry identity changed; refusing removal"
            );
            if *directory {
                fs::remove_dir(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        self.owned.clear();
        fs::remove_dir(&self.path)?;
        self.published = true;
        Ok(())
    }
}

fn validate_root(root: &Path, source: &Path) -> Result<()> {
    let (_, _) = open_checked(root, true)?;
    guard(source)?;
    let root = fs::canonicalize(root)?;
    let source = fs::canonicalize(source)?;
    ensure!(
        !root.starts_with(&source) && !source.starts_with(&root),
        "source and install roots must not overlap"
    );
    Ok(())
}

fn no_existing_id(root: &Path, id: &str) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let name = entry?.file_name();
        let name = name
            .to_str()
            .context("nonportable existing install-root name")?;
        ensure!(
            !name.eq_ignore_ascii_case(id),
            "package already exists (including case alias); updates are not supported"
        );
    }
    Ok(())
}

fn file_mode(mode: Option<u32>, entrypoint: bool, target: &str) -> Result<()> {
    if let Some(mode) = mode {
        ensure!(
            mode & !0o777 == 0 && mode & 0o400 != 0 && mode & 0o022 == 0,
            "unsupported, unreadable or group/other-writable packaging permissions"
        );
    }
    if !target.contains("windows") && entrypoint {
        ensure!(
            mode.is_some_and(|m| m & 0o100 != 0),
            "Unix native entrypoint needs packaged owner execute permission"
        );
    }
    Ok(())
}

fn set_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn transfer(
    reader: &mut impl Read,
    writer: &mut impl Write,
    bytes: u64,
    digest: &str,
) -> Result<()> {
    let mut hash = Sha256::new();
    let mut remaining = bytes;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let cap = remaining.min(buffer.len() as u64) as usize;
        let n = reader.read(&mut buffer[..cap])?;
        ensure!(n > 0, "truncated package file");
        hash.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
        remaining -= n as u64;
    }
    ensure!(
        reader.read(&mut buffer[..1])? == 0,
        "package file exceeds declared bytes"
    );
    ensure!(
        manifest::hex_digest(&hash.finalize()) == digest,
        "package SHA256 mismatch"
    );
    Ok(())
}

/// Install into exactly `plugins_root/<manifest.id>`, where plugins_root already exists. No
/// replacement, data-directory creation, process execution or activation. Source stays unchanged.
pub fn install_directory(
    plugins_root: &Path,
    source: &Path,
    target: &str,
) -> Result<InstalledPackage> {
    install_directory_expected(plugins_root, source, target, None)
}

fn check_expected_id(manifest: &Manifest, expected_id: Option<&str>) -> Result<()> {
    if let Some(id) = expected_id {
        manifest::validate_id(id)?;
        ensure!(
            manifest.id == id,
            "expected Memory provider {id}, found {}",
            manifest.id
        );
    }
    Ok(())
}

pub fn install_directory_expected(
    plugins_root: &Path,
    source: &Path,
    target: &str,
    expected_id: Option<&str>,
) -> Result<InstalledPackage> {
    validate_root(plugins_root, source)?;
    let (manifest, bytes, manifest_snapshot) = read_manifest(&source.join(MANIFEST_NAME), target)?;
    check_expected_id(&manifest, expected_id)?;
    let before = inventory(source, &manifest, false)?;
    ensure!(
        before.get(MANIFEST_NAME) == Some(&manifest_snapshot),
        "manifest source changed"
    );
    no_existing_id(plugins_root, &manifest.id)?;
    let mut stage = Stage::new(plugins_root)?;
    let result = (|| {
        stage.prepare_dirs(&manifest)?;
        stage.write(MANIFEST_NAME, &bytes)?;
        let mut files = Vec::new();
        for declared in &manifest.files {
            let path = source.join(&declared.path);
            let (mut input, snap) = open_checked(&path, false)?;
            ensure!(
                before.get(&declared.path) == Some(&snap),
                "source identity changed before copy"
            );
            file_mode(snap.mode, declared.path == manifest.entrypoint, target)?;
            let mut output = stage.create_file(&declared.path)?;
            transfer(&mut input, &mut output, declared.bytes, &declared.sha256)?;
            output.sync_all()?;
            drop(output);
            recheck(&path, &input, &snap, false)?;
            set_mode(&stage.path.join(&declared.path), snap.mode)?;
            files.push(FilePin {
                path: declared.path.clone(),
                bytes: declared.bytes,
                sha256: declared.sha256.clone(),
                unix_mode: snap.mode,
            });
        }
        ensure!(
            inventory(source, &manifest, false)? == before,
            "source tree changed during install"
        );
        let (again, again_bytes, again_snapshot) =
            read_manifest(&source.join(MANIFEST_NAME), target)?;
        ensure!(
            again == manifest && again_bytes == bytes && again_snapshot == manifest_snapshot,
            "manifest changed during install"
        );
        finish(plugins_root, &mut stage, manifest, &bytes, files, target)
    })();
    finish_or_cleanup(&mut stage, result)
}

fn finish_or_cleanup(
    stage: &mut Stage,
    result: Result<InstalledPackage>,
) -> Result<InstalledPackage> {
    match result {
        Ok(package) => Ok(package),
        Err(error) => match stage.cleanup() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "owned stage retained at {}: cleanup failed: {cleanup:#}",
                stage.path.display()
            ))),
        },
    }
}

fn finish(
    root: &Path,
    stage: &mut Stage,
    manifest: Manifest,
    bytes: &[u8],
    files: Vec<FilePin>,
    target: &str,
) -> Result<InstalledPackage> {
    let package = finish_prepared(stage, manifest, bytes, files, target)?;
    publish_fresh(root, stage, package)
}

fn finish_prepared(
    stage: &mut Stage,
    manifest: Manifest,
    bytes: &[u8],
    files: Vec<FilePin>,
    target: &str,
) -> Result<InstalledPackage> {
    let pins = InstallPins {
        schema_version: 1,
        id: manifest.id.clone(),
        target: manifest.target.clone(),
        manifest: FilePin {
            path: MANIFEST_NAME.to_string(),
            bytes: bytes.len() as u64,
            sha256: manifest::sha256(bytes),
            unix_mode: cfg!(unix).then_some(0o600),
        },
        files,
    };
    let metadata = serde_json::to_vec(&pins)?;
    ensure!(
        metadata.len() <= MAX_MANIFEST_BYTES * 2,
        "install metadata size limit"
    );
    stage.write(RECEIPT, &metadata)?;
    verify_at(&stage.path, &pins, target)?;
    sync_directories(&stage.path)?;
    Ok(InstalledPackage {
        directory: stage.path.clone(),
        entrypoint: stage.path.join(&manifest.entrypoint),
        manifest,
        pins,
    })
}

fn publish_fresh(
    root: &Path,
    stage: &mut Stage,
    package: InstalledPackage,
) -> Result<InstalledPackage> {
    let InstalledPackage { manifest, pins, .. } = package;
    no_existing_id(root, &manifest.id)?;
    guard(root)?;
    let (_, current) = open_checked(&stage.path, true)?;
    ensure!(
        current.identity == stage.identity,
        "owned stage replaced before publish"
    );
    let destination = root.join(&manifest.id);
    publish_no_replace(&stage.path, &destination)?;
    stage.published = true;
    // The atomic rename is the commit point. Never remove a committed package on subsequent error.
    sync_after_publication(root).with_context(|| {
        format!(
            "package committed at {}; directory durability unknown",
            destination.display()
        )
    })?;
    Ok(InstalledPackage {
        entrypoint: destination.join(&manifest.entrypoint),
        directory: destination,
        manifest,
        pins,
    })
}

#[cfg(windows)]
pub(crate) fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let from: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    ensure!(
        unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } != 0,
        "non-overwriting publication failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(target_os = "linux")]
pub(crate) fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(destination.as_os_str().as_bytes())?;
    ensure!(
        unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } == 0,
        "atomic exclusive rename unsupported/failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(target_os = "macos")]
pub(crate) fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(destination.as_os_str().as_bytes())?;
    ensure!(
        unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0,
        "atomic exclusive rename unsupported/failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) fn publish_no_replace(_: &Path, _: &Path) -> Result<()> {
    bail!("atomic no-replace publication unsupported")
}

fn sync_directories(root: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if fs::symlink_metadata(entry.path())?.is_dir() {
                sync_directories(&entry.path())?;
            }
        }
        File::open(root)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}
fn sync_after_publication(_root: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_root)?.sync_all()?;
    Ok(())
}

/// Prelaunch read-only verification. `pins` MUST come from trusted host installation state, never
/// the package or its receipt. No cleanup, repair, permission widening or data access occurs.
/// Keep manager admission through actual launch; returning a pathname does not seal later IO.
pub fn verify_installed(
    plugins_root: &Path,
    pins: &InstallPins,
    target: &str,
) -> Result<InstalledPackage> {
    manifest::validate_id(&pins.id)?;
    let (_, _) = open_checked(plugins_root, true)?;
    let directory = plugins_root.join(&pins.id);
    let manifest = verify_at(&directory, pins, target)?;
    Ok(InstalledPackage {
        entrypoint: directory.join(&manifest.entrypoint),
        directory,
        manifest,
        pins: pins.clone(),
    })
}

fn verify_at(directory: &Path, pins: &InstallPins, target: &str) -> Result<Manifest> {
    let (directory_handle, directory_snapshot) = open_checked(directory, true)?;
    verify_private(directory, true)?;
    let manifest_path = directory.join(MANIFEST_NAME);
    verify_private(&manifest_path, false)?;
    verify_private(&directory.join(RECEIPT), false)?;
    ensure!(
        pins.schema_version == 1 && pins.target == target && pins.files.len() <= MAX_FILES,
        "invalid trusted pins version/target/count"
    );
    manifest::validate_id(&pins.id)?;
    manifest::validate_digest(&pins.manifest.sha256)?;
    ensure!(
        pins.manifest.path == MANIFEST_NAME && pins.manifest.bytes <= MAX_MANIFEST_BYTES as u64,
        "invalid manifest pin"
    );
    let (manifest, bytes, manifest_snapshot) =
        read_manifest(&directory.join(MANIFEST_NAME), target)?;
    #[cfg(unix)]
    {
        ensure!(
            manifest_snapshot.mode == pins.manifest.unix_mode
                && manifest_snapshot.mode == Some(0o600),
            "manifest metadata mode changed"
        );
        let (_, package_root) = open_checked(directory, true)?;
        ensure!(
            package_root.mode == Some(0o700),
            "package root must remain owner-only"
        );
    }
    #[cfg(not(unix))]
    let _ = manifest_snapshot;
    ensure!(
        manifest.id == pins.id
            && bytes.len() as u64 == pins.manifest.bytes
            && manifest::sha256(&bytes) == pins.manifest.sha256,
        "manifest pin mismatch"
    );
    ensure!(
        manifest.files.len() == pins.files.len(),
        "file pin count mismatch"
    );
    for relative in expected_dirs(&manifest) {
        verify_private(&directory.join(relative), true)?;
    }
    let before = inventory(directory, &manifest, true)?;
    let (mut receipt, snap) = open_checked(&directory.join(RECEIPT), false)?;
    #[cfg(unix)]
    ensure!(
        snap.mode == Some(0o600),
        "install metadata must remain owner-only"
    );
    ensure!(
        read_bounded(&mut receipt, MAX_MANIFEST_BYTES * 2)? == serde_json::to_vec(pins)?,
        "host install receipt differs from trusted pins"
    );
    recheck(&directory.join(RECEIPT), &receipt, &snap, false)?;
    let mut paths = BTreeSet::new();
    for pin in &pins.files {
        ensure!(paths.insert(&pin.path), "duplicate trusted file pin");
        let declared = manifest
            .files
            .iter()
            .find(|f| f.path == pin.path)
            .context("pin is not declared")?;
        ensure!(
            declared.bytes == pin.bytes && declared.sha256 == pin.sha256,
            "file pin differs from pinned manifest"
        );
        file_mode(pin.unix_mode, pin.path == manifest.entrypoint, target)?;
        let path = directory.join(&pin.path);
        let (mut file, snap) = open_checked(&path, false)?;
        ensure!(
            before.get(&pin.path) == Some(&snap) && snap.bytes == pin.bytes,
            "installed file changed"
        );
        #[cfg(windows)]
        verify_private(&path, false)?;
        #[cfg(unix)]
        ensure!(
            snap.mode == pin.unix_mode,
            "installed permissions differ from trusted pins"
        );
        transfer(&mut file, &mut std::io::sink(), pin.bytes, &pin.sha256)?;
        if pin.path == manifest.entrypoint {
            verify_native(&mut file, target)?;
        }
        recheck(&path, &file, &snap, false)?;
    }
    let (_, again, _) = read_manifest(&directory.join(MANIFEST_NAME), target)?;
    ensure!(
        again == bytes && inventory(directory, &manifest, true)? == before,
        "installed tree changed during verification"
    );
    recheck(directory, &directory_handle, &directory_snapshot, true)?;
    verify_private(directory, true)?;
    Ok(manifest)
}

/// Require the platform native executable magic, class, architecture and executable type. This
/// is not a full loader, signature verifier or trust check; actual OS loader failures remain possible.
fn verify_native(file: &mut File, target: &str) -> Result<()> {
    let (kind, arm) = manifest::target_kind(target)?;
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header)
        .context("native executable header truncated")?;
    match kind {
        "pe" => {
            ensure!(&header[..2] == b"MZ", "entrypoint is not native PE");
            let offset = u32::from_le_bytes(header[60..64].try_into()?) as u64;
            ensure!(
                (64..=1024 * 1024).contains(&offset) && offset + 96 <= file.metadata()?.len(),
                "invalid PE header offset"
            );
            file.seek(SeekFrom::Start(offset))?;
            let mut pe = [0u8; 96];
            file.read_exact(&mut pe)?;
            ensure!(
                &pe[..4] == b"PE\0\0"
                    && u16::from_le_bytes(pe[4..6].try_into()?)
                        == if arm { 0xaa64 } else { 0x8664 },
                "PE architecture mismatch"
            );
            let flags = u16::from_le_bytes(pe[22..24].try_into()?);
            let optional_len = u16::from_le_bytes(pe[20..22].try_into()?) as usize;
            let sections = u16::from_le_bytes(pe[6..8].try_into()?) as usize;
            ensure!(
                flags & 2 != 0
                    && flags & 0x2000 == 0
                    && (112..=4096).contains(&optional_len)
                    && (1..=96).contains(&sections),
                "PE entrypoint is not an executable"
            );
            ensure!(
                offset + 24 + optional_len as u64 + sections as u64 * 40 <= file.metadata()?.len(),
                "PE optional/section headers truncated"
            );
            file.seek(SeekFrom::Start(offset + 24))?;
            let mut optional = vec![0u8; optional_len];
            file.read_exact(&mut optional)?;
            ensure!(
                u16::from_le_bytes(optional[..2].try_into()?) == 0x20b
                    && u32::from_le_bytes(optional[16..20].try_into()?) != 0
                    && matches!(u16::from_le_bytes(optional[68..70].try_into()?), 2 | 3),
                "PE native entrypoint/subsystem invalid"
            );
            let directories = u32::from_le_bytes(optional[108..112].try_into()?) as usize;
            ensure!(
                directories <= 16 && 112 + directories * 8 <= optional_len,
                "PE data directories invalid"
            );
            if directories > 14 {
                ensure!(
                    optional[224..232].iter().all(|&b| b == 0),
                    "managed CLR entrypoints require an external runtime and are unsupported"
                );
            }
        }
        "elf" => {
            ensure!(
                &header[..4] == b"\x7fELF" && header[4] == 2 && header[5] == 1 && header[6] == 1,
                "entrypoint is not ELF64 little endian"
            );
            let ty = u16::from_le_bytes(header[16..18].try_into()?);
            ensure!(
                matches!(ty, 2 | 3)
                    && u16::from_le_bytes(header[18..20].try_into()?) == if arm { 183 } else { 62 },
                "ELF executable architecture/type mismatch"
            );
            ensure!(
                u64::from_le_bytes(header[24..32].try_into()?) != 0,
                "ELF entrypoint missing"
            );
        }
        "macho" => {
            ensure!(
                &header[..4] == b"\xcf\xfa\xed\xfe"
                    && u32::from_le_bytes(header[4..8].try_into()?)
                        == if arm { 0x0100000c } else { 0x01000007 }
                    && u32::from_le_bytes(header[12..16].try_into()?) == 2,
                "entrypoint is not matching thin Mach-O executable"
            );
        }
        _ => bail!("native executable format unsupported"),
    }
    Ok(())
}

struct ZipEntry {
    index: usize,
    path: String,
    directory: bool,
    mode: Option<u32>,
    bytes: u64,
}

fn zip_inventory(archive: &mut ZipArchive<File>) -> Result<Vec<ZipEntry>> {
    ensure!(
        !archive.is_empty() && archive.len() <= MAX_TREE_ENTRIES,
        "ZIP entry count limit"
    );
    let mut namespace = Namespace::default();
    let mut entries = Vec::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        ensure!(!entry.encrypted(), "encrypted ZIP unsupported");
        ensure!(
            matches!(
                entry.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            ),
            "unsupported ZIP codec"
        );
        let raw = std::str::from_utf8(entry.name_raw()).context("ZIP names must be UTF8/ASCII")?;
        ensure!(raw == entry.name(), "ambiguous ZIP filename encoding");
        let directory = raw.ends_with('/');
        let path = if directory {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        namespace.insert(path, directory)?;
        let mode = entry.unix_mode();
        if let Some(mode) = mode {
            let ty = mode & 0o170000;
            ensure!(
                ty == 0 || ty == if directory { 0o040000 } else { 0o100000 },
                "ZIP link/device/special entry unsupported"
            );
            ensure!(mode & 0o7000 == 0, "ZIP special permissions unsupported");
        }
        // Reject extra fields: Unix link metadata and vendor-specific path/type overrides are
        // intentionally unsupported. Zip64 is unnecessary under our strict 256MiB package limits.
        ensure!(
            entry.extra_data().is_none_or(|data| data.is_empty()),
            "ZIP extra fields unsupported"
        );
        let size = entry.size();
        ensure!(
            if directory {
                size == 0
            } else {
                size <= manifest::MAX_FILE_BYTES
            },
            "ZIP file size limit"
        );
        ensure!(
            size <= entry.compressed_size().saturating_mul(MAX_RATIO),
            "ZIP expansion ratio limit"
        );
        total = total.checked_add(size).context("ZIP expansion overflow")?;
        ensure!(
            total <= MAX_PACKAGE_BYTES + MAX_MANIFEST_BYTES as u64,
            "ZIP total expansion limit"
        );
        entries.push(ZipEntry {
            index,
            path: path.to_string(),
            directory,
            mode: mode.map(|m| m & 0o777),
            bytes: size,
        });
    }
    Ok(entries)
}

/// Bounded Stored/Deflate ZIP installation. Archive is never extracted by pathname-based ZIP APIs,
/// and no contained code is run. Only manifest-allowlisted files reach an owned private stage.
pub fn install_zip(plugins_root: &Path, source: &Path, target: &str) -> Result<InstalledPackage> {
    install_zip_expected(plugins_root, source, target, None)
}

/// Reviewed release pins checked on the installer's own handle before staging/publication.
pub(crate) struct ReleaseExpectation<'a> {
    pub id: &'a str,
    pub version: &'a str,
    pub archive_sha256: &'a str,
}

pub(crate) fn install_zip_release(
    plugins_root: &Path,
    source: &Path,
    target: &str,
    expectation: &ReleaseExpectation<'_>,
) -> Result<InstalledPackage> {
    manifest::validate_digest(expectation.archive_sha256)?;
    install_zip_checked(
        plugins_root,
        source,
        target,
        Some(expectation.id),
        Some(expectation),
    )
}

pub fn install_zip_expected(
    plugins_root: &Path,
    source: &Path,
    target: &str,
    expected_id: Option<&str>,
) -> Result<InstalledPackage> {
    install_zip_checked(plugins_root, source, target, expected_id, None)
}

fn install_zip_checked(
    plugins_root: &Path,
    source: &Path,
    target: &str,
    expected_id: Option<&str>,
    release: Option<&ReleaseExpectation<'_>>,
) -> Result<InstalledPackage> {
    let mut prepared = stage_zip_checked(plugins_root, source, target, expected_id, release)?;
    let result = publish_fresh(plugins_root, &mut prepared.stage, prepared.package.clone());
    finish_or_cleanup(&mut prepared.stage, result)
}

/// Verified private stage. Before journaling Drop removes only owned entries; after handoff it
/// deliberately retains all evidence even if cancellation unwinds the update.
pub(crate) struct VerifiedStage {
    stage: Stage,
    pub(crate) package: InstalledPackage,
}
impl VerifiedStage {
    pub(crate) fn retain_for_recovery(&mut self) {
        self.stage.published = true;
    }
}
impl Drop for VerifiedStage {
    fn drop(&mut self) {
        let _ = self.stage.cleanup();
    }
}

pub(crate) fn stage_release(
    plugins_root: &Path,
    source: &Path,
    target: &str,
    expectation: &ReleaseExpectation<'_>,
    expected_archive_bytes: u64,
) -> Result<VerifiedStage> {
    manifest::validate_digest(expectation.archive_sha256)?;
    // Owned source handle also confirms the reviewed compressed size, not only its digest.
    let (source_handle, snap) = open_checked(source, false)?;
    ensure!(
        snap.bytes == expected_archive_bytes,
        "ZIP size differs from reviewed release"
    );
    let prepared = stage_zip_checked(
        plugins_root,
        source,
        target,
        Some(expectation.id),
        Some(expectation),
    )?;
    recheck(source, &source_handle, &snap, false)?;
    Ok(prepared)
}

fn stage_zip_checked(
    plugins_root: &Path,
    source: &Path,
    target: &str,
    expected_id: Option<&str>,
    release: Option<&ReleaseExpectation<'_>>,
) -> Result<VerifiedStage> {
    validate_root(plugins_root, source)?;
    let (mut archive_file, snap) = open_checked(source, false)?;
    ensure!(snap.bytes <= MAX_ARCHIVE_BYTES, "ZIP compressed byte limit");
    // Hash before and after, not metadata alone: Unix writers are not blocked by read handles.
    let archive_digest = hash_file(&mut archive_file, snap.bytes)?;
    if let Some(expected) = release {
        ensure!(
            archive_digest == expected.archive_sha256,
            "ZIP differs from reviewed release SHA256"
        );
    }
    preflight_zip(&mut archive_file, snap.bytes)?;
    archive_file.seek(SeekFrom::Start(0))?;
    let mut archive = ZipArchive::new(archive_file).context("invalid ZIP")?;
    let entries = zip_inventory(&mut archive)?;
    let manifest_entry = entries
        .iter()
        .find(|e| e.path == MANIFEST_NAME && !e.directory)
        .context("ZIP missing plugin.json")?;
    ensure!(
        manifest_entry.bytes <= MAX_MANIFEST_BYTES as u64,
        "manifest size limit"
    );
    let mut bytes = Vec::new();
    archive
        .by_index(manifest_entry.index)?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let manifest = manifest::parse_manifest(&bytes, target)?;
    check_expected_id(&manifest, expected_id)?;
    if let Some(expected) = release {
        ensure!(
            manifest.version == expected.version,
            "ZIP manifest differs from reviewed release version"
        );
    }
    let dirs = expected_dirs(&manifest);
    ensure!(
        entries.iter().filter(|e| !e.directory).count() == manifest.files.len() + 1,
        "ZIP missing/undeclared files"
    );
    for entry in &entries {
        if entry.directory {
            ensure!(dirs.contains(&entry.path), "undeclared/empty ZIP directory");
        } else if entry.path != MANIFEST_NAME {
            let declared = manifest
                .files
                .iter()
                .find(|f| f.path == entry.path)
                .context("undeclared ZIP file")?;
            ensure!(
                declared.bytes == entry.bytes,
                "ZIP declared file length mismatch"
            );
            file_mode(entry.mode, entry.path == manifest.entrypoint, target)?;
        }
    }
    let mut stage = Stage::new(plugins_root)?;
    let result = (|| {
        stage.prepare_dirs(&manifest)?;
        stage.write(MANIFEST_NAME, &bytes)?;
        let mut files = Vec::new();
        for declared in &manifest.files {
            let entry = entries
                .iter()
                .find(|e| e.path == declared.path)
                .context("missing declared ZIP file")?;
            let mut reader = archive.by_index(entry.index)?;
            let mut output = stage.create_file(&entry.path)?;
            transfer(&mut reader, &mut output, declared.bytes, &declared.sha256)?;
            output.sync_all()?;
            drop(output);
            set_mode(&stage.path.join(&entry.path), entry.mode)?;
            files.push(FilePin {
                path: declared.path.clone(),
                bytes: declared.bytes,
                sha256: declared.sha256.clone(),
                unix_mode: entry.mode,
            });
        }
        let mut file = archive.into_inner();
        recheck(source, &file, &snap, false)?;
        ensure!(
            hash_file(&mut file, snap.bytes)? == archive_digest,
            "ZIP changed while installing"
        );
        recheck(source, &file, &snap, false)?;
        finish_prepared(&mut stage, manifest, &bytes, files, target)
    })();
    let package = finish_or_cleanup(&mut stage, result)?;
    Ok(VerifiedStage { stage, package })
}

/// Stable directory identity and complete package verification for host-generated transaction paths.
pub(crate) fn directory_identity(path: &Path) -> Result<Identity> {
    Ok(open_checked(path, true)?.1.identity)
}
pub(crate) fn verify_transaction_package(
    path: &Path,
    pins: &InstallPins,
    expected: &Identity,
) -> Result<InstalledPackage> {
    ensure!(
        &directory_identity(path)? == expected,
        "transaction directory identity changed"
    );
    let manifest = verify_at(path, pins, &pins.target)?;
    ensure!(
        &directory_identity(path)? == expected,
        "transaction directory identity changed"
    );
    Ok(InstalledPackage {
        directory: path.to_owned(),
        entrypoint: path.join(&manifest.entrypoint),
        manifest,
        pins: pins.clone(),
    })
}

/// Inspect host metadata without creating missing directories or following links. Used by offline
/// configuration/status paths as well as mutations; absent configuration is an ordinary state.
pub(crate) fn read_host_file(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    guard(path)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspecting host metadata"),
        Ok(_) => {}
    }
    let (mut file, snapshot) = open_checked(path, false)?;
    ensure!(
        snapshot.bytes <= limit as u64,
        "host metadata exceeds byte bound"
    );
    let bytes = read_bounded(&mut file, limit)?;
    recheck(path, &file, &snapshot, false)?;
    Ok(Some(bytes))
}

/// Inspect a host-owned directory without creating it or accepting links/special entries.
pub(crate) fn host_directory_exists(path: &Path) -> Result<bool> {
    guard(path)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspecting host directory"),
        Ok(_) => {}
    }
    open_checked(path, true)?;
    Ok(true)
}

/// Create a host-owned private storage root without following links or hardening unrelated parents.
pub(crate) fn ensure_private_root(path: &Path) -> Result<()> {
    guard(path)?;
    if path.try_exists()? {
        open_checked(path, true)?;
        return Ok(());
    }
    let parent = path
        .parent()
        .context("private storage root has no parent")?;
    ensure_private_root(parent)?;
    create_private_dir(path)
}

/// Remove only a reverified package's declared payload and host metadata. No recursive deletion,
/// provider execution or access to its data root. On partial failure, the manager keeps it disabled
/// and retains pins for explicit recovery; unknown entries are never deleted. The pinned package
/// target is checked for integrity, not compatibility with the current Core: removal runs no code.
pub(crate) fn uninstall_verified(plugins_root: &Path, pins: &InstallPins) -> Result<()> {
    let package = verify_installed(plugins_root, pins, &pins.target)?;
    let before = inventory(&package.directory, &package.manifest, true)?;
    for (relative, expected) in &before {
        let path = package.directory.join(relative);
        let (handle, actual) = open_checked(&path, false)?;
        ensure!(
            &actual == expected,
            "package changed before removal; refusing deletion"
        );
        drop(handle);
        fs::remove_file(&path)
            .context("package removal incomplete; package must remain disabled")?;
    }
    let mut directories: Vec<_> = expected_dirs(&package.manifest).into_iter().collect();
    directories.sort_by_key(|relative| std::cmp::Reverse(relative.split('/').count()));
    for relative in directories {
        let path = package.directory.join(relative);
        open_checked(&path, true)?;
        fs::remove_dir(&path)?;
    }
    open_checked(&package.directory, true)?;
    fs::remove_dir(&package.directory)?;
    sync_after_publication(plugins_root)
}

/// Inspect the classic ZIP envelope before ZipArchive can allocate its central index. The
/// supported subset has no Zip64, comments, SFX prefix, extra fields, descriptors or hidden local
/// records. Every central record must correspond to one contiguous, identical local record.
fn preflight_zip(file: &mut File, bytes: u64) -> Result<()> {
    ensure!(bytes >= 22, "truncated ZIP envelope");
    file.seek(SeekFrom::Start(bytes - 22))?;
    let mut end = [0u8; 22];
    file.read_exact(&mut end)?;
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    ensure!(
        &end[..4] == b"PK\x05\x06"
            && u16_at(&end, 4) == 0
            && u16_at(&end, 6) == 0
            && u16_at(&end, 20) == 0,
        "ZIP comments/multidisk/trailing data unsupported"
    );
    let count = u16_at(&end, 10) as usize;
    let central_bytes = u32_at(&end, 12) as u64;
    let central = u32_at(&end, 16) as u64;
    ensure!(
        count > 0 && count <= MAX_TREE_ENTRIES && u16_at(&end, 8) as usize == count,
        "ZIP entry count limit"
    );
    ensure!(
        central_bytes <= 4 * 1024 * 1024 && central.checked_add(central_bytes) == Some(bytes - 22),
        "ZIP central-directory byte limit/offset"
    );
    let mut position = central;
    let mut next_local = 0u64;
    let mut namespace = Namespace::default();
    for _ in 0..count {
        ensure!(
            position + 46 <= central + central_bytes,
            "truncated ZIP central record"
        );
        file.seek(SeekFrom::Start(position))?;
        let mut record = [0u8; 46];
        file.read_exact(&mut record)?;
        ensure!(
            &record[..4] == b"PK\x01\x02",
            "invalid ZIP central signature"
        );
        let flags = u16_at(&record, 8);
        let codec = u16_at(&record, 10);
        ensure!(
            flags & !0x0806 == 0 && matches!(codec, 0 | 8),
            "ZIP encryption/descriptors/codec unsupported"
        );
        ensure!(
            u16_at(&record, 6) <= 20
                && u16_at(&record, 30) == 0
                && u16_at(&record, 32) == 0
                && u16_at(&record, 34) == 0,
            "ZIP extras/comments/Zip64 unsupported"
        );
        let name_len = u16_at(&record, 28) as usize;
        ensure!(
            name_len > 0 && name_len <= manifest::MAX_PATH_BYTES + 1,
            "ZIP filename limit"
        );
        let mut name = vec![0u8; name_len];
        file.read_exact(&mut name)?;
        let text = std::str::from_utf8(&name)?;
        let directory = text.ends_with('/');
        namespace.insert(
            if directory {
                &text[..text.len() - 1]
            } else {
                text
            },
            directory,
        )?;
        // Only DOS readonly/hidden/archive/directory/normal bits; system/volume/device/reparse
        // attributes are unsupported. Hidden entries are rejected even with an ordinary name.
        let attrs = u32_at(&record, 38);
        ensure!(
            attrs & 0xffff & !0xb1 == 0,
            "ZIP DOS hidden/device/special attributes unsupported"
        );
        let local = u32_at(&record, 42) as u64;
        ensure!(
            local == next_local && local + 30 <= central,
            "ZIP local entries overlap, have gaps or hidden records"
        );
        file.seek(SeekFrom::Start(local))?;
        let mut header = [0u8; 30];
        file.read_exact(&mut header)?;
        ensure!(
            &header[..4] == b"PK\x03\x04"
                && header[4..26] == record[6..28]
                && u16_at(&header, 26) as usize == name_len
                && u16_at(&header, 28) == 0,
            "ZIP local/central headers disagree or have unsupported extras"
        );
        let mut local_name = vec![0u8; name_len];
        file.read_exact(&mut local_name)?;
        ensure!(local_name == name, "ZIP local/central filename mismatch");
        next_local = local
            .checked_add(30 + name_len as u64)
            .and_then(|n| n.checked_add(u32_at(&record, 20) as u64))
            .context("ZIP offset overflow")?;
        ensure!(next_local <= central, "ZIP data overlaps central directory");
        position += 46 + name_len as u64;
    }
    ensure!(
        position == central + central_bytes && next_local == central,
        "ZIP hidden or incomplete records"
    );
    Ok(())
}

fn hash_file(file: &mut File, bytes: u64) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut remaining = bytes;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let cap = remaining.min(buffer.len() as u64) as usize;
        let n = file.read(&mut buffer[..cap])?;
        ensure!(n > 0, "truncated source file");
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    ensure!(file.read(&mut buffer[..1])? == 0, "source file grew");
    Ok(manifest::hex_digest(&hash.finalize()))
}

#[cfg(test)]
mod expected_identity_tests {
    use super::*;

    #[test]
    fn directory_and_zip_wrong_provider_identity_do_not_publish_payloads() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let plugins = fixture.path().join("plugins");
        let source = fixture.path().join("source");
        ensure_private_root(&plugins).unwrap();
        ensure_private_root(&source).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version":1,"id":"other-memory","name":"Synthetic package","version":"0.1.0",
            "kind":"memory","protocol":{"major":1,"min_minor":0,"max_minor":0},
            "target":"x86_64-pc-windows-msvc","entrypoint":"provider.exe",
            "capabilities":[],"permissions":{"network":false,"host_llm":false},
            "files":[{"path":"provider.exe","bytes":1,"sha256":"a".repeat(64)}]
        }))
        .unwrap();
        fs::write(source.join(MANIFEST_NAME), &bytes).unwrap();
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file(
                MANIFEST_NAME,
                zip::write::SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Stored)
                    .unix_permissions(0o600),
            )
            .unwrap();
        archive.write_all(&bytes).unwrap();
        let archive_path = fixture.path().join("package.zip");
        fs::write(&archive_path, archive.finish().unwrap().into_inner()).unwrap();
        let source_archive = fs::read(&archive_path).unwrap();
        for result in [
            install_directory_expected(&plugins, &source, "x86_64-pc-windows-msvc", Some("mnemos")),
            install_zip_expected(
                &plugins,
                &archive_path,
                "x86_64-pc-windows-msvc",
                Some("mnemos"),
            ),
        ] {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("expected Memory provider mnemos")
            );
            assert_eq!(fs::read_dir(&plugins).unwrap().count(), 0);
        }
        assert_eq!(fs::read(source.join(MANIFEST_NAME)).unwrap(), bytes);
        assert_eq!(fs::read(&archive_path).unwrap(), source_archive);
    }
}
