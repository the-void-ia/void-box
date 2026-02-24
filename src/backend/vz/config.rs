//! Configuration mapping from [`BackendConfig`] to VZ types.
//!
//! Translates VoidBox's platform-agnostic configuration into the
//! Virtualization.framework objects needed to boot a VM.

use crate::backend::BackendConfig;

/// Build the kernel command line for a VZ-based VM.
///
/// Key differences from KVM:
/// - Console is `hvc0` (virtio-console), not `ttyS0` (UART serial)
/// - No `virtio_mmio.device=` declarations — VZ uses PCI auto-discovery
/// - Shared: `voidbox.secret`, `voidbox.clock`, `ipv6.disable=1`
pub fn build_kernel_cmdline(config: &BackendConfig) -> String {
    let mut parts = vec![
        "console=hvc0".to_string(),
        "loglevel=4".to_string(),
        "reboot=k".to_string(),
        "panic=1".to_string(),
        // No pci=off — VZ needs PCI for virtio
        "nokaslr".to_string(),
    ];

    // Inject session secret
    let secret_hex: String = config
        .security
        .session_secret
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    parts.push(format!("voidbox.secret={}", secret_hex));

    // Inject host wall-clock for TLS cert validation
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    parts.push(format!("voidbox.clock={}", epoch_secs));

    // Disable IPv6 if networking is enabled (our guest stack is IPv4 only)
    if config.network {
        parts.push("ipv6.disable=1".to_string());
    }

    // Root device: VZ presents the disk as /dev/vda via virtio-blk
    // (same as KVM, since both use virtio)
    // Note: rootfs setup is handled by the caller if needed

    // Mount config: tell the guest-agent which virtiofs tags to mount and where.
    // Format: voidbox.mount<N>=<tag>:<guest_path>:<ro|rw>
    for (i, mount) in config.mounts.iter().enumerate() {
        let mode = if mount.read_only { "ro" } else { "rw" };
        parts.push(format!(
            "voidbox.mount{}=mount{}:{}:{}",
            i, i, mount.guest_path, mode
        ));
    }

    // OCI rootfs: tell the guest-agent to pivot_root to the mounted rootfs.
    if let Some(ref oci_path) = config.oci_rootfs {
        parts.push(format!("voidbox.oci_rootfs={}", oci_path));
    }

    parts.join(" ")
}

/// Compute memory size in bytes from the config's megabytes.
pub fn memory_bytes(config: &BackendConfig) -> u64 {
    (config.memory_mb as u64) * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendConfig, BackendSecurityConfig};
    use std::path::PathBuf;

    fn test_config() -> BackendConfig {
        BackendConfig {
            memory_mb: 256,
            vcpus: 2,
            kernel: PathBuf::from("/tmp/vmlinuz"),
            initramfs: Some(PathBuf::from("/tmp/initrd")),
            rootfs: None,
            network: true,
            enable_vsock: true,
            shared_dir: None,
            mounts: vec![],
            oci_rootfs: None,
            env: vec![],
            security: BackendSecurityConfig {
                session_secret: [0xAB; 32],
                command_allowlist: vec![],
                network_deny_list: vec![],
                max_connections_per_second: 50,
                max_concurrent_connections: 64,
                seccomp: false,
            },
        }
    }

    #[test]
    fn cmdline_uses_hvc0() {
        let config = test_config();
        let cmdline = build_kernel_cmdline(&config);
        assert!(cmdline.contains("console=hvc0"));
        assert!(!cmdline.contains("ttyS0"));
    }

    #[test]
    fn cmdline_has_no_mmio_declarations() {
        let config = test_config();
        let cmdline = build_kernel_cmdline(&config);
        assert!(!cmdline.contains("virtio_mmio"));
    }

    #[test]
    fn cmdline_includes_secret() {
        let config = test_config();
        let cmdline = build_kernel_cmdline(&config);
        assert!(cmdline.contains("voidbox.secret="));
        // Secret is 32 bytes of 0xAB
        assert!(cmdline.contains(&"ab".repeat(32)));
    }

    #[test]
    fn cmdline_includes_clock() {
        let config = test_config();
        let cmdline = build_kernel_cmdline(&config);
        assert!(cmdline.contains("voidbox.clock="));
    }

    #[test]
    fn cmdline_disables_ipv6_when_network_enabled() {
        let config = test_config();
        let cmdline = build_kernel_cmdline(&config);
        assert!(cmdline.contains("ipv6.disable=1"));
    }

    #[test]
    fn memory_bytes_conversion() {
        let config = test_config();
        assert_eq!(memory_bytes(&config), 256 * 1024 * 1024);
    }
}
