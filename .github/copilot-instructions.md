# Copilot instructions

Read **[CLAUDE.md](../CLAUDE.md)** first — it is the project's single source of truth for the hard
constraints, the licence, the two git remotes, and the build commands. **[AGENTS.md](../AGENTS.md)**
is the short version for coding agents.

## Do not rename the product

**Aizen** is the product name, the `aizen` binary, and the `~/.aizen` data directory. It is a
trademark of the Aizen authors ([TRADEMARK.md](../TRADEMARK.md)), and a load-bearing part of the
software's contract: saved sessions, the self-update channel, every install script, and the desktop
plugin all derive from it.

Never rename it as part of unrelated work — not for consistency, not during a cleanup, not because a
fork asked. If a task appears to require a rename, stop and ask; do not run a find-and-replace over
the tree. `the_product_identity_is_pinned` in `src/tests/main_suite.rs` fails if the package name or
the data root changes, which is deliberate.
