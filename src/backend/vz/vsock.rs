//! VZ virtio-socket connection adapter.
//!
//! `VZVirtioSocketConnection.fileDescriptor()` returns a raw `c_int` fd.
//! This module wraps that fd in a `VzSocketStream` that implements
//! [`GuestStream`], making it usable with the transport-agnostic
//! `ControlChannel`.
//!
//! The implementation is nearly identical to how `VsockStream` works for
//! AF_VSOCK on Linux — both are just thin wrappers around `libc::read`,
//! `libc::write`, and `setsockopt(SO_RCVTIMEO)`.

use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;
use std::time::Duration;

use crate::backend::control_channel::GuestStream;

/// A stream wrapping a raw file descriptor from `VZVirtioSocketConnection`.
///
/// Implements `Read`, `Write`, and [`GuestStream`] for use with
/// `ControlChannel`.
///
/// The fd is owned by the ObjC `VZVirtioSocketConnection` object.
/// Callers must ensure the connection outlives this stream.
pub struct VzSocketStream {
    fd: RawFd,
}

impl VzSocketStream {
    /// Wrap an existing fd from `VZVirtioSocketConnection.fileDescriptor()`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `fd` is a valid, open file descriptor that
    /// remains valid for the lifetime of this struct.
    pub unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }
}

impl Read for VzSocketStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl Write for VzSocketStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl GuestStream for VzSocketStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let tv = match timeout {
            Some(d) => libc::timeval {
                tv_sec: d.as_secs() as libc::time_t,
                tv_usec: d.subsec_micros() as libc::suseconds_t,
            },
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        let ret = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
