#[test]
fn inbox_snapshot_round_trips() {
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 2,
        entries: vec![void_box::sidecar::InboxEntry {
            message_id: "msg-001".into(),
            from_candidate_id: "c-2".into(),
            kind: "proposal".into(),
            payload: serde_json::json!({"summary_text": "hello"}),
        }],
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let parsed: void_box::sidecar::InboxSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.entries[0].message_id, "msg-001");
}

#[test]
fn sidecar_intent_minimal_fields() {
    let json = r#"{"kind":"signal","audience":"broadcast","payload":{"summary_text":"converging"},"priority":"normal"}"#;
    let intent: void_box::sidecar::SubmittedIntent = serde_json::from_str(json).unwrap();
    assert_eq!(intent.kind, "signal");
    assert_eq!(intent.audience, "broadcast");
}

#[test]
fn sidecar_intent_batch_parses() {
    let json = r#"[
        {"kind":"proposal","audience":"broadcast","payload":{"summary_text":"A"},"priority":"normal"},
        {"kind":"evaluation","audience":"leader","payload":{"summary_text":"B"},"priority":"high"}
    ]"#;
    let batch: Vec<void_box::sidecar::SubmittedIntent> = serde_json::from_str(json).unwrap();
    assert_eq!(batch.len(), 2);
}

#[test]
fn sidecar_context_serializes() {
    let ctx = void_box::sidecar::SidecarContext {
        execution_id: "exec-1".into(),
        candidate_id: "c-3".into(),
        iteration: 2,
        role: "candidate".into(),
        peers: vec!["c-1".into(), "c-2".into()],
        sidecar_version: "0.1.0".into(),
    };
    let json = serde_json::to_string(&ctx).unwrap();
    assert!(json.contains("\"iteration\":2"));
}

#[test]
fn sidecar_health_serializes() {
    let health = void_box::sidecar::SidecarHealth {
        status: "ok".into(),
        sidecar_version: "0.1.0".into(),
        run_id: "run-1".into(),
        buffer_depth: 0,
        inbox_version: 0,
        uptime_ms: 100,
    };
    let json = serde_json::to_string(&health).unwrap();
    assert!(json.contains("\"status\":\"ok\""));
}

#[test]
fn stamped_intent_includes_auto_fields() {
    let stamped = void_box::sidecar::StampedIntent {
        intent_id: "int-1".into(),
        from_candidate_id: "c-1".into(),
        iteration: 3,
        kind: "proposal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "test"}),
        priority: "normal".into(),
        ttl_iterations: 2,
    };
    let json = serde_json::to_string(&stamped).unwrap();
    assert!(json.contains("\"intent_id\":\"int-1\""));
    assert!(json.contains("\"iteration\":3"));
}
