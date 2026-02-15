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
//! - ARP: custom handler responds as gateway for all 10.0.2.x IPs
//! - TCP: NAT proxy (raw packet parsing + host TCP sockets)
//! - UDP port 53 (DNS): forwarded to host resolver
//! - Other: silently dropped
//!
//! The smoltcp library is used for its Ethernet/IPv4/TCP/UDP wire types
//! and checksum computation, but all packet handling is done manually.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, HardwareAddress, IpAddress,
    IpCidr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    TcpSeqNumber, UdpPacket,
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

// ──────────────────────────────────────────────────────────────────────
//  TCP NAT connection tracking
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum TcpNatState {
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    Closed,
}

/// Key for NAT table: (guest_src_port, dst_ip, dst_port)
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
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
    /// Data received from host, pending delivery to guest
    to_guest: Vec<u8>,
    last_activity: Instant,
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
//  SLIRP Stack
// ──────────────────────────────────────────────────────────────────────

pub struct SlirpStack {
    queue: Arc<Mutex<PacketQueue>>,
    iface: Interface,
    sockets: SocketSet<'static>,
    _device: VirtualDevice,
    /// TCP NAT table
    tcp_nat: HashMap<NatKey, TcpNatEntry>,
    /// Frames to inject into guest (built by our NAT, not by smoltcp)
    inject_to_guest: Vec<Vec<u8>>,
}

impl SlirpStack {
    pub fn new() -> Result<Self> {
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
        debug!(
            "SLIRP stack created - Gateway: {}, DNS: {}",
            SLIRP_GATEWAY_IP, SLIRP_DNS_IP
        );

        Ok(Self {
            queue,
            iface,
            sockets,
            _device: device,
            tcp_nat: HashMap::new(),
            inject_to_guest: Vec::new(),
        })
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

    /// Poll the stack. Returns ethernet frames to send to the guest.
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        // Check rx_queue size before polling
        let rx_count = {
            let q = self.queue.lock().unwrap();
            q.rx_queue.len()
        };

        // 1. Let smoltcp handle ARP
        let ts = smol_instant_now();
        let mut dev = VirtualDevice::new(self.queue.clone());
        let changed = self.iface.poll(ts, &mut dev, &mut self.sockets);

        // 2. Process TCP NAT data relay
        self.relay_tcp_nat_data();

        // 3. Collect frames: smoltcp ARP responses + our NAT-built frames
        let mut frames = Vec::new();
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
            frames.append(&mut q.tx_queue);
        }
        frames.append(&mut self.inject_to_guest);

        frames
    }

    /// Forward a DNS query to host resolvers and return the response.
    fn forward_dns_query(&self, query: &[u8]) -> Option<Vec<u8>> {
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        for server in &["8.8.8.8:53", "1.1.1.1:53"] {
            if sock.send_to(query, server).is_ok() {
                let mut resp = vec![0u8; 4096];
                if let Ok((n, _)) = sock.recv_from(&mut resp) {
                    resp.truncate(n);
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

        // DNS (UDP to 10.0.2.3:53) – handle specially
        if dst_ip == SLIRP_DNS_IP && protocol == IpProtocol::Udp {
            return self.handle_dns_frame(&ipv4);
        }

        // TCP to any external IP (not gateway) – NAT proxy
        if protocol == IpProtocol::Tcp {
            // Also handle TCP to gateway IP (for potential proxy use)
            if dst_ip != SLIRP_GUEST_IP {
                return self.handle_tcp_frame(&ipv4);
            }
        }

        // Everything else (ICMP, etc.) – drop silently
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

        // Forward to host DNS
        if let Some(response) = self.forward_dns_query(query) {
            // Build response: Ethernet(IP(UDP(dns_response)))
            let frame = self.build_udp_response(
                SLIRP_DNS_IP,   // src = DNS server
                SLIRP_GUEST_IP, // dst = guest
                53,             // src port
                src_port,       // dst port
                &response,
            );
            self.inject_to_guest.push(frame);
            debug!("SLIRP DNS: sent {} byte response", response.len());
        } else {
            warn!("SLIRP DNS: failed to resolve query");
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

            // Remove any stale entry with the same key
            self.tcp_nat.remove(&key);

            // Create host TCP connection.
            // Map the SLIRP gateway IP (10.0.2.2) to localhost so the guest
            // can reach host services (e.g. Ollama at localhost:11434).
            let host_ip = if dst_ip == SLIRP_GATEWAY_IP {
                std::net::Ipv4Addr::new(127, 0, 0, 1)
            } else {
                std::net::Ipv4Addr::new(dst_ip.0[0], dst_ip.0[1], dst_ip.0[2], dst_ip.0[3])
            };
            let addr = SocketAddr::new(std::net::IpAddr::V4(host_ip), dst_port);

            match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
                Ok(stream) => {
                    stream.set_nonblocking(true).ok();
                    let our_seq: u32 = rand_seq();
                    let entry = TcpNatEntry {
                        host_stream: stream,
                        state: TcpNatState::SynReceived,
                        our_seq,
                        guest_ack: seq + 1,
                        to_guest: Vec::new(),
                        last_activity: Instant::now(),
                    };
                    self.tcp_nat.insert(key.clone(), entry);

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
        let entry = match self.tcp_nat.get_mut(&key) {
            Some(e) => e,
            None => {
                trace!(
                    "SLIRP TCP: no NAT entry for {}:{} -> {}:{}",
                    src_ip,
                    src_port,
                    dst_ip,
                    dst_port
                );
                return Ok(());
            }
        };

        entry.last_activity = Instant::now();

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

        // Data payload
        let payload = tcp.payload();
        if !payload.is_empty() && entry.state == TcpNatState::Established {
            // Forward to host
            match entry.host_stream.write_all(payload) {
                Ok(()) => {
                    entry.guest_ack = seq.wrapping_add(payload.len() as u32);
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
                }
                Err(e) => {
                    warn!("SLIRP TCP: write to host failed: {}", e);
                    entry.state = TcpNatState::Closed;
                }
            }
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
        let mut to_remove = Vec::new();
        // Collect frames to inject (built separately to avoid borrow issues)
        let mut frames_to_inject: Vec<Vec<u8>> = Vec::new();

        // Phase 1: Read from host, update state, build frames
        for (key, entry) in self.tcp_nat.iter_mut() {
            if entry.state == TcpNatState::Closed {
                to_remove.push(key.clone());
                continue;
            }
            if entry.last_activity.elapsed() > Duration::from_secs(300) {
                to_remove.push(key.clone());
                continue;
            }
            if entry.state != TcpNatState::Established {
                continue;
            }

            // Read from host
            let mut buf = [0u8; 16384];
            match entry.host_stream.read(&mut buf) {
                Ok(0) => {
                    debug!("SLIRP TCP: host closed for {}:{}", key.dst_ip, key.dst_port);
                    entry.state = TcpNatState::Closed;
                }
                Ok(n) => {
                    entry.to_guest.extend_from_slice(&buf[..n]);
                    entry.last_activity = Instant::now();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    trace!("SLIRP TCP: host read error: {}", e);
                    entry.state = TcpNatState::Closed;
                }
            }

            // Build data frames for guest
            while !entry.to_guest.is_empty() && entry.state == TcpNatState::Established {
                let chunk_size = entry.to_guest.len().min(MTU - 54);
                let chunk: Vec<u8> = entry.to_guest.drain(..chunk_size).collect();
                let frame = build_tcp_packet_static(
                    key.dst_ip,
                    SLIRP_GUEST_IP,
                    key.dst_port,
                    key.guest_src_port,
                    entry.our_seq,
                    entry.guest_ack,
                    TcpControl::None,
                    &chunk,
                );
                entry.our_seq = entry.our_seq.wrapping_add(chunk.len() as u32);
                frames_to_inject.push(frame);
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

        for key in to_remove {
            self.tcp_nat.remove(&key);
        }
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

/// Build a TCP packet (free function to avoid borrow issues with &self methods)
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

impl Default for SlirpStack {
    fn default() -> Self {
        Self::new().expect("Failed to create default SlirpStack")
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
        let stack = SlirpStack::new();
        assert!(stack.is_ok());
    }

    #[test]
    fn test_ipv4_checksum() {
        // All zeros (except version/ihl) should produce a valid checksum
        let mut header = [0u8; 20];
        header[0] = 0x45;
        let cksum = ipv4_checksum(&header);
        assert_ne!(cksum, 0);
    }
}
