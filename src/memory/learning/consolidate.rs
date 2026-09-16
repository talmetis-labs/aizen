//! Mem0-style consolidation: before persisting a new fact, check it against the
//! existing store. A near-duplicate UPDATEs (reinforces) the existing fact instead of
//! inserting a twin — this is the primary anti-bloat lever on the write path (the
//! semantic-dup variant via cosine lands with the dense tier in P5).

use crate::memory::bloat::dedup;
use crate::memory::learning::match_text;
use crate::memory::path_scope::Tier;
use crate::memory::score::lexical_score_tokens;
use crate::memory::store::MemoryEntry;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemOp {
    /// No similar fact exists — insert a new one.
    Add,
    /// A near-duplicate exists — reinforce it (bump recency/frequency) instead.
    Reinforce { id: String },
}

/// Lowest lexical score for a same-slot supersede on a **Correction** turn (below
/// [`learn_dedup_threshold`] so we do not reinforce the stale fact).
pub const SUPERSEDE_SLOT_MIN: f64 = 0.52;

/// Best lexical match in `existing` for `candidate_tokens`.
pub fn best_match(candidate_tokens: &[String], existing: &[MemoryEntry]) -> Option<(String, f64)> {
    let mut best: Option<(String, f64)> = None;
    for e in existing {
        let s = lexical_score_tokens(candidate_tokens, &e.tokens);
        if best.as_ref().map(|(_, bs)| s > *bs).unwrap_or(true) {
            best = Some((e.id.clone(), s));
        }
    }
    best
}

/// Stage 2 looks again at a candidate whose lexical score landed in `[STAGE2_LEXICAL_MIN,
/// dedup_threshold)`: close enough that a reworded restatement is plausible, not close enough for
/// the token scorer to say so on its own.
pub const STAGE2_LEXICAL_MIN: f64 = 0.45;
/// Stage 2 floor on the character-level MinHash (5-gram shingles): survives reordering, typos and
/// small edits, not a genuinely different sentence.
pub const STAGE2_MINHASH_MIN: f64 = 0.60;
/// Stage 2 floor on the normalised-token measure ([`match_text::match_similarity`]: the max of
/// Jaccard and guarded containment over accent-folded, particle-stripped tokens).
pub const STAGE2_SHAPE_MIN: f64 = 0.55;

/// Which stage called a candidate a duplicate — written on the audit line, so the write path can
/// be measured stage by stage instead of guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The token-level score cleared `dedup_threshold` outright.
    Lexical,
    /// The token-level score sat in the stage-2 band and both shape measures agreed.
    Shape,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Lexical => "lexical",
            Stage::Shape => "shape",
        }
    }
}

/// The two-stage duplicate check. Stage 1 is the lexical best match at `dedup_threshold`; when
/// nothing clears it, stage 2 re-examines every entry whose lexical score sits in the band with
/// MinHash + normalised tokens, and a hit needs BOTH. Returns the winning entry, its score and the
/// stage that decided — `None` means "genuinely new, insert it".
///
/// Measured before this existed (quality plan M1): one best match at 0.78 within one partition
/// produced 0 `reinforce` events over 1,056 audit lines while the store held plain restatements,
/// so every inferred fact stayed at `sessions: 1` and the frozen core stayed empty.
pub fn find_duplicate<'a, I>(
    candidate: &str,
    candidate_tokens: &[String],
    pool: I,
    dedup_threshold: f64,
) -> Option<(String, f64, Stage)>
where
    I: IntoIterator<Item = &'a MemoryEntry>,
{
    if candidate_tokens.is_empty() {
        return None;
    }
    let mut best_lex: Option<(&MemoryEntry, f64)> = None;
    let mut band: Vec<&MemoryEntry> = Vec::new();
    for e in pool {
        let s = lexical_score_tokens(candidate_tokens, &e.tokens);
        if best_lex.map(|(_, b)| s > b).unwrap_or(true) {
            best_lex = Some((e, s));
        }
        if s >= STAGE2_LEXICAL_MIN && s < dedup_threshold {
            band.push(e);
        }
    }
    if let Some((e, s)) = best_lex {
        if s >= dedup_threshold {
            return Some((e.id.clone(), s, Stage::Lexical));
        }
    }
    if band.is_empty() {
        return None;
    }
    let sig = dedup::signature(candidate);
    let mut best_shape: Option<(&MemoryEntry, f64)> = None;
    for e in band {
        let minhash = dedup::similarity(&sig, &dedup::signature(&e.body));
        if minhash < STAGE2_MINHASH_MIN {
            continue;
        }
        let shape = match_text::match_similarity(candidate, &e.body);
        if shape < STAGE2_SHAPE_MIN {
            continue;
        }
        let score = minhash.min(shape);
        if best_shape.map(|(_, b)| score > b).unwrap_or(true) {
            best_shape = Some((e, score));
        }
    }
    best_shape.map(|(e, s)| (e.id.clone(), s, Stage::Shape))
}

/// Two entries may merge when they sit in the same partition, or in DIFFERENT tiers. The same
/// tier at another anchor or device stays apart on purpose: two checkouts, or two machines, can
/// hold genuinely different truths about the same sentence. A sentence that landed in two tiers
/// is a classification artefact (quality plan M3), not two truths.
pub fn may_merge(a: &MemoryEntry, b: &MemoryEntry) -> bool {
    if a.tier != b.tier {
        return true;
    }
    match a.tier {
        Tier::User => true,
        Tier::Device => a.device == b.device,
        Tier::Place => a.anchor == b.anchor,
    }
}

/// One duplicate the store-wide pass found: `dup` says what `keep` already says.
#[derive(Debug, Clone, PartialEq)]
pub struct DupPair {
    pub keep: String,
    pub dup: String,
    pub score: f64,
    pub stage: Stage,
}

/// Which of two duplicates survives: more confirmations, then more sessions, then more
/// reinforcements, then the older row, then the smaller id — a total order, so a re-run makes the
/// same choice.
fn keeper<'a>(a: &'a MemoryEntry, b: &'a MemoryEntry) -> &'a MemoryEntry {
    let key = |e: &MemoryEntry| {
        (
            e.confirmations,
            e.sessions,
            e.reinforced,
            std::cmp::Reverse(
                e.created
                    .clone()
                    .unwrap_or_else(|| "9999-99-99".to_string()),
            ),
            std::cmp::Reverse(e.id.clone()),
        )
    };
    if key(b) > key(a) {
        b
    } else {
        a
    }
}

/// The store-wide, model-free pass: every live entry against the entries it [`may_merge`] with,
/// through [`find_duplicate`]. Each unordered pair is considered once, and a retired entry never
/// becomes a keeper — its keeper stands in — so a cluster {a, b, c} ends with exactly one survivor
/// rather than three pairwise-correct retirements that empty it.
pub fn plan_pass(live: &[MemoryEntry], dedup_threshold: f64) -> Vec<DupPair> {
    let by_id: HashMap<&str, &MemoryEntry> = live.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut hits: Vec<(String, String, f64, Stage)> = Vec::new();
    for e in live {
        let pool = live.iter().filter(|o| o.id != e.id && may_merge(e, o));
        let Some((other, score, stage)) = find_duplicate(&e.body, &e.tokens, pool, dedup_threshold)
        else {
            continue;
        };
        let key = if e.id < other {
            (e.id.clone(), other.clone())
        } else {
            (other.clone(), e.id.clone())
        };
        if !seen.insert(key) {
            continue;
        }
        hits.push((e.id.clone(), other, score, stage));
    }
    hits.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut keeper_of: HashMap<String, String> = HashMap::new();
    let resolve = |id: &str, m: &HashMap<String, String>| -> String {
        let mut cur = id.to_string();
        while let Some(k) = m.get(&cur) {
            cur = k.clone();
        }
        cur
    };
    let mut out = Vec::new();
    for (a, b, score, stage) in hits {
        let a = resolve(&a, &keeper_of);
        let b = resolve(&b, &keeper_of);
        if a == b {
            continue; // both already merged into the same survivor
        }
        let (Some(ea), Some(eb)) = (by_id.get(a.as_str()), by_id.get(b.as_str())) else {
            continue;
        };
        if !may_merge(ea, eb) {
            continue; // a redirect landed on a keeper this row must stay apart from
        }
        let keep = keeper(ea, eb).id.clone();
        let dup = if keep == a { b } else { a };
        keeper_of.insert(dup.clone(), keep.clone());
        out.push(DupPair {
            keep,
            dup,
            score,
            stage,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryType;
    use crate::memory::tokenize::tokenize;

    fn entry(id: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            mtype: MemoryType::User,
            body: body.into(),
            tokens: tokenize(body),
            ..Default::default()
        }
    }

    fn decide(candidate_tokens: &[String], existing: &[MemoryEntry], threshold: f64) -> MemOp {
        let text = candidate_tokens.join(" ");
        match find_duplicate(&text, candidate_tokens, existing, threshold) {
            Some((id, _, _)) => MemOp::Reinforce { id },
            None => MemOp::Add,
        }
    }

    fn tiered(id: &str, body: &str, tier: Tier, anchor: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            tier,
            anchor: anchor.map(str::to_string),
            ..entry(id, body)
        }
    }

    #[test]
    fn stage_two_catches_a_near_identical_restatement_stage_one_missed() {
        let pool = vec![entry(
            "deploy",
            "the deploy pipeline uses fly and rolls back on a failed health check",
        )];
        let cand = "the deploy pipeline uses fly and rolls back on failed health checks";
        // An impossible stage-1 bar: this can only be a stage-2 hit.
        let hit = find_duplicate(cand, &tokenize(cand), &pool, 0.99).expect("stage 2 hit");
        assert_eq!((hit.0.as_str(), hit.2), ("deploy", Stage::Shape));
        // One changed word is a different fact: in the lexical band, but the 5-gram shingles
        // disagree, so stage 2 refuses it.
        let other = vec![entry("dark", "prefers the dark theme")];
        let cand = "prefers the light theme";
        assert!(
            find_duplicate(cand, &tokenize(cand), &other, 0.99).is_none(),
            "dark vs light must not merge"
        );
    }

    #[test]
    fn a_store_pass_merges_across_tiers_but_never_across_places() {
        let a = tiered("a", "the deploy pipeline uses fly", Tier::User, None);
        let b = tiered(
            "b",
            "the deploy pipeline uses fly",
            Tier::Place,
            Some("c:/work/proja"),
        );
        let c = tiered(
            "c",
            "the deploy pipeline uses fly",
            Tier::Place,
            Some("c:/work/projb"),
        );
        let pairs = plan_pass(&[a, b, c], 0.78);
        let mut dups: Vec<&str> = pairs.iter().map(|p| p.dup.as_str()).collect();
        dups.sort_unstable();
        assert_eq!(dups, vec!["b", "c"], "{pairs:?}");
        assert!(
            pairs
                .iter()
                .all(|p| p.keep == "a" && p.stage == Stage::Lexical),
            "one survivor — the smallest id on a full tie: {pairs:?}"
        );
        // The two places alone: the same sentence at two anchors is two truths.
        let b = tiered(
            "b",
            "the deploy pipeline uses fly",
            Tier::Place,
            Some("c:/work/proja"),
        );
        let c = tiered(
            "c",
            "the deploy pipeline uses fly",
            Tier::Place,
            Some("c:/work/projb"),
        );
        assert!(plan_pass(&[b, c], 0.78).is_empty());
    }

    #[test]
    fn a_cluster_keeps_exactly_one_survivor_the_most_seen_row() {
        let mk = |id: &str, sessions: u32| MemoryEntry {
            sessions,
            ..entry(id, "only rust-analyzer is installed on this machine")
        };
        let pairs = plan_pass(&[mk("x", 1), mk("y", 3), mk("z", 1)], 0.78);
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        assert!(pairs.iter().all(|p| p.keep == "y"), "{pairs:?}");
        let dups: HashSet<&str> = pairs.iter().map(|p| p.dup.as_str()).collect();
        assert_eq!(dups, HashSet::from(["x", "z"]));
    }

    #[test]
    fn near_duplicate_reinforces() {
        let existing = vec![entry("prefers-pnpm", "prefers pnpm over npm")];
        let op = decide(&tokenize("prefers pnpm over npm"), &existing, 0.82);
        assert_eq!(
            op,
            MemOp::Reinforce {
                id: "prefers-pnpm".into()
            }
        );
    }

    #[test]
    fn novel_fact_adds() {
        let existing = vec![entry("prefers-pnpm", "prefers pnpm over npm")];
        let op = decide(&tokenize("deploys on fridays only"), &existing, 0.78);
        assert_eq!(op, MemOp::Add);
    }

    #[test]
    fn reworded_restatement_reinforces() {
        // the default 0.78 threshold absorbs a reworded restatement of the same fact…
        let existing = vec![entry(
            "prefers-pnpm",
            "prefers pnpm over npm for everything",
        )];
        let op = decide(&tokenize("prefers pnpm over npm"), &existing, 0.78);
        assert_eq!(
            op,
            MemOp::Reinforce {
                id: "prefers-pnpm".into()
            }
        );
    }

    #[test]
    fn different_topic_does_not_false_merge() {
        // …but distinct facts sharing a token or two stay separate.
        let existing = vec![entry("dark-theme", "prefers the dark theme")];
        let op = decide(&tokenize("prefers the light theme"), &existing, 0.78);
        assert_eq!(op, MemOp::Add);
    }

    #[test]
    fn empty_store_adds() {
        assert_eq!(decide(&tokenize("anything"), &[], 0.82), MemOp::Add);
    }
}
