//! Network configuration module
//!
//! This module handles network configuration for VMs, including:
//! - TAP device setup
//! - SLIRP user-mode networking (smoltcp-based)
//! - virtio-net configuration
//! - Network isolation and NAT

pub mod slirp;

use std::ffi::CString;

use crate::{Error, Result};

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
        // TAP creation requires /dev/net/tun and typically CAP_NET_ADMIN.
        // We try to create the device and surface any OS error back to the caller.
        let name = name.unwrap_or("void-tap0");

        // Open /dev/net/tun
        let fd = unsafe {
            let path = b"/dev/net/tun\0";
            libc::open(
                path.as_ptr() as *const libc::c_char,
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };

        if fd < 0 {
            return Err(Error::Device(format!(
                "failed to open /dev/net/tun: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Prepare ifreq with interface name and flags (IFF_TAP | IFF_NO_PI).
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        let cname = CString::new(name)
            .map_err(|e| Error::Device(format!("invalid TAP device name '{}': {}", name, e)))?;

        unsafe {
            // Copy name into the ifreq name field
            libc::strncpy(
                ifr.ifr_name.as_mut_ptr(),
                cname.as_ptr(),
                libc::IFNAMSIZ as usize,
            );

            // Set flags to create a TAP device without extra packet info header.
            ifr.ifr_ifru.ifru_flags = (libc::IFF_TAP | libc::IFF_NO_PI) as libc::c_short;

            // TUNSETIFF ioctl: from <linux/if_tun.h>
            const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
            let ret = libc::ioctl(fd, TUNSETIFF, &ifr);
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(Error::Device(format!(
                    "failed to create TAP device '{}': {}",
                    name, err
                )));
            }
        }

        Ok(Self {
            name: name.to_string(),
            fd,
        })
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

    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b0, b1, b2, b3, b4, b5
    )
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
