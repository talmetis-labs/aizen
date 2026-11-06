//! Host-owned provider data roots. Planning is read-only; only an admitted operation initializes data.

use super::settings::{MemoryPlugins, PackageState};
use crate::sandbox::request::{path_within, paths_overlap, planned_directory};
use anyhow::{ensure, Result};
use std::path::{Path, PathBuf};

pub(crate) fn default_root(id: &str) -> Result<PathBuf> {
    super::manifest::validate_id(id)?;
    Ok(crate::core::config::aizen_home()
        .join("memory-data")
        .join(id))
}

pub(crate) fn resolve(id: &str, package: &PackageState) -> Result<PathBuf> {
    super::manifest::validate_id(id)?;
    ensure!(package.pins.id == id, "Memory storage identity mismatch");
    Ok(package.data_root.clone().unwrap_or(default_root(id)?))
}

pub(crate) fn validate(
    root: &Path,
    id: &str,
    state: &MemoryPlugins,
    workspace: Option<&Path>,
) -> Result<PathBuf> {
    let root = validate_in(
        root,
        id,
        state,
        workspace,
        &crate::core::config::aizen_home(),
        &crate::core::config::home_dir(),
    )?;
    let config =
        crate::core::cli_config::read_config_for_memory(&crate::core::cli_config::config_path())?;
    let mut model_roots = crate::retrieval::embed::host_hf_roots();
    for model in [std::env::var("AIZEN_EMBED_MODEL").ok(), config.embed_model]
        .into_iter()
        .flatten()
    {
        let model = PathBuf::from(model);
        if model.is_absolute() {
            model_roots.push(model);
        }
    }
    for model in model_roots {
        let absolute = std::path::absolute(&model)?;
        ensure!(
            !paths_overlap(&root, &absolute),
            "Memory storage overlaps Core model storage"
        );
        // Deny roots may be user-managed symlinks; unlike write grants, resolving them grants nothing.
        if let Ok(canonical) = model.canonicalize() {
            ensure!(
                !paths_overlap(&root, &canonical),
                "Memory storage overlaps Core model storage"
            );
        }
    }
    Ok(root)
}

fn validate_in(
    root: &Path,
    id: &str,
    state: &MemoryPlugins,
    workspace: Option<&Path>,
    aizen: &Path,
    home: &Path,
) -> Result<PathBuf> {
    super::manifest::validate_id(id)?;
    let root = planned_directory(root)?;
    let aizen = planned_directory(aizen)?;
    let home = planned_directory(home)?;
    let default = planned_directory(&aizen.join("memory-data").join(id))?;
    let adopted = super::adoption::registered_storage(&root, id, &aizen)?;
    // Only exact host-registered published leaves extend the legacy default inside Core HOME.
    ensure!(
        !path_within(&aizen, &root),
        "Memory storage cannot contain AIZEN_HOME"
    );
    ensure!(
        !path_within(&home, &root),
        "Memory storage cannot contain HOME"
    );
    ensure!(
        !path_within(&root, &aizen) || same(&root, &default) || adopted,
        "custom Memory storage cannot overlap Core package, control, temp or model directories"
    );
    if let Some(workspace) = workspace {
        let workspace = planned_directory(workspace)?;
        ensure!(
            !path_within(&workspace, &root),
            "Memory storage must be a dedicated directory, not the whole workspace"
        );
        for name in [".git", ".aizen"] {
            // These are deny paths, not directory grants: a linked worktree's .git is a file.
            let control = workspace.join(name);
            let own_default = (same(&root, &default) || adopted) && same(&control, &aizen);
            ensure!(
                !paths_overlap(&root, &control) || own_default,
                "Memory storage overlaps workspace control directories"
            );
            if let Ok(canonical) = control.canonicalize() {
                ensure!(
                    !paths_overlap(&root, &canonical) || own_default,
                    "Memory storage overlaps workspace control directories"
                );
            }
        }
    }
    for (other_id, package) in &state.packages {
        if other_id == id {
            continue;
        }
        let other_default = planned_directory(&aizen.join("memory-data").join(other_id))?;
        ensure!(
            !paths_overlap(&root, &other_default),
            "Memory storage overlaps another provider's default directory"
        );
        if let Some(custom) = &package.data_root {
            ensure!(
                !paths_overlap(&root, &planned_directory(custom)?),
                "Memory storage overlaps another provider's custom directory"
            );
        }
    }
    #[cfg(windows)]
    {
        let platform = crate::sandbox::request::NativePluginPlatformEnv::for_host()?;
        ensure!(
            !paths_overlap(&root, platform.system_root()),
            "Memory storage overlaps the platform runtime directory"
        );
    }
    #[cfg(not(windows))]
    for system in [
        "/bin", "/sbin", "/usr", "/lib", "/lib64", "/etc", "/proc", "/sys", "/dev", "/run",
    ] {
        // Runtime roots can themselves be distro-managed symlinks. They are deny roots, not grants.
        let system = Path::new(system);
        ensure!(
            !paths_overlap(&root, system),
            "Memory storage overlaps a platform runtime directory"
        );
        if let Ok(canonical) = system.canonicalize() {
            ensure!(
                !paths_overlap(&root, &canonical),
                "Memory storage overlaps a platform runtime directory"
            );
        }
    }
    Ok(root)
}

fn same(a: &Path, b: &Path) -> bool {
    path_within(a, b) && path_within(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_state() -> MemoryPlugins {
        let mut state = MemoryPlugins::default();
        for id in ["one", "two"] {
            state.record_install(serde_json::from_value(serde_json::json!({
                "schema_version":1,"id":id,"target":"fixture",
                "manifest":{"path":"plugin.json","bytes":1,"sha256":"a".repeat(64)},"files":[]
            })).unwrap()).unwrap();
        }
        state
    }

    #[test]
    fn planning_preserves_absent_paths_and_existing_parents() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let aizen = home.join(".aizen");
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let permissions = std::fs::metadata(&workspace).unwrap().permissions();
        let sentinel = workspace.join("keep");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let custom = workspace.join("dedicated").join("records");
        let state = fixture_state();
        let normalized =
            validate_in(&custom, "one", &state, Some(&workspace), &aizen, &home).unwrap();
        assert!(normalized.is_absolute());
        assert!(!custom.exists() && !home.exists());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        assert_eq!(
            std::fs::metadata(&workspace)
                .unwrap()
                .permissions()
                .readonly(),
            permissions.readonly()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&workspace).unwrap().permissions().mode(),
                permissions.mode()
            );
        }
        validate_in(
            &aizen.join("memory-data/one"),
            "one",
            &state,
            Some(&workspace),
            &aizen,
            &home,
        )
        .unwrap();
        assert!(!aizen.exists());
    }

    #[test]
    fn broad_unsafe_control_and_cross_provider_roots_are_refused() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let aizen = home.join(".aizen");
        let workspace = fixture.path().join("workspace");
        let mut state = fixture_state();
        let other = fixture.path().join("other-data");
        state.packages.get_mut("two").unwrap().data_root = Some(other.clone());
        for path in [
            home.clone(),
            aizen.clone(),
            workspace.clone(),
            aizen.join("plugins/one"),
            aizen.join("plugin-state"),
            aizen.join("plugin-temp"),
            aizen.join("models"),
            workspace.join(".git/records"),
            workspace.join(".aizen/records"),
            aizen.join("memory-data/two"),
            other.clone(),
            other.join("child"),
            fixture.path().to_path_buf(),
            PathBuf::from(format!("{}/../escape", workspace.display())),
            workspace.join("NUL"),
            workspace.join("bad:stream"),
            workspace.join("bad."),
            PathBuf::from("relative"),
        ] {
            assert!(
                validate_in(&path, "one", &state, Some(&workspace), &aizen, &home).is_err(),
                "{}",
                path.display()
            );
        }
        assert!(!home.exists() && !workspace.exists() && !other.exists());
    }

    #[test]
    fn missing_external_model_roots_remain_denied_without_creation() {
        let _lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let aizen = home.join(".aizen");
        let hf = fixture.path().join("hf-cache");
        let embed = fixture.path().join("embed-model");
        let plain = |path: &Path| {
            path.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned()
        };
        let _env = crate::core::config::EnvGuard::set([
            ("AIZEN_HOME", plain(&aizen)),
            ("USERPROFILE", plain(&home)),
            ("HOME", plain(&home)),
            ("HF_HUB_CACHE", plain(&hf)),
            ("HF_HOME", plain(&fixture.path().join("hf-home"))),
            ("XDG_CACHE_HOME", plain(&fixture.path().join("xdg"))),
            ("AIZEN_EMBED_MODEL", plain(&embed)),
        ]);
        let state = fixture_state();
        for path in [&hf, &hf.join("data"), &embed, &embed.join("data")] {
            assert!(
                validate(path, "one", &state, None).is_err(),
                "{}",
                path.display()
            );
        }
        assert!(!hf.exists() && !embed.exists() && !home.exists());
    }

    #[test]
    fn linked_worktree_control_file_does_not_block_safe_roots() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let aizen = home.join(".aizen");
        let workspace = fixture.path().join("worktree");
        std::fs::create_dir_all(&workspace).unwrap();
        let control = workspace.join(".git");
        let contents = b"gitdir: ../main/.git/worktrees/fixture\n";
        std::fs::write(&control, contents).unwrap();
        let state = fixture_state();
        for root in [
            aizen.join("memory-data/one"),
            workspace.join("private-memory"),
        ] {
            validate_in(&root, "one", &state, Some(&workspace), &aizen, &home).unwrap();
            assert!(!root.exists());
        }
        assert!(validate_in(&control, "one", &state, Some(&workspace), &aizen, &home).is_err());
        assert_eq!(std::fs::read(control).unwrap(), contents);
        assert!(!home.exists());
    }

    #[test]
    fn exact_default_remains_valid_when_workspace_is_home() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        let aizen = home.join(".aizen");
        let state = fixture_state();
        let root = aizen.join("memory-data/one");
        validate_in(&root, "one", &state, Some(&home), &aizen, &home).unwrap();
        for denied in [
            aizen.clone(),
            aizen.join("models"),
            aizen.join("memory-data/two"),
        ] {
            assert!(validate_in(&denied, "one", &state, Some(&home), &aizen, &home).is_err());
        }
        assert!(!home.exists());
    }

    #[test]
    fn planning_rejects_files_and_link_ancestors() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let file = fixture.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        assert!(planned_directory(&file).is_err());
        assert!(planned_directory(&file.join("child")).is_err());
        #[cfg(unix)]
        {
            let link = fixture.path().join("link");
            std::os::unix::fs::symlink(fixture.path(), &link).unwrap();
            assert!(planned_directory(&link.join("missing")).is_err());
        }
    }
}
