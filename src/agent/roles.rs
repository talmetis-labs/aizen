//! The Pantheon — aizen's built-in sub-agent roles as ONE declarative table.
//!
//! Seven roles: `argus` (searcher) · `metis` (planner) · `daedalus` (coder) · `nemesis`
//! (reviewer) · `themis` (tester) · `clio` (librarian) · `mnemosyne` (historian — session and
//! memory recall). Each entry carries the role's name, the legacy names still accepted on the
//! wire, the one-line brief for the sub-agent prompt, and the capability grants beyond the shared
//! read-only base. `builtin::role_registry`, the prompt's role brief, the `<project_context>`
//! switch, and the writer/parallelism classification all derive from this table, so a new role is
//! one entry here — not four hand-synced `match` arms drifting apart.
//!
//! Naming: the mythological name is the product identity; the FUNCTION is what a routing model
//! needs. Every brief therefore leads with `name (function) —` so the semantics travel with the
//! name into every prompt and result header. Legacy aliases (`coder`, `planner`, `reviewer`,
//! `tester`) are kept **forever**: an alias is one table row, and dropping it would break saved
//! workflow specs, user scripts, and `mcp serve`'s defaults for zero benefit.

/// The three access classes a dispatch can hold. Derived from the grant flags
/// ([`RoleProfile::access`]) so classification can never disagree with the actual registry:
/// - `ReadOnly` → safe to fan out in parallel;
/// - `Execute` → may run processes but cannot edit source; NOT parallel-safe on a shared tree;
/// - `Write` → edits files and runs commands; at most one per workflow (the singular writer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessClass {
    ReadOnly,
    Execute,
    Write,
}

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
    /// `session_recall` — read-only access to recent same-project conversations (the historian's
    /// instrument; other roles get memory search but not transcript recall).
    pub history: bool,
    /// Include `<project_context>` (build/test conventions) — the roles whose job is building
    /// and testing pay for it; investigation roles don't.
    pub project_context: bool,
    /// Default total step budget for a dispatch that doesn't pass `max_steps`. Sized to the
    /// work: a locate job (argus) is done in a dozen steps or it is lost, while an implement
    /// job (daedalus) reads, edits, builds and re-edits. The profile — not a scattered
    /// constant — answers the question; the `task` tool and workflow children both read it.
    pub default_max_steps: usize,
    /// The role's working-method prompt (embedded at compile time from `roles/<name>.md`),
    /// rendered inside `<role>` after the brief. A sub-agent prompt is paid uncached per
    /// dispatch, so each file stays tight: method, then the report contract — no lore.
    pub prompt: &'static str,
}

impl RoleProfile {
    /// The typed access class, derived from the grant flags (see [`AccessClass`]).
    pub fn access(&self) -> AccessClass {
        if self.edit {
            AccessClass::Write
        } else if self.shell {
            AccessClass::Execute
        } else {
            AccessClass::ReadOnly
        }
    }
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
        history: false,
        project_context: true,
        default_max_steps: 45,
        prompt: include_str!("roles/daedalus.md"),
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
        history: false,
        project_context: false,
        default_max_steps: 15,
        prompt: include_str!("roles/argus.md"),
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
        history: false,
        project_context: false,
        default_max_steps: 25,
        prompt: include_str!("roles/metis.md"),
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
        history: false,
        project_context: false,
        default_max_steps: 25,
        prompt: include_str!("roles/nemesis.md"),
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
        history: false,
        project_context: true,
        default_max_steps: 30,
        prompt: include_str!("roles/themis.md"),
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
        history: false,
        project_context: false,
        default_max_steps: 20,
        prompt: include_str!("roles/clio.md"),
    },
    RoleProfile {
        name: "mnemosyne",
        aliases: &["historian"],
        brief: "mnemosyne (historian) — recover prior decisions and project/session history: what \
                was decided, when, and whether it was later revised. Tools: memory search/list + \
                session_recall (recent same-project conversations) + read/glob files; READ-ONLY: \
                no web, no edits, no shell, no memory writes.",
        web: false,
        shell: false,
        edit: false,
        history: true,
        project_context: false,
        default_max_steps: 25,
        prompt: include_str!("roles/mnemosyne.md"),
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
            ("historian", "mnemosyne"),
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
    fn every_role_ships_a_tight_working_method_prompt() {
        for p in ROLES {
            assert!(p.prompt.contains("## Working method"), "{}", p.name);
            assert!(p.prompt.contains("## Report"), "{}", p.name);
            // Paid uncached on every dispatch — a role prompt that balloons is a cost regression.
            assert!(
                p.prompt.len() < 2_000,
                "{}: {} bytes — keep it tight",
                p.name,
                p.prompt.len()
            );
        }
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

    #[test]
    fn access_classes_derive_from_the_grants() {
        for p in ROLES {
            let want = match p.name {
                "daedalus" => AccessClass::Write,
                "themis" => AccessClass::Execute,
                _ => AccessClass::ReadOnly,
            };
            assert_eq!(p.access(), want, "{}", p.name);
        }
        // Exactly one Write in the whole table — the singular-writer invariant at the source.
        let writers = ROLES
            .iter()
            .filter(|p| p.access() == AccessClass::Write)
            .count();
        assert_eq!(writers, 1);
        // History (session_recall) is the historian's instrument alone.
        for p in ROLES {
            assert_eq!(p.history, p.name == "mnemosyne", "{}", p.name);
        }
    }

    #[test]
    fn budgets_are_sized_to_the_work() {
        let budget = |n: &str| canonical(n).unwrap().default_max_steps;
        // Locate < research < plan/review/recall < test < implement. A searcher that has not
        // found its symbol in fifteen steps is lost; an implementer that has to build and
        // re-edit needs three times that. Seven identical 25s was the audit's O4.
        assert_eq!(budget("argus"), 15);
        assert_eq!(budget("clio"), 20);
        assert_eq!(budget("metis"), 25);
        assert_eq!(budget("nemesis"), 25);
        assert_eq!(budget("mnemosyne"), 25);
        assert_eq!(budget("themis"), 30);
        assert_eq!(budget("daedalus"), 45);
        for p in ROLES {
            assert!(
                (1..=crate::agent::task_tool::MAX_STEP_BUDGET).contains(&p.default_max_steps),
                "{}: default must fit under the shared cap",
                p.name
            );
        }
    }

    #[test]
    fn briefs_carry_no_runs_of_spaces() {
        // A missing `\` continuation in a string literal ships its indentation to every
        // dispatch (mnemosyne once carried three runs of seventeen spaces).
        for p in ROLES {
            assert!(
                !p.brief.contains("  "),
                "{}: run of spaces in brief: {:?}",
                p.name,
                p.brief
            );
        }
    }
}
