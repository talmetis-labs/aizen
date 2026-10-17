//! Immutable, public GitHub skill bundles. No Git subprocess, external runtime, or script execution.
//! Archive names are validated before the ZIP reader (which otherwise deduplicates names), then
//! only the requested component-matched subtree is written into an operation-owned empty stage.

use super::{storage, Skill, SkillProvenance};
use crate::agent::reach::http;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::time::Duration;

const MAX_ARCHIVE: usize = 64 * 1024 * 1024;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_DEPTH: usize = 32;
const INSTALL_TIMEOUT: u64 = 120;

/// Download and install one directory, resolving the requested/default branch to an immutable SHA.
/// An install never replaces another skill, and copying scripts does not run them.
pub async fn install(
    repository: &str,
    path: &str,
    reference: Option<&str>,
    name: Option<&str>,
) -> Result<Skill> {
    let (owner, repo) = repository_parts(repository)?;
    let selected = selected_path(path)?;
    let fallback = selected.last().map(String::as_str).unwrap_or(&repo);
    let slug = install_slug(name.unwrap_or(fallback))?;
    let reference = reference.map(str::trim);
    if reference.is_some_and(|r| r.is_empty() || r.len() > 512 || r.chars().any(char::is_control)) {
        bail!("GitHub ref must be a non-empty branch, tag, or commit (at most 512 bytes)");
    }
    let source = format!("{owner}/{repo}");
    let (sha, bytes) = tokio::time::timeout(Duration::from_secs(INSTALL_TIMEOUT), async {
        let client = http::client()?;
        let sha = resolve_commit(&client, &owner, &repo, reference).await?;
        let url = archive_url(&owner, &repo, &sha)?;
        crate::core::net_guard::guard_url_async(url.as_str()).await?;
        let fetched = http::get_bounded(&client, url.as_str(), &[], MAX_ARCHIVE).await?;
        ensure_success(fetched.status, &fetched.body)?;
        Ok::<_, anyhow::Error>((sha, fetched.body))
    })
    .await
    .context("GitHub skill download timed out (120s total)")??;
    let provenance = SkillProvenance {
        source: format!("https://github.com/{source}"),
        repository: Some(source),
        path: Some(if selected.is_empty() {
            ".".into()
        } else {
            selected.join("/")
        }),
        sha: Some(sha),
    };
    install_archive(&slug, &selected, &bytes, provenance)
}

fn repository_parts(source: &str) -> Result<(String, String)> {
    let parts: Vec<_> = source.trim().split('/').collect();
    if parts.len() != 2 {
        bail!("GitHub source must be OWNER/REPO, not a URL");
    }
    let valid = |s: &str, max: usize| {
        !s.is_empty()
            && s.len() <= max
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !valid(parts[0], 39) || !valid(parts[1], 100) || parts[1].ends_with(".git") {
        bail!("invalid GitHub OWNER/REPO (use the repository name without .git)");
    }
    Ok((parts[0].into(), parts[1].into()))
}

fn safe_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with(['.', ' '])
        || component.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        bail!("unsafe skill resource path component");
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if device {
        bail!("reserved Windows device name in skill resource path");
    }
    Ok(())
}

fn selected_path(path: &str) -> Result<Vec<String>> {
    let path = path.trim();
    if path == "." {
        return Ok(Vec::new());
    }
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
        bail!("--path must name a relative skill directory (use . for repository root)");
    }
    let parts: Vec<_> = path.split('/').map(str::to_string).collect();
    if parts.len() > MAX_DEPTH {
        bail!("skill directory exceeds depth limit ({MAX_DEPTH})");
    }
    for part in &parts {
        safe_component(part)?;
    }
    Ok(parts)
}

fn install_slug(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 120 {
        bail!("skill install name must be 1..=120 bytes");
    }
    let slug = super::sanitize_name(name);
    safe_component(&slug)?;
    if slug == "p" {
        bail!("skill name 'p' is reserved for project zones");
    }
    Ok(slug)
}

fn api_url(owner: &str, repo: &str, reference: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://api.github.com")?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid GitHub API base"))?;
        segments.extend(["repos", owner, repo]);
        if let Some(reference) = reference {
            segments.extend(["commits", reference]);
        }
    }
    Ok(url)
}

fn archive_url(owner: &str, repo: &str, sha: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://codeload.github.com")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid GitHub archive base"))?
        .extend([owner, repo, "zip", sha]);
    Ok(url)
}

fn ensure_success(status: u16, _body: &[u8]) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        404 => bail!("GitHub repository or ref not found; only public repositories are supported"),
        403 | 429 => bail!(
            "GitHub refused the request (HTTP {status}); anonymous API rate limit may be exhausted"
        ),
        _ => bail!("GitHub returned HTTP {status}"),
    }
}

async fn github_json(client: &reqwest::Client, url: &reqwest::Url) -> Result<serde_json::Value> {
    crate::core::net_guard::guard_url_async(url.as_str()).await?;
    let response = http::get(
        client,
        url.as_str(),
        &[("Accept", "application/vnd.github+json".into())],
    )
    .await?;
    ensure_success(response.status, &response.body)?;
    serde_json::from_slice(&response.body).context("parsing GitHub API response")
}

fn default_branch(metadata: &serde_json::Value) -> Result<&str> {
    metadata
        .get("default_branch")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .context("GitHub did not return a valid default branch")
}

fn commit_sha(metadata: &serde_json::Value) -> Result<String> {
    metadata
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .context("GitHub did not return a full commit SHA")
}

async fn resolve_commit(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    reference: Option<&str>,
) -> Result<String> {
    let branch;
    let reference = if let Some(reference) = reference {
        reference
    } else {
        let metadata = github_json(client, &api_url(owner, repo, None)?).await?;
        branch = default_branch(&metadata)?.to_string();
        &branch
    };
    commit_sha(&github_json(client, &api_url(owner, repo, Some(reference))?).await?)
}

#[derive(Debug)]
struct ArchiveEntry {
    name: String,
    parts: Vec<String>,
    directory: bool,
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16> {
    let data = bytes
        .get(offset..offset + 2)
        .context("truncated ZIP header")?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}
fn le32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes
        .get(offset..offset + 4)
        .context("truncated ZIP header")?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

/// Bound the central directory BEFORE zip::ZipArchive allocates/deduplicates it. GitHub emits
/// ordinary, single-disk ZIPs; ZIP64, self-extracting, encrypted and multi-disk archives are refused.
fn archive_entries(bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
    if bytes.len() > MAX_ARCHIVE || bytes.len() < 22 {
        bail!("skill archive is empty or exceeds 64 MiB");
    }
    let end = (bytes.len().saturating_sub(65557)..=bytes.len() - 22)
        .rev()
        .find(|&i| {
            bytes.get(i..i + 4) == Some(b"PK\x05\x06")
                && le16(bytes, i + 20).is_ok_and(|n| i + 22 + n as usize == bytes.len())
        })
        .context("invalid ZIP end record")?;
    let count = le16(bytes, end + 10)? as usize;
    if le16(bytes, end + 4)? != 0
        || le16(bytes, end + 6)? != 0
        || le16(bytes, end + 8)? as usize != count
        || count == 0
        || count > MAX_ENTRIES
    {
        bail!("skill ZIP must be single-disk with 1..={MAX_ENTRIES} entries");
    }
    let size = le32(bytes, end + 12)? as usize;
    let start = le32(bytes, end + 16)? as usize;
    if start.checked_add(size) != Some(end) || bytes.get(..4) != Some(b"PK\x03\x04") {
        bail!("unsupported ZIP64 or prefixed archive layout");
    }
    let mut entries = Vec::with_capacity(count);
    let mut paths = BTreeMap::<String, bool>::new();
    let mut spellings = BTreeMap::<String, String>::new();
    let mut root: Option<String> = None;
    let mut position = start;
    for _ in 0..count {
        if bytes.get(position..position + 4) != Some(b"PK\x01\x02") {
            bail!("invalid ZIP central entry");
        }
        let flags = le16(bytes, position + 8)?;
        let method = le16(bytes, position + 10)?;
        if flags & (1 | 0x40) != 0 || !matches!(method, 0 | 8) {
            bail!("encrypted or unsupported ZIP compression (Stored/Deflate only)");
        }
        let name_len = le16(bytes, position + 28)? as usize;
        let extra_len = le16(bytes, position + 30)? as usize;
        let comment_len = le16(bytes, position + 32)? as usize;
        if le16(bytes, position + 34)? != 0 {
            bail!("multi-disk ZIP entry refused");
        }
        if [20, 24, 42]
            .into_iter()
            .any(|offset| le32(bytes, position + offset).is_ok_and(|n| n == u32::MAX))
        {
            bail!("ZIP64 entries are not supported");
        }
        let attributes = le32(bytes, position + 38)?;
        let mode = attributes >> 16;
        if !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000) || attributes & 0x400 != 0 {
            bail!("links/reparse or special files are not allowed in skill archives");
        }
        let name_start = position.checked_add(46).context("ZIP offset overflow")?;
        let next = name_start
            .checked_add(name_len)
            .and_then(|n| n.checked_add(extra_len))
            .and_then(|n| n.checked_add(comment_len))
            .context("ZIP offset overflow")?;
        if next > end {
            bail!("truncated ZIP central directory");
        }
        let mut extra = name_start + name_len;
        while extra < name_start + name_len + extra_len {
            let id = le16(bytes, extra)?;
            let length = le16(bytes, extra + 2)? as usize;
            extra = extra
                .checked_add(4 + length)
                .context("ZIP extra-field overflow")?;
            if extra > name_start + name_len + extra_len {
                bail!("truncated ZIP extra field");
            }
            if matches!(id, 0x0001 | 0x9901) {
                bail!("ZIP64/encrypted extra fields are not supported");
            }
        }
        let local = le32(bytes, position + 42)? as usize;
        if local >= start || bytes.get(local..local + 4) != Some(b"PK\x03\x04") {
            bail!("invalid ZIP local entry offset");
        }
        let name = std::str::from_utf8(
            bytes
                .get(name_start..name_start + name_len)
                .context("truncated ZIP filename")?,
        )
        .context("skill ZIP filenames must be UTF-8")?
        .to_string();
        if le16(bytes, local + 6)? != flags || le16(bytes, local + 8)? != method {
            bail!("ZIP local and central flags/compression disagree");
        }
        let local_name_len = le16(bytes, local + 26)? as usize;
        let data_start = local
            .checked_add(30)
            .and_then(|n| n.checked_add(local_name_len))
            .and_then(|n| n.checked_add(le16(bytes, local + 28).ok()? as usize))
            .context("ZIP local-header overflow")?;
        if bytes.get(local + 30..local + 30 + local_name_len) != Some(name.as_bytes())
            || data_start
                .checked_add(le32(bytes, position + 20)? as usize)
                .is_none_or(|n| n > start)
        {
            bail!("ZIP local filename/data range does not match its central entry");
        }
        let directory = name.ends_with('/');
        let path = if directory {
            &name[..name.len() - 1]
        } else {
            &name
        };
        let parts: Vec<_> = path.split('/').map(str::to_string).collect();
        if parts.len() > MAX_DEPTH + 1 {
            bail!("skill archive exceeds depth limit");
        }
        for part in &parts {
            safe_component(part)?;
        }
        if root.as_ref().is_some_and(|r| r != &parts[0]) {
            bail!("GitHub archive must have one repository root");
        }
        root.get_or_insert_with(|| parts[0].clone());
        let key = parts.join("/").to_lowercase();
        if paths.insert(key, directory).is_some() {
            bail!("duplicate or case-colliding ZIP resource path");
        }
        // Implicit directories must agree in spelling too: A/one and a/two would merge on Windows.
        let mut prefix = String::new();
        for part in &parts {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if let Some(previous) = spellings.insert(prefix.to_lowercase(), prefix.clone()) {
                if previous != prefix {
                    bail!("case-colliding ZIP directory path");
                }
            }
        }
        entries.push(ArchiveEntry {
            name,
            parts,
            directory,
        });
        position = next;
    }
    if position != end {
        bail!("unexpected records in ZIP central directory");
    }
    for path in paths.keys() {
        let mut prefix = String::new();
        let parts: Vec<_> = path.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if paths.get(&prefix) == Some(&false) {
                bail!("ZIP file/directory destination collision");
            }
        }
    }
    Ok(entries)
}

fn extract_selected(bytes: &[u8], selected: &[String], staging: &Path) -> Result<()> {
    storage::guard(staging)?;
    if !fs::symlink_metadata(staging)?.is_dir() || fs::read_dir(staging)?.next().is_some() {
        bail!("skill extraction needs an empty, owned staging directory");
    }
    let entries = archive_entries(bytes)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("reading skill ZIP")?;
    if archive.len() != entries.len() {
        bail!("ZIP reader changed the validated entry set");
    }
    let mut total = 0u64;
    let mut found_entry = false;
    let mut destinations = HashSet::new();
    for (i, entry) in entries.iter().enumerate() {
        let relative = &entry.parts[1..];
        if !relative.starts_with(selected) {
            continue;
        }
        let relative = &relative[selected.len()..];
        if relative.is_empty() {
            if !entry.directory {
                bail!("selected GitHub path is a file, not a skill directory");
            }
            continue;
        }
        let mut file = archive.by_index(i).context("opening ZIP resource")?;
        if file.name_raw() != entry.name.as_bytes()
            || file.is_dir() != entry.directory
            || file.is_symlink()
            || file.encrypted()
        {
            bail!("ZIP resource metadata does not match validated central entry");
        }
        let mut target = staging.to_path_buf();
        for component in relative {
            target.push(component);
        }
        storage::guard(&target)?;
        if !destinations.insert(target.clone()) {
            bail!("duplicate skill resource destination");
        }
        if entry.directory {
            storage::ensure_dir(&target)?;
            continue;
        }
        if file.size() > MAX_FILE {
            bail!("skill file exceeds 16 MiB");
        }
        if let Some(parent) = target.parent() {
            storage::ensure_dir(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .context("creating staged skill resource")?;
        let mut size = 0u64;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .context("decompressing ZIP resource (including CRC check)")?;
            if read == 0 {
                break;
            }
            size += read as u64;
            total += read as u64;
            if size > MAX_FILE || total > MAX_TOTAL {
                bail!("skill resources exceed 16 MiB/file or 64 MiB total");
            }
            output.write_all(&buffer[..read])?;
        }
        if size != file.size() {
            bail!("ZIP resource size does not match its declared size");
        }
        output.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            // Preserve executable scripts, never setuid/setgid/sticky bits.
            fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777))?;
        }
        if relative.len() == 1 && relative[0] == "SKILL.md" {
            found_entry = true;
        }
    }
    if !found_entry {
        bail!("selected GitHub directory has no direct SKILL.md");
    }
    storage::validate_tree(staging)?;
    let document =
        fs::read_to_string(staging.join("SKILL.md")).context("SKILL.md must be UTF-8")?;
    let document = super::document::SkillDocument::parse(&document)?;
    if document.body().trim().is_empty() {
        bail!("SKILL.md needs non-empty steps");
    }
    Ok(())
}

fn install_archive(
    slug: &str,
    selected: &[String],
    bytes: &[u8],
    provenance: SkillProvenance,
) -> Result<Skill> {
    // Metadata validation is read-only and happens before the live store is touched.
    archive_entries(bytes)?;
    let staging = super::bundle_staging(slug)?;
    let result = extract_selected(bytes, selected, &staging)
        .and_then(|()| super::publish_bundle(slug, &staging, provenance));
    if let Err(error) = super::discard_staging(&staging) {
        if result.is_ok() {
            return Err(error).context("skill published, but staging cleanup failed");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn fixture(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn temporary<T>(name: &str, run: impl FnOnce(&Path) -> T) -> T {
        let path = std::env::temp_dir().join(format!(
            "aizen-github-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        let path = fs::canonicalize(path).unwrap();
        let value = run(&path);
        fs::remove_dir_all(&path).unwrap();
        value
    }

    #[test]
    fn source_paths_and_refs_are_data() {
        assert_eq!(
            repository_parts("anthropics/skills").unwrap(),
            ("anthropics".into(), "skills".into())
        );
        for bad in ["https://github.com/o/r", "o/r/extra", "o/..", "o/r.git"] {
            assert!(repository_parts(bad).is_err());
        }
        for bad in [
            "",
            "../x",
            "/x",
            "C:/x",
            "a\\b",
            "a//b",
            "a/CON",
            "a/x:stream",
            "a/x.",
        ] {
            assert!(selected_path(bad).is_err(), "{bad}");
        }
        assert!(selected_path(".").unwrap().is_empty());
        assert!(install_slug("p").is_err());
        let url = api_url("o", "r", Some("feature/docs v2")).unwrap();
        assert!(url.as_str().ends_with("/commits/feature%2Fdocs%20v2"));
        assert_eq!(
            default_branch(&serde_json::json!({"default_branch":"trunk"})).unwrap(),
            "trunk"
        );
        assert!(default_branch(&serde_json::json!({})).is_err());
        let sha = "a".repeat(40);
        assert_eq!(commit_sha(&serde_json::json!({"sha":sha})).unwrap(), sha);
        assert!(commit_sha(&serde_json::json!({"sha":"main"})).is_err());
    }

    #[test]
    fn extracts_only_exact_subtree_with_raw_resources() {
        temporary("selected", |stage| {
            let doc = b"---\nname: upstream\ndescription: >\n  folded\n  description\nlicense: Apache-2.0\nmetadata:\n  nested: retained\n---\nRead references/guide.md.\n";
            let zip = fixture(&[
                ("repo-sha/skills/demo/SKILL.md", doc),
                ("repo-sha/skills/demo/.hidden", b"yes"),
                ("repo-sha/skills/demo/assets/icon.bin", &[0, 255, 128]),
                ("repo-sha/skills/demo/references/guide.md", b"guide"),
                ("repo-sha/skills/demo-extra/SKILL.md", b"not selected"),
                ("repo-sha/other.txt", b"unrelated"),
            ]);
            extract_selected(&zip, &selected_path("skills/demo").unwrap(), stage).unwrap();
            assert_eq!(fs::read(stage.join("SKILL.md")).unwrap(), doc);
            assert_eq!(
                fs::read(stage.join("assets/icon.bin")).unwrap(),
                [0, 255, 128]
            );
            assert_eq!(fs::read(stage.join(".hidden")).unwrap(), b"yes");
            assert!(!stage.join("demo-extra").exists());
            assert!(!stage.join("other.txt").exists());
        });
    }

    #[test]
    fn rejects_path_collisions_and_missing_entry() {
        for files in [
            vec![("repo/x/../outside", b"x".as_slice())],
            vec![
                ("repo/x/A.md", b"x".as_slice()),
                ("repo/x/a.md", b"y".as_slice()),
            ],
            vec![
                ("repo/x/file", b"x".as_slice()),
                ("repo/x/file/child", b"y".as_slice()),
            ],
            vec![("repo/x/NUL.txt", b"x".as_slice())],
            vec![("repo/x/a:stream", b"x".as_slice())],
        ] {
            assert!(archive_entries(&fixture(&files)).is_err());
        }
        temporary("missing", |stage| {
            assert!(extract_selected(
                &fixture(&[("repo/x/readme.md", b"no entry")]),
                &["x".into()],
                stage
            )
            .is_err());
        });
    }

    #[test]
    fn rejects_metadata_limits_duplicates_and_implicit_case_collisions() {
        for files in [
            vec![
                ("repo/x/A/one", b"x".as_slice()),
                ("repo/x/a/two", b"y".as_slice()),
            ],
            vec![("repo/x/NUL .txt", b"x".as_slice())],
            vec![("repo/x/CONIN$.txt", b"x".as_slice())],
        ] {
            assert!(archive_entries(&fixture(&files)).is_err());
        }
        let deep = format!("repo/{}/SKILL.md", vec!["d"; MAX_DEPTH + 1].join("/"));
        assert!(archive_entries(&fixture(&[(&deep, b"steps")])).is_err());
        let original = fixture(&[("repo/x/a", b"one"), ("repo/x/b", b"two")]);
        let central: Vec<_> = original
            .windows(4)
            .enumerate()
            .filter_map(|(i, w)| (w == b"PK\x01\x02").then_some(i))
            .collect();
        let mut duplicate = original.clone();
        duplicate[central[1] + 46 + "repo/x/".len()] = b'a';
        assert!(archive_entries(&duplicate).is_err());
        let end = original.len() - 22;
        let mut too_many = original.clone();
        for offset in [8, 10] {
            too_many[end + offset..end + offset + 2]
                .copy_from_slice(&((MAX_ENTRIES + 1) as u16).to_le_bytes());
        }
        assert!(archive_entries(&too_many).is_err());
        for offset in [20, 24, 42] {
            let mut zip64 = original.clone();
            zip64[central[0] + offset..central[0] + offset + 4]
                .copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(archive_entries(&zip64).is_err());
        }
        let mut encrypted = original.clone();
        encrypted[central[0] + 8] |= 1;
        assert!(archive_entries(&encrypted).is_err());
        let mut multi_disk = original.clone();
        multi_disk[end + 4] = 1;
        assert!(archive_entries(&multi_disk).is_err());
        let mut local_mismatch = original;
        local_mismatch[30] = b'X';
        assert!(archive_entries(&local_mismatch).is_err());
    }

    #[test]
    fn actual_streamed_bytes_obey_file_total_and_crc_limits() {
        let oversized = vec![b'a'; MAX_FILE as usize + 1];
        let mut lied = fixture(&[("repo/x/SKILL.md", &oversized)]);
        let pos = lied.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        lied[pos + 24..pos + 28].copy_from_slice(&1u32.to_le_bytes());
        temporary("actual-file", |stage| {
            assert!(extract_selected(&lied, &["x".into()], stage).is_err());
        });
        let resource = vec![b'x'; MAX_FILE as usize];
        let total = fixture(&[
            ("repo/x/SKILL.md", b"steps"),
            ("repo/x/a.bin", &resource),
            ("repo/x/b.bin", &resource),
            ("repo/x/c.bin", &resource),
            ("repo/x/d.bin", &resource),
        ]);
        temporary("actual-total", |stage| {
            assert!(extract_selected(&total, &["x".into()], stage).is_err());
        });
        let mut crc = fixture(&[("repo/x/SKILL.md", b"steps")]);
        let pos = crc.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        crc[pos + 16] ^= 0xff;
        temporary("crc", |stage| {
            assert!(extract_selected(&crc, &["x".into()], stage).is_err());
        });
    }

    #[test]
    fn offline_install_publishes_resources_provenance_and_cleans_failures() {
        let _lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct Home(Option<std::ffi::OsString>);
        impl Drop for Home {
            fn drop(&mut self) {
                match &self.0 {
                    Some(home) => std::env::set_var("AIZEN_HOME", home),
                    None => std::env::remove_var("AIZEN_HOME"),
                }
            }
        }
        temporary("publish", |home| {
            let _home = Home(std::env::var_os("AIZEN_HOME"));
            std::env::set_var("AIZEN_HOME", home);
            let slug = format!(
                "github-fixture-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap()
            );
            let provenance = || SkillProvenance {
                source: "https://github.com/owner/repository".into(),
                repository: Some("owner/repository".into()),
                path: Some("skills/demo".into()),
                sha: Some("a".repeat(40)),
            };
            let raw = b"---\r\nname: upstream\r\nmetadata:\r\n  unchanged: yes\r\n---\r\nRead references/guide.md.\r\n";
            let zip = fixture(&[
                ("repo/skills/demo/SKILL.md", raw),
                ("repo/skills/demo/references/guide.md", b"guide"),
                ("repo/skills/demo/assets/data.bin", &[0, 255]),
            ]);
            let selected = selected_path("skills/demo").unwrap();
            let skill = install_archive(&slug, &selected, &zip, provenance()).unwrap();
            assert_eq!(skill.key(), slug);
            assert_eq!(skill.name, "upstream");
            assert_eq!(skill.provenance, Some(provenance()));
            let location = skill.location.unwrap();
            assert!(location.entry.is_absolute() && location.resource_root.is_absolute());
            assert_eq!(fs::read(&location.entry).unwrap(), raw);
            assert_eq!(
                fs::read(location.resource_root.join("assets/data.bin")).unwrap(),
                [0, 255]
            );
            let state = super::super::skills_dir()
                .join(".state")
                .join(format!("{slug}.json"));
            let state_before = fs::read(&state).unwrap();
            assert!(install_archive(&slug, &selected, &zip, provenance()).is_err());
            assert_eq!(fs::read(&state).unwrap(), state_before);
            assert_eq!(fs::read(&location.entry).unwrap(), raw);
            let failed = format!("{slug}-failed");
            let bad = fixture(&[("repo/skills/demo/SKILL.md", b"---\nname: empty\n---\n")]);
            assert!(install_archive(&failed, &selected, &bad, provenance()).is_err());
            let root = super::super::skills_dir();
            assert!(!root.join(&failed).exists());
            assert!(!root.join(".state").join(format!("{failed}.json")).exists());
            assert!(!root.join(".state/.transaction.json").exists());
            assert_eq!(fs::read_dir(root.join(".stage")).unwrap().count(), 0);
        });
    }

    #[test]
    fn rejects_links_limits_and_invalid_document() {
        let mut link = fixture(&[("repo/x/SKILL.md", b"steps")]);
        let pos = link.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        link[pos + 38..pos + 42].copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
        assert!(archive_entries(&link).is_err());
        let mut method = fixture(&[("repo/x/SKILL.md", b"steps")]);
        let pos = method.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        method[pos + 10..pos + 12].copy_from_slice(&99u16.to_le_bytes());
        assert!(archive_entries(&method).is_err());
        temporary("oversize", |stage| {
            let oversized = vec![b'a'; MAX_FILE as usize + 1];
            assert!(extract_selected(
                &fixture(&[("repo/x/SKILL.md", &oversized)]),
                &["x".into()],
                stage
            )
            .is_err());
        });
        for (name, doc) in [
            ("utf8", b"\xff".as_slice()),
            ("empty", b"---\nname: test\n---\n".as_slice()),
            ("yaml", b"---\ndescription: [broken\n---\nsteps".as_slice()),
        ] {
            temporary(name, |stage| {
                assert!(extract_selected(
                    &fixture(&[("repo/x/SKILL.md", doc)]),
                    &["x".into()],
                    stage
                )
                .is_err())
            });
        }
    }
}
