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
    /// Host directory mounts into the guest VM.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// OCI base image for the sandbox (e.g. "python:3.12").
    #[serde(default)]
    pub image: Option<String>,
    /// OCI image containing kernel + initramfs (e.g. "ghcr.io/the-void-ia/voidbox-guest:v0.1.0").
    /// Set to "" to disable auto-pull.
    #[serde(default)]
    pub guest_image: Option<String>,
    /// Path or hash-prefix of a snapshot to restore from.
    /// If not set, the sandbox cold-boots normally.
    #[serde(default)]
    pub snapshot: Option<String>,
}

/// Specification for a host directory mount into the guest VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Host directory path.
    pub host: String,
    /// Guest mount point.
    pub guest: String,
    /// Mount mode: "ro" (default) or "rw".
    #[serde(default = "default_mount_mode")]
    pub mode: String,
}

fn default_mount_mode() -> String {
    "ro".to_string()
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
    #[serde(default)]
    pub telemetry_buffer_size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagingSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider_bridge: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    #[default]
    Task,
    Service,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub output_file: Option<String>,
    #[serde(default)]
    pub messaging: Option<MessagingSpec>,
    #[serde(default)]
    pub mode: AgentMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSpec {
    pub boxes: Vec<PipelineBoxSpec>,
    #[serde(default)]
    pub stages: Vec<PipelineStageSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxSandboxOverride {
    #[serde(default)]
    pub memory_mb: Option<usize>,
    #[serde(default)]
    pub vcpus: Option<usize>,
    #[serde(default)]
    pub network: Option<bool>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Additional host directory mounts for this box.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// Path or hash-prefix of a snapshot to restore from (per-box override).
    #[serde(default)]
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineBoxSpec {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
    #[serde(default)]
    pub llm: Option<LlmSpec>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub sandbox: Option<BoxSandboxOverride>,
    /// Named outputs this stage produces (host directories mounted rw).
    #[serde(default)]
    pub outputs: Vec<PipelineOutputSpec>,
    /// Named inputs this stage consumes (host directories mounted ro).
    #[serde(default)]
    pub inputs: Vec<PipelineInputSpec>,
}

/// A named output directory that a pipeline stage writes to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineOutputSpec {
    /// Output name (used to reference from another stage's inputs).
    pub name: String,
    /// Guest path where the stage writes output.
    #[serde(default = "default_output_guest_path")]
    pub guest: String,
}

/// A named input directory that a pipeline stage reads from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineInputSpec {
    /// Name of the output from a previous stage.
    pub from: String,
    /// Guest path where the input is mounted.
    #[serde(default = "default_input_guest_path")]
    pub guest: String,
}

fn default_output_guest_path() -> String {
    "/workspace/output".to_string()
}

fn default_input_guest_path() -> String {
    "/workspace/input".to_string()
}

/// A skill entry in YAML — either a simple string or an OCI image object.
///
/// ```yaml
/// skills:
///   - "agent:claude-code"               # Simple string
///   - image: ghcr.io/voidbox/skill-jq   # OCI image object
///     mount: /skills/jq
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SkillEntry {
    /// Simple `<type>:<value>` string (e.g. "agent:claude-code").
    Simple(String),
    /// OCI image skill with mount configuration.
    Oci {
        image: String,
        mount: String,
        #[serde(default = "default_oci_readonly")]
        readonly: bool,
    },
    /// MCP server skill (programmatic injection, not from YAML).
    Mcp {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    },
    /// Inline skill with content provided directly (not from YAML, used programmatically).
    Inline { name: String, content: String },
}

fn default_oci_readonly() -> bool {
    true
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StepMode {
    Service,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStepSpec {
    pub name: String,
    pub run: WorkflowRunSpec,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub mode: Option<StepMode>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
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
            mounts: Vec::new(),
            image: None,
            guest_image: None,
            snapshot: None,
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
    1024
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
            if agent.mode == AgentMode::Service {
                if agent.timeout_secs.is_some() {
                    return Err(Error::Config(
                        "agent: mode: service and timeout_secs are mutually exclusive".into(),
                    ));
                }
                if agent.output_file.is_none() {
                    return Err(Error::Config(
                        "agent: mode: service requires output_file".into(),
                    ));
                }
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
            for step in &workflow.steps {
                if step.mode == Some(StepMode::Service) && step.timeout_secs.is_some() {
                    return Err(Error::Config(format!(
                        "step '{}': mode: service and timeout_secs are mutually exclusive",
                        step.name
                    )));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_step_mode_service_parses() {
        let yaml = r#"
api_version: v1
kind: workflow
name: test
workflow:
  steps:
    - name: svc
      mode: service
      run:
        program: sleep
        args: ["infinity"]
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        validate_spec(&spec).unwrap();
        let step = &spec.workflow.unwrap().steps[0];
        assert_eq!(step.mode, Some(StepMode::Service));
        assert!(step.timeout_secs.is_none());
    }

    #[test]
    fn workflow_step_timeout_secs_parses() {
        let yaml = r#"
api_version: v1
kind: workflow
name: test
workflow:
  steps:
    - name: build
      timeout_secs: 300
      run:
        program: make
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        validate_spec(&spec).unwrap();
        let step = &spec.workflow.unwrap().steps[0];
        assert!(step.mode.is_none());
        assert_eq!(step.timeout_secs, Some(300));
    }

    #[test]
    fn workflow_step_mode_service_and_timeout_rejects() {
        let yaml = r#"
api_version: v1
kind: workflow
name: test
workflow:
  steps:
    - name: bad
      mode: service
      timeout_secs: 300
      run:
        program: sleep
        args: ["infinity"]
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn agent_spec_with_messaging() {
        let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  mode: mock
agent:
  prompt: "test prompt"
  messaging:
    enabled: true
    provider_bridge: claude_channels
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        let agent = spec.agent.unwrap();
        let messaging = agent.messaging.unwrap();
        assert!(messaging.enabled);
        assert_eq!(
            messaging.provider_bridge.as_deref(),
            Some("claude_channels")
        );
    }

    #[test]
    fn agent_spec_without_messaging_defaults_to_none() {
        let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  mode: mock
agent:
  prompt: "test prompt"
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        let agent = spec.agent.unwrap();
        assert!(agent.messaging.is_none());
    }

    #[test]
    fn agent_spec_messaging_enabled_false_by_default() {
        let yaml = r#"
api_version: v1
kind: agent
name: test-agent
sandbox:
  mode: mock
agent:
  prompt: "test prompt"
  messaging: {}
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        let agent = spec.agent.unwrap();
        let messaging = agent.messaging.unwrap();
        assert!(!messaging.enabled);
        assert!(messaging.provider_bridge.is_none());
    }

    #[test]
    fn agent_mode_service_parses() {
        let yaml = r#"
api_version: v1
kind: agent
name: test
sandbox:
  mode: auto
agent:
  prompt: "run the gateway"
  output_file: /workspace/output.json
  mode: service
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        validate_spec(&spec).unwrap();
        let agent = spec.agent.unwrap();
        assert_eq!(agent.mode, AgentMode::Service);
    }

    #[test]
    fn agent_mode_defaults_to_task() {
        let yaml = r#"
api_version: v1
kind: agent
name: test
sandbox:
  mode: auto
agent:
  prompt: "do something"
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        validate_spec(&spec).unwrap();
        let agent = spec.agent.unwrap();
        assert_eq!(agent.mode, AgentMode::Task);
    }

    #[test]
    fn agent_mode_service_rejects_timeout() {
        let yaml = r#"
api_version: v1
kind: agent
name: test
sandbox:
  mode: auto
agent:
  prompt: "run"
  mode: service
  timeout_secs: 60
  output_file: /workspace/output.json
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn agent_mode_service_rejects_missing_output_file() {
        let yaml = r#"
api_version: v1
kind: agent
name: test
sandbox:
  mode: auto
agent:
  prompt: "run"
  mode: service
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("output_file"));
    }

    #[test]
    fn workflow_step_no_mode_no_timeout_parses() {
        let yaml = r#"
api_version: v1
kind: workflow
name: test
workflow:
  steps:
    - name: plain
      run:
        program: echo
        args: ["hello"]
"#;
        let spec: RunSpec = serde_yaml::from_str(yaml).unwrap();
        validate_spec(&spec).unwrap();
        let step = &spec.workflow.unwrap().steps[0];
        assert!(step.mode.is_none());
        assert!(step.timeout_secs.is_none());
    }
}
