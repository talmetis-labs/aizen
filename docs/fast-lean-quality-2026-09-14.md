# Fast, lean, and still correct — research note and design (2026-09-14)

Companion to `aizen-quality-plan-2026-09-14.md`. That document lists defects. This one answers a
different question: how does an agent get **faster and cheaper per task without losing quality**,
what does the published evidence say works, and what does aizen measure today. Numbers marked
*measured* come from this checkout (debug build of `prerelease/v0.6.7`) or from the 78 session
files on this machine; everything else cites a source at the end.

## 0. Thesis

Speed, leanness and quality are not three levers. They are one: **fewer round-trips, a smaller
resend per round-trip, more signal per token the model reads, and a verification step that closes
the loop so the model stops when the work is actually done**. Every technique below serves at
least two of the three at once, and the ones that trade one for another are marked.

## 1. Measured baseline

### 1.1 What every request resends (`aizen prompt-size`, this checkout)

| Block | Bytes | Note |
|---|---|---|
| Stable system lane (base prompt + `<environment>` + project context) | 15,950 | date is process-fixed (`OnceLock`), so this lane is byte-stable within a session: good |
| Dynamic system lane (soul, persona, self, user_memory, skills, sessions) | 8,881 | inserted at index 1 and changes every turn (RC2 in the quality plan) |
| Tool schemas, 44 tools | 42,166 | avg 958 B; the in-repo ratchet allows 31,000 B because it measures with LSP off |
| **Fixed per turn** | **66,997 (~16.7 k tokens by aizen's chars/4 estimate)** | before the first user word; the Anthropic tokenizer on JSON/code will count more |

Largest schemas: `workflow` 2,275 B, `task` 2,229, `process` 1,399, `todo_write` 1,338,
`search_files` 1,312, `persona_create` 1,306, `checkpoint` 1,293, `memory_save` 1,270,
`checkpoint_view` 1,266, `web_search` 1,217. `persona_create` and `workflow` ride every coding turn.

### 1.2 What a real turn looks like (78 sessions, 229 user turns, 2,562 tool calls)

| Metric | Value |
|---|---|
| Tool calls per user turn | mean 11.2 · median 4 · p90 30 · max 165 |
| Tool result size (chars) | median 188 · p90 2,709 · max 18,487 |
| Results sitting exactly at the 4,096 cut | 112 (4.4 %) |
| Assistant text per message (chars) | median 84 · p90 882 |
| Identical call repeated within one turn | 118 (4.6 % of calls) |
| Call mix | `shell_run` 600 (23 %) · `file_read` 289 · `file_write` 263 · `process` 248 · `search_files` 207 · `file_edit` 148 · `file_glob` 144 · `multi_edit` 44 |
| Read : edit ratio | 0.7 (the agent writes more than it reads) |

Two things stand out. The model is already terse (84 chars median), so output tokens are not the
problem. The p90 turn is 30 calls, which is **31 model requests**, each resending the fixed block
plus a growing history: input tokens, not output tokens, are where the money and the wall-clock go.

### 1.3 Where the wall-clock goes that is not the model

| Source | Cost | Where |
|---|---|---|
| Post-turn learning (secretary → reflection → auto-compact) runs inline before the next prompt | fires on 39 % of turns, on the main coding model at max effort, up to 600 s | `repl/turn.rs:302-320` |
| Prompt build re-reads up to 8 session files (400–750 KB each) and all 466 memory files | every turn | `session_store.rs:743`, `memory/store.rs:373` |
| LSP `edit_feedback` blocks the tool thread | up to 3.5 s per edit, nothing on the first edit | `lsp/mod.rs:459` |
| Stream stall deadline measured to first useful chunk | a reasoning model silent > 90 s is replayed up to twice | `llm/client.rs:581` |
| Codex path buffers the whole response | no incremental output | `responses_codex.rs:329` |
| Retry sleeps print nothing | user kills the turn | `llm/client.rs:740` |

### 1.4 What we cannot measure yet

Usage (`prompt_tokens`, `cached_tokens`) is parsed per response but never persisted: session
`meta` has `created, model, project_key, project_root, project_slug, updated` and nothing else. The
cached share of input, the single most important efficiency number, has no history on this
machine. Making it visible is the first task below.

### 1.5 The cost of the cache bust, estimated

Correction made while implementing E0.3: lane 1 is rebuilt at each **user turn**, not at each
model call. Within a turn the rolling breakpoint on the last assistant/tool message still lets
calls 2…N read the prefix, so the loss is one full re-write of the transcript per user turn, not
per call. An earlier draft of this section multiplied by the call count and overstated the effect.

Assumptions: history of 30 k tokens at the start of a turn, Sonnet 5 at $2 / M input, cache read
0.1×, cache write 1.25× (Fable 5.1 reads at 0.025×, which makes the gap wider).

| First request of a user turn | Token-equivalents billed on the 30 k history | ≈ $ |
|---|---|---|
| Today (lane 1 differs → the transcript after it is re-written) | 30 k × 1.25 = 37.5 k | 0.075 |
| After a byte-stable prefix (the transcript is a cache read) | 30 k × 0.1 = 3 k | 0.006 |

About 35 k token-equivalents saved per user turn, so on a 30-turn session with a growing
transcript roughly 1 M token-equivalents (≈ $2 on Sonnet 5, ≈ $5 on Opus 5), plus a shorter prefill
before the first token of every turn. Real but smaller than a per-call bust would be; the
per-call economics are already right because the rolling breakpoint works. Item A adds the
measurement (per-turn cached %) that replaces this estimate.

## 2. What the evidence says works

| Source | Finding | Number |
|---|---|---|
| Manus, context engineering | KV-cache hit rate is "the single most important metric" for a production agent; stable prefix, append-only context, no timestamps, deterministic serialisation; **mask tools, do not add or remove them mid-run**; put large observations in files and keep the path; recite the plan (todo) into recent context; keep error traces | cached 10× cheaper than uncached; ~50 tool calls per task; 100:1 input : output ratio |
| SWE-agent (ACI paper) | Interface design alone, at a fixed model, moved SWE-bench Lite from 11.0 % (bash only) to 18.0 % | file viewer 100 lines 18.0 % vs full file 12.7 %; summarised search 18.0 % vs iterative 12.0 %; linter-guarded edits 18.0 % vs 15.0 %; keep only the last 5 observations in full 18.0 % vs full history 15.0 %; recovery after a failed edit drops from 90.5 % to 57.2 % as failed edits accumulate |
| Aider, architect/editor | A strong model writes the plan in prose, a second model turns it into edits; each attends to one job | o1-preview + Sonnet 82.7 % vs o1-preview alone 79.7 %; Sonnet + Sonnet 80.5 % vs 77.4 % solo; GPT-4o 75.2 % vs 71.4 % |
| Anthropic, writing tools for agents | Consolidate tools; paginate, range-select, filter, truncate with sensible defaults; a `concise` / `detailed` response format; descriptions written like onboarding a new hire are "one of the most effective methods" | Claude Code caps a tool response at 25,000 tokens; concise format saved ~⅓ of tokens |
| Anthropic, context engineering | Just-in-time retrieval of file paths and identifiers over pre-loading; compaction that keeps decisions and open issues; structured notes outside the context; sub-agents return 1,000–2,000-token summaries | "the smallest set of high-signal tokens" |
| Anthropic, prompt caching | Reads 0.1× base (Fable 5.1: 0.025×); writes 1.25× (5-min) / 2× (1-hour); mid-conversation `role: system` messages on Opus 5 / Fable are the cache-safe operator channel | two requests break even on 5-min TTL |
| Claude Code best practices | Give the agent a check it can run and make it show the evidence; plan only for multi-file or unfamiliar changes ("if you could describe the diff in one sentence, skip the plan"); subagents for investigation so reads stay out of the main context; a fresh-context reviewer is less biased than the writer; `/clear` between tasks; lower effort means fewer, more consolidated tool calls | — |
| 2026 harness survey (arXiv 2606.20683) | Verification in the loop is the primary limiter for coding agents; richer observations raise grounding but cost context; heterogeneous models per role is the emerging pattern | 70B → 405B cost 4.4× compute for +2.6 MMLU |
| mini-swe-agent | Bash-only, ~100-line agent scores > 74 % on SWE-bench Verified | the harness must not get in the model's way |

Read together: **compress observations aggressively but losslessly, keep the prefix stable, run a
check the model can read, and let a strong model think while a fast model types.**

## 3. The programme for aizen

Ten levers, each with the evidence, what aizen does today, the change, and where it sits relative
to the quality plan (QP). Sizes are S ≤ 1 day, M 2–4 days, L a week, for one person.

### A. Cache-first request shape (speed + cost; no quality trade)

- Evidence: Manus; Anthropic caching.
- Today: RC2 busts the prefix every turn; the tool list is built once per turn and MCP
  `list_changed` is applied only at a fresh turn (good); `<environment>` date is process-fixed
  (good); usage is never persisted (§1.4).
- Change: QP P0.3 (byte-stable dynamic lane behind the history). **New:** persist per-request
  `usage` (input, output, cached) into the session `meta` and show *cached %* in the HUD and
  `/cost`; on the Anthropic gateway send nudges as mid-conversation `role: system` messages (the
  sanctioned channel), elsewhere in the user turn, never by editing earlier history (QP L8).
- Effect: §1.5. Size S beyond P0.3.

### B. Lossless observation compression (all three)

- Evidence: SWE-agent (every ablation), Anthropic tools.
- Today: `file_read` double-cut by path keywords (QP T1), logs head-⅔ (QP L3), tail hints
  deleted (QP T3); 4.4 % of results sit at the cut.
- Change: QP P0.1/P0.2. **New:** (1) *age-based history collapsing*: after 8 newer observations,
  an older tool result becomes a one-line digest (`file_read src/x.rs:120-260 · 140 lines ·
  collapsed`) regardless of context %, so the working set is always the recent window SWE-agent
  found optimal, with the full text recoverable from the read cache; (2) *file-system spill*: any
  result over 16 KB is written to the scratch dir and the model gets the path plus a 40-line head;
  (3) a `format: concise | detailed` parameter on `shell_run`, `search_files`, `process` with
  `concise` as default (exit code, error-anchored tail, counts).
- Effect: fewer tokens per step and, per SWE-agent, higher resolve rate. Size M.

### C. Round-trip diet, second pass (speed + cost)

- Evidence: Manus 50 calls/task; aizen's own P1–P5 diet; Claude Code effort semantics.
- Today: median 4, p90 30 calls per turn; 4.6 % repeats; a failed `file_edit` costs a round-trip
  (QP T5), the model often spends a call on `cargo check` after an edit.
- Change: QP P1.13 (`dry_run`, `replace_all` on every rung, removed-lines-only diff). **New:**
  fold workspace diagnostics into the edit result asynchronously (QP T10 done right) so the
  post-edit `cargo check` call disappears for Rust/TS; when the model batches ≥ 3 edits, the
  harness runs the fast check itself and appends the result to the last edit's output.
- Effect: one to two fewer requests per edit batch. Size M.

### D. Architect / editor split through the Pantheon (quality + speed)

- Evidence: Aider (+3 to +5 pp), Anthropic effort guidance (low effort for sub-agents), survey.
- Today: all seven roles run the same model at the same budget (QP O3/O4).
- Change: QP P2.2 (per-role model) and P2.4 (`implement` preset). **New:** an *architect mode*
  under `/effort max` for multi-file tasks: `metis` on the strongest model writes the plan in
  prose, `daedalus` on the fastest capable model applies it at low effort, `themis` runs the
  narrowest check, one fix loop. Single-file tasks never enter it.
- Effect: Aider's numbers say quality up; wall-clock down because the strong model never emits
  code tokens and the editor model streams fast. Size M on top of P2.

### E. A fast path for light turns (speed; no quality trade)

- Evidence: Claude Code ("skip the plan"), Amp's low mode, Anthropic effort.
- Today: every turn pays memory recall, the dynamic lane, skills, and possibly post-turn learning;
  effort tiers change only `reasoning_effort` (QP L9).
- Change: a `TurnShape` classifier (question · small edit · multi-file · research) reusing
  `classify_effort_with`: pure questions skip recall, skills and self-review; `low` maps to the
  cheap model in `models_by_effort` (QP P1.4); post-turn learning stays gated on ≥ 4 tool calls
  (already true) and moves off the critical path (QP P4.4).
- Effect: chat-shaped turns answer in one request with a ~45 KB prefix instead of 67 KB plus
  recall. Size S.

### F. Verification that is cheap, targeted, and always present (quality; small speed cost)

- Evidence: survey (verification is the limiter), SWE-agent linter guard, Claude Code "give it a
  check".
- Today: gate exists for Rust/TS typecheck only (QP L1/L2); workflow children run unverified
  (QP O9); LSP diagnostics block (QP T10).
- Change: QP P0.4. **New:** a *verify ladder* chosen by cost: (1) LSP diagnostics on edited files,
  free and async; (2) typecheck; (3) the narrowest test (sibling test file, `cargo test module::`,
  `pytest path::test`); (4) full suite only at Done on multi-file changes. `/init` writes
  `.aizen/verify.json` with the detected commands and their measured durations so the gate picks
  the fastest sufficient rung; the final message must quote the check it ran and its exit code.
- Effect: verified-done becomes real in seven languages at a few seconds per rung instead of a
  full suite. Size M.

### G. Output and schema diet (cost + speed)

- Evidence: Anthropic tools (concise −⅓), Manus (mask, do not remove).
- Today: assistant text is already terse; `diff_preview` echoes the model's own added lines
  (QP T6); 42 KB of schemas, of which `workflow`, `persona_*`, `skill_save/refine/forget`,
  `checkpoint*`, `memory_save/update/forget` are rarely used in a coding turn.
- Change: QP T6. **New:** a *deferred set* decided once per conversation at turn 1 by `TurnShape`
  and then frozen (so the tool list stays byte-stable): coding turns advertise ~28 tools and reach
  the rest through the existing `tool_search`; `persona_create` leaves the model surface entirely
  (slash only). Trim the six largest descriptions to the "new hire" standard and re-run the schema
  ratchet with LSP on (QP L13).
- Effect: ~12 KB (≈ 3 k tokens) off every request. Size S.

### H. Wall-clock hygiene outside the model (speed)

- Today: §1.3.
- Change: QP P4.4 (learning off the critical path, cheap model), P4.5 (prompt-build caches),
  P1.7 (two-phase stall), P3.4 (retry captions), P1.10 (Codex streaming). **New:** warm LSP
  servers and refresh the `/init` index in a background task after the first frame, never before
  it (startup stays 10 ms); keep `MAX_PARALLEL = 5` reads and eager tool start as they are.
- Effect: the next prompt is accepted within a second of the previous turn; the first edit of a
  session gets diagnostics. Size S beyond the QP items.

### I. Prompt altitude (quality; measured, not assumed)

- Evidence: Anthropic altitude and Claude Code's "would removing this cause a mistake?"; the
  Fable 5.1 migration notes say over-prescriptive prompts reduce output quality on current models.
- Today: 14.8 KB base prompt plus a routing map; tool usage prose lives both in the prompt and in
  descriptions; `CLAUDE.md` is skipped in favour of a smaller `AGENTS.md` (QP M10).
- Change: A/B the base prompt on the task suite (QP P6.3): cut sections that do not change
  behaviour, keep tool guidance in one place (the description), honour `CLAUDE.md`. Size S once
  the suite exists; do not cut without the suite.

### J. Keep what already works

Error-aware clearing keeps failure traces (Manus says never hide them); the todo reminder every 8
steps is recitation; deterministic partial reports; barrier scheduling for writers; parallel reads;
eager tool start; the R1–R6 edit ladder. None of these should be "optimised" away.

## 4. Targets

| Metric | Now (measured) | Target | How measured |
|---|---|---|---|
| Fixed bytes per request | 66,997 | ≤ 45,000 | `aizen prompt-size` |
| Cached share of input tokens on a ≥ 20-call turn | unknown (not persisted) | ≥ 85 % | usage persisted per request (A) |
| Results mangled by the 4,096 cut | 4.4 % at the cut | 0 mangled (cuts are by section or anchor, tail kept) | task suite + session stats script |
| Tool calls per task on the suite | baseline TBD | −20 % vs baseline, pass rate not below baseline | QP P6.3 |
| Next-prompt latency after a turn | up to minutes on 39 % of turns | < 1 s | timer in `repl/turn.rs` |
| Languages with a real verify gate | 2 (typecheck only) | 7, with the narrowest test | QP P0.4 + F |
| Identical repeat calls per turn | 4.6 % | < 2 % | session stats script |

The quality guardrail for every item is the same: the task suite's verified-done rate may not
drop. SWE-agent's ablations are the reason to expect it to rise.

## 5. Where these land in the quality plan

| Lever | New work to add to the QP | Phase |
|---|---|---|
| A | persist usage per request; cached-% in HUD and `/cost`; nudges as mid-conversation system messages on Anthropic | 0 |
| B | age-based history collapsing; file-system spill over 16 KB; `format: concise` default | 1 |
| C | async workspace diagnostics in edit results; harness-run fast check after an edit batch | 1 |
| E | `TurnShape` gating of recall/skills/self-review | 1 |
| G | frozen per-conversation deferred tool set; `persona_create` off the model surface | 1 |
| F | verify ladder by measured cost; `/init` writes `verify.json` | 0–1 |
| D | architect mode under `/effort max` | 2 |
| H | background LSP and index warm-up after first frame | 3 |
| I | prompt A/B on the suite | 6 |

## Sources

- Manus, "Context Engineering for AI Agents": <https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus>
- SWE-agent, "Agent-Computer Interfaces Enable Automated Software Engineering": <https://arxiv.org/abs/2405.15793>
- Aider, "Separating code reasoning and editing": <https://aider.chat/2024/09/26/architect.html>
- Anthropic, "Writing effective tools for agents": <https://www.anthropic.com/engineering/writing-tools-for-agents>
- Anthropic, "Effective context engineering for AI agents": <https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents>
- Claude Code best practices: <https://code.claude.com/docs/en/best-practices>
- Survey, "From Question Answering to Task Completion: Agent System and Harness Design" (2026): <https://arxiv.org/abs/2606.20683>
- mini-swe-agent: <https://github.com/SWE-agent/mini-swe-agent>
- Prompt-caching economics: Anthropic prompt caching reference (cache read 0.1× base, 0.025× on Fable 5.1; write 1.25× / 2×)
