// Copyright 2026 Aizen authors. Licensed under Apache-2.0.
// Extracted from Aizen; original implementation comments and tests retained below.

//! Corpus-relative BM25 ranking with the legacy Jaro-Winkler fuzzy bridge.

use std::collections::{HashMap, HashSet};

/// BM25 tuning. Defaults are the bench-confirmed Okapi values; exposed for a grid-search.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    pub k1: f64,
    pub b: f64,
    /// Fuzzy bridge: a query term with no exact doc match is matched against the doc's terms
    /// by Jaro-Winkler; a best sim ≥ this floor counts as a damped hit. Conservative (0.92) so
    /// fuzzy never out-scores an exact hit and false-bridges stay rare. `0.0` disables fuzzy.
    pub fuzzy_min_sim: f64,
    /// Only query terms at least this long are eligible for the fuzzy bridge (short tokens are
    /// noisy under edit-distance, e.g. `test`/`text`).
    pub fuzzy_min_len: usize,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Bm25Params {
            k1: 1.2,
            b: 0.75,
            fuzzy_min_sim: 0.92,
            fuzzy_min_len: 4,
        }
    }
}

/// A BM25 index over a corpus of pre-tokenized docs: document frequencies + average doc length,
/// computed once per search (the personal corpus is small — O(N) build is < 1ms at 1k facts).
pub struct Bm25Index {
    df: HashMap<String, u32>,
    n_docs: f64,
    avgdl: f64,
    params: Bm25Params,
}

impl Bm25Index {
    /// Build from a corpus of token lists (one per doc). Empty corpus → a valid empty index.
    pub fn build<'a, I>(docs: I) -> Bm25Index
    where
        I: IntoIterator<Item = &'a [String]>,
    {
        Bm25Index::build_with(docs, Bm25Params::default())
    }

    pub fn build_with<'a, I>(docs: I, params: Bm25Params) -> Bm25Index
    where
        I: IntoIterator<Item = &'a [String]>,
    {
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut n_docs = 0u64;
        let mut total_len = 0u64;
        for doc in docs {
            n_docs += 1;
            total_len += doc.len() as u64;
            let mut seen: HashSet<&str> = HashSet::new();
            for t in doc {
                if seen.insert(t.as_str()) {
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        let avgdl = if n_docs == 0 {
            1.0
        } else {
            (total_len as f64 / n_docs as f64).max(1.0)
        };
        Bm25Index {
            df,
            n_docs: n_docs as f64,
            avgdl,
            params,
        }
    }

    /// Lucene/Okapi floored IDF: `ln(1 + (N - df + 0.5)/(df + 0.5))` — always ≥ 0, stable on a
    /// tiny personal corpus (a term appearing in every doc still scores a small positive, so an
    /// overlapping doc is never filtered out as a zero).
    pub fn idf(&self, term: &str) -> f64 {
        let df = *self.df.get(term).unwrap_or(&0) as f64;
        (1.0 + (self.n_docs - df + 0.5) / (df + 0.5)).ln()
    }

    /// The length-normalization denominator factor `k1·(1 - b + b·|D|/avgdl)`.
    fn norm(&self, doc_len: f64) -> f64 {
        self.params.k1 * (1.0 - self.params.b + self.params.b * (doc_len / self.avgdl))
    }

    /// Exact BM25 baseline retained for tests comparing the production fuzzy bridge.
    #[cfg(test)]
    pub fn score(&self, q_tokens: &[String], doc_tokens: &[String]) -> f64 {
        self.score_inner(q_tokens, doc_tokens, false)
    }

    /// BM25 + fuzzy fallback (the production retrieval path).
    pub fn score_fuzzy(&self, q_tokens: &[String], doc_tokens: &[String]) -> f64 {
        self.score_inner(q_tokens, doc_tokens, true)
    }

    fn score_inner(&self, q_tokens: &[String], doc_tokens: &[String], fuzzy: bool) -> f64 {
        if q_tokens.is_empty() || doc_tokens.is_empty() {
            return 0.0;
        }
        let mut tf: HashMap<&str, u32> = HashMap::new();
        for t in doc_tokens {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        let norm = self.norm(doc_tokens.len() as f64);
        let k1 = self.params.k1;
        let q_set: HashSet<&str> = q_tokens.iter().map(String::as_str).collect();

        let mut score = 0.0;
        for t in &q_set {
            if let Some(&f) = tf.get(*t) {
                let f = f as f64;
                score += self.idf(t) * (f * (k1 + 1.0)) / (f + norm);
            } else if fuzzy
                && self.params.fuzzy_min_sim > 0.0
                && t.chars().count() >= self.params.fuzzy_min_len
            {
                // No exact hit — bridge to the closest doc term (typo / morphology).
                if let Some((best_term, sim)) = best_fuzzy(t, &tf, self.params.fuzzy_min_sim) {
                    // Damped single-occurrence hit, scored against the matched term's rarity.
                    score += self.idf(best_term) * sim * (1.0 * (k1 + 1.0)) / (1.0 + norm);
                }
            }
        }
        score
    }
}

/// Best fuzzy match for `q_term` among the doc's terms by Jaro-Winkler, gated by `min_sim`.
fn best_fuzzy<'a>(
    q_term: &str,
    doc_tf: &HashMap<&'a str, u32>,
    min_sim: f64,
) -> Option<(&'a str, f64)> {
    let mut best: Option<(&str, f64)> = None;
    for &dt in doc_tf.keys() {
        // skip equal-or-trivial-length doc terms that can't be a meaningful near-match
        if dt.chars().count() < 3 {
            continue;
        }
        let sim = strsim::jaro_winkler(q_term, dt);
        if sim >= min_sim && best.map(|(_, bs)| sim > bs).unwrap_or(true) {
            best = Some((dt, sim));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::tokenize::tokenize;

    fn corpus3() -> Vec<Vec<String>> {
        vec![
            tokenize("postgres index tuning and query plans"),
            tokenize("react suspense and tanstack query"),
            tokenize("auth login oauth jwt session refresh"),
        ]
    }

    #[test]
    fn bm25_ranks_rarer_term_higher() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        // "oauth" is rare (1 doc) → the auth doc must win for an oauth query.
        let s_auth = idx.score(&tokenize("oauth login"), &docs[2]);
        let s_pg = idx.score(&tokenize("oauth login"), &docs[0]);
        assert!(s_auth > s_pg, "auth={s_auth} pg={s_pg}");
        assert_eq!(s_pg, 0.0, "no overlap → zero");
    }

    #[test]
    fn bm25_idf_is_non_negative_on_ubiquitous_term() {
        // "query" appears in 2 of 3 docs; IDF stays ≥0 (floored form) so an overlap never zeroes.
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        let s = idx.score(&tokenize("query"), &docs[0]);
        assert!(
            s > 0.0,
            "ubiquitous-but-present term still scores positive, got {s}"
        );
    }

    #[test]
    fn bm25_empty_query_or_doc_is_zero() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        assert_eq!(idx.score(&[], &docs[0]), 0.0);
        assert_eq!(idx.score(&tokenize("auth"), &[]), 0.0);
    }

    #[test]
    fn bm25_length_norm_prefers_concise_match() {
        // Same single hit, but a shorter doc should score higher (length normalization).
        let short = tokenize("kubernetes deployment");
        let long = tokenize(&format!(
            "kubernetes deployment {}",
            "unrelated filler words ".repeat(20)
        ));
        let docs = [short.clone(), long.clone()];
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        let s_short = idx.score(&tokenize("kubernetes"), &short);
        let s_long = idx.score(&tokenize("kubernetes"), &long);
        assert!(s_short > s_long, "short={s_short} long={s_long}");
    }

    #[test]
    fn fuzzy_bridges_a_typo_that_exact_bm25_misses() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        // "postgers" is a typo of "postgres" — exact BM25 scores 0, fuzzy bridges it.
        let exact = idx.score(&tokenize("postgers tuning"), &docs[0]);
        let fuzzy = idx.score_fuzzy(&tokenize("postgers tuning"), &docs[0]);
        // "tuning" hits exactly in both; the typo only contributes under fuzzy.
        assert!(fuzzy > exact, "fuzzy={fuzzy} should beat exact={exact}");
    }

    #[test]
    fn fuzzy_never_out_scores_an_exact_hit() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        let exact = idx.score_fuzzy(&tokenize("postgres"), &docs[0]);
        let typo = idx.score_fuzzy(&tokenize("postgers"), &docs[0]);
        assert!(exact > typo, "exact={exact} must beat fuzzy={typo}");
    }
}
