---
name: profile
description: Use when investigating performance issues, VM freezes, high CPU usage, or when the user asks to profile a voidbox process. Requires a running process PID.
---

# Profile a VoidBox Process

Profile CPU, off-CPU, and hardware counters using the eBPF-based `perf-agent` tool.

## Collect

```bash
/home/diego/github/perf-agent/perf-agent --pid <PID> --profile --offcpu --pmu --duration <DURATION>
```

Default duration: `60s`. Use `120s` or longer to capture intermittent issues.

Outputs `profile.pb.gz` and `offcpu.pb.gz` in `/home/diego/github/perf-agent/`.

## Analyze

```bash
cd /home/diego/github/perf-agent
go tool pprof -text -nodecount=50 -cum profile.pb.gz
go tool pprof -text -nodecount=50 -cum offcpu.pb.gz
```

Use `-cum` for cumulative (call tree) view. Use flat (no `-cum`) to see where time is actually spent.

## Interpret PMU Metrics

The tool prints PMU metrics inline after collection:

| Metric | Healthy | Investigate |
|--------|---------|-------------|
| IPC | > 1.0 | < 0.8 (memory-bound) |
| Cache Misses/1K Instr | < 5 | > 10 (cache thrashing) |
| P99.9 on-CPU | < 50ms | > 100ms (long hold) |
| Voluntary switches | > 80% | < 50% (CPU-bound) |
| Runqueue latency P99 | < 1ms | > 5ms (CPU contention) |

## Key VoidBox Threads

| Thread name | Expected behavior |
|-------------|-------------------|
| `vcpu-N` | In `KVM_RUN` ioctl (off-CPU is normal) |
| `net-poll` | 5ms sleep loop, slirp poll + relay |
| `vsock-irq` | Blocked on vsock read (off-CPU is normal) |
| `tokio-runtime-w` | `epoll_wait` when idle (normal) |
| `PtySession::run` | Blocked on PTY read (normal) |

## Quick Diagnosis

- **High CPU in `BigEndian::read_u16` / checksum**: Packet parsing overhead in slirp
- **High CPU in `Virtio9pDevice::handle_readdir`**: Host filesystem ops from guest dir traversal
- **Off-CPU `Mutex::lock` > 5%**: Lock contention between threads
- **P99.9 on-CPU > 200ms**: A thread holding CPU too long, likely blocking others
- **Process dies during profiling**: Profiles may not flush — rely on PMU metrics printed to stdout
