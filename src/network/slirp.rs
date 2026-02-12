//! SLIRP-style user-mode networking using smoltcp
//!
//! This module provides NAT-based network connectivity for guest VMs without
//! requiring root privileges, TAP devices, or iptables configuration.
//!
//! Network layout (SLIRP standard):
//! - Guest IP: 10.0.2.15/24
//! - Gateway:  10.0.2.2
//! - DNS:      10.0.2.3
//!
//! The SlirpStack handles:
//! - Ethernet frame parsing/building
//! - TCP/IP stack via smoltcp
//! - NAT for outbound connections
//! - DNS forwarding to host resolver

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer, State as TcpState};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr, Ipv4Address,
};

use tracing::{debug, trace, warn};

use crate::Result;

/// Get current smoltcp timestamp from system time
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

/// Guest MAC address (locally administered)
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// Gateway MAC address
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x01];

/// Maximum transmission unit
const MTU: usize = 1500;
/// Maximum packet queue size
const MAX_QUEUE_SIZE: usize = 64;

/// A NAT connection tracking entry
#[derive(Debug)]
struct NatEntry {
    /// Host-side TCP connection
    host_stream: Option<TcpStream>,
    /// Host-side UDP socket
    host_udp: Option<UdpSocket>,
    /// Remote address this connection is going to
    remote_addr: SocketAddr,
    /// smoltcp socket handle
    socket_handle: SocketHandle,
    /// Last activity time
    last_activity: Instant,
    /// Pending data to send to host
    pending_to_host: Vec<u8>,
    /// Pending data to send to guest
    pending_to_guest: Vec<u8>,
}

/// Packet queue for virtual ethernet device
struct PacketQueue {
    /// Packets from guest (to be processed by stack)
    rx_queue: Vec<Vec<u8>>,
    /// Packets to guest (from stack)
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

/// Virtual ethernet device for smoltcp
struct VirtualDevice {
    queue: Arc<Mutex<PacketQueue>>,
}

impl VirtualDevice {
    fn new(queue: Arc<Mutex<PacketQueue>>) -> Self {
        Self { queue }
    }
}

impl Device for VirtualDevice {
    type RxToken<'a> = VirtualRxToken;
    type TxToken<'a> = VirtualTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(1);
        caps
    }

    fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut queue = self.queue.lock().unwrap();
        if queue.rx_queue.is_empty() {
            return None;
        }
        let packet = queue.rx_queue.remove(0);
        Some((
            VirtualRxToken {
                buffer: packet,
            },
            VirtualTxToken {
                queue: self.queue.clone(),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(VirtualTxToken {
            queue: self.queue.clone(),
        })
    }
}

struct VirtualRxToken {
    buffer: Vec<u8>,
}

impl RxToken for VirtualRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.buffer)
    }
}

struct VirtualTxToken {
    queue: Arc<Mutex<PacketQueue>>,
}

impl TxToken for VirtualTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        let mut queue = self.queue.lock().unwrap();
        if queue.tx_queue.len() < MAX_QUEUE_SIZE {
            queue.tx_queue.push(buffer);
        }
        result
    }
}

/// SLIRP stack providing user-mode NAT networking
pub struct SlirpStack {
    /// Packet queue shared with virtual device
    queue: Arc<Mutex<PacketQueue>>,
    /// smoltcp interface
    iface: Interface,
    /// Socket set
    sockets: SocketSet<'static>,
    /// NAT connection table
    nat_table: HashMap<u16, NatEntry>,
    /// Next local port for NAT
    next_local_port: u16,
    /// Virtual device (kept alive for interface)
    _device: VirtualDevice,
}

impl SlirpStack {
    /// Create a new SLIRP stack
    pub fn new() -> Result<Self> {
        debug!("Creating SLIRP stack");

        let queue = Arc::new(Mutex::new(PacketQueue::new()));
        let device = VirtualDevice::new(queue.clone());

        // Create smoltcp interface
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        let mut iface = Interface::new(config, &mut VirtualDevice::new(queue.clone()), smol_instant_now());

        // Configure interface with gateway IP
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(10, 0, 2, 2), SLIRP_NETMASK))
                .unwrap();
        });

        // Add default route (we are the gateway)
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
            nat_table: HashMap::new(),
            next_local_port: 10000,
            _device: device,
        })
    }

    /// Process an ethernet frame from the guest
    pub fn process_guest_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() < 14 {
            return Ok(()); // Too short for ethernet header
        }

        // Parse ethernet frame
        let eth_frame = match EthernetFrame::new_checked(frame) {
            Ok(f) => f,
            Err(e) => {
                trace!("Invalid ethernet frame: {}", e);
                return Ok(());
            }
        };

        trace!(
            "Guest frame: {} -> {}, type={:?}",
            eth_frame.src_addr(),
            eth_frame.dst_addr(),
            eth_frame.ethertype()
        );

        // Queue the frame for smoltcp processing
        {
            let mut queue = self.queue.lock().unwrap();
            if queue.rx_queue.len() < MAX_QUEUE_SIZE {
                queue.rx_queue.push(frame.to_vec());
            }
        }

        Ok(())
    }

    /// Poll the stack and process pending work
    /// Returns frames to send to the guest
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let timestamp = smol_instant_now();

        // Create a temporary device for polling
        let mut device = VirtualDevice::new(self.queue.clone());

        // Poll the interface
        let _ = self.iface.poll(timestamp, &mut device, &mut self.sockets);

        // Process NAT connections
        self.process_nat_connections();

        // Collect frames to send to guest
        let mut frames = Vec::new();
        {
            let mut queue = self.queue.lock().unwrap();
            frames.append(&mut queue.tx_queue);
        }

        frames
    }

    /// Handle a new TCP connection from guest
    pub fn handle_tcp_connect(
        &mut self,
        _src_port: u16,
        dst_addr: Ipv4Address,
        dst_port: u16,
    ) -> Result<()> {
        debug!(
            "NAT: TCP connect to {}:{}",
            dst_addr, dst_port
        );

        // Check if this is a DNS request (port 53)
        if dst_port == 53 {
            // Handle DNS specially
            return Ok(());
        }

        // Try to connect to the remote host
        let remote_addr = format!("{}:{}", dst_addr, dst_port);
        let addrs: Vec<SocketAddr> = match remote_addr.to_socket_addrs() {
            Ok(a) => a.collect(),
            Err(e) => {
                warn!("Failed to resolve {}: {}", remote_addr, e);
                return Ok(());
            }
        };

        if addrs.is_empty() {
            warn!("No addresses resolved for {}", remote_addr);
            return Ok(());
        }

        // Connect with timeout
        let stream = match TcpStream::connect_timeout(&addrs[0], Duration::from_secs(10)) {
            Ok(s) => {
                s.set_nonblocking(true).ok();
                s
            }
            Err(e) => {
                warn!("Failed to connect to {}: {}", remote_addr, e);
                return Ok(());
            }
        };

        debug!("NAT: Connected to {}", addrs[0]);

        // Create NAT entry
        let local_port = self.allocate_local_port();

        // Create smoltcp TCP socket for the guest side
        let rx_buffer = SocketBuffer::new(vec![0; 65536]);
        let tx_buffer = SocketBuffer::new(vec![0; 65536]);
        let tcp_socket = TcpSocket::new(rx_buffer, tx_buffer);
        let handle = self.sockets.add(tcp_socket);

        let entry = NatEntry {
            host_stream: Some(stream),
            host_udp: None,
            remote_addr: addrs[0],
            socket_handle: handle,
            last_activity: Instant::now(),
            pending_to_host: Vec::new(),
            pending_to_guest: Vec::new(),
        };

        self.nat_table.insert(local_port, entry);

        Ok(())
    }

    /// Process active NAT connections
    fn process_nat_connections(&mut self) {
        let mut to_remove = Vec::new();

        for (&local_port, entry) in self.nat_table.iter_mut() {
            // Check for timeout (5 minutes idle)
            if entry.last_activity.elapsed() > Duration::from_secs(300) {
                to_remove.push(local_port);
                continue;
            }

            // Process TCP connections
            if let Some(ref mut stream) = entry.host_stream {
                // Read from host, queue to guest
                let mut buf = [0u8; 4096];
                match stream.read(&mut buf) {
                    Ok(0) => {
                        // Connection closed
                        to_remove.push(local_port);
                    }
                    Ok(n) => {
                        entry.pending_to_guest.extend_from_slice(&buf[..n]);
                        entry.last_activity = Instant::now();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No data available
                    }
                    Err(_) => {
                        to_remove.push(local_port);
                    }
                }

                // Write pending data to host
                if !entry.pending_to_host.is_empty() {
                    match stream.write(&entry.pending_to_host) {
                        Ok(n) => {
                            entry.pending_to_host.drain(..n);
                            entry.last_activity = Instant::now();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            to_remove.push(local_port);
                        }
                    }
                }
            }

            // Transfer data between smoltcp socket and pending buffers
            let socket = self.sockets.get_mut::<TcpSocket>(entry.socket_handle);

            // Send pending data to guest via smoltcp
            if socket.can_send() && !entry.pending_to_guest.is_empty() {
                let n = socket.send_slice(&entry.pending_to_guest).unwrap_or(0);
                if n > 0 {
                    entry.pending_to_guest.drain(..n);
                }
            }

            // Receive data from guest via smoltcp
            if socket.can_recv() {
                let mut buf = vec![0u8; socket.recv_queue()];
                if let Ok(n) = socket.recv_slice(&mut buf) {
                    entry.pending_to_host.extend_from_slice(&buf[..n]);
                }
            }

            // Check if socket is closed
            if socket.state() == TcpState::Closed {
                to_remove.push(local_port);
            }
        }

        // Remove closed connections
        for port in to_remove {
            if let Some(entry) = self.nat_table.remove(&port) {
                self.sockets.remove(entry.socket_handle);
                debug!("NAT: Removed connection on port {}", port);
            }
        }
    }

    /// Allocate a local port for NAT
    fn allocate_local_port(&mut self) -> u16 {
        let port = self.next_local_port;
        self.next_local_port = if self.next_local_port >= 60000 {
            10000
        } else {
            self.next_local_port + 1
        };
        port
    }

    /// Handle DNS query and return response
    pub fn handle_dns_query(&self, query: &[u8]) -> Option<Vec<u8>> {
        // Forward DNS query to system resolver
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return None,
        };

        socket.set_read_timeout(Some(Duration::from_secs(5))).ok()?;

        // Try common DNS servers
        let dns_servers = ["8.8.8.8:53", "1.1.1.1:53"];

        for server in dns_servers {
            if socket.send_to(query, server).is_ok() {
                let mut response = vec![0u8; 512];
                if let Ok((n, _)) = socket.recv_from(&mut response) {
                    response.truncate(n);
                    return Some(response);
                }
            }
        }

        None
    }

    /// Get packets waiting to be sent to the guest
    pub fn get_guest_packets(&mut self) -> Vec<Vec<u8>> {
        let mut queue = self.queue.lock().unwrap();
        std::mem::take(&mut queue.tx_queue)
    }

    /// Queue a packet from the guest for processing
    pub fn queue_guest_packet(&mut self, packet: Vec<u8>) {
        let mut queue = self.queue.lock().unwrap();
        if queue.rx_queue.len() < MAX_QUEUE_SIZE {
            queue.rx_queue.push(packet);
        }
    }
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
        // Verify locally administered bit is set
        assert!(GUEST_MAC[0] & 0x02 != 0);
        assert!(GATEWAY_MAC[0] & 0x02 != 0);
    }

    #[test]
    fn test_slirp_stack_creation() {
        let stack = SlirpStack::new();
        assert!(stack.is_ok());
    }
}
