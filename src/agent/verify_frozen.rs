//! Commands and local source identity frozen before approval. Git receipt binding lives in core.
use super::*;
use crate::core::{cancel, exec_ctx};
use anyhow::{ensure, Context, Result};
use std::future::Future;

#[derive(Debug)]
pub(crate) struct FrozenVerifyPlan {
    root: PathBuf,
    commands: Vec<VerifyCommand>,
    timeout_secs: u64,
    /// `timeout_secs` from the trusted project file; it applies only to its own commands.
    custom_timeout: Option<u64>,
    source: [u8; 32],
    binding: Option<exec_ctx::WorkspaceBinding>,
}

impl FrozenVerifyPlan {
    pub(crate) fn prepare(root: &Path, timeout_secs: u64) -> Result<Self> {
        crate::agent::planning::check_automation_boundary()?;
        let root = root
            .canonicalize()
            .context("verification workspace unavailable")?;
        let source = source_fingerprint(&root)
            .context("cannot capture bounded verification source identity")?;
        let commands = detect_verify_commands(&root);
        ensure!(
            !commands.is_empty() && commands.len() <= 8,
            "destination requires 1-8 supported verification commands"
        );
        ensure!(
            commands.iter().all(|command| {
                let text = command.command_line();
                !text.trim().is_empty()
                    && text.len() <= 4096
                    && !matches!(
                        crate::agent::cmd_guard::classify(&text),
                        crate::agent::cmd_guard::Verdict::Blocked(_)
                    )
            }),
            "verification command is invalid or exceeds its limit"
        );
        ensure!(
            source_fingerprint(&root) == Some(source),
            "source changed during verification preparation"
        );
        let custom_timeout = custom_verify_timeout(&root);
        Ok(Self {
            root,
            commands,
            timeout_secs: timeout_secs.clamp(1, 600),
            custom_timeout,
            source,
            binding: exec_ctx::current().and_then(|ctx| ctx.workspace_binding().cloned()),
        })
    }

    pub(crate) fn commands(&self) -> Vec<String> {
        self.commands
            .iter()
            .map(VerifyCommand::command_line)
            .collect()
    }
    pub(crate) fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    pub(crate) fn source_identity(&self) -> String {
        self.source
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        crate::agent::planning::check_automation_boundary()?;
        exec_ctx::current()
            .unwrap_or_default()
            .require_current_binding(self.binding.as_ref())?;
        ensure!(
            self.root.is_dir() && source_fingerprint(&self.root) == Some(self.source),
            "verification source changed after approval; prepare again"
        );
        ensure!(
            detect_verify_commands(&self.root) == self.commands
                && custom_verify_timeout(&self.root) == self.custom_timeout,
            "verification commands or project trust changed after approval; prepare again"
        );
        Ok(())
    }

    /// Called before publishing a running record. A synchronous tool bridge cannot nest a
    /// current-thread runtime; ordinary CLI/REPL hosts are multi-thread or standalone.
    pub(crate) fn require_runtime() -> Result<()> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            ensure!(
                handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread,
                "destination verification cannot run inside a current-thread runtime; run it from the CLI or a multithread host"
            );
        }
        Ok(())
    }

    pub(crate) fn execute(
        &self,
        ctx: &ExecutionContext,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<VerifyGateResult> {
        Self::require_runtime()?;
        let token = cancel::current();
        let mut future = Box::pin(self.run(ctx, owner));
        let scoped = std::future::poll_fn(|cx| {
            let mut poll = || exec_ctx::with_current(ctx.clone(), || future.as_mut().poll(cx));
            match &token {
                Some(token) => cancel::with_current(token.clone(), poll),
                None => poll(),
            }
        });
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(scoped)),
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building verification runtime")?
                .block_on(scoped),
        }
    }

    async fn run(
        &self,
        ctx: &ExecutionContext,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<VerifyGateResult> {
        self.revalidate()?;
        let start = Instant::now();
        let limit = start + Duration::from_secs(self.timeout_secs);
        let deadline = ctx.deadline().map_or(limit, |d| d.min(limit));
        let mut passed = Vec::new();
        let mut last = None;
        for command in &self.commands {
            self.revalidate()?;
            let secs = match command {
                VerifyCommand::Custom(_) => self.custom_timeout.unwrap_or(self.timeout_secs),
                _ => self.timeout_secs,
            };
            let budget = deadline.saturating_duration_since(Instant::now());
            // Sub-second remainders still get one bounded attempt; only an exhausted deadline stops.
            let remaining = budget.as_secs().max(u64::from(!budget.is_zero())).min(secs);
            if cancel::current().is_some_and(|token| token.is_cancelled()) || budget.is_zero() {
                return verify_terminated(
                    "verify",
                    start,
                    if budget.is_zero() {
                        VerifyTermination::TimedOut
                    } else {
                        VerifyTermination::Cancelled
                    },
                    "verification stopped before the next approved command",
                )
                .context("missing stop result");
            }
            let mut result =
                run_one_verify_owned(&self.root, command, remaining, ctx, Some(owner.clone()))
                    .await
                    .context("verification returned no outcome")?;
            if result.effect_state != crate::agent::tools::ToolEffectState::Settled {
                return Ok(result);
            }
            validate_source_result(&mut result, &self.root, Some(self.source));
            if !result.accepted() {
                return Ok(result);
            }
            passed.push(command.command_line());
            last = Some(result);
        }
        let mut result = validate_final_result(last, &self.root, Some(self.source))
            .context("no verification command completed")?;
        if result.accepted() {
            result.passed_commands = passed;
        }
        Ok(result)
    }
}
