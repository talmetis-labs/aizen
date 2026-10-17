//! agentskill.sh skill registry client — search + install reusable skills from the public
//! marketplace (274k+ skills). Pure HTTP/JSON over reqwest (NO npx/Node). Search hits
//! `GET {base}/api/skills?q=…`; the API returns each skill's GitHub coordinates, and the body is
//! fetched from `raw.githubusercontent.com`. The base defaults to `https://agentskill.sh` and is
//! overridable via `cli-config.json` `skill_registry`.
//!
//! Installed skills are THIRD-PARTY instructions the agent will then follow → `skill_install` is
//! `is_destructive() = true` (approval-gated) and search surfaces the registry's `securityScore`.
//!
//! Network posture: every outbound URL (the registry base AND the GitHub raw URL built from the
//! registry's response) passes the `net_guard` SSRF floor, and all fetches go through the shared
//! `reach::http` guarded client — auto-redirects disabled, every redirect hop re-vetted, bodies
//! bounded — so neither a poisoned `skill_registry` config nor a malicious registry response can
//! steer a fetch at loopback/private/link-local targets (cloud metadata et al.).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::tools::Tool;

/// Default marketplace (the owner's instruction: "ng mặc định lên đây kiếm").
pub const DEFAULT_REGISTRY: &str = "https://agentskill.sh";
/// Default search result cap.
const DEFAULT_LIMIT: usize = 20;

/// One search hit from `/api/skills` (the subset of fields we use).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySkill {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub content_quality_score: Option<u32>,
    #[serde(default)]
    pub security_score: Option<u32>,
    #[serde(default)]
    pub github_owner: String,
    #[serde(default)]
    pub github_repo: String,
    #[serde(default)]
    pub github_branch: String,
    #[serde(default)]
    pub github_sha: String,
    #[serde(default)]
    pub github_path: String,
}

#[derive(Debug, Default, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<RegistrySkill>,
    #[serde(default)]
    total: usize,
    /// Whether another page exists — the browse fallback stops rather than asking past the end.
    #[serde(default, rename = "hasMore")]
    has_more: bool,
}

/// The by-slug record, from the route that serves one named skill. It carries the markdown itself
/// rather than GitHub coordinates, so an install through it never leaves the registry.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectSkill {
    #[serde(default)]
    name: String,
    #[serde(default)]
    skill_md: String,
}

impl RegistrySkill {
    /// `owner/name` identifier (prefers the API `slug`).
    pub fn id(&self) -> String {
        if !self.slug.is_empty() {
            self.slug.clone()
        } else if !self.owner.is_empty() {
            format!("{}/{}", self.owner, self.name)
        } else {
            self.name.clone()
        }
    }

    /// The GitHub raw URL for the skill markdown — pinned to the immutable SHA when present (so a
    /// later upstream edit can't change what we installed), else the branch, else `main`.
    pub fn raw_url(&self) -> Option<String> {
        if self.github_owner.is_empty()
            || self.github_repo.is_empty()
            || self.github_path.is_empty()
        {
            return None;
        }
        let reff = if !self.github_sha.is_empty() {
            self.github_sha.as_str()
        } else if !self.github_branch.is_empty() {
            self.github_branch.as_str()
        } else {
            "main"
        };
        let path = self.github_path.trim_start_matches('/');
        Some(format!(
            "https://raw.githubusercontent.com/{}/{}/{reff}/{path}",
            self.github_owner, self.github_repo
        ))
    }

    /// One-line summary for the search UI / tool result.
    pub fn summary_line(&self) -> String {
        let q = self
            .content_quality_score
            .map(|s| format!(" · quality {s}"))
            .unwrap_or_default();
        let s = self
            .security_score
            .map(|s| format!(" · security {s}"))
            .unwrap_or_default();
        let desc: String = self.description.chars().take(90).collect();
        format!("{}{q}{s} — {desc}", self.id())
    }
}

/// The configured registry base (default `agentskill.sh`).
pub fn registry_base() -> String {
    crate::core::cli_config::load()
        .skill_registry
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

/// Search the registry by keyword. `limit` caps results. SSRF-guarded (see the module doc).
///
/// The keyword index is asked first and, when it fails, is not the last word: the catalogue is
/// still served page by page, and matching the query against those pages here finds the same rows
/// the index would have. Slower, and only on the failing path — but a registry whose search is
/// down is not a registry with no skills on it, and for a day of HTTP 500s that is exactly how
/// this read to everyone using it.
pub async fn search(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let indexed = match search_index(query, limit).await {
        Ok(hits) => return Ok(hits),
        Err(e) => e,
    };
    // Nothing found by hand does not disprove the index — but the index being down is not the
    // whole story either. The catalogue only serves its most recent entries (observed 2026-10-09:
    // `total` is a single running count and `totalPages` shifts between pages, so old skills fall
    // out of the window), and a skill that was there last month can be unfindable today. Saying
    // just "HTTP 500" reads as "the marketplace is down, give up" when the truth is "the index is
    // down AND the recent catalogue holds no match" — the difference decides whether retrying
    // later or falling back to local skills is the right move.
    match browse_match(query, limit).await {
        Ok(hits) if !hits.is_empty() => Ok(hits),
        Ok(_) => bail!(
            "{indexed:#}; the browse fallback found no match for '{query}' in the recent \
             catalogue (it only serves the registry's most recent entries, so an older skill may \
             exist but be unreachable until the search index recovers)"
        ),
        Err(_) => Err(indexed),
    }
}

/// Every word of the query has to appear somewhere in what the row says about itself — the way a
/// person reading down the catalogue would match it.
fn matches(sk: &RegistrySkill, words: &[String]) -> bool {
    let hay = format!("{} {} {}", sk.id(), sk.name, sk.description).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// How far down the catalogue the fallback is willing to read. Bounded on purpose: this runs only
/// when the index is down, and a search that quietly turns into hundreds of requests is its own
/// kind of outage.
const BROWSE_PAGES: usize = 12;
const BROWSE_PAGE_SIZE: usize = 100;

/// Page the listing route and filter locally. Stops at the first full page of results, at the end
/// of the catalogue, or at `BROWSE_PAGES` — whichever comes first.
async fn browse_match(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let base = registry_base();
    let client = crate::agent::reach::http::client()?;
    let mut out: Vec<RegistrySkill> = Vec::new();
    for page in 1..=BROWSE_PAGES {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/api/skills", base.trim_end_matches('/')),
            &[
                ("limit", BROWSE_PAGE_SIZE.to_string()),
                ("page", page.to_string()),
            ],
        )
        .with_context(|| format!("bad registry base '{base}'"))?;
        crate::core::net_guard::guard_url_async(url.as_str()).await?;
        let f = crate::agent::reach::http::get(&client, url.as_str(), &[])
            .await
            .with_context(|| format!("reading the catalogue on {base}"))?;
        if !f.is_success() {
            break;
        }
        let Ok(parsed) = serde_json::from_slice::<SearchResponse>(&f.body) else {
            break;
        };
        if parsed.data.is_empty() {
            break;
        }
        let more = parsed.has_more;
        for sk in parsed.data {
            if matches(&sk, &words) && !out.iter().any(|o| o.id() == sk.id()) {
                out.push(sk);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        if !more {
            break;
        }
    }
    Ok(out)
}

/// Ask the registry's own keyword index.
async fn search_index(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let base = registry_base();
    let url = reqwest::Url::parse_with_params(
        &format!("{}/api/skills", base.trim_end_matches('/')),
        &[("q", query), ("limit", &limit.to_string())],
    )
    .with_context(|| format!("bad registry base '{base}'"))?;
    crate::core::net_guard::guard_url_async(url.as_str()).await?;
    let client = crate::agent::reach::http::client()?;
    let f = crate::agent::reach::http::get(&client, url.as_str(), &[])
        .await
        .with_context(|| format!("searching {base}"))?;
    if !f.is_success() {
        bail!("registry {base} returned HTTP {}", f.status);
    }
    let parsed: SearchResponse =
        serde_json::from_slice(&f.body).context("parsing registry JSON")?;
    let _ = parsed.total;
    Ok(parsed.data)
}

/// Fetch a registry skill's markdown body from its GitHub raw URL. The URL is BUILT from the
/// registry's response (untrusted input) → it passes the SSRF floor like any model-supplied URL.
pub async fn fetch_body(sk: &RegistrySkill) -> Result<String> {
    let url = sk
        .raw_url()
        .context("this skill has no GitHub source location")?;
    crate::core::net_guard::guard_url_async(&url).await?;
    let client = crate::agent::reach::http::client()?;
    let f = crate::agent::reach::http::get(&client, &url, &[])
        .await
        .with_context(|| format!("GET {url}"))?;
    if !f.is_success() {
        bail!("GitHub returned HTTP {} for {url}", f.status);
    }
    Ok(raw_document(&f.body)?.to_string())
}

/// Decode downloaded skill documents without silently changing invalid UTF-8 bytes.
/// Both marketplace and direct URL imports use this before their raw-document write path.
pub(crate) fn raw_document(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("skill document must be valid UTF-8")
}

/// Preserve the entire document, including unknown YAML and guards. The install identity is
/// separate from the upstream name, and imports refuse an existing flat or bundled destination.
pub(crate) fn import_raw_document(
    fallback: &str,
    document: &str,
    name_override: Option<&str>,
    provenance: crate::skills::SkillProvenance,
) -> Result<crate::skills::Skill> {
    let parsed = crate::skills::parse_markdown(document, fallback);
    let name = name_override
        .unwrap_or_else(|| {
            if parsed.name.trim().is_empty() {
                fallback
            } else {
                &parsed.name
            }
        })
        .trim();
    if name.is_empty() {
        bail!("a skill install name is required");
    }
    crate::skills::import_document(name, document, provenance)
}

/// Shared CLI/menu/tool report: use the final entry and lookup alias, not a guessed flat path.
/// Installing globally does not make a copy active when a zone/repo copy has precedence.
pub(crate) fn install_report(sk: &crate::skills::Skill) -> String {
    let clean =
        |s: &str| crate::agent::task_tool::sanitize_agent_body(s).replace(['\r', '\n'], " ");
    let mut report = format!("installed '{}'", clean(sk.key()));
    if sk.name != sk.key() {
        report.push_str(&format!(" (upstream name: '{}')", clean(&sk.name)));
    }
    if let Some(location) = &sk.location {
        report.push_str(&format!(" → {}", clean(&location.entry.to_string_lossy())));
        report.push_str(&format!(
            "\nRelative resources resolve under {}. Scripts and dependencies are not run automatically.",
            clean(&location.resource_root.to_string_lossy())
        ));
        match crate::skills::load(sk.key()).and_then(|active| active.location) {
            Some(active_location) if active_location.entry != location.entry => {
                report.push_str(&format!(
                    "\nWarning: this copy is shadowed by {} ({}); the installed copy is not active here.",
                    clean(&active_location.entry.to_string_lossy()),
                    match active_location.origin {
                        crate::skills::SkillOrigin::Repo => "repo",
                        crate::skills::SkillOrigin::Project => "project zone",
                        crate::skills::SkillOrigin::Global => "global",
                    }
                ));
            }
            None => report.push_str(
                "\nWarning: the active copy could not be determined (missing or ambiguous skill location).",
            ),
            Some(_) => {}
        }
    } else {
        report.push_str("\nWarning: the final entry and active copy could not be determined.");
    }
    report
}

/// The URL that serves one skill by its `owner/name`. Built through `path_segments_mut` so the
/// slash inside the slug is encoded as a slug and cannot become another path segment — a name is
/// data here, and a registry route is not a place to let data spell itself.
fn by_slug_url(base: &str, slug: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base.trim_end_matches('/'))
        .with_context(|| format!("bad registry base '{base}'"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("registry base '{base}' cannot hold a path"))?
        .pop_if_empty()
        .extend(["api", "agent", "skills", slug, "install"]);
    Ok(url)
}

/// Fetch one skill by `owner/name`. `Ok(None)` means the registry does not serve this route or
/// does not know the slug — both are reasons to fall back to a search, not reasons to fail.
async fn by_slug(slug: &str) -> Result<Option<DirectSkill>> {
    let base = registry_base();
    let url = by_slug_url(&base, slug)?;
    crate::core::net_guard::guard_url_async(url.as_str()).await?;
    let client = crate::agent::reach::http::client()?;
    let f = crate::agent::reach::http::get(&client, url.as_str(), &[])
        .await
        .with_context(|| format!("asking {base} for '{slug}'"))?;
    if !f.is_success() {
        return Ok(None);
    }
    // Invalid UTF-8 is not a route miss: do not hide corrupt source bytes behind search fallback.
    let document = raw_document(&f.body)?;
    let got: DirectSkill = match serde_json::from_str(document) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    Ok((!got.skill_md.trim().is_empty()).then_some(got))
}

/// Install one skill by name. An exact `owner/name` is fetched by slug; anything else is a search
/// followed by the best match.
///
/// The by-slug route first because an install by exact name should not borrow the search index's
/// outages. One caveat, observed 2026-10-09: the route reads the same index search does, so it
/// answers 404 for a slug that has fallen out of the catalogue's recent-entries window — it
/// rescues installs while the index is down, not installs OF skills the index no longer lists.
pub async fn install(query: &str) -> Result<crate::skills::Skill> {
    let q = query.trim();
    if q.is_empty() {
        bail!("a marketplace skill slug or query is required");
    }
    if q.contains('/') {
        if let Some(saved) = install_by_slug(q, q).await? {
            return Ok(saved);
        }
    }

    let hits = search(q, DEFAULT_LIMIT).await?;
    if hits.is_empty() {
        bail!("no skill on {} matches '{query}'", registry_base());
    }
    let want = q.to_lowercase();
    let chosen = hits
        .iter()
        .find(|s| s.id().to_lowercase() == want || s.name.to_lowercase() == want)
        .unwrap_or(&hits[0])
        .clone();

    // The registry's own copy before GitHub's. Its stored coordinates are a snapshot of where the
    // file was, and repositories move: `bytedance/frontend-design` is listed at a path that now
    // answers 404, while the registry still serves the same skill by slug. Falling at that fence
    // would fail an install for a reason that has nothing to do with the skill.
    if let Some(saved) = install_by_slug(&chosen.id(), &chosen.name).await? {
        return Ok(saved);
    }
    let body_md = fetch_body(&chosen).await?;
    let provenance = crate::skills::SkillProvenance {
        source: chosen
            .raw_url()
            .context("this skill has no GitHub source location")?,
        repository: Some(format!("{}/{}", chosen.github_owner, chosen.github_repo)),
        path: Some(chosen.github_path.clone()),
        sha: (!chosen.github_sha.is_empty()).then_some(chosen.github_sha.clone()),
    };
    import_raw_document(&chosen.name, &body_md, None, provenance)
}

/// Fetch `slug` from the registry's by-slug route and save it. `Ok(None)` when that route has
/// nothing for this slug, which leaves the caller free to try GitHub.
async fn install_by_slug(slug: &str, name_hint: &str) -> Result<Option<crate::skills::Skill>> {
    let Some(direct) = by_slug(slug).await? else {
        return Ok(None);
    };
    let fallback = [direct.name.trim(), name_hint.trim()]
        .into_iter()
        .find(|n| !n.is_empty())
        .unwrap_or(slug)
        .to_string();
    // A bare `owner/` is not a name to save a file under.
    let fallback = fallback
        .rsplit('/')
        .find(|p| !p.is_empty())
        .unwrap_or(slug)
        .to_string();
    let provenance = crate::skills::SkillProvenance {
        source: by_slug_url(&registry_base(), slug)?.to_string(),
        repository: None,
        path: None,
        sha: None,
    };
    import_raw_document(&fallback, &direct.skill_md, None, provenance).map(Some)
}

/// Bridge an async future to the sync `Tool::execute` path — the shared cancel-aware bridge
/// (valid on workers AND spawn_blocking threads; Esc aborts an in-flight marketplace call).
fn block<T>(f: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    crate::agent::tools::block_for_tool(f)
}

// ── agent tools ────────────────────────────────────────────────────────────────

pub struct SkillSearch;
impl Tool for SkillSearch {
    fn name(&self) -> &str {
        "skill_search"
    }
    fn description(&self) -> &str {
        "Search the agentskill.sh marketplace for a reusable skill (a saved procedure) by keyword. \
         Call this when a task needs a recurring how-to you don't already have a local skill for; \
         then skill_install the best match to use it."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "keywords, e.g. 'deploy fastapi' or 'release notes'"},
                "limit": {"type": "integer", "description": "max results (default 20)"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true // read-only marketplace query — the shared bridge is spawn_blocking-safe
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .context("missing required string arg 'query'")?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, 50);
        let hits = block(search(query, limit))?;
        if hits.is_empty() {
            return Ok(format!("no skills on {} match '{query}'", registry_base()));
        }
        let mut out = format!(
            "{} result(s) from {} (skill_install \"<owner/name>\" to use one):\n",
            hits.len(),
            registry_base()
        );
        for sk in &hits {
            out.push_str(&format!("- {}\n", sk.summary_line()));
        }
        Ok(out.trim_end().to_string())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InstallSource<'a> {
    Registry(&'a str),
    GitHub {
        repository: &'a str,
        path: &'a str,
        reference: Option<&'a str>,
        name: Option<&'a str>,
    },
}

/// Runtime validation complements JSON Schema: providers do not all enforce source exclusivity.
/// Reject malformed requests before entering the async/network/store path.
fn install_source(args: &Value) -> Result<InstallSource<'_>> {
    let object = args
        .as_object()
        .context("skill_install expects an object")?;
    for key in object.keys() {
        if !["slug", "github", "path", "ref", "name"].contains(&key.as_str()) {
            bail!("unknown skill_install argument '{key}'");
        }
    }
    let string = |key: &str| -> Result<Option<&str>> {
        let Some(value) = object.get(key) else {
            return Ok(None);
        };
        let value = value
            .as_str()
            .with_context(|| format!("'{key}' must be a string"))?
            .trim();
        if value.is_empty() {
            bail!("'{key}' must not be empty");
        }
        Ok(Some(value))
    };
    let slug = string("slug")?;
    let github = string("github")?;
    let path = string("path")?;
    let reference = string("ref")?;
    let name = string("name")?;
    match (slug, github) {
        (Some(slug), None) if path.is_none() && reference.is_none() && name.is_none() => {
            Ok(InstallSource::Registry(slug))
        }
        (Some(_), _) => bail!("provide either 'slug' alone or 'github' with 'path', not both"),
        (None, Some(repository)) => Ok(InstallSource::GitHub {
            repository,
            path: path
                .context("GitHub installs require 'path' (the directory containing SKILL.md)")?,
            reference,
            name,
        }),
        (None, None) => {
            bail!("provide 'slug' or 'github' with 'path'; 'ref' and 'name' require 'github'")
        }
    }
}

pub struct SkillInstall;
impl Tool for SkillInstall {
    fn name(&self) -> &str {
        "skill_install"
    }
    fn description(&self) -> &str {
        "Install third-party instructions: marketplace slug OR public GitHub github+path \
         (optional ref/name). Requires approval. GitHub ref resolves to SHA; resources kept. \
         No replacement, script execution, or dependency installs. Returns entry/steps; \
         repo/zone may shadow."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {"type": "string", "minLength": 1, "description": "marketplace owner/name from skill_search"},
                "github": {"type": "string", "minLength": 1, "description": "public GitHub OWNER/REPO"},
                "path": {"type": "string", "minLength": 1, "description": "directory containing SKILL.md; . for root"},
                "ref": {"type": "string", "minLength": 1, "description": "branch/tag/commit; defaults to default branch"},
                "name": {"type": "string", "minLength": 1, "description": "local alias; preserves upstream name"}
            },
            "oneOf": [
                {"required": ["slug"], "not": {"anyOf": [
                    {"required": ["github"]}, {"required": ["path"]},
                    {"required": ["ref"]}, {"required": ["name"]}
                ]}},
                {"required": ["github", "path"], "not": {"required": ["slug"]}}
            ],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        true // installs + persists third-party instructions the agent will then follow
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let sk = match install_source(args)? {
            InstallSource::Registry(slug) => block(install(slug))?,
            InstallSource::GitHub {
                repository,
                path,
                reference,
                name,
            } => block(crate::skills::github::install(
                repository, path, reference, name,
            ))?,
        };
        Ok(format!(
            "{}\n\n{}",
            install_report(&sk),
            crate::skills::render_loaded(&sk)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloaded_documents_require_utf8_without_lossy_replacement() {
        let raw = b"---\r\nname: preserved\r\n---\r\nSteps\r\n";
        assert_eq!(raw_document(raw).unwrap().as_bytes(), raw);
        assert!(raw_document(b"steps\xff")
            .unwrap_err()
            .to_string()
            .contains("UTF-8"));
    }

    #[test]
    fn install_tool_source_validation_keeps_slug_and_accepts_github() {
        assert_eq!(
            install_source(&json!({"slug": " owner/skill "})).unwrap(),
            InstallSource::Registry("owner/skill")
        );
        assert_eq!(
            install_source(&json!({"github": "owner/repo", "path": "skills/release"})).unwrap(),
            InstallSource::GitHub {
                repository: "owner/repo",
                path: "skills/release",
                reference: None,
                name: None,
            }
        );
        assert_eq!(
            install_source(&json!({
                "github": "owner/repo", "path": "skills/release", "ref": "v1", "name": "local",
            }))
            .unwrap(),
            InstallSource::GitHub {
                repository: "owner/repo",
                path: "skills/release",
                reference: Some("v1"),
                name: Some("local"),
            }
        );
    }

    #[test]
    fn install_tool_rejects_bad_sources_before_network_or_store_access() {
        for args in [
            json!({}),
            json!({"github": "owner/repo"}),
            json!({"path": "skills/release"}),
            json!({"ref": "main"}),
            json!({"name": "local"}),
            json!({"slug": "owner/skill", "github": "owner/repo", "path": "skills/release"}),
            json!({"slug": "owner/skill", "path": "skills/release"}),
            json!({"slug": "owner/skill", "ref": "main"}),
            json!({"slug": "owner/skill", "name": "local"}),
            json!({"slug": " "}),
            json!({"github": "owner/repo", "path": ""}),
            json!({"github": "owner/repo", "path": "skills/release", "ref": " "}),
            json!({"github": "owner/repo", "path": "skills/release", "name": null}),
            json!({"slug": 7}),
            json!({"slug": "owner/skill", "replace": true}),
            json!([]),
        ] {
            assert!(install_source(&args).is_err(), "must reject {args}");
            assert!(
                SkillInstall.execute(&args).is_err(),
                "runtime must reject {args}"
            );
        }
    }

    #[test]
    fn install_schema_requires_exactly_one_complete_source() {
        let schema = SkillInstall.parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("replace").is_none());
        assert_eq!(schema["oneOf"][0]["required"], json!(["slug"]));
        assert_eq!(
            schema["oneOf"][0]["not"]["anyOf"],
            json!([
                {"required": ["github"]}, {"required": ["path"]},
                {"required": ["ref"]}, {"required": ["name"]},
            ])
        );
        assert_eq!(schema["oneOf"][1]["required"], json!(["github", "path"]));
        assert_eq!(schema["oneOf"][1]["not"], json!({"required": ["slug"]}));
    }

    #[test]
    fn raw_import_keeps_metadata_alias_provenance_and_refuses_replacement() {
        let _lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct Fixture {
            path: std::path::PathBuf,
            previous_home: Option<std::ffi::OsString>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                match &self.previous_home {
                    Some(value) => std::env::set_var("AIZEN_HOME", value),
                    None => std::env::remove_var("AIZEN_HOME"),
                }
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aizen-raw-skill-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        // macOS temp paths commonly traverse /var -> /private/var. The store refuses any symlinked
        // ancestor, so the fixture must be canonical before it becomes AIZEN_HOME — canonicalizing
        // the test root never relaxes that production guard.
        #[cfg(unix)]
        let path = std::fs::canonicalize(&path).unwrap();
        let fixture = Fixture {
            path,
            previous_home: std::env::var_os("AIZEN_HOME"),
        };
        std::env::set_var("AIZEN_HOME", &fixture.path);
        let alias = format!("import-alias-{nonce}");
        let raw = "---\r\nname: Upstream title\r\ndescription: >-\r\n  Folded summary\r\n  preserved\r\nrequires: [file_read]\r\nplatforms: [windows, unix]\r\nextra:\r\n  retained: true\r\n---\r\nRead references/guide.md.\r\n";
        let provenance = || crate::skills::SkillProvenance {
            source: "https://example.com/original.md".to_string(),
            repository: None,
            path: None,
            sha: None,
        };
        let installed = import_raw_document("fallback", raw, Some(&alias), provenance()).unwrap();
        assert_eq!(installed.key(), alias);
        assert_eq!(installed.name, "Upstream title");
        assert_eq!(installed.requires, ["file_read"]);
        assert_eq!(installed.platforms, ["windows", "unix"]);
        let entry = &installed.location.as_ref().unwrap().entry;
        assert_eq!(std::fs::read(entry).unwrap(), raw.as_bytes());
        let state = crate::skills::skills_dir()
            .join(".state")
            .join(format!("{alias}.json"));
        let state_before = std::fs::read(&state).unwrap();
        assert!(String::from_utf8(state_before.clone())
            .unwrap()
            .contains("https://example.com/original.md"));
        assert!(
            import_raw_document("fallback", "Replacement steps", Some(&alias), provenance())
                .is_err()
        );
        assert_eq!(std::fs::read(entry).unwrap(), raw.as_bytes());
        assert_eq!(std::fs::read(&state).unwrap(), state_before);
        let report = install_report(&installed);
        assert!(report.contains(&alias));
        assert!(report.contains(&entry.to_string_lossy().to_string()));
        assert!(report.contains("not run automatically"));

        let zone = crate::skills::project_zone_dir();
        std::fs::create_dir_all(&zone).unwrap();
        std::fs::write(zone.join(format!("{alias}.md")), "Zone override steps").unwrap();
        let shadowed = install_report(&installed);
        assert!(shadowed.contains("shadowed") && shadowed.contains("project zone"));
        assert!(shadowed.contains("not active here"));
        std::fs::create_dir(zone.join(&alias)).unwrap();
        std::fs::write(zone.join(&alias).join("SKILL.md"), "Ambiguous bundle steps").unwrap();
        let ambiguous = install_report(&installed);
        assert!(ambiguous.contains("active copy could not be determined"));

        for (index, invalid) in ["---\nname: Empty\n---\n", "---\nname: [invalid\n---\nSteps"]
            .into_iter()
            .enumerate()
        {
            let key = format!("invalid-{nonce}-{index}");
            assert!(import_raw_document("fallback", invalid, Some(&key), provenance()).is_err());
            assert!(!crate::skills::skills_dir()
                .join(format!("{key}.md"))
                .exists());
        }
    }

    fn skill(owner: &str, repo: &str, sha: &str, path: &str) -> RegistrySkill {
        RegistrySkill {
            github_owner: owner.into(),
            github_repo: repo.into(),
            github_sha: sha.into(),
            github_path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn raw_url_pins_to_sha_then_branch() {
        let s = skill(
            "NousResearch",
            "hermes-agent",
            "abc123",
            "skills/dev/spike/SKILL.md",
        );
        assert_eq!(
            s.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/NousResearch/hermes-agent/abc123/skills/dev/spike/SKILL.md")
        );
        // no sha → branch
        let mut b = skill("o", "r", "", "p.md");
        b.github_branch = "dev".into();
        assert_eq!(
            b.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/o/r/dev/p.md")
        );
        // no sha + no branch → main
        let m = skill("o", "r", "", "p.md");
        assert_eq!(
            m.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/o/r/main/p.md")
        );
        // missing coordinates → None
        assert_eq!(RegistrySkill::default().raw_url(), None);
    }

    /// The slug is one path segment, however many slashes it contains. If it ever stopped being
    /// one, `a/../b` would address something other than the skill named `a/../b`.
    #[test]
    fn a_slug_is_one_encoded_segment() {
        assert_eq!(
            by_slug_url("https://agentskill.sh", "agentskill-sh/learn")
                .unwrap()
                .as_str(),
            "https://agentskill.sh/api/agent/skills/agentskill-sh%2Flearn/install"
        );
        // A base with its own path keeps it, and a trailing slash does not become an empty segment.
        assert_eq!(
            by_slug_url("https://reg.example.com/aizen/", "o/n")
                .unwrap()
                .as_str(),
            "https://reg.example.com/aizen/api/agent/skills/o%2Fn/install"
        );
        // Traversal is data, not structure.
        assert_eq!(
            by_slug_url("https://agentskill.sh", "../../etc")
                .unwrap()
                .as_str(),
            "https://agentskill.sh/api/agent/skills/..%2F..%2Fetc/install"
        );
    }

    /// The fallback matches on what a row says about itself, and only on all of the words —
    /// otherwise a two-word query returns everything that shares its commonest half.
    #[test]
    fn the_browse_fallback_matches_every_word_or_none() {
        let mut sk = RegistrySkill {
            name: "frontend-design".into(),
            slug: "bytedance/frontend-design".into(),
            description: "Create distinctive, production-grade interfaces".into(),
            ..Default::default()
        };
        let words =
            |q: &str| -> Vec<String> { q.split_whitespace().map(str::to_lowercase).collect() };
        assert!(matches(&sk, &words("frontend")));
        assert!(
            matches(&sk, &words("FRONTEND")),
            "case is not a distinction"
        );
        assert!(matches(&sk, &words("frontend interfaces")));
        assert!(
            matches(&sk, &words("bytedance")),
            "the owner is part of the row"
        );
        assert!(
            !matches(&sk, &words("frontend rust")),
            "both words or neither"
        );
        assert!(!matches(&sk, &words("kubernetes")));
        // A row that says nothing about itself matches nothing but its own name.
        sk.description = String::new();
        assert!(!matches(&sk, &words("interfaces")));
    }

    /// The listing's paging flag is read, so the fallback stops at the end of the catalogue rather
    /// than asking twelve times for a page that is not there.
    #[test]
    fn the_listing_says_whether_another_page_exists() {
        let body = r#"{"data":[],"total":4,"page":2,"limit":100,"hasMore":true}"#;
        let r: SearchResponse = serde_json::from_str(body).unwrap();
        assert!(r.has_more);
        let last: SearchResponse = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(!last.has_more, "a response that omits it has no more pages");
    }

    /// The by-slug record is the whole install: markdown in hand, no GitHub round trip.
    #[test]
    fn a_direct_record_carries_the_markdown() {
        let body = r#"{"slug":"o/n","name":"learn","skillMd":"---\nname: learn\n---\nsteps",
            "installCount":5,"securityScore":90}"#;
        let d: DirectSkill = serde_json::from_str(body).unwrap();
        assert_eq!(d.name, "learn");
        assert!(d.skill_md.contains("steps"));
        // A record without a body is not an install — `by_slug` treats it as a miss.
        let empty: DirectSkill = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert!(empty.skill_md.is_empty());
    }

    #[test]
    fn id_prefers_slug_then_owner_name() {
        let mut s = RegistrySkill {
            name: "spike".into(),
            owner: "Nous".into(),
            ..Default::default()
        };
        assert_eq!(s.id(), "Nous/spike");
        s.slug = "Nous/spike-v2".into();
        assert_eq!(s.id(), "Nous/spike-v2");
    }

    #[test]
    fn search_response_parses_camel_case_and_tolerates_missing_fields() {
        let body = r#"{"data":[{"name":"spike","owner":"NousResearch","slug":"NousResearch/spike",
            "description":"throwaway experiments","securityScore":100,"contentQualityScore":83,
            "githubOwner":"NousResearch","githubRepo":"hermes-agent","githubSha":"deadbeef",
            "githubPath":"skills/spike/SKILL.md"}],"total":142}"#;
        let parsed: SearchResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.total, 142);
        assert_eq!(parsed.data.len(), 1);
        let s = &parsed.data[0];
        assert_eq!(s.security_score, Some(100));
        assert_eq!(s.content_quality_score, Some(83));
        assert_eq!(s.id(), "NousResearch/spike");
        assert!(s
            .raw_url()
            .unwrap()
            .ends_with("/deadbeef/skills/spike/SKILL.md"));
        // a sparse object (only name) must not fail
        let sparse: SearchResponse = serde_json::from_str(r#"{"data":[{"name":"x"}]}"#).unwrap();
        assert_eq!(sparse.data[0].name, "x");
    }

    /// Offline SSRF wiring test: a loopback registry base must be refused by the net_guard floor
    /// BEFORE any network I/O (literal private/loopback hosts reject synchronously, no DNS).
    /// A plain #[test] + local block_on so the env lock is never held across an await point.
    #[test]
    fn search_refuses_a_private_registry_base() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-registry-ssrf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);
        let mut cfg = crate::core::cli_config::load();
        cfg.skill_registry = Some("http://127.0.0.1:9".to_string());
        crate::core::cli_config::save(&cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(search("anything", 5));
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let err = res.expect_err("a loopback registry base must be refused");
        assert!(
            format!("{err:#}").contains("SSRF"),
            "refused by the SSRF floor: {err:#}"
        );
    }

    #[test]
    fn install_tool_is_destructive_search_is_not() {
        assert!(
            SkillInstall.is_destructive(),
            "installing third-party instructions must be approval-gated"
        );
        assert!(!SkillSearch.is_destructive());
        assert!(
            SkillSearch.is_concurrency_safe(),
            "read-only marketplace query parallelizes"
        );
        assert!(
            !SkillInstall.is_concurrency_safe(),
            "installs stay serial (writes to disk)"
        );
    }
}
