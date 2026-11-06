//! Host-owned Memory scaffolds. Only a content-bound message receipt authorizes removal.
//! Provider text and marker phrases alone never prove that Core inserted a prefix.

use crate::core::types::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
const MAX_PREFIX_BYTES: usize = 128 * 1024;
/// Legacy fixture spelling only: runtime removal requires a content-bound receipt.
#[cfg(test)]
pub const RECALL_MARKER: &str = "Recalled memory";

/// Storage-only provenance, never accepted from a provider or sent on the model wire. A receipt
/// binds both the exact prefix and the full text, so copying it onto a different request cannot
/// authorize removal. Version-one prefix-only receipts remain ordinary transcript text. This is a
/// content binding, not authentication of a transcript edited by someone with local file access.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallScaffold {
    version: u8,
    provider: String,
    generation: u64,
    session: String,
    turn: String,
    prefix_bytes: usize,
    prefix_sha256: String,
    content_sha256: String,
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn seal(
    message: &mut Message,
    prefix: &str,
    provider: &str,
    generation: u64,
    session: &str,
    turn: &str,
) {
    if message.role != "user" || prefix.is_empty() || prefix.len() > MAX_PREFIX_BYTES {
        return;
    }
    let Some(content) = message.content.as_deref().filter(|content| {
        content
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("\n\n"))
    }) else {
        return;
    };
    message.memory_scaffold = Some(RecallScaffold {
        version: 2,
        provider: provider.into(),
        generation,
        session: session.into(),
        turn: turn.into(),
        prefix_bytes: prefix.len(),
        prefix_sha256: sha256(prefix.as_bytes()),
        content_sha256: sha256(content.as_bytes()),
    });
}

fn verified_content<'a>(message: &'a Message, receipt: &RecallScaffold) -> Option<&'a str> {
    if message.role != "user"
        || receipt.version != 2
        || receipt.prefix_bytes == 0
        || receipt.prefix_bytes > MAX_PREFIX_BYTES
    {
        return None;
    }
    let content = message.content.as_deref()?;
    let prefix = content.get(..receipt.prefix_bytes)?;
    if sha256(prefix.as_bytes()) != receipt.prefix_sha256
        || sha256(content.as_bytes()) != receipt.content_sha256
    {
        return None;
    }
    content.get(receipt.prefix_bytes..)?.strip_prefix("\n\n")
}

/// The storage adapter alone calls this; malformed or stale receipts never reject message text.
pub(crate) fn restore_stored_receipt(message: &mut Message, value: serde_json::Value) {
    message.memory_scaffold = serde_json::from_value::<RecallScaffold>(value)
        .ok()
        .filter(|receipt| verified_content(message, receipt).is_some());
}

/// A storage privacy projection may append a missing-pixels notice. Rebind only an already-valid
/// host receipt, preserving the exact verified prefix, to that deterministic stored text.
pub(crate) fn projected_receipt(
    message: &Message,
    stored_content: Option<&str>,
) -> Option<RecallScaffold> {
    let receipt = message.memory_scaffold.as_ref()?;
    verified_content(message, receipt)?;
    let stored_content = stored_content?;
    let prefix = message.content.as_deref()?.get(..receipt.prefix_bytes)?;
    stored_content.strip_prefix(prefix)?.strip_prefix("\n\n")?;
    let mut projected = receipt.clone();
    projected.content_sha256 = sha256(stored_content.as_bytes());
    Some(projected)
}

/// A borrowed clean view for learning; an unverified saved message remains ordinary user text.
pub(crate) fn clean_content(message: &Message) -> Option<&str> {
    message
        .memory_scaffold
        .as_ref()
        .and_then(|receipt| verified_content(message, receipt))
        .or(message.content.as_deref())
}

pub(crate) fn strip_message(message: &mut Message) {
    if let Some(clean) =
        clean_content(message).filter(|clean| Some(*clean) != message.content.as_deref())
    {
        message.content = Some(clean.to_owned());
    }
    message.memory_scaffold = None;
}

/// Used only at a fresh user/conversation boundary, never during a tool continuation. This changes
/// the outbound in-memory copy, not the saved source transcript or unmarked user-authored text.
pub(crate) fn strip_history(history: &mut [Message]) {
    for message in history {
        strip_message(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_mentions_and_unverified_blocks_preserve_user_text() {
        let prefix = "Recalled memory host-owned-fixture-710\n[m1] fixture";
        for text in [
            "Recalled memory\n\nkeep this",
            "hello Recalled memory",
            "<memory>user text</memory>",
            prefix,
        ] {
            let mut message = Message::user(text);
            strip_message(&mut message);
            assert_eq!(message.content.as_deref(), Some(text));
        }
        let copied = format!("{prefix}\n\nuser pasted this exact block");
        let mut message = Message::user(&copied);
        strip_message(&mut message);
        assert_eq!(message.content.as_deref(), Some(copied.as_str()));
    }

    #[test]
    fn changed_user_text_and_copied_receipts_cannot_authorize_removal() {
        let prefix = "Recalled memory\n[m1] fixture";
        let mut original = Message::user(format!("{prefix}\n\noriginal request"));
        seal(&mut original, prefix, "fixture", 3, "session", "turn");
        assert_eq!(clean_content(&original), Some("original request"));
        for text in [
            format!("{prefix}\n\ndifferent request"),
            format!("{prefix}\n\noriginal request plus user text"),
            format!("changed {prefix}\n\noriginal request"),
        ] {
            let mut copied = Message::user(text.clone());
            copied.memory_scaffold = original.memory_scaffold.clone();
            strip_message(&mut copied);
            assert_eq!(copied.content.as_deref(), Some(text.as_str()));
            assert!(copied.memory_scaffold.is_none());
        }
    }

    #[test]
    fn invalid_stored_receipt_shapes_and_utf8_offsets_preserve_text() {
        let prefix = "Recalled memory ký ức";
        let text = format!("{prefix}\n\ngiữ nguyên yêu cầu");
        let mut original = Message::user(text.clone());
        seal(&mut original, prefix, "fixture", 3, "session", "turn");
        let valid = serde_json::to_value(original.memory_scaffold.as_ref().unwrap()).unwrap();
        let mut malformed = vec![serde_json::Value::Null, serde_json::json!("receipt")];
        for (field, value) in [
            ("version", serde_json::json!(1)),
            ("prefix_bytes", serde_json::json!(0)),
            ("prefix_bytes", serde_json::json!(MAX_PREFIX_BYTES + 1)),
            (
                "prefix_bytes",
                serde_json::json!(prefix.find('ý').unwrap() + 1),
            ),
            ("prefix_sha256", serde_json::json!("0".repeat(64))),
            ("content_sha256", serde_json::json!("0".repeat(64))),
            ("unexpected", serde_json::json!(true)),
        ] {
            let mut receipt = valid.clone();
            receipt[field] = value;
            malformed.push(receipt);
        }
        let mut old = valid.clone();
        old["version"] = serde_json::json!(1);
        old.as_object_mut().unwrap().remove("content_sha256");
        malformed.push(old);
        for receipt in malformed {
            let mut message = Message::user(text.clone());
            restore_stored_receipt(&mut message, receipt);
            assert!(message.memory_scaffold.is_none());
            strip_message(&mut message);
            assert_eq!(message.content.as_deref(), Some(text.as_str()));
        }
        let mut assistant = Message::assistant(text.clone());
        restore_stored_receipt(&mut assistant, valid);
        strip_message(&mut assistant);
        assert_eq!(assistant.content.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn bound_scaffold_removal_is_exact_idempotent_and_not_on_wire() {
        let prefix = "Recalled memory fixture-bound-816";
        let mut message = Message::user(format!("{prefix}\n\nactual user request"));
        seal(&mut message, prefix, "fixture", 3, "session", "turn");
        assert_eq!(clean_content(&message), Some("actual user request"));
        assert!(serde_json::to_value(&message)
            .unwrap()
            .get("memory_scaffold")
            .is_none());
        let mut changed = message.clone();
        changed.content = Some(format!("{prefix} tampered\n\nkeep this"));
        let before = changed.content.clone();
        strip_message(&mut changed);
        assert_eq!(changed.content, before);
        strip_message(&mut message);
        strip_message(&mut message);
        assert_eq!(message.content.as_deref(), Some("actual user request"));
    }
}
