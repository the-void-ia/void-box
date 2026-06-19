//! Guest-side PTY session handler.
//!
//! Manages pseudo-terminal sessions requested by the host via `PtyOpen`
//! messages. Each session forks a child process under a PTY, drops
//! privileges to the sandbox user, and runs a bidirectional I/O loop
//! between the vsock connection and the PTY master fd.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use void_box_protocol::{
    MessageType, PtyClosedResponse, PtyOpenRequest, PtyOpenedResponse, PtyResizeRequest,
    HEADER_SIZE, MAX_MESSAGE_SIZE,
};

use crate::{kmsg, kmsg_emerg, RESOURCE_LIMITS};

/// Tracks the number of active PTY sessions (max [`MAX_PTY_SESSIONS`]).
static PTY_SESSION_COUNT: AtomicU32 = AtomicU32::new(0);

/// Maximum number of concurrent PTY sessions per VM.
const MAX_PTY_SESSIONS: u32 = 4;

/// Buffer size for reads from the PTY master fd.
const PTY_READ_BUF_SIZE: usize = 4096;

/// Grace a non-interactive child gets to exit on its own after the host
/// closes the session, before the session escalates to SIGHUP.
///
/// A non-interactive session (piped or programmatic, e.g. a one-shot
/// command) reaches close because the host has no more input, not because
/// a terminal was hung up. Killing such a child the instant the session
/// closes races its own exit: a command that is already returning a status
/// would be reported as signalled (128 + SIGHUP) instead of by that status.
/// Waiting for the natural exit first makes the reported code deterministic;
/// the escalation below still bounds teardown for a child that never exits.
const NONINTERACTIVE_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Grace between SIGHUP and SIGKILL for a child that ignores the hangup.
const HANGUP_KILL_GRACE: Duration = Duration::from_secs(1);

/// Poll interval for the PTY master drain loop, in milliseconds. Bounds how
/// quickly the loop notices a close request and child exit during teardown.
const PTY_POLL_INTERVAL_MS: libc::c_int = 100;

/// Sandbox user/group id for privilege drop.
const SANDBOX_UID: libc::uid_t = 1000;
const SANDBOX_GID: libc::gid_t = 1000;

fn acquire_session() -> bool {
    loop {
        let current = PTY_SESSION_COUNT.load(Ordering::SeqCst);
        if current >= MAX_PTY_SESSIONS {
            return false;
        }
        if PTY_SESSION_COUNT
            .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
    }
}

fn release_session() {
    PTY_SESSION_COUNT.fetch_sub(1, Ordering::SeqCst);
}

/// Handles a PtyOpen message from the host.
///
/// Validates the command against the allowlist, acquires a session slot,
/// and delegates to `run_pty_session` on success.
///
/// # Errors
///
/// Returns `Err` if sending a response message over the vsock fd fails.
pub fn handle_pty_open(
    fd: RawFd,
    request_id: u32,
    request: &PtyOpenRequest,
    allowlist_check: fn(&str) -> bool,
) -> Result<(), String> {
    if !allowlist_check(&request.program) {
        kmsg(&format!("PTY: command not allowed: {}", request.program));
        let resp = PtyOpenedResponse {
            success: false,
            error: Some(format!(
                "Command '{}' is not in the allowed commands list",
                request.program
            )),
        };
        send_json_message(fd, MessageType::PtyOpened, request_id, &resp)?;
        return Ok(());
    }

    if !acquire_session() {
        kmsg("PTY: max sessions reached");
        let resp = PtyOpenedResponse {
            success: false,
            error: Some(format!("max PTY sessions ({}) reached", MAX_PTY_SESSIONS)),
        };
        send_json_message(fd, MessageType::PtyOpened, request_id, &resp)?;
        return Ok(());
    }

    let result = run_pty_session(fd, request_id, request);

    release_session();

    result
}

/// Forks a child process under a PTY and runs the bidirectional I/O loop.
fn run_pty_session(fd: RawFd, request_id: u32, request: &PtyOpenRequest) -> Result<(), String> {
    let mut master_fd: libc::c_int = -1;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_col = request.cols;
    ws.ws_row = request.rows;

    let pid = unsafe { libc::forkpty(&mut master_fd, std::ptr::null_mut(), std::ptr::null(), &ws) };

    if pid < 0 {
        let err = io::Error::last_os_error();
        kmsg(&format!("PTY: forkpty failed: {}", err));
        let resp = PtyOpenedResponse {
            success: false,
            error: Some(format!("forkpty failed: {}", err)),
        };
        send_json_message(fd, MessageType::PtyOpened, request_id, &resp)?;
        return Ok(());
    }

    if pid == 0 {
        run_pty_child(request);
    }

    kmsg(&format!(
        "PTY: session started pid={} program={}",
        pid, request.program
    ));

    let resp = PtyOpenedResponse {
        success: true,
        error: None,
    };
    send_json_message(fd, MessageType::PtyOpened, request_id, &resp)?;

    pty_io_loop(fd, request_id, master_fd, pid, request.interactive);

    unsafe {
        libc::close(master_fd);
    }

    Ok(())
}

/// Runs in the forked child process. Drops privileges, sets up environment,
/// applies resource limits, and exec's the requested program. Never returns.
fn run_pty_child(request: &PtyOpenRequest) -> ! {
    unsafe {
        if libc::setgid(SANDBOX_GID) != 0 {
            libc::_exit(126);
        }
        if libc::setuid(SANDBOX_UID) != 0 {
            libc::_exit(126);
        }
        libc::setpgid(0, 0);

        if let Some(limits) = RESOURCE_LIMITS.get() {
            let rlim_nofile = libc::rlimit {
                rlim_cur: limits.max_open_files,
                rlim_max: limits.max_open_files,
            };
            libc::setrlimit(libc::RLIMIT_NOFILE, &rlim_nofile);

            let rlim_nproc = libc::rlimit {
                rlim_cur: limits.max_processes,
                rlim_max: limits.max_processes,
            };
            libc::setrlimit(libc::RLIMIT_NPROC, &rlim_nproc);

            if !request.interactive {
                let rlim_fsize = libc::rlimit {
                    rlim_cur: limits.max_file_size,
                    rlim_max: limits.max_file_size,
                };
                libc::setrlimit(libc::RLIMIT_FSIZE, &rlim_fsize);
            }
        }
    }

    let path =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/sbin".to_string());
    let path = if path.contains("/home/sandbox/.local/bin") {
        path
    } else {
        format!("/home/sandbox/.local/bin:{}", path)
    };
    let path = if path.contains("/usr/local/bin") {
        path
    } else {
        format!("/usr/local/bin:{}", path)
    };
    std::env::set_var("PATH", &path);
    std::env::set_var("HOME", "/home/sandbox");
    std::env::set_var("TERM", "xterm-256color");

    for (key, value) in &request.env {
        std::env::set_var(key, value);
    }

    if let Some(ref dir) = request.working_dir {
        let _ = std::env::set_current_dir(dir);
    }

    let Ok(program_c) = CString::new(request.program.as_str()) else {
        kmsg_emerg(&format!(
            "PTY child: program contains NUL byte ({:?}); _exit(127)",
            request.program
        ));
        unsafe {
            libc::_exit(127);
        }
    };

    let mut argv_c: Vec<CString> = Vec::with_capacity(1 + request.args.len());
    argv_c.push(program_c.clone());
    for arg in &request.args {
        let Ok(arg_c) = CString::new(arg.as_str()) else {
            kmsg_emerg(&format!(
                "PTY child: arg contains NUL byte ({:?}); _exit(127)",
                arg
            ));
            unsafe {
                libc::_exit(127);
            }
        };
        argv_c.push(arg_c);
    }

    let mut argv_ptrs: Vec<*const libc::c_char> = Vec::with_capacity(argv_c.len() + 1);
    for c in &argv_c {
        argv_ptrs.push(c.as_ptr());
    }
    argv_ptrs.push(std::ptr::null());

    unsafe {
        libc::execvp(program_c.as_ptr(), argv_ptrs.as_ptr());
        // execvp returned — that only happens on failure. Capture errno
        // before any other libc call clobbers it so /dev/kmsg shows the
        // exact reason. Diagnostic for "child exits 127 instead of N" CI
        // flake (Azure nested-virt only). KERN_EMERG so the message
        // bypasses the guest kernel's `loglevel=0` filter and reaches
        // ttyS0.
        //
        // Deliberately minimal payload: program name (needed to identify
        // the failing exec), arg count (lets us tell apart wrong-shape
        // calls from missing-binary), errno + raw_os_error (the actual
        // reason). The full args and PATH are NOT logged — args may
        // carry user-supplied secrets (e.g. API keys passed as flags),
        // and the host serial console may be archived as an artifact.
        let err = io::Error::last_os_error();
        kmsg_emerg(&format!(
            "PTY child: execvp({:?}, argc={}) failed: {} (raw_os_error={:?}); _exit(127)",
            request.program,
            request.args.len(),
            err,
            err.raw_os_error(),
        ));
        libc::_exit(127);
    }
}

/// Runs the bidirectional I/O loop between the vsock fd and the PTY master fd.
///
/// A reader thread handles vsock-to-master forwarding (PtyData, PtyResize,
/// PtyClose) and flags `close_requested` when the host closes the session.
/// The current thread handles master-to-vsock forwarding, teardown, and
/// child reaping — keeping a single owner of the child's lifecycle so the
/// hangup signal and `waitpid` never race across threads.
fn pty_io_loop(
    vsock_fd: RawFd,
    request_id: u32,
    master_fd: RawFd,
    child_pid: libc::pid_t,
    interactive: bool,
) {
    let close_requested = Arc::new(AtomicBool::new(false));
    let reader_close = Arc::clone(&close_requested);
    let reader_handle = std::thread::spawn(move || {
        pty_reader_thread(vsock_fd, master_fd, reader_close);
    });

    let exit_code = pty_writer_loop(
        vsock_fd,
        request_id,
        master_fd,
        child_pid,
        interactive,
        &close_requested,
    );

    let resp = PtyClosedResponse { exit_code };
    let _ = send_json_message(vsock_fd, MessageType::PtyClosed, request_id, &resp);

    kmsg(&format!(
        "PTY: session ended pid={} exit_code={}",
        child_pid, exit_code
    ));

    let _ = reader_handle.join();
}

/// Drains the PTY master fd to the host and supervises child teardown.
///
/// Polls the master so the loop stays responsive to `close_requested` while
/// still forwarding output. Child output is sent as PtyData; an EOF (or read
/// error) on the master means the child closed the slave and is exiting, so
/// the loop reaps and returns its status.
///
/// When the host closes the session, the child is given a grace period to
/// exit on its own (zero for interactive sessions, where a closed terminal
/// is a genuine hangup), then escalated SIGHUP → SIGKILL if it outlives it.
fn pty_writer_loop(
    vsock_fd: RawFd,
    request_id: u32,
    master_fd: RawFd,
    child_pid: libc::pid_t,
    interactive: bool,
    close_requested: &AtomicBool,
) -> i32 {
    let exit_grace = if interactive {
        Duration::ZERO
    } else {
        NONINTERACTIVE_EXIT_GRACE
    };

    let mut buf = [0u8; PTY_READ_BUF_SIZE];
    let mut hangup_deadline: Option<Instant> = None;
    let mut hangup_sent = false;
    let mut kill_deadline: Option<Instant> = None;

    loop {
        let mut pollfd = libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, PTY_POLL_INTERVAL_MS) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        if ready > 0 && pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let n =
                unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n == 0 {
                break;
            }
            if n < 0 {
                // A signal can interrupt the blocking read; retrying keeps the
                // drain loop (and the teardown escalation below) alive instead
                // of mistaking the interruption for EOF. Any other error means
                // the master is gone, so end the session.
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            } else if send_pty_data(vsock_fd, request_id, &buf[..n as usize]).is_err() {
                break;
            }
        }

        // Once the host has closed the session, hang the child up after the
        // grace period, then SIGKILL if it ignores the hangup.
        if close_requested.load(Ordering::SeqCst) && hangup_deadline.is_none() {
            hangup_deadline = Some(Instant::now() + exit_grace);
        }
        if let Some(deadline) = hangup_deadline {
            if !hangup_sent && Instant::now() >= deadline {
                signal_child_group(child_pid, libc::SIGHUP);
                hangup_sent = true;
                kill_deadline = Some(Instant::now() + HANGUP_KILL_GRACE);
            }
        }
        if let Some(deadline) = kill_deadline {
            if Instant::now() >= deadline {
                signal_child_group(child_pid, libc::SIGKILL);
                kill_deadline = None;
            }
        }
    }

    reap_child(child_pid)
}

/// Sends `signal` to the child's entire process group.
///
/// `run_pty_child` makes the child a session and group leader (forkpty's
/// `setsid` plus `setpgid(0, 0)`), so its process-group id equals its pid.
/// Signalling the group (`-pid`) reaches any grandchildren that inherited
/// the PTY slave, so teardown stays bounded even when those outlive the
/// direct child.
fn signal_child_group(child_pid: libc::pid_t, signal: libc::c_int) {
    unsafe {
        libc::kill(-child_pid, signal);
    }
}

/// Reads framed messages from the vsock fd and dispatches PtyData writes
/// to the master fd. Handles PtyResize and PtyClose. A closed session
/// (PtyClose, vsock EOF, or a fatal protocol error) flags `close_requested`
/// so the writer loop tears the child down; the writer owns the actual
/// signalling so the hangup and reap stay on a single thread.
fn pty_reader_thread(vsock_fd: RawFd, master_fd: RawFd, close_requested: Arc<AtomicBool>) {
    loop {
        let mut header = [0u8; HEADER_SIZE];
        if read_exact_raw(vsock_fd, &mut header).is_err() {
            close_requested.store(true, Ordering::SeqCst);
            return;
        }

        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg_type_byte = header[4];

        if length > MAX_MESSAGE_SIZE {
            close_requested.store(true, Ordering::SeqCst);
            return;
        }

        let mut payload = vec![0u8; length];
        if length > 0 && read_exact_raw(vsock_fd, &mut payload).is_err() {
            close_requested.store(true, Ordering::SeqCst);
            return;
        }

        let Ok(message_type) = MessageType::try_from(msg_type_byte) else {
            continue;
        };

        // Every post-handshake frame carries a 4-byte request_id prefix
        // on its payload. PTY's reader ignores the id (the session was
        // established under one id) but must strip it before interpreting
        // the body.
        if payload.len() < 4 {
            continue;
        }
        let body = &payload[4..];

        match message_type {
            MessageType::PtyData => {
                if write_all_raw(master_fd, body).is_err() {
                    return;
                }
            }
            MessageType::PtyResize => {
                if let Ok(resize) = serde_json::from_slice::<PtyResizeRequest>(body) {
                    let ws = libc::winsize {
                        ws_col: resize.cols,
                        ws_row: resize.rows,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
                    }
                }
            }
            MessageType::PtyClose => {
                close_requested.store(true, Ordering::SeqCst);
                return;
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
            | MessageType::PtyClosed => {}
        }
    }
}

/// Waits for the child process to exit and returns its exit code.
fn reap_child(child_pid: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(child_pid, &mut status, 0) };

    if ret < 0 {
        return -1;
    }

    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}

/// Sends a PtyData message with raw bytes, prefixed by the session request_id.
fn send_pty_data(fd: RawFd, request_id: u32, data: &[u8]) -> Result<(), io::Error> {
    let length = (4 + data.len()) as u32;
    let mut frame = Vec::with_capacity(HEADER_SIZE + 4 + data.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.push(MessageType::PtyData as u8);
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(data);
    write_all_raw(fd, &frame)
}

/// Sends a JSON-serialized message with a multiplex request_id prefix.
fn send_json_message<T: serde::Serialize>(
    fd: RawFd,
    msg_type: MessageType,
    request_id: u32,
    payload: &T,
) -> Result<(), String> {
    let payload_bytes =
        serde_json::to_vec(payload).map_err(|e| format!("Failed to serialize: {}", e))?;
    let length = (4 + payload_bytes.len()) as u32;
    let mut frame = Vec::with_capacity(HEADER_SIZE + 4 + payload_bytes.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.push(msg_type as u8);
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&payload_bytes);
    write_all_raw(fd, &frame).map_err(|e| format!("Failed to write message: {}", e))
}

/// Reads exactly `buf.len()` bytes from a raw fd, looping on partial reads.
fn read_exact_raw(fd: RawFd, buf: &mut [u8]) -> Result<(), io::Error> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf[offset..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - offset,
            )
        };
        if n <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read_exact_raw: EOF or error",
            ));
        }
        offset += n as usize;
    }
    Ok(())
}

/// Writes all bytes to a raw fd, looping on partial writes.
fn write_all_raw(fd: RawFd, data: &[u8]) -> Result<(), io::Error> {
    let mut offset = 0;
    while offset < data.len() {
        let n = unsafe {
            libc::write(
                fd,
                data[offset..].as_ptr() as *const libc::c_void,
                data.len() - offset,
            )
        };
        if n <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write_all_raw: write failed",
            ));
        }
        offset += n as usize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_guard_allows_up_to_max() {
        PTY_SESSION_COUNT.store(0, Ordering::SeqCst);
        for i in 0..MAX_PTY_SESSIONS {
            assert!(acquire_session(), "acquire failed at session {}", i);
        }
        assert_eq!(PTY_SESSION_COUNT.load(Ordering::SeqCst), MAX_PTY_SESSIONS);
        assert!(!acquire_session(), "should reject when full");
        release_session();
        assert!(acquire_session(), "should allow after release");
        PTY_SESSION_COUNT.store(0, Ordering::SeqCst);
    }
}
