//! guest-agent binary entry point.
//!
//! The agent runs as the init process (PID 1) inside the Linux micro-VM; its
//! implementation lives in the Linux-only [`agent`] module. On non-Linux
//! targets this builds as an inert stub, so a workspace build or test needs no
//! per-crate exclusion (`--exclude guest-agent`) to compile on macOS.

#[cfg(target_os = "linux")]
mod agent;

#[cfg(target_os = "linux")]
fn main() {
    agent::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "guest-agent is Linux-only: it runs as PID 1 inside the micro-VM. \
         This build is an inert stub on the current target."
    );
    std::process::exit(1);
}
