# Creative adapter contract v1 — foundation

Implementation: [contract.rs](../src/agent/apps/contract.rs) and
[jobs.rs](../src/agent/apps/jobs.rs). **Not an enabled transport API or adapter support claim.**
No authenticated enrollment, production receipt constructor or `app_*` tools exist yet. Do not
connect an arbitrary MCP server to these types and treat it as trusted. See the
[implementation plan](plans/creative-production-2026-10-29.md) and
[verification log](creative-production-verification-2026-10-29.md).

## Fixed target and capabilities

Protocol version is the integer `1`, distinct from MCP's negotiated protocol version. All wire
objects reject unknown fields. Adapter/app kinds are closed: `blender`, `unity`, `unreal`, `spine`.
The frozen target contains exactly:

- `adapter_id`, `adapter_version`, `app`, `app_version`;
- `instance_epoch`, `project_root`, `document_id`, `revision`, `schema_digest`.

`project_root` is an absolute UTF-8 host path. Contract validation is syntactic only; filesystem
containment, identity, enrollment and permitted project checks belong to the controller/adapter.
Matching fields or hashes confer no authority. Journal admission additionally requires an existing
canonicalizable non-reparse project root, exact managed path spelling and conservative alias checks.
Current journal root matching rejects non-ASCII project roots on Windows/macOS rather than claiming
correct Unicode case folding. Such projects are **not supported by this foundation's admission**.

IDs accept bounded Unicode names/spaces but exclude controls, bidi formatting and endpoint-like
separators; versions use bounded ASCII version text. Schema digests are 64 lowercase hex characters.
These checks reduce accidental leakage/ambiguity, not a universal secret detector.

A capability manifest is `{protocol_version, target, operations, supports_cancel}`. `operations`
contains at most 16 unique operation names. It is data, not permission. A batch is
`{protocol_version, target, operations}`, containing 1–32 operation objects; the target must exactly
match the frozen manifest including epoch, revision, schema and path spelling. No retarget-to-active
fallback. Run schemas/allowlists remain frozen; refresh requires a new prepared call.

## Closed operation vocabulary

All payloads use `kind` as a snake_case discriminator. These are a proposed common subset, not
proof that any host implements the operation or can create a complete game from these six alone.

| Kind | Required payload fields |
|---|---|
| `set_transform` | `object_id`, `translation`, `rotation`, `scale`: three finite numeric triples |
| `assign_material` | `object_id`, `material_id` |
| `import_asset` | `source`: project-relative slash path |
| `export_asset` | `object_id`, `preset_id`, `output`: project-relative slash path |
| `render_still` | `camera_id`, `frame`, `width`, `height`, `output` |
| `run_scenario` | `scenario_id`, `seed`: explicit unsigned 64-bit seed |

Transforms allow zero/negative scales and magnitude at most 1e9. Host units, rotation conventions,
registered export presets and scenario semantics must be pinned by the adapter schema. Still frame
is 0–1,000,000; dimensions 1–8192 and at most 32 Mi pixels. Negative timelines and other unsupported
host features require a future explicit versioned extension, not disguised payloads.

Relative paths: at most 2,048 UTF-8 bytes, 32 slash components, 255 bytes/component; no traversal,
absolute/drive/UNC/backslash/ADS spelling, empty components, Windows reserved names/invalid characters
or trailing whitespace/dots. This is portable lexical rejection, **not** symlink containment.
The adapter must validate inputs/outputs against actual enrolled roots and approved overwrites.

No `eval`, script, shell, arbitrary host method or console operation exists. Native code/script
editing continues through existing reviewed file tools and normal execution approval. Registered
scenario IDs must resolve to bounded trusted scenario drivers; they are not names of arbitrary code.

## Byte boundaries and digest

Use bounded `from_slice` entrypoints: reject raw input larger than 64 KiB before serde, then validate
intrinsic fields/capabilities/target and normalized serialized batch size (also 64 KiB). Count
visitors alone do not bound allocation of a string or the first excess element. Upstream transports
must cap message bytes before buffering; a function receiving an already allocated slice cannot
undo that allocation. Do not forward parse errors containing untrusted values into public status.

Request digest is SHA-256 of the implementation's validated compact serde JSON. It binds order,
values and frozen target, but is not general canonical JSON across languages. A future transport
must specify the exact hashed bytes/float representation; adapters must not assume their own JSON
serializer produces the same digest. Reject nonfinite floats before serialization. `digest()` does
not replace capability/freshness validation or authorize dispatch.

## Durable intent and controller ownership

Journal root is supplied explicitly; CLI reads `AIZEN_HOME/app-jobs`. Fixed files:
`operations.lock` (initialization marker and cooperative writer lock) and `operations.json`.
Reads cap bytes at 1 MiB; at most 128 strict versioned records; no automatic eviction, replay or
ID-to-path lookup. Marker/journal mismatch, empty persisted history, corruption or capacity is an
error, never permission to recreate empty history. A pristine absent pair is empty. Owner-only
atomic writes and short cross-process locks protect bookkeeping, not the editor or workspace.

The controller must acquire whole-effect resource admission **before** journal `begin` and retain
it through settlement. Begin durably writes `prepared/unknown`, then returns a runtime `OwnedJob`.
It does not dispatch. The operation ID is random and independent of log/run IDs. Local nonce/record
ownership is shared only by clones of that Journal, not serialized, restorable from status or
transferable to a second Journal. Restart preserves barriers but does not automatically adopt control.

Execution states: `prepared`, `running`, `succeeded`, `partial`, `failed`, `cancel_requested`,
`cancelled`, `unknown`. Effects: `not_started`, `active`, `settled`, `unknown`.
Cancellation is intent, not settlement. Acceptance cannot resurrect unknown; explicit reconciliation
requires matching evidence. Terminal transitions require a matching settled receipt and are
irreversible. `partial/settled` fails production acceptance despite physical quiescence.
`failed/not_started` is allowed only from prepared with independent definitely-never-dispatched
proof; lack of an acceptance response is not that proof. No automatic timeout unlock.

Receipt fields bind ID, target, digest, owner scope, expected local sequence and effect state.
The test-only receipt constructor is not authentication. Production receipt construction remains
unavailable until enrollment, authenticated transport and verified structured result handling exist.
Generic MCP JSON/prose must never reach a privileged constructor. Monotonic local sequence checks
prevent stale transitions but are not a substitute for host event ordering.

Proof boundaries still to implement: neither the journal nor its operation digest records pending
output/resource claims, host step results, before/after revision or artifact verification. Terminal
receipt matching here tests correlation and settlement state only; it is not a production quality
verdict. Whole-lifetime admission, restart recovery, inspected retention/archival, authenticated
host event ordering and an actual transport are gates before any mutation tool is enabled.

Offline CLI projects only operation ID/execution/effects/sequence. It marks observations as
last-recorded, not live, and does not restore cancel authority. Future recovery/retention requires
explicit inspected design; do not delete a journal or mark jobs settled to make new work run.
