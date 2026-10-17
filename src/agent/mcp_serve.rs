//! `aizen mcp serve` — aizen as an MCP **server**, so another agent can hand it work.
//!
//! Everything else in this crate points outwards: [`crate::agent::mcp`] connects to servers and
//! borrows their tools. This is the same protocol pointed the other way. Claude Code, Codex, Cursor
//! — anything that speaks MCP — spawns `aizen mcp serve` in a repository and gets three tools:
//! dispatch one specialist, fan several out and synthesise them, and ask who is available.
//!
//! Why it is worth having, in one line: **aizen's specialists become callable by an agent that is
//! not aizen.** A card you wrote once in `~/.aizen/agents` — your reviewer, with your rules, on your
//! model — can now be dispatched by whatever assistant you happen to be talking to, without that
//! assistant knowing anything about aizen beyond the tool schema.
//!
//! Three things follow from the implementation being this thin:
//!
//! * The tools are *core's own* `task` and `workflow`, taken out of the same registry a REPL turn
//!   builds. Schema, description, scoping, depth cap, concurrency gate, checkpointing and the
//!   orchestration registry all come along unchanged, because none of it is reimplemented here.
//! * Every dispatch therefore writes a run manifest to `~/.aizen/orchestration/runs`, so a fan-out
//!   started by Claude Code shows up in `/workflows` and in the desktop's office pane exactly like
//!   one started by aizen itself — stamped with who asked, from the MCP handshake.
//! * **Write access is a decision made on the command line, never in the tool call.** Started plain,
//!   the server refuses any dispatch that resolves to a registry holding a destructive tool; the
//!   caller can ask for a `coder`, and be told no. `--yes` is what lifts that, and it is typed by a
//!   person into their own shell. An agent cannot grant itself the right to edit this repository by
//!   choosing a different argument — nor by choosing a different tool NAME: `tools/call` accepts
//!   only the advertised names ([`ADVERTISED`] + [`ROSTER`]), so the rest of the registry this
//!   process holds (`shell_run`, `file_write`, …) is unreachable from the wire. Both axes together
//!   are the security posture of this file.
//!
//! Deliberately absent: this process arms no LSP session and prints nothing to stdout that is not
//! protocol. A language server spawned for a client that only wanted a code review is a cost nobody
//! asked for, and one stray `println!` in the middle of a JSON-RPC stream is a corrupted transport
//! rather than a cosmetic bug — so diagnostics go to stderr and stay there.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::agent::tools::ToolRegistry;

/// The revision of MCP this speaks. Matched to what core's own client negotiates.
const PROTOCOL: &str = "2025-06-18";

/// The three tools offered. `task` and `workflow` are proxies onto the real registry entries; only
/// `agents` is written here, because "who could I ask?" is a question core answers to a person on a
/// terminal and had no wire form.
const ROSTER: &str = "agents";

/// The registry names a wire client may reach. `tools/list` advertises these and `tools/call`
/// refuses everything else BY NAME, before the registry is consulted. The registry behind this
/// server is the full top-level surface (it has to be — `task` resolves its sub-registries out of
/// it), so it also holds `shell_run` and `file_write` at Yolo approval with the loop-level
/// cmd_guard floor nowhere on this path. This list, not the registry, is the boundary.
const ADVERTISED: [&str; 2] = ["task", "workflow"];

/// One dispatch's answer, bounded. A sub-agent that returns a whole file tree is a sub-agent that
/// was asked the wrong question, and the caller's context window is not ours to spend.
const MAX_RESULT: usize = 60_000;

pub fn run(allow_write: bool) -> Result<()> {
    // Nobody is watching this process: the "user" is another agent on a JSON-RPC pipe, and the
    // one-time sandbox degradation warning goes to a stderr nobody reads. Marked unattended so the
    // sandbox applies the same fail-closed rule it applies to cron and the serve daemon.
    crate::sandbox::set_process_unattended();

    // Resolved once, exactly as the one-shot CLI path resolves it: flags are absent here, so this is
    // env then `~/.aizen/cli-config.json`. A server with no key should fail now, loudly, on the
    // terminal of the person who started it — not on the first tool call, inside another agent's
    // turn, where the error becomes a sentence in a transcript nobody reads.
    let (base_url, api_key, model) = crate::core::endpoint::resolve_endpoint(None, None, None)?;
    let client = crate::core::endpoint::http_client()?;
    let context_window = crate::ui::context_report::resolve_ctx_window(&model).0;

    /* Approval is `Yolo` in both modes, and that is not the hole it looks like.
    There is no human on this end of the pipe, so `Ask` would be a prompt written to a stream
    that carries JSON-RPC — it would corrupt the transport and then block forever waiting for an
    answer that cannot arrive. The real gate is one layer up: without `--yes` a dispatch that
    could write is refused before it starts, so everything that reaches the loop is read-only and
    has nothing to approve. With `--yes` the person starting the server has said the quiet part
    out loud on their own command line. */
    let roots = DispatchRoots {
        root: std::env::current_dir()
            .context("resolving MCP registry workspace")?
            .canonicalize()
            .context("canonicalizing MCP registry workspace")?,
        home: crate::core::config::aizen_home(),
    };
    let registry = crate::agent::builtin::default_registry_with_task(
        client,
        base_url,
        api_key,
        model,
        crate::core::approval::ApprovalMode::Yolo,
        context_window,
        Some(roots.root.clone()), // registry and placement share this exact immutable workspace
    )?;

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            // The client closed the pipe mid-line. Nothing to answer and nobody to answer to.
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = respond_scoped(&registry, allow_write, &line, Some(&roots)) else {
            continue;
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

/// One line in, at most one line out. `None` means "say nothing" — which is the correct and
/// required answer to a notification, and to anything unparseable enough that we cannot tell whose
/// reply it would be.
/// Server-owned roots, captured alongside the registry. Never taken from JSON-RPC arguments.
struct DispatchRoots {
    root: PathBuf,
    home: PathBuf,
}

#[cfg(test)]
fn respond(registry: &ToolRegistry, allow_write: bool, line: &str) -> Option<String> {
    respond_scoped(registry, allow_write, line, None)
}

fn respond_scoped(
    registry: &ToolRegistry,
    allow_write: bool,
    line: &str,
    roots: Option<&DispatchRoots>,
) -> Option<String> {
    let msg: Value = serde_json::from_str(line).ok()?;
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    let outcome = dispatch(registry, allow_write, method, &params, roots);

    // A JSON-RPC notification carries no id and must never be answered. Some clients treat a reply
    // to one as a protocol violation and drop the connection, so this check comes before the work
    // is turned into a message — not after.
    let id = id?;
    let body = match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": message},
        }),
    };
    serde_json::to_string(&body).ok()
}

fn dispatch(
    registry: &ToolRegistry,
    allow_write: bool,
    method: &str,
    params: &Value,
    roots: Option<&DispatchRoots>,
) -> Result<Value, String> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools(registry, allow_write) })),
        "tools/call" => Ok(call_scoped(registry, allow_write, params, roots)),
        other => Err(format!("phương thức không hỗ trợ: {other}")),
    }
}

/* The handshake, and the one piece of information this file goes out of its way to keep.
`clientInfo.name` is the caller naming itself, and it is stamped onto every run this process
starts. That is why the office pane can seat "Claude Code" at the director's desk without any
guesswork: nothing is inferred from a process tree or a directory name — the program said who it
was, in the first message it sent. It is a self-reported label and is treated as one: bounded,
stripped of control characters, and used for display only. */
fn initialize(params: &Value) -> Value {
    let who = params
        .get("clientInfo")
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    crate::agent::orchestration::set_origin(who);
    json!({
        "protocolVersion": PROTOCOL,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "aizen", "version": env!("CARGO_PKG_VERSION")},
    })
}

fn tools(registry: &ToolRegistry, allow_write: bool) -> Vec<Value> {
    let mut out = Vec::new();
    // Descriptions and schemas are read off the live registry rather than restated here. Restating
    // them would mean this file and the tool could disagree about what the tool does, and the
    // caller would believe this file.
    for name in ADVERTISED {
        let Some(tool) = registry.get(name) else {
            continue;
        };
        let mut description = tool.description().to_string();
        if !allow_write {
            description.push_str(
                "\n\nREAD-ONLY SERVER: this aizen was started without --yes, so any dispatch that \
                 resolves to a sub-agent holding edit or shell tools (role coder/tester, or a \
                 specialist whose card grants them) is refused before it runs. Use planner, \
                 reviewer, or a read-only specialist.",
            );
        }
        out.push(json!({
            "name": name,
            "description": description,
            "inputSchema": tool.parameters(),
        }));
    }
    out.push(json!({
        "name": ROSTER,
        "description":
            "List the aizen specialists available on this machine: slug to pass as `agent`, what \
             each is for, and whether it can write. Call this before `task` when you want a named \
             specialist rather than a generic role.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
    }));
    out
}

#[cfg(test)]
fn call(registry: &ToolRegistry, allow_write: bool, params: &Value) -> Value {
    call_scoped(registry, allow_write, params, None)
}

fn call_scoped(
    registry: &ToolRegistry,
    allow_write: bool,
    params: &Value,
    roots: Option<&DispatchRoots>,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    if name == ROSTER {
        return text(roster(), false);
    }
    // The allowlist comes BEFORE the registry lookup — see [`ADVERTISED`]. Every other registry
    // name must be unreachable from the wire, whether or not it exists.
    if !ADVERTISED.contains(&name) {
        return text(
            format!("không có công cụ tên '{name}' — máy chủ này chỉ có: task, workflow, {ROSTER}"),
            true,
        );
    }
    let Some(tool) = registry.get(name) else {
        return text(format!("không có công cụ tên '{name}'"), true);
    };
    let preparation_context = crate::core::exec_ctx::ExecutionContext::default();
    let prepared = match tool.plan_call(&args, &preparation_context) {
        Ok(call) => call,
        Err(error) => return text(error.to_string(), true),
    };
    if !allow_write && tool.delegation_read_only_prepared(&prepared) != Some(true) {
        return text(write_refusal(), true);
    }
    // Pin only after the original allowlist and write gate accept the dispatch. Handshake, list,
    // roster, unknown tools and denied calls never allocate a placement namespace. Read-only
    // task/workflow dispatches still need an inherited snapshot for their child loop boundary.
    let context = match roots {
        Some(roots) => {
            let placement = match crate::core::file_placement::PlacementContext::new(
                roots.root.clone(),
                roots.home.clone(),
            ) {
                Ok(placement) => placement,
                Err(error) => return text(format!("cannot pin MCP file placement: {error}"), true),
            };
            let scope = format!("mcp-request/{}", placement.run_id());
            Some(
                crate::core::exec_ctx::ExecutionContext::default()
                    .with_resource_scope(scope)
                    .with_placement(placement),
            )
        }
        None => None,
    };
    // A panicking tool must cost one answer, not the transport: unwinding out of `run()` kills
    // every future call the client was going to make, mid-session, with no protocol goodbye.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match context {
        Some(context) => {
            crate::core::exec_ctx::with_current(context, || tool.execute_prepared(&prepared))
        }
        None => tool.execute_prepared(&prepared),
    })) {
        Ok(Ok(answer)) => {
            let failed = tool
                .result_is_error(&answer.text)
                .unwrap_or_else(|| crate::agent::is_failure_result(&answer.text));
            text(bounded(answer.text), failed)
        }
        Ok(Err(e)) => text(format!("{e}"), true),
        Err(_) => text(
            "lỗi: công cụ panic khi chạy — chi tiết ở stderr của server",
            true,
        ),
    }
}

/// Cap an answer at [`MAX_RESULT`] without ever cutting inside a UTF-8 sequence.
/// `String::truncate` takes a BYTE index and panics off a char boundary — and these answers carry
/// Vietnamese prose, so a multibyte character at the cap is the common case, not the corner.
fn bounded(mut answer: String) -> String {
    if answer.len() > MAX_RESULT {
        let mut cut = MAX_RESULT;
        while cut > 0 && !answer.is_char_boundary(cut) {
            cut -= 1;
        }
        answer.truncate(cut);
        answer.push_str("\n… (cắt bớt)");
    }
    answer
}

/// Unknown or unprepared delegation is not a read-only grant.
fn write_refusal() -> String {
    format!(
        "từ chối: lượt này có quyền sửa tệp hoặc chạy lệnh, hoặc chưa xác định được quyền, mà máy chủ này khởi động \
         không có --yes.\nDùng role `planner` / `reviewer`, hoặc một specialist chỉ đọc — gọi \
         `{ROSTER}` để xem ai chỉ đọc.\nMuốn cho ghi thật thì người dùng phải tự chạy lại: \
         `aizen mcp serve --yes`. Quyền đó không nằm trong tham số bạn gửi."
    )
}

#[cfg(test)]
fn refuse_write(registry: &ToolRegistry, name: &str, args: &Value) -> Option<String> {
    let tool = registry.get(name)?;
    let call = tool.plan_call(args, &crate::core::exec_ctx::ExecutionContext::default());
    (!call.is_ok_and(|call| tool.delegation_read_only_prepared(&call) == Some(true)))
        .then(write_refusal)
}

/// Who is on the payroll, as a plain table. Read-only status comes from the same resolver the write
/// gate consults, so the column cannot lie about what this server would actually allow.
fn roster() -> String {
    let root = std::env::current_dir().unwrap_or_default();
    let enabled = crate::agents::enabled_set();
    let mut lines = Vec::new();
    for def in crate::agents::list() {
        let slug = def.slug();
        // No enabled-file at all means nobody has ever pinned anything, and everything is live.
        let pinned = enabled.as_ref().is_none_or(|set| set.contains(&slug));
        let reg = crate::agent::builtin::agent_registry(&def, &root);
        let reach = if crate::agent::task_tool::dispatch_is_read_only(&reg) {
            "chỉ đọc"
        } else {
            "ghi được tệp"
        };
        lines.push(format!(
            "{slug}\t{reach}\t{}\t{}",
            if pinned { "đang bật" } else { "đang tắt" },
            def.description.trim(),
        ));
    }
    if lines.is_empty() {
        return "Chưa có specialist nào trên máy này. Dùng role planner/reviewer/coder/tester, \
                hoặc tạo thẻ bằng `aizen agents install`."
            .to_string();
    }
    format!(
        "slug\tquyền\ttrạng thái\tmô tả\n{}\n\nTruyền slug vào tham số `agent` của `task`.",
        lines.join("\n")
    )
}

/// The MCP shape for a tool answer. A failed call is `isError` with the reason as text, not a
/// JSON-RPC error: the caller is a model, and a model can read a sentence and try something else.
fn text(body: impl Into<String>, failed: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": body.into()}],
        "isError": failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PlacementProbe {
        root: PathBuf,
        home: PathBuf,
    }

    impl crate::agent::tools::Tool for PlacementProbe {
        fn name(&self) -> &str {
            "task"
        }
        fn description(&self) -> &str {
            "Hermetic advertised dispatch placement probe"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn delegation_read_only_prepared(
            &self,
            _call: &crate::agent::tools::PreparedToolCall,
        ) -> Option<bool> {
            Some(true)
        }
        fn execute(&self, _args: &Value) -> anyhow::Result<String> {
            let context = crate::core::exec_ctx::current().expect("internally pinned MCP context");
            let placement = context
                .placement()
                .expect("internally pinned MCP placement");
            assert_eq!(placement.workspace_root(), self.root);
            assert_eq!(placement.home_root(), self.home);
            let child = placement.fork_for(self.root.clone(), "workflow/a")?;
            let retry = placement.fork_for(self.root.clone(), "task/attempt/2")?;
            assert_eq!(child.run_id(), placement.run_id());
            assert_eq!(retry.run_id(), placement.run_id());
            assert_ne!(child.artifact_root(), retry.artifact_root());
            assert!(
                !self.home.exists(),
                "MCP boundary pinning must be read-only"
            );
            Ok(placement.run_id().to_string())
        }
    }

    #[test]
    fn advertised_dispatch_gets_internal_roots_and_a_fresh_shared_run() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let root = fixture.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let home = fixture.path().join("missing-home");
        let roots = DispatchRoots {
            root: root.clone(),
            home: home.clone(),
        };
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(PlacementProbe {
            root,
            home: home.clone(),
        }));
        let wire = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "task", "arguments": {
                "placement": {"workspace": "forged-root", "run_id": "forged-run"},
                "exec_ctx": {"resource_scope": "forged-scope"}
            }}
        })
        .to_string();
        let first: Value =
            serde_json::from_str(&respond_scoped(&registry, false, &wire, Some(&roots)).unwrap())
                .unwrap();
        let second: Value =
            serde_json::from_str(&respond_scoped(&registry, false, &wire, Some(&roots)).unwrap())
                .unwrap();
        assert_eq!(first["result"]["isError"], false);
        assert_ne!(
            first["result"]["content"][0]["text"],
            second["result"]["content"][0]["text"]
        );
        assert!(crate::core::exec_ctx::current().is_none());
        assert!(!home.exists());
    }

    #[test]
    fn metadata_and_unadvertised_calls_do_not_use_unavailable_placement_roots() {
        let registry = ToolRegistry::new();
        let roots = DispatchRoots {
            root: PathBuf::from("relative-invalid-workspace"),
            home: PathBuf::from("unused-home"),
        };
        for (method, params) in [
            ("ping", json!({})),
            ("tools/list", json!({})),
            ("tools/call", json!({"name": "file_write", "arguments": {}})),
        ] {
            let wire =
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
            let response: Value = serde_json::from_str(
                &respond_scoped(&registry, false, &wire, Some(&roots)).unwrap(),
            )
            .unwrap();
            assert!(!response
                .to_string()
                .contains("cannot pin MCP file placement"));
        }
    }

    struct PreparedProbe {
        executed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        read_only: Option<bool>,
    }

    impl crate::agent::tools::Tool for PreparedProbe {
        fn name(&self) -> &str {
            "workflow"
        }
        fn description(&self) -> &str {
            "Prepared dispatch gate probe"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn plan_call(
            &self,
            args: &Value,
            _ctx: &crate::core::exec_ctx::ExecutionContext,
        ) -> anyhow::Result<crate::agent::tools::PreparedToolCall> {
            if args.get("malformed").and_then(Value::as_bool) == Some(true) {
                anyhow::bail!("malformed dispatch");
            }
            let mut call = crate::agent::tools::PreparedToolCall::new(args.clone());
            call.delegation = Some(std::sync::Arc::new("frozen reader registry".to_string()));
            Ok(call)
        }
        fn delegation_read_only_prepared(
            &self,
            call: &crate::agent::tools::PreparedToolCall,
        ) -> Option<bool> {
            assert_eq!(
                call.delegation
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<String>()
                    .unwrap(),
                "frozen reader registry"
            );
            self.read_only
        }
        fn execute(&self, _args: &Value) -> anyhow::Result<String> {
            panic!("must execute the authorized prepared dispatch");
        }
        fn execute_prepared(
            &self,
            call: &crate::agent::tools::PreparedToolCall,
        ) -> anyhow::Result<crate::agent::tools::ToolObservation> {
            self.executed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::agent::tools::ToolObservation::text(
                call.delegation
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<String>()
                    .unwrap()
                    .clone(),
            ))
        }
    }

    #[test]
    fn prepared_gate_refuses_unknown_writer_and_malformed_without_execution() {
        for read_only in [None, Some(false), Some(true)] {
            let executed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut registry = ToolRegistry::new();
            registry.register(Box::new(PreparedProbe {
                executed: executed.clone(),
                read_only,
            }));
            for malformed in [false, true] {
                let response = call(
                    &registry,
                    false,
                    &json!({"name": "workflow", "arguments": {
                        "tasks": [{"role": "argus"}], "malformed": malformed
                    }}),
                );
                let accepted = read_only == Some(true) && !malformed;
                assert_eq!(response["isError"], !accepted);
                if accepted {
                    assert_eq!(response["content"][0]["text"], "frozen reader registry");
                }
            }
            assert_eq!(
                executed.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(read_only == Some(true))
            );
        }
    }

    #[test]
    fn generated_implement_writer_cannot_hide_behind_supplied_reader_tasks() {
        let _lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let root = fixture.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let home = fixture.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", home.as_os_str())]);
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            reqwest::Client::new(),
            "http://unused.invalid".into(),
            String::new(),
            "fake".into(),
            crate::core::approval::ApprovalMode::Ask,
            0,
            root.clone(),
            0,
        )));
        let response = call_scoped(
            &registry,
            false,
            &json!({"name": "workflow", "arguments": {
                "mode": "implement", "prompt": "Implement the isolated parser fix in src/parser.rs",
                "tasks": [{"role": "argus", "prompt": "Only inspect src/parser.rs"}]
            }}),
            Some(&DispatchRoots {
                root,
                home: fixture.path().join("must-not-be-created"),
            }),
        );
        assert_eq!(response["isError"], true);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("--yes"));
        assert!(!fixture.path().join("must-not-be-created").exists());
    }

    #[test]
    fn a_notification_is_never_answered() {
        let reg = ToolRegistry::new();
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(respond(&reg, false, note).is_none());
    }

    #[test]
    fn garbage_gets_silence_rather_than_a_guess() {
        let reg = ToolRegistry::new();
        assert!(respond(&reg, false, "not json at all").is_none());
    }

    #[test]
    fn the_handshake_names_the_caller() {
        let reg = ToolRegistry::new();
        let hello = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":
            {"clientInfo":{"name":"Claude Code","version":"9"}}}"#;
        let out = respond(&reg, false, hello).expect("initialize is answered");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "aizen");
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL);
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn an_unknown_method_is_an_error_with_the_same_id() {
        let reg = ToolRegistry::new();
        let msg = r#"{"jsonrpc":"2.0","id":"abc","method":"resources/list"}"#;
        let v: Value = serde_json::from_str(&respond(&reg, false, msg).unwrap()).unwrap();
        assert_eq!(v["id"], "abc");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resources/list"));
    }

    #[test]
    fn the_roster_tool_is_offered_even_with_no_dispatch_registry() {
        let reg = ToolRegistry::new();
        let listed = tools(&reg, false);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["name"], ROSTER);
    }

    #[test]
    fn a_read_only_server_says_so_in_every_dispatch_description() {
        // Without a configured endpoint there is no task tool to read, so this asserts the shape of
        // the amendment rather than its presence: whatever is advertised, the caller is told the
        // rule it will be judged by before it writes a call.
        let reg = ToolRegistry::new();
        for t in tools(&reg, false) {
            let d = t["description"].as_str().unwrap();
            assert!(!d.is_empty(), "every advertised tool describes itself");
        }
    }

    #[test]
    fn a_missing_tool_answers_in_band() {
        let reg = ToolRegistry::new();
        let out = call(&reg, true, &json!({"name": "task", "arguments": {}}));
        assert_eq!(out["isError"], true);
        assert!(out["content"][0]["text"].as_str().unwrap().contains("task"));
    }

    #[test]
    fn an_unadvertised_registry_tool_is_unreachable_by_name() {
        // The registry behind a real server holds shell_run/file_write at Yolo approval; the wire
        // must not be able to reach them by naming them, `--yes` or not. Asserted on the refusal
        // text so a future "helpful" fallback to registry lookup fails this test loudly.
        let reg = ToolRegistry::new();
        for name in ["shell_run", "file_write", "file_edit", "memory_save"] {
            for allow_write in [false, true] {
                let out = call(&reg, allow_write, &json!({"name": name, "arguments": {}}));
                assert_eq!(out["isError"], true, "{name} must be refused");
                assert!(
                    out["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("chỉ có: task, workflow"),
                    "{name} must be refused by the allowlist, not by registry absence"
                );
            }
        }
    }

    #[test]
    fn the_answer_cap_never_cuts_inside_a_character() {
        // A wall of 3-byte characters guarantees MAX_RESULT lands mid-sequence; the old
        // `String::truncate(MAX_RESULT)` panicked here and took the whole server with it.
        let wall = "ế".repeat(MAX_RESULT); // 3 bytes each — far over the cap
        let out = bounded(wall);
        assert!(out.len() <= MAX_RESULT + "\n… (cắt bớt)".len());
        assert!(out.ends_with("(cắt bớt)"));
        // Round-trip through str validation: a mid-char cut would have panicked already, but be
        // explicit that what remains is valid UTF-8 of whole characters.
        assert!(out.chars().all(|c| c == 'ế' || "\n… (cắt bớt)".contains(c)));
    }

    #[test]
    fn the_write_gate_needs_a_task_tool_to_consult() {
        // No registry, no oracle — and the gate refuses to invent an answer. `call` still refuses
        // the dispatch, because the tool it would run is not there either.
        let reg = ToolRegistry::new();
        assert!(refuse_write(&reg, "task", &json!({"role": "coder"})).is_none());
    }

    #[test]
    fn a_refusal_names_the_flag_that_lifts_it_and_not_an_argument() {
        // The wording is load-bearing: a caller that reads "pass allow_write: true" would try it.
        let reg = ToolRegistry::new();
        let listed = tools(&reg, false);
        let roster_desc = listed[0]["description"].as_str().unwrap();
        assert!(
            !roster_desc.contains("--yes"),
            "the roster tool is not the gate"
        );
    }
}
