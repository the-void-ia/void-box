//! Divan micro-benchmarks for SLIRP hot paths.
//!
//! Mirrors `benches/startup.rs` in shape. Job: regression detection
//! for the per-packet hot path on the vCPU and net-poll threads.
//!
//! Run with: `cargo bench --bench network`

#[cfg(target_os = "linux")]
use divan::{counter::BytesCount, Bencher};
#[cfg(target_os = "linux")]
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr,
    TcpControl, TcpPacket, TcpRepr, UdpPacket, UdpRepr,
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
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

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

    /// Time `SlirpBackend::process_guest_frame` for a single UDP datagram.
    ///
    /// Mirrors `process_syn` shape: build the frame once outside the timed
    /// loop, fresh stack per iteration. Establishes UDP per-frame cost
    /// for cross-phase regression detection.
    #[divan::bench]
    fn process_udp_frame(bencher: Bencher) {
        let frame = build_udp_frame_for_bench(49152, 8080, b"x");
        bencher.bench_local(|| {
            let mut stack = SlirpBackend::new().unwrap();
            let _ = stack.process_guest_frame(divan::black_box(&frame));
        });
    }

    /// Time `SlirpBackend::process_guest_frame` for a single ICMP echo
    /// request. Note: a fresh stack means the unprivileged ICMP socket is
    /// opened on every iteration, so this measures the full
    /// `open_icmp_socket + insert + send_to` path. If the host's
    /// `net.ipv4.ping_group_range` excludes the calling GID, the underlying
    /// `socket()` call returns EACCES and `process_guest_frame` returns Ok
    /// without touching `flow_table` — divan's measurement still completes
    /// but `flow_table` stays empty. That's fine for regression detection.
    #[divan::bench]
    fn process_icmp_echo_request(bencher: Bencher) {
        let frame = build_icmp_echo_for_bench(0xbeef, 1);
        bencher.bench_local(|| {
            let mut stack = SlirpBackend::new().unwrap();
            let _ = stack.process_guest_frame(divan::black_box(&frame));
        });
    }

    #[divan::bench]
    fn poll_idle(bencher: Bencher) {
        let mut stack = SlirpBackend::new().unwrap();
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(8);
        bencher.bench_local(|| {
            out.clear();
            divan::black_box(&mut stack).drain_to_guest(&mut out);
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
    /// Today the walk is `O(n)`; the unified flow table keeps the same
    /// asymptotic complexity but with smaller per-entry constants.
    #[divan::bench(args = [1, 100, 1000])]
    fn poll_with_n_flows(bencher: Bencher, n: usize) {
        let mut stack = SlirpBackend::new().unwrap();
        for i in 0..n {
            let frame = build_syn(49152u16.wrapping_add(i as u16), 1);
            let _ = stack.process_guest_frame(&frame);
        }
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(8);
        bencher.bench_local(|| {
            out.clear();
            divan::black_box(&mut stack).drain_to_guest(&mut out);
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
        let mut out: Vec<Vec<u8>> = Vec::new();
        for _ in 0..20 {
            out.clear();
            stack.drain_to_guest(&mut out);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let hit = build_dns_query_for_bench(2);
        bencher.bench_local(|| {
            let _ = divan::black_box(&mut stack).process_guest_frame(divan::black_box(&hit));
        });
    }

    /// Pure-compute bench for `nat::translate_outbound`. Baseline for future
    /// hasher / data-structure changes (e.g. moving deny_cidrs from
    /// `Vec<Ipv4Net>` to a longest-prefix trie). Tens of nanoseconds
    /// expected; microseconds would indicate an allocation in the hot path.
    #[divan::bench]
    fn nat_translate_outbound_hot_path(bencher: Bencher) {
        use void_box::network::nat::{translate_outbound, Rules};

        let rules = Rules {
            gateway_loopback: true,
            deny_cidrs: vec!["169.254.0.0/16".parse().unwrap()],
            port_forwards: vec![],
        };
        let dst = SLIRP_GATEWAY_IP;
        let gateway = SLIRP_GATEWAY_IP;

        bencher.bench_local(|| {
            divan::black_box(translate_outbound(
                divan::black_box(&rules),
                divan::black_box(dst),
                divan::black_box(80),
                divan::black_box(gateway),
            ));
        });
    }

    /// Measures TCP bulk throughput through the SLIRP relay under backpressure.
    ///
    /// Pushes 1 MiB through the relay in 1 KiB chunks with a constrained host
    /// receiver (`SO_RCVBUF=4096`) so the backpressure path is exercised every
    /// iteration. Divan reports throughput in MB/s alongside per-iteration
    /// latency, giving a numerical regression signal for the passt-style
    /// sequence-mirroring + don't-ACK-on-EAGAIN backpressure path.
    ///
    /// The 95% delivery threshold mirrors `tcp_writes_more_than_256kb_succeed`
    /// — the binary contract test for TCP backpressure correctness.
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
                        stack.drain_to_guest(&mut frames);
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

                    let mut frames = Vec::new();
                    for _ in 0..4 {
                        stack.drain_to_guest(&mut frames);
                    }
                    for frame in frames {
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
                let mut fin_drain: Vec<Vec<u8>> = Vec::new();
                for _ in 0..40 {
                    fin_drain.clear();
                    stack.drain_to_guest(&mut fin_drain);
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
    fn build_udp_frame_for_bench(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let udp_repr = UdpRepr { src_port, dst_port };
        let ip_repr = Ipv4Repr {
            src_addr: SLIRP_GUEST_IP,
            dst_addr: SLIRP_GATEWAY_IP,
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
            &IpAddress::Ipv4(SLIRP_GATEWAY_IP),
            payload.len(),
            |b| b.copy_from_slice(payload),
            &Default::default(),
        );
        buf
    }

    fn build_icmp_echo_for_bench(ident: u16, seq_no: u16) -> Vec<u8> {
        let icmp_repr = Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data: b"bench",
        };
        let ip_repr = Ipv4Repr {
            src_addr: SLIRP_GUEST_IP,
            dst_addr: smoltcp::wire::Ipv4Address::new(8, 8, 8, 8),
            next_header: IpProtocol::Icmp,
            payload_len: icmp_repr.buffer_len(),
            hop_limit: 64,
        };
        let eth = EthernetRepr {
            src_addr: EthernetAddress(GUEST_MAC),
            dst_addr: EthernetAddress(GATEWAY_MAC),
            ethertype: EthernetProtocol::Ipv4,
        };
        let total = 14 + ip_repr.buffer_len() + icmp_repr.buffer_len();
        let mut buf = vec![0u8; total];
        let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
        eth.emit(&mut e);
        let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
        ip_repr.emit(&mut ip, &Default::default());
        let mut icmp = Icmpv4Packet::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
        icmp_repr.emit(&mut icmp, &Default::default());
        buf
    }

    /// Open `n/3` TCP + `n/3` UDP + `n/3` ICMP-echo flows, then time `poll()`.
    ///
    /// Mirrors `poll_with_n_flows` (TCP-only) but exercises the unified
    /// `flow_table` with all three protocols populated. Catches enum-dispatch
    /// and filter regressions at scale: each `relay_*_data` loop filters
    /// by `FlowKey` variant over the unified table, so per-protocol scan cost
    /// is `O(total_flows)` not `O(this_protocol's_flows)`. This bench is the
    /// regression gate for that property.
    #[divan::bench(args = [3, 99, 999])]
    fn poll_with_n_mixed_flows(bencher: Bencher, n: usize) {
        let mut stack = SlirpBackend::new().unwrap();
        let third = n / 3;

        // n/3 TCP SYNs.
        for i in 0..third {
            let frame = build_syn(49152u16.wrapping_add(i as u16), 1);
            let _ = stack.process_guest_frame(&frame);
        }
        // n/3 UDP datagrams (any non-DNS port; one byte payload).
        for i in 0..third {
            let frame = build_udp_frame_for_bench(50152u16.wrapping_add(i as u16), 8080, b"x");
            let _ = stack.process_guest_frame(&frame);
        }
        // n/3 ICMP echoes (unique guest_id per flow).
        for i in 0..third {
            let frame = build_icmp_echo_for_bench(0x1000 + i as u16, 1);
            let _ = stack.process_guest_frame(&frame);
        }

        let mut out: Vec<Vec<u8>> = Vec::with_capacity(8);
        bencher.bench_local(|| {
            out.clear();
            divan::black_box(&mut stack).drain_to_guest(&mut out);
        });
    }

    /// Insert + remove `n` flow-table entries using synthetic data.
    ///
    /// Pure-compute baseline for the unified `HashMap<FlowKey, FlowEntry>`.
    /// Reference number for hasher experiments (foldhash, ahash, SipHash)
    /// or container-shape changes (e.g. hashbrown raw API). Uses synthetic
    /// `u32` values instead of real
    /// `TcpNatEntry` (which requires TcpStream) to isolate HashMap
    /// mechanics from socket cloning overhead — the real cost is
    /// HashMap insert/remove, not socket ops.
    ///
    /// Pre-builds N unique keys with different `guest_src_port` values
    /// (maintaining the same semantic as real flows), then times one
    /// iteration of insert all + remove all.
    #[divan::bench(args = [10, 100, 1000])]
    fn flow_table_insert_remove(bencher: Bencher, n: usize) {
        use std::collections::HashMap;

        // Build keys outside the timed loop.
        // Each key has a unique guest_src_port to simulate distinct flows.
        let keys: Vec<_> = (0..n)
            .map(|i| {
                smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                    10,
                    0,
                    2,
                    2 + (i % 254) as u8,
                ))
            })
            .collect();

        bencher.bench_local(|| {
            let mut table: HashMap<usize, u32> = HashMap::with_capacity(n);
            // Insert phase
            for (i, _key) in keys.iter().enumerate() {
                table.insert(i, i as u32);
            }
            // Remove phase
            for i in 0..n {
                divan::black_box(table.remove(&i));
            }
        });
    }
    /// Build a SYN-ACK Ethernet frame from the guest toward the gateway.
    ///
    /// src = GUEST_IP:guest_port, dst = GATEWAY_IP:high_port
    /// control = Syn, ack_number = Some(our_seq + 1) → produces SYN+ACK on wire.
    #[cfg(feature = "bench-helpers")]
    fn build_inbound_syn_ack_frame(
        guest_port: u16,
        high_port: u16,
        our_seq: u32,
        guest_seq: u32,
    ) -> Vec<u8> {
        use smoltcp::wire::TcpSeqNumber;

        let tcp_repr = TcpRepr {
            src_port: guest_port,
            dst_port: high_port,
            control: TcpControl::Syn,
            seq_number: TcpSeqNumber(guest_seq as i32),
            ack_number: Some(TcpSeqNumber(our_seq.wrapping_add(1) as i32)),
            window_len: 65535,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            payload: &[],
        };
        let ip_repr = Ipv4Repr {
            src_addr: SLIRP_GUEST_IP,
            dst_addr: SLIRP_GATEWAY_IP,
            next_header: IpProtocol::Tcp,
            payload_len: tcp_repr.buffer_len(),
            hop_limit: 64,
        };
        let eth_repr = EthernetRepr {
            src_addr: EthernetAddress(GUEST_MAC),
            dst_addr: EthernetAddress(GATEWAY_MAC),
            ethertype: EthernetProtocol::Ipv4,
        };
        let total = 14 + ip_repr.buffer_len() + tcp_repr.buffer_len();
        let mut buf = vec![0u8; total];
        let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
        eth_repr.emit(&mut eth);
        let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
        ip_repr.emit(&mut ip, &Default::default());
        let mut tcp = TcpPacket::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
        tcp_repr.emit(
            &mut tcp,
            &IpAddress::Ipv4(SLIRP_GUEST_IP),
            &IpAddress::Ipv4(SLIRP_GATEWAY_IP),
            &Default::default(),
        );
        buf
    }

    /// Seed a `SynSent` entry into `stack`'s flow table.
    ///
    /// Replicates `SlirpBackend::insert_synthetic_synsent_entry` inline.
    /// Requires the `bench-helpers` feature (compile with
    /// `cargo bench --features bench-helpers`).
    #[cfg(feature = "bench-helpers")]
    fn seed_synsent_entry(stack: &mut SlirpBackend, guest_port: u16, high_port: u16, our_seq: u32) {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let host_stream =
            TcpStream::connect(listener.local_addr().unwrap()).expect("connect loopback");
        host_stream.set_nonblocking(true).ok();
        stack.insert_synthetic_synsent_entry(guest_port, high_port, our_seq, host_stream);
    }

    /// Microbench for the inbound SYN-ACK state-machine transition added in
    /// 5.5b.1 (`TcpNatState::SynSent` → `Established`). Each iteration
    /// (re)builds a `SlirpBackend`, seeds one `SynSent` entry, feeds a
    /// synthetic guest SYN-ACK frame to `process_guest_frame`, and lets
    /// the bench timer capture the `process_guest_frame` cost.
    ///
    /// Expected magnitude: tens of µs (same order as `process_syn`, which
    /// also rebuilds a fresh stack per iteration).
    #[cfg(feature = "bench-helpers")]
    #[divan::bench]
    fn tcp_inbound_syn_ack_transition(bencher: Bencher) {
        const GUEST_PORT: u16 = 8080;
        const HIGH_PORT: u16 = 49152;
        const OUR_SEQ: u32 = 1000;
        const GUEST_SEQ: u32 = 42;

        let frame = build_inbound_syn_ack_frame(GUEST_PORT, HIGH_PORT, OUR_SEQ, GUEST_SEQ);

        bencher.bench_local(|| {
            let mut stack = SlirpBackend::new().unwrap();
            seed_synsent_entry(&mut stack, GUEST_PORT, HIGH_PORT, OUR_SEQ);
            let _ = divan::black_box(&mut stack).process_guest_frame(divan::black_box(&frame));
        });
    }

    /// Pure-compute cost of synthesizing an inbound SYN frame for
    /// port-forwarding. No stack allocation or guest frame processing —
    /// just the `build_tcp_packet_static` wire encoding.
    ///
    /// Expected magnitude: sub-microsecond (pure packet construction).
    ///
    /// Requires the `bench-helpers` feature (compile with
    /// `cargo bench --features bench-helpers`).
    #[cfg(feature = "bench-helpers")]
    #[divan::bench]
    fn synthesize_inbound_syn(bencher: Bencher) {
        const HIGH_PORT: u16 = 49152;
        const GUEST_PORT: u16 = 8080;
        const OUR_SEQ: u32 = 1000;

        bencher.bench_local(|| {
            divan::black_box(void_box::network::slirp::synthesize_inbound_syn(
                divan::black_box(HIGH_PORT),
                divan::black_box(GUEST_PORT),
                divan::black_box(OUR_SEQ),
            ));
        });
    }

    /// Returns `true` if `frame` is an Ethernet/IPv4/TCP packet with the SYN
    /// flag set, addressed to `dst_port`.
    ///
    /// The synthesized inbound SYN produced by `synthesize_inbound_syn` uses
    /// `TcpControl::Syn` but smoltcp sets the ACK bit whenever `ack_number`
    /// is `Some(...)`, even when the value is zero.  Checking only `tcp.syn()`
    /// + `dst_port` is therefore correct here.
    fn is_tcp_syn_to_port(frame: &[u8], dst_port: u16) -> bool {
        // Minimum: 14 (Eth) + 20 (IPv4) + 20 (TCP) = 54 bytes.
        if frame.len() < 54 {
            return false;
        }
        let eth = EthernetFrame::new_unchecked(frame);
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_unchecked(eth.payload());
        if ip.next_header() != IpProtocol::Tcp {
            return false;
        }
        let ip_header_len = ip.header_len() as usize;
        let tcp = TcpPacket::new_unchecked(&eth.payload()[ip_header_len..]);
        tcp.syn() && tcp.dst_port() == dst_port
    }

    /// Wall-clock latency of the full inbound port-forward path: host
    /// `TcpStream::connect` → listener thread `accept()` (polled every
    /// `PORT_FORWARD_POLL_INTERVAL = 50 ms`) → mpsc channel push →
    /// `process_pending_inbound_accepts` → `synthesize_inbound_syn` →
    /// first SYN frame visible in `drain_to_guest` output.
    ///
    /// The 50 ms polling ceiling means the distribution will be roughly
    /// uniform on [0, 50 ms] — a median around 25 ms is expected and normal,
    /// not a bug. Regressions in the inbound state machine or the listener
    /// poll loop will shift the distribution upward beyond 50 ms.
    ///
    /// Regressions in the inbound state machine or listener-poll loop will
    /// surface numerically against this measurement.
    #[divan::bench(sample_count = 20, sample_size = 1)]
    fn port_forward_accept_latency(bencher: Bencher) {
        const GUEST_PORT: u16 = 8080;
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
        const DRAIN_POLL: Duration = Duration::from_micros(100);

        // Probe-bind to grab an ephemeral host port, then release the listener
        // so SlirpBackend can bind it.  There is an inherent TOCTOU race
        // between the drop and the SlirpBackend bind — acceptable for benches
        // running on a loopback interface under controlled conditions.
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind for host port");
        let host_port = probe.local_addr().expect("probe local_addr").port();
        drop(probe);

        let mut stack = SlirpBackend::with_security(
            64,
            50,
            &["169.254.0.0/16".to_string()],
            &[(host_port, GUEST_PORT)],
        )
        .expect("SlirpBackend::with_security");

        let mut out: Vec<Vec<u8>> = Vec::new();

        bencher.bench_local(|| {
            // Spawn a worker thread that connects to the host listener port.
            // The listener thread inside SlirpBackend will accept() it on the
            // next poll (within PORT_FORWARD_POLL_INTERVAL = 50ms) and push
            // the accepted stream onto the mpsc channel.
            let connect_addr = format!("127.0.0.1:{host_port}");
            let worker = thread::spawn(move || {
                let addr: std::net::SocketAddr = connect_addr.parse().expect("parse connect addr");
                std::net::TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
                    .expect("connect to listener");
            });

            // Poll drain_to_guest until a SYN frame appears in the output.
            loop {
                out.clear();
                stack.drain_to_guest(&mut out);
                if out
                    .iter()
                    .any(|frame| is_tcp_syn_to_port(frame, GUEST_PORT))
                {
                    break;
                }
                thread::sleep(DRAIN_POLL);
            }

            worker.join().expect("worker thread panicked");
        });
    }

    /// Cost of one `drain_to_guest` call when one TCP flow is `Established`
    /// and the host kernel has data ready to relay.
    ///
    /// Captures the per-packet SLIRP dispatch overhead via epoll: epoll_wait
    /// (non-blocking, zero-timeout), readiness scan, peek, and Ethernet frame
    /// construction. Only the flows with data ready are dispatched — flows
    /// with nothing to relay are skipped.
    ///
    /// This bench cannot exercise the `net_poll_thread` 50 ms epoll cycle
    /// (that thread does not run inside divan).  The wall-clock latency floor
    /// is captured separately by `voidbox-network-bench`'s `tcp_rx_latency_us_p50`
    /// field; see that binary's `Report` struct for the measurement shape.
    ///
    /// Requires the `bench-helpers` feature (compile with
    /// `cargo bench --features bench-helpers`).
    #[cfg(feature = "bench-helpers")]
    #[divan::bench(sample_count = 50, sample_size = 10)]
    fn tcp_rx_latency_one_packet(bencher: Bencher) {
        use smoltcp::wire::TcpControl;
        use std::io::Write;
        use std::net::TcpListener;

        const GUEST_SRC_PORT: u16 = 49155;
        const INITIAL_GUEST_SEQ: u32 = 5000;
        const PAYLOAD: &[u8] = &[0xAB; 64];

        // Build a fresh stack with one Established TCP flow.  Setup happens
        // outside the timed loop so divan only measures the relay dispatch.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host_port = listener.local_addr().unwrap().port();
        let server_thread = thread::spawn(move || listener.accept().unwrap());

        let mut stack = SlirpBackend::new().unwrap();

        // 3-way handshake: guest sends SYN → stack produces SYN-ACK → guest
        // sends ACK.  This mirrors `tcp_bulk_throughput_1mb` setup.
        let syn = build_tcp_syn_for_latency_bench(GUEST_SRC_PORT, host_port, INITIAL_GUEST_SEQ);
        stack.process_guest_frame(&syn).unwrap();

        // Drain for up to 200 ms to collect the SYN-ACK.
        let mut drain_frames: Vec<Vec<u8>> = Vec::new();
        let gateway_seq = {
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            loop {
                drain_frames.clear();
                stack.drain_to_guest(&mut drain_frames);
                if let Some((seq, _, _, _)) = drain_frames
                    .iter()
                    .find_map(|f| parse_tcp_to_guest_frame(f))
                {
                    break seq;
                }
                if std::time::Instant::now() > deadline {
                    panic!("no SYN-ACK within deadline");
                }
                thread::sleep(Duration::from_millis(5));
            }
        };

        // Complete the handshake: guest sends ACK.
        let ack = build_tcp_data_frame(
            SLIRP_GATEWAY_IP,
            GUEST_SRC_PORT,
            host_port,
            INITIAL_GUEST_SEQ + 1,
            gateway_seq + 1,
            TcpControl::None,
            &[],
        );
        stack.process_guest_frame(&ack).unwrap();

        // The server thread accepted the connection; grab the socket.
        let (mut server_sock, _) = server_thread.join().unwrap();
        server_sock
            .set_nonblocking(true)
            .expect("server non-blocking");

        // Set up state for the timed loop.
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(8);
        let guest_seq = INITIAL_GUEST_SEQ + 1;

        // Prime: put one payload in the kernel buffer before the first
        // iteration begins so the first measured call sees a ready event.
        let _ = server_sock.write(PAYLOAD);

        bencher.bench_local(|| {
            out.clear();
            // Refill the kernel buffer from the previous iteration's drain.
            // write() may return EAGAIN if the buffer is full; that is fine —
            // the previous iteration's peek left data in place.
            let _ = server_sock.write(divan::black_box(PAYLOAD));

            // The cost we are measuring: one non-blocking epoll_wait + relay.
            divan::black_box(&mut stack).drain_to_guest(&mut out);

            // Consume the relay output so inject_to_guest doesn't grow
            // unboundedly across iterations.
            divan::black_box(&out);

            // Keep the TCP stream happy: send an ACK for any data the relay
            // fed into inject_to_guest (frame content doesn't matter for the
            // bench; we just need the host stream not to stall).
            for frame in &out {
                if let Some((data_seq, _, _, plen)) = parse_tcp_to_guest_frame(frame) {
                    if plen > 0 {
                        let ack_back = build_tcp_data_frame(
                            SLIRP_GATEWAY_IP,
                            GUEST_SRC_PORT,
                            host_port,
                            guest_seq,
                            data_seq.wrapping_add(plen as u32),
                            TcpControl::None,
                            &[],
                        );
                        let _ = stack.process_guest_frame(&ack_back);
                    }
                }
            }
        });
    }

    /// Build a SYN frame from the guest toward the host for the latency bench.
    ///
    /// Identical to `build_tcp_data_frame` with `TcpControl::Syn` and zero
    /// `ack`.  Kept as a separate function to document intent: this is the
    /// opening segment of the 3-way handshake used by
    /// `tcp_rx_latency_one_packet`.
    #[cfg(feature = "bench-helpers")]
    fn build_tcp_syn_for_latency_bench(src_port: u16, dst_port: u16, seq: u32) -> Vec<u8> {
        build_tcp_data_frame(
            SLIRP_GATEWAY_IP,
            src_port,
            dst_port,
            seq,
            0,
            smoltcp::wire::TcpControl::Syn,
            &[],
        )
    }
} // mod linux_benches
