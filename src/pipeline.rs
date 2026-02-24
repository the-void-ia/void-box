//! Pipelines compose [`VoidBox`] stages into a data-flow.
//!
//! ## Model
//! - A pipeline is an ordered list of stages.
//! - Each stage is either:
//!   - **Single**: runs one `VoidBox` sequentially.
//!   - **Parallel (fan-out)**: runs multiple `VoidBox` instances concurrently on the same input.
//!
//! ## Data passing
//! Each stage receives optional "carry" data from the previous stage:
//! - If a stage produces `file_output`, that is forwarded.
//! - Otherwise, if it produces non-empty `result_text`, that text is forwarded as bytes.
//! - Otherwise, carry becomes `None`.
//!
//! Fan-out merges all stage `result_text` values into a JSON array (`["...","..."]`) for the next stage.
//! Fan-out results are collected in completion order (not input order)
//!
//! ## Failure semantics
//! - The pipeline stops early on the first failing stage.
//! - A fan-out stops the pipeline if **any** box in the group fails.
//!
//! ## Streaming vs non-streaming
//! `run_streaming` delivers at least one output event per stage by emitting a synthetic
//! `ExecOutputChunk` from the final `result_text` (in addition to any live output produced by the VM).
//!
//! ## Observability
//! `Pipeline::observe` wraps execution with OTLP spans and metrics but shares the same core execution loop.
//!
//! ## Design notes (for contributors)
//! - Keep execution semantics centralized in `run_pipeline_core`.
//! - Avoid duplicating stage loops in `Pipeline` vs `ObservablePipeline`.
//! - If you change carry semantics or fan-out merge format, update module docs and tests.

use std::time::Instant;

use crate::agent_box::VoidBox;
use crate::guest::protocol::ExecOutputChunk;
use crate::observe::claude::{create_otel_spans, ClaudeExecResult};
use crate::observe::tracer::SpanStatus;
use crate::observe::{ObserveConfig, ObservedResult, Observer};

/// Result of running a full pipeline.
#[derive(Debug)]
pub struct PipelineResult {
    /// Name of the pipeline
    pub name: String,
    /// Results from each stage.
    /// For fan-out groups, results are appended in completion order.
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
    Single(Box<VoidBox>),
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
            stages: vec![PipelineStage::Single(Box::new(first))],
        }
    }

    /// Create a named pipeline from a single Box.
    pub fn named(name: impl Into<String>, first: VoidBox) -> Self {
        Self {
            name: name.into(),
            stages: vec![PipelineStage::Single(Box::new(first))],
        }
    }

    /// Pipe the output of the previous stage into the next Box.
    pub fn pipe(mut self, next: VoidBox) -> Self {
        self.stages.push(PipelineStage::Single(Box::new(next)));
        self
    }

    /// Run multiple Boxes concurrently on the same carry data.
    /// Their `result_text` outputs are merged into a JSON array for the next stage.
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
        let mut hook = NoopOutputHook;
        run_pipeline_core(self.name, self.stages, &mut hook, None).await
    }

    /// Execute the pipeline with a streaming callback for output chunks.
    ///
    /// Behaves like [`run()`](Self::run) but calls `on_output` for each
    /// `ExecOutputChunk` received from a stage's VM execution. The first
    /// argument is the stage (box) name, the second is the output chunk.
    ///
    /// The final `PipelineResult` is identical to what `run()` would return.
    pub async fn run_streaming<F>(self, on_output: F) -> crate::Result<PipelineResult>
    where
        F: FnMut(&str, &ExecOutputChunk) + Send,
    {
        let mut hook = StreamingOutputHook(on_output);
        run_pipeline_core(self.name, self.stages, &mut hook, None).await
    }

    /// Number of stages in the pipeline.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Check if pipeline is empty.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Attach observability to this pipeline.
    ///
    /// Returns an [`ObservablePipeline`] whose `run()` / `run_streaming()` methods
    /// create OTLP spans and metrics for every stage, then return an
    /// [`ObservedResult<PipelineResult>`].
    ///
    /// # Span hierarchy
    ///
    /// ```text
    /// pipeline:{name}                       ← root span
    /// ├── stage:{box_name}                  ← per-stage child
    /// │   └── claude.exec                   ← agent execution (via create_otel_spans)
    /// │       ├── claude.tool.Read
    /// │       └── claude.tool.Bash
    /// ├── fan_out:[a|b]                     ← parallel stage parent
    /// │   ├── stage:a → claude.exec → …
    /// │   └── stage:b → claude.exec → …
    /// └── stage:{final}
    /// ```
    pub fn observe(self, config: ObserveConfig) -> ObservablePipeline {
        ObservablePipeline {
            pipeline: self,
            observer: Observer::new(config),
        }
    }
}

// ---------------------------------------------------------------------------
// Output hook trait — abstracts "streaming" vs "non-streaming"
// ---------------------------------------------------------------------------

/// Called after each stage result to optionally emit streaming output chunks.
///
/// Implementors must be `Send` so the pipeline future can be spawned on
/// multi-threaded runtimes (e.g. `tokio::spawn`).
trait OutputHook: Send {
    fn on_stage_result(&mut self, box_name: &str, result: &StageResult);
}

/// No-op hook for `Pipeline::run()` (no streaming output).
struct NoopOutputHook;

impl OutputHook for NoopOutputHook {
    fn on_stage_result(&mut self, _box_name: &str, _result: &StageResult) {}
}

/// Hook that emits synthetic `ExecOutputChunk`s for `run_streaming()`.
struct StreamingOutputHook<F>(F);

impl<F: FnMut(&str, &ExecOutputChunk) + Send> OutputHook for StreamingOutputHook<F> {
    fn on_stage_result(&mut self, box_name: &str, result: &StageResult) {
        emit_synthetic_chunk(result, box_name, &mut self.0);
    }
}

// ---------------------------------------------------------------------------
// Core pipeline execution — single implementation for all four public methods
// ---------------------------------------------------------------------------

/// Core pipeline loop used by both plain and observed execution.
///
/// Design: streaming and observability are orthogonal concerns injected via:
/// - `OutputHook` for streaming callbacks
/// - `Option<&Observer>` for tracing/metrics
async fn run_pipeline_core(
    pipeline_name: String,
    pipeline_stages: Vec<PipelineStage>,
    output_hook: &mut dyn OutputHook,
    observer: Option<&Observer>,
) -> crate::Result<PipelineResult> {
    let total_stages = pipeline_stages.len();
    let tracer = observer.map(|o| o.tracer().clone());

    // Root span (only when observed)
    let mut root_span: Option<(crate::observe::tracer::Span, Instant)> = None;
    let mut root_ctx: Option<crate::observe::tracer::SpanContext> = None;

    if let Some(t) = tracer.as_ref() {
        let mut span = t.start_span(&format!("pipeline:{}", pipeline_name));
        span.set_attribute("pipeline.name", &pipeline_name);
        span.set_attribute("pipeline.stages", total_stages.to_string());

        root_ctx = Some(span.context.clone());
        root_span = Some((span, Instant::now()));
    }

    let mut stages: Vec<StageResult> = Vec::new();
    let mut carry_data: Option<Vec<u8>> = None;
    let mut had_pipeline_error = false;

    for (i, stage) in pipeline_stages.into_iter().enumerate() {
        match stage {
            PipelineStage::Single(agent_box) => {
                let box_name = agent_box.name.clone();
                eprintln!(
                    "[pipeline] Stage {}/{}: [vm:{}] starting ...",
                    i + 1,
                    total_stages,
                    box_name
                );

                let stage_start = Instant::now();
                let stage_result = agent_box.run(carry_data.as_deref()).await?;
                let elapsed = stage_start.elapsed();

                output_hook.on_stage_result(&box_name, &stage_result);

                if let (Some(t), Some(obs), Some(root)) =
                    (tracer.as_ref(), observer, root_ctx.as_ref())
                {
                    finish_single_stage_span(t, obs, root, &stage_result, elapsed);
                }

                carry_data = extract_carry_data(&stage_result);

                log_stage_complete(i, total_stages, &box_name, &stage_result);

                if stage_result.claude_result.is_error {
                    log_stage_error(&box_name, &stage_result.claude_result);
                    had_pipeline_error = true;
                    stages.push(stage_result);
                    break;
                }

                stages.push(stage_result);
            }
            PipelineStage::Parallel(boxes) => {
                let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
                let names_pretty = names.join(" | ");
                let names_compact = names.join("|"); // for span name

                eprintln!(
                    "[pipeline] Stage {}/{}: fan-out [{}] ({} VMs in parallel)",
                    i + 1,
                    total_stages,
                    names_pretty,
                    boxes.len()
                );

                // Optional fan-out parent span (only when observed)
                let mut fan_out_span: Option<(crate::observe::tracer::Span, Instant)> = None;
                let mut fan_out_ctx: Option<crate::observe::tracer::SpanContext> = None;

                if let (Some(t), Some(root)) = (tracer.as_ref(), root_ctx.as_ref()) {
                    let label = format!("fan_out:[{}]", names_compact);
                    let span = t.start_span_with_parent(&label, root);
                    fan_out_ctx = Some(span.context.clone());
                    fan_out_span = Some((span, Instant::now()));
                }

                let mut join_set = tokio::task::JoinSet::new();
                for agent_box in boxes {
                    let input = carry_data.clone();
                    join_set.spawn(async move { agent_box.run(input.as_deref()).await });
                }

                let mut parallel_results: Vec<StageResult> = Vec::new();
                let mut had_error = false;

                while let Some(result) = join_set.join_next().await {
                    let stage_result =
                        result.map_err(|e| crate::Error::Guest(format!("Join error: {}", e)))??;

                    output_hook.on_stage_result(&stage_result.box_name, &stage_result);

                    if let (Some(t), Some(obs), Some(fo_ctx)) =
                        (tracer.as_ref(), observer, fan_out_ctx.as_ref())
                    {
                        finish_parallel_stage_span(t, obs, fo_ctx, &stage_result);
                    }

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

                // Finish fan-out parent span (only when observed)
                if let (Some(t), Some((mut span, start))) = (tracer.as_ref(), fan_out_span.take()) {
                    span.duration = Some(start.elapsed());
                    span.status = if had_error {
                        SpanStatus::Error("fan-out had errors".into())
                    } else {
                        SpanStatus::Ok
                    };
                    t.finish_span(span);
                }

                carry_data = Some(merge_parallel_outputs(&parallel_results));
                stages.extend(parallel_results);

                if had_error {
                    eprintln!("[pipeline] Fan-out stage had errors; stopping pipeline early.");
                    had_pipeline_error = true;
                    break;
                }
            }
        }
    }

    let output = stages
        .last()
        .map(|s| s.claude_result.result_text.clone())
        .unwrap_or_default();

    if let (Some(t), Some((mut span, start))) = (tracer.as_ref(), root_span.take()) {
        span.duration = Some(start.elapsed());
        span.status = if had_pipeline_error {
            SpanStatus::Error("pipeline had errors".into())
        } else {
            SpanStatus::Ok
        };
        span.end();
        t.finish_span(span);
    }

    if observer.is_some() {
        if let Err(e) = crate::observe::flush_global_otel() {
            eprintln!("[pipeline] WARN: failed to flush OTLP exporters: {e}");
        }
    }

    Ok(PipelineResult {
        name: pipeline_name,
        stages,
        output,
    })
}

/// Create and finish the OTel span for a single (sequential) stage.
fn finish_single_stage_span(
    tracer: &crate::observe::tracer::Tracer,
    observer: &Observer,
    root_ctx: &crate::observe::tracer::SpanContext,
    stage_result: &StageResult,
    elapsed: std::time::Duration,
) {
    let mut span =
        tracer.start_span_with_parent(&format!("stage:{}", stage_result.box_name), root_ctx);
    let ctx = span.context.clone();
    instrument_stage_result(stage_result, &ctx, observer);
    set_stage_span_attrs(&mut span, stage_result);
    span.duration = Some(elapsed);
    span.status = stage_status(stage_result);
    tracer.finish_span(span);
}

/// Create and finish the OTel span for one box within a fan-out stage.
fn finish_parallel_stage_span(
    tracer: &crate::observe::tracer::Tracer,
    observer: &Observer,
    fan_out_ctx: &crate::observe::tracer::SpanContext,
    stage_result: &StageResult,
) {
    let mut span =
        tracer.start_span_with_parent(&format!("stage:{}", stage_result.box_name), fan_out_ctx);
    let ctx = span.context.clone();
    instrument_stage_result(stage_result, &ctx, observer);
    set_stage_span_attrs(&mut span, stage_result);
    span.duration = Some(std::time::Duration::from_millis(
        stage_result.claude_result.duration_ms,
    ));
    span.status = stage_status(stage_result);
    tracer.finish_span(span);
}

fn stage_status(result: &StageResult) -> SpanStatus {
    if result.claude_result.is_error {
        SpanStatus::Error(
            result
                .claude_result
                .error
                .clone()
                .unwrap_or_else(|| "stage error".into()),
        )
    } else {
        SpanStatus::Ok
    }
}

fn log_stage_complete(stage_idx: usize, total_stages: usize, box_name: &str, result: &StageResult) {
    eprintln!(
        "[pipeline] Stage {}/{}: [vm:{}] complete | {} tokens, ${:.4}",
        stage_idx + 1,
        total_stages,
        box_name,
        result.claude_result.input_tokens + result.claude_result.output_tokens,
        result.claude_result.total_cost_usd,
    );
}

/// A pipeline with observability attached.
///
/// Created via [`Pipeline::observe()`]. Wraps every stage execution with OTLP
/// spans and per-stage metrics, then returns [`ObservedResult<PipelineResult>`].
pub struct ObservablePipeline {
    pipeline: Pipeline,
    observer: Observer,
}

impl ObservablePipeline {
    /// Execute the observed pipeline, instrumenting each stage with spans and metrics.
    pub async fn run(self) -> crate::Result<ObservedResult<PipelineResult>> {
        let observer = self.observer;

        let mut hook = NoopOutputHook;
        let result = run_pipeline_core(
            self.pipeline.name,
            self.pipeline.stages,
            &mut hook,
            Some(&observer),
        )
        .await?;

        Ok(ObservedResult::new(result, &observer))
    }

    /// Execute the observed pipeline with a streaming callback for output chunks.
    pub async fn run_streaming<F>(
        self,
        on_output: F,
    ) -> crate::Result<ObservedResult<PipelineResult>>
    where
        F: FnMut(&str, &ExecOutputChunk) + Send,
    {
        let observer = self.observer;

        let mut hook = StreamingOutputHook(on_output);
        let result = run_pipeline_core(
            self.pipeline.name,
            self.pipeline.stages,
            &mut hook,
            Some(&observer),
        )
        .await?;

        Ok(ObservedResult::new(result, &observer))
    }
}

/// Create `claude.exec` + tool child spans and record per-stage metrics.
fn instrument_stage_result(
    stage_result: &StageResult,
    stage_ctx: &crate::observe::tracer::SpanContext,
    observer: &Observer,
) {
    let r = &stage_result.claude_result;
    let box_name = stage_result.box_name.as_str();

    // Create claude.exec + claude.tool.* child spans via the existing helper
    create_otel_spans(r, Some(stage_ctx), observer.tracer());

    // Record per-stage metrics with stage label
    let labels = &[("stage", box_name)];
    let metrics = observer.metrics();
    metrics.add_counter("pipeline.stage.duration_ms", r.duration_ms as f64, labels);
    metrics.add_counter("pipeline.stage.input_tokens", r.input_tokens as f64, labels);
    metrics.add_counter(
        "pipeline.stage.output_tokens",
        r.output_tokens as f64,
        labels,
    );
    metrics.add_counter("pipeline.stage.cost_usd", r.total_cost_usd, labels);
    metrics.add_counter(
        "pipeline.stage.tool_calls",
        r.tool_calls.len() as f64,
        labels,
    );
}

/// Set GenAI semantic convention attributes on a stage span.
fn set_stage_span_attrs(span: &mut crate::observe::tracer::Span, stage_result: &StageResult) {
    let r = &stage_result.claude_result;
    span.set_attribute("gen_ai.usage.input_tokens", r.input_tokens.to_string());
    span.set_attribute("gen_ai.usage.output_tokens", r.output_tokens.to_string());
    span.set_attribute("claude.total_cost_usd", format!("{:.6}", r.total_cost_usd));
    span.set_attribute("claude.tools_count", r.tool_calls.len().to_string());
    if !r.model.is_empty() {
        span.set_attribute("gen_ai.request.model", &r.model);
    }
}

/// Computes carry-forward bytes for the next stage.
///
/// Precedence: `file_output` > non-empty `result_text` > `None`.
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

/// Ensures streaming callers observe at least one event per stage even if the VM produced no chunks.
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

/// Fan-out carry format: JSON array of each stage's `result_text`.
/// (File outputs are not merged.)
/// If serialization fails, returns `[]`.
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
