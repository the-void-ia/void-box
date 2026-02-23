//! Backend abstraction for VM execution.
//!
//! This module defines the [`VmmBackend`] trait that all VM backends must
//! implement. The trait captures the full lifecycle: boot, exec, file I/O,
//! telemetry, and teardown.
//!
//! Platform-specific backends:
//! - **Linux**: [`KvmBackend`](kvm::KvmBackend) — KVM micro-VMs with virtio-mmio devices
//! - **macOS**: `VzBackend` — Apple Virtualization.framework

pub mod control_channel;

#[cfg(target_os = "linux")]
pub mod kvm;

#[cfg(target_os = "macos")]
pub mod vz;

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::guest::protocol::{ExecOutputChunk, ExecResponse, TelemetrySubscribeRequest};
use crate::observe::telemetry::TelemetryAggregator;
use crate::observe::tracer::SpanContext;
use crate::observe::Observer;
use crate::ExecOutput;

/// Configuration passed to [`VmmBackend::start`].
///
/// This is a backend-agnostic description of what the caller wants.
/// Each backend maps it to its own platform-specific configuration.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Memory size in megabytes.
    pub memory_mb: usize,
    /// Number of vCPUs.
    pub vcpus: usize,
    /// Path to the kernel image.
    pub kernel: PathBuf,
    /// Path to initramfs (optional).
    pub initramfs: Option<PathBuf>,
    /// Path to root filesystem image (optional).
    pub rootfs: Option<PathBuf>,
    /// Enable networking.
    pub network: bool,
    /// Enable vsock for host-guest communication.
    pub enable_vsock: bool,
    /// Host directory to share with guest (virtiofs on macOS, future on Linux).
    pub shared_dir: Option<PathBuf>,
    /// Environment variables to inject into guest commands.
    pub env: Vec<(String, String)>,
    /// Security configuration.
    pub security: BackendSecurityConfig,
}

/// Security-relevant settings for the backend.
#[derive(Debug, Clone)]
pub struct BackendSecurityConfig {
    /// 32-byte session secret for vsock authentication.
    pub session_secret: [u8; 32],
    /// Command allowlist for the guest.
    pub command_allowlist: Vec<String>,
    /// Network deny list in CIDR notation.
    pub network_deny_list: Vec<String>,
    /// Maximum new TCP connections per second.
    pub max_connections_per_second: u32,
    /// Maximum concurrent TCP connections.
    pub max_concurrent_connections: usize,
    /// Whether to install seccomp-bpf (Linux only, ignored on macOS).
    pub seccomp: bool,
}

/// Trait that all VM backends must implement.
///
/// Each method corresponds to a host→guest operation. The control channel
/// (vsock protocol) is encapsulated inside the backend — callers interact
/// at the `exec` / `write_file` level, not raw sockets.
#[async_trait::async_trait]
pub trait VmmBackend: Send + Sync {
    /// Boot the VM with the given configuration.
    async fn start(&mut self, config: BackendConfig) -> Result<()>;

    /// Execute a command in the guest and wait for the response.
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
        env: &[(String, String)],
        working_dir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput>;

    /// Execute a command with streaming output chunks.
    ///
    /// Returns a channel of `ExecOutputChunk` and a oneshot for the final response.
    async fn exec_streaming(
        &self,
        program: &str,
        args: &[&str],
        env: &[(String, String)],
        working_dir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<ExecOutputChunk>,
        tokio::sync::oneshot::Receiver<Result<ExecResponse>>,
    )>;

    /// Write a file to the guest filesystem.
    async fn write_file(&self, path: &str, content: &[u8]) -> Result<()>;

    /// Create directories in the guest filesystem (mkdir -p).
    async fn mkdir_p(&self, path: &str) -> Result<()>;

    /// Start a telemetry subscription from the guest.
    async fn start_telemetry(
        &mut self,
        observer: Observer,
        opts: TelemetrySubscribeRequest,
    ) -> Result<Arc<TelemetryAggregator>>;

    /// Set the active span context for TRACEPARENT propagation.
    fn set_span_context(&mut self, ctx: SpanContext);

    /// Check if the VM is running.
    fn is_running(&self) -> bool;

    /// Stop the VM and clean up resources.
    async fn stop(&mut self) -> Result<()>;

    /// Get the vsock CID for this VM.
    fn cid(&self) -> u32;
}

/// Default command allowlist for guest execution.
///
/// Shared across all backends. Previously lived in `vmm::config`.
pub const DEFAULT_COMMAND_ALLOWLIST: &[&str] = &[
    "sh",
    "bash",
    "claude-code",
    "claude",
    "python3",
    "pip",
    "git",
    "gh",
    "curl",
    "wget",
    "jq",
    "cat",
    "ls",
    "mkdir",
    "cp",
    "mv",
    "rm",
    "chmod",
    "find",
    "grep",
    "sed",
    "awk",
    "tr",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "env",
    "echo",
    "printf",
    "date",
    "touch",
    "tar",
    "gzip",
    "ip",
    "test",
];

/// Per-process resource limits applied in the guest via `setrlimit`.
///
/// Shared across all backends. Previously lived in `vmm::config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum virtual memory per process in bytes (RLIMIT_AS).
    pub max_virtual_memory: u64,
    /// Maximum number of open file descriptors (RLIMIT_NOFILE).
    pub max_open_files: u64,
    /// Maximum number of processes per user (RLIMIT_NPROC).
    pub max_processes: u64,
    /// Maximum file size in bytes (RLIMIT_FSIZE).
    pub max_file_size: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_virtual_memory: 4 * 1024 * 1024 * 1024, // 4 GB
            max_open_files: 1024,
            max_processes: 512,
            max_file_size: 100 * 1024 * 1024, // 100 MB
        }
    }
}

/// Create the platform-appropriate backend.
///
/// On Linux, returns a [`KvmBackend`](kvm::KvmBackend).
/// On macOS, returns a `VzBackend`.
#[cfg(target_os = "linux")]
pub fn create_backend() -> Box<dyn VmmBackend> {
    Box::new(kvm::KvmBackend::new())
}

/// Create the platform-appropriate backend.
///
/// On macOS, returns a `VzBackend`.
#[cfg(target_os = "macos")]
pub fn create_backend() -> Box<dyn VmmBackend> {
    Box::new(vz::VzBackend::new())
}
