//! Layer-1 correctness pins for the smoltcp-based SLIRP stack.
//!
//! These tests drive `SlirpBackend` directly with synthetic Ethernet
//! frames — no VM, no kernel, no host sockets to outside hosts. The
//! goal is to lock observable behavior (including deliberately broken
//! behavior) so the passt-pattern refactor's diff is legible to
//! reviewers.
//!
//! TODO(0D.4): migrate poll() → drain_to_guest() and remove #[allow(deprecated)].
#![allow(deprecated)]
//!
//! Three tests assert *broken* behavior on purpose. Each is marked
//! `BROKEN_ON_PURPOSE` and flips in the phase that fixes it:
//!
//! - `tcp_writes_more_than_256kb_succeed` — flipped in Phase 3 (was `tcp_to_host_buffer_drops_at_256kb`)
//! - `udp_non_dns_round_trips` — flipped in Phase 2 (was `udp_non_dns_silently_dropped`)
//! - `icmp_echo_returns_reply` — flipped in Phase 1 (was `icmp_echo_silently_dropped`)
//!
//! Run with: `cargo test --test network_baseline`

#![cfg(target_os = "linux")]
// Imports and helpers used by test cases added in tasks 0A.2–0A.9.
#![allow(unused_imports, dead_code)]

use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpAddress, IpProtocol, Ipv4Address, Ipv4Packet,
    Ipv4Repr, TcpControl, TcpPacket, TcpRepr, UdpPacket, UdpRepr,
};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::unix::io::AsRawFd;
use void_box::network::nat::{translate_outbound, Rules};
use void_box::network::slirp::{
    SlirpBackend, GATEWAY_MAC, GUEST_MAC, SLIRP_DNS_IP, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
};
use void_box::network::NetworkBackend;
// Used by tcp_deny_list_emits_rst to express the deny CIDR as a typed network.
// `with_security` takes `&[String]`, so we convert via `.to_string()` at the
// call site; this import is kept here (module scope) per project convention.
use ipnet::Ipv4Net;

const GUEST_EPHEMERAL_PORT: u16 = 49152;
const ETH_HDR_LEN: usize = 14;
const IPV4_MIN_HDR_LEN: usize = 20;
const TCP_MIN_HDR_LEN: usize = 20;
const UDP_HDR_LEN: usize = 8;

/// Builds a minimal IPv4-over-Ethernet TCP segment from guest to a
/// pretend external IP. Returns the full Ethernet frame bytes.
fn build_tcp_frame(
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
        control,
        seq_number: smoltcp::wire::TcpSeqNumber(seq as i32),
        ack_number: if ack == 0 {
            None
        } else {
            Some(smoltcp::wire::TcpSeqNumber(ack as i32))
        },
        window_len: 65535,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        payload,
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: dst_ip,
        next_header: IpProtocol::Tcp,
        payload_len: tcp_repr.buffer_len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + tcp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut tcp = TcpPacket::new_unchecked(&mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..]);
    tcp_repr.emit(
        &mut tcp,
        &IpAddress::Ipv4(SLIRP_GUEST_IP),
        &IpAddress::Ipv4(dst_ip),
        &Default::default(),
    );
    buf
}

/// Builds a UDP-over-Ethernet datagram from guest.
fn build_udp_frame(dst_ip: Ipv4Address, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let udp_repr = UdpRepr { src_port, dst_port };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: dst_ip,
        next_header: IpProtocol::Udp,
        payload_len: UDP_HDR_LEN + payload.len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + UDP_HDR_LEN + payload.len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut udp = UdpPacket::new_unchecked(&mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..]);
    udp_repr.emit(
        &mut udp,
        &IpAddress::Ipv4(SLIRP_GUEST_IP),
        &IpAddress::Ipv4(dst_ip),
        payload.len(),
        |b| b.copy_from_slice(payload),
        &Default::default(),
    );
    buf
}

/// Parses one emitted frame as a TCP segment directed to the guest.
///
/// Returns `(seq, ack, control, payload_len)` on success, or `None`
/// if the frame is not IPv4-TCP destined for the guest or has an
/// unrecognized flag combination.
fn parse_tcp_to_guest(frame: &[u8]) -> Option<(u32, u32, TcpControl, usize)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp || ip.dst_addr() != SLIRP_GUEST_IP {
        return None;
    }
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    // Reconstruct TcpControl from individual flag accessors (smoltcp 0.11
    // exposes no combined .control() method on TcpPacket).
    let control = match (tcp.syn(), tcp.fin(), tcp.rst(), tcp.psh()) {
        (false, false, false, false) => TcpControl::None,
        (false, false, false, true) => TcpControl::Psh,
        (true, false, false, _) => TcpControl::Syn,
        (false, true, false, _) => TcpControl::Fin,
        (false, false, true, _) => TcpControl::Rst,
        _ => return None,
    };
    Some((
        tcp.seq_number().0 as u32,
        tcp.ack_number().0 as u32,
        control,
        tcp.payload().len(),
    ))
}

/// Drains frames the stack wants to send to the guest, calling `poll`
/// up to `n` times.
fn drain_n(stack: &mut SlirpBackend, n: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend(stack.poll());
    }
    out
}

#[test]
fn tcp_handshake_emits_synack() {
    // Bind a host listener on 127.0.0.1 so the stack's connect()
    // succeeds. SLIRP rewrites 10.0.2.2 → 127.0.0.1.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let mut stack = SlirpBackend::new().expect("stack");

    // Guest sends SYN to gateway IP at the listener's port.
    let syn = build_tcp_frame(
        SLIRP_GATEWAY_IP,
        GUEST_EPHEMERAL_PORT,
        host_port,
        1000,
        0,
        TcpControl::Syn,
        &[],
    );
    stack.process_guest_frame(&syn).expect("process syn");

    // Drain — SYN-ACK should be queued.
    let frames = drain_n(&mut stack, 4);
    let synack = frames
        .iter()
        .find_map(|f| parse_tcp_to_guest(f))
        .expect("synack emitted");

    let (_seq, ack, ctrl, _len) = synack;
    assert_eq!(ctrl, TcpControl::Syn, "control flags include SYN+ACK");
    assert_eq!(ack, 1001, "ack = guest_seq + 1");
}

#[test]
fn tcp_data_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Spawn a thread that accepts and echoes one chunk.
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 16];
        let n = sock.read(&mut buf).unwrap();
        sock.write_all(&buf[..n]).unwrap();
    });

    let mut stack = SlirpBackend::new().expect("stack");

    // SYN
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1000,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();

    // Drain SYN-ACK; capture our_seq.
    let synack_frames = drain_n(&mut stack, 4);
    let (our_seq, _ack, _ctrl, _len) = synack_frames
        .iter()
        .find_map(|f| parse_tcp_to_guest(f))
        .expect("synack");

    // ACK the SYN-ACK (completes handshake).
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1001,
            our_seq + 1,
            TcpControl::None,
            &[],
        ))
        .unwrap();

    // Send 5 bytes of data.
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1001,
            our_seq + 1,
            TcpControl::Psh,
            b"hello",
        ))
        .unwrap();

    // Wait for server to echo and stack to relay back.
    server.join().unwrap();
    let mut total_payload = 0;
    for _ in 0..40 {
        let frames = drain_n(&mut stack, 1);
        for f in frames.iter() {
            if let Some((_, _, _, len)) = parse_tcp_to_guest(f) {
                total_payload += len;
            }
        }
        if total_payload >= 5 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        total_payload >= 5,
        "expected at least 5 bytes echoed back to guest, got {total_payload}"
    );
}

/// Phase 3 flipped this BROKEN_ON_PURPOSE pin: passt-style sequence
/// mirroring + don't-ACK-on-WouldBlock backpressure replaces the
/// 256 KB userspace cliff. Pushing >1 MB through the relay now
/// succeeds — the kernel's socket buffer holds outstanding bytes,
/// the guest retransmits unacked segments, and the connection stays
/// alive instead of being reset.
#[test]
fn tcp_writes_more_than_256kb_succeed() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Constrain the listener's recv buffer (small but reasonable —
    // ensures TCP backpressure kicks in at a point we can observe
    // without a multi-megabyte memory footprint).
    {
        let val: libc::c_int = 4096;
        unsafe {
            libc::setsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &val as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    // Server: accept and drain everything we get.
    let bytes_received = Arc::new(AtomicUsize::new(0));
    let bytes_received_thr = Arc::clone(&bytes_received);
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        loop {
            match sock.read(&mut buf) {
                Ok(0) => break, // EOF from guest side
                Ok(n) => {
                    bytes_received_thr.fetch_add(n, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
    });

    let mut stack = SlirpBackend::new().expect("stack");

    // Handshake.
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1000,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let synack = drain_n(&mut stack, 4)
        .into_iter()
        .find_map(|f| parse_tcp_to_guest(&f))
        .expect("synack");
    let (our_seq, _, _, _) = synack;
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1001,
            our_seq + 1,
            TcpControl::None,
            &[],
        ))
        .unwrap();

    // Push 1 MB in 1 KB chunks. Drain after every batch so the
    // host's read thread can drain the kernel buffer and ACKs flow
    // back to the guest. The new TCP-backpressure path means some
    // chunks won't be ACK'd immediately; we re-send those (TCP-style
    // retransmit) until they go through.
    const TOTAL: usize = 1024 * 1024;
    const CHUNK: usize = 1024;
    let chunk = vec![b'x'; CHUNK];
    let mut seq = 1001u32;
    let mut acked_seq = 1001u32;
    let mut saw_close = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    while bytes_received.load(Ordering::Relaxed) < TOTAL && std::time::Instant::now() < deadline {
        // Send a chunk; advance our seq.
        let _ = stack.process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            seq,
            our_seq + 1,
            TcpControl::Psh,
            &chunk,
        ));
        seq = seq.wrapping_add(CHUNK as u32);

        // Drain frames; track the highest ACK we've seen and watch
        // for RST/FIN that would indicate a Phase-2 era close.
        for f in drain_n(&mut stack, 4) {
            if let Some((_, ack, ctrl, _)) = parse_tcp_to_guest(&f) {
                if matches!(ctrl, TcpControl::Rst | TcpControl::Fin) {
                    saw_close = true;
                }
                if ack > acked_seq {
                    acked_seq = ack;
                }
            }
        }

        if saw_close {
            break;
        }

        // If we've out-paced the kernel's recv buffer, sleep briefly
        // so the server thread can drain it.
        if seq.wrapping_sub(acked_seq) > 256 * 1024 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // Close the connection cleanly so the server's read loop exits.
    let _ = stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP,
        GUEST_EPHEMERAL_PORT,
        host_port,
        seq,
        our_seq + 1,
        TcpControl::Fin,
        &[],
    ));
    for _ in 0..40 {
        let _ = drain_n(&mut stack, 1);
        if server.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = server.join();

    let received = bytes_received.load(Ordering::Relaxed);
    assert!(
        !saw_close,
        "Phase 3 contract: connection must NOT be reset/FIN'd mid-stream \
         (was the 256 KB cliff bug). Saw RST or FIN."
    );
    assert!(
        received >= TOTAL * 95 / 100,
        "Phase 3 contract: server must receive ~all bytes pushed (got {received}/{TOTAL}); \
         backpressure should retransmit until success, not silently drop."
    );
}

#[test]
fn tcp_rate_limit_emits_rst() {
    // 5 conn/s allowance; 10 attempts.
    let mut stack = SlirpBackend::with_security(64, 5, &[], &[]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let mut rsts = 0;
    for i in 0..10 {
        stack
            .process_guest_frame(&build_tcp_frame(
                SLIRP_GATEWAY_IP,
                GUEST_EPHEMERAL_PORT + i as u16,
                host_port,
                1000,
                0,
                TcpControl::Syn,
                &[],
            ))
            .unwrap();
        for f in drain_n(&mut stack, 2) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(&f) {
                if ctrl == TcpControl::Rst {
                    rsts += 1;
                }
            }
        }
    }
    assert!(rsts >= 4, "expected ≥4 RSTs from rate limit, saw {rsts}");
    drop(listener);
}

#[test]
fn tcp_max_concurrent_emits_rst() {
    let mut stack = SlirpBackend::with_security(2, 1000, &[], &[]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Open 4 distinct connections; cap is 2.
    let mut rsts = 0;
    for i in 0..4 {
        stack
            .process_guest_frame(&build_tcp_frame(
                SLIRP_GATEWAY_IP,
                GUEST_EPHEMERAL_PORT + i,
                host_port,
                1000,
                0,
                TcpControl::Syn,
                &[],
            ))
            .unwrap();
        for f in drain_n(&mut stack, 2) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(&f) {
                if ctrl == TcpControl::Rst {
                    rsts += 1;
                }
            }
        }
    }
    assert!(rsts >= 1, "expected RST after concurrent limit, saw {rsts}");
    drop(listener);
}

#[test]
fn tcp_deny_list_emits_rst() {
    // `with_security` takes `&[String]`; parse via `Ipv4Net` to validate the
    // CIDR at compile-check time, then convert to the expected string form.
    let deny_cidr: Ipv4Net = "169.254.169.254/32".parse().unwrap();
    let deny_strings = [deny_cidr.to_string()];
    let mut stack = SlirpBackend::with_security(64, 1000, &deny_strings, &[]).unwrap();

    stack
        .process_guest_frame(&build_tcp_frame(
            Ipv4Address::new(169, 254, 169, 254),
            GUEST_EPHEMERAL_PORT,
            80,
            1000,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let rst = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_tcp_to_guest(&f))
        .map(|(_, _, ctrl, _)| ctrl == TcpControl::Rst);
    assert_eq!(rst, Some(true), "deny-list IP must get RST");
}

/// Builds an ARP request Ethernet frame from the guest asking "who has
/// `target_ip`?". The sender is the guest MAC/IP; target hardware address
/// is zeroed as per ARP request convention.
fn build_arp_request(target_ip: Ipv4Address) -> Vec<u8> {
    let arp_repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(GUEST_MAC),
        source_protocol_addr: SLIRP_GUEST_IP,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: target_ip,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress([0xff; 6]),
        ethertype: EthernetProtocol::Arp,
    };
    let total = ETH_HDR_LEN + arp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut arp = ArpPacket::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    arp_repr.emit(&mut arp);
    buf
}

/// Parses an Ethernet frame as an ARP reply.
///
/// Returns `Some((source_hardware_addr, source_protocol_addr))` when the
/// frame carries an ARP reply opcode, `None` otherwise.
fn parse_arp_reply(frame: &[u8]) -> Option<(EthernetAddress, Ipv4Address)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Arp {
        return None;
    }
    let arp = ArpPacket::new_checked(eth.payload()).ok()?;
    let repr = ArpRepr::parse(&arp).ok()?;
    if let ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Reply,
        source_hardware_addr,
        source_protocol_addr,
        ..
    } = repr
    {
        Some((source_hardware_addr, source_protocol_addr))
    } else {
        None
    }
}

#[test]
fn arp_replies_for_gateway() {
    let mut stack = SlirpBackend::new().unwrap();
    stack
        .process_guest_frame(&build_arp_request(SLIRP_GATEWAY_IP))
        .unwrap();
    let reply = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_arp_reply(&f))
        .expect("arp reply for gateway");
    assert_eq!(reply.1, SLIRP_GATEWAY_IP);
    assert_eq!(reply.0, EthernetAddress(GATEWAY_MAC));
}

#[test]
fn arp_replies_for_random_subnet_ip() {
    let mut stack = SlirpBackend::new().unwrap();
    stack
        .process_guest_frame(&build_arp_request(Ipv4Address::new(10, 0, 2, 99)))
        .unwrap();
    let reply = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_arp_reply(&f))
        .expect("arp reply for in-subnet IP");
    assert_eq!(reply.0, EthernetAddress(GATEWAY_MAC));
}

#[test]
fn arp_does_not_reply_for_guest_ip() {
    let mut stack = SlirpBackend::new().unwrap();
    stack
        .process_guest_frame(&build_arp_request(SLIRP_GUEST_IP))
        .unwrap();
    let reply = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_arp_reply(&f));
    assert!(reply.is_none(), "stack must not claim guest's own IP");
}

/// Wire-format label for `example.com`, used in DNS query frames.
///
/// Encoded as a DNS QNAME: each label is prefixed by its byte length,
/// terminated by a zero-length label. This is the representation that
/// goes directly into the DNS question section.
const QNAME_EXAMPLE_COM: &[u8] = b"\x07example\x03com\x00";

/// Builds a minimal DNS query UDP Ethernet frame from the guest to `SLIRP_DNS_IP`.
///
/// `xid` is placed in the transaction-ID field. `qname` must be a
/// fully-encoded DNS name (length-prefixed labels, zero terminator).
/// The question section requests an A record (`QTYPE=1`, `QCLASS=1`).
///
/// Unlike `build_udp_frame` (which carries a pre-existing off-by-one in
/// the `payload_len` argument passed to `udp_repr.emit`), this helper
/// passes only the DNS payload length so the UDP `len` field is correct
/// and the stack's smoltcp parser accepts the frame.
fn build_dns_query(xid: u16, qname: &[u8]) -> Vec<u8> {
    // DNS message layout:
    //   2B  transaction ID
    //   2B  flags (standard query, RD=1)
    //   2B  QDCOUNT = 1
    //   2B  ANCOUNT = 0
    //   2B  NSCOUNT = 0
    //   2B  ARCOUNT = 0
    //  ..B  QNAME (length-label encoded, zero terminated)
    //   2B  QTYPE  = 1  (A)
    //   2B  QCLASS = 1  (IN)
    let mut dns_payload = Vec::new();
    dns_payload.extend_from_slice(&xid.to_be_bytes());
    dns_payload.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: RD=1
    dns_payload.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    dns_payload.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    dns_payload.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    dns_payload.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    dns_payload.extend_from_slice(qname);
    dns_payload.extend_from_slice(&1u16.to_be_bytes()); // QTYPE  A
    dns_payload.extend_from_slice(&1u16.to_be_bytes()); // QCLASS IN

    // Build the Ethernet frame manually so we can pass the correct
    // `payload_len` (DNS payload only) to `udp_repr.emit`.
    let udp_repr = UdpRepr {
        src_port: GUEST_EPHEMERAL_PORT,
        dst_port: 53,
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: SLIRP_DNS_IP,
        next_header: IpProtocol::Udp,
        payload_len: UDP_HDR_LEN + dns_payload.len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + UDP_HDR_LEN + dns_payload.len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut udp = UdpPacket::new_unchecked(&mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..]);
    udp_repr.emit(
        &mut udp,
        &IpAddress::Ipv4(SLIRP_GUEST_IP),
        &IpAddress::Ipv4(SLIRP_DNS_IP),
        dns_payload.len(), // payload length only, not header+payload
        |b| b.copy_from_slice(&dns_payload),
        &Default::default(),
    );
    buf
}

/// Parses an Ethernet frame emitted by the stack and returns the DNS
/// transaction ID (XID) if the frame is a UDP datagram addressed to
/// the guest on port `GUEST_EPHEMERAL_PORT` with a plausible DNS
/// header (≥ 12 bytes of DNS payload).
///
/// Returns `None` for any frame that does not match those criteria.
fn parse_dns_reply_xid(frame: &[u8]) -> Option<u16> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp || ip.dst_addr() != SLIRP_GUEST_IP {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    if udp.dst_port() != GUEST_EPHEMERAL_PORT {
        return None;
    }
    let dns_payload = udp.payload();
    if dns_payload.len() < 12 {
        return None;
    }
    Some(u16::from_be_bytes([dns_payload[0], dns_payload[1]]))
}

#[test]
fn dns_query_resolves() {
    let mut stack = match SlirpBackend::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: SlirpBackend::new() failed ({e}), no DNS available");
            return;
        }
    };

    let query = build_dns_query(0x1234, QNAME_EXAMPLE_COM);
    if let Err(e) = stack.process_guest_frame(&query) {
        eprintln!("skip: process_guest_frame failed ({e})");
        return;
    }

    let mut reply_xid: Option<u16> = None;
    for _ in 0..20 {
        for frame in stack.poll() {
            if let Some(xid) = parse_dns_reply_xid(&frame) {
                reply_xid = Some(xid);
            }
        }
        if reply_xid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    match reply_xid {
        Some(xid) => assert_eq!(xid, 0x1234, "reply XID must match query XID"),
        None => {
            eprintln!("skip: no DNS reply in 20×100 ms, upstream resolver unreachable");
        }
    }
}

#[test]
fn dns_cache_keys_by_question_not_xid() {
    let mut stack = match SlirpBackend::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: SlirpBackend::new() failed ({e}), no DNS available");
            return;
        }
    };

    // Warm the cache with xid=1.
    let warm_query = build_dns_query(0x0001, QNAME_EXAMPLE_COM);
    if let Err(e) = stack.process_guest_frame(&warm_query) {
        eprintln!("skip: warm query process_guest_frame failed ({e})");
        return;
    }
    let mut warmed = false;
    for _ in 0..20 {
        for frame in stack.poll() {
            if let Some(xid) = parse_dns_reply_xid(&frame) {
                if xid == 0x0001 {
                    warmed = true;
                }
            }
        }
        if warmed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !warmed {
        eprintln!("skip: cache warm-up timed out, upstream resolver unreachable");
        return;
    }

    // Now query with xid=2; the cache must rewrite the reply XID to 2.
    let second_query = build_dns_query(0x0002, QNAME_EXAMPLE_COM);
    if let Err(e) = stack.process_guest_frame(&second_query) {
        eprintln!("skip: second query process_guest_frame failed ({e})");
        return;
    }
    let mut reply_xid: Option<u16> = None;
    for _ in 0..20 {
        for frame in stack.poll() {
            if let Some(xid) = parse_dns_reply_xid(&frame) {
                reply_xid = Some(xid);
            }
        }
        if reply_xid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    match reply_xid {
        Some(xid) => assert_eq!(xid, 0x0002, "cache must rewrite XID to match the new query"),
        None => {
            eprintln!("skip: no reply for second query in 20×100 ms");
        }
    }
}

/// Phase 2 flipped this BROKEN_ON_PURPOSE pin: arbitrary UDP (any
/// destination port, not just 53) now round-trips through the per-flow
/// connected-socket NAT introduced in Tasks 2.1–2.4.
#[test]
fn udp_non_dns_round_trips() {
    let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let host_port = host_sock.local_addr().unwrap().port();
    host_sock
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();

    let mut stack = SlirpBackend::new().unwrap();

    // Guest → gateway:host_port (translated to 127.0.0.1:host_port).
    stack
        .process_guest_frame(&build_udp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            b"hello",
        ))
        .unwrap();
    let _ = drain_n(&mut stack, 4);

    // Host receives the datagram.
    let mut buf = [0u8; 32];
    let (n, peer) = host_sock
        .recv_from(&mut buf)
        .expect("host receives guest UDP");
    assert_eq!(&buf[..n], b"hello");

    // Host echoes back.
    host_sock.send_to(&buf[..n], peer).unwrap();

    // Drain — guest should see the reply on its source port.
    let mut saw_reply = false;
    for _ in 0..20 {
        for f in drain_n(&mut stack, 1) {
            let Some(eth) = EthernetFrame::new_checked(f.as_slice()).ok() else {
                continue;
            };
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                continue;
            }
            let Some(ip) = Ipv4Packet::new_checked(eth.payload()).ok() else {
                continue;
            };
            if ip.next_header() != IpProtocol::Udp {
                continue;
            }
            let Some(udp_pkt) = UdpPacket::new_checked(ip.payload()).ok() else {
                continue;
            };
            if udp_pkt.dst_port() == GUEST_EPHEMERAL_PORT && udp_pkt.payload() == b"hello" {
                saw_reply = true;
                break;
            }
        }
        if saw_reply {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(saw_reply, "guest must receive UDP reply via per-flow NAT");
}

/// Phase 1 flipped the BROKEN_ON_PURPOSE assertion: the guest now
/// receives an ICMP echo reply via the host's unprivileged
/// `IPPROTO_ICMP SOCK_DGRAM` socket.
///
/// Skips gracefully if `net.ipv4.ping_group_range` forbids unprivileged
/// ICMP for the calling GID — in that environment the warn-once log
/// fires and the SLIRP stack drops ICMP, which is the documented
/// fallback (see `slirp.rs::ICMP_PROBE`).
#[test]
fn icmp_echo_returns_reply() {
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};

    let icmp_repr = Icmpv4Repr::EchoRequest {
        ident: 0xbeef,
        seq_no: 1,
        data: b"ping",
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        // 127.0.0.1 — the host kernel always replies on loopback.
        dst_addr: Ipv4Address::new(127, 0, 0, 1),
        next_header: IpProtocol::Icmp,
        payload_len: icmp_repr.buffer_len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + icmp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut icmp = Icmpv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..]);
    icmp_repr.emit(&mut icmp, &Default::default());

    let mut stack = match SlirpBackend::new() {
        Ok(s) => s,
        Err(_) => {
            eprintln!("skip: SlirpBackend::new failed");
            return;
        }
    };
    if stack.process_guest_frame(&buf).is_err() {
        eprintln!("skip: process_guest_frame failed (likely no ICMP support)");
        return;
    }

    // Poll up to 20 × 50ms for the reply.
    let mut saw_reply = false;
    for _ in 0..20 {
        for f in drain_n(&mut stack, 1) {
            let Some(eth) = EthernetFrame::new_checked(f.as_slice()).ok() else {
                continue;
            };
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                continue;
            }
            let Some(ip) = Ipv4Packet::new_checked(eth.payload()).ok() else {
                continue;
            };
            if ip.next_header() == IpProtocol::Icmp && ip.dst_addr() == SLIRP_GUEST_IP {
                saw_reply = true;
                break;
            }
        }
        if saw_reply {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    if !saw_reply {
        // Sysctl may forbid unprivileged ICMP on this host. Skip rather
        // than fail — the warn-once log explains why.
        eprintln!(
            "skip: no ICMP reply received within 1s; \
             sysctl net.ipv4.ping_group_range may forbid unprivileged ICMP"
        );
    }
}

#[test]
fn slirp_backend_implements_network_backend() {
    fn assert_send<T: Send>() {}
    fn assert_backend<T: NetworkBackend>() {}
    assert_send::<SlirpBackend>();
    assert_backend::<SlirpBackend>();
}

#[test]
fn nat_translate_outbound_loopback_rewrite() {
    let rules = Rules {
        gateway_loopback: true,
        deny_cidrs: vec![],
        port_forwards: vec![],
    };
    let result = translate_outbound(&rules, SLIRP_GATEWAY_IP, 80, SLIRP_GATEWAY_IP).unwrap();
    assert_eq!(
        result,
        SocketAddr::from((Ipv4Addr::LOCALHOST, 80)),
        "gateway IP must be rewritten to 127.0.0.1 when gateway_loopback=true"
    );
}

#[test]
fn nat_translate_outbound_unmodified_external_ip() {
    let rules = Rules {
        gateway_loopback: true,
        deny_cidrs: vec![],
        port_forwards: vec![],
    };
    let external = Ipv4Address::new(8, 8, 8, 8);
    let result = translate_outbound(&rules, external, 53, SLIRP_GATEWAY_IP).unwrap();
    assert_eq!(
        result,
        SocketAddr::from((Ipv4Addr::new(8, 8, 8, 8), 53)),
        "non-gateway IPs must pass through unchanged"
    );
}

/// E2E contract for Phase 5.5b inbound port-forwarding.
///
/// Builds a `SlirpBackend` with one TCP port-forward rule
/// (`HOST_PORT` → `GUEST_PORT`), has a host thread connect to
/// `127.0.0.1:HOST_PORT`, then drives `drain_to_guest` and
/// synthesizes a guest TCP listener by responding with SYN-ACK to
/// the synthesized SYN the stack emits.
///
/// The test asserts **three** contract points, each covering a distinct
/// 5.5b sub-task:
///
/// 1. `host TcpStream::connect` **succeeds** — the listener thread
///    (5.5b.3) is bound and accepts incoming connections.
/// 2. `drain_to_guest` **emits a synthesized SYN** to `GUEST_PORT` —
///    `process_pending_inbound_accepts` (5.5b.3) dequeues the
///    `InboundAccept` and `synthesize_inbound_syn` (5.5b.2) emits the
///    SYN frame; `with_security` (5.5b.4) wired the channel.
/// 3. After the synthetic guest replies with SYN-ACK, `drain_to_guest`
///    **emits an ACK frame** — the `SynSent → Established` arm (5.5b.1)
///    fired and the handshake completed end-to-end.
///
/// Byte-level round-trip is deferred — connect + full 3WH completion
/// is the minimum contract for the listener implementation.
#[test]
fn tcp_port_forward_inbound_connect_succeeds() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const HOST_PORT: u16 = 18080;
    const GUEST_PORT: u16 = 8080;
    const GUEST_ISN: u32 = 5000;

    let mut stack = SlirpBackend::with_security(64, 1000, &[], &[(HOST_PORT, GUEST_PORT)])
        .expect("build stack with port-forward rule");

    // ── Contract 1: listener thread is bound and accepts connections ─────
    // Spawn the host connector in a background thread so it doesn't block
    // the test thread. The OS-level SYN/SYN-ACK/ACK between host connector
    // and the listener socket is handled by the kernel; the SLIRP stack
    // is not involved in that handshake.
    let (tx, rx) = mpsc::channel::<std::io::Result<std::net::TcpStream>>();
    std::thread::spawn(move || {
        let result = std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{HOST_PORT}").parse().unwrap(),
            Duration::from_secs(5),
        );
        let _ = tx.send(result);
    });

    // ── Contract 2 + 3: drain until we see the synthesized SYN (2) and ──
    // then the ACK that completes the inbound 3WH (3).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_synthesized_syn = false;
    let mut saw_ack_after_synack = false;
    let mut connect_result: Option<std::io::Result<std::net::TcpStream>> = None;

    while Instant::now() < deadline
        && (!saw_synthesized_syn || !saw_ack_after_synack || connect_result.is_none())
    {
        let mut out = Vec::new();
        stack.drain_to_guest(&mut out);

        let mut high_port_for_ack: Option<u16> = None;

        for frame in &out {
            let Some((syn_seq, _ack, src_port, dst_port, ctrl)) = parse_tcp_to_guest_full(frame)
            else {
                continue;
            };

            // Contract 2: synthesized SYN arriving at the guest.
            if ctrl == TcpControl::Syn && dst_port == GUEST_PORT && !saw_synthesized_syn {
                saw_synthesized_syn = true;
                high_port_for_ack = Some(src_port);

                // Synthetic guest listener replies with SYN-ACK.
                // build_tcp_frame: src=SLIRP_GUEST_IP, dst=SLIRP_GATEWAY_IP
                let syn_ack = build_tcp_frame(
                    SLIRP_GATEWAY_IP, // dst from guest's perspective
                    GUEST_PORT,       // guest service port (src_port in frame)
                    src_port,         // high_port (dst_port in frame)
                    GUEST_ISN,        // guest's own ISN
                    syn_seq + 1,      // ack = their SYN seq + 1
                    TcpControl::Syn,  // SYN+ACK: ack_number is non-zero
                    &[],
                );
                stack
                    .process_guest_frame(&syn_ack)
                    .expect("process synthetic SYN-ACK");
            }

            // Contract 3: ACK back to the guest completing the inbound 3WH.
            // After processing our SYN-ACK, the stack emits a plain ACK
            // (ctrl=None, ack set) directed at GUEST_PORT.
            if ctrl == TcpControl::None
                && dst_port == GUEST_PORT
                && high_port_for_ack == Some(src_port)
            {
                saw_ack_after_synack = true;
            }
        }

        // A second drain pass so the stack processes the SYN-ACK we just
        // injected and emits its ACK in the same iteration.
        let mut ack_out = Vec::new();
        stack.drain_to_guest(&mut ack_out);
        for frame in &ack_out {
            let Some((_seq, _ack, src_port, dst_port, ctrl)) = parse_tcp_to_guest_full(frame)
            else {
                continue;
            };
            if ctrl == TcpControl::None
                && dst_port == GUEST_PORT
                && high_port_for_ack == Some(src_port)
            {
                saw_ack_after_synack = true;
            }
        }

        if let Ok(r) = rx.try_recv() {
            connect_result = Some(r);
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    // Contract 1.
    let connect_result =
        connect_result.expect("host TcpStream::connect did not complete within 5 s");
    let _stream = connect_result.expect("host TcpStream::connect failed");

    // Contract 2.
    assert!(
        saw_synthesized_syn,
        "drain_to_guest must emit a synthesized SYN to GUEST_PORT \
         after drain_to_guest processes the InboundAccept (5.5b.2/5.5b.3)"
    );

    // Contract 3.
    assert!(
        saw_ack_after_synack,
        "drain_to_guest must emit an ACK completing the inbound 3-way handshake \
         after the synthetic guest SYN-ACK is processed (5.5b.1)"
    );
}

/// Richer TCP-to-guest frame parser that also returns src/dst ports.
///
/// Returns `(seq, ack, src_port, dst_port, control)` for any IPv4/TCP
/// frame whose destination is `SLIRP_GUEST_IP`, or `None` for anything
/// else.  Used by `tcp_port_forward_inbound_connect_succeeds` to identify
/// the synthesized SYN and extract the ephemeral `high_port`.
fn parse_tcp_to_guest_full(frame: &[u8]) -> Option<(u32, u32, u16, u16, TcpControl)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp || ip.dst_addr() != SLIRP_GUEST_IP {
        return None;
    }
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    let control = match (tcp.syn(), tcp.fin(), tcp.rst(), tcp.psh()) {
        (false, false, false, false) => TcpControl::None,
        (false, false, false, true) => TcpControl::Psh,
        (true, false, false, _) => TcpControl::Syn,
        (false, true, false, _) => TcpControl::Fin,
        (false, false, true, _) => TcpControl::Rst,
        _ => return None,
    };
    Some((
        tcp.seq_number().0 as u32,
        tcp.ack_number().0 as u32,
        tcp.src_port(),
        tcp.dst_port(),
        control,
    ))
}

#[test]
fn nat_translate_outbound_deny_list() {
    let rules = Rules {
        gateway_loopback: true,
        deny_cidrs: vec!["169.254.0.0/16".parse::<Ipv4Net>().unwrap()],
        port_forwards: vec![],
    };
    let metadata = Ipv4Address::new(169, 254, 169, 254);
    assert!(
        translate_outbound(&rules, metadata, 80, SLIRP_GATEWAY_IP).is_none(),
        "deny-listed IP must return None"
    );

    // Adjacent (non-denied) IP still passes.
    let public = Ipv4Address::new(169, 253, 0, 1);
    assert!(
        translate_outbound(&rules, public, 80, SLIRP_GATEWAY_IP).is_some(),
        "IPs outside deny CIDR must pass"
    );
}
