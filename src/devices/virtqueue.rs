//! Split virtqueue implementation for userspace virtio devices.
//!
//! Pure data-structure module for reading/writing split virtqueues from
//! guest memory. No device logic — used by the userspace vsock backend.

use std::os::unix::io::RawFd;

use serde::{Deserialize, Serialize};
use tracing::{debug, trace, warn};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

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

    /// Check if there are available descriptors to process.
    pub fn has_avail(&self, mem: &GuestMemoryMmap) -> bool {
        // avail->idx is at avail_ring_addr + 2
        let avail_idx: u16 = mem
            .read_obj(GuestAddress(self.avail_ring_addr + 2))
            .unwrap_or(self.last_avail_idx);
        avail_idx != self.last_avail_idx
    }

    /// Pop the next available descriptor chain from the queue.
    ///
    /// Returns `None` if no descriptors are available.
    pub fn pop_avail(&mut self, mem: &GuestMemoryMmap) -> Option<DescriptorChain> {
        // Read avail->idx (u16 at offset 2 in the available ring)
        let avail_idx: u16 = mem.read_obj(GuestAddress(self.avail_ring_addr + 2)).ok()?;

        if avail_idx == self.last_avail_idx {
            return None;
        }

        // Read the descriptor index from avail->ring[last_avail_idx % num]
        let ring_offset = 4 + (self.last_avail_idx % self.num) as u64 * 2;
        let head_index: u16 = mem
            .read_obj(GuestAddress(self.avail_ring_addr + ring_offset))
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

            let desc_addr = self.desc_table_addr + idx as u64 * 16;
            let addr: u64 = mem.read_obj(GuestAddress(desc_addr)).ok()?;
            let len: u32 = mem.read_obj(GuestAddress(desc_addr + 8)).ok()?;
            let flags: u16 = mem.read_obj(GuestAddress(desc_addr + 12)).ok()?;
            let next: u16 = mem.read_obj(GuestAddress(desc_addr + 14)).ok()?;

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
    pub fn push_used(&mut self, mem: &GuestMemoryMmap, head_index: u16, len: u32) {
        // used->ring[last_used_idx % num] = { id: head_index, len }
        let ring_offset = 4 + (self.last_used_idx % self.num) as u64 * 8;
        let used_elem_addr = self.used_ring_addr + ring_offset;

        // Write used element (id: u32, len: u32)
        let _ = mem.write_obj(head_index as u32, GuestAddress(used_elem_addr));
        let _ = mem.write_obj(len, GuestAddress(used_elem_addr + 4));

        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        // Update used->idx
        let _ = mem.write_obj(self.last_used_idx, GuestAddress(self.used_ring_addr + 2));

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
