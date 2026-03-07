//! KVM setup and VM management

use kvm_bindings::{
    kvm_irqchip, kvm_pit_config, kvm_pit_state2, kvm_userspace_memory_region,
    KVM_MEM_LOG_DIRTY_PAGES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VmFd};
use tracing::debug;
use vm_memory::{Address, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use crate::vmm::snapshot::{kvm_struct_from_bytes, kvm_struct_to_bytes, IrqchipState};
use crate::{Error, Result};

/// x86_64 memory layout constants
pub mod layout {
    use vm_memory::GuestAddress;

    /// Start of RAM
    pub const RAM_START: GuestAddress = GuestAddress(0);

    /// Start of the MMIO gap (for legacy devices)
    pub const MMIO_GAP_START: u64 = 0xD000_0000; // 3.25 GB

    /// End of the MMIO gap
    pub const MMIO_GAP_END: u64 = 0x1_0000_0000; // 4 GB

    /// High RAM start (above 4GB)
    pub const HIGH_RAM_START: GuestAddress = GuestAddress(0x1_0000_0000);

    /// Kernel load address
    pub const KERNEL_LOAD_ADDR: GuestAddress = GuestAddress(0x0100_0000); // 16 MB

    /// Initramfs load address
    pub const INITRAMFS_LOAD_ADDR: GuestAddress = GuestAddress(0x0400_0000); // 64 MB

    /// Boot parameters (zero page) address
    pub const BOOT_PARAMS_ADDR: GuestAddress = GuestAddress(0x0000_7000);

    /// Kernel command line address
    pub const CMDLINE_ADDR: GuestAddress = GuestAddress(0x0002_0000);

    /// Maximum kernel command line size
    pub const CMDLINE_MAX_SIZE: usize = 4096;

    /// PCI MMIO space start
    pub const PCI_MMIO_START: u64 = 0xC000_0000;

    /// PCI MMIO space size
    pub const PCI_MMIO_SIZE: u64 = 0x1000_0000; // 256 MB
}

/// Represents a KVM virtual machine
pub struct Vm {
    /// KVM system handle
    kvm: Kvm,
    /// VM file descriptor
    vm_fd: VmFd,
    /// Guest memory mapping
    guest_memory: GuestMemoryMmap,
    /// Memory size in bytes
    memory_size: u64,
}

impl Vm {
    /// Create a new KVM VM with the specified memory size
    pub fn new(memory_mb: usize) -> Result<Self> {
        let memory_size = (memory_mb as u64) * 1024 * 1024;

        // Open /dev/kvm
        let kvm = Kvm::new().map_err(Error::Kvm)?;
        debug!("KVM API version: {}", kvm.get_api_version());

        // Check required extensions
        Self::check_extensions(&kvm)?;

        // Create the VM
        let vm_fd = kvm.create_vm().map_err(Error::Kvm)?;
        debug!("Created KVM VM");

        // Create guest memory
        let guest_memory = Self::create_guest_memory(memory_size)?;
        debug!("Created guest memory: {} MB", memory_mb);

        let vm = Self {
            kvm,
            vm_fd,
            guest_memory,
            memory_size,
        };

        // Register memory with KVM
        vm.register_memory()?;

        // Create irqchip (for interrupt handling)
        vm.vm_fd.create_irq_chip().map_err(Error::Kvm)?;
        debug!("Created IRQ chip");

        // Create PIT (Programmable Interval Timer)
        let pit_config = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm.vm_fd.create_pit2(pit_config).map_err(Error::Kvm)?;
        debug!("Created PIT");

        Ok(vm)
    }

    /// Check that required KVM extensions are available
    fn check_extensions(kvm: &Kvm) -> Result<()> {
        use kvm_ioctls::Cap;

        let required_caps = [(Cap::Irqchip, "IRQCHIP"), (Cap::UserMemory, "USER_MEMORY")];

        for (cap, name) in required_caps {
            if !kvm.check_extension(cap) {
                return Err(Error::Kvm(kvm_ioctls::Error::new(libc::ENOTSUP)));
            }
            debug!("KVM capability {} available", name);
        }

        Ok(())
    }

    /// Create guest memory regions
    fn create_guest_memory(memory_size: u64) -> Result<GuestMemoryMmap> {
        // For simplicity, create a single memory region below the MMIO gap
        // For larger VMs, we'd need to split around the gap
        let effective_size = std::cmp::min(memory_size, layout::MMIO_GAP_START);

        let mem_region = (layout::RAM_START, effective_size as usize);

        GuestMemoryMmap::from_ranges(&[mem_region])
            .map_err(|e| Error::Memory(format!("Failed to create guest memory: {}", e)))
    }

    /// Register memory regions with KVM
    fn register_memory(&self) -> Result<()> {
        for (index, region) in self.guest_memory.iter().enumerate() {
            let memory_region = kvm_userspace_memory_region {
                slot: index as u32,
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: self
                    .guest_memory
                    .get_host_address(region.start_addr())
                    .unwrap() as u64,
                flags: 0,
            };

            // SAFETY: We're passing a valid memory region that will remain valid
            // for the lifetime of the VM
            unsafe {
                self.vm_fd
                    .set_user_memory_region(memory_region)
                    .map_err(Error::Kvm)?;
            }

            debug!(
                "Registered memory region {}: addr={:#x}, size={:#x}",
                index,
                region.start_addr().raw_value(),
                region.len()
            );
        }

        Ok(())
    }

    /// Get reference to the KVM handle
    pub fn kvm(&self) -> &Kvm {
        &self.kvm
    }

    /// Get reference to the VM file descriptor
    pub fn vm_fd(&self) -> &VmFd {
        &self.vm_fd
    }

    /// Get reference to guest memory
    pub fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.guest_memory
    }

    /// Get memory size in bytes
    pub fn memory_size(&self) -> u64 {
        self.memory_size
    }

    /// Create a vCPU for this VM
    pub fn create_vcpu(&self, id: u64) -> Result<kvm_ioctls::VcpuFd> {
        self.vm_fd.create_vcpu(id).map_err(Error::Kvm)
    }

    // ------------------------------------------------------------------
    // Snapshot: IRQchip + PIT capture / restore
    // ------------------------------------------------------------------

    /// Capture the in-kernel irqchip state (PIC master, PIC slave, IOAPIC).
    pub fn capture_irqchip(&self) -> Result<IrqchipState> {
        let pic_master = self.get_irqchip_raw(0)?; // KVM_IRQCHIP_PIC_MASTER
        let pic_slave = self.get_irqchip_raw(1)?; // KVM_IRQCHIP_PIC_SLAVE
        let ioapic = self.get_irqchip_raw(2)?; // KVM_IRQCHIP_IOAPIC
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
    pub fn restore_irqchip(&self, state: &IrqchipState) -> Result<()> {
        self.set_irqchip_raw(0, &state.pic_master)?;
        self.set_irqchip_raw(1, &state.pic_slave)?;
        self.set_irqchip_raw(2, &state.ioapic)?;
        debug!("Restored irqchip state");
        Ok(())
    }

    /// Capture PIT (Programmable Interval Timer) state.
    pub fn capture_pit(&self) -> Result<Vec<u8>> {
        let pit = self.vm_fd.get_pit2().map_err(Error::Kvm)?;
        let bytes = kvm_struct_to_bytes(&pit);
        debug!("Captured PIT state ({} bytes)", bytes.len());
        Ok(bytes)
    }

    /// Restore PIT state from a snapshot.
    pub fn restore_pit(&self, data: &[u8]) -> Result<()> {
        let pit: kvm_pit_state2 = kvm_struct_from_bytes(data)?;
        self.vm_fd.set_pit2(&pit).map_err(Error::Kvm)?;
        debug!("Restored PIT state");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Snapshot: dirty page tracking
    // ------------------------------------------------------------------

    /// Enable dirty page logging on all memory regions.
    ///
    /// After this call, KVM tracks which guest pages are written to.
    /// Use `get_dirty_bitmap` to retrieve the bitmap of modified pages.
    pub fn enable_dirty_log(&self) -> Result<()> {
        for (index, region) in self.guest_memory.iter().enumerate() {
            let memory_region = kvm_userspace_memory_region {
                slot: index as u32,
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: self
                    .guest_memory
                    .get_host_address(region.start_addr())
                    .unwrap() as u64,
                flags: KVM_MEM_LOG_DIRTY_PAGES,
            };

            unsafe {
                self.vm_fd
                    .set_user_memory_region(memory_region)
                    .map_err(Error::Kvm)?;
            }

            debug!(
                "Enabled dirty log on memory slot {}: addr={:#x}, size={:#x}",
                index,
                region.start_addr().raw_value(),
                region.len()
            );
        }
        Ok(())
    }

    /// Disable dirty page logging on all memory regions.
    ///
    /// Re-registers memory regions without the `KVM_MEM_LOG_DIRTY_PAGES` flag.
    pub fn disable_dirty_log(&self) -> Result<()> {
        for (index, region) in self.guest_memory.iter().enumerate() {
            let memory_region = kvm_userspace_memory_region {
                slot: index as u32,
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: self
                    .guest_memory
                    .get_host_address(region.start_addr())
                    .unwrap() as u64,
                flags: 0,
            };

            unsafe {
                self.vm_fd
                    .set_user_memory_region(memory_region)
                    .map_err(Error::Kvm)?;
            }
        }
        debug!("Disabled dirty log on all memory slots");
        Ok(())
    }

    /// Retrieve the dirty page bitmap for all memory slots.
    ///
    /// Returns a vector of `(slot_index, bitmap)` pairs. Each bit in the
    /// bitmap corresponds to one 4 KiB page: bit N = 1 means page N was
    /// written since the last `get_dirty_bitmap` or `enable_dirty_log` call.
    ///
    /// The bitmap is a `Vec<u64>` where each u64 covers 64 pages (256 KiB).
    pub fn get_dirty_bitmap(&self) -> Result<Vec<(u32, Vec<u64>)>> {
        let mut result = Vec::new();
        for (index, region) in self.guest_memory.iter().enumerate() {
            let slot = index as u32;
            let mem_size = region.len() as usize;
            let bitmap = self
                .vm_fd
                .get_dirty_log(slot, mem_size)
                .map_err(Error::Kvm)?;
            let dirty_count: u32 = bitmap.iter().map(|w| w.count_ones()).sum();
            debug!(
                "Dirty bitmap slot {}: {} pages dirty out of {} total",
                slot,
                dirty_count,
                mem_size / 4096
            );
            result.push((slot, bitmap));
        }
        Ok(result)
    }

    fn get_irqchip_raw(&self, chip_id: u32) -> Result<Vec<u8>> {
        let mut chip = kvm_irqchip {
            chip_id,
            ..Default::default()
        };
        self.vm_fd.get_irqchip(&mut chip).map_err(Error::Kvm)?;
        Ok(kvm_struct_to_bytes(&chip))
    }

    fn set_irqchip_raw(&self, chip_id: u32, data: &[u8]) -> Result<()> {
        let mut chip: kvm_irqchip = kvm_struct_from_bytes(data)?;
        chip.chip_id = chip_id; // ensure chip_id matches
        self.vm_fd.set_irqchip(&chip).map_err(Error::Kvm)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // Requires KVM
    fn test_create_vm() {
        let vm = Vm::new(64).expect("Failed to create VM");
        assert_eq!(vm.memory_size(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_layout_constants() {
        // Verify memory layout makes sense
        assert!(layout::KERNEL_LOAD_ADDR.raw_value() > layout::BOOT_PARAMS_ADDR.raw_value());
        assert!(layout::INITRAMFS_LOAD_ADDR.raw_value() > layout::KERNEL_LOAD_ADDR.raw_value());
        const { assert!(layout::MMIO_GAP_START < layout::MMIO_GAP_END) };
    }
}
