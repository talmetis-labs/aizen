//! Offline visual-gate integration: generated PNGs and scripted responses are fixtures, not
//! screenshots of a browser or a claim about real aesthetics/provider delivery.

use super::{
    DesignBriefTool, DesignContext, DesignMode, DesignReviewTool, EvidenceStatus, GateDecision,
    Viewport,
};
use crate::agent::tools::{Tool, ToolObservation, ToolRegistry, WorkspaceEffect};
use crate::agent::{
    mark_projected_evidence, run_agent_loop, AgentConfig, AgentOutcome, StopReason,
};
use crate::core::config::EnvGuard;
use crate::core::exec_ctx::ExecutionContext;
use crate::core::tool_media::{encode_rgba_png, sha256, ImageProvenance, ToolImage};
use crate::core::types::{FunctionCall, Message, ToolCall, ToolDef};
use crate::core::workspace_txn::{stable_key, WorkspaceIdentity, WorkspaceWriterLease};
use crate::llm::client::ChatTurn;
use crate::llm::media::{destination_key, project_messages_for_destination, VisionPolicy};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[test]
fn offline_reference_contract_requires_observation_capture_compare_then_repair() {
    use super::reference_compare::{compare_encoded, ComparisonBinding, ComparisonRequest};
    use super::{CaptureIdentity, DesignBrief, FidelityMode, ReferenceObservationKind};
    let _lock = crate::llm::image_send::image_test_lock();
    let original = encode_rgba_png(&[10, 20, 30, 255], 1, 1).unwrap();
    let mismatch = encode_rgba_png(&[10, 20, 31, 255], 1, 1).unwrap();
    let parent = DesignContext::new("fixture-owner", "fixture-workspace", DesignMode::Auto);
    parent.begin_turn("parent", "fixture-workspace");
    parent
        .apply_brief(
            DesignBrief {
                intent: super::DesignIntent::Preserve,
                brief: "Match fixture original".into(),
                acceptance: "Exact reference and functioning semantic UI".into(),
                required_viewports: Some(vec![Viewport {
                    width: 1,
                    height: 1,
                }]),
                ..Default::default()
            },
            false,
        )
        .unwrap();
    let child = parent.fork_child("child", "fixture-workspace");
    child
        .activate_selected_references(
            vec![("selected".into(), sha256(&original))],
            FidelityMode::Exact,
        )
        .unwrap();
    assert!(child.gate().blocks_completion());
    // Fixture-owned receipt observation represents successful inclusion, not model perception.
    child
        .record_reference_observation(
            "selected",
            &sha256(&original),
            ReferenceObservationKind::Original,
        )
        .unwrap();
    for (id, bytes, source) in [
        ("bad", mismatch.as_slice(), "fixture-v1"),
        ("repaired", original.as_slice(), "fixture-v2"),
    ] {
        child.set_target("fixture-local").unwrap();
        child.set_source_revision(source).unwrap();
        let revision = child.current_revision();
        child
            .record_capture(
                id,
                Some(source),
                "fixture-local",
                Viewport {
                    width: 1,
                    height: 1,
                },
                revision,
                true,
                "fixture readiness",
            )
            .unwrap();
        child.mark_projected(id).unwrap();
        child
            .record_capture_identity(
                id,
                CaptureIdentity {
                    output_sha256: sha256(bytes),
                    dpr_milli: 1000,
                    font_identity: "owned-fixture-font".into(),
                    frame_state: format!("owned-{source}"),
                },
            )
            .unwrap();
        let result = compare_encoded(
            &original,
            "image/png",
            bytes,
            "image/png",
            ComparisonBinding {
                reference_id: "selected".into(),
                reference_sha256: sha256(&original),
                output_evidence_id: id.into(),
                output_sha256: sha256(bytes),
                source_hash: source.into(),
                render_revision: revision,
                target: "fixture-local".into(),
                viewport: Viewport {
                    width: 1,
                    height: 1,
                },
                dpr_milli: 1000,
                font_identity: "owned-fixture-font".into(),
                frame_state: format!("owned-{source}"),
                request: ComparisonRequest {
                    mode: FidelityMode::Exact,
                    ..Default::default()
                },
            },
        )
        .unwrap();
        child.record_comparison(result.receipt).unwrap();
        child
            .record_review(
                id,
                true,
                "fixture model boolean cannot override measurement",
            )
            .unwrap();
        if id == "bad" {
            assert!(child.gate().blocks_completion());
            child.begin_repair().unwrap();
        } else {
            assert_eq!(child.gate(), GateDecision::Verified);
        }
    }
    assert_eq!(child.snapshot().repairs, 1);
    assert!(parent.snapshot().evidence.is_empty());
    assert!(parent.snapshot().reference_observations.is_empty());
}

/// Real loopback wire receipts connect controller grants, child isolation and comparison gates.
/// Generated captures/runtime identity are owned simulations, not browser/aesthetic verification.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // Serialize codec admission and the public wrapper's tape state.
async fn current_reference_child_wire_receipt_requires_exact_scoped_repair() {
    use crate::agent::task_tool::{
        child_execution_context, child_reference_message, seat_child_references,
    };
    use crate::core::cli_config::{self, CliConfig, ResolvedEndpoint};
    use crate::llm::{client, media};
    use crate::repl::turn::{seat_loaded_reference_images, ResolvedTurn};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const OK: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"fixture accepted\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    const ERROR: &str =
        "data: {\"error\":{\"code\":\"FIXTURE_FAILURE\",\"message\":\"owned provider error\"}}\n\n";
    const KEY: &str = "owned-fixture-account";
    const MODEL: &str = "owned-reference-model";
    let _tape = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _images = crate::llm::image_send::image_test_lock();
    crate::llm::replay::disable(); // Explicitly turn off tape playback under its test lock.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let mut requests = Vec::new();
            for response in [ERROR, OK, OK, OK] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "fixture request ended before its body");
                    bytes.extend_from_slice(&chunk[..n]);
                    assert!(bytes.len() <= 64 * 1024, "bounded 1-pixel fixture request");
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        assert!(headers.starts_with("post /chat/completions "));
                        let length = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|s| s.trim().parse::<usize>().ok()).unwrap();
                        if bytes.len() >= end + 4 + length {
                            requests.push(serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length]).unwrap());
                            break;
                        }
                    }
                }
                let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
                socket.write_all(wire.as_bytes()).await.unwrap();
            }
            requests
        }).await.expect("bounded reference fixture server")
    });
    let original = encode_rgba_png(&[10, 20, 30, 255], 1, 1).unwrap();
    let mismatch = encode_rgba_png(&[10, 20, 31, 255], 1, 1).unwrap();
    let image = ToolImage::from_bytes(
        &original,
        "image/png",
        ImageProvenance::new("fixture", None),
    )
    .unwrap();
    let original_url = image.data_url();
    let viewport = Viewport {
        width: 1,
        height: 1,
    };
    let parent_design =
        DesignContext::new("owned-wire-owner", "owned-wire-workspace", DesignMode::Auto);
    parent_design.begin_turn("parent", "owned-wire-workspace");
    let brief = super::DesignBrief {
        intent: super::DesignIntent::Preserve,
        brief: "Match the explicitly chosen owned fixture".into(),
        acceptance: "Exact generated pixels; functionality unmeasured".into(),
        required_viewports: Some(vec![viewport]),
        ..Default::default()
    };
    parent_design.apply_brief(brief, false).unwrap();
    let mut source_cfg = CliConfig::default();
    cli_config::set_vision_support_in(&mut source_cfg, &base, MODEL, Some(true)).unwrap();
    let endpoint = ResolvedEndpoint {
        base_url: base.clone(),
        api_key: KEY.into(),
        model: MODEL.into(),
    };
    let mut resolved = ResolvedTurn::from_frozen(
        endpoint,
        None,
        None,
        parent_design.clone(),
        source_cfg.clone(),
    );
    seat_loaded_reference_images(&mut resolved, std::slice::from_ref(&original_url)).unwrap();
    let parent = resolved.execution_context();
    let ids = parent.references_checked().unwrap().unwrap().ids().to_vec();
    let parent_before = serde_json::to_value(parent_design.snapshot()).unwrap();
    let fresh = |name: &str| {
        child_execution_context(
            &parent,
            name,
            std::path::Path::new("owned-wire-workspace"),
            name,
        )
    };
    for (name, grant, policy) in [
        ("unknown", parent.clone(), VisionPolicy::Unknown),
        (
            "withheld",
            parent.without_references(),
            VisionPolicy::Supported,
        ),
    ] {
        let blocked = fresh(name);
        assert!(
            seat_child_references(&grant, blocked.clone(), &ids, &base, MODEL, KEY, policy)
                .is_err()
        );
        let state = blocked.design().unwrap().snapshot();
        assert!(state.references.is_empty() && state.reference_observations.is_empty());
    }
    assert!(parent_design.gate().blocks_completion());
    let child = seat_child_references(
        &parent,
        fresh("selected"),
        &ids,
        &base,
        MODEL,
        KEY,
        media::vision_policy_in(&resolved.vision_config, &base, MODEL),
    )
    .unwrap();
    let design = child.design().unwrap();
    assert!(design.gate().blocks_completion());
    assert!(!child.trace_visible());
    let messages = vec![
        Message::system("owned child fixture"),
        child_reference_message(&child, "Inspect selected original").unwrap(),
    ];
    assert_eq!(messages[1].images, vec![original_url.clone()]);
    assert_eq!(messages[1].tool_images.len(), 1);
    let marker = &messages[1].tool_images[0].metadata().provenance;
    assert_eq!(marker.evidence_id.as_deref(), Some(ids[0].as_str()));
    let frozen = resolved.vision_config.clone();
    cli_config::set_vision_support_in(&mut source_cfg, &base, MODEL, Some(false)).unwrap();
    assert_eq!(
        media::vision_policy_in(&frozen, &base, MODEL),
        VisionPolicy::Supported
    );
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let send = |outgoing: Vec<Message>| {
        let (http, child, base, frozen) = (&http, &child, &base, &frozen);
        async move {
            let call = client::stream_chat_with_tools_sink_frozen(
                http,
                base,
                KEY,
                MODEL,
                &outgoing,
                &[],
                client::StreamSink::default(),
                None,
                frozen,
            );
            let delivery = media::with_image_delivery(child, base, KEY, MODEL, call);
            tokio::time::timeout(Duration::from_secs(3), delivery)
                .await
                .expect("bounded reference fixture request")
        }
    };
    assert!(
        send(messages.clone()).await.is_err(),
        "provider error cannot return a delivery receipt"
    );
    assert!(design.snapshot().reference_observations.is_empty());
    assert!(design.gate().blocks_completion());
    let (_, receipt) = send(messages.clone()).await.unwrap();
    assert_eq!(receipt.user_images.len(), 1);
    let sent = &receipt.user_images[0];
    assert_eq!((sent.message_index, sent.image_index), (1, 0));
    assert!(sent.original);
    assert_eq!(sent.wire_sha256, sha256(original_url.as_bytes()));
    crate::agent::mark_image_delivery(&messages, &receipt, &design);
    assert_eq!(design.snapshot().reference_observations.len(), 1);
    assert_eq!(
        design.snapshot().reference_observations[0].kind,
        super::ReferenceObservationKind::Original
    );
    assert!(
        design.snapshot().evidence.is_empty(),
        "reference is not implementation evidence"
    );
    for (id, bytes, source) in [
        ("bad", mismatch.as_slice(), "owned-v1"),
        ("repaired", original.as_slice(), "owned-v2"),
    ] {
        design.set_target("owned-generated-output").unwrap();
        design.set_source_revision(source).unwrap();
        let mut provenance = ImageProvenance::new("browser_screenshot", None);
        provenance.evidence_id = Some(id.into());
        provenance.design_scope = Some(design.evidence_scope());
        provenance.approved_destination = Some(media::effective_destination_key(&base, MODEL));
        let output = ToolImage::from_bytes(bytes, "image/png", provenance).unwrap();
        design
            .record_capture(
                id,
                Some(source),
                "owned-generated-output",
                viewport,
                design.current_revision(),
                true,
                "Simulated owned PNG; no browser/render quality claim",
            )
            .unwrap();
        let identity = super::CaptureIdentity {
            output_sha256: sha256(bytes),
            dpr_milli: 1000,
            font_identity: "owned-simulated-face".into(),
            frame_state: "owned-simulated-repeat".into(),
        };
        design.record_capture_identity(id, identity).unwrap();
        let mut outgoing = messages.clone();
        let call = tool_call(id, "browser_screenshot", json!({}));
        outgoing.push(Message::assistant_tool_calls(vec![call]));
        outgoing.push(Message::tool_observation(
            id,
            "Owned generated output",
            vec![output],
        ));
        child.refresh_tool_images(&outgoing);
        let (_, receipt) = send(outgoing.clone()).await.unwrap();
        assert_eq!(receipt.tool_images.len(), 1);
        assert_eq!(receipt.tool_images[0].message_index, 3);
        crate::agent::mark_image_delivery(&outgoing, &receipt, &design);
        let result = crate::core::exec_ctx::with_current(child.clone(), || {
            let compared = super::tools::execute_compare_with(&design,
                &json!({"reference_id":ids[0], "evidence_id":id}), |_ctx, evidence_id| {
                    child.scoped_tool_image(evidence_id).ok_or_else(|| anyhow::anyhow!("missing scoped fixture capture"))
                })?;
            DesignReviewTool.execute(&json!({"evidence_id":id,"passed":true,"note":"Fixture boolean cannot override mismatch"}))?;
            Ok::<_, anyhow::Error>(compared)
        }).unwrap();
        let accepted = serde_json::from_str::<Value>(&result.text).unwrap()["accepted"]
            .as_bool()
            .unwrap();
        if id == "bad" {
            assert!(!accepted);
            assert!(design.gate().blocks_completion());
            design.begin_repair().unwrap();
        } else {
            assert!(accepted);
            assert_eq!(design.gate(), GateDecision::Verified);
        }
    }
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests {
        media::tests::assert_no_storage_metadata(&request);
        assert_eq!(request["model"], MODEL);
        assert_eq!(request["stream"], true);
        let selected: Vec<_> = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "image_url")
            .collect();
        assert_eq!(
            selected.len(),
            if request["messages"].as_array().unwrap().len() > 2 {
                2
            } else {
                1
            }
        );
        assert_eq!(selected[0]["image_url"]["url"], original_url);
    }
    assert_eq!(design.snapshot().repairs, 1);
    assert!(parent.scoped_tool_image("repaired").is_none());
    assert_eq!(
        serde_json::to_value(parent_design.snapshot()).unwrap(),
        parent_before
    );
}

const TARGET: &str = "http://fixture.invalid/product";
const HEIGHT: u32 = 80;

struct TestHome {
    root: PathBuf,
    env: Option<EnvGuard>,
}
impl TestHome {
    // Caller holds HOME, then TAPE, then TODO test locks throughout the async run.
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "aizen-design-loop-{tag}-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let env = EnvGuard::set([
            ("AIZEN_HOME", root.as_os_str()),
            ("AIZEN_PROJECT_ROOT", root.as_os_str()),
        ]);
        Self {
            root,
            env: Some(env),
        }
    }
}
impl Drop for TestHome {
    fn drop(&mut self) {
        drop(self.env.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Default)]
struct FixtureRender {
    source: Mutex<Vec<u8>>,
    captures: AtomicUsize,
    patches: AtomicUsize,
}
impl FixtureRender {
    fn new() -> Self {
        Self {
            source: Mutex::new(b"fixture layout before patch".to_vec()),
            ..Self::default()
        }
    }
}

struct FixtureCapture(Arc<FixtureRender>);
impl Tool for FixtureCapture {
    fn name(&self) -> &str {
        "browser_screenshot"
    }
    fn description(&self) -> &str {
        "Test-only generated PNG; never opens a browser or network."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"id":{"type":"string"},"width":{"type":"integer"}},"required":["id","width"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    // Optional-image capability lets the Unknown-policy test record a text capture while the
    // real executor withholds its payload. This is not a replacement for native browser consent.
    fn may_return_images(&self, _args: &Value) -> bool {
        true
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        anyhow::bail!("fixture must use structured observations")
    }
    fn execute_observation(&self, args: &Value) -> Result<ToolObservation> {
        let ctx = crate::core::exec_ctx::current()
            .and_then(|ctx| ctx.design())
            .ok_or_else(|| anyhow::anyhow!("fixture needs scoped design context"))?;
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing id"))?;
        let width = args["width"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing width"))?;
        ensure!(
            matches!(width, 1200 | 390),
            "fixture viewport must be desktop/mobile"
        );
        let width = width as u32;
        ctx.reserve_visual_operation()?;
        let source = sha256(&self.0.source.lock().unwrap());
        ctx.set_target(TARGET)?;
        ctx.set_source_revision(&source)?;
        let number = self.0.captures.fetch_add(1, Ordering::SeqCst) + 1;
        let color = [number as u8, (width % 255) as u8, 96, 255];
        let pixels = color.repeat(width as usize * HEIGHT as usize);
        let png = encode_rgba_png(&pixels, width, HEIGHT)?;
        let mut provenance = ImageProvenance::new("generated-loop-fixture", None);
        provenance.evidence_id = Some(id.into());
        provenance.design_scope = Some(ctx.evidence_scope());
        let image = ToolImage::from_bytes(&png, "image/png", provenance)?;
        assert_eq!(image.metadata().width, Some(width));
        assert_eq!(image.metadata().height, Some(HEIGHT));
        ctx.record_capture(
            id,
            Some(&source),
            TARGET,
            Viewport {
                width,
                height: HEIGHT,
            },
            ctx.current_revision(),
            true,
            "Generated offline fixture; readiness/source binding are simulated",
        )?;
        Ok(ToolObservation {
            text: format!("Generated capture {id}: {width}x{HEIGHT}; fixture {number}"),
            images: vec![image],
        })
    }
}

/// Build the canonical NON-GIT fixture identity without discovery (which would spawn Git).
/// Only this test owns this unique root. Other tests may create unrelated store/resource locks
/// under the shared temporary HOME, so the top-level `locks` directory proves nothing about it.
fn fixture_identity(root: &std::path::Path) -> WorkspaceIdentity {
    let canonical_root = std::fs::canonicalize(root).unwrap();
    let mut normalized = canonical_root.to_string_lossy().replace('\\', "/");
    if let Some(rest) = normalized.strip_prefix("//?/") {
        normalized = rest.to_owned();
    }
    if normalized.as_bytes().get(1) == Some(&b':') {
        normalized.replace_range(0..1, &normalized[..1].to_ascii_lowercase());
    }
    let normalized = normalized.trim_end_matches('/');
    WorkspaceIdentity {
        repo_id: stable_key(&format!("repo:{normalized}")),
        worktree_id: stable_key(&format!("worktree:{normalized}:non-git")),
        canonical_root,
        canonical_git_dir: None,
        canonical_common_git_dir: None,
    }
}

fn assert_no_fixture_writer_lease(identity: &WorkspaceIdentity) {
    assert!(
        !WorkspaceWriterLease::held_by_this_process(identity),
        "fixture must release actual writer ownership after the body settles"
    );
}

struct FixturePatch {
    render: Arc<FixtureRender>,
    identity: WorkspaceIdentity,
}
impl Tool for FixturePatch {
    fn name(&self) -> &str {
        "fixture_patch"
    }
    fn description(&self) -> &str {
        "Test-only in-memory source patch, no filesystem or process."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","additionalProperties":false})
    }
    // This opaque fixture uses the same exclusive admission as any unknown workspace mutator.
    // It does NOT test native disk writes, but must not fabricate a lease-free writer capability.
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::Paths
    }
    fn recovery_effect(&self, _args: &Value) -> bool {
        false
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        // Assert the real executor's conservative opaque authority while the body is running.
        assert!(WorkspaceWriterLease::held_by_this_process(&self.identity));
        *self.render.source.lock().unwrap() = b"fixture layout after successful patch".to_vec();
        self.render.patches.fetch_add(1, Ordering::SeqCst);
        // No direct DesignContext invalidation here: run_agent_loop must do it on success.
        Ok("Fixture source patched successfully".into())
    }
}

fn registry(render: &Arc<FixtureRender>, identity: &WorkspaceIdentity) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(DesignBriefTool));
    registry.register(Box::new(DesignReviewTool));
    registry.register(Box::new(FixtureCapture(render.clone())));
    registry.register(Box::new(FixturePatch {
        render: render.clone(),
        identity: identity.clone(),
    }));
    registry
}

fn config(
    home: &TestHome,
    design: &DesignContext,
    policy: VisionPolicy,
    bound: usize,
) -> AgentConfig {
    AgentConfig {
        max_iters: bound,
        auto_extend_to: bound,
        approval_mode: crate::core::approval::ApprovalMode::Yolo,
        exec_ctx: ExecutionContext::default()
            .with_resource_scope(design.evidence_scope())
            .with_trace_visible(false)
            .with_design(design.clone()),
        workspace_root: Some(home.root.clone()),
        quiet: true,
        enable_verify_gate: false,
        auto_checkpoint: false,
        checkpoint_each_edit: false,
        context_window: 0,
        collapse_after_observations: usize::MAX,
        spill_result_over_chars: 0,
        harness_check_after_edits: 0,
        todo_reminder_every: 0,
        run_id: None,
        enable_self_review: false,
        enable_lsp: false,
        enable_todo_poke: false,
        enable_confidence_gate: false,
        enable_hill_climb: false,
        max_transient_retries: 0,
        max_continuations: 0,
        max_stall_recoveries: 0,
        goal: None,
        enable_steering: false,
        on_progress: None,
        step_note: None,
        deadline: None,
        tool_image_policy: policy,
        tool_image_destination: Some(destination_key(
            "https://fixture.invalid/v1",
            "fixture-vision",
        )),
        tool_image_destination_label: Some("offline generated-image fixture".into()),
        ..AgentConfig::default()
    }
}

fn tool_call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: FunctionCall {
            name: name.into(),
            arguments: args.to_string(),
        },
    }
}
fn turn(calls: Vec<ToolCall>, text: Option<&str>) -> ChatTurn {
    ChatTurn {
        content: text.map(str::to_owned),
        tool_calls: calls,
        finish_reason: Some("stop".into()),
        usage: None,
        empty_wire_note: None,
        empty_wire_raw: None,
        eager: Vec::new(),
    }
}
fn brief() -> ChatTurn {
    turn(
        vec![tool_call(
            "brief",
            "design_brief",
            json!({
                "intent":"create", "brief":"Generated offline product layout fixture",
                "acceptance":"Scripted checks at desktop and mobile; not live visual validation"
            }),
        )],
        None,
    )
}
fn capture(id: &str, width: u32) -> ToolCall {
    tool_call(id, "browser_screenshot", json!({"id":id,"width":width}))
}
fn review(id: &str, passed: bool) -> ToolCall {
    tool_call(
        &format!("review-{id}"),
        "design_review",
        json!({
            "evidence_id":id, "passed":passed,
            "note": if passed { "Scripted fixture review passes" } else { "Scripted fixture review finds overflow" }
        }),
    )
}

/// Exercise the actual destination-bound projector and occurrence marker on each simulated
/// successful request. The fixture consumes the projected wire copy, never the durable history.
fn projected_ids(messages: &[Message], cfg: &AgentConfig, ctx: &DesignContext) -> Vec<String> {
    assert!(messages.iter().all(|message| message.images.is_empty()));
    let projection = project_messages_for_destination(
        messages,
        cfg.tool_image_policy,
        cfg.tool_image_destination.as_deref(),
    );
    assert!(projection
        .messages
        .iter()
        .all(|message| message.tool_images.is_empty()));
    let wire_count: usize = projection
        .messages
        .iter()
        .map(|message| {
            if !message.images.is_empty() {
                assert_eq!(message.role, "user");
                assert!(message
                    .images
                    .iter()
                    .all(|image| image.starts_with("data:image/png;base64,")));
            }
            message.images.len()
        })
        .sum();
    assert_eq!(wire_count, projection.selected_tool_images.len());
    let ids = projection
        .selected_tool_images
        .iter()
        .map(|occurrence| {
            let image = &messages[occurrence.message_index].tool_images[occurrence.image_index];
            assert_eq!(image.metadata().sha256, occurrence.sha256);
            assert_eq!(
                image.metadata().provenance.design_scope.as_deref(),
                Some(ctx.evidence_scope().as_str())
            );
            image.metadata().provenance.evidence_id.clone().unwrap()
        })
        .collect();
    // A successful scripted ChatTurn stands for delivery; no provider/replay/global state is used.
    mark_projected_evidence(messages, &projection.selected_tool_images, ctx);
    ids
}
fn evidence(ctx: &DesignContext, id: &str) -> super::VisualEvidence {
    ctx.snapshot()
        .evidence
        .into_iter()
        .find(|image| image.id == id)
        .unwrap()
}
fn visual(outcome: &AgentOutcome) -> &Value {
    &outcome
        .verification
        .as_ref()
        .expect("typed design verification")["visual"]
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "HOME, replay tape and todo fixtures are process-global; serialize their complete async run in HOME→TAPE→TODO lock order"
)]
async fn real_loop_repairs_reprojects_two_viewports_and_returns_typed_verified() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _tape_lock = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _todo_lock = crate::agent::todo::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = TestHome::new("verified");
    let design = DesignContext::new(
        "loop-fixture",
        home.root.to_string_lossy().into_owned(),
        DesignMode::Auto,
    );
    design.begin_turn("repair-product", home.root.to_string_lossy().into_owned());
    let cfg = config(&home, &design, VisionPolicy::Supported, 9);
    let render = Arc::new(FixtureRender::new());
    // Deterministically cover the parallel-suite failure: unrelated store artifacts may share
    // HOME/locks without this unique fixture workspace ever taking a writer lease.
    std::fs::create_dir_all(home.root.join("locks/v1/stores/unrelated-fixture")).unwrap();
    let identity = fixture_identity(&home.root);
    assert_no_fixture_writer_lease(&identity);
    let registry = registry(&render, &identity);
    let calls = AtomicUsize::new(0);
    let revision_before_patch = Mutex::new(None);
    let chat = |messages: Vec<Message>, _defs: Vec<ToolDef>| {
        let index = calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            index < 9,
            "script exhausted: loop did not honor visual completion"
        );
        let selected = projected_ids(&messages, &cfg, &design);
        let next = match index {
            0 => brief(),
            1 => turn(vec![capture("desktop-before", 1200)], None),
            2 => {
                assert!(selected.contains(&"desktop-before".into()));
                assert!(evidence(&design, "desktop-before").projected);
                turn(vec![review("desktop-before", false)], None)
            }
            3 => {
                assert_eq!(
                    evidence(&design, "desktop-before").status,
                    EvidenceStatus::Failed
                );
                assert_eq!(design.snapshot().repairs, 0);
                // The REAL loop must reject this completion and begin a repair pass.
                turn(
                    Vec::new(),
                    Some("Premature completion after failed fixture review"),
                )
            }
            4 => {
                assert_eq!(design.snapshot().repairs, 1);
                assert!(messages.iter().any(|message| message.role == "user"
                    && message
                        .content
                        .as_deref()
                        .is_some_and(|text| text.starts_with("Visual acceptance gate:"))));
                assert_eq!(
                    evidence(&design, "desktop-before").status,
                    EvidenceStatus::Stale
                );
                *revision_before_patch.lock().unwrap() = Some(design.current_revision());
                // This capture is fresh just before patching. The loop, not the fake patch,
                // must invalidate it after the successful mutation in this same turn.
                turn(
                    vec![
                        capture("repair-before-patch", 1200),
                        tool_call("patch", "fixture_patch", json!({})),
                    ],
                    None,
                )
            }
            5 => {
                let previous = revision_before_patch.lock().unwrap().unwrap();
                assert_eq!(
                    design.current_revision(),
                    previous + 1,
                    "successful loop edit must invalidate render revision"
                );
                let old = evidence(&design, "repair-before-patch");
                assert_eq!(old.render_revision, previous);
                assert_eq!(old.status, EvidenceStatus::Stale);
                assert_eq!(render.patches.load(Ordering::SeqCst), 1);
                turn(
                    vec![capture("desktop-after", 1200), capture("mobile-after", 390)],
                    None,
                )
            }
            6 => {
                for id in ["desktop-after", "mobile-after"] {
                    assert!(selected.contains(&id.into()));
                    let fresh = evidence(&design, id);
                    assert!(fresh.projected && fresh.ready);
                    assert_eq!(fresh.status, EvidenceStatus::Pending);
                    assert_eq!(fresh.render_revision, design.current_revision());
                    assert_eq!(
                        fresh.source_hash,
                        Some(sha256(&render.source.lock().unwrap()))
                    );
                }
                turn(vec![review("desktop-after", true)], None)
            }
            7 => {
                assert_eq!(
                    evidence(&design, "desktop-after").status,
                    EvidenceStatus::Verified
                );
                assert!(
                    design.gate().blocks_completion(),
                    "desktop alone must not complete"
                );
                turn(vec![review("mobile-after", true)], None)
            }
            8 => {
                assert_eq!(design.gate(), GateDecision::Verified);
                turn(Vec::new(), Some("Scripted visual fixture complete"))
            }
            _ => unreachable!(),
        };
        std::future::ready(Ok(next))
    };
    let mut messages = vec![
        Message::system("Offline loop test"),
        Message::user("Repair the generated fixture"),
    ];
    let outcome = run_agent_loop(chat, &cfg, &registry, &mut messages)
        .await
        .unwrap();
    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.iters, 9);
    assert_eq!(calls.load(Ordering::SeqCst), 9);
    assert_eq!(render.captures.load(Ordering::SeqCst), 4);
    assert_eq!(visual(&outcome)["status"], "verified");
    assert_eq!(visual(&outcome)["repair_rounds"], 1);
    assert_eq!(visual(&outcome)["operations"], 4);
    assert_eq!(
        outcome.verification.as_ref().unwrap()["build_test"],
        "unverified"
    );
    let rows = visual(&outcome)["evidence"].as_array().unwrap();
    for (id, width) in [("desktop-after", 1200), ("mobile-after", 390)] {
        let row = rows.iter().find(|row| row["id"] == id).unwrap();
        assert_eq!(row["status"], "verified");
        assert_eq!(row["width"], width);
        assert_eq!(row["pixels_delivered"], true);
        assert_eq!(row["ready"], true);
        assert_eq!(row["revision"], design.current_revision());
    }
    assert!(
        messages.iter().all(|message| message.images.is_empty()),
        "wire images must never alter durable history"
    );
    assert!(
        !home.root.join("cli-config.json").exists(),
        "no personal/persistent config is written"
    );
    assert_no_fixture_writer_lease(&identity);
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "HOME, replay tape and todo fixtures are process-global; serialize their complete async run in HOME→TAPE→TODO lock order"
)]
async fn real_loop_rejects_unprojected_review_and_returns_typed_unavailable() {
    let _home_lock = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _tape_lock = crate::llm::replay::TAPE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _todo_lock = crate::agent::todo::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = TestHome::new("unavailable");
    let design = DesignContext::new(
        "loop-unavailable",
        home.root.to_string_lossy().into_owned(),
        DesignMode::Auto,
    );
    design.begin_turn("unseen-product", home.root.to_string_lossy().into_owned());
    let cfg = config(&home, &design, VisionPolicy::Unknown, 4);
    let render = Arc::new(FixtureRender::new());
    let identity = fixture_identity(&home.root);
    assert_no_fixture_writer_lease(&identity);
    let registry = registry(&render, &identity);
    let calls = AtomicUsize::new(0);
    let chat = |messages: Vec<Message>, _defs: Vec<ToolDef>| {
        let index = calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            index < 4,
            "unavailable visual setup must stop, not retry indefinitely"
        );
        assert!(projected_ids(&messages, &cfg, &design).is_empty());
        let next =
            match index {
                0 => brief(),
                1 => turn(vec![capture("unseen-desktop", 1200)], None),
                2 => {
                    assert!(!evidence(&design, "unseen-desktop").projected);
                    let image = messages
                        .iter()
                        .flat_map(|message| &message.tool_images)
                        .next()
                        .unwrap();
                    assert!(
                        !image.is_available(),
                        "Unknown policy must withhold the actual payload"
                    );
                    turn(vec![review("unseen-desktop", true)], None)
                }
                3 => {
                    assert_eq!(
                        evidence(&design, "unseen-desktop").status,
                        EvidenceStatus::Pending
                    );
                    assert!(!evidence(&design, "unseen-desktop").projected);
                    assert!(messages.iter().any(|message| message.role == "tool"
                        && message.content.as_deref().is_some_and(
                            |text| text.starts_with("error: visual review unavailable:")
                        )));
                    // Confident prose cannot manufacture visual verification without projected pixels.
                    turn(
                        Vec::new(),
                        Some("Claimed visual success without seeing any pixels"),
                    )
                }
                _ => unreachable!(),
            };
        std::future::ready(Ok(next))
    };
    let mut messages = vec![
        Message::system("Offline loop test"),
        Message::user("Review the unseen fixture"),
    ];
    let outcome = run_agent_loop(chat, &cfg, &registry, &mut messages)
        .await
        .unwrap();
    assert_eq!(outcome.stop, StopReason::VerificationFailed);
    assert_eq!(outcome.iters, 4);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(visual(&outcome)["status"], "unavailable");
    assert!(visual(&outcome)["reason"]
        .as_str()
        .unwrap()
        .contains("no confirmed tool-image support"));
    assert_eq!(visual(&outcome)["evidence"][0]["pixels_delivered"], false);
    assert_eq!(
        outcome.verification.as_ref().unwrap()["build_test"],
        "not_required"
    );
    assert!(outcome
        .final_text
        .as_deref()
        .unwrap()
        .contains("Visual verification incomplete:"));
    assert_eq!(render.captures.load(Ordering::SeqCst), 1);
    assert_eq!(render.patches.load(Ordering::SeqCst), 0);
    assert!(!home.root.join("cli-config.json").exists());
}
