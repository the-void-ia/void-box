//! Guest Agent for void-box VMs
//!
//! This agent runs as the init process (PID 1) inside the micro-VM and handles:
//! - Communication with the host via vsock
//! - Command execution requests
//! - File transfers
//! - Process management

#[cfg(not(target_os = "linux"))]
compile_error!("guest-agent is Linux-only (runs as PID 1 inside the micro-VM)");

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
    ProcessMetrics, SystemMetrics, TelemetryBatch, TelemetrySubscribeRequest, WriteFileRequest,
    WriteFileResponse, MAX_MESSAGE_SIZE,
};

/// vsock port we listen on
const LISTEN_PORT: u32 = 1234;

/// Host CID
#[allow(dead_code)]
const HOST_CID: u32 = 2;

const ALLOWED_WRITE_ROOTS: [&str; 3] = ["/workspace", "/home", "/etc/voidbox"];

/// Parsed session secret from kernel cmdline (set once at startup).
static SESSION_SECRET: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
static OCI_ROOTFS_SETUP_ONCE: std::sync::Once = std::sync::Once::new();
static OCI_SETUP_STATUS: Mutex<&'static str> = Mutex::new("not-run");

// Whether the current connection has been authenticated.
// Each connection thread gets its own copy.
thread_local! {
    static AUTHENTICATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Resource limits applied to child processes via setrlimit.
#[derive(Clone, serde::Deserialize)]
struct ResourceLimits {
    max_virtual_memory: u64, // Not enforced — see comment in pre_exec
    max_open_files: u64,
    max_processes: u64, // Bun worker threads count towards NPROC on Linux
    max_file_size: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_virtual_memory: 4 * 1024 * 1024 * 1024, // 4 GB — Bun/JSC needs ~640MB for startup, more for JIT at runtime
            max_open_files: 1024,
            max_processes: 512, // Bun worker threads count towards NPROC on Linux
            max_file_size: 100 * 1024 * 1024, // 100 MB
        }
    }
}

/// Loaded resource limits (parsed from /etc/voidbox/resource_limits.json or defaults).
static RESOURCE_LIMITS: std::sync::OnceLock<ResourceLimits> = std::sync::OnceLock::new();

/// Loaded command allowlist (parsed from /etc/voidbox/allowed_commands.json or empty = allow all).
static COMMAND_ALLOWLIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

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

/// Parse the session secret from /proc/cmdline (voidbox.secret=<hex>).
fn parse_session_secret() -> Option<[u8; 32]> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    for param in cmdline.split_whitespace() {
        if let Some(hex_str) = param.strip_prefix("voidbox.secret=") {
            if hex_str.len() != 64 {
                kmsg(&format!(
                    "WARNING: voidbox.secret has wrong length: {} (expected 64 hex chars)",
                    hex_str.len()
                ));
                return None;
            }
            let mut secret = [0u8; 32];
            for i in 0..32 {
                match u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16) {
                    Ok(b) => secret[i] = b,
                    Err(_) => {
                        kmsg("WARNING: voidbox.secret contains invalid hex");
                        return None;
                    }
                }
            }
            return Some(secret);
        }
    }
    None
}

/// Set the guest system clock from the `voidbox.clock=<epoch_secs>` kernel
/// cmdline parameter.  Without this the guest starts at 1970-01-01 and TLS
/// certificate validation fails.
fn sync_clock_from_cmdline() {
    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(_) => return,
    };
    for param in cmdline.split_whitespace() {
        if let Some(secs_str) = param.strip_prefix("voidbox.clock=") {
            if let Ok(secs) = secs_str.parse::<i64>() {
                let ts = libc::timespec {
                    tv_sec: secs,
                    tv_nsec: 0,
                };
                let ret = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
                if ret == 0 {
                    kmsg(&format!("System clock set to epoch {}", secs));
                } else {
                    kmsg(&format!(
                        "WARNING: clock_settime failed (errno={})",
                        std::io::Error::last_os_error()
                    ));
                }
                return;
            }
        }
    }
}

fn main() {
    kmsg("void-box guest agent starting...");

    // Initialize the system if we're PID 1
    if std::process::id() == 1 {
        init_system();
    }

    // Set the wall clock before anything that needs accurate time (e.g. TLS).
    if std::process::id() == 1 {
        sync_clock_from_cmdline();
    }

    // Load kernel modules needed for vsock (virtio_mmio + vsock transport)
    // and virtio-net (for SLIRP networking). Must happen after init_system()
    // so filesystems are mounted, but before network setup which needs the drivers.
    load_kernel_modules();

    // Mount shared directories (virtiofs or 9p) specified via kernel cmdline.
    // Must happen after module loading (9p needs 9pnet_virtio.ko).
    mount_shared_dirs();

    // Set up networking after modules are loaded (virtio_net.ko creates eth0).
    // Skip when host did not configure a net virtio-mmio device.
    if std::process::id() == 1 {
        if network_enabled_from_cmdline() {
            setup_network();
        } else {
            kmsg("Network disabled by host config; skipping setup_network()");
        }
    }

    // Parse session secret from kernel cmdline for vsock authentication.
    match parse_session_secret() {
        Some(secret) => {
            let _ = SESSION_SECRET.set(secret);
            kmsg("Session secret loaded from kernel cmdline");
        }
        None => {
            kmsg("WARNING: No session secret found in kernel cmdline -- all connections will be rejected");
        }
    }

    // Load resource limits from config file (written by host during provisioning).
    let limits = match std::fs::read_to_string("/etc/voidbox/resource_limits.json") {
        Ok(content) => match serde_json::from_str::<ResourceLimits>(&content) {
            Ok(limits) => {
                kmsg(&format!(
                    "Loaded resource limits: AS={}MB, NOFILE={}, NPROC={}, FSIZE={}MB",
                    limits.max_virtual_memory / (1024 * 1024),
                    limits.max_open_files,
                    limits.max_processes,
                    limits.max_file_size / (1024 * 1024),
                ));
                limits
            }
            Err(e) => {
                kmsg(&format!(
                    "WARNING: Failed to parse resource_limits.json: {}, using defaults",
                    e
                ));
                ResourceLimits::default()
            }
        },
        Err(_) => {
            kmsg("Using default resource limits (no /etc/voidbox/resource_limits.json)");
            ResourceLimits::default()
        }
    };
    let _ = RESOURCE_LIMITS.set(limits);

    // Load command allowlist from config file (written by host during provisioning).
    match std::fs::read_to_string("/etc/voidbox/allowed_commands.json") {
        Ok(content) => match serde_json::from_str::<Vec<String>>(&content) {
            Ok(allowlist) => {
                kmsg(&format!(
                    "Loaded command allowlist: {} commands",
                    allowlist.len()
                ));
                let _ = COMMAND_ALLOWLIST.set(allowlist);
            }
            Err(e) => {
                kmsg(&format!(
                    "WARNING: Failed to parse allowed_commands.json: {}",
                    e
                ));
            }
        },
        Err(_) => {
            kmsg("No command allowlist loaded (no /etc/voidbox/allowed_commands.json)");
        }
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

    // Mount tmpfs on /tmp so all users can write temp files
    let _ = std::fs::create_dir_all("/tmp");
    let tmpfs = std::ffi::CString::new("tmpfs").unwrap();
    let tmp_path = std::ffi::CString::new("/tmp").unwrap();
    let tmp_opts = std::ffi::CString::new("mode=1777").unwrap();
    unsafe {
        libc::mount(
            tmpfs.as_ptr(),
            tmp_path.as_ptr(),
            tmpfs.as_ptr(),
            0,
            tmp_opts.as_ptr() as *const _,
        );
    }

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
    let virtio_mmio_params = virtio_mmio_params_from_cmdline();

    // Load order matters: dependencies must be loaded first.
    // virtio_mmio needs explicit device= params since the cmdline params may not
    // be forwarded when loading as a module.
    let modules: Vec<(&str, String, bool)> = vec![
        // virtio core modules (required when not built-in).
        ("virtio.ko", String::new(), false),
        ("virtio_ring.ko", String::new(), false),
        ("virtio_mmio.ko", virtio_mmio_params, true),
        // vsock modules
        ("vsock.ko", String::new(), true),
        ("vmw_vsock_virtio_transport_common.ko", String::new(), true),
        ("vmw_vsock_virtio_transport.ko", String::new(), true),
        // Network modules (for SLIRP networking — optional, missing on macOS)
        ("failover.ko", String::new(), false),
        ("net_failover.ko", String::new(), false),
        ("virtio_net.ko", String::new(), false),
        // 9p filesystem modules (for host directory sharing — optional, missing on macOS)
        ("9pnet.ko", String::new(), false),
        ("netfs.ko", String::new(), false),
        ("9p.ko", String::new(), false),
        ("9pnet_virtio.ko", String::new(), false),
        // overlayfs module (required for OCI rootfs writable overlay + pivot_root)
        ("overlay.ko", String::new(), false),
    ];

    for (module_name, params, required) in modules {
        let path = format!("/lib/modules/{}", module_name);
        match load_module_file(&path, &params) {
            Ok(()) => kmsg(&format!(
                "Loaded module: {} (params='{}')",
                module_name, params
            )),
            Err(e) if required => kmsg(&format!("WARNING: failed to load {}: {}", module_name, e)),
            Err(e) => kmsg(&format!(
                "Optional module {} not loaded: {}",
                module_name, e
            )),
        }
    }

    // Give the kernel a moment to probe devices after module loading
    kmsg("Modules loaded, waiting 1s for device probe...");
    std::thread::sleep(std::time::Duration::from_secs(1));
}

fn virtio_mmio_params_from_cmdline() -> String {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let mut params = Vec::new();
    for token in cmdline.split_whitespace() {
        if let Some(dev) = token.strip_prefix("virtio_mmio.device=") {
            params.push(format!("device={}", dev));
        }
    }
    if params.is_empty() {
        "device=512@0xd0000000:10 device=512@0xd0800000:11 device=512@0xd1000000:12".to_string()
    } else {
        params.join(" ")
    }
}

/// Load a single kernel module using finit_module(2), with optional parameters.
///
/// Returns `Ok(())` if the module was loaded, is already loaded, or is built
/// into the kernel (no .ko file on disk). This makes the function resilient
/// to aarch64/VZ environments where virtio modules are compiled in (`=y`)
/// and no .ko files exist.
fn load_module_file(path: &str, params: &str) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    // If the .ko file doesn't exist, check if the module is already built
    // into the kernel. Built-in modules appear in /sys/module/<name>.
    if !std::path::Path::new(path).exists() {
        let mod_name = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        // Kernel uses underscores internally even if the .ko has hyphens
        let sys_name = mod_name.replace('-', "_");
        let sys_path = format!("/sys/module/{}", sys_name);
        if std::path::Path::new(&sys_path).exists() {
            kmsg(&format!(
                "Module {} built-in (found {})",
                mod_name, sys_path
            ));
            return Ok(());
        }
        return Err(format!("file not found: {}", path));
    }

    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {}", path, e))?;

    let fd = file.as_raw_fd();
    let params_c = CString::new(params).unwrap();

    // finit_module(fd, params, flags)
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

/// Mount shared directories specified via kernel cmdline parameters.
///
/// The host encodes mount config as `voidbox.mount<N>=<tag>:<guest_path>:<ro|rw>`.
/// On macOS/VZ the filesystem type is `virtiofs`; on Linux/KVM it's `9p`.
/// We try virtiofs first and fall back to 9p.
fn mount_shared_dirs() {
    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut mounts: Vec<(String, String, bool)> = Vec::new(); // (tag, guest_path, read_only)

    for param in cmdline.split_whitespace() {
        // Match voidbox.mount0=mount0:/workspace/output:rw
        if let Some(rest) = param.strip_prefix("voidbox.mount") {
            // rest = "0=mount0:/workspace/output:rw"
            if let Some(eq_pos) = rest.find('=') {
                let value = &rest[eq_pos + 1..];
                let parts: Vec<&str> = value.splitn(3, ':').collect();
                if parts.len() >= 2 {
                    let tag = parts[0].to_string();
                    let guest_path = parts[1].to_string();
                    let read_only = parts.get(2).map(|&m| m != "rw").unwrap_or(true);
                    mounts.push((tag, guest_path, read_only));
                }
            }
        }
    }

    if mounts.is_empty() {
        return;
    }

    kmsg(&format!(
        "Mounting {} shared director{}",
        mounts.len(),
        if mounts.len() == 1 { "y" } else { "ies" }
    ));

    for (tag, guest_path, read_only) in &mounts {
        // Create the mount point
        if let Err(e) = std::fs::create_dir_all(guest_path) {
            kmsg(&format!(
                "WARNING: failed to create mount point {}: {}",
                guest_path, e
            ));
            continue;
        }

        let mode = if *read_only { "ro" } else { "rw" };

        // Try virtiofs first (macOS/VZ), then 9p (Linux/KVM)
        let tag_cstr = std::ffi::CString::new(tag.as_str()).unwrap();
        let path_cstr = std::ffi::CString::new(guest_path.as_str()).unwrap();
        let virtiofs_type = std::ffi::CString::new("virtiofs").unwrap();
        let p9_type = std::ffi::CString::new("9p").unwrap();

        let ro_flag: libc::c_ulong = if *read_only {
            libc::MS_RDONLY as libc::c_ulong
        } else {
            0
        };

        // Try virtiofs
        let ret = unsafe {
            libc::mount(
                tag_cstr.as_ptr(),
                path_cstr.as_ptr(),
                virtiofs_type.as_ptr(),
                ro_flag,
                std::ptr::null(),
            )
        };

        if ret == 0 {
            kmsg(&format!(
                "Mounted virtiofs '{}' at {} ({})",
                tag, guest_path, mode
            ));
            continue;
        }

        // Try 9p with trans=virtio
        let p9_opts = std::ffi::CString::new(format!(
            "trans=virtio,version=9p2000.L{}",
            if *read_only { ",ro" } else { "" }
        ))
        .unwrap();

        let ret = unsafe {
            libc::mount(
                tag_cstr.as_ptr(),
                path_cstr.as_ptr(),
                p9_type.as_ptr(),
                ro_flag,
                p9_opts.as_ptr() as *const libc::c_void,
            )
        };

        if ret == 0 {
            kmsg(&format!(
                "Mounted 9p '{}' at {} ({})",
                tag, guest_path, mode
            ));
        } else {
            let err = std::io::Error::last_os_error();
            kmsg(&format!(
                "WARNING: failed to mount '{}' at {}: {} (tried virtiofs and 9p)",
                tag, guest_path, err
            ));
        }
    }
}

/// Set up an OCI base image rootfs via overlayfs + pivot_root.
///
/// When the host sets `voidbox.oci_rootfs=<path>` on the kernel cmdline,
/// the guest-agent will:
/// 1. Mount a tmpfs for the overlay upper/work directories
/// 2. Mount overlayfs (OCI rootfs as lower, tmpfs as upper) at a staging point
/// 3. Move /proc, /sys, /dev into the new root
/// 4. pivot_root to switch the root filesystem
/// 5. Unmount the old root
///
/// This gives a writable root filesystem based on the OCI image.
fn setup_oci_rootfs() {
    if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
        *s = "starting";
    }
    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(_) => {
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "cmdline-read-failed";
            }
            return;
        }
    };

    let oci_rootfs_dev = cmdline
        .split_whitespace()
        .find_map(|p| p.strip_prefix("voidbox.oci_rootfs_dev="))
        .map(String::from);
    let oci_rootfs = cmdline
        .split_whitespace()
        .find_map(|p| p.strip_prefix("voidbox.oci_rootfs="))
        .map(String::from);

    let lowerdir = if let Some(dev) = oci_rootfs_dev {
        match mount_oci_block_lowerdir(&dev) {
            Ok(path) => path,
            Err(e) => {
                let msg = format!("WARNING: OCI rootfs device {} mount failed: {}", dev, e);
                kmsg(&msg);
                eprintln!("{}", msg);
                if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                    *s = "block-mount-failed";
                }
                return;
            }
        }
    } else {
        let Some(rootfs_path) = oci_rootfs else {
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "no-oci-rootfs";
            }
            return;
        };

        // Legacy mount-based OCI path for virtiofs/9p backends.
        if !std::path::Path::new(&rootfs_path).is_dir() {
            kmsg(&format!(
                "WARNING: OCI rootfs {} not found, skipping pivot_root",
                rootfs_path
            ));
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "rootfs-path-missing";
            }
            return;
        }
        let has_content = std::fs::read_dir(&rootfs_path)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if !has_content {
            kmsg(&format!(
                "WARNING: OCI rootfs {} is empty (mount may have failed), skipping pivot_root",
                rootfs_path
            ));
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "rootfs-path-empty";
            }
            return;
        }
        rootfs_path
    };
    kmsg(&format!("Setting up OCI rootfs from {}", lowerdir));

    let newroot = "/mnt/newroot";
    // Overlay requires upperdir and workdir to be on the same filesystem.
    // Use one tmpfs parent and create both subdirs inside it.
    let overlay_base = "/mnt/overlay-tmp";
    let upper = "/mnt/overlay-tmp/upper";
    let work = "/mnt/overlay-tmp/work";

    // Create staging directories
    for dir in [newroot, overlay_base] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            kmsg(&format!("WARNING: failed to create {}: {}", dir, e));
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "mkdir-failed";
            }
            return;
        }
    }

    // Mount a single tmpfs that will hold both upper and work directories.
    let tmpfs_type = std::ffi::CString::new("tmpfs").unwrap();
    let overlay_base_c = std::ffi::CString::new(overlay_base).unwrap();
    let ret = unsafe {
        libc::mount(
            tmpfs_type.as_ptr(),
            overlay_base_c.as_ptr(),
            tmpfs_type.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        kmsg(&format!(
            "WARNING: failed to mount tmpfs at {}: {}",
            overlay_base,
            std::io::Error::last_os_error()
        ));
        if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
            *s = "overlay-tmpfs-mount-failed";
        }
        return;
    }
    for dir in [upper, work] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            kmsg(&format!("WARNING: failed to create {}: {}", dir, e));
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "overlay-dir-create-failed";
            }
            return;
        }
    }

    // Mount overlayfs: lower=OCI rootfs (read-only), upper=tmpfs (writable)
    let overlay_type = std::ffi::CString::new("overlay").unwrap();
    let newroot_c = std::ffi::CString::new(newroot).unwrap();
    let overlay_opts = std::ffi::CString::new(format!(
        "lowerdir={},upperdir={},workdir={}",
        lowerdir, upper, work
    ))
    .unwrap();

    let ret = unsafe {
        libc::mount(
            overlay_type.as_ptr(),
            newroot_c.as_ptr(),
            overlay_type.as_ptr(),
            0,
            overlay_opts.as_ptr() as *const libc::c_void,
        )
    };
    if ret != 0 {
        let msg = format!(
            "WARNING: overlayfs mount failed: {} (kernel may lack CONFIG_OVERLAY_FS)",
            std::io::Error::last_os_error()
        );
        kmsg(&msg);
        eprintln!("{}", msg);
        if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
            *s = "overlay-mount-failed";
        }
        return;
    }

    kmsg("Overlayfs mounted, preparing pivot_root...");

    // Create mount points in the new root
    for dir in [
        "/proc",
        "/sys",
        "/dev",
        "/tmp",
        "/workspace",
        "/home/sandbox",
        "/etc/voidbox",
        "/usr/local/bin",
        "/lib/modules",
        "/mnt/oldroot",
    ] {
        let full = format!("{}{}", newroot, dir);
        let _ = std::fs::create_dir_all(&full);
    }

    // Stage essential host-provided tools from initramfs into the new root.
    // This keeps control-plane commands functional even for minimal OCI roots.
    stage_bootstrap_tools_into_newroot(newroot);

    // If the initramfs has claude-code, stage it into the OCI overlay root so
    // agent-mode runs keep working after root switch.
    stage_claude_into_newroot(newroot);

    // Move existing mounts into the new root
    for mount_point in ["/proc", "/sys", "/dev"] {
        let src = std::ffi::CString::new(mount_point).unwrap();
        let dst = std::ffi::CString::new(format!("{}{}", newroot, mount_point)).unwrap();
        let ret = unsafe {
            libc::mount(
                src.as_ptr(),
                dst.as_ptr(),
                std::ptr::null(),
                libc::MS_MOVE,
                std::ptr::null(),
            )
        };
        if ret != 0 {
            kmsg(&format!(
                "WARNING: failed to move mount {} -> {}{}: {}",
                mount_point,
                newroot,
                mount_point,
                std::io::Error::last_os_error()
            ));
        }
    }

    // Switch to overlay root via pivot_root.
    // pivot_root requires mount propagation to be private.
    unsafe {
        libc::mount(
            std::ptr::null(),
            std::ffi::CString::new("/").unwrap().as_ptr(),
            std::ptr::null(),
            (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
            std::ptr::null(),
        );
    }

    let cwd_newroot = std::ffi::CString::new(newroot).unwrap();
    unsafe {
        libc::chdir(cwd_newroot.as_ptr());
    }
    let put_old_c = std::ffi::CString::new("mnt/oldroot").unwrap();
    let dot_c = std::ffi::CString::new(".").unwrap();
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pivot_root as libc::c_long,
            dot_c.as_ptr(),
            put_old_c.as_ptr(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        let code = err.raw_os_error().unwrap_or_default();
        if code == libc::EINVAL {
            // Initramfs rootfs cannot be pivot_root'ed. Fallback to switch-root.
            let root_c = std::ffi::CString::new("/").unwrap();
            let move_ret = unsafe {
                libc::mount(
                    dot_c.as_ptr(),
                    root_c.as_ptr(),
                    std::ptr::null(),
                    libc::MS_MOVE as libc::c_ulong,
                    std::ptr::null(),
                )
            };
            if move_ret == 0 {
                let chroot_ret = unsafe { libc::chroot(dot_c.as_ptr()) };
                if chroot_ret == 0 {
                    unsafe {
                        libc::chdir(root_c.as_ptr());
                    }
                    if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                        *s = "ok-switch-root";
                    }
                } else if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                    *s = "switch-root-chroot-failed";
                }
            } else if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = "switch-root-move-failed";
            }
            if let Ok(s) = OCI_SETUP_STATUS.lock() {
                if *s == "ok-switch-root" {
                    kmsg("OCI rootfs switch-root fallback complete");
                    eprintln!("OCI rootfs switch-root fallback complete");
                    // Continue with post-switch setup.
                } else {
                    let msg = format!(
                        "WARNING: pivot_root EINVAL and switch-root fallback failed: {}",
                        *s
                    );
                    kmsg(&msg);
                    eprintln!("{}", msg);
                    return;
                }
            }
        } else {
            let msg = format!("WARNING: pivot_root failed (errno={}): {}", code, err);
            kmsg(&msg);
            eprintln!("{}", msg);
            if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
                *s = match code {
                    libc::EBUSY => "pivot-root-ebusy",
                    libc::EPERM => "pivot-root-eperm",
                    libc::ENOENT => "pivot-root-enoent",
                    _ => "pivot-root-failed",
                };
            }
            return;
        }
    }

    let root = std::ffi::CString::new("/").unwrap();
    unsafe {
        libc::chdir(root.as_ptr());
    }

    // Detach old root so it is no longer reachable.
    let oldroot_c = std::ffi::CString::new("/mnt/oldroot").unwrap();
    unsafe {
        libc::umount2(oldroot_c.as_ptr(), libc::MNT_DETACH);
    }
    let _ = std::fs::remove_dir_all("/mnt/oldroot");

    // Mount tmpfs on /tmp in the new root.
    let tmpfs_type = std::ffi::CString::new("tmpfs").unwrap();
    let tmp_path = std::ffi::CString::new("/tmp").unwrap();
    let tmp_opts = std::ffi::CString::new("mode=1777").unwrap();
    unsafe {
        libc::mount(
            tmpfs_type.as_ptr(),
            tmp_path.as_ptr(),
            tmpfs_type.as_ptr(),
            0,
            tmp_opts.as_ptr() as *const libc::c_void,
        );
    }

    // Recreate essential directories (may already exist from the OCI image)
    let _ = std::fs::create_dir_all("/workspace");
    let _ = std::fs::create_dir_all("/home/sandbox");
    let _ = std::fs::create_dir_all("/etc/voidbox");

    // Preserve DNS inside the new root. Network setup happened before root switch.
    // Force a regular resolv.conf (not a dangling symlink from base image layers).
    ensure_resolv_conf("nameserver 10.0.2.3\n");

    // Chown workspace and home to sandbox user (uid 1000)
    unsafe {
        let workspace = std::ffi::CString::new("/workspace").unwrap();
        libc::chown(workspace.as_ptr(), 1000, 1000);
        let home = std::ffi::CString::new("/home/sandbox").unwrap();
        libc::chown(home.as_ptr(), 1000, 1000);
    }

    kmsg("OCI rootfs pivot_root complete — running on overlay filesystem");
    eprintln!("OCI rootfs pivot_root complete");
    if let Ok(mut s) = OCI_SETUP_STATUS.lock() {
        *s = "ok";
    }
}

fn stage_claude_into_newroot(newroot: &str) {
    let src = "/usr/local/bin/claude-code";
    if !std::path::Path::new(src).exists() {
        return;
    }

    let dst = format!("{}/usr/local/bin/claude-code", newroot);
    let dst_path = std::path::Path::new(&dst);
    if !dst_path.exists() {
        match std::fs::copy(src, dst_path) {
            Ok(_) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dst_path, std::fs::Permissions::from_mode(0o755));
                kmsg("Staged claude-code into OCI overlay root");
            }
            Err(e) => {
                kmsg(&format!(
                    "WARNING: failed to stage claude-code into OCI root: {}",
                    e
                ));
                return;
            }
        }
    }

    // Keep the convenience alias.
    let claude_link = format!("{}/usr/local/bin/claude", newroot);
    let _ = std::fs::remove_file(&claude_link);
    let _ = std::os::unix::fs::symlink("claude-code", &claude_link);
}

fn stage_bootstrap_tools_into_newroot(newroot: &str) {
    let src_busybox = "/bin/busybox";
    if !std::path::Path::new(src_busybox).exists() {
        return;
    }

    let dst_busybox = format!("{}/bin/busybox", newroot);
    let dst_busybox_path = std::path::Path::new(&dst_busybox);
    if let Some(parent) = dst_busybox_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !dst_busybox_path.exists() {
        if let Err(e) = std::fs::copy(src_busybox, dst_busybox_path) {
            kmsg(&format!(
                "WARNING: failed to stage busybox into OCI root: {}",
                e
            ));
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dst_busybox_path, std::fs::Permissions::from_mode(0o755));
        kmsg("Staged busybox into OCI overlay root");
    }

    // Provide the minimal applets used by tests and bootstrap commands.
    for applet in ["sh", "cat", "touch", "test", "ls", "mkdir", "rm"] {
        let link = format!("{}/bin/{}", newroot, applet);
        let _ = std::fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink("busybox", &link);
    }
}

fn mount_oci_block_lowerdir(dev: &str) -> Result<String, String> {
    let dev_path = std::path::Path::new(dev);
    for _ in 0..40 {
        if dev_path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !dev_path.exists() {
        return Err(format!("device not found: {}", dev));
    }

    let lowerdir = "/mnt/oci-lower";
    std::fs::create_dir_all(lowerdir).map_err(|e| e.to_string())?;
    let dev_c = std::ffi::CString::new(dev).unwrap();
    let lower_c = std::ffi::CString::new(lowerdir).unwrap();
    let ext4_c = std::ffi::CString::new("ext4").unwrap();
    let ret = unsafe {
        libc::mount(
            dev_c.as_ptr(),
            lower_c.as_ptr(),
            ext4_c.as_ptr(),
            libc::MS_RDONLY as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let has_content = std::fs::read_dir(lowerdir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if !has_content {
        return Err("mounted OCI block rootfs is empty".to_string());
    }
    Ok(lowerdir.to_string())
}

/// Set up network interface.
///
/// Tries DHCP first (for VZ NAT on macOS), falls back to static SLIRP
/// addressing (for KVM/SLIRP on Linux).
fn setup_network() {
    kmsg("Setting up network...");

    for i in 0..300 {
        if std::path::Path::new("/sys/class/net/eth0").exists() {
            kmsg(&format!("eth0 detected after {} attempts", i + 1));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    if !std::path::Path::new("/sys/class/net/eth0").exists() {
        kmsg("Warning: eth0 not found, networking may not be available");
        return;
    }

    let _ = run_cmd("ip", &["link", "set", "lo", "up"]);
    let _ = run_cmd("ip", &["link", "set", "eth0", "up"]);

    // Try DHCP first (works with VZ NAT on macOS), fall back to static below.
    let dhcp_result = Command::new("udhcpc")
        .args(["-i", "eth0", "-n", "-q", "-t", "3", "-T", "2"])
        .output();

    if let Err(e) = &dhcp_result {
        kmsg(&format!("udhcpc failed to run: {}", e));
    }

    let dhcp_ok = dhcp_result.map(|o| o.status.success()).unwrap_or(false);

    if dhcp_ok {
        kmsg("Network configured via DHCP (VZ NAT)");
        ensure_resolv_conf("nameserver 8.8.8.8\n");
        return;
    }

    // Fallback: static SLIRP addressing (KVM)
    kmsg("DHCP failed, falling back to static SLIRP addressing");
    let mut routed = false;
    for _ in 0..20 {
        let _ = run_cmd("ip", &["link", "set", "eth0", "up"]);
        let _ = run_cmd("ip", &["addr", "replace", "10.0.2.15/24", "dev", "eth0"]);
        let _ = run_cmd("ip", &["route", "replace", "default", "via", "10.0.2.2"]);
        if has_default_route() {
            routed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    ensure_resolv_conf("nameserver 10.0.2.3\n");
    if !routed {
        kmsg("WARNING: default route not visible after static network setup");
    }

    kmsg("Network configured: 10.0.2.15/24, gw 10.0.2.2, dns 10.0.2.3");
}

fn network_enabled_from_cmdline() -> bool {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    cmdline
        .split_whitespace()
        .any(|t| t == "virtio_mmio.device=512@0xd0000000:10")
}

fn ensure_resolv_conf(contents: &str) {
    let _ = std::fs::create_dir_all("/etc");
    if let Ok(meta) = std::fs::symlink_metadata("/etc/resolv.conf") {
        if meta.file_type().is_symlink() {
            if let Err(e) = std::fs::remove_file("/etc/resolv.conf") {
                kmsg(&format!("Failed to remove symlink /etc/resolv.conf: {}", e));
            }
        }
    }

    match std::fs::write("/etc/resolv.conf", contents) {
        Ok(()) => kmsg("Wrote /etc/resolv.conf"),
        Err(e) => kmsg(&format!("Failed to write /etc/resolv.conf: {}", e)),
    }
}

/// Run a command and log the result
fn run_cmd(program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).output() {
        Ok(output) => {
            if !output.status.success() {
                eprintln!(
                    "Warning: {} {:?} failed: {}",
                    program,
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
                return false;
            }
            true
        }
        Err(e) => {
            eprintln!("Warning: failed to run {} {:?}: {}", program, args, e);
            false
        }
    }
}

fn has_default_route() -> bool {
    let routes = match std::fs::read_to_string("/proc/net/route") {
        Ok(s) => s,
        Err(_) => return false,
    };
    for line in routes.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() > 1 && cols[0] == "eth0" && cols[1] == "00000000" {
            return true;
        }
    }
    false
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

        // Reject oversized messages before allocating
        if length > MAX_MESSAGE_SIZE {
            eprintln!(
                "Rejecting oversized message: {} bytes (max {})",
                length, MAX_MESSAGE_SIZE
            );
            return Err(format!(
                "Payload too large: {} bytes (max {})",
                length, MAX_MESSAGE_SIZE
            ));
        }

        // Read payload
        let mut payload = vec![0u8; length];
        if length > 0 {
            read_exact(fd, &mut payload)?;
        }

        // Require authentication for all message types except Ping (which IS the auth).
        if msg_type != 3 && !AUTHENTICATED.with(|a| a.get()) {
            eprintln!("Rejecting unauthenticated message type {}", msg_type);
            return Err(
                "Connection not authenticated -- send Ping with session secret first".into(),
            );
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
                // Ping -- payload carries the 32-byte session secret, optionally
                // followed by a 4-byte LE protocol version (36 bytes total).
                // Old hosts send 32 bytes (version 0); new hosts send 36.
                match SESSION_SECRET.get() {
                    Some(secret) if payload.len() >= 32 && payload[..32] == secret[..] => {
                        AUTHENTICATED.with(|a| a.set(true));

                        // Parse optional protocol version from bytes 32..36.
                        let peer_version = if payload.len() >= 36 {
                            u32::from_le_bytes([payload[32], payload[33], payload[34], payload[35]])
                        } else {
                            0 // legacy host, no version field
                        };

                        // Reply with our protocol version in the Pong payload.
                        let version_bytes = void_box_protocol::PROTOCOL_VERSION.to_le_bytes();
                        send_raw_message(fd, MessageType::Pong, &version_bytes)?;

                        kmsg(&format!(
                            "Authenticated (peer_version={}, our_version={})",
                            peer_version,
                            void_box_protocol::PROTOCOL_VERSION
                        ));

                        // Defer OCI pivot_root until after at least one authenticated control
                        // channel exists. This avoids startup deadlocks where OCI mount/pivot
                        // work races the initial host handshake.
                        OCI_ROOTFS_SETUP_ONCE.call_once(|| {
                            setup_oci_rootfs();
                        });
                    }
                    Some(_) => {
                        eprintln!("Authentication failed: invalid secret");
                        return Err("Authentication failed: invalid session secret".into());
                    }
                    None => {
                        eprintln!("Authentication failed: no secret configured");
                        return Err("Authentication failed: no session secret configured".into());
                    }
                }
                // Continue loop - host will send ExecRequest on same connection
            }
            5 => {
                // Shutdown
                eprintln!("Shutdown requested");
                unsafe {
                    libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
                }
                return Ok(());
            }
            10 => {
                // SubscribeTelemetry - enter streaming mode
                let opts: TelemetrySubscribeRequest = if payload.is_empty() {
                    TelemetrySubscribeRequest::default()
                } else {
                    serde_json::from_slice(&payload).unwrap_or_default()
                };
                kmsg(&format!(
                    "Telemetry subscription started (interval={}ms, kernel_threads={})",
                    opts.interval_ms, opts.include_kernel_threads
                ));
                telemetry_stream_loop(fd, &opts);
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

/// Check if a program is allowed by the command allowlist.
/// Returns the resolved program name (basename) for allowlist matching.
fn is_command_allowed(program: &str) -> bool {
    match COMMAND_ALLOWLIST.get() {
        None => true, // No allowlist loaded = allow all
        Some(list) if list.is_empty() => true,
        Some(list) => {
            // Match against basename of the program path
            let basename = Path::new(program)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(program);
            list.iter().any(|allowed| allowed == basename)
        }
    }
}

/// Execute a command, streaming stdout/stderr chunks via ExecOutputChunk
/// messages, then return the final ExecResponse with full accumulated output.
fn execute_command(fd: RawFd, request: &ExecRequest) -> ExecResponse {
    let start = std::time::Instant::now();

    // Check command allowlist before spawning
    if !is_command_allowed(&request.program) {
        eprintln!("Command not allowed: {}", request.program);
        kmsg(&format!("Command not allowed: {}", request.program));
        return ExecResponse {
            stdout: Vec::new(),
            stderr: format!(
                "Command '{}' is not in the allowed commands list",
                request.program
            )
            .into_bytes(),
            exit_code: -1,
            error: Some(format!("Command '{}' is not allowed", request.program)),
            duration_ms: Some(start.elapsed().as_millis() as u64),
        };
    }

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
    //
    // Also apply resource limits (setrlimit) to prevent fork bombs, OOM, and disk filling.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            // Always run child processes as sandbox user.
            if libc::setgid(1000) != 0 || libc::setuid(1000) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Create a new process group so the watchdog can killpg().
            libc::setpgid(0, 0);

            if let Some(limits) = RESOURCE_LIMITS.get() {
                // RLIMIT_AS intentionally omitted: Bun (claude-code runtime)
                // requires large virtual address space for mmap and will abort
                // if constrained. The VM memory limit is the effective bound.

                // RLIMIT_NOFILE: open file descriptors
                let rlim_nofile = libc::rlimit {
                    rlim_cur: limits.max_open_files,
                    rlim_max: limits.max_open_files,
                };
                libc::setrlimit(libc::RLIMIT_NOFILE, &rlim_nofile);

                // RLIMIT_NPROC: max processes (anti-fork-bomb)
                let rlim_nproc = libc::rlimit {
                    rlim_cur: limits.max_processes,
                    rlim_max: limits.max_processes,
                };
                libc::setrlimit(libc::RLIMIT_NPROC, &rlim_nproc);

                // RLIMIT_FSIZE: max file size
                let rlim_fsize = libc::rlimit {
                    rlim_cur: limits.max_file_size,
                    rlim_max: limits.max_file_size,
                };
                libc::setrlimit(libc::RLIMIT_FSIZE, &rlim_fsize);
            }

            Ok(())
        });
    }

    // Spawn the process
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let path_env = std::env::var("PATH").unwrap_or_default();
            let mut msg = format!("Failed to spawn process '{}': {}", request.program, e);
            if e.kind() == std::io::ErrorKind::NotFound {
                let mut checks: Vec<String> = Vec::new();
                if request.program.contains('/') {
                    let p = Path::new(&request.program);
                    if let Ok(meta) = std::fs::metadata(p) {
                        use std::os::unix::fs::PermissionsExt;
                        checks.push(format!(
                            "{} exists mode={:o}",
                            p.display(),
                            meta.permissions().mode()
                        ));
                    }
                } else {
                    for dir in path_env.split(':') {
                        if dir.is_empty() {
                            continue;
                        }
                        let candidate = Path::new(dir).join(&request.program);
                        if let Ok(meta) = std::fs::metadata(&candidate) {
                            use std::os::unix::fs::PermissionsExt;
                            checks.push(format!(
                                "{} exists mode={:o}",
                                candidate.display(),
                                meta.permissions().mode()
                            ));
                        }
                    }
                }
                if !checks.is_empty() {
                    msg.push_str(&format!(
                        "; found candidate binaries [{}] (ENOENT may indicate missing ELF interpreter or loader path)",
                        checks.join(", ")
                    ));
                }
            }
            let diag_paths = [
                "/bin/sh",
                "/usr/bin/bash",
                "/usr/bin/npm",
                "/lib64/ld-linux-x86-64.so.2",
            ];
            let mut diag = Vec::new();
            for p in diag_paths {
                let exists = Path::new(p).exists();
                diag.push(format!("{}={}", p, exists));
            }
            if let Ok(mounts) = std::fs::read_to_string("/proc/1/mounts") {
                if let Some(root_line) = mounts.lines().find(|l| l.contains(" / ")) {
                    diag.push(format!("root_mount={}", root_line));
                }
            }
            if let Ok(status) = OCI_SETUP_STATUS.lock() {
                diag.push(format!("oci_setup={}", *status));
            }
            msg.push_str(&format!("; diag [{}]", diag.join(", ")));
            kmsg(&format!(
                "Failed to spawn '{}': {} (cwd={:?})",
                request.program, e, request.working_dir
            ));
            return ExecResponse {
                stdout: Vec::new(),
                stderr: format!(
                    "{} (cwd={:?}, agent_path={})",
                    msg, request.working_dir, path_env
                )
                .as_bytes()
                .to_vec(),
                exit_code: -1,
                error: Some(msg),
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

    // Spawn a watchdog thread that will SIGKILL the child's process group
    // if it exceeds the timeout. The child PID is captured before we move
    // ownership to the wait logic.
    let child_pid = child.id() as i32;
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_handle = if let Some(timeout_secs) = request.timeout_secs {
        if timeout_secs == 0 {
            None // Service mode: no timeout
        } else {
            let timed_out_flag = timed_out.clone();
            Some(
                std::thread::Builder::new()
                    .name("watchdog".into())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(timeout_secs));
                        eprintln!(
                            "Watchdog: timeout ({}s) reached, sending SIGKILL to pid {}",
                            timeout_secs, child_pid
                        );
                        timed_out_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        // Kill the entire process group (negative PID)
                        unsafe {
                            libc::kill(-child_pid, libc::SIGKILL);
                            // Also kill the specific process in case setpgid wasn't called
                            libc::kill(child_pid, libc::SIGKILL);
                        }
                    })
                    .ok(),
            )
        }
    } else {
        None
    };

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
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    kmsg(&format!(
                        "Process '{}' killed by signal {} (exit_status={:?})",
                        request.program, sig, status,
                    ));
                }
            }
            status.code().unwrap_or(-1)
        }
        Err(e) => {
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

    // The watchdog thread is a daemon -- it will exit when the process dies.
    // We don't join it because it may still be sleeping.
    let _ = watchdog_handle;

    let was_timed_out = timed_out.load(std::sync::atomic::Ordering::SeqCst);

    let error_msg = if was_timed_out {
        Some(format!(
            "Process killed after {}s timeout",
            request.timeout_secs.unwrap_or(0)
        ))
    } else if exit_code == -1 {
        Some("Process killed by signal (exit_code mapped to -1)".to_string())
    } else {
        None
    };

    ExecResponse {
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        exit_code,
        error: error_msg,
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

/// Send a raw (non-JSON) message to the host over the vsock fd.
///
/// Unlike [`send_response`] which JSON-serializes a `T`, this writes the
/// payload bytes verbatim. Used for the Pong reply that carries raw
/// protocol-version bytes instead of JSON.
fn send_raw_message(fd: RawFd, msg_type: MessageType, payload: &[u8]) -> Result<(), String> {
    let length = payload.len() as u32;
    let mut msg = Vec::with_capacity(5 + payload.len());
    msg.extend_from_slice(&length.to_le_bytes());
    msg.push(msg_type as u8);
    msg.extend_from_slice(payload);

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
fn telemetry_stream_loop(fd: RawFd, opts: &TelemetrySubscribeRequest) {
    let interval = std::time::Duration::from_millis(opts.interval_ms.max(100)); // floor at 100ms
    let mut seq: u64 = 0;
    let mut prev_cpu = read_cpu_jiffies();

    loop {
        std::thread::sleep(interval);

        let curr_cpu = read_cpu_jiffies();
        let cpu_percent = compute_cpu_percent(&prev_cpu, &curr_cpu);
        prev_cpu = curr_cpu;

        let (memory_used_bytes, memory_total_bytes) = read_meminfo();
        let (net_rx_bytes, net_tx_bytes) = read_netdev();
        let procs_running = read_procs_running();
        let open_fds = read_open_fds();
        let processes = collect_process_metrics(opts.include_kernel_threads);

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
        if let Some(after_colon) = trimmed.strip_prefix("eth0:") {
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
///
/// When `include_kernel_threads` is false, kernel threads are filtered out.
/// Kernel threads have an empty `/proc/PID/cmdline` — this is the standard
/// Linux way to distinguish them.
fn collect_process_metrics(include_kernel_threads: bool) -> Vec<ProcessMetrics> {
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

        // Filter kernel threads: they have an empty /proc/PID/cmdline
        if !include_kernel_threads {
            let cmdline = std::fs::read(format!("{}/cmdline", base)).unwrap_or_default();
            if cmdline.is_empty() {
                continue;
            }
        }

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
