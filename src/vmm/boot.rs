//! Kernel loading and boot parameter setup

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use linux_loader::bootparam::boot_params;
use linux_loader::loader::elf::Elf as ElfLoader;
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::KernelLoader;
use tracing::{debug, info};
use vm_memory::{Address, ByteValued, GuestAddress, GuestMemoryMmap};

use crate::vmm::kvm::{layout, Vm};
use crate::vmm::memory::{write_to_guest, zero_guest_memory};
use crate::{Error, Result};

/// Magic number for bzImage
const BZIMAGE_MAGIC: u32 = 0x53726448; // "HdrS"

/// Load kernel and optionally initramfs into guest memory
pub fn load_kernel(
    vm: &Vm,
    kernel_path: &Path,
    initramfs_path: Option<&Path>,
    cmdline: &str,
) -> Result<u64> {
    let guest_memory = vm.guest_memory();

    // Detect kernel format and load it
    let (kernel_load_addr, kernel_entry) = load_kernel_image(guest_memory, kernel_path)?;
    info!(
        "Loaded kernel at {:#x}, entry point {:#x}",
        kernel_load_addr, kernel_entry
    );

    // Load initramfs if provided
    let initramfs_info = if let Some(initramfs) = initramfs_path {
        Some(load_initramfs(guest_memory, initramfs)?)
    } else {
        None
    };

    // Set up boot parameters
    setup_boot_params(guest_memory, cmdline, initramfs_info)?;

    // Set up initial page tables for 64-bit mode
    setup_page_tables(guest_memory)?;

    Ok(kernel_entry)
}

/// Load kernel image (supports bzImage and ELF formats)
fn load_kernel_image(
    guest_memory: &GuestMemoryMmap,
    kernel_path: &Path,
) -> Result<(u64, u64)> {
    let mut kernel_file = File::open(kernel_path)
        .map_err(|e| Error::Boot(format!("Failed to open kernel: {}", e)))?;

    // Check if it's a bzImage
    if is_bzimage(&mut kernel_file)? {
        debug!("Loading bzImage kernel");
        load_bzimage(guest_memory, &mut kernel_file)
    } else {
        debug!("Loading ELF kernel");
        load_elf_kernel(guest_memory, &mut kernel_file)
    }
}

/// Check if kernel is bzImage format
fn is_bzimage(kernel_file: &mut File) -> Result<bool> {
    kernel_file.seek(SeekFrom::Start(0x202))
        .map_err(|e| Error::Boot(format!("Failed to seek kernel: {}", e)))?;

    let mut magic = [0u8; 4];
    kernel_file.read_exact(&mut magic)
        .map_err(|e| Error::Boot(format!("Failed to read kernel magic: {}", e)))?;

    kernel_file.seek(SeekFrom::Start(0))
        .map_err(|e| Error::Boot(format!("Failed to seek kernel: {}", e)))?;

    Ok(u32::from_le_bytes(magic) == BZIMAGE_MAGIC)
}

/// Load bzImage format kernel
fn load_bzimage(
    guest_memory: &GuestMemoryMmap,
    kernel_file: &mut File,
) -> Result<(u64, u64)> {
    let kernel_load = BzImage::load(
        guest_memory,
        None, // Use default kernel offset
        kernel_file,
        None, // highmem_start_address
    )
    .map_err(|e| Error::Boot(format!("Failed to load bzImage: {:?}", e)))?;

    let load_addr = kernel_load.kernel_load.raw_value();
    // kernel_end is already u64 in linux-loader 0.13

    // For bzImage, the entry point is at the 64-bit entry in the header
    // The actual entry depends on the boot protocol
    Ok((load_addr, layout::KERNEL_LOAD_ADDR.raw_value()))
}

/// Load ELF format kernel
fn load_elf_kernel(
    guest_memory: &GuestMemoryMmap,
    kernel_file: &mut File,
) -> Result<(u64, u64)> {
    let kernel_load = ElfLoader::load(
        guest_memory,
        None, // Use default kernel offset
        kernel_file,
        None, // highmem_start_address
    )
    .map_err(|e| Error::Boot(format!("Failed to load ELF kernel: {:?}", e)))?;

    let load_addr = kernel_load.kernel_load.raw_value();
    // kernel_end is already u64 in linux-loader 0.13
    let entry_point = kernel_load.kernel_end;

    Ok((load_addr, entry_point))
}

/// Load initramfs into guest memory
fn load_initramfs(
    guest_memory: &GuestMemoryMmap,
    initramfs_path: &Path,
) -> Result<(u64, u64)> {
    let mut initramfs_file = File::open(initramfs_path)
        .map_err(|e| Error::Boot(format!("Failed to open initramfs: {}", e)))?;

    let mut initramfs_data = Vec::new();
    initramfs_file
        .read_to_end(&mut initramfs_data)
        .map_err(|e| Error::Boot(format!("Failed to read initramfs: {}", e)))?;

    let initramfs_addr = layout::INITRAMFS_LOAD_ADDR;
    let initramfs_size = initramfs_data.len() as u64;

    write_to_guest(guest_memory, initramfs_addr, &initramfs_data)?;

    info!(
        "Loaded initramfs at {:#x}, size {} bytes",
        initramfs_addr.raw_value(),
        initramfs_size
    );

    Ok((initramfs_addr.raw_value(), initramfs_size))
}

/// Set up boot parameters (zero page)
fn setup_boot_params(
    guest_memory: &GuestMemoryMmap,
    cmdline: &str,
    initramfs_info: Option<(u64, u64)>,
) -> Result<()> {
    // Write command line
    let cmdline_bytes = cmdline.as_bytes();
    if cmdline_bytes.len() >= layout::CMDLINE_MAX_SIZE {
        return Err(Error::Boot("Kernel command line too long".into()));
    }

    // Null-terminate the command line
    let mut cmdline_with_null = cmdline_bytes.to_vec();
    cmdline_with_null.push(0);
    write_to_guest(guest_memory, layout::CMDLINE_ADDR, &cmdline_with_null)?;
    debug!("Wrote kernel command line: {}", cmdline);

    // Set up boot_params structure
    let mut boot_params = boot_params::default();

    // Set up header fields
    boot_params.hdr.type_of_loader = 0xFF; // Unknown loader
    boot_params.hdr.boot_flag = 0xAA55;
    boot_params.hdr.header = BZIMAGE_MAGIC;
    boot_params.hdr.cmd_line_ptr = layout::CMDLINE_ADDR.raw_value() as u32;
    boot_params.hdr.cmdline_size = cmdline_bytes.len() as u32;
    boot_params.hdr.kernel_alignment = 0x1000000; // 16MB alignment

    // Set up initramfs if provided
    if let Some((addr, size)) = initramfs_info {
        boot_params.hdr.ramdisk_image = addr as u32;
        boot_params.hdr.ramdisk_size = size as u32;
    }

    // Write boot_params to guest memory
    let boot_params_bytes = boot_params.as_slice();
    write_to_guest(guest_memory, layout::BOOT_PARAMS_ADDR, boot_params_bytes)?;
    debug!(
        "Wrote boot params at {:#x}",
        layout::BOOT_PARAMS_ADDR.raw_value()
    );

    Ok(())
}

/// Set up initial page tables for 64-bit mode
fn setup_page_tables(guest_memory: &GuestMemoryMmap) -> Result<()> {
    // Page table addresses (following Firecracker's layout)
    let pml4_addr = GuestAddress(0x9000);
    let pdpte_addr = GuestAddress(0xa000);
    let pde_addr = GuestAddress(0xb000);

    // Zero out page table areas
    zero_guest_memory(guest_memory, pml4_addr, 0x1000)?;
    zero_guest_memory(guest_memory, pdpte_addr, 0x1000)?;
    zero_guest_memory(guest_memory, pde_addr, 0x1000)?;

    // PML4[0] -> PDPTE (present, writable)
    let pml4_entry: u64 = pdpte_addr.raw_value() | 0x3; // Present + Writable
    write_to_guest(guest_memory, pml4_addr, &pml4_entry.to_le_bytes())?;

    // PDPTE[0] -> PDE (present, writable)
    let pdpte_entry: u64 = pde_addr.raw_value() | 0x3;
    write_to_guest(guest_memory, pdpte_addr, &pdpte_entry.to_le_bytes())?;

    // Identity map first 1GB using 2MB pages
    // PDE entries: 512 * 2MB = 1GB
    for i in 0u64..512 {
        let pde_entry: u64 = (i * 0x200000) | 0x83; // Present + Writable + Page Size (2MB)
        let pde_entry_addr = GuestAddress(pde_addr.raw_value() + i * 8);
        write_to_guest(guest_memory, pde_entry_addr, &pde_entry.to_le_bytes())?;
    }

    debug!("Set up identity-mapped page tables");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmdline_max_size() {
        assert!(layout::CMDLINE_MAX_SIZE > 0);
        assert!(layout::CMDLINE_MAX_SIZE <= 4096);
    }

    #[test]
    fn test_bzimage_magic() {
        // "HdrS" in little endian
        assert_eq!(BZIMAGE_MAGIC, 0x53726448);
    }
}
