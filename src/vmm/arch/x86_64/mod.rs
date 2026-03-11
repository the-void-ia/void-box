//! x86_64 KVM architecture support.

pub mod boot;
pub mod cpu;
pub mod kvm;
pub mod snapshot;

use std::path::Path;

use kvm_ioctls::{VcpuFd, VmFd};

use crate::vmm::arch::{Arch, MemoryLayout};
use crate::vmm::kvm::Vm;
use crate::Result;

pub use snapshot::{ArchVmState, IrqchipState, VcpuState};

/// x86_64 architecture marker.
pub struct X86_64;

impl Arch for X86_64 {
    type VcpuState = VcpuState;
    type IrqchipState = IrqchipState;
    type ArchVmState = ArchVmState;

    fn setup_vm(vm_fd: &VmFd, vcpu_count: usize) -> Result<()> {
        kvm::setup_vm(vm_fd, vcpu_count)
    }

    fn load_kernel(vm: &Vm, kernel: &Path, initramfs: Option<&Path>, cmdline: &str) -> Result<u64> {
        boot::load_kernel(vm, kernel, initramfs, cmdline)
    }

    fn configure_vcpu(vcpu_fd: &VcpuFd, vcpu_id: u64, entry_point: u64, vm: &Vm) -> Result<()> {
        cpu::configure_vcpu(vcpu_fd, vcpu_id, entry_point, vm)
    }

    fn capture_vcpu_state(vcpu_fd: &VcpuFd) -> Result<VcpuState> {
        cpu::capture_vcpu_state(vcpu_fd)
    }

    fn capture_irqchip(vm: &Vm) -> Result<IrqchipState> {
        kvm::capture_irqchip(vm)
    }

    fn capture_arch_vm_state(vm: &Vm) -> Result<ArchVmState> {
        kvm::capture_arch_vm_state(vm)
    }

    fn restore_vcpu_state(vcpu_fd: &VcpuFd, state: &VcpuState, vcpu_id: u64) -> Result<()> {
        cpu::restore_vcpu_state(vcpu_fd, state, vcpu_id)
    }

    fn restore_irqchip(vm: &Vm, state: &IrqchipState) -> Result<()> {
        kvm::restore_irqchip(vm, state)
    }

    fn restore_arch_vm_state(vm: &Vm, state: &ArchVmState) -> Result<()> {
        kvm::restore_arch_vm_state(vm, state)
    }

    fn memory_layout() -> &'static MemoryLayout {
        &kvm::MEMORY_LAYOUT
    }
}
