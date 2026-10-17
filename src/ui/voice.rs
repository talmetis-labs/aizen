//! Voice input (speech → prompt draft) — opt-in, compiled only with `--features voice`.
//!
//! Hold-to-talk on a function key (config `voice.hotkey`, default F9; alternate F10):
//! press opens a capture on the default input device; release closes it, downsamples
//! to 16 kHz mono WAV and hands it to a LOCAL sherpa-onnx sidecar:
//! - F9 → Zipformer-vi int8 74 MB (Apache-2.0, RTF ~0.06 — vi-focused, instant)
//! - F10 → streaming Zipformer-en int8 179 MB (Apache-2.0, en-2023-06-21 —
//!   ALL-CAPS output style like F9; needs the 2.6s trailing-silence pad)
//! The answer's `{"text": …}` lands in the prompt draft via [`tui::set_draft`]; it is
//! NEVER auto-sent — the user reviews it and presses Enter like a normal typed message.
//!
//! Posture notes (deliberate, see CLAUDE.md's single-binary contract):
//! - `cpal` is the only audio dependency: pure-Rust capture on supported targets.
//!   The 44-byte WAV/RIFF header is hand-written; the resample is a box-average
//!   anti-alias downsample (speech-grade); silence trimming prepares recordings for
//!   the models instead of imposing "practise speaking" on users.
//! - The STT engines NEVER link into this binary: both are external sidecar
//!   processes, so pure-Rust + rustls + single static binary all hold.
//! - No startup cost: nothing warms up until the first key press, and a default build
//!   (no `voice` feature) compiles none of this file at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossterm::event::KeyCode;
use once_cell::sync::Lazy;

use crate::core::cli_config;

const TARGET_RATE: u32 = 16_000;
/// Hard ceiling so a forgotten release can't grow the buffer forever.
const MAX_CAPTURE_SECS: u64 = 120;
const SHERPA_MODEL_DIR: &str = "sherpa-onnx-zipformer-vi-int8-2025-04-20";
const EN_MODEL_DIR: &str = "sherpa-onnx-streaming-zipformer-en-2023-06-21";

static LISTENING: AtomicBool = AtomicBool::new(false);

/// One live hold-to-talk capture, or `None` while idle. Mutated by the keyboard thread
/// (`toggle` on press, `finish` on release); `cpal::Stream` is `Send`, so the Mutex makes
/// it safe to park between the two.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

struct Session {
    /// Dropping the stream stops the capture (cpal contract).
    stream: cpal::Stream,
    /// Mono samples at the DEVICE rate, collected from the capture callback.
    data: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    /// First cpal error from the capture thread, surfaced on release.
    err: Arc<Mutex<Option<String>>>,
    /// Engine pinned at press; the release must decode with the same one.
    engine: Engine,
}

/// Which ASR backend a capture round will use. Pinned at press time in the session;
/// the release must decode with the SAME engine so F9/F10 can coexist independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Zipformer,
    En,
}

impl Engine {
    fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "en" => Self::En,
            _ => Self::Zipformer,
        }
    }
}

/// Parse a `voice.hotkey*` name ("F1"…"F12"); anything unknown falls back to `default`.
fn parse_hotkey(name: Option<&str>, default: u8) -> KeyCode {
    let name = name.unwrap_or_else(|| match default {
        10 => "F10",
        _ => "F9",
    });
    let name = name.trim().to_ascii_uppercase();
    if name.len() == 2 && name.starts_with('F') {
        if let Ok(n) = name[1..].parse::<u8>() {
            if (1..=12).contains(&n) {
                return KeyCode::F(n);
            }
        }
    }
    KeyCode::F(default)
}

static HOTKEY: Lazy<KeyCode> = Lazy::new(|| {
    parse_hotkey(
        cli_config::load()
            .voice
            .as_ref()
            .and_then(|v| v.hotkey.as_deref()),
        9,
    )
});

static ALT_HOTKEY: Lazy<KeyCode> = Lazy::new(|| {
    parse_hotkey(
        cli_config::load()
            .voice
            .as_ref()
            .and_then(|v| v.hotkey_alt.as_deref()),
        10,
    )
});

/// The primary capture key (F9), read by the input loop's press/release arms.
pub fn hotkey() -> KeyCode {
    *HOTKEY
}

/// The alternate capture key (F10) that pins the English libriheavy engine.
pub fn alt_hotkey() -> KeyCode {
    *ALT_HOTKEY
}

/// Whether a capture is live right now (gates the release event arm).
pub fn is_listening() -> bool {
    LISTENING.load(Ordering::Relaxed)
}

/// Press on either hotkey: open the capture pinned to that hotkey's engine (or no-op on
/// a key repeat while already holding).
fn toggle_engine(engine: Engine) {
    if is_listening() {
        return; // Windows key repeat re-delivers Press while held
    }
    let status = match engine {
        Engine::Zipformer => "🎙 nghe… (thả F9 để chuyển thành text)",
        Engine::En => "🎙 nghe English… (thả F10 để chuyển thành text)",
    };
    match start_capture(engine) {
        Ok(()) => {
            LISTENING.store(true, Ordering::Relaxed);
            crate::ui::tui::set_status(status);
        }
        Err(msg) => crate::ui::tui::note_line(&msg),
    }
}

pub fn toggle_primary() {
    toggle_engine(Engine::Zipformer)
}

pub fn toggle_alt() {
    toggle_engine(Engine::En)
}

/// Release: close the capture and hand the audio to the pinned engine sidecar; the
/// reply lands in the prompt draft.
pub fn finish() {
    if !LISTENING.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(sess) = SESSION.lock().unwrap().take() else {
        return;
    };
    drop(sess.stream); // stops the capture
    let restore_status = || crate::ui::tui::set_status("");

    if let Some(err) = sess.err.lock().unwrap().take() {
        restore_status();
        crate::ui::tui::note_line(&format!("🎙 thu âm lỗi: {err}"));
        return;
    }
    let raw = sess.data.lock().unwrap().clone();
    if raw.is_empty() || (raw.len() as u32) < sess.rate / 3 {
        restore_status();
        crate::ui::tui::note_line("🎙 bỏ qua: ghi quá ngắn");
        return;
    }

    let samples = trim_silence(&resample_to_16k(&raw, sess.rate), TARGET_RATE);
    let (mut energy, mut peak) = (0.0_f32, 0.0_f32);
    for s in &samples {
        energy += s * s;
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    let rms = (energy / samples.len().max(1) as f32).sqrt();
    if rms < 0.015 && peak < 0.05 {
        restore_status();
        crate::ui::tui::note_line(
            "🎙 âm thanh quá nhỏ để nhận dạng — hãy lại gần mic hơn hoặc bật mic louder",
        );
        return;
    }
    if rms > 0.45 || peak > 0.995 {
        restore_status();
        crate::ui::tui::note_line(
            "🎙 âm thanh vượt (clip) — lùi khỏi mic một chút hoặc giảm microphone boost rồi thử lại",
        );
        return;
    }

    // Streaming engines (F10 zipformer-en) never finalise a segment without a trailing
    // endpoint at EOF: append ~2.6s of silence so endpoint rule1 (min_trailing_silence
    // = 2.4s) fires and the transcript flushes. Measured on this architecture class
    // (Nemotron forensics): the 150ms residual left by trim_silence is NOT enough,
    // unflushed tail segments vanish. The offline F9 pass simply decodes through the
    // padding (a few extra ms) — pad only the En path.
    let engine = sess.engine;
    let samples = if engine == Engine::En {
        let mut padded = samples;
        padded.extend(std::iter::repeat(0.0_f32).take((TARGET_RATE as f64 * 2.6) as usize));
        padded
    } else {
        samples
    };
    let wav = wav_bytes(&samples, TARGET_RATE);
    // Save the exact engine input, under a unique name — doubles as the benchmark
    // input set (bench.ps1) and the debug artifact for quality reports. Remove the
    // whole block before the public release.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let bench_dir = std::env::temp_dir().join("aizen-voice-bench");
    let _ = std::fs::create_dir_all(&bench_dir);
    let bench_path = bench_dir.join(format!("utt-{stamp}.wav"));
    let _ = std::fs::write(&bench_path, &wav);
    // Storage hygiene: the debug captures must not bloat the user's drive. Keep only
    // the last 5 minutes of recordings (the transcribed prompt itself is already in
    // Aizen's session log); everything older goes with each new utterance.
    // (Release plan: this whole bench write becomes an explicit opt-in flag.)
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() - 5 * 60)
        .unwrap_or(u64::MAX);
    if let Ok(entries) = std::fs::read_dir(&bench_dir) {
        for e in entries.flatten() {
            if let Ok(meta) = e.metadata() {
                if meta.is_file()
                    && e.file_name().to_string_lossy().starts_with("utt-")
                    && meta
                        .modified()
                        .ok()
                        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|age| age.as_secs() < cutoff)
                        .unwrap_or(false)
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    crate::ui::tui::set_status("🎙 đang chuyển giọng thành text…");
    // Off the keyboard thread: the STT call must never freeze typing/scrolling.
    // The engine was pinned at PRESS time — F9 (zipformer-vi) and F10 (zipformer-en)
    // round-trip with exactly the engine they started with, even mid-flight hotkeys.
    std::thread::spawn(move || {
        let result = match engine {
            // zipformer-en: streaming English pack via the streaming CLI
            // (179MB int8; RTF 0.075 smoke-measured, exact decode on pack test wavs).
            Engine::En => {
                resolve_en().and_then(|(exe, model_dir)| run_en(&bench_path, &exe, &model_dir))
            }
            // zipformer-vi: instant offline subprocess (RTF ~0.06).
            Engine::Zipformer => resolve_sherpa()
                .and_then(|(exe, model_dir)| run_sherpa(&bench_path, &exe, &model_dir)),
        };
        match result {
            Ok(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    restore_status();
                    crate::ui::tui::note_line("🎙 không nghe thấy gì — nói to hơn hoặc thử lại");
                } else {
                    crate::ui::tui::set_draft(&trimmed);
                    crate::ui::tui::set_status("🎙 ✓ — xem lại rồi Enter");
                }
            }
            Err(e) => {
                restore_status();
                crate::ui::tui::note_line(&e);
            }
        }
    });
}

/// Build and play the input stream on the default device; store the session.
fn start_capture(engine: Engine) -> Result<(), String> {
    if SESSION.lock().unwrap().is_some() {
        return Ok(()); // already capturing (repeat edge); release will finalize
    }
    let voice = cli_config::load().voice;
    if !voice.as_ref().and_then(|v| v.enabled).unwrap_or(false) {
        return Err(format!(
            "🎙 Voice đang tắt — thêm \"voice\": {{\"enabled\": true}} vào ~/.aizen/cli-config.json"
        ));
    }

    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| "🎙 không tìm thấy micro".to_string())?;
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("🎙 mở micro thất bại: {e}"))?;
    let rate = cfg.sample_rate().0;
    let rate_usize = rate as usize;
    let channels = cfg.channels() as usize;
    let cpal_cfg = cpal::StreamConfig {
        channels: cfg.channels(),
        sample_rate: cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let data: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::with_capacity(
        rate_usize * MAX_CAPTURE_SECS as usize,
    )));
    let err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cap = rate_usize * MAX_CAPTURE_SECS as usize;

    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => {
            let (sink, eh) = (data.clone(), err.clone());
            device.build_input_stream(
                &cpal_cfg,
                move |frames: &[f32], _: &cpal::InputCallbackInfo| {
                    push(frames, channels, cap, &sink)
                },
                error_cb(eh),
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let (sink, eh) = (data.clone(), err.clone());
            device.build_input_stream(
                &cpal_cfg,
                move |frames: &[i16], _: &cpal::InputCallbackInfo| {
                    let conv: Vec<f32> = frames.iter().map(|s| *s as f32 / 32768.0).collect();
                    push(&conv, channels, cap, &sink)
                },
                error_cb(eh),
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let (sink, eh) = (data.clone(), err.clone());
            device.build_input_stream(
                &cpal_cfg,
                move |frames: &[u16], _: &cpal::InputCallbackInfo| {
                    let conv: Vec<f32> = frames
                        .iter()
                        .map(|s| (s.wrapping_sub(32768) as i16) as f32 / 32768.0)
                        .collect();
                    push(&conv, channels, cap, &sink)
                },
                error_cb(eh),
                None,
            )
        }
        other => {
            return Err(format!("🎙 micro dùng format không hỗ trợ: {other:?}"));
        }
    }
    .map_err(|e| format!("🎙 mở luồng ghi thất bại: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("🎙 bắt đầu ghi thất bại: {e}"))?;

    *SESSION.lock().unwrap() = Some(Session {
        stream,
        data,
        rate,
        err,
        engine,
    });
    Ok(())
}

/// Average a device frame down to one mono sample and keep it (until the cap).
fn push(frames: &[f32], channels: usize, cap: usize, sink: &Mutex<Vec<f32>>) {
    let mut buf = sink.lock().unwrap();
    if buf.len() >= cap {
        return; // forgotten release: keep the ceiling, drop the rest
    }
    for frame in frames.chunks_exact(channels) {
        buf.push(frame.iter().sum::<f32>() / channels as f32);
    }
}

fn error_cb(holder: Arc<Mutex<Option<String>>>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e: cpal::StreamError| {
        let mut g = holder.lock().unwrap();
        if g.is_none() {
            *g = Some(e.to_string());
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// sherpa-onnx release build dir name for the dev layout. Only win-x64 is known-good;
/// unix names follow the conventional sherpa-onnx release naming (not verified to exist).
fn sherpa_build_dir() -> String {
    if cfg!(windows) {
        "sherpa-onnx-v1.13.8-win-x64-static-MT-Release-no-tts".to_string()
    } else {
        let os = match std::env::consts::OS {
            "macos" => "mac",
            other => other,
        };
        format!(
            "sherpa-onnx-v1.13.8-{os}-{}-static-MT-Release-no-tts",
            std::env::consts::ARCH
        )
    }
}

/// Locate the Zipformer-vi sidecar: (exe, model dir). Search order — user-featured env,
/// installed layout, then the repo-adjacent dev layout.
fn resolve_sherpa() -> Result<(PathBuf, PathBuf), String> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("AIZEN_VOICE_HOME") {
        cands.push(PathBuf::from(dir));
    }
    if let Some(dir) = cli_config::load()
        .voice
        .as_ref()
        .and_then(|v| v.sherpa_dir.as_deref())
    {
        cands.push(PathBuf::from(dir));
    }
    if let Some(home) = home_dir() {
        cands.push(PathBuf::from(&home).join(".aizen").join("voice"));
        cands.push(
            PathBuf::from(&home)
                .join("aizen-voice-dev")
                .join(sherpa_build_dir()),
        );
    }

    for base in &cands {
        let exe_bin = base.join("bin").join(exe_name("sherpa-onnx-offline"));
        let exe_flat = base.join(exe_name("sherpa-onnx-offline"));
        let Some(exe) = [exe_bin, exe_flat].into_iter().find(|p| p.is_file()) else {
            continue;
        };
        let model_dev = base
            .parent()
            .map(|p| p.join(SHERPA_MODEL_DIR))
            .unwrap_or_else(|| base.join(SHERPA_MODEL_DIR));
        let model_home = base.join("model");
        let model = if model_dev.is_dir() {
            model_dev
        } else if model_home.is_dir() {
            model_home
        } else {
            continue;
        };
        return Ok((exe, model));
    }
    Err(
        "🎙 chưa tìm thấy sherpa-onnx (đặt AIZEN_VOICE_HOME hoặc ~/.aizen/voice với \
         sherpa-onnx-offline + model/)"
            .to_string(),
    )
}

/// Locate the F10 engine: (sherpa-onnx.exe streaming CLI, streaming-zipformer-en dir).
/// The EN slot runs the streaming binary — a different exe than the F9 offline one.
fn resolve_en() -> Result<(PathBuf, PathBuf), String> {
    let mut exe_cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("AIZEN_VOICE_HOME") {
        exe_cands.push(PathBuf::from(&dir).join(exe_name("sherpa-onnx")));
    }
    if let Some(home) = home_dir() {
        // Default `aizen voice install` layout: streaming CLI parked flat at the voice root.
        let voice_root = PathBuf::from(&home).join(".aizen").join("voice");
        exe_cands.push(voice_root.join(exe_name("sherpa-onnx")));
        exe_cands.push(voice_root.join("bin").join(exe_name("sherpa-onnx")));
        exe_cands.push(
            PathBuf::from(&home)
                .join("aizen-voice-dev")
                .join(sherpa_build_dir())
                .join("bin")
                .join(exe_name("sherpa-onnx")),
        );
    }
    let Some(exe) = exe_cands.into_iter().find(|p| p.is_file()) else {
        return Err(
            "🎙 F10 cần gói English (tùy chọn, ~484 MiB nén) — chưa cài trên máy này; \
             chạy `aizen voice install --with-english`; F9 vẫn hoạt động bình thường"
                .to_string(),
        );
    };

    let mut model_cands: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("AIZEN_VOICE_HOME") {
        model_cands.push(PathBuf::from(dir).join("model_en"));
    }
    if let Some(home) = home_dir() {
        model_cands.push(
            PathBuf::from(&home)
                .join("aizen-voice-dev")
                .join(EN_MODEL_DIR),
        );
        model_cands.push(
            PathBuf::from(&home)
                .join(".aizen")
                .join("voice")
                .join("model_en"),
        );
    }
    let Some(model_dir) = model_cands.into_iter().find(|p| p.is_dir()) else {
        return Err(
            "🎙 F10 cần gói English (tùy chọn, ~484 MiB nén) — model chưa tải; \
             chạy `aizen voice install --with-english`; F9 vẫn hoạt động bình thường"
                .to_string(),
        );
    };
    Ok((exe, model_dir))
}

/// En engine: run the streaming zipformer-en model over one wav via the streaming
/// sherpa CLI (flag combo + int8 files smoke-verified on the pack's own test wavs,
/// decode exact; output is ALL-CAPS like the vi engine).
fn run_en(
    wav: &std::path::Path,
    exe: &std::path::Path,
    model_dir: &std::path::Path,
) -> Result<String, String> {
    let out = std::process::Command::new(exe)
        .arg(format!(
            "--encoder={}",
            model_dir.join("encoder-epoch-99-avg-1.int8.onnx").display()
        ))
        .arg(format!(
            "--decoder={}",
            model_dir.join("decoder-epoch-99-avg-1.int8.onnx").display()
        ))
        .arg(format!(
            "--joiner={}",
            model_dir.join("joiner-epoch-99-avg-1.int8.onnx").display()
        ))
        .arg(format!(
            "--tokens={}",
            model_dir.join("tokens.txt").display()
        ))
        .arg("--num-threads=2")
        .arg(wav)
        .output()
        .map_err(|e| format!("🎙 chạy sherpa-onnx thất bại: {e}"))?;
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr);
        let tail: String = tail.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        return Err(format!("🎙 sherpa-onnx lỗi: {tail}"));
    }
    // The sherpa CLIs (offline + streaming) print the transcript JSON on STDERR (stdout
    // carries only early progress) — search both streams for the `{"text": …}` result line.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    for line in combined.lines() {
        let line = line.trim_start();
        if line.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    return Ok(text.to_string());
                }
            }
        }
    }
    Err("🎙 sherpa-onnx không trả về text".to_string())
}

/// Locate the Zipformer-vi file-decode runner: (exe, model dir); the F9 engine runs the
/// same way (one subprocess per utterance, model load ~0.5s).
/// Zipformer-vi runner: (sherpa-onnx-offline.exe, model dir) — one subprocess per utterance.
fn run_sherpa(
    wav: &std::path::Path,
    exe: &std::path::Path,
    model_dir: &std::path::Path,
) -> Result<String, String> {
    let out = std::process::Command::new(exe)
        .arg(format!(
            "--tokens={}",
            model_dir.join("tokens.txt").display()
        ))
        .arg(format!(
            "--encoder={}",
            model_dir.join("encoder-epoch-12-avg-8.int8.onnx").display()
        ))
        .arg(format!(
            "--decoder={}",
            model_dir.join("decoder-epoch-12-avg-8.onnx").display()
        ))
        .arg(format!(
            "--joiner={}",
            model_dir.join("joiner-epoch-12-avg-8.int8.onnx").display()
        ))
        .arg("--num-threads=2")
        .arg(wav)
        .output()
        .map_err(|e| format!("🎙 chạy sherpa-onnx thất bại: {e}"))?;
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr);
        let tail: String = tail.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        return Err(format!("🎙 sherpa-onnx lỗi: {tail}"));
    }
    // Both sherpa CLIs print the transcript JSON on STDERR (stdout carries only early
    // progress) — search both streams for the `{"text": …}` result line.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    for line in combined.lines() {
        let line = line.trim_start();
        if line.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    return Ok(text.to_string());
                }
            }
        }
    }
    Err("🎙 sherpa-onnx không trả về text".to_string())
}

/// Trim leading/trailing silence (energy gate) — the fix for short sparse replies like
/// "Được rồi OK": streaming models judge trailing silence as non-speech and return
/// nothing if the tail dominates. Keep a 150 ms breathing pad on each side. A clip whose
/// interior never exceeds the gate is returned untouched (the RMS hint reports it).
fn trim_silence(data: &[f32], rate: u32) -> Vec<f32> {
    const GATE: f32 = 0.02;
    const PAD_MS: u32 = 150;
    let mut first = data.len();
    let mut last_end = 0usize;
    for (i, s) in data.iter().enumerate() {
        if s.abs() > GATE {
            first = i;
            break;
        }
    }
    for (i, s) in data.iter().enumerate().rev() {
        if s.abs() > GATE {
            last_end = i + 1;
            break;
        }
    }
    if first >= last_end {
        return data.to_vec();
    }
    let pad = (rate / 1000 * PAD_MS) as usize;
    let start = first.saturating_sub(pad);
    let end = (last_end + pad).min(data.len());
    data[start..end].to_vec()
}

/// Anti-aliased downsample: box-average each output window. Nearest-neighbor folds
/// high-frequency speech content (sibilants s/x/ch/tr) into the aliasing band and
/// measurably hurts STT accuracy; a box filter is ~free and speech-grade. Replace with
/// a proper windowed-sinc FIR only if real-world quality still demands it.
fn resample_to_16k(data: &[f32], from: u32) -> Vec<f32> {
    if from == TARGET_RATE || data.is_empty() {
        return data.to_vec();
    }
    let n = (data.len() as u64 * TARGET_RATE as u64 / from as u64) as usize;
    let step = from as f64 / TARGET_RATE as f64; // e.g. 3.0 for 48 kHz input
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = (i as f64 * step) as usize;
        let end = (((i as f64 + 1.0) * step) as usize).min(data.len());
        if end <= start {
            out.push(data[start.min(data.len() - 1)]);
            continue;
        }
        let mut acc = 0.0_f32;
        for s in &data[start..end] {
            acc += *s;
        }
        out.push(acc / (end - start) as f32);
    }
    out
}

/// Canonical 44-byte RIFF header + 16-bit PCM mono LE samples.
fn wav_bytes(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1_u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2_u16.to_le_bytes()); // block align
    out.extend_from_slice(&16_u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}
