//! Configuration mapping from [`BackendConfig`] to VZ types.
//!
//! Translates VoidBox's platform-agnostic configuration into the
//! Virtualization.framework objects needed to boot a VM.

use crate::backend::{append_common_guest_kernel_args, BackendConfig};

pub(crate) fn current_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Build the kernel command line for a VZ-based VM.
///
/// Key differences from KVM:
/// - Console is `hvc0` (virtio-console), not `ttyS0` (UART serial)
/// - No `virtio_mmio.device=` declarations — VZ uses PCI auto-discovery
/// - Shared: `voidbox.secret`, `voidbox.clock`, `ipv6.disable=1`
pub fn build_kernel_cmdline(config: &BackendConfig) -> String {
    build_kernel_cmdline_with_clock(config, current_epoch_secs())
}

/// Build the kernel command line for a VZ-based VM with an explicit boot clock.
pub fn build_kernel_cmdline_with_clock(config: &BackendConfig, epoch_secs: u64) -> String {
    let mut parts = vec![
        "console=hvc0".to_string(),
        "loglevel=0".to_string(),
        "reboot=k".to_string(),
        "panic=1".to_string(),
        // No pci=off — VZ needs PCI for virtio
        "nokaslr".to_string(),
    ];

    // OCI rootfs: VZ uses virtiofs (not virtio-blk) for OCI rootfs delivery.
    // The host shares the extracted rootfs directory read-only; the guest
    // overlays it with a tmpfs upper layer. See AGENTS.md for details.
    append_common_guest_kernel_args(
        &mut parts,
        config.security.session_secret.expose_secret(),
        epoch_secs,
        config.network,
        true,
        &config.mounts,
        config.oci_rootfs.as_deref(),
        None,
    );

    parts.join(" ")
}

/// Compute memory size in bytes from the config's megabytes.
pub fn memory_bytes(config: &BackendConfig) -> u64 {
    (config.memory_mb as u64) * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendConfig, BackendSecurityConfig, GuestConsoleSink};
    use std::path::PathBuf;
    use void_box_protocol::SessionSecret;

    const TEST_CLOCK_SECS: u64 = 1_700_000_000;

    fn test_config() -> BackendConfig {
        BackendConfig {
            memory_mb: 256,
            vcpus: 2,
            kernel: PathBuf::from("/tmp/vmlinuz"),
            initramfs: Some(PathBuf::from("/tmp/initrd")),
            rootfs: None,
            network: true,
            enable_vsock: true,
            guest_console: GuestConsoleSink::Stderr,
            shared_dir: None,
            mounts: vec![],
            oci_rootfs: None,
            oci_rootfs_dev: None,
            oci_rootfs_disk: None,
            env: vec![],
            security: BackendSecurityConfig {
                session_secret: SessionSecret::new([0xAB; 32]),
                command_allowlist: vec![],
                network_deny_list: vec![],
                max_connections_per_second: 50,
                max_concurrent_connections: 64,
                seccomp: false,
            },
            snapshot: None,
            enable_snapshots: false,
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
        let cmdline = build_kernel_cmdline_with_clock(&config, TEST_CLOCK_SECS);
        assert!(cmdline.contains(&format!("voidbox.clock={TEST_CLOCK_SECS}")));
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
