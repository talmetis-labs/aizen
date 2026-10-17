//! Loaded-conversation reference identities and volatile, controller-issued use grants.
//! Blob hashes identify bytes, never authority. Only exact loaded user attachment occurrences are
//! registered; serialized metadata cannot hydrate a reference from another session or local path.

use crate::core::tool_media::{
    ImageMetadata, ImageProvenance, ImageUnavailable, PixelRect, ToolImage,
};
use crate::core::types::Message;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const MAX_LOADED_REFERENCES: usize = 32;
const MAX_LOADED_REFERENCE_BYTES: usize = 64 * 1024 * 1024;
pub const REFERENCE_SOURCE: &str = "user_reference";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceDescriptor {
    pub reference_id: String,
    pub original: ImageMetadata,
}

/// Both values are opaque runtime keys. Neither is a model-visible bearer capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceRoute {
    pub destination: String,
    pub account: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceGrantScope {
    pub owner: String,
    pub purpose: String,
    pub task_scope: String,
    pub route: ReferenceRoute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceDerivatives {
    pub original: bool,
    pub preview: bool,
    pub crop: bool,
}
impl ReferenceDerivatives {
    pub const ALL: Self = Self {
        original: true,
        preview: true,
        crop: true,
    };
    #[cfg(test)]
    pub const ORIGINAL: Self = Self {
        original: true,
        preview: false,
        crop: false,
    };
}

#[derive(Debug)]
struct Catalog {
    epoch: u64,
    entries: BTreeMap<String, ToolImage>,
}

#[derive(Debug, Clone)]
pub struct ReferenceRegistry {
    owner: String,
    catalog: Arc<Mutex<Catalog>>,
}

/// A non-serializable runtime lease. Clones/children share revocation, not broader selection.
#[derive(Debug, Clone)]
pub struct ReferenceAccess {
    registry: ReferenceRegistry,
    epoch: u64,
    live: Arc<AtomicBool>,
    scope: ReferenceGrantScope,
    ids: Vec<String>,
    derivatives: ReferenceDerivatives,
}

pub fn validate_reference_ids(ids: &[String]) -> Result<()> {
    ensure!(
        ids.len() <= crate::core::tool_media::MAX_REQUEST_IMAGES,
        "reference_ids accepts at most four references"
    );
    let mut seen = BTreeSet::new();
    for id in ids {
        ensure!(valid_reference_id(id), "invalid scoped reference ID");
        ensure!(seen.insert(id), "reference_ids must be unique");
    }
    Ok(())
}

fn valid_reference_id(id: &str) -> bool {
    id.starts_with("ref-")
        && id.len() > 4
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn fresh_reference_id(used: &BTreeSet<String>) -> String {
    loop {
        let id = format!(
            "ref-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_nanos())
                .unwrap_or_default(),
            crate::core::persist::unique_sequence()
        );
        if !used.contains(&id) {
            return id;
        }
    }
}

fn marker_index(image: &ToolImage) -> Option<usize> {
    let p = &image.metadata().provenance;
    (p.source == REFERENCE_SOURCE && p.crop.is_none() && p.parent_sha256.is_none()).then_some(())?;
    p.locator
        .as_deref()?
        .strip_prefix("attachment:")?
        .parse()
        .ok()
}

fn original_from_url(url: &str, id: &str, index: usize) -> Result<ToolImage> {
    ensure!(
        url.starts_with("data:image/"),
        "reference is not a loaded inline image"
    );
    let provenance = ImageProvenance {
        source: REFERENCE_SOURCE.into(),
        evidence_id: Some(id.into()),
        design_scope: None,
        approved_destination: None,
        locator: Some(format!("attachment:{index}")),
        parent_sha256: None,
        crop: None,
    };
    ToolImage::from_data_url(url, provenance)
}

impl ReferenceRegistry {
    pub fn new(owner: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            catalog: Arc::new(Mutex::new(Catalog {
                epoch: 0,
                entries: BTreeMap::new(),
            })),
        }
    }

    /// Called by the controller with its already-loaded canonical conversation, never by a model.
    /// Identity markers persist in existing ToolImage metadata; original URL/blob storage is untouched.
    /// A refresh invalidates all old grants, including grants held by a still-running child.
    pub fn register_loaded(&self, messages: &mut [Message]) -> Result<Vec<ReferenceDescriptor>> {
        let mut entries = BTreeMap::new();
        let mut used_ids = BTreeSet::new();
        let mut bytes = 0usize;
        // Newest attachments win the bounded in-memory catalog. Missing/evicted data is not hydrated
        // from arbitrary marker metadata or scanned blobs; stale markers remain diagnostic only.
        for message in messages.iter_mut().rev().filter(|m| m.role == "user") {
            for index in (0..message.images.len()).rev() {
                if entries.len() >= MAX_LOADED_REFERENCES {
                    break;
                }
                let existing = message
                    .tool_images
                    .iter()
                    .find(|image| marker_index(image) == Some(index));
                let existing_id = existing
                    .and_then(|image| image.metadata().provenance.evidence_id.as_deref())
                    .filter(|id| valid_reference_id(id) && !used_ids.contains(*id));
                let id = existing_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| fresh_reference_id(&used_ids));
                let Ok(original) = original_from_url(&message.images[index], &id, index) else {
                    continue;
                };
                // Never trust a serialized descriptor to relabel a different loaded occurrence.
                if let Some(existing) = existing {
                    let meta = existing.metadata();
                    if meta.sha256 != original.metadata().sha256
                        || meta.mime != original.metadata().mime
                        || meta.byte_len != original.metadata().byte_len
                        || meta.width != original.metadata().width
                        || meta.height != original.metadata().height
                    {
                        continue;
                    }
                }
                let Some(next_bytes) = bytes
                    .checked_add(original.metadata().byte_len)
                    .filter(|n| *n <= MAX_LOADED_REFERENCE_BYTES)
                else {
                    continue;
                };
                bytes = next_bytes;
                used_ids.insert(id.clone());
                message
                    .tool_images
                    .retain(|image| marker_index(image) != Some(index));
                message
                    .tool_images
                    .push(original.without_payload(ImageUnavailable::MetadataOnly));
                entries.insert(id, original);
            }
        }
        let descriptors = entries
            .iter()
            .map(|(id, image)| ReferenceDescriptor {
                reference_id: id.clone(),
                original: image.metadata().clone(),
            })
            .collect();
        let mut catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        catalog.epoch = catalog.epoch.wrapping_add(1);
        catalog.entries = entries;
        Ok(descriptors)
    }

    /// Controller-only consent boundary. Selecting IDs in a model tool call must not call this.
    /// A successful media approval/declaration alone is not a reference-purpose use grant.
    pub fn grant(
        &self,
        ids: &[String],
        scope: ReferenceGrantScope,
        derivatives: ReferenceDerivatives,
    ) -> Result<ReferenceAccess> {
        validate_reference_ids(ids)?;
        ensure!(scope.owner == self.owner, "reference grant owner mismatch");
        ensure!(
            !scope.purpose.trim().is_empty()
                && scope.purpose.len() <= 512
                && !scope.task_scope.is_empty()
                && scope.task_scope.len() <= 2048,
            "reference grant requires a bounded purpose and current task scope"
        );
        ensure!(
            !scope.route.destination.is_empty() && !scope.route.account.is_empty(),
            "reference grant requires an explicit destination and account route"
        );
        ensure!(
            derivatives.original || derivatives.preview || derivatives.crop,
            "reference grant has no allowed representation"
        );
        let catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            ids.iter().all(|id| catalog.entries.contains_key(id)),
            "selected reference unavailable in loaded conversation"
        );
        let selected_bytes: usize = ids
            .iter()
            .filter_map(|id| catalog.entries.get(id))
            .map(|image| image.metadata().byte_len)
            .sum();
        ensure!(
            selected_bytes <= crate::core::tool_media::MAX_REQUEST_IMAGE_BYTES,
            "selected reference originals exceed the request byte budget"
        );
        Ok(ReferenceAccess {
            registry: self.clone(),
            epoch: catalog.epoch,
            live: Arc::new(AtomicBool::new(true)),
            scope,
            ids: ids.to_vec(),
            derivatives,
        })
    }
}

impl ReferenceAccess {
    pub fn scope(&self) -> &ReferenceGrantScope {
        &self.scope
    }
    pub fn ids(&self) -> &[String] {
        &self.ids
    }
    pub fn derivatives(&self) -> ReferenceDerivatives {
        self.derivatives
    }
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "controller revocation boundary retained for active reference-use grants"
        )
    )]
    pub fn revoke(&self) {
        self.live.store(false, Ordering::Release);
    }

    pub fn validate_scope(&self, owner: &str, task: &str) -> Result<()> {
        ensure!(
            self.scope.owner == owner && self.scope.task_scope == task,
            "reference access is outside this conversation/task scope"
        );
        ensure!(self.live.load(Ordering::Acquire), "reference grant revoked");
        let catalog = self
            .registry
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            catalog.epoch == self.epoch,
            "reference grant expired; controller must renew it"
        );
        ensure!(
            self.ids.iter().all(|id| catalog.entries.contains_key(id)),
            "reference payload unavailable in loaded conversation"
        );
        Ok(())
    }

    /// Restrict an existing live grant to selected IDs and a fresh child namespace. Does not grant
    /// another purpose/provider/account; parent and child dispatch helper must supply the real route.
    pub fn select_child(
        &self,
        ids: &[String],
        owner: &str,
        parent_task: &str,
        child_task: &str,
        route: &ReferenceRoute,
    ) -> Result<Self> {
        validate_reference_ids(ids)?;
        self.validate_scope(owner, parent_task)?;
        ensure!(
            &self.scope.route == route,
            "reference setup required: child destination/account needs controller consent"
        );
        ensure!(
            ids.iter().all(|id| self.ids.contains(id)),
            "reference setup required: selected IDs have no live controller use grant"
        );
        ensure!(
            !child_task.is_empty() && child_task.len() <= 2048 && child_task != parent_task,
            "reference child requires a fresh task scope"
        );
        let mut child = self.clone();
        child.ids = ids.to_vec();
        child.scope.task_scope = child_task.into();
        Ok(child)
    }

    pub fn descriptors(&self) -> Result<Vec<ReferenceDescriptor>> {
        self.validate_scope(&self.scope.owner, &self.scope.task_scope)?;
        let catalog = self
            .registry
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            self.live.load(Ordering::Acquire) && catalog.epoch == self.epoch,
            "reference grant revoked or expired"
        );
        Ok(self
            .ids
            .iter()
            .filter_map(|id| {
                catalog.entries.get(id).map(|image| ReferenceDescriptor {
                    reference_id: id.clone(),
                    original: image.metadata().clone(),
                })
            })
            .collect())
    }

    pub fn original(&self, id: &str) -> Result<ToolImage> {
        self.validate_scope(&self.scope.owner, &self.scope.task_scope)?;
        ensure!(
            self.ids.iter().any(|allowed| allowed == id),
            "reference ID not granted in this task"
        );
        let catalog = self
            .registry
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            self.live.load(Ordering::Acquire) && catalog.epoch == self.epoch,
            "reference grant revoked or expired"
        );
        catalog
            .entries
            .get(id)
            .cloned()
            .context("reference original payload unavailable")
    }

    /// User attachments for a fresh child: selected originals only, not parent/sibling history.
    /// Actual transport still needs the current media capability and transmission approval.
    pub fn user_message(&self, text: impl Into<String>) -> Result<Message> {
        ensure!(
            self.derivatives.original,
            "whole-reference handoff requires an original grant; preview-only grants need a bounded preview projector"
        );
        let mut message = Message::user(text);
        for (index, id) in self.ids.iter().enumerate() {
            let image = self.original(id)?;
            message.images.push(image.data_url());
            let mut metadata = image.metadata().clone();
            metadata.provenance.locator = Some(format!("attachment:{index}"));
            message.tool_images.push(ToolImage::unavailable(
                metadata,
                ImageUnavailable::MetadataOnly,
            ));
        }
        Ok(message)
    }

    pub fn read(&self, id: &str, crop: Option<PixelRect>) -> Result<ToolImage> {
        ensure!(
            if crop.is_some() {
                self.derivatives.crop
            } else {
                self.derivatives.original
            },
            "requested reference representation has no live grant"
        );
        let original = self.original(id)?;
        let image = match crop {
            Some(rect) => original.crop_png(rect)?,
            None => original.clone(),
        };
        let mut provenance = image.metadata().provenance.clone();
        provenance.source = "reference_read".into();
        provenance.locator = Some(id.into());
        provenance.evidence_id = Some(format!(
            "reference-read-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        provenance.design_scope = None;
        provenance.approved_destination = None;
        provenance.parent_sha256 = Some(original.metadata().sha256.clone());
        ToolImage::from_bytes(&image.bytes()?, &image.metadata().mime, provenance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool_media::encode_rgba_png;

    fn attachment() -> Message {
        let image = ToolImage::from_bytes(
            &encode_rgba_png(&[9, 8, 7, 255], 1, 1).unwrap(),
            "image/png",
            ImageProvenance::new("fixture", None),
        )
        .unwrap();
        Message::user_with_images("reference", vec![image.data_url()])
    }
    fn scope() -> ReferenceGrantScope {
        ReferenceGrantScope {
            owner: "owner".into(),
            purpose: "match reference".into(),
            task_scope: "parent".into(),
            route: ReferenceRoute {
                destination: "endpoint-model".into(),
                account: "account-a".into(),
            },
        }
    }
    #[test]
    fn stable_identity_roundtrip_without_payload_or_restored_consent() {
        let registry = ReferenceRegistry::new("owner");
        let mut messages = vec![attachment(), attachment()];
        let descriptors = registry.register_loaded(&mut messages).unwrap();
        assert_ne!(descriptors[0].reference_id, descriptors[1].reference_id);
        assert_eq!(
            descriptors[0].original.sha256,
            descriptors[1].original.sha256
        );
        assert!(messages
            .iter()
            .all(|m| m.tool_images.iter().all(|i| !i.is_available())));
        let ids = vec![descriptors[0].reference_id.clone()];
        let access = registry
            .grant(&ids, scope(), ReferenceDerivatives::ALL)
            .unwrap();
        let raw = serde_json::to_string(&messages).unwrap();
        let mut loaded: Vec<Message> = serde_json::from_str(&raw).unwrap();
        let restored = ReferenceRegistry::new("owner");
        assert_eq!(restored.register_loaded(&mut loaded).unwrap(), descriptors);
        assert!(!raw.contains("account-a") && !raw.contains("match reference"));
        registry.register_loaded(&mut messages).unwrap();
        assert!(
            access.original(&ids[0]).is_err(),
            "refresh must revoke prior grants"
        );
    }
    #[test]
    fn same_pixels_are_not_global_or_sibling_capabilities() {
        let registry = ReferenceRegistry::new("owner");
        let mut messages = vec![attachment(), attachment()];
        let descriptors = registry.register_loaded(&mut messages).unwrap();
        let ids = vec![descriptors[0].reference_id.clone()];
        let access = registry
            .grant(&ids, scope(), ReferenceDerivatives::ALL)
            .unwrap();
        assert!(access.original(&descriptors[1].reference_id).is_err());
        assert!(access.original(&descriptors[0].original.sha256).is_err());
        assert!(access.validate_scope("other", "parent").is_err());
        let child = access
            .select_child(&ids, "owner", "parent", "child-a", &scope().route)
            .unwrap();
        assert!(child.validate_scope("owner", "child-b").is_err());
        assert_eq!(child.user_message("child").unwrap().images.len(), 1);
        let different_account = ReferenceRoute {
            account: "account-b".into(),
            ..scope().route
        };
        assert!(access
            .select_child(&ids, "owner", "parent", "child", &different_account)
            .is_err());
        child.revoke();
        assert!(access.original(&ids[0]).is_err());
    }
    #[test]
    fn metadata_cannot_hydrate_other_occurrences_and_tool_images_are_not_references() {
        let registry = ReferenceRegistry::new("owner");
        let mut messages = vec![attachment()];
        registry.register_loaded(&mut messages).unwrap();
        messages[0].images[0] = ToolImage::from_bytes(
            &encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
            "image/png",
            ImageProvenance::new("fixture", None),
        )
        .unwrap()
        .data_url();
        assert!(registry.register_loaded(&mut messages).unwrap().is_empty());
        messages[0].role = "tool".into();
        assert!(registry.register_loaded(&mut messages).unwrap().is_empty());
        assert!(registry
            .grant(&["ref-absent".into()], scope(), ReferenceDerivatives::ALL)
            .is_err());
    }
    #[test]
    fn bounds_derivatives_and_crop_lineage_are_enforced() {
        let registry = ReferenceRegistry::new("owner");
        let mut messages = vec![attachment()];
        let id = registry.register_loaded(&mut messages).unwrap()[0]
            .reference_id
            .clone();
        let access = registry
            .grant(
                std::slice::from_ref(&id),
                scope(),
                ReferenceDerivatives::ORIGINAL,
            )
            .unwrap();
        let rect = PixelRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        assert!(access.read(&id, Some(rect)).is_err());
        let all = registry
            .grant(
                std::slice::from_ref(&id),
                scope(),
                ReferenceDerivatives::ALL,
            )
            .unwrap();
        let crop = all.read(&id, Some(rect)).unwrap();
        assert_eq!(crop.metadata().provenance.source, "reference_read");
        assert_eq!(
            crop.metadata().provenance.locator.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            crop.metadata().provenance.parent_sha256.as_ref(),
            Some(&all.original(&id).unwrap().metadata().sha256)
        );
        assert!(crop.metadata().provenance.approved_destination.is_none());
        assert!(validate_reference_ids(&vec![id.clone(); 5]).is_err());
        assert!(validate_reference_ids(&[id.clone(), id]).is_err());
    }
}
