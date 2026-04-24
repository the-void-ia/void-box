# Guest observability — CPU/heap profiling + learned debugging tools

**Status:** Design approved 2026-04-23; implementation pending.
**Motivation:** The userspace-vsock stall investigation
(`docs/superpowers/plans/2026-04-21-vsock-userspace-stall.md`) took
two days in part because VoidBox has no first-class way to profile
guest code while it's running. Host-side `perf kvm` hit multiple
dead ends on our Fedora + AMD + slim-kernel combination. This spec
lays out what to ship to make the *next* "why is the guest
stuck/slow?" investigation a minutes-long rather than days-long
exercise, and captures the debugging tools the previous
investigation did eventually find useful so they're not
rediscovered from scratch.

Scope for v1: a guest-side pprof sampler (CPU + heap) wired through
the persistent multiplex control channel, exposed as
`voidbox profile --run-id <id> --kind cpu|heap` in the daemon CLI.
Out of scope for v1: shipping perf-agent inside the guest, SLIRP
port-forwarding of HTTP pprof, and ad-hoc attach to non-daemon VMs.
Both are sketched in "Future work" so the next pass doesn't start
blank.

---

## TL;DR for the next reader

1. **Ship first:** pprof-rs CPU sampler + jemalloc_pprof heap
   endpoint in guest-agent, exposed via a new `ProfileRequest` /
   `ProfileResponse` message pair on the persistent multiplex
   channel, invoked from the host by `voidbox profile ...`.
2. **Runtime-gated lazy-init:** pprof-rs is compiled into every
   guest-agent build but installs no signal handler / timer until
   the first request arrives. Zero steady-state cost.
3. **Heap profile requires `jemalloc-pprof` feature** on
   guest-agent (currently only present on host-side `voidbox`). We
   will NOT switch guest-agent's default allocator as part of v1;
   heap requests return `Unavailable(...)` until someone rebuilds
   guest-agent with the feature.
4. **Daemon-only UX** — no ad-hoc attach to `voidbox run` /
   `voidbox shell` in v1. Add later if needed.
5. **Learned-debugging-tools runbook** (below) is the v0 fallback
   for anything this profiler can't see (guest kernel stacks,
   stall patterns where userspace isn't the problem).

---

## Architecture

```
Host                                           Guest
┌───────────────────────────────┐             ┌─────────────────────────────────┐
│ voidbox profile --run-id X    │             │ guest-agent                     │
│   --kind cpu|heap             │             │                                 │
│   --duration 30s -o out.pprof │             │ ┌──────────────────────────┐    │
└────────────┬──────────────────┘             │ │ ProfileRequest handler   │    │
             │ POST /runs/X/profile           │ │  ┌─────────────────────┐ │    │
             ▼                                │ │  │ cpu: pprof-rs       │ │    │
┌───────────────────────────────┐             │ │  │   (SIGPROF, 99Hz)   │ │    │
│ voidbox daemon                │ vsock RPC   │ │  └─────────────────────┘ │    │
│  POST /runs/:id/profile ────► │──via CCh──► │ │  ┌─────────────────────┐ │    │
│  ControlChannel::profile(…)   │◄─pprof bytes│ │  │ heap: jemalloc_pprof│ │    │
└───────────────────────────────┘             │ │  │   (feature-gated)   │ │    │
                                              │ │  └─────────────────────┘ │    │
                                              │ └──────────────────────────┘    │
                                              └─────────────────────────────────┘
```

Key constraints:

- One `MessageType::ProfileRequest` (new = 21) and one
  `MessageType::ProfileResponse` (new = 22). No other protocol
  changes.
- Sampler installs lazily on first CPU request; heap uses
  existing `jemalloc_pprof::dump_pprof()` when the feature is
  compiled into guest-agent.
- Host-side `VmCommand::Profile { request, response_tx }` is one
  more variant in the daemon's event-loop dispatch (parallels
  `Exec`, `WriteFile`, `MkdirP`).
- Output is raw pprof protobuf bytes. Users run
  `go tool pprof -http=:8080 cpu.pprof` themselves.

---

## Components

### Protocol (`void-box-protocol/src/lib.rs`)

```rust
enum MessageType {
    // ... existing 1..=20 ...
    ProfileRequest = 21,
    ProfileResponse = 22,
}

#[derive(Serialize, Deserialize)]
pub struct ProfileRequest {
    pub kind: ProfileKind,      // Cpu | Heap
    pub duration_secs: u32,     // 0 for Heap (snapshot)
    pub frequency_hz: u32,      // CPU only; default 99, clamped to [10, 1000]
}

#[derive(Serialize, Deserialize)]
pub struct ProfileResponse {
    pub kind: ProfileKind,
    pub status: ProfileStatus,  // Ok | Unavailable(reason) | Error(msg)
    pub pprof_gz: Vec<u8>,      // gzipped pprof v1 protobuf
}

pub enum ProfileKind { Cpu, Heap }
pub enum ProfileStatus { Ok, Unavailable(String), Error(String) }
```

### Guest side

New file `guest-agent/src/profiling.rs`:

- `cpu_profile(duration: Duration, hz: u32) -> Result<Vec<u8>>`
  builds `pprof::ProfilerGuardBuilder::default()`, sleeps
  `duration`, calls `.report().build()`, serializes to pprof
  protobuf, gzips. Signal handler + itimer only active during the
  sample window; cleaned up on `ProfilerGuard` drop.
- `heap_profile() -> Result<Vec<u8>>` under
  `#[cfg(feature = "jemalloc-pprof")]` calls
  `jemalloc_pprof::dump_pprof()`. `#[cfg(not(...))]` returns
  `ProfileStatus::Unavailable("guest-agent built without
  jemalloc-pprof feature")`.

Dispatch handler extension in `handle_connection` (new match arm
for `MessageType::ProfileRequest`) runs the sample on a spawned
blocking thread so the dispatch loop keeps processing other RPCs
during the 30 s window.

### Host side

- `src/backend/control_channel.rs` — `async fn
  send_profile_request(&self, req: &ProfileRequest) ->
  Result<ProfileResponse>` following the existing
  `spawn_blocking` pattern.
- `src/vmm/mod.rs` — new `VmCommand::Profile { request,
  response_tx }` variant, wired into `dispatch_vm_command`.
- `src/daemon.rs` — new `POST /runs/:id/profile` endpoint, body
  `{kind, duration_secs, frequency_hz}`, returns
  `Content-Type: application/vnd.google.protobuf` with the pprof
  bytes (no base64 wrapping).
- `src/bin/voidbox/profile.rs` — new `voidbox profile`
  subcommand that hits the daemon and writes stdout or `-o`
  path.

### Cargo dependencies (guest-agent)

- `pprof = { version = "0.13", features = ["protobuf-codec"] }`
  — always on, ~200 KB added to initramfs.
- `jemalloc_pprof` — gated on existing `jemalloc-pprof` feature
  (not default).

---

## Data flow (one request, end to end)

1. User runs `voidbox profile --run-id abc --kind cpu --duration
   30s -o cpu.pprof`.
2. CLI `POST http://unix:/.../daemon.sock/runs/abc/profile`,
   body `{"kind":"cpu","duration_secs":30,"frequency_hz":99}`.
3. Daemon looks up run "abc" → `Arc<MicroVm>` → sends
   `VmCommand::Profile` on `command_tx`. Daemon HTTP worker
   awaits a `oneshot` response.
4. Event loop → `dispatch_vm_command` →
   `ControlChannel::send_profile_request(req)`. Serializes
   request JSON, `spawn_blocking` writes on persistent vsock.
5. Guest-agent dispatch thread reads frame →
   `MessageType::ProfileRequest` → parses JSON → spawns a
   worker thread for the sample. Dispatch continues handling
   other messages.
6. Guest worker runs `profiling::cpu_profile(30 s, 99 Hz)` →
   pprof protobuf → gzip. Builds `ProfileResponse` and sends via
   `send_mux_response` (uses `CONN_WRITE_LOCK` to serialize with
   other traffic).
7. Host `multiplex-reader` routes `request_id` → pending slot →
   wakes waiter. `send_profile_request` returns `Ok(...)`.
8. Daemon HTTP response: 200 OK,
   `Content-Type: application/vnd.google.protobuf`, body =
   `pprof_gz` bytes.
9. CLI writes bytes to `cpu.pprof`, exit 0.
10. User: `go tool pprof -http=:8080 cpu.pprof`.

**Concurrency contract:** the guest-side sample runs on its own
blocking thread. Exec, mkdir, etc. keep flowing on the multiplex
channel during the 30 s window. `CONN_WRITE_LOCK` already
serializes response writes. No new locking.

**Cancellation:** if the host disconnects or the daemon is killed
mid-profile, the guest sample still completes; the response write
fails silently; guest keeps running. `ProfilerGuard::drop`
detaches the signal handler cleanly.

---

## Error handling

| Failure mode | Response | User sees |
|---|---|---|
| `kind=heap` on a guest-agent built without `jemalloc-pprof` | `status: Unavailable("feature jemalloc-pprof not compiled")`, empty body | HTTP 409; CLI prints "heap profiling unavailable — rebuild guest-agent with `--features jemalloc-pprof`", exit 2 |
| `pprof::ProfilerGuardBuilder` fails (e.g. seccomp blocks `sigaction`) | `status: Error("failed to install profiler: {errno}")` | HTTP 500; CLI prints the error, exit 1 |
| `duration_secs > 600` | Daemon rejects at parse | HTTP 400, no VM work |
| `frequency_hz` outside `[10, 1000]` | Daemon rejects at parse | HTTP 400 |
| Run id not found | Daemon 404 | CLI prints "no run abc", exit 2 |
| VM stops mid-profile | `send_profile_request` errors "connection closed"; daemon 503 | CLI prints "VM terminated during profile", exit 1 |
| Guest-agent panics during sample | guest-agent crashes; daemon notices via VM exit | HTTP 503 |
| Sample completes but empty (nothing to sample) | `status: Ok, pprof_gz: empty` | CLI writes empty file, prints warning, exit 0 |

**Non-errors:** idle guest during a CPU window is not a failure —
caller may have wanted to confirm idleness. Empty heap profile
with jemalloc present is valid.

**Guest fault isolation:** worker runs on `std::thread::spawn`. If
the sample panics, the dispatch loop is unaffected. The worker's
join handle returns `Error` on `join()` failure — guest-agent
stays up.

---

## Testing

**Unit tests** (fast, no VM):

- `void-box-protocol` — round-trip `ProfileRequest` /
  `ProfileResponse` through serde + length-prefix framing.
  Assert `MessageType::ProfileRequest = 21`, `ProfileResponse =
  22`; backward-compat for existing message types.
- `guest-agent::profiling::cpu_profile` — against a synthetic
  busy-loop for 1 s at 99 Hz, assert returned bytes gunzip to a
  valid pprof v1 protobuf with ≥ 1 sample (via
  `pprof::protos::Profile::decode`).
- `guest-agent::profiling::heap_profile` under
  `#[cfg(not(feature = "jemalloc-pprof"))]` — assert the
  `Unavailable(...)` variant.
- `src/bin/voidbox/profile.rs` CLI — `--kind`, `--duration`,
  `--frequency-hz`, `-o` argument validation.

**Integration tests** (Linux, VM backend, ignored by default):

- `tests/profile_integration.rs::profile_cpu_roundtrip` — start a
  backend, exec a short busy loop (`sh -c "while :; do :; done"`
  self-killed after 5 s), concurrently request a 3 s CPU
  profile, assert response is `Ok` and pprof body ≥ 1 KiB and
  parses as valid pprof. Gate behind `VOID_BOX_KERNEL` +
  `VOID_BOX_INITRAMFS`.
- `tests/profile_integration.rs::profile_heap_unavailable_without_feature`
  — backend built without `jemalloc-pprof`; request heap
  profile; assert `ProfileStatus::Unavailable(_)`.
- Deferred to a follow-up once guest-agent has a jemalloc build
  variant: `profile_heap_roundtrip` round-trip.

**Regression guard:** extend `persistent_channel_serial_exec_many`
with a midway 500 ms CPU profile request — verifies profiling
does not break multiplex ordering.

**Manual verification** (one-off, not checked in): boot a daemon
VM with a claude-code run, run `voidbox profile --run-id ...
--kind cpu --duration 30s`, open `cpu.pprof` in `go tool pprof
-http=:8080`, spot-check flamegraph shows real guest stacks.

---

## Alternatives considered

### Transport: why not a vsock listener port?

A guest-side vsock listener on a fixed port (e.g. 6060) serving
pprof HTTP would be semantically closest to production pprof
tooling. Rejected because:

- It duplicates the auth layer we already have on the multiplex
  channel (session secret + multiplex framing). A second
  listener needs its own auth or we open a hole.
- VZ (macOS) uses a callback-based vsock connector; a second
  listener doubles the GCD-bridge surface.
- Gain is marginal: `go tool pprof` consumes a file just as
  happily as an HTTP endpoint, and the host CLI shim adds two
  lines of code.

### Transport: why not HTTP over SLIRP?

A guest HTTP listener on the SLIRP interface (`10.0.2.15:6060`)
with host port-forwarding would let `go tool pprof -http
http://localhost:6060/debug/pprof/profile` work verbatim.
Rejected because:

- Requires SLIRP port-forwarding wiring (doesn't exist yet in
  host config surface).
- VZ uses PCI-based networking with a different port-forward
  model; keeping host API identical across backends means more
  work.
- Not available on VMs booted without networking.

### Shipping perf-agent inside the guest

Considered as the secondary profiler because it catches **guest
kernel** stacks (pprof-rs is userspace only). Deferred because:

- Binary size: perf-agent is a ~20 MB static Go blob. Against a
  production Claude initramfs of ~100 MB it's still a 20 %
  bump; on snapshot/restore hot paths this matters.
- Kernel prerequisites: `CONFIG_BPF=y`, `CONFIG_BPF_JIT=y`,
  `CONFIG_DEBUG_INFO_BTF=y`, and a mounted
  `/sys/kernel/btf/vmlinux`. Our slim kernel likely strips some
  of this — needs audit before committing.
- Capability surface: perf-agent needs `CAP_SYS_ADMIN`,
  `CAP_BPF`, `CAP_PERFMON`, `CAP_SYS_PTRACE`,
  `CAP_CHECKPOINT_RESTORE`. Guest-agent runs as PID 1 (has
  them) but leaking caps into child processes needs care.

Plan for the eventual pass: gate perf-agent behind a
`perf-agent` Cargo feature on guest-agent, rebuild the initramfs
only for targeted debugging sessions, expose via a separate
`ProfileKind::Bpf` variant that routes to a perf-agent-driven
eBPF profile rather than pprof-rs.

### Why not just run `perf` on the guest?

Same prerequisite list as perf-agent, plus perf is ~10 MB more,
plus the report format is not pprof — we'd reinvent conversion.
No reason to pick perf over perf-agent given perf-agent already
emits pprof protobuf.

### Why not host-side `perf kvm --guest`?

Hit multiple dead ends during the stall investigation:

- `perf kvm stat record` → `perf kvm stat report` produces
  "incompatible file format" on Fedora 43 + AMD SVM. Recurring
  `perf`/kernel version mismatch bug.
- `perf record -p <vcpu_tid>` only samples when the vcpu thread
  is in the ioctl return path — misses most guest-mode
  execution.
- Guest RIP mapping needs `--guestvmlinux=path` and matching
  kallsyms; our slim kernel strips buildids, so symbol lookup is
  fragile.
- AMD AVIC / PMU virtualization limits what counters are visible
  from the host.

Guest-side profiling sidesteps all of this: the guest profiles
itself with its own timer-driven sampler, hands the host a
finished pprof. Zero kernel-format drama.

---

## Future work

### Ship perf-agent behind a feature flag

Addresses the "guest kernel stack" gap. Plan:

- Audit slim-kernel config for `CONFIG_BPF_*` and `BTF`.
  Re-enable if needed; measure boot-time cost (expect a few ms
  at most — BPF JIT lazy-initializes).
- Add `perf-agent` Cargo feature on guest-agent. Feature gate on
  a new `ProfileKind::Bpf` variant. Binary is included only when
  `--features perf-agent` is set; production builds stay thin.
- Wire a new guest-side worker that exec's perf-agent as a child
  process with the required caps, reads its pprof bytes over a
  pipe, frames as `ProfileResponse`.

### Ad-hoc attach to `voidbox run` / `voidbox shell`

Requires a pidfile / shared socket so `voidbox profile` can
attach to a non-daemon VM. Adds a small amount of wiring, no new
protocol.

### SLIRP-forwarded HTTP pprof for developer ergonomics

Once SLIRP port-forwarding exists as a first-class config field,
expose the existing ControlChannel-backed profiler over
`http://localhost:6060/debug/pprof/profile` as well.
`go tool pprof -http` then works without a CLI shim.

### Extended stall-watchdog with `/proc/*/stat` R-state
enumeration

The stall-investigation watchdog in `guest-agent/src/main.rs`
(`dump_all_task_stacks`, commit `764535a`) currently dumps only
the guest-agent process's thread stacks. A follow-up change
walks `/proc/*/stat` and dumps any process in state `R` with its
kernel stack and current syscall — useful for the specific case
of "guest-agent is sleeping but something else in the guest is
burning CPU". Code was prototyped during the stall investigation
and is ~40 lines; will land as a separate small commit
independent of this spec.

### Profile-on-stall auto-trigger

Tie the existing stall watchdog to `voidbox profile`: when the
watchdog fires (dispatch stalled > N seconds), the daemon
auto-captures a CPU profile and writes it to a known directory.
Useful for production-incident post-mortems where the operator
isn't watching.

---

## Learned debugging tools (runbook)

These are the tools the stall investigation eventually found
useful after a lot of dead ends. Captured here so the next
investigator doesn't have to rediscover them.

### KVM_GET_REGS via gdb — sample guest RIP from the host

When the guest is "stuck", the first question is "what is the
guest actually doing?". Host-side `perf kvm` failed us. The
reliable tool is a gdb script that attaches to the VMM process
and calls `ioctl(vcpu_fd, KVM_GET_REGS, &buf)` through the
inferior's libc, then prints `buf[rip_offset]`:

```gdb
set pagination off
attach <vmm_pid>
set scheduler-locking on
set $buf = (unsigned char *) malloc(152)
set $rc = (int) ioctl(<vcpu_fd>, 0x8090ae81, $buf)
printf "GUEST_RIP=0x%lx\n", *(unsigned long*)($buf + 128)
call (void)free($buf)
detach
quit
```

Find `<vcpu_fd>` from `ls -la /proc/<pid>/fd | grep
kvm-vcpu:0`. `0x8090ae81` is `KVM_GET_REGS`; the struct
`kvm_regs` layout has `rip` at offset 128, `rflags` at 136.
Cross-reference the guest vmlinux with `nm` to symbolize:

```bash
nm target/vmlinux-slim-x86_64 | sort | \
  awk -v target="819ab14b" '... find symbol before/after'
```

The stall investigation's decisive evidence was the RIP landing
6/6 samples at `pv_native_safe_halt` — guest was halted, not
hot-looping.

### `perf kvm --guest stat live --event=ioport` — what PIOs is the guest doing?

Runs continuously, prints per-port vmexit histograms. Unlike
`perf record`, it doesn't write a file, so the Fedora/AMD
file-format bug doesn't apply. Typical use:

```bash
timeout 4 perf kvm --guest stat live --event=ioport
```

What to look for:

- Only `0x20`, `0x21`, `0x40` ports active at ~44 Hz = normal
  scheduler tick in virtual-wire mode (no ACPI MADT).
- Serial port (`0x3F8`) writes = guest is producing console
  output.
- Any unknown ports = custom instrumentation (see below).

### `/dev/port` as a tty-bypass debug write

When the guest is stuck and serial console has no bytes, you
still want a heartbeat from guest userspace. The serial tty path
can deadlock (see stall doc); `/dev/port` bypasses it entirely
by writing a byte directly to an x86 I/O port via a single
`outb` in the kernel, which causes an immediate PIO vmexit
observable from the host.

```rust
fn post_debug(byte: u8) -> bool {
    use std::io::{Seek, SeekFrom, Write};
    let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/port") else {
        return false;
    };
    if f.seek(SeekFrom::Start(0x80)).is_err() { return false; }
    f.write_all(&[byte]).is_ok()
}
```

Observe on the host via the ioport histogram above — port `0x80`
writes count means the writer is running. Requires
`CONFIG_DEVPORT=y` in the guest kernel.

Caveat: `/dev/port` still needs a syscall (`write`), so it
cannot debug "is the writer thread scheduled at all?" under a
guest kernel that has no runnable task. For that, use outb
directly (below).

### `outb`-based milestone markers — zero-syscall progress beacons

For the cheapest possible instrumentation inside a hot path:

```rust
#[inline(always)]
fn pio_mark(port: u16, byte: u8) {
    unsafe {
        std::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") byte,
            options(nomem, nostack, preserves_flags),
        );
    }
}
```

One machine instruction, zero syscalls, causes a PIO vmexit
observable from the host via `perf kvm stat live --event=ioport`
as a distinct `<port>:POUT` sample. Use distinct ports for
distinct milestones (e.g. `0x550..=0x558`) so the histogram
tells you the highest milestone reached.

Requires `iopl(3)` once at guest-agent startup to grant the
process permission to execute `out`. PID 1 has
`CAP_SYS_RAWIO`.

These markers were crucial during the stall investigation for
confirming "the dispatch thread stopped executing instructions
between milestone M and M+1" — something neither `perf kvm` nor
`gdb` could show directly.

### Log-count-vs-iter-at-stall scaling heuristic

If your stall reproduces at iter N, and halving the log-per-iter
count roughly doubles N, the bug is in your logging path. This
is not a tool, it's a pattern — noticed late in the stall
investigation, would have saved a day if noticed early.
Generalize: anything where `(iter_at_stall) × (work_per_iter)`
is constant points at a cumulative-state bug in
`work_per_iter`, not in the RPC plumbing.

### Guest-agent stall watchdog

`guest-agent/src/main.rs::dump_all_task_stacks` (commit
`764535a`) dumps every thread's kernel stack to `/dev/console`
when `DISPATCH_PROGRESS` stops advancing for N seconds. Useful
when the stall is PID-1-internal. The planned R-state
enumeration extension (see Future work) expands it to dump
every process on the system when the stall is outside
guest-agent.

---

## Open questions (to resolve during implementation)

1. **Exact `pprof-rs` blocklist.** Default profiling includes
   `libc` / `pthread` frames which are usually noise. Start with
   `&["libc", "libgcc", "pthread"]` and tune based on first
   real-world profiles.
2. **Response size cap.** `ProfileResponse.pprof_gz` size is
   bounded by sample count × stack depth. At 99 Hz × 30 s with
   deep stacks, expect 100–500 KiB typical, up to a few MiB
   worst case. If it exceeds the multiplex frame size
   (`MAX_MESSAGE_SIZE` in `void-box-protocol`), we stream via
   `ExecOutputChunk`-style chunking or raise the cap. Audit
   during implementation.
3. **Heap profile + jemalloc defaults.** If we later flip
   guest-agent to jemalloc by default, the heap endpoint
   "silently" starts working — which is good. But it also
   changes memory characteristics. Measure RSS and startup
   latency impact before flipping.
