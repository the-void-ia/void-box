// The `submit_recv`/`submit_send` API and surrounding types are
// scaffold for the SLIRP relay wiring that lands in the next
// commit on this experiment branch.  Until that commit, the lib
// build sees them as unused — the unit tests below exercise every
// item, so coverage is not the issue.
#![allow(dead_code)]

//! `io_uring` batching primitive for SLIRP host-socket I/O.
//!
//! Per-packet `recv` / `sendto` against host sockets is one syscall
//! per direction per packet.  On a CRR workload that's ~5 syscalls
//! per iteration on the SLIRP relay path; on bulk-throughput it
//! dominates.  This module wraps an [`IoUring`] instance the
//! relay submits batched `IORING_OP_RECV` / `IORING_OP_SEND`
//! SQEs into and drains CQEs from in a single syscall round-trip.
//!
//! # Threading
//!
//! Each [`UringBatch`] is single-owner: the SLIRP `net_poll_thread`
//! constructs and drives one.  No locking, no cross-thread sharing.
//! The relay submits a batch after each `epoll_wait` and drains it
//! before the next.
//!
//! # Why epoll stays
//!
//! The existing [`crate::network::epoll_dispatch::EpollDispatch`]
//! still owns the readiness signal — io_uring replaces only the
//! data-plane syscalls, not the wake-up.  Keeping the two layers
//! separate means io_uring can be feature-gated off without
//! touching the relay's flow-management state machine.
//!
//! [`IoUring`]: io_uring::IoUring

use std::io;
use std::os::fd::RawFd;

use io_uring::{opcode, types, IoUring};

/// Maximum SQE / CQE entries per [`UringBatch`].  Sized to comfortably
/// hold one submission per active SLIRP flow on a typical CRR cycle
/// without reallocation; oversized batches pay the kernel's submission
/// cost twice rather than failing.
const URING_QUEUE_DEPTH: u32 = 256;

/// Per-submission token tagging the kind of operation a CQE
/// completes.  Encoded into [`io_uring::squeue::Entry::user_data`]
/// so the completion drain can route a CQE back to the caller
/// without per-flow side tables.
///
/// The low 32 bits carry the caller's correlation id (typically a
/// flow token); the top 32 bits encode this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UringOp {
    /// `IORING_OP_RECV` against a host TCP / UDP socket.  CQE
    /// `result` carries the byte count or `-errno`.
    Recv,
    /// `IORING_OP_SEND` against a host TCP / UDP socket.  CQE
    /// `result` carries bytes written or `-errno`.
    Send,
}

impl UringOp {
    const TAG_RECV: u64 = 1;
    const TAG_SEND: u64 = 2;

    /// Encodes the op + correlation id as a single `u64` user-data
    /// field for an SQE.  The [`UringBatch`] inverse is
    /// [`UringBatch::decode_user_data`].
    fn encode(self, correlation_id: u32) -> u64 {
        let tag = match self {
            UringOp::Recv => Self::TAG_RECV,
            UringOp::Send => Self::TAG_SEND,
        };
        (tag << 32) | u64::from(correlation_id)
    }
}

/// Result of draining a single completion from the ring.
///
/// The caller matches on this to dispatch the bytes count (or
/// errno) back to the originating flow keyed by `correlation_id`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UringCompletion {
    /// Operation kind the SQE was tagged with.
    pub op: UringOp,
    /// Caller-supplied correlation id (e.g. SLIRP flow token).
    pub correlation_id: u32,
    /// CQE result field: positive byte count, `0` for EOF, or
    /// `-errno` on failure.  Decoded from the kernel's signed
    /// 32-bit return.
    pub result: i32,
}

/// Owns a single [`IoUring`] instance and serves as the submit /
/// complete entry point for the SLIRP relay.
///
/// # Examples
///
/// ```no_run
/// # #[cfg(all(target_os = "linux", feature = "io-uring"))] {
/// use void_box::network::uring::{UringBatch, UringOp};
/// let mut batch = UringBatch::new().expect("kernel supports io_uring");
/// let mut buf = vec![0u8; 1500];
/// // SAFETY: caller guarantees `buf` lives until the matching CQE drains.
/// unsafe {
///     batch
///         .submit_recv(/*fd=*/ 3, &mut buf, /*correlation_id=*/ 42)
///         .expect("submission queue not full");
/// }
/// batch.submit_and_wait(0).expect("kernel reachable");
/// while let Some(_completion) = batch.drain_one() { /* … */ }
/// # }
/// ```
pub(crate) struct UringBatch {
    ring: IoUring,
}

impl UringBatch {
    /// Creates a new ring sized to [`URING_QUEUE_DEPTH`].
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the kernel's
    /// `io_uring_setup` syscall fails — typically because the
    /// host kernel predates io_uring (Linux ≤ 5.0) or
    /// `kernel.io_uring_disabled` is set.
    pub(crate) fn new() -> io::Result<Self> {
        let ring = IoUring::new(URING_QUEUE_DEPTH)?;
        Ok(Self { ring })
    }

    /// Submits an `IORING_OP_RECV` against `fd` reading into `buf`.
    ///
    /// The SQE is tagged with `correlation_id` so the matching
    /// CQE drained later can be routed back to its originating
    /// flow.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when the submission
    /// queue is full — the caller submits the pending batch via
    /// [`Self::submit_and_wait`] and retries.
    ///
    /// # Safety
    ///
    /// `buf` must remain valid until the matching CQE drains via
    /// [`Self::drain_one`].  The kernel writes into the buffer
    /// asynchronously; dropping or reusing it before completion
    /// is undefined behavior.
    pub(crate) unsafe fn submit_recv(
        &mut self,
        fd: RawFd,
        buf: &mut [u8],
        correlation_id: u32,
    ) -> io::Result<()> {
        let entry = opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
            .build()
            .user_data(UringOp::Recv.encode(correlation_id));
        let mut sq = self.ring.submission();
        // SAFETY: the `Recv` SQE only references `buf`'s pointer; the
        // caller's safety contract on this fn forwards the same
        // lifetime requirement to the kernel.
        unsafe { sq.push(&entry) }.map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "io_uring submission queue full")
        })?;
        Ok(())
    }

    /// Submits an `IORING_OP_SEND` writing `buf` to `fd`.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when the submission
    /// queue is full.
    ///
    /// # Safety
    ///
    /// `buf` must remain valid until the matching CQE drains.
    pub(crate) unsafe fn submit_send(
        &mut self,
        fd: RawFd,
        buf: &[u8],
        correlation_id: u32,
    ) -> io::Result<()> {
        let entry = opcode::Send::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32)
            .build()
            .user_data(UringOp::Send.encode(correlation_id));
        let mut sq = self.ring.submission();
        // SAFETY: `Send` SQE references `buf`'s pointer for the
        // lifetime of the in-flight operation; the caller's safety
        // contract forwards that requirement to the kernel.
        unsafe { sq.push(&entry) }.map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "io_uring submission queue full")
        })?;
        Ok(())
    }

    /// Submits the queued SQEs and waits for `min_complete` CQEs
    /// to land.  `min_complete = 0` returns immediately after
    /// submission.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] from
    /// `io_uring_enter`.
    pub(crate) fn submit_and_wait(&mut self, min_complete: usize) -> io::Result<usize> {
        self.ring.submit_and_wait(min_complete)
    }

    /// Drains one completion if available.  Returns `None` when
    /// the completion queue is empty.
    pub(crate) fn drain_one(&mut self) -> Option<UringCompletion> {
        let cqe = self.ring.completion().next()?;
        Some(Self::decode_user_data(cqe.user_data(), cqe.result()))
    }

    fn decode_user_data(user_data: u64, result: i32) -> UringCompletion {
        let tag = user_data >> 32;
        let correlation_id = (user_data & 0xFFFF_FFFF) as u32;
        let op = match tag {
            UringOp::TAG_RECV => UringOp::Recv,
            UringOp::TAG_SEND => UringOp::Send,
            other => panic!("uring: unknown op tag {other} in user_data {user_data:#x}"),
        };
        UringCompletion {
            op,
            correlation_id,
            result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_round_trip_recv() {
        let encoded = UringOp::Recv.encode(0xDEAD_BEEF);
        let decoded = UringBatch::decode_user_data(encoded, 1500);
        let UringCompletion {
            op,
            correlation_id,
            result,
        } = decoded;
        assert_eq!(op, UringOp::Recv);
        assert_eq!(correlation_id, 0xDEAD_BEEF);
        assert_eq!(result, 1500);
    }

    #[test]
    fn user_data_round_trip_send() {
        let encoded = UringOp::Send.encode(0);
        let decoded = UringBatch::decode_user_data(encoded, -11);
        let UringCompletion {
            op,
            correlation_id,
            result,
        } = decoded;
        assert_eq!(op, UringOp::Send);
        assert_eq!(correlation_id, 0);
        assert_eq!(result, -11);
    }

    #[test]
    fn ring_constructs_on_supported_kernel() {
        let Ok(mut batch) = UringBatch::new() else {
            return;
        };
        let submitted = batch.submit_and_wait(0).expect("submit_and_wait succeeds");
        assert_eq!(submitted, 0);
        assert!(batch.drain_one().is_none());
    }

    /// Exercises `submit_send` + `submit_recv` end-to-end across a
    /// connected `socketpair`.  Skipped on kernels without io_uring.
    #[test]
    fn submit_send_then_recv_round_trips_via_socketpair() {
        use std::os::fd::AsRawFd;

        let Ok(mut batch) = UringBatch::new() else {
            return;
        };

        let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let send_payload: [u8; 5] = *b"hello";
        let mut recv_buf = [0u8; 16];

        // SAFETY: `send_payload` and `recv_buf` outlive the
        // submit_and_wait call below — the kernel only references
        // them while the SQEs are in flight, and we hold those
        // borrows until both CQEs land.
        unsafe {
            batch
                .submit_send(a.as_raw_fd(), &send_payload, /*correlation_id=*/ 7)
                .expect("submit_send not full");
            batch
                .submit_recv(b.as_raw_fd(), &mut recv_buf, /*correlation_id=*/ 9)
                .expect("submit_recv not full");
        }

        let completed = batch
            .submit_and_wait(2)
            .expect("kernel completes both SQEs");
        assert_eq!(completed, 2);

        let mut send_seen = false;
        let mut recv_seen = false;
        while let Some(cqe) = batch.drain_one() {
            let UringCompletion {
                op,
                correlation_id,
                result,
            } = cqe;
            match op {
                UringOp::Send => {
                    assert_eq!(correlation_id, 7);
                    assert_eq!(result, send_payload.len() as i32);
                    send_seen = true;
                }
                UringOp::Recv => {
                    assert_eq!(correlation_id, 9);
                    assert_eq!(result, send_payload.len() as i32);
                    assert_eq!(&recv_buf[..result as usize], &send_payload);
                    recv_seen = true;
                }
            }
        }
        assert!(send_seen, "send CQE drained");
        assert!(recv_seen, "recv CQE drained");
    }
}
