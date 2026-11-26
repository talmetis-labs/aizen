//! Planning uses filesystem-only lease identity, but shares physical and exact-file exclusion.
use super::*;
use crate::core::exec_ctx::{with_current, ExecutionContext};
use crate::core::file_placement::tests::TestDir;
use crate::core::session_control::ExecutionMode;

fn planning_context() -> ExecutionContext {
    let task = crate::agent::todo::TaskContext::private();
    task.control()
        .transition(ExecutionMode::Planning, None, |_| Ok(()))
        .unwrap();
    ExecutionContext::default().with_task(task)
}

#[test]
fn planning_lease_identity_never_enters_git_discovery() {
    let temp = TestDir::new();
    let root = temp.path().canonicalize().unwrap();
    let resource = NativeResource::resolve(&root.join("plan.md")).unwrap();
    with_test_lock_home(&root, || {
        GIT_DISCOVERY_CALLS.with(|calls| calls.set(0));
        with_current(planning_context(), || {
            let identity = lease_identity(&root).unwrap();
            assert_eq!(identity.canonical_root, root);
            assert!(identity.canonical_git_dir.is_none());
            assert!(identity.canonical_common_git_dir.is_none());
            drop(
                NativeResourceLease::acquire(
                    &root,
                    &[(resource, LockMode::Shared)],
                    Duration::ZERO,
                    None,
                    "planning/read",
                )
                .unwrap(),
            );
            drop(
                WorkspaceWriterLease::acquire(&root, Duration::ZERO, None, "create plan").unwrap(),
            );
            drop(
                WorkspaceWriterLease::acquire_scoped(
                    &root,
                    Duration::ZERO,
                    None,
                    "create plan",
                    "planning/write",
                )
                .unwrap(),
            );
        });
        assert_eq!(GIT_DISCOVERY_CALLS.with(|calls| calls.get()), 0);
    });
}

#[test]
fn planning_and_git_identities_conflict_on_exact_files_in_both_directions() {
    let temp = TestDir::new();
    let root = temp.path().canonicalize().unwrap();
    let Ok(mut git) = crate::core::gitx::command() else {
        eprintln!("skipping Git-identity planning contention: Git unavailable");
        return;
    };
    let init = git
        .current_dir(&root)
        .args(["init", "--quiet"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let normal = WorkspaceIdentity::discover_cached(&root).unwrap();
    assert!(normal.canonical_git_dir.is_some());
    let ctx = planning_context();
    let planning = with_current(ctx.clone(), || lease_identity(&root).unwrap());
    assert_ne!(normal.repo_id, planning.repo_id);
    assert_ne!(normal.worktree_id, planning.worktree_id);
    let a = NativeResource::resolve(&root.join("a.md")).unwrap();
    let b = NativeResource::resolve(&root.join("b.md")).unwrap();
    with_test_lock_home(&root, || {
        let normal_writer = NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "normal",
        )
        .unwrap();
        with_current(ctx.clone(), || {
            let conflict = NativeResourceLease::acquire(
                &root,
                &[(a.clone(), LockMode::Shared)],
                Duration::ZERO,
                None,
                "planning",
            )
            .unwrap_err();
            assert!(
                format!("{conflict:#}").contains("native exact-file resource"),
                "{conflict:#}"
            );
            drop(
                NativeResourceLease::acquire(
                    &root,
                    &[(b.clone(), LockMode::Exclusive)],
                    Duration::ZERO,
                    None,
                    "planning",
                )
                .unwrap(),
            );
        });
        drop(normal_writer);
        let planning_writer = with_current(ctx, || {
            NativeResourceLease::acquire(
                &root,
                &[(a.clone(), LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "planning",
            )
            .unwrap()
        });
        let conflict = NativeResourceLease::acquire(
            &root,
            &[(a.clone(), LockMode::Exclusive)],
            Duration::ZERO,
            None,
            "normal",
        )
        .unwrap_err();
        assert!(
            format!("{conflict:#}").contains("native exact-file resource"),
            "{conflict:#}"
        );
        drop(
            NativeResourceLease::acquire(
                &root,
                &[(b, LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "normal",
            )
            .unwrap(),
        );
        drop(planning_writer);
        drop(
            NativeResourceLease::acquire(
                &root,
                &[(a, LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "normal",
            )
            .unwrap(),
        );
    });
}

#[test]
fn planning_synthetic_descendants_cannot_borrow_physical_body_admission() {
    let temp = TestDir::new();
    let root = temp.path().canonicalize().unwrap();
    let resource = NativeResource::resolve(&root.join("plan.md")).unwrap();
    let ctx = planning_context();
    with_test_lock_home(&root, || {
        let dispatch = WorkspaceWriterLease::acquire_scoped(
            &root,
            Duration::ZERO,
            None,
            "normal parent",
            "parent",
        )
        .unwrap();
        let opaque =
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "opaque body", "parent/opaque")
                .unwrap();
        let worker = opaque.clone();
        drop(opaque);
        with_current(ctx.clone(), || {
            // Synthetic workspace admission may share the global ancestor gate, but cannot
            // authorize any second body while an opaque worker retains its physical gate.
            let synthetic = WorkspaceWriterLease::acquire_scoped(
                &root,
                Duration::ZERO,
                None,
                "planning child",
                "parent/planning",
            )
            .unwrap();
            for scope in ["parent", "parent/planning", "parent/opaque/child"] {
                let conflict = NativeResourceLease::acquire(
                    &root,
                    &[(resource.clone(), LockMode::Shared)],
                    Duration::ZERO,
                    None,
                    scope,
                )
                .unwrap_err();
                assert!(
                    format!("{conflict:#}").contains("physical effects gate"),
                    "{conflict:#}"
                );
                assert!(
                    PhysicalEffectLease::exclusive(Duration::ZERO, None, "new body", scope)
                        .is_err()
                );
            }
            drop(synthetic);
        });
        drop(worker);
        let native = with_current(ctx, || {
            NativeResourceLease::acquire(
                &root,
                &[(resource, LockMode::Exclusive)],
                Duration::ZERO,
                None,
                "parent/planning",
            )
            .unwrap()
        });
        assert!(PhysicalEffectLease::exclusive(
            Duration::ZERO,
            None,
            "opaque sibling",
            "parent/opaque"
        )
        .is_err());
        assert!(WorkspaceWriterLease::acquire_scoped(
            &root,
            Duration::ZERO,
            None,
            "unrelated",
            "sibling",
        )
        .is_err());
        drop(dispatch);
        assert!(
            PhysicalEffectLease::exclusive(Duration::ZERO, None, "after observer", "parent")
                .is_err()
        );
        drop(native);
        drop(
            WorkspaceWriterLease::acquire_scoped(&root, Duration::ZERO, None, "settled", "sibling")
                .unwrap(),
        );
        drop(PhysicalEffectLease::exclusive(Duration::ZERO, None, "settled", "sibling").unwrap());
    });
}
