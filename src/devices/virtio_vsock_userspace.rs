//! Userspace virtio-vsock MMIO device for snapshot/restore.
//!
//! Unlike the vhost kernel backend (`VirtioVsockMmio`), this device processes
//! virtio queues entirely in Rust. This gives full control over queue state
//! for clean snapshot/restore — the kernel vhost module corrupts vring state
//! during SET_RUNNING after restore, making reconnection impossible.
//!
//! Host applications connect via AF_UNIX instead of AF_VSOCK.

use std::os::fd::IntoRawFd;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rustix::event::{eventfd, EventfdFlags};
use tracing::{debug, trace, warn};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::devices::virtio_net::mmio;
use crate::devices::virtio_vsock_mmio::VIRTIO_VSOCK_DEVICE_TYPE;
use crate::devices::virtqueue::{SplitVirtqueue, VirtqueueSnapshot, VRING_DESC_F_WRITE};
use crate::devices::vsock_backend::VsockMmioDevice;
use crate::devices::vsock_connection::{VsockConnectionMap, VsockHeader, VSOCK_HEADER_SIZE};
use crate::vmm::snapshot::{QueueSnapshotState, VsockSnapshotState};
use crate::{Error, Result};

/// VIRTIO_F_VERSION_1 — required for virtio-mmio v2 devices.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Queue state for virtio-vsock (rx=0, tx=1, event=2)
#[derive(Default)]
struct QueueConfig {
    num_max: u16,
    num: u16,
    ready: bool,
    desc_addr: u64,
    driver_addr: u64,
    device_addr: u64,
}

/// Userspace virtio-vsock MMIO device.
///
/// Implements the same MMIO register interface as `VirtioVsockMmio` but
/// processes virtio queues in Rust instead of delegating to the kernel
/// vhost-vsock module.
pub struct VirtioVsockUserspace {
    /// Guest CID.
    cid: u32,
    /// MMIO register state.
    device_features: u64,
    driver_features: u64,
    features_sel: u32,
    queue_sel: u32,
    status: u32,
    interrupt_status: u32,
    config_generation: u32,
    /// Queue configuration (set by guest during driver init).
    rx_queue_cfg: QueueConfig,
    tx_queue_cfg: QueueConfig,
    event_queue_cfg: QueueConfig,
    /// MMIO address range.
    mmio_base: u64,
    mmio_size: u64,
    /// Kick eventfds (guest writes these to notify us).
    kick_eventfds: [Option<RawFd>; 3],
    /// Call eventfds (we write these to interrupt the guest).
    call_eventfds: [Option<RawFd>; 3],
    /// Live virtqueues (created when queues become ready).
    rx_queue: Option<SplitVirtqueue>,
    tx_queue: Option<SplitVirtqueue>,
    event_queue: Option<SplitVirtqueue>,
    /// Connection state machine.
    conn_map: Arc<Mutex<VsockConnectionMap>>,
    /// Path to the Unix socket for host connections.
    socket_path: PathBuf,
    /// Background worker thread handle.
    worker_handle: Option<JoinHandle<()>>,
    /// Flag to stop the worker.
    worker_running: Arc<AtomicBool>,
}

impl VirtioVsockUserspace {
    /// Create a new userspace vsock device with the given CID.
    ///
    /// The Unix socket is created at `/tmp/void-box-vsock-{cid}.sock`.
    pub fn new(cid: u32) -> Result<Self> {
        Self::with_socket_path(
            cid,
            PathBuf::from(format!("/tmp/void-box-vsock-{}.sock", cid)),
        )
    }

    /// Create a new userspace vsock device with a specific socket path.
    pub fn with_socket_path(cid: u32, socket_path: PathBuf) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid vsock CID {}: must be >= 3",
                cid
            )));
        }

        let conn_map = VsockConnectionMap::new(cid as u64, &socket_path)?;

        // Create all EventFds first (RAII protects against partial failure)
        let mut kick_fds = Vec::with_capacity(3);
        let mut call_fds = Vec::with_capacity(3);
        for _ in 0..3 {
            kick_fds.push(
                eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC)
                    .map_err(|e| Error::Device(format!("eventfd: {}", e)))?,
            );
            call_fds.push(
                eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC)
                    .map_err(|e| Error::Device(format!("eventfd: {}", e)))?,
            );
        }

        // All allocations succeeded — extract raw fds (mem::forget transfers ownership)
        let mut kick = [None, None, None];
        let mut call = [None, None, None];
        for (i, (k, c)) in kick_fds.into_iter().zip(call_fds).enumerate() {
            let kfd = k.into_raw_fd();
            kick[i] = Some(kfd);
            let cfd = c.into_raw_fd();
            call[i] = Some(cfd);
        }

        debug!(
            "Created userspace vsock device CID {} (socket: {})",
            cid,
            socket_path.display()
        );

        Ok(Self {
            cid,
            device_features: VIRTIO_F_VERSION_1,
            driver_features: 0,
            features_sel: 0,
            queue_sel: 0,
            status: 0,
            interrupt_status: 0,
            config_generation: 0,
            rx_queue_cfg: QueueConfig {
                num_max: 256,
                ..Default::default()
            },
            tx_queue_cfg: QueueConfig {
                num_max: 256,
                ..Default::default()
            },
            event_queue_cfg: QueueConfig {
                num_max: 256,
                ..Default::default()
            },
            mmio_base: 0,
            mmio_size: 0x200,
            kick_eventfds: kick,
            call_eventfds: call,
            rx_queue: None,
            tx_queue: None,
            event_queue: None,
            conn_map: Arc::new(Mutex::new(conn_map)),
            socket_path,
            worker_handle: None,
            worker_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Get the Unix socket path for host connections.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn current_queue_cfg(&self) -> &QueueConfig {
        match self.queue_sel {
            0 => &self.rx_queue_cfg,
            1 => &self.tx_queue_cfg,
            2 => &self.event_queue_cfg,
            _ => &self.rx_queue_cfg,
        }
    }

    fn current_queue_cfg_mut(&mut self) -> &mut QueueConfig {
        match self.queue_sel {
            0 => &mut self.rx_queue_cfg,
            1 => &mut self.tx_queue_cfg,
            2 => &mut self.event_queue_cfg,
            _ => &mut self.rx_queue_cfg,
        }
    }

    /// Activate a queue: create the SplitVirtqueue with the configured addresses.
    fn activate_queue(&mut self, idx: u32) {
        let (cfg, kick_fd, call_fd) = match idx {
            0 => (
                &self.rx_queue_cfg,
                self.kick_eventfds[0],
                self.call_eventfds[0],
            ),
            1 => (
                &self.tx_queue_cfg,
                self.kick_eventfds[1],
                self.call_eventfds[1],
            ),
            2 => (
                &self.event_queue_cfg,
                self.kick_eventfds[2],
                self.call_eventfds[2],
            ),
            _ => return,
        };

        let kick = match kick_fd {
            Some(fd) => fd,
            None => return,
        };
        let call = match call_fd {
            Some(fd) => fd,
            None => return,
        };

        let vq = SplitVirtqueue::new(
            cfg.num,
            cfg.desc_addr,
            cfg.driver_addr,
            cfg.device_addr,
            kick,
            call,
        );

        match idx {
            0 => self.rx_queue = Some(vq),
            1 => self.tx_queue = Some(vq),
            2 => self.event_queue = Some(vq),
            _ => {}
        }

        debug!(
            "virtio-vsock-userspace: queue {} activated (num={})",
            idx, cfg.num
        );
    }

    /// Process TX descriptors from the guest.
    ///
    /// Called when the guest kicks the TX queue (queue 1).
    fn process_tx(&mut self, mem: &GuestMemoryMmap) {
        let tx_queue = match self.tx_queue.as_mut() {
            Some(q) => q,
            None => return,
        };

        let mut processed = 0u32;
        while let Some(chain) = tx_queue.pop_avail(mem) {
            // Read the vsock header from the first descriptor
            let mut hdr_buf = [0u8; VSOCK_HEADER_SIZE];
            if let Some(desc) = chain.descriptors.first() {
                if desc.len >= VSOCK_HEADER_SIZE as u32 {
                    let _ = mem.read(&mut hdr_buf, GuestAddress(desc.addr));
                }
            }

            if let Some(hdr) = VsockHeader::from_bytes(&hdr_buf) {
                // Read data payload from subsequent descriptors
                let mut data = Vec::new();
                for desc in chain.descriptors.iter().skip(1) {
                    if desc.flags & VRING_DESC_F_WRITE == 0 {
                        let mut buf = vec![0u8; desc.len as usize];
                        let _ = mem.read(&mut buf, GuestAddress(desc.addr));
                        data.extend_from_slice(&buf);
                    }
                }
                // Also check if the first descriptor has data after the header
                if let Some(desc) = chain.descriptors.first() {
                    if desc.len > VSOCK_HEADER_SIZE as u32 {
                        let extra = desc.len as usize - VSOCK_HEADER_SIZE;
                        let mut buf = vec![0u8; extra];
                        let _ =
                            mem.read(&mut buf, GuestAddress(desc.addr + VSOCK_HEADER_SIZE as u64));
                        // Prepend to data since it comes from the first descriptor
                        let mut combined = buf;
                        combined.extend_from_slice(&data);
                        data = combined;
                    }
                }

                // Truncate data to the header's len field
                if (hdr.len as usize) < data.len() {
                    data.truncate(hdr.len as usize);
                }

                if let Ok(mut conn_map) = self.conn_map.lock() {
                    conn_map.process_guest_tx(&hdr, &data);
                }
            }

            // Return the descriptor chain to the used ring
            tx_queue.push_used(mem, chain.head_index, 0);
            processed += 1;
        }

        if processed > 0 {
            tx_queue.signal_guest();
            trace!("vsock-userspace: processed {} TX descriptors", processed);
        }
    }

    /// Inject pending RX packets from the connection map into the guest.
    fn process_rx(&mut self, mem: &GuestMemoryMmap) {
        let rx_queue = match self.rx_queue.as_mut() {
            Some(q) => q,
            None => {
                debug!("process_rx: no rx_queue!");
                return;
            }
        };

        let pending = {
            let mut conn_map = match self.conn_map.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            conn_map.drain_rx()
        };

        if pending.is_empty() {
            return;
        }

        let mut injected = 0u32;
        for (hdr, data) in pending {
            // Pop an available RX descriptor
            let chain = match rx_queue.pop_avail(mem) {
                Some(c) => c,
                None => {
                    trace!("vsock-userspace: RX queue full, dropping packet");
                    break;
                }
            };

            // Write header + data scattered across writable descriptors
            let hdr_bytes = hdr.to_bytes();
            let mut write_buf: Vec<u8> = Vec::with_capacity(VSOCK_HEADER_SIZE + data.len());
            write_buf.extend_from_slice(&hdr_bytes);
            write_buf.extend_from_slice(&data);

            let mut bytes_written = 0u32;
            let mut buf_offset = 0usize;

            for desc in &chain.descriptors {
                if desc.flags & VRING_DESC_F_WRITE == 0 {
                    continue;
                }
                let available = desc.len as usize;
                let remaining = write_buf.len() - buf_offset;
                let to_write = remaining.min(available);
                if to_write > 0 {
                    let _ = mem.write(
                        &write_buf[buf_offset..buf_offset + to_write],
                        GuestAddress(desc.addr),
                    );
                    buf_offset += to_write;
                    bytes_written += to_write as u32;
                }
                if buf_offset >= write_buf.len() {
                    break;
                }
            }

            rx_queue.push_used(mem, chain.head_index, bytes_written);
            injected += 1;
        }

        if injected > 0 {
            rx_queue.signal_guest();
            trace!("vsock-userspace: injected {} RX packets", injected);
        }
    }

    /// Start the background worker thread that polls host streams and the
    /// Unix listener for activity.
    fn start_worker(&mut self, _mem: &GuestMemoryMmap) {
        if self.worker_running.load(Ordering::SeqCst) {
            return;
        }

        let conn_map = self.conn_map.clone();
        let running = self.worker_running.clone();
        running.store(true, Ordering::SeqCst);

        // The worker polls host streams and the listener.
        // Actual RX injection happens in the MMIO write handler (QUEUE_NOTIFY)
        // or via the IRQ thread.
        let kick_rx_fd = self.kick_eventfds[0];
        let kick_tx_fd = self.kick_eventfds[1];
        let call_rx_fd = self.call_eventfds[0];

        let handle = std::thread::Builder::new()
            .name("vsock-userspace-worker".into())
            .spawn(move || {
                worker_thread(conn_map, running, kick_rx_fd, kick_tx_fd, call_rx_fd);
            })
            .expect("Failed to spawn vsock-userspace worker");

        self.worker_handle = Some(handle);
        debug!("vsock-userspace: worker thread started");
    }

    /// Restore from snapshot state.
    pub fn restore(
        state: &VsockSnapshotState,
        cid: u32,
        guest_memory: &GuestMemoryMmap,
        socket_path: PathBuf,
    ) -> Result<Self> {
        let mut dev = Self::with_socket_path(cid, socket_path)?;

        // Restore MMIO register state
        dev.device_features = state.device_features;
        dev.driver_features = state.driver_features;
        dev.features_sel = state.features_sel;
        dev.queue_sel = state.queue_sel;
        dev.status = state.status;
        dev.interrupt_status = state.interrupt_status;
        dev.config_generation = state.config_generation;

        // Restore queue configurations
        fn restore_queue_cfg(snap: &QueueSnapshotState) -> QueueConfig {
            QueueConfig {
                num_max: snap.num_max,
                num: snap.num,
                ready: snap.ready,
                desc_addr: snap.desc_addr,
                driver_addr: snap.driver_addr,
                device_addr: snap.device_addr,
            }
        }

        if let Some(q) = state.queues.first() {
            dev.rx_queue_cfg = restore_queue_cfg(q);
        }
        if let Some(q) = state.queues.get(1) {
            dev.tx_queue_cfg = restore_queue_cfg(q);
        }
        if let Some(q) = state.queues.get(2) {
            dev.event_queue_cfg = restore_queue_cfg(q);
        }

        // Activate ready queues
        for idx in 0..3u32 {
            let ready = match idx {
                0 => dev.rx_queue_cfg.ready,
                1 => dev.tx_queue_cfg.ready,
                2 => dev.event_queue_cfg.ready,
                _ => false,
            };
            if ready {
                dev.activate_queue(idx);
            }
        }

        // Restore virtqueue indices from snapshot.
        // When indices are None (snapshot taken with kernel vhost backend which
        // doesn't expose its internal indices), sync from guest memory: read
        // the current avail->idx and used->idx so the userspace backend starts
        // in sync with the guest driver.
        let queue_refs: [(&Option<SplitVirtqueue>, Option<&QueueSnapshotState>); 3] = [
            (&dev.rx_queue, state.queues.first()),
            (&dev.tx_queue, state.queues.get(1)),
            (&dev.event_queue, state.queues.get(2)),
        ];
        // We need indices: collect them first, then apply (borrow-checker).
        let mut idx_updates: Vec<(usize, u16, u16)> = Vec::new();
        for (i, (vq_opt, snap_opt)) in queue_refs.iter().enumerate() {
            if let Some(ref vq) = vq_opt {
                if let Some(q) = snap_opt {
                    let (lai, lui) = match (q.last_avail_idx, q.last_used_idx) {
                        (Some(lai), Some(lui)) => (lai, lui),
                        _ => {
                            // Vhost backend: read current indices from guest memory
                            let avail_idx: u16 = guest_memory
                                .read_obj(GuestAddress(vq.avail_ring_addr + 2))
                                .unwrap_or(0);
                            let used_idx: u16 = guest_memory
                                .read_obj(GuestAddress(vq.used_ring_addr + 2))
                                .unwrap_or(0);
                            debug!(
                                "virtqueue {}: syncing from guest memory avail_idx={} used_idx={}",
                                i, avail_idx, used_idx
                            );
                            (avail_idx, used_idx)
                        }
                    };
                    idx_updates.push((i, lai, lui));
                }
            }
        }
        for (i, lai, lui) in idx_updates {
            let vq = match i {
                0 => dev.rx_queue.as_mut(),
                1 => dev.tx_queue.as_mut(),
                2 => dev.event_queue.as_mut(),
                _ => None,
            };
            if let Some(vq) = vq {
                vq.restore(&VirtqueueSnapshot {
                    last_avail_idx: lai,
                    last_used_idx: lui,
                });
            }
        }

        // Start worker if device was in DRIVER_OK state
        if (dev.status & 4) != 0 {
            dev.start_worker(guest_memory);
        }

        debug!("Restored userspace vsock MMIO (CID {})", cid);
        Ok(dev)
    }

    fn reset(&mut self) {
        debug!("virtio-vsock-userspace: device reset");
        self.status = 0;
        self.interrupt_status = 0;
        self.driver_features = 0;
        self.rx_queue_cfg = QueueConfig {
            num_max: 256,
            ..Default::default()
        };
        self.tx_queue_cfg = QueueConfig {
            num_max: 256,
            ..Default::default()
        };
        self.event_queue_cfg = QueueConfig {
            num_max: 256,
            ..Default::default()
        };
        self.rx_queue = None;
        self.tx_queue = None;
        self.event_queue = None;

        // Stop worker
        self.worker_running.store(false, Ordering::SeqCst);
    }
}

impl VsockMmioDevice for VirtioVsockUserspace {
    fn mmio_base(&self) -> u64 {
        self.mmio_base
    }

    fn mmio_size(&self) -> u64 {
        self.mmio_size
    }

    fn set_mmio_base(&mut self, base: u64) {
        self.mmio_base = base;
        debug!("virtio-vsock-userspace MMIO base set to {:#x}", base);
    }

    fn handles_mmio(&self, addr: u64) -> bool {
        addr >= self.mmio_base && addr < self.mmio_base + self.mmio_size
    }

    fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        let value: u32 = match offset {
            mmio::MAGIC_VALUE => mmio::MAGIC,
            mmio::VERSION => mmio::VERSION_2,
            mmio::DEVICE_ID => VIRTIO_VSOCK_DEVICE_TYPE,
            mmio::VENDOR_ID => 0x554d4551,
            mmio::DEVICE_FEATURES => {
                if self.features_sel == 0 {
                    self.device_features as u32
                } else {
                    (self.device_features >> 32) as u32
                }
            }
            mmio::QUEUE_NUM_MAX => self.current_queue_cfg().num_max as u32,
            mmio::QUEUE_READY => self.current_queue_cfg().ready as u32,
            mmio::INTERRUPT_STATUS => self.interrupt_status,
            mmio::STATUS => self.status,
            mmio::CONFIG_GENERATION => self.config_generation,
            o if (mmio::CONFIG..mmio::CONFIG + 8).contains(&o) => {
                let off = (o - mmio::CONFIG) as usize;
                let cid64 = self.cid as u64;
                if off == 0 {
                    (cid64 & 0xFFFF_FFFF) as u32
                } else {
                    (cid64 >> 32) as u32
                }
            }
            _ => {
                trace!(
                    "virtio-vsock-userspace: unhandled MMIO read offset {:#x}",
                    offset
                );
                0
            }
        };
        let bytes = value.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    fn mmio_write(
        &mut self,
        offset: u64,
        data: &[u8],
        guest_memory: &GuestMemoryMmap,
    ) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let value = u32::from_le_bytes(bytes);

        match offset {
            mmio::DEVICE_FEATURES_SEL => self.features_sel = value,
            mmio::DRIVER_FEATURES => {
                if self.features_sel == 0 {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | (value as u64);
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                }
            }
            mmio::DRIVER_FEATURES_SEL => self.features_sel = value,
            mmio::QUEUE_SEL => self.queue_sel = value,
            mmio::QUEUE_NUM => {
                self.current_queue_cfg_mut().num = value as u16;
            }
            mmio::QUEUE_READY => {
                let idx = self.queue_sel;
                self.current_queue_cfg_mut().ready = value != 0;
                if value != 0 {
                    self.activate_queue(idx);
                }
            }
            mmio::QUEUE_NOTIFY => {
                trace!("vsock-userspace: QUEUE_NOTIFY value={}", value);
                match value {
                    0 => {
                        // RX kick — guest has made RX buffers available.
                        // Try to inject any pending data.
                        self.process_rx(guest_memory);
                    }
                    1 => {
                        // TX kick — guest has put TX descriptors on the ring.
                        self.process_tx(guest_memory);
                        // After processing TX, there may be RX responses to inject.
                        self.process_rx(guest_memory);
                    }
                    _ => {}
                }
                // Write to kick eventfd for the worker thread
                if let Some(fd) = self.kick_eventfds.get(value as usize).and_then(|f| *f) {
                    let val: u64 = 1;
                    let _ = unsafe {
                        libc::write(
                            fd,
                            &val as *const _ as *const libc::c_void,
                            std::mem::size_of::<u64>(),
                        )
                    };
                }
            }
            mmio::INTERRUPT_ACK => {
                trace!("vsock-userspace: INTERRUPT_ACK value={:#x}", value);
                self.interrupt_status &= !value;
                // After the guest acknowledges the interrupt, inject any
                // pending RX data so it is in the descriptors before the
                // guest's bottom-half checks the queue.
                self.process_rx(guest_memory);
            }
            mmio::STATUS => {
                self.status = value;
                if value == 0 {
                    self.reset();
                } else if (value & 4) != 0 {
                    // DRIVER_OK — start the worker
                    self.start_worker(guest_memory);
                }
            }
            mmio::QUEUE_DESC_LOW => {
                let q = self.current_queue_cfg_mut();
                q.desc_addr = (q.desc_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DESC_HIGH => {
                let q = self.current_queue_cfg_mut();
                q.desc_addr = (q.desc_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            mmio::QUEUE_DRIVER_LOW => {
                let q = self.current_queue_cfg_mut();
                q.driver_addr = (q.driver_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DRIVER_HIGH => {
                let q = self.current_queue_cfg_mut();
                q.driver_addr = (q.driver_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            mmio::QUEUE_DEVICE_LOW => {
                let q = self.current_queue_cfg_mut();
                q.device_addr = (q.device_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DEVICE_HIGH => {
                let q = self.current_queue_cfg_mut();
                q.device_addr = (q.device_addr & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            }
            _ => {
                trace!(
                    "virtio-vsock-userspace: unhandled MMIO write offset {:#x} value={:#x}",
                    offset,
                    value
                );
            }
        }
        Ok(())
    }

    fn call_eventfds(&self) -> &[Option<RawFd>; 3] {
        &self.call_eventfds
    }

    fn set_interrupt_status(&mut self, bits: u32) {
        self.interrupt_status |= bits;
    }

    fn snapshot_state(&self) -> VsockSnapshotState {
        let queues = [
            (&self.rx_queue_cfg, &self.rx_queue),
            (&self.tx_queue_cfg, &self.tx_queue),
            (&self.event_queue_cfg, &self.event_queue),
        ]
        .iter()
        .map(|(cfg, vq)| {
            let (lai, lui) = vq
                .as_ref()
                .map(|q| (Some(q.last_avail_idx), Some(q.last_used_idx)))
                .unwrap_or((None, None));
            QueueSnapshotState {
                num_max: cfg.num_max,
                num: cfg.num,
                ready: cfg.ready,
                desc_addr: cfg.desc_addr,
                driver_addr: cfg.driver_addr,
                device_addr: cfg.device_addr,
                last_avail_idx: lai,
                last_used_idx: lui,
            }
        })
        .collect();

        VsockSnapshotState {
            device_features: self.device_features,
            driver_features: self.driver_features,
            features_sel: self.features_sel,
            queue_sel: self.queue_sel,
            status: self.status,
            interrupt_status: self.interrupt_status,
            config_generation: self.config_generation,
            queues,
        }
    }

    fn inject_transport_reset(&mut self, mem: &GuestMemoryMmap) -> Result<()> {
        let event_queue = match self.event_queue.as_mut() {
            Some(q) => q,
            None => {
                debug!("vsock-userspace: no event queue, skipping transport reset");
                return Ok(());
            }
        };

        // Pop an available descriptor from the event queue
        let chain = match event_queue.pop_avail(mem) {
            Some(c) => c,
            None => {
                debug!("vsock-userspace: event queue empty, skipping transport reset");
                return Ok(());
            }
        };

        // Write VIRTIO_VSOCK_EVENT_TRANSPORT_RESET (event id = 0) to the descriptor.
        // The event is a u32 event_id at the start of the buffer.
        // VIRTIO_VSOCK_EVENT_TRANSPORT_RESET = 0
        for desc in &chain.descriptors {
            if desc.flags & VRING_DESC_F_WRITE != 0 && desc.len >= 4 {
                let event_id: u32 = 0; // TRANSPORT_RESET
                let _ = mem.write_obj(event_id, GuestAddress(desc.addr));
                break;
            }
        }

        // Push to used ring and signal guest
        event_queue.push_used(mem, chain.head_index, 4);
        event_queue.signal_guest();

        debug!("vsock-userspace: injected TRANSPORT_RESET event");
        Ok(())
    }
}

impl Drop for VirtioVsockUserspace {
    fn drop(&mut self) {
        self.worker_running.store(false, Ordering::SeqCst);

        // Wake the worker from epoll_wait by writing to a kick eventfd
        if let Some(Some(fd)) = self.kick_eventfds.first() {
            unsafe {
                libc::eventfd_write(*fd, 1);
            }
        }

        // Join worker thread before closing fds
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        // Now safe to close eventfds
        for fd in self.kick_eventfds.iter().flatten() {
            unsafe {
                libc::close(*fd);
            }
        }
        for fd in self.call_eventfds.iter().flatten() {
            unsafe {
                libc::close(*fd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

/// Background worker that polls the Unix listener and host streams.
///
/// When data arrives from a host application, it buffers it in the
/// connection map and writes to the RX call eventfd so the IRQ thread
/// can inject an interrupt and process RX descriptors.
fn worker_thread(
    conn_map: Arc<Mutex<VsockConnectionMap>>,
    running: Arc<AtomicBool>,
    _kick_rx_fd: Option<RawFd>,
    _kick_tx_fd: Option<RawFd>,
    call_rx_fd: Option<RawFd>,
) {
    use libc::{
        epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLIN, EPOLL_CLOEXEC, EPOLL_CTL_ADD,
    };

    let epfd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
    if epfd < 0 {
        warn!(
            "vsock-userspace worker: epoll_create1 failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    // Add listener fd to epoll
    let listener_fd = conn_map.lock().ok().and_then(|m| m.listener_fd());
    if let Some(lfd) = listener_fd {
        let mut ev = epoll_event {
            events: EPOLLIN as u32,
            u64: 0xFFFF_FFFF, // sentinel for listener
        };
        unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, lfd, &mut ev) };
    }

    let mut events = [epoll_event { events: 0, u64: 0 }; 16];

    while running.load(Ordering::Relaxed) {
        let nfds = unsafe { epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, 50) };
        if nfds < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            warn!("vsock-userspace worker: epoll_wait: {}", e);
            break;
        }

        let mut accepted_fds = Vec::new();
        let mut had_data = false;

        for event in events.iter().take(nfds as usize) {
            if event.u64 == 0xFFFF_FFFF {
                // Listener ready — accept incoming connections.
                // accept_incoming() queues OP_REQUEST into rx_queue,
                // so we must signal the guest to process it.
                if let Ok(mut map) = conn_map.lock() {
                    while let Some((_key, fd)) = map.accept_incoming() {
                        accepted_fds.push(fd);
                        had_data = true;
                    }
                }
            } else {
                // Data from a host stream — read and buffer
                let _fd_idx = event.u64 as usize;
                if let Ok(mut map) = conn_map.lock() {
                    // Find the connection by iterating (indexed by fd position)
                    let conn_data: Vec<_> = map
                        .connections
                        .iter_mut()
                        .filter(|(_, c)| {
                            c.state == crate::devices::vsock_connection::ConnState::Connected
                        })
                        .map(|((gp, hp), c)| {
                            let n = c.read_from_host();
                            if n > 0 {
                                let data = c.tx_buf.drain(..).collect::<Vec<_>>();
                                Some((*gp, *hp, data))
                            } else {
                                None
                            }
                        })
                        .collect();

                    for entry in conn_data.into_iter().flatten() {
                        let (gp, hp, data) = entry;
                        map.queue_host_data(gp, hp, &data);
                        had_data = true;
                    }
                }
            }
        }

        // Add newly accepted fds to epoll
        for fd in accepted_fds {
            let mut ev = epoll_event {
                events: EPOLLIN as u32,
                u64: fd as u64,
            };
            unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev) };
        }

        // Also poll all connected streams periodically (every iteration)
        if let Ok(mut map) = conn_map.lock() {
            let conn_data: Vec<_> = map
                .connections
                .iter_mut()
                .filter(|(_, c)| c.state == crate::devices::vsock_connection::ConnState::Connected)
                .filter_map(|((gp, hp), c)| {
                    let n = c.read_from_host();
                    if n > 0 {
                        let data = c.tx_buf.drain(..).collect::<Vec<_>>();
                        Some((*gp, *hp, data))
                    } else {
                        None
                    }
                })
                .collect();

            for (gp, hp, data) in conn_data {
                map.queue_host_data(gp, hp, &data);
                had_data = true;
            }
        }

        // If we have pending RX data, signal via call eventfd
        if had_data {
            trace!("vsock-userspace: signaling call_rx_fd (had_data=true)");
            if let Some(fd) = call_rx_fd {
                let val: u64 = 1;
                let _ = unsafe { libc::write(fd, &val as *const _ as *const libc::c_void, 8) };
            }
        }
    }

    unsafe {
        libc::close(epfd);
    }
    debug!("vsock-userspace worker thread exiting");
}
