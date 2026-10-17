//! Deterministic pixel-edge contours for explicit masks/colors, not semantic segmentation.
//! Paths preserve the selected mask at pixel edges (including holes via evenodd fill).
use super::reference_compare::RgbaImage;
#[cfg(test)]
use super::reference_compare::WorkBudget;
use crate::core::tool_media::PixelRect;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_TRACE_PIXELS: usize = 1_048_576;
pub const MAX_TRACE_EDGES: usize = 32_768;
pub const MAX_TRACE_POINTS: usize = 16_384;
pub const MAX_TRACE_CONTOURS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Foreground {
    /// Explicit inclusive alpha threshold; no implicit background/color inference.
    Alpha { min: u8 },
    /// Each RGBA channel must fall within this caller-selected absolute delta.
    Color {
        rgba: [u8; 4],
        max_channel_delta: u8,
    },
    /// Row-major boolean mask exactly matching the explicitly selected crop.
    Mask { pixels: Vec<bool> },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceRequest {
    pub crop: PixelRect,
    pub foreground: Foreground,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceResult {
    pub crop: PixelRect,
    pub foreground_pixels: usize,
    pub contours: usize,
    pub points: usize,
    /// Crop-local integer pixel coordinates; d is safe data, not a complete SVG document.
    pub path: String,
    pub fill_rule: String,
}
type Point = (u32, u32);

#[cfg(test)]
pub fn trace_rgba(image: &RgbaImage, request: &TraceRequest) -> Result<TraceResult> {
    image.validate()?;
    let crop = request.crop;
    ensure!(
        crop.width > 0
            && crop.height > 0
            && crop
                .x
                .checked_add(crop.width)
                .is_some_and(|x| x <= image.width)
            && crop
                .y
                .checked_add(crop.height)
                .is_some_and(|y| y <= image.height),
        "trace crop outside image bounds"
    );
    let count = (crop.width as usize)
        .checked_mul(crop.height as usize)
        .ok_or_else(|| anyhow::anyhow!("trace size overflow"))?;
    ensure!(
        count <= MAX_TRACE_PIXELS,
        "trace crop exceeds 1,048,576 pixels; choose a smaller explicit region"
    );
    let _budget = WorkBudget::acquire(image.pixels.len().saturating_add(16 * 1024 * 1024))?;
    trace_inner(image, request, count)
}
/// Caller already holds codec/trace peak lease; prevents duplicate accounting in tool entrypoint.
pub(crate) fn trace_admitted(image: &RgbaImage, request: &TraceRequest) -> Result<TraceResult> {
    image.validate()?;
    let crop = request.crop;
    ensure!(
        crop.width > 0
            && crop.height > 0
            && crop
                .x
                .checked_add(crop.width)
                .is_some_and(|x| x <= image.width)
            && crop
                .y
                .checked_add(crop.height)
                .is_some_and(|y| y <= image.height),
        "trace crop outside image bounds"
    );
    let count = (crop.width as usize)
        .checked_mul(crop.height as usize)
        .ok_or_else(|| anyhow::anyhow!("trace size overflow"))?;
    ensure!(
        count <= MAX_TRACE_PIXELS,
        "trace crop pixel budget exceeded"
    );
    trace_inner(image, request, count)
}
fn trace_inner(image: &RgbaImage, request: &TraceRequest, count: usize) -> Result<TraceResult> {
    let crop = request.crop;
    if let Foreground::Mask { pixels } = &request.foreground {
        ensure!(
            pixels.len() == count,
            "foreground mask must exactly match selected crop"
        );
    }
    let mut mask = vec![false; count];
    for y in 0..crop.height {
        for x in 0..crop.width {
            let local = (y as usize * crop.width as usize) + x as usize;
            let offset =
                (((crop.y + y) as usize * image.width as usize) + (crop.x + x) as usize) * 4;
            let p = &image.pixels[offset..offset + 4];
            mask[local] = match &request.foreground {
                Foreground::Alpha { min } => p[3] >= *min,
                Foreground::Color {
                    rgba,
                    max_channel_delta,
                } => p
                    .iter()
                    .zip(rgba)
                    .all(|(a, b)| a.abs_diff(*b) <= *max_channel_delta),
                Foreground::Mask { pixels } => pixels[local],
            };
        }
    }
    let foreground_pixels = mask.iter().filter(|v| **v).count();
    let filled = |x: u32, y: u32| mask[y as usize * crop.width as usize + x as usize];
    // Clockwise directed boundaries. BTree ordering gives stable contour start/order.
    let mut edges: BTreeMap<Point, BTreeSet<Point>> = BTreeMap::new();
    let mut edge_count = 0;
    let mut add = |a: Point, b: Point| -> Result<()> {
        edge_count += 1;
        ensure!(
            edge_count <= MAX_TRACE_EDGES,
            "trace boundary exceeds 32,768 edges; choose a smaller/simple explicit mask"
        );
        edges.entry(a).or_default().insert(b);
        Ok(())
    };
    for y in 0..crop.height {
        for x in 0..crop.width {
            if !filled(x, y) {
                continue;
            }
            if y == 0 || !filled(x, y - 1) {
                add((x, y), (x + 1, y))?;
            }
            if x + 1 == crop.width || !filled(x + 1, y) {
                add((x + 1, y), (x + 1, y + 1))?;
            }
            if y + 1 == crop.height || !filled(x, y + 1) {
                add((x + 1, y + 1), (x, y + 1))?;
            }
            if x == 0 || !filled(x - 1, y) {
                add((x, y + 1), (x, y))?;
            }
        }
    }
    let mut path = String::new();
    let mut points = 0;
    let mut contours = 0;
    while let Some((&start, _)) = edges.first_key_value() {
        contours += 1;
        ensure!(contours <= MAX_TRACE_CONTOURS, "trace exceeds 256 contours");
        let mut polygon = vec![start];
        let mut current = start;
        let mut incoming = None;
        loop {
            let candidates = edges
                .get(&current)
                .ok_or_else(|| anyhow::anyhow!("invalid open mask contour"))?;
            let next = *candidates
                .iter()
                .min_by_key(|next| {
                    let outgoing = direction(current, **next);
                    incoming.map_or(outgoing, |d| match (outgoing + 4 - d) % 4 {
                        1 => 0,
                        0 => 1,
                        3 => 2,
                        _ => 3,
                    })
                })
                .ok_or_else(|| anyhow::anyhow!("empty contour edge"))?;
            let candidates = edges.get_mut(&current).unwrap();
            candidates.remove(&next);
            if candidates.is_empty() {
                edges.remove(&current);
            }
            incoming = Some(direction(current, next));
            current = next;
            if current == start {
                break;
            }
            polygon.push(current);
            ensure!(
                polygon.len() <= MAX_TRACE_EDGES,
                "trace contour budget exceeded"
            );
        }
        // Exact collinearity simplification only: no displacement, smoothing, invented epsilon.
        let mut simplified = Vec::new();
        for i in 0..polygon.len() {
            let prev = polygon[(i + polygon.len() - 1) % polygon.len()];
            let p = polygon[i];
            let next = polygon[(i + 1) % polygon.len()];
            if !((prev.0 == p.0 && p.0 == next.0) || (prev.1 == p.1 && p.1 == next.1)) {
                simplified.push(p);
            }
        }
        points += simplified.len();
        ensure!(
            points <= MAX_TRACE_POINTS,
            "trace exceeds 16,384 simplified points"
        );
        if let Some((x, y)) = simplified.first() {
            if !path.is_empty() {
                path.push(' ');
            }
            path.push_str(&format!("M{x} {y}"));
            for (x, y) in &simplified[1..] {
                path.push_str(&format!("L{x} {y}"));
            }
            path.push('Z');
        }
    }
    Ok(TraceResult {
        crop,
        foreground_pixels,
        contours,
        points,
        path,
        fill_rule: "evenodd".into(),
    })
}
fn direction(a: Point, b: Point) -> u8 {
    if b.0 > a.0 {
        0
    } else if b.1 > a.1 {
        1
    } else if b.0 < a.0 {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(width: u32, height: u32) -> RgbaImage {
        RgbaImage {
            width,
            height,
            pixels: vec![255; (width * height * 4) as usize],
        }
    }
    fn request(width: u32, height: u32, pixels: Vec<bool>) -> TraceRequest {
        TraceRequest {
            crop: PixelRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            foreground: Foreground::Mask { pixels },
        }
    }
    #[test]
    fn rectangle_is_exact_deterministic_and_collinear_simplified() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = image(3, 2);
        let req = request(3, 2, vec![true; 6]);
        let trace = trace_rgba(&a, &req).unwrap();
        assert_eq!(trace.path, "M0 0L3 0L3 2L0 2Z");
        assert_eq!(
            (trace.contours, trace.points, trace.foreground_pixels),
            (1, 4, 6)
        );
        assert_eq!(trace, trace_rgba(&a, &req).unwrap());
    }
    #[test]
    fn holes_and_diagonal_touch_have_separate_contours() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = image(3, 3);
        let mut mask = vec![true; 9];
        mask[4] = false;
        let trace = trace_rgba(&a, &request(3, 3, mask)).unwrap();
        assert_eq!(trace.contours, 2);
        assert_eq!(trace.fill_rule, "evenodd");
        let a = image(2, 2);
        let trace = trace_rgba(&a, &request(2, 2, vec![true, false, false, true])).unwrap();
        assert_eq!(trace.contours, 2);
    }
    #[test]
    fn explicit_color_alpha_crop_and_bounds() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 0, 0],
        };
        let crop = PixelRect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        let req = TraceRequest {
            crop,
            foreground: Foreground::Color {
                rgba: [255, 0, 0, 255],
                max_channel_delta: 0,
            },
        };
        assert_eq!(trace_rgba(&a, &req).unwrap().foreground_pixels, 1);
        let req = TraceRequest {
            crop,
            foreground: Foreground::Alpha { min: 1 },
        };
        assert_eq!(trace_rgba(&a, &req).unwrap().foreground_pixels, 1);
        assert!(trace_rgba(&a, &request(2, 1, vec![true])).is_err());
        assert!(trace_rgba(&a, &request(3, 1, vec![true; 3])).is_err());
        assert_eq!(
            trace_rgba(&a, &request(2, 1, vec![false; 2])).unwrap().path,
            ""
        );
    }
    #[test]
    fn noisy_mask_hits_contour_or_edge_budget_instead_of_unbounded_paths() {
        let _lock = crate::llm::image_send::image_test_lock();
        let a = image(64, 64);
        let mask = (0..4096).map(|i| (i / 64 + i % 64) % 2 == 0).collect();
        assert!(trace_rgba(&a, &request(64, 64, mask)).is_err());
    }
}
