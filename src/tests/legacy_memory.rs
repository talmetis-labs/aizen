//! Legacy Memory paths used only by isolated Core data-preservation tests.

use std::path::PathBuf;

pub(crate) fn root() -> PathBuf {
    crate::core::config::aizen_home().join("cli-memory")
}

pub(crate) fn entries_dir() -> PathBuf {
    root().join("entries")
}

pub(crate) fn frozen_active_path(slug: &str) -> PathBuf {
    root().join("core/active").join(format!("{slug}.md"))
}

pub(crate) fn graph_path() -> PathBuf {
    root().join("graph.tsv")
}
