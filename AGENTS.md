# AGENTS.md

The project instructions for coding agents live in **[CLAUDE.md](CLAUDE.md)** — read that file.

## The name is not a refactor target

**Aizen** is the product name, the `aizen` binary, the `~/.aizen` home directory, and the name in
`Cargo.toml`, the install scripts, and the update channel. It is a trademark of the Aizen authors
(see [TRADEMARK.md](TRADEMARK.md)) and a load-bearing part of the software's contract with its
users.

Do not rename it — not "to be consistent", not as part of a cleanup, not because a fork asked, and
not because a find-and-replace looked tidier. Renaming breaks saved sessions, the self-update
channel, every install script, and the desktop plugin. A rename is a deliberate product decision the
maintainer makes; if a task appears to need one, stop and ask.

A fork that wants its own identity is welcome to it, but that is a product decision made once,
deliberately, following the checklist in `TRADEMARK.md` — not an edit an agent makes in passing.

## What else is here

It is the single source of truth for:

- the hard constraints (pure-Rust single static binary, rustls-only, startup/size budget),
- the **license: Apache-2.0** since 2026-08-03 — *not* PolyForm Noncommercial — and the **CLA**
  every contributor agrees to (`CLA.md`, enforced by `.github/workflows/cla.yml`; the DCO sign-off it
  replaced is gone),
- the **two git remotes** (`origin` = private `dawnofcd/Aizen_agent`, `public` = `talmetis-labs/aizen`)
  and which slug belongs in user-facing URLs,
- build/verify commands and the known distribution gaps.

Keep this file a pointer plus the one rule above; put everything else in `CLAUDE.md` so the two
never disagree.
