// Included inside proctree::tests. All child processes use the sandbox runner and disposable data.

#[test]
fn bounded_input_fixture() -> anyhow::Result<()> {
    if !std::env::args().any(|arg| arg == "core::proctree::tests::bounded_input_fixture") {
        return Ok(());
    }
    let Some(root) = std::env::var_os("AIZEN_BOUNDED_INPUT_FIXTURE_ROOT") else {
        return Ok(());
    };
    let root = std::fs::canonicalize(root)?;
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    anyhow::ensure!(
        root.starts_with(&temp) && root != temp,
        "test-owned fixture required"
    );
    let bytes = match std::env::var("AIZEN_BOUNDED_INPUT_FIXTURE_MODE")?.as_str() {
        "ignore" => b"ignored input; exited successfully".to_vec(),
        "delay" => {
            std::thread::sleep(Duration::from_secs(30));
            b"must not reach delayed mutation".to_vec()
        }
        "read" => {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::io::stdin()
                .take(16 * 1024 * 1024)
                .read_to_end(&mut bytes)?;
            bytes
        }
        _ => anyhow::bail!("unknown bounded input fixture mode"),
    };
    std::fs::write(root.join("receipt.bin"), bytes)?;
    Ok(())
}

fn bounded_input_request(
    root: &std::path::Path,
    home: &std::path::Path,
    mode: &str,
) -> anyhow::Result<crate::sandbox::runner::Sandboxed<Command>> {
    use crate::sandbox::{request::SandboxRequest, runner, CommandOrigin};
    runner::prepare_std(
        SandboxRequest::exec(
            CommandOrigin::InternalTrusted("bounded input fixture"),
            std::env::current_exe()?,
            vec![
                "--exact".into(),
                "core::proctree::tests::bounded_input_fixture".into(),
                "--nocapture".into(),
            ],
            root.to_path_buf(),
            root.to_path_buf(),
        )
        .private_tmp(false)
        .background(false)
        .extra_env(vec![
            ("AIZEN_HOME".into(), home.to_str().unwrap().into()),
            (
                "AIZEN_BOUNDED_INPUT_FIXTURE_ROOT".into(),
                root.to_str().unwrap().into(),
            ),
            ("AIZEN_BOUNDED_INPUT_FIXTURE_MODE".into(), mode.into()),
        ]),
    )
}

#[test]
fn bounded_input_zero_exit_cannot_hide_undelivered_bytes() -> anyhow::Result<()> {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || -> anyhow::Result<()> {
        let result = bounded_input_request(fixture.path(), &home, "ignore")?.output_bounded_owned(
            Some(vec![0xA5; 8 * 1024 * 1024]),
            Duration::from_secs(20),
            Duration::from_secs(2),
            None,
        );
        assert_eq!(
            std::fs::read(fixture.path().join("receipt.bin"))?,
            b"ignored input; exited successfully"
        );
        let error = match result {
            Err(error) => error,
            Ok(output) => anyhow::bail!(
                "undelivered input reported success: code={:?}, timeout={}, settlement={:?}",
                output.code,
                output.timed_out,
                output.settlement
            ),
        };
        assert!(
            error
                .cause
                .to_string()
                .contains("bounded stdin delivery failed"),
            "{error}"
        );
        assert_eq!(error.settlement, BoundedSettlement::Proven);
        check_bounded_effects()?;
        Ok(())
    })
}

#[test]
fn bounded_input_missing_pipe_is_error_after_settlement() -> anyhow::Result<()> {
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
        let (child, containment) = prepared.spawn_contained()?;
        let result = output_bounded_spawned(
            child,
            containment,
            Some(b"required input".to_vec()),
            Duration::from_secs(20),
            Duration::from_secs(2),
            Some(Box::new(prepared.into_guard())),
            true,
            Some(4096),
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => anyhow::bail!("missing stdin pipe accepted required input"),
        };
        assert!(
            error
                .cause
                .to_string()
                .contains("bounded stdin delivery failed"),
            "{error}"
        );
        assert_eq!(error.settlement, BoundedSettlement::Proven);
        assert_eq!(
            std::fs::read(fixture.path().join("receipt.bin"))?,
            b"ignored input; exited successfully"
        );
        check_bounded_effects()?;
        Ok(())
    })
}

#[test]
fn bounded_input_complete_binary_delivery_remains_successful() -> anyhow::Result<()> {
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let home = fixture.path().join("home");
    std::fs::create_dir(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    crate::core::workspace_txn::with_test_lock_home(fixture.path(), || -> anyhow::Result<()> {
        let bytes: Vec<u8> = (0..32768).map(|i| (i % 256) as u8).collect();
        let output = bounded_input_request(fixture.path(), &home, "read")?.output_bounded_owned(
            Some(bytes.clone()),
            Duration::from_secs(20),
            Duration::from_secs(2),
            None,
        )?;
        assert_eq!(output.code, Some(0));
        assert!(!output.timed_out && !output.output_truncated);
        assert_eq!(output.settlement, BoundedSettlement::Proven);
        assert_eq!(std::fs::read(fixture.path().join("receipt.bin"))?, bytes);
        check_bounded_effects()?;
        Ok(())
    })
}
