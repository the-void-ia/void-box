//! Codex stream-JSON event parser.
//!
//! Parses the JSONL event stream emitted by codex 0.118.0+ and accumulates
//! results into [`AgentExecResult`].
//!
//! Implementation is provided in Task 2. This stub declares the module so the
//! crate compiles and `cargo fmt` can resolve it.

use crate::observe::claude::AgentExecResult;

/// Parse a single line of codex JSONL output and update `result` in place.
///
/// Unknown or malformed lines are silently skipped.
pub fn parse_codex_line(_line: &str, _result: &mut AgentExecResult) {
    unimplemented!("codex parser not yet implemented — see Task 2")
}
