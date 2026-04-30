//! SLIRP-style user-mode networking
//!
//! Provides NAT-based network connectivity for guest VMs without
//! requiring root privileges, TAP devices, or iptables configuration.
//!
//! Network layout (SLIRP standard):
//! - Guest IP: 10.0.2.15/24
//! - Gateway:  10.0.2.2
//! - DNS:      10.0.2.3
//!
//! Architecture:
//! - Unified flow table: All TCP/UDP/ICMP echo flows live in a single
//!   `flow_table: HashMap<FlowKey, FlowEntry>` (Phase 4). Per-protocol
//!   relay logic dispatches on the FlowEntry variant.
//! - ARP: custom handler responds as gateway for all 10.0.2.x IPs
//! - TCP: passt-style sequence-mirroring NAT (host→guest via
//!   `recv(MSG_PEEK)` + ACK-driven consume; guest→host via direct
//!   write + don't-ACK-on-WouldBlock TCP backpressure). No userspace
//!   per-connection buffers — the host kernel's socket buffer holds
//!   outstanding data.
//! - ICMP echo: relayed via unprivileged `SOCK_DGRAM IPPROTO_ICMP`
//! - UDP: per-flow connected sockets; DNS to 10.0.2.3:53 takes a
//!   cached fast-path
//! - Other: silently dropped
//!
//! The smoltcp library is used for its Ethernet/IPv4/TCP/UDP wire types
//! and checksum computation, but all packet handling is done manually.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::network::{nat, NetworkBackend};

/// Cached DNS response with expiry.
struct DnsCacheEntry {
    response: Vec<u8>,
    expires: Instant,
}

/// A DNS query waiting to be resolved on the net-poll thread.
struct PendingDnsQuery {
    query: Vec<u8>,
    guest_src_port: u16,
}

/// DNS cache TTL (seconds).  DNS responses carry their own TTL but parsing
/// every record type is overkill — a short blanket TTL covers 99 % of cases
/// while keeping the implementation simple.
const DNS_CACHE_TTL_SECS: u64 = 60;

use ipnet::Ipv4Net;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, HardwareAddress, Icmpv4Packet,
    Icmpv4Repr, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl,
    TcpPacket, TcpRepr, TcpSeqNumber, UdpPacket, UdpRepr,
};

use tracing::{debug, trace, warn};

use crate::Result;

fn smol_instant_now() -> SmolInstant {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    SmolInstant::from_micros(now.as_micros() as i64)
}

/// SLIRP network configuration
pub const SLIRP_GUEST_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
pub const SLIRP_GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
pub const SLIRP_DNS_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 3);
pub const SLIRP_NETMASK: u8 = 24;

pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x01];

const MTU: usize = 1500;
const MAX_QUEUE_SIZE: usize = 64;
const TCP_WINDOW: u16 = 65535;
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// ICMP unprivileged probe state.
///
/// `0` = unknown (not yet probed), `1` = available, `2` = unavailable
/// (kernel returned `EACCES` or `EPERM` — typically `net.ipv4.ping_group_range`
/// excludes the calling GID). Once set to `2`, `open_icmp_socket` short-circuits.
static ICMP_PROBE: AtomicU8 = AtomicU8::new(0);

// ──────────────────────────────────────────────────────────────────────
//  TCP NAT connection tracking
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub(crate) enum TcpNatState {
    /// Guest sent SYN; we responded with SYN-ACK; waiting for guest's
    /// final ACK to complete the outbound 3-way handshake.
    SynReceived,
    /// We synthesized a SYN to the guest (port-forwarding); waiting
    /// for the guest's SYN-ACK to advance to Established.
    SynSent,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    Closed,
}

/// Key for NAT table: (guest_src_port, dst_ip, dst_port)
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct NatKey {
    guest_src_port: u16,
    dst_ip: Ipv4Address,
    dst_port: u16,
}

struct TcpNatEntry {
    host_stream: TcpStream,
    state: TcpNatState,
    /// Our sequence number (we are the "server" from the guest's perspective)
    our_seq: u32,
    /// Last acknowledged guest sequence number
    guest_ack: u32,
    last_activity: Instant,
    /// Bytes sent to the guest but not yet ACK'd by the guest.
    /// Equivalent to `our_seq - last_acked_seq`, stored explicitly so
    /// the relay can decide how much new payload to peek+send each poll.
    /// The ACK-driven consume path decrements this as the guest ACKs data.
    bytes_in_flight: u32,
}

/// Key for the ICMP echo NAT table: (guest ICMP id, destination IP).
///
/// The host kernel rewrites the ICMP id when sending through a
/// `SOCK_DGRAM IPPROTO_ICMP` socket; we keep the guest's original id here so
/// the reply frame can be translated back before injection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct IcmpEchoKey {
    guest_id: u16,
    dst_ip: Ipv4Address,
}

/// State for one in-flight ICMP echo request from the guest.
struct IcmpEchoEntry {
    /// Host-side socket: `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`.
    /// Set non-blocking; the kernel handles ICMP framing — no
    /// `CAP_NET_RAW` needed.
    sock: std::net::UdpSocket,
    /// The guest's original ICMP id from the echo request.  The host kernel
    /// rewrites the id to a kernel-assigned value when the `SOCK_DGRAM`
    /// ICMP socket sends; we translate back to `guest_id` when emitting the
    /// reply frame.
    // Read in `relay_icmp_echo` when translating the reply frame.
    guest_id: u16,
    last_activity: Instant,
}

/// Key for the UDP flow NAT table: (guest source port, destination IP, destination port).
///
/// Each unique 3-tuple maps to its own connected `UdpSocket` on the host,
/// mirroring passt's `udp_flow_from_tap` per-flow design.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct UdpFlowKey {
    guest_src_port: u16,
    dst_ip: Ipv4Address,
    dst_port: u16,
}

/// State for one active UDP flow from the guest.
struct UdpFlowEntry {
    /// Connected `UdpSocket`. The host kernel handles source-port
    /// preservation and reply demux; we just `send` and `recv`.
    /// Set non-blocking.
    sock: std::net::UdpSocket,
    /// Last frame timestamp; read by Task 2.4 idle-timeout reaper.
    last_activity: Instant,
}

/// Unified flow-table key. Each variant wraps the protocol-specific
/// key already defined elsewhere in this module — no field changes,
/// just one type the unified `flow_table` `HashMap` (added in Task 4.2)
/// can store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum FlowKey {
    Tcp(NatKey),
    Udp(UdpFlowKey),
    IcmpEcho(IcmpEchoKey),
}

/// Unified flow-table value. Each variant wraps the protocol's existing
/// entry struct.
enum FlowEntry {
    Tcp(TcpNatEntry),
    Udp(UdpFlowEntry),
    IcmpEcho(IcmpEchoEntry),
}

/// Open an unprivileged ICMP socket (`SOCK_DGRAM IPPROTO_ICMP`).
///
/// The kernel handles ICMP framing; `CAP_NET_RAW` is **not** required.
/// The socket is set `SOCK_NONBLOCK | SOCK_CLOEXEC` at creation time.
///
/// Returns `Err` if the kernel rejects the call (e.g. the
/// `net.ipv4.ping_group_range` sysctl excludes the current GID).
/// After the first rejection, subsequent calls short-circuit and return
/// `PermissionDenied` without retrying the syscall.
fn open_icmp_socket() -> io::Result<std::net::UdpSocket> {
    if ICMP_PROBE.load(Ordering::Relaxed) == 2 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ICMP unprivileged probe previously failed",
        ));
    }
    // SAFETY: socket(2) returns -1 on error; we check before wrapping.
    // IPPROTO_ICMP + SOCK_DGRAM is the unprivileged ICMP path: the kernel
    // handles ICMP framing, no CAP_NET_RAW required.
    let raw = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            libc::IPPROTO_ICMP,
        )
    };
    if raw < 0 {
        let err = io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
            // First failure transitions 0 → 2 and emits the warn-once log.
            // swap returns the previous value; only log if we were the first
            // to set it.
            if ICMP_PROBE.swap(2, Ordering::Relaxed) != 2 {
                warn!(
                    "SLIRP: unprivileged ICMP unavailable on this host \
                     (sysctl net.ipv4.ping_group_range likely restricts \
                     it); ICMP echo from guests will be dropped."
                );
            }
        }
        return Err(err);
    }
    ICMP_PROBE.store(1, Ordering::Relaxed);
    // SAFETY: `raw` is a valid fd from socket(2); UdpSocket adopts
    // ownership and closes on drop.
    Ok(unsafe { std::net::UdpSocket::from_raw_fd(raw) })
}

/// Open a connected UDP socket for one guest→host flow.
///
/// Binds to an ephemeral port on `0.0.0.0`, sets non-blocking mode,
/// then calls `connect(dst)` so that:
/// - `send` delivers datagrams to `dst` without specifying the address each time.
/// - Incoming datagrams are filtered to replies from `dst` only, enabling
///   per-flow demux without an additional dispatch table.
///
/// No `CAP_NET_RAW` required — `SOCK_DGRAM` UDP is fully unprivileged.
fn open_udp_flow_socket(dst: std::net::SocketAddr) -> io::Result<std::net::UdpSocket> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    sock.set_nonblocking(true)?;
    sock.connect(dst)?;
    Ok(sock)
}

/// Non-blocking `recv(MSG_PEEK)` on a `TcpStream`, returning the
/// number of bytes available without consuming them from the
/// kernel's recv queue.
///
/// `std::net::TcpStream` does not expose `MSG_PEEK`; we go through
/// `libc::recv` directly. `MSG_DONTWAIT` keeps the call non-blocking
/// even if the underlying stream's `set_nonblocking` flag was
/// dropped at some intermediate point.
///
/// Used by the passt-style host→guest TCP relay (Task 3.3): peek
/// what's in the kernel buffer, send the un-ACK'd portion to the
/// guest. Bytes stay in the kernel until the guest ACKs and Task
/// 3.4's ACK-driven `read()` consumes them.
fn recv_peek(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `stream` outlives the syscall; `buf` is uniquely
    // borrowed and `len` matches the slice length.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

// ──────────────────────────────────────────────────────────────────────
//  smoltcp plumbing (ARP only)
// ──────────────────────────────────────────────────────────────────────

struct PacketQueue {
    rx_queue: Vec<Vec<u8>>,
    tx_queue: Vec<Vec<u8>>,
}
impl PacketQueue {
    fn new() -> Self {
        Self {
            rx_queue: Vec::new(),
            tx_queue: Vec::new(),
        }
    }
}
struct VirtualDevice {
    queue: Arc<Mutex<PacketQueue>>,
}
impl VirtualDevice {
    fn new(q: Arc<Mutex<PacketQueue>>) -> Self {
        Self { queue: q }
    }
}
impl Device for VirtualDevice {
    type RxToken<'a> = VRx;
    type TxToken<'a> = VTx;
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = MTU;
        c.max_burst_size = Some(1);
        c
    }
    fn receive(&mut self, _ts: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut q = self.queue.lock().unwrap();
        if q.rx_queue.is_empty() {
            return None;
        }
        let pkt = q.rx_queue.remove(0);
        Some((
            VRx { buffer: pkt },
            VTx {
                queue: self.queue.clone(),
            },
        ))
    }
    fn transmit(&mut self, _ts: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(VTx {
            queue: self.queue.clone(),
        })
    }
}
struct VRx {
    buffer: Vec<u8>,
}
impl RxToken for VRx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R {
        f(&mut self.buffer)
    }
}
struct VTx {
    queue: Arc<Mutex<PacketQueue>>,
}
impl TxToken for VTx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        let mut q = self.queue.lock().unwrap();
        if q.tx_queue.len() < MAX_QUEUE_SIZE {
            q.tx_queue.push(buf);
        }
        r
    }
}

// ──────────────────────────────────────────────────────────────────────
//  Host DNS discovery
// ──────────────────────────────────────────────────────────────────────

/// Read the host's `/etc/resolv.conf` and return `"ip:53"` entries.
/// Falls back to `["8.8.8.8:53", "1.1.1.1:53"]` if the file is missing
/// or contains no usable nameservers.
fn parse_resolv_conf() -> Vec<String> {
    let fallback = vec!["8.8.8.8:53".to_string(), "1.1.1.1:53".to_string()];
    let content = match std::fs::read_to_string("/etc/resolv.conf") {
        Ok(c) => c,
        Err(_) => return fallback,
    };
    let servers: Vec<String> = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                return None;
            }
            let mut parts = line.split_whitespace();
            if parts.next()? != "nameserver" {
                return None;
            }
            let ip = parts.next()?;
            // Skip localhost entries (systemd-resolved stub) — they won't
            // work from inside the VMM's network namespace.
            if ip.starts_with("127.") {
                return None;
            }
            Some(format!("{}:53", ip))
        })
        .collect();
    if servers.is_empty() {
        fallback
    } else {
        servers
    }
}

// ──────────────────────────────────────────────────────────────────────
//  SLIRP Stack
// ──────────────────────────────────────────────────────────────────────

pub struct SlirpBackend {
    queue: Arc<Mutex<PacketQueue>>,
    iface: Interface,
    sockets: SocketSet<'static>,
    _device: VirtualDevice,
    /// Frames to inject into guest (built by our NAT, not by smoltcp)
    inject_to_guest: Vec<Vec<u8>>,
    /// Maximum concurrent TCP connections allowed
    max_concurrent_connections: usize,
    /// Maximum new connections per second
    max_connections_per_second: u32,
    /// Sliding window of recent connection timestamps for rate limiting
    connection_timestamps: VecDeque<Instant>,
    /// Stateless outbound translation rules (deny-list, gateway loopback, port forwards).
    nat: nat::Rules,
    /// Host DNS servers (parsed from /etc/resolv.conf, fallback to public)
    dns_servers: Vec<String>,
    /// DNS response cache keyed by the raw query bytes (question section)
    dns_cache: HashMap<Vec<u8>, DnsCacheEntry>,
    /// DNS queries waiting to be resolved on the net-poll thread.
    pending_dns: Vec<PendingDnsQuery>,
    /// Unified flow table — Phase 4.
    ///
    /// All three protocols (TCP, UDP, ICMP echo) are keyed here after Task 4.5.
    /// ICMP migrated in 4.3; UDP in 4.4; TCP in 4.5.
    flow_table: HashMap<FlowKey, FlowEntry>,
}

impl SlirpBackend {
    pub fn new() -> Result<Self> {
        Self::with_security(64, 50, &["169.254.0.0/16".to_string()], &[])
    }

    /// Create a SLIRP stack with security parameters.
    ///
    /// `port_forwards` maps host ports to guest ports as `(host_port, guest_port)` pairs.
    /// Each entry is stored in [`nat::Rules`] as a TCP forward rule; host listeners are
    /// spawned in sub-task B (5.5b) and not yet active.
    pub fn with_security(
        max_concurrent_connections: usize,
        max_connections_per_second: u32,
        deny_list_cidrs: &[String],
        port_forwards: &[(u16, u16)],
    ) -> Result<Self> {
        debug!("Creating SLIRP stack");
        let queue = Arc::new(Mutex::new(PacketQueue::new()));
        let device = VirtualDevice::new(queue.clone());

        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        let mut iface = Interface::new(
            config,
            &mut VirtualDevice::new(queue.clone()),
            smol_instant_now(),
        );

        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::v4(10, 0, 2, 2), SLIRP_NETMASK))
                .unwrap();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(SLIRP_GATEWAY_IP)
            .unwrap();

        let sockets = SocketSet::new(vec![]);

        let deny_cidrs: Vec<Ipv4Net> = deny_list_cidrs
            .iter()
            .filter_map(|cidr| {
                cidr.parse::<Ipv4Net>()
                    .map_err(|e| {
                        warn!("SLIRP: invalid deny list CIDR '{}': {}", cidr, e);
                        e
                    })
                    .ok()
            })
            .collect();

        let nat_port_forwards: Vec<nat::PortForward> = port_forwards
            .iter()
            .map(|&(host_port, guest_port)| nat::PortForward {
                proto: nat::ForwardProto::Tcp,
                host_port,
                guest_port,
            })
            .collect();

        let nat = nat::Rules {
            gateway_loopback: true,
            deny_cidrs,
            port_forwards: nat_port_forwards,
        };

        let dns_servers = parse_resolv_conf();
        debug!(
            "SLIRP stack created - Gateway: {}, DNS: {}, max_conn: {}, rate: {}/s, deny_list: {} CIDRs, port_forwards: {}, dns_servers: {:?}",
            SLIRP_GATEWAY_IP, SLIRP_DNS_IP, max_concurrent_connections, max_connections_per_second,
            nat.deny_cidrs.len(), nat.port_forwards.len(), dns_servers
        );

        Ok(Self {
            queue,
            iface,
            sockets,
            _device: device,
            inject_to_guest: Vec::new(),
            max_concurrent_connections,
            max_connections_per_second,
            connection_timestamps: VecDeque::new(),
            nat,
            dns_servers,
            dns_cache: HashMap::new(),
            pending_dns: Vec::new(),
            flow_table: HashMap::new(),
        })
    }

    /// Check if a new connection is allowed by the rate limiter.
    /// Returns true if the connection is allowed.
    fn check_rate_limit(&mut self) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(1);

        // Remove timestamps older than 1 second
        while let Some(&oldest) = self.connection_timestamps.front() {
            if now.duration_since(oldest) > window {
                self.connection_timestamps.pop_front();
            } else {
                break;
            }
        }

        if self.connection_timestamps.len() >= self.max_connections_per_second as usize {
            return false;
        }

        self.connection_timestamps.push_back(now);
        true
    }

    // ── Public API ──────────────────────────────────────────────────

    /// Process an ethernet frame from the guest
    pub fn process_guest_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() < 14 {
            return Ok(());
        }

        let eth = match EthernetFrame::new_checked(frame) {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };

        match eth.ethertype() {
            EthernetProtocol::Arp => {
                self.handle_arp_frame(frame)?;
            }
            EthernetProtocol::Ipv4 => {
                self.handle_ipv4_frame(frame)?;
            }
            _ => {
                trace!("SLIRP: ignoring ethertype {:?}", eth.ethertype());
            }
        }
        Ok(())
    }

    /// Drain frames destined to the guest into `out`, reusing the caller's
    /// buffer across calls and avoiding a fresh allocation on every tick.
    ///
    /// See [`crate::network::NetworkBackend::drain_to_guest`].
    pub fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
        // Check rx_queue size before polling.
        let rx_count = {
            let q = self.queue.lock().unwrap();
            q.rx_queue.len()
        };

        // 1. Let smoltcp handle ARP.
        let ts = smol_instant_now();
        let mut dev = VirtualDevice::new(self.queue.clone());
        let changed = self.iface.poll(ts, &mut dev, &mut self.sockets);

        // 2. Resolve pending DNS queries (off vCPU thread).
        self.resolve_pending_dns();

        // 3. Process TCP NAT data relay.
        self.relay_tcp_nat_data();

        // 4. Relay ICMP echo replies from host sockets back to the guest.
        self.relay_icmp_echo();

        // 5. Relay UDP flow replies from host sockets back to the guest.
        self.relay_udp_flows();

        // 6. Collect frames: smoltcp ARP responses + our NAT-built frames.
        {
            let mut q = self.queue.lock().unwrap();
            if !q.tx_queue.is_empty() || rx_count > 0 {
                debug!(
                    "SLIRP poll: rx_in={}, tx_out={}, changed={}, inject={}",
                    rx_count,
                    q.tx_queue.len(),
                    changed,
                    self.inject_to_guest.len()
                );
            }
            out.append(&mut q.tx_queue);
        }
        out.append(&mut self.inject_to_guest);
    }

    /// Poll the stack and return ethernet frames to send to the guest.
    ///
    /// # Deprecated
    ///
    /// Allocates a fresh [`Vec`] on every call. Prefer [`drain_to_guest`],
    /// which writes into a caller-supplied buffer and avoids the allocation.
    ///
    /// [`drain_to_guest`]: SlirpBackend::drain_to_guest
    #[deprecated(note = "use drain_to_guest")]
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.drain_to_guest(&mut out);
        out
    }

    /// Extract the DNS question section (bytes after the 12-byte header up to
    /// and including the QCLASS) to use as a cache key.  This is stable for
    /// identical queries regardless of the random transaction ID.
    fn dns_cache_key(query: &[u8]) -> Option<Vec<u8>> {
        if query.len() < 17 {
            return None; // too short to contain a question
        }
        // Question section starts at byte 12.  Walk labels until the root
        // (0-length label), then grab QTYPE (2) + QCLASS (2).
        let mut pos = 12;
        loop {
            if pos >= query.len() {
                return None;
            }
            let len = query[pos] as usize;
            if len == 0 {
                pos += 1; // skip root label
                break;
            }
            pos += 1 + len;
        }
        pos += 4; // QTYPE + QCLASS
        if pos > query.len() {
            return None;
        }
        Some(query[12..pos].to_vec())
    }

    /// Drains the pending DNS queue and resolves each query. Called from
    /// `poll()` on the net-poll thread, never from a vCPU thread.
    fn resolve_pending_dns(&mut self) {
        if self.pending_dns.is_empty() {
            return;
        }
        let queries: Vec<PendingDnsQuery> = self.pending_dns.drain(..).collect();
        for pending in queries {
            if let Some(response) = self.forward_dns_query(&pending.query) {
                let frame = self.build_udp_response(
                    SLIRP_DNS_IP,
                    SLIRP_GUEST_IP,
                    53,
                    pending.guest_src_port,
                    &response,
                );
                self.inject_to_guest.push(frame);
                debug!(
                    "SLIRP DNS: resolved pending query, {} byte response",
                    response.len()
                );
            } else {
                warn!("SLIRP DNS: failed to resolve pending query");
            }
        }
    }

    /// Forward a DNS query to host resolvers and return the response.
    ///
    /// Uses a 60-second cache and reads nameservers from the host's
    /// `/etc/resolv.conf` (falls back to 8.8.8.8 / 1.1.1.1).  Timeout is
    /// kept short (2 s) so that tool preflight checks don't stall.
    fn forward_dns_query(&mut self, query: &[u8]) -> Option<Vec<u8>> {
        // ── Check cache ────────────────────────────────────────────
        if let Some(key) = Self::dns_cache_key(query) {
            if let Some(entry) = self.dns_cache.get(&key) {
                if Instant::now() < entry.expires {
                    // Patch the cached response with the caller's
                    // transaction ID (first 2 bytes) so the guest
                    // matches it to its pending request.
                    let mut resp = entry.response.clone();
                    if resp.len() >= 2 && query.len() >= 2 {
                        resp[0] = query[0];
                        resp[1] = query[1];
                    }
                    debug!("SLIRP DNS: cache hit");
                    return Some(resp);
                } else {
                    self.dns_cache.remove(&key);
                }
            }
        }

        // ── Forward to upstream ────────────────────────────────────
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok()?;

        for server in &self.dns_servers {
            if sock.send_to(query, server).is_ok() {
                let mut resp = vec![0u8; 4096];
                if let Ok((n, _)) = sock.recv_from(&mut resp) {
                    resp.truncate(n);
                    // Store in cache
                    if let Some(key) = Self::dns_cache_key(query) {
                        self.dns_cache.insert(
                            key,
                            DnsCacheEntry {
                                response: resp.clone(),
                                expires: Instant::now() + Duration::from_secs(DNS_CACHE_TTL_SECS),
                            },
                        );
                    }
                    return Some(resp);
                }
            }
        }
        None
    }

    // ── ARP handling ─────────────────────────────────────────────────

    fn handle_arp_frame(&mut self, frame: &[u8]) -> Result<()> {
        // ARP: Ethernet(14) + ARP(28)
        if frame.len() < 42 {
            return Ok(());
        }

        let arp = &frame[14..];
        let hw_type = u16::from_be_bytes([arp[0], arp[1]]);
        let proto_type = u16::from_be_bytes([arp[2], arp[3]]);
        let opcode = u16::from_be_bytes([arp[6], arp[7]]);

        // Only handle Ethernet/IPv4 ARP requests
        if hw_type != 1 || proto_type != 0x0800 || opcode != 1 {
            return Ok(());
        }

        let sender_mac = &arp[8..14];
        let sender_ip = &arp[14..18];
        let target_ip = &arp[24..28];

        debug!(
            "SLIRP ARP: who has {}.{}.{}.{}, tell {}.{}.{}.{}",
            target_ip[0],
            target_ip[1],
            target_ip[2],
            target_ip[3],
            sender_ip[0],
            sender_ip[1],
            sender_ip[2],
            sender_ip[3]
        );

        // Reply for any IP in our subnet (10.0.2.x) except the guest's own IP
        let target = Ipv4Address::from_bytes(target_ip);
        if target == SLIRP_GUEST_IP {
            return Ok(()); // Don't respond for the guest's own IP
        }

        // Build ARP reply: Ethernet(14) + ARP(28) = 42 bytes
        let mut reply = vec![0u8; 42];
        // Ethernet header
        reply[0..6].copy_from_slice(sender_mac); // dst = original sender
        reply[6..12].copy_from_slice(&GATEWAY_MAC); // src = gateway
        reply[12] = 0x08;
        reply[13] = 0x06; // EtherType = ARP

        // ARP payload (28 bytes starting at offset 14)
        reply[14..16].copy_from_slice(&1u16.to_be_bytes()); // hw type = Ethernet
        reply[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // proto = IPv4
        reply[18] = 6; // hw addr len
        reply[19] = 4; // proto addr len
        reply[20..22].copy_from_slice(&2u16.to_be_bytes()); // opcode = reply
        reply[22..28].copy_from_slice(&GATEWAY_MAC); // sender hw addr = gateway
        reply[28..32].copy_from_slice(target_ip); // sender proto addr = requested IP
        reply[32..38].copy_from_slice(sender_mac); // target hw addr = original sender
        reply[38..42].copy_from_slice(sender_ip); // target proto addr = original sender IP

        debug!(
            "SLIRP ARP: reply {} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            target,
            GATEWAY_MAC[0],
            GATEWAY_MAC[1],
            GATEWAY_MAC[2],
            GATEWAY_MAC[3],
            GATEWAY_MAC[4],
            GATEWAY_MAC[5]
        );

        self.inject_to_guest.push(reply);
        Ok(())
    }

    // ── IPv4 handling ────────────────────────────────────────────────

    fn handle_ipv4_frame(&mut self, frame: &[u8]) -> Result<()> {
        let eth =
            EthernetFrame::new_checked(frame).map_err(|e| crate::Error::Network(e.to_string()))?;
        let ipv4 = match Ipv4Packet::new_checked(eth.payload()) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let dst_ip = ipv4.dst_addr();
        let protocol = ipv4.next_header();

        // UDP — DNS keeps its dedicated cache+forward handler; everything
        // else goes through the per-flow connected-socket NAT.
        if protocol == IpProtocol::Udp {
            if dst_ip == SLIRP_DNS_IP {
                return self.handle_dns_frame(&ipv4);
            }
            return self.handle_udp_frame(&ipv4);
        }

        // TCP to any external IP (not gateway) – NAT proxy
        if protocol == IpProtocol::Tcp {
            // Also handle TCP to gateway IP (for potential proxy use)
            if dst_ip != SLIRP_GUEST_IP {
                return self.handle_tcp_frame(&ipv4);
            }
        }

        // ICMP echo requests — forward via unprivileged SOCK_DGRAM IPPROTO_ICMP socket
        if protocol == IpProtocol::Icmp {
            return self.handle_icmp_frame(&ipv4);
        }

        // Everything else – drop silently
        trace!("SLIRP: dropping {:?} packet to {}", protocol, dst_ip);
        Ok(())
    }

    // ── DNS forwarding ───────────────────────────────────────────────

    fn handle_dns_frame(&mut self, ipv4: &Ipv4Packet<&[u8]>) -> Result<()> {
        let udp = match UdpPacket::new_checked(ipv4.payload()) {
            Ok(u) => u,
            Err(_) => return Ok(()),
        };
        let src_port = udp.src_port();
        let query = udp.payload();

        debug!(
            "SLIRP DNS: query from guest port {} ({} bytes)",
            src_port,
            query.len()
        );

        // Fast path: serve from cache (safe on vCPU thread)
        if let Some(key) = Self::dns_cache_key(query) {
            if let Some(entry) = self.dns_cache.get(&key) {
                if Instant::now() < entry.expires {
                    let mut resp = entry.response.clone();
                    if resp.len() >= 2 && query.len() >= 2 {
                        resp[0] = query[0];
                        resp[1] = query[1];
                    }
                    debug!("SLIRP DNS: cache hit (vCPU fast path)");
                    let frame =
                        self.build_udp_response(SLIRP_DNS_IP, SLIRP_GUEST_IP, 53, src_port, &resp);
                    self.inject_to_guest.push(frame);
                    return Ok(());
                } else {
                    self.dns_cache.remove(&key);
                }
            }
        }

        // Slow path: queue for resolution on net-poll thread
        debug!("SLIRP DNS: queuing for async resolution");
        self.pending_dns.push(PendingDnsQuery {
            query: query.to_vec(),
            guest_src_port: src_port,
        });
        Ok(())
    }

    // ── Non-DNS UDP forwarding ────────────────────────────────────────

    /// Forward a non-DNS guest UDP datagram to the host via a per-flow connected socket.
    ///
    /// Each unique (guest source port, destination IP, destination port) 3-tuple maps to
    /// one connected `UdpSocket`. On the first frame for a flow the socket is created via
    /// [`open_udp_flow_socket`] and stored in `flow_table` under `FlowKey::Udp`. Subsequent
    /// frames reuse the existing socket, updating `last_activity` for idle-timeout reaping (Task 2.4).
    ///
    /// The SLIRP gateway address (`10.0.2.2`) is translated to `127.0.0.1` before
    /// connecting, mirroring the same translation used on the TCP NAT path.
    ///
    /// Reply delivery back to the guest is handled by Task 2.3 (`relay_udp_flows`).
    fn handle_udp_frame(&mut self, ipv4: &Ipv4Packet<&[u8]>) -> Result<()> {
        let udp = match UdpPacket::new_checked(ipv4.payload()) {
            Ok(u) => u,
            Err(_) => return Ok(()),
        };
        let payload = udp.payload().to_vec();
        let key = UdpFlowKey {
            guest_src_port: udp.src_port(),
            dst_ip: ipv4.dst_addr(),
            dst_port: udp.dst_port(),
        };

        let dst =
            match nat::translate_outbound(&self.nat, key.dst_ip, key.dst_port, SLIRP_GATEWAY_IP) {
                Some(addr) => addr,
                None => {
                    trace!(
                        "SLIRP UDP: deny-list reject dst={}:{} from guest_port={}",
                        key.dst_ip,
                        key.dst_port,
                        key.guest_src_port
                    );
                    return Ok(());
                }
            };

        let flow_key = FlowKey::Udp(key);
        let entry: &mut UdpFlowEntry = match self.flow_table.entry(flow_key) {
            std::collections::hash_map::Entry::Occupied(o) => match o.into_mut() {
                FlowEntry::Udp(e) => e,
                _ => unreachable!("FlowKey::Udp must map to FlowEntry::Udp"),
            },
            std::collections::hash_map::Entry::Vacant(v) => {
                let sock = match open_udp_flow_socket(dst) {
                    Ok(s) => s,
                    Err(e) => {
                        trace!("SLIRP UDP: open flow socket failed: {e}");
                        return Ok(());
                    }
                };
                match v.insert(FlowEntry::Udp(UdpFlowEntry {
                    sock,
                    last_activity: Instant::now(),
                })) {
                    FlowEntry::Udp(e) => e,
                    _ => unreachable!(),
                }
            }
        };
        entry.last_activity = Instant::now();

        if let Err(e) = entry.sock.send(&payload) {
            trace!("SLIRP UDP: send failed: {e}");
        }
        Ok(())
    }

    // ── ICMP echo forwarding ─────────────────────────────────────────

    /// Forward a guest ICMP echo request to the host kernel via an unprivileged
    /// `SOCK_DGRAM IPPROTO_ICMP` socket.
    ///
    /// The kernel rewrites the ICMP identifier on `send_to`; the entry stores
    /// the guest's original `ident` so the reply path (Task 1.3) can translate
    /// it back before injecting the frame into the guest.
    fn handle_icmp_frame(&mut self, ipv4: &Ipv4Packet<&[u8]>) -> Result<()> {
        let icmp = match Icmpv4Packet::new_checked(ipv4.payload()) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        let repr = match Icmpv4Repr::parse(&icmp, &Default::default()) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let (ident, seq_no, data) = match repr {
            Icmpv4Repr::EchoRequest {
                ident,
                seq_no,
                data,
            } => (ident, seq_no, data),
            _ => return Ok(()), // only echo request handled today
        };

        // Copy data before the mutable borrow of self.flow_table below.
        let data_owned: Vec<u8> = data.to_vec();

        let key = IcmpEchoKey {
            guest_id: ident,
            dst_ip: ipv4.dst_addr(),
        };
        let flow_key = FlowKey::IcmpEcho(key);
        let entry: &mut IcmpEchoEntry = match self.flow_table.entry(flow_key) {
            std::collections::hash_map::Entry::Occupied(occupied) => match occupied.into_mut() {
                FlowEntry::IcmpEcho(e) => e,
                _ => unreachable!("FlowKey::IcmpEcho must map to FlowEntry::IcmpEcho"),
            },
            std::collections::hash_map::Entry::Vacant(vacant) => {
                let sock = match open_icmp_socket() {
                    Ok(s) => s,
                    Err(e) => {
                        // Sysctl-driven fallback handled in Task 1.4.
                        trace!("SLIRP ICMP: open socket failed: {e}");
                        return Ok(());
                    }
                };
                match vacant.insert(FlowEntry::IcmpEcho(IcmpEchoEntry {
                    sock,
                    guest_id: ident,
                    last_activity: Instant::now(),
                })) {
                    FlowEntry::IcmpEcho(e) => e,
                    _ => unreachable!(),
                }
            }
        };
        entry.last_activity = Instant::now();

        // Build a wire ICMP echo packet with seq + data; the kernel will
        // rewrite the ident on send_to.
        let req = Icmpv4Repr::EchoRequest {
            ident: 0, // kernel rewrites
            seq_no,
            data: &data_owned,
        };
        let mut buf = vec![0u8; req.buffer_len()];
        let mut pkt = Icmpv4Packet::new_unchecked(&mut buf);
        req.emit(&mut pkt, &Default::default());

        let dst = SocketAddr::from((
            Ipv4Addr::from(ipv4.dst_addr().0),
            0u16, // port ignored for ICMP
        ));
        if let Err(e) = entry.sock.send_to(&buf, dst) {
            trace!("SLIRP ICMP: send_to failed: {e}");
        }
        Ok(())
    }

    // ── TCP NAT ─────────────────────────────────────────────────────

    fn handle_tcp_frame(&mut self, ipv4: &Ipv4Packet<&[u8]>) -> Result<()> {
        let tcp = match TcpPacket::new_checked(ipv4.payload()) {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };

        let src_ip = ipv4.src_addr();
        let dst_ip = ipv4.dst_addr();
        let src_port = tcp.src_port();
        let dst_port = tcp.dst_port();
        let seq = tcp.seq_number().0 as u32;

        let key = NatKey {
            guest_src_port: src_port,
            dst_ip,
            dst_port,
        };

        // SYN (new connection)
        if tcp.syn() && !tcp.ack() {
            debug!(
                "SLIRP TCP: SYN {}:{} -> {}:{}",
                src_ip, src_port, dst_ip, dst_port
            );

            // Phase 5 unified outbound translation: combines the gateway-loopback
            // rewrite + deny-list check in one pure-function call. Returns None if
            // the dst is denied; on Some, the SocketAddr already has the right
            // host IP (loopback for the gateway, original for everything else).
            let dst_addr =
                match nat::translate_outbound(&self.nat, dst_ip, dst_port, SLIRP_GATEWAY_IP) {
                    Some(addr) => addr,
                    None => {
                        warn!(
                            "SLIRP TCP: connection to {}:{} denied by network deny list",
                            dst_ip, dst_port
                        );
                        let rst = build_tcp_packet_static(
                            dst_ip,
                            SLIRP_GUEST_IP,
                            dst_port,
                            src_port,
                            0,
                            seq + 1,
                            TcpControl::Rst,
                            &[],
                        );
                        self.inject_to_guest.push(rst);
                        return Ok(());
                    }
                };

            // Check max concurrent connections
            let tcp_flow_count = self
                .flow_table
                .keys()
                .filter(|k| matches!(k, FlowKey::Tcp(_)))
                .count();
            if tcp_flow_count >= self.max_concurrent_connections {
                warn!(
                    "SLIRP TCP: max concurrent connections ({}) reached, rejecting SYN to {}:{}",
                    self.max_concurrent_connections, dst_ip, dst_port
                );
                let rst = build_tcp_packet_static(
                    dst_ip,
                    SLIRP_GUEST_IP,
                    dst_port,
                    src_port,
                    0,
                    seq + 1,
                    TcpControl::Rst,
                    &[],
                );
                self.inject_to_guest.push(rst);
                return Ok(());
            }

            // Check rate limit
            if !self.check_rate_limit() {
                warn!(
                    "SLIRP TCP: connection rate limit ({}/s) exceeded, rejecting SYN to {}:{}",
                    self.max_connections_per_second, dst_ip, dst_port
                );
                let rst = build_tcp_packet_static(
                    dst_ip,
                    SLIRP_GUEST_IP,
                    dst_port,
                    src_port,
                    0,
                    seq + 1,
                    TcpControl::Rst,
                    &[],
                );
                self.inject_to_guest.push(rst);
                return Ok(());
            }

            // Remove any stale entry with the same key
            self.flow_table.remove(&FlowKey::Tcp(key));

            // Connect to the host address resolved by translate_outbound above.
            match TcpStream::connect_timeout(&dst_addr, Duration::from_secs(3)) {
                Ok(stream) => {
                    stream.set_nonblocking(true).ok();
                    let our_seq: u32 = rand_seq();
                    let entry = TcpNatEntry {
                        host_stream: stream,
                        state: TcpNatState::SynReceived,
                        our_seq,
                        guest_ack: seq + 1,
                        last_activity: Instant::now(),
                        bytes_in_flight: 0,
                    };
                    self.flow_table
                        .insert(FlowKey::Tcp(key), FlowEntry::Tcp(entry));

                    // Send SYN-ACK back to guest
                    let syn_ack = build_tcp_packet_static(
                        dst_ip,
                        SLIRP_GUEST_IP,
                        dst_port,
                        src_port,
                        our_seq,
                        seq + 1,
                        TcpControl::Syn,
                        &[],
                    );
                    self.inject_to_guest.push(syn_ack);
                    debug!("SLIRP TCP: SYN-ACK sent for {}:{}", dst_ip, dst_port);
                }
                Err(e) => {
                    warn!(
                        "SLIRP TCP: connect to {}:{} failed: {}",
                        dst_ip, dst_port, e
                    );
                    // Send RST to guest
                    let rst = build_tcp_packet_static(
                        dst_ip,
                        SLIRP_GUEST_IP,
                        dst_port,
                        src_port,
                        0,
                        seq + 1,
                        TcpControl::Rst,
                        &[],
                    );
                    self.inject_to_guest.push(rst);
                }
            }
            return Ok(());
        }

        // Look up existing connection
        let flow_key = FlowKey::Tcp(key);
        let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) else {
            trace!(
                "SLIRP TCP: no NAT entry for {}:{} -> {}:{}",
                src_ip,
                src_port,
                dst_ip,
                dst_port
            );
            return Ok(());
        };

        entry.last_activity = Instant::now();

        // Inbound port-forward: guest's SYN-ACK completing the host-initiated
        // 3-way handshake.  We synthesized a SYN to the guest (5.5b.2/5.5b.3);
        // the guest's kernel accepted it and replied with SYN+ACK.  Send an ACK
        // back so the guest's TCP stack transitions to Established on its side,
        // then record our state as Established too.
        //
        // NatKey for the inbound flow: guest_src_port = guest service port,
        // dst_ip = SLIRP_GATEWAY_IP, dst_port = the ephemeral high port we
        // used as the SYN's source port.  The ACK frame therefore flows
        // src=SLIRP_GATEWAY_IP:dst_port → dst=SLIRP_GUEST_IP:guest_src_port.
        if entry.state == TcpNatState::SynSent && tcp.syn() && tcp.ack() {
            let ack_frame = build_tcp_packet_static(
                SLIRP_GATEWAY_IP,              // src_ip  — the "host" side of the forward
                SLIRP_GUEST_IP,                // dst_ip  — the guest
                key.dst_port, // src_port — high ephemeral port we sent the SYN from
                key.guest_src_port, // dst_port — the guest's service port
                entry.our_seq.wrapping_add(1), // seq — our ISN + 1 (SYN consumed one)
                tcp.seq_number().0.wrapping_add(1) as u32, // ack — guest ISN + 1
                TcpControl::None,
                &[],
            );
            self.inject_to_guest.push(ack_frame);
            entry.our_seq = entry.our_seq.wrapping_add(1);
            entry.guest_ack = tcp.seq_number().0.wrapping_add(1) as u32;
            entry.state = TcpNatState::Established;
            trace!(
                "SLIRP TCP: inbound 3WH complete for guest_port={} high_port={}, → Established",
                key.guest_src_port,
                key.dst_port
            );
            return Ok(());
        }

        // ACK (completing handshake or acknowledging data)
        if tcp.ack() && entry.state == TcpNatState::SynReceived {
            entry.state = TcpNatState::Established;
            // our_seq was the SYN-ACK seq, so now it's +1
            entry.our_seq += 1;
            debug!(
                "SLIRP TCP: connection established for {}:{}",
                dst_ip, dst_port
            );
        }

        // ACK-driven consume: when the guest acknowledges data we sent via
        // peek-based relay (Task 3.3), read those bytes from the kernel recv
        // buffer to advance the kernel's read pointer.  Without this step the
        // kernel buffer fills up and recv_peek keeps returning the same bytes.
        //
        // Only runs in Established state — the SynReceived ACK above does not
        // carry data acknowledgements from us yet (bytes_in_flight == 0 then).
        if tcp.ack() && entry.state == TcpNatState::Established && entry.bytes_in_flight > 0 {
            // segment_ack: what the guest is now confirming it has received
            // from us (our send-side sequence space).
            let segment_ack: u32 = tcp.ack_number().0 as u32;

            // last_sent_acked: the highest our-seq the guest had already
            // confirmed before this segment.  `our_seq` is the *next* byte we
            // would send, so subtracting bytes_in_flight gives the start of the
            // in-flight window.
            // All arithmetic is wrapping — TCP sequence numbers wrap at 2^32.
            let last_sent_acked: u32 = entry.our_seq.wrapping_sub(entry.bytes_in_flight);

            // acked_bytes: how many new bytes the guest acknowledged in this
            // segment.  Guards:
            //   > 0   — ACK actually advances (not a duplicate or stale ACK)
            //   <= bytes_in_flight — guest cannot ack more than we've sent
            //   (defends against malformed / spoofed ACKs from a guest)
            let acked_bytes: u32 = segment_ack.wrapping_sub(last_sent_acked);

            if acked_bytes > 0 && acked_bytes <= entry.bytes_in_flight {
                let mut sink = [0u8; 65536];
                let mut to_drain = acked_bytes as usize;
                let mut drained: u32 = 0;
                while to_drain > 0 {
                    let want = to_drain.min(sink.len());
                    match entry.host_stream.read(&mut sink[..want]) {
                        Ok(0) => break, // EOF — nothing more to drain
                        Ok(n) => {
                            to_drain -= n;
                            drained = drained.wrapping_add(n as u32);
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            warn!(
                                "SLIRP TCP: ACK-driven read failed on flow guest_port={}, marking Closed: {}",
                                key.guest_src_port, e
                            );
                            entry.state = TcpNatState::Closed;
                            break;
                        }
                    }
                }
                entry.bytes_in_flight = entry.bytes_in_flight.wrapping_sub(drained);
                trace!(
                    "SLIRP TCP: ACK consumed {} bytes from kernel (in_flight now={}, segment_ack={})",
                    drained, entry.bytes_in_flight, segment_ack
                );
            }
        }

        let payload = tcp.payload();
        if !payload.is_empty() && entry.state == TcpNatState::Established {
            // Phase 3 guest→host: rely on the kernel's send buffer + TCP
            // retransmit for backpressure.  ACK only the bytes the kernel
            // accepted right now; on WouldBlock, don't ACK at all and let
            // the guest retransmit.  No userspace buffering, no 256 KB cap.
            let payload_seq = seq;
            let n_written = match entry.host_stream.write(payload) {
                Ok(n) => n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                Err(e) => {
                    warn!(
                        "SLIRP TCP: write to host failed on flow guest_port={}, marking Closed: {}",
                        key.guest_src_port, e
                    );
                    entry.state = TcpNatState::Closed;
                    return Ok(());
                }
            };

            if n_written > 0 {
                let ack_seq = payload_seq.wrapping_add(n_written as u32);
                entry.guest_ack = ack_seq;
                let ack_frame = build_tcp_packet_static(
                    dst_ip,
                    SLIRP_GUEST_IP,
                    dst_port,
                    src_port,
                    entry.our_seq,
                    entry.guest_ack,
                    TcpControl::None,
                    &[],
                );
                self.inject_to_guest.push(ack_frame);
                trace!(
                    "SLIRP TCP guest→host: wrote {}/{} bytes, ACK={}",
                    n_written,
                    payload.len(),
                    ack_seq
                );
            }
            // else: kernel send buffer full (WouldBlock) — don't ACK.
            // Guest TCP will retransmit; kernel buffer drains over time.
        }

        // FIN from guest
        if tcp.fin() {
            debug!("SLIRP TCP: FIN from guest for {}:{}", dst_ip, dst_port);
            entry.guest_ack = seq.wrapping_add(1);
            let fin_ack_frame = build_tcp_packet_static(
                dst_ip,
                SLIRP_GUEST_IP,
                dst_port,
                src_port,
                entry.our_seq,
                entry.guest_ack,
                TcpControl::Fin,
                &[],
            );
            self.inject_to_guest.push(fin_ack_frame);
            entry.our_seq = entry.our_seq.wrapping_add(1);
            entry.state = TcpNatState::Closed;
        }

        // RST from guest
        if tcp.rst() {
            debug!("SLIRP TCP: RST from guest for {}:{}", dst_ip, dst_port);
            entry.state = TcpNatState::Closed;
        }

        Ok(())
    }

    /// Relay data from host TCP connections to guest
    fn relay_tcp_nat_data(&mut self) {
        let mut to_remove: Vec<FlowKey> = Vec::new();
        // Collect frames to inject (built separately to avoid borrow issues)
        let mut frames_to_inject: Vec<Vec<u8>> = Vec::new();

        let tcp_flow_keys: Vec<FlowKey> = self
            .flow_table
            .keys()
            .copied()
            .filter(|k| matches!(k, FlowKey::Tcp(_)))
            .collect();

        for flow_key in tcp_flow_keys {
            let FlowKey::Tcp(key) = flow_key else {
                continue;
            };
            let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) else {
                continue;
            };

            if entry.state == TcpNatState::Closed {
                to_remove.push(flow_key);
                continue;
            }
            if entry.last_activity.elapsed() > Duration::from_secs(300) {
                to_remove.push(flow_key);
                continue;
            }
            if entry.state != TcpNatState::Established {
                continue;
            }

            // Phase 3 host→guest path: peek what's in the kernel recv buffer
            // without consuming. Send only the un-ACK'd portion (bytes past
            // what we've already sent). The kernel's socket buffer holds the
            // outstanding data; Task 3.4's ACK-driven `read()` consumes it
            // once the guest ACKs.
            let mut peek_buf = [0u8; 65536];
            match recv_peek(&entry.host_stream, &mut peek_buf) {
                Ok(0) => {
                    // Host closed the connection. Send FIN to guest below.
                    debug!(
                        "SLIRP TCP: host EOF on flow guest_port={}, marking Closed",
                        key.guest_src_port
                    );
                    entry.state = TcpNatState::Closed;
                }
                Ok(peek_n) => {
                    let in_flight = entry.bytes_in_flight as usize;
                    if peek_n > in_flight {
                        let new_bytes = &peek_buf[in_flight..peek_n];
                        let mut sent_total: usize = 0;
                        for chunk in new_bytes.chunks(MTU - 54) {
                            let frame = build_tcp_packet_static(
                                key.dst_ip,
                                SLIRP_GUEST_IP,
                                key.dst_port,
                                key.guest_src_port,
                                entry.our_seq,
                                entry.guest_ack,
                                TcpControl::None,
                                chunk,
                            );
                            frames_to_inject.push(frame);
                            entry.our_seq = entry.our_seq.wrapping_add(chunk.len() as u32);
                            entry.bytes_in_flight =
                                entry.bytes_in_flight.wrapping_add(chunk.len() as u32);
                            sent_total += chunk.len();
                        }
                        entry.last_activity = Instant::now();
                        trace!(
                            "SLIRP TCP relay: peeked {} bytes (in_flight before={}, sent now={})",
                            peek_n,
                            in_flight,
                            sent_total
                        );
                    }
                    // else: kernel buffer holds only already-in-flight bytes.
                    // Wait for guest ACK before sending more (Task 3.4).
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Kernel recv buffer empty; nothing to do this poll.
                }
                Err(e) => {
                    warn!(
                        "SLIRP TCP: recv_peek failed on flow guest_port={}, marking Closed: {}",
                        key.guest_src_port, e
                    );
                    entry.state = TcpNatState::Closed;
                }
            }

            // FIN if host closed
            if entry.state == TcpNatState::Closed {
                let fin = build_tcp_packet_static(
                    key.dst_ip,
                    SLIRP_GUEST_IP,
                    key.dst_port,
                    key.guest_src_port,
                    entry.our_seq,
                    entry.guest_ack,
                    TcpControl::Fin,
                    &[],
                );
                frames_to_inject.push(fin);
            }
        }

        self.inject_to_guest.append(&mut frames_to_inject);

        for flow_key in to_remove {
            self.flow_table.remove(&flow_key);
        }
    }

    /// Drain replies from each active ICMP echo socket and emit echo-reply
    /// frames to the guest.
    ///
    /// Called on every [`drain_to_guest`] tick.  Entries idle longer than
    /// `ICMP_IDLE_TIMEOUT` are evicted.
    fn relay_icmp_echo(&mut self) {
        const ICMP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
        let now = Instant::now();

        let flow_keys: Vec<FlowKey> = self
            .flow_table
            .keys()
            .copied()
            .filter(|k| matches!(k, FlowKey::IcmpEcho(_)))
            .collect();
        for flow_key in flow_keys {
            let FlowKey::IcmpEcho(key) = flow_key else {
                continue;
            };
            let frame = {
                let Some(FlowEntry::IcmpEcho(entry)) = self.flow_table.get_mut(&flow_key) else {
                    continue;
                };
                if now.duration_since(entry.last_activity) > ICMP_IDLE_TIMEOUT {
                    None // mark for removal below
                } else {
                    let mut buf = [0u8; 1500];
                    match entry.sock.recv_from(&mut buf) {
                        Ok((n, _addr)) => {
                            entry.last_activity = now;
                            // Wrap in Some to distinguish from the idle-timeout
                            // None arm in the outer match.
                            Some(Self::build_icmp_echo_reply_to_guest(
                                key.dst_ip,
                                entry.guest_id,
                                &buf[..n],
                            ))
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(_) => continue,
                    }
                }
            };
            match frame {
                None => {
                    // Idle timeout — evict entry.
                    self.flow_table.remove(&FlowKey::IcmpEcho(key));
                }
                Some(Some(frame_bytes)) => self.inject_to_guest.push(frame_bytes),
                Some(None) => {} // build failed; drop silently
            }
        }
    }

    /// Build an Ethernet/IPv4/ICMP echo-reply frame addressed to the guest.
    ///
    /// `src_ip` is the original ping destination (becomes the reply source).
    /// `guest_id` is the ICMP identifier to write into the reply so the guest
    /// can match it against its outstanding echo request.
    /// `raw_icmp` is the raw ICMP packet received from the host kernel via
    /// the `SOCK_DGRAM IPPROTO_ICMP` socket (no IP header; ICMP type + code +
    /// checksum + payload).
    ///
    /// Returns `Some(frame)` on success, `None` if the packet cannot be parsed
    /// or is not an `EchoReply`.
    fn build_icmp_echo_reply_to_guest(
        src_ip: Ipv4Address,
        guest_id: u16,
        raw_icmp: &[u8],
    ) -> Option<Vec<u8>> {
        let icmp = Icmpv4Packet::new_checked(raw_icmp).ok()?;
        let parsed = Icmpv4Repr::parse(&icmp, &Default::default()).ok()?;
        // Copy the payload before `icmp` / `parsed` go out of scope so we can
        // build the outgoing `EchoReply` with a fresh borrow.  Mirrors the
        // same pattern used in `handle_icmp_frame` (Task 1.2).
        let (seq_no, data_owned) = match parsed {
            Icmpv4Repr::EchoReply { seq_no, data, .. } => (seq_no, data.to_vec()),
            _ => return None,
        };
        let reply = Icmpv4Repr::EchoReply {
            ident: guest_id,
            seq_no,
            data: &data_owned,
        };
        let ip_repr = Ipv4Repr {
            src_addr: src_ip,
            dst_addr: SLIRP_GUEST_IP,
            next_header: IpProtocol::Icmp,
            payload_len: reply.buffer_len(),
            hop_limit: 64,
        };
        let eth_repr = EthernetRepr {
            src_addr: EthernetAddress(GATEWAY_MAC),
            dst_addr: EthernetAddress(GUEST_MAC),
            ethertype: EthernetProtocol::Ipv4,
        };
        let total = 14 + ip_repr.buffer_len() + reply.buffer_len();
        let mut buf = vec![0u8; total];
        let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
        eth_repr.emit(&mut eth);
        let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
        ip_repr.emit(&mut ip, &Default::default());
        let mut icmp_out = Icmpv4Packet::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
        reply.emit(&mut icmp_out, &Default::default());
        Some(buf)
    }

    /// Drain replies from each active UDP flow socket and emit UDP frames to
    /// the guest.
    ///
    /// Called on every [`drain_to_guest`] tick.  Each connected socket is
    /// polled non-blocking; `WouldBlock` and other errors are silently skipped
    /// so a stale or unreachable flow never stalls the relay loop.
    ///
    /// Reply addressing mirrors the original guest datagram in reverse: the
    /// frame's IP source is the original destination (`key.dst_ip`) and UDP
    /// source port is `key.dst_port`; the destination is the guest IP and
    /// `key.guest_src_port`.
    fn relay_udp_flows(&mut self) {
        let now = Instant::now();
        // Reap idle flows; the per-flow connected socket is closed by Drop.
        let stale: Vec<FlowKey> = self
            .flow_table
            .iter()
            .filter(|(k, e)| {
                matches!(k, FlowKey::Udp(_))
                    && match e {
                        FlowEntry::Udp(entry) => {
                            now.duration_since(entry.last_activity) > UDP_IDLE_TIMEOUT
                        }
                        _ => false,
                    }
            })
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            self.flow_table.remove(&k);
        }

        let flow_keys: Vec<FlowKey> = self
            .flow_table
            .keys()
            .copied()
            .filter(|k| matches!(k, FlowKey::Udp(_)))
            .collect();
        for flow_key in flow_keys {
            let FlowKey::Udp(key) = flow_key else {
                continue;
            };
            let frame = {
                let Some(FlowEntry::Udp(entry)) = self.flow_table.get_mut(&flow_key) else {
                    continue;
                };
                let mut buf = [0u8; 1500];
                match entry.sock.recv(&mut buf) {
                    Ok(n) => {
                        entry.last_activity = now;
                        Self::build_udp_reply_to_guest(
                            key.dst_ip,
                            key.dst_port,
                            key.guest_src_port,
                            &buf[..n],
                        )
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => continue,
                }
            };
            if let Some(frame_bytes) = frame {
                self.inject_to_guest.push(frame_bytes);
            }
        }
    }

    /// Build an Ethernet/IPv4/UDP frame addressed to the guest, carrying a
    /// reply from a host-side UDP flow socket.
    ///
    /// - `src_ip` — original destination IP (becomes the reply source address).
    /// - `src_port` — original destination port (becomes the reply source port).
    /// - `dst_port` — guest's ephemeral source port (becomes the reply destination).
    /// - `payload` — raw UDP payload received from the host socket.
    ///
    /// Returns `Some(frame)` on success.  Currently infallible, but wrapped in
    /// `Option` for symmetry with [`build_icmp_echo_reply_to_guest`].
    fn build_udp_reply_to_guest(
        src_ip: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        let udp_repr = UdpRepr { src_port, dst_port };
        let ip_repr = Ipv4Repr {
            src_addr: src_ip,
            dst_addr: SLIRP_GUEST_IP,
            next_header: IpProtocol::Udp,
            payload_len: 8 + payload.len(),
            hop_limit: 64,
        };
        let eth_repr = EthernetRepr {
            src_addr: EthernetAddress(GATEWAY_MAC),
            dst_addr: EthernetAddress(GUEST_MAC),
            ethertype: EthernetProtocol::Ipv4,
        };
        let total = 14 + ip_repr.buffer_len() + 8 + payload.len();
        let mut buf = vec![0u8; total];
        let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
        eth_repr.emit(&mut eth);
        let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
        ip_repr.emit(&mut ip, &Default::default());
        let mut udp = UdpPacket::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
        udp_repr.emit(
            &mut udp,
            &IpAddress::Ipv4(src_ip),
            &IpAddress::Ipv4(SLIRP_GUEST_IP),
            payload.len(),
            |b| b.copy_from_slice(payload),
            &Default::default(),
        );
        Some(buf)
    }

    // ── Packet building helpers ──────────────────────────────────────

    fn build_udp_response(
        &self,
        src_ip: Ipv4Address,
        dst_ip: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        // Ethernet(14) + IPv4(20) + UDP(8) + payload
        let udp_len = 8 + payload.len();
        let ip_len = 20 + udp_len;
        let total_len = 14 + ip_len;
        let mut buf = vec![0u8; total_len];

        // Ethernet header
        buf[0..6].copy_from_slice(&GUEST_MAC);
        buf[6..12].copy_from_slice(&GATEWAY_MAC);
        buf[12] = 0x08;
        buf[13] = 0x00; // IPv4

        // IPv4 header
        let ip = &mut buf[14..14 + 20];
        ip[0] = 0x45; // version=4, IHL=5
        let ip_total = ip_len as u16;
        ip[2..4].copy_from_slice(&ip_total.to_be_bytes());
        ip[4..6].copy_from_slice(&rand_id().to_be_bytes()); // identification
        ip[8] = 64; // TTL
        ip[9] = 17; // UDP
        ip[12..16].copy_from_slice(src_ip.as_bytes());
        ip[16..20].copy_from_slice(dst_ip.as_bytes());
        // Checksum
        let cksum = ipv4_checksum(&buf[14..34]);
        buf[24..26].copy_from_slice(&cksum.to_be_bytes());

        // UDP header
        let udp_offset = 34;
        buf[udp_offset..udp_offset + 2].copy_from_slice(&src_port.to_be_bytes());
        buf[udp_offset + 2..udp_offset + 4].copy_from_slice(&dst_port.to_be_bytes());
        let udp_length = udp_len as u16;
        buf[udp_offset + 4..udp_offset + 6].copy_from_slice(&udp_length.to_be_bytes());
        // UDP checksum = 0 (optional for IPv4)
        buf[udp_offset + 6..udp_offset + 8].copy_from_slice(&[0, 0]);

        // Payload
        buf[udp_offset + 8..].copy_from_slice(payload);

        buf
    }
}

impl NetworkBackend for SlirpBackend {
    fn process_guest_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        SlirpBackend::process_guest_frame(self, frame).map_err(|e| io::Error::other(e.to_string()))
    }

    fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
        SlirpBackend::drain_to_guest(self, out)
    }
}

/// Build a TCP packet (free function to avoid borrow issues with &self methods)
#[allow(clippy::too_many_arguments)]
fn build_tcp_packet_static(
    src_ip: Ipv4Address,
    dst_ip: Ipv4Address,
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    control: TcpControl,
    payload: &[u8],
) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port,
        dst_port,
        seq_number: TcpSeqNumber(seq as i32),
        ack_number: Some(TcpSeqNumber(ack as i32)),
        window_len: TCP_WINDOW,
        window_scale: None,
        control,
        max_seg_size: if control == TcpControl::Syn {
            Some(MTU as u16 - 40)
        } else {
            None
        },
        sack_permitted: false,
        sack_ranges: [None; 3],
        payload,
    };

    let ip_repr = Ipv4Repr {
        src_addr: src_ip,
        dst_addr: dst_ip,
        next_header: IpProtocol::Tcp,
        payload_len: tcp_repr.header_len() + payload.len(),
        hop_limit: 64,
    };

    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: EthernetAddress(GUEST_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };

    let checksums = ChecksumCapabilities::default();
    let total_len =
        eth_repr.buffer_len() + ip_repr.buffer_len() + tcp_repr.header_len() + payload.len();
    let mut buf = vec![0u8; total_len];

    let mut eth_frame = EthernetFrame::new_unchecked(&mut buf);
    eth_repr.emit(&mut eth_frame);
    let mut ip_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
    ip_repr.emit(&mut ip_packet, &checksums);
    let mut tcp_packet = TcpPacket::new_unchecked(ip_packet.payload_mut());
    tcp_repr.emit(
        &mut tcp_packet,
        &IpAddress::Ipv4(src_ip),
        &IpAddress::Ipv4(dst_ip),
        &checksums,
    );

    buf
}

// ── Utility functions ────────────────────────────────────────────────

fn rand_seq() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u32).wrapping_mul(2654435761) // Knuth multiplicative hash
}

fn rand_id() -> u16 {
    (rand_seq() & 0xFFFF) as u16
}

fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for i in (0..header.len()).step_by(2) {
        if i == 10 {
            continue;
        } // skip checksum field
        let word = if i + 1 < header.len() {
            ((header[i] as u32) << 8) | (header[i + 1] as u32)
        } else {
            (header[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !sum as u16
}

impl Default for SlirpBackend {
    fn default() -> Self {
        Self::new().expect("Failed to create default SlirpBackend")
    }
}

/// Test-only helpers — not compiled into production builds.
///
/// These are `#[cfg(test)]` methods on `SlirpBackend` that allow unit tests to
/// insert synthetic flow entries without widening the visibility of private types.
/// The full behavioral contract for the SynSent → Established transition is
/// pinned in the E2E test `tcp_inbound_syn_ack_completes_handshake` below and
/// will be further exercised end-to-end in task 5.5b.5
/// (`tcp_port_forward_inbound` in `tests/network_baseline.rs`).
#[cfg(test)]
impl SlirpBackend {
    /// Insert a synthetic `SynSent` entry into the flow table.
    ///
    /// Used by `tcp_inbound_syn_ack_completes_handshake` to pre-seed the state
    /// that would normally be created by `synthesize_inbound_syn` (5.5b.2).
    ///
    /// `guest_port`: the guest's listening service port (e.g. 8080).
    /// `high_port`:  the ephemeral source port we used for the synthesized SYN.
    /// `our_isn`:    the ISN we put in the synthesized SYN.
    /// `host_stream`: a `TcpStream` representing the accepted host-side connection.
    pub(crate) fn insert_synthetic_synsent_entry(
        &mut self,
        guest_port: u16,
        high_port: u16,
        our_isn: u32,
        host_stream: TcpStream,
    ) {
        let key = NatKey {
            guest_src_port: guest_port,
            dst_ip: SLIRP_GATEWAY_IP,
            dst_port: high_port,
        };
        let entry = TcpNatEntry {
            host_stream,
            state: TcpNatState::SynSent,
            our_seq: our_isn,
            guest_ack: 0,
            last_activity: Instant::now(),
            bytes_in_flight: 0,
        };
        self.flow_table
            .insert(FlowKey::Tcp(key), FlowEntry::Tcp(entry));
    }

    /// Return the `TcpNatState` for the flow identified by `(guest_port, GATEWAY_IP, high_port)`,
    /// or `None` if no such entry exists in the flow table.
    pub(crate) fn tcp_flow_state(&self, guest_port: u16, high_port: u16) -> Option<TcpNatState> {
        let key = NatKey {
            guest_src_port: guest_port,
            dst_ip: SLIRP_GATEWAY_IP,
            dst_port: high_port,
        };
        match self.flow_table.get(&FlowKey::Tcp(key))? {
            FlowEntry::Tcp(entry) => Some(entry.state),
            _ => None,
        }
    }

    /// Count how many frames queued for injection carry the given TCP flags.
    ///
    /// Checks `inject_to_guest` for Ethernet/IPv4/TCP frames where the TCP
    /// `ack` flag is set and the `syn` flag is clear (i.e. a plain ACK).
    pub(crate) fn injected_plain_ack_count(&self) -> usize {
        self.inject_to_guest
            .iter()
            .filter(|frame| {
                // Ethernet(14) + IPv4(≥20) + TCP(≥20) = ≥54 bytes.
                if frame.len() < 54 {
                    return false;
                }
                // Parse TCP flags from the fixed-offset byte: ETH(14) + IP(20) + flags@13
                let tcp_offset = 14 + 20;
                let flags_byte = frame[tcp_offset + 13];
                let ack = flags_byte & 0x10 != 0;
                let syn = flags_byte & 0x02 != 0;
                ack && !syn
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slirp_constants() {
        assert_eq!(SLIRP_GUEST_IP, Ipv4Address::new(10, 0, 2, 15));
        assert_eq!(SLIRP_GATEWAY_IP, Ipv4Address::new(10, 0, 2, 2));
        assert_eq!(SLIRP_DNS_IP, Ipv4Address::new(10, 0, 2, 3));
    }

    #[test]
    fn test_mac_addresses() {
        assert!(GUEST_MAC[0] & 0x02 != 0);
        assert!(GATEWAY_MAC[0] & 0x02 != 0);
    }

    #[test]
    fn test_slirp_stack_creation() {
        let stack = SlirpBackend::new();
        assert!(stack.is_ok());
    }

    #[test]
    fn test_ipv4_checksum() {
        let mut header = [0u8; 20];
        header[0] = 0x45;
        let cksum = ipv4_checksum(&header);
        assert_ne!(cksum, 0);
    }

    /// Build a TCP frame from the guest (SLIRP_GUEST_IP) to a given destination.
    ///
    /// Used by `tcp_inbound_syn_ack_completes_handshake` to synthesize the
    /// guest's SYN-ACK reply to our port-forward SYN.
    fn build_guest_tcp_frame(
        dst_ip: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack_number: u32,
        control: TcpControl,
        set_ack_flag: bool,
    ) -> Vec<u8> {
        use smoltcp::wire::{
            EthernetAddress, EthernetFrame, EthernetRepr, IpAddress, Ipv4Packet, Ipv4Repr,
            TcpPacket, TcpRepr, TcpSeqNumber,
        };
        let tcp_repr = TcpRepr {
            src_port,
            dst_port,
            control,
            seq_number: TcpSeqNumber(seq as i32),
            ack_number: if set_ack_flag {
                Some(TcpSeqNumber(ack_number as i32))
            } else {
                None
            },
            window_len: 65535,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload: &[],
        };
        let ip_repr = Ipv4Repr {
            src_addr: SLIRP_GUEST_IP,
            dst_addr: dst_ip,
            next_header: smoltcp::wire::IpProtocol::Tcp,
            payload_len: tcp_repr.buffer_len(),
            hop_limit: 64,
        };
        let eth_repr = EthernetRepr {
            src_addr: EthernetAddress(GUEST_MAC),
            dst_addr: EthernetAddress(GATEWAY_MAC),
            ethertype: smoltcp::wire::EthernetProtocol::Ipv4,
        };
        let checksums = smoltcp::phy::ChecksumCapabilities::default();
        let total = eth_repr.buffer_len() + ip_repr.buffer_len() + tcp_repr.buffer_len();
        let mut buf = vec![0u8; total];
        let mut eth = EthernetFrame::new_unchecked(&mut buf);
        eth_repr.emit(&mut eth);
        let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
        ip_repr.emit(&mut ip, &checksums);
        let mut tcp = TcpPacket::new_unchecked(ip.payload_mut());
        tcp_repr.emit(
            &mut tcp,
            &IpAddress::Ipv4(SLIRP_GUEST_IP),
            &IpAddress::Ipv4(dst_ip),
            &checksums,
        );
        buf
    }

    /// Verify that a guest SYN-ACK frame on a SynSent entry:
    ///   (a) transitions the flow state to Established, and
    ///   (b) queues exactly one plain ACK frame towards the guest.
    ///
    /// The full E2E behavioral contract (including host-listener wiring) will be
    /// pinned in `tests/network_baseline.rs::tcp_port_forward_inbound` (task 5.5b.5).
    #[test]
    fn tcp_inbound_syn_ack_completes_handshake() {
        use std::net::TcpListener;

        let guest_port: u16 = 8080;
        let high_port: u16 = 44000;
        let our_isn: u32 = 0x0000_1000;
        let guest_isn: u32 = 0xDEAD_BEEF;

        // Create a loopback TcpStream pair for the host_stream field.
        // The stream is never read/written in this unit test — we only
        // exercise the TCP state machine.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let host_stream =
            TcpStream::connect(listener.local_addr().unwrap()).expect("connect loopback");
        host_stream.set_nonblocking(true).ok();

        let mut backend = SlirpBackend::new().expect("SlirpBackend::new");
        backend.insert_synthetic_synsent_entry(guest_port, high_port, our_isn, host_stream);

        // Confirm state is SynSent before feeding the SYN-ACK.
        assert_eq!(
            backend.tcp_flow_state(guest_port, high_port),
            Some(TcpNatState::SynSent),
            "entry must start as SynSent"
        );

        // Build the guest's SYN-ACK: src=GUEST:guest_port, dst=GATEWAY:high_port,
        // SYN+ACK, seq=guest_isn, ack=our_isn+1.
        let syn_ack = build_guest_tcp_frame(
            SLIRP_GATEWAY_IP,
            guest_port,
            high_port,
            guest_isn,
            our_isn.wrapping_add(1),
            TcpControl::Syn, // SYN flag — combined with ACK flag via ack_number=Some(...)
            true,            // set ACK flag
        );

        backend
            .process_guest_frame(&syn_ack)
            .expect("process SYN-ACK");

        // (a) state must be Established now.
        assert_eq!(
            backend.tcp_flow_state(guest_port, high_port),
            Some(TcpNatState::Established),
            "state must be Established after SYN-ACK"
        );

        // (b) exactly one plain ACK must have been queued for injection to the guest.
        assert_eq!(
            backend.injected_plain_ack_count(),
            1,
            "exactly one plain ACK must be queued for the guest"
        );
    }
}
