use super::reference_compare::{
    ComparisonBinding, ComparisonReceipt, ComparisonRequest, FidelityMode,
};
use crate::core::tool_media::PixelRect;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

pub const STATE_VERSION: u32 = 1;
pub const MAX_REPAIRS: u8 = 2;
pub const MAX_CAPTURES_PER_PASS: u8 = 4;
/// Aggregate attempts, including failures, across viewport changes, captures and image reads.
pub const MAX_VISUAL_OPERATIONS: u8 = 36;
const MAX_EVIDENCE: usize = 12;
const MAX_FIELD_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignMode {
    #[default]
    Auto,
    On,
    Off,
}
impl fmt::Display for DesignMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        })
    }
}
impl FromStr for DesignMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err("design mode must be auto, on, or off".into()),
        }
    }
}

/// Semantic intent, supplied by the model from the user's actual requested change, not keywords.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignIntent {
    Nonvisual,
    Create,
    Redesign,
    Preserve,
    #[default]
    Unknown,
}
impl DesignIntent {
    pub fn is_visual(self) -> bool {
        matches!(self, Self::Create | Self::Redesign | Self::Preserve)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignBrief {
    pub intent: DesignIntent,
    #[serde(default)]
    pub audience: String,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub palette: String,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub assets: String,
    #[serde(default)]
    pub responsive: String,
    #[serde(default)]
    pub interactions: String,
    #[serde(default)]
    pub acceptance: String,
    /// Exact user-requested sizes; None keeps the current task's requirements/defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_viewports: Option<Vec<Viewport>>,
    /// Identity constraints only, never reference access/transmission authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_reference_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity_mode: Option<FidelityMode>,
}
impl DesignBrief {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in self.fields() {
            if value.len() > MAX_FIELD_BYTES
                || value
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                bail!("design brief {name} must be at most {MAX_FIELD_BYTES} UTF-8 bytes without control characters");
            }
        }
        if let Some(viewports) = &self.required_viewports {
            validate_required_viewports(viewports)?;
        }
        if let Some(ids) = &self.required_reference_ids {
            validate_reference_ids(ids)?;
            if !self.intent.is_visual() {
                bail!("selected references require a visual intent");
            }
        }
        if self.intent.is_visual()
            && (self.brief.trim().is_empty() || self.acceptance.trim().is_empty())
        {
            bail!("visual intent needs a brief and observable acceptance criteria");
        }
        Ok(())
    }
    fn fields(&self) -> [(&str, &str); 9] {
        [
            ("audience", &self.audience),
            ("brief", &self.brief),
            ("type", &self.r#type),
            ("palette", &self.palette),
            ("layout", &self.layout),
            ("assets", &self.assets),
            ("responsive", &self.responsive),
            ("interactions", &self.interactions),
            ("acceptance", &self.acceptance),
        ]
    }
    pub fn constraints(&self) -> String {
        let mut out = format!("intent={:?}; ", self.intent);
        if let Some(ids) = &self.required_reference_ids {
            out.push_str(&format!(
                "references: {}; fidelity: {:?}; ",
                ids.join(", "),
                self.fidelity_mode.unwrap_or(FidelityMode::Exact)
            ));
        }
        if let Some(viewports) = &self.required_viewports {
            out.push_str("required viewports: ");
            for viewport in viewports {
                out.push_str(&format!("{}x{}; ", viewport.width, viewport.height));
            }
        }
        for (name, value) in self
            .fields()
            .into_iter()
            .filter(|(_, v)| !v.trim().is_empty())
        {
            out.push_str(&format!("{name}: {}; ", bounded(value, 160)));
        }
        bounded(&out, 900)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}
impl Viewport {
    fn validate(self) -> Result<()> {
        if self.width == 0 || self.height == 0 || self.width > 16384 || self.height > 16384 {
            bail!("invalid design viewport dimensions");
        }
        Ok(())
    }
}

fn validate_required_viewports(viewports: &[Viewport]) -> Result<()> {
    if viewports.is_empty() || viewports.len() > 4 {
        bail!("require between one and four unique design viewports");
    }
    for (index, viewport) in viewports.iter().enumerate() {
        viewport.validate()?;
        // Required sizes must be capturable by the bounded native browser backend.
        if viewport.width > 4096
            || viewport.height > 4096
            || u64::from(viewport.width) * u64::from(viewport.height) > 8_388_608
        {
            bail!("required viewport exceeds 4096 pixels per side or 8,388,608 pixels total");
        }
        if viewports[..index].contains(viewport) {
            bail!("required design viewports must be unique");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Pending,
    Verified,
    Failed,
    Unavailable,
    Stale,
    SkippedByUser,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualEvidence {
    pub id: String,
    pub owner_id: String,
    pub task_id: String,
    pub status: EvidenceStatus,
    pub source_hash: Option<String>,
    pub target: String,
    pub viewport: Viewport,
    pub render_revision: u64,
    pub projected: bool,
    pub ready: bool,
    pub pass: u8,
    pub note: String,
    #[serde(default)]
    pub capture_identity: Option<CaptureIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureIdentity {
    pub output_sha256: String,
    pub dpr_milli: u32,
    /// Runtime-observed platform face signature; not a font-file/version identity claim.
    pub font_identity: String,
    /// Stable repeated observed raster/viewport signature; not all-future-frame determinism.
    pub frame_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "crop", rename_all = "snake_case")]
pub enum ReferenceObservationKind {
    Original,
    Preview,
    Crop(PixelRect),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceObservation {
    pub reference_id: String,
    pub original_sha256: String,
    pub kind: ReferenceObservationKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceRequirement {
    pub reference_id: String,
    /// Set only by live trusted registry/controller, never model-visible authority.
    pub original_sha256: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasuredComparison {
    pub binding: ComparisonBinding,
    pub accepted: Option<bool>,
    #[serde(default)]
    pub capture_identity_verified: bool,
}
fn validate_reference_ids(ids: &[String]) -> Result<()> {
    if ids.is_empty() || ids.len() > crate::core::tool_media::MAX_REQUEST_IMAGES {
        bail!("require one to four unique reference IDs");
    }
    for (i, id) in ids.iter().enumerate() {
        validate_label("reference id", id, 128)?;
        if ids[..i].contains(id) {
            bail!("duplicate reference ID");
        }
    }
    Ok(())
}

/// A bounded, portable snapshot. It contains no credentials, screenshots, callbacks or process state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDesignState {
    pub version: u32,
    pub owner_id: String,
    pub task_id: String,
    pub workspace: String,
    pub mode: DesignMode,
    pub intent: DesignIntent,
    pub active: bool,
    pub archived: bool,
    pub brief: Option<DesignBrief>,
    pub prior_brief: Option<DesignBrief>,
    pub prior_task_id: Option<String>,
    pub source_hash: Option<String>,
    pub target: Option<String>,
    pub render_revision: u64,
    pub evidence: Vec<VisualEvidence>,
    pub required_viewports: Vec<Viewport>,
    pub repairs: u8,
    pub captures_this_pass: u8,
    #[serde(default)]
    pub visual_operations: u8,
    pub skipped_by_user: Option<String>,
    pub unavailable: Option<String>,
    #[serde(default)]
    pub references: Vec<ReferenceRequirement>,
    #[serde(default)]
    pub reference_observations: Vec<ReferenceObservation>,
    #[serde(default)]
    pub comparisons: Vec<MeasuredComparison>,
    /// Frozen by trusted controller; restored snapshots do not restore measured acceptance.
    #[serde(default)]
    pub comparison_request: Option<ComparisonRequest>,
}
impl TaskDesignState {
    fn new(owner_id: String, workspace: String, mode: DesignMode) -> Self {
        Self {
            version: STATE_VERSION,
            owner_id,
            task_id: String::new(),
            workspace,
            mode,
            intent: DesignIntent::Unknown,
            active: false,
            archived: false,
            brief: None,
            prior_brief: None,
            prior_task_id: None,
            source_hash: None,
            target: None,
            render_revision: 0,
            evidence: Vec::new(),
            required_viewports: Vec::new(),
            repairs: 0,
            captures_this_pass: 0,
            visual_operations: 0,
            skipped_by_user: None,
            unavailable: None,
            references: Vec::new(),
            reference_observations: Vec::new(),
            comparisons: Vec::new(),
            comparison_request: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Disabled,
    NotRequired,
    Verified,
    Continue(String),
    Unavailable(String),
    Failed(String),
    SkippedByUser(String),
}
impl GateDecision {
    /// A bounded/unavailable run may finish INCOMPLETE, never claim visual verification or Done.
    pub fn blocks_completion(&self) -> bool {
        matches!(
            self,
            Self::Continue(_) | Self::Unavailable(_) | Self::Failed(_)
        )
    }
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Continue(s) | Self::Unavailable(s) | Self::Failed(s) | Self::SkippedByUser(s) => {
                Some(s)
            }
            _ => None,
        }
    }
    #[cfg(test)]
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// Clone shares one run; fork_child makes a new mutex and copies only bounded brief constraints.
#[derive(Debug, Clone)]
pub struct DesignContext(Arc<Mutex<TaskDesignState>>);
impl Default for DesignContext {
    fn default() -> Self {
        Self::new("default", ".", DesignMode::Auto)
    }
}
impl DesignContext {
    pub fn new(
        owner_id: impl Into<String>,
        workspace: impl Into<String>,
        mode: DesignMode,
    ) -> Self {
        Self(Arc::new(Mutex::new(TaskDesignState::new(
            owner_id.into(),
            workspace.into(),
            mode,
        ))))
    }
    fn with<T>(&self, f: impl FnOnce(&mut TaskDesignState) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn snapshot(&self) -> TaskDesignState {
        self.with(|s| s.clone())
    }
    /// Unambiguous owner+task namespace used by typed image provenance, including delegated scopes.
    pub fn scope(&self) -> String {
        self.with(|s| {
            crate::core::tool_media::sha256(
                format!(
                    "{}:{}{}:{}",
                    s.owner_id.len(),
                    s.owner_id,
                    s.task_id.len(),
                    s.task_id
                )
                .as_bytes(),
            )
        })
    }
    pub fn evidence_scope(&self) -> String {
        self.scope()
    }
    #[cfg(test)]
    pub fn mode(&self) -> DesignMode {
        self.with(|s| s.mode)
    }
    pub fn set_mode(&self, mode: DesignMode) {
        self.with(|s| s.mode = mode);
    }
    #[cfg(test)]
    pub fn brief(&self) -> Option<DesignBrief> {
        self.with(|s| s.brief.clone())
    }
    #[cfg(any(feature = "browser", test))]
    pub fn current_revision(&self) -> u64 {
        self.with(|s| s.render_revision)
    }
    pub fn set_intent(&self, intent: DesignIntent) {
        self.with(|s| {
            // Unknown cannot erase an already grounded visual contract in this run.
            if s.brief.as_ref().is_some_and(|b| b.intent.is_visual()) && !intent.is_visual() {
                return;
            }
            s.intent = intent;
            if intent.is_visual() {
                s.active = true;
                s.archived = false;
            }
        });
    }
    pub fn status(&self) -> String {
        self.with(|s| {
            format!(
                "design: {} · {:?} · {} · {} evidence · repair {}/{} (session only)",
                s.mode,
                s.intent,
                if s.active {
                    "active"
                } else if s.brief.is_some() || s.prior_brief.is_some() {
                    "prior brief; resume explicitly"
                } else {
                    "no brief"
                },
                s.evidence.len(),
                s.repairs,
                MAX_REPAIRS
            )
        })
    }
    pub fn reset(&self) {
        self.with(|s| *s = TaskDesignState::new(s.owner_id.clone(), s.workspace.clone(), s.mode));
    }
    /// Called once at a NEW user turn, never on retries, continuation or compaction of that run.
    pub fn begin_turn(&self, task_id: impl Into<String>, workspace: impl Into<String>) {
        let task_id = task_id.into();
        let workspace = workspace.into();
        self.with(|s| {
            let same_workspace = s.workspace == workspace;
            let had_current = s.brief.is_some();
            let prior = if same_workspace {
                s.brief.take().or_else(|| s.prior_brief.take())
            } else {
                None
            };
            let prior_task = if same_workspace && prior.is_some() {
                if had_current && !s.task_id.is_empty() {
                    Some(s.task_id.clone())
                } else {
                    s.prior_task_id.clone()
                }
            } else {
                None
            };
            let mut next = TaskDesignState::new(s.owner_id.clone(), workspace, s.mode);
            next.task_id = task_id;
            next.prior_brief = prior;
            next.prior_task_id = prior_task;
            *s = next;
        });
    }
    pub fn finalize(&self) {
        self.with(|s| {
            s.active = false;
            s.archived = true;
        });
    }
    pub fn fork_child(
        &self,
        child_task_id: impl Into<String>,
        workspace: impl Into<String>,
    ) -> Self {
        let parent = self.snapshot();
        let child_task_id = child_task_id.into();
        let ctx = Self::new(
            format!("{}/child/{child_task_id}", parent.owner_id),
            workspace,
            parent.mode,
        );
        ctx.with(|s| {
            s.task_id = child_task_id;
            s.prior_brief = parent.brief.or(parent.prior_brief);
            s.prior_task_id = Some(parent.task_id);
        });
        ctx
    }
    pub fn inherited_constraints(&self) -> Option<String> {
        self.with(|s| {
            s.brief
                .as_ref()
                .or(s.prior_brief.as_ref())
                .map(DesignBrief::constraints)
        })
    }
    /// Trusted reference handoff must be seated BEFORE planning, never grants network/file access.
    pub fn activate_selected_references(
        &self,
        references: Vec<(String, String)>,
        mode: FidelityMode,
    ) -> Result<()> {
        let ids: Vec<String> = references.iter().map(|(id, _)| id.clone()).collect();
        validate_reference_ids(&ids)?;
        for (_, hash) in &references {
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("reference original requires SHA-256");
            }
        }
        self.with(|s| {
            let mut brief = s.brief.clone().or_else(|| s.prior_brief.clone()).unwrap_or_else(|| DesignBrief {
                intent: DesignIntent::Preserve,
                brief: "Implement the user's selected reference using measured composition and original geometry.".into(),
                acceptance: "Inspect selected references, capture current implementation and measure whole-frame fidelity; retain functioning semantic UI.".into(),
                ..Default::default()
            });
            if !brief.intent.is_visual() { brief.intent = DesignIntent::Preserve; }
            brief.required_reference_ids = Some(ids);
            brief.fidelity_mode = Some(mode);
            brief.validate()?;
            s.required_viewports = brief.required_viewports.clone().unwrap_or_default();
            s.intent = brief.intent; s.brief = Some(brief); s.active = true; s.archived = false;
            // Selected-reference contracts are mandatory even if parent guidance was off.
            if s.mode == DesignMode::Off { s.mode = DesignMode::Auto; }
            s.references = references.into_iter().map(|(reference_id,original_sha256)| ReferenceRequirement {reference_id,original_sha256:Some(original_sha256)}).collect();
            s.comparison_request = Some(ComparisonRequest { mode, ..Default::default() });
            s.reference_observations.clear(); s.comparisons.clear();
            invalidate(s);
            Ok(())
        })
    }
    /// Freeze authorized near-match limits/regions. Models cannot invoke this through design_brief.
    pub fn set_comparison_requirements(&self, request: ComparisonRequest) -> Result<()> {
        request.validate()?;
        self.with(|s| {
            if s.references.is_empty() {
                bail!("comparison requirements need selected reference contract");
            }
            s.comparison_request = Some(request.clone());
            if let Some(brief) = &mut s.brief {
                brief.fidelity_mode = Some(request.mode);
            }
            invalidate(s);
            Ok(())
        })
    }
    /// Called only after an accepted successful live request includes this exact occurrence.
    pub fn record_reference_observation(
        &self,
        id: &str,
        original_hash: &str,
        kind: ReferenceObservationKind,
    ) -> Result<()> {
        self.with(|s| {
            let required = s
                .references
                .iter()
                .find(|r| r.reference_id == id)
                .ok_or_else(|| anyhow::anyhow!("reference is not selected for this design task"))?;
            if required.original_sha256.as_deref() != Some(original_hash) {
                bail!("reference observation original hash mismatch");
            }
            if let ReferenceObservationKind::Crop(crop) = &kind {
                if crop.width == 0
                    || crop.height == 0
                    || crop.x.checked_add(crop.width).is_none()
                    || crop.y.checked_add(crop.height).is_none()
                {
                    bail!("invalid observed reference crop");
                }
            }
            if !s.reference_observations.iter().any(|r| {
                r.reference_id == id && r.original_sha256 == original_hash && r.kind == kind
            }) {
                if s.reference_observations.len() >= 16 {
                    bail!("reference observation budget exhausted");
                }
                s.reference_observations.push(ReferenceObservation {
                    reference_id: id.into(),
                    original_sha256: original_hash.into(),
                    kind,
                });
            }
            Ok(())
        })
    }
    /// Pure local measurement cannot itself certify successful model delivery or model critique.
    pub fn record_comparison(&self, receipt: ComparisonReceipt) -> Result<()> {
        self.with(|s| {
            let binding = receipt.binding();
            let required = s
                .references
                .iter()
                .find(|r| r.reference_id == binding.reference_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("comparison reference is not selected in this task")
                })?;
            if required.original_sha256.as_deref() != Some(binding.reference_sha256.as_str())
                || s.comparison_request.as_ref() != Some(&binding.request)
            {
                bail!("comparison does not match frozen original/requirements");
            }
            let e = s
                .evidence
                .iter()
                .find(|e| {
                    e.id == binding.output_evidence_id
                        && e.owner_id == s.owner_id
                        && e.task_id == s.task_id
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("comparison output is not scoped implementation evidence")
                })?;
            if !e.ready
                || e.status == EvidenceStatus::Stale
                || e.render_revision != s.render_revision
                || e.render_revision != binding.render_revision
                || e.source_hash.as_deref() != Some(binding.source_hash.as_str())
                || e.source_hash != s.source_hash
                || e.viewport != binding.viewport
                || e.target != binding.target
            {
                bail!(
                    "comparison output is stale or does not match current capture/source/viewport"
                );
            }
            let capture_identity_verified = e.capture_identity.as_ref().is_some_and(|identity| {
                identity.output_sha256 == binding.output_sha256
                    && identity.dpr_milli == binding.dpr_milli
                    && identity.font_identity == binding.font_identity
                    && identity.frame_state == binding.frame_state
            });
            if s.comparisons.len() >= MAX_EVIDENCE {
                bail!("comparison receipt budget exhausted");
            }
            s.comparisons.retain(|c| {
                !(c.binding.output_evidence_id == binding.output_evidence_id
                    && c.binding.reference_id == binding.reference_id)
            });
            s.comparisons.push(MeasuredComparison {
                binding: binding.clone(),
                accepted: receipt.accepted(),
                capture_identity_verified,
            });
            Ok(())
        })
    }
    pub fn apply_brief(&self, brief: DesignBrief, resume: bool) -> Result<()> {
        brief.validate()?;
        self.with(|s| {
            if resume && (brief.fields().iter().any(|(_, value)| !value.is_empty()) || brief.required_viewports.is_some() || brief.required_reference_ids.is_some() || brief.fidelity_mode.is_some() || brief.intent != DesignIntent::Unknown) {
                bail!("resume uses the prior brief; omit replacement brief fields and viewports");
            }
            let mut selected = if resume {
                s.brief.clone().or_else(|| s.prior_brief.clone()).ok_or_else(|| anyhow::anyhow!("no prior design brief to resume"))?
            } else { brief };
            selected.validate()?;
            if s.brief.as_ref().is_some_and(|b| b.intent.is_visual()) && !selected.intent.is_visual() {
                bail!("a current visual brief cannot be downgraded by the model; use the session control for unrelated work");
            }
            if !s.references.is_empty() {
                let ids: Vec<String> = s.references.iter().map(|r| r.reference_id.clone()).collect();
                if selected.required_reference_ids.as_ref().is_some_and(|requested| requested != &ids)
                    || selected.fidelity_mode.is_some_and(|mode| Some(mode) != s.comparison_request.as_ref().map(|r| r.mode)) {
                    bail!("model brief cannot replace selected reference IDs or frozen fidelity requirements");
                }
                selected.required_reference_ids = Some(ids);
                selected.fidelity_mode = s.comparison_request.as_ref().map(|r| r.mode);
            } else if let Some(ids) = &selected.required_reference_ids {
                s.references = ids.iter().map(|id| ReferenceRequirement {reference_id:id.clone(),original_sha256:None}).collect();
                s.comparison_request = Some(ComparisonRequest {mode:selected.fidelity_mode.unwrap_or(FidelityMode::Exact),..Default::default()});
            }
            if let Some(viewports) = &selected.required_viewports {
                s.required_viewports = viewports.clone();
            } else if !s.required_viewports.is_empty() {
                selected.required_viewports = Some(s.required_viewports.clone());
            }
            s.intent = selected.intent;
            s.brief = Some(selected);
            s.active = true;
            s.archived = false;
            // A changed/resumed contract must earn current-run evidence again.
            for e in &mut s.evidence { e.status = EvidenceStatus::Stale; }
            s.comparisons.clear();
            s.skipped_by_user = None;
            s.unavailable = None;
            Ok(())
        })
    }
    /// Restore is a pointer, not a claim that the saved browser/source revision is still current.
    pub fn restore(&self, mut saved: TaskDesignState) -> Result<()> {
        self.with(|s| {
            if saved.version != STATE_VERSION
                || saved.owner_id != s.owner_id
                || saved.workspace != s.workspace
            {
                bail!("design snapshot version, owner or workspace mismatch");
            }
            if saved.evidence.len() > MAX_EVIDENCE
                || saved.repairs > MAX_REPAIRS
                || saved.captures_this_pass > MAX_CAPTURES_PER_PASS
                || saved.visual_operations > MAX_VISUAL_OPERATIONS
                || saved.required_viewports.len() > 4
                || saved.references.len() > 4
                || saved.reference_observations.len() > 16
                || saved.comparisons.len() > MAX_EVIDENCE
            {
                bail!("design snapshot exceeds bounded evidence budget");
            }
            if !saved.required_viewports.is_empty() {
                validate_required_viewports(&saved.required_viewports)?;
            }
            for brief in [&saved.brief, &saved.prior_brief].into_iter().flatten() {
                brief.validate()?;
            }
            for e in &mut saved.evidence {
                if e.owner_id != saved.owner_id || e.task_id != saved.task_id {
                    bail!("design evidence namespace mismatch");
                }
                e.viewport.validate()?;
                e.status = EvidenceStatus::Stale;
                e.projected = false;
                e.ready = false;
                e.capture_identity = None;
            }
            // Saved IDs/provenance survive; runtime grants, inspection and comparisons do not.
            saved.references.clear();
            if let Some(ids) = saved
                .brief
                .as_ref()
                .and_then(|b| b.required_reference_ids.as_ref())
            {
                saved.references = ids
                    .iter()
                    .map(|id| ReferenceRequirement {
                        reference_id: id.clone(),
                        original_sha256: None,
                    })
                    .collect();
            }
            saved.reference_observations.clear();
            saved.comparisons.clear();
            saved.comparison_request = saved.brief.as_ref().and_then(|b| {
                b.required_reference_ids
                    .as_ref()
                    .map(|_| ComparisonRequest {
                        mode: b.fidelity_mode.unwrap_or(FidelityMode::Exact),
                        ..Default::default()
                    })
            });
            saved.active = false;
            saved.source_hash = None;
            saved.skipped_by_user = None;
            saved.unavailable = None;
            *s = saved;
            Ok(())
        })
    }
    /// Explicit session switch: validate a foreign session snapshot in an isolated handle before
    /// adopting it. Workspace ownership remains strict, and all saved evidence becomes stale.
    pub fn restore_session(&self, saved: TaskDesignState) -> Result<()> {
        let current = self.snapshot();
        if saved.workspace != current.workspace {
            bail!("cannot restore design state from a foreign workspace");
        }
        let isolated = Self::new(saved.owner_id.clone(), current.workspace, current.mode);
        isolated.restore(saved)?;
        let mut validated = isolated.snapshot();
        validated.mode = current.mode; // session switch cannot overwrite the user's current control
        self.with(|s| *s = validated);
        Ok(())
    }
    #[cfg(any(feature = "browser", test))]
    pub fn set_target(&self, target: &str) -> Result<()> {
        validate_label("target", target, 2048)?;
        self.with(|s| {
            if s.target.as_deref() != Some(target) {
                invalidate(s);
                s.target = Some(target.to_string());
            }
        });
        Ok(())
    }
    /// Trusted runtime fingerprint of the actual workspace inputs, NOT a model-supplied hash.
    #[cfg(any(feature = "browser", test))]
    pub fn set_source_revision(&self, source_hash: &str) -> Result<()> {
        validate_label("source hash", source_hash, 128)?;
        self.with(|s| {
            if s.source_hash.as_deref() != Some(source_hash) {
                invalidate(s);
                s.source_hash = Some(source_hash.to_string());
            }
        });
        Ok(())
    }
    pub fn mark_render_changed(&self) {
        self.with(invalidate);
    }
    #[cfg(test)]
    pub fn require_viewports(&self, viewports: Vec<Viewport>) -> Result<()> {
        validate_required_viewports(&viewports)?;
        self.with(|s| {
            if let Some(brief) = &mut s.brief {
                brief.required_viewports = Some(viewports.clone());
            }
            s.required_viewports = viewports;
        });
        Ok(())
    }
    /// Reserve before approved native visual work starts, so failures also consume the task budget.
    /// Repairs, brief edits, resume and compaction never replenish these attempts.
    pub fn reserve_visual_operation(&self) -> Result<()> {
        self.with(|s| {
            if s.mode == DesignMode::Off || !s.active || !s.intent.is_visual() {
                bail!("visual operation reservation requires an active visual design task");
            }
            if s.visual_operations >= MAX_VISUAL_OPERATIONS {
                bail!("visual operation budget exhausted (36); report incomplete, do not keep retrying");
            }
            s.visual_operations += 1;
            Ok(())
        })
    }
    /// Called by screenshot runtime. source_hash is Some ONLY when the ACTUAL rendered build marker
    /// binds to the current trusted source fingerprint; readiness includes fonts/images/layout settle.
    #[cfg(any(feature = "browser", test))]
    #[allow(clippy::too_many_arguments)]
    pub fn record_capture(
        &self,
        id: &str,
        source_hash: Option<&str>,
        target: &str,
        viewport: Viewport,
        render_revision: u64,
        ready: bool,
        note: &str,
    ) -> Result<()> {
        validate_label("evidence id", id, 128)?;
        validate_label("target", target, 2048)?;
        viewport.validate()?;
        self.with(|s| {
            if s.captures_this_pass >= MAX_CAPTURES_PER_PASS {
                bail!("design capture budget exhausted for this pass (4)");
            }
            if s.evidence.iter().any(|e| e.id == id) {
                bail!("duplicate design evidence id in this task");
            }
            s.captures_this_pass += 1;
            let fresh = ready
                && render_revision == s.render_revision
                && source_hash.is_some()
                && source_hash == s.source_hash.as_deref()
                && s.target.as_deref() == Some(target);
            s.evidence.push(VisualEvidence {
                id: id.into(),
                owner_id: s.owner_id.clone(),
                task_id: s.task_id.clone(),
                status: if fresh {
                    EvidenceStatus::Pending
                } else {
                    EvidenceStatus::Stale
                },
                source_hash: source_hash.map(str::to_string),
                target: target.into(),
                viewport,
                render_revision,
                projected: false,
                ready,
                pass: s.repairs,
                note: bounded(note, 512),
                capture_identity: None,
            });
            Ok(())
        })
    }
    /// Runtime-only identity; readiness, an expected build marker or model text is insufficient.
    #[cfg(any(feature = "browser", test))]
    pub fn record_capture_identity(&self, id: &str, identity: CaptureIdentity) -> Result<()> {
        if identity.output_sha256.len() != 64
            || !identity
                .output_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || identity.dpr_milli == 0
            || identity.dpr_milli > 16000
        {
            bail!("invalid capture hash/DPR identity");
        }
        validate_label("font identity", &identity.font_identity, 128)?;
        validate_label("frame state", &identity.frame_state, 128)?;
        self.with(|s| {
            let e = s
                .evidence
                .iter_mut()
                .find(|e| e.id == id && e.owner_id == s.owner_id && e.task_id == s.task_id)
                .ok_or_else(|| anyhow::anyhow!("capture identity is not from this task"))?;
            if e.status == EvidenceStatus::Stale || e.render_revision != s.render_revision {
                bail!("cannot bind stale capture identity");
            }
            if e.capture_identity
                .as_ref()
                .is_some_and(|old| old != &identity)
            {
                bail!("capture identity is immutable; recapture changed render");
            }
            e.capture_identity = Some(identity);
            Ok(())
        })
    }
    /// Only the request projector calls this, after real image parts have been seated in the request.
    pub fn mark_projected(&self, id: &str) -> Result<()> {
        self.with(|s| {
            let e = s
                .evidence
                .iter_mut()
                .find(|e| e.id == id && e.owner_id == s.owner_id && e.task_id == s.task_id)
                .ok_or_else(|| anyhow::anyhow!("evidence is not from this design task"))?;
            e.projected = true;
            Ok(())
        })
    }
    /// Model critique is a distinct operation from capture and projection. A review of a path, stale
    /// render, text-only result or unseen screenshot can NEVER become verified evidence.
    pub fn record_review(&self, id: &str, passed: bool, note: &str) -> Result<()> {
        validate_label("review note", note, 512)?;
        self.with(|s| {
            let e = s.evidence.iter_mut().find(|e| e.id == id && e.owner_id == s.owner_id && e.task_id == s.task_id)
                .ok_or_else(|| anyhow::anyhow!("evidence is not from this design task"))?;
            if !e.projected || !e.ready || e.render_revision != s.render_revision || e.source_hash.is_none()
                || e.source_hash != s.source_hash || s.target.as_deref() != Some(e.target.as_str()) || e.status == EvidenceStatus::Stale {
                bail!("visual review unavailable: need an actually projected, ready, current-source screenshot of this target/revision");
            }
            e.status = if passed { EvidenceStatus::Verified } else { EvidenceStatus::Failed };
            e.note = note.to_string();
            Ok(())
        })
    }
    pub fn begin_repair(&self) -> Result<()> {
        self.with(|s| {
            if s.repairs >= MAX_REPAIRS { bail!("design repair budget exhausted (2); report remaining issues, do not keep retrying"); }
            s.repairs += 1;
            s.captures_this_pass = 0;
            invalidate(s);
            Ok(())
        })
    }
    pub fn mark_unavailable(&self, note: &str) {
        self.with(|s| {
            s.unavailable = Some(bounded(note, 512));
        });
    }
    /// User/controller-only override. No native model tool can manufacture this exemption.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Reserved trusted controller override; model tools must not expose user consent authority"
        )
    )]
    pub fn skip_by_user(&self, reason: &str) -> Result<()> {
        validate_label("user skip reason", reason, 512)?;
        self.with(|s| {
            s.skipped_by_user = Some(reason.into());
            for e in &mut s.evidence {
                e.status = EvidenceStatus::SkippedByUser;
            }
        });
        Ok(())
    }
    pub fn gate(&self) -> GateDecision {
        self.with(evaluate)
    }
    /// Volatile USER-turn capsule only. Full brief is for an active run; later turns get a pointer.
    pub fn capsule(&self) -> Option<String> {
        self.with(|s| {
            if s.mode == DesignMode::Off { return None; }
            let active = s.active && (s.intent.is_visual() || s.mode == DesignMode::On);
            if active {
                let contract = s.brief.as_ref().map(DesignBrief::constraints).unwrap_or_default();
                let verdict = evaluate(s);
                Some(bounded(&format!("Design task {} (scope {}). {}\n{}\nEvidence: {:?}; revision {}; {} captures/pass, {} repairs maximum; visual operations {}/{} including failed calls. Only critique images actually shown; paths, DOM and stale builds are not visual verification. Preserve repository conventions and existing branding unless explicitly redesigning.\n{}",
                    bounded(&s.task_id, 128), bounded(&s.owner_id, 128), contract,
                    super::guide::FOUNDATION, verdict, s.render_revision, MAX_CAPTURES_PER_PASS, MAX_REPAIRS, s.visual_operations, MAX_VISUAL_OPERATIONS,
                    verdict.message().unwrap_or("Record grounded acceptance checks before finishing.")), 2400))
            } else if let Some(brief) = s.brief.as_ref().or(s.prior_brief.as_ref()) {
                Some(bounded(&format!("Prior design brief for task {}: {}. It is NOT active for this new turn. Resume explicitly with design_brief resume=true only if the user intends to continue it; do not apply old styling or visual gates to unrelated work.", s.prior_task_id.as_deref().unwrap_or(&s.task_id), bounded(&brief.brief, 240)), 600))
            } else if s.mode == DesignMode::On {
                Some(format!("Design guidance enabled for this session. Decide semantic create/redesign/preserve/unknown intent and record a bounded design brief before visual work. {}", super::guide::FOUNDATION))
            } else { None }
        })
    }
}

fn evaluate(s: &mut TaskDesignState) -> GateDecision {
    if s.mode == DesignMode::Off {
        return GateDecision::Disabled;
    }
    if !s.active || !s.intent.is_visual() {
        return GateDecision::NotRequired;
    }
    if s.brief.is_none() {
        if s.visual_operations >= MAX_VISUAL_OPERATIONS {
            return exhausted_operations();
        }
        return GateDecision::Continue("Visual intent identified: record a bounded design_brief with observable acceptance criteria before finishing.".into());
    }
    if let Some(reason) = &s.skipped_by_user {
        return GateDecision::SkippedByUser(format!(
            "Visual review skipped by user: {reason}; do not claim verified."
        ));
    }
    let current: Vec<&VisualEvidence> = s
        .evidence
        .iter()
        .filter(|e| {
            e.owner_id == s.owner_id
                && e.task_id == s.task_id
                && e.render_revision == s.render_revision
                && e.source_hash.is_some()
                && e.source_hash == s.source_hash
                && s.target.as_deref() == Some(e.target.as_str())
                && e.projected
                && e.ready
        })
        .collect();
    let checks = if s.required_viewports.is_empty() {
        // A responsive surface needs a real narrow and a real wide image. The controller can set
        // explicit target viewports for a native/fixed-size surface instead.
        current
            .iter()
            .any(|e| e.status == EvidenceStatus::Verified && e.viewport.width <= 480)
            && current
                .iter()
                .any(|e| e.status == EvidenceStatus::Verified && e.viewport.width >= 900)
    } else {
        s.required_viewports.iter().all(|v| {
            current
                .iter()
                .any(|e| e.viewport == *v && e.status == EvidenceStatus::Verified)
        })
    };
    let fidelity_pending = if s.references.is_empty() {
        None
    } else {
        let observed = s.references.iter().all(|r| {
            r.original_sha256.as_ref().is_some_and(|hash| {
                s.reference_observations.iter().any(|o| {
                    o.reference_id == r.reference_id
                        && &o.original_sha256 == hash
                        && o.kind == ReferenceObservationKind::Original
                })
            })
        });
        let comparisons_pass = checks
            && current
                .iter()
                .filter(|e| e.status == EvidenceStatus::Verified)
                .all(|e| {
                    s.references.iter().all(|r| {
                        s.comparisons.iter().any(|c| {
                            c.accepted == Some(true)
                                && c.capture_identity_verified
                                && c.binding.reference_id == r.reference_id
                                && r.original_sha256.as_deref()
                                    == Some(c.binding.reference_sha256.as_str())
                                && c.binding.output_evidence_id == e.id
                                && c.binding.render_revision == s.render_revision
                                && c.binding.source_hash
                                    == s.source_hash.clone().unwrap_or_default()
                                && Some(&c.binding.request) == s.comparison_request.as_ref()
                        })
                    })
                });
        if !observed {
            Some("Reference inspection pending: selected original occurrences need successful live delivery; previews/crops do not certify whole-original inspection.")
        } else if !comparisons_pass {
            Some("Reference fidelity pending/failed: need current whole-frame measurements and runtime-observed capture/platform-face/repeated-frame identity under frozen requirements; a model boolean or passing crop cannot waive mismatch or missing identity.")
        } else {
            None
        }
    };
    let checks = checks && fidelity_pending.is_none();
    let failed = current.iter().any(|e| e.status == EvidenceStatus::Failed);
    if s.visual_operations >= MAX_VISUAL_OPERATIONS {
        return if checks && !failed {
            GateDecision::Verified
        } else {
            exhausted_operations()
        };
    }
    if let Some(reason) = &s.unavailable {
        return GateDecision::Unavailable(format!(
            "Visual review unavailable: {reason}; report incomplete, not verified."
        ));
    }
    if checks && !failed {
        return GateDecision::Verified;
    }
    let note = if let Some(note) = fidelity_pending {
        note
    } else if failed {
        "Visual acceptance failed; repair concrete observed issues and capture the current revision again."
    } else {
        "Visual acceptance pending: capture ready current-source desktop and mobile views (or controller-required viewports), project the actual images, then review against the brief. DOM/build success is not visual evidence."
    };
    if s.repairs >= MAX_REPAIRS || s.captures_this_pass >= MAX_CAPTURES_PER_PASS {
        GateDecision::Failed(format!(
            "{note} Bounded review budget reached; report incomplete with remaining issues."
        ))
    } else {
        GateDecision::Continue(note.into())
    }
}
fn exhausted_operations() -> GateDecision {
    GateDecision::Failed(format!(
        "Visual operation budget exhausted ({MAX_VISUAL_OPERATIONS} attempts including failures); report incomplete with remaining issues, not verified."
    ))
}
fn invalidate(s: &mut TaskDesignState) {
    s.render_revision = s.render_revision.saturating_add(1);
    s.comparisons.clear();
    for e in &mut s.evidence {
        e.status = EvidenceStatus::Stale;
    }
    s.unavailable = None;
}
fn validate_label(name: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        bail!(
            "design {name} must be nonempty, at most {max} bytes and contain no control characters"
        );
    }
    Ok(())
}
pub(super) fn bounded(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(3);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}
