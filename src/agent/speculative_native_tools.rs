//! Typed private-overlay implementations for the native file tools.
//!
//! This module is intentionally a child of `builtin`: it reuses the parent tool's bounded
//! replacement and view helpers, but never resolves a live destination or consults the live tree.

use super::{
    bounded_native_arguments, bounded_write_diff, budget_view, diff_preview, dry_run_arg,
    range_view, FileEdit, FileRead, FileWrite, DRY_RUN_PREFIX, FILE_READ_MAX_BYTES,
    FILE_READ_MAX_LINES, NOOP_WRITE_PREFIX, PREPARED_EDIT_MAX_BYTES,
};
use crate::agent::tools::{PreparedFootprint, PreparedToolCall, WorkspaceEffect};
use crate::core::exec_ctx::ExecutionContext;
use crate::core::native_scope::NativeWriteScope;
use crate::core::repo_lock::LockMode;
use crate::core::speculation::store::{OverlayFile, OverlayHandle, OverlayRead, WriteProvenance};
use crate::core::speculation::workers::{scope_sha256, NativeWorkerKind};
use anyhow::{bail, ensure, Context, Result};
use ring::digest::{Context as DigestContext, SHA256};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

const PRIVATE_OVERLAY_NOTE: &str = "private overlay; not yet integrated";

struct OverlayReadEntry {
    label: String,
    start: Option<u64>,
    end: Option<u64>,
    file: OverlayFile,
    snapshot: OverlayRead,
}

pub(crate) struct FrozenOverlayRead {
    pub(crate) ctx: ExecutionContext,
    pub(crate) handle: OverlayHandle,
    entries: Vec<OverlayReadEntry>,
    pub(crate) number: bool,
    pub(crate) batch: bool,
    pub(crate) omitted: usize,
    pub(crate) generation: u64,
}

pub(crate) struct FrozenOverlayEdit {
    pub(crate) ctx: ExecutionContext,
    pub(crate) handle: OverlayHandle,
    pub(crate) file: OverlayFile,
    pub(crate) snapshot: OverlayRead,
    pub(crate) content: String,
    pub(crate) provenance: WriteProvenance,
    pub(crate) output: String,
    pub(crate) dry_run: bool,
    pub(crate) no_op: bool,
    pub(crate) generation: u64,
}

pub(crate) struct FrozenOverlayWrite {
    pub(crate) ctx: ExecutionContext,
    pub(crate) handle: OverlayHandle,
    pub(crate) file: OverlayFile,
    pub(crate) snapshot: OverlayRead,
    pub(crate) content: String,
    pub(crate) provenance: WriteProvenance,
    pub(crate) output: String,
    pub(crate) no_op: bool,
    pub(crate) generation: u64,
}

fn overlay_parts(ctx: &ExecutionContext) -> Result<(&OverlayHandle, &NativeWriteScope, String)> {
    let handle = ctx
        .overlay()
        .context("private overlay context is missing its handle")?;
    let scope = ctx
        .native_scope()
        .context("private overlay context is missing its sealed native scope")?;
    let owner = ctx.resource_scope();
    handle.check_owner_scope(&owner, scope)?;
    Ok((handle, scope, owner))
}

fn prepare_snapshot(
    handle: &OverlayHandle,
    owner: &str,
    file: &OverlayFile,
    writable: bool,
) -> Result<OverlayRead> {
    handle.validate(file, writable)?;
    let body = handle.body_lease(owner)?;
    handle.snapshot(file, &body)
}

fn snapshot_text(snapshot: &OverlayRead, label: &str) -> Result<String> {
    let bytes = snapshot
        .bytes()
        .context("private overlay target is missing; no live file was read")?;
    ensure!(
        bytes.len() <= PREPARED_EDIT_MAX_BYTES,
        "private overlay file exceeds the 4 MiB limit"
    );
    String::from_utf8(bytes.to_vec()).with_context(|| format!("{label} is not valid UTF-8"))
}

fn overlay_output(output: String) -> String {
    if let Some((first, rest)) = output.split_once('\n') {
        if rest.is_empty() {
            format!("{first}\n{PRIVATE_OVERLAY_NOTE}")
        } else {
            format!("{first}\n{PRIVATE_OVERLAY_NOTE}\n{rest}")
        }
    } else {
        format!("{output} ({PRIVATE_OVERLAY_NOTE})")
    }
}

fn overlay_preview(
    title: String,
    mut lines: Vec<String>,
    diff: Option<String>,
) -> crate::agent::tools::ApprovalPreview {
    lines.push(PRIVATE_OVERLAY_NOTE.to_string());
    crate::agent::tools::ApprovalPreview { title, diff, lines }
}

fn localized_provenance(before: &str, after: &str) -> WriteProvenance {
    let before_bytes = before.as_bytes();
    let after_bytes = after.as_bytes();
    let mut prefix = 0usize;
    while prefix < before_bytes.len()
        && prefix < after_bytes.len()
        && before_bytes[prefix] == after_bytes[prefix]
    {
        prefix += 1;
    }
    while prefix > 0 && (!before.is_char_boundary(prefix) || !after.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = 0usize;
    while suffix < before_bytes.len().saturating_sub(prefix)
        && suffix < after_bytes.len().saturating_sub(prefix)
        && before_bytes[before_bytes.len() - 1 - suffix]
            == after_bytes[after_bytes.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0 {
        let before_start = before_bytes.len() - suffix;
        let after_start = after_bytes.len() - suffix;
        if before.is_char_boundary(before_start) && after.is_char_boundary(after_start) {
            break;
        }
        suffix -= 1;
    }
    suffix = suffix.min(before_bytes.len().saturating_sub(prefix));
    WriteProvenance::Localized {
        start: prefix,
        end: before_bytes.len().saturating_sub(suffix),
    }
}

fn overlay_footprint(
    handle: &OverlayHandle,
    files: impl IntoIterator<Item = (OverlayFile, LockMode)>,
) -> PreparedFootprint {
    PreparedFootprint::Overlay {
        handle: handle.clone(),
        files: files.into_iter().collect(),
    }
}

/// Frozen operation identity for launch evidence. It contains no executable context carrier.
/// The digest is historical evidence, never permission to execute a reconstructed operation.
#[derive(Debug, Clone)]
pub(crate) struct PreparedNativeWorker {
    handle: OverlayHandle,
    scope_sha256: [u8; 32],
    generation: u64,
    kind: NativeWorkerKind,
    prepared_sha256: [u8; 32],
}

impl PreparedNativeWorker {
    pub(crate) fn check_attempt(
        &self,
        overlay: &OverlayHandle,
        scope: &NativeWriteScope,
    ) -> Result<()> {
        ensure!(
            self.handle.same_handle(overlay),
            "prepared worker belongs to another overlay"
        );
        overlay.check_owner_scope(overlay.owner(), scope)?;
        overlay.check_scope(scope)?;
        ensure!(
            self.scope_sha256 == native_scope_sha256(scope)?,
            "prepared worker scope changed"
        );
        ensure!(
            self.generation == overlay.generation()?,
            "prepared worker generation changed before launch"
        );
        Ok(())
    }

    pub(crate) fn check_lifecycle(
        &self,
        lifecycle: &crate::core::speculation::lifecycle::AttemptLifecycle,
    ) -> Result<()> {
        lifecycle.check_overlay(self.handle.base_id(), self.handle.id(), self.handle.owner())?;
        ensure!(
            self.handle
                .lifecycle()
                .is_some_and(|bound| bound.same_attempt(lifecycle)),
            "prepared worker belongs to another live lifecycle"
        );
        self.check_attempt(&self.handle, lifecycle.scope())
    }

    pub(crate) fn kind(&self) -> NativeWorkerKind {
        self.kind
    }
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.prepared_sha256
    }
}

pub(crate) fn native_scope_sha256(scope: &NativeWriteScope) -> Result<[u8; 32]> {
    scope_sha256(scope.resources().iter().map(|resource| {
        (
            resource.key(),
            resource.path(),
            scope.write_keys().iter().any(|key| key == resource.key()),
        )
    }))
}

struct OperationDigest(DigestContext);

impl OperationDigest {
    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.0.update(bytes);
    }

    fn u64(&mut self, value: u64) {
        self.0.update(&value.to_le_bytes());
    }
    fn flag(&mut self, value: bool) {
        self.0.update(&[u8::from(value)]);
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.u64(value);
        }
    }

    fn snapshot(
        &mut self,
        file: &OverlayFile,
        snapshot: &OverlayRead,
        writable: bool,
    ) -> Result<()> {
        self.bytes(file.key().as_bytes());
        self.bytes(
            file.logical_path()
                .to_str()
                .context("prepared worker path is not UTF-8")?
                .as_bytes(),
        );
        self.flag(writable);
        self.u64(snapshot.generation());
        let fingerprint = snapshot.fingerprint();
        self.flag(fingerprint.exists);
        self.u64(fingerprint.byte_len);
        self.bytes(&fingerprint.sha256);
        Ok(())
    }

    fn provenance(&mut self, provenance: WriteProvenance) {
        match provenance {
            WriteProvenance::Rewrite => self.bytes(b"rewrite"),
            WriteProvenance::Localized { start, end } => {
                self.bytes(b"localized");
                self.u64(start as u64);
                self.u64(end as u64);
            }
        }
    }
}

fn begin_worker_digest(
    prepared: &PreparedToolCall,
    ctx: &ExecutionContext,
    handle: &OverlayHandle,
    generation: u64,
    kind: NativeWorkerKind,
    files: &[(&OverlayFile, LockMode)],
) -> Result<(PreparedNativeWorker, OperationDigest)> {
    let scope = ctx
        .native_scope()
        .context("prepared worker lost its exact native scope")?;
    ensure!(
        ctx.overlay()
            .is_some_and(|active| active.same_handle(handle)),
        "prepared worker context changed"
    );
    handle.check_owner_scope(&ctx.resource_scope(), scope)?;
    handle.check_scope(scope)?;
    ensure!(
        prepared.file.is_none(),
        "private worker cannot carry a live file preparation"
    );
    let Some(PreparedFootprint::Overlay {
        handle: footprint,
        files: bound,
    }) = &prepared.footprint
    else {
        bail!("prepared worker requires a typed private footprint");
    };
    ensure!(
        handle.same_handle(footprint) && !files.is_empty() && bound.len() == files.len(),
        "prepared worker footprint differs from frozen operation"
    );
    for ((expected, mode), (file, bound_mode)) in files.iter().zip(bound) {
        handle.check_file(expected)?;
        handle.check_file(file)?;
        ensure!(
            expected.key() == file.key() && mode == bound_mode,
            "prepared worker resource/access differs from frozen operation"
        );
    }
    let scope_sha256 = native_scope_sha256(scope)?;
    let mut digest = OperationDigest(DigestContext::new(&SHA256));
    digest.bytes(b"aizen-speculation-prepared-native-v1");
    digest.bytes(match kind {
        NativeWorkerKind::Read => b"read",
        NativeWorkerKind::Edit => b"edit",
        NativeWorkerKind::Write => b"write",
    });
    digest.bytes(handle.base_id().as_bytes());
    digest.bytes(handle.id().as_bytes());
    digest.bytes(handle.owner().as_bytes());
    digest.bytes(&scope_sha256);
    digest.u64(generation);
    Ok((
        PreparedNativeWorker {
            handle: handle.clone(),
            scope_sha256,
            generation,
            kind,
            prepared_sha256: [0; 32],
        },
        digest,
    ))
}

/// Encode the actual typed frozen operation, never mutable model args, previews, or Debug output.
/// No live source read, stat, scope resolution, or executable context clone occurs here.
pub(crate) fn prepared_native_worker(
    prepared: &PreparedToolCall,
) -> Result<Option<PreparedNativeWorker>> {
    let Some(frozen) = &prepared.delegation else {
        ensure!(
            !matches!(prepared.footprint, Some(PreparedFootprint::Overlay { .. })),
            "private worker lost its frozen operation"
        );
        return Ok(None);
    };
    let (mut worker, digest) = if let Some(read) = frozen.downcast_ref::<FrozenOverlayRead>() {
        let files = read
            .entries
            .iter()
            .map(|entry| (&entry.file, LockMode::Shared))
            .collect::<Vec<_>>();
        let (worker, mut digest) = begin_worker_digest(
            prepared,
            &read.ctx,
            &read.handle,
            read.generation,
            NativeWorkerKind::Read,
            &files,
        )?;
        digest.flag(read.number);
        digest.flag(read.batch);
        digest.u64(read.omitted as u64);
        digest.u64(read.entries.len() as u64);
        for entry in &read.entries {
            digest.snapshot(&entry.file, &entry.snapshot, false)?;
            digest.bytes(entry.label.as_bytes());
            digest.optional_u64(entry.start);
            digest.optional_u64(entry.end);
        }
        (worker, digest)
    } else if let Some(edit) = frozen.downcast_ref::<FrozenOverlayEdit>() {
        let (worker, mut digest) = begin_worker_digest(
            prepared,
            &edit.ctx,
            &edit.handle,
            edit.generation,
            NativeWorkerKind::Edit,
            &[(&edit.file, LockMode::Exclusive)],
        )?;
        digest.snapshot(&edit.file, &edit.snapshot, true)?;
        digest.bytes(edit.content.as_bytes());
        digest.provenance(edit.provenance);
        digest.flag(edit.dry_run);
        digest.flag(edit.no_op);
        digest.bytes(edit.output.as_bytes());
        (worker, digest)
    } else if let Some(write) = frozen.downcast_ref::<FrozenOverlayWrite>() {
        let (worker, mut digest) = begin_worker_digest(
            prepared,
            &write.ctx,
            &write.handle,
            write.generation,
            NativeWorkerKind::Write,
            &[(&write.file, LockMode::Exclusive)],
        )?;
        digest.snapshot(&write.file, &write.snapshot, true)?;
        digest.bytes(write.content.as_bytes());
        digest.provenance(write.provenance);
        digest.flag(write.no_op);
        digest.bytes(write.output.as_bytes());
        (worker, digest)
    } else {
        ensure!(
            !matches!(prepared.footprint, Some(PreparedFootprint::Overlay { .. })),
            "private worker has an unknown frozen operation"
        );
        return Ok(None);
    };
    worker
        .prepared_sha256
        .copy_from_slice(digest.0.finish().as_ref());
    Ok(Some(worker))
}

pub(crate) fn is_overlay_read(prepared: &PreparedToolCall) -> bool {
    prepared
        .delegation
        .as_ref()
        .is_some_and(|value| value.downcast_ref::<FrozenOverlayRead>().is_some())
}

pub(crate) fn is_overlay_edit(prepared: &PreparedToolCall) -> bool {
    prepared
        .delegation
        .as_ref()
        .is_some_and(|value| value.downcast_ref::<FrozenOverlayEdit>().is_some())
}

pub(crate) fn is_overlay_write(prepared: &PreparedToolCall) -> bool {
    prepared
        .delegation
        .as_ref()
        .is_some_and(|value| value.downcast_ref::<FrozenOverlayWrite>().is_some())
}

pub(crate) fn workspace_effect_prepared(prepared: &PreparedToolCall) -> Option<WorkspaceEffect> {
    if let Some(edit) = prepared
        .delegation
        .as_ref()
        .and_then(|value| value.downcast_ref::<FrozenOverlayEdit>())
    {
        return Some(if edit.dry_run || edit.no_op {
            WorkspaceEffect::None
        } else {
            WorkspaceEffect::PrivateOverlay
        });
    }
    if let Some(write) = prepared
        .delegation
        .as_ref()
        .and_then(|value| value.downcast_ref::<FrozenOverlayWrite>())
    {
        return Some(if write.no_op {
            WorkspaceEffect::None
        } else {
            WorkspaceEffect::PrivateOverlay
        });
    }
    if is_overlay_read(prepared) {
        return Some(WorkspaceEffect::None);
    }
    None
}

pub(crate) fn prepare_read_overlay(
    tool: &FileRead,
    args: &Value,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    bounded_native_arguments(args, "file_read")?;
    let list = args.get("files").and_then(Value::as_array);
    if list.is_some_and(Vec::is_empty) {
        bail!("files[] is empty — pass at least one {{path,start,end}} entry, or use `path`");
    }
    let (handle, _scope, owner) = overlay_parts(ctx)?;
    let single;
    let declarations: &[Value] = if let Some(list) = list {
        list.as_slice()
    } else {
        str_arg(args, "path")?;
        single = vec![args.clone()];
        &single
    };

    // Resolve every declared entry, including entries beyond the eight returned in one call.
    // Resolution is sealed lexical lookup and does not canonicalize/stat/read the live path.
    let mut resolved = Vec::with_capacity(declarations.len());
    for entry in declarations {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .context("private-overlay file_read entry requires an exact path")?;
        let file = handle.resolve(&tool.root, Path::new(path))?;
        handle.validate(&file, false)?;
        resolved.push((path.to_string(), file));
    }
    let shown_len = resolved.len().min(super::MULTI_READ_MAX_FILES);
    let generation_before = handle.generation()?;
    let body = handle.body_lease(&owner)?;
    let mut entries = Vec::with_capacity(shown_len);
    for (index, (label, file)) in resolved.into_iter().take(shown_len).enumerate() {
        let snapshot = handle.snapshot(&file, &body)?;
        entries.push(OverlayReadEntry {
            label,
            start: declarations[index].get("start").and_then(Value::as_u64),
            end: declarations[index].get("end").and_then(Value::as_u64),
            file,
            snapshot,
        });
    }
    drop(body);
    let generation = handle.generation()?;
    ensure!(
        generation == generation_before,
        "private overlay changed during preparation; prepare again"
    );
    let mut prepared = PreparedToolCall::new(args.clone());
    prepared.footprint = Some(overlay_footprint(
        handle,
        entries
            .iter()
            .map(|entry| (entry.file.clone(), LockMode::Shared)),
    ));
    prepared.delegation = Some(Arc::new(FrozenOverlayRead {
        ctx: ctx.clone(),
        handle: handle.clone(),
        entries,
        number: args.get("number").and_then(Value::as_bool).unwrap_or(false),
        batch: list.is_some(),
        omitted: list.map_or(0, |items| {
            items.len().saturating_sub(super::MULTI_READ_MAX_FILES)
        }),
        generation,
    }));
    Ok(prepared)
}

fn render_overlay_read_entry(
    entry: &OverlayReadEntry,
    number: bool,
    max_lines: usize,
    max_bytes: usize,
) -> Result<String> {
    let content = snapshot_text(&entry.snapshot, &entry.label)?;
    if entry.start.is_none() && entry.end.is_none() && !number {
        return Ok(budget_view(&content, &entry.label, max_lines, max_bytes));
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = entry.start.unwrap_or(1).max(1) as usize;
    let end = (entry.end.unwrap_or(lines.len() as u64) as usize).min(lines.len());
    if start > end {
        return Ok(String::new());
    }
    Ok(range_view(
        &lines,
        start,
        end,
        number,
        &entry.label,
        max_lines,
        max_bytes,
    ))
}

pub(crate) fn execute_read_overlay(prepared: &PreparedToolCall) -> Option<Result<String>> {
    let frozen = prepared
        .delegation
        .as_ref()
        .and_then(|value| value.downcast_ref::<FrozenOverlayRead>())?;
    Some(execute_read_overlay_inner(frozen))
}

fn execute_read_overlay_inner(frozen: &FrozenOverlayRead) -> Result<String> {
    super::verify_frozen_native_context(Some(&frozen.ctx))?;
    let scope = frozen
        .ctx
        .native_scope()
        .context("private overlay read lost its sealed native scope")?;
    frozen
        .handle
        .check_owner_scope(&frozen.ctx.resource_scope(), scope)?;
    ensure!(
        frozen.handle.generation()? == frozen.generation,
        "private overlay changed after preparation; prepare again"
    );
    let body = frozen.handle.body_lease(&frozen.ctx.resource_scope())?;
    for entry in &frozen.entries {
        frozen.handle.validate(&entry.file, false)?;
        frozen
            .handle
            .validate_snapshot(&entry.file, &entry.snapshot, &body)?;
    }
    ensure!(
        frozen.handle.generation()? == frozen.generation,
        "private overlay changed while reading; prepare again"
    );
    if !frozen.batch {
        let output = render_overlay_read_entry(
            &frozen.entries[0],
            frozen.number,
            FILE_READ_MAX_LINES,
            FILE_READ_MAX_BYTES,
        )?;
        ensure!(
            output.len() <= PREPARED_EDIT_MAX_BYTES,
            "file_read output exceeds the 4 MiB limit"
        );
        return Ok(format!("{output}\n[{PRIVATE_OVERLAY_NOTE}]"));
    }
    let per_lines = (FILE_READ_MAX_LINES / frozen.entries.len()).max(1);
    let per_bytes = (FILE_READ_MAX_BYTES / frozen.entries.len()).max(1);
    let mut output = String::new();
    for entry in &frozen.entries {
        let header = match (entry.start, entry.end) {
            (None, None) => format!("── {} ──", entry.label),
            (start, end) => format!(
                "── {}:{}-{} ──",
                entry.label,
                start.map_or("1".to_string(), |value| value.to_string()),
                end.map_or("end".to_string(), |value| value.to_string())
            ),
        };
        let body = match render_overlay_read_entry(entry, frozen.number, per_lines, per_bytes) {
            Ok(body) => body,
            Err(error) => format!("error: {error:#}"),
        };
        ensure!(
            output
                .len()
                .saturating_add(header.len())
                .saturating_add(body.len())
                .saturating_add(2)
                <= PREPARED_EDIT_MAX_BYTES,
            "file_read output exceeds the 4 MiB limit"
        );
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&header);
        output.push('\n');
        output.push_str(&body);
    }
    if frozen.omitted > 0 {
        let note = format!(
            "\n…[{} more file(s) not read — {} per call max; call again for the rest]",
            frozen.omitted,
            super::MULTI_READ_MAX_FILES
        );
        ensure!(
            output.len().saturating_add(note.len()) <= PREPARED_EDIT_MAX_BYTES,
            "file_read output exceeds the 4 MiB limit"
        );
        output.push_str(&note);
    }
    Ok(output)
}

fn parse_edit_inputs(args: &Value) -> Result<(String, Option<&Vec<Value>>, usize)> {
    let path = super::str_arg(args, "path")?.to_string();
    let edits = args.get("edits").and_then(Value::as_array);
    if let Some(edits) = edits {
        if edits.is_empty() {
            bail!("file_edit `edits` must be a non-empty array (or omit it and pass new_string for a single edit)");
        }
        if edits.len() > 1024 {
            bail!("file_edit preparation supports at most 1024 edits; nothing written");
        }
    } else if args.get("new_string").is_none() {
        bail!("file_edit needs either `new_string` (single edit, with `old_string`) or a non-empty `edits` array (batch of {{old_string, new_string}} applied atomically to {path})");
    } else if args
        .get("old_string")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        bail!("file_edit requires non-empty old_string and an existing file; use file_write with purpose:project|artifact|scratch to create a file or replace its whole content");
    }
    let input_bytes = if let Some(edits) = edits {
        edits.iter().fold(0usize, |total, edit| {
            total
                .saturating_add(
                    edit.get("old_string")
                        .and_then(Value::as_str)
                        .map_or(0, str::len),
                )
                .saturating_add(
                    edit.get("new_string")
                        .and_then(Value::as_str)
                        .map_or(0, str::len),
                )
        })
    } else {
        args.get("old_string")
            .and_then(Value::as_str)
            .map_or(0, str::len)
            .saturating_add(super::str_arg(args, "new_string")?.len())
    };
    ensure!(
        input_bytes <= PREPARED_EDIT_MAX_BYTES,
        "file_edit arguments exceed the 4 MiB preparation limit; nothing written"
    );
    Ok((path, edits, input_bytes))
}

pub(crate) fn prepare_edit_overlay(
    tool: &FileEdit,
    args: &Value,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    bounded_native_arguments(args, "file_edit")?;
    let (path, edits, _) = parse_edit_inputs(args)?;
    let (handle, _scope, owner) = overlay_parts(ctx)?;
    let file = handle.resolve(&tool.root, Path::new(&path))?;
    handle.validate(&file, true)?;
    let generation_before = handle.generation()?;
    let snapshot = prepare_snapshot(handle, &owner, &file, true)?;
    let generation = handle.generation()?;
    ensure!(
        generation == generation_before,
        "private overlay changed during preparation; prepare again"
    );
    let original = snapshot_text(&snapshot, &path)?;
    let dry_run = dry_run_arg(args);
    let (content, mut output) = if let Some(edits) = edits {
        let mut buf = original.clone();
        let mut summaries = Vec::with_capacity(edits.len());
        let mut diffs = Vec::with_capacity(edits.len());
        let mut preview_bytes = 0usize;
        for (index, edit) in edits.iter().enumerate() {
            let n = index + 1;
            let old = edit
                .get("old_string")
                .and_then(Value::as_str)
                .with_context(|| format!("edit #{n}: missing old_string"))?;
            let new = edit
                .get("new_string")
                .and_then(Value::as_str)
                .with_context(|| format!("edit #{n}: missing new_string"))?;
            let replace_all = edit
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let applied = FileEdit::bounded_edit(
                &buf,
                old,
                new,
                replace_all,
                &format!("edit #{n} ({path})"),
            )?;
            let detail = match (applied.rung, applied.count) {
                ("exact", count) if replace_all => format!("{count} replacement(s), replace_all"),
                ("exact", count) => format!("{count} replacement(s)"),
                ("indent", 1) => "1 replacement, indentation-tolerant".to_string(),
                (other, 1) => format!("1 replacement, {other} match"),
                (other, count) => format!("{count} replacements, {other} match, replace_all"),
            };
            let summary = format!("  #{n}: {detail}");
            let diff = diff_preview(&applied.before, &applied.after, applied.start_line);
            preview_bytes = preview_bytes
                .saturating_add(summary.len())
                .saturating_add(diff.len());
            ensure!(
                preview_bytes <= PREPARED_EDIT_MAX_BYTES,
                "file_edit preview exceeds the 4 MiB preparation limit; nothing written"
            );
            summaries.push(summary);
            diffs.push(diff);
            buf = applied.content;
        }
        let output = if buf == original {
            format!(
                "{NOOP_WRITE_PREFIX}: {path} unchanged after {} edit(s) net to nothing",
                edits.len()
            )
        } else if dry_run {
            format!(
                "{DRY_RUN_PREFIX}: would edit {path} ({} edits); nothing written\n{}\n{}",
                edits.len(),
                summaries.join("\n"),
                diffs.join("\n")
            )
        } else {
            format!(
                "edited {path} ({} edits applied)\n{}\n{}",
                edits.len(),
                summaries.join("\n"),
                diffs.join("\n")
            )
        };
        (buf, output)
    } else {
        let applied = FileEdit::bounded_edit(
            &original,
            super::str_arg(args, "old_string")?,
            super::str_arg(args, "new_string")?,
            args.get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            &path,
        )?;
        let output = if applied.content == original {
            format!("{NOOP_WRITE_PREFIX}: {path} unchanged (old_string == new_string)")
        } else if dry_run {
            format!(
                "{DRY_RUN_PREFIX}: would edit {path} ({}); nothing written\n{}",
                applied.summary(),
                diff_preview(&applied.before, &applied.after, applied.start_line)
            )
        } else {
            format!(
                "edited {path} ({})\n{}",
                applied.summary(),
                diff_preview(&applied.before, &applied.after, applied.start_line)
            )
        };
        (applied.content, output)
    };
    ensure!(
        content.len() <= PREPARED_EDIT_MAX_BYTES,
        "file_edit output exceeds the 4 MiB preparation limit; nothing written"
    );
    let no_op = content == original;
    let actual = file.logical_path().display().to_string();
    output = overlay_output(output);
    ensure!(
        output.len() <= PREPARED_EDIT_MAX_BYTES,
        "file_edit preview exceeds the 4 MiB preparation limit; nothing written"
    );
    let (first, rest) = output.split_once('\n').unwrap_or((&output, ""));
    let mut prepared = PreparedToolCall::new(args.clone());
    prepared.args["path"] = Value::String(actual.clone());
    prepared.preview = Some(overlay_preview(
        actual,
        vec![first.to_string()],
        (!rest.is_empty()).then(|| rest.to_string()),
    ));
    prepared.preview_lines = vec![PRIVATE_OVERLAY_NOTE.to_string()];
    prepared.footprint = Some(overlay_footprint(
        handle,
        [(file.clone(), LockMode::Exclusive)],
    ));
    prepared.delegation = Some(Arc::new(FrozenOverlayEdit {
        ctx: ctx.clone(),
        handle: handle.clone(),
        file,
        snapshot,
        content: content.clone(),
        provenance: localized_provenance(&original, &content),
        output,
        dry_run,
        no_op,
        generation,
    }));
    Ok(prepared)
}

pub(crate) fn execute_edit_overlay(prepared: &PreparedToolCall) -> Option<Result<String>> {
    let frozen = prepared
        .delegation
        .as_ref()
        .and_then(|value| value.downcast_ref::<FrozenOverlayEdit>())?;
    Some(execute_edit_overlay_inner(frozen))
}

fn execute_edit_overlay_inner(frozen: &FrozenOverlayEdit) -> Result<String> {
    super::verify_frozen_native_context(Some(&frozen.ctx))?;
    let scope = frozen
        .ctx
        .native_scope()
        .context("private overlay edit lost its sealed native scope")?;
    frozen
        .handle
        .check_owner_scope(&frozen.ctx.resource_scope(), scope)?;
    ensure!(
        frozen.handle.generation()? == frozen.generation,
        "private overlay changed after preparation; prepare again"
    );
    let body = frozen.handle.body_lease(&frozen.ctx.resource_scope())?;
    frozen.handle.validate(&frozen.file, true)?;
    frozen
        .handle
        .validate_snapshot(&frozen.file, &frozen.snapshot, &body)?;
    ensure!(
        frozen.handle.generation()? == frozen.generation,
        "private overlay changed before edit; prepare again"
    );
    if frozen.dry_run || frozen.no_op {
        return Ok(frozen.output.clone());
    }
    #[cfg(test)]
    super::tests::native_body_rendezvous("file_edit", frozen.file.logical_path());
    frozen.handle.compare_write(
        &frozen.file,
        &frozen.snapshot,
        frozen.content.as_bytes(),
        frozen.provenance,
        &body,
    )?;
    Ok(frozen.output.clone())
}

pub(crate) fn prepare_write_overlay(
    tool: &FileWrite,
    args: &Value,
    ctx: &ExecutionContext,
) -> Result<PreparedToolCall> {
    bounded_native_arguments(args, "file_write")?;
    let path = super::str_arg(args, "path")?;
    let content = super::str_arg(args, "content")?;
    ensure!(
        content.len() <= PREPARED_EDIT_MAX_BYTES,
        "file_write content exceeds the 4 MiB preparation limit; nothing written"
    );
    let purpose = crate::core::file_placement::FilePurpose::parse_optional(args.get("purpose"))?;
    if matches!(
        purpose,
        Some(
            crate::core::file_placement::FilePurpose::Artifact
                | crate::core::file_placement::FilePurpose::Scratch
        )
    ) {
        bail!("private overlay permits only exact project files, not managed destinations");
    }
    let (handle, _scope, owner) = overlay_parts(ctx)?;
    let file = handle.resolve(&tool.root, Path::new(path))?;
    handle.validate(&file, true)?;
    let generation_before = handle.generation()?;
    let snapshot = prepare_snapshot(handle, &owner, &file, true)?;
    let generation = handle.generation()?;
    ensure!(
        generation == generation_before,
        "private overlay changed during preparation; prepare again"
    );
    let before = snapshot
        .bytes()
        .map(|bytes| {
            ensure!(
                bytes.len() <= PREPARED_EDIT_MAX_BYTES,
                "private overlay file exceeds the 4 MiB limit"
            );
            String::from_utf8(bytes.to_vec()).with_context(|| format!("{path} is not valid UTF-8"))
        })
        .transpose()?;
    let no_op = before.as_deref() == Some(content);
    let diff = before
        .as_deref()
        .filter(|before| *before != content)
        .map(|before| bounded_write_diff(before, content))
        .transpose()?;
    let actual = file.logical_path().display().to_string();
    let lines = vec![if no_op {
        format!("{actual} unchanged (identical content)")
    } else {
        format!(
            "overwrite {actual}: {} → {} bytes",
            before.as_ref().map_or(0, |text| text.len()),
            content.len()
        )
    }];
    let raw_output = if no_op {
        format!("{NOOP_WRITE_PREFIX}: {actual} already holds this exact content")
    } else {
        let mut output = format!("overwrote {actual} ({} line(s))", content.lines().count());
        if let Some(diff) = &diff {
            output.push('\n');
            output.push_str(diff);
        }
        output
    };
    let output = overlay_output(raw_output);
    ensure!(
        output.len() <= PREPARED_EDIT_MAX_BYTES,
        "file_write output exceeds the 4 MiB preparation limit; nothing written"
    );
    let mut prepared = PreparedToolCall::new(args.clone());
    prepared.args["path"] = Value::String(actual.clone());
    prepared.preview = Some(overlay_preview(actual, lines, diff));
    prepared.preview_lines = vec![PRIVATE_OVERLAY_NOTE.to_string()];
    prepared.footprint = Some(overlay_footprint(
        handle,
        [(file.clone(), LockMode::Exclusive)],
    ));
    prepared.delegation = Some(Arc::new(FrozenOverlayWrite {
        ctx: ctx.clone(),
        handle: handle.clone(),
        file,
        snapshot,
        content: content.to_string(),
        provenance: WriteProvenance::Rewrite,
        output,
        no_op,
        generation,
    }));
    Ok(prepared)
}

pub(crate) fn execute_write_overlay(prepared: &PreparedToolCall) -> Option<Result<String>> {
    let frozen = prepared
        .delegation
        .as_ref()
        .and_then(|value| value.downcast_ref::<FrozenOverlayWrite>())?;
    Some(execute_write_overlay_inner(frozen))
}

fn execute_write_overlay_inner(frozen: &FrozenOverlayWrite) -> Result<String> {
    super::verify_frozen_native_context(Some(&frozen.ctx))?;
    let scope = frozen
        .ctx
        .native_scope()
        .context("private overlay write lost its sealed native scope")?;
    frozen
        .handle
        .check_owner_scope(&frozen.ctx.resource_scope(), scope)?;
    ensure!(
        frozen.handle.generation()? == frozen.generation,
        "private overlay changed after preparation; prepare again"
    );
    let body = frozen.handle.body_lease(&frozen.ctx.resource_scope())?;
    frozen.handle.validate(&frozen.file, true)?;
    frozen
        .handle
        .validate_snapshot(&frozen.file, &frozen.snapshot, &body)?;
    ensure!(
        frozen.handle.generation()? == frozen.generation,
        "private overlay changed before write; prepare again"
    );
    if frozen.no_op {
        return Ok(frozen.output.clone());
    }
    frozen.handle.compare_write(
        &frozen.file,
        &frozen.snapshot,
        frozen.content.as_bytes(),
        frozen.provenance,
        &body,
    )?;
    Ok(frozen.output.clone())
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    super::str_arg(args, key)
}

#[cfg(test)]
mod authority_tests {
    include!("speculative_native_tools_tests.rs");
}

#[cfg(test)]
mod tests {
    use super::localized_provenance;
    use crate::core::speculation::store::WriteProvenance;

    #[test]
    fn localized_provenance_stays_on_character_boundaries() {
        assert_eq!(
            localized_provenance("α before", "β before"),
            WriteProvenance::Localized { start: 0, end: 2 }
        );
        assert_eq!(
            localized_provenance("prefix α suffix", "prefix β suffix"),
            WriteProvenance::Localized { start: 7, end: 9 }
        );
    }
}
