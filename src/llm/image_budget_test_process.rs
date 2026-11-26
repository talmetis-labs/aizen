//! Exhaustion fixtures must not consume the parallel harness's process-wide image budget.
//! Run the same exact test in a bounded child; production admission and assertions stay unchanged.

use crate::core::{exec_ctx, file_placement::tests::TestDir};
use crate::sandbox::{request::SandboxRequest, runner, CommandOrigin};
use std::sync::Arc;
use std::time::Duration;

/// Returns true in the parent after the exact child test passed; false only inside that child.
/// Call before taking any image, HOME, sandbox or workspace test lock.
pub(crate) fn isolated_image_budget_test(loopback_fixture: bool) -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("named image-budget test thread");
    const CHILD: &str = "AIZEN_IMAGE_BUDGET_TEST_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        let args: Vec<_> = std::env::args().collect();
        assert!(args.iter().any(|arg| arg == "--exact"));
        assert!(args.iter().any(|arg| arg == name));
        assert!(args.iter().any(|arg| arg == "--test-threads=1"));
        return false;
    }
    let _mode = runner::tests::lock();
    let _home = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = Arc::new(TestDir::new());
    let root = fixture.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
    crate::core::workspace_txn::with_test_lock_home(&root, || {
        let context = exec_ctx::ExecutionContext::default()
            .with_resource_scope("isolated-image-budget-test")
            .with_task(crate::agent::todo::TaskContext::private());
        exec_ctx::with_current(context, || {
            let request = SandboxRequest::exec(
                CommandOrigin::InternalTrusted("isolated image budget test"),
                std::env::current_exe().unwrap(),
                vec![
                    "--exact".into(),
                    name.into(),
                    "--nocapture".into(),
                    "--test-threads=1".into(),
                ],
                root.clone(),
                root.clone(),
            )
            // The transport fixture binds an ephemeral loopback address and uses a no-proxy
            // client. Other image-budget cases keep the child offline.
            .network(loopback_fixture)
            .private_tmp(false)
            .background(false)
            .extra_env(vec![
                (CHILD.into(), name.into()),
                ("AIZEN_HOME".into(), home.to_str().unwrap().into()),
            ]);
            let output = runner::prepare_std(request)
                .unwrap()
                .output_bounded_owned(
                    None,
                    Duration::from_secs(90),
                    Duration::from_secs(2),
                    Some(Box::new(fixture.clone())),
                )
                .unwrap();
            assert!(!output.timed_out && !output.output_truncated);
            assert_eq!(
                output.settlement,
                crate::core::proctree::BoundedSettlement::Proven
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(
                output.code,
                Some(0),
                "{name}:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains("test result: ok. 1 passed;"),
                "exact child test was not executed: {stdout}"
            );
        });
    });
    true
}
