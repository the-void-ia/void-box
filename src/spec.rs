use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    #[serde(default = "default_api_version")]
    pub api_version: String,
    pub kind: RunKind,
    pub name: String,
    #[serde(default)]
    pub sandbox: SandboxSpec,
    #[serde(default)]
    pub llm: Option<LlmSpec>,
    #[serde(default)]
    pub observe: Option<ObserveSpec>,
    #[serde(default)]
    pub agent: Option<AgentSpec>,
    #[serde(default)]
    pub pipeline: Option<PipelineSpec>,
    #[serde(default)]
    pub workflow: Option<WorkflowSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Agent,
    Pipeline,
    Workflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub initramfs: Option<String>,
    #[serde(default = "default_memory")]
    pub memory_mb: usize,
    #[serde(default = "default_vcpus")]
    pub vcpus: usize,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSpec {
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveSpec {
    #[serde(default)]
    pub traces: bool,
    #[serde(default)]
    pub metrics: bool,
    #[serde(default)]
    pub logs: bool,
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    #[serde(default)]
    pub service_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub output_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSpec {
    pub boxes: Vec<PipelineBoxSpec>,
    #[serde(default)]
    pub stages: Vec<PipelineStageSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineBoxSpec {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub llm: Option<LlmSpec>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PipelineStageSpec {
    Box { name: String },
    FanOut { boxes: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub steps: Vec<WorkflowStepSpec>,
    #[serde(default)]
    pub output_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStepSpec {
    pub name: String,
    pub run: WorkflowRunSpec,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin_from: Option<String>,
}

impl Default for SandboxSpec {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            kernel: None,
            initramfs: None,
            memory_mb: default_memory(),
            vcpus: default_vcpus(),
            network: false,
            env: HashMap::new(),
        }
    }
}

fn default_api_version() -> String {
    "v1".to_string()
}

fn default_sandbox_mode() -> String {
    "auto".to_string()
}

fn default_memory() -> usize {
    512
}

fn default_vcpus() -> usize {
    1
}

pub fn load_spec(path: &Path) -> Result<RunSpec> {
    let raw = fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("failed to read {}: {}", path.display(), e)))?;

    let is_yaml = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
        .unwrap_or(false);

    let spec: RunSpec = if is_yaml {
        serde_yaml::from_str(&raw)
            .map_err(|e| Error::Config(format!("invalid YAML spec {}: {}", path.display(), e)))?
    } else {
        serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("invalid JSON spec {}: {}", path.display(), e)))?
    };

    validate_spec(&spec)?;
    Ok(spec)
}

pub fn validate_spec(spec: &RunSpec) -> Result<()> {
    if spec.api_version != "v1" {
        return Err(Error::Config(format!(
            "unsupported api_version '{}', expected 'v1'",
            spec.api_version
        )));
    }

    match spec.kind {
        RunKind::Agent => {
            let Some(agent) = &spec.agent else {
                return Err(Error::Config("kind=agent requires 'agent' section".into()));
            };
            if agent.prompt.trim().is_empty() {
                return Err(Error::Config("agent.prompt cannot be empty".into()));
            }
        }
        RunKind::Pipeline => {
            let Some(pipeline) = &spec.pipeline else {
                return Err(Error::Config(
                    "kind=pipeline requires 'pipeline' section".into(),
                ));
            };
            if pipeline.boxes.is_empty() {
                return Err(Error::Config("pipeline.boxes cannot be empty".into()));
            }
        }
        RunKind::Workflow => {
            let Some(workflow) = &spec.workflow else {
                return Err(Error::Config(
                    "kind=workflow requires 'workflow' section".into(),
                ));
            };
            if workflow.steps.is_empty() {
                return Err(Error::Config("workflow.steps cannot be empty".into()));
            }
        }
    }

    Ok(())
}
