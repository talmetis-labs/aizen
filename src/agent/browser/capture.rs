//! Viewport-scoped PNG capture with honest page-readiness and source-revision evidence.
use super::*;
use crate::agent::tools::ToolObservation;
use crate::core::tool_media::{ImageProvenance, ToolImage, MAX_IMAGE_BYTES};
use serde::{Deserialize, Serialize};

const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 8 * 1024 * 1024;
const SETTLE_TIME: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Viewport {
    pub width: u32,
    pub height: u32,
    pub mobile: bool,
}
impl Viewport {
    fn from_args(args: &Value) -> Result<Self> {
        let width = dimension(args, "width")?;
        let height = dimension(args, "height")?;
        validate_dimensions(width, height)?;
        let mobile = match args.get("mobile") {
            Some(v) => v.as_bool().context("mobile must be boolean")?,
            None => false,
        };
        if args
            .get("device_pixel_ratio")
            .is_some_and(|v| v.as_f64() != Some(1.0))
        {
            bail!("browser viewport supports device_pixel_ratio=1 only");
        }
        Ok(Self {
            width,
            height,
            mobile,
        })
    }
}
fn dimension(args: &Value, key: &str) -> Result<u32> {
    let n = args
        .get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("{key} must be an integer"))?;
    u32::try_from(n).with_context(|| format!("{key} is out of range"))
}
fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        bail!("viewport/clip must be 1..{MAX_DIMENSION} CSS pixels per side and at most {MAX_PIXELS} pixels");
    }
    Ok(())
}

pub struct BrowserSetViewport;
impl Tool for BrowserSetViewport {
    fn name(&self) -> &str {
        "browser_set_viewport"
    }
    fn description(&self) -> &str {
        "Set the explicitly attached page's CSS viewport (bounded width/height, DPR 1, optional mobile). Approval-gated mutation; never retries. Override is scoped to the attached target, stays in effect until reset:true, and may be retained after cancellation. Does not launch a browser."
    }
    fn parameters(&self) -> Value {
        tool_parameters(json!({"type":"object","properties":{
            "width":{"type":"integer","minimum":1,"maximum":4096},"height":{"type":"integer","minimum":1,"maximum":4096},
            "device_pixel_ratio":{"type":"number","enum":[1]},"mobile":{"type":"boolean"},
            "reset":{"type":"boolean","description":"Explicitly clear CDP device metrics override instead of setting dimensions"}
        },"additionalProperties":false}))
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::External
    }
    fn preview(&self, args: &Value) -> Option<ApprovalPreview> {
        let action = if args.get("reset").and_then(Value::as_bool) == Some(true) {
            "Clear the selected target's device metrics override; no network occurs during this preview.".to_string()
        } else {
            match Viewport::from_args(args) {
                Ok(v) => format!("Set viewport {}×{} CSS px, DPR 1, mobile={}. This override persists until explicitly reset; cancellation may retain it.", v.width, v.height, v.mobile),
                Err(_) => "Invalid viewport arguments; execution will refuse before connecting.".into(),
            }
        };
        browser_preview(self.name(), args, &action)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let reset = match args.get("reset") {
            Some(v) => v.as_bool().context("reset must be boolean")?,
            None => false,
        };
        let viewport = if reset {
            None
        } else {
            Some(Viewport::from_args(args)?)
        };
        let (name, profile) = operation_target(args, None)?;
        block(with_cdp(name, profile, false, |c| {
            Box::pin(async move {
                // Set bookkeeping before the await: an ambiguous command/cancel must not claim restored.
                c.viewport = viewport;
                c.refs.clear();
                if let Some(v) = viewport {
                    c.call(
                        "Emulation.setDeviceMetricsOverride",
                        json!({"width":v.width,"height":v.height,
                    "deviceScaleFactor":1,"mobile":v.mobile}),
                    )
                    .await?;
                } else {
                    c.call("Emulation.clearDeviceMetricsOverride", json!({}))
                        .await?;
                }
                let readiness = settle(c).await?;
                let state = page_state(c).await?;
                Ok(json!({"action":if reset {"viewport_override_cleared"}else{"viewport_set"},
                "viewport":state["viewport"],"readiness":readiness,"freshness":"unknown",
                "state_retained":!reset,"note":"Viewport overrides are not silently restored; reset:true clears the override, not an unknown pre-existing override"}).to_string())
            })
        }))
    }
}

// Read-only page evaluation. Bounds the returned DOM/layout/image slice and strings; no JS globals
// or user profile state are written. CDP's timeout plus the outer deadline bound a wedged renderer.
const PAGE_STATE_JS: &str = r#"(()=>{
 const root=document.documentElement, vv=window.visualViewport;
 const imgs=Array.from(document.images).slice(0,513);
 const sample=Array.from(document.querySelectorAll('body *')).slice(0,128).map(e=>{const r=e.getBoundingClientRect();return [Math.round(r.x*10),Math.round(r.y*10),Math.round(r.width*10),Math.round(r.height*10)]});
 const marker=document.querySelector('meta[name="aizen-build-marker"]')?.content ?? window.__AIZEN_BUILD_MARKER__ ?? null;
 return {url:String(location.href).slice(0,8192),title:String(document.title).slice(0,512),document_token:performance.timeOrigin,
 viewport:{width:window.innerWidth,height:window.innerHeight,device_pixel_ratio:window.devicePixelRatio,page_x:vv?.pageLeft??window.scrollX,page_y:vv?.pageTop??window.scrollY,scale:vv?.scale??1},
 document:document.readyState,fonts:!document.fonts?'unsupported':document.fonts.status,
 images:imgs.length>512?'over_limit':imgs.every(i=>i.complete&&(!i.currentSrc||i.naturalWidth>0))?'ready':'pending_or_failed',
 layout_signature:JSON.stringify([root?.scrollWidth,root?.scrollHeight,...sample]),
 observed_build_marker:typeof marker==='string'?marker.slice(0,256):null};
})()"#;

pub(super) async fn page_state(c: &mut CdpClient) -> Result<Value> {
    let result = c
        .call(
            "Runtime.evaluate",
            json!({"expression":PAGE_STATE_JS,"returnByValue":true,"timeout":5000}),
        )
        .await?;
    if result.get("exceptionDetails").is_some() {
        bail!("page readiness evaluation failed");
    }
    let state = result
        .get("result")
        .and_then(|r| r.get("value"))
        .filter(|v| v.is_object())
        .context("page readiness evaluation returned no object")?
        .clone();
    if serde_json::to_vec(&state)?.len() > 64 * 1024 {
        bail!("page readiness evidence exceeds limit");
    }
    Ok(state)
}

pub(super) async fn settle(c: &mut CdpClient) -> Result<Value> {
    let until = tokio::time::Instant::now() + SETTLE_TIME;
    let mut previous = Value::Null;
    let mut stable = 0u32;
    loop {
        let state = page_state(c).await?;
        if state["layout_signature"] == previous && !previous.is_null() {
            stable += 1;
        } else {
            stable = 0;
        }
        previous = state["layout_signature"].clone();
        let doc = state["document"] == "complete";
        let fonts = state["fonts"] == "loaded";
        let images = state["images"] == "ready";
        let layout = stable >= 2;
        let ready = doc && fonts && images && layout;
        if ready || tokio::time::Instant::now() >= until {
            return Ok(
                json!({"ready":ready,"document":state["document"],"fonts":state["fonts"],
                "images":state["images"],"layout_settled":layout,"bounded_wait_ms":3000,
                "note":"Bounded DOM layout samples; not proof of network idle, stopped animations, or latest source"}),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Clone, Copy)]
struct Clip {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}
fn parse_clip(args: &Value) -> Result<Option<Clip>> {
    let Some(clip) = args.get("clip") else {
        return Ok(None);
    };
    let x = dimension(clip, "x")?;
    let y = dimension(clip, "y")?;
    let width = dimension(clip, "width")?;
    let height = dimension(clip, "height")?;
    validate_dimensions(width, height)?;
    Ok(Some(Clip {
        x,
        y,
        width,
        height,
    }))
}

/// Target identity includes the selected page, not just its human URL. Hash length-delimited JSON
/// fields so aliases/same-URL tabs cannot share responsive evidence; endpoints contain no credentials.
fn evidence_target(profile: &str, endpoint: &str, target_id: &str, page_url: &str) -> String {
    let identity = json!([profile, endpoint, target_id, page_url]).to_string();
    format!(
        "browser:{}",
        crate::core::tool_media::sha256(identity.as_bytes())
    )
}

/// Actual platform-face observations, not CSS family declarations/font-ready guesses.
/// The conservative static subset excludes controls/pseudo content/shadow DOM which this surface
/// cannot exhaustively attribute. Unsupported attribution stays unverified.
const MAX_FONT_DOM_NODES: usize = 2048;
const MAX_FONT_TEXT_PARENTS: usize = 64;
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlatformFaceObservation {
    signature: String,
    text_parents: usize,
}
fn text_parent_nodes(root: &Value) -> Result<Vec<i64>> {
    let mut pending = vec![root];
    let mut visited = 0;
    let mut parents = std::collections::BTreeSet::new();
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > MAX_FONT_DOM_NODES {
            bail!("platform-font DOM node budget exceeded");
        }
        let id = node["nodeId"]
            .as_i64()
            .filter(|n| *n > 0)
            .context("platform-font DOM node ID missing")?;
        if node
            .get("shadowRoots")
            .and_then(Value::as_array)
            .is_some_and(|n| !n.is_empty())
            || node
                .get("pseudoElements")
                .and_then(Value::as_array)
                .is_some_and(|n| !n.is_empty())
            || node.get("contentDocument").is_some()
        {
            bail!("platform-font attribution excludes shadow/pseudo/frame content");
        }
        if let Some(name) = node["nodeName"].as_str() {
            if name == "OL" || name == "UL" || name == "LI" {
                bail!("platform-font attribution for generated list markers unsupported");
            }
            if name == "STYLE"
                && node
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|children| {
                        children.iter().any(|child| {
                            child["nodeValue"].as_str().is_some_and(|text| {
                                text.contains("::")
                                    || text.to_ascii_lowercase().contains(":before")
                                    || text.to_ascii_lowercase().contains(":after")
                            })
                        })
                    })
            {
                bail!("platform-font attribution excludes CSS pseudo-generated content/selectors");
            }
            if ["INPUT", "TEXTAREA", "SELECT", "OPTION", "CANVAS", "IFRAME"].contains(&name) {
                bail!("platform-font attribution for controls/embedded content unsupported");
            }
        }
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            if node
                .get("childNodeCount")
                .and_then(Value::as_u64)
                .is_some_and(|count| count != children.len() as u64)
            {
                bail!("incomplete platform-font DOM tree");
            }
            if !["STYLE", "TITLE", "SCRIPT"].contains(&node["nodeName"].as_str().unwrap_or(""))
                && children.iter().any(|child| {
                    child["nodeType"].as_i64() == Some(3)
                        && child["nodeValue"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty())
                })
            {
                parents.insert(id);
            }
            for child in children.iter().rev() {
                pending.push(child);
            }
        } else if node["childNodeCount"]
            .as_u64()
            .is_some_and(|count| count > 0)
        {
            bail!("incomplete platform-font DOM tree");
        }
        if parents.len() > MAX_FONT_TEXT_PARENTS {
            bail!("platform-font text-parent budget exceeded");
        }
    }
    Ok(parents.into_iter().collect())
}
fn normalized_platform_faces(value: &Value) -> Result<Value> {
    let fonts = value["fonts"]
        .as_array()
        .context("actual platform-font list missing")?;
    if fonts.len() > 16 {
        bail!("platform-font face budget exceeded");
    }
    let mut out = Vec::new();
    for font in fonts {
        let family = font["familyName"]
            .as_str()
            .context("actual platform-font family missing")?;
        let postscript = font["postScriptName"]
            .as_str()
            .context("actual platform-font postscript name missing")?;
        let custom = font["isCustomFont"]
            .as_bool()
            .context("platform-font custom flag missing")?;
        let glyphs = font["glyphCount"]
            .as_u64()
            .context("platform-font glyph count missing")?;
        if family.is_empty()
            || family.len() > 256
            || postscript.is_empty()
            || postscript.len() > 256
            || family
                .chars()
                .chain(postscript.chars())
                .any(char::is_control)
            || glyphs > 1_048_576
        {
            bail!("invalid bounded platform-font record");
        }
        out.push(json!([family, postscript, custom, glyphs]));
    }
    out.sort_by_key(Value::to_string);
    Ok(Value::Array(out))
}
async fn observe_platform_faces(c: &mut CdpClient) -> Result<PlatformFaceObservation> {
    c.call("DOM.enable", json!({})).await?;
    c.call("CSS.enable", json!({})).await?;
    let document = c
        .call("DOM.getDocument", json!({"depth":-1,"pierce":false}))
        .await?;
    let root = document
        .get("root")
        .context("platform-font document root missing")?;
    let ids = text_parent_nodes(root)?;
    let mut observations = Vec::new();
    for id in &ids {
        let fonts = c
            .call("CSS.getPlatformFontsForNode", json!({"nodeId":id}))
            .await?;
        observations.push(json!([id, normalized_platform_faces(&fonts)?]));
    }
    // Complete census is repeated after capture; IDs and exact face/glyph records must agree.
    Ok(PlatformFaceObservation {
        signature: crate::core::tool_media::sha256(
            serde_json::to_string(&observations)?.as_bytes(),
        ),
        text_parents: ids.len(),
    })
}
async fn platform_faces_optional(
    c: &mut CdpClient,
    source: bool,
) -> Option<PlatformFaceObservation> {
    if !source {
        return None;
    }
    observe_platform_faces(c).await.ok()
}

pub struct BrowserScreenshot;
impl Tool for BrowserScreenshot {
    fn name(&self) -> &str {
        "browser_screenshot"
    }
    fn description(&self) -> &str {
        "Capture bounded viewport/CSS-clip PNG, DPR 1, from an explicitly attached target. Requires image-transmission approval and capable destination. Returns image plus URL/readiness/source evidence. Markers do not prove freshness. deterministic_static+reload proves only scriptless workspace file HTML. No full-page capture or mutation retry."
    }
    fn parameters(&self) -> Value {
        tool_parameters(json!({"type":"object","properties":{
            "transmit_image":{"type":"boolean","enum":[true],"description":"Explicit image transmission; approval required"},
            "clip":{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1,"maximum":4096},"height":{"type":"integer","minimum":1,"maximum":4096}},"required":["x","y","width","height"],"additionalProperties":false},
            "reload":{"type":"boolean","description":"Reload ignoring cache; mutation, no retry"},
            "deterministic_static":{"type":"boolean","description":"Prove scriptless workspace file HTML; requires reload"},
            "expected_build_marker":{"type":"string","maxLength":256,"description":"Expected marker; not source proof"}
        },"required":["transmit_image"],"additionalProperties":false}))
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        if args.get("reload").and_then(Value::as_bool) == Some(true) {
            WorkspaceEffect::External
        } else {
            WorkspaceEffect::None
        }
    }
    fn transmits_images(&self, _args: &Value) -> bool {
        true
    }
    fn preview(&self, args: &Value) -> Option<ApprovalPreview> {
        let mut preview = browser_preview(self.name(),args,
            "Capture and transmit potentially sensitive visible page pixels as PNG to the configured image-capable model destination. This preview performs no network/attach/capture.")?;
        preview.lines.push(format!(
            "region: {} · reload={} · source freshness defaults to unknown",
            if args.get("clip").is_some() {
                "bounded CSS clip inside viewport"
            } else {
                "viewport only"
            },
            args.get("reload").and_then(Value::as_bool).unwrap_or(false)
        ));
        Some(preview)
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        bail!("browser_screenshot requires the approval-gated image-aware observation executor")
    }
    fn execute_observation(&self, args: &Value) -> Result<ToolObservation> {
        if args.get("transmit_image").and_then(Value::as_bool) != Some(true) {
            bail!("browser_screenshot requires explicit transmit_image:true and destination capability approval before capture");
        }
        let clip = parse_clip(args)?;
        let reload = match args.get("reload") {
            Some(v) => v.as_bool().context("reload must be boolean")?,
            None => false,
        };
        let expected = match args.get("expected_build_marker") {
            Some(v) => Some(
                v.as_str()
                    .context("expected_build_marker must be string")?
                    .to_string(),
            ),
            None => None,
        };
        if expected
            .as_deref()
            .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            bail!("invalid expected_build_marker");
        }
        let deterministic_static = match args.get("deterministic_static") {
            Some(v) => v
                .as_bool()
                .context("deterministic_static must be boolean")?,
            None => false,
        };
        if deterministic_static && !reload {
            bail!("deterministic_static requires explicit reload:true");
        }
        let design = crate::core::exec_ctx::current().and_then(|ctx| ctx.design());
        if deterministic_static && design.is_none() {
            bail!("deterministic_static requires a scoped design workspace context");
        }
        if reload && !deterministic_static {
            mark_design_render_changed();
        }
        let revision = design.as_ref().map(|ctx| ctx.current_revision());
        let scope = design.as_ref().map(|ctx| ctx.evidence_scope());
        if scope.as_deref().is_some_and(|s| s.len() > 128) {
            bail!("design evidence scope exceeds image provenance limit");
        }
        let evidence_id = format!(
            "browser-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        );
        let (name, profile) = operation_target(args, None)?;
        block(with_cdp(name, profile, false, |c| {
            let expected = expected.clone();
            let scope = scope.clone();
            let evidence_id = evidence_id.clone();
            let design = design.clone();
            Box::pin(async move {
                let static_source = if deterministic_static {
                    let ctx = design.as_ref().unwrap();
                    let workspace = ctx.snapshot().workspace;
                    let state = page_state(c).await?;
                    let url = state["url"].as_str().context("missing static page URL")?;
                    let static_source = static_source::local_source(c, &workspace, url).await?;
                    let target = evidence_target(&c.profile, &c.endpoint, &c.target_id, url);
                    ctx.set_target(&target)?;
                    ctx.set_source_revision(&static_source.hash)?;
                    Some(static_source)
                } else {
                    None
                };
                let revision = if static_source.is_some() {
                    design.as_ref().map(|ctx| ctx.current_revision())
                } else {
                    revision
                };
                if reload {
                    let token = page_state(c).await?["document_token"].clone();
                    c.refs.clear();
                    c.call("Page.reload", json!({"ignoreCache":true})).await?;
                    let until = tokio::time::Instant::now() + Duration::from_secs(3);
                    loop {
                        let state = page_state(c).await?;
                        if !state["document_token"].is_null() && state["document_token"] != token {
                            break;
                        }
                        if tokio::time::Instant::now() >= until {
                            bail!("reload was requested but a new document was not observed within bounded readiness wait; refusing a potentially stale capture");
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                let mut readiness = settle(c).await?;
                if let Some(source) = static_source.as_ref() {
                    static_source::confirm_served(c, source).await?;
                }
                let before = page_state(c).await?;
                let width = dimension(&before["viewport"], "width")?;
                let height = dimension(&before["viewport"], "height")?;
                validate_dimensions(width, height)?;
                if before["viewport"]["device_pixel_ratio"].as_f64() != Some(1.0)
                    || before["viewport"]["scale"].as_f64() != Some(1.0)
                {
                    bail!("screenshot requires DPR 1 and unzoomed visual viewport; use browser_set_viewport first");
                }
                let region = clip.unwrap_or(Clip {
                    x: 0,
                    y: 0,
                    width,
                    height,
                });
                if region.x.checked_add(region.width).is_none_or(|n| n > width)
                    || region
                        .y
                        .checked_add(region.height)
                        .is_none_or(|n| n > height)
                {
                    bail!("CSS clip must be wholly inside the current viewport");
                }
                let page_x = before["viewport"]["page_x"]
                    .as_f64()
                    .context("missing viewport page_x")?;
                let page_y = before["viewport"]["page_y"]
                    .as_f64()
                    .context("missing viewport page_y")?;
                if !page_x.is_finite() || !page_y.is_finite() {
                    bail!("invalid viewport offset");
                }
                let font_before =
                    platform_faces_optional(c, static_source.is_some() && clip.is_none()).await;
                let capture_params = json!({"format":"png","fromSurface":true,"captureBeyondViewport":false,
                    "clip":{"x":page_x+f64::from(region.x),"y":page_y+f64::from(region.y),"width":region.width,"height":region.height,"scale":1}});
                let captured = c
                    .call("Page.captureScreenshot", capture_params.clone())
                    .await?;
                let encoded = captured
                    .get("data")
                    .and_then(Value::as_str)
                    .context("CDP capture returned no PNG")?;
                if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
                    bail!("CDP screenshot exceeds 8 MiB limit");
                }
                // Repeated observed raster, not a promise about all future frames.
                let repeated = if font_before.is_some() {
                    c.call("Page.captureScreenshot", capture_params)
                        .await
                        .ok()
                        .and_then(|value| {
                            value
                                .get("data")
                                .and_then(Value::as_str)
                                .filter(|s| s.len() <= MAX_IMAGE_BYTES.div_ceil(3) * 4)
                                .map(str::to_owned)
                        })
                } else {
                    None
                };
                let font_after = platform_faces_optional(c, font_before.is_some()).await;
                let after = page_state(c).await?;
                if let Some(source) = static_source.as_ref() {
                    static_source::confirm_served(c, source).await?;
                }
                let changed = before["url"] != after["url"]
                    || before["document_token"] != after["document_token"]
                    || before["viewport"] != after["viewport"]
                    || before["observed_build_marker"] != after["observed_build_marker"]
                    || before["layout_signature"] != after["layout_signature"];
                if changed
                    || after["document"] != "complete"
                    || after["fonts"] != "loaded"
                    || after["images"] != "ready"
                    || clip.is_some()
                {
                    readiness["ready"] = json!(false);
                    readiness["changed_during_capture"] = json!(changed);
                    readiness["viewport_coverage_complete"] = json!(clip.is_none());
                }
                let observed = before["observed_build_marker"].as_str();
                let marker_match = expected.as_deref().is_some_and(|e| Some(e) == observed);
                // An explicit marker is evidence of the marker only, never proof that the served
                // build corresponds to current workspace bytes. Only static resource verification
                // establishes source provenance until a trusted build integration exists.
                let known_source = if !changed {
                    static_source.as_ref().map(|s| s.hash.as_str())
                } else {
                    None
                };
                let url = before["url"].as_str().context("missing page URL")?;
                let target = evidence_target(&c.profile, &c.endpoint, &c.target_id, url);
                let locator = serde_json::to_string(
                    &json!({"profile":c.profile,"target_id":c.target_id,"url":url}),
                )?;
                if locator.len() > 2048 {
                    bail!("screenshot locator exceeds bounded provenance limit");
                }
                let mut provenance = ImageProvenance::new("browser_screenshot", Some(locator));
                provenance.evidence_id = Some(evidence_id.clone());
                provenance.design_scope = scope.clone();
                let encoded = encoded.to_string();
                let expected_width = region.width;
                let expected_height = region.height;
                let image = c
                    .budget
                    .run(async move {
                        tokio::task::spawn_blocking(move || {
                            let bytes = crate::core::tool_media::base64_decode_bounded(
                                &encoded,
                                MAX_IMAGE_BYTES,
                            )?;
                            // Reject malicious dimensions before the bounded full PNG decode.
                            if bytes.len() < 24
                                || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                                || &bytes[12..16] != b"IHDR"
                                || u32::from_be_bytes(bytes[16..20].try_into().unwrap())
                                    != expected_width
                                || u32::from_be_bytes(bytes[20..24].try_into().unwrap())
                                    != expected_height
                            {
                                bail!("CDP PNG header dimensions do not match requested region");
                            }
                            ToolImage::from_bytes(&bytes, "image/png", provenance)
                        })
                        .await
                        .context("PNG validation task failed")?
                    })
                    .await?;
                if image.metadata().width != Some(region.width)
                    || image.metadata().height != Some(region.height)
                {
                    bail!("CDP PNG dimensions do not match requested CSS capture region at DPR 1");
                }
                let repeated_raster_sha = if let Some(encoded) = repeated {
                    let expected_width = region.width;
                    let expected_height = region.height;
                    let budget = c.budget.clone();
                    budget
                        .run(async move {
                            tokio::task::spawn_blocking(move || {
                                let bytes = crate::core::tool_media::base64_decode_bounded(
                                    &encoded,
                                    MAX_IMAGE_BYTES,
                                )?;
                                let size =
                                    crate::agent::design::reference_compare::encoded_rgba_size(
                                        &bytes,
                                        "image/png",
                                    )?;
                                let _memory = crate::llm::image_send::try_image_memory_lease(
                                    2 * size
                                        + crate::core::tool_media::MAX_PNG_DECODE_BYTES
                                        + bytes.len(),
                                )
                                .context("repeated raster memory budget busy")?;
                                let rgba =
                                    crate::agent::design::reference_compare::decode_supported(
                                        &bytes,
                                        "image/png",
                                    )?;
                                if rgba.width != expected_width || rgba.height != expected_height {
                                    bail!("repeated raster dimensions changed");
                                }
                                Ok(crate::core::tool_media::sha256(&rgba.pixels))
                            })
                            .await
                            .context("repeated raster decode task failed")?
                        })
                        .await
                        .ok()
                } else {
                    None
                };
                let ready = readiness["ready"].as_bool().unwrap_or(false);
                let primary_raster_sha = if repeated_raster_sha.is_some() {
                    let bytes = image.bytes()?;
                    let expected_width = region.width;
                    let expected_height = region.height;
                    let budget = c.budget.clone();
                    budget
                        .run(async move {
                            tokio::task::spawn_blocking(move || {
                                let size =
                                    crate::agent::design::reference_compare::encoded_rgba_size(
                                        &bytes,
                                        "image/png",
                                    )?;
                                let _memory = crate::llm::image_send::try_image_memory_lease(
                                    2 * size
                                        + crate::core::tool_media::MAX_PNG_DECODE_BYTES
                                        + bytes.len(),
                                )
                                .context("primary raster memory budget busy")?;
                                let rgba =
                                    crate::agent::design::reference_compare::decode_supported(
                                        &bytes,
                                        "image/png",
                                    )?;
                                if rgba.width != expected_width || rgba.height != expected_height {
                                    bail!("primary raster dimensions changed");
                                }
                                Ok(crate::core::tool_media::sha256(&rgba.pixels))
                            })
                            .await
                            .context("primary raster decode task failed")?
                        })
                        .await
                        .ok()
                } else {
                    None
                };
                let capture_identity = match (font_before.as_ref(), primary_raster_sha.as_ref()) {
                    (Some(fonts), Some(raster_sha))
                        if ready
                            && !changed
                            && known_source.is_some()
                            && font_before == font_after
                            && primary_raster_sha == repeated_raster_sha =>
                    {
                        Some(crate::agent::design::CaptureIdentity {
                            output_sha256: image.metadata().sha256.clone(),
                            dpr_milli: 1000,
                            font_identity: format!("observed-platform-faces:{}", fonts.signature),
                            frame_state: format!("two-observed-rasters:{raster_sha}"),
                        })
                    }
                    _ => None,
                };
                if let Some(ctx) = design {
                    ctx.record_capture(&evidence_id,known_source,&target,crate::agent::design::Viewport{width,height},revision.unwrap_or(0),ready,
                    if static_source.is_some(){"Deterministic self-contained static local HTML reload; browser resource bytes and canonical workspace bytes match before/after capture"}else{"Source freshness unknown; page marker does not establish current workspace fingerprint correspondence"})?;
                    if let Some(identity) = capture_identity.as_ref() {
                        ctx.record_capture_identity(&evidence_id, identity.clone())?;
                    }
                }
                let metadata = json!({"capture_id":evidence_id,"design_scope":scope,"profile":c.profile,"target_id":c.target_id,
                    "url":url,"evidence_target":target,"viewport":before["viewport"],"mobile_override":c.viewport.map(|v|v.mobile),
                    "clip":{"x":region.x,"y":region.y,"width":region.width,"height":region.height},"readiness":readiness,
                    "source_revision":{"render_revision":revision,"source_hash":known_source,"observed_build_marker":observed,"expected_build_marker":expected,
                        "marker_matches":marker_match,"freshness":if !changed&&static_source.is_some(){"deterministic_static_resource_matches_workspace"}else{"unknown"}},
                    "captured_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
                    "reload":reload,"image":image.metadata(),"capture_identity":capture_identity,"identity_scope":"Actual platform-face name/custom/glyph-count signatures and two observed equal rasters only; not font-file version identity, future stability or universal determinism.","font_text_parents":font_before.as_ref().map(|f|f.text_parents),"note":"Image capture is not image projection or visual review. No latest-source, full-page, or animation-idle claim."});
                Ok(ToolObservation {
                    text: metadata.to_string(),
                    images: vec![image],
                })
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_actual_font_records_and_complete_dom_census() {
        let node = json!({"nodeId":1,"nodeName":"DIV","nodeType":1,"childNodeCount":1,"children":[{"nodeId":2,"nodeName":"#text","nodeType":3,"nodeValue":"actual text"}]});
        assert_eq!(text_parent_nodes(&node).unwrap(), vec![1]);
        let fonts = json!({"fonts":[{"familyName":"Arial","postScriptName":"ArialMT","isCustomFont":false,"glyphCount":8}]});
        assert!(normalized_platform_faces(&fonts).is_ok());
        assert!(normalized_platform_faces(
            &json!({"fonts":[{"familyName":"Arial","glyphCount":8}]})
        )
        .is_err());
        assert!(
            text_parent_nodes(&json!({"nodeId":1,"nodeName":"DIV","childNodeCount":1})).is_err()
        );
        assert!(text_parent_nodes(&json!({"nodeId":1,"nodeName":"INPUT"})).is_err());
        assert!(
            text_parent_nodes(&json!({"nodeId":1,"nodeName":"DIV","pseudoElements":[{}]})).is_err()
        );
        let mut many = node.clone();
        let children:Vec<Value> = (0..65).map(|i| json!({"nodeId":10+i*2,"nodeName":"SPAN","children":[{"nodeId":11+i*2,"nodeType":3,"nodeValue":"text"}]})).collect();
        many["children"] = json!(children);
        many["childNodeCount"] = json!(65);
        assert!(text_parent_nodes(&many).is_err());
        let mut pseudo = node.clone();
        pseudo["nodeName"] = json!("STYLE");
        pseudo["children"][0]["nodeValue"] = json!("div::before {content: 'text'}");
        assert!(text_parent_nodes(&pseudo).is_err());
    }
    #[test]
    fn bounded_viewport_and_clip_validation() {
        assert!(Viewport::from_args(&json!({"width":1280,"height":720})).is_ok());
        for value in [
            json!({"width":0,"height":1}),
            json!({"width":4096,"height":4096}),
            json!({"width":1,"height":1,"device_pixel_ratio":2}),
            json!({"width":-1,"height":2}),
        ] {
            assert!(Viewport::from_args(&value).is_err());
        }
        assert!(
            parse_clip(&json!({"clip":{"x":0,"y":0,"width":10,"height":10}}))
                .unwrap()
                .is_some()
        );
        assert!(parse_clip(&json!({"clip":{"x":0,"y":0,"width":0,"height":1}})).is_err());
    }
    #[tokio::test]
    async fn loopback_actual_platform_faces_observe_changes_without_a_browser() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut reads = 0;
            while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "DOM.enable" | "CSS.enable" => json!({}),
                    "DOM.getDocument" => {
                        json!({"root":{"nodeId":1,"nodeName":"DIV","childNodeCount":1,"children":[{"nodeId":2,"nodeName":"#text","nodeType":3,"nodeValue":"fixture"}]}})
                    }
                    "CSS.getPlatformFontsForNode" => {
                        reads += 1;
                        json!({"fonts":[{"familyName":if reads < 3 {"FixtureFace"} else {"ChangedFace"},"postScriptName":"FixturePS","isCustomFont":false,"glyphCount":7}]})
                    }
                    _ => panic!("unexpected fixture command"),
                };
                if ws
                    .send(WsMessage::Text(
                        json!({"id":request["id"],"result":result}).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let budget = transport::Budget::new(Duration::from_secs(3));
        let selection = config::BrowserProfile {
            provider: "cdp".into(),
            endpoint: format!("http://{addr}"),
            auth_env: None,
            target_id: None,
            ws_origins: Vec::new(),
        };
        let ws = transport::ws_connect(
            "fixture",
            &selection,
            &format!("ws://{addr}/fixture"),
            &budget,
        )
        .await
        .unwrap();
        let lock_path = std::env::temp_dir().join(format!(
            "aizen-font-fixture-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let mut c = CdpClient {
            ws,
            next_id: 1,
            _ownership: crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                &lock_path,
                Duration::ZERO,
            )
            .unwrap(),
            profile: "fixture".into(),
            endpoint: selection.endpoint.clone(),
            selection,
            target_id: "fixture".into(),
            budget,
            viewport: None,
            refs: HashMap::new(),
        };
        let first = observe_platform_faces(&mut c).await.unwrap();
        assert_eq!(first.text_parents, 1);
        assert_eq!(first, observe_platform_faces(&mut c).await.unwrap());
        assert_ne!(first, observe_platform_faces(&mut c).await.unwrap());
        drop(c);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_file(lock_path);
    }
    #[test]
    fn same_url_different_page_or_profile_cannot_share_evidence() {
        let url = "file:///workspace/index.html";
        let desktop_target = evidence_target("dedicated", "http://127.0.0.1:9222", "tab-a", url);
        let mobile_target = evidence_target("dedicated", "http://127.0.0.1:9222", "tab-b", url);
        assert_ne!(desktop_target, mobile_target);
        assert_ne!(
            desktop_target,
            evidence_target("other", "http://127.0.0.1:9222", "tab-a", url)
        );
        assert_ne!(
            desktop_target,
            evidence_target("dedicated", "http://127.0.0.1:9333", "tab-a", url)
        );
        assert_eq!(
            desktop_target,
            evidence_target("dedicated", "http://127.0.0.1:9222", "tab-a", url)
        );
        assert!(!desktop_target.contains("127.0.0.1") && !desktop_target.contains("workspace"));
        let ctx = crate::agent::design::DesignContext::new(
            "browser-target-test",
            "",
            crate::agent::design::DesignMode::On,
        );
        let hash = crate::core::tool_media::sha256(b"static bytes");
        ctx.set_source_revision(&hash).unwrap();
        ctx.set_target(&desktop_target).unwrap();
        let old_revision = ctx.current_revision();
        ctx.record_capture(
            "desktop",
            Some(&hash),
            &desktop_target,
            crate::agent::design::Viewport {
                width: 1280,
                height: 720,
            },
            old_revision,
            true,
            "same URL first tab",
        )
        .unwrap();
        ctx.set_target(&mobile_target).unwrap();
        assert!(ctx.current_revision() > old_revision);
        ctx.record_capture(
            "mobile",
            Some(&hash),
            &mobile_target,
            crate::agent::design::Viewport {
                width: 390,
                height: 844,
            },
            ctx.current_revision(),
            true,
            "same URL other tab",
        )
        .unwrap();
        let evidence = ctx.snapshot().evidence;
        assert_eq!(
            evidence[0].status,
            crate::agent::design::EvidenceStatus::Stale
        );
        assert_ne!(evidence[0].target, evidence[1].target);
        assert_ne!(evidence[0].render_revision, evidence[1].render_revision);
    }
    #[test]
    fn viewport_changes_share_source_revision_for_responsive_evidence() {
        let desktop = Viewport::from_args(&json!({"width":1280,"height":720})).unwrap();
        let mobile = Viewport::from_args(&json!({"width":390,"height":844,"mobile":true})).unwrap();
        let source = b"<!doctype html><html><body><h1>Same source</h1></body></html>";
        let hash = crate::core::tool_media::sha256(source);
        // Static captures always bind exactly the same bytes for both viewport dimensions;
        // viewport sizing itself is not a source edit/DOM mutation revision.
        assert_ne!(desktop.width, mobile.width);
        assert_eq!(hash, crate::core::tool_media::sha256(source));
        let context = crate::agent::design::DesignContext::new(
            "browser-responsive",
            "",
            crate::agent::design::DesignMode::On,
        );
        context.set_target("file:///responsive.html").unwrap();
        context.set_source_revision(&hash).unwrap();
        let revision = context.current_revision();
        context.set_target("file:///responsive.html").unwrap();
        context.set_source_revision(&hash).unwrap();
        assert_eq!(context.current_revision(), revision);
        for (id, view) in [("desktop", desktop), ("mobile", mobile)] {
            context
                .record_capture(
                    id,
                    Some(&hash),
                    "file:///responsive.html",
                    crate::agent::design::Viewport {
                        width: view.width,
                        height: view.height,
                    },
                    revision,
                    true,
                    "static responsive capture",
                )
                .unwrap();
        }
        assert!(context
            .snapshot()
            .evidence
            .iter()
            .all(|e| e.render_revision == revision));
        assert_eq!(context.snapshot().evidence.len(), 2);
    }
    #[test]
    fn screenshot_requires_explicit_transmission_and_preview_is_offline() {
        let tool = BrowserScreenshot;
        // Schema bound is measured on compact wire JSON, matching registration schema tests.
        assert!(tool.parameters().to_string().len() <= 1400);
        assert!(tool.is_destructive());
        assert!(tool.transmits_images(&json!({})));
        assert!(tool
            .execute_observation(&json!({}))
            .unwrap_err()
            .to_string()
            .contains("transmit_image"));
        let preview = tool
            .preview(&json!({"profile":"nonexistent","target_id":"fake"}))
            .unwrap();
        assert!(preview.lines.join("\n").contains("no network"));
    }
}
