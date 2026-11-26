use super::*;

fn parse(args: &[&str]) -> Result<AgentArgs, clap::Error> {
    let cli = Cli::try_parse_from(["aizen", "agent"].into_iter().chain(args.iter().copied()))?;
    match cli.command {
        Some(Commands::Agent(args)) => Ok(args),
        _ => unreachable!("agent command"),
    }
}

#[test]
fn bare_plan_never_consumes_the_positional_task() {
    for args in [
        vec!["--plan", "inspect code"],
        vec!["inspect code", "--plan"],
    ] {
        let parsed = parse(&args).unwrap();
        assert_eq!(parsed.plan, Some(None));
        assert_eq!(parsed.task.as_deref(), Some("inspect code"));
        assert!(!parsed.exit_plan);
    }
    let parsed = parse(&["--plan", "docs/plans/task.md"]).unwrap();
    assert_eq!(parsed.task.as_deref(), Some("docs/plans/task.md"));
    assert_eq!(parsed.plan, Some(None));
    assert!(parse(&["--plan", "docs/plans/task.md", "inspect code"]).is_err());
}

#[test]
fn plan_path_requires_equals_and_coexists_with_task_file_resume() {
    let parsed = parse(&[
        "--plan=docs/plans/task.md",
        "--task-file",
        "task.txt",
        "--resume",
        "saved",
    ])
    .unwrap();
    assert_eq!(parsed.plan, Some(Some("docs/plans/task.md".into())));
    assert!(parsed.task.is_none());
    assert_eq!(parsed.resume.as_deref(), Some("saved"));
    assert!(parse(&["--plan=", "inspect"]).is_err());
}

#[test]
fn plan_and_exit_are_mutually_exclusive_and_omission_is_distinct() {
    for args in [
        vec!["--plan", "--exit-plan", "inspect"],
        vec!["--exit-plan", "--plan=docs/plans/task.md", "inspect"],
    ] {
        assert!(parse(&args).is_err());
    }
    let normal = parse(&["inspect"]).unwrap();
    assert!(normal.plan.is_none());
    assert!(!normal.exit_plan);
    let exit = parse(&["--exit-plan", "--resume", "saved", "implement"]).unwrap();
    assert!(exit.exit_plan);
    assert!(exit.plan.is_none());
}
