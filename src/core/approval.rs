//! Unified approval policy shared by the interactive REPL, one-shot runners, sub-agents, and bots.
//!
//! The three levels form one ordered capability ladder:
//! - `ask`: prompt before destructive tools;
//! - `smart`: auto-run shell commands the hard guard classifies as read-only, prompt for the rest;
//! - `yolo`: pre-authorize destructive tools after the non-overridable hard safety floor.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Ask,
    Smart,
    Yolo,
}

impl ApprovalMode {
    /// Whether a shell command classified as read-only may skip the approval prompt.
    pub fn approves_readonly_shell(self) -> bool {
        matches!(self, Self::Smart | Self::Yolo)
    }

    /// Whether every destructive tool may skip the approval prompt (the hard floor still runs first).
    pub fn approves_all(self) -> bool {
        self == Self::Yolo
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Smart => "smart",
            Self::Yolo => "yolo",
        }
    }
}

/// One standing approval: `tool` may run without asking — everywhere, or only when the call's
/// target directory lies under `under`. Session grants come from the approval menu's "always"
/// rows and die with the window (`/clear` too); project grants come from
/// `<root>/.aizen/approvals.json` and are read on every check, so an edit takes effect on the next
/// call. The hard `cmd_guard` floor still runs before any grant is consulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under: Option<PathBuf>,
}

impl Grant {
    /// Does this grant cover a call of `tool` whose write lands in `target` (an absolute directory,
    /// or `None` when the tool names no path)? A tool-wide grant covers every call; a
    /// directory-scoped one needs a target under its directory — a call with no target is NOT
    /// under any directory.
    pub fn covers(&self, tool: &str, target: Option<&Path>) -> bool {
        if self.tool != tool {
            return false;
        }
        match &self.under {
            None => true,
            Some(dir) => target.is_some_and(|t| canon_or_self(t).starts_with(canon_or_self(dir))),
        }
    }

    /// `shell_run` / `file_edit under src/parser`, for status lines and traces.
    pub fn describe(&self) -> String {
        match &self.under {
            None => self.tool.clone(),
            Some(d) => format!("{} under {}", self.tool, d.display()),
        }
    }
}

/// The canonical form of the deepest EXISTING ancestor with the rest re-appended, so both sides of
/// a prefix test carry the same spelling (Windows `\\?\` prefix, 8.3 short names resolved). A
/// grant for a directory must cover a target the tool is about to CREATE under it, and a plain
/// `canonicalize` fails on a path that does not exist yet — which is exactly the target of a
/// `file_write` or a `file_move` into a new folder.
fn canon_or_self(p: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut out = canon;
            for seg in rest.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        match (
            cur.file_name().map(|s| s.to_os_string()),
            cur.parent().map(Path::to_path_buf),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

static SESSION_GRANTS: Mutex<Vec<Grant>> = Mutex::new(Vec::new());

/// Remember a grant for the rest of this session (the menu's `always for <tool>` rows).
pub fn grant_session(tool: &str, under: Option<PathBuf>) {
    let g = Grant {
        tool: tool.to_string(),
        under: under.map(|d| canon_or_self(&d)),
    };
    let mut v = SESSION_GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    if !v.contains(&g) {
        v.push(g);
    }
}

pub fn session_grants() -> Vec<Grant> {
    SESSION_GRANTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Forget every session grant (called on `/clear`, next to the session allow-all).
pub fn reset_session_grants() {
    SESSION_GRANTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// The file name of the project allowlist under `.aizen/`.
pub const PROJECT_ALLOWLIST: &str = "approvals.json";

/// The project allowlist: `<root>/.aizen/approvals.json`, shaped
/// `{"allow": [{"tool": "shell_run", "under": "scripts"}, {"tool": "file_edit"}]}` with `under`
/// relative to the project root (or absolute). Missing or unreadable ⇒ no grants; a malformed
/// file is reported once per check on stderr rather than silently ignored.
pub fn project_grants() -> Vec<Grant> {
    let root = crate::core::config::project_root();
    let path = crate::core::config::project_aizen_dir().join(PROJECT_ALLOWLIST);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match parse_allowlist(&text, &root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}: {e} — ignored", path.display());
            Vec::new()
        }
    }
}

/// Parse an allowlist document; relative `under` paths are anchored at `root`.
pub fn parse_allowlist(text: &str, root: &Path) -> Result<Vec<Grant>, String> {
    #[derive(Deserialize)]
    struct Doc {
        #[serde(default)]
        allow: Vec<Grant>,
    }
    let doc: Doc =
        serde_json::from_str(text).map_err(|e| format!("invalid approvals.json: {e}"))?;
    let mut out = Vec::with_capacity(doc.allow.len());
    for mut g in doc.allow {
        let tool = g.tool.trim();
        if tool.is_empty() {
            return Err("an allow entry has an empty tool name".to_string());
        }
        g.tool = tool.to_string();
        if let Some(d) = g.under.take() {
            g.under = Some(if d.is_absolute() { d } else { root.join(d) });
        }
        out.push(g);
    }
    Ok(out)
}

/// The grant that lets a call of `tool` at `target` run without asking, session grants first.
pub fn granted(tool: &str, target: Option<&Path>) -> Option<Grant> {
    session_grants()
        .into_iter()
        .chain(project_grants())
        .find(|g| g.covers(tool, target))
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApprovalMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "manual" | "prompt" => Ok(Self::Ask),
            "smart" => Ok(Self::Smart),
            "yolo" | "auto" | "yes" => Ok(Self::Yolo),
            other => Err(format!(
                "unknown approval mode '{other}' — use ask, smart, or yolo"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_cover_by_tool_and_by_directory() {
        let root = std::env::temp_dir().join(format!("aizen-grants-{}", std::process::id()));
        let src = root.join("src");
        let scripts = root.join("scripts");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&scripts).unwrap();
        let wide = Grant {
            tool: "shell_run".into(),
            under: None,
        };
        assert!(wide.covers("shell_run", None));
        assert!(wide.covers("shell_run", Some(&scripts)));
        assert!(!wide.covers("file_edit", Some(&scripts)));
        let scoped = Grant {
            tool: "file_edit".into(),
            under: Some(src.clone()),
        };
        assert!(scoped.covers("file_edit", Some(&src)));
        assert!(scoped.covers("file_edit", Some(&src.join("agent"))));
        assert!(!scoped.covers("file_edit", Some(&scripts)));
        assert!(
            !scoped.covers("file_edit", None),
            "no target is not under any directory"
        );
        assert_eq!(wide.describe(), "shell_run");
        assert!(scoped.describe().starts_with("file_edit under "));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_project_allowlist_parses_and_anchors_relative_dirs() {
        let root = Path::new("/proj");
        let v = parse_allowlist(
            r#"{"allow": [{"tool": "shell_run", "under": "scripts"}, {"tool": " file_edit "}]}"#,
            root,
        )
        .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].under.as_deref(), Some(root.join("scripts").as_path()));
        assert_eq!(v[1].tool, "file_edit");
        assert!(v[1].under.is_none());
        assert!(parse_allowlist("{}", root).unwrap().is_empty());
        assert!(parse_allowlist("not json", root).is_err());
        assert!(parse_allowlist(r#"{"allow": [{"tool": ""}]}"#, root).is_err());
    }

    #[test]
    fn session_grants_are_remembered_deduplicated_and_reset() {
        let tool = format!("test-tool-{}", std::process::id());
        assert!(granted(&tool, None).is_none());
        grant_session(&tool, None);
        grant_session(&tool, None);
        assert_eq!(
            session_grants().iter().filter(|g| g.tool == tool).count(),
            1
        );
        assert_eq!(granted(&tool, None).map(|g| g.tool), Some(tool.clone()));
        reset_session_grants();
        assert!(granted(&tool, None).is_none());
    }

    #[test]
    fn parses_aliases_and_displays_canonical_names() {
        assert_eq!("ask".parse(), Ok(ApprovalMode::Ask));
        assert_eq!("manual".parse(), Ok(ApprovalMode::Ask));
        assert_eq!("smart".parse(), Ok(ApprovalMode::Smart));
        assert_eq!("yes".parse(), Ok(ApprovalMode::Yolo));
        assert_eq!(ApprovalMode::Yolo.to_string(), "yolo");
        assert!("unsafe".parse::<ApprovalMode>().is_err());
    }

    #[test]
    fn capability_ladder_is_monotonic() {
        assert!(!ApprovalMode::Ask.approves_readonly_shell());
        assert!(!ApprovalMode::Ask.approves_all());
        assert!(ApprovalMode::Smart.approves_readonly_shell());
        assert!(!ApprovalMode::Smart.approves_all());
        assert!(ApprovalMode::Yolo.approves_readonly_shell());
        assert!(ApprovalMode::Yolo.approves_all());
    }

    #[test]
    fn serde_uses_lowercase_strings() {
        assert_eq!(
            serde_json::to_string(&ApprovalMode::Smart).unwrap(),
            "\"smart\""
        );
        assert_eq!(
            serde_json::from_str::<ApprovalMode>("\"yolo\"").unwrap(),
            ApprovalMode::Yolo
        );
    }
}
