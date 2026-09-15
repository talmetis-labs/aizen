# Aizen execution plan (2026-09-14)

This is the single ordered backlog for everything researched on 2026-09-14. Evidence lives in two
companion documents and is not repeated here:

- `aizen-quality-plan-2026-09-14.md` (QP) — the findings ledger, `file:line` for every defect,
  IDs like `L3`, `T1`, `C1`, `M6`, `U1`, `S2`, `O8`, `Q3`.
- `fast-lean-quality-2026-09-14.md` (FL) — measured baseline (67 KB fixed per request, p90 30
  calls per turn, 4.4 % of results at the cut) and the external evidence; levers `A`–`J`.

Status: proposal. Nothing applied. Estimates assume one person who knows the code, full time.
S = 1 day, M = 3 days, L = 6 days. The earlier "10 weeks" in the QP was written before the FL
additions; with them the honest total is **about 18 weeks**, and the plan is built so it can stop
after any phase and still have shipped something whole.

---

## 1. Goals and guardrails

Goal: the same binary, same tools, same seven agents, but the model reads true inputs, the prefix
caches, verification is real in every language, the operator sees what they approve, and every
change is measured on a task suite before it ships.

Hard constraints (unchanged): single static pure-Rust binary, rustls only, no C deps, startup
10.8 ms and 34 MB must not regress, depth-1 delegation, single writer.

Quality guardrail for every item: the task suite's verified-done rate and wrong-file-edit count
may not get worse than the baseline recorded in Phase 0. An item that regresses is reverted, not
argued about.

Working agreement: one branch per item off `dev` (`fix/<id>-<slug>` or `feat/<id>-<slug>`), one PR
into `dev`, `cargo test --bin aizen` green on three OSes, task suite green, docs updated in the
same PR when a user-facing behaviour changes. Commit messages follow the repo's own convention
and carry no tool attribution.

---

## 2. Phase 0 — measure, and stop feeding the model false inputs (3 weeks)

Everything later is gated on numbers this phase produces, so it goes first even though parts of it
are pure plumbing.

**Status 2026-09-15:** E0.1–E0.8 committed on branch `feat/e0.1-usage-ledger` as seven themed
commits (f570234..1bfd7a3). Full suite `cargo test --bin aizen` at that point: 1,865 passed, 0
failed, 2 ignored. `aizen prompt-size --live` on the branch: prefix STABLE (two volatile markers
before). Deviations from the rows below: E0.3 keeps the dynamic lane at index 1 and makes its
contents byte-stable instead of moving the lane (see the row); E0.7 ships the manifest coverage,
the missing-toolchain skip and the absence demand, but not yet the cost-ranked verify ladder or
the `/init`-written `verify.json` (those move to E1.10 / FL lever F).

**E0.9 / E0.10, same day, same branch (uncommitted at the time of writing):** the tape is hooked
at the three client functions every model call passes through, not at the `run_agent_loop` seam,
so sub-agents, chores and the REPL are taped too; two fingerprints per line (system prompt vs
conversation + tool names) so a drift report says which half moved. The task suite ships four
fixtures (`fix-failing-test`, `fix-build-error`, `add-feature`, `no-edit-control`); the two
remaining shapes from QP P6.3 (`refactor-with-tests`, `multi-file-wire`) are Phase 1 additions.
`task.json` instead of `task.toml` — the tree has no TOML parser and a fixture manifest is not
worth a new dependency. **No tape has been recorded yet:** the maintainer's machine had no
endpoint configured during the implementation session, so `bench-fixtures/loop-baseline.json`
does not exist and the CI `bench` job passes vacuously (every task SKIPPED) until
`aizen bench tasks --record` and `--update-baseline` are run against a real model and the tapes
are committed. The harness itself is proven by a synthetic tape in `bench::task_eval::tests`:
the replayed edit lands, the verify gate and the verify command run `cargo test` in the copy, and
the diff is exactly the allowed file.

| ID | Item | From | Files | Size | Depends on | Done when |
|---|---|---|---|---|---|---|
| E0.1 | Persist per-request `usage` (input, output, cached, cache-write) into session `meta`; show cached % in HUD and `/cost` | FL A, QP C2 | `core/session_store.rs`, `llm/client.rs:1681`, `ui/context_report.rs` | S | — | a session file carries a `usage[]` array; HUD shows `cache 0 %` today |
| E0.2 | `aizen prompt-size --live`: per-block bytes of both lanes plus a lane-1 stability check across two consecutive builds | QP P6.1 | `cli/run_cmds.rs` | S | — | command prints `dynamic lane: CHANGED` on the current code |
| E0.3 | Byte-stable dynamic lane. Implemented as: `<sessions>` rows carry the file's calendar day, no message count, never the conversation being autosaved, and are adopted once per conversation boundary; `<session_memory>` (rewritten by the learning pass between turns) moves to the user-turn fold beside recall and skills for the REPL, and is stripped with them at compaction; the persona `<self>` block is adopted at the boundary like the frozen core. The lane stays at index 1, so the existing breakpoints and every "two leading system messages" invariant hold unchanged — moving the lane behind the history (the original idea) was not needed once nothing in it varied per turn. | QP P0.3, FL A | `core/session_store.rs` (`recent_sessions_block`, `day_label`), `agent/prompt_lanes.rs` (`system_prompt_bundle_in`, `fold_context_into_query`, `strip_session_mem_prefix`, `refreshed_system_prompt_bundle`), `persona/mod.rs` (`ADOPTED_SELF`) | M | E0.1, E0.2 | `prompt-size --live` reports the prefix STABLE; a test proves the lane is byte-identical across an autosave and a session note; E0.1 shows cached % ≥ 80 % on the first call of a later turn against an Anthropic-compatible endpoint |
| E0.4 | One truncation authority for reads: drop `file_read` from relevance truncation and `"path"` from its keys; `budget_view` on every read branch including ranged, numbered, `read_many`; `truncate_relevant` re-appends the last ~200 chars | QP P0.1 (T1–T3) | `agent/mod.rs:3343-3360`, `:5012`, `agent/builtin.rs:1865-1936` | S | — | reading `src/agent/mod.rs` lines 400–900 returns a contiguous window with a real marker; a 200-match search still ends with its cap hint |
| E0.5 | Log and delegate budgets: `max_log_result_chars` (16 k, tail-weighted with an error-line anchor) for `shell_run`/`process`; `max_delegate_result_chars` (24 k, cut by `## ` section with a named omission) for `task`/`fanout`/`verify` | QP P0.2 (L3, L4, O1) | `agent/mod.rs:705`, `:3337`, `:5093` | S | — | a `cargo test` output with three failures reaches the model with all three `error[` blocks; a 9-finding nemesis report arrives whole |
| E0.6 | Structural command guard: tokenizer that splits on `\n`/`\r` and newline-in-quotes; inspect every token; allowlist split into pure readers and argument-gated (`find` without `-delete/-exec`, `cargo fmt --check`, `git branch/tag/remote` without mutating flags); drop `npm test`/`npm audit`; new "runs code, still ask" tier; fix the `&>` false positive | QP P0.5 (T4, T7, T8) | `agent/cmd_guard.rs:170`, `:183-212`, `:341-355` | M | — | `printf 'ls\nrm -rf x'` → Ask; `cargo build &> log` → allowed; `find . -delete` → Ask; existing guard tests green plus 12 new cases |
| E0.7 | Verify gate v2: detect `pyproject.toml`/`setup.py`, `go.mod`, `pom.xml`/`build.gradle`, `Makefile` targets, `*.csproj`; a one-shot demand when nothing is detected and files were edited; the verify ladder (LSP diagnostics → typecheck → narrowest test → full suite at Done for multi-file); `/init` writes `.aizen/verify.json` with detected commands and measured durations | QP P0.4 (L1, L2), FL F | `agent/verify_gate.rs:9-37`, `agent/mod.rs:1775`, `agent/codebase.rs` | M+ | — | a Python fixture with a failing pytest cannot reach `Done`; a Rust edit with a sibling test runs `cargo test module::` not the whole suite |
| E0.8 | Atomic config write with a rolling `.prev`; `/yolo` session-scoped with explicit `--persist` | QP P0.6 (S1, S9) | `core/cli_config.rs:1226`, `core/approval.rs` | S | — | killing the process mid-save leaves a loadable config; `/yolo` in one window does not arm the next |
| E0.9 | Replay harness: `AIZEN_TAPE=record\|replay\|strict`, one JSONL per task, fnv1a64 fingerprint of scrubbed messages + tool names, matched by ordinal | QP P6.2 (Q3) | new `llm/replay.rs`, seam `run_agent_loop<F, Fut>` | M | — | a recorded turn replays byte-identically offline; a prompt edit warns in `replay`, fails in `strict` |
| E0.10 | Task suite v1: three fixture crates (`fix-failing-test`, `fix-build-error`, `add-feature`) plus the zero-edit control; runner asserts `Done`, `cargo test` exit 0, changed files ⊆ allowed, steps and tokens ≤ baseline × 1.25, repeat-call rate < 2 %; `bench-fixtures/loop-baseline.json`; CI job on ubuntu + windows | QP P6.3, P6.4 (Q1, Q2) | new `bench-tasks/`, `bench/task_eval.rs`, `bench/metrics.rs`, `.github/workflows/ci.yml` | L | E0.9 | `aizen bench tasks --json` runs in CI and records the baseline the later phases are measured against |

Phase 0 gate (all must hold): E0.2 stable · E0.1 cached % visible · three `error[` blocks reach
the model · Python fixture cannot fake `Done` · multi-line `rm` is asked · suite baseline recorded.

---

## 3. Phase 1 — loop, client, lean (5 weeks)

**Status 2026-09-15:** implemented E1.1, E1.4, E1.5, E1.6, E1.7, E1.8, E1.11, E1.12, E1.14,
E1.15, E1.16 (commits `0807671`, `24c92a4`, `99c9a5f`, `b34bbb8` on `feat/e0.1-usage-ledger`),
then E1.2 (`src/agent/observe.rs`) and E1.13 (`src/agent/lenient.rs`) (commits `5b062f0`,
`f1a9c6a`, `a08ac6e`), E1.3 (`src/agent/result_format.rs`, `db4e8b4`) and E1.9 (edit ladder,
`compact_edit_diff`, `file_glob` `ignore`, `search_routing!`, `8447991`) and E1.10 (async LSP
fold with callers, `harness_check_after_edits`) on the same branch. E1.17 (Codex parity) was
**deferred by the maintainer on 2026-09-15** — parked, not cut: C5 (the 401 refresh loop has no
attempt counter) and C6 (overload markers matched against the whole body, model output
included) are a few dozen lines each and can be picked up on their own; C4 (streamed SSE on
the shared watchdog) is the L part.
Deviations worth knowing: (e) E1.2 keeps the full text of a collapsed result in a scratch spill
file, not in the read cache — the read cache stores fingerprints and a prefix, never bodies — and
collapses in batches of eight rather than one per step, because every mid-history rewrite busts
the prompt cache from that byte on. (f) E1.13 also accepts Mistral's `[TOOL_CALLS] [...]` array
and Python literals, and its gate is pinned by a scripted-loop test until a local-model tape
exists. (g) E1.3 `concise` is a shape applied only above a threshold (4,000 chars for a log, 40
rows for a search), so the common short result is byte-identical to before, and the full text is
spilled to the scratch dir rather than dropped. (h) E1.9 keeps `file_glob`'s default at "see
everything" — the maintainer asked for that explicitly when the confine guard was removed, and
the test pinning it says so — and adds `ignore: true` for `.gitignore` + heavy-dir pruning; the
diff compaction happens in the loop after the TUI has drawn the full diff, so the screen and the
model see different shapes on purpose. (i) E1.10 keeps a 300 ms inline wait so a warm TypeScript
or Go server still folds in the same result; only the slow case goes asynchronous. The caller
scan reports a file only against a baseline it already has, so a project's pre-existing error
wall is never dumped on the model. The harness check's gate ("the post-edit `cargo check` call
disappears from suite traces") stays unmeasured until tapes exist. (a) E1.5 nudges retire at the START of the next run rather than at the
end of the current one, because the loop bench and several tests read a run's nudges off its
history; the cache effect is the same (the new user turn rewrites the prefix from that point
anyway). Delivery role is `system` on Anthropic-style bases and a tagged user-turn message
everywhere else. (b) E1.8 keeps `persona_create` on the registry as a DEFERRED tool rather than
removing it — there is no slash path that mints a character, so removal would have broken the
"create a persona named X" flow; deferral takes its schema off the request all the same. (c) E1.8
built-in deferral is ON by default only for first-party APIs (`api.anthropic.com`,
`api.openai.com`): REFERENCE records a measured A/B on a hosted gateway that grammar-locks
tool-call names to the advertised set, where a deferred tool can never be called. `lean_tools`
in `cli-config.json` overrides either way. The six-largest-description trim was not needed for
the ≤ 55 KB gate (measured 53 KB lean on a coding turn) and was left alone: a tool description is
the model's instruction for that tool. (d) E1.1 single-turn compaction re-seats the prompt
verbatim after the boundary note; a failed compaction latches the cadence only on the second
consecutive failure (one blip should not silence compaction for a whole cooldown).

| ID | Item | From | Size | Depends on | Done when |
|---|---|---|---|---|---|
| E1.1 | Compaction for single-turn runs (`plan_compact_cut_at` on an assistant/tool boundary) and touchpoints prepended to every auto-compaction; a failed compaction does not arm `last_compact` | QP P1.1 (L5, L6, L12) | M | E0.10 | a 60-step single-prompt run compacts once and the path list survives |
| E1.2 | Age-based history collapsing (older than 8 observations → one-line digest, full text in read cache) and file-system spill for results > 16 KB | FL B | M | E0.4, E0.5 | suite tokens per task drop with pass rate unchanged |
| E1.3 | `format: concise \| detailed` on `shell_run`, `search_files`, `process`, concise default | FL B | S | E0.5 | — |
| E1.4 | Todo scoped to the conversation, cleared at the user-turn boundary unless the previous turn ended abnormally | QP P1.2 (L7) | M | — | a question after an edit turn costs zero todo-poke round-trips |
| E1.5 | Nudges that retire; on Anthropic gateways sent as mid-conversation `role: system` messages, elsewhere in the user turn | QP P1.3 (L8), FL A | S | E0.3 | — |
| E1.6 | Effort tiers with teeth: `apply_effort` maps tier → `max_iters`, continuations, verify attempts, self-review, log budget; `models_by_effort` so a tier can change the model | QP P1.4 (L9, L10) | S | — | `/effort low` on a local endpoint behaves differently from `/effort max` |
| E1.7 | `TurnShape` classifier (question · small edit · multi-file · research) gating recall, skills, self-review; `low` → cheap model | FL E | S | E1.6 | a pure question is answered in one request with no recall block |
| E1.8 | Frozen per-conversation deferred tool set chosen at turn 1 by `TurnShape`; `persona_create` off the model surface; schema ratchet run with LSP on; six largest descriptions trimmed | FL G, QP L13, T9 | S | E1.7 | `prompt-size` fixed block ≤ 55 KB on a coding turn and the tool list is byte-stable across the conversation |
| E1.9 | Edit tool polish: `replace_all` on every rung, `dry_run`, removed-lines-only diff capped at 3 hunks, `file_glob` honours `.gitignore` and prunes build dirs, shared routing sentence across the five search tools | QP P1.13 (T5, T6, T11) | M | E0.4 | — |
| E1.10 | Async workspace diagnostics folded into the edit result (no 3.5 s block, callers included); harness runs the fast check after a batch of ≥ 3 edits | FL C, QP T10 | M | E0.7 | the post-edit `cargo check` call disappears from suite traces |
| E1.11 | Usage recorded once at stream end from the last seen chunk; `include_usage` on the visual-contract path built via `build_chat_body` | QP P1.6 (C2, C3) | S | E0.1 | `/cost` populated on llama.cpp |
| E1.12 | Two-phase stall deadline (first chunk 600 s, inter-chunk 90 s) | QP P1.7 (C7) | S | — | a 3-minute silent reasoning turn is not replayed |
| E1.13 | Lenient tool-call recovery: JSON repair plus `<tool_call>`/fenced fallback only when native `tool_calls` is empty | QP P1.8 (C8) | M | E0.10 | local-model tape with a malformed call completes without an empty-reply retry |
| E1.14 | Capability strip-and-retry per model (`max_tokens`→`max_completion_tokens`, `parallel_tool_calls`, `tool_choice`, `cache_control`) driven by the 400 body | QP P1.9 (C9, C10) | M | — | o-series and strict local servers work without config edits |
| E1.15 | One token estimator (chars/4 Latin, chars/1.8 CJK or combining marks) shared by loop and memory caps | QP P1.11 (C11, M9) | S | — | — |
| E1.16 | Overflow re-arm (3 attempts) | QP P1.5 (L11) | S | — | — |
| E1.17 | Codex path parity: streamed SSE with watchdog, auth attempt counter, overload markers on the error envelope only | QP P1.10 (C4–C6) | L | — | **first to cut if the phase slips** |

Phase 1 gate: suite pass rate ≥ baseline with steps and tokens per task ≥ 20 % lower; fixed
block ≤ 55 KB; `/cost` on a local endpoint; single-turn compaction observed on a tape.

---

## 4. Phase 2 — the Pantheon (3 weeks)

**Status 2026-09-15:** E2.1 implemented on `feat/e0.1-usage-ledger` (`roles.pantheon` map and
`pantheon_endpoint` in `cli_config.rs`, differentiated `RoleProfile::default_max_steps`, workflow
children read the same profile, `/config` → Sub-agents → Pantheon roles). Deviation (j): no static
`RoleProfile::model` field — a compile-time model name would be a vendor pin inside a binary that
points at any endpoint, so the config map (plus the existing `AIZEN_<ROLE>_MODEL` env) is the
whole feature. Workflow children without `max_steps` now take the role's default (argus 15 …
daedalus 45) instead of the flat 30; a specialist card keeps 30. The "done when" is pinned by
`resolve_dispatch_pins_a_role_to_its_pantheon_model` and
`role_tasks_reach_their_pantheon_model_in_one_workflow`. E2.2 implemented
(`src/agent/context_pack.rs`, `read_cache_recent` in `agent/mod.rs`, `context` on `task` and on
workflow tasks, `todo::active_item`). Deviation (k): the pack rides INSIDE the child's one user
message ahead of the brief rather than as a second user message — strict providers reject two
consecutive user turns — and the reading list is "this conversation" (the parent's read-cache
scope, newest first) rather than "this turn": the store carries no turn stamp, is cleared on
compaction, and records only turns with no destructive call, so it is a best-effort list, never
a promise. The "done when" (no re-reads of the parent's files in fan-out traces) stays
unmeasured until tapes exist. E2.3 implemented (`after`, `waves`, `schedule`, `retry_on_fail`,
`verdict_failed` in `workflow.rs`; `implement` mode in `workflow_tool.rs`;
`bench-fixtures/workflows/implement.json`). Deviation (l): the fix loop is a generic
`retry_on_fail` edge any spec can use rather than preset-only logic, so the CLI spec file gets
it too; within a wave the readers run BEFORE the writer (a consistent pre-edit snapshot — a
review that must see the change says `after`); both attempts of a retried task stay in the
trace as `id` and `id#2`. The "done when" (a themis FAIL triggers exactly one daedalus retry)
is pinned by `schedule_runs_waves_in_order_and_fix_loops_once` against a scripted runner; the
unattended live run of `aizen workflow implement.json` waits for an endpoint. E2.4 implemented
(`src/agent/architect.rs`, `repl::turn::architect_phase`, hooks in both REPLs and `aizen agent`,
`architect_mode` config + `AIZEN_ARCHITECT` env). Deviation (m): the "themis" leg is the turn's
own verify gate and post-edit harness check rather than a third child — a separate themis would
re-run the same commands — and the editor phase keeps the `max` tier's harness budgets, dropping
only the wire `reasoning_effort` to `low`, because a multi-file change on `low`'s twelve steps
would be cut off mid-plan. The "done when" (a suite multi-file task passes with lower wall-clock
than the single-model run) stays unmeasured until tapes and a second model exist. E2.5
implemented (`workspace_txn::reentry_allowed` + `acquire_scoped`, `src/agent/blackboard.rs`,
workflow child scopes derived under the parent's). Deviation (n): the lease is not strictly
non-reentrant across parent/child — the loop holds its lease from the first edit to the end of
the run, so a strictly non-reentrant child would time out on every "edit, then delegate daedalus"
turn. Reentry follows scope ancestry instead: a descendant reenters, a sibling waits, which is
the exclusion the QP's O6 wanted and which two `serve` lanes on one worktree never had. The
board is per conversation (not per workflow run) and holds finished reports, not live notes: a
read-only child cannot write files, so the harness files each child's report for it. E2.6
implemented (`ExecutionContext::dispatch_label` prefixed in every `approve` path,
`AgentConfig::step_note` → `orchestration::note_step`, `add_usage` + `tokens_in/out` on
`TaskOutcome` and the board row, one retry with `tightened_brief` for a `Deadline` /
`VerificationFailed` writer and auto-restore of a pre-dispatch checkpoint). Deviation (o): the
checkpoint restored is one the `task` tool takes itself before a write-capable dispatch
(`timemachine::save`, tree-deduplicated), not the loop's pre-edit note — that note is
process-global and, in a turn that already edited, points at the PARENT's pre-edit tree; and
the retry is for `task` dispatches only, since workflow children already have the wave-level
`retry_on_fail` and a second mechanism there would retry twice. E2.7 implemented (delegation
bullet in `system_prompt.md`, `task_tool::thin_brief` on `task` and on workflow tasks, the CLI
runner's synthesis cap). **Phase 2 is code-complete** on `feat/e0.1-usage-ledger`; its gate
(`aizen workflow implement.json` unattended, `/workflows` per-child tokens, attributed approvals)
is observable only with an endpoint, which this machine does not have.

| ID | Item | From | Size | Depends on | Done when |
|---|---|---|---|---|---|
| E2.1 | Per-role `model` and differentiated budgets (argus 15, clio 20, metis/nemesis/mnemosyne 25, themis 30, daedalus 45); `[roles.pantheon]` config map; fix the mnemosyne brief whitespace | QP P2.2 (O3, O4, O13) | S | — | nemesis runs on a different model than argus in one workflow |
| E2.2 | Child context pack: ≤ 15 `path:line` entries the parent read, ≤ 10 established findings via a `context` arg, the active todo; cap 2,500 chars | QP P2.3 (O2) | M | E0.4 | fan-out traces show no re-reads of files the parent already read |
| E2.3 | Chained workflow: `after` on tasks, Kahn-ordered waves, singular writer per wave, read-only siblings never share a wave with the writer; workflow writers get the verify gate; `implement` preset daedalus → themis → nemesis with one fix loop | QP P2.4 (O5, O8, O9) | L | E2.1, E0.7 | `aizen workflow implement.json` on a fixture runs unattended; themis FAIL triggers exactly one daedalus retry |
| E2.4 | Architect mode under `/effort max` for multi-file tasks: metis (strong, high effort) → daedalus (fast, low effort) → themis | FL D | M | E2.3, E1.6 | suite multi-file task passes with lower wall-clock than the single-model run |
| E2.5 | Write lease non-reentrant across parent/child; sibling blackboard under `scratch::dir()/blackboard/<run>/` listed in `<environment>` | QP P2.5 (O6) | M | E2.3 | — |
| E2.6 | Attribution, cost, live status: `dispatch_label` in approvals and `/workflows`; `tokens_in/out` per child; `Track::note(step, tool)`; one retry with a tightened brief for a `Deadline`/`VerificationFailed` writer, auto-restore of the pre-edit checkpoint if that fails too | QP P2.6 (O7, O10, O11) | M | E0.1 | an approval raised by a child names the child; `/workflows` shows per-child tokens |
| E2.7 | Delegation guidance in the prompt (when not to, brief shape, nemesis reads / themis runs); brief-quality refusal under ~80 chars with no path or symbol; CLI runner uses the same synthesis cap | QP P2.7 (O12, O14) | S | — | — |

---

## 5. Phase 3 — the operator surface (3 weeks)

**Status 2026-09-15:** E3.1 implemented on `feat/e0.1-usage-ledger` (`Tool::preview` →
`ApprovalPreview`; previews on `file_edit` (its own dry-run), `file_write`, `shell_run`,
`file_move`; `approve` renders the patch through the existing diff box and the rows as faint
lines above the question, on the TUI, on a plain terminal and in the Telegram message;
`rel_path_display` in headers and diff-box titles). Deviation (p): the preview is not a new
overlay panel — it is emitted into the transcript directly above the existing 4-row menu, which
keeps the menu code untouched and the payload scrollable; and no preview is drawn under a session
allow-all, where no question is asked. E3.2 implemented (`core::approval::Grant`, session grants
from two new menu rows, `.aizen/approvals.json` project allowlist, `granted` checked in the
executor before the question, `/approval grants`). Deviation (q): the menu keeps the `Stop` row
and the session allow-all row rather than replacing them (six rows, `t`/`d` accelerators), so
the existing muscle memory (`y`/`a`/`n`/Esc) is unchanged; a grant auto-approval is always
printed, never silent. E3.3 implemented (`RenderCache` LRU + height table, `draw_transcript`
windowed on prefix sums, `rows_offset` through `TranscriptGeom`, `shift_selection`, and
`InjectCtx.rows_offset`). Deviation (r): the "under 10 ms per frame at 3,000 blocks" gate is
pinned structurally — a frame after the first renders at most the window's blocks
(`a_frame_renders_the_viewport_not_the_session`) — rather than by a timing assertion, which
would flake on CI; the first frame after `/resume`, a resize or a theme switch still measures
every block once. E3.4 implemented (`ToolEvent.body`, auto-expanded tail on a failed row,
`Ctrl-E` → text overlay via `row_tool_seq` / the kept bodies, `send_retry_note` caption + note).
Deviation (s): the expand key is `Ctrl-E`, not `Ctrl-O` — `Ctrl-O` has been the screenshot key
since the vision work and is documented as such; and a failed row expands INLINE (its last lines
under the digest) rather than popping an overlay mid-turn, which would steal the input box. E3.5
implemented (`rewind` alias, `timemachine::undo_target` / `working_tree_differs_from`, the stat
before the restore, `--yes` on a dirty tree, `emit_patch_boxes` for `/diff --patch`). Deviation
(t): the dirty-tree confirmation is a second command (`/undo --yes`) rather than a modal y/n —
the approval menu's rows and grants are about tools, and a slash handler has no clean inline
prompt on the plain REPL; "dirty" means the working tree differs from the current checkpoint,
measured with the same diff the stat uses, not `git status`. E3.6 implemented
(`retained::output_quiet_for` + `OUTPUT_QUIET_SECS` in the idle check; `char_chunks` by display
width) and E3.7 implemented (`warm_up_after_first_frame` in `main.rs`: a background thread after
the retained surface is up — LSP runtime + one `documentSymbol` probe per language present
(`discovery::probe_files`, roots inside the project only), then an incremental `/init` refresh when an index exists).
Deviation (u): the warm-up refreshes only an EXISTING index; it never builds one unasked,
because a first `/init` is a user decision (it scans and redacts the whole repo).
**Phase 3 is code-complete** on `feat/e0.1-usage-ledger`; its gates (frame time at 3,000
blocks, a visible 429, first-edit diagnostics) are pinned structurally or wait for a terminal
session on a real endpoint.

| ID | Item | From | Size | Depends on | Done when |
|---|---|---|---|---|---|
| E3.1 | Pre-flight approval payload: `Tool::preview` with the patch computed before any write, full argv + cwd for shell, repo-relative path in the header | QP P3.1 (U1, U2, U12) | M | E1.9 (`dry_run`) | approving a `file_edit` shows the patch first |
| E3.2 | Grant scopes: always for tool · always for tool under dir · project allowlist in `.aizen/` | QP P3.2 (U9) | M | E3.1 | — |
| E3.3 | Transcript render windowing and a real LRU keyed by block id | QP P3.3 (U3) | S | — | 3,000-block session paints under 10 ms per frame |
| E3.4 | Expandable tool results (`Ctrl-O` on a tool row, auto-expand on non-zero exit); retry and rate-limit captions in the HUD | QP P3.4 (U4, U6) | M | — | a 429 with `Retry-After` is visible |
| E3.5 | Reviewable rewind: `/rewind` alias, diff stat before applying, confirm on a dirty tree, `/diff --patch` through the diff box | QP P3.5 (U5, U7, U8) | S | — | — |
| E3.6 | Screensaver only after idle **and** no output for 60 s; CJK-safe wrapping | QP P3.6 (U10, U11) | S | — | — |
| E3.7 | Background warm-up of LSP servers and the `/init` index after the first frame | FL H | S | — | first edit of a session gets diagnostics; startup unchanged |

---

## 6. Phase 4 — memory that earns its tokens (2.5 weeks)

**Status 2026-09-15:** E4.1 implemented on `feat/e0.1-usage-ledger`
(`consolidate::find_duplicate`: lexical at `learn_dedup_threshold`, then MinHash ≥ 0.60 AND
normalised-token ≥ 0.55 for the 0.45–0.78 band, the stage written on the audit line;
`consolidate::plan_pass` + `aizen memory consolidate [--apply]`, a model-free store-wide pass
that retires each duplicate revivably and reinforces its survivor). Deviation (v): the check
widens across TIERS, not across partitions of the same tier — the same sentence at two anchors
or two devices stays two facts (the `apply_store_never_merges_across_places` contract holds),
while a sentence held in another tier is a classification artefact (M3) and reinforces that
row. Deviation (w): `aizen memory consolidate` did not exist (the plan says "re-run" it;
`reconcile` is the model-judged pass), so it was written as a local pass and the one-time merge
needs no endpoint. Live-store run (2026-09-15, backed up first): 466 entries / 424 live, 4
merges over two rounds (one verify-pipeline fact written four times under a ping-pong of
`supersedes`; retiring a twin un-hides the row it had buried, hence the rounds), the survivor
now `sessions: 3`, audit `reinforce` 0 → 4. The frozen core is unchanged (1 entry): the
survivor is a `place` fact of another project. The 0.45 band floor does not reach the
Vietnamese restatement cluster (peak lexical 0.44, see `match_text`) — those stay
`reconcile`'s to judge. E4.2 implemented (`tiering::mentions_project` and
`TierProposal.mentions_project`; `decide` re-files a `user` proposal that names the work as
`place` with the usual clamp; the stored type follows the tier; `bench profile` gains
`bench-fixtures/tier-hints.jsonl`, 14/14 with the 8 profile cases still green). Deviation (x): existing rows are not
re-filed — their project is not recoverable from the text and a guessed anchor would be a wrong
one — so M3's 239 `user` rows stay until touched; the rule guards the write path from here on.
E4.3 implemented (`memory::gate_coverage`: IDF-weighted over the `GATE_QUERY_TERMS = 6`
heaviest query tokens, covered by any of the top `GATE_TOP_HITS = 3`; the index comes back from
the same scoped search via `search_scoped_with_index`, no second load; `tokenize` emits an
identifier's words next to the identifier — snake, kebab and camel; `bench memory` prints a
gate-admission line per split with a threshold sweep). Measured on the fixtures after the
change: ranking unchanged (`GATE: PASS` vs baseline; gate recall@5 1.000, paraphrase 0.769);
at 0.34 the gate admits 10/10 on the gate split and 13/18 on tune, every admission with an
acceptable fact in the top 3, 2 correct queries refused; the sweep admits one more correct query
at 0.20 and refuses none wrongly at any threshold — but the fixtures carry no negative queries,
so the over-injection side (the reason the gate exists) is not measurable here and the
threshold stays at 0.34 until live audit data says otherwise. The live gate
(2–3 facts per turn at ≥ 31 % citation) needs sessions on a real endpoint to measure.
E4.4 implemented (`repl::learning_queue`: `finish_turn` spawns the secretary + reflection on
a copy of `last_turn_slice`; `drain_learning_before_turn` joins ≤ 5 s at turn start,
`drain_learning_before_exit` ≤ 60 s on quit/EOF with Esc; `chore_call_timeout` 60 s,
`AIZEN_CHORE_CALL_SECS`, never above the sub-agent ceiling; `summarizer_endpoint` falls back
to `models_by_effort.low`). Deviation (y): auto-compaction stays inline — it rewrites the
history the next prompt is built from — under the same 600 s block, now with a 60 s cap per
summary call. The "next prompt < 1 s" gate is structural (nothing model-bound remains between
`finish_turn` and the prompt except a compaction that fires only over the context threshold)
and is measured on a real endpoint. E4.5 implemented (`session_store::read_session_brief`
rows cached per file by `(mtime, len)`; `store::load_from` keeps a per-directory
`(fingerprint, entry)` map and parses only new or changed files — `write_atomic` renames are
what invalidate a row; both keyed by directory so a test home gets its own).
E4.6 implemented (`self_mem::save_insight` returns the id of a near-duplicate live insight —
same folded text or content-token Jaccard ≥ 0.75 — instead of writing again;
`project_context::load_project_context` reads both `AGENTS.md` and `CLAUDE.md` per directory
and records the `(mtime, len)` of every path it probed, keyed by cwd; `conventions_changed`
is asked in `refresh_dynamic_prompt_lane` and rebuilds both lanes only on a change).
Measured cost: with this repo's own `CLAUDE.md` (7,092 B) now in `<project_context>` beside
the 787-byte `AGENTS.md`, `prompt-size` in this checkout reads lean 52.8 KB → 58.5 KB and
fixed 65.1 KB → 72.0 KB. The Phase 1 ≤ 55 KB lean gate was measured with the pointer file
alone; the growth is the repository's instruction file, not aizen's overhead. Deviation (z):
that gate is exceeded in this checkout by design of E4.6 — whether to re-baseline it (≈ 60 KB)
or cap `<project_context>` lower is the maintainer's call; nothing here truncates a user's
instructions silently. E4.7 implemented (`persona::suppress_for_turn` guard raised by
`refresh_dynamic_prompt_lane` when `persona_gate_applies(shape, previous_turn_used_tools,
keep)`; `build_system_prompt_bundle` leaves soul/persona/self out while it is up;
`turn_shape::current_turn_shape` is this turn's un-widened shape; `persona_for_coding` +
`/persona coding on|off`; the gate is raised only on the REPL's per-turn path, so hostbot
lanes keep their persona). **Phase 4 is code-complete** on `feat/e0.1-usage-ledger`; its
live gates (facts per turn, citation rate, next-prompt latency) wait for sessions on a real
endpoint.

| ID | Item | From | Size | Depends on | Done when |
|---|---|---|---|---|---|
| E4.1 | Two-stage dedup (lexical 0.78 across tiers, then MinHash 0.6 + containment); one `consolidate --apply` on the existing store | QP P4.1 (M1, M2) | M | — | `learning-audit.jsonl` shows `reinforce` > 0; core non-empty |
| E4.2 | Tier classification rule: project names/paths → `project`, never `user` | QP P4.2 (M3) | S | E4.1 | — |
| E4.3 | Recall gate: IDF-weighted coverage over top-3 with `min(|q|, 6)` denominator; identifier splitting in the tokenizer; re-tune on `bench memory` | QP P4.3 (M4, M5) | M | — | 2–3 facts injected per turn at ≥ 31 % citation |
| E4.4 | Post-turn learning off the critical path (queued, drained at next turn start with a 5 s join); chores capped at 60 s on the cheapest configured model | QP P4.4 (M6) | M | E1.6 | next prompt accepted < 1 s after a turn |
| E4.5 | Caches on the prompt-build path (session rows by mtime/len, `load_all` per process) | QP P4.5 (M7) | S | — | — |
| E4.6 | `is_near_duplicate` in `save_insight`; `CLAUDE.md` honoured with `AGENTS.md`; refresh on mtime | QP P4.6 (M8, M10) | S | — | — |
| E4.7 | Persona/self/soul injected only on non-tool-bound turns unless explicitly enabled for coding | QP P4.7 (M11) | S | E1.7 | — |

---

## 7. Phase 5 — persistence and update (1.5 weeks)

**Status 2026-09-15:** E5.1 implemented on `feat/e0.1-usage-ledger` (`apply_tree` stages the
worktree with `add -A` into the seeded temporary index — a checkpoint's own coverage — then
`paths_absent_from` lists what the target tree lacks, `restore_in_reported` writes that list
into the journal before `read-tree --reset -u`, the post-restore verification is unchanged,
and `restore_with_report` / `undo_with_report` / `redo_with_report` hand the list to the CLI
and to `/undo`). Pinned by an end-to-end test on a scratch repository (`git init`, two
checkpoints, an untracked file between them, restore back and forward). This is the bug the
2026-08-17 note recorded as "restore cannot delete an untracked file".

| ID | Item | From | Size | Done when |
|---|---|---|---|---|
| E5.1 | Restore removes files absent from the target tree (journaled, reported) | QP P5.1 (S2) | M | `/undo` succeeds after a run that created a file |
| E5.2 | `SHA256SUMS` (+ minisign) per release; verify, `sync_all`, then swap; backups pinned 7 days | QP P5.2 (S4–S6) | M | `aizen update` refuses a tampered asset |
| E5.3 | Bounded sessions: `sessions_keep`/`sessions_max_bytes`, images as content-addressed blobs, JSONL deltas with periodic compaction | QP P5.3 (S3) | L | a 200-turn session stays under 5 MB |
| E5.4 | Cron spec written before registration, owner-only log, failures to the notify channel; `time gc` prunes unreachable objects | QP P5.4 (S7, S8) | S | — |

---

## 8. Phase 6 — continuous measurement (starts in Phase 0, never ends)

| ID | Item | From | Size | When |
|---|---|---|---|---|
| E6.1 | Suite grows to six tasks (`refactor-with-tests`, `multi-file-wire`); local-model tapes added | QP P6.3 | M | end of Phase 1 |
| E6.2 | Weekly `cargo test --release` on Windows; `clippy -D warnings` gating; `src/repl/` smoke test; the silent git skip fails under `CI=true` | QP P6.4, P6.5 (Q4, Q5) | S | Phase 1 |
| E6.3 | Prompt A/B on the suite: cut base-prompt sections that do not change behaviour; tool guidance in one place | FL I | S | after E6.1 |
| E6.4 | Session stats script (`tool calls per turn`, results at the cut, repeats) checked into `bench/` and run on release | FL §1.2 | S | Phase 1 |

---

## 9. Dependency spine

```
E0.1 usage persisted ──► E0.3 stable lane ──► E1.5 nudges · E2.6 child cost
E0.2 prompt-size --live ┘
E0.9 replay ──► E0.10 suite v1 ──► every "no regression" gate from Phase 1 on ──► E6.1/E6.3
E0.4 read truncation ──► E1.2 collapsing · E1.9 edit polish ──► E3.1 approval preview
E0.7 verify v2 ──► E1.10 harness-run check · E2.3 workflow verify
E1.6 effort tiers ──► E1.7 TurnShape ──► E1.8 deferred set · E4.7 persona gating
E2.1 role model ──► E2.3 chained workflow ──► E2.4 architect mode · E2.5 lease
```

Items with no arrow into them (E0.4–E0.8, E1.4, E1.12, E1.14–E1.16, E3.3–E3.6, E4.1, E4.3,
E4.5, E4.6, E5.x) can be picked up whenever a dependency-bound item is blocked.

---

## 10. The first ten working days, concretely

| Day | Work |
|---|---|
| 1 | E0.1 usage persistence + HUD cached %; E0.2 `prompt-size --live` |
| 2–4 | E0.3 stable dynamic lane; verify with E0.1/E0.2 against a real endpoint |
| 5 | E0.4 read truncation authority; E0.5 log and delegate budgets |
| 6–8 | E0.6 structural cmd_guard with the new allowlist tiers and test cases |
| 9–10 | E0.8 atomic config + session yolo; start E0.9 replay harness |
| 11–15 | E0.7 verify gate v2 + ladder + `verify.json` |
| 16–18 | E0.9 finish; E0.10 suite v1 with three fixtures, baseline recorded, CI job |

---

## 11. Decisions the maintainer owns

These change user-visible behaviour or cost money, so they are asked, not assumed.

1. **Allowlist tightening (E0.6).** `cargo fmt`, `npm test`, `find -delete`, `git branch -d` will
   start asking under `smart`. Confirm the tier list before it ships.
2. **`/yolo` becomes session-scoped (E0.8).** Anyone relying on the persisted flag will notice.
3. **`adaptive_effort` default and `models_by_effort` defaults (E1.6).** Which cheap model is the
   `low` tier and the chores model, per provider.
4. **Docker as a dev-time dependency of the task suite (E0.10).** The shipped binary is unaffected.
5. **Raising the schema ceiling once, deliberately (E1.8),** after measuring with LSP on.
6. **Feature freeze on persona, SOUL, and the reach channels** for the programme's duration.
7. **Code signing and checksum signing keys (E5.2).** Minisign key custody.
8. **Whether to delete `plan-beyond-amp-2026-09-14.md`.** It is not a dependency of anything here.

---

## 12. Cut lines

- If Phase 0 runs past 4 weeks: ship E0.1–E0.8 as a release on their own; the measurement items
  continue in Phase 1.
- If Phase 1 slips: cut E1.17 (Codex parity) first, then E1.14, then E1.3.
- If Phase 2 slips: ship E2.1, E2.2, E2.6 and defer the wave scheduler (E2.3) with E2.4/E2.5.
- Phases 3–5 are independent of each other and can be reordered by whatever hurts most at the
  time; Phase 4 first if memory bloat crosses the 500 cap before then.

---

## 13. Out of scope, on purpose

New tools, new product areas, editor integrations, cloud threads, the Windows kernel sandbox
backend, package-manager distribution and code signing beyond the checksum in E5.2. The Amp
positioning note lists those; none of them is a prerequisite for anything above.
