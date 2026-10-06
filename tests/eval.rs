//! Eval harness cho `aizen chat` (Phase 1). Mỗi task = một prompt + một rubric cơ chế được
//! (keyword/file phải tồn tại) — KHÔNG gọi LLM-judge, nên chạy được trong CI và rẻ.
//! Judge bằng LLM là bước sau; ở đây ta mới cần số liệu baseline chạy/không chạy được.
//!
//! Chạy: `cargo test --test eval` (cần AIZEN_MODEL + key; không có thì toàn bộ `skipped`).

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Copy)]
struct EvalTask {
    id: &'static str,
    prompt: &'static str,
    /// Chữ phải xuất hiện trong stdout (case-insensitive).
    expect_stdout: &'static [&'static str],
    /// File phải tồn tại sau khi chạy (relative to cwd).
    expect_files: &'static [&'static str],
}

/// Tập ~20 task trải khắp các kiểu việc chính (code gen, refactor, đọc hiểu, tool dùng).
/// Không cần mạng cho `cargo test` pass — chúng chỉ chạy thật khi có endpoint.
const TASKS: &[EvalTask] = &[
    EvalTask { id: "echo-hello",      prompt: "Print exactly: hello aizen",                       expect_stdout: &["hello aizen"],                  expect_files: &[] },
    EvalTask { id: "add-two-numbers", prompt: "What is 2+2? Reply with just the number.",          expect_stdout: &["4"],                            expect_files: &[] },
    EvalTask { id: "list-capital",    prompt: "Capital of France? One word.",                      expect_stdout: &["paris"],                        expect_files: &[] },
    EvalTask { id: "rust-fn-sig",     prompt: "Write a Rust fn signature `add(a: i32, b: i32) -> i32` and nothing else.", expect_stdout: &["fn add"], expect_files: &[] },
    EvalTask { id: "json-out",        prompt: "Output {\"ok\":true} as JSON.",                     expect_stdout: &["\"ok\""],                       expect_files: &[] },
    EvalTask { id: "lower-shout",     prompt: "Reply with the lowercase of: HELLO",                expect_stdout: &["hello"],                        expect_files: &[] },
    EvalTask { id: "count-to-three",  prompt: "Count: 1 2 3",                                      expect_stdout: &["1", "2", "3"],                 expect_files: &[] },
    EvalTask { id: "say-red",         prompt: "Say a primary color. One word.",                    expect_stdout: &[],                                expect_files: &[] },
    EvalTask { id: "bool-true",       prompt: "Is the sky blue on a clear day? yes/no.",           expect_stdout: &["yes"],                          expect_files: &[] },
    EvalTask { id: "negate",          prompt: "Negate the word: hot",                              expect_stdout: &["cold"],                         expect_files: &[] },
    EvalTask { id: "py-len",          prompt: "In Python, what does len(\"abc\") return? Number.", expect_stdout: &["3"],                            expect_files: &[] },
    EvalTask { id: "odd-or-even",     prompt: "Is 7 odd or even? One word.",                       expect_stdout: &["odd"],                          expect_files: &[] },
    EvalTask { id: "translate-xin",   prompt: "Translate 'hello' to Vietnamese. One word.",        expect_stdout: &["chào", "xin"],                  expect_files: &[] },
    EvalTask { id: "date-year",       prompt: "What year did the Apollo 11 land? Number.",         expect_stdout: &["1969"],                         expect_files: &[] },
    EvalTask { id: "http-verb",       prompt: "Which HTTP verb creates a resource? One word.",     expect_stdout: &["post"],                         expect_files: &[] },
    EvalTask { id: "git-undo",        prompt: "Which git command undoes the last commit, keeping changes? One line.", expect_stdout: &["git", "reset"], expect_files: &[] },
    EvalTask { id: "big-o",           prompt: "Binary search time complexity? Big-O only.",        expect_stdout: &["log"],                           expect_files: &[] },
    EvalTask { id: "http-404",        prompt: "What does HTTP 404 mean? Two words.",               expect_stdout: &["not found"],                    expect_files: &[] },
    EvalTask { id: "sql-select-all",  prompt: "SQL to select all rows from table t?",              expect_stdout: &["select", "from"],                expect_files: &[] },
    EvalTask { id: "regex-digit",     prompt: "Regex for a digit? One token.",                     expect_stdout: &["\\d", "d"],                      expect_files: &[] },
];

fn endpoint_ready() -> bool {
    // `aizen chat` resolves endpoint từ ~/.aizen/cli-config.json; chỉ cần file đó tồn tại.
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    home.map(|h| PathBuf::from(h).join(".aizen").join("cli-config.json").exists())
        .unwrap_or(false)
}

/// LLM-judge: hỏi model chấm (output, rubric) → `{"pass": bool, "reason": "..."}`.
/// Trả Err khi không parse được — eval ghi nhận là judge-error, không tính vào pass/fail.
fn judge(output: &str, rubric: &str) -> Result<bool, String> {
    let exe = env!("CARGO_BIN_EXE_aizen");
    let prompt = format!(
        "You are an eval judge. Output ONLY a JSON object with keys `pass` (boolean) and \
         `reason` (one sentence). No prose, no markdown fences.\n\
         RUBRIC: {rubric}\n\
         OUTPUT TO JUDGE:\n{output}"
    );
    let out = Command::new(exe)
        .args(["chat", "-p", &prompt])
        .output()
        .map_err(|e| format!("judge spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("judge exit {:?}", out.status.code()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Tìm JSON object đầu tiên trong output (model đôi khi vẫn thêm text xung quanh).
    let start = text.find('{').ok_or("judge: no json in output")?;
    let end = text.rfind('}').ok_or("judge: no json close")?;
    let v: serde_json::Value = serde_json::from_str(&text[start..=end])
        .map_err(|e| format!("judge json: {e}"))?;
    v.get("pass")
        .and_then(|p| p.as_bool())
        .ok_or_else(|| "judge: missing `pass` bool".to_string())
}

fn run_task(t: &EvalTask) -> Result<bool, String> {
    let exe = env!("CARGO_BIN_EXE_aizen");
    let out = Command::new(exe)
        .args(["chat", "-p", t.prompt])
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("exit {:?}\nstdout: {stdout}\nstderr: {stderr}", out.status.code()));
    }
    let cwd = std::env::current_dir().unwrap();
    for f in t.expect_files {
        let p: PathBuf = cwd.join(f);
        if !p.exists() {
            return Err(format!("missing file {f}"));
        }
    }
    for needle in t.expect_stdout {
        if !stdout.contains(&needle.to_lowercase()) {
            return Err(format!("stdout missing `{needle}`\nstdout: {stdout}"));
        }
    }
    Ok(true)
}

/// Bài đo baseline — gọi LLM thật (~20 task × 2 call, nhiều phút), KHÔNG phải CI gate.
/// Mặc định bị ignore để `cargo test --test eval` xong trong vài giây; chạy thật bằng:
///   cargo test --test eval --release -- --ignored --nocapture
#[test]
#[ignore = "needs a live endpoint and takes minutes; run with --ignored --nocapture"]
fn eval_baseline() {
    if !endpoint_ready() {
        eprintln!("skip: no ~/.aizen/cli-config.json — eval needs an endpoint");
        return;
    }
    let mut passed = 0usize;
    let mut failed = Vec::new();
    let mut judged = 0usize;
    for t in TASKS {
        match run_task(t) {
            Ok(true) => {
                // Keyword/file pass → hỏi judge xem có đồng ý không (chỉ để đo, không gate).
                if let Ok(output) = capture_output(t) {
                    match judge(output.trim(), &t.judge_rubric()) {
                        Ok(true) => judged += 1,
                        Ok(false) => eprintln!("  judge disagree: {}", t.id),
                        Err(e) => eprintln!("  judge error: {}: {e}", t.id),
                    }
                }
                passed += 1;
            }
            Ok(false) | Err(_) => failed.push(t.id),
        }
    }
    let total = TASKS.len();
    println!("eval baseline: {passed}/{total} keyword-passed, {judged}/{passed} judge-agreed");
    for id in &failed {
        println!("  fail: {id}");
    }
    assert_eq!(passed, total, "{} task(s) failed: {:?}", failed.len(), failed);
}

/// Chạy lại task để lấy output (judge cần). Tách khỏi run_task để keyword-check nhanh hơn.
fn capture_output(t: &EvalTask) -> Result<String, String> {
    let exe = env!("CARGO_BIN_EXE_aizen");
    let out = Command::new(exe)
        .args(["chat", "-p", t.prompt])
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("exit {:?}", out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl EvalTask {
    fn judge_rubric(&self) -> String {
        match self.expect_stdout.first() {
            Some(kw) => format!("The output must contain or clearly express: `{kw}`."),
            None => "The output must be a valid answer to the prompt.".to_string(),
        }
    }
}
