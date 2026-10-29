//! Image (vision) input — turn a clipboard image (Ctrl-O) or a referenced/dragged image FILE into
//! an OpenAI-compatible `image_url` data URL the chat layer attaches to a user message.
//!
//! Two ways in, because terminals can't deliver Ctrl-V image paste (Windows Terminal intercepts
//! Ctrl-V for its own paste):
//!  1. **File path** — drag an image file onto the window (the terminal pastes its path), or type/
//!     paste a path; `extract_image_attachments` pulls it off the input line on Enter. Pure +
//!     cross-platform, no clipboard.
//!  2. **Clipboard image** — press Ctrl-O to grab a copied screenshot (Win+Shift+S / "Copy image").
//!     DESKTOP-ONLY (Windows/macOS via `arboard`): on Linux `arboard` needs X11/Wayland libs at
//!     runtime, which would break the headless static binary `aizen serve` ships as — so it's
//!     `cfg`-gated and a no-op stub elsewhere.

use anyhow::{Context, Result};

/// Hard ceiling on the image bytes (before base64). A larger image is rejected, not sent.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Image extensions we accept as drag-drop / path attachments.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Base64-encode (standard alphabet, padded). Hand-rolled (encode-only) to avoid a dependency.
pub fn base64_encode(input: &[u8]) -> String {
    crate::core::tool_media::base64_encode(input)
}

/// Build a `data:<mime>;base64,<…>` URL.
fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64_encode(bytes))
}

/// Detect an image mime from leading magic bytes. `None` if not a recognized image.
#[cfg(test)]
fn sniff_mime(b: &[u8]) -> Option<&'static str> {
    crate::core::tool_media::sniff_mime(b)
}

/// Read an image FILE → a data URL (cross-platform). Mime sniffed from magic bytes; non-image or
/// oversize files are rejected.
pub fn image_file_to_data_url(path: &str) -> Result<String> {
    crate::core::tool_media::ToolImage::from_file(
        std::path::Path::new(path),
        crate::core::tool_media::ImageProvenance::new("user_file", Some(path.to_owned())),
    )
    .map(|image| image.data_url())
}

/// Pull image-file attachments off an input line (drag a file onto the terminal → it pastes the
/// path; a typed/pasted path works too). Returns (text with those paths removed, data URLs). A
/// token becomes an attachment ONLY when it's an existing file with an image extension — so
/// mentioning a name that isn't a real image path stays as text. A malformed file or excess batch
/// refuses the whole selection, never silently sends a partial batch or treats rejected paths as prose.
/// Quote-aware (drag-drop quotes paths containing spaces).
pub fn extract_image_attachments(line: &str) -> Result<(String, Vec<String>)> {
    extract_with_existing_sizes(line, Vec::new())
}

fn extract_with_existing_sizes(
    line: &str,
    existing_sizes: Vec<usize>,
) -> Result<(String, Vec<String>)> {
    let tokens: Vec<_> = tokenize_quoted(line)
        .into_iter()
        .map(|token| {
            let selected = has_image_ext(&token);
            (token, selected)
        })
        .collect();
    let count = existing_sizes.len() + tokens.iter().filter(|(_, selected)| *selected).count();
    crate::core::tool_media::validate_image_batch_sizes(std::iter::repeat_n(1, count))?;
    let mut source_sizes = existing_sizes.clone();
    for (token, selected) in &tokens {
        if *selected {
            let size = std::fs::metadata(token)
                .context("cannot inspect selected image; no message was sent")?
                .len();
            source_sizes.push(usize::try_from(size).context("selected image size overflow")?);
        }
    }
    // Refuse the whole file/clipboard selection before reading, decoding or base64-encoding files.
    // Metadata is only a fast preflight; actual loaded bytes are checked again against the budget.
    crate::core::tool_media::validate_image_batch_sizes(source_sizes)?;
    let mut text: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut sizes = existing_sizes;
    for (tok, selected) in tokens {
        if selected {
            let url = image_file_to_data_url(&tok)
                .context("cannot attach selected image; no message was sent")?;
            let (_, data) = crate::core::tool_media::split_data_url(&url)?;
            sizes.push(crate::core::tool_media::base64_decoded_len(
                data, MAX_BYTES,
            )?);
            crate::core::tool_media::validate_image_batch_sizes(sizes.iter().copied())?;
            images.push(url);
            continue;
        }
        text.push(tok);
    }
    Ok((text.join(" ").trim().to_string(), images))
}

/// Validate the combined clipboard/path selection without consuming the editable draft.
pub(crate) fn prepare_image_attachments(
    line: &str,
    images: &[String],
) -> Result<(String, Vec<String>)> {
    if images.is_empty() {
        extract_image_attachments(line)
    } else {
        extract_with_existing_sizes(line, attachment_sizes(images)?)
    }
}

fn attachment_sizes<'a>(urls: impl IntoIterator<Item = &'a String>) -> Result<Vec<usize>> {
    urls.into_iter()
        .map(|url| {
            let (_, data) = crate::core::tool_media::split_data_url(url)?;
            crate::core::tool_media::base64_decoded_len(data, MAX_BYTES)
        })
        .collect()
}

fn validate_attachment_urls<'a>(urls: impl IntoIterator<Item = &'a String>) -> Result<()> {
    crate::core::tool_media::validate_image_batch_sizes(attachment_sizes(urls)?)?;
    Ok(())
}

/// Refuse an excess clipboard selection before retaining another image in the draft.
pub(crate) fn append_clipboard_image(images: &mut Vec<String>, url: String) -> Result<()> {
    validate_attachment_urls(images.iter().chain(std::iter::once(&url)))?;
    images.push(url);
    Ok(())
}

/// Detect selected paths without reading/encoding them, before text-only routing.
pub(crate) fn contains_image_path(line: &str) -> bool {
    tokenize_quoted(line)
        .iter()
        .any(|token| has_image_ext(token))
}

/// True when `path` ends in an image extension AND is an existing file.
fn has_image_ext(path: &str) -> bool {
    let ext_ok = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    ext_ok && std::path::Path::new(path).is_file()
}

/// Whitespace-split, keeping double-quoted spans together and stripping the quotes (drag-drop
/// quotes paths that contain spaces, e.g. `"C:\…\my shot.png"`).
fn tokenize_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => in_q = !in_q,
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Grab an image from the OS clipboard (a copied screenshot) and return it as a PNG data URL —
/// the Ctrl-O action. `Ok(None)` when the clipboard holds no image (it's text / empty / a file
/// reference). Desktop-only — a no-op stub elsewhere so the caller compiles everywhere.
#[cfg(any(windows, target_os = "macos"))]
pub fn clipboard_image_data_url() -> Result<Option<String>> {
    let mut cb = arboard::Clipboard::new().context("opening the clipboard")?;
    let img = match cb.get_image() {
        Ok(i) => i,
        Err(_) => return Ok(None), // no image on the clipboard
    };
    let w = u32::try_from(img.width).context("clipboard image width overflow")?;
    let h = u32::try_from(img.height).context("clipboard image height overflow")?;
    clipboard_rgba_data_url(&img.bytes, w, h).map(Some)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn clipboard_image_data_url() -> Result<Option<String>> {
    Ok(None)
}

/// Keep original clipboard pixels. Derivatives belong to request projection, never intake.
#[cfg(any(windows, target_os = "macos", test))]
fn clipboard_rgba_data_url(rgba: &[u8], w: u32, h: u32) -> Result<String> {
    let size = crate::core::tool_media::rgba_size(w, h).context(
        "pasted image exceeds decoded limit; copy a smaller screenshot; no automatic resize",
    )?;
    anyhow::ensure!(
        rgba.len() == size,
        "clipboard RGBA layout does not match dimensions"
    );
    let png = encode_png(rgba, w, h).context(
        "original clipboard pixels exceed the PNG limit; crop locally or copy a smaller screenshot",
    )?;
    anyhow::ensure!(
        png.len() <= MAX_BYTES,
        "pasted image exceeds 8 MiB encoded limit"
    );
    Ok(data_url("image/png", &png))
}

/// Encode an RGBA8 buffer to PNG bytes (pure-Rust `png` crate).
#[cfg(any(windows, target_os = "macos", test))]
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    crate::core::tool_media::encode_rgba_png(rgba, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_high_bytes() {
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64_encode(&[0x00]), "AA==");
    }

    #[test]
    fn data_url_shape() {
        assert_eq!(data_url("image/png", b"foo"), "data:image/png;base64,Zm9v");
    }

    #[test]
    fn sniff_mime_detects_formats() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_mime(b"hello not an image"), None);
    }

    #[test]
    fn tokenize_keeps_quoted_paths_together() {
        let toks = tokenize_quoted(r#"look at "C:\a b\shot.png" please"#);
        assert_eq!(toks, vec!["look", "at", r"C:\a b\shot.png", "please"]);
    }

    fn temp_png(tag: &str) -> std::path::PathBuf {
        // A minimal valid PNG (header + bytes good enough for the magic-byte sniff).
        let p = std::env::temp_dir().join(format!("aizen-img-{tag}-{}.png", std::process::id()));
        let bytes = crate::core::tool_media::encode_rgba_png(&[255, 0, 0, 255], 1, 1).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn image_file_to_data_url_reads_png() {
        let p = temp_png("read");
        let url = image_file_to_data_url(p.to_str().unwrap()).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        std::fs::remove_file(&p).ok();
        // a non-image file is rejected
        let txt = std::env::temp_dir().join(format!("aizen-not-img-{}.png", std::process::id()));
        std::fs::write(&txt, b"i am plain text").unwrap();
        assert!(
            image_file_to_data_url(txt.to_str().unwrap()).is_err(),
            "not a real image"
        );
        std::fs::remove_file(&txt).ok();
    }

    #[test]
    fn extract_pulls_existing_image_path_and_keeps_prose() {
        let p = temp_png("extract");
        let path = p.to_str().unwrap();
        let line = format!("what is in {path} ?");
        let (text, imgs) = extract_image_attachments(&line).unwrap();
        assert_eq!(imgs.len(), 1, "the real image file is attached");
        assert!(imgs[0].starts_with("data:image/png;base64,"));
        assert_eq!(text, "what is in ?", "the path is removed from the prose");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn extract_ignores_nonexistent_or_nonimage_tokens() {
        // A path-looking token that isn't a real file stays as text (no false positive).
        let (text, imgs) = extract_image_attachments("describe nope.png and run main.rs").unwrap();
        assert!(imgs.is_empty());
        assert_eq!(text, "describe nope.png and run main.rs");
    }

    #[test]
    fn extract_refuses_excess_or_malformed_selected_files_instead_of_partial_send() {
        let path = temp_png("batch");
        let quoted = format!("\"{}\"", path.display());
        let four = std::iter::repeat_n(quoted.as_str(), 4)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(extract_image_attachments(&four).unwrap().1.len(), 4);
        assert!(extract_image_attachments(&format!("{four} {quoted}")).is_err());
        std::fs::write(&path, b"not an image").unwrap();
        assert!(extract_image_attachments(&quoted).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn intake_count_preflight_precedes_file_decoding() {
        let path = temp_png("preflight");
        std::fs::write(&path, b"not an image").unwrap();
        let quoted = format!("\"{}\"", path.display());
        let five = std::iter::repeat_n(quoted.as_str(), 5)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(extract_image_attachments(&five)
            .unwrap_err()
            .to_string()
            .contains("select at most four images"));
        let existing = vec![data_url("image/png", b"placeholder"); 4];
        assert!(prepare_image_attachments(&quoted, &existing)
            .unwrap_err()
            .to_string()
            .contains("select at most four images"));
        let error = prepare_image_attachments(&quoted, &[]).unwrap_err();
        assert!(error.to_string().contains("cannot attach selected image"));
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_BYTES as u64 + 1).unwrap();
        drop(file);
        assert!(prepare_image_attachments(&quoted, &[])
            .unwrap_err()
            .to_string()
            .contains("each image must contain 1 byte to 8 MiB"));
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(100).unwrap();
        drop(file);
        assert!(extract_with_existing_sizes(&quoted, vec![MAX_BYTES; 2])
            .unwrap_err()
            .to_string()
            .contains("selected images exceed 16 MiB total"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn combined_intake_and_clipboard_refusal_keep_the_draft_selection() {
        let path = temp_png("combined");
        let line = format!("inspect \"{}\"", path.display());
        assert!(contains_image_path(&line));
        let url = image_file_to_data_url(path.to_str().unwrap()).unwrap();
        let mut images = vec![url.clone(); 4];
        assert!(prepare_image_attachments(&line, &images).is_err());
        assert_eq!(images, vec![url.clone(); 4]);
        assert!(append_clipboard_image(&mut images, url.clone()).is_err());
        assert_eq!(images, vec![url.clone(); 4]);
        images.pop();
        let (cleaned, from_line) = prepare_image_attachments(&line, &images).unwrap();
        assert_eq!(cleaned, "inspect");
        assert_eq!(from_line, vec![url]);
        assert_eq!(
            images.len(),
            3,
            "preparation does not consume clipboard input"
        );
        assert!(!contains_image_path(
            "inspect nonexistent-aizen-fixture.png"
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn clipboard_batch_enforces_total_bytes_before_mutating() {
        let url = format!(
            "data:image/png;base64,{}",
            "A".repeat(MAX_BYTES.div_ceil(3) * 4 - 4)
        );
        let mut images = vec![url.clone(), url.clone()];
        assert!(append_clipboard_image(&mut images, url).is_err());
        assert_eq!(images.len(), 2);
    }

    #[test]
    fn clipboard_keeps_original_pixels_beyond_old_dimension_cap() {
        let (w, h) = (2048, 12);
        let mut rgba = vec![255; (w * h * 4) as usize];
        for x in (1990..2030).step_by(2) {
            rgba[(3 * w as usize + x) * 4..(3 * w as usize + x) * 4 + 3].fill(0);
        }
        let url = clipboard_rgba_data_url(&rgba, w, h).unwrap();
        let image = crate::core::tool_media::ToolImage::from_data_url(
            &url,
            crate::core::tool_media::ImageProvenance::new("test", None),
        )
        .unwrap();
        assert_eq!(
            (image.metadata().width, image.metadata().height),
            (Some(w), Some(h))
        );
        let (_, decoded) = crate::core::tool_media::decode_png(&image.bytes().unwrap()).unwrap();
        assert_eq!(decoded, rgba);
        assert!(clipboard_rgba_data_url(&[0; 3], 1, 1).is_err());
        assert!(clipboard_rgba_data_url(&[], 4097, 4096).is_err());
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn png_round_trips_through_data_url() {
        let png = encode_png(&[255, 0, 0, 255], 1, 1).unwrap();
        assert!(
            png.starts_with(&[0x89, b'P', b'N', b'G']),
            "valid PNG signature"
        );
        assert!(data_url("image/png", &png).starts_with("data:image/png;base64,"));
    }
}
