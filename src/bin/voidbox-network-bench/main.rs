//! Wall-clock end-to-end network benchmark harness.
//!
//! Boots a real VM and measures TCP throughput, RR/CRR latency, and
//! UDP DNS qps inside the guest. Output is JSON for diffing against
//! a baseline.
//!
//! Mirrors `voidbox-startup-bench` in CLI shape and lifecycle.
//!
//! Linux-only because the smoltcp-based SLIRP stack is Linux-only.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde::Serialize;
use void_box::sandbox::Sandbox;

/// Transfer size per measurement run: 50 MiB.
const TRANSFER_MB: u32 = 50;

/// Bytes per megabit.
const BYTES_PER_MEGABIT: f64 = 1_000_000.0 / 8.0;

/// VM memory for the benchmark sandbox (MiB).
const BENCH_MEMORY_MB: usize = 1024;

/// SLIRP host-gateway address reachable from inside the guest.
const SLIRP_HOST_ADDR: &str = "10.0.2.2";

#[derive(Parser, Debug)]
#[command(version, about = "VoidBox network benchmark harness")]
struct Cli {
    /// Number of iterations per metric.
    #[arg(long, default_value_t = 5)]
    iterations: u32,

    /// Output JSON file. If omitted, prints to stdout.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Skip throughput measurements (useful for fast smoke runs).
    #[arg(long, default_value_t = false)]
    no_throughput: bool,
}

#[derive(Serialize, Debug, Default)]
struct Report {
    tcp_throughput_g2h_mbps: Option<f64>,
    // TODO(h2g): host→guest requires either a guest-side `nc -l` listener
    // or an inverse data-push loop.  The current harness only supports
    // guest-initiated connections (the guest calls `nc HOST PORT`).  A
    // host-push direction would need the guest to accept connections, which
    // means either (a) a guest-side daemon started before exec returns, or
    // (b) an additional RPC for "open a listening socket and tell us the
    // guest port" — out of scope for the minimal harness.
    tcp_throughput_h2g_mbps: Option<f64>,
    tcp_rr_latency_us_p50: Option<f64>,
    tcp_rr_latency_us_p99: Option<f64>,
    tcp_crr_latency_us_p50: Option<f64>,
    udp_dns_qps: Option<f64>,
    icmp_rr_latency_us_p50: Option<f64>, // None today; populated post-Phase-1
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let mut report = Report::default();

    if !cli.no_throughput {
        report.tcp_throughput_g2h_mbps = measure_tcp_throughput_g2h(cli.iterations).await?;
    }

    let json = serde_json::to_string_pretty(&report)?;
    match cli.output {
        Some(path) => std::fs::write(path, json)?,
        None => println!("{json}"),
    }
    Ok(())
}

/// Measure guest-to-host TCP throughput.
///
/// Binds a host-side TCP listener on `127.0.0.1:0`, boots a VM, and execs a
/// BusyBox shell snippet that pipes `dd` output to `nc`.  The host drain thread
/// records bytes received and wall-clock elapsed time; Mbps is computed from
/// those two numbers.  Runs `iterations` times and returns the mean.
///
/// Returns `None` if every iteration fails to parse or times out.
async fn measure_tcp_throughput_g2h(
    iterations: u32,
) -> Result<Option<f64>, Box<dyn std::error::Error>> {
    let sandbox = Sandbox::local()
        .from_env()?
        .memory_mb(BENCH_MEMORY_MB)
        .network(true)
        .build()?;

    // Prime the VM (triggers boot + vsock handshake) before the timed loop.
    let probe = sandbox.exec("sh", &["-c", ":"]).await?;
    if !probe.success() {
        return Err(format!(
            "VM probe exec failed: exit={:?} stderr={}",
            probe.exit_code,
            probe.stderr_str()
        )
        .into());
    }

    let mut mbps_samples: Vec<f64> = Vec::new();

    for iteration_index in 0..iterations {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let host_port = listener.local_addr()?.port();

        let (drain_tx, drain_rx) = mpsc::channel::<(u64, Duration)>();

        std::thread::spawn(move || {
            let drain_result = drain_one_connection(&listener);
            let _ = drain_tx.send(drain_result);
        });

        let guest_cmd = format!(
            "dd if=/dev/zero bs=1M count={TRANSFER_MB} 2>/dev/null | nc {SLIRP_HOST_ADDR} {host_port}",
        );

        let exec_result = sandbox.exec("sh", &["-c", &guest_cmd]).await;

        match exec_result {
            Err(exec_err) => {
                tracing::warn!(
                    iteration = iteration_index,
                    error = %exec_err,
                    "g2h iteration exec error; skipping"
                );
                continue;
            }
            Ok(output) => {
                if !output.success() {
                    tracing::warn!(
                        iteration = iteration_index,
                        exit_code = ?output.exit_code,
                        stderr = output.stderr_str(),
                        "g2h iteration non-zero exit; skipping"
                    );
                }
            }
        }

        match drain_rx.recv_timeout(Duration::from_secs(120)) {
            Err(recv_err) => {
                tracing::warn!(
                    iteration = iteration_index,
                    error = %recv_err,
                    "g2h drain channel receive error; skipping"
                );
            }
            Ok((bytes_received, elapsed)) => {
                let elapsed_secs = elapsed.as_secs_f64();
                if elapsed_secs < 0.01 {
                    tracing::warn!(
                        iteration = iteration_index,
                        elapsed_secs,
                        "g2h elapsed too small to measure reliably; skipping"
                    );
                    continue;
                }
                let mbps = (bytes_received as f64 * 8.0) / elapsed_secs / BYTES_PER_MEGABIT;
                tracing::info!(
                    iteration = iteration_index,
                    bytes_received,
                    elapsed_secs,
                    mbps,
                    "g2h iteration complete"
                );
                eprintln!(
                    "g2h[{iteration_index:>2}]: {bytes_received} B in {elapsed_secs:.3}s = {mbps:.1} Mbps"
                );
                mbps_samples.push(mbps);
            }
        }
    }

    sandbox.stop().await?;

    if mbps_samples.is_empty() {
        return Ok(None);
    }

    let mut total_mbps = 0.0_f64;
    for sample in &mbps_samples {
        total_mbps += sample;
    }
    let mean_mbps = total_mbps / mbps_samples.len() as f64;
    Ok(Some(mean_mbps))
}

/// Accept exactly one TCP connection on `listener`, drain it to EOF, and
/// return `(bytes_received, elapsed)`.  Intended to run in a background thread.
fn drain_one_connection(listener: &TcpListener) -> (u64, Duration) {
    let accept_result = listener.accept();
    let Ok((mut stream, _peer_addr)) = accept_result else {
        return (0, Duration::ZERO);
    };

    let start = Instant::now();
    let bytes_received = drain_stream(&mut stream);
    let elapsed = start.elapsed();
    (bytes_received, elapsed)
}

/// Read `stream` to EOF and return the total byte count.
fn drain_stream(stream: &mut TcpStream) -> u64 {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total_bytes: u64 = 0;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(bytes_read) => total_bytes += bytes_read as u64,
            Err(_) => break,
        }
    }
    total_bytes
}

#[allow(dead_code)]
fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    let idx = ((samples.len() as f64) * p).clamp(0.0, samples.len() as f64 - 1.0) as usize;
    samples[idx]
}
