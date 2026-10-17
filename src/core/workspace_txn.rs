//! Canonical workspace identity and ordered cross-process resource leases.
//!
//! The identity distinguishes linked Git worktrees that share one repository store. Stable hashed
//! keys keep lock paths short, cross-platform, and free of source paths or credentials.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::repo_lock::{LockMode, RepoTxnLock};

const LOCK_PROTOCOL: u32 = 1;

#[cfg(test)]
thread_local! {
    static TEST_LOCK_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn lock_home() -> PathBuf {
    #[cfg(test)]
    if let Some(home) = TEST_LOCK_HOME.with(|home| home.borrow().clone()) {
        return home;
    }
    crate::core::config::aizen_home()
}

/// Thread-local fixture override, restored on return or unwind. Worker threads must opt in with the
/// same fixture root explicitly; this never changes process-global environment or production paths.
#[cfg(test)]
pub(crate) fn with_test_lock_home<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LOCK_HOME.with(|home| {
                *home.borrow_mut() = self.0.take();
            });
        }
    }
    assert!(
        root.is_absolute() && root.is_dir(),
        "lock home must be an existing owned fixture"
    );
    let previous = TEST_LOCK_HOME.with(|home| home.replace(Some(root.to_path_buf())));
    let _restore = Restore(previous);
    f()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub repo_id: String,
    pub worktree_id: String,
    pub canonical_root: PathBuf,
    pub canonical_git_dir: Option<PathBuf>,
    pub canonical_common_git_dir: Option<PathBuf>,
}

impl WorkspaceIdentity {
    /// [`discover`] with a process-wide per-root cache. Discovery costs three git spawns and the
    /// writer lease runs it on EVERY destructive tool call — within one process the answer never
    /// changes for a given root, and lock correctness wants all callers to AGREE on the key more
    /// than it wants mid-process freshness (a `git init` mid-run re-keys on the next process).
    pub fn discover_cached(root: &Path) -> Result<Self> {
        static CACHE: std::sync::Mutex<
            Option<std::collections::HashMap<String, WorkspaceIdentity>>,
        > = std::sync::Mutex::new(None);
        // The nearest `.git` marker is part of the key: a `git init` mid-process must re-key —
        // an aizen started before it and one after must still agree on the lock path, or the
        // writer lease stops excluding across processes. One bounded stat-walk, no git spawn.
        let marker = {
            let mut cur = Some(root);
            let mut found = String::new();
            while let Some(p) = cur {
                if p.join(".git").exists() {
                    found = normalized_path(p);
                    break;
                }
                cur = p.parent();
            }
            found
        };
        let key = format!("{}|{marker}", normalized_path(root));
        if let Ok(guard) = CACHE.lock() {
            if let Some(id) = guard.as_ref().and_then(|m| m.get(&key)) {
                return Ok(id.clone());
            }
        }
        let id = Self::discover(root)?;
        if let Ok(mut guard) = CACHE.lock() {
            guard
                .get_or_insert_with(Default::default)
                .insert(key, id.clone());
        }
        Ok(id)
    }

    pub fn discover(root: &Path) -> Result<Self> {
        let canonical_root = canonical_existing_or_parent(root)?;
        let top = git_path(root, ["rev-parse", "--show-toplevel"]);
        let git_dir = git_path(root, ["rev-parse", "--absolute-git-dir"]);
        let common_dir = git_path(root, ["rev-parse", "--git-common-dir"]);

        let canonical_root = top
            .as_deref()
            .map(canonical_existing_or_parent)
            .transpose()?
            .unwrap_or(canonical_root);
        let canonical_git_dir = git_dir
            .as_deref()
            .map(canonical_existing_or_parent)
            .transpose()?;
        let canonical_common_git_dir = common_dir
            .as_deref()
            .map(|p| {
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    canonical_root.join(p)
                }
            })
            .map(|p| canonical_existing_or_parent(&p))
            .transpose()?;

        let repo_key = canonical_common_git_dir
            .as_ref()
            .or(canonical_git_dir.as_ref())
            .unwrap_or(&canonical_root);
        let repo_id = stable_key(&format!("repo:{}", normalized_path(repo_key)));
        let worktree_id = stable_key(&format!(
            "worktree:{}:{}",
            normalized_path(&canonical_root),
            canonical_git_dir
                .as_ref()
                .map(|p| normalized_path(p))
                .unwrap_or_else(|| "non-git".to_string())
        ));
        Ok(Self {
            repo_id,
            worktree_id,
            canonical_root,
            canonical_git_dir,
            canonical_common_git_dir,
        })
    }

    pub fn lock_root(&self) -> PathBuf {
        lock_home()
            .join("locks")
            .join(format!("v{LOCK_PROTOCOL}"))
            .join("repo")
            .join(&self.repo_id)
    }

    pub fn repository_store_lock(&self) -> PathBuf {
        self.lock_root().join("store.lock")
    }

    pub fn workspace_lock(&self) -> PathBuf {
        self.lock_root()
            .join("worktrees")
            .join(&self.worktree_id)
            .join("workspace.lock")
    }

    /// Path of the per-worktree Time Machine lock. Reached only through
    /// `WorkspaceWriterLease::time_machine_lock`, which is itself unused — Time Machine still takes
    /// its lock via the older `RepoTxnLock::acquire` path.
    #[allow(dead_code)]
    pub fn timemachine_lock(&self) -> PathBuf {
        self.lock_root()
            .join("worktrees")
            .join(&self.worktree_id)
            .join("timemachine.lock")
    }
}

// Worktree ids whose workspace lease THIS process already holds, keyed by the scope that took it.
//
// An OS advisory lock is owned by the process, but both `LockFileEx` on a second handle and `flock`
// on a second descriptor still conflict — a nested acquire inside one process blocks against itself
// and then fails on timeout. That is not a theoretical case: the agent loop takes the lease on the
// first writing tool of a turn and holds it to the end of the turn, so a delegated sub-agent that
// writes (`task` with a write-capable role runs on the serialized path while the parent's lease is
// live) would deadlock against its own parent and report "workspace writer lease was not acquired"
// for an edit nothing was actually contending.
//
// Reentrancy is granted to the holder's scope and its descendants only (see `reentry_allowed`):
// a child inside its parent's blocking dispatch is one writer with it, while a sibling scope in
// the same process — another `serve` lane, a parallel dispatch — is a second writer and waits.

/// One in-process holder: the acquiring scope and a weak reference to the shared OS exclusion.
/// The registry never owns a lease; every live descendant or worker keeps the locks alive.
struct Held {
    scope: String,
    operation: String,
    locks: std::sync::Weak<LockSet>,
}

static HELD: Mutex<Option<HashMap<String, Held>>> = Mutex::new(None);
// The exclusive native-effects gate is shared only with its holder's authorized scope tree.
// Its Weak entry never owns the lock. Resource and per-worktree locks still acquire independently.
static GLOBAL_HELD: Mutex<Option<Held>> = Mutex::new(None);

fn inherited_global_gate(scope: &str) -> Option<std::sync::Arc<LockSet>> {
    let mut held = GLOBAL_HELD
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if held
        .as_ref()
        .is_some_and(|held| held.locks.strong_count() == 0)
    {
        *held = None;
    }
    held.as_ref()
        .filter(|held| reentry_allowed(&held.scope, scope))
        .and_then(|held| held.locks.upgrade())
}

fn native_effects_gate() -> PathBuf {
    resource_lock("native-effects", "global")
}

fn exclusive_global_gate(
    scope: &str,
    operation: &str,
    timeout: Duration,
    cancel: Option<&crate::core::cancel::TurnCancel>,
) -> Result<std::sync::Arc<LockSet>> {
    if let Some(gate) = inherited_global_gate(scope) {
        return Ok(gate);
    }
    let other_holder = {
        let held = GLOBAL_HELD
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        held.as_ref()
            .filter(|held| held.locks.strong_count() > 0)
            .map(|held| format!("{} ({})", held.scope, held.operation))
    };
    let gate = LockSet::acquire(
        vec![LockRequest::new(
            LockClass::Capacity,
            native_effects_gate(),
            LockMode::Exclusive,
            "global native-effects gate",
        )],
        timeout,
        cancel,
        &LockOwner::new(scope, operation),
    )
    .map_err(|error| match other_holder {
        Some(holder) => error.context(format!("held in this process by scope {holder}")),
        None => error,
    })?;
    let gate = std::sync::Arc::new(gate);
    let mut held = GLOBAL_HELD
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *held = Some(Held {
        scope: scope.to_string(),
        operation: operation.to_string(),
        locks: std::sync::Arc::downgrade(&gate),
    });
    Ok(gate)
}

fn held_map<T>(f: impl FnOnce(&mut HashMap<String, Held>) -> T) -> T {
    let mut guard = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.retain(|_, held| held.locks.strong_count() > 0);
    f(map)
}

/// May `requester` reenter a lease `holder` took? Only the holder's own scope and its
/// descendants (`conv/task/3` under `conv`): a delegated child runs INSIDE its parent's blocking
/// dispatch, so the two are one writer. A sibling scope — the next lane of `serve`, a parallel
/// dispatch — is a second writer and must wait for the OS lock like a second process would.
/// Reentrancy used to be process-wide, which let two `serve` lanes on one worktree share a
/// lease that was supposed to keep them apart.
pub(crate) fn reentry_allowed(holder: &str, requester: &str) -> bool {
    requester == holder
        || requester
            .strip_prefix(holder)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The resource scope of the context this thread is running under, or `default`.
fn current_scope() -> String {
    crate::core::exec_ctx::current()
        .map(|c| c.resource_scope())
        .unwrap_or_else(|| "default".to_string())
}

impl std::fmt::Debug for WorkspaceWriterLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceWriterLease")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct WorkspaceWriterLease {
    identity: WorkspaceIdentity,
    /// Every descendant/worker owns the same exclusion; dropping its ancestor cannot release it.
    _locks: std::sync::Arc<LockSet>,
}

impl WorkspaceWriterLease {
    /// Take the lease under the CURRENT execution context's resource scope (see
    /// [`reentry_allowed`]); `default` outside any context.
    pub fn acquire(
        root: &Path,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
    ) -> Result<Self> {
        let identity = WorkspaceIdentity::discover_cached(root)?;
        Self::acquire_identity(identity, timeout, cancel, operation)
    }

    /// [`Self::acquire`] under an explicit resource scope — the agent loop passes its own
    /// `exec_ctx`'s, which is the one its delegated children derive theirs from.
    pub fn acquire_scoped(
        root: &Path,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
        scope: &str,
    ) -> Result<Self> {
        let identity = WorkspaceIdentity::discover_cached(root)?;
        Self::acquire_identity_scoped(identity, timeout, cancel, operation, scope)
    }

    pub fn acquire_identity(
        identity: WorkspaceIdentity,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
    ) -> Result<Self> {
        Self::acquire_identity_scoped(identity, timeout, cancel, operation, &current_scope())
    }

    pub fn acquire_identity_scoped(
        identity: WorkspaceIdentity,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
        scope: &str,
    ) -> Result<Self> {
        // Already ours — the same scope, or a child running inside our dispatch? Take a nested
        // reference instead of deadlocking against our own handle. Any other scope in this
        // process is a second writer and goes through the OS lock below like another process.
        let (reentrant, other_holder) = held_map(|m| match m.get(&identity.worktree_id) {
            Some(h) if reentry_allowed(&h.scope, scope) => (h.locks.upgrade(), None),
            Some(h) => (None, Some(format!("{} ({})", h.scope, h.operation))),
            None => (None, None),
        });
        if let Some(locks) = reentrant {
            return Ok(Self {
                identity,
                _locks: locks,
            });
        }

        let global = exclusive_global_gate(scope, operation, timeout, cancel)?;
        let owner = LockOwner::new(format!("{}-{}", std::process::id(), now_unix()), operation);
        let mut locks = match LockSet::acquire(
            vec![
                LockRequest::new(
                    LockClass::RepositoryStore,
                    identity.repository_store_lock(),
                    LockMode::Shared,
                    "repository store",
                ),
                LockRequest::new(
                    LockClass::Workspace,
                    identity.workspace_lock(),
                    LockMode::Exclusive,
                    "workspace writer",
                ),
            ],
            timeout,
            cancel,
            &owner,
        ) {
            Ok(l) => l,
            // Name the in-process holder when there is one: "timed out" alone reads like
            // another aizen window, and the fix — wait for that dispatch, or order it with
            // `after` — is different.
            Err(e) => match other_holder {
                Some(h) => return Err(e.context(format!("held in this process by scope {h}"))),
                None => return Err(e),
            },
        };
        locks._parent = Some(global);
        // Registered only AFTER the OS locks are in hand, so a failed acquire never leaves a phantom
        // entry that would let a later nested acquire believe it is covered.
        let locks = std::sync::Arc::new(locks);
        held_map(|m| {
            m.insert(
                identity.worktree_id.clone(),
                Held {
                    scope: scope.to_string(),
                    operation: operation.to_string(),
                    locks: std::sync::Arc::downgrade(&locks),
                },
            )
        });
        Ok(Self {
            identity,
            _locks: locks,
        })
    }

    /// Does this process already hold the workspace lease for `identity`'s worktree?
    ///
    /// Reentrancy is handled inside acquisition (it bumps a refcount rather than deadlocking), so no
    /// caller has to ask first. Kept as the queryable form of that state.
    #[allow(dead_code)]
    pub fn held_by_this_process(identity: &WorkspaceIdentity) -> bool {
        held_map(|m| m.contains_key(&identity.worktree_id))
    }

    /// Deprecated compatibility shim. Time Machine's real transaction lock is owned by its
    /// namespace adapter; this path is not a valid integration lock domain.
    #[allow(dead_code)]
    pub fn time_machine_lock(&self) -> Result<RepoTxnLock> {
        anyhow::bail!("use the Time Machine namespace transaction lock adapter")
    }

    /// The workspace this lease was taken for.
    #[allow(dead_code)]
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
}

/// One exact-file resource, bound to existing, non-link ancestors rather than the leaf's inode.
/// Atomic replacement deliberately retains the key. This is cooperative coordination, not kernel
/// confinement or a conditional write: callers must also recheck their content/fingerprint guard
/// and call `verify` immediately before applying approved bytes while holding the resource lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeResource {
    path: PathBuf,
    key: String,
    ancestors: Vec<NativeAncestor>,
}

const MAX_NATIVE_PATH_BYTES: usize = 4096;
const MAX_NATIVE_ANCESTORS: usize = 256;
const MAX_NATIVE_RESOURCES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeAncestor {
    path: PathBuf,
    identity: (u64, u64),
    created: Option<SystemTime>,
}

impl NativeResource {
    /// Check the caller's exact spelling BEFORE joining a workspace root. On Windows, joining a
    /// canonical verbatim root normalizes `.` and `..`, erasing the traversal we must refuse.
    /// Relative paths are allowed here, but rooted/drive-relative aliases are not.
    pub(crate) fn validate_spelling(path: &Path) -> Result<()> {
        let text = path
            .to_str()
            .context("native resource path must be valid Unicode")?;
        if text.is_empty() || text.len() > MAX_NATIVE_PATH_BYTES || text.ends_with(['/', '\\']) {
            anyhow::bail!("native resource needs a bounded exact-file path");
        }
        // Validate every component, not just the leaf: no wildcard, ADS, device namespace, or
        // traversal spelling is accepted as authority. Verbatim disk paths from canonicalize are OK.
        #[cfg(windows)]
        match path.components().next() {
            Some(std::path::Component::Prefix(prefix))
                if path.is_absolute()
                    && matches!(
                        prefix.kind(),
                        std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                    ) => {}
            Some(std::path::Component::Prefix(_) | std::path::Component::RootDir) => {
                anyhow::bail!(
                    "native resource requires a local disk path or an unrooted relative path, not an alias"
                );
            }
            _ => {}
        }
        let spelling = text.strip_prefix(r"\\?\").unwrap_or(text);
        if spelling
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            anyhow::bail!("native resource does not accept traversal or ambiguous path spelling");
        }
        for component in path.components() {
            match component {
                std::path::Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .context("native resource component is not Unicode")?;
                    if name
                        .chars()
                        .any(|c| matches!(c, '[' | ']' | '{' | '}' | '\\'))
                    {
                        anyhow::bail!(
                            "native resource does not accept glob or ambiguous components"
                        );
                    }
                    crate::core::file_placement::validate_managed_name(name)?;
                }
                std::path::Component::ParentDir | std::path::Component::CurDir => {
                    anyhow::bail!("native resource does not accept traversal components");
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn resolve(path: &Path) -> Result<Self> {
        Self::validate_spelling(path)?;
        if !path.is_absolute() {
            anyhow::bail!("native resource needs a bounded absolute exact-file path");
        }
        let basename = path
            .file_name()
            .context("native resource needs a file basename")?;
        let parent = path
            .parent()
            .context("native resource needs an existing parent")?;
        let supplied = native_ancestors(parent)?;
        let canonical_parent = std::fs::canonicalize(parent).with_context(|| {
            format!("canonicalizing native resource parent {}", parent.display())
        })?;
        if native_path_key(parent)? != native_path_key(&canonical_parent)? {
            anyhow::bail!(
                "native resource parent has an ambiguous alias; use its canonical spelling"
            );
        }
        let ancestors = native_ancestors(&canonical_parent)?;
        verify_native_ancestors(&supplied)?;
        let path = canonical_parent.join(basename);
        let resource = Self {
            key: stable_key(&format!("native-file:{}", native_path_key(&path)?)),
            path,
            ancestors,
        };
        resource.verify()?;
        Ok(resource)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    /// Revalidate ancestors and leaf safety, accepting regular-file replacement or creation/deletion.
    /// Content drift is the prepared tool's separate fingerprint responsibility.
    pub(crate) fn verify(&self) -> Result<()> {
        verify_native_ancestors(&self.ancestors)?;
        match std::fs::symlink_metadata(&self.path) {
            Ok(md) => {
                if crate::core::file_placement::is_link_or_reparse(&md) || !md.is_file() {
                    anyhow::bail!("native resource destination must be a regular non-link file");
                }
                let (_, links) = native_node_identity(&self.path, &md)?;
                if links != 1 {
                    anyhow::bail!("native resource refuses hardlinked or ambiguous destinations");
                }
                let canonical = std::fs::canonicalize(&self.path)?;
                if native_path_key(&canonical)? != native_path_key(&self.path)? {
                    anyhow::bail!("native resource destination has an ambiguous alias");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("reading native resource destination"),
        }
        // Do not silently bind an ancestor swapped during the leaf inspection.
        verify_native_ancestors(&self.ancestors)
    }
}

fn native_path_key(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("native resource identity is not Unicode")?;
    if text.len() > MAX_NATIVE_PATH_BYTES {
        anyhow::bail!("native resource path exceeds safety bound");
    }
    #[cfg(windows)]
    let text = crate::core::config::normalize_path_key(path);
    #[cfg(not(windows))]
    let text = text.to_string();
    // Full Unicode case-folding and host alias rules are not interchangeable. Refuse uncertain
    // names on case-folding hosts; ASCII case-folding may over-serialize case-sensitive directories.
    #[cfg(any(windows, target_os = "macos"))]
    {
        if !text.is_ascii() {
            anyhow::bail!("non-ASCII native resource identity is ambiguous on this host");
        }
        Ok(text.to_ascii_lowercase())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(text)
    }
}

fn native_ancestors(parent: &Path) -> Result<Vec<NativeAncestor>> {
    let mut ancestors = Vec::new();
    for path in parent.ancestors() {
        if ancestors.len() == MAX_NATIVE_ANCESTORS {
            anyhow::bail!("native resource ancestry exceeds safety bound");
        }
        let md = std::fs::symlink_metadata(path)
            .with_context(|| format!("native resource parent must exist: {}", path.display()))?;
        if crate::core::file_placement::is_link_or_reparse(&md) || !md.is_dir() {
            anyhow::bail!("native resource ancestors must be existing non-link directories");
        }
        let (identity, _) = native_node_identity(path, &md)?;
        ancestors.push(NativeAncestor {
            path: path.to_path_buf(),
            identity,
            created: md.created().ok(),
        });
    }
    Ok(ancestors)
}

fn verify_native_ancestors(ancestors: &[NativeAncestor]) -> Result<()> {
    for ancestor in ancestors {
        let md = std::fs::symlink_metadata(&ancestor.path).with_context(|| {
            format!(
                "native resource ancestor drift: {}",
                ancestor.path.display()
            )
        })?;
        if crate::core::file_placement::is_link_or_reparse(&md)
            || !md.is_dir()
            || native_node_identity(&ancestor.path, &md)?.0 != ancestor.identity
            || md.created().ok() != ancestor.created
        {
            anyhow::bail!("native resource ancestor changed; resolve and approve again");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn native_node_identity(_path: &Path, md: &std::fs::Metadata) -> Result<((u64, u64), u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok(((md.dev(), md.ino()), md.nlink()))
}

#[cfg(windows)]
fn native_node_identity(path: &Path, _md: &std::fs::Metadata) -> Result<((u64, u64), u64)> {
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
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("opening native resource identity");
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    let error = (ok == 0).then(std::io::Error::last_os_error);
    unsafe {
        CloseHandle(handle);
    }
    if let Some(error) = error {
        return Err(error).context("reading native resource identity");
    }
    if info.dwFileAttributes & 0x400 != 0 {
        anyhow::bail!("native resource identity traverses a reparse point");
    }
    Ok((
        (
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ),
        info.nNumberOfLinks as u64,
    ))
}

#[cfg(not(any(unix, windows)))]
fn native_node_identity(_path: &Path, _md: &std::fs::Metadata) -> Result<((u64, u64), u64)> {
    anyhow::bail!("native resource identity is unavailable on this platform")
}

/// Exclusion for an ACTUAL physical effect, separate from reentrant dispatch bookkeeping. Every
/// new body must acquire independently, even under the same/descendant scope. Cloning retains ONE
/// admitted body's lifetime; it must never be used as admission for another body. Opaque callers
/// acquire exclusive after WorkspaceWriterLease and retain it through physical process exit.
#[derive(Clone)]
pub(crate) struct PhysicalEffectLease {
    _locks: std::sync::Arc<LockSet>,
    mode: LockMode,
}

impl std::fmt::Debug for PhysicalEffectLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalEffectLease")
            .finish_non_exhaustive()
    }
}

impl PhysicalEffectLease {
    pub(crate) fn exclusive(
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
        scope: &str,
    ) -> Result<Self> {
        Self::acquire(LockMode::Exclusive, timeout, cancel, operation, scope)
    }

    fn acquire(
        mode: LockMode,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
        scope: &str,
    ) -> Result<Self> {
        if scope.is_empty() || scope.len() > 4096 || operation.len() > 4096 {
            anyhow::bail!("physical effect lease requires bounded operation and nonempty scope");
        }
        let locks = LockSet::acquire(
            vec![LockRequest::new(
                LockClass::PhysicalEffect,
                resource_lock("physical-effects", "global"),
                mode,
                "physical effects gate",
            )],
            timeout,
            cancel,
            &LockOwner::new(scope, operation),
        )?;
        Ok(Self {
            _locks: std::sync::Arc::new(locks),
            mode,
        })
    }

    pub(crate) fn is_exclusive(&self) -> bool {
        self.mode == LockMode::Exclusive
    }
}

/// Which category of live sidecar this is — and therefore how it relates to scoped native work.
///
/// The two categories carry different effect risk, so they take SEPARATE gates:
///
/// - A **language server** is a read-mostly whole-repo observer, so it SHARES its gate with scoped
///   native bodies. That coexistence is sound because host writes are atomic
///   (`compare_and_atomic_write`), so a reader sees the old or the new bytes and never a torn file,
///   and the server's own outputs land outside any declared exact path. It is also necessary: with
///   the single EXCLUSIVE gate this used to be, one live language server barred the native body of
///   *every* child for the whole session, so parallel agents silently edited nothing.
/// - An **MCP server** has arbitrary effects and may write anywhere, so it holds its gate
///   EXCLUSIVELY: it is excluded by scoped native work, and excludes it in turn.
///
/// Separate gates also keep the two categories able to coexist, which the old single pooled gate
/// allowed; one shared gate at two different modes would have made them exclude each other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SidecarKind {
    LanguageServer,
    McpServer,
}

impl SidecarKind {
    fn gate(self) -> PathBuf {
        match self {
            Self::LanguageServer => resource_lock("sidecar-effects", "lsp"),
            Self::McpServer => resource_lock("sidecar-effects", "mcp"),
        }
    }

    fn mode(self) -> LockMode {
        match self {
            Self::LanguageServer => LockMode::Shared,
            Self::McpServer => LockMode::Exclusive,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::LanguageServer => "live language server",
            Self::McpServer => "live MCP server",
        }
    }
}

/// One live sidecar's admission. Holds one lock set per gate it took, so a scoped native body (which
/// must be excluded by MCP servers while coexisting with language servers) can carry both.
#[derive(Clone)]
pub(crate) struct SidecarEffectLease {
    _locks: Vec<std::sync::Arc<LockSet>>,
}

impl std::fmt::Debug for SidecarEffectLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarEffectLease").finish_non_exhaustive()
    }
}

/// Pooled per category, so an MCP start can never adopt a language server's SHARED handle — that
/// would silently grant it the coexistence it must not have.
static SIDECAR_HELD: Mutex<Vec<(SidecarKind, std::sync::Weak<LockSet>)>> = Mutex::new(Vec::new());

impl SidecarEffectLease {
    pub(crate) fn acquire(
        kind: SidecarKind,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
    ) -> Result<Self> {
        let start = std::time::Instant::now();
        let path = kind.gate();
        // No OS wait under the pooling mutex: a failed attempt leaves no phantom gate. The
        // recheck loop allows simultaneous same-category starts to pool a newly acquired handle.
        loop {
            if cancel.is_some_and(crate::core::cancel::TurnCancel::is_cancelled) {
                anyhow::bail!("cancelled before sidecar admission");
            }
            let attempt = {
                let mut held = SIDECAR_HELD
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                // Retire dead entries so a long session cannot grow the pool without bound.
                held.retain(|(_, locks)| locks.strong_count() > 0);
                if let Some(locks) = held
                    .iter()
                    .find(|(pooled, _)| *pooled == kind)
                    .and_then(|(_, locks)| locks.upgrade())
                {
                    return Ok(Self {
                        _locks: vec![locks],
                    });
                }
                match LockSet::acquire(
                    vec![LockRequest::new(
                        LockClass::SidecarEffect,
                        path.clone(),
                        kind.mode(),
                        kind.label(),
                    )],
                    Duration::ZERO,
                    cancel,
                    &LockOwner::new("sidecar-category", kind.label()),
                ) {
                    Ok(locks) => {
                        let locks = std::sync::Arc::new(locks);
                        held.push((kind, std::sync::Arc::downgrade(&locks)));
                        return Ok(Self {
                            _locks: vec![locks],
                        });
                    }
                    Err(error) => error,
                }
            };
            if attempt
                .downcast_ref::<crate::core::repo_lock::LockBusy>()
                .is_none()
                || start.elapsed() >= timeout
            {
                return Err(attempt);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Independent SHARED admission for one scoped native body. It takes BOTH sidecar gates at SHARED:
/// an MCP server (EXCLUSIVE) is excluded, while a language server (SHARED) coexists — so an
/// LSP-attached session can still run scoped native children instead of starving every native body
/// they make. Must be retained with its actual resource lifetime; never replace it with a clone of a
/// pooled sidecar exclusive gate.
pub(crate) fn acquire_shared_sidecar(
    timeout: Duration,
    cancel: Option<&crate::core::cancel::TurnCancel>,
) -> Result<SidecarEffectLease> {
    let start = std::time::Instant::now();
    let mut locks = Vec::with_capacity(2);
    for kind in [SidecarKind::LanguageServer, SidecarKind::McpServer] {
        locks.push(std::sync::Arc::new(LockSet::acquire(
            vec![LockRequest::new(
                LockClass::SidecarEffect,
                kind.gate(),
                LockMode::Shared,
                "scoped native sidecar barrier",
            )],
            timeout.saturating_sub(start.elapsed()),
            cancel,
            &LockOwner::new("scoped-native", "native sidecar barrier"),
        )?));
    }
    Ok(SidecarEffectLease { _locks: locks })
}

/// Shared physical/workspace-effect gates plus sorted exact-file locks. Only actual worker
/// ownership releases exclusion; dropping an observer clone cannot release a live body's resources.
/// Neither physical admission nor resource locks reenter an ancestor's lease.
#[derive(Clone)]
pub(crate) struct NativeResourceLease {
    _locks: std::sync::Arc<NativeLocks>,
}

struct NativeLocks {
    // Field drop order releases higher-order exclusion before lower-order admission gates.
    _resources: LockSet,
    _workspace: std::sync::Arc<LockSet>,
    _physical: PhysicalEffectLease,
    _sidecar: Option<SidecarEffectLease>,
    _global: std::sync::Arc<LockSet>,
}

impl std::fmt::Debug for NativeResourceLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeResourceLease")
            .finish_non_exhaustive()
    }
}

impl NativeResourceLease {
    pub(crate) fn acquire(
        root: &Path,
        resources: &[(NativeResource, LockMode)],
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        scope: &str,
    ) -> Result<Self> {
        if resources.is_empty()
            || resources.len() > MAX_NATIVE_RESOURCES
            || scope.is_empty()
            || scope.len() > 4096
        {
            anyhow::bail!("native resource lease requires a bounded nonempty footprint and scope");
        }
        if cancel.is_some_and(crate::core::cancel::TurnCancel::is_cancelled) {
            anyhow::bail!("cancelled before acquiring native resource lease");
        }
        let start = std::time::Instant::now();
        for (resource, _) in resources {
            resource.verify()?;
        }
        let identity = WorkspaceIdentity::discover_cached(root)?;
        // The holder's exact scope or a '/' descendant may borrow its exclusive gate. The Arc
        // owns the OS handles even if the ancestor/observer disappears during this worker's run.
        let inherited = held_map(|map| {
            map.get(&identity.worktree_id)
                .filter(|held| reentry_allowed(&held.scope, scope))
                .and_then(|held| held.locks.upgrade())
        });
        let owner = LockOwner::new(scope, "native resource lease");
        let inherited_global = inherited_global_gate(scope);
        let borrowed_global = inherited_global.is_some();
        let global = match inherited_global {
            Some(global) => global,
            None => std::sync::Arc::new(native_lock_set(
                vec![LockRequest::new(
                    LockClass::Capacity,
                    native_effects_gate(),
                    LockMode::Shared,
                    "global native-effects gate",
                )],
                start,
                timeout,
                cancel,
                &owner,
            )?),
        };
        let scoped =
            crate::core::exec_ctx::current().is_some_and(|ctx| ctx.native_scope().is_some());
        let sidecar = if scoped {
            Some(acquire_shared_sidecar(
                timeout.saturating_sub(start.elapsed()),
                cancel,
            )?)
        } else {
            None
        };
        // NEVER borrow an ancestor's physical gate: an opaque sibling can own it while dispatch
        // bookkeeping shares the global/workspace exclusive lease. Same-scope bodies also conflict.
        let physical = PhysicalEffectLease::acquire(
            LockMode::Shared,
            timeout.saturating_sub(start.elapsed()),
            cancel,
            "native resource body",
            scope,
        )?;
        // Opaque dispatch acquires WorkspaceWriterLease before its physical gate. If the global
        // exclusive gate was inherited but a sibling owns a non-reentrant workspace gate, never
        // wait for that gate while holding physical shared (which the sibling may need exclusive).
        let workspace_timeout = if borrowed_global {
            Duration::ZERO
        } else {
            timeout
        };
        let workspace = match inherited {
            Some(gate) => gate,
            None => std::sync::Arc::new(native_lock_set(
                vec![
                    LockRequest::new(
                        LockClass::RepositoryStore,
                        identity.repository_store_lock(),
                        LockMode::Shared,
                        "native repository gate",
                    ),
                    LockRequest::new(
                        LockClass::Workspace,
                        identity.workspace_lock(),
                        LockMode::Shared,
                        "native workspace gate",
                    ),
                ],
                start,
                workspace_timeout,
                cancel,
                &owner,
            )?),
        };
        // The same path may be both read and written in a prepared footprint: collapse it to the
        // strongest mode, then sort by global path key (not workspace, so managed paths also agree).
        let mut requests: HashMap<String, LockMode> = HashMap::new();
        for (resource, mode) in resources {
            requests
                .entry(resource.key().to_string())
                .and_modify(|old| {
                    if *mode == LockMode::Exclusive {
                        *old = LockMode::Exclusive;
                    }
                })
                .or_insert(*mode);
        }
        let requests = requests
            .into_iter()
            .map(|(key, mode)| {
                LockRequest::new(
                    LockClass::Resource,
                    resource_lock("native-file", &key),
                    mode,
                    "native exact-file resource",
                )
            })
            .collect();
        let resource_locks = native_lock_set(requests, start, timeout, cancel, &owner)?;
        for (resource, _) in resources {
            resource.verify()?;
        }
        Ok(Self {
            _locks: std::sync::Arc::new(NativeLocks {
                _resources: resource_locks,
                _workspace: workspace,
                _physical: physical,
                _sidecar: sidecar,
                _global: global,
            }),
        })
    }
}

/// One total acquisition budget, including waiting for earlier locks. Existing global lease timeout
/// behavior stays unchanged; native footprints cannot multiply the supplied timeout by 128 paths.
fn native_lock_set(
    mut requests: Vec<LockRequest>,
    start: std::time::Instant,
    timeout: Duration,
    cancel: Option<&crate::core::cancel::TurnCancel>,
    owner: &LockOwner,
) -> Result<LockSet> {
    requests.sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));
    let mut result = LockSet {
        held: Vec::with_capacity(requests.len()),
        _parent: None,
    };
    for request in requests {
        let mut next = LockSet::acquire(
            vec![request],
            timeout.saturating_sub(start.elapsed()),
            cancel,
            owner,
        )?;
        result.held.append(&mut next.held);
    }
    Ok(result)
}

/// Physical delegated-work ownership, independent of writer/resource admission. Seat one Arc in
/// the common child context and clone it into actual workers/process registries. The Weak registry
/// observes lifetime but cannot keep an abandoned activity alive itself.
#[derive(Debug)]
pub(crate) struct WorkspaceActivity {
    #[cfg(test)]
    scope: String,
}

const MAX_WORKSPACE_ACTIVITIES: usize = 4096;
const MAX_ACTIVITY_WORKSPACES: usize = 1024;
type ActivityMap = HashMap<String, Vec<std::sync::Weak<WorkspaceActivity>>>;
static ACTIVITIES: Mutex<Option<ActivityMap>> = Mutex::new(None);

fn activity_map<T>(f: impl FnOnce(&mut ActivityMap) -> T) -> T {
    let mut guard = ACTIVITIES.lock().unwrap_or_else(|error| error.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.retain(|_, activities| {
        activities.retain(|activity| activity.strong_count() > 0);
        !activities.is_empty()
    });
    f(map)
}

impl WorkspaceActivity {
    pub(crate) fn begin(root: &Path, scope: &str) -> Result<std::sync::Arc<Self>> {
        if scope.is_empty() || scope.len() > 4096 {
            anyhow::bail!("workspace activity requires a bounded nonempty scope");
        }
        let identity = WorkspaceIdentity::discover_cached(root)?;
        begin_activity_identity(identity.worktree_id, scope)
    }
}

fn begin_activity_identity(
    worktree_id: String,
    scope: &str,
) -> Result<std::sync::Arc<WorkspaceActivity>> {
    activity_map(|map| {
        if !map.contains_key(&worktree_id) && map.len() >= MAX_ACTIVITY_WORKSPACES {
            anyhow::bail!("workspace activity registry is full");
        }
        let activities = map.entry(worktree_id).or_default();
        if activities.len() >= MAX_WORKSPACE_ACTIVITIES {
            anyhow::bail!("too many live activities in this workspace");
        }
        #[cfg(not(test))]
        let _ = scope;
        let activity = std::sync::Arc::new(WorkspaceActivity {
            #[cfg(test)]
            scope: scope.to_string(),
        });
        activities.push(std::sync::Arc::downgrade(&activity));
        Ok(activity)
    })
}

/// Refuse parent recovery while ANY physical delegated activity is live, even the caller's scope.
/// This snapshot helper is NOT atomic drain admission or cross-process exclusion; runtime restore
/// must use `WorkspaceRestoreGuard` across synchronous destructive work, plus exclusive OS gates.
#[cfg(test)]
pub(crate) fn require_workspace_quiescent(root: &Path) -> Result<()> {
    let identity = WorkspaceIdentity::discover_cached(root)?;
    activity_map(|map| {
        let live: Vec<_> = map
            .get(&identity.worktree_id)
            .into_iter()
            .flatten()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        if !live.is_empty() {
            let scopes: Vec<_> = live
                .iter()
                .take(4)
                .map(|activity| activity.scope.as_str())
                .collect();
            anyhow::bail!(
                "workspace is not quiescent: {} live delegated activities ({})",
                live.len(),
                scopes.join(", ")
            );
        }
        Ok(())
    })
}

/// Atomic in-process restore admission. Hold through the OUTERMOST synchronous restore body only;
/// nested restore must not acquire again. Starts on any root wait conservatively for this guard,
/// but a restore attempt never waits for physical workers to settle: live activity refuses it.
/// Not Send, not an OS lock: retain exclusive global/workspace gates separately before acquiring.
pub(crate) struct WorkspaceRestoreGuard {
    _activities: std::sync::MutexGuard<'static, Option<ActivityMap>>,
}

impl WorkspaceRestoreGuard {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        let identity = WorkspaceIdentity::discover_cached(root)?;
        // Reentrant/nested attempts fail rather than deadlock. No lock or worker wait here: a
        // conflicting restore/start may be retried by the controller after this operation ends.
        let mut activities = match ACTIVITIES.try_lock() {
            Ok(activities) => activities,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                anyhow::bail!("workspace restore admission is busy")
            }
        };
        let map = activities.get_or_insert_with(HashMap::new);
        if map.get(&identity.worktree_id).is_some_and(|activities| {
            activities
                .iter()
                .any(|activity| activity.strong_count() > 0)
        }) {
            anyhow::bail!("workspace restore refused while delegated activity is live");
        }
        Ok(Self {
            _activities: activities,
        })
    }
}

/// Lock ordering classes. Global admission -> categorized sidecar barrier -> independent physical
/// effects -> store/workspace -> sorted resources -> journals. A WorkspaceWriterLease is dispatch
/// admission, not a physical body;
/// opaque integration takes its physical gate after that reentrant lease, before doing any effects.
/// Never upgrade the workspace gate while holding resource locks.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockClass {
    Capacity = 1,
    SidecarEffect = 2,
    PhysicalEffect = 3,
    RepositoryStore = 4,
    Workspace = 5,
    Resource = 6,
    TimeMachine = 7,
    Journal = 8,
}

#[derive(Debug, Clone)]
pub struct LockRequest {
    pub class: LockClass,
    pub path: PathBuf,
    pub mode: LockMode,
    pub label: String,
}

impl LockRequest {
    pub fn new(class: LockClass, path: PathBuf, mode: LockMode, label: impl Into<String>) -> Self {
        Self {
            class,
            path,
            mode,
            label: label.into(),
        }
    }
}

pub struct LockSet {
    held: Vec<HeldLock>,
    // Lower-order reused gate; field order releases our locks before this ancestor's handle.
    _parent: Option<std::sync::Arc<LockSet>>,
}

struct HeldLock {
    _lock: RepoTxnLock,
    owner_path: PathBuf,
}

impl LockSet {
    pub fn acquire(
        mut requests: Vec<LockRequest>,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        owner: &LockOwner,
    ) -> Result<Self> {
        requests.sort_by(|a, b| {
            (a.class, &a.path, mode_rank(a.mode)).cmp(&(b.class, &b.path, mode_rank(b.mode)))
        });
        for pair in requests.windows(2) {
            if pair[0].path == pair[1].path {
                anyhow::bail!("duplicate lock request for {}", pair[0].path.display());
            }
        }

        let mut held = Vec::with_capacity(requests.len());
        for request in requests {
            let lock = RepoTxnLock::acquire_mode(&request.path, request.mode, timeout, cancel)
                .with_context(|| format!("acquiring {}", request.label))?;
            let owner_path = owner_path(&request.path);
            write_owner(&owner_path, owner, &request);
            held.push(HeldLock {
                _lock: lock,
                owner_path,
            });
        }
        Ok(Self {
            held,
            _parent: None,
        })
    }

    /// How many locks this set holds. The set is acquired all-or-nothing and released by `Drop`, so
    /// callers never need to count it; these are its inspection surface.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

impl Drop for LockSet {
    fn drop(&mut self) {
        for held in self.held.iter().rev() {
            let _ = std::fs::remove_file(&held.owner_path);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LockOwner {
    protocol: u32,
    pid: u32,
    binary_version: &'static str,
    run_id: String,
    operation: String,
    acquired_unix: u64,
}

impl LockOwner {
    pub fn new(run_id: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            protocol: LOCK_PROTOCOL,
            pid: std::process::id(),
            binary_version: env!("CARGO_PKG_VERSION"),
            run_id: run_id.into(),
            operation: operation.into(),
            acquired_unix: now_unix(),
        }
    }
}

#[derive(Serialize)]
struct OwnerRecord<'a> {
    #[serde(flatten)]
    owner: &'a LockOwner,
    resource: &'a str,
    mode: &'static str,
}

pub fn stable_key(value: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, value.as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn resource_lock(kind: &str, logical_key: &str) -> PathBuf {
    lock_home()
        .join("locks")
        .join(format!("v{LOCK_PROTOCOL}"))
        .join("resources")
        .join(safe_segment(kind))
        .join(format!("{}.lock", stable_key(logical_key)))
}

pub fn store_lock(kind: &str, logical_key: &str) -> PathBuf {
    lock_home()
        .join("locks")
        .join(format!("v{LOCK_PROTOCOL}"))
        .join("stores")
        .join(safe_segment(kind))
        .join(format!("{}.lock", stable_key(logical_key)))
}

fn write_owner(path: &Path, owner: &LockOwner, request: &LockRequest) {
    let record = OwnerRecord {
        owner,
        resource: &request.label,
        mode: match request.mode {
            LockMode::Shared => "shared",
            LockMode::Exclusive => "exclusive",
        },
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
        let _ = crate::core::persist::atomic_write_owner_only(path, &bytes);
    }
}

fn owner_path(lock: &Path) -> PathBuf {
    let mut name = lock.file_name().unwrap_or_default().to_os_string();
    name.push(".owner.json");
    lock.with_file_name(name)
}

fn mode_rank(mode: LockMode) -> u8 {
    match mode {
        LockMode::Shared => 0,
        LockMode::Exclusive => 1,
    }
}

fn git_path<const N: usize>(root: &Path, args: [&str; N]) -> Option<PathBuf> {
    let out = crate::core::gitx::command()
        .ok()?
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let path = text.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn canonical_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("canonicalizing {}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing parent {}", parent.display()))?;
    Ok(path
        .file_name()
        .map(|n| canonical_parent.join(n))
        .unwrap_or(canonical_parent))
}

fn normalized_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/") {
        value = rest.to_string();
    }
    if value.as_bytes().get(1) == Some(&b':') {
        value.replace_range(0..1, &value[..1].to_ascii_lowercase());
    }
    value.trim_end_matches('/').to_string()
}

fn safe_segment(value: &str) -> String {
    let out: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "resource".to_string()
    } else {
        out
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_fixture() -> (
        crate::core::file_placement::tests::TestDir,
        NativeResource,
        NativeResource,
    ) {
        let temp = crate::core::file_placement::tests::TestDir::new();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        let a = NativeResource::resolve(&temp.path().join("workspace/a.txt")).unwrap();
        let b = NativeResource::resolve(&temp.path().join("workspace/b.txt")).unwrap();
        (temp, a, b)
    }

    #[test]
    fn native_key_survives_atomic_replacement_and_creation() {
        let (_temp, resource, _) = native_fixture();
        let key = resource.key().to_string();
        crate::core::persist::atomic_write(resource.path(), b"first").unwrap();
        assert_eq!(NativeResource::resolve(resource.path()).unwrap().key(), key);
        crate::core::persist::atomic_write(resource.path(), b"replacement").unwrap();
        assert_eq!(NativeResource::resolve(resource.path()).unwrap().key(), key);
        resource.verify().unwrap();
        std::fs::remove_file(resource.path()).unwrap();
        resource.verify().unwrap();
        assert_eq!(NativeResource::resolve(resource.path()).unwrap().key(), key);
    }

    #[test]
    fn native_resources_refuse_unsafe_and_ambiguous_destinations() {
        let (temp, resource, _) = native_fixture();
        assert!(NativeResource::resolve(Path::new("relative.txt")).is_err());
        assert!(NativeResource::resolve(temp.path()).is_err());
        for name in [
            "*.txt",
            "[ab].txt",
            "{a,b}.txt",
            "CON",
            "trailing.",
            "stream:ads",
        ] {
            assert!(
                NativeResource::resolve(&temp.path().join(name)).is_err(),
                "{name}"
            );
        }
        assert!(NativeResource::resolve(&temp.path().join("missing/file.txt")).is_err());
        for spelling in [
            "workspace/../escape.txt",
            "workspace/./a.txt",
            "workspace\\..\\escape.txt",
            "workspace\\.\\a.txt",
        ] {
            // PathBuf::join on a canonical verbatim Windows root removes traversal. Append to
            // the OsString instead so this fixture actually passes the unsafe spelling to resolve.
            let mut raw = temp.path().as_os_str().to_os_string();
            raw.push(std::path::MAIN_SEPARATOR_STR);
            raw.push(spelling);
            let path = PathBuf::from(raw);
            let error = NativeResource::resolve(&path).expect_err("raw traversal must be refused");
            assert!(
                error.to_string().contains("traversal"),
                "wrong refusal for {}: {error:#}",
                path.display()
            );
        }
        std::fs::write(resource.path(), b"file").unwrap();
        std::fs::hard_link(resource.path(), temp.path().join("alias.txt")).unwrap();
        assert!(resource.verify().is_err());
        assert!(NativeResource::resolve(resource.path()).is_err());
        assert!(NativeResource::resolve(&temp.path().join("alias.txt")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_spelling_refuses_rooted_drive_relative_and_namespace_aliases() {
        for spelling in [
            r"C:escape.txt",
            r"\escape.txt",
            r"/escape.txt",
            r"\\server\share\file.txt",
            r"\\?\UNC\server\share\file.txt",
            r"\\.\C:\file.txt",
        ] {
            assert!(
                NativeResource::validate_spelling(Path::new(spelling)).is_err(),
                "unsafe alias accepted: {spelling}"
            );
        }
        for spelling in [
            "one.txt",
            r"nested\one.txt",
            r"C:\one.txt",
            r"\\?\C:\one.txt",
        ] {
            NativeResource::validate_spelling(Path::new(spelling))
                .unwrap_or_else(|error| panic!("safe spelling refused: {spelling}: {error:#}"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_resources_refuse_leaf_and_ancestor_symlinks() {
        let (temp, resource, _) = native_fixture();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(resource.path().parent().unwrap(), &link).unwrap();
        assert!(NativeResource::resolve(&link.join("file.txt")).is_err());
        std::os::unix::fs::symlink(temp.path().join("absent"), resource.path()).unwrap();
        assert!(resource.verify().is_err());
        assert!(NativeResource::resolve(resource.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_ascii_case_spelling_has_one_key() {
        let (_temp, resource, _) = native_fixture();
        let other = NativeResource::resolve(&resource.path().with_file_name("A.TXT")).unwrap();
        assert_eq!(resource.key(), other.key());
    }

    #[test]
    fn native_resources_preserve_ancestor_binding() {
        let (temp, resource, _) = native_fixture();
        let parent = resource.path().parent().unwrap();
        std::fs::rename(parent, temp.path().join("old-workspace")).unwrap();
        std::fs::create_dir(parent).unwrap();
        assert!(
            resource.verify().is_err(),
            "rebinding a replacement directory must fail"
        );
        assert_eq!(
            NativeResource::resolve(resource.path()).unwrap().key(),
            resource.key()
        );
    }

    const NATIVE_CHILD_TEST: &str = "core::workspace_txn::tests::native_resource_subprocess_helper";
    const NATIVE_CHILD_ROOT: &str = "AIZEN_NATIVE_LOCK_TEST_ROOT";
    const NATIVE_CHILD_MODE: &str = "AIZEN_NATIVE_LOCK_TEST_MODE";

    #[test]
    fn native_resource_subprocess_helper() -> Result<()> {
        use std::io::{BufRead, Write};
        let Some(root) = std::env::var_os(NATIVE_CHILD_ROOT) else {
            return Ok(());
        };
        // Ordinary suite execution is a no-op even if an unrelated caller inherited the marker.
        if !std::env::args().any(|arg| arg == NATIVE_CHILD_TEST)
            || !std::env::args().any(|arg| arg == "--exact")
        {
            return Ok(());
        }
        let root = std::fs::canonicalize(PathBuf::from(root))?;
        let temp = std::fs::canonicalize(std::env::temp_dir())?;
        anyhow::ensure!(
            root.starts_with(&temp) && root != temp,
            "child needs an owned temp root"
        );
        let home = root.join("home");
        let workspace = root.join("workspace");
        anyhow::ensure!(
            home.is_dir() && workspace.is_dir(),
            "missing child fixture directories"
        );
        let mode = match std::env::var(NATIVE_CHILD_MODE)?.as_str() {
            "reader" => LockMode::Shared,
            "writer" => LockMode::Exclusive,
            _ => anyhow::bail!("unknown native child lock mode"),
        };
        with_test_lock_home(&home, || -> Result<()> {
            let resource = NativeResource::resolve(&workspace.join("a.txt"))?;
            let lease = NativeResourceLease::acquire(
                &workspace,
                &[(resource.clone(), mode)],
                Duration::from_secs(2),
                None,
                "native-child",
            )?;
            // A dedicated input reader gives this helper a self-enforced deadline: parent panic,
            // closed stdin or a missing release never strands a lock-owning subprocess indefinitely.
            let (send, receive) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut line = String::new();
                let input = std::io::stdin().lock().read_line(&mut line);
                let _ = send.send(input.map(|_| line));
            });
            println!("NATIVE_LOCK_READY:{}", resource.key());
            std::io::stdout().flush()?;
            let release = receive.recv_timeout(Duration::from_secs(8));
            drop(lease);
            let release = release.context("native child release deadline expired")??;
            anyhow::ensure!(
                release == "release\n",
                "native child input closed or invalid release"
            );
            Ok(())
        })
    }

    #[test]
    fn native_resources_coordinate_across_processes_and_replacement() -> Result<()> {
        use crate::sandbox::request::SandboxRequest;
        use crate::sandbox::runner::{self, Outcome};
        use std::io::{BufRead, Read, Write};
        use std::process::Stdio;

        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, a, b) = native_fixture();
        let home = temp.path().join("home");
        std::fs::create_dir(&home)?;
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let exe = std::env::current_exe()?;
        // Kernel backends need read/execute access to the existing test binary outside the owned
        // write root. This config lives ONLY in the fixture home; sandbox mode is not overridden.
        std::fs::write(
            home.join("cli-config.json"),
            serde_json::to_vec(&serde_json::json!({"sandbox": {
                "read_only_roots": [exe.parent().context("test exe has no parent")?]
            }}))?,
        )?;
        with_test_lock_home(&home, || -> Result<()> {
            let root = temp.path().join("workspace");
            for mode in ["writer", "reader"] {
                let request = SandboxRequest::exec(
                    crate::sandbox::CommandOrigin::InternalTrusted(
                        "native resource lock regression",
                    ),
                    exe.clone(),
                    vec![
                        "--exact".into(),
                        NATIVE_CHILD_TEST.into(),
                        "--nocapture".into(),
                        "--test-threads=1".into(),
                    ],
                    temp.path().to_path_buf(),
                    temp.path().to_path_buf(),
                )
                .background(false)
                .private_tmp(false)
                .wall_timeout(Duration::from_secs(12))
                .extra_env(vec![
                    (
                        NATIVE_CHILD_ROOT.into(),
                        temp.path().to_str().context("fixture not Unicode")?.into(),
                    ),
                    (NATIVE_CHILD_MODE.into(), mode.into()),
                    (
                        "AIZEN_HOME".into(),
                        home.to_str().context("fixture home not Unicode")?.into(),
                    ),
                ]);
                let mut sandboxed = runner::prepare_std(request)?;
                sandboxed
                    .command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit());
                let mut child = match sandboxed.command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        sandboxed.finish(Outcome::SpawnFailed);
                        return Err(error).context("spawning native lock helper");
                    }
                };
                let containment = sandboxed.contain(&child);
                let mut input = child.stdin.take();
                let output = child.stdout.take().context("missing child stdout")?;
                let (ready_send, ready) = std::sync::mpsc::channel();
                // Drain after readiness too: closing stdout early would turn libtest's final report
                // into a broken-pipe failure. Capture is bounded; no user log files are involved.
                let (output_send, drained) = std::sync::mpsc::channel();
                let reader = std::thread::spawn(move || {
                    let result = (|| -> std::io::Result<String> {
                        let mut captured = String::new();
                        let lines = std::io::BufReader::new(output.take(64 * 1024)).lines();
                        for line in lines {
                            let line = line?;
                            if let Some((_, key)) = line.split_once("NATIVE_LOCK_READY:") {
                                let _ = ready_send.send(key.to_owned());
                            }
                            captured.push_str(&line);
                            captured.push('\n');
                        }
                        Ok(captured)
                    })();
                    let _ = output_send.send(result);
                });
                let checks = (|| -> Result<()> {
                    let key = ready
                        .recv_timeout(Duration::from_secs(5))
                        .context("native helper exited or did not report readiness by deadline")?;
                    anyhow::ensure!(
                        key == a.key(),
                        "child and parent resource identities differ"
                    );
                    let same = |mode| {
                        NativeResourceLease::acquire(
                            &root,
                            &[(a.clone(), mode)],
                            Duration::ZERO,
                            None,
                            "native-parent",
                        )
                    };
                    for requested in [LockMode::Shared, LockMode::Exclusive] {
                        if mode == "reader" && requested == LockMode::Shared {
                            let _overlap = same(requested)?;
                        } else {
                            let error = same(requested)
                                .expect_err("same-file cross-process lock must conflict");
                            anyhow::ensure!(
                                error
                                    .downcast_ref::<crate::core::repo_lock::LockBusy>()
                                    .is_some()
                                    && format!("{error:#}").contains("native exact-file resource"),
                                "{mode} conflict was not the exact-file resource lock: {error:#}"
                            );
                        }
                    }
                    for requested in [LockMode::Exclusive, LockMode::Shared] {
                        let _disjoint = NativeResourceLease::acquire(
                            &root,
                            &[(b.clone(), requested)],
                            Duration::ZERO,
                            None,
                            "native-parent",
                        )?;
                    }
                    if mode == "writer" {
                        // Simulate an uncooperative atomic editor while the other process holds the
                        // cooperative lease. Replacement must not move admission to a fresh inode key.
                        crate::core::persist::atomic_write(a.path(), b"cross-process replacement")?;
                        let replaced = NativeResource::resolve(a.path())?;
                        anyhow::ensure!(
                            replaced.key() == key,
                            "replacement changed resource lock key"
                        );
                        let error = NativeResourceLease::acquire(
                            &root,
                            &[(replaced, LockMode::Exclusive)],
                            Duration::ZERO,
                            None,
                            "native-parent",
                        )
                        .expect_err("replacement bypassed the child-held resource lock");
                        anyhow::ensure!(
                            error
                                .downcast_ref::<crate::core::repo_lock::LockBusy>()
                                .is_some()
                                && format!("{error:#}").contains("native exact-file resource"),
                            "replacement failed for an unrelated reason: {error:#}"
                        );
                    }
                    Ok(())
                })();
                // Always release before returning a failed check. EOF also releases the helper;
                // its own watchdog bounds lock lifetime if this parent's thread panics instead.
                let release = input
                    .as_mut()
                    .context("missing child stdin")
                    .and_then(|input| input.write_all(b"release\n").map_err(Into::into));
                drop(input.take());
                let deadline = std::time::Instant::now() + Duration::from_secs(12);
                let status = loop {
                    if let Some(status) = child.try_wait()? {
                        break status;
                    }
                    if std::time::Instant::now() >= deadline {
                        sandboxed.finish(Outcome::Timeout);
                        anyhow::bail!("native helper failed to exit after release/watchdog; no process termination attempted");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                sandboxed.finish(Outcome::Exit(status.code()));
                let output = drained
                    .recv_timeout(Duration::from_secs(2))
                    .context("native child exited but stdout did not close by deadline")??;
                reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("child stdout reader panicked"))?;
                drop(containment);
                anyhow::ensure!(
                    status.success(),
                    "native helper {mode} failed: {status}; stdout: {output}; checks: {checks:?}"
                );
                checks?;
                release?;
                // The child exited rather than merely sending a marker: all its OS leases are gone.
                NativeResourceLease::acquire(
                    &root,
                    &[(a.clone(), LockMode::Exclusive)],
                    Duration::ZERO,
                    None,
                    "native-parent",
                )?;
            }
            Ok(())
        })
    }

    #[test]
    fn native_readers_overlap_and_disjoint_writers_overlap() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, a, b) = native_fixture();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let root = temp.path().join("workspace");
        let reader = NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Shared)],
            Duration::ZERO,
            None,
            "reader-a",
        )
        .unwrap();
        let reader2 = NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Shared)],
            Duration::ZERO,
            None,
            "reader-b",
        )
        .unwrap();
        let disjoint = NativeResourceLease::acquire(
            &root,
            &[(b.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "writer-b",
        )
        .unwrap();
        assert!(NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "writer-a"
        )
        .is_err());
        drop(reader2);
        drop(reader);
        let writer = NativeResourceLease::acquire(
            &root,
            &[(a, LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "writer-a",
        )
        .unwrap();
        // Both write leases are held at once, without sleeps or scheduler timing assumptions.
        drop(disjoint);
        drop(writer);
    }

    #[test]
    fn native_same_file_writers_and_readers_block_and_duplicates_promote_to_write() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, a, _) = native_fixture();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let root = temp.path().join("workspace");
        let writer = NativeResourceLease::acquire(
            &root,
            &[
                (a.clone(), LockMode::Shared),
                (a.clone(), LockMode::Exclusive),
            ],
            Duration::ZERO,
            None,
            "writer",
        )
        .unwrap();
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            assert!(NativeResourceLease::acquire(
                &root,
                &[(a.clone(), mode)],
                Duration::ZERO,
                None,
                "other"
            )
            .is_err());
        }
        crate::core::persist::atomic_write(a.path(), b"replacement").unwrap();
        let replaced = NativeResource::resolve(a.path()).unwrap();
        assert!(NativeResourceLease::acquire(
            &root,
            &[(replaced.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "other"
        )
        .is_err());
        drop(writer);
        NativeResourceLease::acquire(
            &root,
            &[(replaced, LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "other",
        )
        .unwrap();
    }

    #[test]
    fn native_lease_bounds_cancellation_and_partial_failure_release() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, a, b) = native_fixture();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let root = temp.path().join("workspace");
        assert!(NativeResourceLease::acquire(&root, &[], Duration::ZERO, None, "scope").is_err());
        let too_many = vec![(a.clone(), LockMode::Shared); MAX_NATIVE_RESOURCES + 1];
        assert!(
            NativeResourceLease::acquire(&root, &too_many, Duration::ZERO, None, "scope").is_err()
        );
        let cancel = crate::core::cancel::TurnCancel::new();
        cancel.cancel();
        assert!(NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            Some(&cancel),
            "cancelled"
        )
        .is_err());
        let (first, last) =
            if resource_lock("native-file", a.key()) < resource_lock("native-file", b.key()) {
                (a, b)
            } else {
                (b, a)
            };
        let held = NativeResourceLease::acquire(
            &root,
            &[(last.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "holder",
        )
        .unwrap();
        assert!(NativeResourceLease::acquire(
            &root,
            &[
                (first.clone(), LockMode::Exclusive),
                (last, LockMode::Exclusive)
            ],
            Duration::ZERO,
            None,
            "partial"
        )
        .is_err());
        // A failed multi-resource acquire must release its already-acquired first lock.
        NativeResourceLease::acquire(
            &root,
            &[(first, LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "released",
        )
        .unwrap();
        drop(held);
    }

    #[test]
    fn native_descendants_reuse_only_the_gate_not_resource_locks() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, a, b) = native_fixture();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let root = temp.path().join("workspace");
        let parent =
            WorkspaceWriterLease::acquire_scoped(&root, Duration::ZERO, None, "parent", "parent")
                .unwrap();
        let child = NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "parent/task/a",
        )
        .unwrap();
        for scope in ["parent", "parent/task/a/child", "parent/task/b"] {
            assert!(NativeResourceLease::acquire(
                &root,
                &[(a.clone(), LockMode::Exclusive)],
                Duration::ZERO,
                None,
                scope
            )
            .is_err());
        }
        let other = NativeResourceLease::acquire(
            &root,
            &[(b.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "parent/task/b",
        )
        .unwrap();
        assert!(NativeResourceLease::acquire(
            &root,
            &[(b, LockMode::Shared)],
            Duration::ZERO,
            None,
            "parent-unrelated"
        )
        .is_err());
        drop(parent);
        // Descendant ownership retains both the global and workspace exclusive handles.
        assert!(WorkspaceWriterLease::acquire_scoped(
            &root,
            Duration::ZERO,
            None,
            "outsider",
            "outsider"
        )
        .is_err());
        drop(other);
        drop(child);
        WorkspaceWriterLease::acquire_scoped(&root, Duration::ZERO, None, "after", "outsider")
            .unwrap();
    }

    #[test]
    fn global_exclusive_excludes_native_in_both_directions_and_across_roots() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, _a, _) = native_fixture();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let root = temp.path().join("workspace");
        let other_root = temp.path().join("other-workspace");
        std::fs::create_dir(&other_root).unwrap();
        let managed = NativeResource::resolve(&temp.path().join("managed.txt")).unwrap();
        let native = NativeResourceLease::acquire(
            &root,
            &[(managed.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "native",
        )
        .unwrap();
        for writer_root in [root.as_path(), other_root.as_path()] {
            assert!(WorkspaceWriterLease::acquire_scoped(
                writer_root,
                Duration::ZERO,
                None,
                "global",
                "opaque"
            )
            .is_err());
        }
        drop(native);
        let opaque = WorkspaceWriterLease::acquire_scoped(
            &other_root,
            Duration::ZERO,
            None,
            "global",
            "opaque",
        )
        .unwrap();
        assert!(NativeResourceLease::acquire(
            &root,
            &[(managed.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "native"
        )
        .is_err());
        // Authorized same-scope leases on different roots reuse ONLY the global exclusive gate.
        let second = WorkspaceWriterLease::acquire_scoped(
            &root,
            Duration::ZERO,
            None,
            "second root",
            "opaque",
        )
        .unwrap();
        let child = NativeResourceLease::acquire(
            &root,
            &[(managed, LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "opaque/task",
        )
        .unwrap();
        drop(opaque);
        drop(second);
        assert!(WorkspaceWriterLease::acquire_scoped(
            &root,
            Duration::ZERO,
            None,
            "unrelated",
            "other"
        )
        .is_err());
        drop(child);
    }

    #[test]
    fn native_actual_worker_retains_lease_after_observer_detaches() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, resource, _) = native_fixture();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let root = temp.path().join("workspace");
        let observer = NativeResourceLease::acquire(
            &root,
            &[(resource.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "worker",
        )
        .unwrap();
        let worker_lease = observer.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let lease = worker_lease;
            started.send(()).unwrap();
            let released = wait.recv_timeout(Duration::from_secs(5));
            drop(lease);
            released.unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(observer);
        let blocked = NativeResourceLease::acquire(
            &root,
            &[(resource.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "other",
        );
        let global_blocked =
            WorkspaceWriterLease::acquire_scoped(&root, Duration::ZERO, None, "global", "other");
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(blocked.is_err());
        assert!(global_blocked.is_err());
        NativeResourceLease::acquire(
            &root,
            &[(resource, LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "after",
        )
        .unwrap();
    }

    #[test]
    fn workspace_activity_weak_registry_drops_only_after_last_owner() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        require_workspace_quiescent(temp.path()).unwrap();
        let parent = WorkspaceActivity::begin(temp.path(), "parent/task").unwrap();
        let worker = parent.clone();
        let weak = std::sync::Arc::downgrade(&parent);
        assert!(require_workspace_quiescent(temp.path()).is_err());
        drop(parent);
        assert!(weak.upgrade().is_some());
        assert!(require_workspace_quiescent(temp.path()).is_err());
        drop(worker);
        assert!(weak.upgrade().is_none(), "registry must not own activity");
        require_workspace_quiescent(temp.path()).unwrap();
    }

    #[test]
    fn workspace_restore_refuses_live_activity_even_under_same_scope_exclusive() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let lease = WorkspaceWriterLease::acquire_scoped(
            temp.path(),
            Duration::ZERO,
            None,
            "restore",
            "parent",
        )
        .unwrap();
        let activity = WorkspaceActivity::begin(temp.path(), "parent").unwrap();
        assert!(
            WorkspaceRestoreGuard::acquire(temp.path()).is_err(),
            "live activity must refuse atomic restore admission"
        );
        assert!(
            require_workspace_quiescent(temp.path()).is_err(),
            "no holder-scope exemption for restore"
        );
        let other = temp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        require_workspace_quiescent(&other).unwrap();
        drop(activity);
        require_workspace_quiescent(temp.path()).unwrap();
        let restore = WorkspaceRestoreGuard::acquire(temp.path()).unwrap();
        assert!(
            WorkspaceRestoreGuard::acquire(temp.path()).is_err(),
            "nested restore must not deadlock"
        );
        drop(restore);
        drop(lease);
    }

    #[test]
    fn restore_admission_blocks_new_activity_until_outer_guard_drops() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        let identity = WorkspaceIdentity::discover_cached(temp.path()).unwrap();
        let restore = WorkspaceRestoreGuard::acquire(temp.path()).unwrap();
        assert!(
            matches!(
                ACTIVITIES.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ),
            "guard must retain the same registry mutex used by begin"
        );
        let (started, ready) = std::sync::mpsc::channel();
        let (seated, registered) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            // Public begin uses this same registration path after read-only identity discovery.
            let activity = begin_activity_identity(identity.worktree_id, "new-task").unwrap();
            seated.send(activity).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        // Mutex ownership, not a timed absence claim, proves begin cannot seat before release.
        assert!(matches!(
            registered.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(restore);
        let activity = registered.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert!(WorkspaceRestoreGuard::acquire(temp.path()).is_err());
        drop(activity);
        WorkspaceRestoreGuard::acquire(temp.path()).unwrap();
    }

    #[test]
    fn language_servers_coexist_with_scoped_native_while_mcp_servers_exclude_it() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, a, b) = native_fixture();
        with_test_lock_home(temp.path(), || {
            let root = temp.path().join("workspace");
            let approved = crate::core::native_scope::NativeWriteScope::resolve(
                &root,
                &["a.txt".into(), "b.txt".into()],
            )
            .unwrap();
            let ctx = crate::core::exec_ctx::ExecutionContext::default()
                .with_resource_scope("scoped")
                .with_native_scope(approved)
                .unwrap();

            // A live LANGUAGE SERVER no longer starves scoped native bodies. This is the regression
            // the maintainer hit: with one exclusive gate, an attached language server barred every
            // native body of every parallel child, so they silently edited nothing.
            let lsp = SidecarEffectLease::acquire(SidecarKind::LanguageServer, Duration::ZERO, None)
                .unwrap();
            let pooled =
                SidecarEffectLease::acquire(SidecarKind::LanguageServer, Duration::ZERO, None)
                    .unwrap();
            assert!(
                std::sync::Arc::ptr_eq(&lsp._locks[0], &pooled._locks[0]),
                "same-category starts pool the existing process holder"
            );
            crate::core::exec_ctx::with_current(ctx.clone(), || {
                let body = acquire_shared_sidecar(Duration::ZERO, None)
                    .expect("a scoped native body must coexist with a language server");
                drop(body);
            });
            // …while a live MCP SERVER still excludes it, because its effects are arbitrary.
            let mcp = SidecarEffectLease::acquire(SidecarKind::McpServer, Duration::ZERO, None)
                .unwrap();
            assert!(
                !std::sync::Arc::ptr_eq(&lsp._locks[0], &mcp._locks[0]),
                "the two categories hold separate gates and must not pool across them"
            );
            assert!(
                acquire_shared_sidecar(Duration::ZERO, None).is_err(),
                "a live MCP server excludes scoped native work"
            );
            crate::core::exec_ctx::with_current(ctx.clone(), || {
                assert!(NativeResourceLease::acquire(
                    &root,
                    &[(a.clone(), LockMode::Exclusive)],
                    Duration::ZERO,
                    None,
                    "scoped"
                )
                .is_err());
            });
            drop(mcp);
            // The controller's ordinary file IO remains usable while a sidecar is attached.
            NativeResourceLease::acquire(
                &root,
                &[(a.clone(), LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "parent",
            )
            .unwrap();

            // A scoped native body excludes a NEW MCP server and a new scoped body, while a language
            // server may still start alongside it.
            crate::core::exec_ctx::with_current(ctx, || {
                let first = NativeResourceLease::acquire(
                    &root,
                    &[(a, LockMode::Exclusive)],
                    Duration::ZERO,
                    None,
                    "scoped/a",
                )
                .unwrap();
                let second = NativeResourceLease::acquire(
                    &root,
                    &[(b, LockMode::Exclusive)],
                    Duration::ZERO,
                    None,
                    "scoped/b",
                )
                .unwrap();
                assert!(
                    SidecarEffectLease::acquire(SidecarKind::McpServer, Duration::ZERO, None).is_err(),
                    "scoped native shared barrier excludes a new MCP server"
                );
                SidecarEffectLease::acquire(SidecarKind::LanguageServer, Duration::ZERO, None)
                    .expect("a scoped native body must not bar a language server");
                drop(first);
                assert!(
                    SidecarEffectLease::acquire(SidecarKind::McpServer, Duration::ZERO, None).is_err()
                );
                drop(second);
            });
            SidecarEffectLease::acquire(SidecarKind::McpServer, Duration::ZERO, None).unwrap();
        });
    }

    #[test]
    fn independent_physical_gate_blocks_native_descendants_across_roots() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, resource, _) = native_fixture();
        let other_root = temp.path().join("other");
        std::fs::create_dir(&other_root).unwrap();
        with_test_lock_home(temp.path(), || {
            let root = resource.path().parent().unwrap();
            let dispatch = WorkspaceWriterLease::acquire_scoped(
                root,
                Duration::ZERO,
                None,
                "dispatch",
                "parent",
            )
            .unwrap();
            let child_dispatch = WorkspaceWriterLease::acquire_scoped(
                root,
                Duration::ZERO,
                None,
                "child dispatch",
                "parent/task/opaque",
            )
            .unwrap();
            let opaque = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "opaque body",
                "parent/task/opaque",
            )
            .unwrap();
            for scope in ["parent", "parent/task/native", "parent/task/opaque/child"] {
                for native_root in [root, other_root.as_path()] {
                    assert!(
                        NativeResourceLease::acquire(
                            native_root,
                            &[(resource.clone(), LockMode::Exclusive)],
                            Duration::ZERO,
                            None,
                            scope
                        )
                        .is_err(),
                        "physical opaque exclusion must not inherit ancestor reentry"
                    );
                }
                assert!(
                    PhysicalEffectLease::exclusive(Duration::ZERO, None, "other body", scope)
                        .is_err(),
                    "new opaque admission must not reenter either"
                );
            }
            drop(opaque);
            drop(child_dispatch);
            let native = NativeResourceLease::acquire(
                root,
                &[(resource.clone(), LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "parent/task/native",
            )
            .unwrap();
            assert!(
                PhysicalEffectLease::exclusive(Duration::ZERO, None, "opaque", "parent").is_err(),
                "shared physical native holder excludes an opaque body"
            );
            drop(native);
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "after", "parent").unwrap();
            drop(dispatch);
        });
    }

    #[test]
    fn opaque_background_physical_lease_outlives_detached_observer() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (temp, resource, _) = native_fixture();
        with_test_lock_home(temp.path(), || {
            let root = resource.path().parent().unwrap();
            let parent = WorkspaceWriterLease::acquire_scoped(
                root,
                Duration::ZERO,
                None,
                "parent dispatch",
                "parent",
            )
            .unwrap();
            let observer = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "background body",
                "parent/task/opaque",
            )
            .unwrap();
            let actual_body = observer.clone();
            let worker_dispatch = parent.clone();
            let (started, ready) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let physical = actual_body;
                let dispatch = worker_dispatch;
                started.send(()).unwrap();
                let released = wait.recv_timeout(Duration::from_secs(5));
                drop(physical);
                drop(dispatch);
                released.unwrap();
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(observer);
            drop(parent);
            let blocked = NativeResourceLease::acquire(
                root,
                &[(resource.clone(), LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "parent/task/native",
            );
            let opaque_blocked = PhysicalEffectLease::exclusive(
                Duration::ZERO,
                None,
                "another background body",
                "parent/task/opaque/child",
            );
            release.send(()).unwrap();
            worker.join().unwrap();
            assert!(
                blocked.is_err(),
                "bookkeeping reentry must not defeat detached body's physical exclusion"
            );
            assert!(opaque_blocked.is_err());
            NativeResourceLease::acquire(
                root,
                &[(resource.clone(), LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "parent/task/native",
            )
            .unwrap();
        });
    }

    #[test]
    fn test_lock_home_is_thread_local_nested_and_unwind_safe() {
        let (outer, _, _) = native_fixture();
        let (inner, _, _) = native_fixture();
        let identity = WorkspaceIdentity::discover_cached(outer.path()).unwrap();
        let saved = TEST_LOCK_HOME.with(|home| home.borrow().clone());
        with_test_lock_home(outer.path(), || {
            assert!(identity.lock_root().starts_with(outer.path()));
            assert!(resource_lock("test", "key").starts_with(outer.path()));
            assert!(store_lock("test", "key").starts_with(outer.path()));
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_test_lock_home(inner.path(), || {
                    assert!(identity.lock_root().starts_with(inner.path()));
                    panic!("exercise fixture-home RAII restoration");
                });
            }));
            assert!(unwind.is_err());
            assert!(identity.lock_root().starts_with(outer.path()));
            let root = outer.path().to_path_buf();
            let thread = std::thread::spawn(move || {
                assert!(
                    TEST_LOCK_HOME.with(|home| home.borrow().is_none()),
                    "override must not leak to another thread"
                );
                with_test_lock_home(&root, || {
                    assert!(resource_lock("test", "key").starts_with(&root))
                });
                assert!(TEST_LOCK_HOME.with(|home| home.borrow().is_none()));
            });
            thread.join().unwrap();
        });
        assert_eq!(TEST_LOCK_HOME.with(|home| home.borrow().clone()), saved);
    }

    #[test]
    fn native_lock_order_puts_resources_before_journals() {
        assert!(LockClass::Capacity < LockClass::SidecarEffect);
        assert!(LockClass::SidecarEffect < LockClass::PhysicalEffect);
        assert!(LockClass::PhysicalEffect < LockClass::RepositoryStore);
        assert!(LockClass::RepositoryStore < LockClass::Workspace);
        assert!(LockClass::Workspace < LockClass::Resource);
        assert!(LockClass::Resource < LockClass::Journal);
        assert!(LockClass::Resource < LockClass::TimeMachine);
    }

    #[test]
    fn stable_keys_hide_the_input_and_are_deterministic() {
        let input = "secret-token-value";
        let a = stable_key(input);
        assert_eq!(a, stable_key(input));
        assert_eq!(a.len(), 64);
        assert!(!a.contains(input));
    }

    /// A nested acquire inside one process must SUCCEED and must not release the OS locks early.
    ///
    /// Before reentrancy this deadlocked against itself: the agent loop holds the lease from the first
    /// writing tool of a turn to the end of the turn, so a delegated sub-agent's first edit waited the
    /// full timeout and then failed with "workspace writer lease was not acquired" — for a worktree
    /// nothing else was contending.
    #[test]
    fn a_nested_lease_in_one_process_is_granted_and_outlives_the_inner_handle() {
        // The lock root lives under `aizen_home()`: hold the home lock so a test that points
        // `AIZEN_HOME` at a temp dir and removes it cannot vanish the root mid-acquire.
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        let base = temp.path().to_path_buf();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let identity = WorkspaceIdentity::discover(&base).unwrap();

        let outer = WorkspaceWriterLease::acquire_identity(
            identity.clone(),
            Duration::from_millis(200),
            None,
            "outer",
        )
        .expect("first acquire holds the OS locks");
        assert!(WorkspaceWriterLease::held_by_this_process(&identity));

        let inner = WorkspaceWriterLease::acquire_identity(
            identity.clone(),
            Duration::from_millis(200),
            None,
            "nested",
        )
        .expect("nested acquire must be granted, not time out against our own handle");

        // Dropping the INNER handle must not surrender the worktree: the outer holder is still live.
        drop(inner);
        assert!(
            WorkspaceWriterLease::held_by_this_process(&identity),
            "inner drop must not release the lease the outer handle still owns"
        );

        drop(outer);
        assert!(
            !WorkspaceWriterLease::held_by_this_process(&identity),
            "outermost drop must clear the entry, or the next acquire would skip the OS lock and \
             silently lose cross-session exclusion"
        );

        // Proof the OS lock really came back: a fresh acquire succeeds immediately.
        WorkspaceWriterLease::acquire_identity(identity, Duration::from_millis(200), None, "after")
            .expect("lease must be re-acquirable once every handle is gone");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn worker_lease_outlives_ancestor_and_blocks_siblings() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let identity = WorkspaceIdentity::discover(temp.path()).unwrap();
        let short = Duration::from_millis(50);
        let owner = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            short,
            None,
            "ancestor",
            "parent",
        )
        .unwrap();
        let child = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            short,
            None,
            "child",
            "parent/task/1",
        )
        .unwrap();
        let worker = child.clone();
        drop(owner);
        drop(child);
        assert!(WorkspaceWriterLease::held_by_this_process(&identity));
        assert!(WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            short,
            None,
            "sibling",
            "other"
        )
        .is_err());
        drop(worker);
        assert!(!WorkspaceWriterLease::held_by_this_process(&identity));
        WorkspaceWriterLease::acquire_identity_scoped(identity, short, None, "after", "other")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[expect(
        clippy::await_holding_lock,
        reason = "hermetic test must serialize process-global AIZEN_HOME through worker completion"
    )]
    async fn blocking_worker_keeps_writer_lease_after_observer_timeout() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let identity = WorkspaceIdentity::discover(temp.path()).unwrap();
        let lease = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::from_secs(1),
            None,
            "worker lifetime",
            "parent/task",
        )
        .unwrap();
        let ctx =
            crate::core::exec_ctx::ExecutionContext::default().with_writer_lease(lease.clone());
        let worker_ctx = ctx.with_resource_scope("parent/task/tool");
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let mut worker = tokio::task::spawn_blocking(move || {
            crate::core::exec_ctx::with_current(worker_ctx, || {
                let _ = started.send(());
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            let _ = finished.send(());
        });
        ready.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut worker)
            .await
            .is_err());
        drop(worker);
        drop(ctx);
        drop(lease);
        let blocked = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            Duration::from_millis(20),
            None,
            "unrelated writer",
            "sibling",
        );
        release.send(()).unwrap();
        done.await.unwrap();
        assert!(
            blocked.is_err(),
            "observer timeout is not release of the actual writer"
        );
        WorkspaceWriterLease::acquire_identity_scoped(
            identity,
            Duration::from_secs(1),
            None,
            "after worker exit",
            "sibling",
        )
        .unwrap();
    }

    #[test]
    fn reentry_is_for_the_holder_and_its_descendants_only() {
        assert!(reentry_allowed("conv-A", "conv-A"));
        assert!(reentry_allowed("conv-A", "conv-A/task/3"));
        assert!(reentry_allowed("conv-A", "conv-A/workflow/7/implement"));
        assert!(!reentry_allowed("conv-A", "conv-B"));
        assert!(
            !reentry_allowed("conv-A", "conv-AB"),
            "a prefix is not an ancestor"
        );
        assert!(
            !reentry_allowed("conv-A/task/3", "conv-A"),
            "an ancestor does not reenter a child's lease"
        );
    }

    /// A child (descendant scope) reenters its parent's lease; a sibling scope in the same
    /// process does NOT — it waits for the OS lock and fails at the timeout, naming the holder.
    /// This is the exclusion two `serve` lanes on one worktree never had.
    #[test]
    fn a_sibling_scope_waits_for_the_os_lock_while_a_descendant_reenters() {
        let _home = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = crate::core::file_placement::tests::TestDir::new();
        let base = temp.path().to_path_buf();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", temp.path())]);
        let identity = WorkspaceIdentity::discover(&base).unwrap();
        let short = Duration::from_millis(200);
        let outer = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            short,
            None,
            "file_edit",
            "conv-A",
        )
        .expect("first acquire holds the OS locks");
        let child = WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            short,
            None,
            "child edit",
            "conv-A/task/1",
        )
        .expect("a descendant scope reenters its parent's lease");
        let err = match WorkspaceWriterLease::acquire_identity_scoped(
            identity.clone(),
            short,
            None,
            "sibling edit",
            "conv-B",
        ) {
            Ok(_) => panic!("a sibling scope must not reenter"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("conv-A") && err.contains("file_edit"), "{err}");
        drop(child);
        assert!(WorkspaceWriterLease::held_by_this_process(&identity));
        drop(outer);
        WorkspaceWriterLease::acquire_identity_scoped(identity, short, None, "later", "conv-B")
            .expect("free once every handle is gone");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn different_non_git_roots_get_different_worktree_ids() {
        let base = std::env::temp_dir().join(format!(
            "aizen-workspace-id-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let ia = WorkspaceIdentity::discover(&a).unwrap();
        let ib = WorkspaceIdentity::discover(&b).unwrap();
        assert_ne!(ia.worktree_id, ib.worktree_id);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn lock_set_sorts_requests_and_releases_all() {
        let base = std::env::temp_dir().join(format!(
            "aizen-lock-set-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let a = base.join("a.lock");
        let b = base.join("b.lock");
        let owner = LockOwner::new("test", "ordered locks");
        let locks = LockSet::acquire(
            vec![
                LockRequest::new(
                    LockClass::Workspace,
                    b.clone(),
                    LockMode::Exclusive,
                    "workspace",
                ),
                LockRequest::new(
                    LockClass::RepositoryStore,
                    a.clone(),
                    LockMode::Shared,
                    "store",
                ),
            ],
            Duration::from_millis(100),
            None,
            &owner,
        )
        .unwrap();
        assert_eq!(locks.len(), 2);
        drop(locks);
        RepoTxnLock::acquire_exclusive(&b, Duration::from_millis(100)).unwrap();
        let _ = std::fs::remove_dir_all(base);
    }

    /// The workspace lock is NOT reentrant, not even within one process: a second `LockSet` on the
    /// same path from the same process is refused. Any design that takes the writer lease while one
    /// is already held (a nested agent loop, a helper that re-acquires "just to be safe") must
    /// therefore reuse the held lease rather than ask for a second one.
    #[test]
    fn the_same_process_cannot_take_one_lock_twice() {
        let base = std::env::temp_dir().join(format!(
            "aizen-lock-reentrancy-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let path = base.join("x.lock");
        let owner = LockOwner::new("test", "reentrancy");
        let request = |label: &'static str| {
            vec![LockRequest::new(
                LockClass::Workspace,
                path.clone(),
                LockMode::Exclusive,
                label,
            )]
        };
        let first = LockSet::acquire(request("first"), Duration::from_millis(50), None, &owner)
            .expect("first acquisition must succeed");
        assert!(
            LockSet::acquire(request("second"), Duration::from_millis(50), None, &owner).is_err(),
            "a second lock on a path this process already holds must be refused"
        );
        drop(first);
        LockSet::acquire(request("third"), Duration::from_millis(100), None, &owner)
            .expect("the path must be lockable again once released");
        let _ = std::fs::remove_dir_all(base);
    }
}
