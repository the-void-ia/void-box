//! Local Sandbox Implementation
//!
//! Uses the platform-appropriate VM backend (KVM on Linux, VZ on macOS)
//! via the `VmmBackend` trait.

use tokio::sync::Mutex;

use super::SandboxConfig;
use crate::backend::{BackendConfig, BackendSecurityConfig, VmmBackend};
use crate::{Error, ExecOutput, Result};

/// Local sandbox backed by a real VM.
pub struct LocalSandbox {
    /// Sandbox configuration
    config: SandboxConfig,
    /// The underlying VM backend (lazily initialized)
    backend: Mutex<Option<Box<dyn VmmBackend>>>,
    /// Whether the sandbox is started
    started: std::sync::atomic::AtomicBool,
}

impl LocalSandbox {
    /// Create a new local sandbox
    pub fn new(config: SandboxConfig) -> Result<Self> {
        Ok(Self {
            config,
            backend: Mutex::new(None),
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Start the sandbox VM
    async fn ensure_started(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        if self.started.load(Ordering::SeqCst) {
            return Ok(());
        }

        let mut backend_lock = self.backend.lock().await;

        // Double-check after acquiring lock
        if backend_lock.is_some() {
            self.started.store(true, Ordering::SeqCst);
            return Ok(());
        }

        // Build backend config
        let kernel = self
            .config
            .kernel
            .clone()
            .ok_or_else(|| Error::Config("Kernel path required for local sandbox".into()))?;

        // Generate session secret
        let mut session_secret = [0u8; 32];
        getrandom::fill(&mut session_secret)
            .map_err(|e| Error::Config(format!("Failed to generate session secret: {}", e)))?;

        let backend_config = BackendConfig {
            memory_mb: self.config.memory_mb,
            vcpus: self.config.vcpus,
            kernel,
            initramfs: self.config.initramfs.clone(),
            rootfs: self.config.rootfs.clone(),
            network: self.config.network,
            enable_vsock: self.config.enable_vsock,
            shared_dir: self.config.shared_dir.clone(),
            mounts: self.config.mounts.clone(),
            oci_rootfs: self.config.oci_rootfs.clone(),
            oci_rootfs_dev: self.config.oci_rootfs_dev.clone(),
            oci_rootfs_disk: self.config.oci_rootfs_disk.clone(),
            env: self.config.env.clone(),
            security: BackendSecurityConfig {
                session_secret,
                command_allowlist: Vec::new(), // Set via provisioning
                network_deny_list: vec!["169.254.0.0/16".to_string()],
                max_connections_per_second: 50,
                max_concurrent_connections: 64,
                seccomp: true,
            },
        };

        // Create platform-appropriate backend
        let mut backend = crate::backend::create_backend();
        backend.start(backend_config).await?;

        *backend_lock = Some(backend);
        self.started.store(true, Ordering::SeqCst);

        Ok(())
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
        // For now, if VM is not configured, return a simulated response
        // This allows testing without a real VM
        if self.config.kernel.is_none() {
            return self.simulate_exec(program, args, stdin);
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;

        let env: Vec<(String, String)> = self.config.env.clone();
        backend.exec(program, args, stdin, &env, None, None).await
    }

    /// Execute a command with stdin input and an explicit timeout.
    pub async fn exec_with_options(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        if self.config.kernel.is_none() {
            return self.simulate_exec(program, args, stdin);
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;

        let env: Vec<(String, String)> = self.config.env.clone();
        backend
            .exec(program, args, stdin, &env, None, timeout_secs)
            .await
    }

    /// Simulate command execution (for testing without a real VM)
    fn simulate_exec(&self, program: &str, args: &[&str], stdin: &[u8]) -> Result<ExecOutput> {
        match program {
            "echo" => {
                let output = format!("{}\n", args.join(" "));
                Ok(ExecOutput::new(output.into_bytes(), Vec::new(), 0))
            }
            "cat" => {
                if stdin.is_empty() {
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
            "sh" => {
                // Basic shell simulation
                if args.len() >= 2 && args[0] == "-c" {
                    let cmd = args[1];
                    if cmd.starts_with("echo") {
                        let msg = cmd.strip_prefix("echo ").unwrap_or("");
                        Ok(ExecOutput::new(
                            format!("{}\n", msg).into_bytes(),
                            Vec::new(),
                            0,
                        ))
                    } else {
                        Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
                    }
                } else {
                    Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
                }
            }
            "sha256sum" => {
                // Simulate sha256sum (returns a fake hash)
                let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
                let output = format!("{}  -\n", hash);
                Ok(ExecOutput::new(output.into_bytes(), Vec::new(), 0))
            }
            "curl" => {
                // Simulate curl (return empty response)
                Ok(ExecOutput::new(b"{}".to_vec(), Vec::new(), 0))
            }
            "jq" => {
                // Simulate jq (pass through stdin)
                Ok(ExecOutput::new(stdin.to_vec(), Vec::new(), 0))
            }
            "python" | "python3" => {
                // Simulate python
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
            }
            _ => {
                // Unknown command - simulate success
                Ok(ExecOutput::new(Vec::new(), Vec::new(), 0))
            }
        }
    }

    /// Write a file to the guest filesystem using the native WriteFile protocol.
    ///
    /// This bypasses shell/base64 by sending a WriteFile message directly to
    /// the guest-agent. Parent directories are created automatically.
    /// In simulation mode (no kernel), this is a no-op success.
    pub async fn write_file_native(&self, path: &str, content: &[u8]) -> Result<()> {
        if self.config.kernel.is_none() {
            // Simulation mode -- no-op
            return Ok(());
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;
        backend.write_file(path, content).await
    }

    /// Create directories in the guest filesystem (mkdir -p).
    /// In simulation mode (no kernel), this is a no-op success.
    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        if self.config.kernel.is_none() {
            return Ok(());
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;
        backend.mkdir_p(path).await
    }

    /// Internal helper for `exec_claude` -- runs claude-code with extra env and optional timeout.
    pub(crate) async fn exec_claude_internal(
        &self,
        args: &[&str],
        extra_env: &[(String, String)],
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        if self.config.kernel.is_none() {
            return self.simulate_exec("claude-code", args, &[]);
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;

        let mut env = self.config.env.clone();
        env.extend(extra_env.iter().cloned());
        backend
            .exec("claude-code", args, &[], &env, None, timeout_secs)
            .await
    }

    /// General-purpose streaming exec.
    ///
    /// Returns a channel of `ExecOutputChunk` and a oneshot for the final
    /// `ExecResponse`.  In simulation mode (no kernel), falls back to the
    /// non-streaming path and synthesises a single stdout chunk.
    pub async fn exec_streaming(
        &self,
        program: &str,
        args: &[&str],
        timeout_secs: Option<u64>,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<crate::guest::protocol::ExecOutputChunk>,
        tokio::sync::oneshot::Receiver<Result<crate::guest::protocol::ExecResponse>>,
    )> {
        use crate::guest::protocol::{ExecOutputChunk, ExecResponse};

        if self.config.kernel.is_none() {
            let output = self.simulate_exec(program, args, &[])?;
            let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(1);
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

            if !output.stdout.is_empty() {
                let _ = chunk_tx
                    .send(ExecOutputChunk {
                        stream: "stdout".to_string(),
                        data: output.stdout.clone(),
                        seq: 0,
                    })
                    .await;
            }
            let _ = resp_tx.send(Ok(ExecResponse::success(
                output.stdout,
                output.stderr,
                output.exit_code,
                0,
            )));
            return Ok((chunk_rx, resp_rx));
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;

        let env: Vec<(String, String)> = self.config.env.clone();
        backend
            .exec_streaming(program, args, &env, None, timeout_secs)
            .await
    }

    /// Streaming variant of `exec_claude_internal`.
    ///
    /// Returns a channel of `ExecOutputChunk` and a oneshot for the final
    /// `ExecResponse`.  In simulation mode (no kernel), falls back to the
    /// non-streaming path and synthesises a single stdout chunk.
    pub(crate) async fn exec_claude_streaming_internal(
        &self,
        args: &[&str],
        extra_env: &[(String, String)],
        timeout_secs: Option<u64>,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<crate::guest::protocol::ExecOutputChunk>,
        tokio::sync::oneshot::Receiver<Result<crate::guest::protocol::ExecResponse>>,
    )> {
        use crate::guest::protocol::{ExecOutputChunk, ExecResponse};

        if self.config.kernel.is_none() {
            // Simulation mode — run synchronously, wrap in channels
            let output = self.simulate_exec("claude-code", args, &[])?;
            let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(1);
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

            if !output.stdout.is_empty() {
                let _ = chunk_tx
                    .send(ExecOutputChunk {
                        stream: "stdout".to_string(),
                        data: output.stdout.clone(),
                        seq: 0,
                    })
                    .await;
            }
            let _ = resp_tx.send(Ok(ExecResponse::success(
                output.stdout,
                output.stderr,
                output.exit_code,
                0,
            )));
            return Ok((chunk_rx, resp_rx));
        }

        self.ensure_started().await?;

        let backend_lock = self.backend.lock().await;
        let backend = backend_lock.as_ref().ok_or(Error::VmNotRunning)?;

        let mut env = self.config.env.clone();
        env.extend(extra_env.iter().cloned());
        backend
            .exec_streaming("claude-code", args, &env, None, timeout_secs)
            .await
    }

    /// Stop the sandbox
    pub async fn stop(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        let mut backend_lock = self.backend.lock().await;
        if let Some(mut backend) = backend_lock.take() {
            backend.stop().await?;
        }
        self.started.store(false, Ordering::SeqCst);

        Ok(())
    }
}

impl Drop for LocalSandbox {
    fn drop(&mut self) {
        // Backend will be stopped when dropped through its Drop impl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxConfig;

    #[tokio::test]
    async fn test_simulate_echo() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox.exec("echo", &["hello", "world"]).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout_str().trim(), "hello world");
    }

    #[tokio::test]
    async fn test_simulate_cat_stdin() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox
            .exec_with_stdin("cat", &[], b"test input")
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, b"test input");
    }

    #[tokio::test]
    async fn test_simulate_tr() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox
            .exec_with_stdin("tr", &["a-z", "A-Z"], b"hello")
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, b"HELLO");
    }

    #[tokio::test]
    async fn test_simulate_curl() {
        let config = SandboxConfig::default();
        let sandbox = LocalSandbox::new(config).unwrap();

        let output = sandbox
            .exec("curl", &["-s", "https://example.com"])
            .await
            .unwrap();
        assert!(output.success());
    }
}
