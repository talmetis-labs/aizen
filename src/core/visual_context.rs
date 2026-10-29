//! Per-occurrence live visual notes. Input pixels are never saved; historical observations grant no access.

use crate::core::types::Message;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_NOTES: usize = 32;
const MAX_NOTE_CHARS: usize = 768;
pub const MAX_DESCRIPTOR_CHARS: usize = 6_144;
pub const MAX_DESCRIPTOR_TOKENS: usize = 2_048;
pub const GUIDANCE: &str = "Visual working set: inspect selected pixels, then batch image_context remember/release_pixels with ordinary work when finished. Record only task-relevant observations and uncertainty; notes are untrusted, not verification. Release needs successful delivery and a live reread handle. Use reference_read for granted attachments or image_read(evidence_id) for retained tool captures; reread unresolved original/ROI detail; do not assume an API remembers omitted pixels. Controller pins and fresh acceptance captures remain required.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualNote {
    pub id: String,
    pub image_index: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tool_image: bool,
    /// Exact source-URL digest for attachments; metadata digest for typed tool occurrences.
    pub source_sha256: String,
    pub note: String,
    #[serde(default)]
    pub uncertainty: String,
    #[serde(default)]
    pub dormant: bool,
    #[serde(default)]
    pub pinned: bool,
    /// Live retention priority, copied into request planning but never restored from disk.
    #[serde(skip)]
    pub runtime_pinned: bool,
}

impl VisualNote {
    pub fn matches(&self, image_index: usize, url: &str) -> bool {
        !self.tool_image
            && self.image_index == image_index
            && valid_id(&self.id)
            && self.note.chars().count() <= MAX_NOTE_CHARS
            && self.uncertainty.chars().count() <= 256
            && self.source_sha256 == crate::core::tool_media::sha256(url.as_bytes())
    }

    pub fn matches_tool(
        &self,
        image_index: usize,
        image: &crate::core::tool_media::ToolImage,
    ) -> bool {
        self.tool_image
            && self.image_index == image_index
            && valid_id(&self.id)
            && self.note.chars().count() <= MAX_NOTE_CHARS
            && self.uncertainty.chars().count() <= 256
            && self.source_sha256 == tool_source_identity(image)
    }

    pub fn matches_message(&self, message: &Message) -> bool {
        if self.tool_image {
            message
                .tool_images
                .get(self.image_index)
                .is_some_and(|image| self.matches_tool(self.image_index, image))
        } else {
            message
                .images
                .get(self.image_index)
                .is_some_and(|url| self.matches(self.image_index, url))
        }
    }

    pub fn releases_pixels(&self) -> bool {
        self.dormant && !self.pinned && !self.note.trim().is_empty()
    }
}

fn tool_source_identity(image: &crate::core::tool_media::ToolImage) -> String {
    let mut metadata = image.metadata().clone();
    // Saved disclosure approval is not part of source identity and never becomes runtime authority.
    metadata.provenance.approved_destination = None;
    crate::core::tool_media::sha256(
        &serde_json::to_vec(&metadata).expect("image metadata serializes"),
    )
}

#[derive(Debug, Clone)]
enum Reread {
    Reference(String),
    Evidence(String),
}

#[derive(Debug, Clone)]
struct Entry {
    state: VisualNote,
    delivered: bool,
    reread: Option<Reread>,
    controller_pinned: bool,
    tool_sha256: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct VisualRequirements {
    pub references: Vec<String>,
    pub captures: Vec<(String, String, String)>,
}

impl VisualRequirements {
    fn occurrences(&self, messages: &[Message]) -> BTreeSet<(usize, bool, usize)> {
        let mut selected = BTreeSet::new();
        if self.references.is_empty() && self.captures.is_empty() {
            return selected;
        }
        let mut references = BTreeSet::new();
        let mut captures = BTreeSet::new();
        let complete: BTreeSet<_> = crate::llm::media::completed_tool_image_occurrences(messages)
            .into_iter()
            .map(|image| (image.message_index, image.image_index))
            .collect();
        for (mi, message) in messages.iter().enumerate().rev() {
            for ii in (0..message.images.len()).rev() {
                for image in &message.tool_images {
                    let provenance = &image.metadata().provenance;
                    if provenance.source == crate::core::reference_media::REFERENCE_SOURCE
                        && provenance.locator.as_deref()
                            == Some(format!("attachment:{ii}").as_str())
                    {
                        if let Some(id) = provenance
                            .evidence_id
                            .as_ref()
                            .filter(|id| self.references.contains(id))
                        {
                            if references.insert(id.clone()) {
                                selected.insert((mi, false, ii));
                            }
                        }
                    }
                }
            }
            for (ii, image) in message.tool_images.iter().enumerate().rev() {
                if !complete.contains(&(mi, ii)) {
                    continue;
                }
                let provenance = &image.metadata().provenance;
                for (id, hash, scope) in &self.captures {
                    if provenance.evidence_id.as_ref() == Some(id)
                        && provenance.design_scope.as_ref() == Some(scope)
                        && &image.metadata().sha256 == hash
                        && captures.insert(id.clone())
                    {
                        selected.insert((mi, true, ii));
                    }
                }
            }
        }
        selected
    }
}

#[derive(Debug, Default)]
pub struct VisualContext {
    entries: BTreeMap<String, Entry>,
    revision: u64,
    group_mutations: BTreeSet<String>,
}

fn valid_id(id: &str) -> bool {
    id.starts_with("visual-")
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl VisualContext {
    #[cfg(test)]
    pub fn refresh(
        &mut self,
        messages: &mut [Message],
        refs: Option<&crate::core::reference_media::ReferenceAccess>,
    ) {
        self.refresh_scoped(messages, refs, &[]);
    }

    #[cfg(test)]
    pub fn refresh_scoped(
        &mut self,
        messages: &mut [Message],
        refs: Option<&crate::core::reference_media::ReferenceAccess>,
        retained: &[crate::core::tool_media::ToolImage],
    ) {
        self.refresh_required(messages, refs, retained, &VisualRequirements::default())
            .unwrap();
    }

    pub(crate) fn pinned_occurrences(
        &self,
        messages: &[Message],
        required: &VisualRequirements,
    ) -> BTreeSet<(usize, bool, usize)> {
        let mut pinned = required.occurrences(messages);
        for (mi, message) in messages.iter().enumerate() {
            for (tool, count) in [
                (false, message.images.len()),
                (true, message.tool_images.len()),
            ] {
                for ii in 0..count {
                    let live_pin = message.visual_context.iter().any(|state| {
                        state.tool_image == tool
                            && state.image_index == ii
                            && self.entries.get(&state.id).is_some_and(|entry| {
                                entry.state.pinned
                                    && entry.state.source_sha256 == state.source_sha256
                            })
                            && state.matches_message(message)
                    });
                    if live_pin {
                        pinned.insert((mi, tool, ii));
                    }
                }
            }
        }
        pinned
    }

    pub(crate) fn refresh_required(
        &mut self,
        messages: &mut [Message],
        refs: Option<&crate::core::reference_media::ReferenceAccess>,
        retained: &[crate::core::tool_media::ToolImage],
        required: &VisualRequirements,
    ) -> Result<()> {
        let controller_pins = required.occurrences(messages);
        let priority = self.pinned_occurrences(messages, required);
        ensure!(priority.len() <= MAX_NOTES, "live image pins exceed the visual working-set limit; unpin optional images before continuing");
        for message in messages.iter_mut() {
            for state in &mut message.visual_context {
                state.runtime_pinned = false;
            }
        }
        let complete: BTreeSet<_> = crate::llm::media::completed_tool_image_occurrences(messages)
            .into_iter()
            .map(|image| (image.message_index, image.image_index))
            .collect();
        let mut present = BTreeSet::new();
        let mut occupied = BTreeSet::new();
        let mut changed = false;
        // Live pins win over newer optional occurrences; saved pin flags alone never get priority.
        for pinned_pass in [true, false] {
            for message_index in (0..messages.len()).rev() {
                let message = &messages[message_index];
                let attachments = (0..message.images.len()).rev().map(|index| (false, index));
                let outputs = (0..message.tool_images.len())
                    .rev()
                    .filter(|&index| {
                        message.tool_images[index].metadata().provenance.source
                            != crate::core::reference_media::REFERENCE_SOURCE
                            && (complete.contains(&(message_index, index))
                                || message.role == "system")
                    })
                    .map(|index| (true, index));
                let occurrences: Vec<_> = attachments.chain(outputs).collect();
                for (tool_image, image_index) in occurrences {
                    if priority.contains(&(message_index, tool_image, image_index)) != pinned_pass {
                        continue;
                    }
                    if present.len() >= MAX_NOTES {
                        break;
                    }
                    let message = &messages[message_index];
                    let source = if tool_image {
                        tool_source_identity(&message.tool_images[image_index])
                    } else {
                        crate::core::tool_media::sha256(message.images[image_index].as_bytes())
                    };
                    let saved = message
                        .visual_context
                        .iter()
                        .find(|state| {
                            state.tool_image == tool_image
                                && state.image_index == image_index
                                && state.matches_message(message)
                        })
                        .cloned();
                    let mut id =
                        saved
                            .as_ref()
                            .map(|state| state.id.clone())
                            .unwrap_or_else(|| {
                                format!(
                                    "visual-{}-{}",
                                    std::process::id(),
                                    crate::core::persist::unique_sequence()
                                )
                            });
                    if !occupied.insert(id.clone()) {
                        id = format!(
                            "visual-{}-{}",
                            std::process::id(),
                            crate::core::persist::unique_sequence()
                        );
                    }
                    present.insert(id.clone());
                    let reread = if tool_image {
                        let image = &message.tool_images[image_index];
                        let provenance = &image.metadata().provenance;
                        if provenance.source == "reference_read" {
                            provenance
                                .locator
                                .as_ref()
                                .filter(|id| refs.is_some_and(|access| access.original(id).is_ok()))
                                .cloned()
                                .map(Reread::Reference)
                        } else {
                            provenance
                                .evidence_id
                                .as_ref()
                                .filter(|_| {
                                    retained.iter().any(|candidate| {
                                        candidate.metadata() == image.metadata()
                                            && candidate.is_available()
                                    })
                                })
                                .cloned()
                                .map(Reread::Evidence)
                        }
                    } else {
                        message
                            .tool_images
                            .iter()
                            .find(|image| {
                                let p = &image.metadata().provenance;
                                p.source == crate::core::reference_media::REFERENCE_SOURCE
                                    && p.locator.as_deref()
                                        == Some(format!("attachment:{image_index}").as_str())
                            })
                            .and_then(|image| image.metadata().provenance.evidence_id.clone())
                            .filter(|id| refs.is_some_and(|access| access.original(id).is_ok()))
                            .map(Reread::Reference)
                    };
                    changed |= !self.entries.contains_key(&id);
                    let entry = self.entries.entry(id.clone()).or_insert_with(|| {
                        let mut state = saved.unwrap_or(VisualNote {
                            id: id.clone(),
                            image_index,
                            tool_image,
                            source_sha256: source.clone(),
                            note: String::new(),
                            uncertainty: String::new(),
                            dormant: false,
                            pinned: false,
                            runtime_pinned: false,
                        });
                        state.id = id.clone();
                        // Saved pins describe the previous task; they never pin a fresh execution.
                        state.pinned = false;
                        Entry {
                            state,
                            delivered: false,
                            reread: None,
                            controller_pinned: false,
                            tool_sha256: None,
                        }
                    });
                    if entry.state.source_sha256 != source {
                        changed = true;
                        entry.state = VisualNote {
                            id: id.clone(),
                            image_index,
                            tool_image,
                            source_sha256: source,
                            note: String::new(),
                            uncertainty: String::new(),
                            dormant: false,
                            pinned: false,
                            runtime_pinned: false,
                        };
                        entry.delivered = false;
                    }
                    entry.state.image_index = image_index;
                    entry.tool_sha256 = tool_image.then(|| {
                        messages[message_index].tool_images[image_index]
                            .metadata()
                            .sha256
                            .clone()
                    });
                    entry.reread = reread;
                    let controller_pinned =
                        controller_pins.contains(&(message_index, tool_image, image_index));
                    if entry.controller_pinned != controller_pinned {
                        changed = true;
                        entry.controller_pinned = controller_pinned;
                        if controller_pinned {
                            entry.state.dormant = false;
                        }
                    }
                    entry.state.runtime_pinned = controller_pinned || entry.state.pinned;
                    messages[message_index].visual_context.retain(|state| {
                        state.tool_image != tool_image || state.image_index != image_index
                    });
                    messages[message_index]
                        .visual_context
                        .push(entry.state.clone());
                }
                let valid: Vec<_> = messages[message_index]
                    .visual_context
                    .iter()
                    .filter(|state| state.matches_message(&messages[message_index]))
                    .cloned()
                    .collect();
                messages[message_index].visual_context = valid;
            }
        }
        let before = self.entries.len();
        self.entries.retain(|id, _| present.contains(id));
        if changed || before != self.entries.len() {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(())
    }

    pub fn begin_group(&mut self) {
        self.group_mutations.clear();
    }
    pub fn reject_group_conflicts(&mut self, ids: impl IntoIterator<Item = String>) {
        let mut seen = BTreeSet::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                self.group_mutations.insert(id);
            }
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn has_images(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn record_delivery(&mut self, receipt: &crate::llm::media::ImageDeliveryReceipt) {
        for delivered in &receipt.user_images {
            for entry in self.entries.values_mut() {
                if !entry.state.tool_image
                    && delivered.visual_id.as_deref() == Some(entry.state.id.as_str())
                    && entry.state.source_sha256 == delivered.source_sha256
                {
                    entry.delivered = true;
                }
            }
        }
        for delivered in &receipt.tool_images {
            for entry in self.entries.values_mut() {
                if entry.state.tool_image
                    && delivered
                        .visual_identity
                        .as_ref()
                        .is_some_and(|(id, source)| {
                            id == &entry.state.id && source == &entry.state.source_sha256
                        })
                    && entry.tool_sha256.as_deref() == Some(delivered.sha256.as_str())
                {
                    entry.delivered = true;
                }
            }
        }
    }

    pub fn mutate(
        &mut self,
        id: &str,
        action: &str,
        note: Option<&str>,
        uncertainty: Option<&str>,
        release: bool,
    ) -> Result<()> {
        ensure!(
            !self.group_mutations.contains(id),
            "conflicting image_context mutations for one occurrence in this tool group"
        );
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("unknown image occurrence in this execution scope"))?;
        if let Some(note) = note {
            ensure!(
                !note.trim().is_empty() && note.chars().count() <= MAX_NOTE_CHARS,
                "visual note must contain 1..768 characters"
            );
        }
        let uncertainty = uncertainty.unwrap_or("");
        ensure!(
            uncertainty.chars().count() <= 256,
            "uncertainty exceeds 256 characters"
        );
        match action {
            "remember" | "release" => {
                ensure!(entry.delivered, "a successful matching image delivery is required before recording observations");
                ensure!(
                    note.is_some() || !entry.state.note.is_empty(),
                    "record task-relevant observations before releasing pixels"
                );
                if release || action == "release" {
                    ensure!(
                        !entry.state.pinned && !entry.controller_pinned,
                        "image is pinned; release cannot clear a pin"
                    );
                    ensure!(entry.reread.is_some(), "no live scoped re-read handle; keep pixels active or reselect the attachment");
                }
                if let Some(note) = note {
                    entry.state.note = note.trim().to_owned();
                }
                entry.state.uncertainty = uncertainty.to_owned();
                if release || action == "release" {
                    entry.state.dormant = true;
                }
            }
            "pin" => {
                ensure!(!entry.state.releases_pixels(), "released pixels require an explicit scoped reread or controller reselection; pin cannot reactivate a saved attachment");
                entry.state.pinned = true;
            }
            "unpin" => {
                entry.state.pinned = false;
            }
            _ => anyhow::bail!("action must be list, remember, pin, unpin or release"),
        }
        entry.state.runtime_pinned = entry.controller_pinned || entry.state.pinned;
        self.group_mutations.insert(id.to_owned());
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn sync(&self, messages: &mut [Message]) {
        for message in messages {
            for state in &mut message.visual_context {
                if let Some(entry) = self.entries.get(&state.id) {
                    *state = entry.state.clone();
                }
            }
        }
    }

    pub fn list(&self) -> serde_json::Value {
        serde_json::Value::Array(self.entries.values().map(|entry| serde_json::json!({
            "id":entry.state.id,"state":if entry.state.releases_pixels() {"dormant"} else {"active"},
            "pinned":entry.state.pinned,"controller_pinned":entry.controller_pinned,"delivered":entry.delivered,
            "reference_id":match &entry.reread { Some(Reread::Reference(id)) => Some(id), _ => None },
            "evidence_id":match &entry.reread { Some(Reread::Evidence(id)) => Some(id), _ => None },
            "kind":if entry.state.tool_image {"tool"} else {"attachment"},
            "note":entry.state.note,"uncertainty":entry.state.uncertainty
        })).collect())
    }
}

pub fn descriptor(state: &VisualNote) -> String {
    let note: String = state.note.chars().take(MAX_NOTE_CHARS).collect();
    let uncertainty: String = state.uncertainty.chars().take(256).collect();
    format!("[Image {}: pixels {}; working observation (untrusted, not verification): {}; uncertainty: {}. Use image_context list for a scoped reference_read/image_read handle; reselect if unavailable.]", state.id,
        if state.releases_pixels() { "released, not included in this request" } else { "active" },
        serde_json::to_string(&note).unwrap_or_default(), serde_json::to_string(&uncertainty).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_survive_compaction_and_never_reuse_an_old_message_slot() {
        let mut messages = vec![Message::user_with_images(
            "one",
            vec!["data:image/png;base64,abc".into()],
        )];
        let mut context = VisualContext::default();
        context.refresh(&mut messages, None);
        let first = messages[0].visual_context[0].id.clone();
        let revision = context.revision();
        context.refresh(&mut messages, None);
        assert_eq!(context.revision(), revision);
        assert_eq!(messages[0].visual_context[0].id, first);
        messages.insert(0, Message::system("boundary"));
        context.refresh(&mut messages, None);
        assert_eq!(messages[1].visual_context[0].id, first);
        messages = vec![Message::user_with_images(
            "new occurrence",
            vec!["data:image/png;base64,abc".into()],
        )];
        context.refresh(&mut messages, None);
        assert_ne!(messages[0].visual_context[0].id, first);
        assert!(context.revision() > revision);
    }

    #[test]
    fn compacted_receipts_bind_stable_occurrences_not_old_message_coordinates() {
        use crate::core::tool_media::{encode_rgba_png, ImageProvenance, ToolImage};
        let destination = "a".repeat(64);
        let image = |id: &str| {
            let mut provenance = ImageProvenance::new("browser_screenshot", None);
            provenance.evidence_id = Some(id.into());
            provenance.design_scope = Some("own-task".into());
            provenance.approved_destination = Some(destination.clone());
            ToolImage::from_bytes(
                &encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
                "image/png",
                provenance,
            )
            .unwrap()
        };
        let first_image = image("first");
        let second_image = image("second");
        let mut messages = vec![
            Message::system("SYS"),
            Message::user("inspect"),
            Message::assistant("working"),
            Message::assistant("continuing"),
        ];
        let group = |id: &str, image| {
            vec![
                Message::assistant_tool_calls(vec![crate::core::types::ToolCall {
                    id: id.into(),
                    kind: "function".into(),
                    function: crate::core::types::FunctionCall {
                        name: "image_read".into(),
                        arguments: "{}".into(),
                    },
                }]),
                Message::tool_observation(id, "captured", vec![image]),
            ]
        };
        messages.extend(group("first", first_image.clone()));
        messages.push(Message::assistant("between captures"));
        messages.extend(group("second", second_image.clone()));
        messages.push(Message::user("continue"));
        let mut context = VisualContext::default();
        context.refresh_scoped(&mut messages, None, &[first_image, second_image]);
        let first = messages[5].visual_context[0].id.clone();
        let second = messages[8].visual_context[0].id.clone();
        crate::agent::compact::splice_compacted(&mut messages, 9, "summary").unwrap();
        let (_, receipt) = crate::llm::media::project_messages_for_send(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&destination),
            crate::llm::image_send::ImageSendPolicy::Original,
        );
        assert_eq!(receipt.tool_images.len(), 1);
        assert_eq!(receipt.tool_images[0].message_index, 5);
        context.record_delivery(&receipt);
        assert!(!context.entries[&first].delivered);
        assert!(context.entries[&second].delivered);
        assert!(context
            .mutate(&first, "release", Some("Unseen duplicate"), None, true)
            .is_err());
        context
            .mutate(
                &second,
                "release",
                Some("Second occurrence only"),
                None,
                true,
            )
            .unwrap();
    }

    #[test]
    fn conflicting_mutations_reject_both_and_model_unpin_cannot_clear_controller_pin() {
        let mut messages = vec![Message::user_with_images(
            "inspect",
            vec!["data:image/png;base64,abc".into()],
        )];
        let mut context = VisualContext::default();
        context.refresh(&mut messages, None);
        let id = messages[0].visual_context[0].id.clone();
        context.begin_group();
        context.reject_group_conflicts([id.clone(), id.clone()]);
        assert!(context.mutate(&id, "pin", None, None, false).is_err());
        assert!(context.mutate(&id, "unpin", None, None, false).is_err());
        context.begin_group();
        context.entries.get_mut(&id).unwrap().controller_pinned = true;
        context.mutate(&id, "unpin", None, None, false).unwrap();
        assert!(context.entries[&id].controller_pinned);
        assert!(!context.entries[&id].state.pinned);
    }

    #[test]
    fn current_selection_remains_manageable_and_pin_cannot_restore_dormant_pixels() {
        let mut messages = (0..MAX_NOTES + 1)
            .map(|i| {
                Message::user_with_images(
                    format!("occurrence {i}"),
                    vec![format!("data:image/png;base64,{i}")],
                )
            })
            .collect::<Vec<_>>();
        let mut context = VisualContext::default();
        context.refresh(&mut messages, None);
        let latest = messages.last().unwrap().visual_context[0].id.clone();
        assert!(context.entries.contains_key(&latest));
        assert_eq!(context.entries.len(), MAX_NOTES);
        assert!(messages[0].visual_context.is_empty());
        {
            let state = &mut context.entries.get_mut(&latest).unwrap().state;
            state.note = "Recorded before ending the previous visual task".into();
            state.dormant = true;
        }
        context.sync(&mut messages);
        let mut restored: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let mut fresh = VisualContext::default();
        fresh.refresh(&mut restored, None);
        assert!(fresh.mutate(&latest, "pin", None, None, false).is_err());
        assert!(fresh.entries[&latest].state.releases_pixels());
        assert!(!fresh.entries[&latest].delivered);
    }

    #[test]
    fn tool_pixels_release_only_with_exact_delivery_and_live_scoped_reread() {
        use crate::core::types::{FunctionCall, ToolCall};
        let destination = "a".repeat(64);
        let mut provenance =
            crate::core::tool_media::ImageProvenance::new("browser_screenshot", None);
        provenance.evidence_id = Some("capture-original".into());
        provenance.design_scope = Some("own-scope".into());
        provenance.approved_destination = Some(destination.clone());
        let image = crate::core::tool_media::ToolImage::from_bytes(
            &crate::core::tool_media::encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
            "image/png",
            provenance,
        )
        .unwrap();
        let call = |id: &str| {
            Message::assistant_tool_calls(vec![ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "image_read".into(),
                    arguments: "{}".into(),
                },
            }])
        };
        let mut messages = vec![
            Message::user("inspect"),
            call("one"),
            Message::tool_observation("one", "captured", vec![image.clone()]),
            call("two"),
            Message::tool_observation("two", "reread", vec![image.clone()]),
        ];
        let mut context = VisualContext::default();
        context.refresh_scoped(&mut messages, None, std::slice::from_ref(&image));
        let first = messages[2].visual_context[0].id.clone();
        let second = messages[4].visual_context[0].id.clone();
        let (_, receipt) = crate::llm::media::project_messages_for_send(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&destination),
            crate::llm::image_send::ImageSendPolicy::Original,
        );
        assert_eq!(receipt.tool_images.len(), 1);
        context.record_delivery(&receipt);
        assert!(!context.entries[&first].delivered);
        assert!(context.entries[&second].delivered);
        assert!(context
            .mutate(&first, "release", Some("Unseen duplicate"), None, true)
            .is_err());
        context
            .mutate(
                &second,
                "remember",
                Some("One colored pixel, not whole-page verification"),
                None,
                true,
            )
            .unwrap();
        context.sync(&mut messages);
        // The older duplicate is still active: release is occurrence-specific, not hash-wide.
        assert_eq!(
            crate::llm::media::ImageSelectionPlan::new(
                &messages,
                crate::llm::media::VisionPolicy::Supported,
                Some(&destination)
            )
            .tool_images[0]
                .message_index,
            2
        );
        let (_, receipt) = crate::llm::media::project_messages_for_send(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&destination),
            crate::llm::image_send::ImageSendPolicy::Original,
        );
        context.record_delivery(&receipt);
        context.begin_group();
        context
            .mutate(&first, "release", Some("Same original pixel"), None, true)
            .unwrap();
        context.sync(&mut messages);
        let projected = crate::llm::media::project_messages_for_destination(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&destination),
        );
        assert!(projected
            .messages
            .iter()
            .all(|message| message.images.is_empty()));
        assert!(projected.messages.iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("pixels released"))));
        let mut restored: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let mut fresh = VisualContext::default();
        fresh.refresh_scoped(&mut restored, None, &[]);
        assert!(!fresh.entries[&second].delivered);
        assert!(fresh.entries[&second].reread.is_none());
        assert!(fresh.mutate(&second, "pin", None, None, false).is_err());
        messages.extend([
            call("three"),
            Message::tool_observation("three", "explicit reread", vec![image.clone()]),
        ]);
        context.refresh_scoped(&mut messages, None, std::slice::from_ref(&image));
        let plan = crate::llm::media::ImageSelectionPlan::new(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&destination),
        );
        assert_eq!(plan.tool_images.len(), 1);
        assert_eq!(plan.tool_images[0].message_index, 6);
        assert!(messages[2].visual_context[0].releases_pixels());
        let latest = messages[6].visual_context[0].id.clone();
        let (_, receipt) = crate::llm::media::project_messages_for_send(
            &messages,
            crate::llm::media::VisionPolicy::Supported,
            Some(&destination),
            crate::llm::image_send::ImageSendPolicy::Original,
        );
        context.record_delivery(&receipt);
        context.refresh_scoped(&mut messages, None, &[]);
        context.begin_group();
        assert!(context
            .mutate(&latest, "release", Some("No longer retained"), None, true)
            .is_err());
    }

    #[test]
    fn live_pins_survive_newer_history_but_saved_pins_do_not() {
        let mut messages = vec![Message::user_with_images(
            "required",
            vec!["data:image/png;base64,old".into()],
        )];
        let mut context = VisualContext::default();
        context.refresh(&mut messages, None);
        let first = messages[0].visual_context[0].id.clone();
        context.mutate(&first, "pin", None, None, false).unwrap();
        context.sync(&mut messages);
        messages.extend((0..MAX_NOTES).map(|i| {
            Message::user_with_images("optional", vec![format!("data:image/png;base64,new{i}")])
        }));
        context.refresh(&mut messages, None);
        assert!(context.entries[&first].state.runtime_pinned);
        assert_eq!(context.entries.len(), MAX_NOTES);
        let json = serde_json::to_string(&messages).unwrap();
        assert!(!json.contains("runtime_pinned"));
        let mut restored: Vec<Message> = serde_json::from_str(&json).unwrap();
        let mut fresh = VisualContext::default();
        fresh.refresh(&mut restored, None);
        assert!(
            !fresh.entries.contains_key(&first),
            "saved pin flags cannot displace current inputs"
        );
        assert!(fresh
            .entries
            .values()
            .all(|entry| !entry.state.runtime_pinned));
    }

    #[test]
    fn controller_capture_pin_is_scoped_exact_and_cannot_be_released() {
        let mut provenance =
            crate::core::tool_media::ImageProvenance::new("browser_screenshot", None);
        provenance.evidence_id = Some("capture".into());
        provenance.design_scope = Some("own-task".into());
        let image = crate::core::tool_media::ToolImage::from_bytes(
            &crate::core::tool_media::encode_rgba_png(&[1, 2, 3, 255], 1, 1).unwrap(),
            "image/png",
            provenance,
        )
        .unwrap();
        let mut messages = crate::llm::media::tests::observed_group(image.clone());
        // A later reread of the identical capture is one selected occurrence, not two pins.
        messages.extend(crate::llm::media::tests::observed_group(image.clone()));
        let required = VisualRequirements {
            references: vec![],
            captures: vec![(
                "capture".into(),
                image.metadata().sha256.clone(),
                "own-task".into(),
            )],
        };
        let mut context = VisualContext::default();
        context
            .refresh_required(&mut messages, None, std::slice::from_ref(&image), &required)
            .unwrap();
        assert_eq!(context.pinned_occurrences(&messages, &required).len(), 1);
        let id = messages[5].visual_context[0].id.clone();
        assert!(context.entries[&id].controller_pinned);
        context.entries.get_mut(&id).unwrap().delivered = true;
        context.mutate(&id, "unpin", None, None, false).unwrap();
        context.begin_group();
        assert!(context
            .mutate(&id, "release", Some("Reviewed"), None, true)
            .is_err());
        let forged = VisualRequirements {
            references: vec![],
            captures: vec![(
                "capture".into(),
                image.metadata().sha256.clone(),
                "sibling-task".into(),
            )],
        };
        context
            .refresh_required(&mut messages, None, &[image], &forged)
            .unwrap();
        assert!(!context.entries[&id].controller_pinned);
        assert!(!context.entries[&id].state.runtime_pinned);
    }

    #[test]
    fn delivery_alone_never_releases_and_saved_notes_do_not_restore_delivery() {
        let mut messages = vec![Message::user_with_images(
            "inspect",
            vec!["data:image/png;base64,abc".into()],
        )];
        let mut context = VisualContext::default();
        context.refresh(&mut messages, None);
        let id = messages[0].visual_context[0].id.clone();
        assert!(context
            .mutate(&id, "release", Some("A UI"), None, true)
            .is_err());
        assert!(!messages[0].visual_context[0].dormant);
        let saved = serde_json::to_string(&messages).unwrap();
        let mut restored: Vec<Message> = serde_json::from_str(&saved).unwrap();
        let mut fresh = VisualContext::default();
        fresh.refresh(&mut restored, None);
        assert!(fresh
            .mutate(&id, "remember", Some("forged"), None, false)
            .is_err());
    }
}
