use super::*;
use std::cell::Cell;

#[test]
fn streaming_counter_refuses_oversize_without_advancing() {
    let mut bytes = 0;
    accumulate(&mut bytes, 4, 8).unwrap();
    assert!(accumulate(&mut bytes, 5, 8).is_err());
    assert_eq!(bytes, 4);
    accumulate(&mut bytes, 4, 8).unwrap();
    assert!(accumulate(&mut bytes, 1, 8).is_err());
}

#[tokio::test]
async fn cancellation_drops_only_the_private_partial_download() {
    let directory = DownloadDirectory::create().unwrap();
    let path = directory.0.clone();
    let mut file = directory.file().unwrap();
    file.write_all(b"partial fixture bytes").unwrap();
    let result = tokio::time::timeout(Duration::from_millis(1), async move {
        let _directory = directory;
        let _file = file;
        std::future::pending::<()>().await;
    })
    .await;
    assert!(result.is_err());
    assert!(!path.exists());
}

fn fixture_release(value: &str, id: u64) -> (Release, Vec<u8>) {
    let sums: String = filenames(value)
        .iter()
        .map(|name| format!("{}  {name}\n", "a".repeat(64)))
        .collect();
    let mut assets: Vec<_> = filenames(value)
        .iter()
        .enumerate()
        .map(|(n, name)| Asset {
            id: id * 10 + n as u64 + 1,
            name: name.clone(),
            size: 256,
            state: "uploaded".into(),
            browser_download_url: asset_url(value, name),
            digest: Some(format!("sha256:{}", "a".repeat(64))),
        })
        .collect();
    assets.push(Asset {
        id: id * 10 + 4,
        name: "SHA256SUMS".into(),
        size: sums.len() as u64,
        state: "uploaded".into(),
        browser_download_url: asset_url(value, "SHA256SUMS"),
        digest: Some(format!("sha256:{}", manifest::sha256(sums.as_bytes()))),
    });
    (
        Release {
            id,
            tag_name: format!("v{value}"),
            draft: false,
            prerelease: false,
            assets,
        },
        sums.into_bytes(),
    )
}
struct Fake {
    pages: Vec<(Vec<Release>, bool)>,
    release: Release,
    sums: Vec<u8>,
    checksum_calls: Cell<usize>,
}
impl Fake {
    fn new(value: &str) -> Self {
        let (release, sums) = fixture_release(value, 4);
        Self {
            pages: vec![(vec![release.clone()], false)],
            release,
            sums,
            checksum_calls: Cell::new(0),
        }
    }
}
impl Source for Fake {
    async fn list(&self, page: usize) -> Result<(Vec<Release>, bool)> {
        Ok(self.pages[page - 1].clone())
    }
    async fn by_tag(&self, _: &str) -> Result<Option<Release>> {
        Ok(Some(self.release.clone()))
    }
    async fn by_id(&self, _: u64) -> Result<Release> {
        Ok(self.release.clone())
    }
    async fn checksum_bytes(&self, _: &Asset) -> Result<Vec<u8>> {
        self.checksum_calls.set(self.checksum_calls.get() + 1);
        Ok(self.sums.clone())
    }
    async fn download(&self, _: &Asset, _: &str, _: &mut File) -> Result<()> {
        panic!("discovery and review never download or execute an archive")
    }
}

#[test]
fn stable_versions_are_canonical_and_numeric() {
    assert!(version("0.1.0").unwrap() < version("0.10.0").unwrap());
    assert!(version("9.0.0").unwrap() < version("10.0.0").unwrap());
    for bad in [
        "v1.0.0",
        "01.0.0",
        "1.0",
        "1.0.0.1",
        "1.0.0-beta",
        "1.0.0+build",
        "1000000000.0.0",
        "1.0.-1",
        "1.0.０",
        "1.0.0\n",
    ] {
        assert!(version(bad).is_err(), "{bad}");
    }
}
#[tokio::test]
async fn metadata_only_highest_release_does_not_mask_a_real_package() {
    let mut source = Fake::new("0.1.0");
    let (mut empty, _) = fixture_release("1.0.0", 5);
    empty.assets.clear();
    source.pages[0].0.insert(0, empty);
    let (highest, selected, complete) = discover(&source, TARGETS[1], None).await.unwrap();
    assert_eq!(highest.as_deref(), Some("1.0.0"));
    assert_eq!(selected.unwrap().version, "0.1.0");
    assert!(complete);
    assert_eq!(source.checksum_calls.get(), 1);
}
#[tokio::test]
async fn assetless_channel_is_honestly_unavailable_and_downloads_nothing() {
    let mut source = Fake::new("1.0.0");
    source.pages[0].0[0].assets.clear();
    let (highest, selected, complete) = discover(&source, TARGETS[1], None).await.unwrap();
    assert_eq!(highest.as_deref(), Some("1.0.0"));
    assert!(selected.is_none() && complete);
    assert_eq!(source.checksum_calls.get(), 0);
}
#[tokio::test]
async fn discovery_numeric_sort_and_prerelease_policy() {
    let mut source = Fake::new("0.10.0");
    let (older, _) = fixture_release("0.9.0", 5);
    let (mut draft, _) = fixture_release("2.0.0", 6);
    draft.draft = true;
    let (mut prerelease, _) = fixture_release("3.0.0", 7);
    prerelease.prerelease = true;
    source.pages[0].0 = vec![older, source.release.clone(), draft, prerelease];
    let (highest, selected, _) = discover(&source, TARGETS[1], None).await.unwrap();
    assert_eq!(highest.as_deref(), Some("0.10.0"));
    assert_eq!(selected.unwrap().version, "0.10.0");
}
#[tokio::test]
async fn incomplete_discovery_never_returns_an_authoritative_latest() {
    let mut source = Fake::new("0.1.0");
    source.pages = (1..=3)
        .map(|id| (vec![fixture_release(&format!("0.{id}.0"), id).0], true))
        .collect();
    let (highest, selected, complete) = discover(&source, TARGETS[1], None).await.unwrap();
    assert_eq!(highest.as_deref(), Some("0.3.0"));
    assert!(selected.is_none() && !complete);
    assert_eq!(source.checksum_calls.get(), 0);
}
#[tokio::test]
async fn exact_version_does_not_follow_a_different_tag() {
    let source = Fake::new("0.1.0");
    assert!(discover(&source, TARGETS[1], Some("0.2.0")).await.is_err());
    assert_eq!(source.checksum_calls.get(), 0);
}
#[test]
fn release_inventory_refuses_duplicate_missing_extra_and_foreign_assets() {
    let (base, _) = fixture_release("0.1.0", 1);
    assert!(base.inventory().is_ok());
    for mutation in 0..9 {
        let mut release = base.clone();
        match mutation {
            0 => {
                release.assets.pop();
            }
            1 => {
                release.assets.push(release.assets[0].clone());
            }
            2 => release.assets[0].name = "source.zip".into(),
            3 => release.assets[0].id = release.assets[1].id,
            4 => release.assets[0].size = MAX_ARCHIVE + 1,
            5 => release.assets[0].state = "new".into(),
            6 => {
                release.assets[0].browser_download_url =
                    "https://github.com/another/repo/releases/download/v0.1.0/x.zip".into()
            }
            7 => release.assets[0].digest = Some("sha256:invalid".into()),
            _ => release.assets[0].id = MAX_ID + 1,
        }
        assert!(release.inventory().is_err(), "mutation {mutation}");
    }
}
#[test]
fn checksum_grammar_requires_exact_unique_three_zip_rows() {
    let (_, valid) = fixture_release("0.1.0", 1);
    assert_eq!(checksums(&valid, "0.1.0").unwrap().len(), 3);
    let text = String::from_utf8(valid).unwrap();
    let first = text.lines().next().unwrap();
    for bad in [
        text.replace("  ", " "),
        text.replace('\n', "\r\n"),
        text.to_uppercase(),
        text.trim_end().to_owned(),
        format!("{text}{first}\n"),
        format!("{text}{}  extra.zip\n", "b".repeat(64)),
        format!("{first}\n"),
        format!("{}\n", "a".repeat(64)),
    ] {
        assert!(checksums(bad.as_bytes(), "0.1.0").is_err());
    }
    assert!(checksums(&vec![b'a'; MAX_CHECKSUMS + 1], "0.1.0").is_err());
}
#[tokio::test]
async fn corrupt_checksum_is_not_silently_replaced_by_an_older_release() {
    let mut source = Fake::new("0.2.0");
    source.pages[0].0.push(fixture_release("0.1.0", 5).0);
    source.sums = b"malformed\n".to_vec();
    assert!(discover(&source, TARGETS[1], None).await.is_err());
    assert_eq!(source.checksum_calls.get(), 1);
}
#[tokio::test]
async fn candidate_schema_rejects_urls_unknown_fields_and_unsafe_numeric_ids() {
    let source = Fake::new("0.1.0");
    let good = candidate(&source, &source.release, TARGETS[1])
        .await
        .unwrap();
    let mut json = serde_json::to_value(&good).unwrap();
    json["url"] = serde_json::json!("https://attacker.example/x");
    assert!(serde_json::from_value::<Candidate>(json).is_err());
    for mutation in 0..6 {
        let mut bad = good.clone();
        match mutation {
            0 => bad.release_id = MAX_ID + 1,
            1 => bad.asset_id = bad.checksums_asset_id,
            2 => bad.filename = "../provider.zip".into(),
            3 => bad.target = "x86_64-apple-darwin".into(),
            4 => bad.sha256 = "A".repeat(64),
            _ => bad.bytes = 0,
        }
        assert!(bad.validate().is_err());
    }
}
#[tokio::test]
async fn reviewed_release_is_bound_to_ids_version_target_size_and_checksum() {
    let mut source = Fake::new("0.1.0");
    let good = candidate(&source, &source.release, TARGETS[1])
        .await
        .unwrap();
    assert!(reviewed_asset(&source, &good).await.is_ok());
    source.release.assets[0].size += 1;
    // Even changes to an unselected platform remain valid metadata; selected pins are fixed.
    let mut changed = good.clone();
    changed.bytes += 1;
    let error = reviewed_asset(&source, &changed).await.unwrap_err();
    assert_eq!(error_code(&error), "release_changed");
    source.sums.push(b'\n');
    assert_eq!(
        error_code(&reviewed_asset(&source, &good).await.unwrap_err()),
        "release_changed"
    );
}
#[test]
fn anonymous_transport_has_no_default_authorization_and_refuses_unsafe_redirects() {
    let client = Http::new(30).unwrap();
    let request = client.client.get("https://api.github.com").build().unwrap();
    assert!(!request
        .headers()
        .contains_key(reqwest::header::AUTHORIZATION));
    for bad in [
        "http://release-assets.githubusercontent.com/x",
        "https://user:pass@release-assets.githubusercontent.com/x",
        "https://release-assets.githubusercontent.com:444/x",
        "https://release-assets.githubusercontent.com/x#secret",
        "https://release-assets.githubusercontent.com.evil.example/x",
        "https://github.com/another/repo/x",
        "https://127.0.0.1/x",
        "https://objects.githubusercontent.com@evil.example/x",
    ] {
        assert!(
            validate_url(&Url::parse(bad).unwrap(), true, true).is_err(),
            "{bad}"
        );
    }
    for good in ["https://release-assets.githubusercontent.com/github-production-release-asset/x?sig=private", "https://objects.githubusercontent.com/x"] {
        assert!(validate_url(&Url::parse(good).unwrap(), true, true).is_ok());
    }
    assert!(validate_url(
        &Url::parse("https://api.github.com/repos/another/repo/releases").unwrap(),
        false,
        false
    )
    .is_err());
    assert!(validate_url(
        &Url::parse("https://api.github.com/repos/talmetis-labs/aizen-mnemos-releases/releases")
            .unwrap(),
        false,
        false
    )
    .is_ok());
}
#[test]
fn errors_do_not_retain_untrusted_transport_urls() {
    let error = fail(
        "network_error",
        "Mnemos release request failed or timed out",
    );
    assert_eq!(error_code(&error), "network_error");
    assert!(!format!("{error:#}").contains("https://"));
    // Caller-side preflight failures retain their release code through anyhow context wrapping.
    let stale = fail("stale", "Workspace binding changed during download")
        .context("post-download workspace preflight");
    assert_eq!(error_code(&stale), "stale");
}
#[test]
fn installed_and_available_are_distinct_comparisons() {
    let source = Fake::new("0.2.0");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let package = runtime
        .block_on(candidate(&source, &source.release, TARGETS[1]))
        .unwrap();
    assert_eq!(comparison(None, Some(&package)), "absent");
    let mut installed = InstalledVersion {
        version: Some("0.1.0".into()),
        verified: true,
        error: None,
    };
    assert_eq!(comparison(Some(&installed), Some(&package)), "newer");
    installed.version = Some("0.2.0".into());
    assert_eq!(comparison(Some(&installed), Some(&package)), "same");
    installed.version = Some("0.3.0".into());
    assert_eq!(comparison(Some(&installed), Some(&package)), "older");
    installed.verified = false;
    assert_eq!(comparison(Some(&installed), Some(&package)), "unknown");
}
#[test]
fn operation_cleanup_removes_only_its_own_archive_and_empty_directory() {
    let directory = DownloadDirectory::create().unwrap();
    let path = directory.0.clone();
    let mut file = directory.file().unwrap();
    file.write_all(b"partial bytes, never executable").unwrap();
    drop(file);
    drop(directory);
    assert!(!path.exists());
    let directory = DownloadDirectory::create().unwrap();
    let path = directory.0.clone();
    std::fs::write(path.join("unrecognized-evidence"), b"preserve").unwrap();
    drop(directory);
    assert_eq!(
        std::fs::read(path.join("unrecognized-evidence")).unwrap(),
        b"preserve"
    );
    std::fs::remove_file(path.join("unrecognized-evidence")).unwrap();
    std::fs::remove_dir(path).unwrap();
}

fn synthetic_zip(path: &std::path::Path, id: &str, value: &str, target: &str) -> Vec<u8> {
    let mut native = vec![0u8; 256];
    let executable = if target.ends_with("windows-msvc") {
        native[..2].copy_from_slice(b"MZ");
        native[60..64].copy_from_slice(&64u32.to_le_bytes());
        native[64..68].copy_from_slice(b"PE\0\0");
        native[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        native[70..72].copy_from_slice(&1u16.to_le_bytes());
        native[84..86].copy_from_slice(&112u16.to_le_bytes());
        native[86..88].copy_from_slice(&2u16.to_le_bytes());
        native[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        native[104..108].copy_from_slice(&1u32.to_le_bytes());
        native[156..158].copy_from_slice(&3u16.to_le_bytes());
        "provider.exe"
    } else if target.contains("linux") {
        native[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        native[16..18].copy_from_slice(&2u16.to_le_bytes());
        native[18..20].copy_from_slice(&62u16.to_le_bytes());
        native[24..32].copy_from_slice(&1u64.to_le_bytes());
        "provider"
    } else {
        native[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
        native[4..8].copy_from_slice(&0x0100000cu32.to_le_bytes());
        native[12..16].copy_from_slice(&2u32.to_le_bytes());
        "provider"
    };
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"id":id,"name":"Synthetic non-executable Memory fixture","version":value,"kind":"memory",
        "protocol":{"major":1,"min_minor":0,"max_minor":0},"target":target,"entrypoint":executable,
        "capabilities":[],"permissions":{"network":false,"host_llm":false},
        "files":[{"path":executable,"bytes":native.len(),"sha256":manifest::sha256(&native)}]
    })).unwrap();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes, mode) in [
        ("plugin.json", manifest.as_slice(), 0o600),
        (executable, native.as_slice(), 0o700),
    ] {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored)
                    .unix_permissions(mode),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    std::fs::write(path, &bytes).unwrap();
    bytes
}
#[test]
fn installer_pins_wrong_sha_version_id_and_target_before_publication() {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let root = fixture.path().join("plugins");
    package_install::ensure_private_root(&root).unwrap();
    let path = fixture.path().join("package.zip");
    let target = TARGETS[1];
    let bytes = synthetic_zip(&path, "mnemos", "0.1.0", target);
    let digest = manifest::sha256(&bytes);
    for (id, value, sha, host) in [
        ("mnemos", "0.1.0", "b".repeat(64), target),
        ("mnemos", "0.2.0", digest.clone(), target),
        ("other-memory", "0.1.0", digest.clone(), target),
        ("mnemos", "0.1.0", digest.clone(), TARGETS[0]),
    ] {
        let expectation = package_install::ReleaseExpectation {
            id,
            version: value,
            archive_sha256: &sha,
        };
        assert!(package_install::install_zip_release(&root, &path, host, &expectation).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
    // Replacing the downloaded path after review cannot bypass the installer's own hash pin.
    synthetic_zip(&path, "mnemos", "0.2.0", target);
    let expectation = package_install::ReleaseExpectation {
        id: "mnemos",
        version: "0.1.0",
        archive_sha256: &digest,
    };
    assert!(package_install::install_zip_release(&root, &path, target, &expectation).is_err());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}
#[test]
fn admitted_release_install_rechecks_generation_and_never_activates() {
    let Ok(target) = manager::host_target() else {
        return;
    };
    let _guard = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    let _env = crate::core::config::EnvGuard::set([
        ("AIZEN_HOME", home.as_path()),
        ("USERPROFILE", fixture.path()),
        ("HOME", fixture.path()),
    ]);
    let path = fixture.path().join("package.zip");
    let bytes = synthetic_zip(&path, "mnemos", "0.1.0", target);
    let candidate = Candidate {
        release_id: 1,
        version: "0.1.0".into(),
        target: target.into(),
        asset_id: 2,
        filename: archive_name("0.1.0", target),
        bytes: bytes.len() as u64,
        sha256: manifest::sha256(&bytes),
        checksums_asset_id: 3,
        checksums_sha256: "a".repeat(64),
    };
    let before = manager::read_state().unwrap();
    assert_eq!(
        error_code(
            &manager::install_release(&path, &candidate, before.generation + 1).unwrap_err()
        ),
        "stale"
    );
    assert!(!manager::packages_root().join("mnemos").exists());
    // Unregistered payloads (including foreign files) are also never overwritten or swept.
    package_install::ensure_private_root(&manager::packages_root()).unwrap();
    std::fs::write(
        manager::packages_root().join("mnemos"),
        b"preserve orphan evidence",
    )
    .unwrap();
    assert_eq!(
        error_code(&manager::install_release(&path, &candidate, before.generation).unwrap_err()),
        "already_installed"
    );
    assert_eq!(
        std::fs::read(manager::packages_root().join("mnemos")).unwrap(),
        b"preserve orphan evidence"
    );
    std::fs::remove_file(manager::packages_root().join("mnemos")).unwrap();
    manager::install_release(&path, &candidate, before.generation).unwrap();
    let after = manager::read_state().unwrap();
    assert!(
        !after.enabled
            && after.selected.is_none()
            && !after.packages["mnemos"].enabled
            && after.packages["mnemos"].grant.is_none()
    );
    assert!(!home.join("memory-data").exists() && !home.join("cli-memory").exists());
    assert_eq!(
        error_code(&manager::install_release(&path, &candidate, after.generation).unwrap_err()),
        "already_installed"
    );
    assert_eq!(manager::read_state().unwrap(), after);
}
