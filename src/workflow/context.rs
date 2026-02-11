//! Step Execution Context
//!
//! Provides the execution context for workflow steps, including:
//! - Access to previous step outputs
//! - Sandbox execution methods
//! - Input data and environment

use std::collections::HashMap;
use std::sync::Arc;

use crate::sandbox::Sandbox;
use crate::{Error, ExecOutput, Result};

/// Output from a step execution
#[derive(Debug, Clone)]
pub struct StepOutput {
    /// Standard output
    pub stdout: Vec<u8>,
    /// Standard error
    pub stderr: Vec<u8>,
    /// Exit code
    pub exit_code: i32,
}

impl StepOutput {
    /// Create a new step output
    pub fn new(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: i32) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
        }
    }

    /// Create from ExecOutput
    pub fn from_exec_output(output: ExecOutput) -> Self {
        Self {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
        }
    }

    /// Get stdout as string
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Get stderr as string
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Check if step succeeded
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Context for executing a workflow step
#[derive(Clone)]
pub struct StepContext {
    /// Current step name
    pub step_name: String,
    /// Sandbox for execution
    sandbox: Arc<Sandbox>,
    /// Outputs from previous steps
    previous_outputs: Arc<HashMap<String, StepOutput>>,
    /// Input data for this step (from piped step)
    input: Option<Vec<u8>>,
    /// Environment variables
    env: HashMap<String, String>,
    /// Working directory
    working_dir: Option<String>,
}

impl StepContext {
    /// Create a new step context
    pub fn new(
        step_name: impl Into<String>,
        sandbox: Arc<Sandbox>,
        previous_outputs: HashMap<String, StepOutput>,
    ) -> Self {
        Self {
            step_name: step_name.into(),
            sandbox,
            previous_outputs: Arc::new(previous_outputs),
            input: None,
            env: HashMap::new(),
            working_dir: None,
        }
    }

    /// Set the input data for this step
    pub fn with_input(mut self, input: Vec<u8>) -> Self {
        self.input = Some(input);
        self
    }

    /// Set environment variables
    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Set working directory
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Get the output from a previous step
    pub fn output(&self, step_name: &str) -> Option<&StepOutput> {
        self.previous_outputs.get(step_name)
    }

    /// Get the input data (from piped step)
    pub fn input(&self) -> Option<&[u8]> {
        self.input.as_deref()
    }

    /// Get the previous step's output (for piped workflows)
    pub fn prev(&self) -> Option<&[u8]> {
        self.input.as_deref()
    }

    /// Execute a command in the sandbox
    pub async fn exec(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        let output = self.sandbox.exec(program, args).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(Error::Guest(format!(
                "Command failed with exit code {}: {}",
                output.exit_code,
                output.stderr_str()
            )))
        }
    }

    /// Execute a command with stdin
    pub async fn exec_with_stdin(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
        let output = self.sandbox.exec_with_stdin(program, args, stdin).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(Error::Guest(format!(
                "Command failed with exit code {}: {}",
                output.exit_code,
                output.stderr_str()
            )))
        }
    }

    /// Execute a command piping input from previous step
    pub async fn exec_piped(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        let stdin = self.input.as_deref().unwrap_or(&[]);
        self.exec_with_stdin(program, args, stdin).await
    }

    /// Execute a raw command (returns full output including exit code)
    pub async fn exec_raw(&self, program: &str, args: &[&str]) -> Result<ExecOutput> {
        self.sandbox.exec(program, args).await
    }

    /// Execute a raw command with stdin
    pub async fn exec_raw_with_stdin(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<ExecOutput> {
        self.sandbox.exec_with_stdin(program, args, stdin).await
    }

    /// Get the sandbox reference
    pub fn sandbox(&self) -> &Arc<Sandbox> {
        &self.sandbox
    }
}

/// Builder for creating step contexts (used by scheduler)
pub struct StepContextBuilder {
    step_name: String,
    sandbox: Arc<Sandbox>,
    previous_outputs: HashMap<String, StepOutput>,
    input: Option<Vec<u8>>,
    env: HashMap<String, String>,
    working_dir: Option<String>,
}

impl StepContextBuilder {
    /// Create a new builder
    pub fn new(step_name: impl Into<String>, sandbox: Arc<Sandbox>) -> Self {
        Self {
            step_name: step_name.into(),
            sandbox,
            previous_outputs: HashMap::new(),
            input: None,
            env: HashMap::new(),
            working_dir: None,
        }
    }

    /// Add a previous step output
    pub fn with_output(mut self, step_name: impl Into<String>, output: StepOutput) -> Self {
        self.previous_outputs.insert(step_name.into(), output);
        self
    }

    /// Set all previous outputs
    pub fn with_outputs(mut self, outputs: HashMap<String, StepOutput>) -> Self {
        self.previous_outputs = outputs;
        self
    }

    /// Set the input data
    pub fn with_input(mut self, input: Vec<u8>) -> Self {
        self.input = Some(input);
        self
    }

    /// Set environment variables
    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Set working directory
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Build the context
    pub fn build(self) -> StepContext {
        StepContext {
            step_name: self.step_name,
            sandbox: self.sandbox,
            previous_outputs: Arc::new(self.previous_outputs),
            input: self.input,
            env: self.env,
            working_dir: self.working_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_step_output() {
        let output = StepOutput::new(b"hello\n".to_vec(), b"error\n".to_vec(), 0);
        assert!(output.success());
        assert_eq!(output.stdout_str(), "hello\n");
        assert_eq!(output.stderr_str(), "error\n");
    }

    #[test]
    fn test_step_output_failure() {
        let output = StepOutput::new(vec![], b"failed".to_vec(), 1);
        assert!(!output.success());
    }
}
