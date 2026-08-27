//! The single source of truth for the Aizen TUI palette — the **moonlight** identity: one calm
//! silver-blue accent + a small, restrained set of semantic colours (ok / error / warn / link) + a
//! code-syntax sub-palette. Everything is 256-colour (universally supported; no truecolor
//! dependency), routed through `console::style` so `NO_COLOR` and non-TTY output are auto-stripped.
//!
//! Discipline: hue carries MEANING, never decoration. The silver moonlight stays the brand +
//! structure (prompt, gutter, borders, headings) — Aizen "holds the moon", so the ground is
//! moonlit. On top of it sit two small palettes. The SEMANTIC colours mark outcomes (green =
//! success/added, salmon = error/removed, gold = warning/yolo, blue = links/inline-code). The
//! WORK-LANE colours ([`tool_color`]) give each *kind* of work one muted hue — read = blue,
//! edit = gold, shell = mauve, web = cyan, memory = violet, delegation/asking = pink,
//! plan/checkpoint = teal — so a glance down the transcript shows what Aizen was doing without
//! reading a single tool name. The lanes are the opt-in **`lanes` theme** (`/theme lanes`,
//! persisted as `"theme": "lanes"`); the default **`moonlight`** theme keeps every work surface
//! brand-silver. Gold deliberately spans both palettes: warnings, the `⚡ yolo` chip and file
//! mutations are one warm "this changes things" family. Everything else is neutral grey.
//!
//! Use the helpers (`accent`, `ok`, `err`, …) instead of scattering raw `color256(..)` calls so the
//! palette can be retuned in one place.
//!
//! Mapped from the claude.ai/design "Aizen CLI" spec:
//!   moonlight #c3ccd8 (≈ 252, ACCENT) · dim silver #b6c0cf (≈ 248, ACCENT_DIM) · gold #d8b46a
//!   (≈ 179, WARN + the edit lane) · green #5fbf7f (≈ 71, OK) · salmon #c98a82 (≈ 174, ERR) ·
//!   faint #56544c (≈ 240, FAINT). The PetalMark + wordmark are silver-white on the dark ground.

use console::{style, StyledObject};
use std::fmt::Display;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

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

// ── work lanes (one muted hue per kind of work; folded from `Lane` by `tool_color`) ──
/// Reading & searching the repo — file read/glob/search, repo map, LSP navigation.
pub const LANE_READ: u8 = 110; // blue #87afd7 (shares LINK's blue: both mean "looking, not touching")
/// Mutating files — write/edit/move + symbol edits. Deliberately the same index as [`WARN`]:
/// gold is the "this changes things" family (edits, the yolo chip, warnings).
pub const LANE_EDIT: u8 = 179; // gold #d7af5f
/// Shell commands + long-running processes.
pub const LANE_EXEC: u8 = 176; // mauve #d787d7
/// The outside world — web search/fetch/crawl, browser, MCP integrations.
pub const LANE_WEB: u8 = 116; // cyan #87d7d7
/// Aizen's own mind — memory, session recall, skills, persona.
pub const LANE_MIND: u8 = 140; // violet #af87d7
/// Talking to someone — sub-agents/workflows/team, clarify, telegram/notify.
pub const LANE_TALK: u8 = 175; // pink #d787af
/// Progress & time — the todo plan and checkpoints.
pub const LANE_PLAN: u8 = 73; // teal #5fafaf

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
// ── theme selection (runtime) ────────────────────────────────────────────────────
/// Which theme paints the work surfaces. 0 = unresolved (ask the config once), 1 = moonlight
/// (the default: every work surface brand-silver), 2 = lanes (each kind of work in its own hue).
static THEME: AtomicU8 = AtomicU8::new(0);
/// Bumped on every theme switch; the retained renderer folds it into its cache key so rows
/// painted under the old theme re-render instead of being served stale.
static THEME_GEN: AtomicU32 = AtomicU32::new(0);

/// Whether the `lanes` theme is active. Resolved from `cli-config.json` (`"theme": "lanes"`) on
/// the first ask and cached — the draw path calls this per row and must not touch the filesystem.
pub fn lanes_enabled() -> bool {
    match THEME.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = crate::core::cli_config::load()
                .theme
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("lanes"))
                .unwrap_or(false);
            THEME.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Switch themes live (`/theme`). Bumps [`theme_generation`] so rows already painted under the
/// old theme re-render in the new one.
pub fn set_lanes_enabled(on: bool) {
    THEME.store(if on { 2 } else { 1 }, Ordering::Relaxed);
    THEME_GEN.fetch_add(1, Ordering::Relaxed);
}

/// Monotonic counter of theme switches — part of the retained render-cache key.
pub fn theme_generation() -> u32 {
    THEME_GEN.load(Ordering::Relaxed)
}

/// The work-lane colour a tool name paints with under the CURRENT theme: its lane hue under
/// `lanes`, brand-silver under `moonlight` (so the default looks exactly as it always did).
pub fn tool_color(name: &str) -> u8 {
    if lanes_enabled() {
        lane_color(name)
    } else {
        ACCENT
    }
}

/// The lane hue itself, theme-independent: [`crate::agent::tool_routing::Lane`] — the one
/// name→capability table — folded down to the seven lane hues. Unknown tools stay brand-silver
/// rather than guessing a meaning the table doesn't know. Used by the `lanes` theme and by the
/// `/theme` list's colour swatch.
pub fn lane_color(name: &str) -> u8 {
    use crate::agent::tool_routing::Lane as L;
    match crate::agent::tool_routing::lane_for(name) {
        Some(L::FileRead | L::Structure | L::LspNav) => LANE_READ,
        Some(L::FileWrite | L::LspEdit) => LANE_EDIT,
        Some(L::Shell | L::Process) => LANE_EXEC,
        Some(L::Web | L::Browser | L::Mcp) => LANE_WEB,
        Some(L::MemoryRead | L::MemoryWrite | L::Skills | L::Persona) => LANE_MIND,
        Some(L::Delegation | L::Coordination | L::Clarify | L::Messaging) => LANE_TALK,
        Some(L::Todo | L::Checkpoint) => LANE_PLAN,
        None => ACCENT,
    }
}

/// `Some(lane hue)` under the `lanes` theme, `None` under `moonlight` — for callers whose
/// untinted fallback is NOT silver (the working caption keeps its link-blue when untinted).
pub fn lane_tint(name: &str) -> Option<u8> {
    lanes_enabled().then(|| lane_color(name))
}

/// Style `d` in `name`'s [`tool_color`] — the tint of a tool row's icon + name.
pub fn lane<D: Display>(name: &str, d: D) -> StyledObject<D> {
    style(d).color256(tool_color(name))
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
    fn lane_colours_are_distinct_and_dont_shadow_results() {
        // Every lane must be tellable from the others AND from the result tints that sit directly
        // under a tool name (green ok / salmon err digests). LANE_EDIT == WARN is the one
        // deliberate share: gold is the whole "this changes things" family.
        let lanes = [
            LANE_READ, LANE_EDIT, LANE_EXEC, LANE_WEB, LANE_MIND, LANE_TALK, LANE_PLAN,
        ];
        for (i, a) in lanes.iter().enumerate() {
            for b in &lanes[i + 1..] {
                assert_ne!(a, b, "two lanes share colour index {a}");
            }
        }
        for l in lanes {
            assert_ne!(l, OK, "a lane must not read as a success digest");
            assert_ne!(l, ERR, "a lane must not read as a failure digest");
        }
        assert_eq!(LANE_EDIT, WARN, "gold = the mutation family, on purpose");
    }

    #[test]
    fn tool_colours_follow_the_routing_table() {
        // The mapping rides `lane_for`, so a tool renamed or re-laned there re-colours here
        // automatically; this pins one representative per hue plus the unknown fallback.
        // `lane_color` (not `tool_color`) so the assertions hold regardless of which theme other
        // tests in this process have switched to.
        assert_eq!(lane_color("file_read"), LANE_READ);
        assert_eq!(lane_color("lsp_references"), LANE_READ);
        assert_eq!(lane_color("file_edit"), LANE_EDIT);
        assert_eq!(lane_color("symbol_replace"), LANE_EDIT);
        assert_eq!(lane_color("shell_run"), LANE_EXEC);
        assert_eq!(lane_color("web_search"), LANE_WEB);
        assert_eq!(lane_color("mcp_github_create_issue"), LANE_WEB);
        assert_eq!(lane_color("memory_search"), LANE_MIND);
        assert_eq!(lane_color("task"), LANE_TALK);
        assert_eq!(lane_color("clarify"), LANE_TALK);
        assert_eq!(lane_color("todo_write"), LANE_PLAN);
        assert_eq!(lane_color("checkpoint"), LANE_PLAN);
        assert_eq!(
            lane_color("mystery_tool"),
            ACCENT,
            "unknown stays brand-silver"
        );
    }

    #[test]
    fn moonlight_is_all_silver_and_lanes_is_opt_in() {
        // The default theme must look exactly like the pre-lanes UI: every tool silver, no
        // caption tint. Switching to `lanes` colours the same calls, and each switch bumps the
        // generation so the retained render cache re-renders old rows.
        set_lanes_enabled(false);
        assert_eq!(tool_color("file_edit"), ACCENT);
        assert_eq!(tool_color("shell_run"), ACCENT);
        assert_eq!(lane_tint("file_edit"), None);
        let g = theme_generation();
        set_lanes_enabled(true);
        assert_eq!(tool_color("file_edit"), LANE_EDIT);
        assert_eq!(lane_tint("file_edit"), Some(LANE_EDIT));
        assert!(theme_generation() > g, "a switch must bump the generation");
        set_lanes_enabled(false); // restore the default for the rest of the process
    }

    #[test]
    fn helpers_render_to_nonempty() {
        // Under the test harness colours may be stripped (no TTY); the text must still be present.
        assert!(accent("x").to_string().contains('x'));
        assert!(ok("done").to_string().contains("done"));
    }
}
