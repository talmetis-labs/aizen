// Copyright 2026 Aizen authors. Licensed under Apache-2.0.
// Extracted from Aizen; original implementation comments and tests retained below.

//! Non-destructive archive name selection shared by Core-owned documents.

use std::path::{Path, PathBuf};

/// A free `{id}.md` path under `dir`, appending `-2`, `-3`, … on collision.
///
/// Shared by every archive path in the tree (memory facts, persona self-memory, the review
/// queue) so that "move aside instead of delete" can never silently overwrite an earlier
/// archive file that happens to have the same stem.
pub fn unique_in(dir: &Path, id: &str) -> PathBuf {
    let p = dir.join(format!("{id}.md"));
    if !p.exists() {
        return p;
    }
    for n in 2..10_000 {
        let c = dir.join(format!("{id}-{n}.md"));
        if !c.exists() {
            return c;
        }
    }
    p
}
