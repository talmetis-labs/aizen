//! Approval-gated local PNG observations. No runtime, upload client or global evidence lookup.

use super::tools::{ApprovalPreview, PreparedToolCall, Tool, ToolObservation};
use crate::core::tool_media::{ImageProvenance, PixelRect, ToolImage};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct ImageRead {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    transmit_image: bool,
    path: Option<String>,
    evidence_id: Option<String>,
    ref_id: Option<String>,
    crop: Option<Crop>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Args {
    fn parse(value: &Value) -> Result<Self> {
        ensure!(
            value.get("transmit_image").and_then(Value::as_bool) == Some(true),
            "image_read requires explicit transmit_image:true and image destination approval"
        );
        let args: Self =
            serde_json::from_value(value.clone()).context("invalid image_read arguments")?;
        ensure!(args.transmit_image, "image transmission was not requested");
        let sources = usize::from(args.path.is_some())
            + usize::from(args.evidence_id.is_some())
            + usize::from(args.ref_id.is_some());
        ensure!(
            sources == 1,
            "provide exactly one of path, evidence_id or ref_id (received {sources}); crop is not a source; retry with one source and omit the others"
        );
        if let Some(path) = &args.path {
            ensure!(
                !path.trim().is_empty()
                    && path.len() <= 2048
                    && !path.chars().any(char::is_control),
                "path must be nonempty, at most 2048 bytes, without control characters"
            );
        }
        if let Some(id) = &args.evidence_id {
            ensure!(
                !id.trim().is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
                "evidence_id must be nonempty, at most 128 bytes, without control characters"
            );
        }
        if let Some(id) = &args.ref_id {
            crate::core::reference_media::validate_reference_ids(std::slice::from_ref(id))?;
        }
        if let Some(crop) = &args.crop {
            ensure!(
                crop.width > 0
                    && crop.height > 0
                    && crop.x.checked_add(crop.width).is_some()
                    && crop.y.checked_add(crop.height).is_some(),
                "invalid pixel crop dimensions"
            );
        }
        Ok(args)
    }
}

impl ImageRead {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn source_path(&self, path: &str) -> Result<PathBuf> {
        reject_network_shape(Path::new(path))?;
        let supplied = Path::new(path);
        let joined = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            self.root.join(supplied)
        };
        let absolute = if joined.is_absolute() {
            joined
        } else {
            std::env::current_dir()?.join(joined)
        };
        reject_network_shape(&absolute)?;
        check_local_drive(&absolute)?;
        // Refuse links/reparse points before canonicalizing: following one can contact a remote
        // share even when the caller supplied an ordinary-looking local path.
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            prefix.push(component.as_os_str());
            // Verbatim disk prefixes (\\\\?\\C:) report has_root() even before RootDir, but
            // are not valid metadata targets. Probe only after the explicit root separator.
            if matches!(component, std::path::Component::Prefix(_)) || !prefix.has_root() {
                continue;
            }
            let metadata =
                std::fs::symlink_metadata(&prefix).context("local image source is unavailable")?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "image_read refuses symbolic-link sources"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes() & 0x400 == 0,
                    "image_read refuses reparse-point sources"
                );
            }
        }
        let canonical =
            std::fs::canonicalize(&absolute).context("canonical image source is unavailable")?;
        reject_network_shape(&canonical)?;
        check_local_drive(&canonical)?;
        ensure!(
            std::fs::metadata(&canonical)?.is_file(),
            "image source must be a regular local file"
        );
        ensure!(
            canonical.to_string_lossy().len() <= 2048,
            "canonical image path exceeds provenance limit"
        );
        Ok(canonical)
    }
}

fn reject_network_shape(path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    let normalized = text.replace('\\', "/");
    // canonicalize on Windows produces the safe verbatim disk form, not an UNC/device path.
    let verbatim_disk = normalized
        .as_bytes()
        .get(4)
        .is_some_and(u8::is_ascii_alphabetic)
        && normalized.as_bytes().get(5) == Some(&b':')
        && normalized.as_bytes().get(6) == Some(&b'/')
        && normalized.starts_with("//?/");
    ensure!(
        !normalized.contains("://") && (!normalized.starts_with("//") || verbatim_disk),
        "image_read accepts local filesystem paths only; URLs, UNC and device paths are refused"
    );
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            ensure!(
                matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
                    && path.is_absolute(),
                "image_read requires an absolute disk path or a workspace-relative path"
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
fn check_local_drive(path: &Path) -> Result<()> {
    use std::path::{Component, Prefix};
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    // Win32 drive-kind constants; avoid enabling WindowsProgramming just for three values.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;
    let drive = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => bail!("image source must be on a local disk"),
        },
        _ => bail!("image source must be on a local disk"),
    };
    let root = [u16::from(drive), b':' as u16, b'\\' as u16, 0];
    // This queries the drive kind locally; it does not probe a caller-supplied SMB share.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    ensure!(
        matches!(kind, DRIVE_FIXED | DRIVE_RAMDISK | DRIVE_REMOVABLE),
        "image source drive is not local"
    );
    Ok(())
}

#[cfg(not(windows))]
fn check_local_drive(_path: &Path) -> Result<()> {
    Ok(())
}

fn safe_display(text: &str) -> String {
    text.chars().flat_map(char::escape_default).collect()
}

impl Tool for ImageRead {
    fn name(&self) -> &str {
        "image_read"
    }
    fn description(&self) -> &str {
        "Send PNG pixels: exactly one local path, scoped evidence_id or granted user ref_id; optional pixel crop. Requires transmit_image:true and destination approval. Refs need live controller consent. No freshness/visual-review claim."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","minLength":1,"maxLength":2048,"description":"Local PNG path; omit evidence_id and ref_id."},
            "evidence_id":{"type":"string","minLength":1,"maxLength":128,"description":"Scoped PNG evidence; omit path and ref_id."},
            "ref_id":{"type":"string","minLength":1,"maxLength":128,"description":"Granted user reference; omit path and evidence_id."},
            "transmit_image":{"type":"boolean","enum":[true]},
            "crop":{"type":"object","properties":{
                "x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},
                "width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}
            },"required":["x","y","width","height"],"additionalProperties":false}
        },"required":["transmit_image"],"oneOf":[
            {"required":["path"]},{"required":["evidence_id"]},{"required":["ref_id"]}
        ],"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn transmits_images(&self, _args: &Value) -> bool {
        true
    }
    fn plan_call(
        &self,
        args: &Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        self.prepare_call(args, ctx)
    }
    fn prepare_call(
        &self,
        args: &Value,
        _ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        Args::parse(args)?;
        Ok(PreparedToolCall::new(args.clone()))
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        bail!("image_read requires the approval-gated image-aware observation executor")
    }
    fn preview(&self, value: &Value) -> Option<ApprovalPreview> {
        let source = match Args::parse(value) {
            Ok(args) => {
                match args.path {
                    Some(path) => match self.source_path(&path) {
                        Ok(path) => format!(
                            "canonical source: {}",
                            safe_display(&path.to_string_lossy())
                        ),
                        Err(error) => {
                            format!("source unavailable: {}", safe_display(&error.to_string()))
                        }
                    },
                    None if args.ref_id.is_some() => {
                        let id = args.ref_id.as_deref().unwrap_or_default();
                        let image = crate::core::exec_ctx::current()
                            .and_then(|ctx| ctx.references())
                            .and_then(|access| access.original(id).ok());
                        match image {
                            Some(image) => format!(
                                "granted user reference: {} · {}x{} · original payload available",
                                safe_display(id),
                                image.metadata().width.unwrap_or_default(),
                                image.metadata().height.unwrap_or_default()
                            ),
                            None => {
                                "reference unavailable or no live controller grant in this task"
                                    .into()
                            }
                        }
                    }
                    None => {
                        let id = args.evidence_id.as_deref().unwrap_or_default();
                        let image = crate::core::exec_ctx::current()
                            .and_then(|ctx| ctx.design())
                            .and_then(|ctx| {
                                crate::core::session_store::scoped_live_tool_image(&ctx, id)
                            });
                        match image {
                        Some(image) => format!("retained evidence: {} · source: {} · payload {}", safe_display(id),
                            safe_display(image.metadata().provenance.locator.as_deref().unwrap_or("unavailable")),
                            if image.is_available() { "available" } else { "unavailable" }),
                        None => format!("retained evidence: {} (current-session scoped payload unavailable)", safe_display(id)),
                    }
                    }
                }
            }
            Err(error) => {
                return Some(ApprovalPreview {
                    title: self.name().into(),
                    diff: None,
                    lines: vec![format!(
                        "invalid request: {}",
                        safe_display(&error.to_string())
                    )],
                });
            }
        };
        Some(ApprovalPreview {
            title: "image_read".into(), diff: None,
            lines: vec![source,
                "Transmit potentially sensitive PNG pixels to the configured image-capable model destination; approval is still required.".into(),
                "Local source validation only; no network, capture, runtime, install or upload in this preview.".into(),
                format!("pixel crop: {}", safe_display(&value.get("crop").map(Value::to_string).unwrap_or_else(|| "none".into()))),
            ],
        })
    }
    fn execute_observation(&self, value: &Value) -> Result<ToolObservation> {
        let args = Args::parse(value)?;
        let design = crate::core::exec_ctx::current().and_then(|ctx| ctx.design());
        if let Some(id) = args.ref_id.as_deref() {
            let access = crate::core::exec_ctx::current()
                .and_then(|ctx| ctx.references())
                .context("reference unavailable: no live controller grant in this task")?;
            let crop = args.crop.map(|crop| PixelRect {
                x: crop.x,
                y: crop.y,
                width: crop.width,
                height: crop.height,
            });
            let image = access.read(id, crop)?;
            ensure!(
                image.metadata().mime == "image/png",
                "image_read supports PNG only"
            );
            // This explicit reference lineage can credit only a reference observation receipt,
            // never implementation capture/review evidence in the parent's or child's ledger.
            return Ok(ToolObservation {
                text: json!({"image":image.metadata(),"reference_id":id,
                    "freshness":"unavailable", "note":"Granted user reference pixels; not rendered acceptance evidence or visual review."}).to_string(),
                images: vec![image],
            });
        }
        let mut image = if let Some(path) = args.path {
            let path = self.source_path(&path)?;
            let mut provenance =
                ImageProvenance::new("image_read", Some(path.to_string_lossy().into_owned()));
            provenance.evidence_id = Some(format!(
                "image-{}-{}",
                std::process::id(),
                crate::core::persist::unique_sequence()
            ));
            provenance.design_scope = design.as_ref().map(|ctx| ctx.evidence_scope());
            ToolImage::from_file(&path, provenance)?
        } else {
            let id = args.evidence_id.as_deref().context("evidence_id missing")?;
            let ctx = design.as_ref().context("retained evidence image unavailable: no coherent current-session scoped payload lookup")?;
            let image = crate::core::session_store::scoped_live_tool_image(ctx, id)
                .context("retained evidence image unavailable in this current-session scope")?;
            ensure!(
                image.is_available(),
                "retained evidence image payload unavailable: {:?}",
                image.unavailable_reason()
            );
            ensure!(
                args.crop.is_none() || image.metadata().provenance.crop.is_none(),
                "retained image is already cropped; nested crops are unavailable"
            );
            image
        };
        ensure!(
            image.metadata().mime == "image/png",
            "image_read supports PNG only"
        );
        if let Some(crop) = args.crop {
            let retained = args.evidence_id.is_some();
            image = image.crop_png(PixelRect {
                x: crop.x,
                y: crop.y,
                width: crop.width,
                height: crop.height,
            })?;
            if retained {
                // A crop is new reference evidence, not a projection of the whole registered
                // viewport. Do not let its projection make the original acceptance image seen.
                let mut provenance = image.metadata().provenance.clone();
                provenance.evidence_id = Some(format!(
                    "image-{}-{}",
                    std::process::id(),
                    crate::core::persist::unique_sequence()
                ));
                provenance.approved_destination = None;
                image = ToolImage::from_bytes(&image.bytes()?, "image/png", provenance)?;
            }
        }
        // Manual reference reads are not rendered acceptance captures and must not spend the
        // design viewport capture budget. Their scoped identity is selection provenance only.
        Ok(ToolObservation {
            text: json!({"image":image.metadata(),"freshness":"unavailable",
                "note":"Actual PNG reference pixels attached; not rendered acceptance evidence or visual review."})
            .to_string(),
            images: vec![image],
        })
    }
}

/// Reference-only surface for planners. Never accepts a path or rendered evidence handle.
pub struct ReferenceRead(ImageRead);
impl ReferenceRead {
    pub fn new(root: PathBuf) -> Self {
        Self(ImageRead::new(root))
    }

    fn validate(value: &Value) -> Result<()> {
        let args = Args::parse(value)?;
        ensure!(
            args.ref_id.is_some() && args.path.is_none() && args.evidence_id.is_none(),
            "reference_read accepts a granted ref_id only, never a path or evidence_id"
        );
        Ok(())
    }
}

impl Tool for ReferenceRead {
    fn name(&self) -> &str {
        "reference_read"
    }
    fn description(&self) -> &str {
        "Read original PNG pixels or a pixel crop from a current-task granted user ref_id only. Requires transmit_image:true, vision support and destination approval. No filesystem path, rendered evidence or sibling access. Not visual review."
    }
    fn parameters(&self) -> Value {
        let mut parameters = self.0.parameters();
        if let Some(properties) = parameters
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            properties.remove("path");
            properties.remove("evidence_id");
        }
        parameters.as_object_mut().unwrap().remove("oneOf");
        parameters["required"] = json!(["ref_id", "transmit_image"]);
        parameters["properties"]["ref_id"]["description"] =
            json!("Current-task granted user reference ID.");
        parameters
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn transmits_images(&self, _args: &Value) -> bool {
        true
    }
    fn plan_call(
        &self,
        args: &Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        self.prepare_call(args, ctx)
    }
    fn prepare_call(
        &self,
        args: &Value,
        _ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        Self::validate(args)?;
        Ok(PreparedToolCall::new(args.clone()))
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        bail!("reference_read requires the approval-gated image-aware observation executor")
    }
    fn preview(&self, value: &Value) -> Option<ApprovalPreview> {
        if let Err(error) = Self::validate(value) {
            return Some(ApprovalPreview {
                title: self.name().into(),
                diff: None,
                lines: vec![format!(
                    "invalid request: {}",
                    safe_display(&error.to_string())
                )],
            });
        }
        self.0.preview(value).map(|mut preview| {
            preview.title = self.name().into();
            preview
        })
    }
    fn execute_observation(&self, value: &Value) -> Result<ToolObservation> {
        Self::validate(value)?;
        self.0.execute_observation(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool_media::{encode_rgba_png, MAX_IMAGE_BYTES};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "aizen-image-read-{}-{}",
                std::process::id(),
                crate::core::persist::unique_sequence()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(std::fs::canonicalize(root).unwrap())
        }
        fn tool(&self) -> ImageRead {
            ImageRead::new(self.0.clone())
        }
        fn png(&self) {
            std::fs::write(
                self.0.join("test.png"),
                encode_rgba_png(&[255, 0, 0, 255, 0, 0, 255, 255], 2, 1).unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reference_only_surface_never_accepts_filesystem_or_output_handles() {
        let tool = ReferenceRead::new(PathBuf::from("unused-fixture-root"));
        for args in [
            json!({"path":"x.png","transmit_image":true}),
            json!({"evidence_id":"output","transmit_image":true}),
            json!({"ref_id":"ref-id","path":"x.png","transmit_image":true}),
        ] {
            assert!(tool.execute_observation(&args).is_err());
        }
        let parameters = tool.parameters();
        assert!(parameters["properties"].get("path").is_none());
        assert!(parameters["properties"].get("evidence_id").is_none());
        assert_eq!(parameters["required"], json!(["ref_id", "transmit_image"]));
        assert!(tool.is_destructive() && tool.transmits_images(&json!({})));
    }

    #[test]
    fn opt_in_exactly_one_source_and_strict_crop() {
        for value in [
            json!({"path":"test.png"}),
            json!({"path":"test.png","transmit_image":false}),
            json!({"transmit_image":true}),
            json!({"path":"x","evidence_id":"id","transmit_image":true}),
            json!({"path":"x","ref_id":"ref-id","transmit_image":true}),
            json!({"evidence_id":"id","ref_id":"ref-id","transmit_image":true}),
            json!({"ref_id":"hash-not-capability","transmit_image":true}),
            json!({"path":"x","transmit_image":true,"extra":true}),
            json!({"path":"x","transmit_image":true,"crop":{"x":0,"y":0,"width":0,"height":1}}),
            json!({"path":"x","transmit_image":true,"crop":{"x":-1,"y":0,"width":1,"height":1}}),
            json!({"path":"x","transmit_image":true,"crop":{"x":4294967295u64,"y":0,"width":1,"height":1}}),
        ] {
            assert!(Args::parse(&value).is_err(), "{value}");
        }
    }

    #[test]
    fn planning_rejects_invalid_sources_without_loading_pixels() {
        let tool = ImageRead::new(PathBuf::from("unused-fixture-root"));
        let ctx = crate::core::exec_ctx::ExecutionContext::new(
            crate::core::convo::ConversationId::new("image-read-plan"),
        );
        for (args, count) in [
            (
                json!({"transmit_image":true,"crop":{"x":536,"y":0,"width":1000,"height":770}}),
                0,
            ),
            (
                json!({"transmit_image":true,"path":"x.png","evidence_id":"id"}),
                2,
            ),
            (json!({"transmit_image":true,"path":null,"ref_id":null}), 0),
        ] {
            for result in [tool.plan_call(&args, &ctx), tool.prepare_call(&args, &ctx)] {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(&format!("received {count}")), "{error}");
            }
            let preview = tool.preview(&args).unwrap();
            assert_eq!(preview.lines.len(), 1);
            assert!(!preview.lines[0].contains("approval is still required"));
        }
        for args in [
            json!({"transmit_image":true,"path":"missing.png"}),
            json!({"transmit_image":true,"evidence_id":"scoped-id"}),
            json!({"transmit_image":true,"ref_id":"ref-test"}),
            json!({"transmit_image":true,"path":"missing.png","evidence_id":null,"ref_id":null}),
        ] {
            assert_eq!(tool.plan_call(&args, &ctx).unwrap().args, args);
            assert_eq!(tool.prepare_call(&args, &ctx).unwrap().args, args);
        }
        let reference = ReferenceRead::new(PathBuf::from("unused-fixture-root"));
        for args in [
            json!({"transmit_image":true}),
            json!({"transmit_image":true,"path":"missing.png"}),
            json!({"transmit_image":true,"evidence_id":"scoped-id"}),
        ] {
            assert!(reference.plan_call(&args, &ctx).is_err());
            assert!(reference.prepare_call(&args, &ctx).is_err());
        }
        assert!(reference
            .plan_call(&json!({"transmit_image":true,"ref_id":"ref-test"}), &ctx)
            .is_ok());
        assert!(reference.parameters().get("oneOf").is_none());
        assert_eq!(
            tool.parameters()["oneOf"],
            json!([
                {"required":["path"]}, {"required":["evidence_id"]}, {"required":["ref_id"]}
            ])
        );
    }

    #[test]
    fn actual_png_and_crop_have_typed_pixels_and_lineage() {
        let fixture = Fixture::new();
        fixture.png();
        let tool = fixture.tool();
        let full = tool
            .execute_observation(&json!({"path":"test.png","transmit_image":true}))
            .unwrap();
        assert_eq!(full.images.len(), 1);
        assert!(full.images[0].is_available());
        assert_eq!(full.images[0].metadata().width, Some(2));
        assert!(!full.text.contains("base64"));
        let cropped = tool
            .execute_observation(&json!({"path":"test.png","transmit_image":true,
            "crop":{"x":1,"y":0,"width":1,"height":1}}))
            .unwrap();
        assert_eq!(cropped.images[0].metadata().width, Some(1));
        assert_eq!(
            cropped.images[0]
                .metadata()
                .provenance
                .parent_sha256
                .as_ref(),
            Some(&full.images[0].metadata().sha256)
        );
        assert!(tool
            .execute_observation(&json!({"path":"test.png","transmit_image":true,
            "crop":{"x":2,"y":0,"width":1,"height":1}}))
            .is_err());
    }

    #[test]
    fn rejects_corrupt_non_png_missing_directory_and_oversize() {
        let fixture = Fixture::new();
        let tool = fixture.tool();
        std::fs::write(fixture.0.join("corrupt.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(fixture.0.join("other.png"), b"GIF89a-not-decoded").unwrap();
        let file = std::fs::File::create(fixture.0.join("large.png")).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        for path in ["corrupt.png", "other.png", "missing.png", "large.png", "."] {
            assert!(
                tool.execute_observation(&json!({"path":path,"transmit_image":true}))
                    .is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn approval_is_offline_sanitized_and_schema_is_small() {
        let fixture = Fixture::new();
        fixture.png();
        let tool = fixture.tool();
        assert!(tool.is_destructive());
        assert!(tool.transmits_images(&json!({})));
        assert!(!tool.is_concurrency_safe());
        assert!(tool.execute(&json!({})).is_err());
        let preview = tool
            .preview(&json!({"path":"test.png","transmit_image":true}))
            .unwrap();
        assert!(preview.lines[0].contains("canonical source"));
        assert!(preview.lines.join("\n").contains("no network"));
        let malicious = tool
            .preview(&json!({"path":"\u{1b}[31m\nsource","transmit_image":true}))
            .unwrap();
        assert!(!malicious.lines.join("").chars().any(char::is_control));
        let def = crate::core::types::ToolDef::function(
            tool.name(),
            tool.description(),
            tool.parameters(),
        );
        assert!(serde_json::to_vec(&def).unwrap().len() <= 1400);
    }

    #[test]
    fn reference_reads_require_live_scoped_grants_and_never_become_output_evidence() {
        use crate::core::reference_media::{
            ReferenceDerivatives, ReferenceGrantScope, ReferenceRegistry, ReferenceRoute,
        };
        let fixture = Fixture::new();
        fixture.png();
        let original = ToolImage::from_file(
            &fixture.0.join("test.png"),
            ImageProvenance::new("fixture", None),
        )
        .unwrap();
        let mut messages = vec![crate::core::types::Message::user_with_images(
            "reference",
            vec![original.data_url()],
        )];
        let registry = ReferenceRegistry::new("owner");
        let id = registry.register_loaded(&mut messages).unwrap()[0]
            .reference_id
            .clone();
        let access = registry
            .grant(
                std::slice::from_ref(&id),
                ReferenceGrantScope {
                    owner: "owner".into(),
                    purpose: "reference match".into(),
                    task_scope: "task".into(),
                    route: ReferenceRoute {
                        destination: "destination".into(),
                        account: "account".into(),
                    },
                },
                ReferenceDerivatives::ALL,
            )
            .unwrap();
        let ctx = crate::core::exec_ctx::ExecutionContext::new(
            crate::core::convo::ConversationId::new("owner"),
        )
        .with_resource_scope("task")
        .with_references(access.clone());
        let value = json!({"ref_id":id,"transmit_image":true,
            "crop":{"x":1,"y":0,"width":1,"height":1}});
        let observation = crate::core::exec_ctx::with_current(ctx.clone(), || {
            fixture.tool().execute_observation(&value)
        })
        .unwrap();
        let provenance = &observation.images[0].metadata().provenance;
        assert_eq!(provenance.source, "reference_read");
        assert_eq!(provenance.locator.as_deref(), Some(id.as_str()));
        assert_eq!(
            provenance.parent_sha256.as_ref(),
            Some(&original.metadata().sha256)
        );
        assert!(provenance.design_scope.is_none());
        assert!(provenance.approved_destination.is_none());
        assert!(
            crate::core::exec_ctx::with_current(ctx.with_resource_scope("sibling"), || fixture
                .tool()
                .execute_observation(&value))
            .is_err()
        );
        access.revoke();
        assert!(crate::core::exec_ctx::with_current(ctx, || fixture
            .tool()
            .execute_observation(&value))
        .is_err());
        // A reference ID is not an evidence ID, a local path or a globally accessible blob hash.
        assert!(fixture
            .tool()
            .execute_observation(&json!({"ref_id":id,"transmit_image":true}))
            .is_err());
    }

    #[test]
    fn evidence_id_fails_explicitly_without_unrelated_disk_fallback() {
        let fixture = Fixture::new();
        fixture.png();
        let error = fixture
            .tool()
            .execute_observation(&json!({"evidence_id":"test.png","transmit_image":true}))
            .unwrap_err();
        assert!(error.to_string().contains("scoped payload lookup"));
    }

    #[test]
    fn missing_retained_evidence_stays_unavailable_with_explicit_design_scope() {
        let fixture = Fixture::new();
        let owner = format!(
            "image-read-test-{}",
            crate::core::persist::unique_sequence()
        );
        let design = super::super::design::DesignContext::new(
            owner,
            fixture.0.to_string_lossy(),
            super::super::design::DesignMode::Auto,
        );
        design.begin_turn("task", fixture.0.to_string_lossy());
        let ctx = crate::core::exec_ctx::ExecutionContext::default().with_design(design);
        let result = crate::core::exec_ctx::with_current(ctx, || {
            fixture
                .tool()
                .execute_observation(&json!({"evidence_id":"not-published","transmit_image":true}))
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("current-session scope"));
    }

    #[test]
    fn local_design_evidence_never_claims_source_freshness_or_review() {
        let fixture = Fixture::new();
        fixture.png();
        let design = super::super::design::DesignContext::new(
            "owner",
            fixture.0.to_string_lossy(),
            super::super::design::DesignMode::Auto,
        );
        design.begin_turn("task", fixture.0.to_string_lossy());
        let scope = design.evidence_scope();
        let ctx = crate::core::exec_ctx::ExecutionContext::default().with_design(design.clone());
        let observation = crate::core::exec_ctx::with_current(ctx, || {
            fixture
                .tool()
                .execute_observation(&json!({"path":"test.png","transmit_image":true}))
        })
        .unwrap();
        let metadata = observation.images[0].metadata();
        assert_eq!(
            metadata.provenance.design_scope.as_deref(),
            Some(scope.as_str())
        );
        let state = design.snapshot();
        assert!(state.evidence.is_empty());
        assert_eq!(state.captures_this_pass, 0);
        let id = metadata.provenance.evidence_id.as_deref().unwrap();
        assert!(id.starts_with("image-") && id.len() <= 128);
        assert!(design.record_review(id, true, "seen").is_err());
        // More than the design capture allowance still works: these are reference reads.
        for _ in 0..5 {
            let ctx =
                crate::core::exec_ctx::ExecutionContext::default().with_design(design.clone());
            assert!(crate::core::exec_ctx::with_current(ctx, || fixture
                .tool()
                .execute_observation(&json!({"path":"test.png","transmit_image":true})))
            .is_ok());
        }
        assert_eq!(design.snapshot().captures_this_pass, 0);
    }

    #[test]
    fn refuses_network_and_device_shapes_before_file_probe() {
        let tool = ImageRead::new(PathBuf::from("unused-root"));
        for path in [
            "https://host/image.png",
            "//host/share/image.png",
            r"\\host\share\image.png",
            r"\\.\pipe\image",
            r"\\?\UNC\host\share\image.png",
        ] {
            assert!(
                tool.source_path(path)
                    .unwrap_err()
                    .to_string()
                    .contains("local filesystem"),
                "{path}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_disk_root_is_probed_only_after_its_separator() {
        let fixture = Fixture::new();
        fixture.png();
        // Fixture::new canonicalizes, so the path carries Windows' verbatim disk prefix.
        assert!(
            matches!(fixture.0.components().next(), Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_)))
        );
        let path = fixture.0.join("test.png");
        let resolved = fixture.tool().source_path(&path.to_string_lossy()).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_before_following_it() {
        let fixture = Fixture::new();
        fixture.png();
        std::os::unix::fs::symlink(fixture.0.join("test.png"), fixture.0.join("link.png")).unwrap();
        assert!(fixture
            .tool()
            .source_path("link.png")
            .unwrap_err()
            .to_string()
            .contains("symbolic-link"));
    }
}
