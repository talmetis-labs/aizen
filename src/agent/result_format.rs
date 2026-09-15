//! `format: concise | detailed` for the tools whose output is a log or a match list.
//!
//! `shell_run`, `process` and `search_files` are the three tools whose result is routinely the
//! largest thing in a request and the least read: the model wants the exit code, the first error,
//! the tail and how much there was — not two thousand warning lines. The loop already cuts logs
//! at its budget (16 KB), but a budget is not a shape: a 15 KB `cargo test` log went through whole,
//! and the model paid for it on every later request until it aged out.
//!
//! `concise` is the default and changes nothing for output under [`CONCISE_LOG_CHARS`] — a short
//! result is already concise. Past that, the tool writes the full output to the scratch dir, cuts
//! the way `truncate_log` reads a log (head, first error, tail), and says on its second line how
//! much there was and where the rest is. `detailed` skips the cut; the loop's budget and the 16 KB
//! spill still apply above it. Nothing is lost either way, so the model never has to re-run a
//! build to see a line the concise view dropped.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultFormat {
    Concise,
    Detailed,
}

/// Chars a concise log keeps: the head, the first error window and the tail (see `truncate_log`).
pub const CONCISE_LOG_CHARS: usize = 4_000;
/// Match rows a concise search shows before the rest becomes per-file counts.
pub const CONCISE_SEARCH_ROWS: usize = 40;

/// `format` from the call's arguments; anything but an explicit `detailed` is concise.
pub fn from_args(args: &Value) -> ResultFormat {
    match args.get("format").and_then(Value::as_str) {
        Some("detailed") => ResultFormat::Detailed,
        _ => ResultFormat::Concise,
    }
}

/// The schema property the three tools share.
pub fn schema_property() -> Value {
    serde_json::json!({
        "type": "string",
        "enum": ["concise", "detailed"],
        "description": "default concise: head, first error and tail, full text saved to a file; detailed: everything"
    })
}

/// Concise view of a log-shaped result whose FIRST LINE is its status (`exit N`, `proc_1 [running]
/// next_cursor=…`, `error: …`). Under the budget, or with `format: detailed`, the text is returned
/// untouched.
pub fn finish_log(tool: &str, args: &Value, out: String) -> String {
    if from_args(args) == ResultFormat::Detailed || out.chars().count() <= CONCISE_LOG_CHARS {
        return out;
    }
    let lines = out.lines().count();
    let path = crate::agent::observe::spill_to_scratch(tool, &out);
    let cut = crate::agent::truncate_log(&out, CONCISE_LOG_CHARS);
    let (first, rest) = match cut.split_once('\n') {
        Some((f, r)) => (f.to_string(), r.to_string()),
        None => (cut.clone(), String::new()),
    };
    let mut note = format!(
        "[concise: {lines} lines, {}; showing the head, the first error and the tail",
        crate::agent::observe::human_size(out.len())
    );
    match path {
        Some(p) => note.push_str(&format!(" · full output at {} (file_read it)", p.display())),
        None => note.push_str(" · the full output could not be saved"),
    }
    note.push_str(" · format:\"detailed\" returns everything]");
    format!("{first}\n{note}\n{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_defaults_to_concise_and_only_detailed_opts_out() {
        assert_eq!(from_args(&serde_json::json!({})), ResultFormat::Concise);
        assert_eq!(
            from_args(&serde_json::json!({"format": "concise"})),
            ResultFormat::Concise
        );
        assert_eq!(
            from_args(&serde_json::json!({"format": "detailed"})),
            ResultFormat::Detailed
        );
        assert_eq!(
            from_args(&serde_json::json!({"format": "verbose"})),
            ResultFormat::Concise
        );
    }

    #[test]
    fn a_short_log_passes_through_untouched() {
        let short = "exit 0\nall good".to_string();
        assert_eq!(
            finish_log("shell_run", &serde_json::json!({}), short.clone()),
            short
        );
    }

    #[test]
    fn a_long_log_keeps_its_status_line_and_names_the_spill() {
        let mut log = String::from("exit 101\n");
        for i in 0..400 {
            log.push_str(&format!("warning: unused variable number {i}\n"));
        }
        log.push_str("error[E0308]: mismatched types\n");
        for i in 0..200 {
            log.push_str(&format!("more output line {i}\n"));
        }
        log.push_str("test result: FAILED. 3 passed; 1 failed\n");
        let total_lines = log.lines().count();
        let out = finish_log("shell_run", &serde_json::json!({}), log.clone());
        let mut it = out.lines();
        assert_eq!(it.next(), Some("exit 101"), "status line first: {out}");
        let note = it.next().unwrap();
        assert!(
            note.starts_with(&format!("[concise: {total_lines} lines, ")),
            "{note}"
        );
        assert!(note.contains("-shell_run.txt (file_read it)"), "{note}");
        assert!(out.contains("error[E0308]"), "first error kept: {out}");
        assert!(out.contains("test result: FAILED"), "tail kept: {out}");
        assert!(
            out.chars().count() < CONCISE_LOG_CHARS + 400,
            "cut to the concise budget: {}",
            out.chars().count()
        );
        // The spilled file holds everything.
        let path = note
            .split("full output at ")
            .nth(1)
            .and_then(|s| s.split(" (file_read it)").next())
            .expect("path in note");
        assert_eq!(std::fs::read_to_string(path).unwrap(), log);
        let _ = std::fs::remove_file(path);

        // `detailed` returns the log whole.
        assert_eq!(
            finish_log(
                "shell_run",
                &serde_json::json!({"format": "detailed"}),
                log.clone()
            ),
            log
        );
    }
}
