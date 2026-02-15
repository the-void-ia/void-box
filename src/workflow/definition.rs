//! Workflow Definition DSL
//!
//! Provides a declarative DSL for defining workflows with steps and composition.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::composition::CompositionOp;
use super::context::StepContext;
use crate::{Error, Result};

/// Type alias for step functions
pub type StepFn =
    Arc<dyn Fn(StepContext) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>> + Send + Sync>;

/// A single step in a workflow
#[derive(Clone)]
pub struct Step {
    /// Step name (must be unique within workflow)
    pub name: String,
    /// Step function
    pub func: StepFn,
    /// Steps that must complete before this one
    pub depends_on: Vec<String>,
    /// Timeout for this step in seconds
    pub timeout_secs: Option<u64>,
    /// Retry configuration
    pub retry: Option<RetryConfig>,
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step")
            .field("name", &self.name)
            .field("depends_on", &self.depends_on)
            .field("timeout_secs", &self.timeout_secs)
            .field("retry", &self.retry)
            .finish()
    }
}

/// Retry configuration for a step
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Initial delay between retries in milliseconds
    pub initial_delay_ms: u64,
    /// Backoff multiplier
    pub backoff_multiplier: f64,
    /// Maximum delay between retries
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 100,
            backoff_multiplier: 2.0,
            max_delay_ms: 30000,
        }
    }
}

/// A complete workflow definition
#[derive(Clone)]
pub struct Workflow {
    /// Workflow name
    pub name: String,
    /// Steps in the workflow
    pub steps: HashMap<String, Step>,
    /// Composition operations (pipes, maps, etc.)
    pub compositions: Vec<CompositionOp>,
    /// Final step that produces the output
    pub output_step: Option<String>,
}

impl std::fmt::Debug for Workflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workflow")
            .field("name", &self.name)
            .field("steps", &self.steps.keys().collect::<Vec<_>>())
            .field("compositions", &self.compositions)
            .field("output_step", &self.output_step)
            .finish()
    }
}

impl Workflow {
    /// Start building a new workflow
    pub fn define(name: impl Into<String>) -> WorkflowBuilder {
        WorkflowBuilder::new(name)
    }

    /// Get the execution order based on dependencies
    pub fn execution_order(&self) -> Result<Vec<String>> {
        let mut order = Vec::new();
        let mut visited = HashMap::new();
        let mut temp_visited = HashMap::new();

        for name in self.steps.keys() {
            if !visited.contains_key(name) {
                self.topological_sort(name, &mut visited, &mut temp_visited, &mut order)?;
            }
        }

        Ok(order)
    }

    fn topological_sort(
        &self,
        name: &str,
        visited: &mut HashMap<String, bool>,
        temp_visited: &mut HashMap<String, bool>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        if temp_visited.get(name).copied().unwrap_or(false) {
            return Err(Error::Config(format!(
                "Circular dependency detected at step '{}'",
                name
            )));
        }

        if visited.get(name).copied().unwrap_or(false) {
            return Ok(());
        }

        temp_visited.insert(name.to_string(), true);

        if let Some(step) = self.steps.get(name) {
            for dep in &step.depends_on {
                self.topological_sort(dep, visited, temp_visited, order)?;
            }
        }

        temp_visited.insert(name.to_string(), false);
        visited.insert(name.to_string(), true);
        order.push(name.to_string());

        Ok(())
    }

    /// Get the output step name
    pub fn output_step(&self) -> Option<&str> {
        self.output_step.as_deref()
    }

    /// Get all step names
    pub fn step_names(&self) -> Vec<&str> {
        self.steps.keys().map(|s| s.as_str()).collect()
    }
}

/// Builder for creating workflows
pub struct WorkflowBuilder {
    name: String,
    steps: HashMap<String, Step>,
    compositions: Vec<CompositionOp>,
    output_step: Option<String>,
}

impl WorkflowBuilder {
    /// Create a new workflow builder
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: HashMap::new(),
            compositions: Vec::new(),
            output_step: None,
        }
    }

    /// Add a step to the workflow
    pub fn step<F, Fut>(mut self, name: impl Into<String>, func: F) -> Self
    where
        F: Fn(StepContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        let name = name.into();
        let func = Arc::new(move |ctx: StepContext| {
            let fut = func(ctx);
            Box::pin(fut) as Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>
        });

        self.steps.insert(
            name.clone(),
            Step {
                name,
                func,
                depends_on: Vec::new(),
                timeout_secs: None,
                retry: None,
            },
        );

        self
    }

    /// Add a step with dependencies
    pub fn step_depends<F, Fut>(
        mut self,
        name: impl Into<String>,
        depends_on: &[&str],
        func: F,
    ) -> Self
    where
        F: Fn(StepContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        let name = name.into();
        let func = Arc::new(move |ctx: StepContext| {
            let fut = func(ctx);
            Box::pin(fut) as Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>
        });

        self.steps.insert(
            name.clone(),
            Step {
                name,
                func,
                depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
                timeout_secs: None,
                retry: None,
            },
        );

        self
    }

    /// Pipe output from one step to another
    pub fn pipe(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        let from = from.into();
        let to = to.into();

        // Add dependency
        if let Some(step) = self.steps.get_mut(&to) {
            if !step.depends_on.contains(&from) {
                step.depends_on.push(from.clone());
            }
        }

        self.compositions.push(CompositionOp::Pipe { from, to });

        self
    }

    /// Set a step's timeout
    pub fn timeout(mut self, step_name: impl Into<String>, secs: u64) -> Self {
        let name = step_name.into();
        if let Some(step) = self.steps.get_mut(&name) {
            step.timeout_secs = Some(secs);
        }
        self
    }

    /// Configure retry for a step
    pub fn retry(mut self, step_name: impl Into<String>, config: RetryConfig) -> Self {
        let name = step_name.into();
        if let Some(step) = self.steps.get_mut(&name) {
            step.retry = Some(config);
        }
        self
    }

    /// Set the output step (determines final workflow output)
    pub fn output(mut self, step_name: impl Into<String>) -> Self {
        self.output_step = Some(step_name.into());
        self
    }

    /// Build the workflow
    pub fn build(mut self) -> Workflow {
        // Auto-detect output step if not specified
        if self.output_step.is_none() {
            // Find steps that no other step depends on
            let mut candidates: Vec<_> = self.steps.keys().cloned().collect();

            for step in self.steps.values() {
                for dep in &step.depends_on {
                    candidates.retain(|c| c != dep);
                }
            }

            // If there's exactly one, use it
            if candidates.len() == 1 {
                self.output_step = Some(candidates.remove(0));
            }
        }

        Workflow {
            name: self.name,
            steps: self.steps,
            compositions: self.compositions,
            output_step: self.output_step,
        }
    }
}

/// Trait for types that can be converted into a step function
pub trait IntoStepFn {
    fn into_step_fn(self) -> StepFn;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_builder() {
        let workflow = Workflow::define("test")
            .step("step1", |_ctx| async { Ok(b"hello".to_vec()) })
            .step("step2", |_ctx| async { Ok(b"world".to_vec()) })
            .pipe("step1", "step2")
            .build();

        assert_eq!(workflow.name, "test");
        assert_eq!(workflow.steps.len(), 2);
        assert!(workflow.steps.contains_key("step1"));
        assert!(workflow.steps.contains_key("step2"));
    }

    #[test]
    fn test_execution_order_simple() {
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step("b", |_ctx| async { Ok(vec![]) })
            .pipe("a", "b")
            .build();

        let order = workflow.execution_order().unwrap();
        let a_pos = order.iter().position(|s| s == "a").unwrap();
        let b_pos = order.iter().position(|s| s == "b").unwrap();
        assert!(a_pos < b_pos);
    }

    #[test]
    fn test_execution_order_complex() {
        // a -> b -> d
        // a -> c -> d
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step_depends("b", &["a"], |_ctx| async { Ok(vec![]) })
            .step_depends("c", &["a"], |_ctx| async { Ok(vec![]) })
            .step_depends("d", &["b", "c"], |_ctx| async { Ok(vec![]) })
            .build();

        let order = workflow.execution_order().unwrap();
        let a_pos = order.iter().position(|s| s == "a").unwrap();
        let b_pos = order.iter().position(|s| s == "b").unwrap();
        let c_pos = order.iter().position(|s| s == "c").unwrap();
        let d_pos = order.iter().position(|s| s == "d").unwrap();

        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[test]
    fn test_circular_dependency_detection() {
        let workflow = Workflow::define("test")
            .step_depends("a", &["b"], |_ctx| async { Ok(vec![]) })
            .step_depends("b", &["a"], |_ctx| async { Ok(vec![]) })
            .build();

        let result = workflow.execution_order();
        assert!(result.is_err());
    }

    #[test]
    fn test_auto_output_detection() {
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .step("b", |_ctx| async { Ok(vec![]) })
            .pipe("a", "b")
            .build();

        // b should be auto-detected as output since nothing depends on it
        assert_eq!(workflow.output_step, Some("b".to_string()));
    }

    #[test]
    fn test_retry_config() {
        let workflow = Workflow::define("test")
            .step("a", |_ctx| async { Ok(vec![]) })
            .retry("a", RetryConfig::default())
            .build();

        let step = workflow.steps.get("a").unwrap();
        assert!(step.retry.is_some());
        assert_eq!(step.retry.as_ref().unwrap().max_attempts, 3);
    }
}
