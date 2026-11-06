//! [`SandboxRequest`] — everything the runner must know about one spawn, gathered at the call
//! site. The request carries WHAT should run and WHO asked; the runner decides HOW (mode, backend,
//! environment, roots) and refuses when the platform cannot honor a strict policy.

use super::CommandOrigin;
use anyhow::{bail, ensure, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// A host-owned saved grant, not a provider declaration or the global network default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum NativePluginNetworkGrant {
    #[default]
    Missing,
    Denied,
    Granted,
}

/// The only ambient platform value a restricted native executable needs. No PATH, proxy,
/// toolchain/cache setting, locale override or caller-supplied arbitrary environment is accepted.
#[derive(Debug, Clone)]
pub struct NativePluginPlatformEnv {
    #[cfg(windows)]
    system_root: PathBuf,
}

impl NativePluginPlatformEnv {
    /// Read only SystemRoot on Windows; read no parent environment values on Unix. A host may
    /// instead provide an explicit Windows directory with `windows` (also useful for fixtures).
    pub fn for_host() -> Result<Self> {
        #[cfg(windows)]
        {
            let root = std::env::var_os("SystemRoot").context("Windows SystemRoot is missing")?;
            Self::windows(PathBuf::from(root))
        }
        #[cfg(not(windows))]
        {
            Ok(Self {})
        }
    }

    #[cfg(windows)]
    pub fn windows(system_root: PathBuf) -> Result<Self> {
        Ok(Self {
            system_root: restricted_path(&system_root, true)?,
        })
    }

    #[cfg(windows)]
    pub(crate) fn system_root(&self) -> &Path {
        &self.system_root
    }
}

/// Host-owned filesystem/environment inputs for an untrusted native executable. This is not an
/// authorization receipt: the manager must check package hashes, saved consent and admission.
/// Existing directories are required; this type never creates or removes them. HOME must be a
/// private child of temp; the caller owns both until verified process-tree AND pipe settlement.
/// Package and model roots are read-only; only data and temp are writable. No workspace/cache or
/// global extra-root grants are folded in. Validation is preflight, not protection against an
/// unrelated process swapping an ancestor afterwards (TOCTOU).
#[derive(Debug, Clone)]
pub struct NativePluginPolicy {
    pub(super) package_root: PathBuf,
    pub(super) data_root: PathBuf,
    pub(super) model_roots: Vec<PathBuf>,
    pub(super) synthetic_home: PathBuf,
    pub(super) private_temp: PathBuf,
    pub(super) platform_env: NativePluginPlatformEnv,
    pub(super) network: NativePluginNetworkGrant,
    /// Sealed adoption extends reads, never ambient grants. Its temp is inside the envelope.
    pub(super) adoption: Option<AdoptionAccess>,
}

#[derive(Debug, Clone)]
pub(super) struct AdoptionAccess {
    pub read_files: Vec<PathBuf>,
    pub write_envelope: bool,
}

/// Offline migration policy: source/package/immutable plan are read-only; only the freshly
/// allocated operation envelope can be written. It cannot be constructed from provider JSON.
#[derive(Debug, Clone)]
pub(crate) struct NativeAdoptionPolicy(NativePluginPolicy);
impl NativeAdoptionPolicy {
    pub(crate) fn new(
        package_root: PathBuf,
        source: Option<PathBuf>,
        plan: Option<PathBuf>,
        envelope: PathBuf,
        private_temp: PathBuf,
        write_envelope: bool,
    ) -> Result<Self> {
        let policy = NativePluginPolicy {
            package_root: restricted_path(&package_root, true)?,
            data_root: restricted_path(&envelope, true)?,
            model_roots: source
                .into_iter()
                .map(|p| restricted_path(&p, true))
                .collect::<Result<_>>()?,
            synthetic_home: restricted_path(&private_temp.join("home"), true)?,
            private_temp: restricted_path(&private_temp, true)?,
            platform_env: NativePluginPlatformEnv::for_host()?,
            network: NativePluginNetworkGrant::Denied,
            adoption: Some(AdoptionAccess {
                read_files: plan
                    .into_iter()
                    .map(|p| restricted_path(&p, false))
                    .collect::<Result<_>>()?,
                write_envelope,
            }),
        };
        policy.validate()?;
        Ok(Self(policy))
    }
}

/// Explicit host-owned roots. The provider does not get to select these paths.
#[derive(Debug, Clone)]
pub struct NativePluginPaths {
    pub package_root: PathBuf,
    pub data_root: PathBuf,
    pub model_roots: Vec<PathBuf>,
    pub synthetic_home: PathBuf,
    pub private_temp: PathBuf,
}

impl NativePluginPolicy {
    pub fn new(
        paths: NativePluginPaths,
        platform_env: NativePluginPlatformEnv,
        network: NativePluginNetworkGrant,
    ) -> Result<Self> {
        let NativePluginPaths {
            package_root,
            data_root,
            model_roots,
            synthetic_home,
            private_temp,
        } = paths;
        ensure!(model_roots.len() <= 32, "too many restricted model roots");
        let policy = Self {
            package_root: restricted_path(&package_root, true)?,
            data_root: restricted_path(&data_root, true)?,
            model_roots: model_roots
                .iter()
                .map(|p| restricted_path(p, true))
                .collect::<Result<_>>()?,
            synthetic_home: restricted_path(&synthetic_home, true)?,
            private_temp: restricted_path(&private_temp, true)?,
            platform_env,
            network,
            adoption: None,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub(super) fn validate(&self) -> Result<()> {
        for path in [
            &self.package_root,
            &self.data_root,
            &self.synthetic_home,
            &self.private_temp,
        ]
        .into_iter()
        .chain(self.model_roots.iter())
        {
            restricted_path(path, true)?;
        }
        ensure!(
            path_within(&self.synthetic_home, &self.private_temp)
                && !path_within(&self.private_temp, &self.synthetic_home),
            "synthetic HOME must be a private child of restricted temp"
        );
        if let Some(adoption) = &self.adoption {
            ensure!(
                self.network == NativePluginNetworkGrant::Denied,
                "adoption is always offline"
            );
            ensure!(
                path_within(&self.private_temp, &self.data_root)
                    && self.private_temp != self.data_root,
                "adoption temp must stay within its operation envelope"
            );
            for file in &adoption.read_files {
                restricted_path(file, false)?;
                ensure!(
                    !paths_overlap(file, &self.data_root),
                    "adoption plan overlaps writable envelope"
                );
            }
        } else {
            ensure!(
                !paths_overlap(&self.data_root, &self.private_temp),
                "restricted data and temp roots overlap"
            );
        }
        for read in std::iter::once(&self.package_root).chain(self.model_roots.iter()) {
            for write in [&self.data_root, &self.private_temp] {
                ensure!(
                    !paths_overlap(read, write),
                    "restricted read-only and writable roots overlap"
                );
            }
        }
        #[cfg(windows)]
        {
            let system = self.platform_env.system_root();
            restricted_path(system, true)?;
            for root in [&self.package_root, &self.data_root, &self.private_temp]
                .into_iter()
                .chain(self.model_roots.iter())
            {
                ensure!(
                    !paths_overlap(root, system),
                    "restricted roots overlap the platform runtime directory"
                );
            }
        }
        #[cfg(not(windows))]
        let _ = &self.platform_env;
        Ok(())
    }
}

/// Component-wise comparison, including Windows drive/verbatim and case-insensitive spellings.
/// Callers preflight writable grants separately; comparison does not authorize UNC/device paths.
pub(crate) fn path_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        fn disk(component: Component<'_>) -> Option<u8> {
            match component {
                Component::Prefix(prefix) => match prefix.kind() {
                    std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) => {
                        Some(drive.to_ascii_lowercase())
                    }
                    _ => None,
                },
                _ => None,
            }
        }
        let mut parts = path.components();
        root.components().all(|r| {
            parts.next().is_some_and(|p| match (disk(p), disk(r)) {
                (Some(a), Some(b)) => a == b,
                _ => {
                    p.as_os_str().to_string_lossy().to_lowercase()
                        == r.as_os_str().to_string_lossy().to_lowercase()
                }
            })
        })
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

pub(crate) fn paths_overlap(a: &Path, b: &Path) -> bool {
    path_within(a, b) || path_within(b, a)
}

/// Reject links/reparse points in every existing ancestor before canonicalizing. The installer
/// separately validates package contents/digests; model trees are never promoted to write roots.
fn restricted_path(path: &Path, directory: bool) -> Result<PathBuf> {
    checked_path(path, directory, false)
}

/// Read-only directory planning: canonicalize the existing ancestor and retain the missing suffix.
/// This does not create directories or change permissions, and is not an ancestor-swap defense.
pub(crate) fn planned_directory(path: &Path) -> Result<PathBuf> {
    checked_path(path, true, true)
}

fn checked_path(path: &Path, directory: bool, allow_missing: bool) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "restricted paths must be absolute");
    ensure!(
        path.parent().is_some(),
        "restricted paths cannot be filesystem roots"
    );
    let spelling = path.to_str().context("restricted path must be UTF-8")?;
    let spelling = spelling.strip_prefix(r"\\?\").unwrap_or(spelling);
    ensure!(
        !spelling
            .split(['/', '\\'])
            .any(|part| matches!(part, "." | "..")),
        "restricted path contains raw traversal"
    );
    let mut current = PathBuf::new();
    let mut missing = false;
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir | Component::CurDir => bail!("restricted path contains traversal"),
            #[cfg(windows)]
            Component::Prefix(prefix) => ensure!(
                matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                ),
                "restricted paths cannot use UNC/device namespaces"
            ),
            Component::Normal(part) => {
                let part = part.to_str().context("restricted paths must be UTF-8")?;
                ensure!(
                    !part.ends_with(['.', ' '])
                        && !part.chars().any(|c| {
                            c.is_control()
                                || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*' | '\\')
                        }),
                    "restricted path contains an unsafe component"
                );
                let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
                ensure!(
                    !matches!(
                        stem.as_str(),
                        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
                    ) && !["COM", "LPT"].iter().any(|p| {
                        stem.strip_prefix(p).is_some_and(|s| {
                            matches!(
                                s,
                                "1" | "2"
                                    | "3"
                                    | "4"
                                    | "5"
                                    | "6"
                                    | "7"
                                    | "8"
                                    | "9"
                                    | "¹"
                                    | "²"
                                    | "³"
                            )
                        })
                    }),
                    "restricted path contains a reserved component"
                );
            }
            _ => {}
        }
        current.push(component.as_os_str());
        // A Windows drive prefix is not an existing directory until its root separator is added.
        #[cfg(windows)]
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        if missing {
            resolved.push(component.as_os_str());
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                resolved = current
                    .parent()
                    .context("missing directory has no parent")?
                    .canonicalize()?;
                resolved.push(component.as_os_str());
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking restricted path {}", current.display()))
            }
        };
        ensure!(
            !crate::core::file_placement::is_link_or_reparse(&metadata),
            "restricted path traverses a symlink/reparse point"
        );
        if current != path || directory {
            ensure!(
                metadata.is_dir(),
                "restricted path has a non-directory ancestor"
            );
        }
    }
    if missing {
        return Ok(resolved);
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "restricted path has the wrong file type"
    );
    Ok(path.canonicalize()?)
}

/// The program half of a request: a shell command line (wrapped in the platform shell exactly the
/// way `shell_run` always has), or a direct program + argv (LSP/MCP servers, git plumbing).
#[derive(Debug, Clone)]
pub enum CommandSpec {
    /// Run through the platform shell: `cmd /C chcp 65001>nul & <line>` on Windows, `sh -c <line>`
    /// elsewhere. The `chcp` prelude is behavior-preserving — legacy builtins emit UTF-8 instead of
    /// the OEM codepage (see `shell_run`).
    Shell { line: String },
    /// Spawn `program` directly with `args` (no shell). `use_cmd_shim` routes through `cmd /C` on
    /// Windows for `.cmd`/`.bat` runners that `CreateProcessW` cannot execute (npx/uvx shims).
    Exec {
        program: PathBuf,
        args: Vec<String>,
        use_cmd_shim: bool,
    },
}

/// One spawn's worth of intent. Built by call sites; consumed by [`super::runner`].
#[derive(Debug, Clone)]
pub struct SandboxRequest {
    /// Who asked (a call-site constant, never model data).
    pub origin: CommandOrigin,
    /// What to run.
    pub spec: CommandSpec,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// The workspace this run is confined to (the write root for filesystem policy).
    pub workspace_root: PathBuf,
    /// Whether the network capability was REQUESTED (and, for model-originated commands, already
    /// approved upstream). `false` ⇒ the policy default (deny unless config says otherwise).
    pub network: bool,
    /// Execution/resource scope (the `exec_ctx` resource scope) — audit correlation.
    pub scope: String,
    /// Background (no wall-clock cap; lives past the tool call) vs foreground.
    pub background: bool,
    /// The wall-clock cap the CALL SITE will enforce, recorded for audit/status. The runner does
    /// not run the wait loop — the battle-tested per-site loops (cancel-aware polling, condvars,
    /// async timeouts) stay where they are.
    pub wall_timeout: Option<Duration>,
    /// Extra variables applied AFTER the scrub (an MCP server's configured env — its own tokens
    /// are its business; Aizen's are not inherited).
    pub extra_env: Vec<(String, String)>,
    /// Give the child a PRIVATE temp directory (TMP/TEMP/TMPDIR pointed at a fresh per-run dir,
    /// removed when the guard drops). Off for user escapes, where surprising temp relocation
    /// would be visible.
    pub private_tmp: bool,
    /// Sealed restrictive policy. Ordinary builders leave it absent, preserving their behavior.
    restricted: Option<NativePluginPolicy>,
}

impl SandboxRequest {
    /// A shell-line request with the common defaults: foreground, no network, private temp.
    pub fn shell(
        origin: CommandOrigin,
        line: impl Into<String>,
        cwd: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            origin,
            spec: CommandSpec::Shell { line: line.into() },
            cwd,
            workspace_root,
            network: false,
            scope: current_scope(),
            background: false,
            wall_timeout: None,
            extra_env: Vec::new(),
            private_tmp: true,
            restricted: None,
        }
    }

    /// A direct-exec request (server processes, plumbing).
    pub fn exec(
        origin: CommandOrigin,
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            origin,
            spec: CommandSpec::Exec {
                program,
                args,
                use_cmd_shim: false,
            },
            cwd,
            workspace_root,
            network: false,
            scope: current_scope(),
            background: true,
            wall_timeout: None,
            extra_env: Vec::new(),
            private_tmp: true,
            restricted: None,
        }
    }

    /// Build a shell-free native plugin request. The package is its read-only cwd, never the
    /// project workspace. Keep actual workspace admission separately in the manager. Generic
    /// `.network`, `.extra_env`, `.private_tmp` and `.cmd_shim` cannot weaken this sealed policy.
    pub fn native_plugin(
        origin: CommandOrigin,
        program: PathBuf,
        args: Vec<String>,
        policy: NativePluginPolicy,
    ) -> Result<Self> {
        policy.validate()?;
        let program = restricted_path(&program, false)?;
        ensure!(
            path_within(&program, &policy.package_root),
            "native plugin executable must be inside its package"
        );
        ensure!(
            !program.extension().is_some_and(|e| {
                e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat")
            }),
            "native plugins cannot use command shims"
        );
        let mut request = Self::exec(
            origin,
            program,
            args,
            policy.package_root.clone(),
            policy.package_root.clone(),
        );
        request.background = false;
        request.private_tmp = false;
        request.restricted = Some(policy);
        Ok(request)
    }

    pub(crate) fn native_adoption(
        program: PathBuf,
        args: Vec<String>,
        policy: NativeAdoptionPolicy,
    ) -> Result<Self> {
        Self::native_plugin(CommandOrigin::MemoryProvider, program, args, policy.0)
    }

    pub(super) fn restricted_policy(&self) -> Option<&NativePluginPolicy> {
        self.restricted.as_ref()
    }

    /// Recheck path ancestors immediately before command construction. Request fields remain
    /// public for legacy callers, so reject mutations that would escape the sealed package.
    pub(super) fn validate_restricted(&self) -> Result<()> {
        let Some(policy) = &self.restricted else {
            return Ok(());
        };
        policy.validate()?;
        ensure!(
            self.cwd == policy.package_root && self.workspace_root == policy.package_root,
            "native plugin cwd/root must remain the read-only package"
        );
        ensure!(
            self.extra_env.is_empty(),
            "native plugins cannot receive arbitrary extra env"
        );
        let CommandSpec::Exec {
            program,
            use_cmd_shim: false,
            ..
        } = &self.spec
        else {
            bail!("native plugins require direct exec without a shell shim")
        };
        let program = restricted_path(program, false)?;
        ensure!(
            path_within(&program, &policy.package_root),
            "native plugin executable escaped its package"
        );
        ensure!(
            !program.extension().is_some_and(|e| {
                e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat")
            }),
            "native plugins cannot use command shims"
        );
        Ok(())
    }

    pub fn network(mut self, allowed: bool) -> Self {
        self.network = allowed;
        self
    }
    pub fn background(mut self, bg: bool) -> Self {
        self.background = bg;
        self
    }
    pub fn wall_timeout(mut self, t: Duration) -> Self {
        self.wall_timeout = Some(t);
        self
    }
    pub fn extra_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }
    pub fn private_tmp(mut self, on: bool) -> Self {
        self.private_tmp = on;
        self
    }
    pub fn cmd_shim(mut self, on: bool) -> Self {
        if let CommandSpec::Exec {
            ref mut use_cmd_shim,
            ..
        } = self.spec
        {
            *use_cmd_shim = on;
        }
        self
    }

    /// The command as one displayable line (for audit redaction/truncation).
    pub fn display_line(&self) -> String {
        match &self.spec {
            CommandSpec::Shell { line } => line.clone(),
            CommandSpec::Exec { program, args, .. } => {
                let mut s = program.display().to_string();
                for a in args {
                    s.push(' ');
                    s.push_str(a);
                }
                s
            }
        }
    }
}

/// The calling turn's resource scope (audit correlation), read from the execution context the same
/// way stateful tools do. `default` outside any pinned turn.
fn current_scope() -> String {
    crate::core::exec_ctx::current()
        .map(|c| c.resource_scope())
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
pub(super) fn native_plugin_fixture() -> (
    crate::core::file_placement::tests::TestDir,
    NativePluginPolicy,
    PathBuf,
) {
    let fixture = crate::core::file_placement::tests::TestDir::new();
    let package_root = fixture.path().join("package");
    let data_root = fixture.path().join("provider-data");
    let private_temp = fixture.path().join("private-temp");
    let synthetic_home = private_temp.join("home");
    let models = fixture.path().join("models");
    for path in [&package_root, &data_root, &synthetic_home, &models] {
        std::fs::create_dir_all(path).unwrap();
    }
    #[cfg(windows)]
    let platform_env = {
        let system = fixture.path().join("platform-runtime");
        std::fs::create_dir(&system).unwrap();
        NativePluginPlatformEnv::windows(system).unwrap()
    };
    #[cfg(not(windows))]
    let platform_env = NativePluginPlatformEnv::for_host().unwrap();
    let program = package_root.join("provider.exe");
    // The fixture is inspected, never executed.
    std::fs::write(&program, b"not an executable").unwrap();
    let policy = NativePluginPolicy::new(
        NativePluginPaths {
            package_root,
            data_root,
            model_roots: vec![models],
            synthetic_home,
            private_temp,
        },
        platform_env,
        NativePluginNetworkGrant::Missing,
    )
    .unwrap();
    (fixture, policy, program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn disk_and_verbatim_paths_overlap_without_losing_component_boundaries() {
        let plain = Path::new(r"C:\models\hf-new");
        let verbatim = Path::new(r"\\?\c:\MODELS\hf-new");
        assert!(path_within(plain, verbatim) && path_within(verbatim, plain));
        assert!(paths_overlap(&verbatim.join("data"), plain));
        assert!(!paths_overlap(Path::new(r"C:\models\hf-newer"), verbatim));
        assert!(!paths_overlap(Path::new(r"D:\models\hf-new"), verbatim));
    }

    #[test]
    fn native_plugin_rejects_overlapping_model_and_write_roots() {
        let (_fixture, mut policy, _program) = native_plugin_fixture();
        policy.model_roots = vec![policy.data_root.clone()];
        assert!(policy.validate().is_err());
        policy.model_roots = vec![policy.private_temp.parent().unwrap().to_path_buf()];
        assert!(policy.validate().is_err());
    }

    #[test]
    fn native_plugin_rejects_broad_relative_and_traversing_paths() {
        let (fixture, policy, _program) = native_plugin_fixture();
        assert!(restricted_path(Path::new("relative"), true).is_err());
        // TestDir may have a Windows verbatim prefix: PathBuf::join normalizes `..` for that
        // spelling before this validator ever sees it. Construct the raw input without joining.
        let traversal = PathBuf::from(format!(
            "{}/package/../provider-data",
            fixture.path().display()
        ));
        assert!(restricted_path(&traversal, true).is_err());
        assert!(restricted_path(fixture.path().ancestors().last().unwrap(), true).is_err());
        let mut overlap = policy.clone();
        overlap.private_temp = overlap.data_root.clone();
        assert!(overlap.validate().is_err());
        let mut home = policy;
        home.synthetic_home = home.private_temp.clone();
        assert!(home.validate().is_err());
    }

    #[test]
    fn native_plugin_seals_cwd_program_shell_and_environment() {
        let (fixture, policy, program) = native_plugin_fixture();
        let request = SandboxRequest::native_plugin(
            CommandOrigin::McpStdio,
            program.clone(),
            Vec::new(),
            policy.clone(),
        )
        .unwrap();
        assert_eq!(request.cwd, policy.package_root);
        assert!(request.validate_restricted().is_ok());
        let mut changed = request.clone();
        changed.cwd = policy.data_root;
        assert!(changed.validate_restricted().is_err());
        assert!(request
            .clone()
            .cmd_shim(true)
            .validate_restricted()
            .is_err());
        assert!(request
            .clone()
            .extra_env(vec![("KEY".into(), "secret".into())])
            .validate_restricted()
            .is_err());
        let mut changed = request.clone();
        changed.spec = CommandSpec::Shell {
            line: "ignored".into(),
        };
        assert!(changed.validate_restricted().is_err());
        let outside = fixture.path().join("outside.exe");
        std::fs::write(&outside, b"not executable").unwrap();
        assert!(SandboxRequest::native_plugin(
            CommandOrigin::McpStdio,
            outside,
            vec![],
            request.restricted.unwrap()
        )
        .is_err());
        // Recheck immediately before prepare, not just at construction.
        std::fs::remove_file(program).unwrap();
        assert!(changed.validate_restricted().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_plugin_rejects_symlinked_model_roots_and_ancestors() {
        let (fixture, mut policy, _program) = native_plugin_fixture();
        let link = fixture.path().join("linked-models");
        std::os::unix::fs::symlink(&policy.data_root, &link).unwrap();
        policy.model_roots = vec![link.clone()];
        assert!(policy.validate().is_err());
        std::fs::create_dir(policy.data_root.join("child")).unwrap();
        assert!(restricted_path(&link.join("child"), true).is_err());
    }

    #[test]
    fn builders_set_what_they_say() {
        let r = SandboxRequest::shell(
            CommandOrigin::ShellRun,
            "echo hi",
            PathBuf::from("."),
            PathBuf::from("."),
        )
        .network(true)
        .background(true)
        .wall_timeout(Duration::from_secs(5));
        assert!(r.network);
        assert!(r.background);
        assert_eq!(r.wall_timeout, Some(Duration::from_secs(5)));
        assert_eq!(r.display_line(), "echo hi");

        let e = SandboxRequest::exec(
            CommandOrigin::Lsp,
            PathBuf::from("rust-analyzer"),
            vec!["--version".into()],
            PathBuf::from("."),
            PathBuf::from("."),
        )
        .cmd_shim(true);
        match e.spec {
            CommandSpec::Exec { use_cmd_shim, .. } => assert!(use_cmd_shim),
            _ => panic!("exec spec expected"),
        }
        assert_eq!(e.display_line(), "rust-analyzer --version");
    }
}
