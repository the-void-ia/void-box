//! Divan micro-benchmarks for SLIRP hot paths.
//!
//! Mirrors `benches/startup.rs` in shape. Job: regression detection
//! for the per-packet hot path on the vCPU and net-poll threads.
//!
//! Run with: `cargo bench --bench network`

// TODO(0D.5): migrate poll() → drain_to_guest() and remove this allowance.
#![allow(deprecated)]
#![cfg(target_os = "linux")]

use divan::Bencher;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    UdpPacket, UdpRepr,
};
use void_box::network::slirp::{
    SlirpBackend, GATEWAY_MAC, GUEST_MAC, SLIRP_DNS_IP, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
};

fn main() {
    divan::main();
}

fn build_syn(src_port: u16, dst_port: u16) -> Vec<u8> {
    let tcp = TcpRepr {
        src_port,
        dst_port,
        control: TcpControl::Syn,
        seq_number: smoltcp::wire::TcpSeqNumber(1000),
        ack_number: None,
        window_len: 65535,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        payload: &[],
    };
    let ip = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: SLIRP_GATEWAY_IP,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = 14 + ip.buffer_len() + tcp.buffer_len();
    let mut buf = vec![0u8; total];
    let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut e);
    let mut ipp = Ipv4Packet::new_unchecked(&mut buf[14..]);
    ip.emit(&mut ipp, &Default::default());
    let mut tcpp = TcpPacket::new_unchecked(&mut buf[14 + ip.buffer_len()..]);
    tcp.emit(
        &mut tcpp,
        &IpAddress::Ipv4(SLIRP_GUEST_IP),
        &IpAddress::Ipv4(SLIRP_GATEWAY_IP),
        &Default::default(),
    );
    buf
}

#[divan::bench]
fn process_syn(bencher: Bencher) {
    let frame = build_syn(49152, 1);
    bencher.bench_local(|| {
        let mut stack = SlirpBackend::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&frame));
    });
}

#[divan::bench]
fn poll_idle(bencher: Bencher) {
    let mut stack = SlirpBackend::new().unwrap();
    bencher.bench_local(|| {
        let _ = divan::black_box(&mut stack).poll();
    });
}

#[divan::bench]
fn process_arp_request(bencher: Bencher) {
    let arp_repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(GUEST_MAC),
        source_protocol_addr: SLIRP_GUEST_IP,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: SLIRP_GATEWAY_IP,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress([0xff; 6]),
        ethertype: EthernetProtocol::Arp,
    };
    let total = 14 + arp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut e);
    let mut a = ArpPacket::new_unchecked(&mut buf[14..]);
    arp_repr.emit(&mut a);

    bencher.bench_local(|| {
        let mut stack = SlirpBackend::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&buf));
    });
}

/// Open `n` distinct guest→gateway flows, then time `poll()`.
///
/// Each iteration builds `n` SYN frames with unique source ports and feeds
/// them into a single [`SlirpBackend`], producing up to `n` NAT table entries.
/// `process_guest_frame` errors are ignored — the goal is "many NAT entries",
/// not "all connections succeed" (the default rate-limit may drop some).
///
/// The timed section is a single `poll()` call on the pre-populated stack,
/// so the measurement reflects the NAT-walk cost at that table size.
/// Today the walk is `O(n)`; the unified flow table planned for Phase 4
/// should keep the same asymptotic complexity but with smaller constants.
#[divan::bench(args = [1, 100, 1000])]
fn poll_with_n_flows(bencher: Bencher, n: usize) {
    let mut stack = SlirpBackend::new().unwrap();
    for i in 0..n {
        let frame = build_syn(49152u16.wrapping_add(i as u16), 1);
        let _ = stack.process_guest_frame(&frame);
    }
    bencher.bench_local(|| {
        let _ = divan::black_box(&mut stack).poll();
    });
}

/// Builds a minimal DNS A-query Ethernet frame from the guest to [`SLIRP_DNS_IP`].
///
/// `xid` is placed in the DNS transaction-ID field. The question section
/// queries `example.com` for an A record. The frame is a complete Ethernet →
/// IPv4 → UDP → DNS wire encoding suitable for passing to
/// [`SlirpBackend::process_guest_frame`].
fn build_dns_query_for_bench(xid: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&xid.to_be_bytes());
    // flags: RD=1; QDCOUNT=1; ANCOUNT/NSCOUNT/ARCOUNT = 0
    payload.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    // QNAME: \x07example\x03com\x00
    payload.extend_from_slice(b"\x07example\x03com\x00");
    // QTYPE=A (1), QCLASS=IN (1)
    payload.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);

    let udp_repr = UdpRepr {
        src_port: 49152,
        dst_port: 53,
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: SLIRP_DNS_IP,
        next_header: IpProtocol::Udp,
        payload_len: 8 + payload.len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = 14 + ip_repr.buffer_len() + 8 + payload.len();
    let mut buf = vec![0u8; total];
    let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut e);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut udp = UdpPacket::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
    udp_repr.emit(
        &mut udp,
        &IpAddress::Ipv4(SLIRP_GUEST_IP),
        &IpAddress::Ipv4(SLIRP_DNS_IP),
        payload.len(),
        |b| b.copy_from_slice(&payload),
        &Default::default(),
    );
    buf
}

/// Times the stack's DNS processing path when the cache has no entry for the
/// queried name.
///
/// Each iteration creates a fresh [`SlirpBackend`] (so the DNS cache is empty)
/// and processes one DNS query frame. The measurement captures stack
/// initialisation plus first-query cache-miss handling, giving a baseline for
/// the cold-cache cost.
#[divan::bench]
fn dns_cache_miss(bencher: Bencher) {
    let frame = build_dns_query_for_bench(1);
    bencher.bench_local(|| {
        let mut stack = SlirpBackend::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&frame));
    });
}

/// Times the stack's DNS processing path when a cache entry already exists for
/// the queried name.
///
/// Before the timed section, one query is injected and the stack is polled
/// for up to one second to allow the upstream DNS response to populate the
/// cache. The timed section then processes a second query (different XID,
/// same name) on the warm stack, isolating the cache-hit fast path.
#[divan::bench]
fn dns_cache_hit(bencher: Bencher) {
    let mut stack = SlirpBackend::new().unwrap();
    let warm = build_dns_query_for_bench(1);
    let _ = stack.process_guest_frame(&warm);
    for _ in 0..20 {
        let _ = stack.poll();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let hit = build_dns_query_for_bench(2);
    bencher.bench_local(|| {
        let _ = divan::black_box(&mut stack).process_guest_frame(divan::black_box(&hit));
    });
}
