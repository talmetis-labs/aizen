//! Ephemeral user-image previews. Never call this for typed tool images or token estimates.
//! Source/history bytes stay untouched; all refusals retain the exact input URL.

use crate::core::tool_media::{base64_decode_bounded, base64_decoded_len};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, ExtendedColorType, RgbImage};
use std::collections::VecDeque;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

const MAX_ENCODED: usize = 8 * 1024 * 1024;
const MAX_DECODED: usize = 64 * 1024 * 1024;
const SOFT_URI_TARGET: usize = 512 * 1024;
const CACHE_ENTRIES: usize = 8;
const CACHE_BYTES: usize = 16 * 1024 * 1024;
const JPEG_PREFIX: &str = "data:image/jpeg;base64,";
const CODEC_VERSION: u8 = 1;
pub const IMAGE_MEMORY_BUDGET: usize = 192 * 1024 * 1024;
static IMAGE_MEMORY_USED: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn image_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Snapshot once when constructing a request/dispatch context, not midway through projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImageSendPolicy {
    Original,
    /// Unknown content is pixel-sensitive until an explicit compressible/photo hint is supplied.
    Auto,
    #[default]
    Balanced,
}
impl ImageSendPolicy {
    pub fn snapshot_environment() -> Self {
        let environment = std::env::var("AIZEN_IMAGE_SEND_MODE").ok();
        if environment.is_some() {
            return Self::from_environment_value(environment.as_deref());
        }
        match crate::core::cli_config::load().image_mode {
            Some(crate::core::types::ImagePreference::Original) => Self::Original,
            Some(crate::core::types::ImagePreference::Auto) => Self::Auto,
            Some(crate::core::types::ImagePreference::Photo) | None => Self::Balanced,
        }
    }

    pub fn from_environment_value(value: Option<&str>) -> Self {
        if value.is_some_and(|v| v.trim().eq_ignore_ascii_case("original")) {
            Self::Original
        } else if value.is_some_and(|v| v.trim().eq_ignore_ascii_case("auto")) {
            Self::Auto
        } else {
            Self::Balanced
        }
    }
}

/// Shared nonblocking weighted admission for image codecs AND local comparison buffers.
/// Acquire before allocation. Exhaustion means keep originals / report comparison unavailable.
#[derive(Debug)]
pub struct ImageMemoryLease {
    bytes: usize,
}
pub fn try_image_memory_lease(bytes: usize) -> Option<ImageMemoryLease> {
    reserve_memory(bytes).then(|| ImageMemoryLease { bytes })
}
fn reserve_memory(bytes: usize) -> bool {
    IMAGE_MEMORY_USED
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes)
                .filter(|n| *n <= IMAGE_MEMORY_BUDGET)
        })
        .is_ok()
}
impl ImageMemoryLease {
    pub fn try_grow(&mut self, additional: usize) -> bool {
        if !reserve_memory(additional) {
            return false;
        }
        self.bytes += additional;
        true
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    // Transfer reserved ownership to an encoded payload that may outlive its work/cache.
    pub(crate) fn split(&mut self, bytes: usize) -> Self {
        assert!(bytes <= self.bytes);
        self.bytes -= bytes;
        Self { bytes }
    }
}
impl Drop for ImageMemoryLease {
    fn drop(&mut self) {
        IMAGE_MEMORY_USED.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageTransform {
    Original,
    Jpeg { quality: u8, resized: bool },
}
/// Per-send optimizer accounting. Retained for the optimizer's own tests and callers that want the
/// numbers programmatically — it is deliberately NOT rendered as a per-request transcript line any
/// more (the user saw one line per image send and asked for it gone).
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageSendStats {
    pub encode_micros: u64,
    pub candidates_encoded: u8,
}

/// Only encoded bytes survive a call. The lease follows every Arc even after cache eviction.
pub struct ImageDerivative {
    url: Box<str>,
    identity: [u8; 32],
    source_dimensions: (u32, u32),
    dimensions: (u32, u32),
    transform: ImageTransform,
    _memory: ImageMemoryLease,
}
impl std::fmt::Debug for ImageDerivative {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageDerivative")
            .field("uri_bytes", &self.url.len())
            .field("dimensions", &self.dimensions)
            .field("transform", &self.transform)
            .finish_non_exhaustive()
    }
}
#[derive(Clone)]
pub enum ImageSendUrl<'a> {
    Original(&'a str),
    Derivative(Arc<ImageDerivative>),
}
impl std::fmt::Debug for ImageSendUrl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let diagnostics expose source URLs (which may contain secrets) or pixels.
        match self {
            Self::Original(url) => f
                .debug_struct("Original")
                .field("uri_bytes", &url.len())
                .finish(),
            Self::Derivative(image) => std::fmt::Debug::fmt(image, f),
        }
    }
}
impl ImageSendUrl<'_> {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Original(url) => url,
            Self::Derivative(image) => &image.url,
        }
    }
}
impl AsRef<str> for ImageSendUrl<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl std::ops::Deref for ImageSendUrl<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
pub struct ImageSendResult<'a> {
    pub url: ImageSendUrl<'a>,
    pub stats: ImageSendStats,
    /// SHA-256 of exact source/wire data-URL bytes, NOT the canonical blob/pixel digest.
    pub source_identity: [u8; 32],
    pub derivative_identity: [u8; 32],
    pub source_dimensions: Option<(u32, u32)>,
    pub dimensions: Option<(u32, u32)>,
    pub transform: ImageTransform,
}
impl std::fmt::Debug for ImageSendResult<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSendResult")
            .field("stats", &self.stats)
            .field("source_dimensions", &self.source_dimensions)
            .field("dimensions", &self.dimensions)
            .field("transform", &self.transform)
            .finish_non_exhaustive()
    }
}
fn identity(bytes: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .try_into()
        .expect("SHA-256 length")
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    source: [u8; 32],
    policy: ImageSendPolicy,
    version: u8,
}
#[derive(Clone)]
enum CachedImage {
    Derivative(Arc<ImageDerivative>),
    Original,
}
impl CachedImage {
    fn bytes(&self) -> usize {
        match self {
            Self::Derivative(image) => image.url.len(),
            Self::Original => 0,
        }
    }
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<(CacheKey, CachedImage)>,
    bytes: usize,
    // Weak markers need no cache lock on drop, including early returns and unwinding.
    // Limit distinct cold work too; cache contention never waits on an async executor thread.
    in_flight: Vec<(CacheKey, Weak<()>)>,
    #[cfg(test)]
    before_work: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}
impl Cache {
    fn get(&mut self, key: CacheKey) -> Option<CachedImage> {
        let index = self.entries.iter().position(|(k, _)| *k == key)?;
        let entry = self.entries.remove(index)?;
        let image = entry.1.clone();
        self.entries.push_back(entry);
        Some(image)
    }

    fn begin(&mut self, key: CacheKey) -> Option<Arc<()>> {
        self.in_flight.retain(|(_, work)| work.strong_count() != 0);
        if self.in_flight.len() >= CACHE_ENTRIES || self.in_flight.iter().any(|(k, _)| *k == key) {
            return None;
        }
        let work = Arc::new(());
        self.in_flight.push((key, Arc::downgrade(&work)));
        Some(work)
    }

    fn publish(&mut self, key: CacheKey, image: CachedImage) {
        let bytes = image.bytes();
        if bytes > CACHE_BYTES || self.entries.iter().any(|(k, _)| *k == key) {
            return;
        }
        while self.entries.len() >= CACHE_ENTRIES || self.bytes.saturating_add(bytes) > CACHE_BYTES
        {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.bytes();
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back((key, image));
    }
}
fn publish_cached(cache: &Mutex<Cache>, key: CacheKey, image: CachedImage) {
    if let Ok(mut cache) = cache.try_lock() {
        cache.publish(key, image);
    }
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Warm/negative/in-flight hits do not need a cloned source or a blocking-pool dispatch.
/// Contention falls back to original pixels, just like the optimizer's nonblocking cache access.
pub fn user_image_preparation_needed(data_url: &str, policy: ImageSendPolicy) -> bool {
    if policy != ImageSendPolicy::Balanced || !data_url.starts_with("data:image/png;base64,") {
        return false;
    }
    let key = CacheKey {
        source: identity(data_url.as_bytes()),
        policy,
        version: CODEC_VERSION,
    };
    CACHE
        .get_or_init(Mutex::default)
        .try_lock()
        .is_ok_and(|cache| {
            !cache.entries.iter().any(|(candidate, _)| *candidate == key)
                && !cache
                    .in_flight
                    .iter()
                    .any(|(candidate, work)| *candidate == key && work.strong_count() != 0)
        })
}

/// Network paths prewarm off-executor. A cold/busy cache must not start codec work in projection.
pub fn cached_user_image(data_url: &str, policy: ImageSendPolicy) -> ImageSendResult<'_> {
    let source_identity = identity(data_url.as_bytes());
    let dimensions = source_png_dimensions(data_url);
    if policy == ImageSendPolicy::Balanced {
        let key = CacheKey {
            source: source_identity,
            policy,
            version: CODEC_VERSION,
        };
        if let Ok(mut cache) = CACHE.get_or_init(Mutex::default).try_lock() {
            if let Some(CachedImage::Derivative(image)) = cache.get(key) {
                return derivative_result(source_identity, image, 0, 0);
            }
        }
    }
    ImageSendResult {
        url: ImageSendUrl::Original(data_url),
        stats: ImageSendStats::default(),
        source_identity,
        derivative_identity: source_identity,
        source_dimensions: dimensions,
        dimensions,
        transform: ImageTransform::Original,
    }
}

/// Optimize a selected ordinary USER image only. Exact inspection always bypasses this cache.
/// Remote URLs, unsupported metadata, malformed/oversized inputs and admission failures are no-ops.
pub fn optimize_user_image(
    data_url: &str,
    policy: ImageSendPolicy,
    exact: bool,
) -> ImageSendResult<'_> {
    optimize_with_cache(data_url, policy, exact, CACHE.get_or_init(Mutex::default))
}
fn optimize_with_cache<'a>(
    data_url: &'a str,
    policy: ImageSendPolicy,
    exact: bool,
    cache: &Mutex<Cache>,
) -> ImageSendResult<'a> {
    let source_identity = identity(data_url.as_bytes());
    let source_dimensions = source_png_dimensions(data_url);
    let original = || ImageSendResult {
        url: ImageSendUrl::Original(data_url),
        stats: ImageSendStats::default(),
        source_identity,
        derivative_identity: source_identity,
        source_dimensions,
        dimensions: source_dimensions,
        transform: ImageTransform::Original,
    };
    if exact || matches!(policy, ImageSendPolicy::Original | ImageSendPolicy::Auto) {
        return original();
    }
    let Some(encoded) = data_url.strip_prefix("data:image/png;base64,") else {
        return original();
    };
    let key = CacheKey {
        source: source_identity,
        policy,
        version: CODEC_VERSION,
    };
    // Claim a key and release the global lock BEFORE decoding/encoding. A concurrent caller
    // returns its original immediately; a later request can reuse the completed outcome.
    let _work = {
        let Ok(mut cache) = cache.try_lock() else {
            return original();
        };
        if let Some(image) = cache.get(key) {
            return match image {
                CachedImage::Derivative(image) => derivative_result(source_identity, image, 0, 0),
                CachedImage::Original => original(),
            };
        }
        let Some(work) = cache.begin(key) else {
            return original();
        };
        work
    };
    #[cfg(test)]
    if let Some(barriers) = cache.try_lock().ok().and_then(|c| c.before_work.clone()) {
        barriers.0.wait();
        barriers.1.wait();
    }
    let negative = || publish_cached(cache, key, CachedImage::Original);
    let started = Instant::now();
    let Ok(size) = base64_decoded_len(encoded, MAX_ENCODED) else {
        negative();
        return original();
    };
    let Some(mut lease) = try_image_memory_lease(size) else {
        return original();
    };
    let Ok(bytes) = base64_decode_bounded(encoded, MAX_ENCODED) else {
        negative();
        return original();
    };
    let Some((width, height, channels)) = supported_png(&bytes) else {
        negative();
        return original();
    };
    let mut fallback = original();
    fallback.source_dimensions = Some((width, height));
    fallback.dimensions = Some((width, height));
    let Some(pixel_bytes) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(channels))
        .filter(|n| *n <= MAX_DECODED)
    else {
        negative();
        return fallback;
    };
    // Two fixed-capacity JPEG candidates, bounded by the largest payload that can beat the
    // original URI (and 8 MiB). No candidate Vec can grow beyond its admitted capacity.
    let candidate_limit = useful_jpeg_limit(data_url.len());
    let uri_limit = uri_len(candidate_limit);
    let rgb_bytes = width as usize * height as usize * 3;
    let normalization_bytes = if matches!(channels, 2 | 4) {
        rgb_bytes
    } else {
        0
    };
    let working = MAX_DECODED
        + pixel_bytes
        + normalization_bytes
        + 2 * candidate_limit
        + 2 * uri_limit
        + 1024 * 1024;
    if !lease.try_grow(working) {
        return fallback;
    }
    let Some((pixels, color)) = decode_pixels(&bytes, width, height, pixel_bytes) else {
        negative();
        return fallback;
    };
    let mut attempts = 1;
    let mut best = encode_candidate(&pixels, width, height, color, 88, candidate_limit);
    let mut quality = 88;
    if best
        .as_ref()
        .is_none_or(|v| uri_len(v.len()) > SOFT_URI_TARGET)
    {
        attempts += 1;
        if let Some(candidate) =
            encode_candidate(&pixels, width, height, color, 82, candidate_limit)
        {
            if best.as_ref().is_none_or(|v| candidate.len() < v.len()) {
                best = Some(candidate);
                quality = 82;
            }
        }
    }
    let mut dimensions = (width, height);
    let mut complete = true;
    if best
        .as_ref()
        .is_none_or(|v| uri_len(v.len()) > SOFT_URI_TARGET)
        && width.max(height) > 1280
    {
        let target = resized_dimensions(width, height);
        // image::resize holds a width*new_height RGBA-f32 intermediate, output RGB and
        // filter weights. Grayscale expansion also requires its own full-size RGB buffer.
        let rgb_extra = if color == ExtendedColorType::L8 {
            pixel_bytes * 3
        } else {
            0
        };
        let resize_bytes = width as usize * target.1 as usize * 16
            + target.0 as usize * target.1 as usize * 3
            + width.max(height) as usize * 16
            + rgb_extra;
        if lease.try_grow(resize_bytes) {
            let rgb = if color == ExtendedColorType::Rgb8 {
                RgbImage::from_raw(width, height, pixels)
            } else {
                let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
                for value in pixels {
                    rgb.extend_from_slice(&[value; 3]);
                }
                RgbImage::from_raw(width, height, rgb)
            };
            let Some(rgb) = rgb else { return fallback };
            let resized = image::imageops::resize(&rgb, target.0, target.1, FilterType::Lanczos3);
            attempts += 1;
            if let Some(candidate) = encode_candidate(
                resized.as_raw(),
                target.0,
                target.1,
                ExtendedColorType::Rgb8,
                82,
                candidate_limit,
            ) {
                if best.as_ref().is_none_or(|v| candidate.len() < v.len()) {
                    best = Some(candidate);
                    quality = 82;
                    dimensions = target;
                }
            }
        } else {
            // A useful full-resolution candidate still wins over the original. Do not cache
            // this partial outcome: a later send with available memory should retry resizing.
            complete = false;
        }
    }
    let Some(jpeg) = best
        .filter(|v| uri_len(v.len()) < data_url.len())
        .and_then(|v| v.bytes)
    else {
        if complete {
            negative();
        }
        fallback.stats.candidates_encoded = attempts;
        fallback.stats.encode_micros = elapsed_micros(started);
        return fallback;
    };
    let url = jpeg_url(&jpeg);
    let derivative_identity = identity(url.as_bytes());
    let retained =
        url.len() + std::mem::size_of::<ImageDerivative>() + 2 * std::mem::size_of::<usize>();
    let image = Arc::new(ImageDerivative {
        url: url.into_boxed_str(),
        identity: derivative_identity,
        source_dimensions: (width, height),
        dimensions,
        transform: ImageTransform::Jpeg {
            quality,
            resized: dimensions != (width, height),
        },
        _memory: lease.split(retained),
    });
    if complete {
        publish_cached(cache, key, CachedImage::Derivative(image.clone()));
    }
    derivative_result(source_identity, image, elapsed_micros(started), attempts)
}
fn derivative_result<'a>(
    source_identity: [u8; 32],
    image: Arc<ImageDerivative>,
    encode_micros: u64,
    candidates_encoded: u8,
) -> ImageSendResult<'a> {
    ImageSendResult {
        stats: ImageSendStats {
            encode_micros,
            candidates_encoded,
        },
        source_identity,
        derivative_identity: image.identity,
        source_dimensions: Some(image.source_dimensions),
        dimensions: Some(image.dimensions),
        transform: image.transform,
        url: ImageSendUrl::Derivative(image),
    }
}
fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u64::MAX as u128) as u64
}
fn uri_len(binary: usize) -> usize {
    JPEG_PREFIX.len() + binary.div_ceil(3) * 4
}
fn useful_jpeg_limit(source_uri_len: usize) -> usize {
    // Base64 is rounded to groups of four; require a STRICTLY smaller final URI.
    (source_uri_len.saturating_sub(JPEG_PREFIX.len() + 1) / 4)
        .saturating_mul(3)
        .min(MAX_ENCODED)
}

// No codec or allocation: decode only enough canonical base64 for PNG signature + IHDR.
// Informational dimensions; the full decoder still validates chunk CRCs before conversion.
pub(crate) fn source_png_dimensions(url: &str) -> Option<(u32, u32)> {
    let encoded = url.strip_prefix("data:image/png;base64,")?;
    let prefix = encoded.as_bytes().get(..44)?;
    let mut bytes = [0u8; 33];
    for (group, out) in prefix.chunks_exact(4).zip(bytes.chunks_exact_mut(3)) {
        let a = base64_sextet(group[0])?;
        let b = base64_sextet(group[1])?;
        let c = base64_sextet(group[2])?;
        let d = base64_sextet(group[3])?;
        out[0] = (a << 2) | (b >> 4);
        out[1] = (b << 4) | (c >> 2);
        out[2] = (c << 6) | d;
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes[8..12] != [0, 0, 0, 13]
        || &bytes[12..16] != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}
fn base64_sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
fn resized_dimensions(width: u32, height: u32) -> (u32, u32) {
    let long = u64::from(width.max(height));
    (
        ((u64::from(width) * 1280 / long).max(1)) as u32,
        ((u64::from(height) * 1280 / long).max(1)) as u32,
    )
}

/// Conservative pre-decoder chunk allowlist. Reject palette transparency, animation, 16-bit,
/// ICC/gamma/sRGB/chromaticity and unknown chunks (including compressed metadata).
/// Explicit alpha channels are accepted only if EVERY decoded alpha sample is fully opaque.
fn supported_png(bytes: &[u8]) -> Option<(u32, u32, usize)> {
    if bytes.len() < 33
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes[8..12] != [0, 0, 0, 13]
        || &bytes[12..16] != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    let channels = match bytes[25] {
        0 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    if width == 0
        || height == 0
        || width > u16::MAX as u32
        || height > u16::MAX as u32
        || bytes[24] != 8
        || bytes[26] != 0
        || bytes[27] != 0
        || bytes[28] > 1
        || u64::from(width) * u64::from(height) * 4 > MAX_DECODED as u64
    {
        return None;
    }
    let mut offset = 8usize;
    let mut seen_data = false;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset.checked_add(8)?)?;
        let len = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        let end = offset.checked_add(12)?.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        match &header[4..8] {
            b"IHDR" if offset == 8 && len == 13 => {}
            b"IDAT" => seen_data = true,
            b"IEND" if len == 0 && seen_data && end == bytes.len() => {
                return Some((width, height, channels))
            }
            b"pHYs" if len == 9 && !seen_data => {}
            _ => return None,
        }
        offset = end;
    }
    None
}
fn decode_pixels(
    bytes: &[u8],
    width: u32,
    height: u32,
    size: usize,
) -> Option<(Vec<u8>, ExtendedColorType)> {
    let mut decoder =
        png::Decoder::new_with_limits(Cursor::new(bytes), png::Limits { bytes: MAX_DECODED });
    decoder.ignore_checksums(false);
    // Identity transformation only: never silently strip 16-bit or expand transparency.
    let mut reader = decoder.read_info().ok()?;
    if reader.output_buffer_size() != size {
        return None;
    }
    let mut pixels = vec![0; size];
    let frame = reader.next_frame(&mut pixels).ok()?;
    if frame.width != width || frame.height != height || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let (pixels, color) = match frame.color_type {
        png::ColorType::Grayscale => (pixels, ExtendedColorType::L8),
        png::ColorType::Rgb => (pixels, ExtendedColorType::Rgb8),
        png::ColorType::GrayscaleAlpha if pixels.chunks_exact(2).all(|p| p[1] == 255) => {
            let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
            for pixel in pixels.chunks_exact(2) {
                rgb.extend_from_slice(&[pixel[0]; 3]);
            }
            (rgb, ExtendedColorType::Rgb8)
        }
        png::ColorType::Rgba if pixels.chunks_exact(4).all(|p| p[3] == 255) => {
            let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
            for pixel in pixels.chunks_exact(4) {
                rgb.extend_from_slice(&pixel[..3]);
            }
            (rgb, ExtendedColorType::Rgb8)
        }
        _ => return None,
    };
    reader.finish().ok()?;
    Some((pixels, color))
}
struct JpegCandidate {
    bytes: Option<Vec<u8>>,
    len: usize,
}
impl JpegCandidate {
    fn len(&self) -> usize {
        self.len
    }
}
struct BoundedJpeg {
    candidate: JpegCandidate,
    limit: usize,
}
impl BoundedJpeg {
    fn new(limit: usize) -> Self {
        Self {
            candidate: JpegCandidate {
                bytes: Some(Vec::with_capacity(limit)),
                len: 0,
            },
            limit,
        }
    }
}
impl Write for BoundedJpeg {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let end = self.candidate.len.saturating_add(bytes.len());
        if end > MAX_ENCODED {
            return Err(std::io::Error::other("JPEG preview exceeds encoded limit"));
        }
        self.candidate.len = end;
        if end > self.limit {
            // Count without retaining a non-useful JPEG, preserving the existing quality/resize
            // decisions based on its actual length rather than treating it as a codec failure.
            self.candidate.bytes = None;
        } else if let Some(output) = &mut self.candidate.bytes {
            output.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_candidate(
    pixels: &[u8],
    width: u32,
    height: u32,
    color: ExtendedColorType,
    quality: u8,
    limit: usize,
) -> Option<JpegCandidate> {
    let mut writer = BoundedJpeg::new(limit);
    JpegEncoder::new_with_quality(&mut writer, quality)
        .encode(pixels, width, height, color)
        .ok()?;
    Some(writer.candidate)
}
fn jpeg_url(jpeg: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut url = String::with_capacity(uri_len(jpeg.len()));
    url.push_str(JPEG_PREFIX);
    for c in jpeg.chunks(3) {
        let a = c[0] as usize;
        let b = c.get(1).copied().unwrap_or(0) as usize;
        let d = c.get(2).copied().unwrap_or(0) as usize;
        url.push(T[a >> 2] as char);
        url.push(T[((a & 3) << 4) | (b >> 4)] as char);
        url.push(if c.len() > 1 {
            T[((b & 15) << 2) | (d >> 6)] as char
        } else {
            '='
        });
        url.push(if c.len() > 2 { T[d & 63] as char } else { '=' });
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool_media::base64_encode;

    fn fixture(
        width: u32,
        height: u32,
        noise: bool,
        color: png::ColorType,
        depth: png::BitDepth,
    ) -> String {
        let channels = color.samples();
        let mut pixels = vec![
            0;
            width as usize
                * height as usize
                * channels
                * if depth == png::BitDepth::Sixteen {
                    2
                } else {
                    1
                }
        ];
        let mut seed = 0x0bad_f00du32;
        for (index, byte) in pixels.iter_mut().enumerate() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = if noise {
                seed as u8
            } else {
                let x = index / channels % width as usize;
                let y = index / channels / width as usize;
                if x % 64 < 3 || y % 40 < 2 {
                    24
                } else {
                    230
                }
            };
        }
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(color);
            encoder.set_depth(depth);
            encoder.set_compression(png::Compression::Fast);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&pixels)
                .unwrap();
        }
        format!("data:image/png;base64,{}", base64_encode(&png))
    }

    #[test]
    fn numerical_cold_warm_size_and_identity() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        for noise in [false, true] {
            let source = fixture(1536, 1024, noise, png::ColorType::Rgb, png::BitDepth::Eight);
            let unchanged = source.clone();
            let cold_started = Instant::now();
            let cold = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
            let cold_us = elapsed_micros(cold_started);
            let warm_started = Instant::now();
            let warm = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
            let warm_us = elapsed_micros(warm_started);
            eprintln!("image_send noise={noise} source_uri={} sent_uri={} cold_us={cold_us} warm_us={warm_us} candidates={} dimensions={:?}", source.len(), cold.url.as_str().len(), cold.stats.candidates_encoded, cold.dimensions);
            // High-entropy is guaranteed to benefit; UI PNGs may already be smaller than JPEG.
            assert!(cold.url.as_str().len() <= source.len());
            assert!(cold.stats.candidates_encoded <= 3);
            assert_eq!(source, unchanged);
            assert_eq!(cold.url.as_str(), warm.url.as_str());
            assert_eq!(
                cold.derivative_identity,
                identity(cold.url.as_str().as_bytes())
            );
            assert_eq!(cold.source_identity, identity(source.as_bytes()));
            if noise {
                assert!(cold.url.as_str().len() < source.len());
                assert_eq!(warm.stats.candidates_encoded, 0);
                assert_eq!(warm.stats.encode_micros, 0);
            }
        }
    }

    #[test]
    fn bypasses_exact_original_remote_and_invalid() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let source = fixture(32, 32, true, png::ColorType::Rgb, png::BitDepth::Eight);
        for (url, policy, exact) in [
            (source.as_str(), ImageSendPolicy::Original, false),
            (source.as_str(), ImageSendPolicy::Auto, false),
            (source.as_str(), ImageSendPolicy::Balanced, true),
            (
                "https://example.invalid/private.png",
                ImageSendPolicy::Balanced,
                false,
            ),
            (
                "data:image/png;base64,!!!!",
                ImageSendPolicy::Balanced,
                false,
            ),
            (
                "data:image/jpeg;base64,AAAA",
                ImageSendPolicy::Balanced,
                false,
            ),
        ] {
            let result = optimize_with_cache(url, policy, exact, &cache);
            assert_eq!(result.url.as_str(), url);
            assert_eq!(result.stats.candidates_encoded, 0);
        }
        assert!(cache
            .lock()
            .unwrap()
            .entries
            .iter()
            .all(|(_, image)| matches!(image, CachedImage::Original)));
        assert_eq!(
            ImageSendPolicy::from_environment_value(Some(" ORIGINAL ")),
            ImageSendPolicy::Original
        );
        assert_eq!(
            ImageSendPolicy::from_environment_value(None),
            ImageSendPolicy::Balanced
        );
    }

    #[test]
    fn conservative_metadata_and_decode_guards() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        for (color, depth) in [
            (png::ColorType::Rgba, png::BitDepth::Eight),
            (png::ColorType::Rgb, png::BitDepth::Sixteen),
        ] {
            let source = fixture(16, 16, true, color, depth);
            assert_eq!(
                optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache)
                    .url
                    .as_str(),
                source
            );
        }
        let source = fixture(16, 16, true, png::ColorType::Rgb, png::BitDepth::Eight);
        let encoded = source.strip_prefix("data:image/png;base64,").unwrap();
        let bytes = base64_decode_bounded(encoded, MAX_ENCODED).unwrap();
        for chunk in [
            b"acTL", b"fcTL", b"tRNS", b"iCCP", b"gAMA", b"sRGB", b"cHRM", b"cICP",
        ] {
            let mut changed = bytes.clone();
            // Guard rejects chunk type BEFORE a decoder can expand metadata or trust bad CRC.
            changed.splice(
                33..33,
                [0, 0, 0, 0]
                    .into_iter()
                    .chain(chunk.iter().copied())
                    .chain([0, 0, 0, 0]),
            );
            assert!(supported_png(&changed).is_none());
        }
        let mut bomb = bytes.clone();
        bomb[16..20].copy_from_slice(&65535u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&65535u32.to_be_bytes());
        assert!(supported_png(&bomb).is_none());
        let mut corrupt = bytes;
        corrupt[29] ^= 1; // IHDR CRC
        let url = format!("data:image/png;base64,{}", base64_encode(&corrupt));
        assert_eq!(
            optimize_with_cache(&url, ImageSendPolicy::Balanced, false, &cache)
                .url
                .as_str(),
            url
        );
        assert_eq!(resized_dimensions(1536, 1024), (1280, 853));
        assert_eq!(resized_dimensions(1024, 1536), (853, 1280));
    }

    #[test]
    fn opaque_rgba_is_optimized_but_transparent_alpha_is_preserved() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let mut rgba = vec![0; 640 * 480 * 4];
        let mut seed = 42u32;
        for pixel in rgba.chunks_exact_mut(4) {
            for byte in &mut pixel[..3] {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *byte = seed as u8;
            }
            pixel[3] = 255;
        }
        let make_url = |pixels: &[u8]| {
            let bytes = crate::core::tool_media::encode_rgba_png(pixels, 640, 480).unwrap();
            format!("data:image/png;base64,{}", base64_encode(&bytes))
        };
        let source = make_url(&rgba);
        let optimized = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert!(optimized.url.as_str().starts_with(JPEG_PREFIX));
        assert!(optimized.url.as_str().len() < source.len());
        assert_eq!(optimized.source_dimensions, Some((640, 480)));
        assert_eq!(optimized.dimensions, Some((640, 480)));
        let jpeg = base64_decode_bounded(
            optimized.url.as_str().strip_prefix(JPEG_PREFIX).unwrap(),
            MAX_ENCODED,
        )
        .unwrap();
        let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (640, 480));
        rgba[3] = 254;
        let transparent = make_url(&rgba);
        let preserved = optimize_with_cache(&transparent, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(preserved.url.as_str(), transparent);
        assert_eq!(preserved.stats.candidates_encoded, 0);
    }

    #[test]
    fn no_larger_candidate_is_kept_and_budget_exhaustion_is_original() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let source = fixture(1, 1, false, png::ColorType::Rgb, png::BitDepth::Eight);
        let result = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(result.url.as_str(), source);
        assert_eq!(result.transform, ImageTransform::Original);
        // Existing encoded cache leases may persist; reserve precisely the remaining capacity.
        let available = IMAGE_MEMORY_BUDGET - IMAGE_MEMORY_USED.load(Ordering::Acquire);
        let lease = try_image_memory_lease(available).unwrap();
        let larger = fixture(32, 32, true, png::ColorType::Rgb, png::BitDepth::Eight);
        let fallback = optimize_with_cache(&larger, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(fallback.url.as_str(), larger);
        assert_eq!(fallback.stats.candidates_encoded, 0);
        let before_refusal = IMAGE_MEMORY_USED.load(Ordering::Acquire);
        assert!(try_image_memory_lease(1).is_none());
        assert_eq!(
            IMAGE_MEMORY_USED.load(Ordering::Acquire),
            before_refusal,
            "refused admission must not drop an unowned lease"
        );
        drop(lease);
        assert!(try_image_memory_lease(1).is_some());
    }

    #[test]
    fn cache_evicts_by_count_and_encoded_bytes_without_decoded_buffers() {
        let _guard = image_test_lock();
        let mut cache = Cache::default();
        for i in 0..10u8 {
            let key = CacheKey {
                source: [i; 32],
                policy: ImageSendPolicy::Balanced,
                version: CODEC_VERSION,
            };
            let image = Arc::new(ImageDerivative {
                url: "x".repeat(3 * 1024 * 1024).into_boxed_str(),
                identity: [i; 32],
                source_dimensions: (1, 1),
                dimensions: (1, 1),
                transform: ImageTransform::Original,
                _memory: try_image_memory_lease(3 * 1024 * 1024).unwrap(),
            });
            cache.publish(key, CachedImage::Derivative(image));
            assert!(cache.entries.len() <= CACHE_ENTRIES);
            assert!(cache.bytes <= CACHE_BYTES);
        }
        assert!(cache
            .get(CacheKey {
                source: [0; 32],
                policy: ImageSendPolicy::Balanced,
                version: CODEC_VERSION
            })
            .is_none());
    }

    #[test]
    fn cache_count_bound_and_retained_lease_survive_eviction() {
        let _guard = image_test_lock();
        let baseline = IMAGE_MEMORY_USED.load(Ordering::Acquire);
        let mut cache = Cache::default();
        let mut held = None;
        for i in 0..9u8 {
            let key = CacheKey {
                source: [i; 32],
                policy: ImageSendPolicy::Balanced,
                version: CODEC_VERSION,
            };
            let image = Arc::new(ImageDerivative {
                url: "x".repeat(1024).into_boxed_str(),
                identity: [i; 32],
                source_dimensions: (1, 1),
                dimensions: (1, 1),
                transform: ImageTransform::Original,
                _memory: try_image_memory_lease(1024).unwrap(),
            });
            if i == 0 {
                held = Some(image.clone());
            }
            cache.publish(key, CachedImage::Derivative(image));
        }
        assert_eq!(cache.entries.len(), CACHE_ENTRIES);
        assert_eq!(
            IMAGE_MEMORY_USED.load(Ordering::Acquire),
            baseline + 9 * 1024
        );
        drop(cache);
        assert_eq!(IMAGE_MEMORY_USED.load(Ordering::Acquire), baseline + 1024);
        drop(held);
        assert_eq!(IMAGE_MEMORY_USED.load(Ordering::Acquire), baseline);
    }

    #[test]
    fn debug_is_numeric_only() {
        let _guard = image_test_lock();
        let source = "https://example.invalid/private?secret=do-not-print";
        let result = optimize_user_image(source, ImageSendPolicy::Original, false);
        let debug = format!("{result:?} {:?}", result.url);
        assert!(!debug.contains(source));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("source_identity"));
    }

    #[test]
    fn useful_candidate_storage_is_bounded_without_changing_length_decisions() {
        let _guard = image_test_lock();
        for source_len in JPEG_PREFIX.len() + 5..512 {
            let limit = useful_jpeg_limit(source_len);
            assert!(uri_len(limit) < source_len);
            assert!(uri_len(limit + 1) >= source_len);
        }
        assert_eq!(useful_jpeg_limit(usize::MAX), MAX_ENCODED);
        let mut writer = BoundedJpeg::new(7);
        assert_eq!(writer.candidate.bytes.as_ref().unwrap().capacity(), 7);
        writer.write_all(&[0; 7]).unwrap();
        assert_eq!(writer.candidate.bytes.as_ref().unwrap().capacity(), 7);
        writer.write_all(&[0]).unwrap();
        assert!(writer.candidate.bytes.is_none());
        assert_eq!(writer.candidate.len(), 8);
        writer.write_all(&vec![0; MAX_ENCODED - 8]).unwrap();
        assert_eq!(writer.candidate.len(), MAX_ENCODED);
        assert!(writer.write_all(&[0]).is_err());
        assert_eq!(writer.candidate.len(), MAX_ENCODED);

        let pixels = vec![230; 32 * 32 * 3];
        let full =
            encode_candidate(&pixels, 32, 32, ExtendedColorType::Rgb8, 88, MAX_ENCODED).unwrap();
        let bounded = encode_candidate(&pixels, 32, 32, ExtendedColorType::Rgb8, 88, 1).unwrap();
        assert_eq!(bounded.len(), full.len());
        assert!(bounded.bytes.is_none());
    }

    #[test]
    fn bypasses_do_not_consult_or_populate_even_a_warm_cache() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let source = fixture(64, 64, true, png::ColorType::Rgb, png::BitDepth::Eight);
        let cold = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert!(matches!(cold.url, ImageSendUrl::Derivative(_)));
        // Holding the cache lock proves bypasses and lock contention do not wait on it.
        let locked = cache.lock().unwrap();
        for (policy, exact) in [
            (ImageSendPolicy::Original, false),
            (ImageSendPolicy::Auto, false),
            (ImageSendPolicy::Balanced, true),
            (ImageSendPolicy::Balanced, false),
        ] {
            let result = optimize_with_cache(&source, policy, exact, &cache);
            assert_eq!(result.url.as_str(), source);
            assert_eq!(result.stats.candidates_encoded, 0);
        }
        assert_eq!(locked.entries.len(), 1);
    }

    #[test]
    fn deterministic_negative_cache_is_warm_and_bounded() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let source = fixture(1, 1, false, png::ColorType::Rgb, png::BitDepth::Eight);
        let cold = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(cold.url.as_str(), source);
        assert_eq!(cold.stats.candidates_encoded, 1);
        let warm = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(warm.url.as_str(), source);
        assert_eq!(warm.stats.candidates_encoded, 0);
        assert_eq!(warm.stats.encode_micros, 0);
        for i in 0..CACHE_ENTRIES + 2 {
            let invalid = format!("data:image/png;base64,invalid{i}");
            let result = optimize_with_cache(&invalid, ImageSendPolicy::Balanced, false, &cache);
            assert_eq!(result.url.as_str(), invalid);
            let c = cache.lock().unwrap();
            assert!(c.entries.len() <= CACHE_ENTRIES);
            assert_eq!(c.bytes, 0);
            assert!(c.in_flight.len() <= CACHE_ENTRIES);
        }
        let cold_again = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(cold_again.stats.candidates_encoded, 1);
    }

    #[test]
    fn transient_admission_refusals_do_not_poison_the_cache() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let source = fixture(64, 64, true, png::ColorType::Rgb, png::BitDepth::Eight);
        let encoded = source.strip_prefix("data:image/png;base64,").unwrap();
        let decoded_size = base64_decoded_len(encoded, MAX_ENCODED).unwrap();
        // Test both initial admission and codec-working-set admission separately.
        for spare in [0, decoded_size] {
            let available = IMAGE_MEMORY_BUDGET - IMAGE_MEMORY_USED.load(Ordering::Acquire);
            let held = try_image_memory_lease(available - spare).unwrap();
            let refused = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
            assert_eq!(refused.url.as_str(), source);
            assert_eq!(refused.stats.candidates_encoded, 0);
            assert!(cache.lock().unwrap().entries.is_empty());
            drop(held);
        }
        let retried = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert!(matches!(retried.url, ImageSendUrl::Derivative(_)));
        assert!(retried.stats.candidates_encoded > 0);
    }

    #[test]
    fn resize_admission_failure_keeps_useful_full_resolution_and_retries() {
        let _guard = image_test_lock();
        let cache = Mutex::new(Cache::default());
        let source = fixture(1536, 1024, true, png::ColorType::Rgb, png::BitDepth::Eight);
        let size = base64_decoded_len(
            source.strip_prefix("data:image/png;base64,").unwrap(),
            MAX_ENCODED,
        )
        .unwrap();
        let limit = useful_jpeg_limit(source.len());
        let initial =
            size + MAX_DECODED + 1536 * 1024 * 3 + 2 * limit + 2 * uri_len(limit) + 1024 * 1024;
        let available = IMAGE_MEMORY_BUDGET - IMAGE_MEMORY_USED.load(Ordering::Acquire);
        let held = try_image_memory_lease(available - initial).unwrap();
        let partial = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert!(partial.url.as_str().len() < source.len());
        assert_eq!(partial.dimensions, Some((1536, 1024)));
        assert_eq!(partial.stats.candidates_encoded, 2);
        assert!(matches!(
            partial.transform,
            ImageTransform::Jpeg { resized: false, .. }
        ));
        assert!(cache.lock().unwrap().entries.is_empty());
        drop(held);
        let retry = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(retry.stats.candidates_encoded, 3);
        assert_eq!(retry.dimensions, Some((1280, 853)));
        let warm = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(warm.stats.candidates_encoded, 0);
        assert_eq!(warm.url.as_str(), retry.url.as_str());
    }

    #[test]
    fn concurrent_same_key_has_one_encoder_and_never_waits_for_it() {
        let _guard = image_test_lock();
        let barriers = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        let cache = Mutex::new(Cache {
            before_work: Some(barriers.clone()),
            ..Cache::default()
        });
        let source = fixture(64, 64, true, png::ColorType::Rgb, png::BitDepth::Eight);
        std::thread::scope(|scope| {
            let first = scope
                .spawn(|| optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache));
            barriers.0.wait();
            assert!(
                cache.try_lock().is_ok(),
                "encoding must not hold the cache lock"
            );
            for _ in 0..4 {
                let concurrent = scope
                    .spawn(|| {
                        optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache)
                    })
                    .join()
                    .unwrap();
                assert_eq!(concurrent.url.as_str(), source);
                assert_eq!(concurrent.stats.candidates_encoded, 0);
            }
            barriers.1.wait();
            let cold = first.join().unwrap();
            assert!(cold.stats.candidates_encoded > 0);
            assert!(matches!(cold.url, ImageSendUrl::Derivative(_)));
        });
        let warm = optimize_with_cache(&source, ImageSendPolicy::Balanced, false, &cache);
        assert_eq!(warm.stats.candidates_encoded, 0);
        assert!(matches!(warm.url, ImageSendUrl::Derivative(_)));
    }

    #[test]
    fn oversized_memory_reservation_is_nonblocking() {
        let _guard = image_test_lock();
        assert!(try_image_memory_lease(IMAGE_MEMORY_BUDGET + 1).is_none());
        let mut lease = try_image_memory_lease(0).unwrap();
        assert!(!lease.try_grow(IMAGE_MEMORY_BUDGET + 1));
        assert_eq!(lease.bytes(), 0);
    }
}
