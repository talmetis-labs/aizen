//! Guarded skill storage and recoverable multi-path mutations. This is not a universal atomic
//! directory transaction: cooperating readers/writers take the store lock and recover the intent.
use super::{Skill, SkillLayout, SkillLocation, SkillOrigin, SkillProvenance};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SkillState {
    pub slug: String,
    pub version: u32,
    pub uses: u32,
    pub updated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SkillProvenance>,
}

impl SkillState {
    pub fn from_skill(sk: &Skill) -> Self {
        Self {
            slug: sk.key().to_string(),
            version: sk.version,
            uses: sk.uses,
            updated: sk.updated.clone(),
            provenance: sk.provenance.clone(),
        }
    }
}

pub(crate) fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

pub(crate) fn validate_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with(['.', ' '])
        || component.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        bail!("unsafe skill path component '{component}'");
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(*prefix).is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    }) {
        bail!("reserved skill path component '{component}'");
    }
    Ok(())
}

/// Reject links/reparse points in every existing ancestor, including configured HOME/root paths.
/// This preflight cannot defend against an unrelated process swapping an ancestor after the check;
/// the store lock coordinates Aizen processes, not hostile operating-system users.
pub(crate) fn guard(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            bail!("skill path contains parent traversal: {}", path.display());
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let linked = metadata.file_type().is_symlink();
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    linked || metadata.file_attributes() & 0x400 != 0
                };
                if linked {
                    bail!(
                        "skill path is a symlink/reparse point: {}",
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

pub(crate) fn exists(path: &Path) -> Result<bool> {
    guard(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

pub(crate) fn ensure_dir(path: &Path) -> Result<()> {
    guard(path)?;
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    guard(path)
}

pub(crate) fn state_path(root: &Path, slug: &str) -> PathBuf {
    root.join(".state").join(format!("{slug}.json"))
}

pub(crate) fn read_state(root: &Path, slug: &str) -> Result<Option<SkillState>> {
    let path = state_path(root, slug);
    if !exists(&path)? {
        return Ok(None);
    }
    if fs::symlink_metadata(&path)?.len() > 1024 * 1024 {
        bail!("skill state exceeds 1 MiB");
    }
    let state: SkillState = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("invalid skill state {}", path.display()))?;
    if super::sanitize_name(&state.slug) != state.slug || state.version == 0 {
        bail!("invalid skill state identity/version: {}", path.display());
    }
    Ok(Some(state))
}

pub(crate) fn write_state(root: &Path, key: &str, state: &SkillState) -> Result<()> {
    let path = state_path(root, key);
    ensure_dir(&root.join(".state"))?;
    guard(&path)?;
    crate::core::persist::atomic_write(&path, &serde_json::to_vec_pretty(state)?)
}

pub(crate) fn lock(root: &Path) -> Result<crate::core::repo_lock::RepoTxnLock> {
    ensure_dir(&root.join(".state"))?;
    let path = root.join(".state/.store.lock");
    guard(&path)?;
    let lock =
        crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&path, Duration::from_secs(5))?;
    recover(root)?;
    Ok(lock)
}

/// Called by readers only on an existing writable root; absent stores stay absent.
pub(crate) fn read_lock(root: &Path) -> Result<Option<crate::core::repo_lock::RepoTxnLock>> {
    if !exists(root)? {
        return Ok(None);
    }
    lock(root).map(Some)
}

pub(crate) fn location(
    root: &Path,
    slug: &str,
    origin: SkillOrigin,
) -> Result<Option<SkillLocation>> {
    guard(root)?;
    validate_component(slug)?;
    if slug.starts_with('.')
        || (origin == SkillOrigin::Global && root == super::skills_dir() && slug == "p")
    {
        return Ok(None);
    }
    let flat = root.join(format!("{slug}.md"));
    let bundle = root.join(slug);
    let has_flat = exists(&flat)?;
    let has_bundle_dir = exists(&bundle)?;
    let entry = bundle.join("SKILL.md");
    let has_bundle = has_bundle_dir && exists(&entry)?;
    if has_flat && has_bundle {
        bail!(
            "ambiguous skill '{slug}': both {} and {} exist",
            flat.display(),
            entry.display()
        );
    }
    if has_flat {
        if !fs::symlink_metadata(&flat)?.is_file() {
            bail!("skill entry is not a file: {}", flat.display());
        }
        return Ok(Some(SkillLocation {
            slug: slug.to_string(),
            root: root.to_path_buf(),
            resource_root: root.to_path_buf(),
            entry: flat,
            origin,
            layout: SkillLayout::Flat,
        }));
    }
    if has_bundle {
        if !fs::symlink_metadata(&bundle)?.is_dir() || !fs::symlink_metadata(&entry)?.is_file() {
            bail!("invalid skill bundle: {}", bundle.display());
        }
        // Validate all resources without following links; nested Markdown remains a resource.
        validate_tree(&bundle)?;
        return Ok(Some(SkillLocation {
            slug: slug.to_string(),
            root: root.to_path_buf(),
            resource_root: bundle,
            entry,
            origin,
            layout: SkillLayout::Bundle,
        }));
    }
    Ok(None)
}

pub(crate) fn collision(root: &Path, slug: &str) -> Result<()> {
    for path in [
        root.join(format!("{slug}.md")),
        root.join(slug),
        state_path(root, slug),
    ] {
        if exists(&path)? {
            bail!(
                "skill '{slug}' already exists or has pending local state: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn keys(root: &Path, global: bool) -> Result<Vec<String>> {
    guard(root)?;
    let mut keys = std::collections::BTreeSet::new();
    if !exists(root)? {
        return Ok(Vec::new());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || (global && name == "p") {
            continue;
        }
        let key = if path.extension().and_then(|s| s.to_str()) == Some("md") {
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("")
        } else {
            name
        };
        // Discovery never remaps an unsafe filesystem identity to somebody else's slug.
        if !key.is_empty() && super::sanitize_name(key) == key {
            keys.insert(key.to_string());
        }
    }
    Ok(keys.into_iter().collect())
}

fn tree_entries(path: &Path, depth: usize, count: &mut usize, bytes: &mut u64) -> Result<()> {
    guard(path)?;
    let metadata = fs::symlink_metadata(path)?;
    *count += 1;
    if depth > 32 || *count > 4096 {
        bail!("skill resource tree exceeds depth/entry limits");
    }
    if metadata.is_dir() {
        let mut names = std::collections::HashSet::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .context("skill resource filename must be UTF-8")?;
            validate_component(name)?;
            if !names.insert(name.to_lowercase()) {
                bail!("case-colliding skill resource filename: {name}");
            }
            tree_entries(&entry.path(), depth + 1, count, bytes)?;
        }
    } else if metadata.is_file() {
        *bytes = bytes
            .checked_add(metadata.len())
            .context("skill resource byte count overflow")?;
        if metadata.len() > 16 * 1024 * 1024 || *bytes > 64 * 1024 * 1024 {
            bail!("skill resource tree exceeds file/total byte limits");
        }
    } else {
        bail!("unsupported skill resource file type: {}", path.display());
    }
    Ok(())
}

pub(crate) fn validate_tree(path: &Path) -> Result<()> {
    tree_entries(path, 0, &mut 0, &mut 0)
}

#[cfg(all(test, unix))]
pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    copy_tree_document(source, destination, None)
}

/// Write a replacement entry while copied directories are still writable, then restore their
/// original modes bottom-up. This supports owner-read-only bundles without widening live modes.
pub(crate) fn copy_refined_bundle(
    source: &Path,
    destination: &Path,
    document: &[u8],
) -> Result<()> {
    copy_tree_document(source, destination, Some(document))
}

fn copy_tree_document(source: &Path, destination: &Path, replacement: Option<&[u8]>) -> Result<()> {
    validate_tree(source)?;
    let entry = source.join("SKILL.md");
    fn copy(
        source: &Path,
        destination: &Path,
        entry: &Path,
        replacement: Option<&[u8]>,
    ) -> Result<()> {
        guard(source)?;
        guard(destination)?;
        let metadata = fs::symlink_metadata(source)?;
        if metadata.is_dir() {
            fs::create_dir(destination)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Keep restrictive group/other permissions while guaranteeing owner access during
                // construction, even under an unusually restrictive umask.
                fs::set_permissions(
                    destination,
                    fs::Permissions::from_mode(metadata.permissions().mode() | 0o700),
                )?;
            }
            for child in fs::read_dir(source)? {
                let child = child?;
                copy(
                    &child.path(),
                    &destination.join(child.file_name()),
                    entry,
                    replacement,
                )?;
            }
            #[cfg(unix)]
            fs::set_permissions(destination, metadata.permissions())?;
        } else {
            if exists(destination)? {
                bail!("skill staging destination already exists");
            }
            if let Some(document) = replacement.filter(|_| source == entry) {
                crate::core::persist::atomic_write(destination, document)?;
            } else {
                let mut input = fs::File::open(source)?;
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(destination)?;
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
            }
            fs::set_permissions(destination, metadata.permissions())?;
        }
        Ok(())
    }
    copy(source, destination, &entry, replacement)
}

pub(crate) fn remove_owned(path: &Path) -> Result<()> {
    guard(path)?;
    if !exists(path)? {
        return Ok(());
    }
    // Validate links independently of import byte limits: a transaction can temporarily hold both
    // the old and replacement 64-MiB bundles, and rejected oversized staging must be removable.
    fn preflight(path: &Path, depth: usize, count: &mut usize) -> Result<()> {
        guard(path)?;
        *count += 1;
        if depth > 40 || *count > 16384 {
            bail!("skill cleanup tree exceeds safety bounds");
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                preflight(&entry?.path(), depth + 1, count)?;
            }
        } else if !metadata.is_file() {
            bail!("unsupported skill cleanup file type");
        }
        Ok(())
    }
    preflight(path, 0, &mut 0)?;
    fn remove(path: &Path) -> Result<()> {
        guard(path)?;
        if fs::symlink_metadata(path)?.is_dir() {
            for entry in fs::read_dir(path)? {
                remove(&entry?.path())?;
            }
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }
    remove(path)
}

fn owned_stages() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static OWNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    OWNED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

pub(crate) fn new_stage(root: &Path, prefix: &str) -> Result<PathBuf> {
    let base = root.join(".stage");
    ensure_dir(&base)?;
    let path = base.join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    ));
    guard(&path)?;
    fs::create_dir(&path)?;
    if prefix.starts_with("bundle-") {
        owned_stages()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.clone());
    }
    Ok(path)
}

pub(crate) fn bundle_owned(path: &Path) -> Result<()> {
    if !owned_stages()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
    {
        bail!(
            "skill staging directory was not created by this operation: {}",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn forget_bundle(path: &Path) {
    owned_stages()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

pub(crate) fn stage_owned(root: &Path, path: &Path, prefix: &str) -> Result<()> {
    guard(root)?;
    guard(path)?;
    if path.parent() != Some(root.join(".stage").as_path())
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with(prefix))
    {
        bail!("not an owned skill staging directory: {}", path.display());
    }
    if exists(path)? && !fs::symlink_metadata(path)?.is_dir() {
        bail!("skill staging path is not a directory");
    }
    Ok(())
}

/// Remove preparation leftovers only when no durable intent references them. If recovery could
/// not finish, leave the journal and its resources together for the next guarded reader.
pub(crate) struct StageCleanup {
    root: PathBuf,
    staging: PathBuf,
}
impl StageCleanup {
    pub fn new(root: &Path, staging: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            staging: staging.to_path_buf(),
        }
    }
}
impl Drop for StageCleanup {
    fn drop(&mut self) {
        if matches!(exists(&intent_path(&self.root)), Ok(false)) {
            let _ = remove_owned(&self.staging);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}
#[derive(Debug, Serialize, Deserialize)]
struct Intent {
    staging: PathBuf,
    moves: Vec<Move>,
    applied: usize,
    pending: bool,
    committed: bool,
}

fn intent_path(root: &Path) -> PathBuf {
    root.join(".state/.transaction.json")
}
fn save_intent(root: &Path, intent: &Intent) -> Result<()> {
    guard(&intent_path(root))?;
    crate::core::persist::atomic_write(&intent_path(root), &serde_json::to_vec_pretty(intent)?)
}

fn checked_relative(root: &Path, path: &Path) -> Result<PathBuf> {
    guard(path)?;
    let relative = path
        .strip_prefix(root)
        .context("skill transaction escaped its store")?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("unsafe skill transaction path");
    }
    Ok(relative.to_path_buf())
}

pub(crate) fn transact(root: &Path, staging: &Path, moves: Vec<Move>) -> Result<()> {
    stage_owned(root, staging, "txn-")?;
    let relative = moves
        .iter()
        .map(|m| {
            Ok(Move {
                from: checked_relative(root, &m.from)?,
                to: checked_relative(root, &m.to)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut intent = Intent {
        staging: checked_relative(root, staging)?,
        moves: relative,
        applied: 0,
        pending: false,
        committed: false,
    };
    if exists(&intent_path(root))? {
        bail!("unrecovered skill transaction exists");
    }
    save_intent(root, &intent)?;
    let result = (|| -> Result<()> {
        for operation in &intent.moves.clone() {
            let source = root.join(&operation.from);
            let destination = root.join(&operation.to);
            if !exists(&source)? || exists(&destination)? {
                bail!(
                    "skill transaction collision or missing source: {}",
                    destination.display()
                );
            }
            ensure_dir(
                destination
                    .parent()
                    .context("skill transaction missing parent")?,
            )?;
            intent.pending = true;
            save_intent(root, &intent)?;
            fs::rename(&source, &destination).with_context(|| {
                format!("moving {} to {}", source.display(), destination.display())
            })?;
            sync_dirs(&source, &destination);
            intent.applied += 1;
            intent.pending = false;
            save_intent(root, &intent)?;
        }
        intent.committed = true;
        save_intent(root, &intent)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            // A committed mutation stays committed even if cleanup is interrupted. The next reader
            // retries cleanup; never report a rollback when the commit marker reached disk.
            let _ = recover(root);
            Ok(())
        }
        Err(error) => {
            recover(root).context("skill rollback failed; durable intent retained for recovery")?;
            Err(error)
        }
    }
}

fn sync_dirs(_source: &Path, _destination: &Path) {
    #[cfg(unix)]
    for path in [_source, _destination] {
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
}

pub(crate) fn recover(root: &Path) -> Result<()> {
    let path = intent_path(root);
    if !exists(&path)? {
        return Ok(());
    }
    if fs::symlink_metadata(&path)?.len() > 1024 * 1024 {
        bail!("skill transaction intent exceeds 1 MiB");
    }
    let mut intent: Intent =
        serde_json::from_slice(&fs::read(&path)?).context("invalid skill transaction intent")?;
    if intent.moves.len() > 16 {
        bail!("skill transaction has too many operations");
    }
    for operation in &intent.moves {
        for path in [&operation.from, &operation.to] {
            if path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
                || path.as_os_str().is_empty()
            {
                bail!("unsafe skill recovery intent");
            }
            guard(&root.join(path))?;
        }
    }
    let staging = root.join(&intent.staging);
    stage_owned(root, &staging, "txn-")?;
    if intent.applied > intent.moves.len() {
        bail!("invalid skill recovery progress");
    }
    if !intent.committed {
        if intent.pending {
            let operation = intent
                .moves
                .get(intent.applied)
                .context("invalid pending skill recovery operation")?;
            let from = exists(&root.join(&operation.from))?;
            let to = exists(&root.join(&operation.to))?;
            match (from, to) {
                (false, true) => intent.applied += 1,
                (true, false) => {},
                _ => bail!("skill recovery paths changed outside the store transaction; refusing overwrite"),
            }
            intent.pending = false;
            save_intent(root, &intent)?;
        }
        while intent.applied > 0 {
            let operation = &intent.moves[intent.applied - 1];
            let source = root.join(&operation.to);
            let destination = root.join(&operation.from);
            match (exists(&source)?, exists(&destination)?) {
                (true, false) => {
                    ensure_dir(
                        destination
                            .parent()
                            .context("skill rollback missing parent")?,
                    )?;
                    fs::rename(&source, &destination)?;
                    sync_dirs(&source, &destination);
                }
                // Recovery itself may have been killed after this rename but before decrementing.
                (false, true) => {}
                _ => bail!("skill rollback paths changed; refusing overwrite"),
            }
            intent.applied -= 1;
            save_intent(root, &intent)?;
        }
    }
    remove_owned(&staging)?;
    fs::remove_file(path)?;
    Ok(())
}

pub(crate) fn unique_archive(root: &Path, preferred: &str) -> Result<String> {
    let archive = root.join(".archive");
    ensure_dir(&archive)?;
    for suffix in 0..10000 {
        let key = if suffix == 0 {
            preferred.to_string()
        } else {
            format!("{preferred}-{suffix}")
        };
        if collision(&archive, &key).is_ok() {
            return Ok(key);
        }
        // Ensure link/permission failures are not silently treated as ordinary name collisions.
        guard(&archive.join(&key))?;
        guard(&archive.join(format!("{key}.md")))?;
        guard(&state_path(&archive, &key))?;
    }
    bail!("too many archived copies for skill '{preferred}'")
}

pub(crate) fn payload(location: &SkillLocation) -> PathBuf {
    match location.layout {
        SkillLayout::Flat => location.entry.clone(),
        SkillLayout::Bundle => location.resource_root.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "aizen-skill-store-{tag}-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        let root = fs::canonicalize(&root).unwrap();
        root
    }

    #[test]
    fn transaction_rolls_back_first_move_when_second_collides() {
        let root = fixture("rollback");
        let _lock = lock(&root).unwrap();
        let stage = new_stage(&root, "txn").unwrap();
        let source = root.join("old.md");
        let archive = root.join(".archive/old.md");
        fs::write(&source, b"original").unwrap();
        fs::write(stage.join("new"), b"replacement").unwrap();
        fs::write(root.join("occupied"), b"other").unwrap();
        let result = transact(
            &root,
            &stage,
            vec![
                Move {
                    from: source.clone(),
                    to: archive.clone(),
                },
                Move {
                    from: stage.join("new"),
                    to: root.join("occupied"),
                },
            ],
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&source).unwrap(), b"original");
        assert_eq!(fs::read(root.join("occupied")).unwrap(), b"other");
        assert!(!archive.exists());
        assert!(!intent_path(&root).exists());
        drop(_lock);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_infers_pending_move_and_committed_cleanup() {
        for committed in [false, true] {
            let root = fixture("recover");
            let stage = new_stage(&root, "txn").unwrap();
            let source = root.join("old.md");
            let destination = root.join("archived.md");
            fs::write(&destination, b"original").unwrap();
            ensure_dir(&root.join(".state")).unwrap();
            save_intent(
                &root,
                &Intent {
                    staging: stage.strip_prefix(&root).unwrap().to_path_buf(),
                    moves: vec![Move {
                        from: PathBuf::from("old.md"),
                        to: PathBuf::from("archived.md"),
                    }],
                    applied: if committed { 1 } else { 0 },
                    pending: !committed,
                    committed,
                },
            )
            .unwrap();
            recover(&root).unwrap();
            assert_eq!(source.exists(), !committed);
            assert_eq!(destination.exists(), committed);
            assert!(!stage.exists());
            assert!(!intent_path(&root).exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_preserves_directory_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture("permissions");
        let source = root.join("source");
        let destination = root.join("copy");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/asset.bin"), [0, 255, 128]).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(source.join("nested"), fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(
            source.join("nested/asset.bin"),
            fs::Permissions::from_mode(0o400),
        )
        .unwrap();

        copy_tree(&source, &destination).unwrap();
        for (relative, expected) in [("", 0o700), ("nested", 0o500), ("nested/asset.bin", 0o400)] {
            let mode = fs::symlink_metadata(destination.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, expected, "permission changed for {relative}");
        }
        assert_eq!(
            fs::read(destination.join("nested/asset.bin")).unwrap(),
            [0, 255, 128]
        );
        // Restore write permission only for fixture cleanup, never for the copied production tree.
        for tree in [&source, &destination] {
            fs::set_permissions(tree.join("nested"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recursive_copy_and_cleanup_never_follow_links() {
        use std::os::unix::fs::symlink;
        let root = fixture("links");
        let outside = fixture("outside");
        fs::write(outside.join("keep"), b"untouched").unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        assert!(validate_tree(&root).is_err());
        assert!(copy_tree(&root, &root.with_extension("copy")).is_err());
        assert!(remove_owned(&root).is_err());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"untouched");
        fs::remove_file(root.join("linked")).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
