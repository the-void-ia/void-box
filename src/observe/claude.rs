//! Claude-Code Output Instrumentation
//!
//! Parses the JSONL output from `claude-code --output-format stream-json` and
//! extracts structured telemetry: tool calls, token usage, cost, model info.
//!
//! When the `opentelemetry` feature is enabled, creates OTel child spans for
//! each tool call under a parent `claude.exec` span following the
//! [OTel Semantic Conventions for GenAI](https://opentelemetry.io/docs/specs/semconv/gen-ai/).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Parsed result of a claude-code execution via `--output-format stream-json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeExecResult {
    /// Final text answer (from the `result` event).
    pub result_text: String,
    /// Model used (e.g. "sonnet", "opus").
    pub model: String,
    /// Session ID from claude-code.
    pub session_id: String,
    /// Total cost in USD.
    pub total_cost_usd: f64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// API-only duration in milliseconds.
    pub duration_api_ms: u64,
    /// Number of conversation turns.
    pub num_turns: u32,
    /// Total input tokens consumed.
    pub input_tokens: u64,
    /// Total output tokens produced.
    pub output_tokens: u64,
    /// Whether the execution ended in error.
    pub is_error: bool,
    /// Error message (if `is_error` is true).
    pub error: Option<String>,
    /// Tool calls made during the session, in order.
    pub tool_calls: Vec<ClaudeToolCall>,
}

/// A single tool call made by claude-code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeToolCall {
    /// Tool name (e.g. "Bash", "Read", "Write").
    pub tool_name: String,
    /// Tool use ID (e.g. "toolu_1").
    pub tool_use_id: String,
    /// Tool input arguments.
    pub input: serde_json::Value,
    /// Tool result/output (if captured).
    pub output: Option<String>,
}

impl ClaudeToolCall {
    /// Short human-readable summary of what this tool call does.
    pub fn tool_summary(&self) -> String {
        match self.tool_name.as_str() {
            "Bash" => {
                let cmd = self
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                truncate(cmd, 80)
            }
            "Read" | "Write" | "Edit" => self
                .input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "Glob" | "Grep" => self
                .input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "Task" => self
                .input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "WebFetch" => self
                .input
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "WebSearch" => self
                .input
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        }
    }
}

/// Truncate a string to `max` characters, appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Event emitted during incremental JSONL parsing.
#[derive(Debug, Clone)]
pub enum ClaudeStreamEvent {
    /// A tool_use block was found in an assistant message.
    ToolUse(ClaudeToolCall),
}

/// Parse a single JSONL line incrementally, updating `state` and returning
/// any events that should be emitted immediately.
///
/// Call this repeatedly as new stdout lines arrive from claude-code.
/// The `tool_id_map` tracks tool_use_id → index for matching results.
pub fn parse_jsonl_line(
    line: &str,
    state: &mut ClaudeExecResult,
    tool_id_map: &mut HashMap<String, usize>,
) -> Vec<ClaudeStreamEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }

    let event: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut events = Vec::new();

    match event_type {
        "system" => {
            if let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) {
                state.session_id = sid.to_string();
            }
            if let Some(model) = event.get("model").and_then(|v| v.as_str()) {
                state.model = model.to_string();
            }
        }
        "assistant" => {
            if let Some(msg) = event.get("message") {
                if let Some(usage) = msg.get("usage") {
                    state.input_tokens += usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    state.output_tokens += usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }

                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if block_type == "tool_use" {
                            let tool = ClaudeToolCall {
                                tool_name: block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                                tool_use_id: block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                input: block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                                output: None,
                            };
                            let idx = state.tool_calls.len();
                            tool_id_map.insert(tool.tool_use_id.clone(), idx);
                            events.push(ClaudeStreamEvent::ToolUse(tool.clone()));
                            state.tool_calls.push(tool);
                        }
                    }
                }

                if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                    if !model.is_empty() {
                        state.model = model.to_string();
                    }
                }
            }
        }
        "user" => {
            if let Some(msg) = event.get("message") {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if block_type == "tool_result" {
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            let output_text = extract_tool_result_text(block);

                            if let Some(&idx) = tool_id_map.get(tool_use_id) {
                                if let Some(tc) = state.tool_calls.get_mut(idx) {
                                    tc.output = Some(output_text);
                                }
                            }
                        }
                    }
                }
            }
        }
        "result" => {
            state.result_text = event
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            state.total_cost_usd = event
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            state.duration_ms = event
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            state.duration_api_ms = event
                .get("duration_api_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            state.num_turns = event.get("num_turns").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            state.is_error = event
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            state.error = event
                .get("error")
                .and_then(|v| v.as_str())
                .map(String::from);

            if let Some(usage) = event.get("usage") {
                if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                    state.input_tokens = it;
                }
                if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                    state.output_tokens = ot;
                }
            }
        }
        _ => {}
    }

    events
}

/// Options for `exec_claude()`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeExecOpts {
    /// Skip permission prompts (`--dangerously-skip-permissions`).
    pub dangerously_skip_permissions: bool,
    /// Extra arguments to pass to claude-code.
    pub extra_args: Vec<String>,
    /// Additional environment variables.
    pub env: Vec<(String, String)>,
    /// Per-request timeout in seconds.
    /// `None` means use the system default (1200s).
    pub timeout_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse JSONL stdout from `claude-code --output-format stream-json`.
///
/// Returns a `ClaudeExecResult` with all extracted telemetry.
pub fn parse_stream_json(stdout: &[u8]) -> ClaudeExecResult {
    let text = String::from_utf8_lossy(stdout);
    let mut result = ClaudeExecResult {
        result_text: String::new(),
        model: String::new(),
        session_id: String::new(),
        total_cost_usd: 0.0,
        duration_ms: 0,
        duration_api_ms: 0,
        num_turns: 0,
        input_tokens: 0,
        output_tokens: 0,
        is_error: false,
        error: None,
        tool_calls: Vec::new(),
    };

    // Map tool_use_id -> index in tool_calls for matching results
    let mut tool_id_map: HashMap<String, usize> = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "system" => {
                if let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) {
                    result.session_id = sid.to_string();
                }
                if let Some(model) = event.get("model").and_then(|v| v.as_str()) {
                    result.model = model.to_string();
                }
            }
            "assistant" => {
                if let Some(msg) = event.get("message") {
                    // Extract token usage from this turn
                    if let Some(usage) = msg.get("usage") {
                        result.input_tokens += usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        result.output_tokens += usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }

                    // Extract tool_use content blocks
                    if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                        for block in content {
                            let block_type =
                                block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if block_type == "tool_use" {
                                let tool = ClaudeToolCall {
                                    tool_name: block
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown")
                                        .to_string(),
                                    tool_use_id: block
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    input: block
                                        .get("input")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null),
                                    output: None,
                                };
                                let idx = result.tool_calls.len();
                                tool_id_map.insert(tool.tool_use_id.clone(), idx);
                                result.tool_calls.push(tool);
                            }
                        }
                    }

                    // Update model from message if present
                    if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                        if !model.is_empty() {
                            result.model = model.to_string();
                        }
                    }
                }
            }
            "user" => {
                // Match tool_result blocks to previous tool_use
                if let Some(msg) = event.get("message") {
                    if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                        for block in content {
                            let block_type =
                                block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if block_type == "tool_result" {
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                let output_text = extract_tool_result_text(block);

                                if let Some(&idx) = tool_id_map.get(tool_use_id) {
                                    if let Some(tc) = result.tool_calls.get_mut(idx) {
                                        tc.output = Some(output_text);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "result" => {
                result.result_text = event
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                result.total_cost_usd = event
                    .get("total_cost_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                result.duration_ms = event
                    .get("duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                result.duration_api_ms = event
                    .get("duration_api_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                result.num_turns =
                    event.get("num_turns").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                result.is_error = event
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                result.error = event
                    .get("error")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                // Override tokens from result-level usage if present
                if let Some(usage) = event.get("usage") {
                    if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        result.input_tokens = it;
                    }
                    if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        result.output_tokens = ot;
                    }
                }
            }
            _ => {
                // Ignore stream_event and other types
            }
        }
    }

    result
}

/// Extract text from a tool_result content block.
/// Content can be either a string or an array of text blocks.
fn extract_tool_result_text(block: &serde_json::Value) -> String {
    if let Some(content) = block.get("content") {
        match content {
            serde_json::Value::String(s) => return s.clone(),
            serde_json::Value::Array(arr) => {
                let mut parts = Vec::new();
                for item in arr {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        parts.push(text);
                    }
                }
                return parts.join("");
            }
            _ => {}
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// OTel span creation (feature-gated)
// ---------------------------------------------------------------------------

/// Create spans from a parsed `ClaudeExecResult`.
///
/// Creates a root `claude.exec` span with child spans for each tool call,
/// following the [OTel GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/).
/// Attribute names come from the `opentelemetry-semantic-conventions` crate
/// so they stay in sync with the spec automatically.
///
/// Spans are recorded in the provided `Tracer`'s in-memory storage and,
/// when the `opentelemetry` feature is enabled, also exported via the
/// OTel SDK bridge.
pub fn create_otel_spans(
    result: &ClaudeExecResult,
    parent_context: Option<&crate::observe::tracer::SpanContext>,
    tracer: &crate::observe::tracer::Tracer,
) {
    use crate::observe::tracer::Span;
    use opentelemetry_semantic_conventions::attribute as semconv;

    // Create the root claude.exec span
    let mut exec_span = if let Some(parent) = parent_context {
        Span::child("claude.exec", parent)
    } else {
        Span::new("claude.exec")
    };

    // --- OTel GenAI semconv: Required ---
    exec_span.set_attribute(semconv::GEN_AI_OPERATION_NAME, "invoke_agent");
    exec_span.set_attribute(semconv::GEN_AI_SYSTEM, "anthropic");

    // --- OTel GenAI semconv: Conditionally Required ---
    exec_span.set_attribute(semconv::GEN_AI_REQUEST_MODEL, &result.model);
    exec_span.set_attribute(semconv::GEN_AI_CONVERSATION_ID, &result.session_id);

    // --- OTel GenAI semconv: Recommended ---
    exec_span.set_attribute(semconv::GEN_AI_RESPONSE_MODEL, &result.model);
    exec_span.set_attribute(
        semconv::GEN_AI_USAGE_INPUT_TOKENS,
        result.input_tokens.to_string(),
    );
    exec_span.set_attribute(
        semconv::GEN_AI_USAGE_OUTPUT_TOKENS,
        result.output_tokens.to_string(),
    );

    // --- OTel semconv: error (Stable) ---
    if result.is_error {
        exec_span.set_attribute(semconv::ERROR_TYPE, "agent_error");
    }

    // --- Custom void-box extensions (no semconv equivalent) ---
    exec_span.set_attribute(
        "claude.total_cost_usd",
        format!("{:.6}", result.total_cost_usd),
    );
    exec_span.set_attribute("claude.num_turns", result.num_turns.to_string());
    exec_span.set_attribute("claude.duration_ms", result.duration_ms.to_string());
    exec_span.set_attribute("claude.duration_api_ms", result.duration_api_ms.to_string());
    exec_span.set_attribute(
        "claude.tools_used",
        result
            .tool_calls
            .iter()
            .map(|t| t.tool_name.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
    );
    exec_span.set_attribute("claude.tools_count", result.tool_calls.len().to_string());

    let exec_ctx = exec_span.context.clone();

    // Set duration from the result
    if result.duration_ms > 0 {
        exec_span.duration = Some(std::time::Duration::from_millis(result.duration_ms));
    }

    // Create child spans for each tool call
    for tool in &result.tool_calls {
        let mut tool_span = Span::child(&format!("claude.tool.{}", tool.tool_name), &exec_ctx);

        // OTel GenAI semconv on tool spans
        tool_span.set_attribute(semconv::GEN_AI_OPERATION_NAME, "execute_tool");
        tool_span.set_attribute(semconv::GEN_AI_SYSTEM, "anthropic");

        // Custom tool attributes (no semconv equivalent yet)
        tool_span.set_attribute("tool.name", &tool.tool_name);
        tool_span.set_attribute("tool.use_id", &tool.tool_use_id);

        // Truncate input for span attributes (avoid huge payloads)
        let input_str = tool.input.to_string();
        if input_str.len() <= 2000 {
            tool_span.set_attribute("tool.input", &input_str);
        } else {
            tool_span.set_attribute("tool.input", &input_str[..2000]);
            tool_span.set_attribute("tool.input.truncated", "true");
        }

        if let Some(ref output) = tool.output {
            if output.len() <= 2000 {
                tool_span.set_attribute("tool.output", output);
            } else {
                tool_span.set_attribute("tool.output", &output[..2000]);
                tool_span.set_attribute("tool.output.truncated", "true");
            }
        }

        tool_span.end();
        tracer.finish_span(tool_span);
    }

    exec_span.end();
    tracer.finish_span(exec_span);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session_jsonl() -> &'static str {
        r#"{"type":"system","subtype":"init","session_id":"sess_01","model":"sonnet","tools":["Bash","Read","Write"],"cwd":"/workspace"}
{"type":"assistant","session_id":"sess_01","message":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"I'll create the script."}],"usage":{"input_tokens":120,"output_tokens":45}}}
{"type":"assistant","session_id":"sess_01","message":{"id":"msg_2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"/workspace/hello.py","content":"print('hello')"}}],"usage":{"input_tokens":30,"output_tokens":20}}}
{"type":"user","session_id":"sess_01","message":{"id":"msg_3","type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"File written successfully"}]}}
{"type":"assistant","session_id":"sess_01","message":{"id":"msg_4","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"python /workspace/hello.py"}}],"usage":{"input_tokens":40,"output_tokens":15}}}
{"type":"user","session_id":"sess_01","message":{"id":"msg_5","type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"hello\n"}]}}
{"type":"result","subtype":"success","session_id":"sess_01","total_cost_usd":0.0042,"is_error":false,"duration_ms":8500,"duration_api_ms":7200,"num_turns":3,"result":"Done. Created hello.py and ran it successfully.","usage":{"input_tokens":190,"output_tokens":80}}"#
    }

    #[test]
    fn test_parse_system_event() {
        let result = parse_stream_json(sample_session_jsonl().as_bytes());
        assert_eq!(result.session_id, "sess_01");
        assert_eq!(result.model, "sonnet");
    }

    #[test]
    fn test_parse_tool_use_events() {
        let result = parse_stream_json(sample_session_jsonl().as_bytes());
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].tool_name, "Write");
        assert_eq!(result.tool_calls[0].tool_use_id, "toolu_1");
        assert_eq!(result.tool_calls[1].tool_name, "Bash");
        assert_eq!(result.tool_calls[1].tool_use_id, "toolu_2");
    }

    #[test]
    fn test_parse_tool_results() {
        let result = parse_stream_json(sample_session_jsonl().as_bytes());
        assert_eq!(
            result.tool_calls[0].output,
            Some("File written successfully".to_string())
        );
        assert_eq!(result.tool_calls[1].output, Some("hello\n".to_string()));
    }

    #[test]
    fn test_parse_result_event() {
        let result = parse_stream_json(sample_session_jsonl().as_bytes());
        assert_eq!(
            result.result_text,
            "Done. Created hello.py and ran it successfully."
        );
        assert!(!result.is_error);
        assert!(result.error.is_none());
        assert_eq!(result.total_cost_usd, 0.0042);
        assert_eq!(result.duration_ms, 8500);
        assert_eq!(result.duration_api_ms, 7200);
        assert_eq!(result.num_turns, 3);
        // Result-level usage overrides per-turn accumulation
        assert_eq!(result.input_tokens, 190);
        assert_eq!(result.output_tokens, 80);
    }

    #[test]
    fn test_parse_error_result() {
        let jsonl = r#"{"type":"system","subtype":"init","session_id":"sess_02","model":"sonnet","tools":["Bash"],"cwd":"/workspace"}
{"type":"result","subtype":"error","session_id":"sess_02","total_cost_usd":0.001,"is_error":true,"duration_ms":2000,"duration_api_ms":1800,"num_turns":1,"result":"","error":"Permission denied","usage":{"input_tokens":50,"output_tokens":10}}"#;
        let result = parse_stream_json(jsonl.as_bytes());
        assert!(result.is_error);
        assert_eq!(result.error, Some("Permission denied".to_string()));
        assert_eq!(result.result_text, "");
        assert_eq!(result.num_turns, 1);
    }

    #[test]
    fn test_parse_empty_input() {
        let result = parse_stream_json(b"");
        assert_eq!(result.session_id, "");
        assert_eq!(result.model, "");
        assert_eq!(result.tool_calls.len(), 0);
        assert!(!result.is_error);
    }

    #[test]
    fn test_parse_invalid_json_lines_skipped() {
        let jsonl = "not json\n{\"type\":\"system\",\"session_id\":\"s1\",\"model\":\"opus\"}\nalso not json\n";
        let result = parse_stream_json(jsonl.as_bytes());
        assert_eq!(result.session_id, "s1");
        assert_eq!(result.model, "opus");
    }

    #[test]
    fn test_parse_tool_result_array_content() {
        let jsonl = r#"{"type":"assistant","session_id":"s1","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","session_id":"s1","message":{"id":"msg_2","role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"file1.txt"},{"type":"text","text":"\nfile2.txt"}]}]}}"#;
        let result = parse_stream_json(jsonl.as_bytes());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(
            result.tool_calls[0].output,
            Some("file1.txt\nfile2.txt".to_string())
        );
    }

    #[test]
    fn test_tool_summary_bash() {
        let tc = ClaudeToolCall {
            tool_name: "Bash".into(),
            tool_use_id: "toolu_1".into(),
            input: serde_json::json!({"command": "git clone https://github.com/example/repo.git"}),
            output: None,
        };
        assert_eq!(
            tc.tool_summary(),
            "git clone https://github.com/example/repo.git"
        );
    }

    #[test]
    fn test_tool_summary_bash_truncated() {
        let long_cmd = "a".repeat(100);
        let tc = ClaudeToolCall {
            tool_name: "Bash".into(),
            tool_use_id: "toolu_1".into(),
            input: serde_json::json!({"command": long_cmd}),
            output: None,
        };
        let summary = tc.tool_summary();
        assert!(summary.len() <= 83); // 80 + "..."
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn test_tool_summary_read() {
        let tc = ClaudeToolCall {
            tool_name: "Read".into(),
            tool_use_id: "toolu_1".into(),
            input: serde_json::json!({"file_path": "/workspace/src/main.rs"}),
            output: None,
        };
        assert_eq!(tc.tool_summary(), "/workspace/src/main.rs");
    }

    #[test]
    fn test_tool_summary_grep() {
        let tc = ClaudeToolCall {
            tool_name: "Grep".into(),
            tool_use_id: "toolu_1".into(),
            input: serde_json::json!({"pattern": "fn main"}),
            output: None,
        };
        assert_eq!(tc.tool_summary(), "fn main");
    }

    #[test]
    fn test_tool_summary_unknown_tool() {
        let tc = ClaudeToolCall {
            tool_name: "CustomTool".into(),
            tool_use_id: "toolu_1".into(),
            input: serde_json::json!({}),
            output: None,
        };
        assert_eq!(tc.tool_summary(), "");
    }

    #[test]
    fn test_parse_jsonl_line_tool_events() {
        let mut state = ClaudeExecResult {
            result_text: String::new(),
            model: String::new(),
            session_id: String::new(),
            total_cost_usd: 0.0,
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_error: false,
            error: None,
            tool_calls: Vec::new(),
        };
        let mut tool_id_map = HashMap::new();

        // System line
        let events = parse_jsonl_line(
            r#"{"type":"system","session_id":"s1","model":"sonnet"}"#,
            &mut state,
            &mut tool_id_map,
        );
        assert!(events.is_empty());
        assert_eq!(state.session_id, "s1");

        // Tool use line
        let events = parse_jsonl_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}]}}"#,
            &mut state,
            &mut tool_id_map,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClaudeStreamEvent::ToolUse(tc) => {
                assert_eq!(tc.tool_name, "Bash");
                assert_eq!(tc.tool_summary(), "ls -la");
            }
        }
        assert_eq!(state.tool_calls.len(), 1);

        // Tool result line
        let events = parse_jsonl_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file1\nfile2"}]}}"#,
            &mut state,
            &mut tool_id_map,
        );
        assert!(events.is_empty());
        assert_eq!(state.tool_calls[0].output, Some("file1\nfile2".to_string()));
    }

    #[test]
    fn test_parse_jsonl_line_matches_batch() {
        // Verify incremental parsing produces the same result as batch
        let jsonl = sample_session_jsonl();
        let batch_result = parse_stream_json(jsonl.as_bytes());

        let mut incr_result = ClaudeExecResult {
            result_text: String::new(),
            model: String::new(),
            session_id: String::new(),
            total_cost_usd: 0.0,
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_error: false,
            error: None,
            tool_calls: Vec::new(),
        };
        let mut tool_id_map = HashMap::new();
        for line in jsonl.lines() {
            parse_jsonl_line(line, &mut incr_result, &mut tool_id_map);
        }

        assert_eq!(incr_result.session_id, batch_result.session_id);
        assert_eq!(incr_result.model, batch_result.model);
        assert_eq!(incr_result.result_text, batch_result.result_text);
        assert_eq!(incr_result.total_cost_usd, batch_result.total_cost_usd);
        assert_eq!(incr_result.tool_calls.len(), batch_result.tool_calls.len());
        for (a, b) in incr_result.tool_calls.iter().zip(&batch_result.tool_calls) {
            assert_eq!(a.tool_name, b.tool_name);
            assert_eq!(a.tool_use_id, b.tool_use_id);
            assert_eq!(a.output, b.output);
        }
    }
}
