//! Stream parser for `codex exec --json` JSONL output.
//!
//! Codex emits one JSON object per line with a top-level `type` discriminant.
//! This module deserializes the event stream into structured updates on a
//! shared [`AgentExecResult`](crate::observe::claude::AgentExecResult), the
//! same struct populated by the Claude parser. Both parsers produce the same
//! result shape so downstream consumers (`agent_box::run`,
//! `Sandbox::exec_agent_streaming`) don't branch on provider.
//!
//! Event taxonomy (verified against codex 0.118.0):
//!
//! - `thread.started { thread_id }` → populates `result.session_id`
//! - `turn.started {}` → no-op
//! - `item.started { item }` → no-op for the accumulator (parser tracks open
//!   items only if needed for in-progress UI; the accumulator only commits on
//!   `item.completed`)
//! - `item.completed { item }` → dispatches on `item.type`:
//!     - `agent_message` → updates `result.result_text` (last one wins)
//!     - `file_change` / `command_execution` / other → appends to
//!       `result.tool_calls`
//! - `turn.completed { usage }` → updates token counts
//! - `error { message }` → logged via `tracing::warn!`, not stored
//! - `turn.failed { error }` → sets `result.is_error = true` and overwrites
//!   `result.result_text` with the error message

use crate::observe::claude::{AgentExecResult, ClaudeToolCall};
use serde::Deserialize;

/// Parse a single line of codex JSONL output and update the accumulator.
///
/// Lines that fail to parse as JSON are logged at debug level and ignored —
/// codex occasionally emits non-JSONL diagnostic lines that should not break
/// the parser.
pub fn parse_codex_line(line: &str, result: &mut AgentExecResult) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    let event: CodexEvent = match serde_json::from_str(trimmed) {
        Ok(event) => event,
        Err(error) => {
            tracing::debug!(
                error = %error,
                line = trimmed,
                "codex parser: skipping non-JSONL line"
            );
            return;
        }
    };

    match event {
        CodexEvent::ThreadStarted { thread_id } => {
            result.session_id = thread_id;
        }
        CodexEvent::TurnStarted => {}
        CodexEvent::ItemStarted { .. } => {}
        CodexEvent::ItemCompleted { item } => {
            apply_item_completed(&item, result);
        }
        CodexEvent::TurnCompleted { usage } => {
            result.input_tokens = usage.input_tokens;
            result.output_tokens = usage.output_tokens;
        }
        CodexEvent::Error { message } => {
            tracing::warn!(message = %message, "codex emitted recoverable error event");
        }
        CodexEvent::TurnFailed { error } => {
            result.is_error = true;
            result.result_text = error.message;
        }
    }
}

/// Dispatch on `item.type` from a parsed codex `item.completed` payload.
///
/// Uses a `serde_json::Value`-based approach instead of a `#[serde(tag)]`
/// enum so unknown item types degrade gracefully (recorded as a generic
/// tool call with the unknown type name) instead of failing the whole
/// parse.
fn apply_item_completed(item: &serde_json::Value, result: &mut AgentExecResult) {
    let Some(item_type) = item.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match item_type {
        "agent_message" => {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                result.result_text = text.to_string();
            }
        }
        "file_change" => {
            let changes = item
                .get("changes")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            result.tool_calls.push(ClaudeToolCall {
                tool_name: "file_change".to_string(),
                tool_use_id: id,
                input: serde_json::json!({ "changes": changes }),
                output: Some(status),
            });
        }
        "command_execution" => {
            let command = item
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let aggregated_output = item
                .get("aggregated_output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            result.tool_calls.push(ClaudeToolCall {
                tool_name: "command_execution".to_string(),
                tool_use_id: id,
                input: serde_json::json!({
                    "command": command,
                    "exit_code": exit_code,
                    "status": status,
                }),
                output: Some(aggregated_output),
            });
        }
        unknown => {
            result.tool_calls.push(ClaudeToolCall {
                tool_name: unknown.to_string(),
                tool_use_id: id,
                input: serde_json::json!({}),
                output: None,
            });
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },

    #[serde(rename = "turn.started")]
    TurnStarted,

    #[serde(rename = "item.started")]
    ItemStarted {
        #[allow(dead_code)]
        item: serde_json::Value,
    },

    #[serde(rename = "item.completed")]
    ItemCompleted { item: serde_json::Value },

    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: CodexUsage },

    #[serde(rename = "error")]
    Error { message: String },

    #[serde(rename = "turn.failed")]
    TurnFailed { error: CodexErrorPayload },
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    input_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    cached_input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CodexErrorPayload {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thread_started() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"thread.started","thread_id":"abc-123"}"#,
            &mut result,
        );
        assert_eq!(result.session_id, "abc-123");
    }

    #[test]
    fn parses_turn_completed_extracts_tokens() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":25}}"#,
            &mut result,
        );
        assert_eq!(result.input_tokens, 100);
        assert_eq!(result.output_tokens, 25);
    }

    #[test]
    fn agent_message_completed_updates_result_text() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#,
            &mut result,
        );
        assert_eq!(result.result_text, "hello");
    }

    #[test]
    fn last_agent_message_wins() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"first"}}"#,
            &mut result,
        );
        parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"second"}}"#,
            &mut result,
        );
        assert_eq!(result.result_text, "second");
    }

    #[test]
    fn file_change_completed_appends_tool_call() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"/workspace/x","kind":"add"}],"status":"completed"}}"#,
            &mut result,
        );
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool_name, "file_change");
        assert_eq!(result.tool_calls[0].tool_use_id, "item_1");
        assert_eq!(result.tool_calls[0].output.as_deref(), Some("completed"));
    }

    #[test]
    fn turn_failed_sets_is_error_and_text() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#,
            &mut result,
        );
        assert!(result.is_error);
        assert_eq!(result.result_text, "401 Unauthorized");
    }

    #[test]
    fn error_event_does_not_set_is_error() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
            &mut result,
        );
        assert!(!result.is_error);
        assert_eq!(result.result_text, "");
    }

    #[test]
    fn empty_line_is_noop() {
        let mut result = AgentExecResult::default();
        parse_codex_line("", &mut result);
        parse_codex_line("   ", &mut result);
        assert_eq!(result.session_id, "");
    }

    #[test]
    fn malformed_json_is_skipped() {
        let mut result = AgentExecResult::default();
        parse_codex_line("not json at all", &mut result);
        parse_codex_line(r#"{"type":"unknown_event"}"#, &mut result);
        assert_eq!(result.session_id, "");
    }

    #[test]
    fn unknown_item_type_records_generic_tool_call() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_99","type":"future_tool"}}"#,
            &mut result,
        );
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool_name, "future_tool");
        assert_eq!(result.tool_calls[0].tool_use_id, "item_99");
        assert!(result.tool_calls[0].output.is_none());
    }

    #[test]
    fn command_execution_completed_appends_tool_call() {
        let mut result = AgentExecResult::default();
        parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_5","type":"command_execution","command":"ls -la","aggregated_output":"total 0\n","exit_code":0,"status":"completed"}}"#,
            &mut result,
        );
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool_name, "command_execution");
        assert_eq!(result.tool_calls[0].output.as_deref(), Some("total 0\n"));
    }
}
