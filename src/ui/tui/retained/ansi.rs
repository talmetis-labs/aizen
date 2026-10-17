//! Text that came from somewhere else, made safe to put on a retained frame.
//!
//! Two jobs: measuring/clipping by DISPLAY columns rather than bytes or chars (a CJK glyph is two
//! cells wide), and dealing with ANSI — either stripping it, or parsing the subset we honour into
//! ratatui spans. Control sequences that could move the cursor or clear the screen are removed:
//! this backend owns the screen, and text from a tool must never be able to repaint it.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;

/// Clip a plain string to `max` display columns (ellipsis when it would overflow). Width-aware so a
/// wide glyph never half-lands past the frame.
pub(super) fn clip_to(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if console::measure_text_width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0usize;
    for glyph in s.graphemes(true) {
        let cw = console::measure_text_width(glyph);
        if w.saturating_add(cw) > budget {
            break;
        }
        out.push_str(glyph);
        w += cw;
    }
    out.push('…');
    out
}

/// Clip then right-pad a plain string to exactly `width` display columns (for a box header).
pub(super) fn pad_to(s: &str, width: usize) -> String {
    let clipped = clip_to(s, width);
    let w = console::measure_text_width(&clipped);
    format!("{clipped}{}", " ".repeat(width.saturating_sub(w)))
}

/// Wrap one sanitised row (SGR-only escapes) to `width` display columns. Prefers breaking after
/// the last space inside the window, hard-breaks a word wider than the window, and preserves all
/// whitespace byte-for-byte (emitted text may be pre-aligned — collapsing would shift it). The
/// SGR codes in force at the break are re-emitted at the head of the continuation row, so a
/// styled note keeps its colour past the fold; escapes are zero-width and never count toward the
/// window. This is what keeps a long `sandbox:`/`[dense]`/`mcp:` boot note readable instead of
/// clipped at the pane edge.
pub(super) fn wrap_keep_sgr(row: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let (glyphs, trailing) = crate::ui::links::sgr_glyphs(row);
    let mut out = Vec::new();
    let mut start = 0;
    while start < glyphs.len() {
        let mut end = start;
        let mut used = 0usize;
        let mut space = None;
        while end < glyphs.len() && glyphs[end].text != "\n" {
            let w = glyphs[end].width.min(width);
            if used.saturating_add(w) > width {
                break;
            }
            used += w;
            end += 1;
            if glyphs[end - 1].text == " " {
                space = Some(end);
            }
        }
        let newline = end < glyphs.len() && glyphs[end].text == "\n";
        if !newline && end < glyphs.len() {
            end = space.filter(|at| *at > start).unwrap_or(end);
        }
        let mut rendered = glyphs[start].prefix.clone();
        for glyph in &glyphs[start..end] {
            if glyph.width > width {
                // A double-width glyph cannot physically fit in a one-cell pane. Replace the
                // WHOLE grapheme, never half an emoji or a base stripped of its combining marks.
                rendered.push('�');
            } else {
                rendered.push_str(&glyph.raw);
            }
        }
        if end == glyphs.len() {
            rendered.push_str(&trailing);
        }
        if rendered.contains('\x1b') {
            rendered.push_str("\x1b[0m");
        }
        out.push(rendered);
        start = end + usize::from(newline);
    }
    if out.is_empty() || row.ends_with('\n') {
        out.push(String::new());
    }
    out
}

/// Strip ALL terminal control sequences INCLUDING colour/SGR, leaving printable text only. Used for
/// the intro block (rendered as one flat dim line) and where a plain-text guarantee is wanted.
pub(super) fn sanitize_text(input: &str) -> String {
    console::strip_ansi_codes(&sanitize_keep_sgr(input)).into_owned()
}

/// Strip terminal control sequences that would corrupt the retained frame (cursor moves, screen
/// erase, save/restore, carriage returns) but PRESERVE colour/SGR (`\x1b[…m`) so [`ansi_spans`] can
/// turn them into styled ratatui spans. This is what keeps the `❯` user echo in accent, the edit
/// diff green/salmon, and the `◆` tool anchor tinted by state — instead of one flat grey.
pub(super) fn sanitize_keep_sgr(input: &str) -> String {
    crate::ui::links::sanitize_sgr(input)
}

/// Map a basic ANSI colour index (0–15, the `30`–`37` / `90`–`97` SGR families) to a ratatui colour.
fn basic_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Fold one SGR parameter list (the `;`-separated numbers before an `m`) into the running style,
/// resetting to `base` on `0`/`39`. Supports the codes the app actually emits: reset, bold, dim,
/// italic, underline, the 8+8 basic-colour families (fore- and background), and 256-colour
/// (`38;5;n` / `48;5;n`) / truecolor (`38;2` / `48;2`) — backgrounds carry the diff panes' tints.
fn apply_sgr(base: Style, cur: Style, params: &str) -> Style {
    let codes: Vec<u16> = params
        .split(';')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();
    if codes.is_empty() {
        return base; // a bare `\x1b[m` is `\x1b[0m` — a full reset
    }
    let mut style = cur;
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = base,
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => {
                style = style
                    .remove_modifier(Modifier::BOLD)
                    .remove_modifier(Modifier::DIM)
            }
            30..=37 => style = style.fg(basic_color(codes[i] - 30)),
            39 => style = style.fg(base.fg.unwrap_or(Color::Gray)),
            40..=47 => style = style.bg(basic_color(codes[i] - 40)),
            49 => style.bg = base.bg,
            90..=97 => style = style.fg(basic_color(codes[i] - 90 + 8)),
            100..=107 => style = style.bg(basic_color(codes[i] - 100 + 8)),
            38 => match codes.get(i + 1) {
                Some(5) => {
                    if let Some(&n) = codes.get(i + 2) {
                        style = style.fg(Color::Indexed(n as u8));
                    }
                    i += 2;
                }
                Some(2) => {
                    if let (Some(&r), Some(&g), Some(&b)) =
                        (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                    {
                        style = style.fg(Color::Rgb(r as u8, g as u8, b as u8));
                    }
                    i += 4;
                }
                _ => {}
            },
            48 => match codes.get(i + 1) {
                Some(5) => {
                    if let Some(&n) = codes.get(i + 2) {
                        style = style.bg(Color::Indexed(n as u8));
                    }
                    i += 2;
                }
                Some(2) => {
                    if let (Some(&r), Some(&g), Some(&b)) =
                        (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                    {
                        style = style.bg(Color::Rgb(r as u8, g as u8, b as u8));
                    }
                    i += 4;
                }
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    style
}

/// Parse a single already-sanitised row (SGR only, no cursor moves) into ratatui spans over `base`.
/// A row with no colour codes collapses to one span in the base style, so uncoloured output looks
/// exactly as before.
pub(crate) fn ansi_spans(row: &str, base: Style) -> Vec<Span<'static>> {
    let (glyphs, _) = crate::ui::links::sgr_glyphs(row);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = base;
    for glyph in glyphs {
        let mut rest = glyph.raw.as_str();
        let mut glyph_style = None;
        while let Some(i) = rest.find("\x1b[") {
            if i > 0 {
                glyph_style.get_or_insert(cur);
            }
            rest = &rest[i..];
            let end = rest.find('m').expect("sanitized SGR terminator");
            cur = apply_sgr(base, cur, &rest[2..end]);
            rest = &rest[end + 1..];
        }
        let style = glyph_style.unwrap_or(cur);
        // A style change inside an extended grapheme affects following glyphs, but must not
        // split this one's base and combining/ZWJ sequence into independently measured spans.
        if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
            last.content.to_mut().push_str(&glyph.text);
        } else {
            spans.push(Span::styled(glyph.text, style));
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}
