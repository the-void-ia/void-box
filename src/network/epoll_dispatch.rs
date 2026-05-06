//! Linux epoll-driven readiness dispatch for SLIRP host sockets.
//!
//! Owns one `epoll_fd` plus an eagerly-initialized self-pipe.  Callers
//! register socket FDs with a `FlowToken` (a 64-bit identifier the
//! dispatcher returns on readiness).  The poll thread calls
//! `wait_with_timeout` to block until any registered FD is ready or the
//! timeout fires, then drains the events into a caller-owned buffer.
//!
//! `EpollDispatch` is `Sync`: the Linux kernel serializes concurrent
//! `epoll_ctl` and `epoll_wait` calls on the same epoll fd internally.
//! Callers can therefore share one `Arc<EpollDispatch>` across threads
//! and call `register`/`unregister` without an outer `Mutex`, eliminating
//! the lock-contention between `wait_with_timeout` (net-poll thread) and
//! `register` (vCPU thread handling new TCP SYNs).
//!
//! Why no crate? The standard `mio`/`tokio` story would pull in a
//! reactor + a runtime that the SLIRP poll loop does not need.
//! `libc::epoll_*` is two syscalls, fully observable, and the surface
//! fits in ~200 lines.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Opaque per-FD identifier the caller uses to look up which flow a
/// readiness event belongs to.  Encoded into `epoll_data.u64`.
pub type FlowToken = u64;

/// One readiness event, mapped from `libc::epoll_event`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct EpollEvent {
    pub token: FlowToken,
    pub readable: bool,
    pub writable: bool,
}

/// Direction of interest for an `EpollDispatch::register` call.
///
/// Closed enum lets the type system reject impossible combinations (e.g.
/// "neither read nor write") at compile time and gives a clear name to
/// each mode rather than two opaque booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterMode {
    /// Wake on EPOLLIN only.
    Read,
    /// Wake on EPOLLOUT only.
    Write,
    /// Wake on either EPOLLIN or EPOLLOUT.
    ReadWrite,
}

/// Sentinel token reserved for the self-pipe wakeup mechanism.
/// Never returned to callers.
const SELF_PIPE_TOKEN: FlowToken = u64::MAX;

/// `EpollDispatch` is `Sync`: concurrent `epoll_ctl` and `epoll_wait`
/// on the same epoll fd are kernel-serialized and safe from multiple
/// threads.  The only shared state beyond the fd is `registered_count`
/// (an `AtomicUsize`) and the self-pipe (immutable after construction).
pub struct EpollDispatch {
    epoll_fd: OwnedFd,
    /// Read end of the self-pipe; registered with EPOLLIN at construction.
    read_end: OwnedFd,
    /// Cloneable waker backed by the write end of the self-pipe.
    waker_handle: Arc<OwnedFd>,
    /// Number of user-registered FDs (excludes the self-pipe).
    registered_count: AtomicUsize,
}

// SAFETY: All mutable state is either atomic or only accessed from one
// thread at a time (epoll_ctl/epoll_wait are kernel-serialized on the fd).
unsafe impl Sync for EpollDispatch {}

impl EpollDispatch {
    /// Create a new epoll instance with `EPOLL_CLOEXEC` and eagerly
    /// initialize the self-pipe so `waker()` is lock-free.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `epoll_create1` returns -1 on error and a valid fd
        // otherwise.  We wrap into OwnedFd so Drop closes it.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let epoll_fd = unsafe { OwnedFd::from_raw_fd(raw) };

        // Eagerly create the self-pipe and register its read end.
        // This avoids the lazy-init branch in the hot path and lets
        // `waker()` take `&self` instead of `&mut self`.
        let (read_fd, write_fd) = create_pipe2_nonblock_cloexec();
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: SELF_PIPE_TOKEN,
        };
        // SAFETY: epoll_ctl ADD with a valid fd and event struct.
        let epoll_ctl_result = unsafe {
            libc::epoll_ctl(
                epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                read_fd.as_raw_fd(),
                &mut ev as *mut _,
            )
        };
        if epoll_ctl_result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            epoll_fd,
            read_end: read_fd,
            waker_handle: Arc::new(write_fd),
            registered_count: AtomicUsize::new(0),
        })
    }

    /// Register `fd` with the dispatcher under `token` for the requested
    /// readiness `mode`.  `token` is opaque to the dispatcher — returned
    /// verbatim on readiness events.
    ///
    /// Thread-safe: concurrent calls with `unregister` and
    /// `wait_with_timeout` are serialized by the kernel's per-epoll-fd lock.
    pub fn register(&self, fd: RawFd, token: FlowToken, mode: RegisterMode) -> io::Result<()> {
        let events: u32 = match mode {
            RegisterMode::Read => libc::EPOLLIN as u32,
            RegisterMode::Write => libc::EPOLLOUT as u32,
            RegisterMode::ReadWrite => (libc::EPOLLIN | libc::EPOLLOUT) as u32,
        };
        let mut ev = libc::epoll_event { events, u64: token };
        // SAFETY: epoll_ctl reads `ev` for ADD; we own `fd` for the
        // lifetime of the registration (caller's contract).
        let epoll_ctl_result = unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd,
                &mut ev as *mut _,
            )
        };
        if epoll_ctl_result < 0 {
            return Err(io::Error::last_os_error());
        }
        if token != SELF_PIPE_TOKEN {
            self.registered_count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Thread-safe: concurrent calls with `register` and `wait_with_timeout`
    /// are serialized by the kernel's per-epoll-fd lock.
    pub fn unregister(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: epoll_ctl ignores the event pointer for DEL but
        // still requires it to be non-null on older kernels.
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let epoll_ctl_result = unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                &mut ev as *mut _,
            )
        };
        if epoll_ctl_result < 0 {
            return Err(io::Error::last_os_error());
        }
        self.registered_count.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    /// Returns the number of user-registered FDs (excludes the self-pipe).
    #[cfg(any(test, feature = "bench-helpers"))]
    pub(crate) fn registered_fd_count(&self) -> usize {
        self.registered_count.load(Ordering::Relaxed)
    }

    /// Block up to `timeout` for any registered FD to become ready.
    /// Drains ready events into `out` (cleared first).  Returns the
    /// number of raw kernel events (including self-pipe wakes) so callers
    /// can use it for adaptive-timeout decisions.
    ///
    /// `timeout = Duration::ZERO` is a non-blocking poll.
    ///
    /// Self-pipe events are drained to EAGAIN in-place: no extra allocation.
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

        let raw_count = n as usize;
        let mut drained_pipe = false;

        // Single pass: filter self-pipe events (draining the pipe to EAGAIN
        // on first occurrence), push real events into `out`.
        // No extra allocation: `out` was cleared at the top of this function.
        for &raw in &raw_events[..raw_count] {
            if raw.u64 == SELF_PIPE_TOKEN {
                if !drained_pipe {
                    // Drain the self-pipe to EAGAIN so EPOLLIN is not
                    // re-asserted on the next wait.  A single read is
                    // insufficient when wakes arrive faster than we drain
                    // (burst connection setup), so loop until read returns
                    // ≤ 0 or a partial fill (pipe empty).
                    let mut scratch = [0u8; 64];
                    loop {
                        // SAFETY: read from O_NONBLOCK pipe;
                        // EAGAIN / EOF terminates the loop.
                        let r = unsafe {
                            libc::read(
                                self.read_end.as_raw_fd(),
                                scratch.as_mut_ptr() as *mut _,
                                scratch.len(),
                            )
                        };
                        if r <= 0 || (r as usize) < scratch.len() {
                            break;
                        }
                    }
                    drained_pipe = true;
                }
                continue;
            }
            out.push(EpollEvent {
                token: raw.u64,
                readable: (raw.events & libc::EPOLLIN as u32) != 0,
                writable: (raw.events & libc::EPOLLOUT as u32) != 0,
            });
        }

        Ok(raw_count)
    }

    /// Returns a `Waker` that, when called, unblocks any thread
    /// currently inside `wait_with_timeout`.  The waker is cheap to
    /// clone and may be stored across threads.
    pub fn waker(&self) -> Waker {
        Waker {
            write_end: self.waker_handle.clone(),
        }
    }

    #[cfg(test)]
    fn epoll_fd_for_test(&self) -> RawFd {
        self.epoll_fd.as_raw_fd()
    }
}

/// Cloneable wakeup handle for `EpollDispatch`.  Writing one byte to
/// the underlying pipe wakes a thread blocked in `wait_with_timeout`.
#[derive(Debug, Clone)]
pub struct Waker {
    write_end: Arc<OwnedFd>,
}

impl Waker {
    pub fn wake(&self) {
        let buf = [0u8; 1];
        // SAFETY: write to a non-blocking pipe never blocks.  We
        // ignore EAGAIN — the pipe already has bytes pending, which
        // means a wakeup is already queued.
        let _ = unsafe { libc::write(self.write_end.as_raw_fd(), buf.as_ptr() as *const _, 1) };
    }
}

fn create_pipe2_nonblock_cloexec() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2 with O_NONBLOCK | O_CLOEXEC writes two fds into fds.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    assert!(rc == 0, "pipe2 failed: {}", io::Error::last_os_error());
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    (read_end, write_end)
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
        let dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        let token: FlowToken = 0xDEAD_BEEF;
        dispatch
            .register(listener.as_raw_fd(), token, RegisterMode::Read)
            .expect("register");
        dispatch
            .unregister(listener.as_raw_fd())
            .expect("unregister");
    }

    #[test]
    fn register_invalid_fd_returns_error() {
        let dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        let result = dispatch.register(-1, 0, RegisterMode::Read);
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

        let dispatch = EpollDispatch::new().expect("new");
        dispatch
            .register(stream.as_raw_fd(), 0xCAFE, RegisterMode::Read)
            .expect("register");

        let mut events: Vec<EpollEvent> = Vec::new();
        let n = dispatch
            .wait_with_timeout(&mut events, Duration::from_secs(1))
            .expect("wait");
        assert_eq!(n, 1);
        assert_eq!(events[0].token, 0xCAFE);
        assert!(events[0].readable);
    }

    #[test]
    fn wakeup_unblocks_wait_immediately() {
        use std::time::Instant;
        let dispatch = EpollDispatch::new().expect("new");
        let waker = dispatch.waker();

        // Start the wait in another thread with a long timeout.
        let wait_thread = std::thread::spawn(move || -> std::time::Duration {
            let mut events: Vec<EpollEvent> = Vec::new();
            let start = Instant::now();
            let _ = dispatch.wait_with_timeout(&mut events, Duration::from_secs(5));
            start.elapsed()
        });

        // Wake immediately.
        std::thread::sleep(Duration::from_millis(10));
        waker.wake();

        let elapsed = wait_thread.join().expect("wait thread");
        // Wait thread should return well under the 5 s timeout.
        assert!(
            elapsed < Duration::from_secs(1),
            "wait did not return on wakeup: {elapsed:?}"
        );
    }
}
