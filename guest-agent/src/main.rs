//! Guest Agent for void-box VMs
//!
//! This agent runs as the init process (PID 1) inside the micro-VM and handles:
//! - Communication with the host via vsock
//! - Command execution requests
//! - File transfers
//! - Process management

use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::RawFd;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Serialize;

// Import shared wire-format types from the protocol crate (single source of truth).
use void_box_protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, MessageType, MkdirPRequest, MkdirPResponse,
    ProcessMetrics, SystemMetrics, TelemetryBatch, WriteFileRequest, WriteFileResponse,
};

/// vsock port we listen on
const LISTEN_PORT: u32 = 1234;

/// Host CID
#[allow(dead_code)]
const HOST_CID: u32 = 2;

const ALLOWED_WRITE_ROOTS: [&str; 2] = ["/workspace", "/home"];

/// CPU jiffies snapshot from /proc/stat
struct CpuJiffies {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
}

impl CpuJiffies {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq
    }

    fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }
}

/// Write a message to /dev/kmsg so it appears on the kernel serial console
fn kmsg(msg: &str) {
    // Write to both stderr and /dev/kmsg for maximum visibility
    eprintln!("{}", msg);
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        use std::io::Write;
        let _ = writeln!(f, "guest-agent: {}", msg);
    }
}

fn main() {
    kmsg("void-box guest agent starting...");

    // Initialize the system if we're PID 1
    if std::process::id() == 1 {
        init_system();
    }

    // Load kernel modules needed for vsock (virtio_mmio + vsock transport)
    // and virtio-net (for SLIRP networking). Must happen after init_system()
    // so filesystems are mounted, but before network setup which needs the drivers.
    load_kernel_modules();

    // Set up networking after modules are loaded (virtio_net.ko creates eth0)
    if std::process::id() == 1 {
        setup_network();
    }

    // Create vsock listener, retrying since module loading + device probe takes time
    let listener_fd = {
        let mut fd = -1i32;
        for attempt in 0..30 {
            fd = create_vsock_listener(LISTEN_PORT);
            if fd >= 0 {
                kmsg(&format!(
                    "vsock listener created on attempt {}",
                    attempt + 1
                ));
                break;
            }
            let errno = std::io::Error::last_os_error();
            kmsg(&format!(
                "vsock listener attempt {} failed: {} retrying in 200ms...",
                attempt + 1,
                errno
            ));
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        fd
    };

    if listener_fd < 0 {
        kmsg("Failed to create vsock listener after retries, entering idle loop (PID 1 must not exit)");
        // PID 1 must never exit or the kernel panics
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    kmsg(&format!("Listening on vsock port {}", LISTEN_PORT));

    // Accept connections and handle requests (multi-threaded for concurrent telemetry + exec)
    loop {
        let client_fd =
            unsafe { libc::accept(listener_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if client_fd < 0 {
            eprintln!("Accept failed");
            continue;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("conn".into())
            .spawn(move || {
                if let Err(e) = handle_connection(client_fd) {
                    eprintln!("Connection error: {}", e);
                }
                unsafe {
                    libc::close(client_fd);
                }
            })
        {
            eprintln!("Failed to spawn connection thread: {}", e);
        }
    }
}

/// Initialize the system when running as init (PID 1)
fn init_system() {
    // Set PATH early - as PID 1, we inherit no environment
    std::env::set_var("PATH", "/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin");
    std::env::set_var("HOME", "/root");
    std::env::set_var("TERM", "linux");

    kmsg("Running as init, setting up system...");

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

    // Create /workspace for user projects and /home/sandbox for the sandbox user
    let _ = std::fs::create_dir_all("/workspace");
    let _ = std::fs::create_dir_all("/home/sandbox");
    // Make them writable by uid 1000 (sandbox user)
    unsafe {
        let workspace = std::ffi::CString::new("/workspace").unwrap();
        libc::chown(workspace.as_ptr(), 1000, 1000);
        let home = std::ffi::CString::new("/home/sandbox").unwrap();
        libc::chown(home.as_ptr(), 1000, 1000);
    }

    // Create /etc for resolv.conf
    let _ = std::fs::create_dir_all("/etc");

    // Configure YAMA ptrace scope to allow same-UID ptrace
    // ptrace_scope=0: classic ptrace permissions (same UID can ptrace)
    // This allows ripgrep and debuggers to work in the sandbox
    match std::fs::write("/proc/sys/kernel/yama/ptrace_scope", "0\n") {
        Ok(()) => kmsg("Configured YAMA ptrace_scope=0"),
        Err(e) => {
            // Non-fatal: kernel may not have YAMA compiled in
            kmsg(&format!("Note: Could not configure YAMA: {}", e));
        }
    }

    // Note: network setup is done after module loading in main(), not here,
    // because virtio_net.ko creates eth0 and must be loaded first.

    kmsg("System initialization complete");
}

/// Load kernel modules required for virtio-mmio and vsock.
/// Modules are expected in /lib/modules/ as .ko.xz files.
/// Uses the finit_module(2) syscall which handles compressed modules.
fn load_kernel_modules() {
    // Load order matters: dependencies must be loaded first.
    // virtio_mmio needs explicit device= params since the cmdline params may not
    // be forwarded when loading as a module.
    let modules: &[(&str, &str)] = &[
        (
            "virtio_mmio.ko",
            "device=512@0xd0000000:10 device=512@0xd0800000:11",
        ),
        // vsock modules
        ("vsock.ko", ""),
        ("vmw_vsock_virtio_transport_common.ko", ""),
        ("vmw_vsock_virtio_transport.ko", ""),
        // Network modules (for SLIRP networking)
        ("failover.ko", ""),
        ("net_failover.ko", ""),
        ("virtio_net.ko", ""),
    ];

    for (module_name, params) in modules {
        let path = format!("/lib/modules/{}", module_name);
        match load_module_file(&path, params) {
            Ok(()) => kmsg(&format!(
                "Loaded module: {} (params='{}')",
                module_name, params
            )),
            Err(e) => kmsg(&format!("WARNING: failed to load {}: {}", module_name, e)),
        }
    }

    // Give the kernel a moment to probe devices after module loading
    kmsg("Modules loaded, waiting 1s for device probe...");
    std::thread::sleep(std::time::Duration::from_secs(1));
}

/// Load a single kernel module using finit_module(2), with optional parameters.
fn load_module_file(path: &str, params: &str) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {}", path, e))?;

    let fd = file.as_raw_fd();
    let params_c = CString::new(params).unwrap();

    // finit_module(fd, params, flags) - syscall 313 on x86_64
    let ret = unsafe { libc::syscall(libc::SYS_finit_module, fd, params_c.as_ptr(), 0i32) };

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        // EEXIST means module already loaded - that's fine
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(format!("finit_module: {}", err));
    }
    Ok(())
}

/// Set up network interface for SLIRP networking
/// SLIRP network layout:
/// - Guest IP:  10.0.2.15/24
/// - Gateway:   10.0.2.2
/// - DNS:       10.0.2.3
fn setup_network() {
    eprintln!("Setting up network...");

    // Wait briefly for eth0 to appear (virtio-net driver initialization)
    for i in 0..10 {
        if std::path::Path::new("/sys/class/net/eth0").exists() {
            eprintln!("eth0 detected after {} attempts", i + 1);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Check if eth0 exists
    if !std::path::Path::new("/sys/class/net/eth0").exists() {
        eprintln!("Warning: eth0 not found, networking may not be available");
        return;
    }

    // Bring up loopback interface
    run_cmd("ip", &["link", "set", "lo", "up"]);

    // Bring up eth0
    run_cmd("ip", &["link", "set", "eth0", "up"]);

    // Configure IP address (SLIRP guest IP: 10.0.2.15/24)
    run_cmd("ip", &["addr", "add", "10.0.2.15/24", "dev", "eth0"]);

    // Add default route via SLIRP gateway (10.0.2.2)
    run_cmd("ip", &["route", "add", "default", "via", "10.0.2.2"]);

    // Configure DNS resolver (SLIRP DNS: 10.0.2.3)
    match std::fs::write(
        "/etc/resolv.conf",
        "nameserver 10.0.2.3\nnameserver 8.8.8.8\n",
    ) {
        Ok(()) => kmsg("Wrote /etc/resolv.conf"),
        Err(e) => kmsg(&format!("Failed to write /etc/resolv.conf: {}", e)),
    }

    kmsg("Network configured: 10.0.2.15/24, gw 10.0.2.2, dns 10.0.2.3");
}

/// Run a command and log the result
fn run_cmd(program: &str, args: &[&str]) {
    match Command::new(program).args(args).output() {
        Ok(output) => {
            if !output.status.success() {
                eprintln!(
                    "Warning: {} {:?} failed: {}",
                    program,
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to run {} {:?}: {}", program, args, e);
        }
    }
}

/// Create a vsock listener socket
fn create_vsock_listener(port: u32) -> RawFd {
    let socket_fd =
        unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };

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
        unsafe {
            libc::close(socket_fd);
        }
        return -1;
    }

    let ret = unsafe { libc::listen(socket_fd, 5) };
    if ret < 0 {
        unsafe {
            libc::close(socket_fd);
        }
        return -1;
    }

    socket_fd
}

/// Handle a connection – process messages in a loop until the peer disconnects
/// or a terminal message (Shutdown) is received.
fn handle_connection(fd: RawFd) -> Result<(), String> {
    loop {
        // Read message header (4 bytes length + 1 byte type)
        let mut header = [0u8; 5];
        match read_exact(fd, &mut header) {
            Ok(()) => {}
            Err(_) => {
                // Connection closed by peer (normal)
                return Ok(());
            }
        }

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

                let response = execute_command(fd, &request);
                send_response(fd, MessageType::ExecResponse, &response)?;
            }
            3 => {
                // Ping
                send_response(fd, MessageType::Pong, &())?;
                // Continue loop - host will send ExecRequest on same connection
            }
            5 => {
                // Shutdown
                eprintln!("Shutdown requested");
                unsafe {
                    libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF as i32);
                }
                return Ok(());
            }
            10 => {
                // SubscribeTelemetry - enter streaming mode
                kmsg("Telemetry subscription started");
                telemetry_stream_loop(fd);
                return Ok(());
            }
            11 => {
                // WriteFile - native file write (no shell/base64 needed)
                let request: WriteFileRequest = serde_json::from_slice(&payload)
                    .map_err(|e| format!("Failed to parse WriteFileRequest: {}", e))?;
                let response = handle_write_file(&request);
                send_response(fd, MessageType::WriteFileResponse, &response)?;
            }
            13 => {
                // MkdirP - create directories
                let request: MkdirPRequest = serde_json::from_slice(&payload)
                    .map_err(|e| format!("Failed to parse MkdirPRequest: {}", e))?;
                let response = handle_mkdir_p(&request);
                send_response(fd, MessageType::MkdirPResponse, &response)?;
            }
            _ => {
                eprintln!("Unknown message type: {}", msg_type);
            }
        }
    }
}

/// Execute a command, streaming stdout/stderr chunks via ExecOutputChunk
/// messages, then return the final ExecResponse with full accumulated output.
fn execute_command(fd: RawFd, request: &ExecRequest) -> ExecResponse {
    let start = std::time::Instant::now();

    let mut cmd = Command::new(&request.program);
    cmd.args(&request.args);

    // Ensure PATH includes common binary locations
    let path =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/sbin".to_string());
    if !path.contains("/usr/local/bin") {
        cmd.env("PATH", format!("/usr/local/bin:{}", path));
    } else {
        cmd.env("PATH", &path);
    }

    // Child processes run as uid=1000 (sandbox user) but inherit HOME=/root
    // from init. Since /root is not writable by uid=1000, set HOME to the
    // sandbox user's home directory so tools like claude-code can write to
    // $HOME/.claude/ for config and cache.
    cmd.env("HOME", "/home/sandbox");

    // Set environment variables from request (may override PATH and HOME above)
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

    // Drop privileges to sandbox user (uid=1000, gid=1000) for child processes.
    // This is required because claude-code refuses --dangerously-skip-permissions as root.
    // The guest-agent (PID 1) stays root, but child commands run as sandbox user.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            // Set supplementary groups to empty
            libc::setgroups(0, std::ptr::null());
            // Drop to gid 1000, uid 1000
            if libc::setgid(1000) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(1000) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Spawn the process
    let mut child = match cmd.spawn() {
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

    // Write stdin if provided, then close
    if !request.stdin.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&request.stdin);
        }
    }

    // Take stdout/stderr pipes for streaming
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Wrap fd in a Mutex so both streaming threads can send chunks safely
    // without interleaving wire-format messages.
    let fd_mutex = Arc::new(Mutex::new(fd));

    let fd_for_stdout = fd_mutex.clone();
    let stdout_handle =
        std::thread::spawn(move || stream_pipe(fd_for_stdout, stdout_pipe, "stdout"));

    let fd_for_stderr = fd_mutex.clone();
    let stderr_handle =
        std::thread::spawn(move || stream_pipe(fd_for_stderr, stderr_pipe, "stderr"));

    // Wait for process to exit
    let exit_code = match child.wait() {
        Ok(status) => status.code().unwrap_or(-1),
        Err(e) => {
            // Still join threads to get whatever output was collected
            let stdout_bytes = stdout_handle.join().unwrap_or_default();
            let stderr_bytes = stderr_handle.join().unwrap_or_default();
            let duration_ms = start.elapsed().as_millis() as u64;
            return ExecResponse {
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                exit_code: -1,
                error: Some(format!("Failed to wait for process: {}", e)),
                duration_ms: Some(duration_ms),
            };
        }
    };

    // Collect accumulated output from streaming threads
    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();

    let duration_ms = start.elapsed().as_millis() as u64;

    ExecResponse {
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        exit_code,
        error: None,
        duration_ms: Some(duration_ms),
    }
}

/// Read from a pipe and send ExecOutputChunk messages as data arrives.
/// Returns the full accumulated output for the final ExecResponse.
fn stream_pipe(fd: Arc<Mutex<RawFd>>, pipe: Option<impl Read>, stream_name: &str) -> Vec<u8> {
    let mut accumulated = Vec::new();
    let mut seq = 0u64;
    let mut buf = [0u8; 4096];

    if let Some(mut pipe) = pipe {
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    accumulated.extend_from_slice(&buf[..n]);
                    let chunk = ExecOutputChunk {
                        stream: stream_name.to_string(),
                        data: buf[..n].to_vec(),
                        seq,
                    };
                    // Best-effort: if send fails, we still accumulate for the
                    // final ExecResponse so backward compat is preserved.
                    if let Ok(locked_fd) = fd.lock() {
                        let _ = send_response(*locked_fd, MessageType::ExecOutputChunk, &chunk);
                    }
                    seq += 1;
                }
                Err(_) => break,
            }
        }
    }
    accumulated
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
fn send_response<T: Serialize>(
    fd: RawFd,
    msg_type: MessageType,
    payload: &T,
) -> Result<(), String> {
    let payload_bytes =
        serde_json::to_vec(payload).map_err(|e| format!("Failed to serialize response: {}", e))?;

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

// ---------------------------------------------------------------------------
// Native file operations (no shell/base64 required)
// ---------------------------------------------------------------------------

/// Handle a WriteFile request: write content directly to the guest filesystem.
/// Runs as root (no privilege drop) since this is host-initiated provisioning.
/// After writing, the file and its parent directories are chowned to uid 1000
/// so the sandbox user can read them (e.g., when claudio runs as uid 1000).
fn handle_write_file(request: &WriteFileRequest) -> WriteFileResponse {
    let target = Path::new(&request.path);
    if !is_allowed_guest_path(target) {
        return WriteFileResponse {
            success: false,
            error: Some(format!(
                "Refusing write outside allowed roots {:?}: {}",
                ALLOWED_WRITE_ROOTS, request.path
            )),
        };
    }

    // Create parent directories if requested
    if request.create_parents {
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return WriteFileResponse {
                    success: false,
                    error: Some(format!(
                        "Failed to create parent dirs {}: {}",
                        parent.display(),
                        e
                    )),
                };
            }
            // Make parent directories readable by sandbox user (uid 1000)
            chown_recursive(parent);
        }
    }

    // Write the file content
    match std::fs::write(&request.path, &request.content) {
        Ok(()) => {
            // Make the file readable by sandbox user (uid 1000)
            let c_path = std::ffi::CString::new(request.path.as_str()).unwrap_or_default();
            unsafe {
                libc::chown(c_path.as_ptr(), 1000, 1000);
                // Ensure file is world-readable
                libc::chmod(c_path.as_ptr(), 0o644);
            }
            kmsg(&format!(
                "Wrote {} bytes to {}",
                request.content.len(),
                request.path
            ));
            WriteFileResponse {
                success: true,
                error: None,
            }
        }
        Err(e) => WriteFileResponse {
            success: false,
            error: Some(format!("Failed to write {}: {}", request.path, e)),
        },
    }
}

/// Recursively chown a path and its parents to uid 1000:1000.
/// Only affects directories that are owned by root.
fn chown_recursive(path: &std::path::Path) {
    if !is_allowed_guest_path(path) {
        return;
    }

    let mut current = path.to_path_buf();
    loop {
        if !is_allowed_guest_path(&current) {
            break;
        }
        chown_dir_if_root_owned(&current);

        if is_allowed_root(&current) {
            break;
        }
        match current.parent() {
            Some(p) if p != current => {
                current = p.to_path_buf();
            }
            _ => break,
        }
    }
}

/// Handle a MkdirP request: create directories recursively.
/// Runs as root (no privilege drop) since this is host-initiated provisioning.
/// After creating, directories are chowned to uid 1000 so the sandbox user
/// can access them.
fn handle_mkdir_p(request: &MkdirPRequest) -> MkdirPResponse {
    let target = Path::new(&request.path);
    if !is_allowed_guest_path(target) {
        return MkdirPResponse {
            success: false,
            error: Some(format!(
                "Refusing mkdir outside allowed roots {:?}: {}",
                ALLOWED_WRITE_ROOTS, request.path
            )),
        };
    }

    match std::fs::create_dir_all(&request.path) {
        Ok(()) => {
            chown_recursive(target);
            kmsg(&format!("Created directory {}", request.path));
            MkdirPResponse {
                success: true,
                error: None,
            }
        }
        Err(e) => MkdirPResponse {
            success: false,
            error: Some(format!(
                "Failed to create directory {}: {}",
                request.path, e
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Telemetry: procfs parsing and streaming
// ---------------------------------------------------------------------------

/// Stream telemetry data to the host until the connection drops.
fn telemetry_stream_loop(fd: RawFd) {
    let mut seq: u64 = 0;
    let mut prev_cpu = read_cpu_jiffies();

    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));

        let curr_cpu = read_cpu_jiffies();
        let cpu_percent = compute_cpu_percent(&prev_cpu, &curr_cpu);
        prev_cpu = curr_cpu;

        let (memory_used_bytes, memory_total_bytes) = read_meminfo();
        let (net_rx_bytes, net_tx_bytes) = read_netdev();
        let procs_running = read_procs_running();
        let open_fds = read_open_fds();
        let processes = collect_process_metrics();

        let batch = TelemetryBatch {
            seq,
            timestamp_ms: unix_millis(),
            system: Some(SystemMetrics {
                cpu_percent,
                memory_used_bytes,
                memory_total_bytes,
                net_rx_bytes,
                net_tx_bytes,
                procs_running,
                open_fds,
            }),
            processes,
            trace_context: None,
        };

        if send_response(fd, MessageType::TelemetryData, &batch).is_err() {
            kmsg("Telemetry subscription ended (write error)");
            return;
        }

        seq += 1;
    }
}

/// Read aggregate CPU jiffies from the first line of /proc/stat.
fn read_cpu_jiffies() -> CpuJiffies {
    let default = CpuJiffies {
        user: 0,
        nice: 0,
        system: 0,
        idle: 0,
        iowait: 0,
        irq: 0,
        softirq: 0,
    };
    let content = match std::fs::read_to_string("/proc/stat") {
        Ok(c) => c,
        Err(_) => return default,
    };
    // First line: "cpu  user nice system idle iowait irq softirq ..."
    let line = match content.lines().next() {
        Some(l) => l,
        None => return default,
    };
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 8 || fields[0] != "cpu" {
        return default;
    }
    let parse = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    CpuJiffies {
        user: parse(1),
        nice: parse(2),
        system: parse(3),
        idle: parse(4),
        iowait: parse(5),
        irq: parse(6),
        softirq: parse(7),
    }
}

/// Compute CPU usage percentage from two jiffies snapshots.
fn compute_cpu_percent(prev: &CpuJiffies, curr: &CpuJiffies) -> f64 {
    let total_delta = curr.total().saturating_sub(prev.total());
    if total_delta == 0 {
        return 0.0;
    }
    let idle_delta = curr.idle_total().saturating_sub(prev.idle_total());
    let busy_delta = total_delta.saturating_sub(idle_delta);
    (busy_delta as f64 / total_delta as f64) * 100.0
}

/// Read memory info from /proc/meminfo. Returns (used_bytes, total_bytes).
fn read_meminfo() -> (u64, u64) {
    let content = match std::fs::read_to_string("/proc/meminfo") {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let mut mem_total_kb: u64 = 0;
    let mut mem_available_kb: u64 = 0;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            mem_total_kb = parse_meminfo_value(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            mem_available_kb = parse_meminfo_value(rest);
        }
    }
    let total = mem_total_kb * 1024;
    let used = total.saturating_sub(mem_available_kb * 1024);
    (used, total)
}

/// Parse a /proc/meminfo value line like "    12345 kB" -> 12345
fn parse_meminfo_value(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Read network device stats from /proc/net/dev for eth0. Returns (rx_bytes, tx_bytes).
fn read_netdev() -> (u64, u64) {
    let content = match std::fs::read_to_string("/proc/net/dev") {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("eth0:") {
            let after_colon = &trimmed["eth0:".len()..];
            let fields: Vec<&str> = after_colon.split_whitespace().collect();
            let rx = fields
                .first()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            // tx_bytes is the 9th field (index 8) after the colon
            let tx = fields
                .get(8)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            return (rx, tx);
        }
    }
    (0, 0)
}

/// Read number of running processes from /proc/stat.
fn read_procs_running() -> u32 {
    let content = match std::fs::read_to_string("/proc/stat") {
        Ok(c) => c,
        Err(_) => return 0,
    };
    parse_procs_running(&content)
}

/// Read number of allocated file descriptors from /proc/sys/fs/file-nr.
fn read_open_fds() -> u32 {
    let content = match std::fs::read_to_string("/proc/sys/fs/file-nr") {
        Ok(c) => c,
        Err(_) => return 0,
    };
    parse_open_fds(&content)
}

/// Collect per-process metrics by scanning /proc/[0-9]*/.
fn collect_process_metrics() -> Vec<ProcessMetrics> {
    let mut processes = Vec::new();
    let page_size = page_size_bytes();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return processes,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Only numeric directories (PIDs)
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let base = format!("/proc/{}", pid);

        // Read comm
        let comm = std::fs::read_to_string(format!("{}/comm", base))
            .unwrap_or_default()
            .trim()
            .to_string();
        if comm.is_empty() {
            continue;
        }

        // Read RSS from statm (second field, in pages)
        let rss_bytes = std::fs::read_to_string(format!("{}/statm", base))
            .ok()
            .and_then(|s| {
                s.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0)
            * page_size;

        // Read state and cpu jiffies from stat
        let (state, cpu_jiffies) = read_proc_stat_fields(&base);

        processes.push(ProcessMetrics {
            pid,
            comm,
            rss_bytes,
            cpu_jiffies,
            state,
        });
    }

    processes
}

/// Read process state and CPU jiffies (utime + stime) from /proc/PID/stat.
fn read_proc_stat_fields(base: &str) -> (char, u64) {
    let content = match std::fs::read_to_string(format!("{}/stat", base)) {
        Ok(c) => c,
        Err(_) => return ('?', 0),
    };
    parse_proc_stat_fields_content(&content)
}

fn parse_proc_stat_fields_content(content: &str) -> (char, u64) {
    // /proc/PID/stat format: pid (comm) state ... utime(14) stime(15) ...
    // Find the closing ')' to skip the comm field (which may contain spaces/parens)
    let after_comm = match content.rfind(')') {
        Some(pos) => &content[pos + 1..],
        None => return ('?', 0),
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0] = state, fields[11] = utime, fields[12] = stime
    let state = fields.first().and_then(|s| s.chars().next()).unwrap_or('?');
    let utime = fields
        .get(11)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let stime = fields
        .get(12)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    (state, utime + stime)
}

fn parse_procs_running(content: &str) -> u32 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("procs_running ") {
            return rest.trim().parse::<u32>().unwrap_or(0);
        }
    }
    0
}

fn parse_open_fds(content: &str) -> u32 {
    // Format: "allocated_fds  free_fds  max_fds"
    content
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
}

fn page_size_bytes() -> u64 {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        4096
    } else {
        page_size as u64
    }
}

fn is_allowed_guest_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }

    let normalized = normalize_path(path);
    ALLOWED_WRITE_ROOTS.iter().any(|root| {
        let root_path = Path::new(root);
        normalized == root_path || normalized.starts_with(root_path)
    })
}

fn is_allowed_root(path: &Path) -> bool {
    ALLOWED_WRITE_ROOTS
        .iter()
        .any(|root| path == Path::new(root))
}

fn chown_dir_if_root_owned(path: &Path) {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.uid() != 0 {
        return;
    }

    if let Ok(c_path) = std::ffi::CString::new(path.to_string_lossy().as_ref()) {
        unsafe {
            libc::chown(c_path.as_ptr(), 1000, 1000);
            // Ensure directory is traversable
            libc::chmod(c_path.as_ptr(), 0o755);
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(seg) => normalized.push(seg),
            Component::Prefix(_) => {}
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        normalized
    }
}

/// Get current time as Unix milliseconds.
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proc_stat_fields_content_ok() {
        let line = "1234 (my(proc) name) S 1 2 3 4 5 6 7 8 9 10 100 200 0 0 0 0\n";
        let (state, jiffies) = parse_proc_stat_fields_content(line);
        assert_eq!(state, 'S');
        assert_eq!(jiffies, 300);
    }

    #[test]
    fn test_parse_proc_stat_fields_content_malformed() {
        let (state, jiffies) = parse_proc_stat_fields_content("not-a-valid-stat-line");
        assert_eq!(state, '?');
        assert_eq!(jiffies, 0);
    }

    #[test]
    fn test_parse_procs_running() {
        let content = "cpu  1 2 3 4 5 6 7 8\nprocs_running 9\n";
        assert_eq!(parse_procs_running(content), 9);
        assert_eq!(parse_procs_running("cpu 1 2 3"), 0);
    }

    #[test]
    fn test_parse_open_fds() {
        assert_eq!(parse_open_fds("123 456 789\n"), 123);
        assert_eq!(parse_open_fds("oops"), 0);
    }

    #[test]
    fn test_allowed_guest_path_policy() {
        assert!(is_allowed_guest_path(Path::new("/workspace/file.txt")));
        assert!(is_allowed_guest_path(Path::new(
            "/home/sandbox/.claude/skills/x.md"
        )));
        assert!(!is_allowed_guest_path(Path::new("/usr/local/bin/foo")));
        assert!(!is_allowed_guest_path(Path::new("/etc/hosts")));
        assert!(!is_allowed_guest_path(Path::new("relative/path")));
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(
            normalize_path(Path::new("/workspace/a/../b/./c")),
            PathBuf::from("/workspace/b/c")
        );
    }

    #[test]
    fn test_chown_recursive_stays_in_allowed_roots() {
        // Function should be a no-op for disallowed roots.
        chown_recursive(Path::new("/usr/local/bin"));
    }

    #[test]
    fn test_page_size_bytes_positive() {
        assert!(page_size_bytes() > 0);
    }
}
