//! Closed, bounded Desktop adoption protocol. Clients never supply plans, journals or argv.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Defaults {},
    Preview {
        source: Option<String>,
        destination_parent: Option<String>,
        consent: bool,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    Status {
        operation_id: Option<String>,
        offset: usize,
        limit: usize,
    },
    Inventory {
        operation_id: String,
        offset: usize,
        limit: usize,
    },
    Apply {
        operation_id: String,
        reviewed_digest: String,
        writers_stopped: bool,
        consent: bool,
    },
    Resume {
        operation_id: String,
        reviewed_digest: String,
        writers_stopped: bool,
        consent: bool,
    },
    Recover {
        operation_id: String,
        reviewed_digest: String,
        consent: bool,
    },
}
impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Defaults {} => {}
            Self::Preview {
                source,
                destination_parent,
                consent,
                attachments,
            } => {
                validate_attachments(attachments)?;
                ensure!(
                    *consent,
                    "adoption preview requires explicit path and execution consent"
                );
                for path in [source, destination_parent].into_iter().flatten() {
                    super::super::memory_api::text(path, 32768, false)?;
                    ensure!(
                        std::path::Path::new(path).is_absolute(),
                        "adoption paths must be absolute"
                    );
                }
            }
            Self::Status {
                operation_id,
                offset,
                limit,
            } => {
                page(*offset, *limit)?;
                if let Some(id) = operation_id {
                    operation_id_valid(id)?;
                }
            }
            Self::Inventory {
                operation_id,
                offset,
                limit,
            } => {
                operation_id_valid(operation_id)?;
                page(*offset, *limit)?;
            }
            Self::Apply {
                operation_id,
                reviewed_digest,
                writers_stopped,
                consent,
            }
            | Self::Resume {
                operation_id,
                reviewed_digest,
                writers_stopped,
                consent,
            } => {
                operation_id_valid(operation_id)?;
                digest(reviewed_digest)?;
                ensure!(
                    *consent && *writers_stopped,
                    "adoption requires explicit execution consent and all source writers stopped"
                );
            }
            Self::Recover {
                operation_id,
                reviewed_digest,
                consent,
            } => {
                operation_id_valid(operation_id)?;
                digest(reviewed_digest)?;
                ensure!(
                    *consent,
                    "adoption recovery requires explicit execution consent"
                );
            }
        }
        Ok(())
    }
}
pub(super) fn operation_id_valid(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid adoption operation id"
    );
    Ok(())
}
pub(super) fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid adoption reviewed digest"
    );
    Ok(())
}
fn page(offset: usize, limit: usize) -> Result<()> {
    ensure!(
        (1..=100).contains(&limit) && offset <= 100_000,
        "adoption page must have limit 1..100 and offset <= 100000"
    );
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Planning,
    Preview,
    Interrupted,
    PublishedVerified,
    SettlementUnknown,
    NeedsReview,
    CheckFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attachment {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}
pub(super) fn relative_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.contains('\\')
            && path.split('/').count() <= 9,
        "invalid adoption relative path"
    );
    for part in path.split('/') {
        ensure!(
            !part.is_empty()
                && part.len() <= 240
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.contains('~')
                && !part
                    .chars()
                    .any(|c| c.is_control() || ":<>\"|?*".contains(c)),
            "invalid adoption path component"
        );
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "reserved adoption path"
        );
    }
    Ok(())
}
pub(super) fn validate_attachments(items: &[Attachment]) -> Result<()> {
    ensure!(items.len() <= 32, "too many adoption attachments");
    let mut previous = "";
    let mut total = 0u64;
    for item in items {
        relative_path(&item.path)?;
        ensure!(
            item.path.as_str() > previous
                && matches!(item.path.split('/').collect::<Vec<_>>().as_slice(),
                ["archive", file] if !file.to_ascii_lowercase().ends_with(".md"))
                && item.bytes <= 8 * 1024 * 1024,
            "invalid adoption attachment"
        );
        digest(&item.sha256)?;
        previous = &item.path;
        total = total
            .checked_add(item.bytes)
            .ok_or_else(|| anyhow::anyhow!("attachment overflow"))?;
    }
    ensure!(
        total <= 16 * 1024 * 1024,
        "adoption attachment aggregate bound"
    );
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Diagnostic {
    pub code: String,
    pub path: Option<String>,
    pub expected_bytes: Option<u64>,
    pub actual_bytes: Option<u64>,
    pub limit_bytes: Option<u64>,
    pub remedy: String,
}
impl Diagnostic {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.code.as_str(),
                "file_too_large"
                    | "total_too_large"
                    | "attachment_limit"
                    | "attachment_authorization"
                    | "unsupported_layout"
                    | "invalid_content"
                    | "unsafe_filesystem"
                    | "source_changed"
                    | "invalid_request"
            ),
            "invalid adoption diagnostic code"
        );
        ensure!(
            matches!(
                self.remedy.as_str(),
                "review_attachments"
                    | "reduce_source"
                    | "repair_source"
                    | "recheck_source"
                    | "check_request"
            ),
            "invalid adoption remedy"
        );
        if let Some(path) = &self.path {
            relative_path(path)?;
        }
        ensure!(
            [self.expected_bytes, self.actual_bytes, self.limit_bytes]
                .into_iter()
                .flatten()
                .all(|n| n <= 4 * 1024 * 1024 * 1024),
            "diagnostic byte bound"
        );
        Ok(())
    }
    pub(super) fn message(&self) -> &'static str {
        match self.code.as_str() {
            "file_too_large" => "A source file exceeds its policy limit. Reduce the source or review the reported path.",
            "total_too_large" => "The source exceeds the bounded transfer size. Reduce the source before checking again.",
            "attachment_limit" => "Archive attachments exceed the review limits (32 files, 8 MiB each, 16 MiB total).",
            "attachment_authorization" => "The attachment inventory differs from the exact authorization. Check and review again.",
            "unsupported_layout" => "An unsupported source path was found. Repair the source layout; invalid Memory is never skipped.",
            "invalid_content" => "A supported Memory file is invalid. Repair the reported file and check again.",
            "source_changed" => "The source changed. Stop all writers and obtain a fresh reviewed plan.",
            "invalid_request" => "The transfer request is invalid. Check the request and review again.",
            _ => "A filesystem safety check failed. Review the source and retained operation before retrying.",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Summary {
    pub copy_files: usize,
    pub excluded_files: usize,
    pub total_bytes: u64,
    pub copied_bytes: u64,
    pub inventory_items: usize,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Operation {
    pub operation_id: String,
    pub provider: String,
    pub state: State,
    pub source: String,
    pub data_root: String,
    pub journal: String,
    pub backup: String,
    pub previous_root: String,
    pub reviewed_digest: Option<String>,
    pub summary: Option<Summary>,
    pub message: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub diagnostic: Option<Diagnostic>,
}
#[derive(Debug, Serialize)]
pub(crate) struct InventoryItem {
    pub path: String,
    pub directory: bool,
    pub disposition: String,
    pub reason: Option<String>,
    pub bytes: u64,
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum View {
    Defaults {
        source: String,
        provider: Option<String>,
        supported: bool,
        attachment_review_supported: bool,
        enabled: bool,
        managed_parent: String,
        warnings: Vec<String>,
    },
    Operation {
        operation: Box<Operation>,
    },
    History {
        operations: Vec<Operation>,
        offset: usize,
        total: usize,
    },
    Inventory {
        operation_id: String,
        items: Vec<InventoryItem>,
        offset: usize,
        total: usize,
    },
}
