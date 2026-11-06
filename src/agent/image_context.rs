//! Run-local image notes; no pixel disclosure or durable consent is created here.

use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub struct ImageContext;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    id: Option<String>,
    note: Option<String>,
    uncertainty: Option<String>,
    #[serde(default)]
    release_pixels: bool,
}

impl Tool for ImageContext {
    fn name(&self) -> &str {
        "image_context"
    }
    fn description(&self) -> &str {
        "Manage current-task images without uploading: list scoped handles, remember task-relevant observations/uncertainty, release_pixels when finished, or pin/unpin your own retention choice. Controller pins cannot be cleared. Release requires successful pixel delivery and a live reference_read or scoped image_read handle. Notes are not verification; reread unresolved detail. Batch alongside ordinary work."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["list","remember","release","pin","unpin"]},
            "id":{"type":"string","maxLength":128},
            "note":{"type":"string","maxLength":768},
            "uncertainty":{"type":"string","maxLength":256},
            "release_pixels":{"type":"boolean"}
        },"required":["action"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, value: &Value) -> Result<String> {
        let args: Args = serde_json::from_value(value.clone())?;
        let ctx = crate::core::exec_ctx::current()
            .context("image_context requires a current execution scope")?;
        // Revalidate revocation before allowing a dormant state with a purported re-read handle.
        ctx.references_checked()?;
        let mut visual = ctx.visual();
        if args.action != "list" {
            visual.mutate(
                args.id.as_deref().context("image occurrence id required")?,
                &args.action,
                args.note.as_deref(),
                args.uncertainty.as_deref(),
                args.release_pixels,
            )?;
        }
        Ok(visual.list().to_string())
    }
}
