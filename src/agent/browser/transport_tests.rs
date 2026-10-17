//! Hermetic loopback HTTP/WS fixtures only; no browser is launched or contacted.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use transport::{Budget, MAX_DISCOVERY_BYTES};

fn profile(endpoint: String) -> config::BrowserProfile {
    config::BrowserProfile {
        provider: "cdp".into(),
        endpoint,
        auth_env: None,
        target_id: None,
        ws_origins: Vec::new(),
    }
}
async fn http_fixture(
    status: &str,
    headers: &str,
    body: Vec<u8>,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",
        body.len()
    );
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 8192];
        let n = stream.read(&mut bytes).await.unwrap();
        stream.write_all(response.as_bytes()).await.unwrap();
        let _ = stream.write_all(&body).await;
        String::from_utf8_lossy(&bytes[..n]).into_owned()
    });
    (format!("http://{address}"), task)
}
fn page(id: &str, ws: &str) -> Value {
    json!({"type":"page","id":id,"webSocketDebuggerUrl":ws})
}

#[test]
fn target_selection_refuses_ambiguity_nonpages_and_oversize() {
    let pages = json!([
        page("a", "ws://127.0.0.1:9222/a"),
        page("b", "ws://127.0.0.1:9222/b")
    ]);
    assert!(transport::select_target(pages.clone(), None)
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    assert_eq!(transport::select_target(pages, Some("b")).unwrap().id, "b");
    assert!(transport::select_target(json!([{"type":"browser","id":"root"}]), None).is_err());
    assert!(
        transport::select_target(json!(vec![page("a", "ws://127.0.0.1:9222/a"); 65]), None)
            .is_err()
    );
}
#[test]
fn origin_pin_checks_host_port_scheme_userinfo_and_explicit_origins() {
    let mut p = profile("https://cdp.example:9443".into());
    assert!(transport::pin_ws_origin(&p, "wss://cdp.example:9443/target").is_ok());
    for raw in [
        "ws://cdp.example:9443/target",
        "wss://other.example:9443/target",
        "wss://cdp.example:9444/target",
        "wss://user:password@cdp.example:9443/target",
        "http://cdp.example:9443/target",
        "wss://cdp.example:9443/target?token=secret",
    ] {
        assert!(transport::pin_ws_origin(&p, raw).is_err(), "{raw}");
    }
    p.ws_origins.push("wss://other.example:9443".into());
    assert!(transport::pin_ws_origin(&p, "wss://other.example:9443/target").is_ok());
    assert!(transport::pin_ws_origin(&p, "ws://other.example:9443/target").is_err());
    assert!(config::endpoint_url("file:///tmp/a").is_err());
    assert!(navigate_url("javascript:alert(1)").is_err());
    assert!(navigate_url("https://u:p@example.com").is_err());
}
#[tokio::test]
async fn discovery_refuses_redirect_before_another_request() {
    let (endpoint, task) = http_fixture(
        "302 Found",
        "Location: http://127.0.0.1:1/stolen\r\n",
        vec![],
    )
    .await;
    let e = transport::discover(
        "mock",
        &profile(endpoint),
        &Budget::new(Duration::from_secs(1)),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("redirect"));
    assert!(task.await.unwrap().starts_with("GET /json "));
}
#[tokio::test]
async fn discovery_body_is_bounded_and_ws_origin_rejected() {
    let (endpoint, task) = http_fixture("200 OK", "", vec![b' '; MAX_DISCOVERY_BYTES + 1]).await;
    assert!(transport::discover(
        "mock",
        &profile(endpoint),
        &Budget::new(Duration::from_secs(1))
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("body exceeds"));
    task.await.unwrap();
    let (endpoint, task) = http_fixture(
        "200 OK",
        "",
        json!([page("a", "ws://127.0.0.1:1/a")])
            .to_string()
            .into_bytes(),
    )
    .await;
    assert!(transport::discover(
        "mock",
        &profile(endpoint),
        &Budget::new(Duration::from_secs(1))
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("cross-origin"));
    task.await.unwrap();
}
#[tokio::test]
async fn one_deadline_covers_slow_body_ws_connect_and_lock_wait() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 8192];
        let _ = socket.read(&mut bytes).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n[")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let start = tokio::time::Instant::now();
    let e = transport::discover(
        "mock",
        &profile(format!("http://{address}")),
        &Budget::new(Duration::from_millis(40)),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("total"));
    assert!(start.elapsed() < Duration::from_secs(1));
    server.abort();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let p = profile(format!("http://{address}"));
    assert!(transport::ws_connect(
        "mock",
        &p,
        &format!("ws://{address}/a"),
        &Budget::new(Duration::from_millis(40))
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("total"));
    server.abort();
    let lock_path = std::env::temp_dir().join(format!(
        "aizen-browser-mock-lock-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    ));
    let lock =
        crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&lock_path, Duration::ZERO).unwrap();
    let e = Budget::new(Duration::from_millis(40))
        .run(async {
            loop {
                if let Ok(l) = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
                    &lock_path,
                    Duration::ZERO,
                ) {
                    break Ok(l);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_err();
    assert!(e.to_string().contains("total"));
    drop(lock);
    let _ = std::fs::remove_file(lock_path);
}
#[tokio::test]
async fn cancellation_aborts_wait_immediately_and_reports_retained_state() {
    let token = crate::core::cancel::TurnCancel::new();
    let budget = crate::core::cancel::with_current(token.clone(), || Budget::new(OP_TIMEOUT));
    token.cancel();
    let e = budget
        .run(std::future::pending::<Result<()>>())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("cancelled"));
    assert!(e.to_string().contains("viewport"));
}

async fn mock_cdp(oversize: bool) -> (CdpClient, tokio::task::JoinHandle<()>, std::path::PathBuf) {
    mock_cdp_source(oversize, None).await
}
async fn mock_cdp_source(
    oversize: bool,
    served: Option<(String, String)>,
) -> (CdpClient, tokio::task::JoinHandle<()>, std::path::PathBuf) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
            let command: Value = serde_json::from_str(&text).unwrap();
            let id = command["id"].clone();
            if oversize {
                let _ = ws
                    .send(WsMessage::Text("x".repeat(transport::MAX_WS_BYTES + 1)))
                    .await;
                break;
            }
            let result = match command["method"].as_str().unwrap() {
                "Runtime.evaluate" => {
                    json!({"result":{"value":{"url":"http://app.test/","title":"mock","viewport":{"width":2,"height":2,"device_pixel_ratio":1,"page_x":0,"page_y":0,"scale":1},"document":"complete","fonts":"loaded","images":"ready","layout_signature":"stable","observed_build_marker":null}}})
                }
                "Page.captureScreenshot" => {
                    assert_eq!(command["params"]["format"], "png");
                    assert_eq!(command["params"]["captureBeyondViewport"], false);
                    let bytes = crate::core::tool_media::encode_rgba_png(&[255; 16], 2, 2).unwrap();
                    json!({"data":crate::core::tool_media::base64_encode(&bytes)})
                }
                "Page.getFrameTree" => {
                    json!({"frameTree":{"frame":{"id":"f","url":served.as_ref().unwrap().0}}})
                }
                "Page.getResourceContent" => {
                    json!({"content":served.as_ref().unwrap().1,"base64Encoded":false})
                }
                _ => json!({}),
            };
            if ws
                .send(WsMessage::Text(
                    json!({"id":id,"result":result}).to_string(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let p = profile(format!("http://{address}"));
    let budget = Budget::new(Duration::from_secs(2));
    let ws = transport::ws_connect("mock", &p, &format!("ws://{address}/a"), &budget)
        .await
        .unwrap();
    let path = std::env::temp_dir().join(format!(
        "aizen-browser-cdp-mock-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    ));
    let client = CdpClient {
        ws,
        next_id: 1,
        _ownership: crate::core::repo_lock::RepoTxnLock::acquire_exclusive(&path, Duration::ZERO)
            .unwrap(),
        profile: "mock".into(),
        endpoint: p.endpoint.clone(),
        selection: p,
        target_id: "a".into(),
        budget,
        viewport: None,
        refs: HashMap::new(),
    };
    (client, server, path)
}
#[tokio::test]
async fn cdp_screenshot_png_and_readiness_are_mocked() {
    let (mut c, server, path) = mock_cdp(false).await;
    let readiness = capture::settle(&mut c).await.unwrap();
    assert_eq!(readiness["ready"], true);
    let result = c
        .call(
            "Page.captureScreenshot",
            json!({"format":"png","captureBeyondViewport":false}),
        )
        .await
        .unwrap();
    let image = crate::core::tool_media::ToolImage::from_base64(
        result["data"].as_str().unwrap(),
        "image/png",
        crate::core::tool_media::ImageProvenance::new("mock", None),
    )
    .unwrap();
    assert_eq!(image.metadata().width, Some(2));
    assert_eq!(image.metadata().height, Some(2));
    drop(c);
    server.abort();
    let _ = std::fs::remove_file(path);
}
#[tokio::test]
async fn static_resource_matches_local_bytes_and_rejects_changes() {
    let root = std::env::temp_dir().join(format!(
        "aizen-browser-static-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("index.html");
    let html = "<!doctype html><html><body><h1>Static</h1></body></html>";
    std::fs::write(&path, html).unwrap();
    let url = url::Url::from_file_path(&path).unwrap().to_string();
    let (mut c, server, lock) = mock_cdp_source(false, Some((url.clone(), html.into()))).await;
    let source = static_source::local_source(&mut c, root.to_str().unwrap(), &url)
        .await
        .unwrap();
    assert_eq!(
        source.hash,
        crate::core::tool_media::sha256(html.as_bytes())
    );
    static_source::confirm_served(&mut c, &source)
        .await
        .unwrap();
    std::fs::write(&path, "<h1>Changed</h1>").unwrap();
    assert!(static_source::confirm_served(&mut c, &source)
        .await
        .unwrap_err()
        .to_string()
        .contains("changed"));
    let outside = root.join("nested");
    std::fs::create_dir_all(&outside).unwrap();
    assert!(
        static_source::local_source(&mut c, outside.to_str().unwrap(), &url)
            .await
            .unwrap_err()
            .to_string()
            .contains("outside")
    );
    drop(c);
    server.abort();
    let _ = std::fs::remove_file(lock);
    let _ = std::fs::remove_dir_all(root);
}
#[tokio::test]
async fn cdp_oversized_frame_is_refused() {
    let (mut c, server, path) = mock_cdp(true).await;
    assert!(c
        .call("Runtime.evaluate", json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("oversized"));
    drop(c);
    server.abort();
    let _ = std::fs::remove_file(path);
}
#[tokio::test]
#[expect(
    clippy::result_large_err,
    reason = "accept_hdr_async requires a callback returning tungstenite's unboxed HTTP ErrorResponse; this test only accepts the handshake"
)]
async fn websocket_auth_is_forwarded_only_after_origin_pin() {
    // Unique variable avoids global environment interference and never edits live config.
    let var = format!(
        "AIZEN_BROWSER_MOCK_AUTH_{}",
        crate::core::persist::unique_sequence()
    );
    std::env::set_var(&var, "Bearer fixture-only");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _ = tokio_tungstenite::accept_hdr_async(
            socket,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                assert_eq!(
                    request.headers().get("authorization").unwrap(),
                    "Bearer fixture-only"
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
    });
    let mut p = profile(format!("http://{address}"));
    p.auth_env = Some(var.clone());
    assert!(transport::ws_connect(
        "mock",
        &p,
        "ws://127.0.0.1:1/stolen",
        &Budget::new(Duration::from_secs(1))
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("cross-origin"));
    let ws = transport::ws_connect(
        "mock",
        &p,
        &format!("ws://{address}/a"),
        &Budget::new(Duration::from_secs(1)),
    )
    .await
    .unwrap();
    drop(ws);
    server.await.unwrap();
    std::env::remove_var(var);
}
