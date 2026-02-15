//! Functional Composition Operations
//!
//! Provides functional programming primitives for composing workflow steps:
//! - pipe: Connect output of one step to input of another
//! - map: Transform step outputs
//! - filter: Conditionally skip steps
//! - branch: Conditional execution paths

use std::collections::HashMap;

use super::context::StepOutput;

/// Composition operations that can be applied to workflows
#[derive(Debug, Clone)]
pub enum CompositionOp {
    /// Pipe output from one step to another
    Pipe { from: String, to: String },
    /// Map/transform output using a function
    Map {
        step: String,
        transform_name: String,
    },
    /// Filter - skip step based on condition
    Filter {
        step: String,
        condition_name: String,
    },
    /// Parallel execution of multiple steps
    Parallel { steps: Vec<String> },
    /// Conditional branching
    Branch {
        condition_step: String,
        true_branch: String,
        false_branch: String,
    },
    /// Merge multiple step outputs
    Merge { steps: Vec<String>, into: String },
}

/// A pipeline of steps with composition operations
#[derive(Debug, Clone)]
pub struct Pipeline {
    /// Ordered list of step names
    steps: Vec<String>,
    /// Composition operations
    operations: Vec<CompositionOp>,
}

impl Pipeline {
    /// Create a new empty pipeline
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            operations: Vec::new(),
        }
    }

    /// Add a step to the pipeline
    pub fn then(mut self, step: impl Into<String>) -> Self {
        let step = step.into();

        // Auto-pipe from previous step if there is one
        if let Some(prev) = self.steps.last() {
            self.operations.push(CompositionOp::Pipe {
                from: prev.clone(),
                to: step.clone(),
            });
        }

        self.steps.push(step);
        self
    }

    /// Add parallel steps
    pub fn parallel(mut self, steps: Vec<String>) -> Self {
        self.operations.push(CompositionOp::Parallel {
            steps: steps.clone(),
        });
        self.steps.extend(steps);
        self
    }

    /// Add a conditional branch
    pub fn branch(mut self, condition: &str, if_true: &str, if_false: &str) -> Self {
        self.operations.push(CompositionOp::Branch {
            condition_step: condition.to_string(),
            true_branch: if_true.to_string(),
            false_branch: if_false.to_string(),
        });
        self
    }

    /// Get the steps in order
    pub fn steps(&self) -> &[String] {
        &self.steps
    }

    /// Get the composition operations
    pub fn operations(&self) -> &[CompositionOp] {
        &self.operations
    }

    /// Check if step A should pipe to step B
    pub fn should_pipe(&self, from: &str, to: &str) -> bool {
        self.operations.iter().any(|op| match op {
            CompositionOp::Pipe { from: f, to: t } => f == from && t == to,
            _ => false,
        })
    }

    /// Get parallel step groups
    pub fn parallel_groups(&self) -> Vec<Vec<String>> {
        self.operations
            .iter()
            .filter_map(|op| match op {
                CompositionOp::Parallel { steps } => Some(steps.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper for building pipelines with a fluent API
pub struct PipelineBuilder {
    pipeline: Pipeline,
}

impl PipelineBuilder {
    /// Create a new pipeline builder
    pub fn new() -> Self {
        Self {
            pipeline: Pipeline::new(),
        }
    }

    /// Start the pipeline with a step
    pub fn start(self, step: impl Into<String>) -> Self {
        Self {
            pipeline: self.pipeline.then(step),
        }
    }

    /// Add the next step
    pub fn then(self, step: impl Into<String>) -> Self {
        Self {
            pipeline: self.pipeline.then(step),
        }
    }

    /// Build the pipeline
    pub fn build(self) -> Pipeline {
        self.pipeline
    }
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve pipe inputs for a step based on composition operations
pub fn resolve_pipe_input(
    step_name: &str,
    operations: &[CompositionOp],
    outputs: &HashMap<String, StepOutput>,
) -> Option<Vec<u8>> {
    for op in operations {
        if let CompositionOp::Pipe { from, to } = op {
            if to == step_name {
                if let Some(output) = outputs.get(from) {
                    return Some(output.stdout.clone());
                }
            }
        }
    }
    None
}

/// Get all steps that should run in parallel with the given step
pub fn get_parallel_steps(step_name: &str, operations: &[CompositionOp]) -> Vec<String> {
    for op in operations {
        if let CompositionOp::Parallel { steps } = op {
            if steps.contains(&step_name.to_string()) {
                return steps.iter().filter(|s| *s != step_name).cloned().collect();
            }
        }
    }
    Vec::new()
}

/// Check if a step should be skipped based on filter conditions
pub fn should_skip_step(
    _step_name: &str,
    _operations: &[CompositionOp],
    _outputs: &HashMap<String, StepOutput>,
) -> bool {
    // Filter conditions would be evaluated here
    // For now, always execute
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_builder() {
        let pipeline = PipelineBuilder::new()
            .start("step1")
            .then("step2")
            .then("step3")
            .build();

        assert_eq!(pipeline.steps(), &["step1", "step2", "step3"]);
        assert!(pipeline.should_pipe("step1", "step2"));
        assert!(pipeline.should_pipe("step2", "step3"));
        assert!(!pipeline.should_pipe("step1", "step3"));
    }

    #[test]
    fn test_resolve_pipe_input() {
        let operations = vec![CompositionOp::Pipe {
            from: "step1".to_string(),
            to: "step2".to_string(),
        }];

        let mut outputs = HashMap::new();
        outputs.insert(
            "step1".to_string(),
            StepOutput::new(b"hello".to_vec(), vec![], 0),
        );

        let input = resolve_pipe_input("step2", &operations, &outputs);
        assert_eq!(input, Some(b"hello".to_vec()));

        let no_input = resolve_pipe_input("step1", &operations, &outputs);
        assert_eq!(no_input, None);
    }

    #[test]
    fn test_parallel_steps() {
        let operations = vec![CompositionOp::Parallel {
            steps: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        }];

        let parallel = get_parallel_steps("a", &operations);
        assert_eq!(parallel, vec!["b".to_string(), "c".to_string()]);

        let not_parallel = get_parallel_steps("x", &operations);
        assert!(not_parallel.is_empty());
    }
}
