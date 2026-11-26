//! Durable snapshot identities shared by checkpoint storage and crash recovery.
//!
//! These are provenance, not restore capabilities. Deserialization never acquires admission, pins
//! an object or authorizes a write. Legacy registry IDs and Git-native workspace IDs are deliberately
//! separated by the backend tag; neither numeric IDs nor OIDs imply ordering or authorship.

use anyhow::{ensure, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Component, PathBuf};

/// A full Git object name. The length carries the repository's SHA-1/SHA-256 object format.
/// A zero OID is a ref-update sentinel, not a snapshot, and abbreviated/revision expressions are not
/// immutable identities. Object existence and type still need checking in the admitted repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct GitOid(String);

impl GitOid {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(
            matches!(value.len(), 40 | 64)
                && value.bytes().all(|b| b.is_ascii_hexdigit())
                && value.bytes().any(|b| b != b'0'),
            "snapshot requires a full nonzero SHA-1 or SHA-256 object ID"
        );
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for GitOid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Local provenance only; never put absolute machine paths in Git commit metadata. IDs in a legacy
/// reference belong to its old registry, not to the newer path-derived workspace admission domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBinding {
    pub repo_id: String,
    pub worktree_id: String,
    pub canonical_root: PathBuf,
    pub canonical_git_dir: PathBuf,
    pub canonical_common_git_dir: PathBuf,
}

impl SnapshotBinding {
    fn validate(&self) -> Result<()> {
        validate_id(&self.repo_id)?;
        validate_id(&self.worktree_id)?;
        for path in [
            &self.canonical_root,
            &self.canonical_git_dir,
            &self.canonical_common_git_dir,
        ] {
            ensure!(
                path.is_absolute()
                    && !path
                        .components()
                        .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
                "snapshot provenance requires absolute normalized paths"
            );
        }
        Ok(())
    }
}

fn validate_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid snapshot repository, worktree or task ID"
    );
    Ok(())
}

/// Immutable backend-qualified identity. No Ord implementation: neither a legacy ID in another
/// namespace nor an arbitrary Git OID is a successor boundary for co-op attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotRef {
    Legacy {
        binding: SnapshotBinding,
        checkpoint_id: u32,
    },
    Git {
        binding: SnapshotBinding,
        task_id: String,
        commit: GitOid,
    },
}

impl SnapshotRef {
    pub fn binding(&self) -> &SnapshotBinding {
        match self {
            Self::Legacy { binding, .. } | Self::Git { binding, .. } => binding,
        }
    }

    /// Structural validation only. Always recheck live identity, coverage, object type and admission
    /// before any later operation; a valid persisted record is still untrusted evidence.
    pub fn validate(&self) -> Result<()> {
        self.binding().validate()?;
        match self {
            Self::Legacy { checkpoint_id, .. } => {
                ensure!(*checkpoint_id > 0, "legacy checkpoint IDs start at one");
            }
            Self::Git { task_id, .. } => validate_id(task_id)?,
        }
        Ok(())
    }

    pub fn display_label(&self) -> String {
        match self {
            Self::Legacy { checkpoint_id, .. } => format!("legacy checkpoint #{checkpoint_id}"),
            Self::Git {
                task_id, commit, ..
            } => {
                format!("task {task_id} @ {}", &commit.as_str()[..12])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn binding() -> SnapshotBinding {
        let root = std::env::temp_dir().join("snapshot-identity-fixture");
        SnapshotBinding {
            repo_id: "repo-one".into(),
            worktree_id: "worktree-one".into(),
            canonical_git_dir: root.join(".git"),
            canonical_common_git_dir: root.join(".git"),
            canonical_root: root,
        }
    }

    #[test]
    fn full_git_oids_round_trip_in_both_formats() {
        for length in [40, 64] {
            let oid = GitOid::parse(&"A".repeat(length)).unwrap();
            assert_eq!(oid.as_str(), "a".repeat(length));
            assert_eq!(
                serde_json::from_str::<GitOid>(&serde_json::to_string(&oid).unwrap()).unwrap(),
                oid
            );
        }
    }

    #[test]
    fn non_object_names_are_rejected_even_during_deserialization() {
        for value in [
            "HEAD".into(),
            "abc1234".into(),
            "a".repeat(39),
            "a".repeat(41),
            "a".repeat(63),
            "a".repeat(65),
            "0".repeat(40),
            "0".repeat(64),
            "g".repeat(40),
            format!("{}\n", "a".repeat(40)),
            "--help".into(),
        ] {
            assert!(GitOid::parse(&value).is_err(), "{value:?}");
            assert!(serde_json::from_value::<GitOid>(serde_json::json!(value)).is_err());
        }
    }

    #[test]
    fn backend_tags_preserve_numeric_legacy_identity() {
        let legacy = SnapshotRef::Legacy {
            binding: binding(),
            checkpoint_id: 7,
        };
        let git = SnapshotRef::Git {
            binding: binding(),
            task_id: "task-7".into(),
            commit: GitOid::parse(&"7".repeat(64)).unwrap(),
        };
        for reference in [legacy.clone(), git.clone()] {
            reference.validate().unwrap();
            let json = serde_json::to_value(&reference).unwrap();
            assert_eq!(
                serde_json::from_value::<SnapshotRef>(json).unwrap(),
                reference
            );
        }
        assert_ne!(legacy, git);
        assert!(serde_json::from_value::<SnapshotRef>(serde_json::json!(7)).is_err());
    }

    #[test]
    fn equal_ids_in_different_worktrees_do_not_alias() {
        let a = SnapshotRef::Legacy {
            binding: binding(),
            checkpoint_id: 1,
        };
        let mut other = binding();
        other.worktree_id = "other".into();
        other.canonical_git_dir = other.canonical_common_git_dir.join("worktrees/other");
        let b = SnapshotRef::Legacy {
            binding: other,
            checkpoint_id: 1,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn malformed_provenance_and_zero_legacy_id_fail_validation() {
        let mut reference = SnapshotRef::Legacy {
            binding: binding(),
            checkpoint_id: 0,
        };
        assert!(reference.validate().is_err());
        if let SnapshotRef::Legacy {
            binding,
            checkpoint_id,
        } = &mut reference
        {
            *checkpoint_id = 1;
            binding.repo_id = "../../elsewhere".into();
        }
        assert!(reference.validate().is_err());
        if let SnapshotRef::Legacy { binding, .. } = &mut reference {
            binding.repo_id = "repo".into();
            binding.canonical_root = PathBuf::from("relative");
        }
        assert!(reference.validate().is_err());
    }

    #[test]
    fn mixed_or_future_backend_records_are_not_accepted() {
        let reference = SnapshotRef::Legacy {
            binding: binding(),
            checkpoint_id: 1,
        };
        let mut json = serde_json::to_value(&reference).unwrap();
        json["commit"] = serde_json::json!("a".repeat(40));
        assert!(serde_json::from_value::<SnapshotRef>(json).is_err());
        let mut json = serde_json::to_value(&reference).unwrap();
        json["backend"] = serde_json::json!("future");
        assert!(serde_json::from_value::<SnapshotRef>(json).is_err());
    }
}
