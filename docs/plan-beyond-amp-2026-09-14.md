# Aizen beyond Amp — the plan (2026-09-14)

Status: proposal for the maintainer. Nothing here is decided. Every number is from the tool output
of the day this was written; every claim about Amp cites a public page. Where something could not
be verified it says so.

## 0. The bet in one paragraph

Amp cannot be beaten at being Amp. Sourcegraph shipped roughly thirty product changes between June
and September 2026 (Orbs, Puck, multiplayer threads, Slack, iOS, Restack, a free tier), and every
one of them leans on a cloud that a solo maintainer will not build. Feature parity is the wrong
goal. The winnable goal is to be the **best local-first coding agent**: one static binary, runs
offline against any model including a local one, kernel-sandboxed on every OS, and the only agent
in its class that **publishes its own benchmark scores**. Amp has none of those four, and its
architecture (Node CLI + threads on ampcode.com) makes the first two expensive to copy. "Strongest
in the world" is then a claim we can defend with a scoreboard instead of a feature list.

## 1. Where both stand today (evidence)

### Aizen 0.6.7 (measured on this checkout)

| Fact | Value | Source |
|---|---|---|
| Runtime | one static binary, 34 MB, 10.8 ms start | CLAUDE.md, measured 2026-08-02 |
| Source | 159,152 lines Rust, 1,862 tests, 23 TODOs | `find`/`grep` today |
| CI | build+test green on public `main` (3 OS) | GitHub Actions run 2026-09-10 |
| Public repo | 114 stars, 22 forks, 3 watchers | GitHub API today |
| v0.6.7 downloads | Windows 17 · Linux 2 · macOS 1 | GitHub API today |
| Package managers | not on crates.io (404), Homebrew (404), winget (404) | curl today |
| Signing | Windows exe unsigned, macOS not notarized | CLAUDE.md |
| Editor surface | none (no VS Code/JetBrains/ACP); `mcp_serve` only | grep of docs + src |
| Task benchmark | none; benches cover memory recall, profile, dialectic, and a scripted loop eval | `src/bench/`, `docs/REFERENCE.md` |

What aizen already has that maps onto Amp's headline features:

| Amp feature (date shipped) | Aizen equivalent today | Gap |
|---|---|---|
| Steer, don't queue (2026-09-08) | mid-turn steering mailbox (`steer.rs`, Alt+Enter / `>`) | none |
| Subagents + Oracle | Pantheon: 7 roles in one table (`src/agent/roles.rs`), depth 1 | every role runs the **same model**; no "oracle" tier |
| The Dial (low/medium/high/ultra, agent+oracle model per rung) | 5-tier effort ladder, adaptive routing **off by default** (`core/effort.rs`, `cli_config.rs`) | effort changes reasoning budget only, never the model |
| Librarian (remote code search) | `codebase_search` BM25 over `/init` chunks, per-repo | single repo, local only |
| Threads, handoff, resume | `/sessions` `/resume` `/handoff`, foreign-session import from Claude Code and Codex | local only, no sharing |
| Restack / ordered diffs (2026-09-01, 09-11) | checkpoint trees + per-session ledgers keyed by pre-image (`timemachine.rs`, `coop.rs`) | no commit re-organisation on top of it |
| Orbs (cloud machines per thread) | `aizen serve` on systemd/Docker/K8s, Telegram/Discord control, OS-scheduler cron | not a product; no cloud |
| Puck (meta-agent, voice, usage explain) | `/workflows` live registry, `/cost`, `/tokens` | no coordinator persona |
| Attach video/PDF/logs | image input only | PDF/log ingestion missing |
| Amp Tab (editor autocomplete) | nothing | requires an editor extension; out of scope |
| Runners (free local execution, 2026-09-13) | the whole product | Amp still needs Node and ampcode.com |

### Amp (public sources, September 2026)

- CLI is an npm package (`@ampcode/cli`, formerly `@sourcegraph/amp`); needs Node.
- Free "Hobby" tier since 2026-09-13 when you bring your own keys or subscriptions; Orbs (remote
  machines) stay pay-as-you-go; Megawatt $20 / Gigawatt $200 buy Orb discounts.
- Reviews list the same weaknesses repeatedly: threads live on Sourcegraph servers, no offline or
  local-model mode found in any source, a visible cost meter, and **no published benchmarks**.
- Amp's own post "Who Cares About the Model?" states the default model was swapped overnight
  without user complaints. The product is deliberately model-opaque. That is the opposite of BYOK.

Amp's free tier removed aizen's "it's free" pitch. What is left must be structural: local,
sovereign, sandboxed, fast, and measured.

## 2. What "strongest" can honestly mean here

Three claims are defensible within six months of one person's work:

1. **Best harness at equal model.** Run Terminal-Bench 2.1 and SWE-bench Verified through aizen
   with the same model other harnesses publish (Claude Sonnet 5, GPT-5.6, a local Qwen3-Coder), and
   report pass rate **and tokens per task**. If aizen's loop is not within a few points of Claude
   Code on the same model, the bench says exactly which scenario class loses, and Phase 1 fixes it.
2. **Only agent with kernel enforcement on all three desktops.** Linux and macOS already are.
   Windows is honestly `partial`. Closing it is the single largest engineering item in this plan.
3. **Best agent for models you own.** Local llama.cpp/vLLM, air-gapped, no upload, prompt cache
   honoured. Nobody with a cloud product will chase this market seriously.

Claims we should stop making: "self-evolving persona", "seven named hands". They are real code but
they are not why a developer switches tools, and they read as marketing.

## 3. The five bets

### Bet A — a scoreboard (highest leverage, do first)

- New `aizen bench tasks` in `src/bench/`: drives `aizen agent --yes` inside a container per task,
  scores with the benchmark's own tests, writes JSON. Docker is a **dev-time** dependency of the
  bench only; the shipped binary stays static.
- Targets: Terminal-Bench 2.1 (89 tasks, Harbor format) and a 100-task SWE-bench Verified slice.
- Three model configs published side by side: frontier BYOK, mid-tier via OpenRouter, local
  Qwen3-Coder on llama.cpp. Report pass rate, tokens/task, wall-clock/task, $/task.
- CI: a nightly `task-bench.yml` on a 10-task smoke subset so every merge has a number.
- Deliverable: `docs/BENCHMARKS.md` with reproduction commands and the raw JSON checked in.

### Bet B — the Dial, but yours

- `roles.rs` gains an optional `model:` per role. `metis` (plan) and `nemesis` (review) become the
  "oracle" seat: a stronger model when the tier allows, cheaper for `daedalus`.
- Effort tiers map to model pairs in config (`models_by_effort: {low: …, high: …}`); `/effort` and
  adaptive routing then change the model, not only the reasoning budget. Adaptive routing turns on
  by default once Bet A shows it does not lose points.
- The gateway plans the maintainer already sells (`aizen sub plan`, combos) become curated dials
  for people who do not want to pick; BYOK users keep full control. Same shape as Amp's Dial,
  without model opacity.

### Bet C — local models as a first-class tier

- `src/llm/profiles.rs`: per-model capability profile (tool-call dialect, parallel tool calls
  yes/no, context length, reasoning field name, llama.cpp `cache_prompt`/slot hints). Detected from
  `/models` where possible, overridable in config.
- Fix what the local config of Bet A exposes: tool-call repair for models that emit malformed JSON,
  smaller tool schemas under the 1,400 B/tool budget, a "strict" system prompt path already exists
  (`system_prompt_strict.md`) and should be selected by profile.
- Ship a one-line recipe in the README: `llama-server … && aizen config` → working agent, no key.

### Bet D — editor presence without writing editors

- Implement the Agent Client Protocol (JSON-RPC 2.0 over stdio, created by Zed, co-maintained by
  JetBrains; Zed and JetBrains native, Neovim/Emacs/VS Code via plugins; ~50 agents in the registry
  as of June 2026). `aizen acp` reuses the `run_agent_loop` seam and the `mcp_serve` scaffolding.
- Register in the ACP registry. This puts aizen inside Zed and every JetBrains IDE for the cost of
  one server module, and it is pure Rust.
- Not Amp Tab. Autocomplete needs a per-editor extension and a FIM model; say so in the docs.

### Bet E — git-native review on infrastructure we already have

- `aizen time restack`: split a thread's changes into logical commits using the per-session ledger
  and the pre-image checkpoint keys in `coop.rs`. Final tree identical; only commit boundaries
  change. Amp shipped this on 2026-09-11 as a cloud feature; ours runs offline.
- `/diff --ordered`: files that explain the change first (new types, then callers, then tests).
  A heuristic over `git_inspect` output is enough for v1.
- `/pr`: open a PR with a generated description through the existing GitHub reach channel.

### The hard item — Windows enforcement

- Replace Job-Object-only containment with an AppContainer profile (`windows-sys` 0.59 has the
  APIs): filesystem via ACL grants on the workspace and a private temp, network denied by omitting
  the internet capabilities, secrets scrubbed as today. Report `enforced` only for what is actually
  enforced; keep the `partial` word for the rest. `docs/SANDBOX.md` matrix is the contract.
- This matters because 85 % of downloads are Windows and because it is the one claim in §2 no
  competitor can match this year.

## 4. Phases, gates, and rough sizing (one person)

| Phase | Weeks | Contents | Gate to leave the phase |
|---|---|---|---|
| 0 Scoreboard | 2 | Bet A harness, first numbers, README repositioned | `docs/BENCHMARKS.md` with 3 configs; README under 110 lines leads with local-first + scores |
| 1 Loop quality | 6 | fix what Phase 0 exposes; Bet B model pairing; Bet C profiles | Terminal-Bench gap to Claude Code at equal model ≤ 3 points; tokens/task ≤ theirs |
| 2 Editors | 3 | Bet D ACP server + registry listing | aizen runs a full edit-verify turn inside Zed and IntelliJ |
| 3 Git review | 3 | Bet E restack, ordered diff, `/pr` | restack reproduces identical tree on 20 recorded sessions |
| 4 Windows sandbox | 6 | AppContainer backend | `sandbox doctor` reports `enforced` for fs+net on Windows 11; audit log unchanged |
| 5 Distribution | parallel | crates.io, Homebrew tap, winget, scoop, AUR; signing | `cargo install aizen` works; SmartScreen silent; notarized dmg |

Phase 5 needs money and accounts (Azure Trusted Signing or an EV cert, Apple Developer $99/yr,
crates.io ownership). Those are the maintainer's decisions; the plan only lists them.

## 5. Not doing, and why

- **Orbs / cloud machines, shared threads, iOS app, Slack.** Requires a hosted product and on-call.
  Aizen's answer is `aizen serve` on your own VPS, controlled from your phone through Telegram.
- **Amp Tab autocomplete.** Per-editor extension work with no reuse; wrong fight.
- **A Puck clone.** The hostbot already is a coordinator; add spawn/track of local runs to it later
  if Phase 2 leaves time, not before.
- **More surfaces.** Freeze persona, SOUL, and the reach channels (Twitter/YouTube/HackerNews) for
  the duration. 159 k lines is already a large bug surface for one maintainer; the session memory of
  this project is a log of bugs found by dogfooding, not by users. Consider feature-gating the
  channels so the default binary shrinks.

## 6. Risks, stated plainly

- **Velocity.** Amp ships several features a week with a team. This plan wins by choosing four
  claims and holding them, not by keeping up. If Phase 1 slips past eight weeks, cut Bet E first.
- **The bench may show the loop is worse than we think.** That is the point of running it. Publish
  the number anyway; a public, reproducible 70 % beats a private "it works".
- **BYOK quality ceiling.** With a weak model aizen is weak. Bet C narrows it; it cannot remove it.
- **Windows AppContainer is fiddly** (path ACLs, console handles, `cargo` inside the container).
  Time-box it; if it fails, the honest `partial` stays and the claim in §2 is dropped from the docs.
- **Distribution is the actual bottleneck.** 20 downloads per release means "strongest" is unheard
  either way. Phase 5 is cheap engineering and expensive paperwork; it should start in week 1.

## 7. What to measure every release

| Metric | Today | Where it comes from |
|---|---|---|
| Terminal-Bench 2.1 pass rate, per model config | unmeasured | Bet A |
| SWE-bench Verified slice pass rate | unmeasured | Bet A |
| Tokens per task and $ per task | unmeasured | Bet A |
| Cold start / binary size | 10.8 ms / 34.1 MB | keep in CI |
| `sandbox doctor` enforcement per OS | Linux enforced · macOS enforced · Windows partial | `ci.yml` already runs it |
| Downloads per release, stars | 20 / 114 | GitHub API |
| Editors reachable | 0 | ACP registry |

## 8. File touchpoints

| Area | Files |
|---|---|
| Bench | `src/bench/mod.rs`, `src/bench/loop_eval.rs`, `src/bench/metrics.rs`, new `src/bench/tasks.rs`, `.github/workflows/task-bench.yml` |
| Routing | `src/core/effort.rs`, `src/core/cli_config.rs`, `src/agent/roles.rs`, `src/ui/effort_ui.rs` |
| Local models | `src/llm/client.rs`, `src/llm/mod.rs`, new `src/llm/profiles.rs`, `src/agent/system_prompt_strict.md` |
| ACP | new `src/features/acp.rs`, pattern from `src/agent/mcp_serve.rs`, entry in `src/cli/` |
| Restack / diff / PR | `src/features/timemachine.rs`, `src/features/coop.rs`, `src/agent/reach/` (GitHub channel) |
| Windows sandbox | `src/sandbox/backend/`, `src/sandbox/policy.rs`, `docs/SANDBOX.md` |
| Distribution | `install.ps1`, `install.sh`, `.github/workflows/release.yml`, `Cargo.toml` metadata, `dist/` |

## Sources

- Amp news, June–September 2026: <https://ampcode.com/news>
- Free Agent (2026-09-13): <https://ampcode.com/news/free-agent>
- Meet Puck: <https://ampcode.com/news/meet-puck>
- Restack: <https://ampcode.com/news/restack-your-changes>
- Amp vs Claude Code review: <https://www.alexdunlop.com/writing/amp-vs-claude-code-worth-switching-2026>
- Amp weaknesses (offline, privacy): <https://cursor-alternatives.com/ide-extensions/amp-by-sourcegraph/>
- Amp CLI on npm: <https://www.npmjs.com/package/@ampcode/cli>
- ACP: <https://zed.dev/acp>, <https://www.jetbrains.com/acp/>
- Terminal-Bench 2.1: <https://snorkel.ai/leaderboard/terminal-bench-2-1/>
