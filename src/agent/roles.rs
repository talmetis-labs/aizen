//! The Pantheon — aizen's built-in sub-agent roles as ONE declarative table.
//!
//! Six roles: `argus` (searcher) · `metis` (planner) · `daedalus` (coder) · `nemesis` (reviewer) ·
//! `themis` (tester) · `clio` (librarian). Each entry carries the role's name, the legacy names
//! still accepted on the wire, the one-line brief for the sub-agent prompt, and the capability
//! grants beyond the shared read-only base. `builtin::role_registry`, the prompt's role brief, and
//! the `<project_context>` switch all derive from this table, so a new role is one entry here —
//! not four hand-synced `match` arms drifting apart.
//!
//! Naming: the mythological name is the product identity; the FUNCTION is what a routing model
//! needs. Every brief therefore leads with `name (function) —` so the semantics travel with the
//! name into every prompt and result header. Legacy aliases (`coder`, `planner`, `reviewer`,
//! `tester`) are kept **forever**: an alias is one table row, and dropping it would break saved
//! workflow specs, user scripts, and `mcp serve`'s defaults for zero benefit.

/// One built-in sub-agent role. Capability flags are grants ON TOP of the shared read-only base
/// (`builtin::subagent_read_only_base`): memory + read/glob/search + code intel + `git_inspect` +
/// a scoped scratch plan.
pub struct RoleProfile {
    /// The canonical role name (the Pantheon name).
    pub name: &'static str,
    /// Older/functional names still accepted anywhere a role name is read. Never removed.
    pub aliases: &'static [&'static str],
    /// One-line role brief appended to the sub-agent system prompt. Leads with the function —
    /// the mythological name is opaque to a routing model, the function is not. MUST stay
    /// consistent with the capability flags below (a read-only brief on a shell-granted role
    /// would lie to the child about its own hands).
    pub brief: &'static str,
    /// Web research tools (`web_search`/`web_fetch`/`web_crawl`).
    pub web: bool,
    /// `shell_run` + the scoped `process` pool (run builds/tests; no file editing implied).
    pub shell: bool,
    /// File mutation: `file_edit`/`file_write`/`file_move` + `skill_save`/refine + symbolic edit.
    pub edit: bool,
    /// Include `<project_context>` (build/test conventions) — the roles whose job is building
    /// and testing pay for it; investigation roles don't.
    pub project_context: bool,
}

/// The Pantheon, in dispatch-frequency order (the order routing surfaces list them in).
pub const ROLES: &[RoleProfile] = &[
    RoleProfile {
        name: "daedalus",
        aliases: &["coder"],
        brief: "daedalus (coder) — implement the change. Tools: read/glob/edit files, run shell \
                (build/test/search), memory. Read before editing; make sure your change compiles.",
        web: true,
        shell: true,
        edit: true,
        project_context: true,
    },
    RoleProfile {
        name: "argus",
        aliases: &["searcher"],
        brief: "argus (searcher) — locate code: files, symbols, call paths, usages. Return exact \
                file:line locations with just enough surrounding context to act on, not file dumps. \
                READ-ONLY: read/glob/search + code intel + git_inspect + memory; no web, no edits, \
                no shell.",
        web: false,
        shell: false,
        edit: false,
        project_context: false,
    },
    RoleProfile {
        name: "metis",
        aliases: &["planner"],
        brief: "metis (planner) — analyze root cause, architecture, and trade-offs; produce a \
                concrete, ordered plan. READ-ONLY: read/glob files + git_inspect + web + memory; \
                you cannot edit files or run shell.",
        web: true,
        shell: false,
        edit: false,
        project_context: false,
    },
    RoleProfile {
        name: "nemesis",
        aliases: &["reviewer"],
        brief: "nemesis (reviewer) — adversarial review: correctness, security, quality. Inspect \
                the actual change (git_inspect shows status/log/diff/blame) and report findings \
                with file:line. READ-ONLY: read/glob files + git_inspect + web + memory; you \
                cannot edit or run shell.",
        web: true,
        shell: false,
        edit: false,
        project_context: false,
    },
    RoleProfile {
        name: "themis",
        aliases: &["tester"],
        brief: "themis (tester) — run and analyze tests/builds/benchmarks; report verdicts with \
                the evidence (exact commands, exit codes, failing output). Tools: read/glob files, \
                run shell, git_inspect, memory; you cannot edit files.",
        web: true,
        shell: true,
        edit: false,
        project_context: true,
    },
    RoleProfile {
        name: "clio",
        aliases: &["librarian"],
        brief: "clio (librarian) — research OUTSIDE the repo: dependencies, upstream docs, \
                changelogs, third-party APIs. Tools: read/glob files + web search/fetch/crawl + \
                memory; cite URLs for every claim. READ-ONLY: you cannot edit or run shell.",
        web: true,
        shell: false,
        edit: false,
        project_context: false,
    },
];

/// Resolve a role name — canonical or legacy, case-insensitively — to its profile. `None` for an
/// unknown name (callers fall back to the conservative read-only base + a generic brief).
pub fn canonical(name: &str) -> Option<&'static RoleProfile> {
    let n = name.trim();
    ROLES.iter().find(|p| {
        p.name.eq_ignore_ascii_case(n) || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(n))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_resolves_by_canonical_and_legacy_name() {
        for (legacy, canon) in [
            ("coder", "daedalus"),
            ("searcher", "argus"),
            ("planner", "metis"),
            ("reviewer", "nemesis"),
            ("tester", "themis"),
            ("librarian", "clio"),
        ] {
            assert_eq!(canonical(legacy).map(|p| p.name), Some(canon), "{legacy}");
            assert_eq!(canonical(canon).map(|p| p.name), Some(canon), "{canon}");
            // Case-insensitive + whitespace-tolerant (a workflow spec hand-typed in Title Case).
            assert_eq!(
                canonical(&format!(" {} ", canon.to_ascii_uppercase())).map(|p| p.name),
                Some(canon)
            );
        }
        assert!(canonical("weird").is_none(), "unknown stays unknown");
        assert!(canonical("").is_none());
    }

    #[test]
    fn names_and_aliases_never_collide() {
        let mut seen = std::collections::HashSet::new();
        for p in ROLES {
            assert!(seen.insert(p.name), "duplicate role name {}", p.name);
            for a in p.aliases {
                assert!(seen.insert(*a), "alias {a} collides");
            }
        }
    }

    #[test]
    fn briefs_lead_with_the_name_and_match_capabilities() {
        for p in ROLES {
            assert!(
                p.brief.starts_with(p.name),
                "{}: brief must lead with the role name for attribution",
                p.name
            );
            // A brief that says READ-ONLY on a role holding shell or edit lies to the child.
            let claims_read_only = p.brief.contains("READ-ONLY");
            assert_eq!(
                claims_read_only,
                !(p.shell || p.edit),
                "{}: READ-ONLY claim disagrees with grants",
                p.name
            );
        }
    }

    #[test]
    fn only_daedalus_edits_and_only_builders_get_project_context() {
        for p in ROLES {
            assert_eq!(p.edit, p.name == "daedalus", "{}", p.name);
            assert_eq!(
                p.project_context, p.shell,
                "{}: project context rides with build/test capability",
                p.name
            );
        }
    }
}
