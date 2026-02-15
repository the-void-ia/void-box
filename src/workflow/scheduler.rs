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
    /// Create a new execution plan from a workflow.
    ///
    /// Steps are grouped by "level" — steps at the same level have all their
    /// dependencies satisfied by previous levels and can run in parallel.
    pub fn from_workflow(workflow: &Workflow) -> Result<Self> {
        let steps = workflow.execution_order()?;

        // Compute the level of each step: a step's level is one more than the
        // maximum level of its dependencies. Steps with no deps are at level 0.
        let mut step_level: HashMap<String, usize> = HashMap::new();
        let mut levels: Vec<Vec<String>> = Vec::new();

        for step_name in &steps {
            let step = workflow.steps.get(step_name).ok_or_else(|| {
                Error::Config(format!("Step '{}' not found in workflow", step_name))
            })?;

            let level = if step.depends_on.is_empty() {
                0
            } else {
                step.depends_on
                    .iter()
                    .map(|dep| step_level.get(dep).copied().unwrap_or(0) + 1)
                    .max()
                    .unwrap_or(0)
            };

            step_level.insert(step_name.clone(), level);

            while levels.len() <= level {
                levels.push(Vec::new());
            }
            levels[level].push(step_name.clone());
        }

        Ok(Self {
            steps,
            parallel_groups: levels,
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

    /// Execute a workflow in a sandbox.
    ///
    /// Steps within the same parallel group (i.e. at the same dependency level)
    /// are executed concurrently via a [`tokio::task::JoinSet`]. Groups are
    /// processed in level order so that all dependencies are satisfied before
    /// a group begins.
    pub async fn execute(
        &self,
        workflow: &Workflow,
        sandbox: Arc<Sandbox>,
    ) -> Result<WorkflowResult> {
        let start_time = Instant::now();

        // Start workflow span
        let workflow_span = self.observer.start_workflow_span(&workflow.name);
        let workflow_ctx = workflow_span.context();

        // Get execution plan (with parallel groups)
        let plan = ExecutionPlan::from_workflow(workflow)?;

        // Track step outputs — shared across parallel tasks via RwLock
        let step_outputs: Arc<tokio::sync::RwLock<HashMap<String, StepOutput>>> =
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        // Execute groups in level order
        for group in &plan.parallel_groups {
            if group.len() == 1 {
                // Single step — execute directly (no JoinSet overhead)
                let step_name = &group[0];
                let step = workflow.steps.get(step_name).ok_or_else(|| {
                    Error::Config(format!("Step '{}' not found in workflow", step_name))
                })?;

                let mut step_span = self
                    .observer
                    .start_step_span(step_name, Some(&workflow_ctx));

                let outputs_snapshot = step_outputs.read().await.clone();
                let mut ctx_builder = StepContextBuilder::new(step_name, sandbox.clone())
                    .with_outputs(outputs_snapshot.clone());

                if let Some(input) =
                    resolve_pipe_input(step_name, &workflow.compositions, &outputs_snapshot)
                {
                    ctx_builder = ctx_builder.with_input(input);
                }

                let ctx = ctx_builder.build();
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
                        step_outputs
                            .write()
                            .await
                            .insert(step_name.clone(), step_output);
                        step_span.set_ok();
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        let step_output =
                            StepOutput::new(Vec::new(), error_msg.as_bytes().to_vec(), 1);
                        step_span.record_stderr(error_msg.len());
                        step_outputs
                            .write()
                            .await
                            .insert(step_name.clone(), step_output);
                        step_span.set_error(&error_msg);
                        self.observer
                            .logger()
                            .error(&format!("Step {} failed: {}", step_name, error_msg), &[]);
                    }
                }
            } else {
                // Multiple steps — run in parallel with JoinSet
                let mut join_set = tokio::task::JoinSet::new();
                let outputs_snapshot = step_outputs.read().await.clone();

                for step_name in group {
                    let step = workflow.steps.get(step_name).ok_or_else(|| {
                        Error::Config(format!("Step '{}' not found in workflow", step_name))
                    })?;

                    let name = step_name.clone();
                    let func = step.func.clone();
                    let retry = step.retry.clone();
                    let sb = sandbox.clone();
                    let compositions = workflow.compositions.clone();
                    let outputs_snap = outputs_snapshot.clone();
                    let observer = self.observer.clone();
                    let wf_ctx = workflow_ctx.clone();

                    join_set.spawn(async move {
                        let mut step_span = observer.start_step_span(&name, Some(&wf_ctx));

                        let mut ctx_builder =
                            StepContextBuilder::new(&name, sb).with_outputs(outputs_snap.clone());

                        if let Some(input) = resolve_pipe_input(&name, &compositions, &outputs_snap)
                        {
                            ctx_builder = ctx_builder.with_input(input);
                        }

                        let ctx = ctx_builder.build();
                        let result = if let Some(ref retry_config) = retry {
                            // Inline retry logic since we can't call &self methods
                            let mut last_error = None;
                            let mut res = Err(Error::Guest("Unknown error".into()));
                            for attempt in 0..retry_config.max_attempts {
                                match func(ctx.clone()).await {
                                    Ok(r) => {
                                        res = Ok(r);
                                        last_error = None;
                                        break;
                                    }
                                    Err(e) => {
                                        last_error = Some(e);
                                        if attempt + 1 < retry_config.max_attempts {
                                            tokio::time::sleep(tokio::time::Duration::from_millis(
                                                100 * (attempt as u64 + 1),
                                            ))
                                            .await;
                                        }
                                    }
                                }
                            }
                            if let Some(e) = last_error {
                                res = Err(e);
                            }
                            res
                        } else {
                            func(ctx).await
                        };

                        let step_output = match result {
                            Ok(output) => {
                                step_span.record_stdout(output.len());
                                step_span.set_ok();
                                StepOutput::new(output, Vec::new(), 0)
                            }
                            Err(e) => {
                                let error_msg = e.to_string();
                                step_span.record_stderr(error_msg.len());
                                step_span.set_error(&error_msg);
                                observer
                                    .logger()
                                    .error(&format!("Step {} failed: {}", name, error_msg), &[]);
                                StepOutput::new(Vec::new(), error_msg.as_bytes().to_vec(), 1)
                            }
                        };

                        (name, step_output)
                    });
                }

                // Collect results from all parallel tasks
                while let Some(result) = join_set.join_next().await {
                    let (name, output) =
                        result.map_err(|e| Error::Guest(format!("Join error: {}", e)))?;
                    step_outputs.write().await.insert(name, output);
                }
            }
        }

        // Get final output
        let outputs = step_outputs.read().await;
        let (output, exit_code) = if let Some(output_step) = &workflow.output_step {
            if let Some(step_output) = outputs.get(output_step) {
                (step_output.stdout.clone(), step_output.exit_code)
            } else {
                (Vec::new(), 1)
            }
        } else {
            if let Some(last_step) = plan.steps.last() {
                if let Some(step_output) = outputs.get(last_step) {
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
            step_outputs: outputs.clone(),
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

    #[test]
    fn test_parallel_groups_simple_pipe() {
        // a -> b: two levels, no parallelism
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step("b", |_ctx| async { Ok(vec![]) })
            .pipe("a", "b")
            .build();

        let plan = ExecutionPlan::from_workflow(&workflow).unwrap();
        assert_eq!(plan.parallel_groups.len(), 2);
        assert_eq!(plan.parallel_groups[0], vec!["a"]);
        assert_eq!(plan.parallel_groups[1], vec!["b"]);
    }

    #[test]
    fn test_parallel_groups_diamond() {
        // a -> b -> d
        // a -> c -> d
        // b and c should be in the same parallel group
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step_depends("b", &["a"], |_ctx| async { Ok(vec![]) })
            .step_depends("c", &["a"], |_ctx| async { Ok(vec![]) })
            .step_depends("d", &["b", "c"], |_ctx| async { Ok(vec![]) })
            .build();

        let plan = ExecutionPlan::from_workflow(&workflow).unwrap();
        assert_eq!(plan.parallel_groups.len(), 3);

        // Level 0: a
        assert_eq!(plan.parallel_groups[0], vec!["a"]);
        // Level 1: b and c (in some order)
        assert_eq!(plan.parallel_groups[1].len(), 2);
        assert!(plan.parallel_groups[1].contains(&"b".to_string()));
        assert!(plan.parallel_groups[1].contains(&"c".to_string()));
        // Level 2: d
        assert_eq!(plan.parallel_groups[2], vec!["d"]);
    }

    #[test]
    fn test_parallel_groups_independent() {
        // a, b, c with no deps — all at level 0
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step("b", |_ctx| async { Ok(vec![]) })
            .step("c", |_ctx| async { Ok(vec![]) })
            .build();

        let plan = ExecutionPlan::from_workflow(&workflow).unwrap();
        assert_eq!(plan.parallel_groups.len(), 1);
        assert_eq!(plan.parallel_groups[0].len(), 3);
    }
}
