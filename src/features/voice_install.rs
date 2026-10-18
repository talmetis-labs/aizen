//! `aizen voice install` — download, verify, and extract the sherpa-onnx speech engine + models
//! into the exact layout `ui::voice::{resolve_sherpa, resolve_en}` probes (`AIZEN_VOICE_HOME`
//! first, else `~/.aizen/voice`):
//!
//! ```text
//! ~/.aizen/voice/
//!   bin/sherpa-onnx-offline{.exe}   ← F9 offline CLI (resolve_sherpa's first candidate)
//!   sherpa-onnx{.exe}               ← F10 streaming CLI (flat — see install_engine)
//!   model/                          ← zipformer-vi int8 pack (F9)
//!   model_en/                       ← streaming zipformer-en pack (F10, --with-english)
//! ```
//!
//! Posture (CLAUDE.md): the STT engine stays an EXTERNAL sidecar process — never linked — and
//! extraction runs on pure-Rust `tar` + `bzip2-rs` (no libbz2 C, no toolchain anywhere). Every
//! asset streams through `core::download` (size-capped, hashed on the fly) and is checked
//! against the release's published SHA-256 BEFORE a byte is unpacked: the GitHub API's
//! per-asset `digest` for the v1.13.8 engine assets, or the `asr-models` release's
//! `checksum.txt` for the model packs (whose API digests are `null` — they predate the field).

use crate::core::download::{download_to, fetch_checksum, http_client, verify_checksum};
use crate::features::voice_manifest::{self, PackEntry};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const REPO: &str = "k2-fsa/sherpa-onnx";
/// Engine release tag — the asset names below embed the same version.
const ENGINE_TAG: &str = "v1.13.8";
/// Model release tag; this one also publishes `checksum.txt` (one sha256 per asset).
const MODEL_TAG: &str = "asr-models";
const VI_MODEL: &str = "sherpa-onnx-zipformer-vi-int8-2025-04-20.tar.bz2";
const EN_MODEL: &str = "sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2";
/// Runaway ceilings — GitHub's `Content-Length` is checked against these before the body
/// streams. The win-x64 static-MT engine tarball is 228 MiB, the en pack 484 MiB compressed.
const MAX_ENGINE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MODEL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Print one line to whichever surface owns the screen (same routing as `features::update`).
fn emit(line: &str) {
    crate::ui::tui::emit_line(line);
}

/// Human-readable size. `0` means GitHub did not report one (manifest fallback path).
fn fmt_size(bytes: u64) -> String {
    if bytes == 0 {
        return "—".to_string();
    }
    format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// The sherpa-onnx engine tarball for a platform, or a refusal naming the pair we don't ship.
fn engine_asset_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("sherpa-onnx-v1.13.8-win-x64-static-MT-Release-no-tts.tar.bz2"),
        ("macos", "aarch64") => Ok("sherpa-onnx-v1.13.8-osx-arm64-static-no-tts.tar.bz2"),
        ("linux", "x86_64") => Ok("sherpa-onnx-v1.13.8-linux-x64-static-no-tts.tar.bz2"),
        (os, arch) => bail!(
            "unsupported platform: {os}/{arch} — sherpa-onnx voice engines are published for \
             Windows x86_64, macOS arm64, and Linux x86_64 only"
        ),
    }
}

/// Where installs land: `AIZEN_VOICE_HOME` first (the resolvers honour it first too), else the
/// `~/.aizen/voice` layout — built from the same HOME vars `ui::voice` reads, deliberately NOT
/// `aizen_home()` (an `AIZEN_HOME` override must not move the install out of the resolver's reach).
fn voice_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("AIZEN_VOICE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    crate::core::config::home_dir().join(".aizen").join("voice")
}

/// Scratch dir for one extraction, INSIDE the install root so the post-extract moves are
/// same-volume renames (a temp-dir stage on another volume would degrade to copy+delete).
fn stage_dir(root: &Path) -> PathBuf {
    root.join(format!(".aizen-voice-stage-{}", std::process::id()))
}

/// What GitHub says about one release asset. `sha256` is the API's per-asset `digest`
/// (`sha256:…`), `None` when the asset predates the field — those carry their digest in the
/// release's `checksum.txt` instead.
struct AssetInfo {
    url: String,
    size: u64,
    sha256: Option<String>,
}

/// Read the release manifest and pull this asset's URL, size, and published digest.
async fn asset_info(tag: &str, asset_name: &str) -> Result<AssetInfo> {
    let api = format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}");
    crate::core::net_guard::guard_url_async(&api).await?;
    let client = http_client()?;
    let mut req = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = crate::core::cli_config::load()
        .reach
        .and_then(|r| r.resolved_github_token())
    {
        req = req.bearer_auth(token);
    }
    let response = req
        .send()
        .await
        .with_context(|| format!("fetching the {tag} release manifest"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("reading the release manifest")?;
    if !status.is_success() {
        bail!(
            "GitHub API returned HTTP {} for release {tag}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).context("parsing the release manifest")?;
    let asset = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(asset_name))
        })
        .with_context(|| format!("release {tag} does not publish {asset_name}"))?;
    let url = asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .context("the release asset has no download URL")?
        .to_string();
    let size = asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    Ok(AssetInfo { url, size, sha256 })
}

/// This asset's digest from a checksum file, as a `verify_checksum`-readable `<hex>  <name>`
/// line. Two shapes ship upstream: classic sha256sum (`<hex>  <name>`) and k2-fsa's
/// `checksum.txt` (`<name>\t<hex>` — field order reversed). `None` ⇒ names no digest for it.
fn digest_line(checksums: &str, asset_name: &str) -> Option<String> {
    for line in checksums.lines() {
        let mut it = line.split_whitespace();
        let Some(first) = it.next() else { continue };
        let Some(second) = it.next() else { continue };
        if first == asset_name {
            return Some(format!("{second}  {asset_name}"));
        }
        if second.trim_start_matches('*') == asset_name {
            return Some(format!("{first}  {asset_name}"));
        }
    }
    None
}

/// Verify the downloaded bytes against whatever this release publishes: the API `digest` first,
/// else the release's `checksum.txt`. NO published digest at all ⇒ install size-checked only,
/// said out loud — the same policy `features::update` uses for a release without a `.sha256`
/// asset. A mismatch is a refusal, never a warning.
///
/// Returns whether the bytes were digest-verified.
async fn verify_asset(
    tag: &str,
    asset_name: &str,
    api_sha256: Option<&str>,
    digest_hex: &str,
) -> Result<bool> {
    let verified = |hex16: &str| emit(&format!("verified SHA-256 {hex16}…"));
    if let Some(published) = api_sha256 {
        verify_checksum(
            &format!("{published}  {asset_name}"),
            asset_name,
            digest_hex,
        )?;
        verified(&digest_hex[..16]);
        return Ok(true);
    }
    let checksums_url = match std::env::var("AIZEN_VOICE_UPSTREAM_BASE") {
        Ok(base) if !base.trim().is_empty() => {
            format!("{}/{tag}/checksum.txt", base.trim_end_matches('/'))
        }
        _ => format!("https://github.com/{REPO}/releases/download/{tag}/checksum.txt"),
    };
    match fetch_checksum(&checksums_url).await {
        Ok(text) => match digest_line(&text, asset_name) {
            Some(line) => {
                verify_checksum(&line, asset_name, digest_hex)?;
                verified(&digest_hex[..16]);
                return Ok(true);
            }
            None => emit(&format!(
                "⚠ checksum.txt names no digest for {asset_name} — installing size-checked only"
            )),
        },
        Err(e) => emit(&format!(
            "⚠ no published sha256 for {asset_name} ({e}) — installing size-checked only"
        )),
    }
    Ok(false)
}

/// Download one `.tar.bz2` release asset into `root`, verify it, unpack it into the stage dir.
/// Returns the archive's SINGLE top-level directory (every one of these tarballs wraps its
/// payload in one root dir like `sherpa-onnx-zipformer-vi-int8-2025-04-20/`); an archive with
/// no wrapper yields the stage dir itself.
async fn fetch_asset(tag: &str, asset_name: &str, max_bytes: u64, root: &Path) -> Result<PathBuf> {
    let info = match std::env::var("AIZEN_VOICE_UPSTREAM_BASE") {
        Ok(base) if !base.trim().is_empty() => AssetInfo {
            url: format!("{}/{tag}/{asset_name}", base.trim_end_matches('/')),
            size: 0,
            sha256: None,
        },
        _ => match asset_info(tag, asset_name).await {
            Ok(info) => info,
            Err(e) => {
                // Manifest unreachable (API hiccup, rate limit): fall back to the fixed release URL.
                // The download itself still runs size-capped; only the digest check is lost.
                emit(&format!(
                    "⚠ release manifest unavailable ({e}) — using the fixed release URL"
                ));
                AssetInfo {
                    url: format!("https://github.com/{REPO}/releases/download/{tag}/{asset_name}"),
                    size: 0,
                    sha256: None,
                }
            }
        },
    };
    emit(&format!(
        "downloading {asset_name} ({})",
        fmt_size(info.size)
    ));
    let part = root.join(format!(".aizen-voice-{}-{asset_name}", std::process::id()));
    let (size, digest) = download_to(&info.url, &part, info.size, max_bytes)
        .await
        .with_context(|| format!("downloading {asset_name}"))?;
    if let Err(e) = verify_asset(tag, asset_name, info.sha256.as_deref(), &digest).await {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    emit(&format!("extracting {asset_name} ({})", fmt_size(size)));
    extract_archive(&part, root).await
}

/// Download one asset through the manifest entry, verifying the manifest's `size_bytes` and
/// `sha256` before unpacking it.
async fn fetch_manifest_asset(
    entry: &PackEntry,
    asset_name: &str,
    max_bytes: u64,
    root: &Path,
) -> Result<PathBuf> {
    emit(&format!(
        "downloading {asset_name} ({}) from manifest",
        fmt_size(entry.size_bytes)
    ));
    let part = root.join(format!(".aizen-voice-{}-{asset_name}", std::process::id()));
    let (size, digest) = download_to(&entry.url, &part, entry.size_bytes, max_bytes)
        .await
        .with_context(|| format!("downloading {asset_name} from the manifest"))?;
    if size != entry.size_bytes {
        let _ = std::fs::remove_file(&part);
        bail!(
            "manifest size mismatch for {asset_name}: expected {}, downloaded {size}",
            entry.size_bytes
        );
    }
    if !entry.sha256.eq_ignore_ascii_case(&digest) {
        let _ = std::fs::remove_file(&part);
        bail!(
            "manifest SHA-256 mismatch for {asset_name}: expected {}, got {digest}",
            entry.sha256
        );
    }
    emit(&format!("extracting {asset_name} ({})", fmt_size(size)));
    extract_archive(&part, root).await
}

/// Extract the already-downloaded archive into a clean stage and return its payload root.
async fn extract_archive(part: &Path, root: &Path) -> Result<PathBuf> {
    let stage = stage_dir(root);
    let _ = std::fs::remove_dir_all(&stage); // a stage from a crashed previous run
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let (p, s) = (part.to_path_buf(), stage.clone());
    let outcome = tokio::task::spawn_blocking(move || extract_tar_bz2(&p, &s))
        .await
        .context("extraction task panicked")?;
    let _ = std::fs::remove_file(part);
    match outcome {
        Ok(stripped) => Ok(stripped),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            Err(e)
        }
    }
}

/// Pure-Rust `.tar.bz2` unpack: `bzip2_rs::DecoderReader` (no libbz2) feeding `tar::Archive`.
/// Blocking disk work → always called inside `spawn_blocking`.
fn extract_tar_bz2(part: &Path, stage: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(part).with_context(|| format!("opening {}", part.display()))?;
    let mut archive = tar::Archive::new(bzip2_rs::DecoderReader::new(file));
    archive
        .unpack(stage)
        .with_context(|| format!("extracting {}", part.display()))?;
    stripped_root(stage)
}

/// The payload dir: the tarballs all carry ONE wrapper directory — return it so the install can
/// strip it (model lands at `voice/model/`, not `voice/sherpa-onnx-…/model/`). An archive that
/// extracts bare (several top-level entries) is its own root.
fn stripped_root(stage: &Path) -> Result<PathBuf> {
    let entries = std::fs::read_dir(stage)
        .with_context(|| format!("reading {}", stage.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() == 1 && entries[0].file_type()?.is_dir() {
        Ok(entries[0].path())
    } else {
        Ok(stage.to_path_buf())
    }
}

/// Rename `from` onto `to`, replacing an existing file (a re-run upgrades in place).
fn move_file(from: &Path, to: &Path) -> Result<()> {
    let _ = std::fs::remove_file(to);
    std::fs::rename(from, to).with_context(|| format!("installing {}", to.display()))
}

/// A runtime shared library (kept) versus a link-time archive (dropped).
fn is_shared_lib(name: &str) -> bool {
    name.ends_with(".so")
        || name.contains(".so.")
        || name.ends_with(".dylib")
        || name.ends_with(".dll")
}

/// Move the engine package's runtime into place: `bin/sherpa-onnx-offline` → `voice/bin/` (what
/// `resolve_sherpa` probes) and `bin/sherpa-onnx` parked FLAT at `voice/` (what `resolve_en`
/// probes). Shared libraries, when a package ships them, keep their relative home; link-time
/// archives do not travel.
///
/// The upstream package is a developer bundle: 19 CLIs (302 MB) and 11 static `.lib` archives
/// (993 MB) — measured 2026-10-17 on the win-x64 pack. Moving all of it turned a 74 MB model
/// install into 1.4 GB on disk, for the 35 MB of binaries the REPL ever spawns.
///
/// Returns whether the package carried the streaming CLI at all: if not, `--with-english` has
/// nothing to serve F10 — and no OTHER asset can supply it, because the platform tarball is the
/// only place these binaries are published (the v1.13.8 asset list has packages, no loose CLIs).
fn install_engine(engine_root: &Path, voice_dir: &Path) -> Result<bool> {
    let offline = exe_name("sherpa-onnx-offline");
    let streaming = exe_name("sherpa-onnx");
    let src_bin = engine_root.join("bin");
    if !src_bin.is_dir() || !src_bin.join(&offline).is_file() {
        bail!(
            "the engine archive has no bin/{offline} — not a sherpa-onnx package for {}/{}?",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }
    let dst_bin = voice_dir.join("bin");
    std::fs::create_dir_all(&dst_bin).with_context(|| format!("creating {}", dst_bin.display()))?;

    // Runtime shared libraries that sit beside the CLIs travel with them.
    for entry in
        std::fs::read_dir(&src_bin).with_context(|| format!("reading {}", src_bin.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_shared_lib(&name) {
            move_file(&entry.path(), &dst_bin.join(&name))?;
        }
    }
    move_file(&src_bin.join(&offline), &dst_bin.join(&offline))?;
    let has_streaming = src_bin.join(&streaming).is_file();
    if has_streaming {
        move_file(&src_bin.join(&streaming), &voice_dir.join(&streaming))?;
    }
    let src_lib = engine_root.join("lib");
    if src_lib.is_dir() {
        for entry in
            std::fs::read_dir(&src_lib).with_context(|| format!("reading {}", src_lib.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_shared_lib(&name) {
                continue; // static archives are link-time only
            }
            let dst_lib = voice_dir.join("lib");
            std::fs::create_dir_all(&dst_lib)
                .with_context(|| format!("creating {}", dst_lib.display()))?;
            move_file(&entry.path(), &dst_lib.join(&name))?;
        }
    }
    Ok(has_streaming)
}

/// A pack without `tokens.txt` is not a usable engine dir (layout drift or a partial extract) —
/// refuse BEFORE it replaces a working install.
fn require_pack(dir: &Path) -> Result<()> {
    if dir.join("tokens.txt").is_file() {
        Ok(())
    } else {
        bail!(
            "{} holds no tokens.txt — not a sherpa-onnx model pack; refusing to install it",
            dir.display()
        )
    }
}

/// Swap a freshly extracted pack into `dest` as a whole directory. The previous pack is renamed
/// aside first and restored if the swap fails: a failed re-install must never leave `model/`
/// missing — the bytes are re-downloadable, but a silently broken F9 is worse than the old pack.
fn replace_dir(new: &Path, dest: &Path) -> Result<()> {
    let name = dest
        .file_name()
        .context("destination has no file name")?
        .to_string_lossy()
        .into_owned();
    let backup = dest.with_file_name(format!(".{name}.old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&backup);
    let had_old = dest.exists();
    if had_old {
        std::fs::rename(dest, &backup)
            .with_context(|| format!("setting aside the previous {name}"))?;
    }
    if let Err(e) = std::fs::rename(new, dest) {
        if had_old {
            let _ = std::fs::rename(&backup, dest);
        }
        return Err(e).with_context(|| format!("installing {}", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(())
}

/// `aizen voice install [--with-english]` — the whole command.
pub async fn run(with_english: bool) -> Result<()> {
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let manifest = voice_manifest::fetch_manifest().await?;
    match manifest.as_ref() {
        Some(_) => emit("source: manifest v1"),
        None => emit("source: upstream"),
    }
    let voice_dir = voice_root();
    std::fs::create_dir_all(&voice_dir)
        .with_context(|| format!("creating {}", voice_dir.display()))?;
    emit(&format!(
        "voice engine for {}/{} → {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        voice_dir.display()
    ));

    // 1 — engine package: BOTH CLIs (offline F9 + streaming F10) come out of this one tarball.
    let engine_entry = manifest.as_ref().and_then(|m| m.engine(&platform));
    let engine_root = match engine_entry {
        Some(entry) => {
            let root = fetch_manifest_asset(entry, "engine", MAX_ENGINE_BYTES, &voice_dir).await?;
            root
        }
        None => {
            let asset = engine_asset_for(std::env::consts::OS, std::env::consts::ARCH)?;
            fetch_asset(ENGINE_TAG, asset, MAX_ENGINE_BYTES, &voice_dir).await?
        }
    };
    let has_streaming = install_engine(&engine_root, &voice_dir)?;
    let _ = std::fs::remove_dir_all(stage_dir(&voice_dir));
    if let Some(binaries) = engine_entry.and_then(|entry| entry.binaries.as_ref()) {
        for binary in binaries {
            let present =
                voice_dir.join(binary).is_file() || voice_dir.join("bin").join(binary).is_file();
            if !present {
                bail!("manifest engine binaries mismatch: {binary} was not installed");
            }
        }
    }
    if with_english && !has_streaming {
        emit("⚠ the engine package ships no streaming sherpa-onnx CLI — F10 (English) will be unavailable");
    }

    // 2 — F9 Vietnamese pack.
    let vi_entry = manifest.as_ref().and_then(|m| m.model("vi"));
    let model_root = match vi_entry {
        Some(entry) => {
            let root = fetch_manifest_asset(entry, "vi model", MAX_MODEL_BYTES, &voice_dir).await?;
            if let Some(files) = &entry.files {
                for file in files {
                    if !root.join(file).is_file() {
                        bail!("manifest vi files mismatch: {file} is missing");
                    }
                }
            }
            root
        }
        None => fetch_asset(MODEL_TAG, VI_MODEL, MAX_MODEL_BYTES, &voice_dir).await?,
    };
    require_pack(&model_root)?;
    replace_dir(&model_root, &voice_dir.join("model"))?;
    let _ = std::fs::remove_dir_all(stage_dir(&voice_dir));

    // 3 — F10 English pack (opt-in).
    if with_english {
        let en_entry = manifest.as_ref().and_then(|m| m.model("en"));
        let en_root = match en_entry {
            Some(entry) => {
                let root =
                    fetch_manifest_asset(entry, "en model", MAX_MODEL_BYTES, &voice_dir).await?;
                if let Some(files) = &entry.files {
                    for file in files {
                        if !root.join(file).is_file() {
                            bail!("manifest en files mismatch: {file} is missing");
                        }
                    }
                }
                root
            }
            None => fetch_asset(MODEL_TAG, EN_MODEL, MAX_MODEL_BYTES, &voice_dir).await?,
        };
        require_pack(&en_root)?;
        replace_dir(&en_root, &voice_dir.join("model_en"))?;
        let _ = std::fs::remove_dir_all(stage_dir(&voice_dir));
    }

    emit(&format!(
        "✓ engine + {} model pack{} installed under {}",
        if with_english { 2 } else { 1 },
        if with_english { "s" } else { "" },
        voice_dir.display()
    ));
    emit(
        "next: set \"voice\": { \"enabled\": true } in ~/.aizen/cli-config.json, \
         then hold F9 to talk (F10 = English)",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("aizen-voice-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn engine_assets_match_the_published_matrix() {
        assert_eq!(
            engine_asset_for("windows", "x86_64").unwrap(),
            "sherpa-onnx-v1.13.8-win-x64-static-MT-Release-no-tts.tar.bz2"
        );
        assert_eq!(
            engine_asset_for("macos", "aarch64").unwrap(),
            "sherpa-onnx-v1.13.8-osx-arm64-static-no-tts.tar.bz2"
        );
        assert_eq!(
            engine_asset_for("linux", "x86_64").unwrap(),
            "sherpa-onnx-v1.13.8-linux-x64-static-no-tts.tar.bz2"
        );
        for (os, arch) in [
            ("macos", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "aarch64"),
        ] {
            let err = engine_asset_for(os, arch).unwrap_err().to_string();
            assert!(err.contains("unsupported platform"), "{err}");
        }
    }

    #[test]
    fn digest_line_reads_both_upstream_checksum_shapes() {
        let sha = "ab".repeat(32);
        let asset = "sherpa-onnx-zipformer-vi-int8-2025-04-20.tar.bz2";
        // k2-fsa checksum.txt: `<name>\t<hex>` (fields reversed vs sha256sum).
        let tabbed = format!("{asset}\t{sha}");
        assert_eq!(
            digest_line(&tabbed, asset).as_deref(),
            Some(format!("{sha}  {asset}").as_str())
        );
        // Classic sha256sum shape, `*name` binary marker, other assets skipped.
        let classic = format!("{sha}  other.tar.bz2\n{sha} *target.tar.bz2\n");
        assert_eq!(
            digest_line(&classic, "target.tar.bz2").as_deref(),
            Some(format!("{sha}  target.tar.bz2").as_str())
        );
        assert!(digest_line(&classic, "absent.tar.bz2").is_none());
    }

    #[test]
    fn strip_the_single_wrapper_dir_but_keep_bare_archives() {
        // Wrapped: one top-level dir → that dir is the payload root.
        let root = scratch("wrapped");
        let pack = root.join("sherpa-onnx-pack");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("tokens.txt"), b"x").unwrap();
        assert_eq!(stripped_root(&root).unwrap(), pack);

        // Bare: several top-level entries → the stage itself is the root.
        let bare = scratch("bare");
        std::fs::write(bare.join("a.txt"), b"a").unwrap();
        std::fs::write(bare.join("b.txt"), b"b").unwrap();
        assert_eq!(stripped_root(&bare).unwrap(), bare);
    }

    #[test]
    fn replace_dir_swaps_the_pack_whole_and_cleans_the_backup() {
        let root = scratch("swap");
        let dest = root.join("model");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), b"old").unwrap();
        let new = root.join("pack");
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("tokens.txt"), b"new").unwrap();

        replace_dir(&new, &dest).unwrap();

        assert!(dest.join("tokens.txt").is_file());
        assert!(!dest.join("old.txt").exists());
        assert!(!new.exists());
        assert!(!root.join(".model.old-0").exists());
    }

    #[test]
    fn install_engine_keeps_only_the_two_clis_and_shared_libs() {
        // The upstream package is a 19-CLI developer bundle with ~1 GB of static archives; an
        // install must not copy any of that (measured 1.4 GB on disk before this gate).
        let root = scratch("engine");
        let bin = root.join("bin");
        let lib = root.join("lib");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        let offline = exe_name("sherpa-onnx-offline");
        let streaming = exe_name("sherpa-onnx");
        for name in [
            offline.as_str(),
            streaming.as_str(),
            "sherpa-onnx-vad.exe",
            "sherpa-onnx-version.exe",
        ] {
            std::fs::write(bin.join(name), b"x").unwrap();
        }
        std::fs::write(lib.join("libonnxruntime.a"), b"static").unwrap();
        std::fs::write(lib.join("libonnxruntime.so"), b"shared").unwrap();

        let voice = root.join("voice");
        let has_streaming = install_engine(&root, &voice).unwrap();

        assert!(has_streaming);
        assert!(voice.join("bin").join(&offline).is_file());
        assert!(
            voice.join(&streaming).is_file(),
            "streaming CLI must sit flat"
        );
        assert!(
            !voice.join("bin").join("sherpa-onnx-vad.exe").exists(),
            "unrelated CLIs must not be installed"
        );
        assert!(
            !voice.join("lib").join("libonnxruntime.a").exists(),
            "static archives are link-time only"
        );
        assert!(voice.join("lib").join("libonnxruntime.so").is_file());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manifest_install_uses_local_http_assets() {
        use serde_json::json;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn sha256(data: &[u8]) -> String {
            crate::core::download::hex_lower(
                ring::digest::digest(&ring::digest::SHA256, data).as_ref(),
            )
        }

        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice");
        let engine_bytes = std::fs::read(fixture.join("engine.tar.bz2")).unwrap();
        let model_bytes = std::fs::read(fixture.join("model.tar.bz2")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let offline = exe_name("sherpa-onnx-offline");
        let streaming = exe_name("sherpa-onnx");
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let mut engines = serde_json::Map::new();
        engines.insert(
            platform,
            json!({
                "url": format!("http://{address}/engine.tar.bz2"),
                "size_bytes": engine_bytes.len(),
                "sha256": sha256(&engine_bytes),
                "license": "Apache-2.0",
                "binaries": [offline.clone(), streaming.clone()],
                "min_aizen": "0.7.0"
            }),
        );
        let manifest = json!({
            "schema": 1,
            "generated_at": "2026-10-18T00:00:00Z",
            "upstream": {},
            "engines": engines,
            "models": {
                "vi": {
                    "url": format!("http://{address}/model.tar.bz2"),
                    "size_bytes": model_bytes.len(),
                    "sha256": sha256(&model_bytes),
                    "license": "Apache-2.0",
                    "files": ["tokens.txt"]
                }
            }
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let body: &[u8] = if request.starts_with("GET /manifest.json") {
                    &manifest_bytes
                } else if request.starts_with("GET /engine.tar.bz2") {
                    &engine_bytes
                } else {
                    &model_bytes
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });

        let root = scratch("manifest-install").join("voice");
        std::env::set_var("AIZEN_VOICE_HOME", &root);
        std::env::set_var("AIZEN_ALLOW_PRIVATE_NET", "1");
        std::env::set_var("NO_PROXY", "127.0.0.1");
        std::env::set_var(
            "AIZEN_VOICE_MANIFEST",
            format!("http://{address}/manifest.json"),
        );
        run(false).await.unwrap();
        std::env::remove_var("AIZEN_VOICE_MANIFEST");
        std::env::remove_var("AIZEN_ALLOW_PRIVATE_NET");
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("AIZEN_VOICE_HOME");
        server.abort();

        assert!(root.join("bin").join(offline).is_file());
        assert!(root.join(streaming).is_file());
        assert!(root.join("model").join("tokens.txt").is_file());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schema_two_manifest_falls_back_to_upstream_mirror() {
        use serde_json::json;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice");
        let engine_bytes = std::fs::read(fixture.join("engine.tar.bz2")).unwrap();
        let model_bytes = std::fs::read(fixture.join("model.tar.bz2")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let schema_two = serde_json::to_vec(&json!({
            "schema": 2,
            "generated_at": "2026-10-18T00:00:00Z",
            "upstream": {},
            "engines": {},
            "models": {}
        }))
        .unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let body: &[u8] = if request.starts_with("GET /manifest.json") {
                    &schema_two
                } else if request.contains("v1.13.8") {
                    &engine_bytes
                } else {
                    &model_bytes
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });

        let root = scratch("fallback-install").join("voice");
        std::env::set_var("AIZEN_VOICE_HOME", &root);
        std::env::set_var("AIZEN_ALLOW_PRIVATE_NET", "1");
        std::env::set_var("NO_PROXY", "127.0.0.1");
        std::env::set_var(
            "AIZEN_VOICE_MANIFEST",
            format!("http://{address}/manifest.json"),
        );
        std::env::set_var("AIZEN_VOICE_UPSTREAM_BASE", format!("http://{address}"));
        run(false).await.unwrap();
        std::env::remove_var("AIZEN_VOICE_MANIFEST");
        std::env::remove_var("AIZEN_VOICE_UPSTREAM_BASE");
        std::env::remove_var("AIZEN_ALLOW_PRIVATE_NET");
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("AIZEN_VOICE_HOME");
        server.abort();

        assert!(root
            .join("bin")
            .join(exe_name("sherpa-onnx-offline"))
            .is_file());
        assert!(root.join(exe_name("sherpa-onnx")).is_file());
        assert!(root.join("model").join("tokens.txt").is_file());
    }
}
