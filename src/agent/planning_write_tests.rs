use super::*;
use crate::agent::todo::{CheckResult, Requirement, Status, TaskContext, TaskState, Todo};
use crate::core::file_placement::tests::TestDir;
use crate::core::session_control::{ControlContext, ExecutionMode};

const ID: &str = "p-0123456789abcdef0123456789abcdef";
const RELATIVE: &str = "docs/plans/work.md";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestFault {
    CancelAfterParent,
    RemoveBeforeReadback,
}

thread_local! {
    static FAULT: std::cell::Cell<Option<TestFault>> = const { std::cell::Cell::new(None) };
}

// Private, thread-scoped, one-shot seams: exercise real directory creation/readback without
// replacing native helpers, synthesizing mutation receipts, or changing production behavior.
fn with_fault<T>(fault: TestFault, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<TestFault>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FAULT.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(FAULT.with(|slot| slot.replace(Some(fault))));
    let result = f();
    assert!(
        FAULT.with(|slot| slot.get().is_none()),
        "fault was not reached"
    );
    result
}

fn take_fault(fault: TestFault) -> bool {
    FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    })
}

pub(super) fn after_parent_mutation() {
    if take_fault(TestFault::CancelAfterParent) {
        crate::core::cancel::current()
            .expect("fixture owns the cancellation token")
            .cancel();
    }
}

pub(super) fn before_readback(path: &Path, approved: &[u8]) {
    if take_fault(TestFault::RemoveBeforeReadback) {
        assert_eq!(
            std::fs::read(path).unwrap(),
            approved,
            "real write must finish first"
        );
        // An independent editor removes the postimage before the checked read. PlanAttachment::load
        // itself must fail; the seam does not return a fabricated readback error.
        std::fs::remove_file(path).unwrap();
    }
}

fn document(body: &str) -> String {
    format!("---\naizen_plan: 1\nid: {ID}\n---\n\n<!-- aizen:phase investigate -->\n## Investigate\n{body}\n")
}

struct Fixture {
    _dir: TestDir,
    root: PathBuf,
    task: TaskContext,
    ctx: ExecutionContext,
    control: ControlContext,
}
impl Fixture {
    fn new(parents: bool) -> Self {
        let dir = TestDir::new();
        let root = dir.path().canonicalize().unwrap();
        if parents {
            std::fs::create_dir_all(root.join("docs/plans")).unwrap();
        }
        let task = TaskContext::private();
        let control = task.control();
        control
            .transition(
                ExecutionMode::Planning,
                Some(PlanTarget {
                    root: root.to_string_lossy().into_owned(),
                    path: RELATIVE.into(),
                    id: ID.into(),
                }),
                |_| Ok(()),
            )
            .unwrap();
        let ctx = ExecutionContext::default().with_task(task.clone());
        Self {
            _dir: dir,
            root,
            task,
            ctx,
            control,
        }
    }
    fn prepare(&self, body: &str) -> Result<PreparedToolCall> {
        prepare(
            &self.root,
            &serde_json::json!({"path":RELATIVE,"content":document(body),"purpose":"project"}),
            &self.ctx,
        )
    }
    fn legacy(&self, bytes: &[u8]) -> LegacyMigration {
        crate::core::persist::atomic_write(&self.root.join(RELATIVE), bytes).unwrap();
        LegacyMigration::read(
            &self.root,
            None,
            self.control.snapshot().plan_target.as_ref(),
        )
        .unwrap()
    }
    fn migration_context(&self) -> ExecutionContext {
        self.ctx
            .with_native_scope(
                crate::core::native_scope::NativeWriteScope::resolve(
                    &self.root,
                    &[RELATIVE.into()],
                )
                .unwrap(),
            )
            .unwrap()
    }
    fn implementation_and_delivery_requirements(&self) {
        self.task
            .replace_with_requirements(
                vec![
                    Todo::new("implement", Status::Done),
                    Todo::new("read saved plan", Status::Done),
                ],
                vec![
                    (
                        "@index:0".into(),
                        Requirement::Check {
                            command: "cargo check".into(),
                            root: "workspace".into(),
                            source_identity: None,
                        },
                    ),
                    (
                        "@index:1".into(),
                        Requirement::Observe {
                            target: RELATIVE.into(),
                        },
                    ),
                ],
            )
            .unwrap();
        self.task
            .record_check(
                self.task.evidence_capture().unwrap(),
                "cargo check",
                "workspace",
                "source-before-plan",
                CheckResult::Pass,
            )
            .unwrap();
        assert_eq!(self.task.snapshot().items[0].status, Status::Done);
    }
    fn assert_unresolved_write(&self, call: &PreparedToolCall, before: &TaskState) {
        let after = self.task.snapshot();
        assert!(!proof(call).unwrap().successful_write());
        assert!(after.effect_epoch > before.effect_epoch);
        assert_eq!(after.id, before.id);
        assert_eq!(after.requirements, before.requirements);
        assert_eq!(after.completion_requests, before.completion_requests);
        assert_eq!(after.history, before.history);
        assert_eq!(
            after.plan, before.plan,
            "failed readback cannot attach a plan"
        );
        assert_eq!(after.item_phases, before.item_phases);
        assert_eq!(after.checks, before.checks, "no new Check receipt");
        assert_eq!(
            after.observations, before.observations,
            "no delivery receipt"
        );
        assert_eq!(
            after.reports, before.reports,
            "no successful report receipt"
        );
        assert_eq!(after.items.len(), before.items.len());
        for (row, old) in after.items.iter().zip(&before.items) {
            assert_eq!(row.id, old.id);
            assert_eq!(row.content, old.content);
            assert_eq!(row.status, Status::Pending);
        }
        assert!(
            after.obligations.iter().any(|obligation| {
                obligation.item_id.is_empty()
                    && obligation.root.as_deref() == self.root.to_str()
                    && !obligation.resolved
            }),
            "a partial physical effect must add unresolved workspace debt"
        );
        assert!(self
            .task
            .verification_roots()
            .iter()
            .any(|root| Path::new(root) == self.root));
        assert!(!self.task.is_complete());
        assert!(self.control.snapshot().is_planning());
    }
    // Like the existing native body fixtures, this exercises the actual helper under the frozen
    // context. Executor resource leases/approval are integration coverage, not claimed here.
    fn run(&self, call: &PreparedToolCall) -> Result<String> {
        exec_ctx::with_current(self.ctx.clone(), || {
            execute(call).expect("host prepared plan call")
        })
    }
}

#[test]
fn migration_uses_explicit_workspace_and_refuses_old_generation_before_write() {
    use crate::core::file_placement::PlacementContext;

    let f = Fixture::new(true);
    let migration = f.legacy(b"# Legacy workspace plan\n");
    let original = f.migration_context().with_placement(
        PlacementContext::with_paths(
            f.root.clone(),
            f.root.join("home"),
            f.root.join("scratch"),
            "migration-workspace",
        )
        .unwrap(),
    );
    let other = f.root.join("other");
    std::fs::create_dir(&other).unwrap();
    let moved = original.with_placement(
        original
            .placement()
            .unwrap()
            .fork_for(other, "focus")
            .unwrap(),
    );
    let returned = moved.with_placement(
        moved
            .placement()
            .unwrap()
            .fork_for(f.root.clone(), "return")
            .unwrap(),
    );
    // Human migration prepares outside the future that executes it. Use its explicit carrier,
    // not an unrelated outer poll's workspace token, just as with no ambient context at all.
    let call = exec_ctx::with_current(moved.clone(), || {
        prepare_migration(&migration, &original).unwrap()
    });
    assert_eq!(
        call.workspace_binding.as_ref(),
        original.workspace_binding()
    );
    proof(&call)
        .unwrap()
        .check_prepared(&original, &call)
        .unwrap();
    let before = f.task.snapshot();
    for ctx in [f.migration_context(), moved, returned.clone()] {
        let mut altered = call.clone();
        altered.workspace_binding = ctx.workspace_binding().cloned();
        let error = exec_ctx::with_current(ctx, || execute(&altered).unwrap()).unwrap_err();
        assert!(error.to_string().contains("binding is stale"), "{error:#}");
        assert_eq!(
            std::fs::read(f.root.join(RELATIVE)).unwrap(),
            migration.before
        );
        assert_eq!(f.task.snapshot(), before);
        assert!(!proof(&call).unwrap().state.lock().unwrap().started);
    }
    let fresh = prepare_migration(&migration, &returned).unwrap();
    let storage = f.root.join("journal-home");
    std::fs::create_dir(&storage).unwrap();
    crate::core::native_journal::with_test_storage(&storage, || {
        exec_ctx::with_current(returned, || execute(&fresh).unwrap()).unwrap();
    });
    assert!(proof(&fresh).unwrap().successful_write());
    assert_eq!(
        std::fs::read(f.root.join(RELATIVE)).unwrap(),
        migration.content.as_bytes()
    );
    assert!(f.control.snapshot().is_planning());
}

#[test]
fn legacy_migration_freezes_exact_bytes_and_preserves_prose() {
    let f = Fixture::new(true);
    let before = "\u{feff}---\r\ntitle: old notes\r\n---\r\n# Kế hoạch\r\n\r\n- [x] prose only  \r\n```text\r\nkeep tabs\tand spacing\r\n```";
    let migration = f.legacy(before.as_bytes());
    assert_eq!(migration.before, before.as_bytes());
    assert_eq!(
        migration.target(),
        f.control.snapshot().plan_target.as_ref().unwrap()
    );
    let prefix = format!("---\naizen_plan: 1\nid: {ID}\n---\n\n<!-- aizen:phase legacy -->\n\n");
    assert_eq!(
        migration.content.as_bytes(),
        [prefix.as_bytes(), before.as_bytes()].concat()
    );
    let ctx = f.migration_context();
    let prepared = prepare_migration(&migration, &ctx).unwrap();
    assert!(is_human_migration(&prepared));
    assert_eq!(prepared.args["content"], migration.content);
    assert_eq!(
        prepared.file.as_ref().unwrap().expected,
        Some(FileFingerprint::for_bytes(before.as_bytes()))
    );
    assert!(prepared.preview.as_ref().unwrap().diff.is_some());
    assert_eq!(
        std::fs::read(f.root.join(RELATIVE)).unwrap(),
        before.as_bytes()
    );
    assert!(
        f.task.snapshot().plan.is_none(),
        "selection/preparation is not readback"
    );
}

#[test]
fn ordinary_prepare_and_model_json_cannot_migrate_legacy_or_ambiguous_contracts() {
    let f = Fixture::new(true);
    let migration = f.legacy(b"# Historical prose\n");
    let ctx = f.migration_context();
    let prepared = prepare_migration(&migration, &ctx).unwrap();
    assert!(prepare(&f.root, &prepared.args, &ctx).is_err());
    let mut forged = prepared.args.clone();
    forged["human_migration"] = true.into();
    assert!(prepare(&f.root, &forged, &ctx).is_err());
    assert!(!is_human_migration(&PreparedToolCall::new(forged)));
    assert!(f.prepare("model rewrite").is_err());
    for text in [
        "---\naizen_plan: 2\nid: damaged\n---\n",
        "```text\n<!-- aizen:phase broken -->\n```\n",
        "---\n'id': old-identity\n---\n# Prose\n",
    ] {
        crate::core::persist::atomic_write(&f.root.join(RELATIVE), text.as_bytes()).unwrap();
        assert!(LegacyMigration::read(&f.root, Some(RELATIVE), None).is_err());
        assert!(f
            .prepare("do not repair malformed modern contracts")
            .is_err());
        assert_eq!(
            std::fs::read(f.root.join(RELATIVE)).unwrap(),
            text.as_bytes()
        );
    }
}

#[test]
fn migration_requires_native_write_authority_for_the_selected_file() {
    use crate::core::native_scope::NativeWriteScope;
    let f = Fixture::new(true);
    let migration = f.legacy(b"# Legacy\n");
    assert!(prepare_migration(&migration, &f.ctx).is_err());
    let read_only = f
        .ctx
        .with_native_scope(
            NativeWriteScope::resolve_access(&f.root, &[RELATIVE.into()], &[]).unwrap(),
        )
        .unwrap();
    assert!(prepare_migration(&migration, &read_only).is_err());
    let unrelated = f
        .ctx
        .with_native_scope(
            NativeWriteScope::resolve(&f.root, &["docs/plans/other.md".into()]).unwrap(),
        )
        .unwrap();
    assert!(prepare_migration(&migration, &unrelated).is_err());
    let ctx = f.migration_context();
    let call = prepare_migration(&migration, &ctx).unwrap();
    assert!(proof(&call)
        .unwrap()
        .check_prepared(&unrelated, &call)
        .is_err());
    let normal = ExecutionContext::default()
        .with_task(TaskContext::private())
        .with_native_scope(ctx.native_scope().unwrap().clone())
        .unwrap();
    assert!(prepare_migration(&migration, &normal).is_err());
    assert_eq!(std::fs::read(f.root.join(RELATIVE)).unwrap(), b"# Legacy\n");
}

#[test]
fn migration_requires_live_generation_and_unchanged_selected_identity() {
    let f = Fixture::new(true);
    let migration = f.legacy(b"# Legacy\n");
    let ctx = f.migration_context();
    let prepared = prepare_migration(&migration, &ctx).unwrap();
    f.control
        .transition(
            ExecutionMode::Planning,
            Some(migration.target().clone()),
            |_| Ok(()),
        )
        .unwrap();
    assert!(prepare_migration(&migration, &ctx).is_err());
    assert!(exec_ctx::with_current(ctx.clone(), || execute(&prepared).unwrap()).is_err());
    let fresh = ExecutionContext::default()
        .with_task(f.task.clone())
        .with_native_scope(ctx.native_scope().unwrap().clone())
        .unwrap();
    assert!(prepare_migration(&migration, &fresh).is_ok());
    let mut changed = migration.target().clone();
    changed.id = "p-ffffffffffffffffffffffffffffffff".into();
    f.control
        .transition(ExecutionMode::Planning, Some(changed), |_| Ok(()))
        .unwrap();
    let changed_ctx = ExecutionContext::default()
        .with_task(f.task.clone())
        .with_native_scope(ctx.native_scope().unwrap().clone())
        .unwrap();
    assert!(prepare_migration(&migration, &changed_ctx).is_err());
    assert!(!proof(&prepared).unwrap().successful_write());
    assert_eq!(std::fs::read(f.root.join(RELATIVE)).unwrap(), b"# Legacy\n");
}

#[test]
fn migration_cas_never_rebases_selection_or_prepared_bytes() {
    let f = Fixture::new(true);
    let migration = f.legacy(b"# Before selection\n");
    let ctx = f.migration_context();
    crate::core::persist::atomic_write(&f.root.join(RELATIVE), b"# Editor before prepare\n")
        .unwrap();
    let error = prepare_migration(&migration, &ctx).err().unwrap();
    assert!(error.to_string().contains("changed after selection"));
    let migration = LegacyMigration::read(&f.root, None, Some(migration.target())).unwrap();
    let prepared = prepare_migration(&migration, &ctx).unwrap();
    crate::core::persist::atomic_write(&f.root.join(RELATIVE), b"# Editor during approval\n")
        .unwrap();
    let epoch = f.task.effect_epoch();
    let error = exec_ctx::with_current(ctx, || execute(&prepared).unwrap()).unwrap_err();
    assert!(error.to_string().contains("changed"), "{error:#}");
    assert_eq!(
        std::fs::read(f.root.join(RELATIVE)).unwrap(),
        b"# Editor during approval\n"
    );
    assert!(!proof(&prepared).unwrap().successful_write());
    assert_eq!(f.task.effect_epoch(), epoch);
    assert!(f.task.snapshot().plan.is_none());
    assert!(f.task.snapshot().checks.is_empty());
}

#[test]
fn migration_journals_exact_images_and_readback_without_erasing_implementation_debt() {
    use crate::core::native_journal::{
        inspect_existing_records, with_test_storage, RecordClaim, RecordProofStatus,
    };
    let f = Fixture::new(true);
    let migration = f.legacy(b"# Original prose\r\n- [x] not implementation evidence");
    f.task
        .replace_with_requirements(
            vec![Todo::new("implement", Status::Pending)],
            vec![(
                "@index:0".into(),
                Requirement::Check {
                    command: "cargo check".into(),
                    root: "workspace".into(),
                    source_identity: None,
                },
            )],
        )
        .unwrap();
    f.ctx.note_task_effects_at(Some(&f.root));
    let before = f.task.snapshot();
    assert!(!before.obligations.is_empty());
    let ctx = f.migration_context();
    let owner = ctx.resource_scope();
    let resource = NativeResource::resolve(&f.root.join(RELATIVE)).unwrap();
    let storage = f.root.join("journal-home");
    std::fs::create_dir(&storage).unwrap();
    with_test_storage(&storage, || {
        let call = prepare_migration(&migration, &ctx).unwrap();
        let output = exec_ctx::with_current(ctx, || execute(&call).unwrap()).unwrap();
        assert!(output.contains("implementation checks remain outstanding"));
        assert!(proof(&call).unwrap().successful_write());
        let records = inspect_existing_records(&[RecordClaim {
            id: 1,
            expected_owner: &owner,
            resource: &resource,
            before: &FileFingerprint::for_bytes(&migration.before),
            after: &FileFingerprint::for_bytes(migration.content.as_bytes()),
        }])
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, RecordProofStatus::SettledApplied);
    });
    let after = f.task.snapshot();
    assert_eq!(
        std::fs::read(f.root.join(RELATIVE)).unwrap(),
        migration.content.as_bytes()
    );
    assert_eq!(after.effect_epoch, before.effect_epoch + 1);
    assert_eq!(after.items, before.items);
    assert_eq!(after.requirements, before.requirements);
    assert_eq!(after.obligations, before.obligations);
    assert_eq!(after.checks, before.checks);
    assert!(!f.task.is_complete());
    assert_eq!(
        after.plan.as_ref().unwrap(),
        &super::super::todo::PlanAttachment::load(&f.root, RELATIVE).unwrap()
    );
    assert!(after.plan.as_ref().unwrap().live);
    assert_eq!(
        after.plan.as_ref().unwrap().phases,
        vec!["legacy".to_owned()]
    );
    assert!(f.control.snapshot().is_planning());
}

#[test]
fn selected_plan_write_advances_epoch_without_a_new_check_obligation() {
    let f = Fixture::new(true);
    let start = f.task.effect_epoch();
    let call = f.prepare("new plan").unwrap();
    assert!(f.run(&call).unwrap().starts_with("Plan saved:"));
    assert!(proof(&call).unwrap().successful_write());
    assert_eq!(f.task.effect_epoch(), start + 1);
    let state = f.task.snapshot();
    assert!(state.obligations.is_empty());
    assert!(state.checks.is_empty());
    assert_eq!(state.plan.as_ref().unwrap().id, ID);
    assert_eq!(
        std::fs::read_to_string(f.root.join(RELATIVE)).unwrap(),
        document("new plan")
    );
    assert!(f.run(&call).is_err(), "a prepared invocation is single use");
}

#[test]
fn creates_only_fixed_parents_and_accounts_for_each_physical_effect() {
    let f = Fixture::new(false);
    let call = f.prepare("first plan").unwrap();
    assert!(
        !f.root.join("docs").exists(),
        "preparation must be side-effect-free"
    );
    assert!(matches!(call.footprint, Some(PreparedFootprint::Global)));
    let start = f.task.effect_epoch();
    f.run(&call).unwrap();
    assert_eq!(
        f.task.effect_epoch(),
        start + 3,
        "docs, plans and document each changed physically"
    );
    assert!(f.task.snapshot().obligations.is_empty());
    assert!(proof(&call).unwrap().successful_write());
}

#[test]
fn rejects_wrong_path_id_purpose_and_oversized_or_malformed_bytes() {
    let f = Fixture::new(true);
    for args in [
        serde_json::json!({"path":"docs/plans/other.md","content":document("x")}),
        serde_json::json!({"path":"docs/plans/../work.md","content":document("x")}),
        serde_json::json!({"path":RELATIVE,"content":document("x"),"purpose":"scratch"}),
        serde_json::json!({"path":RELATIVE,"content":document("x"),"waiver":true}),
        serde_json::json!({"path":RELATIVE,"content":"legacy prose"}),
        serde_json::json!({"path":RELATIVE,"content":document("x").replace(ID,"p-ffffffffffffffffffffffffffffffff")}),
        serde_json::json!({"path":RELATIVE,"content":document(&"x".repeat(65_536))}),
    ] {
        assert!(prepare(&f.root, &args, &f.ctx).is_err(), "accepted {args}");
    }
    assert!(!f.root.join(RELATIVE).exists());
}

#[test]
fn rejects_missing_arbitrary_nested_parent() {
    let f = Fixture::new(true);
    f.control
        .transition(
            ExecutionMode::Planning,
            Some(PlanTarget {
                root: f.root.to_string_lossy().into_owned(),
                path: "docs/plans/nested/work.md".into(),
                id: ID.into(),
            }),
            |_| Ok(()),
        )
        .unwrap();
    let ctx = ExecutionContext::default().with_task(f.task.clone());
    assert!(prepare(
        &f.root,
        &serde_json::json!({
            "path":"docs/plans/nested/work.md","content":document("x")
        }),
        &ctx
    )
    .is_err());
    assert!(!f.root.join("docs/plans/nested").exists());
}

#[test]
fn no_op_has_no_mutation_receipt_or_epoch_change() {
    let f = Fixture::new(true);
    let call = f.prepare("same").unwrap();
    f.run(&call).unwrap();
    let epoch = f.task.effect_epoch();
    let call = f.prepare("same").unwrap();
    assert_eq!(effect(&call), Some(WorkspaceEffect::None));
    assert!(f
        .run(&call)
        .unwrap()
        .starts_with(super::super::builtin::NOOP_WRITE_PREFIX));
    assert!(!proof(&call).unwrap().successful_write());
    assert_eq!(f.task.effect_epoch(), epoch);
}

#[test]
fn changed_file_or_first_create_race_yields_no_mutation_receipt() {
    let f = Fixture::new(true);
    f.implementation_and_delivery_requirements();
    let call = f.prepare("approved").unwrap();
    std::fs::write(f.root.join(RELATIVE), document("editor wins")).unwrap();
    let before = f.task.snapshot();
    assert!(f.run(&call).is_err());
    assert!(!proof(&call).unwrap().successful_write());
    assert_eq!(f.task.snapshot(), before, "a failed CAS grants no receipt");
    assert!(f.task.snapshot().obligations.is_empty());
    assert_eq!(
        std::fs::read_to_string(f.root.join(RELATIVE)).unwrap(),
        document("editor wins")
    );
}

#[test]
fn same_id_editor_atomic_save_needs_reprepare_but_is_not_permanently_rejected() {
    let f = Fixture::new(true);
    std::fs::write(f.root.join(RELATIVE), document("before")).unwrap();
    let stale = f.prepare("approved").unwrap();
    crate::core::persist::atomic_write(&f.root.join(RELATIVE), document("editor").as_bytes())
        .unwrap();
    assert!(f.run(&stale).is_err());
    crate::core::read_ledger::note(
        &f.root.join(RELATIVE),
        &FileFingerprint::for_bytes(document("editor").as_bytes()),
    );
    let fresh = f.prepare("approved again").unwrap();
    f.run(&fresh).unwrap();
    assert!(proof(&fresh).unwrap().successful_write());
    std::fs::write(
        f.root.join(RELATIVE),
        document("changed identity").replace(ID, "p-ffffffffffffffffffffffffffffffff"),
    )
    .unwrap();
    assert!(f.prepare("no silent reattach").is_err());
}

#[test]
fn policy_transition_and_prepared_argument_or_resource_tamper_are_rejected() {
    let f = Fixture::new(true);
    let call = f.prepare("approved").unwrap();
    let mut changed = call.clone();
    changed.args["path"] = "src/main.rs".into();
    assert!(proof(&changed)
        .unwrap()
        .check_prepared(&f.ctx, &changed)
        .is_err());
    let mut changed = call.clone();
    changed.args["content"] = document("not approved").into();
    assert!(f.run(&changed).is_err());
    let mut changed = call.clone();
    changed.footprint = Some(PreparedFootprint::Global);
    assert!(proof(&changed)
        .unwrap()
        .check_prepared(&f.ctx, &changed)
        .is_err());
    f.control
        .transition(ExecutionMode::Normal, None, |_| Ok(()))
        .unwrap();
    assert!(f.run(&call).is_err());
    assert!(!f.root.join(RELATIVE).exists());
}

#[test]
fn successful_plan_reopens_existing_implementation_check_without_replacing_rows() {
    let f = Fixture::new(true);
    f.task
        .replace_with_requirements(
            vec![Todo::new("implement", Status::Done)],
            vec![(
                "@index:0".into(),
                Requirement::Check {
                    command: "cargo check".into(),
                    root: "workspace".into(),
                    source_identity: None,
                },
            )],
        )
        .unwrap();
    f.task
        .record_check(
            f.task.evidence_capture().unwrap(),
            "cargo check",
            "workspace",
            "source-v1",
            CheckResult::Pass,
        )
        .unwrap();
    assert!(f.task.is_complete());
    let before = f.task.snapshot();
    let call = f.prepare("planning only").unwrap();
    assert!(f
        .run(&call)
        .unwrap()
        .contains("implementation checks remain outstanding"));
    let after = f.task.snapshot();
    assert_eq!(after.items.len(), 1);
    assert_eq!(after.items[0].id, before.items[0].id);
    assert_eq!(
        after.checks, before.checks,
        "historical evidence is retained, not granted anew"
    );
    assert_eq!(after.obligations.len(), before.obligations.len());
    assert!(!f.task.is_complete());
    assert_eq!(after.items[0].status, Status::Pending);
}

#[test]
fn partial_effect_guard_falls_back_to_unresolved_generic_accounting() {
    let f = Fixture::new(true);
    let call = f.prepare("approved").unwrap();
    let p = proof(&call).unwrap();
    assert!(
        !p.record_mutation(&f.ctx, std::slice::from_ref(&p.target)),
        "unarmed proof is not a waiver"
    );
    let ctx = f.ctx.with_plan_write_proof(p.clone());
    {
        let mut state = p.state.lock().unwrap();
        state.started = true;
        state.active_thread = Some(std::thread::current().id());
    }
    {
        let _guard = OperationGuard {
            proof: p,
            ctx: &ctx,
            settled: false,
        };
        ctx.note_native_mutation(std::slice::from_ref(&p.target));
    }
    assert!(!p.successful_write());
    assert!(!f.task.snapshot().obligations.is_empty());
    assert!(!f.task.verification_roots().is_empty());
}

#[test]
fn cancelled_parent_creation_accounts_for_real_partial_directory_without_receipts() {
    let f = Fixture::new(false);
    f.implementation_and_delivery_requirements();
    let call = f.prepare("interrupted parents").unwrap();
    let before = f.task.snapshot();
    assert!(before.obligations.is_empty());
    let cancel = crate::core::cancel::TurnCancel::new();
    let error = crate::core::cancel::with_current(cancel.clone(), || {
        with_fault(TestFault::CancelAfterParent, || f.run(&call))
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("cancelled while creating"),
        "{error:#}"
    );
    assert!(cancel.is_cancelled());
    assert!(
        f.root.join("docs").is_dir(),
        "actual first parent remains on disk"
    );
    assert!(
        !f.root.join("docs/plans").exists(),
        "second parent was never created"
    );
    assert!(!f.root.join(RELATIVE).exists());
    assert_eq!(
        proof(&call).unwrap().state.lock().unwrap().mutations,
        vec![f.root.join("docs")]
    );
    assert_eq!(
        f.task.effect_epoch(),
        before.effect_epoch + 2,
        "one native effect plus conservative failure accounting"
    );
    f.assert_unresolved_write(&call, &before);
}

#[test]
fn cancellation_before_parent_creation_does_not_add_any_receipts_or_debt() {
    let f = Fixture::new(false);
    f.implementation_and_delivery_requirements();
    let call = f.prepare("not written").unwrap();
    let before = f.task.snapshot();
    let cancel = crate::core::cancel::TurnCancel::new();
    cancel.cancel();
    let error = crate::core::cancel::with_current(cancel, || f.run(&call)).unwrap_err();
    assert!(
        error.to_string().contains("cancelled before creating"),
        "{error:#}"
    );
    assert!(!f.root.join("docs").exists());
    assert!(!proof(&call).unwrap().successful_write());
    assert!(proof(&call)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .mutations
        .is_empty());
    assert_eq!(
        f.task.snapshot(),
        before,
        "no physical effect grants no receipt and adds no debt"
    );
}

#[test]
fn failed_post_write_readback_keeps_requirements_and_cannot_attach_or_report_success() {
    use crate::core::native_journal::with_test_storage;
    use crate::core::native_scope::NativeWriteScope;

    // Exercise both atomic creation and journaled replacement. An existing attachment must not
    // silently become the failed postimage; a fresh document must not acquire one at all.
    for journaled in [false, true] {
        let mut f = Fixture::new(true);
        f.implementation_and_delivery_requirements();
        if journaled {
            std::fs::write(f.root.join(RELATIVE), document("previously attached")).unwrap();
            f.task
                .attach_plan(super::super::todo::PlanAttachment::load(&f.root, RELATIVE).unwrap())
                .unwrap();
            f.ctx = f
                .ctx
                .with_native_scope(NativeWriteScope::resolve(&f.root, &[RELATIVE.into()]).unwrap())
                .unwrap();
        }
        let storage = f.root.join("journal-home");
        std::fs::create_dir(&storage).unwrap();
        with_test_storage(&storage, || {
            let call = f.prepare("written but unreadable").unwrap();
            let before = f.task.snapshot();
            assert!(before.obligations.is_empty());
            let error = with_fault(TestFault::RemoveBeforeReadback, || f.run(&call)).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::NotFound),
                "{error:#}"
            );
            assert!(!error.to_string().contains("Plan saved:"));
            assert!(!f.root.join(RELATIVE).exists());
            assert_eq!(
                proof(&call).unwrap().state.lock().unwrap().mutations,
                vec![f.root.join(RELATIVE)]
            );
            assert_eq!(f.task.effect_epoch(), before.effect_epoch + 2);
            f.assert_unresolved_write(&call, &before);
        });
    }
}

#[test]
fn journal_failure_after_actual_write_remains_unresolved_without_delivery_receipts() {
    use crate::core::native_journal::{
        with_test_after_write_failure, with_test_storage, PossiblyApplied,
    };
    use crate::core::native_scope::NativeWriteScope;

    let mut f = Fixture::new(true);
    f.ctx = f
        .ctx
        .with_native_scope(NativeWriteScope::resolve(&f.root, &[RELATIVE.into()]).unwrap())
        .unwrap();
    f.implementation_and_delivery_requirements();
    let storage = f.root.join("journal-home");
    std::fs::create_dir(&storage).unwrap();
    with_test_storage(&storage, || {
        let call = f.prepare("physical postimage").unwrap();
        let before = f.task.snapshot();
        let error = with_test_after_write_failure(|| f.run(&call)).unwrap_err();
        assert!(
            error.downcast_ref::<PossiblyApplied>().is_some(),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(f.root.join(RELATIVE)).unwrap(),
            document("physical postimage").as_bytes()
        );
        assert_eq!(
            proof(&call).unwrap().state.lock().unwrap().mutations,
            vec![f.root.join(RELATIVE)]
        );
        f.assert_unresolved_write(&call, &before);
    });
}

#[test]
fn hardlinked_existing_plan_is_not_write_authority() {
    let f = Fixture::new(true);
    let path = f.root.join(RELATIVE);
    std::fs::write(&path, document("linked")).unwrap();
    match std::fs::hard_link(&path, f.root.join("alias.md")) {
        Ok(()) => assert!(f.prepare("refuse").is_err()),
        Err(error) => eprintln!("SKIP hardlink fixture: {error}"),
    }
}

#[cfg(windows)]
#[test]
fn windows_reparse_parent_blocks_preparation_and_stale_execution_without_receipts() {
    use std::os::windows::fs::{symlink_dir, MetadataExt};

    for after_prepare in [false, true] {
        let f = Fixture::new(after_prepare);
        f.implementation_and_delivery_requirements();
        let outside = TestDir::new();
        let outside_docs = outside.path().canonicalize().unwrap().join("docs");
        std::fs::create_dir_all(outside_docs.join("plans")).unwrap();
        let outside_plan = outside_docs.join("plans/work.md");
        let original = document("outside workspace; must remain unchanged");
        std::fs::write(&outside_plan, original.as_bytes()).unwrap();
        let call = after_prepare.then(|| f.prepare("must not escape").unwrap());
        if after_prepare {
            std::fs::rename(f.root.join("docs"), f.root.join("original-docs")).unwrap();
        }
        match symlink_dir(&outside_docs, f.root.join("docs")) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) || matches!(error.raw_os_error(), Some(1 | 50 | 1314)) =>
            {
                eprintln!("SKIP Windows reparse plan-path fixture: directory symlinks unavailable (Developer Mode/privilege or filesystem support): {error}");
                return;
            }
            Err(error) => panic!("creating Windows reparse plan-path fixture: {error}"),
        }
        let md = std::fs::symlink_metadata(f.root.join("docs")).unwrap();
        assert_ne!(
            md.file_attributes() & 0x400,
            0,
            "fixture must be an actual reparse point"
        );
        let before = f.task.snapshot();
        if let Some(call) = call {
            let error = f.run(&call).unwrap_err();
            assert!(
                error.to_string().contains("non-link directories"),
                "{error:#}"
            );
            assert!(!proof(&call).unwrap().successful_write());
            assert!(proof(&call)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .mutations
                .is_empty());
            assert!(!f.root.join("original-docs/plans/work.md").exists());
        } else {
            let error = f.prepare("must not escape").err().unwrap();
            assert!(
                error.to_string().contains("non-link directories"),
                "{error:#}"
            );
        }
        assert_eq!(std::fs::read(&outside_plan).unwrap(), original.as_bytes());
        assert_eq!(
            f.task.snapshot(),
            before,
            "rejected reparse traversal grants no receipt"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_parent_is_refused_before_canonicalization() {
    let f = Fixture::new(false);
    std::fs::create_dir_all(f.root.join("outside/plans")).unwrap();
    std::os::unix::fs::symlink(f.root.join("outside"), f.root.join("docs")).unwrap();
    assert!(f.prepare("refuse").is_err());
    assert!(!f.root.join("outside/plans/work.md").exists());
}
