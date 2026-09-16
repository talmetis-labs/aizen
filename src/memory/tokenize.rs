//! Lexical tokenizer for the relevance scorer.
//!
//! Unicode-aware (P6 — Vietnamese repair). The original path (lowercase → `[a-z0-9_]+`)
//! SHREDDED Vietnamese diacritics: a word like `cà phê đen` tokenized to garbage (`["ph","en"]`)
//! because the ASCII regex matched only the latin runs between accented codepoints. The fix is
//! three free, pure-Rust changes:
//!   1. NFC-normalize BEFORE lowercasing, so a decomposed `ế` (e + ◌̂ + ◌́) and a composed `ế`
//!      tokenize identically and combining marks aren't stranded.
//!   2. Widen the token regex `[a-z0-9_]+` → `[\p{L}\p{N}_]+` (letters of ANY script + digits +
//!      underscore), so accented/CJK/etc. letters are captured whole.
//!   3. A bilingual (EN + VI) stopword set so VN glue words don't dominate the match.
//!
//! NFC normalization is FREE: `icu_normalizer` (+ its compiled data) is already in the dependency
//! tree via url→idna, so this adds NO new crate and the single static binary is preserved
//! (verified against Cargo.lock 2026-06-25). Tokens are compared by CHARACTER count, not byte
//! length, so multibyte VN tokens aren't wrongly dropped.

use icu_normalizer::ComposingNormalizer;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

// Letters (any script, incl. composed Vietnamese), numbers, underscore — and a hyphen INSIDE a
// token, so a kebab-case identifier (`rate-limit`) is one token as well as its parts. `\p{L}`/
// `\p{N}` rely on regex's unicode feature, which is enabled by default.
static TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\p{L}\p{N}_]+(?:-[\p{L}\p{N}_]+)*").unwrap());

/// Bilingual stopword set: the original 48 English words (verbatim from the extension's proven
/// tokenizer, globalMemoryStore.ts:233) + ~45 high-frequency Vietnamese function words so VN text
/// isn't dominated by glue words ("và", "của", "là", …).
static STOPWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        // English (verbatim)
        "the", "a", "an", "of", "and", "or", "is", "to", "in", "on", "for", "with", "by", "at",
        "as", "be", "this", "that", "it", "from", "but", "not", "are", "was", "were", "has",
        "have", "had", "do", "does", "did", "i", "you", "we", "they", "he", "she", "them", "us",
        "my", "our", "your", "their", "its", "also", "just", "into", "than", "then", "so", "if",
        "else",
        // Vietnamese function words (high-frequency; content words like "cà phê", "dữ liệu" stay)
        "và", "là", "của", "có", "không", "được", "cho", "với", "các", "những", "một", "này", "đó",
        "đây", "khi", "để", "đã", "sẽ", "đang", "thì", "mà", "ra", "vào", "lên", "trong", "trên",
        "về", "từ", "đến", "theo", "như", "nếu", "vì", "nên", "hay", "hoặc", "cũng", "rất", "quá",
        "gì", "nào", "thế", "vậy", "làm",
    ]
    .into_iter()
    .collect()
});

/// Tokenize: NFC-normalize → split on `[\p{L}\p{N}_-]+` runs → lowercase, drop <2-char tokens +
/// stopwords. An identifier emits the whole token AND its words (`get_by_id` → `get_by_id`,
/// `get`, `id`; `getById` likewise; `rate-limit` → `rate-limit`, `rate`, `limit`), so "get by
/// id" meets `get_by_id` instead of scoring zero against it — half a coding corpus is
/// identifiers (quality plan M5). Case is read BEFORE lowercasing: that is where the camel
/// boundaries are.
pub fn tokenize(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    // NFC compose first (free; static compiled data).
    let normalized = ComposingNormalizer::new_nfc().normalize(s);
    let mut out = Vec::new();
    for m in TOKEN_RE.find_iter(&normalized) {
        let raw = m.as_str();
        push_token(&mut out, raw.to_lowercase());
        let parts = identifier_parts(raw);
        if parts.len() > 1 {
            for p in parts {
                push_token(&mut out, p.to_lowercase());
            }
        }
    }
    out
}

fn push_token(out: &mut Vec<String>, t: String) {
    if t.chars().count() < 2 || STOPWORDS.contains(t.as_str()) {
        return;
    }
    out.push(t);
}

/// The words of an identifier: snake and kebab segments, then camel boundaries inside each
/// (`getById` → get, By, Id; `HTTPServer` → HTTP, Server; `v2Beta` → v2, Beta). A plain word
/// comes back as itself, in one piece.
fn identifier_parts(raw: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for seg in raw.split(['_', '-']) {
        if seg.is_empty() {
            continue;
        }
        let chars: Vec<char> = seg.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (prev, cur) = (chars[i - 1], chars[i]);
            let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            let boundary = ((prev.is_lowercase() || prev.is_numeric()) && cur.is_uppercase())
                || (prev.is_uppercase() && cur.is_uppercase() && next_lower);
            if boundary {
                parts.push(chars[start..i].iter().collect());
                start = i;
            }
        }
        parts.push(chars[start..].iter().collect());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_short_and_stopwords() {
        // "a" (stopword), "is" (stopword), "x" (len<2) dropped; "auth"/"flow" kept.
        let t = tokenize("A login is the auth flow x");
        assert_eq!(t, vec!["login", "auth", "flow"]);
    }

    #[test]
    fn lowercases_and_keeps_identifiers() {
        // The whole identifier survives (a literal query for it still matches), and its words
        // follow; `by` is a stopword, `GPT5` has no boundary to split on.
        let t = tokenize("AiProxy get_by_id GPT5");
        assert_eq!(
            t,
            vec!["aiproxy", "ai", "proxy", "get_by_id", "get", "id", "gpt5"]
        );
    }

    #[test]
    fn identifiers_emit_the_whole_and_their_parts() {
        assert_eq!(tokenize("getById"), vec!["getbyid", "get", "id"]);
        assert_eq!(
            tokenize("HTTPServer rate-limit"),
            vec![
                "httpserver",
                "http",
                "server",
                "rate-limit",
                "rate",
                "limit"
            ]
        );
        assert_eq!(identifier_parts("v2Beta"), vec!["v2", "Beta"]);
        assert_eq!(identifier_parts("plain"), vec!["plain"]);
        // M5: "get by id" now meets `get_by_id` on every one of its tokens.
        let q = tokenize("get by id");
        let d = tokenize("fn get_by_id(x)");
        assert!(q.iter().all(|t| d.contains(t)), "{q:?} vs {d:?}");
    }

    #[test]
    fn empty_is_empty() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn vietnamese_diacritics_survive() {
        // The old [a-z0-9_]+ path shredded this to ["ph","en"]; now each syllable tokenizes whole.
        let t = tokenize("Cà phê đen buổi sáng");
        assert!(t.contains(&"cà".to_string()), "got {t:?}");
        assert!(t.contains(&"phê".to_string()), "got {t:?}");
        assert!(t.contains(&"đen".to_string()), "got {t:?}");
        assert!(t.contains(&"sáng".to_string()), "got {t:?}");
        // diacritics preserved, not stripped to ascii
        assert!(!t.contains(&"ca".to_string()));
    }

    #[test]
    fn nfc_and_nfd_tokenize_identically() {
        // composed "phê" (U+00EA) vs decomposed "phê" (e + U+0302 combining circumflex)
        let composed = "ph\u{00EA}";
        let decomposed = "phe\u{0302}";
        assert_eq!(tokenize(composed), tokenize(decomposed));
        assert_eq!(tokenize(composed), vec!["phê".to_string()]);
    }

    #[test]
    fn vietnamese_stopwords_dropped() {
        // và / của are VN glue words → dropped; content "liệu" kept.
        let t = tokenize("và của hệ thống dữ liệu");
        assert!(!t.contains(&"và".to_string()), "got {t:?}");
        assert!(!t.contains(&"của".to_string()), "got {t:?}");
        assert!(t.contains(&"liệu".to_string()), "got {t:?}");
    }
}
