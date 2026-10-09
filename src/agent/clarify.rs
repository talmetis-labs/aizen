//! `clarify` — the ask-then-YIELD tool: when a task is genuinely ambiguous and a wrong guess
//! would waste real work, the model poses one or more focused questions and the turn PAUSES so the
//! user answers before it goes on.
//!
//! Why a yield (not a blocking stdin read): under the retained TUI a background thread owns stdin,
//! so a tool that `read_line`s would fight it (and deadlock / eat keystrokes); under `aizen serve`
//! there is no terminal at all. So instead of READING input, the tool RECORDS the questions in a
//! process-global cell and the agent loop, on seeing it, stops with `StopReason::AwaitingInput`.
//! Whatever input mechanism is already in play — the retained answer panel, the plain REPL
//! readline, or a Telegram message — then supplies the answer as the next user turn, re-entering the
//! same conversation. One mechanism, every surface, zero stdin contention.
//!
//! Distinct from its neighbours (the repo's anti-overlap discipline): `telegram_ask` is
//! approve/deny over inline buttons for UNATTENDED runs; `memory_ask` recalls what the user
//! ALREADY told us. `clarify` is interactive disambiguation that blocks forward progress.
//!
//! A question carries optional SUGGESTED answers (Claude-Code-style): each option has a short label
//! and an optional one-line description, and a question may be `multi_select` (check more than one).
//! The retained UI renders them as a picker with checkboxes; every other surface (plain REPL,
//! Telegram, `aizen agent`) falls back to [`Ask::display`] — the question plus its numbered options —
//! and the user simply types their answer.

use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Mutex;

/// The tool's advertised name — also the key the agent loop uses to decide whether to consult the
/// pending cell this turn (so a turn that never called `clarify` can't drain a stale value).
pub const NAME: &str = "clarify";

/// Upper bounds on what one ask may carry. Claude Code caps at 4 questions; we allow the same and
/// cap options at 8 so the picker stays one screen. Excess is dropped, not an error (a model that
/// over-asks still gets a usable panel).
pub const MAX_QUESTIONS: usize = 4;
pub const MAX_OPTIONS: usize = 8;

/// One suggested answer: a short `label` (what gets submitted) and an optional `description`
/// (the "why", shown dim beside it in the picker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOption {
    pub label: String,
    pub description: String,
}

/// One question in an ask: the text, a short `header` (tab label when there are several questions),
/// the suggested options, and whether more than one may be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<AskOption>,
    pub multi_select: bool,
}

/// The whole ask: one or more questions the turn is paused on. Carried in
/// `StopReason::AwaitingInput` so every caller — the retained panel, the plain REPL, Telegram,
/// `aizen agent` — reads the same structured payload, rendering it to text only where there is no
/// picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub questions: Vec<AskQuestion>,
}

impl Ask {
    /// Parse an ask from the tool arguments. Two shapes are accepted, and both may coexist:
    /// the single-question form (`question` + optional `options` / `multi_select` / `header`) and
    /// the multi-question form (`questions: [{ question, header, options, multi_select }, …]`).
    /// `options` may be plain strings or `{label, description}` objects. Questions and options are
    /// capped, never rejected — an over-long ask is truncated to something answerable.
    pub fn from_args(args: &Value) -> Result<Ask> {
        let mut questions: Vec<AskQuestion> = Vec::new();

        if let Some(arr) = args.get("questions").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(q) = parse_question(item) {
                    questions.push(q);
                }
            }
        }
        // The single-question form is honoured too (and appended after any `questions` entries) so a
        // model that mixes the two shapes loses nothing.
        if let Some(q) = parse_question(args) {
            questions.push(q);
        }

        if questions.is_empty() {
            anyhow::bail!("clarify needs a non-empty `question` (or a `questions` array)");
        }
        if questions.len() > MAX_QUESTIONS {
            questions.truncate(MAX_QUESTIONS);
        }
        Ok(Ask { questions })
    }

    /// The user-facing text rendering: each question with its numbered options. This is what the
    /// plain REPL, Telegram and `aizen agent` show, and what the retained transcript keeps as a
    /// durable record of what was asked (the picker itself is dismissible).
    pub fn display(&self) -> String {
        let multi = self.questions.len() > 1;
        let mut out = String::new();
        for (i, q) in self.questions.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let head = if multi && !q.header.is_empty() {
                format!("[{}] {}", q.header, q.question)
            } else {
                q.question.clone()
            };
            out.push_str(&head);
            for (j, o) in q.options.iter().enumerate() {
                out.push_str(&format!("\n  {}. {}", j + 1, o.label));
            }
        }
        out
    }

    /// The model-facing acknowledgement, handed back as the `clarify` tool result: it names what was
    /// asked and instructs the model to STOP. Kept short — the display string already carries the
    /// detail to the user.
    pub fn ack(&self) -> String {
        let n = self.questions.len();
        let labels: Vec<String> = self
            .questions
            .iter()
            .map(|q| format!("\"{}\"", q.question))
            .collect();
        let opts: Vec<String> = self
            .questions
            .iter()
            .flat_map(|q| q.options.iter().map(|o| o.label.clone()))
            .collect();
        let opt_hint = if opts.is_empty() {
            String::new()
        } else {
            format!(" Suggested answers: {}.", opts.join(" / "))
        };
        let noun = if n == 1 { "question" } else { "questions" };
        format!(
            "Posed {n} {noun} to the user: {}.{opt_hint} STOP now — do NOT call clarify again or \
             answer on their behalf; their reply arrives as the next user message and you continue \
             from there.",
            labels.join("; ")
        )
    }

    /// Format the user's answers as the message text the next turn receives. `answers[i]` is the
    /// list of chosen labels (multi-select) or the single free-text answer for question `i`; an
    /// empty inner vec means that question was left blank. A single-question ask collapses to just
    /// the answer; a multi-question ask labels each answer with its question so the model can map
    /// them back.
    pub fn format_answers(&self, answers: &[Vec<String>]) -> String {
        let clean: Vec<Vec<String>> = (0..self.questions.len())
            .map(|i| {
                answers
                    .get(i)
                    .map(|a| {
                        a.iter()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        if self.questions.len() == 1 {
            return clean.first().cloned().unwrap_or_default().join(", ");
        }
        let mut out = String::new();
        for (i, q) in self.questions.iter().enumerate() {
            let a = &clean[i];
            if a.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{} → {}", q.question, a.join(", ")));
        }
        out
    }
}

/// Parse ONE question object (`{ question, header, options, multi_select }`). Returns `None` when
/// there is no non-empty question text, so a malformed entry is skipped rather than poisoning the
/// whole ask. `options` accepts strings or `{label, description}` objects.
fn parse_question(v: &Value) -> Option<AskQuestion> {
    let question = v
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let header = v
        .get("header")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .chars()
        .take(24)
        .collect::<String>();
    let mut options: Vec<AskOption> = v
        .get("options")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(parse_option).collect())
        .unwrap_or_default();
    options.truncate(MAX_OPTIONS);
    let multi_select = v
        .get("multi_select")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        // A multi-select with no options is meaningless — treat it as free text.
        && !options.is_empty();
    Some(AskQuestion {
        question,
        header,
        options,
        multi_select,
    })
}

/// Parse one option: a bare string becomes a label with no description; an object reads `label` +
/// optional `description`. A blank label is dropped.
fn parse_option(v: &Value) -> Option<AskOption> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| AskOption {
                label: s.to_string(),
                description: String::new(),
            })
        }
        Value::Object(_) => {
            let label = v
                .get("label")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .to_string();
            let description = v
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            Some(AskOption { label, description })
        }
        _ => None,
    }
}

/// The outstanding ask, set by `Clarify::execute` and drained by the agent loop (`take_pending`)
/// the same turn. `None` whenever no clarification is in flight. A turn that contains `clarify`
/// runs serially (it is not concurrency-safe), so there is never a race here.
static PENDING: Lazy<Mutex<Option<Ask>>> = Lazy::new(|| Mutex::new(None));

/// Take (and clear) the pending ask, if any. The agent loop calls this after executing a turn's tool
/// calls; `Some` means "a clarify fired this turn → stop and yield to the user".
pub fn take_pending() -> Option<Ask> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

fn set_pending(ask: Ask) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(ask);
}

/// Serializes every test (here AND in the agent-loop module) that touches the process-global
/// `PENDING` — cargo runs tests in parallel, so without this a concurrent set/take would interleave.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

pub struct Clarify;

impl Tool for Clarify {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Ask the user when the task is genuinely ambiguous and a wrong guess would waste real work \
         (which file, which framework, confirm a risky direction). The turn PAUSES; the user's next \
         message is the answer. Ask one question (`question` + optional `options`) or several at \
         once (`questions`) — shown as one panel. `multi_select: true` checks several options. For \
         unattended approve/deny use telegram_ask."
    }

    fn parameters(&self) -> Value {
        // One option item, reused by both forms: a plain string, or `{label, description}`.
        let option_item = json!({
            "anyOf": [
                {"type": "string"},
                {"type": "object", "properties": {
                    "label": {"type": "string"},
                    "description": {"type": "string"}
                }, "required": ["label"], "additionalProperties": false}
            ]
        });
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "single question"},
                "options": {"type": "array", "items": option_item},
                "multi_select": {"type": "boolean", "description": "check several options"},
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "header": {"type": "string", "description": "short tab label"},
                            "options": {"type": "array", "items": {"type": "string"}},
                            "multi_select": {"type": "boolean"}
                        },
                        "required": ["question"],
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        })
    }

    /// Asking changes nothing on disk and needs no approval.
    fn is_destructive(&self) -> bool {
        false
    }

    /// Control-flow tool with a process-global side effect → must run serially, never in a
    /// parallel batch.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let ask = Ask::from_args(args).context("invalid clarify arguments")?;
        let ack = ask.ack();
        set_pending(ask);
        Ok(ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_question_with_options_parses_and_renders() {
        let ask = Ask::from_args(&json!({
            "question": "Which file?",
            "options": ["src/a.rs", "src/b.rs"]
        }))
        .unwrap();
        assert_eq!(ask.questions.len(), 1);
        assert_eq!(ask.questions[0].options.len(), 2);
        assert!(!ask.questions[0].multi_select);
        assert_eq!(ask.display(), "Which file?\n  1. src/a.rs\n  2. src/b.rs");
        assert!(ask.ack().contains("Which file?"));
        assert!(ask.ack().contains("src/a.rs / src/b.rs"));
        assert!(ask.ack().contains("STOP now"));
    }

    #[test]
    fn options_accept_label_and_description_objects() {
        let ask = Ask::from_args(&json!({
            "question": "Pick one",
            "options": [
                {"label": "Fast", "description": "ship now"},
                {"label": "Correct"}
            ]
        }))
        .unwrap();
        let o = &ask.questions[0].options;
        assert_eq!(o[0].label, "Fast");
        assert_eq!(o[0].description, "ship now");
        assert_eq!(o[1].label, "Correct");
        assert_eq!(o[1].description, "");
    }

    #[test]
    fn multi_question_form_parses_all_and_numbers_by_tab_header() {
        let ask = Ask::from_args(&json!({
            "questions": [
                {"question": "Which DB?", "header": "DB", "options": ["pg", "sqlite"]},
                {"question": "Which cache?", "header": "Cache", "multi_select": true,
                 "options": ["redis", "memory"]}
            ]
        }))
        .unwrap();
        assert_eq!(ask.questions.len(), 2);
        assert!(ask.questions[1].multi_select);
        let d = ask.display();
        assert!(d.contains("[DB] Which DB?"), "{d}");
        assert!(d.contains("[Cache] Which cache?"), "{d}");
    }

    #[test]
    fn multi_select_without_options_degrades_to_free_text() {
        let ask = Ask::from_args(&json!({"question": "Anything?", "multi_select": true})).unwrap();
        assert!(!ask.questions[0].multi_select);
    }

    #[test]
    fn questions_and_options_are_capped_not_rejected() {
        let opts: Vec<String> = (0..20).map(|i| format!("o{i}")).collect();
        let qs: Vec<Value> = (0..9)
            .map(|i| json!({"question": format!("q{i}")}))
            .collect();
        let ask = Ask::from_args(&json!({"questions": qs, "options": opts})).unwrap();
        assert_eq!(ask.questions.len(), MAX_QUESTIONS); // 9 from `questions` + 1 top-level form → capped
                                                        // The single-question form's 20 options are capped.
        let ask2 = Ask::from_args(&json!({"question": "x", "options": (0..20).map(|i| format!("o{i}")).collect::<Vec<_>>()})).unwrap();
        assert_eq!(ask2.questions[0].options.len(), MAX_OPTIONS);
    }

    #[test]
    fn blank_question_is_rejected() {
        assert!(Ask::from_args(&json!({})).is_err());
        assert!(Ask::from_args(&json!({"question": "   "})).is_err());
    }

    #[test]
    fn format_answers_collapses_single_and_labels_multi() {
        let single = Ask::from_args(&json!({"question": "Q?"})).unwrap();
        assert_eq!(single.format_answers(&[vec!["hello".to_string()]]), "hello");
        let multi = Ask::from_args(&json!({
            "questions": [{"question": "Q1"}, {"question": "Q2"}]
        }))
        .unwrap();
        assert_eq!(
            multi.format_answers(&[vec!["a".into(), "b".into()], vec![]]),
            "Q1 → a, b"
        );
    }

    #[test]
    fn build_without_options_is_just_the_question() {
        let ask = Ask::from_args(&json!({"question": "  Proceed?  "})).unwrap();
        assert_eq!(ask.display(), "Proceed?");
        assert!(!ask.ack().contains("Suggested answers"));
    }

    #[test]
    fn execute_sets_pending_and_take_drains_it() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = take_pending(); // clear any leftover from another test
        let ack = Clarify
            .execute(&json!({"question": "A or B?", "options": ["A", "B"]}))
            .unwrap();
        assert!(ack.contains("A or B?"));
        let pending = take_pending().expect("an ask must be pending after execute");
        assert_eq!(pending.questions[0].question, "A or B?");
        assert_eq!(pending.questions[0].options.len(), 2);
        assert!(take_pending().is_none(), "take must drain");
    }

    #[test]
    fn execute_rejects_empty_or_missing_question() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(Clarify.execute(&json!({})).is_err(), "missing question");
        assert!(
            Clarify.execute(&json!({"question": "   "})).is_err(),
            "blank question"
        );
        let _ = take_pending();
    }

    #[test]
    fn flags_are_nondestructive_and_serial() {
        assert!(
            !Clarify.is_destructive(),
            "asking a question changes nothing"
        );
        assert!(
            !Clarify.is_concurrency_safe(),
            "global side effect → must run serially"
        );
    }
}
