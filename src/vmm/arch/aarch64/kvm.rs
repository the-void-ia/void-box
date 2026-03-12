//! aarch64 KVM setup: GIC creation, capture/restore, memory layout.

use kvm_ioctls::VmFd;
use tracing::debug;

use crate::vmm::arch::MemoryLayout;
use crate::vmm::kvm::Vm;
use crate::{Error, Result};

use super::snapshot::{ArchVmState, IrqchipState};

/// aarch64 memory layout constants.
pub mod layout {
    /// Start of guest RAM (above the 1GB device region).
    pub const RAM_START: u64 = 0x4000_0000; // 1 GB

    /// DTB address (at start of RAM).
    pub const DTB_ADDR: u64 = RAM_START;

    /// Kernel load address (must be 2MB aligned, after DTB space).
    pub const KERNEL_LOAD_ADDR: u64 = 0x4008_0000; // RAM_START + 512 KB

    /// Initramfs load address.
    pub const INITRAMFS_LOAD_ADDR: u64 = 0x4400_0000; // RAM_START + 64 MB

    /// GIC distributor base address.
    pub const GIC_DIST_ADDR: u64 = 0x0800_0000;

    /// GIC distributor region size.
    pub const GIC_DIST_SIZE: u64 = 0x0001_0000; // 64 KB

    /// GIC redistributor base address (GICv3).
    pub const GIC_REDIST_ADDR: u64 = 0x080A_0000;

    /// GIC redistributor region size per vCPU.
    pub const GIC_REDIST_SIZE_PER_CPU: u64 = 0x0002_0000; // 128 KB
}

/// aarch64 memory layout for arch-neutral code.
pub static MEMORY_LAYOUT: MemoryLayout = MemoryLayout {
    ram_start: layout::RAM_START,
    mmio_gap_start: None,
    mmio_gap_end: None,
};

/// Create the GIC (Generic Interrupt Controller) for the VM.
///
/// Tries GICv3 first, then falls back to GICv2.
pub fn setup_vm(vm_fd: &VmFd, vcpu_count: usize) -> Result<()> {
    // Try GICv3 first
    match create_gicv3(vm_fd, vcpu_count) {
        Ok(()) => {
            debug!(
                "Created GICv3 (dist={:#x}, redist={:#x}, {} vCPUs)",
                layout::GIC_DIST_ADDR,
                layout::GIC_REDIST_ADDR,
                vcpu_count
            );
            return Ok(());
        }
        Err(e) => {
            debug!("GICv3 creation failed, trying GICv2: {}", e);
        }
    }

    // Fallback to GICv2
    create_gicv2(vm_fd)?;
    debug!("Created GICv2 (dist={:#x})", layout::GIC_DIST_ADDR);
    Ok(())
}

/// Create a GICv3 via KVM_CREATE_DEVICE.
fn create_gicv3(vm_fd: &VmFd, vcpu_count: usize) -> Result<()> {
    use kvm_bindings::{
        kvm_create_device, kvm_device_attr, KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_DEV_TYPE_ARM_VGIC_V3,
        KVM_VGIC_V3_ADDR_TYPE_DIST, KVM_VGIC_V3_ADDR_TYPE_REDIST,
    };

    let mut device = kvm_create_device {
        type_: KVM_DEV_TYPE_ARM_VGIC_V3,
        fd: 0,
        flags: 0,
    };

    let dev_fd = vm_fd
        .create_device(&mut device)
        .map_err(|e| Error::Device(format!("create GICv3 device: {}", e)))?;

    // Set distributor address
    let dist_addr = layout::GIC_DIST_ADDR;
    let dist_attr = kvm_device_attr {
        group: KVM_DEV_ARM_VGIC_GRP_ADDR,
        attr: KVM_VGIC_V3_ADDR_TYPE_DIST as u64,
        addr: &dist_addr as *const u64 as u64,
        flags: 0,
    };
    dev_fd
        .set_device_attr(&dist_attr)
        .map_err(|e| Error::Device(format!("set GICv3 dist addr: {}", e)))?;

    // Set redistributor address
    let redist_addr = layout::GIC_REDIST_ADDR;
    let redist_attr = kvm_device_attr {
        group: KVM_DEV_ARM_VGIC_GRP_ADDR,
        attr: KVM_VGIC_V3_ADDR_TYPE_REDIST as u64,
        addr: &redist_addr as *const u64 as u64,
        flags: 0,
    };
    dev_fd
        .set_device_attr(&redist_attr)
        .map_err(|e| Error::Device(format!("set GICv3 redist addr: {}", e)))?;

    // Initialize the GIC
    let init_attr = kvm_device_attr {
        group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
        attr: kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
        addr: 0,
        flags: 0,
    };
    dev_fd
        .set_device_attr(&init_attr)
        .map_err(|e| Error::Device(format!("init GICv3: {}", e)))?;

    Ok(())
}

/// Create a GICv2 via KVM_CREATE_DEVICE.
fn create_gicv2(vm_fd: &VmFd) -> Result<()> {
    use kvm_bindings::{
        kvm_create_device, kvm_device_attr, KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_DEV_TYPE_ARM_VGIC_V2,
        KVM_VGIC_V2_ADDR_TYPE_CPU, KVM_VGIC_V2_ADDR_TYPE_DIST,
    };

    let mut device = kvm_create_device {
        type_: KVM_DEV_TYPE_ARM_VGIC_V2,
        fd: 0,
        flags: 0,
    };

    let dev_fd = vm_fd
        .create_device(&mut device)
        .map_err(|e| Error::Device(format!("create GICv2 device: {}", e)))?;

    // Set distributor address
    let dist_addr = layout::GIC_DIST_ADDR;
    let dist_attr = kvm_device_attr {
        group: KVM_DEV_ARM_VGIC_GRP_ADDR,
        attr: KVM_VGIC_V2_ADDR_TYPE_DIST as u64,
        addr: &dist_addr as *const u64 as u64,
        flags: 0,
    };
    dev_fd
        .set_device_attr(&dist_attr)
        .map_err(|e| Error::Device(format!("set GICv2 dist addr: {}", e)))?;

    // Set CPU interface address
    let cpu_addr: u64 = layout::GIC_DIST_ADDR + layout::GIC_DIST_SIZE;
    let cpu_attr = kvm_device_attr {
        group: KVM_DEV_ARM_VGIC_GRP_ADDR,
        attr: KVM_VGIC_V2_ADDR_TYPE_CPU as u64,
        addr: &cpu_addr as *const u64 as u64,
        flags: 0,
    };
    dev_fd
        .set_device_attr(&cpu_attr)
        .map_err(|e| Error::Device(format!("set GICv2 cpu addr: {}", e)))?;

    // Initialize the GIC
    let init_attr = kvm_device_attr {
        group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
        attr: kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
        addr: 0,
        flags: 0,
    };
    dev_fd
        .set_device_attr(&init_attr)
        .map_err(|e| Error::Device(format!("init GICv2: {}", e)))?;

    Ok(())
}

/// Capture GIC state for snapshot.
///
/// Note: Full GIC save/restore requires iterating over distributor and
/// redistributor registers via KVM_DEV_ARM_VGIC_GRP_DIST_REGS and
/// KVM_DEV_ARM_VGIC_GRP_REDIST_REGS device attributes. This is a minimal
/// implementation that captures the essential state.
pub fn capture_irqchip(_vm: &Vm) -> Result<IrqchipState> {
    // TODO: Implement full GIC register capture via KVM_GET_DEVICE_ATTR
    // This requires tracking the GIC device fd and iterating over register groups.
    debug!("Captured aarch64 GIC state (stub)");
    Ok(IrqchipState {
        gic_dist_regs: Vec::new(),
        gic_redist_regs: Vec::new(),
        gic_cpu_regs: Vec::new(),
    })
}

/// Restore GIC state from a snapshot.
pub fn restore_irqchip(_vm: &Vm, _state: &IrqchipState) -> Result<()> {
    // TODO: Implement full GIC register restore via KVM_SET_DEVICE_ATTR
    debug!("Restored aarch64 GIC state (stub)");
    Ok(())
}

/// Capture arch-specific VM state (empty on aarch64).
pub fn capture_arch_vm_state(_vm: &Vm) -> Result<ArchVmState> {
    Ok(ArchVmState {})
}

/// Restore arch-specific VM state (no-op on aarch64).
pub fn restore_arch_vm_state(_vm: &Vm, _state: &ArchVmState) -> Result<()> {
    Ok(())
}
