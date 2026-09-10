## Aizen v0.6.7 — the subscription is a sign-in

This release turns an Aizen plan into something you reach the way you reach any modern coding CLI:
you sign in, and it works. No key to find on a dashboard, no string to paste into a config, nothing
copied into shell history. Everything here is client-side — **there are no server changes in this
release** — and it slots in beside the Pantheon sub-agent roster and the Tool Search slimming that
landed earlier in the cycle.

### The short version

```sh
aizen account login     # browser opens, you approve, done — the plan calls models straight away
aizen models            # already listed against your plan; nothing else to set up
```

On a machine that cannot open a browser — SSH, a container, CI — pair it instead:

```sh
aizen login             # approve a short code; comes back with a per-device credential
```

And to leave:

```sh
aizen logout            # drops the session AND the local key here — revokes neither elsewhere
```

### Signing in, and why it isn't a key

The Aizen plan is sold by signing in. `aizen account login` opens your browser to a loopback
redirect on `127.0.0.1`, guarded by a random `state` so a redirect from anywhere else is thrown
away; the code that comes back is traded for a **session token**, which is stored in
`~/.aizen/session.json` with owner-only permissions and kept in its own file, apart from the gateway
key, so neither can ever be handed to the wrong door. Since 2026-09-06 that session token opens
`/v1` directly — which is why, once you are signed in, there is nothing to paste and nothing to
fetch.

- `aizen account login --password` stays for the small number of accounts that actually have a
  password. Most do not — an account created through Google or GitHub has no password hash — so the
  browser flow is the default and the password form is the exception.
- `aizen account whoami` says who is signed in and **never prints the token**.
- `aizen account logout` drops the session and nothing else.
- The token lives about 30 days with no refresh path. A 401 in the middle of a run means *sign in
  again*, not a network blip to retry.

### Buying and managing subscriptions — `aizen sub`

Three kinds of thing an account can buy, one command:

- a **plan**,
- a marketplace **model** or **combo** (two names for one door — both are listings, subscribed and
  cancelled through the same route; the server decides which kind a listing id names),
- a paid **plugin**.

Each has `ls` / `buy` / `add` / `confirm` / `rm`, plus `quote` and `download` where they apply. The
design is built around the ways spending goes wrong:

- **No retry on any call that spends.** A lost answer resent is karma charged twice, so a spend that
  does not come back is reported, never re-fired.
- **The coupon price is the server's quote**, surfaced as-is and never recomputed on the client.
- **A `needs_review` hold is shown, not swallowed**, and `gone` / `cancelled` read plainly instead of
  collapsing into a generic error.
- **A spend confirms first.** In a non-interactive shell it stops hard (exit `2`) unless you pass
  `--yes`, so a script can never buy something by accident.
- **Stable exit codes:** `401/403 → 4`, `429 → 5`, `400/404/409/410/422 → 2`.

### Your own endpoints and your own keys

- `aizen custom` — **bring your own provider** (BYOK): `ls` · `add` · `set` · `rm` for your own
  OpenAI-compatible endpoints, managed against the account instead of hand-edited into the config.
- `aizen key` — **see what the plan key can call, and manage your keys**: `ls` · `show` · `rotate` ·
  `reveal` · `loadout` · `models`. The plan's own key appears for reference but has no string to
  copy: the plan is bought by signing in, and no route hands out a string for it.

Both need a session first (`aizen account login`).

### Device pairing and leaving cleanly

- `aizen login` pairs this machine by approving a short code and returns a **per-device** credential
  — the right path for a headless box. `aizen gateway login|logout|status|env` are the narrow verbs
  for the key alone.
- `aizen logout` leaves Aizen entirely — the account session **and** the local gateway key — and
  says plainly that it revokes neither: the token stays valid on your other machines until it
  expires, and the device row stays live until you unpin it in the dashboard.

### In the REPL

- `/login` renews a session that just 401'd without dropping the conversation you are in.
- `/logout` runs the same teardown as `aizen logout`, in place.

### The CLI and the desktop app now agree

Both read the one `~/.aizen/session.json`, so signing in on either side signs in on both:

- A CLI whose **first launch** finds a desktop sign-in skips the setup wizard, and the splash and
  status bar name the session endpoint and "signed in as …" instead of "not set".
- The **turn after** the desktop signs out points you at `/login`, not at the setup screen.
- The **desktop window** watches the shared files and updates its account card live when
  `aizen account login` or `aizen logout` runs in a terminal beside it.

### Also fixed

- **Sandboxed `git` can reach `/dev/null` again on Linux.** The Landlock filesystem allow-list
  granted only the workspace roots, so a sandboxed child opening a standard pseudo-device hit
  `Permission denied` — `git status` opens `/dev/null` read-write and broke `git_inspect` under the
  sandbox on Landlock kernels (Windows/macOS were fine). The Linux ruleset now also grants
  `/dev/{null,zero,full,random,urandom,tty}`.

### Compatibility

- Existing config-file setups (`base URL` + `API key`, or the `AIZEN_*` environment variables) are
  untouched and still take precedence — the session only fills a gap, it never quietly redirects
  traffic you configured yourself.
- No server changes: nothing about this release requires a gateway or web-host update.
- The pre-2026-09-06 shared plan key still works where it is already pinned; new machines get a
  per-device credential instead.
