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
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

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

    /// Register `fd` with the dispatcher.  `readable`/`writable`
    /// select EPOLLIN / EPOLLOUT.  `token` is opaque to the
    /// dispatcher — returned verbatim on readiness events.
    pub fn register(
        &mut self,
        fd: RawFd,
        token: FlowToken,
        readable: bool,
        writable: bool,
    ) -> io::Result<()> {
        let mut events: u32 = 0;
        if readable {
            events |= libc::EPOLLIN as u32;
        }
        if writable {
            events |= libc::EPOLLOUT as u32;
        }
        let mut ev = libc::epoll_event { events, u64: token };
        // SAFETY: epoll_ctl reads `ev` for ADD; we own `fd` for the
        // lifetime of the registration (caller's contract).
        let rc = unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd,
                &mut ev as *mut _,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn unregister(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: epoll_ctl ignores the event pointer for DEL but
        // still requires it to be non-null on older kernels.
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let rc = unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                &mut ev as *mut _,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Block up to `timeout` for any registered FD to become ready.
    /// Drains ready events into `out` (cleared first).  Returns the
    /// number of events drained.
    ///
    /// `timeout = Duration::ZERO` is non-blocking poll;
    /// `timeout = Duration::from_secs(...)` waits up to that long.
    pub fn wait_with_timeout(
        &self,
        out: &mut Vec<EpollEvent>,
        timeout: Duration,
    ) -> io::Result<usize> {
        out.clear();

        // Pre-allocate a fixed-size event buffer.  64 ready FDs per
        // wait is more than enough for our flow counts; events not
        // returned this round will surface on the next wait.
        let mut raw_events: [libc::epoll_event; 64] = [libc::epoll_event { events: 0, u64: 0 }; 64];

        let timeout_ms: i32 = timeout.as_millis().min(i32::MAX as u128) as i32;

        // SAFETY: epoll_wait writes up to raw_events.len() entries;
        // returns -1 on error, 0 on timeout, n>0 on events.
        let n = unsafe {
            libc::epoll_wait(
                self.epoll_fd.as_raw_fd(),
                raw_events.as_mut_ptr(),
                raw_events.len() as i32,
                timeout_ms,
            )
        };
        if n < 0 {
            // EINTR is non-fatal — caller can retry on next tick.
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(0);
            }
            return Err(err);
        }
        for raw in &raw_events[..n as usize] {
            out.push(EpollEvent {
                token: raw.u64,
                readable: (raw.events & libc::EPOLLIN as u32) != 0,
                writable: (raw.events & libc::EPOLLOUT as u32) != 0,
            });
        }
        Ok(n as usize)
    }

    #[cfg(test)]
    fn epoll_fd_for_test(&self) -> RawFd {
        self.epoll_fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn dispatch_new_creates_epoll_fd() {
        let dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        assert!(dispatch.epoll_fd_for_test() >= 0);
    }

    #[test]
    fn register_then_unregister_round_trip() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let mut dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        let token: FlowToken = 0xDEAD_BEEF;
        dispatch
            .register(listener.as_raw_fd(), token, true, false)
            .expect("register");
        dispatch
            .unregister(listener.as_raw_fd())
            .expect("unregister");
    }

    #[test]
    fn register_invalid_fd_returns_error() {
        let mut dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        let result = dispatch.register(-1, 0, true, false);
        assert!(result.is_err());
    }

    #[test]
    fn wait_returns_event_when_socket_becomes_readable() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"hi").unwrap();
        });
        let stream = TcpStream::connect(addr).expect("connect");
        server.join().unwrap();

        let mut dispatch = EpollDispatch::new().expect("new");
        dispatch
            .register(stream.as_raw_fd(), 0xCAFE, true, false)
            .expect("register");

        let mut events: Vec<EpollEvent> = Vec::new();
        let n = dispatch
            .wait_with_timeout(&mut events, Duration::from_secs(1))
            .expect("wait");
        assert_eq!(n, 1);
        assert_eq!(events[0].token, 0xCAFE);
        assert!(events[0].readable);
    }
}
