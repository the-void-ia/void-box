//! claudio: configurable mock of `claude-code` CLI for void-box testing and playground.
//!
//! Emits valid `--output-format stream-json` JSONL to stdout.
//! Controlled via environment variables:
//!
//! | Env Var                     | Default                  | Description                                      |
//! | --------------------------- | ------------------------ | ------------------------------------------------ |
//! | MOCK_CLAUDE_SCENARIO        | simple                   | simple, multi_tool, error, heavy, custom          |
//! | MOCK_CLAUDE_TOOLS           | (scenario default)       | Override number of tool calls                     |
//! | MOCK_CLAUDE_TURNS           | (scenario default)       | Override number of conversation turns             |
//! | MOCK_CLAUDE_INPUT_TOKENS    | 500                      | Simulated input tokens                            |
//! | MOCK_CLAUDE_OUTPUT_TOKENS   | 200                      | Simulated output tokens                           |
//! | MOCK_CLAUDE_COST            | 0.003                    | Simulated cost in USD                             |
//! | MOCK_CLAUDE_DELAY_MS        | 0                        | Delay between events (for streaming simulation)   |
//! | MOCK_CLAUDE_MODEL           | claude-sonnet-4-20250514 | Model name in output                              |
//! | MOCK_CLAUDE_ERROR           | (none)                   | If set, emit an error result with this message    |
//! | MOCK_CLAUDE_CUSTOM_JSONL    | (none)                   | Path to custom JSONL file to emit verbatim        |

use std::env;
use std::io::{self, Write, BufRead, BufReader};
use std::thread;
use std::time::Duration;

fn main() {
    // Parse command line args to extract the prompt (mimic claude-code CLI)
    let args: Vec<String> = env::args().collect();
    let prompt = extract_prompt(&args);

    // Read configuration from environment
    let config = Config::from_env();

    // Custom JSONL mode: just emit the file verbatim
    if let Some(ref path) = config.custom_jsonl {
        emit_custom_jsonl(path, config.delay_ms);
        return;
    }

    // Emit the stream-json session
    emit_session(&config, &prompt);
}

/// Extract the prompt from `-p <prompt>` arguments.
fn extract_prompt(args: &[String]) -> String {
    for (i, arg) in args.iter().enumerate() {
        if (arg == "-p" || arg == "--prompt") && i + 1 < args.len() {
            return args[i + 1].clone();
        }
    }
    "no prompt provided".to_string()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct Config {
    scenario: String,
    num_tools: Option<usize>,
    num_turns: Option<usize>,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    delay_ms: u64,
    model: String,
    error_message: Option<String>,
    custom_jsonl: Option<String>,
    session_id: String,
    traceparent: Option<String>,
}

impl Config {
    fn from_env() -> Self {
        Self {
            scenario: env::var("MOCK_CLAUDE_SCENARIO").unwrap_or_else(|_| "simple".to_string()),
            num_tools: env::var("MOCK_CLAUDE_TOOLS").ok().and_then(|v| v.parse().ok()),
            num_turns: env::var("MOCK_CLAUDE_TURNS").ok().and_then(|v| v.parse().ok()),
            input_tokens: env::var("MOCK_CLAUDE_INPUT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500),
            output_tokens: env::var("MOCK_CLAUDE_OUTPUT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200),
            cost: env::var("MOCK_CLAUDE_COST")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.003),
            delay_ms: env::var("MOCK_CLAUDE_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            model: env::var("MOCK_CLAUDE_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string()),
            error_message: env::var("MOCK_CLAUDE_ERROR").ok().filter(|s| !s.is_empty()).or_else(|| {
                // The "error" scenario implies an error by default
                if env::var("MOCK_CLAUDE_SCENARIO").ok().as_deref() == Some("error") {
                    Some("Permission denied: operation not allowed".to_string())
                } else {
                    None
                }
            }),
            custom_jsonl: env::var("MOCK_CLAUDE_CUSTOM_JSONL")
                .ok()
                .filter(|s| !s.is_empty()),
            session_id: format!("mock_sess_{}", std::process::id()),
            traceparent: env::var("TRACEPARENT").ok().filter(|s| !s.is_empty()),
        }
    }

    /// Get effective number of tool calls for the scenario.
    fn effective_tools(&self) -> usize {
        if let Some(n) = self.num_tools {
            return n;
        }
        match self.scenario.as_str() {
            "simple" => 1,
            "multi_tool" => 5,
            "error" => 2,
            "heavy" => 20,
            _ => 3,
        }
    }

    /// Get effective number of turns for the scenario.
    fn effective_turns(&self) -> usize {
        if let Some(n) = self.num_turns {
            return n;
        }
        match self.scenario.as_str() {
            "simple" => 1,
            "multi_tool" => 3,
            "error" => 2,
            "heavy" => 10,
            _ => 2,
        }
    }
}

// ---------------------------------------------------------------------------
// JSONL event emission
// ---------------------------------------------------------------------------

fn emit_line(line: &str, delay_ms: u64) {
    println!("{}", line);
    io::stdout().flush().ok();
    if delay_ms > 0 {
        thread::sleep(Duration::from_millis(delay_ms));
    }
}

fn emit_session(config: &Config, prompt: &str) {
    let stdout = io::stdout();
    let _lock = stdout.lock();

    // --- system event ---
    let traceparent_field = config
        .traceparent
        .as_ref()
        .map(|tp| format!(r#","traceparent":"{}""#, tp))
        .unwrap_or_default();

    let tools_json: Vec<String> = get_tool_names(config)
        .iter()
        .map(|t| format!(r#""{}""#, t))
        .collect();

    let system_event = format!(
        r#"{{"type":"system","subtype":"init","session_id":"{}","model":"{}","tools":[{}],"cwd":"/workspace"{}}}"#,
        config.session_id,
        config.model,
        tools_json.join(","),
        traceparent_field,
    );
    emit_line(&system_event, config.delay_ms);

    // --- generate tool call turns ---
    let num_tools = config.effective_tools();
    let tool_names = get_tool_names(config);
    let mut msg_id = 1;
    let mut tool_id = 1;

    for t in 0..num_tools {
        let tool_name = &tool_names[t % tool_names.len()];
        let (input_json, output_text) = tool_content(tool_name, t, prompt);

        // Assistant message with tool_use
        let per_turn_input = config.input_tokens / (num_tools as u64).max(1);
        let per_turn_output = config.output_tokens / (num_tools as u64).max(1);

        let assistant_event = format!(
            r#"{{"type":"assistant","session_id":"{}","message":{{"id":"msg_{}","type":"message","role":"assistant","content":[{{"type":"tool_use","id":"toolu_{}","name":"{}","input":{}}}],"model":"{}","usage":{{"input_tokens":{},"output_tokens":{}}}}}}}"#,
            config.session_id, msg_id, tool_id, tool_name, input_json, config.model,
            per_turn_input, per_turn_output,
        );
        emit_line(&assistant_event, config.delay_ms);
        msg_id += 1;

        // User message with tool_result
        let user_event = format!(
            r#"{{"type":"user","session_id":"{}","message":{{"id":"msg_{}","type":"message","role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_{}","content":"{}"}}]}}}}"#,
            config.session_id, msg_id, tool_id, escape_json(&output_text),
        );
        emit_line(&user_event, config.delay_ms);
        msg_id += 1;
        tool_id += 1;
    }

    // --- result event ---
    let is_error = config.error_message.is_some();
    let result_text = if is_error {
        String::new()
    } else {
        format!(
            "Mock execution complete. Prompt was: {}. Tools used: {}.",
            truncate(prompt, 100),
            num_tools,
        )
    };

    let error_field = config
        .error_message
        .as_ref()
        .map(|msg| format!(r#","error":"{}""#, escape_json(msg)))
        .unwrap_or_default();

    let duration_ms = config.delay_ms * (num_tools as u64 * 2 + 2) + 500;

    let result_event = format!(
        r#"{{"type":"result","subtype":"{}","session_id":"{}","total_cost_usd":{},"is_error":{},"duration_ms":{},"duration_api_ms":{},"num_turns":{},"result":"{}","usage":{{"input_tokens":{},"output_tokens":{}}}{}}}
"#,
        if is_error { "error" } else { "success" },
        config.session_id,
        config.cost,
        is_error,
        duration_ms,
        duration_ms.saturating_sub(100),
        config.effective_turns(),
        escape_json(&result_text),
        config.input_tokens,
        config.output_tokens,
        error_field,
    );
    emit_line(result_event.trim(), config.delay_ms);
}

/// Emit a custom JSONL file verbatim, line by line.
fn emit_custom_jsonl(path: &str, delay_ms: u64) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("claudio: failed to open custom JSONL {}: {}", path, e);
            std::process::exit(1);
        }
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        match line {
            Ok(l) if !l.trim().is_empty() => emit_line(&l, delay_ms),
            Ok(_) => {}
            Err(e) => {
                eprintln!("claudio: error reading JSONL: {}", e);
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool generation helpers
// ---------------------------------------------------------------------------

/// Get tool names for the scenario.
fn get_tool_names(config: &Config) -> Vec<String> {
    match config.scenario.as_str() {
        "simple" => vec!["Write".into()],
        "multi_tool" => vec![
            "Read".into(),
            "Write".into(),
            "Bash".into(),
            "Read".into(),
            "Write".into(),
        ],
        "error" => vec!["Bash".into(), "Write".into()],
        "heavy" => vec![
            "Read".into(),
            "Write".into(),
            "Bash".into(),
            "Glob".into(),
            "Grep".into(),
        ],
        _ => vec!["Bash".into(), "Read".into(), "Write".into()],
    }
}

/// Generate tool input JSON and output text for a given tool.
fn tool_content(tool_name: &str, index: usize, prompt: &str) -> (String, String) {
    match tool_name {
        "Write" => {
            let path = format!("/workspace/file_{}.py", index);
            let content = format!("# Generated for: {}\nprint('hello from mock')\n", truncate(prompt, 50));
            let input = serde_json::json!({
                "file_path": path,
                "content": content,
            });
            (input.to_string(), format!("File written: {}", path))
        }
        "Read" => {
            let path = format!("/workspace/file_{}.py", index);
            let input = serde_json::json!({
                "file_path": path,
            });
            (input.to_string(), format!("# contents of {}\nprint('hello')\n", path))
        }
        "Bash" => {
            let cmd = format!("echo 'step {} done'", index);
            let input = serde_json::json!({
                "command": cmd,
            });
            (input.to_string(), format!("step {} done\n", index))
        }
        "Glob" => {
            let input = serde_json::json!({
                "pattern": "/workspace/**/*.py",
            });
            (
                input.to_string(),
                "/workspace/file_0.py\n/workspace/file_1.py\n".to_string(),
            )
        }
        "Grep" => {
            let input = serde_json::json!({
                "pattern": "hello",
                "path": "/workspace",
            });
            (
                input.to_string(),
                "/workspace/file_0.py:2:print('hello from mock')\n".to_string(),
            )
        }
        _ => {
            let input = serde_json::json!({ "arg": format!("value_{}", index) });
            (input.to_string(), format!("output for tool {} step {}", tool_name, index))
        }
    }
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
