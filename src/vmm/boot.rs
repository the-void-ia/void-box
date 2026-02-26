//! Kernel loading and boot parameter setup

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use linux_loader::bootparam::boot_params;
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::elf::Elf as ElfLoader;
use linux_loader::loader::KernelLoader;
use tracing::{debug, info};
use vm_memory::{Address, ByteValued, GuestAddress, GuestMemoryMmap};

use crate::vmm::kvm::{layout, Vm};
use crate::vmm::memory::{read_from_guest, write_to_guest, zero_guest_memory};
use crate::{Error, Result};

/// Magic number for bzImage
const BZIMAGE_MAGIC: u32 = 0x53726448; // "HdrS"
const EARLY_IDENTITY_MAP_LIMIT: u64 = 0x4000_0000; // first 1 GiB

/// Load kernel and optionally initramfs into guest memory
pub fn load_kernel(
    vm: &Vm,
    kernel_path: &Path,
    initramfs_path: Option<&Path>,
    cmdline: &str,
) -> Result<u64> {
    let guest_memory = vm.guest_memory();
    let memory_size = vm.memory_size();

    // Open kernel file once — used by both the loader and the header reader
    let mut kernel_file = File::open(kernel_path)
        .map_err(|e| Error::Boot(format!("Failed to open kernel: {}", e)))?;

    // Read setup-header limits before loading.
    let _init_size = read_bzimage_init_size(&mut kernel_file).unwrap_or(0);
    let initrd_addr_max = read_bzimage_initrd_addr_max(&mut kernel_file).unwrap_or(u32::MAX);

    // Detect kernel format and load it
    let (kernel_load_addr, kernel_entry) = load_kernel_image(guest_memory, &mut kernel_file)?;
    info!(
        "Loaded kernel at {:#x}, entry point {:#x}",
        kernel_load_addr, kernel_entry
    );

    // Place the initramfs under constraints:
    // 1) Linux setup-header initrd_addr_max
    // 2) our early boot identity mapping window (first 1 GiB)
    // 3) guest RAM end
    // Align down to 2 MiB boundary.
    let initramfs_file_size = initramfs_path
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .unwrap_or(0);
    let safe_initramfs_addr = if initramfs_file_size > 0 {
        let max_end = (memory_size.saturating_sub(1))
            .min(initrd_addr_max as u64)
            .min(EARLY_IDENTITY_MAP_LIMIT.saturating_sub(1));
        if initramfs_file_size > max_end.saturating_add(1) {
            return Err(Error::Boot(format!(
                "initramfs too large ({} bytes) for placement window end={:#x}",
                initramfs_file_size, max_end
            )));
        }
        let addr = (max_end + 1 - initramfs_file_size) & !0x1F_FFFF;
        debug!(
            "Placing initramfs at {:#x} (ram_end={:#x}, initrd_addr_max={:#x}, early_map_limit={:#x}, size={:#x})",
            addr,
            memory_size.saturating_sub(1),
            initrd_addr_max,
            EARLY_IDENTITY_MAP_LIMIT - 1,
            initramfs_file_size
        );
        addr
    } else {
        layout::INITRAMFS_LOAD_ADDR.raw_value()
    };

    // Load initramfs if provided
    let initramfs_info = if let Some(initramfs) = initramfs_path {
        Some(load_initramfs(
            guest_memory,
            initramfs,
            GuestAddress(safe_initramfs_addr),
        )?)
    } else {
        None
    };

    // Set up boot parameters (reads setup header from kernel file)
    setup_boot_params(
        guest_memory,
        &mut kernel_file,
        cmdline,
        initramfs_info,
        memory_size,
    )?;

    // Set up initial page tables for 64-bit mode
    setup_page_tables(guest_memory)?;

    Ok(kernel_entry)
}

/// Read the init_size field from a bzImage setup header.
/// Returns 0 if the file is not a bzImage or the field can't be read.
fn read_bzimage_init_size(kernel_file: &mut File) -> Result<u32> {
    // init_size is at offset 0x260 in the bzImage file (setup_header offset 0x260 - 0x1f1 = 0x6f
    // within the header, but it's easier to just seek to the absolute file offset).
    // Actually, init_size is at byte offset 0x260 in the boot sector/setup header area.
    // In the setup_header struct, init_size is at a known position.
    // File offset of init_size = 0x260.
    kernel_file
        .seek(SeekFrom::Start(0x260))
        .map_err(|e| Error::Boot(format!("Failed to seek to init_size: {}", e)))?;
    let mut buf = [0u8; 4];
    kernel_file
        .read_exact(&mut buf)
        .map_err(|e| Error::Boot(format!("Failed to read init_size: {}", e)))?;
    kernel_file
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Boot(format!("Failed to rewind kernel file: {}", e)))?;
    Ok(u32::from_le_bytes(buf))
}

/// Read the initrd_addr_max field from a bzImage setup header.
/// Returns error if unreadable.
fn read_bzimage_initrd_addr_max(kernel_file: &mut File) -> Result<u32> {
    // setup_header.initrd_addr_max lives at file offset 0x22c.
    kernel_file
        .seek(SeekFrom::Start(0x22c))
        .map_err(|e| Error::Boot(format!("Failed to seek to initrd_addr_max: {}", e)))?;
    let mut buf = [0u8; 4];
    kernel_file
        .read_exact(&mut buf)
        .map_err(|e| Error::Boot(format!("Failed to read initrd_addr_max: {}", e)))?;
    kernel_file
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Boot(format!("Failed to rewind kernel file: {}", e)))?;
    Ok(u32::from_le_bytes(buf))
}

/// Load kernel image (supports bzImage and ELF formats)
fn load_kernel_image(guest_memory: &GuestMemoryMmap, kernel_file: &mut File) -> Result<(u64, u64)> {
    // Check if it's a bzImage
    if is_bzimage(kernel_file)? {
        debug!("Loading bzImage kernel");
        load_bzimage(guest_memory, kernel_file)
    } else {
        debug!("Loading ELF kernel");
        load_elf_kernel(guest_memory, kernel_file)
    }
}

/// Check if kernel is bzImage format
fn is_bzimage(kernel_file: &mut File) -> Result<bool> {
    kernel_file
        .seek(SeekFrom::Start(0x202))
        .map_err(|e| Error::Boot(format!("Failed to seek kernel: {}", e)))?;

    let mut magic = [0u8; 4];
    kernel_file
        .read_exact(&mut magic)
        .map_err(|e| Error::Boot(format!("Failed to read kernel magic: {}", e)))?;

    kernel_file
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Boot(format!("Failed to seek kernel: {}", e)))?;

    Ok(u32::from_le_bytes(magic) == BZIMAGE_MAGIC)
}

/// Load bzImage format kernel
fn load_bzimage(guest_memory: &GuestMemoryMmap, kernel_file: &mut File) -> Result<(u64, u64)> {
    let kernel_load = BzImage::load(
        guest_memory,
        None, // Use default kernel offset
        kernel_file,
        None, // highmem_start_address
    )
    .map_err(|e| Error::Boot(format!("Failed to load bzImage: {:?}", e)))?;

    let load_addr = kernel_load.kernel_load.raw_value();

    // For bzImage booting in 64-bit long mode, startup_64 is at offset +0x200
    // from the start of the protected-mode code (startup_32 is at load_addr).
    let entry_64 = load_addr + 0x200;
    Ok((load_addr, entry_64))
}

/// Load ELF format kernel
fn load_elf_kernel(guest_memory: &GuestMemoryMmap, kernel_file: &mut File) -> Result<(u64, u64)> {
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

/// Load initramfs into guest memory at the given address.
fn load_initramfs(
    guest_memory: &GuestMemoryMmap,
    initramfs_path: &Path,
    initramfs_addr: GuestAddress,
) -> Result<(u64, u64)> {
    let mut initramfs_file = File::open(initramfs_path)
        .map_err(|e| Error::Boot(format!("Failed to open initramfs: {}", e)))?;

    let mut initramfs_data = Vec::new();
    initramfs_file
        .read_to_end(&mut initramfs_data)
        .map_err(|e| Error::Boot(format!("Failed to read initramfs: {}", e)))?;

    let initramfs_size = initramfs_data.len() as u64;

    write_to_guest(guest_memory, initramfs_addr, &initramfs_data)?;

    // Verify first bytes were written correctly
    let verify = read_from_guest(guest_memory, initramfs_addr, 16)?;
    info!(
        "Loaded initramfs at {:#x}, size {} bytes, first bytes: {:02x?}",
        initramfs_addr.raw_value(),
        initramfs_size,
        &verify[..std::cmp::min(16, verify.len())]
    );

    Ok((initramfs_addr.raw_value(), initramfs_size))
}

/// Set up boot parameters (zero page).
///
/// Reads the setup header from the actual kernel file so the kernel gets its
/// own header fields (version, loadflags, init_size, etc.) and then overrides
/// the loader-specific fields (cmdline, initramfs, e820 map).
fn setup_boot_params(
    guest_memory: &GuestMemoryMmap,
    kernel_file: &mut File,
    cmdline: &str,
    initramfs_info: Option<(u64, u64)>,
    memory_size: u64,
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

    // Start with a zeroed boot_params, then copy the real setup_header from
    // the kernel file so we get the kernel's own version, loadflags, init_size, etc.
    let mut params = boot_params::default();

    // The setup header in a bzImage lives at file offset 0x1f1.
    // Its size in linux_loader::bootparam is the size of setup_header.
    let hdr_file_offset: u64 = 0x1f1;
    let hdr_size = std::mem::size_of_val(&params.hdr);
    kernel_file
        .seek(SeekFrom::Start(hdr_file_offset))
        .map_err(|e| Error::Boot(format!("Failed to seek to setup header: {}", e)))?;

    // SAFETY: setup_header is a plain-old-data #[repr(C)] struct from linux-loader.
    let hdr_slice =
        unsafe { std::slice::from_raw_parts_mut(&mut params.hdr as *mut _ as *mut u8, hdr_size) };
    kernel_file
        .read_exact(hdr_slice)
        .map_err(|e| Error::Boot(format!("Failed to read setup header: {}", e)))?;

    // Copy fields out of packed struct before formatting (avoid unaligned ref UB)
    let hdr_version = { params.hdr.version };
    let hdr_loadflags = { params.hdr.loadflags };
    let hdr_init_size = { params.hdr.init_size };
    debug!(
        "Read setup header: version={:#x}, loadflags={:#x}, init_size={:#x}",
        hdr_version, hdr_loadflags, hdr_init_size
    );

    // Override loader-specific fields
    params.hdr.type_of_loader = 0xFF;
    params.hdr.cmd_line_ptr = layout::CMDLINE_ADDR.raw_value() as u32;
    params.hdr.cmdline_size = cmdline_bytes.len() as u32;

    // Set up initramfs if provided
    if let Some((addr, size)) = initramfs_info {
        params.hdr.ramdisk_image = addr as u32;
        params.hdr.ramdisk_size = size as u32;
    }

    // --- E820 memory map (set inside struct before writing) ---
    // NOTE: The kernel requires at least 2 e820 entries (append_e820_table()
    // returns -1 if nr_entries < 2). We split RAM into low memory (below 1MB)
    // and high memory (above 1MB), matching a real BIOS layout.
    use linux_loader::bootparam::boot_e820_entry;
    let mut e820_idx: u8 = 0;

    // Entry 1: Usable low memory (0 to 640KB - EBDA)
    params.e820_table[e820_idx as usize] = boot_e820_entry {
        addr: 0,
        size: 0x9FC00, // 639KB (standard PC low memory)
        type_: 1,      // E820_RAM
    };
    e820_idx += 1;

    // Entry 2: Reserved BIOS area (640KB-1MB: VGA, ROM, etc.)
    params.e820_table[e820_idx as usize] = boot_e820_entry {
        addr: 0x9FC00,
        size: 0x100000 - 0x9FC00, // ~384KB reserved
        type_: 2,                 // E820_RESERVED
    };
    e820_idx += 1;

    // Entry 3: Usable high memory (1MB to end of RAM, below MMIO gap)
    let high_mem_end = std::cmp::min(memory_size, layout::MMIO_GAP_START);
    if high_mem_end > 0x100000 {
        params.e820_table[e820_idx as usize] = boot_e820_entry {
            addr: 0x100000,
            size: high_mem_end - 0x100000,
            type_: 1, // E820_RAM
        };
        e820_idx += 1;
    }

    // Entry 4: If memory extends above the MMIO gap, add high RAM
    if memory_size > layout::MMIO_GAP_START {
        let high_size = memory_size - layout::MMIO_GAP_START;
        params.e820_table[e820_idx as usize] = boot_e820_entry {
            addr: layout::MMIO_GAP_END,
            size: high_size,
            type_: 1, // E820_RAM
        };
        e820_idx += 1;
    }

    params.e820_entries = e820_idx;

    // Write the complete boot_params struct (including e820) to guest memory
    let params_bytes = params.as_slice();
    let bp = layout::BOOT_PARAMS_ADDR.raw_value();
    debug!(
        "boot_params struct size: {} bytes, e820_entries offset in struct: {}",
        params_bytes.len(),
        std::mem::offset_of!(boot_params, e820_entries)
    );
    write_to_guest(guest_memory, layout::BOOT_PARAMS_ADDR, params_bytes)?;

    // Verify e820 was written correctly by reading back
    let verify_entries = read_from_guest(guest_memory, GuestAddress(bp + 0x1e8), 1)?;
    let verify_table = read_from_guest(guest_memory, GuestAddress(bp + 0x2d0), 20)?;
    debug!(
        "Verify: e820_entries at {:#x} = {}, first entry addr={:#x} size={:#x} type={}",
        bp + 0x1e8,
        verify_entries[0],
        u64::from_le_bytes(verify_table[0..8].try_into().unwrap()),
        u64::from_le_bytes(verify_table[8..16].try_into().unwrap()),
        u32::from_le_bytes(verify_table[16..20].try_into().unwrap()),
    );

    debug!(
        "Wrote boot params at {:#x} ({} e820 entries, {} bytes RAM)",
        bp, e820_idx, memory_size
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
        const { assert!(layout::CMDLINE_MAX_SIZE > 0) };
        const { assert!(layout::CMDLINE_MAX_SIZE <= 4096) };
    }

    #[test]
    fn test_bzimage_magic() {
        // "HdrS" in little endian
        assert_eq!(BZIMAGE_MAGIC, 0x53726448);
    }
}
