//! Streaming download + checksum verification shared by every asset install: the self-updater
//! (`features::update`) and `aizen voice install` alike. The body is streamed chunk by chunk and
//! hashed on the fly, so a 650 MB model never sits in RAM; the size cap is a parameter because a
//! release binary (300 MiB) and a voice model (650 MB) do not share a ceiling.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const CONNECT_TIMEOUT_SECS: u64 = 30;
const READ_TIMEOUT_SECS: u64 = 60;

/// The client used for asset downloads. Deliberately NOT a whole-response deadline: a 228 MiB
/// engine pack on a slow link legitimately takes minutes, and a total-request timeout turns every
/// such install into "operation timed out" at a fixed byte. `read_timeout` bounds each read
/// instead, so a stalled connection still fails while a slow one keeps making progress.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .read_timeout(Duration::from_secs(READ_TIMEOUT_SECS))
        .build()
        .context("building update client")
}

/// Lower-case hex of a digest.
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check a downloaded asset's SHA-256 against the published checksum text. Accepts the
/// `sha256sum` / `shasum -a 256` line shapes (`<hex>  <name>`, `<hex> *<name>`) and a bare hex
/// digest; when a line names a file, it must name THIS asset. A mismatch is a refusal, not a
/// warning: a tampered or corrupt binary must never be renamed over the live one (S4).
pub fn verify_checksum(published: &str, asset_name: &str, digest_hex: &str) -> Result<()> {
    for line in published.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(hex) = it.next() else {
            continue;
        };
        let named = it.next().map(|n| n.trim_start_matches('*'));
        if named.is_some_and(|n| n != asset_name) {
            continue; // another asset's line in a combined SHA256SUMS
        }
        if hex.eq_ignore_ascii_case(digest_hex) {
            return Ok(());
        }
        bail!(
            "refusing to install {asset_name}: SHA-256 mismatch (published {hex}, downloaded {digest_hex}) — the asset is corrupt or tampered"
        );
    }
    // Every line naming this asset returned above; reaching here means none did.
    bail!("refusing to install {asset_name}: the published checksum file names no digest for it")
}

/// Stream the asset to `part`, returning its size and SHA-256 (hex), refusing anything over
/// `max_bytes` (checked against the advertised `Content-Length` and against the running total).
/// The file is `sync_all`ed before it is handed to the swap: a rename over the live executable of
/// bytes still in the page cache is how a power cut leaves a zero-length `aizen` (S5).
pub async fn download_to(
    url: &str,
    part: &Path,
    expected_size: u64,
    max_bytes: u64,
) -> Result<(u64, String)> {
    let client = http_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("download returned HTTP {}", response.status().as_u16());
    }
    if response.content_length().is_some_and(|n| n > max_bytes) {
        bail!("release asset is too large");
    }
    match stream_asset(response, part, expected_size, max_bytes).await {
        Ok(done) => Ok(done),
        Err(e) => {
            // A half-written part file is not progress: remove it so a retry starts clean and a
            // 157 MB leftover never sits in ~/.aizen/voice pretending to be an install.
            let _ = tokio::fs::remove_file(part).await;
            Err(e)
        }
    }
}

/// Write the response body to `part`, hashing as it streams. Split out of `download_to` so every
/// failure path (read, write, size cap, short body) funnels through one cleanup.
async fn stream_asset(
    response: reqwest::Response,
    part: &Path,
    expected_size: u64,
    max_bytes: u64,
) -> Result<(u64, String)> {
    let mut file = tokio::fs::File::create(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut stream = response.bytes_stream();
    let mut total = 0u64;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading release asset")?;
        digest.update(&chunk);
        total = total.saturating_add(chunk.len() as u64);
        if total > max_bytes {
            bail!("release asset exceeds {} MiB", max_bytes / 1024 / 1024);
        }
        file.write_all(&chunk)
            .await
            .context("writing release asset")?;
    }
    file.flush().await.context("flushing release asset")?;
    file.sync_all()
        .await
        .context("syncing release asset to disk")?;
    drop(file);
    if expected_size > 0 && expected_size != total {
        bail!("release asset size mismatch (expected {expected_size}, downloaded {total})");
    }
    Ok((total, hex_lower(digest.finish().as_ref())))
}

/// Fetch the small published checksum text for a release asset.
pub async fn fetch_checksum(url: &str) -> Result<String> {
    let client = http_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading checksum {url}"))?;
    if !response.status().is_success() {
        bail!(
            "checksum download returned HTTP {}",
            response.status().as_u16()
        );
    }
    let text = response.text().await.context("reading checksum")?;
    if text.len() > 64 * 1024 {
        bail!("checksum file is implausibly large");
    }
    Ok(text)
}
