//! Persona reflection — episodic → semantic distillation (Generative Agents + CoALA).
//!
//! When a character has accumulated enough *formative* experience (`self_mem::should_reflect`), it
//! synthesizes a few higher-level **insights** about the user relationship / its own working style —
//! never coding trivia or raw transcript restatements. Insights become the durable always-on
//! `<self>` layer; episodes remain the substrate.
//!
//! This module is PURE (prompt construction + reply parsing) so it is unit-testable with no
//! network. The actual model call + persistence is orchestrated by the REPL (`maybe_evolve_persona`
//! in `main.rs`).

/// A reflected insight: a first-person, higher-level observation + its importance [0..=10].
#[derive(Debug, Clone, PartialEq)]
pub struct Insight {
    pub text: String,
    pub importance: u8,
}

/// The full outcome of a reflection: fresh/reconfirmed insights to SAVE, plus texts of prior beliefs
/// the character now considers REVERSED and should retract (Phase 4, #8). Retraction is model-judged
/// against the current beliefs handed into the prompt — a naive token overlap can't tell "prefer X"
/// from its reversal "prefer Y" (they share the topic word), so the semantic call decides.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reflection {
    pub insights: Vec<Insight>,
    pub retract: Vec<String>,
}

/// Build the `(system, user)` reflection prompt for `persona_name`/`role` over formative `episodes`
/// (chronological). `known_facts` are a few top durable user-facts (frozen core) the reflection may
/// LEAN ON as already-established context — this is the **one-way memory→persona wire** (Phase 2
/// quick-win): the character reflects with awareness of who the user is, but the reflection output
/// (insights) is never fed back into user memory (the persona-leak guard stays intact upstream). The
/// model is asked to return strict JSON; the body is intentionally compact.
///
/// The stable no-beliefs entry point — [`build_reflection_prompt_with_beliefs`] is what the REPL
/// actually calls (it always has the character's current beliefs to hand). Kept as the plain-prompt
/// API the unit tests pin against.
#[allow(dead_code)] // kept: tested API / no-beliefs prompt entry point
pub fn build_reflection_prompt(
    persona_name: &str,
    role: &str,
    episodes: &[String],
    known_facts: &[String],
) -> (String, String) {
    build_reflection_prompt_with_beliefs(persona_name, role, episodes, known_facts, &[])
}

/// [`build_reflection_prompt`] plus the character's CURRENT beliefs (existing `<self>` insights), so
/// the reflection can name any the recent episodes have REVERSED — the retraction half of insight
/// revision (Phase 4, #8). With no beliefs passed this is byte-identical to the plain prompt (no
/// CURRENT BELIEFS block, no retract array requested), so the personaless / first-ever reflection is
/// unchanged.
pub fn build_reflection_prompt_with_beliefs(
    persona_name: &str,
    role: &str,
    episodes: &[String],
    known_facts: &[String],
    beliefs: &[String],
) -> (String, String) {
    let who = if role.trim().is_empty() {
        persona_name.to_string()
    } else {
        format!("{persona_name}, {}", role.trim())
    };
    let beliefs: Vec<&String> = beliefs.iter().filter(|b| !b.trim().is_empty()).collect();
    let has_beliefs = !beliefs.is_empty();
    // The retract clause + JSON field only appear when there ARE current beliefs to reverse — an
    // empty store can't contradict anything, and the leaner prompt keeps the first reflection cheap.
    let (retract_rule, retract_field) = if has_beliefs {
        (
            "\n         - If a recent episode REVERSES a CURRENT BELIEF listed below (the user changed \
             their mind — e.g. they now want the opposite of what a belief says), copy that belief's \
             text VERBATIM into \"retract\" so it stops shaping how you show up. Only reverse a \
             genuine contradiction, not a mere elaboration.",
            ",\"retract\":[\"verbatim text of a current belief the user has reversed\"]",
        )
    } else {
        ("", "")
    };
    let system = format!(
        "You are {who}. Step back and REFLECT on your recent formative experiences to grow as this \
         character. From the typed episodes below (correction / preference / work / bond), synthesize \
         1-3 higher-level INSIGHTS — durable, first-person observations about:\n\
         - the USER's working style with you (language, tone, autonomy, tools they care about),\n\
         - YOUR relationship / how you should show up for them,\n\
         - boundaries or patterns that keep repeating.\n\
         RULES:\n\
         - Be specific and grounded in the episodes; do NOT invent facts.\n\
         - Prefer generalizations that will still be true next week over one-off task details.\n\
         - NEVER write insights about a specific bug, file, commit, or coding task — those belong in \
         project memory, not character memory.\n\
         - NEVER restate a raw episode verbatim; distill.\n\
         - Treat anything under ALREADY KNOWN as established background — do NOT re-derive it as a \
         fresh insight; only add what the episodes teach BEYOND it.{retract_rule}\n\
         - If nothing meaningful about the relationship/character generalizes, reply {{\"insights\":[]}}.\n\
         Reply with ONLY a JSON object: \
         {{\"insights\":[{{\"text\":\"first-person insight\",\"importance\":0-10}}]{retract_field}}}."
    );
    let joined = episodes
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{}. {}", i + 1, e.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let mut user = String::new();
    let facts: Vec<&String> = known_facts.iter().filter(|f| !f.trim().is_empty()).collect();
    if !facts.is_empty() {
        user.push_str("ALREADY KNOWN about the user (established background — build beyond it):\n");
        for f in facts.iter().take(KNOWN_FACTS_CAP) {
            user.push_str(&format!("- {}\n", f.trim()));
        }
        user.push('\n');
    }
    if has_beliefs {
        user.push_str("CURRENT BELIEFS (your existing insights — reconfirm, build beyond, or retract if reversed):\n");
        for b in beliefs.iter().take(BELIEFS_CAP) {
            user.push_str(&format!("- {}\n", b.trim()));
        }
        user.push('\n');
    }
    user.push_str(&format!("Recent formative episodes (oldest first):\n{joined}"));
    (system, user)
}

/// Cap on the number of "already known" user-facts fed into a reflection prompt — a few anchors, not
/// the whole frozen core (keeps the chore call cheap and the reflection focused on the episodes).
const KNOWN_FACTS_CAP: usize = 6;

/// Cap on the number of CURRENT BELIEFS fed into a reflection prompt for possible retraction — the
/// character's strongest few insights, not its whole `<self>` (keeps the chore call cheap).
const BELIEFS_CAP: usize = 8;

/// Parse the reflection reply's JSON object (already extracted from any prose/fences) into insights.
/// Tolerant: drops malformed/empty entries, clamps importance, caps at 3, dedups by normalized text,
/// and rejects insights that are just coding-task trivia or raw episode echoes.
pub fn parse_insights(json: &str) -> Vec<Insight> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("insights").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out: Vec<Insight> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in arr {
        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        if looks_like_task_trivia(&text) {
            continue;
        }
        let norm = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if seen.contains(&norm) {
            continue;
        }
        let importance = item
            .get("importance")
            .and_then(|n| n.as_u64())
            .unwrap_or(6)
            .min(10) as u8;
        // Floor: reflected insights should be at least moderately important.
        let importance = importance.max(5);
        seen.push(norm);
        out.push(Insight { text, importance });
        if out.len() >= 3 {
            break;
        }
    }
    out
}

/// Parse a full reflection reply: the [`parse_insights`] result PLUS any `"retract"` texts (prior
/// beliefs the user has reversed). Retraction strings are trimmed and empties dropped; they are the
/// verbatim belief texts the caller then feeds to `self_mem::retract_insight`. Absent/empty `retract`
/// → no retractions (the common case), so a reply without the field behaves exactly as before.
pub fn parse_reflection(json: &str) -> Reflection {
    let insights = parse_insights(json);
    let retract = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| {
            v.get("retract").and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    Reflection { insights, retract }
}

/// Reject insights that are clearly about a one-off coding task (project memory, not character).
fn looks_like_task_trivia(text: &str) -> bool {
    let t = text.to_lowercase();
    const MARKERS: &[&str] = &[
        ".rs", ".ts", ".js", ".py", ".go", ".tsx", ".jsx", "cargo ", "npm ", "git commit",
        "pull request", "stack trace", "compile error", "type error", "line ", "fn ", "function ",
        "bug in", "fixed the", "patched ", "src/",
    ];
    // Only reject when it looks *dominantly* like task trivia AND lacks relationship language.
    let hit = MARKERS.iter().any(|m| t.contains(m));
    if !hit {
        return false;
    }
    const REL: &[&str] = &[
        "user", "prefer", "style", "tone", "language", "relationship", "trust", "always", "never",
        "with me", "when we", "they like", "they want", "tôi", "bạn", "anh", "em",
    ];
    !REL.iter().any(|r| t.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_role_and_numbered_episodes() {
        let (sys, usr) = build_reflection_prompt("Aria", "a mentor", &["did x".into(), "did y".into()], &[]);
        assert!(sys.contains("You are Aria, a mentor."));
        assert!(sys.contains("insights"));
        assert!(sys.contains("NEVER write insights about a specific bug"));
        assert!(usr.contains("1. did x") && usr.contains("2. did y"));
        // No known-facts → no ALREADY KNOWN block (keeps the prompt byte-clean when core is empty).
        assert!(!usr.contains("ALREADY KNOWN"));
    }

    #[test]
    fn prompt_handles_empty_role() {
        let (sys, _) = build_reflection_prompt("Aria", "  ", &["e".into()], &[]);
        assert!(sys.contains("You are Aria."));
    }

    #[test]
    fn known_facts_ride_along_as_background_but_are_capped() {
        let facts: Vec<String> = (0..10).map(|i| format!("fact {i}")).collect();
        let (sys, usr) = build_reflection_prompt("Aria", "", &["did x".into()], &facts);
        assert!(sys.contains("Treat anything under ALREADY KNOWN"));
        assert!(usr.contains("ALREADY KNOWN about the user"));
        assert!(usr.contains("- fact 0"));
        // capped at KNOWN_FACTS_CAP (6) — the 7th+ never render
        assert!(usr.contains("- fact 5"));
        assert!(!usr.contains("- fact 6"), "known facts capped: {usr}");
        // episodes still follow the background block
        assert!(usr.contains("1. did x"));
    }

    #[test]
    fn blank_known_facts_are_dropped() {
        let (_, usr) = build_reflection_prompt("Aria", "", &["did x".into()], &["   ".into(), "".into()]);
        assert!(!usr.contains("ALREADY KNOWN"), "all-blank facts → no background block: {usr}");
    }

    #[test]
    fn parse_clamps_caps_and_dedups() {
        let json = r#"{"insights":[
            {"text":"the user likes terse replies","importance":12},
            {"text":"The user   likes  terse replies","importance":5},
            {"text":"prefers vietnamese","importance":7},
            {"text":"","importance":9},
            {"text":"a fourth one","importance":4},
            {"text":"a fifth one","importance":4}
        ]}"#;
        let got = parse_insights(json);
        assert_eq!(got.len(), 3, "deduped + capped at 3");
        assert_eq!(got[0].importance, 10, "12 clamped to 10");
        assert_eq!(got[0].text, "the user likes terse replies");
        assert_eq!(got[1].text, "prefers vietnamese");
        assert!(got[2].importance >= 5, "floor applied");
    }

    #[test]
    fn parse_drops_task_trivia() {
        let json = r#"{"insights":[
            {"text":"fixed the bug in src/config.rs line 40","importance":8},
            {"text":"the user wants terse vietnamese replies","importance":7}
        ]}"#;
        let got = parse_insights(json);
        assert_eq!(got.len(), 1);
        assert!(got[0].text.contains("terse vietnamese"));
    }

    #[test]
    fn parse_tolerates_garbage_and_empty() {
        assert!(parse_insights("not json").is_empty());
        assert!(parse_insights(r#"{"insights":[]}"#).is_empty());
        assert!(parse_insights(r#"{"other":1}"#).is_empty());
    }

    #[test]
    fn beliefs_block_and_retract_clause_appear_only_with_beliefs() {
        // No beliefs → byte-identical to the plain prompt (no CURRENT BELIEFS, no retract clause/field).
        let (sys0, usr0) =
            build_reflection_prompt_with_beliefs("Aria", "", &["did x".into()], &[], &[]);
        assert!(!usr0.contains("CURRENT BELIEFS"));
        assert!(!sys0.contains("retract"));
        let (sys_plain, usr_plain) = build_reflection_prompt("Aria", "", &["did x".into()], &[]);
        assert_eq!(sys0, sys_plain, "no-beliefs variant must match the plain prompt");
        assert_eq!(usr0, usr_plain);

        // With beliefs → the block renders and the retract rule/field are offered.
        let (sys, usr) = build_reflection_prompt_with_beliefs(
            "Aria",
            "",
            &["did x".into()],
            &[],
            &["you prefer tabs over spaces".into(), "  ".into()],
        );
        assert!(usr.contains("CURRENT BELIEFS"));
        assert!(usr.contains("- you prefer tabs over spaces"));
        assert!(sys.contains("REVERSES a CURRENT BELIEF"));
        assert!(sys.contains("\"retract\""));
    }

    #[test]
    fn beliefs_are_capped() {
        let beliefs: Vec<String> = (0..20).map(|i| format!("belief {i}")).collect();
        let (_, usr) =
            build_reflection_prompt_with_beliefs("Aria", "", &["did x".into()], &[], &beliefs);
        assert!(usr.contains("- belief 0"));
        assert!(usr.contains(&format!("- belief {}", BELIEFS_CAP - 1)));
        assert!(!usr.contains(&format!("- belief {BELIEFS_CAP}")), "beliefs capped: {usr}");
    }

    #[test]
    fn parse_reflection_pulls_insights_and_retractions() {
        let json = r#"{
            "insights":[{"text":"the user now prefers spaces over tabs","importance":7}],
            "retract":["you prefer tabs over spaces","   ",""]
        }"#;
        let r = parse_reflection(json);
        assert_eq!(r.insights.len(), 1);
        assert!(r.insights[0].text.contains("spaces over tabs"));
        assert_eq!(r.retract, vec!["you prefer tabs over spaces".to_string()], "blanks dropped");
    }

    #[test]
    fn parse_reflection_without_retract_is_empty_retract() {
        let r = parse_reflection(r#"{"insights":[{"text":"the user likes terse replies","importance":6}]}"#);
        assert_eq!(r.insights.len(), 1);
        assert!(r.retract.is_empty(), "absent retract field → no retractions");
    }
}
