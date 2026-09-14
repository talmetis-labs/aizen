# Aizen — quality audit and improvement roadmap (2026-09-14)

Status: proposal for the maintainer, nothing applied. Scope is the product as it is on
`prerelease/v0.6.7` (159,152 lines, 1,862 tests): make the existing agent, its tools, its seven
sub-agents and its operator surface as strong as they can be, without adding new product areas.

Method: eight parallel read-only audits (loop core, tools, LLM client, memory/learning, TUI/UX,
sessions/config/safety, measurement + prior plans, Pantheon orchestration), each required to cite
`file:line`. The six highest-severity claims were re-checked by hand against the source before
this document was written; those are marked **[verified]**. Everything else is agent-reported with
the agent's stated confidence. Prior plans (`aizen-improvement-plan.md` §9, the 2026-07-26 audit
P0s, `p0-harness-persistence.md`) were re-verified against code: 30 of 36 items are DONE, the rest
are listed in Appendix A.

---

## 0. Summary

**What is already above the median terminal agent, and must not regress**

- Loop discipline: divergence 2-cycle latch cleared only by novel content, evidence/stall ledger
  with normalised `error_class`, tool-result pre-fill so Esc never leaves a dangling `tool_calls`,
  verify gate with baseline delta, merged done-gate cascade (verify → self-review → todo → confidence)
  in one round-trip. `src/agent/mod.rs`.
- Edit ladder R1–R6 with one-match-per-rung invariant and `nearest_miss`, CRLF-preserving splices,
  atomic in-memory batch edit, clobber guard via read ledger. `src/agent/builtin.rs`.
- Streaming client: lenient duplicate-key recovery, tool-call id re-routing, `args_complete`,
  blank-stream replay gated on nothing produced, mid-drop salvage, reasoning-channel fallback on
  both paths. `src/llm/client.rs`.
- Pantheon: one declarative role table with invariant tests, unknown role refused instead of
  substituted, real per-call and wall deadlines, deterministic partial reports, per-child cancel.
  `src/agent/roles.rs`, `src/agent/task_tool.rs`.
- Persistence: session v2 with the image round-trip closed, crash-lease recovery that never
  replays tools, time-machine journal + OS lock, sandbox honesty contract that matches the doc.
- Retained TUI: Esc-at-approval denies and unwinds, cancel autosaves the partial turn, Windows
  mode re-assertion and ConPTY resize-settle.

**Eight root causes that make the shipped agent weaker than its code**

| # | Root cause | Anchor |
|---|---|---|
| RC1 | The harness lies to the model about what it read: `file_read` is cut a second time by keywords from the file *path*; `shell_run` and `task` are cut to 4,096 chars head-⅔/tail-⅓; every tool's trailing "truncated / narrow your query" hint is deleted by the relevance cut | `src/agent/mod.rs:3343-3360`, `:705`, `:5012` **[verified]** |
| RC2 | The prompt cache is broken on every turn: the dynamic system lane is inserted at index 1, ahead of history, and its `<sessions>` rows carry the current session's live message count and age | `src/agent/prompt_lanes.rs:144`, `src/core/session_store.rs:757` **[verified]** |
| RC3 | "Verified done" exists only for Rust and TypeScript, and only as a typecheck; every other language falls straight to `Done` with no trace line | `src/agent/verify_gate.rs:9-21` **[verified]** |
| RC4 | The no-prompt tier has structural holes: newlines are flattened before the program token is inspected, so `ls\nrm -rf build` is read-only; `find -delete`, `cargo fmt`, `npm test`, `git branch -d` are allow-listed | `src/agent/cmd_guard.rs:341-355`, `:183-212` **[verified]** |
| RC5 | A long single-turn run cannot compact (planner needs ≥ 2 user messages) and auto-compaction drops the touched-file list that manual `/compact` keeps | `src/agent/compact.rs:192`, `:246` |
| RC6 | The seven agents are uniform where they should differ: same model, same 25-step budget, a one-shot brief with no parent context, report truncated at 4,096, no dependency chain, approvals not attributed to the child | `src/agent/roles.rs:88-172` **[verified]**, `task_tool.rs:880`, `workflow.rs:60-88` |
| RC7 | The operator cannot see what they approve: basename or 52-char clip, diff rendered after the write, shell rows show only `exit N`, retry sleeps print nothing, transcript cache wiped past 512 blocks | `src/agent/mod.rs:5140`, `:3717`, `:3855`, `src/llm/client.rs:740`, `src/ui/tui/retained.rs:278` |
| RC8 | Nothing measures task success: the loop bench drives a scripted model with the verify gate off and CI never runs it; no replay layer, no task suite, `src/repl/` has zero tests, the release profile is never tested | `src/bench/loop_eval.rs:570-590`, `.github/workflows/ci.yml:45-51` |

Two more, outside the loop but costly: the memory write path's dedup gate has never fired on this
machine (1,056 audit events, zero `add`/`reinforce`), so the always-on core is empty and the store
is 88 % of its cap; and post-turn learning runs three sequential model calls on the main coding
model at max effort, blocking the next prompt on 39 % of turns.

**Order of work**: fix what makes the model reason from false inputs (Phase 0), then the loop and
client (1), then the Pantheon (2), then the operator surface (3), memory (4), persistence (5), with
measurement (6) started in week 1 and running throughout.

---

## 1. What aizen has today

| Area | Surface | Main files |
|---|---|---|
| Loop | step cap + auto-extend + continuations, steering drain, tool-result clearing 60→45 %, auto-compaction, budget nudge, divergence guard, evidence ledger, done-gate cascade, goal gate | `src/agent/mod.rs` (10.5 k lines) |
| Tools | memory ×7, session_recall, file_read/glob/edit/write/move, search_files, codebase_search, repo_map, web_search/fetch/crawl, skills ×4, telegram/bot_admin/notify, checkpoint ×2, team_status, shell_run, git_inspect, process ×n, LSP ×10, task, fanout, verify, browser ×5 (gated), MCP passthrough | `src/agent/builtin.rs`, `search.rs`, `codebase.rs`, `repo_map.rs`, `lsp/`, `process.rs`, `web_tools.rs`, `mcp.rs` |
| Client | OpenAI dialect streaming with eager tool start, non-streaming path, Codex Responses dialect, gateway/session auth, Anthropic cache breakpoints, cost meter, `/models` probe | `src/llm/` |
| Prompt | stable lane (14.8 KB base + environment + project context) and dynamic lane (soul, persona, self, user_memory, skills, sessions); recall folded into the user turn | `src/agent/prompt_lanes.rs`, `system_prompt.md` |
| Sub-agents | 7 roles, depth 1, contract with boundaries/expected_output/step_budget, slot files, wall deadline, partial report; flat workflow fan-out + synthesis; `/workflows` registry; specialist cards with per-card model | `roles.rs`, `roles/*.md`, `task_tool.rs`, `workflow.rs`, `orchestration.rs`, `src/agents/` |
| Memory | 3 tiers, BM25 with bilingual tokenizer, frozen core, secretary/reflection post-turn, MinHash bloat tools, optional dense | `src/memory/` (19 k lines) |
| Safety | hard floor blocklist, approval ask/smart/yolo, OS sandbox (Landlock+seccomp, Seatbelt, Job Object partial), SSRF floor, audit log | `cmd_guard.rs`, `src/core/approval.rs`, `src/sandbox/` |
| Persistence | session v2 + provenance, crash recovery, time machine (git object store), coop manifests, foreign-session import, cron via OS scheduler, self-update | `src/core/`, `src/features/` |
| UI | retained ratatui TUI, markdown, side-by-side diff box, approval + clarify menus, `/workflows` panel, image input, sixel screensaver | `src/ui/` (23 k lines) |
| Measurement | memory recall bench with baseline, profile/dialectic golden sets, 19-scenario scripted loop bench | `src/bench/`, `bench-fixtures/` |

---

## 2. Findings ledger

Severity: P0 = the model or the user acts on false information, or data/safety is at risk;
P1 = measurable quality or cost loss on common paths; P2 = real but bounded. Confidence is the
auditing agent's unless marked [verified].

### 2.1 Loop core (`src/agent/`)

| ID | Sev | Where | Defect |
|---|---|---|---|
| L1 | P0 | `verify_gate.rs:9-21` [verified] | Detection knows `Cargo.toml`, npm typecheck script, `tsconfig.json` only. Python/Go/Java/.NET/C → `None` → `verify_attempts` stays 0 → `Done` with no verification and no trace line, while the prompt promises "verified done". |
| L2 | P1 | `verify_gate.rs:37` | Supported path runs `cargo check` / `tsc --noEmit`: typecheck, never tests, unless `.aizen/verify.json` exists. |
| L3 | P0 | `mod.rs:705`, `:3343` [verified] | `max_tool_result_chars = 4096`, `shell_run` not relevance-truncatable: build/test output is cut head-⅔/tail-⅓ at ~1 k tokens. Three failing tests → the middle ones vanish → model fixes what it can see and re-claims done. |
| L4 | P1 | `mod.rs:3343` | `task` results are head/tail-clipped at 4,096 too: the child's findings list is the middle. |
| L5 | P1 | `compact.rs:192` | `plan_compact_cut` returns `None` with < 2 user messages; the canonical shape (one prompt, 50 tool steps) can never auto-compact; only clearing remains, which cannot touch assistant text or tool-call args. |
| L6 | P1 | `compact.rs:246` | `context_touchpoints` (paths, skills, commands) is used only by manual `/compact`; auto-compaction splices summary prose only, and the model re-searches for paths it had. |
| L7 | P1 | `todo.rs:54`, `mod.rs:1934` | `TODOS` is process-global, cleared only by `/new` `/clear` `/resume`. A stale item from turn 1 makes turn 2 ("what does X do?") burn up to 2 todo-poke round-trips; lanes under `serve` share the list. |
| L8 | P1 | `mod.rs:4974` | Nudges are pushed as `system` mid-history and never retired. On the Codex path every system message is hoisted into the instructions blob, so "you repeated the same call" becomes a permanent instruction and busts the cache. |
| L9 | P2 | `core/effort.rs` | Five tiers change only the `reasoning_effort` wire string; no tier touches `max_iters`, verify attempts, self-review, result budgets. On providers that ignore the field, `ultrathink` is a no-op. |
| L10 | P2 | `mod.rs:749` | `enable_self_review: false` by default: no harness check of minimal-diff / no-speculative-refactor before Done. |
| L11 | P2 | `mod.rs:1447` | `overflow_shrunk` is a one-shot bool per run; a second overflow fails even with evictable bodies left. |
| L12 | P2 | `mod.rs:1229` | Compaction failure (summariser down) still arms `last_compact`; run continues toward overflow. |
| L13 | P2 | `builtin.rs:4235` | Schema-budget ratchet measures the registry with LSP off, but LSP is default-on: ~8 KB of LSP/repo_map schema rides every turn uncounted. Fixed per-turn overhead plausibly 18–25 k tokens. |

### 2.2 Tools (`src/agent/builtin.rs` and friends)

| ID | Sev | Where | Defect |
|---|---|---|---|
| T1 | P0 | `mod.rs:3352` [verified] | `relevance_query_from_args` includes `"path"`, so `file_read` output is scored by tokens of its own path and returned as `head + … + keyword window`; the model's `old_string` copied across the seam fails to match. |
| T2 | P0 | `builtin.rs:1865,1881,1913` | `number:true` or `start`/`end` bypass `budget_view`: a 10 k-line file is materialised whole, then mangled by T1; the "partial view" marker never fires. |
| T3 | P1 | `mod.rs:5012` | `truncate_relevant` drops the tail, deleting `…[capped at 200 matches]`, `…[N more files]`, and the byte-branch marker. The model cannot tell a list is truncated. |
| T4 | P1 | `cmd_guard.rs:170` | `cargo build &> build.log` is hard-BLOCKED as file-blanking (the `&` matches the leading class). Unappealable. |
| T5 | P1 | `builtin.rs:2923,2944` | `replace_all` exists only on rung R1 (exact). CRLF or indent drift → R2 finds N blocks and errors "add more context"; N round-trips follow. |
| T6 | P1 | `builtin.rs:3322,2378` | `diff_preview` echoes up to 40 added lines per edit (text the model just wrote); a 10-edit batch emits ~800 lines then hits the 4,096 cut. |
| T7 | P1 | `cmd_guard.rs:183-212` [verified] | `find` (with `-delete`/`-exec rm`), `cargo fmt`, `cargo clippy`, `npm test`, `npm audit`, `git branch -d`, `git tag -d`, `git remote remove` are read-only and auto-run under `smart`. |
| T8 | P0 | `cmd_guard.rs:341-355` [verified] | `collapse_ws` flattens newlines; segments split on `| ; &` only; only the program token is inspected. `ls\nrm -rf build` classifies as `ls` → Allow. |
| T9 | P2 | `mcp.rs:1930` | MCP auto-deferral default is 0 (off): every connected server's full schema rides every request; `tool_search` is a mitigation nobody turns on. |
| T10 | P2 | `lsp/mod.rs:459` | `edit_feedback` blocks up to 3.5 s per edit, returns nothing on the first edit of a session, and reports only the edited file's diagnostics. |
| T11 | P2 | `builtin.rs:2112,2155` | `file_glob` structured walk ignores `.gitignore` and does not prune `target/`, `node_modules/`; `search_files` does. Two siblings, opposite semantics, no hint. |

### 2.3 LLM client (`src/llm/`)

| ID | Sev | Where | Defect |
|---|---|---|---|
| C1 | P0 | `prompt_lanes.rs:144`, `session_store.rs:757` [verified] | Dynamic lane at index 1 with live `{turns} msgs` and `age_label` of the autosaving current session → prefix differs every turn → only tools + system[0] cache; whole transcript re-billed (~$0.22 vs ~$0.06 per turn at 60 k on a Sonnet-class model). |
| C2 | P1 | `client.rs:1681` | Streamed `usage` recorded only when `choices.is_empty()`; providers that send usage on the final content chunk (Ollama shim, llama.cpp, LiteLLM) never register → `/cost` blank, `RealAnchor` never arms, HUD % stays chars/4. |
| C3 | P2 | `client.rs:1068` | `stream_chat_with_visual_contract` sends no `stream_options.include_usage` yet reads `chunk.usage`; hand-rolls body instead of `build_chat_body`. |
| C4 | P1 | `responses_codex.rs:329` | Codex path buffers the whole SSE: no incremental text, no eager tools, no stall watchdog; frozen spinner for the whole generation. |
| C5 | P1 | `responses_codex.rs:280` | 401 branch refreshes and `continue`s with no attempt counter: a rejected refreshed token loops forever. |
| C6 | P2 | `responses_codex.rs:336` | Overload markers matched against the whole body including model output; quoting `server_is_overloaded` discards and re-bills a finished turn. |
| C7 | P1 | `client.rs:581,599` | `STREAM_STALL_SECS = 90` measured to first useful chunk; reasoning models silent > 90 s are treated as "never started" and replayed up to twice: three billed runs discarded. |
| C8 | P1 | `client.rs:1379`, `mod.rs:3169` | Strict `serde_json` only: no JSON repair, no `<tool_call>`/fenced-JSON fallback. Local/open models' malformed calls are dropped, the turn returns empty, the loop re-sends the identical request. |
| C9 | P2 | `client.rs:1225` | Always `max_tokens`, never `max_completion_tokens`; o-series/gpt-5 reject with 400, no strip-and-retry. |
| C10 | P2 | `client.rs:1232`, `:100-124` | `parallel_tool_calls`, `tool_choice`, and `cache_control` (for any model name matching `opus|sonnet|haiku|fable`) sent unconditionally; strict local servers 400. |
| C11 | P2 | `mod.rs:4256` | `estimate_defs_tokens` uses bytes/4, `estimate_message_tokens` chars/4; both under-count code and CJK by 30–50 %. |

### 2.4 Memory, learning, persona

| ID | Sev | Where | Defect |
|---|---|---|---|
| M1 | P0 | `learning/mod.rs:393`, `consolidate.rs:33` | Dedup is one best-match at lexical 0.78 within the same tier partition. On disk: 1,056 audit events, 0 `add`, 0 `reinforce`; 466 entries all `sessions: 1`; store at 88 % of the 500 cap heading to LRU eviction. |
| M2 | P1 | `frozen_core.rs:79` | Core requires `sessions >= 2`; with M1 that is unreachable. `core/active/*.md` are 0 bytes: the always-on memory delivers nothing. |
| M3 | P1 | learning write path | Tier misclassification: 239 `tier: user` entries include other projects' architecture notes; if M1 is fixed they enter the core in every project. |
| M4 | P1 | `memory/mod.rs:329,564` | Recall gate = coverage of the query by the top hit ≥ 0.34; long Vietnamese queries cannot clear it. Live: 1.1 facts injected per turn, 31 % cited. |
| M5 | P1 | `tokenize.rs:49` | No identifier splitting: `get_by_id` is one token; "get by id" scores zero against it. Half the corpus is identifiers. |
| M6 | P1 | `repl/turn.rs:302-320`, `postturn.rs` | Secretary → persona reflection → auto-compact run sequentially inline before the next prompt, on the main coding model at max effort (no summariser role configured), 300 s each under a 600 s block. Fires on 39 % of turns. |
| M7 | P1 | `session_store.rs:743`, `memory/store.rs:373` | `recent_sessions_block` reads and parses up to 8 session files (400–750 KB each) per turn; `load_all` re-reads 466 markdown files per recall and again in the secretary. No cache. |
| M8 | P1 | `persona/self_mem.rs:376` | `save_insight` has no near-duplicate check; three copies of the same insight on disk re-spend the 700-token `<self>` budget. |
| M9 | P2 | `memory/render.rs:32` | `chars/4` token estimate under-counts Vietnamese ~2×; every cap is really double. |
| M10 | P2 | `project_context.rs:28,57` | `CONVENTION_FILES` takes the first hit per directory: this repo's 7 KB `CLAUDE.md` is ignored in favour of the 787-byte `AGENTS.md`; never refreshed mid-conversation. |
| M11 | P2 | `agent/mod.rs:234-244` | Persona + self + soul cost up to 1,900 nominal tokens per turn when a persona is active, ungated by relevance, and do nothing for a coding task. Sub-agents correctly skip all of it. |

### 2.5 Operator surface (`src/ui/`, `src/repl/`)

| ID | Sev | Where | Defect |
|---|---|---|---|
| U1 | P0 | `agent/mod.rs:5140`, `:3538` | Approval prompt is `⚙ tool  target` where target is a basename or 52-char clip. `python deploy.py --prod --force-delete` shows as `deploy.py`. No cwd, no argv, no diff. |
| U2 | P0 | `agent/mod.rs:3717` | Diff is rendered inside `emit_tool_result`, after the write. There is no preview-before-approve for any edit tool. |
| U3 | P0 | `ui/tui/retained.rs:278`, `paint.rs:409` | `rows.clear()` at 512 blocks while `BLOCK_LIMIT = 2048`; paint walks all blocks per frame. Past 512 blocks every frame re-renders the whole transcript at 110 ms. |
| U4 | P1 | `agent/mod.rs:3855` | `shell_run` transcript row is `exit 101` and nothing else; no expand key. |
| U5 | P1 | `repl/turn.rs:360` | Verification-failed message tells the user to type `/rewind`, which does not exist. |
| U6 | P1 | `llm/client.rs:740` | Retry loop sleeps on `Retry-After`/backoff and prints nothing: indistinguishable from a hang. |
| U7 | P1 | `cli/time.rs:303` | `/diff -p` is monochrome (`git diff-tree -p` over a pipe). |
| U8 | P1 | `slash_handlers.rs:1867` | `/undo` applies immediately: no preview, no confirm on a dirty tree, receipt names no files. |
| U9 | P1 | `core/approval.rs:14` | Approval is a 3-value global plus one session allow-all; no per-tool / per-path / per-prefix grant, so users escape prompt fatigue with allow-all. |
| U10 | P1 | `ui/tui.rs:136` | 15 s idle screensaver (sixel hosts) covers the screen while the user reads a long diff. |
| U11 | P2 | `ui/markdown.rs:702` | Over-long token split by chars not display width; CJK paragraphs come out ~2× the budget. |
| U12 | P2 | `agent/mod.rs:3717` | Diff box header uses basename; several `mod.rs` edits in one turn are indistinguishable. |

### 2.6 Sessions, config, safety, update

| ID | Sev | Where | Defect |
|---|---|---|---|
| S1 | P1 | `cli_config.rs:1226` | Config (API key, providers, approval mode) written with `std::fs::write`, not `persist::atomic_write`; a crash mid-write loses it and `load()` returns defaults. |
| S2 | P1 | `timemachine.rs:2564,2745` | `apply_tree` seeds the temp index from the source `.git/index`, so files created since the checkpoint survive; post-restore verification then fails and rolls back. `/undo` fails on the most common case. |
| S3 | P2 | `session_store.rs:295-322,471` | Every turn re-reads and rewrites the whole session (pretty JSON, full tool results, base64 images); no size cap, no pool pruning: O(n²) I/O on an AV-scanned dir. |
| S4 | P1 | `update.rs:242-283` | Download verified only against GitHub's reported size. No SHA-256, no signature. |
| S5 | P1 | `update.rs:277` | `.part` is flushed but never `sync_all()`'d before the rename over the live executable. |
| S6 | P2 | `update.rs:303`, `repl/startup.rs:225` | Backups `aizen*.old-*` deleted on the next launch with no age gate; rollback survives one terminal open. |
| S7 | P2 | `cron.rs:97-107,196` | OS entry registered before the spec is written; job log not owner-only; failures never reach the notify channel. |
| S8 | P2 | `timemachine.rs:1148` | Store never prunes unreachable objects; size is monotonic per repo. |
| S9 | P1 | `core/approval.rs` | `/yolo` persists globally, arming every future window and cron job. |

### 2.7 Pantheon and orchestration

| ID | Sev | Where | Defect |
|---|---|---|---|
| O1 | P0 | `agent/mod.rs:705`, `:3337` | Parent sees the child's report head/tail-truncated at 4,096 chars; a 9-finding nemesis report loses findings 4–6. (= L4) |
| O2 | P1 | `task_tool.rs:880` | Child message list is `[system, prompt]`: no parent findings digest, no read-cache manifest, no file list, no todo. Read cache is per resource scope, so each child re-reads from zero. |
| O3 | P1 | `roles.rs:32-60` [verified] | `RoleProfile` has no `model` field; only the global `roles.subagent_default` and a per-call `model` arg. Specialist cards can pin a model, built-in roles cannot. |
| O4 | P1 | `roles.rs:88-172` [verified] | All seven `default_max_steps = 25`; argus (locate 3 symbols) and daedalus (implement + build + test) share a budget. |
| O5 | P1 | `workflow.rs:463,326` | Singular writer is enforced, but read-only siblings run in the same `join_all` chunk as the writer: nemesis reviews a tree daedalus is mutating. |
| O6 | P1 | `core/workspace_txn.rs:188-198` | Writer lease is process-reentrant per worktree; parent/child clobber is prevented only by barrier scheduling, not by a lock. |
| O7 | P1 | `agent/mod.rs:5143-5177` | A child's destructive call prompts with no attribution: the user sees `Run rm -rf build — approve?` with no `daedalus · fix parser`. |
| O8 | P1 | `workflow.rs:60-88` | No `after`/`depends_on`: daedalus → themis → nemesis must be hand-chained by the model across turns, re-briefed each time, through O1. |
| O9 | P1 | `task_tool.rs:1207`, `workflow.rs:845` | Workflow children run with `enable_verify_gate: false`; a daedalus inside a fan-out returns "implemented" unbuilt, and the parent's gate does not cover it. |
| O10 | P2 | `task_tool.rs:940`, `workflow.rs:1006` | No child-level retry: a `Deadline` or `VerificationFailed` child becomes an error string, never re-dispatched with a tightened brief. Pre-edit checkpoint exists but is never auto-restored, so half-done edits stay. |
| O11 | P2 | `workflow.rs:100`, `llm/client.rs:18-41` | No per-child token/cost attribution; usage is a process-global static. |
| O12 | P2 | `workflow.rs:248` | CLI runner passes `None` as synthesis cap; a 32-task spec can build a ~128 k-char synthesis request. |
| O13 | P2 | `roles.rs:166` | mnemosyne's brief contains runs of 17 literal spaces (missing `\` continuation); ships in every dispatch. |
| O14 | P2 | `system_prompt.md:141-147` | Delegation guidance is all *how*; nothing says when not to delegate, what a good brief looks like, or that nemesis and themis differ only by shell. `prompt` is checked non-empty only. |

### 2.8 Measurement

| ID | Sev | Where | Defect |
|---|---|---|---|
| Q1 | P1 | `bench/loop_eval.rs:37-56,570-590` | The scripted model ignores its input; verify gate, checkpoints, context guard, continuations and stall recovery are all disabled. "verified-done 100 %" means the state machine reached `Done`. |
| Q2 | P1 | `.github/workflows/ci.yml:45-51` | CI runs build/test/sandbox-doctor only; the loop bench never runs. `fmt`/`clippy` are `continue-on-error`. |
| Q3 | P1 | whole tree | No end-to-end test runs the real loop against a real or recorded model on a repo task. No record/replay layer. `Cargo.toml` has no `[dev-dependencies]`. |
| Q4 | P2 | `src/repl/` (1,739 lines, 0 tests), `src/cli/` (369 lines/test) | The turn loop and startup have no tests. 38 `cfg(unix)`-gated tests mean FS/permission behaviour is never asserted on Windows, the primary platform. |
| Q5 | P2 | `ci.yml`, `Cargo.toml:133-138` | Release profile (`lto = fat`, `codegen-units = 1`, `strip`) is built on tags only and never tested. |
| Q6 | P2 | `aizen-improvement-plan.md §10` | 6 of 10 planned quality metrics have no implementation (wrong-file edits, data loss, search sufficiency, cross-check, overflow, time-to-first-action). |

---

## 3. Roadmap

Sizes: S ≤ 1 day, M 2–4 days, L a week or more, for one person who knows the code. Each phase
ends with a gate that is a command, not an opinion.

### Phase 0 — stop feeding the model false inputs (week 1)

| Task | Fixes | Design | Size |
|---|---|---|---|
| P0.1 One truncation authority for reads | T1, T2, T3 | Remove `file_read` from `is_relevance_truncatable`; drop `"path"` from `relevance_query_from_args` KEYS; make `FileRead::read_one` apply `budget_view` on every branch (ranged, numbered, `read_many`) with real line numbers in the marker; `truncate_relevant` always re-appends the last ~200 chars of the original so tail hints survive. | S |
| P0.2 Log-shaped results | L3, L4, O1 | Add `max_log_result_chars` (~16 k) for `shell_run`/`process`, `max_delegate_result_chars` (~24 k) for `task`/`fanout`/`verify`; logs cut tail-weighted with an error-line anchor (`^(error|FAILED|panicked|error\[E\d+\])`); delegate results cut by whole `## ` sections with a named omission line (reuse `build_synthesis_prompt_capped`). | S |
| P0.3 Byte-stable dynamic lane | C1 | `recent_sessions_block`: exclude the current slug, coarse day bucket instead of `age_label`, drop `{turns} msgs`; build the block once per conversation (only in `refreshed_system_prompt_bundle`). Move the dynamic lane to sit immediately before the newest user turn instead of index 1; spend the freed breakpoint on the rolling history point. Add a debug assertion in `refresh_dynamic_prompt_lane` that lane 1 is unchanged unless memory/persona/tool surface changed. | M |
| P0.4 Verify gate that admits absence | L1, L2 | `detect_verify_command`: add `pyproject.toml`/`setup.py` → `pytest -x -q`, `go.mod` → `go build ./... && go test ./...`, `pom.xml`/`build.gradle`, `Makefile` `test`/`check` target, `*.csproj`; when detection is `None` and the run edited files, push a one-shot demand: "no verify command detected: run the project's build/test and paste the result before finishing". For Rust/TS, run tests when the edit touched a file with a sibling test, else typecheck. | M |
| P0.5 Structural command guard | T4, T7, T8 | Tokeniser that treats `\n`, `\r`, and newline-in-quotes as separators; inspect every token of a segment; split the allowlist into pure readers and argument-gated ones (`find` without `-delete/-exec/-execdir/-ok`; `cargo fmt --check` only; `git branch/tag/remote` with no mutating flag); drop `npm test`/`npm audit`; add a "runs code, still ask" tier for `cargo test/build`, `pytest`, `go test`, `jest`. Fix the `&>` false positive by anchoring the blank-redirect pattern on `(^|[;|]) *:? *>`. | M |
| P0.6 Atomic config | S1, S9 | Route `save_unlocked` through `persist::atomic_write_owner_only`; keep a `.prev` on every successful save; make `/yolo` session-scoped with an explicit `--persist`. | S |

Gate: `aizen prompt-size --live` (new flag, P6.1) shows lane 1 byte-identical across two consecutive
turns; a Python fixture with a failing pytest cannot reach `Done`; `printf 'ls\nrm -rf x'` is
classified `Ask`; a 3-failure `cargo test` output reaches the model with all three `error[` blocks.

### Phase 1 — loop and client (weeks 2–3)

| Task | Fixes | Design | Size |
|---|---|---|---|
| P1.1 Compaction for single-turn runs | L5, L6, L12 | `plan_compact_cut_at` that cuts on an assistant/tool boundary when < 2 user messages; `splice_compacted` prepends `context_touchpoints(older)` verbatim; a failed compaction does not arm `last_compact`. | M |
| P1.2 Todo scoped to the conversation | L7 | `Mutex<HashMap<ConvId, Vec<Todo>>>` keyed by `exec_ctx` conversation id; cleared at the user-turn boundary unless the previous turn ended `MaxIters`/`Deadline`/`VerificationFailed`. | M |
| P1.3 Nudges that retire | L8 | Track nudge ids; remove a nudge when its condition clears; on the Codex path keep nudges in the user channel, not `system`. | S |
| P1.4 Effort tiers with teeth | L9, L10 | `AgentConfig::apply_effort(Effort)` mapping tier → `max_iters`/`max_continuations`/`max_verify_attempts`/`enable_self_review`/`max_log_result_chars` (Low 12/0/1/off, High 25/3/2/on, Max 40/5/3/on); called where effort is resolved in `repl/turn.rs`. | S |
| P1.5 Overflow re-arm | L11 | Attempt counter (3) re-armed after history shrinks. | S |
| P1.6 Usage recorded once at stream end | C2, C3 | Keep `last_seen: Option<Usage>` across chunks, record after the loop; add `include_usage` to the visual-contract path and build its body with `build_chat_body`. | S |
| P1.7 Two-phase stall deadline | C7 | `first_chunk` (600 s, matching send budget) and `inter_chunk` (90 s). | S |
| P1.8 Lenient tool-call recovery | C8 | `recover_tool_args`: balanced-brace truncation repair, trailing-comma strip, single→double quote; content scanner for `<tool_call>…</tool_call>` and fenced JSON that synthesises a call only when `tool_calls` is empty. | M |
| P1.9 Capability strip-and-retry | C9, C10 | Generalise `effort_unsupported_set` into a per-model `UnsupportedParams` set covering `max_tokens→max_completion_tokens`, `parallel_tool_calls`, `tool_choice`, `cache_control`, driven by the 400 body; persist per endpoint. | M |
| P1.10 Codex path parity | C4, C5, C6 | Consume `bytes_stream().eventsource()` with the same watchdog and incremental rendering; auth attempt counter on 401; match overload markers on the parsed error envelope only. | L |
| P1.11 Honest token estimate | C11, M9 | One estimator: chars/4 Latin, chars/1.8 for text with combining marks or CJK, bytes ignored; used by both `estimate_*` and `memory/render.rs`. | S |
| P1.12 Schema ratchet measures what ships | L13, T9 | Run the budget test with LSP on and the task/persona registry; raise the ceiling once, deliberately; turn MCP auto-deferral on by default at a sane token threshold and print a one-line warning when MCP schema exceeds the builtin surface. | S |
| P1.13 Edit tool polish | T5, T6, T10, T11 | Thread `replace_all` into R2/R3; `dry_run: true` on `file_edit`; diff output = removed lines + `@@` anchor + `+N lines` count, batch capped at 3 hunks; make `edit_feedback` non-blocking after the first edit and add one workspace-diagnostics pass before Done; `file_glob` honours `.gitignore` and prunes `target/`, `node_modules/`, `.git/`; shared routing sentence across the five search tools. | M |

Gate: `aizen bench loop` passes with verify gate ON in every edit scenario (P6.2 makes that
possible); `bench tasks` (P6.3) smoke subset green on Linux and Windows; `/cost` populated on a
llama.cpp endpoint.

### Phase 2 — the Pantheon (weeks 4–5)

The seven roles stay seven, depth stays 1, single writer stays. What changes is that they stop
being seven copies of the same worker.

| Task | Fixes | Design | Size |
|---|---|---|---|
| P2.1 Structured, un-truncated child reports | O1 | Done in P0.2. Additionally require each role's `## Report` to emit fixed `## ` sections (already in the prompts) so section-wise capping is lossless for short reports. | S |
| P2.2 Per-role model and budget | O3, O4, O13 | `RoleProfile { model: Option<&'static str>, .. }` plus a `[roles.pantheon] nemesis = { model = "…" }` config map resolved in `resolve_dispatch` between the `model` arg and `subagent_default`. Differentiated `default_max_steps`: argus 15, clio 20, metis 25, mnemosyne 25, nemesis 25, themis 30, daedalus 45. Fix the mnemosyne brief whitespace. | S |
| P2.3 Child context pack | O2 | `parent_digest()` injected as a second user message before the brief: ≤ 15 `path:line-range` entries the parent read this turn (harvested from the parent's read-cache scope), ≤ 10 "established findings" bullets passed via a new optional `context` arg, and the active todo item. Cap 2,500 chars. | M |
| P2.4 Chained workflow with a fix loop | O5, O8, O9 | `WorkflowTask { after: Option<Vec<String>>, .. }`; replace the chunker with a Kahn-ordered wave scheduler, flat-parallel within a wave, singular writer enforced per wave; read-only siblings never share a wave with the writer. `mode: "implement"` preset = daedalus → themis → nemesis, with a themis FAIL re-dispatching daedalus once carrying the verbatim failure. Workflow writers get the verify gate (drop the `false` at `task_tool.rs:1207`), serialised so the build lock does not thrash. | L |
| P2.5 Write lease and blackboard | O6 | Extend `coop::Claim` to in-process children keyed by `exec_ctx.resource_scope()`, non-reentrant across parent/child; `scratch::dir()/blackboard/<run>/<child>.md` append-only notes listed in each child's `<environment>` and readable by siblings via `file_read`. No new tool. | M |
| P2.6 Attribution, cost, live status | O7, O11, O10 | `dispatch_label: Option<String>` on `ExecutionContext`, prefixed in `approve` (`daedalus · fix parser wants: Run …`) and in the `/workflows` row; `tokens_in/out` on `TaskOutcome` and the orchestration `Entry` summed from each child's `ChatTurn.usage`; `Track::note(step, tool)` from the child loop so `/workflows` shows `step 7/45 · file_edit parser.rs`. One retry for a `Deadline`/`VerificationFailed` writer with a tightened brief, and auto-restore of the pre-edit checkpoint when the retry also fails. | M |
| P2.7 Delegation guidance | O14, O12 | Add to `system_prompt.md`: do not delegate what two reads answer; a brief names files, the question, and the report shape; nemesis reads, themis runs. Brief-quality check in `task`: refuse a brief under ~80 chars with no path or symbol. CLI workflow runner passes the same synthesis cap as the in-conversation path. | S |

Gate: `aizen workflow implement.json` on a fixture repo runs daedalus → themis → nemesis
unattended, themis's FAIL triggers exactly one daedalus retry, `/workflows` shows per-child tokens,
and an approval raised by a child names the child.

### Phase 3 — the operator surface (weeks 6–7)

| Task | Fixes | Design | Size |
|---|---|---|---|
| P3.1 Pre-flight approval payload | U1, U2, U12 | `Tool::preview(args) -> ApprovalPreview { title, cwd, body }`; edit tools compute the patch without writing (the `diff_preview` machinery) and hand it to `render_diff_box`; `shell_run` returns full argv + cwd, wrapped not clipped; header uses the repo-relative path. Rendered above the existing 4-row menu. | M |
| P3.2 Grant scopes | U9 | Menu: `Yes · Always for <tool> · Always for <tool> under <dir> · No · Stop`; session `Vec<Grant{tool, path_prefix}>` checked before `approve()`; optional persisted project allowlist in `.aizen/`. | M |
| P3.3 Render windowing and real LRU | U3 | Resolve scroll offset first, render only blocks intersecting the viewport (+ margin); LRU eviction keyed by block id sized ≥ `BLOCK_LIMIT`. | S |
| P3.4 Expandable results and live network state | U4, U6 | Keep the last N KB of each tool's stdout in the block; `Ctrl-O` on a tool row opens it in `text_overlay`; auto-expand the tail on non-zero exit. `send_with_retry` publishes `set_work_caption("rate-limited — retrying in 43 s (2/3)")`. | M |
| P3.5 Reviewable rewind | U5, U7, U8 | `/rewind` alias of `/undo`; both print the restore's diff stat before applying and confirm when the tree is dirty; `/diff --patch` goes through `DiffHunk → render_diff_box`. | S |
| P3.6 Small correctness | U10, U11 | Screensaver only after 15 s idle **and** no new output in the last 60 s; `char_chunks` by display width. | S |

Gate: approving a `file_edit` shows the patch before any byte is written; a 3,000-block session
paints under 10 ms per frame; a 429 with `Retry-After` is visible in the HUD.

### Phase 4 — memory that earns its tokens (weeks 8–9)

| Task | Fixes | Design | Size |
|---|---|---|---|
| P4.1 Two-stage dedup | M1, M2 | Stage 1: lexical 0.78 across all tiers (drop `same_partition` from the dup check, keep it for the merge target). Stage 2: 0.45–0.78 falls back to MinHash 0.6 plus normalised-token containment. Bump `sessions` on any hit. Re-run `aizen memory consolidate --apply` once on the existing store. | M |
| P4.2 Tier classification | M3 | Facts mentioning a project name or path are `project`, never `user`; a one-shot classifier rule set in the write path, with a `bench profile` golden extension. | S |
| P4.3 Gate denominator and tokenizer | M4, M5 | IDF-weighted coverage over the top-3 hits with denominator `min(|q|, 6)`; snake/camel/kebab splitting that emits both the whole identifier and its parts. Re-tune `RECALL_GATE_COVERAGE` with `bench memory --split all`. | M |
| P4.4 Post-turn learning off the critical path | M6 | Spawn secretary/reflection on a tokio task writing to a queue; drain at the start of the next turn with a 5 s join; chores get a 60 s per-call cap; `summarizer_endpoint` falls back to the cheapest configured model, never the max-effort coder. | M |
| P4.5 Caches on the prompt-build path | M7 | Session-row cache keyed by `(path, mtime, len)`; memory `load_all` cached per process with mtime invalidation. | S |
| P4.6 Self-mem dedup, CLAUDE.md precedence, refresh | M8, M10 | `is_near_duplicate` in `save_insight`; `CONVENTION_FILES` prefers the larger of `CLAUDE.md`/`AGENTS.md` or concatenates both under a cap; re-read on mtime change at lane refresh. | S |
| P4.7 Persona gating | M11 | Persona/self/soul blocks are injected only when the turn is not tool-bound (no file/shell tools in the last N steps) or when `/persona` is explicitly on for coding. | S |

Gate: after P4.1 on this machine, `learning-audit.jsonl` shows non-zero `reinforce` and the core
is non-empty; recall injection rises from ~1.1 to 2–3 facts per turn with citation rate not below
31 %; the next prompt is accepted within 1 s of the previous turn ending.

### Phase 5 — persistence and update (week 10)

| Task | Fixes | Design | Size |
|---|---|---|---|
| P5.1 Restore that converges | S2 | Before `apply_tree`, compute worktree paths absent from the target tree and not ignored, remove them in the same journal phase, report the list; keep the post-restore verification. | M |
| P5.2 Verified, durable update | S4, S5, S6 | Publish `SHA256SUMS` (plus a minisign/ed25519 signature) with each release; verify the `.part` digest, `sync_all()`, then swap; pin backups for 7 days. `ring` already provides both primitives. | M |
| P5.3 Bounded sessions | S3 | `sessions_keep`/`sessions_max_bytes`; images moved to `sessions/blobs/<sha>` referenced by id; per-turn autosave appends a JSONL delta with periodic compaction into the v2 envelope. | L |
| P5.4 Cron and store hygiene | S7, S8 | Write the spec atomically before registering; job log owner-only; failures posted to the configured notify channel; `time gc` gains an unreachable-object prune using the OID-selected pack path. | S |

Gate: `/undo` succeeds on a run that created a new file; `aizen update` refuses a tampered asset;
a 200-turn session stays under 5 MB.

### Phase 6 — measurement (starts week 1, runs throughout)

| Task | Fixes | Design | Size |
|---|---|---|---|
| P6.1 `prompt-size --live` | RC2 | Per-block byte/token table of both lanes plus a lane-1 stability check between two consecutive builds. | S |
| P6.2 Replay harness | Q3 | `src/llm/replay.rs`: `AIZEN_TAPE=record|replay|strict`; one JSONL per task, one line per model call with an fnv1a64 fingerprint of scrubbed messages + tool names; replay matches by ordinal, warns on fingerprint drift, fails in `strict`. Uses the existing `run_agent_loop<F, Fut>` seam. | M |
| P6.3 Task suite | Q1, Q6 | `bench-tasks/<id>/{repo/, task.toml, tapes/}` with five tasks (fix-failing-test, fix-build-error, add-feature, refactor-with-tests, multi-file-wire) plus a zero-edit control, each a dependency-free cargo crate so `cargo test` takes ~2 s; runner builds the real registry rooted in a temp copy, verify gate ON, asserts `Done`, `cargo test` exit 0, changed files ⊆ `allowed_files`, steps and tokens ≤ baseline × 1.25, repeat-call rate < 2 %. `bench-fixtures/loop-baseline.json` mirrors the memory baseline pattern. | L |
| P6.4 CI gates | Q2, Q5 | `bench` job on ubuntu and windows running `bench loop` and `bench tasks --json`; weekly `cargo test --release` on windows; `clippy -D warnings` gating instead of `continue-on-error`. | S |
| P6.5 Coverage where it is zero | Q4 | One `src/repl/` smoke test driving a turn through `repl::turn` on a tape; the silent git-skip at `builtin.rs:6110` becomes a hard failure under `CI=true`; the two `#[ignore]` network tests become taped. | S |

---

## 4. What every release reports

| Metric | Now | Target after the roadmap | Source |
|---|---|---|---|
| Task suite verified-done | unmeasured | 6/6 | P6.3 |
| Wrong-file edits in the suite | unmeasured | 0 | P6.3 |
| Steps and tokens per task vs baseline | unmeasured | ≤ 1.25× | P6.3 |
| Lane-1 cache stability across turns | broken every turn | byte-identical | P6.1 |
| Cached input share on a 30-turn session | tools + system[0] only | > 80 % of prompt tokens | provider usage |
| Verify gate coverage | Rust, TS (typecheck) | Rust, TS, Python, Go, JVM, .NET, Makefile; tests when present | P0.4 |
| `smart`-tier false negatives in `cmd_guard` tests | 5 known | 0 | P0.5 |
| Memory `reinforce` events | 0 | > 0 and core non-empty | P4.1 |
| Next-prompt latency after a turn | up to minutes on 39 % of turns | < 1 s | P4.4 |
| Frame time at 3,000 blocks | O(session) | O(viewport) | P3.3 |
| Cold start / binary size | 10.8 ms / 34.1 MB | unchanged | CI |

---

## 5. Deliberately not changed

- Depth of delegation stays 1, the writer stays singular, unknown roles stay refused.
- No new tools, no new product areas, no editor integration, no cloud. This document is about
  making what ships correct.
- Persona, SOUL, hostbot platforms, and the reach channels are not touched beyond gating their
  prompt cost; a feature freeze on them for the duration is recommended.
- The `dense` embedding tier stays optional; the bench data that keeps it off is still valid.
- The Windows sandbox stays honestly `partial` here; a kernel-enforced backend is a separate
  project and does not block any phase above.

---

## Appendix A — status of prior plans (verified against code)

DONE (30): the prompt rewrite; PowerShell blocklist; canonical signature ring + 2-cycle latch;
per-signature nudge reset; productive-step definition; conditional auto-extend; verify re-fire
latch; relevance truncation; `result_is_error`; no-op write guard; auto-checkpoint before
destructive ops; sub-agent verify gate; multi-query fan-out and dedup; fetch cap split + TTL cache;
fuzzy recall measured before enabling; `ScopedTodo`; 19-scenario loop bench; all four P-ctx items;
slug re-key by canonical path; project-aware sessions; autosave error surfacing; `/where`; git-off-
PATH benign path; `/handoff` no longer clobbers; both lanes rebuilt on restore; the three
P0-harness-persistence items.

PARTIAL (2): `system_prompt_strict.md` lacks the confidence/hill-climb paragraph; sessions still
one flat directory with no writer lock.

OPEN (4): no test asserts prompt ↔ loop consistency; `confine(must_exist=false)` canonicalises the
parent only, by design; `enable_self_review` still off by default (P1.4 fixes); 6 of 10 §10
metrics unimplemented (P6.3 fixes).

REVERTED on purpose (2): DDG container parsing and the Marginalia keyless backend; search is
keyed-only now.

## Appendix B — companion document

`docs/plan-beyond-amp-2026-09-14.md`, written the same day, is a market positioning note against
Amp. It is not a prerequisite for anything here and can be deleted if unwanted.
