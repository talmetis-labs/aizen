# bench-tasks — the task suite behind `aizen bench tasks`

Each directory is one task the real agent loop is driven through, with the real tools, on a
throwaway copy of `repo/`:

```
<id>/task.json      what to ask, which files may change, how to verify
<id>/repo/          a dependency-free cargo crate (so `cargo test` takes seconds)
<id>/tapes/         recorded model answers (`<name>.jsonl`), replayed offline in CI
```

`task.json` fields: `id`, `prompt`, `allowed_files` (paths relative to the repo that the run may
create or change; anything else changed is a wrong-file edit), `expect` (`done` — the loop must
reach Done and the verify command must exit 0; `no-edit` — additionally nothing may change),
`verify` (argv run in the copy after the loop; default `cargo test --quiet`), `max_steps`.

Record a tape from a configured endpoint (spends real tokens):

```
aizen bench tasks --record                 # every task → tapes/default.jsonl
aizen bench tasks --record --task add-feature --tape sonnet
```

The six tasks: `fix-failing-test`, `fix-build-error`, `add-feature`, `refactor-with-tests`,
`multi-file-wire`, and the zero-edit control `no-edit-control`.

Replay is the default when a tape exists; a task without a tape is reported as SKIPPED. Tapes
are matched call-by-call, and a run whose prompt or tool surface drifted from the recording says
so (see `src/llm/replay.rs`). Each fixture manifest carries an empty `[workspace]` table so cargo
never climbs into this repository's own manifest.
