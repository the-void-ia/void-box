//! Host-side PTY session that bridges terminal I/O to a guest PTY over vsock.
//!
//! [`PtySession`] owns a vsock connection to the guest agent. After opening,
//! it enters an interactive I/O loop: host stdin is read on a writer thread
//! and forwarded as `PtyData` frames, while the main thread reads `PtyData`
//! and `PtyClosed` frames from the guest.

use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use crate::guest::protocol::{
    Message, MessageType, PtyClosedResponse, PtyOpenRequest, PtyOpenedResponse,
};
use crate::{Error, Result};

use super::control_channel::{connect_with_handshake_sync, GuestConnector};

/// RAII guard that puts the host terminal into raw mode on creation and
/// restores the original settings on drop.
pub struct RawModeGuard {
    original: libc::termios,
    fd: RawFd,
}

impl RawModeGuard {
    /// Puts file descriptor `fd` into raw mode and returns a guard that
    /// restores the original terminal settings on drop.
    pub fn engage(fd: RawFd) -> io::Result<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { original, fd })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// An open PTY session connected to a guest agent over vsock.
///
/// Created by [`PtySession::open`], which performs the connect+handshake
/// and sends a `PtyOpen` request. Call [`PtySession::run`] to enter the
/// interactive I/O loop.
pub struct PtySession {
    stream: Box<dyn super::control_channel::GuestStream>,
}

impl PtySession {
    /// Connects to the guest agent, handshakes, sends a `PtyOpen` request,
    /// and waits for the `PtyOpened` response.
    pub fn open(
        connector: &GuestConnector,
        session_secret: &[u8; 32],
        boot_wait_done: &std::sync::atomic::AtomicBool,
        request: &PtyOpenRequest,
    ) -> Result<Self> {
        let mut stream = connect_with_handshake_sync(
            connector,
            session_secret,
            boot_wait_done,
            Duration::from_secs(3),
            "pty-open",
        )?;

        let msg_bytes = Message {
            msg_type: MessageType::PtyOpen,
            payload: serde_json::to_vec(request)?,
        }
        .serialize();

        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        stream
            .write_all(&msg_bytes)
            .map_err(|e| Error::Guest(format!("failed to send PtyOpen: {e}")))?;

        debug!("pty_session: sent PtyOpen, waiting for PtyOpened");

        let response_msg = Message::read_from_sync(&mut *stream)?;
        if response_msg.msg_type != MessageType::PtyOpened {
            return Err(Error::Guest(format!(
                "expected PtyOpened, got {:?}",
                response_msg.msg_type
            )));
        }

        let response: PtyOpenedResponse = serde_json::from_slice(&response_msg.payload)?;
        if !response.success {
            return Err(Error::Guest(format!(
                "guest refused PtyOpen: {}",
                response.error.unwrap_or_default()
            )));
        }

        let _ = stream.set_read_timeout(None);

        debug!("pty_session: PtyOpened successfully");
        Ok(Self { stream })
    }

    /// Enters the interactive I/O loop, returning the PTY process exit code.
    ///
    /// Spawns a writer thread that reads host stdin and sends `PtyData`
    /// frames to the guest. The calling thread reads frames from the guest:
    /// `PtyData` bytes are written to stdout, `PtyClosed` terminates the
    /// loop and returns the exit code.
    pub fn run(self) -> Result<i32> {
        let fd = self.stream.as_raw_fd();
        let write_fd = unsafe { libc::dup(fd) };
        if write_fd < 0 {
            return Err(Error::Guest(format!(
                "failed to dup vsock fd: {}",
                io::Error::last_os_error()
            )));
        }

        let done = Arc::new(AtomicBool::new(false));
        let done_writer = Arc::clone(&done);

        let writer_handle = std::thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                if done_writer.load(Ordering::Relaxed) {
                    break;
                }
                let n = match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let msg = Message {
                    msg_type: MessageType::PtyData,
                    payload: buf[..n].to_vec(),
                };
                let serialized = msg.serialize();
                let ret = unsafe {
                    libc::write(
                        write_fd,
                        serialized.as_ptr() as *const libc::c_void,
                        serialized.len(),
                    )
                };
                if ret < 0 {
                    break;
                }
            }

            let close_msg = Message {
                msg_type: MessageType::PtyClose,
                payload: Vec::new(),
            };
            let serialized = close_msg.serialize();
            unsafe {
                libc::write(
                    write_fd,
                    serialized.as_ptr() as *const libc::c_void,
                    serialized.len(),
                );
                libc::close(write_fd);
            }
        });

        let mut stream = self.stream;
        let mut stdout = io::stdout().lock();
        let exit_code;

        loop {
            let msg = match Message::read_from_sync(&mut *stream) {
                Ok(m) => m,
                Err(e) => {
                    done.store(true, Ordering::Relaxed);
                    let _ = writer_handle.join();
                    return Err(Error::Guest(format!("pty read error: {e}")));
                }
            };
            match msg.msg_type {
                MessageType::PtyData => {
                    let _ = stdout.write_all(&msg.payload);
                    let _ = stdout.flush();
                }
                MessageType::PtyClosed => {
                    let resp: PtyClosedResponse = serde_json::from_slice(&msg.payload)
                        .unwrap_or(PtyClosedResponse { exit_code: -1 });
                    exit_code = resp.exit_code;
                    break;
                }
                MessageType::ExecRequest
                | MessageType::ExecResponse
                | MessageType::Ping
                | MessageType::Pong
                | MessageType::Shutdown
                | MessageType::FileTransfer
                | MessageType::FileTransferResponse
                | MessageType::TelemetryData
                | MessageType::TelemetryAck
                | MessageType::SubscribeTelemetry
                | MessageType::WriteFile
                | MessageType::WriteFileResponse
                | MessageType::MkdirP
                | MessageType::MkdirPResponse
                | MessageType::ExecOutputChunk
                | MessageType::ExecOutputAck
                | MessageType::SnapshotReady
                | MessageType::ReadFile
                | MessageType::ReadFileResponse
                | MessageType::FileStat
                | MessageType::FileStatResponse
                | MessageType::PtyOpen
                | MessageType::PtyOpened
                | MessageType::PtyResize
                | MessageType::PtyClose => {
                    debug!(
                        "pty_session: ignoring unexpected message {:?}",
                        msg.msg_type
                    );
                }
            }
        }

        done.store(true, Ordering::Relaxed);
        let _ = writer_handle.join();

        debug!("pty_session: finished with exit_code={exit_code}");
        Ok(exit_code)
    }
}
