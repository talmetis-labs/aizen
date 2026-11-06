//! Mnemos rich inspection/management extension v1, transported by the existing Commands capability.
//! This module is deliberately engine-independent so hosts can mirror its finite wire vocabulary.
use super::api::{self, Validate};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_PAGE_BYTES: usize = 262_144;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Partition {
    Entries,
    Archive,
    Pending,
    Discarded,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub partition: Partition,
    pub id: String,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewScope {
    #[default]
    Current,
    User,
    Device,
    Project,
    All,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    #[default]
    All,
    Active,
    Retired,
    Archive,
    Pending,
    Discarded,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub scope: ViewScope,
    pub lifecycle: Lifecycle,
    pub query: String,
    pub kind: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub limit: usize,
    pub max_bytes: usize,
    pub cursor: Option<String>,
    pub expected_snapshot: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Inspect {
    Schema,
    Summary {
        filter: Filter,
    },
    Records {
        filter: Filter,
        page: Page,
    },
    GraphNodes {
        filter: Filter,
        page: Page,
    },
    GraphEdges {
        filter: Filter,
        page: Page,
    },
    Detail {
        filter: Filter,
        reference: Reference,
        expected_snapshot: Option<String>,
    },
    Stats {
        filter: Filter,
    },
    Profile {
        filter: Filter,
    },
    Style {
        filter: Filter,
    },
    Frozen {
        filter: Filter,
    },
    Diagnostics {
        filter: Filter,
    },
    Outcome {
        operation_id: String,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub reference: Reference,
    pub revision: u64,
    pub lifecycle: Lifecycle,
    pub name: String,
    pub kind: String,
    pub content: String,
    pub description: String,
    pub scope: ViewScope,
    pub anchor: Option<String>,
    pub device: Option<String>,
    pub confidence: f64,
    pub reinforced: u32,
    pub confirmations: u32,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub last_used: Option<String>,
    pub origin_id: Option<String>,
    pub superseded_by: Option<Reference>,
    pub supersedes: Option<Reference>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub active: usize,
    pub retired: usize,
    pub archive: usize,
    pub pending: usize,
    pub discarded: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraphPolicy {
    pub floor: f64,
    pub half_life_days: f64,
    pub max_edges: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub reference: Reference,
    pub name: String,
    pub kind: String,
    pub lifecycle: Lifecycle,
    pub degree: usize,
    pub confidence: f64,
    pub confirmations: u32,
    pub reinforced: u32,
    pub last_used: Option<String>,
    pub scope: ViewScope,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub source: Reference,
    pub target: Reference,
    pub stored_weight: f64,
    pub effective_weight: f64,
    pub last_cofire: String,
    pub faded: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PageResult<T> {
    pub snapshot: String,
    pub as_of: String,
    pub items: Vec<T>,
    pub total_matching: usize,
    pub complete: bool,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportItem {
    pub name: String,
    pub value: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Inspection {
    Schema {
        version: u32,
        inspect: bool,
        manage: bool,
    },
    Summary {
        snapshot: String,
        as_of: String,
        counts: Counts,
        nodes: usize,
        edges: usize,
        graph_policy: GraphPolicy,
    },
    Records {
        page: PageResult<Record>,
    },
    GraphNodes {
        page: PageResult<Node>,
    },
    GraphEdges {
        page: PageResult<Edge>,
    },
    Detail {
        snapshot: String,
        as_of: String,
        record: Record,
    },
    Stats {
        snapshot: String,
        as_of: String,
        counts: Counts,
        graph_policy: GraphPolicy,
        items: Vec<ReportItem>,
    },
    Profile {
        snapshot: String,
        as_of: String,
        items: Vec<ReportItem>,
    },
    Style {
        snapshot: String,
        as_of: String,
        text: String,
    },
    Frozen {
        snapshot: String,
        as_of: String,
        text: String,
    },
    Diagnostics {
        snapshot: String,
        as_of: String,
        items: Vec<ReportItem>,
    },
    Outcome {
        operation_id: String,
        status: OutcomeStatus,
        recovery_required: bool,
        receipt: Option<Receipt>,
        error: Option<api::MemoryError>,
    },
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Unknown,
    Done,
    RecoveryRequired,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub reference: Reference,
    pub expected_revision: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub keep: Target,
    pub duplicate: Target,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub content: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub scope: ViewScope,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manage {
    pub filter: Filter,
    pub expected_snapshot: String,
    pub action: Action,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Create {
        draft: Draft,
    },
    Update {
        target: Target,
        draft: Draft,
    },
    Forget {
        target: Target,
    },
    QueuePromote {
        target: Target,
    },
    QueueDrop {
        target: Target,
    },
    QueueClear {
        targets: Vec<Target>,
    },
    Revive {
        target: Target,
    },
    Restore {
        target: Target,
        as_id: Option<String>,
    },
    Purge {
        targets: Vec<Target>,
        confirmed: bool,
    },
    Merge {
        pairs: Vec<Pair>,
    },
    Worklist,
    StyleSet {
        text: String,
    },
    FrozenRebuild,
    CompactPrepare,
    CompactApply {
        targets: Vec<Target>,
        confirmed: bool,
    },
    ConsolidatePrepare,
    ConsolidateApply {
        pairs: Vec<Pair>,
        confirmed: bool,
    },
    ReconcilePrepare,
    ReconcilePreview {
        binding: String,
        completion: String,
    },
    ReconcileApply {
        binding: String,
        completion: String,
        confirmed: bool,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: String,
    pub snapshot_before: String,
    pub changed: Vec<Reference>,
    pub created: Option<Reference>,
    pub archived: Option<Reference>,
    pub items: Vec<ReportItem>,
    pub targets: Vec<Target>,
    pub pairs: Vec<Pair>,
    pub binding: Option<String>,
    pub system: Option<String>,
    pub input: Option<String>,
}
impl Validate for Reference {
    fn validate(&self) -> api::Result<()> {
        api::opaque_id(&self.id)
    }
}
impl Validate for Filter {
    fn validate(&self) -> api::Result<()> {
        api::text(&self.query, 4096, true)?;
        if let Some(kind) = &self.kind {
            api::text(kind, 64, false)?;
        }
        Ok(())
    }
}
impl Validate for Page {
    fn validate(&self) -> api::Result<()> {
        if !(1..=100).contains(&self.limit) || !(4096..=MAX_PAGE_BYTES).contains(&self.max_bytes) {
            return Err(api::MemoryError::invalid("invalid rich page bounds"));
        }
        for value in self.cursor.iter().chain(self.expected_snapshot.iter()) {
            api::text(value, 2048, false)?;
        }
        if self.cursor.is_some() && self.expected_snapshot.is_none() {
            return Err(api::MemoryError::invalid(
                "cursor requires expected_snapshot",
            ));
        }
        Ok(())
    }
}
impl Inspect {
    pub fn filter(&self) -> Option<&Filter> {
        match self {
            Self::Schema | Self::Outcome { .. } => None,
            Self::Summary { filter }
            | Self::Records { filter, .. }
            | Self::GraphNodes { filter, .. }
            | Self::GraphEdges { filter, .. }
            | Self::Detail { filter, .. }
            | Self::Stats { filter }
            | Self::Profile { filter }
            | Self::Style { filter }
            | Self::Frozen { filter }
            | Self::Diagnostics { filter } => Some(filter),
        }
    }
}
impl Validate for Inspect {
    fn validate(&self) -> api::Result<()> {
        if let Some(filter) = self.filter() {
            filter.validate()?;
        }
        match self {
            Self::Records { page, .. }
            | Self::GraphNodes { page, .. }
            | Self::GraphEdges { page, .. } => page.validate()?,
            Self::Detail {
                reference,
                expected_snapshot,
                ..
            } => {
                reference.validate()?;
                if let Some(s) = expected_snapshot {
                    api::opaque_id(s)?;
                }
            }
            Self::Outcome { operation_id } => api::opaque_id(operation_id)?,
            _ => {}
        }
        Ok(())
    }
}
impl Validate for Target {
    fn validate(&self) -> api::Result<()> {
        self.reference.validate()?;
        if self.expected_revision == 0 {
            return Err(api::MemoryError::invalid(
                "expected_revision must be positive",
            ));
        }
        Ok(())
    }
}
impl Validate for Draft {
    fn validate(&self) -> api::Result<()> {
        api::text(&self.content, api::MAX_TEXT, true)?;
        api::text(&self.name, 256, false)?;
        api::text(&self.description, 2048, false)?;
        api::text(&self.kind, 64, false)?;
        if self.content.trim().is_empty()
            || !matches!(
                self.scope,
                ViewScope::User | ViewScope::Device | ViewScope::Project
            )
        {
            return Err(api::MemoryError::invalid(
                "invalid draft content or placement",
            ));
        }
        Ok(())
    }
}
impl Validate for Manage {
    fn validate(&self) -> api::Result<()> {
        self.filter.validate()?;
        api::opaque_id(&self.expected_snapshot)?;
        let mut targets = Vec::new();
        match &self.action {
            Action::Create { draft } => draft.validate()?,
            Action::Update { target, draft } => {
                targets.push(target);
                draft.validate()?;
            }
            Action::Forget { target }
            | Action::QueuePromote { target }
            | Action::QueueDrop { target }
            | Action::Revive { target } => targets.push(target),
            Action::Restore { target, as_id } => {
                targets.push(target);
                if let Some(id) = as_id {
                    api::opaque_id(id)?;
                }
            }
            Action::QueueClear { targets: ts } => targets.extend(ts),
            Action::Purge {
                targets: ts,
                confirmed,
            }
            | Action::CompactApply {
                targets: ts,
                confirmed,
            } => {
                if !confirmed {
                    return Err(api::MemoryError::invalid("explicit confirmation required"));
                }
                targets.extend(ts);
            }
            Action::Merge { pairs } | Action::ConsolidateApply { pairs, .. } => {
                if matches!(
                    &self.action,
                    Action::ConsolidateApply {
                        confirmed: false,
                        ..
                    }
                ) {
                    return Err(api::MemoryError::invalid("explicit confirmation required"));
                }
                if pairs.len() > 50 {
                    return Err(api::MemoryError::invalid("too many merge pairs"));
                }
                let duplicate_refs = pairs
                    .iter()
                    .map(|p| &p.duplicate.reference)
                    .collect::<std::collections::BTreeSet<_>>();
                for p in pairs {
                    p.keep.validate()?;
                    if duplicate_refs.contains(&p.keep.reference) {
                        return Err(api::MemoryError::invalid(
                            "merge keeper is also a duplicate target",
                        ));
                    }
                    targets.push(&p.duplicate);
                }
            }
            Action::StyleSet { text } => api::text(text, api::MAX_TEXT, true)?,
            Action::ReconcilePreview {
                binding,
                completion,
            }
            | Action::ReconcileApply {
                binding,
                completion,
                ..
            } => {
                api::opaque_id(binding)?;
                api::text(completion, api::MAX_TEXT, true)?;
                if completion.trim().is_empty()
                    || matches!(
                        &self.action,
                        Action::ReconcileApply {
                            confirmed: false,
                            ..
                        }
                    )
                {
                    return Err(api::MemoryError::invalid(
                        "explicit completion/confirmation required",
                    ));
                }
            }
            _ => {}
        }
        if targets.len() > 100 {
            return Err(api::MemoryError::invalid("too many management targets"));
        }
        let mut unique = std::collections::BTreeSet::new();
        for t in targets {
            t.validate()?;
            if !unique.insert(&t.reference) {
                return Err(api::MemoryError::invalid("duplicate management target"));
            }
        }
        Ok(())
    }
}
