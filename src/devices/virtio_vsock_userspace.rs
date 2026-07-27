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
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// Interval of the worker's periodic sweep: the epoll wait bound in the
/// normal path, and the sleep between sweeps in the degraded (no-epoll)
/// path.
const WORKER_TICK: Duration = Duration::from_millis(50);

/// Deadline for joining the worker thread during device teardown. The
/// worker re-checks its lifecycle flag every [`WORKER_TICK`]; hitting
/// this means it is wedged, and teardown detaches it with a warning.
const WORKER_JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// Poll interval while waiting for the worker thread to exit.
const WORKER_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Locks the connection map, recovering from a poisoned mutex: skipping
/// every subsequent lock would leave the listener and every host stream
/// silently unserviced, a worse outcome than one possibly-desynced
/// connection. Warns once per device.
fn lock_conn_map(conn_map: &Mutex<VsockConnectionMap>) -> MutexGuard<'_, VsockConnectionMap> {
    match conn_map.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let guard = poisoned.into_inner();
            if !guard.poison_warned.swap(true, Ordering::Relaxed) {
                warn!("vsock-userspace: connection map mutex poisoned; recovering");
            }
            guard
        }
    }
}

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
    /// Set when [`stop_worker`](Self::stop_worker) gave up joining. A
    /// detached worker may still write the call eventfd, so `Drop` must
    /// leak the eventfds — closed descriptor numbers get reused.
    worker_detached: bool,
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

        let mut device = Self {
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
            worker_detached: false,
        };
        device.start_worker();
        Ok(device)
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

                lock_conn_map(&self.conn_map).process_guest_tx(&hdr, &data);
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

        let pending = lock_conn_map(&self.conn_map).drain_rx();

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

    /// Start the background worker thread that services host streams and
    /// the Unix listener. Idempotent.
    ///
    /// The worker's lifetime is the device's, not the driver's: it
    /// starts at construction and survives guest resets, so connects
    /// made while the driver is down are accepted and queued instead of
    /// sitting in an unserviced accept backlog. It stops only through
    /// [`stop_worker`](Self::stop_worker).
    fn start_worker(&mut self) {
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

    /// Stop the worker thread and join it, bounded by
    /// [`WORKER_JOIN_DEADLINE`]; a worker that fails to exit is
    /// detached with a warning rather than hanging teardown.
    fn stop_worker(&mut self) {
        self.worker_running.store(false, Ordering::SeqCst);
        let Some(handle) = self.worker_handle.take() else {
            return;
        };
        let deadline = Instant::now() + WORKER_JOIN_DEADLINE;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                warn!(
                    "vsock-userspace: worker did not exit within {:?}; detaching it",
                    WORKER_JOIN_DEADLINE
                );
                self.worker_detached = true;
                return;
            }
            std::thread::sleep(WORKER_JOIN_POLL_INTERVAL);
        }
        let _ = handle.join();
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

        // The worker already runs (started at construction), even for a
        // snapshot taken before the guest driver reached DRIVER_OK.

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

        // The worker deliberately survives a driver reset — its
        // lifetime is the device's (see `start_worker`).
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
                    // DRIVER_OK — defensive restart; the worker
                    // normally already runs from construction.
                    self.start_worker();
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

    fn shutdown_worker(&mut self) {
        self.stop_worker();
    }
}

impl Drop for VirtioVsockUserspace {
    fn drop(&mut self) {
        // Stop and join the worker before closing fds; its epoll wait
        // is bounded by WORKER_TICK, so no wakeup write is needed.
        self.stop_worker();

        // A detached worker may still write the call eventfd; leak the
        // fds rather than let that write land on a reused descriptor.
        if self.worker_detached {
            return;
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

/// Background worker that services the Unix listener and host streams.
///
/// When data arrives from a host application, it buffers it in the
/// connection map and writes to the RX call eventfd so the IRQ thread
/// can inject an interrupt and process RX descriptors.
///
/// Every iteration unconditionally accepts pending connections and
/// drains host streams; epoll is only a wakeup accelerator on top of
/// that sweep, so any epoll failure degrades wakeup latency to one
/// [`WORKER_TICK`] instead of leaving the listener unserviced.
fn worker_thread(
    conn_map: Arc<Mutex<VsockConnectionMap>>,
    running: Arc<AtomicBool>,
    _kick_rx_fd: Option<RawFd>,
    _kick_tx_fd: Option<RawFd>,
    call_rx_fd: Option<RawFd>,
) {
    use libc::{
        epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLET, EPOLLIN, EPOLL_CLOEXEC,
        EPOLL_CTL_ADD,
    };

    // Edge-triggered: the sweep ignores the events, and level-triggered
    // would re-fire nonstop for a stream deliberately left unread at
    // its buffering cap, busy-spinning the loop.
    let registration_events = EPOLLIN as u32 | EPOLLET as u32;

    let mut epfd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
    if epfd < 0 {
        warn!(
            "vsock-userspace worker: epoll_create1 failed: {}; \
             falling back to a {:?} periodic sweep",
            std::io::Error::last_os_error(),
            WORKER_TICK
        );
    } else {
        // Register the listener for early wakeup on incoming connects.
        let listener_fd = lock_conn_map(&conn_map).listener_fd();
        if let Some(lfd) = listener_fd {
            let mut ev = epoll_event {
                events: registration_events,
                u64: lfd as u64,
            };
            unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, lfd, &mut ev) };
        }
    }

    let mut events = [epoll_event { events: 0, u64: 0 }; 16];

    while running.load(Ordering::Relaxed) {
        // Wait for activity or the tick. The returned events are not
        // inspected — the sweep below services everything — so a failed
        // wait costs only latency.
        if epfd >= 0 {
            let nfds = unsafe {
                epoll_wait(
                    epfd,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    WORKER_TICK.as_millis() as i32,
                )
            };
            if nfds < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EINTR) {
                    // Non-EINTR failures tend to repeat; drop epoll for
                    // good and live on the periodic sweep.
                    warn!(
                        "vsock-userspace worker: epoll_wait: {}; \
                         switching to a {:?} periodic sweep",
                        e, WORKER_TICK
                    );
                    unsafe { libc::close(epfd) };
                    epfd = -1;
                }
            }
        } else {
            std::thread::sleep(WORKER_TICK);
        }

        // Unconditional sweep: service the listener (established
        // connections queue OP_REQUESTs, so the guest must be
        // signaled), then drain host streams and reap closed
        // connections (their fds close on removal, deregistering them
        // from epoll).
        let accepted_fds;
        let mut had_data = false;
        {
            let mut map = lock_conn_map(&conn_map);
            accepted_fds = map.service_listener();
            if !accepted_fds.is_empty() {
                had_data = true;
            }
            if map.drain_host_streams() {
                had_data = true;
            }
        }

        // Register newly accepted fds for early wakeup (best-effort —
        // the sweep services them even if registration fails).
        if epfd >= 0 {
            for fd in accepted_fds {
                let mut ev = epoll_event {
                    events: registration_events,
                    u64: fd as u64,
                };
                unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev) };
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

    if epfd >= 0 {
        unsafe {
            libc::close(epfd);
        }
    }
    debug!("vsock-userspace worker thread exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::os::unix::net::UnixStream;

    fn temp_socket_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "void-box-vsock-test-{}-{}.sock",
            tag,
            std::process::id()
        ))
    }

    /// The worker services the listener for the device's lifetime: it
    /// runs before the guest driver reaches DRIVER_OK, and a STATUS=0
    /// reset (guest reboot or panic) must not stop it.
    #[test]
    fn worker_runs_from_construction_and_survives_driver_reset() {
        let mut dev = VirtioVsockUserspace::with_socket_path(3, temp_socket_path("lifecycle"))
            .expect("create device");
        assert!(
            dev.worker_running.load(Ordering::SeqCst),
            "worker must start with the device"
        );

        dev.reset();
        assert!(
            dev.worker_running.load(Ordering::SeqCst),
            "driver reset must not stop the worker"
        );
        let handle = dev.worker_handle.as_ref().expect("worker handle");
        assert!(!handle.is_finished(), "worker thread must still be alive");
    }

    /// A host application connecting while the guest driver is down must
    /// be accepted and queued for the guest, not parked in the accept
    /// backlog.
    #[test]
    fn host_connect_is_accepted_without_driver_ok() {
        let path = temp_socket_path("accept");
        let dev = VirtioVsockUserspace::with_socket_path(3, path.clone()).expect("create device");

        let mut stream = UnixStream::connect(&path).expect("connect to device socket");
        stream
            .write_all(&1234u32.to_le_bytes())
            .expect("send target port");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock_conn_map(&dev.conn_map).has_pending_rx() {
            assert!(
                Instant::now() < deadline,
                "worker never accepted the connection"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The teardown hook stops and joins the worker.
    #[test]
    fn shutdown_worker_stops_and_joins_the_thread() {
        let mut dev = VirtioVsockUserspace::with_socket_path(3, temp_socket_path("shutdown"))
            .expect("create device");
        dev.shutdown_worker();
        assert!(!dev.worker_running.load(Ordering::SeqCst));
        assert!(dev.worker_handle.is_none());
        assert!(
            !dev.worker_detached,
            "worker must have been joined, not detached"
        );
    }
}
