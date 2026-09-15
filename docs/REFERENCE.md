# Aizen — full reference

Everything the [README](../README.md) intentionally leaves out: the REPL surface, every command and
flag, self-hosting, MCP, the browser tools, and the safety model in detail.

> The README is the 5-minute version. This file is the manual.

---

## Interactive REPL (just run `aizen`)

Run the binary with **no arguments** to land on a splash screen (block-art title + a bordered
panel of tools/commands) and a **unified chat+agent REPL** — there is no separate "chat" vs
"agent" mode: you just type. A plain message is answered; a task that needs tools uses them — one
loop. A status line shows the model, session tokens, turn count, and a **`% context` bar**
(green→yellow→red as the window fills).

```text
⚡ gpt-4o-mini  ·  ~1.2K/128K tok  ·  3 turns  ·  ███░░░░░░░ 27% ctx
╭──────────────────────────────────────────────────────────────────╮
│ ❯ refactor the parser and run the tests                           │
╰──────────────────────────────────────────────────────────────────╯
```

The chat box is a small line editor: type / Backspace / Del at the cursor, **←/→** move, **Home/End**
jump, **↑/↓** recall past prompts, Enter sends. A braille spinner (`⠹ thinking`) shows while the
model is responding, clearing the moment the first token streams.

The **mouse works in the box too**: click to put the caret on the character you clicked instead of
walking there with ←/→, and drag across the text to select it — the selection is copied to the
clipboard on release, and **Ctrl-C** copies it too (with nothing selected, Ctrl-C copies the whole
draft). Typing over a selection replaces it; Backspace/Del deletes it. On a draft longer than the box
the view only scrolls when the caret would leave it, so the text stays put under the cursor while you
move around in it.

**Attach an image** (vision) — two ways, because Ctrl-V can't be used (Windows Terminal intercepts
it for its own paste, so the keystroke never reaches `aizen`):
- **Ctrl-O** — grab a copied screenshot from the clipboard (Win+Shift+S, or "Copy image" in a
  browser). An `[1 img]` tag shows in the top border.
- **Drag an image file onto the window** — the terminal pastes the file path; press Enter and `aizen`
  turns image-file paths on the line into attachments (you can also type/paste a path). Real image
  files only — prose like `nope.png` that isn't a file stays as text.

Both send the image with your text to a **vision-capable** model. **Ctrl-X** removes the most recent
attachment (keeps your text); **Esc** clears the line and all attachments at once. Clipboard
screenshots are downscaled to ≤1568px and encoded inline; the token gauge ignores attachments (they
ride outside `content`). Clipboard grab is desktop-only (Windows/macOS); drag-drop/path works
everywhere.

The context window is **auto-detected** from the provider's `/models` when it reports one
(OpenRouter/LiteLLM-style gateways do; the bare OpenAI schema doesn't). When it's absent the bar
shows `ctx·est` and estimates by model name (Claude 200K · Gemini/GPT-4.1 1M · DeepSeek 64K · else
128K). Override it explicitly with `aizen config set --context-window <tokens>`.

**Slash commands** (the meta layer):

| command | does |
| --- | --- |
| `/help` | list commands |
| `/model` | list the provider's models (with context windows) + arrow-key pick one |
| `/provider [name|add|manage]` | one-pick switch among saved providers; add/edit/rename/delete in the same manager |
| `/login` | sign in to Aizen in a browser — the session opens the gateway, so nothing is stored but the token |
| `/logout` | leave Aizen here: the session **and** the pinned key. Revokes neither |
| `/config` | provider-first settings: add/edit/switch connections, then assign providers/models to roles and specialists |
| `/memory [query]` | show your profile, or search memory |
| `/persona` | character the agent plays + its evolving self-memory: select · new · paste-to-create · view/reset self-memory |
| `/skills` | saved procedures the agent can load: list · view · new · delete |
| `/commands` | your **custom slash commands** — markdown macros you fire (see below) |
| `/mcp` | MCP lifecycle status: connected tools, generation, health, and per-turn schema pinning (see below) |
| `/browser` | browser profile / host-route / pinned-session status (`--features browser`) |
| `/apps` | connected apps & MCP catalog — Telegram/Discord/Slack/webhook notify + browser-sign-in MCP apps |
| `/telegram` | Telegram integration menu: setup · test · status · start daemon · disable |
| `/sessions` | saved conversations — restore · save · delete (every turn auto-saves under a topic-date name) |
| `/compact` | summarize older turns now to free context |
| `/approval [ask|smart|yolo]` | one approval setting: ask every time, auto-run read-only shell, or pre-authorize tools after the hard safety floor |
| `/timemachine` · `/checkpoint [note]` · `/diff` | `/timemachine` lists every crash-recoverable, worktree-scoped Git checkpoint and jumps back to the code **and** chat of the one you pick (one gesture, reversible); `/checkpoint` saves one now; `/diff` (or `aizen time diff`) shows what changed between two checkpoints, or `working` for the live tree. CLI: `aizen time doctor` inspects without touching the tree and reports loose objects once they pile up; `aizen time gc` compacts this repo's store (packs loose objects — a save does it automatically past 2,048); `aizen time gc --all` sweeps orphaned stores left by deleted/moved repos (dry-run by default, `--apply` moves them to a trash dir, which you then delete to reclaim the space) |
| `/update` | list every published version (the one you're running is marked) and install whichever you pick — newer or older, so the same command is the rollback |
| `/cost` | session token usage + a $ estimate (real provider usage when reported; set rates via `aizen config set --price-in/--price-out`) |
| `/theme [moonlight\|lanes]` | colour theme: `moonlight` (default) keeps the calm all-silver look; `lanes` colours each kind of work — read=blue, edit=gold, shell=mauve, web=cyan, memory=violet, talk=pink, plan=teal. Bare `/theme` lists both with a live colour swatch; the choice persists |
| `/clear` | fresh conversation · `/tokens` usage · `/quit` exit |

**Input shortcuts** — on a normally typed message (not with an image):

| type | does |
| --- | --- |
| `#<text>` | **remember** `<text>` as a durable memory fact in one keystroke (straight into the brain the agent reads) — sends no turn |
| `!<cmd>` | **shell escape** — run `<cmd>` and show its output (the hard safety floor still blocks catastrophic commands) — sends no turn |
| `@<path>` | inline that file's contents into your message (only when the file exists — `@handle` in prose is left alone) |
| `` !`<cmd>` `` | splice a **read-only** command's output into your message (same gate as custom commands) |

**Context window + auto-compact** live in **`/config`** (so the settings stay in one place). The
window drives the `% context` HUD (auto-detected from `/models` when the provider reports it, else
estimated by model name, else whatever you type). Auto-compact (default **80%**, the `⊟ 80%` marker
on the status line) summarizes older turns into one dense note when usage crosses the threshold,
keeping the last few turns verbatim — the cut is always at a user-message boundary (no orphan tool
results). `/compact` forces it now. Both also settable non-interactively:
`aizen config set --context-window <tokens> --compact-threshold <0–95>` (`0` = off).

Two cheaper measures run before compaction ever triggers, whatever the threshold: tool results
older than the eight most recent collapse to a one-line digest (tool, target, size, and the scratch
file holding the full text) once eight of them qualify, and a raw tool result over 16 KB is written
to the scratch dir in full before the budget cut, so the cut result can point at it. Both are
batched and file-backed — the prompt cache breaks rarely and nothing a tool produced is lost; the
agent reads the named file for the part it needs.

The REPL needs a real terminal; piped/CI stdin prints a hint and exits (`AIZEN_MENU=1` forces it).

**Icons** — the TUI uses a curated glyph set. Pick the style in `/config` or `aizen config set --icons
<emoji|nerd|off>` (persisted): `emoji` (default — renders everywhere, no font install), `nerd`
(dev-style Nerd Font glyphs — **only render if your terminal's font is a patched Nerd Font** like
"Cascadia Code NF", else you'll see boxes), or `off` (plain text). One-off override: `AIZEN_NERD=1` /
`AIZEN_NO_ICONS=1`. (A CLI can't bundle a font the terminal will use — `nerd` needs the font set in the
terminal itself.)

**Reply visuals** — final answers can use responsive terminal tables and compact text diagrams. The
persisted mode is `auto` (default: only when it clarifies), `always` (every substantial final reply),
or `off`: choose it under `/config` → Display or run
`aizen config set --response-visuals <auto|always|off>`. Wide terminals get boxed tables; narrow
terminals fall back to stacked `key: value` records. Diagram fences preserve their monospace layout;
piped/CI output remains raw Markdown.

## Telegram — control `aizen` from your phone

`aizen serve` runs a long-lived daemon that listens on a Telegram bot (long-poll, no public URL): send
it a message → it runs the agent and replies; **destructive ops (file edits / shell) ask you to
approve from your phone** (inline ✓/✗). Replies use Telegram-native formatting: short bold headings,
clean lists, tappable safe links, copyable code blocks, and narrow stacked table records instead of raw
Markdown pipes. One temporary `✦ Đang xử lý…` status is removed when the final answer arrives; if rich
HTML is ever rejected, Aizen retries the same content as plain text. Pure-Rust (no teloxide), single binary.

```bash
aizen telegram setup     # paste the @BotFather token, message the bot to capture your chat id
aizen telegram test      # send a test message
aizen serve              # start listening (Ctrl-C to stop)
```
In a chat: a plain message runs read-only-safe (destructive ops prompt you here); prefix `/agent `
to run fully autonomously. **Follow-ups keep context** — "now fix it" works because each chat's
conversation is carried across messages (memory + SOUL + persona seeded once per session). `/new`
(or `/reset`) starts fresh; `/resume` reports how much context is kept. If the agent needs to
disambiguate it asks (the `clarify` tool) and your next message is the answer. Token lives in
`~/.aizen/cli-config.json` (or `AIZEN_TELEGRAM_TOKEN`); only `allowed_chat_ids` may talk to it. The
agent can also call `telegram_send` / `telegram_ask`.

**In-chat command menu**: the bot publishes a `/` menu (`setMyCommands`) so every control is one tap
away — `/sh <cmd>` (runs now; the `cmd_guard` floor still blocks catastrophic commands), `/cd` · `/pwd`,
`/approval` · `/ultimate` · `/effort`, `/model`, `/memory`, `/tools`, `/status`.

**Host more than one bot from one daemon**: from the primary bot, `/addbot <name> <token>` validates a
second @BotFather token and hot-spawns it into the running daemon (no restart); `/bots` lists them and
`/rmbot <name>` stops one. Extra bots share your `allowed_chat_ids` (a private chat id is your user id,
identical across all your bots), and a destructive-op approval always returns to the bot the request
came from. Extra bots persist in `telegram_bots` so a restart re-hosts them.

### Host it 24/7 on a Linux VPS

`aizen serve` is a foreground process — to keep the bot alive across logout, crashes, and reboots, run
it as a systemd service:

```bash
aizen telegram setup                    # once: token + your chat id
aizen serve --install --user --now      # write + enable the user unit, start now
```

`--install` writes a `Restart=always` unit (auto-restart on crash) with `network-online.target`
(waits for the network after a reboot); `--user` needs no root and calls `loginctl enable-linger` so it
survives logout. Drop `--now` to just write the unit and print the enable steps; omit `--user` for a
system unit (prints the `sudo` steps unless you're already root). `aizen serve --uninstall --user`
removes it. On Windows/macOS the command prints the NSSM / launchd equivalent. Note: the bot lives only
while the **VPS is on** — a powered-off VPS runs nothing (systemd restarts it the moment the VPS boots).

### Host it in Docker

Same daemon, packaged. Useful when you'd rather not install a toolchain on the host, or want the
agent's shell commands confined to a container:

```bash
cp .env.example .env       # AIZEN_API_KEY + AIZEN_TELEGRAM_TOKEN
docker compose up -d
docker compose logs -f     # a first run prints a pairing code here — send it to your bot
```

Two things to know about the image. There is **no published port**, because the daemon listens on
nothing — Telegram is long-poll, Discord an outbound websocket — so there's no ingress to expose or
firewall. And `tini` is PID 1: the agent spawns builds, test runners, and language servers, and
without a reaper those accumulate as zombies.

Two volumes matter. `aizen-home` (`/home/aizen/.aizen`) holds everything that must outlive the
container — the chat ids pairing wrote, sub-bot tokens, per-chat sessions, memory, the codebase index;
lose it and you lose owner pairing and all memory. `./workspace` is mounted at `/work` and is what the
agent edits, so mount only what you're willing to have edited (`:ro` for an audit-only run).

Health is `aizen serve --health`, wired as the image's `HEALTHCHECK`. It reads a heartbeat the daemon
stamps from inside its own event loop, and distinguishes idle from busy — so a probe tight enough to
notice a wedged loop won't restart a container that's ten minutes into a legitimate build. Raise
`AIZEN_HEALTH_MAX_BUSY_SECS` if your turns routinely run longer than 30 minutes.

Build without the dense retrieval tier by passing `FEATURES=` (empty) — a smaller image, at the cost
of the embedding-based memory tier.

### Host it on Kubernetes

Manifests in [`deploy/k8s/`](../deploy/k8s/), with the reasoning in
[`deploy/k8s/README.md`](../deploy/k8s/README.md):

```bash
kubectl apply -f deploy/k8s/namespace.yaml
kubectl -n aizen create secret generic aizen-secrets \
  --from-literal=AIZEN_API_KEY='sk-...' \
  --from-literal=AIZEN_TELEGRAM_TOKEN='123456:ABC-...'
kubectl apply -k deploy/k8s/          # configmap, statefulset, service, networkpolicy
kubectl -n aizen logs -f sts/aizen    # pairing code
```

It's a **StatefulSet with one replica**, and that's the finished shape rather than a starting point to
scale from. Telegram allows exactly one `getUpdates` poller per token — a second replica gets HTTP 409
forever, so it's one healthy pod plus one permanent crashloop, not double throughput. The on-disk
stores guard writes with local file locks that don't coordinate across pods (hence `ReadWriteOnce`, not
a shared volume). And turns run serially by design, which is what keeps approval routing race-free.

If that sounds like it buys little over systemd, that's the honest read: pick k8s when you already run
one and want its secret handling, node-failure rescheduling, and rollout mechanics — not for
throughput. To host more bots, use `/addbot` in an existing daemon rather than adding replicas.

The bundled `NetworkPolicy` is the part worth keeping even if you change everything else: it denies all
ingress and all *private* egress, including `169.254.169.254`, the cloud metadata endpoint that hands
out node credentials to any pod that asks. `net_guard`'s SSRF check covers the web tools, but the agent
also has a shell, and `curl` from there never passes through it — only the network layer catches both.
It needs a CNI that enforces NetworkPolicy (Calico, Cilium, Antrea); on a cluster without one the
object is accepted and enforces nothing. The manifest also sets `automountServiceAccountToken: false`,
since a shell that can read the projected token can talk to the API server as the pod.

## Configure

**ChatGPT Codex (experimental)** uses ChatGPT/Codex *consumer* OAuth (not the OpenAI Platform API key). Pick **ChatGPT Codex (experimental)** in `aizen config` → Providers & connection: it skips the API-key prompt and offers the browser sign-in in place of it, so no separate command is needed. The manual equivalents still work — `aizen auth login codex`, or `aizen config set --base-url https://chatgpt.com/backend-api/codex --api-key codex-oauth --model gpt-5.4-mini`. Model ids come from a shipped catalog, since the Codex backend has no `GET /models`. Tokens live in `~/.aizen/provider-tokens/codex.json` and `aizen config show` reports whether you are still signed in. Kill-switch: `AIZEN_DISABLE_CODEX=1`. **Risk:** private backend APIs may break or conflict with vendor terms; prefer Platform API keys / OpenRouter for supported production use. Logout: `aizen auth logout codex`.

All network commands read three settings, as flags or env vars:

| Env var | Flag | Meaning |
| --- | --- | --- |
| `AIZEN_BASE_URL` | `--base-url` | OpenAI-compatible base, e.g. `https://api.openai.com/v1` |
| `AIZEN_API_KEY` | `--api-key` | Bearer token for the endpoint |
| `AIZEN_MODEL` | `-m, --model` | Model id, e.g. `gpt-4o-mini` |

Resolution precedence per command: explicit `--flag` > `AIZEN_*` env var > saved config (below).

```bash
export AIZEN_BASE_URL=https://api.openai.com/v1
export AIZEN_API_KEY=sk-...
export AIZEN_MODEL=gpt-4o-mini
```

### `aizen login` — pin this machine to the Aizen gateway

The short way to a working endpoint when your key comes from the Aizen gateway: no key to find, no
URL to type. `aizen` prints a short code, you approve it in a browser you are already signed into,
and the gateway sends back the key, both base URLs and a default model — written straight into
`~/.aizen/cli-config.json` and switched on.

It is also the **first row of the provider picker, drawn in green**: `aizen config` → Providers &
connection → **Aizen (subscription)** runs the same pairing in place of the API-key prompt, the way
the Codex row runs a browser sign-in. Green marks the one provider you can start using without
going anywhere else first; picking it there carries on into the model list the fresh key can already
reach. It does not stop to ask what to call the profile: the pairing has already written the key
into one, and `~/.aizen/gateway.json` points at that row, so the only name that keeps the key and
the pin together is the one it chose.

```bash
aizen login                     # print a code, wait, save the key
aizen login --no-browser        # a server or an SSH session: print, don't try to open anything
aizen login --no-activate       # save the profile without making it the endpoint in use
aizen gateway status            # what the key is allowed to do, asked live
aizen gateway env               # the two base URLs, for a tool that is not this CLI
aizen gateway logout            # drop the local key
```

One domain is all the CLI knows: `https://aizen.talmetis.com`, overridable with `AIZEN_GATEWAY_URL`
(falling back to `OMNIROUTE_PUBLIC_API_BASE_URL`). The page you open comes back from the gateway —
nothing here builds a URL.

It is the same domain `aizen account` and `aizen sub` use, deliberately: it serves `/v1/*` against
the pairing key and `/auth/*` against the session JWT. One name, two prefixes, two credentials — a
pairing key sent to `/auth/*` answers `401 "Not signed in"`, which is about the paper, not the
login. `api.talmetis.com` was the gateway before the names merged; it still answers `/v1/` and is
still recognised as a gateway root, so machines pinned against it keep working.

Three things worth knowing, because each is wrong in a way that still looks like it works:

* **The short code only ever goes one way — from this machine to you.** It names a pending pairing
  for the account owner to look at; the 256-bit device code, which is what actually collects the
  key, is printed nowhere and written to no file. So if a code reaches you from anywhere else — a
  message, an email, someone reading it out — that is somebody else's pairing, and approving it
  hands them a key on your account. There is deliberately nowhere in this CLI to paste one.
* **The key crosses the wire exactly once.** It is minted at hand-over, not at approval, so
  approving and then closing the laptop leaves no live key behind. `aizen login` writes it to disk
  before it prints anything. If you lose that one answer, pair again — it cannot be re-sent.
* **The gateway states two base URLs, and they are not the same string.** OpenAI-shaped clients
  append `/chat/completions` to `…/v1`; Anthropic-shaped clients append `/v1/messages` to the root
  *without* `/v1`. `aizen gateway env` prints both as the gateway gave them. Deriving one from the
  other is how a request ends up at `/v1/v1/messages` and a 404 gets read as "the gateway is down".

`aizen gateway logout` removes the local copy and nothing else. The key stays live until you unpin
the device in the dashboard (**API keys → devices**), which revokes it in the same transaction.

**Each machine carries its own credential.** Pairing used to hand back the account's `ak_…` plan
key — the same string on every machine — so "unpin this device" in the dashboard could not cut
anything: revoking that string would cut every other machine and the account's key with it. Since
2026-09-06 pairing answers with a **JWT bound to this machine's device row**, the gateway reads that
row on every `/v1` call, and unpinning cuts exactly one machine on its next request. The plan key
stops leaving the server. Nothing about billing moved: the loadout, the per-minute ceiling and the
spend ledger still hang off the plan key's row, and calls still count against it.

Three consequences worth knowing, because two of them fail silently:

* **Never test the credential's prefix.** What is stored is `eyJ…`, and a client still checking for
  `ak_` rejects the one string that works while reporting it as a malformed key — which reads like a
  server fault. The CLI checks no shape anywhere; the one place it looks at the first three
  characters is to notice a pre-2026-09-06 pairing and say so.
* **`key_prefix` is the PLAN key's head, not a piece of what this machine holds.** It is shown so a
  row can be matched against the keys screen. `aizen gateway status` calls it `plan key`, beside the
  `device` row that the dashboard's unpin button acts on.
* **A `401` can now arrive at any moment**, and usually means somebody cut this machine on purpose:
  unpinned in the dashboard, logged out from another machine, or the account password changed (a
  password change ends every pairing, deliberately). All three end the same way — the credential is
  removed from this machine and one sentence names `aizen login`. It is never retried: `401` is
  absent from the retry table, and it is its own error kind so that goal mode stops at once instead
  of spending a backoff budget on calls that cannot be built.

**A machine pinned before this still works**, and it is not urgent — but it cannot be cut remotely,
and the dashboard labels it as such. `aizen gateway status` says so and points at `aizen login`;
nothing re-pairs on its own. `aizen logout` on one of those tells the gateway nothing, deliberately:
that string is the account's, has no device row behind it, and posting it to a route that cuts
device rows is a way to cut the wrong one.

The full wire contract — every field, the error table, the browser-side routes — is
`docs/reference/DEVICE_PAIRING.md` in `admin_aizen`.

### `aizen account` — signing in, which is how the plan is bought
```bash
aizen account login             # opens the browser
aizen account login --password  # email + password, for an account that has one
aizen account whoami
aizen account logout            # drops the token here; revokes nothing
aizen logout                    # leaves Aizen entirely: session AND pinned key
```

In the REPL the same two doors are `/login` and `/logout`.

**Deleting the Aizen provider row is a logout, not a row deletion.** Both delete paths —
`aizen config provider remove <name>` and the manager inside `aizen config` — check whether the row
is Aizen's, by the pin *or* by its endpoint, and tear down the whole credential when it is: the
profile, the pin, the local key, and the session. Deleting a keyless row while leaving the session
behind would be the worse half of the job, since the session still spends and `resolve_endpoint`
picks it up again on the next turn. Every other provider row deletes as a row.

**Signing in is the whole of it.** The session token opens both prefixes: `/auth/*` for plans and
purchases, and `/v1/*` for model calls. There is no key to fetch, nothing to paste, and nothing
written to disk but the token — the Aizen subscription is sold by sign-in, not by a string.
`aizen login` (device pairing) is the older door and still hands out a key; it is for machines that
cannot open a browser at all. With a browser, use this one.

**The browser is the default and `--password` is the exception**, because most accounts have no
password: created through Google or GitHub, their `password_hash` is NULL, and a password prompt
tells the majority they typed their own password wrong.

**Leaving is one word.** `/logout` in the REPL and `aizen logout` in a shell drop both credentials
— the session and this machine's pinned one — because from outside they are one thing, and while
the plan rides the session a logout that dropped only the pin would leave somebody who typed the
word still able to spend. `aizen account logout` and `aizen gateway logout` are the narrow ones.

The pinned half now really is cut: `aizen logout` calls `POST /v1/device/logout` with this
machine's own credential, which cuts this machine's device row and nothing else, effective on its
very next call. **The local teardown happens whatever the server answers** — 200, 401 and a dead
network all mean one thing here, and the `forget_token` in the reply is an instruction rather than
a suggestion, since the server cannot reach this disk. Calling twice is another 200: the route
reports a state, not an event. The account session half still revokes nothing — that token stays
valid on other machines until it expires.

**The token is a bearer credential.** It is stored owner-only, never printed, never logged, and
never a field of `--json` output. It lives 30 days with no refresh route, and a password change
anywhere kills every token minted before it — so a `401` in the middle of a run means *sign in
again*, not a network hiccup to retry. The CLI says exactly that when it sees one, and `401` is
absent from the retry table on purpose.

At `/v1` only the `Authorization` header opens the door; a cookie does not. The CLI sends none, and
should not gain one: a cookie that spent money would make every open browser tab a wallet.

The browser flow binds `127.0.0.1:0` *first* (the port is part of the URL), opens
`/auth/cli/authorize?port=…&state=…`, catches the redirect, and trades the code at
`/auth/cli/exchange` for the same JWT a password login returns. It waits **ten minutes**, not the
code's two: that clock starts only once you have a session, and step one is often a whole round trip
through Google.

Two things are worth knowing here as well:

* **`state` is the only defence.** A page in the same browser can point at `/auth/cli/authorize`
  while the listener is up, and the redirect that follows is indistinguishable at the socket. A
  mismatched `state` means the code is discarded and never exchanged — exchanging it would burn a
  stranger's code and adopt whatever account it belonged to. The page returned to the browser also
  says only fixed sentences: `?error=` is chosen by whoever provoked the redirect.
* **The code burns once, and its three refusals are three different problems.** `404` no such code,
  `409` already used, `410` expired. Each is printed in the server's own words rather than flattened
  into "invalid code", and all three exit `2` — a retry is the one thing that cannot help.

### `aizen key` — your keys, and what your plan may call
```bash
aizen key ls                       # your keys; the plan's row is marked
aizen key reveal --id k_123        # the string of a key you made yourself (--id required)
aizen key loadout ls               # the plans it may call, in `auto` preference order
aizen key models                   # what those plans resolve to today
```

**Your plan is not a key you can copy.** Signing in is what buys it, and no route emits a string for
it. A row of `api_keys` does stand behind each account server-side — it carries the loadout, the
per-minute ceiling, the budget and every `usage_history.api_key_id` — but it is an internal detail
this CLI never holds.

So there is no `key show` and no `key rotate`. They existed briefly against a contract that had
`/auth/plan-key` and `/auth/plan-key/rotate`; those routes were withdrawn before they shipped and
answer `404` permanently. `reveal` takes no default for the same reason: guessing the plan's row
there could only produce a refusal that reads like a bug in the command, so it names the row and
says why instead.

Keys still exist in two places, and neither changed: a key **you made yourself**, for models bought
from a seller, and your own key at the far end of a BYOK endpoint (`mine/gpt-4o`), which bills there
and never touches a plan.

There is no revoke subcommand and none should be added — `/auth/keys/{id}` is DELETE, and the
server refuses it at the plan's row anyway.

Your plan's row also breaks three expectations an ordinary key sets:

* it calls **Aizen's own plans only** — a seller's model through it is a `403`, and belongs on a key
  you made yourself;
* an **empty loadout means it can call nothing**, the opposite of an empty allow-list on any other
  key, so a freshly issued one refuses everything until something is loaded;
* its loadout holds **plans, not models**, at most 10, and the order is meaning: `auto` resolves to
  the first entry callable at that moment. `auto` points *into* the list and can never be in it.

Two route shapes are worth knowing because getting them wrong is expensive. The id goes **last** —
`/auth/keys/reveal/{id}`, `/auth/keys/loadout/{id}` — because `/auth/keys/{id}` is the revoke route.
And the plan key is found by the `plan_key` flag on each row, never by its label: one issued through
the admin door is labelled with the owner's email, not `main`.

### `aizen sub` — plans, combos, model subscriptions, plugins
```bash
aizen sub plan ls                     # what a plan grants, costs and caps
aizen sub plan buy pro                # spends karma — confirms first unless --yes
aizen sub combo ls                    # the marketplace shelf
aizen sub model add nbz/glm-5-air     # subscribe (free; usage is charged at call time)
aizen sub model ls --all              # what you hold, cancelled ones included
aizen sub model confirm nbz/glm-5-air # accept a price change and unblock it
aizen sub plugin buy <slug> --code X  # the coupon price is the server's quote, never recomputed
```

Everything here runs on the account session, never on a gateway key, and nothing here mints,
reveals or loads one.

**Buying a thing and being able to call it are two different questions**, and the gap between them
is the source of this API's most confusing `403`:

* the **plan key carries Aizen's own plans only**, so a seller's model bought through `model add` is
  subscribed and still refused through that key — it needs a key of your own (`aizen key ls`);
* an **empty loadout on the plan key means it can call nothing**, inverting what an empty allow-list
  means on every other key, so a freshly bought plan refuses everything until `aizen key loadout`
  holds it.

Both sentences are printed at the moment of the purchase rather than left for the first failed call.

Three details that decide whether a client is correct:

* **Spends never retry.** A purchase whose answer was lost must not be sent again — the second send
  charges twice, and the server's `409` is a last fence rather than a contract (a plugin *renewal*
  is legitimately repeatable, so it cannot catch a double send at all).
* **`null` is not `0`.** A null `quota_units` is "no cap" while `0` is a real cap of zero; a null
  `karma_price` means "not sold for karma", which is the opposite of free. `owned` on a plugin is
  the string `none`/`active`/`expired` — read as a boolean it is truthy in three cases out of three.
* **A listing id is cut from the END.** The model is the last segment and the namespace before it
  may hold further `/`, so `two/slashes/here` is a legal id, not a typo. The `/` also stays a real
  separator in the URL path: `%2F` makes the route stop matching.

Own endpoints (`mine/…`, BYOK) do not pass through this door at any point — they are billed by your
provider at the far end, spend no karma, and hold no subscription. `aizen sub model add mine/gpt-4o`
is refused here rather than at the server, because the answer is `aizen custom`, not a retry.

### `aizen config` — provider-first setup (recommended)
Run it with no subcommand for the config dashboard. **Providers & connection** is the first row: add a
name, endpoint, API key, and default model once, then switch by choosing that named row. The same
manager supports Use, Edit, Rename, and Delete; API keys are masked in every list/display.
```bash
aizen config            # Providers & connection → Add provider
```
After this, `aizen chat`/`agent`/`workflow` work with **zero env vars**. Non-interactive equivalents:
```bash
aizen config set --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini
aizen config show       # API key masked
aizen config path
```

Save complete URL + key + model profiles when you have more than one compatible gateway, then switch
manually without restarting the REPL:

```bash
aizen config provider add primary --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini --use
aizen config provider add backup --base-url https://backup.example/v1 --api-key bk-... --model model-x
aizen config provider list
aizen config provider use backup
aizen config provider edit backup --base-url https://backup-2.example/v1 --api-key bk-2... --model model-y
aizen config provider rename backup secondary
```

Inside the REPL, `/provider` is the fast one-pick switcher. `/provider add` opens the add wizard,
`/provider manage` opens Use/Edit/Rename/Delete, and `/provider backup` switches directly. The next turn and health probe use the selected URL, key, and model. This is manual failover, not an
automatic retry/failover chain. `AIZEN_BASE_URL`, `AIZEN_API_KEY`, and `AIZEN_MODEL` still override the
saved selection; Aizen prints a note when those environment variables mask a switch.

Adding or re-pointing a provider starts from the preset list (OpenAI, ChatGPT Codex, Anthropic,
OpenRouter, Groq, DeepSeek, OpenCode, Ollama, or **Custom gateway** to type your own URL) — the same
list first-run setup shows, so a provider added in a later release is reachable without reinstalling.
The preset also supplies the default profile name, and its URL already carries the version suffix.

Changing an existing provider's endpoint asks for a new key instead of offering the previous
endpoint's credential. Cancelling any wizard step leaves the complete saved profile unchanged.

**OpenCode (free)** is a preset for OpenCode's zen gateway — a zero-cost way to try an agent before
bringing a key. The base URL is `https://opencode.ai/zen/v1` and the free tier's shared token is the
literal string `public` (type it at the key step; no sign-up anywhere). Its free models are the
`-free`-suffixed ids in the live list (`deepseek-v4-flash-free`, `mimo-v2.5-free`, `hy3-free`, …) plus
`big-pickle`; `aizen models` and the model pickers tag those rows with `· free` so a free-tier id
stands out from the paid ones in the same list. Expect free-tier rate limits — a 429 is retried with
the gateway's `Retry-After` like any other transient failure. The gateway reports no context
windows, so the HUD estimates them from the model name until you set one.

Sub-agent configuration uses the same provider list. In `/config` → **Sub-agents**, choose a saved
provider and either its default model or a model override for Sub-agent default, Summarizer, Oracle,
Apply, one of the seven Pantheon roles (`roles.pantheon.<role>`, above the sub-agent default), or an
installed specialist. No endpoint/key is retyped. Scriptable specialist equivalent:

```bash
aizen agents set-provider code-reviewer backup              # provider default model
aizen agents set-provider code-reviewer backup model-y      # model override
aizen agents set-provider code-reviewer --clear             # inherit sub-agent default
```

Direct role URLs/keys, model→endpoint mappings, and endpoint fields in specialist cards remain
supported as advanced compatibility overrides. Environment variables remain highest precedence;
advanced overrides can therefore mask a provider selection and are labelled as such in `/config`.

Gateways differ in how they shape their streaming frames, and Aizen absorbs the differences quietly:
a frame it cannot read strictly is retried leniently, and whatever is still unreadable is keepalive
noise it drops without a word. If a new gateway ever *does* go quiet or lose tool calls on you, set
`AIZEN_DEBUG_STREAM=1` to print the offending frames (capped at 3 per response plus a total) — that
output is the useful thing to attach to a bug report.

A stream has two deadlines: `AIZEN_STREAM_FIRST_FRAME_SECS` (default 600) until the first frame
parses — a reasoning model that streams nothing until its answer starts is legitimately silent
for minutes — and `AIZEN_STREAM_STALL_SECS` (default 90) between frames after that. A stream that
dies or goes quiet before producing anything is replayed up to twice; once it has produced text or
a tool call it is never replayed, so nothing is duplicated. Some models reject a request field
(`max_tokens` on o-series/gpt-5 models, `parallel_tool_calls` or `tool_choice` on strict local
servers, `cache_control` on some gateways, `reasoning_effort` out of range): the 400 is read, the
field is dropped or renamed (`max_tokens` → `max_completion_tokens`), the request is re-sent, and
the model is remembered for the session so it costs one failed call per model, once.

### `aizen models` — list the provider's models
```bash
aizen models                       # GET {base}/models, marks your default
aizen config set --model <id>      # pick one as the default
```

The memory brain lives under `~/.aizen/cli-memory/` (override the root with `AIZEN_HOME`).
Memory commands are fully offline — no creds needed.

Retrieval is **Unicode-aware**: the lexical tokenizer NFC-normalizes before lowercasing and
splits on `\p{L}\p{N}_`, so Vietnamese (and any accented script) is matched whole instead of
being shredded to ASCII fragments. This is pure-Rust and adds no dependency to the static binary.
Measured on the recall bench, Vietnamese paraphrase recall@5 went 0.00 → 1.00 with literal/English
recall unchanged. Verify with `aizen bench memory --split all`.

Ranking is **BM25** (Okapi k1=1.2, b=0.75, floored IDF over the active corpus + length
normalization) — term rarity and doc length now shape relevance, so concise on-point facts beat
verbose keyword-stuffed ones. A pure-Rust Jaro-Winkler fuzzy bridge for typo'd query terms is
implemented + unit-tested but **off by default** (on the current corpus it adds candidate noise
without a recall gain; one flag from on).

The store **evolves from reuse** — no LLM, zero extra tokens. Every fact the agent retrieves into
context is reinforced (at most once/day); ranking is `bm25 · decay · salience`, where reused facts
decay slower (`half_life·(1+ln1p(reinforced))`) and gain salience (`0.5 + 0.3·reuse + 0.2·recency`,
capped so BM25 stays dominant), and the always-on frozen core is packed salience-greedy so the
prompt prefix holds the facts you actually use. This is provable, not marketing: `aizen bench memory
--evolution` runs a 6-session reuse simulation and **fails** unless recall@5 climbs ≥5%/session
until it plateaus.

## Commands

### `aizen chat` — one-shot streaming chat
```bash
aizen chat -p "explain this error: ..."
echo "summarize this" | aizen chat        # prompt from stdin
```

### `aizen agent` — the tool-using loop
The model reads/edits files, runs shell, and uses memory to finish a task end-to-end.
```bash
aizen agent "add a --version flag and update the help text"
aizen agent --yes "fix the failing test in src/parse.rs"   # pre-approve file/shell ops
aizen agent --max-iters 40 "..."                            # raise the step cap
aizen agent --save-session "..."                            # keep the transcript for /sessions
aizen agent --effort high "..."                             # this run only; the config is untouched
aizen agent --image shot.png "why is this button misaligned?"  # vision: repeat --image for more
```
Behavior worth knowing:
- **Nothing is saved unless you ask.** This subcommand is also the scripting and CI entry point, so
  it writes no session file by default — a file per invocation would bury the pool `/sessions` reads.
  With `--save-session` the finished conversation is written to `~/.aizen/sessions` with the same
  provenance stamp the REPL writes (project key, root and slug), so `/sessions` reopens it without
  caring which surface produced it, and the path is printed to **stderr** — stdout stays the answer.
  A run that ends in an error is saved too: it still happened.
- **Effort is per run, not per config.** `/effort` in the REPL pins a tier by writing
  `reasoning_effort` into the config; `--effort` arms the same per-turn override the REPL arms and
  persists nothing, so a front-end can send one hard run and one cheap one without editing the
  user's settings between them. `--effort auto` runs the same keyword-plus-complexity classifier a
  typed REPL turn goes through, against the task text. Omit the flag and nothing changes: the
  configured `reasoning_effort` applies and the request is byte-identical to one from a core that
  never had the flag. The tier is named on **stderr**, next to the rest of the trace.
- **A tier changes the harness, not only the wire.** In the REPL the resolved tier also sets the
  step cap and its extension, how many fresh budgets a still-progressing run may claim, how many
  verify-and-fix rounds a broken tree gets, whether the self-review pass runs before Done
  (`xhigh`/`max`), and how much of a build log reaches the model — so `/effort low` and
  `/effort max` behave differently even on a provider that ignores `reasoning_effort`. A tier can
  also change the model: `"models_by_effort": {"low": "cheap-model", "max": "strong-model"}` in
  `cli-config.json` sends turns of that tier to that model on the same endpoint (the effort line
  then names it); tiers without an entry use the main model.
- **Architect mode under `max`.** A multi-file turn at `max` effort is planned first: `metis` on
  the strongest configured model (`models_by_effort.max`, else `xhigh`, else the turn's model)
  writes an ordered plan in prose — file:line anchors, the change and its check per step, what
  not to touch, the verifying command, no code — and the turn's own loop then applies it on the
  fastest model (`models_by_effort.low`, else the same model) at low wire effort, the plan folded
  into the request under `<architect_plan>` and the `max` budgets (steps, verify rounds,
  self-review) kept. The status line says `architect: plan by metis on X → applying on Y`.
  Single-file turns, questions and research never enter it; a planner that fails or returns
  nothing leaves the turn as it would have run. Off with `architect_mode: false` or
  `AIZEN_ARCHITECT=0`.
- **Images are attached, not described.** `--image <PATH>` inlines a PNG/JPEG/GIF/WebP (≤ 8 MB) into
  the first user message as an `image_url` data part — the same wire shape the REPL produces when you
  drag a file onto the window or press Ctrl-O to grab a screenshot — so a front-end driving this
  subcommand gets vision without driving the REPL. Repeat the flag for more than one image. The model
  must be vision-capable; the files are read **before** the endpoint is resolved, and a path that is
  not a readable image ends the run there rather than being skipped, because an attachment that
  silently vanished would yield a confident answer about an image nothing ever sent. Note the
  difference from the REPL: a typed line only lifts paths that really are images and leaves anything
  else as prose, which is right for something a human typed — a flag is deliberate, so it is loud.
- **How tools reach the model** — the session's tool registry is the single source of truth. Its
  definitions (name, description, JSON Schema) are sent through the provider's **native** tool field:
  OpenAI-compatible gateways and Anthropic get `tools[{type:"function",function:{…}}]`, the ChatGPT
  Codex endpoint gets the same tools in the Responses dialect's flat shape. The system prompt carries
  only a compact **`# Tool routing`** map — capability → the exact tool names enabled *right now* —
  generated from that same registry, never the schemas. So `/tools`, `/lsp off`, `/apps`, a missing
  Telegram token or a build without `--features browser` all remove a tool from the prompt and the
  request together, and the model is never told about a tool it cannot call.
- **Parallel reads** — when a turn only reads (file_read/glob/memory), the calls run
  concurrently; any turn that edits or runs shell stays serial (and approval-gated).
- **Continuing earlier work** — a fresh conversation's prompt lists this project's recent saved
  conversations in a `<sessions>` block, and the read-only `session_recall` tool returns a clipped
  digest (opening request + latest exchanges) so "continue the most recent session" resumes the
  work instead of sending the model hunting for transcripts. Restoring a full transcript stays
  yours: `/resume`.
- **Scratch directory** — `<environment>` names a per-run `scratch:` path (under the OS temp dir)
  where the agent is told to put throwaway helper files instead of your repo or cwd; abandoned
  scratch dirs are swept automatically a week after their run ends. Two things land there
  without being asked: the full text of any tool result over 16 KB (the cut result names the file),
  and the full text of every tool result that has aged past the eight most recent and been
  collapsed to a one-line digest — see the context notes under the REPL section.
- **Calls written as text are still calls** — arguments that are almost JSON (a trailing comma, a
  raw newline inside a string, Python quotes, a brace cut off by `max_tokens`) are repaired and the
  repair is traced; a call a local model wrote into its text — `<tool_call>…</tool_call>`, a
  ```json fence, a bare `{"name": …, "arguments": {…}}` reply or Mistral's `[TOOL_CALLS] [...]` —
  is executed when the native `tool_calls` array is empty and every name is a registered tool.
  Prose with no such block, or a block naming an unknown tool, is left exactly as written.
- **Diagnostics ride the next result, and the loop checks after three edits** — the post-edit
  LSP fold waits 300 ms; a slower analysis finishes in the background and its diagnostics — the
  edited file's, plus new errors in up to three caller files — are appended to the next tool
  result, or become a demand before Done when the model is about to finish. After every three
  successful edits the loop runs the project's fast check itself and appends the verdict to the
  last edit's result; a pass satisfies the verify gate. `harness_check_after_edits` (default 3,
  `0` off) sizes the batch.
- **Edits: `replace_all` on every rung, `dry_run`, and a diff the model does not re-read** —
  `file_edit` matches on a ladder (exact, indentation-tolerant, whitespace-normalized, …) and
  `replace_all` now applies on whichever rung matches. `dry_run: true` shows the diff and writes
  nothing. The result the model sees keeps the removed lines, the `@@` line anchor and a
  `+N line(s)` count per hunk (three hunks at most, the rest summed); the terminal still shows
  the full diff. `file_glob` sees everything by default (dotfiles, `target/`, `node_modules/`);
  `ignore: true` honours `.gitignore` and skips the heavy dirs the way `search_files` does. The
  five search tools — `file_glob`, `search_files`, `codebase_search`, `lsp_workspace_symbol`,
  `read_symbol` — end their descriptions with the same routing sentence.
- **Concise tool output by default** — `shell_run`, `process` and `search_files` take
  `format: concise | detailed`. Concise, the default, changes nothing for a short result; a log
  over about 4,000 characters is cut to its status line, the head, the first error and the tail,
  with a second line stating the line and byte counts and the scratch file that holds the full
  text; a search past 40 rows shows those rows and counts the rest per file, the whole list on
  disk. `detailed` returns everything up to the loop's budget (16 KB for logs), and the 16 KB
  spill still applies above that.
- **Approval** — destructive tools (`file_edit`, `shell_run`) prompt before running, and the
  prompt shows what the call WILL do first: an edit's patch in the diff box (computed without
  writing), a write's create-or-overwrite line with its patch, a shell command's directory and
  full command line, a move's both ends — the same payload reaches a Telegram approval. Edit
  headers carry the repo-relative path. In the sticky
  REPL each one shows an inline **`[y]es · [n]o · [a]llow all this session`** prompt (the `[a]`
  choice is a session-scoped temporary Yolo grant, reset by `/clear`). `/approval ask|smart|yolo`
  is the three-level setting: `ask` prompts, `smart` auto-runs read-only-shaped shell, and `yolo`
  pre-authorizes all non-floor operations. It applies to **this window only** unless you add
  `--persist`, which writes it to `cli-config.json` as the default every new window, `aizen serve`
  lane and cron job starts from — a one-off `/yolo` in one terminal no longer arms the whole
  machine. Legacy `/smart` and `/yolo` toggles are session-scoped the same way. Non-TTY (CI/pipes) safely denies unless `--yes` is set;
  under `aizen serve` the prompt is routed to your phone. The hard `cmd_guard` floor blocks catastrophic
  commands underneath all of these.
- **Verify gate** — after an editing run, a fast check runs before the agent reports done and
  its errors are fed back for a fix turn: `cargo check`, a `typecheck` npm script or
  `npx tsc --noEmit`, `go build ./...` then `go vet ./...`, `mvn -q -DskipTests compile`,
  `gradle -q compileJava` (through the repo's wrapper when it ships one), `dotnet build`, or a
  Python byte-compile pass (`python -m compileall`, syntax only — Python has no universal
  typecheck). A toolchain that is not installed counts as "nothing ran", never as a failure. When
  nothing could run at all — no recognised manifest, no toolchain — the model is asked once to
  run the project's own build or test command and quote the result before finishing, instead of
  reaching "done" unverified in silence.
- **Sub-agents (the Pantheon)** — the agent can call the `task` tool to delegate a self-contained
  sub-task to a fresh role-scoped sub-agent. Seven built-in roles, each with its own tool scope
  and embedded working method: `argus` (searcher — read-only, repo-local), `metis` (planner —
  read-only), `daedalus` (coder — the only role that edits; read/edit/shell), `nemesis` (reviewer
  — read-only), `themis` (tester — shell, no edit), `clio` (librarian — read-only web research),
  `mnemosyne` (historian — read-only memory + session recall, no web). Every role gets
  `git_inspect`, a read-only git window (status/log/diff/show/blame), so a reviewer can see the
  diff it reviews without holding a shell. The legacy names `coder`/`planner`/`reviewer`/`tester`
  are still accepted everywhere a role is named (deprecated: result headers answer with the
  canonical name). An unknown `agent` or `role` is refused with the real list — never silently
  substituted. With neither given, the dispatch runs as `argus` (the safe read-only default;
  editing must be asked for by name: `role=daedalus`). Read-only dispatches fan out in parallel;
  write-capable ones stay serial. Single depth: a sub-agent cannot spawn further sub-agents.
  Each role has its own default step budget — argus 15, clio 20, metis / nemesis / mnemosyne 25,
  themis 30, daedalus 45 (`max_steps` overrides; cap 80) — and can be pinned to its own provider
  or model: `roles.pantheon.<role>` in `cli-config.json`, or `/config` → Sub-agents → Pantheon
  roles (env `AIZEN_<ROLE>_MODEL` wins; an explicit `model` on the dispatch beats the pin;
  unpinned roles take the sub-agent default). A child does not start from zero: its brief
  opens with a `<parent_context>` block carrying the parent's in-progress todo item, the
  findings passed in the `context` arg (up to ten short lines), and up to fifteen
  `path:start-end` locations the parent already read in this conversation (from its read
  cache), capped at 2,500 chars — so the child goes to those lines instead of searching, and
  does not re-derive what the parent states as established. Every child's full report is also
  filed on the conversation's blackboard (`<scratch>/blackboard/<scope>/<child>.md`, append-only,
  named in each child's `<environment>` with the notes already there), so a later child can
  `file_read` a sibling's whole report. The workspace writer lease is keyed by scope: a child
  reenters its parent's lease, a sibling scope (another `serve` lane, a parallel dispatch)
  waits for the OS lock and is told who holds it. An approval a child raises is attributed
  (`daedalus · fix parser wants: Run …`); `/workflows` shows each child's current step and its
  own tokens (`12.3k→1.2k tok`), which the `task` result header, the workflow status lines and
  `--trace` repeat. A write-capable `task` child that runs out of time or fails verification is
  retried once with a tightened brief (its partial report attached); a second failure restores
  the checkpoint taken before the dispatch and the result header says `retried` and
  `AUTO-RESTORED`. A brief under 80 chars that names no file or symbol is refused (`brief too
  thin`) — a child starts from an empty context, so the brief must say what to look at and what
  to return.
  Example: `task(agent="argus", prompt="find every caller of parse_server_line …")` — and a solid
  change flow is one `daedalus` implementation followed by separate `themis` (verify) and
  `nemesis` (review) dispatches.
- **Specialist cards** — markdown personas under `.aizen/agents/` / `.claude/agents/` still
  dispatch via `task(agent="<slug>")`. **Migration note:** a card with no `tools:` line now runs
  READ-ONLY (it used to receive the full coder scope implicitly). A card that needs to edit or
  run commands must say so in frontmatter — add e.g. `tools: Edit, Bash` (a shell grant carries
  the background `process` pool with it). Runtime capability always comes from the resolved tool
  registry, never from the card's prose.
- **Clarify, don't guess** — when a choice is genuinely ambiguous and a wrong guess would waste
  real work, the agent calls `clarify` to ask ONE question; the turn pauses and your next message
  is the answer (in the REPL, the plain prompt, or over Telegram — no stdin contention with the
  input box). For low-stakes choices it assumes and states rather than stalling.
- **Web research** — `web_search` (needs a free Tavily key — set `TAVILY_API_KEY`) finds pages; `web_fetch` GETs a URL and
  returns it as readable text (HTML reduced to prose, capped); `web_crawl` spiders a site from a
  seed URL (see `aizen crawl` below). Read-only; available to every role except `argus`, whose
  whole job is inside the repository.

### `aizen workflow <spec.json>` — fan-out + synthesis
Run several role-scoped sub-agents concurrently (bounded to a machine-derived cap, shared with
in-REPL dispatches), then merge their results into one answer (mixture-of-agents). See
[examples/review.workflow.json](../examples/review.workflow.json):
```bash
aizen workflow examples/review.workflow.json
```
Spec shape:
```jsonc
{
  "name": "review-changes",
  "tasks": [ {
    "id": "bugs", "role": "nemesis", "prompt": "...",
    "model": "optional-per-task",
    // optional dispatch contract — same semantics as the task tool:
    "boundaries": "Do not edit files",
    "expected_output": "Findings with severity and file:line evidence",
    "context": ["what the parent already established — the child does not re-derive it"],
    "after": ["implement"],               // wait for these tasks; their reports ride in ahead of the brief
    "retry_on_fail": "implement",         // on VERDICT: FAIL, re-run that task once with the failure, then this one
    "max_steps": 25,                      // total step budget (default: the role's own; cap 80)
    "expects": { "type": "object" }       // JSON Schema the child's answer must satisfy
  }, ... ],
  "synthesis": { "model": "optional-override", "prompt": "optional merge instruction" }
}
```
Roles set each sub-agent's tools (see the Pantheon above; omitted role = `nemesis`, read-only —
legacy role names in existing specs keep working, unknown ones are refused). The contract fields
travel INTO the child's prompt exactly as they do on a `task` dispatch; an `expects` schema is
validated (one repair attempt) and the task's status carries `json:ok`/`json:invalid`. A failed
task never aborts the workflow — its result is captured and the synthesis still runs. The synthesis uses `AIZEN_MODEL` unless `synthesis.model` overrides it.
**Model diversity (mixture-of-agents):** each task may set its own `model` (e.g. a cheap model
scouts, a strong one reviews) — else the workflow default. `--trace <path>` writes a JSON audit of
the fan-out (per-task model + outcome + the synthesis model).
**Chains and the fix loop:** `after` orders tasks into dependency waves (Kahn order; unknown
ids and cycles are refused). Within a wave the read-only tasks fan out first and the wave's one
writer (`daedalus`, or `themis`, which holds a shell) runs alone, so a reviewer never reads a
tree the implementer is mutating — put the review AFTER the change with `after`. Two writers
may share a workflow only when `after` orders them. A chained task's brief opens with an
`<upstream>` block carrying its dependencies' latest reports (4,000 chars each, 12,000 total).
A task with `retry_on_fail` whose report opens with `VERDICT: FAIL` re-runs the named upstream
task once with the failure attached, then runs again; the trace keeps both attempts as `id`
and `id#2`. Workflow writers run under the verify gate like a `task` writer. From the REPL,
`workflow(mode="implement", prompt="…")` prebuilds implement (daedalus) → verify (themis,
first line `VERDICT: PASS|FAIL`) → review (nemesis) with that fix loop; the same spec as a
file is `bench-fixtures/workflows/implement.json`.

### `aizen crawl <url>` — katana-style web crawler
BFS over HTTP from a seed URL: extracts links from HTML (`href`/`src`/`action`) and endpoints
from JS (regex over quoted paths/URLs), follows the in-scope, unseen ones up to a depth/page cap.
Pure Rust — no headless browser, no passive sources (those would break the single binary).
```bash
aizen crawl https://example.com                       # depth 2, same host, ≤200 URLs
aizen crawl https://example.com --depth 1 --max-pages 50 --show-source
aizen crawl https://example.com --scope subs          # also follow *.example.com subdomains
aizen crawl https://example.com --json                # [{url, depth, via}]
```
Only GET requests; scope defaults to the seed host (`--scope subs` for subdomains); `--max-pages`
is a hard ceiling. Also exposed to the agent as the `web_crawl` tool.

### `aizen sandbox` — the OS sandbox around model-run commands

```bash
aizen sandbox status          # live capability matrix: enforced / partial / advisory / unavailable
aizen sandbox doctor [--json] # probe backend, audit-log check, real env-scrub self-test, tmp sweep
aizen sandbox explain         # how the next shell_run would be sandboxed, step by step
aizen sandbox run -- <cmd>    # run one command under the sandbox by hand
aizen --sandbox strict …      # per-invocation mode override (also AIZEN_SANDBOX, /sandbox, config)
```

Modes: `auto` (default — strongest backend, guarded fallback for interactive sessions only) ·
`strict` (kernel enforcement or refusal) · `guarded` (software guards, said plainly) · `off`.
Config lives under `"sandbox": {…}` in `~/.aizen/cli-config.json`. Threat model, per-platform
matrix and limitations: [SANDBOX.md](SANDBOX.md).

### `/persona` — a character that evolves
A **persona** is *who the agent is* — a third identity layer alongside **user_memory** (who you are)
and **skills** (how to do things). Cards live as human-editable markdown under
`~/.aizen/personas/<name>.md` (frontmatter `name`/`role`/`voice` + body). The active one renders
into a `<persona>` block in the system prompt; switching applies to the current chat **in place**
(no lost history).

Three ways to make one in **`/persona`**: **New** (type name/role/voice + a multi-line body),
**Paste a character prompt → auto-create** (paste any character/system prompt — the model distills
it into a structured card for you), or just select an existing one. (Pasting a character prompt as a
normal chat message only role-plays for that one turn — it isn't saved, doesn't survive `/clear`, and
isn't cached. Use paste-to-create to make it persistent.)

A fourth way needs no menu: **just ask in chat.** "Create a persona named Mira, a wry noir
detective, and be her" → the agent fills in name/role/voice/backstory and calls the **`persona_create`**
tool (approval-gated, writes the card). By default it switches to the new character, which goes live
from your **next message** (the switch happens at the turn boundary so the prefix cache stays warm).

**It grows like a human** (Generative-Agents pattern, on by default when a persona is active):
- **Self-memory (`<self>`)** — after each turn the character records a *free* episode of what it
  lived through, importance-scored (corrections + real work + substance score higher). The top
  experiences (`importance × recency`, insights weighted up) are injected as a `<self>` block so the
  character carries its past forward. Stored per-persona under `~/.aizen/personas/<slug>.self/`.
- **Reflection** — once enough formative experience piles up, one model call distills recent episodes
  into durable first-person **insights** (`🌱 … reflected — +N insight(s)`). This is what makes the
  character deepen across sessions instead of resetting.

`/persona` also has: **View self-memory** (insights + recent episodes), **Reset self-memory**, and an
**Evolution ON/OFF** toggle. Off → a frozen character. Toggle persists via `persona_evolve`
(`aizen config set --persona-evolve false`). Honest framing: this is a *consistent, accumulating,
self-reflecting character* — not raised model intelligence.

Scriptable, fully offline (no creds) — handy for setup and for inspecting what the model sees:
```bash
aizen persona new Aria -r "a sharp mentor" -v "concise, warm" -b "You value clarity."  # body via -b or stdin
aizen persona use Aria / clear                         # set / clear the active persona
aizen persona list / show <name>                        # list (● active, with self-memory counts) / show a card
aizen persona self [name]                               # view accumulated insights + recent episodes
aizen persona remember "what I just lived through"      # record a free episode (auto-scored importance)
aizen persona block                                     # print the <persona> + <self> blocks the model sees
```

### `aizen soul` — the agent's operating identity
Where a **persona** is a swappable costume, the **SOUL** is *who the agent is operationally* — durable
values and policies that hold across EVERY persona and project (e.g. "always run tests before claiming
done", "reply in Vietnamese", "never push without asking"). It lives at `~/.aizen/SOUL.md`
(**HOME only, never cwd** — so a cloned repo can't silently rewrite the agent's rules) and renders into
an `<agent_identity>` block **above** `<persona>` in the system prompt, reaching chat / agent / serve /
workflow alike. The body is sanitized + secret/injection-scanned before injection (fail-closed: a
poisoned line drops the whole block).
```bash
aizen soul set -b "Always run tests before saying done. Never push without asking."  # body via -b or stdin
aizen soul show        # print the <agent_identity> the model actually sees
aizen soul path        # ~/.aizen/SOUL.md — edit directly in any editor
aizen soul clear
```

### `aizen skill` — reusable procedures (skills)
A **skill** is a saved step-by-step playbook (deploy the VPS, cut a release, triage logs) — distinct
from **memory** (facts/preferences). Skills live as human-editable markdown under `~/.aizen/skills/`.
A compact index (`name: when`) is injected into the agent's system prompt (`<skills>`); the agent
pulls a skill's full steps on demand with the **`skill_load`** tool, and can persist a new one with
**`skill_save`** (approval-gated). Manage them from the REPL with **`/skills`** (list · view · new ·
**fetch from URL** · delete), or the CLI:
```bash
aizen skill add deploy-vps -d "ship over SSH" -w "asked to deploy"   # body from --body or stdin
aizen skill fetch https://example.com/deploy-vps.md                  # pull a shared skill from a URL
aizen skill list / show <name> / delete <name>
aizen skill where                                                    # the three folders + counts
```

Optional frontmatter narrows when a skill shows in the index: **`requires:`** (tool names — hidden
unless every one is in the live tool surface, so a `browser_*` skill is silent when browser tools
aren't built) and **`platforms:`** (`linux`/`macos`/`windows`, or `unix`/`posix` — hidden off-OS).

**Self-learning** — after a completed multi-step task, the REPL distills a *generalizable* procedure
into a new skill automatically (conservative: skips one-offs and duplicates; prints `↯ learned skill
'…'`). It fires when a turn did real work (**≥4 tool calls**) **or recovered from a dead end** (a tool
errored, then a later call succeeded — that hard-won path is worth saving). It's like memory's
self-learning, but for how-to. Toggle in `/config` (default on) or `aizen config set --auto-skill-learn false`.

### Custom slash commands — markdown macros you fire
Where a **skill** is something the *agent* pulls when relevant, a **custom command** is a prompt-macro
*you* fire by name. Drop a markdown file in `~/.aizen/commands/` (global) or `./.aizen/commands/`
(project — git-check it in to share with your team); a subdir namespaces it (`git/commit.md` →
`/git:commit`). Project files win over global on a name clash. List them with **`/commands`**; they
also show in the bare-`/` picker and `/help`.

```markdown
---
description: Review the staged diff for bugs and risky changes
argument-hint: [path]
---
Review this staged diff and flag bugs, security issues, and risky changes:
!`git diff --cached $ARGUMENTS`
```

The body is expanded at fire time, then submitted as a normal chat turn (full agent loop + tools +
memory apply):
- **`$ARGUMENTS`** → everything you typed after the command; **`$1`..`$9`** → positional words.
- **`@<path>`** → inlines that file's contents (confined to the working dir; only at a word boundary,
  so emails/handles pass through).
- **`` !`cmd` ``** → splices a shell command's output, but **only read-only commands run** — it goes
  through the same safety floor as the agent, so a blocked or write/network command is refused, never
  executed silently.

### MCP servers (`/mcp`) — bring your own tools
`aizen` can use tools from any [Model Context Protocol](https://modelcontextprotocol.io) server. Declare
them in `~/.aizen/mcp.json` (the same `mcpServers` shape Claude Desktop uses) over **stdio** (a
local child process) or **HTTP** (a remote endpoint):

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "remote":     { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer …" },
                    "include": ["search", "fetch"] }
  }
}
```

At startup `aizen` connects each enabled server, lists its tools, and exposes each one to the agent as
**`mcp_<server>_<tool>`** (per-server `include`/`exclude` filters which). External tools are
**approval-gated by default** (unless the server marks a tool read-only). A pure-Rust client — no
Node/Python MCP SDK, no extra runtime; the single static binary is preserved. `aizen mcp list` or
**`/mcp`** shows the manager/connection generation, sanitized health, pinned schema hash, and tools.

**Tool Search — big surfaces stop bloating the context.** Every advertised tool's JSON Schema rides
on *every* request, so a few schema-heavy servers (GitHub-sized) can burn thousands of tokens per
turn before a single call is made. Aizen defers them instead: a deferred server's tools are still
fully callable, but their schemas leave the request — the agent discovers them through a small
`tool_search` tool whose results carry each match's full schema in-band, then calls the found tool
directly by name. The request's tool list stays byte-stable all session, so the provider's prefix
cache is never invalidated by connecting more integrations, and this works on ANY endpoint (it is
client-side — no provider feature required). Control it per server with `"defer": true` (always
deferred) / `"defer": false` (always advertised), or opt into the automatic budget: set
`"deferAutoTokens"` (top-level in mcp.json) and when the combined schema estimate of all connected
servers exceeds it, the **largest servers defer first** until the advertised remainder fits.
`/mcp` marks a deferred server with `deferred → tool_search`.

**Built-in tools defer too, where it is safe.** On first-party APIs (`api.anthropic.com`,
`api.openai.com`) the rarely-used built-ins — `workflow`, `persona_create`, `checkpoint` /
`checkpoint_view`, the memory and skill write surface (`memory_save`/`update`/`forget`/`ask`/
`profile`, `skill_save`/`refine`/`forget`/`search`/`install`), `team_status`, `notify`, and
`web_crawl` outside research turns — ride behind `tool_search` instead of on every request
(about 14.5 KB of the 42 KB schema block); a conversation that is a pure question also defers
`process`, `file_move` and `task`. The set is decided by the conversation's shape (question ·
small edit · multi-file · research, classified from the prompt in English or Vietnamese) and only
ever widens, so the advertised tool list stays byte-stable. Elsewhere it is off for the reason
below; `"lean_tools": true` in `cli-config.json` turns it on for a gateway you have checked,
`false` turns it off everywhere. `aizen prompt-size` prints both sizes.

**Deferral is opt-in — check your provider first.** It requires an endpoint that lets the model
call a tool whose name is not in the request's `tools` array. First-party APIs (Anthropic, OpenAI)
accept that; some hosted gateways grammar-lock generated call names to the advertised set, and
there a deferred tool can never be called (measured A/B on one such gateway: the same model called
the tool instantly when advertised and could not produce the call at all when deferred). That is
why nothing defers until you set `deferAutoTokens` or pin a server `"defer": true`.

```json
{
  "deferAutoTokens": 4000,
  "mcpServers": {
    "github": { "url": "https://api.githubcopilot.com/mcp/", "auth": "oauth", "defer": true },
    "time":   { "command": "uvx", "args": ["mcp-server-time"], "defer": false }
  }
}
```

MCP schemas are **pinned for one agent run**. If a server emits `notifications/tools/list_changed`,
Aizen defers the new schema until the next fresh user message instead of mutating the tool registry
mid-run. Connection EOF/send/read/timeout poisons that connection: a read-only MCP call may reconnect
and replay once after confirming the schema is unchanged; a state-changing call is never replayed
after an ambiguous transport failure (its side effect may already have happened). HTTP 404 session
expiry and OAuth refresh retain their narrow one-retry behavior.

**OAuth sign-in apps** — the marquee SaaS servers (Linear, Notion, Slack, Gmail/Google, Atlassian)
are OAuth-only. `aizen` speaks the full OAuth 2.1 (PKCE) flow: an entry with `"auth": "oauth"` triggers a
browser sign-in (`aizen apps login <key>`, or automatically when you `aizen apps add` one), caches the token
at `~/.aizen/mcp-tokens/<key>.json` (0600), and refreshes it transparently. No API key to paste — you
authenticate with the real vendor; servers without dynamic client registration take an `"oauth": {
"client_id": "…" }` block.

```json
{ "mcpServers": { "linear": { "url": "https://mcp.linear.app/mcp", "auth": "oauth" } } }
```

**Connect apps without editing JSON** — `aizen apps` is a curated catalog over the official MCP registry:
`aizen apps list` (featured + connected), `aizen apps search <kw>`, `aizen apps add <key|name>` (picks a server,
prompts only for real secrets, signs you in if it's OAuth), `aizen apps info <key>` (config with secrets
masked + a live tool probe), `aizen apps login <key>`, `aizen apps remove <key>`. Also the `/apps` TUI hub.
LOCAL-FIRST: a self-hostable package (runs on your machine with your keys) beats a hosted gateway.

### Browser automation (`--features browser`)
Build with `cargo build --release --features browser` to give the agent five CDP tools that drive an
**existing** Chrome/Edge/Brave (it never bundles a browser). The legacy local setup still works:
```bash
chrome --remote-debugging-port=9222     # or msedge / brave; AIZEN_BROWSER_CDP overrides local
```

For multiple/local-or-remote CDP endpoints, create versioned `~/.aizen/browser.json`:
```json
{
  "schema": 1,
  "default_profile": "local",
  "profiles": {
    "local": { "provider": "cdp", "endpoint": "127.0.0.1:9222" },
    "work":  { "provider": "cdp", "endpoint": "https://cdp.example.com", "auth_env": "WORK_CDP_AUTH" }
  },
  "routes": { "*.corp.example": "work", "localhost": "local" }
}
```
`auth_env` is an **environment-variable name only**; credential values are never accepted in the
file, tool schemas, status output, or logs. When a profile sets `auth_env`, that value is attached as
the `Authorization` header on **both** the HTTP `/json` discovery request **and** the WebSocket upgrade,
so a CDP endpoint behind an auth proxy is reachable. `browser_navigate` resolves the URL host to a
profile and pins that session; snapshot/click/type/eval continue on the same profile. Switching profiles
drops the old websocket and invalidates its `@ref`s. Browser sessions are **keyed per conversation**
(REPL session, or `serve` platform:route:chat), so two chats never share a page, profile, or `@ref`s;
`/new`, session deletion, and hostbot route removal release a conversation's session. `/browser` shows
sanitized routing/session status; `/browser doctor` live-probes every profile without printing
credential values.

Tools: **`browser_navigate`** (open a URL), **`browser_snapshot`** (the page's accessibility tree as
`[@ref] role "name"` lines), **`browser_click`** / **`browser_type`** (act on a `@ref`), and
**`browser_eval`** (run JS — read DOM/state, await fetches). `browser_snapshot` may reconnect/retry
once after a transport failure; navigate/click/type/eval are never replayed after transport ambiguity.
Still a **pure-Rust static binary, no bundled browser/Node/Playwright/CEF**. An absent browser returns
an actionable error, not a crash.

### `aizen memory` — the self-learning brain
```bash
aizen memory add "prefer-pnpm" -t feedback -b "I prefer pnpm over npm"
aizen memory list [current|global|project|<zone>]   # narrow a long listing to one workspace view
aizen memory search "package manager" [--dimension tooling]
aizen memory profile [--json]      # derived preferences rollup (verbosity/tooling/stack/…)
aizen memory ask "which package manager should I use?"   # abstains rather than guessing
aizen memory learn "<a user turn>"  # free extraction → threat-scan → route → store
aizen memory frozen                 # the always-on prompt-prefix core
aizen memory style | review | as-of <date> | supersede <old> <new> | archive | restore <id> | compact
aizen memory where                  # the folders + counts, for editing or clearing out many at once
```

Ids are whole words. A fact named "Người dùng giao tiếp bằng tiếng Việt" files as
`nguoi-dung-giao-tiep-bang-tieng-viet` — accents are folded off each letter, and `-` marks a word
boundary and nothing else. An earlier slugifier tested one codepoint at a time, so every accented
letter became a separator and cut inside words: the same name came out `ng-i-d-ng-giao-ti-p…`. Stores
written by that version are re-slugged once, automatically, on the first run of a build that has this
— the old→new table is left in `cli-memory/.id-migration-<date>.tsv`, and graph edges are re-pointed
in the same pass. Set `AIZEN_NO_ID_MIGRATE=1` to skip it.

A fact's id comes from its display `name`, which is the first 60 characters of the fact — so that cut
has to land on a word too. It used to cut anywhere: 73 entries on one store had names ending in a
one- or two-letter fragment, and the id inherited it (`…-la-khe-uoc-lam-viec-l`, where `l` began
`lâu`). New facts back the cut up to the last word; the names already written are left alone, since
their bodies still hold the full text and a second automatic rewrite of a store belongs behind a
command you type, not a startup pass.

The same rule now governs every name derived from free text — memory ids, `#remember` captures,
persona self-memories, session saves, and the project zone key all share one implementation:

| Surface | Where | Migrated? |
|---|---|---|
| memory entry id | `cli-memory/entries/` | yes — `.id-migration-<date>.tsv` |
| `#remember` id | same | yes, same pass |
| persona self-memory | `personas/<slug>.self/` | yes — `.stem-migration-<persona>-<date>.tsv` |
| session save name | `sessions/` | no — existing files keep working, see below |
| project zone key | `skills/p/<slug>`, index | only if the checkout path is non-ASCII |

**Persona self-memories** get one extra thing: a short content hash on the end
(`ep-hoan-thien-landing-install-tabs-os-85ed`). Every episode body opens with its own type label, so a
stem taken from the first few words described the format rather than the memory — twelve files on one
store all read `ep-correction-user-redirected-me-todo`, separated only by a counter. The stem now skips
the label and carries a hash, so it identifies one memory.

**Session names** are derived, not migrated. New saves fold to ASCII whole words; files already saved
with accents keep their names and stay loadable, listable and deletable. Two guards on derivation:
credential-shaped tokens are dropped before the name exists (a key pasted as the first line of a chat
used to become the filename — and `/sessions` prints filenames), and a name is never cut mid-word.
The credential guard covers name derivation only: it does not redact what is inside a saved transcript,
so a key pasted into a chat is still in that file's message text.

### `aizen bench` — anti-oracle benches
```bash
aizen bench memory [--split gate|tune|all] [--hybrid]   # retrieval recall vs a baseline
aizen bench memory --evolution                          # multi-session reuse gate (≥5%/session lift)
aizen bench profile                                     # golden set for the profile rollup
aizen bench dialectic                                   # golden set incl. abstain-when-unknown
aizen bench loop                                        # loop discipline vs a scripted model (offline)
aizen bench tasks [--task <id>] [--json]                # task suite: real loop + real tools on fixture crates
aizen bench tasks --record [--task <id>]                # record the model's answers once (spends tokens)
aizen bench tasks --update-baseline                     # capture steps/tokens per task as the baseline
```

`bench tasks` runs each `bench-tasks/<id>/` (a prompt plus a dependency-free cargo crate) through
the real agent loop with the verify gate on and every tool rooted in a throwaway copy, then checks
that the loop reached Done, `cargo test` exits 0, only the task's `allowed_files` changed, and
steps/tokens stay within 1.25× of `bench-fixtures/loop-baseline.json`. The model's answers come
from a recorded tape, so the suite runs in CI without a key; a task with no tape is skipped.

### Record and replay model calls (`AIZEN_TAPE`)

Any run can be taped. `AIZEN_TAPE=record` writes one JSON line per model call to
`AIZEN_TAPE_FILE` (default `.aizen/tapes/<stamp>-<pid>.jsonl`); `AIZEN_TAPE=replay` answers from
that file instead of the provider, matching calls by position and warning when what the model was
shown differs from the recording (the workspace root, dates, times, durations and hashes are
normalised first — set `AIZEN_TAPE_ROOT` when the workspace moved); `AIZEN_TAPE=strict` fails the
call on such drift or on a tape that runs out. Tools still execute for real on replay; only the
model is simulated. Record single-threaded flows (`aizen agent`, the task suite) — the REPL's
background chores race the turn and land on the tape in arrival order.

## Exit codes
`0` success · `1` error (bad args, network/HTTP failure, a bench gate FAIL). The agent loop
returns `0` even if it stops on the step limit or divergence — it prints the reason to stderr.

## Safety model
Three layers, bottom to top. (1) A **hard safety floor** — a deterministic blocklist (`rm -rf /`
incl. GNU long flags like `rm --recursive --force /`, `mkfs`, `dd of=/dev/…`, fork bombs,
`curl|sh`, `format C:`, …) that runs *before* the `/yolo` short-circuit, so catastrophic commands
are refused **even under auto-approve** (the same check applies to background `process start` +
`` !`cmd` `` in custom commands). (2) **Approval**: destructive ops are approval-gated (non-TTY
safe-deny; `--yes` pre-authorizes, transitively for sub-agents); a command that requests the
`network` capability is an escalation `smart` never auto-clears. (3) An **OS sandbox** under both:
every model/repo-influenced child runs with Aizen's secrets scrubbed from its environment, a
private per-run temp, deny-by-default network, and a workspace-write filesystem policy —
kernel-enforced on Linux (Landlock + seccomp) and macOS (Seatbelt), software-guarded on Windows
(Job-Object containment + resource ceilings; honestly reported as `partial`). `strict` mode fails
closed instead of degrading; unattended runs (cron, hosted bots) fail closed by default on
platforms without a kernel backend. `aizen sandbox status|doctor|explain` show what *your*
machine enforces; every spawn lands in an owner-only audit log. Full detail: [SANDBOX.md](SANDBOX.md).

File tools resolve paths anywhere on disk by explicit user decision (the old cwd-confinement
guard was removed) — the sandbox above confines *child processes*, not the file tools. The web
tools (`web_fetch`/`web_crawl`/`aizen crawl`) carry an **SSRF floor**: a URL that resolves to a
loopback/private/link-local address (incl. the cloud metadata endpoint `169.254.169.254`) is
refused — set `AIZEN_ALLOW_PRIVATE_NET=1` to allow local/internal targets. Long-lived secret
files (`cli-config.json`, OAuth/MCP token caches, saved sessions) are written owner-only. MCP and
other external tools are destructive-by-default. `shell_run` is wall-clock capped at 120s.
Internal git plumbing runs with repo hooks, fsmonitor and credential helpers disabled, so a
checkout's `.git/hooks` never executes because Aizen made a checkpoint. Tool results and file
contents are treated as data, never as instructions.

## Remote control & notifications
`aizen serve` / `aizen discord serve` run long-lived daemons that drive the agent from Telegram or a
Discord bot (pure-Rust gateways, no SDKs); destructive ops ask for approval from your phone. `aizen
cron` schedules unattended runs (model pinned at create time). Outbound `notify` channels (Discord /
Slack / generic webhook) and the two-way bots are all managed from the **`/apps`** hub.
