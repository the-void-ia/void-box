//! Pipeline: Compose Boxes into sequential data-flow pipelines.
//!
//! A Pipeline chains multiple [`VoidBox`] instances so that the output of one
//! becomes the input of the next.  Each Box boots a fresh, isolated VM, runs its
//! agent with the provisioned skills, and produces structured output.
//!
//! # Example
//!
//! ```no_run
//! use void_box::pipeline::Pipeline;
//! # use void_box::agent_box::VoidBox;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! # let data_box: VoidBox = todo!();
//! # let quant_box: VoidBox = todo!();
//! # let strategy_box: VoidBox = todo!();
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

use crate::agent_box::VoidBox;
use crate::guest::protocol::ExecOutputChunk;
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

/// A single stage in a pipeline — either one Box or multiple Boxes in parallel.
enum PipelineStage {
    /// A single Box executed sequentially.
    Single(VoidBox),
    /// Multiple Boxes executed in parallel (fan-out). Their outputs are merged
    /// as a JSON array for the next stage.
    Parallel(Vec<VoidBox>),
}

/// A composable pipeline of Boxes.
pub struct Pipeline {
    name: String,
    stages: Vec<PipelineStage>,
}

impl Pipeline {
    /// Start a pipeline from a single Box.
    pub fn from(first: VoidBox) -> Self {
        Self {
            name: first.name.clone(),
            stages: vec![PipelineStage::Single(first)],
        }
    }

    /// Create a named pipeline from a single Box.
    pub fn named(name: impl Into<String>, first: VoidBox) -> Self {
        Self {
            name: name.into(),
            stages: vec![PipelineStage::Single(first)],
        }
    }

    /// Pipe the output of the previous stage into the next Box.
    pub fn pipe(mut self, next: VoidBox) -> Self {
        self.stages.push(PipelineStage::Single(next));
        self
    }

    /// Fan out: run multiple Boxes in parallel on the same input.
    ///
    /// All Boxes in the group receive the same carry-forward data from the
    /// previous stage. Their outputs are merged into a JSON array that becomes
    /// the input for the next stage.
    pub fn fan_out(mut self, boxes: Vec<VoidBox>) -> Self {
        self.stages.push(PipelineStage::Parallel(boxes));
        self
    }

    /// Execute the pipeline: run each stage in order, piping output forward.
    ///
    /// For `PipelineStage::Single` stages, a single Box is booted and run.
    /// For `PipelineStage::Parallel` stages, all Boxes are run concurrently
    /// via a [`tokio::task::JoinSet`] and their outputs are merged as a JSON
    /// array for the next stage.
    pub async fn run(self) -> crate::Result<PipelineResult> {
        let mut stages: Vec<StageResult> = Vec::new();
        let mut carry_data: Option<Vec<u8>> = None;
        let total_stages = self.stages.len();

        for (i, stage) in self.stages.into_iter().enumerate() {
            match stage {
                PipelineStage::Single(agent_box) => {
                    let box_name = agent_box.name.clone();
                    eprintln!(
                        "[pipeline] Stage {}/{}: [vm:{}] starting ...",
                        i + 1,
                        total_stages,
                        box_name
                    );

                    let stage_result = agent_box.run(carry_data.as_deref()).await?;

                    carry_data = extract_carry_data(&stage_result);

                    eprintln!(
                        "[pipeline] Stage {}/{}: [vm:{}] complete | {} tokens, ${:.4}",
                        i + 1,
                        total_stages,
                        box_name,
                        stage_result.claude_result.input_tokens
                            + stage_result.claude_result.output_tokens,
                        stage_result.claude_result.total_cost_usd,
                    );

                    if stage_result.claude_result.is_error {
                        log_stage_error(&box_name, &stage_result.claude_result);
                        stages.push(stage_result);
                        break;
                    }

                    stages.push(stage_result);
                }
                PipelineStage::Parallel(boxes) => {
                    let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
                    eprintln!(
                        "[pipeline] Stage {}/{}: fan-out [{}] ({} VMs in parallel)",
                        i + 1,
                        total_stages,
                        names.join(" | "),
                        boxes.len()
                    );

                    let mut join_set = tokio::task::JoinSet::new();
                    for agent_box in boxes {
                        let input = carry_data.clone();
                        join_set.spawn(async move { agent_box.run(input.as_deref()).await });
                    }

                    let mut parallel_results: Vec<StageResult> = Vec::new();
                    let mut had_error = false;
                    while let Some(result) = join_set.join_next().await {
                        let stage_result = result
                            .map_err(|e| crate::Error::Guest(format!("Join error: {}", e)))??;

                        eprintln!(
                            "[pipeline]   [vm:{}] fan-out complete | {} tokens, ${:.4}",
                            stage_result.box_name,
                            stage_result.claude_result.input_tokens
                                + stage_result.claude_result.output_tokens,
                            stage_result.claude_result.total_cost_usd,
                        );

                        if stage_result.claude_result.is_error {
                            log_stage_error(&stage_result.box_name, &stage_result.claude_result);
                            had_error = true;
                        }

                        parallel_results.push(stage_result);
                    }

                    // Merge outputs as a JSON array for the next stage
                    carry_data = Some(merge_parallel_outputs(&parallel_results));
                    stages.extend(parallel_results);

                    if had_error {
                        eprintln!("[pipeline] Fan-out stage had errors; stopping pipeline early.");
                        break;
                    }
                }
            }
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

    /// Execute the pipeline with a streaming callback for output chunks.
    ///
    /// Behaves like [`run()`](Self::run) but calls `on_output` for each
    /// `ExecOutputChunk` received from a stage's VM execution. The first
    /// argument is the stage (box) name, the second is the output chunk.
    ///
    /// The final `PipelineResult` is identical to what `run()` would return.
    pub async fn run_streaming<F>(self, mut on_output: F) -> crate::Result<PipelineResult>
    where
        F: FnMut(&str, &ExecOutputChunk),
    {
        let mut stages: Vec<StageResult> = Vec::new();
        let mut carry_data: Option<Vec<u8>> = None;
        let total_stages = self.stages.len();

        for (i, stage) in self.stages.into_iter().enumerate() {
            match stage {
                PipelineStage::Single(agent_box) => {
                    let box_name = agent_box.name.clone();
                    eprintln!(
                        "[pipeline] Stage {}/{}: [vm:{}] starting ...",
                        i + 1,
                        total_stages,
                        box_name
                    );

                    // TODO: When VoidBox gains streaming exec support, use it
                    // here to call on_output(box_name, chunk) for each chunk.
                    let stage_result = agent_box.run(carry_data.as_deref()).await?;

                    // Emit a synthetic chunk with the full result text so
                    // callers always see at least one output event per stage.
                    emit_synthetic_chunk(&stage_result, &box_name, &mut on_output);

                    carry_data = extract_carry_data(&stage_result);

                    eprintln!(
                        "[pipeline] Stage {}/{}: [vm:{}] complete | {} tokens, ${:.4}",
                        i + 1,
                        total_stages,
                        box_name,
                        stage_result.claude_result.input_tokens
                            + stage_result.claude_result.output_tokens,
                        stage_result.claude_result.total_cost_usd,
                    );

                    if stage_result.claude_result.is_error {
                        log_stage_error(&box_name, &stage_result.claude_result);
                        stages.push(stage_result);
                        break;
                    }

                    stages.push(stage_result);
                }
                PipelineStage::Parallel(boxes) => {
                    let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
                    eprintln!(
                        "[pipeline] Stage {}/{}: fan-out [{}] ({} VMs in parallel)",
                        i + 1,
                        total_stages,
                        names.join(" | "),
                        boxes.len()
                    );

                    let mut join_set = tokio::task::JoinSet::new();
                    for agent_box in boxes {
                        let input = carry_data.clone();
                        join_set.spawn(async move { agent_box.run(input.as_deref()).await });
                    }

                    let mut parallel_results: Vec<StageResult> = Vec::new();
                    let mut had_error = false;
                    while let Some(result) = join_set.join_next().await {
                        let stage_result = result
                            .map_err(|e| crate::Error::Guest(format!("Join error: {}", e)))??;

                        emit_synthetic_chunk(
                            &stage_result,
                            &stage_result.box_name.clone(),
                            &mut on_output,
                        );

                        if stage_result.claude_result.is_error {
                            log_stage_error(&stage_result.box_name, &stage_result.claude_result);
                            had_error = true;
                        }

                        parallel_results.push(stage_result);
                    }

                    carry_data = Some(merge_parallel_outputs(&parallel_results));
                    stages.extend(parallel_results);

                    if had_error {
                        break;
                    }
                }
            }
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

    /// Number of stages in the pipeline.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Check if pipeline is empty.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

/// Extract carry-forward data from a stage result.
fn extract_carry_data(result: &StageResult) -> Option<Vec<u8>> {
    if result.file_output.is_some() {
        result.file_output.clone()
    } else if !result.claude_result.result_text.is_empty() {
        Some(result.claude_result.result_text.as_bytes().to_vec())
    } else {
        None
    }
}

/// Log a stage error with appropriate messaging.
fn log_stage_error(box_name: &str, result: &ClaudeExecResult) {
    if looks_like_login_error(result) {
        eprintln!(
            "[pipeline] [vm:{}] FAILED: agent authentication error. \
Run `claude-code /login` in the guest image (or configure OLLAMA_MODEL) and retry.",
            box_name
        );
    } else {
        eprintln!(
            "[pipeline] [vm:{}] FAILED: {}",
            box_name,
            result.error.as_deref().unwrap_or("unknown error"),
        );
    }
}

/// Emit a synthetic ExecOutputChunk for callers that want at least one event per stage.
fn emit_synthetic_chunk<F>(result: &StageResult, box_name: &str, on_output: &mut F)
where
    F: FnMut(&str, &ExecOutputChunk),
{
    if !result.claude_result.result_text.is_empty() {
        on_output(
            box_name,
            &ExecOutputChunk {
                stream: "stdout".to_string(),
                data: result.claude_result.result_text.as_bytes().to_vec(),
                seq: 0,
            },
        );
    }
}

/// Merge parallel stage outputs into a JSON array.
fn merge_parallel_outputs(results: &[StageResult]) -> Vec<u8> {
    let texts: Vec<&str> = results
        .iter()
        .map(|r| r.claude_result.result_text.as_str())
        .collect();
    serde_json::to_vec(&texts).unwrap_or_else(|_| b"[]".to_vec())
}

fn looks_like_login_error(result: &ClaudeExecResult) -> bool {
    let text = format!(
        "{} {}",
        result.result_text.to_ascii_lowercase(),
        result.error.as_deref().unwrap_or("").to_ascii_lowercase()
    );
    text.contains("not logged in") || text.contains("/login")
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
