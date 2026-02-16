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

    /// Provision security configuration into the guest.
    ///
    /// Writes resource limits and command allowlist as JSON files that
    /// the guest-agent reads at connection time.
    async fn provision_security(&self, sandbox: &Sandbox) -> Result<()> {
        let tag = &self.name;

        // Write resource limits
        let limits = serde_json::json!({
            "max_virtual_memory": 4096 * 1024 * 1024_u64,
            "max_open_files": 1024_u64,
            "max_processes": 64_u64,
            "max_file_size": 100 * 1024 * 1024_u64,
        });
        let limits_json = serde_json::to_string_pretty(&limits).map_err(|e| {
            crate::Error::Config(format!("Failed to serialize resource limits: {}", e))
        })?;
        sandbox.mkdir_p("/etc/voidbox").await?;
        sandbox
            .write_file("/etc/voidbox/resource_limits.json", limits_json.as_bytes())
            .await?;
        eprintln!(
            "[vm:{}] Wrote resource limits to /etc/voidbox/resource_limits.json",
            tag,
        );

        // Write command allowlist
        let allowlist: Vec<&str> = crate::vmm::config::DEFAULT_COMMAND_ALLOWLIST.to_vec();
        let allowlist_json = serde_json::to_string_pretty(&allowlist).map_err(|e| {
            crate::Error::Config(format!("Failed to serialize command allowlist: {}", e))
        })?;
        sandbox
            .write_file(
                "/etc/voidbox/allowed_commands.json",
                allowlist_json.as_bytes(),
            )
            .await?;
        eprintln!(
            "[vm:{}] Wrote command allowlist ({} commands) to /etc/voidbox/allowed_commands.json",
            tag,
            allowlist.len(),
        );

        Ok(())
    }

    /// Provision skills into the sandbox: write SKILL.md files and MCP config.
    async fn provision_skills(&self, sandbox: &Sandbox) -> Result<()> {
        let tag = &self.name;

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
                        "[vm:{}] Installing skill '{}' ({}) -> {}",
                        tag,
                        skill.name,
                        skill
                            .description_text
                            .as_deref()
                            .unwrap_or("no description"),
                        guest_path
                    );
                }
                SkillKind::Remote { id } => {
                    let guest_path = format!("{}/skills/{}.md", CLAUDE_HOME, skill.name);
                    eprintln!(
                        "[vm:{}] Fetching remote skill '{}' from skills.sh/{}",
                        tag, skill.name, id
                    );
                    match skill.fetch_remote_content().await {
                        Ok(content) => {
                            sandbox.write_file(&guest_path, content.as_bytes()).await?;
                            eprintln!(
                                "[vm:{}] Installed remote skill '{}' -> {}",
                                tag, skill.name, guest_path
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "[vm:{}] WARN: Failed to fetch skill '{}': {}. Writing fallback.",
                                tag, skill.name, e
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
                        "[vm:{}] Registering MCP server '{}' (cmd: {}, args: {:?})",
                        tag, skill.name, command, args
                    );
                }
                SkillKind::Cli { command } => {
                    eprintln!(
                        "[vm:{}] CLI tool '{}' available at {}",
                        tag, skill.name, command
                    );
                    // CLI binaries are expected to be in the initramfs already
                }
                SkillKind::Agent { command } => {
                    eprintln!(
                        "[vm:{}] Reasoning engine: {} ({})",
                        tag, skill.name, command
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
            eprintln!(
                "[vm:{}] Wrote MCP config ({} servers) to {}/mcp.json",
                tag,
                mcp_servers.len(),
                CLAUDE_HOME
            );
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

        // Provision security configuration (resource limits, command allowlist)
        self.provision_security(sandbox).await?;

        // Provision skills into the guest
        self.provision_skills(sandbox).await?;

        let tag = &self.name;

        // Write input data if provided
        if let Some(data) = input {
            sandbox.write_file("/workspace/input.json", data).await?;
            eprintln!(
                "[vm:{}] Writing input ({} bytes) to /workspace/input.json",
                tag,
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
            "[vm:{}] Executing agent | llm={} | prompt_len={} chars",
            tag,
            self.config.llm.description(),
            full_prompt.len()
        );

        // Execute the agent
        let mut claude_result = sandbox
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

        // Local providers (Ollama) have no real API cost; claude-code
        // still reports a dollar amount using Anthropic pricing, so zero it.
        if self.config.llm.is_local() {
            claude_result.total_cost_usd = 0.0;
        }

        eprintln!(
            "[vm:{}] Agent finished | tokens={}in/{}out | tools={} | cost=${:.4} | error={}",
            tag,
            claude_result.input_tokens,
            claude_result.output_tokens,
            claude_result.tool_calls.len(),
            claude_result.total_cost_usd,
            claude_result.is_error,
        );

        // Log tool calls if any
        for tc in &claude_result.tool_calls {
            eprintln!("[vm:{}]   tool: {}({})", tag, tc.tool_name, tc.tool_use_id);
        }

        // Try to read the output file
        let file_output = match sandbox.read_file(&self.config.output_file).await {
            Ok(data) if !data.is_empty() => {
                eprintln!(
                    "[vm:{}] Reading output ({} bytes) from {}",
                    tag,
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
