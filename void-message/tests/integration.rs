//! Integration tests: run void-message CLI against a real sidecar.
//!
//! These tests start a sidecar, build the void-message binary, and invoke
//! it as a subprocess with VOID_SIDECAR_URL set.

use std::process::Command;

fn void_message_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    // Navigate from target/debug/deps/integration-xxx to target/debug/void-message
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("void-message");
    path.to_string_lossy().to_string()
}

fn build_binary() {
    let bin = void_message_bin();
    if std::path::Path::new(&bin).exists() {
        return;
    }
    let status = Command::new("cargo")
        .args(["build", "-p", "void-message"])
        .status()
        .expect("failed to build void-message");
    assert!(status.success(), "cargo build failed");
}

fn run_void_message(sidecar_url: &str, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(void_message_bin())
        .args(args)
        .env("VOID_SIDECAR_URL", sidecar_url)
        .output()
        .expect("failed to run void-message");

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (code, stdout, stderr)
}

fn start_sidecar() -> (
    String,
    tokio::runtime::Runtime,
    void_box::sidecar::SidecarHandle,
) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = rt.block_on(async {
        void_box::sidecar::start_sidecar(
            "run-cli-test",
            "exec-cli-test",
            "c-1",
            vec!["c-2".into()],
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .expect("failed to start sidecar")
    });
    let url = format!("http://127.0.0.1:{}", handle.addr().port());
    (url, rt, handle)
}

#[test]
fn cli_health() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    let (code, stdout, stderr) = run_void_message(&url, &["health"]);
    assert_eq!(code, 0, "health failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["status"], "ok");

    rt.block_on(handle.stop());
}

#[test]
fn cli_context() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    let (code, stdout, stderr) = run_void_message(&url, &["context"]);
    assert_eq!(code, 0, "context failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["candidate_id"], "c-1");
    assert_eq!(parsed["peers"].as_array().unwrap().len(), 1);

    rt.block_on(handle.stop());
}

#[test]
fn cli_inbox_empty() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    let (code, stdout, stderr) = run_void_message(&url, &["inbox"]);
    assert_eq!(code, 0, "inbox failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["version"], 0);

    rt.block_on(handle.stop());
}

#[test]
fn cli_inbox_with_messages() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-cli-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![void_box::sidecar::InboxEntry {
            message_id: "msg-1".into(),
            from_candidate_id: "c-2".into(),
            kind: "proposal".into(),
            payload: serde_json::json!({"summary_text": "use approach A"}),
        }],
    }));

    let (code, stdout, stderr) = run_void_message(&url, &["inbox"]);
    assert_eq!(code, 0, "inbox failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["entries"].as_array().unwrap().len(), 1);

    rt.block_on(handle.stop());
}

#[test]
fn cli_inbox_since() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(void_box::sidecar::InboxSnapshot {
        version: 3,
        execution_id: "exec-cli-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![void_box::sidecar::InboxEntry {
            message_id: "msg-1".into(),
            from_candidate_id: "c-2".into(),
            kind: "signal".into(),
            payload: serde_json::json!({"summary_text": "hello"}),
        }],
    }));

    // --since 3 should return no entries
    let (code, stdout, stderr) = run_void_message(&url, &["inbox", "--since", "3"]);
    assert_eq!(code, 0, "inbox --since failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed["entries"].as_array().unwrap().is_empty());

    rt.block_on(handle.stop());
}

#[test]
fn cli_send_intent() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    // Load inbox first (sets iteration)
    rt.block_on(handle.load_inbox(void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-cli-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    }));

    let (code, stdout, stderr) = run_void_message(
        &url,
        &[
            "send",
            "--kind",
            "signal",
            "--audience",
            "broadcast",
            "--summary",
            "cache misses dominate p99",
        ],
    );
    assert_eq!(code, 0, "send failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["kind"], "signal");
    assert_eq!(parsed["audience"], "broadcast");
    assert_eq!(parsed["iteration"], 1);

    // Verify sidecar received it
    let drained = rt.block_on(handle.drain_intents());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind, "signal");

    rt.block_on(handle.stop());
}

#[test]
fn cli_send_with_priority() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(void_box::sidecar::InboxSnapshot {
        version: 1,
        execution_id: "exec-cli-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    }));

    let (code, stdout, stderr) = run_void_message(
        &url,
        &[
            "send",
            "--kind",
            "proposal",
            "--audience",
            "leader",
            "--summary",
            "promote cache-aware variant",
            "--priority",
            "high",
        ],
    );
    assert_eq!(code, 0, "send with priority failed: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["priority"], "high");

    rt.block_on(handle.stop());
}

#[test]
fn cli_send_missing_required_args() {
    build_binary();
    let (url, _rt, _handle) = start_sidecar();

    // Missing --kind
    let (code, _, _) = run_void_message(
        &url,
        &["send", "--audience", "broadcast", "--summary", "test"],
    );
    assert_ne!(code, 0);

    // Missing --audience
    let (code, _, _) = run_void_message(&url, &["send", "--kind", "signal", "--summary", "test"]);
    assert_ne!(code, 0);

    // Missing --summary
    let (code, _, _) = run_void_message(
        &url,
        &["send", "--kind", "signal", "--audience", "broadcast"],
    );
    assert_ne!(code, 0);
}

#[test]
fn cli_send_invalid_kind() {
    build_binary();
    let (url, _rt, _handle) = start_sidecar();

    let (code, _, stderr) = run_void_message(
        &url,
        &[
            "send",
            "--kind",
            "invalid",
            "--audience",
            "broadcast",
            "--summary",
            "test",
        ],
    );
    assert_ne!(code, 0);
    assert!(stderr.contains("invalid kind"));
}

#[test]
fn cli_no_sidecar_url() {
    build_binary();
    let output = Command::new(void_message_bin())
        .args(["health"])
        .env_remove("VOID_SIDECAR_URL")
        .output()
        .expect("failed to run void-message");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("VOID_SIDECAR_URL"));
}

#[test]
fn cli_unknown_command() {
    build_binary();
    let output = Command::new(void_message_bin())
        .args(["bogus"])
        .env("VOID_SIDECAR_URL", "http://127.0.0.1:1")
        .output()
        .expect("failed to run void-message");
    assert!(!output.status.success());
}
