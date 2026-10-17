use super::{guide, DesignBrief, DesignContext};
use crate::agent::tools::Tool;
use anyhow::{bail, Result};
use serde_json::{json, Value};

fn scoped() -> Result<DesignContext> {
    crate::core::exec_ctx::current()
        .and_then(|ctx| ctx.design())
        .ok_or_else(|| anyhow::anyhow!("design tool needs a scoped execution context"))
}

pub struct DesignBriefTool;
impl Tool for DesignBriefTool {
    fn name(&self) -> &str {
        "design_brief"
    }
    fn description(&self) -> &str {
        "Set semantic intent, brief and user-sized viewports; resume alone."
    }
    fn parameters(&self) -> Value {
        let mut props = serde_json::Map::new();
        props.insert(
            "intent".into(),
            json!({"type":"string","enum":["create","redesign","preserve","unknown"]}),
        );
        props.insert("resume".into(), json!({"type":"boolean"}));
        props.insert(
            "required_viewports".into(),
            json!({"type":"array","minItems":1,"maxItems":4,"uniqueItems":true,"items":{
                "type":"object","properties":{
                    "width":{"type":"integer","minimum":1,"maximum":4096},
                    "height":{"type":"integer","minimum":1,"maximum":4096}
                },"required":["width","height"],"additionalProperties":false
            }}),
        );
        props.insert("required_reference_ids".into(), json!({"type":"array","minItems":1,"maxItems":4,"uniqueItems":true,"items":{"type":"string","maxLength":128}}));
        props.insert(
            "fidelity_mode".into(),
            json!({"type":"string","enum":["measure","exact","near_match"]}),
        );
        for name in [
            "audience",
            "brief",
            "type",
            "palette",
            "layout",
            "assets",
            "responsive",
            "interactions",
            "acceptance",
        ] {
            props.insert(name.into(), json!({"type":"string","maxLength":512}));
        }
        json!({"type":"object","properties":props,"additionalProperties":false})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        execute_brief(&scoped()?, args)
    }
}

pub fn execute_brief(ctx: &DesignContext, args: &Value) -> Result<String> {
    let Some(mut object) = args.as_object().cloned() else {
        bail!("design_brief requires an object");
    };
    let resume = match object.remove("resume") {
        None => false,
        Some(Value::Bool(resume)) => resume,
        Some(_) => bail!("resume must be a boolean"),
    };
    if resume {
        if !object.is_empty() {
            bail!("resume=true takes no replacement brief fields");
        }
        ctx.apply_brief(DesignBrief::default(), true)?;
    } else {
        if !object.contains_key("intent") {
            bail!("design_brief needs semantic intent: create, redesign, preserve or unknown");
        }
        if object.get("required_viewports").is_some_and(Value::is_null) {
            bail!("required_viewports must be an array of one to four sizes, not null");
        }
        let brief: DesignBrief = serde_json::from_value(Value::Object(object))?;
        ctx.apply_brief(brief, false)?;
    }
    Ok(format!(
        "{}\n{}",
        ctx.status(),
        ctx.capsule().unwrap_or_else(|| {
            "Automatic design guidance is off; manual design tools remain available.".into()
        })
    ))
}

pub struct DesignGuideTool;
impl Tool for DesignGuideTool {
    fn name(&self) -> &str {
        "design_guide"
    }
    fn description(&self) -> &str {
        "Read a design guide; preserve repo conventions."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"topic":{"type":"string","enum":["general","layout","type","color","assets","responsive","interactions","accessibility","review","graphics","dataviz"]}},"required":["topic"],"additionalProperties":false})
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let Some(object) = args.as_object() else {
            bail!("design_guide requires an object");
        };
        if object.len() != 1 {
            bail!("design_guide takes only topic");
        }
        let topic = args.get("topic").and_then(Value::as_str).unwrap_or("");
        Ok(guide::get(topic)
            .ok_or_else(|| anyhow::anyhow!("unknown design guide topic"))?
            .to_string())
    }
}

pub struct DesignReviewTool;
impl Tool for DesignReviewTool {
    fn name(&self) -> &str {
        "design_review"
    }
    fn description(&self) -> &str {
        "Critique only shown, ready, current-render evidence; no waiver."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"evidence_id":{"type":"string","maxLength":128},"passed":{"type":"boolean"},"note":{"type":"string","maxLength":512}},"required":["evidence_id","passed","note"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Review {
            evidence_id: String,
            passed: bool,
            note: String,
        }
        let review: Review = serde_json::from_value(args.clone())?;
        let ctx = scoped()?;
        ctx.record_review(&review.evidence_id, review.passed, &review.note)?;
        Ok(format!("design review recorded: {:?}", ctx.gate()))
    }
}

/// Reference lookup is granted, output lookup is scoped; neither tool accepts a path fallback.
pub struct ReferenceCompareTool;
impl Tool for ReferenceCompareTool {
    fn name(&self) -> &str {
        "reference_compare"
    }
    fn description(&self) -> &str {
        "Measure granted original vs current scoped output; no alignment/tolerance changes."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"reference_id":{"type":"string","maxLength":128},"evidence_id":{"type":"string","maxLength":128},"diagnostics":{"type":"boolean","default":false,"description":"Diff/overlay pixels; separate outbound image approval and vision required."}},"required":["reference_id","evidence_id"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn may_return_images(&self, args: &Value) -> bool {
        args.get("diagnostics").and_then(Value::as_bool) == Some(true)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        Ok(self.execute_observation(args)?.text)
    }
    fn execute_observation(&self, args: &Value) -> Result<crate::agent::tools::ToolObservation> {
        execute_compare_with(&scoped()?, args, |_ctx, id| {
            crate::core::exec_ctx::current()
                .and_then(|ctx| ctx.scoped_tool_image(id))
                .ok_or_else(|| {
                    anyhow::anyhow!("scoped live output capture unavailable in this execution")
                })
        })
    }
}

/// The guard is acquired before the byte reader is invoked. Fields drop buffers before owner.
type AdmittedPair = super::reference_compare::EncodedInputs;
fn admitted_pair_bytes(
    reference: &crate::core::tool_media::ToolImage,
    output: &crate::core::tool_media::ToolImage,
    mut read: impl FnMut(&crate::core::tool_media::ToolImage) -> Result<Vec<u8>>,
) -> Result<AdmittedPair> {
    let a = reference.metadata().byte_len;
    let b = output.metadata().byte_len;
    if a > crate::core::tool_media::MAX_IMAGE_BYTES || b > crate::core::tool_media::MAX_IMAGE_BYTES
    {
        bail!("encoded image exceeds budget");
    }
    let budget = super::reference_compare::WorkBudget::acquire(
        a.checked_add(b)
            .ok_or_else(|| anyhow::anyhow!("encoded size overflow"))?,
    )?;
    let reference = read(reference)?;
    let output = read(output)?;
    if reference.len() != a || output.len() != b {
        bail!("encoded bytes differ from frozen lengths");
    }
    Ok(AdmittedPair {
        reference,
        output,
        budget: Some(budget),
    })
}
struct AdmittedSingle {
    bytes: Vec<u8>,
    budget: super::reference_compare::WorkBudget,
}
fn admitted_single_bytes(
    image: &crate::core::tool_media::ToolImage,
    read: impl FnOnce(&crate::core::tool_media::ToolImage) -> Result<Vec<u8>>,
) -> Result<AdmittedSingle> {
    let size = image.metadata().byte_len;
    if size > crate::core::tool_media::MAX_IMAGE_BYTES {
        bail!("encoded image exceeds budget");
    }
    let budget = super::reference_compare::WorkBudget::acquire(size)?;
    let bytes = read(image)?;
    if bytes.len() != size {
        bail!("encoded bytes differ from frozen length");
    }
    Ok(AdmittedSingle { bytes, budget })
}

/// A controller may supply a child-local bounded output lookup, retaining the same strict checks.
pub fn execute_compare_with(
    ctx: &DesignContext,
    args: &Value,
    lookup_output: impl FnOnce(&DesignContext, &str) -> Result<crate::core::tool_media::ToolImage>,
) -> Result<crate::agent::tools::ToolObservation> {
    use super::reference_compare::{compare_owned, ComparisonBinding};
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        reference_id: String,
        evidence_id: String,
        #[serde(default)]
        diagnostics: bool,
    }
    let args: Args = serde_json::from_value(args.clone())?;
    let refs = crate::core::exec_ctx::current()
        .and_then(|e| e.references())
        .ok_or_else(|| {
            anyhow::anyhow!("reference comparison needs a live scoped reference grant")
        })?;
    let reference = refs.original(&args.reference_id)?;
    let output = lookup_output(ctx, &args.evidence_id)?;
    let snapshot = ctx.snapshot();
    let evidence = snapshot
        .evidence
        .iter()
        .find(|e| {
            e.id == args.evidence_id
                && e.owner_id == snapshot.owner_id
                && e.task_id == snapshot.task_id
        })
        .ok_or_else(|| anyhow::anyhow!("not a current implementation capture"))?;
    let required = snapshot
        .references
        .iter()
        .find(|r| r.reference_id == args.reference_id)
        .ok_or_else(|| anyhow::anyhow!("reference is not selected in this design contract"))?;
    if required.original_sha256.as_deref() != Some(reference.metadata().sha256.as_str()) {
        bail!("original does not match selected design reference hash");
    }
    if evidence.status == super::EvidenceStatus::Stale
        || !evidence.ready
        || evidence.render_revision != snapshot.render_revision
        || evidence.source_hash != snapshot.source_hash
        || snapshot.target.as_deref() != Some(evidence.target.as_str())
    {
        bail!("comparison output capture/source is stale or not ready");
    }
    let provenance = &output.metadata().provenance;
    if provenance.evidence_id.as_deref() != Some(args.evidence_id.as_str())
        || provenance.design_scope.as_deref() != Some(ctx.evidence_scope().as_str())
        || provenance.source != "browser_screenshot"
        || provenance.crop.is_some()
        || provenance.parent_sha256.is_some()
    {
        bail!("reference/derived image cannot substitute for whole implementation screenshot");
    }
    let binding = ComparisonBinding {
        reference_id: args.reference_id,
        reference_sha256: reference.metadata().sha256.clone(),
        output_evidence_id: args.evidence_id,
        output_sha256: output.metadata().sha256.clone(),
        source_hash: evidence
            .source_hash
            .clone()
            .ok_or_else(|| anyhow::anyhow!("output current-source provenance unavailable"))?,
        render_revision: evidence.render_revision,
        target: evidence.target.clone(),
        viewport: evidence.viewport,
        dpr_milli: evidence
            .capture_identity
            .as_ref()
            .map_or(1000, |i| i.dpr_milli),
        // Missing runtime identity remains explicitly unverified and blocks fidelity completion.
        font_identity: evidence
            .capture_identity
            .as_ref()
            .map_or_else(|| "unverified".into(), |i| i.font_identity.clone()),
        frame_state: evidence
            .capture_identity
            .as_ref()
            .map_or_else(|| "unverified".into(), |i| i.frame_state.clone()),
        request: snapshot
            .comparison_request
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no frozen comparison requirements"))?,
    };
    ctx.reserve_visual_operation()?;
    let mut inputs = admitted_pair_bytes(&reference, &output, |image| image.bytes())?;
    let mut result = compare_owned(
        &mut inputs,
        &reference.metadata().mime,
        &output.metadata().mime,
        binding,
    )?;
    let text = serde_json::to_string(
        &json!({"metrics":result.receipt.metrics(),"accepted":result.receipt.accepted(),"note":result.artifact_note,"capture_identity_verified":evidence.capture_identity.as_ref().is_some_and(|i| i.output_sha256 == output.metadata().sha256),"scope":"whole-frame pixels only; not functionality/accessibility/mobile proof; missing runtime-observed platform-face/repeated-frame identity blocks fidelity completion"}),
    )?;
    ctx.record_comparison(result.receipt.clone())?;
    let images = if args.diagnostics {
        result.diagnostic_images()?
    } else {
        Vec::new()
    };
    Ok(crate::agent::tools::ToolObservation { text, images })
}

pub struct ReferenceTraceTool;
impl Tool for ReferenceTraceTool {
    fn name(&self) -> &str {
        "reference_trace"
    }
    fn description(&self) -> &str {
        "Trace explicit granted crop by exact alpha/color/mask criteria; no semantic segmentation."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"reference_id":{"type":"string","maxLength":128},"crop":{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"],"additionalProperties":false},"foreground":{"oneOf":[{"type":"object","properties":{"kind":{"type":"string","enum":["alpha"]},"min":{"type":"integer","minimum":0,"maximum":255}},"required":["kind","min"],"additionalProperties":false},{"type":"object","properties":{"kind":{"type":"string","enum":["color"]},"rgba":{"type":"array","minItems":4,"maxItems":4,"items":{"type":"integer","minimum":0,"maximum":255}},"max_channel_delta":{"type":"integer","minimum":0,"maximum":255}},"required":["kind","rgba","max_channel_delta"],"additionalProperties":false},{"type":"object","properties":{"kind":{"type":"string","enum":["mask"]},"pixels":{"type":"array","maxItems":1048576,"items":{"type":"boolean"}}},"required":["kind","pixels"],"additionalProperties":false}]}},"required":["reference_id","crop","foreground"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            reference_id: String,
            crop: crate::core::tool_media::PixelRect,
            foreground: super::reference_trace::Foreground,
        }
        let args: Args = serde_json::from_value(args.clone())?;
        let refs = crate::core::exec_ctx::current()
            .and_then(|e| e.references())
            .ok_or_else(|| anyhow::anyhow!("trace needs a live scoped reference grant"))?;
        if u64::from(args.crop.width) * u64::from(args.crop.height)
            > super::reference_trace::MAX_TRACE_PIXELS as u64
        {
            bail!("trace crop exceeds pixel budget");
        }
        let image = refs.original(&args.reference_id)?;
        scoped()?.reserve_visual_operation()?;
        let mut input = admitted_single_bytes(&image, |image| image.bytes())?;
        let size =
            super::reference_compare::encoded_rgba_size(&input.bytes, &image.metadata().mime)?;
        input.budget.ensure_total(
            2 * size
                + crate::core::tool_media::MAX_PNG_DECODE_BYTES
                + input.bytes.len()
                + 16 * 1024 * 1024,
        )?;
        let rgba =
            super::reference_compare::decode_supported(&input.bytes, &image.metadata().mime)?;
        let trace = super::reference_trace::trace_admitted(
            &rgba,
            &super::reference_trace::TraceRequest {
                crop: args.crop,
                foreground: args.foreground,
            },
        )?;
        Ok(serde_json::to_string(&trace)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionDef, ToolDef};
    fn exhaust_image_memory() -> crate::llm::image_send::ImageMemoryLease {
        use crate::llm::image_send::{try_image_memory_lease, IMAGE_MEMORY_BUDGET};
        let mut lease = try_image_memory_lease(0).unwrap();
        let mut step = IMAGE_MEMORY_BUDGET.next_power_of_two();
        while step > 0 {
            lease.try_grow(step);
            step /= 2;
        }
        lease
    }

    #[test]
    fn compare_and_trace_exhaustion_precedes_encoded_byte_readers() {
        let _lock = crate::llm::image_send::image_test_lock();
        let image = crate::core::tool_media::ToolImage::from_bytes(
            b"GIF89a-fixture",
            "image/gif",
            crate::core::tool_media::ImageProvenance::new("fixture", None),
        )
        .unwrap();
        let exhausted = exhaust_image_memory();
        let mut calls = 0;
        assert!(admitted_pair_bytes(&image, &image, |image| {
            calls += 1;
            image.bytes()
        })
        .is_err());
        assert!(admitted_single_bytes(&image, |image| {
            calls += 1;
            image.bytes()
        })
        .is_err());
        assert_eq!(calls, 0, "rejected admission must precede any encoded copy");
        drop(exhausted);
        let pair = admitted_pair_bytes(&image, &image, |image| image.bytes()).unwrap();
        assert_eq!(pair.reference.len(), image.metadata().byte_len);
        assert_eq!(pair.output.len(), image.metadata().byte_len);
        drop(pair);
        let single = admitted_single_bytes(&image, |image| image.bytes()).unwrap();
        assert_eq!(single.bytes.len(), image.metadata().byte_len);
    }

    #[test]
    fn admitted_encoded_reader_failure_releases_owner() {
        let _lock = crate::llm::image_send::image_test_lock();
        let image = crate::core::tool_media::ToolImage::from_bytes(
            b"GIF89a-fixture",
            "image/gif",
            crate::core::tool_media::ImageProvenance::new("fixture", None),
        )
        .unwrap();
        let mut calls = 0;
        assert!(admitted_pair_bytes(&image, &image, |image| {
            calls += 1;
            if calls == 2 {
                bail!("fixture reader error");
            }
            image.bytes()
        })
        .is_err());
        assert_eq!(calls, 2);
        let exhausted = exhaust_image_memory();
        assert!(crate::llm::image_send::try_image_memory_lease(1).is_none());
        drop(exhausted);
        assert!(admitted_single_bytes(&image, |_| bail!("fixture reader error")).is_err());
        assert!(admitted_single_bytes(&image, |image| image.bytes()).is_ok());
    }

    #[test]
    fn guide_topics_include_graphics_and_dataviz_with_bounded_native_schema() {
        let tool = DesignGuideTool;
        let schema = tool.parameters();
        for topic in schema["properties"]["topic"]["enum"].as_array().unwrap() {
            let topic = topic.as_str().unwrap();
            assert_eq!(
                tool.execute(&json!({"topic": topic})).unwrap(),
                guide::get(topic).unwrap()
            );
        }
        for topic in ["graphics", "dataviz"] {
            assert!(schema["properties"]["topic"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!(topic)));
        }
        assert!(tool.execute(&json!({"topic": "unknown"})).is_err());
        assert!(tool
            .execute(&json!({"topic": "graphics", "extra": true}))
            .is_err());
        let def = ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: tool.name().into(),
                description: tool.description().into(),
                parameters: schema,
            },
            cache_control: None,
        };
        let bytes = serde_json::to_vec(&def).unwrap().len();
        assert!(
            bytes <= 1400,
            "design_guide native definition {bytes} bytes"
        );
    }

    #[test]
    fn brief_native_schema_is_bounded() {
        let tool = DesignBriefTool;
        let def = ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: tool.name().into(),
                description: tool.description().into(),
                parameters: tool.parameters(),
            },
            cache_control: None,
        };
        let bytes = serde_json::to_vec(&def).unwrap().len();
        assert!(
            bytes <= 1800,
            "design_brief native definition {bytes} bytes"
        );
    }
    fn visual_brief() -> Value {
        json!({"intent":"preserve","brief":"Keep the existing layout","acceptance":"No overflow at the requested sizes"})
    }

    fn active_context() -> DesignContext {
        let ctx = DesignContext::new("owner", "workspace", super::super::DesignMode::Auto);
        ctx.begin_turn("task", "workspace");
        let mut args = visual_brief();
        args["required_viewports"] = json!([{"width":800,"height":600}]);
        execute_brief(&ctx, &args).unwrap();
        ctx.reserve_visual_operation().unwrap();
        ctx.begin_repair().unwrap();
        ctx
    }

    #[test]
    fn required_viewport_wrong_types_duplicates_and_bounds_are_atomic() {
        let ctx = active_context();
        let before = serde_json::to_value(ctx.snapshot()).unwrap();
        for invalid in [
            Value::Null,
            json!("800x600"),
            json!({"width":800,"height":600}),
            json!([]),
            json!([null]),
            json!([{"width":"800","height":600}]),
            json!([{"width":800.5,"height":600}]),
            json!([{"width":true,"height":600}]),
            json!([{"width":800}]),
            json!([{"width":800,"height":600,"passed":true}]),
            json!([{"width":0,"height":600}]),
            json!([{"width":800,"height":-1}]),
            json!([{"width":4097,"height":1}]),
            json!([{"width":4096,"height":2049}]),
            json!([{"width":800,"height":600},{"height":600,"width":800}]),
            json!([
                {"width":100,"height":100},{"width":200,"height":100},
                {"width":300,"height":100},{"width":400,"height":100},
                {"width":500,"height":100}
            ]),
        ] {
            let mut args = visual_brief();
            args["brief"] = json!("Must not replace the existing contract");
            args["required_viewports"] = invalid.clone();
            assert!(execute_brief(&ctx, &args).is_err(), "accepted {invalid}");
            assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
        }
        let mut invalid_brief = visual_brief();
        invalid_brief["required_viewports"] = json!([{"width":1,"height":1}]);
        invalid_brief["acceptance"] = json!("");
        assert!(execute_brief(&ctx, &invalid_brief).is_err());
        assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
        invalid_brief["acceptance"] = json!("Valid acceptance");
        invalid_brief["intent"] = json!("unknown");
        assert!(execute_brief(&ctx, &invalid_brief).is_err());
        assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
        let mut waiver = visual_brief();
        waiver["required_viewports"] = json!([{"width":1,"height":1}]);
        waiver["skipped_by_user"] = json!("model waiver");
        assert!(execute_brief(&ctx, &waiver).is_err());
        assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
    }

    #[test]
    fn explicit_required_viewports_drive_exact_visual_gate_without_budget_reset() {
        use super::super::{GateDecision, Viewport};
        let ctx = active_context();
        assert_eq!(
            ctx.snapshot().required_viewports,
            vec![Viewport {
                width: 800,
                height: 600
            }]
        );
        assert_eq!(ctx.snapshot().visual_operations, 1);
        assert_eq!(ctx.snapshot().repairs, 1);
        ctx.set_target("target").unwrap();
        ctx.set_source_revision("source").unwrap();
        for (id, width, height) in [("wrong", 800, 601), ("requested", 800, 600)] {
            ctx.record_capture(
                id,
                Some("source"),
                "target",
                Viewport { width, height },
                ctx.current_revision(),
                true,
                "settled current source",
            )
            .unwrap();
            ctx.mark_projected(id).unwrap();
            ctx.record_review(id, true, "Observed no overflow in the shown viewport")
                .unwrap();
            if id == "wrong" {
                assert!(ctx.gate().blocks_completion());
            }
        }
        assert_eq!(ctx.gate(), GateDecision::Verified);
        execute_brief(&ctx, &visual_brief()).unwrap();
        assert_eq!(
            ctx.snapshot().required_viewports,
            vec![Viewport {
                width: 800,
                height: 600
            }]
        );
        assert_eq!(
            ctx.brief().unwrap().required_viewports,
            Some(ctx.snapshot().required_viewports)
        );
        assert_eq!(ctx.snapshot().visual_operations, 1);
        assert_eq!(ctx.snapshot().repairs, 1);
        assert_eq!(ctx.snapshot().captures_this_pass, 2);
        assert!(
            ctx.gate().blocks_completion(),
            "updated brief invalidates old review"
        );
        let mut args = visual_brief();
        args["required_viewports"] = json!([
            {"width":1,"height":1},{"width":4096,"height":2048},
            {"width":2048,"height":4096},{"width":390,"height":844}
        ]);
        execute_brief(&ctx, &args).unwrap();
        assert_eq!(ctx.snapshot().required_viewports.len(), 4);
        assert_eq!(ctx.snapshot().visual_operations, 1);
        assert!(ctx.capsule().unwrap().contains("4096x2048"));
    }

    #[test]
    fn required_viewports_resume_only_and_persist_with_prior_brief() {
        use super::super::Viewport;
        let ctx = active_context();
        let before = serde_json::to_value(ctx.snapshot()).unwrap();
        for args in [
            json!({"resume":true,"required_viewports":[{"width":390,"height":844}]}),
            json!({"resume":true,"required_viewports":null}),
            json!({"resume":true,"intent":"preserve"}),
        ] {
            assert!(execute_brief(&ctx, &args).is_err());
            assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
        }
        let attempted_replacement = DesignBrief {
            required_viewports: Some(vec![Viewport {
                width: 390,
                height: 844,
            }]),
            ..Default::default()
        };
        assert!(ctx.apply_brief(attempted_replacement, true).is_err());
        assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
        execute_brief(&ctx, &json!({"resume":true})).unwrap();
        assert_eq!(ctx.snapshot().visual_operations, 1);
        ctx.finalize();
        ctx.begin_turn("later", "workspace");
        assert!(ctx.snapshot().required_viewports.is_empty());
        assert!(!ctx.snapshot().active);
        execute_brief(&ctx, &json!({"resume":true})).unwrap();
        assert_eq!(
            ctx.snapshot().required_viewports,
            vec![Viewport {
                width: 800,
                height: 600
            }]
        );
        assert_eq!(ctx.snapshot().visual_operations, 0);
        let encoded = serde_json::to_value(ctx.brief().unwrap()).unwrap();
        assert_eq!(
            encoded["required_viewports"],
            json!([{"width":800,"height":600}])
        );
        let restored = DesignContext::new("owner", "workspace", super::super::DesignMode::Auto);
        restored.restore(ctx.snapshot()).unwrap();
        execute_brief(&restored, &json!({"resume":true})).unwrap();
        assert_eq!(
            restored.snapshot().required_viewports,
            ctx.snapshot().required_viewports
        );
    }

    #[test]
    fn controller_viewports_are_validated_before_any_mutation_and_legacy_briefs_still_work() {
        use super::super::Viewport;
        let ctx = active_context();
        let before = serde_json::to_value(ctx.snapshot()).unwrap();
        for viewports in [
            vec![],
            vec![
                Viewport {
                    width: 1,
                    height: 1
                };
                2
            ],
            vec![Viewport {
                width: 4096,
                height: 4096,
            }],
        ] {
            assert!(ctx.require_viewports(viewports).is_err());
            assert_eq!(serde_json::to_value(ctx.snapshot()).unwrap(), before);
        }
        ctx.require_viewports(vec![Viewport {
            width: 1024,
            height: 768,
        }])
        .unwrap();
        ctx.begin_turn("resume-controller", "workspace");
        execute_brief(&ctx, &json!({"resume":true})).unwrap();
        assert_eq!(
            ctx.snapshot().required_viewports,
            vec![Viewport {
                width: 1024,
                height: 768
            }]
        );
        let legacy: DesignBrief = serde_json::from_value(visual_brief()).unwrap();
        assert!(legacy.required_viewports.is_none());
        let fresh = DesignContext::default();
        execute_brief(&fresh, &visual_brief()).unwrap();
        assert!(
            fresh.snapshot().required_viewports.is_empty(),
            "default narrow/wide unchanged"
        );
    }

    #[test]
    fn brief_validation_does_not_guess_intent_from_words() {
        let ctx = DesignContext::default();
        assert!(execute_brief(&ctx, &json!({"brief":"build a frontend"})).is_err());
        execute_brief(
            &ctx,
            &json!({"intent":"unknown","brief":"UI words, backend migration only"}),
        )
        .unwrap();
        assert_eq!(ctx.gate(), super::super::GateDecision::NotRequired);
        assert!(execute_brief(&ctx, &json!({"intent":"create","brief":"page"})).is_err());
        assert!(execute_brief(
            &ctx,
            &json!({"intent":"create","brief":"page","acceptance":"visible title","projected":true})
        )
        .is_err());
    }
}
