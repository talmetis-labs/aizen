//! Plan identity is document metadata, never execution permission or completion evidence.
use super::*;
use crate::agent::plan_document;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanFreshness {
    Valid,
    Missing,
    Invalid,
    IdentityChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAttachment {
    pub root: String,
    pub path: String,
    pub id: String,
    pub fingerprint: String,
    pub phases: Vec<String>,
    pub freshness: PlanFreshness,
    /// Restored metadata is historical until the bounded disk refresh succeeds.
    #[serde(skip)]
    pub(crate) live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemPhase {
    pub item_id: String,
    pub plan_id: String,
    pub phase_id: String,
}

impl PlanAttachment {
    pub(crate) fn load(root: &Path, path: &str) -> Result<Self> {
        let document = plan_document::read(root, path)?;
        let root = root.canonicalize()?;
        let attachment = Self {
            root: root.to_string_lossy().into_owned(),
            path: path.into(),
            id: document.parsed.id,
            fingerprint: document.fingerprint,
            phases: document.parsed.phases,
            freshness: PlanFreshness::Valid,
            live: true,
        };
        attachment.validate()?;
        Ok(attachment)
    }

    fn validate(&self) -> Result<()> {
        bounded(&self.root, MAX_TEXT, "plan workspace")?;
        ensure!(
            Path::new(&self.root).is_absolute(),
            "plan workspace must be absolute"
        );
        plan_document::validate_relative_path(&self.path)?;
        ensure!(plan_document::valid_plan_id(&self.id), "invalid plan ID");
        ensure!(
            self.fingerprint.len() == 64
                && self
                    .fingerprint
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid plan fingerprint"
        );
        ensure!(
            !self.phases.is_empty() && self.phases.len() <= MAX_ITEMS,
            "invalid plan phases"
        );
        let mut seen = HashSet::new();
        for phase in &self.phases {
            ensure!(
                plan_document::valid_phase_id(phase) && seen.insert(phase),
                "invalid or duplicate phase ID"
            );
        }
        Ok(())
    }

    pub(crate) fn usable(&self) -> bool {
        self.live && self.freshness == PlanFreshness::Valid
    }

    fn refreshed_document(&self) -> (Self, Option<plan_document::ReadPlan>) {
        let mut next = self.clone();
        let mut document = None;
        next.live = true;
        match plan_document::read_bytes(Path::new(&self.root), &self.path) {
            Ok(bytes) => {
                next.fingerprint = plan_document::fingerprint(&bytes);
                match plan_document::parse(&bytes) {
                    Ok(parsed) if parsed.id == self.id => {
                        next.phases = parsed.phases.clone();
                        next.freshness = PlanFreshness::Valid;
                        document = Some(plan_document::ReadPlan {
                            parsed,
                            fingerprint: next.fingerprint.clone(),
                            excerpt: plan_document::excerpt(&bytes),
                        });
                    }
                    Ok(_) => next.freshness = PlanFreshness::IdentityChanged,
                    Err(_) => next.freshness = PlanFreshness::Invalid,
                }
            }
            Err(error) => {
                next.freshness = if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                }) {
                    PlanFreshness::Missing
                } else {
                    PlanFreshness::Invalid
                };
            }
        }
        // A different identity cannot silently regain its old authority by changing back.
        if self.freshness == PlanFreshness::IdentityChanged {
            next.freshness = PlanFreshness::IdentityChanged;
            document = None;
        }
        (next, document)
    }
}

impl TaskState {
    pub(crate) fn completion_basis(&self, item_id: &str) -> CompletionBasis {
        let Some(req) = self.requirement(item_id) else {
            return CompletionBasis::NotRequired;
        };
        if !self.satisfies(req) {
            return CompletionBasis::Unverified;
        }
        match req.requirement {
            Requirement::Observe { .. } => CompletionBasis::Observed,
            Requirement::Check { .. } => CompletionBasis::Verified,
            Requirement::Report => CompletionBasis::Reported,
            Requirement::Unspecified => CompletionBasis::Unverified,
        }
    }

    pub(super) fn validate_plan(&self) -> Result<()> {
        if let Some(plan) = &self.plan {
            plan.validate()?;
        }
        ensure!(
            self.item_phases.len() <= MAX_ITEMS + MAX_HISTORY,
            "too many phase links"
        );
        let mut seen = HashSet::new();
        for link in &self.item_phases {
            ensure!(
                self.items
                    .iter()
                    .chain(&self.history)
                    .any(|row| row.id == link.item_id)
                    && seen.insert(&link.item_id)
                    && plan_document::valid_plan_id(&link.plan_id)
                    && plan_document::valid_phase_id(&link.phase_id),
                "invalid phase link"
            );
        }
        Ok(())
    }

    pub(crate) fn phase_resolved(&self, item: &str) -> bool {
        self.item_phases
            .iter()
            .find(|link| link.item_id == item)
            .is_none_or(|link| {
                self.plan.as_ref().is_some_and(|plan| {
                    plan.usable() && plan.id == link.plan_id && plan.phases.contains(&link.phase_id)
                })
            })
    }

    fn apply_plan(&mut self, attachment: Option<PlanAttachment>) -> Result<()> {
        let Some(attachment) = attachment else {
            return Ok(());
        };
        attachment.validate()?;
        ensure!(
            attachment.usable(),
            "plan attachment requires a fresh native observation"
        );
        if self.plan.as_ref() == Some(&attachment) {
            return Ok(());
        }
        self.ensure_id();
        let changed = self.plan.as_ref().is_some_and(|old| {
            old.id != attachment.id
                || old.root != attachment.root
                || old.path != attachment.path
                || old.fingerprint != attachment.fingerprint
        });
        self.plan = Some(attachment);
        if changed {
            self.bump_epoch();
        }
        self.revise();
        Ok(())
    }

    pub(super) fn apply_phase_proposals(&mut self, phases: Vec<(String, String)>) -> Result<()> {
        let mut seen = HashSet::new();
        for (key, phase_id) in phases {
            let item_id = if let Some(index) = key.strip_prefix("@index:") {
                self.items
                    .get(index.parse::<usize>()?)
                    .map(|row| row.id.clone())
            } else {
                self.items
                    .iter()
                    .find(|row| row.id == key)
                    .map(|row| row.id.clone())
            }
            .ok_or_else(|| anyhow::anyhow!("unknown phase item reference"))?;
            ensure!(seen.insert(item_id.clone()), "duplicate phase proposal");
            let plan = self
                .plan
                .as_ref()
                .filter(|plan| plan.usable())
                .ok_or_else(|| anyhow::anyhow!("phase requires a freshly validated plan"))?;
            ensure!(
                plan.phases.contains(&phase_id),
                "phase does not exist in the attached plan"
            );
            let proposed = ItemPhase {
                item_id: item_id.clone(),
                plan_id: plan.id.clone(),
                phase_id,
            };
            if let Some(old) = self.item_phases.iter().find(|link| link.item_id == item_id) {
                ensure!(old == &proposed, "existing item phase cannot be silently relinked; preserve it or create a new row");
            } else {
                self.item_phases.push(proposed);
            }
        }
        self.resolve();
        self.validate()
    }
}

impl TaskContext {
    /// An authorized plan-only physical effect stales existing proof, without inventing a new
    /// implementation-check obligation. Only the native host proof path calls this method.
    pub(crate) fn invalidate_plan_write_effects(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ensure_id();
        state.bump_epoch();
        state.active = true;
        state.carry_over = true;
        state.revise();
        drop(state);
        self.publish();
    }

    /// Controller-selected readback attachment, not checklist replacement or completion evidence.
    pub(crate) fn attach_plan(&self, attachment: PlanAttachment) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        next.apply_plan(Some(attachment))?;
        next.resolve();
        next.validate()?;
        *state = next;
        drop(state);
        self.publish();
        Ok(())
    }

    pub(crate) fn replace_with_plan(
        &self,
        items: Vec<Todo>,
        requirements: Vec<(String, Requirement)>,
        attachment: Option<PlanAttachment>,
        phases: Vec<(String, String)>,
    ) -> Result<Vec<Todo>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state.active
                || state.id.is_empty()
                || items.is_empty()
                || *state == *self.saved.lock().unwrap_or_else(|e| e.into_inner()),
            "previous task snapshot is unsaved; save it or explicitly clear it first"
        );
        let mut base = state.clone();
        base.apply_plan(attachment)?;
        let mut next = base.replace(items, requirements)?;
        next.apply_phase_proposals(phases)?;
        let rows = next.items.clone();
        *state = next;
        drop(state);
        self.publish();
        Ok(rows)
    }

    /// Refresh the attached document once and return the resulting task snapshot together with
    /// metadata from that exact bounded read. The metadata is raw observation data; callers must
    /// encode it for their destination surface.
    pub(crate) fn refresh_plan_document(
        &self,
    ) -> Result<(TaskState, Option<plan_document::ReadPlan>)> {
        let snapshot = self.snapshot();
        let Some(old) = snapshot.plan.as_ref() else {
            return Ok((snapshot, None));
        };
        let (refreshed, document) = old.refreshed_document();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            *state == snapshot,
            "task changed during plan refresh; retry before proceeding"
        );
        if old == &refreshed {
            return Ok((snapshot, document));
        }
        let physical_change =
            old.fingerprint != refreshed.fingerprint || old.freshness != refreshed.freshness;
        state.plan = Some(refreshed);
        if physical_change {
            state.bump_epoch();
        }
        state.resolve();
        state.revise();
        state.validate()?;
        let refreshed_state = state.clone();
        drop(state);
        self.publish();
        Ok((refreshed_state, document))
    }

    /// Compatibility wrapper for callers that only need the refreshed task state.
    pub(crate) fn refresh_plan(&self) -> Result<()> {
        self.refresh_plan_document().map(|_| ())
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
