//! `aizen sandbox <status|doctor|explain|run>` — the operator's view of the sandbox subsystem.
//!
//! Everything printed here is measured, not asserted: the capability matrix comes from the
//! runtime probe, the doctor actually spawns a child for its env-scrub self-test, and `run`
//! executes the given command through the exact same runner every model command uses.

use crate::cli_args::SandboxCliCmd;
use crate::sandbox::{self, capabilities, policy, request::SandboxRequest, runner, CommandOrigin};
use anyhow::Result;
use console::style;

pub(crate) fn run_sandbox(cmd: SandboxCliCmd) -> Result<()> {
    match cmd {
        SandboxCliCmd::Review { topic } => crate::sandbox::review::run(&topic),
        SandboxCliCmd::Status => status(),
        SandboxCliCmd::Doctor { json } => doctor(json),
        SandboxCliCmd::Explain => explain(),
        SandboxCliCmd::Run { command, network } => run_probe(&command.join(" "), network),
    }
}

fn enforcement_line(name: &str, e: capabilities::Enforcement) -> String {
    let tag = match e {
        capabilities::Enforcement::Enforced => style("enforced").green().to_string(),
        capabilities::Enforcement::Partial => style("partial").yellow().to_string(),
        capabilities::Enforcement::Advisory => style("advisory").yellow().to_string(),
        capabilities::Enforcement::Unavailable => style("unavailable").red().to_string(),
    };
    format!("  {name:<22} {tag}")
}

fn status() -> Result<()> {
    let mode = sandbox::mode();
    let report = capabilities::probe();
    let settings = policy::settings();
    println!(
        "sandbox mode: {mode}   backend: {}",
        report.backend.as_str()
    );
    println!(
        "network default: {}   unattended guarded fallback: {}",
        if policy::network_default_allow(&settings) {
            "allow (weakened by config)"
        } else {
            "deny"
        },
        if settings.allow_guarded_fallback.unwrap_or(false) {
            "allowed by config"
        } else {
            "off (fail-closed)"
        },
    );
    println!("capabilities on this machine:");
    println!("{}", enforcement_line("filesystem read", report.fs_read));
    println!("{}", enforcement_line("filesystem write", report.fs_write));
    println!("{}", enforcement_line("network deny", report.network_deny));
    println!(
        "{}",
        enforcement_line("env isolation", report.env_isolation)
    );
    println!(
        "{}",
        enforcement_line("process containment", report.process_containment)
    );
    println!(
        "{}",
        enforcement_line("resource limits", report.resource_limits)
    );
    if !report.notes.is_empty() {
        println!("notes:");
        for n in &report.notes {
            println!("  • {n}");
        }
    }
    println!(
        "audit log: {}",
        crate::sandbox::audit::audit_dir()
            .join("sandbox.jsonl")
            .display()
    );
    Ok(())
}

fn doctor(json: bool) -> Result<()> {
    let report = capabilities::probe();
    let settings = policy::settings();
    let mode = sandbox::mode();

    // 1) Audit log writable?
    let audit_ok = {
        let dir = crate::sandbox::audit::audit_dir();
        std::fs::create_dir_all(&dir).is_ok()
            && std::fs::write(dir.join(".doctor-probe"), b"ok")
                .map(|()| {
                    let _ = std::fs::remove_file(dir.join(".doctor-probe"));
                })
                .is_ok()
    };

    // 2) Env-scrub self-test: spawn a real child and confirm a planted secret does not appear.
    let scrub_ok = env_scrub_selftest();

    // 3) Stale private-temp sweep.
    let swept = crate::sandbox::backend::guarded::sweep_stale_tmp();

    // 4) Config review.
    let mut warnings: Vec<String> = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if policy::workspace_too_broad(&cwd)
        && !settings.dangerous_allow_broad_workspace.unwrap_or(false)
    {
        warnings.push(format!(
            "current directory {} is a root/home/system directory — a workspace-write sandbox \
             around it protects nothing",
            cwd.display()
        ));
    }
    if policy::network_default_allow(&settings) {
        warnings.push(
            "config sets network=allow: children get network without per-command grants".into(),
        );
    }
    if mode == sandbox::SandboxMode::Off {
        warnings.push("sandbox mode is OFF: children inherit the full environment".into());
    }
    for e in &settings.pass_env {
        if e.trim() == "*" {
            warnings.push("pass_env contains a bare `*` entry (ignored, but remove it)".into());
        }
    }

    if json {
        let out = serde_json::json!({
            "mode": mode.as_str(),
            "capabilities": report,
            "audit_log_writable": audit_ok,
            "env_scrub_selftest": scrub_ok,
            "stale_tmp_swept": swept,
            "warnings": warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "sandbox doctor — mode {mode}, backend {}",
        report.backend.as_str()
    );
    let check = |ok: bool| {
        if ok {
            style("ok").green()
        } else {
            style("FAILED").red()
        }
    };
    println!("  audit log writable      {}", check(audit_ok));
    println!(
        "  env-scrub self-test     {}",
        match scrub_ok {
            Some(true) => style("ok").green().to_string(),
            Some(false) => style("FAILED — a planted secret reached a child")
                .red()
                .to_string(),
            None => style("skipped (could not spawn a shell)")
                .yellow()
                .to_string(),
        }
    );
    println!("  stale temp dirs swept   {swept}");
    if warnings.is_empty() {
        println!("  configuration           {}", style("ok").green());
    } else {
        for w in &warnings {
            println!("  {} {w}", style("warning:").yellow());
        }
    }
    Ok(())
}

/// Spawn a real child with a planted secret in OUR env and check the child does not see it.
/// `None` when no shell could be spawned at all (the test then proves nothing).
fn env_scrub_selftest() -> Option<bool> {
    std::env::set_var("AIZEN_DOCTOR_PROBE_TOKEN", "doctor-secret");
    let line = if cfg!(windows) { "set" } else { "env" };
    let cwd = std::env::current_dir().ok()?;
    let mut sbx = runner::prepare_std(
        SandboxRequest::shell(
            CommandOrigin::InternalTrusted("sandbox doctor env-scrub self-test"),
            line,
            cwd.clone(),
            cwd,
        )
        .private_tmp(false),
    )
    .ok()?;
    let out = sbx
        .command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    std::env::remove_var("AIZEN_DOCTOR_PROBE_TOKEN");
    let out = out.ok()?;
    sbx.finish(runner::Outcome::Exit(out.status.code()));
    let dump = String::from_utf8_lossy(&out.stdout);
    if dump.trim().is_empty() {
        return None;
    }
    Some(!dump.contains("AIZEN_DOCTOR_PROBE_TOKEN"))
}

fn explain() -> Result<()> {
    let mode = sandbox::mode();
    let report = capabilities::probe();
    let settings = policy::settings();
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    println!("how the next shell_run would be sandboxed:");
    println!(
        "  1. mode: {} (runtime override > AIZEN_SANDBOX > config.sandbox.mode > auto)",
        mode
    );
    println!("  2. backend: {}", report.backend.as_str());
    match mode {
        sandbox::SandboxMode::Off => {
            println!("  3. off: no env scrub, no limits — hard command floor + approval + tree containment still apply");
            return Ok(());
        }
        sandbox::SandboxMode::Strict if !report.fs_write.kernel_backed() => {
            println!("  3. strict on this platform: every spawn is REFUSED (no kernel filesystem backend)");
            return Ok(());
        }
        _ => {}
    }
    let scrubbed = policy::scrubbed_count(&settings.pass_env);
    println!(
        "  3. environment: {scrubbed} secret-shaped variables withheld; pass_env restores {}",
        settings.pass_env.len()
    );
    println!(
        "  4. network: {}",
        if policy::network_default_allow(&settings) {
            "allow (config)"
        } else {
            "deny unless the call declares network:true (approval-gated)"
        }
    );
    let fs = policy::fs_policy(&cwd, None, &settings);
    println!("  5. filesystem policy (kernel-enforced only where status says so):");
    for p in fs.read_write.iter().take(6) {
        println!("       rw  {}", p.display());
    }
    println!(
        "       ro  system/toolchain roots ({} entries)",
        fs.read_only.len()
    );
    println!(
        "       deny {} credential directories (~/.ssh, ~/.aws, ~/.aizen, …)",
        fs.deny.len()
    );
    let limits = settings.limits.unwrap_or_default();
    println!(
        "  6. limits: processes ≤ {}, memory {}, wall {}s (foreground)",
        limits
            .max_processes
            .unwrap_or(policy::DEFAULT_MAX_PROCESSES),
        limits
            .memory_mb
            .map(|m| format!("{m} MiB"))
            .unwrap_or_else(|| "unlimited".into()),
        limits.wall_seconds.unwrap_or(120),
    );
    println!("  7. audit: one JSONL line per spawn → ~/.aizen/audit/sandbox.jsonl");
    Ok(())
}

fn run_probe(command: &str, network: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let sbx = runner::prepare_std(
        SandboxRequest::shell(CommandOrigin::UserEscape, command, cwd.clone(), cwd)
            .network(network),
    )?;
    if !sbx.degraded.is_empty() {
        for d in &sbx.degraded {
            println!("{} {d}", style("degraded:").yellow());
        }
    }
    // Move the guard into the physical body; returning from a timed-out observer must not drop
    // sandbox temp or originating policy ownership while a child/pipe worker is still alive.
    let output = sbx.output_bounded(
        std::time::Duration::from_secs(120),
        std::time::Duration::from_secs(2),
    )?;
    if !output.stdout.trim().is_empty() {
        println!("{}", output.stdout.trim_end());
    }
    if !output.stderr.trim().is_empty() {
        eprintln!("{}", output.stderr.trim_end());
    }
    if output.output_truncated {
        eprintln!("sandbox run: output capture truncated");
    }
    let label = probe_output_label(&output)?;
    println!("{} {label}", style("sandbox run:").dim());
    Ok(())
}

fn probe_output_label(output: &crate::core::proctree::BoundedOutput) -> Result<String> {
    if output.settlement == crate::core::proctree::BoundedSettlement::Unknown {
        return Err(crate::core::proctree::BoundedExecutionError {
            cause: std::io::Error::other("sandbox probe physical effects remain unproven"),
            settlement: output.settlement,
        }
        .into());
    }
    Ok(match output.code {
        _ if output.timed_out => "timed out (tree killed)".to_string(),
        Some(code) => format!("exit {code}"),
        None => "killed".to_string(),
    })
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::core::proctree::{BoundedExecutionError, BoundedOutput, BoundedSettlement};

    #[test]
    fn probe_never_labels_unsettled_output_as_exit_or_tree_killed() {
        let mut output = BoundedOutput {
            stdout: "partial probe output".into(),
            stderr: String::new(),
            code: Some(0),
            timed_out: false,
            output_truncated: true,
            settlement: BoundedSettlement::Unknown,
        };
        for timed_out in [false, true] {
            output.timed_out = timed_out;
            let error = probe_output_label(&output).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<BoundedExecutionError>()
                    .unwrap()
                    .settlement,
                BoundedSettlement::Unknown
            );
        }
        output.settlement = BoundedSettlement::Proven;
        assert_eq!(
            probe_output_label(&output).unwrap(),
            "timed out (tree killed)"
        );
        output.timed_out = false;
        assert_eq!(probe_output_label(&output).unwrap(), "exit 0");
        output.code = Some(7);
        assert_eq!(probe_output_label(&output).unwrap(), "exit 7");
        output.code = None;
        assert_eq!(probe_output_label(&output).unwrap(), "killed");
    }
}
