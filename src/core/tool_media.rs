//! Bounded, validated image payloads. Metadata and bytes travel as one value; payloads never
//! appear in Debug output. Only PNG is decoded locally (other supported formats are magic-checked).

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REQUEST_IMAGES: usize = 4;
pub const MAX_REQUEST_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PNG_DECODE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageProvenance {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<PixelRect>,
}

impl ImageProvenance {
    pub fn new(source: impl Into<String>, locator: Option<String>) -> Self {
        Self {
            source: source.into(),
            evidence_id: None,
            design_scope: None,
            approved_destination: None,
            locator,
            parent_sha256: None,
            crop: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMetadata {
    pub sha256: String,
    pub mime: String,
    pub byte_len: usize,
    /// Known only for a PNG whose bounded decode succeeded. Not guessed from file extensions.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub provenance: ImageProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageUnavailable {
    Missing,
    Oversize,
    Corrupt,
    Retention,
    MetadataOnly,
}

/// Clones share the actual encoded allocation and its optional weighted owner.
/// Data is declared first so its storage drops before the reservation is released.
struct SharedImagePayload {
    data: String,
    _memory: Option<crate::llm::image_send::ImageMemoryLease>,
}
#[derive(Clone)]
struct ImagePayload(std::sync::Arc<SharedImagePayload>);
impl ImagePayload {
    fn new(data: String, memory: Option<crate::llm::image_send::ImageMemoryLease>) -> Self {
        Self(std::sync::Arc::new(SharedImagePayload {
            data,
            _memory: memory,
        }))
    }
    fn is_empty(&self) -> bool {
        self.0.data.is_empty()
    }
}
impl std::ops::Deref for ImagePayload {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0.data
    }
}
impl std::fmt::Display for ImagePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.data)
    }
}
impl PartialEq for ImagePayload {
    fn eq(&self, other: &Self) -> bool {
        self.0.data == other.0.data
    }
}
impl Eq for ImagePayload {}
impl Serialize for ImagePayload {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.data)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct ToolImage {
    #[serde(skip_serializing_if = "ImagePayload::is_empty")]
    base64: ImagePayload,
    metadata: ImageMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unavailable: Option<ImageUnavailable>,
}

impl std::fmt::Debug for ToolImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolImage")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for ToolImage {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(default)]
            base64: String,
            metadata: ImageMetadata,
            #[serde(default)]
            unavailable: Option<ImageUnavailable>,
        }
        let stored = Stored::deserialize(de)?;
        if let Some(reason) = stored.unavailable {
            return Ok(Self::unavailable(stored.metadata, reason));
        }
        if stored.base64.is_empty() {
            return Ok(Self::unavailable(
                stored.metadata,
                ImageUnavailable::Missing,
            ));
        }
        match Self::from_base64(
            &stored.base64,
            &stored.metadata.mime,
            stored.metadata.provenance.clone(),
        ) {
            Ok(image) if image.metadata == stored.metadata => Ok(image),
            _ => Ok(Self::unavailable(
                stored.metadata,
                ImageUnavailable::Corrupt,
            )),
        }
    }
}

impl ToolImage {
    pub fn from_bytes(bytes: &[u8], mime: &str, provenance: ImageProvenance) -> Result<Self> {
        Self::from_bytes_owned(bytes, mime, provenance, None)
    }
    /// The caller reserves payload capacity before entering; validation/decode work remains held
    /// by its admitted work guard. No lease is serialized or considered in image equality.
    pub(crate) fn from_bytes_with_memory(
        bytes: &[u8],
        mime: &str,
        provenance: ImageProvenance,
        memory: crate::llm::image_send::ImageMemoryLease,
    ) -> Result<Self> {
        Self::from_bytes_owned(bytes, mime, provenance, Some(memory))
    }
    fn from_bytes_owned(
        bytes: &[u8],
        mime: &str,
        provenance: ImageProvenance,
        memory: Option<crate::llm::image_send::ImageMemoryLease>,
    ) -> Result<Self> {
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "image exceeds 8 MiB limit");
        ensure!(!bytes.is_empty(), "empty image");
        if let Some(owner) = &memory {
            let capacity = bytes
                .len()
                .div_ceil(3)
                .checked_mul(4)
                .context("base64 payload capacity overflow")?;
            ensure!(
                owner.bytes() >= capacity,
                "image payload lacks admitted base64 capacity"
            );
        }
        let actual =
            sniff_mime(bytes).context("unsupported image magic (PNG/JPEG/GIF/WebP only)")?;
        ensure!(mime == actual, "image MIME does not match its magic bytes");
        ensure!(
            provenance
                .evidence_id
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 128)
                && provenance
                    .design_scope
                    .as_ref()
                    .is_none_or(|s| !s.is_empty() && s.len() <= 128)
                && provenance
                    .approved_destination
                    .as_ref()
                    .is_none_or(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid image evidence identity"
        );
        ensure!(
            provenance.source.len() <= 128
                && provenance.locator.as_ref().is_none_or(|s| s.len() <= 2048)
                && provenance
                    .parent_sha256
                    .as_ref()
                    .is_none_or(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid image provenance"
        );
        let (width, height) = if actual == "image/png" {
            let (info, _) = decode_png(bytes)?;
            (Some(info.width), Some(info.height))
        } else {
            (None, None)
        };
        Ok(Self {
            unavailable: None,
            base64: ImagePayload::new(base64_encode(bytes), memory),
            metadata: ImageMetadata {
                sha256: sha256(bytes),
                mime: actual.into(),
                byte_len: bytes.len(),
                width,
                height,
                provenance,
            },
        })
    }

    pub fn from_base64(data: &str, mime: &str, provenance: ImageProvenance) -> Result<Self> {
        let bytes = base64_decode_bounded(data, MAX_IMAGE_BYTES)?;
        Self::from_bytes(&bytes, mime, provenance)
    }

    pub fn from_data_url(url: &str, provenance: ImageProvenance) -> Result<Self> {
        let (mime, data) = split_data_url(url)?;
        Self::from_base64(data, mime, provenance)
    }

    pub fn from_file(path: &Path, provenance: ImageProvenance) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("reading image {}", path.display()))?;
        ensure!(
            file.metadata()?.len() <= MAX_IMAGE_BYTES as u64,
            "image exceeds 8 MiB limit"
        );
        // Bound the read too: metadata can race a growing file or lie for a virtual file.
        let mut bytes = Vec::new();
        file.take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "image exceeds 8 MiB limit");
        let mime = sniff_mime(&bytes).context("not a PNG/JPEG/GIF/WebP image")?;
        Self::from_bytes(&bytes, mime, provenance)
    }

    pub fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }
    pub fn with_approved_destination(&self, destination: impl Into<String>) -> Self {
        let destination = destination.into();
        // A historical approval is not transferrable to a different endpoint/model. The executor
        // must obtain fresh evidence/approval rather than silently rebinding an old observation.
        if self
            .metadata
            .provenance
            .approved_destination
            .as_ref()
            .is_some_and(|old| old != &destination)
        {
            return self.clone();
        }
        let mut image = self.clone();
        image.metadata.provenance.approved_destination = Some(destination);
        image
    }
    pub fn is_available(&self) -> bool {
        self.unavailable.is_none() && !self.base64.is_empty()
    }
    pub fn unavailable_reason(&self) -> Option<ImageUnavailable> {
        self.unavailable
    }
    pub fn unavailable(metadata: ImageMetadata, reason: ImageUnavailable) -> Self {
        Self {
            base64: ImagePayload::new(String::new(), None),
            metadata,
            unavailable: Some(reason),
        }
    }
    pub fn without_payload(&self, reason: ImageUnavailable) -> Self {
        Self::unavailable(self.metadata.clone(), reason)
    }
    pub fn hydrate(&self, bytes: &[u8]) -> Result<Self> {
        let image = Self::from_bytes(bytes, &self.metadata.mime, self.metadata.provenance.clone())?;
        ensure!(
            image.metadata == self.metadata,
            "image metadata does not match its payload"
        );
        Ok(image)
    }
    pub fn data_url(&self) -> String {
        if !self.is_available() {
            return String::new();
        }
        format!("data:{};base64,{}", self.metadata.mime, self.base64)
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            self.is_available(),
            "image payload unavailable: {:?}",
            self.unavailable
        );
        base64_decode_bounded(&self.base64, MAX_IMAGE_BYTES)
    }

    /// Crop in PNG pixel coordinates. Bounds are checked before output allocation; non-PNGs
    /// explicitly fail rather than claiming a decoded/cropped JPEG/GIF/WebP.
    pub fn crop_png(&self, rect: PixelRect) -> Result<Self> {
        ensure!(
            self.metadata.mime == "image/png",
            "local crop supports PNG only"
        );
        let bytes = self.bytes()?;
        let (info, pixels) = decode_png(&bytes)?;
        ensure!(
            rect.width > 0
                && rect.height > 0
                && rect
                    .x
                    .checked_add(rect.width)
                    .is_some_and(|x| x <= info.width)
                && rect
                    .y
                    .checked_add(rect.height)
                    .is_some_and(|y| y <= info.height),
            "crop is outside image bounds"
        );
        let channels = match info.color_type {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            _ => bail!("unsupported PNG color type after expansion"),
        };
        ensure!(
            info.bit_depth == png::BitDepth::Eight,
            "unsupported PNG bit depth after expansion"
        );
        let size = rgba_size(rect.width, rect.height)?;
        let mut out = vec![0u8; size];
        for y in 0..rect.height as usize {
            for x in 0..rect.width as usize {
                let src =
                    ((rect.y as usize + y) * info.width as usize + rect.x as usize + x) * channels;
                let dst = (y * rect.width as usize + x) * 4;
                let p = &pixels[src..src + channels];
                let rgba = match channels {
                    1 => [p[0], p[0], p[0], 255],
                    2 => [p[0], p[0], p[0], p[1]],
                    3 => [p[0], p[1], p[2], 255],
                    _ => [p[0], p[1], p[2], p[3]],
                };
                out[dst..dst + 4].copy_from_slice(&rgba);
            }
        }
        let png = encode_rgba_png(&out, rect.width, rect.height)?;
        let mut provenance = self.metadata.provenance.clone();
        provenance.parent_sha256 = Some(self.metadata.sha256.clone());
        provenance.crop = Some(rect);
        Self::from_bytes(&png, "image/png", provenance)
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn split_data_url(url: &str) -> Result<(&str, &str)> {
    let rest = url
        .strip_prefix("data:")
        .context("image must be a base64 data URL")?;
    let (mime, data) = rest
        .split_once(";base64,")
        .context("image must be a base64 data URL")?;
    ensure!(
        matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ),
        "unsupported image MIME"
    );
    Ok((mime, data))
}

pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or(0) as usize;
        let c = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(T[a >> 2] as char);
        out.push(T[((a & 3) << 4) | (b >> 4)] as char);
        out.push(if chunk.len() > 1 {
            T[((b & 15) << 2) | (c >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[c & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Strict standard, padded base64. Validate size, alphabet, padding and unused bits BEFORE
/// allocating decoded bytes. Whitespace and non-canonical encodings are deliberately rejected.
pub fn base64_decode_bounded(data: &str, max: usize) -> Result<Vec<u8>> {
    let size = base64_decoded_len(data, max)?;
    let mut out = Vec::with_capacity(size);
    for c in data.as_bytes().chunks_exact(4) {
        let a = sextet(c[0]).unwrap();
        let b = sextet(c[1]).unwrap();
        let d = sextet(c[2]).unwrap_or(0);
        let e = sextet(c[3]).unwrap_or(0);
        out.push((a << 2) | (b >> 4));
        if c[2] != b'=' {
            out.push((b << 4) | (d >> 2));
        }
        if c[3] != b'=' {
            out.push((d << 6) | e);
        }
    }
    Ok(out)
}

pub fn base64_decoded_len(data: &str, max: usize) -> Result<usize> {
    ensure!(
        data.len() <= max.div_ceil(3).saturating_mul(4),
        "base64 image exceeds byte limit"
    );
    ensure!(data.len().is_multiple_of(4), "invalid base64 length");
    if data.is_empty() {
        return Ok(0);
    }
    let b = data.as_bytes();
    let padding = if b[b.len() - 2] == b'=' {
        2
    } else if b[b.len() - 1] == b'=' {
        1
    } else {
        0
    };
    let size = (b.len() / 4)
        .checked_mul(3)
        .and_then(|n| n.checked_sub(padding))
        .context("invalid base64 length")?;
    ensure!(size <= max, "base64 image exceeds byte limit");
    ensure!(
        b[..b.len() - padding].iter().all(|c| sextet(*c).is_some())
            && b[b.len() - padding..].iter().all(|c| *c == b'='),
        "invalid base64 alphabet or padding"
    );
    if padding == 2 {
        ensure!(
            sextet(b[b.len() - 3]).unwrap() & 15 == 0,
            "non-canonical base64 padding bits"
        );
    }
    if padding == 1 {
        ensure!(
            sextet(b[b.len() - 2]).unwrap() & 3 == 0,
            "non-canonical base64 padding bits"
        );
    }
    Ok(size)
}

fn sextet(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn rgba_size(width: u32, height: u32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "zero image dimensions");
    let size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("image dimensions overflow")?;
    ensure!(
        size <= MAX_PNG_DECODE_BYTES,
        "decoded PNG exceeds 64 MiB limit"
    );
    Ok(size)
}

pub(crate) fn decode_png(bytes: &[u8]) -> Result<(png::OutputInfo, Vec<u8>)> {
    // Check IHDR before even read_info (which can expand compressed ancillary chunks).
    ensure!(
        bytes.len() >= 33
            && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            && bytes[8..12] == [0, 0, 0, 13]
            && &bytes[12..16] == b"IHDR",
        "invalid PNG header"
    );
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    rgba_size(width, height)?;
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(bytes),
        png::Limits {
            bytes: MAX_PNG_DECODE_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("invalid PNG")?;
    let size = reader.output_buffer_size();
    ensure!(
        size <= MAX_PNG_DECODE_BYTES,
        "decoded PNG exceeds 64 MiB limit"
    );
    let mut pixels = vec![0; size];
    let info = reader
        .next_frame(&mut pixels)
        .context("invalid PNG pixels")?;
    reader.finish().context("invalid PNG trailer")?;
    pixels.truncate(info.buffer_size());
    Ok((info, pixels))
}

pub fn encode_rgba_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    ensure!(
        rgba.len() == rgba_size(width, height)?,
        "RGBA buffer does not match dimensions"
    );
    struct BoundedOutput(Vec<u8>);
    impl std::io::Write for BoundedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let end = self.0.len().saturating_add(bytes.len());
            if end > MAX_IMAGE_BYTES {
                return Err(std::io::Error::other("encoded PNG exceeds 8 MiB limit"));
            }
            if end > self.0.capacity() {
                // Do not let Vec's implicit geometric growth exceed the admitted encoded cap.
                let capacity = end
                    .max(self.0.capacity().saturating_mul(2))
                    .min(MAX_IMAGE_BYTES);
                self.0
                    .try_reserve_exact(capacity - self.0.len())
                    .map_err(std::io::Error::other)?;
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = BoundedOutput(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(rgba)?;
    }
    Ok(out.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn image() -> ToolImage {
        ToolImage::from_bytes(
            &encode_rgba_png(&[255, 0, 0, 255, 0, 0, 255, 255], 2, 1).unwrap(),
            "image/png",
            ImageProvenance::new("test", None),
        )
        .unwrap()
    }
    #[test]
    fn shared_owned_payload_clones_preserve_string_wire_and_release_only_at_last_drop() {
        use crate::llm::image_send::{try_image_memory_lease, IMAGE_MEMORY_BUDGET};
        let _lock = crate::llm::image_send::image_test_lock();
        let bytes = encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap();
        let capacity = bytes.len().div_ceil(3) * 4;
        let provenance = ImageProvenance::new("reference_diff", None);
        let plain = ToolImage::from_bytes(&bytes, "image/png", provenance.clone()).unwrap();
        let owner = try_image_memory_lease(capacity).unwrap();
        let owned =
            ToolImage::from_bytes_with_memory(&bytes, "image/png", provenance, owner).unwrap();
        let clone = owned.clone();
        assert!(std::sync::Arc::ptr_eq(&owned.base64.0, &clone.base64.0));
        assert_eq!(owned, plain, "equality ignores memory ownership");
        let wire = serde_json::to_value(&owned).unwrap();
        assert_eq!(wire, serde_json::to_value(&plain).unwrap());
        assert_eq!(wire["base64"], serde_json::json!(base64_encode(&bytes)));
        assert!(wire["base64"].is_string());
        assert_eq!(serde_json::from_value::<ToolImage>(wire).unwrap(), owned);
        let debug = format!("{owned:?}");
        assert!(!debug.contains(&base64_encode(&bytes)) && !debug.contains("base64"));
        let metadata_only = clone.without_payload(ImageUnavailable::MetadataOnly);
        assert!(metadata_only.base64.0._memory.is_none());
        assert!(serde_json::to_value(&metadata_only)
            .unwrap()
            .get("base64")
            .is_none());
        assert_eq!(metadata_only.hydrate(&bytes).unwrap(), plain);
        drop(owned);

        // Fill remaining admission even if an unrelated encoded cache holds a persistent lease.
        let mut remaining = try_image_memory_lease(0).unwrap();
        let mut step = IMAGE_MEMORY_BUDGET.next_power_of_two();
        while step > 0 {
            remaining.try_grow(step);
            step /= 2;
        }
        assert!(
            try_image_memory_lease(1).is_none(),
            "clone still owns payload reservation"
        );
        drop(clone);
        assert!(
            remaining.try_grow(capacity),
            "last actual allocation releases its reservation"
        );
        assert!(
            try_image_memory_lease(1).is_none(),
            "exactly payload capacity was freed"
        );
        drop(remaining);
        assert!(!metadata_only.is_available());
    }

    #[test]
    fn owned_payload_refuses_insufficient_reservation_before_encoding() {
        let _lock = crate::llm::image_send::image_test_lock();
        let owner = crate::llm::image_send::try_image_memory_lease(1).unwrap();
        assert!(ToolImage::from_bytes_with_memory(
            b"GIF89a-fixture",
            "image/gif",
            ImageProvenance::new("fixture", None),
            owner,
        )
        .unwrap_err()
        .to_string()
        .contains("admitted base64 capacity"));
    }

    #[test]
    fn validated_image_round_trip_and_metadata_tamper_rejected() {
        let image = image();
        let json = serde_json::to_string(&image).unwrap();
        let back: ToolImage = serde_json::from_str(&json).unwrap();
        assert_eq!(image, back);
        assert_eq!(
            (image.metadata().width, image.metadata().height),
            (Some(2), Some(1))
        );
        let mut tampered = serde_json::to_value(image).unwrap();
        tampered["metadata"]["width"] = serde_json::json!(99);
        let bad: ToolImage = serde_json::from_value(tampered).unwrap();
        assert_eq!(bad.unavailable_reason(), Some(ImageUnavailable::Corrupt));
        assert!(!bad.is_available());
    }
    #[test]
    fn strict_base64_and_bounds() {
        for value in ["A", "AA=A", "AB==", "AAB=", "AA==AAAA", "Zm 9v", "____"] {
            assert!(base64_decode_bounded(value, 64).is_err(), "{value}");
        }
        assert_eq!(base64_decode_bounded("Zm9v", 3).unwrap(), b"foo");
        assert!(base64_decode_bounded("Zm9v", 2).is_err());
        assert!(base64_decode_bounded(
            &"A".repeat(MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4),
            MAX_IMAGE_BYTES
        )
        .is_err());
    }
    #[test]
    fn mime_truncation_bomb_and_crop_bounds() {
        let image = image();
        let bytes = image.bytes().unwrap();
        assert!(
            ToolImage::from_bytes(&bytes, "image/jpeg", ImageProvenance::new("test", None))
                .is_err()
        );
        assert!(ToolImage::from_bytes(
            &bytes[..25],
            "image/png",
            ImageProvenance::new("test", None)
        )
        .is_err());
        let mut bomb = bytes.clone();
        bomb[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(
            ToolImage::from_bytes(&bomb, "image/png", ImageProvenance::new("test", None)).is_err()
        );
        assert!(image
            .crop_png(PixelRect {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 1
            })
            .is_err());
        assert!(image
            .crop_png(PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 1
            })
            .is_err());
        let cropped = image
            .crop_png(PixelRect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            })
            .unwrap();
        assert_eq!(cropped.metadata().width, Some(1));
        assert_eq!(
            cropped.metadata().provenance.parent_sha256.as_ref(),
            Some(&image.metadata().sha256)
        );
        let (_, pixels) = decode_png(&cropped.bytes().unwrap()).unwrap();
        assert_eq!(pixels, [0, 0, 255, 255]);
    }
    #[test]
    fn non_png_has_no_claimed_dimensions_or_crop() {
        let image = ToolImage::from_bytes(
            b"GIF89a-not-decoded",
            "image/gif",
            ImageProvenance::new("test", None),
        )
        .unwrap();
        assert_eq!(image.metadata().width, None);
        assert!(image
            .crop_png(PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
            .is_err());
        assert!(!format!("{image:?}").contains("base64"));
    }
}
