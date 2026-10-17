//! Live controller publication of worker evidence. Deserialized records cannot create this owner.

use super::store::{BaseSnapshot, ControllerReservation, SealedManifestIdentity};
use super::verification::DurableEffectState;
use super::workers::{self, AttemptMember, DriverResult, WorkerReceipt, WorkerResult};
use crate::core::native_scope::NativeWriteScope;
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::sync::{Arc, Mutex};

const MAX_WORKERS: usize = 128;
const MAX_METADATA_BYTES: usize = 64 * 1024;
// The bounded owner, label, image digest, and JSON envelope are additional to the payload.
const RECORD_ALLOWANCE: usize = MAX_METADATA_BYTES + 8 * 1024;

struct PublicationState {
    reservations: Vec<ControllerReservation>,
    tickets: usize,
    reserved: Vec<usize>,
    launched: Vec<Vec<String>>,
    closed: Vec<bool>,
    sealed: Vec<bool>,
    poisoned: bool,
}

pub(crate) struct CohortLifecycle {
    base: BaseSnapshot,
    owner: String,
    roster: String,
    members: Vec<AttemptMember>,
    state: Mutex<PublicationState>,
}

impl std::fmt::Debug for CohortLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CohortLifecycle")
            .field("roster", &self.roster)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AttemptLifecycle {
    cohort: Arc<CohortLifecycle>,
    attempt: u32,
    scope: NativeWriteScope,
}

/// A registration ticket, not a persisted launch or an executable context.
pub(crate) struct LaunchTicket {
    attempt: AttemptLifecycle,
}

#[derive(Debug)]
pub(crate) struct WorkerJoinFact {
    pub(crate) worker: u32,
    pub(crate) intent: String,
    pub(crate) result: WorkerResult,
    pub(crate) effect: DurableEffectState,
}

pub(crate) struct AttemptPublication {
    attempt: AttemptLifecycle,
    admission: String,
    workers: Vec<WorkerReceipt>,
    successful: bool,
}

fn publish<T: Serialize>(
    base: &BaseSnapshot,
    reservations: &mut [ControllerReservation],
    label: &str,
    value: &T,
) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_METADATA_BYTES,
        "worker evidence metadata exceeds bound"
    );
    // Each reservation chunk has at most sixteen records. Exhausted chunks are skipped without
    // probing a publication or retrying an uncertain write.
    let reservation = reservations
        .iter_mut()
        .find(|reservation| reservation.remaining_records() > 0)
        .context("worker terminal publication allowance exhausted")?;
    Ok(base
        .controller_record_reserved(reservation, label, &bytes, &[])?
        .as_str()
        .to_owned())
}

impl CohortLifecycle {
    /// Reserve the complete terminal envelope before publishing the roster or exposing a child.
    pub(crate) fn freeze(
        base: &BaseSnapshot,
        owner: &str,
        members: Vec<(AttemptMember, NativeWriteScope)>,
    ) -> Result<Arc<Self>> {
        let roster = workers::WorkerRoster {
            schema: 1,
            lifecycle_version: 1,
            base: base.id().to_owned(),
            owner: owner.to_owned(),
            incarnation: base.id().to_owned(),
            members: members.iter().map(|(member, _)| member.clone()).collect(),
        };
        roster.validate()?;
        for (member, scope) in &members {
            ensure!(
                member.scope_sha256
                    == workers::scope_sha256(scope.resources().iter().map(|resource| {
                        (
                            resource.key(),
                            resource.path(),
                            scope.write_keys().iter().any(|key| key == resource.key()),
                        )
                    }))?,
                "worker roster scope digest differs from supplied authority"
            );
        }
        let records = 1usize
            .checked_add(
                members
                    .len()
                    .checked_mul(2)
                    .context("worker allowance overflow")?,
            )
            .and_then(|count| count.checked_add(MAX_WORKERS * 2))
            .context("worker allowance overflow")?;
        let mut reservations = Vec::new();
        let mut remaining = records;
        while remaining > 0 {
            let count = remaining.min(16);
            reservations.push(
                base.reserve_controller_metadata(
                    owner,
                    count,
                    count
                        .checked_mul(RECORD_ALLOWANCE)
                        .context("worker allowance overflow")?,
                )?,
            );
            remaining -= count;
        }
        let id = publish(base, &mut reservations, workers::ROSTER_LABEL, &roster)?;
        let count = members.len();
        Ok(Arc::new(Self {
            base: base.clone(),
            owner: owner.to_owned(),
            roster: id,
            members: roster.members,
            state: Mutex::new(PublicationState {
                reservations,
                tickets: 0,
                reserved: vec![0; count],
                launched: vec![Vec::new(); count],
                closed: vec![false; count],
                sealed: vec![false; count],
                poisoned: false,
            }),
        }))
    }

    pub(crate) fn attempt(
        self: &Arc<Self>,
        ordinal: usize,
        scope: &NativeWriteScope,
    ) -> Result<AttemptLifecycle> {
        let member = self
            .members
            .get(ordinal)
            .context("worker roster member missing")?;
        ensure!(
            member.scope_sha256
                == workers::scope_sha256(scope.resources().iter().map(|resource| {
                    (
                        resource.key(),
                        resource.path(),
                        scope.write_keys().iter().any(|key| key == resource.key()),
                    )
                }))?,
            "worker roster scope differs from live controller"
        );
        Ok(AttemptLifecycle {
            cohort: self.clone(),
            attempt: member.attempt,
            scope: scope.clone(),
        })
    }

    pub(crate) fn check_member(&self, overlay: &super::store::OverlayHandle) -> Result<()> {
        let bound = overlay
            .lifecycle()
            .context("overlay has no live worker lifecycle")?;
        ensure!(
            std::ptr::eq(self, Arc::as_ptr(&bound.cohort)),
            "overlay belongs to another live cohort"
        );
        bound.check_overlay(overlay.base_id(), overlay.id(), overlay.owner())
    }

    pub(crate) fn check_complete(&self, selected: &[SealedManifestIdentity]) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("worker lifecycle poisoned"))?;
        ensure!(
            !state.poisoned
                && state.closed.iter().all(|closed| *closed)
                && state.sealed.iter().all(|sealed| *sealed),
            "physical attempt publication incomplete"
        );
        drop(state);
        let records = self.base.inspect_controller_records()?;
        let inspection = workers::validate_worker_records(self.base.id(), &records)?;
        ensure!(
            inspection.roster.as_deref() == Some(self.roster.as_str()) && !inspection.unresolved(),
            "expected live worker roster is absent or unresolved"
        );
        let overlays = selected
            .iter()
            .map(|sealed| {
                serde_json::json!({
                    "base": sealed.base, "owner": sealed.owner, "id": sealed.overlay,
                    "generation": sealed.generation, "manifest": sealed.manifest,
                    "author": format!("{}/{}", sealed.owner, sealed.overlay),
                })
            })
            .collect::<Vec<_>>();
        let metadata = serde_json::json!({"base": self.base.id(), "owner": self.owner,
            "overlay_lineage_version": 1, "overlays": overlays});
        inspection.check_selected_lineage(&metadata)
    }
}

impl AttemptLifecycle {
    pub(crate) fn same_attempt(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cohort, &other.cohort) && self.attempt == other.attempt
    }

    pub(crate) fn scope(&self) -> &NativeWriteScope {
        &self.scope
    }

    pub(crate) fn check_overlay(&self, base: &str, overlay: &str, owner: &str) -> Result<()> {
        let member = &self.cohort.members[self.attempt as usize - 1];
        ensure!(
            base == self.cohort.base.id() && overlay == member.overlay && owner == member.owner,
            "worker lifecycle belongs to another overlay/owner/base"
        );
        Ok(())
    }

    /// Registration does no IO. Publication happens on a supervised blocking thread before body spawn.
    pub(crate) fn reserve_launch(&self) -> Result<LaunchTicket> {
        let mut state = self
            .cohort
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("worker lifecycle poisoned"))?;
        ensure!(
            !state.poisoned && !state.closed[self.attempt as usize - 1],
            "worker lifecycle admission is closed"
        );
        ensure!(
            state.tickets < MAX_WORKERS,
            "cohort worker inventory exceeds 128; nothing started"
        );
        state.tickets += 1;
        state.reserved[self.attempt as usize - 1] += 1;
        Ok(LaunchTicket {
            attempt: self.clone(),
        })
    }

    /// Called only by the coordinating parent after consuming the exact volatile release token.
    pub(crate) fn publish_joined(
        &self,
        released: crate::agent::speculative_attempt::ReleasedAttempt,
        overlay: &super::store::OverlayHandle,
    ) -> Result<AttemptPublication> {
        let (mut facts, driver) = released.into_joined_for(self, overlay)?;
        facts.sort_by_key(|fact| fact.worker);
        let mut state = self
            .cohort
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("worker lifecycle poisoned"))?;
        let index = self.attempt as usize - 1;
        ensure!(
            !state.poisoned && !state.closed[index],
            "attempt publication is single-use"
        );
        // Set before IO. A failed write never permits replay or a reconstructed release token.
        state.closed[index] = true;
        let result = (|| {
            ensure!(
                facts.len() == state.reserved[index]
                    && facts.len() == state.launched[index].len()
                    && facts.iter().enumerate().all(|(index, fact)| {
                        fact.worker as usize == index + 1
                            && fact.intent == state.launched[self.attempt as usize - 1][index]
                    }),
                "joined bodies differ from exact launch inventory"
            );
            let admission = workers::AttemptAdmissionClosed {
                schema: 1,
                lifecycle_version: 1,
                base: self.cohort.base.id().to_owned(),
                owner: self.cohort.owner.clone(),
                roster: self.cohort.roster.clone(),
                attempt: self.attempt,
                driver_result: driver,
                intents: facts.iter().map(|fact| fact.intent.clone()).collect(),
            };
            admission.validate()?;
            let admission = publish(
                &self.cohort.base,
                &mut state.reservations,
                workers::ADMISSION_LABEL,
                &admission,
            )?;
            let mut receipts = Vec::new();
            let mut successful = driver == DriverResult::Completed;
            for fact in facts {
                let closed = workers::WorkerClosed {
                    schema: 1,
                    lifecycle_version: 1,
                    base: self.cohort.base.id().to_owned(),
                    owner: self.cohort.owner.clone(),
                    roster: self.cohort.roster.clone(),
                    intent: fact.intent.clone(),
                    result: fact.result,
                    effect_state: fact.effect,
                    ownership_released: true,
                    children: Vec::new(),
                };
                closed.validate()?;
                successful &= fact.result == WorkerResult::Returned
                    && fact.effect == DurableEffectState::Settled;
                let id = publish(
                    &self.cohort.base,
                    &mut state.reservations,
                    workers::CLOSED_LABEL,
                    &closed,
                )?;
                receipts.push(WorkerReceipt {
                    intent: fact.intent,
                    closed: id,
                });
            }
            Ok(AttemptPublication {
                attempt: self.clone(),
                admission,
                workers: receipts,
                successful,
            })
        })();
        if result.is_err() {
            state.poisoned = true;
        }
        result
    }

    pub(crate) fn publish_sealed(
        &self,
        publication: AttemptPublication,
        sealed: SealedManifestIdentity,
    ) -> Result<bool> {
        ensure!(
            self.same_attempt(&publication.attempt),
            "terminal publication belongs to another live attempt"
        );
        self.check_overlay(&sealed.base, &sealed.overlay, &sealed.owner)?;
        let attempt = workers::AttemptClosed {
            schema: 1,
            lifecycle_version: 1,
            base: self.cohort.base.id().to_owned(),
            owner: self.cohort.owner.clone(),
            roster: self.cohort.roster.clone(),
            admission: publication.admission,
            ownership_released: true,
            workers: publication.workers,
            sealed,
        };
        attempt.validate()?;
        let mut state = self
            .cohort
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("worker lifecycle poisoned"))?;
        let index = self.attempt as usize - 1;
        ensure!(
            !state.poisoned && state.closed[index] && !state.sealed[index],
            "worker lifecycle publication is uncertain or already sealed"
        );
        state.sealed[index] = true;
        let result = publish(
            &self.cohort.base,
            &mut state.reservations,
            workers::ATTEMPT_LABEL,
            &attempt,
        );
        if result.is_err() {
            state.poisoned = true;
        }
        result?;
        Ok(publication.successful)
    }
}

impl LaunchTicket {
    pub(crate) fn publish(
        self,
        kind: workers::NativeWorkerKind,
        prepared_sha256: [u8; 32],
    ) -> Result<WorkerJoinFact> {
        let lifecycle = &self.attempt.cohort;
        let mut state = lifecycle
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("worker lifecycle poisoned"))?;
        let index = self.attempt.attempt as usize - 1;
        ensure!(
            !state.poisoned && !state.closed[index],
            "attempt closed before launch publication"
        );
        let worker = u32::try_from(state.launched[index].len() + 1)?;
        let intent = workers::WorkerIntent {
            schema: 1,
            lifecycle_version: 1,
            base: lifecycle.base.id().to_owned(),
            owner: lifecycle.owner.clone(),
            roster: lifecycle.roster.clone(),
            attempt: self.attempt.attempt,
            worker,
            parent_worker: None,
            kind,
            prepared_sha256,
        };
        intent.validate()?;
        match publish(
            &lifecycle.base,
            &mut state.reservations,
            workers::INTENT_LABEL,
            &intent,
        ) {
            Ok(id) => {
                state.launched[index].push(id.clone());
                Ok(WorkerJoinFact {
                    worker,
                    intent: id,
                    result: WorkerResult::NotStarted,
                    effect: DurableEffectState::Settled,
                })
            }
            Err(error) => {
                state.poisoned = true;
                Err(error)
            }
        }
    }
}
