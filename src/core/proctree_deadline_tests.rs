// Included in proctree::tests; the child is a disposable sandboxed test executable.
#[test]
fn expired_absolute_run_deadline_does_not_restart_after_spawn() -> anyhow::Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || -> anyhow::Result<()> {
        let mut prepared = bounded_input_request(fixture.path(), &home, "delay")?;
        configure_bounded_stdio(&mut prepared.command, false);
        let (child, containment) = prepared.spawn_contained()?;
        let now = std::time::Instant::now();
        // Model time consumed by protected spawn/worker startup without a sleep-based race.
        let deadlines = BoundedDeadlines {
            run_until: now.checked_sub(Duration::from_secs(1)).unwrap(),
            settle_until: now + Duration::from_secs(3),
        };
        let output = output_bounded_spawned_timed(
            child,
            containment,
            None,
            BoundedTiming::Absolute(deadlines),
            Duration::from_secs(2),
            Some(Box::new(prepared.into_guard())),
            true,
            Some(4096),
        )?;
        assert!(output.timed_out);
        assert_eq!(output.code, None);
        assert_eq!(output.settlement, BoundedSettlement::Proven);
        assert!(
            now.elapsed() < Duration::from_secs(4),
            "a fresh relative wait was granted"
        );
        assert!(!fixture.path().join("receipt.bin").exists());
        check_bounded_effects()?;
        Ok(())
    })
}

#[cfg(windows)]
#[test]
fn expired_absolute_deadline_refuses_suspended_child_before_execution() -> anyhow::Result<()> {
    let _mode = crate::sandbox::runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || -> anyhow::Result<()> {
        let mut prepared = bounded_input_request(fixture.path(), &home, "ignore")?;
        configure_bounded_stdio(&mut prepared.command, false);
        let now = std::time::Instant::now();
        let result = spawn_contained_std_until(
            &mut prepared.command,
            0,
            None,
            Some(BoundedDeadlines {
                run_until: now,
                settle_until: now + Duration::from_secs(3),
            }),
        );
        let error = match result {
            Err(error) => error,
            Ok((mut child, containment)) => {
                kill_tree(&mut child, &containment);
                anyhow::bail!("expired deadline resumed child");
            }
        };
        assert!(error.cleanup_verified(), "{error}");
        assert!(
            error
                .to_string()
                .contains("deadline exhausted before protected child resume"),
            "{error}"
        );
        assert!(!fixture.path().join("receipt.bin").exists());
        Ok(())
    })
}
