//! Git-generated selected inverse, staged and committed only in an explicitly owned checkout.
//! The destination is never edited here. Conflicts/partial effects retain their ownership barrier.
use super::*;
use crate::core::{exec_ctx, persist::FileFingerprint};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathInverse {
    pub commit: GitOid,
    pub parent: GitOid,
    pub mainline: Option<u32>,
    pub paths: Vec<String>,
}

impl PathInverse {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.commit != self.parent
                && self.commit.as_str().len() == self.parent.as_str().len()
                && self.mainline.is_none_or(|n| n > 0)
                && !self.paths.is_empty()
                && self.paths.len() <= 32
                && self.paths.windows(2).all(|p| p[0] < p[1]),
            "invalid selected inverse evidence"
        );
        ensure!(
            serde_json::to_vec_pretty(self)?.len() <= 24 * 1024,
            "selected inverse metadata exceeds the handoff evidence budget"
        );
        for path in &self.paths {
            super::super::owned::validate_relative(path)?;
            // These are exact Git filenames, not host-dependent path aliases.
            ensure!(
                !path.contains('\\')
                    && !path.contains(':')
                    && !path.contains("//")
                    && !path
                        .split('/')
                        .any(|p| p == "." || p == ".." || p.is_empty()),
                "selected inverse requires exact slash-separated Git paths"
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct PreparedInverse {
    identity: WorkspaceIdentity,
    before: GitOid,
    branch: String,
    binding: Option<exec_ctx::WorkspaceBinding>,
    inverse: PathInverse,
    patch: Vec<u8>,
    creation: super::super::PreparedOwnedTask,
}

impl PreparedInverse {
    pub(crate) fn prepare(
        root: &Path,
        commit: &str,
        mut paths: Vec<String>,
        mainline: Option<u32>,
    ) -> Result<Self> {
        require_parent()?;
        let identity = inspect_identity(root, None)?;
        require_unmodified_history(&identity, None)?;
        let root = &identity.canonical_root;
        let commit = resolve(root, commit, None)?;
        let commit_parents = parents(root, &commit, None)?;
        ensure!(
            !commit_parents.is_empty(),
            "selected inverse of a root commit is not supported"
        );
        let parent = if commit_parents.len() == 1 {
            ensure!(
                mainline.is_none(),
                "mainline is only accepted for a merge commit"
            );
            commit_parents[0].clone()
        } else {
            let n = mainline.context("selected merge inverse requires an explicit mainline; future merges may be affected")?;
            ensure!(
                n > 0 && n as usize <= commit_parents.len(),
                "invalid merge mainline"
            );
            commit_parents[n as usize - 1].clone()
        };
        paths.sort();
        let inverse = PathInverse {
            commit,
            parent,
            mainline,
            paths,
        };
        inverse.validate()?;
        validate_scope(root, &inverse)?;
        let patch = reverse_patch(root, &inverse)?;
        ensure!(
            !patch.is_empty() && patch.len() <= 4 * 1024 * 1024,
            "selected inverse patch is empty or exceeds 4 MiB"
        );
        let before = head(root, None)?;
        let branch = source_branch(root, None)?
            .context("selected inverse requires a local destination branch")?;
        let context = exec_ctx::current().unwrap_or_default();
        let creation = super::super::PreparedOwnedTask::prepare_from(
            root,
            &context.resource_scope(),
            &before,
        )?;
        let prepared = Self {
            identity,
            before,
            branch,
            inverse,
            patch,
            creation,
            binding: context.workspace_binding().cloned(),
        };
        prepared.revalidate()?;
        Ok(prepared)
    }

    fn revalidate(&self) -> Result<()> {
        require_parent()?;
        exec_ctx::current()
            .unwrap_or_default()
            .require_current_binding(self.binding.as_ref())?;
        let root = &self.identity.canonical_root;
        ensure!(
            inspect_identity(root, None)? == self.identity
                && head(root, None)? == self.before
                && source_branch(root, None)?.as_ref() == Some(&self.branch),
            "selected inverse destination moved; prepare again"
        );
        require_unmodified_history(&self.identity, None)?;
        require_idle_operation(
            self.identity
                .canonical_git_dir
                .as_ref()
                .context("missing Git directory")?,
        )?;
        require_no_pending(&self.identity)?;
        crate::features::coop::require_no_other_workspace_owner(&self.identity)?;
        ensure!(
            reverse_patch(root, &self.inverse)? == self.patch,
            "selected inverse delta changed; prepare again"
        );
        Ok(())
    }

    pub(crate) fn preview(&self) -> Result<String> {
        self.revalidate()?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "action":"revert_paths", "destination":self.identity.canonical_root,
            "branch":self.branch, "base":self.before, "inverse":self.inverse,
            "delta_sha256":FileFingerprint::for_bytes(&self.patch).sha256,
            "owned_worktree":self.creation.preview()?,
            "effects":"Creates a registered owned worktree from the frozen destination HEAD. Git applies the reverse binary delta with --3way --index and commits exactly its staged tree with configured hooks/signing. Rename endpoints must both be selected. Conflicts and failed/unknown effects are retained, never reset or replayed.",
            "integration":"Destination files/index/HEAD are untouched. Returned handoff requires separate integrate-task approval and destination verification. No automatic cleanup or verification."
        }))?)
    }

    pub(crate) fn execute(self) -> Result<super::super::owned::TaskHandoff> {
        self.revalidate()?;
        let task = Arc::new(self.creation.execute()?);
        let root = task.root().to_path_buf();
        task.finish_inverse(&self.inverse, self.patch).with_context(|| format!(
            "selected inverse in {:?} did not complete; owned worktree/registration/index/conflicts are retained; inspect manually, no automatic retry or cleanup", root))
    }
}

fn reverse_patch(root: &Path, inverse: &PathInverse) -> Result<Vec<u8>> {
    let mut args = vec![
        "diff",
        "--binary",
        "--full-index",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        inverse.commit.as_str(),
        inverse.parent.as_str(),
        "--",
    ];
    args.extend(inverse.paths.iter().map(String::as_str));
    query(root, &args, None)
}

/// Inspect the whole change with rename detection BEFORE selecting paths; path-limiting first can
/// hide the other endpoint and silently turn a rename into an unrelated addition/deletion.
fn validate_scope(root: &Path, inverse: &PathInverse) -> Result<()> {
    let bytes = query(
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-z",
            "--find-renames=50%",
            "-l0",
            "--no-ext-diff",
            "--no-textconv",
            inverse.parent.as_str(),
            inverse.commit.as_str(),
            "--",
        ],
        None,
    )?;
    ensure!(
        bytes.is_empty() || bytes.ends_with(&[0]),
        "incomplete selected inverse path inspection"
    );
    let mut records = bytes.split(|b| *b == 0).filter(|r| !r.is_empty());
    let mut found = std::collections::BTreeSet::new();
    while let Some(status) = records.next() {
        let status = std::str::from_utf8(status)?;
        ensure!(
            matches!(status, "M" | "A" | "D" | "T") || status.starts_with('R'),
            "unsupported selected inverse change kind"
        );
        let path = std::str::from_utf8(records.next().context("missing changed path")?)?;
        let selected = inverse.paths.iter().any(|p| p == path);
        if status.starts_with('R') {
            let other = std::str::from_utf8(records.next().context("missing rename endpoint")?)?;
            let other_selected = inverse.paths.iter().any(|p| p == other);
            ensure!(
                selected == other_selected,
                "selected inverse of a rename requires both exact endpoints"
            );
            if selected {
                found.insert(other.to_owned());
            }
        }
        if selected {
            found.insert(path.to_owned());
        }
    }
    ensure!(
        found.into_iter().collect::<Vec<_>>() == inverse.paths,
        "select exact changed files, not directories, globs or unchanged paths"
    );
    // Keep executable files supported, but never apply symlink/gitlink entries or .git aliases.
    for revision in [&inverse.commit, &inverse.parent] {
        let mut args = vec!["ls-tree", "-r", "-z", revision.as_str(), "--"];
        args.extend(inverse.paths.iter().map(String::as_str));
        let entries = query(root, &args, None)?;
        for entry in entries.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            ensure!(
                entry.starts_with(b"100644 blob ") || entry.starts_with(b"100755 blob "),
                "symlink/submodule selected inverses require manual Git handling"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "git_ops_inverse_tests.rs"]
mod tests;
