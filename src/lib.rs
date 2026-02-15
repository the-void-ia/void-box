//! void-box: Composable Workflow Sandbox with Native Observability
//!
//! A functional, composable workflow engine that runs in isolated sandbox
//! environments with first-class observability for agent workflows.
//!
//! # Key Features
//!
//! - **Functional Composition**: Like FP - pipe, map, filter workflows
//! - **Native Observability**: OpenTelemetry traces, metrics, logs built-in
//! - **Isolated Execution**: Reproducible, sandboxed environment
//! - **Deep Introspection**: Live span inspection and metrics
//!
//! # Example: Simple Workflow
//!
//! ```no_run
//! use void_box::{workflow::{Workflow, WorkflowExt}, sandbox::Sandbox, observe::ObserveConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Define a composable workflow
//!     let workflow = Workflow::define("data-pipeline")
//!         .step("fetch", |ctx| async move {
//!             ctx.exec("echo", &["hello"]).await
//!         })
//!         .step("process", |ctx| async move {
//!             ctx.exec_piped("tr", &["a-z", "A-Z"]).await
//!         })
//!         .pipe("fetch", "process")
//!         .build();
//!
//!     // Create sandbox and attach observability
//!     let sandbox = Sandbox::mock().build()?;
//!     let result = workflow
//!         .observe(ObserveConfig::test())
//!         .run_in(sandbox)
//!         .await?;
//!
//!     // Access traces, metrics, logs
//!     println!("Output: {}", result.result.output_str());
//!     println!("Traces: {:?}", result.traces().len());
//!
//!     Ok(())
//! }
//! ```
//!
//! # Example: Low-Level VM Access
//!
//! ```no_run
//! use void_box::{VoidBox, VoidBoxConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = VoidBoxConfig::default()
//!         .kernel("/path/to/vmlinux")
//!         .memory_mb(128);
//!
//!     let mut vbox = VoidBox::new(config).await?;
//!     let output = vbox.exec("echo", &["hello", "world"]).await?;
//!
//!     println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
//!     println!("exit code: {}", output.exit_code);
//!
//!     vbox.stop().await?;
//!     Ok(())
//! }
//! ```

// Core modules
pub mod artifacts;
pub mod devices;
pub mod error;
pub mod guest;
pub mod network;
pub mod vmm;

// Composable workflows
pub mod observe;
pub mod sandbox;
pub mod workflow;

// Skill + Environment = Box
pub mod skill;
pub mod llm;
pub mod agent_box;
pub mod pipeline;

// Re-exports for convenience
pub use error::{Error, Result};
pub use vmm::config::VoidBoxConfig;
pub use vmm::VoidBox;

// Prelude for common imports
pub mod prelude {
    pub use crate::error::{Error, Result};
    pub use crate::observe::{ObserveConfig, Observer};
    pub use crate::sandbox::{Sandbox, SandboxBuilder};
    pub use crate::workflow::{Workflow, WorkflowBuilder, WorkflowExt, WorkflowResult};
    pub use crate::skill::Skill;
    pub use crate::llm::LlmProvider;
    pub use crate::agent_box::AgentBox;
    pub use crate::pipeline::Pipeline;
    pub use crate::ExecOutput;
}

/// Output from executing a command in the VM
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Standard output from the command
    pub stdout: Vec<u8>,
    /// Standard error from the command
    pub stderr: Vec<u8>,
    /// Exit code of the command
    pub exit_code: i32,
}

impl ExecOutput {
    /// Create a new ExecOutput
    pub fn new(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: i32) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
        }
    }

    /// Get stdout as a UTF-8 string, replacing invalid characters
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Get stderr as a UTF-8 string, replacing invalid characters
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Check if the command succeeded (exit code 0)
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_output() {
        let output = ExecOutput::new(b"hello\n".to_vec(), b"error\n".to_vec(), 0);
        assert!(output.success());
        assert_eq!(output.stdout_str(), "hello\n");
        assert_eq!(output.stderr_str(), "error\n");
    }

    #[test]
    fn test_exec_output_failure() {
        let output = ExecOutput::new(vec![], b"failed\n".to_vec(), 1);
        assert!(!output.success());
    }
}
