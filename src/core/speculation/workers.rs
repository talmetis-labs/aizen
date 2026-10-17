//! Historical physical-worker lifecycle evidence, never a supervisor or execution capability.
//!
//! An intent without closure is unresolved, including a crash between launch and publication.
//! These checks establish consistency only; only a live controller can witness ownership release.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::store::{InspectedRecord, SealedManifestIdentity};

pub(crate) const ROSTER_LABEL: &str = "speculation-worker-roster";
pub(crate) const INTENT_LABEL: &str = "speculation-worker-intent";
pub(crate) const CLOSED_LABEL: &str = "speculation-worker-closed";
pub(crate) const ADMISSION_LABEL: &str = "speculation-attempt-admission-closed";
pub(crate) const ATTEMPT_LABEL: &str = "speculation-attempt-closed";
const MAX_MEMBERS: usize = 32;
const MAX_WORKERS: usize = 128;
const MAX_METADATA_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptMember {
    pub(crate) attempt: u32,
    pub(crate) owner: String,
    pub(crate) overlay: String,
    pub(crate) scope_sha256: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerRoster {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) incarnation: String,
    #[serde(deserialize_with = "bounded_sequence")]
    pub(crate) members: Vec<AttemptMember>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeWorkerKind {
    Read,
    Edit,
    Write,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerIntent {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) roster: String,
    pub(crate) attempt: u32,
    pub(crate) worker: u32,
    #[serde(deserialize_with = "required_option")]
    pub(crate) parent_worker: Option<u32>,
    pub(crate) kind: NativeWorkerKind,
    /// Digest of the frozen prepared capability, not model-supplied arguments or a live grant.
    pub(crate) prepared_sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerResult {
    Returned,
    Failed,
    Panicked,
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerReceipt {
    pub(crate) intent: String,
    pub(crate) closed: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerClosed {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) roster: String,
    pub(crate) intent: String,
    pub(crate) result: WorkerResult,
    pub(crate) effect_state: super::verification::DurableEffectState,
    pub(crate) ownership_released: bool,
    #[serde(deserialize_with = "bounded_sequence")]
    pub(crate) children: Vec<WorkerReceipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DriverResult {
    Completed,
    Failed,
    Cancelled,
    Panicked,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptAdmissionClosed {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) roster: String,
    pub(crate) attempt: u32,
    pub(crate) driver_result: DriverResult,
    #[serde(deserialize_with = "bounded_sequence")]
    pub(crate) intents: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptClosed {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) roster: String,
    pub(crate) admission: String,
    pub(crate) ownership_released: bool,
    #[serde(deserialize_with = "bounded_sequence")]
    pub(crate) workers: Vec<WorkerReceipt>,
    pub(crate) sealed: SealedManifestIdentity,
}

/// Untrusted inspection results. Absence is legacy/missing evidence, not proof of no launch.
#[derive(Debug, Default)]
pub(crate) struct WorkerInspection {
    pub(crate) roster: Option<String>,
    parent: Option<String>,
    pub(crate) admitted_attempts: usize,
    pub(crate) launch_intents: usize,
    pub(crate) closed_workers: usize,
    pub(crate) closed_attempts: usize,
    pub(crate) successful_attempts: usize,
    pub(crate) sealed: Vec<(SealedManifestIdentity, [u8; 32])>,
}

impl WorkerInspection {
    pub(crate) fn unresolved(&self) -> bool {
        self.roster.is_some() && (self.closed_workers != self.launch_intents
            || self.closed_attempts != self.admitted_attempts
            || self.successful_attempts != self.admitted_attempts)
    }

    pub(crate) fn check_selected_lineage(&self, metadata: &serde_json::Value) -> Result<()> {
        if self.roster.is_none() { return Ok(()); }
        ensure!(!self.unresolved(), "integration has unresolved physical-worker evidence");
        ensure!(metadata.get("owner").and_then(serde_json::Value::as_str) == self.parent.as_deref(),
            "integration physical-worker parent differs");
        let lineage = super::store::sealed_lineage(metadata)?
            .context("worker-backed integration requires exact sealed lineage")?;
        ensure!(lineage.len() == self.admitted_attempts && lineage.len() == self.sealed.len(),
            "integration omits admitted physical attempts");
        for identity in &lineage {
            ensure!(self.sealed.iter().any(|(sealed, _)| sealed == identity),
                "integration selects another physical attempt or sealed generation");
        }
        Ok(())
    }
}

fn required_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where D: Deserializer<'de>, T: Deserialize<'de> {
    Option::<T>::deserialize(deserializer)
}

fn bounded_sequence<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where D: Deserializer<'de>, T: Deserialize<'de> {
    struct Bounded<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Bounded<T> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded worker evidence sequence")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == MAX_WORKERS {
                    return Err(serde::de::Error::custom("worker evidence sequence exceeds bound"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded(std::marker::PhantomData))
}

fn text(value: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
        "invalid worker evidence owner");
    Ok(())
}

fn opaque_id(value: &str) -> Result<()> {
    ensure!(value.len() == 32 && value.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')),
        "invalid worker evidence incarnation/base");
    Ok(())
}

fn record_id(value: &str) -> Result<()> {
    ensure!(super::recovery::immutable_record_id(value), "invalid immutable worker record identity");
    Ok(())
}

fn common(schema: u32, version: u32, base: &str, owner: &str) -> Result<()> {
    ensure!(schema == 1 && version == 1, "unsupported physical-worker evidence version");
    opaque_id(base)?;
    text(owner)
}

fn ordinal(value: u32, bound: usize) -> Result<()> {
    ensure!(value > 0 && value as usize <= bound, "worker evidence ordinal exceeds bound");
    Ok(())
}

fn receipts(values: &[WorkerReceipt]) -> Result<()> {
    ensure!(values.len() <= MAX_WORKERS, "worker receipt count exceeds bound");
    let mut intents = BTreeSet::new();
    let mut closed = BTreeSet::new();
    for value in values {
        record_id(&value.intent)?;
        record_id(&value.closed)?;
        ensure!(value.intent != value.closed && intents.insert(&value.intent) && closed.insert(&value.closed),
            "duplicate or self-linked worker receipt");
    }
    Ok(())
}

impl WorkerRoster {
    pub(crate) fn validate(&self) -> Result<()> {
        common(self.schema, self.lifecycle_version, &self.base, &self.owner)?;
        opaque_id(&self.incarnation)?;
        ensure!(!self.members.is_empty() && self.members.len() <= MAX_MEMBERS, "invalid worker roster size");
        let mut owners = BTreeSet::new();
        let mut overlays = BTreeSet::new();
        for (index, member) in self.members.iter().enumerate() {
            ensure!(member.attempt as usize == index + 1, "worker roster is not in stable attempt order");
            text(&member.owner)?;
            let overlay = member.overlay.strip_prefix("overlay-")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| (1..=32).contains(value))
                .context("worker roster overlay invalid")?;
            ensure!(member.overlay == format!("overlay-{overlay}"), "noncanonical worker roster overlay");
            ensure!(member.owner.starts_with(&format!("{}/", self.owner))
                && owners.insert(&member.owner) && overlays.insert(&member.overlay),
                "foreign or duplicate worker roster member");
        }
        Ok(())
    }
}

impl WorkerIntent {
    pub(crate) fn validate(&self) -> Result<()> {
        common(self.schema, self.lifecycle_version, &self.base, &self.owner)?;
        record_id(&self.roster)?;
        ordinal(self.attempt, MAX_MEMBERS)?;
        ordinal(self.worker, MAX_WORKERS)?;
        if let Some(parent) = self.parent_worker {
            ensure!(parent > 0 && parent < self.worker, "worker parent must precede descendant admission");
        }
        Ok(())
    }
}

impl WorkerClosed {
    pub(crate) fn validate(&self) -> Result<()> {
        use super::verification::DurableEffectState;
        common(self.schema, self.lifecycle_version, &self.base, &self.owner)?;
        record_id(&self.roster)?;
        record_id(&self.intent)?;
        ensure!(self.roster != self.intent && self.ownership_released
            && self.effect_state != DurableEffectState::UnknownWorker,
            "worker closure lacks physical ownership release");
        receipts(&self.children)?;
        if self.result == WorkerResult::NotStarted {
            ensure!(self.children.is_empty() && self.effect_state == DurableEffectState::Settled,
                "not-started worker has descendants or unsettled effects");
        }
        if self.result == WorkerResult::Panicked {
            ensure!(self.effect_state != DurableEffectState::Settled,
                "panicked worker cannot certify settled effects");
        }
        Ok(())
    }
}

impl AttemptAdmissionClosed {
    pub(crate) fn validate(&self) -> Result<()> {
        common(self.schema, self.lifecycle_version, &self.base, &self.owner)?;
        record_id(&self.roster)?;
        ordinal(self.attempt, MAX_MEMBERS)?;
        ensure!(self.intents.len() <= MAX_WORKERS, "attempt launch roster exceeds bound");
        let mut unique = BTreeSet::new();
        for intent in &self.intents {
            record_id(intent)?;
            ensure!(intent != &self.roster && unique.insert(intent), "duplicate attempt launch intent");
        }
        Ok(())
    }
}

impl AttemptClosed {
    pub(crate) fn validate(&self) -> Result<()> {
        common(self.schema, self.lifecycle_version, &self.base, &self.owner)?;
        record_id(&self.roster)?;
        record_id(&self.admission)?;
        ensure!(self.roster != self.admission && self.ownership_released,
            "attempt closure lacks ownership release");
        receipts(&self.workers)?;
        self.sealed.validate()?;
        ensure!(self.sealed.base == self.base, "attempt closure belongs to another sealed base");
        Ok(())
    }
}

/// Exact sorted readable bindings and writable bits; historical digest, never file authority.
pub(crate) fn scope_sha256<'a>(bindings: impl Iterator<Item = (&'a str, &'a std::path::Path, bool)>) -> Result<[u8; 32]> {
    use ring::digest::{Context as DigestContext, SHA256};
    let mut digest = DigestContext::new(&SHA256);
    digest.update(b"aizen-speculation-worker-scope-v1\0");
    let mut previous = None;
    let mut count = 0usize;
    for (key, path, writable) in bindings {
        ensure!(count < 32 && !key.is_empty() && key.len() <= 128
            && previous.is_none_or(|prior| prior < key), "worker scope keys invalid or unordered");
        let path = path.to_str().context("worker scope path is not UTF-8")?;
        ensure!(!path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control),
            "worker scope path invalid");
        for value in [key, path] {
            digest.update(&(value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
        digest.update(&[u8::from(writable)]);
        previous = Some(key);
        count += 1;
    }
    ensure!(count > 0, "worker scope is empty");
    digest.update(&(count as u64).to_le_bytes());
    let mut result = [0; 32];
    result.copy_from_slice(digest.finish().as_ref());
    Ok(result)
}

pub(crate) fn is_worker_label(label: &str) -> bool {
    label.starts_with("speculation-worker-") || label.starts_with("speculation-attempt-")
}

fn decode<T: serde::de::DeserializeOwned + Serialize>(record: &InspectedRecord) -> Result<T> {
    ensure!(record.metadata.len() <= MAX_METADATA_BYTES && record.images.is_empty(),
        "worker evidence envelope exceeds bound or carries images");
    record_id(&record.id)?;
    let value: T = serde_json::from_slice(&record.metadata).context("malformed worker lifecycle evidence")?;
    ensure!(serde_json::to_vec(&value)? == record.metadata, "worker evidence encoding is not canonical");
    Ok(value)
}

fn linked(base: &str, owner: &str, roster: &str, record: &InspectedRecord,
    roster_record: &InspectedRecord, expected: &WorkerRoster) -> Result<()> {
    ensure!(base == expected.base && owner == expected.owner && record.owner == expected.owner
        && roster == roster_record.id && record.id != roster,
        "worker lifecycle wrapper/roster binding mismatch");
    Ok(())
}

/// Validate the complete unordered inventory. Missing closures stay inspectable and unresolved;
/// orphan/duplicate/mismatched evidence is corrupt, never permission to retry or release ownership.
pub(crate) fn validate_worker_records(base: &str, records: &[InspectedRecord]) -> Result<WorkerInspection> {
    let relevant: Vec<_> = records.iter().filter(|r| is_worker_label(&r.label)).collect();
    if relevant.is_empty() { return Ok(WorkerInspection::default()); }
    opaque_id(base)?;
    ensure!(relevant.len() <= 1 + MAX_WORKERS * 2 + MAX_MEMBERS * 2,
        "physical-worker lifecycle record count exceeds bound");
    let mut ids = BTreeSet::new();
    let mut rosters = relevant.iter().filter(|r| r.label == ROSTER_LABEL);
    let roster_record = rosters.next().context("worker lifecycle roster missing")?;
    ensure!(rosters.next().is_none(), "ambiguous worker lifecycle incarnation/roster");
    let roster: WorkerRoster = decode(roster_record)?;
    roster.validate()?;
    ensure!(roster.base == base && roster.owner == roster_record.owner, "worker roster wrapper identity mismatch");
    let mut intents = BTreeMap::new();
    let mut slots = BTreeMap::new();
    let mut closures = BTreeMap::new();
    let mut admissions = BTreeMap::new();
    let mut attempts = BTreeMap::new();
    for record in &relevant {
        ensure!(ids.insert(record.id.as_str()), "duplicate worker lifecycle record");
        match record.label.as_str() {
            ROSTER_LABEL => {},
            INTENT_LABEL => {
                let intent: WorkerIntent = decode(record)?;
                intent.validate()?;
                linked(&intent.base, &intent.owner, &intent.roster, record, roster_record, &roster)?;
                ensure!(intent.attempt as usize <= roster.members.len()
                    && slots.insert((intent.attempt, intent.worker), record.id.as_str()).is_none(),
                    "foreign or duplicate physical worker ordinal");
                intents.insert(record.id.as_str(), intent);
            },
            CLOSED_LABEL => {
                let closed: WorkerClosed = decode(record)?;
                closed.validate()?;
                linked(&closed.base, &closed.owner, &closed.roster, record, roster_record, &roster)?;
                ensure!(closures.insert(closed.intent.clone(), (record.id.as_str(), closed)).is_none(),
                    "ambiguous physical worker closure");
            },
            ADMISSION_LABEL => {
                let admission: AttemptAdmissionClosed = decode(record)?;
                admission.validate()?;
                linked(&admission.base, &admission.owner, &admission.roster, record, roster_record, &roster)?;
                ensure!(admission.attempt as usize <= roster.members.len()
                    && admissions.insert(admission.attempt, (record.id.as_str(), admission)).is_none(),
                    "foreign or duplicate closed attempt admission");
            },
            ATTEMPT_LABEL => {
                let attempt: AttemptClosed = decode(record)?;
                attempt.validate()?;
                linked(&attempt.base, &attempt.owner, &attempt.roster, record, roster_record, &roster)?;
                ensure!(attempts.insert(attempt.admission.clone(), attempt).is_none(),
                    "ambiguous attempt closure");
            },
            _ => anyhow::bail!("unsupported physical-worker lifecycle label"),
        }
    }
    ensure!(intents.len() <= MAX_WORKERS, "cohort launch intent count exceeds bound");
    for member in &roster.members {
        let workers: Vec<_> = slots.range((member.attempt, 0)..=(member.attempt, u32::MAX))
            .map(|((_, worker), id)| (*worker, *id)).collect();
        for (index, (worker, id)) in workers.iter().enumerate() {
            ensure!(*worker as usize == index + 1, "physical worker launch roster has a gap");
            let intent = intents.get(*id).context("physical worker intent missing")?;
            if let Some(parent) = intent.parent_worker {
                ensure!(slots.contains_key(&(member.attempt, parent)), "physical worker parent missing");
            }
        }
        if let Some((_, admission)) = admissions.get(&member.attempt) {
            ensure!(admission.intents.iter().map(String::as_str).eq(workers.iter().map(|(_, id)| *id)),
                "closed attempt admission differs from exact launch roster");
        }
    }
    for (intent_id, (closed_id, closed)) in &closures {
        let intent = intents.get(intent_id.as_str()).context("physical worker closure intent missing")?;
        let children: Vec<_> = slots.iter().filter_map(|((attempt, worker), id)| {
            let child = &intents[id];
            (*attempt == intent.attempt && child.parent_worker == Some(intent.worker)).then_some((*worker, *id))
        }).collect();
        ensure!(children.len() == closed.children.len(), "physical worker closure omits or invents descendants");
        for ((_, child), receipt) in children.iter().zip(&closed.children) {
            let (actual, _) = closures.get(*child).context("physical descendant is not closed")?;
            ensure!(receipt.intent == *child && receipt.closed == *actual && actual != closed_id,
                "physical descendant closure differs from exact roster");
        }
    }
    let mut sealed = Vec::new();
    let mut successful_attempts = 0;
    for (admission_id, attempt) in &attempts {
        let (_, admission) = admissions.values().find(|(id, _)| *id == admission_id)
            .context("attempt closure admission missing")?;
        let member = &roster.members[admission.attempt as usize - 1];
        ensure!(attempt.sealed.owner == member.owner && attempt.sealed.overlay == member.overlay
            && attempt.workers.len() == admission.intents.len(), "attempt closure sealed/member/worker binding mismatch");
        let mut successful = admission.driver_result == DriverResult::Completed;
        for (intent, receipt) in admission.intents.iter().zip(&attempt.workers) {
            let (closed, evidence) = closures.get(intent).context("attempt still has an unclosed physical worker")?;
            ensure!(receipt.intent == *intent && receipt.closed == *closed,
                "attempt closure differs from exact physical-worker closures");
            successful &= evidence.result == WorkerResult::Returned
                && evidence.effect_state == super::verification::DurableEffectState::Settled;
        }
        successful_attempts += usize::from(successful);
        sealed.push((attempt.sealed.clone(), member.scope_sha256));
    }
    Ok(WorkerInspection {
        roster: Some(roster_record.id.clone()), parent: Some(roster.owner), admitted_attempts: roster.members.len(),
        launch_intents: intents.len(), closed_workers: closures.len(), closed_attempts: attempts.len(),
        successful_attempts, sealed,
    })
}

#[cfg(test)]
#[path = "workers_tests.rs"]
mod tests;
