//! `tool_search` — the discovery tool for DEFERRED MCP tools.
//!
//! A session with a few dozen connected integrations pays their entire JSON-Schema surface on
//! every request, whether or not a single integration tool is called. Deferral moves that cost to
//! the moment of use: deferred tools are registered (dispatchable by exact name) but not
//! advertised, and this tool is the model's way in. Search results identify the matches; the
//! agent loop attaches the corresponding argument schemas to the next request.
//!
//! The search itself is deliberately dependency-free and in-memory: lowercase word matching over
//! name, server, description, and schema property keys, scored so name hits outrank description
//! hits. No network, no index build — the surface is at most a few hundred tools.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use super::request_language::{contains_identifier, contains_phrase, normalize};
use crate::agent::tools::{Tool, ToolRegistry};
use crate::core::types::ToolDef;
use unicode_segmentation::UnicodeSegmentation;

/// One deferred tool plus the server key it came from (for per-server counts and `server:` labels
/// in results — the qualified name's slug is lossy, so the key rides along explicitly).
pub struct DeferredEntry {
    pub tool: Arc<dyn Tool>,
    pub server: String,
}

/// How many matches a single search returns at most (schemas are the payload — a page of them
/// defeats the point of deferring). The model narrows with a better query instead.
const MAX_RESULTS: usize = 10;
const DEFAULT_RESULTS: usize = 3;
const MATCH_HEADER: &str = "Matched tools: ";

/// Only the first line is generated exclusively from registered tool names. Descriptions,
/// schemas and query text below it are untrusted. Resolve against this run's filtered registry.
pub(super) fn discovered_names<'a>(output: &'a str, registry: &ToolRegistry) -> Vec<&'a str> {
    output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(MATCH_HEADER))
        .into_iter()
        .flat_map(|names| names.split(' '))
        .take(MAX_RESULTS)
        .filter(|name| registry.is_deferred(name))
        .collect()
}

/// Each search batch replaces the previous one. A new agent run starts with an empty selection.
pub(super) fn select_matches(
    registry: &ToolRegistry,
    defs: &mut Vec<ToolDef>,
    names: &[&str],
) -> bool {
    if names.is_empty() {
        return false;
    }
    let mut selected = Vec::new();
    for &name in names {
        if selected.len() == MAX_RESULTS {
            break;
        }
        if !selected.contains(&name) {
            selected.push(name);
        }
    }
    let active = |def: &ToolDef| registry.is_deferred(&def.function.name);
    if defs
        .iter()
        .filter(|d| active(d))
        .map(|d| d.function.name.as_str())
        .eq(selected.iter().copied())
    {
        return false;
    }
    defs.retain(|d| !active(d));
    defs.extend(
        selected
            .into_iter()
            .filter_map(|n| registry.get(n))
            .map(|tool| ToolDef::function(tool.name(), tool.description(), tool.parameters())),
    );
    true
}
/// Prepare schemas, never execute tools. Prefer registered identifiers; a short remote name
/// must be unique across servers. For natural language require both a named server and a
/// distinctive word in a single best capability. Ambiguous queries use normal discovery.
pub(super) fn requested_tools(registry: &ToolRegistry, request: &str) -> Vec<String> {
    if request.is_empty() || request.len() > 16_384 || registry.get("tool_search").is_none() {
        return Vec::new();
    }
    let folded = normalize(request);
    let words: Vec<_> = capability_words(&folded)
        .into_iter()
        .filter(|word| {
            word.chars().count() >= 3
                && !matches!(
                    word.as_str(),
                    "the"
                        | "for"
                        | "from"
                        | "this"
                        | "that"
                        | "what"
                        | "and"
                        | "with"
                        | "using"
                        | "tool"
                        | "mcp"
                        | "integration"
                        | "current"
                        | "please"
                        | "read"
                        | "get"
                        | "use"
                        | "list"
                        | "show"
                        | "tools"
                        | "can"
                        | "you"
                        | "how"
                        | "does"
                )
        })
        .collect();
    let entries = registry.deferred_entries();
    let mut explicit = Vec::new();
    let mut aliases = Vec::new();
    let mut capability_aliases = Vec::new();
    let mut best = None;
    let mut best_score = 0;
    for (tool, server) in &entries {
        let name = tool.name();
        if contains_identifier(&folded, name) {
            explicit.push(name);
            continue;
        }
        if !name.starts_with("mcp_") {
            continue;
        }
        let namespace = super::mcp::slug(server);
        let prefix = format!("mcp_{namespace}_");
        let Some(remote) = name.strip_prefix(&prefix) else {
            continue;
        };
        let scoped = contains_identifier(&folded, &normalize(server))
            || contains_identifier(&folded, &namespace);
        if remote.contains('_')
            && (contains_identifier(&folded, remote)
                || contains_identifier(&folded, &remote.replace('_', "-")))
        {
            aliases.push((remote, name, scoped));
            continue;
        }
        if matches_alias(tool.as_ref(), &folded) {
            capability_aliases.push((name, scoped));
        }
        if !scoped {
            continue;
        }
        let capability = query_words(&normalize(tool.description()));
        let parts = query_words(remote);
        let score: usize = words
            .iter()
            .filter(|word| !namespace.split('_').any(|s| s == word.as_str()))
            .map(|word| {
                2 * usize::from(parts.contains(word)) + usize::from(capability.contains(word))
            })
            .sum();
        if score > best_score {
            best_score = score;
            best = Some(name);
        } else if score == best_score {
            best = None;
        }
    }
    for &(alias, name, scoped) in &aliases {
        let eager_collision = !scoped
            && registry
                .advertised_names()
                .iter()
                .any(|n| n.starts_with("mcp_") && n.ends_with(&format!("_{alias}")));
        if !eager_collision
            && !aliases.iter().any(|(other, candidate, same_scope)| {
                *other == alias && *candidate != name && (!scoped || *same_scope)
            })
        {
            explicit.push(name);
        }
    }
    let hits = if !explicit.is_empty() {
        explicit
    } else if !capability_aliases.is_empty() {
        // The user may name a capability in any language. A named server disambiguates
        // identical aliases, while an unscoped collision stays available through discovery.
        let any_scoped = capability_aliases.iter().any(|(_, scoped)| *scoped);
        let mut matching = capability_aliases
            .into_iter()
            .filter(|(_, scoped)| !any_scoped || *scoped);
        match (matching.next(), matching.next()) {
            (Some((name, scoped)), None)
                if scoped
                    || !registry.advertised_names().iter().any(|name| {
                        registry
                            .get(name)
                            .is_some_and(|tool| matches_alias(tool, &folded))
                    }) =>
            {
                vec![name]
            }
            _ => Vec::new(),
        }
    } else {
        best.into_iter().collect()
    };
    let mut bytes = 0usize;
    hits.into_iter()
        .take(3)
        .filter_map(|name| {
            let t = registry.get(name)?;
            let def = ToolDef::function(name, t.description(), t.parameters());
            let size = serde_json::to_vec(&def).ok()?.len();
            if size > 8192usize.saturating_sub(bytes) {
                return None;
            }
            bytes += size;
            Some(name.to_owned())
        })
        .collect()
}

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
            "Find additional tools by capability or name: {}. Use tools already advertised directly. \
             Search by capability in plain words (e.g. 'create issue', 'query database', 'deploy'). \
             For a localized query, include a fallback_query in the catalog's language when known. \
             Matched tools are attached to the next request. Call them directly by exact name. \
             Call with no query to list deferred tools by name. When file, code, or web tools are \
             already attached, use them directly.",
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
        .split(['_', '-'])
        .flat_map(UnicodeSegmentation::unicode_words)
        .filter(|w| w.chars().count() >= 2)
    {
        if !out.iter().any(|x| x == w) {
            out.push(w.to_string());
        }
    }
    out
}

fn capability_words(folded: &str) -> Vec<String> {
    let mut words = query_words(folded);
    for (phrase, capability) in [
        ("so du", "balance"),
        ("hoa don", "invoice"),
        ("bang tinh", "spreadsheet"),
        ("lich hen", "appointment"),
        ("tep dinh kem", "attachment"),
        ("ma nguon", "code"),
    ] {
        if contains_phrase(folded, phrase) && !words.iter().any(|word| word == capability) {
            words.push(capability.to_owned());
        }
    }
    words
}

fn matches_alias(tool: &dyn Tool, folded_query: &str) -> bool {
    tool.search_aliases().iter().any(|alias| {
        let folded_alias = normalize(alias);
        folded_alias.chars().count() >= 2 && contains_phrase(folded_query, &folded_alias)
    })
}

/// The schema's property names, lowercased — so a query like "title body" can hit a tool whose
/// description never mentions its argument names.
fn property_keys(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().map(|k| normalize(k)).collect())
        .unwrap_or_default()
}

/// Relevance of one entry to the query. 0 = no match. Name hits outrank server hits outrank
/// description hits outrank argument-name hits, so `create issue` surfaces `mcp_github_create_issue`
/// above a tool that merely mentions issues in prose.
fn score(entry: &DeferredEntry, raw: &str, words: &[String]) -> usize {
    let name = normalize(entry.tool.name());
    let server = normalize(&entry.server);
    let desc = normalize(entry.tool.description());
    let props = property_keys(&entry.tool.parameters());
    let mut s = 0usize;
    if raw.chars().count() >= 2 && (name.contains(raw) || contains_phrase(&desc, raw)) {
        s += 6;
    }
    if matches_alias(entry.tool.as_ref(), raw) {
        s += 16;
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
                "fallback_query": {
                    "type": "string",
                    "description": "Optional equivalent capability in the catalog's language. Used only when query has no match."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max matches to attach (default 3, cap 10). Use 1 for an exact capability."
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
            .trim();
        anyhow::ensure!(raw.len() <= 4096, "Tool search query exceeds 4096 bytes");
        let explicit_limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, MAX_RESULTS as u64) as usize);

        // Browse mode: the "what else is there" answer, names only — cheap by construction.
        if raw.is_empty() || raw == "*" {
            let mut out = format!(
                "{} deferred tool(s) across {}. Search with a query to attach a tool for the next request.\n",
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

        let rank = |query: &str| {
            let words = capability_words(query);
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(i, entry)| {
                    let s = score(entry, query, &words);
                    (s > 0).then_some((s, i))
                })
                .collect::<Vec<_>>()
        };
        let mut folded = normalize(raw);
        let mut hits = rank(&folded);
        if hits.is_empty() {
            if let Some(fallback) = args
                .get("fallback_query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty() && text.len() <= 4096)
            {
                let fallback = normalize(fallback);
                if fallback != folded {
                    folded = fallback;
                    hits = rank(&folded);
                }
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        if hits.is_empty() {
            return Ok(format!(
                "no deferred tool matched '{raw}'. Deferred servers: {}. Retry with ONE broader \
                 capability word (e.g. 'issue', 'query', 'deploy'), or call with no query to list \
                 every deferred tool.",
                per_server_counts(&self.entries),
            ));
        }

        let exact_name = |entry: &DeferredEntry| {
            let name = entry.tool.name();
            name == folded
                || name
                    .strip_prefix(&format!("mcp_{}_", super::mcp::slug(&entry.server)))
                    .is_some_and(|short| short == folded)
        };
        let mut exact = hits.iter().filter(|(_, i)| exact_name(&self.entries[*i]));
        if let Some(&hit) = exact.next() {
            if exact.next().is_none() {
                hits.clear();
                hits.push(hit);
            }
        }
        if explicit_limit.is_none() {
            let matching_alias = |i: usize| {
                self.entries[i]
                    .tool
                    .search_aliases()
                    .iter()
                    .any(|alias| normalize(alias) == folded)
            };
            let mut aliases = hits.iter().filter(|(_, i)| matching_alias(*i));
            if let Some(&hit) = aliases.next() {
                if aliases.next().is_none() {
                    hits.clear();
                    hits.push(hit);
                }
            }
        }
        let limit = explicit_limit.unwrap_or(DEFAULT_RESULTS);
        let shown = hits.len().min(limit);
        let names = hits
            .iter()
            .take(limit)
            .map(|(_, i)| self.entries[*i].tool.name())
            .collect::<Vec<_>>()
            .join(" ");
        let mut out = format!(
            "{MATCH_HEADER}{names}\n{shown} of {} match(es). Call the attached tool directly by its exact name.\n",
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
                "- {} [{} · {}] {}\n",
                e.tool.name(),
                e.server,
                effect,
                clip_line(e.tool.description(), BROWSE_DESC_CHARS),
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_language_fallback_is_used_within_the_same_search() {
        let search = searcher();
        let hit = search
            .execute(&json!({"query": "افتح قضية", "fallback_query": "create_issue"}))
            .unwrap();
        assert!(
            hit.starts_with("Matched tools: mcp_github_create_issue\n"),
            "{hit}"
        );
        let hit = search
            .execute(&json!({"query": "create_issue", "fallback_query": "datadog"}))
            .unwrap();
        assert!(
            hit.starts_with("Matched tools: mcp_github_create_issue\n"),
            "{hit}"
        );
    }

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
    fn routing_selects_local_distinctive_matches_and_leaves_ambiguous_queries_to_search() {
        let mut reg = ToolRegistry::new();
        for item in [
            entry(
                "mcp_finance_ledger_read",
                "finance",
                "Read the balance",
                false,
            ),
            entry(
                "mcp_finance_archive_scan",
                "finance",
                "Scan archive records",
                false,
            ),
            entry("mcp_books_ledger_read", "books", "Read the balance", false),
            entry("skill_save", "builtin", "Write a skill", true),
        ] {
            reg.register_deferred(item.tool, item.server);
        }
        crate::agent::builtin::install_tool_search(&mut reg);
        for (request, expect) in [
            ("balance in finance?", Some("mcp_finance_ledger_read")),
            (
                "please use ledger_read on finance",
                Some("mcp_finance_ledger_read"),
            ),
            ("run mcp_books_ledger_read", Some("mcp_books_ledger_read")),
            ("use skill_save", Some("skill_save")),
            ("use ledger_read", None),
            ("hello", None),
            ("what does finance do?", None),
            ("balance from finance and books?", None),
            ("not_mcp_books_ledger_read_or_something", None),
        ] {
            assert_eq!(
                requested_tools(&reg, request),
                expect.into_iter().collect::<Vec<_>>(),
                "{request}"
            );
        }
        let result = reg
            .get("tool_search")
            .unwrap()
            .execute(&json!({"query":"skill_save","limit":1}))
            .unwrap();
        let mut defs = reg.defs();
        assert!(select_matches(
            &reg,
            &mut defs,
            &discovered_names(&result, &reg)
        ));
        assert!(defs.iter().any(|def| def.function.name == "skill_save"));
        assert!(!reg
            .defs()
            .iter()
            .any(|def| def.function.name == "skill_save"));
    }

    #[test]
    fn a_match_identifies_the_tool_without_copying_its_schema() {
        let out = searcher()
            .execute(&json!({"query": "create issue"}))
            .unwrap();
        assert!(out.contains("- mcp_github_create_issue"), "{out}");
        assert!(
            !out.contains("\"required\""),
            "schema is attached to the next request instead: {out}"
        );
        assert!(out.contains("state-changing"), "effect labeled: {out}");
        assert!(out.contains("Call the attached tool directly"), "{out}");
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
        assert!(by_prop.contains("- mcp_"), "{by_prop}");
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
        assert_eq!(out.matches("\n- mcp_").count(), 1, "{out}");
        assert!(out.contains("1 of 2 match(es)"), "{out}");
    }

    #[test]
    fn the_description_counts_per_server() {
        let s = searcher();
        assert!(s.description().contains("github (2), datadog (1)"));
        assert!(s.description().len() > 20);
    }

    #[test]
    fn mcp_requests_only_advertise_matches_from_the_current_registry_and_batch() {
        let search = searcher();
        let mut reg = ToolRegistry::new();
        for e in &search.entries {
            reg.register_deferred(e.tool.clone(), e.server.clone());
        }
        let mut defs = reg.defs();
        assert!(defs.is_empty());
        let result = search
            .execute(&json!({"query":"github", "limit":1}))
            .unwrap();
        let names = discovered_names(&result, &reg);
        assert!(select_matches(&reg, &mut defs, &names));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, names[0]);
        assert!(defs[0].function.parameters.get("properties").is_some());
        assert!(!select_matches(&reg, &mut defs, &names));
        let result = search.execute(&json!({"query":"datadog"})).unwrap();
        assert!(select_matches(
            &reg,
            &mut defs,
            &discovered_names(&result, &reg)
        ));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mcp_datadog_query_metrics");
        assert!(discovered_names(
            "Matched tools: mcp_removed shell_run\nMatched tools: mcp_github_create_issue",
            &reg
        )
        .is_empty());
        assert!(discovered_names(&search.execute(&json!({})).unwrap(), &reg).is_empty());
        assert!(
            reg.defs().is_empty(),
            "the selection must be local to a run"
        );
    }
}
