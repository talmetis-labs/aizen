//! Narrow deterministic source provenance: self-contained, scriptless local HTML only.
//! Remote/dev-server/HMR renders remain unknown until a real build-source integration exists.
use super::*;
use std::io::Read;
use std::path::{Path, PathBuf};
const MAX_STATIC_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub(super) struct StaticSource {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub hash: String,
    pub url: String,
}

// Conservative allowlist, not a general HTML sanitizer. SVG is validated structurally first.
fn legacy_html(bytes: &[u8], reject_ids: bool) -> Result<()> {
    let text =
        std::str::from_utf8(bytes).context("deterministic static capture requires UTF-8 HTML")?;
    let lower = text.to_ascii_lowercase();
    for forbidden in [
        "script",
        "iframe",
        "object",
        "embed",
        "src",
        "href",
        "url",
        "@",
        "\\",
        "&",
        "<!--",
        "<![",
        "onload",
        "onclick",
        "onerror",
        "http:",
        "https:",
        "file:",
        "data:",
        "javascript:",
    ] {
        if lower.contains(forbidden) {
            bail!("static source contains dynamic or unproven external resources; source freshness remains unknown");
        }
    }
    let tags = [
        "html",
        "head",
        "title",
        "meta",
        "style",
        "body",
        "main",
        "section",
        "article",
        "header",
        "footer",
        "nav",
        "aside",
        "div",
        "span",
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "td",
        "th",
        "caption",
        "button",
        "input",
        "label",
        "textarea",
        "select",
        "option",
        "fieldset",
        "legend",
        "form",
        "a",
        "strong",
        "em",
        "b",
        "i",
        "small",
        "pre",
        "code",
        "blockquote",
        "hr",
        "br",
    ];
    let attrs = [
        "class",
        "id",
        "style",
        "type",
        "value",
        "name",
        "placeholder",
        "disabled",
        "checked",
        "selected",
        "role",
        "aria-label",
        "aria-hidden",
        "tabindex",
        "for",
        "charset",
        "lang",
    ];
    let mut rest = lower.as_str();
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest.find('>').context("unterminated static HTML tag")?;
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if tag.trim() == "!doctype html" {
            continue;
        }
        let tag = tag.trim().strip_prefix('/').unwrap_or(tag.trim()).trim();
        let name = tag
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        if !tags.contains(&name) {
            bail!("static source contains a non-allowlisted HTML tag");
        }
        let mut attributes = &tag[name.len()..];
        while !attributes.trim().trim_matches('/').is_empty() {
            attributes = attributes.trim_start();
            let length = attributes
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .unwrap_or(attributes.len());
            let name = &attributes[..length];
            if !attrs.contains(&name) || reject_ids && name == "id" {
                bail!("static source contains a non-allowlisted or conflicting HTML attribute");
            }
            attributes = &attributes[length..];
            attributes = attributes.trim_start();
            if let Some(value) = attributes.strip_prefix('=') {
                let value = value.trim_start();
                if let Some(quote) = value.chars().next().filter(|c| *c == '\'' || *c == '"') {
                    let after = &value[1..];
                    let end = after
                        .find(quote)
                        .context("unterminated static HTML attribute")?;
                    attributes = &after[end + 1..];
                } else {
                    let end = value.find(char::is_whitespace).unwrap_or(value.len());
                    attributes = &value[end..];
                }
            }
        }
    }
    Ok(())
}
/// Validate inline graphics before applying the original scriptless HTML contract to the rest.
/// External resources remain unsupported, so document byte verification covers every allowed input.
fn static_html(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_STATIC_BYTES {
        bail!("static source exceeds 256 KiB");
    }
    let text = std::str::from_utf8(bytes).context("static source requires UTF-8")?;
    let lower = text.to_ascii_lowercase();
    let mut scrubbed = String::new();
    let mut cursor = 0;
    let mut count = 0;
    while let Some(offset) = lower[cursor..].find("<svg") {
        let start = cursor + offset;
        if !lower
            .as_bytes()
            .get(start + 4)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
        {
            bail!("invalid inline SVG tag");
        }
        scrubbed.push_str(&text[cursor..start]);
        let end = validate_inline_svg(&text[start..])? + start;
        scrubbed.push_str("<div></div>");
        cursor = end;
        count += 1;
        if count > 1 {
            bail!("static source supports one inline SVG root to keep fragment IDs unambiguous");
        }
    }
    scrubbed.push_str(&text[cursor..]);
    legacy_html(scrubbed.as_bytes(), count > 0)
}

/// Restricted SVG parser: quoted attributes, no entities, comments, namespaces other than SVG,
/// dynamic tags or external URLs. Bound structure and prove local fragment dependency acyclicity.
fn validate_inline_svg(text: &str) -> Result<usize> {
    use std::collections::{BTreeMap, BTreeSet};
    const ELEMENTS: &[&str] = &[
        "svg",
        "g",
        "defs",
        "path",
        "rect",
        "circle",
        "ellipse",
        "line",
        "polyline",
        "polygon",
        "title",
        "desc",
        "linearGradient",
        "radialGradient",
        "stop",
        "mask",
        "clipPath",
        "use",
    ];
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut nodes: Vec<Vec<usize>> = Vec::new();
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut pending: Vec<(usize, String)> = Vec::new();
    let mut cursor = 0;
    let mut attributes_total = 0;
    loop {
        if cursor >= text.len() {
            bail!("unterminated inline SVG");
        }
        if !text[cursor..].starts_with('<') {
            let end = text[cursor..]
                .find('<')
                .map(|n| cursor + n)
                .context("unterminated SVG text")?;
            let content = &text[cursor..end];
            if content.contains(['&', '>'])
                || (!content.trim().is_empty()
                    && !stack
                        .last()
                        .is_some_and(|(name, _)| name == "title" || name == "desc"))
            {
                bail!("SVG permits plain title/description text only");
            }
            cursor = end;
        }
        let start = cursor;
        cursor += 1;
        let close = text[cursor..].starts_with('/');
        if close {
            cursor += 1;
        }
        let name_start = cursor;
        while text
            .as_bytes()
            .get(cursor)
            .is_some_and(u8::is_ascii_alphabetic)
        {
            cursor += 1;
        }
        let name = &text[name_start..cursor];
        if !ELEMENTS.contains(&name) {
            bail!("non-allowlisted SVG element/namespace");
        }
        if close {
            while text
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                cursor += 1;
            }
            if text.as_bytes().get(cursor) != Some(&b'>') {
                bail!("invalid SVG closing tag");
            }
            cursor += 1;
            if stack.pop().is_none_or(|(opened, _)| opened != name) {
                bail!("mismatched SVG nesting");
            }
            if stack.is_empty() {
                for (from, fragment) in &pending {
                    let target = *ids
                        .get(fragment)
                        .context("SVG local fragment target missing")?;
                    nodes[*from].push(target);
                }
                // Iterative DFS: reference + containment dependency graph, no recursive stack risk.
                for root in 0..nodes.len() {
                    let mut active = BTreeSet::new();
                    let mut visited = BTreeSet::new();
                    let mut walk = vec![(root, false)];
                    let mut expanded = vec![0usize; nodes.len()];
                    let mut depth = vec![0usize; nodes.len()];
                    while let Some((node, exit)) = walk.pop() {
                        if exit {
                            active.remove(&node);
                            visited.insert(node);
                            let mut size = 1usize;
                            let mut level = 1usize;
                            for child in &nodes[node] {
                                size = size.saturating_add(expanded[*child]);
                                level = level.max(depth[*child].saturating_add(1));
                            }
                            if size > 32_768 || level > 32 {
                                bail!("SVG expanded dependency budget exceeded");
                            }
                            expanded[node] = size;
                            depth[node] = level;
                            continue;
                        }
                        if visited.contains(&node) {
                            continue;
                        }
                        if !active.insert(node) {
                            bail!("cyclic SVG local-fragment dependency");
                        }
                        walk.push((node, true));
                        for child in nodes[node].iter().rev() {
                            walk.push((*child, false));
                        }
                    }
                }
                return Ok(cursor);
            }
            continue;
        }
        if nodes.len() >= 2048 || stack.len() >= 32 {
            bail!("SVG structure budget exceeded");
        }
        if nodes.is_empty() && name != "svg" {
            bail!("SVG root required");
        }
        if !nodes.is_empty() && name == "svg" {
            bail!("nested SVG roots unsupported");
        }
        let node = nodes.len();
        nodes.push(Vec::new());
        if let Some((_, parent)) = stack.last() {
            nodes[*parent].push(node);
        }
        let mut seen = BTreeSet::new();
        let mut self_close = false;
        loop {
            while text
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                cursor += 1;
            }
            if text[cursor..].starts_with("/>") {
                self_close = true;
                cursor += 2;
                break;
            }
            if text[cursor..].starts_with('>') {
                cursor += 1;
                break;
            }
            let attr_start = cursor;
            while text
                .as_bytes()
                .get(cursor)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-')
            {
                cursor += 1;
            }
            let attr = &text[attr_start..cursor];
            if attr.is_empty() || !seen.insert(attr) {
                bail!("invalid/duplicate SVG attribute");
            }
            attributes_total += 1;
            if attributes_total > 8192 {
                bail!("SVG attribute budget exceeded");
            }
            while text
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                cursor += 1;
            }
            if text.as_bytes().get(cursor) != Some(&b'=') {
                bail!("SVG attributes must have quoted values");
            }
            cursor += 1;
            while text
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                cursor += 1;
            }
            let quote = *text
                .as_bytes()
                .get(cursor)
                .context("unterminated SVG attribute")?;
            if quote != b'\'' && quote != b'"' {
                bail!("SVG attribute requires quotes");
            }
            cursor += 1;
            let end = text[cursor..]
                .find(quote as char)
                .map(|n| cursor + n)
                .context("unterminated SVG attribute value")?;
            let value = &text[cursor..end];
            cursor = end + 1;
            if value.len() > 64 * 1024
                || value.contains(['<', '>', '&', '\\'])
                || value.chars().any(char::is_control)
            {
                bail!("invalid bounded SVG attribute data");
            }
            let common = [
                "id",
                "fill",
                "stroke",
                "stroke-width",
                "opacity",
                "fill-opacity",
                "stroke-opacity",
                "fill-rule",
                "clip-rule",
                "stroke-linecap",
                "stroke-linejoin",
                "stroke-miterlimit",
                "transform",
                "clip-path",
                "mask",
                "role",
                "aria-label",
                "aria-hidden",
            ];
            let specific: &[&str] = match name {
                "svg" => &["xmlns", "viewBox", "width", "height", "preserveAspectRatio"],
                "path" => &["d"],
                "rect" => &["x", "y", "width", "height", "rx", "ry"],
                "circle" => &["cx", "cy", "r"],
                "ellipse" => &["cx", "cy", "rx", "ry"],
                "line" => &["x1", "y1", "x2", "y2"],
                "polyline" | "polygon" => &["points"],
                "linearGradient" => &[
                    "x1",
                    "y1",
                    "x2",
                    "y2",
                    "gradientUnits",
                    "gradientTransform",
                    "spreadMethod",
                    "href",
                ],
                "radialGradient" => &[
                    "cx",
                    "cy",
                    "r",
                    "fx",
                    "fy",
                    "fr",
                    "gradientUnits",
                    "gradientTransform",
                    "spreadMethod",
                    "href",
                ],
                "stop" => &["offset", "stop-color", "stop-opacity"],
                "mask" => &["x", "y", "width", "height", "maskUnits", "maskContentUnits"],
                "clipPath" => &["clipPathUnits"],
                "use" => &["href", "x", "y", "width", "height"],
                _ => &[],
            };
            if !common.contains(&attr) && !specific.contains(&attr) {
                bail!("non-allowlisted SVG attribute/handler/namespace");
            }
            if attr == "xmlns" {
                if value != "http://www.w3.org/2000/svg" {
                    bail!("SVG namespace must be exact");
                }
            } else if attr == "id" {
                if value.is_empty()
                    || value.len() > 128
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    || ids.insert(value.to_string(), node).is_some()
                {
                    bail!("invalid/duplicate SVG ID");
                }
            } else if attr == "href"
                || ["fill", "stroke", "clip-path", "mask"].contains(&attr)
                    && value.starts_with("url(")
            {
                let fragment = if attr == "href" {
                    value.strip_prefix('#')
                } else {
                    value
                        .strip_prefix("url(#")
                        .and_then(|v| v.strip_suffix(')'))
                }
                .context("SVG references must be literal local fragments")?;
                if fragment.is_empty()
                    || fragment.len() > 128
                    || !fragment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    bail!("invalid SVG fragment");
                }
                pending.push((node, fragment.to_string()));
                if pending.len() > 4096 {
                    bail!("SVG reference budget exceeded");
                }
            } else {
                // No CSS grammar, escapes or functions except finite geometry transforms.
                let lower = value.to_ascii_lowercase();
                if lower.contains([':', '@', ';'])
                    || lower.contains("url")
                    || lower.contains("var(")
                    || lower.contains("calc(")
                    || lower.contains("context-")
                {
                    bail!("dynamic/external SVG attribute value");
                }
                if (attr == "d"
                    || attr == "points"
                    || [
                        "viewBox",
                        "x",
                        "y",
                        "x1",
                        "x2",
                        "y1",
                        "y2",
                        "cx",
                        "cy",
                        "r",
                        "rx",
                        "ry",
                        "fx",
                        "fy",
                        "fr",
                        "width",
                        "height",
                        "stroke-width",
                        "opacity",
                        "fill-opacity",
                        "stroke-opacity",
                        "offset",
                        "stop-opacity",
                    ]
                    .contains(&attr))
                    && !value.bytes().all(|b| {
                        b.is_ascii_digit()
                            || b.is_ascii_whitespace()
                            || b".,+-eE%MmZzLlHhVvCcSsQqTtAa".contains(&b)
                    })
                {
                    bail!("unsupported SVG numeric/path grammar");
                }
                if attr == "transform" || attr == "gradientTransform" {
                    let mut expr = value;
                    while !expr.trim().is_empty() {
                        expr = expr.trim_start();
                        let end = expr.find('(').context("invalid SVG transform")?;
                        if !["matrix", "translate", "scale", "rotate", "skewX", "skewY"]
                            .contains(&&expr[..end])
                        {
                            bail!("unsupported SVG transform");
                        }
                        let close = expr[end + 1..]
                            .find(')')
                            .map(|n| end + 1 + n)
                            .context("unclosed SVG transform")?;
                        if !expr[end + 1..close].bytes().all(|b| {
                            b.is_ascii_digit() || b.is_ascii_whitespace() || b".,+-eE".contains(&b)
                        }) {
                            bail!("invalid SVG transform numbers");
                        }
                        expr = &expr[close + 1..];
                    }
                } else if value.contains(['(', ')']) {
                    bail!("SVG value functions unsupported");
                }
            }
        }
        if self_close {
            if node == 0 {
                return Ok(cursor);
            }
        } else {
            stack.push((name.to_string(), node));
        }
        if cursor <= start {
            bail!("invalid SVG parser progress");
        }
    }
}

fn read_static(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).context("reading deterministic static source")?;
    if file.metadata()?.len() > MAX_STATIC_BYTES as u64 {
        bail!("static source exceeds 256 KiB");
    }
    let mut bytes = Vec::new();
    file.take((MAX_STATIC_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STATIC_BYTES {
        bail!("static source exceeds 256 KiB");
    }
    static_html(&bytes)?;
    Ok(bytes)
}

pub(super) async fn local_source(
    c: &mut CdpClient,
    workspace: &str,
    url: &str,
) -> Result<StaticSource> {
    let parsed = super::navigate_url(url)?;
    if parsed.scheme() != "file" || parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("deterministic static provenance requires an unambiguous local file URL");
    }
    let path = parsed
        .to_file_path()
        .map_err(|_| anyhow!("not a local file URL"))?;
    let workspace = PathBuf::from(workspace);
    let budget = c.budget.clone();
    budget
        .run(async move {
            tokio::task::spawn_blocking(move || {
                let root = workspace
                    .canonicalize()
                    .context("canonicalizing design workspace")?;
                let path = path.canonicalize().context("canonicalizing static file")?;
                if !path.starts_with(&root) {
                    bail!("static source is outside the design workspace");
                }
                if !path.extension().is_some_and(|e| {
                    e.to_str().is_some_and(|e| {
                        e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm")
                    })
                }) {
                    bail!("static provenance requires HTML");
                }
                let bytes = read_static(&path)?;
                let hash = crate::core::tool_media::sha256(&bytes);
                Ok(StaticSource {
                    path,
                    bytes,
                    hash,
                    url: String::new(),
                })
            })
            .await
            .context("static source read task failed")?
        })
        .await
        .map(|mut source| {
            source.url = url.to_string();
            source
        })
}

pub(super) async fn confirm_served(c: &mut CdpClient, source: &StaticSource) -> Result<()> {
    let tree = c.call("Page.getFrameTree", json!({})).await?;
    let frame = &tree["frameTree"]["frame"];
    if frame["url"].as_str() != Some(source.url.as_str())
        || tree["frameTree"]["childFrames"]
            .as_array()
            .is_some_and(|children| !children.is_empty())
    {
        bail!("static capture frame does not match the source file or has child frames");
    }
    let id = frame["id"]
        .as_str()
        .context("static page frame id missing")?;
    let resource = c
        .call(
            "Page.getResourceContent",
            json!({"frameId":id,"url":source.url}),
        )
        .await?;
    if resource["base64Encoded"].as_bool() != Some(false)
        || resource["content"].as_str().map(str::as_bytes) != Some(source.bytes.as_slice())
    {
        bail!("browser document bytes do not match the deterministic workspace source");
    }
    let path = source.path.clone();
    let expected = source.bytes.clone();
    c.budget
        .run(async move {
            tokio::task::spawn_blocking(move || {
                if read_static(&path)? != expected {
                    bail!("workspace static source changed during capture");
                }
                Ok(())
            })
            .await
            .context("static source verification task failed")?
        })
        .await
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_self_contained_static_html_gets_source_provenance() {
        assert!(static_html(b"<!doctype html><html lang='en'><head><style>body { color: red; }</style></head><body><main class='card'><h1>Title</h1><button disabled>Go</button></main></body></html>").is_ok());
        assert!(static_html(b"<!doctype html><html><body><svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'><defs><linearGradient id='paint'><stop offset='0%' stop-color='#f00'/><stop offset='100%' stop-color='#00f'/></linearGradient><mask id='cut'><rect width='10' height='10' fill='white'/></mask></defs><path d='M0 0L10 0L10 10Z' fill='url(#paint)' mask='url(#cut)'/></svg></body></html>").is_ok());
        for source in [
            "<script>x</script>",
            "<img src='x'>",
            "<style>@import 'x';</style>",
            "<div style='background:URL(x)'>x</div>",
            "<div onclick='x'>x</div>",
            "<svg><script/></svg>",
            "<a href='x'>link</a>",
            "<div>&#60;script</div>",
            "<!--x-->",
            "<meta http-equiv='refresh' content='0'>",
            "<svg xmlns='http://bad'><path d='M0 0Z'/></svg>",
            "<svg xmlns:xlink='http://www.w3.org/1999/xlink'><use xlink:href='#x'/></svg>",
            "<svg><foreignObject/></svg>",
            "<svg onload='x'><path d='M0 0Z'/></svg>",
            "<svg><animate attributeName='x'/></svg>",
            "<svg><use href='https://example.invalid/x.svg#x'/></svg>",
            "<svg><path fill='url(https://example.invalid/x)'/></svg>",
            "<svg><path fill='URL(#x)'/></svg>",
            "<svg><path style='fill:red'/></svg>",
            "<svg><path fill='var(--paint)'/></svg>",
            "<svg><use href='#missing'/></svg>",
            "<svg><g id='a'><use href='#a'/></g></svg>",
            "<svg><defs><linearGradient id='a' href='#b'/><linearGradient id='b' href='#a'/></defs></svg>",
            "<svg><path id='x'/><path id='x'/></svg>",
            "<svg><path d='M0 0Z'></rect></svg>",
            "<svg><title>&#60;script</title></svg>",
            "<svg><path d='M0 0Z' unknown='x'/></svg>",
            "<svg><path d='M0 0Z' fill='red' fill='blue'/></svg>",
            "<div id='a'></div><svg><use href='#a'/></svg>",
            "<svg><path id='a'/></svg><svg><use href='#a'/></svg>",
        ] {
            assert!(static_html(source.as_bytes()).is_err(), "{source}");
        }
    }
}
