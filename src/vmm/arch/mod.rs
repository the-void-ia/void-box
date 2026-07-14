//! Architecture-specific KVM support.
//!
//! This module defines the [`Arch`] trait that abstracts over x86_64 and
//! aarch64 KVM differences, and re-exports the current target's
//! implementation as [`CurrentArch`].

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

use std::path::Path;

use kvm_ioctls::{VcpuFd, VmFd};
use serde::{de::DeserializeOwned, Serialize};

use crate::vmm::kvm::Vm;
use crate::Result;

/// Memory layout for the guest physical address space.
pub struct MemoryLayout {
    /// Start of guest RAM.
    pub ram_start: u64,
    /// Start of the MMIO gap (x86 only).
    pub mmio_gap_start: Option<u64>,
    /// End of the MMIO gap (x86 only).
    pub mmio_gap_end: Option<u64>,
}

/// Fixed virtio-mmio device slot assignment, shared by device construction,
/// the kernel cmdline (x86_64), DTB generation (aarch64), IRQ injection, and
/// snapshot metadata.
///
/// Three per-device constants must always agree — the MMIO window base, the
/// irqfd GSI, and the ioeventfd doorbell address — and a drift between them
/// fails silently (an interrupt or doorbell that simply never matches).
/// Deriving all of them from the slot index makes drift structurally
/// impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioSlot {
    /// virtio-net (SLIRP networking).
    Net = 0,
    /// virtio-vsock (host↔guest control channel).
    Vsock = 1,
    /// virtio-9p (host directory mounts).
    P9 = 2,
    /// virtio-blk (OCI rootfs disk).
    Blk = 3,
}

impl VirtioSlot {
    fn index(self) -> u32 {
        self as u32
    }

    /// Guest-physical base of this slot's virtio-mmio window.
    pub fn mmio_base(self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        let layout_base = (
            x86_64::kvm::layout::VIRTIO_MMIO_BASE,
            x86_64::kvm::layout::VIRTIO_MMIO_STRIDE,
        );
        #[cfg(target_arch = "aarch64")]
        let layout_base = (
            aarch64::kvm::layout::VIRTIO_MMIO_BASE,
            aarch64::kvm::layout::VIRTIO_MMIO_STRIDE,
        );
        let (base, stride) = layout_base;
        base + stride * u64::from(self.index())
    }

    /// GSI to register a `KVM_IRQFD` eventfd against.
    ///
    /// x86_64: the raw IOAPIC GSI. aarch64: the SPI index — the vGIC
    /// installs a default identity GSI routing table at vGIC init (gsi *n*
    /// → irqchip pin *n*, delivered as INTID *n* + 32), so the value is the
    /// SPI index. That routing exists only once the vGIC is initialized:
    /// an irqfd registered before `setup_vm_post_vcpus` would report
    /// success and never deliver.
    pub fn irqfd_gsi(self) -> u32 {
        #[cfg(target_arch = "x86_64")]
        {
            x86_64::kvm::layout::VIRTIO_GSI_BASE + self.index()
        }
        #[cfg(target_arch = "aarch64")]
        {
            self.spi()
        }
    }

    /// GIC SPI index (the second cell of the DTB interrupt specifier).
    #[cfg(target_arch = "aarch64")]
    pub fn spi(self) -> u32 {
        aarch64::kvm::layout::VIRTIO_SPI_BASE + self.index()
    }

    /// Value for the `KVM_IRQ_LINE` `irq` field.
    ///
    /// x86_64: the raw GSI. aarch64: packed per the arm64 uapi — irq_type
    /// (bits 27:24, `KVM_ARM_IRQ_TYPE_SHIFT`) = SPI, irq_id (bits 15:0) =
    /// the GIC INTID (32 + SPI index). The shift and type constants come
    /// from `kvm-bindings`, so the encoding cannot drift from the ABI.
    pub fn irq_line_value(self) -> u32 {
        #[cfg(target_arch = "x86_64")]
        {
            self.irqfd_gsi()
        }
        #[cfg(target_arch = "aarch64")]
        {
            use kvm_bindings::{KVM_ARM_IRQ_TYPE_SHIFT, KVM_ARM_IRQ_TYPE_SPI};

            const SPI_INTID_BASE: u32 = 32;
            (KVM_ARM_IRQ_TYPE_SPI << KVM_ARM_IRQ_TYPE_SHIFT) | (SPI_INTID_BASE + self.spi())
        }
    }
}

/// Facts about the virtual platform the arch boot code needs at kernel-load
/// time. The aarch64 DTB describes CPUs and every device the VMM creates;
/// x86_64 carries the same facts in the kernel cmdline instead and ignores
/// this struct.
pub struct BootPlatform {
    /// Number of vCPUs the VM will have.
    pub vcpu_count: usize,
    /// Populated virtio-mmio slots, in slot order.
    pub virtio_slots: Vec<VirtioSlot>,
}

/// Trait that abstracts architecture-specific KVM operations.
///
/// Implemented by `X86_64` and `Aarch64`; the current target's type is
/// re-exported as [`CurrentArch`].  All methods are static — no `&self` —
/// so callers use `CurrentArch::method(...)`.
pub trait Arch {
    /// Per-vCPU register state for snapshot/restore.
    type VcpuState: Serialize + DeserializeOwned + Clone + Send + std::fmt::Debug;
    /// Interrupt controller state for snapshot/restore.
    type IrqchipState: Serialize + DeserializeOwned + Clone + Send + std::fmt::Debug;
    /// Extra arch-specific VM state (e.g. PIT + KVM clock on x86).
    type ArchVmState: Serialize + DeserializeOwned + Clone + Send + std::fmt::Debug + Default;

    // -- Boot --

    /// Arch-specific VM setup that must run **before** any vCPU is created.
    ///
    /// x86_64 creates the in-kernel irqchip + PIT here: KVM rejects
    /// `KVM_CREATE_IRQCHIP` once vCPUs exist, and each vCPU's in-kernel
    /// LAPIC is wired according to the irqchip mode at `KVM_CREATE_VCPU`
    /// time. No-op on aarch64, whose GIC has the opposite ordering
    /// constraint — see [`Arch::setup_vm_post_vcpus`].
    fn setup_vm(vm_fd: &VmFd) -> Result<()>;

    /// Arch-specific VM setup that must run **after** all vCPUs are created
    /// and **before** any of them runs.
    ///
    /// aarch64 creates and initializes the vGIC here:
    /// `KVM_DEV_ARM_VGIC_CTRL_INIT` sizes per-vCPU redistributor state and
    /// freezes the vGIC configuration, so the kernel rejects any later
    /// `KVM_CREATE_VCPU` with `EBUSY` — and refuses to create a vGIC at all
    /// once a vCPU has run. No-op on x86_64.
    fn setup_vm_post_vcpus(vm_fd: &VmFd, vcpu_count: usize) -> Result<()>;

    /// Load kernel (and optionally initramfs) into guest memory.
    ///
    /// `platform` describes the vCPUs and populated virtio slots — aarch64
    /// needs both at load time because the generated DTB carries one
    /// `/cpus` node per vCPU, sizes the GICv3 redistributor region, and
    /// declares one virtio-mmio node per populated slot; x86_64 ignores it.
    fn load_kernel(
        vm: &Vm,
        kernel: &Path,
        initramfs: Option<&Path>,
        cmdline: &str,
        platform: &BootPlatform,
    ) -> Result<u64>;

    /// Configure a freshly-created vCPU for cold boot.
    fn configure_vcpu(vcpu_fd: &VcpuFd, vcpu_id: u64, entry_point: u64, vm: &Vm) -> Result<()>;

    // -- Snapshot capture --

    /// Capture full vCPU register state.
    fn capture_vcpu_state(vcpu_fd: &VcpuFd) -> Result<Self::VcpuState>;

    /// Capture interrupt controller state.
    fn capture_irqchip(vm: &Vm) -> Result<Self::IrqchipState>;

    /// Capture arch-specific VM state (PIT + KVM clock on x86, empty on aarch64).
    fn capture_arch_vm_state(vm: &Vm) -> Result<Self::ArchVmState>;

    // -- Snapshot restore --

    /// Restore vCPU register state from a snapshot.
    fn restore_vcpu_state(vcpu_fd: &VcpuFd, state: &Self::VcpuState, vcpu_id: u64) -> Result<()>;

    /// Restore interrupt controller state.
    fn restore_irqchip(vm: &Vm, state: &Self::IrqchipState) -> Result<()>;

    /// Restore arch-specific VM state.
    fn restore_arch_vm_state(vm: &Vm, state: &Self::ArchVmState) -> Result<()>;

    // -- Layout --

    /// Return the guest physical memory layout for this architecture.
    fn memory_layout() -> &'static MemoryLayout;
}

// Compile-time arch dispatch
#[cfg(target_arch = "aarch64")]
pub use aarch64::Aarch64 as CurrentArch;
#[cfg(target_arch = "x86_64")]
pub use x86_64::X86_64 as CurrentArch;

// Re-export arch-specific types under generic names
pub type VcpuState = <CurrentArch as Arch>::VcpuState;
pub type IrqchipState = <CurrentArch as Arch>::IrqchipState;
pub type ArchVmState = <CurrentArch as Arch>::ArchVmState;

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the x86 slot tuple to the exact pre-slot-table literals so the
    /// refactor's byte-identical-x86 claim is checked, not asserted.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_slot_tuple_matches_pre_slot_table_values() {
        assert_eq!(VirtioSlot::Net.mmio_base(), 0xd000_0000);
        assert_eq!(VirtioSlot::Vsock.mmio_base(), 0xd080_0000);
        assert_eq!(VirtioSlot::P9.mmio_base(), 0xd100_0000);
        assert_eq!(VirtioSlot::Blk.mmio_base(), 0xd180_0000);
        assert_eq!(VirtioSlot::Net.irqfd_gsi(), 10);
        assert_eq!(VirtioSlot::Net.irq_line_value(), 10);
        assert_eq!(VirtioSlot::Vsock.irq_line_value(), 11);
        assert_eq!(VirtioSlot::P9.irq_line_value(), 12);
        assert_eq!(VirtioSlot::Blk.irq_line_value(), 13);
        // TX-notify ioeventfd doorbell: net base + QUEUE_NOTIFY offset.
        assert_eq!(VirtioSlot::Net.mmio_base() + 0x50, 0xd000_0050);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_slot_values_match_memory_map_and_irq_abi() {
        assert_eq!(VirtioSlot::Net.mmio_base(), 0x0a00_0000);
        assert_eq!(VirtioSlot::Vsock.mmio_base(), 0x0a00_1000);
        assert_eq!(VirtioSlot::P9.mmio_base(), 0x0a00_2000);
        assert_eq!(VirtioSlot::Blk.mmio_base(), 0x0a00_3000);
        assert_eq!(VirtioSlot::Net.spi(), 16);
        assert_eq!(VirtioSlot::Net.irqfd_gsi(), 16);
        // KVM_IRQ_LINE packing: irq_type SPI (1) at bits 27:24, INTID in
        // bits 15:0 (32 + SPI index).
        assert_eq!(VirtioSlot::Net.irq_line_value(), (1 << 24) | 48);
        assert_eq!(VirtioSlot::Vsock.irq_line_value(), (1 << 24) | 49);
        assert_eq!(VirtioSlot::Blk.irq_line_value(), (1 << 24) | 51);
    }
}
