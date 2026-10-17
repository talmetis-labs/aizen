//! Typed skill metadata with a lossless source document. Memory's lightweight parser is unrelated.
use anyhow::{bail, Context, Result};
use yaml_rust2::{Yaml, YamlLoader};

#[derive(Debug, Clone, PartialEq)]
pub struct SkillDocument {
    pub raw: String,
    pub(crate) body_start: usize,
    header_start: usize,
    header_end: usize,
    fields: Yaml,
}

impl SkillDocument {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() > 16 * 1024 * 1024 {
            bail!("skill document exceeds 16 MiB");
        }
        let mut start = raw
            .strip_prefix('\u{feff}')
            .map_or(0, |_| '\u{feff}'.len_utf8());
        while raw[start..].starts_with('\n') || raw[start..].starts_with("\r\n") {
            start += if raw[start..].starts_with("\r\n") {
                2
            } else {
                1
            };
        }
        let mut header_start = 0;
        let mut header_end = 0;
        let mut body_start = 0;
        let mut fields = Yaml::Hash(Default::default());
        if let Some(first) = raw[start..].split_inclusive('\n').next() {
            if first.trim_end_matches(['\r', '\n']) == "---" {
                header_start = start + first.len();
                let mut pos = header_start;
                let mut closed = false;
                for line in raw[header_start..].split_inclusive('\n') {
                    if line.trim_end_matches(['\r', '\n']) == "---" {
                        header_end = pos;
                        body_start = pos + line.len();
                        closed = true;
                        break;
                    }
                    pos += line.len();
                }
                if !closed {
                    bail!("skill frontmatter has no closing --- fence");
                }
                let header = &raw[header_start..header_end];
                if header.len() > 256 * 1024 {
                    bail!("skill YAML frontmatter exceeds 256 KiB");
                }
                // Reject anchors/aliases before the loader can expand an alias bomb. Quoted values
                // and comments are untouched; ordinary Markdown bodies are never inspected here.
                reject_aliases(header)?;
                let docs =
                    YamlLoader::load_from_str(header).context("invalid skill YAML frontmatter")?;
                if docs.len() > 1 {
                    bail!("skill frontmatter must be one YAML mapping");
                }
                if let Some(doc) = docs.into_iter().next() {
                    if !matches!(doc, Yaml::Hash(_) | Yaml::Null) {
                        bail!("skill frontmatter must be a YAML mapping");
                    }
                    if !matches!(doc, Yaml::Null) {
                        fields = doc;
                    }
                }
            }
        }
        let document = Self {
            raw: raw.to_string(),
            body_start,
            header_start,
            header_end,
            fields,
        };
        for field in ["name", "description", "when", "updated"] {
            document.string(field)?;
        }
        for field in ["requires", "platforms"] {
            document.list(field)?;
        }
        Ok(document)
    }

    pub(crate) fn string(&self, key: &str) -> Result<Option<&str>> {
        match &self.fields[key] {
            Yaml::BadValue | Yaml::Null => Ok(None),
            Yaml::String(s) => Ok(Some(s)),
            _ => bail!("skill '{key}' metadata must be a string"),
        }
    }

    pub(crate) fn list(&self, key: &str) -> Result<Vec<String>> {
        match &self.fields[key] {
            Yaml::BadValue | Yaml::Null => Ok(Vec::new()),
            Yaml::String(s) => Ok(super::parse_list(s)),
            Yaml::Array(values) => values
                .iter()
                .map(|v| match v {
                    Yaml::String(s) => Ok(s.trim().to_string()),
                    _ => bail!("skill '{key}' metadata must contain only strings"),
                })
                .filter(|v| !matches!(v, Ok(s) if s.is_empty()))
                .collect(),
            _ => bail!("skill '{key}' metadata must be a string or string sequence"),
        }
    }

    pub(crate) fn counter(&self, key: &str, fallback: u32) -> u32 {
        match &self.fields[key] {
            Yaml::Integer(n) => u32::try_from(*n).unwrap_or(fallback),
            Yaml::String(s) => s.trim().parse().unwrap_or(fallback),
            _ => fallback,
        }
    }

    pub(crate) fn body(&self) -> String {
        self.raw[self.body_start..]
            .trim_start_matches(['\r', '\n'])
            .trim_end()
            .replace("\r\n", "\n")
    }

    /// Replace the body only unless a requested known value actually changed. Untouched headers,
    /// including comments, nested keys, unknown fields and CRLF, remain byte-for-byte identical.
    pub(crate) fn refined(
        &self,
        body: &str,
        description: Option<&str>,
        when: Option<&str>,
    ) -> Result<String> {
        let mut patches = Vec::new();
        for (key, value) in [("description", description), ("when", when)] {
            if let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) {
                if self.string(key)? != Some(value) {
                    patches.push((key, value));
                }
            }
        }
        let newline = if self.raw[..self.body_start].contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut prefix = self.raw[..self.body_start].to_string();
        if !patches.is_empty() {
            if self.body_start == 0 {
                prefix = format!("---{newline}");
                for (key, value) in patches {
                    prefix.push_str(&format!("{key}: {}{newline}", quote(value)));
                }
                prefix.push_str(&format!("---{newline}"));
            } else {
                let mut header = self.raw[self.header_start..self.header_end].to_string();
                for (key, value) in patches {
                    header = patch_field(&header, key, value, newline)?;
                }
                prefix = format!(
                    "{}{}{}",
                    &self.raw[..self.header_start],
                    header,
                    &self.raw[self.header_end..self.body_start]
                );
            }
        }
        let output = format!("{prefix}{}{newline}", body.trim());
        Self::parse(&output)?;
        Ok(output)
    }
}

pub(crate) fn quote(value: &str) -> String {
    // JSON double-quoted strings are valid YAML and avoid colon/hash/multiline ambiguities.
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn reject_aliases(header: &str) -> Result<()> {
    // yaml-rust2's scanner identifies aliases precisely, including flow/nested values. Parsing an
    // alias token is unnecessary for skill metadata and potentially expands unbounded trees.
    use yaml_rust2::scanner::{Scanner, TokenType};
    let mut scanner = Scanner::new(header.chars());
    let mut depth = 0usize;
    let mut nodes = 0usize;
    for token in scanner.by_ref() {
        nodes += 1;
        if nodes > 8192 {
            bail!("skill YAML exceeds 8192 tokens");
        }
        match token.1 {
            TokenType::Alias(_) | TokenType::Anchor(_) => {
                bail!("skill YAML anchors and aliases are not supported")
            }
            TokenType::BlockSequenceStart
            | TokenType::BlockMappingStart
            | TokenType::FlowSequenceStart
            | TokenType::FlowMappingStart => {
                depth += 1;
                if depth > 32 {
                    bail!("skill YAML exceeds nesting depth 32");
                }
            }
            TokenType::BlockEnd | TokenType::FlowSequenceEnd | TokenType::FlowMappingEnd => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    if let Some(error) = scanner.get_error() {
        bail!("invalid skill YAML frontmatter: {error}");
    }
    Ok(())
}

/// Identify only root mapping entries. Scalar continuations and nested mappings are indented and
/// cannot be mistaken for keys; a root flow mapping refuses edits rather than losing unknown data.
fn patch_field(header: &str, key: &str, value: &str, newline: &str) -> Result<String> {
    let mut entries = Vec::new();
    let mut pos = 0;
    for line in header.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if !text.is_empty() && !text.starts_with([' ', '\t', '#']) {
            let Some((candidate, _)) = text.split_once(':') else {
                bail!("cannot losslessly patch this YAML mapping; edit the document directly");
            };
            let candidate = candidate.trim().trim_matches(['\'', '"']);
            if candidate.starts_with(['{', '[', '?']) {
                bail!("cannot losslessly patch a flow/complex YAML mapping; edit the document directly");
            }
            entries.push((pos, candidate.to_string()));
        }
        pos += line.len();
    }
    let matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, (_, k))| k == key)
        .collect();
    if matches.len() > 1 {
        bail!("duplicate skill metadata key '{key}'");
    }
    let replacement = format!("{key}: {}{newline}", quote(value));
    if let Some((index, (start, _))) = matches.first() {
        let mut end = entries
            .get(index + 1)
            .map_or(header.len(), |(offset, _)| *offset);
        // Root comments/blank lines between entries are not part of a scalar continuation. Keep
        // them even when replacing a folded/literal known field. Indented scalar content stays
        // attached to the field being replaced.
        let mut offset = *start;
        let mut preserve_from = end;
        for line in header[*start..end].split_inclusive('\n') {
            let text = line.trim_end_matches(['\r', '\n']);
            if text.starts_with('#') || text.is_empty() {
                if preserve_from == end {
                    preserve_from = offset;
                }
            } else {
                preserve_from = end;
            }
            offset += line.len();
        }
        end = preserve_from;
        Ok(format!(
            "{}{}{}",
            &header[..*start],
            replacement,
            &header[end..]
        ))
    } else {
        Ok(format!(
            "{header}{}{replacement}",
            if header.is_empty() || header.ends_with('\n') {
                ""
            } else {
                newline
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_typed_view_preserves_source_and_nested_unknowns() {
        let raw = "---\r\nname: 'upstream'\r\ndescription: >-\r\n  Folded text\r\n  with a colon: yes\r\nrequires: [shell_run, file_read]\r\nplatforms:\r\n  - windows\r\nunknown:\r\n  nested: [a, b]\r\n---\r\n\r\nsteps\r\n";
        let doc = SkillDocument::parse(raw).unwrap();
        assert_eq!(doc.raw, raw);
        assert_eq!(
            doc.string("description").unwrap(),
            Some("Folded text with a colon: yes")
        );
        assert_eq!(doc.list("requires").unwrap(), ["shell_run", "file_read"]);
        assert_eq!(doc.list("platforms").unwrap(), ["windows"]);
        let changed = doc.refined("new steps", None, None).unwrap();
        assert!(changed.starts_with(&raw[..doc.body_start]));
        let patched = doc
            .refined("new steps", Some("new: # quoted"), None)
            .unwrap();
        assert!(patched.contains("unknown:\r\n  nested: [a, b]\r\n"));
        assert_eq!(
            SkillDocument::parse(&patched)
                .unwrap()
                .string("description")
                .unwrap(),
            Some("new: # quoted")
        );
    }

    #[test]
    fn literal_scalar_and_string_guards() {
        let doc = SkillDocument::parse("---\ndescription: |\n  line one\n  description: not a key\nrequires: 'a, b; c'\n---\nbody").unwrap();
        assert_eq!(
            doc.string("description").unwrap(),
            Some("line one\ndescription: not a key\n")
        );
        let patched = doc.refined("body", Some("new"), None).unwrap();
        assert_eq!(
            SkillDocument::parse(&patched)
                .unwrap()
                .list("requires")
                .unwrap(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn malformed_types_and_aliases_refuse() {
        let raw = format!(
            "---\nunknown: {}x{}\n---\nbody",
            "[".repeat(64),
            "]".repeat(64)
        );
        assert!(SkillDocument::parse(&raw).is_err());
        let raw = "---\nname: first\nname: second\n---\nbody";
        assert!(SkillDocument::parse(raw).is_err());
        for raw in [
            "---\ndescription: [x]\n---\nbody",
            "---\nrequires: [x, 3]\n---\nbody",
            "---\nx: &a [x]\ny: *a\n---\nbody",
            "---\nname: unfinished",
        ] {
            assert!(SkillDocument::parse(raw).is_err(), "{raw}");
        }
    }
}
