//! The Aizen landing splash — the block-art wordmark centred ABOVE one bordered panel that holds
//! the body (info, tools, commands) with the braille sun mark docked INSIDE on its right flank.
//! One cohesive moonlight-silver accent (no rainbow), rich structure. The layout is chosen
//! against the width the pane will ACTUALLY have (the retained sidebar's columns subtracted) —
//! the old docked layout measured the raw terminal instead, and the difference clipped the
//! panel's right edge. A pane too narrow for the flank stacks the sun above the wordmark.
//! Rendered once when you open the interactive menu (bare `aizen`). [`render`] builds it as a string
//! (so the sticky TUI can print it into its scroll region); [`print`] writes that string to stdout.

use crate::core::cli_config;
use console::{measure_text_width, style};
use std::fmt::Write as _;
use std::io::IsTerminal as _;

/// Silver gradient for the 5 title rows (near-white → darker grey), 256-color. The block-art wordmark
/// is metallic silver-white — the brightest note in the moonlight palette, the way the design renders
/// the AIZEN wordmark at #f5f4f0 over the dark ground.
const TITLE: [u8; 5] = [255, 253, 251, 248, 245];
/// The single brand accent — the moonlight silver from [`crate::ui::theme`], shared across the whole
/// TUI so headers / item names / the input box all speak one colour ("the one who holds the moon").
pub const ACCENT: u8 = crate::ui::theme::ACCENT;
const INNER: usize = 78;
/// Braille sun width in glyphs (`DW / 2` in [`sun_lines`]).
const SUN_W: usize = 32;
/// Total columns of the wide layout: frame chrome (4) + content column + gap + the sun docked on
/// the frame's right flank.
const WIDE_TOTAL: usize = 4 + INNER + 2 + SUN_W;

/// Agent tools, grouped (mirrors `agent::builtin` — keep in sync). Conditional/dynamic tools
/// (telegram_* when configured, mcp_<server>_<tool> from ~/.aizen/mcp.json) are not counted here.
const TOOL_GROUPS: &[(&str, &str)] = &[
    ("memory", "memory_search, memory_profile, memory_ask"),
    (
        "skills",
        "skill_load, skill_save, skill_search, skill_install",
    ),
    (
        "files",
        "file_read, file_glob, file_edit, file_write, search_files",
    ),
    ("shell", "shell_run, process"),
    ("web", "web_search, web_fetch, web_crawl"),
    ("tasks", "todo_write, clarify"),
    ("persona", "persona_create"),
    ("delegate", "task"),
];
/// Browser tools exist only under `--features browser`; listed (and counted) when present.
#[cfg(feature = "browser")]
const BROWSER_TOOLS: &str =
    "browser_navigate, browser_snapshot, browser_click, browser_type, browser_eval";
#[cfg(feature = "browser")]
const BROWSER_EXTRA: usize = 5;
#[cfg(not(feature = "browser"))]
const BROWSER_EXTRA: usize = 0;
const TOOL_COUNT: usize = 21 + BROWSER_EXTRA;

/// Top-level commands (mirrors `Commands` in main.rs, in declaration order).
const COMMANDS: &str =
    "chat · agent · workflow · memory · skill · persona · soul · bench · config · models · crawl · serve · telegram · discord · time · cron · mcp · apps";
const COMMAND_COUNT: usize = 18;

// ── block-art title ────────────────────────────────────────────────────────────

/// 5-row block glyphs (each exactly 5 columns) for the letters in AIZEN — the only wordmark
/// [`push_title`] is ever called with. Anything else renders as blanks by design.
fn glyph(c: char) -> [&'static str; 5] {
    match c {
        'A' => [" ███ ", "█   █", "█████", "█   █", "█   █"],
        'I' => ["█████", "  █  ", "  █  ", "  █  ", "█████"],
        'Z' => ["█████", "   █ ", "  █  ", " █   ", "█████"],
        'E' => ["█████", "█    ", "████ ", "█    ", "█████"],
        'N' => ["█   █", "██  █", "█ █ █", "█  ██", "█   █"],
        _ => ["     ", "     ", "     ", "     ", "     "],
    }
}

/// Fill the PetalMark into a `dw`×`dh` boolean pixel mask (true = petal). Resolution-aware so the mark
/// reads at every size: the high-res sixel raster (200²) keeps the true 16 long + 16 short thin rays of
/// the brand's chrysanthemum-sun, while a low-res char grid (braille, ≤120px) would shatter those thin
/// lances into scattered dots — there we draw a bolder 12-ray sun (thicker lances, larger solid core)
/// so it still reads as a sun instead of noise. Each ray is a tapered lance from the centre.
fn petal_mask(dw: usize, dh: usize) -> Vec<bool> {
    use std::f64::consts::PI;
    let mut grid = vec![false; dw * dh];
    let c = (dw as f64 / 2.0, dh as f64 / 2.0);
    let r = (dw.min(dh) as f64) / 2.0 - 1.0;

    let sign = |a: (f64, f64), b: (f64, f64), p: (f64, f64)| {
        (a.0 - p.0) * (b.1 - p.1) - (b.0 - p.0) * (a.1 - p.1)
    };
    let in_tri = |p: (f64, f64), a: (f64, f64), b: (f64, f64), cc: (f64, f64)| {
        let (d1, d2, d3) = (sign(a, b, p), sign(b, cc, p), sign(cc, a, p));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };

    // (petal count, length, half-width at the shoulder, angular offset in steps). High-res sixel keeps
    // the true 16 long + 16 short thin lances so the DARK GAPS between adjacent rays read clearly; a
    // low-res char grid (braille) can't resolve 32 thin rays without shattering into scattered dots, so
    // there we draw a bolder 12-ray single layer (thicker lances) that still reads as a clean sun.
    let hi_res = dw.min(dh) >= 128;
    let layers: &[(usize, f64, f64, f64)] = if hi_res {
        &[
            (16, r * 0.97, r * 0.026, 0.0),
            (16, r * 0.40, r * 0.018, 0.5),
        ]
    } else {
        &[(12, r * 0.95, r * 0.055, 0.0)]
    };
    for &(count, len, halfw, off) in layers {
        for i in 0..count {
            let ang = 2.0 * PI * ((i as f64 + off) / count as f64);
            let (s, co) = ang.sin_cos();
            let (d, perp) = ((co, s), (-s, co));
            let base = c;
            let tip = (c.0 + d.0 * len, c.1 + d.1 * len);
            // widest near the middle → a thin lance/leaf tapering to points at BOTH the tip and the
            // centre (a clean radial convergence, no central disc/"eye").
            let sh = (c.0 + d.0 * 0.5 * len, c.1 + d.1 * 0.5 * len);
            let rs = (sh.0 + perp.0 * halfw, sh.1 + perp.1 * halfw);
            let ls = (sh.0 - perp.0 * halfw, sh.1 - perp.1 * halfw);
            // scan only the petal's bounding box (keeps the high-res sixel fill fast)
            let xs = [base.0, tip.0, rs.0, ls.0];
            let ys = [base.1, tip.1, rs.1, ls.1];
            let x0 = xs.iter().cloned().fold(f64::MAX, f64::min).floor().max(0.0) as usize;
            let x1 = (xs.iter().cloned().fold(f64::MIN, f64::max).ceil() as usize).min(dw - 1);
            let y0 = ys.iter().cloned().fold(f64::MAX, f64::min).floor().max(0.0) as usize;
            let y1 = (ys.iter().cloned().fold(f64::MIN, f64::max).ceil() as usize).min(dh - 1);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let p = (x as f64 + 0.5, y as f64 + 0.5);
                    if in_tri(p, base, rs, tip) || in_tri(p, base, tip, ls) {
                        grid[y * dw + x] = true;
                    }
                }
            }
        }
    }
    // Solid core: the petals converge to a mathematical point, so the pixels BETWEEN their thin bases
    // can be left unfilled — a transparent pinhole that reads as a central "eye". Fill a disc so the
    // convergence is clean and solid (matches the reference: petals meeting at a filled centre). The
    // low-res mark needs a larger core so the 12 bolder lances knit into a solid hub, not a ragged star.
    let core = if hi_res { r * 0.035 } else { r * 0.11 };
    for y in 0..dh {
        for x in 0..dw {
            let (dx, dy) = (x as f64 + 0.5 - c.0, y as f64 + 0.5 - c.1);
            if dx * dx + dy * dy <= core * core {
                grid[y * dw + x] = true;
            }
        }
    }
    grid
}

/// The sun as Unicode-braille lines (2×4 dots/glyph) — the pure-text fallback when the terminal has
/// no inline-image support. Looks like a dotted approximation; sixel (below) is the smooth version.
fn sun_lines() -> Vec<String> {
    const DW: usize = 64;
    const DH: usize = 64;
    let grid = petal_mask(DW, DH);
    const BIT: [[u32; 4]; 2] = [[0, 1, 2, 6], [3, 4, 5, 7]];
    let mut lines = Vec::with_capacity(DH / 4);
    for cy in (0..DH).step_by(4) {
        let mut line = String::new();
        for cx in (0..DW).step_by(2) {
            let mut bits: u32 = 0;
            for dx in 0..2 {
                for dy in 0..4 {
                    if grid[(cy + dy) * DW + (cx + dx)] {
                        bits |= 1 << BIT[dx][dy];
                    }
                }
            }
            line.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
        }
        lines.push(line);
    }
    lines
}

fn sixel_rle(out: &mut String, ch: char, n: usize) {
    if n == 0 {
        return;
    }
    if n >= 4 {
        out.push('!');
        out.push_str(&n.to_string());
        out.push(ch);
    } else {
        for _ in 0..n {
            out.push(ch);
        }
    }
}

/// The sun as a TRUE raster image via the sixel protocol — smooth petals, moonlight silver, on a
/// transparent ground. This is the only way to show the actual vector logo in a terminal (Windows
/// Terminal & other sixel terminals). `petal_mask` rasterised at 200² then packed into 6-row sixel bands.
fn sun_sixel() -> String {
    const W: usize = 200;
    const H: usize = 200;
    let mask = petal_mask(W, H);
    let mut s = String::from("\x1bP0;1;0q"); // DCS … P2=1 ⇒ 0-bits stay transparent
                                             // Raster attributes "Pan;Pad;Ph;Pv = 1:1 pixel aspect (else P1=0 means 2:1 → the sun renders
                                             // stretched/oval) + the image size. Proven libsixel pattern; honoured by Windows Terminal et al.
    let _ = write!(s, "\"1;1;{W};{H}");
    s.push_str("#1;2;82;83;85"); // register colour 1 = moonlight silver #d2d4d9 (sixel uses 0–100 RGB)
    for band in (0..H).step_by(6) {
        s.push_str("#1");
        let (mut run_ch, mut run_n) = ('\u{0}', 0usize);
        for x in 0..W {
            let mut bits = 0u8;
            for row in 0..6 {
                let y = band + row;
                if y < H && mask[y * W + x] {
                    bits |= 1 << row;
                }
            }
            let ch = (0x3F + bits as u32) as u8 as char;
            if run_n > 0 && ch == run_ch {
                run_n += 1;
            } else {
                sixel_rle(&mut s, run_ch, run_n);
                run_ch = ch;
                run_n = 1;
            }
        }
        sixel_rle(&mut s, run_ch, run_n);
        s.push('-'); // graphics newline (next band)
    }
    s.push_str("\x1b\\"); // ST — end the DCS
    s
}

/// Terminals known NOT to decode sixel, identified positively rather than by absence of a flag.
///
/// Each of these self-identifies through its own variable, which is what makes the check reliable in
/// the face of inherited environment: `TERM_PROGRAM=vscode` is set by VS Code's own terminal
/// integration for the shell it spawns, so it describes the emulator actually reading our stdout.
///
/// Deliberately a DENY list, not "allow only what I recognise". A terminal we've never heard of that
/// does support sixel keeps working (and `AIZEN_LOGO=sixel` forces it); the failure mode we must
/// prevent is the opposite one — dumping megabytes at an emulator that will stall parsing it.
fn is_sixel_denied() -> bool {
    sixel_denied_in(|k| std::env::var(k).ok())
}

/// The deny decision as a pure function of the environment, so it can be unit-tested without
/// mutating process-wide state (`set_var` is unsound under a parallel test runner).
fn sixel_denied_in(get: impl Fn(&str) -> Option<String>) -> bool {
    // VS Code / Cursor / Windsurf and other VS Code forks: xterm.js, no sixel (`TERM_PROGRAM=vscode`).
    // Apple Terminal.app: no sixel (its `TERM_PROGRAM` is a distinct value from iTerm2's).
    if let Some(tp) = get("TERM_PROGRAM") {
        let tp = tp.to_ascii_lowercase();
        if tp.contains("vscode") || tp.contains("cursor") || tp.contains("windsurf") {
            return true;
        }
        if tp == "apple_terminal" {
            return true;
        }
    }
    // JetBrains IDEs (IDEA/PyCharm/CLion terminal): `TERMINAL_EMULATOR=JetBrains-JediTerm`, no sixel.
    if get("TERMINAL_EMULATOR").is_some_and(|v| v.contains("JediTerm")) {
        return true;
    }
    // VS Code also exports these for its integrated shell; belt-and-braces for when a shell profile
    // overwrites TERM_PROGRAM (common with starship / oh-my-posh setups).
    get("VSCODE_INJECTION").is_some() || get("VSCODE_GIT_IPC_HANDLE").is_some()
}

/// Logo mode. `AIZEN_LOGO=sixel|braille` forces it. Otherwise: an explicit DENY list of terminals
/// known to lack sixel, then auto-detect terminals KNOWN to render it — cross-platform (Windows /
/// Linux / macOS) — falling back to braille everywhere else (raw sixel on a terminal that can't read
/// it shows escape-code garbage, so we only emit it when confident). A DA1 capability probe would be
/// more precise but needs raw-mode timed stdin reads; this env heuristic covers the common cases with
/// zero risk. Tests always use braille.
///
/// NOTE for Linux: the default **Ubuntu GNOME Terminal (VTE) does NOT support sixel**, so it gets the
/// braille fallback. Sixel-capable Linux terminals — **foot, WezTerm, mlterm, recent xterm, Konsole
/// ≥22.04, contour** — show the real image (auto-detected where they self-identify, else
/// `AIZEN_LOGO=sixel`). macOS: **iTerm2** yes; Terminal.app no.
pub(crate) fn logo_is_sixel() -> bool {
    if cfg!(test) {
        return false;
    }
    match std::env::var("AIZEN_LOGO").ok().as_deref() {
        Some("sixel") => return true,
        Some("braille") | Some("text") | Some("off") => return false,
        _ => {}
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    // ── Explicit DENY list, checked BEFORE any allow rule ──────────────────────────────
    // An embedded terminal that cannot decode sixel must never be handed one, and the deny
    // must win over the `WT_SESSION` allow below: environment variables are INHERITED, so
    // launching VS Code (or any editor) from a Windows Terminal window leaves `WT_SESSION`
    // set in every child terminal it opens. The variable therefore proves what the ANCESTOR
    // was, never what is actually parsing our bytes.
    //
    // Cost of getting this wrong is not a cosmetic glitch. The screensaver blits a
    // fullscreen sixel sized to the viewport: measured on this box, ~0.55 MB at 640x360,
    // 1.9 MB at 1264x684, and 3.0 MB at 1912x1044 — one DCS string, written in a single
    // `write_all` + flush. Windows Terminal parses that natively. VS Code's terminal is
    // xterm.js (JavaScript, in the renderer process) with NO sixel support, so it walks the
    // whole payload through its escape-sequence state machine looking for the terminating
    // ST, on the UI thread, and discards it. That is the freeze — and because the render
    // thread owns the alt-screen `Stdout`, a stalled writer blocks every later frame behind
    // it. Nothing to do with the network: what looks like "requests are slow" is streamed
    // output queued behind a terminal still chewing on megabytes of graphics it cannot draw.
    if is_sixel_denied() {
        return false;
    }
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        if tp == "WezTerm" || tp == "iTerm.app" {
            return true;
        }
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return true; // Windows Terminal
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.starts_with("foot") || term == "mlterm" || term.contains("sixel") {
        return true;
    }
    // Konsole gained sixel in 22.04 → KONSOLE_VERSION like "220400".
    if let Ok(kv) = std::env::var("KONSOLE_VERSION") {
        if kv
            .trim()
            .parse::<u32>()
            .map(|v| v >= 220400)
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Print the sun mark: a real sixel image where supported, else the braille rows centred over
/// `width`. `allow_sixel` is false for the retained backend, whose alt-screen renderer can't pass
/// a raw DCS image through — there we always take the braille path so the intro is pure printable
/// text.
fn push_sun(out: &mut String, allow_sixel: bool, width: usize) {
    if allow_sixel && logo_is_sixel() {
        out.push_str("       "); // nudge the image toward the wordmark's centre
        out.push_str(&sun_sixel());
        out.push('\n');
    } else {
        for line in sun_rows(width) {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// The braille sun as styled rows centred over `width` — the stacked shape the narrow landing
/// screen and the welcome banner print verbatim (the wide landing screen instead docks the raw
/// [`sun_lines`] inside its frame, on the right flank).
fn sun_rows(width: usize) -> Vec<String> {
    let ind = " ".repeat(width.saturating_sub(SUN_W) / 2);
    sun_lines()
        .iter()
        .map(|l| format!("{ind}{}", style(l).color256(ACCENT).bold()))
        .collect()
}

/// The block-art wordmark (silver gradient) + tagline as styled rows centred over `width`.
fn wordmark_rows(word: &str, width: usize) -> Vec<String> {
    let w = word.chars().count() * 6 - 1; // 5-col glyphs joined by 1-col gaps
    let ind = " ".repeat(width.saturating_sub(w) / 2);
    let mut rows: Vec<String> = (0..5)
        .map(|row| {
            let line: String = word
                .chars()
                .map(|c| glyph(c)[row])
                .collect::<Vec<_>>()
                .join(" ");
            format!("{ind}{}", style(line).color256(TITLE[row]).bold())
        })
        .collect();
    const TAGLINE: &str = "ARTIFICIAL INTELLIGENCE AGENT";
    let ind = " ".repeat(width.saturating_sub(TAGLINE.chars().count()) / 2);
    rows.push(format!(
        "{ind}{}",
        style(TAGLINE).color256(crate::ui::theme::MUTED)
    ));
    rows
}

/// Append the Aizen logo for the unframed welcome banner: the sun mark above, then the centred
/// block-art wordmark + tagline.
fn push_title(out: &mut String, word: &str, allow_sixel: bool) {
    out.push('\n');
    push_sun(out, allow_sixel, INNER + 4);
    out.push('\n');
    for r in wordmark_rows(word, INNER + 4) {
        let _ = writeln!(out, "{r}");
    }
    out.push('\n');
}

// ── bordered panel ───────────────────────────────────────────────────────────

fn rule(out: &mut String, left: &str, right: &str, inner: usize) {
    // The frame sits one step quieter than the content so the panel reads as a calm moonlit border,
    // not a bright cage (the design keeps panel chrome near-invisible).
    let _ = writeln!(
        out,
        "{}",
        style(format!("{left}{}{right}", "─".repeat(inner + 2)))
            .color256(crate::ui::theme::ACCENT_DIM)
    );
}

/// Border the body rows into the moonlit panel. With `sun`, the raw braille mark (32-glyph rows
/// from [`sun_lines`]) docks INSIDE the frame on its right flank, vertically centred against the
/// body — the text column keeps its full `INNER` width, so the panel is content wall-to-wall
/// instead of a narrow card floating in a wide window. Content is padded ANSI-aware
/// (`measure_text_width`) so colour codes never throw off the right border.
fn frame(rows: &[String], sun: Option<&[String]>) -> String {
    let extra = if sun.is_some() { 2 + SUN_W } else { 0 };
    let mut out = String::new();
    rule(&mut out, "╭", "╮", INNER + extra);
    let total = rows.len().max(sun.map_or(0, |s| s.len()));
    let off = sun.map_or(0, |s| total.saturating_sub(s.len()) / 2);
    let b = style("│").color256(crate::ui::theme::ACCENT_DIM);
    for i in 0..total {
        let row = rows.get(i).map(String::as_str).unwrap_or("");
        let pad = " ".repeat(INNER.saturating_sub(measure_text_width(row)));
        match sun {
            Some(s) => {
                let flank = match i.checked_sub(off).and_then(|j| s.get(j)) {
                    Some(l) => style(l).color256(ACCENT).bold().to_string(),
                    None => " ".repeat(SUN_W),
                };
                let _ = writeln!(out, "{b} {row}{pad}  {flank} {b}");
            }
            None => {
                let _ = writeln!(out, "{b} {row}{pad} {b}");
            }
        }
    }
    rule(&mut out, "╰", "╯", INNER + extra);
    out
}

/// Pack `sep`-separated items into as few lines as possible, each (visible) no wider than `avail`.
/// Used for the Commands row so a long list wraps inside the box instead of spilling past the border.
fn wrap_items(items: &str, sep: &str, avail: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for item in items.split(sep) {
        let candidate = if cur.is_empty() {
            item.to_string()
        } else {
            format!("{cur}{sep}{item}")
        };
        if !cur.is_empty() && measure_text_width(&candidate) > avail {
            lines.push(std::mem::take(&mut cur));
            cur = item.to_string();
        } else {
            cur = candidate;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Build the whole landing screen (centred wordmark over one panel, the sun on the panel's right
/// flank) as a string. The sun mark uses a sixel image where the terminal supports it
/// (classic/plain output can pass a DCS payload through); the plain path lays out against the raw
/// terminal, which IS its pane.
pub fn render() -> String {
    let avail = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(0);
    render_inner(true, avail)
}

/// Text-only landing screen for the RETAINED backend: its alt-screen renderer sanitizes CSI but a
/// raw sixel DCS image would survive as garbage, so the sun is always the braille approximation and
/// nothing DCS-bearing reaches the frame. Laid out against the width the transcript pane WILL have
/// once the backend takes the grid (sidebar subtracted) — measuring the raw terminal here is
/// exactly what used to push the panel's right edge under the sidebar and clip it.
pub fn render_text_only() -> String {
    render_inner(false, crate::ui::tui::splash_width())
}

fn render_inner(allow_sixel: bool, avail: usize) -> String {
    let mut out = String::new();
    out.push('\n');
    // The wordmark sits OUTSIDE the panel, centred over its full width; the braille sun docks
    // INSIDE the panel on its right flank when `avail` can hold the wide layout. Two suns cannot
    // take that flank: a sixel sun is a raster DCS (pixels, not columns — it cannot be merged into
    // a character frame) and stays stacked above; and a pane below WIDE_TOTAL stacks the braille
    // sun above the wordmark rather than letting the flank clip off the pane's right edge.
    let sixel_sun = allow_sixel && logo_is_sixel();
    let sun_right = !sixel_sun && avail >= WIDE_TOTAL;
    let frame_w = if sun_right { WIDE_TOTAL } else { INNER + 4 };
    if sixel_sun {
        push_sun(&mut out, true, frame_w);
        out.push('\n');
    } else if !sun_right {
        for line in sun_rows(frame_w) {
            let _ = writeln!(out, "{line}");
        }
        out.push('\n');
    }
    for r in wordmark_rows("AIZEN", frame_w) {
        let _ = writeln!(out, "{r}");
    }
    out.push('\n');

    let sun = sun_right.then(sun_lines);
    out.push_str(&frame(&body_rows(), sun.as_deref()));
    out
}

/// The panel body — endpoint info, tools, commands — as plain rows ([`frame`] adds the borders).
fn body_rows() -> Vec<String> {
    let cfg = cli_config::load();
    let model = cfg.model.as_deref().unwrap_or("(not set)");
    let endpoint = cfg.base_url.as_deref().unwrap_or("(not set)");
    let key = if cfg.api_key.is_some() {
        style("configured").color256(ACCENT).to_string()
    } else {
        style("not set").red().to_string()
    };

    let mut rows: Vec<String> = Vec::new();
    rows.push(format!(
        "{} {}",
        style("Aizen").color256(ACCENT).bold(),
        style(format!(
            "v{} · {} · {}",
            env!("CARGO_PKG_VERSION"),
            model,
            endpoint
        ))
        .dim()
    ));
    rows.push(format!("{} {key}", style("key:").dim()));
    rows.push(String::new());

    rows.push(format!(
        "{}{}",
        crate::ui::icons::g(crate::ui::icons::hdr_tools()),
        style("Agent tools").color256(ACCENT).bold()
    ));
    for (label, items) in TOOL_GROUPS {
        rows.push(format!(
            "  {}{} {}",
            crate::ui::icons::g(crate::ui::icons::tool_group(label)),
            style(format!("{label:<9}")).dim(),
            style(*items).color256(ACCENT)
        ));
    }
    #[cfg(feature = "browser")]
    for line in wrap_items(BROWSER_TOOLS, ", ", INNER.saturating_sub(14)) {
        rows.push(format!(
            "  {}{} {}",
            crate::ui::icons::g(crate::ui::icons::tool_group("browser")),
            style(format!("{:<9}", "browser")).dim(),
            style(line).color256(ACCENT)
        ));
    }
    rows.push(String::new());

    rows.push(format!(
        "{}{}",
        crate::ui::icons::g(crate::ui::icons::hdr_commands()),
        style("Commands").color256(ACCENT).bold()
    ));
    // Indent is 2 cols → wrap to INNER-2 so styled lines never overrun the right border.
    for line in wrap_items(COMMANDS, " · ", INNER - 2) {
        rows.push(format!("  {}", style(line).color256(ACCENT)));
    }
    rows.push(String::new());

    rows.push(
        style(format!("{TOOL_COUNT} tools · {COMMAND_COUNT} commands · type to chat · /help · Esc/Ctrl-C to exit"))
            .dim()
            .to_string(),
    );
    rows
}

/// Render the landing screen to stdout (plain line-REPL path).
pub fn print() {
    print!("{}", render());
}

/// First-run welcome banner: the block-art logo + a short value prop. Shown ONCE to a brand-new
/// user (no endpoint configured yet) right before the setup wizard — the "intro" before onboarding.
pub fn welcome() -> String {
    let mut out = String::new();
    push_title(&mut out, "AIZEN", true);
    // Prose lines centred under the centred logo (indent computed on the PLAIN text — styling
    // would inflate the measure).
    let center = |text: &str| {
        format!(
            "{}{text}",
            " ".repeat(INNER.saturating_sub(measure_text_width(text)) / 2)
        )
    };
    let _ = writeln!(
        out,
        "{}",
        style(center("Welcome to Aizen — your agentic coding companion."))
            .color256(ACCENT)
            .bold()
    );
    let _ = writeln!(
        out,
        "{}",
        style(center(
            "One fast binary: chat · tools · automation · a memory that learns you."
        ))
        .dim()
    );
    let _ = writeln!(
        out,
        "{}",
        style(center("Let's get you connected — about 30 seconds.")).dim()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The displayed counts must match the displayed lists — else the splash lies about its own
    /// surface. (Guards against editing one and forgetting the other.)
    #[test]
    fn counts_match_listed_names() {
        let tools: usize = TOOL_GROUPS
            .iter()
            .map(|(_, items)| items.split(", ").count())
            .sum::<usize>()
            + BROWSER_EXTRA;
        assert_eq!(tools, TOOL_COUNT, "TOOL_COUNT out of sync with TOOL_GROUPS");
        assert_eq!(
            COMMANDS.split(" · ").count(),
            COMMAND_COUNT,
            "COMMAND_COUNT out of sync with COMMANDS"
        );
        // Mirror of the top-level `Commands` enum in main.rs — every variant must be listed so a new
        // subcommand can't be added without surfacing on the splash (the `apps` omission regression).
        for c in [
            "chat", "agent", "workflow", "memory", "skill", "persona", "soul", "bench", "config",
            "models", "crawl", "serve", "telegram", "discord", "time", "cron", "mcp", "apps",
        ] {
            assert!(
                COMMANDS.split(" · ").any(|x| x == c),
                "command '{c}' missing from splash COMMANDS"
            );
        }
    }

    /// The retained backend sanitizes CSI but would pass a raw sixel DCS image through as garbage, so
    /// `render_text_only` must never emit a Device Control String (`ESC P` … `ESC \`). The interactive
    /// `render` MAY (classic/plain output handles it), so we only assert the text-only variant is clean.
    #[test]
    fn text_only_splash_has_no_sixel_dcs() {
        // Colour CSI codes are fine (the retained backend strips them); a DCS image is not — its body
        // is printable and would survive sanitization as garbage. Assert only that no DCS is opened.
        let out = render_text_only();
        assert!(
            !out.contains("\u{1b}P"),
            "retained intro must not open a sixel DCS"
        );
        assert!(
            !out.contains("\u{1b}\\"),
            "retained intro must not contain a DCS string terminator"
        );
    }

    /// The sixel DENY list must fire for every embedded terminal we know cannot decode a DCS, and
    /// must NOT fire for the ones that can.
    ///
    /// The load-bearing case is the last one: `WT_SESSION` is INHERITED, so launching VS Code from a
    /// Windows Terminal window leaves it set in the integrated terminal's environment. Before the deny
    /// list, that alone said "sixel supported" and the screensaver blitted megabytes at xterm.js, which
    /// walks the whole payload looking for the ST and draws nothing — the freeze the user reported as
    /// "requests are slow in VS Code but fine in cmd".
    #[test]
    fn sixel_deny_list_covers_embedded_terminals() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| {
                owned
                    .iter()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.clone())
            }
        };

        // Denied: VS Code and its forks, JetBrains, Apple Terminal.
        assert!(sixel_denied_in(env(&[("TERM_PROGRAM", "vscode")])));
        assert!(
            sixel_denied_in(env(&[("TERM_PROGRAM", "Cursor")])),
            "case-insensitive fork match"
        );
        assert!(sixel_denied_in(env(&[("TERM_PROGRAM", "windsurf")])));
        assert!(sixel_denied_in(env(&[("TERM_PROGRAM", "apple_terminal")])));
        assert!(sixel_denied_in(env(&[(
            "TERMINAL_EMULATOR",
            "JetBrains-JediTerm"
        )])));
        // Denied via the belt-and-braces vars, for a shell profile that overwrote TERM_PROGRAM.
        assert!(sixel_denied_in(env(&[
            ("TERM_PROGRAM", "xterm"),
            ("VSCODE_INJECTION", "1")
        ])));
        assert!(sixel_denied_in(env(&[(
            "VSCODE_GIT_IPC_HANDLE",
            r"\\.\pipe\vscode-git-x"
        )])));

        // Allowed: real sixel terminals, and a bare environment.
        assert!(!sixel_denied_in(env(&[("TERM_PROGRAM", "WezTerm")])));
        assert!(!sixel_denied_in(env(&[("TERM_PROGRAM", "iTerm.app")])));
        assert!(
            !sixel_denied_in(env(&[("WT_SESSION", "abc")])),
            "plain Windows Terminal keeps sixel"
        );
        assert!(!sixel_denied_in(env(&[])));

        // THE REGRESSION: VS Code launched from Windows Terminal inherits WT_SESSION. Deny must win.
        assert!(
            sixel_denied_in(env(&[("WT_SESSION", "abc"), ("TERM_PROGRAM", "vscode")])),
            "an inherited WT_SESSION must not re-enable sixel inside VS Code"
        );
    }

    /// Every bordered line must be exactly the layout's one width — a wider row means content
    /// overran the right border (the Commands-row overflow regression), a narrower one a broken
    /// flank. Both layouts, deterministically: wide (sun on the right flank) and narrow (stacked).
    #[test]
    fn no_boxed_line_overflows() {
        for (avail, want) in [(200usize, WIDE_TOTAL), (80, INNER + 4)] {
            let widths: Vec<usize> = render_inner(false, avail)
                .lines()
                .filter(|l| l.contains('│'))
                .map(measure_text_width)
                .collect();
            assert!(!widths.is_empty());
            assert!(
                widths.iter().all(|w| *w == want),
                "boxed rows drift at avail={avail}: {widths:?}"
            );
        }
    }

    /// The wide layout: wordmark ABOVE the frame centred over its full width, and the braille sun
    /// INSIDE the frame on its right flank — never past the pane's width (the "hidden text" bug —
    /// the old docked layout sized itself against the full terminal while the retained transcript
    /// pane is narrower once the sidebar docks; the layout now receives the pane's width).
    #[test]
    fn wide_layout_centres_wordmark_above_and_docks_sun_right() {
        let out = render_inner(false, 200);
        let plain: Vec<String> = out
            .lines()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect();
        let top = plain
            .iter()
            .position(|l| l.contains('╭'))
            .expect("top rule");
        let bottom = plain
            .iter()
            .position(|l| l.contains('╰'))
            .expect("bottom rule");
        // Wordmark above the frame, centred over WIDE_TOTAL columns.
        let word = plain
            .iter()
            .position(|l| l.contains("█████"))
            .expect("wordmark row");
        assert!(word < top, "wordmark must sit above the frame");
        let tag = plain
            .iter()
            .position(|l| l.contains("ARTIFICIAL INTELLIGENCE AGENT"))
            .expect("tagline row");
        let ind = plain[tag].chars().take_while(|c| *c == ' ').count();
        let want = (WIDE_TOTAL - 29) / 2; // tagline is 29 cols wide
        assert!(
            ind.abs_diff(want) <= 1,
            "tagline off-centre: indent {ind}, want ~{want}"
        );
        // Braille sun: inside the borders, on the RIGHT of the text column.
        let sun: Vec<(usize, usize)> = plain
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                l.chars()
                    .position(|c| ('⠁'..='⣿').contains(&c))
                    .map(|p| (i, p))
            })
            .collect();
        assert!(!sun.is_empty(), "no braille sun on the wide landing screen");
        assert!(
            sun.iter().all(|&(i, _)| top < i && i < bottom),
            "sun escaped the frame"
        );
        assert!(
            sun.iter().all(|&(_, p)| p > INNER),
            "sun must dock on the right flank, not in the text column"
        );
    }

    /// A pane too narrow for the right flank stacks sun and wordmark ABOVE the frame instead of
    /// letting the flank clip — nothing braille or block-art between the rules.
    #[test]
    fn narrow_layout_stacks_sun_and_wordmark_above_the_frame() {
        let out = render_inner(false, 80);
        let plain: Vec<String> = out
            .lines()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect();
        let top = plain
            .iter()
            .position(|l| l.contains('╭'))
            .expect("top rule");
        let sun_last = plain
            .iter()
            .rposition(|l| l.chars().any(|c| ('⠁'..='⣿').contains(&c)))
            .expect("no braille sun on the narrow landing screen");
        let word = plain
            .iter()
            .position(|l| l.contains("█████"))
            .expect("wordmark row");
        assert!(sun_last < word, "sun stacks above the wordmark");
        assert!(word < top, "wordmark sits above the frame");
    }
}
