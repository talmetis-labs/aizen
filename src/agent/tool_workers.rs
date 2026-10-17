//! Actual blocking-body joins, separate from disposable result observation.
//!
//! This proves only that registered tool bodies returned and released their closure captures.
//! It is not an attempt/resource-release receipt and cannot certify processes or durable effects.

use super::{ToolEffectState, ToolEffectsUnsettled, ToolOutcome};
use crate::core::speculation::lifecycle::{AttemptLifecycle, WorkerJoinFact};
use crate::core::speculation::verification::DurableEffectState;
use crate::core::speculation::workers::{NativeWorkerKind, WorkerResult};
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

const MAX_BODIES: usize = 128;

pub(crate) struct JoinedBody {
    pub(crate) completed: Instant,
    pub(crate) outcome: ToolOutcome,
}

struct BodyClosure {
    effect: ToolEffectState,
    fact: Option<WorkerJoinFact>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct WorkerJoinTestProbe(Arc<dyn Fn(usize) + Send + Sync>);

#[cfg(test)]
impl WorkerJoinTestProbe {
    pub(crate) fn new(probe: impl Fn(usize) + Send + Sync + 'static) -> Self {
        Self(Arc::new(probe))
    }
}

#[cfg(test)]
impl std::fmt::Debug for WorkerJoinTestProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerJoinTestProbe")
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Admission {
    closed: bool,
    handles: Vec<JoinHandle<BodyClosure>>,
    #[cfg(test)]
    join_probe: Option<WorkerJoinTestProbe>,
}

/// The context carries only a weak producer, never ownership of its own join supervisor.
#[derive(Clone, Debug)]
pub(crate) struct ToolWorkerProducer {
    admission: Weak<Mutex<Admission>>,
    lifecycle: Option<AttemptLifecycle>,
}

pub(crate) struct ToolWorkerOwner {
    admission: Arc<Mutex<Admission>>,
    pending: Vec<JoinHandle<BodyClosure>>,
    joined: usize,
    unsettled: bool,
    facts: Vec<WorkerJoinFact>,
    lifecycle: Option<AttemptLifecycle>,
}

/// Body quiescence only. Full attempt closure additionally requires explicit resource releases.
#[derive(Debug)]
#[must_use]
pub(crate) struct JoinedToolBodies {
    _workers: usize,
    pub(crate) facts: Vec<WorkerJoinFact>,
}

impl ToolWorkerOwner {
    pub(crate) fn new() -> Self {
        Self::new_with_optional_lifecycle(None)
    }

    pub(crate) fn new_with_lifecycle(lifecycle: AttemptLifecycle) -> Self {
        Self::new_with_optional_lifecycle(Some(lifecycle))
    }

    fn new_with_optional_lifecycle(lifecycle: Option<AttemptLifecycle>) -> Self {
        Self {
            admission: Arc::new(Mutex::new(Admission::default())),
            pending: Vec::new(),
            joined: 0,
            unsettled: false,
            facts: Vec::new(),
            lifecycle,
        }
    }

    pub(crate) fn producer(&self) -> ToolWorkerProducer {
        ToolWorkerProducer {
            admission: Arc::downgrade(&self.admission),
            lifecycle: self.lifecycle.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn observe_joins(&mut self, probe: WorkerJoinTestProbe) {
        let mut admission = self.admission.lock().unwrap();
        assert!(
            !admission.closed && admission.handles.is_empty(),
            "join probe must precede launch"
        );
        admission.join_probe = Some(probe);
    }

    /// Close launch admission without discarding any physical joins.
    pub(crate) fn close_admission(&mut self) {
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.closed = true;
    }

    /// Registration and admission closure share one mutex. A timeout retains every pending join;
    /// dropping the observer/owner only detaches supervisors, never aborts actual blocking bodies.
    pub(crate) async fn close_and_drain(&mut self, deadline: Instant) -> Result<JoinedToolBodies> {
        {
            let mut admission = self.admission.lock().map_err(|_| {
                anyhow::Error::new(ToolEffectsUnsettled(ToolEffectState::UnknownWorker))
                    .context("tool-body admission poisoned")
            })?;
            admission.closed = true;
            self.pending.append(&mut admission.handles);
        }
        while let Some(handle) = self.pending.last_mut() {
            let result = tokio::time::timeout_at(deadline, handle).await.map_err(|_| {
                anyhow::Error::new(ToolEffectsUnsettled(ToolEffectState::UnknownWorker))
                    .context("tool bodies remain physically live; joins retained, no retry or seal authorized")
            })?;
            self.pending.pop();
            self.joined += 1;
            match result {
                Ok(closed) => {
                    self.unsettled |= closed.effect != ToolEffectState::Settled;
                    if let Some(fact) = closed.fact {
                        self.facts.push(fact);
                    }
                }
                Err(_) => self.unsettled = true,
            }
        }
        anyhow::ensure!(
            !self.unsettled,
            ToolEffectsUnsettled(ToolEffectState::UnknownWorker)
        );
        Ok(JoinedToolBodies {
            _workers: self.joined,
            facts: std::mem::take(&mut self.facts),
        })
    }
}

impl Drop for ToolWorkerOwner {
    fn drop(&mut self) {
        // Prevent already-upgraded producers from launching after owner abandonment. Dropping
        // JoinHandles detaches their supervisors; it never aborts or fabricates closure proof.
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.closed = true;
    }
}

/// Bind native launch evidence to the actual frozen operation and the exact attempt controller.
pub(crate) fn launch_prepared(
    producer: Option<&ToolWorkerProducer>,
    prepared: &super::PreparedToolCall,
    body: impl FnOnce() -> ToolOutcome + Send + 'static,
    panic_effect: ToolEffectState,
) -> Result<oneshot::Receiver<JoinedBody>> {
    match crate::agent::builtin::prepared_native_worker(prepared)? {
        Some(worker) => {
            let lifecycle = producer
                .and_then(|producer| producer.lifecycle.clone())
                .context("native worker producer has no live lifecycle controller")?;
            worker.check_lifecycle(&lifecycle)?;
            launch_inner(
                producer,
                Some((lifecycle, worker.kind(), worker.digest())),
                body,
                panic_effect,
            )
        }
        None => launch(producer, body, panic_effect),
    }
}

pub(crate) fn launch(
    producer: Option<&ToolWorkerProducer>,
    body: impl FnOnce() -> ToolOutcome + Send + 'static,
    panic_effect: ToolEffectState,
) -> Result<oneshot::Receiver<JoinedBody>> {
    anyhow::ensure!(
        producer.is_none_or(|producer| producer.lifecycle.is_none()),
        "native attempt requires a typed frozen worker; nothing started"
    );
    launch_inner(producer, None, body, panic_effect)
}

struct NativeLaunch {
    ticket: crate::core::speculation::lifecycle::LaunchTicket,
    kind: NativeWorkerKind,
    prepared_sha256: [u8; 32],
}

fn launch_inner(
    producer: Option<&ToolWorkerProducer>,
    native: Option<(AttemptLifecycle, NativeWorkerKind, [u8; 32])>,
    body: impl FnOnce() -> ToolOutcome + Send + 'static,
    panic_effect: ToolEffectState,
) -> Result<oneshot::Receiver<JoinedBody>> {
    let runtime = tokio::runtime::Handle::try_current().context("tool body requires a runtime")?;
    let admission = producer
        .map(|producer| {
            producer
                .admission
                .upgrade()
                .context("tool-body owner is gone; nothing started")
        })
        .transpose()?;
    let mut gate = admission
        .as_ref()
        .map(|admission| {
            admission
                .lock()
                .map_err(|_| anyhow::anyhow!("tool-body admission poisoned; nothing started"))
        })
        .transpose()?;
    if let Some(gate) = gate.as_ref() {
        anyhow::ensure!(
            !gate.closed,
            "tool-body admission is closed; nothing started"
        );
        anyhow::ensure!(
            gate.handles.len() < MAX_BODIES,
            "tool-body inventory exceeds 128; nothing started"
        );
    }
    let native = native
        .map(|(lifecycle, kind, prepared_sha256)| {
            Ok::<_, anyhow::Error>(NativeLaunch {
                ticket: lifecycle.reserve_launch()?,
                kind,
                prepared_sha256,
            })
        })
        .transpose()?;
    #[cfg(test)]
    let join_probe = gate.as_ref().and_then(|gate| {
        gate.join_probe
            .clone()
            .map(|probe| (probe, gate.handles.len()))
    });
    let (send, receive) = oneshot::channel();
    let delivery = Arc::new(Mutex::new(Some(send)));
    let body_delivery = delivery.clone();
    let supervisor = runtime.spawn(async move {
        let mut fact = if let Some(native) = native {
            match tokio::task::spawn_blocking(move || {
                native.ticket.publish(native.kind, native.prepared_sha256)
            }).await {
                Ok(Ok(fact)) => Some(fact),
                result => {
                    // A failed publication may already be durable. Never retry it or release
                    // attempt ownership merely because the file body was not started.
                    let detail = match result {
                        Ok(Err(error)) => format!("{error:#}"),
                        Err(error) => error.to_string(),
                        Ok(Ok(_)) => unreachable!(),
                    };
                    drop(body);
                    deliver(&delivery, ToolOutcome::failed_with_effect_state(
                        format!("error: native worker intent publication uncertain; file body not started: {detail}"),
                        ToolEffectState::UnknownWorker,
                    ));
                    return BodyClosure { effect: ToolEffectState::UnknownWorker, fact: None };
                }
            }
        } else { None };
        let result = tokio::task::spawn_blocking(move || {
            let outcome = body();
            let effect = outcome.effect_state;
            let result = if outcome.failure.is_none() { WorkerResult::Returned } else { WorkerResult::Failed };
            // Completion delivery must not require an async forwarding hop, in either lane.
            deliver(&body_delivery, outcome);
            (effect, result)
        }).await;
        let (effect, result) = match result {
            Ok(closed) => {
                #[cfg(test)]
                if let Some((probe, index)) = join_probe { (probe.0)(index); }
                closed
            }
            Err(_) => {
                deliver(&delivery, ToolOutcome::failed_with_effect_state(
                    "error: tool body panicked; effects may be unsettled; inspect before retrying",
                    panic_effect,
                ));
                (panic_effect, WorkerResult::Panicked)
            }
        };
        if let Some(fact) = fact.as_mut() {
            fact.result = result;
            fact.effect = durable_effect(effect);
        }
        BodyClosure { effect, fact }
    });
    if let Some(gate) = gate.as_mut() {
        gate.handles.push(supervisor);
    } else {
        // Tokio retains a detached direct supervisor and its actual blocking join.
        drop(supervisor);
    }
    Ok(receive)
}

fn deliver(delivery: &Mutex<Option<oneshot::Sender<JoinedBody>>>, outcome: ToolOutcome) {
    let joined = JoinedBody {
        completed: Instant::now(),
        outcome,
    };
    if let Ok(mut send) = delivery.lock() {
        if let Some(send) = send.take() {
            let _ = send.send(joined);
        }
    }
}

fn durable_effect(effect: ToolEffectState) -> DurableEffectState {
    match effect {
        ToolEffectState::Settled => DurableEffectState::Settled,
        ToolEffectState::PossiblyApplied(record) => DurableEffectState::PossiblyApplied {
            record: record.and_then(std::num::NonZeroU64::new),
        },
        ToolEffectState::UnknownWorker => DurableEffectState::UnknownWorker,
    }
}

#[cfg(test)]
#[path = "tool_workers_tests.rs"]
mod tests;
