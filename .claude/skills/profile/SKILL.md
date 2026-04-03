---
name: profile
description: Use when investigating performance issues, VM freezes, high CPU usage, or when the user asks to profile a voidbox process. Also invoked as /perf.
---

# Profile a VoidBox Process

Profile CPU, off-CPU, and hardware counters using the eBPF-based
[perf-agent](https://github.com/dpsoft/perf-agent) tool.

## Prerequisites

**Tell the user FIRST before doing anything else:**

> perf-agent requires Linux capabilities to run without sudo. Please run:
> ```bash
> sudo setcap 'cap_bpf,cap_perfmon,cap_sys_ptrace,cap_checkpoint_restore=ep' $HOME/.local/bin/perf-agent
> ```
> (Only needed once after each download.)
>
> **Note:** The binary must NOT be on a `nosuid` filesystem (e.g. `/tmp`), as file capabilities are silently ignored there.

Do NOT attempt to run `sudo` yourself. Ask the user and wait for confirmation.

## Setup

Run these checks yourself (no sudo needed):

**1. Detect architecture and download if needed:**

```bash
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  SUFFIX="linux-amd64" ;;
  aarch64) SUFFIX="linux-arm64" ;;
esac

mkdir -p "$HOME/.local/bin"

LATEST=$(curl -s https://api.github.com/repos/dpsoft/perf-agent/releases/latest | grep '"tag_name"' | cut -d'"' -f4)

if [ -x "$HOME/.local/bin/perf-agent" ]; then
  CURRENT=$("$HOME/.local/bin/perf-agent" --version 2>/dev/null || echo "unknown")
  if echo "$CURRENT" | grep -q "${LATEST#v}"; then
    echo "perf-agent up-to-date ($LATEST)"
  else
    echo "Downloading perf-agent $LATEST..."
    curl -fSL -o "$HOME/.local/bin/perf-agent" "https://github.com/dpsoft/perf-agent/releases/download/${LATEST}/perf-agent-${SUFFIX}"
    chmod +x "$HOME/.local/bin/perf-agent"
  fi
else
  echo "Downloading perf-agent $LATEST..."
  curl -fSL -o "$HOME/.local/bin/perf-agent" "https://github.com/dpsoft/perf-agent/releases/download/${LATEST}/perf-agent-${SUFFIX}"
  chmod +x "$HOME/.local/bin/perf-agent"
fi
```

After download or upgrade, ask the user to run the `setcap` command.

**2. Verify Go (required for pprof analysis):**

```bash
go version
```

## Find the PID

Auto-detect the running voidbox process:

```bash
pgrep -fa voidbox
```

If multiple processes, pick the `voidbox shell` one. If none found, tell the
user to start a voidbox process first.

## Collect

```bash
$HOME/.local/bin/perf-agent --pid <PID> --profile --offcpu --pmu \
  --duration 60s \
  --profile-output profile.pb.gz \
  --offcpu-output offcpu.pb.gz \
  --pmu-output pmu.txt
```

### perf-agent flags

| Flag | Description | Default |
|------|-------------|---------|
| `--pid <PID>` | Target process ID | required (or `--all`) |
| `-a, --all` | System-wide profiling (all processes) | false |
| `--profile` | Enable CPU profiling with stack traces | false |
| `--offcpu` | Enable off-CPU profiling with stack traces | false |
| `--pmu` | Enable PMU hardware counters | false |
| `--duration <DUR>` | Collection duration | `10s` |
| `--sample-rate <HZ>` | CPU profiling sample rate | `99` |
| `--profile-output <PATH>` | Output path for CPU profile | auto-generated |
| `--offcpu-output <PATH>` | Output path for off-CPU profile | auto-generated |
| `--pmu-output <PATH>` | Output path for PMU metrics | stdout |
| `--per-pid` | Show per-PID breakdown (only with `-a --pmu`) | false |
| `--tag key=value` | Add tag to profile (repeatable) | — |

## Analyze

```bash
go tool pprof -text -nodecount=50 -cum profile.pb.gz    # call tree view
go tool pprof -text -nodecount=50 -cum offcpu.pb.gz     # off-CPU call tree
go tool pprof -text -nodecount=20 profile.pb.gz          # flat hotspots
cat pmu.txt                                              # PMU metrics
```

## Performance Investigation

Use this workflow to find bottlenecks in a running (non-frozen) VM.

**1. Baseline profile** — capture 60s under normal workload.

**2. Read PMU summary** from `pmu.txt`:

- IPC < 0.8 → memory-bound, look for cache thrashing
- IPC > 1.5 → compute-bound, look for hot loops
- Cache misses > 10/1K → data structures not cache-friendly
- P99.9 on-CPU > 100ms → some thread holding CPU too long
- Preempted > 20% → CPU saturation

**3. CPU hotspots** — functions consuming > 5% of flat CPU:

- Packet parsing (`BigEndian::read_u16`, checksum) → optimize hot path
- Filesystem ops (`statx`, `getdents`) → async or batch I/O
- Lock overhead (`MutexGuard::drop`) → reduce critical section
- Memory allocation (`malloc`, `Vec::new`) → pre-allocate or pool

**4. Off-CPU bottlenecks** — unexpected blocking:

- `Mutex::lock` > 3% → lock contention, reduce lock scope
- `read`/`write` on vCPU thread → blocking I/O on wrong thread
- `nanosleep` in net-poll → normal (5ms poll interval)
- `epoll_wait` in tokio → normal (idle reactor)

**5. Thread-level snapshot** with gstack:

```bash
gstack <PID> 2>&1 | awk '/^Thread/{t=$0} /void_box|slirp|virtio|vcpu_run|pty/{if(t){print t; t=""} print $0}'
```

**6. Compare after optimization** — re-profile and check IPC, cache misses,
P99.9, and whether the target function dropped from top-20.

## Frozen VM Triage

When a VM freezes (PTY stops producing output):

**1. gstack first** — identify which thread is stuck:

```bash
gstack <PID> 2>&1 | awk '/^Thread/{t=$0} /void_box|slirp|virtio|vcpu_run|pty|forward_dns/{if(t){print t; t=""} print $0}'
```

Key patterns:
- vcpu-N NOT in `KVM_RUN` → stuck in host-side handler (blocking I/O)
- vcpu-N in `KVM_RUN` + PTY at `read_from_sync` → guest process stopped output
- vcpu-N in `KVM_RUN` + PTY at `writer_handle.join()` → guest sent PtyClosed
- All threads normal → guest-side issue, check exit code

**2. Profile** for 30s to capture PMU metrics.

**3. Exit code** after process dies:

```bash
echo $?
# 153 = SIGXFSZ (file size limit), 137 = SIGKILL (OOM), 139 = SIGSEGV
```

## PMU Reference

| Metric | Healthy | Investigate |
|--------|---------|-------------|
| IPC | > 1.0 | < 0.8 (memory-bound) |
| Cache Misses/1K Instr | < 5 | > 10 (cache thrashing) |
| P99.9 on-CPU | < 50ms | > 100ms (long hold) |
| Voluntary switches | > 80% | < 50% (CPU-bound) |
| Runqueue latency P99 | < 1ms | > 5ms (CPU contention) |

## VoidBox Thread Reference

| Thread name | Expected behavior |
|-------------|-------------------|
| `vcpu-N` | In `KVM_RUN` ioctl (off-CPU is normal) |
| `net-poll` | 5ms sleep loop, slirp poll + relay |
| `vsock-irq` | Blocked on vsock read (off-CPU is normal) |
| `tokio-runtime-w` | `epoll_wait` when idle (normal) |
| `PtySession::run` | Blocked on PTY read (normal) |

## Known Issues

- **vcpu-N stuck in `forward_dns_query`**: DNS blocking vCPU (fixed in 50cdb9f)
- **vcpu-N stuck in `TcpStream::connect_timeout`**: TCP connect blocking vCPU (known, not yet fixed)
- **Exit code 153 (SIGXFSZ)**: File size limit exceeded (fixed in cb2e2b1 for interactive mode)
- **High CPU in `Virtio9pDevice::handle_readdir`**: Host FS ops on vCPU thread
- **Process dies during profiling**: Profiles may not flush — rely on `pmu.txt` or stdout

## Comparing Profiles

| Metric | Meaning |
|--------|---------|
| Max on-CPU | Worst-case thread hold (stale if process was stuck earlier) |
| P99.9 on-CPU | Tail latency — should drop after fixing blocking calls |
| IPC | Higher is better, < 0.8 is memory-bound |
| Cache misses/1K | Should drop after removing hot-path work from vCPU threads |
