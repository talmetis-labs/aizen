//! Where a new fact belongs: the ONE decision point that turns a *proposal*
//! (what the model / write path asked for) into a *choice* (what actually gets written).
//!
//! Replaces `learning::scope_for`, which decided placement from `MemoryType` alone — so
//! "the user prefers Vietnamese" and "this repo pins windows-sys 0.59" were separated by a
//! type tag rather than by where they are true.
//!
//! Both entry points are PURE: filesystem existence arrives as an injected `exists` closure,
//! the current directory / home / device arrive as a [`Lineage`]. That is what makes the
//! clamp rules testable without touching a real disk.
//!
//! ## The rules (plan §Phase 1)
//!
//! | proposal | result |
//! |---|---|
//! | `user` | `User`, no anchor |
//! | `device` | `Device` tagged with this machine's id, no anchor |
//! | `place` + anchor is an ancestor of cwd | accepted (possibly walked up to an existing dir) |
//! | `place` + anchor is NOT an ancestor | clamped to the narrowest project/cwd, confidence ×0.8 |
//! | `place` + anchor at or above home | `Device` if the text is about the machine, else `User` |
//! | cwd itself IS home | same home fallback — **never** anchor the whole home dir |
//! | nothing / nonsense | `place` at the narrowest project/cwd, confidence ×0.7 |
//!
//! A guess that is too NARROW costs a missed recall; a guess that is too BROAD pollutes the
//! always-on prefix for every future directory. Missing is the cheaper failure, so every
//! ambiguous case clamps down, never up.

use crate::memory::path_scope::{is_ancestor, Lineage, Tier};
use once_cell::sync::Lazy;
use regex::Regex;

/// Confidence multiplier for an anchor that had to be clamped (the write path named a place
/// that does not contain the cwd — plausible, but we trust it less).
const CLAMP_PENALTY: f64 = 0.8;
/// Confidence multiplier when nothing usable was proposed and we fell back to the cwd.
const GUESS_PENALTY: f64 = 0.7;

/// What a write path *asked* for. Every field is optional/lenient — this is untrusted input
/// (a model's tool call, a legacy caller), not a decision.
#[derive(Debug, Clone, Default)]
pub struct TierProposal {
    /// The tier the caller named, if it named a valid one.
    pub tier: Option<Tier>,
    /// The anchor the caller named, already normalized (`config::anchor_of`).
    pub anchor: Option<String>,
    /// Does the fact text talk about the machine (toolchain, paths, hardware)? Only consulted
    /// when a `place` proposal collapses to the home dir and we must pick `Device` vs `User`.
    pub mentions_machine: bool,
    /// Does the fact text name the work — a path, a source file, a project marker or name
    /// ([`mentions_project`])? A `user` proposal that does is re-filed as `place`: a fact about
    /// a project is never about the person, whatever grammar it was written in.
    pub mentions_project: bool,
}

/// The resolved placement. `confidence_mult` is applied by the caller to its own confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct TierChoice {
    pub tier: Tier,
    pub anchor: Option<String>,
    pub device: Option<String>,
    pub confidence_mult: f64,
}

impl TierChoice {
    /// The unambiguous "about the person, applies everywhere" placement. Public because a few
    /// write paths know their fact is a user fact by construction (a denied STYLE rule, a
    /// `#remember` the user tagged themselves) and must not have it re-derived from the cwd.
    pub fn user_tier() -> Self {
        Self::user()
    }

    fn user() -> Self {
        TierChoice {
            tier: Tier::User,
            anchor: None,
            device: None,
            confidence_mult: 1.0,
        }
    }
    fn device(id: &str) -> Self {
        TierChoice {
            tier: Tier::Device,
            anchor: None,
            device: Some(id.to_string()),
            confidence_mult: 1.0,
        }
    }
    fn place(anchor: String, mult: f64) -> Self {
        TierChoice {
            tier: Tier::Place,
            anchor: Some(anchor),
            device: None,
            confidence_mult: mult,
        }
    }
}

/// Resolve a proposal into the placement that will actually be written.
///
/// `exists` answers "is this normalized path a real directory right now?" — injected so the
/// clamp logic is testable, and so a junction/`subst` path that has not been created yet gets
/// walked up to a directory that HAS been, instead of writing an anchor that can never match.
pub fn decide(p: &TierProposal, lin: &Lineage, exists: &dyn Fn(&str) -> bool) -> TierChoice {
    match p.tier {
        // A `user` fact that names a project or a path is about that project (M3): file it
        // where the work is, with the clamp a mis-named place gets. At home there is no
        // project to anchor to and `decide_place` falls back honestly.
        Some(Tier::User) if p.mentions_project => decide_place(p, lin, exists, CLAMP_PENALTY),
        Some(Tier::User) => TierChoice::user(),
        Some(Tier::Device) => TierChoice::device(&lin.device),
        Some(Tier::Place) => decide_place(p, lin, exists, CLAMP_PENALTY),
        // Nothing was said (or it did not parse): guess narrow and mark it as a guess.
        None => decide_place(p, lin, exists, GUESS_PENALTY),
    }
}

/// The `place` branch. `miss_penalty` is the multiplier applied when the named anchor is
/// unusable and we fall back to the cwd — different for "you named a bad place" (0.8) vs
/// "you named no place at all" (0.7).
fn decide_place(
    p: &TierProposal,
    lin: &Lineage,
    exists: &dyn Fn(&str) -> bool,
    miss_penalty: f64,
) -> TierChoice {
    // Standing IN the home dir means there is no project to anchor to: anchoring home would
    // make the fact fire in every directory the user ever visits, i.e. a `user` fact wearing a
    // `place` label. Say so honestly instead.
    if at_or_above_home(&lin.cwd, lin.home.as_deref()) {
        return home_fallback(p, lin);
    }

    let named = p.anchor.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let Some(named) = named else {
        // No anchor proposed → narrowest plausible place, flagged as a guess.
        return TierChoice::place(lin.narrowest_project_or_cwd(), miss_penalty);
    };

    let named = named.to_ascii_lowercase();

    // An anchor at/above home is the same pollution as cwd-at-home, just spelled explicitly.
    if at_or_above_home(&named, lin.home.as_deref()) {
        return home_fallback(p, lin);
    }

    if !is_ancestor(&named, &lin.cwd) {
        // The named place does not contain where we are. It may still be true, but we cannot
        // verify it from here, so clamp to what we CAN verify.
        return TierChoice::place(lin.narrowest_project_or_cwd(), miss_penalty);
    }

    // Accepted. Walk up to a directory that actually exists so the anchor can ever match.
    match nearest_existing(&named, lin, exists) {
        Some(a) => TierChoice::place(a, 1.0),
        None => TierChoice::place(lin.narrowest_project_or_cwd(), miss_penalty),
    }
}

/// `place` collapsed onto the home dir: re-file as `device` when the fact is about the machine,
/// else as `user`. Never returns an anchor.
fn home_fallback(p: &TierProposal, lin: &Lineage) -> TierChoice {
    if p.mentions_machine {
        TierChoice::device(&lin.device)
    } else {
        TierChoice::user()
    }
}

/// Is `path` the home dir itself, or an ancestor of it (drive root, `/Users`, …)?
/// No home resolvable → false, so a headless/CI environment still anchors normally.
fn at_or_above_home(path: &str, home: Option<&str>) -> bool {
    match home {
        Some(h) => is_ancestor(path, h), // path == home, or home lives under path
        None => false,
    }
}

/// The nearest ancestor of `anchor` (starting with `anchor` itself) that exists, stopping
/// before home. `None` when nothing in the chain exists.
fn nearest_existing(anchor: &str, lin: &Lineage, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut cur = anchor.trim_end_matches('/').to_string();
    loop {
        if exists(&cur) {
            return Some(cur);
        }
        if at_or_above_home(&cur, lin.home.as_deref()) {
            return None;
        }
        match cur.rfind('/') {
            Some(0) | None => return None,
            Some(i) => cur.truncate(i),
        }
    }
}

/// Real-filesystem `exists` for production callers.
pub fn fs_exists(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

/// Cheap, allocation-free hint for the `device` vs `user` fork: does this text talk about the
/// machine rather than the person? Deliberately conservative — a false negative files the fact
/// as `user` (still always-on, just not machine-scoped), a false positive would hide it on
/// every other machine.
pub fn mentions_machine(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "this machine",
        "this device",
        "this computer",
        "this laptop",
        "this pc",
        "máy này",
        "máy tính này",
        "installed at",
        "is installed",
        "not installed",
        "on my machine",
        "toolchain",
        "gpu",
        "cpu",
        "ram",
        "windows",
        "linux",
        "macos",
        "wsl",
        "path environment",
        "%localappdata%",
        "program files",
        "/usr/bin",
        "/usr/local",
    ];
    let low = text.to_lowercase();
    MARKERS.iter().any(|m| low.contains(m))
}

/// Build the proposal a plain `MemoryType`-only caller implies, so legacy write paths keep
/// working while they migrate to naming a tier explicitly.
///
/// `user`/`feedback` are about the person → `User`. `project`/`reference` are about the work in
/// front of us → `Place`, anchored where we stand.
pub fn proposal_from_mtype(
    mtype: crate::memory::store::MemoryType,
    body: &str,
    lin: &Lineage,
) -> TierProposal {
    use crate::memory::store::MemoryType;
    match mtype {
        MemoryType::User | MemoryType::Feedback => TierProposal {
            tier: Some(Tier::User),
            anchor: None,
            mentions_machine: mentions_machine(body),
            mentions_project: mentions_project(body, lin),
        },
        MemoryType::Project | MemoryType::Reference => TierProposal {
            tier: Some(Tier::Place),
            anchor: Some(lin.narrowest_project_or_cwd()),
            mentions_machine: mentions_machine(body),
            mentions_project: true,
        },
    }
}

/// Does the fact text name the work rather than the person — a path, a source file, a project
/// marker, or the name of a place in the lineage? Quality plan M3: 239 `tier: user` rows on the
/// measured store were other projects' architecture notes, filed as `user` because the extractor
/// keyed the tier off the sentence's grammar ("I …", "prefer …"). A fact that names a project or
/// a path is about that project — it is `place`, never `user` — and [`decide`] enforces it.
///
/// Deliberately narrow: a product name that looks like a file (`node.js`, `next.js`) is a
/// preference, not a path, so the extension list leaves `js`/`ts`/`go`/`c` out; `and/or`, `w/o`
/// are not paths; a single generic word (`src`, `docs`) taken from the lineage is not a name.
pub fn mentions_project(text: &str, lin: &Lineage) -> bool {
    let low = text.to_lowercase();
    if PROJECT_MARKERS.iter().any(|m| low.contains(m)) {
        return true;
    }
    if RE_DRIVE_PATH.is_match(&low) || RE_SOURCE_FILE.is_match(&low) {
        return true;
    }
    if has_slash_path(&low) {
        return true;
    }
    project_names(lin).iter().any(|name| has_word(&low, name))
}

/// Phrases that name the work explicitly, in either language the store holds.
const PROJECT_MARKERS: &[&str] = &[
    "this repo",
    "the repo",
    "this project",
    "this codebase",
    "the codebase",
    "this crate",
    "this workspace",
    "in the repo",
    "monorepo",
    "cargo.toml",
    "package.json",
    "pyproject",
    "repo này",
    "dự án này",
    "codebase này",
    "project này",
    "trong repo",
    "trong dự án",
];

static RE_DRIVE_PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[a-z]:[\\/]").expect("regex"));
static RE_SOURCE_FILE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b[\w-]+\.(?:rs|py|pyi|tsx|jsx|mjs|cjs|toml|yml|yaml|lock|md|sh|ps1|cpp|hpp)\b")
        .expect("regex")
});

/// `a/b` with real segments on both sides: `src/main.rs`, `docs/plan`, `./scripts` — but not
/// `and/or`, `w/o`, `24/7`.
fn has_slash_path(low: &str) -> bool {
    let b = low.as_bytes();
    let is_seg = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.');
    for (i, &c) in b.iter().enumerate() {
        if c != b'/' && c != b'\\' {
            continue;
        }
        let mut l = i;
        while l > 0 && is_seg(b[l - 1]) {
            l -= 1;
        }
        let mut r = i + 1;
        while r < b.len() && is_seg(b[r]) {
            r += 1;
        }
        let (left, right) = (&low[l..i], &low[i + 1..r]);
        if left.is_empty() || right.is_empty() {
            continue;
        }
        if left.bytes().all(|c| c.is_ascii_digit()) && right.bytes().all(|c| c.is_ascii_digit()) {
            continue; // 24/7, 3/4
        }
        if matches!(
            left,
            "and" | "w" | "he" | "she" | "his" | "her" | "yes" | "s"
        ) {
            continue;
        }
        if left.contains('.') || right.contains('.') || (left.len() >= 3 && right.len() >= 3) {
            return true;
        }
    }
    false
}

/// Names of the places in the lineage below home — the project directory's basename and the
/// like. Generic directory words are not names.
fn project_names(lin: &Lineage) -> Vec<String> {
    const GENERIC: &[&str] = &[
        "src",
        "lib",
        "app",
        "apps",
        "bin",
        "test",
        "tests",
        "docs",
        "doc",
        "home",
        "desktop",
        "documents",
        "projects",
        "project",
        "code",
        "work",
        "dev",
        "repos",
        "repo",
        "github",
        "source",
        "sources",
        "tmp",
        "temp",
        "users",
        "user",
    ];
    let mut names: Vec<String> = Vec::new();
    for place in lin.places.iter().chain(std::iter::once(&lin.cwd)) {
        if at_or_above_home(place, lin.home.as_deref()) {
            continue;
        }
        let Some(base) = place.trim_end_matches('/').rsplit('/').next() else {
            continue;
        };
        let base = base.to_ascii_lowercase();
        if base.len() < 3 || GENERIC.contains(&base.as_str()) || names.contains(&base) {
            continue;
        }
        names.push(base);
    }
    names
}

/// Whole-word, case-folded containment: `aizen` in "the aizen repl", not in "aizenite".
fn has_word(low: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = low[start..].find(word) {
        let i = start + pos;
        let j = i + word.len();
        let before_ok = i == 0 || !low.as_bytes()[i - 1].is_ascii_alphanumeric();
        let after_ok = j >= low.len() || !low.as_bytes()[j].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = j;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lineage rooted under a fake home, with no real filesystem behind it.
    fn lin(cwd: &str) -> Lineage {
        Lineage {
            cwd: cwd.to_string(),
            places: vec![cwd.to_string()],
            device: "dev-deadbeef".to_string(),
            home: Some("c:/users/admin".to_string()),
        }
    }

    fn all_exist(_: &str) -> bool {
        true
    }
    fn none_exist(_: &str) -> bool {
        false
    }

    /// A lineage that knows its project directory, the way `Lineage::current` builds one.
    fn lin_in_project() -> Lineage {
        Lineage {
            cwd: "c:/users/admin/work/aizen/src".to_string(),
            places: vec![
                "c:/users/admin/work/aizen/src".to_string(),
                "c:/users/admin/work/aizen".to_string(),
            ],
            device: "dev-deadbeef".to_string(),
            home: Some("c:/users/admin".to_string()),
        }
    }

    #[test]
    fn a_fact_that_names_the_work_is_never_a_user_fact() {
        let lin = lin_in_project();
        for text in [
            "I keep the config in src/core/config.rs",
            "prefer rustls in this repo",
            "my notes live under C:\\work\\notes",
            "the aizen binary must stay a single static file",
            "Cargo.toml uses license = Apache-2.0",
            "tôi muốn giữ README.md dưới 110 dòng",
        ] {
            assert!(mentions_project(text, &lin), "{text:?} names the work");
            let c = decide(
                &TierProposal {
                    tier: Some(Tier::User),
                    anchor: None,
                    mentions_machine: false,
                    mentions_project: true,
                },
                &lin,
                &all_exist,
            );
            assert_eq!(c.tier, Tier::Place, "{text:?} is re-filed as place");
            assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/work/aizen/src"));
        }
        for text in [
            "prefers terse answers",
            "I like node.js and next.js",
            "reply in Vietnamese and/or English",
            "available 24/7 on weekdays",
            "my name is Bao",
        ] {
            assert!(
                !mentions_project(text, &lin),
                "{text:?} is about the person"
            );
        }
        // Generic directory words in the lineage are not project names.
        assert!(
            !mentions_project("put it in src", &lin),
            "a generic directory word is not a project name"
        );
        let names = project_names(&lin);
        assert_eq!(names, vec!["aizen"], "{names:?}");
    }

    #[test]
    fn user_proposal_never_gets_an_anchor() {
        let c = decide(
            &TierProposal {
                tier: Some(Tier::User),
                ..Default::default()
            },
            &lin("c:/users/admin/proj"),
            &all_exist,
        );
        assert_eq!(c.tier, Tier::User);
        assert_eq!(c.anchor, None);
        assert_eq!(c.device, None);
    }

    #[test]
    fn device_proposal_is_tagged_with_this_machine() {
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Device),
                ..Default::default()
            },
            &lin("c:/users/admin/proj"),
            &all_exist,
        );
        assert_eq!(c.tier, Tier::Device);
        assert_eq!(c.device.as_deref(), Some("dev-deadbeef"));
        assert_eq!(c.anchor, None);
    }

    #[test]
    fn ancestor_anchor_is_accepted_verbatim() {
        let l = lin("c:/users/admin/proj/src/agent");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/proj".into()),
                mentions_machine: false,
                mentions_project: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/proj"));
        assert_eq!(c.confidence_mult, 1.0);
    }

    #[test]
    fn clamp_rejects_non_ancestor() {
        let l = lin("c:/users/admin/proj");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/other".into()),
                mentions_machine: false,
                mentions_project: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(c.tier, Tier::Place);
        assert_ne!(c.anchor.as_deref(), Some("c:/users/admin/other"));
        assert!(
            c.confidence_mult < 1.0,
            "a clamped anchor must cost confidence"
        );
    }

    #[test]
    fn clamp_never_goes_above_home() {
        let l = lin("c:/users/admin/proj");
        for above in ["c:/users/admin", "c:/users", "c:/"] {
            let c = decide(
                &TierProposal {
                    tier: Some(Tier::Place),
                    anchor: Some(above.into()),
                    mentions_machine: false,
                    mentions_project: false,
                },
                &l,
                &all_exist,
            );
            assert_ne!(
                c.tier,
                Tier::Place,
                "{above} must not become a place anchor"
            );
            assert_eq!(c.anchor, None, "{above} must not produce an anchor");
        }
    }

    #[test]
    fn cwd_at_home_yields_user_or_device_never_anchor() {
        let l = lin("c:/users/admin");
        let as_user = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin".into()),
                mentions_machine: false,
                mentions_project: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(as_user.tier, Tier::User);
        assert_eq!(as_user.anchor, None);

        let as_device = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: None,
                mentions_machine: true,
                mentions_project: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(as_device.tier, Tier::Device);
        assert_eq!(as_device.anchor, None);
    }

    #[test]
    fn nonexistent_path_falls_back_to_existing_ancestor() {
        let l = lin("c:/users/admin/proj/src/agent/lsp");
        // Only `.../proj` exists on this fake disk.
        let exists = |p: &str| p == "c:/users/admin/proj";
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/proj/src/agent/lsp".into()),
                mentions_machine: false,
                mentions_project: false,
            },
            &l,
            &exists,
        );
        assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/proj"));
    }

    #[test]
    fn nothing_existing_clamps_instead_of_writing_a_dead_anchor() {
        let l = lin("c:/users/admin/proj");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/proj/ghost".into()),
                mentions_machine: false,
                mentions_project: false,
            },
            &l,
            &none_exist,
        );
        assert!(c.confidence_mult < 1.0);
        assert_ne!(c.anchor.as_deref(), Some("c:/users/admin/proj/ghost"));
    }

    #[test]
    fn no_proposal_guesses_narrow_and_pays_for_it() {
        let l = lin("c:/users/admin/proj");
        let c = decide(&TierProposal::default(), &l, &all_exist);
        assert_eq!(c.tier, Tier::Place);
        assert!(c.anchor.is_some());
        assert_eq!(c.confidence_mult, GUESS_PENALTY);
    }

    #[test]
    fn anchor_case_is_folded_so_prefix_matching_holds() {
        let l = lin("c:/users/admin/proj/src");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("C:/Users/Admin/Proj".into()),
                mentions_machine: false,
                mentions_project: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/proj"));
    }

    #[test]
    fn mentions_machine_catches_both_languages() {
        assert!(mentions_machine("this machine has no gcc"));
        assert!(mentions_machine("máy này không có gcc"));
        assert!(mentions_machine("git is installed at C:/Program Files/Git"));
        assert!(!mentions_machine("prefers concise answers"));
    }

    #[test]
    fn mtype_proposal_routes_person_vs_work() {
        use crate::memory::store::MemoryType;
        let l = lin("c:/users/admin/proj");
        assert_eq!(
            proposal_from_mtype(MemoryType::User, "likes tabs", &l).tier,
            Some(Tier::User)
        );
        assert_eq!(
            proposal_from_mtype(MemoryType::Feedback, "be terser", &l).tier,
            Some(Tier::User)
        );
        assert_eq!(
            proposal_from_mtype(MemoryType::Project, "pins windows-sys", &l).tier,
            Some(Tier::Place)
        );
        assert!(proposal_from_mtype(MemoryType::Reference, "see docs", &l)
            .anchor
            .is_some());
    }
}
