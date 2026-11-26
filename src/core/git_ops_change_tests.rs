use super::*;
use crate::core::git_ops::{Change, PreparedChange};

struct Fixture {
    root: PathBuf,
    git: PathBuf,
}
impl Fixture {
    fn git(&self, args: &[&str]) -> Result<String> {
        let out = std::process::Command::new(&self.git)
            .current_dir(&self.root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()?;
        ensure!(
            out.status.success(),
            "fixture: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8(out.stdout)?.trim().into())
    }
    fn commit_file(&self, path: &str, bytes: &[u8]) -> Result<String> {
        fs::write(self.root.join(path), bytes)?;
        self.git(&["add", "--", path])?;
        self.git(&["commit", "-qm", "fixture change"])?;
        self.git(&["rev-parse", "HEAD"])
    }
    fn revert(&self, oid: &str) -> Result<ChangeReceipt> {
        PreparedChange::prepare(
            &self.root,
            Change::Revert {
                commits: vec![oid.into()],
                mainline: None,
            },
        )?
        .execute()
    }
}
fn fixture(test: impl FnOnce(&Fixture) -> Result<()>) -> Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct Mode(crate::sandbox::SandboxMode);
    impl Drop for Mode {
        fn drop(&mut self) {
            crate::sandbox::set_mode(self.0);
        }
    }
    let _restore = Mode(crate::sandbox::mode());
    crate::sandbox::set_mode(crate::sandbox::SandboxMode::Guarded);
    let temp = crate::core::file_placement::tests::TestDir::new();
    let root = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir(&root)?;
    fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let git = crate::core::gitx::git_exe().context("Git required for mutation fixtures")?;
    let f = Fixture { root, git };
    f.git(&["init", "-q"])?;
    // Production must preserve configuration; configure only this disposable repository.
    f.git(&["config", "user.name", "Fixture"])?;
    f.git(&["config", "user.email", "fixture@example.invalid"])?;
    f.git(&["config", "commit.gpgsign", "false"])?;
    f.git(&["config", "core.hooksPath", ".git/hooks"])?;
    f.commit_file("a.txt", b"base\n")?;
    crate::core::workspace_txn::with_test_lock_home(&home, || test(&f))
}

#[test]
fn revert_and_explicit_redo_preserve_later_independent_commit() -> Result<()> {
    fixture(|f| {
        let selected = f.commit_file("a.txt", b"changed\n")?;
        let later = f.commit_file("later.txt", b"keep\n")?;
        let undo = f.revert(&selected)?;
        assert_eq!(undo.state, "settled_unverified");
        assert!(!undo.verified);
        assert_eq!(undo.resulting_commits.len(), 1);
        assert_eq!(
            fs::read_to_string(f.root.join("a.txt"))?.replace("\r\n", "\n"),
            "base\n"
        );
        assert_eq!(
            fs::read_to_string(f.root.join("later.txt"))?.replace("\r\n", "\n"),
            "keep\n"
        );
        assert!(f.git(&["rev-list", "HEAD"])?.contains(&later));
        let redo = PreparedChange::prepare(
            &f.root,
            Change::Redo {
                receipt: undo.id.clone(),
            },
        )?
        .execute()?;
        assert_eq!(redo.resulting_commits.len(), 1);
        assert!(matches!(&redo.change, Change::Redo { receipt } if receipt == &undo.id));
        assert!(PreparedChange::prepare(&f.root, Change::Redo { receipt: undo.id }).is_err());
        assert_eq!(
            fs::read_to_string(f.root.join("a.txt"))?.replace("\r\n", "\n"),
            "changed\n"
        );
        Ok(())
    })
}

#[test]
fn recorded_redo_reverses_the_whole_inverse_list_and_preserves_new_work() -> Result<()> {
    fixture(|f| {
        let first = f.commit_file("a.txt", b"first\n")?;
        let second = f.commit_file("a.txt", b"second\n")?;
        let undo = PreparedChange::prepare(
            &f.root,
            Change::Revert {
                commits: vec![second, first],
                mainline: None,
            },
        )?
        .execute()?;
        assert_eq!(undo.resulting_commits.len(), 2);
        let later = f.commit_file("later.txt", b"keep after undo\n")?;
        let plan = PreparedChange::prepare(
            &f.root,
            Change::Redo {
                receipt: undo.id.clone(),
            },
        )?;
        assert_eq!(
            plan.targets,
            undo.resulting_commits
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
        );
        let redo = plan.execute()?;
        assert_eq!(redo.resulting_commits.len(), 2);
        assert_eq!(
            fs::read_to_string(f.root.join("a.txt"))?.replace("\r\n", "\n"),
            "second\n"
        );
        assert!(f.git(&["rev-list", "HEAD"])?.contains(&later));
        assert_eq!(
            fs::read_to_string(f.root.join("later.txt"))?.replace("\r\n", "\n"),
            "keep after undo\n"
        );
        Ok(())
    })
}

#[test]
fn recorded_redo_refuses_unrelated_foreign_incomplete_and_changed_receipts() -> Result<()> {
    fixture(|f| {
        let selected = f.commit_file("a.txt", b"changed\n")?;
        let undo = f.revert(&selected)?;
        let change = || Change::Redo {
            receipt: undo.id.clone(),
        };
        for id in [
            "../escape",
            "#42",
            "42",
            "missing-receipt",
            selected.as_str(),
        ] {
            assert!(PreparedChange::prepare(&f.root, Change::Redo { receipt: id.into() }).is_err());
        }
        let branch = f.git(&["branch", "--show-current"])?;
        f.git(&["switch", "-qc", "other"])?;
        assert!(PreparedChange::prepare(&f.root, change()).is_err());
        f.git(&["switch", "-q", &branch])?;
        let identity = inspect_identity(&f.root, None)?;
        let path = receipt_directory(&identity).join(format!("{}.json", undo.id));
        let original = fs::read(&path)?;
        for field in ["incomplete", "foreign", "missing-map", "non-undo"] {
            let mut value = serde_json::to_value(&undo)?;
            match field {
                "incomplete" => value["state"] = serde_json::json!("failed_or_conflicted"),
                "foreign" => value["root"] = serde_json::json!(f.root.join("other")),
                "missing-map" => value["resulting_commits"] = serde_json::json!([]),
                "non-undo" => {
                    value["change"] = serde_json::json!({"action":"integrate", "commit": selected})
                }
                _ => unreachable!(),
            }
            fs::write(&path, serde_json::to_vec(&value)?)?;
            assert!(
                PreparedChange::prepare(&f.root, change()).is_err(),
                "accepted {field}"
            );
        }
        fs::write(&path, &original)?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        let mut changed = original.clone();
        changed.push(b'\n'); // Equivalent JSON still invalidates the exact approved selection.
        fs::write(&path, &changed)?;
        let before = f.git(&["rev-parse", "HEAD"])?;
        assert!(plan.execute().is_err());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        fs::write(&path, &original)?;
        assert_eq!(operation_receipts(&f.root)?.len(), 1);
        let held = path.with_extension("held");
        fs::rename(&path, &held)?;
        assert!(
            operation_receipts(&f.root).is_err(),
            "filename and receipt ID must match"
        );
        Ok(())
    })
}

#[test]
fn branch_retirement_refuses_grafted_history_and_late_overlay() -> Result<()> {
    fixture(|f| {
        let base = f.git(&["rev-parse", "HEAD"])?;
        f.git(&["switch", "-qc", "topic"])?;
        let topic = f.commit_file("topic.txt", b"unique topic\n")?;
        f.git(&["switch", "-qc", "retained", &base])?;
        let retained = f.commit_file("retained.txt", b"independent retained\n")?;
        let index = fs::read(f.root.join(".git/index"))?;
        let change = || Change::DeleteBranch {
            branch: "topic".into(),
            retained_ref: "refs/heads/retained".into(),
        };
        let range = format!("{retained}..{topic}");
        assert_eq!(f.git(&["rev-list", "--max-count=1", &range])?, topic);
        let graft = f.root.join(".git/info/grafts");
        fs::write(&graft, format!("{retained} {topic}\n"))?;
        assert!(f.git(&["rev-list", "--max-count=1", &range])?.is_empty());
        // Even --no-replace-objects does not disable this deprecated ancestry overlay.
        assert!(f
            .git(&["--no-replace-objects", "rev-list", "--max-count=1", &range])?
            .is_empty());
        let error = PreparedChange::prepare(&f.root, change()).unwrap_err();
        assert!(error.to_string().contains("unmodified commit ancestry"));
        assert_eq!(f.git(&["rev-parse", "topic"])?, topic);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index);
        fs::remove_file(&graft)?;
        f.git(&["branch", "--force", "topic", &base])?;
        let prepared = PreparedChange::prepare(&f.root, change())?;
        fs::write(&graft, format!("{retained} {base}\n"))?;
        assert!(
            prepared.execute().is_err(),
            "overlay introduced after approval must refuse"
        );
        assert_eq!(f.git(&["rev-parse", "topic"])?, base);
        assert!(operation_receipts(&f.root)?.is_empty());
        assert_eq!(
            fs::read_to_string(f.root.join("retained.txt"))?.replace("\r\n", "\n"),
            "independent retained\n"
        );
        Ok(())
    })
}

#[test]
fn branch_retirement_refuses_replace_refs_and_linked_shallow_boundaries() -> Result<()> {
    fixture(|f| {
        let base = f.git(&["rev-parse", "HEAD"])?;
        f.git(&["switch", "-qc", "topic"])?;
        let topic = f.commit_file("topic.txt", b"unique topic\n")?;
        f.git(&["switch", "-qc", "retained", &base])?;
        let retained = f.commit_file("retained.txt", b"independent retained\n")?;
        f.git(&["worktree", "add", "-q", "-b", "linked", "../linked"])?;
        let linked = f.root.parent().unwrap().join("linked");
        let change = || Change::DeleteBranch {
            branch: "topic".into(),
            retained_ref: "refs/heads/retained".into(),
        };
        f.git(&["replace", "--graft", &retained, &topic])?;
        let range = format!("{retained}..{topic}");
        assert!(f.git(&["rev-list", "--max-count=1", &range])?.is_empty());
        assert_eq!(
            f.git(&["--no-replace-objects", "rev-list", "--max-count=1", &range])?,
            topic
        );
        assert!(PreparedChange::prepare(&linked, change())
            .unwrap_err()
            .to_string()
            .contains("replace refs"));
        assert_eq!(f.git(&["rev-parse", "topic"])?, topic);
        f.git(&["replace", "--delete", &retained])?;
        f.git(&["branch", "--force", "topic", &base])?;
        let prepared = PreparedChange::prepare(&linked, change())?;
        fs::write(f.root.join(".git/shallow"), format!("{base}\n"))?;
        assert!(prepared.execute().is_err());
        assert!(PreparedChange::prepare(&linked, change())
            .unwrap_err()
            .to_string()
            .contains("shallow boundaries"));
        assert_eq!(f.git(&["rev-parse", "topic"])?, base);
        assert!(operation_receipts(&linked)?.is_empty());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, retained);
        Ok(())
    })
}

#[test]
fn branch_retirement_requires_explicit_reachability_and_preserves_checkout() -> Result<()> {
    fixture(|f| {
        let branch = f.git(&["branch", "--show-current"])?;
        let retained = format!("refs/heads/{branch}");
        f.git(&["branch", "old-topic"])?;
        let before = f.commit_file("later.txt", b"keep\n")?;
        let index = fs::read(f.root.join(".git/index"))?;
        let change = || Change::DeleteBranch {
            branch: "old-topic".into(),
            retained_ref: retained.clone(),
        };
        let plan = PreparedChange::prepare(&f.root, change())?;
        assert!(plan
            .preview()?
            .contains("no files or worktrees are removed"));
        assert_eq!(
            f.git(&["show-ref", "--verify", "refs/heads/old-topic"])?
                .split_whitespace()
                .count(),
            2
        );
        let receipt = plan.execute()?;
        assert_eq!(receipt.state, "settled_unverified");
        assert!(receipt.resulting_commits.is_empty());
        assert!(f
            .git(&["show-ref", "--verify", "refs/heads/old-topic"])
            .is_err());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        assert_eq!(f.git(&["rev-parse", &retained])?, before);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(
            fs::read_to_string(f.root.join("later.txt"))?.replace("\r\n", "\n"),
            "keep\n"
        );
        assert!(PreparedChange::prepare(
            &f.root,
            Change::DeleteBranch {
                branch,
                retained_ref: "refs/heads/retained".into(),
            }
        )
        .is_err());
        Ok(())
    })
}

#[test]
fn branch_retirement_refuses_unique_checked_out_and_moved_refs() -> Result<()> {
    fixture(|f| {
        f.git(&["branch", "retained"])?;
        f.commit_file("unique.txt", b"unique\n")?;
        f.git(&["branch", "topic"])?;
        let change = || Change::DeleteBranch {
            branch: "topic".into(),
            retained_ref: "refs/heads/retained".into(),
        };
        assert!(
            PreparedChange::prepare(&f.root, change()).is_err(),
            "HEAD ancestry is not the explicit retained-ref proof"
        );
        f.git(&["branch", "-f", "retained", "HEAD"])?;
        // Git for Windows does not accept canonical verbatim paths as worktree arguments.
        f.git(&["worktree", "add", "-q", "../linked", "topic"])?;
        assert!(
            PreparedChange::prepare(&f.root, change()).is_err(),
            "checked-out branch must be retained"
        );
        f.git(&["worktree", "remove", "../linked"])?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        f.git(&["branch", "-f", "retained", "HEAD~1"])?;
        assert!(plan.execute().is_err());
        f.git(&["show-ref", "--verify", "refs/heads/topic"])?;
        f.git(&["branch", "-f", "retained", "HEAD"])?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        f.git(&["branch", "-f", "topic", "HEAD~1"])?;
        assert!(plan.execute().is_err());
        f.git(&["show-ref", "--verify", "refs/heads/topic"])?;
        fs::write(f.root.join("untracked"), b"keep")?;
        assert!(PreparedChange::prepare(&f.root, change()).is_err());
        assert_eq!(fs::read(f.root.join("untracked"))?, b"keep");
        Ok(())
    })
}

#[test]
fn branch_retirement_checks_sibling_receipt_binding_and_outcome() -> Result<()> {
    fixture(|f| {
        let branch = f.git(&["branch", "--show-current"])?;
        f.git(&["branch", "topic"])?;
        f.git(&["worktree", "add", "-q", "-b", "sibling", "../sibling"])?;
        let identity = inspect_identity(&f.root.parent().unwrap().join("sibling"), None)?;
        let before = head(&f.root, None)?;
        let receipt = ChangeReceipt {
            schema: 1,
            id: "sibling-operation".into(),
            root: identity.canonical_root.clone(),
            git_dir: identity.canonical_git_dir.clone().unwrap(),
            common_dir: identity.canonical_common_git_dir.clone().unwrap(),
            before: before.clone(),
            after: None,
            branch_before: "refs/heads/sibling".into(),
            change: Change::Integrate {
                commit: before.as_str().into(),
            },
            targets: vec![before.clone()],
            state: "not_started".into(),
            resulting_commits: Vec::new(),
            verified: false,
            reconciliation: None,
            handoff: None,
            verification: None,
            sequencer: None,
        };
        let directory = receipt_directory(&identity);
        let witness = crate::core::speculation::namespace::create_metadata_directory(&directory)?;
        let path = directory.join("sibling-operation.json");
        persist(&path, &receipt, &witness)?;
        let change = || Change::DeleteBranch {
            branch: "topic".into(),
            retained_ref: format!("refs/heads/{branch}"),
        };
        // A known pre-submission refusal in another worktree is not a pending operation.
        let plan = PreparedChange::prepare(&f.root, change())?;
        for state in [
            "prepared",
            "running",
            "unsettled",
            "failed_or_conflicted",
            "unknown",
        ] {
            let mut value = serde_json::to_value(&receipt)?;
            value["state"] = serde_json::json!(state);
            fs::write(&path, serde_json::to_vec(&value)?)?;
            assert!(
                PreparedChange::prepare(&f.root, change()).is_err(),
                "accepted {state}"
            );
        }
        // Approval made before a sibling started work cannot be consumed after it started.
        assert!(plan.execute().is_err());
        for field in [
            "id",
            "root",
            "git_dir",
            "common_dir",
            "schema",
            "after",
            "settled",
        ] {
            let mut value = serde_json::to_value(&receipt)?;
            match field {
                "id" => value["id"] = serde_json::json!("different-id"),
                "root" | "git_dir" | "common_dir" => {
                    value[field] = serde_json::json!(f.root.join("foreign"));
                }
                "schema" => value["schema"] = serde_json::json!(2),
                "after" => value["after"] = serde_json::to_value(&before)?,
                "settled" => value["state"] = serde_json::json!("settled_unverified"),
                _ => unreachable!(),
            }
            fs::write(&path, serde_json::to_vec(&value)?)?;
            assert!(
                PreparedChange::prepare(&f.root, change()).is_err(),
                "accepted {field}"
            );
        }
        fs::write(&path, b"{broken")?;
        assert!(PreparedChange::prepare(&f.root, change()).is_err());
        persist(&path, &receipt, &witness)?;
        let renamed = path.with_extension("held");
        fs::rename(&path, &renamed)?;
        assert!(PreparedChange::prepare(&f.root, change()).is_err());
        fs::rename(&renamed, &path)?;
        assert_eq!(head(&f.root, None)?, before);
        f.git(&["show-ref", "--verify", "refs/heads/topic"])?;
        let result = PreparedChange::prepare(&f.root, change())?.execute()?;
        assert_eq!(result.state, "settled_unverified");
        assert!(path.exists(), "retirement never removes sibling evidence");
        Ok(())
    })
}

#[test]
fn branch_retirement_refuses_sibling_verification_after_approval() -> Result<()> {
    fixture(|f| {
        let branch = f.git(&["branch", "--show-current"])?;
        f.git(&["branch", "topic"])?;
        f.git(&["worktree", "add", "-q", "-b", "sibling", "../sibling"])?;
        let sibling = f.root.parent().unwrap().join("sibling");
        let identity = inspect_identity(&sibling, None)?;
        let before = head(&f.root, None)?;
        let index_before = fs::read(f.root.join(".git/index"))?;
        let change = || Change::DeleteBranch {
            branch: "topic".into(),
            retained_ref: format!("refs/heads/{branch}"),
        };
        let approved = PreparedChange::prepare(&f.root, change())?;
        // A crash can leave verification evidence even with no active owner or operation bucket.
        let directory = crate::core::config::aizen_home()
            .join("verification-v1")
            .join(&identity.repo_id)
            .join(&identity.worktree_id);
        let witness = crate::core::speculation::namespace::create_metadata_directory(&directory)?;
        let path = directory.join("sibling-verification.json");
        let mut record = serde_json::json!({
            "schema": 1,
            "id": "sibling-verification",
            "integration": "sibling-integration",
            "root": identity.canonical_root,
            "git_dir": identity.canonical_git_dir,
            "common_dir": identity.canonical_common_git_dir,
            "branch": "refs/heads/sibling",
            "destination": before,
            "commands": ["exit 0"],
            "state": "running",
            "verified": false,
            "termination": "running",
            "passed_commands": [],
            "output": "",
            "duration_ms": 0,
            "started_at": 1
        });
        let running_bytes = serde_json::to_vec(&record)?;
        fs::write(&path, &running_bytes)?;
        witness.verify()?;
        assert_eq!(
            verification_records(&sibling)?.len(),
            1,
            "valid sibling record"
        );
        let error = PreparedChange::prepare(&f.root, change()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unfinished destination verification"),
            "{error:#}"
        );
        let error = approved.execute().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unfinished destination verification"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(&path)?,
            running_bytes,
            "refusal never rewrites evidence"
        );
        assert_eq!(head(&f.root, None)?, before);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index_before);
        f.git(&["show-ref", "--verify", "refs/heads/topic"])?;
        // Removing an already clean disposable sibling is not proof that its workers settled.
        f.git(&["worktree", "remove", "../sibling"])?;
        for state in ["running", "unsettled"] {
            record["state"] = serde_json::json!(state);
            record["termination"] = serde_json::json!(state);
            if state == "unsettled" {
                record["finished_at"] = serde_json::json!(2);
            }
            fs::write(&path, serde_json::to_vec(&record)?)?;
            let error = PreparedChange::prepare(&f.root, change()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("unfinished destination verification"),
                "{state}: {error:#}"
            );
        }
        // A durable terminal failure is physically settled, but does not claim verification.
        record["state"] = serde_json::json!("failed");
        record["termination"] = serde_json::json!("error");
        let settled_bytes = serde_json::to_vec(&record)?;
        fs::write(&path, &settled_bytes)?;
        let result = PreparedChange::prepare(&f.root, change())?.execute()?;
        assert_eq!(result.state, "settled_unverified");
        assert_eq!(
            fs::read(&path)?,
            settled_bytes,
            "retirement retains sibling evidence"
        );
        assert_eq!(head(&f.root, None)?, before);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index_before);
        Ok(())
    })
}

#[test]
fn branch_retirement_validates_sibling_verification_metadata() -> Result<()> {
    fixture(|f| {
        f.git(&["worktree", "add", "-q", "-b", "sibling", "../sibling"])?;
        let sibling = inspect_identity(&f.root.parent().unwrap().join("sibling"), None)?;
        let parent = inspect_identity(&f.root, None)?;
        let directory = crate::core::config::aizen_home()
            .join("verification-v1")
            .join(&sibling.repo_id)
            .join(&sibling.worktree_id);
        let witness = crate::core::speculation::namespace::create_metadata_directory(&directory)?;
        let path = directory.join("sibling-verification.json");
        let record = serde_json::json!({
            "schema": 1,
            "id": "sibling-verification",
            "integration": "sibling-integration",
            "root": sibling.canonical_root,
            "git_dir": sibling.canonical_git_dir,
            "common_dir": sibling.canonical_common_git_dir,
            "branch": "refs/heads/sibling",
            "destination": head(&f.root, None)?,
            "commands": ["exit 0"],
            "state": "failed",
            "verified": false,
            "termination": "error",
            "passed_commands": [],
            "output": "",
            "duration_ms": 1,
            "started_at": 1,
            "finished_at": 2
        });
        let bytes = serde_json::to_vec(&record)?;
        fs::write(&path, &bytes)?;
        verification::require_repository_settled(&parent)?;
        for (field, value) in [
            ("schema", serde_json::json!(2)),
            ("id", serde_json::json!("different-id")),
            ("integration", serde_json::json!("")),
            ("root", serde_json::json!(f.root.join("missing-root"))),
            ("git_dir", serde_json::json!(f.root.join("missing-git"))),
            (
                "common_dir",
                serde_json::json!(f.root.join("foreign-repository")),
            ),
            ("root", serde_json::json!("relative-root")),
            ("state", serde_json::json!("unknown")),
            ("verified", serde_json::json!(true)),
            ("commands", serde_json::json!([])),
            ("commands", serde_json::json!([""])),
            ("started_at", serde_json::json!(0)),
            ("finished_at", serde_json::json!(null)),
            ("source_identity", serde_json::json!("invalid")),
        ] {
            let mut invalid = record.clone();
            invalid[field] = value;
            fs::write(&path, serde_json::to_vec(&invalid)?)?;
            assert!(
                verification::require_repository_settled(&parent).is_err(),
                "accepted {field}"
            );
        }
        let mut unknown = record.clone();
        unknown["state"] = serde_json::json!("settled");
        unknown["termination"] = serde_json::json!("unsettled");
        fs::write(&path, serde_json::to_vec(&unknown)?)?;
        assert!(verification::require_repository_settled(&parent).is_err());
        for corrupt in [b"{broken".to_vec(), vec![b' '; 128 * 1024 + 1]] {
            fs::write(&path, corrupt)?;
            assert!(verification::require_repository_settled(&parent).is_err());
        }
        fs::write(&path, &bytes)?;
        let renamed = path.with_extension("held");
        fs::rename(&path, &renamed)?;
        assert!(verification::require_repository_settled(&parent).is_err());
        fs::rename(&renamed, &path)?;
        let alias = f.root.parent().unwrap().join("verification-alias");
        fs::hard_link(&path, &alias)?;
        assert!(
            verification::require_repository_settled(&parent).is_err(),
            "hardlinked evidence"
        );
        fs::remove_file(&alias)?;
        let foreign_bucket = directory.parent().unwrap().join("wrong-worktree");
        fs::rename(&directory, &foreign_bucket)?;
        assert!(
            verification::require_repository_settled(&parent).is_err(),
            "misfiled evidence"
        );
        fs::rename(&foreign_bucket, &directory)?;
        witness.verify()?;
        verification::require_repository_settled(&parent)?;
        #[cfg(windows)]
        {
            let home = crate::core::config::aizen_home();
            let text = home.to_string_lossy();
            let ordinary = PathBuf::from(text.strip_prefix(r"\\?\").context("verbatim test home")?);
            let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &ordinary)]);
            verification::require_repository_settled(&parent)?;
        }
        assert_eq!(
            fs::read(&path)?,
            bytes,
            "inspection preserves exact evidence"
        );
        Ok(())
    })
}

#[test]
fn branch_retirement_does_not_bypass_upstream_porcelain_refusal() -> Result<()> {
    fixture(|f| {
        f.git(&["branch", "upstream"])?;
        f.commit_file("new.txt", b"retained by current branch\n")?;
        f.git(&["branch", "topic"])?;
        f.git(&["branch", "--set-upstream-to=upstream", "topic"])?;
        let branch = f.git(&["branch", "--show-current"])?;
        let before = f.git(&["rev-parse", "HEAD"])?;
        let plan = PreparedChange::prepare(
            &f.root,
            Change::DeleteBranch {
                branch: "topic".into(),
                retained_ref: format!("refs/heads/{branch}"),
            },
        )?;
        assert!(plan.execute().is_err());
        assert_eq!(f.git(&["rev-parse", "refs/heads/topic"])?, before);
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        let receipts = operation_receipts(&f.root)?;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].state, "failed_or_conflicted");
        Ok(())
    })
}

#[test]
fn dirty_or_drifted_worktree_blocks_before_git_mutation() -> Result<()> {
    fixture(|f| {
        let target = f.commit_file("a.txt", b"target\n")?;
        let plan = PreparedChange::prepare(
            &f.root,
            Change::Revert {
                commits: vec![target.clone()],
                mainline: None,
            },
        )?;
        fs::write(f.root.join("private.txt"), b"preserve")?;
        let before = f.git(&["rev-parse", "HEAD"])?;
        assert!(plan.execute().is_err());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        assert_eq!(fs::read(f.root.join("private.txt"))?, b"preserve");
        fs::remove_file(f.root.join("private.txt"))?;
        let plan = PreparedChange::prepare(
            &f.root,
            Change::Revert {
                commits: vec![target],
                mainline: None,
            },
        )?;
        f.commit_file("other.txt", b"new\n")?;
        assert!(plan.execute().is_err());
        Ok(())
    })
}

#[test]
fn conflict_keeps_git_sequencer_and_blocks_blind_retry() -> Result<()> {
    fixture(|f| {
        let target = f.commit_file("a.txt", b"target\n")?;
        f.commit_file("a.txt", b"later overlap\n")?;
        let error = f.revert(&target).unwrap_err();
        assert!(format!("{error:#}").contains("retained"));
        assert!(f.root.join(".git/REVERT_HEAD").exists());
        assert!(f.revert(&target).is_err());
        let directory = receipt_directory(&inspect_identity(&f.root, None)?);
        let receipts = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(receipts.len(), 1);
        let receipt: ChangeReceipt = serde_json::from_slice(&fs::read(receipts[0].path())?)?;
        assert_eq!(receipt.state, "failed_or_conflicted");
        Ok(())
    })
}

#[test]
fn switch_and_fast_forward_use_frozen_local_targets() -> Result<()> {
    fixture(|f| {
        let initial = f.git(&["branch", "--show-current"])?;
        f.git(&["branch", "topic"])?;
        let receipt = PreparedChange::prepare(
            &f.root,
            Change::Switch {
                branch: "topic".into(),
            },
        )?
        .execute()?;
        assert!(!receipt.verified);
        let target = f.commit_file("topic.txt", b"topic\n")?;
        PreparedChange::prepare(
            &f.root,
            Change::Switch {
                branch: initial.clone(),
            },
        )?
        .execute()?;
        // Config cannot turn the approved fast-forward into a squash/index-only change.
        f.git(&[
            "config",
            &format!("branch.{initial}.mergeOptions"),
            "--squash",
        ])?;
        PreparedChange::prepare(
            &f.root,
            Change::Integrate {
                commit: target.clone(),
            },
        )?
        .execute()?;
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, target);
        assert!(PreparedChange::prepare(
            &f.root,
            Change::Revert {
                commits: vec!["#1".into()],
                mainline: None
            }
        )
        .is_err());
        Ok(())
    })
}

#[test]
fn divergent_merge_records_two_parents_and_requires_explicit_mainline_for_undo() -> Result<()> {
    fixture(|f| {
        let destination = f.git(&["branch", "--show-current"])?;
        f.git(&["switch", "-qc", "topic"])?;
        let topic = f.commit_file("topic.txt", b"topic\n")?;
        f.git(&["switch", "-q", &destination])?;
        let before = f.commit_file("destination.txt", b"destination\n")?;
        assert!(PreparedChange::prepare(
            &f.root,
            Change::Integrate {
                commit: topic.clone()
            }
        )
        .is_err());
        // Explicit operation semantics override config without suppressing hooks or signing.
        f.git(&[
            "config",
            &format!("branch.{destination}.mergeOptions"),
            "--squash --no-commit",
        ])?;
        f.git(&["config", "merge.autoStash", "true"])?;
        let plan = PreparedChange::prepare(
            &f.root,
            Change::Merge {
                commit: topic.clone(),
            },
        )?;
        let preview: serde_json::Value = serde_json::from_str(&plan.preview()?)?;
        assert_eq!(preview["frozen_targets"][0], topic);
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        let result = plan.execute()?;
        assert_eq!(result.state, "settled_unverified");
        assert!(!result.verified);
        assert_eq!(result.resulting_commits.len(), 1);
        let merge = result.after.as_ref().unwrap();
        assert_eq!(
            parents(&f.root, merge, None)?,
            vec![GitOid::parse(&before)?, GitOid::parse(&topic)?]
        );
        assert!(f.root.join("topic.txt").exists() && f.root.join("destination.txt").exists());
        receipts::require_completed(&result)?;
        assert!(PreparedChange::prepare(&f.root, Change::Merge { commit: topic }).is_err());
        assert!(
            f.revert(merge.as_str()).is_err(),
            "merge undo must never guess mainline"
        );
        let undo = PreparedChange::prepare(
            &f.root,
            Change::Revert {
                commits: vec![merge.as_str().into()],
                mainline: Some(1),
            },
        )?
        .execute()?;
        assert!(!f.root.join("topic.txt").exists());
        assert!(f.root.join("destination.txt").exists());
        PreparedChange::prepare(&f.root, Change::Redo { receipt: undo.id })?.execute()?;
        assert!(f.root.join("topic.txt").exists());
        Ok(())
    })
}

#[test]
fn merge_conflict_retains_exact_target_and_never_retries_or_cleans() -> Result<()> {
    fixture(|f| {
        let destination = f.git(&["branch", "--show-current"])?;
        f.git(&["switch", "-qc", "topic"])?;
        let topic = f.commit_file("a.txt", b"topic\n")?;
        f.git(&["switch", "-q", &destination])?;
        let before = f.commit_file("a.txt", b"destination\n")?;
        let change = || Change::Merge {
            commit: topic.clone(),
        };
        assert!(PreparedChange::prepare(&f.root, change())?
            .execute()
            .is_err());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        assert_eq!(
            fs::read_to_string(f.root.join(".git/MERGE_HEAD"))?.trim(),
            topic
        );
        assert!(!f.git(&["ls-files", "--unmerged"])?.is_empty());
        let records = operation_receipts(&f.root)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "failed_or_conflicted");
        assert!(receipts::require_completed(&records[0]).is_err());
        let conflict = fs::read(f.root.join("a.txt"))?;
        assert!(PreparedChange::prepare(&f.root, change()).is_err());
        assert_eq!(fs::read(f.root.join("a.txt"))?, conflict);
        assert_eq!(operation_receipts(&f.root)?.len(), 1);
        Ok(())
    })
}

#[test]
fn cherry_pick_preserves_order_renames_and_later_destination_work() -> Result<()> {
    fixture(|f| {
        let destination = f.git(&["branch", "--show-current"])?;
        f.git(&["switch", "-qc", "topic"])?;
        f.git(&["mv", "a.txt", "renamed file.txt"])?;
        f.git(&["commit", "-qm", "rename"])?;
        let first = f.git(&["rev-parse", "HEAD"])?;
        let second = f.commit_file("renamed file.txt", b"renamed and edited\n")?;
        f.git(&["switch", "-q", &destination])?;
        let before = f.commit_file("destination.txt", b"keep\n")?;
        let result = PreparedChange::prepare(
            &f.root,
            Change::CherryPick {
                commits: vec![first.clone(), second.clone()],
                mainline: None,
            },
        )?
        .execute()?;
        assert_eq!(
            result.targets,
            vec![GitOid::parse(&first)?, GitOid::parse(&second)?]
        );
        assert_eq!(result.resulting_commits.len(), 2);
        assert_eq!(
            parents(&f.root, &result.resulting_commits[0], None)?,
            vec![GitOid::parse(&before)?]
        );
        assert_eq!(
            parents(&f.root, &result.resulting_commits[1], None)?,
            vec![result.resulting_commits[0].clone()]
        );
        assert!(!result.verified);
        receipts::require_completed(&result)?;
        assert!(!f.root.join("a.txt").exists());
        assert_eq!(
            fs::read_to_string(f.root.join("renamed file.txt"))?.replace("\r\n", "\n"),
            "renamed and edited\n"
        );
        assert!(f.root.join("destination.txt").exists());
        assert!(PreparedChange::prepare(
            &f.root,
            Change::Redo {
                receipt: result.id.clone()
            }
        )
        .is_err());
        let mut bad = result;
        bad.resulting_commits.pop();
        assert!(receipts::require_completed(&bad).is_err());
        Ok(())
    })
}

#[test]
fn cherry_pick_conflict_keeps_partial_mapping_and_blocks_replay() -> Result<()> {
    fixture(|f| {
        let destination = f.git(&["branch", "--show-current"])?;
        f.git(&["switch", "-qc", "topic"])?;
        let first = f.commit_file("topic.txt", b"first\n")?;
        let second = f.commit_file("a.txt", b"topic\n")?;
        f.git(&["switch", "-q", &destination])?;
        f.commit_file("a.txt", b"destination\n")?;
        let change = || Change::CherryPick {
            commits: vec![first.clone(), second.clone()],
            mainline: None,
        };
        assert!(PreparedChange::prepare(&f.root, change())?
            .execute()
            .is_err());
        let records = operation_receipts(&f.root)?;
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.state, "failed_or_conflicted");
        assert_eq!(record.resulting_commits.len(), 1);
        assert_eq!(record.after.as_ref(), record.resulting_commits.last());
        assert_eq!(
            f.git(&["rev-parse", "HEAD"])?,
            record.after.as_ref().unwrap().as_str()
        );
        assert_eq!(
            fs::read_to_string(f.root.join(".git/CHERRY_PICK_HEAD"))?.trim(),
            second
        );
        assert!(f.root.join("topic.txt").exists());
        assert!(PreparedChange::prepare(&f.root, change()).is_err());
        assert!(require_no_pending(&inspect_identity(&f.root, None)?).is_err());
        Ok(())
    })
}

#[test]
fn cherry_pick_duplicate_empty_and_merge_mainline_are_not_guessed() -> Result<()> {
    fixture(|f| {
        let oid = f.commit_file("a.txt", b"already here\n")?;
        assert!(PreparedChange::prepare(
            &f.root,
            Change::CherryPick {
                commits: vec![oid.clone(), oid.clone()],
                mainline: None,
            }
        )
        .is_err());
        assert!(PreparedChange::prepare(
            &f.root,
            Change::CherryPick {
                commits: vec![oid.clone()],
                mainline: Some(1),
            }
        )
        .is_err());
        let change = Change::CherryPick {
            commits: vec![oid.clone()],
            mainline: None,
        };
        assert!(PreparedChange::prepare(&f.root, change)?.execute().is_err());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, oid);
        let records = operation_receipts(&f.root)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "failed_or_conflicted");
        assert!(records[0].resulting_commits.is_empty());
        assert!(f.root.join(".git/CHERRY_PICK_HEAD").exists());
        Ok(())
    })
}

#[test]
fn explicit_reconciliation_preserves_failed_receipt_and_requires_manual_resolution() -> Result<()> {
    fixture(|f| {
        let target = f.commit_file("a.txt", b"selected\n")?;
        let before = f.commit_file("a.txt", b"later overlapping change\n")?;
        assert!(f.revert(&target).is_err());
        let original = operation_receipts(&f.root)?.remove(0);
        let change = || Change::Reconcile {
            receipt: original.id.clone(),
        };
        assert!(
            PreparedChange::prepare(&f.root, change()).is_err(),
            "live sequencer is not a manual resolution"
        );
        // The fixture operator, not Aizen, explicitly aborts Git's failed revert.
        f.git(&["revert", "--abort"])?;
        let identity = inspect_identity(&f.root, None)?;
        assert!(require_no_pending(&identity).is_err());
        let path = receipt_directory(&identity).join(format!("{}.json", original.id));
        let bytes = fs::read(&path)?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        let preview: serde_json::Value = serde_json::from_str(&plan.preview()?)?;
        assert_eq!(
            preview["selected_failed_operation"]["state"],
            "failed_or_conflicted"
        );
        assert_eq!(fs::read(&path)?, bytes, "preview cannot acknowledge");
        let index = fs::read(f.root.join(".git/index"))?;
        let file = fs::read(f.root.join("a.txt"))?;
        let reconciled = plan.execute()?;
        assert_eq!(reconciled.state, original.state);
        assert_eq!(reconciled.after, original.after);
        assert_eq!(reconciled.resulting_commits, original.resulting_commits);
        assert_eq!(
            reconciled
                .reconciliation
                .as_ref()
                .unwrap()
                .observed_head
                .as_str(),
            before
        );
        assert!(!reconciled.verified);
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
        assert_eq!(fs::read(f.root.join(".git/index"))?, index);
        assert_eq!(fs::read(f.root.join("a.txt"))?, file);
        assert_eq!(
            operation_receipts(&f.root)?.len(),
            1,
            "acknowledgement is not a fake successful operation"
        );
        require_no_pending(&identity)?;
        assert!(
            PreparedChange::prepare(&f.root, change()).is_err(),
            "cannot acknowledge twice"
        );
        assert!(
            PreparedChange::prepare(
                &f.root,
                Change::Redo {
                    receipt: original.id
                }
            )
            .is_err(),
            "failed undo cannot become redo authority"
        );
        f.git(&["branch", "topic"])?;
        PreparedChange::prepare(
            &f.root,
            Change::Switch {
                branch: "topic".into(),
            },
        )?
        .execute()?;
        Ok(())
    })
}

#[test]
fn reconciliation_refuses_unknown_effects_receipt_drift_dirty_state_and_history_loss() -> Result<()>
{
    fixture(|f| {
        let target = f.commit_file("a.txt", b"selected\n")?;
        f.commit_file("a.txt", b"later overlapping change\n")?;
        assert!(f.revert(&target).is_err());
        let original = operation_receipts(&f.root)?.remove(0);
        f.git(&["revert", "--abort"])?;
        let identity = inspect_identity(&f.root, None)?;
        let path = receipt_directory(&identity).join(format!("{}.json", original.id));
        let bytes = fs::read(&path)?;
        let change = || Change::Reconcile {
            receipt: original.id.clone(),
        };
        for state in [
            "running",
            "prepared",
            "unsettled",
            "unknown",
            "not_started",
            "settled_unverified",
        ] {
            let mut record = serde_json::to_value(&original)?;
            record["state"] = serde_json::json!(state);
            fs::write(&path, serde_json::to_vec(&record)?)?;
            assert!(
                PreparedChange::prepare(&f.root, change()).is_err(),
                "accepted {state}"
            );
        }
        fs::write(&path, &bytes)?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        let mut drifted = bytes.clone();
        drifted.push(b'\n');
        fs::write(&path, &drifted)?;
        assert!(plan.execute().is_err());
        assert_eq!(fs::read(&path)?, drifted);
        fs::write(&path, &bytes)?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        fs::write(f.root.join("untracked"), b"keep")?;
        assert!(plan.execute().is_err());
        assert_eq!(fs::read(f.root.join("untracked"))?, b"keep");
        fs::remove_file(f.root.join("untracked"))?;
        let plan = PreparedChange::prepare(&f.root, change())?;
        f.commit_file("later.txt", b"drift\n")?;
        assert!(plan.execute().is_err(), "HEAD drift invalidates approval");
        f.git(&["switch", "-qc", "other"])?;
        assert!(
            PreparedChange::prepare(&f.root, change()).is_err(),
            "receipt belongs to original branch"
        );
        let mut record = serde_json::to_value(&original)?;
        record["branch_before"] = serde_json::json!("refs/heads/other");
        record["after"] = serde_json::json!("1111111111111111111111111111111111111111");
        fs::write(&path, serde_json::to_vec(&record)?)?;
        assert!(
            PreparedChange::prepare(&f.root, change()).is_err(),
            "unknown recorded history cannot be acknowledged"
        );
        Ok(())
    })
}

#[test]
fn reconciled_partial_cherry_pick_keeps_mapping_and_later_manual_resolution() -> Result<()> {
    fixture(|f| {
        let destination = f.git(&["branch", "--show-current"])?;
        f.git(&["switch", "-qc", "topic"])?;
        let first = f.commit_file("topic.txt", b"first\n")?;
        let second = f.commit_file("a.txt", b"topic\n")?;
        f.git(&["switch", "-q", &destination])?;
        f.commit_file("a.txt", b"destination\n")?;
        assert!(PreparedChange::prepare(
            &f.root,
            Change::CherryPick {
                commits: vec![first, second],
                mainline: None
            }
        )?
        .execute()
        .is_err());
        let original = operation_receipts(&f.root)?.remove(0);
        assert_eq!(original.resulting_commits.len(), 1);
        f.git(&["cherry-pick", "--abort"])?;
        let reconciled = PreparedChange::prepare(
            &f.root,
            Change::Reconcile {
                receipt: original.id.clone(),
            },
        )?
        .execute()?;
        assert_eq!(reconciled.resulting_commits, original.resulting_commits);
        assert_eq!(reconciled.after, original.after);
        assert_eq!(reconciled.state, "failed_or_conflicted");
        assert!(f.root.join("topic.txt").exists());
        receipts::require_completed(&reconciled)?;
        // An acknowledgement never changes an unknown/running outcome into safe terminal state.
        let mut forged = reconciled;
        forged.state = "unsettled".into();
        assert!(receipts::require_completed(&forged).is_err());
        Ok(())
    })
}

#[test]
fn pre_submission_refusal_is_not_a_pending_mutation() -> Result<()> {
    let refusal: Result<()> = Err(anyhow::anyhow!("policy refused before submission"));
    assert_eq!(outcome_state(&refusal, false), "not_started");
    assert_eq!(outcome_state(&refusal, true), "failed_or_conflicted");
    let unsettled: Result<()> = Err(anyhow::Error::new(
        crate::agent::tools::ToolEffectsUnsettled(
            crate::agent::tools::ToolEffectState::UnknownWorker,
        ),
    ));
    assert_eq!(outcome_state(&unsettled, false), "unsettled");
    fixture(|f| {
        let identity = inspect_identity(&f.root, None)?;
        let directory = receipt_directory(&identity);
        let witness = crate::core::speculation::namespace::create_metadata_directory(&directory)?;
        let before = head(&f.root, None)?;
        let mut receipt = ChangeReceipt {
            schema: 1,
            id: "refused".into(),
            root: identity.canonical_root.clone(),
            git_dir: identity.canonical_git_dir.clone().unwrap(),
            common_dir: identity.canonical_common_git_dir.clone().unwrap(),
            before: before.clone(),
            after: None,
            branch_before: source_branch(&f.root, None)?.unwrap(),
            change: Change::Integrate {
                commit: before.as_str().into(),
            },
            targets: vec![before],
            state: "not_started".into(),
            resulting_commits: Vec::new(),
            verified: false,
            reconciliation: None,
            handoff: None,
            verification: None,
            sequencer: None,
        };
        let path = directory.join("refused.json");
        persist(&path, &receipt, &witness)?;
        require_no_pending(&identity)?;
        for state in ["prepared", "running", "unsettled", "failed_or_conflicted"] {
            receipt.state = state.into();
            persist(&path, &receipt, &witness)?;
            assert!(require_no_pending(&identity).is_err());
        }
        Ok(())
    })
}

#[test]
fn hook_failure_retains_effects_without_bypass_or_retry() -> Result<()> {
    fixture(|f| {
        let target = f.commit_file("a.txt", b"target\n")?;
        // Git's sequencer does not invoke pre-commit for revert. Its prepare-commit-msg hook
        // does run and must not be suppressed or retried without the configured hook.
        let hook = f.root.join(".git/hooks/prepare-commit-msg");
        fs::write(
            &hook,
            b"#!/bin/sh\nprintf 'hook effect\\n' > hook.txt\nexit 7\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
        }
        assert!(f.revert(&target).is_err());
        assert!(f.root.join("hook.txt").exists());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, target);
        Ok(())
    })
}

#[path = "git_ops_sequencer_tests.rs"]
mod sequencer_tests;

#[path = "git_ops_operation_inspection_tests.rs"]
mod operation_inspection_tests;
