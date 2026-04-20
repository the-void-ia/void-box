# Startup: next milestones — kernel shrink, PVH boot, persistent control channel

**Status:** Draft
**Date:** 2026-04-20
**Context:** follow-up to the Milestone A landings on this branch
(`feat/startup-milestone-a`): Lever 6 (pre-warm vsock handshake) and
Levers 8 + 4 (defensive KVM HLT/PAUSE cap, guest-agent module-load
fast path). Measured warm p50 138 ms → 82 ms; cold held at ~256 ms.

Each of the three levers below attacks a distinct part of the
remaining startup timeline.

## Lever 7 — Persistent control channel (1–2 weeks, biggest agent-workload win)

### Today

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
overhead on top of the actual work. For the startup bench (one exec)
that's nothing. For agent workloads it compounds:

| Workload                                                                  | RPCs per run | Overhead today | With persistent channel     |
|---------------------------------------------------------------------------|-------------:|---------------:|-----------------------------|
| Startup bench (`sh -c :`)                                                 |            1 |           3 ms | 3 ms (first call handshakes) |
| HN agent (~15 tool calls, each `write_file + mkdir + exec + exec_response`) |          ~70 |         170 ms | ~35 ms                      |
| Long Claude session (~50 tool calls)                                      |         ~250 |         600 ms | ~120 ms                     |

### The fix

Open ONE long-lived connection per `Sandbox`. All requests multiplex
over it with a 4-byte `request_id` in the framing. The existing
`ExecOutputChunk` streaming already multiplexes streaming-vs-final on
one connection — we extend that pattern to all RPC types.

### Why it's 1–2 weeks

- **New protocol layer**: request IDs, response router, concurrent
  read-from-stream handling
- **Guest-agent side**: one handler that demuxes by `request_id`, must
  preserve in-flight exec streaming semantics
- **Back-compat**: old guest-agents on production images expect
  one-call-per-connection — protocol version negotiation in the
  Ping/Pong
- **Error recovery**: one stuck call must not wedge siblings
- Re-exercise all 748 unit tests + 34 integration tests

---

## Lever 1 — Shrink slim kernel further (3–5 days, 40–80 ms cold)

### Today

Our slim kernel = Firecracker's 6.1 microvm config + 7 CONFIGs we
added (9p, virtiofs, overlayfs, fuse, `VIRTIO_MMIO_CMDLINE_DEVICES`) +
upstream 6.12.30. Output: ~30 MB `vmlinux` with debug info.

Firecracker's config is pruned, but it inherits Kconfig defaults that
we don't need. Candidates to disable:

| Config                                                    | Why disable                                  | Saves                |
|-----------------------------------------------------------|----------------------------------------------|----------------------|
| `CONFIG_DEBUG_INFO_*`                                     | Only needed for debugging; ship separate debug kernel | ~5 MB binary, faster load |
| `CONFIG_AUDITSYSCALL`, `CONFIG_AUDIT_WATCH`               | No auditd in our guest                       | initcalls, ~1–2 ms   |
| `CONFIG_MAGIC_SYSRQ`                                      | No serial SysRq path needed                  | small                |
| `CONFIG_SECURITY_SELINUX`, `CONFIG_SECURITY_APPARMOR`     | Our guest-agent is unconstrained             | LSM hook overhead    |
| `CONFIG_SND_*`, `CONFIG_DRM_*`, `CONFIG_USB_*`, `CONFIG_INPUT_*` (non-PS/2) | Zero devices                   | many probe initcalls |
| `CONFIG_HW_RANDOM_*` (keep only `CONFIG_RANDOM_TRUST_CPU`)| RDRAND is enough                             | rng init             |
| `CONFIG_BTRFS_FS`, `CONFIG_XFS_FS`, `CONFIG_F2FS_FS`, `CONFIG_JBD2` | Only ext4/tmpfs/overlay/9p/virtiofs used | fs module init   |
| `CONFIG_NETFILTER` excess tables                          | SLIRP handles packet filtering host-side     | netfilter init       |

Each CONFIG disable removes an initcall from boot. Kernel initcalls
on our slim are currently ~100 ms total; aggressive trim could take
that to 50–60 ms.

### Why it's 3–5 days

- **Iterate-bench-iterate**: disable a batch → rebuild slim → run all
  integration tests → if broken, re-enable the culprit
- **Risk per disable**: each can have a non-obvious dependency (e.g.
  dropping a security LSM can change mount flags)
- Must re-validate all 4 integration suites + HN + openclaw each
  iteration
- Pin the final minimal config so future kernel bumps don't silently
  re-enable things

---

## Lever 2 — PVH boot entry (2–3 days, 15–40 ms cold)

### Today

x86_64 boot path loads `vmlinux` ELF, puts vCPU into **64-bit long
mode** with pre-populated page tables, jumps to `e_entry = 0x1000123`
(kernel's `startup_64`). Works, but the kernel's `startup_64` still
does real/protected-mode compatibility setup as if it were called
from bzImage — redundant work we pay for.

### PVH boot

Documented in `Documentation/x86/boot.rst`:

- Linux advertises a `XEN_ELFNOTE_PHYS32_ENTRY` note in its ELF
  program headers — the PVH entry point
- VMM reads that note, builds an `hvm_start_info` struct (memory map
  + cmdline pointer)
- vCPU enters **32-bit protected mode** with `%ebx = &hvm_start_info`
- Kernel's PVH entry skips the real-mode+16→32 transition stub
  entirely — lands ~100 lines deeper in `startup_32`
- Faster path to `start_kernel()`

Firecracker and cloud-hypervisor both use PVH. Their loader code is
~200 lines of Rust and is a working reference we can copy.

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

- Parse PT_NOTE section in ELF (easy, but needs tests)
- Build `hvm_start_info` struct (well-documented,
  cloud-hypervisor has a working impl)
- Change vCPU setup to enter 32-bit protected mode with `%ebx`
  pointing at the struct
- **Failure mode is silent triple-fault** — same failure class as the
  ELF entry-point bug we already fixed. Need `--console-file
  loglevel=7` during bringup
- aarch64: different story (already uses a similar "raw Image + DTB"
  path)

---

## How they stack

| Path                | Lever 7            | Lever 1       | Lever 2       | Total cold   | Total warm |
|---------------------|--------------------|---------------|---------------|--------------|------------|
| Cold                | 0 ms               | −40 to −80 ms | −15 to −40 ms | 160–200 ms   | same       |
| Warm startup-bench  | small              | 0             | 0             | same         | same       |
| Warm agent RTT      | −140 ms (20 tools) | 0             | 0             | same         | −140 ms    |

## Ordering suggestion

1. **Lever 1 first** (quick, low-risk, clearly-measurable delta, only
   toggling Kconfigs)
2. **Lever 2 next** (cold path, 2–3 days, gets us under 200 ms cold)
3. **Lever 7 last** (biggest but most invasive, plays better as its
   own PR)

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

## References

- `docs/superpowers/plans/2026-04-19-startup-push-to-sub-100ms.md` —
  parent plan describing all eight levers
- `docs/architecture.md` §"Snapshots / Performance" — current
  published numbers
- Commits `63dd396`, `58f0c6a` on this branch — Milestone A landings
- Firecracker microvm configs:
  <https://github.com/firecracker-microvm/firecracker/tree/main/resources/guest_configs>
- PVH boot protocol: `Documentation/x86/boot.rst` in the Linux source
- cloud-hypervisor PVH loader:
  <https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/arch/src/x86_64/mod.rs>
