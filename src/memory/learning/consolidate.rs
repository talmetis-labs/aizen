//! Mem0-style consolidation: before persisting a new fact, check it against the
//! existing store. A near-duplicate UPDATEs (reinforces) the existing fact instead of
//! inserting a twin — this is the primary anti-bloat lever on the write path (the
//! semantic-dup variant via cosine lands with the dense tier in P5).

use crate::memory::score::lexical_score_tokens;
use crate::memory::store::MemoryEntry;

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

/// Decide ADD vs REINFORCE for `candidate_tokens` against the current store.
/// Picks the single best lexical match; reinforces if it clears `dedup_threshold`.
pub fn decide(candidate_tokens: &[String], existing: &[MemoryEntry], dedup_threshold: f64) -> MemOp {
    let mut best: Option<(&str, f64)> = None;
    for e in existing {
        let s = lexical_score_tokens(candidate_tokens, &e.tokens);
        if best.map(|(_, bs)| s > bs).unwrap_or(true) {
            best = Some((e.id.as_str(), s));
        }
    }
    match best {
        Some((id, s)) if s >= dedup_threshold => MemOp::Reinforce { id: id.to_string() },
        _ => MemOp::Add,
    }
}

// ── Phase 4B: manual consolidation pass (cluster redundant aging facts → 1 distilled insight) ──
//
// This is the ONE place in the batch that bulk-rewrites the durable user store from LM judgment, so
// every knob here leans toward *keeping too much* over *folding the wrong things together*. The
// clustering + eligibility below are PURE (no LM, no I/O) — the LM distill + the actual supersede
// live in the driver (`memory::consolidate_pass`), gated behind an explicit `--apply` + confirm.

/// Lowest lexical similarity for two facts to land in the SAME consolidation cluster. Deliberately
/// far above the write-time dedup threshold and [`SUPERSEDE_SLOT_MIN`]: near-duplicates are already
/// folded on the write path, so 4B targets the *distinct-but-orbiting-one-topic* case, not token
/// coincidence. When in doubt, two facts stay separate.
pub const CONSOLIDATE_CLUSTER_MIN: f64 = 0.62;
/// Hard cap on clusters folded in a single `consolidate --apply` run — one command can never rewrite
/// the whole store. Want more? Run it again after reviewing.
pub const CONSOLIDATE_MAX_CLUSTERS: usize = 10;
/// Hard cap on members pulled into one cluster (bounds the distill prompt + the supersede blast).
pub const CONSOLIDATE_MAX_MEMBERS: usize = 6;
/// A fact touched (created / reinforced / retrieved) within this many days is too fresh to fold —
/// it may still be actively true. Only aging facts are candidates.
pub const CONSOLIDATE_FRESH_DAYS: f64 = 21.0;

/// Is `e` safe to EVER fold into a consolidation cluster? The inviolable list — a fact must clear
/// ALL of these or it is left completely untouched:
/// - **Inferred only.** Manual / user-explicit / imported facts are the user's deliberate words.
/// - **Not `core_denied`.** An explicit "keep this out of core" is a signal about this fact — honor it.
/// - **Still active.** An already-superseded fact has nothing to consolidate.
/// - **Not core-eligible.** `sessions >= 2` means it earned its way toward the always-on core — leave it.
/// - **Not fresh.** Touched within [`CONSOLIDATE_FRESH_DAYS`] → too soon to fold (may still be live).
pub fn eligible_for_consolidation(e: &MemoryEntry, today: &str, fresh_days: f64) -> bool {
    use crate::memory::provenance::ProvenanceKind;
    if e.source != ProvenanceKind::Inferred {
        return false;
    }
    if e.core_denied {
        return false;
    }
    if !e.is_active() {
        return false;
    }
    if e.sessions >= 2 {
        return false;
    }
    // Age = the MOST RECENT touch across created / updated / last-retrieved. If that is younger than
    // the freshness window, the fact is still warm → skip. Unparseable/absent dates → treated as old
    // (a legacy fact with no stamps is not "fresh").
    let newest = [e.updated.as_deref(), e.created.as_deref(), e.last_retrieved.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|d| crate::memory::bloat::decay::age_days(d, today))
        .fold(f64::INFINITY, f64::min);
    if newest.is_finite() && newest < fresh_days {
        return false;
    }
    true
}

/// Group eligible facts into consolidation clusters — PURE, deterministic, no LM. Two hard gates:
/// facts are first bucketed by `(scope, mtype, category)` (never fold across a zone, a storage type,
/// or a content category), then within each bucket a greedy single-link pass pulls together facts
/// whose lexical similarity to the seed clears `threshold`. Only clusters of **≥ 2** members are
/// returned; each is capped at `max_members`. Returns member-id lists in a stable order.
pub fn cluster(eligible: &[&MemoryEntry], threshold: f64, max_members: usize) -> Vec<Vec<String>> {
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<(Option<String>, &str, &str), Vec<usize>> = BTreeMap::new();
    for (i, e) in eligible.iter().enumerate() {
        buckets.entry((e.scope.clone(), e.mtype.as_str(), e.category.as_str())).or_default().push(i);
    }
    let mut out = Vec::new();
    for (_key, idxs) in buckets {
        let mut used = vec![false; idxs.len()];
        for a in 0..idxs.len() {
            if used[a] {
                continue;
            }
            let seed = eligible[idxs[a]];
            let mut members = vec![seed.id.clone()];
            used[a] = true;
            for b in (a + 1)..idxs.len() {
                if used[b] || members.len() >= max_members {
                    continue;
                }
                let other = eligible[idxs[b]];
                if lexical_score_tokens(&seed.tokens, &other.tokens) >= threshold {
                    members.push(other.id.clone());
                    used[b] = true;
                }
            }
            if members.len() >= 2 {
                out.push(members);
            }
        }
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

    #[test]
    fn near_duplicate_reinforces() {
        let existing = vec![entry("prefers-pnpm", "prefers pnpm over npm")];
        let op = decide(&tokenize("prefers pnpm over npm"), &existing, 0.82);
        assert_eq!(op, MemOp::Reinforce { id: "prefers-pnpm".into() });
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
        let existing = vec![entry("prefers-pnpm", "prefers pnpm over npm for everything")];
        let op = decide(&tokenize("prefers pnpm over npm"), &existing, 0.78);
        assert_eq!(op, MemOp::Reinforce { id: "prefers-pnpm".into() });
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

    // ── Phase 4B: consolidation eligibility + clustering (pure) ──
    use crate::memory::category::Category;
    use crate::memory::provenance::ProvenanceKind;

    /// An old, inferred, low-session fact — the baseline *eligible* candidate. Callers mutate one
    /// field at a time to prove each gate. `today` in these tests is fixed at 2026-07-24.
    fn cand(id: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            mtype: MemoryType::Project,
            body: body.into(),
            tokens: tokenize(body),
            source: ProvenanceKind::Inferred,
            sessions: 1,
            created: Some("2026-01-01".into()),
            updated: Some("2026-01-01".into()),
            category: Category::Codebase,
            ..Default::default()
        }
    }

    const TODAY: &str = "2026-07-24";

    #[test]
    fn old_inferred_low_session_fact_is_eligible() {
        assert!(eligible_for_consolidation(&cand("a", "the auth module lives in src/auth"), TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn manual_fact_is_never_eligible() {
        let mut e = cand("a", "x");
        e.source = ProvenanceKind::Manual;
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn user_explicit_fact_is_never_eligible() {
        let mut e = cand("a", "x");
        e.source = ProvenanceKind::UserExplicit;
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn core_eligible_fact_is_protected() {
        let mut e = cand("a", "x");
        e.sessions = 2; // earned its way toward always-on core
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn core_denied_fact_is_protected() {
        let mut e = cand("a", "x");
        e.core_denied = true;
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn superseded_fact_is_not_eligible() {
        let mut e = cand("a", "x");
        e.valid_to = Some("2026-02-01".into());
        e.superseded_by = Some("b".into());
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn fresh_fact_is_skipped() {
        let mut e = cand("a", "x");
        e.updated = Some("2026-07-20".into()); // 4 days ago < 21-day window
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn recent_retrieval_keeps_a_fact_warm() {
        let mut e = cand("a", "x");
        e.last_retrieved = Some("2026-07-22".into()); // retrieved 2 days ago → still live
        assert!(!eligible_for_consolidation(&e, TODAY, CONSOLIDATE_FRESH_DAYS));
    }

    #[test]
    fn cluster_folds_similar_same_bucket_facts() {
        let a = cand("a", "the auth login handler validates the session token on every request");
        let b = cand("b", "auth login handler validates session token for each request made");
        let refs = vec![&a, &b];
        let clusters = cluster(&refs, CONSOLIDATE_CLUSTER_MIN, CONSOLIDATE_MAX_MEMBERS);
        assert_eq!(clusters.len(), 1, "two restatements of one fact fold: {clusters:?}");
        assert_eq!(clusters[0].len(), 2);
    }

    #[test]
    fn cluster_never_folds_across_category() {
        let mut a = cand("a", "the deploy pipeline runs the fly deploy command on every merge to main");
        a.category = Category::Command;
        let mut b = cand("b", "the deploy pipeline runs the fly deploy command on every merge to main");
        b.category = Category::DeployNote;
        let refs = vec![&a, &b];
        // Identical text, but different content category → different bucket → never merged.
        assert!(cluster(&refs, CONSOLIDATE_CLUSTER_MIN, CONSOLIDATE_MAX_MEMBERS).is_empty());
    }

    #[test]
    fn cluster_never_folds_across_zone() {
        let mut a = cand("a", "the build uses the release profile with lto enabled for the final binary");
        a.scope = Some("proj-aaaa".into());
        let mut b = cand("b", "the build uses the release profile with lto enabled for the final binary");
        b.scope = Some("proj-bbbb".into());
        let refs = vec![&a, &b];
        assert!(cluster(&refs, CONSOLIDATE_CLUSTER_MIN, CONSOLIDATE_MAX_MEMBERS).is_empty());
    }

    #[test]
    fn cluster_leaves_distinct_topics_alone() {
        let a = cand("a", "the auth module lives under src/auth");
        let b = cand("b", "the payment webhook retries three times with backoff");
        let refs = vec![&a, &b];
        assert!(cluster(&refs, CONSOLIDATE_CLUSTER_MIN, CONSOLIDATE_MAX_MEMBERS).is_empty());
    }

    #[test]
    fn cluster_caps_member_count() {
        // Five near-identical facts, cap at 3 → the cluster holds at most 3 members.
        let bodies: Vec<MemoryEntry> = (0..5)
            .map(|i| cand(&format!("f{i}"), "the cache layer stores rendered pages keyed by url and locale"))
            .collect();
        let refs: Vec<&MemoryEntry> = bodies.iter().collect();
        let clusters = cluster(&refs, CONSOLIDATE_CLUSTER_MIN, 3);
        assert!(clusters.iter().all(|c| c.len() <= 3), "member cap respected: {clusters:?}");
    }
}
