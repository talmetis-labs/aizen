//! Actual Git fixtures; every ref/index/worktree mutation is confined to disposable repositories.
use super::*;
use crate::core::{
    convo::ConversationId,
    exec_ctx::ExecutionContext,
    git_ops::{Change, Prepared, PreparedOutcome},
};

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    git: PathBuf,
}
impl Fixture {
    fn git_at(&self, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let out = std::process::Command::new(&self.git)
            .current_dir(root)
            .args(args)
            .output()?;
        ensure!(
            out.status.success(),
            "fixture Git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(out.stdout)
    }
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.git_at(&self.root, args)
    }
    fn commit(&self) -> Result<String> {
        self.git(&["add", "--all"])?;
        self.git(&["commit", "-qm", "fixture change"])?;
        Ok(String::from_utf8(self.git(&["rev-parse", "HEAD"])?)?
            .trim()
            .into())
    }
    fn prepare(&self, commit: &str, paths: &[&str]) -> Result<PreparedInverse> {
        PreparedInverse::prepare(
            &self.root,
            commit,
            paths.iter().map(|p| (*p).into()).collect(),
            None,
        )
    }
    fn manifest(&self, root: &Path) -> Result<crate::features::coop::SessionManifest> {
        let identity = inspect_identity(root, None)?;
        let directory = self
            .home
            .join("coop")
            .join(&identity.repo_id)
            .join("sessions");
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let manifest: crate::features::coop::SessionManifest =
                    serde_json::from_slice(&fs::read(path)?)?;
                if Path::new(&manifest.root) == root {
                    return Ok(manifest);
                }
            }
        }
        anyhow::bail!("missing inverse registration")
    }
    fn child_root(&self) -> Result<PathBuf> {
        let entries = crate::core::git_ops::worktree_inventory(&self.root)?;
        ensure!(
            entries.len() == 2,
            "expected exactly one retained inverse worktree"
        );
        Ok(entries
            .into_iter()
            .map(|entry| PathBuf::from(entry.path))
            .find(|p| fs::canonicalize(p).is_ok_and(|p| p != self.root))
            .context("missing child")?
            .canonicalize()?)
    }
    fn hook(&self, name: &str, body: &str) -> Result<()> {
        let path = self.root.join(".git/hooks").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}
fn fixture(test: impl FnOnce(&Fixture) -> Result<()>) -> Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct Restore(crate::sandbox::SandboxMode);
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::sandbox::set_mode(self.0);
        }
    }
    let _restore = Restore(crate::sandbox::mode());
    crate::sandbox::set_mode(crate::sandbox::SandboxMode::Guarded);
    let temp = crate::core::file_placement::tests::TestDir::new();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir(&root)?;
    fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let f = Fixture {
        root,
        home,
        git: crate::core::gitx::git_exe().context("Git required for inverse fixtures")?,
    };
    f.git(&["init", "-q"])?;
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
    ] {
        f.git(&["config", key, value])?;
    }
    let hooks = f
        .root
        .join(".git/hooks")
        .to_string_lossy()
        .replace('\\', "/");
    #[cfg(windows)]
    let hooks = hooks.strip_prefix("//?/").unwrap_or(&hooks).to_owned();
    f.git(&["config", "core.hooksPath", &hooks])?;
    fs::write(
        f.root.join("selected.txt"),
        b"old\n1\n2\n3\n4\n5\n6\n7\n8\nend\n",
    )?;
    fs::write(f.root.join("other.txt"), b"base\n")?;
    f.commit()?;
    crate::core::workspace_txn::with_test_lock_home(&f.home, || {
        let placement =
            crate::core::file_placement::PlacementContext::new(f.root.clone(), f.home.clone())?;
        let context =
            ExecutionContext::new(ConversationId::new("inverse-parent")).with_placement(placement);
        exec_ctx::with_current(context, || test(&f))
    })
}

#[test]
fn selected_inverse_preserves_dirty_parent_and_later_work_then_integrates_explicitly() -> Result<()>
{
    fixture(|f| {
        fs::create_dir(f.root.join(".aizen"))?;
        fs::write(
            f.root.join(".aizen/verify.json"),
            r#"{"commands":["exit 0"],"timeout_secs":30}"#,
        )?;
        fs::write(f.root.join("Cargo.toml"), "[package]\n")?;
        f.commit()?;
        crate::agent::mcp::trust_project()?;
        fs::write(
            f.root.join("selected.txt"),
            b"new\n1\n2\n3\n4\n5\n6\n7\n8\nend\n",
        )?;
        fs::write(f.root.join("other.txt"), b"selected other\n")?;
        let selected = f.commit()?;
        fs::write(
            f.root.join("selected.txt"),
            b"new\n1\n2\n3\n4\n5\n6\n7\n8\nlater\n",
        )?;
        let base = f.commit()?;
        let committed = fs::read(f.root.join("selected.txt"))?;
        fs::write(f.root.join("selected.txt"), b"staged user\n")?;
        f.git(&["add", "--", "selected.txt"])?;
        fs::write(f.root.join("selected.txt"), b"unstaged user\n")?;
        fs::write(f.root.join("private.tmp"), b"untracked user\n")?;
        let index = fs::read(f.root.join(".git/index"))?;
        let inventory = f.git(&["worktree", "list", "--porcelain", "-z"])?;
        let prepared = f.prepare(&selected, &["selected.txt"])?;
        assert!(prepared.preview()?.contains(&selected));
        assert_eq!(
            f.git(&["worktree", "list", "--porcelain", "-z"])?,
            inventory
        );
        let handoff = prepared.execute()?;
        assert_eq!(handoff.base.as_str(), base);
        assert_eq!(handoff.paths, ["selected.txt"]);
        assert_eq!(
            fs::read(handoff.root.join("selected.txt"))?,
            b"old\n1\n2\n3\n4\n5\n6\n7\n8\nlater\n"
        );
        assert_eq!(
            fs::read(handoff.root.join("other.txt"))?,
            b"selected other\n"
        );
        assert_eq!(fs::read(f.root.join("selected.txt"))?, b"unstaged user\n");
        assert_eq!(fs::read(f.root.join("private.tmp"))?, b"untracked user\n");
        assert_eq!(fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(head(&f.root, None)?.as_str(), base);
        let integration = || Change::IntegrateTask {
            receipt: handoff.registration.registration_id.clone(),
        };
        assert!(Prepared::prepare(&f.root, integration()).is_err());
        // Restore only fixture-created pending bytes. The product never restores/stashes the parent.
        f.git(&["restore", "--staged", "--", "selected.txt"])?;
        fs::write(f.root.join("selected.txt"), committed)?;
        fs::remove_file(f.root.join("private.tmp"))?;
        crate::core::git_ops::validate_task_handoff(&handoff)?;
        let PreparedOutcome::Change(receipt) =
            Prepared::prepare(&f.root, integration())?.execute()?
        else {
            anyhow::bail!("not an integration");
        };
        assert_eq!(receipt.after.as_ref(), Some(&handoff.tip));
        assert!(!receipt.verified);
        assert_eq!(receipt.handoff.as_ref(), Some(&handoff));
        let verification =
            crate::core::git_ops::PreparedVerification::prepare(&f.root, &receipt.id)?.execute()?;
        assert!(verification.verified);
        assert_eq!(
            crate::core::git_ops::operation_receipts(&f.root)?[0]
                .handoff
                .as_ref(),
            Some(&handoff)
        );
        assert_eq!(fs::read(f.root.join("other.txt"))?, b"selected other\n");
        // Redo is an explicit inverse of the REAL inverse commit, never a stale full-tree replay.
        PreparedChange::prepare(
            &f.root,
            Change::Revert {
                commits: vec![handoff.tip.as_str().into()],
                mainline: None,
            },
        )?
        .execute()?;
        assert_eq!(
            fs::read(f.root.join("selected.txt"))?,
            b"new\n1\n2\n3\n4\n5\n6\n7\n8\nlater\n"
        );
        Ok(())
    })
}

#[test]
fn selected_inverse_handles_binary_add_delete_and_literal_names() -> Result<()> {
    fixture(|f| {
        fs::write(f.root.join("deleted.bin"), b"old\0binary\xff")?;
        fs::write(f.root.join("file with space.bin"), b"before\0\xff")?;
        let parent = f.commit()?;
        fs::remove_file(f.root.join("deleted.bin"))?;
        fs::write(f.root.join("added.bin"), b"added\0binary\xfe")?;
        fs::write(f.root.join("file with space.bin"), b"after\0\xfe")?;
        let selected = f.commit()?;
        let handoff = f
            .prepare(
                &selected,
                &["deleted.bin", "added.bin", "file with space.bin"],
            )?
            .execute()?;
        assert_eq!(
            fs::read(handoff.root.join("deleted.bin"))?,
            b"old\0binary\xff"
        );
        assert_eq!(
            fs::read(handoff.root.join("file with space.bin"))?,
            b"before\0\xff"
        );
        assert!(!handoff.root.join("added.bin").exists());
        assert_eq!(handoff.inverse.as_ref().unwrap().parent.as_str(), parent);
        assert_eq!(head(&f.root, None)?.as_str(), selected);
        crate::core::git_ops::validate_task_handoff(&handoff)?;
        Ok(())
    })
}

#[test]
fn selected_inverse_requires_both_rename_endpoints_and_retains_other_paths() -> Result<()> {
    fixture(|f| {
        f.git(&["mv", "selected.txt", "renamed.txt"])?;
        fs::write(f.root.join("other.txt"), b"keep this selected change\n")?;
        let selected = f.commit()?;
        assert!(f
            .prepare(&selected, &["renamed.txt"])
            .unwrap_err()
            .to_string()
            .contains("both exact endpoints"));
        assert!(f.prepare(&selected, &["selected.txt"]).is_err());
        let handoff = f
            .prepare(&selected, &["renamed.txt", "selected.txt"])?
            .execute()?;
        assert!(handoff.root.join("selected.txt").exists());
        assert!(!handoff.root.join("renamed.txt").exists());
        assert_eq!(
            fs::read(handoff.root.join("other.txt"))?,
            b"keep this selected change\n"
        );
        assert_eq!(handoff.paths, ["renamed.txt", "selected.txt"]);
        Ok(())
    })
}

#[test]
fn selected_inverse_conflict_retains_owned_index_and_blocks_adoption_or_removal() -> Result<()> {
    fixture(|f| {
        fs::write(f.root.join("selected.txt"), b"selected\n")?;
        let selected = f.commit()?;
        fs::write(f.root.join("selected.txt"), b"later overlap\n")?;
        let before = f.commit()?;
        let index = fs::read(f.root.join(".git/index"))?;
        assert!(f.prepare(&selected, &["selected.txt"])?.execute().is_err());
        let child = f.child_root()?;
        assert!(!f.git_at(&child, &["ls-files", "-u"])?.is_empty());
        let manifest = f.manifest(&child)?;
        assert_eq!(manifest.git_task.unwrap().phase, "applying_inverse");
        let identity = inspect_identity(&child, None)?;
        assert!(crate::features::coop::require_no_other_workspace_owner(&identity).is_err());
        assert!(
            crate::features::coop::FrozenGitTask::select(&identity, &manifest.session_id).is_err()
        );
        assert_eq!(head(&f.root, None)?.as_str(), before);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(fs::read(f.root.join("selected.txt"))?, b"later overlap\n");
        Ok(())
    })
}

#[test]
fn selected_inverse_refuses_unsafe_scope_stale_approval_and_merge_without_mainline() -> Result<()> {
    fixture(|f| {
        fs::write(f.root.join("selected.txt"), b"selected\n")?;
        let selected = f.commit()?;
        for paths in [
            vec![],
            vec!["../selected.txt"],
            vec!["."],
            vec!["*.txt"],
            vec![":(glob)*"],
            vec!["[file]"],
            vec!["selected.txt", "selected.txt"],
            vec![".git/config"],
            vec!["other.txt"],
            vec!["dir//file"],
            vec!["selected.txt/"],
        ] {
            assert!(f.prepare(&selected, &paths).is_err(), "accepted {paths:?}");
        }
        let original_context = exec_ctx::current().context("missing fixture context")?;
        let stale = f.prepare(&selected, &["selected.txt"])?;
        let sibling = f.home.join("other-workspace");
        fs::create_dir(&sibling)?;
        let later_generation = original_context
            .with_placement(crate::core::file_placement::PlacementContext::new(
                sibling,
                f.home.clone(),
            )?)
            .with_placement(crate::core::file_placement::PlacementContext::new(
                f.root.clone(),
                f.home.clone(),
            )?);
        exec_ctx::with_current(later_generation, || {
            assert!(stale
                .execute()
                .unwrap_err()
                .to_string()
                .contains("binding is stale"));
        });
        let prepared = f.prepare(&selected, &["selected.txt"])?;
        fs::write(f.root.join("later.txt"), b"new base\n")?;
        f.commit()?;
        assert!(prepared.execute().is_err());
        assert_eq!(crate::core::git_ops::worktree_inventory(&f.root)?.len(), 1);
        let branch = String::from_utf8(f.git(&["symbolic-ref", "--short", "HEAD"])?)?
            .trim()
            .to_owned();
        f.git(&["checkout", "-qb", "side"])?;
        fs::write(f.root.join("side.txt"), b"side\n")?;
        f.commit()?;
        f.git(&["checkout", &branch])?;
        f.git(&["merge", "--no-ff", "--no-edit", "side"])?;
        let merge = head(&f.root, None)?;
        assert!(f.prepare(merge.as_str(), &["side.txt"]).is_err());
        let prepared =
            PreparedInverse::prepare(&f.root, merge.as_str(), vec!["side.txt".into()], Some(1))?;
        let handoff = prepared.execute()?;
        assert!(!handoff.root.join("side.txt").exists());
        Ok(())
    })
}

#[test]
fn selected_inverse_preserves_hook_failure_and_rejects_hook_added_tree() -> Result<()> {
    for (body, expected_phase) in [
        ("exit 7", "committing_inverse"),
        (
            "printf 'hook effect\\n' > hook.txt\ngit add -- hook.txt",
            "committing_inverse",
        ),
    ] {
        fixture(|f| {
            fs::write(f.root.join("selected.txt"), b"selected\n")?;
            let selected = f.commit()?;
            f.hook("pre-commit", body)?;
            assert!(f.prepare(&selected, &["selected.txt"])?.execute().is_err());
            let child = f.child_root()?;
            let manifest = f.manifest(&child)?;
            assert_eq!(manifest.git_task.unwrap().phase, expected_phase);
            let identity = inspect_identity(&child, None)?;
            assert!(crate::features::coop::require_no_other_workspace_owner(&identity).is_err());
            assert_eq!(head(&f.root, None)?.as_str(), selected);
            if body != "exit 7" {
                assert!(child.join("hook.txt").exists());
            }
            Ok(())
        })?;
    }
    Ok(())
}
