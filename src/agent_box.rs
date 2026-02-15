//! AgentBox: Skill + Environment = Box
//!
//! An `AgentBox` binds skills (MCP servers, CLI tools, procedural knowledge)
//! to an isolated execution environment (KVM micro-VM). Each Box:
//!
//! - Has a name and a purpose (prompt)
//! - Has one or more Skills installed
//! - Runs in a fresh, disposable VM
//! - Produces structured output for the next Box
//!
//! Inspired by [Ed Huang's "Box" concept](https://me.0xffff.me/agent_infra.html):
//! *"A Box exposes no execution details, has no external dependencies,
//! has no side effects, and encapsulates Skill-guided Actions +
//! a reproducible, disposable environment."*
//!
//! # Example
//!
//! ```no_run
//! use void_box::skill::Skill;
//! use void_box::agent_box::AgentBox;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let reasoning = Skill::agent("claude-code")
//!     .description("Autonomous reasoning and code execution");
//!
//! let market_data = Skill::mcp("market-data-mcp")
//!     .description("Provides OHLCV and news data for equities");
//!
//! let data_box = AgentBox::new("data_analyst")
//!     .skill(market_data)
//!     .skill(reasoning)
//!     .memory_mb(256)
//!     .prompt("Fetch 30 days of OHLCV data for AAPL, NVDA, MSFT, GOOGL")
//!     .build()?;
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use crate::llm::LlmProvider;
use crate::observe::claude::ClaudeExecOpts;
use crate::pipeline::StageResult;
use crate::sandbox::Sandbox;
use crate::skill::{Skill, SkillKind};
use crate::Result;

const CLAUDE_HOME: &str = "/home/sandbox/.claude";

/// An agent Box: Skill + Environment.
///
/// Constructed via the builder pattern with `AgentBox::new("name")`.
pub struct AgentBox {
    /// Human-readable name of this Box
    pub name: String,
    /// The prompt that defines what this Box does
    pub prompt: String,
    /// Skills installed in this Box
    pub skills: Vec<Skill>,
    /// The underlying sandbox (built lazily or eagerly)
    sandbox: Option<Arc<Sandbox>>,
    /// Builder config (before build)
    config: AgentBoxConfig,
}

/// Internal configuration before the Box is built.
#[derive(Debug, Clone)]
struct AgentBoxConfig {
    memory_mb: usize,
    vcpus: usize,
    network: bool,
    kernel: Option<PathBuf>,
    initramfs: Option<PathBuf>,
    env: Vec<(String, String)>,
    /// Path where the agent should write its output (read after execution)
    output_file: String,
    /// Whether to use mock sandbox
    mock: bool,
    /// LLM provider (default: Claude)
    llm: LlmProvider,
    /// Per-stage timeout in seconds (overrides the default vsock read timeout).
    /// `None` means use the system default (1200s / 20 minutes).
    timeout_secs: Option<u64>,
}

impl Default for AgentBoxConfig {
    fn default() -> Self {
        Self {
            memory_mb: 256,
            vcpus: 1,
            network: false,
            kernel: None,
            initramfs: None,
            env: Vec::new(),
            output_file: "/workspace/output.json".to_string(),
            mock: false,
            llm: LlmProvider::default(),
            timeout_secs: None,
        }
    }
}

impl AgentBox {
    /// Create a new AgentBox builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prompt: String::new(),
            skills: Vec::new(),
            sandbox: None,
            config: AgentBoxConfig::default(),
        }
    }

    // -- Builder methods --

    /// Add a Skill to this Box.
    pub fn skill(mut self, skill: Skill) -> Self {
        self.skills.push(skill);
        self
    }

    /// Set the prompt that defines this Box's purpose.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Set memory in MB.
    pub fn memory_mb(mut self, mb: usize) -> Self {
        self.config.memory_mb = mb;
        self
    }

    /// Set number of vCPUs.
    pub fn vcpus(mut self, count: usize) -> Self {
        self.config.vcpus = count;
        self
    }

    /// Enable or disable networking.
    pub fn network(mut self, enable: bool) -> Self {
        self.config.network = enable;
        self
    }

    /// Set kernel path.
    pub fn kernel(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.kernel = Some(path.into());
        self
    }

    /// Set initramfs path.
    pub fn initramfs(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.initramfs = Some(path.into());
        self
    }

    /// Add an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.push((key.into(), value.into()));
        self
    }

    /// Set the output file path the agent should write to.
    /// Defaults to `/workspace/output.json`.
    pub fn output_file(mut self, path: impl Into<String>) -> Self {
        self.config.output_file = path.into();
        self
    }

    /// Set the LLM provider (default: Claude).
    ///
    /// When set to `Ollama` or `Custom`, the appropriate environment variables
    /// are injected into the guest VM and networking is auto-enabled.
    ///
    /// ```no_run
    /// use void_box::llm::LlmProvider;
    /// use void_box::agent_box::AgentBox;
    ///
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let ab = AgentBox::new("local")
    ///     .llm(LlmProvider::ollama("qwen3-coder"))
    ///     .prompt("hello")
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn llm(mut self, provider: LlmProvider) -> Self {
        self.config.llm = provider;
        self
    }

    /// Set a per-stage timeout in seconds.
    ///
    /// Overrides the system default (1200s / 20 min).  Useful when running
    /// small local models that are slower or faster than the default.
    ///
    /// ```no_run
    /// # use void_box::agent_box::AgentBox;
    /// let ab = AgentBox::new("fast_box")
    ///     .timeout_secs(300) // 5 minutes
    ///     .prompt("Quick task")
    ///     .build().unwrap();
    /// ```
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.config.timeout_secs = Some(secs);
        self
    }

    /// Use a mock sandbox (for testing without KVM).
    pub fn mock(mut self) -> Self {
        self.config.mock = true;
        self
    }

    /// Build the Box, creating the underlying sandbox.
    pub fn build(mut self) -> Result<Self> {
        let sandbox = self.create_sandbox()?;
        self.sandbox = Some(sandbox);
        Ok(self)
    }

    /// Create the sandbox from the current configuration.
    fn create_sandbox(&self) -> Result<Arc<Sandbox>> {
        let mut builder = if self.config.mock {
            Sandbox::mock()
        } else {
            Sandbox::local()
        };

        // Auto-enable networking if the LLM provider needs it
        let needs_network = self.config.network || self.config.llm.requires_network();

        builder = builder
            .memory_mb(self.config.memory_mb)
            .vcpus(self.config.vcpus)
            .network(needs_network);

        if let Some(ref k) = self.config.kernel {
            builder = builder.kernel(k);
        }
        if let Some(ref i) = self.config.initramfs {
            builder = builder.initramfs(i);
        }

        // Inject LLM provider env vars first, then user overrides
        for (k, v) in self.config.llm.env_vars() {
            builder = builder.env(&k, &v);
        }
        for (k, v) in &self.config.env {
            builder = builder.env(k, v);
        }

        builder.build()
    }

    /// Provision skills into the sandbox: write SKILL.md files and MCP config.
    async fn provision_skills(&self, sandbox: &Sandbox) -> Result<()> {
        // Collect MCP servers for mcp.json
        let mut mcp_servers = serde_json::Map::new();

        for skill in &self.skills {
            match &skill.kind {
                SkillKind::File { path } => {
                    // Read local SKILL.md and write to guest
                    let content = std::fs::read(path).map_err(|e| {
                        crate::Error::Config(format!(
                            "Failed to read skill file {}: {}",
                            path.display(),
                            e
                        ))
                    })?;
                    let guest_path = format!("{}/skills/{}.md", CLAUDE_HOME, skill.name);
                    sandbox.write_file(&guest_path, &content).await?;
                    eprintln!(
                        "  [skill] Installed local skill '{}' -> {}",
                        skill.name, guest_path
                    );
                }
                SkillKind::Remote { id } => {
                    let guest_path = format!("{}/skills/{}.md", CLAUDE_HOME, skill.name);
                    match skill.fetch_remote_content().await {
                        Ok(content) => {
                            sandbox.write_file(&guest_path, content.as_bytes()).await?;
                            eprintln!(
                                "  [skill] Fetched remote skill '{}' (skills.sh/{}) -> {}",
                                skill.name, id, guest_path
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "  [skill] WARN: Failed to fetch '{}': {}. Writing fallback.",
                                skill.name, e
                            );
                            let fallback = format!(
                                "# Skill: {} (fetch failed)\n\n\
                                 Source: https://skills.sh/{}\n\n\
                                 Could not fetch: {}\n\n\
                                 Install manually: `npx skills add {}`\n",
                                skill.name, id, e, id
                            );
                            sandbox.write_file(&guest_path, fallback.as_bytes()).await?;
                        }
                    }
                }
                SkillKind::Mcp { command, args, env } => {
                    // Add to MCP config
                    let mut entry = serde_json::json!({
                        "command": command,
                        "args": args,
                    });
                    if !env.is_empty() {
                        entry["env"] = serde_json::json!(env);
                    }
                    mcp_servers.insert(skill.name.clone(), entry);
                    eprintln!(
                        "  [skill] Registered MCP server '{}' ({})",
                        skill.name, command
                    );
                }
                SkillKind::Cli { command } => {
                    eprintln!(
                        "  [skill] CLI tool '{}' available at {}",
                        skill.name, command
                    );
                    // CLI binaries are expected to be in the initramfs already
                }
                SkillKind::Agent { command } => {
                    eprintln!(
                        "  [skill] Agent '{}' ({}) will be the reasoning engine",
                        skill.name, command
                    );
                }
            }
        }

        // Write MCP config if any MCP servers were registered
        if !mcp_servers.is_empty() {
            let mcp_config = serde_json::json!({
                "mcpServers": mcp_servers
            });
            let config_str = serde_json::to_string_pretty(&mcp_config).map_err(|e| {
                crate::Error::Config(format!("Failed to serialize MCP config: {}", e))
            })?;
            sandbox
                .write_file(&format!("{}/mcp.json", CLAUDE_HOME), config_str.as_bytes())
                .await?;
            eprintln!("  [skill] Wrote MCP config to {}/mcp.json", CLAUDE_HOME);
        }

        Ok(())
    }

    /// Run this Box: provision skills, execute the agent, return the result.
    ///
    /// If `input` is provided, it's written to `/workspace/input.json` before
    /// the agent runs, and the prompt is augmented to reference it.
    pub async fn run(self, input: Option<&[u8]>) -> Result<StageResult> {
        let sandbox = self.sandbox.as_ref().ok_or_else(|| {
            crate::Error::Config("AgentBox not built -- call .build() first".into())
        })?;

        // Provision skills into the guest
        self.provision_skills(sandbox).await?;

        // Write input data if provided
        if let Some(data) = input {
            sandbox.write_file("/workspace/input.json", data).await?;
            eprintln!(
                "  [box:{}] Wrote {} bytes to /workspace/input.json",
                self.name,
                data.len()
            );
        }

        // Build the full prompt.
        // We embed the previous stage's output directly in the prompt so models
        // don't need to use file-reading tools (small local models often can't).
        // The data is still written to /workspace/input.json for models that
        // prefer tool-based file access.
        let full_prompt = if let Some(data) = input {
            let input_text = String::from_utf8_lossy(data);
            // Truncate if very large to avoid blowing context window
            let inline = if input_text.len() > 4000 {
                format!(
                    "{}...\n(truncated; full data in /workspace/input.json)",
                    &input_text[..4000]
                )
            } else {
                input_text.to_string()
            };
            format!(
                "{}\n\n--- Previous stage output ---\n{}\n--- End previous stage output ---\n\n\
                 The above data is also available at /workspace/input.json.\n\
                 Write your output to {}.",
                self.prompt, inline, self.config.output_file
            )
        } else {
            format!(
                "{}\n\nWrite your output to {}.",
                self.prompt, self.config.output_file
            )
        };

        eprintln!(
            "[box:{}] Running agent with {} ({} chars)...",
            self.name,
            self.config.llm.description(),
            full_prompt.len()
        );

        // Execute the agent
        let claude_result = sandbox
            .exec_claude(
                &full_prompt,
                ClaudeExecOpts {
                    dangerously_skip_permissions: true,
                    extra_args: self.config.llm.cli_args(),
                    timeout_secs: self.config.timeout_secs,
                    ..Default::default()
                },
            )
            .await?;

        // Try to read the output file
        let file_output = match sandbox.read_file(&self.config.output_file).await {
            Ok(data) if !data.is_empty() => {
                eprintln!(
                    "  [box:{}] Read {} bytes from {}",
                    self.name,
                    data.len(),
                    self.config.output_file
                );
                Some(data)
            }
            _ => None,
        };

        Ok(StageResult {
            box_name: self.name.clone(),
            claude_result,
            file_output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Skill;

    #[test]
    fn test_agent_box_builder() {
        let reasoning = Skill::agent("claude-code").description("Autonomous reasoning");

        let market_data = Skill::mcp("market-data-mcp").description("Market data provider");

        let ab = AgentBox::new("data_analyst")
            .skill(market_data)
            .skill(reasoning)
            .memory_mb(512)
            .prompt("Fetch OHLCV data for AAPL")
            .mock()
            .build()
            .unwrap();

        assert_eq!(ab.name, "data_analyst");
        assert_eq!(ab.skills.len(), 2);
        assert!(!ab.prompt.is_empty());
        assert!(ab.sandbox.is_some());
    }

    #[tokio::test]
    async fn test_agent_box_run_mock() {
        let reasoning = Skill::agent("claude-code");

        let ab = AgentBox::new("test_box")
            .skill(reasoning)
            .prompt("Do something")
            .mock()
            .build()
            .unwrap();

        // Mock sandbox will return default claude-code response
        let result = ab.run(None).await.unwrap();
        assert_eq!(result.box_name, "test_box");
    }
}
