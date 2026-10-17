//! Hermes-style **tool bundles** (`platform_toolsets.cli`): group built-in tools by capability so
//! users can shrink the model's tool schema (`disabled_toolsets` / `enabled_toolsets` in
//! `cli-config.json`) without forking the binary. MCP tools map to the `mcp` bundle; everything
//! else is classified by tool name prefix / explicit map.
//!
//! Sub-agent registries (`role_registry`, `agent_registry`) are **not** filtered — only the
//! top-level surface that pays the per-turn schema cost.

use crate::agent::tools::ToolRegistry;
use crate::core::cli_config::{self, CliConfig};

/// One Hermes-like bundle (see `platform_toolsets.cli` in Hermes `config.yaml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolsetInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub blurb: &'static str,
}

/// Catalog order matches Hermes CLI bundles where aizen has an analogue (+ `mcp`, `lsp`).
pub const CATALOG: &[ToolsetInfo] = &[
    ToolsetInfo {
        id: "memory",
        label: "memory",
        blurb: "list/recall facts, profile, Q&A, save/edit/forget",
    },
    ToolsetInfo {
        id: "file",
        label: "file",
        blurb: "read, glob, search, edit, write, move",
    },
    ToolsetInfo {
        id: "terminal",
        label: "terminal",
        blurb: "shell_run + background process pool",
    },
    ToolsetInfo {
        id: "web",
        label: "web",
        blurb: "search, fetch, crawl (Reach backends)",
    },
    ToolsetInfo {
        id: "skills",
        label: "skills",
        blurb: "load/save/refine + marketplace search/install",
    },
    ToolsetInfo {
        id: "delegation",
        label: "delegation",
        blurb: "task sub-agents + workflow fan-out",
    },
    ToolsetInfo {
        id: "todo",
        label: "todo",
        blurb: "in-session checklist (todo_write)",
    },
    ToolsetInfo {
        id: "clarify",
        label: "clarify",
        blurb: "ask one question and yield the turn",
    },
    ToolsetInfo {
        id: "checkpoint",
        label: "checkpoint",
        blurb: "git time-machine restore points",
    },
    ToolsetInfo {
        id: "messaging",
        label: "messaging",
        blurb: "telegram + outbound notify (when configured)",
    },
    ToolsetInfo {
        id: "persona",
        label: "persona",
        blurb: "mint/switch character (persona_create)",
    },
    ToolsetInfo {
        id: "mcp",
        label: "mcp",
        blurb: "all MCP app tools (mcp_<server>_<tool>)",
    },
    ToolsetInfo {
        id: "lsp",
        label: "lsp",
        blurb: "references, definition, read_symbol, hover, symbols, diagnostics, symbol_replace/insert, repo_map",
    },
    ToolsetInfo {
        id: "browser",
        label: "browser",
        blurb: "lazy attach-only CDP automation and screenshots",
    },
];

/// Map a registered tool name → bundle id. `None` = unknown (left visible when filtering).
///
/// A VIEW over [`crate::agent::tool_routing::lane_for`], which is the single name→capability table.
/// It used to be a second hand-maintained match, and the prompt's tool catalog was a third; keeping
/// one table is what stops "this tool is in the `lsp` bundle" and "this tool is an editing tool" from
/// disagreeing.
pub fn classify_tool(name: &str) -> Option<&'static str> {
    crate::agent::tool_routing::lane_for(name).map(crate::agent::tool_routing::Lane::toolset)
}

fn list_match(hay: &str, list: &[String]) -> bool {
    list.iter().any(|t| t.eq_ignore_ascii_case(hay))
}

/// Is bundle `id` allowed under `cfg`? Empty `enabled_toolsets` ⇒ no whitelist; empty
/// `disabled_toolsets` ⇒ nothing extra disabled.
pub fn toolset_allowed(id: &str, cfg: &CliConfig) -> bool {
    if let Some(ref en) = cfg.enabled_toolsets {
        if !en.is_empty() && !list_match(id, en) {
            return false;
        }
    }
    if let Some(ref dis) = cfg.disabled_toolsets {
        if list_match(id, dis) {
            return false;
        }
    }
    true
}

/// Drop tools whose bundle is disabled. Unknown tools are kept (forward-compatible).
pub fn apply_toolset_filter(registry: &mut ToolRegistry) {
    apply_toolset_filter_in(registry, &cli_config::load());
}

fn apply_toolset_filter_in(registry: &mut ToolRegistry, cfg: &CliConfig) {
    registry.retain(|name| {
        classify_tool(name)
            .map(|ts| toolset_allowed(ts, cfg))
            .unwrap_or(true)
    });
    // Discovery is a registry capability, not an MCP-only capability. Rebuild its entries after
    // filtering so disabled MCP tools cannot leak, while enabled deferred built-ins stay reachable.
    crate::agent::builtin::install_tool_search(registry);
}

/// Human summary for `/tools` and `config show`.
///
/// Both surfaces now render the registry themselves (they need per-toolset controls, not a flat
/// string). Kept as the one-call textual summary.
#[allow(dead_code)]
pub fn format_status(registry: &ToolRegistry) -> String {
    let cfg = cli_config::load();
    let names = registry.names();
    let mut by_ts: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut other: Vec<String> = Vec::new();
    for n in &names {
        match classify_tool(n) {
            Some(ts) => by_ts.entry(ts).or_default().push(n.clone()),
            None => other.push(n.clone()),
        }
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{} tool(s) advertised this session\n",
        names.len()
    ));
    for info in CATALOG {
        let allowed = toolset_allowed(info.id, &cfg);
        let count = by_ts.get(info.id).map(|v| v.len()).unwrap_or(0);
        let mark = if allowed {
            if count > 0 {
                "●"
            } else {
                "○"
            }
        } else {
            "✗"
        };
        let state = if allowed {
            if count > 0 {
                format!("{count} tool(s)")
            } else {
                "none registered (config off or not available)".to_string()
            }
        } else {
            "disabled in config".to_string()
        };
        out.push_str(&format!(
            "  {mark} {} — {}  [{state}]\n",
            info.id, info.blurb
        ));
    }
    if !other.is_empty() {
        out.push_str(&format!("  ? unclassified: {}\n", other.join(", ")));
    }
    if let Some(ref dis) = cfg.disabled_toolsets {
        if !dis.is_empty() {
            out.push_str(&format!("\ndisabled_toolsets: {}\n", dis.join(", ")));
        }
    }
    if let Some(ref en) = cfg.enabled_toolsets {
        if !en.is_empty() {
            out.push_str(&format!(
                "enabled_toolsets (whitelist): {}\n",
                en.join(", ")
            ));
        }
    }
    out.push_str(
        "\nconfig: `aizen config set --disabled-toolsets web,browser` or /tools in the REPL\n",
    );
    out.push_str("apps: connect more tools via /apps (MCP) — bundle `mcp`\n");
    out.trim_end().to_string()
}

/// Config-only status (no registry build — safe for `/tools status` without MCP connect).
pub fn format_config_status() -> String {
    let cfg = cli_config::load();
    let mut out = String::from("Tool bundles (Hermes-style platform_toolsets):\n");
    for info in CATALOG {
        let allowed = toolset_allowed(info.id, &cfg);
        let mark = if allowed { "●" } else { "✗" };
        out.push_str(&format!("  {mark} {:<12} {}\n", info.id, info.blurb));
    }
    if let Some(ref dis) = cfg.disabled_toolsets {
        if !dis.is_empty() {
            out.push_str(&format!("\ndisabled: {}\n", dis.join(", ")));
        }
    }
    if let Some(ref en) = cfg.enabled_toolsets {
        if !en.is_empty() {
            out.push_str(&format!("whitelist: {}\n", en.join(", ")));
        }
    }
    out.push_str(
        "\nMore tools: /apps (MCP). Shrink schema: config set --disabled-toolsets web,browser\n",
    );
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DeferredStub(&'static str);

    impl crate::agent::tools::Tool for DeferredStub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Deferred capability for filtering tests."
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn execute(&self, _: &serde_json::Value) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[test]
    fn disabled_mcp_does_not_hide_enabled_builtin_discovery_or_leak_entries() {
        let mut registry = ToolRegistry::new();
        for (name, origin) in [
            ("mcp_private_lookup", "private"),
            ("browser_screenshot", "browser"),
            ("image_read", "file"),
        ] {
            registry.register_deferred(std::sync::Arc::new(DeferredStub(name)), origin.into());
        }
        crate::agent::builtin::install_tool_search(&mut registry);
        apply_toolset_filter_in(
            &mut registry,
            &CliConfig {
                disabled_toolsets: Some(vec!["mcp".into()]),
                ..Default::default()
            },
        );
        assert!(registry.get("mcp_private_lookup").is_none());
        let discovery = registry
            .get("tool_search")
            .expect("built-ins still need discovery");
        let result = discovery.execute(&serde_json::json!({})).unwrap();
        assert!(result.contains("browser_screenshot") && result.contains("image_read"));
        assert!(!result.contains("mcp_private_lookup") && !result.contains("private ("));
        apply_toolset_filter_in(
            &mut registry,
            &CliConfig {
                disabled_toolsets: Some(vec!["mcp".into(), "browser".into(), "file".into()]),
                ..Default::default()
            },
        );
        assert!(
            registry.get("tool_search").is_none(),
            "empty discovery is removed"
        );
    }

    #[test]
    fn classify_core_bundles() {
        assert_eq!(classify_tool("memory_search"), Some("memory"));
        assert_eq!(classify_tool("file_edit"), Some("file"));
        assert_eq!(classify_tool("reference_read"), Some("file"));
        assert_eq!(classify_tool("reference_compare"), Some("todo"));
        assert_eq!(classify_tool("reference_trace"), Some("todo"));
        assert_eq!(classify_tool("shell_run"), Some("terminal"));
        assert_eq!(classify_tool("web_search"), Some("web"));
        assert_eq!(classify_tool("task"), Some("delegation"));
        assert_eq!(classify_tool("mcp_github_issues"), Some("mcp"));
        assert_eq!(
            classify_tool("tool_search"),
            Some("mcp"),
            "classification is retained for the catalog; the discovery door is rebuilt after filtering"
        );
        assert_eq!(classify_tool("lsp_references"), Some("lsp"));
        assert_eq!(classify_tool("read_symbol"), Some("lsp"));
        assert_eq!(classify_tool("lsp_hover"), Some("lsp"));
        assert_eq!(classify_tool("symbol_replace"), Some("lsp"));
        assert_eq!(classify_tool("symbol_insert"), Some("lsp"));
        assert_eq!(classify_tool("repo_map"), Some("lsp"));
        assert_eq!(classify_tool("codebase_search"), Some("lsp"));
        assert_eq!(classify_tool("skill_forget"), Some("skills"));
        assert_eq!(classify_tool("team_status"), Some("delegation"));
        assert_eq!(classify_tool("goal_complete"), Some("todo"));
        assert_eq!(classify_tool("bot_admin"), Some("messaging"));
    }

    #[test]
    fn reference_tools_follow_existing_file_and_todo_filters_not_browser_or_shell() {
        let mut registry = ToolRegistry::new();
        for name in [
            "reference_read",
            "reference_compare",
            "reference_trace",
            "shell_run",
            "browser_screenshot",
        ] {
            registry.register(Box::new(DeferredStub(name)));
        }
        apply_toolset_filter_in(
            &mut registry,
            &CliConfig {
                disabled_toolsets: Some(vec!["browser".into(), "terminal".into()]),
                ..Default::default()
            },
        );
        assert!(registry.get("shell_run").is_none());
        assert!(registry.get("browser_screenshot").is_none());
        for name in ["reference_read", "reference_compare", "reference_trace"] {
            assert!(registry.get(name).is_some());
        }
        apply_toolset_filter_in(
            &mut registry,
            &CliConfig {
                disabled_toolsets: Some(vec!["file".into()]),
                ..Default::default()
            },
        );
        assert!(registry.get("reference_read").is_none());
        assert!(registry.get("reference_compare").is_some());
        assert!(registry.get("reference_trace").is_some());
        apply_toolset_filter_in(
            &mut registry,
            &CliConfig {
                disabled_toolsets: Some(vec!["todo".into()]),
                ..Default::default()
            },
        );
        assert!(registry.get("reference_compare").is_none());
        assert!(registry.get("reference_trace").is_none());
    }

    #[test]
    fn disabled_hides_bundle() {
        let cfg = CliConfig {
            disabled_toolsets: Some(vec!["web".into()]),
            ..Default::default()
        };
        assert!(!toolset_allowed("web", &cfg));
        assert!(toolset_allowed("file", &cfg));
    }

    #[test]
    fn whitelist_overrides() {
        let cfg = CliConfig {
            enabled_toolsets: Some(vec!["memory".into(), "file".into()]),
            ..Default::default()
        };
        assert!(toolset_allowed("memory", &cfg));
        assert!(!toolset_allowed("web", &cfg));
    }
}
