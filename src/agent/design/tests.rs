use super::*;

fn context() -> DesignContext {
    let ctx = DesignContext::new("session-a", "/workspace", DesignMode::Auto);
    ctx.begin_turn("task-a", "/workspace");
    ctx.apply_brief(
        DesignBrief {
            intent: DesignIntent::Create,
            brief: "Readable product page".into(),
            acceptance: "Headline and primary action visible without overflow".into(),
            ..Default::default()
        },
        false,
    )
    .unwrap();
    ctx.set_target("http://localhost:3000/").unwrap();
    ctx.set_source_revision("trusted-workspace-hash").unwrap();
    ctx
}
fn capture(ctx: &DesignContext, id: &str, width: u32) {
    ctx.record_capture(
        id,
        Some("trusted-workspace-hash"),
        "http://localhost:3000/",
        Viewport { width, height: 800 },
        ctx.current_revision(),
        true,
        "fonts/images/layout settled; build marker matches",
    )
    .unwrap();
}
fn verified_pair(ctx: &DesignContext) {
    for (id, width) in [("desktop", 1200), ("mobile", 390)] {
        capture(ctx, id, width);
        ctx.mark_projected(id).unwrap();
        ctx.record_review(
            id,
            true,
            "Readable headline and action; no overflow in actual shown image",
        )
        .unwrap();
    }
}

#[test]
fn selected_reference_child_is_active_and_does_not_share_acceptance_or_grants() {
    let parent = context();
    let child = parent.fork_child("graphic-worker", "/workspace");
    assert_eq!(child.gate(), GateDecision::NotRequired);
    child
        .activate_selected_references(vec![("ref-a".into(), "a".repeat(64))], FidelityMode::Exact)
        .unwrap();
    assert!(child.snapshot().active);
    assert!(child.gate().blocks_completion());
    assert!(child.capsule().unwrap().contains("ref-a"));
    assert!(parent.snapshot().references.is_empty());
    assert!(child
        .record_reference_observation("other", &"a".repeat(64), ReferenceObservationKind::Original)
        .is_err());
    assert!(child
        .record_reference_observation("ref-a", &"b".repeat(64), ReferenceObservationKind::Original)
        .is_err());
    child
        .record_reference_observation("ref-a", &"a".repeat(64), ReferenceObservationKind::Preview)
        .unwrap();
    let before = serde_json::to_value(child.snapshot()).unwrap();
    assert!(child
        .apply_brief(
            DesignBrief {
                intent: DesignIntent::Preserve,
                brief: "waive".into(),
                acceptance: "pass".into(),
                fidelity_mode: Some(FidelityMode::Measure),
                ..Default::default()
            },
            false
        )
        .is_err());
    assert_eq!(serde_json::to_value(child.snapshot()).unwrap(), before);
}

#[test]
fn missing_or_failed_measured_comparison_cannot_be_overridden_by_model_boolean() {
    use super::reference_compare::{compare_rgba, ComparisonBinding, ComparisonRequest, RgbaImage};
    let _lock = crate::llm::image_send::image_test_lock();
    let ctx = context();
    ctx.require_viewports(vec![Viewport {
        width: 2,
        height: 1,
    }])
    .unwrap();
    ctx.activate_selected_references(vec![("ref-a".into(), "a".repeat(64))], FidelityMode::Exact)
        .unwrap();
    ctx.set_target("local").unwrap();
    ctx.set_source_revision("source").unwrap();
    ctx.record_capture(
        "out",
        Some("source"),
        "local",
        Viewport {
            width: 2,
            height: 1,
        },
        ctx.current_revision(),
        true,
        "fixture",
    )
    .unwrap();
    ctx.mark_projected("out").unwrap();
    ctx.record_review("out", true, "model says pass").unwrap();
    assert!(ctx.gate().blocks_completion());
    ctx.record_reference_observation("ref-a", &"a".repeat(64), ReferenceObservationKind::Preview)
        .unwrap();
    assert!(ctx.gate().blocks_completion());
    ctx.record_reference_observation("ref-a", &"a".repeat(64), ReferenceObservationKind::Original)
        .unwrap();
    let binding = ComparisonBinding {
        reference_id: "ref-a".into(),
        reference_sha256: "a".repeat(64),
        output_evidence_id: "out".into(),
        output_sha256: "b".repeat(64),
        source_hash: "source".into(),
        render_revision: ctx.current_revision(),
        target: "local".into(),
        viewport: Viewport {
            width: 2,
            height: 1,
        },
        dpr_milli: 1000,
        font_identity: "fixture-font".into(),
        frame_state: "fixture-frame".into(),
        request: ComparisonRequest {
            mode: FidelityMode::Exact,
            ..Default::default()
        },
    };
    let reference = RgbaImage {
        width: 2,
        height: 1,
        pixels: vec![0; 8],
    };
    let mut output = reference.clone();
    output.pixels[0] = 1;
    ctx.record_comparison(
        compare_rgba(&reference, &output, binding.clone())
            .unwrap()
            .receipt,
    )
    .unwrap();
    ctx.record_review("out", true, "second model says pass anyway")
        .unwrap();
    assert!(ctx.gate().blocks_completion());
    ctx.record_comparison(
        compare_rgba(&reference, &reference, binding.clone())
            .unwrap()
            .receipt,
    )
    .unwrap();
    assert!(
        ctx.gate().blocks_completion(),
        "equal observed pixels do not invent font/frame identity proof"
    );
    ctx.record_capture_identity(
        "out",
        CaptureIdentity {
            output_sha256: binding.output_sha256.clone(),
            dpr_milli: binding.dpr_milli,
            font_identity: binding.font_identity.clone(),
            frame_state: binding.frame_state.clone(),
        },
    )
    .unwrap();
    ctx.record_comparison(
        compare_rgba(&reference, &reference, binding)
            .unwrap()
            .receipt,
    )
    .unwrap();
    assert_eq!(ctx.gate(), GateDecision::Verified);
    ctx.mark_render_changed();
    assert!(ctx.snapshot().comparisons.is_empty());
    assert!(ctx.gate().blocks_completion());
    ctx.set_source_revision("changed").unwrap();
    assert!(ctx.gate().blocks_completion());
}

#[test]
fn restored_reference_identity_cannot_restore_inspection_or_comparison_acceptance() {
    let ctx = context();
    ctx.activate_selected_references(vec![("ref-a".into(), "a".repeat(64))], FidelityMode::Exact)
        .unwrap();
    ctx.record_reference_observation("ref-a", &"a".repeat(64), ReferenceObservationKind::Original)
        .unwrap();
    let saved = ctx.snapshot();
    ctx.restore(saved).unwrap();
    assert!(ctx.snapshot().reference_observations.is_empty());
    assert!(ctx.snapshot().references[0].original_sha256.is_none());
    assert!(ctx.snapshot().comparisons.is_empty());
    ctx.apply_brief(DesignBrief::default(), true).unwrap();
    assert!(ctx.gate().blocks_completion());
}

#[test]
fn controls_are_scoped_and_nonvisual_auto_is_inactive() {
    assert_eq!("AUTO".parse::<DesignMode>().unwrap(), DesignMode::Auto);
    assert!("enabled".parse::<DesignMode>().is_err());
    let a = context();
    let b = DesignContext::new("b", "/workspace", DesignMode::Auto);
    b.begin_turn("backend", "/workspace");
    b.set_intent(DesignIntent::Nonvisual);
    assert!(b.capsule().is_none());
    assert_eq!(b.gate(), GateDecision::NotRequired);
    a.set_mode(DesignMode::Off);
    assert_eq!(a.gate(), GateDecision::Disabled);
    assert!(a.capsule().is_none());
    assert_eq!(b.mode(), DesignMode::Auto);
    // Off disables automation, not explicit user-selected knowledge or manual state tools.
    a.apply_brief(
        DesignBrief {
            intent: DesignIntent::Preserve,
            brief: "Keep brand".into(),
            acceptance: "Same logo".into(),
            ..Default::default()
        },
        false,
    )
    .unwrap();
    assert!(guide::get("layout").is_some());
}

#[test]
fn visual_intent_demands_a_brief_without_keyword_or_role_gate() {
    let ctx = DesignContext::default();
    ctx.set_intent(DesignIntent::Preserve);
    assert!(ctx.capsule().is_some());
    assert!(ctx.gate().blocks_completion());
    ctx.set_intent(DesignIntent::Unknown);
    assert_eq!(ctx.gate(), GateDecision::NotRequired);
}

#[test]
fn new_turn_has_pointer_only_until_explicit_resume_and_finalization_archives() {
    let ctx = context();
    assert!(ctx
        .capsule()
        .unwrap()
        .contains("Start from the user's audience"));
    ctx.finalize();
    assert!(ctx.snapshot().archived);
    assert!(ctx.capsule().unwrap().contains("Prior design brief"));
    ctx.begin_turn("new-question", "/workspace");
    let pointer = ctx.capsule().unwrap();
    assert!(pointer.contains("NOT active"));
    assert!(!pointer.contains("Start from the user's audience"));
    assert_eq!(ctx.gate(), GateDecision::NotRequired);
    ctx.apply_brief(DesignBrief::default(), true).unwrap();
    assert!(ctx.snapshot().active);
    assert!(ctx.gate().blocks_completion());
    ctx.begin_turn("foreign", "/other-workspace");
    assert!(ctx.capsule().is_none());
}

#[test]
fn visual_review_requires_actual_projection_ready_fresh_source_and_two_viewports() {
    let ctx = context();
    capture(&ctx, "desktop", 1200);
    assert!(ctx.record_review("desktop", true, "guess").is_err());
    ctx.mark_projected("desktop").unwrap();
    ctx.record_review("desktop", true, "actual image: no overflow")
        .unwrap();
    assert!(
        ctx.gate().blocks_completion(),
        "desktop alone is insufficient"
    );
    capture(&ctx, "mobile", 390);
    ctx.mark_projected("mobile").unwrap();
    ctx.record_review("mobile", true, "actual image: primary action readable")
        .unwrap();
    assert_eq!(ctx.gate(), GateDecision::Verified);
    ctx.mark_render_changed();
    assert!(ctx.gate().blocks_completion());
    assert!(ctx.record_review("mobile", true, "old image").is_err());
}

#[test]
fn unknown_source_or_unready_capture_can_never_verify() {
    let ctx = context();
    ctx.record_capture(
        "old",
        None,
        "http://localhost:3000/",
        Viewport {
            width: 1200,
            height: 800,
        },
        ctx.current_revision(),
        true,
        "server alive but source correspondence unknown",
    )
    .unwrap();
    ctx.mark_projected("old").unwrap();
    assert!(ctx.record_review("old", true, "looks fine").is_err());
    ctx.record_capture(
        "loading",
        Some("trusted-workspace-hash"),
        "http://localhost:3000/",
        Viewport {
            width: 390,
            height: 800,
        },
        ctx.current_revision(),
        false,
        "fonts not settled",
    )
    .unwrap();
    ctx.mark_projected("loading").unwrap();
    assert!(ctx.record_review("loading", true, "looks fine").is_err());
    ctx.mark_unavailable("endpoint does not support image projection");
    assert!(matches!(ctx.gate(), GateDecision::Unavailable(_)));
    assert!(ctx.gate().blocks_completion());
    ctx.skip_by_user("user explicitly accepts unverified appearance")
        .unwrap();
    assert!(matches!(ctx.gate(), GateDecision::SkippedByUser(_)));
    assert!(!ctx.gate().is_verified());
}

#[test]
fn repair_and_capture_budgets_are_bounded_not_endless_model_retries() {
    let ctx = context();
    for i in 0..4 {
        capture(&ctx, &format!("capture-{i}"), 1200);
    }
    assert!(ctx
        .record_capture(
            "fifth",
            None,
            "http://localhost:3000/",
            Viewport {
                width: 390,
                height: 800
            },
            ctx.current_revision(),
            true,
            "x"
        )
        .is_err());
    assert!(matches!(ctx.gate(), GateDecision::Failed(_)));
    ctx.begin_repair().unwrap();
    assert_eq!(ctx.snapshot().captures_this_pass, 0);
    ctx.begin_repair().unwrap();
    assert!(ctx.begin_repair().is_err());
    assert_eq!(ctx.snapshot().repairs, 2);
}

#[test]
fn failed_visual_calls_consume_a_shared_task_budget_without_evidence() {
    let ctx = context();
    let shared = ctx.clone();
    for _ in 0..super::state::MAX_VISUAL_OPERATIONS {
        // Reservation precedes execution; a failing browser/image call records no capture.
        shared.reserve_visual_operation().unwrap();
    }
    assert!(ctx.snapshot().evidence.is_empty());
    assert_eq!(ctx.snapshot().visual_operations, 36);
    assert!(ctx.reserve_visual_operation().is_err());
    assert_eq!(ctx.snapshot().visual_operations, 36);
    assert!(matches!(ctx.gate(), GateDecision::Failed(_)));
    ctx.mark_unavailable("browser unreachable");
    assert!(matches!(ctx.gate(), GateDecision::Failed(_)));
    assert!(ctx.capsule().unwrap().len() <= 2500);
}

#[test]
fn repairs_brief_changes_and_resume_never_refill_visual_operations() {
    let ctx = context();
    ctx.reserve_visual_operation().unwrap();
    ctx.begin_repair().unwrap();
    assert_eq!(ctx.snapshot().visual_operations, 1);
    let mut changed = ctx.brief().unwrap();
    changed.acceptance = "Readable primary action at both viewports".into();
    ctx.apply_brief(changed, false).unwrap();
    ctx.apply_brief(DesignBrief::default(), true).unwrap();
    assert_eq!(ctx.snapshot().visual_operations, 1);
    ctx.mark_render_changed();
    assert_eq!(ctx.snapshot().visual_operations, 1);
    let child = ctx.fork_child("fresh-child", "/workspace");
    assert_eq!(child.snapshot().visual_operations, 0);
    assert!(child.reserve_visual_operation().is_err());
    child.apply_brief(DesignBrief::default(), true).unwrap();
    child.reserve_visual_operation().unwrap();
    assert_eq!(ctx.snapshot().visual_operations, 1);
    ctx.begin_turn("fresh-task", "/workspace");
    assert_eq!(ctx.snapshot().visual_operations, 0);
    assert!(ctx.reserve_visual_operation().is_err());
    ctx.apply_brief(DesignBrief::default(), true).unwrap();
    ctx.reserve_visual_operation().unwrap();
    ctx.set_mode(DesignMode::Off);
    assert!(ctx.reserve_visual_operation().is_err());
    assert_eq!(ctx.snapshot().visual_operations, 1);
    ctx.set_mode(DesignMode::Auto);
    ctx.reset();
    assert_eq!(ctx.snapshot().visual_operations, 0);
}

#[test]
fn operation_exhaustion_never_overrides_current_verified_evidence() {
    let ctx = context();
    for _ in 0..super::state::MAX_VISUAL_OPERATIONS {
        ctx.reserve_visual_operation().unwrap();
    }
    // The last reserved operation can still earn verification; reserving itself is not failure.
    verified_pair(&ctx);
    assert_eq!(ctx.gate(), GateDecision::Verified);
    assert!(ctx.reserve_visual_operation().is_err());
    assert_eq!(ctx.gate(), GateDecision::Verified);
    ctx.mark_render_changed();
    assert!(matches!(ctx.gate(), GateDecision::Failed(_)));
    let unbriefed = DesignContext::default();
    unbriefed.set_intent(DesignIntent::Create);
    for _ in 0..super::state::MAX_VISUAL_OPERATIONS {
        unbriefed.reserve_visual_operation().unwrap();
    }
    assert!(matches!(unbriefed.gate(), GateDecision::Failed(_)));
}

#[test]
fn restored_operation_budget_is_defaulted_validated_and_not_replenished() {
    let ctx = context();
    ctx.reserve_visual_operation().unwrap();
    let saved = ctx.snapshot();
    let mut legacy = serde_json::to_value(&saved).unwrap();
    legacy.as_object_mut().unwrap().remove("visual_operations");
    let legacy: TaskDesignState = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.visual_operations, 0);
    let restored = DesignContext::new("session-a", "/workspace", DesignMode::Auto);
    restored.restore(saved.clone()).unwrap();
    restored.apply_brief(DesignBrief::default(), true).unwrap();
    assert_eq!(restored.snapshot().visual_operations, 1);
    let mut invalid = saved.clone();
    invalid.visual_operations = super::state::MAX_VISUAL_OPERATIONS + 1;
    assert!(restored.restore(invalid).is_err());
    assert_eq!(restored.snapshot().visual_operations, 1);
    let mut exhausted = saved;
    exhausted.visual_operations = super::state::MAX_VISUAL_OPERATIONS;
    restored.restore(exhausted).unwrap();
    restored.apply_brief(DesignBrief::default(), true).unwrap();
    assert!(restored.reserve_visual_operation().is_err());
    assert!(matches!(restored.gate(), GateDecision::Failed(_)));
}

#[test]
fn child_forks_and_restored_snapshots_cannot_reuse_parent_verified_evidence() {
    let ctx = context();
    verified_pair(&ctx);
    let child = ctx.fork_child("child", "/workspace");
    assert_ne!(ctx.evidence_scope(), child.evidence_scope());
    assert!(child.snapshot().evidence.is_empty());
    assert!(child.inherited_constraints().unwrap().len() <= 900);
    child.set_mode(DesignMode::Off);
    assert_eq!(ctx.mode(), DesignMode::Auto);
    let snapshot = ctx.snapshot();
    let serialized = serde_json::to_string(&snapshot).unwrap();
    let saved: TaskDesignState = serde_json::from_str(&serialized).unwrap();
    let restored = DesignContext::new("session-a", "/workspace", DesignMode::Auto);
    restored.restore(saved).unwrap();
    assert!(!restored.snapshot().active);
    assert!(restored
        .snapshot()
        .evidence
        .iter()
        .all(|e| e.status == EvidenceStatus::Stale && !e.projected));
    restored.apply_brief(DesignBrief::default(), true).unwrap();
    assert!(restored.gate().blocks_completion());
    let foreign = DesignContext::new("other", "/workspace", DesignMode::Auto);
    assert!(foreign.restore(snapshot.clone()).is_err());
    foreign.restore_session(snapshot).unwrap();
    assert_eq!(foreign.snapshot().owner_id, "session-a");
    let foreign_workspace = DesignContext::new("session-a", "/other", DesignMode::Auto);
    assert!(foreign_workspace.restore_session(ctx.snapshot()).is_err());
}

#[test]
fn model_cannot_downgrade_or_fake_current_visual_contract() {
    let ctx = context();
    ctx.set_intent(DesignIntent::Nonvisual);
    assert_eq!(ctx.snapshot().intent, DesignIntent::Create);
    assert!(ctx
        .apply_brief(
            DesignBrief {
                intent: DesignIntent::Unknown,
                ..Default::default()
            },
            false
        )
        .is_err());
    let mut invalid = ctx.snapshot();
    invalid.evidence.push(VisualEvidence {
        id: "spoof".into(),
        owner_id: "sibling".into(),
        task_id: invalid.task_id.clone(),
        status: EvidenceStatus::Verified,
        source_hash: invalid.source_hash.clone(),
        target: invalid.target.clone().unwrap(),
        viewport: Viewport {
            width: 1200,
            height: 800,
        },
        render_revision: invalid.render_revision,
        projected: true,
        ready: true,
        pass: 0,
        note: "spoof".into(),
        capture_identity: None,
    });
    assert!(ctx.restore(invalid).is_err());
    assert!(ctx.capsule().unwrap().len() <= 2500);
}
