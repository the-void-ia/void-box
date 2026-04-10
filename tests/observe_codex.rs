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
