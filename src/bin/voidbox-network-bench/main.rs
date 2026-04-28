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

use clap::Parser;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

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
    tcp_throughput_h2g_mbps: Option<f64>,
    tcp_rr_latency_us_p50: Option<f64>,
    tcp_rr_latency_us_p99: Option<f64>,
    tcp_crr_latency_us_p50: Option<f64>,
    udp_dns_qps: Option<f64>,
    icmp_rr_latency_us_p50: Option<f64>, // None today; populated post-Phase-1
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut report = Report::default();

    eprintln!("voidbox-network-bench: scaffold (no measurements yet)");
    let _ = (cli.iterations, &cli.output, cli.no_throughput, &mut report);

    let json = serde_json::to_string_pretty(&report)?;
    match cli.output {
        Some(path) => std::fs::write(path, json)?,
        None => println!("{json}"),
    }
    Ok(())
}

#[allow(dead_code)]
fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    let idx = ((samples.len() as f64) * p).clamp(0.0, samples.len() as f64 - 1.0) as usize;
    samples[idx]
}
