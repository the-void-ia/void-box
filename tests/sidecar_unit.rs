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

use void_box::sidecar::SidecarState;

#[test]
fn state_starts_empty() {
    let state = SidecarState::new("run-1", "exec-1", "c-1", vec!["c-2".into()]);
    assert_eq!(state.inbox_version(), 0);
    assert_eq!(state.buffer_depth(), 0);
    assert_eq!(state.current_iteration(), 0);
}

#[test]
fn load_inbox_sets_version_and_iteration() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 5,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 3,
        entries: vec![],
    };
    state.load_inbox(snapshot);
    assert_eq!(state.inbox_version(), 5);
    assert_eq!(state.current_iteration(), 3);
}

#[test]
fn accept_intent_stamps_iteration_and_candidate() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 2,
        entries: vec![],
    };
    state.load_inbox(snapshot);

    let submitted = void_box::sidecar::SubmittedIntent {
        kind: "proposal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "test"}),
        priority: "normal".into(),
    };
    let result = state.accept_intent(submitted, None);
    assert!(result.is_ok());
    let stamped = result.unwrap();
    assert_eq!(stamped.iteration, 2);
    assert_eq!(stamped.from_candidate_id, "c-1");
    assert!(!stamped.intent_id.is_empty());
    assert_eq!(state.buffer_depth(), 1);
}

#[test]
fn rejects_intent_over_per_iteration_limit() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    };
    state.load_inbox(snapshot);

    for i in 0..3 {
        let submitted = void_box::sidecar::SubmittedIntent {
            kind: "proposal".into(),
            audience: "broadcast".into(),
            payload: serde_json::json!({"summary_text": format!("intent {i}")}),
            priority: "normal".into(),
        };
        assert!(state.accept_intent(submitted, None).is_ok());
    }
    // 4th should fail
    let submitted = void_box::sidecar::SubmittedIntent {
        kind: "signal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "too many"}),
        priority: "normal".into(),
    };
    let result = state.accept_intent(submitted, None);
    assert!(result.is_err());
}

#[test]
fn rejects_oversized_payload() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    };
    state.load_inbox(snapshot);

    let big_text = "x".repeat(5000);
    let submitted = void_box::sidecar::SubmittedIntent {
        kind: "proposal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": big_text}),
        priority: "normal".into(),
    };
    let result = state.accept_intent(submitted, None);
    assert!(result.is_err());
}

#[test]
fn drain_clears_buffer() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    };
    state.load_inbox(snapshot);

    let submitted = void_box::sidecar::SubmittedIntent {
        kind: "signal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "hello"}),
        priority: "normal".into(),
    };
    state.accept_intent(submitted, None).unwrap();
    assert_eq!(state.buffer_depth(), 1);

    let drained = state.drain_intents();
    assert_eq!(drained.len(), 1);
    assert_eq!(state.buffer_depth(), 0);

    // Second drain returns empty
    let drained2 = state.drain_intents();
    assert!(drained2.is_empty());
}

#[test]
fn dedup_by_content_hash() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    };
    state.load_inbox(snapshot);

    let intent = void_box::sidecar::SubmittedIntent {
        kind: "proposal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "same content"}),
        priority: "normal".into(),
    };
    assert!(state.accept_intent(intent.clone(), None).is_ok());
    // Same content should be deduped — not counted against limit
    assert!(state.accept_intent(intent, None).is_ok());
    assert_eq!(state.buffer_depth(), 1);
}

#[test]
fn idempotency_key_dedup() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    };
    state.load_inbox(snapshot);

    let intent1 = void_box::sidecar::SubmittedIntent {
        kind: "proposal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "first"}),
        priority: "normal".into(),
    };
    let intent2 = void_box::sidecar::SubmittedIntent {
        kind: "signal".into(),
        audience: "leader".into(),
        payload: serde_json::json!({"summary_text": "different"}),
        priority: "high".into(),
    };
    let key = Some("same-key".to_string());
    assert!(state.accept_intent(intent1, key.clone()).is_ok());
    // Same idempotency key → returns ok but no new intent
    assert!(state.accept_intent(intent2, key).is_ok());
    assert_eq!(state.buffer_depth(), 1);
}

#[test]
fn new_inbox_resets_iteration_counters() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot1 = void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    };
    state.load_inbox(snapshot1);

    // Fill up to limit
    for i in 0..3 {
        let submitted = void_box::sidecar::SubmittedIntent {
            kind: "proposal".into(),
            audience: "broadcast".into(),
            payload: serde_json::json!({"summary_text": format!("intent {i}")}),
            priority: "normal".into(),
        };
        assert!(state.accept_intent(submitted, None).is_ok());
    }

    // Drain and load new inbox → counters reset
    state.drain_intents();
    let snapshot2 = void_box::sidecar::InboxSnapshot {
        version: 2,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 2,
        entries: vec![],
    };
    state.load_inbox(snapshot2);
    assert_eq!(state.current_iteration(), 2);

    // Can accept intents again
    let submitted = void_box::sidecar::SubmittedIntent {
        kind: "proposal".into(),
        audience: "broadcast".into(),
        payload: serde_json::json!({"summary_text": "new iteration"}),
        priority: "normal".into(),
    };
    assert!(state.accept_intent(submitted, None).is_ok());
}

#[test]
fn incremental_inbox_query() {
    let mut state = SidecarState::new("run-1", "exec-1", "c-1", vec![]);
    let snapshot = void_box::sidecar::InboxSnapshot {
        version: 3,
        execution_id: "exec-1".into(),
        candidate_id: "c-1".into(),
        iteration: 2,
        entries: vec![
            void_box::sidecar::InboxEntry {
                message_id: "msg-1".into(),
                from_candidate_id: "c-2".into(),
                kind: "proposal".into(),
                payload: serde_json::json!({"summary_text": "A"}),
            },
            void_box::sidecar::InboxEntry {
                message_id: "msg-2".into(),
                from_candidate_id: "c-3".into(),
                kind: "signal".into(),
                payload: serde_json::json!({"summary_text": "B"}),
            },
        ],
    };
    state.load_inbox(snapshot);

    // Full inbox
    let full = state.get_inbox(None);
    assert_eq!(full.entries.len(), 2);
    assert_eq!(full.version, 3);

    // Incremental: since version 3 → no new entries
    let incremental = state.get_inbox(Some(3));
    assert!(incremental.entries.is_empty());
}

#[test]
fn messaging_skill_content_documents_cli() {
    let content = void_box::sidecar::messaging_skill_content();
    assert!(content.contains("void-message context"));
    assert!(content.contains("void-message inbox"));
    assert!(content.contains("void-message send"));
    assert!(content.contains("proposal"));
    assert!(content.contains("signal"));
    assert!(content.contains("evaluation"));
    assert!(content.contains("broadcast"));
    assert!(content.contains("leader"));
    // Should NOT contain raw HTTP instructions
    assert!(!content.contains("curl"));
    assert!(!content.contains("wget"));
    assert!(!content.contains("http://10.0.2.2"));
}

#[test]
fn inline_skill_constructs_correctly() {
    let skill = void_box::skill::Skill::inline("test-skill", "# Test\nHello world");
    assert_eq!(skill.name, "test-skill");
    match skill.kind {
        void_box::skill::SkillKind::Inline { ref content } => {
            assert!(content.contains("# Test"));
            assert!(content.contains("Hello world"));
        }
        _ => panic!("expected Inline kind"),
    }
}
