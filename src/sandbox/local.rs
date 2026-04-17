//! Local Sandbox Implementation
//!
//! Uses the platform-appropriate VM backend (KVM on Linux, VZ on macOS)
//! via the `VmmBackend` trait.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::SandboxConfig;
use crate::backend::{BackendConfig, BackendSecurityConfig, VmmBackend};
use crate::guest::protocol::TelemetrySubscribeRequest;
use crate::observe::telemetry::{TelemetryAggregator, TelemetryBuffer};
use crate::observe::{ObserveConfig, Observer};
use crate::{Error, ExecOutput, Result};

const DEFAULT_NETWORK_DENY_LIST: &[&str] = &["169.254.0.0/16"];
const DEFAULT_MAX_CONNECTIONS_PER_SECOND: u32 = 50;
const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 64;

fn default_network_deny_list() -> Vec<String> {
    DEFAULT_NETWORK_DENY_LIST
        .iter()
        .map(|cidr| (*cidr).to_string())
        .collect()
}

/// Local sandbox backed by a real VM.
pub struct LocalSandbox {
    config: SandboxConfig,
    /// VM backend behind a Mutex for lifecycle (start/stop) and an Arc for
    /// concurrent operational access. Operational methods clone the Arc and
    /// drop the lock immediately so long-running execs don't block file RPC.
    backend: Mutex<Option<Arc<dyn VmmBackend>>>,
    started: std::sync::atomic::AtomicBool,
}

impl LocalSandbox {
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

        if backend_lock.is_some() {
            self.started.store(true, Ordering::SeqCst);
            return Ok(());
        }

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
            guest_console: self.config.guest_console.clone(),
            shared_dir: self.config.shared_dir.clone(),
            mounts: self.config.mounts.clone(),
            oci_rootfs: self.config.oci_rootfs.clone(),
            oci_rootfs_dev: self.config.oci_rootfs_dev.clone(),
            oci_rootfs_disk: self.config.oci_rootfs_disk.clone(),
            env: self.config.env.clone(),
            security: BackendSecurityConfig {
                session_secret,
                command_allowlist: Vec::new(), // Set via provisioning
                network_deny_list: default_network_deny_list(),
                max_connections_per_second: DEFAULT_MAX_CONNECTIONS_PER_SECOND,
                max_concurrent_connections: DEFAULT_MAX_CONCURRENT_CONNECTIONS,
                seccomp: true,
            },
            snapshot: self.config.snapshot.clone(),
            enable_snapshots: self.config.enable_snapshots || self.config.snapshot.is_some(),
        };

        // Create platform-appropriate backend
        let mut backend = crate::backend::create_backend();
        backend.start(backend_config).await?;

        *backend_lock = Some(Arc::from(backend));
        self.started.store(true, Ordering::SeqCst);

        Ok(())
    }

    /// Returns a cloned Arc to the backend, dropping the mutex immediately.
    async fn get_backend(&self) -> Result<Arc<dyn VmmBackend>> {
        self.ensure_started().await?;
        let lock = self.backend.lock().await;
        let Some(ref backend) = *lock else {
            return Err(Error::VmNotRunning);
        };
        Ok(Arc::clone(backend))
    }

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

        let backend = self.get_backend().await?;

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

        let backend = self.get_backend().await?;

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

        let backend = self.get_backend().await?;
        backend.write_file(path, content).await
    }

    /// Create directories in the guest filesystem (mkdir -p).
    /// In simulation mode (no kernel), this is a no-op success.
    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        if self.config.kernel.is_none() {
            return Ok(());
        }

        let backend = self.get_backend().await?;
        backend.mkdir_p(path).await
    }

    /// Returns file metadata from the guest filesystem via native RPC.
    pub(crate) async fn file_stat_native(
        &self,
        path: &str,
    ) -> Result<crate::guest::protocol::FileStatResponse> {
        let backend = self.get_backend().await?;
        backend.file_stat(path).await
    }

    /// Reads a file from the guest filesystem via native RPC.
    pub(crate) async fn read_file_native(&self, path: &str) -> Result<Vec<u8>> {
        let backend = self.get_backend().await?;
        backend.read_file_native(path).await
    }

    /// Internal helper for `exec_agent` -- runs the given binary with extra env and optional timeout.
    pub(crate) async fn exec_agent_internal(
        &self,
        binary: &str,
        args: &[&str],
        extra_env: &[(String, String)],
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        if self.config.kernel.is_none() {
            return self.simulate_exec(binary, args, &[]);
        }

        let backend = self.get_backend().await?;

        let mut env = self.config.env.clone();
        env.extend(extra_env.iter().cloned());
        backend
            .exec(binary, args, &[], &env, None, timeout_secs)
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

        let backend = self.get_backend().await?;

        let env: Vec<(String, String)> = self.config.env.clone();
        backend
            .exec_streaming(program, args, &env, None, timeout_secs)
            .await
    }

    /// Streaming variant of `exec_agent_internal`.
    ///
    /// Returns a channel of `ExecOutputChunk` and a oneshot for the final
    /// `ExecResponse`.  In simulation mode (no kernel), falls back to the
    /// non-streaming path and synthesises a single stdout chunk.
    pub(crate) async fn exec_agent_streaming_internal(
        &self,
        binary: &str,
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
            let output = self.simulate_exec(binary, args, &[])?;
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

        let backend = self.get_backend().await?;

        let mut env = self.config.env.clone();
        env.extend(extra_env.iter().cloned());
        backend
            .exec_streaming(binary, args, &env, Some("/workspace"), timeout_secs)
            .await
    }

    /// Start guest telemetry collection.
    ///
    /// Subscribes to CPU/memory/IO metrics from the guest-agent at 1s intervals.
    /// Returns the `TelemetryAggregator` that accumulates the samples.
    pub async fn start_telemetry(
        &self,
        ring_buffer: Option<TelemetryBuffer>,
    ) -> Result<Arc<TelemetryAggregator>> {
        self.ensure_started().await?;
        let mut backend_lock = self.backend.lock().await;
        let Some(ref mut arc) = *backend_lock else {
            return Err(Error::VmNotRunning);
        };
        let backend = Arc::get_mut(arc).ok_or_else(|| {
            Error::Config("cannot start telemetry: backend has concurrent users".into())
        })?;
        let observer = Observer::new(ObserveConfig::default());
        let opts = TelemetrySubscribeRequest {
            interval_ms: 1000,
            include_kernel_threads: false,
        };
        backend.start_telemetry(observer, opts, ring_buffer).await
    }

    /// Opens a PTY session on the guest via the backend.
    pub async fn attach_pty(
        &self,
        request: void_box_protocol::PtyOpenRequest,
    ) -> Result<crate::backend::pty_session::PtySession> {
        let backend = self.get_backend().await?;
        backend.attach_pty(request).await
    }

    /// Delegate auto-snapshot to the VM backend.
    pub async fn create_auto_snapshot(
        &self,
        snapshot_dir: &std::path::Path,
        config_hash: String,
    ) -> Result<()> {
        self.ensure_started().await?;
        let mut backend_lock = self.backend.lock().await;
        let Some(ref mut backend) = *backend_lock else {
            return Err(Error::VmNotRunning);
        };
        let backend_mut = Arc::get_mut(backend)
            .ok_or_else(|| Error::Config("backend has outstanding references".into()))?;
        backend_mut
            .create_auto_snapshot(snapshot_dir, config_hash)
            .await
    }

    pub async fn stop(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        let mut backend_lock = self.backend.lock().await;
        if let Some(ref mut arc) = *backend_lock {
            let Some(backend) = Arc::get_mut(arc) else {
                return Err(Error::Config(
                    "cannot stop: backend has concurrent users".into(),
                ));
            };
            backend.stop().await?;
        }
        *backend_lock = None;
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
