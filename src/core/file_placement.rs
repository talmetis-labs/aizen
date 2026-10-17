//! Purpose-aware native destinations. Planning is read-only; only `ensure` creates directories.
//!
//! These guards detect ordinary approval drift, not kernel-atomic isolation from a hostile actor
//! racing path operations. Project paths retain their explicit meaning and existing link semantics;
//! managed paths reject links/reparse points throughout their ancestry.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::core::persist::FileFingerprint;
use crate::core::repo_lock::{LockMode, RepoTxnLock};

const MAX_FINGERPRINT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ANCESTORS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePurpose {
    Project,
    Artifact,
    Scratch,
}

impl FilePurpose {
    /// Missing is deliberately not inferred here: the native tool enforces purpose for new files.
    pub fn parse_optional(value: Option<&Value>) -> Result<Option<Self>> {
        match value {
            None => Ok(None),
            Some(Value::String(value)) => match value.as_str() {
                "project" => Ok(Some(Self::Project)),
                "artifact" => Ok(Some(Self::Artifact)),
                "scratch" => Ok(Some(Self::Scratch)),
                _ => bail!("purpose must be project, artifact, or scratch (for example purpose: \"project\")"),
            },
            Some(_) => bail!("purpose must be a string: project, artifact, or scratch"),
        }
    }
}

/// Shared ownership lasts until every context and prepared destination for the run is dropped.
#[derive(Debug, Default)]
struct Ownership {
    scratch_lock: Option<RepoTxnLock>,
    /// Only directories created by approved ensures are allowed to replace a missing ancestor.
    created: HashMap<PathBuf, NodeStamp>,
    namespaces: HashMap<ScratchIdentity, String>,
    namespace_stamps: HashMap<PathBuf, NodeStamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
struct ScratchIdentity {
    workspace: String,
    scope: String,
}

impl ScratchIdentity {
    fn new(root: &Path, scope: &str) -> Self {
        let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Self {
            workspace: crate::core::config::normalize_path_key(&canonical),
            scope: scope.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
struct ScratchNamespace {
    metadata: PathBuf,
    content: Vec<u8>,
}

impl ScratchNamespace {
    fn new(
        run_root: &Path,
        run_id: &str,
        handle: &str,
        identity: &ScratchIdentity,
    ) -> Result<Self> {
        Ok(Self {
            metadata: run_root.join(format!(".namespace-{handle}.json")),
            content: serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "run": run_id,
                "identity": identity,
            }))?,
        })
    }
}

fn scratch_run_component(run_id: &str) -> String {
    if run_id.len() == 32
        && run_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        // Encode every bit; the prefix separates generated IDs from explicit caller labels.
        let id = u128::from_str_radix(run_id, 16).expect("validated hexadecimal run id");
        format!("r-{}", crate::core::scratch::base36(id))
    } else {
        format!("label-{run_id}")
    }
}

#[derive(Debug, Clone)]
pub struct PlacementContext {
    workspace: PathBuf,
    home: PathBuf,
    scratch_process: PathBuf,
    run_id: String,
    artifact: PathBuf,
    scratch: PathBuf,
    scratch_run: PathBuf,
    scratch_namespace: ScratchNamespace,
    owner: Arc<Mutex<Ownership>>,
}

impl PlacementContext {
    /// Snapshot explicit roots and allocate a run id without creating any directories.
    pub fn new(workspace: PathBuf, home: PathBuf) -> Result<Self> {
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|error| anyhow::anyhow!("cannot allocate file-placement run id: {error}"))?;
        let run_id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        Self::with_paths(
            workspace,
            home,
            crate::core::scratch::planned_dir().to_path_buf(),
            run_id,
        )
    }

    /// Explicit roots/run id for hermetic callers. Does not consult cwd, config, or the filesystem
    /// except the read-only workspace identity normalization used by artifact discovery.
    pub fn with_paths(
        workspace: PathBuf,
        home: PathBuf,
        scratch_process: PathBuf,
        run_id: impl Into<String>,
    ) -> Result<Self> {
        if !workspace.is_absolute() || !scratch_process.is_absolute() {
            bail!("placement workspace and process scratch root must be explicit absolute paths");
        }
        let home = if home.is_absolute() {
            home
        } else {
            workspace.join(home)
        };
        let run_id = run_id.into();
        validate_component(&run_id)?;
        let scratch_component = scratch_run_component(&run_id);
        validate_component(&scratch_component)?;
        let scope = "root".to_string();
        let artifact = artifact_workspace_root(&workspace, &home)
            .join(&run_id)
            .join(&scope);
        let scratch_run = scratch_process.join("managed").join(scratch_component);
        let identity = ScratchIdentity::new(&workspace, "root");
        let scratch = scratch_run.join("root");
        let scratch_namespace = ScratchNamespace::new(&scratch_run, &run_id, "root", &identity)?;
        let mut ownership = Ownership::default();
        ownership.namespaces.insert(identity, "root".into());
        Ok(Self {
            workspace,
            home,
            scratch_process,
            run_id,
            artifact,
            scratch,
            scratch_run,
            scratch_namespace,
            owner: Arc::new(Mutex::new(ownership)),
        })
    }

    /// A child shares the run and its lifetime ownership, not the parent's destination directory.
    /// Hash the resource label rather than interpreting '/' or ':' in it as filesystem syntax.
    pub fn fork_for(&self, root: PathBuf, scope: &str) -> Result<Self> {
        if !root.is_absolute() || scope.is_empty() {
            bail!("child placement needs an absolute workspace and nonempty resource scope");
        }
        let identity = ScratchIdentity::new(&root, scope);
        let handle = {
            let mut owner = self
                .owner
                .lock()
                .map_err(|_| anyhow::anyhow!("placement ownership lock poisoned"))?;
            if let Some(handle) = owner.namespaces.get(&identity) {
                handle.clone()
            } else {
                let handle = format!("agent-{}", owner.namespaces.len());
                if owner.namespaces.values().any(|other| other == &handle) {
                    bail!("scratch namespace handle collision; prepare again");
                }
                owner.namespaces.insert(identity.clone(), handle.clone());
                handle
            }
        };
        let artifact_scope = format!("child-{}", digest_hex(scope.as_bytes()));
        let artifact = artifact_workspace_root(&root, &self.home)
            .join(&self.run_id)
            .join(&artifact_scope);
        let scratch = self.scratch_run.join(&handle);
        let scratch_namespace =
            ScratchNamespace::new(&self.scratch_run, &self.run_id, &handle, &identity)?;
        Ok(Self {
            workspace: root,
            home: self.home.clone(),
            scratch_process: self.scratch_process.clone(),
            run_id: self.run_id.clone(),
            artifact,
            scratch,
            scratch_run: self.scratch_run.clone(),
            scratch_namespace,
            owner: self.owner.clone(),
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace
    }
    pub fn home_root(&self) -> &Path {
        &self.home
    }
    #[cfg(test)]
    pub fn artifact_root(&self) -> &Path {
        &self.artifact
    }
    pub fn scratch_root(&self) -> &Path {
        &self.scratch
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Display-only provenance from this owned namespace, never from tool output or purpose JSON.
    pub(crate) fn scratch_display(&self, path: &Path) -> Option<DisplayPath> {
        let relative = path.strip_prefix(&self.scratch).ok()?;
        let name = relative.to_str()?;
        let destination = prepare_destination(Some(self), name, Some(FilePurpose::Scratch)).ok()?;
        if destination.actual_path() != path {
            return None;
        }
        let namespace = self.scratch.file_name()?.to_str()?;
        let relative = name.replace('\\', "/");
        let label = if namespace == "root" {
            format!("scratch/{relative}")
        } else {
            format!("scratch/{namespace}/{relative}")
        };
        Some(DisplayPath {
            label,
            argument: path.to_path_buf(),
            actual: path.to_path_buf(),
        })
    }
}

/// A short UI label bound by placement preparation to its real absolute destination.
#[derive(Debug, Clone)]
pub(crate) struct DisplayPath {
    label: String,
    argument: PathBuf,
    actual: PathBuf,
}

impl DisplayPath {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
    pub(crate) fn actual(&self) -> &Path {
        &self.actual
    }
    pub(crate) fn matches_argument(&self, argument: &Path) -> bool {
        self.argument == argument
    }
    /// Bind the prepared read's original spelling without changing its real link/copy target.
    pub(crate) fn with_argument(mut self, argument: PathBuf) -> Self {
        self.argument = argument;
        self
    }
}

/// All durable output runs for the supplied workspace, without reading live config or creating it.
pub fn artifact_workspace_root(root: &Path, home: &Path) -> PathBuf {
    let home = if home.is_absolute() {
        home.to_path_buf()
    } else {
        root.join(home)
    };
    home.join("artifacts").join(workspace_namespace(root))
}

fn workspace_namespace(root: &Path) -> String {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let key = crate::core::config::normalize_path_key(&canonical);
    // Reuse normalized identity, with a longer digest than legacy display slugs for managed paths.
    digest_hex(key.as_bytes())
}

fn digest_hex(bytes: &[u8]) -> String {
    FileFingerprint::for_bytes(bytes).sha256[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Validate on every host: a Windows-unsafe name must not become legal merely on Unix.
pub fn validate_managed_name(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() || raw.starts_with(['/', '\\']) || Path::new(raw).is_absolute() {
        bail!("managed destination must be a safe relative name, for example reports/result.txt");
    }
    let mut result = PathBuf::new();
    for component in raw.split(['/', '\\']) {
        validate_component(component)?;
        result.push(component);
    }
    Ok(result)
}

fn validate_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with(['.', ' '])
        || component.len() > 255
        || component.chars().any(|c| {
            c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*' | '/' | '\\')
        })
    {
        bail!("unsafe managed path component {component:?}; use a relative name without traversal, streams, or empty components");
    }
    let stem = component.split('.').next().unwrap_or(component);
    let device = stem.trim_end_matches(' ').to_uppercase();
    let numbered = device
        .strip_prefix("COM")
        .or_else(|| device.strip_prefix("LPT"));
    if matches!(
        device.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered.is_some_and(|suffix| {
        matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    }) || component.eq_ignore_ascii_case(".ownership.lock")
        || component.eq_ignore_ascii_case(crate::core::scratch::OWNER_MARKER)
    {
        bail!("reserved managed path component {component:?}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeStamp {
    identity: (u64, u64),
    followed_identity: Option<(u64, u64)>,
    kind: u8,
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    readonly: bool,
}

impl NodeStamp {
    pub(crate) fn identity(&self) -> (u64, u64) {
        self.identity
    }

    pub(crate) fn capture(path: &Path, md: &Metadata, allow_links: bool) -> Result<Self> {
        let link = is_link_or_reparse(md);
        if link && !allow_links {
            bail!(
                "managed destination traverses a link/reparse point: {}",
                path.display()
            );
        }
        let followed_identity = if link {
            let followed =
                fs::metadata(path).with_context(|| format!("resolving {}", path.display()))?;
            Some(file_identity(path, &followed, true)?)
        } else {
            None
        };
        Ok(Self {
            identity: file_identity(path, md, false)?,
            followed_identity,
            kind: if link {
                3
            } else if md.is_dir() {
                2
            } else if md.is_file() {
                1
            } else {
                4
            },
            len: md.len(),
            modified: md.modified().ok(),
            created: md.created().ok(),
            readonly: md.permissions().readonly(),
        })
    }

    fn same_directory(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.followed_identity == other.followed_identity
            && self.kind == other.kind
            && self.created == other.created
    }
}

pub(crate) fn is_link_or_reparse(md: &Metadata) -> bool {
    if md.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(unix)]
fn file_identity(_path: &Path, md: &Metadata, _follow: bool) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((md.dev(), md.ino()))
}

#[cfg(windows)]
fn file_identity(path: &Path, _md: &Metadata, follow: bool) -> Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let name: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let flags = FILE_FLAG_BACKUP_SEMANTICS
        | if follow {
            0
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        };
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("opening placement identity handle");
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    let error = if ok == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    if let Some(error) = error {
        return Err(error).context("reading placement identity");
    }
    Ok((
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_path: &Path, _md: &Metadata, _follow: bool) -> Result<(u64, u64)> {
    bail!("file placement identity guards are unavailable on this platform")
}

/// Read existing evidence through a bounded, checked descriptor. No creation, hardening or writes.
/// Path/descriptor rechecks detect ordinary drift, not hostile kernel-atomic path substitution.
pub(crate) fn read_existing_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    limit
        .checked_add(1)
        .context("existing file byte bound overflow")?;
    let resource = crate::core::workspace_txn::NativeResource::resolve(path)?;
    let md = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        md.is_file() && !is_link_or_reparse(&md) && md.len() <= limit,
        "existing evidence must be a bounded regular non-link file"
    );
    let stamp = NodeStamp::capture(path, &md, false)?;
    resource.verify()?;
    let file = open_existing_read_only(path)?;
    read_existing_regular_descriptor(&resource, &stamp, file, limit)
}

fn read_existing_regular_descriptor(
    resource: &crate::core::workspace_txn::NativeResource,
    stamp: &NodeStamp,
    mut file: fs::File,
    limit: u64,
) -> Result<Vec<u8>> {
    let read_limit = limit
        .checked_add(1)
        .context("existing file byte bound overflow")?;
    anyhow::ensure!(
        stamp.len <= limit && existing_regular_stamp(&file)? == *stamp,
        "existing evidence descriptor differs from selected file"
    );
    resource.verify()?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(stamp.len).context("existing file length overflow")?)?;
    (&mut file).take(read_limit).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 == stamp.len && existing_regular_stamp(&file)? == *stamp,
        "existing evidence descriptor changed during inspection"
    );
    resource.verify()?;
    anyhow::ensure!(
        NodeStamp::capture(
            resource.path(),
            &fs::symlink_metadata(resource.path())?,
            false
        )? == *stamp,
        "existing evidence pathname changed during inspection"
    );
    Ok(bytes)
}

fn open_existing_read_only(path: &Path) -> Result<fs::File> {
    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))]
    {
        let mut options = OpenOptions::new();
        options.read(true).create(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // O_NOFOLLOW | O_NONBLOCK: a substituted FIFO must not stall before descriptor checks.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            options.custom_flags(0x20000 | 0x800);
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            options.custom_flags(0x100 | 0x4);
        }
        Ok(options.open(path)?)
    }
    #[cfg(not(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )))]
    {
        let _ = path;
        bail!("existing-only evidence reads unavailable on this platform")
    }
}

fn existing_regular_stamp(file: &fs::File) -> Result<NodeStamp> {
    let md = file.metadata()?;
    anyhow::ensure!(
        md.is_file() && !is_link_or_reparse(&md),
        "existing evidence descriptor must be a regular non-link file"
    );
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            md.nlink() == 1,
            "existing evidence descriptor must be single-link"
        );
        (md.dev(), md.ino())
    };
    #[cfg(windows)]
    let identity = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading existing evidence identity");
        }
        anyhow::ensure!(
            info.nNumberOfLinks == 1 && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "existing evidence descriptor must be a non-link single-link file"
        );
        (
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        )
    };
    #[cfg(not(any(unix, windows)))]
    let identity = {
        bail!("existing evidence descriptor identity unavailable on this platform");
    };
    Ok(NodeStamp {
        identity,
        followed_identity: None,
        kind: 1,
        len: md.len(),
        modified: md.modified().ok(),
        created: md.created().ok(),
        readonly: md.permissions().readonly(),
    })
}

#[derive(Debug, Clone)]
struct Ancestor {
    path: PathBuf,
    baseline: Option<NodeStamp>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDestination {
    pub actual: PathBuf,
    pub managed_root: Option<PathBuf>,
    expected: Option<NodeStamp>,
    fingerprint: Option<FileFingerprint>,
    ancestors: Vec<Ancestor>,
    owner: Option<Arc<Mutex<Ownership>>>,
    scratch_process: Option<PathBuf>,
    scratch_lock_root: Option<PathBuf>,
    scratch_namespace: Option<(ScratchNamespace, Option<NodeStamp>)>,
}

pub fn prepare_destination(
    ctx: Option<&PlacementContext>,
    raw: &str,
    purpose: Option<FilePurpose>,
) -> Result<ResolvedDestination> {
    if raw.is_empty() || raw.contains('\0') {
        bail!("destination path must not be empty or contain NUL");
    }
    let managed = matches!(purpose, Some(FilePurpose::Artifact | FilePurpose::Scratch));
    let (actual, managed_root, scratch_lock_root) = if managed {
        let ctx = ctx.context("managed purpose needs a pinned placement context; use an explicit project path or retry through the executor")?;
        let relative = validate_managed_name(raw)?;
        let root = if purpose == Some(FilePurpose::Artifact) {
            &ctx.artifact
        } else {
            &ctx.scratch
        };
        (
            root.join(relative),
            Some(root.clone()),
            if purpose == Some(FilePurpose::Scratch) {
                Some(ctx.scratch_run.clone())
            } else {
                None
            },
        )
    } else {
        let path = PathBuf::from(raw);
        let actual = if path.is_absolute() {
            path
        } else {
            ctx.context("relative project destination needs a pinned workspace or an explicit absolute path")?.workspace.join(path)
        };
        (actual, None, None)
    };
    let mut paths: Vec<PathBuf> = actual
        .parent()
        .context("destination must have a parent")?
        .ancestors()
        .map(Path::to_path_buf)
        .collect();
    if paths.len() > MAX_ANCESTORS {
        bail!("destination ancestry exceeds placement safety limit");
    }
    paths.reverse();
    let mut ancestors = Vec::with_capacity(paths.len());
    for path in paths {
        let baseline = optional_stamp(&path, !managed)?;
        if let Some(stamp) = &baseline {
            if stamp.kind != 2 && !(stamp.kind == 3 && fs::metadata(&path)?.is_dir()) {
                bail!("destination parent is not a directory: {}", path.display());
            }
        }
        ancestors.push(Ancestor { path, baseline });
    }
    let expected = optional_stamp(&actual, !managed)?;
    if managed && expected.as_ref().is_some_and(|stamp| stamp.kind == 4) {
        bail!(
            "managed destination is not a regular file or directory: {}",
            actual.display()
        );
    }
    let fingerprint = bounded_fingerprint(&actual, expected.as_ref())?;
    // Hashing is bounded, but still must not bind content read across an identity change.
    if optional_stamp(&actual, !managed)? != expected {
        bail!("destination changed during preparation; prepare again");
    }
    let scratch_namespace = ctx
        .filter(|_| scratch_lock_root.is_some())
        .map(|ctx| {
            let namespace = ctx.scratch_namespace.clone();
            let baseline = optional_stamp(&namespace.metadata, false)?;
            if baseline.is_none() && optional_stamp(&ctx.scratch, false)?.is_some() {
                bail!("scratch namespace exists without identity metadata; prepare again");
            }
            verify_namespace_content(&namespace, baseline.as_ref())?;
            Ok::<_, anyhow::Error>((namespace, baseline))
        })
        .transpose()?;
    let result = ResolvedDestination {
        actual,
        managed_root,
        expected,
        fingerprint,
        ancestors,
        scratch_namespace,
        owner: Some(
            ctx.map(|ctx| ctx.owner.clone())
                .unwrap_or_else(|| Arc::new(Mutex::new(Ownership::default()))),
        ),
        scratch_process: ctx
            .filter(|_| scratch_lock_root.is_some())
            .map(|ctx| ctx.scratch_process.clone()),
        scratch_lock_root,
    };
    result.verify()?;
    Ok(result)
}

fn optional_stamp(path: &Path, allow_links: bool) -> Result<Option<NodeStamp>> {
    match fs::symlink_metadata(path) {
        Ok(md) => NodeStamp::capture(path, &md, allow_links).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("checking destination {}", path.display()))
        }
    }
}

fn verify_namespace_content(namespace: &ScratchNamespace, stamp: Option<&NodeStamp>) -> Result<()> {
    let Some(stamp) = stamp else {
        return Ok(());
    };
    if stamp.kind != 1 || stamp.len != namespace.content.len() as u64 {
        bail!("scratch namespace metadata is invalid; prepare again");
    }
    let mut bytes = Vec::with_capacity(namespace.content.len());
    fs::File::open(&namespace.metadata)?
        .take(namespace.content.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes != namespace.content
        || optional_stamp(&namespace.metadata, false)?.as_ref() != Some(stamp)
    {
        bail!("scratch namespace identity changed; prepare again");
    }
    Ok(())
}

fn bounded_fingerprint(path: &Path, stamp: Option<&NodeStamp>) -> Result<Option<FileFingerprint>> {
    let Some(stamp) = stamp else {
        return Ok(Some(FileFingerprint::missing()));
    };
    if stamp.kind != 1 || stamp.len > MAX_FINGERPRINT_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(stamp.len as usize);
    fs::File::open(path)?
        .take(MAX_FINGERPRINT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FINGERPRINT_BYTES {
        bail!("destination grew during preparation; prepare again");
    }
    Ok(Some(FileFingerprint::for_bytes(&bytes)))
}

impl ResolvedDestination {
    pub fn actual_path(&self) -> &Path {
        &self.actual
    }
    pub fn is_managed(&self) -> bool {
        self.managed_root.is_some()
    }
    pub fn exists_at_preparation(&self) -> bool {
        self.expected.is_some()
    }
    /// Exact preview baseline for regular files up to 4 MiB. Large files/directories return None;
    /// tools reading a full preview must bind that read's own persistence/CAS fingerprint as well.
    #[cfg(test)]
    pub fn fingerprint(&self) -> Option<&FileFingerprint> {
        self.fingerprint.as_ref()
    }

    pub fn verify(&self) -> Result<()> {
        let owner = self
            .owner
            .as_ref()
            .map(|owner| {
                owner
                    .lock()
                    .map_err(|_| anyhow::anyhow!("placement ownership lock poisoned"))
            })
            .transpose()?;
        self.verify_with(owner.as_deref())
    }

    fn verify_with(&self, owner: Option<&Ownership>) -> Result<()> {
        if let Some((namespace, baseline)) = &self.scratch_namespace {
            let current = optional_stamp(&namespace.metadata, false)?;
            let expected = baseline.as_ref().or_else(|| {
                owner.and_then(|owner| owner.namespace_stamps.get(&namespace.metadata))
            });
            if current.as_ref() != expected {
                bail!("scratch namespace metadata changed since preparation; prepare again");
            }
            verify_namespace_content(namespace, current.as_ref())?;
        }
        for ancestor in &self.ancestors {
            let current = optional_stamp(&ancestor.path, !self.is_managed())?;
            let expected = ancestor
                .baseline
                .as_ref()
                .or_else(|| owner.and_then(|owner| owner.created.get(&ancestor.path)));
            match (expected, current.as_ref()) {
                (None, None) => {}
                (Some(expected), Some(current)) if expected.same_directory(current) => {}
                _ => bail!(
                    "destination ancestor changed since preparation: {}; prepare and approve again",
                    ancestor.path.display()
                ),
            }
        }
        if optional_stamp(&self.actual, !self.is_managed())? != self.expected {
            bail!("destination existence or identity changed since preparation: {}; prepare and approve again", self.actual.display());
        }
        if let Some(expected) = &self.fingerprint {
            if bounded_fingerprint(&self.actual, self.expected.as_ref())?.as_ref() != Some(expected)
            {
                bail!(
                    "destination content changed since preparation: {}; prepare and approve again",
                    self.actual.display()
                );
            }
        }
        Ok(())
    }

    /// Post-approval only. Managed directories are created one checked component at a time, never
    /// with `create_dir_all` on model-selected paths. Project parent creation remains tool policy.
    pub fn ensure(&self) -> Result<()> {
        if !self.is_managed() {
            return self.verify();
        }
        self.ensure_parents()
    }

    /// Post-approval FileMove `create_dirs` support. Project writes do not call this method.
    pub fn ensure_project_parents(&self) -> Result<()> {
        if self.is_managed() {
            bail!("project parent ensure cannot be used for managed destinations");
        }
        self.ensure_parents()
    }

    fn ensure_parents(&self) -> Result<()> {
        let Some(owner) = &self.owner else {
            return self.verify();
        };
        let mut owner = owner
            .lock()
            .map_err(|_| anyhow::anyhow!("placement ownership lock poisoned"))?;
        self.verify_with(Some(&owner))?;
        let cancel = crate::core::cancel::current();
        if cancel
            .as_ref()
            .is_some_and(crate::core::cancel::TurnCancel::is_cancelled)
        {
            bail!("cancelled before creating managed destination directories");
        }
        for ancestor in &self.ancestors {
            if cancel
                .as_ref()
                .is_some_and(crate::core::cancel::TurnCancel::is_cancelled)
            {
                bail!("cancelled while creating managed destination directories");
            }
            self.verify_with(Some(&owner))?;
            if optional_stamp(&ancestor.path, !self.is_managed())?.is_none() {
                // Previous components have already been checked/created. Refuse an unexpected race
                // rather than accepting AlreadyExists and binding an attacker-controlled directory.
                fs::create_dir(&ancestor.path).with_context(|| {
                    format!("creating managed directory {}", ancestor.path.display())
                })?;
                let stamp = optional_stamp(&ancestor.path, false)?
                    .context("managed directory disappeared after creation")?;
                if stamp.kind != 2 {
                    bail!("managed parent is not a directory");
                }
                owner.created.insert(ancestor.path.clone(), stamp);
            }
            if self.scratch_process.as_ref() == Some(&ancestor.path) {
                crate::core::scratch::mark_owned(&ancestor.path)?;
            }
            if self.scratch_lock_root.as_ref() == Some(&ancestor.path)
                && owner.scratch_lock.is_none()
            {
                let path = ancestor.path.join(".ownership.lock");
                if let Ok(md) = fs::symlink_metadata(&path) {
                    if is_link_or_reparse(&md) || !md.is_file() {
                        bail!("invalid managed scratch ownership lock: {}", path.display());
                    }
                }
                let lock = RepoTxnLock::acquire_mode(
                    &path,
                    LockMode::Exclusive,
                    Duration::from_secs(2),
                    cancel.as_ref(),
                )?;
                // No marker or lock read is trusted to authorize traversal; recheck directory chain.
                owner.scratch_lock = Some(lock);
                self.verify_with(Some(&owner))?;
            }
            if self.scratch_lock_root.as_ref() == Some(&ancestor.path) {
                if let Some((namespace, _)) = &self.scratch_namespace {
                    self.verify_with(Some(&owner))?;
                    if optional_stamp(&namespace.metadata, false)?.is_none() {
                        let mut options = OpenOptions::new();
                        options.write(true).create_new(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            options.mode(0o600);
                        }
                        let mut file = options
                            .open(&namespace.metadata)
                            .context("creating scratch namespace identity metadata")?;
                        file.write_all(&namespace.content)?;
                        file.sync_all()?;
                        let stamp = optional_stamp(&namespace.metadata, false)?
                            .context("scratch namespace metadata disappeared")?;
                        verify_namespace_content(namespace, Some(&stamp))?;
                        owner
                            .namespace_stamps
                            .insert(namespace.metadata.clone(), stamp);
                    }
                    self.verify_with(Some(&owner))?;
                }
            }
        }
        self.verify_with(Some(&owner))
    }
}

#[cfg(test)]
#[path = "file_placement_tests.rs"]
pub(crate) mod tests;
