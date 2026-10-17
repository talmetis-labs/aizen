// Real FileRead/FileEdit/FileWrite preparation and bodies under controller-created typed lanes.
// No provider, scheduler override, external process, live workspace, or engine-policy bypass.

use super::*;
use crate::agent::tools::{PreparedToolCall, Tool, ToolOutcome};
use crate::core::exec_ctx::{self, ExecutionContext};
use crate::core::file_placement::{tests::TestDir, PlacementContext};
use crate::core::native_scope::NativeWriteScope;
use crate::core::persist::FileFingerprint;
use crate::core::speculation::store::BaseSnapshot;
use crate::core::workspace_txn::{
    stable_key, with_test_lock_home, PhysicalEffectLease, WorkspaceIdentity, WorkspaceWriterLease,
};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const BASE: &str = "pub fn owned() -> i32 {\n    101\n}\n";
const OWN_A: &str = "pub fn owned() -> i32 {\n    111\n}\n";
const OWN_B: &str = "pub fn owned() -> i32 {\n    222\n}\n";
const LEDGER_SENTINEL: &[u8] = b"a prior live observation, deliberately unlike the captured base";

struct Fixture {
    _temp: TestDir,
    root: PathBuf,
    home: PathBuf,
    store: PathBuf,
    identity: WorkspaceIdentity,
    parent: ExecutionContext,
}

impl Fixture {
    fn new() -> Self {
        let temp = TestDir::new();
        let root = temp.path().join("workspace");
        let home = temp.path().join("home");
        let store = temp.path().join("store");
        for path in [&root, &home, &store] {
            fs::create_dir(path).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let identity = WorkspaceIdentity {
            repo_id: stable_key(&format!("native-tool-repo:{}", root.display())),
            worktree_id: stable_key(&format!("native-tool-worktree:{}", root.display())),
            canonical_root: root.clone(),
            canonical_git_dir: None,
            canonical_common_git_dir: None,
        };
        let placement = PlacementContext::with_paths(
            root.clone(), home.clone(), temp.path().join("process"), "native-overlay-test",
        )
        .unwrap();
        let parent = ExecutionContext::default()
            .with_resource_scope("parent")
            .with_placement(placement);
        fs::write(root.join("owned.rs"), BASE).unwrap();
        fs::write(root.join("read-only.txt"), "read-only captured bytes").unwrap();
        fs::write(root.join("outside.txt"), "PRIVATE_OUTSIDE_SCOPE").unwrap();
        fs::write(root.join("uncaptured.txt"), "PRIVATE_NOT_CAPTURED").unwrap();
        for name in ["owned.rs", "read-only.txt", "outside.txt", "uncaptured.txt", "created.rs"] {
            crate::core::read_ledger::note(&root.join(name), &FileFingerprint::for_bytes(LEDGER_SENTINEL));
        }
        Self { _temp: temp, root, home, store, identity, parent }
    }

    fn scope(&self, reads: &[&str], writes: &[&str]) -> NativeWriteScope {
        NativeWriteScope::resolve_access(
            &self.root,
            &reads.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>(),
            &writes.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn standard_scope(&self) -> NativeWriteScope {
        self.scope(&["read-only.txt"], &["owned.rs", "created.rs"])
    }

    fn in_context<T>(&self, ctx: &ExecutionContext, f: impl FnOnce() -> T) -> T {
        with_test_lock_home(&self.home, || {
            crate::core::native_journal::with_test_storage(&self.home, || {
                exec_ctx::with_current(ctx.clone(), f)
            })
        })
    }

    fn capture(&self, scopes: &[NativeWriteScope]) -> BaseSnapshot {
        self.in_context(&self.parent, || {
            let writer = WorkspaceWriterLease::acquire_identity_scoped(
                self.identity.clone(), Duration::ZERO, None, "native overlay fixture capture", "parent",
            )
            .unwrap();
            let physical = PhysicalEffectLease::exclusive(
                Duration::ZERO, None, "native overlay fixture capture", "parent",
            )
            .unwrap();
            BaseSnapshot::capture(&self.identity, scopes, &self.store, &writer, &physical).unwrap()
        })
    }

    fn lane(&self, base: &BaseSnapshot, scope: &NativeWriteScope, owner: &str) -> ExecutionContext {
        let overlay = base.create_overlay(owner, scope).unwrap();
        self.parent
            .with_resource_scope(owner)
            .without_workspace_restore()
            .with_placement(self.parent.placement().unwrap().fork_for(self.root.clone(), owner).unwrap())
            .with_native_scope(scope.clone())
            .unwrap()
            .with_overlay(overlay)
            .unwrap()
    }

    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        super::super::file_registry_for_test(&self.root).get_arc(name).unwrap()
    }

    fn prepare(&self, ctx: &ExecutionContext, name: &str, args: Value) -> PreparedToolCall {
        self.in_context(ctx, || self.tool(name).prepare_call(&args, ctx)).unwrap()
    }

    fn execute(&self, ctx: &ExecutionContext, name: &str, call: &PreparedToolCall) -> Result<String> {
        self.in_context(ctx, || self.tool(name).execute_prepared(call).map(|result| result.text))
    }

    fn invoke(&self, ctx: &ExecutionContext, name: &str, args: Value) -> String {
        let call = self.prepare(ctx, name, args);
        self.execute(ctx, name, &call).unwrap()
    }

    fn read(&self, ctx: &ExecutionContext, name: &str) -> String {
        self.invoke(ctx, "file_read", json!({"path":name}))
    }

    fn edit(&self, ctx: &ExecutionContext, old: &str, new: &str) -> String {
        self.invoke(ctx, "file_edit", json!({"path":"owned.rs","old_string":old,"new_string":new}))
    }

    fn outcome(&self, ctx: &ExecutionContext, name: &str, call: &PreparedToolCall) -> ToolOutcome {
        self.in_context(ctx, || {
            crate::agent::run_prepared_tool_body(
                self.tool(name), call, true,
                crate::agent::ResultBudgets { default: 16_384, fetch: 16_384, log: 16_384, delegate: 16_384, spill_over: 0 },
                None,
            )
        })
    }

    fn assert_live_and_ledger_unchanged(&self) {
        assert_eq!(fs::read(self.root.join("owned.rs")).unwrap(), BASE.as_bytes());
        assert_eq!(fs::read_to_string(self.root.join("read-only.txt")).unwrap(), "read-only captured bytes");
        assert_eq!(fs::read_to_string(self.root.join("outside.txt")).unwrap(), "PRIVATE_OUTSIDE_SCOPE");
        assert_eq!(fs::read_to_string(self.root.join("uncaptured.txt")).unwrap(), "PRIVATE_NOT_CAPTURED");
        assert!(!self.root.join("created.rs").exists());
        for name in ["owned.rs", "read-only.txt", "outside.txt", "uncaptured.txt", "created.rs"] {
            assert_eq!(
                crate::core::read_ledger::observed(&self.root.join(name)),
                Some(FileFingerprint::for_bytes(LEDGER_SENTINEL)),
                "private preparation/read/write must not borrow or refresh live read_ledger: {name}"
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this exclusively-created fixture's ledger entries, never a global clear.
        for name in ["owned.rs", "read-only.txt", "outside.txt", "uncaptured.txt", "created.rs"] {
            crate::core::read_ledger::forget(&self.root.join(name));
        }
    }
}

#[test]
fn prepared_worker_digest_binds_frozen_operation_not_wire_args_or_preview() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let lane = f.lane(&base, &scope, "parent/digest/a");
        let args = json!({"path":"owned.rs","old_string":"101","new_string":"111"});
        let prepared = f.prepare(&lane, "file_edit", args.clone());
        let descriptor = prepared_native_worker(&prepared).unwrap().unwrap();
        descriptor.check_attempt(lane.overlay().unwrap(), &scope).unwrap();
        assert_eq!(descriptor.kind(), NativeWorkerKind::Edit);
        let same = f.prepare(&lane, "file_edit", args);
        assert_eq!(descriptor.digest(), prepared_native_worker(&same).unwrap().unwrap().digest());
        let mut retargeted = prepared.clone();
        retargeted.args = json!({"path":"outside.txt","new_string":"forged"});
        retargeted.preview = None;
        retargeted.preview_lines = vec!["forged preview".into()];
        assert_eq!(descriptor.digest(), prepared_native_worker(&retargeted).unwrap().unwrap().digest());
        for (name, args) in [
            ("file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"222"})),
            ("file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111","dry_run":true})),
            ("file_write", json!({"path":"owned.rs","content":OWN_A})),
        ] {
            let other = f.prepare(&lane, name, args);
            assert_ne!(descriptor.digest(), prepared_native_worker(&other).unwrap().unwrap().digest());
        }
        let sibling = f.lane(&base, &scope, "parent/digest/b");
        let sibling_call = f.prepare(&sibling, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111"}));
        assert_ne!(descriptor.digest(), prepared_native_worker(&sibling_call).unwrap().unwrap().digest());
        assert!(descriptor.check_attempt(sibling.overlay().unwrap(), &scope).is_err());
        let narrow = f.scope(&[], &["owned.rs"]);
        assert!(descriptor.check_attempt(lane.overlay().unwrap(), &narrow).is_err());
        f.execute(&lane, "file_edit", &prepared).unwrap();
        assert!(descriptor.check_attempt(lane.overlay().unwrap(), &scope).is_err());
        // The old identity remains historical; it is not re-derived from the changed live overlay.
        assert_eq!(descriptor.digest(), prepared_native_worker(&prepared).unwrap().unwrap().digest());
    });
}

#[test]
fn prepared_worker_digest_binds_read_ranges_options_order_and_missing_image() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let lane = f.lane(&base, &scope, "parent/digest/read");
        let digest = |args| {
            prepared_native_worker(&f.prepare(&lane, "file_read", args)).unwrap().unwrap().digest()
        };
        let original = digest(json!({"path":"owned.rs"}));
        for args in [
            json!({"path":"owned.rs","start":1}),
            json!({"path":"owned.rs","end":2}),
            json!({"path":"owned.rs","number":true}),
            json!({"files":[{"path":"owned.rs"}]}),
            json!({"path":"created.rs"}),
        ] { assert_ne!(original, digest(args)); }
        assert_ne!(
            digest(json!({"files":[{"path":"owned.rs"},{"path":"read-only.txt"}]})),
            digest(json!({"files":[{"path":"read-only.txt"},{"path":"owned.rs"}]})),
        );
        let mut hidden = vec![json!({"path":"owned.rs"}); super::super::MULTI_READ_MAX_FILES];
        let full = digest(json!({"files":hidden}));
        hidden.push(json!({"path":"read-only.txt"}));
        assert_ne!(full, digest(json!({"files":hidden})));
    });
}

#[test]
fn prepared_worker_digest_refuses_missing_or_substituted_typed_footprint() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let lane = f.lane(&base, &scope, "parent/digest/footprint");
        let prepared = f.prepare(&lane, "file_read", json!({"path":"owned.rs"}));
        let mut missing = prepared.clone();
        missing.delegation = None;
        assert!(prepared_native_worker(&missing).is_err());
        let mut altered = prepared.clone();
        altered.footprint = None;
        assert!(prepared_native_worker(&altered).is_err());
        altered.footprint = Some(overlay_footprint(lane.overlay().unwrap(), [(
            lane.overlay().unwrap().resolve(&f.root, Path::new("read-only.txt")).unwrap(), LockMode::Shared,
        )]));
        assert!(prepared_native_worker(&altered).is_err());
        altered.footprint = Some(overlay_footprint(lane.overlay().unwrap(), [(
            lane.overlay().unwrap().resolve(&f.root, Path::new("owned.rs")).unwrap(), LockMode::Exclusive,
        )]));
        assert!(prepared_native_worker(&altered).is_err());
        assert!(prepared_native_worker(&PreparedToolCall::new(json!({}))).unwrap().is_none());
        // A genuine Read cannot be substituted into the registered Edit executor either.
        let outcome = f.outcome(&lane, "file_edit", &prepared);
        assert!(outcome.failure.as_deref().unwrap().contains("another tool"));
    });
}

fn with_fixture(f: impl FnOnce(&Fixture)) {
    let _home_lock = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", fixture.home.as_os_str()),
        ("AIZEN_PROJECT_ROOT", fixture.root.as_os_str()),
    ]);
    let _hooks = crate::core::config::EnvGuard::unset(["AIZEN_NO_HOOKS"]);
    assert!(crate::agent::hooks::configured().is_none(), "isolated fixture has no inherited hooks");
    f(&fixture);
    fixture.assert_live_and_ledger_unchanged();
}

fn rejected(result: Result<String>, needle: &str) {
    let error = result.expect_err("frozen native authority must refuse this operation");
    let text = format!("{error:#}");
    assert!(text.contains(needle), "expected {needle:?}, got {text}");
    assert!(!text.contains("PRIVATE_OUTSIDE_SCOPE") && !text.contains("PRIVATE_NOT_CAPTURED"));
}

#[test]
fn overlay_read_validates_hidden_batch_entries_before_any_private_observation() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let outside = f.scope(&["outside.txt"], &[]);
        let base = f.capture(&[scope.clone(), outside]);
        for (index, (hidden, expected)) in [
            (json!({"path":"outside.txt"}), "outside child scope"),
            (json!({"path":"uncaptured.txt"}), "outside captured exact-file set"),
            (json!({"start":1}), "requires an exact path"),
        ]
        .into_iter()
        .enumerate()
        {
            let lane = f.lane(&base, &scope, &format!("parent/hidden/{index}"));
            let mut files = vec![json!({"path":"owned.rs"}); super::super::MULTI_READ_MAX_FILES];
            files.push(hidden);
            let result = f.in_context(&lane, || f.tool("file_read").prepare_call(&json!({"files":files}), &lane));
            let error = result.expect_err("even an omitted ninth declaration must be validated");
            assert!(error.to_string().contains(expected), "{error:#}");
            let overlay = lane.overlay().unwrap();
            let sealed = overlay.seal(overlay.owner()).unwrap();
            assert!(sealed.observations().unwrap().is_empty(), "no early allowed entry may have been snapshotted");
            assert!(sealed.changes().unwrap().is_empty());
        }
    });
}

#[test]
fn overlay_file_write_creates_private_missing_file_then_real_file_read_sees_it() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.lane(&base, &scope, "parent/create/a");
        let b = f.lane(&base, &scope, "parent/create/b");
        let missing = f.prepare(&a, "file_read", json!({"path":"created.rs"}));
        rejected(f.execute(&a, "file_read", &missing), "private overlay target is missing");
        let created = "pub fn created() -> i32 { 303 }\n";
        let write = f.prepare(&a, "file_write", json!({"path":"created.rs","purpose":"project","content":created}));
        assert!(write.file.is_none(), "private creation must not prepare a live destination");
        assert!(matches!(write.footprint, Some(PreparedFootprint::Overlay { .. })));
        assert_eq!(f.tool("file_write").workspace_effect_prepared(&write), WorkspaceEffect::PrivateOverlay);
        assert!(f.tool("file_write").workspace_target_prepared(&write).is_none());
        assert!(f.execute(&a, "file_write", &write).unwrap().contains(PRIVATE_OVERLAY_NOTE));
        assert!(!f.root.join("created.rs").exists());
        assert_eq!(f.read(&a, "created.rs"), format!("{created}\n[{PRIVATE_OVERLAY_NOTE}]"));
        let sibling_missing = f.prepare(&b, "file_read", json!({"path":"created.rs"}));
        rejected(f.execute(&b, "file_read", &sibling_missing), "private overlay target is missing");
        rejected(f.execute(&a, "file_write", &write), "changed after preparation");
        let overlay = a.overlay().unwrap();
        let sealed = overlay.seal(overlay.owner()).unwrap();
        let changes = sealed.changes().unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].base_bytes().is_none());
        assert_eq!(changes[0].bytes(), created.as_bytes());
        assert!(sealed.observations().unwrap().iter().any(|observation| observation.generation() == 1));
    });
}

#[test]
fn overlay_native_edit_reads_own_changes_without_leaking_to_sibling_or_read_only_lane() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let reader_scope = f.scope(&["owned.rs"], &[]);
        let base = f.capture(&[scope.clone(), reader_scope.clone()]);
        let a = f.lane(&base, &scope, "parent/isolation/a");
        let b = f.lane(&base, &scope, "parent/isolation/b");
        let reader = f.lane(&base, &reader_scope, "parent/isolation/argus");
        assert!(f.read(&a, "owned.rs").starts_with(BASE));
        assert!(f.edit(&a, "101", "111").contains(PRIVATE_OVERLAY_NOTE));
        assert!(f.read(&a, "owned.rs").starts_with(OWN_A));
        assert!(f.read(&b, "owned.rs").starts_with(BASE));
        assert!(f.edit(&b, "101", "222").contains(PRIVATE_OVERLAY_NOTE));
        assert!(f.read(&b, "owned.rs").starts_with(OWN_B));
        assert!(f.read(&a, "owned.rs").starts_with(OWN_A));
        assert!(f.read(&reader, "owned.rs").starts_with(BASE));
        for (name, args) in [
            ("file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"999"})),
            ("file_write", json!({"path":"owned.rs","purpose":"project","content":"forbidden"})),
        ] {
            let result = f.in_context(&reader, || f.tool(name).prepare_call(&args, &reader));
            assert!(result.unwrap_err().to_string().contains("read-only"));
        }
    });
}

#[test]
fn overlay_generation_rejects_stale_reads_edits_writes_dry_runs_noops_and_replay() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.lane(&base, &scope, "parent/stale/a");
        let read = f.prepare(&a, "file_read", json!({"path":"owned.rs"}));
        let other_read = f.prepare(&a, "file_read", json!({"path":"read-only.txt"}));
        let edit = f.prepare(&a, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111"}));
        let stale_edit = f.prepare(&a, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"222"}));
        let write = f.prepare(&a, "file_write", json!({"path":"owned.rs","content":OWN_B}));
        let dry_run = f.prepare(&a, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"222","dry_run":true}));
        let no_op = f.prepare(&a, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"101"}));
        let no_op_write = f.prepare(&a, "file_write", json!({"path":"owned.rs","content":BASE}));
        assert_eq!(a.overlay().unwrap().generation().unwrap(), 0, "preparation and observations do not mutate private bytes");
        f.execute(&a, "file_edit", &edit).unwrap();
        assert_eq!(a.overlay().unwrap().generation().unwrap(), 1);
        for (name, frozen) in [
            ("file_read", &read), ("file_read", &other_read), ("file_edit", &edit),
            ("file_edit", &stale_edit), ("file_write", &write), ("file_edit", &dry_run),
            ("file_edit", &no_op), ("file_write", &no_op_write),
        ] {
            rejected(f.execute(&a, name, frozen), "changed after preparation");
        }
        assert_eq!(a.overlay().unwrap().generation().unwrap(), 1);
        assert!(f.read(&a, "owned.rs").starts_with(OWN_A));
        f.edit(&a, "111", "222");
        assert!(f.read(&a, "owned.rs").starts_with(OWN_B), "fresh preparation succeeds after a stale refusal");
        assert_eq!(a.overlay().unwrap().generation().unwrap(), 2);
    });
}

#[test]
fn overlay_dry_run_and_noop_do_not_author_private_changes_or_live_evidence() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.lane(&base, &scope, "parent/noop/a");
        for (name, args, prefix) in [
            ("file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111","dry_run":true}), DRY_RUN_PREFIX),
            ("file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"101"}), NOOP_WRITE_PREFIX),
            ("file_write", json!({"path":"owned.rs","content":BASE}), NOOP_WRITE_PREFIX),
        ] {
            let prepared = f.prepare(&a, name, args);
            assert_eq!(f.tool(name).workspace_effect_prepared(&prepared), WorkspaceEffect::None);
            let result = f.execute(&a, name, &prepared).unwrap();
            assert!(result.starts_with(prefix), "{result}");
            assert!(result.contains(PRIVATE_OVERLAY_NOTE));
            assert_eq!(a.overlay().unwrap().generation().unwrap(), 0);
        }
        let overlay = a.overlay().unwrap();
        let sealed = overlay.seal(overlay.owner()).unwrap();
        assert!(sealed.changes().unwrap().is_empty());
        assert!(!sealed.observations().unwrap().is_empty(), "private preparation observations are not live read evidence");
    });
}

#[test]
fn frozen_overlay_tools_refuse_owner_sibling_base_and_live_context_transplants() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.lane(&base, &scope, "parent/transplant/a");
        let sibling = f.lane(&base, &scope, "parent/transplant/b");
        let same_owner_other_overlay = f.lane(&base, &scope, "parent/transplant/a");
        let other_base = f.capture(std::slice::from_ref(&scope));
        assert_ne!(base.id(), other_base.id());
        let foreign_base_same_owner = f.lane(&other_base, &scope, "parent/transplant/a");
        let wrong_owner = a.with_resource_scope("parent/transplant/impostor");
        let direct_scoped = f.parent.with_resource_scope(a.resource_scope()).with_native_scope(scope.clone()).unwrap();
        let calls = [
            ("file_read", f.prepare(&a, "file_read", json!({"path":"owned.rs"}))),
            ("file_edit", f.prepare(&a, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111"}))),
            ("file_write", f.prepare(&a, "file_write", json!({"path":"owned.rs","content":OWN_B}))),
        ];
        for (name, frozen) in &calls {
            for ambient in [&sibling, &same_owner_other_overlay, &foreign_base_same_owner, &f.parent, &direct_scoped] {
                rejected(f.execute(ambient, name, frozen), "cannot cross live/private overlay or owner/base boundaries");
            }
            rejected(f.execute(&wrong_owner, name, frozen), "owner differs");
            assert!(exec_ctx::current().is_none());
            rejected(f.tool(name).execute_prepared(frozen).map(|result| result.text), "cannot cross live/private overlay or owner/base boundaries");
        }
        // Even preparation under a transplanted owner is refused, before it can snapshot bytes.
        let invalid_prepare = f.in_context(&wrong_owner, || f.tool("file_read").prepare_call(&json!({"path":"owned.rs"}), &wrong_owner));
        assert!(invalid_prepare.unwrap_err().to_string().contains("owner/scope"));
        assert_eq!(a.overlay().unwrap().generation().unwrap(), 0);
        assert_eq!(sibling.overlay().unwrap().generation().unwrap(), 0);
        assert_eq!(foreign_base_same_owner.overlay().unwrap().generation().unwrap(), 0);
        assert!(f.read(&a, "owned.rs").starts_with(BASE));
    });
}

#[test]
fn real_executor_refuses_direct_private_prepared_substitution_and_unsupported_file_move() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.lane(&base, &scope, "parent/executor/a");
        let direct_scoped = f.parent.with_resource_scope("parent/direct").with_native_scope(scope).unwrap();
        for (name, args) in [
            ("file_read", json!({"path":"owned.rs"})),
            ("file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111"})),
            ("file_write", json!({"path":"owned.rs","content":OWN_B})),
        ] {
            let private = f.prepare(&a, name, args.clone());
            for direct in [&f.parent, &direct_scoped] {
                let live = f.prepare(direct, name, args.clone());
                let outcome = f.outcome(&a, name, &live);
                assert_eq!(outcome.effect_state, crate::agent::tools::ToolEffectState::Settled);
                assert!(outcome.failure.as_deref().unwrap().contains("sealed private resources; no live fallback"));
                let outcome = f.outcome(direct, name, &private);
                assert_eq!(outcome.effect_state, crate::agent::tools::ToolEffectState::Settled);
                assert!(outcome.failure.is_some(), "private bytes cannot escape into a direct execution lane");
                assert!(!outcome.observation.text.contains(OWN_A) && !outcome.observation.text.contains(OWN_B));
            }
        }
        // An actual registered non-allowlisted native tool, not an invented scheduler/test tool.
        // Generic preparation is enough: the executor must reject the tool BEFORE its body can
        // resolve source/destination, touch journal state, or mutate any live file.
        let file_move = PreparedToolCall::new(json!({"from":"owned.rs","to":"created.rs","purpose":"project"}));
        let outcome = f.outcome(&a, "file_move", &file_move);
        assert_eq!(outcome.effect_state, crate::agent::tools::ToolEffectState::Settled);
        assert!(outcome.failure.as_deref().unwrap().contains("outside its approved file-only allowlist"));
        assert_eq!(a.overlay().unwrap().generation().unwrap(), 0);
    });
}

#[test]
fn frozen_overlay_mutations_ignore_retargeted_wire_arguments_and_keep_sealed_scope() {
    with_fixture(|f| {
        let scope = f.standard_scope();
        let base = f.capture(std::slice::from_ref(&scope));
        let a = f.lane(&base, &scope, "parent/frozen-args/a");
        let mut edit = f.prepare(&a, "file_edit", json!({"path":"owned.rs","old_string":"101","new_string":"111"}));
        edit.args = json!({"path":"outside.txt","old_string":"PRIVATE_OUTSIDE_SCOPE","new_string":"forged"});
        let outcome = f.outcome(&a, "file_edit", &edit);
        assert!(outcome.failure.is_none(), "{}", outcome.observation.text);
        assert!(f.read(&a, "owned.rs").starts_with(OWN_A));
        let mut write = f.prepare(&a, "file_write", json!({"path":"created.rs","purpose":"project","content":"owned private creation\n"}));
        write.args = json!({"path":"uncaptured.txt","purpose":"project","content":"forged"});
        let outcome = f.outcome(&a, "file_write", &write);
        assert!(outcome.failure.is_none(), "{}", outcome.observation.text);
        assert!(f.read(&a, "created.rs").starts_with("owned private creation\n"));
        assert!(!f.root.join("created.rs").exists());
    });
}
