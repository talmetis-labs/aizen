# PROPOSAL: Voice Input for Aizen (F9/F10 speech → prompt)

**Status:** Prototype complete and user-validated on branch `feat/voice-input` (8 commits).
Awaiting design approval before Phase 2 (packaging). Estimated reading: 5 minutes.

---

## 1. Problem

Prompt entry is type-only. Speaking a prompt is 3–4× faster than typing it, and for
hands-off / long-context prompts the speed gap becomes the bottleneck of the whole
session. This adds a **second input method**: hold a key, speak, release — the
transcript lands in the prompt draft. The user reviews it and presses Enter, exactly
like typed input. Nothing downstream changes (agent core, sandbox, tools untouched).

## 2. User experience

```
Hold F9  → 🎙 recording → release → ~0.3 s → text in prompt draft → review → Enter  (Vietnamese)
Hold F10 → same, ~3–4 s → text in draft                                              (en + 40 locales)
```

- F9 = **Vietnamese-first engine** (Zipformer-vi); F10 = **NVIDIA Nemotron streaming**
  (multilingual). Engine is pinned per capture, so mixed hotkey use never cross-routes.
- Voice is **off by default** (`voice.enabled: false`); hotkeys configurable
  (`voice.hotkey`, `voice.hotkey_alt`).

## 3. Architecture (deliberately boring)

```
press F9/F10 ──► capture mic 16 kHz mono (cpal, pure Rust)
release     ──► box-average downsample + WAV written by hand (~90 lines)
            ──► external sherpa-onnx CLI subprocess decodes the WAV
            ──► transcript JSON → tui::set_draft() → user presses Enter
```

| Touchpoint | What |
|---|---|
| `src/ui/voice.rs` (new, gated) | capture, resample, WAV, subprocess, draft |
| `src/ui/tui.rs` (2 press arms + 1 release arm, gated) | hotkey interception in the prompt event loop |
| `src/core/cli_config.rs` | `VoiceConfig` schema (off by default) |
| `Cargo.toml` | `[features] voice = ["dep:cpal"]` — **zero impact on the default binary** |

Engines are **external sidecars, never linked** (pure-Rust + rustls + single static
binary posture preserved). No resident server: each utterance is one short-lived
subprocess.

## 4. Measured on the dev machine (not estimates)

| Metric | F9 Zipformer-vi | F10 Nemotron-0.6B |
|---|---|---|
| Decode RTF (CPU) | **0.02–0.06** (0.2–0.5 s) | 0.15–0.23 (0.4–7 s by length) |
| RAM peak while decoding | 270 MB @ 10 s clip | 764 MB (length-independent) |
| RAM resident after | **0** | **0** |
| Engine footprint on disk | 92 MB (model 74 + exe 18) | +650 MB optional install |
| Default binary size delta | **+0.11 MB incl. cpal** (30.06 → 30.17 MB) | n/a (not compiled) |
| Requirement | any CPU | any CPU (GPU explicitly not needed — see 8) |

## 5. Engine due diligence (why these two)

- **Zipformer-vi 74 MB, Apache-2.0** (VietASR, Interspeech 2025): SOTA Vietnamese
  (WER ~8–12% VLSP-class; beats PhoWhisper-1.5B and whisper-large-v3 on public sets),
  community-documented in sherpa-onnx official predtrained pages.
- **Nemotron-3.5-ASR-Streaming 0.6B, OpenMDW-1.1** ("ready for commercial use" per
  NVIDIA): vi+en+40 locales, no zh-leak, **streaming-capable** (Phase 2.5 upgrade path).
- Evaluated and rejected with data: whisper family (fast tiers unusable for vi;
  accurate tier 22 s/case CPU), Qwen3-ASR (zh-leak ×3 in UAT, heavier, weaker vi),
  PhoWhisper/parakeet-vi/ChunkFormer/hynt-30M (license or WER/speed fails),
  gipformer-68M (MIT; A/B identical on our data — kept on radar).
- **GPU route measured and dropped**: RTX 3080 with full CUDA/cuDNN runtime ran
  F10's decoder **no faster than CPU** (RTF ~0.7–1.1 vs 0.4–0.76) and degraded output —
  this feature stays CPU-only, which is also the multi-machine-friendly answer.

## 6. Known caveats (accepted for the prototype; Phase 2 backlog)

1. **Draft race:** typing manually while a transcript is in flight gets overwritten —
   guard planned (store draft snapshot at release, only fill if unchanged).
2. **No hold cap auto-finish:** forgetting to release stops at the 120 s buffer cap
   silently (RAM-safe, but the status label lingers) — auto-finish + note planned.
3. **F10 on legacy conhost:** the classic console menu-key swallows F10 — config
   escape exists (`hotkey_alt`), documented workaround `F11`.
4. **Installer not built yet** (`aizen voice install` reuses the updater's
   download/checksum machinery) — hand-install instructions for now.
5. Config parity: everything reads live except hotkeys (read once, cached).

## 7. Phase 2 (packaging, after approval)

1. `aizen voice install` (default: 92 MB vi bundle; `--with-multilingual`: +650 MB)
   with sha256 checksums, self-test decode, writes `~/.aizen/voice/` layout.
2. graceful F10 message when the optional bundle is absent (code already prepared).
3. OS matrix: Windows (done) / macOS (CoreAudio) / Linux (ALSA — like the existing
   arboard target-gating precedent; same feature gate keeps all three default builds).
4. bench-WAV debug capture becomes opt-in (currently 5-minute TTL hygiene).
5. Docs: `docs/REFERENCE.md` voice section + speaking guidelines (Common Voice-derived;
   no ASR vendor publishes speaking guidance — verified across Qwen/whisper/sherpa/VLSP).
6. Optional Phase 2.5: true realtime streaming transcript (Nemotron architecture
   already supports it).

## 8. Compliance checklist

- Pure-Rust single binary: cpal has no C toolchain; engines external → ✅
- rustls-only, no OpenSSL: ✅
- Default build unaffected (feature-gated): ✅ measured +0.11 MB on the voice build only
- Licenses: vi model Apache-2.0; multilingual model OpenMDW-1.1 (approver decision);
  no model is linked or redistributed inside the binary ✅
- rustfmt/clippy + `cargo test --bin aizen`: 2069 passed / 0 failed on this branch ✅
- No push to `main` — branch awaits review ✅

## 9. Decision requested

1. Feature-gated `voice` (default binary untouched) — **approve / reject**
2. Ship two engine bundles (default vi 92 MB + optional multilingual 650 MB) —
   **approve / reject**; if only one, keep the vi default and drop F10 to a stub
3. Multilingual engine license **OpenMDW-1.1** accepted as second-tier optional
   (not Apache/MIT — NVIDIA's commercial-ready open-weights license) — **yes / no**
