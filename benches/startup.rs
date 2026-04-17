//! Divan microbenchmarks for VM startup hot paths.
//!
//! These target pure-compute slices identified by profiling
//! (`voidbox-startup-bench` loop + perf-agent). Code that requires KVM,
//! async, or host I/O lives in the loop harness (`src/bin/voidbox-startup-bench`),
//! not here.
//!
//! Run with: `cargo bench --bench startup`

fn main() {
    divan::main();
}

// ---------------------------------------------------------------------------
// Placeholder bench group. Real benches land here after the first profile.
// ---------------------------------------------------------------------------

#[divan::bench]
fn noop(bencher: divan::Bencher) {
    bencher.bench(|| divan::black_box(1u64 + 1));
}
