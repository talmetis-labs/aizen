//! The shape of a user turn — question · small edit · multi-file · research — decided from the
//! prompt alone, before any model call, in English or Vietnamese.
//!
//! Two things hang off it. Per TURN, a pure question skips the memory-recall and gated-skills
//! blocks that every turn used to pay for, so a chat-shaped question is answered in one request
//! against the cached prefix. Per CONVERSATION, the widest shape seen so far decides which
//! built-in tools ride on every request and which are deferred behind `tool_search` (see
//! `agent::builtin::deferred_builtins`); it only ever widens, so the advertised tool list stays
//! byte-stable across turns and the provider's prompt cache keeps covering it.
//!
//! Precision-first: anything that is not clearly a question or clearly research is treated as
//! an edit, the shape that keeps the most context. Misclassifying an edit as a question would
//! cost the model the recall block; misclassifying a question as an edit costs a few hundred
//! tokens. The classifier is pure and dependency-free.

use std::sync::Mutex;

/// Ordered by how much of the surface the shape needs: a later variant is "wider".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TurnShape {
    /// Asks for an explanation or a fact; nothing is to be changed.
    Question,
    /// A change scoped to one place — the default when nothing says otherwise.
    SmallEdit,
    /// A change across many files or the whole tree.
    MultiFile,
    /// Investigate, compare, audit: read widely (files and the web), change nothing yet.
    Research,
}

impl TurnShape {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnShape::Question => "question",
            TurnShape::SmallEdit => "small-edit",
            TurnShape::MultiFile => "multi-file",
            TurnShape::Research => "research",
        }
    }
}

/// Verbs that mean "change something" (English single words, Vietnamese words/phrases).
const EDIT_WORDS: &[&str] = &[
    "fix",
    "add",
    "implement",
    "change",
    "refactor",
    "write",
    "create",
    "remove",
    "delete",
    "update",
    "rename",
    "move",
    "make",
    "build",
    "migrate",
    "replace",
    "extract",
    "convert",
    "wire",
    "patch",
    "apply",
    "edit",
    "insert",
    "append",
    "bump",
    "upgrade",
    "rewrite",
    "revert",
    "commit",
    "install",
    "generate",
    "improve",
    "optimize",
    "optimise",
    "clean",
    "split",
    "merge",
    // Vietnamese
    "sửa",
    "thêm",
    "viết",
    "tạo",
    "xóa",
    "xoá",
    "đổi",
    "cập nhật",
    "triển khai",
    "chuyển",
    "gộp",
    "tách",
    "nâng cấp",
    "bỏ",
    "dời",
    "chỉnh",
    "làm",
    "cài",
    "tối ưu",
    "refactor",
    "đẩy",
    "commit",
];

/// A turn that opens with one of these, or ends with `?`, and names no edit is a question.
const QUESTION_HEADS: &[&str] = &[
    "what",
    "why",
    "how",
    "does",
    "do",
    "did",
    "is",
    "are",
    "was",
    "were",
    "can",
    "could",
    "should",
    "would",
    "will",
    "where",
    "which",
    "who",
    "when",
    "explain",
    "describe",
    "tell me",
    "show me",
    "summarize",
    "summarise",
    "list",
    // Vietnamese
    "tại sao",
    "vì sao",
    "thế nào",
    "như nào",
    "như thế nào",
    "giải thích",
    "ở đâu",
    "cái gì",
    "có phải",
    "mô tả",
    "tóm tắt",
    "liệt kê",
    "cho tôi biết",
    "nói",
    "kể",
];

/// Vietnamese questions often carry the marker at the END ("... là gì", "... không").
const QUESTION_TAILS: &[&str] = &[
    "là gì",
    "không",
    "chưa",
    "à",
    "nhỉ",
    "hả",
    "sao",
    "thế nào",
    "như nào",
    "ra sao",
    "ở đâu",
    "khi nào",
    "bao nhiêu",
];

/// Interrogative phrases that mark a question wherever they sit in the sentence.
const QUESTION_ANYWHERE: &[&str] = &[
    "what is",
    "what does",
    "what are",
    "how does",
    "how do",
    "how is",
    "why does",
    "why is",
    "why do",
    "explain",
    "is there",
    "are there",
    // Vietnamese
    "tại sao",
    "vì sao",
    "thế nào",
    "như nào",
    "như thế nào",
    "ra sao",
    "là gì",
    "ở đâu",
    "khi nào",
    "bao nhiêu",
    "có phải",
    "giải thích",
];

/// Edit words that also mean something else in ordinary sentences — "thêm" is "more",
/// "làm" is "do", "make" is "make sense". They still count for the wide/small split, but they
/// cannot on their own turn a question or a research request into an edit.
const WEAK_EDIT_WORDS: &[&str] = &["thêm", "làm", "make", "build", "apply", "clean"];

const RESEARCH_WORDS: &[&str] = &[
    "research",
    "investigate",
    "audit",
    "compare",
    "survey",
    "analyze",
    "analyse",
    "benchmark",
    "evaluate",
    "assess",
    "review",
    "look into",
    "find out",
    "explore",
    // Vietnamese
    "nghiên cứu",
    "tìm hiểu",
    "phân tích",
    "rà soát",
    "so sánh",
    "khảo sát",
    "đánh giá",
    "điều tra",
    "review",
    "audit",
];

const MULTI_WORDS: &[&str] = &[
    "across",
    "all files",
    "every file",
    "throughout",
    "whole",
    "entire",
    "codebase",
    "repo-wide",
    "crate-wide",
    "everywhere",
    "all the",
    "each module",
    "every module",
    // Vietnamese
    "toàn bộ",
    "tất cả",
    "mọi file",
    "khắp",
    "cả repo",
    "toàn repo",
    "mọi nơi",
    "cả dự án",
];

const PATH_EXTS: &[&str] = &[
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".kt", ".cs", ".c", ".h", ".cpp",
    ".md", ".toml", ".json", ".yaml", ".yml", ".sh", ".ps1", ".sql", ".css", ".html",
];

/// Whole-word match, Unicode-aware: a hit must not be glued to letters or digits on either side
/// (so `just` does not fire on `adjust`, and `sửa` does not fire inside `sửa_chữa`).
fn has_word(hay: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = hay[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_ok = hay[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

fn any_word(hay: &str, words: &[&str]) -> bool {
    words.iter().any(|w| has_word(hay, w))
}

/// Tokens that look like file paths or file names.
fn path_count(p: &str) -> usize {
    p.split_whitespace()
        .filter(|t| {
            let t = t.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.');
            (t.contains('/') && t.len() > 2 && !t.starts_with("http"))
                || PATH_EXTS.iter().any(|e| t.ends_with(e))
        })
        .count()
}

/// PURE. Classify one user turn.
pub fn classify(prompt: &str) -> TurnShape {
    let p = prompt.trim().to_lowercase();
    if p.is_empty() {
        return TurnShape::SmallEdit;
    }
    let edit = any_word(&p, EDIT_WORDS);
    let edit_strong = EDIT_WORDS
        .iter()
        .any(|w| !WEAK_EDIT_WORDS.contains(w) && has_word(&p, w));
    let research = any_word(&p, RESEARCH_WORDS);
    let paths = path_count(&p);
    let words = p.split_whitespace().count();
    let question = (p.ends_with('?')
        || QUESTION_HEADS.iter().any(|h| {
            p.starts_with(h) && {
                // The head must be the whole first word(s): "isolate" is not "is".
                let rest = &p[h.len()..];
                rest.is_empty() || !rest.chars().next().is_some_and(|c| c.is_alphanumeric())
            }
        })
        || QUESTION_TAILS
            .iter()
            .any(|t| p.trim_end_matches(['?', '.', '!']).ends_with(t))
        || any_word(&p, QUESTION_ANYWHERE))
        && !edit_strong;
    if research && !edit_strong {
        return TurnShape::Research;
    }
    if question {
        return TurnShape::Question;
    }
    if any_word(&p, MULTI_WORDS) || paths >= 3 || (edit && words > 120) {
        return TurnShape::MultiFile;
    }
    TurnShape::SmallEdit
}

/// The conversation's shape so far: the widest turn seen since the last reset.
static CONVERSATION: Mutex<Option<TurnShape>> = Mutex::new(None);
/// The shape of the turn being seated right now, un-widened — what the persona gate reads.
static THIS_TURN: Mutex<Option<TurnShape>> = Mutex::new(None);

/// Record this turn's shape and return the conversation's (widened) shape. Widening only, so
/// the deferred tool set — and with it the advertised tool list — changes at most a few times
/// per conversation and never flips back and forth.
pub fn note_turn(shape: TurnShape) -> TurnShape {
    *THIS_TURN.lock().unwrap_or_else(|e| e.into_inner()) = Some(shape);
    let mut g = CONVERSATION.lock().unwrap_or_else(|e| e.into_inner());
    let next = g.map_or(shape, |cur| cur.max(shape));
    *g = Some(next);
    next
}

pub fn conversation_shape() -> Option<TurnShape> {
    *CONVERSATION.lock().unwrap_or_else(|e| e.into_inner())
}

/// The shape of the current turn alone (not widened by earlier turns), `None` before the first.
pub fn current_turn_shape() -> Option<TurnShape> {
    *THIS_TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// `/clear`, `/new`, a fresh thread: the next turn decides afresh.
pub fn reset_conversation() {
    *CONVERSATION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *THIS_TURN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_in_both_languages() {
        for q in [
            "What does the verify gate do?",
            "how is the todo list cleared",
            "Explain the compaction cut.",
            "is the cache prefix stable?",
            "tại sao test này fail?",
            "verify gate hoạt động thế nào",
            "cái này là gì",
            "aizen có dùng rustls không",
            "làm sao để chạy bench tasks?",
            "cho tôi biết todo được clear khi nào",
        ] {
            assert_eq!(classify(q), TurnShape::Question, "{q}");
        }
    }

    #[test]
    fn an_edit_verb_or_a_path_makes_it_an_edit_even_with_a_question_mark() {
        for e in [
            "can you fix the failing test?",
            "add a --json flag to bench tasks",
            "src/agent/mod.rs has a bug near the verify gate",
            "sửa lỗi ở cmd_guard",
            "thêm test cho replay",
            "the build is broken",
            "isolate the failure",
        ] {
            assert_eq!(classify(e), TurnShape::SmallEdit, "{e}");
        }
    }

    #[test]
    fn wide_changes_and_research_are_told_apart() {
        assert_eq!(
            classify("rename estimate_tokens across the whole codebase"),
            TurnShape::MultiFile
        );
        assert_eq!(
            classify("đổi tên hàm này trong toàn bộ dự án"),
            TurnShape::MultiFile
        );
        assert_eq!(
            classify("update src/a.rs, src/b.rs and src/c.rs to use the new estimator"),
            TurnShape::MultiFile
        );
        assert_eq!(
            classify("research how other agents handle prompt caching"),
            TurnShape::Research
        );
        assert_eq!(
            classify("nghiên cứu thêm về cách làm nhanh và gọn"),
            TurnShape::Research
        );
        assert_eq!(
            classify("investigate and then fix the stall"),
            TurnShape::SmallEdit,
            "research plus an edit verb is an edit"
        );
    }

    #[test]
    fn conversation_shape_only_widens_until_reset() {
        reset_conversation();
        assert_eq!(conversation_shape(), None);
        assert_eq!(note_turn(TurnShape::Question), TurnShape::Question);
        assert_eq!(note_turn(TurnShape::MultiFile), TurnShape::MultiFile);
        assert_eq!(
            note_turn(TurnShape::Question),
            TurnShape::MultiFile,
            "a later question does not narrow the surface"
        );
        reset_conversation();
        assert_eq!(note_turn(TurnShape::SmallEdit), TurnShape::SmallEdit);
        reset_conversation();
    }
}
