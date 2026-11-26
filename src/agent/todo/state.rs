//! Versioned task state. Model proposals and trusted runtime authority are separate.
use super::{Status, Todo};
use anyhow::{ensure, Result};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[path = "plan.rs"]
mod plan;
pub use plan::{ItemPhase, PlanAttachment};

const VERSION: u32 = 3;
const MAX_ITEMS: usize = 64;
const MAX_HISTORY: usize = 128;
const MAX_OBLIGATIONS: usize = 128;
const MAX_RECEIPTS: usize = 256;
const MAX_CONTENT: usize = 1024;
const MAX_TEXT: usize = 512;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Requirement {
    #[default]
    Unspecified,
    Observe {
        target: String,
    },
    Check {
        command: String,
        root: String,
        /// Runtime/persistence only; the model DTO never accepts fingerprints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_identity: Option<String>,
    },
    Report,
}

impl Requirement {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Unspecified | Self::Report => Ok(()),
            Self::Observe { target } => bounded(target, MAX_TEXT, "observation target"),
            Self::Check {
                command,
                root,
                source_identity,
            } => {
                bounded(command, MAX_TEXT, "check command")?;
                bounded(root, MAX_TEXT, "check root")?;
                if let Some(identity) = source_identity {
                    bounded(identity, 256, "source identity")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRequirement {
    pub item_id: String,
    pub revision: u64,
    pub requirement: Requirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequest {
    pub item_id: String,
    pub requirement_revision: u64,
}

/// Removed unfinished work is retained as abandonment. Empty item IDs identify native workspace
/// obligations; their exact root is required to discharge them. `None` means unknown coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskObligation {
    pub item_id: String,
    pub content: String,
    pub root: Option<String>,
    pub requirement_revision: u64,
    pub resolved: bool,
    pub abandoned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Fail,
    Unavailable,
    Cancelled,
    TimedOut,
    Deferred,
    Unstable,
    Unsettled,
}
impl CheckResult {
    pub fn accepted(self) -> bool {
        self == Self::Pass
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReceipt {
    pub version: u32,
    pub task_id: String,
    pub item_id: String,
    pub requirement_revision: u64,
    pub epoch: u64,
    pub command: String,
    pub root: String,
    pub source_identity: String,
    pub result: CheckResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationReceipt {
    pub version: u32,
    pub task_id: String,
    pub item_id: String,
    pub requirement_revision: u64,
    pub epoch: u64,
    pub target: String,
    pub path: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportReceipt {
    pub version: u32,
    pub task_id: String,
    pub item_id: String,
    pub requirement_revision: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionBasis {
    Observed,
    Reported,
    Verified,
    NotRequired,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    Completed,
    Incomplete,
    Interrupted,
    Failed,
    AwaitingInput,
    Abandoned,
    Cancelled,
    Deadline,
    MaxIterations,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishDisposition {
    Completed,
    Incomplete,
    Retained,
    Abandoned,
}

/// In-process capability, never deserialized. Captured before execution, consumed under the same
/// task lock as effects. The generation prevents restore/reset from resurrecting an old token.
#[derive(Debug, Clone)]
pub struct EvidenceToken {
    owner: Arc<()>,
    generation: u64,
    task_id: String,
    item_id: String,
    requirement_revision: u64,
    epoch: u64,
    requirement: Requirement,
    scope_root: Option<String>,
    observations: Vec<CompletionRequest>,
}
impl EvidenceToken {
    pub fn requirement(&self) -> &Requirement {
        &self.requirement
    }
    pub fn scope_root(&self) -> Option<&str> {
        self.scope_root.as_deref()
    }
    pub fn matches_check(&self, command: &str, root: &str) -> bool {
        match &self.requirement {
            Requirement::Check {
                command: expected,
                root: expected_root,
                ..
            } => expected == command && expected_root == root,
            _ => {
                self.item_id.is_empty()
                    && self.scope_root.as_deref() == Some(root)
                    && !command.is_empty()
            }
        }
    }
}

/// `items` contains canonical effective rows, not the model's status claims. A pending completion
/// request is stored separately, so a later receipt can resolve it without another model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskState {
    pub version: u32,
    pub id: String,
    pub revision: u64,
    pub semantic_revision: u64,
    pub effect_epoch: u64,
    pub active: bool,
    pub carry_over: bool,
    pub abandoned: bool,
    pub next_item: u64,
    pub items: Vec<Todo>,
    /// Legacy hints only; never evidence that a plan was saved or validated.
    pub plan_path: Option<String>,
    pub phase_id: Option<String>,
    pub plan: Option<PlanAttachment>,
    pub item_phases: Vec<ItemPhase>,
    pub requirements: Vec<ItemRequirement>,
    pub completion_requests: Vec<CompletionRequest>,
    /// Omitted rows preserve content, identity and original effective status as history.
    pub history: Vec<Todo>,
    /// Original v1 claims, including evidence-free Done, preserved without granting authority.
    pub legacy_claims: Vec<Todo>,
    pub obligations: Vec<TaskObligation>,
    pub checks: Vec<CheckReceipt>,
    pub observations: Vec<ObservationReceipt>,
    pub reports: Vec<ReportReceipt>,
    pub terminal: Option<TurnEnd>,
    /// Fail closed if an infallible native effect admission cannot allocate another obligation.
    pub effect_overflow: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStateWire {
    version: u32,
    id: String,
    revision: u64,
    active: bool,
    carry_over: bool,
    abandoned: bool,
    next_item: u64,
    items: Vec<Todo>,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    phase_id: Option<String>,
    #[serde(default)]
    plan: Option<PlanAttachment>,
    #[serde(default)]
    item_phases: Option<Vec<ItemPhase>>,
    #[serde(default)]
    semantic_revision: Option<u64>,
    #[serde(default)]
    effect_epoch: Option<u64>,
    #[serde(default)]
    requirements: Option<Vec<ItemRequirement>>,
    #[serde(default)]
    completion_requests: Option<Vec<CompletionRequest>>,
    #[serde(default)]
    history: Option<Vec<Todo>>,
    #[serde(default)]
    legacy_claims: Option<Vec<Todo>>,
    #[serde(default)]
    obligations: Option<Vec<TaskObligation>>,
    #[serde(default)]
    checks: Option<Vec<CheckReceipt>>,
    #[serde(default)]
    observations: Option<Vec<ObservationReceipt>>,
    #[serde(default)]
    reports: Option<Vec<ReportReceipt>>,
    #[serde(default)]
    terminal: Option<TurnEnd>,
    #[serde(default)]
    effect_overflow: Option<bool>,
}

impl<'de> Deserialize<'de> for TaskState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let w = TaskStateWire::deserialize(deserializer)?;
        if !(1..=VERSION).contains(&w.version) {
            return Err(D::Error::custom("unsupported task state version"));
        }
        if w.version == 1
            && (w.semantic_revision.is_some()
                || w.effect_epoch.is_some()
                || w.requirements.is_some()
                || w.completion_requests.is_some()
                || w.history.is_some()
                || w.legacy_claims.is_some()
                || w.obligations.is_some()
                || w.checks.is_some()
                || w.observations.is_some()
                || w.reports.is_some()
                || w.terminal.is_some()
                || w.effect_overflow.is_some())
        {
            return Err(D::Error::custom(
                "v1 task state cannot contain v2 authority fields",
            ));
        }
        if w.version < 3 && (w.plan.is_some() || w.item_phases.is_some()) {
            return Err(D::Error::custom(
                "legacy task state cannot contain v3 plan fields",
            ));
        }
        if w.version == 3 && w.item_phases.is_none() {
            return Err(D::Error::custom("v3 task state requires phase metadata"));
        }
        let old_version = w.version;
        let mut state = Self {
            version: VERSION,
            id: w.id,
            revision: w.revision,
            active: w.active,
            carry_over: w.carry_over,
            abandoned: w.abandoned,
            next_item: w.next_item,
            items: w.items,
            plan_path: w.plan_path,
            phase_id: w.phase_id,
            plan: w.plan,
            item_phases: w.item_phases.unwrap_or_default(),
            semantic_revision: w.semantic_revision.unwrap_or(0),
            effect_epoch: w.effect_epoch.unwrap_or(0),
            requirements: w.requirements.unwrap_or_default(),
            completion_requests: w.completion_requests.unwrap_or_default(),
            history: w.history.unwrap_or_default(),
            legacy_claims: w.legacy_claims.unwrap_or_default(),
            obligations: w.obligations.unwrap_or_default(),
            checks: w.checks.unwrap_or_default(),
            observations: w.observations.unwrap_or_default(),
            reports: w.reports.unwrap_or_default(),
            terminal: w.terminal,
            effect_overflow: w.effect_overflow.unwrap_or(false),
        };
        if old_version == 1 {
            state.legacy_claims = state.items.clone();
            for row in &state.items {
                state.requirements.push(ItemRequirement {
                    item_id: row.id.clone(),
                    revision: 1,
                    requirement: Requirement::Unspecified,
                });
                if row.status == Status::Done {
                    state.completion_requests.push(CompletionRequest {
                        item_id: row.id.clone(),
                        requirement_revision: 1,
                    });
                }
            }
            if state.active {
                state.resolve();
            }
        }
        state.validate().map_err(D::Error::custom)?;
        Ok(state)
    }
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            version: VERSION,
            id: String::new(),
            revision: 0,
            semantic_revision: 0,
            effect_epoch: 0,
            active: false,
            carry_over: false,
            abandoned: false,
            next_item: 1,
            items: Vec::new(),
            plan_path: None,
            phase_id: None,
            plan: None,
            item_phases: Vec::new(),
            requirements: Vec::new(),
            completion_requests: Vec::new(),
            history: Vec::new(),
            legacy_claims: Vec::new(),
            obligations: Vec::new(),
            checks: Vec::new(),
            observations: Vec::new(),
            reports: Vec::new(),
            terminal: None,
            effect_overflow: false,
        }
    }
}

fn identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn bounded(s: &str, max: usize, label: &str) -> Result<()> {
    ensure!(
        !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control),
        "invalid {label}"
    );
    ensure!(
        crate::agent::codebase::redact_for_review(s) == s,
        "{label} appears to contain a credential"
    );
    Ok(())
}
fn fingerprint_string(fp: &crate::core::persist::FileFingerprint) -> String {
    format!(
        "{}:{}:{}",
        u8::from(fp.exists),
        fp.byte_len,
        fp.sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
fn file_identity(path: &Path) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= MAX_FILE_BYTES,
        "observation source exceeds fingerprint bound"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "observation source grew beyond fingerprint bound"
    );
    Ok(fingerprint_string(
        &crate::core::persist::FileFingerprint::for_bytes(&bytes),
    ))
}

impl TaskState {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported task state version {}",
            self.version
        );
        ensure!(
            self.id.is_empty() || identifier(&self.id),
            "invalid task ID"
        );
        ensure!(
            !self.id.is_empty() || *self == Self::default(),
            "unidentified task state"
        );
        ensure!(
            !self.carry_over || self.active,
            "inactive task cannot request continuation"
        );
        ensure!(
            self.next_item > 0 && self.next_item < u64::MAX,
            "invalid task item sequence"
        );
        ensure!(
            self.effect_epoch < u64::MAX
                && self.semantic_revision < u64::MAX
                && self.revision < u64::MAX,
            "task counter exhausted"
        );
        ensure!(
            self.items.len() <= MAX_ITEMS
                && self.history.len() <= MAX_HISTORY
                && self.legacy_claims.len() <= MAX_ITEMS,
            "task row limit exceeded"
        );
        ensure!(
            self.requirements.len() <= MAX_ITEMS + MAX_HISTORY
                && self.completion_requests.len() <= MAX_ITEMS,
            "task requirement limit exceeded"
        );
        ensure!(
            self.obligations.len() <= MAX_OBLIGATIONS && self.receipt_count() <= MAX_RECEIPTS,
            "task ledger limit exceeded"
        );
        let mut known = HashSet::new();
        for row in self.items.iter().chain(&self.history) {
            ensure!(known.insert(row.id.as_str()), "duplicate task item ID");
            self.validate_row(row)?;
        }
        let mut legacy = HashSet::new();
        for row in &self.legacy_claims {
            self.validate_row(row)?;
            ensure!(
                known.contains(row.id.as_str()) && legacy.insert(row.id.as_str()),
                "invalid legacy claim reference"
            );
        }
        ensure!(
            self.items
                .iter()
                .filter(|r| r.status == Status::InProgress)
                .count()
                <= 1,
            "only one task item may be in_progress"
        );
        let mut requirements = HashMap::new();
        for req in &self.requirements {
            ensure!(
                known.contains(req.item_id.as_str())
                    && requirements.insert(req.item_id.as_str(), req).is_none(),
                "invalid requirement reference"
            );
            ensure!(
                req.revision > 0 && req.revision <= self.semantic_revision.max(1),
                "invalid requirement revision"
            );
            req.requirement.validate()?;
        }
        ensure!(
            requirements.len() == known.len(),
            "missing item requirement metadata"
        );
        let mut requested = HashSet::new();
        for request in &self.completion_requests {
            ensure!(
                self.items.iter().any(|r| r.id == request.item_id)
                    && requested.insert(request.item_id.as_str()),
                "invalid completion request reference"
            );
            ensure!(
                requirements[request.item_id.as_str()].revision == request.requirement_revision,
                "stale completion request"
            );
        }
        let mut roots = HashSet::new();
        for obligation in &self.obligations {
            bounded(&obligation.content, MAX_CONTENT, "obligation content")?;
            if obligation.item_id.is_empty() {
                ensure!(
                    !obligation.abandoned && roots.insert(obligation.root.as_deref()),
                    "invalid workspace obligation"
                );
                if let Some(root) = &obligation.root {
                    bounded(root, MAX_TEXT, "obligation root")?;
                }
                ensure!(
                    obligation.requirement_revision <= self.effect_epoch,
                    "future workspace obligation"
                );
            } else {
                ensure!(
                    known.contains(obligation.item_id.as_str())
                        && obligation.abandoned
                        && !obligation.resolved,
                    "invalid abandoned obligation"
                );
            }
        }
        for receipt in &self.checks {
            self.validate_receipt(
                receipt.version,
                &receipt.task_id,
                &receipt.item_id,
                receipt.requirement_revision,
                receipt.epoch,
                &requirements,
            )?;
            bounded(&receipt.command, MAX_TEXT, "check command")?;
            bounded(&receipt.root, MAX_TEXT, "check root")?;
            bounded(&receipt.source_identity, 256, "source identity")?;
            if receipt.item_id.is_empty() {
                ensure!(
                    self.obligations
                        .iter()
                        .any(|o| o.item_id.is_empty() && o.root.as_deref() == Some(&receipt.root)),
                    "unknown check coverage"
                );
            }
        }
        for receipt in &self.observations {
            ensure!(!receipt.item_id.is_empty(), "observation requires an item");
            self.validate_receipt(
                receipt.version,
                &receipt.task_id,
                &receipt.item_id,
                receipt.requirement_revision,
                receipt.epoch,
                &requirements,
            )?;
            bounded(&receipt.target, MAX_TEXT, "observation target")?;
            ensure!(
                receipt.path.is_some() == receipt.fingerprint.is_some(),
                "partial observation identity"
            );
            if let (Some(path), Some(fp)) = (&receipt.path, &receipt.fingerprint) {
                bounded(path, MAX_TEXT, "observation path")?;
                ensure!(
                    fp.len() <= 100 && fp.bytes().all(|b| b.is_ascii_hexdigit() || b == b':'),
                    "invalid observation fingerprint"
                );
            }
        }
        for receipt in &self.reports {
            ensure!(!receipt.item_id.is_empty(), "report requires an item");
            self.validate_receipt(
                receipt.version,
                &receipt.task_id,
                &receipt.item_id,
                receipt.requirement_revision,
                receipt.epoch,
                &requirements,
            )?;
        }
        if let Some(phase) = &self.phase_id {
            ensure!(
                identifier(phase) && self.plan_path.is_some(),
                "phase link requires a valid ID and plan path"
            );
        }
        if let Some(path) = &self.plan_path {
            ensure!(
                path.len() <= 512
                    && path.starts_with("docs/plans/")
                    && path.ends_with(".md")
                    && !path.contains('\\')
                    && !path.chars().any(char::is_control)
                    && path
                        .split('/')
                        .all(|p| !p.is_empty() && p != "." && p != ".."),
                "invalid task plan link"
            );
        }
        self.validate_plan()?;
        Ok(())
    }
    fn validate_row(&self, row: &Todo) -> Result<()> {
        let sequence = row
            .id
            .strip_prefix("item-")
            .and_then(|n| n.parse::<u64>().ok());
        ensure!(
            identifier(&row.id) && sequence.is_some_and(|n| n > 0 && n < self.next_item),
            "invalid task item ID"
        );
        bounded(&row.content, MAX_CONTENT, "task content")?;
        ensure!(
            row.confidence.is_none_or(|n| n <= 100) && row.hill_climbable.is_none_or(|n| n <= 100),
            "task score exceeds 100"
        );
        Ok(())
    }
    fn validate_receipt(
        &self,
        version: u32,
        owner: &str,
        item: &str,
        revision: u64,
        epoch: u64,
        requirements: &HashMap<&str, &ItemRequirement>,
    ) -> Result<()> {
        ensure!(
            version == 1 && owner == self.id && epoch <= self.effect_epoch,
            "invalid receipt version/owner/epoch"
        );
        if !item.is_empty() {
            ensure!(
                requirements
                    .get(item)
                    .is_some_and(|r| revision > 0 && revision <= r.revision),
                "invalid receipt requirement reference"
            );
        }
        Ok(())
    }
    fn requirement(&self, item: &str) -> Option<&ItemRequirement> {
        self.requirements.iter().find(|r| r.item_id == item)
    }
    fn receipt_count(&self) -> usize {
        self.checks.len() + self.observations.len() + self.reports.len()
    }
    fn reserve_receipts(&self, count: usize) -> Result<()> {
        ensure!(
            self.receipt_count().saturating_add(count) <= MAX_RECEIPTS,
            "task receipt ledger is full; no supporting receipt was evicted"
        );
        Ok(())
    }
    fn satisfies(&self, req: &ItemRequirement) -> bool {
        if !self.phase_resolved(&req.item_id) {
            return false;
        }
        match &req.requirement {
            Requirement::Unspecified => false,
            Requirement::Observe { target } => self.observations.iter().rev().any(|r| {
                r.item_id == req.item_id
                    && r.requirement_revision == req.revision
                    && r.epoch == self.effect_epoch
                    && &r.target == target
            }),
            Requirement::Report => self.reports.iter().rev().any(|r| {
                r.item_id == req.item_id
                    && r.requirement_revision == req.revision
                    && r.epoch == self.effect_epoch
            }),
            Requirement::Check {
                command,
                root,
                source_identity,
            } => self
                .checks
                .iter()
                .rev()
                .find(|r| {
                    r.item_id == req.item_id
                        && r.requirement_revision == req.revision
                        && r.epoch == self.effect_epoch
                        && &r.command == command
                        && &r.root == root
                })
                .is_some_and(|r| {
                    r.result.accepted()
                        && source_identity
                            .as_deref()
                            .is_none_or(|identity| identity == r.source_identity)
                }),
        }
    }
    fn resolve(&mut self) {
        let satisfied: HashSet<String> = self
            .completion_requests
            .iter()
            .filter(|request| {
                self.requirement(&request.item_id).is_some_and(|r| {
                    r.revision == request.requirement_revision && self.satisfies(r)
                })
            })
            .map(|r| r.item_id.clone())
            .collect();
        let requests: HashSet<&str> = self
            .completion_requests
            .iter()
            .map(|r| r.item_id.as_str())
            .collect();
        for row in &mut self.items {
            if requests.contains(row.id.as_str()) {
                row.status = if satisfied.contains(&row.id) {
                    Status::Done
                } else {
                    Status::Pending
                };
            } else if row.status == Status::Done {
                row.status = Status::Pending;
            }
        }
    }
    fn pending(&self) -> bool {
        self.abandoned
            || self.effect_overflow
            || self.items.iter().any(|row| {
                row.status != Status::Done
                    || self.requirement(&row.id).is_none_or(|r| !self.satisfies(r))
            })
            || self.obligations.iter().any(|o| !o.resolved || o.abandoned)
    }
    fn ensure_id(&mut self) {
        if self.id.is_empty() {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let mut bytes = [0; 16];
            self.id = if getrandom::getrandom(&mut bytes).is_ok() {
                format!(
                    "task-{}",
                    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                )
            } else {
                // Identity, not a secret; keep native effect admission fail-safe if RNG fails.
                format!(
                    "task-{}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed)
                )
            };
        }
    }
    fn bump_epoch(&mut self) {
        if self.effect_epoch >= u64::MAX - 1 {
            self.effect_overflow = true;
        } else {
            self.effect_epoch += 1;
        }
        // Resolved workspace obligations must be proven again after any later effect.
        for obligation in &mut self.obligations {
            if obligation.item_id.is_empty() {
                obligation.resolved = false;
            }
        }
        self.resolve();
    }
    fn revise(&mut self) {
        self.revision = self.revision.saturating_add(1).min(u64::MAX - 1);
    }
    fn abandon_row(&mut self, row: &Todo) -> Result<()> {
        if !self
            .obligations
            .iter()
            .any(|o| o.item_id == row.id && o.abandoned)
        {
            ensure!(
                self.obligations.len() < MAX_OBLIGATIONS,
                "task obligation ledger is full"
            );
            self.obligations.push(TaskObligation {
                item_id: row.id.clone(),
                content: row.content.clone(),
                root: None,
                requirement_revision: self.requirement(&row.id).map_or(1, |r| r.revision),
                resolved: false,
                abandoned: true,
            });
        }
        Ok(())
    }
    fn replace(&self, mut items: Vec<Todo>, requested: Vec<(String, Requirement)>) -> Result<Self> {
        ensure!(
            items.len() <= MAX_ITEMS,
            "task list exceeds {MAX_ITEMS} items"
        );
        let mut next = self.clone();
        next.ensure_id();
        let mut used = HashSet::new();
        for row in items.iter().filter(|r| !r.id.is_empty()) {
            ensure!(
                self.items.iter().any(|old| old.id == row.id) && used.insert(row.id.clone()),
                "unknown or duplicate task item ID"
            );
        }
        for i in 0..items.len() {
            if !items[i].id.is_empty() {
                continue;
            }
            let unique = items
                .iter()
                .filter(|r| r.content == items[i].content)
                .count()
                == 1;
            let matches: Vec<_> = self
                .items
                .iter()
                .filter(|r| r.content == items[i].content && !used.contains(&r.id))
                .collect();
            let id = if unique && matches.len() == 1 {
                matches[0].id.clone()
            } else {
                ensure!(
                    next.next_item < u64::MAX - 1,
                    "task item sequence exhausted"
                );
                let id = format!("item-{}", next.next_item);
                next.next_item += 1;
                id
            };
            used.insert(id.clone());
            items[i].id = id;
        }
        let requested: HashMap<_, _> = requested.into_iter().collect();
        for (key, requirement) in &requested {
            ensure!(
                items.iter().any(|r| &r.id == key)
                    || key
                        .strip_prefix("@index:")
                        .and_then(|n| n.parse::<usize>().ok())
                        .is_some_and(|i| i < items.len()),
                "unknown requirement item reference"
            );
            requirement.validate()?;
        }
        let semantic_change = items.len() != self.items.len()
            || items.iter().enumerate().any(|(index, row)| {
                self.items
                    .iter()
                    .find(|old| old.id == row.id)
                    .is_none_or(|old| old.content != row.content)
                    || requested
                        .get(&row.id)
                        .or_else(|| requested.get(&format!("@index:{index}")))
                        .is_some_and(|new| {
                            self.requirement(&row.id)
                                .is_none_or(|old| &old.requirement != new)
                        })
            });
        if semantic_change {
            next.semantic_revision = next
                .semantic_revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("task semantic revision exhausted"))?;
        }
        for old in &self.items {
            if !used.contains(&old.id) {
                ensure!(next.history.len() < MAX_HISTORY, "task history is full");
                if old.status != Status::Done
                    || self.requirement(&old.id).is_none_or(|r| !self.satisfies(r))
                {
                    next.abandon_row(old)?;
                }
                next.history.push(old.clone());
            }
        }
        let mut requests = Vec::new();
        for (index, row) in items.iter().enumerate() {
            let old = self.requirement(&row.id);
            let requirement = requested
                .get(&row.id)
                .or_else(|| requested.get(&format!("@index:{index}")))
                .cloned()
                .unwrap_or_else(|| old.map_or(Requirement::Unspecified, |r| r.requirement.clone()));
            let content_changed = self
                .items
                .iter()
                .find(|r| r.id == row.id)
                .is_none_or(|r| r.content != row.content);
            let changed = content_changed || old.is_none_or(|r| r.requirement != requirement);
            let revision = if changed {
                next.semantic_revision.max(1)
            } else {
                old.map_or(1, |r| r.revision)
            };
            // Requirement weakening cannot erase an unsatisfied check or a failed runtime check.
            if let Some(old) = old {
                if matches!(old.requirement, Requirement::Check { .. })
                    && old.requirement != requirement
                    && !self.satisfies(old)
                {
                    if let Some(old_row) = self.items.iter().find(|r| r.id == row.id) {
                        next.abandon_row(old_row)?;
                    }
                }
            }
            let metadata = ItemRequirement {
                item_id: row.id.clone(),
                revision,
                requirement,
            };
            if let Some(position) = next.requirements.iter().position(|r| r.item_id == row.id) {
                next.requirements[position] = metadata;
            } else {
                next.requirements.push(metadata);
            }
            if row.status == Status::Done {
                requests.push(CompletionRequest {
                    item_id: row.id.clone(),
                    requirement_revision: revision,
                });
            }
        }
        next.completion_requests = requests;
        next.items = items;
        next.abandoned = next.items.is_empty();
        next.active = true;
        next.carry_over = true;
        next.terminal = None;
        next.resolve();
        next.revise();
        next.validate()?;
        Ok(next)
    }
}

#[derive(Debug, Clone)]
pub struct TaskContext {
    state: Arc<Mutex<TaskState>>,
    saved: Arc<Mutex<TaskState>>,
    owner: Arc<()>,
    generation: Arc<AtomicU64>,
    /// Conversation policy outlives checklist replacement, reset and completion.
    control: crate::core::session_control::ControlContext,
    /// In-process clarify handoff only; never part of TaskState or completion/permission evidence.
    workflow_continuation: Arc<Mutex<Option<(String, crate::agent::plan_nudge::AdviceContext)>>>,
    visible: bool,
}
impl Default for TaskContext {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(TaskState::default())),
            saved: Arc::new(Mutex::new(TaskState::default())),
            owner: Arc::new(()),
            generation: Arc::new(AtomicU64::new(0)),
            control: crate::core::session_control::ControlContext::default(),
            workflow_continuation: Arc::new(Mutex::new(None)),
            visible: true,
        }
    }
}
impl TaskContext {
    pub(crate) fn control(&self) -> crate::core::session_control::ControlContext {
        self.control.clone()
    }
    pub(crate) fn pause_workflow_advice(
        &self,
        call_id: String,
        advice: crate::agent::plan_nudge::AdviceContext,
    ) {
        *self
            .workflow_continuation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((call_id, advice));
    }
    /// Consume once, including on mismatch. A new conversation or non-clarify run cannot reuse it.
    pub(crate) fn resume_workflow_advice(
        &self,
        call_id: Option<&str>,
    ) -> Option<crate::agent::plan_nudge::AdviceContext> {
        self.workflow_continuation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .filter(|(expected, _)| Some(expected.as_str()) == call_id)
            .map(|(_, advice)| advice)
    }
    /// Delegated task state is private, but execution permission stays conversation-owned.
    pub(crate) fn with_control(
        mut self,
        control: crate::core::session_control::ControlContext,
    ) -> Self {
        self.control = control;
        self
    }
    pub fn private() -> Self {
        Self {
            visible: false,
            ..Self::default()
        }
    }
    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
    }
    pub fn snapshot(&self) -> TaskState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn effective_items(&self) -> Vec<Todo> {
        self.snapshot().items
    }
    #[cfg(test)]
    pub fn effect_epoch(&self) -> u64 {
        self.snapshot().effect_epoch
    }
    pub fn restore(&self, state: TaskState) -> Result<()> {
        self.install_inner(state, true)
    }
    pub fn install(&self, state: TaskState) -> Result<()> {
        self.install_inner(state, false)
    }
    fn install_inner(&self, mut state: TaskState, from_disk: bool) -> Result<()> {
        state.validate()?;
        let saved = state.clone();
        if let Some(plan) = &mut state.plan {
            plan.live = false;
        }
        if !state.checks.is_empty() || !state.observations.is_empty() || !state.reports.is_empty() {
            state.bump_epoch();
            state.revise();
        }
        state.resolve();
        state.validate()?;
        {
            let mut current = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.generation.fetch_add(1, Ordering::Relaxed);
            *current = state;
            if from_disk {
                *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = saved;
            }
        }
        self.resume_workflow_advice(None);
        self.publish();
        Ok(())
    }
    pub fn mark_saved(&self, state: &TaskState) {
        let current = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *current == *state {
            *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
        }
    }
    pub fn is_dirty(&self) -> bool {
        let current = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *current != *self.saved.lock().unwrap_or_else(|e| e.into_inner())
    }
    #[cfg(test)]
    pub fn replace(&self, items: Vec<Todo>) -> Result<Vec<Todo>> {
        self.replace_with_requirements(items, Vec::new())
    }
    pub fn replace_with_requirements(
        &self,
        items: Vec<Todo>,
        requirements: Vec<(String, Requirement)>,
    ) -> Result<Vec<Todo>> {
        self.replace_with_plan(items, requirements, None, Vec::new())
    }
    /// Explicit human/session reset, not the model's empty-list update.
    pub fn clear(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.generation.fetch_add(1, Ordering::Relaxed);
            *state = TaskState::default();
        }
        self.resume_workflow_advice(None);
        self.publish();
    }
    pub fn begin_user_turn(&self) -> bool {
        // A follow-up planning turn edits the same contract; delivery is not permission to
        // replace its task or silently discard its attachment/implementation obligations.
        let planning = self.control().snapshot().is_planning();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let dirty = *state != *self.saved.lock().unwrap_or_else(|e| e.into_inner());
        let clear = !planning && !state.carry_over && !dirty;
        if clear {
            self.generation.fetch_add(1, Ordering::Relaxed);
            *state = TaskState::default();
        } else if state.carry_over {
            state.carry_over = false;
            state.active = true;
            state.terminal = None;
            state.resolve();
            state.revise();
        }
        drop(state);
        self.publish();
        clear
    }
    pub fn end_turn(&self, abnormal: bool) -> FinishDisposition {
        self.finish_turn(if abnormal {
            TurnEnd::Interrupted
        } else {
            TurnEnd::Completed
        })
    }
    pub fn finish_turn(&self, end: TurnEnd) -> FinishDisposition {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.id.is_empty() {
            return if end == TurnEnd::Completed {
                FinishDisposition::Completed
            } else {
                FinishDisposition::Retained
            };
        }
        state.resolve();
        let (actual, disposition) = if end == TurnEnd::Completed && state.pending() {
            (TurnEnd::Incomplete, FinishDisposition::Incomplete)
        } else if end == TurnEnd::Completed {
            (end, FinishDisposition::Completed)
        } else if end == TurnEnd::Abandoned {
            (end, FinishDisposition::Abandoned)
        } else if end == TurnEnd::Incomplete {
            (end, FinishDisposition::Incomplete)
        } else {
            (end, FinishDisposition::Retained)
        };
        state.terminal = Some(actual);
        state.active = !matches!(actual, TurnEnd::Completed | TurnEnd::Abandoned);
        state.carry_over = state.active;
        if actual == TurnEnd::Abandoned {
            state.abandoned = true;
        }
        state.revise();
        drop(state);
        self.publish();
        disposition
    }
    pub fn completion_pending(&self) -> bool {
        self.snapshot().pending()
    }
    pub fn is_complete(&self) -> bool {
        !self.snapshot().pending()
    }
    pub fn needs_check(&self) -> bool {
        let s = self.snapshot();
        s.obligations
            .iter()
            .any(|o| o.item_id.is_empty() && !o.resolved)
            || s.items.iter().any(|row| {
                s.requirement(&row.id).is_some_and(|r| {
                    matches!(r.requirement, Requirement::Check { .. }) && !s.satisfies(r)
                })
            })
    }
    pub fn completion_basis(&self, item_id: &str) -> CompletionBasis {
        let s = self.snapshot();
        let Some(req) = s.requirement(item_id) else {
            return CompletionBasis::NotRequired;
        };
        if !s.satisfies(req) {
            return CompletionBasis::Unverified;
        }
        match req.requirement {
            Requirement::Observe { .. } => CompletionBasis::Observed,
            Requirement::Check { .. } => CompletionBasis::Verified,
            Requirement::Report => CompletionBasis::Reported,
            Requirement::Unspecified => CompletionBasis::Unverified,
        }
    }
    pub fn only_report_pending(&self) -> bool {
        let s = self.snapshot();
        if s.abandoned
            || s.effect_overflow
            || s.obligations.iter().any(|o| !o.resolved || o.abandoned)
        {
            return false;
        }
        let open: Vec<_> = s
            .items
            .iter()
            .filter(|row| row.status != Status::Done)
            .collect();
        !open.is_empty()
            && open.iter().all(|row| {
                s.completion_requests.iter().any(|r| r.item_id == row.id)
                    && s.requirement(&row.id)
                        .is_some_and(|r| r.requirement == Requirement::Report)
            })
    }
    pub fn handoff_eligible(&self) -> bool {
        let s = self.snapshot();
        if s.abandoned
            || s.effect_overflow
            || s.items.iter().any(|r| {
                r.status != Status::Done || s.requirement(&r.id).is_none_or(|req| !s.satisfies(req))
            })
            || s.obligations
                .iter()
                .any(|o| o.abandoned || (!o.resolved && o.root.is_none()))
        {
            return false;
        }
        if s.checks
            .iter()
            .any(|r| !r.result.accepted() && r.epoch == s.effect_epoch)
        {
            return false;
        }
        s.obligations
            .iter()
            .any(|o| o.item_id.is_empty() && !o.resolved && o.root.is_some())
    }
    fn token(
        &self,
        state: &TaskState,
        req: Option<&ItemRequirement>,
        obligation: Option<&TaskObligation>,
    ) -> EvidenceToken {
        EvidenceToken {
            owner: self.owner.clone(),
            generation: self.generation.load(Ordering::Relaxed),
            task_id: state.id.clone(),
            item_id: req.map_or_else(String::new, |r| r.item_id.clone()),
            requirement_revision: req.map_or_else(
                || obligation.map_or(0, |o| o.requirement_revision),
                |r| r.revision,
            ),
            epoch: state.effect_epoch,
            requirement: req.map_or(Requirement::Unspecified, |r| r.requirement.clone()),
            scope_root: obligation.and_then(|o| o.root.clone()),
            observations: Vec::new(),
        }
    }
    pub fn evidence_captures(&self) -> Vec<EvidenceToken> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut tokens: Vec<_> = state
            .items
            .iter()
            .filter_map(|row| state.requirement(&row.id))
            .filter(|r| matches!(r.requirement, Requirement::Check { .. }) && !state.satisfies(r))
            .map(|r| self.token(&state, Some(r), None))
            .collect();
        tokens.extend(
            state
                .obligations
                .iter()
                .filter(|o| o.item_id.is_empty() && !o.resolved)
                .map(|o| self.token(&state, None, Some(o))),
        );
        tokens
    }
    #[cfg(test)]
    pub fn evidence_capture(&self) -> Option<EvidenceToken> {
        self.evidence_captures().into_iter().next()
    }
    pub fn verification_roots(&self) -> Vec<String> {
        let mut roots = Vec::new();
        for token in self.evidence_captures() {
            let root = match token.requirement() {
                Requirement::Check { root, .. } => Some(root.as_str()),
                _ => token.scope_root(),
            };
            if let Some(root) = root {
                if !roots.iter().any(|r| r == root) {
                    roots.push(root.to_string());
                }
            }
        }
        roots
    }
    pub fn observation_capture(&self, target: &str) -> Option<EvidenceToken> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let matched: Vec<_> = state.items.iter().filter_map(|row| state.requirement(&row.id)).filter(|r| matches!(&r.requirement, Requirement::Observe { target: expected } if expected == target) && !state.satisfies(r)).collect();
        let first = matched.first()?;
        let mut token = self.token(&state, Some(first), None);
        token.observations = matched
            .iter()
            .map(|r| CompletionRequest {
                item_id: r.item_id.clone(),
                requirement_revision: r.revision,
            })
            .collect();
        Some(token)
    }
    fn validate_token(&self, state: &TaskState, token: &EvidenceToken) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&token.owner, &self.owner)
                && token.generation == self.generation.load(Ordering::Relaxed)
                && token.task_id == state.id,
            "evidence token belongs to another task incarnation"
        );
        ensure!(
            token.epoch == state.effect_epoch && !state.effect_overflow,
            "evidence token is stale after effects"
        );
        if token.item_id.is_empty() {
            ensure!(
                state.obligations.iter().any(|o| o.item_id.is_empty()
                    && o.root == token.scope_root
                    && o.requirement_revision == token.requirement_revision),
                "workspace obligation changed"
            );
        } else {
            ensure!(
                state.items.iter().any(|r| r.id == token.item_id)
                    && state
                        .requirement(&token.item_id)
                        .is_some_and(|r| r.revision == token.requirement_revision
                            && r.requirement == token.requirement),
                "evidence requirement changed"
            );
        }
        Ok(())
    }
    pub fn record_check(
        &self,
        token: EvidenceToken,
        command: impl Into<String>,
        root: impl Into<String>,
        source_identity: impl Into<String>,
        result: CheckResult,
    ) -> Result<()> {
        let command = command.into();
        let root = root.into();
        let source_identity = source_identity.into();
        bounded(&command, MAX_TEXT, "check command")?;
        bounded(&root, MAX_TEXT, "check root")?;
        bounded(&source_identity, 256, "source identity")?;
        ensure!(
            token.matches_check(&command, &root),
            "receipt does not match exact runtime check/root"
        );
        if let Requirement::Check {
            source_identity: Some(expected),
            ..
        } = &token.requirement
        {
            ensure!(
                *expected == source_identity,
                "check source identity mismatch"
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.validate_token(&state, &token)?;
        let mut next = state.clone();
        next.reserve_receipts(1)?;
        next.checks.push(CheckReceipt {
            version: 1,
            task_id: token.task_id,
            item_id: token.item_id.clone(),
            requirement_revision: token.requirement_revision,
            epoch: token.epoch,
            command,
            root: root.clone(),
            source_identity,
            result,
        });
        if token.item_id.is_empty() {
            for obligation in &mut next.obligations {
                if obligation.item_id.is_empty() && obligation.root.as_deref() == Some(&root) {
                    obligation.resolved = result.accepted();
                }
            }
        }
        next.resolve();
        next.revise();
        next.validate()?;
        *state = next;
        drop(state);
        self.publish();
        Ok(())
    }
    pub fn record_file_observation(
        &self,
        token: EvidenceToken,
        target: impl Into<String>,
        path: &Path,
        fingerprint: &crate::core::persist::FileFingerprint,
    ) -> Result<()> {
        ensure!(
            fingerprint.exists && fingerprint.byte_len <= MAX_FILE_BYTES,
            "observation source unavailable or too large"
        );
        let expected = fingerprint_string(fingerprint);
        ensure!(
            file_identity(path)? == expected,
            "observation source changed during read"
        );
        self.record_observation_inner(
            token,
            target.into(),
            Some((path.to_string_lossy().into_owned(), expected)),
        )
    }
    #[cfg(test)]
    pub fn record_observation_with_token(
        &self,
        token: EvidenceToken,
        target: impl Into<String>,
    ) -> Result<()> {
        self.record_observation_inner(token, target.into(), None)
    }
    fn record_observation_inner(
        &self,
        token: EvidenceToken,
        target: String,
        source: Option<(String, String)>,
    ) -> Result<()> {
        bounded(&target, MAX_TEXT, "observation target")?;
        ensure!(
            matches!(&token.requirement, Requirement::Observe { target: expected } if expected == &target),
            "observation target mismatch"
        );
        if let Some((path, _)) = &source {
            bounded(path, MAX_TEXT, "observation path")?;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.validate_token(&state, &token)?;
        let mut next = state.clone();
        next.reserve_receipts(token.observations.len())?;
        for binding in &token.observations {
            ensure!(
                next.items.iter().any(|row| row.id == binding.item_id)
                    && next
                        .requirement(&binding.item_id)
                        .is_some_and(|r| r.revision == binding.requirement_revision
                            && r.requirement == token.requirement),
                "observation requirement changed"
            );
            next.observations.push(ObservationReceipt {
                version: 1,
                task_id: token.task_id.clone(),
                item_id: binding.item_id.clone(),
                requirement_revision: binding.requirement_revision,
                epoch: token.epoch,
                target: target.clone(),
                path: source.as_ref().map(|s| s.0.clone()),
                fingerprint: source.as_ref().map(|s| s.1.clone()),
            });
        }
        next.resolve();
        next.revise();
        next.validate()?;
        *state = next;
        drop(state);
        self.publish();
        Ok(())
    }
    pub fn record_report(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let pending: Vec<_> = state
            .completion_requests
            .iter()
            .filter(|request| {
                state
                    .requirement(&request.item_id)
                    .is_some_and(|r| r.requirement == Requirement::Report && !state.satisfies(r))
            })
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let mut next = state.clone();
        next.reserve_receipts(pending.len())?;
        for request in pending {
            next.reports.push(ReportReceipt {
                version: 1,
                task_id: next.id.clone(),
                item_id: request.item_id,
                requirement_revision: request.requirement_revision,
                epoch: next.effect_epoch,
            });
        }
        next.resolve();
        next.revise();
        next.validate()?;
        *state = next;
        drop(state);
        self.publish();
        Ok(())
    }
    /// Navigation invalidates receipts and in-flight observation tokens, but is not a source write.
    /// Keep every mandatory obligation/plan attachment; never resolve or invent one for focus.
    pub(crate) fn invalidate_workspace_evidence(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ensure_id();
        state.bump_epoch();
        state.revise();
        drop(state);
        self.publish();
    }

    pub fn invalidate_effects(&self) {
        self.invalidate_effects_at(None);
    }
    pub fn invalidate_effects_at(&self, root: Option<&Path>) {
        let root = root.map(|r| r.to_string_lossy().into_owned());
        let root = root.filter(|r| bounded(r, MAX_TEXT, "effect root").is_ok());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ensure_id();
        state.bump_epoch();
        let epoch = state.effect_epoch;
        if let Some(existing) = state
            .obligations
            .iter_mut()
            .find(|o| o.item_id.is_empty() && o.root == root)
        {
            existing.requirement_revision = epoch;
            existing.resolved = false;
        } else if state.obligations.len() < MAX_OBLIGATIONS {
            state.obligations.push(TaskObligation {
                item_id: String::new(),
                content: "Workspace mutation requires fresh verification".into(),
                root,
                requirement_revision: epoch,
                resolved: false,
                abandoned: false,
            });
        } else {
            state.effect_overflow = true;
        }
        state.active = true;
        state.carry_over = true;
        state.revise();
        drop(state);
        self.publish();
    }
    pub fn fresh_sources(&self) -> Vec<(String, String)> {
        let state = self.snapshot();
        let mut sources = Vec::new();
        for receipt in state
            .checks
            .iter()
            .filter(|r| r.result.accepted() && r.epoch == state.effect_epoch)
        {
            let source = (receipt.root.clone(), receipt.source_identity.clone());
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        sources
    }
    /// Native observations need bounded file revalidation in addition to the mutation epoch.
    /// Drift stales live authority even when the caller only consumes this method's side effect.
    pub fn revalidate_sources(&self) -> Result<()> {
        let snapshot = self.snapshot();
        let drift = snapshot
            .observations
            .iter()
            .filter(|r| r.epoch == snapshot.effect_epoch)
            .filter_map(|r| r.path.as_ref().zip(r.fingerprint.as_ref()))
            .any(|(path, expected)| {
                file_identity(Path::new(path)).map_or(true, |actual| &actual != expected)
            });
        if drift {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.id == snapshot.id && state.effect_epoch == snapshot.effect_epoch {
                state.bump_epoch();
                state.revise();
            }
            drop(state);
            self.publish();
            anyhow::bail!("native observation source changed or became unavailable");
        }
        Ok(())
    }
    pub(super) fn publish(&self) {
        if self.visible {
            super::publish(&self.snapshot());
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
