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
    fn load_kernel(vm: &Vm, kernel: &Path, initramfs: Option<&Path>, cmdline: &str) -> Result<u64>;

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
