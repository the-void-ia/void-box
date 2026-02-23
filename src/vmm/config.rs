//! Configuration for VoidBox VMs

use std::path::PathBuf;

use crate::{Error, Result};

// Re-export from the cross-platform backend module for backward compatibility.
pub use crate::backend::{DEFAULT_COMMAND_ALLOWLIST, ResourceLimits};

/// Security configuration for VoidBox VMs.
///
/// All security features are mandatory by default. No opt-out toggles
/// unless there's a concrete development need.
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// 32-byte session secret for vsock authentication.
    /// Auto-generated via `getrandom` in `Default`.
    pub session_secret: [u8; 32],
    /// Allowlist of commands that may be executed in the guest.
    pub command_allowlist: Vec<String>,
    /// Per-process resource limits for the guest.
    pub resource_limits: ResourceLimits,
    /// Network deny list in CIDR notation.
    pub network_deny_list: Vec<String>,
    /// Maximum new TCP connections per second from the guest.
    pub max_connections_per_second: u32,
    /// Maximum concurrent TCP connections from the guest.
    pub max_concurrent_connections: usize,
    /// Whether to install seccomp-bpf filter on the VMM process.
    pub seccomp: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("Failed to generate session secret");
        Self {
            session_secret: secret,
            command_allowlist: DEFAULT_COMMAND_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
            resource_limits: ResourceLimits::default(),
            network_deny_list: vec!["169.254.0.0/16".to_string()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        }
    }
}

/// Configuration for creating a new VoidBox VM
#[derive(Debug, Clone)]
pub struct VoidBoxConfig {
    /// Memory size in megabytes (default: 128)
    pub memory_mb: usize,
    /// Number of vCPUs (default: 1)
    pub vcpus: usize,
    /// Path to the kernel image (vmlinux or bzImage)
    pub kernel: PathBuf,
    /// Path to initramfs (optional)
    pub initramfs: Option<PathBuf>,
    /// Path to root filesystem image (optional, for virtio-blk)
    pub rootfs: Option<PathBuf>,
    /// Enable networking
    pub network: bool,
    /// TAP device name for networking
    pub tap_name: Option<String>,
    /// Host directory to share with guest
    pub shared_dir: Option<PathBuf>,
    /// Enable vsock for host-guest communication
    pub enable_vsock: bool,
    /// Vsock context ID (auto-generated if not specified)
    pub cid: Option<u32>,
    /// Additional kernel command line arguments
    pub extra_cmdline: Vec<String>,
    /// Security configuration (auth, allowlists, limits, seccomp).
    pub security: SecurityConfig,
}

impl Default for VoidBoxConfig {
    fn default() -> Self {
        Self {
            memory_mb: 128,
            vcpus: 1,
            kernel: PathBuf::new(),
            initramfs: None,
            rootfs: None,
            network: false,
            tap_name: None,
            shared_dir: None,
            enable_vsock: true,
            cid: None,
            extra_cmdline: Vec::new(),
            security: SecurityConfig::default(),
        }
    }
}

impl VoidBoxConfig {
    /// Create a new configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the memory size in megabytes
    pub fn memory_mb(mut self, mb: usize) -> Self {
        self.memory_mb = mb;
        self
    }

    /// Set the number of vCPUs
    pub fn vcpus(mut self, count: usize) -> Self {
        self.vcpus = count;
        self
    }

    /// Set the kernel image path
    pub fn kernel<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.kernel = path.into();
        self
    }

    /// Set the initramfs path
    pub fn initramfs<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.initramfs = Some(path.into());
        self
    }

    /// Set the root filesystem image path
    pub fn rootfs<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.rootfs = Some(path.into());
        self
    }

    /// Enable or disable networking
    pub fn network(mut self, enable: bool) -> Self {
        self.network = enable;
        self
    }

    /// Set the TAP device name
    pub fn tap_name<S: Into<String>>(mut self, name: S) -> Self {
        self.tap_name = Some(name.into());
        self
    }

    /// Set the shared directory path
    pub fn shared_dir<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.shared_dir = Some(path.into());
        self
    }

    /// Enable or disable vsock
    pub fn enable_vsock(mut self, enable: bool) -> Self {
        self.enable_vsock = enable;
        self
    }

    /// Set the vsock CID
    pub fn cid(mut self, cid: u32) -> Self {
        self.cid = Some(cid);
        self
    }

    /// Add extra kernel command line arguments
    pub fn extra_cmdline<S: Into<String>>(mut self, args: S) -> Self {
        self.extra_cmdline.push(args.into());
        self
    }

    /// Build the kernel command line string
    pub fn kernel_cmdline(&self) -> String {
        let mut cmdline = vec![
            "console=ttyS0".to_string(),
            "loglevel=4".to_string(), // Suppress INFO messages (keeps warnings/errors)
            "earlyprintk=serial,ttyS0,115200".to_string(),
            "reboot=k".to_string(),
            "panic=1".to_string(),
            "pci=off".to_string(),
            "nokaslr".to_string(),
            "i8042.noaux".to_string(),
        ];

        // Only add nomodules if vsock is NOT enabled (vsock needs modprobe)
        if !self.enable_vsock {
            cmdline.push("nomodules".to_string());
        }

        // Virtio MMIO devices so the guest kernel discovers them (no ACPI in minimal boot).
        // Format: size@baseaddr:irq (see Linux virtio_mmio driver).
        if self.network {
            cmdline.push("virtio_mmio.device=512@0xd0000000:10".to_string());
            // Disable IPv6 - our SLIRP stack only supports IPv4
            cmdline.push("ipv6.disable=1".to_string());
        }
        if self.enable_vsock {
            cmdline.push("virtio_mmio.device=512@0xd0800000:11".to_string());
        }

        // Add root device if rootfs is specified
        if self.rootfs.is_some() {
            cmdline.push("root=/dev/vda".to_string());
            cmdline.push("rootfstype=ext4".to_string());
            cmdline.push("rw".to_string());
        }

        // Inject session secret for vsock authentication
        let secret_hex: String = self
            .security
            .session_secret
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        cmdline.push(format!("voidbox.secret={}", secret_hex));

        // Inject host wall-clock so the guest can set its system time.
        // Without this, the guest starts at epoch (1970) and TLS cert
        // validation fails.
        let epoch_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        cmdline.push(format!("voidbox.clock={}", epoch_secs));

        // Add extra arguments
        cmdline.extend(self.extra_cmdline.clone());

        cmdline.join(" ")
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<()> {
        // Check kernel path exists
        if !self.kernel.exists() {
            return Err(Error::Config(format!(
                "Kernel not found: {}",
                self.kernel.display()
            )));
        }

        // Check initramfs if specified
        if let Some(ref initramfs) = self.initramfs {
            if !initramfs.exists() {
                return Err(Error::Config(format!(
                    "Initramfs not found: {}",
                    initramfs.display()
                )));
            }
        }

        // Check rootfs if specified
        if let Some(ref rootfs) = self.rootfs {
            if !rootfs.exists() {
                return Err(Error::Config(format!(
                    "Root filesystem not found: {}",
                    rootfs.display()
                )));
            }
        }

        // Check shared_dir if specified
        if let Some(ref shared_dir) = self.shared_dir {
            if !shared_dir.exists() || !shared_dir.is_dir() {
                return Err(Error::Config(format!(
                    "Shared directory not found or not a directory: {}",
                    shared_dir.display()
                )));
            }
        }

        // Validate memory size (minimum 16MB, maximum 16GB)
        if self.memory_mb < 16 {
            return Err(Error::Config("Memory must be at least 16MB".into()));
        }
        if self.memory_mb > 16 * 1024 {
            return Err(Error::Config("Memory must be at most 16GB".into()));
        }

        // Validate vCPU count
        if self.vcpus == 0 {
            return Err(Error::Config("Must have at least 1 vCPU".into()));
        }
        if self.vcpus > 256 {
            return Err(Error::Config("Maximum 256 vCPUs supported".into()));
        }

        // Validate CID if specified (must be > 2)
        if let Some(cid) = self.cid {
            if cid < 3 {
                return Err(Error::Config(
                    "vsock CID must be >= 3 (0-2 are reserved)".into(),
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = VoidBoxConfig::default();
        assert_eq!(config.memory_mb, 128);
        assert_eq!(config.vcpus, 1);
        assert!(config.enable_vsock);
    }

    #[test]
    fn test_builder_pattern() {
        let config = VoidBoxConfig::new()
            .memory_mb(256)
            .vcpus(2)
            .kernel("/path/to/kernel")
            .network(true);

        assert_eq!(config.memory_mb, 256);
        assert_eq!(config.vcpus, 2);
        assert_eq!(config.kernel, PathBuf::from("/path/to/kernel"));
        assert!(config.network);
    }

    #[test]
    fn test_kernel_cmdline() {
        let config = VoidBoxConfig::new().extra_cmdline("quiet");
        let cmdline = config.kernel_cmdline();
        assert!(cmdline.contains("console=ttyS0"));
        assert!(cmdline.contains("quiet"));
    }

    #[test]
    fn test_validation_memory() {
        let config = VoidBoxConfig::new().memory_mb(8).kernel("/tmp/nonexistent");
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_vcpus() {
        let config = VoidBoxConfig::new().vcpus(0).kernel("/tmp/nonexistent");
        assert!(config.validate().is_err());
    }
}
