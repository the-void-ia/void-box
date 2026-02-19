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

                    // Tool events stream in real-time via AgentBox::run() → exec_claude_streaming().
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
        let pipeline_name = self.pipeline.name;
        let pipeline_stages = self.pipeline.stages;
        let total_stages = pipeline_stages.len();
        let tracer = observer.tracer().clone();

        // Root span: pipeline:{name}
        let mut root_span = tracer.start_span(&format!("pipeline:{}", pipeline_name));
        root_span.set_attribute("pipeline.name", &pipeline_name);
        root_span.set_attribute("pipeline.stages", total_stages.to_string());
        let root_ctx = root_span.context.clone();

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

                    let mut stage_span = tracer.start_span_with_parent(
                        &format!("stage:{}", box_name),
                        &root_ctx,
                    );
                    let stage_ctx = stage_span.context.clone();
                    let stage_start = Instant::now();

                    let stage_result = agent_box.run(carry_data.as_deref()).await?;
                    let elapsed = stage_start.elapsed();

                    // Instrument: create claude.exec + tool spans, record metrics
                    instrument_stage_result(
                        &stage_result,
                        &stage_ctx,
                        &observer,
                    );

                    // Set stage span attributes and status
                    set_stage_span_attrs(&mut stage_span, &stage_result);
                    stage_span.duration = Some(elapsed);
                    if stage_result.claude_result.is_error {
                        stage_span.status = SpanStatus::Error(
                            stage_result
                                .claude_result
                                .error
                                .clone()
                                .unwrap_or_else(|| "stage error".into()),
                        );
                    } else {
                        stage_span.status = SpanStatus::Ok;
                    }
                    tracer.finish_span(stage_span);

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
                        had_pipeline_error = true;
                        stages.push(stage_result);
                        break;
                    }

                    stages.push(stage_result);
                }
                PipelineStage::Parallel(boxes) => {
                    let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
                    let fan_out_label = format!("fan_out:[{}]", names.join("|"));
                    eprintln!(
                        "[pipeline] Stage {}/{}: fan-out [{}] ({} VMs in parallel)",
                        i + 1,
                        total_stages,
                        names.join(" | "),
                        boxes.len()
                    );

                    let mut fan_out_span =
                        tracer.start_span_with_parent(&fan_out_label, &root_ctx);
                    let fan_out_ctx = fan_out_span.context.clone();
                    let fan_out_start = Instant::now();

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

                        // Create stage span under fan_out
                        let mut stage_span = tracer.start_span_with_parent(
                            &format!("stage:{}", stage_result.box_name),
                            &fan_out_ctx,
                        );
                        let stage_ctx = stage_span.context.clone();

                        instrument_stage_result(
                            &stage_result,
                            &stage_ctx,
                            &observer,
                        );

                        set_stage_span_attrs(&mut stage_span, &stage_result);
                        let stage_duration = std::time::Duration::from_millis(
                            stage_result.claude_result.duration_ms,
                        );
                        stage_span.duration = Some(stage_duration);
                        if stage_result.claude_result.is_error {
                            stage_span.status = SpanStatus::Error(
                                stage_result
                                    .claude_result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "stage error".into()),
                            );
                        } else {
                            stage_span.status = SpanStatus::Ok;
                        }
                        tracer.finish_span(stage_span);

                        eprintln!(
                            "[pipeline]   [vm:{}] fan-out complete | {} tokens, ${:.4}",
                            stage_result.box_name,
                            stage_result.claude_result.input_tokens
                                + stage_result.claude_result.output_tokens,
                            stage_result.claude_result.total_cost_usd,
                        );

                        if stage_result.claude_result.is_error {
                            log_stage_error(
                                &stage_result.box_name,
                                &stage_result.claude_result,
                            );
                            had_error = true;
                        }

                        parallel_results.push(stage_result);
                    }

                    fan_out_span.duration = Some(fan_out_start.elapsed());
                    fan_out_span.status = if had_error {
                        SpanStatus::Error("fan-out had errors".into())
                    } else {
                        SpanStatus::Ok
                    };
                    tracer.finish_span(fan_out_span);

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

        root_span.status = if had_pipeline_error {
            SpanStatus::Error("pipeline had errors".into())
        } else {
            SpanStatus::Ok
        };
        root_span.end();
        tracer.finish_span(root_span);

        if let Err(e) = crate::observe::flush_global_otel() {
            eprintln!("[pipeline] WARN: failed to flush OTLP exporters: {e}");
        }

        let result = PipelineResult {
            name: pipeline_name,
            stages,
            output,
        };

        Ok(ObservedResult::new(result, &observer))
    }

    /// Execute the observed pipeline with a streaming callback for output chunks.
    pub async fn run_streaming<F>(
        self,
        mut on_output: F,
    ) -> crate::Result<ObservedResult<PipelineResult>>
    where
        F: FnMut(&str, &ExecOutputChunk),
    {
        let observer = self.observer;
        let pipeline_name = self.pipeline.name;
        let pipeline_stages = self.pipeline.stages;
        let total_stages = pipeline_stages.len();
        let tracer = observer.tracer().clone();

        let mut root_span = tracer.start_span(&format!("pipeline:{}", pipeline_name));
        root_span.set_attribute("pipeline.name", &pipeline_name);
        root_span.set_attribute("pipeline.stages", total_stages.to_string());
        let root_ctx = root_span.context.clone();

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

                    let mut stage_span = tracer.start_span_with_parent(
                        &format!("stage:{}", box_name),
                        &root_ctx,
                    );
                    let stage_ctx = stage_span.context.clone();
                    let stage_start = Instant::now();

                    let stage_result = agent_box.run(carry_data.as_deref()).await?;
                    let elapsed = stage_start.elapsed();

                    emit_synthetic_chunk(&stage_result, &box_name, &mut on_output);

                    instrument_stage_result(
                        &stage_result,
                        &stage_ctx,
                        &observer,
                    );

                    set_stage_span_attrs(&mut stage_span, &stage_result);
                    stage_span.duration = Some(elapsed);
                    if stage_result.claude_result.is_error {
                        stage_span.status = SpanStatus::Error(
                            stage_result
                                .claude_result
                                .error
                                .clone()
                                .unwrap_or_else(|| "stage error".into()),
                        );
                    } else {
                        stage_span.status = SpanStatus::Ok;
                    }
                    tracer.finish_span(stage_span);

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
                        had_pipeline_error = true;
                        stages.push(stage_result);
                        break;
                    }

                    stages.push(stage_result);
                }
                PipelineStage::Parallel(boxes) => {
                    let names: Vec<&str> = boxes.iter().map(|b| b.name.as_str()).collect();
                    let fan_out_label = format!("fan_out:[{}]", names.join("|"));
                    eprintln!(
                        "[pipeline] Stage {}/{}: fan-out [{}] ({} VMs in parallel)",
                        i + 1,
                        total_stages,
                        names.join(" | "),
                        boxes.len()
                    );

                    let mut fan_out_span =
                        tracer.start_span_with_parent(&fan_out_label, &root_ctx);
                    let fan_out_ctx = fan_out_span.context.clone();
                    let fan_out_start = Instant::now();

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

                        let mut stage_span = tracer.start_span_with_parent(
                            &format!("stage:{}", stage_result.box_name),
                            &fan_out_ctx,
                        );
                        let stage_ctx = stage_span.context.clone();

                        instrument_stage_result(
                            &stage_result,
                            &stage_ctx,
                            &observer,
                        );

                        set_stage_span_attrs(&mut stage_span, &stage_result);
                        let stage_duration = std::time::Duration::from_millis(
                            stage_result.claude_result.duration_ms,
                        );
                        stage_span.duration = Some(stage_duration);
                        if stage_result.claude_result.is_error {
                            stage_span.status = SpanStatus::Error(
                                stage_result
                                    .claude_result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "stage error".into()),
                            );
                        } else {
                            stage_span.status = SpanStatus::Ok;
                        }
                        tracer.finish_span(stage_span);

                        if stage_result.claude_result.is_error {
                            log_stage_error(
                                &stage_result.box_name,
                                &stage_result.claude_result,
                            );
                            had_error = true;
                        }

                        parallel_results.push(stage_result);
                    }

                    fan_out_span.duration = Some(fan_out_start.elapsed());
                    fan_out_span.status = if had_error {
                        SpanStatus::Error("fan-out had errors".into())
                    } else {
                        SpanStatus::Ok
                    };
                    tracer.finish_span(fan_out_span);

                    carry_data = Some(merge_parallel_outputs(&parallel_results));
                    stages.extend(parallel_results);

                    if had_error {
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

        root_span.status = if had_pipeline_error {
            SpanStatus::Error("pipeline had errors".into())
        } else {
            SpanStatus::Ok
        };
        root_span.end();
        tracer.finish_span(root_span);

        if let Err(e) = crate::observe::flush_global_otel() {
            eprintln!("[pipeline] WARN: failed to flush OTLP exporters: {e}");
        }

        let result = PipelineResult {
            name: pipeline_name,
            stages,
            output,
        };

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
