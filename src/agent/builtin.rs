//! The built-in tool surface (H3). Five orthogonal tools the agent loop advertises:
//! `memory_search` (the brain), `file_read`/`file_glob` (read-only), `file_edit`/`shell_run`
//! (destructive → approval-gated by the loop).
//!
//! Paths: `root` (captured at registry-build time) is the anchor for resolving RELATIVE paths, so
//! tools stay testable against a temp dir without mutating the process-global cwd. The workspace
//! CONFINEMENT boundary was removed by user request — file/shell ops may now reach paths anywhere
//! on disk (a relative path still resolves under `root`; `../` and absolute paths escape freely).
//! SECURITY trade-off: agent-read web/tool content can now steer a write to an arbitrary path.

use crate::agent::tools::{
    FilePreparation, PreparedFootprint, PreparedToolCall, Tool, ToolObservation, ToolRegistry,
    WorkspaceEffect,
};
use crate::core::exec_ctx::ExecutionContext;
use crate::core::file_placement::{prepare_destination, FilePurpose, ResolvedDestination};
use crate::core::repo_lock::LockMode;
use crate::core::workspace_txn::NativeResource;
use anyhow::{bail, Context, Result};
use ignore::{WalkBuilder, WalkState};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

#[path = "speculative_native_tools.rs"]
mod speculative_native_tools;
pub(crate) use speculative_native_tools::prepared_native_worker;

/// Default wall-clock cap for `shell_run` (a hung command must never freeze the agent loop).
const SHELL_TIMEOUT_SECS: u64 = 120;

/// Env override for the `shell_run` cap. 120s is right for a test run and wrong for a cold
/// `cargo build` on a big workspace — a fixed ceiling turns "slow" into "killed", and the model then
/// retries the same command, paying the full wait twice. Clamped to a sane band so a typo cannot
/// disable the ceiling that keeps the loop responsive.
const SHELL_TIMEOUT_ENV: &str = "AIZEN_SHELL_TIMEOUT_SECS";

/// How long to wait for the drain threads AFTER the child is gone.
///
/// This is the fix for a confirmed hang: on Windows `kill()` reaps only the `cmd.exe` wrapper, and a
/// surviving grandchild still holds the inherited write end of our pipes, so `read_to_end` never sees
/// EOF. The old code called `join()` unconditionally and could block forever — measured still blocked
/// 12s after a kill, against a 45s sleeper. Tree-kill removes the cause; this bound means even an
/// exotic case (a descendant we could not contain, a pipe inherited by an unrelated process) costs
/// two seconds and a partial-output note instead of the whole session.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Resolve the `shell_run` wall-clock cap: env override, else the sandbox config's
/// `limits.wall_seconds`, else the default. All clamped 10s..=3600s.
fn shell_timeout_secs() -> u64 {
    std::env::var(SHELL_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            crate::sandbox::policy::settings()
                .limits
                .and_then(|l| l.wall_seconds)
        })
        .map(|v| v.clamp(10, 3600))
        .unwrap_or(SHELL_TIMEOUT_SECS)
}

/// How often to say which command is still running. The working pill already animates and counts
/// elapsed seconds, so plain liveness is covered; what it cannot show is *which* command is out and
/// how close the ceiling is — and without that a slow build and a wedged process look identical.
const SLOW_NOTE_EVERY_SECS: u64 = 30;

/// Note a still-running command into the transcript — but ONLY for interactive top-level turns
/// where the progress is visible. A delegated child (quiet, trace_visible=false) must not write
/// into the parent transcript; its live status is visible through the orchestration board instead.
fn note_slow_command(command: &str, waited: u64, cap: u64) {
    // Suppress for delegated quiet children.
    if let Some(ctx) = crate::core::exec_ctx::current() {
        if !ctx.trace_visible() {
            return;
        }
    }
    // One line, command clipped: a 300-char pipeline would wrap and push the transcript around.
    let mut label: String = command.trim().chars().take(60).collect();
    if command.trim().chars().count() > 60 {
        label.push('…');
    }
    crate::agent::emit_trace_public(&format!(
        "→ still running after {waited}s (cap {cap}s): {label}  ·  Esc cancels"
    ));
}

/// Prefix a write tool returns when the target already held byte-identical content — so nothing
/// was written to disk. The agent loop keys off this exact prefix (`turn_made_edits`) to NOT arm
/// the verify gate for a no-op (an unchanged tree can't have broken), and the thrash guard sees it
/// as "not an edit" so a model re-writing the same bytes climbs to a stop instead of looping.
pub(crate) const NOOP_WRITE_PREFIX: &str = "no change (identical content)";

/// `file_read` budget: a WHOLE-file read over EITHER cap returns a head+tail preview with a loud
/// marker (so the model knows it has a partial view). A range/numbered read is NEVER bounded. Small
/// files (the common case) stay byte-exact so `old_string` round-trips. `0` disables a cap.
const FILE_READ_MAX_LINES: usize = 2000;
/// The ONE read budget. `file_read` is the only authority on how much of a file the model sees:
/// the loop no longer re-cuts its output (see `agent::is_self_budgeted`), so this number is what
/// actually reaches the model — roughly the 12 k-char window the loop used to keep, with a little
/// headroom, and every cut it makes is contiguous and marked with the lines it left out.
const FILE_READ_MAX_BYTES: usize = 16_000;
/// Soft threshold (well below the hard `FILE_READ_MAX_LINES` budget): a WHOLE-file read of a
/// SOURCE file longer than this earns a one-line hint to prefer `lsp_document_symbols` + `read_symbol`
/// — the single biggest token sink in a real task is reading whole files the model only needs one
/// item from, re-sent in history every loop. Advisory only (the full content still follows); the
/// prompt already says "read the slice you need", but nothing applied pressure at call time.
const FILE_READ_SYMBOL_HINT_LINES: usize = 400;

/// The retrieval-first hint for a whole-file read, or `None` when it wouldn't help. Pure (no I/O /
/// globals) so the policy is unit-testable: a hint fires only when the file is SOURCE (`is_code` —
/// a language server exists for its extension, so the symbol tools actually work), the LSP surface
/// is live (`lsp_on` — else the tools it names aren't registered), and the read is a long whole-file
/// pull (`line_count` in the soft-hint band, under the hard budget where `budget_view` already warns).
fn whole_file_symbol_hint(is_code: bool, lsp_on: bool, line_count: usize) -> Option<String> {
    if is_code
        && lsp_on
        && line_count > FILE_READ_SYMBOL_HINT_LINES
        && line_count <= FILE_READ_MAX_LINES
    {
        Some(format!(
            "[hint: read {line_count} lines whole. If you need one item, `lsp_document_symbols` \
             (outline) then `read_symbol` (one full body) is far cheaper and keeps context lean — \
             this whole file is re-sent every turn. Ignore if you genuinely need all of it.]"
        ))
    } else {
        None
    }
}

/// The live top-level tool surface, published once the session's registry is built. The skills
/// index consults it to hide any skill whose `requires:` tool is absent from this build/session
/// (e.g. `browser_*` when `--features browser` is off, or MCP tools when no server is configured).
/// `None` until published → the filter is a no-op (show all), so the unit tests and the offline
/// `aizen skill` path are unaffected.
/// Stored in ADVERTISED order, not as a set: the system prompt's tool-routing map is generated from
/// this list and must name tools in the same order the request's `tools` array carries them.
/// DEFERRED tools (schema reached via `tool_search`) are deliberately absent — the map may only
/// name what the request advertises.
static ACTIVE_TOOL_NAMES: Lazy<Mutex<Option<Vec<String>>>> = Lazy::new(|| Mutex::new(None));

/// The DEFERRED half of the surface: tool names + per-server counts, published beside
/// [`ACTIVE_TOOL_NAMES`]. Names feed the skills `requires:` gate (a deferred tool is callable, so
/// a skill needing it still applies); counts feed the prompt's deferred-integrations note.
static DEFERRED_TOOL_SURFACE: Lazy<Mutex<Option<crate::agent::tools::DeferredSurface>>> =
    Lazy::new(|| Mutex::new(None));

/// Publish the live tool surface (idempotent). ONLY the top-level registry calls this — never the
/// smaller `role_registry`, so the set is never wrongly shrunk when a sub-agent assembles a prompt.
///
/// The deferred half is published only while `tool_search` survived the toolset filter: deferred
/// tools without their discovery tool are unreachable, and advertising unreachable tools in the
/// prompt is exactly the drift the routing map exists to prevent.
pub(crate) fn publish_active_tools(r: &ToolRegistry) {
    *ACTIVE_TOOL_NAMES.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.advertised_names());
    let deferred = if r.get("tool_search").is_some() {
        r.deferred_summary()
    } else {
        None
    };
    *DEFERRED_TOOL_SURFACE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = deferred;
}

/// The published live tool surface in advertised order, or `None` if no session registry has been
/// built yet. This is what the top-level system prompt's routing map is generated from, which is why
/// the order matters: the prompt and the request's `tools` array must agree name-for-name.
pub fn active_tool_surface() -> Option<Vec<String>> {
    ACTIVE_TOOL_NAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The published live tool surface as a set, for membership tests (the `<skills>` index filter).
/// Includes DEFERRED tool names: a deferred tool is callable once discovered through `tool_search`,
/// so a skill that `requires:` one must not be hidden by the deferral.
pub fn active_tool_names() -> Option<HashSet<String>> {
    let mut set: HashSet<String> = match active_tool_surface() {
        Some(v) => v.into_iter().collect(),
        None => return None,
    };
    if let Some((names, _)) = DEFERRED_TOOL_SURFACE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        set.extend(names.iter().cloned());
    }
    Some(set)
}

/// The prompt block for the DEFERRED surface, or `None` when nothing is deferred (the common case
/// — zero bytes added). Appended to the top-level prompt's dynamic lane right after the routing
/// map, whose "these are the only tools" claim it narrows: deferred integrations exist, and
/// `tool_search` is the door. Byte-stable across turns of one session (same surface ⇒ same string),
/// so it costs no cache churn.
pub fn deferred_tools_note() -> Option<String> {
    let guard = DEFERRED_TOOL_SURFACE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(deferred_tools_note_for)
}

/// The note text for one deferred surface. Built-in deferred tools are NAMED (there are a dozen
/// and the model must know `checkpoint` exists to ask for it); MCP tools are counted per server
/// (there can be hundreds). Byte-stable for a stable surface.
pub(crate) fn deferred_tools_note_for(surface: &crate::agent::tools::DeferredSurface) -> String {
    let (names, by_origin) = surface;
    let builtin: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| {
            DEFERRED_ALWAYS.contains(n) || DEFERRED_FOR_QUESTIONS.contains(n) || *n == "web_crawl"
        })
        .collect();
    let mcp: Vec<String> = by_origin
        .iter()
        .filter(|(o, _)| o != "builtin")
        .map(|(s, n)| format!("{s} ({n})"))
        .collect();
    let mut s = String::from("# Deferred tools (via tool_search)\nBeyond the surface above, ");
    if !builtin.is_empty() {
        s.push_str(&format!(
            "these built-in tools are registered but not pre-loaded: {}",
            builtin.join(", ")
        ));
    }
    if !mcp.is_empty() {
        if !builtin.is_empty() {
            s.push_str("; and ");
        }
        s.push_str(&format!(
            "{} MCP tool(s) are connected but not pre-loaded: {}",
            names.len() - builtin.len(),
            mcp.join(", ")
        ));
    }
    s.push_str(
        ". When the task needs one, find it with `tool_search` (by name or capability) — a match \
         returns the full argument schema, and that tool is then callable directly by its exact \
         name.\n",
    );
    s
}

/// Swap the published surface, returning the previous value — TESTS ONLY.
///
/// The surface is process-global (one session, one registry), so a test that builds a real registry
/// would otherwise leak a tool-routing block into every prompt assembled by whatever test runs next.
/// Callers save, act, and restore.
#[cfg(test)]
pub(crate) fn swap_active_tools_for_test(next: Option<Vec<String>>) -> Option<Vec<String>> {
    let mut g = ACTIVE_TOOL_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *g, next)
}

/// Swap the published DEFERRED surface — TESTS ONLY (same save/act/restore discipline as
/// [`swap_active_tools_for_test`], same reason: process-global state).
#[cfg(test)]
pub(crate) fn swap_deferred_tools_for_test(
    next: Option<crate::agent::tools::DeferredSurface>,
) -> Option<crate::agent::tools::DeferredSurface> {
    let mut g = DEFERRED_TOOL_SURFACE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *g, next)
}

/// Resolve + canonicalize the working-directory root — the base that relative file/shell paths
/// resolve against (NOT a hard boundary: `confine` no longer rejects paths that escape it, so an
/// absolute or `../` path may reach elsewhere on disk — see [`confine`]).
fn resolve_root() -> Result<PathBuf> {
    std::env::current_dir()
        .context("resolving cwd")?
        .canonicalize()
        .context("canonicalizing cwd")
}

/// The built-in tools rooted at `root`. Shared by the top-level registry and the `coder`
/// sub-agent role, and by the task suite (`bench tasks`), which wants the real surface minus the
/// `task`/`workflow` dispatchers.
pub(crate) fn default_registry_in(root: &Path) -> ToolRegistry {
    #[cfg(feature = "browser")]
    let browser_configured = crate::agent::browser::is_configured();
    #[cfg(not(feature = "browser"))]
    let browser_configured = false;
    default_registry_for_browser(root, browser_configured)
}

fn default_registry_for_browser(root: &Path, browser_configured: bool) -> ToolRegistry {
    #[cfg(not(feature = "browser"))]
    let _ = browser_configured;
    use crate::agent::web_tools::{WebCrawl, WebFetch, WebSearch};
    // Registry construction happens exactly once per fresh top-level user turn. Apply any deferred
    // MCP `tools/list_changed` notification here — never from inside an agent run — so the model's
    // advertised tool schemas remain pinned for the duration of that run.
    crate::agent::mcp::prepare_fresh_turn();
    let mut r = ToolRegistry::new();
    r.register(Box::new(MemorySearch));
    r.register(Box::new(crate::agent::design::DesignBriefTool));
    r.register(Box::new(crate::agent::design::DesignGuideTool));
    r.register(Box::new(crate::agent::design::DesignReviewTool));
    r.register(Box::new(crate::agent::design::ReferenceCompareTool));
    r.register(Box::new(crate::agent::design::ReferenceTraceTool));
    r.register(Box::new(crate::agent::image_read::ReferenceRead::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(MemoryList));
    r.register(Box::new(MemoryProfile));
    r.register(Box::new(MemoryAsk));
    // Top-level only, like the `<sessions>` block that advertises it: a sub-agent serves one
    // stated task and has no business rummaging through the user's other conversations.
    r.register(Box::new(SessionRecall));
    r.register(Box::new(FileRead::new(root.to_path_buf())));
    r.register(Box::new(FileGlob::new(root.to_path_buf())));
    r.register(Box::new(crate::agent::search::SearchFiles::new(
        root.to_path_buf(),
    )));
    // Semantic chunk search over the `/init` index (read-only). Self-errors with "run /init" when
    // the index is missing, so it's safe to always advertise.
    r.register(Box::new(crate::agent::codebase::CodebaseSearch));
    r.register(Box::new(WebSearch));
    r.register(Box::new(WebFetch));
    r.register(Box::new(WebCrawl));
    register_skill_load(&mut r);
    register_skill_registry(&mut r);
    register_telegram(&mut r);
    // `bot_admin` (host/persona extra bots on the owner's word) is TOP-LEVEL ONLY — never in
    // `subagent_read_only_base`, so a specialist sub-agent can't touch bot config.
    register_bot_admin(&mut r);
    register_notify(&mut r);
    // Two tools, not five: `checkpoint` carries the mutating actions (save/rewind/restore) and
    // `checkpoint_view` the read-only ones (diff/list). The split is on `is_destructive`, which the
    // trait defines per TYPE rather than per args — folding the reads in would make listing a
    // timeline ask the user for approval.
    r.register(Box::new(crate::features::timemachine::Checkpoint::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::features::timemachine::CheckpointView::new(
        root.to_path_buf(),
    )));
    // Explicit per-owner undo belongs only to the coordinating parent's full registry.
    r.register(Box::new(crate::agent::native_undo::NativeOwnerUndo::new(
        root.to_path_buf(),
    )));
    // Who else is editing this repository right now. Top-level only, and not because it is unsafe
    // (it is read-only): a sub-agent's writes are attributed to the SESSION that spawned it, so the
    // overlap warning it would need already surfaces on this window's turn. Registering it in
    // `subagent_read_only_base` would pay its schema on every delegated run for an answer the parent
    // is the one acting on.
    r.register(Box::new(crate::features::coop::TeamStatus));
    // Memory WRITE surface — top-level only. A sub-agent gets `memory_list`/`memory_search` but may
    // never mutate the user's long-term store: a specialist run is short-lived and unsupervised, so a
    // wrong write there would outlive the run it came from with no one having seen it.
    r.register(Box::new(MemorySave));
    r.register(Box::new(MemoryUpdate));
    r.register(Box::new(MemoryForget));
    r.register(Box::new(FileEdit::new(root.to_path_buf())));
    r.register(Box::new(FileWrite::new(root.to_path_buf())));
    r.register(Box::new(FileMove::new(root.to_path_buf())));
    r.register(Box::new(ShellRun::new(root.to_path_buf())));
    r.register(Box::new(SkillSave));
    register_skill_refine(&mut r);
    register_skill_forget(&mut r);
    // Top-level only (NOT in role sub-agents) — the in-session todo list is shared with the user's
    // scroll region, so a sub-agent must not clobber it. `role_registry` builds its own scratch list.
    r.register(Box::new(crate::agent::todo::TodoWrite));
    // `process` IS granted to the shell-capable sub-agent roles (see `role_registry`): its pool is
    // keyed by execution scope, so a child sees only the handles it started.
    r.register(Box::new(crate::agent::process::Process::new(
        root.to_path_buf(),
    )));
    // `clarify` yields the turn back to the interactive user — meaningless inside an autonomous
    // sub-agent (no user to answer), so it stays top-level only, like todo/process.
    r.register(Box::new(crate::agent::clarify::Clarify));
    // `goal_complete` is the DONE-signal for goal mode (`/goal <text>`). Gated on `goal::is_armed()`
    // so ordinary chat/agent turns never pay its schema cost; top-level only (a sub-agent has no
    // goal of its own to declare complete), just like clarify/todo/process.
    if crate::agent::goal::is_armed() {
        r.register(Box::new(crate::agent::goal::GoalComplete));
    }
    // LSP navigation + diagnostics + symbolic edit (top-level only). Default ON + lazy: the
    // manager is armed at session start; tools register when `LSP.is_enabled()` (still true after
    // `/lsp on`, false after `/lsp off`). Sub-agents use `subagent_read_only_base` and never get it.
    if crate::agent::lsp::LSP.is_enabled() {
        r.register(Box::new(crate::agent::lsp::tools::LspReferences::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspDefinition::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspSymbolBody::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspHover::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspDocumentSymbols::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspWorkspaceSymbol::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspDiagnostics::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::SymbolReplace::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::SymbolInsert::new(
            root.to_path_buf(),
        )));
        // The ranked codebase skeleton rides the LSP gate too (its symbols come from the servers).
        r.register(Box::new(crate::agent::repo_map::RepoMap::new(
            root.to_path_buf(),
        )));
    }
    // User-configurable MCP servers (`~/.aizen/mcp.json`) — each remote tool wrapped as
    // `mcp_<server>_<tool>`. Empty (zero cost) when MCP is unconfigured. Top-level only, like
    // todo/process: sub-agents share the same live connections via the global manager but don't
    // need the surface advertised to them.
    //
    // A server the deferral plan marked (pinned `defer: true`, or auto — the combined schema
    // estimate over `deferAutoTokens`) registers its tools DEFERRED: dispatchable by name, absent
    // from `defs()`. `tool_search` is their discovery door and registers only when at least one
    // tool is deferred — a session with a small surface pays neither the tool nor its schema.
    for d in crate::agent::mcp::discovered_tools() {
        if d.deferred {
            r.register_deferred(std::sync::Arc::from(d.tool), d.server);
        } else {
            r.register(d.tool);
        }
    }
    r.register(Box::new(crate::agent::image_read::ImageRead::new(
        root.to_path_buf(),
    )));
    // Lazy attach-only CDP client. No dead schemas without an explicitly configured target.
    #[cfg(feature = "browser")]
    if browser_configured {
        crate::agent::browser::register_browser_tools(&mut r);
    }
    install_tool_search(&mut r);
    r
}

/// Advertise `skill_load` only when at least one skill exists (else it'd be a dead tool with an
/// empty `<skills>` index). Available to every registry.
fn register_skill_load(r: &mut ToolRegistry) {
    if crate::skills::has_any() {
        r.register(Box::new(SkillLoad));
    }
}

/// Advertise `skill_refine` only when at least one skill exists — you can't refine what isn't there,
/// so it'd be pure schema-token cost on a fresh install. Destructive (rewrites + archives a skill),
/// so it rides only the AUTHORING scopes (top-level + coder/specialist), never the read-only base.
fn register_skill_refine(r: &mut ToolRegistry) {
    if r.get("skill_load").is_some() {
        r.register(Box::new(SkillRefine));
    }
}

/// Advertise `skill_forget` only when at least one skill exists, and only at TOP LEVEL — the same
/// posture as `memory_forget`: retiring a procedure is a curation decision about the user's own
/// curriculum, which a focused sub-agent has no business making mid-task.
fn register_skill_forget(r: &mut ToolRegistry) {
    if r.get("skill_load").is_some() {
        r.register(Box::new(SkillForget));
    }
}

/// The agentskill.sh marketplace tools (`skill_search`/`skill_install`) — ALWAYS available (their
/// whole point is finding a skill when you have none locally). Every registry.
fn register_skill_registry(r: &mut ToolRegistry) {
    r.register(Box::new(crate::skills::registry::SkillSearch));
    r.register(Box::new(crate::skills::registry::SkillInstall));
}

/// Advertise the Telegram tools only when a bot token + allowed chat are configured (otherwise
/// they'd be dead tools that just error). Available to every registry.
fn register_telegram(r: &mut ToolRegistry) {
    if crate::hostbot::platforms::telegram::is_configured() {
        r.register(Box::new(crate::hostbot::platforms::telegram::TelegramSend));
        r.register(Box::new(crate::hostbot::platforms::telegram::TelegramAsk));
    }
}

/// Advertise the `bot_admin` tool only when Telegram is configured — it manages the extra bots this
/// daemon hosts (add/remove/set-persona from a chat message). Top-level only: NOT added to
/// `subagent_read_only_base`/`canonical_subagent_tool`, so a specialist sub-agent can't touch the
/// bot registry or write tokens to config.
fn register_bot_admin(r: &mut ToolRegistry) {
    if crate::hostbot::platforms::telegram::is_configured() {
        r.register(Box::new(crate::hostbot::platforms::telegram::BotAdmin));
    }
}

/// Advertise the `notify` broadcast tool only when at least one outbound channel (Discord / Slack /
/// webhook) is configured — otherwise it'd be a dead tool that just errors. Every registry.
fn register_notify(r: &mut ToolRegistry) {
    if crate::channels::notify::any_configured() {
        r.register(Box::new(crate::channels::notify::Notify));
    }
}

/// The default registry PLUS the `task` sub-agent dispatch tool (top level, depth 0). The task
/// tool needs the chat creds so a spawned sub-agent can call the model itself.
///
/// `persona_create` lives ONLY here (the top-level user-facing path), never in `role_registry`:
/// a coder/tester/reviewer sub-agent has no business minting characters.
///
/// `root`: the directory the file/shell tools resolve relative paths against. `None` ⇒ the process
/// cwd (the REPL and one-shot CLI, where cwd IS the project). The `serve` daemon passes its LANE's
/// own root, because lanes run concurrently — a cwd read would hand one bot the directory another
/// bot happens to be working in.
pub fn default_registry_with_task(
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    approval_mode: crate::core::approval::ApprovalMode,
    context_window: usize,
    root: Option<PathBuf>,
) -> Result<ToolRegistry> {
    let root = match root {
        // A lane-supplied root is canonicalized so the tools, the writer lease, and the checkpoint
        // path all key off ONE spelling of the directory.
        Some(r) => r.canonicalize().unwrap_or(r),
        None => resolve_root()?,
    };
    // Decided before `base_url` moves into the delegation tools below.
    let lean = crate::core::cli_config::lean_tools_enabled(&base_url);
    let mut r = default_registry_in(&root);
    r.register(Box::new(PersonaCreate));
    r.register(Box::new(crate::agent::task_tool::TaskTool::new(
        client.clone(),
        base_url.clone(),
        api_key.clone(),
        model.clone(),
        approval_mode,
        root.clone(),
        0,
        context_window,
    )));
    // The model-callable fan-out primitive is available by default: hiding it left a normal install
    // with only the single-child `task` tool, so broad work collapsed into one oversized coder. An
    // explicit `workflow_tool: false` remains the opt-out for users who prefer the smaller schema;
    // the delegation toolset filter below can also remove the whole orchestration surface.
    if should_register_workflow() {
        r.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            client,
            base_url,
            api_key,
            model,
            approval_mode,
            0,
            root.clone(),
            context_window,
        )));
    }
    crate::agent::toolsets::apply_toolset_filter(&mut r);
    // The rarely-used built-ins ride behind `tool_search` instead of on every request; which ones
    // depends on the conversation's shape (widest turn so far), so the list stays byte-stable.
    // Only where a non-advertised tool can still be called (`lean_tools`): a grammar-locking
    // gateway would leave a deferred tool uncallable, silently.
    if lean {
        defer_builtins_for_shape(&mut r, crate::core::turn_shape::conversation_shape());
    }
    // Publish the live surface so the `<skills>` index can hide skills that `require:` an absent tool.
    publish_active_tools(&r);
    Ok(r)
}

/// Built-in tools a coding turn rarely calls, kept OFF the request and reachable through
/// `tool_search`: the orchestration and character surfaces, the memory and skill WRITE surfaces,
/// the time machine, the crawler. Measured at ~14.5 KB of the 42 KB schema block (2026-09-15).
/// Every one of them stays dispatchable — a model that needs `checkpoint` asks `tool_search` for
/// it, gets the schema back, and calls it by name.
const DEFERRED_ALWAYS: &[&str] = &[
    "workflow",
    "persona_create",
    "checkpoint",
    "checkpoint_view",
    "memory_save",
    "memory_update",
    "memory_forget",
    "memory_ask",
    "memory_profile",
    "skill_save",
    "skill_refine",
    "skill_forget",
    "skill_search",
    "skill_install",
    "team_status",
    "notify",
    // The language-server query and symbolic-edit surface, concept search and session recall:
    // 4 of ~2,800 tool calls across 79 saved sessions (2026-09-16) for 7.4 KB of schema on every
    // coding turn. `read_symbol`, `lsp_document_symbols` and `lsp_diagnostics` stay — they are
    // the ones that get called.
    "lsp_references",
    "lsp_definition",
    "lsp_hover",
    "lsp_workspace_symbol",
    "symbol_replace",
    "symbol_insert",
    "codebase_search",
    "session_recall",
    "design_guide",
    "image_read",
    "reference_read",
    "reference_compare",
    "reference_trace",
    "browser_navigate",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_eval",
    "browser_set_viewport",
    "browser_screenshot",
];

/// Deferred on top of [`DEFERRED_ALWAYS`] when the conversation is a pure question: nothing is
/// going to be moved, run in the background, or delegated.
const DEFERRED_FOR_QUESTIONS: &[&str] = &["file_move", "process", "task"];

/// The built-in names to defer for a conversation shape. `None` (no turn classified yet —
/// `serve` lanes, `prompt-size`) takes the coding set. Research keeps the crawler; everything
/// else defers it.
pub(crate) fn deferred_builtins(
    shape: Option<crate::core::turn_shape::TurnShape>,
) -> Vec<&'static str> {
    use crate::core::turn_shape::TurnShape;
    let mut out: Vec<&'static str> = DEFERRED_ALWAYS.to_vec();
    match shape {
        Some(TurnShape::Research) => {}
        Some(TurnShape::Question) => {
            out.push("web_crawl");
            out.extend_from_slice(DEFERRED_FOR_QUESTIONS);
        }
        _ => out.push("web_crawl"),
    }
    out
}

/// Defer the built-ins for `shape` and (re)install `tool_search` over everything deferred.
pub(crate) fn defer_builtins_for_shape(
    r: &mut ToolRegistry,
    shape: Option<crate::core::turn_shape::TurnShape>,
) {
    for name in deferred_builtins(shape) {
        r.defer(name, "builtin");
    }
    install_tool_search(r);
}

/// The schema bytes `tool_search` itself costs once anything is deferred (for `prompt-size`).
pub(crate) fn tool_search_schema_bytes() -> usize {
    let t = crate::agent::tool_search::ToolSearch::new(Vec::new());
    serde_json::to_string(&crate::core::types::ToolDef::function(
        t.name(),
        t.description(),
        t.parameters(),
    ))
    .map(|s| s.len())
    .unwrap_or(0)
}

/// Register `tool_search` over the registry's deferred tools (built-in and MCP), replacing any
/// earlier instance. No deferred tools ⇒ no `tool_search` — a small surface pays neither the
/// tool nor its schema.
pub(crate) fn install_tool_search(r: &mut ToolRegistry) {
    r.retain(|n| n != "tool_search");
    let entries: Vec<crate::agent::tool_search::DeferredEntry> = r
        .deferred_entries()
        .into_iter()
        .map(|(tool, server)| crate::agent::tool_search::DeferredEntry { tool, server })
        .collect();
    if !entries.is_empty() {
        r.register(Box::new(crate::agent::tool_search::ToolSearch::new(
            entries,
        )));
    }
}

/// Register the `workflow` tool unless the user explicitly opts out. The ~350-token schema is a
/// deliberate default cost: without it a fresh install has no batch fan-out primitive, so the model
/// can only serialize `task` calls or overload one coder with a whole broad job.
fn should_register_workflow() -> bool {
    crate::core::cli_config::load()
        .workflow_tool
        .unwrap_or(true)
}

/// The read-only tool base shared by EVERY sub-agent registry (role- or specialist-scoped): memory +
/// read/glob/search + `git_inspect` + skill_load/registry + LSP navigation (when enabled). NEVER
/// includes `task`, checkpoint, edit/shell, or top-level-only stateful tools. Factored out so
/// `role_registry` and `agent_registry` cannot drift.
///
/// Two deliberate ABSENCES relative to the old base:
/// - **telegram/notify**: reaching the user is the PARENT's channel. A dispatched child pinging
///   the user's Telegram mid-fan-out is noise the parent never sanctioned; whatever a child needs
///   the user to know rides its result back to the parent, which owns user communication.
/// - **web research**: a per-role grant now (see [`crate::agent::roles::RoleProfile::web`]) —
///   `argus` works entirely inside the repository, so its surface stays lean; specialists keep it
///   (see [`agent_registry`]).
fn subagent_read_only_base(root: &Path) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Box::new(MemorySearch));
    r.register(Box::new(crate::agent::design::DesignBriefTool));
    r.register(Box::new(crate::agent::design::DesignGuideTool));
    r.register(Box::new(crate::agent::design::DesignReviewTool));
    r.register(Box::new(crate::agent::design::ReferenceCompareTool));
    r.register(Box::new(crate::agent::design::ReferenceTraceTool));
    r.register(Box::new(crate::agent::image_read::ReferenceRead::new(
        root.to_path_buf(),
    )));
    // `memory_list` is read-only, so a sub-agent may inventory what is stored (that's what stops it
    // guessing) — but the WRITE trio (save/update/forget) is top-level only, registered in
    // `default_registry_in` and deliberately absent here.
    r.register(Box::new(MemoryList));
    r.register(Box::new(MemoryProfile));
    r.register(Box::new(MemoryAsk));
    r.register(Box::new(FileRead::new(root.to_path_buf())));
    r.register(Box::new(FileGlob::new(root.to_path_buf())));
    r.register(Box::new(crate::agent::search::SearchFiles::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::codebase::CodebaseSearch));
    // Read-only git queries for the roles that hold NO shell: without this, a reviewer literally
    // cannot see the diff it was dispatched to review. Closed argv schema — not a shell escape.
    r.register(Box::new(GitInspect::new(root.to_path_buf())));
    register_skill_load(&mut r);
    register_skill_registry(&mut r);
    // W17: a private, per-instance `todo_write` scratch plan (never the top-level TodoWrite — that
    // one owns the process-global list + the user's scroll region, which no sub-agent may touch).
    // Self-contained state means concurrent read-only sub-agents (planner/reviewer fan-out) never
    // race on it either.
    r.register(Box::new(crate::agent::todo::ScopedTodo::new()));
    // LSP READ navigation for sub-agents (same gate as top-level: only when manager is enabled).
    // Symbolic edit (`symbol_replace`/`symbol_insert`) is granted only to write-capable scopes
    // below — putting it here would let planner/reviewer mutate the tree.
    register_subagent_lsp_read(&mut r, root);
    r
}

/// LSP navigation tools shared by every sub-agent when LSP is enabled. Read-only (references /
/// definition / outline / workspace symbol / diagnostics / repo_map). No symbolic edit here.
fn register_subagent_lsp_read(r: &mut ToolRegistry, root: &Path) {
    if !crate::agent::lsp::LSP.is_enabled() {
        return;
    }
    r.register(Box::new(crate::agent::lsp::tools::LspReferences::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspDefinition::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspSymbolBody::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspHover::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspDocumentSymbols::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspWorkspaceSymbol::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspDiagnostics::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::repo_map::RepoMap::new(
        root.to_path_buf(),
    )));
}

/// Symbolic edit tools for write-capable sub-agents (coder / specialist with edit scope). Gated on
/// the same `LSP.is_enabled()` flag as top-level so `/lsp off` also hides them from sub-agents.
fn register_subagent_lsp_write(r: &mut ToolRegistry, root: &Path) {
    if !crate::agent::lsp::LSP.is_enabled() {
        return;
    }
    r.register(Box::new(crate::agent::lsp::tools::SymbolReplace::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::SymbolInsert::new(
        root.to_path_buf(),
    )));
}

/// The web research trio, granted per role profile / to specialists — not part of the shared base
/// (see the absence note on [`subagent_read_only_base`]).
fn register_subagent_web(r: &mut ToolRegistry) {
    use crate::agent::web_tools::{WebCrawl, WebFetch, WebSearch};
    r.register(Box::new(WebSearch));
    r.register(Box::new(WebFetch));
    r.register(Box::new(WebCrawl));
}

/// Build a READ/WRITE-scoped registry for a sub-agent of the given `role`. NEVER includes the
/// `task` tool → a sub-agent physically cannot dispatch further sub-agents (recursion guard).
///
/// Scoping is deterministic, derived from the ONE role table ([`crate::agent::roles::ROLES`], which
/// also accepts the legacy names). On top of the shared read-only base:
/// - `daedalus` (coder) → edit/shell/process + skill authoring + symbolic edit + web
/// - `themis` (tester) → shell/process + web (no `file_edit`)
/// - `metis` / `nemesis` / `clio` → read-only + web
/// - `argus` → read-only, NO web (its whole job is inside the repository)
/// - unknown → the conservative read-only base
///
/// The shell-capable roles get `process` too, and that pairing is deliberate: `shell_run`'s timeout
/// message tells the caller to re-run a long command through `process`, which used to be advice a
/// sub-agent could not act on — it had no such tool, so its only recourse was re-running the command
/// that had just timed out. The pool is keyed by execution scope (see [`crate::agent::process`]), so a
/// child can only see the handles it started; one dispatch cannot log or kill a sibling's build.
pub fn role_registry(role: &str, root: &Path) -> ToolRegistry {
    let mut r = subagent_read_only_base(root);
    let Some(p) = crate::agent::roles::canonical(role) else {
        // Unknown role → conservative read-only (already has LSP nav + git_inspect when enabled).
        return r;
    };
    if p.web {
        register_subagent_web(&mut r);
    }
    if p.history {
        // The historian's instrument: read-only recall of recent same-project conversations.
        // Deliberately NOT in the shared base — transcript access is mnemosyne's job, and every
        // other role paying its schema (and temptation) bought nothing.
        r.register(Box::new(SessionRecall));
    }
    if p.shell {
        r.register(Box::new(ShellRun::new(root.to_path_buf())));
        r.register(Box::new(crate::agent::process::Process::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::image_read::ImageRead::new(
            root.to_path_buf(),
        )));
        #[cfg(feature = "browser")]
        if crate::agent::browser::is_configured() {
            crate::agent::browser::register_browser_tools(&mut r);
        }
    }
    if p.edit {
        r.register(Box::new(FileEdit::new(root.to_path_buf())));
        r.register(Box::new(FileWrite::new(root.to_path_buf())));
        r.register(Box::new(FileMove::new(root.to_path_buf())));
        r.register(Box::new(SkillSave));
        register_skill_refine(&mut r);
        register_subagent_lsp_write(&mut r, root);
    }
    r
}

/// Build a tool registry for a dispatched SPECIALIST agent (see [`crate::agents`]). Same read-only
/// base as [`role_registry`] plus web research, plus a destructive scope derived from the persona's
/// `tools:` frontmatter:
/// - EMPTY `tools:` → **read-only** (the safe default; write/process capability must be asked for
///   explicitly — a card that relied on the old implicit coder scope adds `tools: Edit, Bash`).
/// - non-empty `tools:` → exactly those, mapped by name (Claude-Code casing accepted via the alias
///   map in [`canonical_subagent_tool`]; duplicates collapsed; a shell grant carries the scoped
///   `process` pool with it, the same pairing the built-in roles get). The `cmd_guard` floor +
///   per-op approval still apply underneath every grant.
///
/// Capability invariants (a third-party persona body is UNTRUSTED): this NEVER grants `task`
/// (recursion guard) or the top-level-only tools `todo`/`process`/`clarify`/`persona_create`/`mcp_*`
/// — they map to `None` and are silently dropped even if listed. Unknown names are ignored
/// (forward-compatible). Never calls `publish_active_tools` (only the top-level registry does).
pub fn agent_registry(def: &crate::agents::AgentDef, root: &Path) -> ToolRegistry {
    let mut r = subagent_read_only_base(root);
    // Specialists keep web research unconditionally (the pre-Pantheon surface): a persona card's
    // `tools:` names only DESTRUCTIVE grants, so there is no way for a card to ask for web back
    // if the base dropped it. Role-level web trimming is a built-in-role concern (`argus`).
    register_subagent_web(&mut r);
    // A card with NO `tools:` used to receive the full coder scope implicitly — a third-party
    // persona whose author never asked for edit/shell got both anyway. The safe default is now
    // READ-ONLY: write and process capability must be requested explicitly in frontmatter
    // (`tools: Edit, Bash, …`). Older cards that relied on the implicit scope add one line —
    // see the migration note in docs/REFERENCE.md.
    if def.tools.is_empty() {
        return r;
    }
    let mut granted: HashSet<&'static str> = HashSet::new();
    let mut has_file_edit = false;
    for raw in &def.tools {
        let Some(canon) = canonical_subagent_tool(raw) else {
            continue; // read-only (already in base), forbidden, or unknown
        };
        if !granted.insert(canon) {
            continue; // dedup
        }
        match canon {
            "file_edit" => {
                r.register(Box::new(FileEdit::new(root.to_path_buf())));
                has_file_edit = true;
            }
            "file_write" => r.register(Box::new(FileWrite::new(root.to_path_buf()))),
            "file_move" => r.register(Box::new(FileMove::new(root.to_path_buf()))),
            "shell_run" => {
                r.register(Box::new(ShellRun::new(root.to_path_buf())));
                // The scoped process pool rides with shell (same pairing as the built-in roles):
                // shell_run's timeout message says "re-run it through process", and advice a
                // child cannot act on is worse than none.
                r.register(Box::new(crate::agent::process::Process::new(
                    root.to_path_buf(),
                )));
            }
            "image_read" => r.register(Box::new(crate::agent::image_read::ImageRead::new(
                root.to_path_buf(),
            ))),
            #[cfg(feature = "browser")]
            "browser" => {
                if crate::agent::browser::is_configured() {
                    crate::agent::browser::register_browser_tools(&mut r);
                }
            }
            "skill_save" => {
                // A persona granted skill authoring gets the refine companion too (gated on any
                // skill existing — see register_skill_refine), so it can evolve, not just mint.
                r.register(Box::new(SkillSave));
                register_skill_refine(&mut r);
            }
            _ => unreachable!("canonical_subagent_tool only yields grantable destructive tools"),
        }
    }
    // Symbolic edit rides with any file-edit capability (same intent as top-level coder scope).
    if has_file_edit || granted.contains("file_write") {
        register_subagent_lsp_write(&mut r, root);
    }
    r
}

/// Map a requested tool name (a persona's `tools:` entry, possibly in Claude-Code casing) to the
/// canonical name of a GRANTABLE destructive tool, or `None`. `None` covers three cases, all meaning
/// "don't add it": read-only tools (already in the base — `Read`/`Grep`/`Glob`/…), forbidden tools
/// (`task`/`todo`/`process`/`clarify`/`persona_create`/`mcp_*`), and unknown names. This is the single
/// structural choke-point for the capability invariant: only the grantable destructive tool names
/// (`file_edit`/`file_write`/`file_move`/`shell_run`/`skill_save`) can ever be returned,
/// so nothing else can be granted to a specialist.
fn canonical_subagent_tool(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        // editing — file_edit now covers both the single and batch (multi-edit) forms, so the old
        // multi_edit aliases fold into it (a persona that still lists MultiEdit gets file_edit).
        "edit" | "file_edit" | "fileedit" | "str_replace" | "str_replace_editor" | "multiedit"
        | "multi_edit" => Some("file_edit"),
        // whole-file create/overwrite (Claude-Code's "Write" maps here now that file_write exists)
        "write" | "file_write" | "filewrite" | "write_file" | "writefile" | "create"
        | "create_file" => Some("file_write"),
        // rename / move
        "file_move" | "filemove" | "move_file" | "movefile" | "mv_file" | "mv" | "file_rename"
        | "filerename" | "rename_file" | "renamefile" | "rename" => Some("file_move"),
        // shell
        "bash" | "shell" | "shell_run" | "shellrun" | "run" | "run_command" | "terminal" => {
            Some("shell_run")
        }
        "image_read" | "imageread" => Some("image_read"),
        #[cfg(feature = "browser")]
        "browser" => Some("browser"),
        // skill authoring
        "skill_save" | "skillsave" => Some("skill_save"),
        // read-only (already in base) / forbidden / unknown → not grantable.
        _ => None,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn write_effect(root: &Path, raw: Option<&str>) -> WorkspaceEffect {
    let Some(raw) = raw else {
        return WorkspaceEffect::OpaqueWorkspace;
    };
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut ancestor = Some(joined.as_path());
    while let Some(path) = ancestor {
        match path.canonicalize() {
            Ok(canon) => {
                return if canon.starts_with(&canonical_root) {
                    WorkspaceEffect::Paths
                } else {
                    WorkspaceEffect::External
                }
            }
            Err(_) => ancestor = path.parent(),
        }
    }
    WorkspaceEffect::OpaqueWorkspace
}

/// The DIRECTORY a write will land in — what [`Tool::workspace_target`] hands the checkpoint gate so
/// it looks for a repository where the change actually happens. Mirrors `confine`'s resolution
/// (relative → `root`, absolute as-is) but stops at the parent: a not-yet-created file has no
/// directory of its own, and the parent is what a repo lookup needs anyway. Uncanonicalized —
/// `RepoContext::discover` runs git in it, which resolves the rest.
fn write_target_dir(root: &Path, raw: Option<&str>) -> Option<std::path::PathBuf> {
    let raw = raw?;
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if joined.is_dir() {
        return Some(joined);
    }
    let mut parent = joined.parent();
    while let Some(directory) = parent {
        if directory.is_dir() {
            return Some(directory.to_path_buf());
        }
        parent = directory.parent();
    }
    None
}

fn current_managed_path(root: &Path, raw: Option<&str>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    let Some(ctx) = crate::core::exec_ctx::current() else {
        return false;
    };
    let Some(placement) = ctx.placement() else {
        return false;
    };
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    joined.starts_with(crate::core::file_placement::artifact_workspace_root(
        placement.workspace_root(),
        placement.home_root(),
    )) || joined.starts_with(placement.scratch_root())
}

fn move_effect(root: &Path, args: &Value) -> WorkspaceEffect {
    let from_raw = args.get("from").and_then(Value::as_str);
    let from = if current_managed_path(root, from_raw) {
        WorkspaceEffect::External
    } else {
        write_effect(root, from_raw)
    };
    let to = if matches!(
        args.get("purpose").and_then(Value::as_str),
        Some("artifact" | "scratch")
    ) {
        WorkspaceEffect::External
    } else {
        write_effect(root, args.get("to").and_then(Value::as_str))
    };
    if matches!(from, WorkspaceEffect::Paths) || matches!(to, WorkspaceEffect::Paths) {
        // A move removes its source even when the output is managed outside the checkout.
        WorkspaceEffect::Paths
    } else if matches!(from, WorkspaceEffect::External) && matches!(to, WorkspaceEffect::External) {
        WorkspaceEffect::External
    } else {
        WorkspaceEffect::OpaqueWorkspace
    }
}

fn validate_managed_source_ancestry(source: &Path) -> Result<()> {
    for path in source.ancestors() {
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("checking managed source ancestry {}", path.display()))?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = false;
        if meta.file_type().is_symlink() || reparse {
            bail!(
                "managed source refuses symlink/reparse ancestry {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn placement_name(root: &Path, raw: &str, purpose: Option<FilePurpose>) -> String {
    if matches!(purpose, Some(FilePurpose::Artifact | FilePurpose::Scratch)) {
        raw.to_string()
    } else {
        let path = Path::new(raw);
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let mut ancestor = absolute.parent();
        let mut resolved = absolute.clone();
        while let Some(parent) = ancestor {
            if let Ok(canonical) = parent.canonicalize() {
                if let Ok(suffix) = absolute.strip_prefix(parent) {
                    resolved = canonical.join(suffix);
                }
                break;
            }
            ancestor = parent.parent();
        }
        resolved.to_string_lossy().into_owned()
    }
}

fn placement_preview(
    raw: &str,
    purpose: Option<FilePurpose>,
    destination: &ResolvedDestination,
) -> Vec<String> {
    let purpose = match purpose {
        Some(FilePurpose::Project) => "project",
        Some(FilePurpose::Artifact) => "artifact",
        Some(FilePurpose::Scratch) => "scratch",
        None => "existing destination",
    };
    vec![format!(
        "{purpose}: {raw} → {}",
        destination.actual_path().display()
    )]
}

fn require_existing_parent(target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .context("destination has no parent directory")?;
    if !parent.is_dir() {
        bail!("parent directory {} must exist", parent.display());
    }
    Ok(())
}

/// No git spawn during read-only preparation. A .git directory or worktree pointer identifies
/// repository ownership; workspace-root membership handles a checkout with no Git metadata too.
fn move_owner(root: &Path, target: &Path) -> Option<PathBuf> {
    let mut ancestor = target.parent();
    while let Some(dir) = ancestor {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        ancestor = dir.parent();
    }
    target.starts_with(root).then(|| root.to_path_buf())
}

/// Managed imports must not acquire symlink/reparse descendants or unsupported special files.
/// Refuse an unprovable oversized tree instead of following it or silently weakening placement.
fn managed_move_tree_snapshot(
    source: &Path,
) -> Result<Vec<(PathBuf, crate::core::persist::FileFingerprint)>> {
    let mut pending = vec![(source.to_path_buf(), 0usize)];
    let mut snapshot = Vec::new();
    let mut bytes = 0u64;
    while let Some((path, depth)) = pending.pop() {
        if crate::core::cancel::current().is_some_and(|token| token.is_cancelled()) {
            bail!("managed directory move cancelled during preflight");
        }
        if snapshot.len() >= 1024 || depth > 32 {
            bail!("managed directory move exceeds preflight limit (1024 entries, depth 32)");
        }
        if let Ok(relative) = path.strip_prefix(source) {
            if !relative.as_os_str().is_empty() {
                crate::core::file_placement::validate_managed_name(&relative.to_string_lossy())?;
            }
        }
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("checking managed move source {}", path.display()))?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = false;
        if meta.file_type().is_symlink() || reparse {
            bail!(
                "managed directory move refuses symlink/reparse entry {}",
                path.display()
            );
        }
        let fingerprint = if meta.is_dir() {
            let stamp = format!("directory:{}:{:?}", meta.len(), meta.modified()?);
            for entry in std::fs::read_dir(&path)? {
                if pending.len() + snapshot.len() >= 1024 {
                    bail!("managed directory move exceeds preflight limit (1024 entries)");
                }
                pending.push((entry?.path(), depth + 1));
            }
            crate::core::persist::FileFingerprint::for_bytes(stamp.as_bytes())
        } else if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            if bytes > 32 * 1024 * 1024 {
                bail!("managed directory move exceeds preflight limit (32 MiB content)");
            }
            let file = std::fs::File::open(&path)?;
            let mut contents = Vec::new();
            use std::io::Read as _;
            file.take(32 * 1024 * 1024 + 1).read_to_end(&mut contents)?;
            if contents.len() as u64 != meta.len() {
                bail!(
                    "managed directory move source changed during preflight: {}",
                    path.display()
                );
            }
            crate::core::persist::FileFingerprint::for_bytes(&contents)
        } else {
            bail!(
                "managed directory move refuses special file {}",
                path.display()
            );
        };
        snapshot.push((path, fingerprint));
    }
    snapshot.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(snapshot)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing required string arg '{key}'"))
}

/// Resolve `path` (relative → `base`) into an absolute, canonicalized path.
///
/// NOTE: the workspace confinement boundary was REMOVED by user request — file/shell tools may now
/// touch paths ANYWHERE on disk, not just under `base`. `base` is retained only as the anchor for
/// resolving RELATIVE paths (so `foo.rs` still means `<base>/foo.rs` and `../sibling/x` reaches the
/// sibling project the user asked to work in). Absolute paths pass through unchanged. No escape or
/// symlink-target check is performed. SECURITY: any web page / tool output the agent reads can now
/// steer a write or delete to an arbitrary path — this is the accepted trade-off for the removal.
/// `must_exist`: canonicalize the full path (errors if missing); else canonicalize the parent and
/// re-join the file name (so a not-yet-existing target still resolves).
pub(crate) fn confine(base: &Path, path: &str, must_exist: bool) -> Result<PathBuf> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    let resolved = if must_exist {
        joined
            .canonicalize()
            .with_context(|| format!("resolving {}", joined.display()))?
    } else {
        let parent = joined.parent().unwrap_or(base);
        let cparent = parent
            .canonicalize()
            .with_context(|| format!("resolving parent of {}", joined.display()))?;
        let fname = joined.file_name().context("path has no file name")?;
        cparent.join(fname)
    };
    Ok(resolved)
}

/// Translate a glob (`*`, `**`, `**/`, `?`) into an anchored regex over `/`-separated paths.
fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        re.push_str("(?:.*/)?"); // **/ → zero-or-more dirs
                        i += 3;
                        continue;
                    }
                    re.push_str(".*"); // ** → across dirs
                    i += 2;
                    continue;
                }
                re.push_str("[^/]*"); // * → within a path segment
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
        i += 1;
    }
    re.push('$');
    re
}

/// Compile a glob into a `Regex`, honoring smart-case (`ci` prepends the `(?i)` inline flag). The
/// body from [`glob_to_regex`] is already `^…$`-anchored, so `(?i)^…$` is valid.
fn compile_glob(glob: &str, ci: bool) -> Result<regex::Regex> {
    let body = glob_to_regex(glob);
    let full = if ci { format!("(?i){body}") } else { body };
    regex::Regex::new(&full).context("invalid glob pattern")
}

/// The intended NAME behind a pattern: its last `/`-separated segment with glob metacharacters
/// stripped, lower-cased. `src/**/*.rs` → `.rs`… no — we want the meaningful name, so we take the
/// last segment that still carries literal characters. `**/mini_project` → `mini_project`,
/// `src/**/*.rs` → `.rs` is useless, so fall back to the last segment that ISN'T pure glob. Used to
/// rank hits and to seed the fuzzy fallback.
fn last_literal_segment(pattern: &str) -> String {
    let norm = pattern.replace('\\', "/");
    let strip = |s: &str| s.trim_matches(|c| c == '*' || c == '?').to_string();
    // Walk segments right-to-left; the first one that has a literal (non-glob) character wins.
    for seg in norm.rsplit('/') {
        let lit = strip(seg);
        if !lit.is_empty() {
            return lit.to_ascii_lowercase();
        }
    }
    String::new()
}

// ── file_glob walk engine ────────────────────────────────────────────────────
// A BOUNDED, PARALLEL directory walk (ripgrep's `ignore` crate, same engine as `search_files`)
// replaces the old single-threaded recursive DFS. The old walk had three fatal bugs on a real
// Windows tree: (1) it counted MATCHES not NODES, so a rare-match glob scanned the WHOLE drive
// before returning; (2) no wall-clock or node ceiling, so it could hang for minutes (the model then
// gave up and shelled out to `where`); (3) it followed junctions/symlinks and swallowed errors, so
// a reparse-point loop under `C:\Users` could spin forever. The engine below fixes all three:
// node-count + wall-clock BUDGET (checked per entry), `follow_links(false)` +
// `same_file_system(true)` (no junction loops, no crossing into other volumes), a bounded depth, and
// optional subtree pruning of heavy/system dirs for BROAD (home/drive-wide) searches.

/// Result of a bounded walk: the collected paths and WHY it stopped (so the tool can tell the model
/// the list may be incomplete and it should narrow, instead of trusting a silently-truncated list).
struct WalkOutcome {
    paths: Vec<PathBuf>,
    /// The node or wall-clock budget was exhausted before the tree was fully scanned.
    budget_hit: bool,
    /// The per-walk match cap was reached (more matches almost certainly exist).
    capped: bool,
}

/// Node-count + wall-clock ceiling shared across the parallel walk threads. `tick()` is called once
/// per visited entry; when either limit trips it flips an atomic flag and every thread quits.
struct WalkBudget {
    nodes: AtomicUsize,
    max_nodes: usize,
    deadline: Instant,
    tripped: std::sync::atomic::AtomicBool,
}
impl WalkBudget {
    fn new(max_nodes: usize, max_wall: Duration) -> Self {
        Self {
            nodes: AtomicUsize::new(0),
            max_nodes,
            deadline: Instant::now() + max_wall,
            tripped: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// Returns `true` while there is budget left; flips `tripped` and returns `false` once spent.
    /// The wall-clock is only polled every 512 nodes (per thread) so `Instant::now()` isn't on the
    /// hot path of every single entry.
    fn alive(&self) -> bool {
        if self.tripped.load(Ordering::Relaxed) {
            return false;
        }
        let n = self.nodes.fetch_add(1, Ordering::Relaxed);
        if n >= self.max_nodes || (n.is_multiple_of(512) && Instant::now() >= self.deadline) {
            self.tripped.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }
    fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Relaxed)
    }
}

/// Directory names whose ENTIRE subtree is pruned during a BROAD (home-/drive-wide) walk — package
/// caches, VCS internals, build output, and OS/system folders that never hold user source and would
/// otherwise burn the whole budget. NOT applied to a NARROW anchored walk (there the user pointed at
/// a specific dir and asked to see everything, build output included).
static BROAD_PRUNE: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".hg",
    ".svn",
    "vendor",
    "dist",
    "build",
    "out",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".gradle",
    ".m2",
    ".nuget",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".terraform",
    ".tox",
    ".idea",
    ".vscode",
    "AppData",
    "$Recycle.Bin",
    "System Volume Information",
    "Windows",
    "WinSxS",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$WinREAgent",
    ".Trash",
];

/// Run a bounded, parallel walk over `roots`, keeping every entry (file OR directory) for which
/// `keep(&full_path, rel_slashed)` returns `true`. `rel_slashed` is the path relative to the walk
/// root that produced it, forward-slashed (what a glob regex matches against). Each root carries its
/// OWN max depth — the cwd is walked deep, ancestors shallow (so a name-find sees siblings and the
/// ancestor dir itself without re-scanning the whole tree). `prune` toggles the [`BROAD_PRUNE`]
/// subtree skipping (on for a broad name-find, off for a structured anchored glob). `match_cap`
/// bounds how many kept paths we retain (ranking later trims to a display count). All roots SHARE one
/// budget, so the global node/wall-clock ceiling bounds the total work across every root.
fn bounded_walk<F>(
    roots: &[(PathBuf, usize)],
    prune: bool,
    git_aware: bool,
    match_cap: usize,
    budget: &WalkBudget,
    keep: F,
) -> WalkOutcome
where
    F: Fn(&Path, &str) -> bool + Sync,
{
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let kept = AtomicUsize::new(0);
    for (root, depth) in roots.iter() {
        if budget.is_tripped() {
            break;
        }
        let mut wb = WalkBuilder::new(root);
        // Default: SEE hidden files/dirs (dotfiles, .env) and ignored paths — the user asked for
        // everything. `git_aware` (file_glob `ignore:true`) flips to search_files' semantics:
        // `.gitignore`/`.ignore` honoured, hidden entries skipped, heavy dirs pruned below.
        wb.follow_links(false) // never chase junctions/symlinks → no reparse-point loops
            .same_file_system(true) // don't cross into other drives/mounts mid-walk
            .hidden(git_aware)
            .git_ignore(git_aware)
            .git_global(git_aware)
            .git_exclude(git_aware)
            .ignore(git_aware)
            .parents(git_aware)
            .require_git(false)
            .max_depth(Some(*depth));
        let tx = tx.clone();
        let keep = &keep;
        let kept = &kept;
        wb.build_parallel().run(|| {
            let tx = tx.clone();
            let root = root.clone();
            Box::new(move |res| {
                if !budget.alive() {
                    return WalkState::Quit;
                }
                let dent = match res {
                    Ok(d) => d,
                    Err(_) => return WalkState::Continue, // swallow permission/loop errors, keep going
                };
                let path = dent.path();
                let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                // Prune heavy/system subtrees on a broad walk (depth>0 so we don't prune a root the
                // caller explicitly pointed at). This is the single highest-leverage speed-up.
                if (prune || git_aware) && is_dir && dent.depth() > 0 {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if BROAD_PRUNE.iter().any(|b| b.eq_ignore_ascii_case(name)) {
                            return WalkState::Skip;
                        }
                    }
                }
                if kept.load(Ordering::Relaxed) >= match_cap {
                    return WalkState::Quit;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if keep(path, &rel) {
                    if kept.fetch_add(1, Ordering::Relaxed) >= match_cap {
                        return WalkState::Quit;
                    }
                    let _ = tx.send(path.to_path_buf());
                }
                WalkState::Continue
            })
        });
    }
    drop(tx);
    let mut paths: Vec<PathBuf> = rx.into_iter().collect();
    paths.sort();
    paths.dedup();
    let capped = kept.load(Ordering::Relaxed) >= match_cap;
    WalkOutcome {
        paths,
        budget_hit: budget.is_tripped(),
        capped,
    }
}

/// Read the user's home directory from the environment WITHOUT pulling the `dirs` crate (its
/// `winsafe`-adjacent deps break our windows-gnu single-static-binary posture). `USERPROFILE` on
/// Windows, `HOME` elsewhere; falls back to the other if the primary is unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Depth the WORKING DIR is walked to on a name-find — deep enough to reach a file buried in a
/// project, bounded so it can't run away.
const SEED_DEPTH_CWD: usize = 40;
/// Depth ANCESTORS + well-known folders are walked to — shallow, so we surface siblings, the ancestor
/// dir itself, and top-of-project files without re-scanning an entire home tree.
const SEED_DEPTH_WIDE: usize = 6;

/// The FAMILIAR-LOCATION seed list for a bare-name find (P1.1), each paired with its OWN walk depth:
/// the working dir (deep), its ancestors (shallow, up to but NOT past home — we never seed a bare
/// drive root, that's the pathology we're killing), and the well-known user folders
/// (Desktop/Documents/Downloads/Projects, incl. the OneDrive-redirected ones Windows creates —
/// shallow). Reduced to a MINIMAL set — any seed that lives inside another is dropped, so we never
/// walk the same subtree twice. This is why a model can ask for a bare name and we find it on the
/// Desktop or one folder up without scanning the entire drive.
fn seed_dirs(root: &Path) -> Vec<(PathBuf, usize)> {
    let home = home_dir();
    let mut seeds: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), SEED_DEPTH_CWD)];
    // Climb ancestors, but STOP before home and before a drive/filesystem root — we must never seed a
    // bare drive root (`C:\`) or home itself, because pruning would then skip AppData/Windows/etc. and
    // the walk would still be enormous. The well-known user folders below (Desktop/Documents/…) are
    // the widen targets instead. Capped at a handful of levels so a deeply-nested cwd doesn't seed a
    // huge ancestor. Each ancestor is walked SHALLOW.
    let mut cur = root.to_path_buf();
    for _ in 0..6 {
        let Some(parent) = cur.parent().map(|p| p.to_path_buf()) else {
            break;
        };
        // Don't seed home or a root: parent-of-parent None means `parent` is a drive/fs root.
        if home.as_deref() == Some(parent.as_path()) || parent.parent().is_none() {
            break;
        }
        seeds.push((parent.clone(), SEED_DEPTH_WIDE));
        cur = parent;
    }
    if let Some(home) = home {
        for sub in [
            "Desktop",
            "Documents",
            "Downloads",
            "Projects",
            "Code",
            "src",
            "dev",
            "repos",
        ] {
            let p = home.join(sub);
            if p.is_dir() {
                seeds.push((p, SEED_DEPTH_WIDE));
            }
        }
        // OneDrive-redirected known folders (Windows silently moves Desktop/Documents there).
        let onedrive = home.join("OneDrive");
        if onedrive.is_dir() {
            for sub in ["Desktop", "Documents"] {
                let p = onedrive.join(sub);
                if p.is_dir() {
                    seeds.push((p, SEED_DEPTH_WIDE));
                }
            }
        }
    }
    minimal_roots(seeds)
}

/// Reduce a seed list to non-overlapping roots. FIRST-SEEN WINS: seeds are proximity-ordered (the cwd
/// is first and is walked DEEP), so we never drop an earlier root in favor of a later one — a later
/// seed is dropped only when it is EQUAL TO or a DESCENDANT OF an already-kept root (already covered
/// downward). A later seed that is an ANCESTOR of a kept root is retained (walked shallow): it surfaces
/// parent/sibling names the deep cwd walk can't reach upward, and the small subtree overlap is bounded
/// by that seed's shallow depth + the shared budget. The old logic dropped the deep cwd whenever an
/// ancestor (e.g. the temp/AppData dir the cwd sits under) came along, so the cwd's own files were
/// walked shallow-and-late and lost to the budget — the exact bug this ordering fixes.
fn minimal_roots(seeds: Vec<(PathBuf, usize)>) -> Vec<(PathBuf, usize)> {
    let seeds: Vec<(PathBuf, usize)> = seeds.into_iter().filter(|(p, _)| p.is_dir()).collect();
    let mut out: Vec<(PathBuf, usize)> = Vec::new();
    for (s, d) in seeds {
        // Drop only if already covered downward by an earlier (higher-proximity) root.
        let covered = out.iter().any(|(o, _)| s == *o || s.starts_with(o));
        if covered {
            continue;
        }
        out.push((s, d));
    }
    out
}

/// Score a candidate path for ranking (P1.3). Higher is better. Combines: filename fuzzy similarity
/// to the needle (Jaro-Winkler, separator-insensitive) · an EXACT-name bonus · proximity to the
/// working dir (shared path-prefix depth) · a small recency nudge from mtime. Used both to order
/// glob hits and to pick the fuzzy-fallback suggestions, so the BEST answer is always line one.
fn score_path(p: &Path, needle: &str, root: &Path, now: std::time::SystemTime) -> f64 {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let strip = |s: &str| {
        s.chars()
            .filter(|c| !matches!(c, '_' | '-' | ' ' | '.'))
            .collect::<String>()
    };
    let mut score = if needle.is_empty() {
        0.5
    } else {
        let sim = strsim::jaro_winkler(needle, &name)
            .max(strsim::jaro_winkler(&strip(needle), &strip(&name)));
        if name == needle {
            1.5 // exact filename — unbeatable
        } else if name.contains(needle) || strip(&name).contains(&strip(needle)) {
            sim.max(0.95)
        } else {
            sim
        }
    };
    // Proximity: how many leading path components the candidate shares with the working dir. A file
    // right next to the cwd beats an identically-named one three folders away.
    let shared = root
        .components()
        .zip(p.components())
        .take_while(|(a, b)| a == b)
        .count();
    score += (shared as f64) * 0.02;
    // Recency: files touched in the last week get a tiny nudge (breaks ties toward live work).
    if let Ok(meta) = p.metadata() {
        if let Ok(mtime) = meta.modified() {
            if let Ok(age) = now.duration_since(mtime) {
                if age < Duration::from_secs(7 * 24 * 3600) {
                    score += 0.03;
                }
            }
        }
    }
    score
}

/// Smart-case: a glob with NO uppercase letter matches case-INSENSITIVELY (the common intent — a
/// model types `readme.md` and means `README.md`); any uppercase makes the whole match
/// case-sensitive. Mirrors ripgrep/`search_files` ergonomics.
fn smart_case_insensitive(pattern: &str) -> bool {
    !pattern.chars().any(|c| c.is_uppercase())
}

/// Split a glob into (anchor_dir, sub_pattern): the leading run of segments with NO glob
/// metacharacter (`*`/`?`) becomes a directory anchor, the rest is matched relative to it. This is
/// what lets `file_glob` reach OUTSIDE the working dir — `../snakegame/**/*.js` anchors at the
/// sibling project, an absolute `C:/x/**/*.rs` anchors at the drive path. A pure-literal pattern
/// (no glob char) anchors at its parent and matches the final name. `..` and absolute paths pass
/// through; a relative anchor is joined to `root`.
fn glob_anchor(root: &Path, pattern: &str) -> (PathBuf, String) {
    let norm = pattern.replace('\\', "/");
    let segs: Vec<&str> = norm.split('/').collect();
    let glob_idx = segs.iter().position(|s| s.contains('*') || s.contains('?'));
    let (lit_segs, rest_segs): (&[&str], &[&str]) = match glob_idx {
        Some(i) => (&segs[..i], &segs[i..]),
        None if segs.is_empty() => (&[], &[]),
        // Pure literal path: anchor at the parent, match the final segment as a name.
        None => (&segs[..segs.len() - 1], &segs[segs.len() - 1..]),
    };
    let lit_str = lit_segs.join("/");
    let anchor = if lit_str.is_empty() {
        root.to_path_buf()
    } else {
        let lp = Path::new(&lit_str);
        // Absolute (unix `/…`, Windows `C:/…`) → use as-is; otherwise resolve against root (`..` ok).
        let is_abs = lp.is_absolute()
            || lit_str.starts_with('/')
            || (lit_str.len() >= 2 && lit_str.as_bytes()[1] == b':');
        if is_abs {
            lp.to_path_buf()
        } else {
            root.join(lp)
        }
    };
    let anchor = anchor.canonicalize().unwrap_or(anchor);
    (anchor, rest_segs.join("/"))
}

/// Strip a Windows verbatim/extended-length prefix (`\\?\` or `\\?\UNC\`) so a canonicalized path
/// displays as a normal `C:\…` path instead of the `//?/C:/…` a naive `\`→`/` replace would produce.
/// No-op on non-Windows paths. Kept as a plain string transform so it works on the `Cow` we already
/// have without another allocation-path branch.
fn strip_verbatim(s: &str) -> &str {
    s.strip_prefix(r"\\?\UNC\")
        .map(|_| s) // UNC verbatim is rare; leave it (rewriting it to `\\server` is lossy to reason about)
        .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(s))
}

/// Display a matched path relative to `root` when it's under the working dir, else as a cleaned
/// absolute path (so results outside the workspace are still copy-pasteable). Windows verbatim
/// prefixes (`\\?\`, left behind by `canonicalize`) are stripped so the model sees `C:/…`, not `//?/C:/…`.
fn display_path(root: &Path, p: &Path) -> String {
    // Compare on the verbatim-stripped forms so a canonicalized `p` still strips against a plain `root`.
    let ps = p.to_string_lossy();
    let ps = strip_verbatim(&ps);
    let rs = root.to_string_lossy();
    let rs = strip_verbatim(&rs);
    let rel = ps
        .strip_prefix(rs)
        .map(|r| r.trim_start_matches(['\\', '/']))
        .filter(|r| !r.is_empty() && ps.len() != rs.len());
    match rel {
        Some(r) => r.replace('\\', "/"),
        None => ps.replace('\\', "/"),
    }
}

// ── memory_search ──────────────────────────────────────────────────────────

struct MemorySearch;
impl Tool for MemorySearch {
    fn name(&self) -> &str {
        "memory_search"
    }
    fn description(&self) -> &str {
        "Find user/project facts by lexical/semantic match; default workspace + global. \
         <user_memory> holds style/global prefs only; overall preferences -> memory_profile. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "what to recall"},
                "limit": {"type": "integer", "description": "max hits (default 5)"},
                "scope": {"type": "string", "enum": ["current", "all", "global"], "description": "current project + global (default), all zones, or global only"},
                "category": {"type": "string", "enum": ["bug-history", "failed-attempt", "success-pattern", "arch-decision", "command", "security-rule", "deploy-note", "codebase"], "description": "filter project knowledge by kind"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    // NOT concurrency-safe: memory::search has a WRITE side effect (record_reuse → record_retrieval,
    // a read-modify-write of per-fact files, plus the shared embedding cache under `dense`). Two
    // parallel searches would race and lose reinforcement counts / clobber the cache, so keep it on
    // the serial path even though it reads like a read-only tool.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = str_arg(args, "query")?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20) as usize;
        let sel = match args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("current")
        {
            "" | "current" => crate::memory::ScopeSel::default_view(),
            "all" => crate::memory::ScopeSel::All,
            "global" => crate::memory::ScopeSel::Global,
            other => {
                return Ok(format!(
                    "error: unknown scope '{other}' (use current|all|global)"
                ))
            }
        };
        let cat = match args
            .get("category")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => match crate::memory::category::Category::parse(s) {
                Some(c) => Some(c),
                None => return Ok(format!("error: unknown category '{s}'")),
            },
        };
        // record_reuse (the reinforcement write side-effect) is intentionally kept on the unfiltered
        // `search_scoped` path; a category-filtered recall is an inspection, not organic reuse.
        let hits = if cat.is_some() {
            crate::memory::search_filtered_scoped_cat(query, limit, None, cat, &sel)?
        } else {
            crate::memory::search_scoped(query, limit, &sel)?
        };
        if hits.is_empty() {
            return Ok(format!("(no memory matches '{query}')"));
        }
        Ok(format_memory_hits(
            &hits,
            crate::memory::settings().search_max_tokens,
        ))
    }
}

/// Render search hits under a hard token budget (chars/4, same estimator as the frozen core):
/// hits are already best-first, so trailing ones are cut and counted rather than blowing the turn.
fn format_memory_hits(hits: &[crate::memory::Hit], max_tokens: usize) -> String {
    let mut s = String::new();
    let mut used = 0usize;
    for (shown, h) in hits.iter().enumerate() {
        let body: String = h.entry.body.chars().take(200).collect();
        let zone = match h.entry.scope.as_deref() {
            Some(z) => format!(" [p:{z}]"),
            None => String::new(),
        };
        // Surface the CoALA content type when the fact HAS one, tagged with its neural bucket
        // (episodic/semantic/procedural) so the agent can weight a past bug vs a durable decision.
        let cat = match h.entry.category {
            crate::memory::category::Category::None => String::new(),
            c => format!(" [{}/{}]", c.kind().as_str(), c.as_str()),
        };
        let line = format!(
            "[{:.2}] {} ({}){}{} — {}\n",
            h.score,
            h.entry.name,
            h.entry.mtype.as_str(),
            zone,
            cat,
            body.replace('\n', " ")
        );
        let cost = crate::memory::render::est_tokens(&line);
        if used + cost > max_tokens && shown > 0 {
            s.push_str(&format!(
                "(+{} more hit(s) over the token budget — narrow the query)\n",
                hits.len() - shown
            ));
            break;
        }
        used += cost;
        s.push_str(&line);
    }
    s.trim_end().to_string()
}

// ── memory_profile (B2) ──────────────────────────────────────────────────────

struct MemoryProfile;
impl Tool for MemoryProfile {
    fn name(&self) -> &str {
        "memory_profile"
    }
    fn description(&self) -> &str {
        "Read the user's working profile: verbosity/autonomy/tools/stack/language/frustrations, \
         per-dimension confidence and cited facts. Use for defaults; fact lookup -> memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        let p = crate::memory::build_profile()?;
        Ok(serde_json::to_string(&p)?)
    }
}

// ── memory_ask (B3 dialectic) ─────────────────────────────────────────────────

struct MemoryAsk;
impl Tool for MemoryAsk {
    fn name(&self) -> &str {
        "memory_ask"
    }
    fn description(&self) -> &str {
        "Answer one question about the user from memory; abstain rather than guess. \
         General fact lookup -> memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"question": {"type": "string"}},
            "required": ["question"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let q = str_arg(args, "question")?;
        let a = crate::memory::answer_about_user(q)?;
        let mut s = a.text.clone();
        if !a.basis.is_empty() {
            let cited: Vec<String> = a.basis.iter().take(3).map(|b| b.name.clone()).collect();
            s.push_str(&format!("\n(based on: {})", cited.join("; ")));
        }
        Ok(s)
    }
}

// ── memory_list ───────────────────────────────────────────────────────────────

/// Inventory of what is stored, WITHOUT a query. `memory_search` needs a query, so before this tool
/// existed the agent had no way to answer "what do you remember about me?" — it could only guess
/// query terms and report whatever happened to match, which reads as confidently not knowing its
/// own state. This is the tool that makes the store legible.
/// The fetch half of "continue the most recent session": a clipped digest of one saved
/// conversation from the pool the `<sessions>` block lists. Read-only by design — restoring a full
/// transcript rewrites live history, which stays a USER move (`/resume`), never a tool's.
struct SessionRecall;
impl Tool for SessionRecall {
    fn name(&self) -> &str {
        "session_recall"
    }
    fn description(&self) -> &str {
        "Read a saved session's opening/latest exchanges to continue prior work. Default newest \
         project session; name from <sessions> selects another. Only user /resume restores full history. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "optional saved session name; default newest for this project"}
            },
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        crate::core::session_store::session_digest(args.get("name").and_then(|v| v.as_str()))
    }
}

struct MemoryList;
impl Tool for MemoryList {
    fn name(&self) -> &str {
        "memory_list"
    }
    fn description(&self) -> &str {
        "List fact id/type/zone/category/summary without a query; audit or obtain \
         memory_update/memory_forget ids. Topic search -> memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {"type": "string", "enum": ["current", "all", "global", "project"], "description": "current project + global (default), all zones, global only, or project only"},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"], "description": "optional memory type filter"},
                "limit": {"type": "integer", "description": "max entries (default 50, max 200)"},
                "include_archived": {"type": "boolean", "description": "recoverable archive, not live store (default false)"}
            },
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let archived = args
            .get("include_archived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mtype = match args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => match crate::memory::store::MemoryType::parse_strict(s) {
                Some(t) => Some(t),
                None => {
                    return Ok(format!(
                        "error: unknown type '{s}' (user|feedback|project|reference)"
                    ))
                }
            },
        };
        let sel = match args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("current")
        {
            "" | "current" => crate::memory::ScopeSel::default_view(),
            "all" => crate::memory::ScopeSel::All,
            "global" => crate::memory::ScopeSel::Global,
            "project" => crate::memory::ScopeSel::Project(crate::core::config::project_slug()),
            other => {
                return Ok(format!(
                    "error: unknown scope '{other}' (use current|all|global|project)"
                ))
            }
        };
        crate::memory::inventory(&sel, mtype, limit, archived)
    }
}

// ── memory_save ───────────────────────────────────────────────────────────────

/// Deliberate write. Distinct from the passive learning pipeline: when the user says "remember
/// this", the agent should be able to act on it in the same turn instead of hoping an extractor
/// picks it up later.
struct MemorySave;
impl Tool for MemorySave {
    fn name(&self) -> &str {
        "memory_save"
    }
    fn description(&self) -> &str {
        "Save durable user-requested/project fact, not turn notes. Check memory_list/memory_search; \
         existing topic -> memory_update. Default project; global for universal facts."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "short unique kebab-case title; becomes id"},
                "body": {"type": "string", "description": "self-contained fact; absolute dates, not relative dates"},
                "description": {"type": "string", "description": "optional one-line recall-ranking summary"},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"], "description": "user=identity; feedback=work preferences; project=codebase state/goals (default); reference=external pointers"},
                "scope": {"type": "string", "description": "global=all workspaces; omitted/project=current project"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    // Writes a file + is the kind of thing the user wants to see; keep it off the parallel path.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        if body.trim().is_empty() {
            return Ok("error: empty body — nothing to remember".to_string());
        }
        let desc = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let t = match args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => crate::memory::store::MemoryType::Project,
            Some(s) => match crate::memory::store::MemoryType::parse_strict(s) {
                Some(t) => t,
                None => {
                    return Ok(format!(
                        "error: unknown type '{s}' (user|feedback|project|reference)"
                    ))
                }
            },
        };
        // Global only when explicitly asked; anything else (absent, "project", "current") stays in
        // this workspace's zone — a fact learned here must not leak into unrelated projects.
        let scope = match args.get("scope").and_then(|v| v.as_str()).map(str::trim) {
            Some(s) if s.eq_ignore_ascii_case("global") => None,
            _ => Some(crate::core::config::project_slug()),
        };
        match crate::memory::store::add_scoped(name, desc, t, body.trim(), scope.as_deref()) {
            Ok(id) => Ok(format!(
                "saved memory '{id}' (type={}, {}). Use memory_update to revise it.",
                t.as_str(),
                scope
                    .as_deref()
                    .map(|s| format!("zone {s}"))
                    .unwrap_or_else(|| "global".into())
            )),
            // The store refuses to overwrite; surface the "update instead" path rather than failing.
            Err(e) => Ok(format!("error: {e}")),
        }
    }
}

// ── memory_update ─────────────────────────────────────────────────────────────

struct MemoryUpdate;
impl Tool for MemoryUpdate {
    fn name(&self) -> &str {
        "memory_update"
    }
    fn description(&self) -> &str {
        "Revise fact by memory_list id; only supplied fields change, id stays fixed. \
         Correct wording/type/global-project scope here, not with duplicate memory_save."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "id or exact name from memory_list"},
                "body": {"type": "string", "description": "full replacement fact"},
                "description": {"type": "string", "description": "replacement summary; empty clears"},
                "name": {"type": "string", "description": "replacement title; id unchanged"},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"]},
                "scope": {"type": "string", "description": "global=everywhere; project=current workspace"}
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let id = str_arg(args, "id")?;
        let mtype = match args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => match crate::memory::store::MemoryType::parse_strict(s) {
                Some(t) => Some(t),
                None => {
                    return Ok(format!(
                        "error: unknown type '{s}' (user|feedback|project|reference)"
                    ))
                }
            },
        };
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .map(|s| {
                if s.eq_ignore_ascii_case("global") || s.is_empty() {
                    None
                } else {
                    Some(crate::core::config::project_slug())
                }
            });
        let patch = crate::memory::store::EntryPatch {
            name: args
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            mtype,
            body: args
                .get("body")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            scope,
            preserve_updated: false, // a content update IS a touch — stamp the aging clock
            // Not exposed to the model: dropping a `supersedes:` claim revives a retired fact, which
            // is `aizen memory revive` — a human decision with its own audit line, not a field an
            // edit tool gets to flip as a side effect of rewording a body.
            clear_supersede: false,
        };
        if patch.is_empty() {
            return Ok(
                "error: nothing to update — pass at least one of body/description/name/type/scope"
                    .to_string(),
            );
        }
        let e = match crate::memory::resolve_entry(id) {
            Ok(e) => e,
            Err(err) => return Ok(format!("error: {err}")),
        };
        match crate::memory::store::update(&e, &patch) {
            Ok(()) => Ok(format!("updated memory '{}'", e.id)),
            Err(err) => Ok(format!("error: {err}")),
        }
    }
}

// ── memory_forget ─────────────────────────────────────────────────────────────

/// Retire a fact. Deliberately a SOFT delete (archive, restorable by the human) and
/// approval-gated: the store's premise is that a fact is never lost, and a model concluding
/// something is obsolete is exactly the case where that premise earns its keep.
struct MemoryForget;
impl Tool for MemoryForget {
    fn name(&self) -> &str {
        "memory_forget"
    }
    fn description(&self) -> &str {
        "Retire a user-identified wrong/obsolete fact to a user-restorable archive, not erase. \
         Prefer memory_update if outdated; forget only if it should stop applying. Ids from memory_list."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "id or exact name from memory_list"},
                "reason": {"type": "string", "description": "why it should stop applying; shown at approval"}
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let id = str_arg(args, "id")?;
        let e = match crate::memory::resolve_entry(id) {
            Ok(e) => e,
            Err(err) => return Ok(format!("error: {err}")),
        };
        match crate::memory::store::retire(&e) {
            Ok(archived) => Ok(format!(
                "retired memory '{}' → archived as '{archived}' (user can restore: `aizen memory restore {archived}`)",
                e.id
            )),
            Err(err) => Ok(format!("error: {err}")),
        }
    }
}

// ── file_read ──────────────────────────────────────────────────────────────

struct FileRead {
    root: PathBuf,
}
impl FileRead {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
/// Cap on entries honoured in one `files:[…]` batch read. Enough to cover every hit of a search
/// turn in one call; anything past it is reported (never silently dropped) so the model knows to
/// call again.
const MULTI_READ_MAX_FILES: usize = 8;

/// Bound argument cloning as well as file bytes, including otherwise ignored model keys.
fn bounded_native_arguments(args: &Value, tool: &str) -> Result<()> {
    struct InputLimit(usize);
    impl std::io::Write for InputLimit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > PREPARED_EDIT_MAX_BYTES {
                return Err(std::io::Error::other("4 MiB native argument limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(InputLimit(0), args)
        .with_context(|| format!("{tool} arguments exceed the 4 MiB preparation limit"))
}

/// An unprovable identity may use the legacy global lane, never a narrowed native lane.
fn prepared_native_resource(
    target: &Path,
    ctx: Option<&ExecutionContext>,
) -> Result<Option<NativeResource>> {
    match NativeResource::resolve(target) {
        Ok(resource) => Ok(Some(resource)),
        Err(error) if ctx.is_some_and(|ctx| ctx.native_scope().is_some()) => Err(error)
            .context("native-scoped call requires an unambiguous file resource; nothing executed"),
        Err(_) => Ok(None),
    }
}

fn check_native_path(
    ctx: Option<&ExecutionContext>,
    root: &Path,
    name: &str,
    writable: bool,
) -> Result<()> {
    let Some(scope) = ctx.and_then(ExecutionContext::native_scope) else {
        return Ok(());
    };
    let path = Path::new(name);
    NativeResource::validate_spelling(path)?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.canonicalize()?.join(path)
    };
    let resource = NativeResource::resolve(&absolute)?;
    if writable {
        scope.check_resource(&resource)
    } else {
        scope.check_read(&resource)
    }
}

/// Frozen execution state cannot be transplanted into a narrower or differently owned lane.
/// Ordinary direct calls keep their legacy compatibility; any ambient native scope fails closed.
fn verify_frozen_native_context(frozen: Option<&ExecutionContext>) -> Result<()> {
    let ambient = crate::core::exec_ctx::current();
    match (
        frozen.and_then(ExecutionContext::overlay),
        ambient.as_ref().and_then(ExecutionContext::overlay),
    ) {
        (Some(frozen_overlay), Some(ambient_overlay))
            if frozen_overlay.same_handle(ambient_overlay) => {}
        (None, None) => {}
        _ => bail!("prepared file call cannot cross live/private overlay or owner/base boundaries; prepare again"),
    }
    let frozen_scope = frozen.and_then(ExecutionContext::native_scope);
    let ambient_scope = ambient.as_ref().and_then(ExecutionContext::native_scope);
    match (frozen_scope, ambient_scope) {
        (None, Some(_)) => {
            bail!("unscoped prepared file call cannot execute under an ambient native scope; prepare again");
        }
        (Some(frozen_scope), Some(ambient_scope)) => {
            if frozen.unwrap().resource_scope() != ambient.as_ref().unwrap().resource_scope()
                || !frozen_scope.same_authority(ambient_scope)
            {
                bail!("prepared native file scope or owner differs from the ambient execution context; prepare again");
            }
        }
        (Some(_), None) => {
            bail!("prepared native file call lost its scoped ambient execution context; prepare again");
        }
        _ => {}
    }
    Ok(())
}

/// A read freezes the leaf identity, unlike a resource lease which names a replaceable file slot.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FrozenReadIdentity(u64, u64);

impl FrozenReadIdentity {
    fn capture(path: &Path) -> Result<Option<Self>> {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() {
            return Ok(None);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Some(Self(metadata.dev(), metadata.ino())))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
            use windows_sys::Win32::Storage::FileSystem::{
                CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
                FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
                FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            };
            let name: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error())
                    .context("opening frozen read identity");
            }
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
            let error = (ok == 0).then(std::io::Error::last_os_error);
            unsafe {
                CloseHandle(handle);
            }
            if let Some(error) = error {
                return Err(error).context("checking frozen read identity");
            }
            Ok(Some(Self(
                info.dwVolumeSerialNumber as u64,
                ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            )))
        }
        #[cfg(not(any(unix, windows)))]
        {
            bail!("frozen native read identity is unavailable on this platform")
        }
    }
}

struct FrozenReadEntry {
    label: String,
    start: Option<u64>,
    end: Option<u64>,
    alias: PathBuf,
    target: Option<PathBuf>,
    identity: Option<FrozenReadIdentity>,
    resource: Option<NativeResource>,
    error: Option<String>,
}

struct FrozenFileRead {
    entries: Vec<FrozenReadEntry>,
    number: bool,
    batch: bool,
    omitted: usize,
    ctx: Option<ExecutionContext>,
}

impl FileRead {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn read_one(
        &self,
        path: &str,
        start: Option<u64>,
        end: Option<u64>,
        number: bool,
        max_lines: usize,
        max_bytes: usize,
        _symbol_hint: bool,
    ) -> Result<String> {
        let prepared = self.prepare_read(
            &serde_json::json!({"path":path,"start":start,"end":end,"number":number}),
            None,
        )?;
        let frozen = prepared
            .delegation
            .as_ref()
            .unwrap()
            .downcast_ref::<FrozenFileRead>()
            .unwrap();
        self.read_entry(&frozen.entries[0], frozen, max_lines, max_bytes)
    }

    fn prepare_read(
        &self,
        args: &Value,
        ctx: Option<&ExecutionContext>,
    ) -> Result<PreparedToolCall> {
        if let Some(ctx) = ctx {
            if ctx.overlay().is_some() {
                return speculative_native_tools::prepare_read_overlay(self, args, ctx);
            }
        }
        bounded_native_arguments(args, "file_read")?;
        let list = args.get("files").and_then(Value::as_array);
        if list.is_some_and(Vec::is_empty) {
            bail!("files[] is empty — pass at least one {{path,start,end}} entry, or use `path`");
        }
        let single;
        let shown = if let Some(list) = list {
            &list[..list.len().min(MULTI_READ_MAX_FILES)]
        } else {
            str_arg(args, "path")?;
            single = vec![args.clone()];
            &single[..]
        };
        let mut entries = Vec::with_capacity(shown.len());
        let mut resources = Vec::with_capacity(shown.len());
        let mut global = false;
        let scoped = ctx.is_some_and(|ctx| ctx.native_scope().is_some());
        if scoped {
            // Validate the whole declaration before any entry can read metadata or content.
            for entry in list.map_or(shown, |entries| entries.as_slice()) {
                let path = entry
                    .get("path")
                    .and_then(Value::as_str)
                    .context("native-scoped file_read entry requires an exact path")?;
                check_native_path(ctx, &self.root, path, false)?;
            }
        }
        for entry in shown {
            let Some(label) = entry.get("path").and_then(Value::as_str) else {
                if scoped {
                    bail!("native-scoped file_read entry requires an exact path");
                }
                global = true;
                entries.push(FrozenReadEntry {
                    label: String::new(),
                    start: None,
                    end: None,
                    alias: PathBuf::new(),
                    target: None,
                    identity: None,
                    resource: None,
                    error: Some("entry missing `path`".to_string()),
                });
                continue;
            };
            let raw = Path::new(label);
            let alias = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                self.root.canonicalize()?.join(raw)
            };
            let target = match confine(&self.root, label, true) {
                Ok(target) => Some(target),
                Err(error) if list.is_some() && !scoped => {
                    global = true;
                    entries.push(FrozenReadEntry {
                        label: label.to_string(),
                        start: entry.get("start").and_then(Value::as_u64),
                        end: entry.get("end").and_then(Value::as_u64),
                        alias,
                        target: None,
                        identity: None,
                        resource: None,
                        error: Some(format!("{error:#}")),
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            let identity = FrozenReadIdentity::capture(target.as_deref().unwrap())?;
            let resource = prepared_native_resource(target.as_deref().unwrap(), ctx)?;
            if let Some(resource) = &resource {
                resources.push((resource.clone(), LockMode::Shared));
            } else {
                global = true;
            }
            entries.push(FrozenReadEntry {
                label: label.to_string(),
                start: entry.get("start").and_then(Value::as_u64),
                end: entry.get("end").and_then(Value::as_u64),
                alias,
                target,
                identity,
                resource,
                error: None,
            });
        }
        let mut prepared = PreparedToolCall::new(args.clone());
        if let Some(placement) = ctx.and_then(ExecutionContext::placement) {
            prepared.display_paths = entries
                .iter()
                .filter_map(|entry| {
                    placement
                        .scratch_display(&entry.alias)
                        .map(|display| display.with_argument(PathBuf::from(&entry.label)))
                })
                .collect();
        }
        prepared.footprint = Some(if global {
            PreparedFootprint::Global
        } else {
            PreparedFootprint::Native(resources)
        });
        prepared.delegation = Some(std::sync::Arc::new(FrozenFileRead {
            entries,
            number: args.get("number").and_then(Value::as_bool).unwrap_or(false),
            batch: list.is_some(),
            omitted: list.map_or(0, |list| list.len().saturating_sub(MULTI_READ_MAX_FILES)),
            ctx: ctx.cloned(),
        }));
        Ok(prepared)
    }

    fn read_entry(
        &self,
        entry: &FrozenReadEntry,
        frozen: &FrozenFileRead,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<String> {
        if let Some(error) = &entry.error {
            bail!("{error}");
        }
        let resolved = entry
            .target
            .as_deref()
            .context("file_read needs a frozen target")?;
        if let Some(resource) = &entry.resource {
            resource.verify()?;
        }
        if entry.alias.canonicalize()? != resolved {
            bail!("file_read alias changed after preparation; prepare again");
        }
        if FrozenReadIdentity::capture(resolved)? != entry.identity {
            bail!("file_read target identity changed after preparation; prepare again");
        }
        // Metadata and opened-handle length are checked before allocating or reading disk bytes.
        #[cfg(test)]
        tests::native_body_rendezvous("file_read", resolved);
        let (content, observed) = FileEdit::read_bounded(resolved).with_context(|| {
            format!(
                "file_read requires a regular UTF-8 file at most 4 MiB: {}",
                resolved.display()
            )
        })?;
        if let Some(resource) = &entry.resource {
            resource.verify()?;
        }
        if entry.alias.canonicalize()? != resolved {
            bail!("file_read alias changed while reading; prepare again");
        }
        if FrozenReadIdentity::capture(resolved)? != entry.identity {
            bail!("file_read target identity changed while reading; prepare again");
        }
        crate::core::read_ledger::note(resolved, &observed);
        if entry.start.is_none() && entry.end.is_none() && !frozen.number {
            let view = budget_view(&content, &entry.label, max_lines, max_bytes);
            // No advisory discovery/RPC inside any exact native resource-locked body.
            if !frozen.batch
                && entry.resource.is_none()
                && frozen
                    .ctx
                    .as_ref()
                    .is_none_or(|ctx| ctx.native_scope().is_none())
            {
                let is_code = crate::agent::lsp::discovery::server_for_path(resolved).is_some();
                let lsp_on = crate::agent::lsp::LSP.is_enabled();
                if let Some(hint) = whole_file_symbol_hint(is_code, lsp_on, content.lines().count())
                {
                    return Ok(format!("{hint}\n{view}"));
                }
            }
            return Ok(view);
        }
        let lines: Vec<&str> = content.lines().collect();
        let s = entry.start.unwrap_or(1).max(1) as usize;
        let e = (entry.end.unwrap_or(lines.len() as u64) as usize).min(lines.len());
        if s > e {
            return Ok(String::new());
        }
        Ok(range_view(
            &lines,
            s,
            e,
            frozen.number,
            &entry.label,
            max_lines,
            max_bytes,
        ))
    }

    fn read_prepared(&self, prepared: &PreparedToolCall) -> Result<String> {
        if speculative_native_tools::is_overlay_read(prepared) {
            return speculative_native_tools::execute_read_overlay(prepared)
                .expect("overlay read delegation");
        }
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileRead>())
            .context("file_read requires internally prepared paths and options")?;
        verify_frozen_native_context(frozen.ctx.as_ref())?;
        if let Some(scope) = frozen.ctx.as_ref().and_then(ExecutionContext::native_scope) {
            for entry in &frozen.entries {
                scope.check_read(
                    entry
                        .resource
                        .as_ref()
                        .context("scoped read lost its exact resource")?,
                )?;
            }
        }
        if !frozen.batch {
            let output = self.read_entry(
                &frozen.entries[0],
                frozen,
                FILE_READ_MAX_LINES,
                FILE_READ_MAX_BYTES,
            )?;
            if output.len() > PREPARED_EDIT_MAX_BYTES {
                bail!("file_read output exceeds the 4 MiB limit");
            }
            return Ok(output);
        }
        let per_lines = (FILE_READ_MAX_LINES / frozen.entries.len()).max(200);
        let per_bytes = (FILE_READ_MAX_BYTES / frozen.entries.len()).max(4_000);
        let mut output = String::new();
        for entry in &frozen.entries {
            let header = if entry.label.is_empty() {
                "── (entry missing `path`) ──".to_string()
            } else {
                match (entry.start, entry.end) {
                    (None, None) => format!("── {} ──", entry.label),
                    (s, e) => format!(
                        "── {}:{}-{} ──",
                        entry.label,
                        s.map_or("1".to_string(), |v| v.to_string()),
                        e.map_or("end".to_string(), |v| v.to_string())
                    ),
                }
            };
            let body = match self.read_entry(entry, frozen, per_lines, per_bytes) {
                Ok(body) => body,
                Err(error) => format!("error: {error:#}"),
            };
            if output
                .len()
                .saturating_add(header.len())
                .saturating_add(body.len())
                .saturating_add(2)
                > PREPARED_EDIT_MAX_BYTES
            {
                bail!("file_read output exceeds the 4 MiB limit");
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&header);
            output.push('\n');
            output.push_str(&body);
        }
        if frozen.omitted > 0 {
            let note = format!("\n…[{} more file(s) not read — {MULTI_READ_MAX_FILES} per call max; call again for the rest]", frozen.omitted);
            if output.len().saturating_add(note.len()) > PREPARED_EDIT_MAX_BYTES {
                bail!("file_read output exceeds the 4 MiB limit");
            }
            output.push_str(&note);
        }
        Ok(output)
    }
}

impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Read 1-based line ranges; batch files:[{path,start,end}] (max 8). Read before edits; \
         number:false keeps raw old_string. Relative to cwd; absolute/../ allowed. \
         For symbols use lsp_document_symbols + read_symbol. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start": {"type": "integer", "description": "optional 1-based first line"},
                "end": {"type": "integer", "description": "optional 1-based last line"},
                "number": {"type": "boolean", "description": "N| line prefixes (default false)"},
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "start": {"type": "integer"},
                            "end": {"type": "integer"}
                        },
                        "required": ["path"]
                    },
                    "description": "batch files/ranges (max 8); overrides path"
                }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn prepare_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        self.prepare_read(args, Some(ctx))
    }
    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        self.read_prepared(prepared).map(ToolObservation::text)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let ctx = crate::core::exec_ctx::current();
        let prepared = self.prepare_read(args, ctx.as_ref())?;
        self.read_prepared(&prepared)
    }
}

/// (start,end) byte span per line; `end` EXCLUDES the trailing `\n` (same scan as
/// `indent_tolerant_blocks`). The final span runs to `content.len()`.
fn line_byte_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            spans.push((start, idx));
            start = idx + 1;
        }
    }
    spans.push((start, content.len()));
    spans
}

/// First `max` bytes of `s`, snapped DOWN to a char boundary (never panics on multibyte).
fn take_bytes_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
/// Last `max` bytes of `s`, snapped UP to a char boundary.
fn take_bytes_suffix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Bound a WHOLE-file read: under both caps → verbatim; over → head+tail with a LOUD marker stating
/// the total size + exact shown ranges, so the model knows it has a partial view (and won't feed the
/// spliced text back as an exact `old_string` — a slice is either fully inside head/tail, which
/// round-trips, or spans the omitted sentinel, which is a clean exact-miss). Slices are byte-exact
/// (real line endings preserved — NOT the lossy `lines()` path). `max_*`=0 disables that cap.
/// A `start`/`end` (or `number:true`) read under the same budget as a whole-file read: the
/// CONTIGUOUS head of the requested range up to the line/byte cap, then a marker naming what was
/// left out and the exact `start` to continue from. Contiguous on purpose — a ranged read exists
/// so the model can copy an `old_string` out of it, and a head+tail or keyword splice would hand
/// it text that does not occur in the file in that order. Zero for either cap disables it.
fn range_view(
    lines: &[&str],
    s: usize,
    e: usize,
    number: bool,
    path: &str,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    for (i, line) in lines[s - 1..e].iter().enumerate() {
        let rendered = if number {
            format!("{}|{line}", s + i)
        } else {
            (*line).to_string()
        };
        if max_lines > 0 && shown >= max_lines {
            break;
        }
        if max_bytes > 0 && out.len() + rendered.len() + 1 > max_bytes {
            if shown == 0 {
                // One line alone exceeds the budget (minified source): show its head, char-safe.
                out.push_str(take_bytes_prefix(&rendered, max_bytes));
                shown = 1;
            }
            break;
        }
        if shown > 0 {
            out.push('\n');
        }
        out.push_str(&rendered);
        shown += 1;
    }
    let requested = e - s + 1;
    if shown < requested || (shown == 1 && requested == 1 && out.len() < lines[s - 1].len()) {
        let last = s + shown.max(1) - 1;
        let kb = lines[s - 1..e].iter().map(|l| l.len() + 1).sum::<usize>() / 1024;
        out.push_str(&format!(
            "\n…[file_read: lines {s}-{e} of {path} are {requested} lines ({kb} KB) — over the read \
             budget ({max_lines} lines / {} KB). Showing {s}-{last}; continue with start:{}, \
             end:{e}.]",
            max_bytes / 1024,
            last + 1,
        ));
    }
    out
}

fn budget_view(content: &str, path: &str, max_lines: usize, max_bytes: usize) -> String {
    let total_bytes = content.len();
    let spans = line_byte_spans(content);
    let total_lines = spans.len();
    let over_lines = max_lines > 0 && total_lines > max_lines;
    let over_bytes = max_bytes > 0 && total_bytes > max_bytes;
    if !over_lines && !over_bytes {
        return content.to_string();
    }
    let kb = total_bytes / 1024;
    // Per-slice byte clamp (a giant line inside the head/tail window can't blow the budget).
    let bcap = if max_bytes > 0 {
        (max_bytes / 2).max(1)
    } else {
        usize::MAX
    };
    if over_lines {
        let half = (max_lines / 2).max(1);
        let head = take_bytes_prefix(&content[..spans[half].0], bcap);
        let tail = take_bytes_suffix(&content[spans[total_lines - half].0..], bcap);
        let tail_first = total_lines - half + 1;
        let omitted = total_lines - 2 * half;
        format!(
            "[file_read: {path} is {total_lines} lines ({kb} KB) — over the {max_lines}-line read \
             budget. Showing lines 1-{half} and {tail_first}-{total_lines}; pass start/end (e.g. \
             start:{}, end:{}) or use search_files to read the omitted middle.]\n{head}\n…[{omitted} \
             lines omitted: {}-{}]…\n{tail}",
            half + 1,
            half + 500,
            half + 1,
            tail_first - 1,
        )
    } else {
        // Byte-dominated (a few very long lines): split by bytes, char-safe.
        let half = (max_bytes / 2).max(1);
        let head = take_bytes_prefix(content, half);
        let tail = take_bytes_suffix(content, half);
        let omitted = total_bytes.saturating_sub(head.len() + tail.len());
        format!(
            "[file_read: {path} is {kb} KB across {total_lines} line(s) — over the {} KB read budget. \
             Showing the first and last {} KB; use start/end or search_files for a specific part.]\n\
             {head}\n…[{omitted} bytes omitted]…\n{tail}",
            max_bytes / 1024,
            (half / 1024).max(1),
        )
    }
}

// ── file_glob ──────────────────────────────────────────────────────────────

struct FileGlob {
    root: PathBuf,
}
impl FileGlob {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for FileGlob {
    fn name(&self) -> &str {
        "file_glob"
    }
    fn description(&self) -> &str {
        concat!(
            "Find names by glob (*, **, ?) or fuzzy bare name. Bare names search cwd, ancestors, \
             Desktop/Documents/home and skip heavy dirs; globs/../absolute target one place. \
             Smart case; dotfiles/target included unless ignore:true. Read-only.",
            crate::search_routing!()
        )
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "glob (src/**/*.rs, ../sibling/**/*.py) or bare name for fuzzy lookup"},
                "ignore": {"type": "boolean", "description": "honour .gitignore; skip node_modules/target/.git (default false)"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let pattern = str_arg(args, "pattern")?;
        let git_aware = args
            .get("ignore")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let now = std::time::SystemTime::now();
        // Split into a literal directory anchor + the glob remainder, so `../x/**/*.rs` and
        // `C:/abs/**/*.rs` reach a SPECIFIC place. A bare-name / bare-`**/name` pattern (no explicit
        // directory the user pointed at) is a BROAD search: we walk the familiar-location seeds
        // (cwd → ancestors → Desktop/Documents/Downloads/home) so the file is found even when it
        // lives above the cwd — the whole reason a model no longer needs to shell out to `where`.
        let (anchor, sub) = glob_anchor(&self.root, pattern);
        // STRUCTURED vs BARE-NAME (decided below) — but a fully LITERAL structured pattern (has a
        // separator or absolute/`..` prefix, yet NO `*`/`?`) names ONE exact path, so a single
        // stat answers it: no walk at all (this is the `ls that path` case — ripgrep-adjacent tools
        // short-circuit the same way). `glob_anchor` already anchored a pure-literal pattern at its
        // PARENT with the final segment in `sub`, so `anchor.join(sub)` is the literal target.
        // Careful: (a) BARE names (`Cargo.toml`, `miniproject`) must NOT take this path — they are
        // intentionally a broad ranked/fuzzy search across the familiar-location seeds; (b) on a
        // MISS we fall through to the normal walk — stat can't answer smart-case and the walk would
        // find `src/A.RS` for `src/a.rs` (mirrors file_read: a missing literal path reaches the same
        // "not found" messaging, just slower); (c) the `?` exclusion matters — `?` is a glob char
        // here, and on Windows it is also illegal in a real file name, so stat could never settle
        // it either way.
        {
            let has_glob_char = sub.contains('*') || sub.contains('?');
            let bare_name = sub == pattern.replace('\\', "/") && anchor == self.root;
            if !has_glob_char && !bare_name {
                let literal = anchor.join(&sub);
                if std::fs::symlink_metadata(&literal).is_ok() {
                    let s = display_path(&self.root, &literal);
                    return Ok(if literal.is_dir() { format!("{s}/") } else { s });
                }
            }
        }
        let sub_re = if sub.is_empty() {
            pattern.replace('\\', "/")
        } else {
            sub.clone()
        };
        // STRUCTURED vs BARE-NAME. A pattern with a path SEPARATOR, a glob metacharacter (`*`/`?`), a
        // `..`, or an absolute prefix is a deliberate STRUCTURED query → walk just its anchor DEEP with
        // NO pruning (the caller pointed somewhere specific and wants everything under it, build output
        // included — this is what `**/*.rs`, `src/**/*.ts`, `../sib/**/*.js` need). A pattern that is a
        // BARE NAME (`Cargo.toml`, `miniproject`) is a "find this by name" → walk the familiar-location
        // seeds (cwd deep, ancestors + Desktop/Documents/… shallow) WITH heavy/system-dir pruning and
        // ranking, so it's found even above the cwd without scanning the whole drive.
        let norm = pattern.replace('\\', "/");
        let structured =
            norm.contains('/') || norm.contains('*') || norm.contains('?') || anchor != self.root;
        let narrow = structured;
        let ci = smart_case_insensitive(&sub_re);
        let re = compile_glob(&sub_re, ci)?;

        // ── exact glob pass ──────────────────────────────────────────────────
        // NARROW: walk just the anchor deep (no pruning). BROAD: walk the seed roots (cwd deep,
        // ancestors + familiar folders shallow) with subtree pruning of heavy/system dirs. Match the
        // entry's path RELATIVE to the walk root against the glob; for a broad walk we also test the
        // BASENAME so a bare `**/*.rs`-style regex matches by name regardless of how deep the seed sits.
        let roots: Vec<(PathBuf, usize)> = if narrow {
            vec![(anchor.clone(), SEED_DEPTH_CWD)]
        } else {
            seed_dirs(&self.root)
        };
        let budget = WalkBudget::new(
            if narrow { 400_000 } else { 250_000 },
            Duration::from_millis(if narrow { 4000 } else { 2500 }),
        );
        let re_ref = &re;
        let outcome = bounded_walk(&roots, !narrow, git_aware, 2000, &budget, |p, rel| {
            if re_ref.is_match(rel) {
                return true;
            }
            // On a broad seed walk the glob is usually name-only; also match the basename so
            // `**/foo` and `foo*` hit regardless of how deep the seed root sits.
            if !narrow {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    return re_ref.is_match(name);
                }
            }
            false
        });
        if !outcome.paths.is_empty() {
            // Rank best-first so line 1 is the most likely answer (P1.3). The needle for scoring is
            // the last literal segment of the pattern (the intended name).
            let needle = last_literal_segment(pattern);
            let mut ranked = outcome.paths;
            ranked.sort_by(|a, b| {
                score_path(b, &needle, &self.root, now)
                    .partial_cmp(&score_path(a, &needle, &self.root, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let shown = 50.min(ranked.len());
            let mut lines: Vec<String> = ranked[..shown]
                .iter()
                .map(|p| {
                    let s = display_path(&self.root, p);
                    if p.is_dir() {
                        format!("{s}/")
                    } else {
                        s
                    }
                })
                .collect();
            // Truncation / budget flags TEACH the model to narrow instead of trusting a partial list.
            if ranked.len() > shown {
                lines.push(format!(
                    "…[{} more — showing the {shown} best; narrow the pattern to see the rest]",
                    ranked.len() - shown
                ));
            } else if outcome.capped {
                lines.push(
                    "…[result cap reached — more matches exist; narrow the pattern]".to_string(),
                );
            } else if outcome.budget_hit {
                lines.push("…[search budget reached before the whole tree was scanned — narrow the pattern or pass a ../ or absolute path to search a specific place]".to_string());
            }
            return Ok(lines.join("\n"));
        }

        // ── fuzzy fallback on the NAME ───────────────────────────────────────
        // No exact glob hit → find the closest names (typo / separator tolerance). Reuse the SAME
        // bounded walk over the same roots (P1.4) — no second bespoke recursive scan.
        let needle = last_literal_segment(pattern);
        let needle = needle
            .trim_matches(|c| c == '*' || c == '?')
            .to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(format!(
                "no files matched '{pattern}' (give a name or a glob like src/**/*.rs)"
            ));
        }
        let strip = |s: &str| {
            s.chars()
                .filter(|c| !matches!(c, '_' | '-' | ' ' | '.'))
                .collect::<String>()
        };
        let needle_ref = &needle;
        let fuzzy_budget = WalkBudget::new(
            if narrow { 400_000 } else { 250_000 },
            Duration::from_millis(if narrow { 4000 } else { 2500 }),
        );
        let pool = bounded_walk(
            &roots,
            !narrow,
            git_aware,
            6000,
            &fuzzy_budget,
            |p, _rel| match p.file_name().and_then(|n| n.to_str()) {
                Some(name) => {
                    let name = name.to_ascii_lowercase();
                    name.contains(needle_ref)
                        || strip(&name).contains(&strip(needle_ref))
                        || strsim::jaro_winkler(needle_ref, &name) >= 0.82
                }
                None => false,
            },
        );
        let mut scored: Vec<(f64, PathBuf)> = pool
            .paths
            .into_iter()
            .map(|p| (score_path(&p, &needle, &self.root, now), p))
            .filter(|(s, _)| *s >= 0.72)
            .collect();
        if scored.is_empty() {
            return Ok(format!("no file or folder named like '{pattern}' found nearby (searched the working dir, its parents, and your Desktop/Documents/Downloads). Pass a ../ or absolute path to search a specific place."));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(15);
        // Lead with a NEUTRAL header (never negative prose that reads like a failure) and tag folders.
        let lines: Vec<String> = scored
            .iter()
            .map(|(_, p)| {
                let s = display_path(&self.root, p);
                if p.is_dir() {
                    format!("{s}/  (folder)")
                } else {
                    s
                }
            })
            .collect();
        Ok(format!(
            "closest matches for '{pattern}' (ranked):\n{}",
            lines.join("\n")
        ))
    }
}

// ── file_edit ────────────────────────────────────────────────────────────────

const PREPARED_EDIT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Approval binds the computed bytes, not a replacement that could rematch changed content.
struct FrozenFileEdit {
    alias: ResolvedDestination,
    destination: ResolvedDestination,
    expected: crate::core::persist::FileFingerprint,
    target: PathBuf,
    resource: Option<NativeResource>,
    ctx: Option<ExecutionContext>,
    content: String,
    output: String,
    dry_run: bool,
    no_op: bool,
    managed: bool,
}

struct FileEdit {
    root: PathBuf,
}
impl FileEdit {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn read_bounded(target: &Path) -> Result<(String, crate::core::persist::FileFingerprint)> {
        use std::io::Read as _;
        let metadata =
            std::fs::metadata(target).with_context(|| format!("checking {}", target.display()))?;
        if !metadata.is_file() || metadata.len() > PREPARED_EDIT_MAX_BYTES as u64 {
            bail!("file_edit preparation requires a regular UTF-8 file at most 4 MiB; nothing written");
        }
        let file =
            std::fs::File::open(target).with_context(|| format!("reading {}", target.display()))?;
        let opened = file.metadata()?;
        if !opened.is_file() || opened.len() > PREPARED_EDIT_MAX_BYTES as u64 {
            bail!(
                "file_edit input changed or exceeds the 4 MiB preparation limit; nothing written"
            );
        }
        let mut bytes = Vec::with_capacity(opened.len() as usize);
        file.take(PREPARED_EDIT_MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > PREPARED_EDIT_MAX_BYTES {
            bail!("file_edit input exceeds the 4 MiB preparation limit; nothing written");
        }
        let expected = crate::core::persist::FileFingerprint::for_bytes(&bytes);
        let content = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", target.display()))?;
        Ok((content, expected))
    }

    fn bounded_edit(
        content: &str,
        old: &str,
        new: &str,
        replace_all: bool,
        label: &str,
    ) -> Result<EditApplied> {
        let count = if old.is_empty() {
            0
        } else {
            content.matches(old).count()
        };
        // Bound expansion BEFORE allocating the replacement, including replace_all amplification.
        let projected = if count > 0 {
            let replacements = if replace_all { count } else { 1 };
            content
                .len()
                .saturating_sub(old.len().saturating_mul(replacements))
                .saturating_add(new.len().saturating_mul(replacements))
        } else {
            // Fallback rungs may match one-byte spans and preserve line endings.
            let replacements = if replace_all { content.len() } else { 1 };
            content
                .len()
                .saturating_add(new.len().saturating_add(2).saturating_mul(replacements))
        };
        if old.len().saturating_add(new.len()) > PREPARED_EDIT_MAX_BYTES
            || projected > PREPARED_EDIT_MAX_BYTES
        {
            bail!("file_edit replacement exceeds the 4 MiB preparation limit; use smaller edits; nothing written");
        }
        let applied = apply_one_edit(content, old, new, replace_all, label)?;
        if applied.content.len() > PREPARED_EDIT_MAX_BYTES {
            bail!("file_edit output exceeds the 4 MiB preparation limit; nothing written");
        }
        Ok(applied)
    }

    fn prepare_edit(
        &self,
        args: &Value,
        ctx: Option<&ExecutionContext>,
    ) -> Result<PreparedToolCall> {
        if let Some(ctx) = ctx {
            if ctx.overlay().is_some() {
                return speculative_native_tools::prepare_edit_overlay(self, args, ctx);
            }
        }
        struct InputLimit(usize);
        impl std::io::Write for InputLimit {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > PREPARED_EDIT_MAX_BYTES {
                    return Err(std::io::Error::other("4 MiB file_edit input limit"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Count serialized arguments without allocating an unbounded copy, including unknown keys.
        serde_json::to_writer(InputLimit(0), args)
            .context("file_edit arguments exceed the 4 MiB preparation limit; nothing written")?;
        let path = str_arg(args, "path")?;
        let edits = args.get("edits").and_then(Value::as_array);
        if let Some(edits) = edits {
            if edits.is_empty() {
                bail!("file_edit `edits` must be a non-empty array (or omit it and pass new_string for a single edit)");
            }
            if edits.len() > 1024 {
                bail!("file_edit preparation supports at most 1024 edits; nothing written");
            }
        } else if args.get("new_string").is_none() {
            bail!("file_edit needs either `new_string` (single edit, with `old_string`) or a non-empty `edits` array (batch of {{old_string, new_string}} applied atomically to {path})");
        } else if args
            .get("old_string")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            bail!("file_edit requires non-empty old_string and an existing file; use file_write with purpose:project|artifact|scratch to create a file or replace its whole content");
        }
        let input_bytes = if let Some(edits) = edits {
            edits.iter().fold(0usize, |total, edit| {
                total
                    .saturating_add(
                        edit.get("old_string")
                            .and_then(Value::as_str)
                            .map_or(0, str::len),
                    )
                    .saturating_add(
                        edit.get("new_string")
                            .and_then(Value::as_str)
                            .map_or(0, str::len),
                    )
            })
        } else {
            args.get("old_string")
                .and_then(Value::as_str)
                .map_or(0, str::len)
                .saturating_add(str_arg(args, "new_string")?.len())
        };
        if input_bytes > PREPARED_EDIT_MAX_BYTES {
            bail!("file_edit arguments exceed the 4 MiB preparation limit; nothing written");
        }
        let raw_path = Path::new(path);
        check_native_path(ctx, &self.root, path, true)?;
        let alias_path = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            self.root.canonicalize()?.join(raw_path)
        };
        // Keep the caller's link/ancestor spelling as a second guard; the write target is canonical.
        let alias = prepare_destination(
            ctx.and_then(ExecutionContext::placement),
            &alias_path.to_string_lossy(),
            None,
        )?;
        let target = confine(&self.root, path, true)?;
        crate::core::native_journal::validate_mutation_target(&target)?;
        let destination = prepare_destination(
            ctx.and_then(ExecutionContext::placement),
            &target.to_string_lossy(),
            None,
        )?;
        let (original, expected) = Self::read_bounded(&target)?;
        alias.verify()?;
        destination.verify()?;
        if alias.actual_path().canonicalize()? != target {
            bail!("file_edit destination alias changed during preparation; nothing written");
        }
        let dry_run = dry_run_arg(args);
        let (content, output) = if let Some(edits) = edits {
            let mut buf = original.clone();
            let mut summaries = Vec::with_capacity(edits.len());
            let mut diffs = Vec::with_capacity(edits.len());
            let mut preview_bytes = 0usize;
            for (i, edit) in edits.iter().enumerate() {
                let n = i + 1;
                let old = edit
                    .get("old_string")
                    .and_then(Value::as_str)
                    .with_context(|| format!("edit #{n}: missing old_string"))?;
                let new = edit
                    .get("new_string")
                    .and_then(Value::as_str)
                    .with_context(|| format!("edit #{n}: missing new_string"))?;
                let replace_all = edit
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let applied = Self::bounded_edit(
                    &buf,
                    old,
                    new,
                    replace_all,
                    &format!("edit #{n} ({path})"),
                )?;
                let detail = match (applied.rung, applied.count) {
                    ("exact", n) if replace_all => format!("{n} replacement(s), replace_all"),
                    ("exact", n) => format!("{n} replacement(s)"),
                    ("indent", 1) => "1 replacement, indentation-tolerant".to_string(),
                    (other, 1) => format!("1 replacement, {other} match"),
                    (other, n) => format!("{n} replacements, {other} match, replace_all"),
                };
                let summary = format!("  #{n}: {detail}");
                let diff = diff_preview(&applied.before, &applied.after, applied.start_line);
                preview_bytes = preview_bytes
                    .saturating_add(summary.len())
                    .saturating_add(diff.len());
                if preview_bytes > PREPARED_EDIT_MAX_BYTES {
                    bail!("file_edit preview exceeds the 4 MiB preparation limit; nothing written");
                }
                summaries.push(summary);
                diffs.push(diff);
                buf = applied.content;
            }
            let output = if buf == original {
                format!(
                    "{NOOP_WRITE_PREFIX}: {path} unchanged after {} edit(s) net to nothing",
                    edits.len()
                )
            } else if dry_run {
                format!(
                    "{DRY_RUN_PREFIX}: would edit {path} ({} edits); nothing written\n{}\n{}",
                    edits.len(),
                    summaries.join("\n"),
                    diffs.join("\n")
                )
            } else {
                format!(
                    "edited {path} ({} edits applied)\n{}\n{}",
                    edits.len(),
                    summaries.join("\n"),
                    diffs.join("\n")
                )
            };
            (buf, output)
        } else {
            let applied = Self::bounded_edit(
                &original,
                str_arg(args, "old_string")?,
                str_arg(args, "new_string")?,
                args.get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                path,
            )?;
            let output = if applied.content == original {
                format!("{NOOP_WRITE_PREFIX}: {path} unchanged (old_string == new_string)")
            } else if dry_run {
                format!(
                    "{DRY_RUN_PREFIX}: would edit {path} ({}); nothing written\n{}",
                    applied.summary(),
                    diff_preview(&applied.before, &applied.after, applied.start_line)
                )
            } else {
                format!(
                    "edited {path} ({})\n{}",
                    applied.summary(),
                    diff_preview(&applied.before, &applied.after, applied.start_line)
                )
            };
            (applied.content, output)
        };
        if output.len() > PREPARED_EDIT_MAX_BYTES {
            bail!("file_edit preview exceeds the 4 MiB preparation limit; nothing written");
        }
        let no_op = content == original;
        let actual = target.to_string_lossy().into_owned();
        let (first, rest) = output.split_once('\n').unwrap_or((&output, ""));
        let preview = crate::agent::tools::ApprovalPreview {
            title: actual.clone(),
            diff: (!rest.is_empty()).then(|| rest.to_string()),
            lines: vec![first.to_string()],
        };
        let target_managed = destination.is_managed()
            || ctx
                .and_then(ExecutionContext::placement)
                .is_some_and(|placement| {
                    target.starts_with(crate::core::file_placement::artifact_workspace_root(
                        placement.workspace_root(),
                        placement.home_root(),
                    )) || target.starts_with(placement.scratch_root())
                });
        let resource = prepared_native_resource(&target, ctx)?;
        let frozen_destination = destination.clone();
        let frozen_expected = expected.clone();
        let mut prepared = PreparedToolCall::new(args.clone());
        prepared.display_paths = ctx
            .and_then(ExecutionContext::placement)
            .and_then(|placement| placement.scratch_display(&target))
            .into_iter()
            .collect();
        prepared.footprint = Some(match &resource {
            Some(resource) => {
                PreparedFootprint::Native(vec![(resource.clone(), LockMode::Exclusive)])
            }
            None => PreparedFootprint::Global,
        });
        prepared.args["path"] = Value::String(actual);
        prepared.preview = Some(preview);
        prepared.preview_lines = placement_preview(path, None, &destination);
        prepared.file = Some(FilePreparation {
            destination,
            source: None,
            source_managed: target_managed,
            expected: Some(expected),
            source_expected: None,
            source_tree: None,
            destination_tree: None,
        });
        prepared.delegation = Some(std::sync::Arc::new(FrozenFileEdit {
            alias,
            destination: frozen_destination,
            expected: frozen_expected,
            target,
            resource,
            ctx: ctx.cloned(),
            content,
            output,
            dry_run,
            no_op,
            managed: target_managed,
        }));
        Ok(prepared)
    }

    fn edit_prepared(
        &self,
        prepared: &PreparedToolCall,
        check_read_ledger: bool,
    ) -> Result<String> {
        if let Some(result) = speculative_native_tools::execute_edit_overlay(prepared) {
            return result;
        }
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileEdit>())
            .context("file_edit requires internally prepared replacement bytes")?;
        verify_frozen_native_context(frozen.ctx.as_ref())?;
        crate::core::native_journal::validate_mutation_target(&frozen.target)?;
        if let Some(resource) = &frozen.resource {
            resource.verify()?;
        }
        frozen.alias.verify()?;
        frozen.destination.verify()?;
        if frozen.alias.actual_path().canonicalize()? != frozen.target {
            bail!(
                "file_edit destination alias changed; prepare and approve again; nothing written"
            );
        }
        let (_, current) = Self::read_bounded(&frozen.target)?;
        if frozen.expected != current {
            bail!("file_edit target changed after preparation; prepare and approve again; nothing written");
        }
        // A preview/no-op must not refresh the ledger or pretend the session read current content.
        if frozen.dry_run || frozen.no_op {
            return Ok(frozen.output.clone());
        }
        if check_read_ledger {
            if let Some(stale) =
                crate::core::read_ledger::overwrite_conflict(&frozen.target, &current)
            {
                bail!(crate::core::read_ledger::conflict_message(
                    &frozen.target.to_string_lossy(),
                    &stale
                ));
            }
        }
        frozen.alias.verify()?;
        frozen.destination.verify()?;
        crate::core::native_journal::validate_mutation_target(&frozen.target)?;
        let scoped = frozen
            .ctx
            .as_ref()
            .is_some_and(|ctx| ctx.native_scope().is_some());
        let written = if scoped {
            let ctx = frozen.ctx.as_ref().unwrap();
            let resource = frozen
                .resource
                .as_ref()
                .context("native-scoped file_edit requires a frozen resource")?;
            ctx.native_scope().unwrap().check_resource(resource)?;
            #[cfg(test)]
            tests::native_body_rendezvous("file_edit", &frozen.target);
            crate::core::native_journal::apply(
                resource,
                &frozen.expected,
                frozen.content.as_bytes(),
                &ctx.resource_scope(),
            )?
        } else {
            crate::core::persist::compare_and_atomic_write(
                &frozen.target,
                &frozen.expected,
                frozen.content.as_bytes(),
            )
            .with_context(|| format!("writing {}", frozen.target.display()))?
        };
        crate::core::read_ledger::note(&frozen.target, &written);
        // Advisory LSP work is deferred until the executor releases physical/resource locks.
        Ok(frozen.output.clone())
    }
}
impl Tool for FileEdit {
    fn name(&self) -> &str {
        "file_edit"
    }
    /// The patch, from the same dry-run the model can ask for — computed in memory, nothing
    /// written. An edit that would fail (no match, bad path) has no preview: the failure is
    /// what the approval would have run into anyway, and refusing to show one lets the
    /// question fall through to the plain header.
    fn preview(&self, args: &Value) -> Option<crate::agent::tools::ApprovalPreview> {
        let mut dry = args.clone();
        dry.as_object_mut()?
            .insert("dry_run".to_string(), Value::Bool(true));
        let out = self.execute(&dry).ok()?;
        if !out.starts_with(DRY_RUN_PREFIX) {
            return None;
        }
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let (first, rest) = out.split_once('\n').unwrap_or((out.as_str(), ""));
        let summary = first
            .strip_prefix(DRY_RUN_PREFIX)
            .map(|s| s.trim_start_matches(':').trim())
            .unwrap_or(first)
            .to_string();
        Some(crate::agent::tools::ApprovalPreview {
            title: path.to_string(),
            diff: (!rest.trim().is_empty()).then(|| rest.to_string()),
            lines: vec![summary],
        })
    }
    fn description(&self) -> &str {
        "Replace exact text; unique match unless replace_all. edits[] apply in order atomically; \
         dry_run previews. On exact miss try indentation tolerance. Use file_write to create or \
         replace a whole file. Absolute/../ paths may write outside cwd."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "non-empty exact text to replace"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"},
                "dry_run": {"type": "boolean", "description": "preview the diff, write nothing"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "ordered edits to this file; alternative to old_string/new_string",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        write_effect(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        write_target_dir(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn workspace_effect_prepared(&self, prepared: &PreparedToolCall) -> WorkspaceEffect {
        if let Some(effect) = speculative_native_tools::workspace_effect_prepared(prepared) {
            return effect;
        }
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileEdit>());
        if frozen.is_some_and(|edit| edit.dry_run || edit.no_op) {
            WorkspaceEffect::None
        } else if frozen.is_some_and(|edit| edit.managed) {
            WorkspaceEffect::External
        } else if let Some(edit) = frozen {
            write_effect(&self.root, Some(&edit.target.to_string_lossy()))
        } else {
            self.workspace_effect(&prepared.args)
        }
    }
    fn workspace_target_prepared(&self, prepared: &PreparedToolCall) -> Option<PathBuf> {
        if speculative_native_tools::is_overlay_edit(prepared) {
            return None;
        }
        prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileEdit>())
            .and_then(|edit| edit.target.parent())
            .map(Path::to_path_buf)
    }
    fn prepare_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        self.prepare_edit(args, Some(ctx))
    }
    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        self.edit_prepared(prepared, true)
            .map(ToolObservation::text)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let ctx = crate::core::exec_ctx::current();
        let prepared = self.prepare_edit(args, ctx.as_ref())?;
        // Legacy direct calls still match a fresh read; admitted calls retain the prior read ledger.
        self.edit_prepared(&prepared, false)
    }
}

// ── file_write ─────────────────────────────────────────────────────────────

/// Create a new file OR completely overwrite an existing one in a single call. This is the tool a
/// model reaches for when it wants to "write the file from scratch". WITHOUT it, models fall back to
/// shelling out (`type NUL > f` to blank the file, then a heredoc to refill it) — which destroys the
/// file and then fails, the exact thrash this tool removes. `file_edit` stays the right tool for a
/// localized change; `file_write` is for whole-file create/replace.
struct FrozenFileWrite {
    destination: ResolvedDestination,
    expected: crate::core::persist::FileFingerprint,
    target: PathBuf,
    resource: Option<NativeResource>,
    ctx: Option<ExecutionContext>,
    content: String,
    output: String,
    no_op: bool,
    managed: bool,
}

/// Preflight the exact lines diff_preview may render before allocating a large write preview.
fn bounded_write_diff(before: &str, after: &str) -> Result<String> {
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();
    let mut prefix = 0;
    while prefix < b.len() && prefix < a.len() && b[prefix] == a[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < b.len().saturating_sub(prefix)
        && suffix < a.len().saturating_sub(prefix)
        && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut projected = 1024usize; // hunk header, elision notes and line prefixes
    for lines in [
        &b[prefix.saturating_sub(2)..prefix],
        &b[prefix..(b.len() - suffix).min(prefix.saturating_add(40))],
        &a[prefix..(a.len() - suffix).min(prefix.saturating_add(40))],
        &b[b.len() - suffix..(b.len() - suffix + 2).min(b.len())],
    ] {
        for line in lines {
            projected = projected.saturating_add(line.len()).saturating_add(2);
            if projected > PREPARED_EDIT_MAX_BYTES {
                bail!("file_write preview exceeds the 4 MiB preparation limit; nothing written");
            }
        }
    }
    Ok(diff_preview(before, after, 1))
}

struct FileWrite {
    root: PathBuf,
}
impl FileWrite {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn read_bounded(
        target: &Path,
    ) -> Result<(Option<String>, crate::core::persist::FileFingerprint)> {
        match std::fs::symlink_metadata(target) {
            Ok(_) => {
                FileEdit::read_bounded(target).map(|(content, expected)| (Some(content), expected))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((None, crate::core::persist::FileFingerprint::missing()))
            }
            Err(error) => Err(error).with_context(|| format!("checking {}", target.display())),
        }
    }

    fn prepare_write(
        &self,
        args: &Value,
        ctx: Option<&ExecutionContext>,
    ) -> Result<PreparedToolCall> {
        if let Some(ctx) = ctx {
            if ctx.overlay().is_some() {
                return speculative_native_tools::prepare_write_overlay(self, args, ctx);
            }
        }
        bounded_native_arguments(args, "file_write")?;
        let path = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;
        if content.len() > PREPARED_EDIT_MAX_BYTES {
            bail!("file_write content exceeds the 4 MiB preparation limit; nothing written");
        }
        let purpose = FilePurpose::parse_optional(args.get("purpose"))?;
        if ctx.is_some_and(|ctx| ctx.native_scope().is_some()) {
            if matches!(purpose, Some(FilePurpose::Artifact | FilePurpose::Scratch)) {
                bail!(
                    "native scope permits only its exact project files, not managed destinations"
                );
            }
            check_native_path(ctx, &self.root, path, true)?;
        }
        let raw = placement_name(&self.root, path, purpose);
        let destination =
            prepare_destination(ctx.and_then(ExecutionContext::placement), &raw, purpose)?;
        if !destination.exists_at_preparation() && purpose.is_none() {
            bail!("new file_write target requires purpose:project|artifact|scratch (e.g. {{\"path\":\"src/main.rs\",\"content\":\"...\",\"purpose\":\"project\"}})");
        }
        if !destination.is_managed() {
            require_existing_parent(destination.actual_path())?;
        }
        let (before, expected) = Self::read_bounded(destination.actual_path())
            .context("file_write preparation requires an existing file at most 4 MiB")?;
        destination.verify()?;
        let target = destination.actual_path().to_path_buf();
        crate::core::native_journal::validate_mutation_target(&target)?;
        let resource = prepared_native_resource(&target, ctx)?;
        let actual = target.to_string_lossy().into_owned();
        let no_op = before.as_deref() == Some(content);
        let diff = before
            .as_deref()
            .filter(|before| *before != content)
            .map(|before| bounded_write_diff(before, content))
            .transpose()?;
        if diff
            .as_ref()
            .is_some_and(|diff| diff.len() > PREPARED_EDIT_MAX_BYTES)
        {
            bail!("file_write preview exceeds the 4 MiB preparation limit; nothing written");
        }
        let lines = vec![if no_op {
            format!("{actual} unchanged (identical content)")
        } else if let Some(before) = &before {
            format!(
                "overwrite {actual}: {} → {} bytes",
                before.len(),
                content.len()
            )
        } else {
            format!(
                "create {actual} ({} bytes, {} line(s))",
                content.len(),
                content.lines().count()
            )
        }];
        let preview = crate::agent::tools::ApprovalPreview {
            title: actual.clone(),
            diff: diff.clone(),
            lines,
        };
        let output = if no_op {
            format!("{NOOP_WRITE_PREFIX}: {actual} already holds this exact content")
        } else {
            let verb = if expected.exists {
                "overwrote"
            } else {
                "created"
            };
            let mut output = format!("{verb} {actual} ({} line(s))", content.lines().count());
            if let Some(diff) = &diff {
                output.push('\n');
                output.push_str(diff);
            }
            output
        };
        if output.len() > PREPARED_EDIT_MAX_BYTES {
            bail!("file_write output exceeds the 4 MiB preparation limit; nothing written");
        }
        let frozen_destination = destination.clone();
        let frozen_expected = expected.clone();
        let mut prepared = PreparedToolCall::new(args.clone());
        prepared.display_paths = ctx
            .and_then(ExecutionContext::placement)
            .and_then(|placement| placement.scratch_display(&target))
            .into_iter()
            .collect();
        prepared.footprint = Some(match &resource {
            Some(resource) => {
                PreparedFootprint::Native(vec![(resource.clone(), LockMode::Exclusive)])
            }
            None => PreparedFootprint::Global,
        });
        prepared.args["path"] = Value::String(actual);
        prepared.preview = Some(preview);
        prepared.preview_lines = placement_preview(path, purpose, &destination);
        let target_managed = destination.is_managed()
            || ctx
                .and_then(ExecutionContext::placement)
                .is_some_and(|placement| {
                    destination.actual_path().starts_with(
                        crate::core::file_placement::artifact_workspace_root(
                            placement.workspace_root(),
                            placement.home_root(),
                        ),
                    ) || destination
                        .actual_path()
                        .starts_with(placement.scratch_root())
                });
        prepared.file = Some(FilePreparation {
            destination,
            source: None,
            source_managed: target_managed,
            expected: Some(expected),
            source_expected: None,
            source_tree: None,
            destination_tree: None,
        });
        prepared.delegation = Some(std::sync::Arc::new(FrozenFileWrite {
            destination: frozen_destination,
            expected: frozen_expected,
            target,
            resource,
            ctx: ctx.cloned(),
            content: content.to_string(),
            output,
            no_op,
            managed: target_managed,
        }));
        Ok(prepared)
    }

    fn write_prepared(&self, prepared: &PreparedToolCall) -> Result<String> {
        if let Some(result) = speculative_native_tools::execute_write_overlay(prepared) {
            return result;
        }
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileWrite>())
            .context("file_write requires internally prepared destination and bytes")?;
        verify_frozen_native_context(frozen.ctx.as_ref())?;
        crate::core::native_journal::validate_mutation_target(&frozen.target)?;
        if let Some(resource) = &frozen.resource {
            resource.verify()?;
        }
        frozen.destination.verify()?;
        let (_, expected) = Self::read_bounded(&frozen.target)?;
        if frozen.expected != expected {
            bail!("file_write target {} changed after preparation; prepare and approve again; nothing written", frozen.target.display());
        }
        // No-op approval does not claim a new read or consult LSP.
        if frozen.no_op {
            return Ok(frozen.output.clone());
        }
        if let Some(stale) = crate::core::read_ledger::overwrite_conflict(&frozen.target, &expected)
        {
            bail!(crate::core::read_ledger::conflict_message(
                &frozen.target.to_string_lossy(),
                &stale
            ));
        }
        let scoped = frozen
            .ctx
            .as_ref()
            .is_some_and(|ctx| ctx.native_scope().is_some());
        // Approval and resource leases are executor-owned. Preparation never creates parents.
        let written = if scoped {
            let ctx = frozen.ctx.as_ref().unwrap();
            let resource = frozen
                .resource
                .as_ref()
                .context("native-scoped file_write requires a frozen resource")?;
            ctx.native_scope().unwrap().check_resource(resource)?;
            #[cfg(test)]
            tests::native_body_rendezvous("file_write", &frozen.target);
            crate::core::native_journal::apply(
                resource,
                &frozen.expected,
                frozen.content.as_bytes(),
                &ctx.resource_scope(),
            )?
        } else {
            frozen.destination.ensure()?;
            frozen.destination.verify()?;
            crate::core::native_journal::validate_mutation_target(&frozen.target)?;
            crate::core::persist::compare_and_atomic_write(
                &frozen.target,
                &frozen.expected,
                frozen.content.as_bytes(),
            )
            .with_context(|| format!("writing {}", frozen.target.display()))?
        };
        crate::core::read_ledger::note(&frozen.target, &written);
        // Advisory LSP work is deferred until the executor releases physical/resource locks.
        Ok(frozen.output.clone())
    }
}
impl Tool for FileWrite {
    fn name(&self) -> &str {
        "file_write"
    }
    /// Create vs overwrite, with the patch against the current content when overwriting.
    fn preview(&self, args: &Value) -> Option<crate::agent::tools::ApprovalPreview> {
        bounded_native_arguments(args, "file_write").ok()?;
        let path = args.get("path")?.as_str()?;
        let content = args.get("content")?.as_str()?;
        let raw = Path::new(path);
        let target = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };
        let title = path.to_string();
        let (before, _) = Self::read_bounded(&target).ok()?;
        let preview = match before {
            Some(before) if before != content => crate::agent::tools::ApprovalPreview {
                title,
                diff: Some(bounded_write_diff(&before, content).ok()?),
                lines: vec![format!(
                    "overwrite {path}: {} → {} bytes",
                    before.len(),
                    content.len()
                )],
            },
            Some(_) => crate::agent::tools::ApprovalPreview {
                title,
                diff: None,
                lines: vec![format!("{path} unchanged (identical content)")],
            },
            None => crate::agent::tools::ApprovalPreview {
                title,
                diff: None,
                lines: vec![format!(
                    "create {path} ({} bytes, {} line(s))",
                    content.len(),
                    content.lines().count()
                )],
            },
        };
        (preview
            .diff
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(preview.title.len())
            .saturating_add(preview.lines.iter().map(String::len).sum::<usize>())
            <= PREPARED_EDIT_MAX_BYTES)
            .then_some(preview)
    }
    fn description(&self) -> &str {
        "Create/full overwrite; small edits -> file_edit. New targets require purpose: project keeps \
         explicit path/existing parent; artifact/scratch use safe relative names outside checkout and \
         create managed parents after approval. Existing overwrites may omit purpose. Never bypass \
         placement via shell. Reports actual path."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "the full file content to write"},
                "purpose": {"type": "string", "enum": ["project", "artifact", "scratch"]}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        if matches!(
            args.get("purpose").and_then(Value::as_str),
            Some("artifact" | "scratch")
        ) {
            WorkspaceEffect::External
        } else {
            write_effect(&self.root, args.get("path").and_then(|v| v.as_str()))
        }
    }
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        write_target_dir(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn workspace_effect_prepared(&self, prepared: &PreparedToolCall) -> WorkspaceEffect {
        if let Some(effect) = speculative_native_tools::workspace_effect_prepared(prepared) {
            return effect;
        }
        let frozen = prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileWrite>());
        if frozen.is_some_and(|write| write.no_op) {
            WorkspaceEffect::None
        } else if frozen.is_some_and(|write| write.managed) {
            WorkspaceEffect::External
        } else if let Some(write) = frozen {
            write_effect(&self.root, Some(&write.target.to_string_lossy()))
        } else {
            self.workspace_effect(&prepared.args)
        }
    }
    fn workspace_target_prepared(&self, prepared: &PreparedToolCall) -> Option<PathBuf> {
        if speculative_native_tools::is_overlay_write(prepared) {
            return None;
        }
        prepared
            .delegation
            .as_ref()
            .and_then(|value| value.downcast_ref::<FrozenFileWrite>())
            .and_then(|write| write.target.parent())
            .map(Path::to_path_buf)
    }
    fn prepare_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        self.prepare_write(args, Some(ctx))
    }
    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        self.write_prepared(prepared).map(ToolObservation::text)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let ctx = crate::core::exec_ctx::current();
        let prepared = self.prepare_write(args, ctx.as_ref())?;
        self.write_prepared(&prepared)
    }
}

// ── file_move ──────────────────────────────────────────────────────────────

/// Rename or move a file or directory in one call — the tool a model reaches for instead of shelling
/// out to `mv`/`move`/`Rename-Item` (which vary by OS and silently clobber). Uses `std::fs::rename`
/// (an atomic same-filesystem rename that PRESERVES the inode + metadata), falling back to a
/// copy-then-remove only across filesystems. Guards against accidental clobber: an existing
/// destination is a hard error unless `overwrite:true`.
struct FileMove {
    root: PathBuf,
}
impl FileMove {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn prepare_move(
        &self,
        args: &Value,
        ctx: Option<&ExecutionContext>,
    ) -> Result<PreparedToolCall> {
        let from = str_arg(args, "from")?;
        let to = str_arg(args, "to")?;
        let purpose = FilePurpose::parse_optional(args.get("purpose"))?;
        for key in ["overwrite", "create_dirs"] {
            if args.get(key).is_some_and(|value| !value.is_boolean()) {
                bail!("file_move {key} must be a boolean");
            }
        }
        let placement = ctx.and_then(ExecutionContext::placement);
        let from_path = Path::new(from);
        let joined_source = if from_path.is_absolute() {
            from_path.to_path_buf()
        } else {
            self.root.join(from_path)
        };
        let requested_source_managed = placement.is_some_and(|placement| {
            joined_source.starts_with(crate::core::file_placement::artifact_workspace_root(
                placement.workspace_root(),
                placement.home_root(),
            )) || joined_source.starts_with(placement.scratch_root())
        });
        if requested_source_managed {
            validate_managed_source_ancestry(&joined_source)?;
        }
        if matches!(purpose, Some(FilePurpose::Artifact | FilePurpose::Scratch)) {
            let from_path = Path::new(from);
            let joined_source = if from_path.is_absolute() {
                from_path.to_path_buf()
            } else {
                self.root.join(from_path)
            };
            let meta = std::fs::symlink_metadata(&joined_source)
                .with_context(|| format!("source {from} not found"))?;
            #[cfg(windows)]
            let reparse = {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let reparse = false;
            if meta.file_type().is_symlink() || reparse || (!meta.is_file() && !meta.is_dir()) {
                bail!(
                    "managed file_move refuses symlink/reparse or special source {}",
                    joined_source.display()
                );
            }
        }
        let raw_source = if matches!(purpose, Some(FilePurpose::Artifact | FilePurpose::Scratch)) {
            placement_name(&self.root, from, Some(FilePurpose::Project))
        } else {
            confine(&self.root, from, true)
                .with_context(|| format!("source {from} not found"))?
                .to_string_lossy()
                .into_owned()
        };
        let source = prepare_destination(placement, &raw_source, Some(FilePurpose::Project))?;
        if !source.exists_at_preparation() {
            bail!("source {from} not found");
        }
        let raw_to = placement_name(&self.root, to, purpose);
        let destination = prepare_destination(placement, &raw_to, purpose)?;
        let same_path = source.actual_path() == destination.actual_path();
        // No-follow proof of an arbitrary directory replacement is deliberately not inferred.
        if purpose.is_none()
            && !same_path
            && (!destination.exists_at_preparation() || source.actual_path().is_dir())
        {
            bail!("new file_move destination or directory move requires purpose:project|artifact|scratch (e.g. {{\"from\":\"tmp.txt\",\"to\":\"report.txt\",\"purpose\":\"artifact\"}})");
        }
        if !destination.is_managed()
            && !args
                .get("create_dirs")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            require_existing_parent(destination.actual_path())?;
        }
        let source_managed = requested_source_managed
            || placement.is_some_and(|placement| {
                source.actual_path().starts_with(
                    crate::core::file_placement::artifact_workspace_root(
                        placement.workspace_root(),
                        placement.home_root(),
                    ),
                ) || source.actual_path().starts_with(placement.scratch_root())
            });
        {
            let root = self
                .root
                .canonicalize()
                .unwrap_or_else(|_| self.root.clone());
            let from_owner = if source_managed {
                None
            } else {
                move_owner(&root, source.actual_path())
            };
            let to_owner = if destination.is_managed() {
                None
            } else {
                move_owner(&root, destination.actual_path())
            };
            if matches!((&from_owner, &to_owner), (Some(a), Some(b)) if a != b) {
                bail!("file_move between distinct repositories/workspaces is unsupported; protect and publish the two workspaces separately");
            }
            let workspace_owner = move_owner(&root, &root.join(".aizen-owner-probe"));
            if [from_owner.as_ref(), to_owner.as_ref()]
                .into_iter()
                .flatten()
                .any(|owner| Some(owner) != workspace_owner.as_ref())
            {
                bail!("file_move in a repository outside the current workspace is unsupported; run the move from that workspace");
            }
        }
        let source_expected = if source.actual_path().is_file() {
            Some(crate::core::persist::read_with_fingerprint(source.actual_path())?.1)
        } else {
            None
        };
        let expected = if destination.actual_path().is_dir() {
            None
        } else {
            Some(crate::core::persist::read_with_fingerprint(destination.actual_path())?.1)
        };
        if source.actual_path().is_dir()
            && destination.actual_path().starts_with(source.actual_path())
            && !same_path
        {
            bail!("file_move cannot move a directory into its own subtree");
        }
        if !same_path
            && destination.exists_at_preparation()
            && !args
                .get("overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && destination.actual_path().canonicalize().ok()
                != source.actual_path().canonicalize().ok()
        {
            bail!("destination {to} already exists; pass overwrite:true to replace it");
        }
        let source_tree =
            if (source_managed || destination.is_managed()) && source.actual_path().is_dir() {
                Some(managed_move_tree_snapshot(source.actual_path())?)
            } else {
                None
            };
        let destination_tree = if destination.is_managed() && destination.actual_path().is_dir() {
            Some(managed_move_tree_snapshot(destination.actual_path())?)
        } else {
            None
        };
        source.verify()?;
        destination.verify()?;
        let mut prepared = PreparedToolCall::new(args.clone());
        if let Some(placement) = ctx.and_then(ExecutionContext::placement) {
            prepared.display_paths = [source.actual_path(), destination.actual_path()]
                .into_iter()
                .filter_map(|path| placement.scratch_display(path))
                .collect();
        }
        prepared.args["from"] = Value::String(source.actual_path().to_string_lossy().into_owned());
        prepared.args["to"] =
            Value::String(destination.actual_path().to_string_lossy().into_owned());
        prepared.preview_lines = placement_preview(to, purpose, &destination);
        prepared.preview = self.preview(&prepared.args);
        prepared.file = Some(FilePreparation {
            destination,
            source: Some(source),
            source_managed,
            expected,
            source_expected,
            source_tree,
            destination_tree,
        });
        Ok(prepared)
    }
}
impl Tool for FileMove {
    fn name(&self) -> &str {
        "file_move"
    }
    fn preview(&self, args: &Value) -> Option<crate::agent::tools::ApprovalPreview> {
        let from = args.get("from")?.as_str()?;
        let to = args.get("to")?.as_str()?;
        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Some(crate::agent::tools::ApprovalPreview {
            title: from.to_string(),
            diff: None,
            lines: vec![format!(
                "move {from} → {to}{}",
                if overwrite { " (overwrite)" } else { "" }
            )],
        })
    }
    fn description(&self) -> &str {
        "Move/rename file or directory; never bypass placement via mv/move. New destinations/directory \
         moves require purpose: project keeps explicit to; artifact/scratch use safe managed relative \
         names. Existing file replacements may omit purpose. Source never routes. Collisions need \
         overwrite; create_dirs creates project parents. Managed imports bounded/no symlinks. \
         Reports actual paths; distinct-workspace moves refused."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "existing source file/directory"},
                "to": {"type": "string", "description": "destination path or managed relative name"},
                "purpose": {"type": "string", "enum": ["project", "artifact", "scratch"]},
                "overwrite": {"type": "boolean", "description": "replace existing destination (default false)"},
                "create_dirs": {"type": "boolean", "description": "create missing destination parents (default false)"}
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        move_effect(&self.root, args)
    }
    fn workspace_effect_prepared(&self, prepared: &PreparedToolCall) -> WorkspaceEffect {
        let Some(file) = prepared.file.as_ref() else {
            return self.workspace_effect(&prepared.args);
        };
        let from = if file.source_managed {
            WorkspaceEffect::External
        } else {
            write_effect(
                &self.root,
                prepared.args.get("from").and_then(Value::as_str),
            )
        };
        let to = if file.destination.is_managed() {
            WorkspaceEffect::External
        } else {
            write_effect(&self.root, prepared.args.get("to").and_then(Value::as_str))
        };
        if from == WorkspaceEffect::Paths || to == WorkspaceEffect::Paths {
            WorkspaceEffect::Paths
        } else if from == WorkspaceEffect::External && to == WorkspaceEffect::External {
            WorkspaceEffect::External
        } else {
            WorkspaceEffect::OpaqueWorkspace
        }
    }
    fn workspace_target_prepared(&self, prepared: &PreparedToolCall) -> Option<PathBuf> {
        let Some(file) = prepared.file.as_ref() else {
            return self.workspace_target(&prepared.args);
        };
        let from = prepared.args.get("from").and_then(Value::as_str);
        let to = prepared.args.get("to").and_then(Value::as_str);
        if !file.source_managed && write_effect(&self.root, from) == WorkspaceEffect::Paths {
            write_target_dir(&self.root, from)
        } else if !file.destination.is_managed()
            && write_effect(&self.root, to) == WorkspaceEffect::Paths
        {
            write_target_dir(&self.root, to)
        } else {
            write_target_dir(&self.root, from)
        }
    }
    /// Protect the source when removing project content, or the destination when publishing
    /// managed output into the project. Two distinct repositories are rejected at preparation.
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        let from = args.get("from").and_then(|v| v.as_str());
        let to = args.get("to").and_then(|v| v.as_str());
        if !current_managed_path(&self.root, from)
            && write_effect(&self.root, from) == WorkspaceEffect::Paths
        {
            write_target_dir(&self.root, from)
        } else if !matches!(
            args.get("purpose").and_then(Value::as_str),
            Some("artifact" | "scratch")
        ) && write_effect(&self.root, to) == WorkspaceEffect::Paths
        {
            write_target_dir(&self.root, to)
        } else {
            write_target_dir(&self.root, from)
        }
    }
    fn prepare_call(&self, args: &Value, ctx: &ExecutionContext) -> Result<PreparedToolCall> {
        self.prepare_move(args, Some(ctx))
    }
    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        self.move_prepared(prepared).map(ToolObservation::text)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let ctx = crate::core::exec_ctx::current();
        let prepared = self.prepare_move(args, ctx.as_ref())?;
        self.move_prepared(&prepared)
    }
}
impl FileMove {
    fn move_prepared(&self, prepared: &PreparedToolCall) -> Result<String> {
        let args = &prepared.args;
        let guard = prepared
            .file
            .as_ref()
            .context("file_move requires internally prepared destination")?;
        let source = guard
            .source
            .as_ref()
            .context("file_move requires internally prepared source")?;
        if guard.source_managed {
            validate_managed_source_ancestry(source.actual_path())?;
        }
        source.verify()?;
        guard.destination.verify()?;
        for (path, expected) in [
            (source.actual_path(), guard.source_expected.as_ref()),
            (guard.destination.actual_path(), guard.expected.as_ref()),
        ] {
            if let Some(expected) = expected {
                if &crate::core::persist::read_with_fingerprint(path)?.1 != expected {
                    bail!("file_move path {} changed after preparation; prepare and approve again; nothing moved", path.display());
                }
            }
        }
        for (path, snapshot) in [
            (source.actual_path(), guard.source_tree.as_ref()),
            (
                guard.destination.actual_path(),
                guard.destination_tree.as_ref(),
            ),
        ] {
            if let Some(snapshot) = snapshot {
                if &managed_move_tree_snapshot(path)? != snapshot {
                    bail!("managed file_move tree {} changed after preparation; prepare and approve again; nothing moved", path.display());
                }
            }
        }
        let from = str_arg(args, "from")?;
        let to = str_arg(args, "to")?;
        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let create_dirs = args
            .get("create_dirs")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let src = source.actual_path().to_path_buf();
        let dst = guard.destination.actual_path().to_path_buf();
        if guard.destination.is_managed() {
            guard.destination.ensure()?;
        } else if create_dirs {
            guard.destination.ensure_project_parents()?;
        }
        source.verify()?;
        guard.destination.verify()?;

        // Moving a path onto itself is a no-op (don't churn the FS or arm the verify gate).
        if src == dst {
            return Ok(format!(
                "{NOOP_WRITE_PREFIX}: {from} and {to} are the same path"
            ));
        }
        // CASE-ONLY (or otherwise same-inode) rename on a case-insensitive FS: `foo.txt` → `Foo.txt`.
        // `src` is canonicalized to the on-disk casing while `dst` keeps the requested casing, so
        // `src == dst` above is FALSE — yet `dst.exists()` is TRUE because both names hit the same
        // inode. The old code then DELETED that one inode and renamed a now-missing source into the
        // void: permanent data loss. Detect it by canonicalizing `dst`; if it resolves to `src`, this
        // is the same file and a direct rename just changes its recorded casing — never delete.
        let dst_is_src = dst.canonicalize().map(|c| c == src).unwrap_or(false);
        if dst_is_src {
            move_path(&src, &dst).with_context(|| format!("renaming {from} → {to}"))?;
            crate::core::read_ledger::forget(&src);
            crate::core::read_ledger::forget(&dst);
            let kind = if dst.is_dir() { "directory" } else { "file" };
            return Ok(format!("moved {kind} {from} → {to}"));
        }
        if dst.exists() {
            if !overwrite {
                bail!("destination {to} already exists; pass overwrite:true to replace it");
            }
            // An overwrite move destroys the destination's content outright, so it needs the same
            // cross-session guard `file_write` has: refuse when another live window is editing the
            // file we are about to replace and this session never looked at it. `file_move` has no
            // CAS of its own, which makes this the ONLY check standing between a blind `overwrite:
            // true` and another window's work.
            if let Some(by) = crate::features::coop::live_peer_claim_at(&dst) {
                if crate::core::read_ledger::observed(&dst).is_none() {
                    bail!(
                        "overwrite conflict: {to} is being edited by session {by} and this session \
                         has not read it; nothing was moved. Replacing it would discard that \
                         window's work — read it first if you mean to replace it."
                    );
                }
            }
            // Overwrite WITHOUT a pre-delete window: if we deleted `dst` first and the subsequent
            // rename then failed (permissions, a race, source vanished), `dst` would be gone with
            // nothing put in its place — an irrecoverable clobber. Instead STASH the existing dst to a
            // sibling temp, attempt the move, and only delete the stash once the move succeeds; on any
            // failure, rename the stash back so `dst` is exactly as it was.
            let stash = stash_path(&dst);
            std::fs::rename(&dst, &stash)
                .with_context(|| format!("staging existing {to} aside before overwrite"))?;
            match move_path(&src, &dst) {
                Ok(()) => {
                    // Move landed — the stashed old destination is now safe to drop.
                    if stash.is_dir() {
                        let _ = std::fs::remove_dir_all(&stash);
                    } else {
                        let _ = std::fs::remove_file(&stash);
                    }
                }
                Err(move_err) => {
                    // Roll back: put the original destination back exactly where it was, then report
                    // the move failure (not the rollback) as the actionable error.
                    let _ = std::fs::rename(&stash, &dst);
                    return Err(anyhow::Error::new(move_err)).with_context(|| {
                        format!("moving {from} → {to} (destination left unchanged)")
                    });
                }
            }
            // Both identities changed hands: the source is gone and the destination holds different
            // content than anything this session observed there. A stale observation on either would
            // make the NEXT overwrite check answer about a file that no longer exists.
            crate::core::read_ledger::forget(&src);
            crate::core::read_ledger::forget(&dst);
            let kind = if dst.is_dir() { "directory" } else { "file" };
            return Ok(format!("moved {kind} {from} → {to}"));
        }
        move_path(&src, &dst).with_context(|| format!("moving {from} → {to}"))?;
        crate::core::read_ledger::forget(&src);
        crate::core::read_ledger::forget(&dst);
        let kind = if dst.is_dir() { "directory" } else { "file" };
        Ok(format!("moved {kind} {from} → {to}"))
    }
}

// A local `atomic_write` used to live here. It is gone: [`crate::core::persist::atomic_write`] does
// the same job and is strictly stronger — it stages with `create_new` (concurrent writers can never
// share a staging path) and replaces via `MoveFileExW(WRITE_THROUGH)` on Windows instead of a plain
// `fs::rename`. Every write path in this file already called the `persist` one; only this copy's own
// tests still referenced it, and they now exercise `persist` directly.

/// A sibling temp path for staging an existing file/dir out of the way during an overwrite. Lives in
/// the same directory as `p`, so the stash rename stays on one filesystem (and is thus atomic).
fn stash_path(p: &Path) -> PathBuf {
    static STASH_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let parent = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("dst");
    let n = STASH_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{fname}.aizen-stash.{}.{n}", std::process::id()))
}

/// Rename `src` → `dst`, falling back to copy-then-delete when they live on different filesystems
/// (`fs::rename` returns `ErrorKind::CrossesDevices`, or a raw OS error on older toolchains). The
/// happy path is a single atomic `rename` that preserves the inode + metadata.
fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            // Cross-device: recursively copy then remove the source. Directories are walked; files
            // are a straight copy (which carries permissions via std on both Windows and Unix).
            copy_recursive(src, dst)?;
            if src.is_dir() {
                std::fs::remove_dir_all(src)
            } else {
                std::fs::remove_file(src)
            }
        }
        Err(e) => Err(e),
    }
}

/// Whether a rename error is "these paths are on different filesystems" (the only case worth a
/// copy-then-delete fallback). `CrossesDevices` is unstable-named but stable-valued; also match the
/// raw OS codes (Windows ERROR_NOT_SAME_DEVICE=17, Unix EXDEV=18) so we don't depend on the name.
fn is_cross_device(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(17) | Some(18))
}

/// Recursively copy `src` → `dst` (file or directory). Used only on the cross-filesystem move path.
fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// Prefix of a `file_edit` result that previewed without writing (`dry_run: true`). The loop
/// keeps the full diff for such a result — the preview IS the answer.
pub(crate) const DRY_RUN_PREFIX: &str = "dry-run";

fn dry_run_arg(args: &Value) -> bool {
    args.get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The outcome of ONE replacement (pure; computed in memory). `before`/`after` feed the diff
/// preview; `count`/`rung` feed the human summary (which rung matched is surfaced so a
/// looser-than-exact match is always visible in the result).
#[derive(Debug)]
struct EditApplied {
    content: String,
    before: String,
    after: String,
    count: usize,
    /// Which ladder rung applied: "exact" | "indent" | "ws-norm" | "anchor-trim" | "unescape" |
    /// "blank-norm".
    rung: &'static str,
    /// 1-based line (in the PRE-edit content) where the replaced region starts — feeds the diff
    /// preview's `@@` hunk header so the TUI can gutter real line numbers. First match for
    /// `replace_all`.
    start_line: usize,
}

/// 1-based line number of byte offset `at` in `content` (`at` must sit on a char boundary).
fn line_at(content: &str, at: usize) -> usize {
    content[..at].matches('\n').count() + 1
}
impl EditApplied {
    /// Human summary. The "exact" and "indent" wordings are byte-identical to the original
    /// `file_edit` strings (regression-gated by the existing tests).
    fn summary(&self) -> String {
        let kind = match self.rung {
            "indent" => "indentation-tolerant match",
            "ws-norm" => "whitespace-normalized match",
            "anchor-trim" => "shared-context-trimmed match",
            "unescape" => "escape-normalized match",
            "blank-norm" => "blank-line-insensitive match",
            _ => return format!("{} replacement(s)", self.count),
        };
        if self.count > 1 {
            format!("{} replacements, {kind}, replace_all", self.count)
        } else {
            format!("1 replacement, {kind}")
        }
    }
}

/// Rungs R3–R5 skip oversized hunks (quadratic-ish scans buy nothing on giant blocks).
const LADDER_MAX_LINES: usize = 200;
const LADDER_MAX_BYTES: usize = 16 * 1024;
/// The nearest-miss report is skipped on very large files (the scan is O(lines × hunk-lines)).
const NEAREST_MISS_MAX_FILE_LINES: usize = 20_000;

/// Apply ONE replacement to `content` — the shared matcher behind both `file_edit` and
/// the batch `edits` form (pure, no IO). A LADDER of progressively looser strategies (the Aider lesson:
/// every rescued edit is a saved round-trip — a ~9× edit-error reduction was measured for this
/// class of matcher), with one invariant at every rung: EXACTLY one match applies; more than one
/// is a hard "ambiguous" error that never falls through to a looser rung (a looser rung must not
/// resolve an ambiguity a stricter one detected); zero falls through.
///
///   R1 exact           — `content.matches(old)` (`replace_all` lives here ONLY)
///   R2 indent-tolerant — per-line trim (the #1 real failure: leading indentation)
///   R3 ws-normalized   — per-line interior whitespace collapsed (`foo( a,  b )` ≡ `foo(a, b)`)
///   R4 anchor-trim     — a first/last line byte-identical in old AND new (pure context) is
///                        dropped from BOTH and R1–R3 retried: semantics-preserving by construction
///   R5 escape-norm     — JSON-unescape old AND new (the double-escaped-arguments failure mode)
///   R5.5 blank-norm    — match ignoring blank lines (the model added/dropped an empty line inside
///                        the block, so its fixed line count no longer aligns with the file)
///   R6 nearest-miss    — terminal failure with the best-scoring region quoted (line numbers +
///                        "copy EXACTLY from this"), so the retry lands in ONE turn
///
/// `old` MUST be non-empty (create-new is the caller's concern). `label` names the target in
/// errors (a path for the single form; "edit #N (path)" for the batch form).
fn apply_one_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    label: &str,
) -> Result<EditApplied> {
    if old.is_empty() {
        bail!("empty old_string is only valid for creating a new file (file_edit), not mid-edit");
    }
    // R1 exact.
    let count = content.matches(old).count();
    if count > 0 {
        if count > 1 && !replace_all {
            bail!("old_string is not unique in {label} ({count} matches); add context or set replace_all");
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        let start_line = content.find(old).map(|p| line_at(content, p)).unwrap_or(0);
        return Ok(EditApplied {
            content: updated,
            before: old.to_string(),
            after: new.to_string(),
            count,
            rung: "exact",
            start_line,
        });
    }
    // R2 indent-tolerant (kept uncapped + byte-stable messages — the original fallback).
    if let Some(a) = block_rung(content, old, new, trim_norm, "indent", label, replace_all)? {
        return Ok(a);
    }
    let within_caps = old.lines().count() <= LADDER_MAX_LINES && old.len() <= LADDER_MAX_BYTES;
    if within_caps {
        // R3 whitespace-run-normalized.
        if let Some(a) = block_rung(
            content,
            old,
            new,
            ws_collapse,
            "ws-norm",
            label,
            replace_all,
        )? {
            return Ok(a);
        }
        // R4 anchor-trim: drop a SHARED (old==new) first/last context line from both sides.
        for (o2, n2) in anchor_trim_variants(old, new) {
            if let Some(a) = try_pair(content, &o2, &n2, "anchor-trim", label, replace_all)? {
                return Ok(a);
            }
        }
        // R5 escape-normalized (only when unescaping actually changes old).
        if let Some(o2) = json_unescape(old) {
            let n2 = json_unescape(new).unwrap_or_else(|| new.to_string());
            if let Some(a) = try_pair(content, &o2, &n2, "unescape", label, replace_all)? {
                return Ok(a);
            }
        }
        // R5.5 blank-line-insensitive: the model added or dropped a blank line INSIDE the block, so
        // its line count no longer lines up with the file and the fixed-`k` rungs above all miss.
        // Match on the non-blank trimmed lines only; splice the whole spanned region (interior blanks
        // included) so `new` fully redefines it. Same 1-match invariant as every other rung.
        {
            let ranges = blank_insensitive_blocks(content, old);
            match ranges.len() {
                0 => {}
                1 => {
                    let (bs, be) = ranges[0];
                    let before = content[bs..be].to_string();
                    let spliced = preserve_eol(new, &before, &content[be..]);
                    let updated = format!("{}{}{}", &content[..bs], spliced, &content[be..]);
                    return Ok(EditApplied {
                        content: updated,
                        before,
                        after: new.to_string(),
                        count: 1,
                        rung: "blank-norm",
                        start_line: line_at(content, bs),
                    });
                }
                n => {
                    if replace_all {
                        return Ok(splice_all(content, &ranges, new, "blank-norm"));
                    }
                    bail!(
                        "old_string (ignoring blank lines) matches {n} blocks in {label}; add more surrounding context to disambiguate"
                    )
                }
            }
        }
    }
    // R6 terminal: nearest-miss report so the model's retry can copy the real bytes.
    if content.lines().count() <= NEAREST_MISS_MAX_FILE_LINES {
        if let Some(hint) = nearest_miss(content, old) {
            bail!(
                "old_string not found in {label} (tried exact, indentation-tolerant, \
                 whitespace-normalized, anchor-trimmed, escape-normalized, and \
                 blank-line-insensitive matching).\n{hint}"
            );
        }
    }
    bail!("old_string not found in {label} (even ignoring indentation) — re-read the file; it may have changed")
}

/// One full sub-ladder (R1 exact → R2 trim → R3 ws-collapse) over a DERIVED (old,new) pair —
/// the driver for the R4/R5 variants. Same invariant: 1 ⇒ apply as `rung`, >1 ⇒ hard error,
/// 0 ⇒ `None` (fall through).
fn try_pair(
    content: &str,
    old: &str,
    new: &str,
    rung: &'static str,
    label: &str,
    replace_all: bool,
) -> Result<Option<EditApplied>> {
    if old.trim().is_empty() {
        return Ok(None); // a variant that trimmed away all signal proves nothing
    }
    let count = content.matches(old).count();
    if count == 1 || (count > 1 && replace_all) {
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        let start_line = content.find(old).map(|p| line_at(content, p)).unwrap_or(0);
        return Ok(Some(EditApplied {
            content: updated,
            before: old.to_string(),
            after: new.to_string(),
            count,
            rung,
            start_line,
        }));
    }
    if count > 1 {
        bail!("old_string ({rung} form) matches {count} places in {label}; add more surrounding context to disambiguate");
    }
    if let Some(a) = block_rung(content, old, new, trim_norm, rung, label, replace_all)? {
        return Ok(Some(a));
    }
    block_rung(content, old, new, ws_collapse, rung, label, replace_all)
}

/// One block-matching rung over normalized lines: exactly 1 block ⇒ apply, >1 ⇒ hard ambiguous
/// error, 0 ⇒ `None` (fall through). The "indent" wording stays byte-identical to the original.
fn block_rung(
    content: &str,
    old: &str,
    new: &str,
    norm: fn(&str) -> String,
    rung: &'static str,
    label: &str,
    replace_all: bool,
) -> Result<Option<EditApplied>> {
    let ranges = normalized_blocks(content, old, norm);
    match ranges.len() {
        0 => Ok(None),
        1 => {
            let (bs, be) = ranges[0];
            let before = content[bs..be].to_string();
            let spliced = preserve_eol(new, &before, &content[be..]);
            let updated = format!("{}{}{}", &content[..bs], spliced, &content[be..]);
            Ok(Some(EditApplied {
                content: updated,
                before,
                after: new.to_string(),
                count: 1,
                rung,
                start_line: line_at(content, bs),
            }))
        }
        n => {
            if replace_all {
                return Ok(Some(splice_all(content, &ranges, new, rung)));
            }
            if rung == "indent" {
                bail!("old_string (ignoring indentation) matches {n} blocks in {label}; add more surrounding context to disambiguate");
            }
            bail!("old_string ({rung} form) matches {n} blocks in {label}; add more surrounding context to disambiguate");
        }
    }
}

/// `replace_all` on a tolerant rung: splice `new` into EVERY matched block (ascending byte ranges;
/// a block that overlaps the previous splice is skipped). `before`/`start_line` describe the first
/// block — the diff preview shows one representative hunk, the summary carries the count.
fn splice_all(
    content: &str,
    ranges: &[(usize, usize)],
    new: &str,
    rung: &'static str,
) -> EditApplied {
    let mut updated = String::with_capacity(content.len());
    let mut cursor = 0usize;
    let mut count = 0usize;
    for &(bs, be) in ranges {
        if bs < cursor {
            continue;
        }
        updated.push_str(&content[cursor..bs]);
        updated.push_str(&preserve_eol(new, &content[bs..be], &content[be..]));
        cursor = be;
        count += 1;
    }
    updated.push_str(&content[cursor..]);
    let (fs, fe) = ranges[0];
    EditApplied {
        content: updated,
        before: content[fs..fe].to_string(),
        after: new.to_string(),
        count,
        rung,
        start_line: line_at(content, fs),
    }
}

/// Preserve the file's line endings across a tolerant-rung splice. The matched span (`before`)
/// includes the trailing `\r` of a CRLF line while the `\n` itself sits just past it — `after_be`
/// (the tail from `be`) starts with that `\n`. A model-supplied `new` is typically bare-LF, so a
/// naive splice drops the boundary `\r` and leaves a lone-LF line among CRLF neighbours (Windows
/// mixed-ending corruption). When the replaced region was CRLF, convert `new`'s internal breaks to
/// CRLF and restore the boundary `\r`. LF files are returned untouched (byte-identical to before).
fn preserve_eol(new: &str, before: &str, after_be: &str) -> String {
    let crlf = before.contains("\r\n") || before.ends_with('\r');
    if !crlf {
        return new.to_string();
    }
    let mut out = if new.contains('\n') && !new.contains("\r\n") {
        new.replace('\n', "\r\n")
    } else {
        new.to_string()
    };
    if after_be.starts_with('\n') && !out.ends_with('\r') {
        out.push('\r');
    }
    out
}

/// Per-line trim — the R2 normalization (leading indentation + trailing whitespace + CRLF).
fn trim_norm(l: &str) -> String {
    l.trim().to_string()
}

/// Per-line whitespace-INSENSITIVE comparison — the R3 normalization (`foo( a,  b )` ≡
/// `foo(a, b)`). All whitespace is dropped for matching; the theoretical conflation (`let x` vs
/// `letx`) is guarded by the exactly-1-match invariant, and the rung name in the result summary
/// makes any loose match visible.
fn ws_collapse(l: &str) -> String {
    l.split_whitespace().collect::<Vec<_>>().concat()
}

/// R4 variants: when the first and/or last line of `old` is BYTE-IDENTICAL in `new` (pure
/// unchanged context), dropping it from BOTH sides cannot change the result — but can rescue a
/// match when that context line was mis-copied elsewhere. Order: drop-first, drop-last, drop-both.
fn anchor_trim_variants(old: &str, new: &str) -> Vec<(String, String)> {
    let ol: Vec<&str> = old.lines().collect();
    let nl: Vec<&str> = new.lines().collect();
    let mut out = Vec::new();
    if ol.len() < 2 || nl.is_empty() {
        return out;
    }
    let first_shared = ol.first() == nl.first();
    let last_shared = ol.last() == nl.last();
    if first_shared {
        out.push((ol[1..].join("\n"), nl[1..].join("\n")));
    }
    if last_shared {
        out.push((ol[..ol.len() - 1].join("\n"), nl[..nl.len() - 1].join("\n")));
    }
    if first_shared && last_shared && ol.len() >= 3 && nl.len() >= 2 {
        out.push((
            ol[1..ol.len() - 1].join("\n"),
            nl[1..nl.len() - 1].join("\n"),
        ));
    }
    out
}

/// Undo one level of JSON string escaping (`\n` `\t` `\r` `\"` `\\`) — the double-escaped-arguments
/// failure mode where the model ships literal `\n` sequences instead of newlines. `None` when
/// nothing changed (the rung is then skipped). Unknown escapes pass through untouched.
fn json_unescape(s: &str) -> Option<String> {
    if !s.contains('\\') {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    let mut changed = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => {
                    out.push('\n');
                    changed = true;
                }
                Some('t') => {
                    out.push('\t');
                    changed = true;
                }
                Some('r') => {
                    out.push('\r');
                    changed = true;
                }
                Some('"') => {
                    out.push('"');
                    changed = true;
                }
                Some('\\') => {
                    out.push('\\');
                    changed = true;
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    changed.then_some(out)
}

/// R6: the best-scoring window of the file vs `old` (mean per-line Jaro-Winkler on trimmed
/// lines), quoted with line numbers so the model's retry copies REAL bytes. `None` below the 0.55
/// similarity floor ("no similar region" is more honest than a random quote).
fn nearest_miss(content: &str, old: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let ol: Vec<&str> = old.lines().collect();
    let k = ol.len().clamp(1, LADDER_MAX_LINES);
    if lines.is_empty() || lines.len() < k {
        return None;
    }
    let mut best_i = 0usize;
    let mut best_score = 0.0f64;
    for i in 0..=(lines.len() - k) {
        let score: f64 = (0..k)
            .map(|j| strsim::jaro_winkler(lines[i + j].trim(), ol.get(j).map_or("", |l| l.trim())))
            .sum::<f64>()
            / k as f64;
        if score > best_score {
            best_score = score;
            best_i = i;
        }
    }
    if best_score < 0.55 {
        return None;
    }
    let ctx_start = best_i.saturating_sub(3);
    let ctx_end = (best_i + k + 3).min(lines.len());
    let mut excerpt = String::new();
    for (n, line) in lines[ctx_start..ctx_end].iter().enumerate() {
        excerpt.push_str(&format!("{:>5}| {line}\n", ctx_start + n + 1));
        if excerpt.len() > 2_000 || n >= 40 {
            excerpt.push_str("    …\n");
            break;
        }
    }
    Some(format!(
        "Nearest match (similarity {best_score:.2}) at lines {}-{}:\n{excerpt}Copy old_string EXACTLY from the excerpt above (including whitespace) and retry.",
        best_i + 1,
        best_i + k
    ))
}

/// Find full-line blocks in `content` that match `old` under a per-line normalization. Returns the
/// byte range [start, end) of each matching block (line-aligned: from the first matched line's
/// start through the last matched line's end, excluding the trailing newline).
fn normalized_blocks(content: &str, old: &str, norm: fn(&str) -> String) -> Vec<(usize, usize)> {
    let old_norm: Vec<String> = old.lines().map(norm).collect();
    let k = old_norm.len();
    if k == 0 {
        return Vec::new();
    }
    // Byte spans of every line in content (line content excludes the '\n').
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            spans.push((start, idx));
            start = idx + 1;
        }
    }
    spans.push((start, content.len()));

    let cnorm: Vec<String> = spans.iter().map(|&(a, b)| norm(&content[a..b])).collect();
    let mut out = Vec::new();
    if cnorm.len() < k {
        return out;
    }
    for i in 0..=(cnorm.len() - k) {
        if (0..k).all(|j| cnorm[i + j] == old_norm[j]) {
            out.push((spans[i].0, spans[i + k - 1].1));
        }
    }
    out
}

/// R5.5: find blocks matching `old` when blank lines are IGNORED on both sides — the model added or
/// dropped an empty line inside the block, so its fixed line count no longer aligns and every rung
/// above (which needs exactly `k` consecutive lines) misses. The comparison is over the trimmed,
/// NON-blank lines only; the returned byte range still covers the WHOLE spanned region in the file
/// (interior blank lines included), so splicing `new` in redefines it wholesale. Returns each
/// matching block's `[start, end)` (line-aligned, excluding the trailing newline). A block whose
/// non-blank content is empty proves nothing → no ranges (the caller then falls through to R6).
fn blank_insensitive_blocks(content: &str, old: &str) -> Vec<(usize, usize)> {
    // The signature we match: `old`'s non-blank lines, trimmed.
    let want: Vec<String> = old
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let k = want.len();
    if k == 0 {
        return Vec::new();
    }
    // Byte spans of every line in content (line content excludes the '\n').
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            spans.push((start, idx));
            start = idx + 1;
        }
    }
    spans.push((start, content.len()));

    // Index the NON-blank lines (trimmed text + which content-line they came from), so a matched run
    // of `k` non-blank lines can be mapped back to the first/last physical line for the byte span.
    let nonblank: Vec<(usize, String)> = spans
        .iter()
        .enumerate()
        .map(|(li, &(a, b))| (li, content[a..b].trim().to_string()))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    if nonblank.len() < k {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..=(nonblank.len() - k) {
        if (0..k).all(|j| nonblank[i + j].1 == want[j]) {
            let first_line = nonblank[i].0;
            let last_line = nonblank[i + k - 1].0;
            out.push((spans[first_line].0, spans[last_line].1));
        }
    }
    out
}

/// A compact **unified diff** of an edit: common leading/trailing lines are trimmed away (so a big
/// block collapses to just its changed window), the removed lines are prefixed `-`, the added lines
/// `+`, and a couple of context lines (prefixed with a space) bracket the change, under a
/// `@@ -N,c +N,c @@` hunk header carrying the window's 1-based file line. Gives the model
/// cheap verifiability AND is what the TUI renders as the side-by-side diff panes — the lines
/// are `^[-+]`-prefixed at column 0 so the display can pick them out unambiguously. Both sides are
/// capped so a giant replacement can't flood the result.
///
/// Callers pass the SMALL changed region (single form: `applied.before`/`after`; batch form: each
/// per-edit before/after), so the prefix/suffix trim is enough — no full LCS needed.
fn diff_preview(before: &str, after: &str, base_line: usize) -> String {
    const CTX: usize = 2; // context lines kept on each side of the change
    const MAX_SIDE: usize = 40; // cap removed / added lines shown per side

    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();

    // Longest common prefix, then longest common suffix that doesn't overlap the prefix.
    let mut p = 0;
    while p < b.len() && p < a.len() && b[p] == a[p] {
        p += 1;
    }
    let mut s = 0;
    while s < b.len().saturating_sub(p)
        && s < a.len().saturating_sub(p)
        && b[b.len() - 1 - s] == a[a.len() - 1 - s]
    {
        s += 1;
    }
    let removed = &b[p..b.len() - s];
    let added = &a[p..a.len() - s];

    let mut out = String::new();
    // `@@ -N,c +N,c @@` hunk header anchoring the window in the FILE (`base_line` = 1-based file
    // line of `before`'s first line; 0 = unknown → header omitted). Counts are the true window
    // sizes even when the visual cap below elides lines — the `…` note says what was cut. Gives
    // the model a line anchor for follow-up edits and gives the TUI its gutter numbers.
    let lead = p.min(CTX);
    if base_line > 0 {
        let start = base_line + p - lead;
        let trail = s.min(CTX);
        out.push_str(&format!(
            "@@ -{start},{} +{start},{} @@\n",
            lead + removed.len() + trail,
            lead + added.len() + trail
        ));
    }
    // leading context (from the shared prefix)
    for line in &b[p.saturating_sub(CTX)..p] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in removed.iter().take(MAX_SIDE) {
        out.push_str(&format!("-{line}\n"));
    }
    if removed.len() > MAX_SIDE {
        out.push_str(&format!(
            "…({} more lines removed)\n",
            removed.len() - MAX_SIDE
        ));
    }
    for line in added.iter().take(MAX_SIDE) {
        out.push_str(&format!("+{line}\n"));
    }
    if added.len() > MAX_SIDE {
        out.push_str(&format!("…({} more lines added)\n", added.len() - MAX_SIDE));
    }
    // trailing context (from the shared suffix)
    let suf_start = b.len() - s;
    for line in &b[suf_start..(suf_start + CTX).min(b.len())] {
        out.push_str(&format!(" {line}\n"));
    }
    out.trim_end_matches('\n').to_string()
}

// ── shell_run ────────────────────────────────────────────────────────────────

/// Restore detection is broader than target proof. Only a simple direct Git command without
/// routing options has a cwd-bound target; wrappers/chains/alternate worktrees fail before spawn.
fn shell_restore_preflight(command: &str, ctx: Option<&ExecutionContext>) -> Result<bool> {
    if !crate::agent::cmd_guard::delegated_restore_command(command) {
        return Ok(false);
    }
    if ctx.is_some_and(|ctx| !ctx.can_restore_workspace()) {
        bail!("delegated shell_run cannot restore workspace state");
    }
    if command.len() > 64 * 1024 {
        bail!("shell restore target exceeds the bounded preflight limit");
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let direct_git = words
        .first()
        .is_some_and(|word| matches!(*word, "git" | "git.exe"));
    if !direct_git {
        bail!("shell restore target cannot be frozen safely; nested aizen time restore/undo can self-lock; use the parent checkpoint/native owner undo tool");
    }
    if command.len() > 64 * 1024
        || command.chars().any(|ch| {
            matches!(
                ch,
                '\'' | '"'
                    | ';'
                    | '&'
                    | '|'
                    | '\r'
                    | '\n'
                    | '`'
                    | '$'
                    | '#'
                    | '<'
                    | '>'
                    | '('
                    | ')'
            )
        })
        || words.get(1).is_none_or(|word| word.starts_with('-'))
        || words.iter().any(|word| {
            word.starts_with("--work-tree")
                || word.starts_with("--git-dir")
                || word.starts_with("-C")
        })
    {
        bail!("shell restore target is ambiguous (routing options, quoting or command chain); use the parent checkpoint/native owner undo tool");
    }
    Ok(true)
}

/// Exclusion belongs to the physical tree and actual pipe readers, never the async observer.
struct ShellOwnership {
    _physical: crate::core::workspace_txn::PhysicalEffectLease,
    _ctx: Option<ExecutionContext>,
    _child: std::sync::Arc<Mutex<std::process::Child>>,
    _containment: std::sync::Arc<crate::core::proctree::Containment>,
}

static SHELL_QUARANTINE: Lazy<Mutex<Vec<ShellOwnership>>> = Lazy::new(|| Mutex::new(Vec::new()));

fn quarantine_shell(ownership: ShellOwnership) {
    SHELL_QUARANTINE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(ownership);
}

type ShellLifetime = crate::agent::process::SettlementLifetime<ShellOwnership>;

/// Also runs on early error/unwind. Unknown containment never becomes a fabricated exit proof.
struct ShellSettlement {
    lifetime: Option<std::sync::Arc<ShellLifetime>>,
    child: std::sync::Arc<Mutex<std::process::Child>>,
    containment: std::sync::Arc<crate::core::proctree::Containment>,
    waiter: Option<crate::agent::process::DrainLifetime<ShellOwnership>>,
}

impl Drop for ShellSettlement {
    fn drop(&mut self) {
        let Some(lifetime) = self.lifetime.clone() else {
            // Gate OFF retains legacy serial cleanup, with no process-long exclusion claim.
            let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
            crate::core::proctree::kill_tree(&mut child, &self.containment);
            return;
        };
        let child = self.child.clone();
        let containment = self.containment.clone();
        let waiter = self.waiter.take();
        let _ = std::thread::Builder::new()
            .name("shell-settlement".into())
            .spawn(move || {
                let _waiter = waiter;
                {
                    let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                    crate::core::proctree::kill_tree(&mut child, &containment);
                }
                loop {
                    match containment.known_quiescent() {
                        Ok(Some(true)) => {
                            lifetime.prove_tree_settled();
                            break;
                        }
                        Ok(Some(false)) => std::thread::sleep(Duration::from_millis(40)),
                        Ok(None) | Err(_) => break, // Final ownership drop quarantines unknown trees.
                    }
                }
            });
    }
}

struct ShellRun {
    root: PathBuf,
}
impl ShellRun {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for ShellRun {
    fn name(&self) -> &str {
        "shell_run"
    }
    /// The full command line and the directory it runs in — the header clips a command to
    /// its first word, and `python deploy.py --prod --force-delete` approved as `deploy.py`
    /// is the audit's first finding.
    fn preview(&self, args: &Value) -> Option<crate::agent::tools::ApprovalPreview> {
        let command = args.get("command")?.as_str()?;
        let dir = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(c) => confine(&self.root, c, true)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| c.to_string()),
            None => self.root.display().to_string(),
        };
        let mut lines = vec![format!("cwd: {dir}"), format!("$ {command}")];
        if args
            .get("network")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            lines.push("network: requested".to_string());
        }
        Some(crate::agent::tools::ApprovalPreview {
            title: "shell_run".to_string(),
            diff: None,
            lines,
        })
    }
    fn description(&self) -> &str {
        "Run shell in working dir for build/test/tools/file management; returns stdout/stderr + exit \
         code. Content search -> search_files, not grep. Timeout 120s (AIZEN_SHELL_TIMEOUT_SECS, \
         10..3600) kills whole process tree. Long builds/servers/watchers -> process (uncapped). \
         Destructive; user confirms."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string", "description": "optional working dir: subdir, ../ or absolute"},
                "network": {"type": "boolean", "description": "request network (default false; sandbox denies sockets where supported). Approval-gated."},
                "format": crate::agent::result_format::schema_property()
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::OpaqueWorkspace
    }
    fn command_to_guard(&self, args: &Value) -> Option<String> {
        args.get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let command = str_arg(args, "command")?;
        let ctx = crate::core::exec_ctx::current();
        if ctx.as_ref().is_some_and(|ctx| ctx.native_scope().is_some()) {
            bail!("shell_run is unavailable in a native-scoped execution context");
        }
        let scope = ctx
            .as_ref()
            .map(ExecutionContext::resource_scope)
            .unwrap_or_else(|| "shell_run/direct".to_string());
        let cancel = crate::core::cancel::current().or_else(crate::ui::tui::active_cancel_token);
        let restore = shell_restore_preflight(command, ctx.as_ref())?;
        let dir = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(c) => confine(&self.root, c, true)?,
            None => self.root.clone(),
        };
        // Resolve/warm repository identity before physical admission, never nested under that gate.
        let restore_root = if restore {
            let actual = crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&dir)?
                .canonical_root;
            crate::core::workspace_txn::WorkspaceIdentity::discover_cached(&actual)?;
            Some(actual)
        } else {
            None
        };
        let network = args
            .get("network")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = Duration::from_secs(shell_timeout_secs());
        // One construction path for every model-run command: the sandbox runner wraps the platform
        // shell (same `cmd /C chcp 65001>nul` / `sh -c` shape as always), scrubs the environment,
        // arms the platform backend, and refuses outright where policy says so (strict without
        // kernel backing, unattended without opt-in).
        let mut sbx = crate::sandbox::runner::prepare_std(
            crate::sandbox::request::SandboxRequest::shell(
                crate::sandbox::CommandOrigin::ShellRun,
                command,
                dir.clone(),
                self.root.clone(),
            )
            .network(network)
            .wall_timeout(timeout),
        )?;
        // stdin = null (not inherited): on Windows a shared console-input handle lets `cmd.exe`
        // (and the CRT) call SetConsoleMode on OUR input buffer and reset it on exit — clearing
        // ENABLE_MOUSE_INPUT and re-enabling QuickEdit. That silently kills the retained TUI's mouse
        // capture, so after a shell command the wheel leaks back through as ↑/↓ keys and the
        // transcript can no longer be scrolled (the "can't scroll during/after a task" bug). A null
        // stdin means the child never touches the console input handle, so our mode survives. The
        // command already runs non-interactively (drained pipes, wall-clock timeout), so it has no
        // legitimate use for the terminal's stdin anyway.
        sbx.command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Fresh physical admission is never borrowed from inherited/scoped writer bookkeeping.
        // The executor excludes shell_run from its central physical gate to avoid double admission.
        let strict_lifetime = crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED;
        let physical = if strict_lifetime {
            Some(crate::core::workspace_txn::PhysicalEffectLease::exclusive(
                Duration::from_secs(15),
                cancel.as_ref(),
                "shell_run physical body",
                &scope,
            )?)
        } else {
            None
        };
        let _restore_guard = restore_root
            .as_deref()
            .map(crate::core::workspace_txn::WorkspaceRestoreGuard::acquire)
            .transpose()?;
        if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
            sbx.finish(crate::sandbox::runner::Outcome::Cancelled);
            return Ok("error: command cancelled before shell spawn".to_string());
        }
        // Contain the tree BEFORE anything can go wrong with it: on Windows we spawn a `cmd.exe`
        // wrapper, so killing our direct child leaves the real work (cargo, node, a dev server)
        // orphaned — still running, and still holding the write end of the pipes below.
        let (mut child, containment) = match sbx.spawn_contained() {
            Ok(spawned) => spawned,
            Err(error) => {
                if !error.cleanup_verified() {
                    let note = error.to_string();
                    // An unproved protected-spawn cleanup retains actual child/job, sandbox and
                    // physical/context ownership. Never convert it into a settled spawn failure.
                    std::mem::forget((error, physical, ctx, sbx, _restore_guard));
                    return Err(
                        anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(
                            crate::agent::tools::ToolEffectState::UnknownWorker,
                        ))
                        .context(note),
                    );
                }
                return Err(error).with_context(|| format!("spawning shell for `{command}`"));
            }
        };
        let containment = std::sync::Arc::new(containment);
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let child = std::sync::Arc::new(Mutex::new(child));
        let lifetime = physical.map(|physical| {
            std::sync::Arc::new(ShellLifetime::new(
                ShellOwnership {
                    _physical: physical,
                    _ctx: ctx,
                    _child: child.clone(),
                    _containment: containment.clone(),
                },
                quarantine_shell,
            ))
        });
        // Gate OFF never seats long-lived writer/context ownership or permanent quarantine.
        // Gate ON registers every actual worker before any can publish settlement.
        let out_lifetime = lifetime.as_ref().map(|lifetime| lifetime.drain());
        let err_lifetime = lifetime.as_ref().map(|lifetime| lifetime.drain());
        let _settlement = ShellSettlement {
            waiter: lifetime.as_ref().map(|lifetime| lifetime.drain()),
            lifetime,
            child: child.clone(),
            containment: containment.clone(),
        };
        // Drain tokens outlive bounded observer joins, including detached pipe workers.
        let oh = std::thread::Builder::new()
            .name("shell-stdout".into())
            .spawn(move || {
                let _lifetime = out_lifetime;
                drain(out_pipe)
            })?;
        let eh = std::thread::Builder::new()
            .name("shell-stderr".into())
            .spawn(move || {
                let _lifetime = err_lifetime;
                drain(err_pipe)
            })?;

        let start = Instant::now();
        let mut next_note = SLOW_NOTE_EVERY_SECS;
        let mut cancelled = false;
        let mut wait_error = None;
        let status = loop {
            let waited = child
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .try_wait();
            match waited {
                Ok(Some(st)) => break Some(st),
                Err(error) => {
                    wait_error = Some(error);
                    let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                    crate::core::proctree::kill_tree(&mut child, &containment);
                    break None;
                }
                Ok(None) => {
                    // User pressed Esc (cooperative cancel) — kill the tree now instead of blocking
                    // the whole turn up to the timeout. This is what makes Esc responsive during a
                    // long command (the confirmed "can't cancel while a tool runs" bug).
                    let cancel_requested =
                        cancel.as_ref().is_some_and(|token| token.is_cancelled());
                    if cancel_requested {
                        let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                        crate::core::proctree::kill_tree(&mut child, &containment);
                        cancelled = true;
                        break None;
                    }
                    if start.elapsed() >= timeout {
                        let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                        crate::core::proctree::kill_tree(&mut child, &containment);
                        break None;
                    }
                    // Tell the user what is taking so long, once every 30s. The working pill already
                    // animates and counts elapsed seconds, so liveness is covered — what it cannot
                    // say is WHICH command is still out, or that a ceiling is approaching. Without
                    // that, a 2-minute build and a wedged process look identical from the outside.
                    let waited = start.elapsed().as_secs();
                    if waited >= next_note {
                        note_slow_command(command, waited, timeout.as_secs());
                        next_note = waited + SLOW_NOTE_EVERY_SECS;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
        };
        // Wrapper completion is not tree completion; terminate remaining contained descendants.
        // The detached settlement monitor owns the proof and retains all gates on uncertainty.
        crate::core::proctree::terminate_tree(&containment);
        // Never join a pipe reader unbounded. If containment failed (or a descendant escaped the job
        // via its own detached wrapper) an orphan can still hold the write end, and `read_to_end`
        // only returns on EOF = last writer closed — a wait that a wedged process never ends. A
        // reproduction of exactly this blocked >12s on a 45s sleeper. Take whatever the readers have
        // finished within the grace window; a lost tail beats a frozen agent.
        let (mut stdout, out_cut) = crate::core::proctree::join_drain(oh, DRAIN_GRACE);
        let (stderr, err_cut) = crate::core::proctree::join_drain(eh, DRAIN_GRACE);
        if out_cut || err_cut {
            // Say so rather than presenting a truncated log as complete — the model would otherwise
            // reason from output it cannot tell is partial.
            stdout.push_str(
                "\n[output truncated: a surviving child process still held the pipe open; \
                 the command's own result above may be incomplete]",
            );
        }

        sbx.finish(match status {
            None if cancelled => crate::sandbox::runner::Outcome::Cancelled,
            None => crate::sandbox::runner::Outcome::Timeout,
            Some(st) => crate::sandbox::runner::Outcome::Exit(st.code()),
        });
        if let Some(error) = wait_error {
            return Err(error).with_context(|| {
                format!(
                    "waiting for shell `{command}`; physical ownership retained until settlement"
                )
            });
        }
        match status {
            None if cancelled => Ok("error: command cancelled by the user (Esc)".to_string()),
            None => {
                // Surface stderr too (the success branch already does) so a killed build's diagnostics
                // aren't lost to the model — they're often the most useful part of a timeout.
                let secs = shell_timeout_secs();
                // Report the kill's real reach. Claiming "whole tree" when containment was
                // unavailable would hide a live orphan from the one person who could stop it.
                let scope = if containment.is_contained() {
                    "killed, whole process tree"
                } else {
                    "killed the shell only — this platform refused process containment, so a \
                     descendant may still be running"
                };
                let mut s = format!(
                    "error: command timed out after {secs}s ({scope})\n\
                     If it needs longer, re-run it with the `process` tool (action=start) which has \
                     no wall-clock cap, or raise AIZEN_SHELL_TIMEOUT_SECS.\n{stdout}"
                );
                if !stderr.trim().is_empty() {
                    s.push_str("\n[stderr]\n");
                    s.push_str(&stderr);
                }
                Ok(crate::agent::result_format::finish_log(
                    "shell_run",
                    args,
                    s.trim_end().to_string(),
                ))
            }
            Some(st) => {
                let mut s = format!("exit {}\n", st.code().unwrap_or(-1));
                s.push_str(&stdout);
                if !stderr.trim().is_empty() {
                    s.push_str("\n[stderr]\n");
                    s.push_str(&stderr);
                }
                Ok(crate::agent::result_format::finish_log(
                    "shell_run",
                    args,
                    s.trim_end().to_string(),
                ))
            }
        }
    }
}

/// Read a child pipe to a string (best-effort; used on a drain thread).
///
/// Reads RAW BYTES then decodes lossily — NOT `read_to_string`, which returns `Err` and leaves
/// the buffer EMPTY on the first invalid-UTF-8 byte. On non-English Windows, `cmd`/PowerShell emit
/// output in the OEM/ANSI codepage (CP437/850/1258, etc.), so `dir` listings and accented
/// filenames are not valid UTF-8 → `read_to_string` would silently drop the ENTIRE output and the
/// agent would see a blank result. Lossy decoding keeps the ASCII structure (paths, sizes, the
/// listing layout) and only the odd accented byte degrades to `�`.
///
/// BOUNDED IN RAM, not just in the transcript: the old `read_to_end` buffered the child's whole
/// output before `truncate_result` ever saw it, so one `cat`ed multi-gigabyte file OOMed the CLI
/// (the `process` tool ring-buffers at ingest; this path forgot to). Head + tail are kept —
/// build/test logs put the verdict at the end — with a marker naming what fell out.
pub(crate) fn drain<R: std::io::Read>(pipe: Option<R>) -> String {
    /// First bytes kept verbatim (the command header, the first error).
    const HEAD: usize = 1 << 20; // 1 MB
    /// Last bytes kept verbatim (the exit summary, the failing test names).
    const TAIL: usize = 1 << 20; // 1 MB
    let Some(mut p) = pipe else {
        return String::new();
    };
    let mut head: Vec<u8> = Vec::new();
    let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut dropped: usize = 0;
    let mut buf = [0u8; 8192];
    loop {
        match p.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut chunk = &buf[..n];
                if head.len() < HEAD {
                    let take = (HEAD - head.len()).min(chunk.len());
                    head.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                }
                if !chunk.is_empty() {
                    tail.extend(chunk.iter().copied());
                    if tail.len() > TAIL {
                        let excess = tail.len() - TAIL;
                        tail.drain(..excess);
                        dropped += excess;
                    }
                }
            }
        }
    }
    let mut out = String::from_utf8_lossy(&head).into_owned();
    if dropped > 0 {
        out.push_str(&format!(
            "\n… [{dropped} bytes omitted — output exceeded the in-memory cap; first and last \
             1 MB kept] …\n"
        ));
    }
    if !tail.is_empty() {
        let tail_bytes: Vec<u8> = tail.into();
        out.push_str(&String::from_utf8_lossy(&tail_bytes));
    }
    out
}

// ── git_inspect ──────────────────────────────────────────────────────────────

/// Wall-clock cap for one `git_inspect` query. Generous for `blame` on a large file; a read-only
/// query that needs longer is a query to narrow, not to wait on.
const GIT_INSPECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Transcript cap for one `git_inspect` result (chars). A whole-tree `git diff` can be megabytes;
/// the drain threads already bound RAM, this bounds what rides the conversation.
const GIT_INSPECT_MAX_CHARS: usize = 20_000;

/// Default / ceiling for `log`'s commit count.
const GIT_LOG_DEFAULT: usize = 20;
const GIT_LOG_CAP: usize = 100;

/// Read-only git queries for sub-agents that hold NO shell. Without this, the read-only roles are
/// blind to version control: a `nemesis` (reviewer) dispatch cannot see the diff it was asked to
/// review, and a `metis` (planner) cannot check what recently changed. The action set is a CLOSED
/// enum mapped to fixed argv (exec form, no shell anywhere), with the two model-supplied strings
/// (`path`, `rev`) validated here — so mutation is impossible by construction, not by prompt.
///
/// Shell-capable scopes get it too (it rides `subagent_read_only_base`): redundant next to
/// `shell_run`, but a uniform base keeps the read-only classification in one place.
struct GitInspect {
    root: PathBuf,
}
impl GitInspect {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Accept a revision / range argument (`HEAD~3`, `main..HEAD`, `a1b2c3`, `v0.6.6`, `@{u}`,
/// `HEAD:src/lib.rs`). Refuses anything that could read as a git OPTION (leading `-`) or smuggle
/// bytes past the argv boundary (whitespace/control chars), and anything absurdly long.
fn valid_git_rev(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '/' | '~' | '^' | '@' | '{' | '}' | ':' | '-')
        })
}

impl Tool for GitInspect {
    fn name(&self) -> &str {
        "git_inspect"
    }
    fn description(&self) -> &str {
        "Read-only git status/log/diff/commit/blame to see what changed and when. No \
         add/commit/push/checkout. diff without rev shows uncommitted changes; blame needs path."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "log", "diff", "show", "blame"]},
                "path": {"type": "string", "description": "optional file/dir filter; required for blame"},
                "rev": {"type": "string", "description": "revision/range (HEAD~3, main..HEAD, a1b2c3); status ignores; show defaults HEAD"},
                "limit": {"type": "integer", "description": "log max commits (default 20, cap 100)"}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let action = str_arg(args, "action")?;
        let rev = match args.get("rev").and_then(|v| v.as_str()).map(str::trim) {
            None | Some("") => None,
            Some(r) if valid_git_rev(r) => Some(r.to_string()),
            Some(r) => {
                return Ok(format!(
                    "error: rev {r:?} is not a plain revision/range — flags and shell syntax are \
                     not accepted here"
                ));
            }
        };
        // The path rides to git AS WRITTEN (a pathspec, resolved by git against the workspace
        // cwd) rather than through `confine`: canonicalizing on Windows yields a `\\?\` verbatim
        // path git does not accept, and the `--` separator below already guarantees a hostile
        // name cannot become an option. Only argv-boundary hygiene is enforced here.
        let path = match args.get("path").and_then(|v| v.as_str()).map(str::trim) {
            None | Some("") => None,
            Some(p)
                if p.len() <= 500 && !p.starts_with('-') && !p.chars().any(char::is_control) =>
            {
                Some(p.to_string())
            }
            Some(p) => {
                return Ok(format!(
                    "error: path {p:?} is not a plain file path — flags and control characters \
                     are not accepted here"
                ));
            }
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, GIT_LOG_CAP))
            .unwrap_or(GIT_LOG_DEFAULT);

        // Fixed argv per action — the only model-shaped bytes are the validated rev + confined path.
        let mut argv: Vec<String> = vec!["-c".into(), "color.ui=false".into()];
        match action {
            "status" => argv.extend(["status".into(), "--short".into(), "--branch".into()]),
            "log" => {
                argv.extend([
                    "log".into(),
                    "--date=short".into(),
                    "--format=%h %ad %an — %s".into(),
                    format!("-n{limit}"),
                ]);
                if let Some(r) = &rev {
                    argv.push(r.clone());
                }
            }
            "diff" => {
                argv.extend(["diff".into(), "--stat".into(), "--patch".into()]);
                if let Some(r) = &rev {
                    argv.push(r.clone());
                }
            }
            "show" => {
                argv.extend(["show".into(), "--stat".into(), "--patch".into()]);
                argv.push(rev.clone().unwrap_or_else(|| "HEAD".into()));
            }
            "blame" => {
                let Some(_) = &path else {
                    return Ok("error: blame requires a path".to_string());
                };
                argv.extend(["blame".into(), "--date=short".into()]);
                if let Some(r) = &rev {
                    argv.push(r.clone());
                }
            }
            other => {
                return Ok(format!(
                    "error: unknown action {other:?} — use status | log | diff | show | blame"
                ));
            }
        }
        if let Some(p) = &path {
            argv.push("--".into());
            argv.push(p.clone());
        }

        // Same containment discipline as every model-influenced spawn (CLAUDE.md: through
        // `runner::prepare_*`, never a bare `Command::new`). Exec form: no shell in the path at all.
        let mut sbx = crate::sandbox::runner::prepare_std(
            crate::sandbox::request::SandboxRequest::exec(
                crate::sandbox::CommandOrigin::GitInspect,
                PathBuf::from("git"),
                argv,
                self.root.clone(),
                self.root.clone(),
            )
            .wall_timeout(GIT_INSPECT_TIMEOUT),
        )?;
        sbx.command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match sbx.command.spawn() {
            Ok(c) => c,
            Err(e) => {
                sbx.finish(crate::sandbox::runner::Outcome::SpawnFailed);
                return Ok(format!(
                    "error: could not run git ({e}) — is git installed and on PATH?"
                ));
            }
        };
        let containment = sbx.contain(&child);
        let oh = {
            let pipe = child.stdout.take();
            std::thread::spawn(move || drain(pipe))
        };
        let eh = {
            let pipe = child.stderr.take();
            std::thread::spawn(move || drain(pipe))
        };
        let start = Instant::now();
        let status = loop {
            match child.try_wait()? {
                Some(st) => break Some(st),
                None if start.elapsed() >= GIT_INSPECT_TIMEOUT => {
                    crate::core::proctree::kill_tree(&mut child, &containment);
                    break None;
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let (stdout, _) = crate::core::proctree::join_drain(oh, DRAIN_GRACE);
        let (stderr, _) = crate::core::proctree::join_drain(eh, DRAIN_GRACE);
        sbx.finish(match status {
            None => crate::sandbox::runner::Outcome::Timeout,
            Some(st) => crate::sandbox::runner::Outcome::Exit(st.code()),
        });
        let Some(st) = status else {
            return Ok(format!(
                "error: git {action} exceeded {}s — narrow it with path/rev/limit",
                GIT_INSPECT_TIMEOUT.as_secs()
            ));
        };
        if !st.success() {
            let msg = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Ok(format!("error: git {action} failed: {}", msg.trim()));
        }
        let mut out = stdout;
        if out.trim().is_empty() {
            out = match action {
                "status" => "clean — no uncommitted changes (branch line above absent means \
                             detached or unborn HEAD)"
                    .into(),
                "diff" => "no differences".into(),
                _ => "(no output)".into(),
            };
        }
        if out.chars().count() > GIT_INSPECT_MAX_CHARS {
            let clipped: String = out.chars().take(GIT_INSPECT_MAX_CHARS).collect();
            out = format!(
                "{clipped}\n… [truncated at {GIT_INSPECT_MAX_CHARS} chars — narrow with \
                 path/rev/limit]"
            );
        }
        Ok(out.trim_end().to_string())
    }
}

// ── skill_load ───────────────────────────────────────────────────────────────

struct SkillLoad;
impl Tool for SkillLoad {
    fn name(&self) -> &str {
        "skill_load"
    }
    fn description(&self) -> &str {
        "Load/follow reusable steps by name from <skills> when its trigger matches. \
         Facts -> memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "a skill name from <skills>"}},
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        match crate::skills::load_checked(name)? {
            Some(sk) => {
                // Voyager reinforcement: a real load is organic reuse — bump `uses` so a repeatedly
                // useful skill floats to the top of the always-on index and survives its line cap.
                // Best-effort: a bump failure (repo-shipped skill, or I/O) must never break the load.
                let _ = crate::skills::record_use_loaded(&sk);
                // Hebbian cross-kind link: this procedure fired alongside the facts recalled for the
                // turn, so wire them together. That is what later lets the facts of a NEW turn name
                // the skills that mattered last time — the "used together" signal is not derivable
                // from either store's contents, exactly like the fact-to-fact edges.
                crate::skills::note_skill_cofire(sk.key());
                Ok(crate::skills::render_loaded(&sk))
            }
            None => {
                let avail: Vec<String> = crate::skills::list()
                    .into_iter()
                    .map(|s| s.key().to_string())
                    .collect();
                Ok(format!(
                    "(no skill named '{name}'; available: {})",
                    avail.join(", ")
                ))
            }
        }
    }
}

// ── skill_save ───────────────────────────────────────────────────────────────

struct SkillSave;
impl Tool for SkillSave {
    fn name(&self) -> &str {
        "skill_save"
    }
    fn description(&self) -> &str {
        "Save a named reusable procedure for skill_load when user asks to remember HOW. \
         Facts/preferences -> memory. Writes ~/.aizen/skills; user confirms."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "description": {"type": "string", "description": "one-line summary"},
                "when": {"type": "string", "description": "trigger hint"},
                "body": {"type": "string", "description": "procedure (markdown)"},
                "scope": {"type": "string", "enum": ["global", "project"], "description": "global=everywhere (default); project=this workspace"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let when = args.get("when").and_then(|v| v.as_str()).unwrap_or("");
        let project_zone = matches!(
            args.get("scope").and_then(|v| v.as_str()).map(str::trim),
            Some("project")
        );
        let path = crate::skills::save_scoped(name, description, when, body, project_zone)?;
        Ok(format!("saved skill '{name}' → {}", path.display()))
    }
}

// ── skill_refine ─────────────────────────────────────────────────────────────

/// Voyager curriculum: improve an EXISTING skill's steps in place, archiving the prior version so
/// nothing learned is lost and the usage track record carries forward. This is the "skills get
/// better with experience" tool — distinct from `skill_save` (which mints a NEW skill and refuses
/// to touch an existing one).
struct SkillRefine;
impl Tool for SkillRefine {
    fn name(&self) -> &str {
        "skill_refine"
    }
    fn description(&self) -> &str {
        "Improve an existing skill after a better sequence works; use instead of skill_save. \
         Supply the whole replacement procedure. Bumps version, archives old copy, preserves usage."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "existing skill name from <skills>"},
                "body": {"type": "string", "description": "full replacement procedure"},
                "description": {"type": "string", "description": "optional summary; omitted keeps old"},
                "when": {"type": "string", "description": "optional trigger; omitted keeps old"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        let description = args.get("description").and_then(|v| v.as_str());
        let when = args.get("when").and_then(|v| v.as_str());
        let (version, archived) = crate::skills::refine(name, body, description, when)?;
        Ok(format!(
            "refined skill '{name}' → v{version} (prior version archived at {})",
            archived.display()
        ))
    }
}

// ── skill_forget ───────────────────────────────────────────────────────────────

/// Retire a skill the curriculum has outgrown. The counterpart `skill_save`/`skill_refine` lacked:
/// skills are minted AUTOMATICALLY by the end-of-turn secretary, so without a retire path the index
/// only ever grows and a wrong or superseded procedure keeps costing prompt lines forever. Soft, like
/// `memory_forget` — the file moves to `.archive/`, so this is reversible (`/skills` restores).
struct SkillForget;
impl Tool for SkillForget {
    fn name(&self) -> &str {
        "skill_forget"
    }
    fn description(&self) -> &str {
        "Retire wrong/obsolete/superseded skill from <skills> and loading; archived, reversible. \
         Still valid but needs better steps -> skill_refine."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "the skill's name (from <skills>)"}
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        if crate::skills::delete(name)? {
            Ok(format!(
                "retired skill '{name}' (archived — restorable from /skills)"
            ))
        } else {
            Ok(format!(
                "no writable skill named '{name}' (repo-shipped skills are removed in the repo)"
            ))
        }
    }
}

// ── persona_create ─────────────────────────────────────────────────────────────

/// Mint (or overwrite) a character persona mid-chat and optionally become it. This is what lets
/// "create a persona named X who is a grumpy detective, and be them" work conversationally — the
/// model fills in name/role/voice/body from the request and calls this tool.
struct PersonaCreate;
impl Tool for PersonaCreate {
    fn name(&self) -> &str {
        "persona_create"
    }
    fn description(&self) -> &str {
        "Create role-play persona and switch by default. CALL, not prose, for become/invent-character \
         requests or pasted card/system prompt to make/save as character. Extract/invent name; rewrite \
         paste into body (backstory/values/manner/voice/boundaries), inferring details without asking. \
         Switch fully applies next user message. User facts -> memory. Writes ~/.aizen/personas; user confirms."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "character name"},
                "role": {"type": "string", "description": "one-line identity"},
                "voice": {"type": "string", "description": "speaking style"},
                "body": {"type": "string", "description": "backstory/values/manner/boundaries (markdown)"},
                "activate": {"type": "boolean", "description": "switch now (default true)"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let voice = args.get("voice").and_then(|v| v.as_str()).unwrap_or("");
        let activate = args
            .get("activate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let path = crate::persona::save(name, role, voice, body)?;
        if activate {
            crate::persona::set_active(name)?;
            Ok(format!(
                "created persona '{name}' → {} and switched to it (takes full effect from the \
                 user's next message).",
                path.display()
            ))
        } else {
            Ok(format!(
                "created persona '{name}' → {} (not active; switch with /persona).",
                path.display()
            ))
        }
    }
}

#[cfg(test)]
pub(crate) fn file_registry_for_test(root: &Path) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FileWrite::new(root.to_path_buf())));
    registry.register(Box::new(FileMove::new(root.to_path_buf())));
    registry.register(Box::new(FileEdit::new(root.to_path_buf())));
    registry.register(Box::new(FileRead::new(root.to_path_buf())));
    registry
}

#[cfg(test)]
type NativeBodyProbeFn = dyn Fn(&str, &Path) + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct NativeBodyTestProbe(std::sync::Arc<NativeBodyProbeFn>);

#[cfg(test)]
impl NativeBodyTestProbe {
    pub(crate) fn new(probe: impl Fn(&str, &Path) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(probe))
    }
}

#[cfg(test)]
impl std::fmt::Debug for NativeBodyTestProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeBodyTestProbe")
    }
}

#[cfg(test)]
type NativeBodyHook = Box<dyn FnOnce(&str, &Path) + Send>;

#[cfg(test)]
pub(crate) fn with_native_body_hook_for_test<T>(hook: NativeBodyHook, f: impl FnOnce() -> T) -> T {
    tests::with_native_body_hook(hook, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        // Opt-in only on the worker executing this test's real body; never a global scheduler hook.
        static NATIVE_BODY_HOOK: std::cell::RefCell<Option<NativeBodyHook>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn native_body_rendezvous(tool: &str, target: &Path) {
        let hook = NATIVE_BODY_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook(tool, target);
        }
        if let Some(ctx) = crate::core::exec_ctx::current() {
            if let Some(probe) = ctx.native_body_probe() {
                (probe.0)(tool, target);
            }
        }
    }

    pub(super) fn with_native_body_hook<T>(hook: NativeBodyHook, f: impl FnOnce() -> T) -> T {
        struct Restore(Option<NativeBodyHook>);
        impl Drop for Restore {
            fn drop(&mut self) {
                NATIVE_BODY_HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let old = NATIVE_BODY_HOOK.with(|slot| slot.replace(Some(hook)));
        let _restore = Restore(old);
        f()
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aizen-agent-tool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// The default registry keys every file/shell tool off ONE root. A lane passes its own, so this
    /// pins that an explicit root is honoured verbatim (canonicalized) rather than silently falling
    /// back to the process cwd — the regression that would let one bot edit another bot's project.
    #[test]
    fn registry_root_comes_from_the_caller_when_supplied() {
        let root = temp_root("registry-root");
        let resolved = root.canonicalize().unwrap();
        assert_ne!(
            resolved,
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            "the temp root must differ from cwd or this test proves nothing"
        );
        // `default_registry_in` is the shared constructor both the None and Some(root) paths use;
        // asserting on it avoids needing live model credentials in a unit test.
        let r = default_registry_in(&resolved);
        assert!(
            r.names().iter().any(|n| n == "file_read"),
            "registry built for an explicit root still carries the file tools"
        );
    }

    #[test]
    fn whole_file_symbol_hint_fires_only_for_long_source_with_lsp() {
        // Fires: long source file, LSP live, under the hard budget.
        assert!(whole_file_symbol_hint(true, true, 800).is_some());
        // Silent: short file (nothing to save), non-source (no symbol tools apply),
        // LSP off (the tools aren't registered), and at/over the hard budget (budget_view
        // already warns there — no double nudge).
        assert!(whole_file_symbol_hint(true, true, FILE_READ_SYMBOL_HINT_LINES).is_none());
        assert!(whole_file_symbol_hint(false, true, 800).is_none());
        assert!(whole_file_symbol_hint(true, false, 800).is_none());
        assert!(whole_file_symbol_hint(true, true, FILE_READ_MAX_LINES + 1).is_none());
    }

    #[test]
    fn file_read_files_batch_reads_slices_and_reports_per_file_errors() {
        let root = temp_root("multi-read");
        std::fs::write(root.join("a.txt"), "alpha-1\nalpha-2\nalpha-3\n").unwrap();
        std::fs::write(root.join("b.txt"), "beta-1\nbeta-2\nbeta-3\nbeta-4\n").unwrap();
        let t = FileRead::new(root);
        let out = t
            .execute(&serde_json::json!({"files": [
                {"path": "a.txt"},
                {"path": "b.txt", "start": 2, "end": 3},
                {"path": "missing.txt"}
            ]}))
            .unwrap();
        // Whole file, a range slice, and an inline per-entry error — all in ONE result.
        assert!(out.contains("── a.txt ──"), "whole-file header: {out}");
        assert!(out.contains("alpha-3"), "whole-file body: {out}");
        assert!(out.contains("── b.txt:2-3 ──"), "range header: {out}");
        assert!(out.contains("beta-2\nbeta-3"), "range body: {out}");
        assert!(!out.contains("beta-4"), "range must exclude line 4: {out}");
        assert!(
            out.contains("── missing.txt ──\nerror:"),
            "a missing file reports inline instead of voiding the batch: {out}"
        );
    }

    #[test]
    fn file_read_files_batch_caps_entries_and_says_so() {
        let root = temp_root("multi-read-cap");
        for i in 0..10 {
            std::fs::write(root.join(format!("f{i}.txt")), format!("body-{i}\n")).unwrap();
        }
        let files: Vec<serde_json::Value> = (0..10)
            .map(|i| serde_json::json!({"path": format!("f{i}.txt")}))
            .collect();
        let t = FileRead::new(root);
        let out = t.execute(&serde_json::json!({ "files": files })).unwrap();
        assert!(out.contains("body-7"), "8th entry still read: {out}");
        assert!(
            !out.contains("body-8"),
            "9th entry must be dropped, not read: {out}"
        );
        assert!(
            out.contains("2 more file(s) not read"),
            "the drop is REPORTED, never silent: {out}"
        );
    }

    #[cfg(feature = "browser")]
    #[test]
    fn browser_schema_requires_explicit_configuration_without_losing_dispatch() {
        let root = temp_root("browser-availability");
        let absent = default_registry_for_browser(&root, false);
        let configured = default_registry_for_browser(&root, true);
        for name in [
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_eval",
            "browser_set_viewport",
            "browser_screenshot",
        ] {
            assert!(absent.get(name).is_none());
            assert!(configured.get(name).is_some());
            assert!(configured.defs().iter().any(|d| d.function.name == name));
        }
    }

    #[test]
    fn tool_schema_stays_within_budget() {
        // The default registry is the CORE top-level surface. Delegation and persona tools are
        // appended by `default_registry_with_task`; keeping this test explicit prevents a new core
        // tool from silently consuming the budget reserved for those conditional additions.
        const TOTAL_CEILING: usize = 31_000; // measured core surface after trimming file_glob
        const PER_TOOL_CEILING: usize = 1_400; // no single tool should dwarf the rest

        let root = temp_root("schema-budget");
        let defs = default_registry_for_browser(&root, false).defs();
        let mut rows: Vec<(String, usize)> = defs
            .iter()
            .map(|d| {
                (
                    d.function.name.clone(),
                    serde_json::to_string(d).unwrap().len(),
                )
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1));

        let total: usize = rows.iter().map(|r| r.1).sum();
        // A readable dump on failure so the diff that tripped the ratchet is obvious.
        let dump = || {
            let mut s = format!(
                "tool schema: count={}, total={total} B (ceiling {TOTAL_CEILING})\n",
                rows.len()
            );
            for (name, whole) in &rows {
                s.push_str(&format!("  {name:<26} {whole:>7}\n"));
            }
            s
        };
        assert!(
            total <= TOTAL_CEILING,
            "total tool schema {total} B exceeds the {TOTAL_CEILING} B budget — trim a \
             description or gate a tool rather than raising the ceiling.\n{}",
            dump()
        );
        // EVERY row, not just the largest: three tools at 1,399 B each used to pass unnoticed
        // while the ratchet only policed the single worst offender.
        for (name, whole) in &rows {
            assert!(
                *whole <= PER_TOOL_CEILING,
                "tool `{name}` is {whole} B (per-tool ceiling {PER_TOOL_CEILING}) — its \
                 description likely explains HOW it works instead of WHEN to pick it.\n{}",
                dump()
            );
        }
    }

    #[test]
    fn top_level_delegation_and_lsp_surface_stays_within_budget() {
        // The previous ratchet stopped at `default_registry_in`, but the actual interactive registry
        // appends delegation/persona and (by default) LSP schemas afterwards. Measure that real
        // maximal shape too; otherwise adding a 2 KB task schema can pass the 31 KB core test forever.
        const TOTAL_CEILING: usize = 45_000;
        const PER_TOOL_CEILING: usize = 2_400;
        let root = temp_root("schema-top-level");
        // Include configured browser support even on hosts without a live browser profile.
        let mut registry = default_registry_for_browser(&root, true);
        registry.register(Box::new(PersonaCreate));
        registry.register(Box::new(crate::agent::task_tool::TaskTool::new(
            reqwest::Client::new(),
            "http://x".into(),
            "k".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            root.clone(),
            0,
            200_000,
        )));
        registry.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            reqwest::Client::new(),
            "http://x".into(),
            "k".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            0,
            root.clone(),
            200_000,
        )));
        registry.register(Box::new(crate::agent::goal::GoalComplete));
        register_subagent_lsp_read(&mut registry, &root);
        register_subagent_lsp_write(&mut registry, &root);

        let mut rows: Vec<(String, usize)> = registry
            .defs()
            .iter()
            .map(|d| {
                (
                    d.function.name.clone(),
                    serde_json::to_string(d).unwrap().len(),
                )
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1));
        let total: usize = rows.iter().map(|(_, n)| *n).sum();
        let dump = || {
            rows.iter()
                .map(|(name, bytes)| format!("  {name:<26} {bytes:>7} B"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            total <= TOTAL_CEILING,
            "top-level tool schema {total} B exceeds {TOTAL_CEILING} B:\n{}",
            dump()
        );
        // Every row (not just the largest) — keep browser rows at the tighter core-tool ceiling.
        for (name, bytes) in &rows {
            let ceiling = if name.starts_with("browser_") {
                1_400
            } else {
                PER_TOOL_CEILING
            };
            assert!(
                *bytes <= ceiling,
                "top-level tool `{name}` is {bytes} B (ceiling {ceiling}):\n{}",
                dump()
            );
        }
    }

    #[test]
    fn registered_conditional_tools_are_classified_for_toolset_filtering() {
        for name in [
            "codebase_search",
            "skill_forget",
            "team_status",
            "bot_admin",
            "goal_complete",
        ] {
            assert!(
                crate::agent::toolsets::classify_tool(name).is_some(),
                "registered tool `{name}` must not become an unclassified escape hatch"
            );
        }
    }

    #[test]
    fn every_registered_tool_survives_repair_of_its_own_valid_shape() {
        // The repair layer reads each tool's own schema, so every tool is covered by it — and the
        // only way that claim stays true as tools are added is to check the WHOLE registry here.
        // For every tool, build a VALID argument object from the schema's required keys (each string
        // required key gets "x", each other-type key a fitting scalar) and assert `repair_args`
        // returns None — i.e. the valid shape is untouched and nothing gets rewritten into an
        // invalid one. A tool that can't even express a valid argument object (or whose repair
        // renames something) fails loudly here, not in a user session.
        let root = temp_root("repair-surface");
        let reg = default_registry_in(&root);
        for tool in reg.tools() {
            let schema = tool.parameters();
            let Some(req) = schema.get("required").and_then(Value::as_array) else {
                continue; // no required keys ⇒ repair never fires
            };
            let mut args = serde_json::Map::new();
            for key in req.iter().filter_map(Value::as_str) {
                let ty = schema
                    .get("properties")
                    .and_then(|p| p.get(key))
                    .and_then(|s| s.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("string");
                let val = match ty {
                    "integer" => Value::from(1),
                    "number" => Value::from(1.0),
                    "boolean" => Value::from(true),
                    "array" => Value::Array(vec![Value::from("x")]),
                    _ => Value::from("x"),
                };
                args.insert(key.to_string(), val);
            }
            let args = Value::Object(args);
            let repaired = crate::agent::tools::repair_args(&schema, &args);
            assert!(
                repaired.is_none(),
                "repair must be a no-op for {}'s own valid shape — it rewrote {:?}",
                tool.name(),
                repaired
            );
            // And the schema must not be lying about what it requires (a schema naming a key the
            // tool can't read would leave the model unable to call it correctly).
            let missing = crate::agent::tools::missing_required_strings(&schema, &args);
            assert!(
                missing.is_empty(),
                "{} schema declares required {} but the constructed valid args miss {}",
                tool.name(),
                req.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                missing.join(", ")
            );
        }
    }

    #[test]
    fn format_memory_hits_respects_the_token_budget() {
        use crate::memory::store::{MemoryEntry, MemoryType};
        let hit = |id: &str, scope: Option<&str>| crate::memory::Hit {
            entry: MemoryEntry {
                id: id.into(),
                name: id.into(),
                mtype: MemoryType::Reference,
                body: "word ".repeat(40),
                scope: scope.map(str::to_string),
                ..Default::default()
            },
            score: 1.0,
        };
        let hits: Vec<_> = (0..20).map(|i| hit(&format!("fact-{i}"), None)).collect();
        // Each line ≈ 55 tokens; a 120-token budget fits ~2 lines then cuts with a counted tail.
        let out = format_memory_hits(&hits, 120);
        assert!(out.contains("fact-0"), "{out}");
        assert!(out.contains("more hit(s) over the token budget"), "{out}");
        assert!(
            crate::memory::render::est_tokens(&out) <= 160,
            "output stays near the budget: {out}"
        );
        // zone tag renders for project-scoped hits
        let tagged = format_memory_hits(&[hit("zoned", Some("proj-00000001"))], 1200);
        assert!(tagged.contains("[p:proj-00000001]"), "{tagged}");
        // and the first hit always renders even when it alone exceeds the budget
        let one = format_memory_hits(&[hit("big", None)], 1);
        assert!(one.contains("big"));
    }

    #[test]
    fn glob_to_regex_matches_expected() {
        let re = regex::Regex::new(&glob_to_regex("src/**/*.rs")).unwrap();
        assert!(re.is_match("src/a.rs"));
        assert!(re.is_match("src/x/y/z.rs"));
        assert!(!re.is_match("src/a.ts"));
        assert!(!re.is_match("other/a.rs"));
        let re2 = regex::Regex::new(&glob_to_regex("*.md")).unwrap();
        assert!(re2.is_match("README.md"));
        assert!(!re2.is_match("docs/README.md"), "* must not cross /");
    }

    #[test]
    fn confine_resolves_relative_and_reaches_outside_root() {
        // Confinement was removed by user request: `confine` is now a pure path resolver, no escape
        // check. A relative in-tree path still resolves; a `..` path that leaves `root` now RESOLVES
        // (used to be rejected) so the agent can work on sibling projects the user points it at.
        let root = temp_root("confine");
        std::fs::write(root.join("ok.txt"), "hi").unwrap();
        assert!(
            confine(&root, "ok.txt", true).is_ok(),
            "in-tree path resolves"
        );
        // A create-target (must_exist=false) that escapes the root no longer errors — it resolves to
        // a path OUTSIDE root. Its parent (root's parent) exists, so canonicalize+rejoin succeeds.
        let escaped =
            confine(&root, "../ng-confine-escape.txt", false).expect("escape now resolves");
        assert!(
            !escaped.starts_with(&root),
            "resolved target is outside the root: {}",
            escaped.display()
        );
    }

    #[test]
    fn diff_preview_is_a_trimmed_unified_diff() {
        // A localized change in a longer block: the shared prefix/suffix collapse to ±2 context
        // lines under a `@@` header anchored at the file line, and the change renders as column-0
        // `-`/`+` lines (what the TUI's diff panes parse).
        let before = "1\n2\n3\n4\n5\nOLD\n7\n8\n9\n10";
        let after = "1\n2\n3\n4\n5\nNEW\n7\n8\n9\n10";
        let d = diff_preview(before, after, 1);
        assert_eq!(
            d, "@@ -4,5 +4,5 @@\n 4\n 5\n-OLD\n+NEW\n 7\n 8",
            "window = header + ±2 context around the one changed line"
        );
        // Unknown base (0) omits the header — the TUI then shows the rows without gutter numbers.
        let d0 = diff_preview(before, after, 0);
        assert!(
            !d0.contains("@@"),
            "no header when the base line is unknown: {d0:?}"
        );
    }

    #[test]
    fn file_read_reads_and_ranges() {
        let root = temp_root("read");
        std::fs::write(root.join("f.txt"), "l1\nl2\nl3\nl4").unwrap();
        let t = FileRead::new(root.clone());
        let all = t.execute(&serde_json::json!({"path":"f.txt"})).unwrap();
        assert_eq!(all, "l1\nl2\nl3\nl4");
        let mid = t
            .execute(&serde_json::json!({"path":"f.txt","start":2,"end":3}))
            .unwrap();
        assert_eq!(mid, "l2\nl3");
    }

    #[test]
    fn file_read_errors_on_missing_path() {
        // NOTE: confinement was removed — a `../` path is NOT rejected for escaping the root. This
        // errors only because the target doesn't exist (canonicalize fails). A `../` path that DOES
        // exist is read successfully by design (see `confine`'s doc). This test guards the
        // missing-file error, not a boundary that no longer exists.
        let root = temp_root("read-missing");
        let t = FileRead::new(root);
        assert!(t
            .execute(&serde_json::json!({"path":"../../nonexistent-xyzzy-secret"}))
            .is_err());
    }

    #[test]
    fn file_glob_lists_matches() {
        let root = temp_root("glob");
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("src/sub/b.rs"), "").unwrap();
        std::fs::write(root.join("src/c.ts"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"src/**/*.rs"}))
            .unwrap();
        assert!(out.contains("src/a.rs"));
        assert!(out.contains("src/sub/b.rs"));
        assert!(!out.contains("c.ts"));
    }

    #[test]
    fn file_glob_literal_path_short_circuits_the_walk() {
        // A structured pattern that names ONE literal path (separator present, no `*`/`?`) is
        // answered by a single stat — the same output line as the exact-walk would produce, without
        // walking anything.
        let root = temp_root("glob-literal");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        let t = FileGlob::new(root);

        let file = t
            .execute(&serde_json::json!({"pattern":"src/a.rs"}))
            .unwrap();
        assert_eq!(file, "src/a.rs", "exact file, one clean line: {file}");

        let dir = t.execute(&serde_json::json!({"pattern":"./src"})).unwrap();
        assert_eq!(dir, "src/", "directories keep the trailing slash: {dir}");

        // A `..`-anchored literal reaches a real sibling file above the root exactly like the walk
        // would — deterministic stat hit, no deep walk of the whole temp parent.
        let base = temp_root("glob-literal-parent");
        let child = base.join("proj");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(base.join("sibling.rs"), "").unwrap();
        let t = FileGlob::new(child.canonicalize().unwrap());
        let out = t
            .execute(&serde_json::json!({"pattern":"../sibling.rs"}))
            .unwrap();
        assert!(
            out.ends_with("sibling.rs") && !out.contains("…["),
            "single absolute answer, no walk trailers: {out}"
        );
    }

    #[test]
    fn file_glob_literal_miss_falls_through_to_the_walk() {
        // A literal structured pattern that does NOT exist must NOT return empty-handed from the
        // short-circuit: it falls through so the walk can answer smart-case (`src/A.RS` for
        // `src/a.rs`) — and, failing that, the fuzzy fallback suggests near names exactly as before.
        let root = temp_root("glob-literal-miss");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/A.RS"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"src/a.rs"}))
            .unwrap();
        if cfg!(any(windows, target_os = "macos")) {
            // Case-insensitive filesystems: the stat itself hits `A.RS`, so the short-circuit
            // answers with the queried casing — the fall-through only runs on Linux.
            assert_eq!(
                out, "src/a.rs",
                "stat resolves case-insensitively on this platform: {out}"
            );
        } else {
            assert!(
                out.contains("A.RS"),
                "smart-case hit via the walk, not a stat miss: {out}"
            );
        }

        let out = t
            .execute(&serde_json::json!({"pattern":"src/totally-missing.txt"}))
            .unwrap();
        assert!(
            !out.contains("A.RS") || out.contains("closest matches"),
            "true miss reaches the usual not-found messaging: {out}"
        );
    }

    #[test]
    fn file_glob_bare_name_never_short_circuits() {
        // A BARE name (`Cargo.toml`) is deliberately a broad ranked search, NOT a stat of
        // root.join(name) — a file sitting ABOVE the cwd must still be found. Guard that the
        // short-circuit only fires for structured patterns.
        let base = temp_root("glob-bare");
        let ws = base.join("sub").join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        // The needle lives at `base/Cargo.toml` — outside the walk anchor's direct stat reach
        // (root is the workspace), reachable only by the broad seed walk over ancestors.
        std::fs::write(base.join("Cargo.toml"), "").unwrap();
        let t = FileGlob::new(ws.canonicalize().unwrap());
        let out = t
            .execute(&serde_json::json!({"pattern":"Cargo.toml"}))
            .unwrap();
        assert!(
            out.contains("Cargo.toml"),
            "bare name still searched broadly above the cwd: {out}"
        );
    }

    #[test]
    fn file_glob_includes_hidden_and_build_dirs() {
        // Hidden files and heavy build dirs (target, node_modules, .git) are NO LONGER skipped —
        // the user asked file_glob to see everything.
        let root = temp_root("glob-hidden");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("target/debug/app.rs"), "").unwrap();
        std::fs::write(root.join(".hidden/secret.rs"), "").unwrap();
        std::fs::write(root.join(".env.rs"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"**/*.rs"}))
            .unwrap();
        assert!(
            out.contains("target/debug/app.rs"),
            "build dir walked: {out}"
        );
        assert!(
            out.contains(".hidden/secret.rs"),
            "hidden dir walked: {out}"
        );
        assert!(out.contains(".env.rs"), "hidden file matched: {out}");
    }

    #[test]
    fn file_glob_ignore_true_honours_gitignore_and_prunes_build_dirs() {
        // The default stays "see everything" (the maintainer asked for it); `ignore:true` opts into
        // search_files' semantics for the structured walk.
        let root = temp_root("glob-ignore");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("target/debug/app.rs"), "").unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("gen/out.rs"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        let t = FileGlob::new(root);
        let all = t
            .execute(&serde_json::json!({"pattern": "**/*.rs"}))
            .unwrap();
        assert!(
            all.contains("target/debug/app.rs") && all.contains("gen/out.rs"),
            "default sees everything: {all}"
        );
        let lean = t
            .execute(&serde_json::json!({"pattern": "**/*.rs", "ignore": true}))
            .unwrap();
        assert!(lean.contains("src/a.rs"), "{lean}");
        assert!(
            !lean.contains("target/debug/app.rs"),
            "build dir pruned: {lean}"
        );
        assert!(!lean.contains("gen/out.rs"), ".gitignore honoured: {lean}");
    }

    #[test]
    fn file_glob_reaches_outside_the_root() {
        // A `../sibling/...` pattern must escape the working dir (confinement removed, #67). Anchor
        // the tool at a subdir and glob back up into a sibling.
        let base = temp_root("glob-escape");
        std::fs::create_dir_all(base.join("proj_a")).unwrap();
        std::fs::create_dir_all(base.join("proj_b")).unwrap();
        std::fs::write(base.join("proj_b/level.js"), "").unwrap();
        let t = FileGlob::new(base.join("proj_a").canonicalize().unwrap());
        let out = t
            .execute(&serde_json::json!({"pattern":"../proj_b/**/*.js"}))
            .unwrap();
        assert!(
            out.contains("level.js"),
            "should reach the sibling project: {out}"
        );
    }

    #[test]
    fn file_glob_fuzzy_fallback_suggests_near_names() {
        // No glob match → fall back to a fuzzy match on the file NAME (#68). A typo'd needle should
        // still surface the real file among the closest suggestions.
        let root = temp_root("glob-fuzzy");
        std::fs::write(root.join("snake_game.js"), "").unwrap();
        std::fs::write(root.join("readme.md"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"snakegame.js"}))
            .unwrap();
        assert!(
            out.contains("closest matches"),
            "fuzzy header present: {out}"
        );
        assert!(out.contains("snake_game.js"), "near-name surfaced: {out}");
    }

    #[test]
    fn file_glob_matches_directories_not_just_files() {
        // "Find the X folder" must return the DIRECTORY itself, not only files under it.
        let root = temp_root("glob-dirs");
        std::fs::create_dir_all(root.join("src/mini_project")).unwrap();
        std::fs::write(root.join("src/mini_project/main.rs"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"**/mini_project"}))
            .unwrap();
        assert!(
            out.contains("src/mini_project"),
            "the folder itself is listed: {out}"
        );
    }

    #[test]
    fn file_glob_fuzzy_finds_parent_folder_by_near_name() {
        // The workspace is often a subdir (e.g. .../mini_project/aizen). Asking for the parent
        // folder by a separator-insensitive near name ("miniproject") must surface "mini_project"
        // even though it lives ABOVE the anchor — a downward-only walk never reaches it.
        let base = temp_root("glob-parent");
        let ws = base.join("mini_project").join("aizen");
        std::fs::create_dir_all(&ws).unwrap();
        let t = FileGlob::new(ws.canonicalize().unwrap());
        let out = t
            .execute(&serde_json::json!({"pattern":"miniproject"}))
            .unwrap();
        assert!(
            out.contains("mini_project"),
            "parent folder surfaced by fuzzy: {out}"
        );
        assert!(out.contains("(folder)"), "it's tagged as a folder: {out}");
    }

    #[test]
    fn file_glob_smart_case_insensitive_by_default() {
        // A lowercase pattern matches case-insensitively (a model types `readme.md`, means
        // `README.md`); an uppercase letter in the pattern makes the match case-sensitive.
        let root = temp_root("glob-case");
        std::fs::write(root.join("README.md"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"readme.md"}))
            .unwrap();
        assert!(
            out.contains("README.md"),
            "lowercase pattern is case-insensitive: {out}"
        );
    }

    #[test]
    fn file_glob_ranks_exact_name_first() {
        // With several matches, the EXACT-name file must sort to line 1 (best-first ranking, P1.3),
        // so a model that reads only the first line still gets the right answer.
        let root = temp_root("glob-rank");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/config.toml.bak"), "").unwrap();
        std::fs::write(root.join("b/config.toml"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"**/config.toml"}))
            .unwrap();
        let first = out.lines().next().unwrap_or("");
        assert!(
            first.ends_with("b/config.toml"),
            "exact name ranked first: {out}"
        );
    }

    #[test]
    fn file_glob_no_match_message_is_not_negative_prose() {
        // A true no-match must NOT open with parenthetical negative prose (the old "(no exact
        // match…)" that made models distrust the tool and shell out). It names where it looked.
        let root = temp_root("glob-nomatch");
        std::fs::write(root.join("alpha.rs"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"zzqwx_nonexistent_qq"}))
            .unwrap();
        assert!(
            !out.starts_with('('),
            "no leading negative parenthetical: {out}"
        );
        assert!(out.contains("searched"), "says where it looked: {out}");
    }

    #[test]
    fn file_edit_replaces_uniquely() {
        let root = temp_root("edit");
        std::fs::write(root.join("f.txt"), "hello world").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path":"f.txt","old_string":"world","new_string":"rust"}))
            .unwrap();
        assert!(r.contains("edited"));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "hello rust"
        );
    }

    #[test]
    fn file_read_number_prefixes_lines() {
        let root = temp_root("read-num");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma").unwrap();
        let t = FileRead::new(root);
        let out = t
            .execute(&serde_json::json!({"path":"f.txt","number":true}))
            .unwrap();
        assert_eq!(out, "1|alpha\n2|beta\n3|gamma");
        // default (no number) stays byte-exact so old_string round-trips into file_edit cleanly
        let plain = t.execute(&serde_json::json!({"path":"f.txt"})).unwrap();
        assert_eq!(plain, "alpha\nbeta\ngamma");
    }

    #[test]
    fn file_read_range_over_budget_is_contiguous_and_says_how_to_continue() {
        let root = temp_root("read-range-budget");
        let body: String = (1..=50).map(|i| format!("row {i}\n")).collect();
        std::fs::write(root.join("f.txt"), &body).unwrap();
        let t = FileRead::new(root);
        // Line cap of 10 on a 50-line range: the first ten rows, in order, then the marker.
        let out = t
            .read_one("f.txt", Some(1), Some(50), false, 10, 0, false)
            .unwrap();
        let (shown, marker) = out.split_once("\n…[file_read:").expect("marker present");
        assert_eq!(
            shown,
            (1..=10)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "a contiguous head of the range, nothing spliced"
        );
        assert!(marker.contains("Showing 1-10"), "{marker}");
        assert!(
            marker.contains("continue with start:11, end:50"),
            "{marker}"
        );
        // `number:true` on the whole file goes through the same budget (it used to bypass it).
        let numbered = t.read_one("f.txt", None, None, true, 5, 0, false).unwrap();
        assert!(
            numbered.starts_with("1|row 1\n2|row 2\n3|row 3\n4|row 4\n5|row 5\n…[file_read:"),
            "{numbered}"
        );
        assert!(
            numbered.contains("continue with start:6, end:50"),
            "{numbered}"
        );
        // Under budget: byte-exact, no marker (the old_string round-trip invariant).
        let small = t
            .read_one("f.txt", Some(3), Some(4), false, 10, 0, false)
            .unwrap();
        assert_eq!(small, "row 3\nrow 4");
        // A byte cap on a range is honoured too.
        let bytes = t
            .read_one("f.txt", Some(1), Some(50), false, 0, 20, false)
            .unwrap();
        assert!(
            bytes.starts_with("row 1\nrow 2\nrow 3\n…[file_read:"),
            "{bytes}"
        );
    }

    #[test]
    fn budget_view_under_cap_is_verbatim() {
        // Small file with CRLF + trailing newline → byte-identical (the round-trip invariant).
        let c = "a\r\nb\r\nc\r\n";
        assert_eq!(budget_view(c, "f", 2000, 200_000), c);
    }

    #[test]
    fn budget_view_over_lines_marks_headtail_byte_exact() {
        let c = "L1\r\nL2\r\nL3\r\nL4\r\nL5\r\n";
        let out = budget_view(c, "f.txt", 4, 0); // line cap 4, byte cap disabled
        assert!(
            out.contains("over the 4-line read budget"),
            "loud marker: {out}"
        );
        assert!(
            out.contains("L1\r\nL2\r\n"),
            "head slice is byte-exact incl CRLF"
        );
        assert!(out.contains("L5"), "tail slice present");
        assert!(!out.contains("L3"), "the omitted middle is not shown");
        assert!(
            out.contains("lines omitted: 3-4"),
            "names the omitted range: {out}"
        );
    }

    #[test]
    fn budget_view_over_bytes_one_long_line() {
        let c = "x".repeat(5000);
        let out = budget_view(&c, "min.js", 0, 2048); // byte cap only; one giant line
        assert!(out.starts_with("[file_read:"), "marker first");
        assert!(out.contains("KB read budget"));
        assert!(out.contains("bytes omitted"));
        assert!(
            out.len() < c.len(),
            "result is bounded below the original size"
        );
    }

    #[test]
    fn budget_view_disabled_when_zero() {
        let c = "line\n".repeat(5000);
        assert_eq!(budget_view(&c, "f", 0, 0), c, "both caps 0 → verbatim");
    }

    #[test]
    fn file_read_over_budget_via_execute() {
        // The real consts (2000 lines): a whole-file read of a 2500-line file is trimmed + marked.
        let root = temp_root("read-budget");
        let big: String = (1..=2500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let t = FileRead::new(root);
        let out = t.execute(&serde_json::json!({"path":"big.txt"})).unwrap();
        assert!(
            out.contains("over the 2000-line read budget"),
            "marker present"
        );
        assert!(out.contains("line1\n"), "head present");
        assert!(out.contains("line2500"), "tail present");
        assert!(
            !out.contains("line1300\n"),
            "the omitted middle is not shown"
        );
    }

    #[test]
    fn file_read_explicit_range_skips_budget() {
        // An explicit start/end on the SAME large file returns the exact range — never re-bounded.
        let root = temp_root("read-budget-range");
        let big: String = (1..=2500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let t = FileRead::new(root);
        let out = t
            .execute(&serde_json::json!({"path":"big.txt","start":5,"end":7}))
            .unwrap();
        assert_eq!(out, "line5\nline6\nline7");
        assert!(
            !out.contains("read budget"),
            "range reads are never budgeted"
        );
    }

    #[test]
    fn ladder_ws_normalized_matches_interior_spacing() {
        // Interior spacing differs (`foo( a,  b )` vs `foo(a, b)`) — R2 trim can't fix that, R3 can.
        let content = "start\n    foo( a,  b );\nend\n";
        let a = apply_one_edit(content, "foo(a, b);", "bar(a, b);", false, "t").unwrap();
        assert_eq!(a.rung, "ws-norm");
        assert!(a.content.contains("bar(a, b);"), "{}", a.content);
        assert!(a.summary().contains("whitespace-normalized"));
    }

    #[test]
    fn ladder_anchor_trim_only_when_context_shared() {
        // First line of old == first line of new (pure context) but that line is WRONG in old
        // ("fn mian" typo'd context) — dropping it from BOTH sides rescues the edit.
        let content = "fn main() {\n    body();\n}\n";
        let old = "fn mian() {\n    body();"; // first line mis-copied
        let new = "fn mian() {\n    new_body();"; // …but identical in new → shared context
        let a = apply_one_edit(content, old, new, false, "t").unwrap();
        assert_eq!(a.rung, "anchor-trim");
        assert!(a.content.contains("new_body();"), "{}", a.content);
        assert!(
            a.content.contains("fn main() {"),
            "the real context line is untouched"
        );
        // NOT shared (the first line actually differs between old and new) → no R4, hard failure.
        let old2 = "fn mian() {\n    body();";
        let new2 = "fn other() {\n    new_body();";
        assert!(
            apply_one_edit(content, old2, new2, false, "t").is_err(),
            "no anchor-trim without shared context"
        );
    }

    #[test]
    fn ladder_ambiguous_at_any_rung_refuses() {
        // Two ws-normalized matches → hard error, must NOT fall through to a looser rung.
        let content = "foo( a );\nfoo(  a );\n";
        let err = apply_one_edit(content, "foo(a );", "bar();", false, "t")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("2") && err.contains("t"),
            "ambiguity is a hard refusal: {err}"
        );
    }

    #[test]
    fn ladder_unescape_applies_pair() {
        // The model shipped literal \n escapes in BOTH strings — R5 unescapes the pair together.
        let content = "alpha\nbeta\ngamma\n";
        let a = apply_one_edit(content, "alpha\\nbeta", "alpha\\nBETA", false, "t").unwrap();
        assert_eq!(a.rung, "unescape");
        assert!(a.content.contains("alpha\nBETA\ngamma"), "{}", a.content);
    }

    #[test]
    fn ladder_blank_line_insensitive_match() {
        // The file has a blank line INSIDE the block that the model's old_string omitted — every
        // fixed-`k` rung misses (line counts differ), but R5.5 matches on the non-blank lines and
        // splices the whole spanned region (blank included) so `new` redefines it.
        let content = "fn f() {\n    let a = 1;\n\n    let b = 2;\n}\n";
        let old = "let a = 1;\n    let b = 2;"; // no blank between — 2 lines vs the file's 3
        let new = "let a = 10;\n    let b = 20;";
        let a = apply_one_edit(content, old, new, false, "t").unwrap();
        assert_eq!(a.rung, "blank-norm");
        assert!(a.content.contains("let a = 10;"), "{}", a.content);
        assert!(a.content.contains("let b = 20;"), "{}", a.content);
        assert!(
            !a.content.contains("let a = 1;"),
            "old first line replaced: {}",
            a.content
        );
        assert!(a.summary().contains("blank-line-insensitive"));
    }

    #[test]
    fn ladder_blank_norm_ambiguous_refuses() {
        // Both occurrences have a blank INSIDE them, so old (no blank) misses every fixed-`k` rung
        // and only R5.5 matches — twice → hard error, never a silent wrong splice.
        let content = "a = 1;\n\nb = 2;\nMID\na = 1;\n\nb = 2;\n";
        let err = apply_one_edit(content, "a = 1;\nb = 2;", "z = 9;", false, "t")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("blank lines") && err.contains('2'),
            "ambiguity is a hard refusal: {err}"
        );
    }

    #[test]
    fn ladder_nearest_miss_reports_line_numbers() {
        let content = (1..=30)
            .map(|i| format!("line number {i} content"))
            .collect::<Vec<_>>()
            .join("\n");
        let err = apply_one_edit(&content, "line number 17 contnet", "x", false, "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Nearest match"), "{err}");
        assert!(
            err.contains("17| "),
            "the real line is quoted with its number: {err}"
        );
        assert!(err.contains("Copy old_string EXACTLY"), "{err}");
    }

    #[test]
    fn ladder_no_similar_region_keeps_plain_error() {
        let content = "completely different text\n";
        let err = apply_one_edit(content, "zzz qqq www", "x", false, "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        assert!(
            !err.contains("Nearest match"),
            "below the similarity floor → no random quote: {err}"
        );
    }

    #[test]
    fn json_unescape_only_reports_real_changes() {
        assert_eq!(json_unescape("a\\nb").as_deref(), Some("a\nb"));
        assert_eq!(json_unescape("a\\\\b").as_deref(), Some("a\\b"));
        assert_eq!(json_unescape("plain"), None, "no backslash → no rung");
        assert_eq!(
            json_unescape("C:\\path\\dir"),
            None,
            "unknown escapes alone don't count as a change"
        );
    }

    #[test]
    fn file_edit_indentation_tolerant_fallback() {
        let root = temp_root("edit-indent");
        // File uses 4-space indentation; the model's old_string uses 2 spaces (a real mismatch).
        std::fs::write(
            root.join("f.rs"),
            "fn main() {\n    let x = 1;\n    foo();\n}\n",
        )
        .unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.rs",
                "old_string": "  let x = 1;\n  foo();",      // wrong indentation, right content
                "new_string": "    let x = 2;\n    bar();"
            }))
            .unwrap();
        assert!(r.contains("indentation-tolerant"), "got: {r}");
        // the unified diff preview shows the removed/added lines (column-0 -/+ markers)
        assert!(
            r.lines().any(|l| l.starts_with('-')),
            "diff shows a removed line: {r}"
        );
        assert!(
            r.lines().any(|l| l.starts_with('+')),
            "diff shows an added line: {r}"
        );
        let after = std::fs::read_to_string(root.join("f.rs")).unwrap();
        assert_eq!(after, "fn main() {\n    let x = 2;\n    bar();\n}\n");
    }

    #[test]
    fn file_edit_preserves_crlf_via_tolerant_rung() {
        let root = temp_root("edit-crlf");
        // A Windows (CRLF) file; the model's old_string is bare-LF with the wrong indent — a real
        // mismatch the indent-tolerant rung rescues. The replaced line MUST stay CRLF, not silently
        // degrade to a lone-LF line among CRLF neighbours (Windows mixed-ending corruption).
        std::fs::write(root.join("f.rs"), "a\r\n    b\r\nc\r\n").unwrap();
        let t = FileEdit::new(root.clone());
        t.execute(&serde_json::json!({"path":"f.rs","old_string":"  b","new_string":"X"}))
            .unwrap();
        let after = std::fs::read_to_string(root.join("f.rs")).unwrap();
        // The invariant is line endings, not indentation (the indent rung reapplies leading space):
        // the edited line is written with CRLF, and no lone-LF line survives anywhere.
        assert!(
            after.contains("X\r\n"),
            "edited line written with CRLF: {after:?}"
        );
        assert!(
            !after.replace("\r\n", "").contains('\n'),
            "no lone-LF line survived the edit: {after:?}"
        );
    }

    #[test]
    fn preserve_eol_restores_boundary_and_internal_crlf() {
        // CRLF block, bare-LF replacement, `\n` sits at the boundary → restore the trailing `\r`.
        assert_eq!(preserve_eol("X", "b\r", "\nc"), "X\r");
        // Multi-line bare-LF new against a CRLF region → every internal break becomes CRLF too.
        assert_eq!(preserve_eol("X\nY", "b\r", "\nc"), "X\r\nY\r");
        // LF file → untouched (byte-identical).
        assert_eq!(preserve_eol("X", "b", "\nc"), "X");
        assert_eq!(preserve_eol("X\nY", "b", "\nc"), "X\nY");
    }

    #[test]
    fn file_edit_replace_all_reaches_the_tolerant_rungs() {
        // Two identically-drifted blocks (4-space file, 2-space old_string): replace_all used to
        // live on the exact rung only, so the indent rung saw 2 blocks and refused.
        let root = temp_root("edit-replace-all-indent");
        let drifted = "fn a() {\n    foo();\n    baz();\n}\nfn b() {\n    foo();\n    baz();\n}\n";
        std::fs::write(root.join("f.rs"), drifted).unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.rs",
                "old_string": "  foo();\n  baz();",
                "new_string": "    bar();\n    baz();",
                "replace_all": true
            }))
            .unwrap();
        assert!(
            r.contains("2 replacements, indentation-tolerant match, replace_all"),
            "got: {r}"
        );
        let after = std::fs::read_to_string(root.join("f.rs")).unwrap();
        assert_eq!(
            after,
            "fn a() {\n    bar();\n    baz();\n}\nfn b() {\n    bar();\n    baz();\n}\n"
        );
        // Without replace_all the same call still refuses: the 1-match invariant holds.
        std::fs::write(root.join("g.rs"), drifted).unwrap();
        let e = t.execute(&serde_json::json!({
            "path": "g.rs",
            "old_string": "  foo();\n  baz();",
            "new_string": "    bar();\n    baz();"
        }));
        assert!(e.is_err(), "ambiguous without replace_all");
    }

    #[test]
    fn previews_show_the_patch_the_command_and_the_destination_before_anything_runs() {
        use crate::agent::tools::Tool as _;
        let root =
            std::env::temp_dir().join(format!("aizen-preview-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let edit = FileEdit::new(root.clone());
        let p = edit
            .preview(&serde_json::json!({"path": "a.txt", "old_string": "two", "new_string": "2"}))
            .expect("an edit previews");
        assert_eq!(p.title, "a.txt");
        let diff = p.diff.expect("the patch");
        assert!(diff.contains("-two") && diff.contains("+2"), "{diff}");
        assert!(p.lines[0].starts_with("would edit a.txt"), "{:?}", p.lines);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n",
            "nothing written"
        );
        assert!(
            edit.preview(
                &serde_json::json!({"path": "a.txt", "old_string": "zzz", "new_string": "2"})
            )
            .is_none(),
            "a failing edit has no preview"
        );
        let write = FileWrite::new(root.clone());
        let p = write
            .preview(&serde_json::json!({"path": "a.txt", "content": "one\n2\nthree\n"}))
            .unwrap();
        assert!(
            p.diff.is_some() && p.lines[0].starts_with("overwrite a.txt"),
            "{:?}",
            p.lines
        );
        let p = write
            .preview(&serde_json::json!({"path": "new.txt", "content": "x\ny\n"}))
            .unwrap();
        assert!(
            p.diff.is_none() && p.lines[0].starts_with("create new.txt"),
            "{:?}",
            p.lines
        );
        let shell = ShellRun::new(root.clone());
        let p = shell
            .preview(&serde_json::json!({"command": "python deploy.py --prod --force-delete", "network": true}))
            .unwrap();
        assert!(p.lines[0].starts_with("cwd: "), "{:?}", p.lines);
        assert_eq!(p.lines[1], "$ python deploy.py --prod --force-delete");
        assert_eq!(p.lines[2], "network: requested");
        let mv = FileMove::new(root.clone());
        let p = mv
            .preview(&serde_json::json!({"from": "a.txt", "to": "b.txt", "overwrite": true}))
            .unwrap();
        assert_eq!(p.lines[0], "move a.txt → b.txt (overwrite)");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_edit_dry_run_previews_without_writing() {
        let root = temp_root("edit-dry-run");
        let original = "a\nb\nc\n";
        std::fs::write(root.join("f.txt"), original).unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.txt", "old_string": "b", "new_string": "B", "dry_run": true
            }))
            .unwrap();
        assert!(r.starts_with("dry-run: would edit f.txt"), "{r}");
        assert!(r.contains("-b") && r.contains("+B"), "diff shown: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            original,
            "nothing written"
        );
        // The batch form honours dry_run too; the old create-new form is now refused.
        let r = t
            .execute(&serde_json::json!({
                "path": "f.txt", "dry_run": true,
                "edits": [
                    {"old_string": "a", "new_string": "A"},
                    {"old_string": "c", "new_string": "C"}
                ]
            }))
            .unwrap();
        assert!(r.starts_with("dry-run: would edit f.txt (2 edits)"), "{r}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            original
        );
        let r = t
            .execute(&serde_json::json!({
                "path": "new.txt", "old_string": "", "new_string": "hello", "dry_run": true
            }))
            .unwrap_err();
        assert!(r.to_string().contains("file_write"), "{r}");
        assert!(!root.join("new.txt").exists());
    }

    #[test]
    fn file_edit_ambiguous_tolerant_match_refuses() {
        let root = temp_root("edit-indent-dup");
        std::fs::write(root.join("f.txt"), "  a\nb\n    a\nb\n").unwrap();
        let t = FileEdit::new(root);
        // "a\nb" matches two blocks once indentation is ignored → refuse, don't corrupt.
        let r =
            t.execute(&serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X"}));
        assert!(r.is_err(), "ambiguous tolerant match must refuse");
    }

    #[test]
    fn file_edit_rejects_nonunique_without_replace_all() {
        let root = temp_root("edit-dup");
        std::fs::write(root.join("f.txt"), "a a a").unwrap();
        let t = FileEdit::new(root);
        assert!(t
            .execute(&serde_json::json!({"path":"f.txt","old_string":"a","new_string":"b"}))
            .is_err());
    }

    #[test]
    fn file_edit_rejects_new_when_old_empty_or_omitted() {
        let root = temp_root("edit-new");
        let t = FileEdit::new(root.clone());
        let ctx = ExecutionContext::new(crate::core::convo::ConversationId::new("test:edit-new"));
        for args in [
            serde_json::json!({"path":"new.txt","old_string":"","new_string":"content"}),
            serde_json::json!({"path":"new.txt","new_string":"content"}),
        ] {
            assert!(t
                .prepare_call(&args, &ctx)
                .unwrap_err()
                .to_string()
                .contains("file_write"));
            assert!(t
                .execute(&args)
                .unwrap_err()
                .to_string()
                .contains("file_write"));
            assert!(!root.join("new.txt").exists());
        }
        FileWrite::new(root.clone())
            .execute(&serde_json::json!({"path":"new.txt","content":"content","purpose":"project"}))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "content"
        );
    }

    #[test]
    fn file_edit_is_destructive() {
        assert!(FileEdit::new(PathBuf::from(".")).is_destructive());
        assert!(ShellRun::new(PathBuf::from(".")).is_destructive());
        assert!(!FileRead::new(PathBuf::from(".")).is_destructive());
    }

    #[test]
    fn write_tools_point_the_checkpoint_gate_at_the_target_not_the_cwd() {
        // THE invariant behind the "run `git init`" misfire: the gate must look for a work tree
        // where the write LANDS. A session rooted in a home directory editing a project two levels
        // down asked about the wrong directory, found no repo, and told the user to `git init` —
        // which, followed literally, inits the home directory. Every path-naming tool must name its
        // own destination so the lookup happens in the project.
        let root = temp_root("target-dir");
        let proj = root.join("mini_project").join("web");
        std::fs::create_dir_all(proj.join("src")).unwrap();

        // A relative path resolves under root, and the DIRECTORY (not the file) is reported.
        let w = FileWrite::new(root.clone());
        assert_eq!(
            w.workspace_target(&serde_json::json!({"path":"mini_project/web/src/app.js"})),
            Some(proj.join("src")),
            "file_write must point at the file's parent directory"
        );
        // Same for a not-yet-existing file: the parent is what a repo lookup needs.
        assert_eq!(
            w.workspace_target(&serde_json::json!({"path":"mini_project/web/src/brand-new.js"})),
            Some(proj.join("src"))
        );
        // An absolute path passes through unchanged (never re-anchored under root).
        let outside = temp_root("target-dir-abs");
        let abs = outside.join("x.txt");
        assert_eq!(
            w.workspace_target(&serde_json::json!({"path": abs.to_string_lossy()})),
            Some(outside.clone())
        );
        // file_edit resolves the target dir the same whether single or batch (same `path` arg).
        assert_eq!(
            FileEdit::new(root.clone())
                .workspace_target(&serde_json::json!({"path":"mini_project/web/src/app.js"})),
            Some(proj.join("src"))
        );
        // file_move must snapshot the SOURCE — that is the side whose content disappears.
        assert_eq!(
            FileMove::new(root.clone()).workspace_target(
                &serde_json::json!({"from":"mini_project/web/src/app.js","to":"other/app.js"})
            ),
            Some(proj.join("src"))
        );
        // A malformed / absent path names no directory rather than guessing at one.
        assert_eq!(w.workspace_target(&serde_json::json!({})), None);
        // shell_run names no path, so it keeps the cwd-relative behavior by declining to answer.
        assert_eq!(
            ShellRun::new(root).workspace_target(&serde_json::json!({"command":"ls"})),
            None
        );
    }

    fn placement_fixture(root: &Path) -> ExecutionContext {
        static FIXTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let workspace = root.canonicalize().unwrap();
        let base = workspace.parent().unwrap();
        let placement = crate::core::file_placement::PlacementContext::with_paths(
            workspace.clone(),
            base.join(format!(
                "{}-home-{sequence}",
                workspace.file_name().unwrap().to_string_lossy()
            )),
            base.join(format!(
                "{}-scratch-{sequence}",
                workspace.file_name().unwrap().to_string_lossy()
            )),
            "native-test-run",
        )
        .unwrap();
        ExecutionContext::new(crate::core::convo::ConversationId::new("test:placement"))
            .with_placement(placement)
    }

    #[test]
    fn file_placement_new_targets_require_purpose_before_any_effects() {
        let root = temp_root("purpose-required");
        let ctx = placement_fixture(&root);
        let write = FileWrite::new(root.clone());
        for args in [
            serde_json::json!({"path":"new.txt","content":"x"}),
            serde_json::json!({"path":"nested/new.txt","content":"x"}),
            serde_json::json!({"path":"new.txt","content":"x","purpose":"unknown"}),
            serde_json::json!({"path":"new.txt","content":"x","purpose":null}),
            serde_json::json!({"path":"new.txt","content":"x","purpose":7}),
        ] {
            assert!(write.prepare_call(&args, &ctx).is_err(), "{args}");
            assert!(write.execute(&args).is_err(), "{args}");
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let placement = ctx.placement().unwrap();
        assert!(!placement.home_root().exists());
        assert!(!placement.scratch_root().exists());
        std::fs::write(root.join("source.txt"), "source").unwrap();
        let mv = FileMove::new(root.clone());
        let args = serde_json::json!({"from":"source.txt","to":"missing/nested/dest.txt","create_dirs":true});
        assert!(mv
            .prepare_call(&args, &ctx)
            .unwrap_err()
            .to_string()
            .contains("purpose"));
        assert!(mv.execute(&args).is_err());
        assert!(!root.join("missing").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("source.txt")).unwrap(),
            "source"
        );
    }

    #[test]
    fn file_placement_managed_writes_use_pinned_roots_and_direct_calls_validate() {
        let root = temp_root("purpose-managed");
        let ctx = placement_fixture(&root);
        let write = FileWrite::new(root.clone());
        for purpose in ["artifact", "scratch"] {
            let args =
                serde_json::json!({"path":"nested/output.txt","content":purpose,"purpose":purpose});
            assert!(write
                .execute(&args)
                .unwrap_err()
                .to_string()
                .contains("pinned"));
            let prepared = write.prepare_call(&args, &ctx).unwrap();
            let actual = PathBuf::from(prepared.args["path"].as_str().unwrap());
            assert!(!actual.exists());
            assert!(!root.join("nested").exists());
            assert!(
                !actual.parent().unwrap().exists(),
                "preparation must not create directories"
            );
            let cached = prepared.preview.as_ref().unwrap();
            assert!(cached.lines[0].contains("create"));
            let out = write.execute_prepared(&prepared).unwrap();
            assert!(out.text.contains(&actual.to_string_lossy().to_string()));
            assert_eq!(std::fs::read_to_string(&actual).unwrap(), purpose);
            let second =
                serde_json::json!({"path":"direct.txt","content":"direct","purpose":purpose});
            let out = crate::core::exec_ctx::with_current(ctx.clone(), || write.execute(&second))
                .unwrap();
            assert!(out.contains("direct.txt"));
            assert!(!root.join("direct.txt").exists());
        }
        for name in [
            "../escape.txt",
            "/absolute.txt",
            "C:relative.txt",
            "nested//bad.txt",
            "file:stream",
        ] {
            assert!(
                write
                    .prepare_call(
                        &serde_json::json!({"path":name,"content":"x","purpose":"artifact"}),
                        &ctx
                    )
                    .is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn file_placement_prepared_write_binds_creation_and_overwrite_snapshot() {
        let root = temp_root("purpose-cas");
        let ctx = placement_fixture(&root);
        let write = FileWrite::new(root.clone());
        let args = serde_json::json!({"path":"new.txt","content":"approved","purpose":"project"});
        let prepared = write.prepare_call(&args, &ctx).unwrap();
        std::fs::write(root.join("new.txt"), "created elsewhere").unwrap();
        assert!(write.execute_prepared(&prepared).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "created elsewhere"
        );
        let args = serde_json::json!({"path":"new.txt","content":"approved"});
        let prepared = write.prepare_call(&args, &ctx).unwrap();
        let cached_preview = prepared.preview.as_ref().unwrap().clone();
        std::fs::write(root.join("new.txt"), "changed after approval").unwrap();
        assert_eq!(
            prepared.preview.as_ref().unwrap().lines,
            cached_preview.lines
        );
        assert_eq!(prepared.preview.as_ref().unwrap().diff, cached_preview.diff);
        assert!(write.execute_prepared(&prepared).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "changed after approval"
        );
        let prepared = write.prepare_call(&args, &ctx).unwrap();
        std::fs::remove_file(root.join("new.txt")).unwrap();
        assert!(write.execute_prepared(&prepared).is_err());
        assert!(!root.join("new.txt").exists());
    }

    #[test]
    fn file_placement_moves_protect_workspace_source_and_publish_destination() {
        let root = temp_root("purpose-move");
        let ctx = placement_fixture(&root);
        let mv = FileMove::new(root.clone());
        std::fs::write(root.join("source.txt"), "source").unwrap();
        let prepared = mv
            .prepare_call(
                &serde_json::json!({"from":"source.txt","to":"report.txt","purpose":"artifact"}),
                &ctx,
            )
            .unwrap();
        assert_eq!(mv.workspace_effect(&prepared.args), WorkspaceEffect::Paths);
        assert_eq!(
            mv.workspace_target(&prepared.args),
            Some(root.canonicalize().unwrap())
        );
        let artifact = PathBuf::from(prepared.args["to"].as_str().unwrap());
        let out = mv.execute_prepared(&prepared).unwrap();
        assert!(out.text.contains(&artifact.to_string_lossy().to_string()));
        assert!(!root.join("source.txt").exists());
        let publish = mv
            .prepare_call(
                &serde_json::json!({"from":artifact,"to":"published.txt","purpose":"project"}),
                &ctx,
            )
            .unwrap();
        assert_eq!(mv.workspace_effect(&publish.args), WorkspaceEffect::Paths);
        assert_eq!(
            mv.workspace_target(&publish.args),
            Some(root.canonicalize().unwrap())
        );
        mv.execute_prepared(&publish).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("published.txt")).unwrap(),
            "source"
        );
        assert!(!root.join("report.txt").exists());
    }

    #[test]
    fn file_placement_managed_roots_inside_workspace_do_not_mark_code_edited() {
        let root = temp_root("purpose-home-inside");
        let workspace = root.canonicalize().unwrap();
        let placement = crate::core::file_placement::PlacementContext::with_paths(
            workspace.clone(),
            workspace.join("internal-home"),
            workspace.join("internal-scratch"),
            "internal-home-run",
        )
        .unwrap();
        let ctx =
            ExecutionContext::new(crate::core::convo::ConversationId::new("test:home-inside"))
                .with_placement(placement);
        let write = FileWrite::new(root.clone());
        let prepared = write
            .prepare_call(
                &serde_json::json!({"path":"a.txt","content":"A","purpose":"artifact"}),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            write.workspace_effect(&prepared.args),
            WorkspaceEffect::External
        );
        write.execute_prepared(&prepared).unwrap();
        let artifact = PathBuf::from(prepared.args["path"].as_str().unwrap());
        let followup = write
            .prepare_call(&serde_json::json!({"path":artifact,"content":"B"}), &ctx)
            .unwrap();
        assert_eq!(
            write.workspace_effect_prepared(&followup),
            WorkspaceEffect::External
        );
        write.execute_prepared(&followup).unwrap();
        let mv = FileMove::new(root.clone());
        let managed = mv
            .prepare_call(
                &serde_json::json!({"from":artifact,"to":"b.txt","purpose":"scratch"}),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            mv.workspace_effect_prepared(&managed),
            WorkspaceEffect::External
        );
        mv.execute_prepared(&managed).unwrap();
        let scratch = PathBuf::from(managed.args["to"].as_str().unwrap());
        let publish = mv
            .prepare_call(
                &serde_json::json!({"from":scratch,"to":"published.txt","purpose":"project"}),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            mv.workspace_effect_prepared(&publish),
            WorkspaceEffect::Paths
        );
        assert_eq!(
            mv.workspace_target_prepared(&publish),
            Some(workspace.clone())
        );
        mv.execute_prepared(&publish).unwrap();
        let external = temp_root("purpose-source-outside");
        std::fs::write(external.join("outside.txt"), "outside").unwrap();
        let out_to_managed = mv.prepare_call(&serde_json::json!({"from":external.join("outside.txt"),"to":"outside.txt","purpose":"artifact"}), &ctx).unwrap();
        assert_eq!(
            mv.workspace_effect(&out_to_managed.args),
            WorkspaceEffect::External
        );
        assert_eq!(
            std::fs::read_to_string(root.join("published.txt")).unwrap(),
            "B"
        );
    }

    #[test]
    fn file_placement_moves_bind_source_and_legacy_existing_destination() {
        let root = temp_root("purpose-move-drift");
        let ctx = placement_fixture(&root);
        let mv = FileMove::new(root.clone());
        std::fs::write(root.join("a.txt"), "A").unwrap();
        std::fs::write(root.join("b.txt"), "B").unwrap();
        let args = serde_json::json!({"from":"a.txt","to":"b.txt","overwrite":true});
        let prepared = mv.prepare_call(&args, &ctx).unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        assert!(mv.execute_prepared(&prepared).is_err());
        assert!(root.join("a.txt").exists());
        std::fs::write(root.join("b.txt"), "B").unwrap();
        let prepared = mv.prepare_call(&args, &ctx).unwrap();
        std::fs::write(root.join("a.txt"), "changed source").unwrap();
        assert!(mv.execute_prepared(&prepared).is_err());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "B");
        let prepared = mv.prepare_call(&args, &ctx).unwrap();
        mv.execute_prepared(&prepared).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "changed source"
        );
    }

    #[test]
    fn file_placement_managed_directory_move_is_bounded_and_detects_descendant_drift() {
        let root = temp_root("purpose-move-tree");
        let ctx = placement_fixture(&root);
        std::fs::create_dir_all(root.join("tree/deep")).unwrap();
        std::fs::write(root.join("tree/deep/a.txt"), "A").unwrap();
        let mv = FileMove::new(root.clone());
        let args = serde_json::json!({"from":"tree","to":"tree","purpose":"artifact"});
        let prepared = mv.prepare_call(&args, &ctx).unwrap();
        std::fs::write(root.join("tree/deep/a.txt"), "B").unwrap();
        assert!(mv.execute_prepared(&prepared).is_err());
        assert!(root.join("tree").is_dir());
        let prepared = mv.prepare_call(&args, &ctx).unwrap();
        let dst = PathBuf::from(prepared.args["to"].as_str().unwrap());
        mv.execute_prepared(&prepared).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("deep/a.txt")).unwrap(),
            "B"
        );
        let big = root.join("big");
        std::fs::create_dir(&big).unwrap();
        for n in 0..1024 {
            std::fs::write(big.join(format!("{n}.txt")), "").unwrap();
        }
        assert!(mv
            .prepare_call(
                &serde_json::json!({"from":"big","to":"big","purpose":"scratch"}),
                &ctx
            )
            .unwrap_err()
            .to_string()
            .contains("limit"));
        assert!(!ctx.placement().unwrap().scratch_root().exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_placement_managed_directory_move_refuses_symlinks() {
        let root = temp_root("purpose-move-links");
        let ctx = placement_fixture(&root);
        std::fs::create_dir(root.join("tree")).unwrap();
        std::os::unix::fs::symlink(root.parent().unwrap(), root.join("tree/escape")).unwrap();
        let mv = FileMove::new(root.clone());
        assert!(mv
            .prepare_call(
                &serde_json::json!({"from":"tree","to":"tree","purpose":"artifact"}),
                &ctx
            )
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(!ctx.placement().unwrap().home_root().exists());
        let write = FileWrite::new(root.clone());
        let prepared = write
            .prepare_call(
                &serde_json::json!({"path":"tree/a.txt","content":"A","purpose":"artifact"}),
                &ctx,
            )
            .unwrap();
        write.execute_prepared(&prepared).unwrap();
        let managed_tree = ctx.placement().unwrap().artifact_root().join("tree");
        std::os::unix::fs::symlink(root.parent().unwrap(), managed_tree.join("escape")).unwrap();
        assert!(mv
            .prepare_call(
                &serde_json::json!({"from":managed_tree,"to":"published","purpose":"project"}),
                &ctx
            )
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(!root.join("published").exists());
    }

    #[test]
    fn file_placement_move_refuses_distinct_workspace_owners() {
        let root = temp_root("purpose-move-owners");
        let outside = temp_root("purpose-move-owner-other");
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(outside.join(".git")).unwrap();
        std::fs::write(root.join("a.txt"), "A").unwrap();
        let ctx = placement_fixture(&root);
        let mv = FileMove::new(root.clone());
        assert!(mv
            .prepare_call(
                &serde_json::json!({"from":"a.txt","to":outside.join("b.txt"),"purpose":"project"}),
                &ctx
            )
            .unwrap_err()
            .to_string()
            .contains("distinct"));
        assert!(root.join("a.txt").exists());
        assert!(!outside.join("b.txt").exists());
    }

    #[test]
    fn file_write_creates_and_overwrites() {
        let root = temp_root("write");
        let t = FileWrite::new(root.clone());
        // create
        let r = t
            .execute(
                &serde_json::json!({"path":"a.txt","content":"one\ntwo\n","purpose":"project"}),
            )
            .unwrap();
        assert!(r.starts_with("created ") && r.contains("a.txt"), "{r:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        // overwrite an EXISTING non-empty file wholesale — the case that used to force `type NUL >`.
        let r2 = t
            .execute(&serde_json::json!({"path":"a.txt","content":"brand new\n"}))
            .unwrap();
        assert!(
            r2.starts_with("overwrote ") && r2.contains("a.txt"),
            "{r2:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "brand new\n"
        );
        assert!(t.is_destructive() && !t.is_concurrency_safe());
    }

    #[test]
    fn file_write_reaches_outside_but_needs_parent() {
        let root = temp_root("write-guard");
        let t = FileWrite::new(root.clone());
        // Confinement was REMOVED: a `../` path now writes to the sibling location (its parent, the
        // temp dir, exists) instead of being refused. Clean up the file we create outside root.
        let outside = root.parent().unwrap().join("evil.txt");
        let _ = std::fs::remove_file(&outside);
        assert!(t
            .execute(&serde_json::json!({"path":"../evil.txt","content":"x","purpose":"project"}))
            .is_ok());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "x");
        let _ = std::fs::remove_file(&outside);
        // The one remaining guard: writing into a non-existent subdir still errors (parent must exist).
        assert!(t
            .execute(&serde_json::json!({"path":"nope/deep.txt","content":"x","purpose":"project"}))
            .is_err());
    }

    #[test]
    fn multi_edit_applies_ordered_edits() {
        // The batch (`edits`) form of file_edit, formerly the separate multi_edit tool.
        let root = temp_root("medit-order");
        std::fs::write(root.join("f.txt"), "alpha beta gamma").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "alpha", "new_string": "A"},
                    {"old_string": "gamma", "new_string": "G"}
                ]
            }))
            .unwrap();
        assert!(r.contains("2 edits applied"), "got: {r}");
        assert!(r.contains("#1") && r.contains("#2"));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "A beta G"
        );
    }

    #[test]
    fn multi_edit_sequential_sees_prior_result() {
        // Edit #2 targets text PRODUCED by edit #1 (evolving-buffer proof).
        let root = temp_root("medit-evolve");
        std::fs::write(root.join("f.txt"), "one").unwrap();
        let t = FileEdit::new(root.clone());
        t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_string": "one", "new_string": "two"},
                {"old_string": "two", "new_string": "three"}
            ]
        }))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "three"
        );
    }

    #[test]
    fn multi_edit_atomic_rollback_on_failure() {
        // Edit #1 valid, #2 absent → whole call fails and the file equals the ORIGINAL (atomicity).
        let root = temp_root("medit-atomic");
        std::fs::write(root.join("f.txt"), "keep me exactly").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_string": "keep", "new_string": "KEEP"},
                {"old_string": "does-not-exist", "new_string": "X"}
            ]
        }));
        assert!(r.is_err(), "a failing edit must abort the whole call");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "keep me exactly",
            "nothing must be written when any edit fails"
        );
    }

    #[test]
    fn multi_edit_reports_failing_index() {
        let root = temp_root("medit-idx");
        std::fs::write(root.join("f.txt"), "a b c").unwrap();
        let t = FileEdit::new(root);
        let err = t
            .execute(&serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "A"},
                    {"old_string": "zzz", "new_string": "Z"}
                ]
            }))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("edit #2"),
            "error names the failing edit index: {err}"
        );
    }

    #[test]
    fn multi_edit_replace_all_per_edit() {
        let root = temp_root("medit-all");
        std::fs::write(root.join("f.txt"), "x x x | y").unwrap();
        let t = FileEdit::new(root.clone());
        t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_string": "x", "new_string": "Z", "replace_all": true},
                {"old_string": "y", "new_string": "Y"}
            ]
        }))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "Z Z Z | Y"
        );
        // A non-unique edit WITHOUT replace_all aborts atomically.
        std::fs::write(root.join("g.txt"), "a a").unwrap();
        let r = t.execute(&serde_json::json!({
            "path": "g.txt",
            "edits": [{"old_string": "a", "new_string": "b"}]
        }));
        assert!(r.is_err(), "non-unique without replace_all must error");
        assert_eq!(std::fs::read_to_string(root.join("g.txt")).unwrap(), "a a");
    }

    #[test]
    fn multi_edit_indent_tolerant_per_edit() {
        // A 2-line block with wrong indentation on BOTH lines isn't an accidental exact substring,
        // so it genuinely exercises the indentation-tolerant fallback (mirrors the single-edit test).
        let root = temp_root("medit-indent");
        std::fs::write(
            root.join("f.rs"),
            "fn main() {\n    let x = 1;\n    foo();\n}\n",
        )
        .unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.rs",
                "edits": [{"old_string": "  let x = 1;\n  foo();", "new_string": "    let x = 2;\n    bar();"}]
            }))
            .unwrap();
        assert!(r.contains("indentation-tolerant"), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "fn main() {\n    let x = 2;\n    bar();\n}\n"
        );
    }

    #[test]
    fn file_edit_without_either_form_names_both() {
        // A call with neither `new_string` nor `edits` must not be told only "missing new_string" —
        // that hides the batch form from a model that was reaching for it.
        let root = temp_root("fe-neither");
        std::fs::write(root.join("f.txt"), "hi").unwrap();
        let t = FileEdit::new(root);
        let err = t
            .execute(&serde_json::json!({"path": "f.txt"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("new_string"), "{err}");
        assert!(
            err.contains("edits"),
            "must mention the batch form too: {err}"
        );
    }

    #[test]
    fn multi_edit_empty_edits_array_errs() {
        let root = temp_root("medit-empty");
        std::fs::write(root.join("f.txt"), "hi").unwrap();
        let t = FileEdit::new(root);
        assert!(t
            .execute(&serde_json::json!({"path": "f.txt", "edits": []}))
            .is_err());
    }

    #[test]
    fn multi_edit_rejects_escape() {
        let root = temp_root("medit-escape");
        let t = FileEdit::new(root);
        let r = t.execute(&serde_json::json!({
            "path": "../../secret",
            "edits": [{"old_string": "a", "new_string": "b"}]
        }));
        assert!(r.is_err(), "path escaping the working dir must be refused");
    }

    #[test]
    fn multi_edit_nonexistent_file_errs() {
        let root = temp_root("medit-nofile");
        let t = FileEdit::new(root.clone());
        let args = serde_json::json!({"path": "nope.txt", "edits": [{"old_string": "a", "new_string": "b"}]});
        let ctx = placement_fixture(&root);
        assert!(t.prepare_call(&args, &ctx).is_err());
        assert!(t.execute(&args).is_err());
        assert!(!root.join("nope.txt").exists());
    }

    #[test]
    fn multi_edit_is_destructive_not_concurrency_safe() {
        let t = FileEdit::new(PathBuf::from("."));
        assert!(t.is_destructive(), "must be approval-gated");
        assert!(!t.is_concurrency_safe(), "must take the serial path");
    }

    #[test]
    fn multi_edit_empty_old_string_errs() {
        // Mid-edit create-new is meaningless; apply_one_edit rejects an empty old_string.
        let root = temp_root("medit-emptyold");
        std::fs::write(root.join("f.txt"), "hi").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [{"old_string": "", "new_string": "X"}]
        }));
        assert!(r.is_err(), "empty old_string mid-edit must error");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "hi",
            "nothing written"
        );
    }

    #[test]
    fn file_edit_batch_and_single_dispatch_by_edits_presence() {
        // The merged tool: `edits` present → batch; absent → single old_string/new_string.
        let root = temp_root("fe-dispatch");
        let p = root.join("f.txt");
        std::fs::write(&p, "one two").unwrap();
        let t = FileEdit::new(root.clone());
        // single form
        t.execute(&serde_json::json!({"path":"f.txt","old_string":"one","new_string":"1"}))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "1 two");
        // batch form
        t.execute(&serde_json::json!({
            "path":"f.txt",
            "edits":[{"old_string":"1","new_string":"ONE"},{"old_string":"two","new_string":"TWO"}]
        }))
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "ONE TWO");
    }

    /// Every new native-preparation test owns all workspace, home and process roots.
    fn native_preparation_fixture() -> (
        crate::core::file_placement::tests::TestDir,
        ExecutionContext,
    ) {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let placement = crate::core::file_placement::PlacementContext::with_paths(
            workspace,
            temp.path().join("home"),
            temp.path().join("process"),
            "bounded-native-test",
        )
        .unwrap();
        let ctx = ExecutionContext::new(crate::core::convo::ConversationId::new(
            "test:bounded-native",
        ))
        .with_placement(placement);
        (temp, ctx)
    }

    struct NativeBodyCall {
        tool: Box<dyn Tool>,
        prepared: PreparedToolCall,
        args: Value,
        ctx: ExecutionContext,
        root: PathBuf,
        target: PathBuf,
    }

    fn native_child_body(
        parent: &ExecutionContext,
        owner: &str,
        name: &str,
        args: Value,
    ) -> NativeBodyCall {
        let root = parent.placement().unwrap().workspace_root().to_path_buf();
        let path = args["path"].as_str().unwrap();
        let target = root.join(path);
        // Derive each child independently from the parent, not by cloning a sibling's identity.
        let ctx = crate::agent::task_tool::child_execution_context(parent, owner, &root, name)
            .with_native_scope(
                crate::core::native_scope::NativeWriteScope::resolve(&root, &[path.into()])
                    .unwrap(),
            )
            .unwrap();
        assert!(!ctx.can_restore_workspace());
        assert!(!ctx.recovery().can_restore_workspace());
        let tool: Box<dyn Tool> = match name {
            "file_read" => Box::new(FileRead::new(root.clone())),
            "file_edit" => Box::new(FileEdit::new(root.clone())),
            "file_write" => Box::new(FileWrite::new(root.clone())),
            _ => panic!("unexpected native test tool"),
        };
        let prepared = tool.prepare_call(&args, &ctx).unwrap();
        NativeBodyCall {
            tool,
            prepared,
            args,
            ctx,
            root,
            target,
        }
    }

    impl NativeBodyCall {
        fn resources(&self) -> &[(NativeResource, LockMode)] {
            match self.prepared.footprint.as_ref().unwrap() {
                PreparedFootprint::Native(resources) => resources,
                _ => panic!("test must admit the actual frozen exact footprint"),
            }
        }

        fn acquire(&self) -> Result<crate::core::workspace_txn::NativeResourceLease> {
            crate::core::workspace_txn::NativeResourceLease::acquire(
                &self.root,
                self.resources(),
                Duration::ZERO,
                None,
                &self.ctx.resource_scope(),
            )
        }

        fn execute(&self) -> String {
            let _lease = self.acquire().unwrap();
            let output = self.tool.execute_prepared(&self.prepared).unwrap().text;
            if self.tool.name() != "file_read" {
                // Still inside the same final lease. Successful settlement leaves no pending intent.
                assert!(
                    crate::core::native_journal::reconcile(&self.resources()[0].0)
                        .unwrap()
                        .is_empty()
                );
                let error = crate::core::native_journal::undo_owner(
                    &self.ctx,
                    &self.resources()[0].0,
                    &self.ctx.resource_scope(),
                )
                .unwrap_err();
                assert!(error
                    .to_string()
                    .contains("delegated execution cannot undo"));
            }
            output
        }
    }

    struct NativeBodyWorker {
        entered: std::sync::mpsc::Receiver<(String, PathBuf)>,
        release: std::sync::mpsc::Sender<()>,
        handle: std::thread::JoinHandle<String>,
    }

    fn native_body_channels(
        call: &NativeBodyCall,
    ) -> (
        NativeBodyHook,
        std::sync::mpsc::Receiver<(String, PathBuf)>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let expected_tool = call.tool.name().to_string();
        let expected_target = call.target.clone();
        let hook: NativeBodyHook = Box::new(move |tool, target| {
            assert_eq!(tool, expected_tool);
            assert_eq!(target, expected_target);
            entered_tx
                .send((tool.to_string(), target.to_path_buf()))
                .unwrap();
            // Timeout is only a deadlock watchdog, never evidence of exclusion or overlap.
            release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        });
        (hook, entered, release)
    }

    fn spawn_native_body(call: NativeBodyCall, home: &Path) -> NativeBodyWorker {
        let (hook, entered, release) = native_body_channels(&call);
        let home = home.to_path_buf();
        let handle = std::thread::spawn(move || {
            crate::core::workspace_txn::with_test_lock_home(&home, || {
                crate::core::native_journal::with_test_storage(&home, || {
                    crate::core::exec_ctx::with_current(call.ctx.clone(), || {
                        with_native_body_hook(hook, || call.execute())
                    })
                })
            })
        });
        NativeBodyWorker {
            entered,
            release,
            handle,
        }
    }

    fn actual_native_bodies_overlap(
        first_name: &str,
        first_args: Value,
        second_name: &str,
        second_args: Value,
    ) {
        let (temp, parent) = native_preparation_fixture();
        let root = parent.placement().unwrap().workspace_root().to_path_buf();
        std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(root.join("b.txt"), "beta\n").unwrap();
        let first = native_child_body(&parent, "test:native/first", first_name, first_args);
        let second = native_child_body(&parent, "test:native/second", second_name, second_args);
        assert_ne!(first.ctx.resource_scope(), second.ctx.resource_scope());
        let first_target = first.target.clone();
        let second_target = second.target.clone();
        let first = spawn_native_body(first, temp.path());
        assert_eq!(
            first.entered.recv_timeout(Duration::from_secs(15)).unwrap(),
            (first_name.into(), first_target)
        );
        let second = spawn_native_body(second, temp.path());
        // Neither body is released until BOTH actual bodies have reported arrival. The writer
        // rendezvous is before Storage::open: journal serialization must not fake a body conflict.
        assert_eq!(
            second
                .entered
                .recv_timeout(Duration::from_secs(15))
                .unwrap(),
            (second_name.into(), second_target)
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "beta\n"
        );
        first.release.send(()).unwrap();
        second.release.send(()).unwrap();
        let first_output = first.handle.join().unwrap();
        let second_output = second.handle.join().unwrap();
        if first_name == "file_read" && second_name == "file_read" {
            assert_eq!(first_output, "alpha\n");
            assert_eq!(second_output, "alpha\n");
            assert!(!temp.path().join("native-journal-v1/records.bin").exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).unwrap(),
                "ALPHA\n"
            );
            assert!(temp.path().join("native-journal-v1/records.bin").is_file());
            if second_name == "file_read" {
                assert_eq!(second_output, "beta\n");
                assert_eq!(
                    std::fs::read_to_string(root.join("b.txt")).unwrap(),
                    "beta\n"
                );
            } else {
                assert_eq!(
                    std::fs::read_to_string(root.join("b.txt")).unwrap(),
                    "BETA\n"
                );
                // Parent can select the exact owner; a sibling owns no records for this file.
                crate::core::workspace_txn::with_test_lock_home(temp.path(), || {
                    crate::core::native_journal::with_test_storage(temp.path(), || {
                        crate::core::exec_ctx::with_current(parent.clone(), || {
                            let resource = NativeResource::resolve(&root.join("a.txt")).unwrap();
                            let _lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                                &root,
                                &[(resource.clone(), LockMode::Exclusive)],
                                Duration::ZERO,
                                None,
                                &parent.resource_scope(),
                            )
                            .unwrap();
                            assert_eq!(
                                crate::core::native_journal::undo_owner(
                                    &parent,
                                    &resource,
                                    "test:native/second",
                                )
                                .unwrap(),
                                0
                            );
                            assert_eq!(
                                crate::core::native_journal::undo_owner(
                                    &parent,
                                    &resource,
                                    "test:native/first",
                                )
                                .unwrap(),
                                1
                            );
                            assert_eq!(
                                std::fs::read_to_string(root.join("a.txt")).unwrap(),
                                "alpha\n"
                            );
                            assert_eq!(
                                std::fs::read_to_string(root.join("b.txt")).unwrap(),
                                "BETA\n"
                            );
                        })
                    })
                });
            }
        }
        crate::core::read_ledger::forget(&root.join("a.txt"));
        crate::core::read_ledger::forget(&root.join("b.txt"));
    }

    #[test]
    fn native_child_disjoint_file_write_and_edit_bodies_overlap_and_settle() {
        actual_native_bodies_overlap(
            "file_write",
            serde_json::json!({"path":"a.txt","content":"ALPHA\n"}),
            "file_edit",
            serde_json::json!({"path":"b.txt","old_string":"beta","new_string":"BETA"}),
        );
    }

    #[test]
    fn native_child_disjoint_file_edit_and_read_bodies_overlap() {
        actual_native_bodies_overlap(
            "file_edit",
            serde_json::json!({"path":"a.txt","old_string":"alpha","new_string":"ALPHA"}),
            "file_read",
            serde_json::json!({"path":"b.txt"}),
        );
    }

    #[test]
    fn native_child_disjoint_file_write_and_read_bodies_overlap() {
        actual_native_bodies_overlap(
            "file_write",
            serde_json::json!({"path":"a.txt","content":"ALPHA\n"}),
            "file_read",
            serde_json::json!({"path":"b.txt"}),
        );
    }

    #[test]
    fn native_child_same_file_read_bodies_overlap() {
        actual_native_bodies_overlap(
            "file_read",
            serde_json::json!({"path":"a.txt"}),
            "file_read",
            serde_json::json!({"path":"a.txt"}),
        );
    }

    fn actual_native_second_body_is_excluded(
        first_name: &str,
        first_args: Value,
        second_name: &str,
        second_args: Value,
        expected: &str,
    ) {
        let (temp, parent) = native_preparation_fixture();
        let root = parent.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("a.txt");
        std::fs::write(&target, "alpha\n").unwrap();
        let first = native_child_body(&parent, "test:excluded/first", first_name, first_args);
        let mut second =
            native_child_body(&parent, "test:excluded/second", second_name, second_args);
        assert_ne!(first.ctx.resource_scope(), second.ctx.resource_scope());
        let resource_key = second.resources()[0].0.key().to_string();
        let (hook, entered, release) = native_body_channels(&second);
        let (busy_tx, busy_rx) = std::sync::mpsc::channel();
        let (retry_tx, retry_rx) = std::sync::mpsc::channel();
        let first = spawn_native_body(first, temp.path());
        first.entered.recv_timeout(Duration::from_secs(15)).unwrap();
        let home = temp.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            crate::core::workspace_txn::with_test_lock_home(&home, || {
                crate::core::native_journal::with_test_storage(&home, || {
                    crate::core::exec_ctx::with_current(second.ctx.clone(), || {
                        with_native_body_hook(hook, || {
                            match second.acquire() {
                                Ok(lease) => {
                                    // Regression must enter the REAL second body, not a fake closure.
                                    second.tool.execute_prepared(&second.prepared).unwrap();
                                    drop(lease);
                                    panic!("same-file conflicting body was admitted");
                                }
                                Err(error) => {
                                    let busy = error.downcast_ref::<crate::core::repo_lock::LockBusy>()
                                        .expect("expected typed exact-resource exclusion, not a body/preparation error");
                                    assert_eq!(
                                        busy.path,
                                        crate::core::workspace_txn::resource_lock(
                                            "native-file",
                                            &resource_key
                                        )
                                    );
                                    busy_tx.send(()).unwrap();
                                }
                            }
                            retry_rx.recv_timeout(Duration::from_secs(15)).unwrap();
                            // A preceding writer changes identity/content. Explicit fresh preparation
                            // is required after releasing it, rather than silently refreshing approval.
                            second.prepared =
                                second.tool.prepare_call(&second.args, &second.ctx).unwrap();
                            second.execute()
                        })
                    })
                })
            })
        });
        busy_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(matches!(
            entered.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "alpha\n");
        first.release.send(()).unwrap();
        first.handle.join().unwrap();
        retry_tx.send(()).unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(15)).unwrap(),
            (second_name.into(), target.clone())
        );
        release.send(()).unwrap();
        let output = handle.join().unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
        if second_name == "file_read" {
            assert_eq!(output, expected);
        }
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn native_child_same_file_write_blocks_edit_body_until_release() {
        actual_native_second_body_is_excluded(
            "file_write",
            serde_json::json!({"path":"a.txt","content":"alpha changed\n"}),
            "file_edit",
            serde_json::json!({"path":"a.txt","old_string":"alpha","new_string":"final"}),
            "final changed\n",
        );
    }

    #[test]
    fn native_child_same_file_read_blocks_write_body_until_release() {
        actual_native_second_body_is_excluded(
            "file_read",
            serde_json::json!({"path":"a.txt"}),
            "file_write",
            serde_json::json!({"path":"a.txt","content":"ALPHA\n"}),
            "ALPHA\n",
        );
    }

    #[test]
    fn native_child_same_file_write_blocks_read_body_until_release() {
        actual_native_second_body_is_excluded(
            "file_write",
            serde_json::json!({"path":"a.txt","content":"ALPHA\n"}),
            "file_read",
            serde_json::json!({"path":"a.txt"}),
            "ALPHA\n",
        );
    }

    #[test]
    fn prepared_file_write_freezes_bytes_destination_output_and_footprint() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let write = FileWrite::new(root.clone());
        for existing in [false, true] {
            let target = root.join(if existing { "existing.txt" } else { "new.txt" });
            if existing {
                std::fs::write(&target, "before\n").unwrap();
            }
            let mut prepared = write
                .prepare_call(
                    &serde_json::json!({
                        "path":target,"content":"approved\n","purpose":"project"
                    }),
                    &ctx,
                )
                .unwrap();
            let frozen = prepared
                .delegation
                .as_ref()
                .unwrap()
                .downcast_ref::<FrozenFileWrite>()
                .unwrap();
            let expected_output = frozen.output.clone();
            assert_eq!(frozen.content, "approved\n");
            match prepared.footprint.as_ref().unwrap() {
                PreparedFootprint::Native(resources) => {
                    assert_eq!(resources.len(), 1);
                    assert_eq!(resources[0].0.path(), target);
                    assert_eq!(resources[0].1, LockMode::Exclusive);
                }
                PreparedFootprint::Global => {
                    panic!("ordinary regular target should resolve exactly")
                }
                PreparedFootprint::Overlay { .. } => {
                    panic!("overlay footprint is not valid in live fixture")
                }
            }
            prepared.args["path"] = serde_json::json!(root.join("unapproved.txt"));
            prepared.args["content"] = serde_json::json!("UNAPPROVED".repeat(1024));
            prepared.args["purpose"] = serde_json::json!("scratch");
            let output = write.execute_prepared(&prepared).unwrap();
            assert!(output.text.starts_with(&expected_output));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved\n");
            assert!(!root.join("unapproved.txt").exists());
            crate::core::read_ledger::forget(&target);
        }
    }

    #[test]
    fn prepared_file_write_refuses_drift_and_rechecks_ledger_without_refreshing_preview() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        let write = FileWrite::new(root.clone());
        std::fs::write(&target, "baseline\n").unwrap();
        let prepared = write
            .prepare_call(
                &serde_json::json!({"path":"f.txt","content":"approved\n"}),
                &ctx,
            )
            .unwrap();
        std::fs::write(&target, "peer\n").unwrap();
        FileRead::new(root.clone())
            .execute(&serde_json::json!({"path":"f.txt"}))
            .unwrap();
        assert!(write.execute_prepared(&prepared).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "peer\n");
        let prepared = write
            .prepare_call(
                &serde_json::json!({"path":"f.txt","content":"approved\n"}),
                &ctx,
            )
            .unwrap();
        let stale = crate::core::persist::FileFingerprint::for_bytes(b"different observation");
        crate::core::read_ledger::note(&target, &stale);
        assert!(write
            .preview(&serde_json::json!({"path":"f.txt","content":"approved\n"}))
            .is_some());
        assert_eq!(
            crate::core::read_ledger::observed(&target),
            Some(stale.clone())
        );
        assert!(write
            .execute_prepared(&prepared)
            .unwrap_err()
            .to_string()
            .contains("overwrite conflict"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "peer\n");
        assert_eq!(crate::core::read_ledger::observed(&target), Some(stale));
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_write_noop_is_frozen_and_does_not_refresh_ledger() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        std::fs::write(&target, "same\n").unwrap();
        let modified = std::fs::metadata(&target).unwrap().modified().unwrap();
        let stale = crate::core::persist::FileFingerprint::for_bytes(b"prior read");
        crate::core::read_ledger::note(&target, &stale);
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.txt".into()]).unwrap();
        let ctx = ctx.with_native_scope(scope).unwrap();
        let write = FileWrite::new(root.clone());
        let mut prepared = write
            .prepare_call(
                &serde_json::json!({"path":"f.txt","content":"same\n"}),
                &ctx,
            )
            .unwrap();
        prepared.args["content"] = serde_json::json!("unapproved");
        assert_eq!(
            write.workspace_effect_prepared(&prepared),
            WorkspaceEffect::None
        );
        assert!(crate::core::exec_ctx::with_current(ctx.clone(), || write
            .execute_prepared(&prepared))
        .unwrap()
        .text
        .starts_with(NOOP_WRITE_PREFIX));
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(crate::core::read_ledger::observed(&target), Some(stale));
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_write_bounds_arguments_file_and_diff() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        let write = FileWrite::new(root.clone());
        std::fs::write(&target, "small").unwrap();
        let oversized = "x".repeat(PREPARED_EDIT_MAX_BYTES + 1);
        for args in [
            serde_json::json!({"path":"f.txt","content":oversized}),
            serde_json::json!({"path":"f.txt","content":"small","ignored":oversized}),
        ] {
            assert!(
                format!("{:#}", write.prepare_call(&args, &ctx).unwrap_err()).contains("4 MiB")
            );
        }
        std::fs::write(&target, &oversized).unwrap();
        assert!(write
            .prepare_call(&serde_json::json!({"path":"f.txt","content":"small"}), &ctx)
            .is_err());
        assert!(write
            .preview(&serde_json::json!({"path":"f.txt","content":"small"}))
            .is_none());
        assert_eq!(
            std::fs::metadata(&target).unwrap().len(),
            oversized.len() as u64
        );
        // Both source and replacement fit individually; their minified-line diff does not.
        std::fs::write(&target, "a".repeat(PREPARED_EDIT_MAX_BYTES * 3 / 4)).unwrap();
        assert!(format!(
            "{:#}",
            write
                .prepare_call(
                    &serde_json::json!({
                        "path":"f.txt","content":"b".repeat(PREPARED_EDIT_MAX_BYTES * 3 / 4)
                    }),
                    &ctx
                )
                .unwrap_err()
        )
        .contains("4 MiB"));
    }

    #[test]
    fn prepared_file_read_freezes_options_targets_and_reads_new_content() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        std::fs::write(&target, "old one\nold two\n").unwrap();
        std::fs::write(root.join("other.txt"), "UNAPPROVED").unwrap();
        let read = FileRead::new(root.clone());
        let mut prepared = read
            .prepare_call(
                &serde_json::json!({"path":"f.txt","start":2,"end":2,"number":true}),
                &ctx,
            )
            .unwrap();
        assert!(crate::core::read_ledger::observed(&target).is_none());
        match prepared.footprint.as_ref().unwrap() {
            PreparedFootprint::Native(resources) => {
                assert_eq!(resources.len(), 1);
                assert_eq!(resources[0].0.path(), target);
                assert_eq!(resources[0].1, LockMode::Shared);
            }
            PreparedFootprint::Global => panic!("regular read target should resolve exactly"),
            PreparedFootprint::Overlay { .. } => panic!("overlay footprint is not valid in live fixture"),
        }
        std::fs::write(&target, "new one\nnew two\n").unwrap();
        prepared.args = serde_json::json!({"path":"other.txt","files":[{"path":"other.txt"}],"start":1,"number":false});
        assert_eq!(read.execute_prepared(&prepared).unwrap().text, "2|new two");
        assert_eq!(
            crate::core::read_ledger::observed(&target),
            Some(crate::core::persist::FileFingerprint::for_bytes(
                b"new one\nnew two\n"
            ))
        );
        assert!(crate::core::read_ledger::observed(&root.join("other.txt")).is_none());
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_read_batch_footprint_is_exact_first_eight_actual_targets() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let mut files = Vec::new();
        for n in 0..9 {
            std::fs::write(root.join(format!("{n}.txt")), format!("body-{n}\n")).unwrap();
            files.push(serde_json::json!({"path":format!("{n}.txt")}));
        }
        let read = FileRead::new(root.clone());
        let mut prepared = read
            .prepare_call(
                &serde_json::json!({"path":"ignored-missing.txt","files":files}),
                &ctx,
            )
            .unwrap();
        match prepared.footprint.as_ref().unwrap() {
            PreparedFootprint::Native(resources) => {
                assert_eq!(resources.len(), MULTI_READ_MAX_FILES);
                for (n, (resource, mode)) in resources.iter().enumerate() {
                    assert_eq!(resource.path(), root.join(format!("{n}.txt")));
                    assert_eq!(*mode, LockMode::Shared);
                }
            }
            PreparedFootprint::Global => panic!("valid batch should resolve exact read footprint"),
            PreparedFootprint::Overlay { .. } => panic!("overlay footprint is not valid in live fixture"),
        }
        prepared.args["files"] = serde_json::json!([{"path":"8.txt"}]);
        let output = read.execute_prepared(&prepared).unwrap().text;
        assert!(output.contains("body-0") && output.contains("body-7"));
        assert!(!output.contains("body-8"));
        assert!(output.contains("1 more file(s) not read"));
        for n in 0..8 {
            crate::core::read_ledger::forget(&root.join(format!("{n}.txt")));
        }
        assert!(crate::core::read_ledger::observed(&root.join("8.txt")).is_none());
    }

    #[test]
    fn prepared_file_read_refuses_identity_drift_and_bounds_disk_read() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        std::fs::write(&target, "original").unwrap();
        let read = FileRead::new(root.clone());
        let prepared = read
            .prepare_call(&serde_json::json!({"path":"f.txt"}), &ctx)
            .unwrap();
        std::fs::rename(&target, root.join("old.txt")).unwrap();
        std::fs::write(&target, "different identity").unwrap();
        assert!(read.execute_prepared(&prepared).is_err());
        assert!(crate::core::read_ledger::observed(&target).is_none());
        let prepared = read
            .prepare_call(&serde_json::json!({"path":"f.txt","start":1,"end":1}), &ctx)
            .unwrap();
        std::fs::write(&target, vec![b'x'; PREPARED_EDIT_MAX_BYTES + 1]).unwrap();
        assert!(format!("{:#}", read.execute_prepared(&prepared).unwrap_err()).contains("4 MiB"));
        assert!(crate::core::read_ledger::observed(&target).is_none());
    }

    #[test]
    fn prepared_native_read_and_write_refuse_ambiguous_scoped_footprints() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.txt".into()]).unwrap();
        let scoped = ctx.with_native_scope(scope).unwrap();
        let read = FileRead::new(root.clone());
        let args = serde_json::json!({"files":[{"path":"missing.txt"}]});
        assert!(matches!(
            read.prepare_call(&args, &ctx).unwrap().footprint,
            Some(PreparedFootprint::Global)
        ));
        assert!(read.prepare_call(&args, &scoped).is_err());
        std::fs::create_dir(root.join("directory")).unwrap();
        let args = serde_json::json!({"path":"directory"});
        assert!(matches!(
            read.prepare_call(&args, &ctx).unwrap().footprint,
            Some(PreparedFootprint::Global)
        ));
        assert!(read.prepare_call(&args, &scoped).is_err());
        assert!(FileWrite::new(root)
            .prepare_call(
                &serde_json::json!({
                    "path":"directory","content":"forbidden","purpose":"project"
                }),
                &scoped
            )
            .is_err());
    }

    #[test]
    fn prepared_scoped_read_skips_lsp_hint_and_scoped_write_rechecks_authority() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.rs");
        let body = "fn item() {}\n".repeat(150);
        std::fs::write(&target, &body).unwrap();
        std::fs::write(root.join("outside.rs"), "outside\n").unwrap();
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.rs".into()]).unwrap();
        let unscoped = ctx.clone();
        let ctx = ctx.with_native_scope(scope).unwrap();
        let read = FileRead::new(root.clone());
        let prepared = read
            .prepare_call(&serde_json::json!({"path":"f.rs"}), &ctx)
            .unwrap();
        assert_eq!(
            crate::core::exec_ctx::with_current(ctx.clone(), || read.execute_prepared(&prepared))
                .unwrap()
                .text,
            body
        );
        let write = FileWrite::new(root.clone());
        let write_args = serde_json::json!({"path":"outside.rs","content":"forbidden\n"});
        assert!(write.prepare_call(&write_args, &ctx).is_err());
        let prepared = write.prepare_call(&write_args, &unscoped).unwrap();
        assert!(crate::core::exec_ctx::with_current(ctx.clone(), || write
            .execute_prepared(&prepared))
        .unwrap_err()
        .to_string()
        .contains("scope"));
        assert_eq!(
            std::fs::read_to_string(root.join("outside.rs")).unwrap(),
            "outside\n"
        );
        let edit = FileEdit::new(root.clone());
        let edit_args = serde_json::json!({"path":"outside.rs","old_string":"outside","new_string":"forbidden"});
        assert!(edit.prepare_call(&edit_args, &ctx).is_err());
        let prepared = edit.prepare_call(&edit_args, &unscoped).unwrap();
        assert!(crate::core::exec_ctx::with_current(ctx.clone(), || edit
            .execute_prepared(&prepared))
        .unwrap_err()
        .to_string()
        .contains("scope"));
        assert_eq!(
            std::fs::read_to_string(root.join("outside.rs")).unwrap(),
            "outside\n"
        );
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn ordinary_prepared_writes_and_edits_cannot_mutate_reserved_native_journal() {
        let (temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let storage = home.join("native-journal-v1");
        std::fs::create_dir(&storage).unwrap();
        let target = storage.join("records.bin");
        std::fs::write(&target, "reserved journal\n").unwrap();
        let write = FileWrite::new(root.clone());
        let edit = FileEdit::new(root.clone());
        let write_args = serde_json::json!({"path":target,"content":"overwrite\n"});
        let edit_args =
            serde_json::json!({"path":target,"old_string":"reserved","new_string":"overwrite"});
        // Freeze before this fixture storage becomes reserved; execution must revalidate, not
        // trust a previously authorized destination or an exposed footprint changed to Global.
        let preparation_home = temp.path().join("other-home");
        let (mut write_call, mut edit_call) =
            crate::core::native_journal::with_test_storage(&preparation_home, || {
                (
                    write.prepare_call(&write_args, &ctx).unwrap(),
                    edit.prepare_call(&edit_args, &ctx).unwrap(),
                )
            });
        write_call.footprint = Some(PreparedFootprint::Global);
        edit_call.footprint = Some(PreparedFootprint::Global);
        crate::core::native_journal::with_test_storage(&home, || {
            assert!(write
                .prepare_call(&write_args, &ctx)
                .unwrap_err()
                .to_string()
                .contains("journal storage"));
            assert!(edit
                .prepare_call(&edit_args, &ctx)
                .unwrap_err()
                .to_string()
                .contains("journal storage"));
            assert!(write
                .execute_prepared(&write_call)
                .unwrap_err()
                .to_string()
                .contains("journal storage"));
            assert!(edit
                .execute_prepared(&edit_call)
                .unwrap_err()
                .to_string()
                .contains("journal storage"));
            assert!(write.prepare_call(&serde_json::json!({
                "path":storage.join("storage.lock"),"content":"forbidden","purpose":"project"
            }), &ctx).is_err());
            let scoped = ctx
                .clone()
                .with_native_scope(
                    crate::core::native_scope::NativeWriteScope::resolve(
                        &root,
                        &[target.to_string_lossy().into_owned()],
                    )
                    .unwrap(),
                )
                .unwrap();
            assert!(write.prepare_call(&write_args, &scoped).is_err());
            assert!(edit.prepare_call(&edit_args, &scoped).is_err());
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "reserved journal\n"
            );
            assert!(!storage.join("storage.lock").exists());
            assert_eq!(std::fs::read_dir(&storage).unwrap().count(), 1);
            assert!(crate::core::read_ledger::observed(&target).is_none());
        });
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_global_write_link_to_reserved_journal_is_refused_without_storage_effects() {
        use std::os::unix::fs::symlink;
        let (temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let storage = home.join("native-journal-v1");
        std::fs::create_dir(&storage).unwrap();
        let target = storage.join("records.bin");
        std::fs::write(&target, "reserved\n").unwrap();
        let alias = root.join("journal-alias");
        symlink(&target, &alias).unwrap();
        let write = FileWrite::new(root.clone());
        let edit = FileEdit::new(root);
        let write_args = serde_json::json!({"path":alias,"content":"forbidden"});
        let edit_args =
            serde_json::json!({"path":alias,"old_string":"reserved","new_string":"forbidden"});
        let preparation_home = temp.path().join("other-home");
        let write_call = crate::core::native_journal::with_test_storage(&preparation_home, || {
            write.prepare_call(&write_args, &ctx).unwrap()
        });
        assert!(matches!(
            write_call.footprint,
            Some(PreparedFootprint::Global)
        ));
        crate::core::native_journal::with_test_storage(&home, || {
            assert!(write.prepare_call(&write_args, &ctx).is_err());
            assert!(edit.prepare_call(&edit_args, &ctx).is_err());
            assert!(write.execute_prepared(&write_call).is_err());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "reserved\n");
            assert!(std::fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(!storage.join("storage.lock").exists());
        });
    }

    #[test]
    fn shell_restore_preflight_denies_nested_ambiguous_and_delegated_before_effects() {
        let parent = ExecutionContext::new(crate::core::convo::ConversationId::new(
            "shell-restore-parent",
        ));
        assert!(shell_restore_preflight("git restore .", Some(&parent)).unwrap());
        assert!(shell_restore_preflight("git reset --hard HEAD", Some(&parent)).unwrap());
        assert!(!shell_restore_preflight("git status", Some(&parent)).unwrap());
        for command in [
            "aizen time restore checkpoint",
            "aizen time undo",
            "git -C elsewhere restore .",
            "git --work-tree=elsewhere reset --hard",
            "git restore . && git status",
            "cd elsewhere; git restore .",
            "git 'restore' .",
        ] {
            assert!(
                shell_restore_preflight(command, Some(&parent)).is_err(),
                "{command}"
            );
        }
        let child = parent.without_workspace_restore();
        assert!(shell_restore_preflight("git restore .", Some(&child)).is_err());
        assert!(!shell_restore_preflight("echo 'git restore .'", Some(&child)).unwrap());
    }

    #[test]
    fn shell_lifetime_retains_exclusion_after_observer_drop_until_tree_and_pipe_settle() {
        struct Owner(std::sync::Arc<AtomicUsize>);
        impl Drop for Owner {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn quarantine(_: Owner) {
            panic!("proven fixture should not quarantine");
        }
        let released = std::sync::Arc::new(AtomicUsize::new(0));
        let observer = std::sync::Arc::new(crate::agent::process::SettlementLifetime::new(
            Owner(released.clone()),
            quarantine,
        ));
        let pipe = observer.drain();
        let tree_waiter = observer.drain();
        let actual_worker = observer.clone();
        drop(observer); // Models async observer select cancellation/drop, not actual process exit.
        assert_eq!(released.load(Ordering::SeqCst), 0);
        actual_worker.prove_tree_settled();
        drop(tree_waiter);
        assert_eq!(
            released.load(Ordering::SeqCst),
            0,
            "unsettled actual pipe keeps admission"
        );
        drop(pipe);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        drop(actual_worker);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shell_lifetime_unknown_settlement_quarantines_instead_of_releasing() {
        static QUARANTINED: AtomicUsize = AtomicUsize::new(0);
        struct Owner(std::sync::Arc<AtomicUsize>);
        impl Drop for Owner {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn quarantine(owner: Owner) {
            QUARANTINED.fetch_add(1, Ordering::SeqCst);
            std::mem::forget(owner); // Test-only generic quarantine; no process/OS lock was created.
        }
        let released = std::sync::Arc::new(AtomicUsize::new(0));
        let lifetime = std::sync::Arc::new(crate::agent::process::SettlementLifetime::new(
            Owner(released.clone()),
            quarantine,
        ));
        let pipe = lifetime.drain();
        drop(lifetime);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        drop(pipe); // EOF alone is deliberately not tree-quiescence proof.
        assert_eq!(QUARANTINED.load(Ordering::SeqCst), 1);
        assert_eq!(released.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn prepared_unscoped_native_bodies_skip_lsp_even_if_visible_footprint_changes() {
        let (temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.rs");
        let body = "fn old() {}\n".repeat(150);
        std::fs::write(&target, &body).unwrap();
        crate::core::workspace_txn::with_test_lock_home(temp.path(), || {
            crate::core::exec_ctx::with_current(ctx.clone(), || {
                let read = FileRead::new(root.clone());
                let mut prepared = read
                    .prepare_call(&serde_json::json!({"path":"f.rs"}), &ctx)
                    .unwrap();
                let resources = match prepared.footprint.as_ref().unwrap() {
                    PreparedFootprint::Native(resources) => resources.clone(),
                    _ => panic!("native read footprint expected"),
                };
                let lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                    &root,
                    &resources,
                    Duration::from_secs(15),
                    None,
                    &ctx.resource_scope(),
                )
                .unwrap();
                prepared.footprint = Some(PreparedFootprint::Global);
                assert_eq!(read.execute_prepared(&prepared).unwrap().text, body);
                drop(lease);
                let write = FileWrite::new(root.clone());
                let mut prepared = write
                    .prepare_call(
                        &serde_json::json!({"path":"f.rs","content":"fn approved() {}\n"}),
                        &ctx,
                    )
                    .unwrap();
                let resources = match prepared.footprint.as_ref().unwrap() {
                    PreparedFootprint::Native(resources) => resources.clone(),
                    _ => panic!("native write footprint expected"),
                };
                let expected = prepared
                    .delegation
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<FrozenFileWrite>()
                    .unwrap()
                    .output
                    .clone();
                let lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                    &root,
                    &resources,
                    Duration::from_secs(15),
                    None,
                    &ctx.resource_scope(),
                )
                .unwrap();
                prepared.footprint = Some(PreparedFootprint::Global);
                assert_eq!(write.execute_prepared(&prepared).unwrap().text, expected);
                drop(lease);
                let edit = FileEdit::new(root.clone());
                let mut prepared = edit.prepare_call(&serde_json::json!({"path":"f.rs","old_string":"approved","new_string":"edited"}), &ctx).unwrap();
                let resources = match prepared.footprint.as_ref().unwrap() {
                    PreparedFootprint::Native(resources) => resources.clone(),
                    _ => panic!("native edit footprint expected"),
                };
                let expected = prepared
                    .delegation
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<FrozenFileEdit>()
                    .unwrap()
                    .output
                    .clone();
                let _lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                    &root,
                    &resources,
                    Duration::from_secs(15),
                    None,
                    &ctx.resource_scope(),
                )
                .unwrap();
                prepared.footprint = Some(PreparedFootprint::Global);
                assert_eq!(edit.execute_prepared(&prepared).unwrap().text, expected);
                assert_eq!(
                    std::fs::read_to_string(&target).unwrap(),
                    "fn edited() {}\n"
                );
            })
        });
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_write_context_cannot_be_reused_under_changed_native_scope_or_owner() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        std::fs::write(&target, "before\n").unwrap();
        std::fs::write(root.join("other.txt"), "other\n").unwrap();
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.txt".into()]).unwrap();
        let scoped = ctx.clone().with_native_scope(scope).unwrap();
        let other_scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["other.txt".into()])
                .unwrap();
        let other_scoped = ctx.clone().with_native_scope(other_scope).unwrap();
        let other_owner = ExecutionContext::new(crate::core::convo::ConversationId::new(
            "test:other-native-owner",
        ))
        .with_placement(ctx.placement().unwrap().clone())
        .with_native_scope(
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.txt".into()]).unwrap(),
        )
        .unwrap();
        let write = FileWrite::new(root.clone());
        let edit = FileEdit::new(root.clone());
        for preparation_ctx in [&ctx, &scoped] {
            let mut write_call = write
                .prepare_call(
                    &serde_json::json!({"path":"f.txt","content":"after\n"}),
                    preparation_ctx,
                )
                .unwrap();
            let mut edit_call = edit
                .prepare_call(
                    &serde_json::json!({"path":"f.txt","old_string":"before","new_string":"after"}),
                    preparation_ctx,
                )
                .unwrap();
            write_call.footprint = Some(PreparedFootprint::Global);
            edit_call.footprint = Some(PreparedFootprint::Global);
            let ambient_contexts = if preparation_ctx.native_scope().is_some() {
                vec![&ctx, &other_scoped, &other_owner]
            } else {
                vec![&scoped]
            };
            for ambient in &ambient_contexts {
                for result in [
                    crate::core::exec_ctx::with_current((*ambient).clone(), || {
                        write.execute_prepared(&write_call)
                    }),
                    crate::core::exec_ctx::with_current((*ambient).clone(), || {
                        edit.execute_prepared(&edit_call)
                    }),
                ] {
                    assert!(result.unwrap_err().to_string().contains("scope"));
                }
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "before\n");
            }
            if preparation_ctx.native_scope().is_some() {
                assert!(write.execute_prepared(&write_call).is_err());
                assert!(edit.execute_prepared(&edit_call).is_err());
            }
        }
        assert!(crate::core::read_ledger::observed(&target).is_none());
    }

    #[test]
    fn native_scope_read_only_enforces_all_entries_and_frozen_modes() {
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        std::fs::write(root.join("allowed.txt"), "owned read\n").unwrap();
        std::fs::write(root.join("outside.txt"), "private outside\n").unwrap();
        let scope = crate::core::native_scope::NativeWriteScope::resolve_access(
            &root,
            &["allowed.txt".into()],
            &[],
        )
        .unwrap();
        let read_ctx = ctx.clone().with_native_scope(scope).unwrap();
        let write_ctx = ctx
            .clone()
            .with_native_scope(
                crate::core::native_scope::NativeWriteScope::resolve(
                    &root,
                    &["allowed.txt".into()],
                )
                .unwrap(),
            )
            .unwrap();
        assert!(read_ctx
            .with_native_scope(write_ctx.native_scope().unwrap().clone())
            .is_err());
        let read = FileRead::new(root.clone());
        assert!(read
            .prepare_call(&serde_json::json!({"path":"outside.txt"}), &read_ctx)
            .is_err());
        let mut files = vec![serde_json::json!({"path":"allowed.txt"}); 8];
        files.push(serde_json::json!({"path":"outside.txt"}));
        assert!(
            read.prepare_call(&serde_json::json!({"files":files}), &read_ctx)
                .is_err(),
            "even the omitted ninth entry must be authorized"
        );
        assert!(FileWrite::new(root.clone())
            .prepare_call(
                &serde_json::json!({"path":"allowed.txt","content":"changed"}),
                &read_ctx
            )
            .is_err());
        assert!(FileEdit::new(root.clone()).prepare_call(&serde_json::json!({"path":"allowed.txt","old_string":"owned","new_string":"changed"}), &read_ctx).is_err());
        let call = read
            .prepare_call(&serde_json::json!({"path":"allowed.txt"}), &read_ctx)
            .unwrap();
        assert!(
            crate::core::exec_ctx::with_current(write_ctx, || read.execute_prepared(&call))
                .is_err(),
            "same file union is not the same authority"
        );
        let result =
            crate::core::exec_ctx::with_current(read_ctx, || read.execute_prepared(&call)).unwrap();
        assert!(result.text.contains("owned read"));
        assert!(!result.text.contains("private outside"));
        assert_eq!(
            std::fs::read_to_string(root.join("allowed.txt")).unwrap(),
            "owned read\n"
        );
    }

    #[test]
    fn prepared_scoped_file_writes_apply_exact_frozen_bytes_via_journal() {
        let (temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.rs");
        std::fs::write(&target, "fn old() {}\n").unwrap();
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.rs".into()]).unwrap();
        let ctx = ctx.with_native_scope(scope).unwrap();
        crate::core::workspace_txn::with_test_lock_home(temp.path(), || {
            crate::core::native_journal::with_test_storage(temp.path(), || {
                crate::core::exec_ctx::with_current(ctx.clone(), || {
                    let write = FileWrite::new(root.clone());
                    let mut prepared = write
                        .prepare_call(
                            &serde_json::json!({"path":"f.rs","content":"fn approved() {}\n"}),
                            &ctx,
                        )
                        .unwrap();
                    let resources = match prepared.footprint.as_ref().unwrap() {
                        PreparedFootprint::Native(resources) => resources.clone(),
                        _ => panic!("exact resource required"),
                    };
                    let lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                        &root,
                        &resources,
                        Duration::from_secs(15),
                        None,
                        &ctx.resource_scope(),
                    )
                    .unwrap();
                    let frozen_output = prepared
                        .delegation
                        .as_ref()
                        .unwrap()
                        .downcast_ref::<FrozenFileWrite>()
                        .unwrap()
                        .output
                        .clone();
                    prepared.args["content"] = serde_json::json!("unapproved");
                    assert_eq!(
                        write.execute_prepared(&prepared).unwrap().text,
                        frozen_output
                    );
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "fn approved() {}\n"
                    );
                    drop(lease);
                    let edit = FileEdit::new(root.clone());
                    let mut prepared = edit.prepare_call(&serde_json::json!({"path":"f.rs","old_string":"approved","new_string":"edited"}), &ctx).unwrap();
                    let resources = match prepared.footprint.as_ref().unwrap() {
                        PreparedFootprint::Native(resources) => resources.clone(),
                        _ => panic!("exact resource required"),
                    };
                    let lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                        &root,
                        &resources,
                        Duration::from_secs(15),
                        None,
                        &ctx.resource_scope(),
                    )
                    .unwrap();
                    let frozen_output = prepared
                        .delegation
                        .as_ref()
                        .unwrap()
                        .downcast_ref::<FrozenFileEdit>()
                        .unwrap()
                        .output
                        .clone();
                    prepared.args["new_string"] = serde_json::json!("unapproved");
                    // Exact frozen output proves no post-write LSP feedback was appended in this lane.
                    assert_eq!(
                        edit.execute_prepared(&prepared).unwrap().text,
                        frozen_output
                    );
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "fn edited() {}\n"
                    );
                    assert_eq!(
                        crate::core::read_ledger::observed(&target),
                        Some(crate::core::persist::FileFingerprint::for_bytes(
                            b"fn edited() {}\n"
                        ))
                    );
                    // Caller retains the same exclusive resource lease during reconciliation.
                    assert!(!crate::core::native_journal::reconcile(&resources[0].0)
                        .unwrap()
                        .iter()
                        .any(|entry| matches!(
                            entry,
                            crate::core::native_journal::Reconciliation::Conflict { .. }
                        )));
                    drop(lease);
                })
            })
        });
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_scoped_write_after_apply_failure_retains_journal_and_prior_ledger() {
        let (temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        let target = root.join("f.txt");
        std::fs::write(&target, "before\n").unwrap();
        let observed = crate::core::persist::FileFingerprint::for_bytes(b"before\n");
        crate::core::read_ledger::note(&target, &observed);
        let scope =
            crate::core::native_scope::NativeWriteScope::resolve(&root, &["f.txt".into()]).unwrap();
        let ctx = ctx.with_native_scope(scope).unwrap();
        crate::core::workspace_txn::with_test_lock_home(temp.path(), || {
            crate::core::native_journal::with_test_storage(temp.path(), || {
                crate::core::exec_ctx::with_current(ctx.clone(), || {
                    let write = FileWrite::new(root.clone());
                    let prepared = write
                        .prepare_call(
                            &serde_json::json!({"path":"f.txt","content":"after\n"}),
                            &ctx,
                        )
                        .unwrap();
                    let resources = match prepared.footprint.as_ref().unwrap() {
                        PreparedFootprint::Native(resources) => resources.clone(),
                        _ => panic!("exact resource required"),
                    };
                    let _lease = crate::core::workspace_txn::NativeResourceLease::acquire(
                        &root,
                        &resources,
                        Duration::from_secs(15),
                        None,
                        &ctx.resource_scope(),
                    )
                    .unwrap();
                    let error = crate::core::native_journal::with_test_after_write_failure(|| {
                        write.execute_prepared(&prepared)
                    })
                    .unwrap_err();
                    assert!(error
                        .downcast_ref::<crate::core::native_journal::PossiblyApplied>()
                        .is_some());
                    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
                    // A possibly-applied tool does not report successful fresh observation.
                    assert_eq!(
                        crate::core::read_ledger::observed(&target),
                        Some(observed.clone())
                    );
                    let reconciled =
                        crate::core::native_journal::reconcile(&resources[0].0).unwrap();
                    assert!(reconciled.iter().any(|entry| matches!(
                        entry,
                        crate::core::native_journal::Reconciliation::After { .. }
                    )));
                    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
                    assert!(write.execute_prepared(&prepared).is_err());
                })
            })
        });
        crate::core::read_ledger::forget(&target);
    }

    #[cfg(unix)]
    #[test]
    fn prepared_file_read_alias_retarget_cannot_widen_read_path() {
        use std::os::unix::fs::symlink;
        let (_temp, ctx) = native_preparation_fixture();
        let root = ctx.placement().unwrap().workspace_root().to_path_buf();
        std::fs::write(root.join("a.txt"), "approved").unwrap();
        std::fs::write(root.join("b.txt"), "unapproved").unwrap();
        symlink(root.join("a.txt"), root.join("alias.txt")).unwrap();
        let read = FileRead::new(root.clone());
        let prepared = read
            .prepare_call(&serde_json::json!({"path":"alias.txt"}), &ctx)
            .unwrap();
        std::fs::remove_file(root.join("alias.txt")).unwrap();
        symlink(root.join("b.txt"), root.join("alias.txt")).unwrap();
        assert!(read.execute_prepared(&prepared).is_err());
        assert!(crate::core::read_ledger::observed(&root.join("a.txt")).is_none());
        assert!(crate::core::read_ledger::observed(&root.join("b.txt")).is_none());
    }

    #[test]
    fn prepared_file_edit_freezes_single_and_batch_bytes_and_preview() {
        let root = temp_root("fe-prepared-bytes");
        let ctx = placement_fixture(&root);
        let edit = FileEdit::new(root.clone());
        let target = root.join("f.txt");
        for args in [
            serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"ALPHA"}),
            serde_json::json!({"path":"f.txt","edits":[
                {"old_string":"alpha","new_string":"A"},
                {"old_string":"A","new_string":"ALPHA"}
            ]}),
        ] {
            std::fs::write(&target, "alpha beta\n").unwrap();
            crate::core::read_ledger::forget(&target);
            let mut prepared = edit.prepare_call(&args, &ctx).unwrap();
            let preview = prepared.preview.as_ref().unwrap().clone();
            let frozen = prepared
                .delegation
                .as_ref()
                .unwrap()
                .downcast_ref::<FrozenFileEdit>()
                .unwrap();
            let expected_output = frozen.output.clone();
            assert_eq!(frozen.content, "ALPHA beta\n");
            assert!(preview.diff.as_deref().unwrap().contains("ALPHA"));
            // Original arguments are not the authority once approved replacement bytes are frozen.
            prepared.args["path"] = serde_json::json!(root.join("unapproved.txt"));
            prepared.args["new_string"] = serde_json::json!("UNAPPROVED");
            prepared.args["edits"] =
                serde_json::json!([{"old_string":"beta","new_string":"UNAPPROVED"}]);
            prepared.args["dry_run"] = serde_json::json!(true);
            let output = edit.execute_prepared(&prepared).unwrap();
            assert!(output.text.starts_with(&expected_output));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "ALPHA beta\n");
            assert!(!root.join("unapproved.txt").exists());
            assert_eq!(prepared.preview.as_ref().unwrap().diff, preview.diff);
        }
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_edit_refuses_drift_even_when_old_text_still_matches() {
        let root = temp_root("fe-prepared-drift");
        let ctx = placement_fixture(&root);
        let edit = FileEdit::new(root.clone());
        let target = root.join("f.txt");
        for args in [
            serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"ALPHA"}),
            serde_json::json!({"path":"f.txt","edits":[{"old_string":"alpha","new_string":"ALPHA"}]}),
        ] {
            std::fs::write(&target, "alpha beta\n").unwrap();
            let prepared = edit.prepare_call(&args, &ctx).unwrap();
            let preview = prepared.preview.as_ref().unwrap().clone();
            let expected = prepared.file.as_ref().unwrap().expected.clone();
            std::fs::write(&target, "alpha peer\n").unwrap();
            // Even refreshing the session ledger cannot authorize re-matching a stale approval.
            FileRead::new(root.clone())
                .execute(&serde_json::json!({"path":"f.txt"}))
                .unwrap();
            assert!(edit.execute_prepared(&prepared).is_err());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "alpha peer\n");
            assert_eq!(prepared.preview.as_ref().unwrap().diff, preview.diff);
            assert_eq!(prepared.file.as_ref().unwrap().expected, expected);
        }
        std::fs::remove_file(&target).unwrap();
        assert!(edit
            .prepare_call(
                &serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"A"}),
                &ctx
            )
            .is_err());
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_edit_rechecks_read_ledger_without_refreshing_it_in_preview() {
        let root = temp_root("fe-prepared-ledger");
        let ctx = placement_fixture(&root);
        let edit = FileEdit::new(root.clone());
        let target = root.join("f.txt");
        std::fs::write(&target, "alpha original\n").unwrap();
        FileRead::new(root.clone())
            .execute(&serde_json::json!({"path":"f.txt"}))
            .unwrap();
        let seen = crate::core::read_ledger::observed(&target).unwrap();
        std::fs::write(&target, "alpha peer\n").unwrap();
        let args = serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"ALPHA"});
        let prepared = edit.prepare_call(&args, &ctx).unwrap();
        assert_eq!(crate::core::read_ledger::observed(&target), Some(seen));
        let error = edit.execute_prepared(&prepared).unwrap_err();
        assert!(format!("{error:#}").contains("overwrite conflict"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "alpha peer\n");
        FileRead::new(root.clone())
            .execute(&serde_json::json!({"path":"f.txt"}))
            .unwrap();
        edit.execute_prepared(&prepared).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ALPHA peer\n");
        assert_eq!(
            crate::core::read_ledger::observed(&target),
            Some(crate::core::persist::FileFingerprint::for_bytes(
                b"ALPHA peer\n"
            ))
        );
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_edit_rechecks_ledger_at_execution_after_successful_preparation() {
        let root = temp_root("fe-prepared-ledger-after");
        let ctx = placement_fixture(&root);
        let target = root.join("f.txt");
        std::fs::write(&target, "alpha\n").unwrap();
        FileRead::new(root.clone())
            .execute(&serde_json::json!({"path":"f.txt"}))
            .unwrap();
        let edit = FileEdit::new(root.clone());
        let prepared = edit
            .prepare_call(
                &serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"A"}),
                &ctx,
            )
            .unwrap();
        crate::core::read_ledger::note(
            &target,
            &crate::core::persist::FileFingerprint::for_bytes(b"different observation"),
        );
        assert!(edit
            .execute_prepared(&prepared)
            .unwrap_err()
            .to_string()
            .contains("overwrite conflict"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "alpha\n");
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_edit_noop_and_dry_run_do_not_write_or_refresh_ledger() {
        let root = temp_root("fe-prepared-noop");
        let ctx = placement_fixture(&root);
        let target = root.join("f.txt");
        std::fs::write(&target, "alpha beta\n").unwrap();
        let edit = FileEdit::new(root.clone());
        let modified = std::fs::metadata(&target).unwrap().modified().unwrap();
        let seen = crate::core::persist::FileFingerprint::for_bytes(b"prior observation");
        crate::core::read_ledger::note(&target, &seen);
        for (args, prefix) in [
            (
                serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"alpha"}),
                NOOP_WRITE_PREFIX,
            ),
            (
                serde_json::json!({"path":"f.txt","edits":[{"old_string":"alpha","new_string":"A"},{"old_string":"A","new_string":"alpha"}]}),
                NOOP_WRITE_PREFIX,
            ),
            (
                serde_json::json!({"path":"f.txt","old_string":"alpha","new_string":"A","dry_run":true}),
                DRY_RUN_PREFIX,
            ),
            (
                serde_json::json!({"path":"f.txt","edits":[{"old_string":"alpha","new_string":"A"}],"dry_run":true}),
                DRY_RUN_PREFIX,
            ),
        ] {
            let prepared = edit.prepare_call(&args, &ctx).unwrap();
            assert_eq!(
                edit.workspace_effect_prepared(&prepared),
                WorkspaceEffect::None
            );
            let output = edit.execute_prepared(&prepared).unwrap();
            assert!(output.text.starts_with(prefix));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "alpha beta\n");
            assert_eq!(
                std::fs::metadata(&target).unwrap().modified().unwrap(),
                modified
            );
            assert_eq!(
                crate::core::read_ledger::observed(&target),
                Some(seen.clone())
            );
        }
        crate::core::read_ledger::forget(&target);
    }

    #[test]
    fn prepared_file_edit_bounds_input_and_replace_all_expansion_before_write() {
        let root = temp_root("fe-prepared-size");
        let ctx = placement_fixture(&root);
        let target = root.join("f.txt");
        let edit = FileEdit::new(root.clone());
        std::fs::write(&target, vec![b'a'; PREPARED_EDIT_MAX_BYTES + 1]).unwrap();
        let args = serde_json::json!({"path":"f.txt","old_string":"a","new_string":"b","replace_all":true});
        assert!(edit
            .prepare_call(&args, &ctx)
            .unwrap_err()
            .to_string()
            .contains("4 MiB"));
        assert_eq!(
            std::fs::metadata(&target).unwrap().len(),
            PREPARED_EDIT_MAX_BYTES as u64 + 1
        );
        std::fs::write(&target, "aa").unwrap();
        let args = serde_json::json!({"path":"f.txt","old_string":"a","new_string":"b".repeat(PREPARED_EDIT_MAX_BYTES / 2 + 1),"replace_all":true});
        assert!(edit
            .prepare_call(&args, &ctx)
            .unwrap_err()
            .to_string()
            .contains("4 MiB"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "aa");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn prepared_file_edit_existing_managed_absolute_paths_keep_external_provenance() {
        let root = temp_root("fe-prepared-managed");
        let workspace = root.canonicalize().unwrap();
        let placement = crate::core::file_placement::PlacementContext::with_paths(
            workspace.clone(),
            workspace.join("owned-home"),
            workspace.join("owned-scratch"),
            "edit-managed",
        )
        .unwrap();
        let ctx =
            ExecutionContext::new(crate::core::convo::ConversationId::new("test:edit-managed"))
                .with_placement(placement);
        let write = FileWrite::new(root.clone());
        let created = write.prepare_call(&serde_json::json!({"path":"existing.txt","content":"alpha\n","purpose":"artifact"}), &ctx).unwrap();
        write.execute_prepared(&created).unwrap();
        let actual = created
            .file
            .as_ref()
            .unwrap()
            .destination
            .actual_path()
            .to_path_buf();
        let edit = FileEdit::new(root.clone());
        let prepared = edit
            .prepare_call(
                &serde_json::json!({"path":actual,"old_string":"alpha","new_string":"ALPHA"}),
                &ctx,
            )
            .unwrap();
        assert!(prepared.file.as_ref().unwrap().source_managed);
        assert_eq!(
            edit.workspace_effect_prepared(&prepared),
            WorkspaceEffect::External
        );
        assert_eq!(
            edit.workspace_target_prepared(&prepared),
            actual.parent().map(Path::to_path_buf)
        );
        edit.execute_prepared(&prepared).unwrap();
        assert_eq!(std::fs::read_to_string(&actual).unwrap(), "ALPHA\n");
        assert!(!root.join("existing.txt").exists());
        crate::core::read_ledger::forget(&actual);
    }

    #[test]
    fn prepared_file_edit_rejects_same_bytes_retargeted_ancestor_alias() {
        let root = temp_root("fe-prepared-alias");
        let ctx = placement_fixture(&root);
        let parent = root.join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("f.txt"), "alpha\n").unwrap();
        let edit = FileEdit::new(root.clone());
        let prepared = edit
            .prepare_call(
                &serde_json::json!({"path":"parent/f.txt","old_string":"alpha","new_string":"A"}),
                &ctx,
            )
            .unwrap();
        std::fs::rename(&parent, root.join("original-parent")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("f.txt"), "alpha\n").unwrap();
        assert!(edit.execute_prepared(&prepared).is_err());
        assert_eq!(
            std::fs::read_to_string(parent.join("f.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("original-parent/f.txt")).unwrap(),
            "alpha\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepared_file_edit_rejects_symlink_alias_retargeting_without_touching_either_target() {
        let root = temp_root("fe-prepared-symlink");
        let ctx = placement_fixture(&root);
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        std::fs::write(&a, "alpha\n").unwrap();
        std::fs::write(&b, "alpha\n").unwrap();
        let alias = root.join("alias.txt");
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        let edit = FileEdit::new(root.clone());
        let prepared = edit
            .prepare_call(
                &serde_json::json!({"path":"alias.txt","old_string":"alpha","new_string":"A"}),
                &ctx,
            )
            .unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&b, &alias).unwrap();
        assert!(edit.execute_prepared(&prepared).is_err());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "alpha\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "alpha\n");
    }

    #[test]
    fn file_write_noop_on_identical_content_does_not_touch_disk() {
        // W16: overwriting a file with the bytes it already holds must not rewrite it (no mtime
        // churn) and must return the no-op marker (so the loop won't arm the verify gate).
        let root = temp_root("fw-noop");
        let p = root.join("f.txt");
        std::fs::write(&p, "same\n").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t = FileWrite::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path": "f.txt", "content": "same\n"}))
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before,
            "no-op overwrite must not rewrite the file"
        );
    }

    #[test]
    fn file_write_refuses_to_overwrite_what_changed_after_this_session_read_it() {
        // The cross-session clobber `compare_and_atomic_write` cannot see: its fingerprint is taken
        // microseconds before the write, so a peer window's rewrite passes the CAS and vanishes. The
        // read ledger is the anchor — read, let someone else rewrite, and the full overwrite refuses.
        let root = temp_root("fw-stale");
        let p = root.join("shared.rs");
        std::fs::write(&p, "fn a() {}\n").unwrap();
        FileRead::new(root.clone())
            .execute(&serde_json::json!({"path": "shared.rs"}))
            .unwrap();
        // Another window (or an external editor) rewrites it while we were thinking.
        std::fs::write(&p, "fn a() {}\nfn peer_work() {}\n").unwrap();
        let err = FileWrite::new(root.clone())
            .execute(
                &serde_json::json!({"path": "shared.rs", "content": "fn a() { /* mine */ }\n"}),
            )
            .expect_err("a stale whole-file overwrite must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("overwrite conflict"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn a() {}\nfn peer_work() {}\n",
            "the peer's work must still be on disk"
        );
        // Re-reading is the documented recovery: now the session knows what is there, and the same
        // write goes through.
        FileRead::new(root.clone())
            .execute(&serde_json::json!({"path": "shared.rs"}))
            .unwrap();
        FileWrite::new(root.clone())
            .execute(&serde_json::json!({"path": "shared.rs", "content": "merged\n"}))
            .expect("after re-reading, the overwrite is allowed");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "merged\n");
        crate::core::read_ledger::forget(&p);
    }

    #[test]
    fn a_sessions_own_edit_does_not_make_its_next_write_look_like_interference() {
        // The write tools update the ledger after a successful write: the bytes we just wrote ARE
        // what this session now knows to be there. Without that, a session's second write to a file
        // it edited itself would be refused as a conflict with its own work.
        let root = temp_root("fw-selfedit");
        let p = root.join("mine.rs");
        std::fs::write(&p, "one\n").unwrap();
        FileWrite::new(root.clone())
            .execute(&serde_json::json!({"path": "mine.rs", "content": "two\n"}))
            .expect("first write: never read, nothing claims it");
        FileEdit::new(root.clone())
            .execute(
                &serde_json::json!({"path": "mine.rs", "old_string": "two", "new_string": "three"}),
            )
            .expect("edit against a fresh read always works");
        FileWrite::new(root.clone())
            .execute(&serde_json::json!({"path": "mine.rs", "content": "four\n"}))
            .expect("our own edit must not read as someone else's interference");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "four\n");
        crate::core::read_ledger::forget(&p);
    }

    #[test]
    fn file_write_empty_new_file_is_a_real_op() {
        // A create with empty content is a genuine op (the file did not exist) — not a no-op.
        let root = temp_root("fw-newempty");
        let t = FileWrite::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path": "new.txt", "content": "", "purpose": "project"}))
            .unwrap();
        assert!(
            !r.starts_with(NOOP_WRITE_PREFIX),
            "creating a new file is never a no-op: {r}"
        );
        assert!(root.join("new.txt").exists());
    }

    #[test]
    fn file_edit_noop_when_old_equals_new() {
        // W16: old_string == new_string produces byte-identical content → no write, no-op marker.
        let root = temp_root("fe-noop");
        let p = root.join("f.txt");
        std::fs::write(&p, "alpha beta\n").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(
                &serde_json::json!({"path": "f.txt", "old_string": "beta", "new_string": "beta"}),
            )
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before
        );
    }

    #[test]
    fn multi_edit_noop_when_edits_net_to_original() {
        // W16: two edits that cancel out (X→Y then Y→X) net to the original → no write, no-op marker.
        let root = temp_root("me-noop");
        let p = root.join("f.txt");
        std::fs::write(&p, "one two three\n").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "two", "new_string": "TWO"},
                    {"old_string": "TWO", "new_string": "two"}
                ]
            }))
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one two three\n");
    }

    #[test]
    fn file_move_renames_a_file() {
        let root = temp_root("fmv-rename");
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "b.txt", "purpose": "project"}))
            .unwrap();
        assert!(r.starts_with("moved file"), "got: {r}");
        assert!(!root.join("a.txt").exists(), "source is gone after a move");
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn file_move_errors_on_missing_source() {
        let root = temp_root("fmv-missing");
        let t = FileMove::new(root.clone());
        let err = t
            .execute(&serde_json::json!({"from": "nope.txt", "to": "x.txt"}))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("nope.txt"),
            "error names the source: {err:#}"
        );
    }

    #[test]
    fn file_move_wont_clobber_without_overwrite() {
        let root = temp_root("fmv-clobber");
        std::fs::write(root.join("a.txt"), "A\n").unwrap();
        std::fs::write(root.join("b.txt"), "B\n").unwrap();
        let t = FileMove::new(root.clone());
        // Default: refuse to overwrite an existing destination.
        let err = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "b.txt", "purpose": "project"}))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("already exists"),
            "got: {err:#}"
        );
        // Both files must be untouched after the refusal.
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "A\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");
    }

    #[test]
    fn file_move_overwrite_replaces_destination() {
        let root = temp_root("fmv-overwrite");
        std::fs::write(root.join("a.txt"), "A\n").unwrap();
        std::fs::write(root.join("b.txt"), "B\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "b.txt", "overwrite": true}))
            .unwrap();
        assert!(r.starts_with("moved file"), "got: {r}");
        assert!(!root.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "A\n",
            "dest now holds source content"
        );
    }

    #[test]
    fn file_move_create_dirs_makes_parents() {
        let root = temp_root("fmv-mkdirs");
        std::fs::write(root.join("a.txt"), "hi\n").unwrap();
        let t = FileMove::new(root.clone());
        // Destination parent (`nested/deep/`) does not exist yet.
        let no_parent = t.execute(
            &serde_json::json!({"from": "a.txt", "to": "nested/deep/b.txt", "purpose": "project"}),
        );
        assert!(
            no_parent.is_err(),
            "missing parent errors without create_dirs"
        );
        // With create_dirs it makes the parents and moves.
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "nested/deep/b.txt", "create_dirs": true, "purpose": "project"}))
            .unwrap();
        assert!(r.starts_with("moved file"), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("nested/deep/b.txt")).unwrap(),
            "hi\n"
        );
    }

    #[test]
    fn file_move_moves_a_directory() {
        let root = temp_root("fmv-dir");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/f.txt"), "x\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "src", "to": "dst", "purpose": "project"}))
            .unwrap();
        assert!(r.starts_with("moved directory"), "got: {r}");
        assert!(!root.join("src").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("dst/f.txt")).unwrap(),
            "x\n"
        );
    }

    #[test]
    fn file_move_same_path_is_a_noop() {
        let root = temp_root("fmv-noop");
        std::fs::write(root.join("a.txt"), "keep\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "a.txt"}))
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn file_move_is_destructive_and_arms_verify_gate() {
        // The verify gate arms on is_destructive() (not a tool-name list), so file_move must report
        // destructive to be verified like file_edit/file_write.
        assert!(FileMove::new(PathBuf::from(".")).is_destructive());
        assert!(!FileMove::new(PathBuf::from(".")).is_concurrency_safe());
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        use crate::core::persist::atomic_write;
        let root = temp_root("atomic-write");
        let target = root.join("f.txt");
        // Fresh create.
        atomic_write(&target, b"first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\n");
        // Overwrite.
        atomic_write(&target, b"second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second\n");
        // No sibling temp file survives (the rename consumed it, no failure path leaked one).
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".aizen-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use crate::core::persist::atomic_write;
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("atomic-perms");
        let target = root.join("script.sh");
        atomic_write(&target, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A rewrite must carry the executable bit over (temp files are created 0644 by default).
        atomic_write(&target, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "rewrite reset the mode: {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn file_write_is_atomic_and_leaves_no_temp() {
        // The file_write tool goes through atomic_write; a successful overwrite must leave the target
        // holding exactly the new bytes and no staging temp behind.
        let root = temp_root("fw-atomic");
        std::fs::write(root.join("a.txt"), "old\n").unwrap();
        let t = FileWrite::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path": "a.txt", "content": "new\n"}))
            .unwrap();
        assert!(r.starts_with("overwrote"), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "new\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.contains(".aizen-tmp.") || n.contains(".aizen-stash.")
            })
            .collect();
        assert!(leftovers.is_empty(), "staging file leaked: {leftovers:?}");
    }

    #[test]
    fn file_move_overwrite_leaves_no_stash_behind() {
        // The overwrite path stashes the old destination out of the way, moves, then deletes the
        // stash. On success no `.aizen-stash.` sibling may survive.
        let root = temp_root("fmv-stash");
        std::fs::write(root.join("a.txt"), "A\n").unwrap();
        std::fs::write(root.join("b.txt"), "B\n").unwrap();
        let t = FileMove::new(root.clone());
        t.execute(&serde_json::json!({"from": "a.txt", "to": "b.txt", "overwrite": true}))
            .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "A\n");
        assert!(!root.join("a.txt").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".aizen-stash."))
            .collect();
        assert!(leftovers.is_empty(), "stash file leaked: {leftovers:?}");
    }

    #[test]
    fn file_move_overwrite_dir_replaces_contents() {
        // Overwriting a directory must leave the destination holding the SOURCE tree, not a merge
        // with the old destination's files (the stash removes the old dst entirely before the move).
        let root = temp_root("fmv-dir-overwrite");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/new.txt"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("dst")).unwrap();
        std::fs::write(root.join("dst/old.txt"), "old\n").unwrap();
        let t = FileMove::new(root.clone());
        t.execute(&serde_json::json!({"from": "src", "to": "dst", "overwrite": true, "purpose": "project"}))
            .unwrap();
        assert!(
            root.join("dst/new.txt").exists(),
            "dst holds the source tree"
        );
        assert!(
            !root.join("dst/old.txt").exists(),
            "old dst contents are gone, not merged"
        );
        assert!(!root.join("src").exists(), "source consumed by the move");
    }

    #[test]
    fn shell_run_executes() {
        let root = temp_root("shell");
        let t = ShellRun::new(root);
        let out = t
            .execute(&serde_json::json!({"command":"echo hello-ng"}))
            .unwrap();
        assert!(out.contains("hello-ng"));
        assert!(out.starts_with("exit 0"));
    }

    #[test]
    fn shell_run_timeout_returns_promptly_even_with_a_surviving_grandchild() {
        // The end-to-end form of the hang: a command whose real work lives in a grandchild used to
        // time out, kill only the `cmd.exe` wrapper, and then block FOREVER in `oh.join()` because
        // the orphan still held the pipe's write end. The tool must come back on its own.
        //
        // Timing is the assertion. A 10s cap (the clamp floor) plus the 2s drain grace bounds the
        // honest worst case well under the 40s the grandchild would otherwise sleep, so a regression
        // shows up as a failure rather than a suite that never finishes.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SHELL_TIMEOUT_ENV, "10");
        let command = if cfg!(windows) {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(
                r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -Command Start-Sleep -Seconds 40"
            )
        } else {
            "sleep 40 & wait".to_string()
        };
        let t = ShellRun::new(temp_root("shell-timeout"));
        let started = Instant::now();
        let out = t.execute(&serde_json::json!({"command": command})).unwrap();
        let elapsed = started.elapsed();
        std::env::remove_var(SHELL_TIMEOUT_ENV);

        assert!(
            out.starts_with("error: command timed out"),
            "should report a timeout; got: {out}"
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "shell_run took {elapsed:?} for a 10s cap — it is waiting on a pipe an orphan still holds"
        );
    }

    #[test]
    fn shell_timeout_env_is_clamped_to_a_sane_band() {
        // A cap that can be set to 0 (or to a week) is not a cap. The band keeps the agent loop
        // responsive no matter what the environment says.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SHELL_TIMEOUT_ENV, "0");
        assert_eq!(shell_timeout_secs(), 10, "0 clamps up to the floor");
        std::env::set_var(SHELL_TIMEOUT_ENV, "999999");
        assert_eq!(
            shell_timeout_secs(),
            3600,
            "absurd values clamp to the ceiling"
        );
        std::env::set_var(SHELL_TIMEOUT_ENV, "600");
        assert_eq!(
            shell_timeout_secs(),
            600,
            "a reasonable value passes through"
        );
        std::env::set_var(SHELL_TIMEOUT_ENV, "not-a-number");
        assert_eq!(
            shell_timeout_secs(),
            SHELL_TIMEOUT_SECS,
            "garbage falls back to the default"
        );
        std::env::remove_var(SHELL_TIMEOUT_ENV);
        assert_eq!(shell_timeout_secs(), SHELL_TIMEOUT_SECS, "unset = default");
    }

    #[test]
    fn drain_keeps_output_on_invalid_utf8() {
        // Regression: `read_to_string` returns Err and leaves the buffer EMPTY on the first
        // invalid-UTF-8 byte, so non-English Windows `dir` output (OEM codepage) was dropped
        // wholesale and the agent saw a blank result. Lossy decode must keep the ASCII structure.
        let bytes = b"Directory of C:\\\nfile-\xe9\xff.txt\n2 files".to_vec();
        let got = drain(Some(std::io::Cursor::new(bytes)));
        assert!(got.contains("Directory of"), "ASCII structure preserved");
        assert!(got.contains("file-"));
        assert!(got.contains("2 files"));
        assert!(
            got.contains('\u{fffd}'),
            "bad bytes degrade to the replacement char, not loss"
        );
    }

    #[test]
    fn persona_create_writes_and_activates() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-persona-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);

        let t = PersonaCreate;
        assert!(t.is_destructive(), "writes to ~/.aizen → approval-gated");

        // default activate=true → minted AND becomes active
        let out = t
            .execute(&serde_json::json!({
                "name": "Sherlock",
                "role": "a sharp consulting detective",
                "voice": "clipped, deductive",
                "body": "You reason from observation. You are blunt but never cruel."
            }))
            .unwrap();
        assert!(out.contains("created persona 'Sherlock'"));
        assert!(out.contains("switched to it"));
        assert_eq!(
            crate::core::cli_config::load().persona.as_deref(),
            Some("Sherlock")
        );
        let p = crate::persona::load("sherlock").expect("persona card written");
        assert_eq!(p.role, "a sharp consulting detective");

        // activate=false → authored for later, active persona unchanged
        let out2 = t
            .execute(
                &serde_json::json!({"name":"Watson","body":"A loyal chronicler.","activate":false}),
            )
            .unwrap();
        assert!(out2.contains("not active"));
        assert_eq!(
            crate::core::cli_config::load().persona.as_deref(),
            Some("Sherlock"),
            "still Sherlock"
        );

        // missing required body → error (no half-formed card)
        assert!(t.execute(&serde_json::json!({"name":"Empty"})).is_err());

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_subagent_scope_gets_a_scratch_plan_but_never_the_top_level_todo() {
        // W17: a sub-agent can track its own multi-step plan (todo_write present in every scope via
        // the read-only base) — but it's the per-instance ScopedTodo, NOT the process-global
        // TodoWrite (which is top-level only, sharing the user's list + scroll region).
        let root = std::env::temp_dir();
        for role in [
            "daedalus",
            "argus",
            "metis",
            "nemesis",
            "themis",
            "clio",
            "mnemosyne",
            "coder",
            "tester",
            "planner",
            "reviewer",
            "unknown-role",
        ] {
            let r = role_registry(role, &root);
            assert!(
                r.get("todo_write").is_some(),
                "role {role} has a scratch plan"
            );
        }
        // A specialist (empty tools = coder scope) also gets it.
        let spec = agent_registry(
            &crate::agents::AgentDef {
                name: "S".into(),
                description: String::new(),
                color: String::new(),
                emoji: String::new(),
                vibe: String::new(),
                tools: vec![],
                model: None,
                base_url: None,
                api_key_ref: None,
                body: "b".into(),
                division: None,
                source: crate::agents::AgentSource::AizenHome,
                source_path: std::path::PathBuf::new(),
            },
            &root,
        );
        assert!(
            spec.get("todo_write").is_some(),
            "specialist has a scratch plan"
        );
        // The scratch plan is a scratch plan — writing to it never touches the process-global list
        // (proven per-instance in todo::tests; here we only assert presence + read-only classification).
        assert!(
            crate::agent::task_tool::dispatch_is_read_only(&role_registry("planner", &root)),
            "todo_write does not make a read-only role a writer (it's not workspace mutation)"
        );
    }

    #[test]
    fn shell_capable_roles_get_the_scoped_process_pool() {
        // `shell_run`'s timeout message tells a child to re-run a long command with `process`. That
        // advice was unreachable while the tool was top-level only, so the child could only re-run
        // the command that had just timed out. The pool is execution-scope keyed, so granting it here
        // does not let one child observe or kill a sibling's handles.
        let root = std::env::temp_dir();
        for role in ["daedalus", "themis", "coder", "tester"] {
            assert!(
                role_registry(role, &root).get("process").is_some(),
                "{role} can start and monitor a long-running command"
            );
        }
        for role in [
            "argus",
            "metis",
            "nemesis",
            "clio",
            "mnemosyne",
            "planner",
            "reviewer",
            "unknown-role",
        ] {
            assert!(
                role_registry(role, &root).get("process").is_none(),
                "{role} is read-only — no background command pool"
            );
        }
    }

    #[test]
    fn pantheon_capability_grants_follow_the_role_table() {
        let root = std::env::temp_dir();
        // Every role (and the unknown fallback) carries the read-only git window — that is the
        // whole point of git_inspect: a reviewer with no shell can still see the diff.
        for role in [
            "daedalus",
            "argus",
            "metis",
            "nemesis",
            "themis",
            "clio",
            "mnemosyne",
            "unknown-role",
        ] {
            let r = role_registry(role, &root);
            assert!(r.get("git_inspect").is_some(), "{role} has git_inspect");
        }
        // Web research is a per-role grant now: argus and mnemosyne work entirely locally.
        for role in ["argus", "mnemosyne"] {
            assert!(
                role_registry(role, &root).get("web_search").is_none(),
                "{role} has no web — its job is local"
            );
        }
        for role in ["metis", "nemesis", "themis", "clio", "daedalus"] {
            assert!(
                role_registry(role, &root).get("web_search").is_some(),
                "{role} keeps web research"
            );
        }
        // The unknown fallback stays conservative: no web either.
        assert!(role_registry("unknown-role", &root)
            .get("web_search")
            .is_none());
        // session_recall is the historian's instrument alone — and mnemosyne stays read-only
        // (the memory WRITE trio is top-level only; nothing here may mutate memory).
        assert!(role_registry("mnemosyne", &root)
            .get("session_recall")
            .is_some());
        for role in ["argus", "metis", "nemesis", "themis", "clio", "daedalus"] {
            assert!(
                role_registry(role, &root).get("session_recall").is_none(),
                "{role} has no transcript recall"
            );
        }
        for tool in ["memory_save", "memory_update", "memory_forget"] {
            assert!(
                role_registry("mnemosyne", &root).get(tool).is_none(),
                "mnemosyne cannot mutate memory ({tool})"
            );
        }
        assert!(
            crate::agent::task_tool::dispatch_is_read_only(&role_registry("mnemosyne", &root)),
            "mnemosyne is read-only → fans out"
        );
        // Legacy aliases land on the same scopes as their canonical names. Compared on the
        // STABLE surface only: skill_*/lsp_*/symbol_*/repo_map registration is gated on global
        // state (skills existing, LSP enabled) that parallel tests legitimately flip between the
        // two builds — equality over those names is a race, not a property of the role table.
        let stable_names = |role: &str| -> Vec<String> {
            role_registry(role, &root)
                .names()
                .into_iter()
                .filter(|n| {
                    !n.starts_with("skill_")
                        && !n.starts_with("lsp_")
                        && !n.starts_with("symbol_")
                        && n != "repo_map"
                        && n != "read_symbol"
                })
                .collect()
        };
        for (legacy, canon) in [
            ("coder", "daedalus"),
            ("planner", "metis"),
            ("reviewer", "nemesis"),
            ("tester", "themis"),
        ] {
            assert_eq!(
                stable_names(legacy),
                stable_names(canon),
                "{legacy} == {canon}"
            );
        }
        // A specialist keeps web unconditionally (its `tools:` can only name destructive grants,
        // so there would be no way to ask for web back).
        let spec = agent_registry(
            &crate::agents::AgentDef {
                name: "S".into(),
                description: String::new(),
                color: String::new(),
                emoji: String::new(),
                vibe: String::new(),
                tools: vec!["Read".into()],
                model: None,
                base_url: None,
                api_key_ref: None,
                body: "b".into(),
                division: None,
                source: crate::agents::AgentSource::AizenHome,
                source_path: std::path::PathBuf::new(),
            },
            &root,
        );
        assert!(spec.get("web_search").is_some(), "specialists keep web");
        assert!(
            spec.get("git_inspect").is_some(),
            "specialists get git_inspect"
        );
    }

    #[test]
    fn git_inspect_runs_real_queries_through_the_sandbox_runner() {
        // End-to-end proof of the exec path (SandboxRequest::exec → spawn → drain): a real repo,
        // real commits, and the closed argv actually reaching git. Skipped where git is absent.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            // A developer box without git may skip; CI may not — a skipped end-to-end proof
            // there is a green check that proved nothing (E6.2, quality plan Q4).
            assert!(
                std::env::var("CI").is_err(),
                "CI must have git on PATH for the git_inspect end-to-end test"
            );
            eprintln!("git not on PATH — skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("aizen-gitinspect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(st.status.success(), "git {args:?}: {:?}", st);
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "-m",
            "first",
        ]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();

        let t = GitInspect::new(dir.clone());
        // A dirty tree shows in status; the uncommitted edit shows in diff; log names the commit.
        let status = t.execute(&serde_json::json!({"action": "status"})).unwrap();
        assert!(
            status.contains("a.txt"),
            "status sees the dirty file: {status}"
        );
        let diff = t.execute(&serde_json::json!({"action": "diff"})).unwrap();
        assert!(diff.contains("+two"), "diff carries the patch: {diff}");
        let log = t
            .execute(&serde_json::json!({"action": "log", "limit": 5}))
            .unwrap();
        assert!(log.contains("first"), "log names the commit: {log}");
        let blame = t
            .execute(&serde_json::json!({"action": "blame", "path": "a.txt", "rev": "HEAD"}))
            .unwrap();
        assert!(blame.contains("one"), "blame shows the line: {blame}");
        // A non-repo directory surfaces git's own error as a soft error, not a panic.
        let outside = std::env::temp_dir().join(format!("aizen-nongit-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        let t2 = GitInspect::new(outside.clone());
        let err = t2
            .execute(&serde_json::json!({"action": "status"}))
            .unwrap();
        assert!(err.starts_with("error: git status failed"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn git_inspect_validates_its_argv_boundary() {
        // Pure-validation paths: no git spawn happens for any of these.
        let t = GitInspect::new(std::env::temp_dir());
        let rej = |args: serde_json::Value| {
            let out = t.execute(&args).unwrap();
            assert!(out.starts_with("error:"), "expected refusal, got: {out}");
            out
        };
        // An option smuggled as a rev/path must never reach argv.
        rej(serde_json::json!({"action": "log", "rev": "--output=/tmp/pwn"}));
        rej(serde_json::json!({"action": "diff", "rev": "HEAD; rm -rf ."}));
        rej(serde_json::json!({"action": "diff", "path": "--work-tree=/etc"}));
        // blame without a path is refused up front.
        rej(serde_json::json!({"action": "blame"}));
        // Read-only classification: never destructive, parallel-safe, no workspace effect.
        assert!(!t.is_destructive());
        assert!(t.is_concurrency_safe());
        assert_eq!(
            t.workspace_effect(&serde_json::json!({"action":"status"})),
            WorkspaceEffect::None
        );
        // Ranges and revisions a real reviewer needs all pass validation.
        for rev in [
            "HEAD~3",
            "main..HEAD",
            "a1b2c3",
            "v0.6.6",
            "@{u}",
            "HEAD:src/lib.rs",
        ] {
            assert!(super::valid_git_rev(rev), "{rev} should be accepted");
        }
        for rev in ["-n1", "--help", "a b", "x\ny"] {
            assert!(!super::valid_git_rev(rev), "{rev} should be refused");
        }
    }
}

#[cfg(test)]
mod deferral_tests {
    use super::*;
    use crate::core::turn_shape::TurnShape;

    fn root(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aizen-defer-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn adv(r: &ToolRegistry, name: &str) -> bool {
        r.advertised_names().iter().any(|n| n == name)
    }

    #[test]
    fn coding_shape_defers_the_rare_builtins_behind_tool_search() {
        let mut r = default_registry_in(&root("coding"));
        r.register(Box::new(PersonaCreate));
        let before = r.advertised_names().len();
        defer_builtins_for_shape(&mut r, Some(TurnShape::SmallEdit));
        for n in [
            "checkpoint",
            "memory_save",
            "skill_save",
            "persona_create",
            "web_crawl",
        ] {
            assert!(!adv(&r, n), "{n} must not ride the request");
            assert!(r.get(n).is_some(), "{n} stays dispatchable");
        }
        for n in [
            "file_read",
            "file_edit",
            "shell_run",
            "process",
            "file_move",
            "memory_search",
        ] {
            assert!(adv(&r, n), "{n} stays advertised on a coding turn");
        }
        assert!(adv(&r, "tool_search"), "the door is advertised");
        assert!(r.advertised_names().len() < before);
        assert!(r.deferred_entries().iter().all(|(_, o)| o == "builtin"));
        // Idempotent: a second pass (the registry is rebuilt every turn) changes nothing.
        let names = r.advertised_names();
        defer_builtins_for_shape(&mut r, Some(TurnShape::SmallEdit));
        assert_eq!(r.advertised_names(), names);
    }

    #[test]
    fn question_shape_defers_more_and_research_keeps_the_crawler() {
        let mut q = default_registry_in(&root("question"));
        defer_builtins_for_shape(&mut q, Some(TurnShape::Question));
        assert!(!adv(&q, "process") && !adv(&q, "file_move"));
        assert!(adv(&q, "file_read") && adv(&q, "search_files"));
        let mut rs = default_registry_in(&root("research"));
        defer_builtins_for_shape(&mut rs, Some(TurnShape::Research));
        assert!(adv(&rs, "web_crawl"));
        assert!(!adv(&rs, "checkpoint"));
    }

    #[test]
    fn deferred_note_names_builtins_and_counts_mcp_servers() {
        let surface: crate::agent::tools::DeferredSurface = (
            vec![
                "checkpoint".into(),
                "memory_save".into(),
                "mcp_github_issue".into(),
            ],
            vec![("builtin".into(), 2), ("github".into(), 1)],
        );
        let note = deferred_tools_note_for(&surface);
        assert!(note.contains("checkpoint, memory_save"), "{note}");
        assert!(
            note.contains("1 MCP tool(s)") && note.contains("github (1)"),
            "{note}"
        );
        assert!(note.contains("tool_search"));
        let only_builtin: crate::agent::tools::DeferredSurface =
            (vec!["workflow".into()], vec![("builtin".into(), 1)]);
        let note = deferred_tools_note_for(&only_builtin);
        assert!(!note.contains("MCP"), "{note}");
    }

    #[test]
    fn coding_turn_advertised_schema_fits_the_lean_budget() {
        // The top-level surface with delegation, persona and LSP registered — the maximal shape —
        // after the coding-turn deferral. Measured 28.2 KB on 2026-09-15 (42.2 KB before).
        const CEILING: usize = 32_000;
        let root = root("lean");
        let mut r = default_registry_in(&root);
        r.register(Box::new(PersonaCreate));
        r.register(Box::new(crate::agent::task_tool::TaskTool::new(
            reqwest::Client::new(),
            "http://x".into(),
            "k".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            root.clone(),
            0,
            200_000,
        )));
        r.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            reqwest::Client::new(),
            "http://x".into(),
            "k".into(),
            "m".into(),
            crate::core::approval::ApprovalMode::Ask,
            0,
            root.clone(),
            200_000,
        )));
        register_subagent_lsp_read(&mut r, &root);
        register_subagent_lsp_write(&mut r, &root);
        defer_builtins_for_shape(&mut r, Some(TurnShape::SmallEdit));
        let bytes: usize = r
            .defs()
            .iter()
            .map(|d| serde_json::to_string(d).unwrap().len())
            .sum();
        assert!(
            bytes <= CEILING,
            "advertised schema on a coding turn is {bytes} B > {CEILING} B"
        );
    }
}
