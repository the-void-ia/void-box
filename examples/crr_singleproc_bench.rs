//! crr_singleproc_bench — voidbox-side N-iteration TCP CRR loop in a
//! single guest process, isolating voidbox's NAT-path cost from the
//! existing bench's per-iteration `nc` fork+exec overhead.
//!
//! NOT meant for the production bench surface; this is a one-off
//! diagnostic that pairs with `tools/perf-harness/crr-client.c` + the
//! pasta side of the head-to-head.  Compile and run directly:
//!
//!     gcc -O2 -static -o /tmp/crr-client tools/perf-harness/crr-client.c
//!     cargo run --release --example crr_singleproc_bench -- \
//!         --iterations 100 --bench-binary /tmp/crr-client
//!
//! Requires the same env vars as voidbox-network-bench:
//!   VOID_BOX_KERNEL, VOID_BOX_INITRAMFS

use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use clap::Parser;
use void_box::backend::MountConfig;
use void_box::sandbox::Sandbox;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Number of CRR iterations.
    #[arg(long, default_value_t = 100)]
    iterations: u32,
    /// Host path to the static crr-client binary.
    #[arg(long, default_value = "/tmp/crr-client")]
    bench_binary: String,
    /// Memory size for the guest VM (MB).
    #[arg(long, default_value_t = 1024)]
    memory_mb: usize,
}

const HOST_LOOPBACK_FROM_GUEST: &str = "10.0.2.2";

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

    let iterations = cli.iterations;
    let server_thread = thread::spawn(move || {
        // Non-blocking accept with a tight poll, deadline-checked.  With
        // a blocking accept the deadline never fires if the guest never
        // connects (boot failure, SLIRP rate limit, etc.) and the
        // example's later `server_thread.join()` would hang forever.
        // The accept-pickup latency directly inflates each guest CRR
        // sample, so the wait is kept short — `from_micros(50)` adds
        // at most ~50 µs of jitter on top of a ~280 µs baseline, while
        // still letting the deadline check fire every ~50 µs.
        let mut accepted = 0u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while accepted < iterations && std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut conn, _)) => {
                    conn.set_nonblocking(false).ok();
                    let mut buf = [0u8; 1];
                    let _ = std::io::Read::read(&mut conn, &mut buf);
                    let _ = std::io::Write::write_all(&mut conn, b"x");
                    accepted += 1;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_micros(50));
                }
                Err(_) => break,
            }
        }
        accepted
    });

    let sandbox = Sandbox::local()
        .from_env()?
        .memory_mb(cli.memory_mb)
        .network(true)
        // Production SLIRP defaults (50/s rate, 64 concurrent) are
        // sized to throttle a guest-side flood — far below what a
        // CRR microbench wants.  Lift both ceilings so the bench
        // exercises the steady-state NAT path, not the rate limiter.
        .network_max_connections_per_second(u32::MAX)
        .network_max_concurrent_connections(usize::MAX)
        .mount(MountConfig {
            host_path: bench_binary_dir.clone(),
            guest_path: "/tmp/host".into(),
            read_only: true,
        })
        .build()?;

    eprintln!(
        "VM booted; running {} CRRs in a single guest process...",
        iterations
    );
    let probe = sandbox.exec("sh", &["-c", ":"]).await?;
    if !probe.success() {
        return Err("VM probe exec failed".into());
    }

    let cmd = format!(
        "/tmp/host/{name} {host} {port} {n}",
        name = bench_binary_name,
        host = HOST_LOOPBACK_FROM_GUEST,
        port = host_port,
        n = iterations,
    );
    let output = sandbox.exec("sh", &["-c", &cmd]).await?;
    let stdout = output.stdout_str().to_string();
    let stderr = output.stderr_str().to_string();
    if !output.success() {
        eprintln!("guest stderr: {stderr}");
        return Err(format!("guest exec failed: {:?}", output.exit_code).into());
    }

    let server_thread_count = server_thread.join().unwrap_or(0);
    eprintln!("host accepts: {server_thread_count}/{iterations}");

    let line = stdout.lines().next().ok_or("empty guest stdout")?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() != 4 {
        return Err(format!("unexpected guest stdout: {line:?}").into());
    }
    let n: u32 = parts[0].parse()?;
    let p50_ns: u64 = parts[1].parse()?;
    let p99_ns: u64 = parts[2].parse()?;
    let mean_ns: u64 = parts[3].parse()?;

    println!();
    println!("voidbox single-process CRR over {n} iterations:");
    println!("  p50:  {} µs", p50_ns / 1000);
    println!("  p99:  {} µs", p99_ns / 1000);
    println!("  mean: {} µs", mean_ns / 1000);

    Ok(())
}
