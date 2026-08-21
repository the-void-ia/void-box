//! virtio-net device for guest networking
//!
//! This module implements a virtio network device that presents eth0 to the guest
//! and connects to the SLIRP stack for user-mode NAT networking.
//!
//! The virtio-net device uses MMIO transport and provides:
//! - Ethernet frame transmission/reception
//! - Integration with SLIRP stack for NAT
//! - No root/TAP required

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_queue::SegQueue;
use tracing::{debug, trace, warn};
use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::devices::virtqueue::{mapped_ring, ring_addr};
use crate::network::slirp::GUEST_MAC;
use crate::network::NetworkBackend;
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
    /// Required for virtio-mmio version 2 devices
    pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
}

/// Largest guest→host frame the device assembles from one descriptor chain.
///
/// Every descriptor carries a guest-written `len` that sizes a host allocation,
/// so without a ceiling one chain can ask for as much memory as the guest cares
/// to name. This bounds the allocation; whether the assembled bytes are the frame
/// the chain described is a separate check after the walk.
///
/// The sum is the largest IP packet, its Ethernet framing, room for two VLAN
/// tags, and the virtio-net header. A Linux guest offered no `VIRTIO_NET_F_MTU`
/// sets `max_mtu` to 65535, so that is the largest frame a driver can hand over;
/// the tags are counted so a frame at that MTU cannot land above the ceiling for
/// carrying them.
const MAX_TX_FRAME_BYTES: usize =
    MAX_IP_PACKET_BYTES + ETHERNET_HEADER_BYTES + VLAN_TAG_BYTES * 2 + VirtioNetHeader::SIZE;

/// Largest IP packet, from the 16-bit total-length field.
const MAX_IP_PACKET_BYTES: usize = 65535;

/// Destination, source, and EtherType.
const ETHERNET_HEADER_BYTES: usize = 14;

/// One 802.1Q tag.
const VLAN_TAG_BYTES: usize = 4;

/// Largest queue the device advertises in `QueueNumMax`, and the ceiling every
/// queue size is held to however it is set — an MMIO write or a restored
/// snapshot. Both descriptor walks bound their chain by the queue size, so this
/// is what keeps that bound the one the device published.
const QUEUE_MAX_SIZE: u16 = 256;

/// Bytes in one used-ring element: the descriptor id and the length written.
const USED_ELEM_BYTES: usize = 8;

/// Bytes in one descriptor-table entry: address, length, flags, and next.
const DESC_BYTES: usize = 16;

/// Bytes a split ring spends before its entries: flags and index.
const RING_HEADER_BYTES: usize = 4;

/// Resolve the three rings of a queue, requiring each to be mapped at full size.
///
/// Returns their base addresses, or `None` when any of them leaves the address
/// space or is not entirely backed by guest memory.
fn queue_rings<M: GuestMemory + ?Sized>(
    mem: &M,
    desc_base: u64,
    avail_base: u64,
    used_base: u64,
    queue_size: usize,
) -> Option<(GuestAddress, GuestAddress, GuestAddress)> {
    let desc = mapped_ring(mem, desc_base, queue_size * DESC_BYTES)?;
    let avail = mapped_ring(mem, avail_base, RING_HEADER_BYTES + queue_size * 2)?;
    let used = mapped_ring(
        mem,
        used_base,
        RING_HEADER_BYTES + queue_size * USED_ELEM_BYTES,
    )?;
    Some((desc, avail, used))
}

/// Frames the device will hold for a guest that is not consuming them.
///
/// A frame that cannot be delivered is kept for the next call rather than
/// dropped, because both callers clear the batch afterwards. That turns a guest
/// which never posts RX buffers — or programs a ring the device declines — into
/// host memory growth at the rate inbound traffic arrives, so the backlog has a
/// ceiling and the oldest frame goes first, the way a full NIC ring behaves.
const MAX_RX_BUFFERED_FRAMES: usize = 1024;

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
    /// Network backend (SLIRP or any [`NetworkBackend`] impl)
    slirp: Arc<Mutex<dyn NetworkBackend>>,
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
    /// Interrupt status, accessed concurrently from the vCPU thread
    /// (MMIO read of `INTERRUPT_STATUS`, MMIO write of `INTERRUPT_ACK`)
    /// and the net-poll thread (sets bit 0 when new RX frames are
    /// queued, polls on idle cycles).
    ///
    /// Wrapped in [`Arc<AtomicU32>`] so the net-poll thread can hold
    /// its own clone and read/update the value without taking the
    /// device mutex.  The vCPU thread accesses it via the device
    /// guard during MMIO dispatch; both sides see the same atomic.
    interrupt_status: Arc<AtomicU32>,
    /// Configuration generation counter
    config_generation: u32,
    /// Receive queue state
    rx_queue: QueueState,
    /// Transmit queue state
    tx_queue: QueueState,
    /// Packets waiting to be received by guest
    rx_buffer: Vec<Vec<u8>>,
    /// Scratch buffer reused across `drain_to_guest` calls to avoid per-poll allocation
    rx_scratch: Vec<Vec<u8>>,
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
    /// Lock-free queue of frames waiting to be written into the guest's
    /// RX descriptors.  The net-poll thread pushes frames here without
    /// taking the device lock; the vCPU thread drains them on its next
    /// MMIO exit (via [`Self::flush_pending_rx`]) and writes the
    /// descriptors in its own context.
    ///
    /// Eliminates the `Arc<Mutex<VirtioNetDevice>>` contention that
    /// previously serialised every net-poll-side `try_inject_rx` call
    /// against vCPU MMIO exits.
    pending_rx: Arc<SegQueue<Vec<u8>>>,
    /// Scratch buffer reused across `flush_pending_rx` calls so the
    /// per-MMIO-exit `Vec<Vec<u8>>` doesn't grow from cap=0 every
    /// time.  Heaptrack measured the previous local-Vec allocation as
    /// 173 calls / 108 MB peak on the CRR microbench.
    flush_scratch: Vec<Vec<u8>>,
}

impl VirtioNetDevice {
    /// Create a new virtio-net device with the given network backend
    pub fn new(slirp: Arc<Mutex<dyn NetworkBackend>>) -> Result<Self> {
        debug!("Creating virtio-net device with SLIRP backend");

        let device_features = features::VIRTIO_NET_F_MAC
            | features::VIRTIO_NET_F_STATUS
            | features::VIRTIO_F_VERSION_1;

        Ok(Self {
            slirp,
            mac: GUEST_MAC,
            device_features,
            driver_features: 0,
            features_sel: 0,
            queue_sel: 0,
            status: 0,
            interrupt_status: Arc::new(AtomicU32::new(0)),
            config_generation: 0,
            rx_queue: QueueState {
                num_max: QUEUE_MAX_SIZE,
                ..Default::default()
            },
            tx_queue: QueueState {
                num_max: QUEUE_MAX_SIZE,
                ..Default::default()
            },
            rx_buffer: Vec::new(),
            rx_scratch: Vec::new(),
            mmio_base: 0,
            mmio_size: 0x200,
            tx_avail_idx: 0,
            tx_used_idx: 0,
            rx_avail_idx: 0,
            rx_used_idx: 0,
            pending_rx: Arc::new(SegQueue::new()),
            flush_scratch: Vec::new(),
        })
    }

    /// Returns a clone of the lock-free RX frame queue Arc.
    ///
    /// The net-poll thread holds this clone and pushes frames to it
    /// without ever taking the [`VirtioNetDevice`] mutex.  The vCPU
    /// thread (which already holds the device mutex during MMIO
    /// dispatch) drains it via [`Self::flush_pending_rx`].
    pub fn pending_rx(&self) -> Arc<SegQueue<Vec<u8>>> {
        Arc::clone(&self.pending_rx)
    }

    /// Returns a clone of the [`NetworkBackend`] arc.
    ///
    /// Lets the net-poll thread call `drain_to_guest` directly without
    /// going through the device mutex.  Combined with [`Self::pending_rx`],
    /// this removes the `Arc<Mutex<VirtioNetDevice>>` contention point
    /// from the per-packet RX hot path.
    pub fn slirp_arc(&self) -> Arc<Mutex<dyn NetworkBackend>> {
        Arc::clone(&self.slirp)
    }

    /// Returns a clone of the [`Arc<AtomicU32>`] backing
    /// `interrupt_status`.  The net-poll thread holds this clone and
    /// reads/updates the ISR without ever taking the device mutex.
    pub fn interrupt_status_arc(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.interrupt_status)
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
            mmio::INTERRUPT_STATUS => self.interrupt_status.load(Ordering::Relaxed),
            mmio::STATUS => self.status,
            mmio::CONFIG_GENERATION => self.config_generation,
            // Device config (MAC address at offset 0x100)
            o if (mmio::CONFIG..mmio::CONFIG + 6).contains(&o) => {
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
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | (value as u64);
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
                // The guest may not exceed the size the device advertises in
                // `QueueNumMax`; the virtio spec forbids it, and both descriptor
                // walks derive their chain bound from this value, so an unclamped
                // write would let the guest choose its own bound.
                let queue = self.current_queue_mut();
                queue.num = (value as u16).min(queue.num_max);
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
                self.interrupt_status.fetch_and(!value, Ordering::Relaxed);
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
                debug!("virtio-net: TX queue notified");
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

    /// Process the TX queue from outside the vCPU thread.
    ///
    /// Called by `net_poll_thread` when the KVM_IOEVENTFD registered for
    /// the virtio-net QUEUE_NOTIFY MMIO fires.  Same body as the
    /// synchronous TX-queue handler used from the MMIO write path,
    /// just exposed under a different name so callers outside this
    /// module can drive it.
    pub fn process_tx_queue_external<M: GuestMemory + ?Sized>(&mut self, mem: &M) -> Result<()> {
        self.process_tx_queue(mem)
    }

    /// Process TX queue: read descriptor chains from guest, send frames to SLIRP, update used ring.
    fn process_tx_queue<M: GuestMemory + ?Sized>(&mut self, mem: &M) -> Result<()> {
        let q = &self.tx_queue;
        if !q.ready || q.num == 0 {
            return Ok(());
        }
        // Every address below is a base the guest wrote to an MMIO register plus
        // an offset. `vm_memory`'s `unchecked_add` is a plain `+`: it panics under
        // `overflow-checks` and wraps otherwise, so a base near `u64::MAX` either
        // ends the VMM process or folds the access onto unrelated mapped memory.
        // `ring_addr` rejects the address instead, leaving the guest with a queue
        // it programmed unusably.
        let (desc_base, avail_base, used_base) = (q.desc_addr, q.driver_addr, q.device_addr);
        let queue_size = q.num as usize;

        // Each of the three rings is checked once, at full size, before anything
        // is read from it. A queue whose rings are not entirely mapped is one the
        // guest cannot operate, so the device declines the whole queue rather
        // than discovering the gap partway through a batch — which is where a
        // half-published used ring or an unretired descriptor comes from.
        let Some(rings) = queue_rings(mem, desc_base, avail_base, used_base, queue_size) else {
            warn!("virtio-net: TX queue rings are not fully mapped; declining the queue");
            return Ok(());
        };
        let (desc_ring, avail_ring, used_ring) = rings;

        // Read available ring: flags at 0, idx at 2, ring starts at 4
        let mut idx_buf = [0u8; 2];
        mem.read_slice(&mut idx_buf, GuestAddress(avail_ring.0 + 2))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let avail_idx = u16::from_le_bytes(idx_buf);

        let initial_tx_used_idx = self.tx_used_idx;

        // Reusable per-call packet buffer.  Capacity carried across
        // iterations within this call so chained-descriptor frames don't
        // re-grow the buffer; cleared between frames so each
        // process_tx_frame sees only this frame's bytes.  Pre-size to
        // a typical MTU + virtio-net header so the common single-segment
        // path needs no realloc.
        let mut packet: Vec<u8> = Vec::with_capacity(1600);

        // A queue holds at most `queue_size` buffers, so a larger gap between the
        // guest's `avail_idx` and ours describes buffers the guest cannot have
        // posted. Without this the 16-bit index difference alone admits 65535
        // chains from one kick, each assembling a full frame — gigabytes of copying
        // on the vCPU thread. Anything genuinely outstanding is picked up by the
        // next kick.
        let mut batched = 0usize;
        // A malformed chain is guest-triggerable at will, so it is counted and
        // reported once per call rather than logged per chain — a warn! per
        // descriptor is formatting work the guest schedules on the vCPU thread.
        let mut malformed_chains = 0usize;
        // The counter is the only signal a field report will carry if a chain a
        // real driver produces is ever rejected, so it keeps the first reason and
        // the two lengths that disagreed rather than reporting a bare count.
        let mut first_reason: Option<(&'static str, usize, usize)> = None;

        while self.tx_avail_idx != avail_idx {
            if batched >= queue_size {
                warn!("virtio-net: TX batch exceeds the {queue_size}-buffer queue; deferring the rest");
                break;
            }
            batched += 1;

            // Ring entry: 2 bytes, at avail_addr + 4 + (tx_avail_idx % queue_size)*2
            let ring_offset = 4 + ((self.tx_avail_idx as usize) % queue_size) * 2;
            let ring_entry_addr = GuestAddress(avail_ring.0 + ring_offset as u64);
            let used_ring_off = 4 + ((self.tx_used_idx as usize) % queue_size) * USED_ELEM_BYTES;
            let used_elem_addr = GuestAddress(used_ring.0 + used_ring_off as u64);
            let mut desc_id_buf = [0u8; 2];
            mem.read_slice(&mut desc_id_buf, ring_entry_addr)
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let head_idx = u16::from_le_bytes(desc_id_buf) as usize;

            packet.clear();
            let mut next = head_idx;
            let mut walked = 0usize;
            // The walk has one legitimate exit: a descriptor without NEXT. Every
            // other way out leaves `packet` holding something the guest did not
            // describe — a prefix, or a cycle's worth of the same buffer repeated
            // — and a fabrication is not a frame. A real adapter drops such a
            // packet rather than putting it on the wire.
            let mut malformed = false;
            let mut reason = "assembled bytes differ from the bytes described";
            // What the chain says it carries, as opposed to what was assembled.
            // A descriptor with `addr == 0` contributes no bytes but still
            // describes them, so comparing assembled against described is what
            // catches a frame the guest did not describe — the ceiling alone
            // measures only the buffer that grew.
            let mut described = 0usize;
            loop {
                // An index outside the table the guest allocated. Whatever has
                // been assembled stops short of the chain the guest described.
                if next >= queue_size {
                    malformed = true;
                    reason = "next index outside the descriptor table";
                    break;
                }
                // A chain cannot be longer than the table it indexes into. A
                // descriptor whose NEXT points back into the chain — at itself,
                // or around a cycle — would otherwise spin here forever, and
                // `packet` would grow by that descriptor's guest-written length
                // on every pass. What it accumulated before the bound is the same
                // buffer repeated, not a frame.
                if walked >= queue_size {
                    malformed = true;
                    reason = "chain longer than the descriptor table";
                    break;
                }
                walked += 1;

                let desc_off = GuestAddress(desc_ring.0 + (next * DESC_BYTES) as u64);
                let mut desc = [0u8; DESC_BYTES];
                if mem.read_slice(&mut desc, desc_off).is_err() {
                    malformed = true;
                    reason = "descriptor unreadable";
                    break;
                }
                let addr = u64::from_le_bytes(desc[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(desc[8..12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(desc[12..14].try_into().unwrap());
                let next_desc = u16::from_le_bytes(desc[14..16].try_into().unwrap()) as usize;
                // `len` is guest-written and sizes the allocation below, so the
                // ceiling bounds what one chain can ask for. A clamped read does
                // not silently shorten the frame: it leaves the assembled bytes
                // short of the described ones, and the check after the walk drops
                // the chain.
                described = described.saturating_add(len);
                let readable = len.min(MAX_TX_FRAME_BYTES.saturating_sub(packet.len()));
                if readable > 0 && addr != 0 {
                    // Read directly into the packet's tail instead of
                    // allocating an intermediate `Vec<u8>` and then
                    // `extend_from_slice`-ing it in.  Saves one alloc
                    // and one full memcpy per descriptor segment.
                    let off = packet.len();
                    packet.resize(off + readable, 0);
                    // A payload the device cannot read is a chain it cannot
                    // process, which is the malformed case: drop the frame and
                    // retire the descriptor so the guest's ring drains. Failing
                    // the call instead would leave the descriptor outstanding and
                    // wedge the queue on every later kick.
                    if mem
                        .read_slice(&mut packet[off..off + readable], GuestAddress(addr))
                        .is_err()
                    {
                        malformed = true;
                        reason = "payload unreadable";
                        break;
                    }
                }
                if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                    break;
                }
                next = next_desc;
            }

            // Every byte the chain described has to be in the frame. Anything
            // else — the ceiling clamping a read, a descriptor pointing at guest
            // address 0, a walk that ended early — leaves a frame the guest did
            // not describe.
            if packet.len() != described {
                malformed = true;
            }

            if malformed {
                malformed_chains += 1;
                first_reason.get_or_insert((reason, packet.len(), described));
            } else if !packet.is_empty() {
                // A backend refusal drops this frame; the descriptor is still
                // retired below, because leaving it outstanding would replay the
                // same frame on every later kick.
                if let Err(e) = self.process_tx_frame(&packet) {
                    warn!("virtio-net: backend refused a TX frame: {e}");
                }
            }

            // Used-ring entry: 8 bytes (head_idx as u32, 0 as u32).
            // Built on the stack to avoid heap-alloc-per-frame from
            // `[...].concat()`.  TX descriptors carry no return data
            // so the length field is always 0. A dropped frame is still
            // completed, so the guest gets its descriptor back.
            let mut used_elem = [0u8; USED_ELEM_BYTES];
            used_elem[0..4].copy_from_slice(&(head_idx as u32).to_le_bytes());
            // bytes [4..8] stay zero (the length field).
            mem.write_slice(&used_elem, used_elem_addr)
                .map_err(|e| crate::Error::Memory(e.to_string()))?;

            self.tx_used_idx = self.tx_used_idx.wrapping_add(1);
            self.tx_avail_idx = self.tx_avail_idx.wrapping_add(1);
        }

        // Publish used.idx ONCE per batch instead of after every frame.
        // virtio spec: the device updates the used-ring entries first,
        // then bumps used.idx; the guest reads used.idx with a memory
        // barrier and iterates new entries.  Per-frame writes are
        // redundant for correctness and waste one mem.write per frame.
        if let Some((reason, assembled, described)) = first_reason {
            warn!(
                "virtio-net: dropped {malformed_chains} malformed TX chain(s) this kick; \
                 first was {reason} (assembled {assembled} of {described} bytes)"
            );
        }

        if self.tx_used_idx != initial_tx_used_idx {
            let used_idx_bytes = self.tx_used_idx.to_le_bytes();
            mem.write_slice(&used_idx_bytes, GuestAddress(used_ring.0 + 2))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
        }

        self.interrupt_status.fetch_or(1, Ordering::Relaxed);
        Ok(())
    }

    /// Drain frames pushed into [`Self::pending_rx`] by the net-poll
    /// thread and write them into the guest's RX descriptors.
    ///
    /// Same descriptor-walking shape as [`Self::try_inject_rx`], but
    /// the input frames come from the lock-free SegQueue instead of
    /// going through the (locked) network backend.  The vCPU thread
    /// calls this on every MMIO entry to virtio-net, materialising any
    /// frames the net-poll thread queued since the last MMIO exit.
    ///
    /// Returns the number of frames written to the RX ring this call.
    pub fn flush_pending_rx<M: GuestMemory + ?Sized>(&mut self, mem: &M) -> Result<usize> {
        // Move the scratch out so we can mutate self while populating
        // it.  The post-write `clear()` keeps capacity, so subsequent
        // calls reuse the buffer instead of growing from cap=0.
        let mut frames = std::mem::take(&mut self.flush_scratch);
        frames.clear();
        while let Some(frame) = self.pending_rx.pop() {
            frames.push(frame);
        }
        let result = if !frames.is_empty() {
            self.write_frames_to_rx_ring(&mut frames, mem)
        } else {
            Ok(0)
        };
        frames.clear();
        self.flush_scratch = frames;
        result
    }

    /// Try to inject received frames from SLIRP into guest RX queue. Call from vCPU loop or after RX notify.
    ///
    /// Returns the number of frames the guest now has visible in its RX
    /// ring after this call.  Callers can use this to decide whether to
    /// raise an IRQ — pulsing the line is only useful when the guest
    /// has new work to do, not on every poll cycle while interrupt_status
    /// is still set from an earlier (un-acked) injection.
    pub fn try_inject_rx<M: GuestMemory + ?Sized>(&mut self, mem: &M) -> Result<usize> {
        let mut frames = self.get_rx_frames();
        if frames.is_empty() {
            return Ok(0);
        }
        let result = self.write_frames_to_rx_ring(&mut frames, mem);
        // Stash drained Vec back as scratch so the next call reuses
        // its capacity instead of allocating from cap=0.
        frames.clear();
        self.rx_scratch = frames;
        result
    }

    /// Write a batch of fully-formed frames (already including the
    /// virtio-net header) into the guest's RX descriptor ring.
    ///
    /// Shared between [`Self::try_inject_rx`] (frames pulled from the
    /// network backend) and [`Self::flush_pending_rx`] (frames pushed
    /// by the net-poll thread into the lock-free SegQueue).
    fn write_frames_to_rx_ring<M: GuestMemory + ?Sized>(
        &mut self,
        frames: &mut Vec<Vec<u8>>,
        mem: &M,
    ) -> Result<usize> {
        let q = &self.rx_queue;
        if !q.ready || q.num == 0 {
            // Queue not ready - buffer frames for later
            debug!(
                "virtio-net: RX queue not ready (ready={}, num={}), buffering {} frames",
                q.ready,
                q.num,
                frames.len()
            );
            self.buffer_rx_frames(frames.drain(..));
            return Ok(0);
        }
        // See `process_tx_queue` for why the rings are checked once, at full
        // size, before anything is read from them.
        let (desc_base, avail_base, used_base) = (q.desc_addr, q.driver_addr, q.device_addr);
        let queue_size = q.num as usize;
        let Some((desc_ring, avail_ring, used_ring)) =
            queue_rings(mem, desc_base, avail_base, used_base, queue_size)
        else {
            warn!("virtio-net: RX queue rings are not fully mapped; declining the queue");
            // Both callers clear the batch after this returns, so a frame not
            // buffered here is a frame dropped. The not-ready path above buffers
            // for the same reason.
            self.buffer_rx_frames(frames.drain(..));
            return Ok(0);
        };

        // avail_idx is monotonically increasing; the driver bumps it
        // whenever it adds new buffers.  Read it once per try_inject_rx
        // call rather than per frame — saves one mem.read per frame in
        // the hot path.  If the device runs out of available buffers
        // mid-batch the remaining frames are buffered for the next
        // call, which is the same correctness contract as before.
        let mut idx_buf = [0u8; 2];
        mem.read_slice(&mut idx_buf, GuestAddress(avail_ring.0 + 2))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let avail_idx = u16::from_le_bytes(idx_buf);

        let mut frames_injected: u16 = 0;
        // Drained explicitly rather than with a `for` over `drain(..)`: an abort
        // has to hand the current frame and the untouched remainder back to
        // `rx_buffer`, and dropping a `Drain` discards whatever it has not
        // yielded.
        let mut batch = frames.drain(..);
        let mut aborted = false;
        // A guest-memory access can fail for an address that is a perfectly
        // ordinary `u64` and simply unmapped, which `ring_addr` cannot see. That
        // is the reachable failure, so it aborts the batch the same way — frame
        // handed back, completions published — and the error surfaces after.
        let mut failure: Option<crate::Error> = None;
        // Counted and reported once per call: a guest can post an unusable chain
        // for every buffer in its ring.
        let mut undeliverable_frames = 0usize;

        for frame in batch.by_ref() {
            if self.rx_avail_idx == avail_idx {
                self.buffer_rx_frame(frame);
                continue;
            }

            let ring_offset = RING_HEADER_BYTES + ((self.rx_avail_idx as usize) % queue_size) * 2;
            let ring_entry_addr = GuestAddress(avail_ring.0 + ring_offset as u64);
            let mut desc_id_buf = [0u8; 2];
            if let Err(e) = mem.read_slice(&mut desc_id_buf, ring_entry_addr) {
                failure = Some(crate::Error::Memory(e.to_string()));
                self.buffer_rx_frame(frame);
                aborted = true;
                break;
            }
            let head_idx = u16::from_le_bytes(desc_id_buf) as usize;

            let used_ring_off =
                RING_HEADER_BYTES + ((self.rx_used_idx as usize) % queue_size) * USED_ELEM_BYTES;
            let used_elem_addr = GuestAddress(used_ring.0 + used_ring_off as u64);

            let mut next = head_idx;
            let mut walked = 0usize;
            let mut written = 0;
            let frame_len = frame.len();
            let mut frame_off = 0;
            // Set when the descriptor chain the guest posted cannot take this
            // frame. The frame is dropped rather than handed back: the chain is
            // the guest's, so retrying the same frame against the same chain
            // fails identically on every later call, and the backlog only grows.
            let mut undeliverable = false;

            loop {
                if next >= queue_size || frame_off >= frame_len {
                    break;
                }
                // Consuming the frame ends a well-formed chain, but a descriptor
                // with `len == 0` or `addr == 0` advances `frame_off` by nothing,
                // so a cycle among those never reaches that condition. The table
                // size bounds the walk regardless of what the chain describes.
                if walked >= queue_size {
                    warn!("virtio-net: RX chain longer than the {queue_size}-entry table (cycle?)");
                    break;
                }
                walked += 1;

                let desc_off = GuestAddress(desc_ring.0 + (next * DESC_BYTES) as u64);
                let mut desc = [0u8; DESC_BYTES];
                if mem.read_slice(&mut desc, desc_off).is_err() {
                    undeliverable = true;
                    break;
                }
                let addr = u64::from_le_bytes(desc[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(desc[8..12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(desc[12..14].try_into().unwrap());
                let next_desc = u16::from_le_bytes(desc[14..16].try_into().unwrap()) as usize;

                if len > 0 && addr != 0 {
                    let to_write = (len).min(frame_len - frame_off);
                    let dest = GuestAddress(addr);
                    if mem
                        .write_slice(&frame[frame_off..frame_off + to_write], dest)
                        .is_err()
                    {
                        undeliverable = true;
                        break;
                    }
                    written += to_write;
                    frame_off += to_write;
                }

                if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                    break;
                }
                next = next_desc;
            }

            if undeliverable {
                // The descriptor is retired with what was written, so the guest's
                // ring drains and the next frame gets a fresh chain.
                undeliverable_frames += 1;
            }

            // Used-ring entry is exactly 8 bytes (2x u32, little-endian).
            // Build it on the stack instead of allocating a Vec via
            // `[...].concat()` — the previous code did a heap alloc per
            // frame in the hot path.
            let mut used_elem = [0u8; USED_ELEM_BYTES];
            used_elem[0..4].copy_from_slice(&(head_idx as u32).to_le_bytes());
            used_elem[4..8].copy_from_slice(&(written as u32).to_le_bytes());
            if let Err(e) = mem.write_slice(&used_elem, used_elem_addr) {
                failure = Some(crate::Error::Memory(e.to_string()));
                self.buffer_rx_frame(frame);
                aborted = true;
                break;
            }

            self.rx_used_idx = self.rx_used_idx.wrapping_add(1);
            self.rx_avail_idx = self.rx_avail_idx.wrapping_add(1);
            frames_injected = frames_injected.wrapping_add(1);
        }

        if aborted {
            self.buffer_rx_frames(batch.by_ref());
        }
        drop(batch);

        if undeliverable_frames > 0 {
            warn!("virtio-net: dropped {undeliverable_frames} RX frame(s) the guest's chains could not take");
        }

        // Publish the new used.idx ONCE at the end of the batch.  The
        // virtio spec only requires the device to update used.idx after
        // it has written all corresponding used-ring entries; the guest
        // reads used.idx with a memory barrier and then iterates new
        // entries.  Per-frame writes are redundant — saves one
        // mem.write per frame on the hot path.
        if frames_injected > 0 {
            if let Some(used_idx_addr) = ring_addr(used_base, 2) {
                let used_idx_bytes = self.rx_used_idx.to_le_bytes();
                mem.write_slice(&used_idx_bytes, used_idx_addr)
                    .map_err(|e| crate::Error::Memory(e.to_string()))?;
            } else {
                warn!("virtio-net: RX used ring at {used_base:#x} overflows the address space");
            }
        }

        if frames_injected > 0 {
            self.interrupt_status.fetch_or(1, Ordering::Relaxed);
        }

        // Surfaced only after the completions are published and the batch is
        // back in `rx_buffer`: the frames already delivered are the guest's
        // regardless of what went wrong on the one that failed.
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(frames_injected as usize)
    }

    /// Hold a frame for a later call, dropping the oldest once the backlog is
    /// full.
    fn buffer_rx_frame(&mut self, frame: Vec<u8>) {
        if self.rx_buffer.len() >= MAX_RX_BUFFERED_FRAMES {
            self.rx_buffer.remove(0);
        }
        self.rx_buffer.push(frame);
    }

    /// Hold a batch of frames for a later call, under the same ceiling.
    fn buffer_rx_frames(&mut self, frames: impl IntoIterator<Item = Vec<u8>>) {
        for frame in frames {
            self.buffer_rx_frame(frame);
        }
    }

    /// Reset device to initial state
    fn reset(&mut self) {
        debug!("virtio-net: device reset");
        self.status = 0;
        self.interrupt_status.store(0, Ordering::Relaxed);
        self.driver_features = 0;
        self.tx_avail_idx = 0;
        self.tx_used_idx = 0;
        self.rx_avail_idx = 0;
        self.rx_used_idx = 0;
        self.rx_queue = QueueState {
            num_max: QUEUE_MAX_SIZE,
            ..Default::default()
        };
        self.tx_queue = QueueState {
            num_max: QUEUE_MAX_SIZE,
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
        // Drain backend frames into the reused scratch buffer.
        self.rx_scratch.clear();
        {
            let mut backend = self.slirp.lock().unwrap();
            backend.drain_to_guest(&mut self.rx_scratch);
        }
        let frames = std::mem::take(&mut self.rx_scratch);

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
        self.buffer_rx_frame(packet);

        // Set interrupt
        self.interrupt_status.fetch_or(1, Ordering::Relaxed);
    }

    /// Capture device state for snapshot.
    pub fn snapshot_state(&self) -> crate::vmm::snapshot::NetSnapshotState {
        use crate::vmm::snapshot::{NetSnapshotState, QueueSnapshotState};

        let queues = [&self.rx_queue, &self.tx_queue]
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let (last_avail_idx, last_used_idx) = if i == 0 {
                    (Some(self.rx_avail_idx), Some(self.rx_used_idx))
                } else {
                    (Some(self.tx_avail_idx), Some(self.tx_used_idx))
                };
                QueueSnapshotState {
                    num_max: q.num_max,
                    num: q.num,
                    ready: q.ready,
                    desc_addr: q.desc_addr,
                    driver_addr: q.driver_addr,
                    device_addr: q.device_addr,
                    last_avail_idx,
                    last_used_idx,
                }
            })
            .collect();

        NetSnapshotState {
            device_features: self.device_features,
            driver_features: self.driver_features,
            features_sel: self.features_sel,
            queue_sel: self.queue_sel,
            status: self.status,
            interrupt_status: self.interrupt_status.load(Ordering::Relaxed),
            config_generation: self.config_generation,
            mac: self.mac,
            queues,
        }
    }

    /// Restore device state from a snapshot onto a freshly created device.
    pub fn restore_state(&mut self, state: &crate::vmm::snapshot::NetSnapshotState) {
        self.device_features = state.device_features;
        self.driver_features = state.driver_features;
        self.features_sel = state.features_sel;
        self.queue_sel = state.queue_sel;
        self.status = state.status;
        self.interrupt_status
            .store(state.interrupt_status, Ordering::Relaxed);
        self.config_generation = state.config_generation;
        self.mac = state.mac;

        if let Some(q) = state.queues.first() {
            self.rx_queue = QueueState {
                // A snapshot is a file, so its queue geometry is not the device's
                // own. Both fields are held to the advertised ceiling here, the
                // same one an MMIO write is held to.
                num_max: q.num_max.min(QUEUE_MAX_SIZE),
                num: q.num.min(q.num_max).min(QUEUE_MAX_SIZE),
                ready: q.ready,
                desc_addr: q.desc_addr,
                driver_addr: q.driver_addr,
                device_addr: q.device_addr,
            };
            self.rx_avail_idx = q.last_avail_idx.unwrap_or(0);
            self.rx_used_idx = q.last_used_idx.unwrap_or(0);
        }
        if let Some(q) = state.queues.get(1) {
            self.tx_queue = QueueState {
                num_max: q.num_max.min(QUEUE_MAX_SIZE),
                num: q.num.min(q.num_max).min(QUEUE_MAX_SIZE),
                ready: q.ready,
                desc_addr: q.desc_addr,
                driver_addr: q.driver_addr,
                device_addr: q.device_addr,
            };
            self.tx_avail_idx = q.last_avail_idx.unwrap_or(0);
            self.tx_used_idx = q.last_used_idx.unwrap_or(0);
        }

        debug!(
            "Restored virtio-net state: status={:#x}, features={:#x}, mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.status, self.driver_features,
            self.mac[0], self.mac[1], self.mac[2], self.mac[3], self.mac[4], self.mac[5],
        );
    }

    /// Check if there are pending interrupts.
    ///
    /// Atomic load — safe to call from any thread without holding the
    /// device mutex.  The net-poll thread uses this to decide whether
    /// to pulse the IRQ line.
    pub fn has_pending_interrupt(&self) -> bool {
        self.interrupt_status.load(Ordering::Relaxed) != 0
    }

    /// Get the MAC address
    pub fn mac(&self) -> &[u8; 6] {
        &self.mac
    }

    /// Return the epoll dispatch instance from the underlying network backend,
    /// if the backend is a `SlirpBackend` (Linux only).
    ///
    /// `net_poll_thread` uses this to block on `epoll_wait` instead of
    /// sleeping, waking immediately when host sockets become readable.
    #[cfg(target_os = "linux")]
    pub fn epoll_arc(
        &self,
    ) -> Option<std::sync::Arc<crate::network::epoll_dispatch::EpollDispatch>> {
        let backend = self.slirp.lock().unwrap();
        backend.epoll_arc()
    }

    /// Forward ready epoll events into the network backend's per-tick queue.
    ///
    /// Called by net_poll_thread after each epoll_wait returns so that
    /// drain_to_guest can process events without re-locking EpollDispatch.
    #[cfg(target_os = "linux")]
    pub fn push_events_to_backend(&self, events: &[crate::network::epoll_dispatch::EpollEvent]) {
        let backend = self.slirp.lock().unwrap();
        backend.push_ready_events(events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::slirp::SlirpBackend;
    use vm_memory::GuestMemoryMmap;

    const TEST_MEM_BYTES: usize = 256 * 1024;
    const DESC_TABLE: u64 = 0x1000;
    const AVAIL_RING: u64 = 0x4000;
    const USED_RING: u64 = 0x5000;
    const DATA_BUFFER: u64 = 0x8000;

    fn test_device() -> VirtioNetDevice {
        let slirp: Arc<Mutex<dyn NetworkBackend>> =
            Arc::new(Mutex::new(SlirpBackend::new().unwrap()));
        VirtioNetDevice::new(slirp).unwrap()
    }

    /// Records what the device transmits, so a test can assert on the frame
    /// itself rather than on the completion that follows it.
    struct RecordingBackend {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl NetworkBackend for RecordingBackend {
        fn process_guest_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
            self.sent.lock().unwrap().push(frame.to_vec());
            Ok(())
        }

        fn drain_to_guest(&mut self, _out: &mut Vec<Vec<u8>>) {}
    }

    /// A device whose transmitted frames are captured in the returned handle.
    fn recording_device() -> (VirtioNetDevice, Arc<Mutex<Vec<Vec<u8>>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let backend = RecordingBackend { sent: sent.clone() };
        let backend: Arc<Mutex<dyn NetworkBackend>> = Arc::new(Mutex::new(backend));
        (VirtioNetDevice::new(backend).unwrap(), sent)
    }

    fn test_memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), TEST_MEM_BYTES)]).unwrap()
    }

    /// Write one 16-byte descriptor into the table at `index`.
    fn write_desc(mem: &GuestMemoryMmap, index: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let at = GuestAddress(DESC_TABLE + index * 16);
        mem.write_obj(addr, at).unwrap();
        mem.write_obj(len, GuestAddress(at.0 + 8)).unwrap();
        mem.write_obj(flags, GuestAddress(at.0 + 12)).unwrap();
        mem.write_obj(next, GuestAddress(at.0 + 14)).unwrap();
    }

    /// Publish one chain head on the available ring.
    fn publish_head(mem: &GuestMemoryMmap, head: u16) {
        mem.write_obj(1u16, GuestAddress(AVAIL_RING + 2)).unwrap();
        mem.write_obj(head, GuestAddress(AVAIL_RING + 4)).unwrap();
    }

    fn program_queue(queue: &mut QueueState, num: u16) {
        queue.num = num;
        queue.ready = true;
        queue.desc_addr = DESC_TABLE;
        queue.driver_addr = AVAIL_RING;
        queue.device_addr = USED_RING;
    }

    /// A descriptor whose NEXT points back at itself used to spin forever while
    /// the frame buffer grew by that descriptor's guest-written length on every
    /// pass. A regression in the bound does not fail — it hangs until the
    /// harness timeout; the assertion on what was sent is what fails if the
    /// cycle is bounded but its output still goes out.
    #[test]
    fn tx_self_referential_chain_terminates_without_transmitting() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        write_desc(&mem, 0, DATA_BUFFER, 0x1000, VIRTQ_DESC_F_NEXT, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert_eq!(
            device.tx_used_idx, 1,
            "the chain is completed rather than walked forever"
        );
        assert!(
            sent.lock().unwrap().is_empty(),
            "a cycle's worth of one buffer repeated is not a frame the guest described"
        );
    }

    /// A cycle short enough to stay under the frame ceiling is the case the
    /// ceiling cannot catch: every read is a full `len`, so a size-based check
    /// never fires while the walk still assembles one buffer repeated
    /// `queue_size` times.
    #[test]
    fn a_bounded_cycle_under_the_frame_ceiling_is_still_not_a_frame() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        // 256 x 256 bytes = 65536, just under MAX_TX_FRAME_BYTES.
        let queue_size = 256u16;
        write_desc(&mem, 0, DATA_BUFFER, 256, VIRTQ_DESC_F_NEXT, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, queue_size);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert!(
            sent.lock().unwrap().is_empty(),
            "the assembled bytes stay under the ceiling, so only the walk's exit can reject them"
        );
        assert_eq!(device.tx_used_idx, 1, "the descriptor is still returned");
    }

    /// A NEXT pointing outside the descriptor table ends the walk early. What
    /// was assembled is a prefix of the chain the guest described, not the frame.
    #[test]
    fn a_dangling_next_is_not_transmitted_as_a_short_frame() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        write_desc(&mem, 0, DATA_BUFFER, 128, VIRTQ_DESC_F_NEXT, 9999);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert!(
            sent.lock().unwrap().is_empty(),
            "a chain that cannot be walked to its end is not a frame"
        );
        assert_eq!(device.tx_used_idx, 1, "the descriptor is still returned");
    }

    /// A used-ring address can be an ordinary `u64` and simply unmapped, which
    /// checked arithmetic cannot see. Transmitting first and discovering the
    /// completion is unwritable afterwards leaves the descriptor unretired, so
    /// every later kick resends the same frame.
    #[test]
    fn a_frame_is_not_transmitted_when_its_completion_is_unwritable() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        let payload_len = 512u32;
        write_desc(&mem, 0, DATA_BUFFER, payload_len, 0, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);
        // Well past the mapped region, but nowhere near overflowing.
        device.tx_queue.device_addr = (TEST_MEM_BYTES as u64) * 4;

        device
            .process_tx_queue(&mem)
            .expect("the batch is deferred");

        assert!(
            sent.lock().unwrap().is_empty(),
            "nothing goes out when the completion cannot be written"
        );
        assert_eq!(device.tx_used_idx, 0, "no completion is claimed");
        assert_eq!(device.tx_avail_idx, 0, "the entry stays for a later kick");
    }

    /// A descriptor pointing at guest address 0 describes bytes and contributes
    /// none, so a size ceiling measured on the assembled buffer never sees the
    /// chain grow. What the guest described and what the device assembled have to
    /// agree.
    #[test]
    fn a_chain_describing_bytes_it_does_not_supply_is_not_a_frame() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        write_desc(&mem, 0, 0, 4096, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&mem, 1, DATA_BUFFER, 4096, 0, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert!(
            sent.lock().unwrap().is_empty(),
            "half of a chain is not the frame the guest described"
        );
        assert_eq!(device.tx_used_idx, 1, "the descriptor is still returned");
    }

    /// The ceiling bounds the allocation inside the walk, which the
    /// described-vs-assembled check cannot: that check runs after the buffer has
    /// already grown. Pinning it needs a chain whose descriptors are backed by
    /// real memory, so that removing the ceiling lets the reads succeed and the
    /// oversized frame go out — against a small region the reads fail instead and
    /// the chain is rejected for the wrong reason.
    #[test]
    fn a_chain_over_the_ceiling_is_dropped_even_when_its_memory_is_backed() {
        let (mut device, sent) = recording_device();
        // Room for two full-ceiling segments plus the rings.
        let mem_bytes = MAX_TX_FRAME_BYTES * 2 + 0x2_0000;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_bytes)]).unwrap();

        let segment = MAX_TX_FRAME_BYTES;
        let first = 0x1_0000u64;
        let second = first + segment as u64;
        write_desc(&mem, 0, first, segment as u32, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&mem, 1, second, segment as u32, 0, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert!(
            sent.lock().unwrap().is_empty(),
            "a chain describing twice the ceiling must not be assembled and sent"
        );
        assert_eq!(device.tx_used_idx, 1, "the descriptor is still returned");
    }

    /// The shape a Linux driver emits whenever it cannot push the header into
    /// the first buffer: a header descriptor chained to a payload descriptor.
    /// Every other multi-descriptor test here is a chain that must be rejected,
    /// so without this nothing shows that a legitimate chained frame is accepted
    /// — which is what the described-vs-assembled check would break first if it
    /// were too strict.
    #[test]
    fn a_conforming_chained_tx_frame_is_transmitted_whole() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        let header = vec![0u8; VirtioNetHeader::SIZE];
        let payload: Vec<u8> = (0..1514u32).map(|i| (i % 251) as u8).collect();
        mem.write_slice(&header, GuestAddress(DATA_BUFFER)).unwrap();
        mem.write_slice(&payload, GuestAddress(DATA_BUFFER + 0x1000))
            .unwrap();

        write_desc(
            &mem,
            0,
            DATA_BUFFER,
            header.len() as u32,
            VIRTQ_DESC_F_NEXT,
            1,
        );
        write_desc(&mem, 1, DATA_BUFFER + 0x1000, payload.len() as u32, 0, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "a chained frame is transmitted, not dropped");
        assert_eq!(sent[0], payload, "and it arrives whole");
    }

    /// The address the ring check exists for: an ordinary `u64`, simply not
    /// backed by guest memory. Checked arithmetic accepts it, so only a mapping
    /// check declines the queue.
    #[test]
    fn rx_frames_survive_a_ring_that_is_unmapped_rather_than_overflowing() {
        let mut device = test_device();
        let mem = test_memory();

        program_queue(&mut device.rx_queue, 4);
        device.rx_queue.device_addr = (TEST_MEM_BYTES as u64) * 4;
        publish_head(&mem, 0);
        write_desc(&mem, 0, DATA_BUFFER, 2048, 0, 0);

        let original = vec![vec![0xAAu8; 64], vec![0xBBu8; 32]];
        let mut batch = original.clone();
        let injected = device
            .write_frames_to_rx_ring(&mut batch, &mem)
            .expect("the ring is declined");

        assert_eq!(injected, 0);
        assert_eq!(device.rx_buffer, original, "every frame is kept, in order");
    }

    /// The backlog a guest can force the device to hold has a ceiling: frames it
    /// cannot deliver are kept rather than dropped, so without one an idle RX
    /// ring turns inbound traffic into unbounded host memory.
    #[test]
    fn the_rx_backlog_has_a_ceiling_and_sheds_the_oldest() {
        let mut device = test_device();
        let mem = test_memory();
        // Not ready, so every frame takes the buffering path the device uses when
        // it cannot deliver — driving the call sites rather than the helper.
        device.rx_queue.ready = false;

        let overflow = 64usize;
        for i in 0..(MAX_RX_BUFFERED_FRAMES + overflow) {
            let mut batch = vec![vec![(i % 251) as u8; 8]];
            device
                .write_frames_to_rx_ring(&mut batch, &mem)
                .expect("frames are buffered");
        }

        assert_eq!(device.rx_buffer.len(), MAX_RX_BUFFERED_FRAMES);
        assert_eq!(
            device.rx_buffer[0],
            vec![(overflow % 251) as u8; 8],
            "the oldest frame is the one shed, as a full adapter ring does"
        );
    }

    /// The RX walk ends when the frame is consumed, so a cycle only runs forever
    /// through descriptors that consume nothing: `len == 0` here.
    #[test]
    fn rx_zero_length_cycle_terminates() {
        let mut device = test_device();
        let mem = test_memory();

        write_desc(&mem, 0, DATA_BUFFER, 0, VIRTQ_DESC_F_NEXT, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.rx_queue, 4);

        let injected = device
            .write_frames_to_rx_ring(&mut vec![vec![0xAAu8; 64]], &mem)
            .expect("RX walk returns");

        assert_eq!(injected, 1, "the frame is completed rather than retried");
    }

    /// Every descriptor length is a guest-written `u32` and the chain sizes a
    /// host allocation, so a chain describing more than one frame must not be
    /// assembled — and the prefix that was assembled must not go out as if it
    /// were the frame.
    #[test]
    fn an_oversized_tx_chain_is_dropped_rather_than_truncated() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        // Four descriptors, each claiming the whole 4 GiB a u32 can express.
        for index in 0..4u64 {
            let last = index == 3;
            write_desc(
                &mem,
                index,
                DATA_BUFFER,
                u32::MAX,
                if last { 0 } else { VIRTQ_DESC_F_NEXT },
                (index + 1) as u16,
            );
        }
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert!(
            sent.lock().unwrap().is_empty(),
            "an over-ceiling chain must not put a truncated frame on the wire"
        );
        assert_eq!(
            device.tx_used_idx, 1,
            "the descriptor is still returned to the guest"
        );
    }

    /// A frame within the ceiling is transmitted whole — the cap must not cost
    /// the ordinary path anything.
    #[test]
    fn a_conforming_tx_frame_is_transmitted_intact() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        let payload_len = 1514u32;
        let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
        mem.write_slice(&payload, GuestAddress(DATA_BUFFER))
            .unwrap();

        write_desc(&mem, 0, DATA_BUFFER, payload_len, 0, 0);
        publish_head(&mem, 0);
        program_queue(&mut device.tx_queue, 4);

        device.process_tx_queue(&mem).expect("TX walk returns");

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "the frame is transmitted");
        // `process_tx_frame` strips the virtio-net header before handing the
        // frame to the backend.
        assert_eq!(sent[0].len(), payload_len as usize - VirtioNetHeader::SIZE);
        assert_eq!(sent[0][..], payload[VirtioNetHeader::SIZE..]);
    }

    /// One kick must not be able to name more buffers than the queue can hold:
    /// the 16-bit index difference alone admits 65535 chains, each assembling a
    /// full frame.
    #[test]
    fn one_kick_processes_at_most_a_queues_worth_of_chains() {
        let (mut device, sent) = recording_device();
        let mem = test_memory();

        let queue_size = 4u16;
        write_desc(&mem, 0, DATA_BUFFER, 64, 0, 0);
        for slot in 0..queue_size {
            mem.write_obj(0u16, GuestAddress(AVAIL_RING + 4 + u64::from(slot) * 2))
                .unwrap();
        }
        // The guest claims far more outstanding buffers than the queue holds.
        mem.write_obj(u16::MAX, GuestAddress(AVAIL_RING + 2))
            .unwrap();
        program_queue(&mut device.tx_queue, queue_size);

        device.process_tx_queue(&mem).expect("TX walk returns");

        assert_eq!(
            sent.lock().unwrap().len(),
            queue_size as usize,
            "the batch is bounded by the queue size, not by the guest's index"
        );
    }

    /// Ring addresses come from unvalidated MMIO writes, so a base near the top
    /// of the address space is reachable and the offset arithmetic must reject it
    /// rather than overflow.
    #[test]
    fn ring_addresses_near_the_top_of_memory_do_not_overflow() {
        let mut device = test_device();
        let mem = test_memory();

        for queue in [&mut device.tx_queue, &mut device.rx_queue] {
            queue.num = 256;
            queue.ready = true;
            queue.desc_addr = u64::MAX;
            queue.driver_addr = u64::MAX;
            queue.device_addr = u64::MAX;
        }

        device
            .process_tx_queue(&mem)
            .expect("TX declines the queue");
        let injected = device
            .write_frames_to_rx_ring(&mut vec![vec![0xAAu8; 64]], &mem)
            .expect("RX declines the queue");
        assert_eq!(injected, 0);
    }

    /// A ring the guest programmed unusably costs it the batch, but the frames
    /// are handed back rather than dropped: both callers clear their batch after
    /// the call, so a frame this function does not keep is a frame gone.
    #[test]
    fn rx_frames_survive_an_unusable_ring() {
        let mut device = test_device();
        let mem = test_memory();

        program_queue(&mut device.rx_queue, 4);
        device.rx_queue.device_addr = u64::MAX;
        publish_head(&mem, 0);
        write_desc(&mem, 0, DATA_BUFFER, 2048, 0, 0);

        let original = vec![vec![0xAAu8; 64], vec![0xBBu8; 32], vec![0xCCu8; 16]];
        let mut batch = original.clone();
        let injected = device
            .write_frames_to_rx_ring(&mut batch, &mem)
            .expect("the ring is declined");

        assert_eq!(injected, 0, "nothing is completed against an unusable ring");
        // Compared by content and in order: a count alone passes an
        // implementation that drops one frame and buffers another twice.
        assert_eq!(
            device.rx_buffer, original,
            "every frame is kept once, in the order it arrived"
        );
    }

    /// `QueueNum` is an MMIO register the guest writes with no validation, and
    /// both descriptor walks derive their chain bound from it.
    #[test]
    fn queue_num_is_clamped_to_the_advertised_maximum() {
        let mut device = test_device();

        let no_mem: Option<&GuestMemoryMmap> = None;
        device.mmio_write(mmio::QUEUE_SEL, &0u32.to_le_bytes(), no_mem);
        device.mmio_write(mmio::QUEUE_NUM, &u32::MAX.to_le_bytes(), no_mem);

        assert_eq!(device.rx_queue.num, device.rx_queue.num_max);
    }

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
        let slirp: Arc<Mutex<dyn NetworkBackend>> =
            Arc::new(Mutex::new(SlirpBackend::new().unwrap()));
        let device = VirtioNetDevice::new(slirp).unwrap();

        let mut data = [0u8; 4];
        device.mmio_read(mmio::MAGIC_VALUE, &mut data);
        let magic = u32::from_le_bytes(data);
        assert_eq!(magic, mmio::MAGIC);
    }

    #[test]
    fn test_mmio_version() {
        let slirp: Arc<Mutex<dyn NetworkBackend>> =
            Arc::new(Mutex::new(SlirpBackend::new().unwrap()));
        let device = VirtioNetDevice::new(slirp).unwrap();

        let mut data = [0u8; 4];
        device.mmio_read(mmio::VERSION, &mut data);
        let version = u32::from_le_bytes(data);
        assert_eq!(version, mmio::VERSION_2);
    }

    #[test]
    fn test_device_type() {
        let slirp: Arc<Mutex<dyn NetworkBackend>> =
            Arc::new(Mutex::new(SlirpBackend::new().unwrap()));
        let device = VirtioNetDevice::new(slirp).unwrap();

        let mut data = [0u8; 4];
        device.mmio_read(mmio::DEVICE_ID, &mut data);
        let device_id = u32::from_le_bytes(data);
        assert_eq!(device_id, VIRTIO_NET_DEVICE_TYPE);
    }
}
