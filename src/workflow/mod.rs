//! Workflow Engine Module
//!
//! This module provides a functional, composable workflow engine for void-box.
//! Workflows are defined declaratively and executed in isolated sandbox environments.
//!
//! # Example
//!
//! ```no_run
//! use void_box::workflow::{Workflow, WorkflowExt};
//! use void_box::sandbox::Sandbox;
//! use void_box::observe::ObserveConfig;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let workflow = Workflow::define("data-pipeline")
//!         .step("fetch", |ctx| async move {
//!             ctx.exec("curl", &["-s", "https://api.example.com"]).await
//!         })
//!         .step("parse", |ctx| async move {
//!             // Use exec_piped to pipe from previous step
//!             ctx.exec_piped("jq", &[".data"]).await
//!         })
//!         .pipe("fetch", "parse")
//!         .build();
//!
//!     let sandbox = Sandbox::mock().build()?;
//!     let result = workflow
//!         .observe(ObserveConfig::test())
//!         .run_in(sandbox)
//!         .await?;
//!
//!     Ok(())
//! }
//! ```

pub mod composition;
pub mod context;
pub mod definition;
pub mod scheduler;

use std::collections::HashMap;
use std::sync::Arc;

pub use composition::{CompositionOp, Pipeline};
pub use context::{StepContext, StepOutput};
pub use definition::{Step, StepFn, Workflow, WorkflowBuilder};
pub use scheduler::{ExecutionPlan, Scheduler};

use crate::observe::{ObserveConfig, ObservedResult, Observer};
use crate::sandbox::Sandbox;
use crate::Result;

/// Result of executing a workflow
#[derive(Debug, Clone)]
pub struct WorkflowResult {
    /// Final output from the last step
    pub output: Vec<u8>,
    /// Exit code (0 for success)
    pub exit_code: i32,
    /// Outputs from each step
    pub step_outputs: HashMap<String, StepOutput>,
    /// Total execution duration in milliseconds
    pub duration_ms: u64,
}

impl WorkflowResult {
    /// Get output as a string
    pub fn output_str(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    /// Check if workflow succeeded
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    /// Get a specific step's output
    pub fn step_output(&self, name: &str) -> Option<&StepOutput> {
        self.step_outputs.get(name)
    }
}

/// A workflow that can be observed and executed
pub struct ObservableWorkflow {
    workflow: Workflow,
    observer: Observer,
}

impl ObservableWorkflow {
    /// Create a new observable workflow
    pub fn new(workflow: Workflow, config: ObserveConfig) -> Self {
        Self {
            workflow,
            observer: Observer::new(config),
        }
    }

    /// Run the workflow in a sandbox
    pub async fn run_in(self, sandbox: Arc<Sandbox>) -> Result<ObservedResult<WorkflowResult>> {
        let scheduler = Scheduler::new(self.observer.clone());
        let result = scheduler.execute(&self.workflow, sandbox).await?;

        Ok(ObservedResult::new(result, &self.observer))
    }

    /// Get the observer for inspection
    pub fn observer(&self) -> &Observer {
        &self.observer
    }
}

/// Extension trait for workflow to add observability
pub trait WorkflowExt {
    /// Attach observability to this workflow
    fn observe(self, config: ObserveConfig) -> ObservableWorkflow;
}

impl WorkflowExt for Workflow {
    fn observe(self, config: ObserveConfig) -> ObservableWorkflow {
        ObservableWorkflow::new(self, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_result() {
        let mut result = WorkflowResult {
            output: b"hello".to_vec(),
            exit_code: 0,
            step_outputs: HashMap::new(),
            duration_ms: 100,
        };

        result.step_outputs.insert(
            "step1".to_string(),
            StepOutput {
                stdout: b"output".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            },
        );

        assert!(result.success());
        assert_eq!(result.output_str(), "hello");
        assert!(result.step_output("step1").is_some());
        assert!(result.step_output("missing").is_none());
    }
}
