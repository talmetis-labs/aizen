//! Fast verify gate (the harness's F2 lever — the extension's `qualityGate`, ported lean).
//!
//! After an editing run, before the agent reports Done, run a FAST typecheck/build (never the
//! test suite — that's slow and flaky) once. On failure, the loop injects the compiler errors
//! and grants one fix turn. This catches the "model says done but it doesn't compile" failure
//! mode for ~one subprocess of cost. Best-effort throughout: a missing toolchain / unknown
//! project shape / spawn failure all degrade to a silent no-op (never block, never panic).
//!
//! Detection priority (cross-platform via `std::path::Path`): `Cargo.toml` → `cargo check`;
//! else `package.json` with a `typecheck`/`type-check`/`tsc` script → `npm run <script>`; else
//! `tsconfig.json` → `npx tsc --noEmit`; else `None`. Commands run through the platform shell
//! (`cmd /C` / `sh -c`) so the npm/npx `.cmd` shims resolve on Windows.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// The detected verify command for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyCommand {
    /// Rust project: `cargo check` (typecheck-equivalent, faster than a full build).
    Cargo,
    /// Node project: `npm run <script>` (the first of typecheck / type-check / tsc that exists).
    Npm(String),
    /// TypeScript with a `tsconfig.json` but no script: `npx tsc --noEmit`.
    NpxTsc,
    /// Go module: `go build ./...` (the typecheck), then [`VerifyCommand::GoVet`].
    GoBuild,
    /// Go module: `go vet ./...` after a clean build.
    GoVet,
    /// Maven project: compile only, tests skipped.
    Maven,
    /// Gradle project: `compileJava` through the wrapper when the repo ships one (the
    /// string is the launcher: `./gradlew`, `gradlew.bat`, or plain `gradle`).
    Gradle(String),
    /// .NET project or solution: `dotnet build`, quiet.
    DotNet,
    /// Python project: a byte-compile pass over the tree. Python has no universal typecheck and
    /// the module's contract is "never the test suite", so this is the honest fast rung: it
    /// catches syntax errors and nothing else. `python3` is tried first, then `python`; a
    /// missing interpreter is a skip, not a failure (see [`looks_like_missing_tool`]).
    Python,
    /// A project-supplied command from `./.aizen/verify.json` (trust-gated — see
    /// [`detect_verify_commands`]).
    Custom(String),
    /// `cargo test -q [--test <target>…] [-- <filter>…]`: the modules the edited Rust files
    /// define (`name::`) and the integration-test targets edited under `tests/`.
    CargoTest {
        filters: Vec<String>,
        targets: Vec<String>,
    },
    /// `<py> -m pytest -q <files>`: the test files that name the edited Python modules.
    Pytest { py: String, files: Vec<String> },
    /// `<py> -m unittest -q <files>`: the same when the project shows no pytest.
    Unittest { py: String, files: Vec<String> },
    /// `go test <pkgs>`: the packages whose files were edited and that carry `_test.go` files.
    GoTest(Vec<String>),
    /// `npx vitest run <files>` / `npx jest <files>`: the sibling test files of the edited
    /// JS/TS files under the runner `package.json` declares.
    NodeTest { runner: String, files: Vec<String> },
    /// The project's whole test suite — detected from the manifest, or `suite` in a trusted
    /// `.aizen/verify.json`. The last rung of the ladder; see [`plan_ladder`].
    Suite(String),
}

impl VerifyCommand {
    /// The shell command line to execute.
    pub fn command_line(&self) -> String {
        match self {
            VerifyCommand::Cargo => "cargo check".to_string(),
            VerifyCommand::Npm(script) => format!("npm run {script}"),
            VerifyCommand::NpxTsc => "npx tsc --noEmit".to_string(),
            VerifyCommand::GoBuild => "go build ./...".to_string(),
            VerifyCommand::GoVet => "go vet ./...".to_string(),
            VerifyCommand::Maven => "mvn -q -DskipTests compile".to_string(),
            VerifyCommand::Gradle(launcher) => format!("{launcher} -q compileJava"),
            VerifyCommand::DotNet => "dotnet build --nologo -v q".to_string(),
            VerifyCommand::Python => {
                const ARGS: &str = r#"-m compileall -q -x "(\.venv|venv|env|node_modules|\.git|build|dist|__pycache__)" ."#;
                format!("python3 {ARGS} || python {ARGS}")
            }
            VerifyCommand::Custom(c) => c.clone(),
            VerifyCommand::CargoTest { filters, targets } => {
                let mut c = String::from("cargo test -q");
                for t in targets {
                    c.push_str(" --test ");
                    c.push_str(t);
                }
                if !filters.is_empty() {
                    c.push_str(" -- ");
                    c.push_str(&filters.join(" "));
                }
                c
            }
            VerifyCommand::Pytest { py, files } => {
                format!("{py} -m pytest -q {}", quote_all(files))
            }
            VerifyCommand::Unittest { py, files } => {
                format!("{py} -m unittest -q {}", quote_all(files))
            }
            VerifyCommand::GoTest(pkgs) => format!("go test {}", pkgs.join(" ")),
            VerifyCommand::NodeTest { runner, files } => match runner.as_str() {
                "vitest" => format!("npx vitest run {}", quote_all(files)),
                _ => format!("npx jest {}", quote_all(files)),
            },
            VerifyCommand::Suite(c) => c.clone(),
        }
    }
}

/// Shell-quote a list of relative paths: bare when plain, double-quoted when they carry a
/// space (the one case both `cmd /C` and `sh -c` agree on).
fn quote_all(files: &[String]) -> String {
    files
        .iter()
        .map(|f| {
            if f.chars().any(|c| c.is_whitespace() || c == '"') {
                format!("\"{}\"", f.replace('"', ""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The demand the loop injects when a run edited files but no verify command could run — no
/// recognised manifest, or a toolchain that is not installed. It exists because the old shape
/// fell straight through to `Done` in silence: "verified done" was true for Rust and TypeScript
/// and a fiction everywhere else.
pub const ABSENCE_DEMAND: &str = "[verify] No build or test command could be run for this project \
    (looked for Cargo.toml, package.json, tsconfig.json, go.mod, pom.xml, build.gradle, a .csproj/.sln, \
    pyproject.toml/setup.py and .aizen/verify.json — or the toolchain is not installed here). You edited \
    files: before finishing, run the project's own build or test command with shell_run and quote its \
    exit code and any failing lines, or state plainly that no such command exists. Do not report done \
    on an unverified change.";

/// The outcome of one verify-gate run.
#[derive(Debug, Clone)]
pub struct VerifyGateResult {
    pub passed: bool,
    pub command: String,
    pub output: String,
    pub duration_ms: u128,
    pub stable: bool,
}

/// Keep only the LAST `MAX_OUTPUT_CHARS` of combined output — compiler error stacks live at the
/// tail (the summary line, the error count), and that's what the model needs to fix.
const MAX_OUTPUT_CHARS: usize = 4000;

/// Detect the project's fast-verify command, or `None` if the shape isn't recognized. The first
/// of [`detect_builtin_verify_commands`]; the loop runs the whole list, so today only the tests
/// (which check one shape at a time) call this.
#[cfg_attr(not(test), allow(dead_code))]
pub fn detect_verify_command(cwd: &Path) -> Option<VerifyCommand> {
    detect_builtin_verify_commands(cwd).into_iter().next()
}

/// The built-in verify command list for a project, by manifest, in run order. Cargo first: a repo
/// may carry both manifests, and `cargo check` is the faster, more precise typecheck for the Rust
/// crate the agent most likely just edited. Then the Node shapes, then Go (build, then vet), the
/// JVM builds, .NET, and last the Python byte-compile. Empty when nothing is recognised.
pub fn detect_builtin_verify_commands(cwd: &Path) -> Vec<VerifyCommand> {
    if cwd.join("Cargo.toml").is_file() {
        return vec![VerifyCommand::Cargo];
    }
    let pkg = cwd.join("package.json");
    if pkg.is_file() {
        if let Some(script) = detect_npm_typecheck_script(&pkg) {
            return vec![VerifyCommand::Npm(script)];
        }
    }
    if cwd.join("tsconfig.json").is_file() {
        return vec![VerifyCommand::NpxTsc];
    }
    if cwd.join("go.mod").is_file() {
        return vec![VerifyCommand::GoBuild, VerifyCommand::GoVet];
    }
    if cwd.join("pom.xml").is_file() {
        return vec![VerifyCommand::Maven];
    }
    if cwd.join("build.gradle").is_file() || cwd.join("build.gradle.kts").is_file() {
        let launcher = if cwd.join("gradlew.bat").is_file() && cfg!(windows) {
            "gradlew.bat".to_string()
        } else if cwd.join("gradlew").is_file() && !cfg!(windows) {
            "./gradlew".to_string()
        } else {
            "gradle".to_string()
        };
        return vec![VerifyCommand::Gradle(launcher)];
    }
    if has_file_with_extension(cwd, &["csproj", "fsproj", "sln"]) {
        return vec![VerifyCommand::DotNet];
    }
    if [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pytest.ini",
        "requirements.txt",
    ]
    .iter()
    .any(|m| cwd.join(m).is_file())
    {
        return vec![VerifyCommand::Python];
    }
    Vec::new()
}

/// Does `dir` (non-recursively) contain a file with one of these extensions?
fn has_file_with_extension(dir: &Path, exts: &[&str]) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| exts.iter().any(|w| x.eq_ignore_ascii_case(w)))
            })
        })
        .unwrap_or(false)
}

/// Did a non-zero exit mean "the toolchain is not installed" rather than "the check failed"?
/// `sh` answers 127 and `cmd.exe` 9009 for an unknown program, and the wrapper shell's message
/// is the LAST thing printed. Judged on the tail of the output on purpose: a `python3 … ||
/// python …` line whose first half is missing and whose second half finds real errors ends with
/// the errors, and must stay a failure.
pub fn looks_like_missing_tool(code: Option<i32>, output: &str) -> bool {
    if matches!(code, Some(0)) {
        return false; // a check that passed is a pass, whatever a first `||` half printed
    }
    if matches!(code, Some(127) | Some(9009)) {
        return true;
    }
    let tail: Vec<&str> = output
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(2)
        .collect();
    tail.iter().any(|l| {
        let l = l.trim();
        l.contains("is not recognized as an internal or external command")
            || l.contains("operable program or batch file")
            || l.ends_with("command not found")
            || l.ends_with(": not found")
            || l.starts_with("No module named ")
            || l.contains(": No module named ")
    })
}

/// Walk up from `start` (a directory, or a file's directory) to the nearest ancestor that looks
/// like a project root — one carrying a manifest `detect_verify_command` recognizes (`Cargo.toml`,
/// `package.json`, `tsconfig.json`) or a `./.aizen/verify.json`. Returns that directory, or `None`
/// if no ancestor qualifies (the gate then falls back to cwd, preserving the old behavior).
///
/// This exists because the file tools can write OUTSIDE the process cwd (the confine guard was
/// removed): the write target's directory is where the edit really landed, but that directory is
/// often a `src/` subfolder, not the crate root where `Cargo.toml` lives. Detection keys on the
/// manifest sitting in the SAME directory, so the gate must first climb to the directory that holds
/// it — otherwise a real edit to `proj/src/x.rs` finds no manifest in `proj/src/` and skips
/// verification silently, the exact gap this closes.
pub fn verify_root(start: &Path) -> Option<std::path::PathBuf> {
    // If `start` names a file, begin at its directory; a directory begins at itself.
    let mut dir: &Path = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    loop {
        let has_manifest = [
            "Cargo.toml",
            "package.json",
            "tsconfig.json",
            "go.mod",
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "pytest.ini",
            "requirements.txt",
        ]
        .iter()
        .any(|m| dir.join(m).is_file())
            || has_file_with_extension(dir, &["csproj", "fsproj", "sln"])
            || dir.join(".aizen").join("verify.json").is_file();
        if has_manifest {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// The COMMAND LIST for a project: `./.aizen/verify.json` (project-supplied, e.g.
/// `{"commands": ["cargo test --lib", "cargo clippy"], "timeout_secs": 180}`) when present AND the
/// project is TRUSTED — auto-running repo-supplied commands is the same supply-chain surface as
/// project mcp.json, so it sits behind the same `mcp::project_trusted()` gate, plus the cmd_guard
/// hard floor per command. Otherwise the built-in single detection. Run in order; first failure is
/// the gate result.
pub fn detect_verify_commands(cwd: &Path) -> Vec<VerifyCommand> {
    if let Some(customs) = load_custom_verify(cwd) {
        if !customs.is_empty() {
            return customs;
        }
    }
    detect_builtin_verify_commands(cwd)
}

/// Parse the trusted `./.aizen/verify.json` commands (≤3 honored; Blocked commands dropped).
/// `None` ⇒ no usable custom file (missing / untrusted / unparseable).
fn load_custom_verify(cwd: &Path) -> Option<Vec<VerifyCommand>> {
    let text = std::fs::read_to_string(cwd.join(".aizen").join("verify.json")).ok()?;
    if !crate::agent::mcp::project_trusted() {
        return None; // untrusted repo → the file is inert
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(
        v.get("commands")?
            .as_array()?
            .iter()
            .filter_map(|c| c.as_str())
            .take(3)
            .filter(|c| {
                !matches!(
                    crate::agent::cmd_guard::classify(c),
                    crate::agent::cmd_guard::Verdict::Blocked(_)
                )
            })
            .map(|c| VerifyCommand::Custom(c.to_string()))
            .collect(),
    )
}

/// The custom file's per-command timeout (clamped [10, 600]); `None` when absent/untrusted.
fn custom_verify_timeout(cwd: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(cwd.join(".aizen").join("verify.json")).ok()?;
    if !crate::agent::mcp::project_trusted() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("timeout_secs")?.as_u64().map(|t| t.clamp(10, 600))
}

/// The verify budget the REPL and `/init` run under when no loop config names one.
pub const DEFAULT_TIMEOUT_SECS: u64 = 90;

/// How a timed-out command reports itself (the ladder reads it back to tell a slow suite from a
/// broken one).
const TIMED_OUT_PREFIX: &str = "verify timed out after ";

// ---------------------------------------------------------------------------------------------
// The verify ladder (FL lever F): cheapest sufficient check first.
//
// Diagnostics on the edited files are rung zero and live in the LSP fold. These are the rungs that
// spawn a process: the typecheck the gate always ran, the narrowest test the edited files name,
// and — at Done, on a multi-file change, when it is known to fit the budget — the whole suite.
// ---------------------------------------------------------------------------------------------

/// Where a verify step sits on the cost ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// The fast build/typecheck — what the gate always ran.
    Typecheck,
    /// The narrowest test the edited files name: a sibling test file, `cargo test -- module::`,
    /// one Go package.
    NarrowTest,
    /// The whole suite — only at Done, only on a multi-file change, only when its last measured
    /// duration fits the budget (or it has never been measured).
    FullSuite,
}

/// One step of the ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub rung: Rung,
    pub cmd: VerifyCommand,
}

/// The steps the gate will run, in order, and what it left out and why.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ladder {
    pub steps: Vec<Step>,
    pub skipped: Vec<String>,
}

/// Choose the steps for one Done-time verification of `edited` under `root`, cheapest first:
/// the typecheck list, the narrow tests the edited files name, and the suite when the change
/// touched more than one file and the suite is not known to exceed `budget_secs` (`measured` is
/// the per-command duration record; a suite that once blew the budget is skipped with a note
/// rather than re-run blindly — the loop would otherwise spend a fix round on a timeout).
pub fn plan_ladder(
    root: &Path,
    edited: &[PathBuf],
    budget_secs: u64,
    measured: &BTreeMap<String, u64>,
) -> Ladder {
    let mut ladder = Ladder::default();
    for cmd in detect_verify_commands(root) {
        ladder.steps.push(Step {
            rung: Rung::Typecheck,
            cmd,
        });
    }
    for cmd in narrow_tests(root, edited) {
        ladder.steps.push(Step {
            rung: Rung::NarrowTest,
            cmd,
        });
    }
    if edited.len() >= 2 {
        if let Some(suite) = suite_command(root) {
            let line = suite.command_line();
            match measured.get(&line) {
                Some(ms) if *ms > budget_secs.saturating_mul(1000) => ladder.skipped.push(format!(
                    "{line}: last run took {} s, over the {budget_secs} s verify budget — run it yourself before shipping",
                    ms / 1000
                )),
                _ => ladder.steps.push(Step {
                    rung: Rung::FullSuite,
                    cmd: suite,
                }),
            }
        }
    }
    ladder
}

/// The narrowest tests the edited files name, per language, in run order. Pure path and file
/// inspection: nothing is spawned. A file whose tests cannot be located contributes nothing (the
/// suite rung covers it at Done); a file outside `root` is ignored.
pub fn narrow_tests(root: &Path, edited: &[PathBuf]) -> Vec<VerifyCommand> {
    let mut out = Vec::new();
    let rel = |p: &Path| -> Option<String> {
        p.strip_prefix(root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    };
    let ext = |p: &Path| -> String {
        p.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let stem = |p: &Path| -> String {
        p.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    let push_unique = |v: &mut Vec<String>, s: String| {
        if !v.contains(&s) {
            v.push(s);
        }
    };

    // Rust: the module a file defines is the filter (`name::`); a file under `tests/` is an
    // integration target. `main`/`lib`/`build` define no module of their own — their tests, if
    // any, are the crate's, which is the suite's job.
    if root.join("Cargo.toml").is_file() {
        let mut filters: Vec<String> = Vec::new();
        let mut targets: Vec<String> = Vec::new();
        for p in edited.iter().filter(|p| ext(p) == "rs") {
            let Some(r) = rel(p) else { continue };
            let s = stem(p);
            if let Some(t) = r.strip_prefix("tests/") {
                if !t.contains('/') {
                    push_unique(&mut targets, s);
                }
                continue;
            }
            if !rust_file_has_tests(p) {
                continue;
            }
            let name = if s == "mod" {
                p.parent()
                    .and_then(|d| d.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                s
            };
            if name.is_empty() || matches!(name.as_str(), "main" | "lib" | "build") {
                continue;
            }
            push_unique(&mut filters, format!("{name}::"));
        }
        if !filters.is_empty() {
            out.push(VerifyCommand::CargoTest {
                filters,
                targets: Vec::new(),
            });
        }
        if !targets.is_empty() {
            out.push(VerifyCommand::CargoTest {
                filters: Vec::new(),
                targets,
            });
        }
    }

    // Python: the edited test file itself, or `test_<stem>.py` / `<stem>_test.py` beside it, in a
    // `tests/` or `test/` dir beside it, or under the root's `tests/` / `test/`.
    let py_edits: Vec<&PathBuf> = edited.iter().filter(|p| ext(p) == "py").collect();
    if !py_edits.is_empty() {
        let mut files: Vec<String> = Vec::new();
        for p in py_edits {
            let s = stem(p);
            let hit = if s.starts_with("test_") || s.ends_with("_test") {
                Some(p.clone())
            } else {
                let dir = p.parent().unwrap_or(root);
                let names = [format!("test_{s}.py"), format!("{s}_test.py")];
                let mut cands: Vec<PathBuf> = Vec::new();
                for n in &names {
                    for base in [
                        dir.to_path_buf(),
                        dir.join("tests"),
                        dir.join("test"),
                        root.join("tests"),
                        root.join("test"),
                    ] {
                        cands.push(base.join(n));
                    }
                }
                cands.into_iter().find(|c| c.is_file())
            };
            if let Some(r) = hit.as_deref().and_then(rel) {
                push_unique(&mut files, r);
            }
        }
        if !files.is_empty() {
            let py = python_launcher().to_string();
            out.push(if uses_pytest(root) {
                VerifyCommand::Pytest { py, files }
            } else {
                VerifyCommand::Unittest { py, files }
            });
        }
    }

    // Go: the package (directory) of each edited file, when it carries any `_test.go`.
    if root.join("go.mod").is_file() {
        let mut pkgs: Vec<String> = Vec::new();
        for p in edited.iter().filter(|p| ext(p) == "go") {
            let Some(dir) = p.parent() else { continue };
            let has_tests = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .any(|e| e.file_name().to_string_lossy().ends_with("_test.go"))
                })
                .unwrap_or(false);
            if !has_tests {
                continue;
            }
            let Some(r) = rel(dir) else { continue };
            push_unique(
                &mut pkgs,
                if r.is_empty() {
                    "./".to_string()
                } else {
                    format!("./{r}/")
                },
            );
        }
        if !pkgs.is_empty() {
            out.push(VerifyCommand::GoTest(pkgs));
        }
    }

    // JS/TS: the edited test file itself, or `<stem>.test.*` / `<stem>.spec.*` beside it or in a
    // sibling `__tests__/`, under the runner `package.json` declares (no runner ⇒ no rung: guessing
    // one spawns the wrong tool).
    if root.join("package.json").is_file() {
        if let Some(runner) = node_test_runner(root) {
            let mut files: Vec<String> = Vec::new();
            for p in edited
                .iter()
                .filter(|p| matches!(ext(p).as_str(), "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"))
            {
                let s = stem(p);
                let hit = if s.ends_with(".test") || s.ends_with(".spec") {
                    Some(p.clone())
                } else {
                    let dir = p.parent().unwrap_or(root);
                    let mut cands: Vec<PathBuf> = Vec::new();
                    for kind in ["test", "spec"] {
                        for e in ["ts", "tsx", "js", "jsx", "mjs"] {
                            cands.push(dir.join(format!("{s}.{kind}.{e}")));
                            cands.push(dir.join("__tests__").join(format!("{s}.{kind}.{e}")));
                            cands.push(dir.join("__tests__").join(format!("{s}.{e}")));
                        }
                    }
                    cands.into_iter().find(|c| c.is_file())
                };
                if let Some(r) = hit.as_deref().and_then(rel) {
                    push_unique(&mut files, r);
                }
            }
            if !files.is_empty() {
                out.push(VerifyCommand::NodeTest { runner, files });
            }
        }
    }

    out
}

/// Does this Rust file carry tests of its own (`#[test]`, `#[cfg(test)]`, `#[tokio::test]`)?
fn rust_file_has_tests(p: &Path) -> bool {
    std::fs::read_to_string(p)
        .map(|s| s.contains("#[test]") || s.contains("#[cfg(test)]") || s.contains("::test]"))
        .unwrap_or(false)
}

/// Does the project show pytest — a config that names it, or a requirement on it?
fn uses_pytest(root: &Path) -> bool {
    if root.join("pytest.ini").is_file() || root.join("conftest.py").is_file() {
        return true;
    }
    let has = |file: &str, needle: &str| {
        std::fs::read_to_string(root.join(file))
            .map(|s| s.contains(needle))
            .unwrap_or(false)
    };
    has("pyproject.toml", "[tool.pytest")
        || has("pyproject.toml", "pytest")
        || has("setup.cfg", "[tool:pytest]")
        || has("tox.ini", "[pytest]")
        || has("requirements.txt", "pytest")
        || has("requirements-dev.txt", "pytest")
        || has("dev-requirements.txt", "pytest")
}

/// The test runner `package.json` declares: `vitest` or `jest` among the dependencies.
fn node_test_runner(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let declared = |name: &str| {
        ["devDependencies", "dependencies"]
            .iter()
            .any(|k| json.get(k).and_then(|d| d.get(name)).is_some())
    };
    if declared("vitest") {
        Some("vitest".to_string())
    } else if declared("jest") {
        Some("jest".to_string())
    } else {
        None
    }
}

/// Does `package.json` carry a real `test` script (not npm's "no test specified" stub)?
fn npm_has_test_script(root: &Path) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|j| {
            j.get("scripts")?
                .get("test")?
                .as_str()
                .map(|s| !s.contains("no test specified"))
        })
        .unwrap_or(false)
}

/// The interpreter the Python rungs run under: `python` on Windows (where `python3` is the
/// Store stub), else `python3` when it answers `--version`, else `python`. Probed once; the
/// probe is a fixed argv, not a model- or repo-influenced spawn.
fn python_launcher() -> &'static str {
    static PY: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    PY.get_or_init(|| {
        if cfg!(windows) {
            return "python";
        }
        let ok = std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            "python3"
        } else {
            "python"
        }
    })
}

/// The project's whole test suite: `suite` in a trusted `.aizen/verify.json`, else by manifest.
/// `None` when the project shows no suite (a `package.json` with npm's stub `test` script, a
/// Python tree with neither pytest nor a `tests/` dir).
pub fn suite_command(root: &Path) -> Option<VerifyCommand> {
    if let Some(s) = custom_suite(root) {
        return Some(VerifyCommand::Suite(s));
    }
    let cmd = if root.join("Cargo.toml").is_file() {
        "cargo test -q".to_string()
    } else if root.join("package.json").is_file() {
        if !npm_has_test_script(root) {
            return None;
        }
        "npm test".to_string()
    } else if root.join("go.mod").is_file() {
        "go test ./...".to_string()
    } else if root.join("pom.xml").is_file() {
        "mvn -q test".to_string()
    } else if root.join("build.gradle").is_file() || root.join("build.gradle.kts").is_file() {
        let launcher = if root.join("gradlew.bat").is_file() && cfg!(windows) {
            "gradlew.bat"
        } else if root.join("gradlew").is_file() && !cfg!(windows) {
            "./gradlew"
        } else {
            "gradle"
        };
        format!("{launcher} -q test")
    } else if has_file_with_extension(root, &["csproj", "fsproj", "sln"]) {
        "dotnet test --nologo -v q".to_string()
    } else if [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pytest.ini",
        "requirements.txt",
    ]
    .iter()
    .any(|m| root.join(m).is_file())
    {
        let py = python_launcher();
        if uses_pytest(root) {
            format!("{py} -m pytest -q")
        } else if root.join("tests").is_dir() || root.join("test").is_dir() {
            format!("{py} -m unittest discover -q")
        } else {
            return None;
        }
    } else {
        return None;
    };
    Some(VerifyCommand::Suite(cmd))
}

/// The `suite` of a trusted `./.aizen/verify.json`, unless `cmd_guard` blocks it.
fn custom_suite(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".aizen").join("verify.json")).ok()?;
    if !crate::agent::mcp::project_trusted() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let s = v.get("suite")?.as_str()?.trim();
    if s.is_empty()
        || matches!(
            crate::agent::cmd_guard::classify(s),
            crate::agent::cmd_guard::Verdict::Blocked(_)
        )
    {
        return None;
    }
    Some(s.to_string())
}

/// What the gate has learned about a project's verify commands, kept in HOME — never in the
/// checkout, for the reason the index lives there too: a generated file must not dirty `git
/// status`. `/init` fills `commands`/`suite` and times the fast rung; every ladder run refreshes
/// `measured_ms`, so the next plan knows which rungs fit the budget.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VerifyRecord {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub suite: Option<String>,
    /// command line → last duration in ms (a timeout is recorded as budget + 1).
    #[serde(default)]
    pub measured_ms: BTreeMap<String, u64>,
}

/// `<aizen home>/verify/<dirname>-<hash>.json` for `root`.
fn record_path(root: &Path) -> PathBuf {
    let spelled = root.to_string_lossy().replace('\\', "/");
    let spelled = if cfg!(windows) {
        spelled.to_ascii_lowercase()
    } else {
        spelled
    };
    // FNV-1a over the normalized spelling: stable, dependency-free, and only ever a cache key.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in spelled.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let name: String = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    let name = if name.is_empty() {
        "project".to_string()
    } else {
        name
    };
    crate::core::config::aizen_home()
        .join("verify")
        .join(format!("{name}-{h:016x}.json"))
}

/// The record for `root`, empty when none was written yet (or it cannot be read).
pub fn load_record(root: &Path) -> VerifyRecord {
    std::fs::read_to_string(record_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the record for `root` atomically.
pub fn save_record(root: &Path, record: &VerifyRecord) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(record)?;
    crate::core::persist::atomic_write(&record_path(root), &bytes)
}

/// Remember how long `command` took under `root`. Best-effort: a record that cannot be written
/// costs nothing but the next plan's knowledge.
fn record_duration(root: &Path, command: &str, ms: u64) {
    let mut record = load_record(root);
    if record.root.is_empty() {
        record.root = root.display().to_string();
    }
    record.measured_ms.insert(command.to_string(), ms);
    let _ = save_record(root, &record);
}

/// Run the Done ladder for `edited` under `root`: every step in order, stopping at the first
/// failure (its result is the gate result); all-pass returns the last pass; `None` when nothing
/// could run. Each finished step's duration is recorded for the next plan. A suite that exceeds
/// the budget is not a failure — it is noted, recorded as over budget so it is not retried
/// blindly, and the ladder's earlier passes stand.
pub async fn run_verify_ladder(
    root: &Path,
    timeout_secs: u64,
    edited: &[PathBuf],
    quiet: bool,
) -> Option<VerifyGateResult> {
    let record = load_record(root);
    let ladder = plan_ladder(root, edited, timeout_secs, &record.measured_ms);
    if !quiet {
        for note in &ladder.skipped {
            crate::ui::tui::verify_line("verify", note);
        }
    }
    if ladder.steps.is_empty() {
        return None;
    }
    let baseline = source_fingerprint(root);
    let custom_timeout = custom_verify_timeout(root);
    let mut last_pass: Option<VerifyGateResult> = None;
    for step in ladder.steps {
        if crate::core::cancel::current().is_some_and(|c| c.is_cancelled()) {
            return None;
        }
        // A pass that is about to be superseded by the next rung is reported here; the loop
        // reports the final result itself.
        if let (false, Some(prev)) = (quiet, &last_pass) {
            crate::ui::tui::verify_line(
                &prev.command,
                &format!("passed ({} ms)", prev.duration_ms),
            );
        }
        let secs = match step.cmd {
            VerifyCommand::Custom(_) | VerifyCommand::Suite(_) => {
                custom_timeout.unwrap_or(timeout_secs)
            }
            _ => timeout_secs,
        };
        let Some(mut r) = run_one_verify(root, &step.cmd, secs).await else {
            continue;
        };
        let timed_out = r.output.starts_with(TIMED_OUT_PREFIX);
        record_duration(
            root,
            &r.command,
            if timed_out {
                secs.saturating_mul(1000) + 1
            } else {
                r.duration_ms as u64
            },
        );
        r.stable = baseline.is_some() && baseline == source_fingerprint(root);
        if !r.stable {
            r.passed = false;
            r.output = "verification result ignored: the source workspace changed while the command was running. No success was reported; retry after the other writer finishes".to_string();
            return Some(r);
        }
        if !r.passed {
            if step.rung == Rung::FullSuite && timed_out {
                if !quiet {
                    crate::ui::tui::verify_line(
                        &r.command,
                        &format!(
                            "full suite exceeded the {secs} s verify budget — skipped; it will not be retried automatically"
                        ),
                    );
                }
                continue;
            }
            return Some(r);
        }
        last_pass = Some(r);
    }
    last_pass
}

/// `/init`'s share of verification: detect the commands and the suite, time the fast rung once,
/// and keep the result in HOME. Returns the lines to show the user.
pub async fn init_verify(root: &Path, timeout_secs: u64) -> Vec<String> {
    let cmds = detect_verify_commands(root);
    let suite = suite_command(root);
    let mut record = load_record(root);
    record.root = root.display().to_string();
    record.commands = cmds.iter().map(|c| c.command_line()).collect();
    record.suite = suite.as_ref().map(|s| s.command_line());
    let mut lines = Vec::new();
    if cmds.is_empty() && suite.is_none() {
        lines.push(
            "verify: no build or test command recognised here — a trusted .aizen/verify.json with \
             {\"commands\": [...], \"suite\": \"...\"} names them"
                .to_string(),
        );
        let _ = save_record(root, &record);
        return lines;
    }
    for cmd in &cmds {
        match run_one_verify(root, cmd, timeout_secs).await {
            Some(r) => {
                let timed_out = r.output.starts_with(TIMED_OUT_PREFIX);
                record.measured_ms.insert(
                    r.command.clone(),
                    if timed_out {
                        timeout_secs.saturating_mul(1000) + 1
                    } else {
                        r.duration_ms as u64
                    },
                );
                lines.push(format!(
                    "verify: `{}` {} in {:.1} s",
                    r.command,
                    if r.passed {
                        "passed"
                    } else if timed_out {
                        "timed out"
                    } else {
                        "FAILED (pre-existing; the gate compares against this baseline)"
                    },
                    r.duration_ms as f64 / 1000.0
                ));
            }
            None => lines.push(format!(
                "verify: `{}` could not run here (toolchain not installed?)",
                cmd.command_line()
            )),
        }
    }
    if let Some(s) = &suite {
        lines.push(format!(
            "verify: suite `{}` runs at Done on multi-file changes — timed on first use, skipped once it is known to exceed the {timeout_secs} s budget",
            s.command_line()
        ));
    }
    let _ = save_record(root, &record);
    lines
}

/// Parse `package.json` and return the first typecheck-flavored script that exists.
/// Best-effort: a missing/invalid file or absent `scripts` → `None` (no panic).
fn detect_npm_typecheck_script(pkg: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let scripts = json.get("scripts")?.as_object()?;
    ["typecheck", "type-check", "tsc"]
        .into_iter()
        .find(|c| scripts.contains_key(*c))
        .map(String::from)
}

/// Build the sandboxed shell command for one verify run (same platform-shell wrapping as
/// `shell_run`, via the central runner — the verify gate must not be a sandbox bypass: its command
/// list is repository-influenced through manifests and `.aizen/verify.json`).
fn sandboxed_verify(
    cwd: &Path,
    command_line: &str,
    timeout_secs: u64,
) -> Option<crate::sandbox::runner::Sandboxed<Command>> {
    crate::sandbox::runner::prepare_tokio(
        crate::sandbox::request::SandboxRequest::shell(
            crate::sandbox::CommandOrigin::VerifyGate,
            command_line,
            cwd.to_path_buf(),
            cwd.to_path_buf(),
        )
        .wall_timeout(Duration::from_secs(timeout_secs.max(1))),
    )
    .ok() // a policy refusal degrades to "no verify ran" — the gate is best-effort by contract
}

/// Run the project's verify command LIST in order, stopping at the first failure (its result is
/// the gate result); all-pass returns the last pass. Returns `None` when there is nothing to run
/// (unknown project) or nothing could even be spawned (best-effort no-op).
pub async fn run_verify_gate(cwd: &Path, timeout_secs: u64) -> Option<VerifyGateResult> {
    let cmds = detect_verify_commands(cwd);
    if cmds.is_empty() {
        return None;
    }
    let baseline = source_fingerprint(cwd);
    let custom_timeout = custom_verify_timeout(cwd);
    let mut last_pass: Option<VerifyGateResult> = None;
    for cmd in cmds {
        // The gate can hold the turn for minutes per command; it must not be the one region Esc
        // cannot reach. Between commands is the cheap, safe boundary — `run_one_verify` also
        // races its wait against this token.
        if crate::core::cancel::current().is_some_and(|c| c.is_cancelled()) {
            return None; // best-effort by contract; the loop's own cancel check ends the run
        }
        let secs = match cmd {
            VerifyCommand::Custom(_) => custom_timeout.unwrap_or(timeout_secs),
            _ => timeout_secs,
        };
        match run_one_verify(cwd, &cmd, secs).await {
            None => continue,
            Some(mut r) => {
                r.stable = baseline.is_some() && baseline == source_fingerprint(cwd);
                if !r.stable {
                    r.passed = false;
                    r.output = "verification result ignored: the source workspace changed while the command was running. No success was reported; retry after the other writer finishes".to_string();
                    return Some(r);
                }
                if !r.passed {
                    return Some(r);
                }
                last_pass = Some(r);
            }
        }
    }
    last_pass
}

/// Run ONE verify command in `cwd` with a wall-clock timeout.
async fn run_one_verify(
    cwd: &Path,
    cmd: &VerifyCommand,
    timeout_secs: u64,
) -> Option<VerifyGateResult> {
    let command_line = cmd.command_line();
    let start = Instant::now();

    let mut sbx = sandboxed_verify(cwd, &command_line, timeout_secs)?;
    sbx.command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true); // a dropped (timed-out) future kills the child.
                             // `kill_on_drop` alone reaches only the `cmd.exe`/`sh` wrapper. The verify command is a BUILD
                             // (`cargo check`, `tsc`, `npm test`), so the surviving grandchild is a compiler holding a lock on
                             // `target/` — the next verify then blocks on that lock and times out too, and the timeouts
                             // compound instead of clearing. Containment makes the timeout actually stop the work.

    let child = match sbx.command.spawn() {
        Ok(c) => c,
        Err(_) => {
            sbx.finish(crate::sandbox::runner::Outcome::SpawnFailed);
            return None; // spawn failure (no toolchain) → silent no-op.
        }
    };
    let containment = sbx.contain_tokio(&child);
    let dur = Duration::from_secs(timeout_secs.max(1));
    // `wait_with_output` consumes the child, so the tree is killed through the containment handle
    // rather than a `&mut Child` we no longer have. The wait is ALSO raced against the turn's
    // cancel token: a verify command can hold the turn for minutes, and this used to be the one
    // long region Esc could not reach — the user watched a `cargo check` they had already
    // abandoned run to completion.
    let cancel = crate::core::cancel::current();
    let waited = timeout(dur, child.wait_with_output());
    let outcome = match &cancel {
        Some(c) => {
            tokio::select! {
                o = waited => Some(o),
                _ = c.cancelled() => None,
            }
        }
        None => Some(waited.await),
    };
    let Some(outcome) = outcome else {
        crate::core::proctree::terminate_tree(&containment);
        sbx.finish(crate::sandbox::runner::Outcome::Cancelled);
        return None; // best-effort: the loop's own cancel check ends the run right after
    };
    if outcome.is_err() {
        crate::core::proctree::terminate_tree(&containment);
    }
    sbx.finish(match &outcome {
        Ok(Ok(output)) => crate::sandbox::runner::Outcome::Exit(output.status.code()),
        Ok(Err(_)) => crate::sandbox::runner::Outcome::SpawnFailed,
        Err(_) => crate::sandbox::runner::Outcome::Timeout,
    });
    match outcome {
        Ok(Ok(output)) => {
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            // A toolchain that is not installed is "nothing ran", not "the check failed": the
            // shell wrapper turns a missing `cargo`/`go`/`python` into exit 127 with a one-line
            // message, and reporting that as a verify FAILURE sent the model chasing a build
            // error that does not exist.
            if !output.status.success() && looks_like_missing_tool(output.status.code(), &combined)
            {
                return None;
            }
            Some(VerifyGateResult {
                passed: output.status.success(),
                command: command_line,
                output: tail_chars(combined.trim_end(), MAX_OUTPUT_CHARS),
                duration_ms: start.elapsed().as_millis(),
                stable: true,
            })
        }
        Ok(Err(_)) => None, // io error draining output → no-op.
        Err(_) => Some(VerifyGateResult {
            passed: false,
            command: command_line,
            output: format!("{TIMED_OUT_PREFIX}{timeout_secs}s (killed)"),
            duration_ms: start.elapsed().as_millis(),
            stable: true,
        }),
    }
}

fn source_fingerprint(cwd: &Path) -> Option<u64> {
    let mut files = Vec::new();
    collect_source_files(cwd, &mut files, 0);
    files.sort();
    if files.is_empty() {
        return None;
    }
    let mut data = Vec::new();
    for path in files {
        data.extend_from_slice(path.to_string_lossy().as_bytes());
        data.push(0);
        data.extend_from_slice(&std::fs::read(&path).ok()?);
        data.push(0xff);
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &data);
    Some(u64::from_le_bytes(digest.as_ref()[..8].try_into().ok()?))
}

fn collect_source_files(dir: &Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == ".git" || name == "target" || name == "node_modules" || name == ".aizen" {
            continue;
        }
        if path.is_dir() {
            collect_source_files(&path, out, depth + 1);
        } else if path.is_file() && is_source_or_manifest(&path) {
            out.push(path);
        }
    }
}

fn is_source_or_manifest(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "toml"
                | "lock"
                | "json"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "go"
                | "java"
                | "cs"
                | "cpp"
                | "h"
                | "hpp"
        )
    )
}

/// output is SHAPED first: deduped error blocks, capped counts — a 400-line wall of repeated
/// errors buys nothing but tokens.
pub fn format_gate_failure(r: &VerifyGateResult) -> String {
    if !r.stable {
        return format!(
            "[aizen verify] `{}` was discarded because the source workspace changed while it ran. {}",
            r.command, r.output
        );
    }
    let mut msg = format!(
        "[aizen verify] `{}` FAILED ({} ms). Fix these errors before reporting the task done:\n\n{}",
        r.command,
        r.duration_ms,
        shape_failure_output(&r.output)
    );
    if let Some(hint) = crate::features::timemachine::recovery_hint() {
        msg.push_str("\n\n");
        msg.push_str(&hint);
        msg.push_str(
            " Prefer a surgical fix when the error is local; rewind when the approach itself is wrong \
             (wrong design, cascading breakage). After a rewind, re-read files — disk contents changed.",
        );
    }
    msg
}

/// Max distinct error blocks/rows surfaced to the model (the rest are counted, not quoted).
const MAX_ERRORS: usize = 10;
/// Hard cap on the shaped text.
const MAX_SHAPED_CHARS: usize = 3_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    pub key: String,
    pub rendered: String,
}

/// Immutable pre-edit compiler state. `diagnostics=None` means the failing output was not a shape we
/// can compare safely; callers must keep the old conservative raw pass/fail policy.
#[derive(Debug, Clone)]
pub struct VerifyBaseline {
    pub command: String,
    pub passed: bool,
    pub diagnostics: Option<Vec<Diagnostic>>,
}

#[derive(Debug, Clone)]
pub struct VerifyDelta {
    pub passed: bool,
    #[allow(dead_code)] // reported to parent via `note`; field kept for future diagnostic tooling
    pub preexisting: usize,
    pub new_diagnostics: Vec<Diagnostic>,
    pub note: Option<String>,
}

/// Extract stable diagnostics for baseline comparison. TSC keys ignore line/column movement and use
/// normalized path + error code + whitespace-normalized message. Cargo keys use the error header plus
/// the first primary `--> path:line:col` location in its block when present.
pub fn parse_diagnostics(raw: &str) -> Option<Vec<Diagnostic>> {
    parse_tsc_diagnostics(raw).or_else(|| parse_cargo_diagnostics(raw))
}

fn normalize_diag_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_diag_path(s: &str) -> String {
    s.trim().replace('\\', "/").to_ascii_lowercase()
}

fn parse_tsc_diagnostics(raw: &str) -> Option<Vec<Diagnostic>> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?m)^(.+)\((\d+),(\d+)\): error (TS\d+): (.*)$").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in RE.captures_iter(raw) {
        let path = normalize_diag_path(&c[1]);
        let message = normalize_diag_text(&c[5]);
        let key = format!("tsc|{path}|{}|{message}", &c[4]);
        if seen.insert(key.clone()) {
            out.push(Diagnostic {
                key,
                rendered: format!("{} {}:{}  {} {}", &c[1], &c[2], &c[3], &c[4], &c[5]),
            });
        }
    }
    (!out.is_empty()).then_some(out)
}

fn parse_cargo_diagnostics(raw: &str) -> Option<Vec<Diagnostic>> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static LOC: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*-->\s+(.+?):\d+:\d+\s*$").unwrap());
    let lines: Vec<&str> = raw.lines().collect();
    let is_start = |l: &str| l.starts_with("error[") || l.starts_with("error:");
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0usize;
    while i < lines.len() {
        if !is_start(lines[i]) {
            i += 1;
            continue;
        }
        let header = normalize_diag_text(lines[i]);
        let mut loc = String::new();
        let mut block = vec![lines[i]];
        let mut j = i + 1;
        while j < lines.len() && !is_start(lines[j]) {
            if loc.is_empty() {
                if let Some(c) = LOC.captures(lines[j]) {
                    loc = normalize_diag_path(&c[1]);
                }
            }
            if !lines[j].trim().is_empty() && block.len() < 6 {
                block.push(lines[j]);
            }
            j += 1;
        }
        let key = format!("cargo|{loc}|{header}");
        if seen.insert(key.clone()) {
            out.push(Diagnostic {
                key,
                rendered: block.join("\n"),
            });
        }
        i = j;
    }
    (!out.is_empty()).then_some(out)
}

pub fn baseline_from_result(result: &VerifyGateResult) -> VerifyBaseline {
    VerifyBaseline {
        command: result.command.clone(),
        passed: result.passed,
        diagnostics: (!result.passed)
            .then(|| parse_diagnostics(&result.output))
            .flatten(),
    }
}

/// Compare the current compiler state to the immutable pre-edit baseline.
///
/// A red baseline with no new diagnostic is accepted: the task did not regress the project. A clean
/// baseline, unparseable failing output, command mismatch, timeout, or unstable result stays
/// conservative and requires a clean exit.
pub fn compare_to_baseline(baseline: &VerifyBaseline, current: &VerifyGateResult) -> VerifyDelta {
    if current.passed {
        return VerifyDelta {
            passed: true,
            preexisting: baseline.diagnostics.as_ref().map_or(0, Vec::len),
            new_diagnostics: Vec::new(),
            note: None,
        };
    }
    if !current.stable || baseline.command != current.command || baseline.passed {
        return VerifyDelta {
            passed: false,
            preexisting: 0,
            new_diagnostics: parse_diagnostics(&current.output).unwrap_or_default(),
            note: None,
        };
    }
    let (Some(before), Some(after)) = (
        baseline.diagnostics.as_ref(),
        parse_diagnostics(&current.output),
    ) else {
        return VerifyDelta {
            passed: false,
            preexisting: 0,
            new_diagnostics: Vec::new(),
            note: None,
        };
    };
    let old: std::collections::HashSet<&str> = before.iter().map(|d| d.key.as_str()).collect();
    let new_diagnostics: Vec<Diagnostic> = after
        .into_iter()
        .filter(|d| !old.contains(d.key.as_str()))
        .collect();
    let passed = new_diagnostics.is_empty();
    VerifyDelta {
        passed,
        preexisting: before.len(),
        note: passed.then(|| {
            format!(
                "verification remains red with {} pre-existing diagnostic(s), 0 new",
                before.len()
            )
        }),
        new_diagnostics,
    }
}

pub fn format_delta_failure(result: &VerifyGateResult, delta: &VerifyDelta) -> String {
    if delta.new_diagnostics.is_empty() {
        return format_gate_failure(result);
    }
    let total = delta.new_diagnostics.len();
    let shown = total.min(MAX_ERRORS);
    let mut body = delta.new_diagnostics[..shown]
        .iter()
        .map(|d| d.rendered.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if total > shown {
        body.push_str(&format!(
            "\n(+{} more NEW error(s) suppressed)",
            total - shown
        ));
    }
    format!(
        "[aizen verify] `{}` introduced {} NEW diagnostic(s) ({} ms). Fix only these regressions before reporting the task done:\n\n{}",
        result.command, total, result.duration_ms, head_chars(&body, MAX_SHAPED_CHARS)
    )
}

/// Shape a failing verify output: cargo-style error blocks (deduped by header, ≤5 lines each) or
/// tsc-style error rows (deduped), capped at [`MAX_ERRORS`] with a suppressed-count note; an
/// unrecognized shape falls back to the raw tail (behavior-preserving floor).
pub fn shape_failure_output(raw: &str) -> String {
    let shaped = shape_cargo(raw).or_else(|| shape_tsc(raw));
    match shaped {
        Some(s) => head_chars(&s, MAX_SHAPED_CHARS),
        None => tail_chars(raw, MAX_OUTPUT_CHARS),
    }
}

/// Cargo/rustc shape: blocks starting `error[...]` / `error:`, header + up to 5 detail lines,
/// deduped by header (the same error at N call sites collapses to one block).
fn shape_cargo(raw: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let is_err_start = |l: &str| l.starts_with("error[") || l.starts_with("error:");
    let mut blocks: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut warnings = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("warning:") {
            warnings += 1;
        }
        if is_err_start(l) {
            let mut block: Vec<&str> = vec![l];
            let mut j = i + 1;
            while j < lines.len()
                && block.len() < 6
                && !is_err_start(lines[j])
                && !lines[j].starts_with("warning:")
            {
                if !lines[j].trim().is_empty() {
                    block.push(lines[j]);
                }
                j += 1;
            }
            if seen.insert(l.to_string()) {
                blocks.push(block.join("\n"));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    if blocks.is_empty() {
        return None;
    }
    let total = blocks.len();
    let shown = total.min(MAX_ERRORS);
    let mut out = blocks[..shown].join("\n");
    let mut extras: Vec<String> = Vec::new();
    if total > shown {
        extras.push(format!("+{} more error(s)", total - shown));
    }
    if warnings > 0 {
        extras.push(format!("{warnings} warning(s)"));
    }
    if !extras.is_empty() {
        out.push_str(&format!("\n({} suppressed)", extras.join(", ")));
    }
    Some(out)
}

/// tsc/npm shape: `file(line,col): error TSxxxx: message` rows, deduped by (file, code, message).
fn shape_tsc(raw: &str) -> Option<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?m)^(.+)\((\d+),(\d+)\): error (TS\d+): (.*)$").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<String> = Vec::new();
    for c in RE.captures_iter(raw) {
        if seen.insert(format!("{}|{}|{}", &c[1], &c[4], &c[5])) {
            rows.push(format!(
                "{} {}:{}  {} {}",
                &c[1], &c[2], &c[3], &c[4], &c[5]
            ));
        }
    }
    if rows.is_empty() {
        return None;
    }
    let total = rows.len();
    let shown = total.min(MAX_ERRORS);
    let mut out = rows[..shown].join("\n");
    if total > shown {
        out.push_str(&format!("\n(+{} more error(s) suppressed)", total - shown));
    }
    Some(out)
}

/// Keep the first `max` chars (shaped output leads with the errors), marking the elision.
fn head_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n…[{} chars truncated]…", n - max)
}

/// Keep the last `max` chars, marking the elision (errors are at the tail).
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max;
    let tail: String = s.chars().skip(skip).collect();
    format!("…[{skip} chars truncated]…\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aizen-verify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_cargo() {
        let d = temp_dir("cargo");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Cargo));
        assert_eq!(VerifyCommand::Cargo.command_line(), "cargo check");
    }

    #[test]
    fn detects_go_jvm_dotnet_and_python_manifests() {
        let d = temp_dir("go");
        std::fs::write(d.join("go.mod"), "module x\n").unwrap();
        assert_eq!(
            detect_builtin_verify_commands(&d),
            vec![VerifyCommand::GoBuild, VerifyCommand::GoVet],
            "build first, vet after a clean build"
        );

        let d = temp_dir("maven");
        std::fs::write(d.join("pom.xml"), "<project/>").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Maven));

        let d = temp_dir("gradle");
        std::fs::write(d.join("build.gradle.kts"), "plugins {}").unwrap();
        assert!(matches!(
            detect_verify_command(&d),
            Some(VerifyCommand::Gradle(l)) if l == "gradle"
        ));

        let d = temp_dir("dotnet");
        std::fs::write(d.join("App.csproj"), "<Project/>").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::DotNet));
        assert!(verify_root(&d.join("src")).is_none() || verify_root(&d).is_some());

        let d = temp_dir("python");
        std::fs::write(d.join("pyproject.toml"), "[project]\nname='x'").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Python));
        let line = VerifyCommand::Python.command_line();
        assert!(
            line.starts_with("python3 -m compileall") && line.contains("|| python -m compileall")
        );

        let d = temp_dir("nothing");
        std::fs::write(d.join("README.md"), "hi").unwrap();
        assert!(detect_builtin_verify_commands(&d).is_empty());
    }

    #[test]
    fn missing_toolchain_is_a_skip_not_a_failure() {
        assert!(looks_like_missing_tool(
            Some(127),
            "sh: 1: cargo: not found"
        ));
        assert!(looks_like_missing_tool(
            Some(1),
            "'go' is not recognized as an internal or external command,\noperable program or batch file."
        ));
        assert!(looks_like_missing_tool(
            Some(1),
            "/usr/bin/python3: No module named compileall"
        ));
        // The first half of `python3 … || python …` missing but the second finding real errors:
        // the tail is the errors, so this must stay a FAILURE.
        assert!(!looks_like_missing_tool(
            Some(1),
            "sh: python3: command not found\n  File \"x.py\", line 3\n    def (\nSyntaxError: invalid syntax"
        ));
        assert!(!looks_like_missing_tool(
            Some(101),
            "error[E0599]: no method named `frob`"
        ));
        assert!(!looks_like_missing_tool(
            Some(0),
            "sh: python3: command not found"
        ));
    }

    #[test]
    fn cargo_takes_precedence_over_npm() {
        let d = temp_dir("both");
        std::fs::write(d.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"typecheck":"tsc"}}"#).unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Cargo));
    }

    #[test]
    fn detects_npm_script_in_priority_order() {
        let d = temp_dir("npm");
        // both type-check and typecheck present → typecheck wins (higher priority).
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"build":"x","type-check":"tsc","typecheck":"tsc --noEmit"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_verify_command(&d),
            Some(VerifyCommand::Npm("typecheck".into()))
        );
        assert_eq!(
            VerifyCommand::Npm("typecheck".into()).command_line(),
            "npm run typecheck"
        );
    }

    #[test]
    fn falls_back_to_npx_tsc() {
        let d = temp_dir("npx");
        // package.json with no typecheck script + a tsconfig → npx tsc --noEmit.
        std::fs::write(d.join("package.json"), r#"{"scripts":{"build":"x"}}"#).unwrap();
        std::fs::write(d.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::NpxTsc));
        assert_eq!(VerifyCommand::NpxTsc.command_line(), "npx tsc --noEmit");
    }

    #[test]
    fn no_recognized_project_is_none() {
        let d = temp_dir("none");
        std::fs::write(d.join("readme.txt"), "hi").unwrap();
        assert_eq!(detect_verify_command(&d), None);
    }

    #[test]
    fn verify_root_walks_up_from_a_subdir_to_the_manifest() {
        // The edit landed in `proj/src/` but `Cargo.toml` sits in `proj/` — the gate must climb.
        let root = temp_dir("walkup");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        let src = root.join("src").join("nested");
        std::fs::create_dir_all(&src).unwrap();
        // Canonicalize both sides: temp_dir may sit behind a symlink (e.g. macOS /var → /private).
        assert_eq!(
            verify_root(&src).map(|p| p.canonicalize().unwrap()),
            Some(root.canonicalize().unwrap())
        );
        // A file argument begins at its parent dir and still finds the root.
        let file = src.join("x.rs");
        std::fs::write(&file, "// x").unwrap();
        assert_eq!(
            verify_root(&file).map(|p| p.canonicalize().unwrap()),
            Some(root.canonicalize().unwrap())
        );
    }

    #[test]
    fn verify_root_is_none_when_no_ancestor_has_a_manifest() {
        // `verify_root` walks to the filesystem root, so this cannot be asserted against a real temp
        // dir: on Windows the temp dir lives UNDER the user profile, and one stray `package.json` in
        // `~` (a mislanded `npm install` will do it) makes every such walk succeed. The test would
        // then fail for a reason that has nothing to do with the code — as it did here.
        //
        // Assert the contract on a tree we fully own instead: `start` and every ancestor up to a
        // directory we created, with no manifest anywhere in it. That still exercises the climb and
        // the `parent()?` termination, without depending on what sits in the user's home.
        let d = temp_dir("walkup-none");
        let sub = d.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        // No manifest between `sub` and `d`: every level in between must decline to claim the root.
        for level in [sub.as_path(), sub.parent().unwrap()] {
            let found = verify_root(level);
            assert!(
                found.as_deref() != Some(level),
                "a manifest-less dir must not claim itself as the verify root: {}",
                level.display()
            );
        }
        // And a manifest placed at the top of our own tree is what the walk should find — proving the
        // climb works, which is the half of the contract that can be tested hermetically.
        std::fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(
            verify_root(&sub).map(|p| p.canonicalize().unwrap()),
            Some(d.canonicalize().unwrap()),
            "the nearest manifest-bearing ancestor wins"
        );
    }

    #[test]
    fn invalid_package_json_degrades_to_none() {
        let d = temp_dir("badpkg");
        std::fs::write(d.join("package.json"), "{not json").unwrap();
        // no Cargo.toml, no tsconfig, unparseable package.json → None (no panic).
        assert_eq!(detect_verify_command(&d), None);
    }

    #[test]
    fn tail_chars_keeps_tail_and_marks_elision() {
        let s = "A".repeat(50) + &"B".repeat(50);
        let t = tail_chars(&s, 30);
        assert!(t.ends_with(&"B".repeat(30)));
        assert!(t.contains("truncated"));
        assert_eq!(tail_chars("short", 4000), "short");
    }

    #[test]
    fn shapes_cargo_errors_deduped_and_capped() {
        // 3 error blocks, one duplicated header, plus warnings — dedup + count, keep ≤5 lines each.
        let raw = "\
warning: unused variable: `x`
error[E0308]: mismatched types
 --> src/a.rs:10:5
  = note: expected `u32`
error[E0308]: mismatched types
 --> src/a.rs:10:5
error[E0425]: cannot find value `foo`
 --> src/b.rs:2:1
warning: dead code
";
        let s = shape_failure_output(raw);
        assert_eq!(
            s.matches("error[E0308]").count(),
            1,
            "duplicate header deduped: {s}"
        );
        assert!(s.contains("error[E0425]"), "{s}");
        assert!(
            s.contains("--> src/a.rs:10:5"),
            "the location line survives: {s}"
        );
        assert!(
            s.contains("2 warning(s)") && s.contains("suppressed"),
            "{s}"
        );
    }

    #[test]
    fn shapes_tsc_error_rows() {
        let raw = "\
src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/lib.ts(3,1): error TS2304: Cannot find name 'foo'.
";
        let s = shape_failure_output(raw);
        assert_eq!(s.matches("TS2322").count(), 1, "duplicate row deduped: {s}");
        assert!(s.contains("src/lib.ts 3:1"), "{s}");
    }

    #[test]
    fn unknown_shape_falls_back_to_tail() {
        let raw = format!("{}THE REAL FAILURE AT THE END", "noise\n".repeat(2000));
        let s = shape_failure_output(&raw);
        assert!(
            s.contains("THE REAL FAILURE AT THE END"),
            "tail preserved: …{}",
            &s[s.len().saturating_sub(80)..]
        );
        assert!(s.contains("truncated"));
    }

    #[test]
    fn custom_verify_requires_trust_gate() {
        // An untrusted repo's verify.json must be INERT. Sandbox AIZEN_HOME so the developer's
        // real trust store (which may trust THIS repo) can't leak into the assertion.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_dir("custom-untrusted-home");
        std::env::set_var("AIZEN_HOME", &home);
        let d = temp_dir("custom-untrusted");
        std::fs::create_dir_all(d.join(".aizen")).unwrap();
        std::fs::write(
            d.join(".aizen").join("verify.json"),
            r#"{"commands":["echo pwned"]}"#,
        )
        .unwrap();
        std::fs::write(d.join("Cargo.toml"), "[package]").unwrap();
        let cmds = detect_verify_commands(&d);
        std::env::remove_var("AIZEN_HOME");
        assert_eq!(
            cmds,
            vec![VerifyCommand::Cargo],
            "untrusted verify.json ignored: {cmds:?}"
        );
    }

    #[test]
    fn format_failure_includes_command_and_output() {
        let r = VerifyGateResult {
            passed: false,
            command: "cargo check".into(),
            output: "error[E0308]: mismatched types".into(),
            duration_ms: 1234,
            stable: true,
        };
        let msg = format_gate_failure(&r);
        assert!(msg.contains("cargo check"));
        assert!(msg.contains("E0308"));
        assert!(msg.contains("Fix these errors"));
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// The done-when of E0.7: a Rust edit with tests of its own runs `cargo test -- module::`,
    /// not the whole suite; a file without tests names no rung; `main`/`lib` never do.
    #[test]
    fn narrow_rust_edit_runs_its_module_not_the_suite() {
        let d = temp_dir("narrow-rust");
        write(&d.join("Cargo.toml"), "[package]\nname = \"x\"\n");
        write(
            &d.join("src/lib.rs"),
            "pub mod agent;\n#[cfg(test)]\nmod t {}\n",
        );
        write(
            &d.join("src/agent/verify_gate.rs"),
            "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
        );
        write(&d.join("src/agent/plain.rs"), "pub fn g() {}\n");
        write(&d.join("tests/e2e.rs"), "#[test]\nfn it_works() {}\n");
        let edited = vec![
            d.join("src/agent/verify_gate.rs"),
            d.join("src/agent/plain.rs"),
            d.join("src/lib.rs"),
        ];
        let cmds = narrow_tests(&d, &edited);
        assert_eq!(
            cmds,
            vec![VerifyCommand::CargoTest {
                filters: vec!["verify_gate::".into()],
                targets: vec![],
            }],
            "{cmds:?}"
        );
        assert_eq!(cmds[0].command_line(), "cargo test -q -- verify_gate::");
        let cmds = narrow_tests(&d, &[d.join("tests/e2e.rs")]);
        assert_eq!(cmds[0].command_line(), "cargo test -q --test e2e");

        // One edited file: no suite. Two: the suite, unless it is known to blow the budget.
        let one = plan_ladder(&d, &edited[..1], 90, &BTreeMap::new());
        assert!(
            one.steps.iter().all(|s| s.rung != Rung::FullSuite),
            "{one:?}"
        );
        assert_eq!(one.steps[0].cmd, VerifyCommand::Cargo, "typecheck first");
        let two = plan_ladder(&d, &edited[..2], 90, &BTreeMap::new());
        assert_eq!(
            two.steps.last().map(|s| (s.rung, s.cmd.command_line())),
            Some((Rung::FullSuite, "cargo test -q".to_string()))
        );
        let mut slow = BTreeMap::new();
        slow.insert("cargo test -q".to_string(), 200_000u64);
        let capped = plan_ladder(&d, &edited[..2], 90, &slow);
        assert!(capped.steps.iter().all(|s| s.rung != Rung::FullSuite));
        assert_eq!(capped.skipped.len(), 1, "{capped:?}");
        assert!(capped.skipped[0].contains("200 s"), "{}", capped.skipped[0]);
    }

    #[test]
    fn narrow_python_edit_finds_its_sibling_test_under_the_projects_runner() {
        let d = temp_dir("narrow-py");
        write(&d.join("requirements.txt"), "");
        write(&d.join("pkg/calc.py"), "def add(a, b):\n    return a + b\n");
        write(&d.join("tests/test_calc.py"), "def test_add():\n    pass\n");
        write(&d.join("pkg/alone.py"), "x = 1\n");
        let edited = vec![d.join("pkg/calc.py"), d.join("pkg/alone.py")];
        let cmds = narrow_tests(&d, &edited);
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        match &cmds[0] {
            VerifyCommand::Unittest { files, .. } => {
                assert_eq!(files, &vec!["tests/test_calc.py".to_string()])
            }
            other => panic!("unittest without a pytest signal, got {other:?}"),
        }
        write(&d.join("pytest.ini"), "[pytest]\n");
        let cmds = narrow_tests(&d, &edited);
        assert!(
            matches!(&cmds[0], VerifyCommand::Pytest { files, .. } if files == &vec!["tests/test_calc.py".to_string()]),
            "{cmds:?}"
        );
        assert!(cmds[0]
            .command_line()
            .contains("-m pytest -q tests/test_calc.py"));
        // An edited test file is its own narrow rung.
        let cmds = narrow_tests(&d, &[d.join("tests/test_calc.py")]);
        assert!(cmds[0].command_line().ends_with("tests/test_calc.py"));
    }

    #[test]
    fn narrow_go_and_node_edits_name_their_package_and_sibling() {
        let d = temp_dir("narrow-go");
        write(&d.join("go.mod"), "module x\n");
        write(&d.join("pkg/x.go"), "package pkg\n");
        write(&d.join("pkg/x_test.go"), "package pkg\n");
        write(&d.join("other/y.go"), "package other\n");
        let cmds = narrow_tests(&d, &[d.join("pkg/x.go"), d.join("other/y.go")]);
        assert_eq!(cmds, vec![VerifyCommand::GoTest(vec!["./pkg/".into()])]);
        assert_eq!(cmds[0].command_line(), "go test ./pkg/");

        let n = temp_dir("narrow-node");
        write(&n.join("src/a.ts"), "export const a = 1;\n");
        write(&n.join("src/a.test.ts"), "test('a', () => {});\n");
        write(&n.join("package.json"), r#"{"scripts":{"test":"vitest"}}"#);
        assert!(
            narrow_tests(&n, &[n.join("src/a.ts")]).is_empty(),
            "no declared runner ⇒ no rung"
        );
        write(
            &n.join("package.json"),
            r#"{"scripts":{"test":"vitest"},"devDependencies":{"vitest":"^2"}}"#,
        );
        let cmds = narrow_tests(&n, &[n.join("src/a.ts")]);
        assert_eq!(
            cmds,
            vec![VerifyCommand::NodeTest {
                runner: "vitest".into(),
                files: vec!["src/a.test.ts".into()],
            }]
        );
        assert_eq!(cmds[0].command_line(), "npx vitest run src/a.test.ts");
    }

    #[test]
    fn suite_is_detected_per_manifest_and_absent_for_npm_stub() {
        let d = temp_dir("suite-cargo");
        write(&d.join("Cargo.toml"), "[package]\n");
        assert_eq!(
            suite_command(&d).map(|c| c.command_line()),
            Some("cargo test -q".into())
        );
        let n = temp_dir("suite-npm");
        write(
            &n.join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        assert_eq!(suite_command(&n), None, "npm's stub is not a suite");
        write(
            &n.join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        );
        assert_eq!(
            suite_command(&n).map(|c| c.command_line()),
            Some("npm test".into())
        );
        let g = temp_dir("suite-go");
        write(&g.join("go.mod"), "module x\n");
        assert_eq!(
            suite_command(&g).map(|c| c.command_line()),
            Some("go test ./...".into())
        );
        let p = temp_dir("suite-py");
        write(&p.join("setup.py"), "");
        assert_eq!(suite_command(&p), None, "no pytest, no tests/ ⇒ no suite");
        write(&p.join("tests/test_a.py"), "");
        assert!(suite_command(&p)
            .map(|c| c.command_line())
            .unwrap()
            .contains("-m unittest discover -q"));
    }

    #[test]
    fn verify_record_round_trips_in_home() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_dir("record-home");
        std::env::set_var("AIZEN_HOME", &home);
        let d = temp_dir("record-proj");
        assert!(load_record(&d).measured_ms.is_empty());
        record_duration(&d, "cargo check", 1234);
        record_duration(&d, "cargo test -q", 91_001);
        let rec = load_record(&d);
        let path = record_path(&d); // resolved while the temp HOME is still set
        std::env::remove_var("AIZEN_HOME");
        assert_eq!(rec.measured_ms.get("cargo check"), Some(&1234));
        assert_eq!(rec.measured_ms.get("cargo test -q"), Some(&91_001));
        assert_eq!(rec.root, d.display().to_string());
        assert!(
            path.starts_with(home.join("verify")),
            "the record lives in HOME, never in the checkout"
        );
    }

    /// The done-when of E0.7: a Python edit whose sibling test fails cannot pass the ladder. The
    /// fixture uses unittest (pytest is not a given on a dev machine); an interpreter that cannot
    /// run at all skips the assertion rather than faking it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_python_edit_with_a_failing_sibling_test_cannot_pass_the_ladder() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_dir("ladder-py-home");
        std::env::set_var("AIZEN_HOME", &home);
        let d = temp_dir("ladder-py");
        write(&d.join("requirements.txt"), "");
        write(&d.join("calc.py"), "def add(a, b):\n    return a - b\n");
        write(&d.join("tests/__init__.py"), "");
        write(
            &d.join("tests/test_calc.py"),
            "import os\nimport sys\nimport unittest\n\nsys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))\nfrom calc import add\n\n\nclass CalcTest(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(1, 2), 3)\n",
        );
        let edited = vec![d.join("calc.py")];
        let result = run_verify_ladder(&d, 60, &edited, true).await;
        let recorded = load_record(&d);
        std::env::remove_var("AIZEN_HOME");
        let Some(r) = result else {
            eprintln!("skipped: no Python interpreter could run here");
            return;
        };
        assert!(
            r.command.contains("-m unittest"),
            "the narrow rung ran and is the gate result: {}",
            r.command
        );
        assert!(
            !r.passed,
            "a failing sibling test fails the gate: {}",
            r.output
        );
        assert!(
            r.output.contains("FAIL") || r.output.contains("AssertionError"),
            "{}",
            r.output
        );
        assert!(
            recorded.measured_ms.contains_key(&r.command),
            "the run was timed for the next plan: {recorded:?}"
        );
    }
}
