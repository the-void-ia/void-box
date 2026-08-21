//! Split virtqueue implementation for userspace virtio devices.
//!
//! Pure data-structure module for reading/writing split virtqueues from
//! guest memory. No device logic — used by the userspace vsock backend.

use std::os::unix::io::RawFd;

use serde::{Deserialize, Serialize};
use tracing::{debug, trace, warn};
use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

/// A single descriptor in a virtio split virtqueue.
#[derive(Debug, Clone, Copy)]
pub struct VirtqDesc {
    /// Guest physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Descriptor flags (NEXT, WRITE, INDIRECT).
    pub flags: u16,
    /// Index of the next descriptor if NEXT flag is set.
    pub next: u16,
}

/// Flag: descriptor continues via `next` field.
pub const VRING_DESC_F_NEXT: u16 = 1;
/// Flag: buffer is device-writable (for RX).
pub const VRING_DESC_F_WRITE: u16 = 2;

/// Snapshot-friendly state for a split virtqueue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtqueueSnapshot {
    pub last_avail_idx: u16,
    pub last_used_idx: u16,
}

/// A split virtqueue that reads/writes guest memory directly.
pub struct SplitVirtqueue {
    /// Maximum queue size (negotiated with guest).
    pub num: u16,
    /// Guest physical address of the descriptor table.
    pub desc_table_addr: u64,
    /// Guest physical address of the available ring.
    pub avail_ring_addr: u64,
    /// Guest physical address of the used ring.
    pub used_ring_addr: u64,
    /// Host-side index tracking which avail entries we've consumed.
    pub last_avail_idx: u16,
    /// Host-side index tracking which used entries we've produced.
    pub last_used_idx: u16,
    /// Eventfd the guest writes to notify us (kick).
    pub kick_fd: RawFd,
    /// Eventfd we write to notify the guest (call/interrupt).
    pub call_fd: RawFd,
}

/// A guest ring address plus an offset into it, or `None` when the sum leaves
/// the 64-bit address space.
///
/// Shared with the 9P device, which walks its own queue: `vm_memory`'s
/// `unchecked_add` is a plain `+`, so it panics under `overflow-checks` rather
/// than wrapping, and both walkers take their base from the same kind of
/// unvalidated guest register.
///
/// The base comes from an MMIO register the guest writes with no validation, so
/// a base near `u64::MAX` is reachable. Plain `+` panics there under
/// `overflow-checks`, which turns a guest register write into a host VMM crash;
/// wrapping instead would fold a wild address back onto memory that is mapped.
/// Rejecting the operation leaves the guest with an unserviced queue, which is
/// the consequence of the address it programmed.
pub(crate) fn ring_addr(base: u64, offset: u64) -> Option<GuestAddress> {
    base.checked_add(offset).map(GuestAddress)
}

/// A guest ring base, once the whole ring is known to fit in the address space
/// and to be backed by mapped guest memory.
///
/// Checking each element as it is touched leaves the rest of the ring unchecked,
/// and it checks the same ring once per element. A queue whose rings are not
/// fully mapped is one the guest cannot operate, so the device declines it as a
/// whole and does so once.
pub(crate) fn mapped_ring<M: GuestMemory + ?Sized>(
    mem: &M,
    base: u64,
    len: usize,
) -> Option<GuestAddress> {
    // The sum has to fit before the range can mean anything.
    ring_addr(base, len as u64)?;
    let start = GuestAddress(base);
    mem.check_range(start, len).then_some(start)
}

/// A chain of descriptors popped from the available ring.
pub struct DescriptorChain {
    /// Index of the head descriptor (needed for push_used).
    pub head_index: u16,
    /// The descriptors in order.
    pub descriptors: Vec<VirtqDesc>,
}

impl SplitVirtqueue {
    /// Create a new split virtqueue with the given parameters.
    pub fn new(
        num: u16,
        desc_table_addr: u64,
        avail_ring_addr: u64,
        used_ring_addr: u64,
        kick_fd: RawFd,
        call_fd: RawFd,
    ) -> Self {
        Self {
            num,
            desc_table_addr,
            avail_ring_addr,
            used_ring_addr,
            last_avail_idx: 0,
            last_used_idx: 0,
            kick_fd,
            call_fd,
        }
    }

    /// Whether the guest has configured this queue with a usable size.
    ///
    /// `num` is whatever the guest last wrote to the `QueueNum` MMIO register,
    /// and a device activates a queue as soon as `QueueReady` is set — neither
    /// step validates the value. Zero is therefore reachable from the guest, and
    /// every ring index here is computed modulo `num`, so the ring math must not
    /// run until this holds.
    fn is_configured(&self) -> bool {
        self.num != 0
    }

    /// Check if there are available descriptors to process.
    pub fn has_avail(&self, mem: &GuestMemoryMmap) -> bool {
        if !self.is_configured() {
            return false;
        }
        // avail->idx is at avail_ring_addr + 2
        let Some(idx_addr) = ring_addr(self.avail_ring_addr, 2) else {
            return false;
        };
        let avail_idx: u16 = mem.read_obj(idx_addr).unwrap_or(self.last_avail_idx);
        avail_idx != self.last_avail_idx
    }

    /// Pop the next available descriptor chain from the queue.
    ///
    /// Returns `None` if no descriptors are available, if the guest has not
    /// configured a queue size, or if the ring addresses it programmed do not
    /// address readable memory.
    pub fn pop_avail(&mut self, mem: &GuestMemoryMmap) -> Option<DescriptorChain> {
        if !self.is_configured() {
            warn!("virtqueue: pop on a queue whose size the guest left at 0");
            return None;
        }

        // Read avail->idx (u16 at offset 2 in the available ring)
        let avail_idx: u16 = mem.read_obj(ring_addr(self.avail_ring_addr, 2)?).ok()?;

        if avail_idx == self.last_avail_idx {
            return None;
        }

        // Read the descriptor index from avail->ring[last_avail_idx % num]
        let ring_offset = 4 + (self.last_avail_idx % self.num) as u64 * 2;
        let head_index: u16 = mem
            .read_obj(ring_addr(self.avail_ring_addr, ring_offset)?)
            .ok()?;

        // Walk the descriptor chain
        let mut descriptors = Vec::new();
        let mut idx = head_index;
        let mut count = 0u16;

        loop {
            if count >= self.num {
                warn!("virtqueue: descriptor chain too long (loop?)");
                break;
            }
            // A descriptor index at or past the queue size is outside the table
            // the guest allocated. Reading there would interpret whatever
            // happens to follow it as a descriptor.
            if idx >= self.num {
                warn!(
                    "virtqueue: descriptor index {idx} is outside the {}-entry table",
                    self.num
                );
                break;
            }

            // `idx as u64 * 16` cannot overflow: a u16 index times 16 fits u64.
            let desc_base = self.desc_table_addr.checked_add(idx as u64 * 16)?;
            let addr: u64 = mem.read_obj(ring_addr(desc_base, 0)?).ok()?;
            let len: u32 = mem.read_obj(ring_addr(desc_base, 8)?).ok()?;
            let flags: u16 = mem.read_obj(ring_addr(desc_base, 12)?).ok()?;
            let next: u16 = mem.read_obj(ring_addr(desc_base, 14)?).ok()?;

            descriptors.push(VirtqDesc {
                addr,
                len,
                flags,
                next,
            });
            count += 1;

            if flags & VRING_DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }

        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        Some(DescriptorChain {
            head_index,
            descriptors,
        })
    }

    /// Push a used descriptor back to the used ring.
    ///
    /// `head_index` is the descriptor chain head from `pop_avail`.
    /// `len` is the number of bytes written to the descriptor chain.
    ///
    /// A queue the guest left unconfigured, or a used-ring address that does not
    /// address writable memory, drops the completion: the guest gets no used
    /// entry for a request it made with a ring it never set up.
    pub fn push_used(&mut self, mem: &GuestMemoryMmap, head_index: u16, len: u32) {
        if !self.is_configured() {
            warn!("virtqueue: used-ring push on a queue whose size the guest left at 0");
            return;
        }

        // used->ring[last_used_idx % num] = { id: head_index, len }
        let ring_offset = 4 + (self.last_used_idx % self.num) as u64 * 8;
        let (Some(used_elem_addr), Some(used_len_addr), Some(used_idx_addr)) = (
            ring_addr(self.used_ring_addr, ring_offset),
            ring_addr(self.used_ring_addr, ring_offset + 4),
            ring_addr(self.used_ring_addr, 2),
        ) else {
            warn!(
                "virtqueue: used ring at {:#x} overflows the address space",
                self.used_ring_addr
            );
            return;
        };

        // Write used element (id: u32, len: u32)
        let _ = mem.write_obj(head_index as u32, used_elem_addr);
        let _ = mem.write_obj(len, used_len_addr);

        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        // Update used->idx
        let _ = mem.write_obj(self.last_used_idx, used_idx_addr);

        trace!(
            "virtqueue: pushed used head={} len={} used_idx={}",
            head_index,
            len,
            self.last_used_idx
        );
    }

    /// Signal the guest by writing to the call eventfd.
    pub fn signal_guest(&self) {
        let val: u64 = 1;
        let _ = unsafe {
            libc::write(
                self.call_fd,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// Capture queue state for snapshot.
    pub fn snapshot(&self) -> VirtqueueSnapshot {
        VirtqueueSnapshot {
            last_avail_idx: self.last_avail_idx,
            last_used_idx: self.last_used_idx,
        }
    }

    /// Restore queue state from snapshot.
    pub fn restore(&mut self, snap: &VirtqueueSnapshot) {
        self.last_avail_idx = snap.last_avail_idx;
        self.last_used_idx = snap.last_used_idx;
        debug!(
            "virtqueue restored: last_avail={} last_used={}",
            self.last_avail_idx, self.last_used_idx
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    fn setup_test_memory() -> GuestMemoryMmap {
        // 1 MB of guest memory
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap()
    }

    #[test]
    fn test_empty_queue_has_no_avail() {
        let mem = setup_test_memory();
        let vq = SplitVirtqueue::new(256, 0x1000, 0x2000, 0x3000, -1, -1);
        assert!(!vq.has_avail(&mem));
    }

    #[test]
    fn test_pop_avail_empty() {
        let mem = setup_test_memory();
        let mut vq = SplitVirtqueue::new(256, 0x1000, 0x2000, 0x3000, -1, -1);
        assert!(vq.pop_avail(&mem).is_none());
    }

    #[test]
    fn test_pop_avail_single_descriptor() {
        let mem = setup_test_memory();
        let desc_table = 0x1000u64;
        let avail_ring = 0x2000u64;
        let used_ring = 0x3000u64;

        // Set up descriptor 0: addr=0x4000, len=64, flags=0 (no NEXT), next=0
        mem.write_obj(0x4000u64, GuestAddress(desc_table)).unwrap(); // addr
        mem.write_obj(64u32, GuestAddress(desc_table + 8)).unwrap(); // len
        mem.write_obj(0u16, GuestAddress(desc_table + 12)).unwrap(); // flags
        mem.write_obj(0u16, GuestAddress(desc_table + 14)).unwrap(); // next

        // Set up avail ring: flags=0, idx=1, ring[0]=0
        mem.write_obj(0u16, GuestAddress(avail_ring)).unwrap(); // flags
        mem.write_obj(1u16, GuestAddress(avail_ring + 2)).unwrap(); // idx
        mem.write_obj(0u16, GuestAddress(avail_ring + 4)).unwrap(); // ring[0]

        let mut vq = SplitVirtqueue::new(256, desc_table, avail_ring, used_ring, -1, -1);
        assert!(vq.has_avail(&mem));

        let chain = vq.pop_avail(&mem).unwrap();
        assert_eq!(chain.head_index, 0);
        assert_eq!(chain.descriptors.len(), 1);
        assert_eq!(chain.descriptors[0].addr, 0x4000);
        assert_eq!(chain.descriptors[0].len, 64);

        assert!(!vq.has_avail(&mem));
    }

    #[test]
    fn test_push_used() {
        let mem = setup_test_memory();
        let used_ring = 0x3000u64;

        let mut vq = SplitVirtqueue::new(256, 0x1000, 0x2000, used_ring, -1, -1);
        vq.push_used(&mem, 5, 128);

        // Check used->idx = 1
        let used_idx: u16 = mem.read_obj(GuestAddress(used_ring + 2)).unwrap();
        assert_eq!(used_idx, 1);

        // Check used->ring[0] = { id: 5, len: 128 }
        let id: u32 = mem.read_obj(GuestAddress(used_ring + 4)).unwrap();
        let len: u32 = mem.read_obj(GuestAddress(used_ring + 8)).unwrap();
        assert_eq!(id, 5);
        assert_eq!(len, 128);
    }

    /// A guest that sets `QueueReady` without ever writing `QueueNum` leaves
    /// `num` at 0. Every ring index is computed modulo `num`, so this used to
    /// panic the VMM process on the first kick.
    #[test]
    fn zero_queue_size_is_inert_rather_than_fatal() {
        let mem = setup_test_memory();
        let avail_ring = 0x2000u64;
        // A non-zero avail->idx, so the "nothing available" early return cannot
        // be what saves us.
        mem.write_obj(1u16, GuestAddress(avail_ring + 2)).unwrap();

        let mut vq = SplitVirtqueue::new(0, 0x1000, avail_ring, 0x3000, -1, -1);
        assert!(!vq.has_avail(&mem));
        assert!(vq.pop_avail(&mem).is_none());
        vq.push_used(&mem, 0, 0);
        assert_eq!(
            vq.last_used_idx, 0,
            "no used entry for an unconfigured queue"
        );
    }

    /// Ring addresses come from unvalidated MMIO writes, so a base near the top
    /// of the address space is reachable. The offset arithmetic must reject it
    /// rather than overflow.
    #[test]
    fn ring_addresses_near_the_top_of_memory_do_not_overflow() {
        let mem = setup_test_memory();
        let mut vq = SplitVirtqueue::new(256, u64::MAX, u64::MAX, u64::MAX, -1, -1);
        assert!(!vq.has_avail(&mem));
        assert!(vq.pop_avail(&mem).is_none());
        vq.push_used(&mem, 0, 0);
        assert_eq!(vq.last_used_idx, 0);
    }

    /// A descriptor index at or past the queue size points outside the table the
    /// guest allocated; following it would read whatever happens to sit there.
    #[test]
    fn descriptor_index_past_the_queue_size_ends_the_walk() {
        let mem = setup_test_memory();
        let desc_table = 0x1000u64;
        let avail_ring = 0x2000u64;

        // Descriptor 0 chains (NEXT) to descriptor 9, one past a 2-entry queue.
        mem.write_obj(0x4000u64, GuestAddress(desc_table)).unwrap();
        mem.write_obj(64u32, GuestAddress(desc_table + 8)).unwrap();
        mem.write_obj(VRING_DESC_F_NEXT, GuestAddress(desc_table + 12))
            .unwrap();
        mem.write_obj(9u16, GuestAddress(desc_table + 14)).unwrap();

        mem.write_obj(1u16, GuestAddress(avail_ring + 2)).unwrap();
        mem.write_obj(0u16, GuestAddress(avail_ring + 4)).unwrap();

        let mut vq = SplitVirtqueue::new(2, desc_table, avail_ring, 0x3000, -1, -1);
        let chain = vq.pop_avail(&mem).expect("head descriptor is in range");
        assert_eq!(
            chain.descriptors.len(),
            1,
            "the out-of-range NEXT must not be followed"
        );
    }

    #[test]
    fn test_snapshot_restore() {
        let snap = VirtqueueSnapshot {
            last_avail_idx: 42,
            last_used_idx: 37,
        };
        let mut vq = SplitVirtqueue::new(256, 0x1000, 0x2000, 0x3000, -1, -1);
        vq.restore(&snap);
        assert_eq!(vq.last_avail_idx, 42);
        assert_eq!(vq.last_used_idx, 37);

        let snap2 = vq.snapshot();
        assert_eq!(snap2.last_avail_idx, 42);
        assert_eq!(snap2.last_used_idx, 37);
    }
}
