# Memory plugins

Status: extraction in progress. Local Windows lifecycle, Agent, recall and offline completed-turn
learning have been exercised with the actual Core and standalone Mnemos executables. This is not a
published package or a claim that every acceptance surface is complete; see verification status.

## Ownership

Aizen Core retains the Runtime, model routes and credentials, approval/sandbox, Skills, Persona,
Persona episodes/reflection/insights, Soul, saved conversations and ephemeral session notes.
Persistent User/Project Memory is optional and supplied only by the selected external provider.
Mnemos extracts the existing fact engine; it is not linked into Aizen. No provider is a normal state.

## Package contract

A platform-specific directory or ZIP contains `plugin.json`, a native executable and declared files.
There are no install scripts or package build hooks. Installation must not execute the package.
`plugin.json` is strict JSON, with duplicate keys and unknown fields rejected:

```json
{
  "schema_version": 1,
  "id": "example-memory",
  "name": "Example memory",
  "version": "0.1.0",
  "kind": "memory",
  "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
  "target": "x86_64-pc-windows-msvc",
  "entrypoint": "example-memory.exe",
  "capabilities": ["recall", "records", "context"],
  "permissions": { "network": false, "host_llm": false },
  "files": [
    { "path": "example-memory.exe", "bytes": 1, "sha256": "<64 lowercase hex digits>" }
  ]
}
```

The file length and digest above are illustrative, not valid integrity metadata or a published
artifact. The manifest itself is pinned separately in installation metadata. Declared files exclude
the manifest, which cannot hash itself. Entry points are relative files, never shell expressions.
Reject traversal, links/reparse points, ADS/device paths, case collisions, undeclared files, excess
expansion and incompatible targets. Integrity establishes exact content, not publisher trust.

Package state, plugin eligibility, provider selection and Memory enablement are separate. Updating
executable/manifest hashes invalidates prior executable authorization. Configuration is HOME-owned;
project files and model tool arguments cannot authorize a package. Uninstall preserves provider data.
Provider binaries and data have separate roots. No automatic download, selection, migration or update.

Uninstall holds the same activation/settlement admission as execution and never runs provider code.
If its package directory is already missing, it can retire the host registration/selection while
preserving data. If an existing package is corrupt, partial, or replaced by an unexpected file, the
operation first disables it, then refuses removal and retains pins/selection for explicit recovery.
Unknown/unverified files are not swept. After explicit inspection/repair removes the remaining tree,
uninstall can retire the registration. An unresolved physical transaction blocks uninstall even
when the package is missing; missing payload is not proof of process settlement.

Local directory/ZIP installation remains generic. Mnemos additionally has a shared public release
channel and reviewed install/update flows for CLI, configuration UI and Desktop:

```bash
aizen plugin releases mnemos --json
# First install only; noninteractive use requires all three reviewed pins:
aizen plugin install-release mnemos --version X.Y.Z --sha256 <64-lowercase-hex> --yes
# Installed package update; interactive review defaults to No:
aizen plugin update mnemos
# Noninteractive update requires the exact version, independently reviewed SHA256 and consent:
aizen plugin update mnemos --version X.Y.Z --sha256 <64-lowercase-hex> --yes
# Offline inspection of interrupted package replacement (no automatic recovery):
aizen plugin recover-update mnemos
```

The fixed anonymous channel is `talmetis-labs/aizen-mnemos-releases`, never the private source
repository. Checks display verified installed and downloadable versions separately and do not execute
providers, create Memory data or change provider configuration. For an installed Mnemos, startup
performs a bounded nonblocking availability check on the same 24-hour cadence as Aizen's updater.
`update_check: false` or `AIZEN_NO_UPDATE_CHECK=1` disables automatic checks; explicit checks still
work. CLI startup reads cached notices without waiting for the network; `/update mnemos` opens the
explicit update review. Desktop uses the same Core cache/policy and shows Mnemos in **Settings →
Updates**. Opening/focusing panels uses cached state, not repeated forced network checks. Snapshots
remain offline. Failed checks retain last-known results as stale and back off automatically for
15 minutes. No installed Mnemos means no automatic Mnemos release request.

Stable metadata-only releases are not installable: a version tag or GitHub's “latest” label does not
prove a target ZIP and checksums exist. Incomplete bounded discovery is not an authoritative latest
version. Cached information is never permission to install or update.

After confirmation, Core revalidates the exact release/asset/checksum identity, downloads the native
target ZIP with mandatory SHA256 checks, and verifies manifest ID/version/target before publication.
First installation does not authorize, select, enable or execute Mnemos. Updating requires a verified
installed package and a strictly newer stable version; same-version replacement, downgrade and
repair are not supported. Update refuses while physical provider work is busy rather than killing
it. Incompatible retained settings also refuse before replacement; they are never silently dropped.

Update preserves the selected provider ID, settings, budgets, configured data-root and Memory data.
It **revokes Mnemos execution and Learning grants** and makes that package ineligible; global Memory
is disabled only when Mnemos is selected. Review and authorize the new executable separately through
existing activation controls. No provider is run and no Memory data is opened, copied or migrated.
Preserved data does not establish future data-format compatibility or safe downgrade after writes.

Replacement retains old payloads and a host-owned transaction journal. Uncertain settlement blocks
provider execution and ordinary package changes. Inspect with `aizen plugin recover-update mnemos`;
only an explicitly confirmed `--apply` settles recognized verified evidence. Noninteractive recovery
also requires `--operation-id <inspected-id> --yes`. Unknown/conflicting evidence remains blocked;
never retry an ambiguous update automatically or delete its evidence. Recovery does not roll back
Memory data or restore execution consent. Platform flush limitations mean this is not a promise of
unconditional power-loss safety.

Hashes protect integrity, not publisher trust or signatures. Generic configured catalogs/arbitrary
HTTPS-URL installation are not implemented. Provider networking and host-model assistance require
separate grants; package checking/installing/updating never transmits conversations.

### Standalone Mnemos packaging

The sibling checkout owns `scripts/package.py` and `scripts/test_package.py`; neither imports Core
or assumes `../aizen`. With Python 3.11+ in isolated mode, package an ordinary byte copy of a prebuilt
binary using `python -I scripts/package.py --binary <file> --target <triple> --output <new-directory>`.
Python is packaging tooling only, not an executable-provider dependency. Supported triples are
`x86_64-pc-windows-msvc`, `x86_64-unknown-linux-musl` and `aarch64-apple-darwin`.

The ZIP includes the executable, strict manifest, Apache license/NOTICE and standalone README/adoption
contract; `SHA256SUMS` accompanies it. No payload is built or executed. Binary-format validation
remains the installer’s responsibility. The manifest declares no network access and does declare
`host_llm`; declaration is not saved consent. Initialize grants it only for an explicit prepared-job
transaction with Learning, after Core's separate learning/model-sharing checks. Hashes are integrity,
not signatures.
Output uses exclusive creation, not atomic directory publication; failed partial output is retained,
never overwritten or silently removed. Use trusted non-concurrently-edited input/output directories.
The standalone CI definition covers lexical/dense builds/tests and local packaging on three platforms
with read-only repository permission and no upload/release step; a definition is not remote run proof.

## Configuration UI and per-provider options

Open `/config` → **Memory**, or run `aizen config memory` directly without first configuring chat
credentials. The everyday menu has five entries: **Provider**, **Learning**, **Storage**, **Advanced**
and **Back**. Advanced holds package administration, options/budgets, diagnostics and saved consent.
Local install never selects or runs the package. No provider is a normal state.

**Provider** always lists Mnemos with an accent color and textual status, even when not installed.
Absent Mnemos opens setup with an explicit public-release check and local directory/ZIP import;
both installation paths verify the package ID before publication. Opening setup does not download.
Release setup displays installed/downloadable versions and requires separate confirmation to download
and install. Installed custom providers use verified manifest names. Choosing activation for a
provider asks for execution/platform/network consent if missing, then atomically makes it eligible,
selects it and enables Memory after confirmation. Saved valid consent can be reused. Obsidian is not
an implemented provider: using a vault would require a separate, explicitly installed adapter.

**Learning** shows effective On only for an active eligible verified provider with a learning
capability, valid execution/sharing consent and enabled automatic policy. First On asks separately
for bounded completed-turn sharing and optional same-turn model assistance (when supported), both
defaulting to No. Off preserves recall, selection, data and saved permissions for the next On;
explicit revocation remains in Advanced. Skills and Persona learning are independent. Learning
policy changes invalidate old queued work instead of reviving it after a later toggle.

**Storage** offers Automatic (the default `<AIZEN_HOME>/memory-data/<provider-id>/`) or a custom
absolute dedicated directory. The provider organizes files inside that host-approved root. Preview
and save do not create the directory or launch a provider; initialization occurs only for an actual
authorized operation. Changing/resetting the root does not migrate, copy, adopt or delete anything:
old records stay at the old location and the new location may appear empty. Disable/uninstall
preserve default and custom data. Core refuses broad HOME/workspace grants, unsafe paths and
package/control/model/runtime or other-provider overlaps. Preflight is defense in depth, not
protection from another same-user process replacing ancestors; Windows guarded limitations remain.

Confirmed lifecycle actions save immediately. Option/budget edits are a draft until **Save**;
Back/Esc discards that draft. Every consent confirmation defaults to No, shows the executable path
and SHA256 separately from the pinned manifest SHA256, and rechecks the configuration generation at
commit. Concurrent edits cause a conflict instead of silently authorizing changed state. Editing
saved consent does not re-enable disabled Memory.
Required network access cannot be withdrawn while keeping valid execution consent: revoke execution
instead. Windows guarded execution still cannot confine filesystem/network access.

- Disable preserves selection, options, grants and data.
- Clear selection disables Memory but preserves installed packages, options, grants and data.
- Revoke removes that package's execution grant; if it is selected, Memory becomes disabled.
  Selection, package, options and data remain.
- Uninstall conservatively removes verified package files only; provider data remains.
- Core codebase embedding is under `/config` → **Codebase**, not inside Memory or a provider model setting.

Equivalent offline commands include:

```bash
aizen plugin storage mnemos                            # inspect without launching
aizen plugin storage mnemos --path /absolute/dedicated-directory
aizen plugin storage mnemos --default                   # no migration or deletion
aizen plugin configure mnemos                          # schema, effective options and host budgets
aizen plugin configure mnemos --set enable_fuzzy=false --set recency_half_life_days=45.5
aizen plugin configure mnemos --reset enable_fuzzy
aizen plugin configure mnemos --context-tokens 800 --recall-tokens 1024 --recall-inject-tokens 300 --recall-limit 12 --max-records 100
aizen plugin revoke mnemos
aizen memory unselect                                  # grants remain; revoke separately
```

`--set` takes `KEY=JSON_SCALAR`; quote string values as JSON, for example `--set 'mode="local"'`
on a POSIX shell. Unknown keys, conflicting/duplicate edits, wrong types and out-of-range values fail
without saving. Each package retains its own options/budgets when another provider is selected.
Configuration inspection and edits do not launch a provider, create its data root, migrate facts or
forward full host configuration/environment. Changes advance generation and are adopted at the next
valid user-turn preparation, not halfway through a running turn.

### Desktop client bridge

Aizen Desktop's provider-backed Memory view uses `aizen memory client`, not the historical
`cli-memory` files. It offers bounded list/search/detail and reviewed Provider, Learning and Storage
settings. Its optional **Mnemos · Aizen Desktop** window shares the read-only browser; it is not an
independent Mnemos application. Configuration stays in the main window. Opening either view does
not install, select or execute Mnemos. Core versions without this command require an explicit user
upgrade; Desktop must not fall back to the old store or automatically install Core.

The command accepts one version-1 JSON request on stdin (at most 256 KiB) and emits one JSON response
on stdout (at most 2 MiB). The request carries `version`, `request_id`, an absolute existing
`workspace`, and a typed `action`. Responses echo version/request ID and contain either `ok:true`
with `data`, or `ok:false` with an error `code` and `message`. Protocol errors can exit zero: clients
must inspect `ok`, not just the process status.

- `snapshot` is offline: package verification, selection, generation, effective Learning, storage,
  capabilities and consent disclosures. It is not a provider health check and does not initialize data.
- Feature `mnemos_releases_v1` adds main-window-only `release_check` (binding, no mutation) and
  `release_install` (binding, exact reviewed candidate and `confirmed:true`). Both delegate to Core's
  fixed anonymous public channel and never run a provider. Check returns installed/downloadable
  versions and package pins. Install revalidates the same release, target and checksum before fresh
  publication; existing packages are refused. Unsupported Core keeps local installation where
  available and requires a separate user-directed upgrade for online release support.
- Feature `mnemos_updates_v1` adds `update_status` (binding and `cached`, `automatic` or `force`
  mode) and `release_update` (binding, exact reviewed candidate, full `expected_pins` and
  `confirmed:true`). Status returns checked/attempted times, cache source, stale/pending/error
  indicators and the installed-versus-downloadable comparison. Cached status is offline; automatic
  status obeys the installed-only cadence and opt-out. Desktop starts one gated automatic check;
  opening/focusing settings reuses cached presentation, while **Check again** explicitly refreshes.
  Update freshly revalidates the frozen candidate and old package under admission. Its response
  contains the new snapshot, `updated_id`, `previous_pins` and `new_pins`; only this response may
  change the selected Mnemos manifest binding, with exactly one generation advance and revoked
  Mnemos consent/eligibility. Another selected provider's identity and activation remain unchanged.
  Recovery-required or ambiguous effects invalidate displayed reviews and must never trigger an
  automatic mutation retry. Older Core requires a separate explicit upgrade for this feature.
- `read` takes the snapshot's complete `binding` and an allowlisted `list`, `search` or `get` query.
  Scope is `project`, `user` or `device`. Results are at most 100 records, sometimes fewer due to byte
  limits; there is no total or pagination promise. Approved reads may start a provider and create its
  storage/lock infrastructure. Detail reads are scope/workspace checked, without management authority.
- `review` takes a binding and proposed typed settings change. It performs no mutation. Its opaque
  review binds the exact change, disclosures, generation, workspace and full installed package pins.
- `apply` returns that review unchanged after explicit confirmation. Core revalidates it under its
  existing transaction rules. The review digest is integrity checking, not a signature or independent
  authorization authority. Stale or ambiguous results require refresh, not automatic mutation retry.

Bindings must not be rebuilt from a later mutable UI selection. Clear displayed records/reviews when
workspace, scope, access or provider changes, discard late responses and revalidate on focus. The
focused Desktop window has a narrow native command allowlist and cannot perform settings writes.
The basic record protocol does not imply graph, editing, lifecycle or maintenance support. Desktop
also negotiates optional rich inspection v1 through the provider's `commands` capability and Core's
`inspect` client feature. This engine-independent extension has a fixed vocabulary for paged records,
graphs, lifecycle views and reports; its schema exposes `version`, `inspect` and `manage`, not a
per-feature UI inventory. Management additionally requires the package's `management` capability,
Core's `manage` feature and reviewed human confirmation. An explicit `unsupported_capability`
negotiation result falls back to basic records; other failures remain visible. Basic browsing is
bounded to 100 records per response and never invents a graph from that list.

The main Desktop Memory pane uses the selected provider's verified name and capabilities, not a
Mnemos identity requirement. The separate Mnemos companion and legacy-to-Mnemos transfer remain
Mnemos-specific. A plugin in another application's catalog, an HTTP API, an MCP server or an Obsidian
vault is not automatically an Aizen provider: an explicitly installed adapter must implement Aizen's
package and stdio protocol contracts. No compatibility with a particular Hermes-catalog provider is
established here. Custom packages currently use Core's generic local installer before appearing in
Desktop settings; the Desktop install shortcut is Mnemos-specific. Switching providers does not
copy or migrate existing data.

Native Desktop record browsing is currently Windows-only: its owned Jobs contain Core/provider
children. Unix provider sessions escape Desktop's process group, so that bridge refuses reads before
launch on Linux/macOS until it has equivalent ownership. Offline settings remain available. This is a
Desktop bridge limitation, not removal of Core's own provider CLI support on those platforms.

### Transferring legacy Aizen memory

Desktop's main Memory settings provides **Transfer old memory to Mnemos**. The normal path needs
no typed directories: Core proposes its effective `AIZEN_HOME/cli-memory` source and a new managed
store. Advanced location controls accept another whole legacy store and an external destination
parent. The provider receives a fresh dedicated operation directory beneath that parent, not write
access to the parent itself. The read-only companion cannot run migration.

1. Install/select/enable a compatible Mnemos package through the ordinary explicit consent flow.
   The package must advertise `legacy_adoption_v1`; this is an offline facility declaration, not an
   agent tool or a normal provider-context grant. Older Core/package versions require an explicit
   upgrade. Opening the wizard or inspecting status never executes a provider or scans the old store.
2. **Check old Memory** explicitly authorizes an offline source preview. No stopped-writers checkbox
   is required for checking. Preview can create private bookkeeping and an empty operation parent;
   it does not publish data or change Storage. Alternate locations and paginated inventory stay in
   expandable details. Direct non-Markdown archive attachments require exact, explicit review before
   a fresh preview; they are backup-only, never imported as facts.
3. **Back up and transfer** requires one acknowledgement that all source writers, including cache
   jobs, are stopped until completion. The action authorizes the reviewed digest. It backs up every
   inventoried source file, copies only supported Memory data and verifies the separate new store.
4. **Use imported Memory** prepares a read-only Storage review after verified publication. One
   **Use this store** click applies that fresh review. **Not now** leaves Storage unchanged. Learning
   policy is unchanged. Selecting the previous store again is explicit and does not merge or undo
   writes made in either store.

The source is the whole flat legacy corpus, not just the open project. IDs, Markdown, graph data and
scope/lifecycle metadata are preserved. Unknown/malformed layouts refuse rather than silently skip
records. Codebase indexes and derived caches are excluded from the new provider store but retained
at the source and in the full backup. Allow enough disk space for the backup plus the copied store;
source limits remain 10,000 inventory nodes and depth 8. V1 retains its original 8 MiB/file and
256 MiB total limits, including exclusions. Packages advertising `legacy_adoption_v2` negotiate
`mnemos-adoption-v2` / `bounded-backup-v2`: actual imported Memory remains 8 MiB/file and 256 MiB
aggregate; recognized opaque derived backup-only files allow 1 GiB/file, with 4 GiB for the complete
source. Hashing/copying uses 64 KiB streaming buffers; metadata and IPC bounds do not increase.
Regular zero-byte `codebase/<name>.json.lock` files are preserved as backup-only; nonempty locks
refuse. Empty locks do not prove that writers have stopped.

V2 attachment review is limited to 32 regular non-Markdown files directly under `archive/`, each
at most 8 MiB and 16 MiB aggregate. The host stores exact sorted paths, lengths and SHA-256 hashes;
the plan must match that authorization exactly and binds it into the reviewed digest. Markdown
(case-insensitive), invalid facts/graph/metadata, unknown subtrees and unsafe files cannot be
reclassified as attachments. Neither v1 nor v2 is granted in normal provider contexts. Old saved
plans retain their format/digest semantics; v1-only packages never receive new flags.

V2 reports are closed `mnemos-adoption-report-v2` envelopes (`ok`, `needs_review`, `error`). Typed
allowlisted diagnostics may include sanitized relative paths and byte limits, never record contents
or arbitrary parser/stderr text. Nonzero exit or malformed output remains a content-free failure.
A preview without a reviewed plan is **Check failed**, not a resumable transfer. Historical
interrupted records without a digest are displayed that way without rewriting their history.

A destination must be new: even an empty existing Mnemos directory is preserved, not reused. An
existing populated provider store stays separate. There is no incremental merge, project splitting,
automatic cleanup or automatic remapping of legacy scopes. Some old project facts lack enough anchor
information to appear in the current project's view; verified copying does not prove universal
visibility. Scope diagnostics that are unavailable are not reported as zero.

Status/history and inventory read Core-owned records without launching Mnemos. Interrupted work
keeps its operation ID; **Resume** is explicit and revalidates source, permissions and package pins.
A changed source can prevent resume. **Recover** verifies an already-written terminal publication
receipt when Core missed the result; it does not manufacture a missing receipt or reimport files.
Unknown physical process settlement remains blocked, not a retry opportunity. No automatic apply,
resume, provider switch or Learning activation occurs after restart.

The typed client action is `adoption`, containing the snapshot binding and an `action`-tagged request:
`defaults`, `preview`, `status`, `inventory`, `apply`, `resume` or `recover`. Clients use opaque
operation IDs and reviewed digests, never editable plan files or executable arguments. The complete
bounded provider plan stays private in Core; the usual 256 KiB request/2 MiB response limits remain.
This Core-managed adoption bridge currently executes only on Windows, where its process ownership
has been verified; host-only status/history remain available on other platforms. This restriction does
not remove the independent Mnemos adoption CLI. Windows filesystem/network restrictions remain
guarded/advisory, not kernel confinement.

### Optional manifest settings schema

A package may add a `settings` object to its pinned manifest. It contains at most 24 nonsecret scalar
options. No declaration means no provider-specific options, not an unrestricted JSON editor.
Updated hosts accept schema-less older packages; older strict hosts may reject schema-bearing
packages. Such packages require the host version containing this configuration extension.

```json
{
  "settings": {
    "enable_fuzzy": {
      "label": "Fuzzy matching", "description": "Use fuzzy lexical matching during retrieval.",
      "type": "boolean", "default": true
    },
    "recency_half_life_days": {
      "label": "Recency half-life", "description": "Days before the recency contribution halves.",
      "type": "number", "default": 30.0, "min": 1.0, "max": 3650.0
    },
    "frozen_core_max_tokens": {
      "label": "Frozen context cap", "description": "Also limited by the host context budget.",
      "type": "integer", "default": 800, "min": 0, "max": 4096
    }
  }
}
```

The fragment is illustrative, not a complete package. Mnemos declares and consumes those three
options. Omission preserves its hosted defaults. Frozen context is additionally capped by the host.
No semantic-model root, separate learning model, endpoint, credential or remote catalog is configured
by these options; hosted Mnemos remains lexical/fuzzy until explicit semantic roots are supported.

Supported definitions are `boolean` (`default`), `integer` or finite `number` (`default`, `min`, `max`),
`string` (`default`, `max_length` in UTF-8 bytes, at most 4096), and `enum` (`default`, 1–32 unique
`choices`, each at most 128 bytes). Each definition requires a nonblank `label` (≤80 bytes) and
`description` (≤400 bytes, may be empty). Defaults must satisfy bounds; serialized values are at most
4096 bytes. Unknown fields, duplicate keys at every depth, controls/bidi overrides and invalid values
are rejected. Keys are lowercase ASCII snake_case, start with a letter and are at most 64 bytes.
Host policy/identity keys such as `prepared_job`, graph controls, grants, paths and budgets are reserved.
No arrays, objects, scripts, environment forwarding, secrets, executable arguments, path grants or
administrator authority can be configured. Labels/help are untrusted terminal text, not instructions.

### Host budgets

| Per-provider host setting | Default | Hard cap | Meaning |
|---|---:|---:|---|
| `context_tokens` | 1024 | 4096 | Standing response and rendered context allowance |
| `recall_tokens` | 1024 | 4096 | Provider recall response allowance |
| `recall_inject_tokens` | 300 | 4096 | Actual recall chat injection, also bounded by `recall_tokens` |
| `recall_limit` | 12 | 100 | Automatic recall result request, also bounded by `max_records` |
| `max_records` | 100 | 100 | Maximum context/record-list response count |

Zero suppresses the corresponding automatic context/recall operation without deleting records.
Response limits are host-validated; providers must return complete items within the supplied envelope.
Host wrappers consume injection allowance too, so actual included records may be fewer. Overall chat
context policy still applies. Budget settings do not grant any capability or permission.

## Transport

JSON-RPC 2.0 over UTF-8 newline-delimited JSON on stdio. Each bounded process handles initialize,
one operation, then shutdown. Notifications, batches, stdout logging and provider-to-host requests
are not allowed. Stderr is drained with a byte ceiling and must not contain secrets or transcripts.

- API major 1, minor 0; explicitly negotiate protocol range and intersect capabilities.
- Host numeric IDs start at 1, are unique within a transaction and must match exactly.
- Maximum frame: 1 MiB including line terminator, enforced before full allocation.
- Maximum transaction IO: 16 MiB; arrays/strings/nesting and context tokens have tighter limits.
- One in-flight operation and a bounded queue. No unbounded `read_line` allocation.
- Initialize: 5 seconds; ordinary operation: 10 seconds; explicit maintenance/learning: 60 seconds.
  Host-clamped overrides do not remove wall deadlines. Shutdown/drain is separately bounded.
- Invalid UTF-8, duplicate JSON keys, unknown response IDs, unsolicited messages, nonfinite scores,
  oversized/truncated/noisy frames and unexpected exit poison the transaction.

A response is not proof of process settlement. The host holds containment, admission and drain
ownership until tree exit and all pipe workers settle. Unknown cleanup remains a recovery barrier.
No session-long writable provider gets the shared-LSP exemption from native-operation exclusion.

## Common request envelope

Initialization identifies the package and protocol and supplies a minimal, explicit context:

- Provider generation, user identity, canonical workspace root/cwd and workspace identity.
- Device identity and approved historical aliases; session ID, logical turn ID, boundary ID.
- Provider-owned data root, allowlisted nonsecret settings and granted scopes/capabilities/budgets.

Do not infer host identities from ambient cwd, USERPROFILE or repository origin. A provider never
receives the full Core config, API keys, approval state or arbitrary environment variables.

Effectful methods carry a stable operation/event ID and a payload digest. Same ID/different payload
is an error. A provider journals intent and effects under its own store lock. A crash between effects
and acknowledgement is not permission to repeat the mutation. Status/recovery resolves the original
operation; neither Core nor the provider blindly retries it with a fresh ID.

Errors have a stable code and bounded message. Initial semantic classes are `unavailable`,
`disabled`, `unsupported_capability`, `invalid_request`, `incompatible_protocol`, `permission_denied`,
`not_found`, `conflict`, `busy`, `timeout`, `ambiguous_result`, `recovery_required` and `internal`.
Details are data, never commands. Logs redact credentials; user content is not routine diagnostics.

## Operations and capabilities

All providers implement `initialize`, `capabilities`, `health`, lifecycle
`session_start/session_end/session_reset`, and `shutdown`. Providers advertise optional functional
capabilities; unsupported methods return a typed error, not a fake successful empty result.

| Capability | Methods | Result boundary |
|---|---|---|
| `recall` | `recall` | Bounded ranked record references and sanitized candidate context items |
| `records` | `store`, `get`, `update`, `forget`, `search`, `list` | Versioned records, provenance, confidence, scope and opaque IDs |
| `context` | `build_context` | Standing context adopted at a real conversation boundary |
| `learning` | `process_turn`, `confirm_use`, `operation_status` | Durable idempotent event receipt; optional prepared job |
| `associations` | `associate`, `affinity` | Opaque host node IDs and bounded scores, not Skill/Persona authority |
| `host_llm` | `prepare_job`, `complete_job` | Explicit no-tool model job and opaque continuation |
| `commands` | `command` | Capability-advertised typed provider extensions, never shell commands |
| `management` | Human-origin record/command invocations | Optional host-sealed authority; never an argument or saved administrator grant |

Advanced extensions cover Mnemos review/archive/revisions, profile/ask, frozen/STYLE, maintenance,
consolidation/reconciliation, model inventory, migration and benchmarks. Extension descriptions
are not automatically registered tools, approvals, policy text or permission grants.

Mnemos supports dimension/category filters, `current`, `global`/`user`, `device` and `project`
selectors, and retired-only `list superseded:true`. Ordinary model requests remain restricted to
visible records. Explicit local CLI/manual invocations may inspect `all` or an exact legacy scope
and move a fact's scope using revision-checked updates. Cross-workspace authority is supplied only
by Core's sealed human caller when the pinned package declares `management`; parameters/settings
cannot grant it. Background learning, context and model tools never inherit this capability.
`archived` is not an accepted list/search/recall field; archive inspection is a separate command.
Listing resolves full-corpus validity and forward supersession before visibility and pagination.
Hosted retrieval currently has no explicit semantic-model roots, so both plain and dense packages
use lexical/fuzzy retrieval rather than enabling hash fallback from the build feature alone.
A dense build conservatively still journals search/recall and requires an operation ID.

### Human management and explicit offline reconciliation

Mnemos's hosted management surface includes review/list/promote/drop/clear, revive, supersede,
archive/restore, explicitly confirmed archive-only purge, compact preview/sweep, consolidation,
doctor, neighbors, frozen rebuild and offline model inventory. These share the native engine's
nonprinting operation bodies under its operation lock. Partial mutation errors retain a recovery
barrier, not a successful result. Audits and graph hygiene remain best-effort where documented.
`model_download` is still unsupported; inventory does not load weights or provision a model.

`aizen memory reconcile` prepares a bounded system/input pair and `binding_sha256` without calling
a model. A separate `--reply <file>` supplies strict JSON with exactly `binding_sha256` and a nonblank
`completion` string. Core bounds the file to 64 KiB and completion to 32 KiB, rejecting duplicate keys
and unknown fields. `--apply` requires this explicit reply. Preparation makes no fact/audit changes;
a judged preview appends best-effort audit rows but leaves facts unchanged. Apply may update facts.
Core assigns operation IDs; Mnemos journals both judged preview and apply, suppressing replay effects.

The binding covers persistent corpus bytes, exact prompts/pair identities, stable host placement,
consent generation and budgets, but not ephemeral invocation/session IDs. A changed corpus or host
scope conflicts before effects. The digest provides integrity, not human authority: every submission
still needs the host-sealed management invocation. No endpoint, credential, alternate model, automatic
request/retry or fallback is implied; hosted responses report `model_calls:0`. Native standalone
`--reply` remains its existing raw-completion format, not this hosted JSON envelope.

## Prompt integration

Core, environment, Soul, Persona and Persona Self-memory retain their existing order and ownership.
Validated optional Memory follows identity, before Skills. Core owns wrappers and token budgets;
provider strings cannot close wrappers, inject system roles or register tools. Context is lower-trust.

Async boundary preparation obtains standing context once. Synchronous prompt builders consume adopted
snapshots only. Recall runs once per logical fresh user turn, not every model/tool continuation.
Snapshots/handles/events are keyed by provider generation and session/turn. Switching or disabling
invalidates them before the next model request. Saved transcripts remain intact; only known injected
scaffolding is removed from the in-memory outbound prompt. User-authored text is preserved.
Subagents retain their existing restriction against automatic personal identity/Memory injection.

## Learning and host-model assistance

### Implemented offline completed-turn path

Executable execution consent and `--allow-learning` are separate. The latter authorizes bounded
completed-turn text to the selected local executable, not a host-model call. `memory_auto_learn=false`
also suppresses preparation and is checked again before provider launch. Disabling the provider uses
the generation/admission boundary; queued text is never redirected to another provider.

Core mints a UUID v4 for each real host-seated user occurrence, including repeated identical text.
Synthetic user-role nudges do not mint IDs. Explicit full-session, JSONL-delta, recovery and hostbot
storage adapters preserve `aizen_user_turn_id`; model-wire serialization neither emits nor restores
it. Legacy/imported unstamped turns are not inferred into learning events. Compaction retains the
current stamped root, but does not promise to preserve every summarized tool/transcript detail.

`process_turn` uses strict typed input: `operation_id`, `user_text`, optional `transcript`, `dry_run`,
`confirmed` and `payload_sha256`. Nonempty transcripts require SHA-256 over canonical compact sorted-key
UTF-8 JSON containing user text, transcript, session ID, turn ID, selection generation and workspace
ID. Mnemos checks it before effects. This is content binding, not executable authentication.
`confirmed:true` requires sealed human management authority and enables explicit STYLE promotion;
automatic completed-turn events always send false. The typed result must echo the exact operation/digest
and explicitly report `learning_mode:"offline"`; only deterministic user-text extraction runs.
Assistant/tool text is not offline fact authority. Result collections and a 64 KiB aggregate serialized
ceiling apply.
After a valid durable receipt, Core may adopt returned candidates into the exact live lane's bounded
ephemeral notes. It rechecks selection generation and volatile owner identity; reset/replacement or a
late result cannot recreate that destination. Provider anchors never choose a host lane or global
scope. Notes are escaped, budgeted and frozen at the next user-turn preparation, then included only
in a lower-trust user prefix. The content-bound scaffold is removed from subsequent outbound history
and learning text. These notes are not automatically promoted into durable facts. The durable outbox
result can still contain the returned candidate bytes. One-shot Agent/capture has no surviving note
owner; multi-turn REPL/hostbot adoption has unit/build coverage but not actual-application coverage yet.

Core records host-owned receipts below `plugin-state/memory-outbox/` before queueing:
`prepared` → `dispatching` → `done` or `failed`. An identical existing receipt in **any** phase is
suppressed; changed text/context/package pins for the same identity conflict. No crash/restart
implicitly replays prepared, failed or ambiguous work. Receipts reserve up to 256 KiB each before
dispatch, including space for the result; the 16 MiB reservation budget currently admits at most
64 receipts (also subject to a 256-file ceiling). Full capacity stops new learning without deleting
receipts. There is no automatic pruning, retry or management/recovery command yet. Offline
`aizen memory status` (including `--json`) reports advisory phase counts, reserved capacity and
available slots without starting the provider or exposing transcript/operation content. Missing
inventory is not created; malformed inventory reports `learning_outbox:null` and
`learning_outbox_invalid:true`, never a successful empty inventory. This inventory is not settlement
proof. Receipts include bounded transcript text and use host-owned private storage. Runtime
physical-settlement/recovery markers remain independent and authoritative. This is fail-closed
at-most-once attempt suppression, **not exactly-once learning, multi-file atomicity or a power-loss
guarantee**.

The common preparation gate accepts only successful `Done` turns on Agent CLI/capture, REPL and
hostbot, before configuration lookup or outbox reservation. Events retain the originating turn's
cancellation token and execution deadline across queueing, all provider RPCs and the bounded model
wait. Already-cancelled/expired events are withheld before reservation/claim; cancellation never
implies replay of an existing receipt. Skill/Persona learning keeps its own independent policy.
Agent CLI/capture await delivery before exiting; REPL queues background delivery and hostbot awaits
it under its learning lock. Actual end-to-end coverage is reported per surface; token inheritance
does not by itself prove that a disarmed TUI turn can still receive Esc during detached learning.

Provider request observers use the earlier of their inherited execution deadline and a 95-second
ceiling. Expiry or observer drop cancels only that request's child token, not its parent/siblings.
It reports unknown effects until the actor proves pre-spawn exit or physical tree/pipe settlement;
logical timeout alone cannot retire recovery state, release admission or authorize a switch.

### Prepared same-turn-model continuation (partial source implementation)

Mnemos owns persistent fact extraction, prompts, gates, parsing, review, deduplication, application,
reconciliation and confirmation. Core independently owns Skill learning and Persona evolution;
neither is disabled merely because Memory is absent.

The new `--allow-turn-model` opt-in requires `--allow-learning` plus declared `host_llm` capability
and permission. Existing saved grants deserialize with model permission **false**. Consent covers one
additional bounded no-tool call on each completed turn's actual chat endpoint/model, including future
chat route choices. Provider-prepared Memory/conversation text may leave the machine when that chat
route is remote and can incur token costs. No API key or full Core configuration is sent to a plugin.
The route, credential, effort and configuration are captured from the turn; no summarizer role,
background-model resolution or alternate-model fallback is used.

`prepare_job` wraps the bound `process_turn` payload, tool-call count and recovery signal. Its ID is
`<turn-operation-id>-prepare`. The typed reply echoes both identities and the payload digest, then
contains either no job or a bounded job (`id`, `system`, `input`, `corpus_sha256`,
`max_output_tokens`, `expires_unix_secs`). It cannot specify an endpoint, model, keys or tools.
The preparing process must settle before model dispatch. Core rechecks generation, saved consent and
auto-learning, holds activation admission through dispatch, and refuses expiry beyond ten minutes.
Output is limited to the tighter of the captured turn configuration's `max_tokens` (if set) and
2,048 tokens/the job cap; a configured zero cap refuses dispatch. The complete HTTP body is limited
to 256 KiB and returned text to 32 KiB. The call has a 60-second ceiling intersected with job/execution
deadlines, no retry and no redirects. Tape replay/strict mode refuses before HTTP; this path does not
record/replay model continuations. Routes requiring the newer output-cap field can explicitly set
Core's config-wide `output_token_parameter:"max_completion_tokens"`; omission uses `max_tokens`
unless an ordinary request already learned the rename quirk. There is no dialect probe, model-name
guessing or automatic endpoint-specific setting. A separate no-redirect HTTP client deliberately
preserves the captured route/auth/config rather than
reusing the chat client's potentially redirect-following transport. Reported usage enters Core's cost
meter. Codex OAuth prepared learning is explicitly unsupported; it does not resolve fresh credentials
or substitute a different transport/model.

`complete_job` returns the bounded completion in a new settled provider transaction under the original
turn operation, prepare identity, job ID and payload digest. Mnemos additionally binds the prepared
job to scope, corpus state and expiry. Core requires `learning_mode:"same_turn_model"` for this
receipt and `"offline"` for `process_turn`. A provider-owned gate returning no job uses a separately
journaled offline operation; an actual model failure never triggers an alternate-model retry.

**Reachable with separate consent:** Mnemos advertises `host_llm` and initialize accepts it only
alongside Learning and the exact boolean prepared-job marker. Core additionally validates the saved
learning and same-turn-model grants. Initialized subprocess tests and actual Core → loopback model →
Mnemos fixtures cover success, HTTP failure, malformed OpenAI responses, captured route/cap/effort,
and offline/no-job gates. These are scripted mechanics, not semantic extraction quality. Actual
revocation, cancellation, timeout and malformed secretary-content cases remain open; see the
[verification record](MNEMOS_VERIFICATION.md). No permission means no extra model call; local
manual/regex/lexical operation remains possible.

## Legacy data

Do not read or modify legacy `cli-memory` merely because Core starts or a provider is installed.
Adoption is never automatic at startup, installation or selection. Preserve unknown scalar
frontmatter and record formats. Core codebase indexes and `code-*` caches are not provider data;
Skills, Persona, Soul, sessions and unrelated models must not be absorbed into Memory. The standalone
Mnemos native CLI now supplies explicit
`adoption plan --source <legacy-cli-memory> --destination <new-root> --dry-run`, followed by
`adoption apply --plan <saved-plan> --reviewed-digest <digest> --journal <new-journal>`.
Planning bypasses Engine initialization. The reviewed inventory binds source identities, hashes and
exclusions; apply backs up the complete inventory, including exclusions, before copying only the
allowlist into a new destination with no-replace publication. Explicit `adoption resume` verifies an
interruption; `adoption restore` publishes the complete legacy backup into another new directory.
There is no automatic resume, source mutation, ID conversion, overwrite or cleanup. Readonly private
backups are tamper-evident, not WORM; torn or corrupt files fail closed instead of being repaired.
See the standalone package's `docs/ADOPTION.md` for the exact layout, limits and commands. Windows
fixture contracts have run; non-Windows adoption and power-loss durability remain unverified.
Uninstall never implies removal of adopted or original data.

## Platform security limits

Providers are executable extensions, not trustworthy because a manifest parses. Use the sandbox
runner, protected contained spawn, strict environment allowlist and per-request restrictive grants.
Do not inherit broad build-cache writes or global network allow as plugin authorization.

Windows guarded mode enforces Job containment/resource limits and environment controls, **not**
filesystem/network confinement. A malicious child may access files/network available to the user.
Strict mode must continue to refuse unsupported isolation. Informed guarded authorization is not
proof of kernel isolation. Linux/macOS policy and actual platform tests must be reported separately.

## Verification status

On 2026-10-30, fixture-owned Windows runs exercised the unchanged prebuilt Core with standalone
Mnemos: 59 lifecycle/CRUD/native-edit/archive/restore/disabled-corrupt-package/missing-registration
cases; actual Agent `memory_save` and
`memory_search`, standing context, on-topic recall and unrelated-query rejection; and offline
completed-turn learning with separate consent, distinct same-text turn IDs, bound receipts,
assistant-transcript rejection, auto-learning/provider disable and read-only corrupt-outbox status.
Each command used a private bounded Job; the reports recorded settled jobs, no cleanup failures and
unchanged input binary hashes. These runs used a loopback scripted model, not live-model quality
measurements. An independent faulty executable also passed 11 runtime fault modes across 97 actual
commands: semantic conflicts settle cleanly, while uncertain effects retain a recovery barrier and
block replay, disable and uninstall. All those Jobs settled without cleanup failures. This is not
actor-panic, queue-cancellation or recovery-resolution coverage. See
[the dated verification record](MNEMOS_VERIFICATION.md) for exact evidence/gaps.

Lane-owned inferred notes, scaffold removal, preparation invalidation and the common `Done` gate have
focused unit/build coverage. Hostbot command-handler regressions exercise successful reset/cwd/model
changes, invalid input and settings-write failure without starting a bot transport. The proposed
multi-turn REPL harness is not runnable yet: its owned ConPTY support helper has not been implemented.
Do not treat its source as a passing TUI result or the command tests as live-bot coverage.

On 2026-10-31, matched-source verification at the pre-review-fix revision passed Core check/debug
build, default/dense strict Clippy, no-default/dense checks and 200 focused tests. Standalone Mnemos
passed formatting, all-target check/strict Clippy, all 443 tests, dense check and 44 dense RPC tests.
The fresh application reruns again passed 59 lifecycle, 11 Agent and 19 offline-learning commands.
Independent review then found Core replay/cap/dialect, prepared persistence/ID-allocation and adoption
lock-origin defects. Fresh matched-source verification after the fixes passed Core check/build,
no-default/dense checks, default/dense strict Clippy and 96 focused tests (including all eight HTTP
regressions). Mnemos passed formatting, checks/build/strict Clippy, all 455 tests (17 adoption),
and another 49 dense RPC tests. Unix-specific runtime coverage remains unverified. Application reruns
with those new binaries passed all 59 lifecycle, 11 Agent and 19 offline-learning cases, with unchanged
hashes and settled Jobs. A fresh whole-binary Core suite at that same source then passed 3,049 tests
(0 failed, 2 ignored); both plain/dense release builds passed. The standalone prebuilt-only ZIP also
passed 58 actual lifecycle commands without Core's generic packaging script. Packaging tests passed
17 with one explicit symlink skip; the six-job standalone CI definition has local YAML/Bash syntax
checks, not a remote execution result. A fresh relocated standalone source copy also passed plain/
dense release builds, its own packaging and strict Clippy with source unchanged. Both exact ZIPs
passed 58 lifecycle commands each through the same unchanged release Core; data was preserved and
all Jobs settled. Paired warm release measurements show no size/startup win: plain grew 114,688 bytes
and its `--version` median changed from 13.3550 to 13.4144 ms; dense grew 168,960 bytes and changed
from 13.3206 to 13.6870 ms. This is one-host subprocess wall time, not provider/retrieval latency.
The dated record retains earlier failures, exclusions and exact source/artifact hashes.
A source-pinned offline observer also passed strict per-query algorithm-slice comparison against
`e3bd8c9`: 1,665 retrieval rows and 140 fixed-date decay cases per implementation, with no ordered-ID,
score/gate or decay mismatch and no observed instability over five repeats. Its 13 verdict tests
passed. This uses verbatim algorithm slices and hash embeddings, not full store/scoping/RPC parity
or semantic-model quality; it is not sufficient to remove dormant Core source files.

A subsequent actual dense-build CLI run exposed false hits for an unrelated query: the hosted
adapter enabled hash fallback solely from the build feature, without semantic-model roots. A
separate review found that RPC listing omitted validity and forward-supersession checks. Both
failures were reproduced before correction. Fresh Mnemos verification then passed all **458 tests**,
**51 dense RPC library tests**, **15 dense RPC subprocess contracts**, formatting and default/dense
strict Clippy. Fresh relocated plain/dense release builds and independently packaged ZIPs passed
**58 lifecycle cases each** through the same unchanged release Core. Each package also passed 159
latency-fixture CLI commands with settled Jobs, no cleanup failures and preserved disabled data.
With 20 retained warm samples per route, health medians were 191.9706/200.1011 ms (plain/dense build);
search medians ranged from 213.1380 to 227.0216 ms. These are whole-CLI, one-host Windows guarded
measurements, not isolated IPC, former-engine speedups or semantic quality. Hosted retrieval is now
lexical/fuzzy on both builds; semantic-model roots are not yet provided. The verification record
preserves the earlier dense failure and pins the corrected source, binaries and ZIPs.

Extraction is not complete. Remaining gates include reachable same-turn-model learning and its
consent/continuation verification, broader provider/entry-surface/fault coverage, full store/scoping/
RPC parity, final source-removal audit, cold-start/isolated-IPC measurements and non-Windows runtime
verification.
No package has been published and no maintainer installation or real-data migration is implied.
