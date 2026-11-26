//! Request-local workflow advice, never an execution mode or a completion gate.
//! Cheap cues suggest an artifact; only the model/user can judge actual system boundaries.

use crate::core::turn_shape::{self, TurnShape};

pub(crate) const LARGE_TASK_FILES: usize = 3;
const MAX_REQUEST_CHARS: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Direct,
    Todo,
    Plan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    Small,
    MultipleSteps,
    MultipleFiles,
    Growth,
    PlanRequested,
    Migration,
    PublicApi,
    MultipleSystems,
    HardToReverse,
}

impl Reason {
    fn text(self) -> &'static str {
        match self {
            Self::Small => "small or exploratory request",
            Self::MultipleSteps => "multiple requested steps",
            Self::MultipleFiles => "request-local multi-file scope",
            Self::Growth => "work grew to three distinct changed files",
            Self::PlanRequested => "requested durable plan or multi-phase audit",
            Self::Migration => "migration scope",
            Self::PublicApi => "public API and callers",
            Self::MultipleSystems => "explicit cross-system scope",
            Self::HardToReverse => "hard-to-reverse scope",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Advice {
    level: Level,
    reason: Reason,
}

/// This bounded detector is advisory only. Ignore architect prose, quoted lines and fenced data;
/// neither retrieved instructions nor a planner draft is the user's requested scope.
fn request_text(request: &str) -> String {
    let brief = request
        .strip_prefix("<architect_plan>")
        .and_then(|s| s.split_once("</architect_plan>"))
        .map_or(request, |(_, brief)| brief);
    let bounded: String = brief.chars().take(MAX_REQUEST_CHARS).collect();
    let mut fence: Option<(char, usize)> = None;
    let mut text = String::new();
    for line in bounded.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            let marker = line.chars().next().unwrap();
            let width = line.chars().take_while(|&c| c == marker).count();
            if let Some((opening, minimum)) = fence {
                // A closing fence may be longer, but not shorter or followed by an info string.
                if marker == opening && width >= minimum && line[width..].trim().is_empty() {
                    fence = None;
                }
            } else {
                fence = Some((marker, width));
            }
            continue;
        }
        if fence.is_none() && !line.starts_with(['>', '"', '\'']) {
            let mut quote = None;
            let mut previous = None;
            for c in line.chars() {
                if let Some(delimiter) = quote {
                    if c == delimiter {
                        quote = None;
                    }
                } else if matches!(c, '"' | '`')
                    || (c == '\'' && previous.is_none_or(|p| !word_char(p)))
                {
                    quote = Some(c);
                    text.push(' ');
                } else {
                    text.push(c);
                }
                previous = Some(c);
            }
            text.push('\n');
        }
    }
    text.to_lowercase()
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Match whole phrases and reject nearby explicit negation in the same clause. This is deliberately
/// conservative, not a natural-language permission parser (and must never become one).
fn positive(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| {
        text.match_indices(phrase).any(|(start, matched)| {
            let before = &text[..start];
            let after = &text[start + matched.len()..];
            if before.chars().next_back().is_some_and(word_char)
                || after.chars().next().is_some_and(word_char)
            {
                return false;
            }
            let clause = before.rsplit(['.', ';', ',', '\n']).next().unwrap_or("");
            !clause.split_whitespace().rev().take(4).any(|word| {
                matches!(
                    word,
                    "no" | "not"
                        | "don't"
                        | "without"
                        | "avoid"
                        | "không"
                        | "đừng"
                        | "chưa"
                        | "khỏi"
                )
            })
        })
    })
}

/// Informational heads stay exploratory even when their subject contains an edit verb.
/// This deliberately differs from the precision-first deferred-tool classifier.
fn exploratory(text: &str) -> bool {
    fn starts(text: &str, phrases: &[&str]) -> bool {
        phrases.iter().any(|prefix| {
            text.strip_prefix(prefix)
                .is_some_and(|rest| rest.chars().next().is_none_or(|c| !word_char(c)))
        })
    }
    let mut text = text.trim();
    while let Some(rest) = [
        "also ",
        "please ",
        "then ",
        "and ",
        "can you ",
        "could you ",
        "sau đó ",
        "đồng thời ",
        "hãy ",
        "vui lòng ",
    ]
    .iter()
    .find_map(|prefix| text.strip_prefix(prefix))
    {
        text = rest.trim_start();
    }
    let shape = turn_shape::classify(text);
    if shape == TurnShape::Question
        || starts(
            text,
            &[
                "how",
                "what",
                "why",
                "when",
                "where",
                "which",
                "who",
                "should i",
                "should we",
                "can i",
                "can we",
                "could i",
                "could we",
                "explain",
                "describe",
                "giải thích",
                "làm thế nào",
                "làm sao",
                "như thế nào",
                "tại sao",
                "vì sao",
                "có nên",
            ],
        )
    {
        return true;
    }
    let research = shape == TurnShape::Research
        || starts(
            text,
            &[
                "research",
                "investigate",
                "analyze",
                "analyse",
                "compare",
                "review",
                "audit",
                "explore",
                "survey",
                "evaluate",
                "assess",
                "benchmark",
                "look into",
                "find out",
                "nghiên cứu",
                "tìm hiểu",
                "phân tích",
                "so sánh",
                "đánh giá",
                "rà soát",
                "khảo sát",
                "điều tra",
            ],
        );
    research
        && !positive(
            text,
            &[
                "multi-phase audit",
                "multiphase audit",
                "kiểm toán nhiều giai đoạn",
            ],
        )
}

/// Two bounded facts, not retained steering transcripts or durable task/permission state.
#[derive(Debug, Default)]
struct ApiScope {
    public_api: bool,
    callers: bool,
}

fn select(text: &str, scope: &mut ApiScope) -> Advice {
    let advice = |level, reason| Advice { level, reason };
    let multiple_steps = positive(
        text,
        &[
            "multi-step",
            "multistep",
            "several steps",
            "multiple steps",
            "nhiều bước",
            "then",
            "sau đó",
        ],
    );
    // Retain explicit implementation follow-ups, but not cues inside informational clauses.
    // Bounded by request_text; no transcript is retained across steering inputs.
    let scope_text = text
        .split_inclusive([';', '\n', '?'])
        .flat_map(|clause| clause.split(". "))
        .flat_map(|clause| clause.split(" then "))
        .flat_map(|clause| clause.split(" sau đó "))
        .filter(|clause| !exploratory(clause))
        .collect::<Vec<_>>()
        .join("\n");
    let text = scope_text.as_str();
    if text.trim().is_empty() {
        return advice(Level::Direct, Reason::Small);
    }
    let shape = turn_shape::classify(text);
    let deliverable = positive(
        text,
        &[
            "write a plan",
            "create a plan",
            "draft a plan",
            "write an implementation plan",
            "durable plan",
            "multi-phase audit",
            "multiphase audit",
            "lập kế hoạch",
            "viết kế hoạch",
            "lên plan",
            "viết plan",
            "kiểm toán nhiều giai đoạn",
        ],
    );
    if deliverable {
        return advice(Level::Plan, Reason::PlanRequested);
    }
    if matches!(shape, TurnShape::Question | TurnShape::Research) {
        return advice(Level::Direct, Reason::Small);
    }
    scope.public_api |= positive(text, &["public api", "api công khai"]);
    scope.callers |= positive(text, &["callers", "consumers", "chỗ gọi", "nơi gọi"]);
    for (reason, cues) in [
        (
            Reason::Migration,
            &[
                "migrate",
                "migration",
                "di chuyển dữ liệu",
                "chuyển đổi dữ liệu",
            ][..],
        ),
        (
            Reason::MultipleSystems,
            &[
                "multiple systems",
                "multi-system",
                "cross-system",
                "across systems",
                "nhiều hệ thống",
                "liên hệ thống",
            ][..],
        ),
        (
            Reason::HardToReverse,
            &[
                "hard to undo",
                "hard-to-reverse",
                "irreversible",
                "khó hoàn tác",
                "khó đảo ngược",
                "không thể hoàn tác",
            ][..],
        ),
    ] {
        if positive(text, cues) {
            return advice(Level::Plan, reason);
        }
    }
    if scope.public_api && scope.callers {
        return advice(Level::Plan, Reason::PublicApi);
    }
    if multiple_steps {
        return advice(Level::Todo, Reason::MultipleSteps);
    }
    if shape == TurnShape::MultiFile {
        return advice(Level::Todo, Reason::MultipleFiles);
    }
    advice(Level::Direct, Reason::Small)
}

fn plan_only(text: &str) -> bool {
    [
        "plan only",
        "plan-only",
        "don't implement",
        "do not implement",
        "no implementation",
        "chỉ lập kế hoạch",
        "chỉ lên plan",
        "chỉ lập plan",
        "chỉ viết plan",
        "chỉ viết spec",
        "chưa triển khai",
        "không triển khai",
        "đừng triển khai",
        "chưa code",
        "đừng code",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// Shared only by continuations of one live request. Never serialized, never permission state.
#[derive(Clone, Debug, Default)]
pub(crate) struct AdviceContext(std::sync::Arc<std::sync::Mutex<Option<AdviceState>>>);

#[derive(Debug)]
struct AdviceState {
    nudge: PlanNudge,
    // Only the threshold matters; bound retained paths even across repeated refreshes.
    edited_paths: Vec<std::path::PathBuf>,
}

/// An abandoned future or failed admission retires advice just like an ordinary terminal result.
pub(crate) struct AdviceRun {
    context: AdviceContext,
    continuing: bool,
}

impl AdviceRun {
    pub(crate) fn preserve_for_refresh(&mut self) {
        self.continuing = true;
    }
}

impl Drop for AdviceRun {
    fn drop(&mut self) {
        if !self.continuing {
            self.context.retire();
        }
    }
}

impl AdviceContext {
    pub(crate) fn run(&self) -> AdviceRun {
        AdviceRun {
            context: self.clone(),
            continuing: false,
        }
    }

    pub(crate) fn begin(&self, request: &str, planning_mode: bool) -> Self {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let advice = current.get_or_insert_with(|| AdviceState {
            nudge: PlanNudge::new(request, planning_mode),
            edited_paths: Vec::new(),
        });
        advice.nudge.suppressed |= planning_mode;
        self.clone()
    }

    fn retire(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Transfer a paused request out of the run config. Reusing that config for unrelated work
    /// cannot accidentally keep its budget; only the matching in-process clarify continuation can.
    pub(crate) fn take(&self) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(
            self.0.lock().unwrap_or_else(|e| e.into_inner()).take(),
        )))
    }

    pub(crate) fn resume(&self, paused: Self) {
        let state = paused.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }

    pub(crate) fn observe_request(&self, request: &str) {
        if let Some(advice) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            advice.nudge.observe_request(request);
        }
    }

    pub(crate) fn observe_task(&self, state: &super::todo::TaskState) {
        if let Some(advice) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            advice.nudge.observe_task(state);
        }
    }

    pub(crate) fn take_reminder(&self, edited_paths: &[std::path::PathBuf]) -> Option<String> {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let advice = current.as_mut()?;
        for path in edited_paths {
            if advice.edited_paths.len() >= LARGE_TASK_FILES {
                break;
            }
            if !advice.edited_paths.contains(path) {
                advice.edited_paths.push(path.clone());
            }
        }
        advice.nudge.take_reminder(advice.edited_paths.len())
    }
}

#[derive(Debug)]
pub(crate) struct PlanNudge {
    advice: Advice,
    api_scope: ApiScope,
    highest_emitted: Level,
    suppressed: bool,
    has_plan: bool,
    has_todos: bool,
}

impl PlanNudge {
    pub(crate) fn new(request: &str, planning_mode: bool) -> Self {
        let text = request_text(request);
        let mut api_scope = ApiScope::default();
        let advice = select(&text, &mut api_scope);
        Self {
            advice,
            api_scope,
            highest_emitted: Level::Direct,
            suppressed: planning_mode || plan_only(&text),
            has_plan: false,
            has_todos: false,
        }
    }

    /// Only explicit additional request input can strengthen scope. Tool/repository/model text
    /// must not call this. Advisory narrowing never changes the execution policy.
    pub(crate) fn observe_request(&mut self, request: &str) {
        let text = request_text(request);
        let next = select(&text, &mut self.api_scope);
        if next.level > self.advice.level {
            self.advice = next;
        }
        self.suppressed |= plan_only(&text);
    }

    /// Replace, do not accumulate: deleted, invalid or identity-changed attachments stop counting
    /// at the next refresh. Completed/historical rows cannot satisfy this request's TODO advice.
    pub(crate) fn observe_task(&mut self, state: &super::todo::TaskState) {
        self.has_plan = state.plan.as_ref().is_some_and(|plan| plan.usable());
        self.has_todos = state.active
            && !state.abandoned
            && state
                .items
                .iter()
                .any(|item| item.status != super::todo::Status::Done);
    }

    pub(crate) fn take_reminder(&mut self, files_touched: usize) -> Option<String> {
        if files_touched >= LARGE_TASK_FILES && self.advice.level == Level::Direct {
            self.advice = Advice {
                level: Level::Todo,
                reason: Reason::Growth,
            };
        }
        let level = self.advice.level;
        if self.suppressed
            || level <= self.highest_emitted
            || self.has_plan
            || (level == Level::Todo && self.has_todos)
        {
            return None;
        }
        self.highest_emitted = level;
        let instruction = match level {
            Level::Direct => return None,
            Level::Todo => "Use a short todo_write checklist (<=5 meaningful items) if this work needs tracking. A small reversible edit does not need a plan file. File count alone does not establish multiple systems.",
            Level::Plan => "Save a plan FIRST with permitted native file tools under docs/plans/: an aizen_plan: 1 header, stable plan ID and unique phase markers, requirements, scope/non-goals, design, acceptance criteria and verification; then track execution with todos. A TODO list, architect draft or suggested path is not a saved plan. Read a relevant existing plan instead of overwriting unrelated work. In planning mode only the controller-selected path and ID are permitted.",
        };
        Some(format!(
            "{} Workflow advice: {:?} ({}; at least {} files changed so far). {} Judge actual scope; this heuristic is not new permission, an edit gate, or completion evidence. Plan-only requests never authorize implementation.",
            super::PLAN_NUDGE_PREFIX, level, self.advice.reason.text(), files_touched, instruction,
        ))
    }
}

#[cfg(test)]
#[path = "plan_nudge_tests.rs"]
mod tests;
