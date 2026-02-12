//! virtio-net device for guest networking
//!
//! This module implements a virtio network device that presents eth0 to the guest
//! and connects to the SLIRP stack for user-mode NAT networking.
//!
//! The virtio-net device uses MMIO transport and provides:
//! - Ethernet frame transmission/reception
//! - Integration with SLIRP stack for NAT
//! - No root/TAP required

use std::sync::{Arc, Mutex};

use tracing::{debug, trace, warn};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory};

use crate::network::slirp::{SlirpStack, GUEST_MAC};
use crate::Result;

/// Virtio descriptor flags
const VIRTQ_DESC_F_NEXT: u16 = 1;

/// Virtio device type for network
pub const VIRTIO_NET_DEVICE_TYPE: u32 = 1;

/// Virtio network device features
pub mod features {
    /// Device has checksum offload
    pub const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
    /// Guest has checksum offload
    pub const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;
    /// Device has MAC address
    pub const VIRTIO_NET_F_MAC: u64 = 1 << 5;
    /// Device status available
    pub const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
    /// Control channel available
    pub const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
}

/// Virtio network header (prepended to frames)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtioNetHeader {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
}

impl VirtioNetHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// Create a new header with default values (no offloading)
    pub fn new() -> Self {
        Self::default()
    }

    /// Serialize header to bytes
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[0] = self.flags;
        bytes[1] = self.gso_type;
        bytes[2..4].copy_from_slice(&self.hdr_len.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.gso_size.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.csum_start.to_le_bytes());
        bytes[8..10].copy_from_slice(&self.csum_offset.to_le_bytes());
        bytes[10..12].copy_from_slice(&self.num_buffers.to_le_bytes());
        bytes
    }

    /// Parse header from bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            flags: bytes[0],
            gso_type: bytes[1],
            hdr_len: u16::from_le_bytes([bytes[2], bytes[3]]),
            gso_size: u16::from_le_bytes([bytes[4], bytes[5]]),
            csum_start: u16::from_le_bytes([bytes[6], bytes[7]]),
            csum_offset: u16::from_le_bytes([bytes[8], bytes[9]]),
            num_buffers: u16::from_le_bytes([bytes[10], bytes[11]]),
        })
    }
}

/// MMIO register offsets for virtio
#[allow(dead_code)]
pub mod mmio {
    pub const MAGIC_VALUE: u64 = 0x000;
    pub const VERSION: u64 = 0x004;
    pub const DEVICE_ID: u64 = 0x008;
    pub const VENDOR_ID: u64 = 0x00c;
    pub const DEVICE_FEATURES: u64 = 0x010;
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    pub const DRIVER_FEATURES: u64 = 0x020;
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    pub const QUEUE_SEL: u64 = 0x030;
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    pub const QUEUE_NUM: u64 = 0x038;
    pub const QUEUE_READY: u64 = 0x044;
    pub const QUEUE_NOTIFY: u64 = 0x050;
    pub const INTERRUPT_STATUS: u64 = 0x060;
    pub const INTERRUPT_ACK: u64 = 0x064;
    pub const STATUS: u64 = 0x070;
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    pub const CONFIG: u64 = 0x100;

    /// Virtio MMIO magic value "virt"
    pub const MAGIC: u32 = 0x74726976;
    /// Virtio MMIO version 2
    pub const VERSION_2: u32 = 2;
}

/// Queue state for virtio
#[derive(Debug, Default)]
struct QueueState {
    /// Maximum queue size
    num_max: u16,
    /// Current queue size
    num: u16,
    /// Queue ready flag
    ready: bool,
    /// Descriptor table address
    desc_addr: u64,
    /// Driver (available) ring address
    driver_addr: u64,
    /// Device (used) ring address
    device_addr: u64,
}

/// Virtio-net device state
pub struct VirtioNetDevice {
    /// SLIRP stack for networking
    slirp: Arc<Mutex<SlirpStack>>,
    /// Guest MAC address
    mac: [u8; 6],
    /// Device features
    device_features: u64,
    /// Driver-selected features
    driver_features: u64,
    /// Feature selection register
    features_sel: u32,
    /// Queue selection register
    queue_sel: u32,
    /// Device status
    status: u32,
    /// Interrupt status
    interrupt_status: u32,
    /// Configuration generation counter
    config_generation: u32,
    /// Receive queue state
    rx_queue: QueueState,
    /// Transmit queue state
    tx_queue: QueueState,
    /// Packets waiting to be received by guest
    rx_buffer: Vec<Vec<u8>>,
    /// MMIO base address
    mmio_base: u64,
    /// MMIO size
    mmio_size: u64,
    /// TX queue: next available index to process (driver's avail.idx we've consumed up to)
    tx_avail_idx: u16,
    /// TX queue: next used index we'll write (device used ring)
    tx_used_idx: u16,
    /// RX queue: next available index to consume (guest-provided buffers)
    rx_avail_idx: u16,
    /// RX queue: next used index we'll write
    rx_used_idx: u16,
}

impl VirtioNetDevice {
    /// Create a new virtio-net device with SLIRP backend
    pub fn new(slirp: Arc<Mutex<SlirpStack>>) -> Result<Self> {
        debug!("Creating virtio-net device with SLIRP backend");

        let device_features = features::VIRTIO_NET_F_MAC | features::VIRTIO_NET_F_STATUS;

        Ok(Self {
            slirp,
            mac: GUEST_MAC,
            device_features,
            driver_features: 0,
            features_sel: 0,
            queue_sel: 0,
            status: 0,
            interrupt_status: 0,
            config_generation: 0,
            rx_queue: QueueState {
                num_max: 256,
                ..Default::default()
            },
            tx_queue: QueueState {
                num_max: 256,
                ..Default::default()
            },
            rx_buffer: Vec::new(),
            mmio_base: 0,
            mmio_size: 0x200,
            tx_avail_idx: 0,
            tx_used_idx: 0,
            rx_avail_idx: 0,
            rx_used_idx: 0,
        })
    }

    /// Set the MMIO base address
    pub fn set_mmio_base(&mut self, base: u64) {
        self.mmio_base = base;
        debug!("virtio-net MMIO base set to {:#x}", base);
    }

    /// Get the MMIO base address
    pub fn mmio_base(&self) -> u64 {
        self.mmio_base
    }

    /// Get the MMIO region size
    pub fn mmio_size(&self) -> u64 {
        self.mmio_size
    }

    /// Check if an address is within this device's MMIO region
    pub fn handles_mmio(&self, addr: u64) -> bool {
        addr >= self.mmio_base && addr < self.mmio_base + self.mmio_size
    }

    /// Handle MMIO read
    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        let value: u32 = match offset {
            mmio::MAGIC_VALUE => mmio::MAGIC,
            mmio::VERSION => mmio::VERSION_2,
            mmio::DEVICE_ID => VIRTIO_NET_DEVICE_TYPE,
            mmio::VENDOR_ID => 0x554d4551, // "QEMU"
            mmio::DEVICE_FEATURES => {
                if self.features_sel == 0 {
                    self.device_features as u32
                } else {
                    (self.device_features >> 32) as u32
                }
            }
            mmio::QUEUE_NUM_MAX => {
                let queue = self.current_queue();
                queue.num_max as u32
            }
            mmio::QUEUE_READY => {
                let queue = self.current_queue();
                queue.ready as u32
            }
            mmio::INTERRUPT_STATUS => self.interrupt_status,
            mmio::STATUS => self.status,
            mmio::CONFIG_GENERATION => self.config_generation,
            // Device config (MAC address at offset 0x100)
            o if o >= mmio::CONFIG && o < mmio::CONFIG + 6 => {
                let idx = (o - mmio::CONFIG) as usize;
                self.mac[idx] as u32
            }
            // Device config (status at offset 0x106)
            o if o == mmio::CONFIG + 6 => {
                // Link up status
                1
            }
            _ => {
                trace!("virtio-net: unhandled MMIO read at offset {:#x}", offset);
                0
            }
        };

        // Write value to data buffer
        let bytes = value.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    /// Handle MMIO write. Pass guest_memory when available so queue notify can process TX/RX.
    pub fn mmio_write<M: GuestMemory + ?Sized>(
        &mut self,
        offset: u64,
        data: &[u8],
        guest_memory: Option<&M>,
    ) {
        if data.is_empty() {
            return;
        }

        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let value = u32::from_le_bytes(bytes);

        match offset {
            mmio::DEVICE_FEATURES_SEL => {
                self.features_sel = value;
            }
            mmio::DRIVER_FEATURES => {
                if self.features_sel == 0 {
                    self.driver_features = (self.driver_features & 0xFFFF_FFFF_0000_0000)
                        | (value as u64);
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                }
            }
            mmio::DRIVER_FEATURES_SEL => {
                self.features_sel = value;
            }
            mmio::QUEUE_SEL => {
                self.queue_sel = value;
            }
            mmio::QUEUE_NUM => {
                let queue = self.current_queue_mut();
                queue.num = value as u16;
            }
            mmio::QUEUE_READY => {
                let queue = self.current_queue_mut();
                queue.ready = value != 0;
                if queue.ready {
                    debug!("virtio-net: queue {} ready", self.queue_sel);
                }
            }
            mmio::QUEUE_NOTIFY => {
                self.handle_queue_notify(value, guest_memory);
            }
            mmio::INTERRUPT_ACK => {
                self.interrupt_status &= !value;
            }
            mmio::STATUS => {
                self.status = value;
                if value == 0 {
                    self.reset();
                }
            }
            mmio::QUEUE_DESC_LOW => {
                let queue = self.current_queue_mut();
                queue.desc_addr = (queue.desc_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DESC_HIGH => {
                let queue = self.current_queue_mut();
                queue.desc_addr =
                    (queue.desc_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            mmio::QUEUE_DRIVER_LOW => {
                let queue = self.current_queue_mut();
                queue.driver_addr = (queue.driver_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DRIVER_HIGH => {
                let queue = self.current_queue_mut();
                queue.driver_addr =
                    (queue.driver_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            mmio::QUEUE_DEVICE_LOW => {
                let queue = self.current_queue_mut();
                queue.device_addr = (queue.device_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DEVICE_HIGH => {
                let queue = self.current_queue_mut();
                queue.device_addr =
                    (queue.device_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            _ => {
                trace!(
                    "virtio-net: unhandled MMIO write at offset {:#x}, value={:#x}",
                    offset,
                    value
                );
            }
        }
    }

    /// Get current queue based on queue_sel
    fn current_queue(&self) -> &QueueState {
        match self.queue_sel {
            0 => &self.rx_queue,
            1 => &self.tx_queue,
            _ => &self.rx_queue,
        }
    }

    /// Get current queue mutably
    fn current_queue_mut(&mut self) -> &mut QueueState {
        match self.queue_sel {
            0 => &mut self.rx_queue,
            1 => &mut self.tx_queue,
            _ => &mut self.rx_queue,
        }
    }

    /// Handle queue notification (guest has added buffers)
    fn handle_queue_notify<M: GuestMemory + ?Sized>(
        &mut self,
        queue_idx: u32,
        guest_memory: Option<&M>,
    ) {
        match queue_idx {
            0 => {
                // RX queue - guest has provided receive buffers; try to inject pending frames
                if let Some(mem) = guest_memory {
                    let _ = self.try_inject_rx(mem);
                } else {
                    trace!("virtio-net: RX queue notified (no guest memory)");
                }
            }
            1 => {
                // TX queue - guest wants to send packets
                if let Some(mem) = guest_memory {
                    if let Err(e) = self.process_tx_queue(mem) {
                        warn!("virtio-net: TX queue processing error: {}", e);
                    }
                } else {
                    trace!("virtio-net: TX queue notified (no guest memory)");
                }
            }
            _ => {
                warn!("virtio-net: unknown queue {} notified", queue_idx);
            }
        }
    }

    /// Process TX queue: read descriptor chains from guest, send frames to SLIRP, update used ring.
    fn process_tx_queue<M: GuestMemory + ?Sized>(&mut self, mem: &M) -> Result<()> {
        let q = &self.tx_queue;
        if !q.ready || q.num == 0 {
            return Ok(());
        }
        let desc_addr = GuestAddress(q.desc_addr);
        let avail_addr = GuestAddress(q.driver_addr);
        let used_addr = GuestAddress(q.device_addr);
        let queue_size = q.num as usize;

        // Read available ring: flags at 0, idx at 2, ring starts at 4
        let mut idx_buf = [0u8; 2];
        mem.read(&mut idx_buf, avail_addr.unchecked_add(2u64))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let avail_idx = u16::from_le_bytes(idx_buf);

        while self.tx_avail_idx != avail_idx {
            // Ring entry: 2 bytes, at avail_addr + 4 + tx_avail_idx*2
            let ring_offset = 4 + (self.tx_avail_idx as usize) * 2;
            let mut desc_id_buf = [0u8; 2];
            mem.read(
                &mut desc_id_buf,
                avail_addr.unchecked_add(ring_offset as u64),
            )
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let head_idx = u16::from_le_bytes(desc_id_buf) as usize;

            // Walk descriptor chain and collect packet
            let mut packet = Vec::new();
            let mut next = head_idx;
            loop {
                if next >= queue_size {
                    break;
                }
                let desc_off = desc_addr.unchecked_add((next * 16) as u64);
                let mut desc = [0u8; 16];
                mem.read(&mut desc, desc_off)
                    .map_err(|e| crate::Error::Memory(e.to_string()))?;
                let addr = u64::from_le_bytes(desc[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(desc[8..12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(desc[12..14].try_into().unwrap());
                let next_desc = u16::from_le_bytes(desc[14..16].try_into().unwrap()) as usize;
                if len > 0 && addr != 0 {
                    let mut buf = vec![0u8; len];
                    mem.read(&mut buf, GuestAddress(addr))
                        .map_err(|e| crate::Error::Memory(e.to_string()))?;
                    packet.extend_from_slice(&buf);
                }
                if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                    break;
                }
                next = next_desc;
            }

            if !packet.is_empty() {
                self.process_tx_frame(&packet)?;
            }

            // Write used ring: used->ring[tx_used_idx] = { id: head_idx, len: 0 }
            let used_ring_off = 4 + (self.tx_used_idx as usize) * 8;
            let used_elem = [
                (head_idx as u32).to_le_bytes(),
                0u32.to_le_bytes(), // len for TX typically 0
            ]
            .concat();
            mem.write(&used_elem, used_addr.unchecked_add(used_ring_off as u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;

            self.tx_used_idx = self.tx_used_idx.wrapping_add(1);
            self.tx_avail_idx = self.tx_avail_idx.wrapping_add(1);

            // Update used.idx so guest sees progress
            let used_idx_bytes = self.tx_used_idx.to_le_bytes();
            mem.write(&used_idx_bytes, used_addr.unchecked_add(2u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
        }

        self.interrupt_status |= 1;
        Ok(())
    }

    /// Try to inject received frames from SLIRP into guest RX queue. Call from vCPU loop or after RX notify.
    pub fn try_inject_rx<M: GuestMemory + ?Sized>(&mut self, mem: &M) -> Result<()> {
        let frames = self.get_rx_frames();
        if frames.is_empty() {
            return Ok(());
        }

        let q = &self.rx_queue;
        if !q.ready || q.num == 0 {
            return Ok(());
        }
        let desc_addr = GuestAddress(q.desc_addr);
        let avail_addr = GuestAddress(q.driver_addr);
        let used_addr = GuestAddress(q.device_addr);
        let queue_size = q.num as usize;

        for frame in frames {
            // Read available ring: how many buffers has driver given us?
            let mut idx_buf = [0u8; 2];
            mem.read(&mut idx_buf, avail_addr.unchecked_add(2u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let avail_idx = u16::from_le_bytes(idx_buf);
            if self.rx_avail_idx == avail_idx {
                self.rx_buffer.push(frame);
                continue;
            }

            let ring_offset = 4 + (self.rx_avail_idx as usize) * 2;
            let mut desc_id_buf = [0u8; 2];
            mem.read(
                &mut desc_id_buf,
                avail_addr.unchecked_add(ring_offset as u64),
            )
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let head_idx = u16::from_le_bytes(desc_id_buf) as usize;

            let mut next = head_idx;
            let mut written = 0;
            let frame_len = frame.len();
            let mut frame_off = 0;

            loop {
                if next >= queue_size || frame_off >= frame_len {
                    break;
                }
                let desc_off = desc_addr.unchecked_add((next * 16) as u64);
                let mut desc = [0u8; 16];
                mem.read(&mut desc, desc_off)
                    .map_err(|e| crate::Error::Memory(e.to_string()))?;
                let addr = u64::from_le_bytes(desc[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(desc[8..12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(desc[12..14].try_into().unwrap());
                let next_desc = u16::from_le_bytes(desc[14..16].try_into().unwrap()) as usize;

                if len > 0 && addr != 0 {
                    let to_write = (len).min(frame_len - frame_off);
                    mem.write(&frame[frame_off..frame_off + to_write], GuestAddress(addr))
                        .map_err(|e| crate::Error::Memory(e.to_string()))?;
                    written += to_write;
                    frame_off += to_write;
                }

                if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                    break;
                }
                next = next_desc;
            }

            let used_ring_off = 4 + (self.rx_used_idx as usize) * 8;
            let used_elem = [
                (head_idx as u32).to_le_bytes(),
                (written as u32).to_le_bytes(),
            ]
            .concat();
            mem.write(&used_elem, used_addr.unchecked_add(used_ring_off as u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;

            self.rx_used_idx = self.rx_used_idx.wrapping_add(1);
            self.rx_avail_idx = self.rx_avail_idx.wrapping_add(1);

            let used_idx_bytes = self.rx_used_idx.to_le_bytes();
            mem.write(&used_idx_bytes, used_addr.unchecked_add(2u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
        }

        self.interrupt_status |= 1;
        Ok(())
    }

    /// Reset device to initial state
    fn reset(&mut self) {
        debug!("virtio-net: device reset");
        self.status = 0;
        self.interrupt_status = 0;
        self.driver_features = 0;
        self.tx_avail_idx = 0;
        self.tx_used_idx = 0;
        self.rx_avail_idx = 0;
        self.rx_used_idx = 0;
        self.rx_queue = QueueState {
            num_max: 256,
            ..Default::default()
        };
        self.tx_queue = QueueState {
            num_max: 256,
            ..Default::default()
        };
        self.rx_buffer.clear();
    }

    /// Process a frame from the guest (TX path)
    pub fn process_tx_frame(&mut self, frame_with_header: &[u8]) -> Result<()> {
        // Skip virtio-net header
        if frame_with_header.len() <= VirtioNetHeader::SIZE {
            return Ok(());
        }

        let frame = &frame_with_header[VirtioNetHeader::SIZE..];
        trace!("virtio-net TX: {} bytes", frame.len());

        // Send to SLIRP stack
        let mut slirp = self.slirp.lock().unwrap();
        slirp.process_guest_frame(frame)?;

        Ok(())
    }

    /// Get frames waiting to be received by guest (RX path)
    pub fn get_rx_frames(&mut self) -> Vec<Vec<u8>> {
        // Poll SLIRP for new packets
        let frames = {
            let mut slirp = self.slirp.lock().unwrap();
            slirp.poll()
        };

        // Prepend virtio-net header to each frame
        let mut result = Vec::new();
        for frame in frames {
            let mut packet = Vec::with_capacity(VirtioNetHeader::SIZE + frame.len());
            packet.extend_from_slice(&VirtioNetHeader::new().to_bytes());
            packet.extend_from_slice(&frame);
            result.push(packet);
        }

        // Also return any buffered frames
        result.append(&mut self.rx_buffer);

        result
    }

    /// Queue a frame for reception by the guest
    pub fn queue_rx_frame(&mut self, frame: Vec<u8>) {
        let mut packet = Vec::with_capacity(VirtioNetHeader::SIZE + frame.len());
        packet.extend_from_slice(&VirtioNetHeader::new().to_bytes());
        packet.extend_from_slice(&frame);
        self.rx_buffer.push(packet);

        // Set interrupt
        self.interrupt_status |= 1;
    }

    /// Check if there are pending interrupts
    pub fn has_pending_interrupt(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Get the MAC address
    pub fn mac(&self) -> &[u8; 6] {
        &self.mac
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtio_net_header() {
        let header = VirtioNetHeader::new();
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), VirtioNetHeader::SIZE);

        let parsed = VirtioNetHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.flags, 0);
        assert_eq!(parsed.gso_type, 0);
    }

    #[test]
    fn test_mmio_magic() {
        let slirp = Arc::new(Mutex::new(SlirpStack::new().unwrap()));
        let device = VirtioNetDevice::new(slirp).unwrap();

        let mut data = [0u8; 4];
        device.mmio_read(mmio::MAGIC_VALUE, &mut data);
        let magic = u32::from_le_bytes(data);
        assert_eq!(magic, mmio::MAGIC);
    }

    #[test]
    fn test_mmio_version() {
        let slirp = Arc::new(Mutex::new(SlirpStack::new().unwrap()));
        let device = VirtioNetDevice::new(slirp).unwrap();

        let mut data = [0u8; 4];
        device.mmio_read(mmio::VERSION, &mut data);
        let version = u32::from_le_bytes(data);
        assert_eq!(version, mmio::VERSION_2);
    }

    #[test]
    fn test_device_type() {
        let slirp = Arc::new(Mutex::new(SlirpStack::new().unwrap()));
        let device = VirtioNetDevice::new(slirp).unwrap();

        let mut data = [0u8; 4];
        device.mmio_read(mmio::DEVICE_ID, &mut data);
        let device_id = u32::from_le_bytes(data);
        assert_eq!(device_id, VIRTIO_NET_DEVICE_TYPE);
    }
}
