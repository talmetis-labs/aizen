// Copyright 2026 Aizen authors. Licensed under Apache-2.0.
//! Explicit Core codebase model provisioning, never invoked by retrieval or a Memory provider.
//!
//! Downloads the three Model2Vec assets from `minishlab` on Hugging Face. HTTP uses the existing
//! rustls-only stack, even in a build without `dense`; loading remains local-only and feature-gated.
//! Unique, exclusively created partial files are removed on errors/cancellation. Publishing uses
//! a hard link, not an overwriting rename, so concurrent downloads cannot clobber a completed file.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The three files `model2vec-rs::from_pretrained` reads from a local model folder.
const FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// The largest supported asset is the multilingual safetensors (~530 MB).
const MAX_FILE_BYTES: u64 = 700 * 1024 * 1024;

/// Provision `models_dir/<name>` only after an explicit host CLI request. Existing regular,
/// nonempty assets are kept; this is resumable at file granularity, not a model integrity check.
/// Absolute paths supported by the local loader are deliberately NOT download destinations.
pub async fn download(models_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let dir = models_dir.join(name);
    guard_directory(&dir)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating model dir {}", dir.display()))?;
    guard_directory(&dir)?;

    // HF redirects its resolve URLs to a CDN. The normal reqwest redirect bound still applies.
    let client = reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .context("building download client")?;

    println!(
        "Downloading Core codebase model '{name}' → {}",
        dir.display()
    );
    for file in FILES {
        let dest = dir.join(file);
        if asset_present(&dest)? {
            println!("  {file}: already present, skipping");
            continue;
        }
        let url =
            format!("https://huggingface.co/minishlab/{name}/resolve/main/{file}?download=true");
        fetch_to_file(&client, &url, &dest, MAX_FILE_BYTES)
            .await
            .with_context(|| format!("downloading {file}"))?;
    }
    println!("Model '{name}' ready for Core codebase retrieval in a `--features dense` build.");
    Ok(dir)
}

/// A single portable HF repository slug, not a filesystem path or an arbitrary URL segment.
fn validate_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let portable = !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !name.contains("..");
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if !portable || reserved {
        bail!(
            "invalid download model name {name:?}: use a single portable minishlab model name \
             (for example potion-base-8M), not a local path or URL; pass an explicit name if \
             embed_model selects a local directory"
        );
    }
    Ok(())
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Covers junctions and other reparse points as well as symbolic links.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Refuse existing symlink/reparse components before creating directories or publishing files.
/// This checks the host's configured path; it is not a sandbox against concurrent parent swaps.
fn guard_directory(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => {}
            Ok(_) => bail!(
                "model directory component is not a plain directory: {}",
                ancestor.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", ancestor.display()))
            }
        }
    }
    Ok(())
}

fn asset_present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || is_link(&metadata) {
                bail!("model asset is not a regular file: {}", path.display());
            }
            if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
                bail!("existing model asset has an invalid size: {}; inspect/remove it before retrying", path.display());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

/// The guard is established synchronously with create_new, so cancellation cannot leave an
/// untracked file between an asynchronous open and assigning its cleanup guard.
struct PartialFile(PathBuf);

impl PartialFile {
    fn create(parent: &Path) -> Result<(Self, std::fs::File)> {
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|error| anyhow::anyhow!("generating partial file name: {error}"))?;
        let path = parent.join(format!(
            ".aizen-model-{:032x}.part",
            u128::from_le_bytes(random)
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok((Self(path), file))
    }

    fn publish(&self, dest: &Path) -> Result<()> {
        let parent = dest
            .parent()
            .context("model asset needs a parent directory")?;
        guard_directory(parent)?;
        // Atomic create-if-absent on both Unix and Windows. Do not fall back to rename: Unix
        // rename silently replaces an existing destination, and exists()+rename is racy.
        std::fs::hard_link(&self.0, dest).with_context(|| {
            format!(
                "publishing {} without replacing an existing file",
                dest.display()
            )
        })
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn fetch_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    max_bytes: u64,
) -> Result<()> {
    let parent = dest
        .parent()
        .context("model asset needs a parent directory")?;
    guard_directory(parent)?;
    if asset_present(dest)? {
        bail!(
            "refusing to replace an existing model asset: {}",
            dest.display()
        );
    }
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("HTTP {} from {url}", resp.status().as_u16());
    }
    let expected = resp.content_length();
    if let Some(len) = expected {
        if len > max_bytes {
            bail!("file too large ({len} bytes > {max_bytes})");
        }
    }
    // Keep the file synchronous in this explicit host CLI. Tokio file writes can outlive a
    // cancelled future in its blocking pool, preventing Windows cleanup from removing a partial.
    guard_directory(parent)?;
    let (partial, mut file) = PartialFile::create(parent)?;
    let mut written = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response chunk")?;
        written += chunk.len() as u64;
        if written > max_bytes {
            bail!("file exceeded {max_bytes} bytes mid-stream");
        }
        file.write_all(&chunk).context("writing chunk to disk")?;
    }
    if written == 0 || expected.is_some_and(|len| len != written) {
        bail!("empty or incomplete model asset ({written} bytes)");
    }
    file.flush().context("flushing model asset")?;
    file.sync_all().context("syncing model asset")?;
    drop(file);
    partial.publish(dest)?;
    println!(
        "  {}: {} KB",
        dest.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
        written / 1024
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            // Canonicalize the host temp root so e.g. macOS's /var symlink is not part of the
            // fixture's path. Every resource below it is owned by this test, never HOME.
            let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let mut random = [0u8; 16];
            getrandom::getrandom(&mut random).unwrap();
            let path = root.join(format!(
                "aizen-model-test-{:032x}",
                u128::from_le_bytes(random)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn entries(&self) -> usize {
            std::fs::read_dir(&self.0).unwrap().count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    async fn serve_once(response: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let byte = tokio::time::timeout(Duration::from_secs(5), socket.read_u8())
                    .await
                    .unwrap()
                    .unwrap();
                headers.push(byte);
                assert!(headers.len() <= 4096);
            }
            socket.write_all(response).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (format!("http://{address}/model"), server)
    }

    #[test]
    fn accepts_portable_model_names() {
        for name in ["potion-base-8M", "potion-multilingual-128M", "model_v1.2"] {
            validate_name(name).unwrap();
        }
        assert!(validate_name(&"a".repeat(129)).is_err());
    }

    #[tokio::test]
    async fn rejects_paths_and_url_segments_before_any_io() {
        let fixture = Fixture::new();
        let root = fixture.0.join("not-created");
        for name in [
            "",
            " ",
            ".",
            "..",
            "../outside",
            "a/b",
            r"a\b",
            "/absolute",
            r"C:\model",
            "https://host/model",
            "%2e%2e",
            "model?x",
            "model#x",
            "model\n",
            "model.",
            "a..b",
            "CON",
            "nul.json",
            "COM1",
            "lpt9.json",
        ] {
            assert!(download(&root, name).await.is_err(), "accepted {name:?}");
            assert!(!root.exists());
        }
        assert_eq!(fixture.entries(), 0);
    }

    #[test]
    fn existing_assets_must_be_regular_and_nonempty() {
        let fixture = Fixture::new();
        let dest = fixture.0.join("config.json");
        assert!(!asset_present(&dest).unwrap());
        std::fs::write(&dest, b"").unwrap();
        assert!(asset_present(&dest).is_err());
        std::fs::write(&dest, b"complete").unwrap();
        assert!(asset_present(&dest).unwrap());
        assert!(asset_present(&fixture.0).is_err());
    }

    #[tokio::test]
    async fn complete_model_rerun_uses_owned_assets_without_network() {
        let fixture = Fixture::new();
        let model = fixture.0.join("potion-base-8M");
        std::fs::create_dir(&model).unwrap();
        for asset in FILES {
            std::fs::write(model.join(asset), b"existing").unwrap();
        }
        assert_eq!(download(&fixture.0, "potion-base-8M").await.unwrap(), model);
        for asset in FILES {
            assert_eq!(std::fs::read(model.join(asset)).unwrap(), b"existing");
        }
        assert_eq!(std::fs::read_dir(model).unwrap().count(), FILES.len());
    }

    #[test]
    fn refuses_plain_file_in_model_directory_path() {
        let fixture = Fixture::new();
        let file = fixture.0.join("not-a-directory");
        std::fs::write(&file, b"keep").unwrap();
        assert!(guard_directory(&file).is_err());
        assert!(guard_directory(&file.join("child")).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    #[test]
    fn exclusive_partials_clean_up_and_publication_never_clobbers() {
        let fixture = Fixture::new();
        let dest = fixture.0.join("model.safetensors");
        let (partial, mut file) = PartialFile::create(&fixture.0).unwrap();
        let (other, other_file) = PartialFile::create(&fixture.0).unwrap();
        assert_ne!(partial.0, other.0);
        drop(other_file);
        drop(other);
        std::io::Write::write_all(&mut file, b"new").unwrap();
        drop(file);
        let part_path = partial.0.clone();
        std::fs::write(&dest, b"existing").unwrap();
        assert!(partial.publish(&dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
        drop(partial);
        assert!(!part_path.exists());
        assert_eq!(fixture.entries(), 1);
    }

    #[tokio::test]
    async fn loopback_success_publishes_complete_file_and_removes_partial() {
        let fixture = Fixture::new();
        let dest = fixture.0.join("config.json");
        let (url, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncomplete",
        )
        .await;
        fetch_to_file(&client(), &url, &dest, 32).await.unwrap();
        server.await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete");
        assert_eq!(fixture.entries(), 1);
        // A pre-existing asset is rejected before even parsing this deliberately invalid URL.
        assert!(fetch_to_file(&client(), "not a URL", &dest, 32)
            .await
            .is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete");
    }

    #[tokio::test]
    async fn loopback_failures_leave_no_asset_or_partial() {
        let fixture = Fixture::new();
        let dest = fixture.0.join("model.safetensors");
        let responses: &[&[u8]] = &[
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n123456789\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nshort",
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ];
        for response in responses {
            let (url, server) = serve_once(response).await;
            assert!(fetch_to_file(&client(), &url, &dest, 8).await.is_err());
            server.await.unwrap();
            assert!(!dest.exists());
            assert_eq!(fixture.entries(), 0);
        }
    }

    #[tokio::test]
    async fn cancelled_download_removes_its_owned_partial() {
        let fixture = Fixture::new();
        let dest = fixture.0.join("model.safetensors");
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(socket.read_u8().await.unwrap());
                assert!(headers.len() <= 4096);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nstart",
                )
                .await
                .unwrap();
            let _ = sent.send(());
            std::future::pending::<()>().await;
        });
        let client = client();
        let url = format!("http://{address}/model");
        {
            let download = fetch_to_file(&client, &url, &dest, 128);
            tokio::pin!(download);
            // The server leaves its body incomplete, so success cannot race cancellation.
            let wait_for_partial = async {
                received.await.unwrap();
                loop {
                    if fixture.entries() != 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut download => panic!("download unexpectedly completed: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(5), wait_for_partial) => result.unwrap(),
            }
            // Leaving this scope cancels the actual future, not just a Pin<&mut _> handle.
        }
        assert!(!dest.exists());
        assert_eq!(fixture.entries(), 0);
        server.abort();
        let _ = server.await;
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_model_directories_and_assets() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = fixture.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = fixture.0.join("model");
        symlink(&outside, &link).unwrap();
        assert!(guard_directory(&link).is_err());
        assert!(guard_directory(&link.join("new-child")).is_err());
        let asset = fixture.0.join("config.json");
        symlink(outside.join("not-created"), &asset).unwrap();
        assert!(asset_present(&asset).is_err());
        assert!(!outside.join("not-created").exists());
    }
}
