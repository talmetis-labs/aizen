//! Controller-approved exact-file authority for bounded native children.
//!
//! Scope is immutable execution state, never inferred from a child's prompt or tool JSON.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::core::workspace_txn::NativeResource;

const MAX_PATHS: usize = 32;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub(crate) struct NativeWriteScope {
    /// Readable union, including every writable resource.
    resources: Vec<NativeResource>,
    writes: Vec<String>,
}

impl NativeWriteScope {
    pub(crate) fn resolve(root: &Path, paths: &[String]) -> Result<Self> {
        Self::resolve_access(root, &[], paths)
    }

    pub(crate) fn resolve_access(root: &Path, reads: &[String], writes: &[String]) -> Result<Self> {
        if reads.len() > MAX_PATHS
            || writes.len() > MAX_PATHS
            || reads.is_empty() && writes.is_empty()
        {
            bail!("read_paths/write_paths requires 1..=32 distinct exact files, never directories or globs");
        }
        let mut resources: Vec<NativeResource> = Vec::new();
        let mut spellings: Vec<String> = Vec::new();
        let mut write_keys = Vec::with_capacity(writes.len());
        for (paths, writable) in [(reads, false), (writes, true)] {
            let mut seen = std::collections::HashSet::new();
            for name in paths {
                if name.is_empty()
                    || name.len() > MAX_PATH_BYTES
                    || name.trim() != name
                    || name.chars().any(char::is_control)
                {
                    bail!("read_paths/write_paths requires bounded non-empty exact file paths");
                }
                let path = Path::new(name);
                NativeResource::validate_spelling(path)?;
                let absolute = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                };
                let resource = NativeResource::resolve(&absolute)
                    .with_context(|| format!("native scope cannot bind exact file {name}"))?;
                if !seen.insert(resource.key().to_owned()) {
                    bail!("read_paths/write_paths contains duplicate or aliased files");
                }
                if writable {
                    write_keys.push(resource.key().to_owned());
                }
                if let Some(index) = resources.iter().position(|r| r.key() == resource.key()) {
                    if spellings[index] != *name {
                        bail!("read_paths/write_paths contains aliased files");
                    }
                } else {
                    resources.push(resource);
                    spellings.push(name.clone());
                }
            }
        }
        if resources.len() > MAX_PATHS {
            bail!("read_paths/write_paths union exceeds 32 distinct exact files");
        }
        resources.sort_by(|a, b| a.key().cmp(b.key()));
        write_keys.sort();
        Ok(Self {
            resources,
            writes: write_keys,
        })
    }

    pub(crate) fn check_read(&self, resource: &NativeResource) -> Result<()> {
        resource.verify()?;
        self.resources.iter().find(|r| r.key() == resource.key())
            .context("native child read is outside controller-approved read_paths/write_paths; nothing read")?
            .verify()
    }

    pub(crate) fn check_resource(&self, resource: &NativeResource) -> Result<()> {
        self.check_read(resource)?;
        if !self.writes.iter().any(|key| key == resource.key()) {
            bail!("native child write is outside controller-approved write_paths; nothing written");
        }
        Ok(())
    }

    pub(crate) fn check_narrowing(&self, child: &Self) -> Result<()> {
        for resource in &child.resources {
            self.check_read(resource)?;
            if child.writes.iter().any(|key| key == resource.key()) {
                self.check_resource(resource)?;
            }
        }
        Ok(())
    }

    /// Compare sealed authority only. Overlay IO must not inspect the mutable live tree.
    pub(crate) fn check_narrowing_frozen(&self, child: &Self) -> Result<()> {
        for resource in &child.resources {
            if !self.resources.iter().any(|r| r.key() == resource.key()) {
                bail!("native child read widens its sealed authority");
            }
        }
        if child.writes.iter().any(|key| !self.writes.contains(key)) {
            bail!("native child write widens its sealed authority");
        }
        Ok(())
    }

    pub(crate) fn resources(&self) -> &[NativeResource] {
        &self.resources
    }

    pub(crate) fn write_keys(&self) -> &[String] {
        &self.writes
    }

    pub(crate) fn has_writes(&self) -> bool {
        !self.writes.is_empty()
    }

    pub(crate) fn same_authority(&self, other: &Self) -> bool {
        self.writes == other.writes
            && self
                .resources
                .iter()
                .map(NativeResource::key)
                .eq(other.resources.iter().map(NativeResource::key))
    }

    pub(crate) fn conflicts(&self, other: &Self) -> bool {
        self.writes
            .iter()
            .any(|key| other.resources.iter().any(|r| r.key() == key))
            || other
                .writes
                .iter()
                .any(|key| self.resources.iter().any(|r| r.key() == key))
    }

    pub(crate) fn prompt(&self) -> String {
        let quote = |r: &NativeResource| {
            serde_json::to_string(&r.path().display().to_string())
                .unwrap_or_default()
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
        };
        let reads = self
            .resources
            .iter()
            .map(&quote)
            .collect::<Vec<_>>()
            .join("\n");
        let writes = self
            .resources
            .iter()
            .filter(|r| self.writes.iter().any(|key| key == r.key()))
            .map(quote)
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n<native_scope>\nOnly bounded native file tools are granted. Read ONLY these exact approved files:\n{reads}\nWrite ONLY these exact approved files (empty means read-only):\n{writes}\nNo shell, build, process, LSP, search, move, delete, delegation, restore or undo. Read back changed files; report build/test checks to the coordinating parent rather than bypassing missing capabilities.\n</native_scope>\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_scope_prompt_quotes_paths_and_refuses_controls() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        assert!(NativeWriteScope::resolve(temp.path(), &["evil\nfile.rs".into()]).is_err());
        assert!(NativeWriteScope::resolve(temp.path(), &["evil\u{1b}file.rs".into()]).is_err());
        let scope = NativeWriteScope::resolve(temp.path(), &["owned.rs".into()]).unwrap();
        let prompt = scope.prompt();
        assert_eq!(prompt.matches("<native_scope>").count(), 1);
        assert_eq!(prompt.matches("</native_scope>").count(), 1);
        let path = serde_json::to_string(&scope.resources()[0].path().display().to_string())
            .unwrap()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        assert!(prompt.lines().any(|line| line == path));
    }

    #[test]
    fn exact_read_write_modes_conflict_and_narrow_without_upgrades() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let read = NativeWriteScope::resolve_access(temp.path(), &["a.rs".into()], &[]).unwrap();
        let write = NativeWriteScope::resolve(temp.path(), &["a.rs".into()]).unwrap();
        let other = NativeWriteScope::resolve(temp.path(), &["b.rs".into()]).unwrap();
        assert!(!read.has_writes());
        assert!(!read.conflicts(&read));
        assert!(read.conflicts(&write));
        assert!(write.conflicts(&read));
        assert!(!read.conflicts(&other));
        assert!(!read.same_authority(&write));
        assert!(write.check_narrowing(&read).is_ok());
        assert!(read.check_narrowing(&write).is_err());
        assert!(read.check_read(&write.resources()[0]).is_ok());
        assert!(read.check_resource(&write.resources()[0]).is_err());
        let mixed =
            NativeWriteScope::resolve_access(temp.path(), &["a.rs".into()], &["b.rs".into()])
                .unwrap();
        assert!(mixed.check_narrowing(&read).is_ok());
        assert!(mixed.check_narrowing(&other).is_ok());
        assert!(mixed.check_narrowing(&write).is_err());
        assert!(!mixed.conflicts(&read));
        assert!(mixed.conflicts(&write));
        assert!(mixed.conflicts(&other));
    }

    #[test]
    fn exact_access_caps_the_union_and_refuses_alias_spellings() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let paths: Vec<_> = (0..32).map(|n| format!("file-{n}.rs")).collect();
        let full = NativeWriteScope::resolve_access(temp.path(), &paths, &paths).unwrap();
        assert_eq!(full.resources().len(), 32);
        assert_eq!(full.write_keys().len(), 32);
        assert!(
            NativeWriteScope::resolve_access(temp.path(), &paths, &["overflow.rs".into()]).is_err()
        );
        assert!(NativeWriteScope::resolve_access(
            temp.path(),
            &["a.rs".into(), "a.rs".into()],
            &[]
        )
        .is_err());
        assert!(NativeWriteScope::resolve_access(
            temp.path(),
            &["a.rs".into()],
            &[temp.path().join("a.rs").to_string_lossy().into_owned()]
        )
        .is_err());
        for invalid in ["", " a.rs", "a.rs ", "../a.rs", "*.rs", "a\n.rs"] {
            assert!(NativeWriteScope::resolve_access(temp.path(), &[invalid.into()], &[]).is_err());
        }
        assert!(NativeWriteScope::resolve_access(temp.path(), &["x".repeat(4097)], &[]).is_err());
        assert!(NativeWriteScope::resolve_access(temp.path(), &[], &[]).is_err());
    }

    #[test]
    fn native_scope_rejects_widening_and_duplicate_resources() {
        let temp = crate::core::file_placement::tests::TestDir::new();
        let a = NativeWriteScope::resolve(temp.path(), &["a.rs".into()]).unwrap();
        let b = NativeWriteScope::resolve(temp.path(), &["b.rs".into()]).unwrap();
        assert!(a.check_narrowing(&a).is_ok());
        assert!(a.check_narrowing(&b).is_err());
        assert!(!a.conflicts(&b));
        assert!(a.conflicts(&a));
        assert!(NativeWriteScope::resolve(temp.path(), &[]).is_err());
        assert!(NativeWriteScope::resolve(temp.path(), &["a.rs".into(), "a.rs".into()]).is_err());
        assert!(NativeWriteScope::resolve(temp.path(), &["*.rs".into()]).is_err());
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        for path in ["../escape.rs", "nested/../escape.rs", "./a.rs"] {
            assert!(
                NativeWriteScope::resolve(&canonical, &[path.into()]).is_err(),
                "raw traversal accepted: {path}"
            );
        }
    }
}
