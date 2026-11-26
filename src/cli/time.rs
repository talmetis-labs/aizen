//! `aizen time …` and `/time` inspect real Git history and share the existing worktree adapters.
//! There is no checkpoint store, numeric revision translation, or conversation rewind here.

use crate::cli_args::TimeCmd;
use crate::core::{exec_ctx, git_ops};
use crate::features::coop;
use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

pub(crate) fn run_time(cmd: TimeCmd) -> Result<()> {
    for line in time_lines(cmd)? {
        println!("{line}");
    }
    Ok(())
}

/// The REPL uses the same closed argument schema as the CLI, never an arbitrary Git command.
/// Whitespace separates arguments here; use the CLI for resource paths containing spaces.
pub(crate) fn parse_time_args(arg: &str) -> Result<TimeCmd> {
    #[derive(Parser)]
    #[command(name = "/time", disable_version_flag = true)]
    struct TimeArguments {
        #[command(subcommand)]
        command: TimeCmd,
    }
    let arg = if arg.trim().is_empty() {
        "status"
    } else {
        arg.trim()
    };
    // Focus consumes one literal path, including spaces. This is not a shell parser: no expansion,
    // escaping or additional options. Other commands retain their existing closed argument grammar.
    if let Some(path) = arg
        .strip_prefix("focus")
        .filter(|rest| rest.starts_with(char::is_whitespace))
    {
        let path = path.trim();
        let path = path
            .strip_prefix('"')
            .and_then(|p| p.strip_suffix('"'))
            .unwrap_or(path);
        return Ok(TimeArguments::try_parse_from(["/time", "focus", path])?.command);
    }
    Ok(
        TimeArguments::try_parse_from(std::iter::once("/time").chain(arg.split_whitespace()))?
            .command,
    )
}

fn inspection_root() -> Result<PathBuf> {
    if let Some(root) = exec_ctx::current().and_then(|ctx| {
        ctx.placement()
            .map(|placement| placement.workspace_root().to_path_buf())
    }) {
        return Ok(root);
    }
    std::env::current_dir().context("locating the current Git worktree")
}

/// Text is returned to the caller so REPL output always goes through the retained renderer.
/// Failed Git queries, unsupported operations and unknown settlement are errors, not empty success.
pub(crate) fn time_lines(cmd: TimeCmd) -> Result<Vec<String>> {
    match cmd {
        TimeCmd::Focus { .. } => anyhow::bail!("focus requires /time focus in the owning interactive session; no workspace was changed"),
        TimeCmd::Create { name, base, yes } => {
            let prepared = coop::prepare_work_new(&inspection_root()?, &name, base.as_deref())?;
            let mut lines = vec![prepared.preview()?];
            if !yes {
                lines.push("Preview only — no worktree or branch was created. Re-run with --yes to approve creation; the base and source state will be prepared again.".into());
                return Ok(lines);
            }
            let wt = prepared.execute()?;
            lines.extend([
                format!("created worktree {} on {}", wt.path.display(), wt.branch),
                format!(
                    "committed base {}; pending parent edits are not copied",
                    wt.base.as_str()
                ),
                format!("open a separate session there: cd {}", wt.path.display()),
            ]);
            if wt.dirty {
                lines.push("checkout left uncommitted/untracked/ignored files; inspect before using this worktree".into());
            }
            Ok(lines)
        }
        TimeCmd::Remove { name } => Ok(vec![coop::work_remove(&name, false)?]),
        TimeCmd::Worktrees => coop::work_inventory_lines(),
        TimeCmd::Switch { branch, yes } => change_lines(git_ops::Change::Switch { branch }, yes),
        TimeCmd::Undo {
            commits,
            mainline,
            yes,
        } => change_lines(git_ops::Change::Revert { commits, mainline }, yes),
        TimeCmd::UndoPaths { commit, paths, mainline, yes } => {
            change_lines(git_ops::Change::RevertPaths { commit, paths, mainline }, yes)
        }
        TimeCmd::Redo { receipt, yes } => change_lines(git_ops::Change::Redo { receipt }, yes),
        TimeCmd::Reconcile { receipt, yes } => {
            change_lines(git_ops::Change::Reconcile { receipt }, yes)
        }
        TimeCmd::Operations { receipt } => {
            let root = inspection_root()?;
            let report = match receipt {
                Some(id) => serde_json::to_string_pretty(&git_ops::inspect_operation(&root, &id)?)?,
                None => serde_json::to_string_pretty(&git_ops::operation_receipts(&root)?)?,
            };
            Ok(vec![report])
        }
        TimeCmd::Verifications => Ok(vec![serde_json::to_string_pretty(
            &git_ops::verification_records(&inspection_root()?)?,
        )?]),
        TimeCmd::DeleteBranch {
            branch,
            retained_ref,
            yes,
        } => change_lines(
            git_ops::Change::DeleteBranch {
                branch,
                retained_ref,
            },
            yes,
        ),
        TimeCmd::Integrate { commit, yes } => {
            change_lines(git_ops::Change::Integrate { commit }, yes)
        }
        TimeCmd::IntegrateTask { receipt, yes } => {
            change_lines(git_ops::Change::IntegrateTask { receipt }, yes)
        }
        TimeCmd::VerifyTask { receipt, yes } => {
            change_lines(git_ops::Change::VerifyTask { receipt }, yes)
        }
        TimeCmd::Sequencer {
            receipt,
            step,
            yes,
        } => change_lines(git_ops::Change::Sequencer { receipt, step }, yes),
        TimeCmd::Merge { commit, yes } => change_lines(git_ops::Change::Merge { commit }, yes),
        TimeCmd::CherryPick {
            commits,
            mainline,
            yes,
        } => change_lines(git_ops::Change::CherryPick { commits, mainline }, yes),
        TimeCmd::Recovery {            cohort,
            prepared,
            resources,
            json,
            record_observation,
        } => recovery_lines(&cohort, &prepared, &resources, json, record_observation),
        TimeCmd::Status => inspect_lines(
            &inspection_root()?,
            "Git worktree status",
            vec![
                "status".into(),
                "--short".into(),
                "--branch".into(),
                "--untracked-files=all".into(),
            ],
        ),
        TimeCmd::History { limit } => {
            let mut lines = inspect_lines(
                &inspection_root()?,
                "Git commit history (HEAD)",
                vec![
                    "log".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--format=%h %s".into(),
                    format!("--max-count={limit}"),
                    "--".into(),
                ],
            )?;
            lines.push(
                "Git commits do not include pending worktree/index edits or untracked files."
                    .into(),
            );
            Ok(lines)
        }
        TimeCmd::Branches => inspect_lines(
            &inspection_root()?,
            "Local Git branches",
            vec![
                "branch".into(),
                "--list".into(),
                "--no-color".into(),
                "--no-abbrev".into(),
                "--format=%(HEAD) %(refname:short) %(objectname)".into(),
            ],
        ),
        TimeCmd::Reflog { limit } => {
            let mut lines = inspect_lines(
                &inspection_root()?,
                "Local Git reflog (HEAD)",
                vec![
                    "reflog".into(),
                    "show".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--format=%h %gd %gs".into(),
                    format!("--max-count={limit}"),
                    "HEAD".into(),
                    "--".into(),
                ],
            )?;
            lines.push("Reflog retention is controlled by Git; it does not save uncommitted or untracked files.".into());
            Ok(lines)
        }
    }
}

fn change_lines(change: git_ops::Change, yes: bool) -> Result<Vec<String>> {
    let inverse = matches!(change, git_ops::Change::RevertPaths { .. });
    let reconcile = matches!(change, git_ops::Change::Reconcile { .. });
    let verification = matches!(change, git_ops::Change::VerifyTask { .. });
    let sequencer = matches!(change, git_ops::Change::Sequencer { .. });
    let prepared = git_ops::Prepared::prepare(&inspection_root()?, change)?;
    let mut lines = vec![prepared.preview()?];
    if !yes {
        lines.push("Preview only — nothing was executed. Re-run with --yes for explicit approval; state, targets and the approved command list are prepared again and revalidated.".into());
        return Ok(lines);
    }
    // Execute this prepared object, never resolve a fresh moving target after preparation.
    let outcome = prepared.execute()?;
    lines.push(match &outcome {
        git_ops::PreparedOutcome::Change(receipt) => serde_json::to_string_pretty(receipt)?,
        git_ops::PreparedOutcome::Verify(record) => serde_json::to_string_pretty(record)?,
        git_ops::PreparedOutcome::Handoff(handoff) => serde_json::to_string_pretty(handoff)?,
    });
    lines.push(if inverse {
        "Selected inverse committed only in its owned worktree, UNVERIFIED and NOT INTEGRATED. Approve integrate-task <registration-id> separately, then verify-task <integration-id> at the destination. No destination edits or cleanup were performed.".into()
    } else if reconcile {
        "Manual resolution acknowledged in metadata only; original failure and partial commit mappings are retained. No Git action was replayed and project verification has NOT run.".into()
    } else if verification {
        "Verification evidence recorded or relinked for this exact destination. Only a settled pass marks the integration verified; relinking runs no command, and a failure or unknown effect is never inferred as success.".into()
    } else if sequencer {
        "Sequencer step settled; project verification has NOT run. A remaining conflict is reported, not cleaned up; inspect Git state before continuing.".into()
    } else {
        "Git operation settled; project verification has NOT run. Inspect the resulting destination and run its required tests before treating the work as complete.".into()
    });
    Ok(lines)
}

fn inspect_lines(root: &Path, title: &str, args: Vec<String>) -> Result<Vec<String>> {
    let bytes = git_ops::inspection_bytes(root, args)?;
    let text = std::str::from_utf8(&bytes).context("Git inspection output is not UTF-8")?;
    // Git metadata is untrusted terminal text. Preserve printable text, not terminal control codes.
    let mut lines = vec![title.to_string()];
    lines.extend(text.lines().map(|line| {
        line.chars()
            .flat_map(|c| {
                if c.is_control() {
                    c.escape_default().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect()
    }));
    if text.trim().is_empty() {
        lines.push("(no entries returned by Git)".into());
    }
    Ok(lines)
}

/// Historical speculation evidence remains readable without granting restore or replay authority.
/// A metadata append requires its own explicit flag; there is no backend repair/GC route.
fn recovery_lines(
    cohort: &str,
    prepared: &str,
    resources: &[String],
    json: bool,
    record_observation: bool,
) -> Result<Vec<String>> {
    use crate::core::exec_ctx::ExecutionContext;
    use crate::core::speculation::restart;
    let root = inspection_root()?;
    let inspect = || {
        if record_observation {
            restart::record_speculation_recovery_observation(&root, cohort, prepared, resources)
        } else {
            restart::inspect_speculation_recovery(&root, cohort, prepared, resources)
        }
    };
    // Only this user-command boundary may seat a parent; never replace an existing restriction.
    let report = if exec_ctx::current().is_some() {
        inspect()?
    } else {
        let parent = ExecutionContext::default()
            .with_resource_scope(format!("cli/recovery-report/{}", std::process::id()));
        exec_ctx::with_current(parent, inspect)?
    };
    Ok(if json {
        vec![serde_json::to_string_pretty(&report.to_json())?]
    } else {
        vec![report.to_text()]
    })
}

/// Retired checkpoint commands are never interpreted as Git revisions or successful no-ops.
pub(crate) fn unsupported_time_hint(arg: &str) -> Option<&'static str> {
    match arg.split_whitespace().next()? {
        "save" | "list" | "restore" | "diff" | "prune" | "rm" | "gc" | "clear" | "doctor" =>
            Some("Checkpoint storage and restore commands were removed. Use /time status, history, branches, worktrees or reflog. Historical numeric IDs are not Git revisions; saved evidence has not been deleted."),
        "continue" | "abort" | "quit" =>
            Some("Use `/time sequencer <failed-operation-id> continue|abort|quit` for a recognized merge/cherry-pick/revert conflict. Root-level continue/abort are not standalone commands; unresolved receipts must not be deleted to bypass a refusal."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_time_is_read_only_status() {
        assert!(matches!(parse_time_args("").unwrap(), TimeCmd::Status));
        assert!(matches!(
            parse_time_args("history --limit 7").unwrap(),
            TimeCmd::History { limit: 7 }
        ));
        assert!(
            matches!(parse_time_args("create topic").unwrap(), TimeCmd::Create { name, base: None, yes: false } if name == "topic")
        );
        assert!(
            matches!(parse_time_args("create topic --base HEAD~2 --yes").unwrap(), TimeCmd::Create { name, base: Some(base), yes: true } if name == "topic" && base == "HEAD~2")
        );
    }

    #[test]
    fn checkpoint_ids_and_unsupported_mutations_are_never_git_arguments() {
        for input in [
            "42",
            "restore 42",
            "diff 42",
            "undo",
            "redo",
            "undo 42 --yes",
            "redo #42 --yes",
            "redo ../escape --yes",
            "redo HEAD~1 --yes",
            "redo one two --yes",
            "integrate 42 --yes",
            "create topic --base 42 --yes",
            "create topic --base --force",
            "create topic --force",
            "save label",
            "clear",
            "gc --all --apply",
            "doctor --repair",
            "focus",
            "delete-branch topic",
            "delete-branch topic --retained-ref refs/heads/main --force",
        ] {
            assert!(parse_time_args(input).is_err(), "accepted {input}");
        }
        assert!(unsupported_time_hint("restore 42")
            .unwrap()
            .contains("not Git revisions"));
        assert!(
            matches!(parse_time_args("focus main").unwrap(), TimeCmd::Focus { path } if path == "main")
        );
        assert!(unsupported_time_hint("focus main").is_none());
        assert!(time_lines(TimeCmd::Focus {
            path: "main".into()
        })
        .unwrap_err()
        .to_string()
        .contains("owning interactive"));
        // Root-level continue/abort are redirected to the owned sequencer subcommand.
        assert!(unsupported_time_hint("continue")
            .unwrap()
            .contains("/time sequencer"));
    }

    #[test]
    fn mutations_require_explicit_targets_and_default_to_preview() {
        assert!(
            matches!(parse_time_args("undo-paths HEAD --path selected.txt --path renamed.txt").unwrap(),
            TimeCmd::UndoPaths { paths, yes: false, mainline: None, .. } if paths == ["selected.txt", "renamed.txt"])
        );
        for bad in [
            "undo-paths HEAD",
            "undo-paths 42 --path file.txt",
            "undo-paths HEAD --path file.txt --force",
        ] {
            assert!(parse_time_args(bad).is_err(), "accepted {bad}");
        }
        assert!(matches!(
            parse_time_args("switch topic").unwrap(),
            TimeCmd::Switch { yes: false, .. }
        ));
        assert!(
            matches!(parse_time_args("undo HEAD~1 HEAD --yes").unwrap(), TimeCmd::Undo { commits, mainline: None, yes: true } if commits == ["HEAD~1", "HEAD"])
        );
        assert!(matches!(
            parse_time_args("undo HEAD --mainline 2").unwrap(),
            TimeCmd::Undo {
                mainline: Some(2),
                yes: false,
                ..
            }
        ));
        assert!(matches!(
            parse_time_args("delete-branch topic --retained-ref refs/heads/main").unwrap(),
            TimeCmd::DeleteBranch { branch, retained_ref, yes: false }
                if branch == "topic" && retained_ref == "refs/heads/main"
        ));
        assert!(matches!(
            parse_time_args("delete-branch topic --retained-ref refs/heads/main --yes").unwrap(),
            TimeCmd::DeleteBranch { yes: true, .. }
        ));
        assert!(matches!(
            parse_time_args("operations").unwrap(),
            TimeCmd::Operations { receipt: None }
        ));
        assert!(matches!(
            parse_time_args("verifications").unwrap(),
            TimeCmd::Verifications
        ));
        assert!(matches!(
            parse_time_args("redo undo-operation-1").unwrap(),
            TimeCmd::Redo { receipt, yes: false } if receipt == "undo-operation-1"
        ));
        assert!(matches!(
            parse_time_args("integrate refs/heads/topic").unwrap(),
            TimeCmd::Integrate { yes: false, .. }
        ));
        assert!(matches!(
            parse_time_args("integrate-task git-task-1-2-3").unwrap(),
            TimeCmd::IntegrateTask { receipt, yes: false } if receipt == "git-task-1-2-3"
        ));
        assert!(matches!(
            parse_time_args("integrate-task git-task-1-2-3 --yes").unwrap(),
            TimeCmd::IntegrateTask { yes: true, .. }
        ));
        assert!(matches!(
            parse_time_args("verify-task 123-456-7").unwrap(),
            TimeCmd::VerifyTask { receipt, yes: false } if receipt == "123-456-7"
        ));
        assert!(matches!(
            parse_time_args("verify-task 123-456-7 --yes").unwrap(),
            TimeCmd::VerifyTask { yes: true, .. }
        ));
        assert!(matches!(
            parse_time_args("sequencer failed-operation-1 abort").unwrap(),
            TimeCmd::Sequencer { receipt, step, yes: false }
                if receipt == "failed-operation-1" && step == "abort"
        ));
        assert!(matches!(
            parse_time_args("sequencer failed-operation-1 continue --yes").unwrap(),
            TimeCmd::Sequencer { step, yes: true, .. } if step == "continue"
        ));
        for bad in [
            "sequencer",
            "sequencer failed-operation-1",
            "sequencer ../escape abort",
            "sequencer failed-operation-1 reset",
        ] {
            assert!(parse_time_args(bad).is_err(), "accepted {bad}");
        }
        for bad in [
            "integrate-task",
            "integrate-task 42",
            "integrate-task ../escape",
            "integrate-task git-task-1 --force",
            "verify-task",
            "verify-task ../escape",
            "verify-task 123-456-7 --force",
        ] {
            assert!(parse_time_args(bad).is_err(), "accepted {bad}");
        }
        assert!(matches!(
            parse_time_args("merge refs/heads/topic").unwrap(),
            TimeCmd::Merge { yes: false, .. }
        ));
        assert!(matches!(
            parse_time_args("cherry-pick HEAD~1 HEAD --yes").unwrap(),
            TimeCmd::CherryPick { commits, mainline: None, yes: true }
                if commits == ["HEAD~1", "HEAD"]
        ));
        assert!(matches!(
            parse_time_args("cherry-pick HEAD --mainline 2").unwrap(),
            TimeCmd::CherryPick {
                mainline: Some(2),
                yes: false,
                ..
            }
        ));
        assert!(matches!(
            parse_time_args("reconcile failed-operation-1").unwrap(),
            TimeCmd::Reconcile { yes: false, .. }
        ));
        assert!(parse_time_args("reconcile ../escape --yes").is_err());
        assert!(parse_time_args("reconcile 42 --yes").is_err());
        assert!(parse_time_args("reconcile failed-operation-1 --force").is_err());
        assert!(unsupported_time_hint("merge topic").is_none());
        assert!(unsupported_time_hint("cherry-pick HEAD").is_none());
        for bad in [
            "merge 42",
            "merge HEAD --force",
            "cherry-pick",
            "cherry-pick 42",
            "cherry-pick HEAD --mainline 0",
            "cherry-pick HEAD --skip",
        ] {
            assert!(parse_time_args(bad).is_err(), "accepted {bad}");
        }
        assert!(parse_time_args("undo HEAD --mainline 0").is_err());
        assert!(parse_time_args("redo --yes").is_err());
    }

    #[test]
    fn inspection_limits_and_extra_arguments_are_rejected() {
        for input in [
            "history --limit 0",
            "history --limit 1001",
            "status --force",
            "reflog 42",
            "branches main",
            "remove topic --force",
        ] {
            assert!(parse_time_args(input).is_err(), "accepted {input}");
        }
    }
}

#[cfg(test)]
mod operation_inspection_args_tests {
    use super::*;

    #[test]
    fn exact_operation_inspection_preserves_list_grammar_and_refuses_other_actions() {
        assert!(matches!(
            parse_time_args("operations").unwrap(),
            TimeCmd::Operations { receipt: None }
        ));
        assert!(
            matches!(parse_time_args("operations --receipt failed-operation-1").unwrap(),
            TimeCmd::Operations { receipt: Some(id) } if id == "failed-operation-1")
        );
        for input in [
            "operations --receipt",
            "operations --receipt ../escape",
            "operations --receipt 42",
            "operations --receipt valid-id --yes",
            "operations --receipt valid-id --force",
            "operations --receipt one-id --receipt two-id",
        ] {
            assert!(parse_time_args(input).is_err(), "accepted {input}");
        }
    }
}
