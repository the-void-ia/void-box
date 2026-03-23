//! HTTP integration tests for the sidecar guest-facing server.

use serde_json::{json, Value};
use void_box::sidecar::{start_sidecar, InboxEntry, InboxSnapshot};

/// Response data extracted in the blocking thread.
struct Resp {
    status: u16,
    body: Value,
}

/// Helper: perform a blocking GET from a spawned blocking thread.
async fn get(url: String) -> Resp {
    tokio::task::spawn_blocking(move || {
        let resp = reqwest::blocking::get(&url).unwrap();
        let status = resp.status().as_u16();
        let body: Value = resp.json().unwrap_or(Value::Null);
        Resp { status, body }
    })
    .await
    .unwrap()
}

/// Helper: perform a blocking POST with JSON body from a spawned blocking thread.
async fn post_json(url: String, body: Value) -> Resp {
    tokio::task::spawn_blocking(move || {
        let resp = reqwest::blocking::Client::new()
            .post(&url)
            .json(&body)
            .send()
            .unwrap();
        let status = resp.status().as_u16();
        let body: Value = resp.json().unwrap_or(Value::Null);
        Resp { status, body }
    })
    .await
    .unwrap()
}

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn sidecar_health_endpoint() {
    let handle = start_sidecar(
        "run-42",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let resp = get(format!("{}/v1/health", base_url(handle.addr().port()))).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["status"], "ok");
    assert_eq!(resp.body["run_id"], "run-42");

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_inbox_empty_before_load() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let resp = get(format!("{}/v1/inbox", base_url(handle.addr().port()))).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["version"], 0);
    assert_eq!(resp.body["entries"], json!([]));

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_context_endpoint() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec!["cand-2".into(), "cand-3".into()],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let resp = get(format!("{}/v1/context", base_url(handle.addr().port()))).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["candidate_id"], "cand-1");
    assert_eq!(resp.body["peers"], json!(["cand-2", "cand-3"]));

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_signals_returns_501() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let resp = get(format!("{}/v1/signals", base_url(handle.addr().port()))).await;
    assert_eq!(resp.status, 501);

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_post_intent_and_read_inbox_flow() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let port = handle.addr().port();

    // Load inbox via handle
    handle
        .load_inbox(InboxSnapshot {
            version: 1,
            execution_id: "exec-1".into(),
            candidate_id: "cand-1".into(),
            iteration: 1,
            entries: vec![InboxEntry {
                message_id: "msg-1".into(),
                from_candidate_id: "cand-2".into(),
                kind: "chat".into(),
                payload: json!({"text": "hello"}),
            }],
        })
        .await;

    // Read inbox via HTTP
    let resp = get(format!("{}/v1/inbox", base_url(port))).await;
    assert_eq!(resp.body["version"], 1);
    assert_eq!(resp.body["entries"].as_array().unwrap().len(), 1);

    // Post intent via HTTP
    let intent_body = json!({
        "kind": "message",
        "audience": "cand-2",
        "payload": {"text": "reply"},
        "priority": "normal"
    });
    let resp = post_json(format!("{}/v1/intents", base_url(port)), intent_body).await;
    assert_eq!(resp.status, 201);
    assert!(resp.body["intent_id"].is_string());
    assert_eq!(resp.body["from_candidate_id"], "cand-1");

    // Drain via handle
    let drained = handle.drain_intents().await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind, "message");

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_rejects_oversized_intent() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    handle
        .load_inbox(InboxSnapshot {
            version: 1,
            execution_id: "exec-1".into(),
            candidate_id: "cand-1".into(),
            iteration: 1,
            entries: vec![],
        })
        .await;

    let port = handle.addr().port();

    // Create a payload that exceeds 4096 bytes
    let big_text = "x".repeat(5000);
    let intent_body = json!({
        "kind": "message",
        "audience": "cand-2",
        "payload": {"text": big_text},
        "priority": "normal"
    });

    let resp = post_json(format!("{}/v1/intents", base_url(port)), intent_body).await;
    assert_eq!(resp.status, 413);

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_rejects_excess_intents() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    handle
        .load_inbox(InboxSnapshot {
            version: 1,
            execution_id: "exec-1".into(),
            candidate_id: "cand-1".into(),
            iteration: 1,
            entries: vec![],
        })
        .await;

    let port = handle.addr().port();

    // Post 3 intents successfully (max per iteration is 3)
    for i in 0..3 {
        let intent_body = json!({
            "kind": "message",
            "audience": format!("cand-{i}"),
            "payload": {"n": i},
            "priority": "normal"
        });
        let resp = post_json(format!("{}/v1/intents", base_url(port)), intent_body).await;
        assert_eq!(resp.status, 201, "intent {i} should succeed");
    }

    // 4th intent should be rejected
    let intent_body = json!({
        "kind": "message",
        "audience": "cand-99",
        "payload": {"n": 99},
        "priority": "normal"
    });
    let resp = post_json(format!("{}/v1/intents", base_url(port)), intent_body).await;
    assert_eq!(resp.status, 429);

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_incremental_inbox_query() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let port = handle.addr().port();

    handle
        .load_inbox(InboxSnapshot {
            version: 5,
            execution_id: "exec-1".into(),
            candidate_id: "cand-1".into(),
            iteration: 2,
            entries: vec![
                InboxEntry {
                    message_id: "m1".into(),
                    from_candidate_id: "cand-2".into(),
                    kind: "chat".into(),
                    payload: json!({"a": 1}),
                },
                InboxEntry {
                    message_id: "m2".into(),
                    from_candidate_id: "cand-3".into(),
                    kind: "chat".into(),
                    payload: json!({"a": 2}),
                },
            ],
        })
        .await;

    // Without since -- returns all entries
    let resp = get(format!("{}/v1/inbox", base_url(port))).await;
    assert_eq!(resp.body["entries"].as_array().unwrap().len(), 2);

    // With since >= version -- returns empty entries
    let resp = get(format!("{}/v1/inbox?since=5", base_url(port))).await;
    assert_eq!(resp.body["entries"].as_array().unwrap().len(), 0);

    // With since < version -- returns all entries
    let resp = get(format!("{}/v1/inbox?since=3", base_url(port))).await;
    assert_eq!(resp.body["entries"].as_array().unwrap().len(), 2);

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_batch_intent_submission() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    handle
        .load_inbox(InboxSnapshot {
            version: 1,
            execution_id: "exec-1".into(),
            candidate_id: "cand-1".into(),
            iteration: 1,
            entries: vec![],
        })
        .await;

    let port = handle.addr().port();

    let batch = json!([
        {"kind": "message", "audience": "cand-2", "payload": {"n": 1}, "priority": "normal"},
        {"kind": "message", "audience": "cand-3", "payload": {"n": 2}, "priority": "normal"}
    ]);

    let resp = post_json(format!("{}/v1/intents", base_url(port)), batch).await;
    assert_eq!(resp.status, 201);

    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr[0]["intent_id"].is_string());
    assert!(arr[1]["intent_id"].is_string());

    handle.stop().await;
}

#[tokio::test]
async fn sidecar_unknown_route_returns_404() {
    let handle = start_sidecar(
        "run-1",
        "exec-1",
        "cand-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let resp = get(format!("{}/v1/unknown", base_url(handle.addr().port()))).await;
    assert_eq!(resp.status, 404);

    handle.stop().await;
}
