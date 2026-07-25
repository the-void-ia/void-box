//! virtio-vsock device for host-guest communication
//!
//! This module implements vsock (VM sockets) for communication between
//! the host and guest. The host connects to the guest using AF_VSOCK
//! with (guest_cid, port). Only one vhost-vsock backend per CID should
//! exist (VirtioVsockMmio sets the CID and owns the backend); this device
//! only performs the host-side connect using the same CID.

use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use tracing::debug;
use void_box_protocol::SessionSecret;

use crate::{Error, Result};

/// vsock port used by the guest agent
pub const GUEST_AGENT_PORT: u32 = 1234;

/// Bound on the blocking `connect(2)` to the userspace backend's Unix socket.
///
/// The backend's accept loop starts only once the guest driver reaches
/// DRIVER_OK, and a listener's accept backlog is drained only by `accept(2)`
/// — an attempt the client has already closed still occupies its slot. While
/// the guest is booting, control-channel handshake retries can therefore
/// fill the backlog, and an unbounded `connect(2)` on a full backlog blocks
/// indefinitely, where the retry loop's deadline (checked between attempts)
/// can never fire. Applying `SO_SNDTIMEO` for the duration of the connect
/// turns that into an error the retry loop handles like any other failed
/// attempt.
const UNIX_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Reserved CIDs (see vsock(7))
pub const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;
pub const VMADDR_CID_HYPERVISOR: u32 = 0;
pub const VMADDR_CID_LOCAL: u32 = 1;
pub const VMADDR_CID_HOST: u32 = 2;

/// Vsock device for host-guest communication (host side only).
/// Does not open /dev/vhost-vsock; the VirtioVsockMmio device is the single
/// vhost-vsock backend for this VM's CID.
pub struct VsockDevice {
    /// Context ID for this VM (guest CID; host uses CID 2).
    cid: u32,
    /// 32-byte session secret for vsock authentication.
    ///
    /// Sent as the Ping payload by [`ControlChannel`]; the guest-agent
    /// validates it against the secret in its kernel cmdline.
    ///
    /// [`ControlChannel`]: crate::backend::control_channel::ControlChannel
    session_secret: SessionSecret,
    /// When set, connect via AF_UNIX instead of AF_VSOCK (userspace backend).
    unix_socket_path: Option<PathBuf>,
}

impl VsockDevice {
    /// Create a new vsock device with the given CID and session secret.
    pub fn new(cid: u32) -> Result<Self> {
        Self::with_secret(cid, SessionSecret::new([0u8; 32]))
    }

    /// Create a new vsock device with the given CID and session secret.
    pub fn with_secret(cid: u32, session_secret: SessionSecret) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid CID {}: must be >= 3 (0-2 reserved)",
                cid
            )));
        }

        debug!(
            "Creating vsock device with CID {} (host connects to guest via AF_VSOCK)",
            cid
        );

        Ok(Self {
            cid,
            session_secret,
            unix_socket_path: None,
        })
    }

    /// Creates a vsock device that connects via AF_UNIX (userspace backend).
    pub fn with_unix_socket(
        cid: u32,
        session_secret: SessionSecret,
        socket_path: PathBuf,
    ) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid CID {}: must be >= 3 (0-2 reserved)",
                cid
            )));
        }
        debug!(
            "Creating vsock device with CID {} (AF_UNIX: {})",
            cid,
            socket_path.display()
        );
        Ok(Self {
            cid,
            session_secret,
            unix_socket_path: Some(socket_path),
        })
    }

    /// Get the CID for this device
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Get the session secret (for snapshot capture).
    pub fn session_secret(&self) -> &SessionSecret {
        &self.session_secret
    }

    /// Returns a [`GuestConnector`] that opens a fresh vsock connection
    /// to the guest-agent port on every call.
    ///
    /// The connector is what [`ControlChannel`] needs to establish its
    /// persistent multiplex channel: one call at first RPC (or on
    /// reconnect after death), and once on each PTY session.
    ///
    /// [`ControlChannel`]: crate::backend::control_channel::ControlChannel
    /// [`GuestConnector`]: crate::backend::control_channel::GuestConnector
    pub fn connector(
        self: &std::sync::Arc<Self>,
    ) -> crate::backend::control_channel::GuestConnector {
        let device = std::sync::Arc::clone(self);
        std::sync::Arc::new(move || {
            let stream = device.connect_to_guest(GUEST_AGENT_PORT)?;
            Ok(Box::new(stream) as Box<dyn crate::backend::control_channel::GuestStream>)
        })
    }

    /// Connects to a port on the guest.
    ///
    /// Uses AF_VSOCK by default. When `unix_socket_path` is set (userspace
    /// backend), connects via AF_UNIX and sends the port as a 4-byte LE header.
    fn connect_to_guest(&self, port: u32) -> Result<VsockStream> {
        if let Some(ref socket_path) = self.unix_socket_path {
            VsockStream::connect_unix(socket_path, port)
        } else {
            VsockStream::connect(self.cid, port)
        }
    }
}

/// Vsock stream wrapper
pub struct VsockStream {
    fd: RawFd,
}

impl VsockStream {
    /// Connect to a vsock endpoint
    pub fn connect(cid: u32, port: u32) -> Result<Self> {
        // Try to create a vsock socket
        let socket_fd =
            unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };

        if socket_fd < 0 {
            return Err(Error::Guest(format!(
                "Failed to create vsock socket: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Set up sockaddr_vm
        #[repr(C)]
        struct SockaddrVm {
            svm_family: libc::sa_family_t,
            svm_reserved1: u16,
            svm_port: u32,
            svm_cid: u32,
            svm_zero: [u8; 4],
        }

        let addr = SockaddrVm {
            svm_family: libc::AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: cid,
            svm_zero: [0; 4],
        };

        let ret = unsafe {
            libc::connect(
                socket_fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };

        if ret < 0 {
            unsafe {
                libc::close(socket_fd);
            }
            return Err(Error::Guest(format!(
                "Failed to connect to vsock {}:{}: {}",
                cid,
                port,
                std::io::Error::last_os_error()
            )));
        }

        Ok(Self { fd: socket_fd })
    }

    /// Connect via AF_UNIX to the userspace vsock backend.
    ///
    /// Sends the target guest port as a 4-byte LE header after connecting.
    ///
    /// The `connect(2)` is bounded by `UNIX_CONNECT_TIMEOUT`; the timeout
    /// is cleared once connected so the established stream keeps ordinary
    /// blocking-write semantics.
    pub fn connect_unix(socket_path: &std::path::Path, port: u32) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;

        let socket_fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if socket_fd < 0 {
            return Err(Error::Guest(format!(
                "Failed to create Unix socket for vsock backend: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mut stream = Self { fd: socket_fd };

        stream
            .set_write_timeout(Some(UNIX_CONNECT_TIMEOUT))
            .map_err(|e| {
                Error::Guest(format!(
                    "Failed to set connect timeout on vsock Unix socket: {}",
                    e
                ))
            })?;

        let path_bytes = socket_path.as_os_str().as_bytes();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        if path_bytes.len() >= addr.sun_path.len() {
            return Err(Error::Guest(format!(
                "vsock Unix socket path too long: {}",
                socket_path.display()
            )));
        }
        for (dst, src) in addr.sun_path.iter_mut().zip(path_bytes.iter()) {
            *dst = *src as libc::c_char;
        }

        let connect_result = unsafe {
            libc::connect(
                socket_fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if connect_result < 0 {
            return Err(Error::Guest(format!(
                "Failed to connect to vsock Unix socket {}: {}",
                socket_path.display(),
                std::io::Error::last_os_error()
            )));
        }

        stream.set_write_timeout(None).map_err(|e| {
            Error::Guest(format!(
                "Failed to clear connect timeout on vsock Unix socket: {}",
                e
            ))
        })?;

        let port_bytes = port.to_le_bytes();
        stream.write_all(&port_bytes).map_err(|e| {
            Error::Guest(format!("Failed to send port to vsock Unix socket: {}", e))
        })?;

        Ok(stream)
    }

    /// Duplicates the underlying file descriptor and returns a new [`VsockStream`].
    ///
    /// Both handles refer to the same underlying kernel socket, so reads
    /// and writes from different threads operate on the same connection
    /// without interfering. Used by the multiplex channel to split
    /// reader/writer halves across threads.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the `dup(2)` syscall fails.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        let dup_fd = unsafe { libc::dup(self.fd) };
        if dup_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd: dup_fd })
    }

    /// Sets the read timeout (e.g. for handshake). Pass `None` for blocking.
    pub fn set_read_timeout(&self, duration: Option<Duration>) -> std::io::Result<()> {
        self.set_socket_timeout(libc::SO_RCVTIMEO, duration)
    }

    /// Sets the write timeout, bounding each blocking wait inside a
    /// write (and, on a connecting socket, the blocking `connect(2)`).
    /// Pass `None` for blocking.
    pub fn set_write_timeout(&self, duration: Option<Duration>) -> std::io::Result<()> {
        self.set_socket_timeout(libc::SO_SNDTIMEO, duration)
    }

    fn set_socket_timeout(
        &self,
        option: libc::c_int,
        duration: Option<Duration>,
    ) -> std::io::Result<()> {
        let tv = match duration {
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            Some(d) => libc::timeval {
                tv_sec: d.as_secs() as i64,
                tv_usec: d.subsec_micros() as i64,
            },
        };
        let ret = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                option,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl AsRawFd for VsockStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cid_validation() {
        assert!(VsockDevice::new(0).is_err());
        assert!(VsockDevice::new(1).is_err());
        assert!(VsockDevice::new(2).is_err());
        // CID 3+ should be valid (may fail if /dev/vhost-vsock unavailable)
    }

    #[test]
    fn test_reserved_cids() {
        assert_eq!(VMADDR_CID_HYPERVISOR, 0);
        assert_eq!(VMADDR_CID_LOCAL, 1);
        assert_eq!(VMADDR_CID_HOST, 2);
    }

    /// A listener whose accept queue is full must fail the connect attempt
    /// rather than block it indefinitely: the userspace backend accepts only
    /// after the guest driver reaches DRIVER_OK, so during boot the control
    /// channel's handshake retries run against exactly this condition.
    #[test]
    fn connect_unix_fails_instead_of_blocking_on_full_backlog() {
        use std::os::unix::ffi::OsStrExt;

        let tmp_dir = std::env::temp_dir();
        let socket_path =
            tmp_dir.join(format!("void-box-backlog-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);

        let listener_fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(listener_fd >= 0, "listener socket creation failed");

        let path_bytes = socket_path.as_os_str().as_bytes();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        assert!(
            path_bytes.len() < addr.sun_path.len(),
            "socket path too long for sun_path; the test would silently bind a \
             truncated path and pass vacuously: {}",
            socket_path.display()
        );
        for (dst, src) in addr.sun_path.iter_mut().zip(path_bytes.iter()) {
            *dst = *src as libc::c_char;
        }
        let bind_result = unsafe {
            libc::bind(
                listener_fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        assert_eq!(bind_result, 0, "bind failed");
        let listen_result = unsafe { libc::listen(listener_fd, 0) };
        assert_eq!(listen_result, 0, "listen failed");

        let mut queued = Vec::new();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        loop {
            let path = socket_path.clone();
            let attempt_tx = result_tx.clone();
            std::thread::spawn(move || {
                let _ = attempt_tx.send(VsockStream::connect_unix(&path, GUEST_AGENT_PORT));
            });
            match result_rx.recv_timeout(UNIX_CONNECT_TIMEOUT + Duration::from_secs(5)) {
                Ok(Ok(stream)) => {
                    queued.push(stream);
                    assert!(
                        queued.len() < 64,
                        "backlog 0 accepted {} connects without an accept(2); \
                         expected the queue to fill almost immediately",
                        queued.len()
                    );
                }
                Ok(Err(_)) => break,
                Err(_) => {
                    panic!(
                        "connect_unix blocked past its timeout on a full accept \
                         backlog instead of returning an error"
                    );
                }
            }
        }

        unsafe { libc::close(listener_fd) };
        let _ = std::fs::remove_file(&socket_path);
    }
}
