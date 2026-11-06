//! Controller-only saved attachment selection. An ID names a loaded occurrence, never a path/grant.

use crate::core::{session_store, tool_media, types::Message, visual_context::VisualNote};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(clap::Args, Debug)]
pub(crate) struct ImagesArgs {
    /// Exact saved-session slug, not a path.
    pub(crate) slug: String,
    /// Emit bounded descriptors only; no pixels or local filenames.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Serialize)]
pub(crate) struct SavedImage {
    id: String,
    kind: &'static str,
    state: &'static str,
    available: bool,
    width: Option<u32>,
    height: Option<u32>,
    note: String,
    uncertainty: String,
}

/// Ignore the replaceable prompt prefix, but keep duplicate conversation occurrences distinct.
fn occurrence_base(history: &[Message]) -> usize {
    history
        .iter()
        .take_while(|message| {
            message.role == "system"
                && !message.content.as_deref().is_some_and(|content| {
                    content.starts_with(crate::agent::compact::COMPACT_MARKER_PREFIX)
                        || content.starts_with(crate::agent::compact::HANDOFF_MARKER_PREFIX)
                })
                && message.images.is_empty()
                && message.tool_images.is_empty()
        })
        .count()
}

fn saved_id(message: &Message, mi: usize, ii: usize) -> String {
    if let Some(state) = message
        .visual_context
        .iter()
        .find(|state| !state.tool_image && state.image_index == ii)
    {
        // Missing CAS data preserves the occurrence ID for an honest unavailable descriptor.
        if state.id.starts_with("visual-")
            && state.id.len() <= 128
            && state
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return state.id.clone();
        }
    }
    let digest = tool_media::sha256(message.images[ii].as_bytes());
    format!("visual-legacy-{mi}-{ii}-{}", &digest[..16])
}

fn saved_tool_id(message: &Message, mi: usize, ii: usize) -> String {
    let image = &message.tool_images[ii];
    if let Some(state) = message
        .visual_context
        .iter()
        .find(|state| state.matches_tool(ii, image))
    {
        return state.id.clone();
    }
    let digest = tool_media::sha256(
        &serde_json::to_vec(image.metadata()).expect("image metadata serializes"),
    );
    format!("visual-tool-{mi}-{ii}-{}", &digest[..16])
}

/// Saved capture selection is controller-only. Incomplete tool groups and reference sidecars are
/// never selectable. A typed compact boundary may retain dormant captures without their old group.
fn saved_tools(history: &[Message]) -> BTreeSet<(usize, usize)> {
    let mut complete: BTreeSet<_> = crate::llm::media::completed_tool_image_occurrences(history)
        .into_iter()
        .map(|image| (image.message_index, image.image_index))
        .collect();
    for (mi, message) in history.iter().enumerate() {
        if message.role == "system"
            && message.content.as_deref().is_some_and(|content| {
                content.starts_with(crate::agent::compact::COMPACT_MARKER_PREFIX)
            })
        {
            for (ii, image) in message.tool_images.iter().enumerate() {
                if message
                    .visual_context
                    .iter()
                    .any(|state| state.matches_tool(ii, image) && state.releases_pixels())
                {
                    complete.insert((mi, ii));
                }
            }
        }
    }
    complete.retain(|&(mi, ii)| {
        history[mi].tool_images[ii].metadata().provenance.source
            != crate::core::reference_media::REFERENCE_SOURCE
    });
    complete
}

pub(crate) fn descriptors(history: &[Message]) -> Vec<SavedImage> {
    let mut result = Vec::new();
    let tools = saved_tools(history);
    let base = occurrence_base(history);
    for (mi, message) in history.iter().enumerate().rev() {
        for (ii, image) in message.tool_images.iter().enumerate().rev() {
            if !tools.contains(&(mi, ii)) {
                continue;
            }
            if result.len() >= crate::core::visual_context::MAX_NOTES {
                return result;
            }
            let note = message
                .visual_context
                .iter()
                .find(|state| state.matches_tool(ii, image));
            result.push(SavedImage {
                id: saved_tool_id(message, mi.saturating_sub(base), ii),
                kind: "tool",
                state: if note.is_some_and(VisualNote::releases_pixels) {
                    "dormant"
                } else {
                    "active"
                },
                available: image.is_available(),
                width: image.metadata().width,
                height: image.metadata().height,
                note: note.map(|note| note.note.clone()).unwrap_or_default(),
                uncertainty: note
                    .map(|note| note.uncertainty.clone())
                    .unwrap_or_default(),
            });
        }
        for (ii, url) in message.images.iter().enumerate().rev() {
            if result.len() >= crate::core::visual_context::MAX_NOTES {
                return result;
            }
            let note = message
                .visual_context
                .iter()
                .find(|state| state.matches(ii, url));
            let dimensions = crate::llm::image_send::source_png_dimensions(url);
            result.push(SavedImage {
                id: saved_id(message, mi.saturating_sub(base), ii),
                kind: "attachment",
                state: if note.is_some_and(VisualNote::releases_pixels) {
                    "dormant"
                } else {
                    "active"
                },
                available: tool_media::split_data_url(url)
                    .and_then(|(_, data)| {
                        tool_media::base64_decoded_len(data, tool_media::MAX_IMAGE_BYTES)
                    })
                    .is_ok(),
                width: dimensions.map(|d| d.0),
                height: dimensions.map(|d| d.1),
                note: note.map(|note| note.note.clone()).unwrap_or_default(),
                uncertainty: note
                    .map(|note| note.uncertainty.clone())
                    .unwrap_or_default(),
            });
        }
    }
    result
}

/// Only the trusted CLI/controller calls this with its already loaded canonical session. Model
/// tools cannot resolve saved IDs, hydrate arbitrary blobs or turn serialized notes into consent.
pub(crate) fn select_saved(history: &[Message], ids: &[String]) -> Result<Vec<String>> {
    ensure!(
        ids.len() <= tool_media::MAX_REQUEST_IMAGES,
        "select at most four saved images"
    );
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    enum SavedSource<'a> {
        Attachment(&'a str),
        Tool(&'a tool_media::ToolImage),
    }
    let tools = saved_tools(history);
    let base = occurrence_base(history);
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for id in ids {
        ensure!(seen.insert(id), "saved image IDs must be unique");
        ensure!(
            id.len() <= 128
                && id.starts_with("visual-")
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid saved image ID"
        );
        let mut matches = Vec::new();
        for (mi, message) in history.iter().enumerate() {
            for (ii, url) in message.images.iter().enumerate() {
                if saved_id(message, mi.saturating_sub(base), ii) == *id {
                    matches.push(SavedSource::Attachment(url));
                }
            }
            for (ii, image) in message.tool_images.iter().enumerate() {
                if tools.contains(&(mi, ii))
                    && saved_tool_id(message, mi.saturating_sub(base), ii) == *id
                {
                    matches.push(SavedSource::Tool(image));
                }
            }
        }
        ensure!(
            matches.len() == 1,
            "saved image unavailable or ambiguous in this session; list and reselect current IDs"
        );
        let url = match matches[0] {
            SavedSource::Attachment(url) => url.to_owned(),
            SavedSource::Tool(image) => {
                ensure!(
                    image.is_available(),
                    "saved capture original is unavailable; capture or attach it again"
                );
                // Only pixels become a fresh user selection. Saved approval, capture identity,
                // design scope, notes and acceptance credit never cross this boundary.
                image.data_url()
            }
        };
        let (_, data) = tool_media::split_data_url(&url)
            .context("saved image original is unavailable; attach it again")?;
        tool_media::base64_decoded_len(data, tool_media::MAX_IMAGE_BYTES)?;
        selected.push(url);
    }
    Ok(selected)
}

pub(crate) fn run(args: ImagesArgs) -> Result<()> {
    let ((history, _, _), _) = session_store::read_session_for_edit(&args.slug)?;
    let images = descriptors(&history);
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"images": images}))?
        );
    } else {
        for image in images {
            println!(
                "{}  {}  {}",
                image.id,
                image.state,
                if image.available {
                    "available; reselect with --resume SLUG --image-id ID"
                } else {
                    "unavailable; attach the original again"
                }
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(value: u8) -> tool_media::ToolImage {
        let mut provenance = tool_media::ImageProvenance::new("browser_screenshot", None);
        provenance.evidence_id = Some(format!("capture-{value}"));
        provenance.design_scope = Some("owned-fixture-scope".into());
        provenance.approved_destination = Some("a".repeat(64));
        tool_media::ToolImage::from_bytes(
            &tool_media::encode_rgba_png(&[value, 0, 0, 255], 1, 1).unwrap(),
            "image/png",
            provenance,
        )
        .unwrap()
    }

    #[test]
    fn saved_selection_is_session_local_and_does_not_change_dormant_originals() {
        let url = crate::llm::media::tests::image(89).data_url();
        let mut message = Message::user_with_images("private source", vec![url.clone()]);
        message.visual_context.push(VisualNote {
            id: "visual-saved-fixture".into(),
            image_index: 0,
            tool_image: false,
            source_sha256: tool_media::sha256(url.as_bytes()),
            note: "a fixture".into(),
            uncertainty: String::new(),
            dormant: true,
            pinned: false,
            runtime_pinned: false,
        });
        let history = vec![message];
        let listed = descriptors(&history);
        assert_eq!(listed[0].id, "visual-saved-fixture");
        assert_eq!(listed[0].state, "dormant");
        assert!(listed[0].available);
        assert!(!serde_json::to_string(&listed).unwrap().contains("base64,"));
        assert_eq!(
            select_saved(&history, &[listed[0].id.clone()]).unwrap(),
            vec![url]
        );
        assert!(history[0].visual_context[0].dormant);
        assert!(select_saved(&[], &[listed[0].id.clone()]).is_err());
        assert!(select_saved(&history, &[listed[0].id.clone(), listed[0].id.clone()]).is_err());
        assert!(select_saved(&history, &["../../private".into()]).is_err());
    }
    #[test]
    fn saved_capture_selection_requires_a_complete_group_and_renews_only_pixels() {
        let image = capture(91);
        let mut history = crate::llm::media::tests::observed_group(image.clone());
        let mut visual = crate::core::visual_context::VisualContext::default();
        visual.refresh_scoped(&mut history, None, std::slice::from_ref(&image));
        let listed = descriptors(&history);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, "tool");
        assert!(listed[0].available);
        assert_eq!(
            select_saved(&history, &[listed[0].id.clone()]).unwrap(),
            vec![image.data_url()]
        );
        let fresh = Message::user_with_images(
            "explicit saved capture selection",
            select_saved(&history, &[listed[0].id.clone()]).unwrap(),
        );
        assert!(fresh.tool_images.is_empty());
        assert!(fresh.visual_context.is_empty());
        assert!(history[1].tool_images[0]
            .metadata()
            .provenance
            .approved_destination
            .is_some());
        let incomplete = vec![history[0].clone(), history[1].clone()];
        assert!(descriptors(&incomplete).is_empty());
        assert!(select_saved(&incomplete, &[listed[0].id.clone()]).is_err());
        history[1].tool_images[0] = image.without_payload(tool_media::ImageUnavailable::Missing);
        let unavailable = descriptors(&history);
        assert_eq!(unavailable[0].id, listed[0].id);
        assert!(!unavailable[0].available);
        assert!(select_saved(&history, &[listed[0].id.clone()]).is_err());
    }

    #[test]
    fn dormant_capture_reselection_survives_compaction_without_restoring_authority() {
        let image = capture(92);
        let mut history = crate::llm::media::tests::observed_group(image.clone());
        let mut visual = crate::core::visual_context::VisualContext::default();
        visual.refresh_scoped(&mut history, None, std::slice::from_ref(&image));
        let id = history[1].visual_context[0].id.clone();
        visual.record_delivery(&crate::llm::media::ImageDeliveryReceipt {
            tool_images: crate::llm::media::completed_tool_image_occurrences(&history),
            ..Default::default()
        });
        visual
            .mutate(
                &id,
                "remember",
                Some("Owned fixture observation"),
                None,
                true,
            )
            .unwrap();
        visual.sync(&mut history);
        let before = serde_json::to_value(descriptors(&history)).unwrap();
        let cut = history.len();
        crate::agent::compact::splice_compacted(&mut history, cut, "fixture summary").unwrap();
        assert_eq!(serde_json::to_value(descriptors(&history)).unwrap(), before);
        assert_eq!(
            select_saved(&history, std::slice::from_ref(&id)).unwrap(),
            vec![image.data_url()]
        );
        let fresh = crate::core::exec_ctx::ExecutionContext::default();
        fresh.refresh_visual(&mut history).unwrap();
        assert!(!fresh.visual().list()[0]["delivered"].as_bool().unwrap());
        assert!(fresh.visual().list()[0]["evidence_id"].is_null());
        history[0].role = "assistant".into();
        assert!(
            descriptors(&history).is_empty(),
            "an arbitrary message cannot impersonate the compact manifest"
        );
    }

    #[test]
    fn legacy_selection_ids_survive_prompt_lane_rewrites_without_merging_duplicates() {
        let image = capture(93);
        let mut history = vec![Message::system("old prompt")];
        history.push(Message::user_with_images("first", vec![image.data_url()]));
        history.extend(crate::llm::media::tests::observed_group(image.clone()));
        history.push(Message::user_with_images(
            "duplicate",
            vec![image.data_url()],
        ));
        let before = descriptors(&history);
        let ids: Vec<_> = before.iter().map(|entry| entry.id.clone()).collect();
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), 3);
        crate::agent::prompt_lanes::splice_prompt_lanes(
            &mut history,
            crate::agent::PromptBundle {
                stable: "new prompt".into(),
                dynamic: "current project".into(),
            },
        );
        assert_eq!(
            descriptors(&history)
                .iter()
                .map(|entry| &entry.id)
                .collect::<Vec<_>>(),
            ids.iter().collect::<Vec<_>>()
        );
        for id in &ids {
            assert_eq!(
                select_saved(&history, std::slice::from_ref(id)).unwrap(),
                vec![image.data_url()]
            );
        }
        history.drain(..2);
        assert_eq!(
            descriptors(&history)
                .iter()
                .map(|entry| &entry.id)
                .collect::<Vec<_>>(),
            ids.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn legacy_ids_are_stable_and_missing_originals_are_not_selectable() {
        let mut history = vec![Message::user_with_images(
            "legacy",
            vec![crate::llm::media::tests::image(90).data_url()],
        )];
        let first = descriptors(&history);
        assert_eq!(descriptors(&history)[0].id, first[0].id);
        assert!(select_saved(&history, &[first[0].id.clone()]).is_ok());
        history[0].images[0].clear();
        assert!(!descriptors(&history)[0].available);
        assert!(select_saved(&history, &[first[0].id.clone()]).is_err());
    }
}
