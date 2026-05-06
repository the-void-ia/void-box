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
//!   `flow_table: HashMap<FlowKey, FlowEntry>`. Per-protocol relay logic
//!   dispatches on the FlowEntry variant.
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
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::network::epoll_dispatch::{EpollDispatch, EpollEvent, RegisterMode, Waker};
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
    TcpOption, TcpPacket, TcpRepr, TcpSeqNumber, UdpPacket, UdpRepr,
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
/// Window-scale shift we advertise on SYN-ACK frames. Matches passt's
/// default. 7 means each unit in `window_len` represents 128 bytes,
/// extending the effective window from 64 KiB to 8 MiB.
const OUR_WINDOW_SCALE: u8 = 7;
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout for TCP entries stuck in the LastAck state (i.e. we sent a FIN
/// but the guest's final ACK never arrived). Two TCP MSLs (2 × 30 s = 60 s)
/// matches the POSIX TIME_WAIT recommendation and prevents LastAck entries
/// from leaking indefinitely when a guest drops the final ACK.
const LAST_ACK_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for TCP entries stuck in the Connecting state (i.e. a non-blocking
/// `connect()` was issued but EPOLLOUT readiness never arrived — a silent
/// firewall drop is the common cause). Matches the pre-Phase-6.2 synchronous
/// `connect_timeout(3 s)` so guest-visible behavior is unchanged.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// ICMP unprivileged probe state.
///
/// `0` = unknown (not yet probed), `1` = available, `2` = unavailable
/// (kernel returned `EACCES` or `EPERM` — typically `net.ipv4.ping_group_range`
/// excludes the calling GID). Once set to `2`, `open_icmp_socket` short-circuits.
static ICMP_PROBE: AtomicU8 = AtomicU8::new(0);

// ──────────────────────────────────────────────────────────────────────
//  EpollDispatch flow tokens
// ──────────────────────────────────────────────────────────────────────

/// High-byte protocol tag embedded in the upper 8 bits of a `FlowToken`.
/// The lower 56 bits are a monotonic per-flow counter (see `FLOW_TOKEN_COUNTER`).
/// The tag lets the relay loop distinguish protocol families with a bitmask
/// instead of a separate lookup; the counter guarantees global uniqueness
/// even when two flows share the same port tuple.
const PROTO_TAG_MASK: u64 = 0xFF00_0000_0000_0000;
const PROTO_TAG_TCP: u64 = 0x0100_0000_0000_0000;
const PROTO_TAG_UDP: u64 = 0x0200_0000_0000_0000;
const PROTO_TAG_ICMP: u64 = 0x0300_0000_0000_0000;
const PROTO_TAG_LISTEN: u64 = 0x0400_0000_0000_0000;

/// Monotonic counter for flow token allocation.  The lower 56 bits of each
/// `FlowToken` are drawn from here; the upper 8 bits carry `PROTO_TAG_*`.
/// 2^56 unique tokens are available before wrap — effectively infinite for
/// any realistic process lifetime.
static FLOW_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Parse the `WindowScale` option from a raw TCP options buffer.
///
/// Returns 0 when no `WindowScale` option is present (the guest is not
/// advertising window scaling, so shift = 0 means no scaling applied).
fn parse_tcp_window_scale(options: &[u8]) -> u8 {
    let mut remaining = options;
    loop {
        match TcpOption::parse(remaining) {
            Ok((_, TcpOption::EndOfList)) | Err(_) => break,
            Ok((_, TcpOption::WindowScale(scale))) => return scale,
            Ok((rest, _)) => remaining = rest,
        }
    }
    0
}

/// Allocate a fresh, globally unique `FlowToken` tagged for the given protocol.
///
/// The lower 56 bits are drawn from a relaxed monotonic counter shared across
/// all `SlirpBackend` instances.  The upper 8 bits carry `proto_tag` so relay
/// loops can demux by protocol without an additional map lookup.
fn next_flow_token(proto_tag: u64) -> u64 {
    let counter = FLOW_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed) & 0x00FF_FFFF_FFFF_FFFF;
    proto_tag | counter
}

/// Build an epoll token for a port-forward listener FD.
///
/// The high byte carries `PROTO_TAG_LISTEN`; the low 16 bits encode the
/// host port. Each port-forward rule has a distinct host port, so tokens
/// are unique across all registered listeners.
fn flow_token_for_listener(host_port: u16) -> u64 {
    PROTO_TAG_LISTEN | u64::from(host_port)
}

// ──────────────────────────────────────────────────────────────────────
//  Inbound port-forward accept channel
// ──────────────────────────────────────────────────────────────────────

/// One accepted host-side TCP connection waiting to be forwarded into the guest.
///
/// Produced by [`SlirpBackend::process_listener_readiness`] (epoll-driven
/// accept) and consumed by [`SlirpBackend::process_pending_inbound_accepts`]
/// on the net-poll thread.
pub(crate) struct InboundAccept {
    /// The accepted host-side TCP stream (non-blocking after accept).
    host_stream: TcpStream,
    /// Ephemeral port used as the synthesized SYN source port on the gateway side.
    /// Derived from the peer's remote port so it is unique per connection.
    high_port: u16,
    /// Guest-side destination port (the service the guest is listening on).
    guest_port: u16,
}

// ──────────────────────────────────────────────────────────────────────
//  TCP NAT connection tracking
// ──────────────────────────────────────────────────────────────────────

/// TCP connection state for the SLIRP NAT relay.
///
/// The state machine models both guest-initiated and host-initiated half-close
/// sequences. `FinWait2` is omitted: distinguishing it from `FinWait1` requires
/// observing per-segment ACKs from the kernel, which the relay does not track.
/// Instead, the relay stays in `FinWait1` until host EOF arrives, then jumps
/// directly to `LastAck`.
///
/// State transitions:
///
/// ```text
///   Connecting ──SO_ERROR==0──► SynReceived ──ACK──► Established ──guest FIN──► FinWait1
///       │                                                  │  │
///       └ SO_ERROR != 0 / CONNECT_TIMEOUT ──► Closed       │  │
///                                                          │  │
///   SynSent ──SYN+ACK──► Established                       │  │
///                              │                            │  └─ host EOF ──► LastAck
///                              │ host EOF                    │                     │
///                              ▼                            │          guest ACK ──┘
///                          CloseWait ◄────────────────────┘            └──► Closed
///                              │ guest FIN
///                              ▼
///                           LastAck ──── LAST_ACK_TIMEOUT ────► Closed
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TcpNatState {
    /// Non-blocking connect issued; waiting for EPOLLOUT readiness to
    /// arrive on the host socket. On readiness we check
    /// `getsockopt(SO_ERROR)`: zero → transition to `SynReceived` and send
    /// SYN-ACK to guest; non-zero → send RST to guest and reap.
    Connecting,
    /// Guest sent SYN; we responded with SYN-ACK; waiting for guest's
    /// final ACK to complete the outbound 3-way handshake.
    SynReceived,
    /// We synthesized a SYN to the guest (port-forwarding); waiting
    /// for the guest's SYN-ACK to advance to Established.
    SynSent,
    /// Both sides exchanged handshake; data flows in both directions.
    Established,
    /// Guest closed its write side (sent FIN); we ACKed and called
    /// shutdown(Write) on the host socket. Host response data may still
    /// be in-flight — relay continues until host EOF.
    FinWait1,
    /// Host closed its write side first (we saw EOF); we sent a FIN to
    /// the guest. Guest may still send data, which the relay forwards.
    CloseWait,
    /// We have sent our FIN to the guest; waiting for the guest's final
    /// ACK. Reaped on ACK or after LAST_ACK_TIMEOUT.
    LastAck,
    /// Connection fully closed; entry pending removal from the flow table.
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
    /// Globally unique epoll token for this flow.  Allocated once on insert
    /// via `next_flow_token(PROTO_TAG_TCP)` and stored here so unregister
    /// sites never need to recompute it.
    flow_token: u64,
    /// Wall clock when the entry's state last changed. Used by
    /// LAST_ACK_TIMEOUT reaping in relay_tcp_nat_data so a missing
    /// final ACK doesn't leak the entry forever.
    last_state_change: Instant,
    /// True once we have sent our FIN to the guest. Prevents re-sending
    /// FIN on repeated epoll readiness events for the same transition.
    /// Read in relay_tcp_nat_data's FIN-emit logic (Task 3).
    our_fin_sent: bool,
    /// Guest's initial sequence number (`seq` from the original SYN
    /// frame).  Stashed here only for entries in `Connecting` state so
    /// the EPOLLOUT-driven completion path can build SYN-ACK with the
    /// correct ack number (= `guest_isn + 1`).  Once the entry transitions
    /// to `SynReceived` this field is no longer read.
    #[allow(dead_code)]
    // Read by EPOLLOUT-driven completion in relay_pending_connects (Task 5).
    guest_isn: u32,
    /// Guest's advertised receive window in bytes, scaled per
    /// `guest_window_scale`. Updated on every incoming TCP frame's
    /// `window_len`. Initial value 65535 matches an unscaled SYN.
    guest_window: u32,
    /// Window-scale shift the guest negotiated in its SYN. Zero
    /// means "guest does not support window scaling" (or we did not
    /// see a window-scale option in the SYN).
    guest_window_scale: u8,
    /// Cached value of `host_recv_window(host_stream)`. Refreshed
    /// every `RECV_WINDOW_TTL` instead of on every outgoing frame
    /// so the bulk-throughput data path doesn't issue a
    /// `getsockopt(TCP_INFO)` per packet. The window we advertise
    /// stays within `RECV_WINDOW_TTL` of reality, which is well below
    /// any realistic RTT.
    cached_recv_window: u16,
    /// Wall clock when `cached_recv_window` was last refreshed.
    /// `Instant::now() - RECV_WINDOW_TTL` at construction forces
    /// the first emit to populate the cache before advertising.
    cached_recv_window_at: Instant,
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
    /// Globally unique epoll token for this flow.  Allocated once on insert
    /// via `next_flow_token(PROTO_TAG_ICMP)` and stored here so unregister
    /// sites never need to recompute it.
    flow_token: u64,
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
    /// Globally unique epoll token for this flow.  Allocated once on insert
    /// via `next_flow_token(PROTO_TAG_UDP)` and stored here so unregister
    /// sites never need to recompute it.
    flow_token: u64,
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
        let errno = err.raw_os_error();
        let unprivileged_icmp_forbidden = errno == Some(libc::EACCES) || errno == Some(libc::EPERM);
        if unprivileged_icmp_forbidden {
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
    /// Unified flow table keyed by protocol + port tuple.
    ///
    /// All three protocols (TCP, UDP, ICMP echo) share this table so a single
    /// dispatch loop handles all active flows.
    flow_table: HashMap<FlowKey, FlowEntry>,
    /// Reverse map from `FlowToken` → `FlowKey` for O(1) readiness-event
    /// dispatch.  Maintained in sync with `flow_table`: every insert adds an
    /// entry; every remove clears it.
    token_to_key: HashMap<u64, FlowKey>,
    /// Live `TcpListener`s for each TCP port-forward rule, keyed by host port.
    /// The tuple value is `(listener, guest_port)`. Each listener's FD is
    /// registered with `EpollDispatch` under `PROTO_TAG_LISTEN`; readiness
    /// events drive the accept loop on the net-poll thread. No dedicated
    /// polling thread per rule.
    port_forward_listeners: HashMap<u16, (TcpListener, u16)>,
    /// Receiver end of the accept channel fed by
    /// [`bind_port_forward_listeners`] via [`SlirpBackend::process_listener_readiness`].
    /// Processed on the net-poll thread in
    /// [`SlirpBackend::process_pending_inbound_accepts`].
    pending_inbound_accepts: mpsc::Receiver<InboundAccept>,
    /// Sender end of `pending_inbound_accepts`. Kept alive so the channel
    /// stays open when no listener threads are running (e.g. in tests) and
    /// so test helpers can inject [`InboundAccept`] values directly.
    #[allow(dead_code)]
    accept_sender: mpsc::Sender<InboundAccept>,
    /// Epoll dispatcher for host socket readiness.  `EpollDispatch` is
    /// `Sync`: `register`/`unregister` and `wait_with_timeout` are
    /// kernel-serialized on the same epoll fd, so no `Mutex` wrapper is
    /// needed.  The `Arc` lets the net-poll thread share the dispatcher
    /// without holding the device lock.
    epoll: Arc<EpollDispatch>,
    /// Cloneable waker that interrupts `EpollDispatch::wait_with_timeout`.
    /// Used after flow-table mutations to unblock the poll thread immediately.
    epoll_waker: Waker,
    /// Ready events fed by the net-poll thread after each blocking
    /// epoll_wait. drain_to_guest drains this on every call without
    /// any EpollDispatch lock contention.
    pending_events: Mutex<Vec<EpollEvent>>,
    /// Flow keys queued for removal because their state advanced to
    /// Closed in a non-relay code path (e.g. guest FIN/RST in
    /// handle_tcp_frame). Drained at the bottom of relay_tcp_nat_data
    /// without scanning the full flow_table.
    pending_close: Vec<FlowKey>,
    /// Set to `true` the first time `push_ready_events` is called —
    /// signals "an external poller (net_poll_thread) is feeding us
    /// readiness events." When true, `drain_to_guest` skips its
    /// non-blocking-poll fallback (one mutex op + one epoll_wait
    /// syscall per call, ~310 ns overhead) and only consumes
    /// `pending_events`. Tests/benches without a net_poll_thread
    /// keep the fallback so synthetic harnesses still observe
    /// readiness.
    has_external_poller: AtomicBool,
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

        let (accept_tx, accept_rx) = mpsc::channel::<InboundAccept>();

        let epoll_inner = EpollDispatch::new()?;
        let epoll_waker = epoll_inner.waker();
        let epoll = Arc::new(epoll_inner);

        // Bind listeners for port-forwards and register their FDs with epoll.
        let port_forward_listeners = bind_port_forward_listeners(&nat, &epoll);

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
            token_to_key: HashMap::new(),
            port_forward_listeners,
            pending_inbound_accepts: accept_rx,
            accept_sender: accept_tx,
            epoll,
            epoll_waker,
            pending_events: Mutex::new(Vec::new()),
            pending_close: Vec::new(),
            has_external_poller: AtomicBool::new(false),
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

    /// Drain the inbound-accept channel and seed a `SynSent` flow-table entry
    /// plus a synthesized SYN frame for each accepted connection.
    ///
    /// Accept connections from any port-forward listeners whose FDs are ready
    /// in `ready` and push them onto the inbound-accept channel for
    /// [`process_pending_inbound_accepts`] to consume.
    ///
    /// Drains until `WouldBlock` so that a burst of connections arriving
    /// between two epoll wakeups is not spread across multiple ticks.
    fn process_listener_readiness(&mut self, ready: &[EpollEvent]) {
        // Accepted connections are collected here first so that the borrow on
        // `port_forward_listeners` ends before we call `accept_sender.send`.
        let mut accepted_batch: Vec<InboundAccept> = Vec::new();
        let mut sender_failed = false;

        for event in ready {
            if !event.readable || event.token & PROTO_TAG_MASK != PROTO_TAG_LISTEN {
                continue;
            }
            let host_port = (event.token & 0xFFFF) as u16;
            let Some((listener, guest_port)) = self.port_forward_listeners.get(&host_port) else {
                continue;
            };
            let guest_port = *guest_port;
            // Drain the listener — multiple connections may have arrived in one
            // EPOLLIN edge.
            loop {
                match listener.accept() {
                    Ok((stream, peer_addr)) => {
                        let high_port = peer_addr.port();
                        let _ = stream.set_nonblocking(true);
                        trace!(
                            host_port,
                            guest_port,
                            high_port,
                            peer = %peer_addr,
                            "SLIRP port-forward: accepted connection"
                        );
                        accepted_batch.push(InboundAccept {
                            host_stream: stream,
                            high_port,
                            guest_port,
                        });
                    }
                    Err(ref would_block) if would_block.kind() == io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(accept_error) => {
                        warn!(
                            host_port,
                            error = %accept_error,
                            "SLIRP port-forward: accept error"
                        );
                        break;
                    }
                }
            }
        }

        // Borrow of `port_forward_listeners` has ended; send the batch.
        for accepted in accepted_batch {
            if self.accept_sender.send(accepted).is_err() {
                sender_failed = true;
                break;
            }
        }
        let _ = sender_failed; // receiver drop handled gracefully on next tick
    }

    /// Called at the top of [`drain_to_guest`] so all `SlirpBackend` mutation
    /// stays on the net-poll thread — same single-writer lock model as the rest
    /// of the relay pipeline. `process_listener_readiness` enqueues accepted
    /// connections via the mpsc channel; this method drains that channel and
    /// seeds the flow table.
    fn process_pending_inbound_accepts(&mut self) {
        loop {
            let accepted = match self.pending_inbound_accepts.try_recv() {
                Ok(accepted) => accepted,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            };
            let InboundAccept {
                host_stream,
                high_port,
                guest_port,
            } = accepted;
            let our_isn = rand_seq();
            let key = NatKey {
                guest_src_port: guest_port,
                dst_ip: SLIRP_GATEWAY_IP,
                dst_port: high_port,
            };
            let token = next_flow_token(PROTO_TAG_TCP);
            let cached_recv_window = host_recv_window(host_stream.as_raw_fd());
            let entry = TcpNatEntry {
                host_stream,
                state: TcpNatState::SynSent,
                our_seq: our_isn,
                guest_ack: 0,
                last_activity: Instant::now(),
                bytes_in_flight: 0,
                flow_token: token,
                last_state_change: Instant::now(),
                our_fin_sent: false,
                // Inbound port-forward entries never enter Connecting; the
                // EPOLLOUT-driven completion path only reads guest_isn for
                // outbound (guest-initiated) SYNs.
                guest_isn: 0,
                guest_window: 65535,
                guest_window_scale: 0,
                cached_recv_window,
                cached_recv_window_at: Instant::now(),
            };
            let host_fd = entry.host_stream.as_raw_fd();
            let flow_key = FlowKey::Tcp(key);
            self.flow_table.insert(flow_key, FlowEntry::Tcp(entry));
            self.token_to_key.insert(token, flow_key);
            if let Err(e) = self.epoll.register(host_fd, token, RegisterMode::Read) {
                warn!(
                    host_port = high_port,
                    guest_port,
                    fd = host_fd,
                    error = %e,
                    "SLIRP port-forward: epoll register failed; flow present but readiness-driven relay disabled"
                );
            }
            self.epoll_waker.wake();
            let syn_frame = synthesize_inbound_syn(high_port, guest_port, our_isn);
            self.inject_to_guest.push(syn_frame);
            trace!(
                host_port = high_port,
                guest_port,
                our_isn,
                "SLIRP port-forward: seeded SynSent entry"
            );
        }
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

        // Track inject_to_guest growth so we can wake the net-poll
        // thread if this call queued any frames. The poll thread blocks
        // in epoll_wait waiting on FD readiness; an ACK queued during
        // guest TX has no FD-side signal (the guest is the writer, not
        // the reader on the SLIRP-side socket). Without an explicit
        // wake the ACK sits up to epoll_wait's timeout before being
        // flushed — TCP send window stalls, throughput drops 10×.
        let inject_len_before = self.inject_to_guest.len();

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

        if self.inject_to_guest.len() > inject_len_before {
            self.epoll_waker.wake();
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

        // 3. Collect ready events.
        //
        // Always drain `pending_events` first — that's the queue
        // `net_poll_thread` fills via `push_ready_events` after every
        // successful `epoll_wait`. If we skipped this and only polled
        // epoll directly, we would lose every event the net-poll thread
        // already drained: level-triggered EPOLLIN doesn't re-fire for
        // data the kernel already reported, so the next non-blocking
        // poll returns 0 events even when there's work to do. CRR
        // connections then wait one full 50 ms epoll cycle for the NEXT
        // data event before their first data is relayed.
        //
        // Then, only if no net-poll thread has populated the queue
        // (unit tests / benches), fall back to a non-blocking poll on
        // the epoll FD ourselves. `try_lock` keeps that fallback safe
        // under contention.
        let ready: Vec<EpollEvent> = {
            let mut events: Vec<EpollEvent> = {
                let mut queue = self.pending_events.lock().unwrap();
                std::mem::take(&mut *queue)
            };
            // Fallback non-blocking poll only when no external poller
            // (net_poll_thread) is feeding us events — otherwise we'd
            // pay one mutex op + one epoll_wait syscall per call
            // (~310 ns) for nothing. The flag is one-way: set by the
            // first push_ready_events and stays set for the backend's
            // lifetime.
            if events.is_empty() && !self.has_external_poller.load(Ordering::Relaxed) {
                let _ = self
                    .epoll
                    .wait_with_timeout(&mut events, std::time::Duration::ZERO);
            }
            events
        };

        // 0a. Accept any newly-ready listener connections (may push into
        //     accept_sender for the next step).
        self.process_listener_readiness(&ready);

        // 0b. Drain the accept channel (epoll-driven listeners + test helpers).
        self.process_pending_inbound_accepts();

        // 3b. Complete any async connects whose EPOLLOUT fired this cycle.
        //     Must run before relay_tcp_nat_data so a flow that transitions
        //     from Connecting→SynReceived within this cycle can be skipped by
        //     the data-relay pass (it's not yet in Established).
        self.relay_pending_connects(&ready);

        // 4. Process TCP NAT data relay.
        self.relay_tcp_nat_data(&ready);

        // 5. Relay ICMP echo replies from host sockets back to the guest.
        self.relay_icmp_echo(&ready);

        // 6. Relay UDP flow replies from host sockets back to the guest.
        self.relay_udp_flows(&ready);

        // 7. Collect frames: smoltcp ARP responses + our NAT-built frames.
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
        // Track whether this is a new entry so we can register it with epoll.
        let mut new_host_fd: Option<std::os::fd::RawFd> = None;
        let mut new_token: u64 = 0;
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
                let token = next_flow_token(PROTO_TAG_UDP);
                new_host_fd = Some(sock.as_raw_fd());
                new_token = token;
                match v.insert(FlowEntry::Udp(UdpFlowEntry {
                    sock,
                    last_activity: Instant::now(),
                    flow_token: token,
                })) {
                    FlowEntry::Udp(e) => e,
                    _ => unreachable!(),
                }
            }
        };
        entry.last_activity = Instant::now();

        if let Some(host_fd) = new_host_fd {
            self.token_to_key.insert(new_token, flow_key);
            if let Err(e) = self.epoll.register(host_fd, new_token, RegisterMode::Read) {
                warn!(
                    guest_src_port = key.guest_src_port,
                    dst_ip = %key.dst_ip,
                    dst_port = key.dst_port,
                    fd = host_fd,
                    error = %e,
                    "SLIRP UDP: epoll register failed; flow present but readiness-driven relay disabled"
                );
            }
            self.epoll_waker.wake();
        }

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
        // Track whether this is a new entry so we can register it with epoll.
        let mut new_icmp_fd: Option<std::os::fd::RawFd> = None;
        let mut new_token: u64 = 0;
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
                let token = next_flow_token(PROTO_TAG_ICMP);
                new_icmp_fd = Some(sock.as_raw_fd());
                new_token = token;
                match vacant.insert(FlowEntry::IcmpEcho(IcmpEchoEntry {
                    sock,
                    guest_id: ident,
                    last_activity: Instant::now(),
                    flow_token: token,
                })) {
                    FlowEntry::IcmpEcho(e) => e,
                    _ => unreachable!(),
                }
            }
        };
        entry.last_activity = Instant::now();

        if let Some(host_fd) = new_icmp_fd {
            self.token_to_key.insert(new_token, flow_key);
            if let Err(e) = self.epoll.register(host_fd, new_token, RegisterMode::Read) {
                warn!(
                    guest_id = key.guest_id,
                    dst_ip = %key.dst_ip,
                    fd = host_fd,
                    error = %e,
                    "SLIRP ICMP: epoll register failed; flow present but readiness-driven relay disabled"
                );
            }
            self.epoll_waker.wake();
        }

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

            // Parse window scaling from the SYN's TCP options so it can be
            // stored on the flow entry.  Zero when the guest omits the option.
            let syn_window_scale = parse_tcp_window_scale(tcp.options());
            let syn_window: u32 = u32::from(tcp.window_len()) << syn_window_scale;
            trace!(
                "SLIRP TCP SYN: guest window_scale={} initial_window={}",
                syn_window_scale,
                syn_window
            );

            // Unified outbound translation: combines the gateway-loopback
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
                            65535,
                            None,
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
                    65535,
                    None,
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
                    65535,
                    None,
                );
                self.inject_to_guest.push(rst);
                return Ok(());
            }

            // Remove any stale entry with the same key, unregistering its FD
            // from the epoll set to avoid a dangling registration.
            if let Some(FlowEntry::Tcp(stale)) = self.flow_table.get(&FlowKey::Tcp(key)) {
                self.token_to_key.remove(&stale.flow_token);
                self.epoll.unregister(stale.host_stream.as_raw_fd()).ok();
            }
            self.flow_table.remove(&FlowKey::Tcp(key));

            // Issue a non-blocking connect to the host address resolved by
            // translate_outbound above.  socket2's Type::STREAM.nonblocking()
            // sets O_NONBLOCK at socket creation so the connect() syscall
            // returns EINPROGRESS immediately for destinations that require a
            // network round-trip (the common case).  The vCPU thread is never
            // blocked.  EPOLLOUT readiness on the connecting socket, handled
            // in relay_pending_connects(), signals completion.
            let socket = match Socket::new(
                Domain::IPV4,
                Type::STREAM.nonblocking(),
                Some(Protocol::TCP),
            ) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "SLIRP TCP: socket() failed for {}:{}: {}",
                        dst_ip, dst_port, e
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
                        65535,
                        None,
                    );
                    self.inject_to_guest.push(rst);
                    return Ok(());
                }
            };
            let sockaddr = SockAddr::from(dst_addr);
            match socket.connect(&sockaddr) {
                Ok(()) => {
                    // Connected immediately (loopback fast path).  Promote
                    // straight to SynReceived and send SYN-ACK without waiting
                    // for EPOLLOUT.
                    let stream = TcpStream::from(socket);
                    let host_fd = stream.as_raw_fd();
                    let our_seq: u32 = rand_seq();
                    let token = next_flow_token(PROTO_TAG_TCP);
                    let flow_key = FlowKey::Tcp(key);
                    let cached_recv_window = host_recv_window(host_fd);
                    let entry = TcpNatEntry {
                        host_stream: stream,
                        state: TcpNatState::SynReceived,
                        our_seq,
                        guest_ack: seq + 1,
                        last_activity: Instant::now(),
                        bytes_in_flight: 0,
                        flow_token: token,
                        last_state_change: Instant::now(),
                        our_fin_sent: false,
                        guest_isn: seq,
                        guest_window: syn_window,
                        guest_window_scale: syn_window_scale,
                        cached_recv_window,
                        cached_recv_window_at: Instant::now(),
                    };
                    self.flow_table.insert(flow_key, FlowEntry::Tcp(entry));
                    self.token_to_key.insert(token, flow_key);
                    if let Err(e) = self.epoll.register(host_fd, token, RegisterMode::Read) {
                        warn!(
                            guest_src_port = key.guest_src_port,
                            dst_ip = %key.dst_ip,
                            dst_port = key.dst_port,
                            fd = host_fd,
                            error = %e,
                            "SLIRP TCP: epoll register failed; flow present but readiness-driven relay disabled"
                        );
                    }
                    self.epoll_waker.wake();
                    let syn_ack = build_tcp_packet_static(
                        dst_ip,
                        SLIRP_GUEST_IP,
                        dst_port,
                        src_port,
                        our_seq,
                        seq + 1,
                        TcpControl::Syn,
                        &[],
                        65535,
                        Some(OUR_WINDOW_SCALE),
                    );
                    self.inject_to_guest.push(syn_ack);
                    debug!(
                        "SLIRP TCP: SYN-ACK sent for {}:{} (immediate connect)",
                        dst_ip, dst_port
                    );
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {
                    // Async connect in progress.  Insert a Connecting entry,
                    // register the FD for EPOLLOUT, and return without sending
                    // a SYN-ACK.  relay_pending_connects() will promote this
                    // entry to SynReceived and send the SYN-ACK once the
                    // kernel's connect finishes.
                    let stream = TcpStream::from(socket);
                    let host_fd = stream.as_raw_fd();
                    let our_seq: u32 = rand_seq();
                    let token = next_flow_token(PROTO_TAG_TCP);
                    let flow_key = FlowKey::Tcp(key);
                    let cached_recv_window = host_recv_window(host_fd);
                    let entry = TcpNatEntry {
                        host_stream: stream,
                        state: TcpNatState::Connecting,
                        our_seq,
                        guest_ack: seq + 1,
                        last_activity: Instant::now(),
                        bytes_in_flight: 0,
                        flow_token: token,
                        last_state_change: Instant::now(),
                        our_fin_sent: false,
                        guest_isn: seq,
                        guest_window: syn_window,
                        guest_window_scale: syn_window_scale,
                        cached_recv_window,
                        cached_recv_window_at: Instant::now(),
                    };
                    self.flow_table.insert(flow_key, FlowEntry::Tcp(entry));
                    self.token_to_key.insert(token, flow_key);
                    if let Err(e) = self.epoll.register(host_fd, token, RegisterMode::Write) {
                        warn!(
                            guest_src_port = key.guest_src_port,
                            dst_ip = %key.dst_ip,
                            dst_port = key.dst_port,
                            fd = host_fd,
                            error = %e,
                            "SLIRP TCP: epoll register (Write) failed for connect-in-progress; \
                             flow will time out via CONNECT_TIMEOUT"
                        );
                    }
                    self.epoll_waker.wake();
                    debug!(
                        "SLIRP TCP: connect-in-progress for {}:{} (our_seq={})",
                        dst_ip, dst_port, our_seq
                    );
                }
                Err(e) => {
                    // Synchronous connect failure (address unreachable, etc.).
                    warn!(
                        "SLIRP TCP: connect to {}:{} failed synchronously: {}",
                        dst_ip, dst_port, e
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
                        65535,
                        None,
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

        // Track whether this processing path sets state=Closed so we can
        // enqueue the key in pending_close once the entry borrow ends.
        // FIN/RST paths push to pending_close and return early; mid-function
        // error paths (ACK-driven read failure, write failure) set this flag.
        let mut closed_by_error = false;

        entry.last_activity = Instant::now();

        // Track the most recent window advertisement from the guest.  Runs for
        // every frame (data, ACK, FIN, RST) so `guest_window` always reflects
        // the current receive-buffer headroom on the guest side.
        entry.guest_window = u32::from(tcp.window_len()) << entry.guest_window_scale;

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
                cached_host_recv_window(entry),
                None,
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

        // ACK while in LastAck — guest acknowledged our FIN. Reap.
        // Placed before the SynReceived ACK branch to be explicit (the
        // states are mutually exclusive, but explicit ordering is clearer).
        if tcp.ack() && entry.state == TcpNatState::LastAck {
            debug!("SLIRP TCP: LastAck → Closed for {}:{}", dst_ip, dst_port);
            entry.state = TcpNatState::Closed;
            self.pending_close.push(flow_key);
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
        // Runs in Established and FinWait1: even after the guest half-closes
        // (FinWait1), the relay may have already sent host response data that
        // the guest must ACK. Draining the kernel buffer on ACK lets
        // recv_peek see Ok(0) (EOF) once all data is consumed, which then
        // triggers the FinWait1 → LastAck transition and the final FIN to guest.
        //
        // Does not run in SynReceived — that ACK doesn't carry data acks yet.
        let ack_consume_state = matches!(
            entry.state,
            TcpNatState::Established | TcpNatState::FinWait1
        );
        if tcp.ack() && ack_consume_state && entry.bytes_in_flight > 0 {
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
                            closed_by_error = true;
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
        // Forward guest data in Established and CloseWait: in CloseWait the
        // host closed its write side but can still read data the guest sends.
        // FinWait1 is not included — in that state the guest already closed
        // its write side (we called shutdown(Write) on the host socket).
        let forward_data = matches!(
            entry.state,
            TcpNatState::Established | TcpNatState::CloseWait
        );
        if !payload.is_empty() && forward_data {
            // Guest→host backpressure: rely on the kernel's send buffer + TCP
            // retransmit.  ACK only the bytes the kernel accepted right now;
            // on WouldBlock, don't ACK at all and let the guest retransmit.
            // No userspace buffering, no fixed byte-cap on in-flight data.
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
                    // entry last used above; borrow ends here before pending_close push.
                    self.pending_close.push(flow_key);
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
                    cached_host_recv_window(entry),
                    None,
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
            match entry.state {
                TcpNatState::Established => {
                    entry.guest_ack = seq.wrapping_add(1);

                    // ACK the guest's FIN — but don't send our own FIN yet. Host
                    // application may have data still to send. We transition to
                    // FinWait1 and shut down the host socket's write side so the
                    // host knows no more data is coming from the guest.
                    let ack_frame = build_tcp_packet_static(
                        dst_ip,
                        SLIRP_GUEST_IP,
                        dst_port,
                        src_port,
                        entry.our_seq,
                        entry.guest_ack,
                        TcpControl::None,
                        &[],
                        cached_host_recv_window(entry),
                        None,
                    );
                    self.inject_to_guest.push(ack_frame);

                    if let Err(e) = entry.host_stream.shutdown(std::net::Shutdown::Write) {
                        warn!(
                            "SLIRP TCP: shutdown(Write) failed on guest FIN, falling back \
                               to immediate close: {}",
                            e
                        );
                        entry.state = TcpNatState::Closed;
                        self.pending_close.push(flow_key);
                        return Ok(());
                    }

                    entry.state = TcpNatState::FinWait1;
                    entry.last_state_change = Instant::now();
                    trace!(
                        "SLIRP TCP: state Established → FinWait1 for {}:{}",
                        dst_ip,
                        dst_port
                    );
                    return Ok(());
                }
                TcpNatState::CloseWait => {
                    // Host already closed its write side; guest just closed
                    // too. Shut down our write side of host_stream so the
                    // host application's read sees EOF, ACK the guest's FIN,
                    // and transition to LastAck waiting for guest's final ACK
                    // of our FIN (which was already sent when we entered
                    // CloseWait).
                    entry.guest_ack = seq.wrapping_add(1);
                    let ack_frame = build_tcp_packet_static(
                        dst_ip,
                        SLIRP_GUEST_IP,
                        dst_port,
                        src_port,
                        entry.our_seq,
                        entry.guest_ack,
                        TcpControl::None,
                        &[],
                        cached_host_recv_window(entry),
                        None,
                    );
                    self.inject_to_guest.push(ack_frame);
                    if let Err(e) = entry.host_stream.shutdown(std::net::Shutdown::Write) {
                        // Non-fatal: host already closed; ENOTCONN or similar
                        // is expected in some cases.
                        trace!("SLIRP TCP: shutdown(Write) in CloseWait (non-fatal): {}", e);
                    }
                    entry.state = TcpNatState::LastAck;
                    entry.last_state_change = Instant::now();
                    trace!(
                        "SLIRP TCP: state CloseWait → LastAck for {}:{}",
                        dst_ip,
                        dst_port
                    );
                    return Ok(());
                }
                _ => {
                    // Repeat FIN or unexpected — ACK and stay where we are.
                }
            }
        }

        // RST from guest
        if tcp.rst() {
            debug!("SLIRP TCP: RST from guest for {}:{}", dst_ip, dst_port);
            entry.state = TcpNatState::Closed;
            // entry last used above; borrow ends before pending_close push.
            self.pending_close.push(flow_key);
            return Ok(());
        }

        // ACK-driven read failure marked the entry Closed but execution
        // continues here (no early return). Push to pending_close so
        // relay_tcp_nat_data removes the flow without an O(n) sweep.
        if closed_by_error {
            self.pending_close.push(flow_key);
        }

        Ok(())
    }

    /// Drive async-connect completion for flows in the `Connecting` state.
    ///
    /// For each EPOLLOUT event that maps to a `Connecting` flow, we call
    /// `getsockopt(SO_ERROR)` to learn the actual connect outcome:
    ///
    /// - `SO_ERROR == 0`: connected.  Transition to `SynReceived`, send
    ///   SYN-ACK to guest, re-register the fd for `EPOLLIN` (Read) via
    ///   `EPOLL_CTL_MOD` so data relay can begin.
    /// - `SO_ERROR != 0`: failed.  Send RST to guest, mark Closed, enqueue
    ///   in `pending_close` for cleanup on the next `relay_tcp_nat_data` pass.
    ///
    /// Called from `drain_to_guest` before `relay_tcp_nat_data` so a flow that
    /// completes connect and has data arrive in the same epoll cycle is handled
    /// correctly: the transition fires here, and data relay skips the flow
    /// because it is still in `SynReceived` (not yet `Established`).
    fn relay_pending_connects(&mut self, ready: &[EpollEvent]) {
        // Collect keys for Connecting flows with an EPOLLOUT event this cycle.
        // We copy the keys to avoid holding a borrow on self while mutating.
        let connecting_keys: Vec<FlowKey> = ready
            .iter()
            .filter(|event| event.writable && event.token & PROTO_TAG_MASK == PROTO_TAG_TCP)
            .filter_map(|event| self.token_to_key.get(&event.token).copied())
            .filter(|flow_key| {
                matches!(
                    self.flow_table.get(flow_key),
                    Some(FlowEntry::Tcp(e)) if e.state == TcpNatState::Connecting
                )
            })
            .collect();

        for flow_key in connecting_keys {
            let FlowKey::Tcp(key) = flow_key else {
                continue;
            };

            // Check SO_ERROR to learn the actual connect outcome.
            let (host_fd, guest_isn, our_seq, flow_token) = {
                let Some(FlowEntry::Tcp(entry)) = self.flow_table.get(&flow_key) else {
                    continue;
                };
                (
                    entry.host_stream.as_raw_fd(),
                    entry.guest_isn,
                    entry.our_seq,
                    entry.flow_token,
                )
            };

            let mut so_error: libc::c_int = 0;
            let mut so_error_len: libc::socklen_t =
                std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: getsockopt with SOL_SOCKET/SO_ERROR writes one c_int.
            let getsockopt_result = unsafe {
                libc::getsockopt(
                    host_fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut so_error as *mut _ as *mut libc::c_void,
                    &mut so_error_len,
                )
            };

            if getsockopt_result < 0 || so_error != 0 {
                // Connect failed.
                let connect_err = if getsockopt_result < 0 {
                    io::Error::last_os_error()
                } else {
                    io::Error::from_raw_os_error(so_error)
                };
                warn!(
                    guest_src_port = key.guest_src_port,
                    dst_ip = %key.dst_ip,
                    dst_port = key.dst_port,
                    error = %connect_err,
                    "SLIRP TCP: async connect failed; sending RST to guest"
                );
                let rst = build_tcp_packet_static(
                    key.dst_ip,
                    SLIRP_GUEST_IP,
                    key.dst_port,
                    key.guest_src_port,
                    0,
                    guest_isn.wrapping_add(1),
                    TcpControl::Rst,
                    &[],
                    65535,
                    None,
                );
                self.inject_to_guest.push(rst);
                if let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) {
                    entry.state = TcpNatState::Closed;
                    entry.last_state_change = Instant::now();
                }
                self.pending_close.push(flow_key);
                continue;
            }

            // Connected.  Re-register for Read before sending SYN-ACK so
            // the next drain_to_guest cycle can relay host→guest data.
            // EPOLL_CTL_MOD is atomic — no window where a data event could
            // be lost between a DEL and ADD.
            if let Err(e) = self.epoll.modify(host_fd, flow_token, RegisterMode::Read) {
                warn!(
                    guest_src_port = key.guest_src_port,
                    error = %e,
                    "SLIRP TCP: epoll modify Write→Read failed; flow may stall on data relay"
                );
            }

            // Transition to SynReceived and send SYN-ACK.
            if let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) {
                entry.state = TcpNatState::SynReceived;
                entry.last_state_change = Instant::now();
            }
            let syn_ack = build_tcp_packet_static(
                key.dst_ip,
                SLIRP_GUEST_IP,
                key.dst_port,
                key.guest_src_port,
                our_seq,
                guest_isn.wrapping_add(1),
                TcpControl::Syn,
                &[],
                65535,
                Some(OUR_WINDOW_SCALE),
            );
            self.inject_to_guest.push(syn_ack);
            debug!(
                "SLIRP TCP: async connect OK for {}:{} guest_src_port={}; SYN-ACK sent",
                key.dst_ip, key.dst_port, key.guest_src_port
            );
        }
    }

    /// Relay data from host TCP connections to guest, driven by epoll readiness.
    ///
    /// Closed flows enqueued by handle_tcp_frame (FIN/RST) are drained from
    /// `pending_close` and removed promptly. Idle-timeout detection iterates
    /// only the flow table entries directly, avoiding a separate Vec allocation.
    /// Data relay is restricted to flows with an EPOLLIN event in `ready`.
    fn relay_tcp_nat_data(&mut self, ready: &[EpollEvent]) {
        // Collect frames to inject (built separately to avoid borrow issues)
        let mut frames_to_inject: Vec<Vec<u8>> = Vec::new();

        // Seed removal set from flows already marked Closed by handle_tcp_frame
        // (FIN/RST path) via the pending_close queue. HashSet gives O(1)
        // membership checks in the idle-timeout sweep and readiness filter below,
        // avoiding the O(n*k) cost of Vec::contains under connection churn.
        let mut to_remove_set: std::collections::HashSet<FlowKey> =
            std::mem::take(&mut self.pending_close)
                .into_iter()
                .collect();

        // Timeout sweep: one pass over the flow table handles two independent
        // timeout conditions without a separate Vec or extra allocation.
        // Uses to_remove_set (HashSet) for O(1) membership checks.
        const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
        for (flow_key, entry) in &self.flow_table {
            if let FlowEntry::Tcp(tcp_entry) = entry {
                if to_remove_set.contains(flow_key) {
                    continue;
                }
                // Standard idle-timeout: 300 s of inactivity in any state.
                if tcp_entry.last_activity.elapsed() > TCP_IDLE_TIMEOUT {
                    to_remove_set.insert(*flow_key);
                    continue;
                }
                // LastAck-timeout: final guest ACK never arrived. Reap so a
                // misbehaving or crashed guest doesn't leak entries forever.
                if tcp_entry.state == TcpNatState::LastAck
                    && tcp_entry.last_state_change.elapsed() > LAST_ACK_TIMEOUT
                {
                    warn!(
                        "SLIRP TCP: LastAck timeout for guest_port={}, reaping",
                        if let FlowKey::Tcp(k) = flow_key {
                            k.guest_src_port
                        } else {
                            0
                        }
                    );
                    to_remove_set.insert(*flow_key);
                }
                // Connecting-timeout: the kernel is still issuing SYNs (silent
                // firewall drop) and EPOLLOUT has not fired within CONNECT_TIMEOUT.
                // Send RST to guest and reap.  This matches the pre-Phase-6.2
                // synchronous connect_timeout(3 s) behavior.
                if tcp_entry.state == TcpNatState::Connecting
                    && tcp_entry.last_state_change.elapsed() > CONNECT_TIMEOUT
                {
                    warn!(
                        "SLIRP TCP: Connecting timeout for guest_port={}, reaping",
                        if let FlowKey::Tcp(k) = flow_key {
                            k.guest_src_port
                        } else {
                            0
                        }
                    );
                    to_remove_set.insert(*flow_key);
                }
            }
        }

        let mut tcp_flow_keys: Vec<FlowKey> = Vec::new();
        for event in ready {
            if !event.readable || event.token & PROTO_TAG_MASK != PROTO_TAG_TCP {
                continue;
            }
            let Some(flow_key) = self.token_to_key.get(&event.token).copied() else {
                continue;
            };
            if to_remove_set.contains(&flow_key) {
                continue;
            }
            tcp_flow_keys.push(flow_key);
        }

        for flow_key in tcp_flow_keys {
            let FlowKey::Tcp(key) = flow_key else {
                continue;
            };

            let mut became_closed = false;
            let mut fin_frame: Option<Vec<u8>> = None;

            {
                let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) else {
                    continue;
                };

                // Relay data for Established and FinWait1 (guest half-closed;
                // host may still have data to send). Skip all other states.
                let relay_data = matches!(
                    entry.state,
                    TcpNatState::Established | TcpNatState::FinWait1
                );
                if !relay_data {
                    continue;
                }

                // Host→guest path: peek what's in the kernel recv buffer
                // without consuming. Send only the un-ACK'd portion (bytes past
                // what we've already sent). The kernel's socket buffer holds the
                // outstanding data; ACK-driven `read()` consumes it once the
                // guest ACKs.
                let mut peek_buf = [0u8; 65536];
                match recv_peek(&entry.host_stream, &mut peek_buf) {
                    Ok(0) => {
                        // Host closed the connection.
                        debug!(
                            "SLIRP TCP: host EOF on flow guest_port={}",
                            key.guest_src_port
                        );
                        match entry.state {
                            TcpNatState::Established => {
                                // Host closed first → CloseWait. We send FIN to
                                // guest; guest may still have data to send which
                                // we'll forward (host's write side may be closed
                                // already — that's a guest write failure, not our
                                // concern).
                                entry.state = TcpNatState::CloseWait;
                                entry.last_state_change = Instant::now();
                                trace!(
                                    "SLIRP TCP: Established → CloseWait for guest_port={}",
                                    key.guest_src_port
                                );
                                became_closed = false;
                            }
                            TcpNatState::FinWait1 => {
                                // Guest closed first; now host has finished writing.
                                // Send FIN to guest, transition to LastAck.
                                entry.state = TcpNatState::LastAck;
                                entry.last_state_change = Instant::now();
                                trace!(
                                    "SLIRP TCP: FinWait1 → LastAck for guest_port={}",
                                    key.guest_src_port
                                );
                                became_closed = false;
                            }
                            _ => {
                                // Already in a closing state or Closed — no action.
                                became_closed = false;
                            }
                        }
                    }
                    Ok(peek_n) => {
                        let in_flight = entry.bytes_in_flight as usize;
                        if peek_n > in_flight {
                            let new_bytes = &peek_buf[in_flight..peek_n];
                            let mut sent_total: usize = 0;
                            let our_window = cached_host_recv_window(entry);
                            for chunk in new_bytes.chunks(MTU - 54) {
                                // Honour the guest's advertised receive window.
                                // `bytes_in_flight` tracks how many bytes the
                                // guest has not yet ACK'd; stop sending once we
                                // would exceed its buffer.
                                let window_remaining = (entry.guest_window as usize)
                                    .saturating_sub(entry.bytes_in_flight as usize);
                                if window_remaining == 0 {
                                    trace!(
                                        "SLIRP TCP: guest window exhausted on flow \
                                         guest_port={} (in_flight={}, window={})",
                                        key.guest_src_port,
                                        entry.bytes_in_flight,
                                        entry.guest_window
                                    );
                                    break;
                                }
                                let send_len = chunk.len().min(window_remaining);
                                let chunk = &chunk[..send_len];
                                let frame = build_tcp_packet_static(
                                    key.dst_ip,
                                    SLIRP_GUEST_IP,
                                    key.dst_port,
                                    key.guest_src_port,
                                    entry.our_seq,
                                    entry.guest_ack,
                                    TcpControl::None,
                                    chunk,
                                    our_window,
                                    None,
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
                        // Wait for guest ACK before sending more.
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
                        became_closed = true;
                    }
                }

                // Send FIN if we just transitioned to a state that demands one.
                let needs_fin =
                    matches!(entry.state, TcpNatState::CloseWait | TcpNatState::LastAck);
                if needs_fin && !entry.our_fin_sent {
                    fin_frame = Some(build_tcp_packet_static(
                        key.dst_ip,
                        SLIRP_GUEST_IP,
                        key.dst_port,
                        key.guest_src_port,
                        entry.our_seq,
                        entry.guest_ack,
                        TcpControl::Fin,
                        &[],
                        cached_host_recv_window(entry),
                        None,
                    ));
                    entry.our_seq = entry.our_seq.wrapping_add(1);
                    entry.our_fin_sent = true;
                    trace!("SLIRP TCP: sent FIN to guest, state={:?}", entry.state);
                }

                // Legacy: FIN for the immediate Closed path (error or RST).
                if entry.state == TcpNatState::Closed && became_closed && fin_frame.is_none() {
                    fin_frame = Some(build_tcp_packet_static(
                        key.dst_ip,
                        SLIRP_GUEST_IP,
                        key.dst_port,
                        key.guest_src_port,
                        entry.our_seq,
                        entry.guest_ack,
                        TcpControl::Fin,
                        &[],
                        cached_host_recv_window(entry),
                        None,
                    ));
                }
            } // entry borrow ends here

            if let Some(fin) = fin_frame {
                frames_to_inject.push(fin);
            }
            // Queue for removal so the cleanup loop below can unregister + drop.
            if became_closed {
                to_remove_set.insert(flow_key);
            }
        }

        self.inject_to_guest.append(&mut frames_to_inject);

        for flow_key in to_remove_set {
            if let Some(FlowEntry::Tcp(entry)) = self.flow_table.get(&flow_key) {
                self.token_to_key.remove(&entry.flow_token);
                self.epoll.unregister(entry.host_stream.as_raw_fd()).ok();
                // Connecting entries that timed out never received a SYN-ACK,
                // so we must send RST now to inform the guest.
                if entry.state == TcpNatState::Connecting {
                    if let FlowKey::Tcp(key) = flow_key {
                        let rst = build_tcp_packet_static(
                            key.dst_ip,
                            SLIRP_GUEST_IP,
                            key.dst_port,
                            key.guest_src_port,
                            0,
                            entry.guest_isn.wrapping_add(1),
                            TcpControl::Rst,
                            &[],
                            65535,
                            None,
                        );
                        frames_to_inject.push(rst);
                    }
                }
            }
            self.flow_table.remove(&flow_key);
        }
        self.inject_to_guest.append(&mut frames_to_inject);
    }

    /// Drain replies from each active ICMP echo socket and emit echo-reply
    /// frames to the guest, driven by epoll readiness.
    ///
    /// Only flows whose token appears in `ready` with EPOLLIN set are visited.
    /// Entries idle longer than `ICMP_IDLE_TIMEOUT` are still evicted on any
    /// readiness event for that flow.
    fn relay_icmp_echo(&mut self, ready: &[EpollEvent]) {
        const ICMP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
        let now = Instant::now();

        let mut ready_flow_keys: Vec<FlowKey> = Vec::new();
        for event in ready {
            if !event.readable || event.token & PROTO_TAG_MASK != PROTO_TAG_ICMP {
                continue;
            }
            let Some(flow_key) = self.token_to_key.get(&event.token).copied() else {
                continue;
            };
            ready_flow_keys.push(flow_key);
        }

        // Mirrors the TCP idle-timeout sweep so ICMP sockets do not accumulate
        // indefinitely when the ping target goes silent.
        let mut icmp_to_remove: std::collections::HashSet<FlowKey> =
            std::collections::HashSet::new();
        for (flow_key, entry) in &self.flow_table {
            let FlowKey::IcmpEcho(_) = flow_key else {
                continue;
            };
            let FlowEntry::IcmpEcho(icmp_entry) = entry else {
                continue;
            };
            if now.duration_since(icmp_entry.last_activity) > ICMP_IDLE_TIMEOUT {
                icmp_to_remove.insert(*flow_key);
            }
        }

        for flow_key in &ready_flow_keys {
            // Skip if already in remove set (idle-timeout caught it first).
            // O(1) via HashSet, not O(k) Vec::contains.
            if icmp_to_remove.contains(flow_key) {
                continue;
            }
            let FlowKey::IcmpEcho(key) = *flow_key else {
                continue;
            };
            let frame = {
                let Some(FlowEntry::IcmpEcho(entry)) = self.flow_table.get_mut(flow_key) else {
                    continue;
                };
                let mut buf = [0u8; 1500];
                match entry.sock.recv_from(&mut buf) {
                    Ok((n, _addr)) => {
                        entry.last_activity = now;
                        Self::build_icmp_echo_reply_to_guest(key.dst_ip, entry.guest_id, &buf[..n])
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => continue,
                }
            };
            if let Some(frame_bytes) = frame {
                self.inject_to_guest.push(frame_bytes);
            }
        }

        for flow_key in icmp_to_remove {
            if let Some(FlowEntry::IcmpEcho(e)) = self.flow_table.get(&flow_key) {
                self.token_to_key.remove(&e.flow_token);
                self.epoll.unregister(e.sock.as_raw_fd()).ok();
            }
            self.flow_table.remove(&flow_key);
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
    /// the guest, driven by epoll readiness.
    ///
    /// Only flows whose token appears in `ready` with EPOLLIN set are visited.
    /// Idle-timeout reaping still runs every call: the reap scan is cheap
    /// (skips flows not in `ready`) and ensures stale entries are eventually
    /// evicted even when no new data arrives.
    ///
    /// Reply addressing mirrors the original guest datagram in reverse: the
    /// frame's IP source is the original destination (`key.dst_ip`) and UDP
    /// source port is `key.dst_port`; the destination is the guest IP and
    /// `key.guest_src_port`.
    fn relay_udp_flows(&mut self, ready: &[EpollEvent]) {
        let now = Instant::now();
        // Per-flow connected sockets are closed by Drop when the entry leaves
        // flow_table.
        let mut stale: Vec<FlowKey> = Vec::new();
        for (flow_key, entry) in &self.flow_table {
            let FlowKey::Udp(_) = flow_key else { continue };
            let FlowEntry::Udp(udp_entry) = entry else {
                continue;
            };
            if now.duration_since(udp_entry.last_activity) > UDP_IDLE_TIMEOUT {
                stale.push(*flow_key);
            }
        }
        for flow_key in stale {
            if let Some(FlowEntry::Udp(entry)) = self.flow_table.get(&flow_key) {
                self.token_to_key.remove(&entry.flow_token);
                self.epoll.unregister(entry.sock.as_raw_fd()).ok();
            }
            self.flow_table.remove(&flow_key);
        }

        let mut flow_keys: Vec<FlowKey> = Vec::new();
        for event in ready {
            if !event.readable || event.token & PROTO_TAG_MASK != PROTO_TAG_UDP {
                continue;
            }
            let Some(flow_key) = self.token_to_key.get(&event.token).copied() else {
                continue;
            };
            flow_keys.push(flow_key);
        }
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

    /// Push events from the net-poll thread into this backend's per-tick
    /// event queue. Called from net_poll_thread after each successful
    /// epoll_wait, while holding no other lock.
    ///
    /// drain_to_guest drains this queue with a brief uncontended lock
    /// instead of re-entering EpollDispatch (which the net-poll thread
    /// holds for the full 50 ms of the blocking wait).
    pub fn push_ready_events(&self, events: &[EpollEvent]) {
        // First push from net_poll_thread flips the flag so drain_to_guest
        // skips its non-blocking-poll fallback.  Stays set for the
        // backend's lifetime — net_poll_thread doesn't disappear mid-run.
        self.has_external_poller.store(true, Ordering::Relaxed);
        if events.is_empty() {
            return;
        }
        let mut queue = self.pending_events.lock().unwrap();
        queue.extend_from_slice(events);
    }
}

impl NetworkBackend for SlirpBackend {
    fn process_guest_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        SlirpBackend::process_guest_frame(self, frame).map_err(|e| io::Error::other(e.to_string()))
    }

    fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
        SlirpBackend::drain_to_guest(self, out)
    }

    #[cfg(target_os = "linux")]
    fn epoll_arc(&self) -> Option<std::sync::Arc<crate::network::epoll_dispatch::EpollDispatch>> {
        Some(std::sync::Arc::clone(&self.epoll))
    }

    #[cfg(target_os = "linux")]
    fn push_ready_events(&self, events: &[crate::network::epoll_dispatch::EpollEvent]) {
        SlirpBackend::push_ready_events(self, events)
    }
}

/// Refresh interval for the per-flow `cached_recv_window`. Bounding the
/// freshness of the advertised window to a few milliseconds keeps it well
/// below any realistic RTT, while collapsing what would otherwise be one
/// `getsockopt(TCP_INFO)` per outgoing frame into one per `RECV_WINDOW_TTL`.
const RECV_WINDOW_TTL: Duration = Duration::from_millis(5);

/// Per-flow cache wrapper around [`host_recv_window`].
///
/// Reads the cached value from `entry` and refreshes it via a real
/// `getsockopt(TCP_INFO)` only when it is older than [`RECV_WINDOW_TTL`].
/// At line-rate this drops the syscall from "every outgoing frame" to
/// "every few milliseconds", which profiling identified as the dominant
/// per-frame cost in Phase 6.3.
#[cfg(target_os = "linux")]
fn cached_host_recv_window(entry: &mut TcpNatEntry) -> u16 {
    if entry.cached_recv_window_at.elapsed() >= RECV_WINDOW_TTL {
        entry.cached_recv_window = host_recv_window(entry.host_stream.as_raw_fd());
        entry.cached_recv_window_at = Instant::now();
    }
    entry.cached_recv_window
}

/// Non-Linux stub: same shape as the Linux version, but `host_recv_window`
/// is itself a constant on non-Linux so caching is moot.
#[cfg(not(target_os = "linux"))]
fn cached_host_recv_window(entry: &mut TcpNatEntry) -> u16 {
    if entry.cached_recv_window_at.elapsed() >= RECV_WINDOW_TTL {
        entry.cached_recv_window = host_recv_window(entry.host_stream.as_raw_fd());
        entry.cached_recv_window_at = Instant::now();
    }
    entry.cached_recv_window
}

/// Host kernel's current receive-buffer headroom for a TCP socket, scaled down
/// by `OUR_WINDOW_SCALE`, for advertising as our `window_len` on outgoing frames.
///
/// A fresh TCP socket has `tcpi_rcv_space` pre-filled to ~32 KiB; under load it
/// grows to 4 MiB+ on Linux with auto-tuning enabled. Dividing by 128 (shift 7)
/// keeps the value within `u16::MAX` and matches the scale we advertised in the
/// SYN-ACK.
///
/// Returns `32768` on `getsockopt` failure rather than `0` (which stalls the
/// connection) or `u16::MAX` (which over-commits buffer space).
///
/// Hot-path callers should use [`cached_host_recv_window`] instead — this
/// function is the uncached primitive used by the cache itself.
#[cfg(target_os = "linux")]
fn host_recv_window(fd: std::os::fd::RawFd) -> u16 {
    use std::mem::MaybeUninit;
    let mut info: MaybeUninit<libc::tcp_info> = MaybeUninit::zeroed();
    let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
    // SAFETY: `getsockopt` writes into `info` when it returns 0; the pointer
    // is valid and the length is exact.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            info.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        return 32768;
    }
    // SAFETY: getsockopt returned 0, so `info` is fully initialised.
    let info = unsafe { info.assume_init() };
    let scaled = info.tcpi_rcv_space >> OUR_WINDOW_SCALE;
    scaled.min(u32::from(u16::MAX)) as u16
}

/// Non-Linux stub: always return a conservative fixed window.
/// The SLIRP relay only runs on Linux; this stub keeps cross-platform builds
/// compiling without `#[cfg]` gating at every call site.
#[cfg(not(target_os = "linux"))]
fn host_recv_window(_fd: std::os::fd::RawFd) -> u16 {
    32768
}

/// Build a TCP packet (free function to avoid borrow issues with &self methods).
///
/// `window_len` is the raw 16-bit window field; `window_scale` is included as a
/// TCP option only when `Some(_)` — callers pass `Some(OUR_WINDOW_SCALE)` on
/// SYN-ACK frames and `None` on all other frames (the scale was already
/// negotiated at handshake time and does not re-appear in later headers).
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
    window_len: u16,
    window_scale: Option<u8>,
) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port,
        dst_port,
        seq_number: TcpSeqNumber(seq as i32),
        ack_number: Some(TcpSeqNumber(ack as i32)),
        window_len,
        window_scale,
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

/// Build a synthetic TCP SYN frame from the SLIRP gateway to the guest,
/// used for inbound port-forwarding.
///
/// The frame mirrors what the guest would see from a real TCP client:
/// - src: `SLIRP_GATEWAY_IP:high_port`
/// - dst: `SLIRP_GUEST_IP:guest_port`
/// - control: `TcpControl::Syn`
/// - seq: caller-supplied `our_seq` (the host's chosen ISN for this flow)
/// - ack: 0 (no piggybacked ACK on the initial SYN)
///
/// Caller pushes the returned bytes into `inject_to_guest`. The guest's
/// kernel sees an inbound TCP SYN, routes it to whatever's bound at
/// `guest_port`, and emits a SYN-ACK that `handle_tcp_frame` matches
/// to the seeded `SynSent` flow_table entry (5.5b.1).
#[cfg(any(test, feature = "bench-helpers"))]
pub fn synthesize_inbound_syn(high_port: u16, guest_port: u16, our_seq: u32) -> Vec<u8> {
    build_tcp_packet_static(
        SLIRP_GATEWAY_IP,
        SLIRP_GUEST_IP,
        high_port,
        guest_port,
        our_seq,
        0,
        TcpControl::Syn,
        &[],
        65535,
        None,
    )
}

#[cfg(not(any(test, feature = "bench-helpers")))]
#[allow(dead_code)] // consumed in 5.5b.3
fn synthesize_inbound_syn(high_port: u16, guest_port: u16, our_seq: u32) -> Vec<u8> {
    build_tcp_packet_static(
        SLIRP_GATEWAY_IP,
        SLIRP_GUEST_IP,
        high_port,
        guest_port,
        our_seq,
        0,
        TcpControl::Syn,
        &[],
        65535,
        None,
    )
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

/// Bind one `TcpListener` per TCP port-forward rule, register each with
/// `epoll`, and return a map from host port to `(listener, guest_port)`.
///
/// Rules whose bind or `set_nonblocking` calls fail are skipped with a
/// `WARN` log; the returned map contains only the rules that succeeded.
/// When `nat.port_forwards` contains no TCP rules the map is empty.
pub(crate) fn bind_port_forward_listeners(
    nat: &nat::Rules,
    epoll: &Arc<EpollDispatch>,
) -> HashMap<u16, (TcpListener, u16)> {
    let mut listeners = HashMap::new();
    for port_forward in &nat.port_forwards {
        if port_forward.proto != nat::ForwardProto::Tcp {
            continue;
        }
        let host_port = port_forward.host_port;
        let guest_port = port_forward.guest_port;
        let listener = match TcpListener::bind(("127.0.0.1", host_port)) {
            Ok(l) => l,
            Err(bind_error) => {
                warn!(
                    host_port,
                    error = %bind_error,
                    "SLIRP port-forward: bind failed, rule disabled"
                );
                continue;
            }
        };
        if let Err(nb_error) = listener.set_nonblocking(true) {
            warn!(
                host_port,
                error = %nb_error,
                "SLIRP port-forward: set_nonblocking failed, rule disabled"
            );
            continue;
        }
        let token = flow_token_for_listener(host_port);
        if let Err(reg_error) = epoll.register(listener.as_raw_fd(), token, RegisterMode::Read) {
            warn!(
                host_port,
                error = %reg_error,
                "SLIRP port-forward: epoll register failed, rule disabled"
            );
            continue;
        }
        debug!(
            host_port,
            guest_port, "SLIRP port-forward: listening on 127.0.0.1 (epoll-driven)"
        );
        listeners.insert(host_port, (listener, guest_port));
    }
    listeners
}

impl Default for SlirpBackend {
    fn default() -> Self {
        Self::new().expect("Failed to create default SlirpBackend")
    }
}

impl SlirpBackend {
    /// Re-register every live host FD in `flow_table` with the current epoll
    /// dispatcher and rebuild `token_to_key`.  Called from snapshot restore:
    /// the `epoll_fd` is a kernel handle that does not survive snapshot, so a
    /// fresh dispatcher starts empty even though `flow_table` deserialized
    /// correctly with new FDs.
    ///
    /// Each existing flow keeps its stored `flow_token` so that any
    /// already-queued readiness events (unlikely post-restore, but safe) still
    /// resolve correctly.  The `token_to_key` map is rebuilt from scratch
    /// because it is in-memory-only state; it does not need to be persisted.
    pub fn rebuild_epoll_from_flow_table(&mut self) {
        use std::os::fd::AsRawFd;
        self.token_to_key.clear();

        // Collect Connecting keys for reaping: post-snapshot the underlying
        // socket fd is dead (the kernel's connect state lives in vhost-vsock
        // and does not survive snapshot).  Re-registering a dead fd for
        // EPOLLOUT would stall the flow until CONNECT_TIMEOUT fires — reaping
        // immediately is correct and matches the "no useful state to persist"
        // principle stated in the Phase 6.2 plan.
        let connecting_keys: Vec<FlowKey> = self
            .flow_table
            .iter()
            .filter_map(|(k, v)| {
                if let FlowEntry::Tcp(e) = v {
                    if e.state == TcpNatState::Connecting {
                        return Some(*k);
                    }
                }
                None
            })
            .collect();
        for key in connecting_keys {
            self.flow_table.remove(&key);
        }

        for (flow_key, entry) in &self.flow_table {
            match (flow_key, entry) {
                (FlowKey::Tcp(_), FlowEntry::Tcp(e)) => {
                    self.token_to_key.insert(e.flow_token, *flow_key);
                    let _ = self.epoll.register(
                        e.host_stream.as_raw_fd(),
                        e.flow_token,
                        RegisterMode::Read,
                    );
                }
                (FlowKey::Udp(_), FlowEntry::Udp(e)) => {
                    self.token_to_key.insert(e.flow_token, *flow_key);
                    let _ =
                        self.epoll
                            .register(e.sock.as_raw_fd(), e.flow_token, RegisterMode::Read);
                }
                (FlowKey::IcmpEcho(_), FlowEntry::IcmpEcho(e)) => {
                    self.token_to_key.insert(e.flow_token, *flow_key);
                    let _ =
                        self.epoll
                            .register(e.sock.as_raw_fd(), e.flow_token, RegisterMode::Read);
                }
                _ => {}
            }
        }
    }
}

/// Test-only helpers — not compiled into production builds.
///
/// These are `#[cfg(test)]`/`#[cfg(feature = "bench-helpers")]` methods on
/// `SlirpBackend` that allow unit tests and divan benches to insert synthetic
/// flow entries without widening the visibility of private types.
/// The full behavioral contract for the SynSent → Established transition is
/// pinned in the E2E test `tcp_inbound_syn_ack_completes_handshake` below and
/// will be further exercised end-to-end in task 5.5b.5
/// (`tcp_port_forward_inbound` in `tests/network_baseline.rs`).
#[cfg(any(test, feature = "bench-helpers"))]
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
    pub fn insert_synthetic_synsent_entry(
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
        let host_fd = host_stream.as_raw_fd();
        let token = next_flow_token(PROTO_TAG_TCP);
        let flow_key = FlowKey::Tcp(key);
        let cached_recv_window = host_recv_window(host_fd);
        let entry = TcpNatEntry {
            host_stream,
            state: TcpNatState::SynSent,
            our_seq: our_isn,
            guest_ack: 0,
            last_activity: Instant::now(),
            bytes_in_flight: 0,
            flow_token: token,
            last_state_change: Instant::now(),
            our_fin_sent: false,
            guest_isn: 0,
            guest_window: 65535,
            guest_window_scale: 0,
            cached_recv_window,
            cached_recv_window_at: Instant::now(),
        };
        self.flow_table.insert(flow_key, FlowEntry::Tcp(entry));
        self.token_to_key.insert(token, flow_key);
        // Skip epoll registration in test/bench contexts: the synthetic
        // stream is already non-blocking but test harnesses check specific
        // state transitions, not readiness events.
        #[cfg(not(any(test, feature = "bench-helpers")))]
        {
            if let Err(e) = self.epoll.register(host_fd, token, RegisterMode::Read) {
                warn!(
                    guest_port,
                    high_port,
                    fd = host_fd,
                    error = %e,
                    "SLIRP: epoll register for synthetic SynSent failed"
                );
            }
            self.epoll_waker.wake();
        }
        #[cfg(any(test, feature = "bench-helpers"))]
        let _ = host_fd;
    }

    /// Return the `TcpNatState` for the flow identified by `(guest_port, GATEWAY_IP, high_port)`,
    /// or `None` if no such entry exists in the flow table.
    #[allow(dead_code)]
    pub fn tcp_flow_state(&self, guest_port: u16, high_port: u16) -> Option<TcpNatState> {
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
    #[allow(dead_code)]
    pub(crate) fn injected_plain_ack_count(&self) -> usize {
        let mut count = 0;
        for frame in &self.inject_to_guest {
            if frame.len() < 54 {
                continue;
            }
            let tcp_offset = 14 + 20;
            let flags_byte = frame[tcp_offset + 13];
            let ack = flags_byte & 0x10 != 0;
            let syn = flags_byte & 0x02 != 0;
            if ack && !syn {
                count += 1;
            }
        }
        count
    }

    /// Inject an [`InboundAccept`] directly into the accept channel, bypassing
    /// the listener thread. Used by unit tests to drive
    /// `process_pending_inbound_accepts` without a real listener.
    #[allow(dead_code)]
    pub(crate) fn push_inbound_accept(&self, accepted: InboundAccept) {
        self.accept_sender
            .send(accepted)
            .expect("accept channel must be open");
    }

    /// Returns the number of user-registered FDs in the epoll set
    /// (excludes the self-pipe).
    pub fn registered_fd_count(&self) -> usize {
        self.epoll.registered_fd_count()
    }

    /// Replace the epoll dispatcher with a fresh empty one, discarding all
    /// existing registrations.  Simulates the post-snapshot state where the
    /// kernel-side `epoll_fd` handle does not survive and a new one is
    /// created.  Used by `epoll_set_rebuilt_from_flow_table_smoke` to set up
    /// the precondition that `rebuild_epoll_from_flow_table` must fix.
    pub fn reset_epoll_for_snapshot_test(&mut self) {
        let new_epoll_inner = EpollDispatch::new().expect("EpollDispatch::new");
        let new_waker = new_epoll_inner.waker();
        self.epoll = Arc::new(new_epoll_inner);
        self.epoll_waker = new_waker;
    }

    /// Insert a synthetic `LastAck` entry into the flow table.
    ///
    /// Used by `tcp_last_ack_timeout_reaps_stale_entry` to pre-seed a flow
    /// in the LastAck state without going through a full half-close exchange.
    ///
    /// The entry's `last_state_change` is set to `Instant::now()` and can be
    /// back-dated with [`Self::set_synthetic_last_state_change`] to simulate
    /// an expired timeout.
    pub fn insert_synthetic_lastack_entry(
        &mut self,
        guest_port: u16,
        high_port: u16,
        host_stream: TcpStream,
    ) {
        let key = NatKey {
            guest_src_port: guest_port,
            dst_ip: SLIRP_GATEWAY_IP,
            dst_port: high_port,
        };
        let token = next_flow_token(PROTO_TAG_TCP);
        let cached_recv_window = host_recv_window(host_stream.as_raw_fd());
        let entry = TcpNatEntry {
            host_stream,
            state: TcpNatState::LastAck,
            our_seq: 1,
            guest_ack: 1,
            last_activity: Instant::now(),
            bytes_in_flight: 0,
            flow_token: token,
            last_state_change: Instant::now(),
            our_fin_sent: true,
            guest_isn: 0,
            guest_window: 65535,
            guest_window_scale: 0,
            cached_recv_window,
            cached_recv_window_at: Instant::now(),
        };
        self.flow_table
            .insert(FlowKey::Tcp(key), FlowEntry::Tcp(entry));
    }

    /// Back-date the `last_state_change` of the flow identified by
    /// `(guest_port, GATEWAY_IP, high_port)` by `age`.  Used by
    /// `tcp_last_ack_timeout_reaps_stale_entry` to simulate a LastAck
    /// entry that has been sitting past `LAST_ACK_TIMEOUT` without
    /// receiving the guest's final ACK.
    pub fn set_synthetic_last_state_change(
        &mut self,
        guest_port: u16,
        high_port: u16,
        age: Duration,
    ) {
        let key = FlowKey::Tcp(NatKey {
            guest_src_port: guest_port,
            dst_ip: SLIRP_GATEWAY_IP,
            dst_port: high_port,
        });
        if let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&key) {
            // Instant::now() - age: subtract by creating an instant that
            // appears to have occurred `age` ago.
            entry.last_state_change = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        }
    }

    /// Insert a synthetic `Connecting` entry into the flow table without
    /// issuing an actual `connect()` syscall.
    ///
    /// Used by `process_syn_during_pending_connects` to pre-populate the flow
    /// table with `n_pending` Connecting entries so the bench can measure
    /// `process_guest_frame`'s cost as a function of pending-connect backlog.
    ///
    /// The synthetic stream is a loopback pair so it has a valid fd; the
    /// entry's state is forced to Connecting, and the fd is registered for
    /// EPOLLOUT (matching what a real non-blocking connect would do).
    pub fn insert_synthetic_connecting_entry(
        &mut self,
        guest_src_port: u16,
        dst_ip: Ipv4Address,
        dst_port: u16,
    ) {
        use std::net::TcpListener;
        // Create a real but idle stream pair so host_stream holds a valid fd.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).expect("connect");
        stream.set_nonblocking(true).ok();
        let key = NatKey {
            guest_src_port,
            dst_ip,
            dst_port,
        };
        let host_fd = stream.as_raw_fd();
        let token = next_flow_token(PROTO_TAG_TCP);
        let flow_key = FlowKey::Tcp(key);
        let cached_recv_window = host_recv_window(host_fd);
        let entry = TcpNatEntry {
            host_stream: stream,
            state: TcpNatState::Connecting,
            our_seq: rand_seq(),
            guest_ack: 1,
            last_activity: Instant::now(),
            bytes_in_flight: 0,
            flow_token: token,
            last_state_change: Instant::now(),
            our_fin_sent: false,
            guest_isn: 1000,
            guest_window: 65535,
            guest_window_scale: 0,
            cached_recv_window,
            cached_recv_window_at: Instant::now(),
        };
        self.flow_table.insert(flow_key, FlowEntry::Tcp(entry));
        self.token_to_key.insert(token, flow_key);
        // Register for EPOLLOUT so the synthetic entry looks like a real
        // in-progress connect from the epoll dispatcher's perspective.
        let _ = self.epoll.register(host_fd, token, RegisterMode::Write);
        // listener is dropped here but stream keeps the connection alive.
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

    /// Verify that `process_pending_inbound_accepts` drains one `InboundAccept`
    /// from the channel, inserts a `SynSent` flow-table entry, and queues a
    /// synthesized SYN frame for injection to the guest.
    ///
    /// This pins the contract for task 5.5b.3.  The test is white-box: it uses
    /// `push_inbound_accept` (a `#[cfg(test)]` helper that injects into the
    /// internal channel) so we don't need a real listener thread.
    #[test]
    fn process_pending_inbound_accepts_seeds_synsent_and_queues_syn() {
        use std::net::TcpListener;

        let guest_port: u16 = 9000;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let local_addr = listener.local_addr().unwrap();
        let host_stream = TcpStream::connect(local_addr).expect("connect loopback");
        let high_port = host_stream.local_addr().unwrap().port();
        host_stream.set_nonblocking(true).ok();

        let mut backend = SlirpBackend::new().expect("SlirpBackend::new");

        // Inject an InboundAccept without a real listener thread.
        backend.push_inbound_accept(InboundAccept {
            host_stream,
            high_port,
            guest_port,
        });

        // Before processing, no flow entry should exist.
        assert_eq!(
            backend.tcp_flow_state(guest_port, high_port),
            None,
            "no flow entry before processing"
        );

        // Drive process_pending_inbound_accepts.
        backend.process_pending_inbound_accepts();

        // After processing, a SynSent entry must exist.
        assert_eq!(
            backend.tcp_flow_state(guest_port, high_port),
            Some(TcpNatState::SynSent),
            "SynSent entry must be present after processing"
        );

        // Exactly one SYN frame must have been queued for injection.
        // Note: build_tcp_packet_static sets ack_number=Some(0) which also
        // sets the ACK flag bit; we detect the SYN by checking just the SYN bit.
        let syn_count = backend
            .inject_to_guest
            .iter()
            .filter(|frame| {
                if frame.len() < 54 {
                    return false;
                }
                let tcp_offset = 14 + 20;
                let flags_byte = frame[tcp_offset + 13];
                flags_byte & 0x02 != 0
            })
            .count();
        assert_eq!(syn_count, 1, "exactly one SYN must be queued for the guest");
    }

    /// Verify that `with_security` binds exactly one epoll-driven listener when
    /// given one TCP port-forward rule, and zero listeners when given none.
    #[test]
    fn with_security_binds_listener_per_tcp_port_forward() {
        // Empty port-forwards: no listeners.
        let empty = SlirpBackend::with_security(64, 50, &["169.254.0.0/16".to_string()], &[])
            .expect("SlirpBackend::with_security (empty)");
        assert_eq!(
            empty.port_forward_listeners.len(),
            0,
            "zero listeners for empty port_forwards"
        );

        // One TCP port-forward: exactly one listener.
        let one =
            SlirpBackend::with_security(64, 50, &["169.254.0.0/16".to_string()], &[(18080, 80)])
                .expect("SlirpBackend::with_security (one forward)");
        assert_eq!(
            one.port_forward_listeners.len(),
            1,
            "one listener for one TCP port-forward rule"
        );
    }
}
