## Aizen v0.6.8 — the harness shows its work

Every change in this release started with a number: what the model is shown on each request and
what it costs, how many steps a task takes, which tools and commands the saved sessions actually
used. The 2026-09 quality plan (`docs/execution-plan-2026-09-14.md`) was worked through against
those numbers, and this is the result — a smaller, byte-stable prompt; a loop that verifies before
it says Done; sub-agents you can pin, chain and bill; approvals that show the exact payload; and an
output stream any Claude Code front-end already reads. Everything is client-side — **there are no
server changes in this release**.

### The short version

```sh
aizen prompt-size --live                  # the cache prefix, block by block, STABLE or VOLATILE
aizen agent --output-format stream-json "fix the failing test in src/parse.rs"
aizen bench tasks --record                # six fixture tasks through the real loop, tapes for CI
aizen memory consolidate --apply          # retire the duplicates the store has been carrying
```

On this build, as `aizen prompt-size` reports it:

| Surface | Size |
|---|---|
| Fixed prefix with every tool advertised | 79.6 KB |
| Fixed prefix on a coding turn (`lean_tools`) | 59.2 KB |
| Tool schema advertised on a coding turn | 20.5 KB of 40.8 KB |

### What the model is shown

- **A usage ledger in every session file.** Each model call's input, output, cached and
  cache-write tokens are appended on autosave; `/cost` shows the session's cached share, and the
  status line's `⛁ N% cached` chip uses the shape-corrected input as its denominator, so it can no
  longer exceed 100 % on an Anthropic-style gateway.
- **`aizen prompt-size --live`** prints the prompt lanes the way a cache sees them and says whether
  two consecutive builds are byte-identical. They are now: `<sessions>` rows carry a calendar day
  instead of "3m ago", session working memory rides the user turn, and the persona's `<self>`
  block is adopted at the boundary like the frozen core.
- **Lean turns.** Each turn is classified as a question, a small edit, a multi-file change or
  research; a pure question is answered in one request against the cached prefix, and the
  conversation's widest shape decides which rarely-used built-ins ride behind `tool_search` —
  now also the language-server queries, the symbolic edits, `codebase_search` and
  `session_recall`, which together were 4 of about 2,800 calls across 79 saved sessions.
  Descriptions on the always-on tools are tighter too. On by default for first-party APIs only,
  because some gateways cannot call a tool that was not advertised; `lean_tools` in
  `cli-config.json` sets it either way.
- **Older tool results collapse; big ones spill to disk.** A result older than the eight most
  recent and longer than 800 chars becomes one line naming its spill file; a raw result over 16 KB
  is written to the scratch dir in full before the budget cut. `shell_run`, `process` and
  `search_files` take `format: concise | detailed`, concise by default, and a log budget keeps the
  first error line and a large tail rather than head-⅔/tail-⅓.
- **One compaction trigger.** The REPL's post-turn auto-compaction is gone; the `/config` threshold
  arms the loop's own mid-turn compaction on both the REPL and `aizen agent`, and a single-turn
  run — one prompt, fifty tool steps — can finally compact at all. `/handoff` (zero uses in 64
  saved sessions) and percentage-based tool-result clearing (nothing left to clear once results
  collapse) are removed; the overflow shrink behind a provider's context-length rejection stays.
- **One token estimator.** CJK, Hangul and precomposed Vietnamese count 1/1.8 per char everywhere;
  every "N-token" cap used to admit roughly twice that in Vietnamese.
- **Effort tiers with teeth.** The tier sets the step cap, continuation budget, verify-and-fix
  rounds and self-review, not only the `reasoning_effort` string half the providers ignore;
  `models_by_effort` sends a tier to another model on the same endpoint.

### Before it says Done

- **The verify gate climbs a ladder.** After the typecheck it runs the narrowest test the edited
  files name, then the whole suite when the change touched more than one file — but only while the
  suite fits the verify budget, which `/init` now times. Go, Maven, Gradle, .NET and Python join
  Rust and TypeScript; a toolchain that is not installed is "nothing ran", not a failure, and when
  nothing could run the model is asked once to run the project's own build or test command.
- **Diagnostics no longer hold the edit.** The post-edit LSP fold waits 300 ms instead of 3.5 s
  and lands late results on the next tool result, naming callers whose new errors appeared; after
  every three successful edits the loop runs the project's fast check itself.
- **Edit tool polish.** `replace_all` on every matching rung, `dry_run: true` on `file_edit`, a
  diff the model does not re-read, `file_glob` with `ignore: true`.
- **Lenient tool-call recovery.** Almost-JSON arguments are repaired; a call a local model wrote
  as text (`<tool_call>`, a ```json fence, a bare `{"name": …}` reply, `[TOOL_CALLS]`) is lifted
  into a real call instead of being echoed as the answer.
- **The todo list is scoped to the turn**, and loop nudges retire at the next run instead of
  living in history forever.

### Sub-agents: the Pantheon

- **Pins and budgets per role.** `roles.pantheon.<role>` puts a reviewer on a strong model and a
  searcher on a cheap one in the same fan-out; default step budgets run from argus 15 to
  daedalus 45.
- **A child starts where its parent is.** Every dispatch opens with a `<parent_context>` block:
  the parent's in-progress todo item, the findings it passes on, and up to fifteen locations it
  already read, as `path:start-end`.
- **Workflows chain.** Tasks name what they wait for (`after`), run in dependency waves with one
  writer per wave, and a task that names `retry_on_fail` re-runs its upstream once on
  `VERDICT: FAIL`; `workflow(mode="implement", …)` prebuilds daedalus → themis → nemesis with that
  loop.
- **Architect mode.** Under `/effort max` a multi-file turn is planned by `metis` on the strongest
  configured model and applied by the fastest, with the plan folded into the request.
  `architect_mode: false` or `AIZEN_ARCHITECT=0` turns it off.
- **Notes between children, and the writer lease scoped.** Every child's full report is filed on
  a per-conversation blackboard a later child can `file_read`; the workspace writer lease is keyed
  by resource scope, so two `serve` lanes no longer share a lease meant to keep them apart.
- **Attribution.** A child's approval says `daedalus · fix parser wants: …`, `/workflows` shows
  its step and its own tokens, and a write-capable child that fails is re-run once with a
  tightened brief, then restored to the pre-dispatch checkpoint if it fails again.
- **When not to delegate.** The prompt says what earns a fresh context, and a brief under 80 chars
  that names no file or symbol is refused.

### Approvals

- **You approve the payload, not a basename.** `file_edit` shows its patch, `file_write` says
  create or overwrite, `shell_run` shows the directory and the full command line, `file_move`
  names both ends — in the TUI and in the Telegram message alike.
- **Grants narrower than allow-all.** `Yes — always for <tool> this session` (`t`) and `… under
  <dir>` (`d`); a project can ship standing grants in `.aizen/approvals.json`; `/approval grants`
  lists them.
- **`/approval`, `/yolo` and `/smart` are session-scoped.** A `/yolo` in one window used to arm
  every other window and every cron job on the machine; `--persist` is now the explicit way to
  change the saved default.
- **`smart` reads a multi-line command as several.** `ls\nrm -rf build` was one segment whose
  program was `ls`. Read-only programs with writing flags ask, `git branch feature` creates,
  `cargo clippy --fix` asks, and bash's `&>` is no longer misread as file blanking.
- **`cli-config.json` is written atomically**, with the last good file kept as
  `cli-config.prev.json`.

### Machine-readable output, and hooks

- **`aizen agent --output-format stream-json`** emits the run as one JSON record per line in the
  shape Claude Code's `stream-json` uses: `system`/`init`, `stream_event` deltas, `assistant` and
  `user` messages with `tool_use`/`tool_result` blocks, `control_request` for a destructive call
  (answered on stdin with a `control_response`), and `result` last with the stop reason and this
  run's tokens. A delegated child's records carry `parent_tool_use_id` and `dispatch`.
  `--output-format json` prints only the closing `result`.
- **Hooks.** `hooks` in `cli-config.json` runs your own commands around the loop — `pre_tool`
  (exit `2` denies, `{"decision":"allow"}` pre-approves), `post_tool` (its output joins the tool
  result) and `stop` — through the same sandbox runner as every child. `aizen hooks` lists them;
  `AIZEN_NO_HOOKS=1` turns them off.

### The model client

- **Two-phase stream deadline.** 600 s until the first frame parses, then 90 s between frames; a
  reasoning model silent for three minutes was "never started", replayed twice and billed three
  times.
- **Request-shape quirks learned per model.** A 400 naming `max_tokens`, `parallel_tool_calls`,
  `tool_choice`, `cache_control` or `reasoning_effort` drops or renames the field and remembers
  the model for the session.
- **The ChatGPT Codex path streams like every other**, on the same watchdog, and a refreshed
  token the backend still rejects asks for `aizen auth login codex` instead of looping.
- **Streamed usage is recorded once**, from the last report the stream carried, so `/cost` works
  on llama.cpp, Ollama shims and LiteLLM, and cumulative gateways are not summed N times.

### In the terminal

- A long session paints the viewport, not the session; the row cache is a real LRU.
- `Ctrl-E` expands a tool result in the text overlay; a failed row shows its last six lines; a
  429 backoff says `rate-limited — retrying in 43s (2/3)`.
- `/rewind` exists (an alias of `/undo`); both show the diff stat first and refuse to discard
  unsaved work without `--yes`. `/diff --patch` draws through the diff box.
- The screensaver waits for a quiet screen; CJK wraps by display width.
- LSP servers and the `/init` index warm up after the first frame; startup time is unchanged
  (`AIZEN_NO_WARMUP=1` turns it off).
- The provider wizard and its siblings leave no trace lines, never echo a key, and read a scheme
  the way it was meant.

### Memory

- **Dedup that fires**, in two stages across tiers, and `aizen memory consolidate [--apply]` runs
  it over the whole store without a model call.
- **A fact that names the work is a project fact**, whatever grammar it was written in, so
  another project's architecture notes no longer travel with you as preferences.
- **The recall gate judges the block it will show**, weighting rare words; identifiers match
  their words (`get_by_id` for "get by id").
- **Learning is off the critical path.** The secretary and the persona reflection run in the
  background; the next turn waits at most 5 s. Chore calls are capped at 60 s.
- **`CLAUDE.md` is honoured beside `AGENTS.md`**, edits are live on the next message, and the
  persona stays out of coding turns unless `/persona coding on`.

### Keeping things

- **`/undo` removes the file the agent just created** — the most common rewind, which used to
  fail its own verification and roll back.
- **`aizen update` verifies what it installs.** Every release ships a `<asset>.sha256`; a mismatch
  discards the download. The previous build's backup stays a week.
- **Sessions stop growing without bound.** Each turn appends to `<session>.jsonl`; images are
  stored once under `sessions/blobs/`; the pool is pruned to `sessions_keep` and
  `sessions_max_bytes`.
- **`aizen cron add` writes the spec before registering**, logs owner-only and posts failures;
  `aizen time gc` finally reclaims the objects no checkpoint reaches.

### Measurement

- **Tapes.** `AIZEN_TAPE=record|replay|strict` records or replays every model call in the
  process.
- **`aizen bench tasks`.** Six fixture crates through the real loop with the verify gate on,
  judged on the files and on steps/tokens against a baseline; CI replays the tapes on ubuntu and
  windows.
- **`aizen bench sessions`** reports the turn-shape statistics of your saved conversations, so a
  loop change can be judged on real turns.

### Compatibility

- `/handoff` is gone; a saved session that carries a handoff seed still loads.
- `lean_tools` is on by default only for `api.anthropic.com` and `api.openai.com`. Set it
  explicitly for a gateway that can call an unadvertised tool, or leave it off for one that
  cannot.
- Session files gain a per-turn `.jsonl` delta beside the transcript; a desktop app build from
  before this change shows a session only up to its last full rewrite.
- Config-file setups, the `AIZEN_*` environment variables and the sign-in flow from 0.6.7 are
  untouched. No server changes.
