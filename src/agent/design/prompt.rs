//! Ephemeral design context goes on the sent user turn, never in stable/system cache lanes.

use super::DesignContext;
use crate::core::types::Message;

pub const MARKER: &str = "<aizen_design_context>\n";
const END: &str = "\n</aizen_design_context>\n\n";

pub fn strip_prefix(content: &str) -> &str {
    if !content.starts_with(MARKER) {
        return content;
    }
    content
        .split_once(END)
        .map(|(_, rest)| rest)
        .unwrap_or(content)
}

pub fn fold_into_query(query: &str, ctx: &DesignContext) -> String {
    let clean = strip_prefix(query);
    match ctx.capsule() {
        Some(capsule) => format!("{MARKER}{capsule}{END}{clean}"),
        None => clean.to_string(),
    }
}

/// Re-seat after a brief/review tool mutates the active run. Strip older injected blocks first so
/// stale styling cannot accumulate. This operates only on the request copy, not persisted history.
pub fn seat_prompt(history: &mut [Message], ctx: &DesignContext) {
    strip_blocks(history);
    if let Some(message) = history.iter_mut().rev().find(|m| m.role == "user") {
        let content = message.content.as_deref().unwrap_or("");
        message.content = Some(fold_into_query(content, ctx));
    }
}

pub fn strip_blocks(history: &mut [Message]) {
    for message in history.iter_mut().filter(|m| m.role == "user") {
        if let Some(content) = message.content.as_deref() {
            let clean = strip_prefix(content);
            if clean.len() != content.len() {
                message.content = Some(clean.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::design::{DesignBrief, DesignIntent, DesignMode};
    #[test]
    fn seating_preserves_prefix_and_strips_prior_turn_blocks() {
        let ctx = DesignContext::new("s", "w", DesignMode::Auto);
        ctx.begin_turn("1", "w");
        ctx.apply_brief(
            DesignBrief {
                intent: DesignIntent::Create,
                brief: "a page".into(),
                acceptance: "readable".into(),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        let mut messages = vec![
            Message::system("frozen"),
            Message::user(fold_into_query("first", &ctx)),
            Message::user("next"),
        ];
        seat_prompt(&mut messages, &ctx);
        assert_eq!(messages[0].content.as_deref(), Some("frozen"));
        assert_eq!(messages[1].content.as_deref(), Some("first"));
        assert!(messages[2].content.as_deref().unwrap().starts_with(MARKER));
        ctx.set_mode(DesignMode::Off);
        seat_prompt(&mut messages, &ctx);
        assert_eq!(messages[2].content.as_deref(), Some("next"));
        assert_eq!(
            strip_prefix("user mentions <aizen_design_context>"),
            "user mentions <aizen_design_context>"
        );
    }
}
