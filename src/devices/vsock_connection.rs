//! Vsock connection state machine for the userspace backend.
//!
//! Manages active connections, maps guest AF_VSOCK ports to host AF_UNIX
//! streams. Processes vsock header packets (TX from guest) and generates
//! RX packets (to guest).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use tracing::{debug, trace, warn};

use crate::Result;

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
    /// Returns number of bytes read, or 0 if nothing available / EOF.
    pub fn read_from_host(&mut self) -> usize {
        let mut buf = [0u8; 65536];
        match self.stream.read(&mut buf) {
            Ok(0) => 0, // EOF
            Ok(n) => {
                self.tx_buf.extend_from_slice(&buf[..n]);
                n
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(_) => 0,
        }
    }

    /// Write data from guest to the host stream.
    pub fn write_to_host(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.stream.write(data)
    }
}

// ---------------------------------------------------------------------------
// Connection map
// ---------------------------------------------------------------------------

/// Key for the connection map: (guest_port, host_port).
pub type ConnKey = (u32, u32);

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
        }
    }

    /// Accept a pending connection from the Unix listener.
    ///
    /// The host application connects to the Unix socket and sends a 4-byte LE
    /// port number as the first thing. This tells us which guest port to
    /// connect to.
    pub fn accept_incoming(&mut self) -> Option<(ConnKey, RawFd)> {
        let listener = self.listener.as_ref()?;
        let (stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) => {
                warn!("vsock listener accept error: {}", e);
                return None;
            }
        };

        // Read the 4-byte port number from the connecting host application
        let _ = stream.set_nonblocking(false);
        let mut port_buf = [0u8; 4];
        let mut tmp = stream.try_clone().ok()?;
        // Give a brief timeout for the port read
        let _ = tmp.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        if tmp.read_exact(&mut port_buf).is_err() {
            warn!("vsock: host connection didn't send port number");
            return None;
        }
        let guest_port = u32::from_le_bytes(port_buf);
        drop(tmp);

        // Use an ephemeral host port
        let host_port = self.next_ephemeral_port();

        let fd = stream.as_raw_fd();
        let conn = VsockConnection::new(guest_port, host_port, stream);
        let key = (guest_port, host_port);

        debug!(
            "vsock: accepted host connection for guest port {} (host_port={})",
            guest_port, host_port
        );
        // Queue a OP_REQUEST to the guest
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

        self.connections.insert(key, conn);
        Some((key, fd))
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

                if let Some(conn) = self.connections.get_mut(&rw_key) {
                    if conn.state == ConnState::Connected && !data.is_empty() {
                        match conn.write_to_host(data) {
                            Ok(n) => {
                                conn.fwd_cnt = conn.fwd_cnt.wrapping_add(n as u32);
                                trace!(
                                    "vsock: forwarded {} bytes to host (port={})",
                                    n,
                                    hdr.dst_port
                                );
                            }
                            Err(e) => {
                                warn!("vsock: write to host failed: {}", e);
                                self.queue_rst(hdr);
                                return true;
                            }
                        }
                    }
                    // Update peer credit info
                    conn.peer_buf_alloc = hdr.buf_alloc;
                    conn.peer_fwd_cnt = hdr.fwd_cnt;
                    true
                } else {
                    // Unknown connection — send RST
                    self.queue_rst(hdr);
                    true
                }
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
    /// Call this after reading data from a host UnixStream.
    pub fn queue_host_data(&mut self, guest_port: u32, host_port: u32, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let key = (guest_port, host_port);
        let conn = match self.connections.get(&key) {
            Some(c) => c,
            None => return,
        };

        // Respect credit: only send up to peer_free bytes
        let max_send = conn.peer_free() as usize;
        let send_len = data.len().min(max_send).min(4096); // cap at 4K per packet
        if send_len == 0 {
            return;
        }

        let hdr = VsockHeader {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            len: send_len as u32,
            r#type: 1,
            op: VsockOp::Rw as u16,
            flags: 0,
            buf_alloc: conn.buf_alloc,
            fwd_cnt: conn.fwd_cnt,
        };
        self.rx_queue.push((hdr, data[..send_len].to_vec()));
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
            conn.tx_buf.drain(..).collect::<Vec<_>>()
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

    fn next_ephemeral_port(&self) -> u32 {
        // Start from 49152 (dynamic port range) and find an unused one
        let mut port = 49152u32;
        loop {
            let in_use = self.connections.values().any(|c| c.host_port == port);
            if !in_use {
                return port;
            }
            port += 1;
            if port > 65535 {
                port = 49152;
                break;
            }
        }
        port
    }

    /// Get the listener's raw fd for epoll.
    pub fn listener_fd(&self) -> Option<RawFd> {
        self.listener.as_ref().map(|l| l.as_raw_fd())
    }

    /// Reset all connections (for snapshot restore).
    pub fn reset_all(&mut self) {
        self.connections.clear();
        self.rx_queue.clear();
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
}
