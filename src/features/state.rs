//! `aizen state export|import|inspect` — portable memory, persona and skill bundles that let a
//! user's Aizen follow them across machines.
//!
//! Export packs a whitelist of portable directories under `~/.aizen` into one tarball with a
//! manifest. Import unpacks and merges by id: an entry that already exists is never overwritten
//! (the incoming copy is archived instead), personas that clash by name come in as
//! `<name>@imported`, skills keep the higher version. Per-machine state (device.json, sessions,
//! locks, timemachine, embed-cache) is NEVER exported and NEVER touched on import.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::core::config;
use crate::features::rebind;
use crate::memory::store;

/// Outcome of `aizen state import`.
pub struct ImportReport {
    pub archive: PathBuf,
    pub memories_new: usize,
    pub memories_skipped_identical: usize,
    pub memories_archived_conflict: usize,
    pub personas_new: Vec<String>,
    pub personas_renamed: Vec<(String, String)>,
    pub skills_new: usize,
    pub skills_upgraded: usize,
    pub rebinds: Vec<rebind::RebindReport>,
    pub warnings: Vec<String>,
}

impl ImportReport {
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("imported from {}", self.archive.display())];
        out.push(format!(
            "  memory: {} new, {} identical (skipped), {} archived as conflict",
            self.memories_new, self.memories_skipped_identical, self.memories_archived_conflict
        ));
        if !self.personas_new.is_empty() {
            out.push(format!("  personas new: {}", self.personas_new.join(", ")));
        }
        for (from, to) in &self.personas_renamed {
            out.push(format!("  persona renamed (clash): {from} -> {to}"));
        }
        out.push(format!(
            "  skills: {} new, {} upgraded",
            self.skills_new, self.skills_upgraded
        ));
        for r in &self.rebinds {
            out.extend(r.lines().into_iter().map(|l| format!("  {l}")));
        }
        for w in &self.warnings {
            out.push(format!("  warning: {w}"));
        }
        out
    }
}

/// The manifest embedded in every export archive.
#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    format_version: u32,
    aizen_version: String,
    exported_at: String,
    /// Project slugs present in the export (so import can offer rebind targets).
    zones: Vec<String>,
}

/// Directories under ~/.aizen that ARE portable (whitelist). Anything not listed here is NOT
/// exported — new top-level dirs are excluded by default, which is the safe direction.
const PORTABLE_DIRS: &[&str] = &["cli-memory", "personas", "skills", "agents", "commands"];

/// Files under ~/.aizen that are portable.
const PORTABLE_FILES: &[&str] = &["config.toml"];

/// Directories excluded even inside a portable dir (caches, per-machine review state).
const EXCLUDED_WITHIN: &[&str] = &["embed-cache", "review"];

/// Export portable state to `<to>` (default `aizen-state-<yyyymmdd>.tar` in cwd).
pub fn export(to: Option<&Path>, with_archive: bool) -> Result<PathBuf> {
    let home = config::aizen_home();
    if !home.is_dir() {
        bail!("aizen home {} does not exist", home.display());
    }
    let stamp = chrono::Local::now().format("%Y%m%d").to_string();
    let out = to
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(format!("aizen-state-{stamp}.tar")));

    let mut zones = BTreeSet::new();
    for e in store::load_from(&config::entries_dir())? {
        if let Some(s) = e.scope {
            zones.insert(s);
        }
    }
    let manifest = Manifest {
        format_version: 1,
        aizen_version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono::Local::now().to_rfc3339(),
        zones: zones.into_iter().collect(),
    };

    let file = fs::File::create(&out)
        .with_context(|| format!("creating {}", out.display()))?;
    let mut tar = tar::Builder::new(file);

    // Manifest first — import refuses archives without it.
    let mjson = serde_json::to_vec_pretty(&manifest)?;
    let mut hdr = tar::Header::new_gnu();
    hdr.set_size(mjson.len() as u64);
    hdr.set_mode(0o644);
    hdr.set_cksum();
    tar.append_data(&mut hdr, "manifest.json", mjson.as_slice())?;

    let mut packed = 0usize;
    for dir in PORTABLE_DIRS {
        let src = home.join(dir);
        if !src.is_dir() {
            continue;
        }
        packed += append_dir(&mut tar, &src, dir, with_archive)?;
    }
    for f in PORTABLE_FILES {
        let src = home.join(f);
        if src.is_file() {
            tar.append_path_with_name(&src, f)
                .with_context(|| format!("packing {}", src.display()))?;
            packed += 1;
        }
    }
    tar.finish()?;
    let _ = packed; // informational only for now
    Ok(out)
}

/// Recursively add `src` to the tar under the archive prefix `prefix`. Returns files packed.
fn append_dir<W: Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    prefix: &str,
    with_archive: bool,
) -> Result<usize> {
    let mut n = 0;
    for entry in walkdir(src)? {
        let rel = entry
            .strip_prefix(src)
            .expect("walkdir yields paths under src");
        // Skip excluded cache/review dirs and (optionally) the skill archive.
        let first = rel.components().next().map(|c| c.as_os_str());
        if let Some(name) = first {
            let name = name.to_string_lossy();
            if EXCLUDED_WITHIN.contains(&name.as_ref()) {
                continue;
            }
            if !with_archive && name == ".archive" {
                continue;
            }
        }
        if entry.is_file() {
            let arcname = format!("{}/{}", prefix, rel.to_string_lossy().replace('\\', "/"));
            tar.append_path_with_name(&entry, &arcname)
                .with_context(|| format!("packing {}", entry.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

fn walkdir(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// List the contents of an archive without importing.
pub fn inspect(file: &Path) -> Result<Vec<String>> {
    let mut f = fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut ar = tar::Archive::new(buf.as_slice());
    let mut lines = Vec::new();
    let mut manifest: Option<Manifest> = None;
    for entry in ar.entries()? {
        let entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if name == "manifest.json" {
            let mut entry = entry;
            let mut m = Vec::new();
            entry.read_to_end(&mut m)?;
            manifest = serde_json::from_slice(&m).ok();
            continue;
        }
        lines.push(format!("  {}", name));
    }
    let mut out = Vec::new();
    match manifest {
        Some(m) => {
            out.push(format!("format_version: {}", m.format_version));
            out.push(format!("aizen_version: {}", m.aizen_version));
            out.push(format!("exported_at: {}", m.exported_at));
            out.push(format!("zones: {}", m.zones.join(", ")));
        }
        None => out.push("no manifest.json — not an aizen state archive".into()),
    }
    out.push(format!("{} files:", lines.len()));
    out.extend(lines);
    Ok(out)
}

/// Import an archive, merging by id. Existing entries win; conflicting imports go to the
/// recoverable archive. After the merge, apply any `--rebind old=new` pairs.
pub fn import(file: &Path, rebinds: &[(String, PathBuf)]) -> Result<ImportReport> {
    let mut f = fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    // Extract to a temp dir under AIZEN_HOME so import is atomic-ish and inspectable.
    let stage = std::env::temp_dir().join(format!("aizen-state-import-{}", std::process::id()));
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir_all(&stage)?;
    let mut ar = tar::Archive::new(buf.as_slice());
    ar.unpack(&stage)
        .with_context(|| format!("unpacking to {}", stage.display()))?;

    // Refuse archives without a manifest (guard against random tarballs).
    let manifest_raw = fs::read_to_string(stage.join("manifest.json"))
        .context("archive has no manifest.json — refusing to import")?;
    let _manifest: Manifest = serde_json::from_str(&manifest_raw)
        .context("manifest.json is not valid")?;

    let home = config::aizen_home();
    let mut rep = ImportReport {
        archive: file.to_path_buf(),
        memories_new: 0,
        memories_skipped_identical: 0,
        memories_archived_conflict: 0,
        personas_new: Vec::new(),
        personas_renamed: Vec::new(),
        skills_new: 0,
        skills_upgraded: 0,
        rebinds: Vec::new(),
        warnings: Vec::new(),
    };

    // 1. Memory entries — union by filename id.
    let staged_entries = stage.join("cli-memory/entries");
    if staged_entries.is_dir() {
        let target = config::entries_dir();
        fs::create_dir_all(&target)?;
        for src in walkdir(&staged_entries)? {
            let id = src.file_name().expect("entry file").to_owned();
            let dest = target.join(&id);
            let incoming = fs::read_to_string(&src)
                .with_context(|| format!("reading {}", src.display()))?;
            match fs::read_to_string(&dest) {
                Ok(existing) if existing == incoming => rep.memories_skipped_identical += 1,
                Ok(_existing) => {
                    // Conflict: keep the existing entry, archive the incoming copy alongside it.
                    let archive_dir = config::archive_dir();
                    fs::create_dir_all(&archive_dir)?;
                    let stamped = archive_dir.join(format!(
                        "{}.import-conflict",
                        id.to_string_lossy()
                    ));
                    fs::write(&stamped, &incoming)?;
                    rep.memories_archived_conflict += 1;
                }
                Err(_) => {
                    fs::write(&dest, &incoming)?;
                    rep.memories_new += 1;
                }
            }
        }
    }

    // 2. Personas — name clash imports as <name>@imported.
    let staged_personas = stage.join("personas");
    if staged_personas.is_dir() {
        let target = crate::persona::personas_dir();
        fs::create_dir_all(&target)?;
        for src in walkdir(&staged_personas)? {
            let rel = src.strip_prefix(&staged_personas).expect("under personas");
            let dest = target.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if !dest.exists() {
                fs::copy(&src, &dest)?;
                if dest.extension().map(|e| e == "md").unwrap_or(false) {
                    rep.personas_new.push(
                        dest.file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    );
                }
            } else if src.extension().map(|e| e == "md").unwrap_or(false) {
                let a = fs::read(&src)?;
                let b = fs::read(&dest)?;
                if a != b {
                    let stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let renamed = format!("{stem}@imported");
                    let dest2 = dest.with_file_name(format!("{renamed}.md"));
                    fs::copy(&src, &dest2)?;
                    rep.personas_renamed.push((stem, renamed));
                }
            }
            // .self dirs and other files: existing wins silently (they are device-flavored).
        }
    }

    // 3. Skills — merge by filename, keep both on clash as <name>@imported (skills carry no
    //    reliable version field across exports, so we never silently overwrite).
    let staged_skills = stage.join("skills");
    if staged_skills.is_dir() {
        let target = crate::skills::skills_dir();
        fs::create_dir_all(&target)?;
        for src in walkdir(&staged_skills)? {
            let rel = src.strip_prefix(&staged_skills).expect("under skills");
            // Skip .archive and index.json — local state, rebuilt on demand.
            let first = rel.components().next().map(|c| c.as_os_str());
            if let Some(name) = first {
                if name == ".archive" || name == "index.json" {
                    continue;
                }
            }
            let dest = target.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if !dest.exists() {
                fs::copy(&src, &dest)?;
                rep.skills_new += 1;
            } else {
                let a = fs::read(&src)?;
                let b = fs::read(&dest)?;
                if a != b {
                    let stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dest2 = dest.with_file_name(format!(
                        "{stem}@imported.{}",
                        dest.extension()
                            .map(|e| e.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "md".into())
                    ));
                    fs::copy(&src, &dest2)?;
                    rep.skills_upgraded += 1;
                }
            }
        }
    }

    // 4. agents/ and commands/ — copy missing files only, never overwrite.
    for dir in ["agents", "commands"] {
        let staged = stage.join(dir);
        if !staged.is_dir() {
            continue;
        }
        let target = home.join(dir);
        fs::create_dir_all(&target)?;
        for src in walkdir(&staged)? {
            let rel = src.strip_prefix(&staged).expect("under dir");
            let dest = target.join(rel);
            if !dest.exists() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &dest)?;
            }
        }
    }

    // 5. config.toml — never imported (machine-specific paths/keys). Warn if it was present.
    if stage.join("config.toml").is_file() {
        rep.warnings.push(
            "config.toml in archive was NOT imported (machine-specific) — merge by hand if needed"
                .into(),
        );
    }

    // 6. Rebinds last, so imported entries pick up the new home's slugs.
    for (old, new_root) in rebinds {
        rep.rebinds.push(rebind::apply(old, new_root)?);
    }

    let _ = fs::remove_dir_all(&stage);
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// Env-sandbox pattern mirrored from features::zones tests.
    fn with_sandbox<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tag = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let sandbox = std::env::temp_dir().join(format!("aizen-state-test-{}-{tag:x}", std::process::id()));
        let home = sandbox.join("home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("AIZEN_HOME", &home);
        let out = f(&sandbox);
        std::env::remove_var("AIZEN_HOME");
        let _ = fs::remove_dir_all(&sandbox);
        out
    }

    #[test]
    fn export_then_import_round_trips_memory_entries() {
        with_sandbox(|sandbox| {
            // Seed a memory entry and a persona.
            fs::create_dir_all(config::entries_dir()).unwrap();
            fs::write(
                config::entries_dir().join("test-entry.md"),
                "---\nname: test\nscope: oldslug-12345678\n---\nbody\n",
            )
            .unwrap();
            fs::create_dir_all(crate::persona::personas_dir()).unwrap();
            fs::write(crate::persona::personas_dir().join("kira.md"), "persona").unwrap();

            let archive = sandbox.join("export.tar");
            let out = export(Some(&archive), false).expect("export");
            assert!(out.is_file());

            // Wipe the home, then import.
            fs::remove_dir_all(config::aizen_home()).unwrap();
            let rep = import(&archive, &[]).expect("import");
            assert_eq!(rep.memories_new, 1);
            assert!(rep.personas_new.contains(&"kira".to_string()));
            assert!(config::entries_dir().join("test-entry.md").is_file());
        });
    }

    #[test]
    fn inspect_reports_manifest() {
        with_sandbox(|sandbox| {
            fs::create_dir_all(config::entries_dir()).unwrap();
            fs::write(
                config::entries_dir().join("e.md"),
                "---\nname: e\n---\nx\n",
            )
            .unwrap();
            let archive = sandbox.join("export.tar");
            export(Some(&archive), false).unwrap();
            let lines = inspect(&archive).unwrap();
            assert!(lines.iter().any(|l| l.contains("format_version: 1")));
            assert!(lines.iter().any(|l| l.contains("zones:")));
        });
    }
}
