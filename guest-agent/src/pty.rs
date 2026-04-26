//! Guest-side PTY session handler.
//!
//! Manages pseudo-terminal sessions requested by the host via `PtyOpen`
//! messages. Each session forks a child process under a PTY, drops
//! privileges to the sandbox user, and runs a bidirectional I/O loop
//! between the vsock connection and the PTY master fd.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};

use void_box_protocol::{
    MessageType, PtyClosedResponse, PtyOpenRequest, PtyOpenedResponse, PtyResizeRequest,
    HEADER_SIZE, MAX_MESSAGE_SIZE,
};

use crate::{kmsg, RESOURCE_LIMITS};

/// Tracks the number of active PTY sessions (max [`MAX_PTY_SESSIONS`]).
static PTY_SESSION_COUNT: AtomicU32 = AtomicU32::new(0);

/// Maximum number of concurrent PTY sessions per VM.
const MAX_PTY_SESSIONS: u32 = 4;

/// Buffer size for reads from the PTY master fd.
const PTY_READ_BUF_SIZE: usize = 4096;

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

    pty_io_loop(fd, request_id, master_fd, pid);

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
        kmsg(&format!(
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
            kmsg(&format!(
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

    let path_for_log = std::env::var("PATH").unwrap_or_default();
    unsafe {
        libc::execvp(program_c.as_ptr(), argv_ptrs.as_ptr());
        // execvp returned — that only happens on failure. Capture errno
        // before any other libc call clobbers it so /dev/kmsg shows the
        // exact reason. Diagnostic for "child exits 127 instead of N" CI
        // flake (Azure nested-virt only).
        let err = io::Error::last_os_error();
        kmsg(&format!(
            "PTY child: execvp({:?}, {:?}) failed: {} (raw_os_error={:?}); PATH={:?}; _exit(127)",
            request.program,
            request.args,
            err,
            err.raw_os_error(),
            path_for_log,
        ));
        libc::_exit(127);
    }
}

/// Runs the bidirectional I/O loop between the vsock fd and the PTY master fd.
///
/// A reader thread handles vsock-to-master forwarding (PtyData, PtyResize,
/// PtyClose). The current thread handles master-to-vsock forwarding and
/// child reaping.
fn pty_io_loop(vsock_fd: RawFd, request_id: u32, master_fd: RawFd, child_pid: libc::pid_t) {
    let reader_handle = std::thread::spawn(move || {
        pty_reader_thread(vsock_fd, master_fd, child_pid);
    });

    let exit_code = pty_writer_loop(vsock_fd, request_id, master_fd, child_pid);

    let resp = PtyClosedResponse { exit_code };
    let _ = send_json_message(vsock_fd, MessageType::PtyClosed, request_id, &resp);

    kmsg(&format!(
        "PTY: session ended pid={} exit_code={}",
        child_pid, exit_code
    ));

    let _ = reader_handle.join();
}

/// Reads from the PTY master fd and sends PtyData messages to the host.
/// On EOF, waits for the child to exit and returns the exit code.
fn pty_writer_loop(
    vsock_fd: RawFd,
    request_id: u32,
    master_fd: RawFd,
    child_pid: libc::pid_t,
) -> i32 {
    let mut buf = [0u8; PTY_READ_BUF_SIZE];

    loop {
        let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

        if n <= 0 {
            break;
        }

        let data = &buf[..n as usize];
        if send_pty_data(vsock_fd, request_id, data).is_err() {
            break;
        }
    }

    reap_child(child_pid)
}

/// Reads framed messages from the vsock fd and dispatches PtyData writes
/// to the master fd. Handles PtyResize and PtyClose. On vsock EOF or error,
/// sends SIGHUP to the child process.
fn pty_reader_thread(vsock_fd: RawFd, master_fd: RawFd, child_pid: libc::pid_t) {
    loop {
        let mut header = [0u8; HEADER_SIZE];
        if read_exact_raw(vsock_fd, &mut header).is_err() {
            unsafe {
                libc::kill(child_pid, libc::SIGHUP);
            }
            return;
        }

        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg_type_byte = header[4];

        if length > MAX_MESSAGE_SIZE {
            unsafe {
                libc::kill(child_pid, libc::SIGHUP);
            }
            return;
        }

        let mut payload = vec![0u8; length];
        if length > 0 && read_exact_raw(vsock_fd, &mut payload).is_err() {
            unsafe {
                libc::kill(child_pid, libc::SIGHUP);
            }
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
                unsafe {
                    libc::kill(child_pid, libc::SIGHUP);
                }
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
