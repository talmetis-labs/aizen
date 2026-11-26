//! Shared Aizen Desktop release v1 contract. The Desktop build uses this module too.

use serde::{Deserialize, Serialize};

pub const MAX_RELEASE_BYTES: usize = 32 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Release {
    pub schema_version: u32,
    pub slug: String,
    pub version: String,
    pub desktop: Desktop,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Desktop {
    pub ui_api_version: u32,
    pub views: Vec<View>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "snake_case")]
pub enum View {
    Board {
        id: String,
        title: String,
        columns: Vec<Column>,
    },
    Calculator {
        id: String,
        title: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub id: String,
    pub title: String,
}

pub fn identifier(value: &str) -> bool {
    (2..=64).contains(&value.len())
        && value.as_bytes()[0] != b'-'
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn label(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 160
        && !value.chars().any(|c| {
            c.is_control() || c == '\u{fffd}'
                || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

fn version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    value.len() <= 32
        && parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && (*part == "0" || !part.starts_with('0'))
                && part.bytes().all(|b| b.is_ascii_digit())
        })
}

pub fn parse(bytes: &[u8]) -> Result<Release, String> {
    if bytes.len() > MAX_RELEASE_BYTES {
        return Err("aizen-release.json exceeds 32 KiB".to_string());
    }
    let release: Release = serde_json::from_slice(bytes).map_err(|e| {
        format!(
            "Invalid release JSON near line {}, column {}",
            e.line(),
            e.column()
        )
    })?;
    if release.schema_version != 1 || release.desktop.ui_api_version != 1 {
        return Err("Unsupported schemaVersion or desktop.uiApiVersion".to_string());
    }
    if !identifier(&release.slug) || !version(&release.version) {
        return Err("Use a lowercase slug and a version in X.Y.Z format".to_string());
    }
    if !(1..=6).contains(&release.desktop.views.len()) {
        return Err("desktop.views must contain 1 to 6 views".to_string());
    }
    let mut ids = std::collections::HashSet::new();
    for view in &release.desktop.views {
        let (id, title) = match view {
            View::Board { id, title, columns } => {
                let mut column_ids = std::collections::HashSet::new();
                if !(2..=8).contains(&columns.len())
                    || columns.iter().any(|column| {
                        !identifier(&column.id)
                            || !label(&column.title)
                            || !column_ids.insert(&column.id)
                    })
                {
                    return Err(
                        "A board requires 2 to 8 unique columns with valid IDs and titles"
                            .to_string(),
                    );
                }
                (id, title)
            }
            View::Calculator { id, title } => (id, title),
        };
        if !identifier(id) || !label(title) || !ids.insert(id) {
            return Err("Each view requires a unique ID and a valid title".to_string());
        }
    }
    Ok(release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_bundle_and_rejects_unknown_or_ambiguous_fields() {
        let sample = include_str!("../../examples/desktop-plugins/aizen-release.json");
        assert_eq!(parse(sample.as_bytes()).unwrap().desktop.views.len(), 2);
        for bad in [
            sample.replace(
                "\"schemaVersion\": 1",
                "\"schemaVersion\": 1,\"schemaVersion\": 1",
            ),
            sample.replace(
                "\"type\": \"board\"",
                "\"type\": \"board\", \"entry\": \"remote.html\"",
            ),
            sample.replace("\"done\"", "\"todo\""),
            sample.replace("\"1.0.0\"", "\"1.00.0\""),
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }
}
