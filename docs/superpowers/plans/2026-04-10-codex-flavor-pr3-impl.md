# Codex Flavor PR 3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the passthrough observer for Codex with a structured stream parser that extracts tool calls, agent messages, and token usage from `codex exec --json` output, populating `AgentExecResult` with the same shape Claude already produces.

**Architecture:** Add a new `src/observe/codex.rs` parser that consumes Codex's JSONL event stream (`thread.started`, `turn.started`, `item.started`, `item.completed`, `turn.completed`). Replace the binary-name-based dispatch in `Sandbox::exec_agent_streaming` with a typed `LlmProvider::observer_kind() -> ObserverKind` selector. The Claude branch keeps `parse_jsonl_line` from `observe::claude`; the Codex branch calls a new `parse_codex_line` from `observe::codex`. Both populate the same `AgentExecResult` struct (renamed flat in PR 2), so downstream consumers in `agent_box.rs` see consistent fields regardless of provider.

**Tech Stack:** Rust (edition 2021), serde + serde_json (for parsing), existing `tracing` for streaming.

**Rust skills:** Apply `rust-style` and `rustdoc` skills to all Rust code. Apply `verify` skill before marking any task complete.

**Spec:** `docs/superpowers/specs/2026-04-07-codex-flavor-design.md` (PR 3 section)

**Prerequisites:**
- PR 1 + PR 2 landed on `feat/codex` (last commit `74e84d0`).
- A real Codex JSONL fixture is captured below from a successful Gate 2 e2e run on 2026-04-10. Use it as the canonical first test fixture in Task 1.

---

## Captured Codex JSONL fixture (use as `tests/fixtures/codex_events/hello_world.jsonl`)

```jsonl
{"type":"thread.started","thread_id":"019d74db-6d81-7c22-92bd-2c05e738e9dd"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Writing the exact requested output to `/workspace/output.json` now."}}
{"type":"item.started","item":{"id":"item_1","type":"file_change","changes":[{"path":"/workspace/output.json","kind":"add"}],"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"/workspace/output.json","kind":"add"}],"status":"completed"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Hello from void-box!"}}
{"type":"turn.completed","usage":{"input_tokens":22578,"cached_input_tokens":19712,"output_tokens":251}}
```

This was emitted by codex 0.118.0 inside a void-box guest in response to the
prompt "Say exactly: Hello from void-box! Then stop." Use it byte-for-byte
in Task 1 — do not paraphrase.

---

## Codex event taxonomy (verified against fixture + upstream `codex-rs/exec/src/exec_events.rs`)

The `--json` flag emits one JSON object per line. Each object has a top-level
`type` discriminant. Observed types:

| `type` | Schema (relevant fields) | Meaning |
|---|---|---|
| `thread.started` | `{ thread_id: String }` | First event of a session. Maps to `AgentExecResult.session_id`. |
| `turn.started` | `{}` | Turn beginning. No payload to extract. |
| `item.started` | `{ item: { id, type, ... } }` | Tool/file/command beginning. Inspect `item.type` to dispatch. |
| `item.completed` | `{ item: { id, type, ... } }` | Tool/file/command finished, OR a final agent text reply. |
| `turn.completed` | `{ usage: { input_tokens, cached_input_tokens, output_tokens } }` | Turn finished — extract token counts. |
| `error` | `{ message: String }` | Recoverable error (codex internal retry). Logged as `WARN`, not propagated. |
| `turn.failed` | `{ error: { message: String } }` | Terminal failure — set `is_error: true` and capture in `result_text`. |

`item.type` discriminants observed:

| `item.type` | Schema | Maps to `AgentToolCall` field shape |
|---|---|---|
| `agent_message` | `{ id, type: "agent_message", text: String }` | NOT a tool call. The latest `item_N` of type `agent_message` becomes `result_text`. Earlier ones are intermediate reasoning and discarded. |
| `file_change` | `{ id, type: "file_change", changes: [{ path, kind }], status }` | Tool call: `tool_name = "file_change"`, `input = { changes }`, `output = Some(status)`. |
| `command_execution` | `{ id, type: "command_execution", command: String, aggregated_output: String, exit_code: Option<i64>, status: String }` | Tool call: `tool_name = "command_execution"`, `input = { command }`, `output = Some(aggregated_output)`. |

**Pairing**: `item.started` and `item.completed` share the same `item.id` (e.g.
`item_0`, `item_1`). The parser tracks open items by id and closes them on
the matching `completed` event. The fixture's `item_0` (an `agent_message`)
arrives only as `completed` with no preceding `started` — both shapes must be
handled.

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/observe/codex.rs` | Create | `parse_codex_line(line: &str, accumulator: &mut AgentExecResult)`, plus internal event types deserialized via serde. Mirrors the shape of `observe::claude::parse_jsonl_line`. |
| `src/observe/mod.rs` | Modify | Add `pub mod codex;` and re-export `parse_codex_line` if the existing module re-exports `parse_jsonl_line`. |
| `src/llm.rs` | Modify | Add `ObserverKind` enum (`StreamJson`, `Passthrough`, `Codex`). Add `pub fn observer_kind(&self) -> ObserverKind` method on `LlmProvider`. Update unit tests. |
| `src/sandbox/mod.rs` | Modify | Replace the two `provider.binary_name() == CLAUDE_CODE_BINARY` dispatch sites in `exec_agent` and `exec_agent_streaming` with `match provider.observer_kind() { ... }`. Drop the local `CLAUDE_CODE_BINARY` const if it's no longer used elsewhere. |
| `tests/fixtures/codex_events/hello_world.jsonl` | Create | Captured fixture from gate 2 (verbatim above). |
| `tests/fixtures/codex_events/error_then_success.jsonl` | Create | Synthetic fixture covering `error` and `turn.failed` event types. |
| `docs/agents/codex.md` | Modify | Update "Validation" section to mention that PR 3 produces structured `tools=N` / `tokens=Min/Mout` summaries instead of `tokens=0in/0out`. |

---

### Task 1: Add `tests/fixtures/codex_events/hello_world.jsonl` and write the failing parser test

**Files:**
- Create: `tests/fixtures/codex_events/hello_world.jsonl`
- Modify: `src/observe/mod.rs` (add `pub mod codex;` declaration only — module file comes in Task 2)

This task lays down the fixture and registers the future module. The test it writes will fail to compile because `observe::codex::parse_codex_line` doesn't exist yet — that's Task 2.

- [ ] **Step 1: Create the fixture directory and file**

```bash
mkdir -p tests/fixtures/codex_events
```

Create `tests/fixtures/codex_events/hello_world.jsonl` with this exact content (one JSON object per line, no trailing newline beyond the last):

```jsonl
{"type":"thread.started","thread_id":"019d74db-6d81-7c22-92bd-2c05e738e9dd"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Writing the exact requested output to `/workspace/output.json` now."}}
{"type":"item.started","item":{"id":"item_1","type":"file_change","changes":[{"path":"/workspace/output.json","kind":"add"}],"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"/workspace/output.json","kind":"add"}],"status":"completed"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Hello from void-box!"}}
{"type":"turn.completed","usage":{"input_tokens":22578,"cached_input_tokens":19712,"output_tokens":251}}
```

- [ ] **Step 2: Register the future codex module in `src/observe/mod.rs`**

Read `src/observe/mod.rs` to see the existing module declarations. There should be `pub mod claude;` already. Add:

```rust
pub mod codex;
```

Place it directly after `pub mod claude;` (alphabetical order). Do NOT add the `codex.rs` file yet — that's Task 2. The compile will fail at this point, which is expected for TDD.

- [ ] **Step 3: Write the failing parser test**

The test lives in `src/observe/codex.rs` (will be created in Task 2). For now, write it as a separate integration test under `tests/observe_codex.rs` so it's visible without the module file existing:

Create `tests/observe_codex.rs` with this exact content:

```rust
//! Integration tests for the codex stream-json parser.
//!
//! These tests load real codex JSONL fixtures from
//! `tests/fixtures/codex_events/` and assert that the parser populates
//! `AgentExecResult` with the expected fields.

use void_box::observe::claude::AgentExecResult;
use void_box::observe::codex::parse_codex_line;

#[test]
fn parses_hello_world_fixture() {
    let raw = std::fs::read_to_string("tests/fixtures/codex_events/hello_world.jsonl")
        .expect("fixture must exist — see PR 3 plan Task 1");

    let mut result = AgentExecResult::default();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        parse_codex_line(line, &mut result);
    }

    // Session id from thread.started
    assert_eq!(result.session_id, "019d74db-6d81-7c22-92bd-2c05e738e9dd");

    // Token usage from turn.completed
    assert_eq!(result.input_tokens, 22578);
    assert_eq!(result.output_tokens, 251);

    // result_text is the LAST agent_message item ("Hello from void-box!"),
    // not the earlier intermediate reasoning ("Writing the exact requested
    // output ...").
    assert_eq!(result.result_text, "Hello from void-box!");

    // Two non-message tool calls: file_change (item_1).
    // (item_0 and item_2 are agent_messages, not tool calls.)
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].tool_name, "file_change");

    // No errors.
    assert!(!result.is_error);
}
```

- [ ] **Step 4: Run the test to verify it fails**

```bash
cargo test --test observe_codex 2>&1 | tail -20
```

Expected: compile failure — `observe::codex` module not found, `parse_codex_line` not defined. This is the expected red state for TDD.

- [ ] **Step 5: Commit Task 1**

```bash
git add tests/fixtures/codex_events/hello_world.jsonl tests/observe_codex.rs src/observe/mod.rs
git commit -m "$(cat <<'EOF'
observe: scaffold codex parser test (red)

Adds the captured codex JSONL fixture from a real PR 2 gate 2 e2e
run as the first canonical test fixture, plus a failing integration
test that asserts the parser populates session_id, tokens, and the
final agent_message result_text. The src/observe/codex module is
declared but not implemented — Task 2 adds the parser.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

The commit deliberately leaves the test in a failing state. Task 2 makes it green.

---

### Task 2: Implement `src/observe/codex.rs`

**Files:**
- Create: `src/observe/codex.rs`

The parser needs to handle 7 event types. Use serde with `#[serde(tag = "type")]` for the discriminated union. The whole module is single-file because each event type is small and all parsing logic belongs together.

- [ ] **Step 1: Read the Claude parser as a structural reference**

```bash
# Use the Read tool, not cat. Look at:
#   src/observe/claude.rs::parse_jsonl_line
#   src/observe/claude.rs::AgentExecResult struct definition
```

The Claude parser is the architectural template. The codex parser will produce the same `AgentExecResult` (the flat type from PR 2). Note how Claude's parser:
- Takes a single line and an `&mut AgentExecResult` accumulator
- Uses `serde_json::from_str` on the line, dispatches by event type
- Mutates fields on the accumulator (session_id, model, tool_calls, tokens, etc.)
- Treats the LAST result/text event as `result_text`

The codex parser follows the same shape but speaks the codex event vocabulary.

- [ ] **Step 2: Create `src/observe/codex.rs` with the full parser**

Create the file with this exact content:

```rust
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
            // Unknown item type — record as a generic tool call so future
            // codex event types don't break the parser. PR 4 (or a later
            // patch) can add explicit handling.
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
        // Both lines should be silently skipped without panic
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
```

- [ ] **Step 3: Run the unit tests inside `observe::codex`**

```bash
cargo test --package void-box --lib observe::codex 2>&1 | tail -20
```

Expected: all 11 unit tests pass.

- [ ] **Step 4: Run the Task 1 integration test (now should pass)**

```bash
cargo test --test observe_codex 2>&1 | tail -15
```

Expected: `parses_hello_world_fixture` passes. The fixture is parsed correctly: session_id is the thread_id, tokens come from turn.completed, result_text is the LAST agent_message ("Hello from void-box!"), tool_calls contains exactly the file_change.

- [ ] **Step 5: Run the full validation sweep**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. No changes to existing tests because the parser is additive — `Sandbox::exec_agent_streaming` still uses the binary-name dispatch from PR 2; Task 3 swaps it.

- [ ] **Step 6: Commit Task 2**

```bash
git add src/observe/codex.rs
git commit -m "$(cat <<'EOF'
observe: implement codex stream-json parser

Adds src/observe/codex.rs with parse_codex_line that consumes codex's
exec --json output and populates AgentExecResult. Handles 7 event
types (thread.started, turn.started, item.started, item.completed,
turn.completed, error, turn.failed) and 4 item.type discriminants
(agent_message, file_change, command_execution, other).

The parser is additive in this commit — Task 3 wires it into
Sandbox::exec_agent_streaming via a new ObserverKind selector,
replacing the binary_name() == CLAUDE_CODE_BINARY branch from PR 2.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Add `LlmProvider::observer_kind()` and replace the dispatch

**Files:**
- Modify: `src/llm.rs` — add `ObserverKind` enum and `observer_kind()` method
- Modify: `src/sandbox/mod.rs` — replace the two `binary_name() == CLAUDE_CODE_BINARY` branches with `match provider.observer_kind() { ... }`

- [ ] **Step 1: Add the `ObserverKind` enum and method to `src/llm.rs`**

Read `src/llm.rs` to find the existing `binary_name()` method (around line 213). Right above it (or in a position that keeps the methods grouped), add the new enum and method.

First add the enum at module scope, after the existing `LlmProvider` enum declaration but before `impl LlmProvider`:

```rust
/// Stream observer dispatcher for [`Sandbox::exec_agent_streaming`].
///
/// Each provider tells the sandbox which parser to use for its agent's
/// stdout. The sandbox dispatches to the matching `parse_*_line` function
/// from the appropriate `observe::*` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserverKind {
    /// Claude Code's `--output-format stream-json` JSONL events.
    /// Parsed by `crate::observe::claude::parse_jsonl_line`.
    ClaudeStreamJson,
    /// Codex's `exec --json` JSONL events.
    /// Parsed by `crate::observe::codex::parse_codex_line`.
    Codex,
}
```

Then add the method to `impl LlmProvider`, placed right after `binary_name()`:

```rust
    /// Stream observer to use for this provider's agent stdout.
    ///
    /// Drives dispatch in [`Sandbox::exec_agent_streaming`]: each
    /// [`ObserverKind`] maps to a different `parse_*_line` function.
    pub fn observer_kind(&self) -> ObserverKind {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. }
            | LlmProvider::Custom { .. } => ObserverKind::ClaudeStreamJson,
            LlmProvider::Codex => ObserverKind::Codex,
        }
    }
```

Per the rust-style skill: exhaustive match, no wildcards.

- [ ] **Step 2: Add unit tests for `observer_kind()`**

In the existing `#[cfg(test)] mod tests` block at the bottom of `src/llm.rs`, add:

```rust
    #[test]
    fn test_codex_observer_kind() {
        assert_eq!(LlmProvider::Codex.observer_kind(), ObserverKind::Codex);
    }

    #[test]
    fn test_claude_shaped_observer_kinds() {
        assert_eq!(
            LlmProvider::Claude.observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::ClaudePersonal.observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::ollama("test-model").observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::lm_studio("test-model").observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::custom("http://localhost:1234").observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
    }
```

- [ ] **Step 3: Run the new tests**

```bash
cargo test --package void-box --lib llm:: -- observer_kind 2>&1 | tail -10
```

Expected: 2 new tests pass (one Codex, one with 5 assertions for Claude-shaped variants).

- [ ] **Step 4: Replace dispatch in `Sandbox::exec_agent` (non-streaming path)**

Read `src/sandbox/mod.rs` around lines 280-400 (the `exec_agent` method, especially the section that calls `parse_stream_json` and the surrounding `if provider.binary_name() == CLAUDE_CODE_BINARY` guard).

Find this block (the exact lines may shift but the pattern is unchanged from PR 2):

```rust
        let result = if provider.binary_name() == CLAUDE_CODE_BINARY {
            crate::observe::claude::parse_stream_json(&output.stdout)
        } else {
            let mut result = crate::observe::claude::AgentExecResult::default();
            result.result_text = String::from_utf8_lossy(&output.stdout).into_owned();
            result.is_error = output.exit_code != 0;
            result
        };
```

Replace with:

```rust
        let result = match provider.observer_kind() {
            crate::llm::ObserverKind::ClaudeStreamJson => {
                crate::observe::claude::parse_stream_json(&output.stdout)
            }
            crate::llm::ObserverKind::Codex => {
                let stdout_text = String::from_utf8_lossy(&output.stdout);
                let mut result = crate::observe::claude::AgentExecResult::default();
                for line in stdout_text.lines() {
                    crate::observe::codex::parse_codex_line(line, &mut result);
                }
                if output.exit_code != 0 && !result.is_error {
                    result.is_error = true;
                }
                result
            }
        };
```

Note: the codex branch sets `is_error = true` only if `turn.failed` didn't already set it AND the exit code is non-zero. This handles the case where codex exits cleanly (turn.completed) but the parent process returned non-zero for an unrelated reason.

Also find the `no_stream_output` heuristic gate from PR 2. It was:

```rust
        if provider.binary_name() == CLAUDE_CODE_BINARY && no_stream_output {
            // Err(Error::Guest(...))
        }
```

Update to:

```rust
        if provider.observer_kind() == crate::llm::ObserverKind::ClaudeStreamJson && no_stream_output {
            // Err(Error::Guest(...))
        }
```

Codex never trips this heuristic because its `result_text` is populated by `agent_message` events even on partial failures.

- [ ] **Step 5: Replace dispatch in `Sandbox::exec_agent_streaming` (streaming path)**

Find the streaming `if provider.binary_name() == CLAUDE_CODE_BINARY { ... } else { ... }` block (around lines 430-590 in PR 2's state). The structure is:

```rust
        if provider.binary_name() == CLAUDE_CODE_BINARY {
            // existing JSONL parse loop calling parse_jsonl_line + on_event
            // + final no_stream_output check
        } else {
            // PR 2's passthrough: accumulate stdout, forward to tracing,
            // build AgentExecResult from joined stdout
        }
```

Replace the outer condition with a match and adapt the body:

```rust
        match provider.observer_kind() {
            crate::llm::ObserverKind::ClaudeStreamJson => {
                // existing JSONL parse loop calling parse_jsonl_line + on_event
                // + no_stream_output check
                // (unchanged from PR 2)
            }
            crate::llm::ObserverKind::Codex => {
                // Same chunk-receive loop, but pass each line through
                // parse_codex_line and accumulate into AgentExecResult.
                let mut result = crate::observe::claude::AgentExecResult::default();
                let mut stdout_accum: Vec<u8> = Vec::new();
                while let Some(chunk) = chunk_rx.recv().await {
                    if chunk.stream == "stdout" {
                        stdout_accum.extend_from_slice(&chunk.data);
                        for line in String::from_utf8_lossy(&chunk.data).lines() {
                            tracing::info!(target: AGENT_STDOUT_TARGET, "{}", line);
                            crate::observe::codex::parse_codex_line(line, &mut result);
                        }
                    }
                }

                let response = response_rx.await.map_err(|_| {
                    crate::Error::Guest(
                        "Failed to receive codex streaming response".into(),
                    )
                })??;

                if response.exit_code != 0 {
                    let stderr_str = String::from_utf8_lossy(&response.stderr);
                    tracing::warn!(
                        exit_code = response.exit_code,
                        "{} failed; stderr={}",
                        provider.binary_name(),
                        if stderr_str.is_empty() {
                            "(empty)"
                        } else {
                            stderr_str.trim()
                        },
                    );
                    if !result.is_error {
                        result.is_error = true;
                    }
                }

                Ok(result)
            }
        }
```

The Claude branch is unchanged from PR 2 — only the outer dispatch syntax flips from `if` to `match`.

**IMPORTANT**: Read the actual streaming branch from PR 2 first to confirm the channel receiver names (`chunk_rx`, `response_rx`) and the stderr-warn block. Adapt the snippet above to match the real code. The intent is the same as PR 2's passthrough but with `parse_codex_line` called on each line.

- [ ] **Step 6: Drop the local `CLAUDE_CODE_BINARY` const if no longer used**

```bash
grep -n CLAUDE_CODE_BINARY src/sandbox/mod.rs
```

If the const is now only referenced by its own declaration (no remaining usages in the file), remove the const declaration. If `provider.binary_name()` is still called anywhere in the file (e.g. in the stderr warn message), the const may stay or be removed depending on whether the comparisons are gone. Goal: zero `binary_name() == CLAUDE_CODE_BINARY` comparisons remain.

- [ ] **Step 7: Run the full validation sweep**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features 2>&1 | tail -20
```

Expected: all green. The Claude test path is unchanged in behavior. The Codex path now produces structured results.

- [ ] **Step 8: Commit Task 3**

```bash
git add src/llm.rs src/sandbox/mod.rs
git commit -m "$(cat <<'EOF'
sandbox,llm: replace binary-name dispatch with ObserverKind selector

Adds LlmProvider::observer_kind() returning ObserverKind::{ClaudeStreamJson,
Codex} and replaces the two `binary_name() == CLAUDE_CODE_BINARY` branches
in Sandbox::exec_agent and Sandbox::exec_agent_streaming with a typed match
on observer_kind(). The Codex branches now call parse_codex_line on each
stdout line, populating AgentExecResult with structured tool calls and
token counts (instead of PR 2's empty passthrough).

Claude path is unchanged in behavior — same parser, same no_stream_output
heuristic, same compat probe gating. The dispatch shape is the only thing
that flipped from `if` to `match`.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Add a synthetic `error_then_success` fixture and round-trip test

**Files:**
- Create: `tests/fixtures/codex_events/error_then_success.jsonl`
- Modify: `tests/observe_codex.rs` — add a second test using the new fixture

This task hardens the parser against the `error` (recoverable) and `turn.failed` (terminal) event types, which the gate-2 fixture doesn't exercise.

- [ ] **Step 1: Create the fixture**

Create `tests/fixtures/codex_events/error_then_success.jsonl`:

```jsonl
{"type":"thread.started","thread_id":"test-error-recovery"}
{"type":"turn.started"}
{"type":"error","message":"Reconnecting... 1/5 (We're currently experiencing high demand, which may cause temporary errors.)"}
{"type":"error","message":"Reconnecting... 2/5"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Recovered after retries."}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50}}
```

- [ ] **Step 2: Add the test in `tests/observe_codex.rs`**

Append to `tests/observe_codex.rs`:

```rust
#[test]
fn parses_error_then_success_fixture() {
    let raw = std::fs::read_to_string("tests/fixtures/codex_events/error_then_success.jsonl")
        .expect("fixture must exist — see PR 3 plan Task 4");

    let mut result = AgentExecResult::default();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        parse_codex_line(line, &mut result);
    }

    // Recoverable error events are warnings, not failures
    assert!(!result.is_error);
    assert_eq!(result.session_id, "test-error-recovery");
    assert_eq!(result.result_text, "Recovered after retries.");
    assert_eq!(result.input_tokens, 100);
    assert_eq!(result.output_tokens, 50);
    // No tool calls — only an agent_message
    assert!(result.tool_calls.is_empty());
}

#[test]
fn turn_failed_overrides_result_text() {
    let raw = r#"
{"type":"thread.started","thread_id":"test-fail"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"intermediate"}}
{"type":"turn.failed","error":{"message":"401 Unauthorized: Missing bearer"}}
"#;
    let mut result = AgentExecResult::default();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        parse_codex_line(line, &mut result);
    }
    assert!(result.is_error);
    assert_eq!(result.result_text, "401 Unauthorized: Missing bearer");
}
```

- [ ] **Step 3: Run both fixture tests**

```bash
cargo test --test observe_codex 2>&1 | tail -15
```

Expected: 3 tests pass (`parses_hello_world_fixture`, `parses_error_then_success_fixture`, `turn_failed_overrides_result_text`).

- [ ] **Step 4: Commit Task 4**

```bash
git add tests/fixtures/codex_events/error_then_success.jsonl tests/observe_codex.rs
git commit -m "$(cat <<'EOF'
observe: add codex error-recovery and turn-failed fixture coverage

The captured hello_world fixture from gate 2 didn't exercise codex's
recoverable `error` events or the terminal `turn.failed` event. This
adds a synthetic fixture covering both, plus a unit test asserting
turn.failed overwrites result_text and sets is_error.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Update `docs/agents/codex.md` to reflect structured observability

**Files:**
- Modify: `docs/agents/codex.md`

PR 2 left a note in the Validation section saying "passthrough output; PR 3
adds the structured observer". After PR 3, that note is obsolete.

- [ ] **Step 1: Read the current Validation section**

Use Read on `docs/agents/codex.md` and locate the Validation section.

- [ ] **Step 2: Update the Validation section**

Replace the Validation section with:

```markdown
## Validation

Two smoke specs exercise different entry points:

- `examples/specs/codex_workflow_smoke.yaml` — `kind: workflow` step
  running `codex --version`. Self-contained, no API key or login
  needed. Verifies the bundled binary is present and allowlisted.
- `examples/specs/codex_smoke.yaml` — `kind: agent` with
  `provider: codex`. Requires either `codex login` on the host (so
  `~/.codex/auth.json` exists for void-box to mount) or a working
  `OPENAI_API_KEY`. Verifies the full exec path through
  `LlmProvider::Codex`.

## Streaming output

Codex's `exec --json` event stream is parsed by
`src/observe/codex.rs::parse_codex_line` and populates the same
`AgentExecResult` struct that the Claude parser produces. The summary
line emitted by `agent_box.rs` reports real token counts, tool calls,
and the final agent message — for example:

```
[vm:my-spec] Agent finished | tokens=22578in/251out | tools=1 | cost=$0.0000 | error=false
```

Tool call tracking covers `file_change` (file edits) and
`command_execution` (shell commands codex runs in the guest).
Unknown item types are recorded as generic tool calls so future codex
event types don't break the parser.
```

Also remove any lingering "PR 2 passthrough" or "structured observer is
deferred to PR 3" wording from the rest of the file.

- [ ] **Step 3: Commit Task 5**

```bash
git add docs/agents/codex.md
git commit -m "$(cat <<'EOF'
docs: reflect codex structured observer in codex.md

PR 3 replaced PR 2's passthrough observer with a structured parser.
Updates the Validation section to mention real token counts and
tool call tracking, and adds a Streaming output section describing
the parser shape.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final validation

- [ ] **Step 1: Workspace fmt + clippy + tests**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. The new `tests/observe_codex.rs` integration test
binary should compile, all 3 fixture tests should pass, the 11 unit
tests in `observe::codex::tests` should pass, and the 2 new tests in
`llm::tests` for `observer_kind` should pass.

- [ ] **Step 2: `e2e_agent_mcp` regression check**

This is the same gate as PR 1/PR 2 — the rename + observer dispatch
change touched the Claude exec path's outer structure. Confirm the
Claude path is still byte-for-byte equivalent.

```bash
scripts/build_claude_rootfs.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
ANTHROPIC_API_KEY=... cargo test --test e2e_agent_mcp -- --ignored --test-threads=1 --nocapture
```

Expected: 2 passed, 0 failed. Same MCP tool count, same intent count
in the sidecar.

- [ ] **Step 3: Codex end-to-end smoke with structured observer**

```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml
```

Expected: same `success: true` as PR 2's gate 2, but the summary line
should now show real token counts:

```
[vm:codex_smoke] Agent finished | tokens=Min/Mout | tools=N | cost=$0.0000 | error=false
```

(`M` and `N` will be non-zero. Cost stays $0.00 because codex doesn't
report cost in the JSONL events — PR 4 or a future patch can add
estimation from a pricing table.)

- [ ] **Step 4: Open PR 3**

```bash
git push
```

Then open the PR on GitHub. PR title suggestion:

```
Codex flavor PR 3: structured observe::codex stream parser
```

PR description:

```
Implements PR 3 of the Codex flavor design:
docs/superpowers/specs/2026-04-07-codex-flavor-design.md

Depends on PR 2 (LlmProvider::Codex + flat AgentExec* rename + ~/.codex mount).

Scope:
- src/observe/codex.rs: parse_codex_line for codex's exec --json JSONL
  output. Handles 7 event types and 4 item.type discriminants.
- src/llm.rs: ObserverKind enum + observer_kind() method on LlmProvider.
- src/sandbox/mod.rs: replace binary_name() == CLAUDE_CODE_BINARY
  dispatch with match on observer_kind().
- tests/fixtures/codex_events/: real captured fixture from a gate-2
  e2e run, plus a synthetic error-recovery fixture.
- tests/observe_codex.rs: integration tests using both fixtures.
- docs/agents/codex.md: updated Validation + new Streaming output section.

Out of scope (deferred to PR 4):
- Codex MCP discovery via ~/.codex/config.toml (the void-mcp HTTP
  server is reachable from inside the guest, but codex isn't told
  where to find it for non-Claude providers).
```
