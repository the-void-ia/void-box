//! crr_concurrent_bench — voidbox-side multi-flow TCP CRR microbench.
//!
//! Companion to `crr_singleproc_bench`.  That one isolates the
//! single-flow NAT-path floor; this one drives **M concurrent
//! crr-client processes** in the same guest so the SLIRP relay
//! sees N>1 ready flows per `net_poll_thread` cycle — the
//! workload io_uring batching, splice-zerocopy, and multi-queue
//! all need to actually win.
//!
//! Each guest-side flow runs its own `N`-iteration loop against
//! the same host listener port.  The host accepts in a single
//! thread that spawns a tiny per-connection handler (recv 1 B,
//! send 1 B, close), so up to `M` concurrent connections make
//! progress at once.
//!
//! Per-flow p50/p99 are reported alongside an aggregate
//! throughput (`M*N` iterations divided by wall-clock).
//!
//! # Examples
//!
//! ```ignore
//! gcc -O2 -static -o /tmp/crr-client tools/perf-harness/crr-client.c
//! cargo run --release --example crr_concurrent_bench -- \
//!     --concurrency 4 --iterations 100
//! ```
//!
//! Requires the same env vars as `voidbox-network-bench`:
//! `VOID_BOX_KERNEL`, `VOID_BOX_INITRAMFS`.

use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use void_box::backend::MountConfig;
use void_box::sandbox::Sandbox;

const HOST_LOOPBACK_FROM_GUEST: &str = "10.0.2.2";
const HOST_ACCEPT_DEADLINE: Duration = Duration::from_secs(120);
const HOST_ACCEPT_POLL: Duration = Duration::from_micros(50);

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Number of concurrent guest-side crr-client processes.
    #[arg(long, default_value_t = 4)]
    concurrency: u32,
    /// CRR iterations per concurrent flow (each client runs `iterations` rounds).
    #[arg(long, default_value_t = 100)]
    iterations: u32,
    /// Host path to the static crr-client binary.
    #[arg(long, default_value = "/tmp/crr-client")]
    bench_binary: String,
    /// Memory size for the guest VM (MB).
    #[arg(long, default_value_t = 1024)]
    memory_mb: usize,
    /// Number of vCPUs.  Multi-queue / multi-poll-thread experiments
    /// need >1 vCPU for the guest to spread connections across cores;
    /// single-vCPU runs are the baseline shape.
    #[arg(long, default_value_t = 1)]
    vcpus: usize,
}

#[derive(Debug, Clone, Copy)]
struct FlowSummary {
    flow_id: u32,
    iterations: u32,
    p50_ns: u64,
    p99_ns: u64,
    mean_ns: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let bench_binary = std::path::PathBuf::from(&cli.bench_binary);
    if !bench_binary.exists() {
        return Err(format!(
            "bench binary not found: {} (compile with `gcc -static -o /tmp/crr-client tools/perf-harness/crr-client.c`)",
            cli.bench_binary
        )
        .into());
    }
    let bench_binary_dir = bench_binary
        .parent()
        .ok_or("bench-binary has no parent dir")?
        .to_string_lossy()
        .into_owned();
    let bench_binary_name = bench_binary
        .file_name()
        .ok_or("bench-binary has no file name")?
        .to_string_lossy()
        .into_owned();

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let host_port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;

    let total_expected: usize = (cli.concurrency as usize) * (cli.iterations as usize);
    let accepts_done = Arc::new(AtomicUsize::new(0));

    let server_thread = thread::spawn({
        let accepts_done = Arc::clone(&accepts_done);
        move || {
            // Host accept-and-handle loop.  Spawns a per-connection
            // worker thread for each accepted socket so up to
            // `concurrency` flows can be in-flight at once — without
            // this fan-out the host serializes M clients, which
            // defeats the multi-flow signal we're trying to measure.
            let deadline = Instant::now() + HOST_ACCEPT_DEADLINE;
            while accepts_done.load(Ordering::Relaxed) < total_expected && Instant::now() < deadline
            {
                let (mut conn, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(HOST_ACCEPT_POLL);
                        continue;
                    }
                    Err(_) => break,
                };
                conn.set_nonblocking(false).ok();
                let accepts_done = Arc::clone(&accepts_done);
                thread::spawn(move || {
                    let mut buf = [0u8; 1];
                    let _ = std::io::Read::read(&mut conn, &mut buf);
                    let _ = std::io::Write::write_all(&mut conn, b"x");
                    accepts_done.fetch_add(1, Ordering::Relaxed);
                });
            }
        }
    });

    let sandbox = Sandbox::local()
        .from_env()?
        .memory_mb(cli.memory_mb)
        .vcpus(cli.vcpus)
        .network(true)
        // Same SLIRP-default lift as `crr_singleproc_bench` — at M=4
        // concurrency × 100 iterations the bench would otherwise trip
        // the 50 conn/s rate limiter and surface as a connect-refused
        // failure mid-run.
        .network_max_connections_per_second(u32::MAX)
        .network_max_concurrent_connections(usize::MAX)
        .mount(MountConfig {
            host_path: bench_binary_dir.clone(),
            guest_path: "/tmp/host".into(),
            read_only: true,
        })
        .build()?;

    eprintln!(
        "VM booted; running {} concurrent flows × {} CRRs each ({} total)...",
        cli.concurrency, cli.iterations, total_expected
    );
    let probe = sandbox.exec("sh", &["-c", ":"]).await?;
    if !probe.success() {
        return Err("VM probe exec failed".into());
    }

    // Kick off `concurrency` crr-client processes in parallel from
    // a single guest shell, each writing its own summary line into
    // a per-flow file.  `wait` blocks until every backgrounded
    // process exits; the trailing loop concatenates the M lines
    // for the host to parse.  The flow-id list is materialized on
    // the host because busybox-static (the guest shell) lacks
    // `seq`.
    let mut flow_ids = String::new();
    for flow_id in 1..=cli.concurrency {
        if !flow_ids.is_empty() {
            flow_ids.push(' ');
        }
        flow_ids.push_str(&flow_id.to_string());
    }
    let cmd = format!(
        "set -eu; rm -rf /tmp/crr_results; mkdir -p /tmp/crr_results; \
         for i in {flow_ids}; do \
             /tmp/host/{name} {host} {port} {iterations} > /tmp/crr_results/$i.txt & \
         done; \
         wait; \
         for i in {flow_ids}; do echo \"$i $(cat /tmp/crr_results/$i.txt)\"; done",
        flow_ids = flow_ids,
        name = bench_binary_name,
        host = HOST_LOOPBACK_FROM_GUEST,
        port = host_port,
        iterations = cli.iterations,
    );
    let wall_start = Instant::now();
    let output = sandbox.exec("sh", &["-c", &cmd]).await?;
    let wall_elapsed = wall_start.elapsed();
    if !output.success() {
        eprintln!("guest stderr: {}", output.stderr_str());
        return Err(format!("guest exec failed: {:?}", output.exit_code).into());
    }

    server_thread.join().unwrap_or(());
    let host_accepts = accepts_done.load(Ordering::Relaxed);
    eprintln!("host accepts: {host_accepts}/{total_expected}");

    let stdout = output.stdout_str().to_string();
    let mut flows: Vec<FlowSummary> = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!("unexpected guest stdout line: {line:?}").into());
        }
        let flow_id: u32 = parts[0].parse()?;
        let iterations: u32 = parts[1].parse()?;
        let p50_ns: u64 = parts[2].parse()?;
        let p99_ns: u64 = parts[3].parse()?;
        let mean_ns: u64 = parts[4].parse()?;
        flows.push(FlowSummary {
            flow_id,
            iterations,
            p50_ns,
            p99_ns,
            mean_ns,
        });
    }

    if flows.len() != cli.concurrency as usize {
        return Err(format!(
            "expected {} flow summaries, got {}",
            cli.concurrency,
            flows.len()
        )
        .into());
    }

    let mut p50s_us: Vec<u64> = Vec::with_capacity(flows.len());
    let mut p99s_us: Vec<u64> = Vec::with_capacity(flows.len());
    let mut means_us: Vec<u64> = Vec::with_capacity(flows.len());
    for flow in &flows {
        p50s_us.push(flow.p50_ns / 1000);
        p99s_us.push(flow.p99_ns / 1000);
        means_us.push(flow.mean_ns / 1000);
    }
    p50s_us.sort_unstable();
    p99s_us.sort_unstable();
    means_us.sort_unstable();
    let mid = p50s_us.len() / 2;
    let median_of_p50s = p50s_us[mid];
    let max_p99 = *p99s_us.last().expect("non-empty");
    let mean_of_means: u64 = means_us.iter().sum::<u64>() / (means_us.len() as u64);

    let total_iters = total_expected as f64;
    let aggregate_qps = total_iters / wall_elapsed.as_secs_f64();

    println!();
    println!(
        "voidbox concurrent CRR: {} flows × {} iterations ({:.3}s wall):",
        cli.concurrency,
        cli.iterations,
        wall_elapsed.as_secs_f64()
    );
    for flow in &flows {
        let FlowSummary {
            flow_id,
            iterations,
            p50_ns,
            p99_ns,
            mean_ns,
        } = *flow;
        println!(
            "  flow {flow_id} ({iterations} iters): p50={} µs  p99={} µs  mean={} µs",
            p50_ns / 1000,
            p99_ns / 1000,
            mean_ns / 1000,
        );
    }
    println!();
    println!("  median-of-p50s:  {median_of_p50s} µs");
    println!("  max p99:         {max_p99} µs");
    println!("  mean-of-means:   {mean_of_means} µs");
    println!("  aggregate qps:   {aggregate_qps:.0} CRRs/s");

    Ok(())
}
