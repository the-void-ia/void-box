//! Network configuration module
//!
//! This module handles network configuration for VMs, including:
//! - TAP device setup
//! - virtio-net configuration
//! - Network isolation

use crate::Result;

/// Network configuration for a VM
#[derive(Debug, Clone, Default)]
pub struct NetworkConfig {
    /// Enable networking
    pub enabled: bool,
    /// TAP device name (auto-generated if not specified)
    pub tap_name: Option<String>,
    /// MAC address (auto-generated if not specified)
    pub mac_address: Option<String>,
    /// Enable NAT for outbound connections
    pub enable_nat: bool,
    /// Host port forwards (host_port, guest_port)
    pub port_forwards: Vec<(u16, u16)>,
}

impl NetworkConfig {
    /// Create a new disabled network config
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Create a new enabled network config with defaults
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            tap_name: None,
            mac_address: None,
            enable_nat: true,
            port_forwards: Vec::new(),
        }
    }

    /// Set the TAP device name
    pub fn tap_name(mut self, name: impl Into<String>) -> Self {
        self.tap_name = Some(name.into());
        self
    }

    /// Set the MAC address
    pub fn mac_address(mut self, mac: impl Into<String>) -> Self {
        self.mac_address = Some(mac.into());
        self
    }

    /// Add a port forward
    pub fn port_forward(mut self, host_port: u16, guest_port: u16) -> Self {
        self.port_forwards.push((host_port, guest_port));
        self
    }
}

/// TAP device handle
pub struct TapDevice {
    name: String,
    fd: i32,
}

impl TapDevice {
    /// Create a new TAP device
    pub fn create(name: Option<&str>) -> Result<Self> {
        // This is a placeholder - actual TAP creation requires root privileges
        // and uses /dev/net/tun with TUNSETIFF ioctl
        let name = name.unwrap_or("void-tap0").to_string();

        // For now, return a placeholder
        Ok(Self { name, fd: -1 })
    }

    /// Get the device name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the file descriptor
    pub fn fd(&self) -> i32 {
        self.fd
    }
}

impl Drop for TapDevice {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

/// Generate a random MAC address with the locally administered bit set
pub fn generate_mac_address() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // Set locally administered bit (bit 1 of first byte)
    // Clear multicast bit (bit 0 of first byte)
    let b0 = ((seed >> 40) as u8 & 0xFC) | 0x02;
    let b1 = (seed >> 32) as u8;
    let b2 = (seed >> 24) as u8;
    let b3 = (seed >> 16) as u8;
    let b4 = (seed >> 8) as u8;
    let b5 = seed as u8;

    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b0, b1, b2, b3, b4, b5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_config_disabled() {
        let config = NetworkConfig::disabled();
        assert!(!config.enabled);
    }

    #[test]
    fn test_network_config_enabled() {
        let config = NetworkConfig::enabled()
            .tap_name("test-tap")
            .port_forward(8080, 80);

        assert!(config.enabled);
        assert_eq!(config.tap_name, Some("test-tap".to_string()));
        assert_eq!(config.port_forwards, vec![(8080, 80)]);
    }

    #[test]
    fn test_generate_mac_address() {
        let mac = generate_mac_address();
        assert_eq!(mac.len(), 17); // XX:XX:XX:XX:XX:XX

        // Parse first byte and verify locally administered bit
        let first_byte = u8::from_str_radix(&mac[0..2], 16).unwrap();
        assert!(first_byte & 0x02 != 0); // Locally administered
        assert!(first_byte & 0x01 == 0); // Not multicast
    }
}
