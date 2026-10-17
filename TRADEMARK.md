# Trademark policy

The **Aizen** name, the Aizen logo, and the `aizen` command name are trademarks of the Aizen
authors. Apache-2.0 §6 grants you the code, not the brand.

This file exists because a fork is allowed — encouraged, even — while a fork that calls itself
Aizen is not. Both statements are true at once: the licence is maximally permissive about the
software, and silent about the name.

## What a fork may do

- Use, modify, and redistribute every line of this source, commercially or not, under Apache-2.0.
- Say plainly what it is built on: *"a fork of Aizen"*, *"based on Aizen"*, *"compatible with the
  Aizen plugin protocol"* — factual, descriptive references are what trademark law calls fair use.
- Keep the historical copyright headers and the `NOTICE` file, as Apache-2.0 §4 requires.

## What a fork may not do

- Ship under the name **Aizen**, `aizen`, **Aizen Desktop**, or a name confusingly similar to them.
- Keep the Aizen logo, wordmark, splash art, or the `aizen` binary name as the product's identity.
- Present itself as the official Aizen, or as endorsed by the Aizen authors, without written
  permission.

The test is what a user sees when they install and run it: if they would believe they have Aizen,
it is not allowed.

## Renaming a fork — the checklist

A fork that wants its own name must change, at minimum:

| Where | What |
|---|---|
| `Cargo.toml` | `name`, `description`, `repository`, `homepage` |
| `src/main.rs` | the binary name, `--version` output, help text |
| `src/core/config.rs` | the `~/.aizen` home directory and every path derived from it |
| `src/features/update.rs` | `DEFAULT_REPO` and the release asset names |
| `install.ps1`, `install.sh` | `$Repo` / `repo=`, the installed binary name |
| `README.md`, `docs/`, `NOTICE` | product name, links, attribution wording |
| `src/agent/system_prompt*.md` | any self-identification in the model's system prompt |
| `src/ui/splash.rs` | the wordmark and splash text |

A mechanical find-and-replace is **not** a rename: it leaves `aizen` in user-facing strings,
protocol fields, and the plugin API. Do it deliberately, file by file.

## Enforcement

This is a policy statement, not a technical restriction. Nothing in the build prevents a
determined person from stripping the name, and pretending otherwise would be dishonest — a binary
that ships to a user can always be modified by that user.

What this repository does do:

1. States the policy where a person reads it (`README.md`, this file, `NOTICE`).
2. States it where an AI coding agent reads it (`CLAUDE.md`, `AGENTS.md`) — the files an agent
   loads before touching anything, so a rename does not happen by accident during unrelated work.
3. Keeps the name in the source it protects: `Cargo.toml`, the `aizen` binary, the `~/.aizen` home
   path, the system prompt's self-identification. A fork that removes them has made a deliberate,
   visible choice, not an oversight.

For permission to use the name — a distribution, a downstream product, a conference talk — open an
issue and ask. Permission is often given; it is never implied.
