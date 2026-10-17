//! Read-only managed output discovery, shared by `aizen files` and `/files`.
//! Planning roots never creates them; traversal reads names/metadata, never file contents.

use crate::core::file_placement::is_link_or_reparse;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 256;
const MAX_DEPTH: usize = 4;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_LABEL_BYTES: usize = 1024;
const TRUNCATION_RESERVE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum FilesKind {
    Artifacts,
    Scratch,
}

pub(crate) fn parse_kind(arg: &str) -> anyhow::Result<Option<FilesKind>> {
    match arg.trim() {
        "" => Ok(None),
        "artifacts" => Ok(Some(FilesKind::Artifacts)),
        "scratch" => Ok(Some(FilesKind::Scratch)),
        _ => {
            anyhow::bail!("usage: /files [artifacts|scratch] (or aizen files [artifacts|scratch])")
        }
    }
}

/// Both paths are pure plans. In particular, do not use the creating `scratch::dir()` here.
pub(crate) fn managed_roots(workspace: &Path, home: &Path) -> (PathBuf, PathBuf) {
    (
        crate::core::file_placement::artifact_workspace_root(workspace, home),
        crate::core::scratch::planned_dir().join("managed"),
    )
}

pub(crate) fn files_report(kind: Option<FilesKind>) -> String {
    let (artifacts, scratch) = managed_roots(
        &crate::core::config::project_root(),
        &crate::core::config::aizen_home(),
    );
    report_roots(kind, &artifacts, &scratch)
}

pub(crate) fn run_files(kind: Option<FilesKind>) -> anyhow::Result<()> {
    crate::ui::tui::emit_line(&files_report(kind));
    Ok(())
}

/// Quote bounded names and paths so terminal controls, line breaks and bidi controls cannot forge
/// rows. Lossy Unicode conversion is only for display; traversal always uses the original path.
pub(crate) fn quoted_path(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        let escaped = match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            c if c.is_control()
                || matches!(c, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') =>
            {
                c.escape_default().to_string()
            }
            c => c.to_string(),
        };
        if out.len() + escaped.len() + 4 > MAX_LABEL_BYTES {
            out.push_str("...");
            break;
        }
        out.push_str(&escaped);
    }
    out.push('"');
    out
}

/// Check ancestors as well as the requested directory: a normal-looking root beneath a junction
/// must not be traversed. This is a read-only path check, not kernel-atomic hostile-race isolation.
pub(crate) fn directory_status(path: &Path) -> Result<(), &'static str> {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(md) if is_link_or_reparse(&md) => return Err("link/reparse directory refused"),
            Ok(md) if !md.is_dir() => return Err("not a directory"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Continue checking the parents, even for a missing root.
            }
            Err(_) => return Err("unavailable (metadata read failed)"),
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("not created yet"),
        Err(_) => Err("unavailable (metadata read failed)"),
    }
}

struct Listing {
    out: String,
    visited: usize,
    entries_cut: bool,
    depth_cut: bool,
    bytes_cut: bool,
}

impl Listing {
    fn line(&mut self, line: &str) -> bool {
        if self.out.len() + line.len() + 1 > MAX_OUTPUT_BYTES - TRUNCATION_RESERVE {
            self.bytes_cut = true;
            return false;
        }
        self.out.push_str(line);
        self.out.push('\n');
        true
    }

    fn root(&mut self, label: &str, root: &Path) {
        if !self.line(&format!("{label}: {}", quoted_path(root))) {
            return;
        }
        if let Err(status) = directory_status(root) {
            self.line(&format!("  ({status})"));
            return;
        }
        let before = self.out.len();
        self.walk(root, root, 0);
        if self.out.len() == before && !self.entries_cut && !self.bytes_cut {
            self.line("  (empty)");
        }
    }

    fn walk(&mut self, root: &Path, dir: &Path, depth: usize) {
        if self.bytes_cut {
            return;
        }
        if self.visited >= MAX_ENTRIES {
            self.entries_cut = true;
            return;
        }
        if let Err(status) = directory_status(dir) {
            self.line(&format!(
                "  {} ({status})",
                quoted_path(dir.strip_prefix(root).unwrap_or(dir))
            ));
            return;
        }
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => {
                self.line("  (unavailable: directory read failed)");
                return;
            }
        };
        // Stream entries rather than collecting a whole directory. The budget is global across
        // roots and recursive calls; directory order is deliberately not promised.
        for entry in rd {
            if self.bytes_cut {
                break;
            }
            if self.visited >= MAX_ENTRIES {
                self.entries_cut = true;
                break;
            }
            self.visited += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    self.line("  (unavailable: entry read failed)");
                    continue;
                }
            };
            let path = entry.path();
            let name = quoted_path(path.strip_prefix(root).unwrap_or(&path));
            let md = match std::fs::symlink_metadata(&path) {
                Ok(md) => md,
                Err(_) => {
                    self.line(&format!("  {name} (unavailable: metadata read failed)"));
                    continue;
                }
            };
            if is_link_or_reparse(&md) {
                self.line(&format!("  {name} (link/reparse skipped)"));
            } else if md.is_dir() {
                if !self.line(&format!("  {name}/")) {
                    break;
                }
                if depth + 1 < MAX_DEPTH {
                    self.walk(root, &path, depth + 1);
                } else {
                    self.depth_cut = true;
                }
            } else if md.is_file() {
                self.line(&format!("  {name}  {} B", md.len()));
            } else {
                self.line(&format!("  {name} (special file skipped)"));
            }
        }
    }
}

fn report_roots(kind: Option<FilesKind>, artifacts: &Path, scratch: &Path) -> String {
    let mut listing = Listing {
        out: String::new(),
        visited: 0,
        entries_cut: false,
        depth_cut: false,
        bytes_cut: false,
    };
    listing.line("Managed files (read-only; names/sizes only; no link traversal)");
    if kind != Some(FilesKind::Scratch) {
        listing.root("artifacts (this workspace, all runs)", artifacts);
    }
    if kind != Some(FilesKind::Artifacts) {
        listing.root("scratch (this process, all managed scopes)", scratch);
    }
    if listing.entries_cut || listing.depth_cut || listing.bytes_cut {
        let _ = writeln!(
            listing.out,
            "(truncated: entries={} depth={} output={}; limits: {MAX_ENTRIES} entries, depth {MAX_DEPTH}, 32 KiB)",
            listing.entries_cut, listing.depth_cut, listing.bytes_cut
        );
    }
    listing.out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    struct Fixture(PathBuf);

    impl Fixture {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aizen-files-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Fixture(std::fs::canonicalize(path).unwrap())
    }

    #[test]
    fn root_planning_is_stable_and_does_not_create_managed_directories() {
        let temp = fixture();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let home = temp.path().join("absent-home");
        let (artifacts, scratch) = managed_roots(&workspace, &home);
        assert_eq!(
            managed_roots(&workspace, &home),
            (artifacts.clone(), scratch.clone())
        );
        assert_eq!(artifacts.parent(), Some(home.join("artifacts").as_path()));
        assert_eq!(scratch, crate::core::scratch::planned_dir().join("managed"));
        assert!(!home.exists());
        assert_eq!(directory_status(&artifacts), Err("not created yet"));
        assert!(!home.exists());
    }

    #[test]
    fn missing_roots_remain_absent_and_filters_match() {
        let temp = fixture();
        let artifacts = temp.path().join("not-created/artifacts");
        let scratch = temp.path().join("not-created/scratch");
        let both = report_roots(None, &artifacts, &scratch);
        assert_eq!(both.matches("not created yet").count(), 2, "{both}");
        assert!(!temp.path().join("not-created").exists());
        let artifacts_only = report_roots(Some(FilesKind::Artifacts), &artifacts, &scratch);
        assert!(artifacts_only.contains("artifacts ("));
        assert!(!artifacts_only.contains("scratch ("));
    }

    #[test]
    fn lists_artifacts_across_runs_without_reading_contents() {
        let temp = fixture();
        let artifacts = temp.path().join("artifacts");
        for run in ["run-a", "run-b"] {
            let scope = artifacts.join(run).join("child");
            std::fs::create_dir_all(&scope).unwrap();
            std::fs::write(scope.join("report.txt"), "NEVER_PRINT_SECRET_CONTENT").unwrap();
        }
        let out = report_roots(
            Some(FilesKind::Artifacts),
            &artifacts,
            &temp.path().join("scratch"),
        );
        assert!(out.contains("run-a") && out.contains("run-b"), "{out}");
        assert_eq!(out.matches("report.txt").count(), 2);
        assert!(!out.contains("NEVER_PRINT_SECRET_CONTENT"));
        assert!(!temp.path().join("scratch").exists());
    }

    #[test]
    fn default_managed_scope_leaves_are_reachable_at_depth_four() {
        let temp = fixture();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let home = temp.path().join("home");
        let process = temp.path().join("process");
        let ctx = crate::core::file_placement::PlacementContext::with_paths(
            workspace.clone(),
            home.clone(),
            process.clone(),
            "isolated-run",
        )
        .unwrap();
        std::fs::create_dir_all(ctx.artifact_root()).unwrap();
        std::fs::create_dir_all(ctx.scratch_root()).unwrap();
        std::fs::write(ctx.artifact_root().join("artifact-leaf.txt"), "a").unwrap();
        std::fs::write(ctx.scratch_root().join("scratch-leaf.txt"), "s").unwrap();
        let artifacts = crate::core::file_placement::artifact_workspace_root(&workspace, &home);
        let out = report_roots(None, &artifacts, &process.join("managed"));
        assert!(out.contains("artifact-leaf.txt"), "{out}");
        assert!(out.contains("scratch-leaf.txt"), "{out}");
        assert!(
            !out.contains("truncated:"),
            "default scope leaves must be within depth cap: {out}"
        );
    }

    #[test]
    fn traversal_and_output_are_bounded() {
        let temp = fixture();
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        for n in 0..300 {
            std::fs::write(artifacts.join(format!("{n:03}.txt")), "x").unwrap();
        }
        let out = report_roots(Some(FilesKind::Artifacts), &artifacts, temp.path());
        assert_eq!(out.matches(" 1 B").count(), MAX_ENTRIES);
        assert!(out.contains("entries=true"), "{out}");
        assert!(out.len() <= MAX_OUTPUT_BYTES);

        let deep = temp.path().join("deep");
        std::fs::create_dir_all(deep.join("a/b/c/d/e")).unwrap();
        std::fs::write(deep.join("a/b/c/d/e/hidden.txt"), "x").unwrap();
        let out = report_roots(Some(FilesKind::Artifacts), &deep, temp.path());
        assert!(out.contains("depth=true"), "{out}");
        assert!(!out.contains("hidden.txt"));
    }

    #[test]
    fn output_cap_and_control_escaping_are_explicit() {
        let mut listing = Listing {
            out: String::new(),
            visited: 0,
            entries_cut: false,
            depth_cut: false,
            bytes_cut: false,
        };
        for _ in 0..100 {
            listing.line(&"x".repeat(1024));
        }
        assert!(listing.bytes_cut);
        assert!(listing.out.len() <= MAX_OUTPUT_BYTES - TRUNCATION_RESERVE);
        let escaped = quoted_path(Path::new("line\n\u{1b}[2J\"\u{202e}bad"));
        assert!(!escaped.contains('\n') && !escaped.contains('\u{1b}'));
        assert!(escaped.contains("\\n") && escaped.contains("\\u{202e}"));
        assert!(quoted_path(Path::new(&"界".repeat(1000))).len() <= MAX_LABEL_BYTES);
        let temp = fixture();
        for n in 0..256 {
            std::fs::write(
                temp.path().join(format!("{n:03}-{}.txt", "x".repeat(140))),
                "x",
            )
            .unwrap();
        }
        let out = report_roots(Some(FilesKind::Artifacts), temp.path(), temp.path());
        assert!(
            out.contains("output=true"),
            "output cap must announce truncation"
        );
        assert!(out.len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn cli_and_slash_share_filter_vocabulary_and_discovery() {
        use crate::cli_args::{Cli, Commands};
        use crate::features::slash::{self, SlashId, Verdict};
        for arg in ["", "artifacts", "scratch"] {
            let mut argv = vec!["aizen", "files"];
            if !arg.is_empty() {
                argv.push(arg);
            }
            let cli = Cli::try_parse_from(argv).unwrap();
            let Some(Commands::Files { kind }) = cli.command else {
                panic!("wrong CLI dispatch");
            };
            assert_eq!(kind, parse_kind(arg).unwrap());
            let input = if arg.is_empty() {
                "/files".to_string()
            } else {
                format!("/files {arg}")
            };
            assert_eq!(
                slash::classify(&input),
                Verdict::Command {
                    name: "files".into(),
                    arg: arg.into()
                }
            );
            assert!(!slash::takes_stdin(input.trim_start_matches('/')));
        }
        assert_eq!(slash::resolve("files").unwrap().id, SlashId::Files);
        assert!(slash::help_page().contains("/files [artifacts|scratch]"));
        assert!(Cli::try_parse_from(["aizen", "files", "delete"]).is_err());
        assert!(parse_kind("delete").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_roots_and_descendants_are_not_followed() {
        use std::os::windows::fs::symlink_dir;
        let temp = fixture();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(outside.join("child")).unwrap();
        std::fs::write(outside.join("child/ESCAPED.txt"), "secret").unwrap();
        let linked = temp.path().join("linked");
        match symlink_dir(&outside, &linked) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.raw_os_error() == Some(1314) =>
            {
                eprintln!(
                    "skipping Windows symlink fixture: OS did not grant symlink creation ({e})"
                );
                return;
            }
            Err(e) => panic!("cannot create isolated symlink fixture: {e}"),
        }
        let child = linked.join("child");
        for root in [&linked, &child] {
            let out = report_roots(Some(FilesKind::Artifacts), root, temp.path());
            assert!(out.contains("link/reparse directory refused"), "{out}");
            assert!(!out.contains("ESCAPED.txt"));
        }
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        symlink_dir(outside.join("child"), artifacts.join("escape")).unwrap();
        let out = report_roots(Some(FilesKind::Artifacts), &artifacts, temp.path());
        assert!(out.contains("link/reparse skipped"), "{out}");
        assert!(!out.contains("ESCAPED.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn links_in_roots_ancestors_and_descendants_are_not_followed() {
        use std::os::unix::fs::symlink;
        let temp = fixture();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(outside.join("child")).unwrap();
        std::fs::write(outside.join("child/ESCAPED.txt"), "secret").unwrap();
        let linked = temp.path().join("linked");
        symlink(&outside, &linked).unwrap();
        for root in [&linked, &linked.join("child")] {
            let out = report_roots(Some(FilesKind::Artifacts), root, temp.path());
            assert!(out.contains("link/reparse directory refused"), "{out}");
            assert!(!out.contains("ESCAPED.txt"));
        }
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        symlink(outside.join("child"), artifacts.join("escape")).unwrap();
        let out = report_roots(Some(FilesKind::Artifacts), &artifacts, temp.path());
        assert!(out.contains("link/reparse skipped"), "{out}");
        assert!(!out.contains("ESCAPED.txt"));
    }
}
