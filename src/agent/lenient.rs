//! Lenient tool-call recovery: JSON repair for stringified arguments, and a content scanner that
//! turns a text-embedded call into a real one when the provider sent no native `tool_calls`.
//!
//! Local and open-weight models behind an OpenAI-compatible server are the target. Three failure
//! shapes were dropping their calls on the floor: (1) arguments that are *almost* JSON — a trailing
//! comma, a raw newline inside a string, Python quotes, or a brace cut off by `max_tokens`; (2) a
//! call written into the assistant text as `<tool_call>{…}</tool_call>` (Hermes / Qwen templates)
//! or inside a ```json fence, with an empty `tool_calls` array; (3) the whole reply being one bare
//! `{"name": …, "arguments": {…}}` object (Llama-3 style) or a `[TOOL_CALLS] [...]` array
//! (Mistral). Strict `serde_json` rejected (1); (2) and (3) read as a final answer, so the loop
//! either returned the model's "call" verbatim to the user or, when the text was otherwise empty,
//! re-sent the identical request as an empty-200 retry.
//!
//! Rules that keep this from becoming a guessing game:
//! - Repair never invents a key or a value. It closes what was opened, drops a dangling comma,
//!   escapes a control character inside a string, maps Python literals, and swaps quote style only
//!   when the text contains no double quote at all. `{not json` stays an error.
//! - A text call is synthesised only when the native array is EMPTY and the name matches a
//!   registered tool. Unknown names leave the text untouched — they are prose, not calls.
//! - The strict parse is always tried first and is the only path for well-formed input, so nothing
//!   measured on a well-behaved provider moves.

use crate::core::types::{FunctionCall, ToolCall};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

/// Parse `raw` as a JSON object, repairing the usual near-misses. `None` when no repair yields an
/// object — the caller keeps its strict error.
pub fn repair_json_object(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let body = &raw[start..];
    let mut candidates = vec![normalize(body, false)];
    if !body.contains('"') {
        candidates.push(normalize(body, true));
    }
    candidates.iter().find_map(|c| {
        serde_json::from_str::<Value>(c)
            .ok()
            .filter(Value::is_object)
    })
}

/// One pass over `body` (which starts at `{`): escapes raw control characters inside strings, drops
/// a comma that sits right before a closer, maps `True`/`False`/`None`, stops after the top-level
/// value closes (a trailing fence or prose is ignored), and finally closes whatever the input left
/// open — a string, then a dangling `:` (→ `null`) or `,` (dropped), then the bracket stack.
/// `swap_quotes` treats `'` as the string delimiter and emits `"`; only used when the input holds
/// no `"` at all, so an apostrophe inside a real JSON string can never be mistaken for a closer.
fn normalize(body: &str, swap_quotes: bool) -> String {
    let quote = if swap_quotes { '\'' } else { '"' };
    let mut out = String::with_capacity(body.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                out.push(c);
                escaped = false;
                continue;
            }
            match c {
                '\\' => {
                    out.push(c);
                    escaped = true;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '"' if swap_quotes => out.push_str("\\\""),
                c if c == quote => {
                    out.push('"');
                    in_string = false;
                }
                c => out.push(c),
            }
            continue;
        }
        match c {
            c if c == quote => {
                out.push('"');
                in_string = true;
            }
            '{' | '[' => {
                stack.push(c);
                out.push(c);
            }
            '}' | ']' => {
                drop_trailing_comma(&mut out);
                stack.pop();
                out.push(c);
                if stack.is_empty() {
                    break;
                }
            }
            c if c.is_ascii_alphabetic() => {
                let mut word = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_alphanumeric() || n == '_' {
                        word.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(match word.as_str() {
                    "True" => "true",
                    "False" => "false",
                    "None" => "null",
                    w => w,
                });
            }
            c => out.push(c),
        }
    }
    if escaped {
        out.pop();
    }
    if in_string {
        out.push('"');
    }
    let end = out.trim_end().len();
    out.truncate(end);
    if out.ends_with(':') {
        out.push_str("null");
    } else if out.ends_with(',') {
        out.pop();
    }
    while let Some(open) = stack.pop() {
        out.push(if open == '{' { '}' } else { ']' });
    }
    out
}

fn drop_trailing_comma(out: &mut String) {
    let end = out.trim_end().len();
    if out[..end].ends_with(',') {
        out.truncate(end - 1);
    }
}

static RECOVERED_SEQ: AtomicU64 = AtomicU64::new(1);

/// Scan assistant text for tool calls the provider failed to lift into `tool_calls`. Returns the
/// synthesised calls and the text that remains once the call blocks are removed (`None` when
/// nothing but the calls was there). `None` when no block names a known tool — the text is prose.
pub fn extract_text_tool_calls(
    content: &str,
    is_known: impl Fn(&str) -> bool,
) -> Option<(Vec<ToolCall>, Option<String>)> {
    const TAG_OPEN: &str = "<tool_call>";
    const TAG_CLOSE: &str = "</tool_call>";
    const FENCE: &str = "```";

    let mut calls: Vec<ToolCall> = Vec::new();
    let mut rest = String::new();
    let mut cursor = 0usize;
    while cursor < content.len() {
        let tail = &content[cursor..];
        let tag = tail.find(TAG_OPEN).map(|i| (i, true));
        let fence = tail.find(FENCE).map(|i| (i, false));
        let Some((at, is_tag)) = [tag, fence].into_iter().flatten().min_by_key(|(i, _)| *i) else {
            rest.push_str(tail);
            break;
        };
        let start = cursor + at;
        rest.push_str(&content[cursor..start]);
        let (inner, end) = if is_tag {
            let s = start + TAG_OPEN.len();
            match content[s..].find(TAG_CLOSE) {
                Some(e) => (&content[s..s + e], s + e + TAG_CLOSE.len()),
                None => (&content[s..], content.len()),
            }
        } else {
            let s = start + FENCE.len();
            // Skip the info string (`json`, `tool_call`, …) up to the end of its line.
            let body = content[s..]
                .find('\n')
                .map(|i| s + i + 1)
                .unwrap_or(content.len());
            match content[body..].find(FENCE) {
                Some(e) => (&content[body..body + e], body + e + FENCE.len()),
                None => {
                    // Unterminated fence: ordinary text.
                    rest.push_str(&content[start..]);
                    break;
                }
            }
        };
        match calls_from_json(inner, &is_known) {
            Some(found) => calls.extend(found),
            None => rest.push_str(&content[start..end]),
        }
        cursor = end;
    }
    if calls.is_empty() {
        // Bare object or array as the whole reply, with or without Mistral's marker.
        let bare = content
            .trim()
            .trim_start_matches("[TOOL_CALLS]")
            .trim_start();
        if bare.starts_with('{') || bare.starts_with('[') {
            let found = calls_from_json(bare, &is_known)?;
            return Some((found, None));
        }
        return None;
    }
    let rest = rest.trim().to_string();
    Some((calls, if rest.is_empty() { None } else { Some(rest) }))
}

/// One block's JSON → calls. Accepts an object, an array of objects, and the
/// `{"type":"function","function":{…}}` wrapper; `arguments` / `parameters` / `input` may be an
/// object or an already-stringified object. Every named tool must be known, or the block is prose.
fn calls_from_json(text: &str, is_known: &impl Fn(&str) -> bool) -> Option<Vec<ToolCall>> {
    let text = text.trim();
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => repair_json_object(text)?,
    };
    let items: Vec<&Value> = match &value {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    if items.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = item
            .get("function")
            .filter(|f| f.is_object())
            .unwrap_or(item);
        let name = obj.get("name")?.as_str()?.trim();
        if name.is_empty() || !is_known(name) {
            return None;
        }
        let args = obj
            .get("arguments")
            .or_else(|| obj.get("parameters"))
            .or_else(|| obj.get("input"));
        let arguments = match args {
            None | Some(Value::Null) => "{}".to_string(),
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(v) if v.is_object() => s.clone(),
                _ => repair_json_object(s)?.to_string(),
            },
            Some(v @ Value::Object(_)) => v.to_string(),
            Some(_) => return None,
        };
        out.push(ToolCall {
            id: format!(
                "recovered-{}",
                RECOVERED_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            kind: "function".into(),
            function: FunctionCall {
                name: name.to_string(),
                arguments,
            },
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(n: &str) -> bool {
        matches!(n, "file_read" | "shell_run" | "file_edit")
    }

    #[test]
    fn well_formed_json_is_untouched_and_junk_stays_junk() {
        assert_eq!(
            repair_json_object(r#"{"path": "a.rs"}"#).unwrap(),
            serde_json::json!({"path": "a.rs"})
        );
        assert!(repair_json_object("{not json").is_none());
        assert!(repair_json_object("no braces at all").is_none());
        assert!(
            repair_json_object("[1, 2]").is_none(),
            "an array is not an arguments object"
        );
    }

    #[test]
    fn trailing_commas_and_truncation_are_repaired() {
        assert_eq!(
            repair_json_object(r#"{"path": "a.rs", "lines": [1, 2,],}"#).unwrap(),
            serde_json::json!({"path": "a.rs", "lines": [1, 2]})
        );
        // Cut by max_tokens inside a nested object, then inside a string, then after a colon.
        assert_eq!(
            repair_json_object(r#"{"path": "a.rs", "opts": {"n": 3"#).unwrap(),
            serde_json::json!({"path": "a.rs", "opts": {"n": 3}})
        );
        assert_eq!(
            repair_json_object(r#"{"path": "src/ma"#).unwrap(),
            serde_json::json!({"path": "src/ma"})
        );
        assert_eq!(
            repair_json_object(r#"{"path": "a.rs", "content":"#).unwrap(),
            serde_json::json!({"path": "a.rs", "content": null})
        );
    }

    #[test]
    fn raw_newlines_inside_strings_are_escaped() {
        let raw = "{\"path\": \"a.rs\", \"content\": \"line one\nline two\ttabbed\"}";
        let v = repair_json_object(raw).unwrap();
        assert_eq!(v["content"], "line one\nline two\ttabbed");
    }

    #[test]
    fn python_quotes_and_literals_are_mapped_only_without_double_quotes() {
        assert_eq!(
            repair_json_object("{'path': 'a.rs', 'all': True, 'limit': None}").unwrap(),
            serde_json::json!({"path": "a.rs", "all": true, "limit": null})
        );
        // An apostrophe inside a real JSON string is data, not a delimiter.
        assert_eq!(
            repair_json_object(r#"{"text": "it's fine"}"#).unwrap()["text"],
            "it's fine"
        );
    }

    #[test]
    fn trailing_prose_and_fences_after_the_object_are_ignored() {
        assert_eq!(
            repair_json_object("```json\n{\"path\": \"a.rs\"}\n```\nThat reads the file.").unwrap(),
            serde_json::json!({"path": "a.rs"})
        );
    }

    #[test]
    fn hermes_blocks_become_calls_and_the_prose_survives() {
        let text = "Let me look.\n<tool_call>\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>\n<tool_call>{\"name\":\"shell_run\",\"arguments\":{\"cmd\":\"ls\"}}</tool_call>";
        let (calls, rest) = extract_text_tool_calls(text, known).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "file_read");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[0].function.arguments).unwrap(),
            serde_json::json!({"path": "a.rs"})
        );
        assert_eq!(calls[1].function.name, "shell_run");
        assert_eq!(rest.as_deref(), Some("Let me look."));
        assert!(calls[0].id.starts_with("recovered-"));
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn fenced_json_and_bare_objects_are_recognised() {
        let fenced =
            "```json\n{\"name\": \"file_read\", \"parameters\": {\"path\": \"b.rs\"}}\n```";
        let (calls, rest) = extract_text_tool_calls(fenced, known).unwrap();
        assert_eq!(calls[0].function.name, "file_read");
        assert!(calls[0].function.arguments.contains("b.rs"));
        assert!(rest.is_none());

        let bare = "{\"name\": \"shell_run\", \"arguments\": \"{\\\"cmd\\\": \\\"ls\\\"}\"}";
        let (calls, _) = extract_text_tool_calls(bare, known).unwrap();
        assert_eq!(calls[0].function.arguments, "{\"cmd\": \"ls\"}");

        let mistral =
            "[TOOL_CALLS] [{\"name\": \"file_read\", \"arguments\": {\"path\": \"c.rs\"}}]";
        let (calls, _) = extract_text_tool_calls(mistral, known).unwrap();
        assert_eq!(calls.len(), 1);

        let wrapped = "{\"type\":\"function\",\"function\":{\"name\":\"file_read\",\"arguments\":{\"path\":\"d.rs\"}}}";
        let (calls, _) = extract_text_tool_calls(wrapped, known).unwrap();
        assert_eq!(calls[0].function.name, "file_read");
    }

    #[test]
    fn unknown_names_and_ordinary_prose_are_left_alone() {
        assert!(extract_text_tool_calls("Here is the answer: 42.", known).is_none());
        assert!(extract_text_tool_calls(
            "<tool_call>{\"name\": \"nope\", \"arguments\": {}}</tool_call>",
            known
        )
        .is_none());
        // A JSON example in prose that happens to have a `name` key but is not a call shape.
        assert!(extract_text_tool_calls(
            "```json\n{\"name\": \"file_read\", \"arguments\": 3}\n```",
            known
        )
        .is_none());
        // A block with an unknown name next to a known one: the whole block is prose (no half calls).
        let mixed = "<tool_call>[{\"name\":\"file_read\",\"arguments\":{}},{\"name\":\"zzz\",\"arguments\":{}}]</tool_call>";
        assert!(extract_text_tool_calls(mixed, known).is_none());
        // An unterminated fence is ordinary text.
        assert!(extract_text_tool_calls("```json\n{\"name\": \"file_read\"", known).is_none());
    }

    #[test]
    fn a_truncated_block_is_still_recovered() {
        let cut = "<tool_call>{\"name\": \"file_read\", \"arguments\": {\"path\": \"a.rs\"";
        let (calls, rest) = extract_text_tool_calls(cut, known).unwrap();
        assert_eq!(calls[0].function.name, "file_read");
        assert!(calls[0].function.arguments.contains("a.rs"));
        assert!(rest.is_none());
    }
}
