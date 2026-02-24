//! Sandbox Module
//!
//! Provides isolated execution environments for workflows.
//! The sandbox abstraction allows workflows to run in:
//! - Local KVM-based micro-VMs
//! - Future: Remote cloud sandboxes
//!
//! # Example
//!
//! ```no_run
//! use void_box::sandbox::Sandbox;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create a local sandbox
//!     let sandbox = Sandbox::local()
//!         .memory_mb(256)
//!         .network(true)
//!         .build()?;
//!
//!     // Execute commands
//!     let output = sandbox.exec("echo", &["hello"]).await?;
//!     println!("Output: {}", output.stdout_str());
//!
//!     Ok(())
//! }
//! ```

pub mod local;

use std::path::PathBuf;
use std::sync::Arc;

pub use local::LocalSandbox;

use crate::observe::ObserveConfig;
use crate::{Error, ExecOutput, Result};

/// Sandbox configuration
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Memory size in MB
    pub memory_mb: usize,
    /// Number of vCPUs
    pub vcpus: usize,
    /// Enable networking
    pub network: bool,
    /// Path to kernel
    pub kernel: Option<PathBuf>,
    /// Path to initramfs
    pub initramfs: Option<PathBuf>,
    /// Path to root filesystem
    pub rootfs: Option<PathBuf>,
    /// Enable vsock for communication
    pub enable_vsock: bool,
    /// Observability configuration
    pub observe: Option<ObserveConfig>,
    /// Shared directory to mount in guest
    pub shared_dir: Option<PathBuf>,
    /// Host directory mounts into the guest.
    pub mounts: Vec<crate::backend::MountConfig>,
    /// Guest path where an OCI rootfs is mounted (triggers pivot_root in guest-agent).
    pub oci_rootfs: Option<String>,
    /// Environment variables
    pub env: Vec<(String, String)>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            memory_mb: 256,
            vcpus: 1,
            network: false,
            kernel: None,
            initramfs: None,
            rootfs: None,
            enable_vsock: true,
            observe: None,
            shared_dir: None,
            mounts: Vec::new(),
            oci_rootfs: None,
            env: Vec::new(),
        }
    }
}

/// A sandbox for isolated execution
pub struct Sandbox {
    /// Sandbox configuration
    config: SandboxConfig,
    /// The underlying implementation
    inner: SandboxInner,
}

enum SandboxInner {
    /// Local KVM-based sandbox
    Local(Box<LocalSandbox>),
    /// Mock sandbox for testing
    Mock(Box<MockSandbox>),
}

impl Sandbox {
    /// Start building a local sandbox
    pub fn local() -> SandboxBuilder {
        SandboxBuilder::new(SandboxType::Local)
    }

    /// Create an ephemeral sandbox (destroyed after use)
    pub fn ephemeral() -> SandboxBuilder {
        SandboxBuilder::new(SandboxType::Local)
    }

    /// Create a mock sandbox for testing
    pub fn mock() -> SandboxBuilder {
        SandboxBuilder::new(SandboxType::Mock)
    }

    /// Execute a command in the sandbox
    pub async fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput> {
        self.exec_with_stdin(program, args, &[]).await
    }

    /// Execute a command with stdin input
    pub async fn exec_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
    ) -> Result<ExecOutput> {
        match &self.inner {
            SandboxInner::Local(local) => local.exec_with_stdin(program, args, stdin).await,
            SandboxInner::Mock(mock) => mock.exec_with_stdin(program, args, stdin).await,
        }
    }

    /// Check if a file exists in the sandbox
    pub async fn file_exists(&self, path: &str) -> Result<bool> {
        let output = self.exec("test", &["-e", path]).await?;
        Ok(output.exit_code == 0)
    }

    /// Read a file from the sandbox
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let output = self.exec("cat", &[path]).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(Error::Guest(format!(
                "Failed to read file: {}",
                output.stderr_str()
            )))
        }
    }

    /// Write a file in the sandbox using the native WriteFile protocol.
    ///
    /// This sends the file content directly to the guest-agent via vsock,
    /// which writes it in Rust without needing `sh`, `echo`, or `base64`.
    /// Parent directories are created automatically.
    pub async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        match &self.inner {
            SandboxInner::Local(local) => local.write_file_native(path, content).await,
            SandboxInner::Mock(_mock) => {
                // Mock: no-op success
                Ok(())
            }
        }
    }

    /// Create directories in the guest filesystem (mkdir -p).
    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        match &self.inner {
            SandboxInner::Local(local) => local.mkdir_p(path).await,
            SandboxInner::Mock(_mock) => Ok(()),
        }
    }

    /// Execute `claude-code` with `--output-format stream-json` and parse the result.
    ///
    /// This is a high-level wrapper that:
    /// 1. Runs `claude-code -p <prompt> --output-format stream-json`
    /// 2. Parses the JSONL stdout into structured `ClaudeExecResult`
    /// 3. Returns both the text result and full telemetry (tokens, cost, tool calls)
    ///
    /// When the `opentelemetry` feature is enabled, OTel spans are created
    /// for the execution and each tool call.
    pub async fn exec_claude(
        &self,
        prompt: &str,
        opts: crate::observe::claude::ClaudeExecOpts,
    ) -> Result<crate::observe::claude::ClaudeExecResult> {
        if let SandboxInner::Local(local) = &self.inner {
            self.verify_claude_code_compat(local, &opts.env).await?;
        }

        let mut args = vec![
            "-p".to_string(),
            prompt.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

        if opts.dangerously_skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        for extra in &opts.extra_args {
            args.push(extra.clone());
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        // Execute via the normal sandbox path
        let output = match &self.inner {
            SandboxInner::Local(local) => {
                // For local sandbox, pass extra env and timeout through
                local
                    .exec_claude_internal(&args_refs, &opts.env, opts.timeout_secs)
                    .await?
            }
            SandboxInner::Mock(mock) => {
                mock.exec_with_stdin("claude-code", &args_refs, &[]).await?
            }
        };

        // Log raw output for debugging (always at debug, stderr at warn on failure)
        {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let stdout_preview = if stdout_str.len() > 500 {
                format!("{}...", &stdout_str[..500])
            } else {
                stdout_str.to_string()
            };

            if output.exit_code != 0 {
                tracing::warn!(
                    exit_code = output.exit_code,
                    "claude-code failed; stderr={}, stdout_head={}",
                    if stderr_str.is_empty() {
                        "(empty)"
                    } else {
                        stderr_str.trim()
                    },
                    stdout_preview,
                );
            } else {
                tracing::debug!(
                    exit_code = output.exit_code,
                    stdout_len = output.stdout.len(),
                    stderr_len = output.stderr.len(),
                    "claude-code finished; stdout_head={}, stderr={}",
                    stdout_preview,
                    if stderr_str.is_empty() {
                        "(empty)"
                    } else {
                        stderr_str.trim()
                    },
                );
            }
        }

        // Parse the stream-json output even on non-zero exit codes.
        // claude-code exits 1 when the task fails or has errors, but still
        // produces valid stream-json with a result event. Only treat it as
        // a hard failure if we get NO parseable stream-json output at all.
        let result = crate::observe::claude::parse_stream_json(&output.stdout);

        let no_stream_output = result.session_id.is_empty()
            && result.model.is_empty()
            && result.result_text.is_empty()
            && result.tool_calls.is_empty()
            && result.input_tokens == 0
            && result.output_tokens == 0
            && !result.is_error;

        if no_stream_output {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let stdout_preview = if stdout_str.len() > 500 {
                format!("{}...", &stdout_str[..500])
            } else {
                stdout_str.to_string()
            };
            return Err(Error::Guest(format!(
                "claude-code returned no stream-json events (exit_code={}). stderr: {}. stdout_head: {}",
                output.exit_code,
                if stderr_str.trim().is_empty() {
                    "(empty)"
                } else {
                    stderr_str.trim()
                },
                if stdout_preview.trim().is_empty() {
                    "(empty)"
                } else {
                    stdout_preview.trim()
                }
            )));
        }

        Ok(result)
    }

    /// Execute `claude-code` with streaming output and incremental JSONL parsing.
    ///
    /// Like [`exec_claude()`](Self::exec_claude), but parses JSONL lines as
    /// they arrive from the guest VM and calls `on_event` for each tool-use
    /// event in real-time.  Returns the same `ClaudeExecResult` as the
    /// non-streaming variant.
    pub async fn exec_claude_streaming<F>(
        &self,
        prompt: &str,
        opts: crate::observe::claude::ClaudeExecOpts,
        mut on_event: F,
    ) -> Result<crate::observe::claude::ClaudeExecResult>
    where
        F: FnMut(crate::observe::claude::ClaudeStreamEvent),
    {
        use crate::observe::claude::{parse_jsonl_line, ClaudeExecResult, ClaudeStreamEvent};
        use std::collections::HashMap;

        if let SandboxInner::Local(local) = &self.inner {
            self.verify_claude_code_compat(local, &opts.env).await?;
        }

        let mut args = vec![
            "-p".to_string(),
            prompt.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

        if opts.dangerously_skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        for extra in &opts.extra_args {
            args.push(extra.clone());
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        match &self.inner {
            SandboxInner::Local(local) => {
                let (mut chunk_rx, response_rx) = local
                    .exec_claude_streaming_internal(&args_refs, &opts.env, opts.timeout_secs)
                    .await?;

                let mut state = ClaudeExecResult {
                    result_text: String::new(),
                    model: String::new(),
                    session_id: String::new(),
                    total_cost_usd: 0.0,
                    duration_ms: 0,
                    duration_api_ms: 0,
                    num_turns: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    is_error: false,
                    error: None,
                    tool_calls: Vec::new(),
                };
                let mut tool_id_map: HashMap<String, usize> = HashMap::new();
                let mut line_buf = String::new();

                // Process stdout chunks as they arrive
                while let Some(chunk) = chunk_rx.recv().await {
                    if chunk.stream != "stdout" {
                        continue;
                    }

                    let text = String::from_utf8_lossy(&chunk.data);
                    line_buf.push_str(&text);

                    // Process all complete lines in the buffer
                    while let Some(newline_pos) = line_buf.find('\n') {
                        let line: String = line_buf.drain(..=newline_pos).collect();
                        for event in parse_jsonl_line(&line, &mut state, &mut tool_id_map) {
                            on_event(event);
                        }
                    }
                }

                // Process any remaining partial line
                if !line_buf.trim().is_empty() {
                    for event in parse_jsonl_line(&line_buf, &mut state, &mut tool_id_map) {
                        on_event(event);
                    }
                }

                // Wait for the final response (for exit code / error info)
                let response = response_rx
                    .await
                    .map_err(|_| Error::Guest("Failed to receive streaming response".into()))?;

                // Log raw output on failure
                if let Ok(ref resp) = response {
                    if resp.exit_code != 0 {
                        let stderr_str = String::from_utf8_lossy(&resp.stderr);
                        tracing::warn!(
                            exit_code = resp.exit_code,
                            "claude-code failed; stderr={}",
                            if stderr_str.is_empty() {
                                "(empty)"
                            } else {
                                stderr_str.trim()
                            },
                        );
                    }
                }

                // Check for empty stream output
                let no_stream_output = state.session_id.is_empty()
                    && state.model.is_empty()
                    && state.result_text.is_empty()
                    && state.tool_calls.is_empty()
                    && state.input_tokens == 0
                    && state.output_tokens == 0
                    && !state.is_error;

                if no_stream_output {
                    let (stderr_str, exit_code, error_str) = match &response {
                        Ok(resp) => (
                            String::from_utf8_lossy(&resp.stderr).to_string(),
                            resp.exit_code,
                            resp.error.clone().unwrap_or_default(),
                        ),
                        Err(e) => (format!("{}", e), -1, String::new()),
                    };
                    return Err(Error::Guest(format!(
                        "claude-code returned no stream-json events (exit_code={}). stderr: {}. error: {}",
                        exit_code,
                        if stderr_str.trim().is_empty() { "(empty)" } else { stderr_str.trim() },
                        if error_str.trim().is_empty() { "(empty)" } else { error_str.trim() },
                    )));
                }

                Ok(state)
            }
            SandboxInner::Mock(mock) => {
                // Mock: fall back to non-streaming, emit events from batch result
                let output = mock.exec_with_stdin("claude-code", &args_refs, &[]).await?;
                let result = crate::observe::claude::parse_stream_json(&output.stdout);

                for tc in &result.tool_calls {
                    on_event(ClaudeStreamEvent::ToolUse(tc.clone()));
                }

                Ok(result)
            }
        }
    }

    /// Lightweight check that `claude-code` exists in the guest PATH.
    ///
    /// Previously this ran `claude-code --help` which booted the full Node.js
    /// runtime and wrote state to `~/.claude/`, corrupting guest config for
    /// the subsequent real execution. Now we run `sh -c "command -v claude-code"`
    /// which is side-effect-free and sub-second. We use `sh` (which is in the
    /// guest command allowlist) with the `command -v` builtin.
    async fn verify_claude_code_compat(
        &self,
        local: &LocalSandbox,
        _extra_env: &[(String, String)],
    ) -> Result<()> {
        let probe_output = local
            .exec_with_stdin("sh", &["-c", "command -v claude-code"], &[])
            .await
            .map_err(|e| {
                Error::Guest(format!(
                    "failed to probe guest for `claude-code` binary: {e}"
                ))
            })?;

        if probe_output.exit_code == 0 {
            let path = String::from_utf8_lossy(&probe_output.stdout);
            tracing::debug!("claude-code found at: {}", path.trim());
            return Ok(());
        }

        Err(Error::Guest(
            "guest does not have `claude-code` in PATH. \
Build a production guest image with claude-code and set VOID_BOX_INITRAMFS: \
`scripts/build_claude_rootfs.sh` then `export VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz`."
                .to_string(),
        ))
    }

    /// Get sandbox configuration
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// Stop the sandbox and cleanup resources gracefully
    pub async fn stop(&self) -> Result<()> {
        match &self.inner {
            SandboxInner::Local(local) => local.stop().await,
            SandboxInner::Mock(_) => Ok(()), // Mock sandbox has no cleanup needed
        }
    }
}

/// Types of sandboxes
#[derive(Debug, Clone, Copy)]
pub enum SandboxType {
    /// Local KVM-based sandbox
    Local,
    /// Mock sandbox for testing
    Mock,
}

/// Builder for creating sandboxes
pub struct SandboxBuilder {
    sandbox_type: SandboxType,
    config: SandboxConfig,
}

impl SandboxBuilder {
    /// Create a new sandbox builder
    pub fn new(sandbox_type: SandboxType) -> Self {
        Self {
            sandbox_type,
            config: SandboxConfig::default(),
        }
    }

    /// Set the memory size in MB
    pub fn memory_mb(mut self, mb: usize) -> Self {
        self.config.memory_mb = mb;
        self
    }

    /// Set the number of vCPUs
    pub fn vcpus(mut self, count: usize) -> Self {
        self.config.vcpus = count;
        self
    }

    /// Enable or disable networking
    pub fn network(mut self, enable: bool) -> Self {
        self.config.network = enable;
        self
    }

    /// Set the kernel path
    pub fn kernel(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.kernel = Some(path.into());
        self
    }

    /// Set the initramfs path
    pub fn initramfs(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.initramfs = Some(path.into());
        self
    }

    /// Set the rootfs path
    pub fn rootfs(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.rootfs = Some(path.into());
        self
    }

    /// Enable or disable vsock
    pub fn enable_vsock(mut self, enable: bool) -> Self {
        self.config.enable_vsock = enable;
        self
    }

    /// Set observability configuration
    pub fn observe(mut self, config: ObserveConfig) -> Self {
        self.config.observe = Some(config);
        self
    }

    /// Set a shared directory
    pub fn shared_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.shared_dir = Some(path.into());
        self
    }

    /// Add a host directory mount.
    pub fn mount(mut self, mount: crate::backend::MountConfig) -> Self {
        self.config.mounts.push(mount);
        self
    }

    /// Set the OCI rootfs guest path (triggers pivot_root in guest-agent).
    pub fn oci_rootfs(mut self, guest_path: impl Into<String>) -> Self {
        self.config.oci_rootfs = Some(guest_path.into());
        self
    }

    /// Add an environment variable
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.push((key.into(), value.into()));
        self
    }

    /// Use pre-built artifacts from GitHub releases
    ///
    /// Downloads kernel and initramfs artifacts from the specified version.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use void_box::sandbox::Sandbox;
    ///
    /// let sandbox = Sandbox::local()
    ///     .with_prebuilt_artifacts("v0.1.0")
    ///     .unwrap()
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn with_prebuilt_artifacts(
        mut self,
        version: &str,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let artifacts = crate::artifacts::download_prebuilt_artifacts(version)?;
        self.config.kernel = Some(artifacts.kernel);
        self.config.initramfs = Some(artifacts.initramfs);
        Ok(self)
    }

    /// Load artifacts from environment variables
    ///
    /// Checks VOID_BOX_KERNEL and VOID_BOX_INITRAMFS environment variables.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use void_box::sandbox::Sandbox;
    ///
    /// let sandbox = Sandbox::local()
    ///     .from_env()
    ///     .unwrap()
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn from_env(mut self) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let artifacts = crate::artifacts::from_env()?;
        self.config.kernel = Some(artifacts.kernel);
        self.config.initramfs = Some(artifacts.initramfs);
        Ok(self)
    }

    /// Build the sandbox
    pub fn build(self) -> Result<Arc<Sandbox>> {
        let inner = match self.sandbox_type {
            SandboxType::Local => {
                let local = LocalSandbox::new(self.config.clone())?;
                SandboxInner::Local(Box::new(local))
            }
            SandboxType::Mock => {
                let mock = MockSandbox::new(self.config.clone());
                SandboxInner::Mock(Box::new(mock))
            }
        };

        Ok(Arc::new(Sandbox {
            config: self.config,
            inner,
        }))
    }
}

/// Mock sandbox for testing
pub struct MockSandbox {
    #[allow(dead_code)]
    config: SandboxConfig,
    responses: std::sync::Mutex<Vec<ExecOutput>>,
}

impl MockSandbox {
    /// Create a new mock sandbox
    pub fn new(config: SandboxConfig) -> Self {
        Self {
            config,
            responses: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Queue a response for the next exec call
    pub fn queue_response(&self, output: ExecOutput) {
        self.responses.lock().unwrap().push(output);
    }

    /// Execute a command (returns queued response or default)
    pub async fn exec_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
    ) -> Result<ExecOutput> {
        let mut responses = self.responses.lock().unwrap();
        if let Some(response) = responses.pop() {
            return Ok(response);
        }
        drop(responses);

        // Simulate common commands
        match program {
            "echo" => {
                let output = format!("{}\n", args.join(" "));
                Ok(ExecOutput::new(output.into_bytes(), Vec::new(), 0))
            }
            "cat" => {
                if stdin.is_empty() && !args.is_empty() {
                    // Reading file - simulate not found
                    Ok(ExecOutput::new(
                        Vec::new(),
                        b"cat: file not found\n".to_vec(),
                        1,
                    ))
                } else {
                    // cat with stdin - echo it back
                    Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
                }
            }
            "tr" => {
                // Simple tr simulation for lowercase -> uppercase
                if args.len() >= 2 && args[0] == "a-z" && args[1] == "A-Z" {
                    let output: Vec<u8> = stdin
                        .iter()
                        .map(|&c| {
                            if c.is_ascii_lowercase() {
                                c.to_ascii_uppercase()
                            } else {
                                c
                            }
                        })
                        .collect();
                    Ok(ExecOutput::new(output, Vec::new(), 0))
                } else {
                    Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
                }
            }
            "test" => {
                // Simulate test command (always fail for -e in simulation)
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 1))
            }
            "sha256sum" => {
                // Simulate sha256sum (returns a fake hash)
                let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
                let output = format!("{}  -\n", hash);
                Ok(ExecOutput::new(output.into_bytes(), Vec::new(), 0))
            }
            "curl" => {
                // Simulate curl (return empty JSON response)
                Ok(ExecOutput::new(b"{}".to_vec(), Vec::new(), 0))
            }
            "jq" => {
                // Simulate jq (pass through stdin for simplicity)
                Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
            }
            "claude-code" => {
                // Mock claude-code:
                // - plan emits one JSON-like line
                // - apply reads stdin and echoes summary
                // - prompt mode (-p ... --output-format stream-json) returns deterministic JSONL
                //
                // When MOCK_CLAUDE_SCENARIO=multi_tool (or similar) env vars are set,
                // generates richer JSONL with tool calls, realistic tokens, and cost.
                let first = args.first().copied().unwrap_or("");
                if first == "-p" {
                    let output_format = args
                        .windows(2)
                        .find(|w| w[0] == "--output-format")
                        .map(|w| w[1])
                        .unwrap_or("");

                    if output_format == "stream-json" {
                        // Minimal JSONL: system event + result event (no fake tool calls)
                        let prompt_preview = args.get(1).copied().unwrap_or("").replace('"', "'");
                        let preview = &prompt_preview[..prompt_preview.len().min(120)];
                        let jsonl = format!(
                            "{}\n{}\n",
                            r#"{"type":"system","session_id":"mock_sess","model":"mock","tools":[],"cwd":"/workspace"}"#,
                            format_args!(
                                r#"{{"type":"result","subtype":"success","session_id":"mock_sess","total_cost_usd":0.0,"is_error":false,"duration_ms":1,"duration_api_ms":1,"num_turns":1,"result":"[mock] {}","usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
                                preview
                            )
                        );
                        Ok(ExecOutput::new(jsonl.into_bytes(), Vec::new(), 0))
                    } else {
                        Ok(ExecOutput::new(
                            Vec::new(),
                            b"mock claude-code: only --output-format stream-json is supported for -p mode\n"
                                .to_vec(),
                            1,
                        ))
                    }
                } else if first == "plan" {
                    let plan = r#"{"steps":[{"id":"1","action":"edit","path":"README.md"}]}"#;
                    Ok(ExecOutput::new(
                        format!("{}\n", plan).into_bytes(),
                        Vec::new(),
                        0,
                    ))
                } else if first == "apply" {
                    let lines = std::str::from_utf8(stdin)
                        .map(|s| s.lines().count())
                        .unwrap_or(0);
                    Ok(ExecOutput::new(
                        format!("Mock applied {} plan line(s).\n", lines).into_bytes(),
                        Vec::new(),
                        0,
                    ))
                } else {
                    Ok(ExecOutput::new(
                        Vec::new(),
                        b"usage: claude-code plan|apply [dir]\n".to_vec(),
                        1,
                    ))
                }
            }
            _ => {
                // Unknown command - simulate success with empty output
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
            }
        }
    }
}

/// Simple base64 encoding (kept for potential future use).
#[allow(dead_code)]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = String::new();
    let mut i = 0;

    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };

        result.push(ALPHABET[(b0 >> 2) as usize] as char);
        result.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);

        if i + 1 < data.len() {
            result.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            result.push('=');
        }

        if i + 2 < data.len() {
            result.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            result.push('=');
        }

        i += 3;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_builder() {
        let sandbox = Sandbox::mock()
            .memory_mb(512)
            .vcpus(2)
            .network(true)
            .build()
            .unwrap();

        assert_eq!(sandbox.config().memory_mb, 512);
        assert_eq!(sandbox.config().vcpus, 2);
        assert!(sandbox.config().network);
    }

    #[tokio::test]
    async fn test_mock_sandbox_exec() {
        let sandbox = Sandbox::mock().build().unwrap();

        let output = sandbox.exec("echo", &["hello", "world"]).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout_str().trim(), "hello world");
    }

    #[tokio::test]
    async fn test_mock_sandbox_queued_response() {
        let sandbox = Sandbox::mock().build().unwrap();

        // Queue a custom response
        if let SandboxInner::Mock(mock) = &sandbox.inner {
            mock.queue_response(ExecOutput::new(b"custom output".to_vec(), Vec::new(), 0));
        }

        let output = sandbox.exec("anything", &[]).await.unwrap();
        assert_eq!(output.stdout, b"custom output");
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }
}
