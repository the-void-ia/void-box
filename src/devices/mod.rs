//! Device emulation for void-box VMs
//!
//! This module contains device implementations:
//! - Serial console (8250 UART)
//! - virtio-vsock for host-guest communication
//! - virtio-net for networking (optional)
//! - virtio-blk for block devices (optional)

pub mod serial;
pub mod virtio_vsock;
// pub mod virtio_net;  // TODO: Phase 4
// pub mod virtio_blk;  // TODO: Future
