# Startup: next milestones — persistent control channel, kernel shrink, PVH boot

**Status:** Draft
**Date:** 2026-04-20 (revised after empirical validation of the
Milestone A work)
**Context:** follow-up to the Milestone A commits on
`feat/startup-milestone-a` (Lever 6 pre-warm handshake, Levers 8 + 4
defensive cap and guest-agent fast path).

## Key finding from Milestone A validation

Running Lever 6 against both the startup bench and the HN agent
revealed that `HANDSHAKE_READ_TIMEOUT` is overloaded — it controls
both the per-retry latency for a single handshake *and* the
concurrency ceiling of the whole userspace vsock worker. Different
workloads want opposite values.

### Measured 5 ms vs 150 ms tradeoff

Fedora 43 / KVM / slim kernel, 20-iter bench, `hackernews_agent.yaml`
validation:

| Config                                               | Bench warm p50 | Bench warm p99 | HN agent              |
|------------------------------------------------------|---------------:|---------------:|-----------------------|
| feat/perf baseline (5 ms, no Lever 6)                | 138 ms         | 148 ms         | **broken pipe flood** |
| feat/perf + Lever 6 (5 ms)                           | **82 ms**      | 89 ms          | broken pipe flood     |
| feat/perf + Lever 6 (150 ms, split warmup at 5 ms)   | 230 ms         | 235 ms         | **passes**            |
| feat/perf + Lever 6 (150 ms everywhere)              | 230 ms         | 235 ms         | passes                |

**Implication**: Lever 6's 82 ms warm win on the bench is attributable
mostly to the aggressive 5 ms per-retry timeout, not to the
pre-warm handshake itself. When 5 ms is relaxed to 150 ms (required
for agent-workload correctness under concurrent telemetry + RPCs),
the single-retry cost dominates the bench and the net warm number
regresses to ~230 ms.

Splitting the constant (`WARM_HANDSHAKE_READ_TIMEOUT = 5 ms` for the
Lever 6 background fire, `HANDSHAKE_READ_TIMEOUT = 150 ms` for every
other RPC path) doesn't help: the bench's first `exec()` still
pays the 150 ms retry when the guest-agent doesn't respond on the
very first try, and Lever 6's background warmup doesn't change
that — it only primes the guest's scheduler.

### Why

The userspace vsock worker (`src/devices/virtio_vsock_userspace.rs`)
is **single-threaded**. When an agent is running, the guest has:

- 1 long-lived telemetry connection streaming batches once per second
- N in-flight RPC connections (write_file + mkdir + exec + exec_response per tool call)

At 5 ms handshake timeout, if the worker is even briefly busy
servicing telemetry or another RPC when a new connection's Ping
arrives, that connection's Pong misses its 5 ms window and the host
closes + retries. Retries compound into broken-pipe floods faster
than the worker can drain.

At 150 ms, each retry waits long enough for the worker to cycle
through. No flood, but the bench pays the full 150 ms on any miss.

## What this changes about the plan

**Lever 7 (persistent control channel) is no longer a
"nice-to-have agent-workload win" — it is the prerequisite to
collect Lever 6's full value.** Once there is one long-lived
connection per `Sandbox` that multiplexes all RPCs, the handshake
retry loop happens exactly once (at connection birth, or at pre-warm
time). After that, every RPC is a framed request/response on the
open stream — no reconnect, no retry, no timeout ambiguity. `5 ms`
vs `150 ms` stops being a user-visible knob because the retry loop
only fires during initial connection establishment.

**Updated ordering:**

1. **Lever 7** (persistent control channel) — **prerequisite**, must
   land first. Removes the 5 ms / 150 ms tradeoff by removing the
   retry hot path from every RPC.
2. **Lever 1** (slim kernel shrink) — cold-path attack, unchanged.
3. **Lever 2** (PVH boot entry) — cold-path attack, unchanged.

With Lever 7 in place, Lever 6 can be revisited: the background
pre-warm becomes a single handshake on the persistent channel, the
warm p50 stabilises somewhere between 82 ms (ideal) and 140 ms
(safe) depending on `WARM_HANDSHAKE_READ_TIMEOUT`.

---

## Lever 7 — Persistent control channel (promoted to P0)

### Today (repeated for context)

Every `sandbox.exec()` / `write_file()` / `mkdir_p()` call does a fresh RPC:

```rust
tokio::spawn_blocking(|| {
    stream = connector()?;        // open NEW UnixStream to vsock device
    stream.write_all(Ping)?;      // handshake
    stream.read_exact(Pong)?;     //  ~1-2ms warm
    stream.write_all(Request)?;   // send payload
    stream.read_exact(Response)?; // wait for reply  ~1ms
    drop(stream);                 // close
});
```

Every RPC pays `connect + Ping/Pong + request/reply + close` ≈ **2–3 ms**
overhead on top of the actual work — and that's the *happy path*.
Under load the handshake retry cost dominates.

| Workload                                                                        | RPCs per run | Overhead today    | With persistent channel      |
|---------------------------------------------------------------------------------|-------------:|------------------:|------------------------------|
| Startup bench (`sh -c :`)                                                       |            1 |    3 ms           | 3 ms (first call handshakes) |
| HN agent (~15 tool calls, each `write_file + mkdir + exec + exec_response`)     |          ~70 |  170 ms at 5 ms timeout / broken pipes under agent concurrency | ~35 ms |
| Long Claude session (~50 tool calls)                                            |         ~250 |  600 ms steady / hangs on concurrency | ~120 ms |

### The fix

Open ONE long-lived connection per `Sandbox`. All requests multiplex
over it with a 4-byte `request_id` in the framing. The existing
`ExecOutputChunk` streaming already multiplexes streaming-vs-final
on one connection — we extend that pattern to all RPC types.

### Protocol sketch

Extend `void_box_protocol::Message` framing with a request_id in the
header:

```
[4 bytes: payload_len][1 byte: msg_type][4 bytes: request_id][payload...]
```

Host side:

- One `ControlChannel::stream` (Arc<Mutex<GuestStream>>).
- `ControlChannel` spawns a single reader task that demuxes incoming
  messages by `request_id` into pending oneshot channels.
- Every `send_exec_request` / `send_write_file` / etc. registers a
  oneshot, sends its request on the shared stream, waits on its
  oneshot.

Guest-agent side:

- One handler loop per accepted connection (the long-lived one).
- Dispatch by `msg_type` as today; copy `request_id` verbatim into
  the response frame.

### Concrete milestones

Split Lever 7 itself into three sub-milestones so it can land
incrementally rather than as a 1-2 week monolith:

**7a. Protocol + back-compat (3–4 days)**

- Bump `PROTOCOL_VERSION` in `void-box-protocol`.
- Extend `Message` to carry `request_id: u32`.
- Guest-agent: Pong now advertises a `supports_multiplex: bool` flag
  in the version payload.
- Host: if peer_version >= 2 and flag set, use multiplex path;
  otherwise fall back to per-RPC reconnect (today's behaviour).
- No behaviour change yet; both sides just speak a slightly-richer
  frame. Gates the rest of the work on version negotiation.

**7b. Persistent channel + demuxer (5–7 days)**

- Single `Arc<Mutex<Box<dyn GuestStream>>>` in `ControlChannel`.
- Dedicated reader task per channel, owning the read half, demuxing
  into `HashMap<request_id, oneshot::Sender<Message>>`.
- Rewrite `send_exec_request` / `send_write_file` / `send_mkdir_p` /
  `send_read_file` / `file_stat` / `subscribe_telemetry` to:
  1. allocate a `request_id`,
  2. register oneshot,
  3. write framed request,
  4. await oneshot.
- Error recovery: if the read task sees the stream close, fail all
  pending oneshots and mark the channel dead. Next RPC opens a new
  persistent channel.
- Keep the `warm_handshake` entry point: now it just forces the
  persistent channel to open and converge.

**7c. Remove retry-timeout tuning knob (1 day)**

- `HANDSHAKE_READ_TIMEOUT` stops being user-visible. It lives in one
  place (channel establishment) and can be set to an aggressive
  value again (5 ms) because agent concurrency no longer creates
  N in-flight handshakes.
- Expect: bench warm returns to ~82 ms, HN agent stays healthy
  (both workloads now share one connection).

### Validation per sub-milestone

7a: `cargo test`, `conformance`, HN agent on old guest-agent
(back-compat verified against the production Apr 14 image).

7b: add a `persistent_channel` integration test that issues 100
concurrent `exec` calls on one `Sandbox` and asserts all complete.
Re-run `conformance`, `oci_integration`, `e2e_mount`,
`snapshot_integration`, `e2e_agent_mcp` on the rebuilt image.

7c: bench (expect ~82 ms warm), HN agent (expect no broken-pipe
flood), openclaw_telegram (expect smoke_message posts).

### Why this is ordering change #1

Without Lever 7, we're forced to choose between bench-speed and
agent-correctness on every timeout knob. With Lever 7, handshake
tuning is a one-time cost on channel birth; the tradeoff disappears.

---

## Lever 1 — Shrink slim kernel further (3–5 days, 40–80 ms cold)

Unchanged from previous draft. Ordering moves to #2.

### Today

Our slim kernel = Firecracker's 6.1 microvm config + 7 CONFIGs we
added (9p, virtiofs, overlayfs, fuse, `VIRTIO_MMIO_CMDLINE_DEVICES`) +
upstream 6.12.30. Output: ~30 MB `vmlinux` with debug info.

Candidates to disable:

| Config                                                                | Why disable                                     | Saves                     |
|-----------------------------------------------------------------------|-------------------------------------------------|---------------------------|
| `CONFIG_DEBUG_INFO_*`                                                 | Debug-only; ship separate debug kernel          | ~5 MB binary, faster load |
| `CONFIG_AUDITSYSCALL`, `CONFIG_AUDIT_WATCH`                           | No auditd in our guest                          | initcalls, ~1–2 ms        |
| `CONFIG_MAGIC_SYSRQ`                                                  | No serial SysRq path needed                     | small                     |
| `CONFIG_SECURITY_SELINUX`, `CONFIG_SECURITY_APPARMOR`                 | Our guest-agent is unconstrained                | LSM hook overhead         |
| `CONFIG_SND_*`, `CONFIG_DRM_*`, `CONFIG_USB_*`, `CONFIG_INPUT_*` (non-PS/2) | Zero devices                                | many probe initcalls      |
| `CONFIG_HW_RANDOM_*` (keep only `CONFIG_RANDOM_TRUST_CPU`)            | RDRAND is enough                                | rng init                  |
| `CONFIG_BTRFS_FS`, `CONFIG_XFS_FS`, `CONFIG_F2FS_FS`, `CONFIG_JBD2`  | Only ext4/tmpfs/overlay/9p/virtiofs used        | fs module init            |
| `CONFIG_NETFILTER` excess tables                                      | SLIRP handles packet filtering host-side        | netfilter init            |

Kernel initcalls are ~100 ms today; aggressive trim targets 50–60 ms.

### Why it's 3–5 days

- **Iterate-bench-iterate**: disable a batch → rebuild slim → run all
  integration tests → if broken, re-enable the culprit.
- **Risk per disable**: non-obvious dependencies (e.g. dropping an
  LSM can change mount flags).
- Re-validate 4 integration suites + HN + openclaw each iteration.
- Pin the final minimal config so kernel bumps don't silently
  re-enable things.

---

## Lever 2 — PVH boot entry (2–3 days, 15–40 ms cold)

Unchanged from previous draft. Ordering moves to #3.

### Today

x86_64 boot path loads `vmlinux` ELF, puts vCPU into **64-bit long
mode** with pre-populated page tables, jumps to `e_entry = 0x1000123`
(kernel's `startup_64`). Works, but `startup_64` still does
real/protected-mode compatibility setup as if it were called from
bzImage — redundant work we pay for.

### PVH boot

Documented in `Documentation/x86/boot.rst`:

- Linux advertises a `XEN_ELFNOTE_PHYS32_ENTRY` note in its ELF
  program headers — the PVH entry point.
- VMM reads that note, builds an `hvm_start_info` struct (memory map
  + cmdline pointer).
- vCPU enters **32-bit protected mode** with `%ebx = &hvm_start_info`.
- Kernel's PVH entry skips the real-mode+16→32 transition stub
  entirely — lands ~100 lines deeper in `startup_32`.
- Faster path to `start_kernel()`.

Firecracker and cloud-hypervisor both use PVH. Reference loader
~200 lines of Rust.

### Implementation sketch

```rust
// src/vmm/arch/x86_64/boot.rs
fn parse_pvh_note(elf: &[u8]) -> Option<u64> {
    // Walk PT_NOTE segments, find XEN_ELFNOTE_PHYS32_ENTRY
}

fn build_hvm_start_info(cmdline_addr: u64, memory_map: &[E820Entry]) -> Vec<u8> {
    // Struct layout per Documentation/x86/boot.rst
}
```

Then in `load_kernel`: try PVH first (if note present), fall back to
the current ELF/bzImage path.

### Why it's 2–3 days

- Parse `PT_NOTE` section in ELF (easy, but needs tests).
- Build `hvm_start_info` struct (well-documented).
- Change vCPU setup to enter 32-bit protected mode with `%ebx`
  pointing at the struct.
- **Failure mode is silent triple-fault** — same failure class as
  the ELF entry-point bug we already fixed. Need `--console-file
  loglevel=7` during bringup.
- aarch64: different story (already uses a "raw Image + DTB" path).

---

## How they stack (updated ordering)

| Path                | Lever 7 (7a+7b+7c) | Lever 1       | Lever 2       | Total cold   | Total warm             |
|---------------------|---------------------|---------------|---------------|--------------|------------------------|
| Cold                | 0 ms                | −40 to −80 ms | −15 to −40 ms | 160–200 ms   | same                   |
| Warm startup-bench  | unlocks 5 ms timeout again → ~82 ms | 0 | 0 | same | **82 ms (recovered)**  |
| Warm agent RTT      | −140 ms (20 tools)  | 0             | 0             | same         | **−140 ms**            |
| Agent correctness   | fixes broken-pipe flood | n/a       | n/a           | n/a          | n/a                    |

## Ordering (final)

1. **Lever 7 (7a + 7b + 7c)** — mandatory prerequisite. Ships the
   persistent-channel architecture and unlocks every other warm-path
   optimisation.
2. **Lever 1** — slim kernel shrink. Quick, cold-path only, isolated
   risk.
3. **Lever 2** — PVH boot entry. Cold-path; requires loglevel=7
   bringup.

## Validation contract

Each lever must pass before landing:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-features` — no regressions
4. `voidbox-startup-bench --iters 20 --breakdown` — cold p95 ≤ 400 ms,
   warm p95 ≤ 200 ms (matching the `verify` skill gate)
5. `conformance`, `oci_integration`, `e2e_mount`,
   `snapshot_integration` — all green
6. HN agent (`examples/hackernews/hackernews_agent.yaml`) runs to
   completion
7. OpenClaw Telegram gateway
   (`examples/openclaw/openclaw_telegram.yaml`) — `smoke_message`
   step posts to Telegram

Lever 7 specifically must **fix** (6) and (7) as a regression
criterion — the current 5 ms config breaks them, the current 150 ms
config regresses the bench.

## Risk register

| Risk                                                                   | Mitigation                                                 |
|------------------------------------------------------------------------|------------------------------------------------------------|
| Lever 7 breaks back-compat with production Apr 14 guest-agent images  | 7a ships version negotiation first; fall back to per-RPC path when peer doesn't advertise multiplex support |
| Lever 7 reader task panic wedges all pending RPCs                     | Every oneshot has a timeout; reader-task death fails all pending futures with `Error::Guest("channel dead")` and triggers reconnect |
| In-flight exec streaming collides with multiplex request_id           | Reuse the exec's `request_id` on all its `ExecOutputChunk` frames; guest-agent never emits unsolicited frames |
| Telemetry subscription competes with RPCs on one channel              | Telemetry batches are bounded-size and low-rate (1/s); reader task is async and doesn't block demux |
| Guest-agent thread-per-connection deadlocks on one slow RPC           | With a single connection, only one handler thread exists — no cross-connection deadlock surface |

## References

- `docs/superpowers/plans/2026-04-19-startup-push-to-sub-100ms.md` —
  parent plan describing all eight levers
- `docs/architecture.md` §"Snapshots / Performance" — current
  published numbers
- Commits `12f9904` (Lever 6), `fc76caa` (Levers 8+4) on
  `feat/startup-milestone-a` — currently **not yet shipped** pending
  Lever 7
- Bench data in this session: `target/tmp/bench_pr45_fix.log`,
  `target/tmp/bench_split_rerun.log`, `target/tmp/hn_pr45*.log`
- Firecracker microvm configs:
  <https://github.com/firecracker-microvm/firecracker/tree/main/resources/guest_configs>
- PVH boot protocol: `Documentation/x86/boot.rst` in the Linux source
- cloud-hypervisor PVH loader:
  <https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/arch/src/x86_64/mod.rs>
