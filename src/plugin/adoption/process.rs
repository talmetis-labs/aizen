//! One-shot offline adoption uses the same physical ownership, containment and drain proof as RPC.
use super::*;
use std::path::Path;

pub(crate) const ADOPTION_TIMEOUT: Duration = Duration::from_secs(120);

/// Admission is shared only with the host operation, never reacquired. The durable marker is
/// removed on physical settlement even for a logical failure: the operation's journal remains
/// explicitly resumable without blocking a healthy unrelated active store.
pub(crate) async fn adoption_execute(
    request: crate::sandbox::request::SandboxRequest,
    admission: Arc<manager::Admission>,
    lease: crate::core::workspace_txn::MemoryProviderLease,
    operation_id: &str,
    private_temp: &Path,
    guarded: bool,
) -> Result<Vec<u8>> {
    check_effects()?;
    let cancel = crate::core::cancel::current().unwrap_or_default().child();
    let deadline = Instant::now() + ADOPTION_TIMEOUT;
    let observer = Observer::new(unsettled());
    let settlement = observer.settlement.clone();
    let mut before_spawn = BeforeSpawnProof::new(settlement.clone());
    let mut prepared =
        crate::sandbox::runner::prepare_tokio(request.wall_timeout(ADOPTION_TIMEOUT))?;
    ensure!(
        prepared.degraded.is_empty() || guarded,
        "adoption requires informed guarded-execution consent on this platform"
    );
    prepared
        .command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let marker = manager::transaction_marker();
    install::ensure_private_root(marker.parent().context("adoption marker has no parent")?)?;
    ensure!(
        !marker.try_exists()?,
        "Memory recovery marker appeared before adoption spawn"
    );
    let bytes = serde_json::to_vec(&json!({"schema_version":1,"kind":"adoption",
        "operation_id":operation_id,"pid":std::process::id()}))?;
    before_spawn.marker = Some((
        marker.clone(),
        crate::core::persist::FileFingerprint::for_bytes(&bytes),
    ));
    crate::core::persist::atomic_write_owner_only(&marker, &bytes)?;
    let terminal = Arc::new(AtomicBool::new(false));
    let spawned =
        before_spawn.spawn_if_live(&cancel, deadline, || prepared.spawn_contained_tokio())?;
    let mut ownership = Ownership {
        _execution: None,
        _admission: admission,
        _lease: lease,
        _sandbox: prepared.into_guard(),
        containment: None,
        marker,
        marker_fingerprint: crate::core::persist::FileFingerprint::for_bytes(&bytes),
        terminal: terminal.clone(),
        private_temp: private_temp.to_owned(),
    };
    let (mut child, containment) = match spawned {
        Ok(child) => child,
        Err(error) if error.cleanup_verified() => {
            terminal.store(true, Ordering::Release);
            drop(ownership);
            settlement.prove_tree_settled();
            return Err(error.into());
        }
        Err(error) => {
            unsettled().charge_permanent();
            FAILED_SPAWNS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(FailedSpawn {
                    _error: error,
                    _ownership: ownership,
                });
            return Err(unknown_worker(
                "adoption spawn settlement_unknown; admission quarantined",
            ));
        }
    };
    let containment = Arc::new(containment);
    ownership.containment = Some(containment.clone());
    let lifetime = Arc::new(SettlementLifetime::new(ownership, quarantine));
    let stdout_drain = lifetime.drain();
    let stderr_drain = lifetime.drain();
    let classification = lifetime.drain();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (released, release) = tokio::sync::oneshot::channel();
    lifetime.on_release(move || {
        settlement.prove_tree_settled();
        let _ = released.send(());
    });
    let exit_success = Arc::new(AtomicBool::new(false));
    let stop = crate::agent::mcp::monitor_contained_settlement_status(
        child,
        containment.clone(),
        lifetime.clone(),
        Some(exit_success.clone()),
    );
    let budget = transport::IoBudget::default();
    let stderr_budget = budget.clone();
    let (failure, mut failed) = tokio::sync::oneshot::channel();
    let stderr_worker = tokio::spawn(async move {
        let _drain = stderr_drain;
        let result = async {
            let mut stderr = stderr.context("adoption stderr pipe missing")?;
            let mut chunk = [0u8; 8192];
            let mut diagnostic = Vec::new();
            loop {
                let count = stderr.read(&mut chunk).await?;
                if count == 0 {
                    return Ok::<_, anyhow::Error>(diagnostic);
                }
                stderr_budget.charge(count)?;
                // Retain only a small private classifier input. Never export provider text.
                let keep = count.min(4096usize.saturating_sub(diagnostic.len()));
                diagnostic.extend_from_slice(&chunk[..keep]);
            }
        }
        .await;
        if result.is_err() {
            let _ = failure.send(());
        }
        result
    });
    let operation = async {
        let _drain = stdout_drain;
        let mut stdout = stdout.context("adoption stdout pipe missing")?;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let count = stdout.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            budget.charge(count)?;
            ensure!(
                bytes.len().saturating_add(count) <= api::MAX_TRANSACTION_IO,
                "adoption report exceeds private plan IO bound"
            );
            bytes.extend_from_slice(&chunk[..count]);
        }
        ensure!(!bytes.is_empty(), "adoption did not return a validated report; source/layout, permissions or journal may require review");
        Ok::<_, anyhow::Error>(bytes)
    };
    let result = tokio::select! {
        result = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), operation) =>
            result.context("adoption execution deadline elapsed; effects may exist").and_then(|r| r),
        _ = cancel.cancelled() => Err(anyhow::anyhow!("adoption cancelled; effects may exist")),
        _ = async { if (&mut failed).await.is_err() { std::future::pending::<()>().await; } } =>
            Err(anyhow::anyhow!("adoption stderr drain failed")),
    };
    if result.is_err() {
        crate::core::proctree::terminate_tree(&containment);
    }
    let diagnostic = match tokio::time::timeout(Duration::from_secs(6), stderr_worker).await {
        Ok(Ok(Ok(bytes))) => Some(bytes),
        _ => None,
    };
    // This marks ONLY physical cleanup authorization, not publication or no-effects. Ownership
    // still retains admission and the marker unless the monitor AND both drains prove release.
    terminal.store(true, Ordering::Release);
    drop(classification);
    if !matches!(
        tokio::time::timeout(Duration::from_secs(11), release).await,
        Ok(Ok(()))
    ) {
        return Err(unknown_worker(
            "adoption settlement_unknown; admission quarantined; do not retry",
        ));
    }
    ensure!(
        diagnostic.is_some(),
        "adoption IO failed after physical settlement; explicit recovery required"
    );
    ensure!(
        !manager::transaction_marker().try_exists()?,
        "adoption settlement marker remains; recovery required"
    );
    drop(stop);
    ensure!(
        exit_success.load(Ordering::Acquire),
        "adoption provider failed; stderr withheld; inspect retained status"
    );
    result.context(diagnostic_reason(diagnostic.as_deref().unwrap_or_default()))
}

/// An untrusted executable's stderr may contain secrets or record bodies. Classify into fixed host
/// messages instead of merely stripping control characters and accidentally exporting their text.
fn diagnostic_reason(_bytes: &[u8]) -> &'static str {
    "Adoption returned no verified report. Provider stderr is withheld; inspect retained status before retrying."
}

#[cfg(test)]
mod tests {
    #[test]
    fn adoption_stderr_diagnostics_never_echo_provider_records() {
        for text in [
            b"secret=abc source drift".as_slice(),
            b"record body: private text",
            b"invalid record: PRIVATE",
            b"terminal publication receipt absent",
        ] {
            let result = super::diagnostic_reason(text);
            assert!(
                !result.contains("PRIVATE")
                    && !result.contains("abc")
                    && !result.contains("private text")
            );
        }
    }
}
