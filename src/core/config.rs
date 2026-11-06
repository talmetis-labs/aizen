//! Core-owned home, workspace identity, and codebase retrieval configuration.
//!
//! Persistent User Memory belongs to the selected provider. The historical
//! `cli-memory/codebase` and dense cache locations remain Core-owned for compatibility.

use std::path::{Path, PathBuf};

/// One process-wide lock for tests that mutate the global home env var. Tests run in parallel
/// across modules, so a per-module mutex isn't enough — they'd race on the same env var. Every
/// home-mutating test must hold THIS lock.
#[cfg(test)]
pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set env vars for a test and RESTORE their prior values on drop.
///
/// The obvious teardown — `remove_var` for each key — is wrong for `USERPROFILE`/`HOME`, and the bug
/// it caused was subtle enough to hunt twice. Several helpers point those at a sandbox and then
/// delete them, so after the first such test the process has NO home at all. Code that treats "no
/// home" as "no home boundary" then loses its guard: `lsp::discovery::is_forbidden_root` returns
/// false for every directory, so a walk that should stop below the user profile climbs past it and
/// claims whatever manifest sits there. That produced a failure in an unrelated test, in a different
/// module, only in a full parallel run — and pointed the blame at a stray `~/package.json` rather
/// than at the teardown.
///
/// Restoring the prior value (including "was absent") keeps a test's mutation from outliving it.
#[cfg(test)]
pub(crate) struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl EnvGuard {
    /// Snapshot `keys`, then apply `set`. Keys in `set` need not appear in `keys`.
    pub(crate) fn set<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut saved = Vec::new();
        for (k, v) in vars {
            let key = k.as_ref().to_string_lossy().to_string();
            if !saved.iter().any(|(s, _): &(String, _)| *s == key) {
                saved.push((key, std::env::var_os(k.as_ref())));
            }
            std::env::set_var(k.as_ref(), v.as_ref());
        }
        Self { saved }
    }

    /// Snapshot `keys` and remove them, restoring on drop. For tests that need a var ABSENT.
    pub(crate) fn unset<I, K>(keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: AsRef<std::ffi::OsStr>,
    {
        let mut saved = Vec::new();
        for k in keys {
            saved.push((
                k.as_ref().to_string_lossy().to_string(),
                std::env::var_os(k.as_ref()),
            ));
            std::env::remove_var(k.as_ref());
        }
        Self { saved }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(val) => std::env::set_var(&k, val),
                None => std::env::remove_var(&k),
            }
        }
    }
}

/// Resolve the home root: `AIZEN_HOME` (legacy `AIZEN_HOME`) else `USERPROFILE`/`HOME`/cwd +
/// `/.aizen`. When no env override is set, a pre-rebrand `~/.aizen` is migrated to `~/.aizen`
/// on first use (atomic same-parent rename) so memory/personas/soul/config carry over. The fn
/// name stays `aizen_home` (internal; called everywhere) — only the path + brand changed.
/// The user's HOME directory (`USERPROFILE`/`HOME`), falling back to the process cwd when neither
/// is set. This is the anchor for the sandbox policy's credential-directory deny list and
/// toolchain-cache roots — distinct from [`aizen_home`], which is the `~/.aizen` DATA root.
pub fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn aizen_home() -> PathBuf {
    for var in ["AIZEN_HOME", "AIZEN_HOME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return PathBuf::from(v); // explicit override (also how tests pin a temp home) — verbatim, no migration
            }
        }
    }
    let home = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| ".".to_string());
    resolve_default_home(Path::new(&home))
}

/// The home data root: always `<base>/.aizen`.
///
/// There is deliberately NO legacy-name fallback here. The pre-rebrand `.aizen` root predates
/// this repository: the earliest tag (v0.4.0) already defaults to `.aizen`, so no released build
/// ever created a `.aizen` home, and a migration branch for a directory nothing ever wrote is
/// dead weight that keeps the old brand alive in every path decision.
fn resolve_default_home(base: &Path) -> PathBuf {
    base.join(".aizen")
}

/// [`resolve_default_home`] for the identity pin in the test suite, which must not depend on the
/// process environment it happens to run under.
#[cfg(test)]
pub(crate) fn resolve_default_home_for_test(base: &Path) -> PathBuf {
    resolve_default_home(base)
}

/// The project root for PROJECT-LOCAL customization (`./.aizen/`): the git repo top-level if we're
/// in one, else the current dir. Repo-root-aware so launching `aizen` from a SUBDIR still finds the
/// repo's `.aizen/` (R4 — fixes the cwd-relative footgun). `AIZEN_PROJECT_ROOT` overrides (tests + an
/// escape hatch). Shell-out to git keeps the pure-static posture (no git2/gix).
pub fn project_root() -> PathBuf {
    if let Ok(v) = std::env::var("AIZEN_PROJECT_ROOT") {
        let v = v.trim();
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(mut cmd) = crate::core::gitx::command() {
        if let Ok(out) = cmd.args(["rev-parse", "--show-toplevel"]).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return PathBuf::from(s);
                }
            }
        }
    }
    // No usable git (absent, or it refused — e.g. dubious-ownership): walk UP for a project
    // marker, pure-Rust. One checkout must keep ONE identity even on a gitless machine — the old
    // straight-to-cwd fallback minted a fresh zone per LAUNCH DIR, which is half of the slug-fork
    // disease. Looking ONLY for `.git` left that fork wide open for trees that aren't repos at all:
    // `proj/` and `proj/src/` became two projects, so each subdir grew its own zone and disowned
    // the other's sessions. A VCS marker outranks a manifest (it bounds the whole checkout), and
    // within each class the OUTERMOST match wins — same semantics as `git rev-parse --show-toplevel`
    // for a workspace member. cwd is the true last resort (a bare dir with no marker at all).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    const VCS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];
    const MANIFEST: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
    // The walk stops BELOW the user's home dir. Without that bound a stray manifest in `~` would
    // make every non-repo tree under it share one root — collapsing unrelated projects into a
    // single zone, which is a worse identity bug than the per-subdir fork this fixes. (`.aizen`
    // deliberately is NOT a marker: `~/.aizen` is the global home, so honoring it would resolve
    // every project under the home dir to the home dir itself.)
    let home_bound = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from);
    let outermost = |markers: &[&str]| -> Option<PathBuf> {
        let mut found = None;
        let mut cur = Some(cwd.as_path());
        while let Some(d) = cur {
            // `h.starts_with(d)` is true exactly when `d` IS the home dir or an ancestor of it.
            if home_bound.as_deref().is_some_and(|h| h.starts_with(d)) {
                break;
            }
            if markers.iter().any(|m| d.join(m).exists()) {
                found = Some(d.to_path_buf());
            }
            cur = d.parent();
        }
        found
    };
    outermost(&VCS)
        .or_else(|| outermost(&MANIFEST))
        .unwrap_or_else(|| cwd.clone())
}

/// True when an env var is set to a truthy token (`1`/`true`/`yes`/`on`).
/// Core Skill-affinity switches share this parser.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Compatibility kill-switch for Core Skill affinity: `AIZEN_NO_GRAPH=1` disables
/// provider-assisted graph ranking. This does not open or write a legacy Memory graph.
pub fn graph_disabled() -> bool {
    env_flag("AIZEN_NO_GRAPH")
}

/// Explicit provider policy for Memory graph expansion; Core never opens the graph itself.
/// Recording must remain enabled and expansion stays opt-in.
pub fn graph_expand_enabled() -> bool {
    !graph_disabled() && env_flag("AIZEN_GRAPH_EXPAND")
}

/// Opt-in switch for ranking the Core `<skills>` index by graph affinity.
/// The selected provider supplies affinity through its explicit capability contract;
/// this flag neither opens nor records a legacy Memory graph.
pub fn skill_graph_rank_enabled() -> bool {
    !graph_disabled() && env_flag("AIZEN_SKILL_GRAPH_RANK")
}

/// FNV-1a 64-bit — tiny local hash for the project-slug stable key (core must not depend on
/// `memory::embed`; 8 lines beats a crate).
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// File/frontmatter-safe slug fragment: lowercase ASCII words, `-` between them and nowhere else.
///
/// This is half of the zone key (`dirname-hex8`), so it names the directory holding a project's
/// memory, skills and index. Folding through [`crate::core::slug`] means a checkout under an
/// accented path gets a readable zone instead of a shredded one — `~/Dự án/aizen` keyed
/// `d-n-aizen-…` before.
///
/// A dirname that is already plain ASCII produces the byte-identical fragment it always did, so no
/// existing zone is re-keyed. For the accented minority the fragment does change; that is a re-key,
/// and [`legacy_slug_candidates_for`] emits the pre-fold spelling so `aizen zone migrate` can still
/// find and merge the old directory.
fn slug_fragment(name: &str) -> String {
    let s = crate::core::slug::slug_words(name, 24);
    if s.is_empty() {
        "project".to_string()
    } else {
        s
    }
}

/// The pre-fold [`slug_fragment`] — kept verbatim so `zone migrate` can name the directory a
/// non-ASCII checkout was keyed under before the fold. Not used for new keys.
fn slug_fragment_prefold(name: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "project".to_string()
    } else {
        s.chars().take(24).collect()
    }
}

/// The stable identity of the current workspace: `dirname-hex8`. The hash key is the workspace
/// root's canonicalized path in ONE normalized spelling ([`workspace_key`]) — identity is WHERE
/// the checkout lives, full stop. The old key preferred `remote.origin.url` with a raw
/// `canonicalize` fallback, which let *whether git happened to spawn* (PATH luck) pick the key:
/// the same repo silently forked into twin memory/skills/index zones. A moved or re-cloned
/// checkout is now an explicit `aizen zone migrate`, never an implicit re-key. Cached per
/// (`AIZEN_PROJECT_ROOT` env, cwd) so tests that repoint `AIZEN_PROJECT_ROOT` are never served a
/// stale slug.
pub fn project_slug() -> String {
    identity().2
}

/// The normalized identity key of the current project root — the exact string [`project_slug`]
/// hashes. Session provenance stamps store it so "same project?" checks agree byte-for-byte with
/// zone identity instead of re-deriving their own path comparison.
pub fn project_key() -> String {
    identity().1
}

/// `(root, key, slug)` for the current workspace, computed ONCE per (`AIZEN_PROJECT_ROOT`, cwd).
///
/// The three are derived from the same `project_root()` call by construction, so a caller can never
/// see a key and a slug that disagree. Sharing the cache also matters for cost: `project_root()` may
/// spawn `git rev-parse`, and stamping session provenance wants all three every autosave — three
/// separate lookups would have meant three git spawns per turn.
fn identity() -> (PathBuf, String, String) {
    type WorkspaceIdentity = (PathBuf, String, String);
    type CachedIdentity = Option<(String, WorkspaceIdentity)>;
    static CACHE: std::sync::Mutex<CachedIdentity> = std::sync::Mutex::new(None);
    let cache_key = format!(
        "{}|{}",
        std::env::var("AIZEN_PROJECT_ROOT").unwrap_or_default(),
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    if let Ok(guard) = CACHE.lock() {
        if let Some((k, ident)) = guard.as_ref() {
            if *k == cache_key {
                return ident.clone();
            }
        }
    }
    let root = project_root();
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let key = workspace_key(&root);
    let slug = slug_for_key(name, &key);
    let ident = (root, key, slug);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((cache_key, ident.clone()));
    }
    ident
}

/// The `dirname-hex8` slug for an explicit (name, hash key) pair — the one formatter both the
/// live keying and the legacy-candidate computation go through (pub(crate) so the zones tests
/// can fabricate byte-exact legacy slugs).
pub(crate) fn slug_for_key(name: &str, key: &str) -> String {
    format!("{}-{:08x}", slug_fragment(name), fnv1a64(key) as u32)
}

/// Hash key for a workspace root: its canonicalized path in ONE normalized spelling — verbatim
/// `\\?\` prefix stripped, `\` → `/`, drive letter lowercased, no trailing slash — mirroring
/// `workspace_txn::normalized_path` so the two identity systems agree on "same directory".
/// Canonicalize-failure falls back to the raw path through the same normalizer (still
/// deterministic for a given spelling), so a vanished dir can't panic the slug.
fn workspace_key(root: &Path) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    normalize_path_key(&canon)
}

/// See [`workspace_key`]. Split out so tests can prove every spelling of one directory
/// (verbatim vs plain, trailing slash, mixed separators) hashes identically.
pub fn normalize_path_key(p: &Path) -> String {
    let mut value = p.to_string_lossy().replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/UNC/") {
        // Verbatim UNC (`\\?\UNC\server\share`) and the plain `\\server\share` spelling of the
        // same share must collapse to one key, or network checkouts fork on canonicalize luck.
        value = format!("//{rest}");
    } else if let Some(rest) = value.strip_prefix("//?/") {
        value = rest.to_string();
    }
    if value.as_bytes().get(1) == Some(&b':') {
        value.replace_range(0..1, &value[..1].to_ascii_lowercase());
    }
    value.trim_end_matches('/').to_string()
}

/// Slugs the pre-2026-07 keying could have produced for the CURRENT project. Two eras to cover:
/// the remote-URL key (git worked), and the path keys (git didn't) — where the OLD gitless
/// fallback rooted at the LAUNCH DIR, so a habitual subdir launch keyed a zone under the
/// SUBDIR's name and path. Candidates therefore span the root itself, today's cwd, and every
/// directory between them, each in its canonicalize-verbatim and raw spellings. The current
/// slug itself is excluded; `aizen zone migrate` merges artifacts found under the rest.
pub fn legacy_slug_candidates() -> Vec<String> {
    let root = project_root();
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
    legacy_slug_candidates_for(&root, &cwd, git_remote_origin(&root), &project_slug())
}

/// The injectable core of [`legacy_slug_candidates`] (cwd is process-global — tests must not
/// `set_current_dir`, so they pass one in).
fn legacy_slug_candidates_for(
    root: &Path,
    cwd: &Path,
    remote_url: Option<String>,
    current: &str,
) -> Vec<String> {
    let mut pairs: Vec<(String, String)> = Vec::new(); // (dirname, hash key)
    let dirname = |p: &Path| {
        p.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string()
    };
    if let Some(url) = remote_url {
        pairs.push((dirname(root), url));
    }
    // The launch dirs the old keying could have rooted at: root, cwd, and everything between.
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut cur = Some(cwd);
    while let Some(p) = cur {
        if p == root {
            break;
        }
        if p.starts_with(root) {
            dirs.push(p.to_path_buf());
        }
        cur = p.parent();
    }
    for d in dirs {
        let n = dirname(&d);
        if let Ok(c) = std::fs::canonicalize(&d) {
            pairs.push((n.clone(), c.display().to_string())); // Windows: `\\?\C:\…` verbatim
        }
        pairs.push((n, d.display().to_string()));
    }
    let mut out: Vec<String> = Vec::new();
    for (n, k) in pairs {
        // Both spellings of the dirname fragment: today's folded one, and the pre-fold one an
        // accented checkout was keyed under before `slug_fragment` folded. For an ASCII dirname the
        // two are identical and the dedup below collapses them.
        for frag in [slug_fragment(&n), slug_fragment_prefold(&n)] {
            let s = format!("{frag}-{:08x}", fnv1a64(&k) as u32);
            if s != current && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// `git config --get remote.origin.url` in `root` — `None` when not a repo / no remote / no git.
/// No longer part of the identity key; kept for `zone migrate` (legacy-URL slugs) and `/where`.
pub fn git_remote_origin(root: &Path) -> Option<String> {
    let out = crate::core::gitx::command()
        .ok()?
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The project-local customization dir a cloned repo ships (skills / mcp.json / personas /
/// commands), merged OVER the home dir by each resolver: always `<root>/.aizen`. Like
/// [`resolve_default_home`], this carries no legacy-name fallback — one name, everywhere.
pub fn project_aizen_dir() -> PathBuf {
    project_root().join(".aizen")
}

/// Tighten a freshly-written secret-bearing file to owner-only (0600) on Unix; a no-op on Windows
/// (where the user-profile ACL governs). Best-effort — never fails the caller. Mirrors the 0600
/// discipline the OAuth/MCP token caches already use, applied to every long-lived secret store.
#[cfg(unix)]
pub fn harden_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
pub fn harden_file(_path: &Path) {}

/// Tighten a directory holding secrets to owner-only (0700) on Unix; a no-op on Windows.
#[cfg(unix)]
pub fn harden_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
pub fn harden_dir(_path: &Path) {}

/// Historical Core cache namespace, retained so codebase indexes keep their on-disk location.
pub fn cli_memory_dir() -> PathBuf {
    aizen_home().join("cli-memory")
}

/// Per-repo codebase index (`/init`). Layout: `cli-memory/codebase/<slug>.json`.
/// The slug is sanitized to a single safe path segment so a hostile repo name can't
/// escape the index dir.
pub fn codebase_index_path(slug: &str) -> PathBuf {
    cli_memory_dir()
        .join("codebase")
        .join(format!("{}.json", safe_core_slug(slug)))
}

/// Sanitize a project slug for use as a single path segment (no `/` `\` `..`).
fn safe_core_slug(slug: &str) -> String {
    let s: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "project".into()
    } else {
        s.chars().take(64).collect()
    }
}

/// Core codebase chunk embeddings (`code-*` namespace), keyed by content hash.
#[cfg(feature = "dense")]
pub fn embed_cache_dir() -> PathBuf {
    cli_memory_dir().join("embed-cache")
}

/// Local Core embedding model store. Shared `~/.aizen/models/` (NOT under cli-memory)
/// so explicit provisioning and model inventory work without a `dense` build.
pub fn models_dir() -> PathBuf {
    aizen_home().join("models")
}

/// The dense embedding model: a subdir name of `models_dir()`, or an absolute path to a model dir.
///
/// Precedence `AIZEN_EMBED_MODEL` → the saved `embed_model` in cli-config → the benched default.
/// The saved choice belongs to Core codebase retrieval, not the provider's Memory settings.
///
/// No recursion risk in reading cli-config from here: `cli_config::load` only reaches back into
/// `config_path()` → `aizen_home()`, neither of which consults this function.
pub fn embed_model_name() -> String {
    std::env::var("AIZEN_EMBED_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::core::cli_config::load()
                .embed_model
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        // potion-base-8M (English static, ~30MB) — the P6 CI bench (dense-bench.yml) measured it
        // LIFTING paraphrase recall@5 more than the ~18x-larger multilingual model (+0.231 vs
        // +0.154) on the bilingual En-Vi fixtures, so it's the default. `AIZEN_EMBED_MODEL` overrides.
        .unwrap_or_else(|| "potion-base-8M".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_fragment_normalizes_and_bounds() {
        assert_eq!(slug_fragment("My Project!"), "my-project");
        assert_eq!(slug_fragment("***"), "project");
        assert!(slug_fragment(&"a".repeat(100)).chars().count() <= 24);
    }

    #[test]
    fn normalize_path_key_is_spelling_invariant() {
        // Pure string transform — host-independent, so windows-style literals are safe here.
        // Every spelling of one directory must hash identically: the OLD key forked the zone on
        // exactly this (verbatim `\\?\` canonicalize vs the plain-path fallback).
        let want = "c:/Users/Admin/proj";
        assert_eq!(
            normalize_path_key(Path::new(r"\\?\C:\Users\Admin\proj")),
            want
        );
        assert_eq!(normalize_path_key(Path::new(r"C:\Users\Admin\proj")), want);
        assert_eq!(normalize_path_key(Path::new("C:/Users/Admin/proj/")), want);
        assert_eq!(
            normalize_path_key(Path::new("/home/u/proj/")),
            "/home/u/proj"
        );
        // Verbatim UNC and the plain spelling of the same share collapse to one key.
        assert_eq!(
            normalize_path_key(Path::new(r"\\?\UNC\srv\share\proj")),
            "//srv/share/proj"
        );
        assert_eq!(
            normalize_path_key(Path::new(r"\\srv\share\proj")),
            "//srv/share/proj"
        );
    }

    #[test]
    fn workspace_key_never_carries_the_verbatim_spelling() {
        let dir = std::env::temp_dir().join(format!("aizen-wskey-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let canon = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
        let key = workspace_key(&dir);
        // The real regression pin (Windows): canonicalize returns the `\\?\` verbatim form, and
        // exactly that spelling reaching the hash is what forked zones. The key must equal the
        // normalizer applied to the verbatim DISPLAY STRING — the old fork input — not just to
        // whatever canonicalize returns (that comparison would be a tautology).
        assert!(
            !key.starts_with("//?/"),
            "verbatim prefix must never reach the key: {key}"
        );
        assert_eq!(
            key,
            normalize_path_key(Path::new(&canon.display().to_string()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_candidates_cover_url_root_and_subdir_launch_eras() {
        let base = std::env::temp_dir().join(format!("aizen-legacy-{}", std::process::id()));
        let root = base.join("repo");
        let cwd = root.join("src").join("agent");
        let _ = std::fs::create_dir_all(&cwd);
        let current = slug_for_key("repo", &workspace_key(&root));
        let cands = legacy_slug_candidates_for(
            &root,
            &cwd,
            Some("https://example.com/o/repo.git".to_string()),
            &current,
        );
        assert!(
            !cands.contains(&current),
            "current slug is never a candidate"
        );
        // URL era: hashed remote under the ROOT's name.
        let url_slug = slug_for_key("repo", "https://example.com/o/repo.git");
        assert!(
            cands.contains(&url_slug),
            "URL-keyed era must be covered: {cands:?}"
        );
        // Gitless era, habitual SUBDIR launch: the old fallback rooted at cwd, so the zone lived
        // under the SUBDIR's name and raw path. That population must be findable.
        let subdir_raw = slug_for_key("agent", &cwd.display().to_string());
        assert!(
            cands.contains(&subdir_raw),
            "cwd-keyed gitless era must be covered: {cands:?}"
        );
        // …and the intermediate dir between cwd and root too.
        let mid_raw = slug_for_key("src", &root.join("src").display().to_string());
        assert!(
            cands.contains(&mid_raw),
            "intermediate launch dirs must be covered: {cands:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn project_slug_is_stable_and_follows_ng_project_root() {
        // AIZEN_PROJECT_ROOT is process-global env → serialize with every other home-mutating test.
        let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-slug-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("AIZEN_PROJECT_ROOT", &dir);
        let a = project_slug();
        let b = project_slug();
        assert_eq!(a, b, "same workspace → same slug (cached)");
        let hex = a.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 8, "hex8 suffix: {a}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        // repointing the root env gives a DIFFERENT zone (the cache can't serve stale)
        let dir2 = std::env::temp_dir().join(format!("aizen-slug2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir2);
        std::env::set_var("AIZEN_PROJECT_ROOT", &dir2);
        assert_ne!(project_slug(), a, "different root → different slug");
        std::env::remove_var("AIZEN_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
