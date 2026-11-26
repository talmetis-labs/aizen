//! Read-only target selection for trusted human ingress, never a model tool or permission grant.
//! Persist `ControlContext::transition` before constructing the new execution context/registry.

use anyhow::{ensure, Context, Result};
use std::path::{Path, PathBuf};

use super::plan_document::{self, ReadPlan};
use crate::core::file_placement::is_link_or_reparse;
use crate::core::session_control::PlanTarget;
use crate::core::workspace_txn::NativeResource;

const ALLOCATION_ATTEMPTS: usize = 8;

/// Selection is not attachment. An allocated target has no saved observation or document bytes.
#[derive(Debug)]
pub(crate) struct SelectedPlan {
    pub target: PlanTarget,
    pub observation: Option<ReadPlan>,
}

/// Call only for a fresh, trusted human planning request at a settled conversation boundary.
/// An omitted path reuses a checked same-ID selected document; a missing selection allocates a
/// fresh absent target. Malformed/unsafe/different-ID existing selections fail, never silently move.
/// Existing documents selected explicitly take their identity from a bounded checked disk read.
/// This does not modify control/task state, create directories, save a draft, or authorize execution.
pub(crate) fn select_target(
    workspace: &Path,
    explicit_path: Option<&str>,
    selected: Option<&PlanTarget>,
) -> Result<SelectedPlan> {
    select_with_id(workspace, explicit_path, selected, plan_document::new_id)
}

fn select_with_id(
    workspace: &Path,
    explicit_path: Option<&str>,
    selected: Option<&PlanTarget>,
    mut new_id: impl FnMut() -> Result<String>,
) -> Result<SelectedPlan> {
    let root = checked_workspace(workspace)?;
    if let Some(path) = explicit_path {
        plan_document::validate_relative_path(path)?;
        if let Some(document) = read_selected(&root, path)? {
            return selection(&root, path, document.parsed.id.clone(), Some(document));
        }
        return selection(&root, path, new_id()?, None);
    }
    if let Some(selected) = selected {
        selected.validate()?;
        ensure!(
            checked_workspace(Path::new(&selected.root))? == root,
            "selected plan belongs to a different workspace; select a plan explicitly"
        );
        if let Some(document) = read_selected(&root, &selected.path)? {
            ensure!(
                document.parsed.id == selected.id,
                "selected plan identity changed; select the document explicitly to reattach"
            );
            return selection(&root, &selected.path, selected.id.clone(), Some(document));
        }
    }
    for _ in 0..ALLOCATION_ATTEMPTS {
        let id = new_id()?;
        ensure!(
            plan_document::valid_plan_id(&id),
            "invalid generated plan ID"
        );
        let path = format!("docs/plans/{id}.md");
        if checked_absent(&root, &path)? {
            return selection(&root, &path, id, None);
        }
    }
    anyhow::bail!("could not allocate an absent plan target after {ALLOCATION_ATTEMPTS} attempts")
}

fn selection(
    root: &Path,
    path: &str,
    id: String,
    observation: Option<ReadPlan>,
) -> Result<SelectedPlan> {
    let target = PlanTarget {
        root: root
            .to_str()
            .context("plan workspace must be Unicode")?
            .to_owned(),
        path: path.to_owned(),
        id,
    };
    target.validate()?;
    Ok(SelectedPlan {
        target,
        observation,
    })
}

fn checked_workspace(workspace: &Path) -> Result<PathBuf> {
    ensure!(
        workspace.is_absolute()
            && workspace
                .to_str()
                .is_some_and(|root| { root.len() <= 4096 && !root.chars().any(char::is_control) }),
        "plan workspace must be a bounded absolute path"
    );
    NativeResource::validate_spelling(workspace)?;
    // Check supplied ancestry first: canonicalization must not hide a symlink/reparse point.
    for ancestor in workspace.ancestors() {
        let md = std::fs::symlink_metadata(ancestor).context("checking plan workspace ancestry")?;
        ensure!(
            md.is_dir() && !is_link_or_reparse(&md),
            "plan workspace ancestry must be non-link directories"
        );
    }
    let root = workspace
        .canonicalize()
        .context("canonicalizing plan workspace")?;
    ensure!(
        crate::core::config::normalize_path_key(workspace)
            == crate::core::config::normalize_path_key(&root),
        "plan workspace needs its canonical spelling"
    );
    Ok(root)
}

/// Validate ancestors without creating them. NativeResource binds the nearest existing ancestry;
/// only the fixed docs/plans parents may be absent. This is an observation, not a reserved pathname.
fn checked_slot(root: &Path, relative: &str) -> Result<Option<NativeResource>> {
    plan_document::validate_relative_path(relative)?;
    let target = root.join(relative);
    let mut missing = Vec::new();
    for parent in target
        .parent()
        .context("plan target has no parent")?
        .ancestors()
    {
        match std::fs::symlink_metadata(parent) {
            Ok(md) => ensure!(
                md.is_dir() && !is_link_or_reparse(&md),
                "plan parents must be non-link directories"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    parent == root.join("docs") || parent == root.join("docs/plans"),
                    "nested plan directories must already exist"
                );
                missing.push(parent.to_path_buf());
            }
            Err(error) => return Err(error).context("checking plan parents"),
        }
    }
    if let Some(nearest) = missing.last() {
        let guard = NativeResource::resolve(nearest)?;
        ensure!(
            guard.path() == nearest,
            "plan parent path has an ambiguous spelling"
        );
        for path in missing.iter().chain(std::iter::once(&target)) {
            ensure!(
                is_absent(path)?,
                "plan ancestry changed during selection; retry"
            );
        }
        guard.verify()?;
        // Creation is not accepted merely because NativeResource allows regular-file replacement.
        ensure!(
            is_absent(nearest)?,
            "plan ancestry changed during selection; retry"
        );
        return Ok(None);
    }
    let resource = NativeResource::resolve(&target)?;
    ensure!(
        resource.path() == target,
        "plan path needs its canonical spelling"
    );
    Ok(Some(resource))
}

fn is_absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).context("checking plan target existence"),
    }
}

fn checked_absent(root: &Path, relative: &str) -> Result<bool> {
    let Some(resource) = checked_slot(root, relative)? else {
        return Ok(true);
    };
    let absent = is_absent(resource.path())?;
    resource.verify()?;
    ensure!(
        is_absent(resource.path())? == absent,
        "plan target changed during allocation; retry"
    );
    Ok(absent)
}

fn read_selected(root: &Path, relative: &str) -> Result<Option<ReadPlan>> {
    let Some(resource) = checked_slot(root, relative)? else {
        return Ok(None);
    };
    if is_absent(resource.path())? {
        resource.verify()?;
        ensure!(
            is_absent(resource.path())?,
            "plan target changed during selection; retry"
        );
        return Ok(None);
    }
    let observed = plan_document::read(root, relative)
        .context("selected plan must be a valid modern contract; legacy migration requires an explicit migration operation")?;
    resource.verify()?;
    Ok(Some(observed))
}

#[cfg(test)]
#[path = "plan_control_tests.rs"]
mod tests;
