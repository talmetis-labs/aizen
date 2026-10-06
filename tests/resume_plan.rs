use std::process::Command;

/// E2E verify cho run-resume: tạo 1 workflow run log giả (2/3 task đã xong),
/// rồi chạy `aizen run-resume <id>` (dry-run) và khẳng định plan đúng:
/// task đã xong → skip, task chưa chạy → run. Không gọi LLM nào.
#[test]
fn resume_plan_skips_completed_tasks_and_reruns_the_rest() {
    let runs_dir = std::env::temp_dir().join(format!("aizen-e2e-resume-{}", std::process::id()));
    std::fs::create_dir_all(&runs_dir).unwrap();

    let run_id = "wf-e2e-verify";
    let log = runs_dir.join(format!("{run_id}.jsonl"));
    std::fs::write(
        &log,
        concat!(
            "{\"kind\":\"note\",\"seq\":0,\"label\":\"workflow-spec\",\"body\":\"{\\\"name\\\":\\\"e2e\\\",\\\"tasks\\\":[\\\"scout\\\",\\\"impl\\\",\\\"verify\\\"]}\"}\n",
            "{\"kind\":\"task\",\"seq\":1,\"task_id\":\"scout\",\"status\":\"ok\",\"summary\":\"found the bug\",\"iters\":3,\"tokens_in\":1200,\"tokens_out\":340}\n",
            "{\"kind\":\"task\",\"seq\":2,\"task_id\":\"impl\",\"status\":\"ok\",\"summary\":\"patched parser\",\"iters\":5,\"tokens_in\":2600,\"tokens_out\":810}\n"
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_aizen");
    let out = Command::new(exe)
        .args(["run-resume", run_id])
        .env("AIZEN_RUNS_DIR", &runs_dir)
        .output()
        .expect("run aizen run-resume");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "run-resume failed: {text}");

    assert!(
        text.contains("skip  scout"),
        "scout must be skipped:\n{text}"
    );
    assert!(text.contains("skip  impl"), "impl must be skipped:\n{text}");
    assert!(text.contains("run   verify"), "verify must re-run:\n{text}");
    assert!(text.contains("dry-run"), "no --spec => dry-run:\n{text}");

    let _ = std::fs::remove_dir_all(&runs_dir);
}
