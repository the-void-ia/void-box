//! Trait abstraction for virtio-vsock MMIO backends.
//!
//! Allows swapping between the kernel vhost-vsock backend (fast, default)
//! and a userspace backend (supports clean snapshot/restore).

use std::os::unix::io::RawFd;

use vm_memory::GuestMemoryMmap;

use crate::vmm::snapshot::VsockSnapshotState;
use crate::Result;

/// Common interface for virtio-vsock MMIO device backends.
///
/// The kernel vhost backend (`VirtioVsockMmio`) and the userspace backend
/// (`VirtioVsockUserspace`) both implement this trait so the VMM can use
/// either one transparently.
pub trait VsockMmioDevice: Send {
    /// MMIO base address of this device.
    fn mmio_base(&self) -> u64;

    /// MMIO region size.
    fn mmio_size(&self) -> u64;

    /// Set the MMIO base address.
    fn set_mmio_base(&mut self, base: u64);

    /// Check if a guest physical address falls within this device's MMIO region.
    fn handles_mmio(&self, addr: u64) -> bool;

    /// Handle an MMIO read from the guest.
    fn mmio_read(&self, offset: u64, data: &mut [u8]);

    /// Handle an MMIO write from the guest.
    fn mmio_write(
        &mut self,
        offset: u64,
        data: &[u8],
        guest_memory: &GuestMemoryMmap,
    ) -> Result<()>;

    /// Return the call eventfds used for IRQ injection.
    /// Index 0 = rx, 1 = tx, 2 = event.
    fn call_eventfds(&self) -> &[Option<RawFd>; 3];

    /// Set interrupt status bits (called by the IRQ handler thread).
    fn set_interrupt_status(&mut self, bits: u32);

    /// Capture device state for snapshotting.
    fn snapshot_state(&self) -> VsockSnapshotState;

    /// Inject a VIRTIO_VSOCK_EVENT_TRANSPORT_RESET into the event queue.
    ///
    /// Only meaningful for the userspace backend — tells the guest driver
    /// to close stale connections after a restore. The vhost backend does
    /// not need this (it's a no-op by default).
    fn inject_transport_reset(&mut self, _mem: &GuestMemoryMmap) -> Result<()> {
        Ok(())
    }
}
