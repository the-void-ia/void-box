//! aarch64 kernel loading: Image binary + DTB generation.
//!
//! ARM64 Linux boot protocol:
//!   - Kernel is an uncompressed `Image` (or ELF) loaded at a 2MB-aligned address
//!   - DTB (Device Tree Blob) is placed at the start of RAM
//!   - x0 = DTB physical address, PC = kernel entry point
//!   - The DTB describes memory, chosen (bootargs, initrd), and PSCI

use std::fs::File;
use std::io::Read;
use std::path::Path;

use tracing::debug;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use crate::vmm::kvm::Vm;
use crate::vmm::memory::write_to_guest;
use crate::{Error, Result};

use super::kvm::layout;

/// Load kernel (Image) and optionally initramfs into guest memory.
///
/// Generates a DTB with memory, chosen (bootargs, initrd), and PSCI nodes.
/// Returns the kernel entry point address.
pub fn load_kernel(
    vm: &Vm,
    kernel_path: &Path,
    initramfs_path: Option<&Path>,
    cmdline: &str,
) -> Result<u64> {
    let guest_memory = vm.guest_memory();
    let memory_size = vm.memory_size();

    // Load kernel Image at KERNEL_LOAD_ADDR
    let kernel_data = std::fs::read(kernel_path)
        .map_err(|e| Error::Boot(format!("Failed to read kernel: {}", e)))?;

    let kernel_addr = GuestAddress(layout::KERNEL_LOAD_ADDR);
    guest_memory
        .write(&kernel_data, kernel_addr)
        .map_err(|e| Error::Boot(format!("Failed to write kernel to guest memory: {}", e)))?;
    debug!(
        "Loaded aarch64 kernel Image at {:#x} ({} bytes)",
        kernel_addr.raw_value(),
        kernel_data.len()
    );

    // Entry point is the start of the Image
    let entry_point = kernel_addr.raw_value();

    // Load initramfs if provided
    let initramfs_info = if let Some(initramfs) = initramfs_path {
        let mut f = File::open(initramfs)
            .map_err(|e| Error::Boot(format!("Failed to open initramfs: {}", e)))?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)
            .map_err(|e| Error::Boot(format!("Failed to read initramfs: {}", e)))?;

        let initramfs_addr = GuestAddress(layout::INITRAMFS_LOAD_ADDR);
        guest_memory
            .write(&data, initramfs_addr)
            .map_err(|e| Error::Boot(format!("Failed to write initramfs: {}", e)))?;
        debug!(
            "Loaded initramfs at {:#x} ({} bytes)",
            initramfs_addr.raw_value(),
            data.len()
        );
        Some((initramfs_addr.raw_value(), data.len() as u64))
    } else {
        None
    };

    // Generate and write DTB
    let dtb = generate_dtb(layout::RAM_START, memory_size, cmdline, initramfs_info)?;

    let dtb_addr = GuestAddress(layout::DTB_ADDR);
    write_to_guest(guest_memory, dtb_addr, &dtb)?;
    debug!(
        "Wrote DTB at {:#x} ({} bytes)",
        dtb_addr.raw_value(),
        dtb.len()
    );

    Ok(entry_point)
}

/// Generate a minimal DTB for the guest.
fn generate_dtb(
    ram_start: u64,
    memory_size: u64,
    cmdline: &str,
    initramfs_info: Option<(u64, u64)>,
) -> Result<Vec<u8>> {
    use vm_fdt::FdtWriter;

    let mut fdt = FdtWriter::new().map_err(|e| Error::Boot(format!("FdtWriter::new: {}", e)))?;

    // Root node
    let root = fdt
        .begin_node("")
        .map_err(|e| Error::Boot(format!("begin root: {}", e)))?;

    fdt.property_string("compatible", "linux,dummy-virt")
        .map_err(|e| Error::Boot(format!("root compatible: {}", e)))?;
    fdt.property_u32("#address-cells", 2)
        .map_err(|e| Error::Boot(format!("root #address-cells: {}", e)))?;
    fdt.property_u32("#size-cells", 2)
        .map_err(|e| Error::Boot(format!("root #size-cells: {}", e)))?;

    // Memory node
    let mem_node = fdt
        .begin_node(&format!("memory@{:x}", ram_start))
        .map_err(|e| Error::Boot(format!("begin memory: {}", e)))?;
    fdt.property_string("device_type", "memory")
        .map_err(|e| Error::Boot(format!("memory device_type: {}", e)))?;
    fdt.property_array_u64("reg", &[ram_start, memory_size])
        .map_err(|e| Error::Boot(format!("memory reg: {}", e)))?;
    fdt.end_node(mem_node)
        .map_err(|e| Error::Boot(format!("end memory: {}", e)))?;

    // Chosen node (bootargs + initrd)
    let chosen = fdt
        .begin_node("chosen")
        .map_err(|e| Error::Boot(format!("begin chosen: {}", e)))?;
    fdt.property_string("bootargs", cmdline)
        .map_err(|e| Error::Boot(format!("chosen bootargs: {}", e)))?;

    if let Some((initrd_start, initrd_size)) = initramfs_info {
        fdt.property_u64("linux,initrd-start", initrd_start)
            .map_err(|e| Error::Boot(format!("initrd-start: {}", e)))?;
        fdt.property_u64("linux,initrd-end", initrd_start + initrd_size)
            .map_err(|e| Error::Boot(format!("initrd-end: {}", e)))?;
    }

    fdt.end_node(chosen)
        .map_err(|e| Error::Boot(format!("end chosen: {}", e)))?;

    // PSCI node (Power State Coordination Interface)
    let psci = fdt
        .begin_node("psci")
        .map_err(|e| Error::Boot(format!("begin psci: {}", e)))?;
    fdt.property_string("compatible", "arm,psci-1.0")
        .map_err(|e| Error::Boot(format!("psci compatible: {}", e)))?;
    fdt.property_string("method", "hvc")
        .map_err(|e| Error::Boot(format!("psci method: {}", e)))?;
    fdt.end_node(psci)
        .map_err(|e| Error::Boot(format!("end psci: {}", e)))?;

    // Timer node (ARM architected timer)
    let timer = fdt
        .begin_node("timer")
        .map_err(|e| Error::Boot(format!("begin timer: {}", e)))?;
    fdt.property_string("compatible", "arm,armv8-timer")
        .map_err(|e| Error::Boot(format!("timer compatible: {}", e)))?;
    // GIC SPI interrupts for: secure phys, non-secure phys, virt, hyp phys
    fdt.property_array_u32(
        "interrupts",
        &[
            1, 13, 0xf08, // Secure physical timer
            1, 14, 0xf08, // Non-secure physical timer
            1, 11, 0xf08, // Virtual timer
            1, 10, 0xf08, // Hypervisor physical timer
        ],
    )
    .map_err(|e| Error::Boot(format!("timer interrupts: {}", e)))?;
    fdt.property_null("always-on")
        .map_err(|e| Error::Boot(format!("timer always-on: {}", e)))?;
    fdt.end_node(timer)
        .map_err(|e| Error::Boot(format!("end timer: {}", e)))?;

    fdt.end_node(root)
        .map_err(|e| Error::Boot(format!("end root: {}", e)))?;

    let dtb_bytes = fdt
        .finish()
        .map_err(|e| Error::Boot(format!("FDT finish: {}", e)))?;

    Ok(dtb_bytes)
}
