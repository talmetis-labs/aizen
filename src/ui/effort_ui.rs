//! The interactive surface of reasoning effort: this turn's tier, the `/effort` slider, and the
//! status report. The CLASSIFIER is pure and lives in `core::effort`; everything here reads config
//! and prints, which is why it could not live there.

use crate::core::cli_config;
use crate::ui::{splash, theme, tui};
use console::style;

/// Decide THIS turn's reasoning effort. When auto-detect is ON (default), classify the user's
/// fully-expanded message (keyword ladder → complexity heuristic); a hit forces that tier, else we
/// fall back to the configured `reasoning_effort` (which may itself be `None` ⇒ omit the field).
/// PURE-ish wrapper (loads config) around the pure `core::effort::classify_effort`. The result is
/// armed into the per-turn override cell read by the LLM client, and cleared at turn end.
pub(crate) fn resolve_turn_effort(line: &str) -> Option<String> {
    // Ultimate mode pins max effort every turn (auto-detect is bypassed) — the aizen `ultracode`.
    if cli_config::ultimate_enabled() {
        return Some("max".to_string());
    }
    if cli_config::auto_effort_enabled() {
        // P3: adaptive routing lets the complexity heuristic climb to `xhigh` on the hardest turns.
        let adaptive = cli_config::adaptive_effort_enabled();
        if let Some(e) = crate::core::effort::classify_effort_with(line, adaptive) {
            return Some(e.as_str().to_string());
        }
    }
    cli_config::load().reasoning_effort.clone()
}

/// The effort chip the HUD's model segment carries: `model·auto`, tinted by the rung the turn
/// resolved to (low/medium = dim silver · high/xhigh = gold · max = salmon, like the slider's
/// tier colours) — auto-detection is the mode the user sees, the rung is the provider's detail.
/// `None` ⇒ no effort field this turn ⇒ faint "default".
pub(crate) fn effort_chip_styled(eff: Option<&str>) -> String {
    match eff {
        Some("low") | Some("medium") => {
            console::style("·auto".to_string()).color256(theme::ACCENT_DIM)
        }
        Some("high") | Some("xhigh") => console::style("·auto".to_string()).color256(theme::WARN),
        Some("max") => console::style("·auto".to_string())
            .color256(theme::ERR)
            .bold(),
        Some("") => return String::new(),
        Some(other) => console::style(format!("·{other}")).color256(theme::ACCENT),
        None => console::style("·default".to_string()).color256(theme::FAINT),
    }
    .to_string()
}

/// What the HUD's model chip should show for THIS turn: the effort-resolved model plus a `·auto`
/// chip tinted by the resolved rung (the rung goes on the wire but stays out of the HUD; a routed
/// model is still named, since that is what requests actually go to). Pushes the composed chip into
/// the status row's side channel (`tui::status_chip_arm`), which `status_text` splices onto the
/// model name on the next rebuild — so the prep-time read already carries the chip on the next
/// frame.
///
/// `routed`: `models_by_effort` sent this tier to another model — the chip names the ROUTED model,
/// which is what the requests actually go to; the bare label returns with the post-turn refresh.
///
/// The old per-turn `effort: <tier> · <shape>` transcript line was removed in favour of this —
/// the shape still drives the deferred tool surface via `turn_shape::note_turn`, it just no
/// longer prints anywhere.
pub(crate) fn apply_turn_effort_hud(eff: &Option<String>, routed: Option<&str>) {
    let chip_styled = effort_chip_styled(eff.as_deref());
    // A missing chip (eff = Some("") — the caller armed "omit this field") means the model
    // segment stands bare; arming unconditionally is what clears a stale chip from the cell.
    let label = match routed {
        Some(m) => format!("{m}{chip_styled}"),
        None => chip_styled,
    };
    tui::status_chip_arm(label);
}

/// What the HUD shows between turns: `·auto` when detection decides each turn, otherwise the
/// pinned tier itself (`·low`…`·max`) — pinned is a standing user choice, so it is named, while a
/// resolved auto rung is not (see `effort_chip_styled`).
/// Called where the REPL already rebuilds the status row (activate / resume / post-slash /
/// post-turn), so the chip follows the setting the moment it changes. No-op when the status row
/// isn't live (plain REPL), where `tui::status_chip_arm` already goes nowhere.
pub(crate) fn refresh_effort_hud() {
    let cfg = cli_config::load();
    let styled = if cli_config::auto_effort_enabled() {
        effort_chip_styled(Some("auto"))
    } else {
        match cfg.reasoning_effort.as_deref() {
            Some(t) if !t.is_empty() => console::style(format!("·{t}"))
                .color256(theme::WARN)
                .to_string(),
            _ => effort_chip_styled(None),
        }
    };
    if !styled.is_empty() {
        tui::status_chip_arm(styled);
    }
}

/// The current effort setting as a slider index: 0 = auto (auto-detect ON, no pinned tier), 1..=5
/// the pinned tier, 6 = ultimate. A pinned-but-unknown effort string, or auto-off with no pin, both
/// fall back to `auto` so the slider always opens on a valid stop.
///
/// Ultimate answers first because it is not a tier: it pins max effort every turn regardless of
/// what `reasoning_effort` says, so opening the slider anywhere else would be showing a setting
/// that is not the one in force.
fn effort_slider_start() -> usize {
    let cfg = cli_config::load();
    if cli_config::ultimate_enabled() {
        return ULTIMATE;
    }
    if cli_config::auto_effort_enabled() {
        return 0; // auto ON ⇒ the "auto" stop, regardless of any stale pinned value
    }
    match cfg.reasoning_effort.as_deref() {
        Some("low") => 1,
        Some("medium") => 2,
        Some("high") => 3,
        Some("xhigh") => 4,
        Some("max") => 5,
        _ => 0,
    }
}

/// The top stop, `tui::E_TIERS`' last index. Named because it is the one stop that is a mode
/// rather than a rung, and three places have to agree about which one it is.
const ULTIMATE: usize = 6;

/// Apply a slider choice to the config and persist it. `0` ⇒ auto (auto_effort=None, clear the
/// pin); `1..=5` ⇒ pin low/medium/high/xhigh/max and turn auto off; `6` ⇒ ultimate. The same writes
/// as `/effort auto`, `/effort low|…|max` and `/ultimate`, so the slider and the text commands stay
/// in lockstep.
///
/// Every branch writes `ultimate`, not just the one that turns it on. Sliding down off the top stop
/// has to turn the mode off too — otherwise the tier you just chose is picked and then silently
/// overruled every turn by a flag nothing cleared.
fn apply_effort_choice(idx: usize) {
    let mut cfg = cli_config::load();
    let msg = match idx {
        ULTIMATE => {
            cfg.ultimate = Some(true);
            // Pinned as well as flagged: `ultimate_enabled` is what forces max at turn time, and
            // this is what `/effort status` and the next slider open both read back.
            cfg.reasoning_effort = Some("max".to_string());
            cfg.auto_effort = Some(false);
            "✦ ultimate ON — max reasoning effort every turn + prefers launching workflows for fan-out-able tasks.".to_string()
        }
        1..=5 => {
            let tier = ["", "low", "medium", "high", "xhigh", "max"][idx];
            cfg.ultimate = None;
            cfg.reasoning_effort = Some(tier.to_string());
            cfg.auto_effort = Some(false);
            format!("effort pinned to {tier} (auto off) — every turn now sends reasoning_effort={tier}.")
        }
        _ => {
            cfg.ultimate = None;
            cfg.auto_effort = None; // None ⇒ auto ON (the default)
            cfg.reasoning_effort = None; // clear any stale pin so auto isn't shadowed
            "effort auto ON — each turn's effort is detected from your message (keyword + complexity).".to_string()
        }
    };
    match cli_config::save(&cfg) {
        Ok(_) => tui::emit_line(&style(msg).color256(splash::ACCENT).to_string()),
        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
    }
    // Recolour the retained input box, exactly as `/ultimate` does: gold while ultimate is ON,
    // moonlight when OFF. Reads the effective state, so an `AIZEN_ULTIMATE` in the environment
    // keeps the box gold even after the slider was dragged down from the top.
    tui::set_ultimate(cli_config::ultimate_enabled());
    // The HUD's model segment carries the new tier from this moment on.
    refresh_effort_hud();
}

/// The plain text status report for `/effort status` (and the off-TTY fallback of the bare `/effort`).
pub(crate) fn effort_status_report() {
    let cfg = cli_config::load();
    let auto = if cli_config::auto_effort_enabled() {
        "on"
    } else {
        "off"
    };
    let fixed = cfg
        .reasoning_effort
        .as_deref()
        .unwrap_or("(none — omitted)");
    tui::emit_line(
        &style(format!(
            "effort: auto-detect {auto} · fixed reasoning_effort {fixed}\n\
             /effort auto|off · /effort low|medium|high (pins it, turns auto off) · /effort none (clear)"
        ))
        .dim()
        .to_string(),
    );
    if std::env::var("AIZEN_AUTO_EFFORT").is_ok() {
        tui::emit_line(
            &style("(note: AIZEN_AUTO_EFFORT is set — it overrides the auto toggle)")
                .dim()
                .to_string(),
        );
    }
}

/// Bare `/effort` → the animated drag slider. Opens on the current setting; a commit persists the
/// choice, Esc keeps things as-is. Off-TTY the slider returns `None` immediately, so we fall back to
/// the text report instead of leaving the user with no output.
pub(crate) fn effort_slider_flow() {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        effort_status_report();
        return;
    }
    match tui::effort_slider(effort_slider_start()) {
        Some(idx) => apply_effort_choice(idx),
        None => tui::emit_line(&style("(effort unchanged)").dim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One temp home per test body, serialised against every other test that reads config.
    fn isolated(tag: &str) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-effort-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("AIZEN_HOME", &home);
        // The env forces ultimate on wherever it is set, which would make every assertion below
        // pass for the wrong reason.
        std::env::remove_var("AIZEN_ULTIMATE");
        std::env::remove_var("AIZEN_AUTO_EFFORT");
        guard
    }

    /// The slider's top stop is a real stop. It used to fall through to the catch-all arm, so
    /// dragging to `ultimate` and pressing Enter turned auto-detect ON — the opposite of the label.
    #[test]
    fn the_top_stop_turns_ultimate_on_rather_than_auto() {
        let _g = isolated("on");

        apply_effort_choice(ULTIMATE);

        let cfg = cli_config::load();
        assert_eq!(cfg.ultimate, Some(true));
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(cfg.auto_effort, Some(false));
        assert!(cli_config::ultimate_enabled());
        // And the slider reopens where it was left, which it could not do before either.
        assert_eq!(effort_slider_start(), ULTIMATE);
    }

    /// Sliding back down has to clear the mode, not just choose a tier underneath it: ultimate
    /// pins max every turn regardless of `reasoning_effort`, so a leftover flag would quietly
    /// overrule the rung the user just picked.
    #[test]
    fn sliding_down_off_the_top_stop_turns_the_mode_off() {
        let _g = isolated("off");

        apply_effort_choice(ULTIMATE);
        apply_effort_choice(2);

        let cfg = cli_config::load();
        assert_eq!(cfg.ultimate, None);
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("medium"));
        assert!(!cli_config::ultimate_enabled());
        assert_eq!(effort_slider_start(), 2);

        // All the way back to auto clears the pin as well as the mode.
        apply_effort_choice(0);
        let cfg = cli_config::load();
        assert_eq!(cfg.ultimate, None);
        assert_eq!(cfg.reasoning_effort, None);
        assert_eq!(effort_slider_start(), 0);
    }

    /// The index this module calls the top stop is the index the rail draws it at.
    #[test]
    fn the_named_top_stop_is_the_rails_last_one() {
        assert_eq!(ULTIMATE, tui::E_TIERS.len() - 1);
        assert_eq!(tui::E_TIERS[ULTIMATE], "ultimate");
    }
}
