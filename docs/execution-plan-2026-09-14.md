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
`f1a9c6a`, `a08ac6e`) and E1.3 (`src/agent/result_format.rs`) on the same branch. Not yet: E1.9
(edit-tool polish), E1.10 (async diagnostics), E1.17 (Codex parity, first to cut).
Deviations worth knowing: (e) E1.2 keeps the full text of a collapsed result in a scratch spill
file, not in the read cache — the read cache stores fingerprints and a prefix, never bodies — and
collapses in batches of eight rather than one per step, because every mid-history rewrite busts
the prompt cache from that byte on. (f) E1.13 also accepts Mistral's `[TOOL_CALLS] [...]` array
and Python literals, and its gate is pinned by a scripted-loop test until a local-model tape
exists. (g) E1.3 `concise` is a shape applied only above a threshold (4,000 chars for a log, 40
rows for a search), so the common short result is byte-identical to before, and the full text is
spilled to the scratch dir rather than dropped. (a) E1.5 nudges retire at the START of the next run rather than at the
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
