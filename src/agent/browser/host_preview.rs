//! Host-only post-action frame. Reuse the exact owned page socket, including on background tabs.
//! A failed or slow viewer read cannot change the completed tool result or replay an action.
use super::{transport, CdpClient};
use anyhow::{ensure, Context, Result};
use serde_json::json;
use std::time::Duration;

pub(super) async fn present(client: &mut CdpClient) {
    client.budget = transport::Budget::new(Duration::from_millis(900));
    let mut title = String::new();
    let frame = capture(client, &mut title).await.unwrap_or_default();
    crate::ui::events::browser_frame(&client.profile, &client.target_id, &title, &frame);
}

async fn capture(client: &mut CdpClient, title: &mut String) -> Result<String> {
    let info = client.call("Target.getTargetInfo", json!({})).await?;
    let page = &info["targetInfo"];
    ensure!(
        page["type"] == "page" && page["targetId"].as_str() == Some(&client.target_id),
        "viewer target changed"
    );
    *title = page["title"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    let layout = client.call("Page.getLayoutMetrics", json!({})).await?;
    let viewport = &layout["cssVisualViewport"];
    let width = viewport["clientWidth"].as_f64().context("missing width")?;
    let height = viewport["clientHeight"]
        .as_f64()
        .context("missing height")?;
    ensure!(
        width >= 1. && height >= 1. && width <= 16384. && height <= 16384.,
        "invalid viewport"
    );
    let scale = (960. / width).min(720. / height).min(1.);
    let image = client.call("Page.captureScreenshot", json!({
        "format":"jpeg","quality":64,"fromSurface":true,"captureBeyondViewport":false,
        "clip":{"x":viewport["pageX"],"y":viewport["pageY"],"width":width,"height":height,"scale":scale}
    })).await?;
    let jpeg = image["data"].as_str().context("missing image")?;
    ensure!(
        jpeg.starts_with("/9j/") && jpeg.len() <= 512 * 1024,
        "invalid viewer frame"
    );
    Ok(jpeg.to_owned())
}
