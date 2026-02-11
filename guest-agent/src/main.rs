//! Guest Agent for void-box VMs
//!
//! This agent runs as the init process (PID 1) inside the micro-VM and handles:
//! - Communication with the host via vsock
//! - Command execution requests
//! - File transfers
//! - Process management

use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// vsock port we listen on
const LISTEN_PORT: u32 = 1234;

/// Host CID
const HOST_CID: u32 = 2;

/// Message types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MessageType {
    ExecRequest = 1,
    ExecResponse = 2,
    Ping = 3,
    Pong = 4,
    Shutdown = 5,
    FileTransfer = 6,
    FileTransferResponse = 7,
}

/// Request to execute a command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: Vec<u8>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub working_dir: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// Response from command execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
}

fn main() {
    eprintln!("void-box guest agent starting...");

    // Initialize the system if we're PID 1
    if std::process::id() == 1 {
        init_system();
    }

    // Create vsock listener
    let listener_fd = create_vsock_listener(LISTEN_PORT);
    if listener_fd < 0 {
        eprintln!("Failed to create vsock listener");
        return;
    }

    eprintln!("Listening on vsock port {}", LISTEN_PORT);

    // Accept connections and handle requests
    loop {
        let client_fd = unsafe { libc::accept(listener_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if client_fd < 0 {
            eprintln!("Accept failed");
            continue;
        }

        if let Err(e) = handle_connection(client_fd) {
            eprintln!("Connection error: {}", e);
        }

        unsafe { libc::close(client_fd); }
    }
}

/// Initialize the system when running as init (PID 1)
fn init_system() {
    eprintln!("Running as init, setting up system...");

    // Mount proc filesystem
    let _ = std::fs::create_dir_all("/proc");
    let proc = std::ffi::CString::new("proc").unwrap();
    let proc_path = std::ffi::CString::new("/proc").unwrap();
    unsafe {
        libc::mount(
            proc.as_ptr(),
            proc_path.as_ptr(),
            proc.as_ptr(),
            0,
            std::ptr::null(),
        );
    }

    // Mount sys filesystem
    let _ = std::fs::create_dir_all("/sys");
    let sysfs = std::ffi::CString::new("sysfs").unwrap();
    let sys_path = std::ffi::CString::new("/sys").unwrap();
    unsafe {
        libc::mount(
            sysfs.as_ptr(),
            sys_path.as_ptr(),
            sysfs.as_ptr(),
            0,
            std::ptr::null(),
        );
    }

    // Mount devtmpfs
    let _ = std::fs::create_dir_all("/dev");
    let devtmpfs = std::ffi::CString::new("devtmpfs").unwrap();
    let dev_path = std::ffi::CString::new("/dev").unwrap();
    unsafe {
        libc::mount(
            devtmpfs.as_ptr(),
            dev_path.as_ptr(),
            devtmpfs.as_ptr(),
            0,
            std::ptr::null(),
        );
    }

    // Create /tmp
    let _ = std::fs::create_dir_all("/tmp");

    eprintln!("System initialization complete");
}

/// Create a vsock listener socket
fn create_vsock_listener(port: u32) -> RawFd {
    let socket_fd = unsafe {
        libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        )
    };

    if socket_fd < 0 {
        return -1;
    }

    // Bind to the port
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
        svm_cid: 0xFFFFFFFF, // VMADDR_CID_ANY
        svm_zero: [0; 4],
    };

    let ret = unsafe {
        libc::bind(
            socket_fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
        )
    };

    if ret < 0 {
        unsafe { libc::close(socket_fd); }
        return -1;
    }

    let ret = unsafe { libc::listen(socket_fd, 5) };
    if ret < 0 {
        unsafe { libc::close(socket_fd); }
        return -1;
    }

    socket_fd
}

/// Handle a single connection
fn handle_connection(fd: RawFd) -> Result<(), String> {
    // Read message header (4 bytes length + 1 byte type)
    let mut header = [0u8; 5];
    read_exact(fd, &mut header)?;

    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let msg_type = header[4];

    // Read payload
    let mut payload = vec![0u8; length];
    if length > 0 {
        read_exact(fd, &mut payload)?;
    }

    // Handle message based on type
    match msg_type {
        1 => {
            // ExecRequest
            let request: ExecRequest = serde_json::from_slice(&payload)
                .map_err(|e| format!("Failed to parse request: {}", e))?;

            let response = execute_command(&request);
            send_response(fd, MessageType::ExecResponse, &response)?;
        }
        3 => {
            // Ping
            send_response(fd, MessageType::Pong, &())?;
        }
        5 => {
            // Shutdown
            eprintln!("Shutdown requested");
            unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF as i32); }
        }
        _ => {
            eprintln!("Unknown message type: {}", msg_type);
        }
    }

    Ok(())
}

/// Execute a command and return the response
fn execute_command(request: &ExecRequest) -> ExecResponse {
    let start = std::time::Instant::now();

    let mut cmd = Command::new(&request.program);
    cmd.args(&request.args);

    // Set environment variables
    for (key, value) in &request.env {
        cmd.env(key, value);
    }

    // Set working directory
    if let Some(ref dir) = request.working_dir {
        cmd.current_dir(dir);
    }

    // Set up stdin
    if !request.stdin.is_empty() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Spawn the process
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ExecResponse {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: -1,
                error: Some(format!("Failed to spawn process: {}", e)),
                duration_ms: None,
            };
        }
    };

    let mut child = child;

    // Write stdin if provided
    if !request.stdin.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&request.stdin);
        }
    }

    // Wait for completion
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => {
            return ExecResponse {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: -1,
                error: Some(format!("Failed to wait for process: {}", e)),
                duration_ms: None,
            };
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    ExecResponse {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code().unwrap_or(-1),
        error: None,
        duration_ms: Some(duration_ms),
    }
}

/// Read exactly `buf.len()` bytes from the socket
fn read_exact(fd: RawFd, buf: &mut [u8]) -> Result<(), String> {
    let mut total_read = 0;
    while total_read < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf[total_read..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - total_read,
            )
        };
        if n <= 0 {
            return Err("Read failed".into());
        }
        total_read += n as usize;
    }
    Ok(())
}

/// Send a response message
fn send_response<T: Serialize>(fd: RawFd, msg_type: MessageType, payload: &T) -> Result<(), String> {
    let payload_bytes = serde_json::to_vec(payload)
        .map_err(|e| format!("Failed to serialize response: {}", e))?;

    let length = payload_bytes.len() as u32;
    let mut msg = Vec::with_capacity(5 + payload_bytes.len());
    msg.extend_from_slice(&length.to_le_bytes());
    msg.push(msg_type as u8);
    msg.extend_from_slice(&payload_bytes);

    let mut total_written = 0;
    while total_written < msg.len() {
        let n = unsafe {
            libc::write(
                fd,
                msg[total_written..].as_ptr() as *const libc::c_void,
                msg.len() - total_written,
            )
        };
        if n <= 0 {
            return Err("Write failed".into());
        }
        total_written += n as usize;
    }

    Ok(())
}
