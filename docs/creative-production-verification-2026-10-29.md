# Creative-production foundation — verification log

Date: 2026-10-29. Branch: `feat/creative-production`. Source baseline: `a134039`.
This is an implementation log, not a release/support or application benchmark claim.

## Baseline

- `cargo test --offline --bin aizen` with process-local
  `RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-msvc`: **3,130 passed, 0 failed, 2 ignored**;
  3,132 tests discovered, test runtime 753.04 s. Compiled before foundation edits.
- Existing `target/release/aizen.exe`: reports `aizen 0.7.3`, 48,144,384 bytes,
  SHA-256 `bbd0d67959824963db7df5278d861fa8daeaf9acaec0ec3578a76310b04eeac6`.
  Its source/feature/build provenance is **unverified**; do not treat it as a matched source baseline.
- Existing artifact warm `--version`: one warmup plus 20 samples via Python subprocess,
  isolated temporary `AIZEN_HOME` and working directory. Median **16.006 ms**,
  min **14.439 ms**, max **32.264 ms**; launch/wait overhead included. This is not cold
  startup or normal interactive startup. No disk-cache purge or installed-binary change.

No application/editor/plugin was launched, installed or probed. No model-provider calls were made.
App versions, licenses, device performance and actual accepted-output latency are not established.
Numeric creative comparison gates remain blocked on an authorized matched real-app baseline.

## Candidate verification

- `cargo test --offline --bin aizen apps`: **51 passed, 0 failed**, 3,132 filtered out;
  0.55 s test runtime (compile 2m24s). Covers 29 contract, 15 Windows journal,
  six offline configuration tests and one CLI argument test. Unix symlink fixture is not executed
  on this Windows host; no Unix compatibility claim is made.
- `cargo build --offline --bin aizen`: passed for the local debug CLI; no install performed.
- `python -I scripts/apps_status_smoke.py target/debug/aizen.exe`: passed. Drives the actual
  CLI under fixture-owned HOME/project paths; checks text/JSON, filtering, redaction, explicit unknown
  live state, project override isolation, no fixture file/directory changes, absent HOME remaining
  absent, missing key errors and malformed/oversized config plus incomplete/corrupt/oversized journal.
  Uses impossible command/remote configuration without connecting; no real user configuration read.
- Scoped Rust formatting and `git -c core.whitespace=cr-at-eol diff --check`: passed.

- `cargo clippy --offline --bin aizen --tests -- -D warnings`: passed (58.74 s).
  Dormant contract/job controller APIs have narrowly documented `dead_code` allowances at their
  module declarations; no other Clippy/compiler diagnostics were suppressed.

- Full integrated `cargo test --offline --bin aizen`: **3,181 passed, 0 failed, 2 ignored**;
  3,183 discovered, 752.70 s test runtime. Adds 51 tests over the 3,132-test checkpoint.
  Subsequent CLI-only changes label records as non-live and redact journal parse errors; the actual
  CLI smoke was rerun after rebuilding those changes.
- Extended CLI smoke includes a valid durable unknown job, exact root-free projection and secret
  strings in invalid journal fields. One intermediate smoke fixture incorrectly selected a connector
  after resetting its config to `{}` and failed as expected; corrected the fixture, then reran
  successfully. No production fix was hidden behind that fixture correction.

- Final scoped `rustfmt --check`, added-file whitespace/Python AST/local documentation link checks:
  passed. Added-content credential-pattern scan: zero matches in the inspected additions; this is
  not proof that every possible secret shape is detectable. `Cargo.toml`/`Cargo.lock` unchanged.
  Desktop tracked source unchanged; its private untracked key was not read. Existing core cache
  and the unrelated image-selection draft remain untouched.
- Strict Clippy was rerun after the final offline status refinements: passed.

- MSVC `cargo build --offline --release --bin aizen`: passed (10m08s), followed by
  `cargo build --offline --release --features dense --bin aizen`: passed (9m19s).
  Both are local build artifacts; neither was installed or released.
- Actual CLI status smoke rerun against the resulting **dense release** artifact: passed.
- Candidate dense release: 48,301,056 bytes, SHA-256
  `595ce15a46dd13e6b4804162e74844763adf19134faa8423e5382121c34fc466`.
  Same warm `--version` procedure: median **12.355 ms**, min **11.830 ms**, max **13.202 ms**.
  The existing artifact's build/features are unverified, so neither the timing difference nor
  156,672-byte size difference is attributable to this change; **no speedup/regression claim**.
  A matched-source baseline and actual-app accepted-output benchmark remain required.

- `cargo check --offline --no-default-features --bin aizen`: passed; foundation does not depend
  on enabling browser support.

Independent journal review is in progress.
Journal tests are deterministic filesystem/state-machine fixtures, not authenticated MCP framing,
whole-lifetime workspace admission, external-process cancellation or real editor compatibility.
