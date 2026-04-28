//! Layer-1 correctness pins for the smoltcp-based SLIRP stack.
//!
//! These tests drive `SlirpStack` directly with synthetic Ethernet
//! frames — no VM, no kernel, no host sockets to outside hosts. The
//! goal is to lock observable behavior (including deliberately broken
//! behavior) so the passt-pattern refactor's diff is legible to
//! reviewers.
//!
//! Three tests assert *broken* behavior on purpose. Each is marked
//! `BROKEN_ON_PURPOSE` and flips in the phase that fixes it:
//!
//! - `tcp_to_host_buffer_drops_at_256kb` — flips in Phase 3
//! - `udp_non_dns_silently_dropped` — flips in Phase 2
//! - `icmp_echo_silently_dropped` — flips in Phase 1
//!
//! Run with: `cargo test --test network_baseline`

#![cfg(target_os = "linux")]
// Imports and helpers used by test cases added in tasks 0A.2–0A.9.
#![allow(unused_imports, dead_code)]

use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket,
    TcpRepr, UdpPacket, UdpRepr,
};
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::os::unix::io::AsRawFd;
use void_box::network::slirp::{
    SlirpStack, GATEWAY_MAC, GUEST_MAC, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
};
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
        UDP_HDR_LEN + payload.len(),
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
fn drain_n(stack: &mut SlirpStack, n: usize) -> Vec<Vec<u8>> {
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

    let mut stack = SlirpStack::new().expect("stack");

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

    let mut stack = SlirpStack::new().expect("stack");

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

/// BROKEN_ON_PURPOSE — flips in Phase 3.
///
/// Today: when guest writes >256 KB to host before host reads,
/// `to_host` buffer overflows and the connection is closed
/// (`slirp.rs:903–910`). The stack silently removes the NAT entry
/// (no RST, no FIN to guest); subsequent frames from the guest are
/// dropped without acknowledgement.
///
/// After Phase 3 (MSG_PEEK + sequence mirroring): the host kernel's
/// socket buffer absorbs the write; no userspace cap, no drop.
/// All data is eventually acknowledged.
#[test]
fn tcp_to_host_buffer_drops_at_256kb() {
    // Pin the listener's SO_RCVBUF to 4 096 bytes. The kernel doubles
    // it to 8 192 B (its enforced minimum) and propagates that to the
    // accepted socket. This constrains how much data the kernel buffers;
    // combined with the sender's default SO_SNDBUF (~208 KB), writes to
    // `host_stream` return WouldBlock after ~1 751 KB.
    //
    // Once the first WouldBlock occurs (slirp.rs:893), payload goes into
    // `to_host`. Each subsequent poll() calls relay_tcp_nat_data() which
    // tries to flush `to_host` but keeps getting WouldBlock (OS still
    // full), so `to_host` grows. After 256 KB accumulates the `else`
    // branch fires (slirp.rs:907), state → Closed, NAT entry removed.
    // No RST/FIN is sent; from the guest's perspective the connection
    // simply goes silent — pushed frames generate no ACKs.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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
    let host_port = listener.local_addr().unwrap().port();

    // Server thread: accept and sleep without reading. The constrained
    // receive buffer fills quickly; TCP flow-control stalls slirp's
    // host_stream writes with WouldBlock.
    let _server = std::thread::spawn(move || {
        let (_sock, _) = listener.accept().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
    });

    let mut stack = SlirpStack::new().expect("stack");

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

    // Push 2 500 × 1 KB chunks in batches of 500, draining after each
    // batch. The drain lets relay_tcp_nat_data() attempt to flush the
    // `to_host` buffer; while the OS receive buffer is full it gets
    // WouldBlock and the buffer keeps growing.
    //
    // Expected timeline (observed on this host):
    //   Chunks   0–1751: direct writes succeed; OS absorbs ~1 751 KB.
    //   Chunks 1752–2007: WouldBlock; payloads go into `to_host`.
    //   Chunk  ~2007: `to_host` exceeds 256 KB → state = Closed.
    //   Chunks 2008–2500: NAT entry gone; no ACKs returned.
    //
    // We detect the connection drop by tracking whether the last batch's
    // poll returned any frame to the guest. After the drop, batches
    // return 0 frames (no ACKs, no FIN, no RST).
    let mut seq = 1001u32;
    let chunk = vec![b'x'; 1024];
    let mut saw_close = false;
    const BATCH: usize = 500;
    const TOTAL: usize = 2500;

    for batch_start in (0..TOTAL).step_by(BATCH) {
        for _ in batch_start..batch_start + BATCH {
            let _ = stack.process_guest_frame(&build_tcp_frame(
                SLIRP_GATEWAY_IP,
                GUEST_EPHEMERAL_PORT,
                host_port,
                seq,
                our_seq + 1,
                TcpControl::Psh,
                &chunk,
            ));
            seq = seq.wrapping_add(1024);
        }
        let frames = stack.poll();
        // After the cliff the connection is silently removed:
        // no ACKs, no FIN, no RST — exactly 0 frames returned for a full
        // batch of pushed data. We require the connection to have been
        // alive for at least the first batch before declaring it dead.
        if batch_start >= BATCH && frames.is_empty() {
            saw_close = true;
            break;
        }
        // Also check for RST/FIN for completeness (not emitted today).
        for f in &frames {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(f) {
                if matches!(ctrl, TcpControl::Rst | TcpControl::Fin) {
                    saw_close = true;
                }
            }
        }
        if saw_close {
            break;
        }
    }
    assert!(
        saw_close,
        "BROKEN_ON_PURPOSE: today the 256 KB to_host cliff silently drops \
         the connection (slirp.rs:907–910) — no RST/FIN sent, subsequent \
         chunks receive no ACK. If this assertion fails, Phase 3 may have \
         already landed — flip the assertion to `assert!(!saw_close)` and \
         verify all 2 500 chunks are eventually acknowledged."
    );
}

#[test]
fn tcp_rate_limit_emits_rst() {
    // 5 conn/s allowance; 10 attempts.
    let mut stack = SlirpStack::with_security(64, 5, &[]).unwrap();
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
    let mut stack = SlirpStack::with_security(2, 1000, &[]).unwrap();
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
    let mut stack = SlirpStack::with_security(64, 1000, &deny_strings).unwrap();

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
