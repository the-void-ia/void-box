//! DAG Execution Scheduler
//!
//! Executes workflow steps in dependency order, respecting composition operations
//! and providing observability for each step.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::composition::resolve_pipe_input;
use super::context::{StepContext, StepContextBuilder, StepOutput};
use super::definition::Workflow;
use super::WorkflowResult;
use crate::observe::Observer;
use crate::sandbox::Sandbox;
use crate::{Error, Result};

/// Execution plan for a workflow
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    /// Steps in execution order
    pub steps: Vec<String>,
    /// Steps that can run in parallel (grouped)
    pub parallel_groups: Vec<Vec<String>>,
}

impl ExecutionPlan {
    /// Create a new execution plan from a workflow
    pub fn from_workflow(workflow: &Workflow) -> Result<Self> {
        let steps = workflow.execution_order()?;

        // For now, execute sequentially
        // TODO: Identify parallel opportunities based on dependencies
        Ok(Self {
            steps,
            parallel_groups: Vec::new(),
        })
    }
}

/// Scheduler for executing workflows
pub struct Scheduler {
    observer: Observer,
}

impl Scheduler {
    /// Create a new scheduler
    pub fn new(observer: Observer) -> Self {
        Self { observer }
    }

    /// Execute a workflow in a sandbox
    pub async fn execute(
        &self,
        workflow: &Workflow,
        sandbox: Arc<Sandbox>,
    ) -> Result<WorkflowResult> {
        let start_time = Instant::now();

        // Start workflow span
        let workflow_span = self.observer.start_workflow_span(&workflow.name);
        let workflow_ctx = workflow_span.context();

        // Get execution plan
        let plan = ExecutionPlan::from_workflow(workflow)?;

        // Track step outputs
        let mut step_outputs: HashMap<String, StepOutput> = HashMap::new();

        // Execute steps in order
        for step_name in &plan.steps {
            let step = workflow.steps.get(step_name).ok_or_else(|| {
                Error::Config(format!("Step '{}' not found in workflow", step_name))
            })?;

            // Start step span
            let mut step_span = self
                .observer
                .start_step_span(step_name, Some(&workflow_ctx));

            // Build step context
            let mut ctx_builder = StepContextBuilder::new(step_name, sandbox.clone())
                .with_outputs(step_outputs.clone());

            // Resolve pipe input if applicable
            if let Some(input) =
                resolve_pipe_input(step_name, &workflow.compositions, &step_outputs)
            {
                ctx_builder = ctx_builder.with_input(input);
            }

            let ctx = ctx_builder.build();

            // Execute step with retry if configured
            let func = step.func.clone();
            let result = if let Some(ref retry_config) = step.retry {
                self.execute_with_retry(func.clone(), ctx.clone(), retry_config.max_attempts)
                    .await
            } else {
                func(ctx).await
            };

            match result {
                Ok(output) => {
                    let step_output = StepOutput::new(output.clone(), Vec::new(), 0);
                    step_span.record_stdout(output.len());
                    step_outputs.insert(step_name.clone(), step_output);
                    step_span.set_ok();
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let step_output = StepOutput::new(Vec::new(), error_msg.as_bytes().to_vec(), 1);
                    step_span.record_stderr(error_msg.len());
                    step_outputs.insert(step_name.clone(), step_output);
                    step_span.set_error(&error_msg);

                    // Log error but continue for now
                    self.observer
                        .logger()
                        .error(&format!("Step {} failed: {}", step_name, error_msg), &[]);
                }
            }
        }

        // Get final output
        let (output, exit_code) = if let Some(output_step) = &workflow.output_step {
            if let Some(step_output) = step_outputs.get(output_step) {
                (step_output.stdout.clone(), step_output.exit_code)
            } else {
                (Vec::new(), 1)
            }
        } else {
            // Use last step's output
            if let Some(last_step) = plan.steps.last() {
                if let Some(step_output) = step_outputs.get(last_step) {
                    (step_output.stdout.clone(), step_output.exit_code)
                } else {
                    (Vec::new(), 1)
                }
            } else {
                (Vec::new(), 0)
            }
        };

        let duration_ms = start_time.elapsed().as_millis() as u64;

        workflow_span.set_ok();

        Ok(WorkflowResult {
            output,
            exit_code,
            step_outputs,
            duration_ms,
        })
    }

    async fn execute_with_retry(
        &self,
        func: super::definition::StepFn,
        ctx: StepContext,
        max_attempts: u32,
    ) -> Result<Vec<u8>> {
        let mut last_error = None;

        for attempt in 0..max_attempts {
            match func(ctx.clone()).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    self.observer.logger().warn(
                        &format!(
                            "Step {} attempt {} failed: {}",
                            ctx.step_name,
                            attempt + 1,
                            e
                        ),
                        &[("attempt", &(attempt + 1).to_string())],
                    );
                    last_error = Some(e);

                    // Wait before retry (exponential backoff could be added here)
                    if attempt + 1 < max_attempts {
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            100 * (attempt as u64 + 1),
                        ))
                        .await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::Guest("Unknown error".into())))
    }
}

/// Execute a single step (utility function for testing)
pub async fn execute_step(
    step_name: &str,
    workflow: &Workflow,
    sandbox: Arc<Sandbox>,
    inputs: HashMap<String, StepOutput>,
) -> Result<StepOutput> {
    let step = workflow
        .steps
        .get(step_name)
        .ok_or_else(|| Error::Config(format!("Step '{}' not found", step_name)))?;

    let mut ctx_builder = StepContextBuilder::new(step_name, sandbox).with_outputs(inputs.clone());

    if let Some(input) = resolve_pipe_input(step_name, &workflow.compositions, &inputs) {
        ctx_builder = ctx_builder.with_input(input);
    }

    let ctx = ctx_builder.build();
    let func = step.func.clone();
    let result = func(ctx).await?;

    Ok(StepOutput::new(result, Vec::new(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_plan() {
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step("b", |_ctx| async { Ok(vec![]) })
            .pipe("a", "b")
            .build();

        let plan = ExecutionPlan::from_workflow(&workflow).unwrap();

        // a should come before b
        let a_pos = plan.steps.iter().position(|s| s == "a").unwrap();
        let b_pos = plan.steps.iter().position(|s| s == "b").unwrap();
        assert!(a_pos < b_pos);
    }
}
