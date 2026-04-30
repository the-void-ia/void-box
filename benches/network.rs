//! Divan micro-benchmarks for SLIRP hot paths.
//!
//! Mirrors `benches/startup.rs` in shape. Job: regression detection
//! for the per-packet hot path on the vCPU and net-poll threads.
//!
//! Run with: `cargo bench --bench network`

// TODO(0D.5): migrate poll() → drain_to_guest() and remove this allowance.
#![allow(deprecated)]

#[cfg(target_os = "linux")]
use divan::{counter::BytesCount, Bencher};
#[cfg(target_os = "linux")]
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    UdpPacket, UdpRepr,
};
#[cfg(target_os = "linux")]
use void_box::network::slirp::{
    SlirpBackend, GATEWAY_MAC, GUEST_MAC, SLIRP_DNS_IP, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
};

fn main() {
    // SLIRP-using benches are Linux-only (smoltcp dep is `cfg(target_os =
    // "linux")` in Cargo.toml). On other platforms, `divan::main()` runs
    // with zero registered benches and exits 0 — that's the right shape
    // for cross-platform CI which runs `cargo bench --no-run` to compile-
    // check the bench binary.
    #[cfg(target_os = "linux")]
    divan::main();
    #[cfg(not(target_os = "linux"))]
    eprintln!("benches/network.rs: SLIRP benches are Linux-only; nothing to run here");
}

// All bench functions and helpers below are Linux-only (depend on smoltcp
// + the SLIRP backend, which are themselves `cfg(target_os = "linux")`
// in the workspace Cargo.toml). Wrapping in a module keeps the cfg gating
// in one place; on macOS the module compiles to nothing and `main()` above
// short-circuits before any of these are referenced.
#[cfg(target_os = "linux")]
mod linux_benches {
    use super::*;

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

    /// Measures TCP bulk throughput through the SLIRP relay under backpressure.
    ///
    /// Pushes 1 MiB through the relay in 1 KiB chunks with a constrained host
    /// receiver (`SO_RCVBUF=4096`) so the post-Phase-3 backpressure path is
    /// exercised every iteration. Divan reports throughput in MB/s alongside
    /// per-iteration latency, giving a numerical regression signal for the
    /// passt-style sequence-mirroring + don't-ACK-on-EAGAIN backpressure path.
    ///
    /// The 95% delivery threshold mirrors `tcp_writes_more_than_256kb_succeed`
    /// — the binary contract test for Phase 3.
    #[divan::bench(sample_count = 10)]
    fn tcp_bulk_throughput_1mb(bencher: Bencher) {
        use smoltcp::wire::TcpControl;
        use std::io::Read;
        use std::os::unix::io::AsRawFd;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        const TOTAL_BYTES: usize = 1024 * 1024;
        const CHUNK_BYTES: usize = 1024;
        const WINDOW_MAX: u32 = 256 * 1024;
        const DEADLINE_SECS: u64 = 5;
        const GUEST_SRC_PORT: u16 = 49200;
        const INITIAL_GUEST_SEQ: u32 = 1000;

        bencher
            .counter(BytesCount::new(TOTAL_BYTES as u64))
            .bench_local(|| {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let host_port = listener.local_addr().unwrap().port();

                unsafe {
                    let rcvbuf: libc::c_int = 4096;
                    libc::setsockopt(
                        listener.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        &rcvbuf as *const libc::c_int as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }

                let bytes_received = Arc::new(AtomicUsize::new(0));
                let bytes_received_thr = Arc::clone(&bytes_received);
                let server = std::thread::spawn(move || {
                    let (mut sock, _) = listener.accept().unwrap();
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                bytes_received_thr.fetch_add(bytes_read, Ordering::Relaxed);
                            }
                            Err(_) => break,
                        }
                    }
                });

                let mut stack = SlirpBackend::new().unwrap();

                let syn = build_tcp_data_frame(
                    SLIRP_GATEWAY_IP,
                    GUEST_SRC_PORT,
                    host_port,
                    INITIAL_GUEST_SEQ,
                    0,
                    TcpControl::Syn,
                    &[],
                );
                stack.process_guest_frame(&syn).unwrap();

                let synack_frames: Vec<Vec<u8>> = {
                    let mut frames = Vec::new();
                    for _ in 0..4 {
                        frames.extend(stack.poll());
                    }
                    frames
                };
                let (gateway_seq, _, _, _) = synack_frames
                    .iter()
                    .find_map(|frame| parse_tcp_to_guest_frame(frame))
                    .expect("synack");

                let ack_frame = build_tcp_data_frame(
                    SLIRP_GATEWAY_IP,
                    GUEST_SRC_PORT,
                    host_port,
                    INITIAL_GUEST_SEQ + 1,
                    gateway_seq + 1,
                    TcpControl::None,
                    &[],
                );
                stack.process_guest_frame(&ack_frame).unwrap();

                let chunk = vec![b'x'; CHUNK_BYTES];
                let mut guest_seq = INITIAL_GUEST_SEQ + 1;
                let mut acked_seq = INITIAL_GUEST_SEQ + 1;
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(DEADLINE_SECS);

                while bytes_received.load(Ordering::Relaxed) < TOTAL_BYTES * 95 / 100
                    && std::time::Instant::now() < deadline
                {
                    let data_frame = build_tcp_data_frame(
                        SLIRP_GATEWAY_IP,
                        GUEST_SRC_PORT,
                        host_port,
                        guest_seq,
                        gateway_seq + 1,
                        TcpControl::Psh,
                        &chunk,
                    );
                    let _ = stack.process_guest_frame(&data_frame);
                    guest_seq = guest_seq.wrapping_add(CHUNK_BYTES as u32);

                    for frame in {
                        let mut frames = Vec::new();
                        for _ in 0..4 {
                            frames.extend(stack.poll());
                        }
                        frames
                    } {
                        if let Some((_, ack, _, _)) = parse_tcp_to_guest_frame(&frame) {
                            if ack > acked_seq {
                                acked_seq = ack;
                            }
                        }
                    }

                    if guest_seq.wrapping_sub(acked_seq) > WINDOW_MAX {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }

                let fin_frame = build_tcp_data_frame(
                    SLIRP_GATEWAY_IP,
                    GUEST_SRC_PORT,
                    host_port,
                    guest_seq,
                    gateway_seq + 1,
                    TcpControl::Fin,
                    &[],
                );
                let _ = stack.process_guest_frame(&fin_frame);
                for _ in 0..40 {
                    let _ = stack.poll();
                    if server.is_finished() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let _ = server.join();

                divan::black_box(bytes_received.load(Ordering::Relaxed));
            });
    }

    /// Builds a minimal IPv4-over-Ethernet TCP segment from guest to gateway.
    ///
    /// Returns the full Ethernet frame bytes. Mirrors the `build_tcp_frame`
    /// helper from `tests/network_baseline.rs` inline so the bench compiles
    /// as a standalone binary without a shared helper crate.
    fn build_tcp_data_frame(
        dst_ip: smoltcp::wire::Ipv4Address,
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack: u32,
        control: TcpControl,
        payload: &[u8],
    ) -> Vec<u8> {
        use smoltcp::wire::{IpAddress, TcpSeqNumber};

        let tcp_repr = TcpRepr {
            src_port,
            dst_port,
            control,
            seq_number: TcpSeqNumber(seq as i32),
            ack_number: if ack == 0 {
                None
            } else {
                Some(TcpSeqNumber(ack as i32))
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
        let eth_hdr_len = 14usize;
        let total = eth_hdr_len + ip_repr.buffer_len() + tcp_repr.buffer_len();
        let mut buf = vec![0u8; total];
        let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
        eth_repr.emit(&mut eth);
        let mut ip = Ipv4Packet::new_unchecked(&mut buf[eth_hdr_len..]);
        ip_repr.emit(&mut ip, &Default::default());
        let mut tcp = TcpPacket::new_unchecked(&mut buf[eth_hdr_len + ip_repr.buffer_len()..]);
        tcp_repr.emit(
            &mut tcp,
            &IpAddress::Ipv4(SLIRP_GUEST_IP),
            &IpAddress::Ipv4(dst_ip),
            &Default::default(),
        );
        buf
    }

    /// Parses one frame emitted by the stack as a TCP segment directed to the guest.
    ///
    /// Returns `(seq, ack, control, payload_len)` on success, `None` otherwise.
    fn parse_tcp_to_guest_frame(frame: &[u8]) -> Option<(u32, u32, TcpControl, usize)> {
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
            control,
            tcp.payload().len(),
        ))
    }
} // mod linux_benches
