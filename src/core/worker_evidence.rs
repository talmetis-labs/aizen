//! Shared worker facts and strict historical encodings, never live execution or release authority.
//! Legacy storage paths re-export these same nominal types during the additive Git transition.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::num::NonZeroU64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeWorkerKind {
    Read,
    Edit,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerResult {
    Returned,
    Failed,
    Panicked,
    NotStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DriverResult {
    Completed,
    Failed,
    Cancelled,
    Panicked,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DurableEffectState {
    Settled,
    PossiblyApplied { record: Option<NonZeroU64> },
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

#[derive(Debug)]
pub(crate) struct WorkerJoinFact {
    pub(crate) worker: u32,
    pub(crate) intent: String,
    pub(crate) result: WorkerResult,
    pub(crate) effect: DurableEffectState,
}

/// Exact sorted readable bindings and writable bits; historical digest, never file authority.
pub(crate) fn scope_sha256<'a>(
    bindings: impl Iterator<Item = (&'a str, &'a std::path::Path, bool)>,
) -> Result<[u8; 32]> {
    use ring::digest::{Context as DigestContext, SHA256};
    let mut digest = DigestContext::new(&SHA256);
    digest.update(b"aizen-speculation-worker-scope-v1\0");
    let mut previous = None;
    let mut count = 0usize;
    for (key, path, writable) in bindings {
        ensure!(
            count < 32
                && !key.is_empty()
                && key.len() <= 128
                && previous.is_none_or(|prior| prior < key),
            "worker scope keys invalid or unordered"
        );
        let path = path.to_str().context("worker scope path is not UTF-8")?;
        ensure!(
            !path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control),
            "worker scope path invalid"
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_paths_are_the_same_nominal_types_not_conversions() {
        let kind: crate::core::speculation::workers::NativeWorkerKind = NativeWorkerKind::Read;
        let _: NativeWorkerKind = kind;
        let result: crate::core::speculation::workers::WorkerResult = WorkerResult::Returned;
        let _: WorkerResult = result;
        let driver: crate::core::speculation::workers::DriverResult = DriverResult::Completed;
        let _: DriverResult = driver;
        let effect: crate::core::speculation::verification::DurableEffectState =
            DurableEffectState::Settled;
        let _: DurableEffectState = effect;
    }

    #[test]
    fn worker_enum_wire_labels_remain_exact() {
        for (value, label) in [
            (NativeWorkerKind::Read, "read"),
            (NativeWorkerKind::Edit, "edit"),
            (NativeWorkerKind::Write, "write"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), json!(label));
            assert_eq!(
                serde_json::from_value::<NativeWorkerKind>(json!(label)).unwrap(),
                value
            );
        }
        for (value, label) in [
            (WorkerResult::Returned, "returned"),
            (WorkerResult::Failed, "failed"),
            (WorkerResult::Panicked, "panicked"),
            (WorkerResult::NotStarted, "not_started"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), json!(label));
            assert_eq!(
                serde_json::from_value::<WorkerResult>(json!(label)).unwrap(),
                value
            );
        }
        for (value, label) in [
            (DriverResult::Completed, "completed"),
            (DriverResult::Failed, "failed"),
            (DriverResult::Cancelled, "cancelled"),
            (DriverResult::Panicked, "panicked"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), json!(label));
            assert_eq!(
                serde_json::from_value::<DriverResult>(json!(label)).unwrap(),
                value
            );
        }
        assert!(serde_json::from_value::<NativeWorkerKind>(json!("future")).is_err());
        assert!(serde_json::from_value::<WorkerResult>(json!("future")).is_err());
        assert!(serde_json::from_value::<DriverResult>(json!("future")).is_err());
    }

    #[test]
    fn effect_wire_shapes_are_strict_and_never_default_missing_evidence() {
        for value in [
            json!({"state":"settled"}),
            json!({"state":"unknown_worker"}),
            json!({"state":"possibly_applied","record":null}),
            json!({"state":"possibly_applied","record":1}),
        ] {
            let effect: DurableEffectState = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(effect).unwrap(), value);
        }
        for value in [
            json!({}),
            json!({"state":"future"}),
            json!({"state":"settled","record":1}),
            json!({"state":"unknown_worker","extra":true}),
            json!({"state":"possibly_applied"}),
            json!({"state":"possibly_applied","record":0}),
            json!({"state":"possibly_applied","record":-1}),
            json!({"state":"possibly_applied","record":1,"extra":true}),
        ] {
            assert!(
                serde_json::from_value::<DurableEffectState>(value.clone()).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn scope_digest_retains_legacy_domain_order_and_write_bits() {
        let bindings = [
            ("a", std::path::Path::new("/fixture/a.rs"), false),
            ("b", std::path::Path::new("/fixture/b.rs"), true),
        ];
        let digest = scope_sha256(bindings.into_iter()).unwrap();
        assert_eq!(
            digest,
            [
                141, 158, 17, 181, 1, 185, 28, 67, 42, 221, 62, 22, 168, 10, 71, 185, 112, 244, 73,
                211, 148, 52, 206, 253, 124, 98, 185, 181, 199, 227, 227, 202
            ]
        );
        assert_eq!(
            digest,
            crate::core::speculation::workers::scope_sha256(bindings.into_iter()).unwrap()
        );
        assert!(scope_sha256(bindings.into_iter().rev()).is_err());
        assert!(scope_sha256(std::iter::empty()).is_err());
        let read_only = [
            ("a", std::path::Path::new("/fixture/a.rs"), false),
            ("b", std::path::Path::new("/fixture/b.rs"), false),
        ];
        assert_ne!(digest, scope_sha256(read_only.into_iter()).unwrap());
    }
}
