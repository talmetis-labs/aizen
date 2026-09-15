//! One token estimator for the whole tree.
//!
//! Every budget in aizen — the context guards, the memory caps, the schema ratchet, the codebase
//! index, the MCP schema budget — used to divide a character count by four. That is a fair guess
//! for English prose and code, and roughly half the truth for anything a tokenizer splits per
//! grapheme: CJK, Hangul, combining marks, and the precomposed Vietnamese letters in Latin
//! Extended Additional. A cap of "2,000 tokens" of Vietnamese memory was really 4,000 on the
//! wire, and the 60/80/90 % context guards fired late on a Vietnamese session. This is the one
//! place the rule lives, so every cap means the same thing.
//!
//! Rule: a "light" char (ASCII, Latin-1 Supplement, Latin Extended-A/B — the `< U+0250` block)
//! counts 1/4 token; every other char counts 1/1.8. For pure-ASCII text the result is exactly
//! `chars / 4`, so nothing measured in English moves. No tokenizer dependency, no per-model
//! tables: the point is one consistent number that does not under-count by half, not exactness.

/// Chars a BPE tokenizer tends to split into two or more tokens.
pub fn is_heavy_char(c: char) -> bool {
    !(c.is_ascii() || ('\u{00A0}'..='\u{024F}').contains(&c))
}

/// Light chars at 1/4 token, heavy chars at 1/1.8 token (9/36 and 20/36), rounded down.
pub fn weighted(light: usize, heavy: usize) -> usize {
    (light * 9 + heavy * 20) / 36
}

/// Same weights, rounded up — for caps that must never admit one char more than they say.
pub fn weighted_ceil(light: usize, heavy: usize) -> usize {
    (light * 9 + heavy * 20).div_ceil(36)
}

/// Accumulates light/heavy counts across several strings so the division happens once (a
/// per-part floor would drift from the old single-division figure on ASCII text).
#[derive(Debug, Default, Clone, Copy)]
pub struct Counter {
    pub light: usize,
    pub heavy: usize,
}

impl Counter {
    pub fn add(&mut self, s: &str) -> &mut Self {
        for c in s.chars() {
            if is_heavy_char(c) {
                self.heavy += 1;
            } else {
                self.light += 1;
            }
        }
        self
    }

    /// Count `n` chars as light without scanning anything (an envelope allowance).
    pub fn add_light(&mut self, n: usize) -> &mut Self {
        self.light += n;
        self
    }

    pub fn tokens(&self) -> usize {
        weighted(self.light, self.heavy)
    }

    pub fn tokens_ceil(&self) -> usize {
        weighted_ceil(self.light, self.heavy)
    }
}

/// Estimated tokens of one string, rounded down.
pub fn estimate_str(s: &str) -> usize {
    Counter::default().add(s).tokens()
}

/// Estimated tokens of one string, rounded up.
pub fn estimate_str_ceil(s: &str) -> usize {
    Counter::default().add(s).tokens_ceil()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_exactly_chars_over_four() {
        let s = "a".repeat(400);
        assert_eq!(estimate_str(&s), 100);
        assert_eq!(estimate_str("abcde"), 1, "floor, like the old chars/4");
        assert_eq!(estimate_str_ceil("abcde"), 2, "ceil variant for caps");
        assert_eq!(estimate_str(""), 0);
    }

    #[test]
    fn latin_1_and_extended_a_are_light_but_vietnamese_precomposed_is_heavy() {
        assert!(!is_heavy_char('é'), "Latin-1 Supplement");
        assert!(!is_heavy_char('ł'), "Latin Extended-A");
        assert!(is_heavy_char('ế'), "Latin Extended Additional (Vietnamese)");
        assert!(is_heavy_char('ạ'));
        assert!(is_heavy_char('\u{0301}'), "combining acute");
        assert!(is_heavy_char('漢'));
        assert!(is_heavy_char('한'));
        assert!(is_heavy_char('😀'));
    }

    #[test]
    fn heavy_text_estimates_near_chars_over_1_8() {
        let cjk = "漢".repeat(18);
        assert_eq!(estimate_str(&cjk), 10);
        // A Vietnamese sentence: ô, ư and é are Latin-1 / Extended-B (light); ố, ố, ệ, ạ are
        // Latin Extended Additional (heavy).
        let vi = "Tôi muốn tối ưu aizen hiện tại nhé";
        let mut light = 0;
        let mut heavy = 0;
        for c in vi.chars() {
            if is_heavy_char(c) {
                heavy += 1;
            } else {
                light += 1;
            }
        }
        assert_eq!(light + heavy, vi.chars().count());
        assert_eq!(heavy, 4, "precomposed letters counted as heavy");
        assert!(
            estimate_str(vi) > vi.chars().count() / 4,
            "must come out above the old chars/4 figure"
        );
    }

    #[test]
    fn counter_matches_one_division_over_the_concatenation() {
        let mut c = Counter::default();
        c.add("hello ").add("wörld").add_light(24);
        let joined = format!("{}{}{}", "hello ", "wörld", "x".repeat(24));
        assert_eq!(c.tokens(), estimate_str(&joined));
    }
}
