//! Orchestration contract tests for void-control integration.
//!
//! These tests validate the HTTP API contract required by void-control:
//! - Structured error responses (`{code, message, retryable}`)
//! - Event envelope schema compliance (`event_id`, `seq`, `attempt_id`, `timestamp`)
//! - State transitions: Running → Succeeded / Failed / Cancelled
//! - Start idempotency (active → return existing, terminal → ALREADY_TERMINAL)
//! - Cancel idempotency (terminal → 200 OK)
//! - List runs with `?state=active` / `?state=terminal` filter
//! - `from_event_id` resume semantics
//! - Policy validation rejects invalid values
//! - `Succeeded` status serializes correctly
//! - Backward compatibility flag (`?api_version=v2`)

use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[path = "common/net.rs"]
mod test_net;

/// Start the daemon on a random port and return the address.
fn start_daemon() -> SocketAddr {
    let addr = test_net::reserve_localhost_addr();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Set state dir to temp to avoid polluting real state
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("VOIDBOX_STATE_DIR", dir.path());
            let _ = void_box::daemon::serve(addr).await;
        });
    });

    // Wait for daemon to start
    for _ in 0..50 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            return addr;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not start within timeout");
}

fn http_request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);

    let status_line = response.lines().next().unwrap_or("");
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let body_str = response.split("\r\n\r\n").nth(1).unwrap_or("{}");

    let json: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
    (status_code, json)
}

#[allow(dead_code)]
fn http_request_raw(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);

    let response_str = String::from_utf8_lossy(&response);
    let status_line = response_str.lines().next().unwrap_or("");
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    // Extract body bytes after \r\n\r\n
    let body_bytes = if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
        response[pos + 4..].to_vec()
    } else {
        Vec::new()
    };
    (status_code, body_bytes)
}

fn wait_until_terminal(addr: SocketAddr, run_id: &str, timeout_ms: u64) -> Value {
    let attempts = timeout_ms / 50;
    for _ in 0..attempts {
        let (_, run) = http_request(addr, "GET", &format!("/v1/runs/{run_id}"), "");
        let status = run
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            status.as_str(),
            "succeeded" | "failed" | "cancelled" | "canceled"
        ) {
            return run;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("run '{run_id}' did not reach terminal state within {timeout_ms}ms");
}

fn write_temp_spec(name: &str, yaml: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "void-box-orch-contract-{name}-{}.yaml",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(&path, yaml).unwrap();
    path.to_string_lossy().to_string()
}

// ==========================================================================
// Error response format
// ==========================================================================

#[test]
fn error_response_has_code_message_retryable() {
    let addr = start_daemon();
    let (status, body) = http_request(addr, "GET", "/v1/nonexistent", "");
    assert_eq!(status, 404);
    assert!(body.get("code").is_some(), "missing 'code' field: {body}");
    assert!(
        body.get("message").is_some(),
        "missing 'message' field: {body}"
    );
    assert!(
        body.get("retryable").is_some(),
        "missing 'retryable' field: {body}"
    );
    assert_eq!(body["code"], "NOT_FOUND");
    assert_eq!(body["retryable"], false);
}

#[test]
fn error_response_invalid_json_returns_invalid_spec() {
    let addr = start_daemon();
    let (status, body) = http_request(addr, "POST", "/v1/runs", "not json");
    assert_eq!(status, 400);
    assert_eq!(body["code"], "INVALID_SPEC");
    assert_eq!(body["retryable"], false);
}

// ==========================================================================
// Create run → enriched response
// ==========================================================================

#[test]
fn create_run_returns_enriched_response() {
    let addr = start_daemon();
    let (status, body) = http_request(addr, "POST", "/v1/runs", r#"{"file":"nonexistent.yaml"}"#);
    assert_eq!(status, 200);
    assert!(body["run_id"].is_string(), "missing run_id: {body}");
    assert_eq!(body["attempt_id"], 1);
    assert_eq!(body["state"], "running");
}

#[test]
fn create_run_with_caller_run_id() {
    let addr = start_daemon();
    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"my-custom-run-123"}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(body["run_id"], "my-custom-run-123");
}

// ==========================================================================
// Event envelope schema
// ==========================================================================

#[test]
fn events_have_required_envelope_fields() {
    let addr = start_daemon();

    // Create a run
    let (_, create_resp) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"evt-schema-test"}"#,
    );
    let run_id = create_resp["run_id"].as_str().unwrap();

    // Get events
    let (status, events) = http_request(addr, "GET", &format!("/v1/runs/{run_id}/events"), "");
    assert_eq!(status, 200);
    let events = events.as_array().expect("events should be an array");
    assert!(!events.is_empty(), "should have at least one event");

    for (i, event) in events.iter().enumerate() {
        assert!(
            event["event_id"].is_string(),
            "event {i} missing event_id: {event}"
        );
        assert!(event["seq"].is_number(), "event {i} missing seq: {event}");
        assert!(
            event["attempt_id"].is_number(),
            "event {i} missing attempt_id: {event}"
        );
        assert!(
            event["timestamp"].is_string(),
            "event {i} missing timestamp: {event}"
        );
        assert!(
            event["event_type_v2"].is_string(),
            "event {i} missing event_type_v2: {event}"
        );
    }

    // Verify seq is strictly increasing
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    for i in 1..seqs.len() {
        assert!(
            seqs[i] > seqs[i - 1],
            "seq should be strictly increasing: {:?}",
            seqs
        );
    }
}

// ==========================================================================
// State transitions: Running → Cancelled
// ==========================================================================

#[test]
fn state_transition_running_to_cancelled() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"cancel-test"}"#,
    );

    // Cancel — the background task may have already failed (nonexistent file),
    // so the run might be Failed instead of Running when cancel arrives.
    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs/cancel-test/cancel",
        r#"{"reason":"user requested"}"#,
    );
    assert_eq!(status, 200);

    // The state should be terminal (either "cancelled" if cancel beat the failure,
    // or "failed" if the background task completed first — cancel is idempotent for terminal).
    let state = body["state"].as_str().unwrap();
    assert!(
        state == "cancelled" || state == "failed",
        "expected terminal state, got: {state}"
    );

    // Inspect to verify the run is in a terminal state
    let (_, run) = http_request(addr, "GET", "/v1/runs/cancel-test", "");
    let run_status = run["status"].as_str().unwrap();
    assert!(
        run_status == "cancelled" || run_status == "failed",
        "expected terminal status, got: {run_status}"
    );
    // If cancel won the race, terminal_reason should be set
    if run_status == "cancelled" {
        assert_eq!(run["terminal_reason"], "user requested");
    }
}

// ==========================================================================
// Succeeded status serialization
// ==========================================================================

#[test]
fn succeeded_status_serializes_correctly() {
    // Test RunStatus serialization directly
    let status = void_box::persistence::RunStatus::Succeeded;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, r#""succeeded""#);
}

#[test]
fn completed_alias_deserializes_to_succeeded() {
    let status: void_box::persistence::RunStatus = serde_json::from_str(r#""completed""#).unwrap();
    assert_eq!(status, void_box::persistence::RunStatus::Succeeded);
}

// ==========================================================================
// Start idempotency
// ==========================================================================

#[test]
fn start_idempotent_active_run_returns_existing_or_terminal() {
    let addr = start_daemon();

    // Create first run
    let (status1, body1) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"idempotent-active"}"#,
    );
    assert_eq!(status1, 200);

    // Create again with same run_id.
    // If the run is still active → 200 (idempotent, return existing).
    // If the background task already failed (nonexistent file) → 409 ALREADY_TERMINAL.
    let (status2, body2) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"idempotent-active"}"#,
    );
    assert!(
        status2 == 200 || status2 == 409,
        "expected 200 (idempotent) or 409 (terminal), got: {status2}"
    );
    if status2 == 200 {
        assert_eq!(body1["run_id"], body2["run_id"]);
    } else {
        assert_eq!(body2["code"], "ALREADY_TERMINAL");
    }
}

/// Direct model-level test for start idempotency (no race condition).
#[test]
fn start_idempotent_active_model_level() {
    let status = void_box::persistence::RunStatus::Running;
    assert!(status.is_active());
    assert!(!status.is_terminal());

    let status = void_box::persistence::RunStatus::Succeeded;
    assert!(status.is_terminal());
    assert!(!status.is_active());

    let status = void_box::persistence::RunStatus::Pending;
    assert!(status.is_active());
}

#[test]
fn start_idempotent_terminal_run_returns_already_terminal() {
    let addr = start_daemon();

    // Create a run and cancel it
    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"idempotent-terminal"}"#,
    );
    let (_, _) = http_request(addr, "POST", "/v1/runs/idempotent-terminal/cancel", "{}");

    // Try to create again → should get ALREADY_TERMINAL
    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"idempotent-terminal"}"#,
    );
    assert_eq!(status, 409);
    assert_eq!(body["code"], "ALREADY_TERMINAL");
}

// ==========================================================================
// Cancel idempotency
// ==========================================================================

#[test]
fn cancel_idempotent_terminal_returns_200() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"cancel-idemp"}"#,
    );

    // Cancel once
    let (status1, body1) = http_request(addr, "POST", "/v1/runs/cancel-idemp/cancel", "{}");
    assert_eq!(status1, 200);

    // Cancel again → should still be 200 (idempotent for terminal runs)
    let (status2, body2) = http_request(addr, "POST", "/v1/runs/cancel-idemp/cancel", "{}");
    assert_eq!(status2, 200);
    // State should be terminal (either cancelled or failed depending on race with background task)
    let state = body2["state"].as_str().unwrap();
    assert!(
        state == "cancelled" || state == "failed",
        "expected terminal state on second cancel, got: {state}"
    );

    // Stable terminal_event_id: repeated cancels must return the same value
    let tid1 = body1["terminal_event_id"].as_str();
    let tid2 = body2["terminal_event_id"].as_str();
    assert!(tid1.is_some(), "first cancel missing terminal_event_id");
    assert_eq!(
        tid1, tid2,
        "terminal_event_id must be stable across repeated cancels: {tid1:?} vs {tid2:?}"
    );
}

// ==========================================================================
// List runs with state filter
// ==========================================================================

#[test]
fn list_runs_with_state_filter() {
    let addr = start_daemon();

    // Create an active run
    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"list-active-1"}"#,
    );

    // Create and cancel a run
    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"list-terminal-1"}"#,
    );
    let (_, _) = http_request(addr, "POST", "/v1/runs/list-terminal-1/cancel", "{}");

    // List all
    let (status, body) = http_request(addr, "GET", "/v1/runs", "");
    assert_eq!(status, 200);
    let all_runs = body["runs"].as_array().unwrap();
    assert!(all_runs.len() >= 2);

    // List active only
    let (status, body) = http_request(addr, "GET", "/v1/runs?state=active", "");
    assert_eq!(status, 200);
    let active_runs = body["runs"].as_array().unwrap();
    for run in active_runs {
        let s = run["status"].as_str().unwrap();
        assert!(
            s == "running" || s == "pending" || s == "starting",
            "expected active status, got: {s}"
        );
    }

    // List terminal only
    let (status, body) = http_request(addr, "GET", "/v1/runs?state=terminal", "");
    assert_eq!(status, 200);
    let terminal_runs = body["runs"].as_array().unwrap();
    for run in terminal_runs {
        let s = run["status"].as_str().unwrap();
        assert!(
            s == "succeeded" || s == "failed" || s == "cancelled",
            "expected terminal status, got: {s}"
        );
    }
}

// ==========================================================================
// from_event_id resume
// ==========================================================================

#[test]
fn from_event_id_returns_subsequent_events() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"resume-test"}"#,
    );

    // Get all events
    let (_, all_events) = http_request(addr, "GET", "/v1/runs/resume-test/events", "");
    let all_events = all_events.as_array().unwrap();
    assert!(all_events.len() >= 2, "need at least 2 events");

    // Resume from the first event
    let first_event_id = all_events[0]["event_id"].as_str().unwrap();
    let (status, resumed) = http_request(
        addr,
        "GET",
        &format!("/v1/runs/resume-test/events?from_event_id={first_event_id}"),
        "",
    );
    assert_eq!(status, 200);
    let resumed = resumed.as_array().unwrap();
    assert_eq!(
        resumed.len(),
        all_events.len() - 1,
        "should return all events after the marker"
    );

    // Verify the first resumed event is the second overall event
    assert_eq!(resumed[0]["event_id"], all_events[1]["event_id"]);
}

#[test]
fn from_event_id_missing_marker_returns_all() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"resume-missing"}"#,
    );

    // Resume with a non-existent event_id → should return all
    let (_, resumed) = http_request(
        addr,
        "GET",
        "/v1/runs/resume-missing/events?from_event_id=nonexistent-id",
        "",
    );
    let resumed = resumed.as_array().unwrap();

    // Get all events (after resume call to avoid race with async event emission)
    let (_, all_events) = http_request(addr, "GET", "/v1/runs/resume-missing/events", "");
    let all_events = all_events.as_array().unwrap();
    assert_eq!(resumed.len(), all_events.len());
}

// ==========================================================================
// Policy validation
// ==========================================================================

#[test]
fn policy_validation_rejects_zero_max_parallel() {
    let addr = start_daemon();
    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","policy":{"max_parallel_microvms_per_run":0}}"#,
    );
    assert_eq!(status, 400);
    assert_eq!(body["code"], "INVALID_POLICY");
}

#[test]
fn policy_validation_rejects_zero_timeout() {
    let addr = start_daemon();
    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","policy":{"stage_timeout_secs":0}}"#,
    );
    assert_eq!(status, 400);
    assert_eq!(body["code"], "INVALID_POLICY");
}

#[test]
fn policy_valid_is_accepted_and_persisted() {
    let addr = start_daemon();
    let (status, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"policy-persist","policy":{"max_parallel_microvms_per_run":2,"max_stage_retries":5,"stage_timeout_secs":600,"cancel_grace_period_secs":10}}"#,
    );
    assert_eq!(status, 200);

    // Inspect the run → policy should be present
    let (_, run) = http_request(addr, "GET", "/v1/runs/policy-persist", "");
    assert_eq!(run["policy"]["max_parallel_microvms_per_run"], 2);
    assert_eq!(run["policy"]["max_stage_retries"], 5);
    assert_eq!(run["policy"]["stage_timeout_secs"], 600);
    assert_eq!(run["policy"]["cancel_grace_period_secs"], 10);
}

// ==========================================================================
// Backward compatibility: api_version=v2
// ==========================================================================

#[test]
fn api_v2_uses_pascal_case_event_types() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"v2-compat-test"}"#,
    );

    // Legacy mode: event_type should be dotted
    let (_, legacy_events) = http_request(addr, "GET", "/v1/runs/v2-compat-test/events", "");
    let legacy_events = legacy_events.as_array().unwrap();
    assert_eq!(legacy_events[0]["event_type"], "run.started");

    // v2 mode: event_type should be PascalCase
    let (_, v2_events) = http_request(
        addr,
        "GET",
        "/v1/runs/v2-compat-test/events?api_version=v2",
        "",
    );
    let v2_events = v2_events.as_array().unwrap();
    assert_eq!(v2_events[0]["event_type"], "RunStarted");
}

// ==========================================================================
// RunState enrichment fields
// ==========================================================================

#[test]
fn run_state_has_enrichment_fields() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"enrich-test"}"#,
    );

    let (_, run) = http_request(addr, "GET", "/v1/runs/enrich-test", "");
    assert_eq!(run["attempt_id"], 1);
    assert!(run["started_at"].is_string(), "missing started_at");
    assert!(run["updated_at"].is_string(), "missing updated_at");
}

// ==========================================================================
// Not-found run returns structured error
// ==========================================================================

#[test]
fn get_nonexistent_run_returns_not_found() {
    let addr = start_daemon();
    let (status, body) = http_request(addr, "GET", "/v1/runs/does-not-exist", "");
    assert_eq!(status, 404);
    assert_eq!(body["code"], "NOT_FOUND");
}

#[test]
fn cancel_nonexistent_run_returns_not_found() {
    let addr = start_daemon();
    let (status, body) = http_request(addr, "POST", "/v1/runs/does-not-exist/cancel", "{}");
    assert_eq!(status, 404);
    assert_eq!(body["code"], "NOT_FOUND");
}

#[test]
fn events_nonexistent_run_returns_not_found() {
    let addr = start_daemon();
    let (status, body) = http_request(addr, "GET", "/v1/runs/does-not-exist/events", "");
    assert_eq!(status, 404);
    assert_eq!(body["code"], "NOT_FOUND");
}

// ==========================================================================
// Artifact error codes serialize correctly
// ==========================================================================

#[test]
fn artifact_error_codes_serialize_correctly() {
    let codes = vec![
        (
            "STRUCTURED_OUTPUT_MISSING",
            void_box::error::ApiError::structured_output_missing("no result.json"),
        ),
        (
            "STRUCTURED_OUTPUT_MALFORMED",
            void_box::error::ApiError::structured_output_malformed("invalid JSON"),
        ),
        (
            "ARTIFACT_NOT_FOUND",
            void_box::error::ApiError::artifact_not_found("report.md"),
        ),
        (
            "ARTIFACT_PUBLICATION_INCOMPLETE",
            void_box::error::ApiError::artifact_publication_incomplete("still publishing"),
        ),
        (
            "ARTIFACT_STORE_UNAVAILABLE",
            void_box::error::ApiError::artifact_store_unavailable("disk full"),
        ),
        (
            "RETRIEVAL_TIMEOUT",
            void_box::error::ApiError::retrieval_timeout("timed out"),
        ),
    ];
    for (expected_code, err) in codes {
        let json: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
        assert_eq!(
            json["code"], expected_code,
            "wrong code for {expected_code}"
        );
        assert!(json["message"].is_string());
        assert!(json["retryable"].is_boolean());
    }
}

// ==========================================================================
// RunState artifact publication and stage_states fields
// ==========================================================================

#[test]
fn run_state_deserializes_with_artifact_publication() {
    let json = r#"{
        "id": "test-1",
        "status": "succeeded",
        "file": "test.yaml",
        "events": [],
        "artifact_publication": {
            "status": "published",
            "published_at": "2026-03-20T18:20:00Z",
            "manifest": [{
                "name": "result.json",
                "stage": "main",
                "media_type": "application/json",
                "size_bytes": 128,
                "retrieval_path": "/v1/runs/test-1/stages/main/output-file"
            }]
        },
        "stage_states": {
            "main": { "status": "succeeded", "started_at": "2026-03-20T18:19:00Z", "completed_at": "2026-03-20T18:20:00Z" }
        },
        "finished_at": "2026-03-20T18:20:00Z"
    }"#;
    let run: void_box::persistence::RunState = serde_json::from_str(json).unwrap();
    assert!(run.artifact_publication.is_some());
    let pub_status = run.artifact_publication.unwrap();
    assert_eq!(
        pub_status.status,
        void_box::persistence::ArtifactPublicationStatus::Published
    );
    assert_eq!(pub_status.manifest.len(), 1);
    assert_eq!(pub_status.manifest[0].name, "result.json");
    assert!(run.stage_states.is_some());
    assert!(run.finished_at.is_some());
}

#[test]
fn run_state_deserializes_without_new_fields() {
    // Backward compat: old RunState JSON without new fields still deserializes
    let json = r#"{
        "id": "old-1",
        "status": "running",
        "file": "test.yaml",
        "events": []
    }"#;
    let run: void_box::persistence::RunState = serde_json::from_str(json).unwrap();
    assert!(run.artifact_publication.is_none());
    assert!(run.stage_states.is_none());
    assert!(run.finished_at.is_none());
}

// ==========================================================================
// Named artifact retrieval
// ==========================================================================

#[test]
fn named_artifact_not_found_returns_typed_error() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"artifact-test"}"#,
    );

    // Try to get a named artifact that doesn't exist
    let (status, body) = http_request(
        addr,
        "GET",
        "/v1/runs/artifact-test/stages/main/artifacts/report.md",
        "",
    );
    assert_eq!(status, 404);
    assert_eq!(body["code"], "ARTIFACT_NOT_FOUND");
}

#[test]
fn named_artifact_run_not_found_returns_not_found() {
    let addr = start_daemon();
    let (status, body) = http_request(
        addr,
        "GET",
        "/v1/runs/no-such-run/stages/main/artifacts/report.md",
        "",
    );
    assert_eq!(status, 404);
    assert_eq!(body["code"], "NOT_FOUND");
}

// ==========================================================================
// Artifact publication status on inspection
// ==========================================================================

#[test]
fn run_inspection_has_artifact_publication_field() {
    let addr = start_daemon();

    // Create a run (will fail because file doesn't exist, but that's fine)
    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"pub-inspect"}"#,
    );

    // Wait briefly for background task to complete
    std::thread::sleep(Duration::from_millis(500));

    let (_, run) = http_request(addr, "GET", "/v1/runs/pub-inspect", "");
    // Should have artifact_publication (even if failed/not_started)
    assert!(
        run.get("artifact_publication").is_some(),
        "missing artifact_publication field: {run}"
    );
    let pub_status = run["artifact_publication"]["status"].as_str().unwrap();
    // A failed run with no output file → not_started
    assert_eq!(
        pub_status, "not_started",
        "expected not_started for failed run without output"
    );
}

// ==========================================================================
// Structured output validation
// ==========================================================================

#[test]
fn structured_output_missing_returns_typed_error() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"output-missing"}"#,
    );

    // Wait for run to complete (will fail because file doesn't exist)
    std::thread::sleep(Duration::from_millis(500));

    // Try to get output file for a stage that doesn't exist
    let (status, body) = http_request(
        addr,
        "GET",
        "/v1/runs/output-missing/stages/main/output-file",
        "",
    );
    // Run exists but has no output → STRUCTURED_OUTPUT_MISSING
    assert_eq!(status, 404);
    assert_eq!(body["code"], "STRUCTURED_OUTPUT_MISSING");
}

#[test]
fn structured_output_malformed_returns_typed_error() {
    // Validate that the error constructor produces the right code
    let err = void_box::error::ApiError::structured_output_malformed("test");
    let json: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
    assert_eq!(json["code"], "STRUCTURED_OUTPUT_MALFORMED");
    assert_eq!(json["retryable"], false);
}

#[test]
fn single_step_workflow_publishes_result_json_and_named_artifacts() {
    let addr = start_daemon();
    let spec = write_temp_spec(
        "structured-output-success",
        r#"api_version: v1
kind: workflow
name: structured-output-success

sandbox:
  mode: mock
  network: false

workflow:
  steps:
    - name: produce
      run:
        program: sh
        args:
          - -lc
          - |
            cat > /workspace/result.json <<'JSON'
            {"status":"success","summary":"ok","metrics":{"latency_p99_ms":87},"artifacts":[{"name":"report.md","media_type":"text/markdown"}]}
            JSON
            cat > /workspace/report.md <<'MD'
            # report
            artifact content
            MD
  output_step: produce
"#,
    );

    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        &format!(r#"{{"file":"{spec}","run_id":"workflow-structured-output"}}"#),
    );
    assert_eq!(status, 200, "body={body}");
    let run = wait_until_terminal(addr, "workflow-structured-output", 5_000);
    assert_eq!(run["status"], "succeeded", "run={run}");
    assert_eq!(
        run["artifact_publication"]["status"], "published",
        "run={run}"
    );
    let manifest = run["artifact_publication"]["manifest"]
        .as_array()
        .expect("manifest array");
    assert!(manifest.iter().any(|entry| entry["name"] == "result.json"));
    assert!(manifest.iter().any(|entry| entry["name"] == "report.md"));

    let (status_output, body_output) = http_request_raw(
        addr,
        "GET",
        "/v1/runs/workflow-structured-output/stages/produce/output-file",
        "",
    );
    assert_eq!(
        status_output,
        200,
        "body={}",
        String::from_utf8_lossy(&body_output)
    );

    let (status_named, body_named) = http_request_raw(
        addr,
        "GET",
        "/v1/runs/workflow-structured-output/stages/produce/artifacts/report.md",
        "",
    );
    assert_eq!(
        status_named,
        200,
        "body={}",
        String::from_utf8_lossy(&body_named)
    );
    assert!(String::from_utf8_lossy(&body_named).contains("artifact content"));
}

#[test]
fn malformed_result_json_preserves_raw_artifact() {
    let addr = start_daemon();
    let spec = write_temp_spec(
        "structured-output-malformed",
        r#"api_version: v1
kind: workflow
name: structured-output-malformed

sandbox:
  mode: mock
  network: false

workflow:
  steps:
    - name: produce
      run:
        program: sh
        args:
          - -lc
          - |
            cat > /workspace/result.json <<'JSON'
            {"summary":"missing status field on purpose"}
            JSON
  output_step: produce
"#,
    );

    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        &format!(r#"{{"file":"{spec}","run_id":"workflow-malformed-output"}}"#),
    );
    assert_eq!(status, 200, "body={body}");

    let run = wait_until_terminal(addr, "workflow-malformed-output", 5_000);

    assert_eq!(run["status"], "failed", "run={run}");
    assert_eq!(
        run["artifact_publication"]["status"], "failed",
        "publication should record failure: run={run}"
    );

    let manifest = run["artifact_publication"]["manifest"]
        .as_array()
        .expect("manifest array");
    assert!(
        manifest.iter().any(|entry| entry["name"] == "result.json"),
        "manifest should still list result.json: manifest={manifest:?}"
    );

    let (status_output, body_output) = http_request_raw(
        addr,
        "GET",
        "/v1/runs/workflow-malformed-output/stages/produce/output-file",
        "",
    );
    assert_eq!(
        status_output,
        200,
        "raw output-file should serve bytes even when validation failed: body={}",
        String::from_utf8_lossy(&body_output)
    );
    assert!(
        String::from_utf8_lossy(&body_output).contains("missing status field on purpose"),
        "body should contain the original raw payload: body={}",
        String::from_utf8_lossy(&body_output)
    );
}

#[test]
fn multi_step_workflow_without_result_json_still_succeeds() {
    let addr = start_daemon();
    let spec = write_temp_spec(
        "baseline-success",
        r#"api_version: v1
kind: workflow
name: baseline-success

sandbox:
  mode: mock
  network: false

workflow:
  steps:
    - name: fetch
      run:
        program: echo
        args: ["hello from workflow"]
    - name: transform
      depends_on: [fetch]
      run:
        program: tr
        args: ["a-z", "A-Z"]
        stdin_from: fetch
  output_step: transform
"#,
    );

    let (status, body) = http_request(
        addr,
        "POST",
        "/v1/runs",
        &format!(r#"{{"file":"{spec}","run_id":"baseline-success-no-artifact"}}"#),
    );
    assert_eq!(status, 200, "body={body}");
    let run = wait_until_terminal(addr, "baseline-success-no-artifact", 5_000);
    assert_eq!(run["status"], "succeeded", "run={run}");
}

// ==========================================================================
// Cancelled run has finished_at and stage_states
// ==========================================================================

#[test]
fn cancelled_run_has_finished_at_and_stage_states() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"cancel-fields"}"#,
    );

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs/cancel-fields/cancel",
        r#"{"reason":"test"}"#,
    );

    let (_, run) = http_request(addr, "GET", "/v1/runs/cancel-fields", "");
    assert!(
        run["finished_at"].is_string(),
        "cancelled run should have finished_at: {run}"
    );
    assert!(
        run.get("stage_states").is_some(),
        "cancelled run should have stage_states: {run}"
    );
    assert!(
        run.get("artifact_publication").is_some(),
        "cancelled run should have artifact_publication: {run}"
    );
}

// ==========================================================================
// Named artifact retrieval success
// ==========================================================================

#[test]
fn named_artifact_retrieval_success() {
    let addr = start_daemon();

    // Create a run
    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"artifact-success"}"#,
    );

    std::thread::sleep(Duration::from_millis(100));

    // Model-level: verify persistence round-trip for named artifacts
    let dir = tempfile::tempdir().unwrap();
    let provider = void_box::persistence::DiskPersistenceProvider::new(dir.path().to_path_buf());
    use void_box::persistence::PersistenceProvider;

    let artifact_data = br#"# My Report"#;
    provider
        .save_named_artifact("artifact-success", "main", "report.md", artifact_data)
        .unwrap();
    let loaded = provider
        .load_named_artifact("artifact-success", "main", "report.md")
        .unwrap();
    assert_eq!(loaded, Some(artifact_data.to_vec()));
}

// ==========================================================================
// Active-run listing for reconciliation
// ==========================================================================

#[test]
fn active_run_listing_has_reconciliation_fields() {
    let addr = start_daemon();

    let (_, _) = http_request(
        addr,
        "POST",
        "/v1/runs",
        r#"{"file":"nonexistent.yaml","run_id":"recon-test"}"#,
    );

    let (status, body) = http_request(addr, "GET", "/v1/runs?state=active", "");
    assert_eq!(status, 200);
    let runs = body["runs"].as_array().unwrap();

    // Find our run (it may have already finished, so check if we got it)
    if let Some(run) = runs.iter().find(|r| r["id"] == "recon-test") {
        // Spec requires these fields for reconciliation
        assert!(run["id"].is_string(), "missing id");
        assert!(run["attempt_id"].is_number(), "missing attempt_id");
        assert!(run["status"].is_string(), "missing status");
        assert!(run["started_at"].is_string(), "missing started_at");
        assert!(run["updated_at"].is_string(), "missing updated_at");
    }
    // If the run already completed (race), the test is still valid — we just
    // can't assert on it being in the active list.
}
