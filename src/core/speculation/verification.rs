//! Durable verifier lifecycle evidence. Records never recreate execution or recovery authority.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::store::InspectedRecord;

pub(crate) const START_LABEL: &str = "integration-verification-started";
pub(crate) const FINISH_LABEL: &str = "integration-verification";
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DurableEffectState {
    Settled,
    PossiblyApplied {
        record: Option<NonZeroU64>,
    },
    UnknownWorker,
}

impl<'de> Deserialize<'de> for DurableEffectState {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Empty struct variants reject extra fields; internally tagged unit variants ignore them.
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
        enum Evidence {
            Settled {},
            PossiblyApplied {
                #[serde(deserialize_with = "required_option")]
                record: Option<NonZeroU64>,
            },
            UnknownWorker {},
        }
        Ok(match Evidence::deserialize(deserializer)? {
            Evidence::Settled {} => Self::Settled,
            Evidence::PossiblyApplied { record } => Self::PossiblyApplied { record },
            Evidence::UnknownWorker {} => Self::UnknownWorker,
        })
    }
}

fn required_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationStarted {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) state: String,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) prepared: String,
    pub(crate) integration: String,
    #[serde(deserialize_with = "required_option")]
    pub(crate) source_identity: Option<[u8; 32]>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationFinished {
    pub(crate) schema: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) state: String,
    pub(crate) base: String,
    pub(crate) owner: String,
    pub(crate) prepared: String,
    pub(crate) integration: String,
    pub(crate) verification_started: String,
    #[serde(deserialize_with = "required_option")]
    pub(crate) source_identity: Option<[u8; 32]>,
    #[serde(deserialize_with = "required_option")]
    pub(crate) command: Option<String>,
    pub(crate) accepted: bool,
    pub(crate) effect_state: DurableEffectState,
    pub(crate) applied_paths: Vec<String>,
    pub(crate) read_dependencies: Vec<Value>,
    pub(crate) read_dependency_status: String,
    pub(crate) scope: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationSummary {
    pub(crate) summary_version: u32,
    pub(crate) lifecycle_version: u32,
    pub(crate) base: String,
    pub(crate) prepared: String,
    pub(crate) integration: String,
    pub(crate) verification_started: String,
    pub(crate) verification: String,
    pub(crate) source_identity: [u8; 32],
    pub(crate) state: String,
    pub(crate) command: String,
    pub(crate) effect_state: DurableEffectState,
    pub(crate) applied_paths: Vec<String>,
    pub(crate) read_dependencies: Vec<Value>,
    pub(crate) read_dependency_status: String,
    pub(crate) scope: String,
}

fn text(value: &str, bound: usize) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= bound && !value.chars().any(char::is_control),
        "invalid bounded verification text");
    Ok(())
}

fn base_id(value: &str) -> Result<()> {
    ensure!(value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid verification base identity");
    Ok(())
}

fn record_id(value: &str) -> Result<()> {
    ensure!(value.len() == 71 && value.starts_with("m-") && value.ends_with(".json")
        && value.as_bytes()[2..66].iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
        "invalid immutable verification record identity");
    Ok(())
}

fn distinct_ids(ids: &[&str]) -> Result<()> {
    let mut selected = BTreeSet::new();
    for id in ids {
        record_id(id)?;
        ensure!(selected.insert(*id), "verification record identities must be distinct");
    }
    Ok(())
}

fn details(paths: &[String], dependencies: &[Value], status: &str, scope: &str) -> Result<()> {
    ensure!(paths.len() <= super::store::MAX_COHORT_FILES
        && dependencies.len() <= 32 * super::store::MAX_COHORT_FILES,
        "verification evidence count exceeds bound");
    let mut unique = BTreeSet::new();
    for path in paths {
        text(path, 4096)?;
        ensure!(unique.insert(path), "duplicate verification applied path");
    }
    ensure!(status == if dependencies.is_empty() { "none" } else { "requires_semantic_review" },
        "verification read-dependency status differs from evidence");
    text(scope, 4096)?;
    // Peer evidence is copied verbatim and also cross-checked against prepared evidence by TM.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Dependency {
        observer: String,
        key: String,
        peer_authors: Vec<String>,
    }
    let mut selected = BTreeSet::new();
    for value in dependencies {
        let dependency: Dependency = serde_json::from_value(value.clone())?;
        text(&dependency.observer, 4096)?;
        text(&dependency.key, 128)?;
        ensure!(selected.insert((dependency.observer.clone(), dependency.key.clone()))
            && !dependency.peer_authors.is_empty() && dependency.peer_authors.len() <= 32,
            "invalid or duplicate verification read dependency");
        let mut peers = BTreeSet::new();
        for peer in &dependency.peer_authors {
            text(peer, 4096)?;
            ensure!(peer != &dependency.observer && peers.insert(peer),
                "invalid verification peer author");
        }
    }
    Ok(())
}

impl VerificationStarted {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1 && self.lifecycle_version == 1 && self.state == "started",
            "unsupported verification start lifecycle");
        base_id(&self.base)?;
        text(&self.owner, 1024)?;
        distinct_ids(&[&self.prepared, &self.integration])?;
        ensure!(serde_json::to_vec(self)?.len() <= MAX_METADATA_BYTES, "verification start exceeds bound");
        Ok(())
    }
}

impl VerificationFinished {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1 && self.lifecycle_version == 1, "unsupported verification finish lifecycle");
        base_id(&self.base)?;
        text(&self.owner, 1024)?;
        distinct_ids(&[&self.prepared, &self.integration, &self.verification_started])?;
        ensure!(matches!(self.state.as_str(), "verified" | "stale" | "verification_failed" | "unverified"),
            "unsupported verification finish state");
        if let Some(command) = &self.command { text(command, 4096)?; }
        ensure!(!self.accepted || self.state == "verified", "only verified evidence can be accepted");
        if self.state == "verified" || self.accepted {
            ensure!(self.accepted && self.source_identity.is_some() && self.command.is_some()
                && self.effect_state == DurableEffectState::Settled,
                "verified evidence requires source, command and settled physical effects");
        }
        details(&self.applied_paths, &self.read_dependencies, &self.read_dependency_status, &self.scope)?;
        ensure!(serde_json::to_vec(self)?.len() <= MAX_METADATA_BYTES, "verification finish exceeds bound");
        Ok(())
    }
}

impl VerificationSummary {
    pub(crate) fn validate(&self, expected_base: &str) -> Result<()> {
        base_id(expected_base)?;
        base_id(&self.base)?;
        ensure!(self.summary_version == 1 && self.lifecycle_version == 1 && self.base == expected_base,
            "unsupported or foreign verification summary");
        distinct_ids(&[&self.prepared, &self.integration, &self.verification_started, &self.verification])?;
        ensure!(self.state == "verified" && self.effect_state == DurableEffectState::Settled,
            "verification summary is not physically settled success");
        text(&self.command, 4096)?;
        details(&self.applied_paths, &self.read_dependencies, &self.read_dependency_status, &self.scope)?;
        ensure!(serde_json::to_vec(self)?.len() <= MAX_SUMMARY_BYTES, "verification summary exceeds bound");
        Ok(())
    }
}

pub(crate) fn parse_success_summary(raw: &str, expected_base: &str) -> Result<VerificationSummary> {
    ensure!(raw.len() <= MAX_SUMMARY_BYTES, "verification summary exceeds bound");
    let summary: VerificationSummary = serde_json::from_str(raw).context("invalid verification summary")?;
    summary.validate(expected_base)?;
    Ok(summary)
}

// Legacy evidence remains readable, but cannot supply lifecycle completion or accepted success.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFinished {
    schema: u32,
    state: String,
    base: String,
    owner: String,
    prepared: String,
    integration: String,
    #[serde(deserialize_with = "required_option")]
    source_identity: Option<[u8; 32]>,
    #[serde(deserialize_with = "required_option")]
    command: Option<String>,
    accepted: bool,
    applied_paths: Vec<String>,
    read_dependencies: Vec<Value>,
    read_dependency_status: String,
    scope: String,
}

fn linked_receipt<'a>(
    base: &str, owner: &str, prepared: &str, integration: &str,
    records: &'a BTreeMap<&str, &InspectedRecord>,
) -> Result<&'a InspectedRecord> {
    let prepared_record = records.get(prepared).context("verification prepared record missing")?;
    let receipt = records.get(integration).context("verification integration receipt missing")?;
    ensure!(prepared_record.label == "integration-prepared" && receipt.label == "integration-receipt"
        && prepared_record.owner == owner && receipt.owner == owner && receipt.images.is_empty(),
        "verification prepared/receipt wrapper linkage mismatch");
    for record in [*prepared_record, *receipt] {
        let value: Value = serde_json::from_slice(&record.metadata)?;
        ensure!(value.get("schema").and_then(Value::as_u64) == Some(1)
            && value.get("base").and_then(Value::as_str) == Some(base)
            && value.get("owner").and_then(Value::as_str) == Some(owner),
            "verification prepared/receipt metadata linkage mismatch");
        if record.label == "integration-receipt" {
            ensure!(value.get("prepared").and_then(Value::as_str) == Some(prepared),
                "verification receipt refers to another prepared record");
        }
    }
    Ok(receipt)
}

/// Order-independent validation of lifecycle evidence only; start-only and nonsettled finishes
/// are inspectable, never a successful verification or permission to release a pending guard.
pub(crate) fn validate_verification_records(base: &str, records: &[InspectedRecord]) -> Result<()> {
    let mut indexed = BTreeMap::new();
    for record in records {
        ensure!(indexed.insert(record.id.as_str(), record).is_none(), "duplicate immutable controller record");
    }
    let mut starts = BTreeMap::new();
    let mut finishes = BTreeSet::new();
    for record in records.iter().filter(|record| record.label == START_LABEL) {
        ensure!(record.metadata.len() <= MAX_METADATA_BYTES && record.images.is_empty(), "invalid verification start envelope");
        let start: VerificationStarted = serde_json::from_slice(&record.metadata)?;
        start.validate()?;
        record_id(&record.id)?;
        ensure!(start.base == base && start.owner == record.owner
            && record.id != start.prepared && record.id != start.integration,
            "verification start wrapper identity mismatch");
        linked_receipt(base, &start.owner, &start.prepared, &start.integration, &indexed)?;
        ensure!(starts.insert(start.prepared.clone(), (record, start)).is_none(),
            "ambiguous verification starts for prepared record");
    }
    for record in records.iter().filter(|record| record.label == FINISH_LABEL) {
        ensure!(record.metadata.len() <= MAX_METADATA_BYTES && record.images.is_empty(), "invalid verification finish envelope");
        record_id(&record.id)?;
        let value: Value = serde_json::from_slice(&record.metadata)?;
        if value.get("lifecycle_version").is_none() {
            let legacy: LegacyFinished = serde_json::from_slice(&record.metadata)?;
            ensure!(legacy.schema == 1 && legacy.base == base && legacy.owner == record.owner
                && matches!(legacy.state.as_str(), "verified" | "stale" | "verification_failed" | "unverified"),
                "invalid legacy verification evidence");
            base_id(&legacy.base)?;
            text(&legacy.owner, 1024)?;
            distinct_ids(&[&legacy.prepared, &legacy.integration, &record.id])?;
            if let Some(command) = &legacy.command { text(command, 4096)?; }
            if legacy.accepted || legacy.state == "verified" {
                ensure!(legacy.accepted && legacy.state == "verified" && legacy.source_identity.is_some()
                    && legacy.command.is_some(), "invalid legacy verification success claim");
            }
            details(&legacy.applied_paths, &legacy.read_dependencies, &legacy.read_dependency_status, &legacy.scope)?;
            linked_receipt(base, &legacy.owner, &legacy.prepared, &legacy.integration, &indexed)?;
            ensure!(!starts.contains_key(&legacy.prepared) && finishes.insert(legacy.prepared),
                "ambiguous or downgraded legacy verification evidence");
            continue;
        }
        let finish: VerificationFinished = serde_json::from_slice(&record.metadata)?;
        finish.validate()?;
        ensure!(finish.base == base && finish.owner == record.owner
            && finishes.insert(finish.prepared.clone()), "ambiguous verification finish identity");
        distinct_ids(&[&finish.prepared, &finish.integration, &finish.verification_started, &record.id])?;
        linked_receipt(base, &finish.owner, &finish.prepared, &finish.integration, &indexed)?;
        let (start_record, start) = starts.get(&finish.prepared).context("verification finish lacks exact start")?;
        ensure!(start_record.id == finish.verification_started && start.base == finish.base
            && start.owner == finish.owner && start.prepared == finish.prepared
            && start.integration == finish.integration && start.source_identity == finish.source_identity,
            "verification finish differs from its exact start");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn success_summary_fixture(base: &str) -> String {
    let id = |digit: char| format!("m-{}.json", digit.to_string().repeat(64));
    let summary = VerificationSummary {
        summary_version: 1, lifecycle_version: 1, base: base.into(), prepared: id('1'),
        integration: id('2'), verification_started: id('3'), verification: id('4'),
        source_identity: [1; 32], state: "verified".into(), command: "test fixture only".into(),
        effect_state: DurableEffectState::Settled, applied_paths: Vec::new(), read_dependencies: Vec::new(),
        read_dependency_status: "none".into(), scope: "fixture evidence only".into(),
    };
    summary.validate(base).unwrap();
    serde_json::to_string(&summary).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "0123456789abcdef0123456789abcdef";

    fn summary() -> Value {
        serde_json::from_str(&success_summary_fixture(BASE)).unwrap()
    }

    fn started() -> Value {
        let summary = summary();
        serde_json::json!({
            "schema":1,"lifecycle_version":1,"state":"started","base":BASE,"owner":"parent",
            "prepared":summary["prepared"],"integration":summary["integration"],"source_identity":null
        })
    }

    fn finished() -> Value {
        let mut value = summary();
        let object = value.as_object_mut().unwrap();
        object.remove("summary_version");
        object.remove("verification");
        object.insert("schema".into(), Value::from(1));
        object.insert("owner".into(), Value::from("parent"));
        object.insert("accepted".into(), Value::Bool(true));
        value
    }

    fn valid_start(value: Value) -> bool {
        serde_json::from_value::<VerificationStarted>(value).ok()
            .is_some_and(|value| value.validate().is_ok())
    }

    fn valid_finish(value: Value) -> bool {
        serde_json::from_value::<VerificationFinished>(value).ok()
            .is_some_and(|value| value.validate().is_ok())
    }

    fn valid_summary(value: Value) -> bool {
        parse_success_summary(&serde_json::to_string(&value).unwrap(), BASE).is_ok()
    }

    #[test]
    fn effect_states_are_strict_and_never_default_missing_evidence() {
        for value in [serde_json::json!({"state":"settled"}), serde_json::json!({"state":"unknown_worker"}),
            serde_json::json!({"state":"possibly_applied","record":null}),
            serde_json::json!({"state":"possibly_applied","record":1})] {
            let effect: DurableEffectState = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(effect).unwrap(), value);
        }
        for value in [serde_json::json!({}), serde_json::json!({"state":"future"}),
            serde_json::json!({"state":"settled","record":1}),
            serde_json::json!({"state":"unknown_worker","extra":true}),
            serde_json::json!({"state":"possibly_applied"}),
            serde_json::json!({"state":"possibly_applied","record":0}),
            serde_json::json!({"state":"possibly_applied","record":-1}),
            serde_json::json!({"state":"possibly_applied","record":1,"extra":true})] {
            assert!(serde_json::from_value::<DurableEffectState>(value.clone()).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn nullable_lifecycle_fields_are_required_and_unknown_fields_rejected() {
        assert!(valid_start(started()));
        assert!(valid_finish(finished()));
        for field in ["source_identity", "schema", "lifecycle_version", "state", "owner", "prepared", "integration"] {
            let mut value = started();
            value.as_object_mut().unwrap().remove(field);
            assert!(!valid_start(value), "start missing {field}");
        }
        for field in ["source_identity", "command", "effect_state", "verification_started", "accepted", "scope"] {
            let mut value = finished();
            value.as_object_mut().unwrap().remove(field);
            assert!(!valid_finish(value), "finish missing {field}");
        }
        let mut start = started();
        start["unknown"] = Value::Bool(true);
        assert!(!valid_start(start));
        let mut finish = finished();
        finish["unknown"] = Value::Bool(true);
        assert!(!valid_finish(finish));
        let mut finish = finished();
        finish["state"] = Value::from("unverified");
        finish["accepted"] = Value::Bool(false);
        finish["source_identity"] = Value::Null;
        finish["command"] = Value::Null;
        finish["effect_state"] = serde_json::json!({"state":"unknown_worker"});
        assert!(valid_finish(finish), "explicit nonterminal uncertainty remains inspectable");
    }

    #[test]
    fn lifecycle_versions_identities_bounds_and_success_effects_are_validated() {
        for field in ["schema", "lifecycle_version"] {
            let mut value = started(); value[field] = Value::from(2);
            assert!(!valid_start(value));
            let mut value = finished(); value[field] = Value::from(2);
            assert!(!valid_finish(value));
        }
        for owner in [String::new(), "x".repeat(1025), "parent\nchild".into()] {
            let mut value = started(); value["owner"] = Value::from(owner);
            assert!(!valid_start(value));
        }
        let mut value = started(); value["owner"] = Value::from("x".repeat(1024));
        assert!(valid_start(value));
        for field in ["prepared", "integration", "verification_started"] {
            let mut value = finished(); value[field] = Value::from("not-immutable");
            assert!(!valid_finish(value));
        }
        let mut value = finished(); value["integration"] = value["prepared"].clone();
        assert!(!valid_finish(value));
        for effect in [serde_json::json!({"state":"unknown_worker"}),
            serde_json::json!({"state":"possibly_applied","record":7})] {
            let mut value = finished(); value["effect_state"] = effect;
            assert!(!valid_finish(value));
        }
        for field in ["source_identity", "command"] {
            let mut value = finished(); value[field] = Value::Null;
            assert!(!valid_finish(value));
        }
        for command in [String::new(), "x".repeat(4097), "test\nsecret".into()] {
            let mut value = finished(); value["command"] = Value::from(command);
            assert!(!valid_finish(value));
        }
        let mut value = finished(); value["state"] = Value::from("abandoned");
        assert!(!valid_finish(value));
    }

    #[test]
    fn success_summary_is_exact_bounded_and_never_blesses_legacy_or_unknown_effects() {
        let good = success_summary_fixture(BASE);
        parse_success_summary(&good, BASE).unwrap();
        assert!(parse_success_summary(&good, "ffffffffffffffffffffffffffffffff").is_err());
        assert!(parse_success_summary(&good, "base").is_err());
        assert!(parse_success_summary(&" ".repeat(MAX_SUMMARY_BYTES + 1), BASE).is_err());
        for field in summary().as_object().unwrap().keys() {
            let mut value = summary(); value.as_object_mut().unwrap().remove(field);
            assert!(!valid_summary(value), "summary missing {field}");
        }
        for field in ["summary_version", "lifecycle_version"] {
            let mut value = summary(); value[field] = Value::from(2);
            assert!(!valid_summary(value));
        }
        for effect in [serde_json::json!({"state":"unknown_worker"}),
            serde_json::json!({"state":"possibly_applied","record":null})] {
            let mut value = summary(); value["effect_state"] = effect;
            assert!(!valid_summary(value));
        }
        let mut value = summary(); value["verification"] = value["verification_started"].clone();
        assert!(!valid_summary(value));
        let mut value = summary(); value["state"] = Value::from("unverified");
        assert!(!valid_summary(value));
        let mut value = summary(); value["extra"] = Value::Bool(true);
        assert!(!valid_summary(value));
        let mut value = summary(); value["applied_paths"] = serde_json::json!(["same", "same"]);
        assert!(!valid_summary(value));
        let mut value = summary(); value["applied_paths"] = serde_json::json!(vec!["x"; super::super::store::MAX_COHORT_FILES + 1]);
        assert!(!valid_summary(value));
        let mut value = summary(); value["read_dependencies"] = serde_json::json!([{}]);
        assert!(!valid_summary(value));
    }
}
