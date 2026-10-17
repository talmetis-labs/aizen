//! Typed historical inspection data, never source-edit, replay, settlement or undo authority.

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

use crate::core::persist::FileFingerprint;

pub(crate) const OBSERVATION_LABEL: &str = "recovery-inspection-observation";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectionObservation {
    pub(crate) schema: u32,
    pub(crate) base: String,
    pub(crate) prepared: String,
    /// Exact receipt observed then, or none. Later receipts cannot rewrite an earlier observation.
    pub(crate) receipt: Option<String>,
    /// Historical integration owner, not the fresh observer's execution identity.
    pub(crate) owner: String,
    pub(crate) observer: String,
    pub(crate) state: ObservationState,
    /// Revision before this record; later appends do not invalidate historical observations.
    pub(crate) inventory_sha256: [u8; 32],
    pub(crate) files: Vec<ObservedFile>,
    pub(crate) native_records: Vec<ObservedNativeRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationState {
    ObservedNonterminal,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservedFile {
    pub(crate) key: String,
    pub(crate) class: ObservedImageClass,
    pub(crate) fingerprint: ObservedFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedImageClass {
    Pre,
    Post,
    SamePreAndPost,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservedFingerprint {
    exists: bool,
    byte_len: u64,
    sha256: [u8; 32],
}

impl From<&FileFingerprint> for ObservedFingerprint {
    fn from(value: &FileFingerprint) -> Self {
        Self { exists: value.exists, byte_len: value.byte_len, sha256: value.sha256 }
    }
}

impl ObservedFingerprint {
    pub(crate) fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint { exists: self.exists, byte_len: self.byte_len, sha256: self.sha256 }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservedNativeRecord {
    pub(crate) id: u64,
    pub(crate) status: ObservedNativeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedNativeStatus {
    Prepared,
    Applied,
    SettledApplied,
    SettledNotApplied,
}

impl From<crate::core::native_journal::RecordProofStatus> for ObservedNativeStatus {
    fn from(value: crate::core::native_journal::RecordProofStatus) -> Self {
        use crate::core::native_journal::RecordProofStatus;
        match value {
            RecordProofStatus::Prepared => Self::Prepared,
            RecordProofStatus::Applied => Self::Applied,
            RecordProofStatus::SettledApplied => Self::SettledApplied,
            RecordProofStatus::SettledNotApplied => Self::SettledNotApplied,
        }
    }
}

impl InspectionObservation {
    pub(crate) fn validate(&self) -> Result<()> {
        use super::store::{MAX_COHORT_FILES, MAX_FILE_BYTES};
        ensure!(self.schema == 1 && self.base.len() == 32
            && self.base.bytes().all(|byte| byte.is_ascii_hexdigit())
            && immutable_record_id(&self.prepared)
            && self.receipt.as_deref().is_none_or(immutable_record_id), "unsupported inspection observation identity");
        ensure!(self.receipt.is_some() || self.native_records.is_empty(), "inspection native proof lacks observed receipt");
        for owner in [&self.owner, &self.observer] {
            ensure!(!owner.is_empty() && owner.len() <= 1024 && !owner.chars().any(char::is_control),
                "inspection observation owner invalid");
        }
        ensure!(self.files.len() <= MAX_COHORT_FILES && self.native_records.len() <= 128,
            "inspection observation exceeds record bounds");
        let mut keys = std::collections::BTreeSet::new();
        for file in &self.files {
            ensure!(!file.key.is_empty() && file.key.len() <= 128 && !file.key.chars().any(char::is_control)
                && keys.insert(file.key.as_str()), "inspection observation file key invalid or duplicated");
            ensure!(file.fingerprint.byte_len <= MAX_FILE_BYTES as u64
                && (file.fingerprint.exists || (file.fingerprint.byte_len == 0 && file.fingerprint.sha256 == [0; 32])),
                "inspection observation fingerprint invalid");
        }
        let mut previous = 0;
        for record in &self.native_records {
            ensure!(record.id > previous, "inspection observation native IDs invalid or unordered");
            previous = record.id;
        }
        Ok(())
    }
}

pub(crate) fn immutable_record_id(id: &str) -> bool {
    id.strip_prefix("m-").and_then(|id| id.strip_suffix(".json"))
        .is_some_and(|hash| hash.len() == 64 && hash.bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn record_id(digit: char) -> String {
        format!("m-{}.json", digit.to_string().repeat(64))
    }

    fn fixture() -> Value {
        json!({
            "schema": 1,
            "base": "a".repeat(32),
            "prepared": record_id('b'),
            "receipt": record_id('c'),
            "owner": "parent/integration",
            "observer": "parent/inspection",
            "state": "observed_nonterminal",
            "inventory_sha256": vec![7u8; 32],
            "files": [{
                "key": "selected-key",
                "class": "post",
                "fingerprint": {
                    "exists": true, "byte_len": 5, "sha256": vec![8u8; 32]
                }
            }],
            "native_records": [{"id": 2, "status": "settled_applied"}]
        })
    }

    fn parse_validated(value: Value) -> Result<InspectionObservation> {
        let observation: InspectionObservation = serde_json::from_value(value)?;
        observation.validate()?;
        Ok(observation)
    }

    #[test]
    fn observation_round_trips_all_nonterminal_classes_and_historical_statuses() {
        for class in ["pre", "post", "same_pre_and_post"] {
            for status in ["prepared", "applied", "settled_applied", "settled_not_applied"] {
                let mut value = fixture();
                value["files"][0]["class"] = json!(class);
                value["native_records"][0]["status"] = json!(status);
                let observation = parse_validated(value.clone()).unwrap();
                assert_eq!(serde_json::to_value(&observation).unwrap(), value);
                assert_eq!(observation.files[0].fingerprint.fingerprint(), FileFingerprint {
                    exists: true, byte_len: 5, sha256: [8; 32],
                });
            }
        }
    }

    #[test]
    fn observation_rejects_unknown_fields_at_every_object_level() {
        for path in ["", "/files/0", "/files/0/fingerprint", "/native_records/0"] {
            let mut value = fixture();
            value.pointer_mut(path).unwrap().as_object_mut().unwrap()
                .insert("unexpected_authority".into(), json!(true));
            assert!(serde_json::from_value::<InspectionObservation>(value).is_err(), "{path}");
        }
    }

    #[test]
    fn observation_rejects_future_schema_terminal_states_and_foreign_class() {
        for schema in [0u32, 2, u32::MAX] {
            let mut value = fixture();
            value["schema"] = json!(schema);
            assert!(parse_validated(value).is_err());
        }
        for state in ["settled", "verified", "applied", "rolled_back", "observed_terminal"] {
            let mut value = fixture();
            value["state"] = json!(state);
            assert!(serde_json::from_value::<InspectionObservation>(value).is_err());
        }
        let mut value = fixture();
        value["files"][0]["class"] = json!("foreign");
        assert!(parse_validated(value).is_err());
        let mut value = fixture();
        value["native_records"][0]["status"] = json!("verified");
        assert!(parse_validated(value).is_err());
    }

    #[test]
    fn observation_owner_and_observer_are_byte_bounded_and_control_free() {
        for field in ["owner", "observer"] {
            let mut boundary = fixture();
            boundary[field] = json!("x".repeat(1024));
            assert!(parse_validated(boundary).is_ok());
            for owner in [String::new(), "x".repeat(1025), "é".repeat(513),
                "parent\0child".into(), "parent\nchild".into(), "parent\u{7f}child".into()] {
                let mut value = fixture();
                value[field] = json!(owner);
                assert!(parse_validated(value).is_err(), "{field}");
            }
        }
    }

    #[test]
    fn observation_file_count_accepts_boundary_and_rejects_overflow() {
        use super::super::store::MAX_COHORT_FILES;
        for count in [0, MAX_COHORT_FILES, MAX_COHORT_FILES + 1] {
            let mut value = fixture();
            let template = value["files"][0].clone();
            value["files"] = Value::Array((0..count).map(|index| {
                let mut file = template.clone();
                file["key"] = json!(format!("key-{index}"));
                file
            }).collect());
            assert_eq!(parse_validated(value).is_ok(), count <= MAX_COHORT_FILES);
        }
    }

    #[test]
    fn observation_native_proof_count_accepts_boundary_and_rejects_overflow() {
        for count in [0usize, 128, 129] {
            let mut value = fixture();
            value["native_records"] = Value::Array((1..=count).map(|id| {
                json!({"id": id, "status": "prepared"})
            }).collect());
            assert_eq!(parse_validated(value).is_ok(), count <= 128);
        }
    }

    #[test]
    fn observation_file_keys_are_bounded_nonempty_control_free_and_unique() {
        let mut boundary = fixture();
        boundary["files"][0]["key"] = json!("k".repeat(128));
        assert!(parse_validated(boundary).is_ok());
        for key in [String::new(), "k".repeat(129), "é".repeat(65), "key\nother".into(), "key\0other".into()] {
            let mut value = fixture();
            value["files"][0]["key"] = json!(key);
            assert!(parse_validated(value).is_err());
        }
        let mut duplicated = fixture();
        let file = duplicated["files"][0].clone();
        duplicated["files"].as_array_mut().unwrap().push(file);
        assert!(parse_validated(duplicated).is_err());
    }

    #[test]
    fn observation_fingerprints_require_complete_fields_and_canonical_missing_image() {
        for field in ["exists", "byte_len", "sha256"] {
            let mut value = fixture();
            value["files"][0]["fingerprint"].as_object_mut().unwrap().remove(field);
            assert!(parse_validated(value).is_err());
        }
        let mut value = fixture();
        value["files"][0].as_object_mut().unwrap().remove("fingerprint");
        assert!(parse_validated(value).is_err());
        let missing = FileFingerprint::missing();
        let mut canonical = fixture();
        canonical["files"][0]["fingerprint"] = serde_json::to_value(ObservedFingerprint::from(&missing)).unwrap();
        let observation = parse_validated(canonical.clone()).unwrap();
        assert_eq!(observation.files[0].fingerprint.fingerprint(), missing);
        for field in ["byte_len", "sha256"] {
            let mut invalid = canonical.clone();
            invalid["files"][0]["fingerprint"][field] = if field == "byte_len" {
                json!(1)
            } else {
                json!(vec![1u8; 32])
            };
            assert!(parse_validated(invalid).is_err());
        }
        for length in [31, 33] {
            let mut invalid = fixture();
            invalid["files"][0]["fingerprint"]["sha256"] = json!(vec![0u8; length]);
            assert!(parse_validated(invalid).is_err());
        }
        let mut boundary = fixture();
        boundary["files"][0]["fingerprint"]["byte_len"] = json!(super::super::store::MAX_FILE_BYTES);
        assert!(parse_validated(boundary.clone()).is_ok());
        boundary["files"][0]["fingerprint"]["byte_len"] = json!(super::super::store::MAX_FILE_BYTES + 1);
        assert!(parse_validated(boundary).is_err());
    }

    #[test]
    fn observation_native_ids_must_be_nonzero_unique_and_strictly_ordered() {
        for (ids, valid) in [
            (vec![0u64], false), (vec![2, 2], false), (vec![3, 2], false),
            (vec![1, 3, u64::MAX], true), (vec![u64::MAX], true),
        ] {
            let mut value = fixture();
            value["native_records"] = Value::Array(ids.into_iter().map(|id| {
                json!({"id": id, "status": "prepared"})
            }).collect());
            assert_eq!(parse_validated(value).is_ok(), valid);
        }
    }

    #[test]
    fn observation_native_proofs_require_receipt_but_empty_proofs_do_not() {
        let mut value = fixture();
        value["receipt"] = Value::Null;
        assert!(parse_validated(value.clone()).is_err());
        value["native_records"] = json!([]);
        assert!(parse_validated(value.clone()).is_ok());
        value.as_object_mut().unwrap().remove("receipt");
        assert!(parse_validated(value).is_ok());
    }

    #[test]
    fn observation_identity_requires_exact_base_and_immutable_record_ids() {
        for base in [String::new(), "a".repeat(31), "a".repeat(33), "g".repeat(32), "../outside".into()] {
            let mut value = fixture();
            value["base"] = json!(base);
            assert!(parse_validated(value).is_err());
        }
        for field in ["prepared", "receipt"] {
            for id in [String::new(), format!("m-{}.json", "a".repeat(63)),
                format!("m-{}.json", "a".repeat(65)), record_id('A'), record_id('g'),
                format!("../{}", record_id('a')), format!("{}.extra", record_id('a')),
                format!("{}.json", "a".repeat(64))] {
                assert!(!immutable_record_id(&id));
                let mut value = fixture();
                value[field] = json!(id);
                assert!(parse_validated(value).is_err(), "{field}");
            }
        }
        assert!(immutable_record_id(&record_id('0')));
        assert!(immutable_record_id(&record_id('f')));
        let mut missing_prepared = fixture();
        missing_prepared.as_object_mut().unwrap().remove("prepared");
        assert!(parse_validated(missing_prepared).is_err());
    }
}
