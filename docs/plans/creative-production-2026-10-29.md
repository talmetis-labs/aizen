# Creative production — implementation plan

Status: implementation started on 2026-10-29; no live adapters enabled.
Base: core checkpoint `a134039`; Desktop checkpoint `61d7941`.
Contract details: [adapter foundation](../CREATIVE_ADAPTERS.md).
Results and verification limits: [verification log](../creative-production-verification-2026-10-29.md).

## Outcome

Reduce time to useful, accepted game/graphics output, not merely time to a successful tool call.
Produce editable assets/code and complete playable loops for both 2D and 3D. Keep planning,
approval, jobs, resource admission, evidence and UX in Aizen. Versioned host adapters own the
application-specific APIs. Existing projects keep their engine, assets and conventions.

Prefer bounded semantic batches over screenshot/click micro-actions. No separate assistant,
resident broker, arbitrary eval language, mandatory external core runtime or duplicated media store.
Core remains one pure-Rust static binary, rustls-only. No new dependency in the foundation slice.

## Current bounded slice

- [x] Strict versioned adapter/target/batch contracts and deterministic validation tests.
- [x] Bounded owner-only operation journal with fail-closed recovery and ownership tests.
- [x] Offline `apps status [key] --json`; configured is not enrolled/probed/used.
- [x] Record baseline checks and the two reference briefs below.
- [x] Full candidate Rust regressions, strict Clippy, release/dense builds and actual offline CLI smoke.
- [ ] Complete independent review; no production app dispatch.

These items do **not** finish P0 or P1. Next integration gates, in order:

1. Explicit approved enrollment bound to configured authenticated connection and a frozen app/schema.
2. Typed prepared approval plus whole-effect project/workspace/asset/GPU admission, including ordinary
   file writes into a live project and durable restart barriers across core processes.
3. Short MCP submit/status/cancel/reconcile with definitely-not-sent vs ambiguous outcomes propagated
   through the complete tool path; no unrestricted alias bypass or blind mutation retry.
4. Owned cleanup and structured job progress/control; only then enable a narrow adapter capability.
5. Actual supported-version Blender acceptance, then both Unity reference tracks under separate
   authorization; no app-specific production or performance claim before those gates.

In particular, journal state alone does not exclude ordinary
file tools, sidecars, other Aizen processes or the editor. Do not register `app_apply` or another
privileged app capability until authenticated enrollment, prepared approval and full-lifetime
resource barriers are integrated and tested. A mock never establishes real-app support or speed.

## Production workflow and acceptance

1. Brief: engine/version, 2D/3D intent, platform/hardware, gameplay pillars, art direction, permitted
   references/assets/rights, editable deliverables and time/cost limits. Infer safe project defaults.
2. Vertical slice: one complete playable loop with camera, controls, UI, feedback/audio and
   win/lose/restart. Evaluate direction before expanding content.
3. Production: reusable native assets/scenes/components, stable IDs and bounded revision deltas.
4. Integration: export/import presets, dependencies, materials, collisions, animation and runtime.
5. Playtest/polish: actual deterministic scenarios and selected views/frames; repair observed defects.
6. Optimize/package: target-device timing, build/run/reopen/reimport and editable-source handoff.

Keep technical validity, visual coherence, animation/gameplay, device performance, maintainability,
rights and human art/game-feel review separate. Build success proves neither aesthetics nor fun.
Missing human review stays pending. No synthetic universal quality score.

### Reference briefs

Original fixture assets only; no vendor/proprietary projects. Do not launch apps or spend on model
calls without separate authorization for isolated test-owned projects/instances.

**2D — courier arena.** One compact top-down arena, original geometric/palette-constrained art,
move/dash, collect and deliver three parcels, one readable hazard, health, timer, win/lose/restart,
keyboard/controller as declared, UI and feedback/audio. Preserve scene/prefab/scripts, source art
and animation/atlas inputs. Exercise collision, objective progression, loss and restart with fixed
seeds. Review silhouettes, palette/texel density, pivots/sorting, atlas bleed, loop seams and UI
safe areas. Engine follows the project; Spine is optional, not a prerequisite.

**3D — salvage room.** One room and short route, original blockout-to-finished assets, coherent
materials/lighting, player camera/movement, collect two objects, interact with exit, one obstacle,
win/lose/restart, UI and feedback/audio. Preserve Blender source where used, engine scene/scripts,
materials, import presets and build settings. Review scale/axes, normals/UVs/texel density,
collisions, camera, lighting and declared geometry/draw/texture budgets. Registered fixed-seed
runtime scenarios must observe real state, errors and frame timing, not injected success flags.

These briefs define matched acceptance, not invented benchmark results. Freeze actual target
hardware, settings and numeric time/performance gates from authorized P0 measurements **before**
evaluating the candidate. At least five paired trials initially; include failures/skips/rework and
manual intervention. Increase samples before tail-percentile claims. Measure first useful preview
and accepted output separately; model/network/approval/admission/import/compile/render waits,
rounds, bytes/tokens/cost where available, CPU/memory/GPU contention and cold/warm differences.
A fast preview is not a final-quality render. No speedup promise without paired results.

## Core contract and lifetime invariants

- Reuse Apps/MCP config/auth/transport and fixed run tool schemas. Capability discovery is data,
  not authorization; server names, `readOnlyHint`, JSON or prose cannot mint trusted receipts.
- Freeze adapter/version, authenticated connection, app/version/epoch, canonical project/document,
  revision, schema digest, operations, paths, owner and resource claims before approval. Preparation
  is pure: no network, spawn or write. Host revalidates before effects; never switch active document.
- Proposed tools: `app_inspect`, `app_apply`, `app_job`, `app_capture`. Do not enable them yet or
  leave generic privileged MCP aliases that bypass the new boundary. No hidden eval/script console.
  Ordinary reviewed script/shader/C# editing remains in existing file tools and execution policy.
- Persist intent before dispatch, using independent random operation ID and request digest. Adapter
  durably records that pair before effects. Lost response queries the original ID; never blind replay.
- Execution and physical effects are independent. Accepted != completed; cancel requested != stopped;
  partial may be physically settled but fails acceptance. Timeout/death/drop/lease expiry/missing
  history never unlock or prove rollback. Keep unknown unresolved across restart.
- Serialized IDs/status are not control authority. Runtime handles remain scoped and nonforgeable.
  Monotonic validated receipts bind operation/target/digest/owner/epoch. No restoration of image grants.
- Hold project/document writer and workspace/asset/GPU claims for the **entire** effect lifetime.
  Start with one writer/project and conservative heavy-render capacity. Keep generic MCP/stdio
  sidecar exclusion. Only owned bounded status/cancel/reconcile may pass cleanup barriers.
- Ordinary writes into a live project can trigger Unity imports, compilation, Editor initialization
  or other code. File-write return != host settlement. Bind the same scope and observe settlement,
  or refuse conflicting automation/use an explicitly authorized closed controlled fixture.
- Root/path/receipt/journal corruption fails closed. Bounded reads before allocation; strict schemas;
  no unresolved pruning to make space. Local cooperative path checks are not kernel isolation.
- Stop cooperatively while core lives. Never kill a user's editor. Force-stop preserves unknown
  external jobs. Disable submissions before removal; disabling never hides or discards unresolved jobs.

## Evidence, assets and quality

Reuse existing media CAS, disclosure grants, route/account/child isolation, successful occurrence
receipts and exact comparison semantics. Acquisition is not permission to upload. Preserve 8 MiB
per image and current 4-image/16-MiB selected-source envelope. Browser gates remain unchanged:
36 visual operations, four captures/pass and two repairs. Native budgets belong to a controller-owned
deliverable and survive task splitting/retries/children/turns/resume; the model cannot reset them.

Native static scene, animation samples and runtime evidence have distinct requirements; do not
force browser narrow/wide viewports onto them. Bind source/dependency and unsaved revisions,
evaluated/imported/play state, epoch, camera/frame/clock/settings, readiness and actual output hashes.
Independently checked bytes/fingerprints differ from host attestations; enrollment identifies the
reporter, not truth. Unknown stays unknown; mutations/imports/user edits invalidate affected evidence.

Preserve native editable sources. Publish exports to versioned staging paths, validate outputs,
then publish the manifest last. Track input/output hashes, tool/importer versions, units/axes,
pivots, color/material/skeleton conventions, presets, provenance/rights, losses and required notices.
No fictional cross-app rollback, opaque binary patching, automatic buys/uploads or overwrite of
later user edits. Application/runtime/plugin/asset/font/audio and source-distribution rights are
separate; core Apache-2.0 grants no vendor rights. Maintainer decides licensing questions.

## Support matrix and phased exits

| Phase | Deliverable and gate |
|---|---|
| P0 | Baseline, contracts, manageable 2D/3D briefs/rubrics; measured frozen comparison gates |
| P1 | Typed enrollment/preparation, owned jobs and whole-effect admission, cleanup/control, offline Apps JSON, deterministic failure harness; no real-app claim |
| P2 | Blender adapter on one recorded supported version: inspect/edit/material/export/preview, stale/partial/cancel/reconnect/file-load tests, reopen/reimport |
| P3 | Unity integration plus complete 2D and 3D slices; actual compile/reload/play/test/build/run, editable sources and separate visual/gameplay/device/human verdicts |
| P4 | Licensed version-pinned Spine CLI/data/atlas handoff and engine runtime tests; unsupported live rigging explicit |
| P5 | Supported Unreal editor/asset/level/sequence/tests and packaged runtime path; Blueprint-authoring limits explicit |
| P6 | Consolidated paired speed/quality, compatibility and regression/startup/size gates before any release request |

P0-P3 is the first reference unit, **not** permission to ship early. It must pass matched benchmark,
quality, security, regressions and size/startup gates before expansion. Blender -> Unity is an
architecture recommendation, not the user's engine choice. No app installed/licensed version or
actual compatibility has been established. All four live adapters are currently unavailable.

Host limits: Blender embedded `bpy` executes in appropriate host context (not arbitrary threads);
operators need mode/context and file loads invalidate bindings. Unity live automation needs an
Editor C# package; batchmode launches, not attaches, and must not open an already-owned project;
Undo does not cover filesystem/import/compile atomically. Unreal Python is Editor automation,
not cooked gameplay; Remote Control only exposes deliberately registered APIs, not universal
Blueprint editing. Spine CLI covers supported import/export/atlas/render, not universal live
weights/rig/keyframe authoring; render needs windowing/OpenGL and export/runtime versions match.

## Integration map and verification

Core: `src/agent/apps/`, existing `tools.rs`, `mcp.rs`, execution/effect handling and resource locks.
CLI: actual dispatcher `src/cli/apps.rs`, args `src/cli_args.rs`. Desktop later uses existing Apps,
JobsPane, StatusHeader, tool-use correlation/control stream and viewer, feature-detecting new fields.
No parallel event service, hidden listener, saved-image reselection or Browser redesign.

Test disconnect after submit, definitely-before-send vs ambiguous failure, duplicate/forged/stale
receipts, partial results, cancel races, scope/epoch/schema/path mismatch, restart/missing history,
journal capacity/corruption, root conflicts and offline no-side-effects. Include authenticated MCP
framing only when transport integration exists; pure state-machine tests do not cover it.
Regress browser/media/approval/hooks/sidecars/sessions/compact and relevant Desktop ownership.
Run focused/full Rust tests, scoped formatting/clippy, release/dense MSVC builds in background and
before/after startup/size. Native apps/human review/provider runs/install/release require their own
authorization; report skipped gates. No `cargo clean`, global toolchain override or secret reads.

## API references (entry points, not compatibility proof)

- [Unity Editor CLI](https://docs.unity.com/en-us/engine/6000.0/manual/unity-editor/command-line-arguments/editor)
  and [Test Framework](https://docs.unity3d.com/Packages/com.unity.test-framework@1.1/manual/reference-command-line.html).
- [Unreal Python](https://dev.epicgames.com/documentation/en-us/unreal-engine/scripting-the-unreal-editor-using-python)
  and [Remote Control](https://dev.epicgames.com/documentation/en-us/unreal-engine/remote-control-for-unreal-engine).
- [Blender API](https://docs.blender.org/api/current/info_quickstart.html),
  [timers](https://docs.blender.org/api/current/bpy.app.timers.html).
- [Spine CLI](https://esotericsoftware.com/spine-command-line-interface),
  [import limitations](https://esotericsoftware.com/spine-import),
  [version compatibility](https://esotericsoftware.com/spine-versioning).
