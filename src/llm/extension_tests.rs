use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Backend {
    endpoint: crate::core::cli_config::ResolvedEndpoint,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Backend {
    async fn new(status: &str, extra_headers: &str, body: Vec<u8>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = crate::core::cli_config::ResolvedEndpoint {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            api_key: "fixture-memory-key".into(),
            model: "fixture-chat-route".into(),
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
            body.len()
        );
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0u8; 4096];
                    let count = socket.read(&mut bytes).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..count]);
                    assert!(request.len() <= 128 * 1024);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")?
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                recorded.lock().unwrap().push(request);
                if socket.write_all(headers.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(&body).await;
                }
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }

    async fn complete(&self) -> anyhow::Result<String> {
        self.complete_with(
            &crate::core::cli_config::CliConfig {
                max_tokens: Some(9000),
                ..Default::default()
            },
            128,
        )
        .await
    }

    async fn complete_with(
        &self,
        config: &crate::core::cli_config::CliConfig,
        limit: u32,
    ) -> anyhow::Result<String> {
        tokio::time::timeout(
            Duration::from_secs(5),
            extension_completion(
                &self.endpoint,
                Some("high".into()),
                config,
                &[
                    Message::system("extract fixture facts"),
                    Message::user("bounded fixture text"),
                ],
                limit,
            ),
        )
        .await
        .expect("fixture request deadline")
    }
}

fn run(test: impl std::future::Future<Output = ()>) {
    let _tape = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct ResetTape;
    impl Drop for ResetTape {
        fn drop(&mut self) {
            crate::llm::replay::disable();
        }
    }
    let _reset = ResetTape;
    crate::llm::replay::disable();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}

fn reply(message: serde_json::Value, finish: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "choices":[{"message":message,"finish_reason":finish}],
        "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}
    }))
    .unwrap()
}

#[test]
fn extension_uses_captured_route_effort_and_no_tools_with_bounded_output() {
    run(async {
        let backend = Backend::new(
            "200 OK",
            "",
            reply(serde_json::json!({"content":"fixture result"}), "stop"),
        )
        .await;
        assert_eq!(backend.complete().await.unwrap(), "fixture result");
        let requests = backend.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let split = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&request[..split]).to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/chat/completions http/1.1\r\n"));
        assert!(headers.contains("authorization: bearer fixture-memory-key\r\n"));
        let body: serde_json::Value = serde_json::from_slice(&request[split + 4..]).unwrap();
        assert_eq!(body["model"], "fixture-chat-route");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("api_key").is_none());
    });
}

#[test]
fn extension_never_follows_redirects_or_retries_provider_failures() {
    run(async {
        for status in [
            "307 Temporary Redirect",
            "429 Too Many Requests",
            "503 Unavailable",
        ] {
            let backend = Backend::new(
                status,
                "Location: /forbidden-redirect\r\n",
                b"PRIVATE arbitrary provider body".to_vec(),
            )
            .await;
            let error = backend.complete().await.unwrap_err().to_string();
            assert!(error.contains("HTTP"), "{error}");
            assert!(!error.contains("PRIVATE"));
            assert!(!error.contains("fixture-memory-key"));
            assert!(!error.contains("127.0.0.1"));
            assert_eq!(backend.requests.lock().unwrap().len(), 1);
        }
    });
}

#[test]
fn extension_rejects_tools_legacy_functions_and_incomplete_output() {
    run(async {
        for (message, finish) in [
            (
                serde_json::json!({"content":"ignored","tool_calls":[{"id":"call-1","type":"function","function":{"name":"file_write","arguments":"{}"}}]}),
                "stop",
            ),
            (
                serde_json::json!({"content":"ignored","function_call":{"name":"file_write","arguments":"{}"}}),
                "stop",
            ),
            (serde_json::json!({"content":"truncated"}), "length"),
            (serde_json::json!({"content":"filtered"}), "content_filter"),
        ] {
            let backend = Backend::new("200 OK", "", reply(message, finish)).await;
            assert!(backend.complete().await.is_err());
            assert_eq!(backend.requests.lock().unwrap().len(), 1);
        }
    });
}

#[test]
fn extension_bounds_wire_and_completion_and_does_not_apply_reasoning() {
    run(async {
        for body in [
            vec![b'x'; 256 * 1024 + 1],
            reply(
                serde_json::json!({"content":"x".repeat(32 * 1024 + 1)}),
                "stop",
            ),
            reply(serde_json::json!({"content":"unsafe\u{001b}"}), "stop"),
            b"PRIVATE malformed reply".to_vec(),
        ] {
            let backend = Backend::new("200 OK", "", body).await;
            let error = backend.complete().await.unwrap_err().to_string();
            assert!(!error.contains("PRIVATE"));
            assert_eq!(backend.requests.lock().unwrap().len(), 1);
        }
        let backend = Backend::new(
            "200 OK",
            "",
            reply(
                serde_json::json!({"reasoning_content":"PRIVATE analysis"}),
                "stop",
            ),
        )
        .await;
        assert_eq!(backend.complete().await.unwrap(), "");
    });
}

#[test]
fn extension_invalid_token_limit_never_sends() {
    run(async {
        let backend = Backend::new(
            "200 OK",
            "",
            reply(serde_json::json!({"content":"unused"}), "stop"),
        )
        .await;
        for limit in [0, 2049, u32::MAX] {
            assert!(
                extension_completion(&backend.endpoint, None, &Default::default(), &[], limit)
                    .await
                    .is_err()
            );
        }
        assert!(backend.requests.lock().unwrap().is_empty());
    });
}

#[test]
fn extension_respects_the_tighter_captured_config_output_cap() {
    run(async {
        let backend = Backend::new(
            "200 OK",
            "",
            reply(serde_json::json!({"content":"bounded"}), "stop"),
        )
        .await;
        for (configured, job, expected) in [
            (Some(128), 2048, 128),
            (None, 256, 256),
            (Some(512), 64, 64),
        ] {
            let config = crate::core::cli_config::CliConfig {
                max_tokens: configured,
                ..Default::default()
            };
            assert_eq!(
                backend.complete_with(&config, job).await.unwrap(),
                "bounded"
            );
            let request = backend.requests.lock().unwrap().last().unwrap().clone();
            let split = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_slice(&request[split + 4..]).unwrap();
            assert_eq!(body["max_tokens"], expected);
        }
        let config = crate::core::cli_config::CliConfig {
            max_tokens: Some(0),
            ..Default::default()
        };
        assert!(backend.complete_with(&config, 128).await.is_err());
        assert_eq!(backend.requests.lock().unwrap().len(), 3);
    });
}

#[test]
fn extension_replay_and_strict_refuse_before_any_http_request() {
    run(async {
        let backend = Backend::new(
            "200 OK",
            "",
            reply(serde_json::json!({"content":"never sent"}), "stop"),
        )
        .await;
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let tape = fixture.path().join("fixture.jsonl");
        std::fs::write(&tape, b"").unwrap();
        for mode in [
            crate::llm::replay::Mode::Replay,
            crate::llm::replay::Mode::Strict,
        ] {
            crate::llm::replay::configure(mode, &tape, None).unwrap();
            let error = backend.complete().await.unwrap_err().to_string();
            assert!(error.contains("tape replay"), "{error}");
            assert_eq!(crate::llm::replay::status().unwrap().position, 0);
        }
        assert!(backend.requests.lock().unwrap().is_empty());
    });
}

#[test]
fn extension_explicit_completion_token_dialect_works_without_a_quirk_probe() {
    run(async {
        let mut backend = Backend::new(
            "200 OK",
            "",
            reply(serde_json::json!({"content":"dialect"}), "stop"),
        )
        .await;
        backend.endpoint.model = "fixture-cold-completion-dialect".into();
        assert!(!quirk_known(
            &backend.endpoint.model,
            Quirk::MaxCompletionTokens
        ));
        let config: crate::core::cli_config::CliConfig =
            serde_json::from_value(serde_json::json!({
                "output_token_parameter":"max_completion_tokens"
            }))
            .unwrap();
        assert_eq!(config.max_tokens, None);
        assert_eq!(
            backend.complete_with(&config, 128).await.unwrap(),
            "dialect"
        );
        let requests = backend.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let split = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let body: serde_json::Value = serde_json::from_slice(&request[split + 4..]).unwrap();
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_completion_tokens"], 128);
        assert!(!quirk_known(
            &backend.endpoint.model,
            Quirk::MaxCompletionTokens
        ));
    });
}
