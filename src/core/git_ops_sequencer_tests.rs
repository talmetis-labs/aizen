//! Recognized conflict continue/abort/quit bound to the recorded failed operation that owns it.
use super::*;
use crate::core::git_ops::{operation_receipts, Change, Prepared, PreparedChange};

/// A revert that conflicts, leaving a REVERT_HEAD and one failed receipt.
fn conflicted_revert(f: &Fixture) -> Result<String> {
    let target = f.commit_file("a.txt", b"target\n")?;
    f.commit_file("a.txt", b"later overlap\n")?;
    assert!(f.revert(&target).is_err());
    ensure!(
        f.root.join(".git/REVERT_HEAD").exists(),
        "fixture did not leave a reverting sequencer"
    );
    let failed = operation_receipts(&f.root)?
        .into_iter()
        .find(|receipt| receipt.state == "failed_or_conflicted")
        .context("missing failed receipt")?;
    Ok(failed.id)
}

fn execute(root: &Path, receipt: &str, step: &str) -> Result<crate::core::git_ops::ChangeReceipt> {
    let prepared = Prepared::prepare(
        root,
        Change::Sequencer {
            receipt: receipt.into(),
            step: step.into(),
        },
    )?;
    match prepared.execute()? {
        crate::core::git_ops::PreparedOutcome::Change(receipt) => Ok(*receipt),
        _ => anyhow::bail!("sequencer step produced a non-change outcome"),
    }
}

#[test]
fn abort_restores_the_recorded_failure_and_clears_only_its_markers() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        for id in ["wrong-operation-1", "../escape", ""] {
            assert!(
                Prepared::prepare(
                    &f.root,
                    Change::Sequencer {
                        receipt: id.into(),
                        step: "abort".into(),
                    }
                )
                .is_err(),
                "accepted wrong selected receipt {id:?}"
            );
        }
        let conflicted = f.git(&["rev-parse", "HEAD"])?;
        let preview: serde_json::Value = serde_json::from_str(
            &Prepared::prepare(
                &f.root,
                Change::Sequencer {
                    receipt: receipt.clone(),
                    step: "abort".into(),
                },
            )?
            .preview()?,
        )?;
        assert_eq!(preview["step"], "abort");
        assert_eq!(preview["owned_receipt"], receipt);
        assert_eq!(preview["operation"], "revert");
        let settled = execute(&f.root, &receipt, "abort")?;
        assert_eq!(settled.state, "settled_unverified");
        assert!(!settled.verified);
        assert!(matches!(settled.action(), Change::Sequencer { .. }));
        assert!(!f.root.join(".git/REVERT_HEAD").exists());
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, conflicted);
        assert_eq!(f.git(&["show", "HEAD:a.txt"])?, "later overlap");
        // The original conflicted receipt still requires explicit reconciliation: aborting the
        // sequencer is not the same as acknowledging the failure. Another mutation stays blocked
        // until that receipt is reconciled, which is the honest state after an abort.
        assert!(
            crate::core::git_ops::require_settled_operations(&inspect_identity(&f.root, None)?)
                .is_err()
        );
        // The original failure remains recorded; the step is a separate receipt.
        assert!(operation_receipts(&f.root)?.len() >= 2);
        Ok(())
    })
}

#[test]
fn continue_requires_a_resolved_index_and_then_commits() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        std::fs::write(f.root.join("a.txt"), b"resolved content\n")?;
        // Unresolved (unstaged) index cannot be committed by Git.
        assert!(execute(&f.root, &receipt, "continue").is_err());
        assert!(f.root.join(".git/REVERT_HEAD").exists());
        f.git(&["add", "--", "a.txt"])?;
        let settled = execute(&f.root, &receipt, "continue")?;
        assert_eq!(settled.state, "settled_unverified");
        assert!(!f.root.join(".git/REVERT_HEAD").exists());
        let head = f.git(&["rev-parse", "HEAD"])?;
        assert_eq!(
            settled.after.as_ref().map(|oid| oid.as_str()),
            Some(head.as_str())
        );
        assert_eq!(std::fs::read(f.root.join("a.txt"))?, b"resolved content\n");
        Ok(())
    })
}

#[test]
fn unknown_or_unowned_sequencer_state_is_refused_not_cleaned() -> Result<()> {
    fixture(|f| {
        // Start a conflict that belongs to no receipt at all.
        let target = f.commit_file("a.txt", b"target\n")?;
        f.commit_file("a.txt", b"later overlap\n")?;
        std::process::Command::new(&f.git)
            .current_dir(&f.root)
            .args(["revert", "--no-edit", &target])
            .output()?;
        assert!(f.root.join(".git/REVERT_HEAD").exists());
        let error = Prepared::prepare(
            &f.root,
            Change::Sequencer {
                receipt: "unknown-operation-1".into(),
                step: "abort".into(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("no recorded failed operation"),
            "{error:#}"
        );
        // The unowned conflict is preserved exactly.
        assert!(f.root.join(".git/REVERT_HEAD").exists());
        assert!(operation_receipts(&f.root)?.is_empty());
        Ok(())
    })
}

#[test]
fn unsupported_sequencer_kinds_and_unknown_steps_are_refused() -> Result<()> {
    fixture(|f| {
        assert!(Prepared::prepare(
            &f.root,
            Change::Sequencer {
                receipt: "x-1".into(),
                step: "reset".into(),
            }
        )
        .is_err());
        // No sequencer at all: a clear refusal, never a silent no-op.
        let error = Prepared::prepare(
            &f.root,
            Change::Sequencer {
                receipt: "x-1".into(),
                step: "abort".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("no recognized"), "{error:#}");
        // A rebase in progress is explicitly out of scope.
        std::fs::create_dir_all(f.root.join(".git/rebase-merge"))?;
        let error = Prepared::prepare(
            &f.root,
            Change::Sequencer {
                receipt: "x-1".into(),
                step: "abort".into(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("unsupported Git operation state"),
            "{error:#}"
        );
        std::fs::remove_dir(f.root.join(".git/rebase-merge"))?;
        Ok(())
    })
}

#[test]
fn step_approval_is_stale_when_the_destination_moves() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        let prepared = Prepared::prepare(
            &f.root,
            Change::Sequencer {
                receipt: receipt.clone(),
                step: "abort".into(),
            },
        )?;
        // Resolve and commit the conflict by hand, moving HEAD and clearing the marker.
        std::fs::write(f.root.join("a.txt"), b"manual\n")?;
        f.git(&["add", "--", "a.txt"])?;
        f.git(&[
            "-c",
            "core.hooksPath=.git/hooks",
            "revert",
            "--continue",
            "--no-edit",
        ])?;
        assert!(prepared.execute().is_err());
        assert!(!f.root.join(".git/REVERT_HEAD").exists());
        Ok(())
    })
}

#[test]
fn sequencer_receipts_round_trip_and_refuse_conflicting_links() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        let settled = execute(&f.root, &receipt, "quit")?;
        assert_eq!(settled.state, "settled_unverified");
        assert!(!f.root.join(".git/REVERT_HEAD").exists());
        // quit only clears markers: the conflicted file still carries its conflict markers.
        let text = String::from_utf8(std::fs::read(f.root.join("a.txt"))?)?;
        assert!(
            text.contains("<<<<<<<") && text.contains(">>>>>>>"),
            "{text}"
        );
        let stored = operation_receipts(&f.root)?
            .into_iter()
            .find(|value| value.id == settled.id)
            .context("missing sequencer receipt")?;
        assert!(stored.verification.is_none() && !stored.verified);
        assert!(matches!(stored.action(), Change::Sequencer { .. }));
        Ok(())
    })
}

#[test]
fn approval_refuses_later_edits_receipt_changes_and_recreated_marker() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        let prepare = || {
            Prepared::prepare(
                &f.root,
                Change::Sequencer {
                    receipt: receipt.clone(),
                    step: "abort".into(),
                },
            )
        };
        let prepared = prepare()?;
        let conflict = fs::read(f.root.join("a.txt"))?;
        fs::write(f.root.join("a.txt"), b"new user resolution\n")?;
        assert!(prepared.execute().is_err());
        assert_eq!(fs::read(f.root.join("a.txt"))?, b"new user resolution\n");
        fs::write(f.root.join("a.txt"), &conflict)?;
        let prepared = prepare()?;
        let identity = inspect_identity(&f.root, None)?;
        let path = receipt_directory(&identity).join(format!("{receipt}.json"));
        let bytes = fs::read(&path)?;
        let mut modified: serde_json::Value = serde_json::from_slice(&bytes)?;
        modified["after"] = modified["before"].clone();
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&modified)?,
        )?;
        assert!(prepared.execute().is_err());
        crate::core::persist::atomic_write_owner_only_checked(&path, &bytes)?;
        let marker = f.root.join(".git/REVERT_HEAD");
        let original = fs::read(&marker)?;
        // Atomically replacing with the identical OID represents another marker instance.
        crate::core::persist::atomic_write_owner_only_checked(&marker, &original)?;
        assert!(prepare().is_err());
        assert_eq!(fs::read(f.root.join("a.txt"))?, conflict);
        Ok(())
    })
}

#[test]
fn malformed_and_legacy_conflict_evidence_never_grants_abort_authority() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        let identity = inspect_identity(&f.root, None)?;
        let path = receipt_directory(&identity).join(format!("{receipt}.json"));
        let bytes = fs::read(&path)?;
        let mut legacy: serde_json::Value = serde_json::from_slice(&bytes)?;
        legacy.as_object_mut().unwrap().remove("sequencer");
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&legacy)?,
        )?;
        assert!(execute(&f.root, &receipt, "abort").is_err());
        crate::core::persist::atomic_write_owner_only_checked(&path, &bytes)?;
        let marker = f.root.join(".git/REVERT_HEAD");
        let oid = fs::read(&marker)?;
        for value in [
            Vec::new(),
            [oid.clone(), oid.clone()].concat(),
            vec![b'0'; 65537],
        ] {
            fs::write(&marker, &value)?;
            assert!(execute(&f.root, &receipt, "quit").is_err());
            assert_eq!(fs::read(&marker)?, value);
        }
        Ok(())
    })
}

#[test]
fn continue_covers_merge_and_cherry_pick_with_exact_parents() -> Result<()> {
    fixture(|f| {
        let base = f.git(&["rev-parse", "HEAD"])?;
        let branch = f.git(&["branch", "--show-current"])?;
        f.git(&["checkout", "-qb", "source"])?;
        let target = f.commit_file("a.txt", b"source\n")?;
        f.git(&["checkout", &branch])?;
        f.commit_file("a.txt", b"destination\n")?;
        assert!(PreparedChange::prepare(
            &f.root,
            Change::Merge {
                commit: target.clone()
            }
        )?
        .execute()
        .is_err());
        let failed = operation_receipts(&f.root)?
            .into_iter()
            .find(|r| r.state == "failed_or_conflicted")
            .unwrap();
        fs::write(f.root.join("a.txt"), b"merged resolution\n")?;
        f.git(&["add", "--", "a.txt"])?;
        let merged = execute(&f.root, &failed.id, "continue")?;
        assert_eq!(
            f.git(&["rev-list", "--parents", "-n", "1", "HEAD"])?
                .split_whitespace()
                .count(),
            3
        );
        assert_eq!(merged.state, "settled_unverified");
        PreparedChange::prepare(&f.root, Change::Reconcile { receipt: failed.id })?.execute()?;
        // New independent destination branch; old failure has been explicitly acknowledged.
        f.git(&["checkout", "-qb", "pick-destination", &base])?;
        f.commit_file("a.txt", b"pick overlap\n")?;
        assert!(PreparedChange::prepare(
            &f.root,
            Change::CherryPick {
                commits: vec![target],
                mainline: None
            }
        )?
        .execute()
        .is_err());
        let failed = operation_receipts(&f.root)?
            .into_iter()
            .find(|r| r.state == "failed_or_conflicted" && r.reconciliation.is_none())
            .unwrap();
        fs::write(f.root.join("a.txt"), b"pick resolution\n")?;
        f.git(&["add", "--", "a.txt"])?;
        assert_eq!(
            execute(&f.root, &failed.id, "continue")?.state,
            "settled_unverified"
        );
        assert_eq!(
            f.git(&["rev-list", "--parents", "-n", "1", "HEAD"])?
                .split_whitespace()
                .count(),
            2
        );
        Ok(())
    })
}

#[test]
fn failed_continue_publishes_an_attempt_that_blocks_retry() -> Result<()> {
    fixture(|f| {
        let receipt = conflicted_revert(f)?;
        fs::write(f.root.join("a.txt"), b"resolution\n")?;
        f.git(&["add", "--", "a.txt"])?;
        let hook = f.root.join(".git/hooks/prepare-commit-msg");
        fs::write(&hook, b"#!/bin/sh\nexit 7\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
        }
        assert!(execute(&f.root, &receipt, "continue").is_err());
        let records = operation_receipts(&f.root)?;
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .any(|r| matches!(r.action(), Change::Sequencer { .. })
                && r.state == "failed_or_conflicted"));
        assert!(Prepared::prepare(
            &f.root,
            Change::Sequencer {
                receipt,
                step: "continue".into()
            }
        )
        .is_err());
        Ok(())
    })
}

#[test]
fn plain_change_preparation_refuses_sequencer_actions() {
    assert!(PreparedChange::prepare(
        &std::env::temp_dir(),
        Change::Sequencer {
            receipt: "x-1".into(),
            step: "abort".into(),
        }
    )
    .is_err());
}
