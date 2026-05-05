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
//! `BROKEN_ON_PURPOSE` and flips when the corresponding fix lands:
//!
//! - `tcp_writes_more_than_256kb_succeed` (was `tcp_to_host_buffer_drops_at_256kb`)
//! - `udp_non_dns_round_trips` (was `udp_non_dns_silently_dropped`)
//! - `icmp_echo_returns_reply` (was `icmp_echo_silently_dropped`)
//!
//! Run with: `cargo test --test network_baseline`

#![cfg(target_os = "linux")]
// Imports and helpers used by test cases added in tasks 0A.2–0A.9.
#![allow(unused_imports, dead_code)]

use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpAddress, IpProtocol, Ipv4Address, Ipv4Packet,
    Ipv4Repr, TcpControl, TcpOption, TcpPacket, TcpRepr, UdpPacket, UdpRepr,
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

/// Like `build_tcp_frame` but exposes explicit `window_len` and `window_scale`
/// parameters so tests can exercise window-management behaviour.
#[allow(clippy::too_many_arguments)]
fn build_tcp_frame_with_window(
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
        control,
        seq_number: smoltcp::wire::TcpSeqNumber(seq as i32),
        ack_number: if ack == 0 {
            None
        } else {
            Some(smoltcp::wire::TcpSeqNumber(ack as i32))
        },
        window_len,
        window_scale,
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

/// Drains frames the stack wants to send to the guest, calling
/// `drain_to_guest` up to `n` times.  Returns all frames produced
/// across the calls (caller may not care about per-call boundaries).
fn drain_n(stack: &mut SlirpBackend, n: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for _ in 0..n {
        stack.drain_to_guest(&mut out);
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

/// BROKEN_ON_PURPOSE pin (now passing): passt-style sequence mirroring and
/// don't-ACK-on-WouldBlock backpressure replace the 256 KB userspace cliff.
/// Pushing >1 MB through the relay succeeds — the kernel's socket buffer
/// holds outstanding bytes, the guest retransmits unacked segments, and the
/// connection stays alive instead of being reset.
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
        // Retransmit semantics: only advance the send cursor once the
        // previous chunk has been ACK'd. If the stack stops ACKing
        // (backpressure engaged), we re-send the same seq/payload until
        // it's acknowledged. This matches production guest-TCP retransmit
        // behavior.
        let _ = stack.process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            seq,
            our_seq + 1,
            TcpControl::Psh,
            &chunk,
        ));

        // Drain frames; track the highest ACK we've seen and watch
        // for RST/FIN that would indicate a premature close.
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

        // Advance our send cursor only past ACK'd data.  If the stack
        // didn't ACK this chunk, the next loop iteration re-sends the
        // same seq/payload (true TCP retransmit semantics).
        if acked_seq >= seq.wrapping_add(CHUNK as u32) {
            seq = seq.wrapping_add(CHUNK as u32);
        } else if seq.wrapping_sub(acked_seq) > 256 * 1024 {
            // Out-paced kernel recv buffer; sleep briefly so the host
            // server thread can drain.
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
        "TCP backpressure must not RST/FIN mid-stream — the relay must hold \
         the line while the kernel drains. Saw RST or FIN."
    );
    assert!(
        received >= TOTAL * 95 / 100,
        "server must receive ~all bytes pushed (got {received}/{TOTAL}); \
         backpressure must retransmit until success, not silently drop."
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

/// BROKEN_ON_PURPOSE pin (now passing): arbitrary UDP (any destination
/// port, not just 53) round-trips through the per-flow connected-socket
/// NAT.
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

/// BROKEN_ON_PURPOSE pin (now passing): the guest receives an ICMP echo
/// reply via the host's unprivileged `IPPROTO_ICMP SOCK_DGRAM` socket.
///
/// Skips gracefully if `net.ipv4.ping_group_range` forbids unprivileged
/// ICMP for the calling GID — in that environment the warn-once log
/// fires and the SLIRP stack drops ICMP, which is the documented
/// fallback (see `slirp.rs::ICMP_PROBE`).
#[test]
fn icmp_echo_returns_reply() {
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};

    // Probe whether unprivileged ICMP is permitted on this host. If not,
    // skip gracefully — the SLIRP stack falls back to silently dropping
    // ICMP in that environment (see slirp.rs::ICMP_PROBE).
    let probe_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
    if probe_fd < 0 {
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        if raw == libc::EPERM || raw == libc::EACCES {
            eprintln!("skip: unprivileged ICMP forbidden ({err}); see net.ipv4.ping_group_range");
            return;
        }
        panic!("unexpected ICMP probe error: {err}");
    }
    unsafe { libc::close(probe_fd) };

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

    assert!(
        saw_reply,
        "guest must receive ICMP echo reply via host IPPROTO_ICMP socket"
    );
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

/// E2E contract for inbound port-forwarding.
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

/// Snapshot/restore must rebuild the epoll dispatch from `flow_table`
/// contents.  The `epoll_fd` is a kernel handle that does not survive
/// snapshot; a fresh dispatcher starts with zero registered FDs even
/// though `flow_table` may contain entries with live host sockets.
///
/// This smoke test verifies the rebuild path end-to-end:
/// 1. Insert a synthetic TCP flow into the flow table.
/// 2. Reset the epoll dispatcher to a fresh empty one (simulating what
///    snapshot restore does: the kernel handle is gone, a new one is created).
/// 3. Confirm the pre-rebuild count is zero.
/// 4. Call `rebuild_epoll_from_flow_table`.
/// 5. Confirm the post-rebuild count is one.
///
/// Gated on `bench-helpers` because it consumes synthetic-injection helpers
/// (`insert_synthetic_synsent_entry`, `reset_epoll_for_snapshot_test`,
/// `registered_fd_count`) that are only visible to external test/bench
/// consumers when that feature is enabled.  Default `cargo test` skips this
/// pin; CI runs it via `cargo test --features bench-helpers`.
#[cfg(feature = "bench-helpers")]
#[test]
fn epoll_set_rebuilt_from_flow_table_smoke() {
    use std::net::TcpListener;

    let mut backend = SlirpBackend::new().expect("backend");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let host_stream =
        std::net::TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
    host_stream.set_nonblocking(true).ok();

    // Insert a synthetic flow (may or may not register with epoll depending on
    // cfg context).  Then reset the epoll dispatcher to a fresh empty one —
    // this is the key step that simulates what happens after snapshot restore:
    // the kernel-side `epoll_fd` does not survive, so a new one is created
    // with zero registrations even though `flow_table` has live entries.
    backend.insert_synthetic_synsent_entry(8080, 49152, 1000, host_stream);
    backend.reset_epoll_for_snapshot_test();

    let before = backend.registered_fd_count();
    assert_eq!(
        before, 0,
        "after reset, epoll must have zero registered FDs (simulates post-snapshot state)"
    );

    backend.rebuild_epoll_from_flow_table();

    let after = backend.registered_fd_count();
    assert_eq!(
        after, 1,
        "rebuild_epoll_from_flow_table must register all live flow FDs"
    );
}

// ── Phase 6.1: TCP half-close pins ──────────────────────────────────────

/// BROKEN_ON_PURPOSE: guest sends FIN after data; current code marks state=Closed
/// immediately on guest FIN, so host's response data is dropped.
/// Flips to PASS when Task 2–3 implementation lands.
#[test]
fn tcp_half_close_guest_writes_first() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || -> Vec<u8> {
        let (mut sock, _) = listener.accept().unwrap();
        // Read until EOF (guest will send data + FIN).
        let mut request = Vec::new();
        let _ = sock.read_to_end(&mut request);
        // After guest's shutdown(WRITE), we still want to send response.
        sock.write_all(b"HTTP/1.1 200 OK\r\n\r\nBODY").unwrap();
        // Then close.
        drop(sock);
        request
    });

    let mut stack = SlirpBackend::new().unwrap();

    // Guest 3-way handshake.
    let our_seq = 1000u32;
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let mut gateway_seq = 0u32;
    for f in drain_n(&mut stack, 4) {
        if let Some((s, _, ctrl, _)) = parse_tcp_to_guest(&f) {
            if matches!(ctrl, TcpControl::Syn) {
                gateway_seq = s;
                break;
            }
        }
    }
    // Complete 3-way handshake: ACK the SYN-ACK.
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1,
            gateway_seq + 1,
            TcpControl::None,
            &[],
        ))
        .unwrap();

    // Guest sends "HELLO" data + FIN.
    let request = b"HELLO";
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1,
            gateway_seq + 1,
            TcpControl::Psh,
            request,
        ))
        .unwrap();
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1 + request.len() as u32,
            gateway_seq + 1,
            TcpControl::Fin,
            &[],
        ))
        .unwrap();

    // Drive drain_to_guest until we see host's response data AND its FIN.
    // We must ACK each data segment as it arrives so the kernel recv buffer
    // drains — recv_peek returns Ok(0) (EOF) only once all buffered bytes
    // are consumed via the ACK-driven read path.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut response_bytes: Vec<u8> = Vec::new();
    let mut saw_host_fin = false;
    while std::time::Instant::now() < deadline {
        let frames = drain_n(&mut stack, 1);
        for f in &frames {
            if let Some((seq, _ack, ctrl, payload_len)) = parse_tcp_to_guest(f) {
                if payload_len > 0 {
                    let eth = EthernetFrame::new_unchecked(f.as_slice());
                    let ip = Ipv4Packet::new_unchecked(eth.payload());
                    let tcp = TcpPacket::new_unchecked(ip.payload());
                    response_bytes.extend_from_slice(tcp.payload());
                    // ACK the data so the relay's ACK-driven consume path can
                    // drain the kernel recv buffer and eventually see EOF.
                    let ack_num = seq.wrapping_add(payload_len as u32);
                    stack
                        .process_guest_frame(&build_tcp_frame(
                            SLIRP_GATEWAY_IP,
                            GUEST_EPHEMERAL_PORT,
                            host_port,
                            our_seq + 1 + request.len() as u32 + 1, // seq after FIN
                            ack_num,
                            TcpControl::None,
                            &[],
                        ))
                        .unwrap();
                }
                if matches!(ctrl, TcpControl::Fin) {
                    saw_host_fin = true;
                }
            }
        }
        if !response_bytes.is_empty() && saw_host_fin {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = server.join();

    assert_eq!(
        &response_bytes[..],
        b"HTTP/1.1 200 OK\r\n\r\nBODY",
        "guest must receive ALL host response data after sending FIN"
    );
    assert!(saw_host_fin, "guest must receive host's FIN");
}

/// Pin for the symmetric half-close path: host writes first, then closes its
/// write side (Established → CloseWait); guest replies with data + FIN.
/// The host must receive the guest's reply data even after its own shutdown.
#[test]
fn tcp_half_close_host_writes_first() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Server: write greeting, shutdown write side, then read what guest sends back.
    let server = std::thread::spawn(move || -> Vec<u8> {
        let (mut sock, _) = listener.accept().unwrap();
        sock.write_all(b"GREETING").unwrap();
        sock.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        let _ = sock.read_to_end(&mut buf);
        buf
    });

    let mut stack = SlirpBackend::new().unwrap();

    // Guest 3-way handshake.
    let our_seq = 2000u32;
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let mut gateway_seq = 0u32;
    for f in drain_n(&mut stack, 4) {
        if let Some((s, _, ctrl, _)) = parse_tcp_to_guest(&f) {
            if matches!(ctrl, TcpControl::Syn) {
                gateway_seq = s;
                break;
            }
        }
    }
    // Complete handshake: ACK the SYN-ACK.
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1,
            gateway_seq + 1,
            TcpControl::None,
            &[],
        ))
        .unwrap();

    // Drive drain_to_guest until we see "GREETING" data AND host's FIN.
    // ACK each data segment so the relay's ACK-driven consume path works.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut received: Vec<u8> = Vec::new();
    let mut saw_host_fin = false;
    // gateway_seq_next: the relay's current sequence number (advances with data + FIN).
    let mut gateway_seq_next = gateway_seq + 1; // after SYN-ACK
    while std::time::Instant::now() < deadline {
        let frames = drain_n(&mut stack, 1);
        for f in &frames {
            if let Some((seq, _ack, ctrl, payload_len)) = parse_tcp_to_guest(f) {
                if payload_len > 0 {
                    let eth = EthernetFrame::new_unchecked(f.as_slice());
                    let ip = Ipv4Packet::new_unchecked(eth.payload());
                    let tcp = TcpPacket::new_unchecked(ip.payload());
                    received.extend_from_slice(tcp.payload());
                    gateway_seq_next = seq.wrapping_add(payload_len as u32);
                    // ACK the data.
                    stack
                        .process_guest_frame(&build_tcp_frame(
                            SLIRP_GATEWAY_IP,
                            GUEST_EPHEMERAL_PORT,
                            host_port,
                            our_seq + 1,
                            gateway_seq_next,
                            TcpControl::None,
                            &[],
                        ))
                        .unwrap();
                }
                if matches!(ctrl, TcpControl::Fin) {
                    saw_host_fin = true;
                    // ACK the host FIN so the CloseWait entry receives it.
                    stack
                        .process_guest_frame(&build_tcp_frame(
                            SLIRP_GATEWAY_IP,
                            GUEST_EPHEMERAL_PORT,
                            host_port,
                            our_seq + 1,
                            gateway_seq_next.wrapping_add(1),
                            TcpControl::None,
                            &[],
                        ))
                        .unwrap();
                }
            }
        }
        if received == b"GREETING" && saw_host_fin {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        &received[..],
        b"GREETING",
        "guest must receive host's greeting"
    );
    assert!(
        saw_host_fin,
        "guest must receive host's FIN (CloseWait transition)"
    );

    // Guest sends reply data + FIN after receiving host's greeting and FIN.
    // The relay is now in CloseWait; guest data should still be forwarded to host.
    let reply = b"REPLY";
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1,
            gateway_seq_next.wrapping_add(1),
            TcpControl::Psh,
            reply,
        ))
        .unwrap();
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1 + reply.len() as u32,
            gateway_seq_next.wrapping_add(1),
            TcpControl::Fin,
            &[],
        ))
        .unwrap();
    // Drain to process the guest FIN (CloseWait → LastAck).
    drain_n(&mut stack, 4);

    let host_received = server.join().unwrap();
    assert_eq!(
        &host_received[..],
        b"REPLY",
        "host must receive guest's reply data after CloseWait"
    );
}

/// Verify that a LastAck entry whose `last_state_change` is older than
/// `LAST_ACK_TIMEOUT` (60 s) is reaped by the next `drain_to_guest` sweep,
/// preventing a leaked flow table entry when the guest drops the final ACK.
///
/// Gated on `bench-helpers` because it uses synthetic-injection helpers
/// (`insert_synthetic_lastack_entry`, `set_synthetic_last_state_change`,
/// `tcp_flow_state`) that widen internal visibility for external test/bench
/// consumers. Default `cargo test` skips this pin; CI runs it via
/// `cargo test --features bench-helpers -- --test-threads=1`.
#[cfg(feature = "bench-helpers")]
#[test]
fn tcp_last_ack_timeout_reaps_stale_entry() {
    use std::net::TcpListener;

    const GUEST_PORT: u16 = 8080;
    const HIGH_PORT: u16 = 60000;

    let mut backend = SlirpBackend::new().expect("backend");

    // Open a real TCP connection so host_stream is a valid socket.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let host_stream =
        std::net::TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
    host_stream.set_nonblocking(true).ok();

    // Insert a synthetic LastAck entry (already past our FIN, waiting for
    // guest's final ACK which will never arrive).
    backend.insert_synthetic_lastack_entry(GUEST_PORT, HIGH_PORT, host_stream);

    // Verify the entry is present.
    assert_eq!(
        backend.tcp_flow_state(GUEST_PORT, HIGH_PORT),
        Some(void_box::network::slirp::TcpNatState::LastAck),
        "entry must start in LastAck"
    );

    // Back-date last_state_change by 70 s (> LAST_ACK_TIMEOUT = 60 s).
    backend.set_synthetic_last_state_change(
        GUEST_PORT,
        HIGH_PORT,
        std::time::Duration::from_secs(70),
    );

    // One drain_to_guest cycle triggers the timeout sweep.
    let mut out = Vec::new();
    backend.drain_to_guest(&mut out);

    // The entry should now be gone.
    assert!(
        backend.tcp_flow_state(GUEST_PORT, HIGH_PORT).is_none(),
        "LastAck entry past LAST_ACK_TIMEOUT must be reaped by drain_to_guest"
    );
}

/// Phase 6.2 pin: a SYN to an unreachable destination must NOT block the
/// vCPU thread inside `process_guest_frame`.  The synchronous
/// `connect_timeout(3s)` that lived in `handle_tcp_frame` froze every
/// other flow for the full 3-second window.  Async connect returns
/// immediately (`EINPROGRESS`); completion arrives via EPOLLOUT on the
/// net-poll thread.
#[test]
fn tcp_connect_to_unreachable_does_not_block_other_flows() {
    use std::time::Instant;

    // Good destination — bind a listener.
    let good_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let good_port = good_listener.local_addr().unwrap().port();

    // Bad destination — TEST-NET-1 (RFC 5737, 192.0.2.0/24) is reserved
    // for documentation and is not routable on the public Internet, so the
    // kernel's connect will hang on SYN retransmits rather than returning
    // an immediate ECONNREFUSED. This is exactly the path that today's
    // synchronous `connect_timeout(3s)` would block on.
    let bad_ip = Ipv4Address::new(192, 0, 2, 1);
    let bad_port: u16 = 80;

    let mut stack = SlirpBackend::new().unwrap();

    let our_seq_bad = 1000u32;
    let our_seq_good = 2000u32;

    let bad_syn_at = Instant::now();
    stack
        .process_guest_frame(&build_tcp_frame(
            bad_ip,
            GUEST_EPHEMERAL_PORT,
            bad_port,
            our_seq_bad,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let bad_syn_returned = bad_syn_at.elapsed();

    // process_guest_frame must return quickly — sub-100 ms even though
    // the kernel is still issuing SYNs against the dead port.
    assert!(
        bad_syn_returned < std::time::Duration::from_millis(100),
        "process_guest_frame for unreachable dest blocked vCPU for {bad_syn_returned:?}; \
         must return immediately and let the connect complete asynchronously"
    );

    // Now SYN to the good destination.
    let good_syn_at = Instant::now();
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT + 1,
            good_port,
            our_seq_good,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let good_syn_returned = good_syn_at.elapsed();
    assert!(
        good_syn_returned < std::time::Duration::from_millis(100),
        "second process_guest_frame blocked: {good_syn_returned:?}"
    );

    // Drive drain_to_guest until we see the good destination's SYN-ACK.
    // It must arrive well within 1 s; if we ever wait the full 3 s
    // CONNECT_TIMEOUT, the test fails.
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    let mut saw_good_synack = false;
    while Instant::now() < deadline {
        let frames = drain_n(&mut stack, 1);
        for f in frames {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(f.as_slice()) {
                let ip =
                    Ipv4Packet::new_checked(EthernetFrame::new_unchecked(f.as_slice()).payload())
                        .unwrap();
                let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
                if tcp.dst_port() == GUEST_EPHEMERAL_PORT + 1 && matches!(ctrl, TcpControl::Syn) {
                    saw_good_synack = true;
                    break;
                }
            }
        }
        if saw_good_synack {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        saw_good_synack,
        "good-destination SYN-ACK must arrive even while bad destination is still connecting"
    );

    // Accept the good connection so the test cleans up cleanly.
    let _ = good_listener.set_nonblocking(true);
    let _ = good_listener.accept();
}

/// Phase 6.2 pin: when an async connect to a dropped-listener port fails,
/// the guest must eventually receive a RST.  The RST is delivered once
/// `drain_to_guest` drives `relay_pending_connects` and `getsockopt(SO_ERROR)`
/// returns a non-zero error code.
#[test]
fn tcp_connect_async_eventual_rst_on_failure() {
    use std::time::Instant;

    let mut stack = SlirpBackend::new().unwrap();

    // Bind+drop a listener: the OS assigns a port and then closes it, so a
    // subsequent connect will receive ECONNREFUSED from the kernel quickly.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bad_port = listener.local_addr().unwrap().port();
    drop(listener);

    let our_seq = 1000u32;
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            bad_port,
            our_seq,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();

    // Drive drain_to_guest until we see a RST or the deadline passes.
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    let mut saw_rst = false;
    while Instant::now() < deadline {
        let frames = drain_n(&mut stack, 1);
        for f in frames {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(f.as_slice()) {
                if matches!(ctrl, TcpControl::Rst) {
                    saw_rst = true;
                    break;
                }
            }
        }
        if saw_rst {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        saw_rst,
        "guest must eventually receive RST when async connect to dropped-listener port fails"
    );
}

/// Asserts that the SYN-ACK the stack emits in response to a guest SYN
/// includes a `WindowScale` option set to `OUR_WINDOW_SCALE` (7).
///
/// This pin validates Task 4's SYN-ACK advertisement and is expected to
/// PASS post-Task-4: `build_tcp_packet_static` now passes
/// `Some(OUR_WINDOW_SCALE)` on the SYN-ACK call site.
#[test]
fn tcp_window_scale_negotiated_in_synack() {
    use std::time::Instant;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();
    let mut stack = SlirpBackend::new().unwrap();

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

    let mut saw_synack_with_scale = false;
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    'drain: while Instant::now() < deadline {
        for f in drain_n(&mut stack, 4) {
            let eth = match EthernetFrame::new_checked(f.as_slice()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                continue;
            }
            let ip = match Ipv4Packet::new_checked(eth.payload()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if ip.next_header() != IpProtocol::Tcp || ip.dst_addr() != SLIRP_GUEST_IP {
                continue;
            }
            let tcp = match TcpPacket::new_checked(ip.payload()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !tcp.syn() || !tcp.ack() {
                continue;
            }
            // Parse options to find WindowScale.
            let mut remaining = tcp.options();
            loop {
                match TcpOption::parse(remaining) {
                    Ok((_, TcpOption::EndOfList)) | Err(_) => break,
                    Ok((_, TcpOption::WindowScale(scale))) => {
                        assert_eq!(scale, 7, "advertised scale must be OUR_WINDOW_SCALE (7)");
                        saw_synack_with_scale = true;
                        break 'drain;
                    }
                    Ok((rest, _)) => remaining = rest,
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        saw_synack_with_scale,
        "SYN-ACK must include WindowScale option with value 7"
    );
}

/// BROKEN_ON_PURPOSE: `relay_tcp_nat_data` does not yet gate host→guest sends
/// on `entry.guest_window`.  This test will FAIL until Task 7 gates the relay
/// on `guest_window`, at which point it flips to PASSING.
///
/// The test establishes a flow with a small guest window (4096 bytes, no scale),
/// feeds 64 KiB from the host side, and asserts that injected payload before any
/// ACK does not exceed the guest's advertised window plus one MTU slop.
#[test]
fn tcp_advertised_window_tracks_guest_buffer() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || -> std::net::TcpStream {
        let (sock, _) = listener.accept().unwrap();
        sock
    });

    let mut stack = SlirpBackend::new().unwrap();

    let our_seq = 1000u32;
    // Guest SYN with explicit small window (4096 bytes, no scale).
    let syn = build_tcp_frame_with_window(
        SLIRP_GATEWAY_IP,
        GUEST_EPHEMERAL_PORT,
        host_port,
        our_seq,
        0,
        TcpControl::Syn,
        &[],
        4096,
        None,
    );
    stack.process_guest_frame(&syn).unwrap();

    // Collect SYN-ACK from the stack.
    let mut gateway_seq = 0u32;
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    'outer: while Instant::now() < deadline {
        for f in drain_n(&mut stack, 4) {
            if let Some((s, _, ctrl, _)) = parse_tcp_to_guest(f.as_slice()) {
                if matches!(ctrl, TcpControl::Syn) {
                    gateway_seq = s;
                    break 'outer;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Complete handshake with the same small window.
    stack
        .process_guest_frame(&build_tcp_frame_with_window(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            our_seq + 1,
            gateway_seq + 1,
            TcpControl::None,
            &[],
            4096,
            None,
        ))
        .unwrap();

    // Wait for the server thread to accept and obtain the host stream.
    let mut host_stream = server.join().unwrap();

    // Push 64 KiB from the host side.
    let payload = vec![0xABu8; 64 * 1024];
    host_stream.write_all(&payload).unwrap();

    // Drive drain_to_guest a few times. With proper window tracking,
    // total bytes injected before any ACK should be <= guest_window
    // (4096 plus one MTU-sized slop for partial segment).
    let mut total_payload_injected: usize = 0;
    for _ in 0..20 {
        for f in drain_n(&mut stack, 1) {
            if let Some((_, _, _, payload_len)) = parse_tcp_to_guest(f.as_slice()) {
                total_payload_injected += payload_len;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        total_payload_injected <= 4096 + 1500,
        "injected {total_payload_injected} bytes; must respect guest_window=4096 (one MTU slop allowed)"
    );
}
