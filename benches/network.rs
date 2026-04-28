//! Divan micro-benchmarks for SLIRP hot paths.
//!
//! Mirrors `benches/startup.rs` in shape. Job: regression detection
//! for the per-packet hot path on the vCPU and net-poll threads.
//!
//! Run with: `cargo bench --bench network`

#![cfg(target_os = "linux")]

use divan::Bencher;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
};
use void_box::network::slirp::{
    SlirpStack, GATEWAY_MAC, GUEST_MAC, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
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
        let mut stack = SlirpStack::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&frame));
    });
}

#[divan::bench]
fn poll_idle(bencher: Bencher) {
    let mut stack = SlirpStack::new().unwrap();
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
        let mut stack = SlirpStack::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&buf));
    });
}
