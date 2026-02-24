//! macOS Virtualization.framework backend for VoidBox.
//!
//! This module implements `VmmBackend` using Apple's Virtualization.framework,
//! enabling VoidBox to run isolated micro-VMs on macOS (Apple Silicon).
//!
//! ## Architecture
//!
//! - **Boot**: `VZLinuxBootLoader` with custom aarch64 kernel, initrd, and cmdline
//! - **Networking**: `VZNATNetworkDeviceAttachment` (macOS manages NAT)
//! - **Host↔Guest control**: `VZVirtioSocketDevice` → raw fd → `GuestStream` adapter
//! - **Shared files**: `VZVirtioFileSystemDevice` for virtiofs skill provisioning (future)

mod backend;
pub mod config;
pub mod vsock;

pub use backend::VzBackend;
