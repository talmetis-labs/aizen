//! The single source of truth for the Aizen TUI palette — the **moonlight** identity: one calm
//! silver-blue accent + a small, restrained set of semantic colours (ok / error / warn / link) + a
//! code-syntax sub-palette. Everything is 256-colour (universally supported; no truecolor
//! dependency), routed through `console::style` so `NO_COLOR` and non-TTY output are auto-stripped.
//!
//! Discipline: NOT a rainbow. The silver moonlight carries brand + structure (prompt, gutter, tool
//! names, headings) — Aizen "holds the moon", so the whole UI is moonlit, not gilded. **Gold is
//! reserved** for one thing: the `⚡ yolo` mode chip + warnings (the one warm spot that should pull
//! the eye). The other semantic colours appear only where they MEAN something (green = success/added,
//! salmon = error/removed, blue = links/inline-code). Anything else is neutral grey.
//!
//! Use the helpers (`accent`, `ok`, `err`, …) instead of scattering raw `color256(..)` calls so the
//! palette can be retuned in one place.
//!
//! Mapped from the claude.ai/design "Aizen CLI" spec:
//!   moonlight #c3ccd8 (≈ 252, ACCENT) · dim silver #b6c0cf (≈ 248, ACCENT_DIM) · gold #d8b46a
//!   (≈ 179, WARN — yolo only) · green #5fbf7f (≈ 71, OK) · salmon #c98a82 (≈ 174, ERR) ·
//!   faint #56544c (≈ 240, FAINT). The PetalMark + wordmark are silver-white on the dark ground.

use console::{style, StyledObject};
use std::fmt::Display;

// ── core palette (256-colour indices) ───────────────────────────────────────────
/// Moonlight silver — brand + structure (prompt arrow, assistant gutter, tool names, headings).
pub const ACCENT: u8 = 252;
/// Dim silver — secondary moonlight: tool arguments/values, the `◆ smart` chip, quiet rules/borders.
pub const ACCENT_DIM: u8 = 248;
/// Neutral grey for secondary text (the old `.dim()` role, but a defined shade).
pub const MUTED: u8 = 245;
/// Very faint grey — separators, the code-block rule, timestamps.
pub const FAINT: u8 = 240;

// ── semantic (used ONLY where the colour carries meaning) ────────────────────────
/// Success / confirmation / added.
pub const OK: u8 = 71;
/// Error / failure / removed — a soft "noir" salmon (#c98a82), not a glaring red.
pub const ERR: u8 = 174;
/// Warning / caution — the reserved warm gold (#d8b46a): the `⚡ yolo` chip + cautions, nothing else.
pub const WARN: u8 = 179;
/// Links + inline code (a calm blue, distinct from the gold accent).
pub const LINK: u8 = 110;

// ── diff panes (the boxed edit preview) ──────────────────────────────────────────
// Changed rows carry a tinted BACKGROUND (the OpenCode/GitHub side-by-side look) with a light
// same-family foreground on top — subtle enough not to shout, distinct enough to scan.
/// Removed-row background — a deep muted red.
pub const DIFF_DEL_BG: u8 = 52;
/// Removed-row text — light salmon that reads on [`DIFF_DEL_BG`].
pub const DIFF_DEL_FG: u8 = 217;
/// Added-row background — a deep muted green.
pub const DIFF_ADD_BG: u8 = 22;
/// Added-row text — light green that reads on [`DIFF_ADD_BG`].
pub const DIFF_ADD_FG: u8 = 157;

// ── code-syntax sub-palette (light, best-effort highlighter) ─────────────────────
pub const CODE_KEYWORD: u8 = 176; // soft mauve
pub const CODE_STRING: u8 = 108; // sage green
pub const CODE_NUMBER: u8 = 110; // blue
pub const CODE_COMMENT: u8 = 244; // grey
pub const CODE_RULE: u8 = 240; // the left │ / box border

// ── helpers (return StyledObject so callers can still chain .bold()/.italic()) ───
pub fn accent<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(ACCENT)
}
pub fn accent_dim<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(ACCENT_DIM)
}
pub fn muted<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(MUTED)
}
pub fn faint<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(FAINT)
}
pub fn ok<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(OK)
}
pub fn err<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(ERR)
}
pub fn warn<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(WARN)
}
pub fn link<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(LINK)
}
/// An added diff row: light green ON the deep-green tint (the whole padded cell, so the
/// background reaches the pane edge).
pub fn diff_add<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(DIFF_ADD_FG).on_color256(DIFF_ADD_BG)
}
/// A removed diff row: light salmon ON the deep-red tint.
pub fn diff_del<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(DIFF_DEL_FG).on_color256(DIFF_DEL_BG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_colours_are_distinct() {
        // A regression guard: if two semantic roles collapse onto the same index the UI loses
        // meaning. Accent/ok/err/link/warn must all differ.
        let all = [ACCENT, OK, ERR, WARN, LINK];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two semantic colours share index {a}");
            }
        }
    }

    #[test]
    fn helpers_render_to_nonempty() {
        // Under the test harness colours may be stripped (no TTY); the text must still be present.
        assert!(accent("x").to_string().contains('x'));
        assert!(ok("done").to_string().contains("done"));
    }
}
