//! Project-convention loading. Walk from the working directory up to the repo root, collect any
//! `AGENTS.md` / `CLAUDE.md` files, and merge them into the top-level system prompt so the agent
//! inherits the codebase's build/test commands, layout, and house rules without the user repeating
//! them every turn. `AGENTS.md` is the 2025 cross-tool standard; `CLAUDE.md` is read for ecosystem
//! compatibility. Read-only and fail-soft: an unreadable file is skipped, and `None` (no block at
//! all) is returned when nothing is found — preserving the byte-stable prompt prefix for projects
//! that ship no conventions file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Max chars of merged project context injected into the prompt — generous but bounded so a giant
/// committed doc can't blow the context budget (and keeps the cached prefix a sane size).
const MAX_CONTEXT_CHARS: usize = 12_000;

/// Hard cap on directories climbed, as insurance against a pathological tree / missing repo root.
const MAX_CLIMB: usize = 40;

/// Convention filenames, in the order they are emitted WITHIN a directory. Both are read when both
/// exist: the first-found-wins rule let a one-paragraph `AGENTS.md` pointer hide a full
/// `CLAUDE.md` (quality plan M10 — this repo's own 7 KB file was never in the prompt).
const CONVENTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// `(mtime, len)` of a convention file; `None` when it does not exist.
type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

/// What the last load from each working directory read or probed, so a mid-conversation edit can
/// be noticed by stats alone (quality plan M10: the conventions were never re-read inside a
/// conversation). Keyed by the canonical cwd, so two sandboxes — or two hostbot lanes — never
/// see each other's record.
/// What one load probed: every candidate path and the stamp it had (`None` = absent).
type Probed = Vec<(PathBuf, Option<Stamp>)>;

fn probed_map() -> &'static Mutex<HashMap<PathBuf, Probed>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, Probed>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Has any convention file on the path from `cwd` to the repo root appeared, changed or vanished
/// since the last [`load_project_context`] from there? Stats only — reads nothing. `false` when
/// nothing has been loaded from `cwd` yet: nothing adopted, nothing to refresh.
pub fn conventions_changed(cwd: &Path) -> bool {
    let start = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let Ok(map) = probed_map().lock() else {
        return false;
    };
    let Some(probed) = map.get(&start) else {
        return false;
    };
    probed.iter().any(|(path, seen)| stamp(path) != *seen)
}

/// Load merged project conventions for `cwd`, or `None` if none exist on the path to the repo root.
///
/// Walks from `cwd` up to (and including) the first ancestor containing `.git` (or the filesystem
/// root), then emits sections farthest→nearest so the NEAREST (most specific) file's guidance lands
/// LAST and therefore wins. Each section is headed by its path relative to the repo root. Never
/// errors. The total is capped at [`MAX_CONTEXT_CHARS`].
pub fn load_project_context(cwd: &Path) -> Option<String> {
    let start = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    // Collect dirs nearest→farthest, stopping at the repo root (a dir containing `.git`).
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur: &Path = &start;
    loop {
        dirs.push(cur.to_path_buf());
        if dirs.len() >= MAX_CLIMB || cur.join(".git").exists() {
            break; // repo root (or the safety cap) — stop climbing
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break, // filesystem root
        }
    }
    // Emit farthest→nearest so the nearest file appears last (highest priority). The repo root is
    // also the label base, so sections read `# AGENTS.md`, `# crate/sub/AGENTS.md`, …
    dirs.reverse();
    let label_base = dirs.first().cloned().unwrap_or_else(|| start.clone());

    let mut sections: Vec<String> = Vec::new();
    let mut probed: Probed = Vec::new();
    for dir in &dirs {
        for name in CONVENTION_FILES {
            let path = dir.join(name);
            probed.push((path.clone(), stamp(&path)));
            if let Ok(body) = std::fs::read_to_string(&path) {
                let body = body.trim();
                if !body.is_empty() {
                    sections.push(format!("# {}\n{}", display_label(&path, &label_base), body));
                }
            }
        }
    }
    if let Ok(mut map) = probed_map().lock() {
        map.insert(start.clone(), probed);
    }
    if sections.is_empty() {
        return None;
    }

    let merged = sections.join("\n\n");
    if merged.chars().count() > MAX_CONTEXT_CHARS {
        let kept: String = merged.chars().take(MAX_CONTEXT_CHARS).collect();
        return Some(format!(
            "{kept}\n…[project context truncated at {MAX_CONTEXT_CHARS} chars]…"
        ));
    }
    Some(merged)
}

/// A readable label for a convention file: its path relative to the repo root, else the file name.
fn display_label(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .ok()
        .map(|p| p.display().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hermetic temp tree with a `.git` marker at its root so the walk stops there (never climbs
    /// into the real filesystem).
    fn sandbox(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("aizen-projctx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        root
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn both_convention_files_in_a_directory_are_honoured_agents_first() {
        let root = sandbox("both");
        write(&root, "AGENTS.md", "POINTER: see CLAUDE.md");
        write(&root, "CLAUDE.md", "THE_FULL_RULES");
        let ctx = load_project_context(&root).unwrap();
        assert!(
            ctx.contains("# AGENTS.md") && ctx.contains("POINTER"),
            "{ctx}"
        );
        assert!(
            ctx.contains("# CLAUDE.md") && ctx.contains("THE_FULL_RULES"),
            "{ctx}"
        );
        assert!(
            ctx.find("# AGENTS.md") < ctx.find("# CLAUDE.md"),
            "AGENTS.md first, CLAUDE.md after: {ctx}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_convention_edit_is_noticed_by_stats_until_the_next_load() {
        let root = sandbox("changed");
        write(&root, "AGENTS.md", "v1");
        assert!(
            !conventions_changed(&root),
            "nothing loaded yet, nothing to refresh"
        );
        let _ = load_project_context(&root);
        assert!(!conventions_changed(&root), "just loaded");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&root, "AGENTS.md", "v2 — longer");
        assert!(conventions_changed(&root), "an edit is a change");
        let _ = load_project_context(&root);
        assert!(!conventions_changed(&root), "the reload adopts it");
        write(&root, "CLAUDE.md", "appeared");
        assert!(
            conventions_changed(&root),
            "a file that appears is a change"
        );
        let _ = load_project_context(&root);
        std::fs::remove_file(root.join("CLAUDE.md")).unwrap();
        assert!(
            conventions_changed(&root),
            "a file that vanishes is a change"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn none_when_absent() {
        let root = sandbox("absent");
        assert!(load_project_context(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loads_root_agents_md() {
        let root = sandbox("root");
        write(&root, "AGENTS.md", "Build with cargo. UNIQUE_ROOT_FACT.");
        let ctx = load_project_context(&root).expect("should load");
        assert!(ctx.contains("UNIQUE_ROOT_FACT"));
        assert!(ctx.contains("# AGENTS.md"), "section headed by path: {ctx}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nearest_wins_appears_last() {
        let root = sandbox("nearest");
        write(&root, "AGENTS.md", "ROOT_RULES");
        write(&root, "crate/sub/AGENTS.md", "SUB_RULES");
        let cwd = root.join("crate/sub");
        let ctx = load_project_context(&cwd).expect("should load");
        let root_at = ctx.find("ROOT_RULES").unwrap();
        let sub_at = ctx.find("SUB_RULES").unwrap();
        assert!(
            root_at < sub_at,
            "nearest (sub) must come last so it wins: {ctx}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_md_alone_is_read() {
        // Both files together are covered by `both_convention_files_in_a_directory_are_honoured_agents_first`
        // (E4.6 ended the first-found-wins rule); this pins that CLAUDE.md on its own is read.
        let root2 = sandbox("compat2");
        write(&root2, "CLAUDE.md", "ONLY_CLAUDE");
        let ctx2 = load_project_context(&root2).expect("should load");
        assert!(
            ctx2.contains("ONLY_CLAUDE"),
            "CLAUDE.md alone is read: {ctx2}"
        );
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn caps_oversized_context() {
        let root = sandbox("cap");
        write(&root, "AGENTS.md", &"x".repeat(MAX_CONTEXT_CHARS + 5_000));
        let ctx = load_project_context(&root).expect("should load");
        assert!(
            ctx.chars().count() <= MAX_CONTEXT_CHARS + 80,
            "capped near MAX: {}",
            ctx.chars().count()
        );
        assert!(ctx.contains("truncated"), "marks truncation");
        let _ = std::fs::remove_dir_all(&root);
    }
}
