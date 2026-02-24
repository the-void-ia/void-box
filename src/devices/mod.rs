//! Device emulation for void-box VMs
//!
//! This module contains device implementations:
//! - Serial console (8250 UART)
//! - virtio-vsock for host-guest communication
//! - virtio-net for networking (SLIRP-based user-mode NAT)
//! - virtio-blk for block devices (optional)

pub mod serial;
pub mod virtio_9p;
pub mod virtio_net;
pub mod virtio_vsock;
pub mod virtio_vsock_mmio;
// pub mod virtio_blk;  // TODO: Future
