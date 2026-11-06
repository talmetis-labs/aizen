//! Shared, bounded Mnemos availability tracking. Cache entries are never mutation authority.
use super::{install, manager, releases};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TTL: u64 = 24 * 60 * 60;
const ERROR_BACKOFF: u64 = 15 * 60;
const MAX_CACHE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Cached,
    Automatic,
    Force,
}

#[derive(Debug, Serialize)]
pub(crate) struct Status {
    pub release: releases::ReleaseStatus,
    pub checked_unix: Option<u64>,
    pub attempted_unix: Option<u64>,
    pub cache: &'static str,
    pub stale: bool,
    pub pending: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    schema_version: u32,
    repository: String,
    target: String,
    checked_unix: Option<u64>,
    attempted_unix: u64,
    highest_seen_version: Option<String>,
    candidate: Option<releases::Candidate>,
    discovery: String,
    unavailable_reason: Option<String>,
    error: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn path() -> PathBuf {
    crate::core::config::aizen_home()
        .join("plugin-state")
        .join("mnemos-update-check.json")
}
fn fresh(cache: &Cache, time: u64) -> bool {
    cache.error.is_none()
        && cache.discovery == "complete"
        && cache
            .checked_unix
            .is_some_and(|checked| checked <= time && time - checked < TTL)
}
fn due(cache: Option<&Cache>, time: u64) -> bool {
    cache.is_none_or(|c| !fresh(c, time) && time.saturating_sub(c.attempted_unix) >= ERROR_BACKOFF)
}
fn bounded_text(value: &str) -> bool {
    value.len() <= 1024 && !value.chars().any(char::is_control)
}
fn validate(cache: &Cache, target: &str, time: u64) -> Result<()> {
    ensure!(
        cache.schema_version == 1
            && cache.repository == releases::REPOSITORY
            && cache.target == target,
        "Mnemos update cache identity mismatch"
    );
    ensure!(
        cache.attempted_unix <= time
            && cache.checked_unix.is_none_or(|t| t <= cache.attempted_unix),
        "Mnemos update cache timestamp invalid"
    );
    ensure!(
        matches!(cache.discovery.as_str(), "complete" | "incomplete"),
        "Invalid cached discovery state"
    );
    if let Some(version) = &cache.highest_seen_version {
        releases::version(version)?;
    }
    if let Some(candidate) = &cache.candidate {
        candidate.validate()?;
        ensure!(
            candidate.target == target
                && cache.discovery == "complete"
                && cache.checked_unix.is_some(),
            "Invalid cached Mnemos candidate"
        );
        ensure!(
            cache
                .highest_seen_version
                .as_ref()
                .is_some_and(|highest| releases::version(highest).ok()
                    >= releases::version(&candidate.version).ok()),
            "Cached candidate exceeds highest observed version"
        );
    }
    for text in [&cache.unavailable_reason, &cache.error]
        .into_iter()
        .flatten()
    {
        ensure!(bounded_text(text), "Invalid cached Mnemos status text");
    }
    Ok(())
}
fn read(target: &str, time: u64) -> Option<Cache> {
    let bytes = install::read_host_file(&path(), MAX_CACHE).ok()??;
    super::memory_api::strict_json(&bytes).ok()?;
    let cache = serde_json::from_slice::<Cache>(&bytes).ok()?;
    validate(&cache, target, time).ok()?;
    Some(cache)
}
fn save(cache: &Cache) -> Result<()> {
    let path = path();
    install::ensure_private_root(path.parent().expect("cache parent"))?;
    // Refuse an existing link/foreign object before the atomic owner-only replacement.
    let _ = install::read_host_file(&path, MAX_CACHE)?;
    let bytes = serde_json::to_vec(cache)?;
    ensure!(
        bytes.len() <= MAX_CACHE,
        "Mnemos update cache exceeds its bound"
    );
    crate::core::persist::atomic_write_owner_only(&path, &bytes)
}
fn present(
    cache: Option<&Cache>,
    installed: Option<releases::InstalledVersion>,
    target: &str,
    time: u64,
) -> Status {
    let candidate = cache.and_then(|c| c.candidate.clone());
    let comparison = releases::comparison(installed.as_ref(), candidate.as_ref()).to_owned();
    Status {
        release: releases::ReleaseStatus {
            repository: releases::REPOSITORY.into(),
            target: target.into(),
            installed,
            highest_seen_version: cache.and_then(|c| c.highest_seen_version.clone()),
            candidate,
            comparison,
            unavailable_reason: cache
                .and_then(|c| c.unavailable_reason.clone())
                .or_else(|| {
                    cache
                        .is_none()
                        .then(|| "No fresh Mnemos update check is available".into())
                }),
            discovery: cache.map_or("incomplete", |c| c.discovery.as_str()).into(),
        },
        checked_unix: cache.and_then(|c| c.checked_unix),
        attempted_unix: cache.map(|c| c.attempted_unix),
        cache: if cache.is_some() { "cached" } else { "empty" },
        stale: cache.is_none_or(|c| !fresh(c, time)),
        pending: false,
        error: cache.and_then(|c| c.error.clone()),
    }
}

pub(crate) async fn status(mode: Mode) -> Result<Status> {
    status_with(mode, || releases::check(None)).await
}

// Keep the transport seam private: production always uses the fixed public channel. Tests inject
// discovery only, never a repository override or an alternate downloader.
async fn status_with<F, Fut>(mode: Mode, check: F) -> Result<Status>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<releases::ReleaseStatus>>,
{
    let target = manager::host_target().map_err(|_| {
        releases::fail(
            "unsupported_target",
            "Mnemos packages are unavailable for this Core platform",
        )
    })?;
    let time = now();
    // No check, locks, cache or directory creation without an installed package.
    if !manager::read_state()?.packages.contains_key("mnemos") {
        return Ok(present(None, None, target, time));
    }
    let installed = tokio::task::spawn_blocking(releases::installed_version).await??;
    let cached = read(target, time);
    if mode == Mode::Cached
        || (mode == Mode::Automatic
            && (!crate::core::cli_config::update_check_enabled(&crate::core::cli_config::load())
                || !due(cached.as_ref(), time)))
    {
        return Ok(present(cached.as_ref(), installed, target, time));
    }
    let lock_path = crate::core::workspace_txn::store_lock("mnemos-updates", "check");
    install::ensure_private_root(lock_path.parent().expect("check lock parent"))?;
    crate::core::workspace_txn::NativeResource::resolve(&lock_path)?.verify()?;
    let Ok(_lock) =
        crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO)
    else {
        let mut status = present(cached.as_ref(), installed, target, time);
        status.pending = true;
        return Ok(status);
    };
    let cached = read(target, time);
    if mode == Mode::Automatic && !due(cached.as_ref(), time) {
        return Ok(present(cached.as_ref(), installed, target, time));
    }
    // Await within the caller's bounded lifetime: a one-shot memory client must not detach this.
    let result = check().await;
    let completed = now();
    let next = match result {
        Ok(release) => Cache {
            schema_version: 1, repository: releases::REPOSITORY.into(), target: target.into(),
            checked_unix: Some(completed), attempted_unix: completed,
            highest_seen_version: release.highest_seen_version, candidate: release.candidate,
            error: (release.discovery != "complete").then(|| "Mnemos release discovery is incomplete; no authoritative latest version is available".into()),
            discovery: release.discovery, unavailable_reason: release.unavailable_reason,
        },
        Err(error) => {
            let mut failed = cached.unwrap_or(Cache {
                schema_version: 1, repository: releases::REPOSITORY.into(), target: target.into(),
                checked_unix: None, attempted_unix: completed, highest_seen_version: None, candidate: None,
                discovery: "incomplete".into(), unavailable_reason: None, error: None,
            });
            failed.attempted_unix = completed;
            failed.error = Some(format!("Mnemos update check failed ({}); no package was changed", releases::error_code(&error)));
            failed
        }
    };
    validate(&next, target, completed)?;
    let persist_error = save(&next).err();
    let mut status = present(Some(&next), installed, target, completed);
    status.cache = "network";
    if persist_error.is_some() {
        status.error =
            Some("Mnemos update status could not be cached; no package was changed".into());
        status.stale = true;
    }
    Ok(status)
}

/// Startup notices verify only the pinned manifest, never synchronously hash the full executable.
pub(crate) fn cached_notice() -> Option<String> {
    let state = manager::read_state().ok()?;
    let pins = &state.packages.get("mnemos")?.pins;
    let target = manager::host_target().ok()?;
    let cache = read(target, now())?;
    if !fresh(&cache, now()) {
        return None;
    }
    let candidate = cache.candidate?;
    let bytes = install::read_host_file(
        &manager::packages_root().join("mnemos").join("plugin.json"),
        64 * 1024,
    )
    .ok()??;
    if super::manifest::sha256(&bytes) != pins.manifest.sha256
        || bytes.len() as u64 != pins.manifest.bytes
    {
        return None;
    }
    let manifest: serde_json::Value = super::memory_api::strict_json(&bytes).ok()?;
    if manifest.get("id")?.as_str()? != "mnemos" || manifest.get("target")?.as_str()? != target {
        return None;
    }
    let old = releases::version(manifest.get("version")?.as_str()?).ok()?;
    (releases::version(&candidate.version).ok()? > old).then(|| {
        format!(
            "⬆ Mnemos {} is available — run /update mnemos",
            candidate.version
        )
    })
}

pub(crate) fn spawn_background_check() {
    if !crate::core::cli_config::update_check_enabled(&crate::core::cli_config::load()) {
        return;
    }
    if !manager::read_state().is_ok_and(|s| s.packages.contains_key("mnemos")) {
        return;
    }
    tokio::spawn(async {
        let _ = status(Mode::Automatic).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        config::{EnvGuard, TEST_HOME_LOCK},
        file_placement::tests::TestDir,
    };

    fn install_fixture(root: &std::path::Path) {
        manager::tests::native_review_fixture(root);
        let manifest_path = root.join("source/plugin.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["id"] = serde_json::json!("mnemos");
        crate::core::persist::atomic_write_owner_only(
            &manifest_path,
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        manager::install_local(&root.join("source")).unwrap();
    }
    fn discovered() -> releases::ReleaseStatus {
        releases::ReleaseStatus {
            repository: releases::REPOSITORY.into(),
            target: manager::host_target().unwrap().into(),
            installed: None,
            highest_seen_version: Some("1.0.0".into()),
            candidate: None,
            comparison: "unknown".into(),
            unavailable_reason: Some("Metadata only".into()),
            discovery: "complete".into(),
        }
    }
    #[test]
    fn absent_cached_and_opted_out_checks_never_request_or_create_cache() {
        if manager::host_target().is_err() {
            return;
        }
        let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = TestDir::new();
            let home = root.path().join("home");
            let _env = EnvGuard::set([
                ("AIZEN_HOME", home.as_path()),
                ("HOME", root.path()),
                ("USERPROFILE", root.path()),
                ("AIZEN_NO_UPDATE_CHECK", std::path::Path::new("1")),
            ]);
            for mode in [Mode::Cached, Mode::Automatic, Mode::Force] {
                let result = status_with(mode, || async {
                    panic!("absent Mnemos must not request discovery")
                })
                .await
                .unwrap();
                assert!(result.release.installed.is_none());
            }
            assert!(!home.exists());
            install_fixture(root.path());
            for mode in [Mode::Cached, Mode::Automatic] {
                let result = status_with(mode, || async {
                    panic!("offline/opt-out must not request discovery")
                })
                .await
                .unwrap();
                assert!(result.release.installed.unwrap().verified);
            }
            assert!(!path().exists());
            let forced = status_with(Mode::Force, || async { Ok(discovered()) })
                .await
                .unwrap();
            assert_eq!(forced.cache, "network");
            assert!(!forced.stale);
            assert!(path().exists());
            assert!(!manager::transaction_marker().exists());
            assert!(!manager::default_data_root("mnemos").unwrap().exists());
        });
    }
    #[test]
    fn ttl_failure_backoff_and_inflight_lock_share_discovery_without_provider_work() {
        if manager::host_target().is_err() {
            return;
        }
        let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = TestDir::new();
            let home = root.path().join("home");
            let _env = EnvGuard::set([
                ("AIZEN_HOME", home.as_path()),
                ("HOME", root.path()),
                ("USERPROFILE", root.path()),
                ("AIZEN_NO_UPDATE_CHECK", std::path::Path::new("0")),
            ]);
            install_fixture(root.path());
            let first = status_with(Mode::Automatic, || async { Ok(discovered()) })
                .await
                .unwrap();
            assert_eq!(first.cache, "network");
            assert!(!first.stale);
            let cached = status_with(Mode::Automatic, || async {
                panic!("fresh negative result has a one-day TTL")
            })
            .await
            .unwrap();
            assert_eq!(cached.cache, "cached");
            assert!(!cached.stale);
            let failed = status_with(Mode::Force, || async {
                Err(releases::fail("network_error", "synthetic failure"))
            })
            .await
            .unwrap();
            assert!(failed.stale && failed.error.is_some());
            assert_eq!(failed.checked_unix, first.checked_unix);
            let backed_off = status_with(Mode::Automatic, || async {
                panic!("failed automatic checks must back off")
            })
            .await
            .unwrap();
            assert!(backed_off.stale);
            let lock_path = crate::core::workspace_txn::store_lock("mnemos-updates", "check");
            let _check_lock =
                crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO)
                    .unwrap();
            let pending = status_with(Mode::Force, || async {
                panic!("another owner already checks discovery")
            })
            .await
            .unwrap();
            assert!(pending.pending && pending.stale);
            assert_eq!(pending.cache, "cached");
            assert!(!manager::transaction_marker().exists());
            assert!(!manager::default_data_root("mnemos").unwrap().exists());
        });
    }
    #[test]
    fn startup_notice_uses_fresh_pinned_manifest_and_never_needs_provider_data() {
        if manager::host_target().is_err() {
            return;
        }
        let _lock = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = TestDir::new();
        let home = root.path().join("home");
        let _env = EnvGuard::set([
            ("AIZEN_HOME", home.as_path()),
            ("HOME", root.path()),
            ("USERPROFILE", root.path()),
        ]);
        install_fixture(root.path());
        let target = manager::host_target().unwrap();
        let mut cache = empty(now());
        cache.target = target.into();
        cache.candidate = Some(releases::Candidate {
            release_id: 1,
            version: "0.2.0".into(),
            target: target.into(),
            asset_id: 2,
            filename: format!("mnemos-0.2.0-{target}.zip"),
            bytes: 100,
            sha256: "a".repeat(64),
            checksums_asset_id: 3,
            checksums_sha256: "b".repeat(64),
        });
        save(&cache).unwrap();
        assert!(cached_notice().unwrap().contains("0.2.0"));
        cache.error = Some("synthetic failure".into());
        save(&cache).unwrap();
        assert!(cached_notice().is_none());
        cache.error = None;
        save(&cache).unwrap();
        let manifest = manager::packages_root().join("mnemos/plugin.json");
        std::fs::write(manifest, b"{}").unwrap();
        assert!(cached_notice().is_none());
        assert!(!manager::default_data_root("mnemos").unwrap().exists());
    }
    fn empty(time: u64) -> Cache {
        Cache {
            schema_version: 1,
            repository: releases::REPOSITORY.into(),
            target: "x86_64-pc-windows-msvc".into(),
            checked_unix: Some(time),
            attempted_unix: time,
            highest_seen_version: Some("1.0.0".into()),
            candidate: None,
            discovery: "complete".into(),
            unavailable_reason: Some("Metadata only".into()),
            error: None,
        }
    }
    #[test]
    fn successful_empty_discovery_is_cached_for_one_day() {
        let c = empty(1000);
        assert!(fresh(&c, 1001));
        assert!(!due(Some(&c), 1001));
        assert!(!fresh(&c, 1000 + TTL));
        assert!(due(Some(&c), 1000 + TTL));
        let status = present(Some(&c), None, &c.target, 1001);
        assert!(status.release.candidate.is_none());
        assert_eq!(status.release.comparison, "absent");
    }
    #[test]
    fn failed_attempt_backoff_never_calls_stale_result_current() {
        let mut c = empty(1000);
        c.error = Some("network_error".into());
        assert!(!fresh(&c, 1001));
        assert!(!due(Some(&c), 1001));
        assert!(due(Some(&c), 1000 + ERROR_BACKOFF));
        assert!(present(Some(&c), None, &c.target, 1001).stale);
    }
    #[test]
    fn future_foreign_incomplete_and_unbounded_cache_are_not_authoritative() {
        let mut c = empty(1000);
        assert!(validate(&c, &c.target, 999).is_err());
        assert!(validate(&c, "aarch64-apple-darwin", 1001).is_err());
        c.discovery = "incomplete".into();
        assert!(!fresh(&c, 1001));
        c.error = Some("x".repeat(1025));
        assert!(validate(&c, &c.target, 1001).is_err());
    }
}
