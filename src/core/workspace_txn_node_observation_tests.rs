//! Fused native node observations: one no-follow attribute observation supplies type, reparse,
//! identity, link count and creation time. Counts are attributed primitives, not OS syscalls.
use super::*;
use crate::core::{file_placement::tests::TestDir, io_attribution};
use std::fs;

fn categories(report: io_attribution::Report) -> Result<serde_json::Value> {
    anyhow::ensure!(report.all_spans_closed, "attribution span left open");
    Ok(serde_json::to_value(report)?["categories"].clone())
}

fn calls(categories: &serde_json::Value, name: &str) -> u64 {
    categories[name]["calls"].as_u64().unwrap_or(u64::MAX)
}

fn fixture() -> Result<(TestDir, NativeResource)> {
    let dir = TestDir::new();
    fs::create_dir_all(dir.path().join("a/b"))?;
    let path = dir.path().join("a/b/leaf");
    fs::write(&path, b"leaf")?;
    let resource = NativeResource::resolve(&path)?;
    Ok((dir, resource))
}

fn refusal(resource: &NativeResource) -> String {
    match resource.verify() {
        Ok(()) => String::new(),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn native_node_observation_uses_one_attribute_observation_per_component() -> Result<()> {
    let (dir, resource) = fixture()?;
    let depth = u64::try_from(resource.ancestors.len())?;
    let session = io_attribution::Session::begin()?;
    resource.verify()?;
    let present = categories(session.finish())?;
    assert_eq!(calls(&present, "ancestor_verify"), 2 * depth, "{present}");
    assert_eq!(calls(&present, "leaf_verify"), 1, "{present}");
    assert_eq!(calls(&present, "path_canonicalize"), 1, "{present}");

    let absent = resource.sibling(std::ffi::OsStr::new("absent"))?;
    let session = io_attribution::Session::begin()?;
    absent.verify()?;
    let missing = categories(session.finish())?;
    assert_eq!(calls(&missing, "ancestor_verify"), 2 * depth, "{missing}");
    assert_eq!(calls(&missing, "path_canonicalize"), 0, "{missing}");

    let session = io_attribution::Session::begin()?;
    let resolved = NativeResource::resolve(&dir.path().join("a/b/leaf"))?;
    let resolve = categories(session.finish())?;
    assert_eq!(resolved.ancestors, resource.ancestors);
    #[cfg(windows)]
    {
        for report in [&present, &missing, &resolve] {
            assert_eq!(calls(report, "path_metadata"), 0, "{report}");
        }
        assert_eq!(calls(&present, "path_identity_open"), 2 * depth + 1);
        assert_eq!(calls(&present, "descriptor_identity"), 2 * depth + 1);
        // The absent leaf's attempted open is counted; it has no descriptor to inspect.
        assert_eq!(calls(&missing, "path_identity_open"), 2 * depth + 1);
        assert_eq!(calls(&missing, "descriptor_identity"), 2 * depth);
    }
    #[cfg(unix)]
    {
        // One lstat already supplies every Unix field; no extra identity open exists to remove.
        assert_eq!(calls(&present, "path_metadata"), 2 * depth + 1);
        assert_eq!(calls(&present, "path_identity_open"), 0);
        assert_eq!(calls(&resolve, "path_identity_open"), 0);
    }
    Ok(())
}

#[test]
fn native_node_observation_refuses_ancestor_and_leaf_drift() -> Result<()> {
    let (dir, resource) = fixture()?;
    let parent = dir.path().join("a/b");
    let moved = dir.path().join("a/b-original");
    let leaf = parent.join("leaf");

    // A different directory at the same spelling refuses; restoring the original rebinds nothing.
    fs::rename(&parent, &moved)?;
    fs::create_dir(&parent)?;
    fs::write(parent.join("leaf"), b"leaf")?;
    let error = refusal(&resource);
    assert!(error.contains("ancestor changed"), "{error}");
    fs::remove_file(parent.join("leaf"))?;
    fs::remove_dir(&parent)?;
    fs::rename(&moved, &parent)?;
    resource.verify()?;

    fs::rename(&parent, &moved)?;
    let error = refusal(&resource);
    assert!(error.contains("ancestor drift"), "{error}");
    fs::write(&parent, b"not a directory")?;
    let error = refusal(&resource);
    assert!(error.contains("ancestor changed"), "{error}");
    fs::remove_file(&parent)?;
    fs::rename(&moved, &parent)?;
    resource.verify()?;

    let alias = dir.path().join("a/alias");
    fs::hard_link(&leaf, &alias)?;
    let error = refusal(&resource);
    assert!(error.contains("hardlinked"), "{error}");
    fs::remove_file(&alias)?;
    resource.verify()?;
    fs::remove_file(&leaf)?;
    resource.verify()?; // Absence is an accepted leaf observation for this primitive.
    fs::create_dir(&leaf)?;
    let error = refusal(&resource);
    assert!(error.contains("regular non-link"), "{error}");
    fs::remove_dir(&leaf)?;
    fs::write(&leaf, b"leaf")?;
    resource.verify()?;
    Ok(())
}

#[cfg(windows)]
fn host_spelling(path: &Path) -> Result<String> {
    let text = path.to_str().context("fixture path is not Unicode")?;
    Ok(text.strip_prefix(r"\\?\").unwrap_or(text).to_owned())
}

/// Directory junctions need no symlink privilege, so this exercises a real reparse point here.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(host_spelling(link)?)
        .arg(host_spelling(target)?)
        .stdout(std::process::Stdio::null())
        .status()?;
    anyhow::ensure!(status.success(), "fixture junction creation failed");
    anyhow::ensure!(
        fs::symlink_metadata(link)?.file_attributes() & 0x400 != 0,
        "fixture junction is not a reparse point"
    );
    Ok(())
}

#[cfg(windows)]
fn set_created(path: &Path, time: SystemTime) -> Result<()> {
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
    };
    let file = fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    file.set_times(fs::FileTimes::new().set_created(time))?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn native_node_observation_refuses_junctions_and_creation_time_drift() -> Result<()> {
    let (dir, resource) = fixture()?;
    let parent = dir.path().join("a/b");
    let moved = dir.path().join("a/b-original");
    let leaf = parent.join("leaf");

    // The junction reaches the SAME original directory and bytes, but is itself a reparse point.
    fs::rename(&parent, &moved)?;
    junction(&parent, &moved)?;
    assert_eq!(fs::read(&leaf)?, b"leaf");
    let error = refusal(&resource);
    assert!(error.contains("ancestor changed"), "{error}");
    assert!(NativeResource::resolve(&leaf).is_err());
    fs::remove_dir(&parent)?; // Removes only the junction, never its target.
    fs::rename(&moved, &parent)?;
    resource.verify()?;

    fs::remove_file(&leaf)?;
    junction(&leaf, &dir.path().join("a"))?;
    let error = refusal(&resource);
    assert!(error.contains("regular non-link"), "{error}");
    fs::remove_dir(&leaf)?;
    fs::write(&leaf, b"leaf")?;
    resource.verify()?;

    // Same identity with a changed creation time still refuses; restoring it is accepted again.
    let created = fs::metadata(&parent)?.created()?;
    set_created(&parent, created - Duration::from_secs(3600))?;
    let error = refusal(&resource);
    assert!(error.contains("ancestor changed"), "{error}");
    set_created(&parent, created)?;
    resource.verify()?;
    Ok(())
}
