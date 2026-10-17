//! Original, compact English guides. These are fetched on demand, never inserted into a system lane.

pub const FOUNDATION: &str = "Start from the user's audience, task and repository conventions. Discover existing branding, tokens, components and asset licenses before changing them. Name one intentional visual direction, a primary action and observable acceptance checks. Build a coherent hierarchy rather than decorating a generic template. Preserve established patterns for preserve intent; a redesign needs explicit scope. Use real content, readable contrast, keyboard access and honest loading/error/empty states. Verify the rendered result at narrow and wide widths; fix what the images actually show.";

const LAYOUT: &str = "Compose around the user's primary task. Group related content with spacing and alignment before adding borders. Give the main action and heading a clear hierarchy; supporting content should not compete. Use a consistent spacing scale, a readable content measure and deliberate density. Grids must reflect content relationships, not arbitrary card counts. Reserve media dimensions to prevent jumps. Avoid horizontal page scroll at phone width; inspect long labels and realistic content.";
const TYPE: &str = "Reuse repository typography first. Choose a small role-based type scale for title, section, body, label and metadata; do not use size alone to convey structure. Keep body text readable with comfortable line height and a bounded line length. Use semantic headings in order. Limit families and weights; use a system fallback and handle font loading. Test long names, localization and zoom without clipping. Do not turn every label into uppercase or every heading into oversized display type.";
const COLOR: &str = "Reuse brand and theme tokens unless redesign is explicit. Define background, surface, foreground, muted, border, accent, focus, success and error roles, not scattered hex literals. Use one dominant accent and intentional semantic colors. Check text and focus contrast in each supported theme; never convey state by color alone. Quiet surfaces should support hierarchy, not wash out labels. Match elevation and borders to content grouping rather than adding gradients everywhere.";
const ASSETS: &str = "Inspect available assets before inventing substitutes. Use relevant, licensed images with meaningful alt text; decorative images get empty alt text. Maintain intrinsic ratios and crop with a stated focal point. Icons must come from an existing coherent set and retain visible labels for ambiguous actions. Do not fabricate logos or imply affiliation. Optimize dimensions and loading; never show a broken remote asset as finished work. Use an honest placeholder when no suitable asset exists.";
const RESPONSIVE: &str = "Design behavior, not only breakpoints. Narrow screens should prioritize the primary task, preserve reading order and offer usable navigation. Choose wrapping, stacking and overflow rules for each group; do not merely shrink desktop text. Keep touch targets usable, tables discoverable and sticky regions from covering content. Test both narrow and wide viewports with real content, including zoom. Respect reduced motion and avoid fixed heights that clip.";
const INTERACTIONS: &str = "Specify the result of each visible action. Implement focus, hover, active, disabled, loading, success, error and empty states where relevant. Keyboard navigation and semantic controls must work without a mouse. Show progress honestly, preserve user input after failure and explain recovery. Avoid dead controls and surprise navigation. Prefer familiar interaction patterns; motion should explain change, not delay a task. Respect reduced-motion preferences.";
const ACCESSIBILITY: &str = "Use semantic landmarks, labels and native controls. Provide visible focus, sensible tab order, escape behavior and focus return for overlays. Associate errors with inputs and announce asynchronous state when useful. Check text contrast, target size, alt text and meaning without color. Keep DOM reading order aligned with visual order. Automated checks complement keyboard and rendered review; they do not prove complete accessibility.";
const REVIEW: &str = "Before Done, compare the actual projected screenshots with the brief's acceptance criteria. Check hierarchy, readable type, spacing, alignment, brand consistency, asset integrity, narrow/wide overflow and interaction states. DOM snapshots and passing builds cannot prove appearance. A screenshot of an old server is stale evidence; require readiness and trusted current-source correspondence (confirmed deterministic static reload here); a marker alone is not proof. Name concrete defects and locations, repair at most twice, and capture at most four times per pass. If vision, rendering or freshness is unavailable, report that limitation and incomplete visual verification; never invent a review.";

const GRAPHICS: &str = "Match selected references, not their theme: inspect original pixels/crops before building, measure composition/landmarks/colors, and reuse authorized decorative crops or deterministic source-derived geometry. Random silhouettes/particles or seeded geometry alone do not prove fidelity; never fake functioning UI with a page screenshot. Explicit alpha/color/mask tracing preserves selected pixel-edge contours, not semantic segmentation. Keep purposeful geometry, focal point and negative space; SVG needs a viewBox, aspect-ratio scaling and readable text. Add accessible title/description or equivalent and respect reduced motion. Capture current implementation; compare whole original and output under frozen viewport/DPR/regions, repair measured defects within budget. Exact means zero differing pixels; missing tolerance means measurements only, never an invented pass. Passing crops do not certify the whole page, mobile, functionality or accessibility.";
const DATAVIZ: &str = "Choose the chart by the question: bars compare magnitudes, lines show time, a stat shows one value. Use actual supplied data; never fabricate values or imply missing observations are zero. Label units, time range and source. Keep scales honest and comparable: bars start at zero; disclose any justified truncated line scale. Avoid dual axes, perspective and decorative area that distort quantities. Direct-label important series and values; provide a semantic table with the same data and scope. Color must not be the only identity channel: add labels or shapes and check colorblind legibility and contrast. Keyboard/focus access must expose tooltip values too. Respect reduced motion.";

pub fn get(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "general" => FOUNDATION,
        "layout" => LAYOUT,
        "type" => TYPE,
        "color" => COLOR,
        "assets" => ASSETS,
        "responsive" => RESPONSIVE,
        "interactions" => INTERACTIONS,
        "accessibility" => ACCESSIBILITY,
        "review" => REVIEW,
        "graphics" => GRAPHICS,
        "dataviz" => DATAVIZ,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graphics_and_dataviz_topics_preserve_accessibility_and_evidence_honesty() {
        let graphics = get("graphics").unwrap();
        for required in [
            "purposeful geometry",
            "focal point",
            "negative space",
            "viewBox",
            "aspect-ratio",
            "readable text",
            "accessible title/description",
            "reduced motion",
        ] {
            assert!(graphics.contains(required), "graphics missing {required}");
        }
        let dataviz = get("dataviz").unwrap();
        for required in [
            "chart by the question",
            "never fabricate values",
            "units",
            "scales honest",
            "bars start at zero",
            "truncated line scale",
            "dual axes",
            "Direct-label",
            "semantic table",
            "Color must not be the only",
            "colorblind",
            "Keyboard/focus",
            "reduced motion",
        ] {
            assert!(dataviz.contains(required), "dataviz missing {required}");
        }
        assert!(get("unknown").is_none());
    }

    #[test]
    fn all_guides_fit_the_original_content_budget() {
        let guides = [
            FOUNDATION,
            LAYOUT,
            TYPE,
            COLOR,
            ASSETS,
            RESPONSIVE,
            INTERACTIONS,
            ACCESSIBILITY,
            REVIEW,
            GRAPHICS,
            DATAVIZ,
        ];
        let bytes: usize = guides.iter().map(|s| s.len()).sum();
        assert!(
            bytes.div_ceil(4) <= 1700,
            "original English guides total {bytes} bytes"
        );
    }
}
