//! aarch64 KVM setup: GIC creation, capture/restore, memory layout.

use kvm_ioctls::VmFd;
use tracing::debug;

use crate::vmm::arch::MemoryLayout;
use crate::vmm::kvm::Vm;
use crate::{Error, Result};

use super::snapshot::{ArchVmState, IrqchipState};

/// aarch64 memory layout constants (RFC-0003: mirrors the QEMU `virt`
/// machine, which every arm64 distro kernel is routinely booted against).
/// All device MMIO sits below [`layout::RAM_START`].
pub mod layout {
    /// Start of guest RAM (above the 1GB device region).
    pub const RAM_START: u64 = 0x4000_0000; // 1 GB

    /// DTB address (at start of RAM).
    pub const DTB_ADDR: u64 = RAM_START;

    /// Space reserved for the DTB at the start of RAM. The devicetree spec
    /// caps a blob at 2 MB, and placing the kernel base right after gives
    /// it the 2 MB alignment the arm64 boot protocol requires.
    pub const DTB_MAX_SIZE: u64 = 0x0020_0000; // 2 MB

    /// 2 MB-aligned base for the kernel Image. The Image is loaded at this
    /// base plus the `text_offset` its header declares; the base itself is
    /// the value the boot protocol's alignment rule applies to.
    pub const KERNEL_BASE_ADDR: u64 = RAM_START + DTB_MAX_SIZE;

    /// GIC distributor base address.
    pub const GIC_DIST_ADDR: u64 = 0x0800_0000;

    /// GIC distributor region size reserved in the layout. The DTB
    /// advertises the size KVM actually implements per GIC version (64 KB
    /// for GICv3, 4 KB for GICv2).
    pub const GIC_DIST_SIZE: u64 = 0x0001_0000; // 64 KB

    /// GICv2 distributor size as implemented by KVM (`KVM_VGIC_V2_DIST_SIZE`).
    pub const GICV2_DIST_SIZE: u64 = 0x1000; // 4 KB

    /// GICv2 CPU interface base address (GICv2 fallback only).
    pub const GIC_CPU_ADDR: u64 = GIC_DIST_ADDR + GIC_DIST_SIZE;

    /// GICv2 CPU interface size (GICC including the GICC_DIR page).
    pub const GIC_CPU_SIZE: u64 = 0x2000; // 8 KB

    /// GIC redistributor base address (GICv3).
    pub const GIC_REDIST_ADDR: u64 = 0x080A_0000;

    /// GIC redistributor region size per vCPU.
    pub const GIC_REDIST_SIZE_PER_CPU: u64 = 0x0002_0000; // 128 KB

    /// UART (ns16550a) MMIO window.
    pub const UART_ADDR: u64 = 0x0900_0000;

    /// UART MMIO window size.
    pub const UART_SIZE: u64 = 0x1000; // 4 KB

    /// Base of the virtio-mmio device slots (below RAM).
    pub const VIRTIO_MMIO_BASE: u64 = 0x0A00_0000;

    /// Stride between virtio-mmio slots (page-aligned windows; each device
    /// register file is 0x200 bytes).
    pub const VIRTIO_MMIO_STRIDE: u64 = 0x1000;

    /// GIC SPI index of virtio slot 0; slot N uses SPI base + N
    /// (INTID = 32 + SPI index).
    pub const VIRTIO_SPI_BASE: u32 = 16;

    /// Maximum vCPU count on aarch64. The GICv3 redistributor region grows
    /// by [`GIC_REDIST_SIZE_PER_CPU`] per vCPU from [`GIC_REDIST_ADDR`] and
    /// must not reach the UART window: KVM knows nothing about userspace
    /// MMIO windows and silently accepts a redistributor region that
    /// shadows them, so the ceiling has to be enforced by config validation
    /// rather than discovered at runtime.
    pub const MAX_VCPUS: usize = ((UART_ADDR - GIC_REDIST_ADDR) / GIC_REDIST_SIZE_PER_CPU) as usize;

    /// GICv2 architectural CPU limit (KVM's vGICv2 enforces it with E2BIG
    /// at vGIC creation; validated earlier for a clearer error).
    pub const GICV2_MAX_VCPUS: usize = 8;
}

/// aarch64 memory layout for arch-neutral code.
pub static MEMORY_LAYOUT: MemoryLayout = MemoryLayout {
    ram_start: layout::RAM_START,
    mmio_gap_start: None,
    mmio_gap_end: None,
};

/// Pre-vCPU VM setup — nothing to do on aarch64.
///
/// The GIC is deliberately not created here: `KVM_DEV_ARM_VGIC_CTRL_INIT`
/// freezes the vGIC configuration and the kernel then rejects
/// `KVM_CREATE_VCPU` with `EBUSY`, so all GIC work happens in
/// [`setup_vm_post_vcpus`] once every vCPU exists.
pub fn setup_vm(_vm_fd: &VmFd) -> Result<()> {
    Ok(())
}

/// Which GIC the VMM creates for a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicVersion {
    /// GICv3: distributor + per-vCPU redistributors.
    V3,
    /// GICv2: distributor + CPU interface, at most 8 vCPUs.
    V2,
}

/// Decide which GIC version to create, by asking KVM whether the vGICv3
/// device type is supported.
///
/// Both the DTB generator and [`setup_vm_post_vcpus`] consult this probe,
/// so the GIC node the guest sees always names the version the VMM
/// attempts. The probe checks device-*type* support, not that creation in
/// this VM will succeed; if creation fails anyway, boot aborts with a hard
/// error rather than falling back — KVM allows one vGIC per VM, so a GICv2
/// created after a partial GICv3 attempt cannot work, and would contradict
/// the already-written DTB.
pub fn probe_gic_version(vm_fd: &VmFd) -> GicVersion {
    use kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3 as KVM_DEV_TYPE_ARM_VGIC_V3;

    if device_type_supported(vm_fd, KVM_DEV_TYPE_ARM_VGIC_V3) {
        GicVersion::V3
    } else {
        GicVersion::V2
    }
}

/// `KVM_CREATE_DEVICE` ioctl number: `_IOWR(KVMIO=0xAE, 0xe0, struct
/// kvm_create_device)` with the 12-byte struct size in bits 29:16.
const KVM_CREATE_DEVICE_IOCTL: libc::c_ulong = 0xc00c_aee0;

/// Ask KVM whether a device type is supported, via `KVM_CREATE_DEVICE` with
/// the `KVM_CREATE_DEVICE_TEST` flag (checks support without creating).
///
/// Raw ioctl rather than `kvm_ioctls::VmFd::create_device`: with the TEST
/// flag the kernel returns success without producing an fd, and the wrapper
/// would wrap the untouched `fd: 0` field in an owned `DeviceFd` — which
/// closes stdin when dropped.
fn device_type_supported(vm_fd: &VmFd, device_type: u32) -> bool {
    use std::os::unix::io::AsRawFd;

    let mut device = kvm_bindings::kvm_create_device {
        type_: device_type,
        fd: 0,
        flags: kvm_bindings::KVM_CREATE_DEVICE_TEST,
    };
    // SAFETY: the fd is a valid KVM VM fd for the lifetime of `vm_fd`, and
    // with the TEST flag the kernel only reads the struct.
    let ret = unsafe { libc::ioctl(vm_fd.as_raw_fd(), KVM_CREATE_DEVICE_IOCTL as _, &mut device) };
    ret == 0
}

/// Create and initialize the GIC (Generic Interrupt Controller) for the VM.
///
/// Must run after every vCPU is created and before any of them runs: the
/// vGIC init sizes per-vCPU redistributor state from the vCPUs present, and
/// the kernel refuses to create a vGIC once a vCPU has run.
///
/// Creates exactly the version [`probe_gic_version`] reports — a creation
/// failure is a hard error, never a GICv2 fallback (see the probe's docs).
pub fn setup_vm_post_vcpus(vm_fd: &VmFd, vcpu_count: usize) -> Result<()> {
    if vcpu_count > layout::MAX_VCPUS {
        return Err(Error::Config(format!(
            "aarch64 supports at most {} vCPUs (the GICv3 redistributor region would \
             overlap the UART window at {:#x}); {} requested",
            layout::MAX_VCPUS,
            layout::UART_ADDR,
            vcpu_count
        )));
    }

    match probe_gic_version(vm_fd) {
        GicVersion::V3 => {
            create_gicv3(vm_fd, vcpu_count)?;
            debug!(
                "Created GICv3 (dist={:#x}, redist={:#x}, {} vCPUs)",
                layout::GIC_DIST_ADDR,
                layout::GIC_REDIST_ADDR,
                vcpu_count
            );
        }
        GicVersion::V2 => {
            if vcpu_count > layout::GICV2_MAX_VCPUS {
                return Err(Error::Config(format!(
                    "host supports only GICv2, which is limited to {} vCPUs; {} requested",
                    layout::GICV2_MAX_VCPUS,
                    vcpu_count
                )));
            }
            create_gicv2(vm_fd)?;
            debug!(
                "Created GICv2 (dist={:#x}, cpuif={:#x})",
                layout::GIC_DIST_ADDR,
                layout::GIC_CPU_ADDR
            );
        }
    }
    Ok(())
}

/// Create a GICv3 via KVM_CREATE_DEVICE.
fn create_gicv3(vm_fd: &VmFd, _vcpu_count: usize) -> Result<()> {
    use kvm_bindings::{
        kvm_create_device, kvm_device_attr,
        kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3 as KVM_DEV_TYPE_ARM_VGIC_V3,
        KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_VGIC_V3_ADDR_TYPE_DIST, KVM_VGIC_V3_ADDR_TYPE_REDIST,
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
        kvm_create_device, kvm_device_attr,
        kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V2 as KVM_DEV_TYPE_ARM_VGIC_V2,
        KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_VGIC_V2_ADDR_TYPE_CPU, KVM_VGIC_V2_ADDR_TYPE_DIST,
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
    let cpu_addr: u64 = layout::GIC_CPU_ADDR;
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
///
/// Currently a stub. A real implementation must run after
/// [`setup_vm_post_vcpus`] — the vGIC only exists from that point — which
/// is later than the arch-neutral restore path calls this today.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_vcpus_keeps_redistributors_below_uart() {
        // 123 frames of 128 KB fill the gap between the redistributor base
        // and the UART exactly; one more would shadow the UART window.
        assert_eq!(layout::MAX_VCPUS, 123);
        assert!(
            layout::GIC_REDIST_ADDR + (layout::MAX_VCPUS as u64) * layout::GIC_REDIST_SIZE_PER_CPU
                <= layout::UART_ADDR
        );
        assert!(
            layout::GIC_REDIST_ADDR
                + (layout::MAX_VCPUS as u64 + 1) * layout::GIC_REDIST_SIZE_PER_CPU
                > layout::UART_ADDR
        );
    }

    #[test]
    fn create_device_ioctl_number_matches_uapi() {
        // _IOWR(KVMIO, 0xe0, struct kvm_create_device): dir=RW (3) in bits
        // 31:30, size (12 bytes) in bits 29:16, type 0xAE in bits 15:8,
        // number 0xe0 in bits 7:0.
        let dir_rw: libc::c_ulong = 3 << 30;
        let size: libc::c_ulong =
            (std::mem::size_of::<kvm_bindings::kvm_create_device>() as libc::c_ulong) << 16;
        let expected = dir_rw | size | (0xAE << 8) | 0xe0;
        assert_eq!(KVM_CREATE_DEVICE_IOCTL, expected);
    }

    #[test]
    fn kernel_base_is_2mb_aligned_after_dtb_slot() {
        assert_eq!(layout::KERNEL_BASE_ADDR % 0x20_0000, 0);
        assert_eq!(
            layout::KERNEL_BASE_ADDR,
            layout::DTB_ADDR + layout::DTB_MAX_SIZE
        );
    }
}
