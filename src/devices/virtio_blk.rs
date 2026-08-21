//! Minimal virtio-blk MMIO device (read-only raw file backend).
//!
//! This device is used to present OCI rootfs disk artifacts as a block device
//! to the guest on Linux/KVM.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use tracing::{debug, trace, warn};
use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use crate::devices::virtio_net::mmio;
use crate::devices::virtqueue::ring_addr;

pub const VIRTIO_BLK_DEVICE_TYPE: u32 = 2;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;

const QUEUE_MAX_SIZE: u16 = 128;

/// Bytes moved between the disk and guest memory in one step.
///
/// A data descriptor's length is a guest-written `u32`, so sizing a host buffer
/// from it hands the guest the allocation. Streaming through a fixed buffer makes
/// the host cost of a request constant in what the guest asked for, and the
/// descriptor's own memory is where the bytes land either way.
const IO_CHUNK_BYTES: usize = 64 * 1024;

/// Descriptors one request may chain: a header, data, and a status byte.
const MAX_CHAIN_DESCS: usize = 32;
const SECTOR_SIZE: u64 = 512;

/// Bytes in one descriptor-table entry: address, length, flags, and next.
const DESC_BYTES: usize = 16;

/// Bytes in a virtio-blk request header: type, reserved, and sector.
const BLK_HEADER_BYTES: usize = 16;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

#[derive(Debug, Default)]
struct QueueState {
    num_max: u16,
    num: u16,
    ready: bool,
    desc_addr: u64,
    driver_addr: u64,
    device_addr: u64,
}

pub struct VirtioBlkDevice {
    mmio_base: u64,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_sel: u32,
    queue: QueueState,
    interrupt_status: u32,
    status: u32,
    avail_idx: u16,
    used_idx: u16,
    disk: File,
    capacity_sectors: u64,
}

impl VirtioBlkDevice {
    pub fn new(path: &Path) -> crate::Result<Self> {
        let disk = File::open(path).map_err(|e| {
            crate::Error::Device(format!("virtio-blk open {}: {}", path.display(), e))
        })?;
        let size = disk
            .metadata()
            .map_err(|e| {
                crate::Error::Device(format!("virtio-blk stat {}: {}", path.display(), e))
            })?
            .len();
        let capacity_sectors = size / SECTOR_SIZE;

        debug!(
            "Creating virtio-blk device: path={}, size={} bytes, sectors={}",
            path.display(),
            size,
            capacity_sectors
        );

        Ok(Self {
            mmio_base: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queue: QueueState {
                num_max: QUEUE_MAX_SIZE,
                ..Default::default()
            },
            interrupt_status: 0,
            status: 0,
            avail_idx: 0,
            used_idx: 0,
            disk,
            capacity_sectors,
        })
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_RO
    }

    pub fn set_mmio_base(&mut self, base: u64) {
        self.mmio_base = base;
        debug!("virtio-blk MMIO base set to {:#x}", base);
    }

    pub fn mmio_base(&self) -> u64 {
        self.mmio_base
    }

    pub fn mmio_size(&self) -> u64 {
        0x200
    }

    pub fn handles_mmio(&self, addr: u64) -> bool {
        addr >= self.mmio_base && addr < self.mmio_base + self.mmio_size()
    }

    pub fn has_pending_interrupt(&self) -> bool {
        self.interrupt_status != 0
    }

    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        if (mmio::CONFIG..mmio::CONFIG + 8).contains(&offset) {
            let cap = self.capacity_sectors.to_le_bytes();
            let start = (offset - mmio::CONFIG) as usize;
            for (i, out) in data.iter_mut().enumerate() {
                *out = *cap.get(start + i).unwrap_or(&0);
            }
            return;
        }

        let value: u32 = match offset {
            mmio::MAGIC_VALUE => mmio::MAGIC,
            mmio::VERSION => mmio::VERSION_2,
            mmio::DEVICE_ID => VIRTIO_BLK_DEVICE_TYPE,
            mmio::VENDOR_ID => 0x554d4551,
            mmio::DEVICE_FEATURES => {
                let f = self.device_features();
                if self.device_features_sel == 0 {
                    f as u32
                } else {
                    (f >> 32) as u32
                }
            }
            mmio::QUEUE_NUM_MAX => self.queue.num_max as u32,
            mmio::QUEUE_READY => self.queue.ready as u32,
            mmio::INTERRUPT_STATUS => self.interrupt_status,
            mmio::STATUS => self.status,
            mmio::CONFIG_GENERATION => 0,
            _ => {
                trace!("virtio-blk: unhandled MMIO read at offset {:#x}", offset);
                0
            }
        };

        let bytes = value.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn mmio_write(&mut self, offset: u64, data: &[u8], guest_mem: Option<&GuestMemoryMmap>) {
        if data.is_empty() {
            return;
        }
        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let value = u32::from_le_bytes(bytes);

        match offset {
            mmio::DEVICE_FEATURES_SEL => self.device_features_sel = value,
            mmio::DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | value as u64;
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                }
            }
            mmio::DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            mmio::QUEUE_SEL => self.queue_sel = value,
            mmio::QUEUE_NUM => {
                // The guest may not exceed the size the device advertises in
                // `QueueNumMax`; the virtio spec forbids it, and the descriptor
                // walk derives its index bound from this value, so an unclamped
                // write would let the guest choose its own bound.
                self.queue.num = (value as u16).min(self.queue.num_max);
            }
            mmio::QUEUE_READY => self.queue.ready = value != 0,
            mmio::QUEUE_NOTIFY => {
                if let Some(mem) = guest_mem {
                    if let Err(e) = self.process_queue(mem) {
                        warn!("virtio-blk: queue processing error: {}", e);
                    }
                }
            }
            mmio::INTERRUPT_ACK => self.interrupt_status &= !value,
            mmio::STATUS => {
                self.status = value;
                if value == 0 {
                    self.reset();
                }
            }
            mmio::QUEUE_DESC_LOW => {
                self.queue.desc_addr =
                    (self.queue.desc_addr & 0xFFFF_FFFF_0000_0000) | (value as u64)
            }
            mmio::QUEUE_DESC_HIGH => {
                self.queue.desc_addr =
                    (self.queue.desc_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32)
            }
            mmio::QUEUE_DRIVER_LOW => {
                self.queue.driver_addr =
                    (self.queue.driver_addr & 0xFFFF_FFFF_0000_0000) | (value as u64)
            }
            mmio::QUEUE_DRIVER_HIGH => {
                self.queue.driver_addr =
                    (self.queue.driver_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32)
            }
            mmio::QUEUE_DEVICE_LOW => {
                self.queue.device_addr =
                    (self.queue.device_addr & 0xFFFF_FFFF_0000_0000) | (value as u64)
            }
            mmio::QUEUE_DEVICE_HIGH => {
                self.queue.device_addr =
                    (self.queue.device_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32)
            }
            _ => {
                trace!(
                    "virtio-blk: unhandled MMIO write at offset {:#x}, value={:#x}",
                    offset,
                    value
                );
            }
        }
    }

    fn reset(&mut self) {
        self.interrupt_status = 0;
        self.status = 0;
        self.driver_features = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        self.queue = QueueState {
            num_max: QUEUE_MAX_SIZE,
            ..Default::default()
        };
        self.avail_idx = 0;
        self.used_idx = 0;
    }

    fn process_queue(&mut self, mem: &GuestMemoryMmap) -> crate::Result<()> {
        let q = &self.queue;
        if !q.ready || q.num == 0 {
            return Ok(());
        }

        // Every address below is a base the guest wrote to an MMIO register plus
        // an offset. `vm_memory`'s `unchecked_add` is a plain `+`: it panics under
        // `overflow-checks` and wraps otherwise, so a base near `u64::MAX` either
        // ends the VMM process or folds the access onto unrelated mapped memory.
        // `ring_addr` rejects the address instead.
        let (desc_base, avail_base, used_base) = (q.desc_addr, q.driver_addr, q.device_addr);
        let queue_size = q.num as usize;

        let Some(avail_idx_addr) = ring_addr(avail_base, 2) else {
            warn!("virtio-blk: available ring at {avail_base:#x} overflows the address space");
            return Ok(());
        };
        let mut idx_buf = [0u8; 2];
        mem.read(&mut idx_buf, avail_idx_addr)
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let avail_idx = u16::from_le_bytes(idx_buf);

        while self.avail_idx != avail_idx {
            let ring_offset = 4 + ((self.avail_idx as usize) % queue_size) * 2;
            let Some(ring_entry_addr) = ring_addr(avail_base, ring_offset as u64) else {
                warn!("virtio-blk: available ring entry overflows the address space");
                return Ok(());
            };
            let mut head_buf = [0u8; 2];
            mem.read(&mut head_buf, ring_entry_addr)
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let head = u16::from_le_bytes(head_buf) as usize;

            let (status, written) = self.handle_request(mem, desc_base, queue_size, head)?;

            let used_ring_off = 4 + ((self.used_idx as usize) % queue_size) * 8;
            let (Some(used_elem_addr), Some(used_idx_addr)) = (
                ring_addr(used_base, used_ring_off as u64),
                ring_addr(used_base, 2),
            ) else {
                warn!("virtio-blk: used ring at {used_base:#x} overflows the address space");
                return Ok(());
            };
            let used_elem = [(head as u32).to_le_bytes(), (written as u32).to_le_bytes()].concat();
            mem.write(&used_elem, used_elem_addr)
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            self.used_idx = self.used_idx.wrapping_add(1);
            self.avail_idx = self.avail_idx.wrapping_add(1);

            let used_idx_bytes = self.used_idx.to_le_bytes();
            mem.write(&used_idx_bytes, used_idx_addr)
                .map_err(|e| crate::Error::Memory(e.to_string()))?;

            if status != VIRTIO_BLK_S_OK {
                trace!("virtio-blk request completed with status={}", status);
            }
        }

        self.interrupt_status |= 1;
        Ok(())
    }

    fn handle_request(
        &mut self,
        mem: &GuestMemoryMmap,
        desc_base: u64,
        queue_size: usize,
        head: usize,
    ) -> crate::Result<(u8, usize)> {
        #[derive(Clone, Copy)]
        struct Desc {
            addr: u64,
            len: u32,
            flags: u16,
            next: u16,
        }

        let mut descs = Vec::new();
        let mut idx = head;
        loop {
            if idx >= queue_size {
                return Ok((VIRTIO_BLK_S_IOERR, 0));
            }
            // Bounded before the read rather than after the push, so an
            // over-long chain costs no descriptor reads past the limit.
            if descs.len() >= MAX_CHAIN_DESCS {
                warn!("virtio-blk: chain longer than {MAX_CHAIN_DESCS} descriptors");
                return Ok((VIRTIO_BLK_S_IOERR, 0));
            }
            let Some(off) = ring_addr(desc_base, (idx * 16) as u64) else {
                warn!("virtio-blk: descriptor table at {desc_base:#x} overflows the address space");
                return Ok((VIRTIO_BLK_S_IOERR, 0));
            };
            let mut raw = [0u8; DESC_BYTES];
            mem.read_slice(&mut raw, off)
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let d = Desc {
                addr: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
                len: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
                flags: u16::from_le_bytes(raw[12..14].try_into().unwrap()),
                next: u16::from_le_bytes(raw[14..16].try_into().unwrap()),
            };
            descs.push(d);
            if (d.flags & VIRTQ_DESC_F_NEXT) == 0 {
                break;
            }
            idx = d.next as usize;
        }

        if descs.len() < 2 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }

        // Header must be readable by device
        if (descs[0].flags & VIRTQ_DESC_F_WRITE) != 0 || descs[0].len < 16 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }

        let mut hdr = [0u8; BLK_HEADER_BYTES];
        mem.read_slice(&mut hdr, GuestAddress(descs[0].addr))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let req_type = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let offset = sector.saturating_mul(SECTOR_SIZE);

        let status_desc = *descs.last().unwrap();
        if (status_desc.flags & VIRTQ_DESC_F_WRITE) == 0 || status_desc.len < 1 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }

        let data_descs = &descs[1..descs.len() - 1];
        let mut total_written = 0usize;

        let status = match req_type {
            VIRTIO_BLK_T_IN => {
                trace!(
                    "virtio-blk: READ request sector={} descs={}",
                    sector,
                    data_descs.len()
                );
                let mut chunk = vec![0u8; IO_CHUNK_BYTES];
                let mut file_off = offset;
                for d in data_descs {
                    if (d.flags & VIRTQ_DESC_F_WRITE) == 0 {
                        return Ok((VIRTIO_BLK_S_IOERR, total_written));
                    }
                    let want = d.len as usize;
                    // The descriptor has to name memory the guest actually has
                    // before the device moves a byte for it. Without this a guest
                    // can name 4 GiB it does not own and the device works through
                    // all of it before the write fails.
                    if !mem.check_range(GuestAddress(d.addr), want) {
                        warn!("virtio-blk: data descriptor names {want} bytes the guest has not mapped");
                        return Ok((VIRTIO_BLK_S_IOERR, total_written));
                    }

                    let mut done = 0usize;
                    while done < want {
                        let step = IO_CHUNK_BYTES.min(want - done);
                        let mut filled = 0usize;
                        while filled < step {
                            match self.disk.read_at(
                                &mut chunk[filled..step],
                                file_off.saturating_add(filled as u64),
                            ) {
                                Ok(0) => break, // EOF: the rest of the step reads as zeros
                                Ok(read_now) => filled += read_now,
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(_) => return Ok((VIRTIO_BLK_S_IOERR, total_written)),
                            }
                        }
                        chunk[filled..step].fill(0);

                        let Some(dest) = ring_addr(d.addr, done as u64) else {
                            return Ok((VIRTIO_BLK_S_IOERR, total_written));
                        };
                        mem.write_slice(&chunk[..step], dest)
                            .map_err(|e| crate::Error::Memory(e.to_string()))?;

                        done += step;
                        file_off = file_off.saturating_add(step as u64);
                    }
                    total_written += want;
                }
                VIRTIO_BLK_S_OK
            }
            VIRTIO_BLK_T_OUT => {
                // Read-only backend
                warn!(
                    "virtio-blk: rejecting write request sector={} (ro backend)",
                    sector
                );
                VIRTIO_BLK_S_UNSUPP
            }
            _ => VIRTIO_BLK_S_UNSUPP,
        };

        mem.write_slice(&[status], GuestAddress(status_desc.addr))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        total_written += 1;

        Ok((status, total_written))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TEST_MEM_BYTES: usize = 256 * 1024;
    const DESC_TABLE: u64 = 0x1000;
    const AVAIL_RING: u64 = 0x4000;
    const USED_RING: u64 = 0x5000;
    const REQ_HEADER: u64 = 0x6000;
    const STATUS_BYTE: u64 = 0x6100;
    const DATA_BUFFER: u64 = 0x8000;
    const DISK_BYTES: usize = 4096;

    fn test_device() -> (VirtioBlkDevice, tempfile::TempDir) {
        device_with_disk(&[0u8; DISK_BYTES])
    }

    /// A device backed by exactly `contents`, so a test can assert on the bytes
    /// that reach the guest.
    fn device_with_disk(contents: &[u8]) -> (VirtioBlkDevice, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir for the backing disk");
        let path = dir.path().join("disk.img");
        let mut file = File::create(&path).expect("create the backing disk");
        file.write_all(contents).expect("size the backing disk");
        let device = VirtioBlkDevice::new(&path).expect("open the backing disk");
        (device, dir)
    }

    fn test_memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), TEST_MEM_BYTES)]).unwrap()
    }

    fn write_desc(mem: &GuestMemoryMmap, index: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let at = DESC_TABLE + index * DESC_BYTES as u64;
        mem.write_obj(addr, GuestAddress(at)).unwrap();
        mem.write_obj(len, GuestAddress(at + 8)).unwrap();
        mem.write_obj(flags, GuestAddress(at + 12)).unwrap();
        mem.write_obj(next, GuestAddress(at + 14)).unwrap();
    }

    /// Program the queue and publish one chain head, with the request header at
    /// `REQ_HEADER` naming a read of `sector`.
    fn post_read_request(device: &mut VirtioBlkDevice, mem: &GuestMemoryMmap, sector: u64) {
        let mut hdr = [0u8; BLK_HEADER_BYTES];
        hdr[0..4].copy_from_slice(&VIRTIO_BLK_T_IN.to_le_bytes());
        hdr[8..16].copy_from_slice(&sector.to_le_bytes());
        mem.write_slice(&hdr, GuestAddress(REQ_HEADER)).unwrap();

        mem.write_obj(1u16, GuestAddress(AVAIL_RING + 2)).unwrap();
        mem.write_obj(0u16, GuestAddress(AVAIL_RING + 4)).unwrap();

        device.queue.num = 16;
        device.queue.ready = true;
        device.queue.desc_addr = DESC_TABLE;
        device.queue.driver_addr = AVAIL_RING;
        device.queue.device_addr = USED_RING;
    }

    /// The status byte the device wrote for the completed request.
    fn status_of(mem: &GuestMemoryMmap) -> u8 {
        mem.read_obj(GuestAddress(STATUS_BYTE)).unwrap()
    }

    /// A data descriptor's length is a guest-written `u32`, and it used to size a
    /// host buffer that was then zero-filled — touching every page, so unlike a
    /// lazily mapped allocation this was real resident memory, up to 30 times per
    /// request. The descriptor has to name memory the guest actually has before
    /// the device moves anything for it.
    #[test]
    fn a_data_descriptor_naming_unmapped_memory_is_refused() {
        let (mut device, _dir) = test_device();
        let mem = test_memory();

        write_desc(
            &mem,
            0,
            REQ_HEADER,
            BLK_HEADER_BYTES as u32,
            VIRTQ_DESC_F_NEXT,
            1,
        );
        write_desc(
            &mem,
            1,
            DATA_BUFFER,
            u32::MAX,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            2,
        );
        write_desc(&mem, 2, STATUS_BYTE, 1, VIRTQ_DESC_F_WRITE, 0);
        post_read_request(&mut device, &mem, 0);

        device.process_queue(&mem).expect("the request completes");

        assert_eq!(status_of(&mem), VIRTIO_BLK_S_IOERR);
        assert_eq!(device.used_idx, 1, "the request is still completed");
    }

    /// The ordinary path: a header, one data descriptor, and a status byte. The
    /// bytes the guest receives are the bytes on the disk.
    #[test]
    fn a_conforming_read_delivers_the_disk_contents() {
        let contents: Vec<u8> = (0..DISK_BYTES).map(|i| (i % 251) as u8).collect();
        let (mut device, _dir) = device_with_disk(&contents);
        let mem = test_memory();

        let want = 1024u32;
        write_desc(
            &mem,
            0,
            REQ_HEADER,
            BLK_HEADER_BYTES as u32,
            VIRTQ_DESC_F_NEXT,
            1,
        );
        write_desc(
            &mem,
            1,
            DATA_BUFFER,
            want,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            2,
        );
        write_desc(&mem, 2, STATUS_BYTE, 1, VIRTQ_DESC_F_WRITE, 0);
        // Sector 2 is byte offset 1024.
        post_read_request(&mut device, &mem, 2);

        device.process_queue(&mem).expect("the request completes");

        assert_eq!(status_of(&mem), VIRTIO_BLK_S_OK);
        let mut got = vec![0u8; want as usize];
        mem.read_slice(&mut got, GuestAddress(DATA_BUFFER)).unwrap();
        assert_eq!(got, contents[1024..1024 + want as usize]);
    }

    /// A read that runs past the end of the disk still reads as zeros there,
    /// which is the behaviour the streaming rewrite has to preserve.
    #[test]
    fn a_read_past_the_end_of_the_disk_is_zero_filled() {
        let contents: Vec<u8> = vec![0xAB; DISK_BYTES];
        let (mut device, _dir) = device_with_disk(&contents);
        let mem = test_memory();

        // Starts one sector before the end and asks for four.
        let want = 4 * SECTOR_SIZE as u32;
        let start_sector = (DISK_BYTES as u64 / SECTOR_SIZE) - 1;
        write_desc(
            &mem,
            0,
            REQ_HEADER,
            BLK_HEADER_BYTES as u32,
            VIRTQ_DESC_F_NEXT,
            1,
        );
        write_desc(
            &mem,
            1,
            DATA_BUFFER,
            want,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            2,
        );
        write_desc(&mem, 2, STATUS_BYTE, 1, VIRTQ_DESC_F_WRITE, 0);
        post_read_request(&mut device, &mem, start_sector);

        device.process_queue(&mem).expect("the request completes");

        assert_eq!(status_of(&mem), VIRTIO_BLK_S_OK);
        let mut got = vec![0u8; want as usize];
        mem.read_slice(&mut got, GuestAddress(DATA_BUFFER)).unwrap();
        let on_disk = SECTOR_SIZE as usize;
        assert!(got[..on_disk].iter().all(|&b| b == 0xAB), "the real bytes");
        assert!(
            got[on_disk..].iter().all(|&b| b == 0),
            "and zeros past the end"
        );
    }

    /// A read larger than one streaming step spans chunks, so the disk offset has
    /// to advance across them rather than restart.
    #[test]
    fn a_read_larger_than_one_chunk_is_assembled_in_order() {
        let disk_bytes = IO_CHUNK_BYTES + 8192;
        let contents: Vec<u8> = (0..disk_bytes).map(|i| (i % 251) as u8).collect();
        let (mut device, _dir) = device_with_disk(&contents);
        let mem =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), disk_bytes + 0x10_0000)]).unwrap();

        let want = (IO_CHUNK_BYTES + 4096) as u32;
        write_desc(
            &mem,
            0,
            REQ_HEADER,
            BLK_HEADER_BYTES as u32,
            VIRTQ_DESC_F_NEXT,
            1,
        );
        write_desc(
            &mem,
            1,
            DATA_BUFFER,
            want,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            2,
        );
        write_desc(&mem, 2, STATUS_BYTE, 1, VIRTQ_DESC_F_WRITE, 0);
        post_read_request(&mut device, &mem, 0);

        device.process_queue(&mem).expect("the request completes");

        assert_eq!(status_of(&mem), VIRTIO_BLK_S_OK);
        let mut got = vec![0u8; want as usize];
        mem.read_slice(&mut got, GuestAddress(DATA_BUFFER)).unwrap();
        assert_eq!(got, contents[..want as usize]);
    }

    /// A chain longer than the device accepts is refused, and refused before it
    /// reads the descriptors past the limit.
    #[test]
    fn a_chain_longer_than_the_limit_is_refused() {
        let (mut device, _dir) = test_device();
        let mem = test_memory();

        // Every descriptor points at the next, so the chain never ends on its own.
        for index in 0..16u64 {
            write_desc(
                &mem,
                index,
                REQ_HEADER,
                BLK_HEADER_BYTES as u32,
                VIRTQ_DESC_F_NEXT,
                ((index + 1) % 16) as u16,
            );
        }
        post_read_request(&mut device, &mem, 0);

        device.process_queue(&mem).expect("the request completes");

        assert_eq!(
            device.used_idx, 1,
            "the request is completed, not looped on"
        );
    }

    /// Ring addresses come from unvalidated MMIO writes, so a base near the top
    /// of the address space is reachable. Every offset added to one used to go
    /// through `unchecked_add`, which is a plain `+`.
    #[test]
    fn ring_addresses_near_the_top_of_memory_do_not_overflow() {
        let (mut device, _dir) = test_device();
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 64 * 1024)]).unwrap();

        device.queue.num = 256;
        device.queue.ready = true;
        device.queue.desc_addr = u64::MAX;
        device.queue.driver_addr = u64::MAX;
        device.queue.device_addr = u64::MAX;

        device.process_queue(&mem).expect("the queue is declined");
        assert_eq!(device.used_idx, 0, "no request is completed");
    }

    /// `QueueNum` is an MMIO register the guest writes with no validation, and
    /// the descriptor walk derives its index bound from it.
    #[test]
    fn queue_num_is_clamped_to_the_advertised_maximum() {
        let (mut device, _dir) = test_device();

        device.mmio_write(mmio::QUEUE_NUM, &u32::MAX.to_le_bytes(), None);

        assert_eq!(device.queue.num, QUEUE_MAX_SIZE);
    }
}
