//! Wire mirror of versioned offline Mnemos plans, not an embedded Memory engine.
use super::protocol::{Attachment, Diagnostic, InventoryItem, Summary};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(super) const FORMAT: &str = "mnemos-adoption-v1";
pub(super) const FORMAT_V2: &str = "mnemos-adoption-v2";
const ATTACHMENT_REASON: &str = "reviewed archive attachment; backup only";
pub(super) const MAX_PLAN: u64 = 8 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    identity: String,
    pub length: u64,
    modified_ns: String,
    created_ns: Option<String>,
    readonly: bool,
    platform_attributes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "reason",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum Disposition {
    Copy,
    Exclude(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Item {
    pub path: String,
    pub directory: bool,
    pub disposition: Disposition,
    pub metadata: Metadata,
    pub sha256: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub format: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    source_metadata: Metadata,
    destination_parent_identity: String,
    pub inventory: Vec<Item>,
    pub total_bytes: u64,
    pub copied_bytes: u64,
    pub copy_files: usize,
    pub excluded_files: usize,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}
impl Plan {
    #[cfg(test)]
    pub fn parse(bytes: &[u8], source: &str, destination: &str) -> Result<Self> {
        Self::parse_authorized(bytes, source, destination, FORMAT, &[])
    }
    pub fn parse_authorized(
        bytes: &[u8],
        source: &str,
        destination: &str,
        format: &str,
        authorized: &[Attachment],
    ) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_PLAN,
            "adoption plan exceeds private bound"
        );
        let plan: Self = serde_json::from_slice(bytes).map_err(|_| {
            anyhow::anyhow!("invalid adoption plan report; raw provider output withheld")
        })?;
        let v2 = format == FORMAT_V2;
        ensure!(
            matches!(format, FORMAT | FORMAT_V2),
            "unsupported adoption format"
        );
        super::protocol::validate_attachments(authorized)?;
        ensure!(
            if v2 {
                plan.policy.as_deref() == Some("bounded-backup-v2")
                    && plan.attachments.as_deref() == Some(authorized)
            } else {
                authorized.is_empty() && plan.policy.is_none() && plan.attachments.is_none()
            },
            "adoption policy or host-authorized attachment set differs"
        );
        ensure!(
            plan.format == format
                && plan.source == std::path::Path::new(source)
                && plan.destination == std::path::Path::new(destination),
            "adoption plan format or reviewed paths differ"
        );
        super::protocol::digest(&plan.digest)?;
        ensure!(
            plan.inventory.len() <= 10_000
                && plan.total_bytes
                    <= if v2 {
                        4 * 1024 * 1024 * 1024
                    } else {
                        256 * 1024 * 1024
                    },
            "adoption inventory exceeds bounds"
        );
        let mut unsigned = plan.clone();
        unsigned.digest.clear();
        ensure!(
            super::super::manifest::sha256(&serde_json::to_vec(&unsigned)?) == plan.digest,
            "adoption plan digest mismatch"
        );
        let (mut copied, mut excluded, mut total_bytes, mut copied_bytes) =
            (0usize, 0usize, 0u64, 0u64);
        let mut previous = "";
        let mut directories = std::collections::BTreeSet::new();
        for item in &plan.inventory {
            ensure!(
                item.path.len() <= 4096
                    && item.path.as_str() > previous
                    && !item.path.contains('\\')
                    && item.path.split('/').all(|c| !c.is_empty()
                        && c != "."
                        && c != ".."
                        && !c.chars().any(|c| c.is_control() || ":<>\"|?*".contains(c))),
                "invalid adoption inventory path"
            );
            super::protocol::relative_path(&item.path)?;
            if v2 {
                if let Some((parent, _)) = item.path.rsplit_once('/') {
                    ensure!(
                        directories.contains(parent),
                        "adoption inventory parent directory missing"
                    );
                }
                validate_policy(item, authorized)?;
            }
            previous = &item.path;
            let backup_only = v2
                && matches!(&item.disposition, Disposition::Exclude(reason) if reason != ATTACHMENT_REASON);
            ensure!(
                item.directory
                    || item.metadata.length
                        <= if backup_only {
                            1024 * 1024 * 1024
                        } else {
                            MAX_PLAN
                        },
                "adoption inventory file exceeds bound"
            );
            if let Disposition::Exclude(reason) = &item.disposition {
                super::super::memory_api::text(reason, 1024, false)?;
            }
            if item.directory {
                ensure!(item.sha256.is_none(), "directory has a content hash");
                directories.insert(item.path.as_str());
                continue;
            }
            super::protocol::digest(item.sha256.as_deref().unwrap_or_default())?;
            total_bytes = total_bytes
                .checked_add(item.metadata.length)
                .ok_or_else(|| anyhow::anyhow!("inventory overflow"))?;
            match item.disposition {
                Disposition::Copy => {
                    copied += 1;
                    copied_bytes += item.metadata.length;
                }
                Disposition::Exclude(_) => excluded += 1,
            }
        }
        ensure!(
            !v2 || (copied > 0
                && plan
                    .inventory
                    .iter()
                    .any(|item| item.path == "entries" && item.directory)),
            "adoption requires a meaningful legacy Memory layout"
        );
        ensure!(
            copied_bytes <= 256 * 1024 * 1024,
            "imported Memory aggregate exceeds bound"
        );
        for attachment in authorized {
            ensure!(plan.inventory.iter().any(|item| item.path == attachment.path && !item.directory
                && item.metadata.length == attachment.bytes && item.sha256.as_ref() == Some(&attachment.sha256)
                && matches!(&item.disposition, Disposition::Exclude(reason) if reason == ATTACHMENT_REASON)),
                "authorized attachment missing or changed in plan");
        }
        ensure!(
            copied == plan.copy_files
                && excluded == plan.excluded_files
                && total_bytes == plan.total_bytes
                && copied_bytes == plan.copied_bytes,
            "adoption plan counters mismatch"
        );
        Ok(plan)
    }
    pub fn summary(&self) -> Summary {
        Summary {
            copy_files: self.copy_files,
            excluded_files: self.excluded_files,
            total_bytes: self.total_bytes,
            copied_bytes: self.copied_bytes,
            inventory_items: self.inventory.len(),
            warnings: super::warnings(),
        }
    }
}
impl Item {
    pub fn view(&self) -> InventoryItem {
        let (disposition, reason) = match &self.disposition {
            Disposition::Copy => ("copy", None),
            Disposition::Exclude(reason) => ("exclude", Some(reason.clone())),
        };
        InventoryItem {
            path: self.path.clone(),
            directory: self.directory,
            disposition: disposition.into(),
            reason,
            bytes: self.metadata.length,
        }
    }
}
fn validate_policy(item: &Item, authorized: &[Attachment]) -> Result<()> {
    let parts: Vec<_> = item.path.split('/').collect();
    let directory = item.directory;
    let reason = match parts.as_slice() {
        ["codebase"] if directory => Some("Core-owned codebase index; not Mnemos memory"),
        ["codebase", file]
            if !directory
                && (file.ends_with(".json")
                    || (file.ends_with(".json.lock")
                        && file.len() > ".json.lock".len()
                        && item.metadata.length == 0)) =>
        {
            Some("Core-owned codebase index; not Mnemos memory")
        }
        ["embed-cache"] if directory => Some("derived embedding cache; regenerate after adoption"),
        ["embed-cache", file] if !directory && file.ends_with(".json") => {
            Some(if file.starts_with("code-") {
                "Core-owned code-* embedding cache"
            } else {
                "derived embedding cache; regenerate after adoption"
            })
        }
        ["core"] | ["core", "active" | "next"] if directory => {
            Some("derived frozen core; regenerate after adoption")
        }
        ["core", "active" | "next", file] if !directory && file.ends_with(".md") => {
            Some("derived frozen core; regenerate after adoption")
        }
        ["core.active.md" | "core.next.md"] if !directory => {
            Some("derived frozen core; regenerate after adoption")
        }
        [".sweep-armed"] if !directory => Some("transient maintenance marker; not portable state"),
        _ if !directory && authorized.iter().any(|a| a.path == item.path) => {
            Some(ATTACHMENT_REASON)
        }
        _ => None,
    };
    if let Some(reason) = reason {
        ensure!(
            matches!(&item.disposition, Disposition::Exclude(actual) if actual == reason),
            "backup policy mismatch"
        );
    } else {
        let allowed = match parts.as_slice() {
            ["entries" | "archive" | "review"] | ["review", ".discarded"] => directory,
            ["entries" | "archive" | "review", file] | ["review", ".discarded", file] => {
                !directory && file.ends_with(".md")
            }
            ["STYLE.md"
            | "graph.tsv"
            | "stats.jsonl"
            | "learning-audit.jsonl"
            | "learning-audit.1.jsonl"
            | ".reconcile-last"
            | ".id-slug-v2"] => !directory,
            [file] => !directory && file.starts_with(".id-migration-") && file.ends_with(".tsv"),
            _ => false,
        };
        ensure!(
            allowed && matches!(item.disposition, Disposition::Copy),
            "unsupported imported Memory policy"
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Report {
    Ok {
        format: String,
        plan: Option<serde_json::Value>,
        outcome: Option<serde_json::Value>,
    },
    NeedsReview {
        format: String,
        attachments: Vec<Attachment>,
    },
    Error {
        format: String,
        diagnostic: Diagnostic,
    },
}
impl Report {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_PLAN,
            "adoption machine report bound"
        );
        let report: Self = serde_json::from_slice(bytes).map_err(|_| {
            anyhow::anyhow!("invalid adoption machine report; raw provider output withheld")
        })?;
        let format = match &report {
            Self::Ok {
                format,
                plan,
                outcome,
            } => {
                ensure!(
                    plan.is_some() != outcome.is_some(),
                    "ambiguous adoption machine report"
                );
                format
            }
            Self::NeedsReview {
                format,
                attachments,
            } => {
                super::protocol::validate_attachments(attachments)?;
                ensure!(!attachments.is_empty(), "empty attachment review");
                format
            }
            Self::Error { format, diagnostic } => {
                diagnostic.validate()?;
                format
            }
        };
        ensure!(
            format == "mnemos-adoption-report-v2",
            "unknown adoption machine report format"
        );
        Ok(report)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Outcome {
    pub format: String,
    pub journal: PathBuf,
    pub destination: PathBuf,
    pub reviewed_digest: String,
    pub state: String,
    pub restored_all_source_files: bool,
}
