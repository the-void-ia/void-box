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
    Local(LocalSandbox),
    /// Mock sandbox for testing
    Mock(MockSandbox),
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
    pub async fn exec_with_stdin(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<ExecOutput> {
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
            Err(Error::Guest(format!("Failed to read file: {}", output.stderr_str())))
        }
    }

    /// Write a file in the sandbox
    pub async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        // Use base64 encoding to handle binary data safely
        let encoded = base64_encode(content);
        let output = self.exec("sh", &["-c", &format!("echo -n '{}' | base64 -d > {}", encoded, path)]).await?;
        if output.success() {
            Ok(())
        } else {
            Err(Error::Guest(format!("Failed to write file: {}", output.stderr_str())))
        }
    }

    /// Get sandbox configuration
    pub fn config(&self) -> &SandboxConfig {
        &self.config
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

    /// Add an environment variable
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.push((key.into(), value.into()));
        self
    }

    /// Build the sandbox
    pub fn build(self) -> Result<Arc<Sandbox>> {
        let inner = match self.sandbox_type {
            SandboxType::Local => {
                let local = LocalSandbox::new(self.config.clone())?;
                SandboxInner::Local(local)
            }
            SandboxType::Mock => {
                let mock = MockSandbox::new(self.config.clone());
                SandboxInner::Mock(mock)
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
    pub async fn exec_with_stdin(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<ExecOutput> {
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
                    Ok(ExecOutput::new(Vec::new(), b"cat: file not found\n".to_vec(), 1))
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
            _ => {
                // Unknown command - simulate success with empty output
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
            }
        }
    }
}

/// Simple base64 encoding (for write_file)
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
            mock.queue_response(ExecOutput::new(
                b"custom output".to_vec(),
                Vec::new(),
                0,
            ));
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
