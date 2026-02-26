//! Minimal virtio-blk MMIO device (read-only raw file backend).
//!
//! This device is used to present OCI rootfs disk artifacts as a block device
//! to the guest on Linux/KVM.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use tracing::{debug, trace, warn};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use crate::devices::virtio_net::mmio;

pub const VIRTIO_BLK_DEVICE_TYPE: u32 = 2;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;

const QUEUE_MAX_SIZE: u16 = 128;
const SECTOR_SIZE: u64 = 512;

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
            mmio::QUEUE_NUM => self.queue.num = value as u16,
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

        let desc_addr = GuestAddress(q.desc_addr);
        let avail_addr = GuestAddress(q.driver_addr);
        let used_addr = GuestAddress(q.device_addr);
        let queue_size = q.num as usize;

        let mut idx_buf = [0u8; 2];
        mem.read(&mut idx_buf, avail_addr.unchecked_add(2u64))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let avail_idx = u16::from_le_bytes(idx_buf);

        while self.avail_idx != avail_idx {
            let ring_offset = 4 + ((self.avail_idx as usize) % queue_size) * 2;
            let mut head_buf = [0u8; 2];
            mem.read(&mut head_buf, avail_addr.unchecked_add(ring_offset as u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let head = u16::from_le_bytes(head_buf) as usize;

            let (status, written) = self.handle_request(mem, desc_addr, queue_size, head)?;

            let used_ring_off = 4 + ((self.used_idx as usize) % queue_size) * 8;
            let used_elem = [(head as u32).to_le_bytes(), (written as u32).to_le_bytes()].concat();
            mem.write(&used_elem, used_addr.unchecked_add(used_ring_off as u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            self.used_idx = self.used_idx.wrapping_add(1);
            self.avail_idx = self.avail_idx.wrapping_add(1);

            let used_idx_bytes = self.used_idx.to_le_bytes();
            mem.write(&used_idx_bytes, used_addr.unchecked_add(2u64))
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
        desc_base: GuestAddress,
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
            let off = desc_base.unchecked_add((idx * 16) as u64);
            let mut raw = [0u8; 16];
            mem.read(&mut raw, off)
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
            if descs.len() > 32 {
                return Ok((VIRTIO_BLK_S_IOERR, 0));
            }
        }

        if descs.len() < 2 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }

        // Header must be readable by device
        if (descs[0].flags & VIRTQ_DESC_F_WRITE) != 0 || descs[0].len < 16 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }

        let mut hdr = [0u8; 16];
        mem.read(&mut hdr, GuestAddress(descs[0].addr))
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
                let mut file_off = offset;
                for d in data_descs {
                    if (d.flags & VIRTQ_DESC_F_WRITE) == 0 {
                        return Ok((VIRTIO_BLK_S_IOERR, total_written));
                    }
                    let mut buf = vec![0u8; d.len as usize];
                    let mut n = 0usize;
                    while n < buf.len() {
                        match self.disk.read_at(&mut buf[n..], file_off.saturating_add(n as u64)) {
                            Ok(0) => break, // EOF: keep remaining bytes zero-filled
                            Ok(read_now) => n += read_now,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => return Ok((VIRTIO_BLK_S_IOERR, total_written)),
                        }
                    }
                    if n < buf.len() {
                        for b in &mut buf[n..] {
                            *b = 0;
                        }
                    }
                    mem.write(&buf, GuestAddress(d.addr))
                        .map_err(|e| crate::Error::Memory(e.to_string()))?;
                    file_off = file_off.saturating_add(d.len as u64);
                    total_written += d.len as usize;
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

        mem.write(&[status], GuestAddress(status_desc.addr))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        total_written += 1;

        Ok((status, total_written))
    }
}
