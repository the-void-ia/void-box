//! virtio-9p device for host directory sharing (9P2000.L)
//!
//! This module implements a virtio 9P transport device that presents a host
//! directory to the guest via the 9P2000.L protocol. The guest mounts it with:
//!
//! ```text
//! mount -t 9p -o trans=virtio,version=9p2000.L mount0 /mnt
//! ```
//!
//! The virtio-9P device uses MMIO transport and provides:
//! - Host directory sharing into the guest
//! - Read-only or read-write access
//! - 9P2000.L protocol subset: version, attach, walk, lopen, read, write,
//!   getattr, readdir, lcreate, mkdir, clunk

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use tracing::{debug, trace, warn};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use crate::devices::virtio_net::mmio;

// ---------------------------------------------------------------------------
// Virtio constants
// ---------------------------------------------------------------------------

/// Virtio device type for 9P transport (VIRTIO_ID_9P)
pub const VIRTIO_9P_DEVICE_TYPE: u32 = 9;

/// 9P mount tag feature bit
const VIRTIO_9P_MOUNT_TAG: u64 = 1 << 0;

/// Required for virtio-mmio version 2 devices
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Virtio descriptor flag: buffer continues via `next` field
const VIRTQ_DESC_F_NEXT: u16 = 1;

/// Virtio descriptor flag: buffer is device-writable (response)
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Maximum virtqueue size for the request queue
const QUEUE_MAX_SIZE: u16 = 128;

/// Maximum number of symlink follows during Twalk path resolution.
/// Linux MAXSYMLINKS is 40; 20 is sufficient for container images with
/// deep alternative-system chains (e.g. Debian update-alternatives).
const MAX_SYMLINK_FOLLOWS: usize = 20;

// ---------------------------------------------------------------------------
// 9P2000.L message types
// ---------------------------------------------------------------------------

const T_VERSION: u8 = 100;
const R_VERSION: u8 = 101;
const T_ATTACH: u8 = 104;
const R_ATTACH: u8 = 105;
const T_WALK: u8 = 110;
const R_WALK: u8 = 111;
const T_LOPEN: u8 = 12;
const R_LOPEN: u8 = 13;
const T_LCREATE: u8 = 14;
const R_LCREATE: u8 = 15;
const T_STATFS: u8 = 8;
const R_STATFS: u8 = 9;
const T_READ: u8 = 116;
const R_READ: u8 = 117;
const T_WRITE: u8 = 118;
const R_WRITE: u8 = 119;
const T_CLUNK: u8 = 120;
const R_CLUNK: u8 = 121;
const T_READLINK: u8 = 22;
const R_READLINK: u8 = 23;
const T_GETATTR: u8 = 24;
const R_GETATTR: u8 = 25;
const T_READDIR: u8 = 40;
const R_READDIR: u8 = 41;
const T_XATTRWALK: u8 = 30;
const R_XATTRWALK: u8 = 31;
const T_MKDIR: u8 = 72;
const R_MKDIR: u8 = 73;
const R_ERROR: u8 = 7;

/// QID size in bytes: type(1) + version(4) + path(8) = 13
const QID_SIZE: usize = 13;

// ---------------------------------------------------------------------------
// Internal state types
// ---------------------------------------------------------------------------

/// State associated with a 9P fid
struct FidState {
    path: PathBuf,
    open_file: Option<std::fs::File>,
}

/// Virtqueue bookkeeping
#[derive(Debug, Default)]
struct QueueState {
    /// Maximum queue size supported by the device
    num_max: u16,
    /// Current queue size configured by the driver
    num: u16,
    /// Queue ready flag
    ready: bool,
    /// Descriptor table guest-physical address
    desc_addr: u64,
    /// Driver (available) ring guest-physical address
    driver_addr: u64,
    /// Device (used) ring guest-physical address
    device_addr: u64,
}

// ---------------------------------------------------------------------------
// Virtio9pDevice
// ---------------------------------------------------------------------------

/// Virtio-MMIO 9P device for host directory sharing
pub struct Virtio9pDevice {
    mmio_base: u64,
    // virtio state
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_sel: u32,
    queue: QueueState,
    interrupt_status: u32,
    status: u32,
    // 9p state
    root_dir: PathBuf,
    mount_tag: String,
    read_only: bool,
    fids: HashMap<u32, FidState>,
    // internal virtqueue tracking
    avail_idx: u16,
    used_idx: u16,
}

impl Virtio9pDevice {
    fn normalize_under_root(root: &PathBuf, path: &std::path::Path) -> Option<PathBuf> {
        let mut out = root.clone();
        for comp in path.components() {
            match comp {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if out == *root {
                        return None;
                    }
                    out.pop();
                }
                std::path::Component::Normal(p) => out.push(p),
                _ => return None,
            }
        }
        Some(out)
    }
    /// Create a new virtio-9P device sharing `root_dir` with the given mount tag.
    ///
    /// The mount tag is what the guest uses in the mount command:
    /// `mount -t 9p -o trans=virtio,version=9p2000.L <tag> /mnt`
    pub fn new(
        root_dir: impl Into<PathBuf>,
        mount_tag: impl Into<String>,
        read_only: bool,
    ) -> Self {
        let root_dir = root_dir.into();
        let mount_tag = mount_tag.into();
        debug!(
            "Creating virtio-9p device: root={:?}, tag={}, ro={}",
            root_dir, mount_tag, read_only
        );

        Self {
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
            root_dir,
            mount_tag,
            read_only,
            fids: HashMap::new(),
            avail_idx: 0,
            used_idx: 0,
        }
    }

    // -- MMIO interface (duck-typed, matching VirtioNetDevice) ----------------

    /// Set the MMIO base address
    pub fn set_mmio_base(&mut self, base: u64) {
        self.mmio_base = base;
        debug!("virtio-9p MMIO base set to {:#x}", base);
    }

    /// Get the MMIO base address
    pub fn mmio_base(&self) -> u64 {
        self.mmio_base
    }

    /// Get the MMIO region size
    pub fn mmio_size(&self) -> u64 {
        0x200
    }

    /// Check if an address falls within this device's MMIO region
    pub fn handles_mmio(&self, addr: u64) -> bool {
        addr >= self.mmio_base && addr < self.mmio_base + self.mmio_size()
    }

    /// Check if there are pending interrupts
    pub fn has_pending_interrupt(&self) -> bool {
        self.interrupt_status != 0
    }

    /// Device features for this 9P device
    fn device_features(&self) -> u64 {
        VIRTIO_9P_MOUNT_TAG | VIRTIO_F_VERSION_1
    }

    /// Build the config-space bytes (tag_len: u16 LE, then tag bytes)
    fn config_space(&self) -> Vec<u8> {
        let tag_bytes = self.mount_tag.as_bytes();
        let tag_len = tag_bytes.len() as u16;
        let mut cfg = Vec::with_capacity(2 + tag_bytes.len());
        cfg.extend_from_slice(&tag_len.to_le_bytes());
        cfg.extend_from_slice(tag_bytes);
        cfg
    }

    /// Handle MMIO read
    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        let value: u32 = match offset {
            mmio::MAGIC_VALUE => mmio::MAGIC,
            mmio::VERSION => mmio::VERSION_2,
            mmio::DEVICE_ID => VIRTIO_9P_DEVICE_TYPE,
            mmio::VENDOR_ID => 0x554d4551, // "QEMU"
            mmio::DEVICE_FEATURES => {
                let feats = self.device_features();
                if self.device_features_sel == 0 {
                    feats as u32
                } else {
                    (feats >> 32) as u32
                }
            }
            mmio::QUEUE_NUM_MAX => self.queue.num_max as u32,
            mmio::QUEUE_READY => self.queue.ready as u32,
            mmio::INTERRUPT_STATUS => self.interrupt_status,
            mmio::STATUS => self.status,
            mmio::CONFIG_GENERATION => 0,
            // Config space starts at 0x100 — byte-addressable
            o if o >= mmio::CONFIG => {
                let cfg = self.config_space();
                let cfg_off = (o - mmio::CONFIG) as usize;
                if cfg_off < cfg.len() {
                    // Read up to 4 bytes from config space at the requested offset
                    let mut val_bytes = [0u8; 4];
                    let avail = (cfg.len() - cfg_off).min(4);
                    val_bytes[..avail].copy_from_slice(&cfg[cfg_off..cfg_off + avail]);
                    u32::from_le_bytes(val_bytes)
                } else {
                    0
                }
            }
            _ => {
                trace!("virtio-9p: unhandled MMIO read at offset {:#x}", offset);
                0
            }
        };

        let bytes = value.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    /// Handle MMIO write. Pass `guest_mem` so queue-notify can process 9P requests.
    pub fn mmio_write(&mut self, offset: u64, data: &[u8], guest_mem: Option<&GuestMemoryMmap>) {
        if data.is_empty() {
            return;
        }

        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let value = u32::from_le_bytes(bytes);

        match offset {
            mmio::DEVICE_FEATURES_SEL => {
                self.device_features_sel = value;
            }
            mmio::DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | (value as u64);
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                }
            }
            mmio::DRIVER_FEATURES_SEL => {
                self.driver_features_sel = value;
            }
            mmio::QUEUE_SEL => {
                self.queue_sel = value;
            }
            mmio::QUEUE_NUM => {
                self.queue.num = value as u16;
            }
            mmio::QUEUE_READY => {
                self.queue.ready = value != 0;
                if self.queue.ready {
                    debug!("virtio-9p: queue {} ready", self.queue_sel);
                }
            }
            mmio::QUEUE_NOTIFY => {
                if let Some(mem) = guest_mem {
                    if let Err(e) = self.process_queue(mem) {
                        warn!("virtio-9p: queue processing error: {}", e);
                    }
                } else {
                    trace!("virtio-9p: queue notify without guest memory");
                }
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
                self.queue.desc_addr =
                    (self.queue.desc_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DESC_HIGH => {
                self.queue.desc_addr =
                    (self.queue.desc_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            mmio::QUEUE_DRIVER_LOW => {
                self.queue.driver_addr =
                    (self.queue.driver_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DRIVER_HIGH => {
                self.queue.driver_addr =
                    (self.queue.driver_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            mmio::QUEUE_DEVICE_LOW => {
                self.queue.device_addr =
                    (self.queue.device_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DEVICE_HIGH => {
                self.queue.device_addr =
                    (self.queue.device_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            _ => {
                trace!(
                    "virtio-9p: unhandled MMIO write at offset {:#x}, value={:#x}",
                    offset,
                    value
                );
            }
        }
    }

    // -- Device reset --------------------------------------------------------

    fn reset(&mut self) {
        debug!("virtio-9p: device reset");
        self.status = 0;
        self.interrupt_status = 0;
        self.driver_features = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        self.queue = QueueState {
            num_max: QUEUE_MAX_SIZE,
            ..Default::default()
        };
        self.fids.clear();
        self.avail_idx = 0;
        self.used_idx = 0;
    }

    // -- Virtqueue processing ------------------------------------------------

    /// Process all pending descriptors on the request queue.
    fn process_queue(&mut self, mem: &GuestMemoryMmap) -> crate::Result<()> {
        let q = &self.queue;
        if !q.ready || q.num == 0 {
            return Ok(());
        }

        let desc_addr = GuestAddress(q.desc_addr);
        let avail_addr = GuestAddress(q.driver_addr);
        let used_addr = GuestAddress(q.device_addr);
        let queue_size = q.num as usize;

        // Read current available index from the driver ring
        let mut idx_buf = [0u8; 2];
        mem.read(&mut idx_buf, avail_addr.unchecked_add(2u64))
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
        let avail_idx = u16::from_le_bytes(idx_buf);

        while self.avail_idx != avail_idx {
            // Read head descriptor index from available ring
            let ring_offset = 4 + ((self.avail_idx as usize) % queue_size) * 2;
            let mut desc_id_buf = [0u8; 2];
            mem.read(
                &mut desc_id_buf,
                avail_addr.unchecked_add(ring_offset as u64),
            )
            .map_err(|e| crate::Error::Memory(e.to_string()))?;
            let head_idx = u16::from_le_bytes(desc_id_buf) as usize;

            // Walk the descriptor chain: collect request bytes and find the
            // writable (response) descriptor(s).
            let mut request_data = Vec::new();
            let mut response_descs: Vec<(u64, u32)> = Vec::new(); // (guest addr, len)
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
                let dlen = u32::from_le_bytes(desc[8..12].try_into().unwrap());
                let flags = u16::from_le_bytes(desc[12..14].try_into().unwrap());
                let next_desc = u16::from_le_bytes(desc[14..16].try_into().unwrap()) as usize;

                if (flags & VIRTQ_DESC_F_WRITE) != 0 {
                    // Device-writable: response buffer
                    response_descs.push((addr, dlen));
                } else {
                    // Device-readable: request data
                    if dlen > 0 && addr != 0 {
                        let mut buf = vec![0u8; dlen as usize];
                        mem.read(&mut buf, GuestAddress(addr))
                            .map_err(|e| crate::Error::Memory(e.to_string()))?;
                        request_data.extend_from_slice(&buf);
                    }
                }

                if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                    break;
                }
                next = next_desc;
            }

            // Process the 9P request and produce a response
            let response = self.handle_9p_request(&request_data);

            // Write response into the writable descriptors
            let mut written: usize = 0;
            let mut resp_off: usize = 0;
            for (gpa, capacity) in &response_descs {
                if resp_off >= response.len() {
                    break;
                }
                let to_write = ((*capacity) as usize).min(response.len() - resp_off);
                mem.write(&response[resp_off..resp_off + to_write], GuestAddress(*gpa))
                    .map_err(|e| crate::Error::Memory(e.to_string()))?;
                written += to_write;
                resp_off += to_write;
            }

            // Update used ring
            let used_ring_off = 4 + ((self.used_idx as usize) % queue_size) * 8;
            let used_elem = [
                (head_idx as u32).to_le_bytes(),
                (written as u32).to_le_bytes(),
            ]
            .concat();
            mem.write(&used_elem, used_addr.unchecked_add(used_ring_off as u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;

            self.used_idx = self.used_idx.wrapping_add(1);
            self.avail_idx = self.avail_idx.wrapping_add(1);

            // Update used.idx so guest sees progress
            let used_idx_bytes = self.used_idx.to_le_bytes();
            mem.write(&used_idx_bytes, used_addr.unchecked_add(2u64))
                .map_err(|e| crate::Error::Memory(e.to_string()))?;
        }

        // Signal interrupt
        self.interrupt_status |= 1;
        Ok(())
    }

    // -- 9P2000.L protocol handling ------------------------------------------

    /// Dispatch an incoming 9P request and return the full response message
    /// (including the 4-byte size header).
    fn handle_9p_request(&mut self, data: &[u8]) -> Vec<u8> {
        // Minimum 9P header: size(4) + type(1) + tag(2) = 7 bytes
        if data.len() < 7 {
            warn!("virtio-9p: request too short ({} bytes)", data.len());
            return Self::build_error(0, libc::EIO as u32);
        }

        let _msg_size = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let msg_type = data[4];
        let tag = u16::from_le_bytes(data[5..7].try_into().unwrap());
        let payload = &data[7..];

        trace!(
            "virtio-9p: request type={} tag={} payload_len={}",
            msg_type,
            tag,
            payload.len()
        );

        match msg_type {
            T_VERSION => self.handle_version(tag, payload),
            T_ATTACH => self.handle_attach(tag, payload),
            T_WALK => self.handle_walk(tag, payload),
            T_LOPEN => self.handle_lopen(tag, payload),
            T_LCREATE => self.handle_lcreate(tag, payload),
            T_STATFS => self.handle_statfs(tag, payload),
            T_READ => self.handle_read(tag, payload),
            T_WRITE => self.handle_write(tag, payload),
            T_CLUNK => self.handle_clunk(tag, payload),
            T_READLINK => self.handle_readlink(tag, payload),
            T_GETATTR => self.handle_getattr(tag, payload),
            T_XATTRWALK => self.handle_xattrwalk(tag, payload),
            T_READDIR => self.handle_readdir(tag, payload),
            T_MKDIR => self.handle_mkdir(tag, payload),
            _ => {
                warn!("virtio-9p: unsupported message type {}", msg_type);
                Self::build_error(tag, libc::EOPNOTSUPP as u32)
            }
        }
    }

    // -- 9P message builders -------------------------------------------------

    /// Build a complete 9P message with header: size(4) + type(1) + tag(2) + payload
    fn build_message(msg_type: u8, tag: u16, payload: &[u8]) -> Vec<u8> {
        let size = (4 + 1 + 2 + payload.len()) as u32;
        let mut msg = Vec::with_capacity(size as usize);
        msg.extend_from_slice(&size.to_le_bytes());
        msg.push(msg_type);
        msg.extend_from_slice(&tag.to_le_bytes());
        msg.extend_from_slice(payload);
        msg
    }

    /// Build an Rerror message
    fn build_error(tag: u16, ecode: u32) -> Vec<u8> {
        Self::build_message(R_ERROR, tag, &ecode.to_le_bytes())
    }

    /// Build a QID from file metadata.
    /// QID: type(1) + version(4) + path(8) = 13 bytes
    fn build_qid(metadata: &fs::Metadata) -> [u8; QID_SIZE] {
        let qtype: u8 = if metadata.is_dir() {
            0x80
        } else if metadata.file_type().is_symlink() {
            0x02
        } else {
            0x00
        };
        // Use mtime as version (truncated to u32)
        let version = metadata.mtime() as u32;
        // Use inode number as path identifier
        let path = metadata.ino();

        let mut qid = [0u8; QID_SIZE];
        qid[0] = qtype;
        qid[1..5].copy_from_slice(&version.to_le_bytes());
        qid[5..13].copy_from_slice(&path.to_le_bytes());
        qid
    }

    // -- 9P message handlers -------------------------------------------------

    /// Handle Tversion: negotiate protocol version
    fn handle_version(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Tversion: msize(4) + version_string(2-byte len + bytes)
        if payload.len() < 6 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let client_msize = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        // We use the minimum of client and a reasonable max
        let msize = client_msize.min(64 * 1024);

        // Clear all fids on version negotiation (as per spec)
        self.fids.clear();

        // Rversion: msize(4) + version_string
        let version = b"9P2000.L";
        let mut resp_payload = Vec::new();
        resp_payload.extend_from_slice(&msize.to_le_bytes());
        resp_payload.extend_from_slice(&(version.len() as u16).to_le_bytes());
        resp_payload.extend_from_slice(version);

        debug!(
            "virtio-9p: Tversion msize={} -> Rversion msize={}",
            client_msize, msize
        );
        Self::build_message(R_VERSION, tag, &resp_payload)
    }

    /// Handle Tattach: establish a fid for the root directory
    fn handle_attach(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Tattach: fid(4) + afid(4) + uname(2+n) + aname(2+n) + n_uname(4)
        if payload.len() < 12 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());

        let root_path = match self.root_dir.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                warn!("virtio-9p: cannot canonicalize root dir: {}", e);
                return Self::build_error(tag, libc::ENOENT as u32);
            }
        };

        let metadata = match fs::metadata(&root_path) {
            Ok(m) => m,
            Err(e) => {
                warn!("virtio-9p: cannot stat root dir: {}", e);
                return Self::build_error(tag, io_error_to_errno(&e));
            }
        };

        let qid = Self::build_qid(&metadata);

        self.fids.insert(
            fid,
            FidState {
                path: root_path,
                open_file: None,
            },
        );

        debug!("virtio-9p: Tattach fid={}", fid);
        Self::build_message(R_ATTACH, tag, &qid)
    }

    /// Handle Twalk: walk a path from an existing fid, producing a new fid
    fn handle_walk(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Twalk: fid(4) + newfid(4) + nwname(2) + wname[nwname](2+n each)
        if payload.len() < 10 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let newfid = u32::from_le_bytes(payload[4..8].try_into().unwrap());
        let nwname = u16::from_le_bytes(payload[8..10].try_into().unwrap());

        let base_path = match self.fids.get(&fid) {
            Some(state) => state.path.clone(),
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        // If nwname == 0, clone fid to newfid
        if nwname == 0 {
            self.fids.insert(
                newfid,
                FidState {
                    path: base_path,
                    open_file: None,
                },
            );
            // Rwalk with 0 qids
            let mut resp = Vec::new();
            resp.extend_from_slice(&0u16.to_le_bytes());
            return Self::build_message(R_WALK, tag, &resp);
        }

        // Walk each name component
        let mut current = base_path;
        let mut qids = Vec::new();
        let mut off = 10;
        let root_path = match self.root_dir.canonicalize() {
            Ok(r) => r,
            Err(_) => return Self::build_error(tag, libc::EIO as u32),
        };

        let mut walked_names: Vec<String> = Vec::new();
        for _ in 0..nwname {
            if off + 2 > payload.len() {
                return Self::build_error(tag, libc::EINVAL as u32);
            }
            let name_len = u16::from_le_bytes(payload[off..off + 2].try_into().unwrap()) as usize;
            off += 2;
            if off + name_len > payload.len() {
                return Self::build_error(tag, libc::EINVAL as u32);
            }
            let name = match std::str::from_utf8(&payload[off..off + name_len]) {
                Ok(s) => s,
                Err(_) => return Self::build_error(tag, libc::EINVAL as u32),
            };
            off += name_len;
            walked_names.push(name.to_string());

            // Resolve each component under rootfs semantics, including symlinks.
            let next = if name == "." {
                current.clone()
            } else if name == ".." {
                if current == root_path {
                    root_path.clone()
                } else {
                    current
                        .parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| root_path.clone())
                }
            } else {
                current.join(name)
            };

            if !next.starts_with(&root_path) {
                return Self::build_error(tag, libc::EACCES as u32);
            }

            let mut resolved = next.clone();
            let mut metadata = match fs::symlink_metadata(&resolved) {
                Ok(m) => m,
                Err(e) => {
                    if !qids.is_empty() {
                        break;
                    }
                    trace!(
                        "virtio-9p: Twalk missing component '{}' under {:?}: {}",
                        name,
                        current,
                        e
                    );
                    return Self::build_error(tag, io_error_to_errno(&e));
                }
            };

            // Follow symlinks with container-root semantics:
            // absolute targets stay within root_path.
            for _ in 0..MAX_SYMLINK_FOLLOWS {
                if !metadata.file_type().is_symlink() {
                    break;
                }
                let target = match fs::read_link(&resolved) {
                    Ok(t) => t,
                    Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
                };
                let candidate = if target.is_absolute() {
                    root_path.join(target.strip_prefix("/").unwrap_or(target.as_path()))
                } else {
                    resolved
                        .parent()
                        .map(|p| p.join(&target))
                        .unwrap_or_else(|| root_path.join(&target))
                };
                resolved = match Self::normalize_under_root(&root_path, &candidate) {
                    Some(p) => p,
                    None => return Self::build_error(tag, libc::EACCES as u32),
                };
                if !resolved.starts_with(&root_path) {
                    return Self::build_error(tag, libc::EACCES as u32);
                }
                metadata = match fs::symlink_metadata(&resolved) {
                    Ok(m) => m,
                    Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
                };
            }
            if metadata.file_type().is_symlink() {
                return Self::build_error(tag, libc::ELOOP as u32);
            }

            qids.push(Self::build_qid(&metadata));
            current = resolved;
        }

        // Install newfid pointing at the walked-to path
        self.fids.insert(
            newfid,
            FidState {
                path: current,
                open_file: None,
            },
        );

        // Rwalk: nwqid(2) + qid[nwqid]
        let mut resp = Vec::new();
        resp.extend_from_slice(&(qids.len() as u16).to_le_bytes());
        for qid in &qids {
            resp.extend_from_slice(qid);
        }

        trace!(
            "virtio-9p: Twalk fid={} newfid={} names={:?} walked={}",
            fid,
            newfid,
            walked_names,
            qids.len()
        );
        Self::build_message(R_WALK, tag, &resp)
    }

    /// Handle Tlopen: open a fid for I/O
    fn handle_lopen(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Tlopen: fid(4) + flags(4)
        if payload.len() < 8 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let flags = u32::from_le_bytes(payload[4..8].try_into().unwrap());

        let state = match self.fids.get_mut(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let metadata = match fs::metadata(&state.path) {
            Ok(m) => m,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        // For regular files, open them; for directories, we don't need an fd
        if metadata.is_file() {
            let linux_flags = flags & 0x3; // O_RDONLY=0, O_WRONLY=1, O_RDWR=2
            let mut options = fs::OpenOptions::new();
            match linux_flags {
                0 => {
                    options.read(true);
                }
                1 => {
                    if self.read_only {
                        return Self::build_error(tag, libc::EROFS as u32);
                    }
                    options.write(true);
                }
                2 => {
                    if self.read_only {
                        return Self::build_error(tag, libc::EROFS as u32);
                    }
                    options.read(true).write(true);
                }
                _ => {
                    options.read(true);
                }
            }

            // Handle O_TRUNC
            if (flags & 0x200) != 0 {
                if self.read_only {
                    return Self::build_error(tag, libc::EROFS as u32);
                }
                options.truncate(true);
            }
            // Handle O_APPEND
            if (flags & 0x400) != 0 {
                options.append(true);
            }

            match options.open(&state.path) {
                Ok(f) => {
                    state.open_file = Some(f);
                }
                Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
            }
        }

        let qid = Self::build_qid(&metadata);
        // Rlopen: qid(13) + iounit(4)
        let mut resp = Vec::new();
        resp.extend_from_slice(&qid);
        resp.extend_from_slice(&0u32.to_le_bytes()); // iounit=0 means use msize
        trace!("virtio-9p: Tlopen fid={} flags={:#x}", fid, flags);
        Self::build_message(R_LOPEN, tag, &resp)
    }

    /// Handle Tlcreate: create a new file and open it
    fn handle_lcreate(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        if self.read_only {
            return Self::build_error(tag, libc::EROFS as u32);
        }

        // Tlcreate: fid(4) + name(2+n) + flags(4) + mode(4) + gid(4)
        if payload.len() < 14 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[4..6].try_into().unwrap()) as usize;
        if payload.len() < 6 + name_len + 12 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }
        let name = match std::str::from_utf8(&payload[6..6 + name_len]) {
            Ok(s) => s.to_owned(),
            Err(_) => return Self::build_error(tag, libc::EINVAL as u32),
        };
        let off = 6 + name_len;
        let flags = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        let _mode = u32::from_le_bytes(payload[off + 4..off + 8].try_into().unwrap());
        let _gid = u32::from_le_bytes(payload[off + 8..off + 12].try_into().unwrap());

        let parent_path = match self.fids.get(&fid) {
            Some(s) => s.path.clone(),
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let new_path = parent_path.join(&name);

        // Security check
        if let Ok(root) = self.root_dir.canonicalize() {
            if let Ok(parent_canon) = parent_path.canonicalize() {
                if !parent_canon.starts_with(&root) {
                    return Self::build_error(tag, libc::EACCES as u32);
                }
            }
        }

        // Create the file
        let linux_flags = flags & 0x3;
        let mut options = fs::OpenOptions::new();
        options.create(true);
        match linux_flags {
            0 => {
                options.read(true).write(true); // CREATE implies write
            }
            1 => {
                options.write(true);
            }
            _ => {
                options.read(true).write(true);
            }
        }
        if (flags & 0x200) != 0 {
            options.truncate(true);
        }

        let file = match options.open(&new_path) {
            Ok(f) => f,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        let metadata = match new_path.metadata() {
            Ok(m) => m,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        let qid = Self::build_qid(&metadata);

        // Update the fid to point to the newly created file
        self.fids.insert(
            fid,
            FidState {
                path: new_path,
                open_file: Some(file),
            },
        );

        // Rlcreate: qid(13) + iounit(4)
        let mut resp = Vec::new();
        resp.extend_from_slice(&qid);
        resp.extend_from_slice(&0u32.to_le_bytes());
        debug!("virtio-9p: Tlcreate fid={} name={}", fid, name);
        Self::build_message(R_LCREATE, tag, &resp)
    }

    /// Handle Tstatfs: return filesystem statistics for a fid path
    fn handle_statfs(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Tstatfs: fid(4)
        if payload.len() < 4 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let state = match self.fids.get(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let cpath = match CString::new(state.path.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => return Self::build_error(tag, libc::EINVAL as u32),
        };

        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut st as *mut libc::statvfs) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO) as u32;
            return Self::build_error(tag, errno);
        }

        // Rstatfs: type(4) + bsize(4) + blocks(8) + bfree(8) + bavail(8) +
        //          files(8) + ffree(8) + fsid(8) + namelen(4)
        let mut resp = Vec::with_capacity(60);
        let fs_type: u32 = 0;
        resp.extend_from_slice(&fs_type.to_le_bytes());
        resp.extend_from_slice(&(st.f_bsize as u32).to_le_bytes());
        resp.extend_from_slice(&st.f_blocks.to_le_bytes());
        resp.extend_from_slice(&st.f_bfree.to_le_bytes());
        resp.extend_from_slice(&st.f_bavail.to_le_bytes());
        resp.extend_from_slice(&st.f_files.to_le_bytes());
        resp.extend_from_slice(&st.f_ffree.to_le_bytes());
        resp.extend_from_slice(&0u64.to_le_bytes()); // fsid not provided by statvfs
        resp.extend_from_slice(&(st.f_namemax as u32).to_le_bytes());

        trace!("virtio-9p: Tstatfs fid={}", fid);
        Self::build_message(R_STATFS, tag, &resp)
    }

    /// Handle Tread: read data from an open fid
    fn handle_read(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Tread: fid(4) + offset(8) + count(4)
        if payload.len() < 16 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let offset = u64::from_le_bytes(payload[4..12].try_into().unwrap());
        let count = u32::from_le_bytes(payload[12..16].try_into().unwrap());

        let state = match self.fids.get_mut(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let file = match state.open_file.as_mut() {
            Some(f) => f,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        // Seek to offset and read
        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return Self::build_error(tag, io_error_to_errno(&e));
        }

        let mut buf = vec![0u8; count as usize];
        let nread = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };
        buf.truncate(nread);

        // Rread: count(4) + data[count]
        let mut resp = Vec::with_capacity(4 + nread);
        resp.extend_from_slice(&(nread as u32).to_le_bytes());
        resp.extend_from_slice(&buf);
        trace!(
            "virtio-9p: Tread fid={} offset={} count={} -> {}",
            fid,
            offset,
            count,
            nread
        );
        Self::build_message(R_READ, tag, &resp)
    }

    /// Handle Twrite: write data to an open fid
    fn handle_write(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        if self.read_only {
            return Self::build_error(tag, libc::EROFS as u32);
        }

        // Twrite: fid(4) + offset(8) + count(4) + data[count]
        if payload.len() < 16 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let offset = u64::from_le_bytes(payload[4..12].try_into().unwrap());
        let count = u32::from_le_bytes(payload[12..16].try_into().unwrap());

        if payload.len() < 16 + count as usize {
            return Self::build_error(tag, libc::EINVAL as u32);
        }
        let write_data = &payload[16..16 + count as usize];

        let state = match self.fids.get_mut(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let file = match state.open_file.as_mut() {
            Some(f) => f,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return Self::build_error(tag, io_error_to_errno(&e));
        }

        let nwritten = match file.write(write_data) {
            Ok(n) => n,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        // Rwrite: count(4)
        trace!(
            "virtio-9p: Twrite fid={} offset={} count={} -> {}",
            fid,
            offset,
            count,
            nwritten
        );
        Self::build_message(R_WRITE, tag, &(nwritten as u32).to_le_bytes())
    }

    /// Handle Tclunk: release a fid
    fn handle_clunk(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        if payload.len() < 4 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        self.fids.remove(&fid);
        trace!("virtio-9p: Tclunk fid={}", fid);
        Self::build_message(R_CLUNK, tag, &[])
    }

    /// Handle Treadlink: read symlink target
    fn handle_readlink(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        if payload.len() < 4 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let state = match self.fids.get(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let target = match fs::read_link(&state.path) {
            Ok(t) => t,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        let target_bytes = target.as_os_str().as_bytes();
        if target_bytes.len() > u16::MAX as usize {
            return Self::build_error(tag, libc::ENAMETOOLONG as u32);
        }

        // Rreadlink payload is a 9P string.
        let mut resp = Vec::with_capacity(2 + target_bytes.len());
        resp.extend_from_slice(&(target_bytes.len() as u16).to_le_bytes());
        resp.extend_from_slice(target_bytes);
        Self::build_message(R_READLINK, tag, &resp)
    }

    /// Handle Tgetattr: get file attributes
    fn handle_getattr(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Tgetattr: fid(4) + request_mask(8)
        if payload.len() < 12 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let request_mask = u64::from_le_bytes(payload[4..12].try_into().unwrap());

        let state = match self.fids.get(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let metadata = match fs::metadata(&state.path) {
            Ok(m) => m,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        let qid = Self::build_qid(&metadata);

        // Rgetattr: valid(8) + qid(13) + mode(4) + uid(4) + gid(4) +
        //           nlink(8) + rdev(8) + size(8) + blksize(8) + blocks(8) +
        //           atime_sec(8) + atime_nsec(8) +
        //           mtime_sec(8) + mtime_nsec(8) +
        //           ctime_sec(8) + ctime_nsec(8) +
        //           btime_sec(8) + btime_nsec(8) +
        //           gen(8) + data_version(8)
        let mut resp = Vec::with_capacity(160);

        // valid: echo back what the client asked for
        resp.extend_from_slice(&request_mask.to_le_bytes());
        resp.extend_from_slice(&qid);
        resp.extend_from_slice(&metadata.mode().to_le_bytes()); // mode
        resp.extend_from_slice(&metadata.uid().to_le_bytes()); // uid
        resp.extend_from_slice(&metadata.gid().to_le_bytes()); // gid
        resp.extend_from_slice(&metadata.nlink().to_le_bytes()); // nlink
        resp.extend_from_slice(&metadata.rdev().to_le_bytes()); // rdev
        resp.extend_from_slice(&metadata.size().to_le_bytes()); // size
        resp.extend_from_slice(&metadata.blksize().to_le_bytes()); // blksize
        resp.extend_from_slice(&metadata.blocks().to_le_bytes()); // blocks
                                                                  // atime
        resp.extend_from_slice(&(metadata.atime() as u64).to_le_bytes());
        resp.extend_from_slice(&(metadata.atime_nsec() as u64).to_le_bytes());
        // mtime
        resp.extend_from_slice(&(metadata.mtime() as u64).to_le_bytes());
        resp.extend_from_slice(&(metadata.mtime_nsec() as u64).to_le_bytes());
        // ctime
        resp.extend_from_slice(&(metadata.ctime() as u64).to_le_bytes());
        resp.extend_from_slice(&(metadata.ctime_nsec() as u64).to_le_bytes());
        // btime (birth time — not available on all Linux fs, use 0)
        resp.extend_from_slice(&0u64.to_le_bytes());
        resp.extend_from_slice(&0u64.to_le_bytes());
        // gen
        resp.extend_from_slice(&0u64.to_le_bytes());
        // data_version
        resp.extend_from_slice(&0u64.to_le_bytes());

        trace!("virtio-9p: Tgetattr fid={}", fid);
        Self::build_message(R_GETATTR, tag, &resp)
    }

    /// Handle Txattrwalk: report xattr size for a file.
    ///
    /// For OCI rootfs execution we only need "no xattr" semantics; returning
    /// size=0 keeps Linux client lookups moving instead of failing on EOPNOTSUPP.
    fn handle_xattrwalk(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Txattrwalk: fid(4) + newfid(4) + name(2+n)
        if payload.len() < 10 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let newfid = u32::from_le_bytes(payload[4..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
        if payload.len() < 10 + name_len {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let base = match self.fids.get(&fid) {
            Some(s) => s.path.clone(),
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        self.fids.insert(
            newfid,
            FidState {
                path: base,
                open_file: None,
            },
        );

        // Rxattrwalk: size(8). Report no xattr data.
        Self::build_message(R_XATTRWALK, tag, &0u64.to_le_bytes())
    }

    /// Handle Treaddir: read directory entries
    fn handle_readdir(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        // Treaddir: fid(4) + offset(8) + count(4)
        if payload.len() < 16 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let fid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let offset = u64::from_le_bytes(payload[4..12].try_into().unwrap());
        let count = u32::from_le_bytes(payload[12..16].try_into().unwrap());

        let state = match self.fids.get(&fid) {
            Some(s) => s,
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let dir_path = state.path.clone();

        let entries = match fs::read_dir(&dir_path) {
            Ok(rd) => rd,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        // Collect all entries, including "." and ".."
        let mut all_entries: Vec<(String, fs::Metadata)> = Vec::new();

        // Add "." entry
        if let Ok(m) = fs::metadata(&dir_path) {
            all_entries.push((".".to_string(), m));
        }
        // Add ".." entry
        if let Some(parent) = dir_path.parent() {
            // Ensure ".." doesn't escape root
            let root_canon = self.root_dir.canonicalize().unwrap_or_default();
            let parent_path = if dir_path == root_canon {
                // At root, ".." points to root itself
                dir_path.clone()
            } else {
                parent.to_path_buf()
            };
            if let Ok(m) = fs::metadata(&parent_path) {
                all_entries.push(("..".to_string(), m));
            }
        }

        // Add regular entries
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.metadata() {
                Ok(m) => all_entries.push((name, m)),
                Err(_) => continue,
            }
        }

        // Build dirent stream starting from the given offset
        // Dirent format: qid(13) + offset(8) + type(1) + name_len(2) + name(n)
        let mut dirent_data = Vec::new();
        let max_bytes = count as usize;

        for (idx, (name, metadata)) in all_entries.iter().enumerate() {
            let entry_offset = idx as u64;
            if entry_offset < offset {
                continue;
            }

            let qid = Self::build_qid(metadata);
            let dtype: u8 = if metadata.is_dir() {
                4 // DT_DIR
            } else if metadata.is_symlink() {
                10 // DT_LNK
            } else {
                8 // DT_REG
            };
            let name_bytes = name.as_bytes();
            let entry_size = QID_SIZE + 8 + 1 + 2 + name_bytes.len();

            // Check if adding this entry would exceed the requested count
            if dirent_data.len() + entry_size > max_bytes {
                break;
            }

            dirent_data.extend_from_slice(&qid);
            // offset points to the *next* entry (so consumer can resume)
            dirent_data.extend_from_slice(&(entry_offset + 1).to_le_bytes());
            dirent_data.push(dtype);
            dirent_data.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            dirent_data.extend_from_slice(name_bytes);
        }

        // Rreaddir: count(4) + data[count]
        let mut resp = Vec::with_capacity(4 + dirent_data.len());
        resp.extend_from_slice(&(dirent_data.len() as u32).to_le_bytes());
        resp.extend_from_slice(&dirent_data);
        trace!(
            "virtio-9p: Treaddir fid={} offset={} count={} -> {} bytes",
            fid,
            offset,
            count,
            dirent_data.len()
        );
        Self::build_message(R_READDIR, tag, &resp)
    }

    /// Handle Tmkdir: create a directory
    fn handle_mkdir(&mut self, tag: u16, payload: &[u8]) -> Vec<u8> {
        if self.read_only {
            return Self::build_error(tag, libc::EROFS as u32);
        }

        // Tmkdir: dfid(4) + name(2+n) + mode(4) + gid(4)
        if payload.len() < 10 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }

        let dfid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[4..6].try_into().unwrap()) as usize;
        if payload.len() < 6 + name_len + 8 {
            return Self::build_error(tag, libc::EINVAL as u32);
        }
        let name = match std::str::from_utf8(&payload[6..6 + name_len]) {
            Ok(s) => s.to_owned(),
            Err(_) => return Self::build_error(tag, libc::EINVAL as u32),
        };

        let parent_path = match self.fids.get(&dfid) {
            Some(s) => s.path.clone(),
            None => return Self::build_error(tag, libc::EBADF as u32),
        };

        let new_dir = parent_path.join(&name);

        // Security check
        if let Ok(root) = self.root_dir.canonicalize() {
            if let Ok(parent_canon) = parent_path.canonicalize() {
                if !parent_canon.starts_with(&root) {
                    return Self::build_error(tag, libc::EACCES as u32);
                }
            }
        }

        if let Err(e) = fs::create_dir(&new_dir) {
            return Self::build_error(tag, io_error_to_errno(&e));
        }

        let metadata = match fs::metadata(&new_dir) {
            Ok(m) => m,
            Err(e) => return Self::build_error(tag, io_error_to_errno(&e)),
        };

        let qid = Self::build_qid(&metadata);
        debug!("virtio-9p: Tmkdir dfid={} name={}", dfid, name);
        Self::build_message(R_MKDIR, tag, &qid)
    }
}

/// Map a `std::io::Error` to a Linux errno value for the 9P Rerror response.
fn io_error_to_errno(e: &std::io::Error) -> u32 {
    match e.raw_os_error() {
        Some(code) => code as u32,
        None => match e.kind() {
            std::io::ErrorKind::NotFound => libc::ENOENT as u32,
            std::io::ErrorKind::PermissionDenied => libc::EACCES as u32,
            std::io::ErrorKind::AlreadyExists => libc::EEXIST as u32,
            std::io::ErrorKind::InvalidInput => libc::EINVAL as u32,
            std::io::ErrorKind::WouldBlock => libc::EAGAIN as u32,
            _ => libc::EIO as u32,
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_device() -> Virtio9pDevice {
        Virtio9pDevice::new("/tmp", "mount0", true)
    }

    #[test]
    fn test_device_creation() {
        let dev = make_device();
        assert_eq!(dev.mmio_base(), 0);
        assert_eq!(dev.mmio_size(), 0x200);
        assert!(!dev.has_pending_interrupt());
        assert_eq!(dev.mount_tag, "mount0");
        assert!(dev.read_only);
    }

    #[test]
    fn test_mmio_base() {
        let mut dev = make_device();
        dev.set_mmio_base(0xd000_0000);
        assert_eq!(dev.mmio_base(), 0xd000_0000);
        assert!(dev.handles_mmio(0xd000_0000));
        assert!(dev.handles_mmio(0xd000_01ff));
        assert!(!dev.handles_mmio(0xd000_0200));
        assert!(!dev.handles_mmio(0xcfff_ffff));
    }

    #[test]
    fn test_mmio_read_magic() {
        let dev = make_device();
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::MAGIC_VALUE, &mut data);
        let magic = u32::from_le_bytes(data);
        assert_eq!(magic, 0x74726976);
    }

    #[test]
    fn test_mmio_read_version() {
        let dev = make_device();
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::VERSION, &mut data);
        let version = u32::from_le_bytes(data);
        assert_eq!(version, 2);
    }

    #[test]
    fn test_mmio_read_device_id() {
        let dev = make_device();
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::DEVICE_ID, &mut data);
        let device_id = u32::from_le_bytes(data);
        assert_eq!(device_id, 9);
    }

    #[test]
    fn test_mmio_read_vendor_id() {
        let dev = make_device();
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::VENDOR_ID, &mut data);
        let vendor_id = u32::from_le_bytes(data);
        assert_eq!(vendor_id, 0x554d4551);
    }

    #[test]
    fn test_mmio_read_features_low() {
        let dev = make_device();
        let mut data = [0u8; 4];
        // features_sel defaults to 0 -> low 32 bits
        dev.mmio_read(mmio::DEVICE_FEATURES, &mut data);
        let feats_lo = u32::from_le_bytes(data);
        // Bit 0 = VIRTIO_9P_MOUNT_TAG
        assert_eq!(feats_lo & 1, 1);
    }

    #[test]
    fn test_mmio_read_features_high() {
        let mut dev = make_device();
        // Select high feature page
        dev.mmio_write(mmio::DEVICE_FEATURES_SEL, &1u32.to_le_bytes(), None);
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::DEVICE_FEATURES, &mut data);
        let feats_hi = u32::from_le_bytes(data);
        // Bit 0 of high word = VIRTIO_F_VERSION_1 (bit 32 overall)
        assert_eq!(feats_hi & 1, 1);
    }

    #[test]
    fn test_config_space_tag_len() {
        let dev = make_device();
        // Config offset 0x100: tag_len (u16 LE)
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::CONFIG, &mut data);
        let tag_len = u16::from_le_bytes([data[0], data[1]]);
        assert_eq!(tag_len, 6); // "mount0" is 6 bytes
    }

    #[test]
    fn test_config_space_tag_bytes() {
        let dev = make_device();
        // tag_len at offset 0x100, tag bytes start at 0x102
        // Reading 4 bytes at 0x100 gives: tag_len(2 bytes) + first 2 tag bytes
        let mut data0 = [0u8; 4];
        dev.mmio_read(mmio::CONFIG, &mut data0);
        // data0 = [6, 0, b'm', b'o']
        assert_eq!(data0[0], 6);
        assert_eq!(data0[1], 0);
        assert_eq!(data0[2], b'm');
        assert_eq!(data0[3], b'o');

        // Read next 4 bytes at 0x104
        let mut data1 = [0u8; 4];
        dev.mmio_read(mmio::CONFIG + 4, &mut data1);
        // data1 = [b'u', b'n', b't', b'0']
        assert_eq!(data1[0], b'u');
        assert_eq!(data1[1], b'n');
        assert_eq!(data1[2], b't');
        assert_eq!(data1[3], b'0');
    }

    #[test]
    fn test_mmio_queue_num_max() {
        let dev = make_device();
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::QUEUE_NUM_MAX, &mut data);
        let max = u32::from_le_bytes(data);
        assert_eq!(max, 128);
    }

    #[test]
    fn test_mmio_status_write_read() {
        let mut dev = make_device();
        dev.mmio_write(mmio::STATUS, &3u32.to_le_bytes(), None);
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::STATUS, &mut data);
        assert_eq!(u32::from_le_bytes(data), 3);
    }

    #[test]
    fn test_device_reset() {
        let mut dev = make_device();
        dev.mmio_write(mmio::STATUS, &3u32.to_le_bytes(), None);
        // Writing 0 triggers reset
        dev.mmio_write(mmio::STATUS, &0u32.to_le_bytes(), None);
        let mut data = [0u8; 4];
        dev.mmio_read(mmio::STATUS, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0);
    }

    #[test]
    fn test_9p_version() {
        let mut dev = make_device();
        // Build Tversion: msize=8192, version="9P2000.L"
        let version_str = b"9P2000.L";
        let mut payload = Vec::new();
        payload.extend_from_slice(&8192u32.to_le_bytes());
        payload.extend_from_slice(&(version_str.len() as u16).to_le_bytes());
        payload.extend_from_slice(version_str);

        let msg_size = (4 + 1 + 2 + payload.len()) as u32;
        let mut request = Vec::new();
        request.extend_from_slice(&msg_size.to_le_bytes());
        request.push(T_VERSION);
        request.extend_from_slice(&0u16.to_le_bytes()); // tag
        request.extend_from_slice(&payload);

        let response = dev.handle_9p_request(&request);
        assert!(response.len() >= 7);
        assert_eq!(response[4], R_VERSION);
    }

    #[test]
    fn test_9p_attach() {
        let mut dev = Virtio9pDevice::new("/tmp", "test", true);

        // First do Tversion
        let mut vp = Vec::new();
        vp.extend_from_slice(&8192u32.to_le_bytes());
        vp.extend_from_slice(&8u16.to_le_bytes());
        vp.extend_from_slice(b"9P2000.L");
        let sz = (7 + vp.len()) as u32;
        let mut vreq = Vec::new();
        vreq.extend_from_slice(&sz.to_le_bytes());
        vreq.push(T_VERSION);
        vreq.extend_from_slice(&0u16.to_le_bytes());
        vreq.extend_from_slice(&vp);
        dev.handle_9p_request(&vreq);

        // Build Tattach: fid=0, afid=u32::MAX, uname="root", aname="", n_uname=0
        let mut ap = Vec::new();
        ap.extend_from_slice(&0u32.to_le_bytes()); // fid
        ap.extend_from_slice(&u32::MAX.to_le_bytes()); // afid (no auth)
        ap.extend_from_slice(&4u16.to_le_bytes()); // uname len
        ap.extend_from_slice(b"root");
        ap.extend_from_slice(&0u16.to_le_bytes()); // aname len
        ap.extend_from_slice(&0u32.to_le_bytes()); // n_uname
        let asz = (7 + ap.len()) as u32;
        let mut areq = Vec::new();
        areq.extend_from_slice(&asz.to_le_bytes());
        areq.push(T_ATTACH);
        areq.extend_from_slice(&1u16.to_le_bytes()); // tag
        areq.extend_from_slice(&ap);

        let response = dev.handle_9p_request(&areq);
        assert!(response.len() >= 7);
        assert_eq!(response[4], R_ATTACH);
        // Response should contain a 13-byte QID after the header
        let resp_payload_size = response.len() - 7;
        assert_eq!(resp_payload_size, QID_SIZE);
    }

    #[test]
    fn test_9p_clunk() {
        let mut dev = Virtio9pDevice::new("/tmp", "test", true);
        // Insert a fake fid
        dev.fids.insert(
            42,
            FidState {
                path: PathBuf::from("/tmp"),
                open_file: None,
            },
        );

        let mut payload = Vec::new();
        payload.extend_from_slice(&42u32.to_le_bytes());
        let sz = (7 + payload.len()) as u32;
        let mut req = Vec::new();
        req.extend_from_slice(&sz.to_le_bytes());
        req.push(T_CLUNK);
        req.extend_from_slice(&1u16.to_le_bytes());
        req.extend_from_slice(&payload);

        let response = dev.handle_9p_request(&req);
        assert_eq!(response[4], R_CLUNK);
        assert!(!dev.fids.contains_key(&42));
    }

    #[test]
    fn test_9p_error_on_bad_fid() {
        let mut dev = make_device();
        // Tgetattr on non-existent fid
        let mut payload = Vec::new();
        payload.extend_from_slice(&999u32.to_le_bytes()); // bad fid
        payload.extend_from_slice(&0xFFFFu64.to_le_bytes()); // request_mask
        let sz = (7 + payload.len()) as u32;
        let mut req = Vec::new();
        req.extend_from_slice(&sz.to_le_bytes());
        req.push(T_GETATTR);
        req.extend_from_slice(&1u16.to_le_bytes());
        req.extend_from_slice(&payload);

        let response = dev.handle_9p_request(&req);
        assert_eq!(response[4], R_ERROR);
    }

    #[test]
    fn test_read_only_write_rejected() {
        let mut dev = Virtio9pDevice::new("/tmp", "test", true);
        dev.fids.insert(
            1,
            FidState {
                path: PathBuf::from("/tmp/nonexistent_test_file"),
                open_file: None,
            },
        );

        // Try Twrite on a read-only device
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes()); // fid
        payload.extend_from_slice(&0u64.to_le_bytes()); // offset
        payload.extend_from_slice(&4u32.to_le_bytes()); // count
        payload.extend_from_slice(b"test");
        let sz = (7 + payload.len()) as u32;
        let mut req = Vec::new();
        req.extend_from_slice(&sz.to_le_bytes());
        req.push(T_WRITE);
        req.extend_from_slice(&1u16.to_le_bytes());
        req.extend_from_slice(&payload);

        let response = dev.handle_9p_request(&req);
        assert_eq!(response[4], R_ERROR);
        // Should be EROFS
        let ecode = u32::from_le_bytes(response[7..11].try_into().unwrap());
        assert_eq!(ecode, libc::EROFS as u32);
    }

    #[test]
    fn test_build_qid_directory() {
        use std::fs;
        let metadata = fs::metadata("/tmp").unwrap();
        let qid = Virtio9pDevice::build_qid(&metadata);
        assert_eq!(qid[0], 0x80); // directory type
        assert_eq!(qid.len(), QID_SIZE);
    }

    #[test]
    fn test_io_error_to_errno() {
        let e = std::io::Error::from_raw_os_error(libc::EPERM);
        assert_eq!(io_error_to_errno(&e), libc::EPERM as u32);

        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert_eq!(io_error_to_errno(&e), libc::ENOENT as u32);
    }
}
