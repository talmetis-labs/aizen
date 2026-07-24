//! P5 — the Hebbian co-retrieval graph: *"neurons that fire together wire together."*
//!
//! The store already has NODES (a fact + its stable id) and two evolution signals living on each
//! node — reinforcement (`reinforced`/`lastRetrieved`) and bi-temporal supersession
//! (`validTo`/`supersededBy`). The one thing that is NOT derivable from a fact's content is which
//! facts get *recalled together* over time — the associative EDGE. That, and only that, is what
//! this module persists.
//!
//! Design, in the same spirit as the rest of the memory brain:
//! - **Always-on, best-effort.** Every `memory_search` that returns ≥2 facts is a co-fire event; we
//!   bump each pair's edge. A read-only store just means "no signal this turn", never a failed
//!   search — exactly the posture of [`crate::memory::store::record_retrieval`].
//! - **Per-day dedup per pair.** A chatty session firing many searches can't inflate an edge; a
//!   pair is bumped at most once/day (mirrors `record_retrieval`'s per-day dedup on nodes).
//! - **Decay on READ, never delete.** An edge's *effective* weight is its stored weight scaled by
//!   the recency of its last co-fire (same `recency_factor` the rank decay uses). A link that
//!   stops firing fades from neighbor lists but the edge stays until the cap evicts it.
//! - **Bounded for years.** A single plain-text TSV (`graph.tsv`); over `MAX_EDGES` the weakest
//!   *effective*-weight edges are dropped on write (the `caps`-style LRU, applied to edges).
//! - **Neighbor expansion is bench-gated, default-OFF.** The recording spine ships on; using the
//!   graph to *expand* a retrieval (pull in a strong neighbor a query missed) is opt-in via
//!   `NG_GRAPH_EXPAND`, the same "reachable + tested, not on-by-default until a bench proves it"
//!   discipline as the dense/fuzzy tiers.
//!
//! File format — one edge per line, tab-separated, endpoints in canonical (sorted) order so a
//! pair has exactly one row regardless of which fact was seen first:
//! ```text
//! <id_a>\t<id_b>\t<weight>\t<YYYY-MM-DD last co-fire>
//! ```
//! Ids come from `store::slugify` (lowercase alnum + `-`), so they never contain a tab or newline.

use crate::core::config;
use crate::memory::bloat::decay::age_days;
use crate::memory::score::recency_factor;
use crate::memory::store::write_atomic;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

/// Hard cap on stored edges. Over this, the weakest *effective*-weight edges are evicted on write
/// — the long tail of associations stays bounded for years without ever touching the fact store.
const MAX_EDGES: usize = 4000;

/// Half-life (days) for edge-weight decay on read. Deliberately longer than the fact recency
/// half-life (30d): an association is a slower-moving signal than a single fact's freshness.
const EDGE_HALF_LIFE_DAYS: f64 = 60.0;

/// One co-retrieval association. Endpoints are stored in canonical sorted order (`a < b`).
#[derive(Debug, Clone)]
struct Edge {
    a: String,
    b: String,
    weight: f64,
    /// `YYYY-MM-DD` of the last co-fire (drives decay + per-day dedup).
    last: String,
}

impl Edge {
    /// Effective weight today: stored weight scaled by how recently the pair last co-fired.
    fn effective(&self, today: &str) -> f64 {
        match age_days(&self.last, today) {
            Some(age) => self.weight * recency_factor(age, EDGE_HALF_LIFE_DAYS),
            None => self.weight, // unparseable date → don't penalize
        }
    }
}

/// Canonical key for an unordered pair.
fn key(x: &str, y: &str) -> (String, String) {
    if x <= y {
        (x.to_string(), y.to_string())
    } else {
        (y.to_string(), x.to_string())
    }
}

/// Last parsed graph, keyed by `(path, mtime-nanos, len)`. Search reinforcement and optional
/// neighbor expansion can touch the same ≤4000-edge TSV repeatedly in one process; a stat+Vec clone
/// is substantially cheaper than read+split+parse+canonicalize every time (perf T3). The write path
/// refreshes the cache immediately, while external/process writes are observed through the key.
type GraphCache = Mutex<Option<(PathBuf, u64, u64, Vec<Edge>)>>;
static GRAPH_CACHE: Lazy<GraphCache> = Lazy::new(|| Mutex::new(None));

fn graph_stamp(path: &std::path::Path) -> (u64, u64) {
    std::fs::metadata(path)
        .ok()
        .map(|m| {
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (mt, m.len())
        })
        .unwrap_or((0, 0))
}

/// Parse the TSV edge file. Missing / unreadable → empty (never errors — the graph is best-effort).
fn load() -> Vec<Edge> {
    let path = config::graph_path();
    let (mtime, len) = graph_stamp(&path);
    if let Ok(guard) = GRAPH_CACHE.lock() {
        if let Some((p, mt, l, edges)) = guard.as_ref() {
            if *p == path && *mt == mtime && *l == len {
                return edges.clone();
            }
        }
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split('\t');
        let (Some(a), Some(b), Some(w)) = (it.next(), it.next(), it.next()) else {
            continue; // malformed row → skip, don't abort the whole graph
        };
        let last = it.next().unwrap_or("").trim().to_string();
        let weight: f64 = match w.trim().parse() {
            Ok(v) if v > 0.0 => v,
            _ => continue,
        };
        let (a, b) = (a.trim(), b.trim());
        if a.is_empty() || b.is_empty() || a == b {
            continue; // no self-loops, no empty endpoints
        }
        let (a, b) = key(a, b);
        out.push(Edge { a, b, weight, last });
    }
    if let Ok(mut guard) = GRAPH_CACHE.lock() {
        let (after_mtime, after_len) = graph_stamp(&path);
        // Do not cache a parse if the file changed while it was being read. The caller still gets
        // this best-effort snapshot, but the next call must re-read instead of pinning stale edges.
        if (mtime, len) == (after_mtime, after_len) {
            *guard = Some((path, after_mtime, after_len, out.clone()));
        }
    }
    out
}

/// Serialize + atomically write the edge set, enforcing the `MAX_EDGES` cap (weakest *effective*
/// weight evicted first). Best-effort: an I/O failure is swallowed by callers.
fn save(mut edges: Vec<Edge>, today: &str) -> anyhow::Result<()> {
    if edges.len() > MAX_EDGES {
        // LRU-by-effective-weight: keep the strongest, drop the faded long tail (like `caps`).
        edges.sort_by(|x, y| {
            y.effective(today)
                .partial_cmp(&x.effective(today))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        edges.truncate(MAX_EDGES);
    }
    // Stable, diff-friendly on-disk order: by endpoints.
    edges.sort_by(|x, y| x.a.cmp(&y.a).then(x.b.cmp(&y.b)));
    let mut s = String::with_capacity(edges.len() * 32);
    for e in &edges {
        s.push_str(&format!("{}\t{}\t{:.4}\t{}\n", e.a, e.b, e.weight, e.last));
    }
    let path = config::graph_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, &s)?;
    let (mtime, len) = graph_stamp(&path);
    if let Ok(mut guard) = GRAPH_CACHE.lock() {
        *guard = Some((path, mtime, len, edges));
    }
    Ok(())
}

/// Record one co-retrieval event: the facts in `ids` were recalled together today, so strengthen
/// every pairwise association among them (Hebbian). **Per-day deduped per pair** — a pair already
/// bumped today is left unchanged, so a session firing many searches can't inflate a link. Fewer
/// than two distinct ids → nothing to associate (no-op). Best-effort: any I/O error is returned but
/// callers ignore it (a read-only store simply grows no graph this turn).
pub fn record_coretrieval(ids: &[&str], today: &str) -> anyhow::Result<bool> {
    // Distinct, non-empty ids — a fact never associates with itself, and a repeated hit in one
    // batch is still one node.
    let mut uniq: Vec<&str> = Vec::new();
    for id in ids {
        let id = id.trim();
        if !id.is_empty() && !uniq.contains(&id) {
            uniq.push(id);
        }
    }
    if uniq.len() < 2 {
        return Ok(false);
    }

    let mut edges = load();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        index.insert((e.a.clone(), e.b.clone()), i);
    }

    let mut changed = false;
    for i in 0..uniq.len() {
        for j in (i + 1)..uniq.len() {
            let k = key(uniq[i], uniq[j]);
            match index.get(&k) {
                Some(&idx) => {
                    // Per-day dedup: this pair already co-fired today → leave it.
                    if edges[idx].last.trim() == today {
                        continue;
                    }
                    edges[idx].weight += 1.0;
                    edges[idx].last = today.to_string();
                    changed = true;
                }
                None => {
                    let e = Edge { a: k.0.clone(), b: k.1.clone(), weight: 1.0, last: today.to_string() };
                    index.insert(k, edges.len());
                    edges.push(e);
                    changed = true;
                }
            }
        }
    }

    if !changed {
        return Ok(false); // every pair already counted today
    }
    save(edges, today)?;
    Ok(true)
}

/// The strongest current associations of `id`: neighbor ids ranked by *effective* (decayed) edge
/// weight, best first, capped at `k`. Edges whose decayed weight has faded below `floor` are
/// excluded so a long-dead link never resurfaces. Empty when the fact has no live associations.
pub fn neighbors(id: &str, today: &str, k: usize, floor: f64) -> Vec<(String, f64)> {
    let id = id.trim();
    if id.is_empty() || k == 0 {
        return Vec::new();
    }
    let edges = load();
    let mut out: Vec<(String, f64)> = edges
        .iter()
        .filter_map(|e| {
            let other = if e.a == id {
                Some(e.b.as_str())
            } else if e.b == id {
                Some(e.a.as_str())
            } else {
                None
            }?;
            let w = e.effective(today);
            if w >= floor {
                Some((other.to_string(), w))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|x, y| {
        y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal).then(x.0.cmp(&y.0))
    });
    out.truncate(k);
    out
}

/// Drop every edge with an endpoint not in `live` (a fact was hard-deleted / never existed). Called
/// from the maintenance pass so the graph can't accumulate dangling links to gone facts. Returns
/// how many edges were pruned. No-op (and no write) when nothing dangles.
pub fn prune(live: &HashSet<String>, today: &str) -> anyhow::Result<usize> {
    let edges = load();
    let before = edges.len();
    let kept: Vec<Edge> =
        edges.into_iter().filter(|e| live.contains(&e.a) && live.contains(&e.b)).collect();
    let pruned = before - kept.len();
    if pruned > 0 {
        save(kept, today)?;
    }
    Ok(pruned)
}

/// Whether the co-fire RECORDING spine runs (writes to `graph.tsv`). On by default; the
/// `NG_NO_GRAPH` kill-switch turns it (and expansion) off, collapsing retrieval to the pre-P5 path.
pub fn recording_enabled() -> bool {
    !config::graph_disabled()
}

/// Whether graph-based neighbor EXPANSION is enabled in production retrieval. Default OFF — the
/// recording spine always runs, but *using* the graph to widen a search is opt-in via
/// `NG_GRAPH_EXPAND` (same "ship the machinery, bench before defaulting" posture as the dense
/// tier). When off, the graph is still built (and queryable via `ng memory neighbors`), it just
/// doesn't alter live `memory_search` results. Delegates to [`config::graph_expand_enabled`] so the
/// flag logic (incl. the `NG_NO_GRAPH` master off) lives in exactly one place.
pub fn expansion_enabled() -> bool {
    config::graph_expand_enabled()
}

// ── Cross-pillar co-fire (Phase 4) ───────────────────────────────────────────────
//
// The intra-memory spine above wires `fact↔fact` from a single `memory_search` that surfaced ≥2
// facts. Cross-pillar co-fire extends the SAME graph to the other two brains — memory, skill, and
// self — by NAMESPACING the node ids: `mem:<fact-id>`, `skill:<name>`, `self:<insight-id>`. When a
// turn USES nodes from ≥2 different pillars (recalls a fact AND loads a skill, say), it fires one
// co-retrieval event over the union, so `neighbors("skill:deploy", …)` can surface the facts that
// deploy is habitually recalled alongside — spreading activation across the three stores.
//
// Two deliberate design choices keep the signal clean and regression-free:
//   * **Usage-driven, not inject-driven.** A node enters the turn set only when it is actually used
//     (a fact returned by `memory_search`, a skill pulled by `skill_load`), not merely present in
//     the always-on prompt. Firing every injected fact against every injected skill each turn would
//     wire everything to everything — meaningless. Only genuine co-USE is Hebbian.
//   * **Namespaced ids never collide with bare ids.** `slugify` output is lowercase-alnum-plus-dash
//     and contains no `:`, so a `mem:`/`skill:`/`self:` node is disjoint from the bare fact ids the
//     intra-memory spine (and `expand_with_graph`) use. The cross-pillar layer shares `graph.tsv`,
//     `Edge`, decay, and the cap, but its edges live in a separate id-space — the pre-Phase-4
//     retrieval path is byte-for-byte unaffected.

/// Namespace a fact id for the cross-pillar graph: `mem:<id>`.
pub fn mem_node(id: &str) -> String {
    format!("mem:{}", id.trim())
}

/// Namespace a skill name for the cross-pillar graph: `skill:<name>` (name lowercased/slug-safe via
/// the skills' own `sanitize_name` so the node matches regardless of how the user typed it).
pub fn skill_node(name: &str) -> String {
    format!("skill:{}", crate::skills::sanitize_name(name))
}

/// Namespace a self-memory (persona/working-relationship insight) id: `self:<id>`.
#[allow(dead_code)] // wired once self-insight reuse is tracked; namespace reserved now for stability
pub fn self_node(id: &str) -> String {
    format!("self:{}", id.trim())
}

/// Process-global set of namespaced nodes USED in the current turn, accumulated across the
/// `spawn_blocking` worker threads the tool bodies run on and drained by the post-turn hook on the
/// driver thread. A thread-local can't bridge that hop (the buffer is written on a worker, read on
/// the driver), and turns are strictly serial per process — the same invariant
/// [`crate::core::convo::active`] already relies on — so one shared set is correct and race-free in
/// practice. Best-effort throughout: a poisoned lock is recovered, never propagated.
static TURN_NODES: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Note that `node` (already namespaced — use [`mem_node`]/[`skill_node`]/[`self_node`]) was used
/// this turn. Deduped on flush, so calling repeatedly for the same node is fine. No-op under the
/// cross-pillar kill-switch, so a disabled build spends nothing.
pub fn note_used(node: &str) {
    if config::graph_xpillar_disabled() {
        return;
    }
    let node = node.trim();
    if node.is_empty() {
        return;
    }
    if let Ok(mut g) = TURN_NODES.lock() {
        g.push(node.to_string());
    }
}

/// Drain the turn's used-node set (clears it for the next turn). Returns the distinct nodes.
fn take_turn_nodes() -> Vec<String> {
    let mut out = match TURN_NODES.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return Vec::new(),
    };
    out.sort();
    out.dedup();
    out
}

/// How many DISTINCT pillars a namespaced node belongs to is what makes a turn cross-pillar. The
/// prefix before the first `:` is the pillar tag (`mem`/`skill`/`self`); a bare id (no `:`) counts
/// as its own "" pillar but never triggers cross-pillar recording on its own.
fn pillar_of(node: &str) -> &str {
    node.split_once(':').map(|(p, _)| p).unwrap_or("")
}

/// Flush the current turn's used nodes into one cross-pillar co-fire event, then clear the buffer.
/// Fires **only when the turn touched ≥2 distinct pillars** — a memory-only or skill-only turn adds
/// nothing here (the intra-memory `fact↔fact` spine already covers within-memory co-fire, and a
/// lone skill has nothing to associate). Best-effort: any graph write error is swallowed. Returns
/// whether an edge was recorded. No-op (and buffer left for the next flush attempt only if it was
/// never populated) under the kill-switch.
pub fn flush_turn_cofire(today: &str) -> bool {
    if config::graph_xpillar_disabled() {
        // Keep the buffer clear so a later-enabled turn doesn't inherit stale nodes.
        let _ = take_turn_nodes();
        return false;
    }
    let nodes = take_turn_nodes();
    if nodes.len() < 2 {
        return false;
    }
    // Need genuine CROSS-pillar activity: at least two different pillar tags among the used nodes.
    let mut pillars: HashSet<&str> = HashSet::new();
    for n in &nodes {
        pillars.insert(pillar_of(n));
    }
    if pillars.len() < 2 {
        return false; // all from one brain → intra-pillar, not this layer's job
    }
    let refs: Vec<&str> = nodes.iter().map(String::as_str).collect();
    record_coretrieval(&refs, today).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config;

    fn with_temp_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-graph-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn co_fire_creates_a_symmetric_edge() {
        with_temp_home("cofire", || {
            assert!(record_coretrieval(&["fact-a", "fact-b"], "2026-07-01").unwrap());
            // symmetric: a→b and b→a both see the neighbor
            let na = neighbors("fact-a", "2026-07-01", 5, 0.0);
            assert_eq!(na.len(), 1);
            assert_eq!(na[0].0, "fact-b");
            let nb = neighbors("fact-b", "2026-07-01", 5, 0.0);
            assert_eq!(nb[0].0, "fact-a");
        });
    }

    #[test]
    fn repeated_co_fire_same_day_does_not_inflate() {
        with_temp_home("dedup", || {
            assert!(record_coretrieval(&["a", "b"], "2026-07-01").unwrap());
            // same day again → no-op (per-day dedup per pair)
            assert!(!record_coretrieval(&["a", "b"], "2026-07-01").unwrap());
            let w1 = neighbors("a", "2026-07-01", 5, 0.0)[0].1;
            // a new day → strengthens
            assert!(record_coretrieval(&["a", "b"], "2026-07-05").unwrap());
            let w2 = neighbors("a", "2026-07-05", 5, 0.0)[0].1;
            assert!(w2 > w1, "a second-day co-fire must strengthen the link (w1={w1} w2={w2})");
        });
    }

    #[test]
    fn all_pairs_in_a_batch_are_linked() {
        with_temp_home("triangle", || {
            // a 3-fact recall wires the full triangle: a-b, a-c, b-c
            record_coretrieval(&["a", "b", "c"], "2026-07-01").unwrap();
            assert_eq!(neighbors("a", "2026-07-01", 5, 0.0).len(), 2);
            assert_eq!(neighbors("b", "2026-07-01", 5, 0.0).len(), 2);
            assert_eq!(neighbors("c", "2026-07-01", 5, 0.0).len(), 2);
        });
    }

    #[test]
    fn a_single_id_or_dup_batch_is_a_noop() {
        with_temp_home("single", || {
            assert!(!record_coretrieval(&["only"], "2026-07-01").unwrap());
            assert!(!record_coretrieval(&["x", "x", "x"], "2026-07-01").unwrap(), "dups collapse to one node");
            assert!(neighbors("only", "2026-07-01", 5, 0.0).is_empty());
        });
    }

    #[test]
    fn weight_decays_over_time_and_floor_hides_faded_links() {
        with_temp_home("decay", || {
            record_coretrieval(&["a", "b"], "2026-01-01").unwrap();
            let fresh = neighbors("a", "2026-01-01", 5, 0.0)[0].1;
            // ~6 months later the same edge is much weaker (60d half-life)
            let faded = neighbors("a", "2026-07-01", 5, 0.0);
            assert!(faded[0].1 < fresh, "an old edge must decay (fresh={fresh} faded={})", faded[0].1);
            // a floor above the faded weight hides it entirely
            assert!(neighbors("a", "2026-07-01", 5, faded[0].1 + 0.01).is_empty());
        });
    }

    #[test]
    fn prune_drops_edges_to_dead_nodes() {
        with_temp_home("prune", || {
            record_coretrieval(&["a", "b"], "2026-07-01").unwrap();
            record_coretrieval(&["a", "gone"], "2026-07-01").unwrap();
            let mut live = HashSet::new();
            live.insert("a".to_string());
            live.insert("b".to_string());
            // "gone" is no longer a fact → its edge is pruned, a-b survives
            assert_eq!(prune(&live, "2026-07-01").unwrap(), 1);
            let n = neighbors("a", "2026-07-01", 5, 0.0);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].0, "b");
            // idempotent: nothing left to prune
            assert_eq!(prune(&live, "2026-07-01").unwrap(), 0);
        });
    }

    #[test]
    fn edges_survive_a_round_trip_through_disk() {
        with_temp_home("roundtrip", || {
            record_coretrieval(&["alpha-1", "beta-2"], "2026-07-01").unwrap();
            record_coretrieval(&["alpha-1", "beta-2"], "2026-07-02").unwrap(); // weight 2.0
            // fresh load from disk sees the accumulated weight
            let n = neighbors("alpha-1", "2026-07-02", 5, 0.0);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].0, "beta-2");
            assert!(n[0].1 > 1.5, "two co-fires accumulate (got {})", n[0].1);
        });
    }

    #[test]
    fn cap_evicts_weakest_effective_edges() {
        with_temp_home("cap", || {
            // Force the cap tiny by writing MAX_EDGES+ synthetic edges is heavy; instead prove the
            // save() truncation policy directly on a small over-cap set.
            let today = "2026-07-01";
            let mut edges = Vec::new();
            for i in 0..(MAX_EDGES + 10) {
                edges.push(Edge {
                    a: format!("n{i}"),
                    b: "hub".to_string(),
                    // strictly increasing weight so the first 10 are the weakest
                    weight: (i + 1) as f64,
                    last: today.to_string(),
                });
            }
            save(edges, today).unwrap();
            let reloaded = load();
            assert_eq!(reloaded.len(), MAX_EDGES, "cap enforced on write");
            // `load()` canonicalizes each pair (endpoints sorted), so assert via the endpoint-
            // agnostic neighbor view rather than the raw `a` field: every surviving edge is
            // `hub`↔`n{i}`, so `hub`'s neighbor set is exactly the kept `n{i}` ids.
            let kept: HashSet<String> =
                neighbors("hub", today, MAX_EDGES, 0.0).into_iter().map(|(id, _)| id).collect();
            assert_eq!(kept.len(), MAX_EDGES, "hub keeps one edge to each surviving node");
            // the weakest (n0..n9, weight 1..10) were evicted; the strongest survive
            assert!(!kept.contains("n0") && !kept.contains("n9"), "weakest edges evicted");
            assert!(kept.contains(&format!("n{}", MAX_EDGES + 9)), "strongest edge kept");
        });
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        with_temp_home("malformed", || {
            let path = config::graph_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                "a\tb\t2.0\t2026-07-01\n\
                 garbage line no tabs\n\
                 c\td\tnotanumber\t2026-07-01\n\
                 self\tself\t1.0\t2026-07-01\n\
                 e\tf\t3.0\t2026-07-01\n",
            )
            .unwrap();
            // two valid edges survive; the malformed / self-loop / bad-weight rows are skipped
            let n = neighbors("a", "2026-07-01", 5, 0.0);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].0, "b");
            assert!(neighbors("self", "2026-07-01", 5, 0.0).is_empty(), "self-loop dropped");
        });
    }

    #[test]
    fn expansion_is_off_by_default() {
        // A clean env must NOT enable expansion (default-OFF, bench-gated like dense/fuzzy).
        std::env::remove_var("NG_GRAPH_EXPAND");
        assert!(!expansion_enabled());
    }

    // ── Cross-pillar co-fire (Phase 4) ──────────────────────────────────────
    // These touch the process-global TURN_NODES buffer, so they run under the same TEST_HOME_LOCK
    // `with_temp_home` holds (serialized). Each drains the buffer first so a prior test's leftover
    // nodes can't bleed in, and clears both kill-switches (default-ON is the tested behavior).

    fn xpillar_reset() {
        std::env::remove_var("NG_NO_GRAPH");
        std::env::remove_var("NG_NO_GRAPH_XPILLAR");
        let _ = take_turn_nodes(); // start from an empty buffer
    }

    #[test]
    fn namespaced_nodes_carry_their_pillar_tag() {
        assert_eq!(mem_node("fact-1"), "mem:fact-1");
        assert_eq!(skill_node("Deploy Web"), "skill:deploy-web"); // sanitize_name lowercases/slugs
        assert_eq!(self_node("in-x"), "self:in-x");
        assert_eq!(pillar_of("mem:fact-1"), "mem");
        assert_eq!(pillar_of("skill:deploy"), "skill");
        assert_eq!(pillar_of("bare-id"), "", "a bare id has the empty pillar");
    }

    #[test]
    fn cross_pillar_use_wires_mem_to_skill() {
        with_temp_home("xpillar", || {
            xpillar_reset();
            // A turn that recalled a fact AND loaded a skill.
            note_used(&mem_node("prefers-pnpm"));
            note_used(&skill_node("deploy"));
            assert!(flush_turn_cofire("2026-07-01"), "≥2 pillars → one cross-pillar edge");
            // The association is queryable in both directions across the namespace boundary.
            let from_skill = neighbors(&skill_node("deploy"), "2026-07-01", 5, 0.0);
            assert_eq!(from_skill.len(), 1);
            assert_eq!(from_skill[0].0, mem_node("prefers-pnpm"));
            let from_mem = neighbors(&mem_node("prefers-pnpm"), "2026-07-01", 5, 0.0);
            assert_eq!(from_mem[0].0, skill_node("deploy"));
        });
    }

    #[test]
    fn single_pillar_turn_records_no_cross_edge() {
        with_temp_home("xsingle", || {
            xpillar_reset();
            // Two facts, no skill/self → all one pillar. The intra-memory spine already covers this;
            // the cross-pillar layer must add nothing.
            note_used(&mem_node("a"));
            note_used(&mem_node("b"));
            assert!(!flush_turn_cofire("2026-07-01"), "one pillar → no cross-pillar edge");
            assert!(neighbors(&mem_node("a"), "2026-07-01", 5, 0.0).is_empty());
        });
    }

    #[test]
    fn flush_clears_the_buffer_between_turns() {
        with_temp_home("xclear", || {
            xpillar_reset();
            note_used(&mem_node("x"));
            note_used(&skill_node("s"));
            assert!(flush_turn_cofire("2026-07-01"));
            // A fresh turn with a lone node must not re-fire the previous turn's pair.
            note_used(&mem_node("y"));
            assert!(!flush_turn_cofire("2026-07-02"), "buffer drained → nothing carries over");
            // The old edge is unchanged (still exactly one co-fire), and `y` has no associations.
            assert!(neighbors(&mem_node("y"), "2026-07-02", 5, 0.0).is_empty());
        });
    }

    #[test]
    fn kill_switch_disables_cross_pillar_recording() {
        with_temp_home("xkill", || {
            xpillar_reset();
            std::env::set_var("NG_NO_GRAPH_XPILLAR", "1");
            note_used(&mem_node("a")); // no-op under the kill-switch
            note_used(&skill_node("s"));
            assert!(!flush_turn_cofire("2026-07-01"), "kill-switch → no recording");
            assert!(neighbors(&skill_node("s"), "2026-07-01", 5, 0.0).is_empty());
            std::env::remove_var("NG_NO_GRAPH_XPILLAR");
        });
    }

    #[test]
    fn master_graph_off_also_disables_cross_pillar() {
        with_temp_home("xmaster", || {
            xpillar_reset();
            std::env::set_var("NG_NO_GRAPH", "1");
            note_used(&mem_node("a"));
            note_used(&skill_node("s"));
            assert!(!flush_turn_cofire("2026-07-01"), "master NG_NO_GRAPH off → no cross-pillar");
            std::env::remove_var("NG_NO_GRAPH");
        });
    }
}
