//! Local branch retirement: explicit retained ref, exact OIDs, and no checkout removal.

use super::*;

pub(super) fn local_ref(root: &Path, branch: &str, owner: Option<&Owner>) -> Result<String> {
    ensure!(
        !branch.is_empty()
            && branch.len() <= 200
            && !branch.starts_with('-')
            && !branch.contains('@')
            && !branch.chars().any(char::is_control),
        "supply a literal local branch name"
    );
    let reference = format!("refs/heads/{branch}");
    query(root, &["check-ref-format", &reference], owner)?;
    Ok(reference)
}

pub(super) fn direct_tip(root: &Path, reference: &str, owner: Option<&Owner>) -> Result<GitOid> {
    ensure!(
        reference.starts_with("refs/heads/"),
        "retention requires an explicit full local branch ref"
    );
    query(root, &["check-ref-format", reference], owner)?;
    let records = query(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            reference,
        ],
        owner,
    )?;
    let text = std::str::from_utf8(&records)?;
    let mut matching = text.lines().filter_map(|line| {
        let fields: Vec<_> = line.split('\0').collect();
        (fields.first().copied() == Some(reference)).then_some(fields)
    });
    let fields = matching
        .next()
        .context("selected local ref does not exist")?;
    ensure!(
        fields.len() == 3 && fields[2].is_empty() && matching.next().is_none(),
        "symbolic or ambiguous branch ref is not supported"
    );
    let oid = GitOid::parse(fields[1])?;
    ensure!(
        resolve(root, reference, owner)? == oid,
        "branch tip changed during inspection"
    );
    Ok(oid)
}

pub(super) fn targets(
    root: &Path,
    branch: &str,
    retained: &str,
    owner: Option<&Owner>,
) -> Result<Vec<GitOid>> {
    let reference = local_ref(root, branch, owner)?;
    ensure!(
        reference != retained,
        "deleted and retained refs must differ"
    );
    Ok(vec![
        direct_tip(root, &reference, owner)?,
        direct_tip(root, retained, owner)?,
    ])
}

pub(super) fn revalidate(plan: &PreparedChange, owner: Option<&Owner>) -> Result<()> {
    revalidate_refs(plan, owner)?;
    crate::features::coop::require_no_other_repository_owner(&plan.identity)?;
    // Pending operations elsewhere must not lose their branch recovery context.
    require_repo_receipts_settled(&plan.identity)?;
    verification::require_repository_settled(&plan.identity)
}

pub(super) fn revalidate_refs(plan: &PreparedChange, owner: Option<&Owner>) -> Result<()> {
    let Change::DeleteBranch {
        branch,
        retained_ref,
    } = &plan.change
    else {
        anyhow::bail!("branch retirement intent required");
    };
    let root = &plan.identity.canonical_root;
    let reference = local_ref(root, branch, owner)?;
    ensure!(reference != plan.branch, "cannot delete the current branch");
    ensure!(
        targets(root, branch, retained_ref, owner)? == plan.targets,
        "deleted or retained ref moved; prepare again"
    );
    let range = format!("{}..{}", plan.targets[1].as_str(), plan.targets[0].as_str());
    ensure!(
        query(root, &["rev-list", "--max-count=1", &range], owner)?.is_empty(),
        "branch has commits not reachable from the explicit retained ref; nothing is deleted"
    );
    for entry in parse_worktrees(&query(
        root,
        &["worktree", "list", "--porcelain", "-z"],
        owner,
    )?)? {
        ensure!(
            entry.branch.as_ref() != Some(&reference),
            "branch is checked out in a registered worktree; remove/focus it separately"
        );
    }
    Ok(())
}

pub(super) fn postcondition(plan: &PreparedChange, owner: &Owner) -> Result<()> {
    let Change::DeleteBranch {
        branch,
        retained_ref,
    } = &plan.change
    else {
        anyhow::bail!("branch retirement intent required");
    };
    let root = &plan.identity.canonical_root;
    let reference = local_ref(root, branch, Some(owner))?;
    let output = query(
        root,
        &["for-each-ref", "--format=%(refname)", &reference],
        Some(owner),
    )?;
    ensure!(
        !std::str::from_utf8(&output)?
            .lines()
            .any(|line| line == reference),
        "branch deletion did not remove the selected ref"
    );
    ensure!(
        direct_tip(root, retained_ref, Some(owner))? == plan.targets[1],
        "retained ref moved during branch deletion"
    );
    Ok(())
}

fn require_repo_receipts_settled(identity: &WorkspaceIdentity) -> Result<()> {
    let directory = receipt_directory(identity)
        .parent()
        .context("missing receipt repository directory")?
        .to_path_buf();
    let (_, witness) = crate::core::speculation::namespace::plan_metadata_directory(&directory)?;
    witness.verify()?;
    let entries = match fs::read_dir(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspecting repository operation receipts"),
        Ok(entries) => entries,
    };
    let mut count = 0usize;
    let mut total = 0usize;
    for (index, entry) in entries.enumerate() {
        ensure!(index < 4096, "repository receipt directory limit reached");
        let path = entry?.path();
        let (_, boundary) = crate::core::speculation::namespace::plan_metadata_directory(&path)?;
        boundary.verify()?;
        for record in fs::read_dir(&path)? {
            count += 1;
            ensure!(count <= 4096, "repository receipt scan limit reached");
            let path = record?.path();
            let (receipt, length) = receipts::read_repository(identity, &path)?;
            total += length;
            ensure!(
                total <= 16 * 1024 * 1024,
                "repository receipt byte limit reached"
            );
            receipts::require_completed(&receipt)?;
        }
        boundary.verify()?;
    }
    witness.verify()
}
