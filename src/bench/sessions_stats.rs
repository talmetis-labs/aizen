//! `aizen bench sessions` — the turn-shape statistics behind the fast-lean plan's §1.2, computed
//! from this machine's saved conversations: tool calls per user turn, tool-result sizes and how
//! many sit exactly at the 4,096-char cut, identical calls repeated inside one turn, the call
//! mix, and the read : edit ratio. Run before a release and after a loop change; the numbers
//! that moved are the ones to explain. Reads only.

use crate::core::session_store::{read_session_path, stat_sessions};
use crate::core::types::Message;
use anyhow::Result;
use std::collections::HashMap;

/// The length a tool result is cut to before it reaches the model (`max_tool_result_chars`), and
/// the marker `truncate_result` leaves in a cut result — the count of cut results is the count
/// carrying the marker, not of results measuring exactly the cap.
const RESULT_CUT_CHARS: usize = 4_096;
const RESULT_CUT_MARKER: &str = "chars truncated]";

#[derive(Debug, Default, serde::Serialize)]
pub struct SessionStats {
    pub sessions: usize,
    pub user_turns: usize,
    pub tool_calls: usize,
    pub calls_per_turn_mean: f64,
    pub calls_per_turn_median: usize,
    pub calls_per_turn_p90: usize,
    pub calls_per_turn_max: usize,
    pub result_chars_median: usize,
    pub result_chars_p90: usize,
    pub result_chars_max: usize,
    pub results_at_cut: usize,
    pub assistant_chars_median: usize,
    pub assistant_chars_p90: usize,
    pub repeated_calls: usize,
    /// Tool name → calls, most-used first.
    pub call_mix: Vec<(String, usize)>,
    pub reads: usize,
    pub edits: usize,
}

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// PURE. The statistics over a set of conversations.
pub fn compute(conversations: &[Vec<Message>]) -> SessionStats {
    let mut per_turn: Vec<usize> = Vec::new();
    let mut result_sizes: Vec<usize> = Vec::new();
    let mut assistant_sizes: Vec<usize> = Vec::new();
    let mut mix: HashMap<String, usize> = HashMap::new();
    let mut st = SessionStats {
        sessions: conversations.len(),
        ..Default::default()
    };
    for conv in conversations {
        let mut calls_this_turn = 0usize;
        let mut seen_this_turn: HashMap<(String, String), usize> = HashMap::new();
        let mut in_turn = false;
        for m in conv {
            match m.role.as_str() {
                "user" => {
                    if in_turn {
                        per_turn.push(calls_this_turn);
                    }
                    in_turn = true;
                    calls_this_turn = 0;
                    seen_this_turn.clear();
                    st.user_turns += 1;
                }
                "assistant" => {
                    if let Some(c) = m.content.as_deref() {
                        if !c.trim().is_empty() {
                            assistant_sizes.push(c.chars().count());
                        }
                    }
                    for call in &m.tool_calls {
                        st.tool_calls += 1;
                        calls_this_turn += 1;
                        let name = call.function.name.clone();
                        *mix.entry(name.clone()).or_insert(0) += 1;
                        let key = (name, call.function.arguments.clone());
                        let n = seen_this_turn.entry(key).or_insert(0);
                        if *n > 0 {
                            st.repeated_calls += 1;
                        }
                        *n += 1;
                    }
                }
                "tool" => {
                    let c = m.content.as_deref().unwrap_or("");
                    result_sizes.push(c.chars().count());
                    if c.contains(RESULT_CUT_MARKER) {
                        st.results_at_cut += 1;
                    }
                }
                _ => {}
            }
        }
        if in_turn {
            per_turn.push(calls_this_turn);
        }
    }
    per_turn.sort_unstable();
    result_sizes.sort_unstable();
    assistant_sizes.sort_unstable();
    st.calls_per_turn_mean = if per_turn.is_empty() {
        0.0
    } else {
        per_turn.iter().sum::<usize>() as f64 / per_turn.len() as f64
    };
    st.calls_per_turn_median = percentile(&per_turn, 0.5);
    st.calls_per_turn_p90 = percentile(&per_turn, 0.9);
    st.calls_per_turn_max = per_turn.last().copied().unwrap_or(0);
    st.result_chars_median = percentile(&result_sizes, 0.5);
    st.result_chars_p90 = percentile(&result_sizes, 0.9);
    st.result_chars_max = result_sizes.last().copied().unwrap_or(0);
    st.assistant_chars_median = percentile(&assistant_sizes, 0.5);
    st.assistant_chars_p90 = percentile(&assistant_sizes, 0.9);
    let mut mix: Vec<(String, usize)> = mix.into_iter().collect();
    mix.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (name, n) in &mix {
        match name.as_str() {
            "file_read" | "search_files" | "file_glob" | "read_symbol" | "codebase_search" => {
                st.reads += n
            }
            "file_write" | "file_edit" | "multi_edit" | "symbol_replace" | "symbol_insert"
            | "file_move" => st.edits += n,
            _ => {}
        }
    }
    st.call_mix = mix;
    st
}

/// Entry point for `aizen bench sessions`.
pub fn run(json: bool) -> Result<()> {
    let conversations: Vec<Vec<Message>> = stat_sessions()
        .iter()
        .filter_map(|s| read_session_path(&s.path))
        .map(|(msgs, _)| msgs)
        .collect();
    let st = compute(&conversations);
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(());
    }
    println!(
        "sessions bench: {} conversation(s) · {} user turn(s) · {} tool call(s)",
        st.sessions, st.user_turns, st.tool_calls
    );
    println!(
        "  tool calls per user turn    mean {:.1} · median {} · p90 {} · max {}",
        st.calls_per_turn_mean,
        st.calls_per_turn_median,
        st.calls_per_turn_p90,
        st.calls_per_turn_max
    );
    println!(
        "  tool result size (chars)    median {} · p90 {} · max {} · exactly at the {} cut: {}",
        st.result_chars_median,
        st.result_chars_p90,
        st.result_chars_max,
        RESULT_CUT_CHARS,
        st.results_at_cut
    );
    println!(
        "  assistant text (chars)      median {} · p90 {}",
        st.assistant_chars_median, st.assistant_chars_p90
    );
    let repeat_rate = if st.tool_calls == 0 {
        0.0
    } else {
        st.repeated_calls as f64 * 100.0 / st.tool_calls as f64
    };
    println!(
        "  identical call repeated within one turn: {} ({repeat_rate:.1} % of calls)",
        st.repeated_calls
    );
    let top: Vec<String> = st
        .call_mix
        .iter()
        .take(8)
        .map(|(n, c)| format!("{n} {c}"))
        .collect();
    println!("  call mix: {}", top.join(" · "));
    let ratio = if st.edits == 0 {
        "n/a".to_string()
    } else {
        format!("{:.2}", st.reads as f64 / st.edits as f64)
    };
    println!(
        "  read : edit ratio           {ratio} ({} reads, {} edits)",
        st.reads, st.edits
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionCall, ToolCall};

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "c".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    fn cut_tool() -> Message {
        let mut m = tool(RESULT_CUT_CHARS);
        m.content = Some(format!(
            "head
…[12000 {RESULT_CUT_MARKER}…
tail{}",
            "x".repeat(RESULT_CUT_CHARS - 40)
        ));
        m
    }

    fn tool(len: usize) -> Message {
        Message {
            role: "tool".to_string(),
            content: Some("x".repeat(len)),
            tool_calls: Vec::new(),
            tool_call_id: Some("c".to_string()),
            images: Vec::new(),
            cache_control: None,
        }
    }

    #[test]
    fn the_turn_statistics_count_what_the_plan_measures() {
        let conv = vec![
            Message::system("lane"),
            Message::user("first"),
            Message::assistant_tool_calls(vec![
                call("file_read", "{\"a\":1}"),
                call("file_read", "{\"a\":1}"),
            ]),
            cut_tool(),
            tool(10),
            Message::assistant_tool_calls(vec![call("file_edit", "{}")]),
            tool(20),
            Message::assistant("done here"),
            Message::user("second"),
            Message::assistant("no tools this time"),
        ];
        let st = compute(&[conv]);
        assert_eq!((st.sessions, st.user_turns, st.tool_calls), (1, 2, 3));
        assert_eq!(st.calls_per_turn_max, 3);
        assert_eq!(
            st.calls_per_turn_median, 3,
            "two turns: 0 and 3 → the midpoint index rounds half away from zero, so the upper"
        );
        assert_eq!(
            st.repeated_calls, 1,
            "the identical file_read within one turn"
        );
        assert_eq!(
            st.results_at_cut, 1,
            "the one result carrying the truncation marker"
        );
        assert!(st.result_chars_max >= RESULT_CUT_CHARS - 40);
        assert_eq!(st.call_mix[0], ("file_read".to_string(), 2));
        assert_eq!((st.reads, st.edits), (2, 1));
        assert_eq!(
            st.assistant_chars_median, 18,
            "'done here' (9) and 'no tools this time' (18): the midpoint index rounds up"
        );
        assert_eq!(compute(&[]).sessions, 0);
    }
}
