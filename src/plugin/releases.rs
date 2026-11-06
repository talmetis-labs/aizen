//! Explicit, anonymous access to the public Mnemos binary channel. Never executes a provider.
//! Checksums establish exact bytes, not publisher authentication or permission to run them.
use super::{install as package_install, manager, manifest};
use anyhow::Result;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    time::Duration,
};
use tokio::time::Instant;
use url::Url;

pub(crate) const REPOSITORY: &str = "talmetis-labs/aizen-mnemos-releases";
const TARGETS: [&str; 3] = [
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
];
const MAX_ARCHIVE: u64 = 272 * 1024 * 1024;
const MAX_JSON: usize = 2 * 1024 * 1024;
const MAX_CHECKSUMS: usize = 64 * 1024;
const CHECK_SECONDS: u64 = 30;
const INSTALL_SECONDS: u64 = 300;
const MAX_ID: u64 = 9_007_199_254_740_991;

#[derive(Debug)]
struct ReleaseError {
    code: &'static str,
    message: String,
}
impl std::fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ReleaseError {}
pub(crate) fn fail(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    ReleaseError {
        code,
        message: message.into(),
    }
    .into()
}
pub(crate) fn error_code(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<ReleaseError>()
        .map_or("integrity_error", |e| e.code)
}
fn require(ok: bool, message: &'static str) -> Result<()> {
    if !ok {
        return Err(fail("integrity_error", message));
    }
    Ok(())
}
fn accumulate(total: &mut u64, chunk: usize, limit: u64) -> Result<()> {
    require(
        chunk as u64 <= limit.saturating_sub(*total),
        "Mnemos response exceeded its reviewed byte bound",
    )?;
    *total += chunk as u64;
    Ok(())
}

/// Only canonical stable versions emitted by the Mnemos packager are accepted.
pub(crate) fn version(value: &str) -> Result<[u32; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    require(parts.len() == 3, "Expected a stable X.Y.Z Mnemos version")?;
    let mut result = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        require(
            !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|c| c.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0')),
            "Expected a canonical stable Mnemos version",
        )?;
        result[index] = part
            .parse()
            .map_err(|_| fail("integrity_error", "Mnemos version component is invalid"))?;
    }
    Ok(result)
}
fn filenames(value: &str) -> BTreeSet<String> {
    TARGETS
        .iter()
        .map(|target| format!("mnemos-{value}-{target}.zip"))
        .collect()
}
fn archive_name(value: &str, target: &str) -> String {
    format!("mnemos-{value}-{target}.zip")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub release_id: u64,
    pub version: String,
    pub target: String,
    pub asset_id: u64,
    pub filename: String,
    pub bytes: u64,
    pub sha256: String,
    pub checksums_asset_id: u64,
    pub checksums_sha256: String,
}
impl Candidate {
    pub(crate) fn validate(&self) -> Result<()> {
        version(&self.version)?;
        require(
            TARGETS.contains(&self.target.as_str()),
            "Unsupported Mnemos package target",
        )?;
        require(
            self.release_id > 0
                && self.release_id <= MAX_ID
                && self.asset_id > 0
                && self.asset_id <= MAX_ID
                && self.checksums_asset_id > 0
                && self.checksums_asset_id <= MAX_ID
                && self.asset_id != self.checksums_asset_id,
            "Invalid Mnemos release or asset identity",
        )?;
        require(
            self.filename == archive_name(&self.version, &self.target),
            "Mnemos package filename does not match its version and target",
        )?;
        require(
            self.bytes > 0 && self.bytes <= MAX_ARCHIVE,
            "Mnemos archive size is invalid",
        )?;
        manifest::validate_digest(&self.sha256)
            .map_err(|_| fail("integrity_error", "Invalid Mnemos archive SHA256"))?;
        manifest::validate_digest(&self.checksums_sha256)
            .map_err(|_| fail("integrity_error", "Invalid Mnemos checksums SHA256"))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InstalledVersion {
    pub version: Option<String>,
    pub verified: bool,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReleaseStatus {
    pub repository: String,
    pub target: String,
    pub installed: Option<InstalledVersion>,
    pub highest_seen_version: Option<String>,
    pub candidate: Option<Candidate>,
    pub comparison: String,
    pub unavailable_reason: Option<String>,
    pub discovery: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    id: u64,
    name: String,
    size: u64,
    state: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
struct Release {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
impl Release {
    fn stable_version(&self) -> Option<&str> {
        if self.id == 0 || self.id > MAX_ID || self.draft || self.prerelease {
            return None;
        }
        let value = self.tag_name.strip_prefix('v')?;
        version(value).ok()?;
        Some(value)
    }
    fn inventory(&self) -> Result<BTreeMap<String, Asset>> {
        let value = self.stable_version().ok_or_else(|| {
            fail(
                "release_unavailable",
                "Mnemos release is not a published stable version",
            )
        })?;
        let mut expected = filenames(value);
        expected.insert("SHA256SUMS".into());
        require(
            self.assets.len() == expected.len(),
            "Mnemos release needs three ZIPs and SHA256SUMS",
        )?;
        let mut assets = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for asset in &self.assets {
            require(
                expected.contains(&asset.name)
                    && ids.insert(asset.id)
                    && asset.id > 0
                    && asset.id <= MAX_ID
                    && asset.state == "uploaded"
                    && asset.size > 0
                    && asset.size
                        <= if asset.name == "SHA256SUMS" {
                            MAX_CHECKSUMS as u64
                        } else {
                            MAX_ARCHIVE
                        },
                "Mnemos release has invalid, duplicate or unexpected assets",
            )?;
            require(
                asset.browser_download_url == asset_url(value, &asset.name),
                "Mnemos asset escaped the fixed public release channel",
            )?;
            if let Some(digest) = &asset.digest {
                let digest = digest
                    .strip_prefix("sha256:")
                    .ok_or_else(|| fail("integrity_error", "Unsupported GitHub release digest"))?;
                manifest::validate_digest(digest)
                    .map_err(|_| fail("integrity_error", "Invalid GitHub release digest"))?;
            }
            require(
                assets.insert(asset.name.clone(), asset.clone()).is_none(),
                "Duplicate Mnemos release asset name",
            )?;
        }
        require(
            assets.keys().cloned().collect::<BTreeSet<_>>() == expected,
            "Mnemos release assets are incomplete",
        )?;
        Ok(assets)
    }
}

fn checksums(bytes: &[u8], value: &str) -> Result<BTreeMap<String, String>> {
    require(
        !bytes.is_empty() && bytes.len() <= MAX_CHECKSUMS && bytes.is_ascii(),
        "Mnemos checksum file is invalid or oversized",
    )?;
    let text = std::str::from_utf8(bytes)
        .map_err(|_| fail("integrity_error", "Mnemos checksums must be ASCII"))?;
    require(
        text.ends_with('\n') && !text.contains('\r'),
        "Mnemos checksums require canonical newline-delimited rows",
    )?;
    let expected = filenames(value);
    let mut result = BTreeMap::new();
    for row in text.strip_suffix('\n').unwrap_or_default().split('\n') {
        let (digest, name) = row
            .split_once("  ")
            .ok_or_else(|| fail("integrity_error", "Malformed Mnemos checksum row"))?;
        manifest::validate_digest(digest)
            .map_err(|_| fail("integrity_error", "Malformed Mnemos checksum digest"))?;
        require(
            expected.contains(name) && result.insert(name.to_owned(), digest.to_owned()).is_none(),
            "Duplicate or unexpected Mnemos checksum filename",
        )?;
    }
    require(
        result.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "Mnemos checksum file is incomplete",
    )?;
    Ok(result)
}
fn asset_url(value: &str, name: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/v{value}/{name}")
}
fn asset_digest(asset: &Asset, actual: &str) -> Result<()> {
    if let Some(digest) = &asset.digest {
        require(
            digest == &format!("sha256:{actual}"),
            "Downloaded Mnemos asset differs from its GitHub digest",
        )?;
    }
    Ok(())
}

// Typed, injectable seam: fixtures cannot replace the production repository or URLs.
#[allow(async_fn_in_trait)]
trait Source {
    async fn list(&self, page: usize) -> Result<(Vec<Release>, bool)>;
    async fn by_tag(&self, value: &str) -> Result<Option<Release>>;
    async fn by_id(&self, id: u64) -> Result<Release>;
    async fn checksum_bytes(&self, asset: &Asset) -> Result<Vec<u8>>;
    async fn download(&self, asset: &Asset, digest: &str, file: &mut File) -> Result<()>;
}

struct Http {
    client: reqwest::Client,
    deadline: Instant,
}
impl Http {
    fn new(seconds: u64) -> Result<Self> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .referer(false)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!(
                "aizen/",
                env!("CARGO_PKG_VERSION"),
                " mnemos-releases"
            ))
            .build()
            .map_err(|_| {
                fail(
                    "network_error",
                    "Could not initialize the anonymous Mnemos release client",
                )
            })?;
        Ok(Self {
            client,
            deadline: Instant::now() + Duration::from_secs(seconds),
        })
    }
    async fn response(&self, initial: &str, asset: bool) -> Result<reqwest::Response> {
        let mut url = Url::parse(initial)
            .map_err(|_| fail("integrity_error", "Invalid Mnemos release endpoint"))?;
        validate_url(&url, asset, false)?;
        for hop in 0..=3 {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| fail("network_error", "Mnemos release request timed out"))?;
            let response = self
                .client
                .get(url.clone())
                .timeout(remaining)
                .send()
                .await
                .map_err(|_| {
                    fail(
                        "network_error",
                        "Mnemos release request failed or timed out",
                    )
                })?;
            if response.status().is_redirection() {
                require(
                    asset && hop < 3,
                    "Unexpected or excessive Mnemos release redirects",
                )?;
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|h| h.to_str().ok())
                    .filter(|s| s.len() <= 8192)
                    .ok_or_else(|| fail("integrity_error", "Invalid Mnemos asset redirect"))?;
                let next = url
                    .join(location)
                    .map_err(|_| fail("integrity_error", "Invalid Mnemos asset redirect"))?;
                validate_url(&next, true, true)?;
                url = next;
                continue;
            }
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (response.status() == reqwest::StatusCode::FORBIDDEN
                    && response
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_some_and(|v| v == "0"))
            {
                return Err(fail(
                    "rate_limited",
                    "GitHub public release rate limit reached; try again later",
                ));
            }
            return Ok(response);
        }
        Err(fail("integrity_error", "Excessive Mnemos asset redirects"))
    }
    async fn bounded(
        &self,
        response: reqwest::Response,
        limit: usize,
        exact: Option<u64>,
    ) -> Result<Vec<u8>> {
        require(
            response.status().is_success(),
            "Mnemos release endpoint returned an unsuccessful response",
        )?;
        require(
            response
                .content_length()
                .is_none_or(|n| n <= limit as u64 && exact.is_none_or(|e| e == n)),
            "Mnemos response exceeds its expected byte bound",
        )?;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| fail("network_error", "Mnemos response was interrupted"))?;
            require(
                chunk.len() <= limit.saturating_sub(bytes.len()),
                "Mnemos response exceeded its byte bound",
            )?;
            bytes.extend_from_slice(&chunk);
        }
        require(
            exact.is_none_or(|n| n == bytes.len() as u64),
            "Mnemos response was incomplete",
        )?;
        Ok(bytes)
    }
    async fn metadata(&self, suffix: &str) -> Result<Option<(Vec<u8>, bool)>> {
        let response = self
            .response(
                &format!("https://api.github.com/repos/{REPOSITORY}/{suffix}"),
                false,
            )
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(fail(
                "network_error",
                "GitHub public release metadata is unavailable",
            ));
        }
        let next = response
            .headers()
            .get_all(reqwest::header::LINK)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .any(|h| h.split(',').any(|link| link.contains("rel=\"next\"")));
        Ok(Some((self.bounded(response, MAX_JSON, None).await?, next)))
    }
}
fn validate_url(url: &Url, asset: bool, redirect: bool) -> Result<()> {
    require(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.fragment().is_none(),
        "Unsafe Mnemos release URL",
    )?;
    let permitted = if !asset {
        !redirect
            && url.host_str() == Some("api.github.com")
            && url
                .path()
                .starts_with(&format!("/repos/{REPOSITORY}/releases"))
    } else if redirect {
        matches!(
            url.host_str(),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
    } else {
        url.host_str() == Some("github.com")
            && url.query().is_none()
            && url
                .path()
                .starts_with(&format!("/{REPOSITORY}/releases/download/v"))
    };
    require(
        permitted,
        "Mnemos download left the fixed public release channel",
    )
}
impl Source for Http {
    async fn list(&self, page: usize) -> Result<(Vec<Release>, bool)> {
        let (bytes, next) = self
            .metadata(&format!("releases?per_page=100&page={page}"))
            .await?
            .ok_or_else(|| {
                fail(
                    "release_unavailable",
                    "The public Mnemos release channel is unavailable",
                )
            })?;
        let releases: Vec<Release> = serde_json::from_slice(&bytes)
            .map_err(|_| fail("integrity_error", "Invalid GitHub release metadata"))?;
        require(
            releases.len() <= 100,
            "Mnemos release page exceeded its item bound",
        )?;
        let count = releases.len();
        Ok((releases, next || count == 100))
    }
    async fn by_tag(&self, value: &str) -> Result<Option<Release>> {
        version(value)?;
        self.metadata(&format!("releases/tags/v{value}"))
            .await?
            .map(|(bytes, _)| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| fail("integrity_error", "Invalid GitHub release metadata"))
            })
            .transpose()
    }
    async fn by_id(&self, id: u64) -> Result<Release> {
        let (bytes, _) = self
            .metadata(&format!("releases/{id}"))
            .await?
            .ok_or_else(|| {
                fail(
                    "release_changed",
                    "The reviewed Mnemos release is no longer available",
                )
            })?;
        serde_json::from_slice(&bytes)
            .map_err(|_| fail("integrity_error", "Invalid GitHub release metadata"))
    }
    async fn checksum_bytes(&self, asset: &Asset) -> Result<Vec<u8>> {
        let response = self.response(&asset.browser_download_url, true).await?;
        let bytes = self
            .bounded(response, MAX_CHECKSUMS, Some(asset.size))
            .await?;
        asset_digest(asset, &manifest::sha256(&bytes))?;
        Ok(bytes)
    }
    async fn download(&self, asset: &Asset, digest: &str, file: &mut File) -> Result<()> {
        let response = self.response(&asset.browser_download_url, true).await?;
        if !response.status().is_success() {
            return Err(fail(
                "network_error",
                "The reviewed Mnemos archive is unavailable",
            ));
        }
        require(
            response.content_length().is_none_or(|n| n == asset.size),
            "Mnemos archive response length changed",
        )?;
        let mut stream = response.bytes_stream();
        let mut bytes = 0u64;
        let mut hash = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|_| fail("network_error", "Mnemos archive download was interrupted"))?;
            accumulate(&mut bytes, chunk.len(), asset.size)?;
            hash.update(&chunk);
            file.write_all(&chunk).map_err(|_| {
                fail(
                    "integrity_error",
                    "Could not write the private Mnemos download",
                )
            })?;
        }
        require(
            bytes == asset.size && bytes > 0,
            "Mnemos archive download was incomplete",
        )?;
        let actual = manifest::hex_digest(&hash.finalize());
        require(actual == digest, "Mnemos archive checksum mismatch")?;
        asset_digest(asset, &actual)?;
        file.flush().and_then(|_| file.sync_all()).map_err(|_| {
            fail(
                "integrity_error",
                "Could not sync the private Mnemos download",
            )
        })?;
        Ok(())
    }
}

async fn candidate<S: Source>(source: &S, release: &Release, target: &str) -> Result<Candidate> {
    let value = release
        .stable_version()
        .ok_or_else(|| fail("release_unavailable", "Mnemos release is not stable"))?;
    let assets = release.inventory()?;
    let checksum_asset = &assets["SHA256SUMS"];
    let bytes = source.checksum_bytes(checksum_asset).await?;
    require(
        bytes.len() as u64 == checksum_asset.size,
        "Mnemos checksums differ from their reviewed size",
    )?;
    asset_digest(checksum_asset, &manifest::sha256(&bytes))?;
    let sums = checksums(&bytes, value)?;
    let filename = archive_name(value, target);
    let asset = assets.get(&filename).ok_or_else(|| {
        fail(
            "unsupported_target",
            "Mnemos has no package for this host target",
        )
    })?;
    let result = Candidate {
        release_id: release.id,
        version: value.into(),
        target: target.into(),
        asset_id: asset.id,
        filename: filename.clone(),
        bytes: asset.size,
        sha256: sums[&filename].clone(),
        checksums_asset_id: checksum_asset.id,
        checksums_sha256: manifest::sha256(&bytes),
    };
    result.validate()?;
    if let Some(digest) = &asset.digest {
        require(
            digest == &format!("sha256:{}", result.sha256),
            "Mnemos archive metadata and checksums disagree",
        )?;
    }
    Ok(result)
}
async fn discover<S: Source>(
    source: &S,
    target: &str,
    exact: Option<&str>,
) -> Result<(Option<String>, Option<Candidate>, bool)> {
    let mut releases = Vec::new();
    let mut complete = true;
    if let Some(value) = exact {
        version(value)?;
        if let Some(release) = source.by_tag(value).await? {
            require(
                release.stable_version() == Some(value),
                "Mnemos release tag did not match the requested version",
            )?;
            releases.push(release);
        }
    } else {
        for page in 1..=3 {
            let (mut batch, next) = source.list(page).await?;
            releases.append(&mut batch);
            if !next {
                break;
            }
            if page == 3 {
                complete = false;
            }
        }
    }
    let mut highest = None::<([u32; 3], String)>;
    let mut eligible = Vec::new();
    let mut seen = BTreeSet::new();
    for release in releases {
        let Some(value) = release.stable_version() else {
            continue;
        };
        let order = version(value)?;
        require(
            seen.insert(value.to_owned()),
            "Duplicate stable Mnemos release versions",
        )?;
        if highest.as_ref().is_none_or(|(v, _)| order > *v) {
            highest = Some((order, value.into()));
        }
        if release.inventory().is_ok() {
            eligible.push((order, release));
        }
    }
    let highest = highest.map(|(_, value)| value);
    if !complete {
        return Ok((highest, None, false));
    }
    eligible.sort_by_key(|(order, _)| std::cmp::Reverse(*order));
    let selected = match eligible.first() {
        Some((_, release)) => Some(candidate(source, release, target).await?),
        None => None,
    };
    Ok((highest, selected, true))
}
pub(crate) fn installed_version() -> Result<Option<InstalledVersion>> {
    let state = manager::read_state()?;
    if !state.packages.contains_key("mnemos") {
        return Ok(None);
    }
    Ok(Some(match manager::info("mnemos") {
        Ok(package) => InstalledVersion {
            version: Some(package.manifest.version),
            verified: true,
            error: None,
        },
        Err(_) => InstalledVersion {
            version: None,
            verified: false,
            error: Some(
                "Installed Mnemos failed package verification; inspect the local package".into(),
            ),
        },
    }))
}
pub(crate) fn comparison(
    installed: Option<&InstalledVersion>,
    candidate: Option<&Candidate>,
) -> &'static str {
    match (installed, candidate) {
        (None, _) => "absent",
        (Some(installed), Some(candidate)) if installed.verified => {
            let old = installed.version.as_deref().and_then(|v| version(v).ok());
            match old.map(|v| {
                version(&candidate.version)
                    .expect("validated candidate")
                    .cmp(&v)
            }) {
                Some(std::cmp::Ordering::Greater) => "newer",
                Some(std::cmp::Ordering::Equal) => "same",
                Some(std::cmp::Ordering::Less) => "older",
                None => "unknown",
            }
        }
        _ => "unknown",
    }
}
pub(crate) async fn check(exact: Option<&str>) -> Result<ReleaseStatus> {
    let target = manager::host_target().map_err(|_| {
        fail(
            "unsupported_target",
            "Mnemos packages are unavailable for this Core platform",
        )
    })?;
    if let Some(value) = exact {
        version(value)?;
    }
    let installed = tokio::task::spawn_blocking(installed_version)
        .await
        .map_err(|_| {
            fail(
                "integrity_error",
                "Mnemos installed-package inspection did not settle",
            )
        })??;
    let http = Http::new(CHECK_SECONDS)?;
    let (highest, candidate, complete) = tokio::time::timeout(
        Duration::from_secs(CHECK_SECONDS),
        discover(&http, target, exact),
    )
    .await
    .map_err(|_| fail("network_error", "Mnemos release check timed out"))??;
    let reason = if !complete {
        Some("Release discovery exceeded its bounded page limit; request an exact version".into())
    } else if candidate.is_none() {
        Some("No installable public Mnemos package was found; metadata-only releases cannot be installed".into())
    } else {
        None
    };
    Ok(ReleaseStatus {
        repository: REPOSITORY.into(),
        target: target.into(),
        comparison: comparison(installed.as_ref(), candidate.as_ref()).into(),
        installed,
        highest_seen_version: highest,
        candidate,
        unavailable_reason: reason,
        discovery: if complete { "complete" } else { "incomplete" }.into(),
    })
}

struct DownloadDirectory(PathBuf);
impl DownloadDirectory {
    fn create() -> Result<Self> {
        let parent = std::env::temp_dir().canonicalize().map_err(|_| {
            fail(
                "integrity_error",
                "Temporary download directory is unavailable",
            )
        })?;
        let mut random = [0; 16];
        getrandom::getrandom(&mut random).map_err(|_| {
            fail(
                "integrity_error",
                "Could not allocate a private download identity",
            )
        })?;
        let path = parent.join(format!(
            "aizen-mnemos-download-{:032x}",
            u128::from_le_bytes(random)
        ));
        package_install::create_private_dir(&path).map_err(|_| {
            fail(
                "integrity_error",
                "Could not exclusively create a private Mnemos download directory",
            )
        })?;
        Ok(Self(path))
    }
    fn archive(&self) -> PathBuf {
        self.0.join("package.zip")
    }
    fn file(&self) -> Result<File> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        options.open(self.archive()).map_err(|_| {
            fail(
                "integrity_error",
                "Could not exclusively create the private Mnemos archive",
            )
        })
    }
}
impl Drop for DownloadDirectory {
    fn drop(&mut self) {
        // Only one exclusively owned file; never recursively remove unknown files or a package.
        let _ = std::fs::remove_file(self.archive());
        let _ = std::fs::remove_dir(&self.0);
    }
}
async fn reviewed_asset<S: Source>(source: &S, expected: &Candidate) -> Result<Asset> {
    let release = source.by_id(expected.release_id).await?;
    let actual = candidate(source, &release, &expected.target)
        .await
        .map_err(|_| {
            fail(
                "release_changed",
                "The reviewed Mnemos release changed; check and review it again",
            )
        })?;
    if &actual != expected {
        return Err(fail(
            "release_changed",
            "The reviewed Mnemos release changed; check and review it again",
        ));
    }
    Ok(release
        .inventory()?
        .remove(&expected.filename)
        .expect("validated archive"))
}
pub(crate) async fn install(
    candidate: &Candidate,
    expected_generation: u64,
) -> Result<package_install::InstalledPackage> {
    install_checked(candidate, expected_generation, || Ok(())).await
}

/// Recheck the caller's workspace binding after potentially long networking, before mutation.
pub(crate) async fn install_checked<F>(
    candidate: &Candidate,
    expected_generation: u64,
    preflight: F,
) -> Result<package_install::InstalledPackage>
where
    F: FnOnce() -> Result<()>,
{
    candidate.validate()?;
    let target = manager::host_target().map_err(|_| {
        fail(
            "unsupported_target",
            "Mnemos packages are unavailable for this Core platform",
        )
    })?;
    require(
        candidate.target == target,
        "The reviewed Mnemos package targets another platform",
    )?;
    let state = manager::read_state()?;
    if state.generation != expected_generation {
        return Err(fail(
            "stale",
            "Memory configuration changed; review installation again",
        ));
    }
    if state.packages.contains_key("mnemos") {
        return Err(fail(
            "already_installed",
            "Mnemos is already installed; automatic replacement is not supported",
        ));
    }
    if manager::transaction_marker().try_exists()? {
        return Err(fail(
            "recovery_required",
            "Memory provider recovery is required before installing",
        ));
    }
    let http = Http::new(INSTALL_SECONDS)?;
    let directory = download_reviewed(&http, candidate).await?;
    preflight()?;
    // The manager admits only fresh installation, rechecks generation and pins the bytes again.
    manager::install_release(&directory.archive(), candidate, expected_generation)
}

async fn download_reviewed(http: &Http, candidate: &Candidate) -> Result<DownloadDirectory> {
    let directory = DownloadDirectory::create()?;
    tokio::time::timeout(Duration::from_secs(INSTALL_SECONDS), async {
        let asset = reviewed_asset(http, candidate).await?;
        let mut file = directory.file()?;
        http.download(&asset, &candidate.sha256, &mut file).await?;
        drop(file);
        reviewed_asset(http, candidate).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| {
        fail(
            "network_error",
            "Mnemos download timed out; no provider was executed",
        )
    })??;
    Ok(directory)
}

/// Network/prompts are outside admission; the caller rechecks workspace authority after download.
/// The transaction itself rehashes the owned archive and rechecks full installed pins and state.
pub(crate) async fn update_checked<F>(
    candidate: &Candidate,
    expected_generation: u64,
    expected_pins: &package_install::InstallPins,
    preflight: F,
) -> Result<package_install::InstalledPackage>
where
    F: FnOnce() -> Result<()>,
{
    candidate.validate()?;
    let state = manager::read_state()?;
    if state.generation != expected_generation
        || !state
            .packages
            .get("mnemos")
            .is_some_and(|p| &p.pins == expected_pins)
    {
        return Err(fail(
            "stale",
            "Memory configuration changed; review the Mnemos update again",
        ));
    }
    require(
        candidate.target == manager::host_target()? && expected_pins.target == candidate.target,
        "Mnemos update targets another platform",
    )?;
    if manager::transaction_marker().try_exists()? || super::update::recovery_required()? {
        return Err(fail(
            "recovery_required",
            "Memory recovery is required before updating",
        ));
    }
    let installed = manager::info("mnemos")?;
    require(
        version(&candidate.version)? > version(&installed.manifest.version)?,
        "Mnemos update must be strictly newer",
    )?;
    let http = Http::new(INSTALL_SECONDS)?;
    let directory = download_reviewed(&http, candidate).await?;
    preflight()?;
    super::update::update_release(
        &directory.archive(),
        candidate,
        expected_generation,
        expected_pins,
    )
}

#[cfg(test)]
#[path = "releases_tests.rs"]
mod tests;
