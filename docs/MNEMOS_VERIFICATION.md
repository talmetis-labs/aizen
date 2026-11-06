# Mnemos verification — 2026-10-31

Status: extraction in progress, not a release or acceptance sign-off. This record separates actual
application runs from focused tests and unverified surfaces. Core remains `aizen`; Mnemos is the
independently built sibling `aizen-mnemos`, not a Core dependency or a published package.

## Legacy transfer repair — 2026-11-03

This is fixture-only repair evidence, not extraction acceptance or an installed-binary sign-off.
The repair adds negotiated v2 bounded backups and exact archive-attachment review while retaining
v1 serialization/journal semantics. Desktop now separates **Check**, **Back up and transfer**, and
**Use this store**. Transfer alone never changes Storage, Learning or the selected provider.

### Completed application evidence

`python -I scripts/memory_adoption_smoke.py --core CORE.exe --engine MNEMOS.exe --large-backup`
completed **39 commands** using actual Core/Mnemos executables. Every command exited zero and its
owned Job settled; cleanup failures are empty. The harness uses disposable HOME/USERPROFILE/
AIZEN_HOME/workspace roots, generated records, a 329,474,395-byte derived cache, three zero-byte
cache locks and two synthetic archive attachments. It snapshots and hash-checks both binaries
before invocation. No real Memory contents, installed executable or live model endpoint was used.

- Report: `%TEMP%/aizen-memory-adoption-tj_r0pxl/report.json`; captured client wire is in `wire/`.
- Core SHA256: `4f18bc29b0aab5c437d66ece20640a13bd8c575bf23d8bdefef081d1f8702a5c`.
- Mnemos SHA256: `bc8435bbf4ef5bd2f8ad200bc2ed2aced7d13b71645821b4d5963de736f25605`.
- This run predates the final Core inventory-parent validation, legacy-history regression and argv
  helper tests. Its hashes must not be presented as exact-current-source verification.
- A separate small-fixture run also completed 39 commands:
  `%TEMP%/aizen-memory-adoption-bltechbx/report.json`.

The large run checks explicit attachment review and tamper refusal, digest/consent/stopped-writer
refusals, full backup hashes, imported exclusions, source/current-store/configuration preservation,
durable host status, recovery with the original source parked, an independently reviewed Storage
switch and switch back, custom destination selection, excluded-cache drift refusal and full-source
restore into another fresh root. Host-registration loss is injected only in a disposable record;
this is not a host crash/power-loss test. No model-assisted learning is exercised.

Measured command wall times from that large report, not aggregate test durations:

| Operation | Seconds |
| --- | ---: |
| Attachment-review check | 7.062 |
| Reviewed default preview | 13.141 |
| Default transfer | 83.359 |
| Recover with source unavailable | 7.328 |
| Custom preview | 13.703 |
| Source-drift refusal | 13.781 |
| Fresh custom transfer | 83.437 |
| Direct Mnemos full-source restore | 97.406 |

The Core-owned transfers remain below the unchanged 120-second execution deadline on this machine.
The 97.406-second restore is a direct engine invocation, not a Desktop/Core restore action. These
numbers are not a throughput guarantee for slower disks, larger stores or the 4 GiB policy limit.
Total command wall time was 364.0 seconds. Policy bounds do not promise every allowed source can
finish within the execution deadline.

### Final Core rebuild and strict application rerun

The final Core parent-inventory/argv changes passed scoped rustfmt and **133 plugin tests**
(0 failures), followed by a successful debug build. The v1-only argv regression covers preview,
apply, resume and inspect/recover, ensuring neither v2 flag is sent. Both offline capabilities
are rejected from ordinary runtime contexts. Legacy no-plan records render as check-failed without
rewriting their saved bytes; settlement-unknown retains priority. Four existing memory-api
unused-code warnings remain.

The rebuilt Core then passed another complete **39-command large-fixture run**, now explicitly
asserting `check_failed / attachment_authorization` for modified attachment authority and
`interrupted / source_changed` for source drift, rather than accepting any non-success state.

- Report: `%TEMP%/aizen-memory-adoption-u06i21c1/report.json`.
- Core SHA256: `1a4232bcb6034f919e5b583746769d03b73df2d466dbcf78966bb603e81b45e2`.
- Mnemos SHA256: `bc8435bbf4ef5bd2f8ad200bc2ed2aced7d13b71645821b4d5963de736f25605`.
- All 39 commands exited zero, all owned Jobs settled and cleanup failures are empty.
- Reviewed preview: **13.234 s**; default/custom apply: **81.406 / 80.453 s**; recovery with
  source parked: **6.922 s**; drift refusal: **13.109 s**; direct full-source restore: **91.125 s**.
  Total command wall time: **350.545 s**. The same runtime/restore limitations stated above apply.
- Complete source/backup/restore hashes, unchanged current store/configuration/Learning and the
  separate Storage review/switch-back assertions passed again.
- Desktop's native captured-wire validator then accepted all **37 request/response pairs** from
  this final run; the subsequent Core sandbox rerun passed **50 tests**, zero failures.

### Initial focused checks and Desktop evidence

- Mnemos adoption contracts: **25 passed, 0 failed, 1 ignored**. Explicitly running the ignored
  329,474,395-byte contract separately: **1 passed**, 258.01 seconds, covering interrupted backup,
  resume, source-independent inspection, restore and backup-tamper refusal. The aggregate duration
  is not one host command's elapsed time.
- Mnemos affinity/native-session/prepared-learning/RPC regressions: **3 / 5 / 7 / 18 passed**.
  Packaging: **18 tests run, 1 skipped**, successful. No whole-Mnemos rerun is implied.
- Core sandbox regressions: **50 passed**, including the final rerun. The intermediate plugin run
  had **132 passed**; the final **133-test** result above covers the argv-helper/parent-set changes.
- Desktop Memory client/graph Node tests: **44 passed**. Native Memory provider tests:
  **13 passed, 1 ignored**. The opt-in captured-wire test was then run separately against each
  actual small and large application corpus: **37 request/response pairs validated per corpus**.
  Wire validation does not itself launch Core or Mnemos.
- Actual Svelte components with synthetic IPC in Chromium: **93 fixture checks passed**. Inspected
  light and narrow-dark screenshots are below `%TEMP%/aizen-memory-ui-OSH4yi/`. This is not native
  Tauri GUI/entitlement E2E; the browser log includes a resource 404, not a clean-console assertion.
- Svelte check: **0 errors, 2 existing AgentMap warnings**. Production `npm run build` passed
  (Vite plus obfuscation), with the AgentMap accessibility and large-chunk warnings retained.
- All literal wizard translation keys are present in English, Japanese and Chinese. The full-app
  i18n gate still **fails with 231 missing strings**; this repair does not establish global coverage.
- Canonical `npm run size` still **fails: 1299.07 / 1250.00 kB gzip, 49.07 kB over budget**.
  That gate rebuilds the non-obfuscated bundle. A separately measured obfuscated `dist/` was larger
  and is not the canonical gate result. No matched before/after baseline isolates this repair's
  contribution. The cap was not raised.
- Core/Desktop tracked `git diff --check` passed, retaining line-ending warnings. New/untracked
  source is not represented by ordinary tracked diff statistics. The Mnemos sibling was not a Git
  repository when inspected, so no Mnemos Git diff validation is claimed.

### Completion follow-up: closed frontend gates and full regressions

The initial failures above are retained as history; they are no longer the current frontend gate
results. No size budget or validation threshold was loosened.

- Global i18n now passes: **4,972 keys per language** (English, Japanese, Chinese), with **200 new
  keys per language** resolving the 231 missing source occurrences. The **744 stale-key warnings**
  remain nonfatal; translations were not deleted to reduce size.
- The Vite build shares catalogue keys while preserving each complete language dictionary. Canonical
  `npm run size` now passes at **1129.30 / 1250.00 KiB gzip**, **120.70 KiB headroom**, including
  all emitted JS/CSS/HTML under the existing gate. This is the pre-obfuscation gate, not the size of
  the obfuscated distribution. No chunk exemption, new dependency or lazy-loading change was added.
- The combined Node catalogue/Memory-client/graph run passed **49 tests**. After strengthening the
  catalogue tests, all **5** passed again, checking exact dictionaries and every translation lookup
  and placeholder substitution through actual production minification and existing obfuscation.
- Svelte/TypeScript checked **437 files: 0 errors, 2 existing AgentMap warnings**. Production
  `npm run build` passed, including obfuscation. The browser fixture passed **93 checks** again;
  inspected light/narrow-dark screenshots are in `%TEMP%/aizen-memory-ui-xR5MDy/`. The resource 404
  remains; this is not a clean-console or native-GUI assertion. Native Memory-provider tests passed
  **13**, with the opt-in captured-wire test intentionally ignored in that default run.
- The full Core `cargo test --offline --bin aizen -- --test-threads=1` run passed **3,139 tests**,
  **0 failures, 2 ignored**, in **858.12 seconds**. This supersedes the earlier absence of a current
  whole-Core regression run. Core Clippy and dense `cargo check` passed; Core Clippy still emits
  six warnings (four unused memory-api items, one adjacent-if style warning and a large-enum warning).
- Mnemos strict `cargo clippy --offline --all-targets -- -D warnings` passed after two local lint
  fixes (adjacent-if formatting and filtering before cloning). The subsequent all-targets suite
  passed **524 tests, 0 failures, 1 ignored**: 466 library, 25 adoption, 3 affinity, 5 native-session,
  7 prepared-learning and 18 RPC. The ignored large adoption contract was explicitly exercised
  earlier as recorded above; it is not counted as run by this final default suite. Packaging tests
  passed again: **18 run, 1 skipped**.

After those Mnemos fixes, the engine was rebuilt and the actual large-fixture application run passed
again: **39 commands**, every exit zero and owned Job settled, no cleanup failures. Report:
`%TEMP%/aizen-memory-adoption-o1zv2pwi/report.json` (wire corpus in `wire/`).

- Core SHA256: `1a4232bcb6034f919e5b583746769d03b73df2d466dbcf78966bb603e81b45e2`.
- Mnemos SHA256: `67516ba167b9c95f7d925f1c3196d3fd90d27650c06f641ba7d3e36a9e654772`.
- Reviewed preview **13.469 s**; default/custom apply **83.297 / 83.813 s**; recovery with original
  source parked **7.188 s**; excluded-cache drift refusal **13.485 s**; direct full-source restore
  **97.657 s**. Total command time **365.783 s**. The unchanged deadline/throughput limitations above
  still apply; direct restore is not a Core/Desktop restore feature.
- All complete source/backup/restore hash assertions, exact authorization diagnostics, imported
  exclusions, existing-store/configuration/Learning preservation and separate Storage switch/back
  assertions passed. No actual user Memory was read or transferred.
- Desktop's opt-in native captured-wire test subsequently passed against this latest corpus,
  validating all **37 actual request/response pairs**, without launching Core or the provider.

The Core dense release build also passed:
`cargo +stable-x86_64-pc-windows-msvc build --offline --release --features dense --bin aizen`
(**9m 22s**, four existing memory-api unused-code warnings). Local artifact: `target/release/aizen.exe`,
**50,822,656 bytes / 48.47 MiB**, SHA256
`b032643fa9070ad445fac2a0ac8457029d41aba4524199ad78246106344e571c`.
This is a dense MSVC build, not a matched comparison with the older 34.1 MB baseline; no size-regression
or startup conclusion is established. The application smoke above uses the separately pinned debug
binaries, not this release artifact. Nothing was installed or published.

Source fingerprints were unchanged before/after these final tests. The SHA256 procedure sorts
repository-relative paths, hashes each UTF-8 path plus NUL, file bytes plus NUL, and includes existing
`Cargo.toml`, `Cargo.lock`, `build.rs`, and `src/**/*.rs` / `src/**/*.md` only:

- Core: **377 files**, `99a75ea090570ab5a8fbfdd439a81992f298da8c468976d00cc414b34bed3975`.
- Mnemos: **73 files**, `0ba1ab2b2f5f15f61af2e8cb368eaf085966a1f1fcadffba28ac7715140215ec`.

These fingerprints do not cover integration tests, scripts, Desktop or this document. Tracked
Core/Desktop `git diff --check` passed again with line-ending warnings; this does not cover all
untracked source. Native Tauri GUI E2E, Linux/macOS runtime, a new startup benchmark, real-data transfer,
installation, commit, push, release publication and upload remain outside this repair verification.

## Desktop Memory client follow-up — 2026-11-02

This is source/client integration evidence, not a Desktop release or installed-binary sign-off.
The provider browser and Desktop-owned Mnemos companion use the versioned bridge described in
[Memory plugins](MEMORY_PLUGINS.md#desktop-client-bridge). No real home, installed executable,
provider data, automatic learning or network model endpoint was used by this follow-up.

Actual `scripts/memory_client_smoke.py` run: **31 commands passed**, all owned Jobs settled and zero
cleanup failures. The second run uses exactly `aizen memory client`, without an extra sandbox flag,
matching Desktop's client invocation. Explicit guarded consent was saved only in the disposable home.

- Evidence: `%TEMP%/aizen-memory-client-sphr60a6/report.json` plus bounded stdin/stdout/stderr files.
- Tested Core SHA256: `9f86a3a99695b536abafd2cfa785bec0a35552bd2d669753d1ee174b058468f3`.
- Tested Mnemos SHA256: `6e8e41934f8aa6a96d014edf655aefa79f024c89eff29683bd5c4412f40344ca`.
- Offline absent/installed snapshots, action review/cancel, activation and storage changes did not
  launch a provider. Initial snapshot left the isolated home empty; offline changes did not initialize
  default/custom provider data. Actual list/search/get retrieved a seeded Mnemos project record.
- Wrong scope, foreign project, mismatched workspace, stale generation and tampered action-review
  requests were rejected. Disable prevented provider launch. Default/custom storage changes preserved
  old data; uninstall preserved the custom canary. Original executables' hashes stayed unchanged.
- No live-model Learning or foreign-device record fixture was run. Scope narrowing has unit coverage,
  but the foreign-project run is not evidence for cross-device visibility.

Focused Core verification: plugin tests **112/112**, management-argument tests **4/4**, configuration
persistence tests **5/5**. After the final config-error sanitizer, client tests **12/12** and the debug
build passed; the debug artifact hash matches the actual 31-command run above. Plain, no-default and
dense `cargo check` all passed; default Clippy passed with four existing memory-api dead-code warnings.
New client files passed scoped rustfmt. This is not a rerun of the whole-Core regression suite.

The new stdin/owned-process support suite passes **7/7**, including three consecutive final reruns.
The initial run exposed a teardown race: Job accounting reached zero before the root process was
signaled, leaving its current-directory handle briefly alive. Cleanup now also waits for the root
handle. That failed attempt remains a failure, not a passing run. The first packaging attempt also
correctly rejected Cargo's hardlinked debug executable; the harness now copies and hash-checks it
before invoking the unchanged package verifier.

Desktop browser-fixture evidence is **14 checks passed** with actual Svelte components and synthetic
IPC; screenshots were inspected. This is not native Tauri window/entitlement integration. Desktop
record reads currently fail closed on non-Windows because process groups cannot own Core's separate
provider sessions; offline settings remain available. No Core release-size/startup measurement or
full Core test-suite rerun is implied by these focused client results.

## Environment and isolation

- Windows 11 x86_64, MSVC; process-local `stable-x86_64-pc-windows-msvc`, offline Cargo.
- Each application harness creates a fresh temporary HOME/USERPROFILE/AIZEN_HOME and workspace,
  allows only explicit environment values, and uses fake fixture credentials with a loopback
  OpenAI-compatible scripted model. No external model, download, real-store adoption or maintainer
  installation is part of these results.
- Each command starts suspended, enters a private kill-on-close Windows Job, and is then resumed.
  Reports below record `job_settled:true` for every case and no cleanup failures. No global PID/name
  scans or foreign-process termination are used.
- Windows guarded execution is explicitly authorized for these fixture packages. It does **not**
  kernel-confine filesystem/network access; environment scrubbing is not that confinement.
- Binary hashes pin tested bytes, not publisher trust. Build-source fingerprints are recorded
  separately. Temporary report/log locations are local evidence pointers, not shipped assets.

## Earlier matched application artifacts

All three passing application runs in the next section used these same unchanged debug binaries.
They precede the manual-command/help corrections and the later uninstall recovery change:

| Artifact | Location relative to its repository | SHA-256 before and after |
|---|---|---|
| Core | `.audit-out/mnemos-sandbox-stage/target/debug/aizen.exe` | `47733668bc35e488cef3a15236c8db7c1ae29aaa5ff5f13295dccda4150cef48` |
| Mnemos | `.audit-out/target/debug/aizen-mnemos.exe` | `53e624d8f24162a142505f2ae065a8150116ceea405ba1a63a02e3bb5272ce7b` |

`mnemos.exe` / `example-memory.exe` inside a disposable package are byte copies of the supplied
Mnemos artifact, not different engine implementations. Core is not rebuilt by any application
harness. Installation, eligibility and selection are distinct fixture operations.

## Actual application results

Run each from the Core checkout with Python isolated mode and explicit prebuilt paths:

```text
python -I scripts/mnemos_learning_smoke.py CORE.exe MNEMOS.exe
python -I scripts/mnemos_agent_smoke.py CORE.exe MNEMOS.exe
python -I scripts/memory_plugin_lifecycle_smoke.py CORE.exe MNEMOS.exe --provider-id mnemos --exercise-provider
```

Reports are below `%LOCALAPPDATA%/Temp/` in this local verification session:

| Harness | Result | Report |
|---|---|---|
| Offline completed-turn learning | PASS: 19 command cases, 19 loopback completion requests, 3 durable `done` receipts | `aizen-mnemos-learning-c9qvqr__/report.json` |
| Agent tools/context/recall | PASS: 11 command cases, 6 loopback completion requests | `aizen-mnemos-agent-jzlzyin8/report.json` |
| Executing Mnemos lifecycle | PASS: 39 cases, including expected refusals; `provider_executed:true` | `aizen-memory-lifecycle-_pe0u8d5/report.json` |

The learning run checks:

- No-provider Agent remains usable and creates no Memory store/outbox.
- Executable consent without separate learning consent does not deliver a learning event.
- Actual `clarify` yields `awaiting_input` without creating an outbox.
- `--max-iters 1` legitimately extends: ten distinct successful read-only file steps plus a
  separate tool-free synthesis request end at `max_iters` (`num_turns:10`, `is_error:true`). The
  outbox is still absent. Fixture request counts are bounded; no production budget was weakened.
- Successful Agent turns await offline delivery. Repeated identical real user text has distinct
  UUID-v4 turn identities and operation IDs. Python independently checks canonical payload digests
  and exact result bindings. Three completed occurrences produce three `done` receipts.
- Assistant/tool transcript is delivered as bounded context but is not mined as offline user fact
  authority. A forged assistant canary does not enter the fact store; passive user text is skipped.
- Disabling auto-learning preserves activation and stops new receipts. Disabling Memory removes its
  tools/context, starts no provider, and preserves provider data/receipts.
- Offline status exposes three done receipts, 61 available slots and reserved bytes, not transcript
  content. A deliberately damaged fixture receipt produces null/invalid inventory, without running
  a provider, rewriting evidence or conflating it with physical recovery status.

The Agent run uses actual model-requested `memory_save` and `memory_search`, positive standing User
context, distinct long-tail Project recall, and unrelated-query gate rejection. Standing facts are
not redundantly recalled. Disabled runs have no Memory context/tools/provider work and preserve data.
It does not grant completed-turn learning or prove live lane-owned candidate adoption.

The lifecycle run checks offline status/listing, ZIP install without execution/selection, overwrite
refusal, eligibility, explicit selection, CRUD/revisions, native edit versus RPC advisory CAS,
immutable archive handles across live-ID recreation, native restore under the actual handle,
disable/reenable/ineligibility, and uninstall with data preserved. Without **both** lifecycle flags
above, an earlier 20-case run did not execute a provider and is not this CRUD/archive evidence.

Harness SHA-256 values for these runs:

| File | SHA-256 |
|---|---|
| `scripts/mnemos_learning_smoke.py` | `2568f68c9a3f455d1d5d0bb71a1b548ff6af9f8108d96fe3fbebc7f743c937b7` |
| `scripts/mnemos_agent_smoke.py` | `d851e68961374ea9e40309e2676451c638531210e89b1243cf69a798e6567b1b` |
| `scripts/memory_plugin_lifecycle_smoke.py` | `7cb4bca5e063e584a34730dd2297c4a210b5c59bf8a0adf239189dc45a17298c` |
| Shared `scripts/memory_agent_smoke.py` | `83e2fb21a9462c1009bfac7a9b5e35e91b3eb7c3ce35406d97788a1c089d9db9` |

### Failed attempts retained as failures

- The earlier `aizen-mnemos-learning-rlqv6z8c/report.json` is **FAIL**. Its one-request fixture rejected
  a legitimate budget continuation, returned HTTP 400, and never reached `MaxIters`. Its clarify
  case did succeed, but that did not make the whole run pass. The corrected bounded schedule above
  is a separate fresh run; the failed report was not rewritten.
- An earlier combined launch used nonexistent `mnemos.exe` in the Cargo target directory and failed
  before starting the app. The real Cargo artifact is `aizen-mnemos.exe`.
- The expanded lifecycle attempt `aizen-memory-lifecycle-dfyv708f/report.json` is **FAIL** after 18
  commands. The fresh provider correctly returned `{"text":null}` for absent STYLE; the harness
  incorrectly required a string. All 18 jobs settled, no cleanup failed, and both binary hashes
  stayed unchanged. Its report is retained. The corrected assertion checks exact null for this
  fresh fixture, rather than changing the engine to satisfy the test.

## Manual command/help batch — actual application reruns

This later Core build adds shared typed local/hosted `/memory` argument mapping, hosted activation
refusal, and help that distinguishes provider capabilities from the removed built-in engine. It
predates the subsequent missing/corrupt uninstall correction; do not use its hash for later builds.

- Core SHA-256: `4ee86e1a71ac273f2e07f6e87e3ed206e866ba5088665f67622052e1439051cc`.
- Mnemos SHA-256: `53e624d8f24162a142505f2ae065a8150116ceea405ba1a63a02e3bb5272ce7b`.
- Both hashes are unchanged before/after each of these three reports.

| Harness | Result | Report below `%LOCALAPPDATA%/Temp/` |
|---|---|---|
| Agent tools/context/recall | PASS: 11 commands, 6 loopback requests; no cleanup/model errors | `aizen-mnemos-agent-4p902ksy/report.json` |
| Offline completed-turn learning | PASS: 19 commands, 19 loopback requests, 3 done receipts; no cleanup/model errors | `aizen-mnemos-learning-senimzpj/report.json` |
| Expanded executing lifecycle | PASS: 49 commands, all private jobs settled; data preserved | `aizen-memory-lifecycle-gs9vnl9j/report.json` |

The 49-case lifecycle adds three offline help checks, actual `style`/`frozen`/`profile`/`ask`/`as-of`
command extensions, and typed refusals for unsupported review/rebuild without data changes or a
leftover transaction marker. STYLE is absent/null in this fresh fixture; frozen text is a string.
The lifecycle script SHA-256 for this passing run is
`1115ab52b32977a9d6b6a18f30c2adfd9e3e2ba24f192e0a98ad7c31ae230ee8`.
Agent, learning and shared-support script hashes match the earlier table. CLI forwarding is not
actual REPL/manual-slash or live-hostbot coverage. Later script extensions need their own run.

## Uninstall recovery batch — historical matched application reruns

The missing/corrupt-package change was then rebuilt and exercised with the same independent Mnemos.
These artifacts precede the cross-target and canonical-turn/lint batches recorded below:

- Core SHA-256: `bb27ff20c29766faaf5e771b831771ad1a5f56a7204361c8c7e66901281040bd`.
- Mnemos SHA-256: `53e624d8f24162a142505f2ae065a8150116ceea405ba1a63a02e3bb5272ce7b`.

| Harness | Result | Report below `%LOCALAPPDATA%/Temp/` |
|---|---|---|
| Executing lifecycle/recovery | PASS: 59 commands | `aizen-memory-lifecycle-40yccohk/report.json` |
| Agent tools/context/recall | PASS: 11 commands, 6 loopback requests | `aizen-mnemos-agent-fleivchp/report.json` |
| Offline completed-turn learning | PASS: 19 commands, 19 loopback requests, 3 done receipts | `aizen-mnemos-learning-20xrdumz/report.json` |

All command jobs settled; input binaries stayed unchanged; cleanup/model error collections are empty.
The 59-case lifecycle preserves the earlier 49 checks and adds ten recovery administration commands.
It reinstalls into the disposable HOME, deliberately corrupts its inspected manifest, and adds an
unknown sentinel. Uninstall fails but disables the package, preserves pins/selection and changes
neither payload nor data. Reenable is refused. The fixture explicitly moves the inspected tree to
its own quarantine; a new uninstall then retires the now-missing package registration without
changing data/quarantine. Every recovery command has exactly one job process: no provider/helper
execution. This does not authorize automatic quarantine/removal of an actual damaged installation.

Lifecycle script SHA-256:
`5f44f721d6c9a2fa1dcb579a1d311a28c2a1f27323ecef2239699bf545b4c86f`.
Other harness/support hashes match the earlier table. These runs remain Windows guarded, CLI/Agent,
fixture-only mechanics; no retained REPL, live-bot or semantic-model-quality claim follows.

## Actual runtime fault matrix

Report: `%LOCALAPPDATA%/Temp/aizen-memory-runtime-faults-ljg1bcph/report.json`.
This uses the uninstall-recovery Core debug artifact above (`bb27ff20...`), not the Mnemos engine.
The independent deliberately faulty executable has unchanged SHA-256
`8545819e9b95f161a1b5e50f8909e5fb9303e22d708e7d945e7c2e358354428b`.

All 11 modes passed across 97 actual commands. Every command Job settled; all cleanup-failure
collections are empty; both input binary hashes stayed unchanged.

- `conflict` and `pipe-child`: exactly one initialize/store and a clean shutdown, no recorded effect,
  no remaining physical marker, and subsequent disable succeeds. The latter also reaches an owned
  descendant holding inherited pipes. The caller still receives the original semantic conflict.
- `ambiguous`, `recovery`, `crash`, `mismatch`, `noise`, `truncated`, `oversized`, `stderr-flood` and
  `stall`: exactly one store/effect and a retained recovery marker bound to the original provider,
  operation ID, method and parameter digest, without raw payload. Replay, disable and uninstall are
  refused; provider data, marker and package remain unchanged. Semantic uncertain errors still
  complete shutdown; that alone does not resolve their effects.

Harness SHA-256: `7cf7137a89aada9d9f853d13b1f28181666d62ff11a6baf248ac3e1df8e3b8c0`;
shared support SHA-256 matches the earlier table. This is Windows guarded CLI fault/settlement
coverage, not actor panic, queue cancellation, host crash, recovery resolution, power-loss, Mnemos
parity or non-Windows runtime evidence.

## Focused Core build/test evidence

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-2zfn0e12/report.json`.
Source fingerprint before = after:
`761660fcc6c65a17ef53a5f95ed8f0b2c8178bb33ef7e9ec1535cfaabb1db7f1`.
The verifier fingerprints Cargo manifests/lock, build script, and Rust/Markdown under `src/`, not
repository docs or Python harnesses.

| Command | Result |
|---|---|
| `cargo check --offline --bin aizen` | PASS |
| `cargo test --offline --bin aizen plugin:: -- --test-threads=1` | 45 passed, 0 failed |
| `cargo test --offline --bin aizen hostbot::daemon::tests::memory_commands_ -- --test-threads=1` | 2 passed, 0 failed |
| `cargo build --offline --bin aizen` | PASS; artifact used above |

Plugin tests cover the central Done-only gate before config/outbox access, bounded lane-owned
candidate adoption, escaping and scaffold removal, preparation/reset invalidation, and outbox
state/capacity/status. Hostbot regressions call the real command handler using an offline platform:
successful new/reset/cwd/model operations retire the current chat's snapshot; invalid input and
settings-save failure preserve it; sibling chat state remains intact. They start no bot transport.
They do not establish route-wide invalidation across chats when shared route settings change.
Existing compiler warnings remain; this is not a strict Core Clippy result.

Later focused batches supersede those counts for their own source revisions:

| Batch/report below `%LOCALAPPDATA%/Temp/` | Source fingerprint before = after | Result |
|---|---|---|
| Manual commands/help: `aizen-memory-core-verify-kh9yudeh/report.json` | `ddfd59ee9b4870dfd7c7365397fe76bb3e33aaf5037dc9c8a8fb68a21cfc5a99` | check/build PASS; 49 plugin tests and 2 hostbot handler tests passed |
| Missing/corrupt uninstall: `aizen-memory-core-verify-5qy5g2f9/report.json` | `a77013124bbb716fe3f4f3a96914366001a9dd955f2ba8e11897009bdd6cf574` | check/build PASS; 54 plugin tests and 2 hostbot handler tests passed |

The manual regressions check typed command arguments, exact record IDs/scope/replacement text,
malformed known-command refusal and hosted host-global activation refusal before config access.
The five uninstall regressions check missing payload registration retirement with data preserved,
corrupt payload disable/pin retention/retry after explicit fixture repair, nondirectory refusal,
physical-recovery marker precedence, and read-only confirmation of registered IDs. No automatic
recursive cleanup or corrupt-package trust was added. A damaged remaining tree is refused and left
untouched; it is not reported as successfully removed. Focused tests are not the full Core suite.

## Core feature/release matrix — before the cross-target uninstall fix

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-wzioi81h/report.json`.
Source fingerprint before = after:
`a77013124bbb716fe3f4f3a96914366001a9dd955f2ba8e11897009bdd6cf574`.
This matches the missing/corrupt-uninstall batch, not the subsequent cross-target removal edit.

| Command | Result |
|---|---|
| `cargo check --offline --no-default-features --bin aizen` | PASS |
| `cargo check --offline --features dense --bin aizen` | PASS |
| `cargo build --offline --release --bin aizen` | PASS; 46,351,360 bytes; SHA-256 `dcb9c8e5e617b93657345c96e1f4a74d3806d72577f741b6a6d9cfc32baf326c` |
| `cargo build --offline --release --features dense --bin aizen` | PASS; 48,360,448 bytes; SHA-256 `94d2a7486dfba5826641062c64e2744b7ebc12884251b518ef700d2e48e86cc6` |

The release files were copied into the report directory before the next build could replace them.
Against the recorded `e3bd8c9` checkpoint artifacts from `aizen-mnemos-baseline-tf6ub4dw`, plain release
is 512 bytes smaller and dense release is 59,392 bytes larger. This is not a meaningful binary-size
win. These are MSVC artifact comparisons, not the historical 34.1 MB measurement from another build.
Strict Core Clippy and a green whole-Core suite are not established by this matrix. Later Rust edits
need new matched-source verification.

## Paired warm startup — debug artifacts only

Report: `%LOCALAPPDATA%/Temp/aizen-mnemos-startup-vna7xo9b/report.json`.
The harness alternated artifact order, discarded one warmup per artifact, and retained 20 samples
per executable. It measured the complete `--version` subprocess through exit, including Python and
Windows process-creation overhead; CPU/antivirus/load were not isolated. It does not measure cold
start, internal `main()`, no-provider Agent readiness, or enabled-provider/retrieval latency.

| Artifact | SHA-256 | Bytes | Median / min / max, ms |
|---|---|---|---|
| Checkpoint debug (`e3bd8c9`) | `2d745011be4936e79d9d88acdd74129197c56d4080824637af0034006d54442c` | 69,352,448 | 16.20315 / 15.0904 / 22.4784 |
| Uninstall-recovery debug | `bb27ff20c29766faaf5e771b831771ad1a5f56a7204361c8c7e66901281040bd` | 69,231,104 | 16.19505 / 15.0154 / 43.2848 |

The median difference is -0.00810 ms: effectively unchanged, not evidence of a speedup. Both hashes
stayed unchanged, no AIZEN_HOME was created, the private Job settled with exactly 43 processes
(one worker plus 42 version invocations), and cleanup failures were empty. This also predates the
cross-target uninstall fix. Harness SHA-256:
`06081d799c4f4e9e4eb2b80c31977755686d1ba19c29dfd227775a90fdf0a924`;
shared support hash matches the earlier table.

The first startup attempt, `aizen-mnemos-startup-_ay9b6zl/report.json`, remains **FAIL**. Its Job did
settle, but it counted 85 rather than 43 processes because nested version invocations did not use
`DETACHED_PROCESS`. Its timings are not accepted performance evidence. The corrected fresh run
above adds that flag; neither failed report nor input executable was rewritten.

## Standalone Mnemos build/test evidence

Report: `%LOCALAPPDATA%/Temp/aizen-mnemos-final-verify-g_acj1ie/report.json`.
Source fingerprint before = after:
`3534260e35bf2aaae483866a0fb8890af022ca3ff8740820cf2f02352c9aa711`.

- `cargo fmt --all -- --check`: PASS.
- `cargo check --offline --all-targets`: PASS.
- `cargo test --offline --all-targets -- --test-threads=1`: 397 library tests and 14 subprocess RPC
  contract tests passed; binary target has no tests.
- `cargo check --offline --all-targets --features dense`: PASS.
- `cargo test --offline --lib --features dense rpc:: -- --test-threads=1`: 33 passed.
- `cargo build --offline --bin aizen-mnemos`: PASS; artifact used above.
- `cargo clippy --offline --all-targets -- -D warnings`: PASS.

These cover normal Windows persistence and injected errors, not power-loss atomicity or semantic
retrieval quality with real model weights. Native/RPC revisions remain advisory for supported
writers, not enforcement over external editors or older binaries.

### Relocated standalone build — before adoption/prepared-job edits

Report: `%LOCALAPPDATA%/Temp/mnemos-independent-build-i_py2gdh/report.json`.
The verifier copied only manifests/lock, license/NOTICE/README, source, tests and supplied fixtures
into a fresh standalone directory outside either checkout. All 74 copied file hashes matched before
and after verification; the full inventory is in the report.

| Command/artifact | Result |
|---|---|
| Offline locked normal/build dependency tree, all features | PASS; no entries from the verifier's forbidden native-build crate list |
| Offline locked plain release | PASS; 4,765,184 bytes; SHA-256 `102e71b54524a831cf7b5068c2c76d80dc1eccd13281b1aabfd78e10ffed535d` |
| Offline locked dense release | PASS; 6,762,496 bytes; SHA-256 `2c4a018d74f21b5d099c36d7c606f0a8d42fb28a5cad2d205a2bdf766c66dbce` |
| Offline locked all-targets strict Clippy (`-D warnings`) | PASS |

The relocated plain-release executable was then packaged and installed into a fresh disposable HOME
by the unchanged Core `bb27ff20...`. Report
`%LOCALAPPDATA%/Temp/aizen-memory-lifecycle-s9ktkjpv/report.json` passed all 59 executing lifecycle and
recovery cases, with data preserved, settled Jobs, no cleanup failures and both binary hashes
unchanged. This establishes independent build-to-package-to-Core execution on Windows; there was no
sibling source dependency or maintainer installation. It is not a non-Windows/runtime quality claim.

The earlier relocated report `mnemos-independent-build-82mibdoq/report.json` remains historical:
six live files changed after its inventory (README, protocol/RPC sources/tests), so its executable
hashes are not substituted for the current-source results above.

### Cross-target offline uninstall

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-ezcfcpv8/report.json`.
Source fingerprint before = after:
`602f52debd1cdd790184766805a0f285c7dd6c833321b607d89dec79bb4c8ffa`.
Check and debug build passed; 56 plugin tests and 2 hosted Memory-command handler tests passed.
The two added synthetic foreign-target tests verify offline removal against the saved package target,
not the current host target. Corrupt foreign payload still fails closed with pins/data preserved;
inspection, selection and execution still require host compatibility. This precedes the same-turn
learning and subsequent lint cleanup source changes, so it is not a current build verdict.

### Cross-target application reruns — before same-turn-model/lint changes

The complete reports have now been read for the later application reruns below. They use unchanged
Core SHA-256 `f3a4cd479a429d18c437c0323b88b0ceeaf6b0d0059f908563a95c8d8a1bfe0e` and relocated
Mnemos SHA-256 `102e71b54524a831cf7b5068c2c76d80dc1eccd13281b1aabfd78e10ffed535d`, not either
current build output:

| Harness | Result | Report below `%LOCALAPPDATA%/Temp/` |
|---|---|---|
| Executing lifecycle/recovery | PASS: 59 commands, data preserved | `aizen-memory-lifecycle-f2f8y22c/report.json` |
| Agent tools/context/recall | PASS: 11 commands, actual save/search and positive/negative recall | `aizen-mnemos-agent-o7z465th/report.json` |
| Offline completed-turn learning | PASS: 19 commands, 3 done receipts, repeated-text identities distinct | `aizen-mnemos-learning-999zxeuw/report.json` |

Every command Job settled; cleanup and model errors were empty, and both input hashes stayed
unchanged. The learning report confirms assistant transcript was not mined, clarification and step
exhaustion did not learn, and absent/disabled/auto-learn-off states suppressed learning. Harness and
shared-support hashes match the earlier tables. These runs remain offline Windows CLI/Agent
mechanics; they do not cover prepared model jobs or latest source.

### Same-turn learning source and failed checks

The same-turn prepared-job implementation is partial and has no Mnemos end-to-end proof.
Core now has separate legacy-safe model consent, typed prepare/complete bindings, captured chat
route/config and a bounded no-tool/no-redirect/no-retry completion path. Focused tests and review
are in progress. Mnemos's initialize `host_llm` refusal remains unchanged after the guard edit was
denied, and Mnemos does not advertise that capability. Dormant backend tests cannot establish a
reachable protocol path. No live model call, credential transfer to a plugin or maintainer activation
was performed.

Failed checks are retained, not overwritten:

- `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-so1_ozob/strict-clippy.log`: exit 101. The earlier
  strict Core Clippy pass reported 58 binary errors and 35 test errors (overlapping diagnostics,
  not 93 unique defects). A later cleanup is not a passing rerun.
- `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-kq31qyz5/check.log`: exit 101, 31.02 seconds.
  Compiler reported `learning_model` outside its scope in `finish_turn`, plus the unused variable
  in `run_agent_turn`. The source fix uses the already-frozen turn configuration and explicit effort,
  never a post-turn configuration/role lookup. The subsequent matched-source run below verifies
  that scope fix; it does not make the failed check successful.

### Core focused learning batch — passing checks, failing strict Clippy

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-8bwy42yf/report.json`.
Source fingerprint before = after:
`7f012b4f69b66acacae8335efb15e2a6901a9e2a51df199de6b9819e586c7ba0`.

| Command/group | Result |
|---|---|
| Offline Core check | PASS, 45.50 seconds |
| Dedicated extension HTTP tests | PASS, 5 tests |
| Plugin tests | PASS, 60 tests |
| One-shot tests | PASS, 4 tests |
| Hosted Memory command tests | PASS, 2 tests |
| Debug build | PASS, 59.47 seconds |
| Strict Core Clippy, binary and tests | **FAIL**, exit 101, 65.75 seconds |

The HTTP loopback tests exercise captured route/auth/model/effort, no tools, capped output, refusal of
redirects/retries and invalid/truncated/tool-bearing responses, body/text bounds and pre-network
output-limit validation. They do not send real credentials or contact a remote model. Strict Clippy
reported 14 binary and 14 test errors with overlapping diagnostics, including unused extracted
surfaces, style lints and fixture HOME locks held across await. Later source cleanup, canonical-turn
Persona guard regressions and explicit Core codebase model provisioning require their own fresh run;
this table does not verify those later edits.

### Explicit adoption fixture slice

Standalone source now implements native plan/apply/resume/restore without constructing Engine during
adoption. The final scoped log
`%LOCALAPPDATA%/Temp/claude/c--Users-admin-Desktop-mini-project-aizen/0de90368-3b98-4eb2-960a-77d393f5bd00/tasks/byjc2xlb9.output`
was read: scoped strict Clippy (`--lib --bins --test adoption_contract -- -D warnings`) passed, and
all 15 adoption contracts passed in 12.45 seconds, including native CLI roundtrip. Coverage includes
byte-preserving allowlisted publication, complete backups including Core/derived exclusions,
reviewed digest and source drift, durable interruption boundaries, explicit idempotent resume,
restore into a new directory, no-overwrite races, torn/corrupt refusal, hardlinks, junctions and ADS.

The implementer also reported an earlier 443-test whole-package pass (408 library, 15 adoption,
6 prepared and 14 RPC). That report was not substituted for final matched-source verification;
the separate pinned all-target run below subsequently passed. Independent adoption review later
found an unsafe restore-origin lock path; the fix and two new contracts are recorded below, with
all 17 adoption contracts verified in the fresh whole-package pass. Backups are tamper-evident, not
WORM; Linux/macOS and power-loss behavior remain unverified. No real legacy data was inspected or adopted.

### Core canonical-turn/lint cleanup — fresh matched-source pass

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-oa0g6nh_/report.json`.
Source fingerprint before = after:
`cb8fa38425f1a1049f0a1fe6355f3e6c1fac5bb7063ef11832a6793cab4a8c95`.
Check, debug build and strict Clippy (`--bin aizen --tests -- -D warnings`) all passed. The selected
200 tests passed: identity 14, postturn 5, turn signals 5, slug 12, codebase CLI parsing 1, provider
admission 1, retrieval 41, extension HTTP 5, plugin 62, sandbox 48, one-shot 4 and hosted Memory
commands 2. These group filters were run serially; they are not the full Core unit suite.

New regressions verify that synthetic user-role continuations retain the canonical turn and do not
reset the actual-tool Persona-authoring guard. A completed Persona-authoring turn creates no
learning outbox/data, while a later canonical user reaches normal package validation. Protocol
operation names round-trip; unknown names fail before encoding. Explicit Core codebase provisioning
has owned loopback tests for bounded/truncated/error transfers, no-clobber publication and partial
cleanup on cancellation; no model weights were downloaded. The current build/lint pass does not
resolve the initialize permission denial or prove the Mnemos model-learning path reachable.

### Standalone adoption/prepared source — fresh matched-source pass

Report: `%LOCALAPPDATA%/Temp/aizen-mnemos-final-verify-qsc9uvfq/report.json`.
Source fingerprint before = after:
`09b97b9cc0555e035edc2375eff7f4acc34220fe9db38edc79ff653de2f4af82`.

- Full formatting and offline all-target check: PASS.
- Offline all-target tests: **443 passed** (408 library, 15 adoption, 6 prepared-learning and 14 RPC).
- Offline all-target dense check: PASS; 44 dense library RPC tests passed.
- Debug binary build: PASS.
- Offline all-target strict Clippy (`-D warnings`): PASS.

This run covers the helper/test-module structural move and the then-current adoption source.
Prepared backend tests remain dormant-path tests: Mnemos still refuses `host_llm` initialization and
advertises no such capability. Independent review subsequently found persistence/ID-allocation and
adoption-lock defects; the 443-test pass does not verify their fixes or disprove those findings.
These are Windows fixture results, not non-Windows runtime, power-loss, semantic-quality or real-data
evidence.

### Canonical-turn feature matrix and application reruns — before review fixes

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-fqa7__da/report.json`.
Source before = after: `cb8fa38425f1a1049f0a1fe6355f3e6c1fac5bb7063ef11832a6793cab4a8c95`,
matching the 200-test/default strict-Clippy batch above. No-default check, dense check and strict
dense Clippy (`--features dense --bin aizen --tests -- -D warnings`) passed.

Three actual-application runs used unchanged Core SHA-256
`c2c2a9fe8fb2abc2632da29d44b603f9adf113725235edb3a02bd8d722cf49c1` and Mnemos SHA-256
`94c6d21fbde9f928778156fa8e688333855e33ce6e7ecd17f41268215f447e8e`:

| Harness | Result | Report below `%LOCALAPPDATA%/Temp/` |
|---|---|---|
| Executing lifecycle/recovery | PASS: 59 commands, provider executed and data preserved | `aizen-memory-lifecycle-pu5_cl6p/report.json` |
| Agent tools/context/recall | PASS: 11 commands, actual save/search, standing context and positive/negative recall | `aizen-mnemos-agent-nj3_x1_2/report.json` |
| Offline completed-turn learning | PASS: 19 commands, 3 learning receipts, distinct repeated-text turns | `aizen-mnemos-learning-uudksbmr/report.json` |

The complete reports were read. Every command Job settled, cleanup/model error collections were
empty and hashes stayed unchanged. Learning again verified no remote model sharing, no assistant
transcript mining, no learning on clarification/budget exhaustion, and preservation on disable.
Lifecycle/shared/Agent/learning harness hashes match the uninstall-recovery and initial tables.
These executables predate the following review fixes; they are not proof of latest-source behavior.

### Independent review findings and source fixes

Read-only reviews completed on 2026-10-31. Findings were source-verified, not initially executable
reproducers. Source changes and new regressions have since been written. The fresh Core and standalone
runs below verify their fixes on Windows without opening Mnemos's denied model-access guard:

- **Core replay boundary:** prepared completion bypassed tape replay/strict and could send a new
  HTTP request. It now refuses before HTTP; new loopback regression checks both modes and no request.
- **Core spending cap:** a job could raise a smaller captured `max_tokens`. The source now intersects
  both caps and refuses zero. Regressions cover both directions and an absent captured cap.
- **Cold output-cap dialect:** no-retry jobs could fail on a route accepting only
  `max_completion_tokens` before the ordinary-chat quirk was learned. An explicit config-wide
  `output_token_parameter` now chooses that wire field without a probe/retry. This does not discover
  unknown endpoint dialects, and it is not a provider-profile setting.
- **Adoption restore-origin lock:** resume trusted `parent_journal` for a create-if-missing lock
  before validating its journal. A tampered manifest could create `run.lock` in the legacy source.
  Source now validates origins read-only first, locks existing checked handles only, revalidates
  after locking and uses origin-before-restore order. Two passing Windows contracts cover redirected
  origins and missing locks; normal adoption/resume/restore also passed with existing read-only handles.
- **Prepared application persistence:** the native best-effort fact API swallowed load/confirm/
  review/write errors and could yield an accepted empty or partial receipt. RPC now uses a fallible
  path that propagates these errors into the existing durable recovery barrier. New regressions
  use real internal filesystem/revision failures, including a Windows deny-delete handle, rather
  than only a post-execution fault hook. Native best-effort behavior remains separate.
- **Prepared logical-ID allocation:** exact lowercase-path checks missed imported mixed-case `.MD`
  names on case-sensitive filesystems. Allocation now uses logical inventory including nonfiles
  and errors on suffix exhaustion instead of reusing a base ID. Portable tests plus a Unix-specific
  mixed-case/dangling-link test are present; the Unix runtime case is not locally verified.

The new replay test initially referenced a nonexistent fixture helper (`E0433`). It now uses the
existing owned fixture API; the fresh Core run below verifies compilation and all eight HTTP tests.
Mnemos's denied initialize guard remains unchanged and `host_llm` is still not advertised.

### Review-fix verification — fresh matched source

Core report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-8d7vnsog/report.json`.
Source before = after: `c2477199e716716372c764e6d214b5b6bc7269486d098de1ec09236056988455`.
Offline check, debug build, no-default/dense checks and default/dense strict Clippy passed. The 96
selected tests passed: identity 14, postturn 5, turn signals 5, Memory config 2, extension HTTP 8 and
plugin 62. In particular, replay/strict emitted no HTTP request, tighter captured caps were retained,
and a cold explicitly configured completion-token dialect used one request without warming quirks.
This is not reachable Mnemos host-model learning or a full Core regression pass.

Standalone report: `%LOCALAPPDATA%/Temp/aizen-mnemos-final-verify-d7ee5eej/report.json`.
Source before = after: `f8fc3f142ba295613982494b34cf22637605c308d72933116928fa1c713a3187`.
Full formatting, offline all-target check, dense check, debug build and all-target strict Clippy
passed. All **455 tests** passed: 418 library, 17 adoption, 6 prepared subprocess and 14 RPC subprocess
contracts. Another 49 dense library RPC tests passed. New real persistence-failure tests verify that
failed add/confirmation/review/record replacement and partial batches retain unresolved intent and
recovery barriers rather than accepted terminal receipts. Revision reservations survive failed
Windows replacement; exact retry does not mutate. The two new adoption tests passed with existing
checked read-only lock handles. Logical-ID inventory/error/exhaustion tests passed on Windows;
the Unix mixed-case/dangling-link runtime regression was not executed here.

`python -I scripts/test_package_memory_plugin.py` also passed all 25 offline metadata/archive tests.
These use synthetic non-native bytes, never execute them and do not prove installer native-format
validation. The tracked diff whitespace check passed; Git only reported CRLF conversion warnings.

### Review-fix binaries — actual application reruns

The complete reports for all three fresh Windows application runs were read. They use unchanged Core
SHA-256 `bf273443b235dd15f399bdd102f0e220146c08acd1ea99d54787c612d4c47f77` (source `c2477199...`)
and Mnemos SHA-256 `43f5e6bb6419c4714d9d8316004283dcfebf5ca0683af432e98f49e429efb67a`
(source `f8fc3f14...`):

| Harness | Result | Report below `%LOCALAPPDATA%/Temp/` |
|---|---|---|
| Executing lifecycle/recovery | PASS: 59 commands, data/pins preserved on refused corrupt removal | `aizen-memory-lifecycle-1uu7vu53/report.json` |
| Agent tools/context/recall | PASS: 11 commands, save/search, positive context/recall, unrelated recall rejected | `aizen-mnemos-agent-bd4v2t4l/report.json` |
| Offline completed-turn learning | PASS: 19 commands, 3 learning receipts, no remote model sharing | `aizen-mnemos-learning-d8bb15ja/report.json` |

Every Job settled, all cleanup/model error collections were empty, and both binaries stayed unchanged.
Disabled runs retained Core session/Skill/Persona tools while removing Memory tools/context; provider
data/receipts were preserved. Learning did not mine assistant text and skipped clarification/budget
exhaustion. Harness hashes match the earlier tables. This remains scripted offline CLI/Agent evidence,
not retained REPL, live bot/cron, reachable Mnemos model learning, semantic quality or power-loss proof.

### Standalone prebuilt-only package — actual installer/lifecycle run

The sibling now owns `scripts/package.py`, `scripts/test_package.py`,
`scripts/check_dependencies.py` and `.github/workflows/standalone.yml`. Packaging uses only the
standalone tree and supplied prebuilt binary, not Core's generic packager. No payload is executed
while packaging. The workflow defines three platforms × lexical/dense (six combinations), read-only
repository permissions and no upload/release step. A local PyYAML structure check passed; actionlint
was unavailable and no remote workflow was dispatched. Linux/macOS execution is still unverified.

The final Windows packaging run passed **17 tests with 1 explicitly skipped symlink test** (18 total).
Coverage includes deterministic ZIP/checksums, closed network/model permissions, three target
manifests, empty input/size refusal, hardlinks, same-size replacement, bounded growth, no-overwrite
races and retained partial output after checksum failure. These synthetic bytes are never executed
and do not establish native executable format validity. An independent source review found three
actual defects, all addressed before the successful application run: compression above Core's
200:1 ceiling (use Stored when needed), empty executable acceptance, and pathname-only input checks
(replaced by bounded, identity-checked open-file reads). Input/output directories must still be
trusted; the packager is not a sandbox against arbitrary concurrent ancestor renames.

A first hardened test run failed because Python 3.12 reports Windows ctime differently for pathname
stat and CRT handles. A first actual package attempt also failed before install at
`mnemos-standalone-package-run-7o857kxy/package.log`: Windows synthesizes executable mode bits for
`.exe` path stats but not handles. Comparison now uses file type on Windows, with identity/link count/
size/mtime and deny-write/delete sharing retained. A new `.exe` regression passed. These failures
were not rewritten as successful attempts. The dependency checker passed offline for all-feature
Windows metadata: 137 resolved packages, no external Cargo path dependency or named native-build
crate. The denylist is a regression check, not proof of every crate implementation.

Successful package report: `%LOCALAPPDATA%/Temp/mnemos-standalone-package-run-le66sd_a/report.json`.
Package SHA-256 `ac85c311a4a474555615b71695526207b0ad304944d524b704d8a9c5909ef5d3`,
3,101,611 bytes, contains the previously verified debug Mnemos `43f5e6bb...`.
Packager SHA-256 `1517768a017a089a4172862994a5a82ceb10f27609ace196130a84e7831d5461`;
Core is unchanged `bf273443...`. The exact ZIP was installed and executed through the actual Core:
`%LOCALAPPDATA%/Temp/aizen-memory-lifecycle-bhe7v5e3/report.json` passed **58 command cases**.
This is the earlier 59-case lifecycle without invoking Core's generic packager, not reduced CRUD/
recovery coverage. All Jobs settled, no cleanup failed, both binary hashes stayed unchanged,
installation/eligibility/selection remained separate, and disable/uninstall preserved data.
The harness checks the installed executable hash against the explicitly supplied Mnemos before
provider execution. Lifecycle script SHA-256:
`41d0b45f890d8e005ec9e784fbe46dd74a89f263dcff0aa70b98bd690966612d`.
This is fixture-owned Windows guarded CLI coverage, not real installation, publication or host-model
learning. Output publication is exclusive but not atomic; partial failed output is retained.

Two relocated verification attempts (`mnemos-independent-build-3xg34l4b` and
`mnemos-independent-build-s95d2dev`) are retained as **FAIL**: their frozen copies built release
executables, but the verifier detected packaging-script/workflow edits in the live sibling while
running. Their successful substeps are not a whole matched-source pass.

### Standalone relocated releases and packages — fresh matched-source PASS

Report: `%LOCALAPPDATA%/Temp/mnemos-independent-build-unuxpcbw/report.json`.
A third run copied the allowlisted standalone manifests, source, tests, fixtures, license/docs,
packaging scripts and workflow into a new directory outside both checkouts. The complete live/copy
inventory stayed identical through every step (`source_unchanged:true`). No Core source/include/path
build dependency was supplied. Package tests passed (17 passed, 1 symlink skip), the offline locked
all-feature normal/build dependency tree passed its named native-build denylist, both offline locked
release builds and their relocated packager passed, and all-target strict Clippy passed. The package
was made by the copied standalone script, not a Core tool. Windows builds requested
`RUSTFLAGS=-C target-feature=+crt-static`. A subsequent local MSVC `dumpbin /dependents` check passed
for both exact saved executables: each imports only `kernel32.dll`, `ntdll.dll`,
`api-ms-win-core-synch-l1-2-0.dll`, `bcryptprimitives.dll` and `advapi32.dll`. No dynamic MSVC CRT or
third-party runtime DLL appears in those import tables; Windows OS DLLs remain required. This is
not a Linux/macOS static-link verification.

| Mnemos build | Executable bytes | Executable SHA-256 | Standalone ZIP SHA-256 |
|---|---:|---|---|
| Plain | 5,407,744 | `6ec4db96d148338479be98fe29bb9c5e48a34f49dac3963b9f2ca6a28a94d5ff` | `90b0af8fcf92625cf9fe2e2fb3a6d02bcb06923c64085a826ee0133f7a284092` |
| Dense | 7,392,256 | `7bbc29c9c829a3a456897661273b3df77be4ef1946ebec3a5bc0424e6512a3e5` | `332ecae47a89b09f96c47555fafca4cc2bf1c853e873c66afce654c3c331387a` |

The exact two ZIPs were then installed, explicitly activated and exercised by the same unchanged
**plain release Core** `2b055a72...`. Wrapper report:
`%LOCALAPPDATA%/Temp/mnemos-relocated-release-app-09yvuwno/report.json`.

| Package | Actual application result | Report below `%LOCALAPPDATA%/Temp/` |
|---|---|---|
| Plain | PASS: 58 lifecycle/CRUD/recovery commands | `aizen-memory-lifecycle-shq2283a/report.json` |
| Dense | PASS: 58 lifecycle/CRUD/recovery commands | `aizen-memory-lifecycle-fip42cpy/report.json` |

All 116 command Jobs settled, both providers actually executed, cleanup failures were empty, and
Core/provider/package hashes stayed unchanged. Install did not select or execute; eligibility and
selection remained explicit. Refused corrupt uninstall disabled the package and retained pins;
missing-package retirement and normal uninstall preserved data. These are disposable fixture roots,
not the maintainer's installation. Dense ran without semantic weights: it verifies lexical/runtime
mechanics, not semantic quality. No remote CI, publication or Linux/macOS runtime result is implied.
The CI definition additionally passed local Bash syntax checks for eight inline blocks; actionlint
and remote jobs were not run.

### Whole-Core regression and release builds — fresh matched-source PASS

Report: `%LOCALAPPDATA%/Temp/aizen-memory-core-verify-di2zvtoc/report.json`.
Source before = after: `c2477199e716716372c764e6d214b5b6bc7269486d098de1ec09236056988455`,
matching the review-fix checks/Clippy and debug application binaries above.
`cargo test --offline --bin aizen -- --test-threads=1` passed **3,049 tests, 0 failed, 2 ignored**
(997.21 seconds test runtime; 998.56 seconds command). The ignored Rust-analyzer navigation and
reach end-to-end tests were not run. Broad integration targets, notably credential-discovering
`tests/eval.rs` and non-isolated `tests/resume_plan.rs`, were deliberately excluded. This green
whole-binary run does not erase earlier checkpoint/Windows resource-failure reports.

Both saved release builds passed; source fingerprint remained unchanged throughout:

| Build | Bytes | SHA-256 |
|---|---:|---|
| Plain | 46,466,560 | `2b055a72ad7ddf69c94efbfa929c2af8608bef19881899848296af801ed4ffd8` |
| Dense | 48,470,016 | `b895a330bcc79517b11dc39c0ee7b4a3c1cace91632a812189b1761653179fb8` |

They are separate local build artifacts, not installed binaries or published releases.

### Paired saved-release startup and size — matched artifacts

Report: `%LOCALAPPDATA%/Temp/mnemos-paired-release-metrics-6daa3cfw/report.json`.
The harness compared the saved `e3bd8c9` plain/dense releases with the saved `c2477199...` releases
above in the same owned HOME/workspace and scrubbed environment. It alternated pair order, discarded
one warmup per artifact and retained 20 samples each. All input hashes stayed unchanged.

| Artifact | Bytes | Warm `--version` median / min / max, ms |
|---|---:|---|
| Checkpoint plain | 46,351,872 | 13.3550 / 12.6326 / 42.5713 |
| Extracted plain | 46,466,560 | 13.4144 / 12.5674 / 40.4236 |
| Checkpoint dense | 48,301,056 | 13.3206 / 12.2334 / 15.1223 |
| Extracted dense | 48,470,016 | 13.6870 / 12.9124 / 14.6106 |

Checkpoint plain SHA-256 is `f363e8f272dbe43e1947b153c6c075ebfbb73134a83c1d2c3d49f1a5b7b6dba7`;
checkpoint dense is `cb767269159501ddf631c69dff6543593f1d42af98eb2df7f7a8d5cd0130123a`.
Plain grew 114,688 bytes (about 0.247%) and its observed median grew 0.0594 ms; dense grew 168,960
bytes (about 0.350%) and 0.3664 ms. This is not a binary-size or startup improvement. The measurement
is warm subprocess wall time on one Windows host, not cold/internal startup, TUI readiness or a
statistically established regression. Process creation, system load and antivirus were not isolated.

Twenty warm no-provider `memory status` samples on the new plain release measured median 15.0496 ms
(min 14.2172, max 43.1922); the fixture AIZEN_HOME was not initialized. This is offline status only,
not enabled-provider latency, disabled idle monitoring or retrieval performance. Those and final
retrieval/source-removal acceptance remain separate gates.

### Source-pinned per-query retrieval and fixed-date decay — algorithm-slice PASS

Report: `%LOCALAPPDATA%/Temp/mnemos-retrieval-parity-58z90v5c/report.json`.
`scripts/mnemos_retrieval_parity.py` reads original source bytes directly from Git commit
`e3bd8c977e472a6b17b4240b26ef4b0edfbc8f87` and copies candidate bytes from standalone Mnemos.
It builds two separate tiny Rust observers offline, copying tokenizer/BM25/RRF/provenance/decay
modules and verbatim marker-delimited search/gate/hash-embedder spans. The only substitutions are
an in-memory record shape and `rrf_k=10` settings seam; no shipping Rust source was changed.
Both observers resolve the same dependency lock SHA-256
`3dab1e579639d324f68b6f0b482149800e85e674a62a94da19318e6062a9a06f`.
Harness SHA-256: `4f4982656183e0460957355a3dfd183fce1a7bccdfb8678e5a79d47a89b2179a`.
Candidate sources, input lock, fixtures and harness stayed unchanged throughout the final run.

- Each observer emitted **1,665 retrieval rows**: 28 original gate/tune queries plus nine edge
  queries, across nine exact/fuzzy/hybrid/gated/exclusion modes, repeated five times.
- Strict ordered record IDs matched in every row; aligned scores, weighted recall coverage/admission,
  dense top-hit coverage and counted embedder invocations matched with finite numeric tolerance
  `1e-12`. No order normalization or score-based tie forgiveness was applied.
- Neither implementation showed within-run repeated-query instability in these samples. Random
  `HashSet` summation and fuzzy `HashMap` equal-similarity iteration remain inherited hazards;
  five repeats do not prove determinism for other inputs.
- All **140 fixed-date decay cases** matched: four provenance kinds × five confirmation counts ×
  seven missing/created/updated/used/retrieved/invalid/future-date cases, with `today=2026-06-01`,
  half-life 30 days and base score 2.75. Decay, salience, strength, evolved score and archive decision
  were compared, not the machine's current date.
- All 13 Python verdict/marker regression tests passed, including shared repeat instability,
  strict ordering, numeric tolerance, boolean typing, nonfinite values and empty/truncated evidence.

The earlier `mnemos-retrieval-parity-f1i24iy7/report.json` remains **FAIL**: baseline build could not
find `link.exe` in the scrubbed environment and never ran an observer. Explicit MSVC/Windows SDK
paths fixed this fixture setup, without a global override. The separate first successful
`mnemos-retrieval-parity-xji0xyws/report.json` also passed 1,665/140 rows per side; the final run above
adds harness/generated-source/lock fingerprints and the extra boolean-comparison regression.

This is **algorithm-slice evidence**, not full engine/store/scoping/frozen/pending-ledger or actual
Core/provider RPC parity. The recall gate uses the full fixture corpus like the existing bench;
exclusion ranking rebuilds its candidate-relative index. Hash embeddings test fusion mechanics,
not semantic model quality. It does not authorize removal of the old Core source files.

### Enabled-provider CLI wall time — plain PASS, dense failure retained

The actual unchanged plain release Core `2b055a72...` installed the exact independently packaged
plain Mnemos `6ec4db96...` / ZIP `90b0af8f...` into a fresh fixture HOME, explicitly enabled/selected
it with guarded consent, and seeded 26 synthetic Project records through Core CLI operations.
Report: `%LOCALAPPDATA%/Temp/aizen-mnemos-latency-rasw_r_6/report.json`.
`scripts/mnemos_latency_smoke.py` discarded one warmup per route and retained 20 warm samples:

| Route | Median wall time (ms) |
|---|---:|
| Active offline status | 16.0904 |
| Provider health | 182.6060 |
| Literal search | 205.0723 |
| Vietnamese search | 237.8642 |
| Unmatched lexical search | 236.9758 |
| Disabled offline status | 15.5430 |

All 159 Core commands succeeded or matched their expected refusal. The worker and its descendants
settled in the private Job (380 total processes, peak sampled 4); no cleanup failed. Core, provider,
package and specification hashes stayed unchanged, and disabling/status/refused health preserved
the provider-data inventory. Harness SHA-256:
`89c812523896860d728ea45adb599be7932401613114678158f31fed0c5b288e`.

The same harness with the earlier dense Mnemos `7bbc29c9...` / ZIP `332ecae4...` is **FAIL**:
`%LOCALAPPDATA%/Temp/aizen-mnemos-latency-3k5thxi9/report.json`. Health and positive searches ran,
but the unmatched-query warmup returned two irrelevant facts (`vn-test`, `dont-edit-dist`) instead
of abstaining. The Job settled (297 processes), with no cleanup failures. Source inspection found
that the hosted adapter enabled dense fusion from the Cargo feature alone, despite having no
explicit model roots, allowing non-semantic hash fallback into ranking. This failure is not erased
by the earlier lifecycle passes or algorithm-slice comparison, which did not test that hosted
initialization default. A correction needs fresh regressions, builds and application evidence.

These are one-host, sequential warm **whole-CLI** measurements, including Core startup, package
verification, sandbox/provider startup, the operation and settlement. They do not isolate IPC or
engine time, compare against the former in-process engine, establish semantic quality, measure
cold starts or prove zero disabled-idle activity by OS tracing. Windows guarded is not kernel
filesystem/network confinement. No real installation or data was used.

### Hosted retrieval adapter corrections — fresh test PASS

Read-only review found that RPC `list` checked only `superseded_by`, unlike the engine's live view.
It could return expired (`validTo` only) records or predecessors retired by a live successor's
forward `supersedes` claim. Two new RPC regressions reproduced both failures before the fix,
including pagination, an out-of-scope live claimant and a retired claimant that must not hide a
survivor. List now computes `supersede::active` over the full corpus before visibility/sort/limit.
The validator already refuses unsupported non-default scopes/dimension/category/superseded filters;
no silent-filter defect was found. `archived` is an invalid query field, not a supported filter.

A new actual standalone RPC subprocess regression also reproduced the model-free dense false hits
from the latency run above. Hosted initialization now retains the default lexical floor rather
than enabling dense solely because it was compiled in. No model roots are currently supplied by
this adapter; the native explicit `--dense` mechanics remain separate. A dense binary still
conservatively journals search/recall and requires operation IDs. The regression checks unrelated
search/recall abstention, a positive literal hit and no embedding-cache creation on both builds.
It uses fixture records and the real initialized binary, not a manually constructed test Server.
Mnemos's `host_llm` initialization refusal and capability list are unchanged.

Pre-fix report `%LOCALAPPDATA%/Temp/mnemos-retrieval-adapter-6k0_ae_3/report.json` retains the two
list assertion failures and dense subprocess assertion failure (Cargo exit 101 for each test run).
They compiled and failed at the expected outputs, not a missing-toolchain error.
Post-fix report `%LOCALAPPDATA%/Temp/mnemos-retrieval-adapter-a5fpb5se/report.json` passed with
unchanged Rust source fingerprint
`70b6cc5fc829eb3a6eff405a14ee910afd11638905cefcb58c18c27791f95f2e`:

- Full standalone suite: **458 tests passed** (420 library, 17 adoption, 6 prepared subprocess,
  15 RPC subprocess), zero failed.
- Dense library RPC: **51 passed**; dense actual RPC subprocess contracts: **15 passed**.
- Whole-package formatting and default/dense all-target strict Clippy passed offline.
- Core product Rust is unchanged. The earlier whole-Core proof remains tied to `c2477199...`;
  these are new standalone results, not a rerun of Core or the earlier release binaries.

The fresh relocated build and actual application results below complete this correction's release
check; they do not change the result of the earlier failed dense attempt.

### Corrected hosted retrieval — relocated release and actual application PASS

Report `%LOCALAPPDATA%/Temp/mnemos-independent-build-ccc4vvex/report.json` records fresh offline
plain/dense static-CRT release builds from an allowlisted standalone copy outside both checkouts,
its own prebuilt-only packaging, packaging tests, dependency check and strict Clippy. The source
inventory stayed unchanged and the dependency checker reported no forbidden native dependencies.
These are new artifacts, not the earlier `6ec4db96...` / `7bbc29c9...` binaries:

| Artifact | Bytes | SHA-256 |
|---|---:|---|
| Plain Mnemos executable | 5,407,744 | `3755d8ef4c740070c0a01dfeadaef10813b16c47d06fe61f136805c0a2d17910` |
| Dense Mnemos executable | 7,392,256 | `d79ee1cf5babf94ee0f0577f3cc3eef3eed027c58589825d23e62fdb35a06b9e` |
| Plain ZIP | — | `ebd95b54442f22423ed9a61b723265c8667b73be881ddfac4ceae34c2b51eddf` |
| Dense ZIP | — | `d06894d09bd1f872f82f31442d56d956b7d7c0bd530955cd99528375cb302e81` |

Fresh `dumpbin /dependents` inspection of both exact executables succeeded: imports were only
`kernel32.dll`, `ntdll.dll`, `api-ms-win-core-synch-l1-2-0.dll`, `bcryptprimitives.dll` and
`advapi32.dll`. No VC runtime, OpenSSL or other third-party DLL was listed. This Windows import-table
check is not a Linux/macOS runtime result or a proof about unused dependency source.

Application wrapper `%LOCALAPPDATA%/Temp/mnemos-adapter-release-app-lcbejwgj/report.json` passed all
four steps through the unchanged plain release Core
`2b055a72ad7ddf69c94efbfa929c2af8608bef19881899848296af801ed4ffd8`:

- Plain ZIP: **58 lifecycle cases passed**;
  `%LOCALAPPDATA%/Temp/aizen-memory-lifecycle-fouv8a2l/report.json`.
- Dense ZIP: **58 lifecycle cases passed**;
  `%LOCALAPPDATA%/Temp/aizen-memory-lifecycle-tdhmn0xr/report.json`.
- Plain latency: `%LOCALAPPDATA%/Temp/aizen-mnemos-latency-jjvqbimf/report.json`.
- Dense latency: `%LOCALAPPDATA%/Temp/aizen-mnemos-latency-8x89ycea/report.json`.

Both lifecycle runs executed the provider, retained provider data and recorded no cleanup failures.
Each latency run passed 159 Core commands, including unrelated-query abstention; its private Job
settled (380 total processes, peak sampled 4), with no cleanup failures and unchanged disabled-data
inventory. Wrapper source/input pins stayed unchanged. With one warmup discarded and 20 retained
warm samples per route, whole-CLI medians were:

| Route | Plain (ms) | Dense build (ms) |
|---|---:|---:|
| Active offline status | 16.6237 | 16.3496 |
| Provider health | 191.9706 | 200.1011 |
| Literal search | 214.5726 | 219.0659 |
| Vietnamese search | 213.1380 | 220.5045 |
| Unmatched lexical search | 214.8068 | 227.0216 |
| Disabled offline status | 16.4036 | 16.9620 |

The same whole-CLI/one-host/guarded limitations above apply. Both hosted builds now use lexical/
fuzzy retrieval without semantic-model roots; a passing dense-build run is not semantic retrieval
quality evidence. This closes the reproduced adapter regressions, not full engine/scoping/RPC
parity, all entry surfaces, cold-start performance or the remaining extraction gates.


### Completed-turn cancellation inheritance — verification in progress

The next Core correction retains the originating `AgentConfig.cancel` alongside the execution
context in queued learning events. CLI/capture, REPL and hostbot call sites pass that exact token;
offline/prepared/completion RPCs no longer invent unrelated tokens. Cancelled/expired `Done` events
are gated before config/outbox reservation and again before dispatch. A bounded model-wait boundary
also observes cancellation and clamps to the original execution deadline; no fallback/retry is added.
Mnemos's `host_llm` refusal and advertised capabilities remain unchanged.

The first fresh allowlisted test run stopped during `ring` compilation because MSVC's `stddef.h`
include directory was missing: `%LOCALAPPDATA%/Temp/mnemos-learning-cancel-6mgwvqpv/events-red.log`.
That exit 101 is a build-environment failure, not a reproduced regression. After explicit MSVC/SDK
include paths were added, `%LOCALAPPDATA%/Temp/mnemos-learning-cancel-qxq_k03l/report.json` records
both new pre-fix tests compiling and failing at their expected cancellation/pre-claim assertions
(3 existing tests passed, 2 failed). The first corrected-source run passed at fingerprint
`3b91b0857d7bb2a13d13b1c2476021c5733133f5f83934c2ce452efcc5a100a3`:
`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-ybcnwibd/report.json`. It recorded 66 plugin,
4 one-shot, 6 cancellation, 10 REPL-turn and 69 hostbot tests passing, no-default/dense checks,
and default/dense strict Clippy. These focused results are not a whole-Core or live-surface run.

A subsequent read-only review found a separate logical-observer gap: the actor already clamps
physical work to the execution deadline, but the async caller previously used only a fixed 95-second
wait. `%LOCALAPPDATA%/Temp/mnemos-learning-cancel-9i30waz7/observer-red.log` records the new
pre-enqueue regression compiling and failing before the follow-up fix. Both async and blocking
boundaries now reject already-cancelled/expired work before selection/enqueue; their observer ceiling
is the earlier of the inherited deadline and 95 seconds. Async expiry/drop cancels only the request
child and reports unknown physical effects; actor ownership is not dropped or declared settled.
New local-channel/drain-token tests cover deadline expiry, queued-observer drop, parent/sibling
isolation, terminal-response priority and retained ownership until tree plus pipe settlement.
A second narrowly scoped read-only review found no new correctness defect in this observer patch;
that review read code/tests only and is not runtime evidence.
The follow-up source fingerprint is
`807a7e4726256c9ee9403303769626dc6f948ceafcb5b253b51cc7fa8469e8f1`.
`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-edd19gil/report.json` records unchanged source and
70 plugin, 4 one-shot, 6 cancellation, 10 REPL-turn and 69 hostbot tests passing, no-default/dense
checks, and default/dense strict Clippy. Its whole-binary unit step **failed** after 799.31 seconds:
3,049 passed, 8 failed, 2 ignored. Seven failures require Git, but the verifier reused the application
fixture's intentionally nonexistent `AIZEN_GIT` and minimal PATH. The eighth background-process test
reported `server exited before regression body`; its command names `powershell.exe`, whose directory
was also absent from that PATH.

The scratch verifier now explicitly supplies installed Git and Windows PowerShell only for whole-unit
and exact-retest modes, with synthetic HOME/temp and empty global/system Git configuration. The
application/provider fixture environment and production sandbox policy remain unchanged.
`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-_bbkuglj/report.json` records all eight exact reruns
passing, each with one verified test execution, at the same unchanged `807a7e47...` fingerprint.
No production-source edit was needed. The fresh whole-binary rerun is now recorded in
`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-yo7128j9/report.json`: 3,057 passed, 0 failed,
2 ignored (`cargo test --offline --locked --bin aizen -- --test-threads=1`, 1,043.55 seconds
including Cargo). Its before/after fingerprint remains `807a7e47...`. The original eight-failure
report is retained; it was not rewritten or counted as passing. The older `3b91b085...` results
do not cover this follow-up, and unsafe/live integration targets were not run.

`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-vblb8rt4/report.json` records both fresh offline/locked
release builds passing with before/after source equal to `807a7e47...`:

| Configuration | Bytes | SHA-256 |
|---|---:|---|
| Plain Core | 46,476,288 | `1b0e51084225db9203f1f99bf1bc2f507ca4101e7d2e82d6f9b45eee0f9c7388` |
| Dense Core | 48,484,864 | `ec3533e88e0a7404921d62f4bcad46258071e505d71e368175b932edde1031b4` |

These artifacts are copied to that report's owned directory, not installed.

Fresh application wrapper: `%LOCALAPPDATA%/Temp/mnemos-learning-cancel-app-43iog8gs/report.json`.
All six actual CLI/Agent runs passed against those two Core releases and the independently built
plain Mnemos release/package from `mnemos-independent-build-ccc4vvex`. The wrapper verifies live
Core source, both Core hashes, standalone source inventory, provider/ZIP hashes and harness hashes
before/after each run. Each nested case reports settled Jobs and empty cleanup-failure lists.

| Core | Fixture | Cases | Nested report below `%LOCALAPPDATA%/Temp/` |
|---|---|---:|---|
| Plain | Executing lifecycle | 58 | `aizen-memory-lifecycle-k5k7v30y/report.json` |
| Plain | Agent tools/recall/context | 11 | `aizen-mnemos-agent-srtnjn8v/report.json` |
| Plain | Offline completed-turn learning | 19 | `aizen-mnemos-learning-g3_mldl8/report.json` |
| Dense | Executing lifecycle | 58 | `aizen-memory-lifecycle-fkgos20m/report.json` |
| Dense | Agent tools/recall/context | 11 | `aizen-mnemos-agent-7al2wmnr/report.json` |
| Dense | Offline completed-turn learning | 19 | `aizen-mnemos-learning-im57kbvd/report.json` |

These repeat the previously described install/selection/data-preservation, real Agent tools/context
and offline Done-event receipt checks, not a real hung-provider/observer-cancel injection. They do
not open Mnemos's host-model guard or prove live model learning. Installs are disposable fixture-only;
the unchanged Core binary is not rebuilt by installation.

Paired startup report: `%LOCALAPPDATA%/Temp/aizen-mnemos-startup-9y53trxf/report.json`.
The four input hashes match the retained `e3bd8c9` checkpoint release builds and fresh `807a7e47...`
release builds. One discarded warmup and 20 alternating-order samples per artifact ran in one owned
Job (85 total processes, settled, no cleanup failure). All return `aizen 0.7.3`, create no host state
and retain their hashes.

| Configuration | Checkpoint bytes | Current bytes | Byte delta | Checkpoint warm median ms | Current warm median ms |
|---|---:|---:|---:|---:|---:|
| Plain | 46,351,872 | 46,476,288 | +124,416 | 13.99835 | 14.26560 |
| Dense | 48,301,056 | 48,484,864 | +183,808 | 14.09900 | 14.00180 |

This is warm `--version` subprocess wall time on one Windows host, including process creation;
concurrent host load, AV and cold-start behavior are not isolated. There is no demonstrated size or
startup win from extraction. The checkpoint's failed unit baseline remains a failure even though
its release-build steps and these measured artifacts passed.

Core source has changed after `c2477199...`; the earlier 3,049-test and release/application results
remain historical evidence for that pinned revision, not proof for this new cancellation change.
No actual TUI cancellation, live bot, model-sharing reachability or induced-crash result is implied.
The CompletedTurn carrier correction does not cover every asynchronous Memory path: association
flush still creates a fresh token, and capture's initial snapshot preparation is separate. The
blocking bridge is still convention-restricted rather than worker-only type-sealed. These remain
follow-ups; the new observer tests use local channels/ownership tokens, not an injected actor panic
or a real hung provider.

### Current release runtime fault matrix

`%LOCALAPPDATA%/Temp/mnemos-current-runtime-faults-eqf6q8p3/report.json` records the actual
protocol-fault CLI matrix on both `807a7e47...` Core releases. Each passes all eleven modes and
97 commands, with settled owned Jobs and empty cleanup-failure lists:

| Core | Nested report below `%LOCALAPPDATA%/Temp/` |
|---|---|
| Plain | `aizen-memory-runtime-faults-36ogzsb5/report.json` |
| Dense | `aizen-memory-runtime-faults-o5qqffw4/report.json` |

The retained reviewed fixture executable has SHA-256
`8545819e9b95f161a1b5e50f8909e5fb9303e22d708e7d945e7c2e358354428b`.
Modes are semantic conflict, owned pipe-holding child, ambiguous effects, recovery-required, crash,
mismatched response ID, stdout noise, truncated JSON, oversized frame, stderr flood and stall.
The settled no-effect controls retire their transaction marker. Ambiguous/recovery/transport-failure
cases retain the original operation receipt and prevent replay, disable and uninstall; they do not
turn a clean process exit into proof of no effect. The wrapper verifies both Core hashes, live source,
fixture and harness hashes before/after. It does not inject an actor panic, observer cancellation,
host crash or power loss, and Windows guarded mode is not kernel filesystem/network confinement.

### Current no-provider and independent custom-provider Agent matrix

`%LOCALAPPDATA%/Temp/mnemos-current-custom-agent-16uqzyok/report.json` records a fresh independent
example-provider build and actual Agent runs on both `807a7e47...` Core releases. The example's
three unit tests and offline/locked MSVC build pass under an allowlisted synthetic environment;
its Cargo manifest/lock/source hashes remain unchanged. The resulting executable is 931,840 bytes,
SHA-256 `bd4d2ec0cb638c7f90f2e1466caf11dedcfd8b6fbf6ca495fe90fa8c08700d5b`.
The application harness does not build Core or Mnemos.

Each Core run passes 28 CLI commands, eleven Agent phases and thirty scripted loopback model
requests. Nested reports below the wrapper's `temp/` are `aizen-memory-agent-3kg99lyh/report.json`
(plain) and `aizen-memory-agent-kj1ayip5/report.json` (dense). Both report settled Jobs, no model or
cleanup errors, unchanged executable hashes and preserved provider data/legacy/session files.

The phases exercise clean no-provider, legacy-but-no-provider, installed-inactive, eligible-unselected,
selected-active, disabled, package-disabled and disabled/uninstalled resume. Soul, Persona, Self-memory
insight and Skill index canaries appear in all expected Core prompt lanes. Model-requested `skill_load`
and `session_recall` actually return fixture data without a Memory provider. Inactive phases advertise
no Memory tools and spawn only Core; installation/eligibility/selection do not initialize provider data.
Active custom-provider recall and `memory_search` return the stored record inside lower-trust wrappers.
After disable/uninstall, resume removes only the sealed external recall prefix, retaining the original
user-authored Memory mention and ordinary assistant history without rewriting saved source sessions.
Uninstall preserves provider data; legacy canaries are neither adopted nor disclosed.

This independent example is not Mnemos algorithm parity evidence. Its `build_context` is empty, so
positive standing-context injection is not covered here. Static identity reads and actual Skill/session
tools do not exercise Persona reflection, Skill auto-learning, retained REPL/TUI or live bot transport.
The source/hash-pinned local example is fixture trust, not publisher authentication or kernel isolation.

### Native fixture bench parity — current standalone artifacts

`%LOCALAPPDATA%/Temp/mnemos-native-bench-parity-283_oa22/report.json` records five native CLI benches
on each independently built plain/dense Mnemos artifact: recall with fuzzy/hash-hybrid, evolution,
profile/tier hints, dialectic and health. All ten commands pass with settled single-process Jobs,
unchanged standalone source/artifact hashes and no cleanup failures. Each stdout matches the retained
successful `e3bd8c9` checkpoint bench output after newline normalization. The nine fixture files and
saved metric baseline match exact Git checkpoint bytes; no baseline is rewritten.

The first scratch verifier attempt failed before running a bench because it compared the current
Core working-tree `tier-hints.jsonl` bytes rather than the checkpoint: that checkout has 14 CRLFs,
whereas Git and the standalone copy have LF. A byte diagnostic confirmed 1,791 versus 1,777 bytes,
identical after newline normalization, and standalone bytes exactly equal Git's checkpoint object.
The corrected verifier reads all fixture objects from `e3bd8c9` directly; this is not a changed oracle.

The dense native recall bench emits its expected missing-model/hash-fallback diagnostic on stderr;
only stdout is compared, and `hash-256` is explicitly reported. This is preserved fixture mechanics,
not semantic quality, full store/scoping/RPC parity or model-assisted learning proof. The evolution
bench is a synthetic in-memory reinforcement simulation, not a durable multi-session acceptance run.

### Native management command/store parity — bounded checkpoint comparison

`%LOCALAPPDATA%/Temp/mnemos-native-store-parity-96fbmxfn/report.json` records 37 paired native
command/state comparisons per plain/dense configuration (74 comparisons, 148 actual process runs).
Each compares the retained `e3bd8c9` Core executable with the corresponding independently built
Mnemos executable under fresh synthetic roots. All commands have settled single-process Jobs,
no cleanup failures, unchanged executable hashes and unchanged standalone source inventory.
Report SHA-256: `147b0be4c88b131b0b73477bf03813a31870911d213f4c458d9cccbf63b2d8cf`.

The 29-step management sequence covers imported frontmatter/body reads, Vietnamese slug creation,
duplicate-add refusal, display-name/type/body edits, invalid-type refusal, supersession/history,
both-direction revive and already-live refusal, recoverable archive, restore collision refusal and
explicit renamed restore, review promotion/drop/clear, STYLE, profile and dialectic JSON, and missing
record refusal. Review promotion retains the fixture's place/device/scope/subpath/supersedes tags.
Edit and archive/restore retain the imported unknown frontmatter field and multiline body.
The eight-step consolidation sequence checks a nonmutating dry run, one actual duplicate retirement
and survivor reinforcement, the graveyard, a no-op repeat and explicit revival. The survivor's
reinforced/session counters become 3/3 while earned confirmations remain 4.

Each paired step compares stdout, stderr and normalized record/audit content, not only an
aggregate metric. This first verifier normalizes line endings/outer CLI whitespace, the exact fixture
data-root literal, the native main's `Error:` versus `error:` label, `lastSession` and audit
`session_id`/`ts` within a whole-scenario clock window. It records Mnemos lock/revision/provenance
metadata separately. Independent review confirmed the 74 retained content comparisons and settled
single-process Jobs, but found assertion gaps: old clock fields could be rewritten without detection,
metadata checks did not enforce exact writer effects or presence, audit equality was not type-strict,
and outside-data writes were not inventoried. No actual product mismatch was demonstrated in that
run. Its pass is therefore normalized-content compatibility evidence, not strict no-mutation,
metadata-correctness or no-outside-side-effect proof. The strengthened verifier addresses these gaps
in a separate fresh run; the original report remains unchanged.

Clock normalization does **not** prove that native `--session-id` controls every command: these copied
native verbs still call the clock-based `default_session_id` directly.

The initial scratch attempt failed before executing commands because the state reader incorrectly
required frontmatter on `STYLE.md`. The next attempt stopped on the native main's error-label casing;
its error body and state matched. Both reports remain at `mnemos-native-store-parity-9qp7ymtz` and
`mnemos-native-store-parity-vjrdxtrt`; neither is counted as passing. No product code was changed.

This is native management-subset parity, not full engine equivalence. Ordinary list/search, frozen,
compact, learn and doctor were excluded because checkpoint paths may discover actual machine identity;
no such device files were created in these fixtures. Reconcile model calls, useful-use confirmation,
affinity, full visibility/scoping, concurrency and crash/power-loss behavior remain unverified here.
Dense executables retain default lexical settings; this adds no semantic-model quality claim.
The checkpoint's whole-unit failure remains recorded and is not made green by these command runs.

### Strengthened native subset verifier — fresh passing rerun

`%LOCALAPPDATA%/Temp/mnemos-native-store-parity-lajiihqu/report.json` repeats all 74 paired
comparisons (148 executions) with tightened assertions. It passes, including negative oracle
self-tests, exact source/artifact pins, settled single-process Jobs and empty cleanup failures.
Report SHA-256: `45584984a6ea38dc22a002ea4e080582b84ae570beda551b0fe90c113c39879d`.
Verifier SHA-256: `b22e7a1bdcbf15ffcb7083dfac3b4daed25103e788beb25a2433572dcf48b3b1`.
Core source remains `807a7e47...`; neither product's source changed for this follow-up.

- Failed commands and consolidation dry runs preserve raw record/audit/revision/provenance bytes;
  validated empty lock-file creation is a separate, permitted adapter effect.
- Existing audit bytes remain an exact prefix. Only newly appended audit clocks are accepted within
  that individual command's start/end window. Existing record `lastSession` values cannot change
  except the two fixture writes that intentionally generate them; their new clocks use the same
  per-command check. Raw data cannot change between commands.
- Revisions must match explicit create/increment effects for each fixture command, with no missing,
  extra, decreased or unexpected read/failure increments. Archive provenance must exist at archive,
  match the reserved revision and exact original bytes, and remain immutable after renamed restore.
  Revision fields reject booleans. These are fixture expectations, not full CAS/concurrency proof.
- Audit parsing rejects duplicate keys and nonfinite numbers. Canonical JSON preserves numeric/bool
  types and event ordering; JSON key order and byte formatting are deliberately not compared.
  Raw stdout/stderr must decode as strict UTF-8 before the documented CLI normalization.
- The common synthetic workspace is inventoried and immutable after each command, preventing it
  from carrying writes between sides. Each side's other synthetic roots permit only runner logs and
  known empty checkpoint entry-lock files; existing outside-data bytes/directories cannot change.

These checks observe owned fixture files, not OS-wide reads/network traffic. They do not prove
kernel confinement, absence of machine-identity reads or zero transient/outside-fixture effects.
All original exclusions still apply; this remains differential compatibility with a checkpoint,
not an independent proof that behavior shared by both implementations is correct.

### Native reconciliation parity — scripted checkpoint completion, offline Mnemos reply

`%LOCALAPPDATA%/Temp/mnemos-reconcile-parity-zxizgyko/report.json` passes 14 paired scenarios per
plain/dense variant: 28 comparisons, 84 settled single-process executions including offline prompt
preparation, and 26 scripted loopback completions to the historical checkpoint only. The empty-store
cases make no model request. Mnemos reads explicit native `--reply` files; its hosted `host_llm` guard
is unchanged. Report SHA-256: `fa608cea7a054474778e858ad721a4bfc4c1ce5052480fe0976e9ee61bdb830f`.
Verifier SHA-256: `394f5f7e34c24bf7f1d643c8f54128a03fdb279841ab4aaa4d70a3b03c518135`.

The fixture compares Mnemos's prepared system/input prompts exactly with the checkpoint's actual
no-tool HTTP request, then compares normalized stdout/stderr and record/audit state after applying the
same completion. Cases cover a symmetric live-duplicate cycle with one survivor, queued-duplicate
retirement to `.discarded/`, low-confidence duplicate retention, protected candidates, same-day repeat
confirmation, refinement with byte-exact archived old wording, protected-target refinement refusal,
contradiction with inherited place/device placement, low-confidence contradiction refusal, unsure,
malformed output, duplicate/out-of-range verdict indices, dry-run and empty store. Explicit assertions
check writer revisions, archive provenance, confirmations, preserved unknown fields on confirm/refine,
unchanged protected records, and immutable original target bytes for forward supersession.

New clocks use individual command windows; prior audit bytes cannot be rewritten. Audit parsing is
duplicate-key/nonfinite rejecting and type-preserving. Raw output must be UTF-8. Workspace and other
owned fixture roots are inventoried, executable/source/helper/report hashes remain pinned, and all Jobs
and the loopback server settle without reported cleanup/model errors. Actual source/build independence
is inherited from the pinned standalone build evidence, not established by executable hashes alone.

One checkpoint behavior is deliberately exposed rather than hidden: native reconciliation **dry-run
appends audit rows**, although both implementations print “nothing was written.” Record bytes and
CAS/provenance remain unchanged. This is shared historical behavior, not a new extraction mismatch;
it must not be described as a completely write-free dry-run. The repeated protected-candidate case
checks once-per-day `confirm_use` mechanics through reconcile only, not the Core `confirm_use` RPC or
load-bearing handle attribution. This run proves neither reachable plugin model learning nor model
semantic quality, full scoping/affinity, concurrency, crash/power-loss or kernel confinement.

### Compiled-owner boundary — bounded source audit

A read-only audit of the live modified/untracked checkout found no direct legacy persistent-store
bypass in the inspected compiled entry paths. `src/main.rs` declares no `memory` module, and
`src/bench/mod.rs` does not attach the dormant Memory bench. The traced CLI, REPL, hostbot and
cron/capture routes reach `plugin::runtime` through manual commands, snapshots or completed-turn
events. Zone migration touches Skills, codebase cache and ordinary sessions, not inactive Memory;
Persona stem migration no longer rewrites a provider graph. Legacy entries/frozen/graph helpers
inspected in `core/config.rs` are test-only. Generic codebase embedding caches and bounded ephemeral
session context are intentional Core ownership, not a legacy Memory fallback.

This audit used the original live checkout, not the delegated reader's initial HEAD-only worktree,
which was discarded as irrelevant. It is a targeted reachability review, not exhaustive filesystem
proof, binary attestation or runtime coverage. Dormant engine source removal and broader parity are
still open. No source or real user data was changed by the audit.

### Latest management application and standalone verification — 2026-10-31

This is the newest matched application/standalone evidence in this record. It supersedes neither
older failures nor the open acceptance gates below; those remain preserved with their original
report paths and status.

The actual Core-to-Mnemos application wrapper passed:

`%LOCALAPPDATA%/Temp/mnemos-management-app-281lms0g/report.json`

The wrapper built fresh Windows MSVC debug artifacts in a disposable fixture root, then ran the
three isolated application harnesses below. No real HOME, installation, adoption, publication or
remote model was used:

| Artifact or harness | Result / artifact | Report below `%LOCALAPPDATA%/Temp/` |
| --- | --- | --- |
| Core debug executable | PASS; 69,566,464 bytes; SHA-256 `633acf67fc0fb2dea34d8c7a24480a8c139acb5f9a8f72d990658bb7bbadda16` | `mnemos-management-app-281lms0g/aizen.exe` |
| Mnemos debug executable | PASS; 10,626,560 bytes; SHA-256 `301d8054975877d6def0a0e5db5f75e9369d724f28f9bba34e98ee8317487cde` | `mnemos-management-app-281lms0g/aizen-mnemos.exe` |
| Lifecycle/management | PASS: 94 cases | `mnemos-management-app-281lms0g/temp/aizen-memory-lifecycle-x2n3ess6/report.json` |
| Agent tools/context/recall | PASS: 11 cases | `mnemos-management-app-281lms0g/temp/aizen-mnemos-agent-o_5dwitx/report.json` |
| Completed-turn learning and same-turn model route | PASS: 26 cases | `mnemos-management-app-281lms0g/temp/aizen-mnemos-learning-wr8oiqa/report.json` |

The wrapper report records exit 0 for both builds and all three harnesses, with nested report SHA-256
values `43865d06ad86ee6fb0b786d3553f41e1e7048dfa91cf6bc83e40873dddaa25ba`,
`c7bf5ab1f6a869d5fd1850cb9564ab76a51244a7623aa2850121eeb204193763` and
`6f4ae1bd57004970132c6ffcf4e16259b8e1e2e4e10ec13cd6858f372dbef398`, respectively. The wrapper's
support hash is `e5338a00cd24a732388ccda2faab548c3bef23983b7283d1fd38eec0136153c2`. Every nested
case reports `job_settled:true`; cleanup-failure collections are empty. The wrapper's before/after
source pins stayed unchanged: Core `05f01da048b5951bf012b79052393f593efff999c6827923b82e52e11e94d162`
and Mnemos `75bd301c238e3ab81e66b329a2b0ceaf4cd8b91dd3dfdce89d8aad72f6139e8e`.

The lifecycle run covers installation/eligibility/selection separation, CRUD, revisions, native
and RPC CAS, archive/recovery, review, management filters, offline reconciliation, disable/enable,
uninstall preservation and corrupt-package recovery. The Agent run covers actual model-requested
Memory save/search, standing User context, Project recall, unrelated-recall rejection, and removal
of Memory tools/context while preserving provider data after disable. The learning run covers
separate consent, repeated-text identity, clarification/budget/passive gates, offline learning,
same-turn model success, HTTP failure and malformed responses. These are Windows fixture mechanics;
they are not semantic-quality or real-provider/API evidence.

The newest independent Mnemos verification ladder also passed:

`%LOCALAPPDATA%/Temp/mnemos-retrieval-adapter-zapr8st4/report.json`

- Source fingerprint before/after: `75bd301c238e3ab81e66b329a2b0ceaf4cd8b91dd3dfdce89d8aad72f6139e8e`; `source_unchanged:true`.
- Offline locked formatting, all-target tests and strict default Clippy passed.
- Dense RPC library tests: **78 passed**, 0 failed.
- Dense initialized RPC subprocess contracts: **18 passed**, 0 failed.
- Strict dense all-target Clippy passed with `-D warnings`.

The standalone run is a fresh source/test result, not a Core dependency or a release/publication
claim. It uses lexical hosted retrieval when no explicit semantic model roots/weights are supplied;
a dense Cargo feature alone is not semantic-retrieval evidence. Its limitations remain: Windows
only, disposable fixtures, loopback scripted model, no retained TUI/live bot coverage, no Linux or
macOS runtime result, no power-loss proof, and guarded execution is not kernel filesystem/network
confinement. The actual application evidence does not close rendered-evidence/use attribution,
revocation/cancellation/timeout application cases, configured catalog/HTTPS acquisition, model
provisioning, health/HUD wiring or physical dormant-Core-source removal.

## Completion checklist — 2026-10-31

This is the current completion inventory, not certification of the newly edited source. Earlier
artifact/source hashes above remain historical. No real installation, adoption or publication is
part of this work.

| Compatibility area | Current implementation / remaining acceptance |
| --- | --- |
| No-provider coding; Skills/Persona/Soul/session history | Fresh plain/dense actual-Agent custom-provider matrix passes, including no-provider and disabled/uninstalled/resume paths; this does not prove positive identity auto-learning. |
| Native record/archive/review/consolidation/reconcile | Previous bounded differential passed. Five native session/provenance subprocess contracts now pass in the fresh suite below. Reconcile dry-run wording now acknowledges audit writes; its differential needs an explicit expectation update. |
| Hosted management | Dimension/category/narrowing scope and superseded inventory now have unit and initialized subprocess coverage. Human management authority, scope moves and remaining former commands still block parity. |
| Standing/recall/scoped visibility | Existing adapter and frozen boundary retained; actual rendered exposure/evidence, smaller-budget packing and complete scope matrix still open. |
| Useful-use / echo suppression | Cross-process rendered-evidence/handle contract and ConfirmUse still open; exposure alone is not useful use. |
| Graph / Skill affinity / associations | Typed affinity, exact originating-turn ownership, cancellation and matching entry drains are implemented. Three initialized affinity subprocess tests pass; real Core Skill-ordering and the full surface matrix remain open. |
| Automatic offline learning | Fresh actual-Agent fixtures pass on both Core variants: no consent/incomplete turns do not learn; separate repeated text occurrences have distinct receipts; assistant-only facts are not mined. |
| Exact originating-turn model learning | Seven wire/initialized subprocess contracts and fresh Core-to-HTTP-to-Mnemos success/error/gate fixtures pass as recorded below. Both saved consents remain separately required and default off; the internal prepared-job marker is not consent. Revocation/cancel/timeout application cases remain open. |
| Frozen/profile/settings/local models | Preserve existing engine behavior; hosted settings, budget and independent model-provisioning parity remain open. |
| Adoption | Existing explicit Windows fixture evidence retained; no real legacy store touched. |
| Acquisition | Local directory/ZIP implemented; configured catalog/pinned HTTPS still open. |
| Physical source cleanup | **Not done**: functional and preservation gates must pass before audited removal. |

Removal inventory: 38 dormant `src/memory/**` files map to the existing standalone engine, except
SessionMem which already belongs to `core/session_mem.rs`; its old file is only a transitional alias.
Detached `src/bench/brain.rs` / `metrics.rs` and the nine Memory-only JSON/JSONL datasets map to
Mnemos benchmarks. Keep Core visual-design/workflow fixtures, loop/task/session benches,
`src/retrieval/**`, `core/memory_context.rs`, neutral plugin code, Skills and Persona. Before removal,
read each exact target and compare unique algorithms/tests/fixture bytes to its retained owner and
checkpoint `e3bd8c9`. The old `cli-memory` directory name still hosts valid Core caches, not permission
to delete user data. The obsolete dense-memory CI job and baseline tooling need explicit updates.

### Fresh source verification — completion follow-up

`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-6sgbpoen/report.json` passes with unchanged
Core fingerprint `9e760b0461488b1fe270d1c08333bb4fa4fd3c0526ab4826be44a857c164a497`:
75 plugin, 4 one-shot, 6 cancellation, 10 REPL-turn and 69 hostbot tests; no-default and dense
checks; default/dense bin-and-tests Clippy with `-D warnings`. This is focused coverage, not a fresh
whole-unit or application run. The initial compile failure (missing `graph_expand_enabled`) remains
at `mnemos-learning-cancel-h8_bw5n2`; the host-only graph policy getter fixed it without linking the
old Memory engine.

`%LOCALAPPDATA%/Temp/mnemos-retrieval-adapter-_t4cbooo/report.json` passes format, default
all-target tests (423 library + 17 adoption + 3 affinity + 5 native-session + 7 prepared-learning +
15 RPC = 470 tests), 54 dense RPC library tests, 15 dense RPC subprocess tests, and default/dense
all-target Clippy with `-D warnings`. Fingerprints are unchanged within that run. Dense without
explicit model roots remains lexical; this does not establish semantic-model quality.

Earlier failed runs remain evidence:

- `mnemos-retrieval-adapter-yca7q8d2`: one fixture byte-inventory failure from the exact empty
  store-wide lock created on first successful read. Establishing that exact lock before the baseline
  preserves strict fact/graph/receipt comparisons; no broad lock exclusions were added.
- `mnemos-retrieval-adapter-kyi4w6vd`: promotion persisted canonical `user-explicit`, not legacy
  input alias `explicit`. Only the test expectation changed; session and engine semantics did not.
- `mnemos-retrieval-adapter-n2yzafv1`: a stale RPC denial test still required unsupported filters
  that are now implemented. Supported narrowing/inventory cases now assert typed item arrays; scope
  moves and unsealed manual confirmation remain denied.
- `mnemos-retrieval-adapter-2_2brv4g`: all tests/dense tests passed, then strict Clippy rejected two
  redundant `map_or(true, ...)` calls. They now use `is_none_or`; no lint suppression was added.

Standalone packaging tests also ran independently: 17 passed, 1 skipped.
`%LOCALAPPDATA%/Temp/mnemos-independent-build-bctj1nkb/report.json` now passes a fresh
allowlisted source relocation, offline dependency-tree check, plain/dense release builds and packages,
and default all-target strict Clippy. The relocated build has no Core source/path dependency;
original and relocated inventories stayed unchanged. Its strict environment uses synthetic runtime
roots, explicit MSVC inputs and `+crt-static`, with no inherited credentials/proxies.

| Standalone artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| Plain executable | 5,458,944 | `516fadf66c1c88ef2fd46024544f7a1f9ef85631cbe6c6ac0c32d2c4e558a5e3` |
| Dense executable | 7,443,968 | `3a4904f0d51fa668398bad7829da98dc7ae5427f2a88c59f57d35e986e0d386d` |

Plain package SHA-256: `b2b65d32e1e9b21f27c610dba42c5ece84c671160ff7cb7c37e26bca323ab102`.
Dense package SHA-256: `c3f92b67b090cdb51dcff4a498f034a827545123803b2e15b94162aa20c0757e`.
These are local fixture artifacts, not installed/published releases. This run did not repeat DLL-import
inspection, use real embedding weights or establish application parity. Fresh Core release and actual
Core-to-model-to-Mnemos application verification are separate and not counted here.

### Fresh Core artifacts and actual application matrix

`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-6x4dkyv5/report.json` passes offline/locked
plain and dense release builds at unchanged Core fingerprint `9e760b0461488b1fe270d1c08333bb4fa4fd3c0526ab4826be44a857c164a497`.

| Core fixture artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| Plain executable | 46,546,432 | `6f7068a981eae30b3732425c15c7fcb923f4f4fa721781052039d11d1c4a0741` |
| Dense executable | 48,555,520 | `4754067fd6356d110766dbfd32456e8f2777ab0b9e34fa7b75e63be9b5cbdbb6` |

`%LOCALAPPDATA%/Temp/mnemos-current-custom-agent-ag4487ha/report.json` passes three
independent example-provider tests and actual Agent fixtures for both Core artifacts: 28 commands,
11 Agent phases and 30 scripted HTTP requests per artifact. Assertions cover provider absence,
selection, disable/uninstall, resumed history, Core Skill load/Persona/Soul/session recall, actual
Memory search and recall invalidation, preserved provider/legacy data and unchanged source/binary
pins. The example is not Mnemos; positive standing context and identity auto-learning are not proved.

`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-app-nuofntpl/report.json` passes the lifecycle,
Mnemos Agent and completed-turn learning harnesses for both Core artifacts against the independent
plain Mnemos artifact above: 58 + 11 + 26 = 95 commands per variant, 190 in total. All recorded
owned Jobs settle and cleanup/model-error arrays are empty. Both products' pinned sources and
artifacts remain unchanged. The dense Core cases use the same **plain** Mnemos executable; they do
not prove semantic dense retrieval or execute the standalone dense binary.

The learning run proves one successful initialized Core -> captured loopback model -> Mnemos
completion per Core variant. At each of the three auxiliary HTTP requests, only Core remains in its
owned Job, the prepare receipt is done, and the provider transaction marker is absent. The request
has exactly system/user messages, no tools, and retains original endpoint/auth/model, high effort
and the captured 777-token `max_completion_tokens` policy even after the synthetic current config
is changed. HTTP 503 and malformed OpenAI-response bodies fail learning without fact/audit changes,
CompleteJob dispatch or offline fallback; the coding result still succeeds. A passive gate produces
no auxiliary request and completes through the separately journaled offline path. Saved learning
and same-turn-model grants remain distinct. This is scripted application mechanics, not model quality.
Malformed secretary-content JSON, revocation, cancellation and timeout application cases remain open.

The first current application attempt at `mnemos-learning-cancel-app-am7x7nfd` failed before
any HTTP request: the Agent fixture's old package manifest omitted the `host_llm` capability advertised
by the new executable. The fixture now declares that surface and permission while leaving saved
learning/model-sharing consent absent. Runtime integrity/consent checks were not weakened; the
failed root and recovery evidence were retained, and rerun used new owned roots.

`%LOCALAPPDATA%/Temp/aizen-mnemos-startup-8s8jv_yx/report.json` records 20 warm `--version`
samples per fresh Core artifact: plain median 13.28745 ms; dense 13.22005 ms. The two artifacts differ
by 2,009,088 bytes. This compares plain versus dense, **not** checkpoint versus extracted Core.
It includes Python/CreateProcess overhead and uncontrolled load/outliers; no cold-start improvement,
extraction size win or provider-ready latency is claimed. Version runs create no host runtime state.

The fresh whole Core binary unit suite also passes at the same `9e760b0461488b1fe270d1c08333bb4fa4fd3c0526ab4826be44a857c164a497` fingerprint:
`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-qg82xt89/report.json` records unchanged source,
3,062 passed, 0 failed and 2 ignored. The test summary reports 907.67 seconds; the verifier step
including Cargo reports 919.38 seconds. This is `--bin aizen -- --test-threads=1`, not unsafe/live
integration targets or execution of the two ignored tests. No actual installation, adoption or
publication took place.

### Preservation mapping — read-only audit, not removal approval

The bounded live-source audit mapped all 38 dormant Core Memory files: 37 have named counterparts
in Mnemos, while SessionMem's implementation and six tests remain in `core/session_mem.rs`.
Of the 38 Core files, 36 match checkpoint Git blobs after newline normalization; exceptions are the
SessionMem alias and a dormant recall-prefix registration in `memory/mod.rs`. Both detached bench
files match their checkpoint and standalone algorithms/tests. No entirely missing Memory
algorithm/test was found in this inventory; reachability and functional equivalence are separate.

Source test annotations are 315 in dormant Core Memory, 326 in sibling Memory, plus six retained
Core SessionMem tests. These are source counts, not executed-test counts. The two identity/Skill
secretary tests and turn-authoring guard were deliberately split into Core identity contracts rather
than delegated to Mnemos. Keep Core neutral helpers, identity secretary/contracts, SessionMem,
all `src/retrieval/**`, Skills/Persona/Soul/session tests and non-Memory fixtures outside removal.

All nine Memory datasets map to the sibling benchmarks and checkpoint normalized blobs. Eight are
byte-identical between live checkouts; `tier-hints.jsonl` has 14 CRLFs in Core versus LF in Mnemos
(1,791 versus 1,777 bytes), with the same 14 JSONL records. Mnemos bytes match the exact checkpoint
blob, consistent with the Git-object bench verifier above. Do not claim nine byte-identical live
copies or rewrite the oracle to hide the newline difference.

Two additional functional gaps were confirmed: Mnemos's copied downloader source is not compiled
or reachable, and production automatic health collection/HUD counts are not wired (`record_health`
has no production caller; old counters are test-only). Merely retaining those algorithms/tests is
not preserved behavior. These join the hosted-management, rendered-evidence/use, local-model and
acquisition blockers. Final removal still needs exact target hashes after functional acceptance;
**no dormant source is removed by this audit**.

## Human authority and hosted management follow-up

The authority-only batch passed at Core fingerprint
`a5a8e1a0df74d7bc4675c2258a62d1f3115be6e2c5fb648102faa65a1f2779fb`
(`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-nqvd7tk3/report.json`): 78 plugin, four
oneshot, six cancellation, ten REPL-turn and 69 hostbot tests; no-default/dense checks and
strict default/dense Clippy passed. The three new authority regressions actually ran: forged
Management contexts are rejected by both bridges before storage, delegated/native-scoped callers
cannot inherit human authority, and model/background callers never receive optional Management.
This was focused verification, not a fresh whole-Core or release run.

A subsequent Mnemos batch implements hosted review/list/promote/drop/clear, revive, supersede,
restore, confirmed archive-only purge, compact, local consolidation, doctor, neighbors, frozen
rebuild and local model inventory. It reuses shared nonprinting native bodies under the existing
RPC operation lock, not reentrant Engine write facades. Human archive/history inspection can see
foreign/orphan records; ordinary model visibility remains unchanged. No JSON `admin`/`manual`
field or provider setting grants authority. Model inventory explicitly reports
`weights_loaded:false`; model download remains an honest offline unsupported operation.

Fresh standalone source fingerprint:
`62ef66e4e0d6290109793c2f49eb9636be59c8fe65fe45b5625e5dde0a934bd0`.
Report: `%LOCALAPPDATA%/Temp/mnemos-retrieval-adapter-zdl4yuit/report.json`.
The fingerprint stayed unchanged throughout:

- Format check passed.
- Default all-target tests: 440 library + 17 adoption + three affinity + five native-session +
  seven prepared-job + 17 initialized RPC subprocess tests = **489 passed**.
- Dense RPC library tests: **71 passed**; dense initialized RPC subprocess tests: **17 passed**.
- Default and dense all-target Clippy passed with `-D warnings`.
- Separate isolated Python packaging suite: 18 cases, **17 passed, one skipped**. This checks
  prebuilt packaging, not executable trust or cross-platform runtime behavior.

New regressions cover strict nested schemas, human-only authority before storage, exact queue
handles, collision allocation, preserved foreign placement, explicit session identity,
review-discard byte preservation, revision/replay, both supersession directions, claim-only revive
with an exhausted unchanged target revision, restore collisions, confirmed purge, compact's first
preview and subsequent recoverable archive, read-only diagnostics/model inventory, consolidation
survivor/session identity, and frozen packing within a smaller host budget. An actual active-write /
next-file cleanup failure returns RecoveryRequired and cannot reexecute; injected late failures
retain barriers for review/revive/compact/frozen. Native claim-read failure is now propagated rather
than silently reported as a successful revive. Audits and graph cleanup retain their documented
best-effort semantics; these tests do not turn them into durable transactional guarantees.

The first attempt is retained as **FAIL** at
`%LOCALAPPDATA%/Temp/mnemos-retrieval-adapter-vkt6li1j/report.json`: compilation succeeded,
436 library tests passed and one new assertion failed. That assertion wrongly required no `updated`
date after supersede had already stamped it. The corrected regression asserts that revive preserves
the pre-revive date; product timestamp behavior was not changed to hide the failed test.

This section certifies standalone tests, including actual initialized Mnemos subprocesses, **not**
the subsequent Core CLI/manual integration or a release artifact. Hosted reconciliation,
rendered-evidence/use, session/health wiring, explicit model provisioning, acquisition and the
broader functional gates remain open. No old Core engine source has been removed.

## Open acceptance gates

- Actual multi-turn REPL/plain/retained candidate delivery remains **unverified**. The proposed
  `scripts/mnemos_tui_smoke.py` imports an absent owned ConPTY helper; source alone is not a run.
- Actual hostbot transport, cron, child task/workflow and capture surface matrices remain incomplete.
  Local handler/store tests are not live-bot evidence.
- Prepared same-turn-model initialized subprocess and actual Core-to-loopback-model-to-Mnemos
  success/error/gate fixtures now pass as recorded above. Actual revocation/cancel/timeout and
  malformed secretary-content cases remain open; earlier application reports were offline-only.
- Explicit backed-up adoption has 17 passing Windows fixture contracts after the reviewed lock-origin
  fix; fresh whole-package verification passed. Native management/consolidation-subset parity now
  passes the bounded comparison above; native reconciliation now passes the explicit-reply/loopback
  comparison. Broader completed-turn learning, visibility/scoping, useful-use handle attribution,
  affinity and the final removal audit remain incomplete. Core no longer declares the old Memory module, but dormant files still
  exist in the checkout; do not call source removal finished.
- Installation supports local directories and bounded ZIPs only. Configured catalog and pinned HTTPS
  source installation remain unimplemented; no public Mnemos registry/release or publisher trust is
  implied by the local package evidence.
- Outbox status is read-only inventory, not retry/prune/recovery UX. Capacity is bounded; no automatic
  receipt replay/deletion is implied. Physical recovery barriers are separate.
- The prior whole-Core binary unit suite and plain/dense release builds passed at source
  `c2477199...`; two ignored tests and unsafe/live integration targets remain unexecuted. Earlier
  Windows OS error 1450/checkpoint LSP failures remain recorded, not retroactively green. At the
  cancellation follow-up `807a7e47...`, focused tests and default/dense strict Clippy passed. The
  first whole-unit run failed eight tests in the restricted verifier environment; exact reruns and
  the fresh whole suite now pass after supplying the required Git/PowerShell test tools, with no
  product-source edit. Fresh plain/dense releases, lifecycle/Agent/offline-learning application runs
  and the eleven-mode runtime fault matrix also pass as detailed above. Unsafe/live integration
  targets and the two ignored tests remain unexecuted; earlier failures remain in their reports.
- Enabled-provider whole-CLI wall time is recorded above. The earlier dense failure is retained;
  corrected plain/dense releases now pass both lifecycle and latency application fixtures. Full
  store/scoping/RPC retrieval parity is still open. There is no isolated IPC, cold-start or
  semantic-quality verdict. Linux/macOS runtime is unverified locally.
- UUIDs/journals provide binding and fail-closed duplicate suppression, not exactly-once delivery,
  multi-file atomicity, induced host-crash coverage or power-loss proof.

No new commit/push, remote repository modification, release, package upload, real installation,
legacy adoption or real-data deletion was performed for this verification.

## Core-only legacy cleanup — 2026-11-02

The live branch was checked as `feat/mnemos-extraction` before this batch. No branch switch, commit,
push, real installation or data migration is part of this cleanup. Existing extraction and unrelated
working-tree changes remain in place.

Implemented in this batch:

- Removed the always-empty Memory/frozen fields and their unreachable summary branches from
  `features/zones.rs::LegacyZone`. Zone migration continues to own skills, codebase indexes and
  saved-session provenance only; it does not inspect or migrate provider/legacy records.
- Removed the permanently-zero provider graph rewrite counter and notice branch from Persona stem
  migration. The pre-rename mapping and Persona migration remain Core-owned; the legacy graph byte
  preservation assertion remains.
- Moved the three legacy test path constructors out of `core/config.rs` into the test-only
  `tests/legacy_memory.rs` fixture. Production Core retains the historical `cli-memory` codebase and
  embedding cache namespace, not persistent Memory store path APIs.
- Kept legacy record/frozen/graph canaries and added a Memory-only-zone regression: such a zone is not
  a Core migration candidate, creates no migration actions, and keeps its seeded bytes unchanged.
- Corrected the root module comment and zone command help to describe the actual provider/Core
  boundary rather than a built-in Memory engine.

Scoped rustfmt check and `git diff --check` passed. Fresh isolated Core verification also passed:
`%LOCALAPPDATA%/Temp/mnemos-learning-cancel-0znjkddd/report.json` records unchanged source
fingerprint `dcf415089afb5cbeb16f04aa075709d4923c786a70e558b016db86a33a117bcf`.

| Check | Result |
| --- | --- |
| Offline/locked no-default Core check | PASS |
| Offline/locked dense Core check | PASS |
| Zone cleanup/preservation tests, including the new Memory-only-zone case | 4 passed |
| Persona stem migration/preserved graph tests | 3 passed |
| Exact `where` legacy-record preservation test | 1 passed |
| Exact saved-session zone migration test | 1 passed |

These nine tests ran with fresh synthetic runtime roots, not the maintainer's real home/store.
The checks/tests still report four warnings for the unconnected rendered-evidence/ConfirmUse
protocol surface in `plugin/memory_api.rs`; this is not a warning-free or strict Clippy result.
No fresh whole-suite, release, or application-runtime result is claimed for this batch.

**Physical dormant-engine removal remains blocked.** All 38 files under `src/memory/`, both detached
Memory benchmark sources and the nine Memory datasets are still present. The read-only follow-up
found retained owners, but was unable to perform a fresh live/checkpoint/sibling hash comparison
under the delegated worktree's permissions. Historical preservation evidence above is not a fresh
hash attestation. Actual Core rendered evidence/exposure/useful-use, remaining functional parity and
the final preservation/removal gates are still open; this cleanup does not close them.

## Flexible provider configuration — 2026-11-02

This batch implements the provider-neutral `/config` → Memory hub and direct `aizen config memory`
entry point, without requiring chat credentials. It remains on `feat/mnemos-extraction`; no real-home
installation/configuration, adoption, migration, commit, push, release or upload is part of this work.
Existing extraction and unrelated WIP are preserved. This is not a verdict that extraction is done.

### Implemented behavior

- Offline local directory/ZIP install, eligibility, selection, global enable/disable, clear selection,
  consent editing/revocation and conservative uninstall share the CLI's manager/persistence APIs.
  Install/inspection/configuration do not run provider code. No provider is a valid configuration.
- Executable authorization, Windows guarded acknowledgement, declared network access, completed-turn
  sharing, same-turn model learning and global automatic-learning policy remain separate. Consent
  defaults to deny. Both surfaces disclose the executable path/hash separately from the manifest hash;
  hashes are integrity, not publisher trust. Windows guarded mode does not confine filesystem/network.
- Confirmed lifecycle actions save immediately; option/budget drafts save only on explicit confirmation.
  Generation/pin changes after package review cause a conflict. Stale dashboard saves preserve the
  latest manager-owned Memory state. Revoke still works for damaged payloads, disables selected Memory
  and preserves selection/package/options/data. Unselect preserves grants; uninstall preserves data.
- An optional manifest schema supports bounded boolean/integer/finite-number/string/enum options,
  strict defaults/types/ranges and at most 24 fields. Duplicate keys, unknown fields/overrides,
  control/bidi text and reserved host authority/identity keys are rejected. Schema-less packages keep
  working on the updated host; old strict hosts may reject schema-bearing packages. Options are not
  secrets, arbitrary JSON, executable arguments, environment forwarding or permission/model settings.
- Each package retains its own validated overrides and host budgets. Defaults remain standing 1024,
  recall response 1024, actual recall injection 300, automatic recall count 12 and response records 100.
  Runtime validates response count/allowance; rendering counts escaped text and wrapper overhead and
  emits complete hits only. Zero skips the corresponding automatic operation; settings apply at the
  next user-turn preparation, not midway through a turn.
- Mnemos packaging declares and RPC consumes `enable_fuzzy` (true), `recency_half_life_days` (30) and
  `frozen_core_max_tokens` (800, additionally host-capped). Core embedding selection remains separate.
  No semantic roots, remote catalog, separate learning model, credentials or model provisioning were
  added. Skills, Persona, Soul, sessions and codebase retrieval remain Core-owned.

### Latest isolated verification

All Cargo commands below used offline/locked dependencies, process-local Windows MSVC settings and
synthetic HOME/workspace roots. No global toolchain override, cache deletion or installed-binary
replacement was performed. Reports are local under `%LOCALAPPDATA%/Temp/`:

| Check | Result | Report directory |
| --- | --- | --- |
| Scoped Core/Mnemos/example rustfmt | PASS | `aizen-provider-config-o5j4ilp6` |
| Core plain/no-default/dense checks; Mnemos plain/dense all-target checks | PASS, five commands | `aizen-provider-config-clk2nwkh` |
| Core plugin/schema/manager/runtime/snapshot tests | 91 passed | `aizen-provider-config-jmgl5y3v` |
| Core config-UI tests | 29 passed | same |
| Core management CLI argument tests | 3 passed | same |
| Core Memory config persistence/stale-save regressions | 3 passed | same |
| Mnemos configuration / ordinary RPC / dense configuration tests | 5 / 60 / 5 passed | same |
| Mnemos prepared contracts | 17 passed | `aizen-provider-config-2nr22z9s` |
| Mnemos default all-target suite | 504 passed | same |
| Mnemos dense configuration tests after full run | 5 passed | same |
| Core, example provider and Mnemos debug builds | PASS | `aizen-provider-config-aaix2vhp` |
| Core plain/dense and Mnemos all-target Clippy | PASS, with Core warnings below | `aizen-provider-config-8k7pnokd` |
| Custom-provider actual CLI configuration smoke | PASS, 36 commands | `aizen-provider-config-smoke-hplz7ofd` |
| Actual Core Agent + Mnemos configuration smoke | PASS, 27 commands | `aizen-mnemos-agent-0nw5fzxs` |

The standalone packaging command `python -I -B ../aizen-mnemos/scripts/test_package.py` also passed:
18 cases, 17 passed and one skipped (0.704 seconds). Synthetic package payloads are never executed.

The 504 Mnemos tests comprise 454 library, 17 adoption, three affinity, five native-session, seven
prepared-learning and 18 initialized RPC subprocess cases. The new negative useful-use evidence,
real failed bookkeeping replacement and semantic reserve-before-failed-replacement regressions
actually ran. The dense result here is focused configuration coverage, not a whole dense suite.

The earlier whole-Core binary unit run remains PASS: **3,083 passed, two ignored**, 841.39 seconds,
in `aizen-provider-config-t25qlz9u/whole-core-unit-tests.log`, at Core fingerprint
`d6685184e904523c5cbee76267cfc942d2d7bf0009ba7c523de430f3e1f98482`.
That report's overall batch failed on Mnemos and must not be called wholly green. Subsequent Core
snapshot/conflict/render tests and executable-disclosure changes were verified by the focused latest
runs above, not by a fresh whole-Core suite. No fresh release-size/startup or release-build result is
claimed for this configuration batch.

Latest Core source fingerprint for check/focused/build/Clippy:
`8413609d321b07b55f4847f2dec8c9c2444811d2a5fbfdd81b375f0c33effbcb`.
Latest Mnemos fingerprint, unchanged across full tests and subsequent checks/build/Clippy:
`90ef3a15defde72980d6d8c40b3e16978da8cd39f098d059bbc259f6bc7baafa`.
The full Mnemos run preceded the final Core-only disclosure change, with Core fingerprint
`65a215ed1dbe27af3ab5b3f63dff83e8230f6fdf1c4bec8a593f8a48a6130d33`.
These verifier fingerprints cover `Cargo.toml`, `Cargo.lock` and `src/**/*.rs` / `src/**/*.md` only;
they exclude integration tests, examples, Python, build scripts and documentation. They are not
complete-checkout preservation attestations. Cargo's actual integration execution is recorded in logs.

### Actual application evidence and bounds

The custom-provider run covers install/default inspection without launch, typed option/host-budget
propagation into initialize, rejection without saved-byte changes, distinct executable/manifest
hash disclosures, disable/edit/re-enable, reset, unselect/grant preservation, another package's separate
configuration, damaged-payload revoke, conservative uninstall refusal and eventual verified uninstall.
Data canaries survive; no fallback provider is selected. All commands have settled owned Jobs and no
cleanup failures. This exercises real CLI processes, not interactive menu navigation.

`mnemos_agent_smoke.py --configuration` uses the standalone packager and actual binaries, with a
loopback scripted chat endpoint. It verifies actual `memory_save`/`memory_search`, standing User Memory,
independent long-tail Project recall, rejection of unrelated recall, custom settings accepted by RPC,
zero standing/recall/max-record/injection caps, tiny injection omitting whole hits, Mnemos's own frozen
cap zero removing standing while leaving recall, and reset restoring standing/recall. Zero records or
both automatic channels disabled launch no provider child; disabling Memory preserves data and removes
Memory tools/context. Active phases retain the complete Memory tool set. There are no model errors or
process cleanup failures. Learning/content-sharing/model-assistance grants are absent in this fixture.
It does not measure extraction/ranking quality, half-life quality, semantic quality or a live model.

Latest exercised executable SHA256 values (debug, not release):

- Core: `e262e6ca5341a4ef3ffce91000a859f9266a85c121d58051d61db7bfc134bd68`.
- Mnemos: `4d0a7e995b3bae4b9a50f029362f6a324bd06791ec5a0773c2b77c02911724d2`.
- Custom provider: `65272ed4fda17d538949aa59573622715a2073765f4422620058bb3d7a79fdae`.
- Custom configuration smoke: `50854afe42b7fb15c160dcd420b645387d662974ebc984b7715f4c983a3aa8a3`.
- Mnemos Agent smoke: `b1f8034b6587f79a067292bcd7d5dcd8cb176e86f9a531acfe9e49b5bfe82fc4`.
- Shared smoke support: `e5338a00cd24a732388ccda2faab548c3bef23983b7283d1fd38eec0136153c2`.
- Standalone packager: `0dc8f66cf61c0c47a1468da91c6455e2a6e2b14f40cc940a2a3764d0b394ef37`.

### Failures retained, not retroactively green

- The initial Core schema run failed four cases because derived `flatten` plus outer
  `deny_unknown_fields` rejected valid tagged settings. A custom decoder now extracts only the shared
  label/help fields and leaves strict type-specific decoding intact. Duplicate-key prevalidation is
  retained; roundtrip/invalid-schema regressions pass.
- `aizen-provider-config-t25qlz9u`: original Mnemos library run had 443 passed and nine failed. Prepared
  fixtures supplied useful-use handles despite `rendered_evidence: None`; the production preflight
  correctly rejected them before effects. Valid fixture replies now use no handles, and an explicit
  invalid-handle/no-evidence regression asserts rejection, no effects and exact replay preservation.
- `aizen-provider-config-7h96u_qe`: intermediate prepared run had 15 passed and two failed. Duplicate
  confirmation uses bookkeeping writes, not semantic revision reservation. The tests now exercise its
  actual entry-lock failure and deny-delete replacement boundary while preserving the prior revision.
  A separate semantic-update replacement failure verifies a reserved revision is never rolled back.
  Recovery barriers, intent receipts and replay assertions remain; production writers were not relaxed.
- `aizen-provider-config-6nzpcub7`: four integration targets passed; RPC had 17 passed and one failed
  because a hand-built completed-turn payload omitted canonical `rendered_evidence:null`. The fixture
  now includes it; digest comparison is unchanged. All 18 RPC cases pass in the latest full run.
- `aizen-provider-config-d7lvz2vc`: the new fixture initially failed compilation (E0282 from an
  unnecessary unconstrained `Ok`). The fixture now returns its lock path directly; no product change.
- `aizen-mnemos-agent-moustr03`: packaging correctly refused Cargo's hardlinked debug executable.
  The fixture now packages an ordinary owned byte copy, without weakening the packager's checks.
  Earlier wrong-artifact-path attempts are not current-binary evidence; latest hashes are above.

### Remaining limits

Core plain/dense Clippy exits zero but still reports four dead-code warnings for
`RenderedEvidence::validate_context`, `UseFeedback`, `ConfirmUse` and `ConfirmUseResult`. This is not
`-D warnings` or a warning-free result. The Core completed-turn path still does not supply rendered
Memory evidence; standalone fixture repairs do not establish actual exposure/useful-use attribution.

Interactive retained TUI navigation/rendering, menu cancellation/consent interaction, live hostbot/cron
and child-task/workflow surface matrices were not driven by this batch. CLI and unit tests do not prove
those surfaces. No denied terminal-automation workaround was used. Linux/macOS runtime, broad parity,
semantic roots and final preservation/removal gates remain open. All dormant Core engine files remain;
this configuration work does not authorize removing them or modifying real provider data.

## Simple Memory settings — 2026-11-02

This follow-up replaces the administration-first hub with five everyday entries: Provider, Learning,
Storage, Advanced and Back. Mnemos always has an accent-colored picker row with a text status, including
when absent. Local import checks its expected ID before package publication; no download or working
Obsidian adapter is implied. Core embedding has an independent Codebase configuration destination.

Learning displays effective state, not only the automatic-policy boolean. New sharing and optional
same-turn-model permissions remain separate default-deny decisions. Reviewed activation and Learning
changes are atomic; Off retains execution/sharing permissions, recall, selection and data while
invalidating old-generation work. Unrelated whole-config saves preserve the latest Memory policy and
provider state; the explicit CLI policy writer remains functional.

Storage is per package: the legacy automatic directory or a dedicated absolute custom directory.
Inspection/save are read-only with respect to provider data. Runtime resolves its admitted package
state, revalidates the path, and initializes only the approved location for an authorized operation.
Changing/resetting storage never moves, copies, adopts or deletes records; disable/uninstall preserve
both roots. Paths overlapping Core control/package/temp/model/runtime or another provider are refused.
Preflight does not protect against arbitrary same-user ancestor swaps; Windows guarded mode still
cannot kernel-confine filesystem/network access.

### Intermediate failures retained

`aizen-provider-config-cxx9uf_t` compiled successfully but its focused plugin suite had **96 passed,
one failed**. The new unsafe-path fixture used `PathBuf::join("../escape")` on a Windows verbatim
base, which normalized the traversal away before validation. The fixture now constructs the raw
spelling so the intended rejection is actually tested. Production path checks were not weakened.
The failed command stopped that chain before UI tests, full suites or application builds.
A subsequent intermediate run (`aizen-provider-config-bnopxegs`) passed 97 plugin, 35 UI, four CLI,
five persistence and 5/60/5 Mnemos configuration/RPC/dense-configuration tests. Its following whole-Core
run (`aizen-provider-config-ccojhekv`) was deliberately stopped for the independent storage fixes below;
it is not a whole-suite result.

Independent read-only review then found three concrete storage regressions: a linked worktree's valid
`.git` file rejected unrelated data roots; the workspace `.aizen` deny path accidentally overrode the
exact-default exception when running at HOME; and Windows disk/verbatim prefixes failed comparison
against absent external model/cache roots. Control paths are now deny paths rather than directory
grants, the narrow exact-default exception is preserved, and path comparison normalizes disk-prefix
identity. Focused regressions and the actual CLI smoke include these cases; results are recorded only
for completed reruns.

`aizen-provider-config-w_4nd7th` earlier passed plain/no-default/dense Core and plain/dense Mnemos
checks, but predates the final transaction/UI/model-root changes and is not proof for the final source.

### Verification bounds

All fresh verification uses offline/locked Cargo with process-local MSVC and synthetic HOME/workspace
roots. The owned-process support suite passed **six tests** in 1.460 seconds, including bounded cleanup
of its own timeout/overflow child trees. The two extended application scripts pass Python syntax
compilation; syntax alone is not application evidence. Completed final-source results:

| Check | Result | Report directory under `%LOCALAPPDATA%/Temp/` |
| --- | --- | --- |
| Scoped Core/Mnemos/example rustfmt | PASS | `aizen-provider-config-f8yudn8c` |
| Core plugin/schema/manager/runtime/snapshot/storage regressions | 100 passed | `aizen-provider-config-xij7se35` |
| Config UI / management CLI / Memory persistence | 35 / 4 / 5 passed | same |
| Sandbox, including disk/verbatim overlap | 49 passed | same |
| Mnemos configuration / RPC / dense configuration | 5 / 60 / 5 passed | same |
| Core, example provider and Mnemos debug builds | PASS, three commands | `aizen-provider-config-7xbx2o1d` |
| Core plain/no-default/dense and Mnemos plain/dense checks | PASS, five commands | `aizen-provider-config-3_p429jn` |
| Core plain/dense and Mnemos all-target Clippy | PASS, Core warnings retained | `aizen-provider-config-psg3mxm5` |
| Custom-provider actual CLI application | PASS, 81 commands | `aizen-provider-config-smoke-cvvcy0br` |
| Actual Mnemos Agent application with configuration checks | PASS, 36 commands / 13 loopback model requests | `aizen-mnemos-agent-n3bxagk6` |
| Full Core binary regression suite | 3,105 passed / 0 failed / 2 ignored | `aizen-provider-config-hu8903yq` |
| Full Mnemos all-target suite | 504 passed / 0 failed / 0 ignored | same |

The fresh full-suite chain exited zero: Core took 1,091.69 seconds including Cargo overhead, and
Mnemos took 46.75 seconds. Mnemos comprises 454 library tests and 50 integration tests across adoption,
affinity, native-session, prepared-learning and RPC contracts. Both full-suite source fingerprints
match the final-source matrix below and remained unchanged. These are serial Windows/MSVC runs using
default features; they are not full dense-suite or cross-platform results. The two Core ignored tests
remain excluded, not passed.

The custom-provider application smoke includes 41 expected nonzero rejection cases. It exercises actual
CLI policy Off/On, stale unrelated-save preservation, read-only storage inspection/save, custom-root
health and record operations, empty default/reset and preserved custom restore, independent provider
roots, linked-worktree `.git` metadata, the default from a HOME workspace, absent external model/cache
overlaps, damaged-package revocation, and conservative uninstall. All 81 owned Jobs settled, cleanup
failures are empty, and the Core/provider input binaries remained unchanged.

The Mnemos application smoke uses the actual Core Agent CLI and Mnemos process with a scripted loopback
model, not a live service. It verifies actual `memory_save` and `memory_search`, positive standing and
recall context after restoring custom storage, unrelated-recall rejection, the zero/tiny provider and
host budgets, and restored context after resetting options. All 36 commands exited zero and their
owned Jobs settled; model errors and cleanup failures are empty. Disable and uninstall preserve both
custom/default roots and their canaries. No completed-turn learning or host-model sharing permission
was granted, so this is not evidence for learning/extraction quality or a live-model verdict.

Both application reports use Core binary SHA256
`5c7c5d4c9e1d8c0a3a69ac548e682b8d03cd8b5d0ece2953e72cf8669e2e5074`.
The custom-provider input is
`65272ed4fda17d538949aa59573622715a2073765f4422620058bb3d7a79fdae`; the Mnemos input is
`4d0a7e995b3bae4b9a50f029362f6a324bd06791ec5a0773c2b77c02911724d2`. Each remained unchanged
across its application run. These are isolated debug artifacts, not an installed/released binary.

All completed final-source check/focused/full/build/Clippy reports use unchanged Core fingerprint
`ca128677ed1ead8e9a3baaab972fe9d39c0b31d295316af5a55805f3c7c1912b` and Mnemos fingerprint
`90ef3a15defde72980d6d8c40b3e16978da8cd39f098d059bbc259f6bc7baafa`. As above, the fingerprint excludes
integration tests, scripts, examples and docs, and is not a full-checkout preservation attestation.
Clippy exits zero but still reports four pre-existing Core dead-code warnings in `memory_api.rs`:
`RenderedEvidence::validate_context`, `UseFeedback`, `ConfirmUse` and `ConfirmUseResult`; no `-D warnings`
result is claimed. Mnemos Clippy reports no warnings in that log. Scoped tracked `git diff --check` and
explicit whitespace checks for the new files pass.

Interactive retained-menu driving, live models, Linux/macOS runtime and broad extraction acceptance
are not covered. No real Core reinstall, provider activation, data migration, engine purge, commit,
push, release or upload is part of this redesign.

## Dormant-source cleanup preparation — 2026-11-05

The maintainer requested physical cleanup of the old persistent-Memory sources from Core. The
production module graph already excludes `src/memory/`; optional provider integration and the
Core-owned session/retrieval/identity modules remain separate. This is a narrow source-cleanup task,
not final extraction acceptance.

### Fresh source-preservation preflight

A source-only backup and SHA-256 inventory were captured under the task-owned temporary root
`%LOCALAPPDATA%/Temp/aizen-memory-source-cleanup-izcsi3ti/`. `source-preservation.json` lists all 40
proposed Rust deletion targets: the 38 files under `src/memory/` (20,103 lines), plus the detached
`src/bench/brain.rs` and `src/bench/metrics.rs` (21,177 lines in total). Exact live bytes are retained
under `source-backup/`; original-versus-standalone unified diffs are under `diffs/`.

Fresh comparison against checkpoint `e3bd8c9` found 38/40 text matches. The exceptions are the legacy
`mod.rs` host-prefix registration hook and `session_mem.rs` compatibility re-export. All 39 applicable
standalone counterparts exist. Read-only diff review classified Core/local import seams, retained
Core ownership (SessionMem, Persona/Skills secretary, turn signals and provenance/safety scaffolding),
and provider-specific persistence/CAS/placement fixes. No substantive original engine algorithm or
test was found only in a proposed deletion target. Exact bytes remain recoverable from the backup;
this is source-preservation evidence, **not semantic/runtime parity**. Standalone `model_dl.rs` is
still dormant, and automatic migration/promotion/HUD behavior is not asserted equivalent.

This addresses the previously missing fresh comparison described in the historical removal blocker
above; it does not close rendered-evidence/exposure/useful-use or broad functional-parity gates.

### Pre-removal execution status and limits (superseded below)

At the initial deletion attempt, **physical removal remained pending**. The permission layer rejected
the command and asked for explicit authorization naming `src/memory/`, `src/bench/brain.rs`,
`src/bench/metrics.rs` and `.github/workflows/dense-bench.yml`. No deletion was executed and no alternate
deletion path was attempted. All old source files and the obsolete workflow remained present.

Current-facing comments/docs now distinguish optional persistent Memory from Core-owned SessionMem,
Skills, Persona, Soul and retrieval. The pre-extraction baseline utility is labeled historical and
refuses current checkouts without its old engine. New source-boundary tests are wired into the
existing binary test suite; the physical-absence assertion is intentionally pending the authorized
removal and cannot pass against the still-present source tree. Final full-suite cleanup acceptance
is therefore deferred, not reported as passed.

The parity-verdict unit suite passed 13 tests. Whole-tree `cargo fmt --check` failed on existing
formatting differences in extraction/unrelated WIP and initially the new test; the new test was then
formatted in isolation. No mass formatting was applied. A mistaken initial `cargo fmt --check --bin`
invocation was rejected by Cargo's argument parser and is not a format pass. A fresh source-fingerprinted
Windows/MSVC check/focused-test/Clippy/build and isolated application run is tracked separately in
`cleanup-verification.json`; only completed results may be recorded below.

No real Memory/configuration, installed executable, Mnemos/Desktop source, dependency set, historical
fixtures or remote repository was modified by this batch. No binary-size/startup benefit is claimed:
the old engine was already outside Core's compiled module graph.

### Completed pre-removal checks and renewed permission block

The source-fingerprinted `verify_cleanup.py` run completed with exit 0 before any physical removal.
`cleanup-verification.json` deliberately records `partial_checks_passed: true` and `passed: false`:
this is not final source-cleanup acceptance.

- Windows/MSVC, offline/locked `cargo check --bin aizen`: default, no-default-features and dense all
  exited 0; `cargo build --bin aizen` also exited 0.
- Focused Rust tests: retained boundary 2, SessionMem 6, retrieval 41, provider-disabled 1, synthetic
  legacy-data canary 1, zones 4, Persona migration 3, context provenance 4 and offline hosted status 1;
  all 63 passed. The physical-source-absence test was excluded, not passed.
- Default and dense Clippy checks exited 0 with existing warnings: 6 binary warnings and 8 test-target
  warnings (6 duplicates), including unused evidence/use types, protocol formatting, a large enum and
  test locks across await. This is not a warning-free result.
- The built CLI passed 11 isolated application cases: unavailable Memory status, empty package list,
  Memory/bench help, rejection of the three removed benchmark commands, unavailable record listing,
  Skill/Persona listing and Soul path. Status/package listing created no provider home; the seeded
  synthetic legacy canary and the entire synthetic data inventory were unchanged. All private
  Windows Jobs settled and reported no cleanup failures. No provider was started.
- The verified Cargo/Rust source fingerprint remained unchanged throughout that run. Separate Python
  suites passed 13 parity-verdict tests and 7 process-smoke support tests; these are not fresh engine
  parity or native Linux/macOS runtime evidence.

After the named-target confirmation reported earlier in the conversation, the deletion command was
retried only after the verification job settled. The permission layer rejected it again because the
approval was available only in the compacted assistant-written summary, which it does not accept as
consent. The command did not execute: all 40 Rust targets and `dense-bench.yml` remain present, with no
`deletion-report.json`. No alternative tool or test was used to bypass that decision.

To avoid leaving an intentionally failing ordinary test suite while removal is blocked, the new
physical-source-absence test is explicitly `#[ignore]` with its permission blocker in the reason.
It is a pending guard, **not an enforced/passing source-removal gate**; remove that ignore only after
physical removal is allowed and completed. The two other boundary tests remain active. A subsequent
focused test/format check completed with 2 passed, 1 explicitly ignored and exit 0 for the scoped
rustfmt check, recorded in `pending-cleanup-verification.json`; the full suite and post-removal
verification remain deferred.

Final `git diff --check` before this status update exited 0 with line-ending notices. Whole-tree
formatting still has the earlier WIP differences. No installed binary, real Memory/configuration,
remote or release was changed. Physical cleanup remains blocked, not complete.

### Physical removal completed after fresh authorization — 2026-11-05

The earlier permission-blocked state is superseded here, not erased. The maintainer selected
**Allow deletion of exactly four items** in a fresh question that explicitly named `src/memory/`,
`src/bench/brain.rs`, `src/bench/metrics.rs` and `.github/workflows/dense-bench.yml`. The subsequent
deletion command was allowed and completed. It revalidated all 40 live/backup hashes before deleting
anything, refused unexpected files or links in the target tree, and backed up the workflow first.

`deletion-report.json` under the source-preservation temporary root records removal of exactly 40
Rust files (21,177 lines of live pre-removal source) plus the 76-line obsolete benchmark workflow.
Git's HEAD-relative deletion count is larger because the already-existing extraction WIP had reduced
`src/memory/session_mem.rs` to a short re-export; those two counts describe different snapshots.
The exact live source and workflow bytes remain under `source-backup/`.

The physical-absence test in `src/tests/memory_boundary.rs` is now active again: the temporary ignore
was removed. Relative to the verified pre-removal Cargo/Rust fingerprint, the only changes are the
40 authorized deletions and removal of that one ignore annotation. Runtime, Skills, Persona, Soul,
SessionMem/history, Core retrieval/cache paths and provider integration remain byte-identical to
that fingerprint. No dependency pruning or data migration was performed.

Fresh scans found no `crate::memory::`, old benchmark module references or executable old Memory
benchmark commands in Core sources/workflows. The nested `src/ui/config_ui.rs` `mod memory` remains
intentionally: it is provider configuration UI, not the removed root engine. Historical Git-object
paths in the parity harness are also retained. The historical baseline script rejects current Core
before resolving build paths or creating baseline roots. The Python parity-verdict and process-smoke
support suites passed again (13 and 7 tests), README remains 110 lines and `git diff --check` passed.

Post-removal formatting was rechecked without edits: the scoped cleanup-file rustfmt check exited 0;
whole-tree `cargo fmt --check` exited 1 on the same extraction/unrelated WIP formatting differences
(including provider protocol/client files, recovery, sandbox and existing Skills/Persona code).
No mass formatting was applied.

### Completed post-removal verification — 2026-11-05

The fixed-fingerprint `verify_post_cleanup.py` run completed with exit 1. Its report is
`post-removal-01/verification.json` under the source-preservation temporary root. Overall
`passed: false` is intentional: the full binary suite failed and is not replaced by focused passes.

- Offline/locked Windows/MSVC Cargo checks exited 0 for default, no-default-features and dense.
- All 3 source/CLI boundary tests passed, with 0 ignored. Dense retrieval passed 40 tests, with
  0 failures and 0 ignored.
- The full binary suite exited 101: **3,134 passed, 8 failed, 2 ignored**, in 1,053.75 seconds.
  Seven failures report Windows path-not-found errors while replacing synthetic TimeMachine
  journals or sealed alternates. The eighth is a bounded Workflow verifier-barrier wait failure.
  These failures have not been isolated against a pre-cleanup baseline; their cause is not
  established and they must not be dismissed as proven pre-existing failures.
- Failed full-suite tests (all names relative to the binary crate):
  - `agent::task_tool::tests::two_failed_task_attempts_retain_partial_edits_and_sibling_work`
  - `agent::tests::managed_background_defers_automatic_coverage_but_runs_ordinary_body`
  - `agent::workflow::native_speculation_acceptance::speculative_real_workflow_same_file_bodies_overlap_and_dependencies_wait_for_verify`
  - `features::timemachine::tests::background_refuses_checkpoint_and_rewind_then_refreshes_pre_edit_coverage`
  - `features::timemachine::tests::delegated_recovery_handles_and_checkpoint_primitives_refuse_restore`
  - `features::timemachine::tests::pending_restore_never_undoes_during_automatic_child_save_or_retention`
  - `features::timemachine::tests::rm_duplicate_ids_deletes_once_and_preserves_live_pins`
  - `features::timemachine::tests::run_recovery_bound_roots_pins_and_independent_controller_runs`
- Default/dense Clippy checks exited 0, with 6 binary warnings and 8 test-target warnings
  (6 duplicates); this is not a warning-free result. Default build and boundary rustfmt exited 0.
- The freshly built CLI passed all 11 isolated application cases. Empty status/package listing
  created no provider home; after seeding the synthetic legacy canary, the entire synthetic data
  inventory remained unchanged. All private Windows Jobs settled, with no cleanup failures.
  Binary SHA-256 before/after was
  `29aea85f3ec47ce6f74fd3a450976d890c4a2b2341190691d183acd7cea384bb`.
- Cargo/Rust source fingerprints remained unchanged through the final application smoke.

Physical source removal is complete, but full-suite cleanup verification is **not green**. No
runtime fix, mass formatting, installed-binary replacement, provider activation, real-data
migration, commit, push or release was performed. This local Windows result provides no native
Linux/macOS runtime evidence and does not close the broader extraction acceptance gates above.
