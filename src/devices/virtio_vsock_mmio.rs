//! virtio-vsock MMIO device for host-guest communication
//!
//! Presents a virtio-vsock device to the guest and attaches it to the kernel
//! vhost-vsock backend so host connect(CID, port) reaches the guest.

use std::os::fd::AsRawFd;
use std::os::fd::IntoRawFd;
use std::os::unix::io::RawFd;
use std::path::Path;

use rustix::event::{eventfd, EventfdFlags};
use rustix::fs::{open, Mode, OFlags};
use tracing::{debug, trace, warn};
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryRegion};

use crate::devices::virtio_net::mmio;
use crate::{Error, Result};

/// Virtio device type for vsock (Linux VIRTIO_ID_VSOCK)
pub const VIRTIO_VSOCK_DEVICE_TYPE: u32 = 19;

/// VIRTIO_F_VERSION_1 - required for virtio-mmio v2 devices
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// vhost ioctl constants (Linux include/uapi/linux/vhost.h).
/// On x86_64: _IO = 0x0000, _IOW = 0x4000, _IOR = 0x8000, _IOWR = 0xC000 (in upper 16 bits).
/// Format: direction(2) | size(14) | type(8) | nr(8)
mod vhost_ioctl {
    use std::os::raw::c_uint;
    // _IO(0xAF, 0x01)
    pub const VHOST_SET_OWNER: c_uint = 0x0000_AF01;
    // _IOW(0xAF, 0x03, struct vhost_memory) - sizeof=8
    pub const VHOST_SET_MEM_TABLE: c_uint = 0x4008_AF03;
    // _IOW(0xAF, 0x10, struct vhost_vring_state) - sizeof=8
    pub const VHOST_SET_VRING_NUM: c_uint = 0x4008_AF10;
    // _IOW(0xAF, 0x11, struct vhost_vring_addr) - sizeof=40
    pub const VHOST_SET_VRING_ADDR: c_uint = 0x4028_AF11;
    // _IOW(0xAF, 0x12, struct vhost_vring_state) - sizeof=8
    pub const VHOST_SET_VRING_BASE: c_uint = 0x4008_AF12;
    // _IOW(0xAF, 0x20, struct vhost_vring_file) - sizeof=8
    pub const VHOST_SET_VRING_KICK: c_uint = 0x4008_AF20;
    // _IOW(0xAF, 0x21, struct vhost_vring_file) - sizeof=8
    pub const VHOST_SET_VRING_CALL: c_uint = 0x4008_AF21;
}

#[repr(C)]
struct VhostMemoryRegion {
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
}

#[repr(C)]
struct VhostVringState {
    index: u32,
    num: u32,
}

#[repr(C)]
struct VhostVringAddr {
    index: u32,
    flags: u32,
    desc_user_addr: u64,
    used_user_addr: u64,
    avail_user_addr: u64,
    log_guest_addr: u64,
}

use crate::vmm::snapshot::{QueueSnapshotState, VsockSnapshotState};

/// Queue state for virtio-vsock (rx=0, tx=1, event=2)
#[derive(Default)]
struct QueueState {
    num_max: u16,
    num: u16,
    ready: bool,
    desc_addr: u64,
    driver_addr: u64,
    device_addr: u64,
}

/// Virtio-vsock MMIO device backed by kernel vhost-vsock
pub struct VirtioVsockMmio {
    cid: u32,
    vhost_fd: Option<RawFd>,
    kick_eventfds: [Option<RawFd>; 3],
    _call_eventfds: [Option<RawFd>; 3],
    device_features: u64,
    driver_features: u64,
    features_sel: u32,
    queue_sel: u32,
    status: u32,
    interrupt_status: u32,
    config_generation: u32,
    rx_queue: QueueState,
    tx_queue: QueueState,
    event_queue: QueueState,
    mmio_base: u64,
    mmio_size: u64,
    vhost_attached: bool,
}

impl VirtioVsockMmio {
    pub fn new(cid: u32) -> Result<Self> {
        Self::new_with_require_vhost(cid, false)
    }

    pub fn new_with_require_vhost(cid: u32, require_vhost: bool) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid vsock CID {}: must be >= 3",
                cid
            )));
        }

        let (vhost_fd, kick_eventfds, call_eventfds) = Self::setup_vhost(cid, require_vhost)?;

        Ok(Self {
            cid,
            vhost_fd,
            kick_eventfds,
            _call_eventfds: call_eventfds,
            device_features: VIRTIO_F_VERSION_1,
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
            event_queue: QueueState {
                num_max: 256,
                ..Default::default()
            },
            mmio_base: 0,
            mmio_size: 0x200,
            vhost_attached: false,
        })
    }

    #[allow(clippy::type_complexity)]
    fn setup_vhost(
        cid: u32,
        require_vhost: bool,
    ) -> Result<(Option<RawFd>, [Option<RawFd>; 3], [Option<RawFd>; 3])> {
        let fd = match open(
            Path::new("/dev/vhost-vsock"),
            OFlags::RDWR | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(f) => f,
            Err(e) => {
                if require_vhost {
                    return Err(Error::Device(format!(
                        "vsock enabled but /dev/vhost-vsock unavailable: {}",
                        e
                    )));
                }
                debug!("vhost-vsock not available: {}", e);
                return Ok((None, [None, None, None], [None, None, None]));
            }
        };

        let raw_fd = fd.as_raw_fd();

        // SET_GUEST_CID
        const VHOST_VSOCK_SET_GUEST_CID: u64 = 0x4008AF60;
        let cid_val: u64 = cid as u64;
        let ret = unsafe { libc::ioctl(raw_fd, VHOST_VSOCK_SET_GUEST_CID as _, &cid_val) };
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::close(raw_fd);
            }
            return Err(Error::Device(format!(
                "VHOST_VSOCK_SET_GUEST_CID failed: {}",
                e
            )));
        }

        let mut kick = [None, None, None];
        let mut call = [None, None, None];
        for i in 0..3 {
            match eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC) {
                Ok(k) => {
                    let f = k.into_raw_fd();
                    kick[i] = Some(f);
                }
                Err(e) => {
                    for f in kick.iter().take(i).flatten() {
                        unsafe {
                            libc::close(*f);
                        }
                    }
                    unsafe {
                        libc::close(raw_fd);
                    }
                    return Err(Error::Device(format!("eventfd: {}", e)));
                }
            }
            match eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC) {
                Ok(c) => {
                    let f = c.into_raw_fd();
                    call[i] = Some(f);
                }
                Err(e) => {
                    for f in kick.iter().take(i + 1).flatten() {
                        unsafe {
                            libc::close(*f);
                        }
                    }
                    for f in call.iter().take(i).flatten() {
                        unsafe {
                            libc::close(*f);
                        }
                    }
                    unsafe {
                        libc::close(raw_fd);
                    }
                    return Err(Error::Device(format!("eventfd: {}", e)));
                }
            }
        }

        // Keep the fd open and transfer ownership to the device.
        let _ = fd.into_raw_fd();

        Ok((Some(raw_fd), kick, call))
    }

    pub fn set_mmio_base(&mut self, base: u64) {
        self.mmio_base = base;
        debug!("virtio-vsock MMIO base set to {:#x}", base);
    }

    pub fn mmio_base(&self) -> u64 {
        self.mmio_base
    }
    pub fn mmio_size(&self) -> u64 {
        self.mmio_size
    }

    /// Return the raw FDs for the call eventfds (used for IRQ injection).
    /// Index 0 = rx, 1 = tx; index 2 (event) may not be used by vhost-vsock.
    pub fn call_eventfds(&self) -> &[Option<RawFd>; 3] {
        &self._call_eventfds
    }

    /// Set interrupt status bits (called by the IRQ handler thread).
    /// The guest ISR reads this via MMIO to determine what happened.
    pub fn set_interrupt_status(&mut self, bits: u32) {
        self.interrupt_status |= bits;
    }

    pub fn handles_mmio(&self, addr: u64) -> bool {
        addr >= self.mmio_base && addr < self.mmio_base + self.mmio_size
    }

    fn current_queue(&self) -> &QueueState {
        match self.queue_sel {
            0 => &self.rx_queue,
            1 => &self.tx_queue,
            2 => &self.event_queue,
            _ => &self.rx_queue,
        }
    }
    fn current_queue_mut(&mut self) -> &mut QueueState {
        match self.queue_sel {
            0 => &mut self.rx_queue,
            1 => &mut self.tx_queue,
            2 => &mut self.event_queue,
            _ => &mut self.rx_queue,
        }
    }

    fn set_vhost_running(&self, running: bool) -> Result<()> {
        let fd = match self.vhost_fd {
            Some(f) => f,
            None => return Ok(()),
        };
        let val: std::ffi::c_int = if running { 1 } else { 0 };
        const VHOST_VSOCK_SET_RUNNING: u64 = 0x4004AF61;
        let ret = unsafe { libc::ioctl(fd, VHOST_VSOCK_SET_RUNNING as _, &val) };

        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_VSOCK_SET_RUNNING({}): {}",
                running,
                std::io::Error::last_os_error()
            )));
        }
        debug!("vhost-vsock SET_RUNNING({})", running);
        Ok(())
    }

    fn attach_vhost(&mut self, guest_memory: &vm_memory::GuestMemoryMmap) -> Result<()> {
        let fd = match self.vhost_fd {
            Some(f) => f,
            None => return Ok(()),
        };
        if self.vhost_attached {
            return Ok(());
        }

        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_OWNER as _) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            // EBUSY means owner already set - that's OK
            if err.raw_os_error() != Some(libc::EBUSY) {
                return Err(Error::Device(format!("VHOST_SET_OWNER: {}", err)));
            }
        }

        let nregions = guest_memory.iter().count() as u32;
        let size = 8 + nregions as usize * std::mem::size_of::<VhostMemoryRegion>();
        let mut buf = vec![0u8; size];
        let hdr = buf.as_mut_ptr() as *mut u32;
        unsafe {
            *hdr = nregions;
        }
        let regions_ptr = unsafe { buf.as_mut_ptr().add(8) as *mut VhostMemoryRegion };
        for (i, region) in guest_memory.iter().enumerate() {
            let host_addr = guest_memory
                .get_host_address(region.start_addr())
                .map_err(|e| Error::Device(format!("get_host_address: {}", e)))?;
            let reg = unsafe { &mut *regions_ptr.add(i) };
            reg.guest_phys_addr = region.start_addr().raw_value();
            reg.memory_size = region.len();
            reg.userspace_addr = host_addr as u64;
        }

        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_MEM_TABLE as _, buf.as_ptr()) };
        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_SET_MEM_TABLE: {}",
                std::io::Error::last_os_error()
            )));
        }

        self.vhost_attached = true;
        debug!("vhost-vsock attached (SET_OWNER + SET_MEM_TABLE)");

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn set_vring(
        &self,
        index: u32,
        num: u32,
        desc: u64,
        avail: u64,
        used: u64,
        guest_memory: &vm_memory::GuestMemoryMmap,
        kick_fd: RawFd,
        call_fd: RawFd,
    ) -> Result<()> {
        let fd = match self.vhost_fd {
            Some(f) => f,
            None => return Ok(()),
        };

        let desc_host = guest_memory
            .get_host_address(GuestAddress(desc))
            .map_err(|e| Error::Device(format!("desc host addr: {}", e)))?
            as u64;
        let avail_host = guest_memory
            .get_host_address(GuestAddress(avail))
            .map_err(|e| Error::Device(format!("avail host addr: {}", e)))?
            as u64;
        let used_host = guest_memory
            .get_host_address(GuestAddress(used))
            .map_err(|e| Error::Device(format!("used host addr: {}", e)))?
            as u64;

        let state = VhostVringState { index, num };
        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_VRING_NUM as _, &state) };
        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_SET_VRING_NUM: {}",
                std::io::Error::last_os_error()
            )));
        }

        let addr = VhostVringAddr {
            index,
            flags: 0,
            desc_user_addr: desc_host,
            used_user_addr: used_host,
            avail_user_addr: avail_host,
            log_guest_addr: 0,
        };
        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_VRING_ADDR as _, &addr) };
        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_SET_VRING_ADDR: {}",
                std::io::Error::last_os_error()
            )));
        }

        let base_state = VhostVringState { index, num: 0 };
        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_VRING_BASE as _, &base_state) };
        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_SET_VRING_BASE: {}",
                std::io::Error::last_os_error()
            )));
        }

        #[repr(C)]
        struct VhostVringFile {
            index: u32,
            fd: i32,
        }
        let kick_file = VhostVringFile { index, fd: kick_fd };
        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_VRING_KICK as _, &kick_file) };
        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_SET_VRING_KICK: {}",
                std::io::Error::last_os_error()
            )));
        }

        let call_file = VhostVringFile { index, fd: call_fd };
        let ret = unsafe { libc::ioctl(fd, vhost_ioctl::VHOST_SET_VRING_CALL as _, &call_file) };
        if ret < 0 {
            return Err(Error::Device(format!(
                "VHOST_SET_VRING_CALL: {}",
                std::io::Error::last_os_error()
            )));
        }

        debug!("vhost vring {} programmed (num={})", index, num);
        Ok(())
    }

    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
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
            mmio::QUEUE_NUM_MAX => self.current_queue().num_max as u32,
            mmio::QUEUE_READY => self.current_queue().ready as u32,
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
                trace!("virtio-vsock: unhandled MMIO read offset {:#x}", offset);
                0
            }
        };
        let bytes = value.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn mmio_write(
        &mut self,
        offset: u64,
        data: &[u8],
        guest_memory: &vm_memory::GuestMemoryMmap,
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
                self.current_queue_mut().num = value as u16;
            }
            mmio::QUEUE_READY => {
                let idx = self.queue_sel;
                let q = self.current_queue_mut();
                q.ready = value != 0;
                if q.ready {
                    let (num, desc, driver, device) =
                        (q.num as u32, q.desc_addr, q.driver_addr, q.device_addr);

                    if !self.vhost_attached {
                        self.attach_vhost(guest_memory)?;
                    }
                    let kick_fd = self.kick_eventfds[idx as usize]
                        .ok_or_else(|| Error::Device("no kick eventfd".into()))?;
                    let call_fd = self._call_eventfds[idx as usize]
                        .ok_or_else(|| Error::Device("no call eventfd".into()))?;
                    let set_vring_result = self.set_vring(
                        idx,
                        num,
                        desc,
                        driver,
                        device,
                        guest_memory,
                        kick_fd,
                        call_fd,
                    );
                    if let Err(e) = set_vring_result {
                        // Some host kernels reject programming queue 2 (EVENT)
                        // in vhost-vsock even though the virtio spec advertises it.
                        // Keep RX/TX online and continue so guest-agent can boot.
                        if idx == 2 {
                            debug!("virtio-vsock: ignoring event queue setup failure: {}", e);
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
            mmio::QUEUE_NOTIFY => {
                self.notify_queue(value);
            }
            mmio::INTERRUPT_ACK => self.interrupt_status &= !value,
            mmio::STATUS => {
                self.status = value;

                if value == 0 {
                    let _ = self.set_vhost_running(false);
                    self.reset();
                } else if (value & 4) != 0 {
                    let _ = self.set_vhost_running(true);
                }
            }
            mmio::QUEUE_DESC_LOW => {
                self.current_queue_mut().desc_addr =
                    (self.current_queue().desc_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DESC_HIGH => {
                self.current_queue_mut().desc_addr = (self.current_queue().desc_addr
                    & 0x0000_0000_FFFF_FFFF)
                    | ((value as u64) << 32);
            }
            mmio::QUEUE_DRIVER_LOW => {
                self.current_queue_mut().driver_addr =
                    (self.current_queue().driver_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DRIVER_HIGH => {
                self.current_queue_mut().driver_addr = (self.current_queue().driver_addr
                    & 0x0000_0000_FFFF_FFFF)
                    | ((value as u64) << 32);
            }
            mmio::QUEUE_DEVICE_LOW => {
                self.current_queue_mut().device_addr =
                    (self.current_queue().device_addr & 0xFFFF_FFFF_0000_0000) | (value as u64);
            }
            mmio::QUEUE_DEVICE_HIGH => {
                self.current_queue_mut().device_addr = (self.current_queue().device_addr
                    & 0x0000_0000_FFFF_FFFF)
                    | ((value as u64) << 32);
            }
            _ => {
                trace!(
                    "virtio-vsock: unhandled MMIO write offset {:#x} value={:#x}",
                    offset,
                    value
                );
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        debug!("virtio-vsock: device reset");
        self.status = 0;
        self.interrupt_status = 0;
        self.driver_features = 0;
        self.rx_queue = QueueState {
            num_max: 256,
            ..Default::default()
        };
        self.tx_queue = QueueState {
            num_max: 256,
            ..Default::default()
        };
        self.event_queue = QueueState {
            num_max: 256,
            ..Default::default()
        };
    }

    pub fn notify_queue(&mut self, queue_index: u32) {
        if queue_index < 3 {
            if let Some(fd) = self.kick_eventfds[queue_index as usize] {
                let val: u64 = 1;
                let ret = unsafe {
                    libc::write(
                        fd,
                        &val as *const _ as *const libc::c_void,
                        std::mem::size_of::<u64>(),
                    )
                };
                if ret < 0 {
                    trace!("virtio-vsock: kick write failed for queue {}", queue_index);
                }
            }
        } else {
            warn!("virtio-vsock: invalid queue notify {}", queue_index);
        }
    }

    // ------------------------------------------------------------------
    // Snapshot support
    // ------------------------------------------------------------------

    /// Capture the device software state for snapshotting.
    ///
    /// FDs (vhost, eventfds) are NOT included — they are re-created on restore.
    pub fn snapshot_state(&self) -> VsockSnapshotState {
        let queues = [&self.rx_queue, &self.tx_queue, &self.event_queue]
            .iter()
            .map(|q| QueueSnapshotState {
                num_max: q.num_max,
                num: q.num,
                ready: q.ready,
                desc_addr: q.desc_addr,
                driver_addr: q.driver_addr,
                device_addr: q.device_addr,
                last_avail_idx: None,
                last_used_idx: None,
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

    /// Restore a virtio-vsock MMIO device from snapshot state.
    ///
    /// Creates a new vhost-vsock FD with the given `cid`, restores software
    /// state from the snapshot, and replays vhost ioctls to re-program the
    /// vring addresses using the restored guest memory.
    pub fn restore(
        state: &VsockSnapshotState,
        cid: u32,
        guest_memory: &vm_memory::GuestMemoryMmap,
    ) -> Result<Self> {
        let (vhost_fd, kick_eventfds, call_eventfds) = Self::setup_vhost(cid, true)?;

        // Restore queue software state from snapshot.
        fn restore_queue(snap: &QueueSnapshotState) -> QueueState {
            QueueState {
                num_max: snap.num_max,
                num: snap.num,
                ready: snap.ready,
                desc_addr: snap.desc_addr,
                driver_addr: snap.driver_addr,
                device_addr: snap.device_addr,
            }
        }

        let rx_queue = state.queues.first().map(restore_queue).unwrap_or_default();
        let tx_queue = state.queues.get(1).map(restore_queue).unwrap_or_default();
        let event_queue = state.queues.get(2).map(restore_queue).unwrap_or_default();

        let mut dev = Self {
            cid,
            vhost_fd,
            kick_eventfds,
            _call_eventfds: call_eventfds,
            device_features: state.device_features,
            driver_features: state.driver_features,
            features_sel: state.features_sel,
            queue_sel: state.queue_sel,
            status: state.status,
            interrupt_status: state.interrupt_status,
            config_generation: state.config_generation,
            rx_queue,
            tx_queue,
            event_queue,
            mmio_base: 0,
            mmio_size: 0x200,
            vhost_attached: false,
        };

        // Re-attach vhost backend with guest memory
        dev.attach_vhost(guest_memory)?;

        // Re-program vrings for each ready queue
        let queues = [&dev.rx_queue, &dev.tx_queue, &dev.event_queue];
        for (idx, q) in queues.iter().enumerate() {
            if !q.ready {
                continue;
            }
            let kick_fd = dev.kick_eventfds[idx]
                .ok_or_else(|| Error::Device("no kick eventfd on restore".into()))?;
            let call_fd = dev._call_eventfds[idx]
                .ok_or_else(|| Error::Device("no call eventfd on restore".into()))?;
            let result = dev.set_vring(
                idx as u32,
                q.num as u32,
                q.desc_addr,
                q.driver_addr,
                q.device_addr,
                guest_memory,
                kick_fd,
                call_fd,
            );
            if let Err(e) = result {
                if idx == 2 {
                    debug!("virtio-vsock restore: ignoring event queue setup: {}", e);
                } else {
                    return Err(e);
                }
            }
        }

        // Resume vhost-vsock if the device was in DRIVER_OK state
        if (dev.status & 4) != 0 {
            dev.set_vhost_running(true)?;
        }

        debug!("Restored virtio-vsock MMIO (CID {})", cid);
        Ok(dev)
    }
}

impl crate::devices::vsock_backend::VsockMmioDevice for VirtioVsockMmio {
    fn mmio_base(&self) -> u64 {
        self.mmio_base
    }
    fn mmio_size(&self) -> u64 {
        self.mmio_size
    }
    fn set_mmio_base(&mut self, base: u64) {
        self.set_mmio_base(base);
    }
    fn handles_mmio(&self, addr: u64) -> bool {
        self.handles_mmio(addr)
    }
    fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        self.mmio_read(offset, data);
    }
    fn mmio_write(
        &mut self,
        offset: u64,
        data: &[u8],
        guest_memory: &vm_memory::GuestMemoryMmap,
    ) -> crate::Result<()> {
        self.mmio_write(offset, data, guest_memory)
    }
    fn call_eventfds(&self) -> &[Option<RawFd>; 3] {
        self.call_eventfds()
    }
    fn set_interrupt_status(&mut self, bits: u32) {
        self.set_interrupt_status(bits);
    }
    fn snapshot_state(&self) -> crate::vmm::snapshot::VsockSnapshotState {
        self.snapshot_state()
    }
}

impl Drop for VirtioVsockMmio {
    fn drop(&mut self) {
        if let Some(fd) = self.vhost_fd {
            unsafe {
                libc::close(fd);
            }
        }
        for f in self.kick_eventfds.iter().flatten() {
            unsafe {
                libc::close(*f);
            }
        }
        for f in self._call_eventfds.iter().flatten() {
            unsafe {
                libc::close(*f);
            }
        }
    }
}
