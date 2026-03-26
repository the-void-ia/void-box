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
    path.pop(); // debug/ (or release/)
    path.push("void-mcp");
    path.to_string_lossy().to_string()
}

fn build_binary() {
    let status = Command::new("cargo")
        .args(["build", "-p", "void-mcp"])
        .status()
        .expect("failed to run cargo build");
    assert!(status.success(), "cargo build -p void-mcp failed");
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
        Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
        }
    }

    fn send(&mut self, json: &str) -> String {
        // Write with Content-Length header
        let msg = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
        self.stdin.write_all(msg.as_bytes()).unwrap();
        self.stdin.flush().unwrap();

        // Read Content-Length header (skip any unrelated lines)
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

        // Read blank line after header
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: initialize handshake + tools/list returns 3 tools.
#[test]
fn mcp_initialize_and_list_tools() {
    build_binary();
    let (url, rt, handle) = start_sidecar();
    let mut sess = McpSession::start(&url);

    // initialize
    let init_json = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let init_resp = sess.send_parsed(init_json);
    assert_eq!(init_resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "void-mcp");

    // tools/list
    let list_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    let list_resp = sess.send_parsed(list_json);
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 3);

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"get_context"));
    assert!(names.contains(&"read_inbox"));
    assert!(names.contains(&"send_message"));

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 2: tools/call get_context returns candidate_id.
#[test]
fn mcp_get_context() {
    build_binary();
    let (url, rt, handle) = start_sidecar();
    let mut sess = McpSession::start(&url);

    // initialize first (required by MCP protocol)
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_context","arguments":{}}}"#;
    let resp = sess.send_parsed(call_json);

    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let ctx: serde_json::Value = serde_json::from_str(text).expect("context JSON");
    assert_eq!(ctx["candidate_id"], "c-1");
    assert_eq!(ctx["execution_id"], "exec-mcp-test");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 3: load inbox then read_inbox returns entries.
#[test]
fn mcp_read_inbox() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    // Load inbox with one entry before spawning MCP session
    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 1,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 0,
        entries: vec![InboxEntry {
            message_id: "msg-001".into(),
            from_candidate_id: "c-2".into(),
            kind: "signal".into(),
            payload: serde_json::json!({"summary_text": "hello from c-2"}),
        }],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_inbox","arguments":{}}}"#;
    let resp = sess.send_parsed(call_json);

    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let inbox: serde_json::Value = serde_json::from_str(text).expect("inbox JSON");
    let entries = inbox["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["message_id"], "msg-001");
    assert_eq!(entries[0]["from_candidate_id"], "c-2");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 4: read_inbox with since=version returns empty entries when already at that version.
#[test]
fn mcp_read_inbox_since() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    // Load inbox at version 3
    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 3,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 1,
        entries: vec![InboxEntry {
            message_id: "msg-002".into(),
            from_candidate_id: "c-2".into(),
            kind: "proposal".into(),
            payload: serde_json::json!({"summary_text": "use approach B"}),
        }],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    // since=3 means we only want messages after version 3 → empty
    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_inbox","arguments":{"since":3}}}"#;
    let resp = sess.send_parsed(call_json);

    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let inbox: serde_json::Value = serde_json::from_str(text).expect("inbox JSON");
    let entries = inbox["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 0, "since=version should yield empty entries");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 5: send_message causes the sidecar to buffer an intent.
#[test]
fn mcp_send_message() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    // Load inbox first so iteration is set
    rt.block_on(handle.load_inbox(InboxSnapshot {
        version: 1,
        execution_id: "exec-mcp-test".into(),
        candidate_id: "c-1".into(),
        iteration: 2,
        entries: vec![],
    }));

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_message","arguments":{"kind":"signal","audience":"broadcast","summary_text":"all done"}}}"#;
    let resp = sess.send_parsed(call_json);

    // Should not be an error
    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "unexpected isError: {resp}"
    );

    // Verify the sidecar received the intent
    let intents = rt.block_on(handle.drain_intents());
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, "signal");
    assert_eq!(intents[0].audience, "broadcast");
    assert_eq!(intents[0].priority, "normal");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 6: send_message with priority high is reflected in the stored intent.
#[test]
fn mcp_send_with_priority() {
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

    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_message","arguments":{"kind":"proposal","audience":"leader","summary_text":"urgent proposal","priority":"high"}}}"#;
    let resp = sess.send_parsed(call_json);

    assert!(
        resp["result"]["isError"].is_null() || resp["result"]["isError"] == false,
        "unexpected isError: {resp}"
    );

    let intents = rt.block_on(handle.drain_intents());
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].priority, "high");
    assert_eq!(intents[0].kind, "proposal");

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 7: send_message missing required field `kind` returns isError: true.
#[test]
fn mcp_send_missing_field() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    // Missing `kind`
    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_message","arguments":{"audience":"broadcast","summary_text":"no kind"}}}"#;
    let resp = sess.send_parsed(call_json);

    assert_eq!(
        resp["result"]["isError"], true,
        "expected isError true, got: {resp}"
    );

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 8: calling an unknown tool name returns isError: true.
#[test]
fn mcp_unknown_tool() {
    build_binary();
    let (url, rt, handle) = start_sidecar();

    let mut sess = McpSession::start(&url);
    sess.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    let call_json = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"totally_unknown","arguments":{}}}"#;
    let resp = sess.send_parsed(call_json);

    assert_eq!(
        resp["result"]["isError"], true,
        "expected isError true for unknown tool, got: {resp}"
    );

    sess.stop();
    rt.block_on(handle.stop());
}

/// Test 9: void-mcp exits with non-zero when VOID_SIDECAR_URL is not set.
#[test]
fn mcp_no_sidecar_url() {
    build_binary();

    let mut child = Command::new(void_mcp_bin())
        // explicitly remove the env var
        .env_remove("VOID_SIDECAR_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn void-mcp");

    // Close stdin so the process can terminate
    drop(child.stdin.take());

    let status = child.wait().expect("failed to wait");
    assert!(
        !status.success(),
        "expected non-zero exit when VOID_SIDECAR_URL is unset, got: {status}"
    );
}
