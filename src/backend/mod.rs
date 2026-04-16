//! Backend abstraction for VM execution.
//!
//! This module defines the [`VmmBackend`] trait that all VM backends must
//! implement. The trait captures the full lifecycle: boot, exec, file I/O,
//! telemetry, and teardown.
//!
//! Platform-specific backends:
//! - **Linux**: `KvmBackend` — KVM micro-VMs with virtio-mmio devices
//! - **macOS**: `VzBackend` — Apple Virtualization.framework

pub mod control_channel;
pub mod pty_session;

#[cfg(target_os = "linux")]
pub mod kvm;

#[cfg(target_os = "macos")]
pub mod vz;

use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::guest::protocol::{ExecOutputChunk, ExecResponse, TelemetrySubscribeRequest};
use crate::observe::telemetry::{TelemetryAggregator, TelemetryBuffer};
use crate::observe::tracer::SpanContext;
use crate::observe::Observer;
use crate::ExecOutput;

/// Extra bytes needed beyond the initramfs footprint: Linux kernel image in
/// memory (~80 MB for Ubuntu arm64 6.8) plus slack for page tables, heap,
/// and the init process (~128 MB).  Both the compressed initramfs (bootloader
/// places it in guest RAM) and the decompressed tmpfs content coexist during
/// early-boot extraction, so the caller adds both on top of this constant.
const INITRAMFS_OVERHEAD_BYTES: u64 = 208 * 1024 * 1024;
#[cfg(target_os = "linux")]
const LINUX_GUEST_HOST_GATEWAY: &str = "10.0.2.2";
#[cfg(target_os = "macos")]
const MACOS_GUEST_HOST_GATEWAY: &str = "192.168.64.1";

fn session_secret_hex(session_secret: &[u8; 32]) -> String {
    session_secret
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

/// A single host→guest directory mount.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Absolute path on the host.
    pub host_path: String,
    /// Mount point inside the guest.
    pub guest_path: String,
    /// Read-only mount.
    pub read_only: bool,
}

/// Host-side routing for the guest serial console.
#[derive(Debug, Clone)]
pub enum GuestConsoleSink {
    /// Suppress host-visible guest serial console output.
    Disabled,
    /// Forward the guest serial console to the host process stderr.
    Stderr,
    /// Append the guest serial console to a host-side log file.
    File(PathBuf),
}

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
    /// Host-side routing for guest serial console output.
    pub guest_console: GuestConsoleSink,
    /// Host directory to share with guest (virtiofs on macOS, future on Linux).
    pub shared_dir: Option<PathBuf>,
    /// Host directory mounts into the guest.
    pub mounts: Vec<MountConfig>,
    /// Guest path where an OCI rootfs is mounted (triggers pivot_root in guest-agent).
    pub oci_rootfs: Option<String>,
    /// OCI rootfs block device in guest (e.g. /dev/vda).
    pub oci_rootfs_dev: Option<String>,
    /// Host path to OCI rootfs disk image to attach via virtio-blk (KVM).
    pub oci_rootfs_disk: Option<PathBuf>,
    /// Environment variables to inject into guest commands.
    pub env: Vec<(String, String)>,
    /// Security configuration.
    pub security: BackendSecurityConfig,
    /// Path to a snapshot directory to restore from (skips cold boot).
    /// If `None`, the VM is cold-booted normally.
    pub snapshot: Option<PathBuf>,
}

impl BackendConfig {
    /// Create a minimal `BackendConfig` with sensible defaults.
    ///
    /// Only requires the fields that have no reasonable default (kernel path,
    /// memory, vCPUs). Everything else is set to safe defaults with vsock
    /// enabled and the default command allowlist.
    pub fn minimal(kernel: impl Into<PathBuf>, memory_mb: usize, vcpus: usize) -> Self {
        let mut session_secret = [0u8; 32];
        getrandom::fill(&mut session_secret).expect("getrandom");
        Self {
            memory_mb,
            vcpus,
            kernel: kernel.into(),
            initramfs: None,
            rootfs: None,
            network: false,
            enable_vsock: true,
            guest_console: GuestConsoleSink::Stderr,
            shared_dir: None,
            mounts: Vec::new(),
            oci_rootfs: None,
            oci_rootfs_dev: None,
            oci_rootfs_disk: None,
            env: Vec::new(),
            security: BackendSecurityConfig {
                session_secret,
                command_allowlist: DEFAULT_COMMAND_ALLOWLIST
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                network_deny_list: Vec::new(),
                max_connections_per_second: 0,
                max_concurrent_connections: 0,
                seccomp: false,
            },
            snapshot: None,
        }
    }

    /// Set the initramfs path.
    pub fn initramfs(mut self, path: impl Into<PathBuf>) -> Self {
        self.initramfs = Some(path.into());
        self
    }

    /// Enable or disable networking.
    pub fn network(mut self, enabled: bool) -> Self {
        self.network = enabled;
        self
    }

    /// Check whether the configured memory is likely sufficient for the initramfs.
    ///
    /// Reads the gzip ISIZE field (last 4 bytes) for the uncompressed size and
    /// applies the heuristic:
    ///   minimum = uncompressed + compressed + 300 MB (kernel + runtime headroom)
    ///
    /// Returns a warning string if memory looks too low, `None` if it looks fine
    /// or the check cannot be performed (missing file, not a gz, etc.).
    pub fn initramfs_memory_warning(&self) -> Option<String> {
        let initramfs = self.initramfs.as_ref()?;
        let mut f = std::fs::File::open(initramfs).ok()?;
        let compressed_bytes = f.metadata().ok()?.len();
        if compressed_bytes < 4 {
            return None;
        }
        f.seek(SeekFrom::End(-4)).ok()?;
        let mut buf = [0u8; 4];
        f.read_exact(&mut buf).ok()?;
        // gzip ISIZE: uncompressed size mod 2^32 (little-endian)
        let uncompressed_bytes = u32::from_le_bytes(buf) as u64;

        // Peak physical memory during boot = compressed (in-flight) + uncompressed (tmpfs) + overhead.
        let min_bytes = uncompressed_bytes + compressed_bytes + INITRAMFS_OVERHEAD_BYTES;
        let configured_bytes = (self.memory_mb as u64) * 1024 * 1024;

        if configured_bytes < min_bytes {
            Some(format!(
                "VM memory ({}MB) is too low for initramfs \
                 (uncompressed ~{}MB, compressed {}MB). \
                 The kernel may silently drop initramfs files (e.g. vsock.ko) during boot. \
                 Recommended minimum: {}MB.",
                self.memory_mb,
                uncompressed_bytes / (1024 * 1024),
                compressed_bytes / (1024 * 1024),
                min_bytes.div_ceil(1024 * 1024),
            ))
        } else {
            None
        }
    }
}

/// Append guest-visible kernel command line arguments shared by KVM and VZ.
///
/// The caller owns the platform-specific prefix (console device, virtio
/// discovery, rootfs device wiring). This helper appends the common suffix:
/// session secret, boot clock, optional guest networking flags, mount
/// descriptors, and OCI rootfs selectors.
#[allow(clippy::too_many_arguments)]
pub(crate) fn append_common_guest_kernel_args(
    cmdline_parts: &mut Vec<String>,
    session_secret: &[u8; 32],
    epoch_secs: u64,
    network_enabled: bool,
    include_guest_network_flag: bool,
    mounts: &[MountConfig],
    oci_rootfs: Option<&str>,
    oci_rootfs_dev: Option<&str>,
) {
    cmdline_parts.push(format!(
        "voidbox.secret={}",
        session_secret_hex(session_secret)
    ));
    cmdline_parts.push(format!("voidbox.clock={}", epoch_secs));

    if network_enabled {
        if include_guest_network_flag {
            cmdline_parts.push("voidbox.network=1".to_string());
        }
        cmdline_parts.push("ipv6.disable=1".to_string());
    }

    for (mount_index, mount) in mounts.iter().enumerate() {
        let mount_mode = if mount.read_only { "ro" } else { "rw" };
        cmdline_parts.push(format!(
            "voidbox.mount{}=mount{}:{}:{}",
            mount_index, mount_index, mount.guest_path, mount_mode
        ));
    }

    if let Some(oci_rootfs_path) = oci_rootfs {
        cmdline_parts.push(format!("voidbox.oci_rootfs={}", oci_rootfs_path));
    }

    if let Some(oci_rootfs_device) = oci_rootfs_dev {
        cmdline_parts.push(format!("voidbox.oci_rootfs_dev={}", oci_rootfs_device));
    }
}

/// Host-reachable gateway address as seen from inside the guest VM.
///
/// Linux/KVM uses the userspace SLIRP gateway, while macOS/VZ uses the
/// Virtualization.framework NAT gateway.
pub fn guest_host_gateway() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        LINUX_GUEST_HOST_GATEWAY
    }
    #[cfg(target_os = "macos")]
    {
        MACOS_GUEST_HOST_GATEWAY
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "127.0.0.1"
    }
}

/// Build a guest-visible HTTP URL to a host-local service.
pub fn guest_host_url(port: u16) -> String {
    format!("http://{}:{}", guest_host_gateway(), port)
}

/// Bind address for host-side services that must be reachable from inside the guest.
///
/// On Linux/KVM, SLIRP forwards guest connections to host loopback. On macOS/VZ,
/// host services must listen on a non-loopback interface to be reachable through
/// the NAT gateway.
pub fn guest_accessible_bind_addr(port: u16) -> SocketAddr {
    #[cfg(target_os = "macos")]
    {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
    }
    #[cfg(not(target_os = "macos"))]
    {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }
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

    /// Checks if a file exists in the guest filesystem.
    async fn file_stat(&self, path: &str) -> Result<crate::guest::protocol::FileStatResponse>;

    /// Reads a file from the guest filesystem.
    async fn read_file_native(&self, path: &str) -> Result<Vec<u8>>;

    /// Start a telemetry subscription from the guest.
    async fn start_telemetry(
        &mut self,
        observer: Observer,
        opts: TelemetrySubscribeRequest,
        ring_buffer: Option<TelemetryBuffer>,
    ) -> Result<Arc<TelemetryAggregator>>;

    /// Set the active span context for TRACEPARENT propagation.
    fn set_span_context(&mut self, ctx: SpanContext);

    /// Opens a PTY session on the guest, returning a handle for interactive I/O.
    async fn attach_pty(
        &self,
        request: void_box_protocol::PtyOpenRequest,
    ) -> Result<pty_session::PtySession>;

    /// Check if the VM is running.
    fn is_running(&self) -> bool;

    /// Stop the VM and clean up resources.
    async fn stop(&mut self) -> Result<()>;

    /// Take a snapshot of the running VM, save it, then restore from it so
    /// the VM continues running (~500 ms stop-and-restart overhead).
    async fn create_auto_snapshot(
        &mut self,
        _snapshot_dir: &std::path::Path,
        _config_hash: String,
    ) -> Result<()> {
        Ok(())
    }

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
    "codex",
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
    "void-message",
    "void-mcp",
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
