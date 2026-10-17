//! Semantic progress of a Chat Completions stream, independent of bytes and rendering.

use crate::core::types::ChatChunk;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) struct Watchdog {
    first: Duration,
    idle: Duration,
    last_progress: Instant,
    started: bool,
    finished: Option<Instant>,
    tools: HashMap<usize, (String, String)>,
}

impl Watchdog {
    pub(crate) fn new(first: Duration, idle: Duration) -> Self {
        Self {
            first,
            idle,
            last_progress: Instant::now(),
            started: false,
            finished: None,
            tools: HashMap::new(),
        }
    }

    pub(crate) fn remaining(&self) -> Duration {
        let now = Instant::now();
        let deadline = if let Some(finished) = self.finished {
            // Consume usage trailers and late errors, but never wait another full response window.
            finished + self.idle.min(Duration::from_secs(5))
        } else {
            self.last_progress + if self.started { self.idle } else { self.first }
        };
        deadline.saturating_duration_since(now)
    }

    pub(crate) async fn wait<T>(&self, next: impl std::future::Future<Output = T>) -> Result<T, ()> {
        // A ready ping must not win against an already-expired deadline.
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(());
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep(remaining) => Err(()),
            result = next => Ok(result),
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished.is_some()
    }

    pub(crate) fn stage(&self) -> &'static str {
        if self.finished.is_some() {
            "completion trailer"
        } else if self.started {
            "useful stream progress"
        } else {
            "first useful output"
        }
    }

    pub(crate) fn observe(&mut self, chunk: &ChatChunk) -> bool {
        let Some(choice) = chunk.choices.first() else {
            return false;
        };
        if choice.finish_reason.is_some() && self.finished.is_none() {
            self.finished = Some(Instant::now());
        }
        let delta = &choice.delta;
        let mut useful = [
            delta.content.as_deref(),
            delta.reasoning_content.as_deref(),
            delta.reasoning.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| !text.is_empty());
        for tool in &delta.tool_calls {
            let seen = self.tools.entry(tool.index).or_default();
            if let Some(id) = tool.id.as_ref().filter(|s| !s.is_empty()) {
                useful |= seen.0 != *id;
                seen.0.clone_from(id);
            }
            if let Some(function) = &tool.function {
                if let Some(name) = function.name.as_ref().filter(|s| !s.is_empty()) {
                    useful |= seen.1 != *name;
                    seen.1.clone_from(name);
                }
                useful |= function.arguments.as_ref().is_some_and(|s| !s.is_empty());
            }
        }
        if useful {
            self.started = true;
            self.last_progress = Instant::now();
        }
        useful
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(json: &str) -> ChatChunk {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn metadata_cannot_renew_useful_output_deadline() {
        let mut watch = Watchdog::new(Duration::from_secs(60), Duration::from_secs(10));
        let start = watch.last_progress;
        for raw in [
            r#"{"type":"ping"}"#,
            r#"{}"#,
            r#"{"choices":[]}"#,
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"content":""}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":1}}"#,
        ] {
            for _ in 0..3 {
                assert!(!watch.observe(&chunk(raw)), "{raw}");
                assert_eq!(watch.last_progress, start);
                assert!(!watch.started);
            }
        }
    }

    #[test]
    fn reasoning_and_advancing_tools_are_progress_but_repeated_headers_are_not() {
        let mut watch = Watchdog::new(Duration::from_secs(60), Duration::from_secs(10));
        assert!(watch.observe(&chunk(r#"{"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#)));
        let header = chunk(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"file_read","arguments":""}}]}}]}"#);
        assert!(watch.observe(&header));
        assert!(!watch.observe(&header));
        assert!(watch.observe(&chunk(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#)));
    }

    #[tokio::test]
    async fn a_ping_only_stream_expires_instead_of_renewing_forever() {
        let mut watch = Watchdog::new(Duration::from_millis(30), Duration::from_millis(10));
        let started = tokio::time::Instant::now();
        loop {
            // A provider that answers 200 and then pings forever must lose to the deadline even
            // though every read is immediately ready.
            match watch.wait(async { Some(chunk(r#"{"type":"ping"}"#)) }).await {
                Ok(Some(chunk)) => {
                    assert!(!watch.observe(&chunk));
                    assert!(started.elapsed() < Duration::from_secs(5));
                }
                Err(()) => break,
                Ok(None) => panic!("fixture never ends"),
            }
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!watch.finished());
    }

    #[tokio::test]
    async fn useful_progress_extends_the_idle_window_but_metadata_does_not() {
        let mut watch = Watchdog::new(Duration::from_millis(20), Duration::from_millis(40));
        assert!(watch.observe(&chunk(r#"{"choices":[{"delta":{"content":"a"}}]}"#)));
        let renewed = watch.remaining();
        assert!(renewed > Duration::from_millis(30), "{renewed:?}");
        assert!(!watch.observe(&chunk(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#)));
        assert!(watch.remaining() <= renewed);
    }

    #[test]
    fn finish_tail_deadline_does_not_renew_on_metadata_or_more_content() {
        let mut watch = Watchdog::new(Duration::from_secs(60), Duration::from_secs(90));
        watch.observe(&chunk(r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#));
        let finished = watch.finished;
        watch.observe(&chunk(r#"{"choices":[{"delta":{"content":"late"},"finish_reason":"stop"}]}"#));
        assert_eq!(watch.finished, finished);
        assert!(watch.remaining() <= Duration::from_secs(5));
        assert_eq!(watch.stage(), "completion trailer");
    }
}
