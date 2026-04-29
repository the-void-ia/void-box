# Phase 3 Implementation Plan: TCP Relay Rewrite (MSG_PEEK + sequence mirroring)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Mandatory skills for every Rust-touching task:**
> `rust-style`, `rustdoc`, `rust-analyzer-ssr`,
> `superpowers:test-driven-development`,
> `superpowers:verification-before-completion`. Use LSP for navigation.
>
> **THIS IS THE HIGH-RISK PHASE.** The TCP relay (~625 LOC at
> `src/network/slirp.rs:82–1048`) is the most fragile path in the
> project. The `tcp_to_host_buffer_drops_at_256kb` test pin is the
> headline assertion to flip. `snapshot_integration` and the
> conformance suite are the safety net — every task ends with both
> green or it doesn't land.

**Spec:** [`2026-04-27-smoltcp-passt-port.md`](2026-04-27-smoltcp-passt-port.md)
**Continues from Phase 2:** [`2026-04-27-smoltcp-passt-port-phase2.md`](2026-04-27-smoltcp-passt-port-phase2.md)

**Goal:** Replace the hand-rolled TCP relay's `to_guest: Vec<u8>` and
`to_host: Vec<u8>` user-space buffers with passt-style sequence
mirroring (host kernel's TCP socket buffer IS the buffer). Eliminate
the 256 KB `to_host` cliff and drop 100s of LOC of fragile state.

**Architecture:** For each direction:

- **host → guest** (host writes, we relay to guest): instead of
  `read()` into `to_guest: Vec<u8>` then drain, use
  `recv(MSG_PEEK)` to inspect what's in the kernel socket without
  consuming it. Send the un-acknowledged portion as TCP segments to
  the guest. Track `bytes_in_flight = our_seq - last_acked_seq`.
  When the guest ACKs, `recv()` (no MSG_PEEK) the ACK'd bytes to
  advance the kernel's read pointer. The kernel's socket buffer
  absorbs backpressure naturally.

- **guest → host** (guest writes, we relay to host): on guest
  segment, attempt non-blocking `send()` on the host socket. If it
  succeeds: ACK the guest. If `WouldBlock` (kernel send buffer full):
  **don't** ACK; let the guest retransmit (TCP's natural backpressure).
  Drop the 256 KB `to_host: Vec<u8>` user-space buffer entirely.

**Tech Stack:** Rust 1.88, `std::net::TcpStream` (already in use).
`libc::recv` with `MSG_PEEK` flag for the host→guest direction
(std doesn't expose MSG_PEEK on `TcpStream`).

**Branch:** `smoltcp-passt-port-phase0` (continuing on the same branch
through all phases — user instruction).

---

## Task structure

8 tasks across three workstreams.

| ID | Workstream | Scope |
|---|---|---|
| 3.1 | impl | Add sequence-mirroring fields to `TcpNatEntry`; default-init alongside existing buffers |
| 3.2 | impl | Add `recv_peek` helper using `libc::recv(MSG_PEEK)` |
| 3.3 | impl | Replace host→guest path: drain via peek, send `bytes_available - bytes_in_flight` |
| 3.4 | impl | Replace guest-ACK handling: consume ACK'd bytes from kernel, send next chunk |
| 3.5 | impl | Drop guest→host `to_host` buffer; rely on kernel send buffer + don't-ACK-on-EAGAIN backpressure |
| 3.6 | impl | Drop `to_guest`, `MAX_TO_HOST_BUFFER`, dead helpers; cleanup |
| 3.7 | test | Flip `tcp_to_host_buffer_drops_at_256kb` BROKEN_ON_PURPOSE pin |
| 3.8 | gate | Phase 3 validation gate (full conformance + snapshot suites + bench) |

---

## Workstream 3A — Add scaffolding (no behavior change)

### Task 3.1: Sequence-mirroring fields on `TcpNatEntry`

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add fields** to `TcpNatEntry` (around line 107 — LSP `documentSymbol` will surface). Add at the end of the struct:

```rust
/// passt-style sequence mirroring: bytes the kernel has buffered
/// past our last consumed point but not yet sent to guest. With
/// MSG_PEEK, we can inspect the kernel's recv queue without
/// consuming, then `recv` (no peek) the ACK'd portion later.
///
/// `bytes_in_flight = our_seq - last_acked_seq` — bytes sent to
/// guest but not yet ACK'd.
#[allow(dead_code)] // consumed in 3.3
bytes_in_flight: u32,
```

`our_seq` and `guest_ack` already exist on the struct. Reuse them; don't introduce new aliases.

- [ ] **Step 2: Initialize** in every construction site of `TcpNatEntry` (LSP `findReferences` on the struct will list them — likely 1–2 sites in `handle_tcp_frame`'s SYN branch). Add `bytes_in_flight: 0,` to each.

- [ ] **Step 3: Verify.**

```bash
cargo check
cargo test --test network_baseline   # 14 tests still pass
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): add bytes_in_flight to TcpNatEntry (no behavior change)"
```

---

### Task 3.2: `recv_peek` helper

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add a module-scope helper.**

```rust
/// Non-blocking `recv(MSG_PEEK)` on a `TcpStream`, returning bytes
/// read without consuming them from the kernel socket buffer.
///
/// `std::net::TcpStream` does not expose `MSG_PEEK`; we go through
/// `libc::recv` directly.
fn recv_peek(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: `stream` outlives the syscall; `buf` is uniquely
    // borrowed and `len` matches.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}
```

`std::os::fd::AsRawFd` is already in the module-scope use block (added in Phase 1.1). `MSG_DONTWAIT` ensures non-blocking even if the stream's `set_nonblocking` flag is dropped somehow.

- [ ] **Step 2: Verify** the helper compiles. No callers yet:

```bash
cargo check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): add recv_peek helper using libc::recv MSG_PEEK"
```

---

## Workstream 3B — The actual relay rewrite

### Task 3.3: Replace host→guest path with peek-based send

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Locate** the host→guest section in `relay_tcp_nat_data`
  via LSP `documentSymbol`. It's the `read` block around lines
  991–1025: read up to 16 KB into `entry.to_guest`, drain `to_guest`
  in MTU-sized chunks, build TCP packets, increment `our_seq`.

- [ ] **Step 2: Replace** that block with a peek-based version. The
  new logic:

```rust
// Host → guest, peek-based sequence-mirroring.
// We don't `read()` into a userspace buffer — the kernel's socket
// buffer holds outstanding data until the guest ACKs, at which point
// Task 3.4 consumes the ACK'd portion via plain `recv()`.
let mut peek_buf = [0u8; 65536];
match recv_peek(&entry.host_stream, &mut peek_buf) {
    Ok(0) => {
        // EOF from host. Send FIN to guest if we haven't already.
        // (FIN handling continues to use the existing block below.)
        entry.state = TcpNatState::Closed;
    }
    Ok(n) => {
        // Send only the un-ACK'd portion: skip what's already in flight.
        let bytes_in_flight = entry.bytes_in_flight as usize;
        if n > bytes_in_flight {
            let new_payload = &peek_buf[bytes_in_flight..n];
            for chunk in new_payload.chunks(MTU - 54) {
                let frame = build_tcp_packet_static(
                    /* ... existing args, payload=chunk, seq=entry.our_seq ... */
                );
                self.inject_to_guest.push(frame);
                entry.our_seq = entry.our_seq.wrapping_add(chunk.len() as u32);
                entry.bytes_in_flight =
                    entry.bytes_in_flight.wrapping_add(chunk.len() as u32);
            }
        }
        // else: everything in the kernel buffer is already in flight;
        // wait for guest to ACK before sending more.
    }
    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
        // Nothing in the kernel buffer yet; nothing to do.
    }
    Err(_) => {
        entry.state = TcpNatState::Closed;
    }
}
```

The exact builder call must match the existing `build_tcp_packet_static` signature — read the current call site and copy verbatim.

- [ ] **Step 3: Run.**

```bash
cargo check
cargo test --test network_baseline   # tcp_data_round_trip MUST pass; the 256KB cliff test still passes (cliff still in place via to_host path which 3.5 will remove)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The `tcp_to_host_buffer_drops_at_256kb` BROKEN_ON_PURPOSE pin tests the **guest→host** direction — it should still pass after this task because we haven't touched that path yet (3.5 owns it).

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "refactor(slirp): peek-based host→guest TCP relay (drops to_guest buffer dependency)"
```

> Note: the `to_guest: Vec<u8>` field is now unused but still on the
> struct. Task 3.6 removes it; until then it stays so the diff per
> task is reviewable.

---

### Task 3.4: ACK handling — consume ACK'd bytes from kernel

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Locate** guest-ACK handling. In `handle_tcp_frame`,
  the ACK branch (around line 855–870) currently advances
  `entry.guest_ack` and may transition state. With peek-based send,
  on each ACK we must also `recv()` (no peek) the ACK'd bytes from
  the kernel socket so the kernel can free them.

- [ ] **Step 2: Compute ACK'd bytes** from the incoming TCP segment's
  ACK number minus the entry's last-known `guest_ack`. Use wrapping
  arithmetic — TCP sequence numbers wrap at 2³².

```rust
let segment_ack = /* ... extract from TcpRepr ... */;
let acked_bytes = segment_ack.wrapping_sub(entry.guest_ack);
// Advance the recorded ack point.
if acked_bytes > 0 && acked_bytes <= entry.bytes_in_flight {
    let mut sink = [0u8; 65536];
    let mut remaining = acked_bytes as usize;
    while remaining > 0 {
        let want = remaining.min(sink.len());
        match entry.host_stream.read(&mut sink[..want]) {
            Ok(0) | Err(_) => break, // EOF or error; let next iteration handle it
            Ok(n) => remaining -= n,
        }
    }
    entry.bytes_in_flight =
        entry.bytes_in_flight.wrapping_sub(acked_bytes - remaining as u32);
    entry.guest_ack = segment_ack;
}
```

The `read()` call (not `recv` directly) consumes from the kernel buffer — equivalent on a non-blocking `TcpStream`. The `entry.host_stream` is already non-blocking, so this won't stall.

- [ ] **Step 3: Test the round trip.** `tcp_data_round_trip` should
  still pass — guest sends 5 bytes, host echoes, guest receives. The
  echo path now uses peek + ACK-driven consume.

```bash
cargo test --test network_baseline tcp_data_round_trip
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): consume ACK'd bytes from kernel on guest ACK"
```

---

### Task 3.5: Drop guest→host `to_host` buffer (kill the 256 KB cliff)

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Locate** the `to_host` write path. In `handle_tcp_frame`
  (around lines 867–911) and `relay_tcp_nat_data` (around lines
  960–989), the current code:
  - Writes guest payload to `entry.host_stream` directly when
    `to_host` is empty.
  - Buffers in `entry.to_host` on `WouldBlock`.
  - Drops the connection when `to_host` exceeds `MAX_TO_HOST_BUFFER`
    (256 KB).
  - Sends ACK on successful write OR sets `to_host_pending_ack` when
    the write was buffered.

- [ ] **Step 2: Replace** with a strict don't-ACK-on-EAGAIN approach:
  - Attempt non-blocking `write` on the host socket.
  - On full success: ACK the guest immediately.
  - On partial success (some bytes written): ACK only those bytes;
    let the guest retransmit the rest.
  - On `WouldBlock` with zero bytes written: **don't ACK**; let the
    guest retransmit per TCP's natural backpressure. The kernel's
    send buffer fills up; when it drains, the next guest retransmit
    succeeds.

```rust
// In handle_tcp_frame's data branch:
let payload = /* ... existing extract ... */;
let n_written = match entry.host_stream.write(payload) {
    Ok(n) => n,
    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => 0,
    Err(_) => {
        entry.state = TcpNatState::Closed;
        return Ok(());
    }
};
if n_written > 0 {
    let ack_seq = segment_seq.wrapping_add(n_written as u32);
    self.send_ack(entry, ack_seq);
    entry.guest_seq = ack_seq;
}
// else: silently drop the segment; guest retransmits.
```

- [ ] **Step 3: Remove the `MAX_TO_HOST_BUFFER` constant** and the
  256 KB-cliff branch. The cliff is gone — TCP backpressure handles
  it naturally.

- [ ] **Step 4: Verify.**

```bash
cargo check
cargo test --test network_baseline   # tcp_data_round_trip still passes
# tcp_to_host_buffer_drops_at_256kb is EXPECTED TO FAIL now —
# Task 3.7 will flip it. For this task, run with --no-fail-fast and
# confirm only that test fails.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): drop to_host buffer + 256KB cliff, use TCP backpressure"
```

---

### Task 3.6: Cleanup — drop unused fields + dead helpers

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Remove unused fields** from `TcpNatEntry`:
  - `to_guest: Vec<u8>` — replaced by peek-based send.
  - `to_host: Vec<u8>` — replaced by kernel send buffer + retransmit.
  - `to_host_pending_ack: Option<u32>` — replaced by direct ACK on
    successful write.

- [ ] **Step 2: Remove dead helpers** that referenced them. Use LSP
  `findReferences` on each removed field to find call sites; remove
  the helpers if they're now orphaned.

- [ ] **Step 3: Update doc comments** — the file-level doc and the
  `TcpNatEntry` doc should reflect the new design.

- [ ] **Step 4: Verify.**

```bash
cargo check
cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): drop to_guest/to_host/pending_ack fields and dead helpers"
```

---

## Workstream 3C — Test + validation

### Task 3.7: Flip `tcp_to_host_buffer_drops_at_256kb` BROKEN_ON_PURPOSE pin

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Locate** the test. It currently asserts that pushing
  ~300 KB closes the connection.

- [ ] **Step 2: Rewrite** to assert the OPPOSITE — pushing >256 KB
  succeeds with no connection close. Rename to
  `tcp_writes_more_than_256kb_succeed`. The test:
  - Bind a host TCP server that accepts and reads ~1 MB.
  - Drive the handshake.
  - Push 1 MB in chunks.
  - Assert no `Rst` / `Fin` arrives at the guest mid-stream.
  - Assert the host server receives all 1 MB.

- [ ] **Step 3: Run.**

```bash
cargo test --test network_baseline tcp_writes_more_than_256kb_succeed
cargo test --test network_baseline    # 14 tests pass
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add tests/network_baseline.rs
git commit -m "test(network): flip 256KB cliff pin — assert >1MB succeeds"
```

---

### Task 3.8: Phase 3 validation gate

**Files:** none (gate only)

- [ ] **Static checks**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Unit + baseline tests**

```bash
cargo test --workspace --all-features
cargo test --test network_baseline
```

- [ ] **Conformance + snapshot integration suites — the safety net**

```bash
export VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test snapshot_integration -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
```

These exercise real TCP traffic through the SLIRP path. **Any
regression here is a Phase 3 blocker.**

- [ ] **Microbench regression check**

```bash
cargo bench --bench network
```

Compare `process_syn`, `poll_idle`, `poll_with_n_flows` against the
Phase 2 baseline. No regression > 10%.

- [ ] **Wall-clock harness**

```bash
./target/release/voidbox-network-bench --iterations 3 \
  --output /tmp/baseline-network-phase3.json
cat /tmp/baseline-network-phase3.json
```

Expected:
- `tcp_throughput_g2h_mbps`: comparable to Phase 2 (~1900 Mbps).
- `tcp_rr_latency_us_p50`: comparable (~2 µs).
- `tcp_crr_latency_us_p50`: **expected to drop** — the new TCP relay
  has fewer per-segment ACK round-trips. From Phase 2's ~10,160 µs
  toward something closer to passt's 135 µs. Anywhere meaningfully
  below 5,000 µs is a clear win.

- [ ] **Startup bench warm-restore** (the bench fixed in 0d0ab20)
  must continue to pass:

```bash
./target/release/voidbox-startup-bench --iters 3 --breakdown
# warm phase exits 0
```

No PR opened — paused per user instruction.

---

## Risks

- **Highest-risk phase by far.** The TCP relay rewrite is ~400 LOC
  replaced. Any subtle bug in the sequence math (off-by-one,
  unsigned wrap, ACK-vs-segment-seq confusion) silently breaks
  long-running connections. The conformance + snapshot suites are
  the safety net.
- **Sequence wrap arithmetic.** TCP seq numbers are 32-bit and wrap
  at 2³². Use `wrapping_add` / `wrapping_sub` everywhere. A naive
  comparison at boundaries is silently wrong.
- **MSG_PEEK + non-blocking + multi-thread.** `recv_peek` is called
  from the net-poll thread. The host socket is non-blocking. Confirm
  no other code path closes the socket concurrently.
- **Window-scaling not implemented.** Today's `TCP_WINDOW = 65535`
  hardcoded. We don't claim window scaling in SYN-ACK options.
  Acceptable for Phase 3 — passt-grade window negotiation is deferred.
- **TCP_INFO not used.** passt queries `TCP_INFO` on the host socket
  to mirror RTT/window. We don't. Connections work without it; window
  semantics are slightly different. Out of scope here.

## File impact

| File | Approximate LOC |
|---|---|
| `src/network/slirp.rs` | **~+250 / −350** (net reduction) |
| `tests/network_baseline.rs` | ~+50 / −60 (rewrite the cliff test) |
| **Total** | **~+300 / −410** |

Net reduction in `slirp.rs` is the headline win. Less code, fewer
fragile invariants, kernel does the buffering.
