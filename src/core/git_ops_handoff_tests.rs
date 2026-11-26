//! Real registered task handoffs integrated only in disposable repositories.
use super::*;
use crate::core::git_ops::operation_receipts;

fn completed(f: &Fixture) -> Result<TaskHandoff> {
    let task = f.task()?;
    let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
    let attempt = f.begin(&task, &scope)?;
    f.write(&attempt, "tracked.txt", b"task result\n")?;
    f.finish(task, &scope, attempt)
}

fn action(handoff: &TaskHandoff) -> Change {
    Change::IntegrateTask {
        receipt: handoff.registration.registration_id.clone(),
    }
}

fn manifest_path(f: &Fixture, handoff: &TaskHandoff) -> PathBuf {
    f.home
        .join("coop")
        .join(&handoff.repo_id)
        .join("sessions")
        .join(format!("{}.json", handoff.registration.registration_id))
}

fn parent_state(f: &Fixture) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    Ok((
        f.git(&["rev-parse", "HEAD"])?,
        std::fs::read(f.root.join(".git/index"))?,
        std::fs::read(f.root.join("tracked.txt"))?,
    ))
}

#[test]
fn registered_task_integration_preserves_source_and_records_exact_unverified_handoff() -> Result<()>
{
    fixture(|f| {
        let handoff = completed(f)?;
        let parent = parent_state(f)?;
        let source_index = f.git_at(&handoff.root, &["rev-parse", "--git-path", "index"])?;
        let source_index = PathBuf::from(std::str::from_utf8(&source_index)?.trim());
        let source_index = if source_index.is_absolute() {
            source_index
        } else {
            handoff.root.join(source_index)
        };
        let source_index_bytes = std::fs::read(&source_index)?;
        let manifest = manifest_path(f, &handoff);
        let manifest_bytes = std::fs::read(&manifest)?;
        let prepared = PreparedChange::prepare(&f.root, action(&handoff))?;
        let preview: serde_json::Value = serde_json::from_str(&prepared.preview()?)?;
        assert_eq!(preview["task_handoff"], serde_json::to_value(&handoff)?);
        assert_eq!(parent_state(f)?, parent);
        assert!(operation_receipts(&f.root)?.is_empty());
        let receipt = prepared.execute()?;
        assert_eq!(receipt.after.as_ref(), Some(&handoff.tip));
        assert_eq!(receipt.state, "settled_unverified");
        assert!(!receipt.verified && receipt.resulting_commits.is_empty());
        assert_eq!(head(&f.root, None)?, handoff.tip);
        assert_eq!(std::fs::read(f.root.join("tracked.txt"))?, b"task result\n");
        assert_eq!(head(&handoff.root, None)?, handoff.tip);
        assert_eq!(std::fs::read(source_index)?, source_index_bytes);
        assert_eq!(std::fs::read(&manifest)?, manifest_bytes);
        let records = operation_receipts(&f.root)?;
        assert_eq!(records.len(), 1);
        let value = serde_json::to_value(&records[0])?;
        assert_eq!(value["schema"], 2);
        assert_eq!(value["handoff"], serde_json::to_value(&handoff)?);
        assert!(PreparedChange::prepare(&f.root, action(&handoff)).is_err());
        // Historical receipt validation never follows source paths or requires a still-current tip.
        f.git_at(
            &handoff.root,
            &["commit", "--allow-empty", "-qm", "later source work"],
        )?;
        assert_eq!(operation_receipts(&f.root)?.len(), 1);
        Ok(())
    })
}

#[test]
fn registered_task_integration_refuses_live_empty_and_changed_registrations() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        let empty = f.finish(task.clone(), &scope, attempt)?;
        let before = parent_state(f)?;
        let error = PreparedChange::prepare(&f.root, action(&empty)).unwrap_err();
        assert!(error.to_string().contains("still live"), "{error:#}");
        drop(task);
        let error = PreparedChange::prepare(&f.root, action(&empty)).unwrap_err();
        assert!(
            error.to_string().contains("empty task integration"),
            "{error:#}"
        );
        let handoff = completed(f)?;
        let manifest = manifest_path(f, &handoff);
        let bytes = std::fs::read(&manifest)?;
        let original: serde_json::Value = serde_json::from_slice(&bytes)?;
        for pointer in [
            "/git_task/phase",
            "/git_task/paths",
            "/worktree_id",
            "/root",
            "/repo_id",
            "/schema",
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).unwrap() = match pointer {
                "/git_task/phase" => serde_json::json!("committing"),
                "/git_task/paths" => serde_json::json!(["outside.txt"]),
                "/schema" => serde_json::json!(999),
                "/root" => serde_json::json!(f.root.parent().unwrap().join("not-in-inventory")),
                _ => serde_json::json!("different-identity"),
            };
            crate::core::persist::atomic_write_owner_only_checked(
                &manifest,
                &serde_json::to_vec(&changed)?,
            )?;
            assert!(
                PreparedChange::prepare(&f.root, action(&handoff)).is_err(),
                "accepted {pointer}"
            );
        }
        crate::core::persist::atomic_write_owner_only_checked(&manifest, &bytes)?;
        let prepared = PreparedChange::prepare(&f.root, action(&handoff))?;
        let mut changed = original;
        changed["git_task"]["owner"] = serde_json::json!("different-owner");
        crate::core::persist::atomic_write_owner_only_checked(
            &manifest,
            &serde_json::to_vec(&changed)?,
        )?;
        let error = prepared.execute().unwrap_err();
        // The v2 lifecycle binds the original owner independently of the frozen manifest.
        // Registry admission rejects this tampering before the later fingerprint comparison.
        assert_eq!(
            error.to_string(),
            "native lifecycle registration binding changed",
            "{error:#}"
        );
        assert_eq!(parent_state(f)?, before);
        assert!(operation_receipts(&f.root)?.is_empty());
        assert_eq!(head(&handoff.root, None)?, handoff.tip);
        Ok(())
    })
}

#[test]
fn registered_task_integration_refuses_source_and_destination_drift_before_effects() -> Result<()> {
    fixture(|f| {
        let handoff = completed(f)?;
        let before = parent_state(f)?;
        // Each fixture mutation is restored from the exact known bytes, never reset/clean.
        for (root, name) in [
            (&handoff.root, "tracked.txt"),
            (&handoff.root, "ignored.tmp"),
            (&f.root, "tracked.txt"),
        ] {
            let path = root.join(name);
            let previous = std::fs::read(&path).ok();
            let prepared = PreparedChange::prepare(&f.root, action(&handoff))?;
            std::fs::write(&path, b"independent pending edits\n")?;
            assert!(prepared.execute().is_err());
            assert_eq!(std::fs::read(&path)?, b"independent pending edits\n");
            assert!(PreparedChange::prepare(&f.root, action(&handoff)).is_err());
            match previous {
                Some(bytes) => std::fs::write(&path, bytes)?,
                None => std::fs::remove_file(&path)?,
            }
            assert_eq!(parent_state(f)?, before);
            assert!(operation_receipts(&f.root)?.is_empty());
        }
        // A different source branch at the same OID also invalidates the approved handoff.
        let prepared = PreparedChange::prepare(&f.root, action(&handoff))?;
        f.git_at(&handoff.root, &["switch", "-qc", "source-other"])?;
        assert!(prepared.execute().is_err());
        f.git_at(
            &handoff.root,
            &[
                "switch",
                "-q",
                handoff.branch.strip_prefix("refs/heads/").unwrap(),
            ],
        )?;
        let prepared = PreparedChange::prepare(&f.root, action(&handoff))?;
        f.git_at(
            &handoff.root,
            &["commit", "--allow-empty", "-qm", "source moved"],
        )?;
        assert!(prepared.execute().is_err());
        assert_eq!(parent_state(f)?, before);
        assert!(operation_receipts(&f.root)?.is_empty());
        Ok(())
    })
}

#[test]
fn registered_task_integration_keeps_applied_state_if_hook_changes_source() -> Result<()> {
    fixture(|f| {
        let task = f.task()?;
        let source = hook_config_path(&task.root().join("tracked.txt")).replace('\'', "'\\''");
        f.hook(
            &task,
            "post-merge",
            &format!("printf 'hook changed source\\n' > '{source}'"),
        )?;
        let scope = NativeWriteScope::resolve(task.root(), &["tracked.txt".into()])?;
        let attempt = f.begin(&task, &scope)?;
        f.write(&attempt, "tracked.txt", b"task result\n")?;
        let handoff = f.finish(task, &scope, attempt)?;
        let error = PreparedChange::prepare(&f.root, action(&handoff))?
            .execute()
            .unwrap_err();
        assert!(format!("{error:#}").contains("clean"), "{error:#}");
        assert_eq!(head(&f.root, None)?, handoff.tip);
        assert_eq!(std::fs::read(f.root.join("tracked.txt"))?, b"task result\n");
        assert_eq!(
            std::fs::read(handoff.root.join("tracked.txt"))?,
            b"hook changed source\n"
        );
        let records = operation_receipts(&f.root)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "failed_or_conflicted");
        assert_eq!(records[0].after.as_ref(), Some(&handoff.tip));
        assert!(!records[0].verified);
        assert!(
            crate::core::git_ops::require_settled_operations(&inspect_identity(&f.root, None)?)
                .is_err()
        );
        Ok(())
    })
}

#[test]
fn task_integration_receipts_reject_missing_or_inconsistent_handoff_without_source_reads(
) -> Result<()> {
    fixture(|f| {
        let handoff = completed(f)?;
        let receipt = PreparedChange::prepare(&f.root, action(&handoff))?.execute()?;
        let original = serde_json::to_value(&receipt)?;
        let identity = inspect_identity(&f.root, None)?;
        let path = f
            .home
            .join("git-operations-v1")
            .join(&identity.repo_id)
            .join(&identity.worktree_id)
            .join(format!("{}.json", receipt.id));
        for pointer in [
            "/handoff",
            "/schema",
            "/handoff/base",
            "/handoff/registration/owner",
            "/handoff/worktree_id",
            "/handoff/commits",
            "/verified",
            "/after",
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).unwrap() = match pointer {
                "/handoff" => serde_json::Value::Null,
                "/schema" => serde_json::json!(1),
                "/handoff/base" => serde_json::to_value(&handoff.tip)?,
                "/handoff/registration/owner" => serde_json::json!("foreign-owner"),
                "/handoff/worktree_id" => serde_json::json!(&identity.worktree_id),
                "/handoff/commits" => serde_json::json!([]),
                "/verified" => serde_json::json!(true),
                "/after" => serde_json::to_value(&handoff.base)?,
                _ => unreachable!(),
            };
            crate::core::persist::atomic_write_owner_only_checked(
                &path,
                &serde_json::to_vec(&changed)?,
            )?;
            assert!(
                crate::core::git_ops::require_settled_operations(&identity).is_err(),
                "accepted {pointer}"
            );
        }
        crate::core::persist::atomic_write_owner_only_checked(
            &path,
            &serde_json::to_vec(&original)?,
        )?;
        crate::core::git_ops::require_settled_operations(&identity)?;
        Ok(())
    })
}
