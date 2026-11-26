// Retained resource reads reuse identity evidence, never cached bytes or a safety verdict.
#[cfg(windows)]
#[test]
fn existing_evidence_reads_accept_ordinary_long_windows_paths() -> Result<()> {
    let fixture = TestDir::new();
    let mut parent = fixture.path().to_path_buf();
    // Isolate a long file path with a short parent: ancestry admission is independently bounded.
    while parent.as_os_str().len() < 220 {
        parent.push("evidence-parent");
    }
    assert!(parent.as_os_str().len() < 260);
    fs::create_dir_all(&parent)?;
    let path = parent.join(format!("{}.bin", "evidence-".repeat(12)));
    fs::write(&path, b"data")?;
    let spelling = path.to_string_lossy();
    let ordinary = PathBuf::from(
        spelling
            .strip_prefix(r"\\?\")
            .expect("verbatim fixture path"),
    );
    assert!(ordinary.as_os_str().len() > 260);
    assert_eq!(read_existing_regular(&ordinary, 4)?, b"data");
    assert!(read_existing_regular(&ordinary, 3).is_err());
    let alias = parent.join("hardlink.bin");
    fs::hard_link(&path, &alias)?;
    assert!(read_existing_regular(&ordinary, 4).is_err());
    fs::remove_file(&alias)?;
    assert_eq!(read_existing_regular(&ordinary, 4)?, b"data");
    Ok(())
}

#[test]
fn bound_evidence_reads_check_current_bytes_bounds_and_leaf_safety() -> Result<()> {
    let fixture = TestDir::new();
    let path = fixture.path().join("evidence.bin");
    fs::write(&path, b"data")?;
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
    let modified = fs::metadata(&path)?.modified()?;
    assert_eq!(read_bound_regular(&resource, 4)?, b"data");
    fs::write(&path, b"edit")?;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(modified))?;
    assert_eq!(read_bound_regular(&resource, 4)?, b"edit");
    assert!(read_bound_regular(&resource, 3).is_err());
    assert!(read_bound_regular(&resource, u64::MAX).is_err());
    let alias = fixture.path().join("alias.bin");
    fs::hard_link(&path, &alias)?;
    assert!(read_bound_regular(&resource, 4).is_err());
    fs::remove_file(&alias)?;
    fs::remove_file(&path)?;
    assert!(read_bound_regular(&resource, 4).is_err());
    assert!(!path.exists());
    fs::create_dir(&path)?;
    assert!(read_bound_regular(&resource, 4).is_err());
    fs::remove_dir(&path)?;
    fs::write(&path, b"")?;
    assert!(read_bound_regular(&resource, 0)?.is_empty());
    Ok(())
}

#[test]
fn bound_evidence_reads_refuse_replaced_parent_instead_of_rebinding() -> Result<()> {
    let fixture = TestDir::new();
    let parent = fixture.path().join("parent");
    fs::create_dir(&parent)?;
    let path = parent.join("evidence.bin");
    fs::write(&path, b"data")?;
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
    let saved = fixture.path().join("saved-parent");
    fs::rename(&parent, &saved)?;
    fs::create_dir(&parent)?;
    fs::write(&path, b"data")?;
    assert_eq!(read_existing_regular(&path, 4)?, b"data");
    assert!(read_bound_regular(&resource, 4).is_err());
    fs::remove_file(&path)?;
    fs::remove_dir(&parent)?;
    fs::rename(&saved, &parent)?;
    assert_eq!(read_bound_regular(&resource, 4)?, b"data");
    Ok(())
}

#[test]
fn bound_evidence_reads_measure_rebinding_cost_without_skipping_fresh_checks() -> Result<()> {
    let fixture = TestDir::new();
    let path = fixture.path().join("evidence.bin");
    fs::write(&path, b"data")?;
    let resource = crate::core::workspace_txn::NativeResource::resolve(&path)?;
    // Alternating samples limit ordering bias. Timing is diagnostic, never a flaky pass threshold.
    let mut rebound = std::time::Duration::ZERO;
    let mut retained = std::time::Duration::ZERO;
    for _ in 0..32 {
        let start = std::time::Instant::now();
        assert_eq!(read_existing_regular(&path, 4)?, b"data");
        rebound += start.elapsed();
        let start = std::time::Instant::now();
        assert_eq!(read_bound_regular(&resource, 4)?, b"data");
        retained += start.elapsed();
    }
    eprintln!(
        "checked descriptor reads (32 each): rebound {} us; retained {} us",
        rebound.as_micros(),
        retained.as_micros()
    );
    Ok(())
}
