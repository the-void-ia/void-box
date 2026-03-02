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
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Start the daemon on a random port and return the address.
fn start_daemon() -> SocketAddr {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = std::net::TcpListener::bind(addr).unwrap();
    let local_addr = listener.local_addr().unwrap();
    drop(listener);

    let addr = local_addr;
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

    // Get all events
    let (_, all_events) = http_request(addr, "GET", "/v1/runs/resume-missing/events", "");
    let all_events = all_events.as_array().unwrap();

    // Resume with a non-existent event_id → should return all
    let (_, resumed) = http_request(
        addr,
        "GET",
        "/v1/runs/resume-missing/events?from_event_id=nonexistent-id",
        "",
    );
    let resumed = resumed.as_array().unwrap();
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
