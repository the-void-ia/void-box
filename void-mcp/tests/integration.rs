//! Integration tests for void-mcp.
//!
//! Spawns the `void-mcp` binary as a subprocess against a real sidecar,
//! communicating via Content-Length-framed JSON-RPC over stdin/stdout.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use void_box::sidecar::{InboxEntry, InboxSnapshot};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn void_mcp_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("void-mcp");
    path.to_string_lossy().to_string()
}

fn build_binary() {
    let status = Command::new("cargo")
        .args(["build", "-p", "void-mcp"])
        .status()
        .expect("failed to build void-mcp");
    assert!(status.success(), "cargo build -p void-mcp failed");
}

struct McpSession {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn start(sidecar_url: &str) -> Self {
        let mut child = Command::new(void_mcp_bin())
            .env("VOID_SIDECAR_URL", sidecar_url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn void-mcp");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        Self {
            child,
            stdin,
            reader,
        }
    }

    fn send(&mut self, json: &str) -> String {
        let msg = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
        self.stdin.write_all(msg.as_bytes()).unwrap();
        self.stdin.flush().unwrap();

        // Read Content-Length header
        let mut header_line = String::new();
        loop {
            header_line.clear();
            self.reader.read_line(&mut header_line).unwrap();
            if header_line.trim().starts_with("Content-Length:") {
                break;
            }
        }
        let content_length: usize = header_line
            .trim()
            .strip_prefix("Content-Length:")
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        // Read blank line
        let mut blank = String::new();
        self.reader.read_line(&mut blank).unwrap();

        // Read body
        let mut body = vec![0u8; content_length];
        self.reader.read_exact(&mut body).unwrap();
        String::from_utf8(body).unwrap()
    }

    fn send_parsed(&mut self, json: &str) -> serde_json::Value {
        let resp = self.send(json);
        serde_json::from_str(&resp).unwrap_or_else(|e| panic!("parse failed: {e}\nraw: {resp}"))
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_sidecar() -> (
    String,
    tokio::runtime::Runtime,
    void_box::sidecar::SidecarHandle,
) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = rt.block_on(async {
        void_box::sidecar::start_sidecar(
            "run-mcp-test",
            "exec-mcp-test",
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: initialize + tools/list returns 4 action-oriented tools.
#[test]
fn mcp_initialize_and_list_tools() {
    build_binary();
    let (url, rt, handle) = start_sidecar();
    let mut sess = McpSession::start(&url);

    let init = sess.send_parsed(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    assert_eq!(init["result"]["serverInfo"]["name"], "void-mcp");

    let list = sess.send_parsed(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 4);

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"read_shared_context"));
    assert!(names.contains(&"read_peer_messages"));
    assert!(names.contains(&"broadcast_observation"));
    assert!(names.contains(&"recommend_to_leader"));

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 2: read_shared_context returns candidate identity.
#[test]
fn mcp_read_shared_context() {
    build_binary();
    let (url, rt, handle) = start_sidecar();
    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_shared_context","arguments":{}}}"#,
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let ctx: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(ctx["candidate_id"], "c-1");
    assert_eq!(ctx["execution_id"], "exec-mcp-test");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 3: read_peer_messages returns inbox entries.
#[test]
fn mcp_read_peer_messages() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 1,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![InboxEntry {
            message_id: "msg-001".into(),
            from_candidate_id: "c-2".into(),
            kind: "signal".into(),
            payload: serde_json::json!({"summary_text": "hello from c-2"}),
        }],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_peer_messages","arguments":{}}}"#,
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let inbox: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(inbox["entries"].as_array().unwrap().len(), 1);

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 4: read_peer_messages with since returns filtered results.
#[test]
fn mcp_read_peer_messages_since() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 3,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![InboxEntry {
            message_id: "msg-002".into(),
            from_candidate_id: "c-2".into(),
            kind: "proposal".into(),
            payload: serde_json::json!({"summary_text": "approach B"}),
        }],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_peer_messages","arguments":{"since":3}}}"#,
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let inbox: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(inbox["entries"].as_array().unwrap().is_empty());

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 5: broadcast_observation sends a signal to broadcast.
#[test]
fn mcp_broadcast_observation() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 1,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 2,
        entries: vec![],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"broadcast_observation","arguments":{"summary_text":"cache misses dominate p99"}}}"#,
    );
    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "unexpected error: {resp}"
    );

    let intents = rt.block_on(handle.drain_intents());
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, "signal");
    assert_eq!(intents[0].audience, "broadcast");
    assert_eq!(intents[0].priority, "normal");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 6: recommend_to_leader sends a proposal to leader.
#[test]
fn mcp_recommend_to_leader() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 1,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recommend_to_leader","arguments":{"summary_text":"promote cache-aware variant","priority":"high"}}}"#,
    );
    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "unexpected error: {resp}"
    );

    let intents = rt.block_on(handle.drain_intents());
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, "proposal");
    assert_eq!(intents[0].audience, "leader");
    assert_eq!(intents[0].priority, "high");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 7: recommend_to_leader with disposition=reject maps to evaluation kind.
#[test]
fn mcp_recommend_to_leader_reject() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 1,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recommend_to_leader","arguments":{"summary_text":"approach has fatal flaw","disposition":"reject"}}}"#,
    );
    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "unexpected error: {resp}"
    );

    let intents = rt.block_on(handle.drain_intents());
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, "evaluation");
    assert_eq!(intents[0].audience, "leader");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 8: broadcast_observation missing summary_text returns isError.
#[test]
fn mcp_broadcast_missing_summary() {
    build_binary();
    let (url, rt, handle) = start_sidecar();
    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"broadcast_observation","arguments":{}}}"#,
    );
    assert_eq!(resp["result"]["isError"], true);

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 9: unknown tool returns isError.
#[test]
fn mcp_unknown_tool() {
    build_binary();
    let (url, rt, handle) = start_sidecar();
    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let resp = sess.send_parsed(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_message","arguments":{}}}"#,
    );
    // Old tool name should NOT work
    assert_eq!(resp["result"]["isError"], true);

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 10: no VOID_SIDECAR_URL exits non-zero.
#[test]
fn mcp_no_sidecar_url() {
    build_binary();
    let mut child = Command::new(void_mcp_bin())
        .env_remove("VOID_SIDECAR_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn");
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert!(!status.success());
}
