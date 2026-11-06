// Copyright 2026 Aizen authors. Licensed under Apache-2.0.
// Extracted from Aizen; original implementation comments and tests retained below.

//! Shared turn-intent classification for Core Skill and Persona learning.
//! Persistent Memory extraction and confidence weighting belong to the provider.

use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// Explicit "remember this" — strongest, → user-explicit provenance.
    Remember,
    /// The user is correcting the assistant ("no, actually …", "đừng …").
    Correction,
    /// A stated preference ("I prefer …", "always …", "tôi thích …").
    Preference,
    /// Nothing actionable — a normal turn. Learning skips it (event-gated).
    Passive,
}

#[derive(Debug, Clone, Copy)]
pub struct Signal {
    pub kind: SignalKind,
}

static RE_REMEMBER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(remember|note this|keep in mind|for the record|don'?t forget|ghi nh[ớo]|nh[ớo] r[ằa]ng|nh[ớo] l[àa])\b")
        .unwrap()
});
static RE_CORRECTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(^|\s)(no,|nope\b|actually\b|that'?s (wrong|incorrect|not right)|not (correct|right)|wrong\b|incorrect\b|không phải|sai r[ồo]i|đ[ừu]ng\b)")
        .unwrap()
});
static RE_PREFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(i (prefer|like|favou?r|want|always|usually|never)|please (use|always|never)|always use|never use|use .+ instead|tôi (thích|muốn)|luôn luôn|thường dùng)\b")
        .unwrap()
});

/// The single strongest signal present in `text`.
pub fn detect(text: &str) -> Signal {
    if RE_REMEMBER.is_match(text) {
        return Signal {
            kind: SignalKind::Remember,
        };
    }
    if RE_CORRECTION.is_match(text) {
        return Signal {
            kind: SignalKind::Correction,
        };
    }
    if RE_PREFERENCE.is_match(text) {
        return Signal {
            kind: SignalKind::Preference,
        };
    }
    Signal {
        kind: SignalKind::Passive,
    }
}

/// The tool whose firing means this turn authored a fictional character.
pub const PERSONA_AUTHORING_TOOL: &str = "persona_create";

/// View-independent guard scoped to the last canonical user turn. The caller supplies roles,
/// host-owned user-boundary identity and actual assistant tool names, never tool-name prose.
pub fn turn_authored_persona<'a, I, T>(history: I) -> bool
where
    I: IntoIterator<Item = (&'a str, bool, T)>,
    T: IntoIterator<Item = &'a str>,
{
    let mut authored = false;
    for (role, user_boundary, tools) in history {
        if role == "user" && user_boundary {
            authored = false;
        }
        if role == "assistant" && tools.into_iter().any(|t| t == PERSONA_AUTHORING_TOOL) {
            authored = true;
        }
    }
    authored
}

/// A later non-error tool result after an earlier error is a recovered dead end.
/// The caller filters the turn to actual tool messages, preserving their order.
pub fn recovered_from_dead_end<'a>(results: impl IntoIterator<Item = &'a str>) -> bool {
    let mut saw_error = false;
    for result in results {
        if result
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("error:")
        {
            saw_error = true;
        } else if saw_error {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passive_turn_has_no_signal() {
        assert_eq!(
            detect("can you open the file and show me line 40").kind,
            SignalKind::Passive
        );
        assert_eq!(
            detect("what does this function return").kind,
            SignalKind::Passive
        );
    }

    #[test]
    fn remember_is_strongest() {
        assert_eq!(
            detect("remember that I deploy on fridays").kind,
            SignalKind::Remember
        );
        assert_eq!(
            detect("ghi nhớ là tôi dùng pnpm").kind,
            SignalKind::Remember
        );
        assert_eq!(
            detect("no, remember that I prefer pnpm").kind,
            SignalKind::Remember
        );
    }

    #[test]
    fn correction_and_preference() {
        assert_eq!(
            detect("no, that's wrong — use tabs").kind,
            SignalKind::Correction
        );
        assert_eq!(
            detect("actually, use tabs not spaces").kind,
            SignalKind::Correction
        );
        assert_eq!(
            detect("I prefer pnpm over npm").kind,
            SignalKind::Preference
        );
    }

    #[test]
    fn authoring_guard_uses_actual_tools_and_only_canonical_boundaries() {
        assert!(turn_authored_persona([
            ("user", true, vec![]),
            ("assistant", false, vec![PERSONA_AUTHORING_TOOL]),
            ("tool", false, vec![]),
            ("user", false, vec![]),
            ("assistant", false, vec![]),
        ]));
        assert!(!turn_authored_persona([
            ("user", true, vec![]),
            ("assistant", false, vec!["file_read"]),
        ]));
        assert!(!turn_authored_persona([
            ("user", true, vec![]),
            ("assistant", false, vec![PERSONA_AUTHORING_TOOL]),
            ("user", true, vec![]),
            ("assistant", false, vec!["file_read"]),
        ]));
        assert!(!turn_authored_persona([(
            "tool",
            false,
            vec![PERSONA_AUTHORING_TOOL]
        )]));
        assert!(!turn_authored_persona(Vec::<(&str, bool, Vec<&str>)>::new()));
    }
    #[test]
    fn recovery_is_ordered_and_case_insensitive() {
        assert!(recovered_from_dead_end([
            "  ERROR: command failed",
            "success"
        ]));
        assert!(!recovered_from_dead_end(["success", "error: failed"]));
        assert!(!recovered_from_dead_end(["error: one", "error: two"]));
        assert!(!recovered_from_dead_end(["success", "success"]));
    }
}
