//! Pipeline: Compose Boxes into sequential data-flow pipelines.
//!
//! A Pipeline chains multiple [`AgentBox`] instances so that the output of one
//! becomes the input of the next.  Each Box boots a fresh, isolated VM, runs its
//! agent with the provisioned skills, and produces structured output.
//!
//! # Example
//!
//! ```no_run
//! use void_box::pipeline::Pipeline;
//! # use void_box::agent_box::AgentBox;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! # let data_box: AgentBox = todo!();
//! # let quant_box: AgentBox = todo!();
//! # let strategy_box: AgentBox = todo!();
//! let result = Pipeline::from(data_box)
//!     .pipe(quant_box)
//!     .pipe(strategy_box)
//!     .run()
//!     .await?;
//!
//! println!("Final output: {}", result.output);
//! # Ok(())
//! # }
//! ```

use crate::agent_box::AgentBox;
use crate::observe::claude::ClaudeExecResult;

/// Result of running a full pipeline.
#[derive(Debug)]
pub struct PipelineResult {
    /// Name of the pipeline
    pub name: String,
    /// Results from each stage, in order
    pub stages: Vec<StageResult>,
    /// The final stage output text
    pub output: String,
}

/// Result from a single pipeline stage.
#[derive(Debug)]
pub struct StageResult {
    /// Name of the Box that produced this result
    pub box_name: String,
    /// The structured Claude execution result
    pub claude_result: ClaudeExecResult,
    /// Raw file output read from the Box (if any)
    pub file_output: Option<Vec<u8>>,
}

impl PipelineResult {
    /// Check if all stages succeeded.
    pub fn success(&self) -> bool {
        self.stages.iter().all(|s| !s.claude_result.is_error)
    }

    /// Total cost across all stages.
    pub fn total_cost_usd(&self) -> f64 {
        self.stages
            .iter()
            .map(|s| s.claude_result.total_cost_usd)
            .sum()
    }

    /// Total input tokens across all stages.
    pub fn total_input_tokens(&self) -> u64 {
        self.stages
            .iter()
            .map(|s| s.claude_result.input_tokens)
            .sum()
    }

    /// Total output tokens across all stages.
    pub fn total_output_tokens(&self) -> u64 {
        self.stages
            .iter()
            .map(|s| s.claude_result.output_tokens)
            .sum()
    }

    /// Total tool calls across all stages.
    pub fn total_tool_calls(&self) -> usize {
        self.stages
            .iter()
            .map(|s| s.claude_result.tool_calls.len())
            .sum()
    }
}

/// A composable pipeline of Boxes.
pub struct Pipeline {
    name: String,
    stages: Vec<AgentBox>,
}

impl Pipeline {
    /// Start a pipeline from a single Box.
    pub fn from(first: AgentBox) -> Self {
        Self {
            name: first.name.clone(),
            stages: vec![first],
        }
    }

    /// Create a named pipeline from a single Box.
    pub fn named(name: impl Into<String>, first: AgentBox) -> Self {
        Self {
            name: name.into(),
            stages: vec![first],
        }
    }

    /// Pipe the output of the previous stage into the next Box.
    pub fn pipe(mut self, next: AgentBox) -> Self {
        self.stages.push(next);
        self
    }

    /// Execute the pipeline: run each Box in sequence, piping output forward.
    ///
    /// Each Box:
    /// 1. Boots a fresh VM
    /// 2. Provisions skills (SKILL.md files, MCP config)
    /// 3. Writes the previous stage's output to `/workspace/input.json`
    /// 4. Runs the agent with its configured prompt
    /// 5. Reads `/workspace/output.json` as the stage output
    /// 6. The VM is destroyed (Drop)
    ///
    /// The final stage's output becomes the pipeline result.
    pub async fn run(self) -> crate::Result<PipelineResult> {
        let mut stages: Vec<StageResult> = Vec::new();
        let mut carry_data: Option<Vec<u8>> = None;
        let total_stages = self.stages.len();

        for (i, agent_box) in self.stages.into_iter().enumerate() {
            let box_name = agent_box.name.clone();
            eprintln!(
                "[pipeline] Stage {}/{}: {} ...",
                i + 1,
                total_stages,
                box_name
            );

            let stage_result = agent_box.run(carry_data.as_deref()).await?;

            // Carry forward the file output or the result text
            carry_data = if stage_result.file_output.is_some() {
                stage_result.file_output.clone()
            } else if !stage_result.claude_result.result_text.is_empty() {
                Some(stage_result.claude_result.result_text.as_bytes().to_vec())
            } else {
                None
            };

            eprintln!(
                "[pipeline] Stage {} complete: {} tokens, ${:.4}",
                box_name,
                stage_result.claude_result.input_tokens + stage_result.claude_result.output_tokens,
                stage_result.claude_result.total_cost_usd,
            );

            if stage_result.claude_result.is_error {
                if looks_like_login_error(&stage_result.claude_result) {
                    eprintln!(
                        "[pipeline] Stage '{}' failed due to agent authentication. \
Run `claude-code /login` in the guest image (or configure OLLAMA_MODEL) and retry.",
                        box_name
                    );
                } else {
                    eprintln!(
                        "[pipeline] Stage '{}' failed; stopping pipeline early.",
                        box_name
                    );
                }
                stages.push(stage_result);
                break;
            }

            stages.push(stage_result);
        }

        let output = stages
            .last()
            .map(|s| s.claude_result.result_text.clone())
            .unwrap_or_default();

        Ok(PipelineResult {
            name: self.name,
            stages,
            output,
        })
    }
}

fn looks_like_login_error(result: &ClaudeExecResult) -> bool {
    let text = format!(
        "{} {}",
        result.result_text.to_ascii_lowercase(),
        result.error.as_deref().unwrap_or("").to_ascii_lowercase()
    );
    text.contains("not logged in") || text.contains("/login")
}

// Can't move out of self.stages while iterating with index... let me fix the run method.
// Actually the issue is that `self.stages` is consumed by `into_iter()` but we also
// reference `agent_box.name` after moving it. Let me restructure.

impl Pipeline {
    /// Number of stages in the pipeline.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Check if pipeline is empty.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detects_login_error_from_result_text() {
        let r = ClaudeExecResult {
            result_text: "Not logged in · Please run /login".into(),
            model: String::new(),
            session_id: String::new(),
            total_cost_usd: 0.0,
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_error: true,
            error: None,
            tool_calls: Vec::new(),
        };
        assert!(looks_like_login_error(&r));
    }

    #[test]
    fn test_detects_login_error_from_error_field() {
        let r = ClaudeExecResult {
            result_text: String::new(),
            model: String::new(),
            session_id: String::new(),
            total_cost_usd: 0.0,
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_error: true,
            error: Some("Please run /login".into()),
            tool_calls: Vec::new(),
        };
        assert!(looks_like_login_error(&r));
    }

    #[test]
    fn test_non_login_error_is_not_detected() {
        let r = ClaudeExecResult {
            result_text: String::new(),
            model: String::new(),
            session_id: String::new(),
            total_cost_usd: 0.0,
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_error: true,
            error: Some("rate limit exceeded".into()),
            tool_calls: Vec::new(),
        };
        assert!(!looks_like_login_error(&r));
    }
}
