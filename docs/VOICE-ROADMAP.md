# VOICE-ROADMAP — shipping local voice input for Aizen

**Status:** execution plan, 2026-10-17. Supersedes the status section of
`PROPOSAL-voice-input.md` (the design rationale there still holds; its numbers about commits,
engines, and Phase-2 scope are stale). Scope: voice for **Aizen only** — not a standalone
dictation app, not a generic harness plugin.

**One sentence:** hold a key, speak, release — the transcript lands in the prompt draft, on
Windows, macOS and Linux, fully local, no account and no API key, installed by one command.

---

## 1. Why it stays inside Aizen

A standalone tool would have to type into whatever terminal has focus. On Windows that is
`SendInput`, on macOS it needs the Accessibility grant, on X11 it needs XTest, and on **Wayland it
is effectively impossible** (no global hotkey, no synthetic input without a compositor portal).
The feature is also 80% Aizen-specific already: it writes into the prompt draft, shares the TUI
event loop, the config file, the update channel and the status line.

So: the feature lives in Aizen. The thing that deserves its own repository is the **engine/model
distribution layer** (section 6) — that is the part that is data, not code, and the part that can
grow without a Rust release.

---

## 2. What already exists (measured 2026-10-17)

| Piece | Where | State |
|---|---|---|
| Capture → 16 kHz → trim → WAV → sidecar → draft | `src/ui/voice.rs` (684 lines) | Works on Windows; macOS/Linux never run |
| Hotkey arms in the prompt loop | `src/ui/tui.rs:2826`, `:2967` | Works; F9 + configurable F10 |
| Config schema (off by default) | `src/core/cli_config.rs:56`, `:167` | Works; no CLI/UI way to enable it |
| `aizen voice install` (fetch → sha256 → `tar.bz2` → layout) | `src/features/voice_install.rs` (480 lines) | Committed; 4 unit tests |
| Shared streaming download + checksum | `src/core/download.rs` (122 lines) | Committed; `features/update.rs` uses it |
| `tar` + `bzip2-rs` (pure Rust) | `Cargo.toml` `voice` feature | Committed |
| Installer platform matrix | `voice_install.rs:58` | windows/x86_64, macos/aarch64, linux/x86_64 — same as Aizen's release matrix |
| Tests for the capture/draft path | — | **0** |
| CI builds with `--features voice` | `.github/workflows/ci.yml` | Added (build + installer tests, 3 OS); first run pending |
| Release artifacts with voice | `.github/workflows/release.yml` | Ships a second `-voice` asset per platform; not released yet |
| Docs (README/REFERENCE/CHANGELOG) | — | **None** |

**Caveat, now fixed:** until this branch, `HEAD` + `#feature = "voice"` did not compile —
`src/main.rs:311` dispatched to `features::voice_install`, which existed only in the working tree.
It is committed now; what remains is CI proof on macOS and Linux, then M1.

### 2.1 Verified end-to-end (Windows, 2026-10-17)

`aizen voice install` was run against a clean `~/.aizen/voice` with the debug voice binary:

| Step | Result |
|---|---|
| Engine pack | 227.8 MiB downloaded, **SHA-256 verified** against GitHub's API digest |
| Vietnamese pack | 57.4 MiB downloaded, **SHA-256 verified** |
| Layout | `bin/sherpa-onnx-offline.exe`, flat `sherpa-onnx.exe`, `model/` with the exact encoder/decoder/joiner names `ui::voice` hard-codes |
| Decode with the installed engine | 6.23 s WAV → 0.141 s, **RTF 0.023**, valid `{"text": …}` JSON |

Two bugs surfaced by that run, both fixed in this branch:

1. **The 120 s whole-request timeout killed large downloads at a fixed byte.** A 228 MiB pack on a
   slow link cannot finish inside a total-request deadline; the first run died mid-body with
   "operation timed out". `core::download::http_client` now uses connect + read timeouts (a stalled
   connection still fails, a slow one keeps going), and a failed download deletes its part file
   instead of leaving 157 MB behind.
2. **The installer copied the whole developer bundle.** The win-x64 package ships 19 CLIs
   (302 MB) and 11 static `.lib` archives (993 MB): 1.39 GB on disk for the 35 MB of binaries the
   REPL ever spawns. `install_engine` now keeps exactly the two CLIs (offline + streaming) plus any
   runtime shared libraries, and drops link-time archives. Measured after the fix: **109.5 MB
   installed** (was 1,387 MB) — 74 MB model + 35 MB of engine binaries.

---

## 3. How voice must fit Aizen (integration map)

Aizen already has a mechanism for every one of these; voice should use it rather than invent a
parallel path.

| Concern | Aizen mechanism | What voice must do |
|---|---|---|
| Subcommand surface | `src/cli_args.rs:256` (`Commands::Voice`) | Grow from `install` to `install · doctor · enable · disable · status · uninstall` |
| Dispatch | `src/main.rs:311` | Same, `#[cfg(feature = "voice")]` |
| Config | `cli_config.rs` + `aizen config set --flag` | `--voice` flag (or `voice enable`) so nobody hand-edits JSON |
| Interactive config | `src/ui/config_ui.rs` sections (`:787`–`:1018`) | A `Voice` section: enable, hotkey, engine install state |
| Slash command | `src/features/slash.rs` | `/voice on|off|status` inside the REPL |
| Machine-readable health | `aizen sandbox doctor --json` (run in CI) | `aizen voice doctor --json`: mic list, engine files, decode self-test |
| Where report | `src/cli/where_report.rs:125` | Show `~/.aizen/voice` size + installed engines |
| Status line | `tui::set_status` (`tui.rs:1850`) | `🎙 listening` / `decoding` / `installed` |
| Draft write | `tui::set_draft` (`tui.rs:1934`) | Snapshot at release; fill only if the draft is unchanged (proposal §6.1) |
| Subprocess spawning | `sandbox::runner::prepare_std` contract, `docs/SANDBOX.md:70–81` | Either route through `InternalTrusted` with a literal reason, or add voice to the documented exception list (fixed argv, user-installed binary, no model-supplied strings) |
| Downloads | `core::download` | Reuse (done); add a progress callback for 60–484 MiB bodies |
| Update | `features/update.rs` (`DEFAULT_REPO`, asset suffix) | Variant-aware asset name when built with `voice` (section 5) |
| Release | `release.yml` matrix | Second asset per platform, or one default build (section 5) |
| Installers | `install.ps1`, `install.sh` | Optional `-Voice` / `--voice` flag that fetches the voice artifact and runs `aizen voice install` |
| Docs | `README.md` (<110 lines), `docs/REFERENCE.md` | Voice section + 3-command quickstart; CHANGELOG entry |
| i18n | English UI strings elsewhere | Convert the 25 Vietnamese messages in `voice.rs` to English (or make both languages explicit) |

---

## 4. Gaps, ranked

### P0 — without these, "everyone can install" is false

1. ~~**Commit the working tree.**~~ Done: `voice_install.rs`, `download.rs`, the `tar`/`bzip2-rs`
   deps and the resolver fixes are tracked, and `--features voice` compiles.
2. ~~**CI job with `--features voice` on all three OS.**~~ Done: the job BUILDS (links ALSA on
   Linux) and runs the installer tests; first run pending the branch being pushed.
3. **Self-test after install.** The installer checks `bin/sherpa-onnx-offline` exists and
   `tokens.txt` is present, but never decodes anything. A pack whose model file names drifted
   installs "successfully" and fails on the first F9. (The manual verification in §2.1 proves
   today's pack; it does not guard tomorrow's.)
4. **`voice doctor`** (human + `--json`): mic enumeration, engine/model file presence, 1-second
   silence decode, and a clear "what is missing + the command that fixes it".
5. **Enable path without hand-editing JSON.** `aizen voice enable` or `aizen config set --voice`,
   plus a config-UI row.
6. **Release artifacts** (section 5) — the workflow now emits a `-voice` asset per platform, but no
   tag has shipped one yet.
7. **Linux truth.** Fixed in `Cargo.toml`; still to do: `SANDBOX.md` and
   `REFERENCE.md` must state that a voice build needs `libasound2-dev` at build time and
   `libasound.so.2` at run time.
8. **Docs**: README one-liner + REFERENCE section + CHANGELOG entry; update the stale status in
   `PROPOSAL-voice-input.md`.

### P1 — makes it pleasant and supportable

9. **Progress reporting** for a 60–484 MiB download (bytes + %, not just "downloading …").
10. **`voice uninstall`** (the en pack is 521 MB on disk; today there is no way back).
11. **Draft-race guard** and **hold-cap auto-finish** (proposal §6.1–6.2).
12. ~~**Honest size strings**~~ Done: the F10 messages now say ~484 MiB compressed and
    name `aizen voice install --with-english`. The en pack is 521 MB extracted on disk.
13. **Tests for `voice.rs`**: resampler, silence trim, WAV header, hotkey parsing — all pure
    functions, no mic needed. A golden WAV → transcript test can run with a tiny fixture model
    in CI, or be `#[ignore]`d behind an env var.
14. **`aizen where`** line for the voice root (installed engines + bytes).
15. **Sandbox decision** (map row above) written into `SANDBOX.md`.
16. **Installer failure UX**: disk-space check before a 484 MiB download, clear message when a
    proxy/rate limit blocks the GitHub API (the code falls back to the fixed URL and loses the
    digest check — say that out loud, which it does).

### P2 — after the launch

17. Streaming transcript (Nemotron architecture already supports it) instead of one subprocess per
    utterance.
18. Model upgrade path (`voice install` today always re-downloads; no "already current" check).
19. macOS mic-permission onboarding copy (first run triggers the TCC prompt).
20. Codesigning/notarization for the sidecar binaries we ship.

---

## 5. Distribution: one product, two repos

```
        talmetis-labs/aizen                     talmetis-labs/aizen-voice
  ┌──────────────────────────────┐        ┌──────────────────────────────────┐
  │ aizen binary                 │        │ engine packs (slim, re-signed)   │
  │  + voice feature (variant)   │◄───────┤ model packs (vi, en)             │
  │  aizen voice install/doctor  │ manifest│ manifest.json + SHA256SUMS       │
  │  hotkeys → draft             │        │ CI packer + license notices      │
  └──────────────────────────────┘        └──────────────────────────────────┘
```

### 5.1 Which binary carries the feature?

| Option | Cost | Verdict |
|---|---|---|
| A. Enable `voice` in the default build (one artifact) | Links `libasound.so.2` on every Linux user, including headless servers/cron; `tar`+`bzip2-rs` always compiled | Revisit **after** a dlopen-based Linux capture exists |
| B. Separate `*-voice` artifact per platform, updater made variant-aware | 6 release builds instead of 3; ~10 lines in `update.rs` | **Recommended for v1** |

Variant mechanics: publish `aizen-<tag>-<plat>` and `aizen-<tag>-<plat>-voice`; `update.rs` appends
`-voice` when `cfg!(feature = "voice")`; `install.ps1 -Voice` / `install.sh --voice` fetch the
variant and then run `aizen voice install`.

### 5.2 Model/engine source

- **v1 (now):** direct from `k2-fsa/sherpa-onnx` (the installer already does this, with API
  digests and a `checksum.txt` fallback). Zero new infrastructure; ships fastest.
- **v2 (the separate repo):** our own slim packs + manifest. The upstream Windows tarball is
  228 MiB compressed / 1.3 GB extracted for two binaries we actually use; a slim repack is
  ~40–70 MiB, which materially changes install conversion.
- Aizen keeps the upstream path as a fallback when the manifest is unreachable.

---

## 6. The `aizen-voice` repository (the deliverable to show)

```
aizen-voice/
├── README.md                 # what it is, 3-command install, matrix, licenses, benchmark table
├── LICENSE                   # Apache-2.0 (the packs we build)
├── NOTICE                    # sherpa-onnx, onnxruntime, model provenance, upstream commits
├── manifest/
│   ├── schema.json           # versioned schema (schema: 1)
│   └── manifest.json         # generated at release, committed for auditability
├── packs/
│   ├── vi.toml               # upstream asset, sha256, license, install dir, self-test sample
│   └── en.toml
├── tools/
│   ├── pack-engine.sh        # fetch upstream → verify → strip → re-sign (macOS) → tar.bz2
│   ├── pack-model.sh
│   └── manifest.sh           # compose manifest.json + SHA256SUMS from built assets
├── docs/
│   ├── INSTALL.md            # per-OS, offline/air-gapped, troubleshooting
│   ├── ENGINES.md            # how to propose a new engine (license + WER + speed gates)
│   └── LICENSES.md
└── .github/
    ├── workflows/release.yml # tag `voice-v*` → build 3 platforms → assets + manifest
    └── ISSUE_TEMPLATE/       # bug, engine-request
```

Manifest shape (Aizen consumes this; unknown `schema` ⇒ fall back to upstream):

```json
{
  "schema": 1,
  "upstream": { "sherpa_onnx": "v1.13.8" },
  "engine": {
    "windows-x86_64": { "url": "…", "size": 50331648, "sha256": "…" },
    "macos-aarch64":  { "url": "…", "size": 48234496, "sha256": "…" },
    "linux-x86_64":   { "url": "…", "size": 52428800, "sha256": "…" }
  },
  "models": {
    "vi": { "url": "…", "size": 61234567, "sha256": "…", "license": "Apache-2.0" },
    "en": { "url": "…", "size": 507510784, "sha256": "…", "license": "Apache-2.0" }
  }
}
```

Release assets per tag: `aizen-voice-engine-<platform>.tar.bz2`, `aizen-voice-model-vi.tar.bz2`,
`aizen-voice-model-en.tar.bz2`, `manifest.json`, `SHA256SUMS`.

Why a separate repo is the right call:
- Engine bumps (sherpa-onnx, new models) ship without a Rust release or a `cargo` build.
- The upstream tarballs are 3–5× larger than what Aizen needs; repacking is the single biggest
  install-friction win.
- It is a small, reviewable, CI-only repo — a good thing to hand a reviewer or a manager: README,
  matrix, licenses, checksums, releases, no giant blobs in git.
- Third-party engines can be contributed without touching Aizen's source; a `packs/*.toml` entry is
  the contribution unit.

Licensing gates before publishing: each upstream binary and model must carry its license file into
the pack; vi is Apache-2.0, en must be confirmed against its model card; nothing is redistributed
without a `NOTICE` line and a provenance pin (upstream tag + asset sha256).

---

## 7. Milestones

| # | Outcome | Work | Acceptance | Est. |
|---|---|---|---|---|
| **M0** | Branch is real | ✅ Squashed onto `origin/dev` (0.7.2) as one reviewable commit | Local: default `cargo check`, `cargo build --features voice`, `cargo fmt --check`, 5 installer + 10 update tests, and the end-to-end install in §2.1. ⏳ 3-OS CI needs the branch pushed by an account with write access. | done |
| **M1** | Install → first word works | Self-test decode, `voice doctor [--json]`, enable/disable/uninstall, progress, English strings, `where` line, sandbox note | Clean Windows VM: `aizen voice install` then F9 produces text; `voice doctor` catches a deleted model file | 2–3 d |
| **M2** | Others can get it | Voice artifacts in `release.yml`, variant-aware updater, installer flags, README + REFERENCE + CHANGELOG | A stranger on macOS installs from the README in <5 min, with no build step | 1–2 d |
| **M3** | The separate repo | `aizen-voice` scaffold, packer, manifest, licenses, first `voice-v1.0.0`; Aizen reads the manifest with upstream fallback | Packs are 3–5× smaller; `voice install` against the manifest passes checksum + self-test; a second engine is added by editing one TOML | 3–5 d |
| **M4** | Community launch | 30-second demo (vi + en), README hero, landing-page sync, announcement posts, Discussions, 3 good-first-issues | Announcement published; install count visible in release download stats | 2–3 d |

Total to a public, installable feature: **~1 week** (M0–M2). The separate repo is **~1 week**
after that, and is what the launch writes about.

---

## 8. Community plan (what actually moves numbers)

- **Demo first.** Voice is a visual feature; a 30 s clip of "hold F9 → speak Vietnamese → agent
  runs the tests" beats any paragraph. Put it at the top of the README.
- **The hook:** local, offline, no account, no key — vs cloud dictation. Vietnamese quality is the
  differentiator: the vi model is 74 MB and decodes at 0.02–0.06 RTF (measured, proposal §4).
- **Honest benchmark table** in `aizen-voice/README.md`: model size, decode RTF, peak RAM,
  languages, license. No WER claims that were not measured on a stated set.
- **Where to post:** Show HN, r/LocalLLaMA, r/rust, X thread with the GIF, Vietnamese AI/ dev
  groups (the vi story is genuinely novel), Aizen Discussions.
- **Contribution ramp:** `packs/*.toml` engine proposals, docs translations, macOS/Linux testers.
  Label them good-first-issue; the repo stays small and low-risk.
- **Landing page sync:** `CLAUDE.md` already flags that the site drifts from releases; add voice
  to the release checklist.

---

## 9. Risks and honest limits

| Risk | Reality |
|---|---|
| Unsigned sidecar | Windows SmartScreen may warn; macOS Gatekeeper may block a repacked binary unless ad-hoc signed (`codesign -s -`). Budget a clean-VM test per OS. |
| Download size | vi ~60 MiB, en ~484 MiB. Add progress + resume, or most people abandon the en pack. |
| Linux is the weak platform | ALSA build dependency, no PipeWire-native path, headless servers have no `libasound.so.2`. Ship the variant, document it, revisit dlopen later. |
| Latency | One subprocess per utterance + ~0.5 s model load. Fine for prompts, not "realtime". Say so. |
| Update variants | A voice build must never silently update to a non-voice binary; that is why `update.rs` must be variant-aware in the same PR as the artifacts. |
| Trust | We will host third-party executables and model weights. Checksums + license files + pinned provenance are the minimum; say where every byte came from. |
| Microphone permission | macOS prompts once (TCC); a refused prompt looks like "voice does nothing". `voice doctor` must explain it. |

---

## 10. Decisions needed (with a recommendation)

1. **Distribution:** separate `-voice` artifacts (recommended) vs one default build. — decides M2.
2. **Repo host:** `talmetis-labs/aizen-voice` (recommended; consistent with the public repo) vs
   a personal account; and whether we redistribute upstream binaries/models at all (recommended:
   yes for vi Apache-2.0, en only after its model card is confirmed).
3. **Default engine set:** vi only in v1 (recommended) vs vi + en together. — decides download UX.
4. **Language of user-facing voice strings:** English (recommended, matches the rest of the CLI and
   the community-facing docs) vs Vietnamese, or both behind a config key.
