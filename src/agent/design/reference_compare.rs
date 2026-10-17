//! Local, bounded pixel measurement. No registration, resizing, implicit tolerance or CV.
//! Binding and acceptance requirements must come from the authorized controller, not tool arguments.
use crate::llm::image_send::{try_image_memory_lease, ImageMemoryLease};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use super::Viewport;
use crate::core::tool_media::{self, PixelRect};

pub const MAX_REGIONS: usize = 16;

/// Shared nonblocking codec/comparison admission. Reject rather than wait holding buffers.
pub(crate) struct WorkBudget {
    lease: ImageMemoryLease,
}
impl std::fmt::Debug for WorkBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkBudget").finish_non_exhaustive()
    }
}
impl WorkBudget {
    pub(crate) fn acquire(bytes: usize) -> Result<Self> {
        let lease = try_image_memory_lease(bytes).ok_or_else(|| anyhow::anyhow!("reference work memory budget busy/exceeded; retry later without retaining decoded buffers"))?;
        Ok(Self { lease })
    }
    pub(crate) fn ensure_total(&mut self, total: usize) -> Result<()> {
        let extra = total.saturating_sub(self.lease.bytes());
        ensure!(
            self.lease.try_grow(extra),
            "reference work memory budget busy/exceeded"
        );
        Ok(())
    }
    fn split(&mut self, bytes: usize) -> Result<ImageMemoryLease> {
        ensure!(
            bytes <= self.lease.bytes(),
            "diagnostic reservation exceeds admitted work"
        );
        Ok(self.lease.split(bytes))
    }
}

pub(crate) fn encoded_rgba_size(bytes: &[u8], mime: &str) -> Result<usize> {
    ensure!(
        bytes.len() <= tool_media::MAX_IMAGE_BYTES && tool_media::sniff_mime(bytes) == Some(mime),
        "invalid bounded encoded image"
    );
    match mime {
        "image/png" => {
            validate_png_encoding(bytes)?;
            tool_media::rgba_size(
                u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
                u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
            )
        }
        "image/jpeg" => {
            validate_jpeg_encoding(bytes)?;
            let mut i = 2;
            while i < bytes.len() {
                while bytes[i] == 0xff {
                    i += 1;
                }
                let marker = bytes[i];
                i += 1;
                let len = u16::from_be_bytes(bytes[i..i + 2].try_into().unwrap()) as usize;
                if marker == 0xc0 {
                    return tool_media::rgba_size(
                        u16::from_be_bytes(bytes[i + 5..i + 7].try_into().unwrap()) as u32,
                        u16::from_be_bytes(bytes[i + 3..i + 5].try_into().unwrap()) as u32,
                    );
                }
                i += len;
            }
            bail!("missing JPEG dimensions")
        }
        _ => bail!("unsupported comparison image format"),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FidelityMode {
    #[default]
    Measure,
    Exact,
    NearMatch,
}

/// Frozen, explicit integer limits; never supplied implicitly by the comparator/model review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonLimits {
    pub max_changed_pixels: u64,
    pub max_channel_delta: u8,
    pub max_total_channel_error: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonRegion {
    pub name: String,
    pub reference: PixelRect,
    pub output: PixelRect,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonRequest {
    #[serde(default)]
    pub mode: FidelityMode,
    #[serde(default)]
    pub limits: Option<ComparisonLimits>,
    /// Additional explicitly aligned regions. Whole-frame measurement is never excluded.
    #[serde(default)]
    pub regions: Vec<ComparisonRegion>,
}
impl ComparisonRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.regions.len() <= MAX_REGIONS,
            "at most 16 comparison regions"
        );
        ensure!(
            self.mode == FidelityMode::NearMatch || self.limits.is_none(),
            "limits require explicit near_match mode"
        );
        for (i, region) in self.regions.iter().enumerate() {
            label(&region.name, 128)?;
            ensure!(
                !self.regions[..i].iter().any(|r| r.name == region.name),
                "comparison region names must be unique"
            );
            ensure!(
                region.reference.width > 0
                    && region.reference.height > 0
                    && region.reference.width == region.output.width
                    && region.reference.height == region.output.height,
                "comparison regions must have explicit equal nonzero dimensions"
            );
        }
        Ok(())
    }
}

/// Trusted identity of the authorized original and the actual output capture; not image paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonBinding {
    pub reference_id: String,
    pub reference_sha256: String,
    pub output_evidence_id: String,
    pub output_sha256: String,
    pub source_hash: String,
    pub render_revision: u64,
    pub target: String,
    pub viewport: Viewport,
    /// Capture identity supplied by runtime. Font readiness alone is not font identity.
    pub dpr_milli: u32,
    pub font_identity: String,
    pub frame_state: String,
    pub request: ComparisonRequest,
}
impl ComparisonBinding {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.reference_id,
            &self.output_evidence_id,
            &self.source_hash,
            &self.font_identity,
            &self.frame_state,
        ] {
            label(value, 128)?;
        }
        label(&self.target, 2048)?;
        for hash in [&self.reference_sha256, &self.output_sha256] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "comparison requires original and output SHA-256 hashes"
            );
        }
        ensure!(
            self.dpr_milli > 0 && self.dpr_milli <= 16000,
            "invalid explicit capture DPR"
        );
        tool_media::rgba_size(self.viewport.width, self.viewport.height)?;
        self.request.validate()
    }
}
fn label(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "invalid bounded comparison identity"
    );
    Ok(())
}

/// Normalized sample channels, not a color-managed or perceptual equality claim.
#[derive(Clone)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}
impl std::fmt::Debug for RgbaImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RgbaImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}
impl RgbaImage {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.pixels.len() == tool_media::rgba_size(self.width, self.height)?,
            "invalid bounded RGBA image"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PixelMetrics {
    pub pixels: u64,
    pub changed_pixels: u64,
    pub changed_fraction: f64,
    pub total_channel_error: u64,
    pub mean_channel_error: f64,
    pub max_channel_delta: u8,
    /// Reference-coordinate extent of all changed pixels, not semantic segmentation.
    pub mismatch_bbox: Option<PixelRect>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionMetrics {
    pub name: String,
    pub metrics: PixelMetrics,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonMetrics {
    pub whole_frame: PixelMetrics,
    pub regions: Vec<RegionMetrics>,
}

/// Produced only by this module. No deserializer/model boolean can manufacture a receipt.
#[derive(Debug, Clone)]
pub struct ComparisonReceipt {
    binding: ComparisonBinding,
    metrics: ComparisonMetrics,
    accepted: Option<bool>,
}
impl ComparisonReceipt {
    pub fn binding(&self) -> &ComparisonBinding {
        &self.binding
    }
    pub fn metrics(&self) -> &ComparisonMetrics {
        &self.metrics
    }
    pub fn accepted(&self) -> Option<bool> {
        self.accepted
    }
}
#[derive(Debug)]
pub struct ComparisonResult {
    pub receipt: ComparisonReceipt,
    /// Diagnostic images only; they are not implementation acceptance evidence.
    pub diff_png: Option<Vec<u8>>,
    pub overlay_png: Option<Vec<u8>>,
    pub artifact_note: Option<String>,
    _memory: Option<WorkBudget>,
}

impl ComparisonResult {
    /// Transfer both storage and payload reservations out of work ownership before returning images.
    pub(crate) fn diagnostic_images(&mut self) -> Result<Vec<tool_media::ToolImage>> {
        let budget = self
            ._memory
            .as_mut()
            .context("diagnostics lack admitted memory owner")?;
        // Separate retained encoded storage, new base64 allocations, decoded validation output and
        // decoder scratch. Splitting payload ownership must not borrow bytes covering raw buffers.
        let mut raw_capacity = 0usize;
        let mut payload_capacity = 0usize;
        let mut decoded_capacity = 0usize;
        for bytes in [&self.diff_png, &self.overlay_png].into_iter().flatten() {
            raw_capacity += bytes.capacity();
            payload_capacity += bytes.len().div_ceil(3) * 4;
            decoded_capacity = decoded_capacity.max(encoded_rgba_size(bytes, "image/png")?);
        }
        budget.ensure_total(
            raw_capacity + payload_capacity + decoded_capacity + tool_media::MAX_PNG_DECODE_BYTES,
        )?;
        let mut images = Vec::new();
        for (source, slot) in [
            ("reference_diff", &mut self.diff_png),
            ("reference_overlay", &mut self.overlay_png),
        ] {
            if let Some(bytes) = slot.take() {
                let payload_bytes = bytes
                    .len()
                    .div_ceil(3)
                    .checked_mul(4)
                    .context("diagnostic base64 capacity overflow")?;
                let owner = budget.split(payload_bytes)?;
                images.push(tool_media::ToolImage::from_bytes_with_memory(
                    &bytes,
                    "image/png",
                    tool_media::ImageProvenance::new(source, None),
                    owner,
                )?);
            }
        }
        Ok(images)
    }
}

/// Fixture wrapper; production verifies admitted original/capture bytes through compare_owned.
#[cfg(test)]
pub fn compare_rgba(
    reference: &RgbaImage,
    output: &RgbaImage,
    binding: ComparisonBinding,
) -> Result<ComparisonResult> {
    reference.validate()?;
    output.validate()?;
    let size = reference
        .pixels
        .len()
        .checked_add(output.pixels.len())
        .context("comparison size overflow")?;
    let budget = WorkBudget::acquire(
        size.saturating_add(reference.pixels.len())
            .saturating_add(32 * 1024 * 1024),
    )?;
    let mut result = compare_inner(reference, output, binding)?;
    result._memory = Some(budget);
    Ok(result)
}

fn compare_inner(
    reference: &RgbaImage,
    output: &RgbaImage,
    binding: ComparisonBinding,
) -> Result<ComparisonResult> {
    binding.validate()?;
    ensure!(reference.width == output.width && reference.height == output.height, "unaligned images: provide equal-size original viewport captures; no implicit resize/alignment");
    // Physical output size must be exactly described by viewport and DPR, with no rounding guess.
    ensure!(
        u64::from(output.width) * 1000
            == u64::from(binding.viewport.width) * u64::from(binding.dpr_milli)
            && u64::from(output.height) * 1000
                == u64::from(binding.viewport.height) * u64::from(binding.dpr_milli),
        "capture dimensions do not match declared viewport/DPR"
    );
    let full = PixelRect {
        x: 0,
        y: 0,
        width: reference.width,
        height: reference.height,
    };
    let whole_frame = measure(reference, output, full, full)?;
    let mut regions = Vec::new();
    for region in &binding.request.regions {
        regions.push(RegionMetrics {
            name: region.name.clone(),
            metrics: measure(reference, output, region.reference, region.output)?,
        });
    }
    let accepted = match binding.request.mode {
        FidelityMode::Exact => Some(
            whole_frame.changed_pixels == 0
                && regions.iter().all(|r| r.metrics.changed_pixels == 0),
        ),
        FidelityMode::Measure => None,
        FidelityMode::NearMatch => binding.request.limits.as_ref().map(|limits| {
            let passes = |m: &PixelMetrics| {
                m.changed_pixels <= limits.max_changed_pixels
                    && m.max_channel_delta <= limits.max_channel_delta
                    && m.total_channel_error <= limits.max_total_channel_error
            };
            passes(&whole_frame) && regions.iter().all(|r| passes(&r.metrics))
        }),
    };
    let metrics = ComparisonMetrics {
        whole_frame,
        regions,
    };
    // One diagnostic RGBA buffer at a time. Failure to encode diagnostics must not hide measurements.
    let mut diagnostic = vec![0; reference.pixels.len()];
    for ((a, b), dst) in reference
        .pixels
        .chunks_exact(4)
        .zip(output.pixels.chunks_exact(4))
        .zip(diagnostic.chunks_exact_mut(4))
    {
        dst.copy_from_slice(&[
            a[0].abs_diff(b[0]),
            a[1].abs_diff(b[1]),
            a[2].abs_diff(b[2]).max(a[3].abs_diff(b[3])),
            255,
        ]);
    }
    let diff = tool_media::encode_rgba_png(&diagnostic, reference.width, reference.height);
    for ((a, b), dst) in reference
        .pixels
        .chunks_exact(4)
        .zip(output.pixels.chunks_exact(4))
        .zip(diagnostic.chunks_exact_mut(4))
    {
        if a == b {
            dst.copy_from_slice(b);
        } else {
            dst.copy_from_slice(&[255, b[1] / 2, b[2] / 2, 255]);
        }
    }
    let overlay = tool_media::encode_rgba_png(&diagnostic, reference.width, reference.height);
    let artifact_note = (diff.is_err() || overlay.is_err()).then(|| "Diagnostic PNG exceeds bounded encoder capacity; numerical measurements remain available.".into());
    Ok(ComparisonResult {
        receipt: ComparisonReceipt {
            binding,
            metrics,
            accepted,
        },
        diff_png: diff.ok(),
        overlay_png: overlay.ok(),
        artifact_note,
        _memory: None,
    })
}

fn bounds(rect: PixelRect, image: &RgbaImage) -> Result<()> {
    ensure!(
        rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|v| v <= image.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|v| v <= image.height),
        "comparison region is outside image bounds"
    );
    Ok(())
}
fn measure(a: &RgbaImage, b: &RgbaImage, ar: PixelRect, br: PixelRect) -> Result<PixelMetrics> {
    bounds(ar, a)?;
    bounds(br, b)?;
    ensure!(
        ar.width == br.width && ar.height == br.height,
        "region alignment must be explicit and equal-sized"
    );
    let mut changed_pixels = 0;
    let mut total_channel_error = 0;
    let mut max_channel_delta = 0;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..ar.height {
        for x in 0..ar.width {
            let ai = (((ar.y + y) as usize * a.width as usize) + (ar.x + x) as usize) * 4;
            let bi = (((br.y + y) as usize * b.width as usize) + (br.x + x) as usize) * 4;
            let pa = &a.pixels[ai..ai + 4];
            let pb = &b.pixels[bi..bi + 4];
            if pa != pb {
                changed_pixels += 1;
                x0 = x0.min(ar.x + x);
                y0 = y0.min(ar.y + y);
                x1 = x1.max(ar.x + x);
                y1 = y1.max(ar.y + y);
            }
            for (ca, cb) in pa.iter().zip(pb) {
                let delta = ca.abs_diff(*cb);
                total_channel_error += u64::from(delta);
                max_channel_delta = max_channel_delta.max(delta);
            }
        }
    }
    let pixels = u64::from(ar.width) * u64::from(ar.height);
    Ok(PixelMetrics {
        pixels,
        changed_pixels,
        changed_fraction: changed_pixels as f64 / pixels as f64,
        total_channel_error,
        mean_channel_error: total_channel_error as f64 / (pixels * 4) as f64,
        max_channel_delta,
        mismatch_bbox: (changed_pixels > 0).then(|| PixelRect {
            x: x0,
            y: y0,
            width: x1 - x0 + 1,
            height: y1 - y0 + 1,
        }),
    })
}

/// Strict supported encoding preflight; color-sensitive/animated/high-depth inputs are not altered.
pub(crate) fn validate_png_encoding(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= tool_media::MAX_IMAGE_BYTES && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "bounded PNG required"
    );
    let mut cursor: usize = 8;
    let mut saw_header = false;
    let mut saw_end = false;
    while cursor < bytes.len() {
        ensure!(bytes.len() - cursor >= 12, "truncated PNG chunk");
        let length = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        let end = cursor
            .checked_add(length)
            .and_then(|v| v.checked_add(12))
            .context("PNG chunk overflow")?;
        ensure!(end <= bytes.len(), "truncated PNG chunk data");
        let kind = &bytes[cursor + 4..cursor + 8];
        match kind {
            b"IHDR" => {
                ensure!(!saw_header && cursor == 8 && length == 13, "invalid PNG header order");
                ensure!(bytes[cursor+16] == 8, "unsupported reference PNG bit depth; retain original, supply 8-bit comparison fixture explicitly");
                let width = u32::from_be_bytes(bytes[cursor+8..cursor+12].try_into().unwrap());
                let height = u32::from_be_bytes(bytes[cursor+12..cursor+16].try_into().unwrap());
                tool_media::rgba_size(width,height)?;
                saw_header = true;
            }
            b"IDAT" | b"PLTE" | b"tRNS" => {},
            b"sRGB" => ensure!(length == 1 && bytes[cursor+8] <= 3, "invalid PNG sRGB metadata"),
            b"IEND" => { ensure!(length == 0 && end == bytes.len(), "invalid PNG trailer"); saw_end = true; },
            // Refuse arbitrary ancillary chunks, including compressed profiles and animation.
            _ => bail!("unsupported PNG metadata/animation; comparison does not silently strip color/profile information"),
        }
        cursor = end;
    }
    ensure!(saw_header && saw_end, "incomplete PNG");
    Ok(())
}

pub(crate) fn decode_supported(bytes: &[u8], mime: &str) -> Result<RgbaImage> {
    ensure!(
        bytes.len() <= tool_media::MAX_IMAGE_BYTES,
        "encoded reference exceeds 8 MiB"
    );
    ensure!(
        tool_media::sniff_mime(bytes) == Some(mime),
        "reference MIME/magic mismatch"
    );
    match mime {
        "image/png" => {
            validate_png_encoding(bytes)?;
            let (info, decoded) = tool_media::decode_png(bytes)?;
            let channels = match info.color_type { png::ColorType::Grayscale => 1, png::ColorType::GrayscaleAlpha => 2, png::ColorType::Rgb => 3, png::ColorType::Rgba => 4, _ => bail!("unsupported expanded PNG color") };
            let mut pixels = vec![0; tool_media::rgba_size(info.width, info.height)?];
            for (source, target) in decoded.chunks_exact(channels).zip(pixels.chunks_exact_mut(4)) {
                let rgba = match channels { 1 => [source[0],source[0],source[0],255], 2 => [source[0],source[0],source[0],source[1]], 3 => [source[0],source[1],source[2],255], _ => [source[0],source[1],source[2],source[3]] };
                target.copy_from_slice(&rgba);
            }
            Ok(RgbaImage { width:info.width, height:info.height, pixels })
        }
        "image/jpeg" => {
            validate_jpeg_encoding(bytes)?;
            use image::ImageDecoder;
            let mut decoder = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(bytes))?;
            let (width,height) = decoder.dimensions();
            let size = tool_media::rgba_size(width,height)?;
            let mut limits = image::Limits::default(); limits.max_alloc = Some(tool_media::MAX_PNG_DECODE_BYTES as u64);
            decoder.set_limits(limits)?;
            ensure!(decoder.color_type() == image::ColorType::Rgb8 && decoder.total_bytes() <= tool_media::MAX_PNG_DECODE_BYTES as u64, "only opaque RGB8 JPEG comparison is supported");
            let mut rgb = vec![0; decoder.total_bytes() as usize];
            decoder.read_image(&mut rgb)?;
            let mut pixels = vec![0; size];
            for (p,t) in rgb.chunks_exact(3).zip(pixels.chunks_exact_mut(4)) { t.copy_from_slice(&[p[0],p[1],p[2],255]); }
            Ok(RgbaImage { width,height,pixels })
        }
        _ => bail!("unsupported reference comparison format; use explicit static 8-bit PNG or opaque RGB8 JPEG"),
    }
}

fn validate_jpeg_encoding(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.starts_with(&[0xff, 0xd8]), "invalid JPEG");
    let mut i = 2;
    let mut baseline = false;
    while i < bytes.len() {
        ensure!(bytes[i] == 0xff, "invalid JPEG marker");
        while i < bytes.len() && bytes[i] == 0xff {
            i += 1;
        }
        ensure!(i < bytes.len(), "truncated JPEG marker");
        let marker = bytes[i];
        i += 1;
        if marker == 0xd9 {
            break;
        }
        ensure!(
            !(0xd0..=0xd8).contains(&marker) && marker != 0x01,
            "unexpected JPEG standalone marker"
        );
        ensure!(i + 2 <= bytes.len(), "truncated JPEG segment");
        let len = u16::from_be_bytes(bytes[i..i + 2].try_into().unwrap()) as usize;
        ensure!(
            len >= 2 && i.checked_add(len).is_some_and(|end| end <= bytes.len()),
            "invalid JPEG segment length"
        );
        let data = &bytes[i + 2..i + len];
        match marker {
            0xe0 => ensure!(data.starts_with(b"JFIF\0"), "unsupported JPEG metadata"),
            0xe1..=0xef | 0xfe => {
                bail!("unsupported JPEG color/orientation metadata; no silent color conversion")
            }
            0xc0 => {
                ensure!(
                    !baseline && data.len() == 15 && data[0] == 8 && data[5] == 3,
                    "only baseline 8-bit RGB JPEG supported"
                );
                tool_media::rgba_size(
                    u16::from_be_bytes(data[3..5].try_into().unwrap()) as u32,
                    u16::from_be_bytes(data[1..3].try_into().unwrap()) as u32,
                )?;
                baseline = true;
            }
            0xda => {
                ensure!(
                    baseline && data.len() == 10 && data[0] == 3,
                    "unsupported non-baseline/multi-scan JPEG"
                );
                // Prove all trailing bytes are scan data/restart markers + final EOI. Profiles or
                // orientation segments after a scan are not silently accepted by a lenient codec.
                i += len;
                while i < bytes.len() {
                    if bytes[i] != 0xff {
                        i += 1;
                        continue;
                    }
                    i += 1;
                    while i < bytes.len() && bytes[i] == 0xff {
                        i += 1;
                    }
                    ensure!(i < bytes.len(), "truncated JPEG scan marker");
                    let marker = bytes[i];
                    i += 1;
                    match marker {
                        0x00 | 0xd0..=0xd7 => {}
                        0xd9 => {
                            ensure!(i == bytes.len(), "JPEG trailing metadata/bytes unsupported");
                            return Ok(());
                        }
                        _ => bail!("JPEG additional scans/metadata unsupported"),
                    }
                }
                bail!("JPEG scan missing terminal EOI");
            }
            0xc4 | 0xdb | 0xdd => {}
            _ => bail!("unsupported JPEG coding/metadata"),
        }
        i += len;
    }
    bail!("incomplete JPEG reference")
}

/// Field order drops materialized encoded buffers before admission on every error path.
pub(crate) struct EncodedInputs {
    pub(crate) reference: Vec<u8>,
    pub(crate) output: Vec<u8>,
    pub(crate) budget: Option<WorkBudget>,
}
pub(crate) fn compare_owned(
    inputs: &mut EncodedInputs,
    reference_mime: &str,
    output_mime: &str,
    binding: ComparisonBinding,
) -> Result<ComparisonResult> {
    let budget = inputs
        .budget
        .as_mut()
        .context("comparison admission missing")?;
    let mut result = compare_admitted_inner(
        &inputs.reference,
        reference_mime,
        &inputs.output,
        output_mime,
        binding,
        budget,
    )?;
    drop(std::mem::take(&mut inputs.output));
    drop(std::mem::take(&mut inputs.reference));
    result._memory = inputs.budget.take();
    Ok(result)
}

/// Fixture entry point; production admission precedes materialization through compare_owned.
#[cfg(test)]
pub fn compare_encoded(
    reference: &[u8],
    reference_mime: &str,
    output: &[u8],
    output_mime: &str,
    binding: ComparisonBinding,
) -> Result<ComparisonResult> {
    let encoded = reference
        .len()
        .checked_add(output.len())
        .context("comparison encoded length overflow")?;
    let budget = WorkBudget::acquire(encoded)?;
    compare_encoded_admitted(
        reference,
        reference_mime,
        output,
        output_mime,
        binding,
        budget,
    )
}

#[cfg(test)]
fn compare_encoded_admitted(
    reference: &[u8],
    reference_mime: &str,
    output: &[u8],
    output_mime: &str,
    binding: ComparisonBinding,
    mut budget: WorkBudget,
) -> Result<ComparisonResult> {
    let mut result = compare_admitted_inner(
        reference,
        reference_mime,
        output,
        output_mime,
        binding,
        &mut budget,
    )?;
    result._memory = Some(budget);
    Ok(result)
}
fn compare_admitted_inner(
    reference: &[u8],
    reference_mime: &str,
    output: &[u8],
    output_mime: &str,
    binding: ComparisonBinding,
    budget: &mut WorkBudget,
) -> Result<ComparisonResult> {
    binding.validate()?;
    ensure!(
        reference.len() <= tool_media::MAX_IMAGE_BYTES
            && output.len() <= tool_media::MAX_IMAGE_BYTES,
        "encoded comparison image exceeds 8 MiB"
    );
    ensure!(
        tool_media::sha256(reference) == binding.reference_sha256
            && tool_media::sha256(output) == binding.output_sha256,
        "comparison byte hashes do not match trusted original/capture binding"
    );
    let ref_size = encoded_rgba_size(reference, reference_mime)?;
    let out_size = encoded_rgba_size(output, output_mime)?;
    // Pre-reserve the worst simultaneous decode/normalize or measurement/diagnostic peak.
    let decode_peak =
        (2 * ref_size).max(ref_size + 2 * out_size) + tool_media::MAX_PNG_DECODE_BYTES;
    // Allow old/new encoder capacity to overlap during bounded reserve_exact growth, plus
    // encoder scratch. Tiny images still allocate incrementally, not fixed 8 MiB buffers.
    let measure_peak = 2 * ref_size + out_size + 32 * 1024 * 1024;
    // Diagnostic encoding plus retained base64 peak; transfer actual capacities after work.
    let retained_base64 = 2 * tool_media::MAX_IMAGE_BYTES.div_ceil(3) * 4;
    let validation_peak = ref_size
        + out_size
        + 2 * tool_media::MAX_IMAGE_BYTES
        + retained_base64
        + tool_media::MAX_PNG_DECODE_BYTES;
    budget.ensure_total(
        decode_peak.max(measure_peak).max(validation_peak) + reference.len() + output.len(),
    )?;
    let reference = decode_supported(reference, reference_mime)?;
    let output = decode_supported(output, output_mime)?;
    reference.validate()?;
    output.validate()?;
    compare_inner(&reference, &output, binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(width: u32, height: u32) -> ComparisonBinding {
        ComparisonBinding {
            reference_id: "ref".into(),
            reference_sha256: "a".repeat(64),
            output_evidence_id: "out".into(),
            output_sha256: "b".repeat(64),
            source_hash: "source".into(),
            render_revision: 0,
            target: "local".into(),
            viewport: Viewport { width, height },
            dpr_milli: 1000,
            font_identity: "font".into(),
            frame_state: "settled".into(),
            request: ComparisonRequest {
                mode: FidelityMode::Exact,
                ..Default::default()
            },
        }
    }
    #[test]
    fn owned_encoded_inputs_transfer_diagnostic_leases_through_last_clone() {
        use crate::llm::image_send::{try_image_memory_lease, IMAGE_MEMORY_BUDGET};
        let _lock = crate::llm::image_send::image_test_lock();
        let png = tool_media::encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap();
        let mut bind = binding(1, 1);
        bind.reference_sha256 = tool_media::sha256(&png);
        bind.output_sha256 = bind.reference_sha256.clone();
        let budget = WorkBudget::acquire(2 * png.len()).unwrap();
        let mut inputs = EncodedInputs {
            reference: png.clone(),
            output: png.clone(),
            budget: Some(budget),
        };
        let mut result = compare_owned(&mut inputs, "image/png", "image/png", bind).unwrap();
        assert_eq!(result.receipt.accepted(), Some(true));
        assert_eq!(
            (inputs.reference.capacity(), inputs.output.capacity()),
            (0, 0)
        );
        assert!(inputs.budget.is_none());
        let images = result.diagnostic_images().unwrap();
        assert_eq!(images.len(), 2);
        assert!(result.diff_png.is_none() && result.overlay_png.is_none());
        let clone = images[0].clone();
        let payload_capacity = clone.metadata().byte_len.div_ceil(3) * 4;
        let metadata_only = clone.without_payload(tool_media::ImageUnavailable::MetadataOnly);
        drop(images);
        drop(result);
        drop(inputs);

        let mut remaining = try_image_memory_lease(0).unwrap();
        let mut step = IMAGE_MEMORY_BUDGET.next_power_of_two();
        while step > 0 {
            remaining.try_grow(step);
            step /= 2;
        }
        assert!(try_image_memory_lease(1).is_none());
        drop(clone);
        assert!(
            remaining.try_grow(payload_capacity),
            "diagnostic clone retains the split lease after result drop"
        );
        assert!(
            try_image_memory_lease(1).is_none(),
            "metadata-only image must not retain payload admission"
        );
        drop(remaining);
        assert!(!metadata_only.is_available());
    }

    #[test]
    fn split_then_grow_tracks_only_the_remaining_work_reservation() {
        let _lock = crate::llm::image_send::image_test_lock();
        let mut budget = WorkBudget::acquire(8).unwrap();
        let payload = budget.split(3).unwrap();
        assert_eq!(budget.lease.bytes(), 5);
        assert_eq!(payload.bytes(), 3);
        budget.ensure_total(8).unwrap();
        assert_eq!(
            budget.lease.bytes(),
            8,
            "growth is based on actual remaining lease"
        );
        assert!(budget.split(9).is_err());
    }

    #[test]
    fn owned_encoded_inputs_keep_admission_until_failed_input_storage_drops() {
        use crate::llm::image_send::{try_image_memory_lease, IMAGE_MEMORY_BUDGET};
        let _lock = crate::llm::image_send::image_test_lock();
        let budget = WorkBudget::acquire(6).unwrap();
        let mut inputs = EncodedInputs {
            reference: b"bad".to_vec(),
            output: b"bad".to_vec(),
            budget: Some(budget),
        };
        assert!(compare_owned(&mut inputs, "image/png", "image/png", binding(1, 1)).is_err());
        assert_eq!(inputs.budget.as_ref().unwrap().lease.bytes(), 6);
        assert_eq!(inputs.reference, b"bad");
        let mut remaining = try_image_memory_lease(0).unwrap();
        let mut step = IMAGE_MEMORY_BUDGET.next_power_of_two();
        while step > 0 {
            remaining.try_grow(step);
            step /= 2;
        }
        assert!(try_image_memory_lease(1).is_none());
        drop(inputs);
        assert!(remaining.try_grow(6));
        assert!(try_image_memory_lease(1).is_none());
    }

    #[test]
    fn equality_one_pixel_and_alpha_have_exact_zero_semantics() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 0, 255, 0, 0, 0, 255],
        };
        let same = compare_rgba(&a, &a, binding(2, 1)).unwrap();
        assert_eq!(same.receipt.accepted(), Some(true));
        assert_eq!(same.receipt.metrics.whole_frame.mismatch_bbox, None);
        let mut b = a.clone();
        b.pixels[7] = 254;
        let diff = compare_rgba(&a, &b, binding(2, 1)).unwrap();
        assert_eq!(diff.receipt.accepted(), Some(false));
        assert_eq!(diff.receipt.metrics.whole_frame.changed_fraction, 0.5);
        assert_eq!(diff.receipt.metrics.whole_frame.total_channel_error, 1);
        assert_eq!(
            diff.receipt.metrics.whole_frame.mismatch_bbox,
            Some(PixelRect {
                x: 1,
                y: 0,
                width: 1,
                height: 1
            })
        );
        assert!(diff.diff_png.is_some() && diff.overlay_png.is_some());
    }
    #[test]
    fn no_alignment_threshold_or_region_exclusion_is_invented() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![0; 8],
        };
        let mut b = a.clone();
        b.pixels[4] = 1;
        let mut bind = binding(2, 1);
        bind.request.mode = FidelityMode::NearMatch;
        assert_eq!(
            compare_rgba(&a, &b, bind.clone())
                .unwrap()
                .receipt
                .accepted(),
            None
        );
        bind.request.mode = FidelityMode::Exact;
        bind.request.regions.push(ComparisonRegion {
            name: "passing crop".into(),
            reference: PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            output: PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        });
        let result = compare_rgba(&a, &b, bind.clone()).unwrap();
        assert_eq!(result.receipt.metrics.regions[0].metrics.changed_pixels, 0);
        assert_eq!(result.receipt.accepted(), Some(false));
        bind.dpr_milli = 2000;
        assert!(compare_rgba(&a, &b, bind).is_err());
        b.width = 1;
        b.height = 2;
        assert!(compare_rgba(&a, &b, binding(2, 1)).is_err());
    }
    #[test]
    fn strict_png_and_hash_binding() {
        let _lock = crate::llm::image_send::image_test_lock();
        let png = tool_media::encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap();
        let mut bind = binding(1, 1);
        bind.reference_sha256 = tool_media::sha256(&png);
        bind.output_sha256 = bind.reference_sha256.clone();
        assert_eq!(
            compare_encoded(&png, "image/png", &png, "image/png", bind.clone())
                .unwrap()
                .receipt
                .accepted(),
            Some(true)
        );
        bind.output_sha256 = "0".repeat(64);
        assert!(compare_encoded(&png, "image/png", &png, "image/png", bind).is_err());
        let mut high = png.clone();
        high[24] = 16;
        assert!(validate_png_encoding(&high).is_err());
        let mut bomb = png.clone();
        bomb[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(validate_png_encoding(&bomb).is_err());
        assert!(validate_png_encoding(&png[..png.len() - 1]).is_err());
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 88)
            .encode(&[1, 2, 3], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!(validate_jpeg_encoding(&jpeg).is_ok());
        let mut jpeg_binding = binding(1, 1);
        jpeg_binding.reference_sha256 = tool_media::sha256(&jpeg);
        jpeg_binding.output_sha256 = tool_media::sha256(&jpeg);
        assert_eq!(
            compare_encoded(&jpeg, "image/jpeg", &jpeg, "image/jpeg", jpeg_binding)
                .unwrap()
                .receipt
                .accepted(),
            Some(true)
        );
        let mut metadata = jpeg.clone();
        metadata.splice(2..2, [0xff, 0xe1, 0, 4, 0, 0]);
        assert!(validate_jpeg_encoding(&metadata).is_err());
        assert!(validate_jpeg_encoding(&jpeg[..jpeg.len() - 2]).is_err());
        let mut trailer = jpeg.clone();
        trailer.push(0);
        assert!(validate_jpeg_encoding(&trailer).is_err());
    }
    #[test]
    fn integer_limits_are_only_explicit_near_match() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        };
        let b = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![1, 0, 0, 0],
        };
        let mut bind = binding(1, 1);
        bind.request.mode = FidelityMode::NearMatch;
        bind.request.limits = Some(ComparisonLimits {
            max_changed_pixels: 1,
            max_channel_delta: 1,
            max_total_channel_error: 1,
        });
        assert_eq!(
            compare_rgba(&a, &b, bind.clone())
                .unwrap()
                .receipt
                .accepted(),
            Some(true)
        );
        bind.request.limits.as_mut().unwrap().max_channel_delta = 0;
        assert_eq!(
            compare_rgba(&a, &b, bind).unwrap().receipt.accepted(),
            Some(false)
        );
    }
}
