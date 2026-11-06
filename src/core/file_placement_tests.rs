use super::*;
use crate::core::exec_ctx::ExecutionContext;

/// A unique fixture-owned directory; no environment override, live home, or added test dependency.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonical test temp root");
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = base.join(format!(
            "aizen-placement-test-{nanos}-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        // Exclusive creation: a preexisting directory is never reused or later cleaned by us.
        fs::create_dir(&path).expect("create unique placement fixture");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // This removes only this helper's exclusively-created fixture, never a sweep namespace.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (TestDir, PlacementContext) {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let ctx = PlacementContext::with_paths(
        workspace,
        temp.path().join("home"),
        temp.path().join("process"),
        "run-fixture",
    )
    .unwrap();
    (temp, ctx)
}

#[test]
fn optional_purpose_is_typed_and_invalid_values_fail() {
    assert_eq!(FilePurpose::parse_optional(None).unwrap(), None);
    for (name, purpose) in [
        ("project", FilePurpose::Project),
        ("artifact", FilePurpose::Artifact),
        ("scratch", FilePurpose::Scratch),
    ] {
        assert_eq!(
            FilePurpose::parse_optional(Some(&Value::String(name.into()))).unwrap(),
            Some(purpose)
        );
    }
    for value in [
        Value::Null,
        Value::Bool(true),
        Value::from(4),
        Value::String("Project".into()),
        Value::String("temp".into()),
    ] {
        assert!(FilePurpose::parse_optional(Some(&value)).is_err());
    }
}

#[test]
fn planning_nested_unicode_names_leaves_managed_roots_absent() {
    let (_temp, ctx) = fixture();
    for purpose in [FilePurpose::Artifact, FilePurpose::Scratch] {
        let destination =
            prepare_destination(Some(&ctx), "báo cáo/結果.txt", Some(purpose)).unwrap();
        assert!(destination.is_managed());
        assert!(!destination.exists_at_preparation());
        assert_eq!(destination.fingerprint(), Some(&FileFingerprint::missing()));
        destination.verify().unwrap();
    }
    assert!(!ctx.home_root().exists());
    assert!(!ctx.scratch_process.exists());
}

#[test]
fn unsafe_managed_names_are_rejected_on_every_host() {
    for name in [
        "",
        ".",
        "..",
        "./a",
        "../a",
        "a/../b",
        "a\\..\\b",
        "/abs",
        "\\abs",
        "C:relative",
        "C:\\absolute",
        "//server/share",
        "\\\\?\\C:\\x",
        "a//b",
        "a\\\\b",
        "a/",
        "a:",
        "a:stream",
        "a.",
        "a ",
        "NUL",
        "nul.txt",
        "CON",
        "CONIN$",
        "COM1.txt",
        "LPT9",
        "COM¹",
        "a?b",
        "a\0b",
        ".ownership.lock",
        ".aizen-scratch-owner",
    ] {
        assert!(validate_managed_name(name).is_err(), "accepted {name:?}");
    }
    for name in [
        "nested/data.txt",
        "nested\\data.txt",
        "báo cáo/結果.txt",
        "normal-name",
        "COM10.txt",
    ] {
        assert!(validate_managed_name(name).is_ok(), "rejected {name:?}");
    }
}

#[test]
fn project_keeps_explicit_absolute_and_parent_path_meaning() {
    let (temp, ctx) = fixture();
    let raw = "../explicit.txt";
    let destination = prepare_destination(Some(&ctx), raw, Some(FilePurpose::Project)).unwrap();
    assert_eq!(destination.actual_path(), ctx.workspace_root().join(raw));
    assert!(!destination.is_managed());
    let absolute = temp.path().join("absolute.txt");
    assert_eq!(
        prepare_destination(None, absolute.to_str().unwrap(), Some(FilePurpose::Project))
            .unwrap()
            .actual,
        absolute
    );
    assert!(prepare_destination(None, "relative.txt", Some(FilePurpose::Project)).is_err());
    for purpose in [FilePurpose::Artifact, FilePurpose::Scratch] {
        assert!(prepare_destination(None, "relative.txt", Some(purpose)).is_err());
        assert!(
            prepare_destination(Some(&ctx), absolute.to_str().unwrap(), Some(purpose)).is_err()
        );
    }
}

#[test]
fn workspace_and_child_scopes_are_stable_and_isolated() {
    let (temp, ctx) = fixture();
    let child = ctx
        .fork_for(ctx.workspace.clone(), "conversation/task:7")
        .unwrap();
    let again = ctx
        .fork_for(ctx.workspace.clone(), "conversation/task:7")
        .unwrap();
    let sibling = ctx
        .fork_for(ctx.workspace.clone(), "conversation/task:8")
        .unwrap();
    let other_root = temp.path().join("other");
    fs::create_dir(&other_root).unwrap();
    let other = ctx.fork_for(other_root, "conversation/task:7").unwrap();
    assert_eq!(child.artifact_root(), again.artifact_root());
    assert_eq!(child.scratch_root(), again.scratch_root());
    assert_ne!(ctx.artifact_root(), child.artifact_root());
    assert_ne!(child.artifact_root(), sibling.artifact_root());
    assert_ne!(child.artifact_root(), other.artifact_root());
    assert_ne!(child.scratch_root(), other.scratch_root());
    assert_eq!(ctx.run_id(), other.run_id());
    assert_eq!(
        ctx.artifact_root().parent().unwrap().parent().unwrap(),
        artifact_workspace_root(ctx.workspace_root(), ctx.home_root())
    );
}

#[test]
fn scratch_handles_are_short_stable_and_metadata_bound() {
    let (temp, ctx) = fixture();
    let child = ctx
        .fork_for(ctx.workspace.clone(), "long:child/名前/scope")
        .unwrap();
    let again = ctx
        .fork_for(ctx.workspace.clone(), "long:child/名前/scope")
        .unwrap();
    let sibling = ctx.fork_for(ctx.workspace.clone(), "other").unwrap();
    assert_eq!(child.scratch_root(), again.scratch_root());
    assert_eq!(child.scratch_root().file_name().unwrap(), "agent-1");
    assert_eq!(sibling.scratch_root().file_name().unwrap(), "agent-2");
    assert_eq!(
        child.scratch_root().parent(),
        Some(ctx.scratch_run.as_path())
    );
    assert!(!ctx.scratch_process.exists());
    let destination = prepare_destination(
        Some(&child),
        "canvas/baseline.cjs",
        Some(FilePurpose::Scratch),
    )
    .unwrap();
    destination.ensure().unwrap();
    let metadata = &child.scratch_namespace.metadata;
    assert!(fs::read_to_string(metadata)
        .unwrap()
        .contains("long:child/名前/scope"));
    destination.verify().unwrap();
    fs::write(metadata, b"foreign identity").unwrap();
    assert!(destination.verify().is_err());
    assert!(destination.ensure().is_err());
    assert!(prepare_destination(Some(&child), "other.txt", Some(FilePurpose::Scratch)).is_err());
    assert!(!temp.path().join("other.txt").exists());
}

#[test]
fn scratch_display_is_typed_bounded_and_namespace_owned() {
    let (temp, ctx) = fixture();
    let name = "báo cáo/結果 e\u{301} 👩‍💻.cjs";
    let path = ctx.scratch_root().join(name);
    let display = ctx.scratch_display(&path).unwrap();
    assert_eq!(display.label(), format!("scratch/{name}"));
    assert_eq!(display.actual(), path);
    assert!(
        !ctx.scratch_process.exists(),
        "display preparation creates nothing"
    );
    assert!(ctx
        .scratch_display(&ctx.workspace_root().join(name))
        .is_none());
    assert!(ctx
        .scratch_display(&ctx.scratch_root().join("../escape.txt"))
        .is_none());
    let child = ctx.fork_for(ctx.workspace.clone(), "task:child").unwrap();
    assert_eq!(
        child
            .scratch_display(&child.scratch_root().join(name))
            .unwrap()
            .label(),
        format!("scratch/agent-1/{name}")
    );
    assert!(ctx
        .scratch_display(&child.scratch_root().join(name))
        .is_none());
    assert!(PlacementContext::with_paths(
        ctx.workspace.clone(),
        ctx.home.clone(),
        temp.path().join("other-process"),
        "x".repeat(250)
    )
    .is_err());
    assert!(PlacementContext::with_paths(
        ctx.workspace.clone(),
        ctx.home.clone(),
        temp.path().join("other-process"),
        "x".repeat(249)
    )
    .is_ok());
    ctx.owner.lock().unwrap().namespaces.insert(
        ScratchIdentity::new(ctx.workspace_root(), "injected"),
        "agent-3".into(),
    );
    assert!(
        ctx.fork_for(ctx.workspace.clone(), "another scope")
            .is_err(),
        "forced handle conflict fails closed"
    );
}

#[test]
fn scratch_run_encoding_is_lossless_and_labels_cannot_alias_it() {
    let id = "ffffffffffffffffffffffffffffffff";
    let encoded = scratch_run_component(id);
    assert_eq!(
        u128::from_str_radix(encoded.strip_prefix("r-").unwrap(), 36).unwrap(),
        u128::MAX
    );
    assert_ne!(scratch_run_component(&encoded), encoded);
    assert_ne!(
        scratch_run_component("run"),
        scratch_run_component("label-run")
    );
    assert_eq!(scratch_run_component("run"), "label-run");
}

#[test]
fn foreign_or_missing_scratch_namespace_metadata_never_reuses_a_handle() {
    let (_temp, ctx) = fixture();
    let destination =
        prepare_destination(Some(&ctx), "file.txt", Some(FilePurpose::Scratch)).unwrap();
    destination.ensure().unwrap();
    let foreign = PlacementContext::with_paths(
        ctx.workspace.clone(),
        ctx.home.clone(),
        ctx.scratch_process.clone(),
        ctx.run_id.clone(),
    )
    .unwrap();
    let child = foreign
        .fork_for(ctx.workspace.clone(), "different scope")
        .unwrap();
    let original_child = ctx
        .fork_for(ctx.workspace.clone(), "original scope")
        .unwrap();
    let original =
        prepare_destination(Some(&original_child), "one.txt", Some(FilePurpose::Scratch)).unwrap();
    original.ensure().unwrap();
    assert_eq!(child.scratch_root(), original_child.scratch_root());
    assert!(prepare_destination(Some(&child), "two.txt", Some(FilePurpose::Scratch)).is_err());
    fs::remove_file(&ctx.scratch_namespace.metadata).unwrap();
    assert!(destination.verify().is_err());
    assert!(prepare_destination(Some(&ctx), "two.txt", Some(FilePurpose::Scratch)).is_err());
}

#[test]
fn context_builders_preserve_placement_without_mutating_parent() {
    let (_temp, placement) = fixture();
    let empty = ExecutionContext::default();
    let parent = empty.with_placement(placement.clone());
    let child = parent
        .with_resource_scope("child")
        .with_persona(Some("name".into()))
        .with_trace_visible(false);
    assert!(empty.placement().is_none());
    assert_eq!(
        child.placement().unwrap().artifact_root(),
        placement.artifact_root()
    );
    assert_eq!(parent.resource_scope(), "default");
}

#[test]
fn ensures_create_only_managed_parents_and_stamps_remain_valid() {
    let (_temp, ctx) = fixture();
    let first =
        prepare_destination(Some(&ctx), "nested/one.txt", Some(FilePurpose::Artifact)).unwrap();
    let second =
        prepare_destination(Some(&ctx), "nested/two.txt", Some(FilePurpose::Artifact)).unwrap();
    first.ensure().unwrap();
    first.verify().unwrap();
    second.verify().unwrap();
    second.ensure().unwrap();
    assert!(first.actual.parent().unwrap().is_dir());
    assert!(!first.actual.exists());
    assert!(!second.actual.exists());
    let project =
        prepare_destination(Some(&ctx), "missing/one.txt", Some(FilePurpose::Project)).unwrap();
    project.ensure().unwrap();
    assert!(!ctx.workspace_root().join("missing").exists());
    project.ensure_project_parents().unwrap();
    project.verify().unwrap();
}

#[test]
fn scratch_ensure_holds_one_lifetime_lock_shared_by_children() {
    let (_temp, ctx) = fixture();
    let destination =
        prepare_destination(Some(&ctx), "nested/one.txt", Some(FilePurpose::Scratch)).unwrap();
    assert!(ctx.owner.lock().unwrap().scratch_lock.is_none());
    destination.ensure().unwrap();
    assert!(ctx.owner.lock().unwrap().scratch_lock.is_some());
    assert!(ctx
        .scratch_process
        .join(crate::core::scratch::OWNER_MARKER)
        .is_file());
    let lock_path = ctx.scratch_run.join(".ownership.lock");
    assert!(RepoTxnLock::acquire(&lock_path, Duration::ZERO).is_err());
    let child = ctx.fork_for(ctx.workspace.clone(), "task/child").unwrap();
    let child_dest =
        prepare_destination(Some(&child), "two.txt", Some(FilePurpose::Scratch)).unwrap();
    child_dest.ensure().unwrap();
    destination.verify().unwrap();
    drop(ctx);
    drop(child);
    assert!(RepoTxnLock::acquire(&lock_path, Duration::ZERO).is_err());
    drop(destination);
    drop(child_dest);
    assert!(RepoTxnLock::acquire(&lock_path, Duration::ZERO).is_ok());
}

#[test]
fn preview_detects_creation_disappearance_and_content_drift() {
    let (_temp, ctx) = fixture();
    let missing = prepare_destination(Some(&ctx), "one.txt", Some(FilePurpose::Project)).unwrap();
    fs::write(&missing.actual, "first").unwrap();
    assert!(missing.verify().is_err());
    let existing = prepare_destination(Some(&ctx), "one.txt", None).unwrap();
    assert!(existing.exists_at_preparation());
    assert_eq!(
        existing.fingerprint(),
        Some(&FileFingerprint::for_bytes(b"first"))
    );
    fs::write(&existing.actual, "other").unwrap();
    assert!(existing.verify().is_err());
    let existing = prepare_destination(Some(&ctx), "one.txt", None).unwrap();
    fs::remove_file(&existing.actual).unwrap();
    assert!(existing.verify().is_err());
}

#[test]
fn unexpectedly_created_parent_requires_fresh_preparation() {
    let (_temp, ctx) = fixture();
    let destination =
        prepare_destination(Some(&ctx), "nested/file.txt", Some(FilePurpose::Artifact)).unwrap();
    fs::create_dir(ctx.home_root()).unwrap();
    assert!(destination.verify().is_err());
    assert!(destination.ensure().is_err());
    assert!(!ctx.artifact_root().exists());
}

#[test]
fn unavailable_root_fails_without_fallback_or_creation() {
    let (temp, ctx) = fixture();
    fs::write(ctx.home_root(), "not a directory").unwrap();
    assert!(prepare_destination(Some(&ctx), "one.txt", Some(FilePurpose::Artifact)).is_err());
    assert!(!temp.path().join("one.txt").exists());
    assert!(!ctx.artifact_root().exists());
}

#[test]
fn cancellation_before_ensure_leaves_roots_absent() {
    let (_temp, ctx) = fixture();
    let destination =
        prepare_destination(Some(&ctx), "one.txt", Some(FilePurpose::Scratch)).unwrap();
    let cancel = crate::core::cancel::TurnCancel::new();
    cancel.cancel();
    crate::core::cancel::with_current(cancel, || assert!(destination.ensure().is_err()));
    assert!(!ctx.scratch_process.exists());
}

#[cfg(unix)]
#[test]
fn managed_link_and_ancestor_replacement_are_refused() {
    use std::os::unix::fs::symlink;
    let (temp, ctx) = fixture();
    let destination =
        prepare_destination(Some(&ctx), "nested/file.txt", Some(FilePurpose::Artifact)).unwrap();
    symlink(ctx.workspace_root(), ctx.home_root()).unwrap();
    assert!(destination.verify().is_err());
    assert!(destination.ensure().is_err());
    assert!(prepare_destination(Some(&ctx), "file.txt", Some(FilePurpose::Artifact)).is_err());
    fs::remove_file(ctx.home_root()).unwrap();
    let destination =
        prepare_destination(Some(&ctx), "nested/file.txt", Some(FilePurpose::Artifact)).unwrap();
    destination.ensure().unwrap();
    let nested = destination.actual.parent().unwrap();
    fs::rename(nested, temp.path().join("saved-directory")).unwrap();
    fs::create_dir(nested).unwrap();
    assert!(destination.verify().is_err());
}

#[test]
fn existing_evidence_reads_are_bounded_and_never_create_or_modify_files() {
    let fixture = TestDir::new();
    let path = fixture.path().join("evidence.bin");
    assert!(read_existing_regular(&path, 4).is_err());
    assert!(!path.exists());
    fs::write(&path, b"data").unwrap();
    assert_eq!(read_existing_regular(&path, 4).unwrap(), b"data");
    assert!(read_existing_regular(&path, 3).is_err());
    assert!(read_existing_regular(&path, u64::MAX).is_err());
    assert!(read_existing_regular(fixture.path(), 4096).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"data");
    let empty = fixture.path().join("empty.bin");
    fs::write(&empty, b"").unwrap();
    assert!(read_existing_regular(&empty, 0).unwrap().is_empty());
    let alias = fixture.path().join("alias.bin");
    fs::hard_link(&path, &alias).unwrap();
    assert!(read_existing_regular(&path, 4).is_err());
    assert!(read_existing_regular(&alias, 4).is_err());
    assert!(existing_regular_stamp(&open_existing_read_only(&path).unwrap()).is_err());
    assert_eq!(fs::read(&alias).unwrap(), b"data");
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 3);
}

#[test]
fn existing_evidence_descriptor_cannot_be_blessed_by_pathname_aba() {
    let fixture = TestDir::new();
    let path = fixture.path().join("evidence.bin");
    let original = fixture.path().join("original.bin");
    let substituted = fixture.path().join("substituted.bin");
    fs::write(&path, b"same bytes").unwrap();
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path).unwrap();
    let stamp = NodeStamp::capture(&path, &fs::symlink_metadata(&path).unwrap(), false).unwrap();
    fs::rename(&path, &original).unwrap();
    fs::write(&path, b"same bytes").unwrap();
    let file = open_existing_read_only(&path).unwrap();
    fs::rename(&path, &substituted).unwrap();
    fs::rename(&original, &path).unwrap();
    assert_eq!(
        NodeStamp::capture(&path, &fs::symlink_metadata(&path).unwrap(), false).unwrap(),
        stamp
    );
    assert!(read_existing_regular_descriptor(&resource, &stamp, file, 10).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"same bytes");
    assert_eq!(fs::read(&substituted).unwrap(), b"same bytes");
    assert_eq!(read_existing_regular(&path, 10).unwrap(), b"same bytes");
}

#[test]
fn existing_evidence_descriptor_refuses_growth_and_replaced_ancestors() {
    let fixture = TestDir::new();
    let parent = fixture.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("evidence.bin");
    fs::write(&path, b"data").unwrap();
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path).unwrap();
    let stamp = NodeStamp::capture(&path, &fs::symlink_metadata(&path).unwrap(), false).unwrap();
    let file = open_existing_read_only(&path).unwrap();
    fs::write(&path, b"data grows").unwrap();
    assert!(read_existing_regular_descriptor(&resource, &stamp, file, 32).is_err());
    let stamp = NodeStamp::capture(&path, &fs::symlink_metadata(&path).unwrap(), false).unwrap();
    let saved = fixture.path().join("saved-parent");
    // Windows refuses renaming a parent with an open child handle. Open the original after the
    // rename instead; its exact node stamp is unchanged while the selected ancestry is replaced.
    fs::rename(&parent, &saved).unwrap();
    let file = open_existing_read_only(&saved.join("evidence.bin")).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(&path, b"data grows").unwrap();
    assert!(read_existing_regular_descriptor(&resource, &stamp, file, 32).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"data grows");
    assert_eq!(fs::read(saved.join("evidence.bin")).unwrap(), b"data grows");
}

#[cfg(unix)]
#[test]
fn existing_evidence_opener_refuses_symlinks_and_nonregular_descriptors() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    let fixture = TestDir::new();
    let target = fixture.path().join("evidence.bin");
    let link = fixture.path().join("link.bin");
    fs::write(&target, b"data").unwrap();
    symlink(&target, &link).unwrap();
    assert!(read_existing_regular(&link, 4).is_err());
    assert!(open_existing_read_only(&link).is_err());
    assert!(existing_regular_stamp(&fs::File::open(fixture.path()).unwrap()).is_err());
    let socket = fixture.path().join("socket.bin");
    drop(socket);
    // A Unix socket address is capped near 104 bytes (SUN_LEN), and this fixture's path plus the
    // macOS per-user temp root is longer than that. Bind a short-named socket directly in the temp
    // root instead of relaxing the opener: the file under test only has to be a socket.
    let socket_dir = fs::canonicalize(std::env::temp_dir()).unwrap();
    let socket = socket_dir.join(format!(
        "aizsock-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    ));
    let listener = UnixListener::bind(&socket).unwrap();
    assert!(read_existing_regular(&socket, 4).is_err());
    assert!(open_existing_read_only(&socket).is_err());
    drop(listener);
    let _ = fs::remove_file(&socket);
    assert_eq!(fs::read(target).unwrap(), b"data");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn existing_evidence_opener_keeps_fifo_nonblocking_and_refuses_descriptor() {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    extern "C" {
        fn mkfifo(path: *const std::os::raw::c_char, mode: u32) -> std::os::raw::c_int;
        fn fcntl(fd: std::os::raw::c_int, command: std::os::raw::c_int, ...)
            -> std::os::raw::c_int;
    }
    let fixture = TestDir::new();
    let path = fixture.path().join("evidence.bin");
    fs::write(&path, b"data").unwrap();
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path).unwrap();
    let stamp = NodeStamp::capture(&path, &fs::symlink_metadata(&path).unwrap(), false).unwrap();
    fs::remove_file(&path).unwrap();
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { mkfifo(name.as_ptr(), 0o600) }, 0);
    // Keep both ends open so even a regressed opener cannot strand this test. Assert the actual
    // O_NONBLOCK descriptor flag (F_GETFL = 3 on Linux/Android) before attempting any reads.
    let _keeper = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x800)
        .open(&path)
        .unwrap();
    let file = open_existing_read_only(&path).unwrap();
    let flags = unsafe { fcntl(file.as_raw_fd(), 3) };
    assert!(flags >= 0 && flags & 0x800 != 0);
    assert!(read_existing_regular_descriptor(&resource, &stamp, file, 4).is_err());
    assert!(read_existing_regular(&path, 4).is_err());
    assert!(!fs::symlink_metadata(&path).unwrap().is_file());
}

#[cfg(windows)]
#[test]
fn windows_reparse_flag_is_rejected_without_link_privilege() {
    use std::os::windows::fs::MetadataExt;
    let (_temp, ctx) = fixture();
    // Plain metadata must not be classified as a reparse point. Actual junction/symlink tests need
    // Developer Mode/admin and are intentionally not silently skipped here.
    let md = fs::symlink_metadata(ctx.workspace_root()).unwrap();
    assert_eq!(is_link_or_reparse(&md), md.file_attributes() & 0x400 != 0);
    assert!(!is_link_or_reparse(&md));
}
