//! Startup benchmark loop harness.
//!
//! Long-lived process that repeatedly boots a VM, does one exec, and shuts
//! it down. Designed to be the target PID for `perf-agent`, which needs a
//! process that lives longer than its 60s capture window. Also reports a
//! wall-clock distribution (p50/p95/p99) that serves as the "subsecond"
//! baseline we're trying to move.
//!
//! # Usage
//!
//! ```sh
//! export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
//! export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
//! cargo run --release --bin voidbox-startup-bench -- --iters 30
//! ```
//!
//! # What it measures
//!
//! Per iteration:
//! - `build` — `Sandbox::local().from_env().build()` (sync config resolution)
//! - `boot`  — first `exec("true", &[])` round-trip = forces `ensure_started`,
//!   kernel boot, vsock handshake, guest-agent ready, one exec RTT
//! - `stop`  — `Sandbox::stop()` shutdown
//! - `total` — sum of the above (what the user waits for end-to-end)

use std::path::PathBuf;
use std::time::{Duration, Instant};

use void_box::backend::GuestConsoleSink;
use void_box::sandbox::Sandbox;

const DEFAULT_ITERS: usize = 20;
const BENCH_CONFIG_HASH: &str = "bench-startup";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    let iters = args
        .iter()
        .position(|a| a == "--iters")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_ITERS);

    let memory_mb = args
        .iter()
        .position(|a| a == "--memory-mb")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1024);

    let warm_only = args.iter().any(|a| a == "--warm-only");
    let cold_only = args.iter().any(|a| a == "--cold-only");
    let breakdown = args.iter().any(|a| a == "--breakdown");
    let console_file: Option<PathBuf> = args
        .iter()
        .position(|a| a == "--console-file")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    eprintln!(
        "voidbox-startup-bench: pid={} iters={} memory_mb={}",
        std::process::id(),
        iters,
        memory_mb,
    );
    eprintln!("attach with: $HOME/.local/bin/perf-agent --pid {} --profile --offcpu --pmu --duration 60s --profile-output profile.pb.gz --offcpu-output offcpu.pb.gz --pmu-output pmu.txt", std::process::id());

    // Warmup — first boot amortizes cold page cache, module loads, etc.
    // We report it separately so the user can see both.
    let warmup = bench_once(memory_mb).await?;
    eprintln!(
        "warmup: build={:>7.1?} boot={:>7.1?} stop={:>7.1?} total={:>7.1?}",
        warmup.build, warmup.boot, warmup.stop, warmup.total
    );

    if !warm_only {
        eprintln!("\n-- Phase 1: cold boot --");
        let mut cold: Vec<Sample> = Vec::with_capacity(iters);
        for i in 0..iters {
            // Route console to a file only on the very first iteration so we
            // have a trace to inspect without amortizing file-write cost
            // across the distribution.
            let cf = if i == 0 {
                console_file.as_deref()
            } else {
                None
            };
            let s = bench_once_with_console(memory_mb, cf).await?;
            eprintln!(
                "cold[{:>2}]: build={:>7.1?} boot={:>7.1?} stop={:>7.1?} total={:>7.1?}",
                i, s.build, s.boot, s.stop, s.total
            );
            cold.push(s);
        }

        report("cold.build", cold.iter().map(|s| s.build));
        report("cold.boot", cold.iter().map(|s| s.boot));
        report("cold.stop", cold.iter().map(|s| s.stop));
        report("cold.total", cold.iter().map(|s| s.total));
    }

    if !cold_only {
        eprintln!("\n-- Phase 2: warm (snapshot-restore) --");
        let tmp = tempfile::tempdir()?;
        let snap_path = capture_snapshot(memory_mb, tmp.path()).await?;
        eprintln!("captured snapshot at: {}", snap_path.display());

        let mut warm: Vec<Sample> = Vec::with_capacity(iters);
        for i in 0..iters {
            let s = bench_restore_once(memory_mb, &snap_path, breakdown).await?;
            eprintln!(
                "warm[{:>2}]: build={:>7.1?} boot={:>7.1?} stop={:>7.1?} total={:>7.1?}",
                i, s.build, s.boot, s.stop, s.total
            );
            warm.push(s);
        }

        report("warm.build", warm.iter().map(|s| s.build));
        report("warm.boot", warm.iter().map(|s| s.boot));
        report("warm.stop", warm.iter().map(|s| s.stop));
        report("warm.total", warm.iter().map(|s| s.total));
    }

    Ok(())
}

/// Boot once, take a snapshot at `dir`, return the same path.
async fn capture_snapshot(
    memory_mb: usize,
    dir: &std::path::Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let sandbox = Sandbox::local()
        .from_env()?
        .memory_mb(memory_mb)
        .network(false)
        .build()?;
    // Trigger cold boot.
    let _ = sandbox.exec("sh", &["-c", ":"]).await?;
    sandbox
        .create_auto_snapshot(dir, BENCH_CONFIG_HASH.into())
        .await?;
    // create_auto_snapshot leaves the backend running on the restored VM;
    // stop it so the snapshot is quiesced on disk.
    sandbox.stop().await?;
    Ok(dir.to_path_buf())
}

async fn bench_restore_once(
    memory_mb: usize,
    snap_path: &std::path::Path,
    breakdown: bool,
) -> Result<Sample, Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let sandbox = Sandbox::local()
        .from_env()?
        .memory_mb(memory_mb)
        .network(false)
        .snapshot(snap_path)
        .build()?;
    let t1 = Instant::now();

    // First exec triggers ensure_started() → from_snapshot (sub-ms, see
    // `void_box::vmm` debug logs) → handshake + exec RTT. Most of the
    // elapsed time here is the guest kernel resuming from HLT/NOHZ-idle
    // on the restored vCPU, not host-side work.
    let out = sandbox.exec("sh", &["-c", ":"]).await?;
    if !out.success() {
        return Err(format!(
            "warm exec failed: exit={:?} stderr={}",
            out.exit_code,
            out.stderr_str()
        )
        .into());
    }
    let t2 = Instant::now();

    // Optional: a second exec measures steady-state RTT on the
    // already-awake guest (no handshake retry, no guest reactivation) and
    // lets us split `first_exec = guest_wake + rtt`.
    if breakdown {
        let t_rtt_start = Instant::now();
        let _ = sandbox.exec("sh", &["-c", ":"]).await?;
        let t_rtt = t_rtt_start.elapsed();
        let t_guest_wake = (t2 - t1).saturating_sub(t_rtt);
        eprintln!(
            "  ^ warm breakdown: first_exec={:?}  second_exec_rtt={:?}  guest_wake_est={:?}",
            t2 - t1,
            t_rtt,
            t_guest_wake,
        );
    }

    sandbox.stop().await?;
    let t3 = Instant::now();

    Ok(Sample {
        build: t1 - t0,
        boot: t2 - t1,
        stop: t3 - t2,
        total: t3 - t0,
    })
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    build: Duration,
    boot: Duration,
    stop: Duration,
    total: Duration,
}

async fn bench_once(memory_mb: usize) -> Result<Sample, Box<dyn std::error::Error>> {
    bench_once_with_console(memory_mb, None).await
}

async fn bench_once_with_console(
    memory_mb: usize,
    console_file: Option<&std::path::Path>,
) -> Result<Sample, Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let mut builder = Sandbox::local()
        .from_env()?
        .memory_mb(memory_mb)
        .network(false);
    if let Some(path) = console_file {
        builder = builder.guest_console(GuestConsoleSink::File(path.to_path_buf()));
    }
    let sandbox = builder.build()?;
    let t1 = Instant::now();

    // First exec forces VM start + kernel boot + vsock handshake + one RTT.
    // This is the "time to ready" a user actually waits for.
    // `sh -c :` is allowlist-safe and exits 0 immediately.
    let out = sandbox.exec("sh", &["-c", ":"]).await?;
    if !out.success() {
        return Err(format!(
            "exec sh : failed: exit={:?} stderr={}",
            out.exit_code,
            out.stderr_str()
        )
        .into());
    }
    let t2 = Instant::now();

    sandbox.stop().await?;
    let t3 = Instant::now();

    Ok(Sample {
        build: t1 - t0,
        boot: t2 - t1,
        stop: t3 - t2,
        total: t3 - t0,
    })
}

fn report(label: &str, samples: impl Iterator<Item = Duration>) {
    let mut v: Vec<Duration> = samples.collect();
    v.sort();
    if v.is_empty() {
        return;
    }
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    eprintln!(
        "{:<6}  min={:>7.1?}  p50={:>7.1?}  p95={:>7.1?}  p99={:>7.1?}  max={:>7.1?}  n={}",
        label,
        v[0],
        p(0.50),
        p(0.95),
        p(0.99),
        v[v.len() - 1],
        v.len(),
    );
}
