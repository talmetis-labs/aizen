//! Git-owned operations shared by the tool and workspace coordinator.
//! No custom object/ref storage or authority inferred from inspection results.

use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::{collections::HashSet, path::Path, time::Duration};

#[path = "git_ops_change.rs"]
mod change;
pub(crate) use change::{
    accepted_task_integration, inspect_operation, operation_receipts, require_verified_destination,
    valid_operation_id, verification_records, Change, PathInverse, Prepared, PreparedOutcome,
    VerifiedTaskIntegration,
};
#[cfg(test)]
pub(crate) use change::{ChangeReceipt, PreparedChange, PreparedVerification, VerificationRecord};

/// Pending, malformed or uncertain outcomes block new mutations and workspace removal.
/// Reading metadata does not settle an operation or authorize replay.
pub(crate) fn require_settled_operations(
    identity: &crate::core::workspace_txn::WorkspaceIdentity,
) -> Result<()> {
    change::require_no_pending(identity)
}

/// Navigation may leave a physically settled conflict intact, but never an admitted/unknown write.
pub(crate) fn require_focus_settled(
    identity: &crate::core::workspace_txn::WorkspaceIdentity,
) -> Result<()> {
    change::require_focus_settled(identity)
}

#[path = "git_ops_focus.rs"]
mod focus;
pub(crate) use focus::PreparedFocus;

#[path = "git_ops_owned.rs"]
mod owned;
pub(crate) use crate::core::git_native_lifecycle::{GitNativeAttempt, GitNativeLaunchTicket};
pub(crate) use owned::{
    OwnedTask, OwnedTaskAllocationOwner, OwnedTaskAllocator, PreparedOwnedTask, TaskHandoff,
};

pub(crate) fn validate_task_handoff(handoff: &TaskHandoff) -> Result<()> {
    change::validate_task_handoff(handoff)
}

pub(crate) fn require_workflow_ancestors(
    root: &Path,
    tip: &crate::core::snapshots::GitOid,
    ancestors: &[crate::core::snapshots::GitOid],
) -> Result<()> {
    change::require_workflow_ancestors(root, tip, ancestors)
}

pub(crate) fn workflow_destination(
    root: &Path,
) -> Result<(crate::core::snapshots::GitOid, String)> {
    let identity = create::inspect_identity(root, None)?;
    change::require_no_pending(&identity)?;
    change::require_workflow_clean(&identity)?;
    Ok((
        create::head(root, None)?,
        create::source_branch(root, None)?
            .context("workflow destination requires a local branch")?,
    ))
}

#[path = "git_ops_create.rs"]
mod create;
pub(crate) use create::{workspace_identity, CreatedWorktree, PreparedWorktreeCreation};

pub(crate) const INSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WORKTREES: usize = 4096;

/// Build a read-only Git invocation. Callers supply only fixed, validated inspection argv.
/// Do not reuse this profile for commits: user-facing mutations must preserve hooks/signing.
pub(crate) fn prepare_inspection(
    root: &Path,
    args: Vec<String>,
) -> Result<crate::sandbox::runner::Sandboxed<std::process::Command>> {
    crate::agent::planning::check_automation_boundary()?;
    let executable = crate::core::gitx::git_exe().ok_or(crate::core::gitx::GitMissing)?;
    let mut argv = vec!["--no-pager".into(), "--literal-pathspecs".into()];
    for value in [
        "color.ui=false",
        "core.fsmonitor=false",
        "log.showSignature=false",
        "gc.auto=0",
        "maintenance.auto=false",
        "credential.helper=",
    ] {
        argv.extend(["-c".into(), value.into()]);
    }
    argv.extend(args);
    let request = crate::sandbox::request::SandboxRequest::exec(
        crate::sandbox::CommandOrigin::GitInspect,
        executable,
        argv,
        root.to_path_buf(),
        root.to_path_buf(),
    )
    .wall_timeout(INSPECT_TIMEOUT);
    #[cfg(unix)]
    let request = request.with_git_metadata(linked_git_metadata(root));
    let mut prepared = crate::sandbox::runner::prepare_std(request)?;
    configure_inspection_environment(
        &mut prepared.command,
        std::env::vars_os().map(|(name, _)| name),
    );
    Ok(prepared)
}

fn configure_inspection_environment(
    command: &mut std::process::Command,
    inherited_names: impl IntoIterator<Item = std::ffi::OsString>,
) {
    // Keep ordinary host/local configuration and effective ignores. Only inherited routing,
    // injection, and executable behavior are overridden; never change repository configuration.
    for name in inherited_names {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if matches!(
            upper.as_str(),
            "GIT_DIR"
                | "GIT_WORK_TREE"
                | "GIT_COMMON_DIR"
                | "GIT_INDEX_FILE"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_NAMESPACE"
                | "GIT_GRAFT_FILE"
                | "GIT_SHALLOW_FILE"
                | "GIT_REPLACE_REF_BASE"
                | "GIT_NO_REPLACE_OBJECTS"
                | "GIT_CONFIG_COUNT"
                | "GIT_CONFIG_PARAMETERS"
                | "GIT_EXTERNAL_DIFF"
                | "GIT_DIFF_OPTS"
                | "GIT_LITERAL_PATHSPECS"
                | "GIT_GLOB_PATHSPECS"
                | "GIT_NOGLOB_PATHSPECS"
                | "GIT_ICASE_PATHSPECS"
        ) || upper.starts_with("GIT_CONFIG_KEY_")
            || upper.starts_with("GIT_CONFIG_VALUE_")
            || upper.starts_with("GIT_TRACE")
        {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
}

/// A linked checkout stores Git's objects, refs and worktree registry outside its workspace.
/// Only use that directory when Git's forward and back pointers agree with the worktree layout.
#[cfg(unix)]
fn linked_git_metadata(root: &Path) -> Option<std::path::PathBuf> {
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    fn pointer(path: &Path) -> Option<String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut text = String::new();
        file.take(4097).read_to_string(&mut text).ok()?;
        let line = text.trim_end_matches(['\r', '\n']);
        (text.len() <= 4096 && !line.is_empty() && !line.contains(['\n', '\r', '\0']))
            .then(|| line.to_owned())
    }

    let root = root.canonicalize().ok()?;
    let (checkout, marker) = root.ancestors().find_map(|directory| {
        let dot_git = directory.join(".git");
        fs::symlink_metadata(&dot_git)
            .ok()
            .map(|_| (directory, dot_git))
    })?;
    let admin = checkout
        .join(pointer(&marker)?.strip_prefix("gitdir: ")?)
        .canonicalize()
        .ok()?;
    let registry = admin.parent()?;
    let common = registry.parent()?;
    if registry.file_name()? != "worktrees"
        || common.file_name()? != ".git"
        || !common.join("objects").is_dir()
        || admin
            .join(pointer(&admin.join("commondir"))?)
            .canonicalize()
            .ok()?
            != common
        || admin
            .join(pointer(&admin.join("gitdir"))?)
            .canonicalize()
            .ok()?
            != marker
    {
        return None;
    }
    Some(common.to_path_buf())
}

/// Complete inventory, including worktrees created outside Aizen. Unknown settlement and truncated
/// transport are errors, never an empty list that could later authorize removal.
pub(crate) fn worktree_inventory(root: &Path) -> Result<Vec<WorktreeEntry>> {
    let output = inspection_bytes(
        root,
        ["worktree", "list", "--porcelain", "-z"]
            .map(String::from)
            .to_vec(),
    )?;
    parse_worktrees(&output)
}

/// Complete fixed-query output for preconditions, never a truncated display string.
pub(crate) fn inspection_bytes(root: &Path, args: Vec<String>) -> Result<Vec<u8>> {
    inspection_bytes_owned(root, args, None)
}

pub(crate) fn inspection_bytes_owned(
    root: &Path,
    args: Vec<String>,
    owner: Option<crate::core::proctree::BoundedOwner>,
) -> Result<Vec<u8>> {
    let prepared = prepare_inspection(root, args)?;
    Ok(checked_output(prepared.output_bounded_owned(
        None,
        INSPECT_TIMEOUT,
        Duration::from_secs(2),
        owner,
    ))?
    .stdout)
}

/// Status alone cannot prove cleanliness for assume-unchanged, skip-worktree, or fsmonitor-valid
/// index entries. Refuse these states rather than clearing flags or trusting a cached clean result.
pub(crate) fn validate_visible_index(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.is_empty() || bytes.ends_with(&[0]),
        "incomplete Git index flag output"
    );
    if !bytes.is_empty() {
        for record in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
            ensure!(record.len() > 2 && record.starts_with(b"H "),
                "worktree index has hidden or unsupported entries (assume-unchanged, skip-worktree, fsmonitor, or unmerged); inspect it before removal");
        }
    }
    Ok(())
}

/// Sequencer/merge state is logically unfinished even when every process has exited.
pub(crate) fn require_idle_operation(git_dir: &Path) -> Result<()> {
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_LOG",
    ] {
        match std::fs::symlink_metadata(git_dir.join(marker)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("checking worktree operation state"),
            Ok(_) => anyhow::bail!(
                "worktree has active Git operation state ({marker}); inspect it before continuing"
            ),
        }
    }
    Ok(())
}

/// Remove one preflighted worktree through the same policy/physical-settlement boundary. The
/// caller must retain its target workspace lease and validate ownership; this never adds --force.
pub(crate) fn remove_worktree(
    repository: &Path,
    target: &Path,
    owner: crate::core::proctree::BoundedOwner,
) -> Result<()> {
    let path = target
        .to_str()
        .context("unsupported non-UTF-8 worktree path")?;
    #[cfg(windows)]
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let prepared = prepare_mutation(
        repository,
        target,
        vec!["worktree".into(), "remove".into(), "--".into(), path.into()],
    )?;
    checked_output(prepared.output_bounded_owned(
        None,
        INSPECT_TIMEOUT,
        Duration::from_secs(2),
        Some(owner),
    ))?;
    Ok(())
}

fn prepare_mutation(
    cwd: &Path,
    workspace: &Path,
    args: Vec<String>,
) -> Result<crate::sandbox::runner::Sandboxed<std::process::Command>> {
    crate::agent::planning::check_automation_boundary()?;
    let git = crate::core::gitx::git_exe().ok_or(crate::core::gitx::GitMissing)?;
    let request = crate::sandbox::request::SandboxRequest::exec(
        crate::sandbox::CommandOrigin::GitMutation,
        git,
        args,
        cwd.to_path_buf(),
        workspace.to_path_buf(),
    )
    .wall_timeout(INSPECT_TIMEOUT);
    #[cfg(unix)]
    let request = request.with_git_metadata(
        linked_git_metadata(workspace)
            .into_iter()
            .chain(linked_git_metadata(cwd)),
    );
    let mut prepared = crate::sandbox::runner::prepare_std(request)?;
    // No hardened-command profile: normal configured hooks/signing are not suppressed.
    configure_inspection_environment(
        &mut prepared.command,
        std::env::vars_os().map(|(name, _)| name),
    );
    Ok(prepared)
}

fn checked_output(
    result: std::result::Result<
        crate::core::proctree::BoundedBytes,
        crate::core::proctree::BoundedExecutionError,
    >,
) -> Result<crate::core::proctree::BoundedBytes> {
    use crate::core::proctree::BoundedSettlement;
    let output = match result {
        Ok(output) if output.settlement == BoundedSettlement::Proven => output,
        Ok(_) => {
            return Err(
                anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(
                    crate::agent::tools::ToolEffectState::UnknownWorker,
                ))
                .context("Git command did not physically settle"),
            )
        }
        Err(error) if error.settlement == BoundedSettlement::Unknown => {
            return Err(
                anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(
                    crate::agent::tools::ToolEffectState::UnknownWorker,
                ))
                .context(format!("Git command did not settle: {error}")),
            );
        }
        Err(error) => return Err(error).context("running Git command"),
    };
    ensure!(!output.timed_out, "Git command timed out");
    ensure!(!output.output_truncated, "Git command was truncated");
    ensure!(
        output.code == Some(0),
        "Git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(4096)
            .collect::<String>()
    );
    Ok(output)
}

/// Observed state, not permission to edit/remove this worktree. Git also lists external worktrees.
#[derive(Debug, Serialize)]
pub(crate) struct WorktreeEntry {
    pub path: String,
    pub head: Option<crate::core::snapshots::GitOid>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

/// Parse only a complete `git worktree list --porcelain -z` response. Never parse human output or
/// replace invalid path bytes lossily; unsupported encodings cannot identify a mutation target.
pub(crate) fn parse_worktrees(bytes: &[u8]) -> Result<Vec<WorktreeEntry>> {
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "worktree inventory exceeds byte limit"
    );
    ensure!(bytes.ends_with(b"\0\0"), "incomplete worktree inventory");
    let mut entries = Vec::new();
    let mut fields: Vec<&str> = Vec::new();
    let mut paths = HashSet::new();
    for field in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        if !field.is_empty() {
            fields
                .push(std::str::from_utf8(field).context("unsupported non-UTF-8 worktree record")?);
            ensure!(fields.len() <= 8, "too many worktree fields");
            continue;
        }
        ensure!(!fields.is_empty(), "empty worktree record");
        let path = fields[0]
            .strip_prefix("worktree ")
            .filter(|path| !path.is_empty())
            .context("worktree record must start with a path")?;
        ensure!(paths.insert(path.to_owned()), "duplicate worktree path");
        let mut entry = WorktreeEntry {
            path: path.into(),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        };
        let mut seen = HashSet::new();
        for field in &fields[1..] {
            let (key, value) = field.split_once(' ').unwrap_or((field, ""));
            ensure!(seen.insert(key), "duplicate worktree field: {key}");
            match key {
                "HEAD" => {
                    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|b| b == b'0') {
                        entry.head = Some(crate::core::snapshots::GitOid::parse(value)?);
                    }
                }
                "branch" => {
                    ensure!(
                        value.starts_with("refs/heads/") && value.len() > 11,
                        "invalid worktree branch"
                    );
                    entry.branch = Some(value.into());
                }
                "bare" | "detached" => {
                    ensure!(value.is_empty(), "unexpected value for {key}");
                    entry.bare |= key == "bare";
                    entry.detached |= key == "detached";
                }
                "locked" => entry.locked = Some(value.into()),
                "prunable" => entry.prunable = Some(value.into()),
                _ => anyhow::bail!("unsupported worktree field: {key}"),
            }
        }
        if entry.bare {
            ensure!(
                !seen.contains("HEAD") && entry.branch.is_none() && !entry.detached,
                "inconsistent bare worktree record"
            );
        } else {
            ensure!(
                seen.contains("HEAD") && (entry.branch.is_some() != entry.detached),
                "incomplete worktree HEAD/branch state"
            );
            ensure!(
                !entry.detached || entry.head.is_some(),
                "detached worktree has no commit"
            );
        }
        entries.push(entry);
        ensure!(entries.len() <= MAX_WORKTREES, "too many worktrees");
        fields.clear();
    }
    ensure!(
        fields.is_empty() && !entries.is_empty(),
        "incomplete worktree records"
    );
    Ok(entries)
}

#[cfg(test)]
#[path = "git_ops_tests.rs"]
mod tests;
