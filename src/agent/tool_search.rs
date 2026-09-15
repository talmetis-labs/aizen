//! `tool_search` — the discovery tool for DEFERRED MCP tools.
//!
//! A session with a few dozen connected integrations pays their entire JSON-Schema surface on
//! every request, whether or not a single integration tool is called. Deferral moves that cost to
//! the moment of use: deferred tools are registered (dispatchable by exact name) but not
//! advertised, and this tool is the model's way in. A search result carries each match's FULL
//! argument schema in-band, after which the model calls the matched tool directly — the request's
//! `tools` array never changes mid-session, so the provider's prefix cache stays warm.
//!
//! The search itself is deliberately dependency-free and in-memory: lowercase word matching over
//! name, server, description, and schema property keys, scored so name hits outrank description
//! hits. No network, no index build — the surface is at most a few hundred tools.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::agent::tools::Tool;

/// One deferred tool plus the server key it came from (for per-server counts and `server:` labels
/// in results — the qualified name's slug is lossy, so the key rides along explicitly).
pub struct DeferredEntry {
    pub tool: Arc<dyn Tool>,
    pub server: String,
}

/// How many matches a single search returns at most (schemas are the payload — a page of them
/// defeats the point of deferring). The model narrows with a better query instead.
const MAX_RESULTS: usize = 10;
const DEFAULT_RESULTS: usize = 5;
/// Browse mode (no query) lists names only; past this many rows it truncates with a count.
const MAX_BROWSE_ROWS: usize = 100;
/// A browse row clips the description to one glanceable line.
const BROWSE_DESC_CHARS: usize = 96;

pub struct ToolSearch {
    entries: Vec<DeferredEntry>,
    description: String,
}

impl ToolSearch {
    pub fn new(entries: Vec<DeferredEntry>) -> Self {
        let description = format!(
            "Find tools that are registered but NOT pre-loaded (deferred built-ins and MCP tools): {}. \
             Search by capability in plain words (e.g. 'create issue', 'query database', 'deploy'). \
             Each match returns the tool's full argument schema; after that, call the matched tool \
             DIRECTLY by its exact name — never route the actual operation through tool_search. \
             Call with no query to list every deferred tool by name. Not for files, code, or the \
             web → use the file/lsp/web tools for those.",
            per_server_counts(&entries),
        );
        Self {
            entries,
            description,
        }
    }
}

/// `github (82), datadog (40)` — first-appearance order, so the string is deterministic and the
/// prompt/description stay byte-stable across turns of one session.
fn per_server_counts(entries: &[DeferredEntry]) -> String {
    let mut by_server: Vec<(&str, usize)> = Vec::new();
    for e in entries {
        match by_server.iter_mut().find(|(s, _)| *s == e.server) {
            Some((_, n)) => *n += 1,
            None => by_server.push((e.server.as_str(), 1)),
        }
    }
    by_server
        .iter()
        .map(|(s, n)| format!("{s} ({n})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Distinct lowercase query words worth matching on (1-char fragments match everything).
fn query_words(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in q
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
    {
        if !out.iter().any(|x| x == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// The schema's property names, lowercased — so a query like "title body" can hit a tool whose
/// description never mentions its argument names.
fn property_keys(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().map(|k| k.to_lowercase()).collect())
        .unwrap_or_default()
}

/// Relevance of one entry to the query. 0 = no match. Name hits outrank server hits outrank
/// description hits outrank argument-name hits, so `create issue` surfaces `mcp_github_create_issue`
/// above a tool that merely mentions issues in prose.
fn score(entry: &DeferredEntry, raw: &str, words: &[String]) -> usize {
    let name = entry.tool.name().to_lowercase();
    let server = entry.server.to_lowercase();
    let desc = entry.tool.description().to_lowercase();
    let props = property_keys(&entry.tool.parameters());
    let mut s = 0usize;
    if raw.chars().count() >= 3 && name.contains(raw) {
        s += 6;
    }
    for w in words {
        if name.contains(w.as_str()) {
            s += 4;
        }
        if server == *w {
            s += 3;
        } else if server.contains(w.as_str()) {
            s += 2;
        }
        if desc.contains(w.as_str()) {
            s += 2;
        }
        if props.iter().any(|p| p.contains(w.as_str())) {
            s += 1;
        }
    }
    s
}

/// First line of a description, clipped to a glanceable width (UTF-8 safe).
fn clip_line(desc: &str, max_chars: usize) -> String {
    let line = desc.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl Tool for ToolSearch {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Capability words to match (e.g. 'create issue'). Empty or absent lists every deferred tool by name."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max matches to return (default 5, cap 10)."
                }
            },
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let raw = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, MAX_RESULTS))
            .unwrap_or(DEFAULT_RESULTS);

        // Browse mode: the "what else is there" answer, names only — cheap by construction.
        if raw.is_empty() || raw == "*" {
            let mut out = format!(
                "{} deferred tool(s) across {}. Search with a query to get a tool's full schema; \
                 a tool found that way is then callable directly by name.\n",
                self.entries.len(),
                per_server_counts(&self.entries),
            );
            for e in self.entries.iter().take(MAX_BROWSE_ROWS) {
                out.push_str(&format!(
                    "- {} [{}] {}\n",
                    e.tool.name(),
                    e.server,
                    clip_line(e.tool.description(), BROWSE_DESC_CHARS),
                ));
            }
            if self.entries.len() > MAX_BROWSE_ROWS {
                out.push_str(&format!(
                    "…and {} more — search to narrow.\n",
                    self.entries.len() - MAX_BROWSE_ROWS
                ));
            }
            return Ok(out.trim_end().to_string());
        }

        let words = query_words(&raw);
        let mut hits: Vec<(usize, usize)> = self // (score, index) — index keeps ties stable
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let s = score(e, &raw, &words);
                (s > 0).then_some((s, i))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        if hits.is_empty() {
            return Ok(format!(
                "no deferred tool matched '{raw}'. Deferred servers: {}. Retry with ONE broader \
                 capability word (e.g. 'issue', 'query', 'deploy'), or call with no query to list \
                 every deferred tool.",
                per_server_counts(&self.entries),
            ));
        }

        let shown = hits.len().min(limit);
        let mut out = format!(
            "{shown} of {} match(es) for '{raw}'. Each is callable NOW — call it directly by its \
             exact name with the schema below; do not call tool_search again for it.\n",
            hits.len(),
        );
        for (_, i) in hits.iter().take(limit) {
            let e = &self.entries[*i];
            let effect = if e.tool.is_destructive() {
                "state-changing"
            } else {
                "read-only"
            };
            out.push_str(&format!(
                "\n## {} — server: {} · {}\n{}\nschema: {}\n",
                e.tool.name(),
                e.server,
                effect,
                e.tool.description(),
                serde_json::to_string(&e.tool.parameters())
                    .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string()),
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        name: &'static str,
        desc: &'static str,
        schema: Value,
        destructive: bool,
    }
    impl Tool for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters(&self) -> Value {
            self.schema.clone()
        }
        fn is_destructive(&self) -> bool {
            self.destructive
        }
        fn execute(&self, _args: &Value) -> Result<String> {
            Ok("ok".into())
        }
    }

    fn entry(
        name: &'static str,
        server: &str,
        desc: &'static str,
        destructive: bool,
    ) -> DeferredEntry {
        DeferredEntry {
            tool: Arc::new(Fake {
                name,
                desc,
                schema: json!({
                    "type": "object",
                    "properties": {"title": {"type": "string"}, "body": {"type": "string"}},
                    "required": ["title"]
                }),
                destructive,
            }),
            server: server.to_string(),
        }
    }

    fn searcher() -> ToolSearch {
        ToolSearch::new(vec![
            entry(
                "mcp_github_create_issue",
                "github",
                "[MCP github] Create a new issue in a repository",
                true,
            ),
            entry(
                "mcp_github_list_repos",
                "github",
                "[MCP github] List repositories for the authenticated user",
                false,
            ),
            entry(
                "mcp_datadog_query_metrics",
                "datadog",
                "[MCP datadog] Query timeseries metrics",
                false,
            ),
        ])
    }

    #[test]
    fn a_match_carries_the_full_schema_and_the_direct_call_instruction() {
        let out = searcher()
            .execute(&json!({"query": "create issue"}))
            .unwrap();
        assert!(out.contains("## mcp_github_create_issue"), "{out}");
        assert!(
            out.contains("\"required\":[\"title\"]"),
            "schema rides in-band: {out}"
        );
        assert!(out.contains("state-changing"), "effect labeled: {out}");
        assert!(out.contains("call it directly"), "{out}");
        // The best match leads: name hits outrank the description-only sibling.
        let create = out.find("mcp_github_create_issue").unwrap();
        let list = out.find("mcp_github_list_repos").unwrap_or(usize::MAX);
        assert!(create < list, "{out}");
    }

    #[test]
    fn server_and_property_words_also_match() {
        let by_server = searcher().execute(&json!({"query": "datadog"})).unwrap();
        assert!(
            by_server.contains("mcp_datadog_query_metrics"),
            "{by_server}"
        );
        // 'title' appears only as a schema property name, never in prose.
        let by_prop = searcher().execute(&json!({"query": "title"})).unwrap();
        assert!(by_prop.contains("## mcp_"), "{by_prop}");
    }

    #[test]
    fn no_match_names_the_servers_and_suggests_broadening() {
        let out = searcher().execute(&json!({"query": "zzzz"})).unwrap();
        assert!(out.contains("no deferred tool matched"), "{out}");
        assert!(out.contains("github (2), datadog (1)"), "{out}");
    }

    #[test]
    fn browse_mode_lists_names_without_schemas() {
        for args in [json!({}), json!({"query": ""}), json!({"query": "*"})] {
            let out = searcher().execute(&args).unwrap();
            assert!(out.contains("3 deferred tool(s)"), "{out}");
            assert!(out.contains("- mcp_github_create_issue [github]"), "{out}");
            assert!(
                !out.contains("\"properties\""),
                "browse carries no schema: {out}"
            );
        }
    }

    #[test]
    fn limit_is_clamped_and_ties_keep_registration_order() {
        let out = searcher()
            .execute(&json!({"query": "github", "limit": 1}))
            .unwrap();
        assert_eq!(out.matches("## mcp_").count(), 1, "{out}");
        assert!(out.contains("1 of 2 match(es)"), "{out}");
    }

    #[test]
    fn the_description_counts_per_server() {
        let s = searcher();
        assert!(s.description().contains("github (2), datadog (1)"));
        assert!(s.description().len() > 20);
    }
}
