"""Compare source-pinned in-memory retrieval slices, never live stores or model discovery.

Builds two tiny Rust fixture observers in a new temporary root. The original is read from a Git
object, not dormant working-tree files. The candidate is copied from the standalone sibling.
Algorithm spans are retained verbatim; only the in-memory record/settings seams are supplied.
This is algorithm-slice evidence, not the actual app/provider RPC or full engine/store parity.
Usage: python -I scripts/mnemos_retrieval_parity.py --cargo C:/.../cargo.exe --git C:/.../git.exe
       --msvc-root C:/.../MSVC/<version> --sdk-lib C:/.../WindowsKits/Lib/<version>
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import time
import traceback

MODULES = ("src/memory/mod.rs", "src/memory/tokenize.rs", "src/memory/score.rs",
           "src/memory/fuse.rs", "src/memory/embed.rs", "src/memory/provenance.rs",
           "src/memory/bloat/decay.rs", "src/memory/store.rs")
FIXTURES = ("memories.jsonl", "queries.gate.jsonl", "queries.tune.jsonl")
REPEATS = 5
TOLERANCE = 1e-12

MANIFEST = '''[package]
name = "mnemos-retrieval-observer"
version = "0.0.0"
edition = "2021"
publish = false
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
regex = "1"
once_cell = "1"
strsim = "0.11"
icu_normalizer = "2"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
[profile.dev]
debug = false
'''

# This adapter carries only fields read by the verbatim slices, not any storage implementation.
SEAM = '''use std::collections::HashSet;
use std::cmp::Ordering;
use score::Bm25Index;
use store::MemoryEntry;
use tokenize::tokenize;
pub mod tokenize;
pub mod score;
pub mod fuse;
pub mod embed;
pub mod provenance;
pub mod bloat { pub mod decay; }
pub mod store {
    use super::provenance::ProvenanceKind;
    #[derive(Clone)]
    pub struct MemoryEntry {
        pub id: String, pub body: String, pub tokens: Vec<String>, pub mtime_ms: u128,
        pub source: ProvenanceKind, pub created: Option<String>, pub updated: Option<String>,
        pub last_used: Option<String>, pub last_retrieved: Option<String>,
        pub confirmations: u32, pub valid_to: Option<String>, pub superseded_by: Option<String>,
    }
    impl MemoryEntry { __IS_ACTIVE__ }
}
pub struct Hit { pub entry: MemoryEntry, pub score: f64 }
struct Settings { rrf_k: f64 }
fn settings() -> Settings { Settings { rrf_k: 10.0 } }
'''

OBSERVER = r'''#![allow(dead_code)]
mod memory;
use memory::{embed::{Embedder,HashEmbedder},score::Bm25Index,store::MemoryEntry};
use serde_json::{json,Value};
use std::{cell::Cell,collections::HashSet};

fn entries() -> Vec<MemoryEntry> {
    include_str!("../fixtures/memories.jsonl").lines().filter(|s|!s.trim().is_empty()).map(|line| {
        let v: Value=serde_json::from_str(line).unwrap();
        let text=|key:&str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let name=text("name"); let description=text("description"); let body=text("body");
        MemoryEntry { id:text("id"),tokens:memory::tokenize::tokenize(&format!("{name}\n{description}\n{body}")),
            body,mtime_ms:0,source:memory::provenance::ProvenanceKind::Manual,
            created:None,updated:None,last_used:None,last_retrieved:None,confirmations:0,
            valid_to:None,superseded_by:None }
    }).collect()
}
struct Counted { inner:HashEmbedder, calls:Cell<usize> }
impl Embedder for Counted {
    fn id(&self)->String {self.inner.id()}
    fn dim(&self)->usize {self.inner.dim()}
    fn embed(&self,text:&str)->Vec<f32> {self.calls.set(self.calls.get()+1);self.inner.embed(text)}
}
fn main() {
    let corpus=entries();
    let ids:HashSet<_>=corpus.iter().map(|e|&e.id).collect();
    assert_eq!(ids.len(),corpus.len(),"duplicate corpus ID");
    let mut queries=Vec::<(String,String,String)>::new();
    for (split,raw) in [("gate",include_str!("../fixtures/queries.gate.jsonl")),
                        ("tune",include_str!("../fixtures/queries.tune.jsonl"))] {
        for line in raw.lines().filter(|s|!s.trim().is_empty()) {
            let v:Value=serde_json::from_str(line).unwrap();
            queries.push((split.into(),v["id"].as_str().unwrap().into(),v["query"].as_str().unwrap().into()));
        }
    }
    for (id,q) in [("empty",""),("stopwords","the và của"),("unknown","unmatchedquasar zyxwvut"),
                  ("literal","postgres"),("typo","postgress schema"),("diacritics","cà phê đen"),
                  ("decomposed","ca\u{300} phe\u{302} đen"),("identifier","get_by_id rate-limit HTTPServer"),
                  ("repeat","login login login auth")] {
        queries.push(("edge".into(),id.into(),q.into()));
    }
    let mut rows=Vec::new();
    for repeat in 0..5 {
        for (split,id,q) in &queries {
            for mode in ["exact","fuzzy","hybrid","gated-0","gated-0.6","gated-1","gated-1.01",
                         "exact-excluding","hybrid-excluding"] {
                let excluded:HashSet<String>=if mode.ends_with("excluding") {corpus.iter().take(2).map(|e|e.id.clone()).collect()} else {HashSet::new()};
                let embed=Counted{inner:HashEmbedder::new(256),calls:Cell::new(0)};
                let lexical=memory::search_excluding(q,usize::MAX,corpus.clone(),&excluded);
                let qtoks:HashSet<String>=memory::tokenize::tokenize(q).into_iter().collect();
                let coverage=lexical.first().map(|h| {
                    let terms:HashSet<_>=h.entry.tokens.iter().collect();
                    if qtoks.is_empty(){0.0}else{qtoks.iter().filter(|t|terms.contains(t)).count() as f64/qtoks.len() as f64}
                }).unwrap_or(0.0);
                let hits=match mode {
                    "exact"=>memory::search_in(q,10,corpus.clone()),
                    "fuzzy"=>memory::search_in_fuzzy(q,10,corpus.clone()),
                    "exact-excluding"=>memory::search_excluding(q,10,corpus.clone(),&excluded),
                    "hybrid"|"hybrid-excluding"=>memory::search_hybrid_in(q,10,corpus.clone(),&excluded,&embed),
                    _=>memory::search_hybrid_gated_in(q,10,corpus.clone(),&excluded,&embed,match mode {
                        "gated-0"=>0.0,"gated-0.6"=>0.6,"gated-1"=>1.0,_=>1.01}),
                };
                // Recall index deliberately uses full corpus, like the existing fixture bench;
                // lexical exclusion independently rebuilds its candidate-relative rank index.
                let idx=Bm25Index::build(corpus.iter().map(|e|e.tokens.as_slice()));
                let recall=memory::gate_coverage(&memory::tokenize::tokenize(q),&hits,&idx);
                rows.push(json!({"repeat":repeat,"split":split,"query_id":id,"mode":mode,
                    "query":q,"ordered_ids":hits.iter().map(|h|&h.entry.id).collect::<Vec<_>>(),
                    "scores":hits.iter().map(|h|h.score).collect::<Vec<_>>(),
                    "recall_coverage":recall,"recall_admitted":!hits.is_empty()&&recall>=memory::RECALL_GATE_COVERAGE,
                    "dense_top_coverage":coverage,"dense_calls":embed.calls.get()}));
            }
        }
    }
    let mut decay=Vec::new();
    for source in ["manual","user-explicit","imported","inferred"] {
        for confirmations in [0,1,2,3,9] {
            for case in ["missing","created","updated","used","retrieved","invalid","future"] {
                let mut e=corpus[0].clone();
                e.source=memory::provenance::ProvenanceKind::parse(source);e.confirmations=confirmations;
                match case {
                    "created"=>e.created=Some("2025-12-01".into()),
                    "updated"=>{e.created=Some("2025-01-01".into());e.updated=Some("2026-04-01".into());},
                    "used"=>{e.created=Some("2025-01-01".into());e.updated=Some("2026-05-30".into());e.last_used=Some("2026-05-01".into());},
                    "retrieved"=>{e.created=Some("2025-01-01".into());e.last_retrieved=Some("2026-05-31".into());},
                    "invalid"=>{e.created=Some("2025-01-01".into());e.last_used=Some("malformed".into());},
                    "future"=>e.created=Some("2026-12-01".into()),_=>{},
                }
                let today="2026-06-01";let half_life=30.0;let base=2.75;
                decay.push(json!({"source":source,"confirmations":confirmations,"case":case,
                    "decayed":memory::bloat::decay::decayed_score(base,&e,today,half_life),
                    "salience":memory::bloat::decay::salience_of(&e,today,half_life),
                    "strength":memory::bloat::decay::strength(&e,today,half_life),
                    "evolved":memory::bloat::decay::evolved_score(base,&e,today,half_life),
                    "archive":memory::bloat::decay::should_archive(&e,today,half_life)}));
            }
        }
    }
    println!("{}",json!({"corpus_entries":corpus.len(),"query_count":queries.len(),
        "k":10,"rrf_k":10.0,"today":"2026-06-01","half_life":30.0,
        "repeats":5,"rows":rows,"decay":decay}));
}
'''


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def span(text: str, start: str, end: str) -> str:
    if text.count(start) != 1 or text.count(end) != 1:
        raise ValueError("source span marker is missing or ambiguous")
    a, b = text.index(start), text.index(end)
    if a >= b:
        raise ValueError("source span marker order changed")
    return text[a:b]


def generate(crate: Path, sources: dict[str, bytes], fixtures: dict[str, bytes]) -> dict:
    memory = crate / "src/memory"
    (memory / "bloat").mkdir(parents=True)
    (crate / "fixtures").mkdir()
    (crate / "Cargo.toml").write_text(MANIFEST, encoding="utf-8")
    (crate / "src/main.rs").write_text(OBSERVER, encoding="utf-8")
    spans = {}
    for name in ("tokenize", "score", "fuse", "provenance", "bloat/decay"):
        path = f"src/memory/{name}.rs"
        (memory / f"{name}.rs").write_bytes(sources[path])
        spans[path] = {"kind": "whole module", "sha256": digest(sources[path])}
    text = sources["src/memory/mod.rs"].decode("utf-8")
    gate = span(text, "pub(crate) const RECALL_GATE_COVERAGE:", "/// The marker every recall block")
    search = span(text, "pub fn search_in(query:", "/// An `Embedder` decorator")
    store = sources["src/memory/store.rs"].decode("utf-8")
    active = span(store, "pub fn is_active(&self)", "    /// A short one-line gist")
    embed = span(sources["src/memory/embed.rs"].decode("utf-8"),
                 "/// Anything that turns text", "/// Content-hash")
    for name, data in (("gate", gate), ("search", search), ("is_active", active), ("embed", embed)):
        spans[name] = {"kind": "verbatim marker-delimited span", "sha256": digest(data.encode())}
    (memory / "mod.rs").write_text(SEAM.replace("__IS_ACTIVE__", active) + gate + search, encoding="utf-8")
    (memory / "embed.rs").write_text("use super::tokenize::tokenize;\n" + embed, encoding="utf-8")
    for name, data in fixtures.items():
        (crate / "fixtures" / name).write_bytes(data)
    return spans


def close(a, b):
    if isinstance(a, bool) or isinstance(b, bool):
        return isinstance(a, bool) and isinstance(b, bool) and a == b
    if isinstance(a, (float, int)) and isinstance(b, (float, int)):
        return math.isfinite(a) and math.isfinite(b) and math.isclose(a, b, rel_tol=TOLERANCE, abs_tol=TOLERANCE)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(close(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(close(a[k], b[k]) for k in a)
    return a == b


def comparisons(baseline: dict, candidate: dict) -> dict:
    failures = []
    order_failures = []
    for index, (a, b) in enumerate(zip(baseline["rows"], candidate["rows"])):
        if a["ordered_ids"] != b["ordered_ids"]:
            order_failures.append({"index": index, "split": a["split"], "query_id": a["query_id"],
                                   "mode": a["mode"], "repeat": a["repeat"], "baseline": a, "candidate": b})
        elif not close(a, b):
            failures.append({"index": index, "baseline": a, "candidate": b})
    instability = {}
    for label, snapshot in (("baseline", baseline), ("candidate", candidate)):
        first = {}
        changed = []
        for row in snapshot["rows"]:
            key = (row["split"], row["query_id"], row["mode"])
            normalized = {k: v for k, v in row.items() if k != "repeat"}
            if key in first and not close(first[key], normalized):
                changed.append({"key": key, "repeat": row["repeat"], "first": first[key], "later": normalized})
            first.setdefault(key, normalized)
        instability[label] = changed
    shapes_match = (
        baseline.keys() == candidate.keys()
        and all(baseline[k] == candidate[k] for k in baseline if k not in ("rows", "decay"))
        and bool(baseline["rows"])
        and len(baseline["rows"]) == len(candidate["rows"])
        and bool(baseline["decay"])
    )
    return {"shapes_match": shapes_match, "ordered_id_failures": order_failures,
            "value_failures": failures, "decay_match": close(baseline["decay"], candidate["decay"]),
            "within_implementation_instability": instability,
            "passed": shapes_match and not failures and not order_failures and close(baseline["decay"], candidate["decay"]) and not any(instability.values())}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", required=True, type=Path)
    parser.add_argument("--git", required=True, type=Path)
    parser.add_argument("--msvc-root", required=True, type=Path,
                        help="MSVC version directory containing bin/Hostx64/x64 and lib/x64")
    parser.add_argument("--sdk-lib", required=True, type=Path,
                        help="Windows SDK versioned Lib directory containing ucrt/x64 and um/x64")
    parser.add_argument("--revision", default="e3bd8c9")
    parser.add_argument("--mnemos", type=Path, default=Path(__file__).resolve().parents[2] / "aizen-mnemos")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    cargo, git, sibling = args.cargo.resolve(strict=True), args.git.resolve(strict=True), args.mnemos.resolve(strict=True)
    msvc_root, sdk_lib = args.msvc_root.resolve(strict=True), args.sdk_lib.resolve(strict=True)
    linker = msvc_root / "bin/Hostx64/x64/link.exe"
    libraries = (msvc_root / "lib/x64", sdk_lib / "ucrt/x64", sdk_lib / "um/x64")
    if not linker.is_file() or not all(path.is_dir() for path in libraries):
        raise ValueError("explicit MSVC linker or Windows SDK library directories are unavailable")
    revision = subprocess.check_output([str(git), "rev-parse", "--verify", args.revision + "^{commit}"], cwd=repo, timeout=30).decode().strip()
    def original(path):
        return subprocess.check_output([str(git), "show", f"{revision}:{path}"], cwd=repo, timeout=30)
    def current(path):
        target = sibling / path
        info = target.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("candidate source must be an ordinary nonlinked file")
        return target.read_bytes()
    old_sources = {path: original(path) for path in MODULES}
    new_sources = {path: current(path) for path in MODULES}
    lock = current("Cargo.lock")
    script = Path(__file__).read_bytes()
    fixtures = {name: original("bench-fixtures/" + name) for name in FIXTURES}
    if any(current("bench-fixtures/" + name) != data for name, data in fixtures.items()):
        raise ValueError("candidate fixtures differ from checkpoint")
    root = Path(tempfile.mkdtemp(prefix="mnemos-retrieval-parity-"))
    print(f"Retrieval parity fixture: {root}", flush=True)
    report = {"root": str(root), "passed": False, "revision": revision,
              "tolerance": TOLERANCE, "repeats": REPEATS, "sources": {}, "steps": [],
              "script_sha256": digest(script), "input_lock_sha256": digest(lock),
              "build_environment": {"linker": str(linker), "library_roots": list(map(str, libraries)),
                                    "toolchain": "stable-x86_64-pc-windows-msvc"},
              "fixture_sha256": {name: digest(data) for name, data in fixtures.items()},
              "limitations": ["Verbatim algorithm slices with minimal in-memory record seam and pinned rrf_k=10, not full app/RPC",
                              "No store, scope, frozen-core/session ledger, cache, model discovery or network coverage",
                              "Hash embedder tests fusion mechanics, not semantic quality",
                              "Strict ordered IDs retained; numeric tolerance does not normalize order failures",
                              "Five repeats can expose random iteration instability, not prove its absence"]}
    def save():
        (root / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    spec = importlib.util.spec_from_file_location("retrieval_owned_env", repo / "scripts/memory_agent_smoke.py")
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    env = support.isolated_environment(root)
    env.update({"CARGO_HOME": "C:/Users/admin/.cargo", "RUSTUP_HOME": "C:/Users/admin/.rustup",
                "RUSTUP_TOOLCHAIN": "stable-x86_64-pc-windows-msvc", "RUSTUP_AUTO_INSTALL": "0",
                "CARGO_NET_OFFLINE": "true",
                "PATH": os.pathsep.join((str(cargo.parent), str(linker.parent), env["PATH"])),
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": str(linker),
                "LIB": os.pathsep.join(map(str, libraries)),
                "CARGO_TARGET_DIR": str(root / "target")})
    try:
        snapshots = {}
        for label, sources in (("baseline", old_sources), ("candidate", new_sources)):
            crate = root / label
            spans = generate(crate, sources, fixtures)
            report["sources"][label] = {"whole_files": {path: digest(data) for path, data in sources.items()}, "spans": spans}
            # Reuse the standalone lock to pin the same registry package versions for both adapters.
            (crate / "Cargo.lock").write_bytes(lock)
            report["sources"][label]["generated_files"] = {
                path.relative_to(crate).as_posix(): digest(path.read_bytes())
                for path in sorted((crate / "src").rglob("*.rs"))
            }
            report["sources"][label]["manifest_sha256"] = digest((crate / "Cargo.toml").read_bytes())
            start = time.monotonic()
            with (root / f"{label}-build.log").open("xb") as output:
                result = subprocess.run([str(cargo), "build", "--offline", "--bin", "mnemos-retrieval-observer"],
                                        cwd=crate, env=env, stdout=output, stderr=subprocess.STDOUT, timeout=600, check=False)
            report["steps"].append({"name": label + "-build", "exit": result.returncode, "seconds": round(time.monotonic() - start, 2)})
            report["sources"][label]["resolved_lock_sha256"] = digest((crate / "Cargo.lock").read_bytes())
            save()
            if result.returncode:
                return 1
            binary = root / "target/debug/mnemos-retrieval-observer.exe"
            report["steps"][-1]["binary_sha256"] = digest(binary.read_bytes())
            # This observer has no child spawns or data IO: bounded run, fresh runtime roots.
            result = subprocess.run([str(binary)], cwd=root, env=env, capture_output=True, timeout=60, check=False)
            (root / f"{label}-snapshot.json").write_bytes(result.stdout)
            (root / f"{label}-runtime.stderr").write_bytes(result.stderr)
            if result.returncode or len(result.stdout) > 8 * 1024 * 1024:
                raise ValueError(label + " observer failed or exceeded output bound")
            snapshots[label] = json.loads(result.stdout)
            print(f"{label}: {len(snapshots[label]['rows'])} retrieval rows, {len(snapshots[label]['decay'])} decay rows", flush=True)
        report["comparison"] = comparisons(snapshots["baseline"], snapshots["candidate"])
        report["resolved_locks_match"] = (
            report["sources"]["baseline"]["resolved_lock_sha256"]
            == report["sources"]["candidate"]["resolved_lock_sha256"]
        )
        report["source_unchanged"] = (
            all(current(path) == data for path, data in new_sources.items())
            and all(current("bench-fixtures/" + name) == data for name, data in fixtures.items())
            and current("Cargo.lock") == lock
            and Path(__file__).read_bytes() == script
        )
        report["passed"] = (report["comparison"]["passed"] and report["source_unchanged"]
                            and report["resolved_locks_match"])
        print(f"ordered-ID mismatches: {len(report['comparison']['ordered_id_failures'])}; value mismatches: {len(report['comparison']['value_failures'])}; decay match: {report['comparison']['decay_match']}", flush=True)
        print(f"{'PASS' if report['passed'] else 'FAIL'} algorithm-slice parity: {root / 'report.json'}", flush=True)
        return 0 if report["passed"] else 1
    except Exception as error:
        report["error"] = str(error)
        traceback.print_exc()
        return 1
    finally:
        save()


if __name__ == "__main__":
    raise SystemExit(main())
