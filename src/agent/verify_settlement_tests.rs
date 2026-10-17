//! Actual owned verifier wrappers and descendants; no synthetic process-tree proof.

#![cfg(windows)]

use super::*;
use crate::sandbox::request::SandboxRequest;
use crate::sandbox::{runner, CommandOrigin};
use std::path::PathBuf;
use std::process::Stdio;

const HELPER: &str = "agent::verify_gate::settlement_tests::verify_descendant_helper";
const ROOT_ENV: &str = "AIZEN_VERIFY_DESCENDANT_TEST_ROOT";

#[test]
fn verify_descendant_helper() -> anyhow::Result<()> {
    if !std::env::args().any(|arg| arg == HELPER) {
        return Ok(());
    }
    let Some(root) = std::env::var_os(ROOT_ENV) else {
        return Ok(());
    };
    let root = std::fs::canonicalize(root)?;
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    anyhow::ensure!(
        root.starts_with(&temp) && root != temp,
        "owned fixture root required"
    );
    let phase = std::env::var("AIZEN_VERIFY_DESCENDANT_TEST_PHASE")?;
    let scenario = std::env::var("AIZEN_VERIFY_DESCENDANT_TEST_SCENARIO")?;
    if phase == "leaf" {
        std::fs::write(root.join("leaf-started"), "owned descendant")?;
        if scenario == "natural" {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !root.join("release-leaf").is_file() {
                anyhow::ensure!(Instant::now() < deadline, "owned leaf was never released");
                std::thread::sleep(Duration::from_millis(10));
            }
        } else {
            std::thread::sleep(Duration::from_secs(15));
        }
        return Ok(());
    }
    let home = root.join("home");
    let request = SandboxRequest::exec(
        CommandOrigin::InternalTrusted("verify descendant fixture"),
        std::env::current_exe()?,
        vec!["--exact".into(), HELPER.into(), "--nocapture".into()],
        root.clone(),
        root.clone(),
    )
    .private_tmp(false)
    .background(false)
    .extra_env(vec![
        (ROOT_ENV.into(), root.to_str().unwrap().into()),
        ("AIZEN_HOME".into(), home.to_str().unwrap().into()),
        ("AIZEN_VERIFY_DESCENDANT_TEST_PHASE".into(), "leaf".into()),
        (
            "AIZEN_VERIFY_DESCENDANT_TEST_SCENARIO".into(),
            scenario.clone(),
        ),
    ]);
    let mut prepared = runner::prepare_std(request)?;
    let keep_pipes = scenario == "timeout" || scenario == "cancel";
    prepared.command.stdin(Stdio::null());
    if keep_pipes {
        prepared
            .command
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        // Windows may inherit unrelated inheritable standard handles even with null stdio.
        // Keep the fixture leaf alive WITHOUT retaining the verifier's pipe writers, so this
        // scenario exercises the separate tree-proof phase rather than the pipe timeout phase.
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        };
        for standard in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: only this owned helper's valid standard handles are modified; no close.
            let ok =
                unsafe { SetHandleInformation(GetStdHandle(standard), HANDLE_FLAG_INHERIT, 0) };
            anyhow::ensure!(
                ok != 0,
                "owned pipe inheritance update failed: {}",
                std::io::Error::last_os_error()
            );
        }
        prepared.command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    // The outer verifier already protected its whole tree. The leaf stays in that owned Job;
    // wrapping it in a second kill-on-close Job would erase the scenario at wrapper exit.
    let _leaf = prepared.command.spawn()?;
    prepared.finish(runner::Outcome::Spawned);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("leaf-started").is_file() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "leaf did not enter its actual body"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(root.join("wrapper-exited"), "owned wrapper")?;
    // Exit the real wrapper process immediately after publication. This avoids libtest/runtime
    // teardown becoming an accidental extra wait; the descendant remains in the outer Job.
    std::process::exit(0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::await_holding_lock,
    reason = "Owned HOME and current context remain pinned until actual verification tree settlement"
)]
async fn real_verify_descendant_settlement_preserves_results_and_ownership() -> anyhow::Result<()> {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let directory = crate::core::file_placement::tests::TestDir::new();
    let root: PathBuf = directory.path().join("workspace");
    let home = root.join("home");
    std::fs::create_dir_all(&home)?;
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    let placement = crate::core::file_placement::PlacementContext::with_paths(
        root.clone(),
        home.clone(),
        directory.path().join("process"),
        "verify-settlement-test",
    )?;
    let ctx = ExecutionContext::default()
        .with_placement(placement)
        .with_resource_scope("verify-settlement-parent");
    let exe = std::env::current_exe()?;
    let temp = std::env::temp_dir();
    for scenario in ["natural", "forced", "timeout", "cancel"] {
        for marker in ["leaf-started", "wrapper-exited", "release-leaf"] {
            let path = root.join(marker);
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        // The real verify sandbox deliberately scrubs AIZEN_* and relocates TEMP. Restore only
        // this owned fixture's values in its command, not the parent environment or pass_env.
        let command = format!(
            "set \"{ROOT_ENV}={}\" & set \"AIZEN_HOME={}\" & set \"TEMP={}\" & set \"TMP={}\" & set \"AIZEN_VERIFY_DESCENDANT_TEST_PHASE=wrapper\" & set \"AIZEN_VERIFY_DESCENDANT_TEST_SCENARIO={scenario}\" & \"{}\" --exact {HELPER} --nocapture --test-threads=1",
            root.display(), home.display(), temp.display(), temp.display(), exe.display(),
        );
        let cancel = crate::core::cancel::TurnCancel::new();
        let controller_root = root.clone();
        let controller_cancel = cancel.clone();
        let identity = crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&root)?;
        let controller = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !controller_root.join("wrapper-exited").is_file() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "actual verifier wrapper not observed"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                crate::core::workspace_txn::WorkspaceWriterLease::held_by_this_process(&identity),
                "actual descendant keeps workspace ownership after wrapper exit"
            );
            if scenario == "natural" {
                std::fs::write(controller_root.join("release-leaf"), "owned release").unwrap();
            }
            if scenario == "cancel" {
                controller_cancel.cancel();
            }
        });
        let result = crate::core::cancel::with_current(cancel, || {
            crate::core::exec_ctx::with_current(ctx.clone(), || {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(run_one_verify(
                        &root,
                        &VerifyCommand::Custom(command.clone()),
                        if scenario == "timeout" { 1 } else { 10 },
                        &ctx,
                    ))
                })
            })
        });
        controller.await?;
        if scenario == "cancel" {
            assert!(
                result.is_none(),
                "settled cancellation cannot become a passing check: {result:?}"
            );
        } else {
            let result = result.expect("owned verifier command must run");
            assert_eq!(
                result.effect_state,
                crate::agent::tools::ToolEffectState::Settled,
                "{scenario}: {}",
                result.output
            );
            assert_eq!(
                result.passed,
                scenario == "natural",
                "{scenario}: {}",
                result.output
            );
            if scenario == "forced" {
                assert!(
                    result.output.contains("requiring tree termination"),
                    "{}",
                    result.output
                );
            }
            if scenario == "timeout" {
                assert!(
                    result.output.starts_with(TIMED_OUT_PREFIX),
                    "{}",
                    result.output
                );
            }
        }
        let identity = crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&root)?;
        assert!(
            !crate::core::workspace_txn::WorkspaceWriterLease::held_by_this_process(&identity),
            "settled {scenario} must release actual ownership"
        );
        assert!(crate::core::proctree::check_bounded_effects().is_ok());
    }
    let failed = crate::core::exec_ctx::with_current(ctx.clone(), || {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(run_one_verify(
                &root,
                &VerifyCommand::Custom("exit /b 7".into()),
                10,
                &ctx,
            ))
        })
    })
    .unwrap();
    assert!(!failed.passed);
    assert_eq!(
        failed.effect_state,
        crate::agent::tools::ToolEffectState::Settled
    );
    Ok(())
}
