//! Working-directory TYPE classification — a coarse "what KIND of place am I working in" verdict
//! that lets the memory brain tune its recall policy per directory kind (Phase 5 wiring).
//!
//! This is a SECOND axis, orthogonal to [`crate::core::config::project_slug`] (the *identity* of a
//! workspace) and to [`crate::memory::store::MemoryType`] (a fact's *aboutness*). Where the slug
//! answers "which zone", `WorkdirKind` answers "what sort of work happens here" — a git code repo, a
//! research/notes folder, a generic working directory, or a large monorepo. A living assistant
//! recalls differently in each: commands + codebase facts matter in a repo; durable reference notes
//! matter in a research folder.
//!
//! # Two-tier, free-first detection
//! - **Always: a shallow scan** of the project root's top-level entries (non-recursive, capped) —
//!   manifest / `.git` → code; docs-dominant → research; no code → work. Zero dependency on `/init`.
//! - **Upgrade when `/init` has run:** read the persisted codebase index (`analysis.workspaces` +
//!   file count) to promote a code repo to `Large` when it is a monorepo / at scale. Reuses the
//!   existing index; never re-scans the tree here.
//!
//! DERIVED, not stored: classification runs on demand (cheap), cached per `(NG_PROJECT_ROOT, cwd)`
//! exactly like [`crate::core::config::project_root`], so a directory change re-classifies but a
//! repeated call in one workspace pays the scan once.

use crate::core::config;
use std::path::Path;

/// Kill-switch: `AIZEN_NO_WORKDIR=1` forces the neutral [`WorkdirKind::Work`] verdict everywhere, so
/// per-kind recall policy collapses to the baseline. Escape hatch for debugging / benching — the
/// classification itself is cheap and always safe, this only decouples policy from it.
pub fn workdir_disabled() -> bool {
    matches!(
        std::env::var("AIZEN_NO_WORKDIR").ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// What sort of work a directory is for. Coarse by design — four buckets a recall policy can key off
/// without a brittle taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkdirKind {
    /// A software project: a build manifest and/or a `.git` dir, or code files dominate.
    Code,
    /// A research / notes / writing folder: docs (`.md`/`.pdf`/`.ipynb`/`.tex`/…) dominate and there
    /// is little or no code.
    Research,
    /// A generic working directory: no strong code or docs signal. The neutral default.
    #[default]
    Work,
    /// A large / monorepo codebase: `Code`, plus the `/init` index shows many packages or files.
    Large,
}

impl WorkdirKind {
    /// A one-line hint bumped into `<environment>` so the model knows how this place is treated
    /// (Phase 5). Kept short — it rides the always-on prefix.
    pub fn environment_hint(self) -> &'static str {
        match self {
            WorkdirKind::Code => {
                "Working directory looks like a code project — recall favors commands, codebase facts, and past fixes."
            }
            WorkdirKind::Large => {
                "Working directory looks like a large/monorepo codebase — recall is tightly scoped to the current area."
            }
            WorkdirKind::Research => {
                "Working directory looks like a research/notes folder — recall favors durable reference notes."
            }
            WorkdirKind::Work => {
                "Working directory is a general workspace — recall uses the default policy."
            }
        }
    }
}

// ── shallow-scan lexicons ──────────────────────────────────────────────────────
/// Top-level filenames that mark a buildable software project (any one ⇒ Code).
const CODE_MANIFESTS: &[&str] = &[
    "cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "gemfile",
    "composer.json",
    "makefile",
    "cmakelists.txt",
    "build.sbt",
    "mix.exs",
    "pubspec.yaml",
    "deno.json",
];
/// Source-file extensions (dominant count ⇒ Code even without a manifest).
const CODE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "kts", "c", "cc", "cpp",
    "cxx", "h", "hpp", "cs", "rb", "php", "swift", "scala", "sh", "bash", "zig", "dart", "ex", "exs",
    "clj", "hs", "ml", "lua",
];
/// Doc / research file extensions (dominant count with little code ⇒ Research).
const DOC_EXTS: &[&str] =
    &["md", "markdown", "pdf", "ipynb", "tex", "rst", "org", "txt", "docx", "csv", "bib"];

/// Files above this many top-level entries stop the shallow scan (bounds a pathological tree). The
/// scan is non-recursive, so this is generous — most roots have far fewer.
const SHALLOW_SCAN_CAP: usize = 512;
/// `/init` index file count at or above which a code repo is treated as `Large`.
const LARGE_FILE_THRESHOLD: usize = 1500;
/// `/init` index workspace (package) count at or above which a code repo is treated as `Large`
/// (a monorepo — several packages under one root).
const LARGE_WORKSPACE_THRESHOLD: usize = 3;

/// The current workspace's directory kind. Cached per `(NG_PROJECT_ROOT, cwd)` — same key shape as
/// [`config::project_root`], so a `cd` re-classifies but repeated callers in one dir share the scan.
pub fn classify() -> WorkdirKind {
    static CACHE: std::sync::Mutex<Option<(String, WorkdirKind)>> = std::sync::Mutex::new(None);
    let cache_key = format!(
        "{}|{}",
        std::env::var("NG_PROJECT_ROOT").unwrap_or_default(),
        std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()
    );
    if let Ok(guard) = CACHE.lock() {
        if let Some((k, kind)) = guard.as_ref() {
            if *k == cache_key {
                return *kind;
            }
        }
    }
    let kind = compute();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((cache_key, kind));
    }
    kind
}

/// Uncached classification behind [`classify`]'s cache.
fn compute() -> WorkdirKind {
    let root = config::project_root();
    let base = shallow_classify(&root);
    // Only a code repo can be promoted to Large — a docs folder never becomes a monorepo.
    if base == WorkdirKind::Code && looks_large(&config::project_slug()) {
        return WorkdirKind::Large;
    }
    base
}

/// The always-available tier: a non-recursive scan of the root's top-level entries.
fn shallow_classify(root: &Path) -> WorkdirKind {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return WorkdirKind::Work, // unreadable → neutral, never guess
    };
    let mut has_manifest = false;
    let mut has_git = false;
    let mut code_files = 0usize;
    let mut doc_files = 0usize;
    // Non-recursive + capped: `take` bounds a pathological root without a manual loop counter.
    for ent in rd.flatten().take(SHALLOW_SCAN_CAP) {
        let path = ent.path();
        let name_lower =
            path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if name_lower == ".git" {
                has_git = true;
            }
            continue; // shallow: don't recurse into subdirs
        }
        if CODE_MANIFESTS.contains(&name_lower.as_str()) {
            has_manifest = true;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if !ext.is_empty() {
            if CODE_EXTS.contains(&ext.as_str()) {
                code_files += 1;
            } else if DOC_EXTS.contains(&ext.as_str()) {
                doc_files += 1;
            }
        }
    }

    if has_manifest || has_git {
        return WorkdirKind::Code;
    }
    // Docs clearly dominate and there's little code → a research/notes folder.
    if doc_files >= 2 && doc_files > code_files {
        return WorkdirKind::Research;
    }
    if code_files > 0 {
        return WorkdirKind::Code;
    }
    WorkdirKind::Work
}

/// Minimal peek at a persisted `/init` index — just the two scale signals, so we never load the
/// (potentially large) per-file records into owned structs. `files` uses [`serde::de::IgnoredAny`]
/// so array elements are counted while their contents are discarded during parse.
#[derive(serde::Deserialize, Default)]
struct IndexPeek {
    #[serde(default)]
    files: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    analysis: AnalysisPeek,
}

#[derive(serde::Deserialize, Default)]
struct AnalysisPeek {
    #[serde(default)]
    workspaces: Vec<String>,
}

/// Does the `/init` index (if present) say this code repo is at monorepo / large scale? Missing or
/// unreadable index → `false` (a repo with no index is judged by the shallow scan alone). Best-effort.
fn looks_large(slug: &str) -> bool {
    let path = config::codebase_index_path(slug);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let peek: IndexPeek = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(_) => return false,
    };
    peek.files.len() >= LARGE_FILE_THRESHOLD
        || peek.analysis.workspaces.len() >= LARGE_WORKSPACE_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Point the project root at a throwaway dir, run `f`, restore. Serializes on the shared home
    /// lock (these mutate the `NG_PROJECT_ROOT` env the classifier reads) and clears the classify
    /// cache both sides so a stale verdict from another test can't bleed in.
    fn with_root<T>(tag: &str, build: impl FnOnce(&Path), f: impl FnOnce() -> T) -> T {
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-workdir-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        build(&dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);
        let out = f();
        std::env::remove_var("NG_PROJECT_ROOT");
        let _ = fs::remove_dir_all(&dir);
        out
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), "x").unwrap();
    }

    #[test]
    fn manifest_marks_code() {
        with_root(
            "code-manifest",
            |d| touch(d, "Cargo.toml"),
            || assert_eq!(shallow_classify(&config::project_root()), WorkdirKind::Code),
        );
    }

    #[test]
    fn git_dir_marks_code() {
        with_root(
            "code-git",
            |d| {
                fs::create_dir_all(d.join(".git")).unwrap();
                touch(d, "notes.md"); // even with a doc, a repo is code
            },
            || assert_eq!(shallow_classify(&config::project_root()), WorkdirKind::Code),
        );
    }

    #[test]
    fn docs_dominant_marks_research() {
        with_root(
            "research",
            |d| {
                touch(d, "paper.md");
                touch(d, "notes.md");
                touch(d, "data.csv");
            },
            || assert_eq!(shallow_classify(&config::project_root()), WorkdirKind::Research),
        );
    }

    #[test]
    fn code_files_without_manifest_marks_code() {
        with_root(
            "loose-code",
            |d| {
                touch(d, "main.rs");
                touch(d, "util.rs");
            },
            || assert_eq!(shallow_classify(&config::project_root()), WorkdirKind::Code),
        );
    }

    #[test]
    fn empty_dir_is_work() {
        with_root(
            "empty",
            |_| {},
            || assert_eq!(shallow_classify(&config::project_root()), WorkdirKind::Work),
        );
    }

    #[test]
    fn one_doc_is_not_enough_for_research() {
        // A single stray README in an otherwise-empty dir is not a research folder.
        with_root(
            "one-doc",
            |d| touch(d, "README.md"),
            || assert_eq!(shallow_classify(&config::project_root()), WorkdirKind::Work),
        );
    }

    #[test]
    fn large_upgrade_from_workspaces() {
        assert!(!looks_large("nonexistent-slug-00000000"), "no index → not large");
        // A hand-written index with >=3 workspaces reads as large.
        with_root(
            "large-mono",
            |_| {},
            || {
                let slug = config::project_slug();
                let idx_path = config::codebase_index_path(&slug);
                fs::create_dir_all(idx_path.parent().unwrap()).unwrap();
                let json = r#"{"version":2,"root":"/x","built_unix":0,
                    "analysis":{"workspaces":["packages/a","packages/b","packages/c"]},
                    "files":[]}"#;
                fs::write(&idx_path, json).unwrap();
                assert!(looks_large(&slug), "3 workspaces → large");
            },
        );
    }

    #[test]
    fn classify_is_cached_and_returns_a_kind() {
        with_root(
            "cache",
            |d| touch(d, "Cargo.toml"),
            || {
                let a = classify();
                let b = classify();
                assert_eq!(a, b);
                assert_eq!(a, WorkdirKind::Code);
            },
        );
    }

    #[test]
    fn disabled_flag_parses() {
        std::env::remove_var("AIZEN_NO_WORKDIR");
        assert!(!workdir_disabled());
    }
}
