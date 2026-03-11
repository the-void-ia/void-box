//! x86_64 KVM setup: irqchip, PIT, clock, memory layout.

use kvm_bindings::{kvm_irqchip, kvm_pit_config, KVM_PIT_SPEAKER_DUMMY};
use kvm_ioctls::VmFd;
use tracing::debug;

use crate::vmm::arch::MemoryLayout;
use crate::vmm::kvm::Vm;
use crate::vmm::snapshot::{kvm_struct_from_bytes, kvm_struct_to_bytes};
use crate::{Error, Result};

use super::snapshot::{ArchVmState, IrqchipState};

/// x86_64 memory layout constants.
pub mod layout {
    use vm_memory::GuestAddress;

    /// Start of RAM.
    pub const RAM_START: GuestAddress = GuestAddress(0);

    /// Start of the MMIO gap (for legacy devices).
    pub const MMIO_GAP_START: u64 = 0xD000_0000; // 3.25 GB

    /// End of the MMIO gap.
    pub const MMIO_GAP_END: u64 = 0x1_0000_0000; // 4 GB

    /// High RAM start (above 4GB).
    pub const HIGH_RAM_START: GuestAddress = GuestAddress(0x1_0000_0000);

    /// Kernel load address.
    pub const KERNEL_LOAD_ADDR: GuestAddress = GuestAddress(0x0100_0000); // 16 MB

    /// Initramfs load address.
    pub const INITRAMFS_LOAD_ADDR: GuestAddress = GuestAddress(0x0400_0000); // 64 MB

    /// Boot parameters (zero page) address.
    pub const BOOT_PARAMS_ADDR: GuestAddress = GuestAddress(0x0000_7000);

    /// Kernel command line address.
    pub const CMDLINE_ADDR: GuestAddress = GuestAddress(0x0002_0000);

    /// Maximum kernel command line size.
    pub const CMDLINE_MAX_SIZE: usize = 4096;

    /// PCI MMIO space start.
    pub const PCI_MMIO_START: u64 = 0xC000_0000;

    /// PCI MMIO space size.
    pub const PCI_MMIO_SIZE: u64 = 0x1000_0000; // 256 MB
}

/// x86_64 memory layout for arch-neutral code.
pub static MEMORY_LAYOUT: MemoryLayout = MemoryLayout {
    ram_start: 0,
    mmio_gap_start: Some(layout::MMIO_GAP_START),
    mmio_gap_end: Some(layout::MMIO_GAP_END),
};

/// Create the in-kernel irqchip (PIC + IOAPIC) and PIT.
pub fn setup_vm(vm_fd: &VmFd, _vcpu_count: usize) -> Result<()> {
    vm_fd.create_irq_chip().map_err(Error::Kvm)?;
    debug!("Created IRQ chip");

    let pit_config = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm_fd.create_pit2(pit_config).map_err(Error::Kvm)?;
    debug!("Created PIT");

    Ok(())
}

/// Capture the in-kernel irqchip state (PIC master, PIC slave, IOAPIC).
pub fn capture_irqchip(vm: &Vm) -> Result<IrqchipState> {
    let pic_master = get_irqchip_raw(vm.vm_fd(), 0)?; // KVM_IRQCHIP_PIC_MASTER
    let pic_slave = get_irqchip_raw(vm.vm_fd(), 1)?; // KVM_IRQCHIP_PIC_SLAVE
    let ioapic = get_irqchip_raw(vm.vm_fd(), 2)?; // KVM_IRQCHIP_IOAPIC
    debug!(
        "Captured irqchip state ({} + {} + {} bytes)",
        pic_master.len(),
        pic_slave.len(),
        ioapic.len()
    );
    Ok(IrqchipState {
        pic_master,
        pic_slave,
        ioapic,
    })
}

/// Restore the in-kernel irqchip state from a snapshot.
///
/// Clears the `remote_irr` bit on all IOAPIC redirection table entries
/// before restoring.  For level-triggered entries, `remote_irr` signals
/// that an interrupt has been delivered but the guest hasn't sent EOI yet.
/// After a snapshot/restore cycle the LAPIC state may not match, so a stale
/// `remote_irr` permanently blocks new interrupts on that pin.
pub fn restore_irqchip(vm: &Vm, state: &IrqchipState) -> Result<()> {
    set_irqchip_raw(vm.vm_fd(), 0, &state.pic_master)?;
    set_irqchip_raw(vm.vm_fd(), 1, &state.pic_slave)?;

    // Clear remote_irr bits in the IOAPIC redirtbl before restoring.
    let mut ioapic_data = state.ioapic.clone();
    const REDIRTBL_OFFSET: usize = 8 + 24; // chip header + ioapic header
    const NUM_PINS: usize = 24;
    for pin in 0..NUM_PINS {
        let entry_base = REDIRTBL_OFFSET + pin * 8;
        if entry_base + 2 <= ioapic_data.len() {
            // remote_irr = bit 14 of the entry = byte 1, bit 6
            if ioapic_data[entry_base + 1] & 0x40 != 0 {
                ioapic_data[entry_base + 1] &= !0x40;
                debug!("Cleared remote_irr on IOAPIC pin {}", pin);
            }
        }
    }
    set_irqchip_raw(vm.vm_fd(), 2, &ioapic_data)?;

    debug!("Restored irqchip state");
    Ok(())
}

/// Capture arch-specific VM state (PIT + KVM clock).
pub fn capture_arch_vm_state(vm: &Vm) -> Result<ArchVmState> {
    let pit = vm.vm_fd().get_pit2().map_err(Error::Kvm)?;
    let pit_bytes = kvm_struct_to_bytes(&pit);
    debug!("Captured PIT state ({} bytes)", pit_bytes.len());

    let clock = vm.vm_fd().get_clock().map_err(Error::Kvm)?;
    let clock_bytes = kvm_struct_to_bytes(&clock);
    debug!("Captured KVM clock ({} bytes)", clock_bytes.len());

    Ok(ArchVmState {
        pit: pit_bytes,
        clock: clock_bytes,
    })
}

/// Restore arch-specific VM state (PIT + KVM clock).
pub fn restore_arch_vm_state(vm: &Vm, state: &ArchVmState) -> Result<()> {
    // Restore KVM clock if present.
    if !state.clock.is_empty() {
        let clock: kvm_bindings::kvm_clock_data = kvm_struct_from_bytes(&state.clock)?;
        vm.vm_fd().set_clock(&clock).map_err(Error::Kvm)?;
        debug!("Restored KVM clock");
    }
    // Note: PIT is NOT restored by default — see mod.rs snapshot_internal
    // comments for the rationale (IRQ 0 collisions).
    Ok(())
}

fn get_irqchip_raw(vm_fd: &VmFd, chip_id: u32) -> Result<Vec<u8>> {
    let mut chip = kvm_irqchip {
        chip_id,
        ..Default::default()
    };
    vm_fd.get_irqchip(&mut chip).map_err(Error::Kvm)?;
    Ok(kvm_struct_to_bytes(&chip))
}

fn set_irqchip_raw(vm_fd: &VmFd, chip_id: u32, data: &[u8]) -> Result<()> {
    let mut chip: kvm_irqchip = kvm_struct_from_bytes(data)?;
    chip.chip_id = chip_id;
    vm_fd.set_irqchip(&chip).map_err(Error::Kvm)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::Address;

    #[test]
    fn test_layout_constants() {
        assert!(layout::KERNEL_LOAD_ADDR.raw_value() > layout::BOOT_PARAMS_ADDR.raw_value());
        assert!(layout::INITRAMFS_LOAD_ADDR.raw_value() > layout::KERNEL_LOAD_ADDR.raw_value());
        const { assert!(layout::MMIO_GAP_START < layout::MMIO_GAP_END) };
    }
}
