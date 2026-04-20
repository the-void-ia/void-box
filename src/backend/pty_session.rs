//! Host-side PTY session that bridges terminal I/O to a guest PTY over vsock.
//!
//! [`PtySession`] owns a vsock connection to the guest agent. After opening,
//! it enters an interactive I/O loop driven by `poll(2)`: host stdin is
//! forwarded as `PtyData` frames while guest `PtyData` and `PtyClosed`
//! frames are read from the vsock stream.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::{read, write, Errno};
use rustix::termios::{tcgetattr, tcgetwinsize, tcsetattr, OptionalActions, Termios};
use signal_hook::consts::signal::SIGWINCH;
use signal_hook::{flag as signal_flag, low_level, SigId};
use tracing::{debug, warn};

use crate::guest::protocol::{
    Message, MessageType, PtyClosedResponse, PtyOpenRequest, PtyOpenedResponse, PtyResizeRequest,
};
use crate::{Error, Result};

use super::control_channel::{connect_with_handshake_sync, GuestConnector};
use super::multiplex::{build_frame, decode_payload};

/// Fixed multiplex request_id for PTY sessions.
///
/// A PTY owns its entire connection, so there is no concurrent RPC to
/// multiplex against. Every outgoing and incoming frame on this
/// connection reuses this one id, which keeps the wire layout uniform
/// with the RPC channel.
const PTY_REQUEST_ID: u32 = 1;

/// Poll timeout for the interactive PTY relay loop.
///
/// Keeps resize and shutdown responsive without waking too aggressively.
const PTY_POLL_TIMEOUT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 100_000_000,
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum StdinReadAction {
    Retry,
    Eof,
    Data(usize),
}

fn borrow_fd<'fd>(fd: RawFd) -> BorrowedFd<'fd> {
    // Safety: callers ensure `fd` stays open for the duration of the immediate use.
    unsafe { BorrowedFd::borrow_raw(fd) }
}

/// RAII guard that puts the host terminal into raw mode on creation and
/// restores the original settings on drop.
pub struct RawModeGuard {
    original: Termios,
    tty_fd: RawFd,
}

impl RawModeGuard {
    /// Puts file descriptor `fd` into raw mode and returns a guard that
    /// restores the original terminal settings on drop.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if `tcgetattr` or `tcsetattr` fails.
    pub fn engage(tty_fd: RawFd) -> io::Result<Self> {
        let original = tcgetattr(borrow_fd(tty_fd)).map_err(io_error_from_errno)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(borrow_fd(tty_fd), OptionalActions::Now, &raw).map_err(io_error_from_errno)?;
        Ok(Self { original, tty_fd })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = tcsetattr(borrow_fd(self.tty_fd), OptionalActions::Now, &self.original);
    }
}

struct SigwinchGuard {
    pending: Arc<AtomicBool>,
    signal_id: SigId,
}

impl SigwinchGuard {
    fn install() -> io::Result<Self> {
        let pending = Arc::new(AtomicBool::new(false));
        let signal_id =
            signal_flag::register(SIGWINCH, Arc::clone(&pending)).map_err(io::Error::other)?;
        Ok(Self { pending, signal_id })
    }

    fn pending(&self) -> &AtomicBool {
        &self.pending
    }
}

impl Drop for SigwinchGuard {
    fn drop(&mut self) {
        let _ = low_level::unregister(self.signal_id);
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
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the connection, handshake, or `PtyOpen`
    /// exchange fails, or if the guest rejects the request.
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

        let msg_bytes = build_frame(
            MessageType::PtyOpen,
            PTY_REQUEST_ID,
            &serde_json::to_vec(request)?,
        );

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

        let Some((_id, response_body)) = decode_payload(&response_msg.payload) else {
            return Err(Error::Guest(
                "PtyOpened payload too short for multiplex request_id".into(),
            ));
        };

        let response: PtyOpenedResponse = serde_json::from_slice(response_body)?;
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
    /// Uses a single `poll(2)` loop over host stdin and the guest vsock.
    /// `PtyData` bytes from stdin are forwarded to the guest; guest
    /// `PtyData` bytes are written to stdout; `PtyClosed` terminates the
    /// loop and returns the exit code.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if polling, stdin reads, protocol reads,
    /// or vsock writes fail during the session.
    pub fn run(self) -> Result<i32> {
        let mut stream = self.stream;
        let vsock_fd = stream.as_raw_fd();
        let stdin_fd = libc::STDIN_FILENO;
        let mut stdout = io::stdout().lock();
        let mut stdin_buf = [0u8; 4096];
        let mut stdin_closed = false;
        let mut close_sent = false;
        let sigwinch_guard = SigwinchGuard::install()
            .map_err(|e| Error::Guest(format!("failed to install SIGWINCH handler: {e}")))?;

        loop {
            if sigwinch_guard.pending().swap(false, Ordering::Relaxed) {
                let (cols, rows) = terminal_size(stdout.as_raw_fd())?;
                debug!("pty_session: sending resize to guest: cols={cols} rows={rows}");
                send_resize(vsock_fd, cols, rows)
                    .map_err(|e| Error::Guest(format!("failed to send PtyResize: {e}")))?;
            }

            let mut pollfds = [
                PollFd::from_borrowed_fd(
                    borrow_fd(stdin_fd),
                    if stdin_closed {
                        PollFlags::empty()
                    } else {
                        PollFlags::IN
                    },
                ),
                PollFd::from_borrowed_fd(borrow_fd(vsock_fd), PollFlags::IN),
            ];

            let ready_count = match poll(&mut pollfds, Some(&PTY_POLL_TIMEOUT)) {
                Ok(ready_count) => ready_count,
                Err(Errno::INTR) => continue,
                Err(err) => {
                    let err = io_error_from_errno(err);
                    return Err(Error::Guest(format!("pty poll failed: {err}")));
                }
            };
            if ready_count == 0 {
                continue;
            }

            if !stdin_closed
                && pollfds[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
            {
                match classify_stdin_read_result(read(borrow_fd(stdin_fd), &mut stdin_buf)) {
                    Ok(StdinReadAction::Retry) => continue,
                    Ok(StdinReadAction::Data(bytes_read)) => {
                        let data_frame = build_frame(
                            MessageType::PtyData,
                            PTY_REQUEST_ID,
                            &stdin_buf[..bytes_read],
                        );
                        write_all_fd(vsock_fd, &data_frame)
                            .map_err(|e| Error::Guest(format!("failed to send PtyData: {e}")))?;
                    }
                    Ok(StdinReadAction::Eof) => {
                        stdin_closed = true;
                        if !close_sent {
                            let close_frame =
                                build_frame(MessageType::PtyClose, PTY_REQUEST_ID, &[]);
                            write_all_fd(vsock_fd, &close_frame).map_err(|e| {
                                Error::Guest(format!("failed to send PtyClose: {e}"))
                            })?;
                            close_sent = true;
                        }
                    }
                    Err(err) => {
                        warn!("pty_session: stdin read failed: {err}");
                        return Err(Error::Guest(format!("stdin read failed: {err}")));
                    }
                }
            }

            if pollfds[1]
                .revents()
                .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
            {
                let incoming_msg = match Message::read_from_sync(&mut *stream) {
                    Ok(incoming_msg) => incoming_msg,
                    Err(e) => return Err(Error::Guest(format!("pty read error: {e}"))),
                };
                let Some((_id, body)) = decode_payload(&incoming_msg.payload) else {
                    debug!(
                        "pty_session: dropping short payload for {:?}",
                        incoming_msg.msg_type
                    );
                    continue;
                };

                match incoming_msg.msg_type {
                    MessageType::PtyData => {
                        let _ = stdout.write_all(body);
                        let _ = stdout.flush();
                    }
                    MessageType::PtyClosed => {
                        let closed_response: PtyClosedResponse = serde_json::from_slice(body)
                            .unwrap_or(PtyClosedResponse { exit_code: -1 });
                        debug!(
                            "pty_session: finished with exit_code={}",
                            closed_response.exit_code
                        );
                        return Ok(closed_response.exit_code);
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
                            incoming_msg.msg_type
                        );
                    }
                }
            }
        }
    }
}

fn io_error_from_errno(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err.raw_os_error())
}

fn classify_stdin_read_result(
    result: std::result::Result<usize, Errno>,
) -> io::Result<StdinReadAction> {
    match result {
        Ok(0) => Ok(StdinReadAction::Eof),
        Ok(bytes_read) => Ok(StdinReadAction::Data(bytes_read)),
        Err(Errno::INTR | Errno::AGAIN) => Ok(StdinReadAction::Retry),
        Err(err) => Err(io_error_from_errno(err)),
    }
}

fn terminal_size(tty_fd: RawFd) -> Result<(u16, u16)> {
    let winsize = tcgetwinsize(borrow_fd(tty_fd)).map_err(io_error_from_errno)?;
    Ok((winsize.ws_col, winsize.ws_row))
}

fn send_resize(stream_fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let resize = PtyResizeRequest { cols, rows };
    let body =
        serde_json::to_vec(&resize).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let frame = build_frame(MessageType::PtyResize, PTY_REQUEST_ID, &body);
    write_all_fd(stream_fd, &frame)
}

fn write_all_fd(stream_fd: RawFd, buf: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        match write(borrow_fd(stream_fd), &buf[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short write on PTY stream",
                ));
            }
            Ok(n) => {
                written += n;
            }
            Err(Errno::INTR | Errno::AGAIN) => continue,
            Err(err) => return Err(io_error_from_errno(err)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_again_is_not_treated_as_fatal() {
        let action = classify_stdin_read_result(Err(Errno::AGAIN)).unwrap();
        assert_eq!(action, StdinReadAction::Retry);
    }

    #[test]
    fn stdin_data_and_eof_are_classified_correctly() {
        assert_eq!(
            classify_stdin_read_result(Ok(4)).unwrap(),
            StdinReadAction::Data(4)
        );
        assert_eq!(
            classify_stdin_read_result(Ok(0)).unwrap(),
            StdinReadAction::Eof
        );
    }
}
