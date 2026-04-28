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

use std::io::{Read, Write};
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

/// Number of RR samples collected per iteration.
const RR_SAMPLES_PER_ITER: u32 = 100;

/// Number of CRR samples collected per iteration.
const CRR_SAMPLES_PER_ITER: u32 = 30;

/// Timeout for the host-side channel receive on RR/CRR measurements.
const LATENCY_RECV_TIMEOUT: Duration = Duration::from_secs(120);

/// Window in seconds for counting DNS queries.
const DNS_QPS_WINDOW_SECS: u32 = 10;

/// SLIRP DNS resolver address inside the guest.
const SLIRP_DNS_ADDR: &str = "10.0.2.3";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "VoidBox network benchmark harness",
    long_about = "VoidBox network benchmark harness\n\
\n\
Boots one VM, exercises TCP throughput, TCP RR/CRR latency, and UDP DNS qps,\n\
then emits a JSON report suitable for automated diffing.\n\
\n\
REQUIRED ENVIRONMENT VARIABLES\n\
  VOID_BOX_KERNEL      Path to the guest kernel image (vmlinuz / vmlinux).\n\
  VOID_BOX_INITRAMFS   Path to the guest initramfs (cpio.gz).\n\
\n\
RECOMMENDED WORKFLOW — CAPTURING AND DIFFING A BASELINE\n\
  # 1. Before a refactor or networking-stack change, capture a baseline:\n\
  cargo run --bin voidbox-network-bench -- --output baseline.json\n\
\n\
  # 2. Make your change, then capture a post-change report:\n\
  cargo run --bin voidbox-network-bench -- --output after.json\n\
\n\
  # 3. Compare with diff or a JSON-diff tool:\n\
  diff baseline.json after.json\n\
  # Or with jq for a side-by-side view of individual metrics:\n\
  jq -s '.[0] as $b | .[1] as $a | {metric: keys} | .metric[] |\n\
    {metric: ., before: $b[.], after: $a[.]}' baseline.json after.json\n\
\n\
METRIC NAMES\n\
  tcp_throughput_g2h_mbps   Guest→host TCP throughput (Mbps)\n\
  tcp_rr_latency_us_p50     Persistent-connection round-trip latency p50 (µs)\n\
  tcp_rr_latency_us_p99     Persistent-connection round-trip latency p99 (µs)\n\
  tcp_crr_latency_us_p50    Connect-request-response latency p50 (µs)\n\
  udp_dns_qps               UDP DNS queries per second against SLIRP resolver\n\
\n\
The metric names mirror the columns in passt's published performance table so\n\
results can be compared directly.\n\
\n\
FAST SMOKE RUN\n\
  cargo run --bin voidbox-network-bench -- --iterations 1 --no-throughput"
)]
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

    // Boot one shared VM for all measurements that require a live guest.
    // Throughput and latency measurements reuse this single sandbox to avoid
    // paying the boot cost multiple times.
    let sandbox = Sandbox::local()
        .from_env()?
        .memory_mb(BENCH_MEMORY_MB)
        .network(true)
        .build()?;

    // Prime the VM (triggers boot + vsock handshake) before any timed work.
    let probe = sandbox.exec("sh", &["-c", ":"]).await?;
    if !probe.success() {
        return Err(format!(
            "VM probe exec failed: exit={:?} stderr={}",
            probe.exit_code,
            probe.stderr_str()
        )
        .into());
    }

    if !cli.no_throughput {
        report.tcp_throughput_g2h_mbps =
            measure_tcp_throughput_g2h(&sandbox, cli.iterations).await?;
    }

    // Latency measurements always run (--no-throughput only skips throughput).
    let (rr_p50, rr_p99) = measure_rr_latency(&sandbox, cli.iterations).await?;
    report.tcp_rr_latency_us_p50 = rr_p50;
    report.tcp_rr_latency_us_p99 = rr_p99;
    report.tcp_crr_latency_us_p50 = measure_crr_latency(&sandbox, cli.iterations).await?;
    report.udp_dns_qps = measure_dns_qps(&sandbox).await?;

    sandbox.stop().await?;

    let json = serde_json::to_string_pretty(&report)?;
    match cli.output {
        Some(path) => std::fs::write(path, json)?,
        None => println!("{json}"),
    }
    Ok(())
}

/// Measure guest-to-host TCP throughput.
///
/// Binds a host-side TCP listener on `127.0.0.1:0` and execs a BusyBox shell
/// snippet inside `sandbox` that pipes `dd` output to `nc`.  The host drain
/// thread records bytes received and wall-clock elapsed time; Mbps is computed
/// from those two numbers.  Runs `iterations` times and returns the mean.
///
/// Returns `None` if every iteration fails to parse or times out.
async fn measure_tcp_throughput_g2h(
    sandbox: &Sandbox,
    iterations: u32,
) -> Result<Option<f64>, Box<dyn std::error::Error>> {
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

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    let idx = ((samples.len() as f64) * p).clamp(0.0, samples.len() as f64 - 1.0) as usize;
    samples[idx]
}

/// Measure TCP RR (Request-Response) latency on a kept-open connection.
///
/// The guest pipes `RR_SAMPLES_PER_ITER` null bytes over a single `nc`
/// connection (`dd if=/dev/zero bs=1 count=N | nc host port`).  The host
/// accepts one connection and services each byte as an independent echo
/// round-trip, timing each host-side `read + write` pair.
///
/// Using dd+nc avoids BusyBox shell limitations around interactive TCP
/// sockets while still measuring per-message in-flight latency on a
/// persistent connection.  The first sample from each iteration is discarded
/// because the first byte arrival absorbs TCP connect and Nagle jitter from
/// the guest side.  Remaining samples are accumulated across all iterations;
/// p50 and p99 are computed over the union.
///
/// Returns `(p50_us, p99_us)`, both `None` if no samples were collected.
async fn measure_rr_latency(
    sandbox: &Sandbox,
    iterations: u32,
) -> Result<(Option<f64>, Option<f64>), Box<dyn std::error::Error>> {
    let mut all_samples: Vec<Duration> = Vec::new();

    for iteration_index in 0..iterations {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let host_port = listener.local_addr()?.port();

        let (echo_tx, echo_rx) = mpsc::channel::<Vec<Duration>>();

        std::thread::spawn(move || {
            let samples = rr_echo_server(&listener, RR_SAMPLES_PER_ITER);
            let _ = echo_tx.send(samples);
        });

        // Guest: pipe RR_SAMPLES_PER_ITER zero bytes over one nc connection.
        // dd generates the bytes; nc forwards them to the host echo server.
        // The guest does not need to read the echoed bytes — the host drives
        // the timing loop and closes when done.  BusyBox dd + nc suffice.
        let guest_cmd = format!(
            "dd if=/dev/zero bs=1 count={n} 2>/dev/null | nc {host} {port}",
            n = RR_SAMPLES_PER_ITER,
            host = SLIRP_HOST_ADDR,
            port = host_port,
        );

        let exec_result = sandbox.exec("sh", &["-c", &guest_cmd]).await;
        if let Err(exec_err) = exec_result {
            tracing::warn!(
                iteration = iteration_index,
                error = %exec_err,
                "rr iteration exec error; skipping"
            );
        }

        match echo_rx.recv_timeout(LATENCY_RECV_TIMEOUT) {
            Err(recv_err) => {
                tracing::warn!(
                    iteration = iteration_index,
                    error = %recv_err,
                    "rr echo channel receive error; skipping"
                );
            }
            Ok(mut samples) => {
                // Discard first sample (absorbs TCP connect jitter).
                if samples.len() > 1 {
                    samples.remove(0);
                }
                let count = samples.len();
                let p50_us = if count > 0 {
                    percentile(&mut samples.clone(), 0.50).as_micros()
                } else {
                    0
                };
                eprintln!("rr[{iteration_index:>2}]: {count} samples, p50={p50_us} µs");
                all_samples.extend(samples);
            }
        }
    }

    if all_samples.is_empty() {
        return Ok((None, None));
    }

    let p50 = percentile(&mut all_samples, 0.50).as_micros() as f64;
    let p99 = percentile(&mut all_samples, 0.99).as_micros() as f64;
    Ok((Some(p50), Some(p99)))
}

/// Host-side echo server for RR latency.
///
/// Accepts one connection, then for each of the `count` iterations: reads
/// one byte, times that read, writes the byte back, and records the elapsed
/// duration.  Returns the list of per-round-trip host-side durations.
///
/// The timer starts just before the blocking `read` call and stops after the
/// `write` returns.  This measures the host-observed round-trip time: the
/// interval from "host waiting for a byte" to "host has written the echo",
/// which is approximately the guest-side send→receive latency plus the
/// network stack overhead on both sides.
fn rr_echo_server(listener: &TcpListener, count: u32) -> Vec<Duration> {
    let Ok((mut stream, _)) = listener.accept() else {
        return Vec::new();
    };

    let mut samples = Vec::with_capacity(count as usize);
    let mut buf = [0u8; 1];

    for _ in 0..count {
        let start = Instant::now();
        match stream.read_exact(&mut buf) {
            Ok(()) => {}
            Err(_) => break,
        }
        match stream.write_all(&buf) {
            Ok(()) => {}
            Err(_) => break,
        }
        samples.push(start.elapsed());
    }

    samples
}

/// Measure TCP CRR (Connect-Request-Response) latency.
///
/// Each sample is one full `accept + read + write + close` cycle on the host,
/// timed from `accept` returning to the connection dropping.  The guest runs
/// a shell loop that performs `CRR_SAMPLES_PER_ITER` independent `nc` invocations
/// per iteration (each is a full connect → send → recv → close).
///
/// Host-side timing is the ground truth: the host observes when the
/// connection arrives and when it closes, so each sample faithfully captures
/// the TCP setup + data round-trip + teardown cost end-to-end.
///
/// Returns `p50_us` across all collected samples, or `None` if none arrived.
async fn measure_crr_latency(
    sandbox: &Sandbox,
    iterations: u32,
) -> Result<Option<f64>, Box<dyn std::error::Error>> {
    let mut all_samples: Vec<Duration> = Vec::new();

    for iteration_index in 0..iterations {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let host_port = listener.local_addr()?.port();

        // The host accepts CRR_SAMPLES_PER_ITER connections, times each cycle,
        // and sends results back over a channel.
        let (crr_tx, crr_rx) = mpsc::channel::<Vec<Duration>>();
        let sample_count = CRR_SAMPLES_PER_ITER;

        std::thread::spawn(move || {
            let samples = crr_echo_server(&listener, sample_count);
            let _ = crr_tx.send(samples);
        });

        // Guest: loop CRR_SAMPLES_PER_ITER times; each iteration is a full
        // nc invocation (connect → send one byte → read echo → disconnect).
        let n = CRR_SAMPLES_PER_ITER;
        let guest_cmd = format!(
            "i=0; while [ $i -lt {n} ]; do printf 'A' | nc {host} {port}; i=$((i+1)); done",
            host = SLIRP_HOST_ADDR,
            port = host_port,
            n = n,
        );

        let exec_result = sandbox.exec("sh", &["-c", &guest_cmd]).await;
        if let Err(exec_err) = exec_result {
            tracing::warn!(
                iteration = iteration_index,
                error = %exec_err,
                "crr iteration exec error; skipping"
            );
        }

        match crr_rx.recv_timeout(LATENCY_RECV_TIMEOUT) {
            Err(recv_err) => {
                tracing::warn!(
                    iteration = iteration_index,
                    error = %recv_err,
                    "crr echo channel receive error; skipping"
                );
            }
            Ok(samples) => {
                let count = samples.len();
                let p50_us = if count > 0 {
                    percentile(&mut samples.clone(), 0.50).as_micros()
                } else {
                    0
                };
                eprintln!("crr[{iteration_index:>2}]: {count} samples, p50={p50_us} µs");
                all_samples.extend(samples);
            }
        }
    }

    if all_samples.is_empty() {
        return Ok(None);
    }

    let p50 = percentile(&mut all_samples, 0.50).as_micros() as f64;
    Ok(Some(p50))
}

/// Measure UDP DNS query throughput against the SLIRP resolver.
///
/// Runs a BusyBox `sh` loop inside the guest for `DNS_QPS_WINDOW_SECS` seconds.
/// Each iteration sends a raw DNS query for `example.com` (type A) to the SLIRP
/// resolver via `nc -u` and checks whether a non-empty reply arrived, counting
/// successes.  Returns `qps = successes / window_secs`.
///
/// Using raw UDP via `nc -u` avoids a dependency on `nslookup` or `dig`, which
/// are not present in the minimal test initramfs.  The DNS query is a
/// pre-encoded fixed packet (transaction-id `0x1234`, type A, class IN);
/// the SLIRP resolver's response need only be non-empty to count as a success.
///
/// The SLIRP stack handles DNS at `10.0.2.3`; after the first query the
/// resolver's cache should absorb subsequent lookups, so the measurement
/// captures the in-stack UDP turnaround cost rather than upstream RTT.
///
/// Returns `None` on exec failure or if the guest output cannot be parsed.
async fn measure_dns_qps(sandbox: &Sandbox) -> Result<Option<f64>, Box<dyn std::error::Error>> {
    let window = DNS_QPS_WINDOW_SECS;
    let dns_addr = SLIRP_DNS_ADDR;

    // Minimal DNS query packet for "example.com" A IN (29 bytes), pre-encoded.
    // Header: txid=0x1234, flags=0x0100 (RD), qdcount=1.
    // Question: 0x07 "example" 0x03 "com" 0x00, qtype=A(1), qclass=IN(1).
    let dns_query_hex = "\\x12\\x34\\x01\\x00\\x00\\x01\\x00\\x00\\x00\\x00\\x00\\x00\
                         \\x07\\x65\\x78\\x61\\x6d\\x70\\x6c\\x65\
                         \\x03\\x63\\x6f\\x6d\\x00\\x00\\x01\\x00\\x01";

    // BusyBox nc exits as soon as its stdin reaches EOF regardless of the -w
    // timeout.  When stdin is a file (`nc < file`), nc sends the file contents
    // and exits before the UDP reply can arrive from SLIRP's async resolver.
    //
    // Fix: pipe from a subshell that sends the query bytes then immediately
    // runs `sleep 0`.  The `sleep 0` extends the pipe's lifetime by one
    // process, keeping nc's stdin open just long enough to allow the shell to
    // fork both cat and sleep before stdin closes.  After the subshell exits,
    // nc still waits up to `-w2` seconds for an incoming UDP reply.
    //
    // Timing analysis:
    //   - First query: SLIRP forwards to upstream DNS (≤100 ms typical).
    //     The reply arrives well within the 2-second -w2 window.
    //   - Subsequent queries: SLIRP serves from its 60-second cache (<1 ms).
    //     The reply arrives almost immediately.
    //   - Each iteration takes ~1 s (dominated by the -w1 timeout that fires
    //     after the reply is received and nc drains its stdin).
    //
    // The guest emits "count=<N>" on a dedicated line so the host can compute
    // a precise f64 qps without relying on integer division inside the guest.
    let guest_cmd = format!(
        "printf '{dns_query_hex}' > /tmp/_dq.bin; \
         end=$(($(date +%s) + {window})); \
         count=0; \
         while [ \"$(date +%s)\" -lt \"$end\" ]; do \
           bytes=$({{ cat /tmp/_dq.bin; sleep 0; }} | nc -u -w1 {dns_addr} 53 2>/dev/null | wc -c); \
           if [ \"$bytes\" -gt 0 ]; then \
             count=$((count + 1)); \
           fi; \
         done; \
         echo \"count=$count\""
    );

    let exec_result = sandbox.exec("sh", &["-c", &guest_cmd]).await;

    let output = match exec_result {
        Err(exec_err) => {
            tracing::warn!(error = %exec_err, "dns_qps exec error; skipping");
            return Ok(None);
        }
        Ok(output) => output,
    };

    if !output.success() {
        tracing::warn!(
            exit_code = ?output.exit_code,
            stderr = output.stderr_str(),
            "dns_qps guest command non-zero exit; skipping"
        );
        return Ok(None);
    }

    let stdout = output.stdout_str();
    tracing::debug!(
        stdout = stdout,
        stderr = output.stderr_str(),
        "dns_qps guest output"
    );

    // Parse "count=<N>" emitted by the guest; compute qps as f64 on the host
    // to avoid integer-division truncation inside the shell.
    let count_value: Option<f64> = stdout
        .lines()
        .find_map(|line| line.strip_prefix("count="))
        .and_then(|value_str| value_str.trim().parse::<f64>().ok());

    match count_value {
        Some(count) => {
            let qps = count / window as f64;
            eprintln!("dns_qps: {qps:.2} qps (count={count}, window={window}s)");
            Ok(Some(qps))
        }
        None => {
            tracing::warn!(
                stdout = stdout,
                "dns_qps: could not parse count line from guest output; skipping"
            );
            Ok(None)
        }
    }
}

/// Host-side echo server for CRR latency.
///
/// Accepts `count` independent connections in sequence.  For each: starts the
/// timer on `accept`, reads one byte, writes it back, closes the connection,
/// and stops the timer.  Returns all per-connection durations.
fn crr_echo_server(listener: &TcpListener, count: u32) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(count as usize);
    let mut buf = [0u8; 1];

    for _ in 0..count {
        let start = Instant::now();
        let Ok((mut stream, _)) = listener.accept() else {
            break;
        };
        // Read the request byte and echo it back.
        if stream.read_exact(&mut buf).is_ok() {
            let _ = stream.write_all(&buf);
        }
        // Explicit drop closes the connection.
        drop(stream);
        samples.push(start.elapsed());
    }

    samples
}
