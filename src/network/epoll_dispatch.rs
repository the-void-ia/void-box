//! Linux epoll-driven readiness dispatch for SLIRP host sockets.
//!
//! Owns one `epoll_fd` plus a self-pipe.  Callers register socket FDs
//! with a `FlowToken` (a 64-bit identifier the dispatcher returns on
//! readiness).  The poll thread calls `wait_with_timeout` to block
//! until any registered FD is ready or the timeout fires, then drains
//! the events into a caller-owned buffer.
//!
//! Why no crate? The standard `mio`/`tokio` story would pull in a
//! reactor + a runtime — Phase 6.4 needs neither.  `libc::epoll_*`
//! is two syscalls, fully observable, and the surface fits in ~150
//! lines.  See plan 2026-04-30-smoltcp-passt-port-phase6.4.md
//! "Architecture notes" for the rationale.

// Task 7 will wire these types into SlirpBackend; allow dead_code until then.
#![allow(dead_code)]

use std::io;
#[cfg(test)]
use std::os::fd::{AsRawFd, RawFd};
use std::os::fd::{FromRawFd, OwnedFd};

/// Opaque per-FD identifier the caller uses to look up which flow a
/// readiness event belongs to.  Encoded into `epoll_data.u64`.
pub type FlowToken = u64;

/// One readiness event, mapped from `libc::epoll_event`.
#[derive(Debug, Clone, Copy)]
pub struct EpollEvent {
    pub token: FlowToken,
    pub readable: bool,
    pub writable: bool,
}

#[derive(Debug)]
pub struct EpollDispatch {
    epoll_fd: OwnedFd,
}

impl EpollDispatch {
    /// Create a new epoll instance with `EPOLL_CLOEXEC`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `epoll_create1` returns -1 on error and a valid fd
        // otherwise.  We wrap into OwnedFd so Drop closes it.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let epoll_fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { epoll_fd })
    }

    #[cfg(test)]
    fn epoll_fd_for_test(&self) -> RawFd {
        self.epoll_fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_new_creates_epoll_fd() {
        let dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        assert!(dispatch.epoll_fd_for_test() >= 0);
    }
}
