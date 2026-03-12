//! KVM setup and VM management.
//!
//! Architecture-specific setup (irqchip, PIT, GIC) is handled by the
//! [`arch`](crate::vmm::arch) module.

use kvm_bindings::{kvm_userspace_memory_region, KVM_MEM_LOG_DIRTY_PAGES};
use kvm_ioctls::{Kvm, VmFd};
use tracing::debug;
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use crate::vmm::arch::{Arch, CurrentArch};
use crate::{Error, Result};

/// Represents a KVM virtual machine.
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
    /// Create a new KVM VM with the specified memory size.
    ///
    /// Also performs arch-specific setup (irqchip + PIT on x86, GIC on aarch64)
    /// via [`CurrentArch::setup_vm`].
    pub fn new(memory_mb: usize) -> Result<Self> {
        Self::with_vcpu_count(memory_mb, 1)
    }

    /// Create a new KVM VM, passing the vCPU count for arch-specific setup.
    pub fn with_vcpu_count(memory_mb: usize, vcpu_count: usize) -> Result<Self> {
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

        // Arch-specific VM setup (irqchip + PIT on x86, GIC on aarch64)
        CurrentArch::setup_vm(&vm.vm_fd, vcpu_count)?;

        Ok(vm)
    }

    /// Check that required KVM extensions are available.
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

    /// Create guest memory regions based on the current arch layout.
    fn create_guest_memory(memory_size: u64) -> Result<GuestMemoryMmap> {
        let layout = CurrentArch::memory_layout();
        let effective_size = if let Some(gap_start) = layout.mmio_gap_start {
            std::cmp::min(memory_size, gap_start - layout.ram_start)
        } else {
            memory_size
        };

        let mem_region = (GuestAddress(layout.ram_start), effective_size as usize);

        GuestMemoryMmap::from_ranges(&[mem_region])
            .map_err(|e| Error::Memory(format!("Failed to create guest memory: {}", e)))
    }

    /// Register memory regions with KVM.
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

    /// Get reference to the KVM handle.
    pub fn kvm(&self) -> &Kvm {
        &self.kvm
    }

    /// Get reference to the VM file descriptor.
    pub fn vm_fd(&self) -> &VmFd {
        &self.vm_fd
    }

    /// Get reference to guest memory.
    pub fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.guest_memory
    }

    /// Get memory size in bytes.
    pub fn memory_size(&self) -> u64 {
        self.memory_size
    }

    /// Create a vCPU for this VM.
    pub fn create_vcpu(&self, id: u64) -> Result<kvm_ioctls::VcpuFd> {
        self.vm_fd.create_vcpu(id).map_err(Error::Kvm)
    }

    // ------------------------------------------------------------------
    // Dirty page tracking (arch-neutral)
    // ------------------------------------------------------------------

    /// Enable dirty page logging on all memory regions.
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
    fn test_memory_layout() {
        let layout = CurrentArch::memory_layout();
        // RAM start should be valid
        assert!(layout.ram_start < u64::MAX);
    }
}
