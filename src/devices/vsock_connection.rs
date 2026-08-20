//! Vsock connection state machine for the userspace backend.
//!
//! Manages active connections, maps guest AF_VSOCK ports to host AF_UNIX
//! streams. Processes vsock header packets (TX from guest) and generates
//! RX packets (to guest).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, trace, warn};

use crate::Result;

/// Cap on concurrent host-side connections — established plus those
/// still owed their port prefix. The listener is serviced even while
/// the guest driver is down, so without a cap a host process holding
/// connections open would grow the map and fd set without bound.
/// Accepts beyond the cap are dropped, closing the stream.
const MAX_HOST_CONNECTIONS: usize = 128;

/// How long an accepted host connection may take to deliver its 4-byte
/// port prefix before it is dropped.
const PORT_PREFIX_DEADLINE: Duration = Duration::from_secs(2);

/// Per-connection cap on bytes read off a host stream but not yet
/// forwarded to the guest (`tx_buf`). At the cap the sweep stops
/// reading that stream, so backpressure lands in the socket buffer
/// (the client's writes block or hit their send timeout) instead of
/// growing host memory while the guest is not draining. Reading — and
/// with it host-side close detection — resumes once the buffer drains.
const HOST_TO_GUEST_BUFFER_CAP: usize = 256 * 1024;

/// Cap on total guest-bound packet data queued in `rx_queue`, the
/// other place host data piles up while the driver is down. A live
/// guest never approaches it; hitting it pauses host-stream reads for
/// a sweep.
const RX_QUEUE_BACKLOG_CAP: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Vsock packet header (virtio-vsock spec §5.10)
// ---------------------------------------------------------------------------

/// Size of a virtio-vsock packet header.
pub const VSOCK_HEADER_SIZE: usize = 44;

/// Host CID (always 2 in vsock spec).
pub const HOST_CID: u64 = 2;

/// Vsock operation codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum VsockOp {
    Invalid = 0,
    Request = 1,
    Response = 2,
    Rst = 3,
    Shutdown = 4,
    Rw = 5,
    CreditUpdate = 6,
    CreditRequest = 7,
}

impl VsockOp {
    pub fn from_u16(v: u16) -> Self {
        match v {
            1 => VsockOp::Request,
            2 => VsockOp::Response,
            3 => VsockOp::Rst,
            4 => VsockOp::Shutdown,
            5 => VsockOp::Rw,
            6 => VsockOp::CreditUpdate,
            7 => VsockOp::CreditRequest,
            _ => VsockOp::Invalid,
        }
    }
}

/// Vsock packet header fields (little-endian on wire).
#[derive(Debug, Clone)]
pub struct VsockHeader {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub r#type: u16, // always 1 = STREAM
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl VsockHeader {
    /// Parse a vsock header from a 44-byte buffer.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < VSOCK_HEADER_SIZE {
            return None;
        }
        Some(Self {
            src_cid: u64::from_le_bytes(buf[0..8].try_into().ok()?),
            dst_cid: u64::from_le_bytes(buf[8..16].try_into().ok()?),
            src_port: u32::from_le_bytes(buf[16..20].try_into().ok()?),
            dst_port: u32::from_le_bytes(buf[20..24].try_into().ok()?),
            len: u32::from_le_bytes(buf[24..28].try_into().ok()?),
            r#type: u16::from_le_bytes(buf[28..30].try_into().ok()?),
            op: u16::from_le_bytes(buf[30..32].try_into().ok()?),
            flags: u32::from_le_bytes(buf[32..36].try_into().ok()?),
            buf_alloc: u32::from_le_bytes(buf[36..40].try_into().ok()?),
            fwd_cnt: u32::from_le_bytes(buf[40..44].try_into().ok()?),
        })
    }

    /// Serialize the header to a 44-byte buffer.
    pub fn to_bytes(&self) -> [u8; VSOCK_HEADER_SIZE] {
        let mut buf = [0u8; VSOCK_HEADER_SIZE];
        buf[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        buf[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        buf[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        buf[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        buf[24..28].copy_from_slice(&self.len.to_le_bytes());
        buf[28..30].copy_from_slice(&self.r#type.to_le_bytes());
        buf[30..32].copy_from_slice(&self.op.to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags.to_le_bytes());
        buf[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        buf[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        buf
    }
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// State of a single vsock connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    /// OP_REQUEST received, OP_RESPONSE queued.
    Connecting,
    /// Connection established, data flowing.
    Connected,
    /// OP_SHUTDOWN received, waiting for RST.
    Closing,
}

/// Result of polling a connection's host stream for data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostReadOutcome {
    /// Bytes were read into `tx_buf`.
    Data(usize),
    /// The stream has no data available right now.
    NoData,
    /// The host application closed its end (EOF or hard error).
    Closed,
}

/// Per-connection state for the userspace vsock backend.
pub struct VsockConnection {
    pub state: ConnState,
    /// Guest-side port (src_port in guest TX).
    pub guest_port: u32,
    /// Host-side port (dst_port in guest TX).
    pub host_port: u32,
    /// Unix stream to the host application.
    pub stream: UnixStream,
    /// Guest's buffer allocation (credit flow control).
    pub peer_buf_alloc: u32,
    /// Guest's forward count (bytes guest has consumed from us).
    pub peer_fwd_cnt: u32,
    /// Our buffer allocation advertised to the guest.
    pub buf_alloc: u32,
    /// Bytes we have forwarded to the guest.
    pub fwd_cnt: u32,
    /// Bytes the guest has sent to us (for credit tracking).
    pub rx_cnt: u32,
    /// Pending data to send to the guest (buffered from host stream).
    pub tx_buf: Vec<u8>,
    /// Guest→host bytes accepted from the guest but not yet fully
    /// written to the host stream — the remainder of a short write on
    /// the non-blocking stream. Private so it moves in lockstep with
    /// `fwd_cnt`: credit advances only when bytes actually reach the
    /// stream.
    ///
    /// Bytes before `host_write_pos` are already delivered. Flushing
    /// advances the cursor instead of draining the front, so the flush
    /// path never memmoves the backlog while the worker holds the
    /// connection-map lock the vCPU threads need; the buffer is
    /// compacted only when the next packet arrives.
    host_write_buf: Vec<u8>,
    /// Cursor into `host_write_buf`: bytes before it are delivered.
    host_write_pos: usize,
}

impl VsockConnection {
    pub fn new(guest_port: u32, host_port: u32, stream: UnixStream) -> Self {
        // Set non-blocking on the stream so we can poll it
        let _ = stream.set_nonblocking(true);
        Self {
            state: ConnState::Connecting,
            guest_port,
            host_port,
            stream,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            buf_alloc: 256 * 1024, // 256 KiB
            fwd_cnt: 0,
            rx_cnt: 0,
            tx_buf: Vec::new(),
            host_write_buf: Vec::new(),
            host_write_pos: 0,
        }
    }

    /// How many bytes of credit the guest has available for us to send.
    pub fn peer_free(&self) -> u32 {
        self.peer_buf_alloc
            .wrapping_sub(self.rx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }

    /// Raw fd of the host stream (for epoll).
    pub fn stream_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Read available data from the host stream into tx_buf.
    ///
    /// Distinguishes "no data right now" from "the host application closed
    /// its end": a closed connection must be removed from the map, or its
    /// fd stays registered in the worker's level-triggered epoll set where
    /// EOF reads as perpetually-ready and turns the poll loop into a busy
    /// spin.
    pub fn read_from_host(&mut self) -> HostReadOutcome {
        let mut buf = [0u8; 65536];
        match self.stream.read(&mut buf) {
            Ok(0) => HostReadOutcome::Closed,
            Ok(n) => {
                self.tx_buf.extend_from_slice(&buf[..n]);
                HostReadOutcome::Data(n)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => HostReadOutcome::NoData,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => HostReadOutcome::NoData,
            Err(_) => HostReadOutcome::Closed,
        }
    }

    /// Deliver guest→host bytes to the host stream, preserving order.
    ///
    /// A single non-blocking `write` can accept only part of a packet;
    /// treating `Ok(n)` as full delivery silently drops the tail of the
    /// frame, and the host's multiplex reader then stalls mid-frame
    /// waiting for bytes that never arrive. Bytes the stream does not
    /// accept immediately are therefore buffered and re-attempted by
    /// [`flush_host_writes`](Self::flush_host_writes) on the worker's
    /// next sweep.
    ///
    /// `fwd_cnt` advances only for bytes actually written to the stream,
    /// so buffered bytes stay counted against the credit advertised to
    /// the guest and a stalled host application throttles the guest
    /// instead of growing the buffer without bound.
    ///
    /// # Errors
    ///
    /// Returns an error if the stream write fails (the connection must
    /// be reset), or if buffering `data` would exceed the advertised
    /// `buf_alloc`. A conforming guest never trips the credit bound —
    /// it cannot have more than `buf_alloc` bytes outstanding — so the
    /// check exists to cap host memory against an untrusted guest that
    /// ignores credit.
    pub fn write_to_host(&mut self, data: &[u8]) -> std::io::Result<()> {
        if self.host_write_pos > 0 {
            self.host_write_buf.drain(..self.host_write_pos);
            self.host_write_pos = 0;
        }
        if self.host_write_buf.len() + data.len() > self.buf_alloc as usize {
            return Err(std::io::Error::other(format!(
                "guest exceeded advertised credit: {} buffered + {} new > buf_alloc {}",
                self.host_write_buf.len(),
                data.len(),
                self.buf_alloc
            )));
        }
        self.host_write_buf.extend_from_slice(data);
        self.flush_host_writes().map(|_| ())
    }

    /// Attempts to deliver buffered guest→host bytes to the host stream,
    /// advancing `fwd_cnt` by the bytes delivered.
    ///
    /// Returns the number of bytes delivered by this call, so the
    /// worker sweep can announce credit freed by a deferred flush.
    /// Stops without error when the stream signals `WouldBlock`; the
    /// worker retries on its next sweep.
    ///
    /// # Errors
    ///
    /// Returns an error if the stream write fails or reports
    /// end-of-stream; the caller must reset the connection.
    pub fn flush_host_writes(&mut self) -> std::io::Result<usize> {
        let mut delivered = 0usize;
        while self.host_write_pos < self.host_write_buf.len() {
            match self
                .stream
                .write(&self.host_write_buf[self.host_write_pos..])
            {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "host stream accepted no bytes",
                    ));
                }
                Ok(written) => {
                    self.host_write_pos += written;
                    delivered += written;
                    self.fwd_cnt = self.fwd_cnt.wrapping_add(written as u32);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.host_write_pos == self.host_write_buf.len() {
            self.host_write_buf.clear();
            self.host_write_pos = 0;
        }
        Ok(delivered)
    }

    /// Whether guest→host bytes are still waiting on the host stream.
    pub fn has_pending_host_writes(&self) -> bool {
        self.host_write_pos < self.host_write_buf.len()
    }
}

// ---------------------------------------------------------------------------
// Connection map
// ---------------------------------------------------------------------------

/// Key for the connection map: (guest_port, host_port).
pub type ConnKey = (u32, u32);

/// An accepted host connection that has not yet delivered the 4-byte
/// prefix naming its target guest port. The stream is non-blocking and
/// the prefix is read incrementally, so a client that stalls
/// mid-prefix never blocks the sweep (which runs under the
/// connection-map lock the vCPU MMIO paths also need).
struct PendingAccept {
    stream: UnixStream,
    port_buf: [u8; 4],
    filled: usize,
    accepted_at: Instant,
}

/// What a sweep decided about one pending accept.
enum PendingOutcome {
    /// Prefix complete — promote to a connection.
    Ready,
    /// Still waiting on prefix bytes, within the deadline.
    Keep,
    /// Closed, errored, or overran the deadline: drop it.
    Discard(&'static str),
}

/// Manages all active vsock connections and the Unix listener socket.
pub struct VsockConnectionMap {
    /// CID of the guest.
    pub guest_cid: u64,
    /// Active connections keyed by (guest_port, host_port).
    pub connections: HashMap<ConnKey, VsockConnection>,
    /// Unix listener for incoming host connections.
    pub listener: Option<UnixListener>,
    /// Path to the Unix socket for cleanup.
    pub socket_path: Option<PathBuf>,
    /// Pending RX packets to inject into the guest (header + optional data).
    pub rx_queue: Vec<(VsockHeader, Vec<u8>)>,
    /// Accepted host connections still owed their port prefix.
    pending_accepts: Vec<PendingAccept>,
    /// Set once a poisoned map mutex has been recovered, so the
    /// recovery warning fires once per device rather than per sweep.
    pub poison_warned: std::sync::atomic::AtomicBool,
}

impl VsockConnectionMap {
    /// Create a new connection map with a Unix listener.
    pub fn new(guest_cid: u64, socket_path: &Path) -> Result<Self> {
        // Remove stale socket
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            crate::Error::Device(format!(
                "bind vsock Unix socket {}: {}",
                socket_path.display(),
                e
            ))
        })?;

        // Restrict socket permissions to owner-only
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).map_err(
            |e| {
                crate::Error::Device(format!(
                    "chmod vsock socket {}: {}",
                    socket_path.display(),
                    e
                ))
            },
        )?;

        listener.set_nonblocking(true).map_err(|e| {
            crate::Error::Device(format!("set_nonblocking on vsock listener: {}", e))
        })?;

        debug!(
            "Vsock userspace listener at {} (CID {})",
            socket_path.display(),
            guest_cid
        );

        Ok(Self {
            guest_cid,
            connections: HashMap::new(),
            listener: Some(listener),
            socket_path: Some(socket_path.to_path_buf()),
            rx_queue: Vec::new(),
            pending_accepts: Vec::new(),
            poison_warned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Create a connection map without a listener (for testing or restore).
    pub fn new_without_listener(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            connections: HashMap::new(),
            listener: None,
            socket_path: None,
            rx_queue: Vec::new(),
            pending_accepts: Vec::new(),
            poison_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Service the Unix listener: accept every queued connection, then
    /// poll connections still owed their 4-byte LE port prefix (the
    /// first thing a host application sends, naming the guest port),
    /// promoting completed ones to real connections.
    ///
    /// Wholly non-blocking. Returns the established fds for epoll
    /// registration; establishing queues an `OP_REQUEST`, so a
    /// non-empty return means the guest should be signaled.
    pub fn service_listener(&mut self) -> Vec<RawFd> {
        while let Some(listener) = self.listener.as_ref() {
            match listener.accept() {
                Ok((stream, _)) => {
                    if self.connections.len() + self.pending_accepts.len() >= MAX_HOST_CONNECTIONS {
                        warn!(
                            "vsock: dropping accepted host connection: \
                             {} connections already open (cap {})",
                            self.connections.len() + self.pending_accepts.len(),
                            MAX_HOST_CONNECTIONS
                        );
                        continue;
                    }
                    let _ = stream.set_nonblocking(true);
                    self.pending_accepts.push(PendingAccept {
                        stream,
                        port_buf: [0u8; 4],
                        filled: 0,
                        accepted_at: Instant::now(),
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("vsock listener accept error: {}", e);
                    break;
                }
            }
        }
        self.poll_pending_accepts()
    }

    /// Poll every pending accept's prefix read once, promoting completed
    /// prefixes to connections and dropping closed, errored, or
    /// deadline-overrunning ones. Returns the established fds.
    fn poll_pending_accepts(&mut self) -> Vec<RawFd> {
        let mut established = Vec::new();
        let mut index = 0;
        while index < self.pending_accepts.len() {
            let pending = &mut self.pending_accepts[index];
            let outcome = loop {
                if pending.filled == pending.port_buf.len() {
                    break PendingOutcome::Ready;
                }
                match pending.stream.read(&mut pending.port_buf[pending.filled..]) {
                    Ok(0) => break PendingOutcome::Discard("closed before sending its port"),
                    Ok(bytes_read) => pending.filled += bytes_read,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if pending.accepted_at.elapsed() >= PORT_PREFIX_DEADLINE {
                            break PendingOutcome::Discard("did not send its port in time");
                        }
                        break PendingOutcome::Keep;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break PendingOutcome::Discard("read error before sending its port"),
                }
            };
            match outcome {
                PendingOutcome::Keep => index += 1,
                PendingOutcome::Discard(reason) => {
                    warn!("vsock: dropping accepted host connection: {}", reason);
                    self.pending_accepts.swap_remove(index);
                }
                PendingOutcome::Ready => {
                    let ready = self.pending_accepts.swap_remove(index);
                    let guest_port = u32::from_le_bytes(ready.port_buf);
                    established.push(self.establish_connection(ready.stream, guest_port));
                }
            }
        }
        established
    }

    /// Register an accepted stream as a connection to `guest_port`,
    /// queueing the `OP_REQUEST` that asks the guest to accept it.
    fn establish_connection(&mut self, stream: UnixStream, guest_port: u32) -> RawFd {
        let host_port = self.next_ephemeral_port();
        let fd = stream.as_raw_fd();
        let conn = VsockConnection::new(guest_port, host_port, stream);

        debug!(
            "vsock: accepted host connection for guest port {} (host_port={})",
            guest_port, host_port
        );
        let hdr = VsockHeader {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            len: 0,
            r#type: 1, // STREAM
            op: VsockOp::Request as u16,
            flags: 0,
            buf_alloc: conn.buf_alloc,
            fwd_cnt: conn.fwd_cnt,
        };
        self.rx_queue.push((hdr, Vec::new()));

        self.connections.insert((guest_port, host_port), conn);
        fd
    }

    /// Process a TX packet from the guest.
    ///
    /// Returns true if the packet was handled, false if it should be dropped.
    pub fn process_guest_tx(&mut self, hdr: &VsockHeader, data: &[u8]) -> bool {
        let op = VsockOp::from_u16(hdr.op);
        let key = (hdr.src_port, hdr.dst_port);

        match op {
            VsockOp::Response => {
                // Guest accepted our OP_REQUEST
                let resolved_key = if self.connections.contains_key(&(hdr.dst_port, hdr.src_port)) {
                    Some((hdr.dst_port, hdr.src_port))
                } else if self.connections.contains_key(&key) {
                    Some(key)
                } else {
                    None
                };

                if let Some(rk) = resolved_key {
                    if let Some(conn) = self.connections.get_mut(&rk) {
                        conn.state = ConnState::Connected;
                        conn.peer_buf_alloc = hdr.buf_alloc;
                        conn.peer_fwd_cnt = hdr.fwd_cnt;
                        debug!(
                            "vsock: connection established guest_port={} host_port={}",
                            rk.0, rk.1
                        );
                    }
                    // Flush any data the host sent while the connection was
                    // still in Connecting state.
                    self.flush_tx_buf(rk.0, rk.1);
                    true
                } else {
                    warn!("vsock: OP_RESPONSE for unknown connection {:?}", key);
                    false
                }
            }
            VsockOp::Rw => {
                // Data from guest to host
                let rw_key = if self.connections.contains_key(&key) {
                    key
                } else {
                    (hdr.dst_port, hdr.src_port)
                };

                let had_buffered = {
                    let Some(conn) = self.connections.get_mut(&rw_key) else {
                        // Unknown connection — send RST (handled in outer else)
                        self.queue_rst(hdr);
                        return true;
                    };
                    if conn.state == ConnState::Connected && !data.is_empty() {
                        match conn.write_to_host(data) {
                            Ok(()) => {
                                trace!(
                                    "vsock: accepted {} bytes for host (port={})",
                                    data.len(),
                                    hdr.dst_port
                                );
                            }
                            Err(e) => {
                                warn!("vsock: write to host failed: {}", e);
                                self.queue_rst(hdr);
                                self.connections.remove(&rw_key);
                                return true;
                            }
                        }
                    }
                    // Update peer credit info
                    let had_buf = !conn.tx_buf.is_empty();
                    conn.peer_buf_alloc = hdr.buf_alloc;
                    conn.peer_fwd_cnt = hdr.fwd_cnt;
                    had_buf
                };
                if had_buffered {
                    self.flush_tx_buf(rw_key.0, rw_key.1);
                }
                true
            }
            VsockOp::Shutdown => {
                let sd_key = if self.connections.contains_key(&key) {
                    key
                } else {
                    (hdr.dst_port, hdr.src_port)
                };

                if let Some(conn) = self.connections.get_mut(&sd_key) {
                    conn.state = ConnState::Closing;
                    debug!("vsock: guest shutdown port={}", hdr.src_port);
                }
                // Send RST back
                self.queue_rst(hdr);
                // Remove connection
                self.connections.remove(&key);
                self.connections.remove(&(hdr.dst_port, hdr.src_port));
                true
            }
            VsockOp::Rst => {
                debug!("vsock: guest RST port={}", hdr.src_port);
                self.connections.remove(&key);
                self.connections.remove(&(hdr.dst_port, hdr.src_port));
                true
            }
            VsockOp::CreditUpdate => {
                let cu_key = if self.connections.contains_key(&key) {
                    key
                } else {
                    (hdr.dst_port, hdr.src_port)
                };
                if let Some(conn) = self.connections.get_mut(&cu_key) {
                    conn.peer_buf_alloc = hdr.buf_alloc;
                    conn.peer_fwd_cnt = hdr.fwd_cnt;
                    trace!(
                        "vsock: credit update peer_buf_alloc={} peer_fwd_cnt={}",
                        hdr.buf_alloc,
                        hdr.fwd_cnt
                    );
                }
                true
            }
            VsockOp::CreditRequest => {
                // Guest wants our credit info — send CreditUpdate
                let cr_key = if self.connections.contains_key(&key) {
                    key
                } else {
                    (hdr.dst_port, hdr.src_port)
                };
                if let Some(conn) = self.connections.get(&cr_key) {
                    let reply = VsockHeader {
                        src_cid: HOST_CID,
                        dst_cid: self.guest_cid,
                        src_port: hdr.dst_port,
                        dst_port: hdr.src_port,
                        len: 0,
                        r#type: 1,
                        op: VsockOp::CreditUpdate as u16,
                        flags: 0,
                        buf_alloc: conn.buf_alloc,
                        fwd_cnt: conn.fwd_cnt,
                    };
                    self.rx_queue.push((reply, Vec::new()));
                }
                true
            }
            VsockOp::Request => {
                // Guest-initiated connection (unexpected for our use case, but handle it)
                debug!(
                    "vsock: guest-initiated connection request to port {}",
                    hdr.dst_port
                );
                // RST — we don't accept guest-initiated connections
                self.queue_rst(hdr);
                true
            }
            _ => {
                warn!("vsock: unknown op {}", hdr.op);
                false
            }
        }
    }

    /// Queue data from a host stream to send to the guest.
    ///
    /// Call this after reading data from a host UnixStream. Splits the
    /// payload into virtio-vsock-sized packets (4 KiB each) and respects
    /// the peer's available credit. If credit is insufficient, the
    /// remainder is buffered in `conn.tx_buf` for later delivery.
    pub fn queue_host_data(&mut self, guest_port: u32, host_port: u32, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let key = (guest_port, host_port);
        if !self.connections.contains_key(&key) {
            return;
        }

        const PACKET_CAP: usize = 4096;
        let mut offset = 0;

        while offset < data.len() {
            let conn = match self.connections.get_mut(&key) {
                Some(c) => c,
                None => return,
            };
            let max_credit = conn.peer_free() as usize;
            let remaining = data.len() - offset;
            let send_len = remaining.min(max_credit).min(PACKET_CAP);

            if send_len == 0 {
                // Out of peer credit; buffer the remainder so it flushes
                // when the guest updates credit.
                conn.tx_buf.extend_from_slice(&data[offset..]);
                return;
            }

            let buf_alloc = conn.buf_alloc;
            let fwd_cnt = conn.fwd_cnt;

            let hdr = VsockHeader {
                src_cid: HOST_CID,
                dst_cid: self.guest_cid,
                src_port: host_port,
                dst_port: guest_port,
                len: send_len as u32,
                r#type: 1,
                op: VsockOp::Rw as u16,
                flags: 0,
                buf_alloc,
                fwd_cnt,
            };
            self.rx_queue
                .push((hdr, data[offset..offset + send_len].to_vec()));
            offset += send_len;
        }
    }

    /// Take all pending RX packets to inject into the guest.
    pub fn drain_rx(&mut self) -> Vec<(VsockHeader, Vec<u8>)> {
        std::mem::take(&mut self.rx_queue)
    }

    /// Check if there are pending RX packets.
    pub fn has_pending_rx(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    /// Flush any data buffered in `tx_buf` for a connection that just
    /// transitioned to Connected.  The host may have sent data (e.g. a Ping)
    /// while the OP_REQUEST→OP_RESPONSE handshake was still in progress.
    fn flush_tx_buf(&mut self, guest_port: u32, host_port: u32) {
        let key = (guest_port, host_port);
        let data = if let Some(conn) = self.connections.get_mut(&key) {
            if conn.state != ConnState::Connected || conn.tx_buf.is_empty() {
                return;
            }
            std::mem::take(&mut conn.tx_buf)
        } else {
            return;
        };

        if !data.is_empty() {
            debug!(
                "vsock: flushing {} bytes buffered during Connecting (port={})",
                data.len(),
                guest_port
            );
            self.queue_host_data(guest_port, host_port, &data);
        }
    }

    fn queue_rst(&mut self, original: &VsockHeader) {
        let rst = VsockHeader {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: original.dst_port,
            dst_port: original.src_port,
            len: 0,
            r#type: 1,
            op: VsockOp::Rst as u16,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        };
        self.rx_queue.push((rst, Vec::new()));
    }

    /// Tear down a connection whose host application is gone: queue an RST
    /// so the guest releases its side, and remove the entry — dropping its
    /// stream closes the fd, which also deregisters it from any epoll set.
    fn close_host_connection(&mut self, guest_port: u32, host_port: u32) {
        debug!(
            "vsock: host side closed, resetting connection guest_port={} host_port={}",
            guest_port, host_port
        );
        let rst = VsockHeader {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            len: 0,
            r#type: 1,
            op: VsockOp::Rst as u16,
            flags: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        };
        self.rx_queue.push((rst, Vec::new()));
        self.connections.remove(&(guest_port, host_port));
    }

    /// Poll every connection's host stream once — regardless of state:
    /// retry buffered guest→host writes, forward data on established
    /// connections to the guest, buffer data read during `Connecting` in
    /// `tx_buf` (forwarded by `flush_tx_buf` when the guest's
    /// OP_RESPONSE lands), and reap connections whose host application
    /// closed.
    ///
    /// Reaping here is load-bearing, not hygiene, and it must cover every
    /// state. A closed-but-unreaped stream stays in the worker's
    /// level-triggered epoll set where EOF reads as perpetually ready, so
    /// each leaked connection turns the worker's poll loop into a busy
    /// spin that starves the vCPU threads. Abandoned control-channel
    /// handshake attempts create such connections on every boot, and an
    /// abandoned attempt whose OP_RESPONSE never completes leaves its
    /// connection parked in `Connecting` — a sweep restricted to
    /// established connections can never reap it.
    ///
    /// Returns `true` if anything was queued for the guest (data or RSTs),
    /// i.e. the guest should be signaled.
    pub fn drain_host_streams(&mut self) -> bool {
        // Guest-bound backlog snapshot for this sweep's read decisions;
        // data queued by this sweep is not re-counted, so the cap can
        // overshoot by at most one read per connection.
        let rx_backlog: usize = self.rx_queue.iter().map(|(_, data)| data.len()).sum();
        let mut forwards: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let mut closed: Vec<(u32, u32)> = Vec::new();
        let mut credit_updates: Vec<(u32, u32)> = Vec::new();
        for ((guest_port, host_port), conn) in self.connections.iter_mut() {
            if conn.has_pending_host_writes() {
                match conn.flush_host_writes() {
                    // A deferred flush advances fwd_cnt with no other
                    // packet to carry it, and the guest may be parked
                    // on exhausted credit waiting for exactly this —
                    // announce the freed credit unsolicited.
                    Ok(delivered) if delivered > 0 => {
                        credit_updates.push((*guest_port, *host_port));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("vsock: flushing buffered host write failed: {}", e);
                        closed.push((*guest_port, *host_port));
                        continue;
                    }
                }
            }
            // A capped connection is not read this sweep, pushing
            // backpressure into its socket buffer (see the cap consts).
            if conn.tx_buf.len() >= HOST_TO_GUEST_BUFFER_CAP || rx_backlog >= RX_QUEUE_BACKLOG_CAP {
                continue;
            }
            match conn.read_from_host() {
                HostReadOutcome::Data(_) => {
                    if conn.state == ConnState::Connected {
                        let data = std::mem::take(&mut conn.tx_buf);
                        forwards.push((*guest_port, *host_port, data));
                    }
                }
                HostReadOutcome::NoData => {}
                HostReadOutcome::Closed => closed.push((*guest_port, *host_port)),
            }
        }
        let mut queued_for_guest = false;
        for (guest_port, host_port, data) in forwards {
            self.queue_host_data(guest_port, host_port, &data);
            queued_for_guest = true;
        }
        for (guest_port, host_port) in credit_updates {
            self.queue_credit_update(guest_port, host_port);
            queued_for_guest = true;
        }
        for (guest_port, host_port) in closed {
            self.close_host_connection(guest_port, host_port);
            queued_for_guest = true;
        }
        queued_for_guest
    }

    /// Queue an unsolicited `OP_CREDIT_UPDATE` announcing the
    /// connection's current `fwd_cnt` and `buf_alloc` to the guest.
    fn queue_credit_update(&mut self, guest_port: u32, host_port: u32) {
        let Some(conn) = self.connections.get(&(guest_port, host_port)) else {
            return;
        };
        let update = VsockHeader {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            len: 0,
            r#type: 1,
            op: VsockOp::CreditUpdate as u16,
            flags: 0,
            buf_alloc: conn.buf_alloc,
            fwd_cnt: conn.fwd_cnt,
        };
        self.rx_queue.push((update, Vec::new()));
    }

    fn next_ephemeral_port(&self) -> u32 {
        let start = 49152u32;
        let end = 65535u32;
        let range_size = end - start + 1;
        let mut port = start;
        let mut checked = 0u32;

        loop {
            let in_use = self.connections.values().any(|c| c.host_port == port);
            if !in_use {
                return port;
            }
            checked += 1;
            if checked >= range_size {
                // All ports exhausted — return start as fallback
                return start;
            }
            port += 1;
            if port > end {
                port = start;
            }
        }
    }

    /// Get the listener's raw fd for epoll.
    pub fn listener_fd(&self) -> Option<RawFd> {
        self.listener.as_ref().map(|l| l.as_raw_fd())
    }

    /// Reset all connections (for snapshot restore).
    pub fn reset_all(&mut self) {
        self.connections.clear();
        self.rx_queue.clear();
        self.pending_accepts.clear();
        debug!("vsock: all connections reset");
    }
}

impl Drop for VsockConnectionMap {
    fn drop(&mut self) {
        if let Some(ref path) = self.socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vsock_header_roundtrip() {
        let hdr = VsockHeader {
            src_cid: 2,
            dst_cid: 42,
            src_port: 1234,
            dst_port: 5678,
            len: 100,
            r#type: 1,
            op: VsockOp::Rw as u16,
            flags: 0,
            buf_alloc: 65536,
            fwd_cnt: 500,
        };
        let bytes = hdr.to_bytes();
        let parsed = VsockHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.src_cid, 2);
        assert_eq!(parsed.dst_cid, 42);
        assert_eq!(parsed.src_port, 1234);
        assert_eq!(parsed.dst_port, 5678);
        assert_eq!(parsed.len, 100);
        assert_eq!(parsed.op, VsockOp::Rw as u16);
    }

    #[test]
    fn test_vsock_op_from_u16() {
        assert_eq!(VsockOp::from_u16(1), VsockOp::Request);
        assert_eq!(VsockOp::from_u16(5), VsockOp::Rw);
        assert_eq!(VsockOp::from_u16(99), VsockOp::Invalid);
    }

    fn temp_socket_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "void-box-connmap-test-{}-{}.sock",
            tag,
            std::process::id()
        ))
    }

    /// A client that delivers its port prefix across multiple writes is
    /// still established — the sweep accumulates the prefix without ever
    /// blocking on the stream.
    #[test]
    fn service_listener_establishes_after_split_port_prefix() {
        let path = temp_socket_path("split-prefix");
        let mut map = VsockConnectionMap::new(42, &path).expect("bind listener");

        let mut client = UnixStream::connect(&path).expect("connect");
        let port_bytes = 4321u32.to_le_bytes();
        client.write_all(&port_bytes[..2]).expect("first half");

        let established = map.service_listener();
        assert!(
            established.is_empty(),
            "half a prefix must not establish a connection"
        );
        assert_eq!(map.pending_accepts.len(), 1);

        client.write_all(&port_bytes[2..]).expect("second half");
        let established = map.service_listener();
        assert_eq!(established.len(), 1, "full prefix must establish");
        assert!(map.pending_accepts.is_empty());
        assert!(
            map.connections
                .keys()
                .any(|(guest_port, _)| *guest_port == 4321),
            "connection must target the prefixed guest port"
        );
        assert!(
            map.has_pending_rx(),
            "OP_REQUEST must be queued for the guest"
        );
    }

    /// Accepts beyond the connection cap are dropped — the client sees a
    /// closed stream instead of the map growing without bound.
    #[test]
    fn service_listener_drops_connections_beyond_cap() {
        let path = temp_socket_path("cap");
        let mut map = VsockConnectionMap::new(42, &path).expect("bind listener");

        // Saturate the cap with synthetic pending accepts whose peers
        // stay open (so they neither complete nor get reaped).
        let mut peers = Vec::new();
        for _ in 0..MAX_HOST_CONNECTIONS {
            let (local, peer) = UnixStream::pair().expect("socket pair");
            local.set_nonblocking(true).expect("nonblocking");
            peers.push(peer);
            map.pending_accepts.push(PendingAccept {
                stream: local,
                port_buf: [0u8; 4],
                filled: 0,
                accepted_at: Instant::now(),
            });
        }

        let mut client = UnixStream::connect(&path).expect("connect");
        client.write_all(&1234u32.to_le_bytes()).expect("send port");
        let established = map.service_listener();

        assert!(established.is_empty(), "over-cap accept must not establish");
        assert_eq!(map.pending_accepts.len(), MAX_HOST_CONNECTIONS);
        client
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout");
        // The drop closes the stream with the client's port bytes still
        // unread, which Linux reports as ECONNRESET rather than EOF.
        let mut probe = [0u8; 1];
        match client.read(&mut probe) {
            Ok(0) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("dropped over-cap connection must read as closed, got {other:?}"),
        }
    }

    /// A connection whose guest-bound buffer reached the cap is not read
    /// further — backpressure stays in the socket buffer — and reading
    /// resumes once the guest drains the buffer.
    #[test]
    fn drain_host_streams_stops_reading_at_buffer_cap() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        let (host_side, mut peer) = UnixStream::pair().unwrap();
        let mut conn = VsockConnection::new(1234, 50000, host_side);
        // Connecting keeps read data parked in tx_buf.
        conn.state = ConnState::Connecting;
        conn.tx_buf = vec![0u8; HOST_TO_GUEST_BUFFER_CAP];
        map.connections.insert((1234, 50000), conn);

        peer.write_all(b"more data").unwrap();
        map.drain_host_streams();
        assert_eq!(
            map.connections[&(1234, 50000)].tx_buf.len(),
            HOST_TO_GUEST_BUFFER_CAP,
            "capped connection must not be read"
        );

        map.connections
            .get_mut(&(1234, 50000))
            .unwrap()
            .tx_buf
            .clear();
        map.drain_host_streams();
        assert_eq!(
            map.connections[&(1234, 50000)].tx_buf,
            b"more data",
            "reading must resume once the buffer drains"
        );
    }

    /// A pending accept that overruns the prefix deadline is dropped on
    /// the next sweep instead of occupying its slot forever.
    #[test]
    fn service_listener_drops_stalled_prefix_after_deadline() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        let Some(expired) =
            Instant::now().checked_sub(PORT_PREFIX_DEADLINE + Duration::from_millis(10))
        else {
            // Clock too young to backdate (process started < deadline ago).
            return;
        };
        let (local, _peer) = UnixStream::pair().expect("socket pair");
        local.set_nonblocking(true).expect("nonblocking");
        map.pending_accepts.push(PendingAccept {
            stream: local,
            port_buf: [0u8; 4],
            filled: 0,
            accepted_at: expired,
        });

        let established = map.service_listener();
        assert!(established.is_empty());
        assert!(
            map.pending_accepts.is_empty(),
            "deadline-overrunning pending accept must be dropped"
        );
        assert!(map.connections.is_empty());
    }

    #[test]
    fn test_connection_peer_free() {
        let (s1, _s2) = UnixStream::pair().unwrap();
        let mut conn = VsockConnection::new(1234, 5678, s1);
        conn.peer_buf_alloc = 1000;
        conn.peer_fwd_cnt = 0;
        conn.rx_cnt = 0;
        assert_eq!(conn.peer_free(), 1000);

        conn.rx_cnt = 300;
        assert_eq!(conn.peer_free(), 700);
    }

    #[test]
    fn test_connection_map_without_listener() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        assert!(map.connections.is_empty());
        assert!(map.listener_fd().is_none());
        assert!(!map.has_pending_rx());
        map.reset_all();
    }

    #[test]
    fn drain_host_streams_reaps_closed_connections() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        let (host_side, peer) = UnixStream::pair().unwrap();
        let mut conn = VsockConnection::new(1234, 50000, host_side);
        conn.state = ConnState::Connected;
        map.connections.insert((1234, 50000), conn);

        drop(peer);
        let queued_for_guest = map.drain_host_streams();

        assert!(queued_for_guest, "an RST must be queued for the guest");
        assert!(
            map.connections.is_empty(),
            "closed connection must be removed from the map"
        );
        let rx = map.drain_rx();
        assert_eq!(rx.len(), 1);
        let (hdr, payload) = &rx[0];
        assert_eq!(hdr.op, VsockOp::Rst as u16);
        assert_eq!(hdr.dst_port, 1234);
        assert_eq!(hdr.src_port, 50000);
        assert!(payload.is_empty());
    }

    #[test]
    fn drain_host_streams_reaps_closed_connecting_connections() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        let (host_side, peer) = UnixStream::pair().unwrap();
        let conn = VsockConnection::new(1234, 50002, host_side);
        assert_eq!(conn.state, ConnState::Connecting);
        map.connections.insert((1234, 50002), conn);

        drop(peer);
        let queued_for_guest = map.drain_host_streams();

        assert!(queued_for_guest, "an RST must be queued for the guest");
        assert!(
            map.connections.is_empty(),
            "a Connecting connection whose host side closed must be reaped"
        );
    }

    #[test]
    fn drain_host_streams_forwards_pending_data_before_reaping() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        let (host_side, mut peer) = UnixStream::pair().unwrap();
        let mut conn = VsockConnection::new(1234, 50001, host_side);
        conn.state = ConnState::Connected;
        conn.peer_buf_alloc = 65536;
        map.connections.insert((1234, 50001), conn);

        peer.write_all(b"tail data").unwrap();
        drop(peer);

        assert!(map.drain_host_streams());
        let rx = map.drain_rx();
        let mut saw_data = false;
        for (hdr, payload) in &rx {
            if hdr.op == VsockOp::Rw as u16 && payload == b"tail data" {
                saw_data = true;
            }
        }
        assert!(
            saw_data,
            "buffered data must reach the guest before the reap"
        );
        assert!(
            map.connections.contains_key(&(1234, 50001)),
            "a readable connection survives the sweep that drained it"
        );

        assert!(map.drain_host_streams());
        assert!(
            map.connections.is_empty(),
            "the following sweep sees EOF and reaps the connection"
        );
        let rx = map.drain_rx();
        let mut saw_rst = false;
        for (hdr, _) in &rx {
            if hdr.op == VsockOp::Rst as u16 {
                saw_rst = true;
            }
        }
        assert!(saw_rst, "the reap must queue an RST for the guest");
    }

    fn shrink_socket_buffer(fd: RawFd, option: libc::c_int) {
        let size: libc::c_int = 4096;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(ret, 0, "setsockopt failed");
    }

    /// A non-blocking stream that accepts only part of a packet must not
    /// lose the tail: the remainder is buffered, later flushes deliver
    /// it in order, and `fwd_cnt` covers exactly the delivered bytes.
    #[test]
    fn write_to_host_buffers_short_writes_without_data_loss() {
        let (host_side, mut peer) = UnixStream::pair().unwrap();
        shrink_socket_buffer(host_side.as_raw_fd(), libc::SO_SNDBUF);
        shrink_socket_buffer(peer.as_raw_fd(), libc::SO_RCVBUF);
        let mut conn = VsockConnection::new(1234, 50000, host_side);
        conn.state = ConnState::Connected;

        let payload: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
        conn.write_to_host(&payload).unwrap();
        assert!(
            conn.has_pending_host_writes(),
            "with shrunken socket buffers a 128 KiB write must be short"
        );
        assert!(
            (conn.fwd_cnt as usize) < payload.len(),
            "fwd_cnt must cover only delivered bytes, not buffered ones"
        );

        peer.set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut received = Vec::new();
        let mut read_buf = [0u8; 8192];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while received.len() < payload.len() {
            assert!(
                std::time::Instant::now() < deadline,
                "drain did not complete within 30 s: {}/{} bytes received",
                received.len(),
                payload.len()
            );
            conn.flush_host_writes().unwrap();
            match peer.read(&mut read_buf) {
                Ok(0) => panic!("peer saw EOF before all bytes arrived"),
                Ok(read_len) => received.extend_from_slice(&read_buf[..read_len]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("peer read failed: {e}"),
            }
        }
        assert_eq!(received, payload, "every byte must arrive, in order");
        assert_eq!(conn.fwd_cnt as usize, payload.len());
        assert!(!conn.has_pending_host_writes());
    }

    /// A guest that keeps sending past the advertised credit window must
    /// get an error (the caller resets the connection) instead of
    /// growing the pending buffer without bound.
    #[test]
    fn write_to_host_rejects_credit_overrun() {
        let (host_side, peer) = UnixStream::pair().unwrap();
        shrink_socket_buffer(host_side.as_raw_fd(), libc::SO_SNDBUF);
        shrink_socket_buffer(peer.as_raw_fd(), libc::SO_RCVBUF);
        let mut conn = VsockConnection::new(1234, 50001, host_side);
        conn.state = ConnState::Connected;

        // Fill the advertised window; nobody reads the peer side, so
        // nearly all of it stays buffered.
        let window = conn.buf_alloc as usize;
        conn.write_to_host(&vec![0u8; window]).unwrap();
        assert!(conn.has_pending_host_writes());

        let overrun = conn
            .write_to_host(&[0u8; 64 * 1024])
            .expect_err("writing past the credit window must fail");
        assert!(
            overrun.to_string().contains("credit"),
            "unexpected error: {overrun}"
        );
        drop(peer);
    }

    /// A flush that delivers deferred bytes advances `fwd_cnt` with no
    /// other packet to carry it; the sweep must announce the freed
    /// credit unsolicited, or a guest parked on exhausted credit never
    /// wakes.
    #[test]
    fn deferred_flush_announces_credit_update() {
        let mut map = VsockConnectionMap::new_without_listener(42);
        let (host_side, mut peer) = UnixStream::pair().unwrap();
        shrink_socket_buffer(host_side.as_raw_fd(), libc::SO_SNDBUF);
        shrink_socket_buffer(peer.as_raw_fd(), libc::SO_RCVBUF);
        let mut conn = VsockConnection::new(1234, 50002, host_side);
        conn.state = ConnState::Connected;

        let payload = vec![7u8; 64 * 1024];
        conn.write_to_host(&payload).unwrap();
        assert!(conn.has_pending_host_writes());
        map.connections.insert((1234, 50002), conn);

        peer.set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut read_buf = [0u8; 8192];
        let mut credit_update_count = 0usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let pending = map
                .connections
                .get(&(1234, 50002))
                .is_some_and(|c| c.has_pending_host_writes());
            if !pending {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "deferred flush did not complete within 30 s"
            );
            // Make room on the stream, then let the sweep flush.
            match peer.read(&mut read_buf) {
                Ok(_) => {}
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("peer read failed: {e}"),
            }
            let queued_for_guest = map.drain_host_streams();
            for (hdr, _) in map.drain_rx() {
                if hdr.op == VsockOp::CreditUpdate as u16 {
                    credit_update_count += 1;
                    assert!(queued_for_guest, "a credit update must signal the guest");
                    assert_eq!(hdr.dst_port, 1234);
                    assert_eq!(hdr.src_port, 50002);
                }
            }
        }
        assert!(
            credit_update_count > 0,
            "deferred delivery must queue an unsolicited OP_CREDIT_UPDATE"
        );
        let final_fwd_cnt = map.connections.get(&(1234, 50002)).unwrap().fwd_cnt;
        assert_eq!(final_fwd_cnt as usize, payload.len());
    }
}
