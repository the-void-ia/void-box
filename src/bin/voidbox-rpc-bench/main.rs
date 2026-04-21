//! Per-RPC latency benchmark.
//!
//! Measures the cost of individual host→guest RPCs on an already-warm
//! [`Sandbox`] — complementing `voidbox-startup-bench` (which measures
//! cold/warm VM lifecycle). Agent workloads do hundreds of RPCs per
//! session, so any regression in per-RPC overhead compounds into
//! seconds of user-visible latency.
//!
//! # Phases
//!
//! The binary boots one VM, warms up with a single `exec`, then runs
//! each phase inside that same VM to isolate RPC cost from boot cost:
//!
//! - `exec_seq`: `N` sequential `exec("sh","-c","echo ...")` calls.
//!   Stresses the streaming dispatch path (`call_stream` with
//!   `ExecResponse` terminator) and the oneshot round trip at its
//!   fastest cadence.
//! - `write_seq`: `N` sequential `write_file("/workspace/bench_N",
//!   "payload")` calls. Stresses the oneshot dispatch path with a
//!   non-trivial payload.
//! - `exec_conc`: `K` parallel `exec` calls fired from `tokio::spawn`.
//!   Stresses writer-mutex contention and the demultiplexer under
//!   concurrent request_ids.
//!
//! Percentiles are reported per phase. Absolute values depend on
//! hardware; use the relative shape to catch regressions between runs.
//!
//! # Usage
//!
//! ```sh
//! export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
//! export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
//! cargo run --release --bin voidbox-rpc-bench -- --seq-iters 32 --conc 16
//! ```
//!
//! Attach a profiler with the PID printed at startup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use void_box::sandbox::Sandbox;

/// Default sample count per sequential phase.
///
/// Capped at 8 because the current guest-agent dispatch loop leaks one
/// watchdog thread per `exec` call; beyond ~16 sequential calls on one
/// connection the guest stalls. Override with `--seq-iters N` once that
/// leak is fixed.
const DEFAULT_SEQ_ITERS: usize = 8;

/// Default concurrent exec fan-out.
const DEFAULT_CONC: usize = 8;

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
    let seq_iters = parse_flag(&args, "--seq-iters", DEFAULT_SEQ_ITERS);
    let conc = parse_flag(&args, "--conc", DEFAULT_CONC);
    let memory_mb = parse_flag(&args, "--memory-mb", 1024);

    eprintln!(
        "voidbox-rpc-bench: pid={} seq_iters={} conc={} memory_mb={}",
        std::process::id(),
        seq_iters,
        conc,
        memory_mb,
    );

    let sandbox = Arc::new(
        Sandbox::local()
            .from_env()?
            .memory_mb(memory_mb)
            .network(false)
            .build()?,
    );

    let t_warmup_start = Instant::now();
    let warmup_output = sandbox.exec("sh", &["-c", ":"]).await?;
    if !warmup_output.success() {
        return Err(format!(
            "warmup exec failed: exit={:?} stderr={}",
            warmup_output.exit_code,
            warmup_output.stderr_str(),
        )
        .into());
    }
    eprintln!("warmup: {:?}", t_warmup_start.elapsed());

    let exec_seq_samples = run_exec_seq(&sandbox, seq_iters).await?;
    report("exec_seq", &exec_seq_samples);

    let write_seq_samples = run_write_seq(&sandbox, seq_iters).await?;
    report("write_seq", &write_seq_samples);

    let exec_conc_samples = run_exec_conc(Arc::clone(&sandbox), conc).await?;
    report("exec_conc", &exec_conc_samples);

    let sandbox = Arc::into_inner(sandbox)
        .ok_or("bench owns sandbox references after join — refusing to drop")?;
    sandbox.stop().await?;
    Ok(())
}

async fn run_exec_seq(
    sandbox: &Sandbox,
    iters: usize,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iters);
    for index in 0..iters {
        let script = format!("echo rpc-{index}");
        let started = Instant::now();
        let output = sandbox.exec("sh", &["-c", &script]).await?;
        let elapsed = started.elapsed();
        if !output.success() {
            return Err(format!("exec_seq[{index}] failed: exit={:?}", output.exit_code).into());
        }
        samples.push(elapsed);
    }
    Ok(samples)
}

async fn run_write_seq(
    sandbox: &Sandbox,
    iters: usize,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iters);
    let payload = b"voidbox-rpc-bench: payload body for write_file latency\n".to_vec();
    for index in 0..iters {
        let path = format!("/workspace/bench_{index}.txt");
        let started = Instant::now();
        sandbox.write_file(&path, &payload).await?;
        samples.push(started.elapsed());
    }
    Ok(samples)
}

async fn run_exec_conc(
    sandbox: Arc<Sandbox>,
    conc: usize,
) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let t_batch_start = Instant::now();
    let mut handles = Vec::with_capacity(conc);
    for index in 0..conc {
        let sandbox = Arc::clone(&sandbox);
        handles.push(tokio::spawn(async move {
            let script = format!("echo rpc-conc-{index}");
            let started = Instant::now();
            let output = sandbox.exec("sh", &["-c", &script]).await?;
            let elapsed = started.elapsed();
            Ok::<(Duration, bool), void_box::Error>((elapsed, output.success()))
        }));
    }

    let mut samples = Vec::with_capacity(conc);
    for (index, handle) in handles.into_iter().enumerate() {
        let (sample, succeeded) = handle.await??;
        if !succeeded {
            return Err(format!("exec_conc[{index}] exited non-zero").into());
        }
        samples.push(sample);
    }
    eprintln!(
        "exec_conc: {} calls in wall-clock {:?}",
        conc,
        t_batch_start.elapsed()
    );
    Ok(samples)
}

fn parse_flag(args: &[String], flag: &str, default: usize) -> usize {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn report(label: &str, samples: &[Duration]) {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        eprintln!("{label}: no samples");
        return;
    }
    let percentile = |quantile: f64| {
        let index = ((sorted.len() as f64 - 1.0) * quantile).round() as usize;
        sorted[index]
    };
    eprintln!(
        "{:<10}  min={:>7.1?}  p50={:>7.1?}  p95={:>7.1?}  p99={:>7.1?}  max={:>7.1?}  n={}",
        label,
        sorted[0],
        percentile(0.50),
        percentile(0.95),
        percentile(0.99),
        sorted[sorted.len() - 1],
        sorted.len(),
    );
}
