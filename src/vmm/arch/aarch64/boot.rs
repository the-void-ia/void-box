//! aarch64 kernel loading: Image binary + DTB generation.
//!
//! ARM64 Linux boot protocol (Documentation/arch/arm64/booting.rst):
//!   - The kernel is an `Image` placed `text_offset` bytes (a header field)
//!     past a 2 MB-aligned base anywhere in usable RAM. An arm64 Image has
//!     no self-decompressor — distro `/boot/vmlinuz` files are gzip
//!     streams the loader must inflate before writing to guest memory.
//!   - The DTB is 8-byte aligned and at most 2 MB.
//!   - x0 = DTB physical address, PC = kernel entry (the Image base).
//!   - The DTB describes the whole platform: memory, CPUs, GIC, timer,
//!     PSCI, devices. A device absent from the DTB does not exist for a
//!     device-tree kernel.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use flate2::read::GzDecoder;
use tracing::debug;
use vm_memory::GuestAddress;

use crate::vmm::arch::BootPlatform;
use crate::vmm::kvm::Vm;
use crate::vmm::memory::write_to_guest;
use crate::{Error, Result};

use super::kvm::{layout, probe_gic_version, GicVersion};

/// arm64 Image header magic ("ARM\x64", little-endian at byte offset 56).
const IMAGE_MAGIC: u32 = 0x644d_5241;
/// Byte offset of the u64 `text_offset` field in the Image header.
const IMAGE_TEXT_OFFSET_FIELD: usize = 8;
/// Byte offset of the u64 `image_size` field in the Image header.
const IMAGE_SIZE_FIELD: usize = 16;
/// Byte offset of the u32 magic field in the Image header.
const IMAGE_MAGIC_FIELD: usize = 56;
/// Length of the Image header prefix the loader inspects.
const IMAGE_HEADER_LEN: usize = 64;

/// `text_offset` mandated for old kernels whose header leaves `image_size`
/// zero (booting.rst: "where image_size is zero, text_offset can be assumed
/// to be 0x80000").
const LEGACY_TEXT_OFFSET: u64 = 0x8_0000;

/// gzip stream magic bytes.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// ELF magic bytes, sniffed only to produce a targeted error message.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// Kernel cmdline length cap, matching the x86 loader's `CMDLINE_MAX_SIZE`.
const BOOTARGS_MAX_LEN: usize = 4096;

/// Alignment for the kernel base (boot protocol) and initramfs placement.
const ALIGN_2MB: u64 = 0x20_0000;

/// Chunk size for streaming the (possibly inflating) kernel into guest RAM.
const LOAD_CHUNK_SIZE: usize = 1 << 20;

/// phandle of the GIC node, referenced by the root `interrupt-parent`.
const GIC_PHANDLE: u32 = 1;

/// First cell of a PPI interrupt specifier.
const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;
/// First cell of an SPI interrupt specifier.
const GIC_FDT_IRQ_TYPE_SPI: u32 = 0;
/// Interrupt-specifier trigger flags: level-triggered, active low.
const IRQ_TYPE_LEVEL_LOW: u32 = 8;
/// Interrupt-specifier trigger flags: level-triggered, active high.
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;

/// UART interrupt: GIC SPI 1 (INTID 33). Declared in the DTB but never
/// injected — kernel console writes poll the line-status register, matching
/// the x86 path, where no serial IRQ is injected either.
const UART_SPI: u32 = 1;

/// UART input clock advertised in the DTB. Required by the 8250 OF binding;
/// it only feeds baud-divisor math the register model ignores.
const UART_CLOCK_HZ: u32 = 1_843_200;

/// Interrupt-specifier trigger flags: edge-triggered, rising.
const IRQ_TYPE_EDGE_RISING: u32 = 1;

/// virtio-mmio register window advertised per device (the virtio-mmio
/// register file; matches the `512@` in the x86 cmdline convention and the
/// devices' `mmio_size`).
const VIRTIO_MMIO_WINDOW_SIZE: u64 = 0x200;

/// Load kernel (Image) and optionally initramfs into guest memory.
///
/// Generates the guest DTB (memory, chosen, cpus, psci, GIC, UART, virtio,
/// and timer nodes) and writes it at [`layout::DTB_ADDR`]. Returns the
/// kernel entry point.
pub fn load_kernel(
    vm: &Vm,
    kernel_path: &Path,
    initramfs_path: Option<&Path>,
    cmdline: &str,
    platform: &BootPlatform,
) -> Result<u64> {
    let memory_size = vm.memory_size();
    let ram_end = layout::RAM_START + memory_size;

    let kernel = load_kernel_image(vm, kernel_path)?;

    let initramfs_info = match initramfs_path {
        Some(path) => Some(place_initramfs(vm, path, kernel.end, ram_end)?),
        None => None,
    };

    let gic_version = probe_gic_version(vm.vm_fd());
    let dtb = generate_dtb(memory_size, cmdline, initramfs_info, platform, gic_version)?;
    if dtb.len() as u64 > layout::DTB_MAX_SIZE {
        return Err(Error::Boot(format!(
            "generated DTB ({} bytes) exceeds the {} byte slot below the kernel base",
            dtb.len(),
            layout::DTB_MAX_SIZE
        )));
    }
    write_to_guest(vm.guest_memory(), GuestAddress(layout::DTB_ADDR), &dtb)?;
    debug!(
        "Wrote DTB at {:#x} ({} bytes, {:?}, {} vCPUs, {} virtio slots)",
        layout::DTB_ADDR,
        dtb.len(),
        gic_version,
        platform.vcpu_count,
        platform.virtio_slots.len()
    );

    Ok(kernel.entry)
}

/// A kernel Image loaded into guest memory.
struct LoadedKernel {
    /// Guest-physical entry point (the Image load address).
    entry: u64,
    /// End of the kernel's runtime footprint: the load address plus the
    /// larger of the header's `image_size` (which includes BSS) and the
    /// bytes actually loaded.
    end: u64,
}

/// Fields of the arm64 Image header the loader consumes.
#[derive(Debug)]
struct ImageHeader {
    /// Offset of the Image from a 2 MB-aligned base.
    text_offset: u64,
    /// Kernel runtime footprint from the load address, including BSS.
    /// Zero on pre-3.17 kernels.
    image_size: u64,
}

/// Open, validate, and stream the kernel Image into guest memory.
fn load_kernel_image(vm: &Vm, kernel_path: &Path) -> Result<LoadedKernel> {
    let guest_memory = vm.guest_memory();
    let memory_size = vm.memory_size();
    let ram_end = layout::RAM_START + memory_size;

    let mut file = File::open(kernel_path)
        .map_err(|e| Error::Boot(format!("Failed to open kernel: {}", e)))?;

    let mut magic = [0u8; 2];
    let sniffed_len = file
        .read(&mut magic)
        .map_err(|e| Error::Boot(format!("Failed to read kernel: {}", e)))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| Error::Boot(format!("Failed to seek kernel: {}", e)))?;
    let is_gzip = sniffed_len == 2 && magic == GZIP_MAGIC;

    // Everything read (or inflated) is capped at the guest RAM size: a
    // kernel that cannot fit in guest memory is unbootable regardless, and
    // the cap keeps a malformed or hostile file from ballooning host memory
    // — the gzip ISIZE field is not trustworthy, so the bound is on the
    // actual output stream.
    let mut reader: Box<dyn Read> = if is_gzip {
        Box::new(GzDecoder::new(file).take(memory_size))
    } else {
        Box::new(file.take(memory_size))
    };

    let mut header = [0u8; IMAGE_HEADER_LEN];
    reader.read_exact(&mut header).map_err(|e| {
        Error::Boot(format!(
            "kernel too short to carry an arm64 Image header: {}",
            e
        ))
    })?;
    let image_header = parse_image_header(&header)?;

    // Header fields are untrusted input: place with checked arithmetic and
    // reject anything that leaves guest RAM. The bound covers the header
    // bytes written below — `write_to_guest` silently truncates a write
    // that starts in-range but runs past the end of guest memory.
    let load_addr = layout::KERNEL_BASE_ADDR
        .checked_add(image_header.text_offset)
        .filter(|addr| {
            addr.checked_add(IMAGE_HEADER_LEN as u64)
                .is_some_and(|header_end| header_end <= ram_end)
        })
        .ok_or_else(|| {
            Error::Boot(format!(
                "Image text_offset {:#x} places the kernel outside guest RAM",
                image_header.text_offset
            ))
        })?;

    // Stream into guest memory: the consumed header first, then chunks.
    write_to_guest(guest_memory, GuestAddress(load_addr), &header)?;
    let mut loaded_bytes = header.len() as u64;
    let mut chunk = vec![0u8; LOAD_CHUNK_SIZE];
    loop {
        let read_len = reader
            .read(&mut chunk)
            .map_err(|e| Error::Boot(format!("Failed to read kernel: {}", e)))?;
        if read_len == 0 {
            break;
        }
        // No overflow: loaded_bytes ≤ memory_size (Take cap) and
        // load_addr < ram_end, both well below 2^63.
        let write_addr = load_addr + loaded_bytes;
        if write_addr + read_len as u64 > ram_end {
            return Err(Error::Boot(format!(
                "kernel Image does not fit in guest RAM ({} MB); raise memory_mb",
                memory_size / (1024 * 1024)
            )));
        }
        write_to_guest(guest_memory, GuestAddress(write_addr), &chunk[..read_len])?;
        loaded_bytes += read_len as u64;
    }
    if loaded_bytes >= memory_size {
        return Err(Error::Boot(format!(
            "kernel Image is at least as large as guest RAM ({} MB); raise memory_mb",
            memory_size / (1024 * 1024)
        )));
    }

    // image_size covers BSS beyond the file bytes; a malformed header must
    // not shrink the footprint below what was actually loaded.
    let runtime_size = image_header.image_size.max(loaded_bytes);
    let kernel_end = load_addr
        .checked_add(runtime_size)
        .filter(|end| *end <= ram_end)
        .ok_or_else(|| {
            Error::Boot(format!(
                "kernel runtime footprint (image_size {:#x}) exceeds guest RAM; raise memory_mb",
                image_header.image_size
            ))
        })?;

    debug!(
        "Loaded aarch64 kernel Image at {:#x} ({} bytes loaded, image_size {:#x}, gzip={})",
        load_addr, loaded_bytes, image_header.image_size, is_gzip
    );

    Ok(LoadedKernel {
        entry: load_addr,
        end: kernel_end,
    })
}

/// Parse and validate the arm64 Image header prefix.
fn parse_image_header(header: &[u8; IMAGE_HEADER_LEN]) -> Result<ImageHeader> {
    if header[..ELF_MAGIC.len()] == ELF_MAGIC {
        return Err(Error::Boot(
            "kernel is an ELF vmlinux; aarch64/KVM boots a raw arm64 Image \
             (e.g. /boot/vmlinuz or arch/arm64/boot/Image, gzip-compressed OK)"
                .into(),
        ));
    }
    let magic = u32::from_le_bytes(
        header[IMAGE_MAGIC_FIELD..IMAGE_MAGIC_FIELD + 4]
            .try_into()
            .unwrap(),
    );
    if magic != IMAGE_MAGIC {
        return Err(Error::Boot(format!(
            "kernel is not an arm64 Image (magic {:#010x} at offset 56, expected {:#010x})",
            magic, IMAGE_MAGIC
        )));
    }
    let text_offset = u64::from_le_bytes(
        header[IMAGE_TEXT_OFFSET_FIELD..IMAGE_TEXT_OFFSET_FIELD + 8]
            .try_into()
            .unwrap(),
    );
    let image_size = u64::from_le_bytes(
        header[IMAGE_SIZE_FIELD..IMAGE_SIZE_FIELD + 8]
            .try_into()
            .unwrap(),
    );
    if image_size == 0 {
        return Ok(ImageHeader {
            text_offset: LEGACY_TEXT_OFFSET,
            image_size: 0,
        });
    }
    Ok(ImageHeader {
        text_offset,
        image_size,
    })
}

/// Load the initramfs into guest memory past the kernel's runtime footprint.
///
/// Returns `(base, length)` for the DTB's `linux,initrd-*` properties.
fn place_initramfs(
    vm: &Vm,
    initramfs_path: &Path,
    kernel_end: u64,
    ram_end: u64,
) -> Result<(u64, u64)> {
    let mut file = File::open(initramfs_path)
        .map_err(|e| Error::Boot(format!("Failed to open initramfs: {}", e)))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| Error::Boot(format!("Failed to read initramfs: {}", e)))?;

    let base = align_up(kernel_end, ALIGN_2MB)
        .ok_or_else(|| Error::Boot("initramfs base address overflows".into()))?;
    let fits = base
        .checked_add(data.len() as u64)
        .is_some_and(|end| end <= ram_end);
    if !fits {
        return Err(Error::Boot(format!(
            "kernel (ends {:#x}) + initramfs ({} MB) exceed guest RAM (ends {:#x}); raise memory_mb",
            kernel_end,
            data.len() / (1024 * 1024),
            ram_end
        )));
    }
    write_to_guest(vm.guest_memory(), GuestAddress(base), &data)?;
    debug!("Loaded initramfs at {:#x} ({} bytes)", base, data.len());
    Ok((base, data.len() as u64))
}

/// Round `value` up to the next multiple of `align` (a power of two);
/// `None` on overflow.
fn align_up(value: u64, align: u64) -> Option<u64> {
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

/// MPIDR affinity KVM assigns to a vCPU (arch/arm64/kvm/sys_regs.c,
/// `reset_mpidr`): Aff0 = id[3:0], Aff1 = id[11:4], Aff2 = id[19:12].
///
/// The `/cpus` `reg` values must carry the same affinities, or the guest's
/// PSCI `CPU_ON` calls name CPUs KVM does not have.
fn mpidr_affinity(vcpu_id: usize) -> u32 {
    let id = vcpu_id as u32;
    (id & 0xf) | (((id >> 4) & 0xff) << 8) | (((id >> 12) & 0xff) << 16)
}

/// Generate the guest DTB: memory, chosen, cpus, psci, GIC, UART, virtio,
/// and timer nodes.
///
/// Every `interrupts` property resolves against the GIC through the root
/// `interrupt-parent`; without the GIC node the kernel cannot wire even its
/// own timer PPIs and hangs silently before any console output.
fn generate_dtb(
    memory_size: u64,
    cmdline: &str,
    initramfs_info: Option<(u64, u64)>,
    platform: &BootPlatform,
    gic_version: GicVersion,
) -> Result<Vec<u8>> {
    use vm_fdt::FdtWriter;

    let vcpu_count = platform.vcpu_count;

    if cmdline.len() > BOOTARGS_MAX_LEN {
        return Err(Error::Boot(format!(
            "kernel cmdline is {} bytes; bootargs are capped at {} (matching the x86 limit)",
            cmdline.len(),
            BOOTARGS_MAX_LEN
        )));
    }

    let ram_start = layout::RAM_START;
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
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)
        .map_err(|e| Error::Boot(format!("root interrupt-parent: {}", e)))?;

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
    // stdout-path lets a bare `earlycon` bootarg find the UART before the
    // full 8250 driver probes.
    fdt.property_string("stdout-path", &format!("/serial@{:x}", layout::UART_ADDR))
        .map_err(|e| Error::Boot(format!("chosen stdout-path: {}", e)))?;

    if let Some((initrd_start, initrd_size)) = initramfs_info {
        fdt.property_u64("linux,initrd-start", initrd_start)
            .map_err(|e| Error::Boot(format!("initrd-start: {}", e)))?;
        fdt.property_u64("linux,initrd-end", initrd_start + initrd_size)
            .map_err(|e| Error::Boot(format!("initrd-end: {}", e)))?;
    }

    fdt.end_node(chosen)
        .map_err(|e| Error::Boot(format!("end chosen: {}", e)))?;

    // CPUs node — enable-method "psci" requires each vCPU to be initialized
    // with the KVM_ARM_VCPU_PSCI_0_2 feature (see cpu::configure_vcpu).
    let cpus = fdt
        .begin_node("cpus")
        .map_err(|e| Error::Boot(format!("begin cpus: {}", e)))?;
    fdt.property_u32("#address-cells", 1)
        .map_err(|e| Error::Boot(format!("cpus #address-cells: {}", e)))?;
    fdt.property_u32("#size-cells", 0)
        .map_err(|e| Error::Boot(format!("cpus #size-cells: {}", e)))?;
    for vcpu_id in 0..vcpu_count {
        let affinity = mpidr_affinity(vcpu_id);
        let cpu = fdt
            .begin_node(&format!("cpu@{:x}", affinity))
            .map_err(|e| Error::Boot(format!("begin cpu@{:x}: {}", affinity, e)))?;
        fdt.property_string("device_type", "cpu")
            .map_err(|e| Error::Boot(format!("cpu device_type: {}", e)))?;
        fdt.property_string("compatible", "arm,armv8")
            .map_err(|e| Error::Boot(format!("cpu compatible: {}", e)))?;
        fdt.property_string("enable-method", "psci")
            .map_err(|e| Error::Boot(format!("cpu enable-method: {}", e)))?;
        fdt.property_u32("reg", affinity)
            .map_err(|e| Error::Boot(format!("cpu reg: {}", e)))?;
        fdt.end_node(cpu)
            .map_err(|e| Error::Boot(format!("end cpu: {}", e)))?;
    }
    fdt.end_node(cpus)
        .map_err(|e| Error::Boot(format!("end cpus: {}", e)))?;

    // PSCI node (Power State Coordination Interface)
    let psci = fdt
        .begin_node("psci")
        .map_err(|e| Error::Boot(format!("begin psci: {}", e)))?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,psci-1.0".to_string(), "arm,psci-0.2".to_string()],
    )
    .map_err(|e| Error::Boot(format!("psci compatible: {}", e)))?;
    fdt.property_string("method", "hvc")
        .map_err(|e| Error::Boot(format!("psci method: {}", e)))?;
    fdt.end_node(psci)
        .map_err(|e| Error::Boot(format!("end psci: {}", e)))?;

    // GIC node — must match the vGIC setup_vm_post_vcpus creates; both
    // consult probe_gic_version.
    let gic = fdt
        .begin_node(&format!("intc@{:x}", layout::GIC_DIST_ADDR))
        .map_err(|e| Error::Boot(format!("begin intc: {}", e)))?;
    fdt.property_null("interrupt-controller")
        .map_err(|e| Error::Boot(format!("gic interrupt-controller: {}", e)))?;
    fdt.property_u32("#interrupt-cells", 3)
        .map_err(|e| Error::Boot(format!("gic #interrupt-cells: {}", e)))?;
    fdt.property_u32("phandle", GIC_PHANDLE)
        .map_err(|e| Error::Boot(format!("gic phandle: {}", e)))?;
    match gic_version {
        GicVersion::V3 => {
            fdt.property_string("compatible", "arm,gic-v3")
                .map_err(|e| Error::Boot(format!("gic compatible: {}", e)))?;
            let redist_size = layout::GIC_REDIST_SIZE_PER_CPU * vcpu_count as u64;
            fdt.property_array_u64(
                "reg",
                &[
                    layout::GIC_DIST_ADDR,
                    layout::GIC_DIST_SIZE,
                    layout::GIC_REDIST_ADDR,
                    redist_size,
                ],
            )
            .map_err(|e| Error::Boot(format!("gic reg: {}", e)))?;
        }
        GicVersion::V2 => {
            fdt.property_string("compatible", "arm,gic-400")
                .map_err(|e| Error::Boot(format!("gic compatible: {}", e)))?;
            fdt.property_array_u64(
                "reg",
                &[
                    layout::GIC_DIST_ADDR,
                    layout::GICV2_DIST_SIZE,
                    layout::GIC_CPU_ADDR,
                    layout::GIC_CPU_SIZE,
                ],
            )
            .map_err(|e| Error::Boot(format!("gic reg: {}", e)))?;
        }
    }
    fdt.end_node(gic)
        .map_err(|e| Error::Boot(format!("end intc: {}", e)))?;

    // virtio-mmio nodes — one per populated slot; a device the VMM did not
    // create gets no node, matching the conditional cmdline args on x86_64.
    // dma-coherent: KVM guest memory shares the host cache hierarchy, so
    // the guest can skip per-transfer cache maintenance.
    for slot in &platform.virtio_slots {
        let node = fdt
            .begin_node(&format!("virtio_mmio@{:x}", slot.mmio_base()))
            .map_err(|e| Error::Boot(format!("begin virtio_mmio: {}", e)))?;
        fdt.property_string("compatible", "virtio,mmio")
            .map_err(|e| Error::Boot(format!("virtio compatible: {}", e)))?;
        fdt.property_array_u64("reg", &[slot.mmio_base(), VIRTIO_MMIO_WINDOW_SIZE])
            .map_err(|e| Error::Boot(format!("virtio reg: {}", e)))?;
        fdt.property_array_u32(
            "interrupts",
            &[GIC_FDT_IRQ_TYPE_SPI, slot.spi(), IRQ_TYPE_EDGE_RISING],
        )
        .map_err(|e| Error::Boot(format!("virtio interrupts: {}", e)))?;
        fdt.property_null("dma-coherent")
            .map_err(|e| Error::Boot(format!("virtio dma-coherent: {}", e)))?;
        fdt.end_node(node)
            .map_err(|e| Error::Boot(format!("end virtio_mmio: {}", e)))?;
    }

    // UART node — the shared 16550 register model over MMIO. The 8250 OF
    // driver (CONFIG_SERIAL_OF_PLATFORM) enumerates it as ttyS0, so the
    // existing console=ttyS0 bootarg is arch-neutral.
    let uart = fdt
        .begin_node(&format!("serial@{:x}", layout::UART_ADDR))
        .map_err(|e| Error::Boot(format!("begin serial: {}", e)))?;
    fdt.property_string("compatible", "ns16550a")
        .map_err(|e| Error::Boot(format!("serial compatible: {}", e)))?;
    fdt.property_array_u64("reg", &[layout::UART_ADDR, layout::UART_SIZE])
        .map_err(|e| Error::Boot(format!("serial reg: {}", e)))?;
    fdt.property_u32("clock-frequency", UART_CLOCK_HZ)
        .map_err(|e| Error::Boot(format!("serial clock-frequency: {}", e)))?;
    fdt.property_array_u32(
        "interrupts",
        &[GIC_FDT_IRQ_TYPE_SPI, UART_SPI, IRQ_TYPE_LEVEL_HIGH],
    )
    .map_err(|e| Error::Boot(format!("serial interrupts: {}", e)))?;
    fdt.end_node(uart)
        .map_err(|e| Error::Boot(format!("end serial: {}", e)))?;

    // Timer node (ARM architected timer). The third specifier cell carries
    // trigger flags plus, on GICv2 only, a CPU mask in bits 15:8 — the
    // GICv3 binding requires those bits to be zero.
    let timer_ppi_flags = match gic_version {
        GicVersion::V3 => IRQ_TYPE_LEVEL_LOW,
        GicVersion::V2 => {
            let cpu_mask = (1u32 << vcpu_count.min(layout::GICV2_MAX_VCPUS)) - 1;
            (cpu_mask << 8) | IRQ_TYPE_LEVEL_LOW
        }
    };
    let timer = fdt
        .begin_node("timer")
        .map_err(|e| Error::Boot(format!("begin timer: {}", e)))?;
    fdt.property_string("compatible", "arm,armv8-timer")
        .map_err(|e| Error::Boot(format!("timer compatible: {}", e)))?;
    fdt.property_array_u32(
        "interrupts",
        &[
            GIC_FDT_IRQ_TYPE_PPI,
            13,
            timer_ppi_flags, // Secure physical timer
            GIC_FDT_IRQ_TYPE_PPI,
            14,
            timer_ppi_flags, // Non-secure physical timer
            GIC_FDT_IRQ_TYPE_PPI,
            11,
            timer_ppi_flags, // Virtual timer
            GIC_FDT_IRQ_TYPE_PPI,
            10,
            timer_ppi_flags, // Hypervisor physical timer
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmm::arch::VirtioSlot;

    fn header_bytes(text_offset: u64, image_size: u64, magic: u32) -> [u8; IMAGE_HEADER_LEN] {
        let mut header = [0u8; IMAGE_HEADER_LEN];
        header[IMAGE_TEXT_OFFSET_FIELD..IMAGE_TEXT_OFFSET_FIELD + 8]
            .copy_from_slice(&text_offset.to_le_bytes());
        header[IMAGE_SIZE_FIELD..IMAGE_SIZE_FIELD + 8].copy_from_slice(&image_size.to_le_bytes());
        header[IMAGE_MAGIC_FIELD..IMAGE_MAGIC_FIELD + 4].copy_from_slice(&magic.to_le_bytes());
        header
    }

    #[test]
    fn parse_header_modern_kernel() {
        let header = header_bytes(0, 0x2c0_0000, IMAGE_MAGIC);
        let parsed = parse_image_header(&header).unwrap();
        assert_eq!(parsed.text_offset, 0);
        assert_eq!(parsed.image_size, 0x2c0_0000);
    }

    #[test]
    fn parse_header_legacy_zero_image_size_assumes_0x80000() {
        let header = header_bytes(0xdead, 0, IMAGE_MAGIC);
        let parsed = parse_image_header(&header).unwrap();
        assert_eq!(parsed.text_offset, LEGACY_TEXT_OFFSET);
    }

    #[test]
    fn parse_header_rejects_bad_magic() {
        let header = header_bytes(0, 0x100_0000, 0x1234_5678);
        assert!(parse_image_header(&header).is_err());
    }

    #[test]
    fn parse_header_rejects_elf() {
        let mut header = header_bytes(0, 0x100_0000, IMAGE_MAGIC);
        header[..4].copy_from_slice(&ELF_MAGIC);
        let err = parse_image_header(&header).unwrap_err().to_string();
        assert!(err.contains("ELF"), "unexpected error: {}", err);
    }

    #[test]
    fn mpidr_affinity_matches_kvm_mapping() {
        assert_eq!(mpidr_affinity(0), 0);
        assert_eq!(mpidr_affinity(5), 5);
        assert_eq!(mpidr_affinity(0xf), 0xf);
        assert_eq!(mpidr_affinity(0x10), 0x100); // Aff1 = 1, Aff0 = 0
        assert_eq!(mpidr_affinity(0x1f), 0x10f);
        assert_eq!(mpidr_affinity(122), 0x70a); // MAX_VCPUS - 1
    }

    #[test]
    fn align_up_rounds_and_detects_overflow() {
        assert_eq!(align_up(0, ALIGN_2MB), Some(0));
        assert_eq!(align_up(1, ALIGN_2MB), Some(ALIGN_2MB));
        assert_eq!(align_up(ALIGN_2MB, ALIGN_2MB), Some(ALIGN_2MB));
        assert_eq!(align_up(ALIGN_2MB + 1, ALIGN_2MB), Some(2 * ALIGN_2MB));
        assert_eq!(align_up(u64::MAX - 5, ALIGN_2MB), None);
    }

    #[test]
    fn dtb_fits_slot_for_both_gic_versions_at_max_vcpus() {
        for gic_version in [GicVersion::V3, GicVersion::V2] {
            let vcpu_count = match gic_version {
                GicVersion::V3 => layout::MAX_VCPUS,
                GicVersion::V2 => layout::GICV2_MAX_VCPUS,
            };
            let platform = BootPlatform {
                vcpu_count,
                virtio_slots: vec![
                    VirtioSlot::Net,
                    VirtioSlot::Vsock,
                    VirtioSlot::P9,
                    VirtioSlot::Blk,
                ],
            };
            let dtb = generate_dtb(
                1024 * 1024 * 1024,
                "console=ttyS0 loglevel=7",
                Some((0x4400_0000, 0x100_0000)),
                &platform,
                gic_version,
            )
            .unwrap();
            assert!((dtb.len() as u64) <= layout::DTB_MAX_SIZE);
            // FDT magic
            assert_eq!(&dtb[..4], &[0xd0, 0x0d, 0xfe, 0xed]);
        }
    }

    #[test]
    fn dtb_rejects_oversized_bootargs() {
        let long_cmdline = "x".repeat(BOOTARGS_MAX_LEN + 1);
        let platform = BootPlatform {
            vcpu_count: 1,
            virtio_slots: Vec::new(),
        };
        let result = generate_dtb(1 << 30, &long_cmdline, None, &platform, GicVersion::V3);
        assert!(result.is_err());
    }
}
