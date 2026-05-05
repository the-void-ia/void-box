# Phase 6.3: TCP Window Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development.

**Goal:** Stop ignoring the guest's advertised window and stop hardcoding our advertised window. Track per-flow guest window (with scaling), advertise our own window from the host kernel's actual recv-buffer headroom, and negotiate `window_scale` on the SYN/SYN-ACK exchange.

**Severity:** Medium — perf gap. The current code emits `window_len: 65535, window_scale: None` on every outgoing frame, never reads `tcp.window_len()` from incoming guest frames, and never honors guest backpressure on host→guest send. Effect: throughput is capped at 64 KB / RTT regardless of available bandwidth, and `inject_to_guest` can grow unbounded if the guest is slow.

**Architecture:** Add `guest_window: u32` and `guest_window_scale: u8` fields to `TcpNatEntry`. Read window updates from incoming guest packets; respect them when deciding how much to send via `frames_to_inject`. Negotiate `window_scale: Some(7)` (128× scale, passt's default) on the SYN-ACK we emit. On every outgoing frame, advertise a window derived from `getsockopt(TCP_INFO).tcpi_rcv_space` so the guest sees real backpressure.

**Tech stack:** smoltcp 0.11 wire types (already in use). `libc::getsockopt(..., TCP_INFO, ...)` for kernel rcv-space. No new crates.

---

## Background

Three things are wrong today:

1. `src/network/slirp.rs:93` — `const TCP_WINDOW: u16 = 65535`. Hardcoded.
2. `build_tcp_packet_static` at `src/network/slirp.rs:2811-2827` — emits `window_len: TCP_WINDOW` and `window_scale: None` on every frame. Never reads anything from the host kernel.
3. The guest-frame parser in `handle_tcp_frame` never reads `tcp.window_len()` from the incoming `TcpRepr`. The guest's advertised window is silently discarded; we treat the guest as having infinite buffer.

The 256 KB host→guest cap that Phase 3 fixed (`tcp_writes_more_than_256kb_succeed`) was a userspace-side band-aid for the symptom. With proper window honoring, host→guest is bounded by the guest's *actual* receive buffer, which is normally far larger than 256 KB on a modern Linux kernel guest with `tcp_window_scaling=1`.

passt's `tcp_conn` ([passt/tcp_conn.h:21](https://passt.top/passt/tree/tcp_conn.h#n21)) tracks `wnd_from_tap`, `wnd_to_tap`, scale factors, and updates ACK/window per [tcp.c:1021](https://passt.top/passt/tree/tcp.c#n1021), [tcp.c:1426](https://passt.top/passt/tree/tcp.c#n1426).

## Invariants (carried)

1. All-Rust path. `libc::getsockopt` is fine.
2. Full observability — log scale negotiation in trace; log window updates at debug.
3. Cross-platform discipline.
4. No regression in Phase 0–5 + 5.5b + 6.4 + listener-on-epoll + 6.1 + 6.2 baselines.
5. Snapshot/restore correctness — new fields need `#[serde(default)]` for backward compatibility with pre-6.3 snapshots; default to scale=0 / window=65535 (current behavior).

---

## File impact

| File | Action |
|---|---|
| `src/network/slirp.rs` | `TcpNatEntry` adds `guest_window: u32`, `guest_window_scale: u8`. `build_tcp_packet_static` signature changes to take a `(window_len, window_scale)` pair. `handle_tcp_frame` reads window updates. SYN/SYN-ACK paths negotiate scale. `relay_tcp_nat_data` gates host→guest sends on `entry.guest_window`. |
| `tests/network_baseline.rs` | Two new pins. |
| `benches/network.rs` | One new bench: `tcp_bulk_throughput_constrained_window` (parametric on guest window size). |

---

## Tasks

### Task 1: Add `guest_window` + `guest_window_scale` fields, default-init

In `src/network/slirp.rs`, extend `TcpNatEntry`:

```rust
struct TcpNatEntry {
    // ... existing fields ...
    /// Guest's advertised receive window in bytes, scaled per
    /// `guest_window_scale`. Updated on every incoming TCP frame's
    /// `window_len`. Initial value 65535 matches an unscaled SYN.
    guest_window: u32,
    /// Window-scale shift the guest negotiated in its SYN. Zero
    /// means "guest does not support window scaling" (or we did not
    /// see a window-scale option in the SYN).
    guest_window_scale: u8,
}
```

Initialize at every `TcpNatEntry { ... }` literal site:
- `handle_tcp_frame` SYN handler (the existing `Connecting`/`SynReceived` paths)
- `process_pending_inbound_accepts`
- `insert_synthetic_synsent_entry` (bench-helpers)
- `insert_synthetic_lastack_entry` (bench-helpers)
- `insert_synthetic_connecting_entry` (bench-helpers, just added by 6.2)

Initial values: `guest_window: 65535, guest_window_scale: 0` (no-op default).

Snapshot serde: `#[serde(default)]` on each new field. Run `cargo check`.

**Commit:** `feat(slirp): TcpNatEntry tracks guest_window + guest_window_scale`

---

### Task 2: Read window scale from guest SYN; track per-flow

When the guest sends SYN (in `handle_tcp_frame`'s SYN flow), parse the TCP options for `WindowScale`. smoltcp's `TcpRepr` exposes `window_scale: Option<u8>`. Stash it in the entry:

```rust
let window_scale = tcp.window_scale().unwrap_or(0);
// ... in the entry literal ...
guest_window_scale: window_scale,
guest_window: u32::from(tcp.window_len()) << window_scale,
```

For the SYN-ACK we emit back to the guest, advertise our own scale (suggest 7 = 128×, matching passt). This requires changing `build_tcp_packet_static`'s signature — see Task 4.

Run `cargo check` (no semantic change yet — we're just stashing values).

**Commit:** `feat(slirp): parse guest's window_scale on SYN, store on flow`

---

### Task 3: Update `guest_window` on every incoming guest packet

In `handle_tcp_frame`, after locating the `entry` and after the existing `entry.last_activity = Instant::now()` line, update window:

```rust
entry.guest_window = u32::from(tcp.window_len()) << entry.guest_window_scale;
```

This runs for every frame the guest sends — data, pure ACK, FIN, RST. Always reflects the most recent advertised window.

`cargo check`. No tests yet.

**Commit:** `feat(slirp): track guest's advertised window on every incoming frame`

---

### Task 4: Change `build_tcp_packet_static` to take `(window_len, window_scale)`

Currently:

```rust
fn build_tcp_packet_static(
    src_ip, dst_ip, src_port, dst_port, seq, ack, control, payload,
) -> Vec<u8> {
    // ... uses TCP_WINDOW + None internally
}
```

Change signature to add explicit window parameters:

```rust
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
        // ...
        window_len,
        window_scale,
        // ...
    };
    // ... unchanged below ...
}
```

Search-replace every call site to thread the new arguments. The call sites split into three cases:

- **SYN-ACK** (in the SYN handler): pass `(65535, Some(OUR_WINDOW_SCALE))` where `OUR_WINDOW_SCALE = 7`.
- **All other frames** (data ACKs, plain ACKs, FIN-ACK, RST): pass `(advertised_window, None)`. `advertised_window` is computed from the host kernel via `host_recv_window(entry.host_stream.as_raw_fd())` — a new helper added in Task 5.

For Task 4 specifically: get the signature change landed and pass `(65535, None)` everywhere except the SYN-ACK site, which passes `(65535, Some(7))`. Subsequent tasks adapt the value.

Add a module-level constant near `TCP_WINDOW`:

```rust
/// Window-scale shift we advertise on SYN-ACK frames. Matches passt's
/// default. 7 means each unit in `window_len` represents 128 bytes,
/// extending the effective window from 64 KiB to 8 MiB.
const OUR_WINDOW_SCALE: u8 = 7;
```

Run `cargo check && cargo test --test network_baseline`. Expected: 22/22 still pass — no behavior change beyond scale negotiation.

**Commit:** `refactor(slirp): build_tcp_packet_static takes (window_len, window_scale)`

---

### Task 5: `host_recv_window` helper + use it on outgoing frames

New helper that reads `TCP_INFO.tcpi_rcv_space` from the host kernel:

```rust
/// Host kernel's current receive-buffer headroom, scaled down by
/// `OUR_WINDOW_SCALE`, for advertising as our `window_len` on
/// outgoing frames.  Returns `65535 / 2` (a conservative middle
/// value) on getsockopt failure rather than 0 (which would stall
/// the connection) or `u16::MAX` (which would overcommit).
fn host_recv_window(fd: RawFd) -> u16 {
    use std::mem::MaybeUninit;
    let mut info: MaybeUninit<libc::tcp_info> = MaybeUninit::zeroed();
    let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
    // SAFETY: getsockopt fills `info` if it returns 0.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            info.as_mut_ptr() as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return 32768;
    }
    let info = unsafe { info.assume_init() };
    let scaled = info.tcpi_rcv_space >> OUR_WINDOW_SCALE;
    scaled.min(u32::from(u16::MAX)) as u16
}
```

`libc::tcp_info` and `libc::TCP_INFO` are stable in the libc crate.

Update every `build_tcp_packet_static` call site that passes `(65535, None)` to instead pass `(host_recv_window(entry.host_stream.as_raw_fd()), None)`. The SYN-ACK site keeps `(65535, Some(OUR_WINDOW_SCALE))`.

Doc-comment the trade: a fresh socket has `tcpi_rcv_space` pre-filled to ~32 KiB; under load it grows to 4 MiB+ on Linux. Scaled by `>> 7`, that gives 256 KiB advertised. Both extremes are within `u16::MAX`.

Run baseline + bulk-throughput bench. Expected: no regression on `tcp_bulk_throughput_1mb`; possibly slight improvement.

**Commit:** `feat(slirp): advertise host-kernel-derived window on outgoing frames`

---

### Task 6: Failing pin — `tcp_advertised_window_tracks_guest_buffer`

Synthesize a guest with a small advertised window (4096 bytes, no scale), push 64 KB of data from host, assert `inject_to_guest` never holds more than ~4 KB of un-acknowledged bytes. Today the test fails because we ignore the guest's window.

```rust
#[test]
fn tcp_advertised_window_tracks_guest_buffer() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || -> std::net::TcpStream {
        let (sock, _) = listener.accept().unwrap();
        sock
    });

    let mut stack = SlirpBackend::new().unwrap();

    let our_seq = 1000u32;
    // Guest SYN with explicit small window (no scale).
    let syn = build_tcp_frame_with_window(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port,
        our_seq, 0, TcpControl::Syn, &[],
        4096, None,
    );
    stack.process_guest_frame(&syn).unwrap();

    let mut gateway_seq = 0u32;
    for f in drain_n(&mut stack, 4) {
        if let Some((s, _, ctrl, _)) = parse_tcp_to_guest(f.as_slice()) {
            if matches!(ctrl, TcpControl::Syn) { gateway_seq = s; break; }
        }
    }

    // Complete handshake with the same small window.
    stack.process_guest_frame(&build_tcp_frame_with_window(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port,
        our_seq + 1, gateway_seq + 1, TcpControl::None, &[],
        4096, None,
    )).unwrap();

    let mut host_stream = server.join().unwrap();
    let payload = vec![0xAB; 64 * 1024];
    host_stream.write_all(&payload).unwrap();

    // Drive drain_to_guest a few times. With proper window tracking,
    // total bytes injected before any ACK should be <= guest_window
    // (4096 plus a small slop for one MTU-sized segment).
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

fn build_tcp_frame_with_window(
    dst_ip: Ipv4Address, src_port: u16, dst_port: u16,
    seq: u32, ack: u32, control: TcpControl, payload: &[u8],
    window_len: u16, window_scale: Option<u8>,
) -> Vec<u8> {
    // Same shape as build_tcp_frame but plumbs window_len/scale.
    // ...
}
```

Run: should FAIL pre-Task-7 — `relay_tcp_nat_data` doesn't gate on `entry.guest_window` yet.

**Commit:** `test(network): pin tcp_advertised_window_tracks_guest_buffer (BROKEN_ON_PURPOSE)`

---

### Task 7: Gate host→guest sends on `entry.guest_window`

In `relay_tcp_nat_data`, where we currently push frames into `frames_to_inject` for the relay's data path, compute the un-ACKed-bytes-in-flight and STOP sending when it would exceed `entry.guest_window`:

```rust
// Before pushing a new payload chunk:
let in_flight = entry.bytes_in_flight as usize;
let window_remaining = (entry.guest_window as usize).saturating_sub(in_flight);
if window_remaining == 0 {
    // Guest window is full; stop sending until guest ACKs.
    trace!(
        "SLIRP TCP: guest window exhausted on flow guest_port={} (in_flight={}, window={})",
        key.guest_src_port, in_flight, entry.guest_window
    );
    break;
}
let chunk_size = chunk.len().min(window_remaining);
let chunk = &chunk[..chunk_size];
```

The `bytes_in_flight` field already exists from Phase 3 — use it.

Run the pin from Task 6: should now PASS.

**Commit:** `feat(slirp): gate host→guest send on guest's advertised window`

---

### Task 8: Failing pin — `tcp_window_scale_negotiated_in_synack`

```rust
#[test]
fn tcp_window_scale_negotiated_in_synack() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();
    let mut stack = SlirpBackend::new().unwrap();

    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port,
        1000, 0, TcpControl::Syn, &[],
    )).unwrap();

    let mut saw_synack_with_scale = false;
    for f in drain_n(&mut stack, 4) {
        let eth = EthernetFrame::new_unchecked(f.as_slice());
        if eth.ethertype() != EthernetProtocol::Ipv4 { continue; }
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        if ip.next_header() != IpProtocol::Tcp || ip.dst_addr() != SLIRP_GUEST_IP {
            continue;
        }
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        if tcp.syn() && tcp.ack() {
            // smoltcp's TcpPacket exposes options via .options() — look for WS.
            for option in TcpOption::parse_from(tcp.options()) {
                if let TcpOption::WindowScale(scale) = option {
                    assert_eq!(scale, 7, "advertised scale must be OUR_WINDOW_SCALE");
                    saw_synack_with_scale = true;
                }
            }
        }
    }
    assert!(saw_synack_with_scale, "SYN-ACK must include WindowScale option");
}
```

Run: should PASS post-Task-4 (we already advertise scale in SYN-ACK).

**Commit:** `test(network): pin tcp_window_scale_negotiated_in_synack`

---

### Task 9: Bench `tcp_bulk_throughput_constrained_window` (parametric)

Mirrors `tcp_bulk_throughput_1mb` but parametrizes the guest's advertised window. Pre-Phase-6.3 throughput should be ~bandwidth-delay-product limited at small windows; post-6.3 it should scale.

```rust
#[divan::bench(args = [4096, 16384, 65536])]
fn tcp_bulk_throughput_constrained_window(bencher: Bencher, guest_window: u32) {
    // ... same harness shape as tcp_bulk_throughput_1mb but uses
    //     build_tcp_frame_with_window to negotiate `guest_window`.
}
```

Documents the win numerically.

**Commit:** `bench(network): tcp_bulk_throughput_constrained_window parametric`

---

### Task 10: Phase 6.3 validation gate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --test network_baseline                                            # 24/24
cargo test --test network_baseline --features bench-helpers -- --test-threads=1   # 26/26
cargo test --lib network                                                      # 23/23+
cargo bench --bench network --features bench-helpers --no-run
cargo build --release
```

Wall-clock sanity:
```bash
voidbox-network-bench --iterations 3 --bulk-mb 10
# g2h ≥ 5.5 Gbps (likely improves with scale negotiation), CRR ~10 ms, RR ~2 µs
```

`bench-compare.sh --baseline 47868f0 --skip-vm`:
- `tcp_bulk_throughput_constrained_window/4096` should NOT regress vs the older "ignore window" path (we now respect it; throughput at 4 KB window is bandwidth-limited but correctness is right).
- `tcp_bulk_throughput_1mb` should be ≤ baseline.
- All other benches no regression.

---

## Out of scope

- `TCP_FASTOPEN`, `SACK`, `TIMESTAMPS` — separate phases or never.
- Dynamic scale renegotiation post-handshake (impossible in TCP) — handled correctly today by sticking with the scale we set in SYN-ACK.

## Reviewer pointers

- Verify `host_recv_window` returns sane numbers under load. Add a bench-helpers method `synthetic_advertised_window(fd)` if it's hard to inspect.
- Verify `bytes_in_flight` tracking is still consistent — the `in_flight` calculation in Task 7 reuses the Phase 3 field; if anyone broke it, the pin from Task 6 would be flaky.
- Snapshot interaction: pre-6.3 entries default to `guest_window: 65535, guest_window_scale: 0` via `#[serde(default)]`. That's the same behavior as before this phase. Verify with snapshot_integration if env is available.

## Document history

- 2026-05-05: initial plan written.
